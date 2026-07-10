//! iperf3 命令构造、文本输出解析、server 进程管理、client 执行（带重试）
//!
//! 说明：统一用文本输出（-f m -i 1）而不是 --json，
//! 原因：--json 要等进程结束才输出（无实时速率），且旧版 Windows iperf3(3.1.x)
//! 不支持 --json-stream。文本模式对所有版本都稳定，且能实时逐行读速率。

use crate::protocol::{
    IperfClientOut, IperfClientReq, IperfClientStatusOut, IperfEventKind, IperfFlowEvent,
    IperfServerStartReq, IperfServerStopOut,
};
use crate::util::{decode_bytes, run_cmd, run_streaming_controlled};
use regex::Regex;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

pub const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);
pub const CLIENT_RETRIES: u32 = 3;
pub const CLIENT_RETRY_DELAY: Duration = Duration::from_secs(1);
/// client 总超时 = duration + 该值
pub const CLIENT_EXTRA_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------- 命令构造 ----------------

pub fn server_args(req: &IperfServerStartReq) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-s".into(),
        "-B".into(),
        req.bind_ip.clone(),
        "-p".into(),
        req.port.to_string(),
        "-i".into(),
        "1".into(),
        "-f".into(),
        "m".into(),
    ];
    a.push(if req.v6 { "-6".into() } else { "-4".into() });
    a
}

pub fn client_args(req: &IperfClientReq) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-c".into(),
        req.dst.clone(),
        "-B".into(),
        req.bind_ip.clone(),
        "-p".into(),
        req.port.to_string(),
        "-t".into(),
        req.duration.to_string(),
        "-i".into(),
        "1".into(),
        "-f".into(),
        "m".into(),
    ];
    a.push(if req.v6 { "-6".into() } else { "-4".into() });
    if req.udp {
        a.push("-u".into());
    }
    a.extend(req.extra.iter().cloned());
    a
}

fn cmdline(bin: &str, args: &[String]) -> String {
    format!("{} {}", bin, args.join(" "))
}

fn supports_forceflush(bin: &str) -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        run_cmd(bin, &["--help"], Duration::from_secs(8))
            .merged()
            .contains("--forceflush")
    })
}

// ---------------- 输出解析 ----------------

#[derive(Debug, Default, Clone)]
pub struct IperfParsed {
    pub sender_mbps: Option<f64>,
    pub receiver_mbps: Option<f64>,
    /// 兜底：最后一行出现的速率
    pub last_mbps: Option<f64>,
    pub udp_loss_pct: Option<f64>,
    pub udp_lost_datagrams: Option<u64>,
    pub udp_total_datagrams: Option<u64>,
}

impl IperfParsed {
    pub fn best_sender(&self) -> Option<f64> {
        self.sender_mbps.or(self.last_mbps)
    }
    pub fn best_receiver(&self) -> Option<f64> {
        self.receiver_mbps.or(self.last_mbps)
    }
    pub fn has_measurement(&self) -> bool {
        [self.sender_mbps, self.receiver_mbps, self.last_mbps]
            .iter()
            .any(|v| v.map(|x| x > 0.0).unwrap_or(false))
    }
}

/// 解析 iperf3 文本输出（-f m）
pub fn parse_output(text: &str) -> IperfParsed {
    let ansi = Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("regex");
    let rate_re = Regex::new(r"(\d+(?:[.,]\d+)?)\s*([KMGT]?)(bits|Bytes)/sec").expect("regex");
    let loss_re = Regex::new(r"\((\d+(?:[.,]\d+)?)%\)").expect("regex");
    let loss_count_re = Regex::new(r"(\d+)\s*/\s*(\d+)\s*\((\d+(?:[.,]\d+)?)%\)").expect("regex");

    let mut p = IperfParsed::default();
    for raw_line in text.lines() {
        let line = ansi.replace_all(raw_line, "");
        let mut last: Option<f64> = None;
        for cap in rate_re.captures_iter(&line) {
            let num: f64 = cap[1].replace(',', ".").parse().unwrap_or(0.0);
            let unit = &cap[2];
            let kind = &cap[3];
            let mut mbps = match unit {
                "K" => num / 1000.0,
                "M" => num,
                "G" => num * 1000.0,
                "T" => num * 1_000_000.0,
                _ => num / 1_000_000.0,
            };
            if kind == "Bytes" {
                mbps *= 8.0;
            }
            last = Some(mbps);
        }
        if let Some(v) = last {
            if line.contains("sender") {
                p.sender_mbps = Some(v);
            } else if line.contains("receiver") {
                p.receiver_mbps = Some(v);
            } else {
                p.last_mbps = Some(v);
            }
        }
    }
    if let Some(cap) = loss_re.captures_iter(text).last() {
        p.udp_loss_pct = cap[1].replace(',', ".").parse().ok();
    }
    if let Some(cap) = loss_count_re.captures_iter(text).last() {
        p.udp_lost_datagrams = cap[1].parse().ok();
        p.udp_total_datagrams = cap[2].parse().ok();
        p.udp_loss_pct = cap[3].replace(',', ".").parse().ok();
    }
    p
}

