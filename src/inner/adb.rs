use super::config::{iface_word, safe_word, InnerConfig};
use crate::nic::counter::NicCounterReader;
use crate::util::{configure_managed_command, run_cmd};
use serde::Serialize;
use std::fs::File;
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

/// 单条 ADB shell 的默认预算：够读计数器，不够跑探测。
const SHELL_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone)]
pub struct Adb {
    pub program: String,
    pub serial: String,
}

pub fn select_serial(text: &str, requested: &str) -> Result<String, String> {
    let devices: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?;
            let state = parts.next()?;
            (serial != "List" && serial != "*").then_some((serial, state))
        })
        .collect();
    if !requested.is_empty() {
        return match devices.iter().find(|(serial, _)| *serial == requested) {
            Some((_, "device")) => Ok(requested.into()),
            Some((_, state)) => Err(format!(
                "ADB 设备 {requested} 状态为 {state}，请检查授权/连接"
            )),
            None => Err(format!("ADB 中找不到设备 {requested}")),
        };
    }
    // 即使只有一台已授权，也不猜它是不是用户想测的那台。
    if devices.len() != 1 {
        return Err(format!(
            "ADB 检出 {} 台设备，请在配置里填写 serial",
            devices.len()
        ));
    }
    select_serial(text, devices[0].0)
}

impl Adb {
    pub fn connect(cfg: &InnerConfig) -> Result<Self, String> {
        // 校验的是 trim 之后的值（`config::adb_program`），执行的也必须是同一个。
        // 从表格里粘出来的路径常带首尾空格：不 trim 的话校验放行、`Command::new`
        // 却去找一个名字带空格的文件，报出来的是「ADB 设备枚举失败」——指向设备，
        // 真因却在路径。第一条命令就得用这个值，不能等到构造结构体时才 trim。
        let program = cfg.adb_path.trim();
        let out = run_cmd(program, &["devices", "-l"], Duration::from_secs(10));
        if !out.ok {
            return Err(format!("ADB 设备枚举失败: {}", out.merged()));
        }
        let serial = select_serial(&out.stdout, &cfg.serial)?;
        if !safe_word(&serial) {
            return Err("ADB serial 含不支持的字符".into());
        }
        Ok(Self {
            program: program.to_string(),
            serial,
        })
    }

    pub fn shell(&self, script: &str) -> Result<String, String> {
        self.shell_within(script, SHELL_TIMEOUT)
    }

    /// 调用方自带预算的版本。
    ///
    /// [`Self::shell`] 的 4 秒对「读一个计数器」够用，但不是所有远端命令都这么
    /// 快：`iperf3 --help` 这类探测走的是 8 秒预算，被压回 4 秒之后，慢一点的
    /// adbd 上就会超时——而超时的后果不是报错，是**静默降级**（探测不到
    /// `--forceflush`，于是 iperf3 按块缓冲吐行，窗口推导退回到「按到达时刻
    /// 估计」那条更差的路）。谁定预算谁说了算，别在这层悄悄改。
    pub fn shell_within(&self, script: &str, timeout: Duration) -> Result<String, String> {
        // 一些嵌入式 adbd 没有 shell v2：远端命令失败，adb 仍返回 0。
        // 用独立子 shell 隔离 exit，再由本层检查末尾状态标记。
        let wrapped = format!("(\n{script}\n)\ncpe_inner_status=$?\nprintf '\\n__CPE_INNER_STATUS__:%s\\n' \"$cpe_inner_status\"");
        let out = run_cmd(
            &self.program,
            &["-s", &self.serial, "shell", &wrapped],
            timeout,
        );
        if out.ok {
            parse_shell_output(&out.stdout)
        } else {
            Err(format!("ADB shell 失败: {}", out.merged()))
        }
    }
}

pub fn parse_shell_output(text: &str) -> Result<String, String> {
    let normalized = text.replace("\r\n", "\n");
    let (output, status) = normalized
        .rsplit_once("\n__CPE_INNER_STATUS__:")
        .ok_or_else(|| "ADB shell 未返回完成标记，不能确认远端命令已成功执行".to_string())?;
    match status.trim().parse::<u32>() {
        Ok(0) => Ok(output.to_string()),
        Ok(code) => Err(format!("板侧命令退出码 {code}: {}", output.trim())),
        Err(_) => Err("ADB shell 完成标记无效".into()),
    }
}

