//! 把公共 iperf client 参数、重试与事件解析接到 ADB；板侧进程有独立停止标记与租约。
use super::*;
use crate::util::{CmdOut, ProcessExecutor, ProcessSpec};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn command(spec: &ProcessSpec) -> String {
    std::iter::once(&spec.program)
        .chain(&spec.args)
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn client_script(spec: &ProcessSpec, dir: &str, lease: u64) -> String {
    let cmd = command(spec);
    let dir = quote(dir);
    format!(
        r#"mkdir {dir} || exit 70
p=''
cleanup() {{
  if [ -n "$p" ]; then kill -KILL "$p" 2>/dev/null; wait "$p" 2>/dev/null; fi
  rm -f {dir}/stop
  rmdir {dir}
}}
trap cleanup EXIT
trap 'exit 143' HUP INT TERM
{cmd} &
p=$!
i=0
while kill -0 "$p" 2>/dev/null && [ ! -f {dir}/stop ] && [ "$i" -lt {lease} ]; do
  sleep 1
  i=$((i + 1))
done
if kill -0 "$p" 2>/dev/null; then
  kill -KILL "$p" 2>/dev/null
  wait "$p" 2>/dev/null
  rc=124
else
  wait "$p"
  rc=$?
fi
p=''
printf '\n__CPE_INNER_STATUS__:%s\n' "$rc"
"#
    )
}

struct Lease<'a> {
    adb: &'a Adb,
    dir: String,
    closed: bool,
}
impl Lease<'_> {
    fn close(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        // 与 `ReceiverServer::stop` 同一条规矩：一开始回收就记为已关，**失败也
        // 不再来第二遍**。以前失败时 `closed` 还是 false，`Drop` 会把整套回收
        // 再跑一遍——又一次写停止文件，外加最多六轮 `adb shell test ! -d`
        // （约 1.5 秒加每次 shell 的往返），对着可能早已消失的目录，同一个故障
        // 还被报两遍。重试要由上层决定，不该藏在析构里。
        self.closed = true;
        self.adb.shell(&format!(
            "if [ -d {0} ]; then : > {0}/stop; fi",
            quote(&self.dir)
        ))?;
        for _ in 0..6 {
            if self
                .adb
                .shell(&format!("test ! -d {}", quote(&self.dir)))
                .is_ok()
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err("板侧 client 回收未确认，禁止继续起流；有限租约仍负责最终停止".into())
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.close();
        }
    }
}

struct Executor<'a> {
    adb: &'a Adb,
    owner: &'a str,
}
impl ProcessExecutor for Executor<'_> {
    fn run(&self, spec: &ProcessSpec, timeout: Duration) -> CmdOut {
        // 预算按调用方给的来。以前这里丢掉 `timeout` 一律走 `Adb::shell` 的
        // 4 秒，于是 `iperf3 --help`（8 秒预算的能力探测）在慢一点的 adbd 上
        // 必然超时，而超时不报错、只是让 `--forceflush` 被悄悄摘掉。
        match self.adb.shell_within(&command(spec), timeout) {
            Ok(stdout) => CmdOut {
                ok: true,
                timed_out: false,
                cancelled: false,
                stdout,
                stderr: String::new(),
            },
            Err(stderr) => CmdOut {
                ok: false,
                timed_out: false,
                cancelled: false,
                stdout: String::new(),
                stderr,
            },
        }
    }
    fn run_streaming(
        &self,
        spec: &ProcessSpec,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
        on_line: &mut dyn FnMut(&str, Instant),
    ) -> CmdOut {
        let mut lease = Lease {
            adb: self.adb,
            dir: format!("/tmp/cpe-inner-{}-client", self.owner),
            closed: false,
        };
        let script = client_script(spec, &lease.dir, timeout.as_secs().saturating_add(5));
        // 上限跟着调用方给的规格走（板侧 iperf client 与本机 client 同一个上限）。
        let adb = ProcessSpec::new(
            self.adb.program.clone(),
            &["-s", &self.adb.serial, "shell", &script],
        )
        .with_stdout_limit(spec.stdout_limit);
        let mut out =
            crate::util::SystemProcessExecutor.run_streaming(&adb, timeout, cancel, on_line);
        // ADB 进程退出码不能代表老板侧 shell 的命令退出码。
        match adb::parse_shell_output(&out.stdout) {
            Ok(text) => out.stdout = text,
            Err(error) => {
                out.ok = false;
                out.stderr.push_str(&format!("\n{error}"));
            }
        }
        if let Err(error) = lease.close() {
            out.ok = false;
            out.stderr.push_str(&format!("\n回收子进程失败: {error}"));
        }
        out
    }
}

