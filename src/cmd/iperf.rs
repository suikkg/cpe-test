//! iperf3 命令构造、文本输出解析、server 进程管理、client 执行（带重试）
//!
//! 说明：统一用文本输出（-f m -i 1）而不是 --json，
//! 原因：--json 要等进程结束才输出（无实时速率），且旧版 Windows iperf3(3.1.x)
//! 不支持 --json-stream。文本模式对所有版本都稳定，且能实时逐行读速率。

use crate::protocol::{
    IperfClientOut, IperfClientReq, IperfClientStartReq, IperfClientStatusOut, IperfClientStopOut,
    IperfEventKind, IperfFlowEvent, IperfServerStartReq, IperfServerStopOut,
};
use crate::util::{decode_bytes, run_cmd, run_streaming_controlled};
use regex::Regex;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::BufReader;
use std::net::{TcpStream, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

pub const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);
pub const CLIENT_RETRIES: u32 = 3;
pub const CLIENT_RETRY_DELAY: Duration = Duration::from_secs(1);
/// client 总超时 = duration + 该值
pub const CLIENT_EXTRA_TIMEOUT: Duration = Duration::from_secs(120);
const SERVER_KILL_WAIT: Duration = Duration::from_secs(5);
const LIFECYCLE_TOMBSTONE_TTL: Duration = Duration::from_secs(10 * 60);
const DEFAULT_CLIENT_STOP_WAIT: Duration = Duration::from_secs(10);
const LIFECYCLE_LOCK_STRIPES: usize = 64;

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lifecycle_id_ok(id: &str) -> bool {
    id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn validate_lifecycle_id(label: &str, id: &str) -> Result<(), String> {
    if id.is_empty() || lifecycle_id_ok(id) {
        Ok(())
    } else {
        Err(format!(
            "{label} 非法：只允许 160 字节以内的字母、数字、-_.:"
        ))
    }
}

fn lease_deadline(lease_secs: u64) -> Result<Option<Instant>, String> {
    if lease_secs == 0 {
        return Ok(None);
    }
    Instant::now()
        .checked_add(Duration::from_secs(lease_secs))
        .map(Some)
        .ok_or_else(|| format!("资源 lease_secs={lease_secs} 过大，无法表示截止时间"))
}

fn lifecycle_lock_index(id: &str) -> usize {
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    hasher.finish() as usize % LIFECYCLE_LOCK_STRIPES
}

#[derive(Debug, Default)]
pub struct LifecycleCleanupResult {
    pub stopped: usize,
    pub errors: Vec<String>,
}

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
    expires_at: Option<Instant>,
    dynamic_lease: bool,
    cmd: String,
    request_id: String,
    owner_id: String,
    fingerprint: String,
    /// 只有本次 Child 通过就绪探测后才为 true；未就绪 entry 不可被重放
    /// start 当成成功实例复用。
    ready: bool,
}

#[derive(Clone)]
struct ServerTombstone {
    port: u16,
    stopped_at: Instant,
    out: IperfServerStopOut,
}