/// 板侧字节计数的读取路径。
///
/// 两条路径通常**来自同一套驱动统计**，没有谁天生更准：`/proc/net/dev` 一次
/// 返回所有接口，适合批量采样；sysfs 每个接口一个文件，解析简单，用作标准
/// 路径不可用时的兼容读取。因此换路径**不能**修复硬件卸载导致的漏计——
/// 那种情况要换统计接口或换测量策略，不是换文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CounterSource {
    ProcNetDev,
    Sysfs,
}

impl CounterSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::ProcNetDev => "/proc/net/dev",
            Self::Sysfs => "/sys/class/net/<接口>/statistics",
        }
    }
}

/// 绑定了读取路径的板侧计数器。路径在预检时定一次，不在采样循环里试探——
/// 每秒换着读两条路径既慢又会让「读取失败」和「计数不动」混成一件事。
#[derive(Clone)]
pub struct BoardCounters {
    pub adb: Adb,
    pub source: CounterSource,
}

impl NicCounterReader for BoardCounters {
    fn read_counters(&self, iface: &str) -> Result<(u64, u64), String> {
        match self.source {
            // iface 只用于本地精确匹配，不拼入远端 shell。
            CounterSource::ProcNetDev => {
                parse_counters(&self.adb.shell("cat /proc/net/dev")?, iface)
            }
            CounterSource::Sysfs => {
                if !iface_word(iface) {
                    return Err(format!("板侧接口名 {iface} 不能用于 sysfs 路径"));
                }
                let text = self.adb.shell(&format!(
                    "cat '/sys/class/net/{iface}/statistics/rx_bytes' '/sys/class/net/{iface}/statistics/tx_bytes'"
                ))?;
                parse_sysfs_counters(&text, iface)
            }
        }
    }
}

/// sysfs 两个文件按顺序 cat 出来就是两行十进制。少一行、多一行、非数字都
/// 直接报错：这里宁可让本腿判 NOT_EVALUATED，也不能把 0 当成「没收到」。
pub fn parse_sysfs_counters(text: &str, iface: &str) -> Result<(u64, u64), String> {
    let values: Vec<&str> = text.split_whitespace().collect();
    if values.len() != 2 {
        return Err(format!(
            "板侧 {iface} 的 sysfs 统计应返回 rx_bytes、tx_bytes 两行，实际 {} 项",
            values.len()
        ));
    }
    let read = |raw: &str| {
        raw.parse::<u64>()
            .map_err(|_| format!("板侧 {iface} sysfs 字节计数非法: {raw}"))
    };
    Ok((read(values[0])?, read(values[1])?))
}

/// 板侧接口清单的一行。
#[derive(Debug, Clone, Serialize)]
pub struct BoardInterface {
    pub name: String,
    /// 该接口上的 IPv4 地址。统计接口**不必**自己持有被测 LAN 地址。
    pub addresses: Vec<String>,
    /// 所属网桥；空表示它不是桥成员。
    pub master: String,
    /// 作为网桥时的成员口。
    pub members: Vec<String>,
    pub proc_counters: bool,
    pub sysfs_counters: bool,
}

impl BoardInterface {
    /// 可用的读取路径，标准路径优先。
    pub fn counter_source(&self) -> Option<CounterSource> {
        if self.proc_counters {
            Some(CounterSource::ProcNetDev)
        } else if self.sysfs_counters {
            Some(CounterSource::Sysfs)
        } else {
            None
        }
    }
}