/// 板侧 `iperf3 --help` 的探测结果，按 (adb 程序, serial, bin) 记住。
///
/// 本地那条早就用 `OnceLock` 记住了同样的东西（`iperf::supports_forceflush`），
/// 板侧这条一直没跟上：每条腿都要多一次 ADB 往返去问同一个问题，一轮几十上百
/// 个单元就是几十上百次；每一次还都可能因为 adbd 慢而超时，把 `--forceflush`
/// 摘掉——同一块板子在同一轮里给出不同答案，测量口径跟着抖。板子不会在一轮
/// 里换 iperf3，缓存到进程级即可。
/// (adb 程序, serial, 板侧 iperf3 路径) → 支持不支持 `--forceflush`。
type ForceflushCache = OnceLock<Mutex<HashMap<(String, String, String), bool>>>;
static BOARD_FORCEFLUSH: ForceflushCache = OnceLock::new();

fn board_supports_forceflush(adb: &Adb, executor: &Executor<'_>, bin: &str) -> bool {
    let key = (adb.program.clone(), adb.serial.clone(), bin.to_string());
    let cache = BOARD_FORCEFLUSH.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = crate::util::lock_recover(cache).get(&key) {
        return *hit;
    }
    let probed = iperf::supports_forceflush_with(executor, bin);
    crate::util::lock_recover(cache).insert(key, probed);
    probed
}