/// iperf3 server 注册表（agent 端与主控本地共用）
pub struct IperfServerMgr {
    inner: Mutex<HashMap<u16, SrvEntry>>,
    /// 只串行化同一端口的 start/stop；不同端口仍可并行准备。
    port_locks: Mutex<HashMap<u16, Arc<Mutex<()>>>>,
    /// 同一 request ID 即使错误地用于不同端口，也必须串行检查，保证全局唯一。
    request_locks: [Mutex<()>; LIFECYCLE_LOCK_STRIPES],
    /// 让 stop 重试可重放，并阻止 stop 后迟到的 start 复活同一 request。
    tombstones: Mutex<HashMap<String, ServerTombstone>>,
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
            port_locks: Mutex::new(HashMap::new()),
            request_locks: std::array::from_fn(|_| Mutex::new(())),
            tombstones: Mutex::new(HashMap::new()),
        }
    }

    fn port_lock(&self, port: u16) -> Arc<Mutex<()>> {
        Arc::clone(
            lock_recover(&self.port_locks)
                .entry(port)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    fn prune_tombstones(&self) {
        lock_recover(&self.tombstones)
            .retain(|_, t| t.stopped_at.elapsed() <= LIFECYCLE_TOMBSTONE_TTL);
    }

    fn server_fingerprint(req: &IperfServerStartReq) -> String {
        format!("{}|{}|{}|{}", req.bind_ip, req.port, req.v6, req.owner_id)
    }

    fn confirm_server_child_running(&self, port: u16, request_id: &str) -> Result<(), String> {
        let mut entries = lock_recover(&self.inner);
        let entry = entries
            .get_mut(&port)
            .ok_or_else(|| format!("iperf3 server 端口 {port} 状态丢失"))?;
        if entry.request_id != request_id {
            return Err(format!(
                "iperf3 server 端口 {port} 已切换到另一个 request_id"
            ));
        }
        match entry.child.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(status)) => Err(format!(
                "iperf3 server 端口 {port} 启动后立即退出: {status}"
            )),
            Err(e) => Err(format!("检查 iperf3 server 进程失败: {e}")),
        }
    }

    /// 启动 server（不带 -1，运行后由调用方主动 stop），TCP connect 探测就绪
    pub fn start(&self, bin: &str, req: &IperfServerStartReq) -> Result<String, String> {
        validate_lifecycle_id("request_id", &req.request_id)?;
        validate_lifecycle_id("owner_id", &req.owner_id)?;
        let requested_deadline = lease_deadline(req.lease_secs)?;
        self.prune_tombstones();

        let _request_guard = (!req.request_id.is_empty())
            .then(|| lock_recover(&self.request_locks[lifecycle_lock_index(&req.request_id)]));
        let port_lock = self.port_lock(req.port);
        let _port_guard = lock_recover(&port_lock);
        let fingerprint = Self::server_fingerprint(req);

        if req.request_id.is_empty() {
            // 旧协议语义：同端口旧实例先完整回收，再启动新实例。
            self.stop_locked(req.port, "", Duration::ZERO, false)?;
        } else {
            if let Some(tombstone) = lock_recover(&self.tombstones).get(&req.request_id) {
                return Err(if tombstone.port == req.port {
                    format!(
                        "iperf3 server request_id {} 已停止，拒绝迟到 start",
                        req.request_id
                    )
                } else {
                    format!(
                        "iperf3 server request_id {} 已用于端口 {}",
                        req.request_id, tombstone.port
                    )
                });
            }

            let mut dead_entry = None;
            let mut unready_live = false;
            {
                let mut entries = lock_recover(&self.inner);
                if let Some((other_port, _)) = entries
                    .iter()
                    .find(|(port, entry)| **port != req.port && entry.request_id == req.request_id)
                {
                    return Err(format!(
                        "iperf3 server request_id {} 已用于端口 {}",
                        req.request_id, other_port
                    ));
                }
                let mut remove_dead = false;
                if let Some(entry) = entries.get_mut(&req.port) {
                    if entry.request_id != req.request_id {
                        return Err(format!(
                            "iperf3 server 端口 {} 已由 request_id {} 占用",
                            req.port,
                            if entry.request_id.is_empty() {
                                "<legacy>"
                            } else {
                                &entry.request_id
                            }
                        ));
                    }
                    if entry.fingerprint != fingerprint {
                        return Err(format!(
                            "iperf3 server request_id {} 的重复 start 参数不一致",
                            req.request_id
                        ));
                    }
                    match entry.child.try_wait() {
                        Ok(None) => {
                            if entry.ready {
                                entry.expires_at = requested_deadline;
                                entry.dynamic_lease = req.lease_secs > 0;
                                return Ok(entry.cmd.clone());
                            }
                            unready_live = true;
                        }
                        Ok(Some(_)) => remove_dead = true,
                        Err(e) => {
                            return Err(format!(
                                "检查 iperf3 server request_id {} 状态失败: {e}",
                                req.request_id
                            ))
                        }
                    }
                }
                if remove_dead {
                    dead_entry = entries.remove(&req.port);
                }
            }
            if let Some(mut entry) = dead_entry {
                let _ = finish_server_output(&mut entry);
            }
            if unready_live {
                self.stop_locked(req.port, &req.request_id, Duration::ZERO, false)
                    .map_err(|cleanup_error| {
                        format!(
                            "iperf3 server request_id {} 上一次 start 未完成就绪且清理未确认: {cleanup_error}",
                            req.request_id
                        )
                    })?;
            }
        }

        let args = server_args(req);
        let cmd_str = cmdline(bin, &args);
        let child = Command::new(bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("启动 iperf3 server 失败: {e} (命令: {cmd_str})"))?;

        let output_arc: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let mut g = lock_recover(&self.inner);
            g.insert(
                req.port,
                SrvEntry {
                    child,
                    output: Arc::clone(&output_arc),
                    readers: Vec::new(),
                    started: Instant::now(),
                    expires_at: requested_deadline,
                    dynamic_lease: req.lease_secs > 0,
                    cmd: cmd_str.clone(),
                    request_id: req.request_id.clone(),
                    owner_id: req.owner_id.clone(),
                    fingerprint,
                    ready: false,
                },
            );
        }

        // 先把 Child 注册进 manager，再创建 reader。即使系统线程资源耗尽，
        // 失败清理仍持有 Child，绝不会因局部变量 drop 丢失可回收句柄。
        let reader_setup = (|| -> Result<(), String> {
            let mut entries = lock_recover(&self.inner);
            let entry = entries
                .get_mut(&req.port)
                .ok_or_else(|| format!("iperf3 server 端口 {} 状态丢失", req.port))?;
            let stdout = entry
                .child
                .stdout
                .take()
                .ok_or_else(|| "iperf3 server stdout pipe 缺失".to_string())?;
            let stdout_output = Arc::clone(&output_arc);
            let stdout_reader = std::thread::Builder::new()
                .name(format!("iperf-server-{}-stdout", req.port))
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    loop {
                        let mut line = Vec::new();
                        match std::io::BufRead::read_until(&mut reader, b'\n', &mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => lock_recover(&stdout_output).extend_from_slice(&line),
                        }
                    }
                })
                .map_err(|e| format!("创建 iperf3 server stdout reader 失败: {e}"))?;
            entry.readers.push(stdout_reader);

            let stderr = entry
                .child
                .stderr
                .take()
                .ok_or_else(|| "iperf3 server stderr pipe 缺失".to_string())?;
            let stderr_output = Arc::clone(&output_arc);
            let stderr_reader = std::thread::Builder::new()
                .name(format!("iperf-server-{}-stderr", req.port))
                .spawn(move || {
                    let mut reader = BufReader::new(stderr);
                    loop {
                        let mut line = Vec::new();
                        match std::io::BufRead::read_until(&mut reader, b'\n', &mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let mut output = lock_recover(&stderr_output);
                                output.extend_from_slice(b"[stderr] ");
                                output.extend_from_slice(&line);
                            }
                        }
                    }
                })
                .map_err(|e| format!("创建 iperf3 server stderr reader 失败: {e}"))?;
            entry.readers.push(stderr_reader);
            Ok(())
        })();
        if let Err(reader_error) = reader_setup {
            let cleanup = self.stop_locked(req.port, &req.request_id, Duration::ZERO, false);
            return Err(match cleanup {
                Ok(_) => reader_error,
                Err(cleanup_error) => {
                    format!("{reader_error}；失败后的 server 清理也未确认: {cleanup_error}")
                }
            });
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
            self.confirm_server_child_running(req.port, &req.request_id)
        } else {
            wait_server_tcp_ready(req.bind_ip.clone(), req.port, SERVER_READY_TIMEOUT, || {
                self.confirm_server_child_running(req.port, &req.request_id)
            })
            .and_then(|_| {
                // connect 可能碰巧连到外部遗留 listener；还必须确认本次 spawn 的
                // Child 没有因 bind 失败而退出，才能宣告 start 成功。给本次
                // spawn 一个很短的稳定期，避免外部 listener 先响应、而新 Child
                // 尚未来得及报告 address-in-use 的竞态。
                std::thread::sleep(Duration::from_millis(100));
                self.confirm_server_child_running(req.port, &req.request_id)
            })
        };
        if let Err(e) = ready {
            let cleanup = self.stop_locked(req.port, &req.request_id, Duration::ZERO, false);
            let detail = cleanup
                .as_ref()
                .ok()
                .map(|stopped| {
                    stopped
                        .output
                        .lines()
                        .take(20)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let cleanup_error = cleanup.err();
            return Err(if detail.trim().is_empty() {
                match cleanup_error {
                    Some(cleanup_error) => {
                        format!("{e}\n启动失败后的 server 清理也失败: {cleanup_error}")
                    }
                    None => e,
                }
            } else {
                format!("{e}\n{detail}")
            });
        }

        {
            let mut entries = lock_recover(&self.inner);
            let entry = entries
                .get_mut(&req.port)
                .ok_or_else(|| format!("iperf3 server 端口 {} 状态丢失", req.port))?;
            if entry.request_id != req.request_id {
                return Err(format!(
                    "iperf3 server 端口 {} 就绪后 request_id 已变化",
                    req.port
                ));
            }
            entry.ready = true;
        }

        Ok(cmd_str)
    }
    /// 旧调用点兼容包装。新 HTTP 路由应调用 stop_checked 并在 Err 时返回失败响应。
    #[allow(dead_code)]
    pub fn stop(&self, port: u16, wait: Duration) -> IperfServerStopOut {
        self.stop_checked(port, "", wait)
            .unwrap_or_else(|e| IperfServerStopOut {
                existed: true,
                terminated: false,
                output: format!("(iperf3 server 停止未确认: {e})"),
            })
    }

    /// 精确停止 request_id 对应的实例。成功返回即表示目标已不存在或已完成 wait 回收。
    pub fn stop_checked(
        &self,
        port: u16,
        request_id: &str,
        wait: Duration,
    ) -> Result<IperfServerStopOut, String> {
        validate_lifecycle_id("request_id", request_id)?;
        self.prune_tombstones();
        let _request_guard = (!request_id.is_empty())
            .then(|| lock_recover(&self.request_locks[lifecycle_lock_index(request_id)]));
        let port_lock = self.port_lock(port);
        let _port_guard = lock_recover(&port_lock);
        self.stop_locked(port, request_id, wait, true)
    }

    fn stop_locked(
        &self,
        port: u16,
        request_id: &str,
        wait: Duration,
        cache_tombstone: bool,
    ) -> Result<IperfServerStopOut, String> {
        if !request_id.is_empty() {
            if let Some(tombstone) = lock_recover(&self.tombstones).get(request_id) {
                if tombstone.port != port {
                    return Err(format!(
                        "iperf3 server request_id {request_id} 属于端口 {}，不是 {port}",
                        tombstone.port
                    ));
                }
                return Ok(tombstone.out.clone());
            }
        }

        let entry = {
            let mut entries = lock_recover(&self.inner);
            match entries.get(&port) {
                Some(entry) if !request_id.is_empty() && entry.request_id != request_id => None,
                Some(_) => entries.remove(&port),
                None => None,
            }
        };

        let Some(mut entry) = entry else {
            let out = IperfServerStopOut {
                existed: false,
                terminated: true,
                output: String::new(),
            };
            if cache_tombstone && !request_id.is_empty() {
                self.cache_server_tombstone(request_id, port, out.clone());
            }
            return Ok(out);
        };

        if let Err(e) = terminate_server_process(&mut entry, wait) {
            // 不能确认进程退出时必须保留 Child，下一次 stop 才能继续回收。
            lock_recover(&self.inner).insert(port, entry);
            return Err(e);
        }

        let output = finish_server_output(&mut entry);
        let out = IperfServerStopOut {
            existed: true,
            terminated: true,
            output,
        };
        if cache_tombstone && !request_id.is_empty() {
            self.cache_server_tombstone(request_id, port, out.clone());
        }
        Ok(out)
    }

    fn cache_server_tombstone(&self, request_id: &str, port: u16, out: IperfServerStopOut) {
        lock_recover(&self.tombstones).insert(
            request_id.to_string(),
            ServerTombstone {
                port,
                stopped_at: Instant::now(),
                out,
            },
        );
    }

    /// 清理超龄 server（防泄漏）
    pub fn sweep(&self, max_age: Duration) -> Vec<String> {
        self.prune_tombstones();
        let targets: Vec<(u16, String)> = {
            let g = lock_recover(&self.inner);
            g.iter()
                .filter(|(_, e)| {
                    if e.dynamic_lease {
                        e.expires_at
                            .map(|deadline| Instant::now() >= deadline)
                            .unwrap_or(false)
                    } else {
                        e.started.elapsed() > max_age
                    }
                })
                .map(|(p, e)| (*p, e.request_id.clone()))
                .collect()
        };
        let mut errors = Vec::new();
        for (port, request_id) in targets {
            if let Err(e) = self.stop_checked(port, &request_id, Duration::ZERO) {
                let message = format!("清理超龄 iperf3 server 端口 {port} 失败: {e}");
                eprintln!("[iperf] {message}");
                errors.push(message);
            }
        }
        errors
    }

    pub fn stop_owner(&self, owner_id: &str, wait: Duration) -> LifecycleCleanupResult {
        let mut result = LifecycleCleanupResult::default();
        if owner_id.is_empty() {
            result.errors.push("owner_id 不能为空".into());
            return result;
        }
        if let Err(e) = validate_lifecycle_id("owner_id", owner_id) {
            result.errors.push(e);
            return result;
        }
        let targets: Vec<(u16, String)> = {
            let entries = lock_recover(&self.inner);
            entries
                .iter()
                .filter(|(_, entry)| entry.owner_id == owner_id)
                .map(|(port, entry)| (*port, entry.request_id.clone()))
                .collect()
        };
        // 不同端口并行终止，批量 cleanup 的最坏等待接近一个 kill/wait
        // 周期，而不是流数 × 5 秒；同端口仍由 port_lock 串行保护。
        let stopped = std::thread::scope(|scope| {
            let handles: Vec<_> = targets
                .into_iter()
                .map(|(port, request_id)| {
                    (
                        port,
                        scope.spawn(move || self.stop_checked(port, &request_id, wait)),
                    )
                })
                .collect();
            handles
                .into_iter()
                .map(|(port, handle)| {
                    (
                        port,
                        handle
                            .join()
                            .unwrap_or_else(|_| Err(format!("server 端口 {port} 清理线程 panic"))),
                    )
                })
                .collect::<Vec<_>>()
        });
        for (port, stopped) in stopped {
            match stopped {
                Ok(out) if out.existed && out.terminated => result.stopped += 1,
                Ok(_) => {}
                Err(e) => result
                    .errors
                    .push(format!("server 端口 {port} 清理失败: {e}")),
            }
        }
        result
    }

    pub fn stop_all(&self) -> Vec<String> {
        let targets: Vec<(u16, String)> = {
            let g = lock_recover(&self.inner);
            g.iter()
                .map(|(port, entry)| (*port, entry.request_id.clone()))
                .collect()
        };
        let mut errors = Vec::new();
        for (port, request_id) in targets {
            if let Err(e) = self.stop_checked(port, &request_id, Duration::ZERO) {
                errors.push(format!("server 端口 {port} 清理失败: {e}"));
            }
        }
        errors
    }
}