// ---------------- server 进程管理 ----------------

struct SrvEntry {
    child: Child,
    /// 收集到的输出（reader thread 写入）
    output: Arc<Mutex<Vec<u8>>>,
    readers: Vec<std::thread::JoinHandle<()>>,
    started: Instant,
    cmd: String,
}

/// iperf3 server 注册表（agent 端与主控本地共用）
pub struct IperfServerMgr {
    inner: Mutex<HashMap<u16, SrvEntry>>,
}

impl Default for IperfServerMgr {
    fn default() -> Self {
        Self::new()
    }
}

impl IperfServerMgr {
    pub fn new() -> Self {
        IperfServerMgr {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 启动 server（不带 -1，运行后由调用方主动 stop），TCP connect 探测就绪
    pub fn start(&self, bin: &str, req: &IperfServerStartReq) -> Result<String, String> {
        // 同端口旧的先杀掉
        self.stop(req.port, Duration::from_secs(0));

        let args = server_args(req);
        let cmd_str = cmdline(bin, &args);
        let mut child = Command::new(bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("启动 iperf3 server 失败: {e} (命令: {cmd_str})"))?;

        let output_arc: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

        let mut readers = Vec::new();
        // 后台线程：逐行读 stdout/stderr 并收集，stop 时 join 后再生成报告。
        if let Some(stdout) = child.stdout.take() {
            let output_clone = Arc::clone(&output_arc);
            readers.push(std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    let mut buf = output_clone.lock().unwrap();
                    writeln!(buf, "{line}").ok();
                }
            }));
        }
        if let Some(stderr) = child.stderr.take() {
            let output_clone = Arc::clone(&output_arc);
            readers.push(std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    let mut buf = output_clone.lock().unwrap();
                    writeln!(buf, "[stderr] {line}").ok();
                }
            }));
        }

        {
            let mut g = self.inner.lock().unwrap();
            g.insert(
                req.port,
                SrvEntry {
                    child,
                    output: output_arc,
                    readers,
                    started: Instant::now(),
                    cmd: cmd_str.clone(),
                },
            );
        }

        // 等待就绪：普通地址用 TCP connect；IPv6 link-local 的 scope 语法在各平台
        // 不一致，改为短暂等待后确认进程仍存活，client 侧另有连接重试兜底。
        let clean_bind = req
            .bind_ip
            .split('%')
            .next()
            .unwrap_or(&req.bind_ip)
            .to_lowercase();
        let ready = if clean_bind.starts_with("fe80:") {
            std::thread::sleep(Duration::from_millis(300));
            let mut guard = self.inner.lock().unwrap();
            let entry = guard
                .get_mut(&req.port)
                .ok_or_else(|| format!("iperf3 server 端口 {} 状态丢失", req.port))?;
            match entry.child.try_wait() {
                Ok(None) => Ok(()),
                Ok(Some(status)) => Err(format!(
                    "iperf3 server 端口 {} 启动后立即退出: {status}",
                    req.port
                )),
                Err(e) => Err(format!("检查 iperf3 server 进程失败: {e}")),
            }
        } else {
            wait_server_tcp_ready(req.bind_ip.clone(), req.port, SERVER_READY_TIMEOUT)
        };
        if let Err(e) = ready {
            let stopped = self.stop(req.port, Duration::from_secs(0));
            let detail = stopped
                .output
                .lines()
                .take(20)
                .collect::<Vec<_>>()
                .join("\n");
            return Err(if detail.trim().is_empty() {
                e
            } else {
                format!("{e}\n{detail}")
            });
        }

        Ok(cmd_str)
    }
}