pub(super) fn run(
    adb: &Adb,
    bin: &str,
    request: &IperfClientReq,
    owner: &str,
    epoch: Instant,
    cancel: &AtomicBool,
) -> Result<(IperfClientOut, Vec<IperfFlowEvent>), String> {
    let mut origin = None;
    let mut events = Vec::new();
    let executor = Executor { adb, owner };
    let client = iperf::run_client_controlled_inner(
        &executor,
        board_supports_forceflush(adb, &executor, bin),
        bin,
        request,
        Some(cancel),
        |_| {},
        |mut event| {
            iperf::align_event_to_epoch(
                &mut event,
                epoch.elapsed().as_millis() as u64,
                &mut origin,
            );
            events.push(event);
        },
    );
    if client.cleanup_confirmed == Some(false) {
        return Err(client.output);
    }
    Ok((client, events))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicUsize;
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct FakeAdb {
        adb: Adb,
        path: PathBuf,
        owner: String,
    }
    impl FakeAdb {
        fn new() -> Self {
            let owner = format!(
                "client-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            );
            let path = std::env::temp_dir().join(format!("{owner}.sh"));
            // 模拟真正的 ADB：本机传输进程被终止时，板侧进程仍独立运行。
            // 输出经临时文件转发，远端不会持有本机 reader 的管道。
            std::fs::write(&path, r#"#!/usr/bin/env python3
import os, subprocess, sys, tempfile, time
with tempfile.TemporaryFile() as log:
    child = subprocess.Popen(['sh', '-c', sys.argv[4]], stdout=log, stderr=log, start_new_session=True)
    offset = 0
    while True:
        data = os.pread(log.fileno(), 65536, offset)
        if data:
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
            offset += len(data)
        elif child.poll() is not None:
            break
        time.sleep(0.02)
    sys.exit(child.wait())
"#).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                adb: Adb {
                    program: path.to_string_lossy().into(),
                    serial: "fake".into(),
                },
                path,
                owner,
            }
        }
    }
    impl Drop for FakeAdb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn board_client_exit_status_and_cancellation_are_checked_and_reaped() {
        for (script, cancel_on_output, expected_ok) in [
            ("printf 'done\\n'; exit 0", false, true),
            ("printf 'bad\\n'; exit 7", false, false),
            ("printf 'started\\n'; exec sleep 30", true, false),
        ] {
            let fake = FakeAdb::new();
            let cancel = AtomicBool::new(false);
            let spec = ProcessSpec::new("sh", &["-c", script]);
            let out = Executor {
                adb: &fake.adb,
                owner: &fake.owner,
            }
            .run_streaming(
                &spec,
                Duration::from_secs(5),
                Some(&cancel),
                &mut |_, _| {
                    if cancel_on_output {
                        cancel.store(true, Ordering::SeqCst);
                    }
                },
            );
            assert_eq!(out.ok, expected_ok, "{}", out.merged());
            assert!(out.cleanup_confirmed(), "{}", out.merged());
            assert_eq!(out.cancelled, cancel_on_output);
            assert!(!Path::new(&format!("/tmp/cpe-inner-{}-client", fake.owner)).exists());
        }
    }

    #[test]
    fn ordinary_board_tcp_and_udp_clients_reach_a_pc_server_when_iperf_is_available() {
        use crate::protocol::IperfServerStartReq;
        let Some(bin) = crate::cmd::tools::find_iperf3() else {
            return;
        };
        let fake = FakeAdb::new();
        let servers = iperf::IperfServerMgr::new();
        for (udp, bind_ip, v6) in [
            (false, "127.0.0.1", false),
            (true, "127.0.0.1", false),
            (false, "::1", true),
            (true, "::1", true),
        ] {
            let listener = std::net::TcpListener::bind((bind_ip, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let id = format!("{}-{udp}-{v6}", fake.owner);
            servers
                .start(
                    &bin,
                    &IperfServerStartReq {
                        bind_ip: bind_ip.into(),
                        port,
                        request_id: id.clone(),
                        owner_id: id.clone(),
                        lease_secs: 60,
                        v6,
                    },
                )
                .unwrap();
            let req = IperfClientReq {
                dst: bind_ip.into(),
                bind_ip: bind_ip.into(),
                duration: 1,
                port,
                udp,
                extra: if udp {
                    vec!["-b".into(), "1M".into()]
                } else {
                    Vec::new()
                },
                v6,
            };
            let result = run(
                &fake.adb,
                &bin,
                &req,
                &id,
                Instant::now(),
                &AtomicBool::new(false),
            );
            let stopped = servers.stop_checked(port, &id, Duration::ZERO).unwrap();
            assert!(stopped.terminated);
            let (out, events) = result.unwrap();
            assert!(out.ok, "{}", out.output);
            assert!(
                measure::parse_receiver_summary(&out.output, 1, ToolOrigin::ClientSummary).is_ok(),
                "{}",
                out.output
            );
            assert!(events
                .iter()
                .any(|event| event.kind == crate::protocol::IperfEventKind::Traffic));
        }
    }

    #[test]
    fn bidirectional_unit_runs_two_ordinary_clients_and_reaps_both_servers_when_iperf_is_available()
    {
        let Some(bin) = crate::cmd::tools::find_iperf3() else {
            return;
        };
        let fake = FakeAdb::new();
        let mut cfg = config::parse_config(include_str!("../../inner.example.json")).unwrap();
        cfg.links.truncate(1);
        cfg.agents.clear();
        cfg.directions = vec![Direction::Bidir];
        cfg.links[0].measurement = Measurement::Tool;
        let mut unit = plan::build(&cfg).unwrap().units.remove(0);
        let mut ports = Vec::new();
        for _ in 0..2 {
            ports.push(std::net::TcpListener::bind("127.0.0.1:0").unwrap());
        }
        for (leg, listener) in unit.legs.iter_mut().zip(&ports) {
            leg.port = listener.local_addr().unwrap().port();
        }
        drop(ports);
        // 仅替身用回环地址，不修改生产配置的 LAN 校验规则。
        cfg.duration_secs = 1;
        cfg.links[0].gateway = "127.0.0.1".parse().unwrap();
        cfg.links[0].local_ip = "127.0.0.1".parse().unwrap();
        cfg.links[0].local_interface = "missing-test-counter".into();
        let dir = std::env::temp_dir().join(&fake.owner);
        std::fs::create_dir(&dir).unwrap();
        let context = UnitContext {
            adb: &fake.adb,
            cfg: &cfg,
            link: &cfg.links[0],
            preflight: &LinkPreflight {
                board_iface: "br0".into(),
                counter_source: None,
                addresses: TrafficAddresses::ipv4(&cfg.links[0]),
            },
            bin: &bin,
            agent: None,
            dir: &dir,
            cancel: &AtomicBool::new(false),
        };
        let result = run_unit(&context, &unit);
        let _ = std::fs::remove_dir_all(&dir);
        let row = result.unwrap();
        assert_eq!(row.legs.len(), 2);
        for leg in &row.legs {
            assert!(leg.client.ok, "{}", leg.client.output);
            assert!(!leg.client.cmd.split_whitespace().any(|arg| arg == "-R"));
            assert_eq!(
                leg.receiver,
                if leg.flow.receiver_is_board() {
                    "br0"
                } else {
                    "missing-test-counter"
                }
            );
            assert!(
                std::net::TcpListener::bind(("127.0.0.1", leg.port)).is_ok(),
                "server 端口未释放"
            );
        }
    }
}