fn terminate_server_process(entry: &mut SrvEntry, wait: Duration) -> Result<(), String> {
    let naturally_exited = if wait > Duration::ZERO {
        match entry.child.wait_timeout(wait) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                eprintln!("[iperf] 等待 server 自然退出失败，将尝试强制终止: {e}");
                false
            }
        }
    } else {
        match entry.child.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                return Err(format!("检查 iperf3 server 进程状态失败: {e}"));
            }
        }
    };
    if naturally_exited {
        return Ok(());
    }

    if let Err(kill_error) = entry.child.kill() {
        return match entry.child.try_wait() {
            Ok(Some(_)) => Ok(()),
            _ => Err(format!("强制终止 iperf3 server 失败: {kill_error}")),
        };
    }
    match entry.child.wait_timeout(SERVER_KILL_WAIT) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!(
            "iperf3 server kill 后 {} 秒仍未确认退出",
            SERVER_KILL_WAIT.as_secs()
        )),
        Err(e) => Err(format!("回收 iperf3 server 进程失败: {e}")),
    }
}

fn finish_server_output(entry: &mut SrvEntry) -> String {
    for reader in entry.readers.drain(..) {
        let _ = reader.join();
    }
    let output = lock_recover(&entry.output).clone();
    format!("$ {}\n{}", entry.cmd, decode_bytes(&output))
}

// ---------------- client 执行 ----------------

use std::time::Duration as StdDuration;