impl IperfServerMgr {
    pub fn stop(&self, port: u16, wait: Duration) -> IperfServerStopOut {
        let entry = {
            let mut g = self.inner.lock().unwrap();
            g.remove(&port)
        };
        let Some(mut e) = entry else {
            return IperfServerStopOut {
                existed: false,
                output: String::new(),
            };
        };
        let exited = if wait > Duration::from_secs(0) {
            matches!(e.child.wait_timeout(wait), Ok(Some(_)))
        } else {
            matches!(e.child.try_wait(), Ok(Some(_)))
        };
        if !exited {
            let _ = e.child.kill();
            let _ = e.child.wait();
        }
        for reader in e.readers.drain(..) {
            let _ = reader.join();
        }
        let output = e.output.lock().unwrap().clone();
        let output = decode_bytes(&output);
        let output = format!("$ {}\n{}", e.cmd, output);
        IperfServerStopOut {
            existed: true,
            output,
        }
    }

    /// 清理超龄 server（防泄漏）
    pub fn sweep(&self, max_age: Duration) {
        let ports: Vec<u16> = {
            let g = self.inner.lock().unwrap();
            g.iter()
                .filter(|(_, e)| e.started.elapsed() > max_age)
                .map(|(p, _)| *p)
                .collect()
        };
        for p in ports {
            let _ = self.stop(p, Duration::from_secs(0));
        }
    }

    pub fn stop_all(&self) {
        let ports: Vec<u16> = {
            let g = self.inner.lock().unwrap();
            g.keys().copied().collect()
        };
        for p in ports {
            let _ = self.stop(p, Duration::from_secs(0));
        }
    }
}

// ---------------- client 执行 ----------------

use std::time::Duration as StdDuration;

/// TCP connect 探测 iperf3 server 是否已就绪（兼容 IPv4 / IPv6，跨平台）
fn wait_server_tcp_ready(bind_ip: String, port: u16, timeout: StdDuration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    // probe 地址要去掉 zone（%en0 / %6），getaddrinfo 不支持带 zone 解析
    let clean = if let Some(idx) = bind_ip.find('%') {
        &bind_ip[..idx]
    } else {
        &bind_ip
    };
    // IPv6 需要用 [addr]:port 格式
    let addr_str = if clean.contains(':') {
        format!("[{clean}]:{port}")
    } else {
        format!("{clean}:{port}")
    };
    while Instant::now() < deadline {
        let addrs = addr_str
            .to_socket_addrs()
            .map_err(|e| format!("解析地址失败 {addr_str}: {e}"))?;
        // 取第一个可用的地址
        if let Some(sa) = addrs.last() {
            match TcpStream::connect_timeout(&sa, StdDuration::from_secs(1)) {
                Ok(_) => return Ok(()),
                Err(_e) => {
                    // ConnectionRefused 正常（server 还没好）
                    std::thread::sleep(StdDuration::from_millis(200));
                }
            }
        } else {
            return Err(format!("无法解析地址 {addr_str}"));
        }
    }
    Err(format!(
        "iperf3 server 端口 {port} 在 {:.1} 秒内未响应 TCP connect",
        timeout.as_secs_f64()
    ))
}

fn is_transient_error(out: &str) -> bool {
    let l = out.to_lowercase();
    l.contains("connection refused")
        || l.contains("unable to connect to server")
        || l.contains("server is busy running a test")
}

fn live_rate(line: &str) -> Option<f64> {
    static RATE_RE: OnceLock<Regex> = OnceLock::new();
    let re = RATE_RE.get_or_init(|| {
        Regex::new(r"(\d+(?:[.,]\d+)?)\s*([KMGT]?)(bits|Bytes)/sec").expect("regex")
    });
    let cap = re.captures_iter(line).last()?;
    let num: f64 = cap[1].replace(',', ".").parse().ok()?;
    let mut mbps = match &cap[2] {
        "K" => num / 1000.0,
        "M" => num,
        "G" => num * 1000.0,
        "T" => num * 1_000_000.0,
        _ => num / 1_000_000.0,
    };
    if &cap[3] == "Bytes" {
        mbps *= 8.0;
    }
    Some(mbps)
}