/// 一次读全板侧接口的名字、桥归属和 sysfs 计数可用性。
///
/// 只用 POSIX sh + cat/readlink，busybox 的 adbd 也能跑；不依赖 `ip -j`、
/// `bridge`、`awk`。任何一个接口读不到统计都不影响其他接口。
pub const INTERFACE_INVENTORY_SCRIPT: &str = r#"for d in /sys/class/net/*; do
  [ -e "$d" ] || continue
  n=${d##*/}
  m=''
  if [ -e "$d/master" ]; then m=$(readlink "$d/master" 2>/dev/null); m=${m##*/}; fi
  r=$(cat "$d/statistics/rx_bytes" 2>/dev/null)
  t=$(cat "$d/statistics/tx_bytes" 2>/dev/null)
  echo "$n|$m|$r|$t"
done"#;

/// 汇总板侧接口清单：地址来自 `ip -o -4 addr`，计数能力来自 `/proc/net/dev`
/// 与 sysfs，桥成员关系来自 `/sys/class/net/<成员>/master`。
///
/// **绝不**把 br0 和 eth1 的计数加在一起：桥和它的成员口多半在数同一批包，
/// 相加就是翻倍。这里只把候选列出来，由用户或地址归属挑一个。
pub fn board_interfaces(
    inventory: &str,
    addresses: &str,
    proc_net_dev: &str,
) -> Vec<BoardInterface> {
    let mut out: Vec<BoardInterface> = Vec::new();
    for line in inventory.lines() {
        let fields: Vec<&str> = line.trim().split('|').collect();
        if fields.len() != 4 || !iface_word(fields[0]) {
            continue;
        }
        out.push(BoardInterface {
            name: fields[0].to_string(),
            addresses: Vec::new(),
            master: if iface_word(fields[1]) {
                fields[1].to_string()
            } else {
                String::new()
            },
            members: Vec::new(),
            proc_counters: parse_counters(proc_net_dev, fields[0]).is_ok(),
            sysfs_counters: fields[2].parse::<u64>().is_ok() && fields[3].parse::<u64>().is_ok(),
        });
    }
    // sysfs 可能不可用；/proc 与地址表发现的接口仍须进入候选清单。
    let names = proc_net_dev
        .lines()
        .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim()))
        .chain(
            addresses
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1))
                .map(|name| name.split('@').next().unwrap_or(name)),
        );
    for name in names {
        if iface_word(name) && !out.iter().any(|iface| iface.name == name) {
            out.push(BoardInterface {
                name: name.into(),
                addresses: Vec::new(),
                master: String::new(),
                members: Vec::new(),
                proc_counters: parse_counters(proc_net_dev, name).is_ok(),
                sysfs_counters: false,
            });
        }
    }
    let masters: Vec<(String, String)> = out
        .iter()
        .filter(|iface| !iface.master.is_empty())
        .map(|iface| (iface.master.clone(), iface.name.clone()))
        .collect();
    for iface in &mut out {
        iface.members = masters
            .iter()
            .filter(|(master, _)| *master == iface.name)
            .map(|(_, member)| member.clone())
            .collect();
        iface.addresses = interface_addresses(addresses, &iface.name);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn interface_addresses(text: &str, iface: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            (fields.len() >= 4 && fields[2] == "inet" && fields[1].split('@').next() == Some(iface))
                .then(|| fields[3].to_string())
        })
        .collect()
}

/// 从 `/proc/net/dev` 里取指定接口的 RX/TX 累计字节。
///
/// 接口名精确匹配，不做前缀/包含匹配：`eth1` 和 `eth10` 只差一个字符，
/// 认错一个就是整条链路的速率认错了对象。
pub fn parse_counters(text: &str, iface: &str) -> Result<(u64, u64), String> {
    let line = text
        .lines()
        .find_map(|line| {
            let (name, values) = line.split_once(':')?;
            (name.trim() == iface).then_some(values)
        })
        .ok_or_else(|| format!("板侧计数器找不到接口 {iface}"))?;
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.len() < 16 {
        return Err(format!("板侧 {iface} 计数器字段不足"));
    }
    let read = |n: usize| {
        fields[n]
            .parse::<u64>()
            .map_err(|_| format!("板侧 {iface} 字节计数非法"))
    };
    Ok((read(0)?, read(8)?))
}

