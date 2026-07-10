//! agent REST server（tiny_http，固定线程池，无 async）
//!
//! 端点（全部 POST JSON，另有 GET /health）：
//!   /info /ping /iperf/server/start /iperf/server/stop
//!   /iperf/client/run（兼容） /iperf/client/start /status /stop
//!   /monitor/start /monitor/stop /screenshot /health
//! 响应统一 {"ok":bool,"error":...,"data":{...}}，HTTP 状态恒 200。

use crate::cmd::iperf::{IperfClientJobMgr, IperfServerMgr};
use crate::config::Config;
use crate::nic::monitor::MonitorMgr;
use crate::nic::scan_host;
use crate::protocol::*;
use crate::util::{find_iperf3, iperf3_version, now_hms, os_name};
use crate::{ping, screenshot};
use base64::Engine;
use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

const WORKERS: usize = 16;
const MAX_BODY: u64 = 100 * 1024 * 1024;
/// 每 30 秒清理一次过期状态（见 PROJECT_PLAN）
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
const SERVER_MAX_AGE: Duration = Duration::from_secs(90_000);
const CLIENT_JOB_MAX_AGE: Duration = Duration::from_secs(90_000);
const MONITOR_MAX_AGE: Duration = Duration::from_secs(90_000);

pub struct AgentState {
    pub servers: IperfServerMgr,
    pub clients: IperfClientJobMgr,
    pub monitors: MonitorMgr,
    pub default_prefixes: Vec<String>,
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
        (Method::Get, "/health") | (Method::Post, "/health") => ok_json(HealthOut {
            hostname: crate::util::hostname(),
            os: os_name(),
            version: env!("CARGO_PKG_VERSION").into(),
            iperf3: iperf3_version(),
        }),
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
            match st.servers.start(&bin, &req) {
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
            let out = st
                .servers
                .stop(req.port, Duration::from_secs(req.wait_secs));
            ok_json(out)
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
            let id = st.clients.start(bin, req.request);
            println!("    iperf3 client 异步作业已创建: {id}");
            ok_json(IperfClientStartOut { id })
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
            match st.clients.stop(&req.id) {
                Ok((existed, was_done)) => ok_json(IperfClientStopOut { existed, was_done }),
                Err(e) => err_json(&e),
            }
        }
        (Method::Post, "/monitor/start") => {
            let req: MonitorStartReq = match parse(body) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match st.monitors.start(&req.iface, req.interval_ms) {
                Ok(id) => ok_json(MonitorStartOut { id }),
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
