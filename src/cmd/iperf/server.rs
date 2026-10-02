//! iperf3 server 进程管理：按端口登记、TCP connect 探测就绪、按 request/owner 停止回收。

use super::args::{cmdline, server_args};
use super::{
    lease_deadline, lifecycle_lock_index, lock_recover, validate_lifecycle_id,
    LifecycleCleanupResult, LIFECYCLE_LOCK_STRIPES, LIFECYCLE_TOMBSTONE_TTL, OUTPUT_LIMIT,
};
use crate::protocol::{IperfServerStartReq, IperfServerStopOut};
use crate::util::{
    configure_managed_command, decode_bytes, spawn_managed_watchdog, BoundedOutput,
    ManagedChildWatchdog,
};
use std::collections::HashMap;
use std::io::BufReader;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

pub const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);
const SERVER_KILL_WAIT: Duration = Duration::from_secs(5);

pub(super) struct SrvEntry {
    pub(super) child: Child,
    pub(super) watchdog: Option<ManagedChildWatchdog>,
    /// 收集到的输出（reader thread 写入）
    pub(super) output: Arc<Mutex<BoundedOutput>>,
    pub(super) readers: Vec<std::thread::JoinHandle<()>>,
    pub(super) started: Instant,
    pub(super) expires_at: Option<Instant>,
    pub(super) dynamic_lease: bool,
    pub(super) cmd: String,
    pub(super) request_id: String,
    pub(super) owner_id: String,
    pub(super) fingerprint: String,
    /// 只有本次 Child 通过就绪探测后才为 true；未就绪 entry 不可被重放
    /// start 当成成功实例复用。
    pub(super) ready: bool,
}

#[derive(Clone)]
struct ServerTombstone {
    port: u16,
    stopped_at: Instant,
    out: IperfServerStopOut,
}

/// iperf3 server 注册表（agent 端与主控本地共用）
pub struct IperfServerMgr {
    pub(super) inner: Mutex<HashMap<u16, SrvEntry>>,
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