/// LAN 地址实际归属哪个板侧接口。
///
/// 必须唯一：同一个地址出现在两个接口上时不猜，因为 server 到底 bind 在
/// 哪一个、包从哪一个进出，直接决定该读谁的计数器。
pub fn address_interface(text: &str, ip: Ipv4Addr) -> Result<String, String> {
    let mut names = std::collections::BTreeSet::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 4
            && fields[2] == "inet"
            && fields[3]
                .split('/')
                .next()
                .and_then(|v| v.parse::<Ipv4Addr>().ok())
                == Some(ip)
        {
            let name = fields[1].split('@').next().unwrap_or("");
            if safe_word(name) {
                names.insert(name.to_string());
            }
        }
    }
    if names.len() != 1 {
        return Err(format!(
            "板侧地址 {ip} 必须唯一归属于一个接口，实际匹配 {} 个",
            names.len()
        ));
    }
    Ok(names.into_iter().next().unwrap())
}

/// 定下这条链路的板侧统计接口和读取路径。
///
/// `gateway_iface` 是 LAN 地址实际归属的接口，由调用方先用
/// [`address_interface`] 确认——**它只用于「没有指定统计接口时的默认值」**。
/// 指定了 `requested` 就用指定的：统计接口可以是桥、桥成员或别的逻辑口，
/// 不要求它自己持有这个 LAN 地址。两种情况都要确认该接口真的读得出计数，
/// 否则提前报错，而不是等跑完一轮拿回一堆 NOT_EVALUATED。
pub fn resolve_rx_interface(
    requested: &str,
    gateway_iface: &str,
    interfaces: &[BoardInterface],
) -> Result<(String, CounterSource), String> {
    let name = if requested.is_empty() {
        gateway_iface.to_string()
    } else {
        requested.to_string()
    };
    let Some(iface) = interfaces.iter().find(|item| item.name == name) else {
        let known: Vec<&str> = interfaces.iter().map(|item| item.name.as_str()).collect();
        return Err(format!(
            "板侧没有接口 {name}；已发现的接口：{}",
            known.join("、")
        ));
    };
    let source = iface.counter_source().ok_or_else(|| {
        let candidates = rx_candidates(gateway_iface, interfaces);
        format!(
            "板侧 {name} 既读不到 /proc/net/dev 也读不到 sysfs 字节计数。\
             这条 LAN 地址附近可计数的接口有：{}；\
             也可以把这条链路改成工具接收速率策略",
            if candidates.is_empty() {
                "（一个都没有）".to_string()
            } else {
                candidates.join("、")
            }
        )
    })?;
    Ok((name, source))
}

/// 统计接口的候选清单，供页面在「自动识别有歧义」时让用户明确指定。
///
/// 桥和它的成员口都列出来，但**从不**替用户把它们加起来：那多半是把同一批
/// 包数了两遍。也不因为「物理口听起来更底层」就认为它比桥口可信。
pub fn rx_candidates(gateway_iface: &str, interfaces: &[BoardInterface]) -> Vec<String> {
    let mut out = vec![gateway_iface.to_string()];
    if let Some(iface) = interfaces.iter().find(|item| item.name == gateway_iface) {
        out.extend(iface.members.iter().cloned());
        if !iface.master.is_empty() {
            out.push(iface.master.clone());
        }
    }
    out.retain(|name| {
        interfaces
            .iter()
            .any(|item| item.name == *name && item.counter_source().is_some())
    });
    // 保序去重，不能用 `dedup()`：它只消**相邻**的重复。清单是
    // `[网关口, ...成员, master]` 拼出来的，网关口本身是桥、而成员表又把它列
    // 回来时（两级桥就会这样），重复的两项并不相邻，`dedup()` 原样放过。
    // 结果是「可计数的接口有：br0、eth1、br0」——正在排查计数来源的人会以为
    // 那是两个不同的接口。
    let mut seen = std::collections::HashSet::new();
    out.retain(|name| seen.insert(name.clone()));
    out
}