fn classify_live_line(line: &str, elapsed_ms: u64) -> Option<IperfFlowEvent> {
    let lower = line.to_lowercase();
    if lower.contains("connected to") {
        return Some(IperfFlowEvent {
            kind: IperfEventKind::Connected,
            elapsed_ms,
            mbps: None,
            line: line.to_string(),
        });
    }
    if lower.contains("error") || lower.contains("failed") || lower.contains("unable to") {
        return Some(IperfFlowEvent {
            kind: IperfEventKind::Error,
            elapsed_ms,
            mbps: None,
            line: line.to_string(),
        });
    }
    let mbps = live_rate(line)?;
    if mbps > 0.0 {
        return Some(IperfFlowEvent {
            kind: IperfEventKind::Traffic,
            elapsed_ms,
            mbps: Some(mbps),
            line: line.to_string(),
        });
    }
    None
}

/// 执行 iperf3 client，逐行回调并上报结构化事件。
/// cancel 用于异步 job 主动终止，瞬态连接错误仍保留原有自动重试。
pub fn run_client_controlled<F, E>(
    bin: &str,
    req: &IperfClientReq,
    cancel: Option<&AtomicBool>,
    mut on_line: F,
    mut on_event: E,
) -> IperfClientOut
where
    F: FnMut(&str),
    E: FnMut(IperfFlowEvent),
{
    let mut args = client_args(req);
    // stdout 接到 pipe 后部分 iperf3 会块缓冲，几十秒后才吐 interval，
    // 事件时间线会被整体推迟。只在当前二进制明确支持时开启逐 interval flush，
    // 保持对更老 Windows 版本的兼容。
    if supports_forceflush(bin) {
        args.push("--forceflush".into());
    }
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let cmd_str = cmdline(bin, &args);
    let timeout = Duration::from_secs(req.duration) + CLIENT_EXTRA_TIMEOUT;
    let started = Instant::now();
    on_event(IperfFlowEvent {
        kind: IperfEventKind::Started,
        elapsed_ms: 0,
        mbps: None,
        line: cmd_str.clone(),
    });

    let mut last = IperfClientOut {
        ok: false,
        timed_out: false,
        cancelled: false,
        cmd: cmd_str.clone(),
        output: String::new(),
    };
    for attempt in 1..=CLIENT_RETRIES {
        let out = run_streaming_controlled(bin, &args_ref, timeout, cancel, |line| {
            on_line(line);
            if let Some(event) = classify_live_line(line, started.elapsed().as_millis() as u64) {
                on_event(event);
            }
        });
        let merged = out.merged();
        last = IperfClientOut {
            ok: out.ok,
            timed_out: out.timed_out,
            cancelled: out.cancelled,
            cmd: cmd_str.clone(),
            output: merged.clone(),
        };
        if out.ok || out.timed_out || out.cancelled {
            break;
        }
        if attempt < CLIENT_RETRIES && is_transient_error(&merged) {
            let message = format!(
                "(第 {attempt} 次连接失败，{}s 后重试...)",
                CLIENT_RETRY_DELAY.as_secs()
            );
            on_line(&message);
            on_event(IperfFlowEvent {
                kind: IperfEventKind::Retry,
                elapsed_ms: started.elapsed().as_millis() as u64,
                mbps: None,
                line: message,
            });
            std::thread::sleep(CLIENT_RETRY_DELAY);
            continue;
        }
        break;
    }
    on_event(IperfFlowEvent {
        kind: IperfEventKind::Ended,
        elapsed_ms: started.elapsed().as_millis() as u64,
        mbps: None,
        line: if last.ok {
            "iperf3 client completed".into()
        } else if last.cancelled {
            "iperf3 client cancelled".into()
        } else if last.timed_out {
            "iperf3 client timed out".into()
        } else {
            "iperf3 client failed".into()
        },
    });
    last
}

/// 兼容旧调用点：同步运行，无取消信号，仅保留逐行回调。
pub fn run_client<F: FnMut(&str)>(bin: &str, req: &IperfClientReq, on_line: F) -> IperfClientOut {
    run_client_controlled(bin, req, None, on_line, |_| {})
}