    pub(super) fn server_fingerprint(req: &IperfServerStartReq) -> String {
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
        let mut command = Command::new(bin);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_managed_command(&mut command);
        let mut child = command
            .spawn()
            .map_err(|e| format!("启动 iperf3 server 失败: {e} (命令: {cmd_str})"))?;
        let watchdog = match spawn_managed_watchdog(child.id()) {
            Ok(watchdog) => watchdog,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("启动 iperf3 server watchdog 失败: {error}"));
            }
        };

        let output_arc = Arc::new(Mutex::new(BoundedOutput::new(Some(OUTPUT_LIMIT))));
        {
            let mut g = lock_recover(&self.inner);
            g.insert(
                req.port,
                SrvEntry {
                    child,
                    watchdog,
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
                            Ok(_) => lock_recover(&stdout_output).push(&decode_bytes(&line)),
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
                            Ok(_) => lock_recover(&stderr_output)
                                .push(&format!("[stderr] {}", decode_bytes(&line))),
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

        // 显式 scope 的 IPv6 也要经 TCP connect 确认；Windows 既有不带 zone 的
        // link-local 绑定仍保留短暂等待 + 进程存活确认，client 侧有连接重试兜底。
        let clean_bind = req
            .bind_ip
            .split('%')
            .next()
            .unwrap_or(&req.bind_ip)
            .to_lowercase();
        let ready = if clean_bind.starts_with("fe80:") && !req.bind_ip.contains('%') {
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

    /// 返回指定 owner 当前登记的 server request ID，供统一资源清单快照使用。
    pub fn resource_ids_for_owner(&self, owner_id: &str) -> Vec<String> {
        let entries = lock_recover(&self.inner);
        let mut ids: Vec<String> = entries
            .iter()
            .filter(|(_, entry)| entry.owner_id == owner_id)
            .map(|(port, entry)| {
                if entry.request_id.is_empty() {
                    format!("server-port-{port}")
                } else {
                    entry.request_id.clone()
                }
            })
            .collect();
        ids.sort();
        ids
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

    pub fn stop_all(&self) -> LifecycleCleanupResult {
        let mut result = LifecycleCleanupResult::default();
        let targets: Vec<(u16, String)> = {
            let g = lock_recover(&self.inner);
            g.iter()
                .map(|(port, entry)| (*port, entry.request_id.clone()))
                .collect()
        };
        for (port, request_id) in targets {
            match self.stop_checked(port, &request_id, Duration::ZERO) {
                Ok(out) if out.existed && out.terminated => result.stopped += 1,
                Ok(_) => {}
                Err(e) => result
                    .errors
                    .push(format!("server 端口 {port} 清理失败: {e}")),
            }
        }
        result
    }
}

fn terminate_server_process(entry: &mut SrvEntry, wait: Duration) -> Result<(), String> {
    let process_result = terminate_server_process_inner(entry, wait);
    let watchdog_result = entry.watchdog.take().map(|mut watchdog| watchdog.stop());
    match (process_result, watchdog_result) {
        (Ok(()), None) | (Ok(()), Some(Ok(()))) => Ok(()),
        (Err(error), None) | (Err(error), Some(Ok(()))) => Err(error),
        (Ok(()), Some(Err(error))) => Err(error),
        (Err(process_error), Some(Err(watchdog_error))) => Err(format!(
            "{process_error}；watchdog 清理失败: {watchdog_error}"
        )),
    }
}

fn terminate_server_process_inner(entry: &mut SrvEntry, wait: Duration) -> Result<(), String> {
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
    let output = lock_recover(&entry.output).render();
    format!("$ {}\n{}", entry.cmd, output)
}

fn server_probe_addresses(bind_ip: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    if let Some((address, zone)) = bind_ip.split_once('%') {
        let ip = address
            .parse::<Ipv6Addr>()
            .map_err(|e| format!("解析 IPv6 地址失败 {bind_ip}: {e}"))?;
        let scope_id = if !zone.is_empty() && zone.bytes().all(|b| b.is_ascii_digit()) {
            zone.parse::<u32>()
                .map_err(|e| format!("IPv6 scope 无效 {bind_ip}: {e}"))?
        } else {
            #[cfg(unix)]
            {
                let name = std::ffi::CString::new(zone)
                    .map_err(|_| format!("IPv6 scope 接口名无效 {bind_ip}"))?;
                // CString 提供以 NUL 结尾的有效接口名，返回 0 表示接口不存在。
                unsafe { libc::if_nametoindex(name.as_ptr()) }
            }
            #[cfg(not(unix))]
            {
                return Err(format!("IPv6 scope 必须使用有效的数字接口索引：{bind_ip}"));
            }
        };
        if scope_id == 0 {
            return Err(format!("IPv6 scope 必须对应有效接口：{bind_ip}"));
        }
        return Ok(vec![SocketAddr::V6(SocketAddrV6::new(
            ip, port, 0, scope_id,
        ))]);
    }
    let addresses: Vec<_> = (bind_ip, port)
        .to_socket_addrs()
        .map_err(|e| format!("解析地址失败 {bind_ip}:{port}: {e}"))?
        .collect();
    if addresses.is_empty() {
        return Err(format!("无法解析地址 {bind_ip}:{port}"));
    }
    Ok(addresses)
}

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
    while Instant::now() < deadline {
        confirm_child_running()?;
        let addrs = server_probe_addresses(&bind_ip, port)?;
        // 取第一个可用的地址
        if let Some(sa) = addrs.last() {
            match TcpStream::connect_timeout(sa, StdDuration::from_secs(1)) {
                Ok(_) => return Ok(()),
                Err(_e) => {
                    // ConnectionRefused 正常（server 还没好）
                    std::thread::sleep(StdDuration::from_millis(200));
                }
            }
        }
    }
    Err(format!(
        "iperf3 server 端口 {port} 在 {:.1} 秒内未响应 TCP connect",
        timeout.as_secs_f64()
    ))
}