/// 前台 shell 持有自己的 server PID。只停止本次进程；无 killall/pkill。
/// 停止标记用于正常收尾；HUP trap 与有限租约覆盖 ADB 断开和 PC 异常退出。
pub fn server_script(bin: &str, ip: Ipv4Addr, port: u16, dir: &str, lease: u64) -> String {
    format!(
        r#"umask 077
mkdir '{dir}' || exit 70
p=''
cleanup() {{
  if [ -n "$p" ]; then kill "$p" 2>/dev/null; wait "$p" 2>/dev/null; fi
  rm -f '{dir}/stop' '{dir}/ready'
  rmdir '{dir}'
}}
trap cleanup EXIT
trap 'exit 0' HUP INT TERM
'{bin}' -s -4 -B {ip} -p {port} -i 1 -f m &
p=$!
i=0
while [ "$i" -lt {lease} ] && kill -0 "$p" 2>/dev/null && [ ! -f '{dir}/stop' ]; do
  sleep 1
  i=$((i + 1))
  if kill -0 "$p" 2>/dev/null; then : > '{dir}/ready'; fi
done
"#
    )
}

pub struct Server {
    adb: Adb,
    dir: String,
    process: Child,
    closed: bool,
}

impl Server {
    /// 每条腿一套 server：独立端口、独立资源目录、独立日志。
    ///
    /// 双向单元两条腿同时在跑，共用一个端口的话两股流会撞进同一个 server
    /// 进程，日志和字节都分不出是哪条腿的。`owner` 已经把腿编进去了。
    pub fn start(
        adb: &Adb,
        cfg: &InnerConfig,
        ip: Ipv4Addr,
        port: u16,
        owner: &str,
        log: &Path,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let dir = format!("/tmp/cpe-inner-{owner}");
        let script = server_script(&cfg.board_iperf, ip, port, &dir, cfg.duration_secs + 150);
        let file = File::create(log).map_err(|e| e.to_string())?;
        let err = file.try_clone().map_err(|e| e.to_string())?;
        let mut command = Command::new(&adb.program);
        command
            .args(["-s", &adb.serial, "shell", &script])
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(err);
        configure_managed_command(&mut command);
        let process = command
            .spawn()
            .map_err(|e| format!("启动板侧 server 控制进程失败: {e}"))?;
        let mut server = Self {
            adb: adb.clone(),
            dir,
            process,
            closed: false,
        };
        let deadline = Instant::now() + Duration::from_secs(12);
        while Instant::now() < deadline && !cancel.load(Ordering::SeqCst) {
            if server
                .process
                .try_wait()
                .map_err(|e| e.to_string())?
                .is_some()
            {
                return Err(format!(
                    "板侧 server 提前退出: {}",
                    std::fs::read_to_string(log).unwrap_or_default()
                ));
            }
            // 只通过 ADB 确认自有 server 就绪；主控可能没有通往被测 LAN 的网口。
            if server
                .adb
                .shell(&format!("test -f '{}/ready'", server.dir))
                .is_ok()
            {
                std::thread::sleep(Duration::from_millis(1100));
                if server
                    .process
                    .try_wait()
                    .map_err(|e| e.to_string())?
                    .is_none()
                {
                    return Ok(server);
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err("板侧 iperf3 server 未就绪或已取消，请检查日志、地址和防火墙".into())
    }

    pub fn stop(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        let request = self.adb.shell(&format!(
            "if [ -d '{0}' ]; then : > '{0}/stop'; fi",
            self.dir
        ));
        let done = self
            .process
            .wait_timeout(Duration::from_secs(6))
            .map_err(|e| e.to_string())?;
        if done.is_none() {
            let _ = self.process.kill();
            let _ = self.process.wait();
            self.closed = true;
            return Err(format!(
                "未确认板侧 server 回收，已终止本机 ADB 控制进程；板侧租约到期将停止。{}",
                request.err().unwrap_or_default()
            ));
        }
        self.closed = true;
        request?;
        let gone = self.adb.shell(&format!("test ! -d '{}'", self.dir));
        gone.map(|_| ())
            .map_err(|_| "板侧 server 资源目录仍存在，禁止继续起流，请检查板侧日志".into())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if !self.closed {
            if let Err(error) = self.stop() {
                eprintln!("内环 server 清理: {error}");
            }
        }
    }
}