#[derive(Clone)]
struct ClientJobEntry {
    events: Arc<Mutex<Vec<IperfFlowEvent>>>,
    result: Arc<Mutex<Option<IperfClientOut>>>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

/// 异步 iperf client 作业管理器。
///
/// HTTP 请求只负责创建/查询/停止 job，不再占住 agent 的固定 worker
/// 直到 -t 结束，因此 20/32 条远端流可以真正同时运行。
pub struct IperfClientJobMgr {
    inner: Mutex<HashMap<String, ClientJobEntry>>,
    seq: AtomicU64,
}

impl Default for IperfClientJobMgr {
    fn default() -> Self {
        Self::new()
    }
}

impl IperfClientJobMgr {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
        }
    }

    pub fn start(&self, bin: String, req: IperfClientReq) -> String {
        self.start_job(move |cancel, events| {
            let event_sink = Arc::clone(&events);
            run_client_controlled(
                &bin,
                &req,
                Some(cancel.as_ref()),
                |_| {},
                move |event| {
                    if let Ok(mut g) = event_sink.lock() {
                        g.push(event);
                    }
                },
            )
        })
    }

    fn start_job<F>(&self, runner: F) -> String
    where
        F: FnOnce(Arc<AtomicBool>, Arc<Mutex<Vec<IperfFlowEvent>>>) -> IperfClientOut
            + Send
            + 'static,
    {
        let id = format!("cli{}", self.seq.fetch_add(1, Ordering::SeqCst));
        let events = Arc::new(Mutex::new(Vec::new()));
        let result = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        self.inner.lock().unwrap().insert(
            id.clone(),
            ClientJobEntry {
                events: Arc::clone(&events),
                result: Arc::clone(&result),
                cancel: Arc::clone(&cancel),
                started: Instant::now(),
            },
        );

        std::thread::spawn(move || {
            let out = runner(cancel, events);
            if let Ok(mut g) = result.lock() {
                *g = Some(out);
            }
        });
        id
    }

    pub fn status(&self, id: &str, cursor: usize) -> Result<IperfClientStatusOut, String> {
        let entry = self
            .inner
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("iperf client job 不存在: {id}"))?;
        let events_guard = entry.events.lock().unwrap();
        let from = cursor.min(events_guard.len());
        let events = events_guard[from..].to_vec();
        let next_cursor = events_guard.len();
        drop(events_guard);
        let result = entry.result.lock().unwrap().clone();
        Ok(IperfClientStatusOut {
            id: id.to_string(),
            done: result.is_some(),
            next_cursor,
            events,
            result,
        })
    }

    pub fn stop(&self, id: &str) -> Result<(bool, bool), String> {
        let entry = self
            .inner
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| format!("iperf client job 不存在: {id}"))?;
        let was_done = entry.result.lock().unwrap().is_some();
        entry.cancel.store(true, Ordering::SeqCst);
        Ok((true, was_done))
    }

    pub fn sweep(&self, max_age: Duration) {
        let expired: Vec<String> = {
            let g = self.inner.lock().unwrap();
            g.iter()
                .filter(|(_, entry)| entry.started.elapsed() > max_age)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in expired {
            if let Some(entry) = self.inner.lock().unwrap().remove(&id) {
                entry.cancel.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{mpsc, Condvar};

    const TCP_SAMPLE: &str = r#"
Connecting to host 192.168.1.3, port 56000
[  5] local 192.168.1.2 port 52822 connected to 192.168.1.3 port 56000
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-1.00   sec   283 MBytes  2372 Mbits/sec
[  5]   1.00-2.00   sec   285 MBytes  2389 Mbits/sec
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-10.00  sec  2.77 GBytes  2379 Mbits/sec                  sender
[  5]   0.00-10.04  sec  2.77 GBytes  2368 Mbits/sec                  receiver

iperf Done.
"#;

    #[test]
    fn test_parse_tcp() {
        let p = parse_output(TCP_SAMPLE);
        assert_eq!(p.sender_mbps, Some(2379.0));
        assert_eq!(p.receiver_mbps, Some(2368.0));
        assert!(p.has_measurement());
    }

    const UDP_SAMPLE: &str = r#"
[  5]   0.00-1.00   sec  11.9 MBytes  99.9 Mbits/sec  8630
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate         Jitter    Lost/Total Datagrams
[  5]   0.00-10.00  sec   119 MBytes  100 Mbits/sec  0.000 ms  0/86380 (0%)  sender
[  5]   0.00-10.04  sec   119 MBytes  99.6 Mbits/sec  0.014 ms  312/86380 (0.36%)  receiver

iperf Done.
"#;

    #[test]
    fn test_parse_udp() {
        let p = parse_output(UDP_SAMPLE);
        assert_eq!(p.sender_mbps, Some(100.0));
        assert_eq!(p.receiver_mbps, Some(99.6));
        assert_eq!(p.udp_loss_pct, Some(0.36));
        assert_eq!(p.udp_lost_datagrams, Some(312));
        assert_eq!(p.udp_total_datagrams, Some(86380));
    }

    #[test]
    fn test_parse_gbits_and_bytes() {
        let p = parse_output("[  5]  0.00-10.00 sec  2.77 GBytes  2.38 Gbits/sec  sender\n");
        assert_eq!(p.sender_mbps, Some(2380.0));
        let p2 = parse_output("[  5]  0.0-1.0 sec  1.00 MBytes/sec\n");
        assert_eq!(p2.last_mbps, Some(8.0));
    }

    #[test]
    fn test_parse_empty() {
        let p = parse_output("iperf3: error - unable to connect to server\n");
        assert!(!p.has_measurement());
    }

    #[test]
    fn test_transient() {
        assert!(is_transient_error("iperf3: error - Connection refused"));
        assert!(is_transient_error(
            "iperf3: error - the server is busy running a test. try again later"
        ));
        assert!(!is_transient_error("iperf3: error - bad file descriptor"));
    }

    #[test]
    fn test_args() {
        let req = IperfClientReq {
            dst: "192.168.1.3".into(),
            bind_ip: "192.168.1.2".into(),
            port: 56001,
            duration: 120,
            udp: true,
            v6: false,
            extra: vec!["-b".into(), "500m".into()],
        };
        let a = client_args(&req);
        assert_eq!(
            a.join(" "),
            "-c 192.168.1.3 -B 192.168.1.2 -p 56001 -t 120 -i 1 -f m -4 -u -b 500m"
        );
        let sreq = IperfServerStartReq {
            bind_ip: "fe80::1%12".into(),
            port: 56001,
            v6: true,
        };
        let sa = server_args(&sreq);
        assert_eq!(sa.join(" "), "-s -B fe80::1%12 -p 56001 -i 1 -f m -6");
    }

    #[test]
    fn test_job_manager_allows_32_concurrent_clients() {
        const JOBS: usize = 32;
        let mgr = IperfClientJobMgr::new();
        let active = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let mut ids = Vec::new();

        for _ in 0..JOBS {
            let active = Arc::clone(&active);
            let gate = Arc::clone(&gate);
            let started_tx = started_tx.clone();
            ids.push(mgr.start_job(move |_cancel, events| {
                active.fetch_add(1, Ordering::SeqCst);
                events.lock().unwrap().push(IperfFlowEvent {
                    kind: IperfEventKind::Started,
                    line: "fake client started".into(),
                    ..Default::default()
                });
                let _ = started_tx.send(());

                let (lock, cv) = &*gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cv.wait(released).unwrap();
                }
                active.fetch_sub(1, Ordering::SeqCst);
                IperfClientOut {
                    ok: true,
                    output: "fake client completed".into(),
                    ..Default::default()
                }
            }));
        }
        drop(started_tx);

        let mut all_started = true;
        for _ in 0..JOBS {
            if started_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                all_started = false;
                break;
            }
        }
        let active_at_barrier = active.load(Ordering::SeqCst);
        {
            let (lock, cv) = &*gate;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }

        assert!(all_started, "32 个异步 client 未能及时全部启动");
        assert_eq!(active_at_barrier, JOBS);

        let deadline = Instant::now() + Duration::from_secs(2);
        for id in &ids {
            loop {
                let status = mgr.status(id, 0).unwrap();
                if status.done {
                    assert_eq!(status.events.len(), 1);
                    assert!(status.result.unwrap().ok);
                    break;
                }
                assert!(Instant::now() < deadline, "job {id} 未及时结束");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        for id in ids {
            assert_eq!(mgr.stop(&id).unwrap(), (true, true));
        }
    }
}
