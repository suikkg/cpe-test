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
use crate::cmd::tools::{
    ctstraffic_platform_supported, ctstraffic_version, find_ctstraffic, find_iperf3, iperf3_version,
};
use crate::config::Config;
use crate::nic::monitor::MonitorMgr;
use crate::nic::scan_host;
use crate::protocol::*;
use crate::resource::{AgentResourceInventory, ResourceInventory};
use crate::util::{now_hms, os_name};
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
/// 每 200ms 轮询取消标志（P0: Ctrl+C 到资源归零 ≤5 秒）；租约清扫每 30 秒一次
const SWEEP_INTERVAL: Duration = Duration::from_millis(200);
const SWEEP_EVERY_TICKS: u64 = 150; // 200ms × 150 = 30s
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
    /// 共享访问令牌；空表示不启用认证。
    token: String,
    /// 状态页要显示的活动记录。业务处理不读它，只往里写。
    activity: Arc<crate::agent::webui::Activity>,
}

/// 启动 agent（阻塞不返回）。
///
/// `ui_port` 为 `None` 时不起本机状态页（`--no-ui`）。
pub fn run(port: u16, cfg: &Config, ui_port: Option<u16>, ui_bind: &str) {
    // P0: agent 也必须安装 Ctrl+C 处理器，否则无法优雅退出/清理。
    crate::cancel::setup_cancel_handler();
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
    if !ctstraffic_platform_supported() {
        println!("ctsTraffic: 当前平台或系统版本不支持（仅 Windows 10+）");
    } else {
        match ctstraffic_version() {
            Some(version) => println!("ctsTraffic: {version}"),
            None => println!(
                "!! 提示: 未找到 ctsTraffic.exe；iperf3/ping 仍可用，CTS 测试会被前置检查拦截。"
            ),
        }
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

    let bind_addr = if cfg.agent_bind.trim().is_empty() {
        "0.0.0.0".to_string()
    } else {
        cfg.agent_bind.trim().to_string()
    };
    let server = match Server::http((bind_addr.as_str(), port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("\n!! 启动失败: {bind_addr}:{port} 无法监听 ({e})");
            eprintln!("!! 可能已有一个 agent 在运行，或端口被占用。");
            std::process::exit(1);
        }
    };
    let auth = if cfg.agent_token.trim().is_empty() {
        None
    } else {
        Some(cfg.agent_token.trim().to_string())
    };
    println!("\nagent 已启动，监听 {bind_addr}:{port}");
    match &auth {
        Some(_) => println!(
            "已启用共享令牌认证：主控需在 config.json 配置相同 agent_token 才能连接。"
        ),
        None => println!(
            "!! 未配置 agent_token：任何能访问该端口的主机都能控制本 agent。\n!! 生产/非隔离网络请用 --token 或 config.json 的 agent_token 开启认证。"
        ),
    }
    println!("等待主控连接...（保持本窗口开着，不要关闭；首次运行请允许防火墙放行）\n");

    let server = Arc::new(server);
    let token_configured = auth.is_some();
    let activity = Arc::new(crate::agent::webui::Activity::new());
    let state = Arc::new(AgentState {
        servers: IperfServerMgr::new(),
        clients: IperfClientJobMgr::new(),
        monitors: MonitorMgr::new(),
        default_prefixes: cfg.ipv4_prefixes.clone(),
        owner_lifecycle: OwnerLifecycle::new(),
        token: auth.unwrap_or_default(),
        activity: Arc::clone(&activity),
    });

    if let Some(ui_port) = ui_port {
        crate::agent::webui::spawn(
            ui_bind,
            ui_port,
            &bind_addr,
            port,
            token_configured,
            &activity,
        );
    }

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

    // 主线程做定期清理；第一次 Ctrl+C 时优雅退出（与主控一致）。
    // P0: Ctrl+C 到资源归零 ≤5 秒 —— 200ms 轮询取消标志，退出前停止全部资源。
    let mut tick: u64 = 0;
    loop {
        if crate::cancel::is_cancelled() {
            println!("\n辅测 agent 收到 Ctrl+C，正在退出...");
            let started = std::time::Instant::now();
            let inv = AgentResourceInventory::new(&state.clients, &state.servers, &state.monitors);
            let out = inv.cleanup_all(Duration::from_secs(5));
            for e in &out.errors {
                eprintln!("[agent] 退出清理错误: {e}");
            }
            println!(
                "[agent] 退出清理完成：servers={} clients={} monitors={} errors={} 耗时 {:.2}s",
                out.servers,
                out.clients,
                out.monitors,
                out.errors.len(),
                started.elapsed().as_secs_f64()
            );
            break;
        }
        std::thread::sleep(SWEEP_INTERVAL);
        tick += 1;
        // Keep the modulo form for the documented Rust 1.82 MSRV.
        #[allow(clippy::manual_is_multiple_of)]
        if tick % SWEEP_EVERY_TICKS == 0 {
            state.servers.sweep(SERVER_MAX_AGE);
            state.clients.sweep(CLIENT_JOB_MAX_AGE);
            state.monitors.sweep(MONITOR_MAX_AGE);
        }
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

    // 认证在路由之前完成：未认证请求必须返回 401 且不创建任何资源。
    if !st.token.is_empty() && !request_authorized(&rq, &st.token) {
        let header = Header::from_bytes(
            &b"Content-Type"[..],
            &b"application/json; charset=utf-8"[..],
        )
        .expect("header");
        let resp = Response::from_data(
            err_json("未认证：缺少或错误的 Authorization: Bearer <token>").into_bytes(),
        )
        .with_status_code(401)
        .with_header(header);
        st.activity
            .record(&peer, url.split('?').next().unwrap_or(&url), false);
        let _ = rq.respond(resp);
        return;
    }

    // handler panic 不能弄崩 server
    let resp_body = std::panic::catch_unwind(AssertUnwindSafe(|| route(&method, &url, &body, st)))
        .unwrap_or_else(|_| err_json("agent 内部错误(panic)，其余功能不受影响"));
    st.activity.record(
        &peer,
        url.split('?').next().unwrap_or(&url),
        response_succeeded(&resp_body),
    );

    let header = Header::from_bytes(
        &b"Content-Type"[..],
        &b"application/json; charset=utf-8"[..],
    )
    .expect("header");
    let resp = Response::from_data(resp_body.into_bytes()).with_header(header);
    let _ = rq.respond(resp);
}

/// 响应体是不是成功的。`ok_json` / `err_json` 产出的都是 `{"ok":bool,...}`，
/// 状态页只需要这一个比特，不值得为它把整个响应再解析一遍。
fn response_succeeded(body: &str) -> bool {
    body.trim_start().starts_with("{\"ok\":true")
}

/// 校验请求的 `Authorization: Bearer <token>` 头。
/// 用恒定时间比较避免令牌侧信道；token 为空时视为未启用认证。
fn request_authorized(rq: &Request, expected: &str) -> bool {
    let header_value = rq
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str());
    bearer_token_ok(header_value, expected)
}

/// 纯函数：校验 Authorization 头的 Bearer 令牌。
pub(crate) fn bearer_token_ok(header_value: Option<&str>, expected: &str) -> bool {
    if expected.is_empty() {
        return true;
    }
    let Some(provided) = header_value.and_then(|v| v.strip_prefix("Bearer ")) else {
        return false;
    };
    let provided = provided.trim();
    if provided.len() != expected.len() {
        return false;
    }
    // 恒定时间比较。
    provided
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn route(method: &Method, url: &str, body: &str, st: &Arc<AgentState>) -> String {
    let path = url.split('?').next().unwrap_or(url);
    match (method, path) {
        (Method::Get, "/health") | (Method::Post, "/health") => {
            let mut capabilities = vec![
                RELIABLE_LIFECYCLE_CAPABILITY.into(),
                LIVE_NIC_PROGRESS_CAPABILITY.into(),
            ];
            if ctstraffic_platform_supported() {
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
            if !ctstraffic_platform_supported() {
                return err_json(
                    "ctsTraffic 仅支持 Windows 10+，当前 agent 平台不支持或系统版本检测未通过",
                );
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
                Ok(id) => {
                    let elapsed_ms = st.monitors.elapsed_ms(&id).unwrap_or(0);
                    ok_json(MonitorStartOut { id, elapsed_ms })
                }
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
                AgentResourceInventory::new(&st.clients, &st.servers, &st.monitors)
                    .cleanup_owner(&req.owner_id, client_wait)
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
            token: String::new(),
            activity: Arc::new(crate::agent::webui::Activity::new()),
        })
    }
    #[test]
    fn bearer_token_auth_rejects_missing_wrong_and_accepts_matching() {
        // 未启用认证：任何头都放行。
        assert!(bearer_token_ok(None, ""));
        assert!(bearer_token_ok(Some("Bearer anything"), ""));
        // 启用认证：缺失头被拒。
        assert!(!bearer_token_ok(None, "secret"));
        // 非 Bearer 前缀被拒。
        assert!(!bearer_token_ok(Some("Basic abc"), "secret"));
        // 错误令牌被拒。
        assert!(!bearer_token_ok(Some("Bearer wrong"), "secret"));
        // 长度不同直接被拒（不进入常量时间比较）。
        assert!(!bearer_token_ok(Some("Bearer secret-extralong"), "secret"));
        // 正确令牌放行（含前后空白容忍）。
        assert!(bearer_token_ok(Some("Bearer secret"), "secret"));
        assert!(bearer_token_ok(Some("Bearer  secret  "), "secret"));
    }

    #[test]
    fn unauthenticated_request_returns_401_without_creating_resources() {
        // 启用令牌的 agent：未认证请求必须 401 且不得创建任何资源。
        // 这里用真实 tiny_http + handle() 验证 401 状态码与资源零创建。
        let state = Arc::new(AgentState {
            servers: IperfServerMgr::new(),
            clients: IperfClientJobMgr::new(),
            monitors: MonitorMgr::new(),
            default_prefixes: Vec::new(),
            owner_lifecycle: OwnerLifecycle::new(),
            token: "unit-secret".into(),
            activity: Arc::new(crate::agent::webui::Activity::new()),
        });
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let st = Arc::clone(&state);
        std::thread::spawn(move || {
            for rq in server.incoming_requests() {
                handle(rq, &st);
            }
        });

        // 未带令牌 → 401，且不创建任何资源。
        let (status, _body) = crate::http_client::post_json_auth(
            "127.0.0.1",
            port,
            "/monitor/start",
            r#"{"iface":"unreachable-iface","interval_ms":1000}"#,
            "",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(status, 401);
        assert!(
            state.monitors.status("mon1").is_err(),
            "未认证请求不得创建 monitor"
        );

        // 错误令牌 → 401。
        let (status, _body) = crate::http_client::post_json_auth(
            "127.0.0.1",
            port,
            "/health",
            "{}",
            "wrong-token",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(status, 401);

        // 正确令牌 → 200。
        let body = serde_json::json!({}).to_string();
        let (status, text) = crate::http_client::post_json_auth(
            "127.0.0.1",
            port,
            "/health",
            &body,
            "unit-secret",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(status, 200);
        let resp: Resp<HealthOut> = serde_json::from_str(&text).unwrap();
        assert!(resp.ok);
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
            ctstraffic_platform_supported(),
            "CTS capability 只能由通过 Windows 10+ 门槛的 agent 声明"
        );
        if !ctstraffic_platform_supported() {
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
