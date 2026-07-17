//! agent REST server（tiny_http，固定线程池，无 async）
//!
//! 端点（全部 POST JSON，另有 GET /health）：
//!   /info /ping /iperf/server/start /iperf/server/stop
//!   /iperf/client/run（兼容） /iperf/client/start /status /stop
//!   /ctstraffic/start /status /stop
//!   /monitor/start /monitor/status /monitor/stop /resources/cleanup /screenshot /health
//! 响应统一 {"ok":bool,"error":...,"data":{...}}，HTTP 状态恒 200。

use crate::cmd::ctstraffic;
use crate::cmd::iperf::{IperfClientJobMgr, IperfServerMgr};
use crate::config::Config;
use crate::nic::monitor::MonitorMgr;
use crate::nic::scan_host;
use crate::protocol::*;
use crate::util::{
    ctstraffic_version, find_ctstraffic, find_iperf3, iperf3_version, now_hms, os_name,
};
use crate::{ping, screenshot};
use base64::Engine;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server};

const WORKERS: usize = 16;
const MAX_BODY: u64 = 100 * 1024 * 1024;
/// 每 30 秒清理一次过期状态（见 PROJECT_PLAN）
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
const SERVER_MAX_AGE: Duration = Duration::from_secs(90_000);
const CLIENT_JOB_MAX_AGE: Duration = Duration::from_secs(90_000);
const MONITOR_MAX_AGE: Duration = Duration::from_secs(90_000);
const OWNER_TOMBSTONE_TTL: Duration = Duration::from_secs(10 * 60);
const OWNER_LOCK_STRIPES: usize = 64;

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read_recover<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_recover<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn owner_id_ok(owner_id: &str) -> bool {
    owner_id.len() <= 160
        && owner_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

struct OwnerLifecycle {
    closed: Mutex<HashMap<String, Instant>>,
    locks: [RwLock<()>; OWNER_LOCK_STRIPES],
}

impl OwnerLifecycle {
    fn new() -> Self {
        Self {
            closed: Mutex::new(HashMap::new()),
            locks: std::array::from_fn(|_| RwLock::new(())),
        }
    }

    fn lock_index(owner_id: &str) -> usize {
        let mut hasher = DefaultHasher::new();
        owner_id.hash(&mut hasher);
        hasher.finish() as usize % OWNER_LOCK_STRIPES
    }

    fn prune_closed(&self) {
        lock_recover(&self.closed)
            .retain(|_, closed_at| closed_at.elapsed() <= OWNER_TOMBSTONE_TTL);
    }

    fn with_start<T, F>(&self, owner_id: &str, start: F) -> Result<T, String>
    where
        F: FnOnce() -> Result<T, String>,
    {
        if owner_id.is_empty() {
            return start();
        }
        if !owner_id_ok(owner_id) {
            return Err("owner_id 非法：只允许 160 字节以内的字母、数字、-_.:".into());
        }
        // 同 owner 的多个 start 可并行；cleanup 使用写锁等待它们全部落地。
        let _guard = read_recover(&self.locks[Self::lock_index(owner_id)]);
        self.prune_closed();
        if lock_recover(&self.closed).contains_key(owner_id) {
            return Err(format!(
                "owner_id {owner_id} 已完成资源清理，拒绝迟到的资源 start"
            ));
        }
        start()
    }

    fn with_cleanup<T, F>(&self, owner_id: &str, cleanup: F) -> T
    where
        F: FnOnce() -> T,
    {
        let _guard = write_recover(&self.locks[Self::lock_index(owner_id)]);
        self.prune_closed();
        // 先封口再做快照清理：同 owner 的并发 start 要么先完成并被本次
        // cleanup 看见，要么排在本次之后并因 tombstone 被拒绝。
        lock_recover(&self.closed).insert(owner_id.to_string(), Instant::now());
        cleanup()
    }
}

pub struct AgentState {
    pub servers: IperfServerMgr,
    pub clients: IperfClientJobMgr,
    pub monitors: MonitorMgr,
    pub default_prefixes: Vec<String>,
    owner_lifecycle: OwnerLifecycle,
}

/// 启动 agent（阻塞不返回）
pub fn run(port: u16, cfg: &Config) {
    println!("==============================================");
    println!(
        "  CPE 子网测试工具 v{} — 辅测 agent",
        env!("CARGO_PKG_VERSION")
    );
    println!("==============================================");

    match find_iperf3() {
        Some(bin) => println!("iperf3: {} ({})", bin, iperf3_version().unwrap_or_default()),
        None => println!(
            "!! 警告: 未找到 iperf3。ping 可用，但灌包测试会失败。\n!!       请把 iperf3 可执行文件放到本程序同目录。"
        ),
    }
    match ctstraffic_version() {
        Some(version) => println!("ctsTraffic: {version}"),
        None if cfg!(windows) => println!(
            "!! 提示: 未找到 ctsTraffic.exe；iperf3/ping 仍可用，CTS 测试会被前置检查拦截。"
        ),
        None => println!("ctsTraffic: 当前平台不支持（仅 Windows 10+）"),
    }

    // 展示本机所有网卡详情，方便小白抄给主控
    let all = scan_host(&[]);
    println!("\n本机网卡详情:");
    for n in &all.interfaces {
        let mut info = n.ipv4.clone();
        if !n.ipv6_ll.is_empty() {
            info.push_str(&format!(" / {}", n.ipv6_ll));
        }
        if !n.ipv6_global.is_empty() {
            info.push_str(&format!(" / {}", n.ipv6_global));
        }
        if !n.gateway_v4.is_empty() {
            info.push_str(&format!("  gw:{}", n.gateway_v4));
        }
        if n.speed_mbps > 0 {
            info.push_str(&format!("  {}Mbps", n.speed_mbps));
        }
        if !n.wifi_band.is_empty() {
            info.push_str(&format!("  {}", n.wifi_band));
        }
        println!("    {} = {}  [{}]", n.name, info, n.role);
    }

    let server = match Server::http(("0.0.0.0", port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("\n!! 启动失败: 端口 {port} 无法监听 ({e})");
            eprintln!("!! 可能已有一个 agent 在运行，或端口被占用。");
            std::process::exit(1);
        }
    };
    println!("\nagent 已启动，监听 0.0.0.0:{port}");
    println!("等待主控连接...（保持本窗口开着，不要关闭；首次运行请允许防火墙放行）\n");

    let server = Arc::new(server);
    let state = Arc::new(AgentState {
        servers: IperfServerMgr::new(),
        clients: IperfClientJobMgr::new(),
        monitors: MonitorMgr::new(),
        default_prefixes: cfg.ipv4_prefixes.clone(),
        owner_lifecycle: OwnerLifecycle::new(),
    });

    for _ in 0..WORKERS {
        let srv = Arc::clone(&server);
        let st = Arc::clone(&state);
        std::thread::spawn(move || loop {
            match srv.recv() {
                Ok(rq) => handle(rq, &st),
                Err(e) => {
                    eprintln!("[agent] 接收请求失败: {e}");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        });
    }

    // 主线程做定期清理
    loop {
        std::thread::sleep(SWEEP_INTERVAL);
        state.servers.sweep(SERVER_MAX_AGE);
        state.clients.sweep(CLIENT_JOB_MAX_AGE);
        state.monitors.sweep(MONITOR_MAX_AGE);
    }
}

fn handle(mut rq: Request, st: &Arc<AgentState>) {
    let method = rq.method().clone();
    let url = rq.url().to_string();
    let peer = rq
        .remote_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|| "?".into());

    let body = {
        let mut limited = rq.as_reader().take(MAX_BODY);
        let mut bytes = Vec::new();
        let _ = limited.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    };
    println!("[{}] {} {} 来自 {}", now_hms(), method, url, peer);

    // handler panic 不能弄崩 server
    let resp_body = std::panic::catch_unwind(AssertUnwindSafe(|| route(&method, &url, &body, st)))
        .unwrap_or_else(|_| err_json("agent 内部错误(panic)，其余功能不受影响"));

    let header = Header::from_bytes(
        &b"Content-Type"[..],
        &b"application/json; charset=utf-8"[..],
    )
    .expect("header");
    let resp = Response::from_data(resp_body.into_bytes()).with_header(header);
    let _ = rq.respond(resp);
}

fn route(method: &Method, url: &str, body: &str, st: &Arc<AgentState>) -> String {
    let path = url.split('?').next().unwrap_or(url);
    match (method, path) {
        (Method::Get, "/health") | (Method::Post, "/health") => {
            let mut capabilities = vec![
                RELIABLE_LIFECYCLE_CAPABILITY.into(),
                LIVE_NIC_PROGRESS_CAPABILITY.into(),
            ];
            if cfg!(windows) {
                capabilities.push(CTS_TRAFFIC_CAPABILITY.into());
            }
            ok_json(HealthOut {
                hostname: crate::util::hostname(),
                os: os_name(),
                version: env!("CARGO_PKG_VERSION").into(),
                iperf3: iperf3_version(),
                ctstraffic: ctstraffic_version(),
                capabilities,
            })
        }
        (Method::Post, "/info") => {
            let req: InfoReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let prefixes = if req.ipv4_prefixes.is_empty() {
                st.default_prefixes.clone()
            } else {
                req.ipv4_prefixes
            };
            ok_json(scan_host(&prefixes))
        }
        (Method::Post, "/ping") => {
            let req: PingReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            println!(
                "    执行 ping: {} -> {} (n={})",
                req.src, req.dst, req.count
            );
            ok_json(ping::run(&req))
        }
        (Method::Post, "/iperf/server/start") => {
            let req: IperfServerStartReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let Some(bin) = find_iperf3() else {
                return err_json("辅测机未找到 iperf3，请把 iperf3.exe 放到 agent 程序同目录");
            };
            match st
                .owner_lifecycle
                .with_start(&req.owner_id, || st.servers.start(&bin, &req))
            {
                Ok(cmd) => {
                    println!("    iperf3 server 已启动: {cmd}");
                    ok_json(IperfServerStartOut { cmd })
                }
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/iperf/server/stop") => {
            let req: IperfServerStopReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match st.servers.stop_checked(
                req.port,
                &req.request_id,
                Duration::from_secs(req.wait_secs),
            ) {
                Ok(out) if out.terminated => ok_json(out),
                Ok(_) => err_json("iperf3 server 停止后未确认退出"),
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/iperf/client/run") => {
            let req: IperfClientReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let Some(bin) = find_iperf3() else {
                return err_json("辅测机未找到 iperf3，请把 iperf3.exe 放到 agent 程序同目录");
            };
            println!(
                "    执行 iperf3 client: -c {} -p {} ({}s)...",
                req.dst, req.port, req.duration
            );
            let out = crate::cmd::iperf::run_client(&bin, &req, |line| {
                if line.contains("/sec") || line.to_lowercase().contains("error") {
                    println!("      {line}");
                }
            });
            ok_json(out)
        }
        (Method::Post, "/iperf/client/start") => {
            let req: IperfClientStartReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let Some(bin) = find_iperf3() else {
                return err_json("辅测机未找到 iperf3，请把 iperf3.exe 放到 agent 程序同目录");
            };
            let owner_id = req.owner_id.clone();
            match st
                .owner_lifecycle
                .with_start(&owner_id, || st.clients.start_request(bin, req))
            {
                Ok(id) => {
                    println!("    iperf3 client 异步作业已创建/复用: {id}");
                    let elapsed_ms = st.clients.elapsed_ms(&id).unwrap_or(0);
                    ok_json(IperfClientStartOut { id, elapsed_ms })
                }
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/iperf/client/status") => {
            let req: IperfClientStatusReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match st.clients.status(&req.id, req.cursor) {
                Ok(out) => ok_json(out),
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/iperf/client/stop") => {
            let req: IperfClientStopReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let wait = if req.wait_secs == 0 {
                Duration::from_secs(10)
            } else {
                Duration::from_secs(req.wait_secs)
            };
            match st.clients.stop_checked(&req.id, wait) {
                Ok(mut out) if out.terminated => {
                    // 常规 iperf client 已通过 status 取过完整结果；stop 响应不再
                    // 重复传输可能很大的 interval 输出，但保留进程是否启动/回收
                    // 等紧凑生命周期证据，供单流安全重试决定是否允许复用端口。
                    // CTS server 的 stop 路由仍保留完整 result，用来审计另一端摘要。
                    if let Some(result) = out.result.as_mut() {
                        result.cmd.clear();
                        result.output.clear();
                    }
                    ok_json(out)
                }
                Ok(_) => err_json("iperf3 client 停止后未确认退出"),
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/ctstraffic/start") => {
            let req: CtsTrafficStartReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            if !cfg!(windows) {
                return err_json("ctsTraffic 仅支持 Windows 10+，当前 agent 平台不支持");
            }
            let Some(bin) = find_ctstraffic() else {
                return err_json(
                    "辅测机未找到 ctsTraffic.exe，请把官方 x64 版本放到 agent 程序同目录或 PATH",
                );
            };
            let owner_id = req.owner_id.clone();
            match st.owner_lifecycle.with_start(&owner_id, || {
                ctstraffic::start_managed_job(&st.clients, bin, req)
            }) {
                Ok(id) => {
                    println!("    ctsTraffic 异步作业已创建/复用: {id}");
                    let elapsed_ms = st.clients.elapsed_ms(&id).unwrap_or(0);
                    ok_json(CtsTrafficStartOut { id, elapsed_ms })
                }
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/ctstraffic/status") => {
            let req: CtsTrafficStatusReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match st.clients.status(&req.id, req.cursor) {
                Ok(out) => ok_json(out),
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/ctstraffic/stop") => {
            let req: CtsTrafficStopReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let wait = if req.wait_secs == 0 {
                Duration::from_secs(10)
            } else {
                Duration::from_secs(req.wait_secs)
            };
            match st.clients.stop_checked(&req.id, wait) {
                Ok(out) if out.terminated => ok_json(out),
                Ok(_) => err_json("ctsTraffic 作业停止后未确认退出"),
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/monitor/start") => {
            let req: MonitorStartReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match st.owner_lifecycle.with_start(&req.owner_id, || {
                st.monitors
                    .start_owned(&req.iface, req.interval_ms, &req.owner_id, req.lease_secs)
            }) {
                Ok(id) => ok_json(MonitorStartOut { id }),
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/monitor/status") => {
            let req: MonitorStatusReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match st.monitors.status(&req.id) {
                Ok(out) => ok_json(out),
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/monitor/stop") => {
            let req: MonitorStopReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match st.monitors.stop(&req.id) {
                Ok(out) => ok_json(out),
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/resources/cleanup") => {
            let req: ResourceCleanupReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            if req.owner_id.is_empty() {
                return err_json("resources cleanup 的 owner_id 不能为空");
            }
            if !owner_id_ok(&req.owner_id) {
                return err_json("resources cleanup 的 owner_id 非法");
            }
            let client_wait = if req.wait_secs == 0 {
                Duration::from_secs(10)
            } else {
                Duration::from_secs(req.wait_secs)
            };

            let out = st.owner_lifecycle.with_cleanup(&req.owner_id, || {
                // 先同时取消发送端 client，再关闭 listener，最后结算 monitor；这样旧
                // client 不会在 server 重建窗口中误连，同时 monitor 能记录完整尾部样本。
                let clients = st.clients.stop_owner(&req.owner_id, client_wait);
                let servers = st.servers.stop_owner(&req.owner_id, Duration::ZERO);
                let monitors = st.monitors.stop_owner(&req.owner_id);
                let mut errors = Vec::new();
                errors.extend(clients.errors);
                errors.extend(servers.errors);
                let mut stopped_monitors = 0usize;
                for (id, result) in monitors {
                    match result {
                        Ok(_) => stopped_monitors += 1,
                        Err(e) => errors.push(format!("monitor {id} 清理失败: {e}")),
                    }
                }
                ResourceCleanupOut {
                    servers: servers.stopped,
                    clients: clients.stopped,
                    monitors: stopped_monitors,
                    errors,
                }
            });
            ok_json(out)
        }
        (Method::Post, "/screenshot") => {
            let _req: ScreenshotReq = parse(body).unwrap_or_default();
            match screenshot::capture_png() {
                Ok(png) => ok_json(ScreenshotOut {
                    image_b64: base64::engine::general_purpose::STANDARD.encode(png),
                    format: "png".into(),
                }),
                Err(e) => err_json(&e),
            }
        }
        _ => err_json(&format!("未知接口: {method} {path}")),
    }
}

fn parse<T: serde::de::DeserializeOwned + Default>(body: &str) -> Result<T, String> {
    if body.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(body).map_err(|e| err_json(&format!("请求 JSON 解析失败: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn empty_state() -> Arc<AgentState> {
        Arc::new(AgentState {
            servers: IperfServerMgr::new(),
            clients: IperfClientJobMgr::new(),
            monitors: MonitorMgr::new(),
            default_prefixes: Vec::new(),
            owner_lifecycle: OwnerLifecycle::new(),
        })
    }

    #[test]
    fn owner_cleanup_route_is_idempotent_and_health_advertises_capability() {
        let state = empty_state();
        let body = serde_json::to_string(&ResourceCleanupReq {
            owner_id: "unit-route-test".into(),
            wait_secs: 1,
        })
        .unwrap();
        for _ in 0..2 {
            let response = route(&Method::Post, "/resources/cleanup", &body, &state);
            let parsed: Resp<ResourceCleanupOut> = serde_json::from_str(&response).unwrap();
            assert!(parsed.ok);
            let out = parsed.data.unwrap();
            assert_eq!((out.servers, out.clients, out.monitors), (0, 0, 0));
            assert!(out.errors.is_empty());
        }

        let late_monitor = serde_json::to_string(&MonitorStartReq {
            iface: "interface-must-not-be-read".into(),
            interval_ms: 1_000,
            owner_id: "unit-route-test".into(),
            lease_secs: 60,
        })
        .unwrap();
        let response = route(&Method::Post, "/monitor/start", &late_monitor, &state);
        let parsed: Resp<MonitorStartOut> = serde_json::from_str(&response).unwrap();
        assert!(!parsed.ok);
        assert!(parsed
            .error
            .unwrap_or_default()
            .contains("拒绝迟到的资源 start"));

        let response = route(&Method::Get, "/health", "", &state);
        let health: Resp<HealthOut> = serde_json::from_str(&response).unwrap();
        assert!(health.ok);
        let health = health.data.unwrap();
        let capabilities = &health.capabilities;
        assert!(capabilities
            .iter()
            .any(|capability| capability == RELIABLE_LIFECYCLE_CAPABILITY));
        assert!(capabilities
            .iter()
            .any(|capability| capability == LIVE_NIC_PROGRESS_CAPABILITY));
        assert_eq!(
            capabilities
                .iter()
                .any(|capability| capability == CTS_TRAFFIC_CAPABILITY),
            cfg!(windows),
            "CTS capability 只能由 Windows agent 声明"
        );
        if !cfg!(windows) {
            assert_eq!(health.ctstraffic, None);
        }

        let response = route(
            &Method::Post,
            "/monitor/status",
            r#"{"id":"missing-monitor"}"#,
            &state,
        );
        let parsed: Resp<MonitorStatusOut> = serde_json::from_str(&response).unwrap();
        assert!(!parsed.ok);
    }

    #[cfg(not(windows))]
    #[test]
    fn ctstraffic_start_route_explicitly_rejects_non_windows_agents() {
        let state = empty_state();
        let body = serde_json::to_string(&CtsTrafficStartReq::default()).unwrap();
        let response = route(&Method::Post, "/ctstraffic/start", &body, &state);
        let parsed: Resp<CtsTrafficStartOut> = serde_json::from_str(&response).unwrap();

        assert!(!parsed.ok);
        let error = parsed.error.unwrap_or_default();
        assert!(error.contains("仅支持 Windows 10+"));
        assert!(error.contains("当前 agent 平台不支持"));
    }

    #[test]
    fn owner_cleanup_waits_for_inflight_start_then_rejects_late_start() {
        let lifecycle = Arc::new(OwnerLifecycle::new());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let start_lifecycle = Arc::clone(&lifecycle);
        let start = std::thread::spawn(move || {
            start_lifecycle.with_start("owner-race", || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let (cleanup_tx, cleanup_rx) = mpsc::channel();
        let cleanup_lifecycle = Arc::clone(&lifecycle);
        let cleanup = std::thread::spawn(move || {
            cleanup_lifecycle.with_cleanup("owner-race", || cleanup_tx.send(()).unwrap());
        });
        assert!(
            cleanup_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "cleanup 写锁必须等待同 owner 的在途 start 完成"
        );
        release_tx.send(()).unwrap();
        assert!(start.join().unwrap().is_ok());
        cleanup_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        cleanup.join().unwrap();

        let late: Result<(), String> = lifecycle.with_start("owner-race", || {
            panic!("owner cleanup 后的迟到 start 不应执行")
        });
        assert!(late.is_err());
    }
}