/// TCP connect 探测 iperf3 server 是否已就绪（兼容 IPv4 / IPv6，跨平台）
fn wait_server_tcp_ready<F>(
    bind_ip: String,
    port: u16,
    timeout: StdDuration,
    mut confirm_child_running: F,
) -> Result<(), String>
where
    F: FnMut() -> Result<(), String>,
{
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
        confirm_child_running()?;
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

fn append_attempt_output(history: &mut Vec<String>, attempt: u32, output: &str) {
    history.push(format!(
        "=== client attempt {attempt} ===\n{}",
        output.trim_end()
    ));
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

fn wait_cancelable(duration: Duration, cancel: Option<&AtomicBool>) -> bool {
    let Some(deadline) = Instant::now().checked_add(duration) else {
        return false;
    };
    loop {
        if cancel
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
        {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
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
    let Some(timeout) = Duration::from_secs(req.duration).checked_add(CLIENT_EXTRA_TIMEOUT) else {
        return IperfClientOut {
            ok: false,
            timed_out: false,
            cancelled: false,
            process_started: Some(false),
            cleanup_confirmed: Some(true),
            cmd: cmd_str,
            output: format!("iperf3 client duration={} 秒过大，无法执行", req.duration),
        };
    };
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
        process_started: Some(false),
        cleanup_confirmed: Some(true),
        cmd: cmd_str.clone(),
        output: String::new(),
    };
    let mut attempt_history = Vec::new();
    for attempt in 1..=CLIENT_RETRIES {
        let out = run_streaming_controlled(bin, &args_ref, timeout, cancel, |line| {
            on_line(line);
            if let Some(event) = classify_live_line(line, started.elapsed().as_millis() as u64) {
                on_event(event);
            }
        });
        let merged = out.merged();
        append_attempt_output(&mut attempt_history, attempt, &merged);
        last = IperfClientOut {
            ok: out.ok,
            timed_out: out.timed_out,
            cancelled: out.cancelled,
            process_started: Some(out.process_started()),
            cleanup_confirmed: Some(out.cleanup_confirmed()),
            cmd: cmd_str.clone(),
            output: merged.clone(),
        };
        if out.ok || out.timed_out || out.cancelled || !out.cleanup_confirmed() {
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
            if !wait_cancelable(CLIENT_RETRY_DELAY, cancel) {
                last.cancelled = true;
                attempt_history.push("iperf3 client cancelled before retry".into());
                break;
            }
            continue;
        }
        break;
    }
    last.output = attempt_history.join("\n");
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

struct ClientJobEntry {
    events: Arc<Mutex<Vec<IperfFlowEvent>>>,
    completion: Arc<ClientCompletion>,
    cancel: Arc<AtomicBool>,
    started: Instant,
    expires_at: Mutex<Option<Instant>>,
    dynamic_lease: AtomicBool,
    owner_id: String,
    fingerprint: String,
    thread: Mutex<ClientThreadState>,
    thread_cv: Condvar,
}

struct ClientCompletion {
    result: Mutex<Option<IperfClientOut>>,
    cv: Condvar,
}

#[derive(Default)]
struct ClientThreadState {
    installed: bool,
    handle: Option<std::thread::JoinHandle<()>>,
    joining: bool,
    joined: bool,
}

#[derive(Clone)]
struct ClientTombstone {
    stopped_at: Instant,
    out: IperfClientStopOut,
}

#[derive(Default)]
struct ClientRegistry {
    jobs: HashMap<String, Arc<ClientJobEntry>>,
    tombstones: HashMap<String, ClientTombstone>,
}

/// 异步 iperf client 作业管理器。
///
/// HTTP 请求只负责创建/查询/停止 job，不再占住 agent 的固定 worker
/// 直到 -t 结束，因此 20/32 条远端流可以真正同时运行。
pub struct IperfClientJobMgr {
    inner: Mutex<ClientRegistry>,
    /// 串行化同一 job ID 的 start/stop，避免 spawn 安装窗口内重复 start
    /// 先返回成功、随后首个 spawn 却失败的竞态；不同 ID 仍可并发。
    job_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
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
            inner: Mutex::new(ClientRegistry::default()),
            job_locks: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
        }
    }

    fn job_lock(&self, id: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            lock_recover(&self.job_locks)
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// 旧调用点兼容包装；新协议必须使用带 request_id/owner_id 的 start_request。
    #[allow(dead_code)]
    pub fn start(&self, bin: String, req: IperfClientReq) -> String {
        self.start_request(
            bin,
            IperfClientStartReq {
                request: req,
                request_id: String::new(),
                owner_id: String::new(),
                lease_secs: 0,
            },
        )
        .expect("legacy client job id generation must not conflict")
    }

    /// 使用 request_id 幂等创建远端 client job。非空 request_id 同时就是实际 job id，
    /// 因而即使 start 响应丢失，调用方仍可精确 stop。
    pub fn start_request(&self, bin: String, start: IperfClientStartReq) -> Result<String, String> {
        validate_lifecycle_id("request_id", &start.request_id)?;
        validate_lifecycle_id("owner_id", &start.owner_id)?;
        let fingerprint = format!(
            "{}|{}",
            start.owner_id,
            serde_json::to_string(&start.request)
                .map_err(|e| format!("序列化 client 请求失败: {e}"))?
        );
        let request_id = start.request_id;
        let owner_id = start.owner_id;
        let lease_secs = start.lease_secs;
        let req = start.request;
        self.start_job_managed(
            request_id,
            owner_id,
            lease_secs,
            fingerprint,
            move |cancel, events| {
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
            },
        )
    }

    /// 为其他受控灌包后端复用同一套幂等异步作业、租约、stop/join 与 owner 清理。
    /// runner 必须在 cancel=true 时终止并回收其子进程后再返回。
    pub(crate) fn start_external_request<F>(
        &self,
        request_id: String,
        owner_id: String,
        lease_secs: u64,
        fingerprint: String,
        runner: F,
    ) -> Result<String, String>
    where
        F: FnOnce(Arc<AtomicBool>, Arc<Mutex<Vec<IperfFlowEvent>>>) -> IperfClientOut
            + Send
            + 'static,
    {
        validate_lifecycle_id("request_id", &request_id)?;
        validate_lifecycle_id("owner_id", &owner_id)?;
        self.start_job_managed(
            request_id,
            owner_id,
            lease_secs,
            format!("external|{fingerprint}"),
            runner,
        )
    }

    #[cfg(test)]
    fn start_job<F>(&self, runner: F) -> String
    where
        F: FnOnce(Arc<AtomicBool>, Arc<Mutex<Vec<IperfFlowEvent>>>) -> IperfClientOut
            + Send
            + 'static,
    {
        self.start_job_managed(String::new(), String::new(), 0, String::new(), runner)
            .expect("legacy client job id generation must not conflict")
    }

    fn next_job_id(&self) -> String {
        loop {
            let id = format!("cli{}", self.seq.fetch_add(1, Ordering::SeqCst));
            let registry = lock_recover(&self.inner);
            if !registry.jobs.contains_key(&id) && !registry.tombstones.contains_key(&id) {
                return id;
            }
        }
    }

    fn prune_client_tombstones(&self) {
        let live_ids: HashSet<String> = {
            let mut registry = lock_recover(&self.inner);
            registry
                .tombstones
                .retain(|_, tombstone| tombstone.stopped_at.elapsed() <= LIFECYCLE_TOMBSTONE_TTL);
            registry
                .jobs
                .keys()
                .chain(registry.tombstones.keys())
                .cloned()
                .collect()
        };
        lock_recover(&self.job_locks)
            .retain(|id, lock| live_ids.contains(id) || Arc::strong_count(lock) > 1);
    }

    fn start_job_managed<F>(
        &self,
        request_id: String,
        owner_id: String,
        lease_secs: u64,
        fingerprint: String,
        runner: F,
    ) -> Result<String, String>
    where
        F: FnOnce(Arc<AtomicBool>, Arc<Mutex<Vec<IperfFlowEvent>>>) -> IperfClientOut
            + Send
            + 'static,
    {
        self.prune_client_tombstones();
        let requested_deadline = lease_deadline(lease_secs)?;
        let id = if request_id.is_empty() {
            self.next_job_id()
        } else {
            request_id
        };
        let job_lock = self.job_lock(&id);
        let _job_guard = lock_recover(&job_lock);

        {
            let registry = lock_recover(&self.inner);
            if registry.tombstones.contains_key(&id) {
                return Err(format!(
                    "iperf client request_id {id} 已停止，拒绝迟到 start"
                ));
            }
            if let Some(entry) = registry.jobs.get(&id) {
                if entry.fingerprint == fingerprint {
                    *lock_recover(&entry.expires_at) = requested_deadline;
                    entry.dynamic_lease.store(lease_secs > 0, Ordering::SeqCst);
                    return Ok(id);
                }
                return Err(format!(
                    "iperf client request_id {id} 的重复 start 参数不一致"
                ));
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let completion = Arc::new(ClientCompletion {
            result: Mutex::new(None),
            cv: Condvar::new(),
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let entry = Arc::new(ClientJobEntry {
            events: Arc::clone(&events),
            completion: Arc::clone(&completion),
            cancel: Arc::clone(&cancel),
            started: Instant::now(),
            expires_at: Mutex::new(requested_deadline),
            dynamic_lease: AtomicBool::new(lease_secs > 0),
            owner_id,
            fingerprint,
            thread: Mutex::new(ClientThreadState::default()),
            thread_cv: Condvar::new(),
        });
        {
            let mut registry = lock_recover(&self.inner);
            // 与 stop-before-start 串行：若未知 ID 已被 stop 建 tombstone，就不能复活。
            if registry.tombstones.contains_key(&id) {
                return Err(format!(
                    "iperf client request_id {id} 已停止，拒绝迟到 start"
                ));
            }
            if let Some(existing) = registry.jobs.get(&id) {
                if existing.fingerprint == entry.fingerprint {
                    *lock_recover(&existing.expires_at) = requested_deadline;
                    existing
                        .dynamic_lease
                        .store(lease_secs > 0, Ordering::SeqCst);
                    return Ok(id);
                }
                return Err(format!(
                    "iperf client request_id {id} 的重复 start 参数不一致"
                ));
            }
            registry.jobs.insert(id.clone(), Arc::clone(&entry));
        }

        let id_for_error = id.clone();
        let handle = match std::thread::Builder::new()
            .name(format!("iperf-client-{id}"))
            .spawn(move || {
                let out = catch_unwind(AssertUnwindSafe(|| runner(cancel, events))).unwrap_or_else(
                    |panic_value| IperfClientOut {
                        ok: false,
                        cancelled: false,
                        output: format!(
                            "iperf client worker panic: {}",
                            panic_message(panic_value.as_ref())
                        ),
                        ..Default::default()
                    },
                );
                *lock_recover(&completion.result) = Some(out);
                completion.cv.notify_all();
            }) {
            Ok(handle) => handle,
            Err(e) => {
                *lock_recover(&entry.completion.result) = Some(IperfClientOut {
                    ok: false,
                    output: format!("创建 iperf client worker 失败: {e}"),
                    ..Default::default()
                });
                entry.completion.cv.notify_all();
                {
                    let mut thread = lock_recover(&entry.thread);
                    thread.installed = true;
                    thread.joined = true;
                    entry.thread_cv.notify_all();
                }
                let mut registry = lock_recover(&self.inner);
                if registry
                    .jobs
                    .get(&id_for_error)
                    .map(|current| Arc::ptr_eq(current, &entry))
                    .unwrap_or(false)
                {
                    registry.jobs.remove(&id_for_error);
                }
                return Err(format!("创建 iperf client worker 失败: {e}"));
            }
        };
        {
            let mut thread = lock_recover(&entry.thread);
            thread.handle = Some(handle);
            thread.installed = true;
            entry.thread_cv.notify_all();
        }
        Ok(id)
    }

    pub fn status(&self, id: &str, cursor: usize) -> Result<IperfClientStatusOut, String> {
        let entry = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| format!("iperf client job 不存在: {id}"))?;
        let events_guard = lock_recover(&entry.events);
        let from = cursor.min(events_guard.len());
        let events = events_guard[from..].to_vec();
        let next_cursor = events_guard.len();
        drop(events_guard);
        let result = lock_recover(&entry.completion.result).clone();
        Ok(IperfClientStatusOut {
            id: id.to_string(),
            done: result.is_some(),
            next_cursor,
            events,
            result,
        })
    }

    pub fn elapsed_ms(&self, id: &str) -> Option<u64> {
        let registry = lock_recover(&self.inner);
        registry
            .jobs
            .get(id)
            .map(|entry| entry.started.elapsed().as_millis().min(u64::MAX as u128) as u64)
    }

    #[cfg(test)]
    pub fn stop(&self, id: &str) -> Result<(bool, bool), String> {
        let out = self.stop_checked(id, DEFAULT_CLIENT_STOP_WAIT)?;
        Ok((out.existed, out.was_done))
    }

    /// cancel 后等待 worker 返回并 join；成功即表示底层 client 子进程已经回收。
    pub fn stop_checked(&self, id: &str, wait: Duration) -> Result<IperfClientStopOut, String> {
        validate_lifecycle_id("client id", id)?;
        if id.is_empty() {
            return Err("client id 不能为空".into());
        }
        self.prune_client_tombstones();
        let job_lock = self.job_lock(id);
        let _job_guard = lock_recover(&job_lock);
        let entry = {
            let mut registry = lock_recover(&self.inner);
            if let Some(tombstone) = registry.tombstones.get(id) {
                return Ok(tombstone.out.clone());
            }
            let Some(entry) = registry.jobs.get(id).cloned() else {
                let out = IperfClientStopOut {
                    existed: false,
                    was_done: false,
                    terminated: true,
                    result: None,
                };
                registry.tombstones.insert(
                    id.to_string(),
                    ClientTombstone {
                        stopped_at: Instant::now(),
                        out: out.clone(),
                    },
                );
                return Ok(out);
            };
            entry
        };

        let was_done = lock_recover(&entry.completion.result).is_some();
        entry.cancel.store(true, Ordering::SeqCst);
        let deadline = Instant::now()
            .checked_add(wait)
            .ok_or_else(|| format!("client stop 等待时间 {} 秒过大", wait.as_secs()))?;
        wait_for_client_result(&entry, Some(deadline), id)?;
        join_client_thread(&entry, Some(deadline), id)?;
        let result = lock_recover(&entry.completion.result).clone();

        let out = IperfClientStopOut {
            existed: true,
            was_done,
            terminated: true,
            result,
        };
        let mut registry = lock_recover(&self.inner);
        if registry
            .jobs
            .get(id)
            .map(|current| Arc::ptr_eq(current, &entry))
            .unwrap_or(false)
        {
            registry.jobs.remove(id);
            registry.tombstones.insert(
                id.to_string(),
                ClientTombstone {
                    stopped_at: Instant::now(),
                    out: out.clone(),
                },
            );
        }
        Ok(registry
            .tombstones
            .get(id)
            .map(|tombstone| tombstone.out.clone())
            .unwrap_or(out))
    }

    pub fn stop_owner(&self, owner_id: &str, wait: Duration) -> LifecycleCleanupResult {
        let mut result = LifecycleCleanupResult::default();
        if owner_id.is_empty() {
            result.errors.push("owner_id 不能为空".into());
            return result;
        }
        if let Err(e) = validate_lifecycle_id("owner_id", owner_id) {
            result.errors.push(e);
            return result;
        }
        let targets: Vec<(String, Arc<ClientJobEntry>)> = {
            let registry = lock_recover(&self.inner);
            registry
                .jobs
                .iter()
                .filter(|(_, entry)| entry.owner_id == owner_id)
                .map(|(id, entry)| (id.clone(), Arc::clone(entry)))
                .collect()
        };
        // 先同时发出取消，再逐项等待；大量并发流异常清理时不会串行多等
        // 一个轮询周期，后续 stop_checked 只负责确认和 join。
        for (_, entry) in &targets {
            entry.cancel.store(true, Ordering::SeqCst);
        }
        let deadline = Instant::now().checked_add(wait);
        for (id, _) in targets {
            let remaining = remaining_until(deadline).unwrap_or(Duration::ZERO);
            match self.stop_checked(&id, remaining) {
                Ok(out) if out.existed && out.terminated => result.stopped += 1,
                Ok(_) => {}
                Err(e) => result.errors.push(format!("client job {id} 清理失败: {e}")),
            }
        }
        result
    }

    /// 主控退出前的最后兜底：同时取消仍登记的全部异步 client/外部作业，
    /// 再在同一总截止时间内逐项确认 worker 与子进程均已回收。
    pub fn stop_all(&self, wait: Duration) -> LifecycleCleanupResult {
        let mut result = LifecycleCleanupResult::default();
        let targets: Vec<(String, Arc<ClientJobEntry>)> = {
            let registry = lock_recover(&self.inner);
            registry
                .jobs
                .iter()
                .map(|(id, entry)| (id.clone(), Arc::clone(entry)))
                .collect()
        };
        for (_, entry) in &targets {
            entry.cancel.store(true, Ordering::SeqCst);
        }
        let deadline = Instant::now().checked_add(wait);
        for (id, _) in targets {
            let remaining = remaining_until(deadline).unwrap_or(Duration::ZERO);
            match self.stop_checked(&id, remaining) {
                Ok(out) if out.existed && out.terminated => result.stopped += 1,
                Ok(_) => {}
                Err(error) => result
                    .errors
                    .push(format!("client job {id} 最终清理失败: {error}")),
            }
        }
        result
    }

    pub fn sweep(&self, max_age: Duration) -> Vec<String> {
        self.prune_client_tombstones();
        let expired: Vec<String> = {
            let registry = lock_recover(&self.inner);
            registry
                .jobs
                .iter()
                .filter(|(_, entry)| {
                    if entry.dynamic_lease.load(Ordering::SeqCst) {
                        lock_recover(&entry.expires_at)
                            .map(|deadline| Instant::now() >= deadline)
                            .unwrap_or(false)
                    } else {
                        entry.started.elapsed() > max_age
                    }
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut errors = Vec::new();
        for id in expired {
            if let Err(e) = self.stop_checked(&id, DEFAULT_CLIENT_STOP_WAIT) {
                let message = format!("清理超龄 iperf client job {id} 失败: {e}");
                eprintln!("[iperf] {message}");
                errors.push(message);
            }
        }
        errors
    }
}

fn panic_message(value: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = value.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = value.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".into()
    }
}

fn remaining_until(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

fn wait_for_client_result(
    entry: &ClientJobEntry,
    deadline: Option<Instant>,
    id: &str,
) -> Result<(), String> {
    let mut result = lock_recover(&entry.completion.result);
    while result.is_none() {
        let remaining = remaining_until(deadline).unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return Err(format!("等待 iperf client job {id} 退出超时"));
        }
        let waited = entry.completion.cv.wait_timeout(result, remaining);
        let (next, timeout) = match waited {
            Ok(pair) => pair,
            Err(poisoned) => poisoned.into_inner(),
        };
        result = next;
        if timeout.timed_out() && result.is_none() {
            return Err(format!("等待 iperf client job {id} 退出超时"));
        }
    }
    Ok(())
}

fn join_client_thread(
    entry: &ClientJobEntry,
    deadline: Option<Instant>,
    id: &str,
) -> Result<(), String> {
    let handle = loop {
        let mut thread = lock_recover(&entry.thread);
        if thread.joined {
            return Ok(());
        }
        if thread.installed && !thread.joining {
            thread.joining = true;
            break thread.handle.take();
        }
        let remaining = remaining_until(deadline).unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return Err(format!("等待 iperf client job {id} worker 回收超时"));
        }
        let waited = entry.thread_cv.wait_timeout(thread, remaining);
        let (next, timeout) = match waited {
            Ok(pair) => pair,
            Err(poisoned) => poisoned.into_inner(),
        };
        if timeout.timed_out() && !next.joined && (!next.installed || next.joining) {
            return Err(format!("等待 iperf client job {id} worker 回收超时"));
        }
    };

    let join_error = handle.and_then(|handle| handle.join().err());
    let mut thread = lock_recover(&entry.thread);
    thread.joining = false;
    thread.joined = true;
    entry.thread_cv.notify_all();
    if let Some(panic_value) = join_error {
        Err(format!(
            "iperf client job {id} worker join 发现 panic: {}",
            panic_message(panic_value.as_ref())
        ))
    } else {
        Ok(())
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
    fn retry_history_keeps_every_client_attempt() {
        let mut history = Vec::new();
        append_attempt_output(&mut history, 1, "first connection refused");
        append_attempt_output(&mut history, 2, "second measurement succeeded");
        let output = history.join("\n");
        assert!(output.contains("=== client attempt 1 ==="));
        assert!(output.contains("first connection refused"));
        assert!(output.contains("=== client attempt 2 ==="));
        assert!(output.contains("second measurement succeeded"));
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
            ..Default::default()
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

    /// 子进程模式下只负责占住一个真实 TCP 监听端口，供父测试验证 kill+wait
    /// 返回后端口确实已经可以重新绑定。普通测试进程中该测试立即返回。
    #[test]
    fn helper_tcp_listener_process() {
        if std::env::var("CPE_TEST_LISTENER_HELPER").as_deref() != Ok("1") {
            return;
        }
        let port: u16 = std::env::var("CPE_TEST_LISTENER_PORT")
            .expect("helper port")
            .parse()
            .expect("numeric helper port");
        let _listener = std::net::TcpListener::bind(("127.0.0.1", port))
            .expect("helper must bind requested port");
        std::thread::sleep(Duration::from_secs(60));
    }

    fn spawn_test_listener() -> (u16, Child) {
        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cmd::iperf::tests::helper_tcp_listener_process",
                "--nocapture",
            ])
            .env("CPE_TEST_LISTENER_HELPER", "1")
            .env("CPE_TEST_LISTENER_PORT", port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "helper 未及时监听端口 {port}");
            std::thread::sleep(Duration::from_millis(10));
        }
        (port, child)
    }

    fn register_test_server(mgr: &IperfServerMgr, req: &IperfServerStartReq, child: Child) {
        lock_recover(&mgr.inner).insert(
            req.port,
            SrvEntry {
                child,
                output: Arc::new(Mutex::new(Vec::new())),
                readers: Vec::new(),
                started: Instant::now(),
                expires_at: lease_deadline(req.lease_secs).unwrap(),
                dynamic_lease: req.lease_secs > 0,
                cmd: "test-listener".into(),
                request_id: req.request_id.clone(),
                owner_id: req.owner_id.clone(),
                fingerprint: IperfServerMgr::server_fingerprint(req),
                ready: true,
            },
        );
    }

    #[test]
    fn server_stop_confirms_process_exit_and_releases_port() {
        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);

        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cmd::iperf::tests::helper_tcp_listener_process",
                "--nocapture",
            ])
            .env("CPE_TEST_LISTENER_HELPER", "1")
            .env("CPE_TEST_LISTENER_PORT", port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "helper 未及时监听端口 {port}");
            std::thread::sleep(Duration::from_millis(10));
        }

        let mgr = IperfServerMgr::new();
        lock_recover(&mgr.inner).insert(
            port,
            SrvEntry {
                child,
                output: Arc::new(Mutex::new(Vec::new())),
                readers: Vec::new(),
                started: Instant::now(),
                expires_at: lease_deadline(60).unwrap(),
                dynamic_lease: true,
                cmd: "test-listener".into(),
                request_id: "server-stop-test".into(),
                owner_id: "owner-stop-test".into(),
                fingerprint: "test-listener-fingerprint".into(),
                ready: true,
            },
        );

        let stopped = mgr
            .stop_checked(port, "server-stop-test", Duration::ZERO)
            .unwrap();
        assert!(stopped.existed);
        assert!(stopped.terminated);
        let rebound = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(
            rebound.is_ok(),
            "stop 成功返回后端口 {port} 仍不可重新绑定: {:?}",
            rebound.err()
        );

        // 丢失第一次 stop 响应后重放同一 request，仍应幂等成功。
        let replay = mgr
            .stop_checked(port, "server-stop-test", Duration::ZERO)
            .unwrap();
        assert_eq!(replay.existed, stopped.existed);
        assert!(replay.terminated);
    }

    #[test]
    fn real_iperf_server_start_replay_stop_and_rebind_when_available() {
        let Some(bin) = crate::util::find_iperf3() else {
            return;
        };
        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let mgr = IperfServerMgr::new();
        let req = IperfServerStartReq {
            bind_ip: "127.0.0.1".into(),
            port,
            v6: false,
            request_id: "real-iperf-server".into(),
            owner_id: "owner-real-iperf-server".into(),
            lease_secs: 60,
        };

        let first = mgr.start(&bin, &req).unwrap();
        let replay = mgr.start(&bin, &req).unwrap();
        assert_eq!(first, replay);
        let stopped = mgr
            .stop_checked(port, &req.request_id, Duration::ZERO)
            .unwrap();
        assert!(stopped.existed && stopped.terminated);
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "真实 iperf3 stop 返回后端口必须立即可重绑"
        );
    }

    #[test]
    fn real_iperf_client_cancel_waits_for_process_reap_when_available() {
        let Some(bin) = crate::util::find_iperf3() else {
            return;
        };
        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let servers = IperfServerMgr::new();
        let server_req = IperfServerStartReq {
            bind_ip: "127.0.0.1".into(),
            port,
            v6: false,
            request_id: "real-client-server".into(),
            owner_id: "owner-real-client".into(),
            lease_secs: 60,
        };
        servers.start(&bin, &server_req).unwrap();

        let clients = IperfClientJobMgr::new();
        let id = clients
            .start_request(
                bin,
                IperfClientStartReq {
                    request: IperfClientReq {
                        dst: "127.0.0.1".into(),
                        bind_ip: "127.0.0.1".into(),
                        port,
                        duration: 30,
                        udp: false,
                        v6: false,
                        extra: Vec::new(),
                    },
                    request_id: "real-iperf-client".into(),
                    owner_id: "owner-real-client".into(),
                    lease_secs: 60,
                },
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let stopped = clients.stop_checked(&id, Duration::from_secs(5)).unwrap();
        assert!(stopped.existed && stopped.terminated);
        assert!(clients.status(&id, 0).is_err());

        servers
            .stop_checked(port, &server_req.request_id, Duration::ZERO)
            .unwrap();
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    #[test]
    fn stale_server_stop_never_kills_new_request_and_request_id_is_global() {
        let (port, child) = spawn_test_listener();
        let mgr = IperfServerMgr::new();
        let req = IperfServerStartReq {
            bind_ip: "127.0.0.1".into(),
            port,
            v6: false,
            request_id: "new-server-request".into(),
            owner_id: "owner-new-server".into(),
            lease_secs: 60,
        };
        register_test_server(&mgr, &req, child);

        let stale = mgr
            .stop_checked(port, "old-server-request", Duration::ZERO)
            .unwrap();
        assert!(!stale.existed);
        assert!(stale.terminated);
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_ok(),
            "迟到的旧 request stop 不得关闭当前 listener"
        );

        // 相同 request + 相同参数的 start 只续租并复用，不再 spawn。
        assert_eq!(mgr.start("unused-binary", &req).unwrap(), "test-listener");

        let mut conflicting = req.clone();
        conflicting.port = if port == u16::MAX { port - 1 } else { port + 1 };
        assert!(
            mgr.start("unused-binary", &conflicting).is_err(),
            "同一 request_id 不能同时代表另一个端口"
        );

        let stopped = mgr
            .stop_checked(port, &req.request_id, Duration::ZERO)
            .unwrap();
        assert!(stopped.existed && stopped.terminated);
    }

    #[test]
    fn replay_never_reuses_a_live_but_unready_server_entry() {
        let (port, child) = spawn_test_listener();
        let mgr = IperfServerMgr::new();
        let req = IperfServerStartReq {
            bind_ip: "127.0.0.1".into(),
            port,
            v6: false,
            request_id: "unready-server-request".into(),
            owner_id: "owner-unready-server".into(),
            lease_secs: 60,
        };
        register_test_server(&mgr, &req, child);
        lock_recover(&mgr.inner).get_mut(&port).unwrap().ready = false;

        let replay = mgr.start("binary-that-must-not-exist", &req);
        assert!(replay.is_err(), "未就绪 Child 不能被重放 start 当成成功");
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_err(),
            "重放 start 前必须先回收未就绪 Child"
        );
        assert!(!lock_recover(&mgr.inner).contains_key(&port));
    }

    #[test]
    fn server_owner_cleanup_and_dynamic_lease_are_isolated_and_idempotent() {
        let mgr = IperfServerMgr::new();
        let (port_a, child_a) = spawn_test_listener();
        let req_a = IperfServerStartReq {
            bind_ip: "127.0.0.1".into(),
            port: port_a,
            v6: false,
            request_id: "server-owner-a".into(),
            owner_id: "owner-a".into(),
            lease_secs: 60,
        };
        register_test_server(&mgr, &req_a, child_a);
        let (port_b, child_b) = spawn_test_listener();
        let req_b = IperfServerStartReq {
            bind_ip: "127.0.0.1".into(),
            port: port_b,
            v6: false,
            request_id: "server-owner-b".into(),
            owner_id: "owner-b".into(),
            lease_secs: 60,
        };
        register_test_server(&mgr, &req_b, child_b);

        let cleanup_a = mgr.stop_owner("owner-a", Duration::ZERO);
        assert_eq!(cleanup_a.stopped, 1);
        assert!(cleanup_a.errors.is_empty());
        assert!(TcpStream::connect(("127.0.0.1", port_a)).is_err());
        assert!(TcpStream::connect(("127.0.0.1", port_b)).is_ok());
        let replay_a = mgr.stop_owner("owner-a", Duration::ZERO);
        assert_eq!(replay_a.stopped, 0);
        assert!(replay_a.errors.is_empty());

        lock_recover(&mgr.inner)
            .get_mut(&port_b)
            .unwrap()
            .expires_at = Some(Instant::now());
        assert!(mgr.sweep(Duration::MAX).is_empty());
        assert!(!lock_recover(&mgr.inner).contains_key(&port_b));
        assert!(TcpStream::connect(("127.0.0.1", port_b)).is_err());
    }

    #[test]
    fn client_stop_waits_for_worker_and_is_idempotent() {
        let mgr = IperfClientJobMgr::new();
        let active = Arc::new(AtomicUsize::new(0));
        let active_runner = Arc::clone(&active);
        let id = mgr
            .start_job_managed(
                "client-stop-test".into(),
                "owner-client-test".into(),
                60,
                "fingerprint".into(),
                move |cancel, _events| {
                    active_runner.store(1, Ordering::SeqCst);
                    while !cancel.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    // 模拟子进程 kill/wait 与 reader join 的收尾延迟。
                    std::thread::sleep(Duration::from_millis(40));
                    active_runner.store(0, Ordering::SeqCst);
                    IperfClientOut {
                        cancelled: true,
                        output: "cancelled and reaped".into(),
                        ..Default::default()
                    }
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while active.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }

        let stopped = mgr.stop_checked(&id, Duration::from_secs(2)).unwrap();
        assert!(stopped.existed);
        assert!(stopped.terminated);
        let result = stopped.result.as_ref().expect("stop 应回传最终输出");
        assert!(result.cancelled);
        assert_eq!(result.output, "cancelled and reaped");
        assert_eq!(active.load(Ordering::SeqCst), 0);

        let replay = mgr.stop_checked(&id, Duration::from_secs(2)).unwrap();
        assert_eq!(replay.existed, stopped.existed);
        assert!(replay.terminated);
        assert_eq!(
            replay.result.as_ref().map(|result| result.output.as_str()),
            Some("cancelled and reaped")
        );
    }

    #[test]
    fn client_stop_all_reaps_every_registered_external_job() {
        let mgr = IperfClientJobMgr::new();
        let active = Arc::new(AtomicUsize::new(0));
        let mut ids = Vec::new();
        for index in 0..2 {
            let active_runner = Arc::clone(&active);
            ids.push(
                mgr.start_job_managed(
                    format!("client-stop-all-{index}"),
                    format!("owner-stop-all-{index}"),
                    60,
                    format!("fingerprint-{index}"),
                    move |cancel, _events| {
                        active_runner.fetch_add(1, Ordering::SeqCst);
                        while !cancel.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        active_runner.fetch_sub(1, Ordering::SeqCst);
                        IperfClientOut {
                            cancelled: true,
                            output: format!("stopped-{index}"),
                            ..Default::default()
                        }
                    },
                )
                .unwrap(),
            );
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while active.load(Ordering::SeqCst) < 2 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }

        let stopped = mgr.stop_all(Duration::from_secs(2));
        assert_eq!(stopped.stopped, 2);
        assert!(stopped.errors.is_empty());
        assert_eq!(active.load(Ordering::SeqCst), 0);
        for id in ids {
            assert!(mgr.status(&id, 0).is_err());
        }
    }

    #[test]
    fn client_request_id_is_idempotent_and_stop_before_start_blocks_revival() {
        let mgr = IperfClientJobMgr::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));

        let runs_first = Arc::clone(&runs);
        let gate_first = Arc::clone(&gate);
        let first = mgr
            .start_job_managed(
                "same-client-request".into(),
                "owner-idempotent".into(),
                60,
                "same-fingerprint".into(),
                move |_cancel, _events| {
                    runs_first.fetch_add(1, Ordering::SeqCst);
                    let (lock, cv) = &*gate_first;
                    let mut released = lock_recover(lock);
                    while !*released {
                        released = cv
                            .wait(released)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    IperfClientOut::default()
                },
            )
            .unwrap();
        let second = mgr
            .start_job_managed(
                "same-client-request".into(),
                "owner-idempotent".into(),
                60,
                "same-fingerprint".into(),
                move |_cancel, _events| {
                    panic!("幂等 start 不应启动第二个 runner");
                },
            )
            .unwrap();
        assert_eq!(first, second);

        let deadline = Instant::now() + Duration::from_secs(2);
        while runs.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        {
            let (lock, cv) = &*gate;
            *lock_recover(lock) = true;
            cv.notify_all();
        }
        mgr.stop_checked(&first, Duration::from_secs(2)).unwrap();

        let stopped_unknown = mgr
            .stop_checked("late-client-request", Duration::from_secs(1))
            .unwrap();
        assert!(!stopped_unknown.existed);
        assert!(stopped_unknown.terminated);
        let late_start = mgr.start_job_managed(
            "late-client-request".into(),
            "owner-idempotent".into(),
            60,
            "late-fingerprint".into(),
            move |_cancel, _events| IperfClientOut::default(),
        );
        assert!(
            late_start.is_err(),
            "stop-before-start 后不允许迟到请求复活"
        );
    }

    #[test]
    fn idempotent_client_start_renews_its_dynamic_lease() {
        let mgr = IperfClientJobMgr::new();
        let id = mgr
            .start_job_managed(
                "client-lease-renew".into(),
                "owner-lease-renew".into(),
                1,
                "lease-fingerprint".into(),
                move |cancel, _events| {
                    while !cancel.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    IperfClientOut::default()
                },
            )
            .unwrap();
        let before = {
            let registry = lock_recover(&mgr.inner);
            let value = *lock_recover(&registry.jobs[&id].expires_at);
            value
        };
        std::thread::sleep(Duration::from_millis(2));
        let replay = mgr
            .start_job_managed(
                id.clone(),
                "owner-lease-renew".into(),
                60,
                "lease-fingerprint".into(),
                move |_cancel, _events| panic!("幂等续租不应启动第二个 worker"),
            )
            .unwrap();
        let after = {
            let registry = lock_recover(&mgr.inner);
            let value = *lock_recover(&registry.jobs[&id].expires_at);
            value
        };
        assert_eq!(replay, id);
        assert!(after > before);
        mgr.stop_checked(&id, Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn client_owner_cleanup_and_dynamic_lease_are_isolated_and_idempotent() {
        let mgr = IperfClientJobMgr::new();
        let start = |id: &str, owner: &str| {
            mgr.start_job_managed(
                id.into(),
                owner.into(),
                60,
                format!("fingerprint-{id}"),
                move |cancel, _events| {
                    while !cancel.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    IperfClientOut {
                        cancelled: true,
                        ..Default::default()
                    }
                },
            )
            .unwrap()
        };
        let id_a = start("client-owner-a", "owner-a");
        let id_b = start("client-owner-b", "owner-b");

        let cleanup_a = mgr.stop_owner("owner-a", Duration::from_secs(2));
        assert_eq!(cleanup_a.stopped, 1);
        assert!(cleanup_a.errors.is_empty());
        assert!(mgr.status(&id_a, 0).is_err());
        assert!(mgr.status(&id_b, 0).is_ok());
        let replay_a = mgr.stop_owner("owner-a", Duration::from_secs(2));
        assert_eq!(replay_a.stopped, 0);
        assert!(replay_a.errors.is_empty());

        {
            let registry = lock_recover(&mgr.inner);
            *lock_recover(&registry.jobs[&id_b].expires_at) = Some(Instant::now());
        }
        assert!(mgr.sweep(Duration::MAX).is_empty());
        assert!(mgr.status(&id_b, 0).is_err());
    }

    #[test]
    fn client_stop_timeout_keeps_entry_for_later_confirmation() {
        let mgr = IperfClientJobMgr::new();
        let release = Arc::new(AtomicBool::new(false));
        let release_runner = Arc::clone(&release);
        let id = mgr
            .start_job_managed(
                "client-stop-timeout".into(),
                "owner-timeout".into(),
                60,
                "timeout-fingerprint".into(),
                move |_cancel, _events| {
                    while !release_runner.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    IperfClientOut::default()
                },
            )
            .unwrap();

        assert!(mgr.stop_checked(&id, Duration::from_millis(20)).is_err());
        assert!(mgr.status(&id, 0).is_ok(), "未确认停止时必须保留 entry");
        release.store(true, Ordering::SeqCst);
        let stopped = mgr.stop_checked(&id, Duration::from_secs(2)).unwrap();
        assert!(stopped.terminated);
    }

    #[test]
    fn client_worker_panic_still_notifies_and_can_be_reaped() {
        let mgr = IperfClientJobMgr::new();
        let id = mgr
            .start_job_managed(
                "client-panic".into(),
                "owner-panic".into(),
                60,
                "panic-fingerprint".into(),
                move |_cancel, _events| panic!("synthetic runner panic"),
            )
            .unwrap();
        let stopped = mgr.stop_checked(&id, Duration::from_secs(2)).unwrap();
        assert!(stopped.terminated);
    }
}
