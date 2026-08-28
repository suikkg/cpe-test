//! 辅测机本地状态页。
//!
//! 辅测机那台电脑上的人只需要弄明白三件事：**把哪个 IP 报给主控、agent 到底
//! 起来没有、现在正在被要求做什么**。控制台窗口里这三件事被滚动的请求日志淹
//! 掉了——一轮 6 小时的测试会刷出几万行 `POST /iperf/client/status`。
//!
//! **只读。** 这个页面不控制 agent：没有启动、停止、改端口。给一个能从浏览器
//! 关掉测试设备的按钮，只会多一条出错路径，换不来任何东西——关窗口和 Ctrl+C
//! 本来就能停，而且那两条路径已经带着资源清理。
//!
//! **默认只监听回环，但可以用 `--ui-bind` 放开。** agent 的业务端口必须对局域网
//! 开放（主控要连），「这台机器上有哪些网卡、主控是谁」却不必跟着一起开放，所以
//! 默认值是 127.0.0.1。放开是一个需要显式写出来的选择：这个页面没有访问口令，
//! 绑到可路由地址上就等于把网卡列表、IP、主机名和「有没有配 token」公开给同网段。
//! 那仍然只是只读的信息泄露（页面本身不控制 agent），所以 `spawn()` 是打印一行
//! 警告而不是像主控控制台那样拒绝启动。

use crate::cmd::tools::iperf3_version;
use crate::nic::scan_host;
use crate::protocol::HostInfo;
use crate::util::{lock_recover, now_hms, os_name};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tiny_http::{Header, Method, Request, Response, Server};

const PAGE: &str = include_str!("webui.html");

/// 活动列表上限。够看清「刚才发生了什么」，又不会让每次轮询拖着几百 KB。
const ACTIVITY_MAX: usize = 200;

/// agent 处理过的一次（或连续同类的一批）请求。
#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    time: String,
    peer: String,
    label: String,
    path: String,
    ok: bool,
    /// 连续同类请求折叠后的次数。
    ///
    /// 主控一轮灌包会把 `/iperf/client/status` 轮询上百次，逐条列出会把真正有
    /// 信息量的那几行（起服务端、起客户端、取采样）整个挤出屏幕。
    count: u32,
}

#[derive(Default)]
struct ActivityInner {
    entries: VecDeque<ActivityEntry>,
    /// 最近一次请求的来源，用来在页面上确认「主控连上了、就是这台」。
    peer: Option<String>,
    last_request: Option<Instant>,
    total: u64,
}

/// agent 活动记录。业务线程写、状态页读，两边共享同一个实例。
#[derive(Default)]
pub struct Activity {
    inner: Mutex<ActivityInner>,
}

#[derive(Debug, Clone, Serialize)]
struct ActivityOut {
    entries: Vec<ActivityEntry>,
    peer: Option<String>,
    /// 距最近一次请求的秒数；页面据此区分「正在跑」和「早就没动静了」。
    idle_secs: Option<u64>,
    total: u64,
}

impl Activity {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一次已处理的请求。`peer` 是 tiny_http 给的 `IP:PORT`。
    pub fn record(&self, peer: &str, path: &str, ok: bool) {
        let peer = peer_ip(peer);
        let mut inner = lock_recover(&self.inner);
        inner.total += 1;
        inner.last_request = Some(Instant::now());
        inner.peer = Some(peer.clone());

        if let Some(last) = inner.entries.back_mut() {
            if last.peer == peer && last.path == path && last.ok == ok {
                last.count += 1;
                last.time = now_hms();
                return;
            }
        }
        inner.entries.push_back(ActivityEntry {
            time: now_hms(),
            peer,
            label: label_for(path).to_string(),
            path: path.to_string(),
            ok,
            count: 1,
        });
        while inner.entries.len() > ACTIVITY_MAX {
            inner.entries.pop_front();
        }
    }

    fn snapshot(&self) -> ActivityOut {
        let inner = lock_recover(&self.inner);
        ActivityOut {
            entries: inner.entries.iter().cloned().collect(),
            peer: inner.peer.clone(),
            idle_secs: inner.last_request.map(|at| at.elapsed().as_secs()),
            total: inner.total,
        }
    }
}

/// `IP:PORT` -> `IP`。IPv6 的 `[::1]:52001` 也要落到 `[::1]` 而不是被从中间切开。
fn peer_ip(peer: &str) -> String {
    match peer.rsplit_once(':') {
        Some((ip, port)) if !ip.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            ip.to_string()
        }
        _ => peer.to_string(),
    }
}

/// 请求路径 -> 页面上显示的中文动作。
///
/// 这里必须覆盖 agent 真正提供的每一个端点，否则页面会把正常测试显示成一串
/// 「未知请求」。`every_agent_endpoint_has_a_label` 会盯着这件事。
fn label_for(path: &str) -> &'static str {
    match path {
        "/health" => "握手与版本检查",
        "/info" => "读取本机网卡",
        "/ping" => "执行 Ping",
        "/iperf/server/start" => "启动 iperf3 服务端",
        "/iperf/server/stop" => "停止 iperf3 服务端",
        "/iperf/client/run" | "/iperf/client/start" => "启动 iperf3 灌包",
        "/iperf/client/status" => "查询灌包进度",
        "/iperf/client/stop" => "停止 iperf3 灌包",
        "/ctstraffic/start" => "启动 ctsTraffic",
        "/ctstraffic/status" => "查询 ctsTraffic 进度",
        "/ctstraffic/stop" => "停止 ctsTraffic",
        "/monitor/start" => "开始网卡计数采样",
        "/monitor/status" => "查询网卡采样",
        "/monitor/stop" => "取回网卡采样结果",
        "/resources/cleanup" => "清理残留进程",
        "/screenshot" => "截屏",
        _ => "未知请求",
    }
}

#[derive(Debug, Clone, Serialize)]
struct StatusOut {
    version: String,
    hostname: String,
    os: String,
    bind: String,
    port: u16,
    token_configured: bool,
    iperf3: Option<String>,
    /// 本机全部网卡，不按前缀过滤：报给主控的地址就得从这里挑。
    nics: HostInfo,
    uptime_secs: u64,
}

struct Console {
    bind: String,
    port: u16,
    token_configured: bool,
    started: Instant,
    activity: Arc<Activity>,
}

/// 在后台线程上起状态页，立刻返回。
///
/// 绑不上端口只打印一行提示就算了：状态页是给人看的便利设施，不能因为
/// 28802 被占用就让整台辅测机不能参与测试。
/// `ui_bind` 是状态页自己的监听地址；`agent_bind`/`agent_port` 是协议服务的，
/// 只用来显示在页面上。两者以前共用一个参数名，放开状态页绑定后必须分清。
pub fn spawn(
    ui_bind: &str,
    port: u16,
    agent_bind: &str,
    agent_port: u16,
    token_configured: bool,
    activity: &Arc<Activity>,
) {
    let console = Arc::new(Console {
        bind: agent_bind.to_string(),
        port: agent_port,
        token_configured,
        started: Instant::now(),
        activity: Arc::clone(activity),
    });
    // 和主控控制台共用一个拼法：裸 IPv6 要补方括号，否则 `--ui-bind ::1`
    // 拼出来的 "::1:28802" 根本解析不了，状态页会无声地起不来。
    let addr = crate::master::webui::listen_addr(ui_bind, port);
    let server = match Server::http(addr.as_str()) {
        Ok(server) => server,
        Err(error) => {
            println!("!! 状态页无法监听 {addr}（{error}）；agent 本身不受影响。");
            println!("!! 需要状态页就换个端口重启：cpe_test agent --ui-port 28812");
            return;
        }
    };
    let url = format!("http://{addr}");
    println!("辅测机状态页: {url}");
    if !crate::master::webui::bind_is_loopback(ui_bind) {
        // 状态页全是只读 GET，放开的后果止于「网卡列表、IP、主机名、
        // 有没有配 token」被同网段看见——比控制台轻得多，所以这里是提示
        // 而不是拒绝启动。但它确实是信息泄露，不能默不作声。
        println!("!! 状态页正监听在 {ui_bind}，同网段可见本机网卡与地址；它没有访问口令。");
    }
    println!("（本机浏览器打开即可看到要报给主控的 IP 和实时活动；不影响测试）");
    crate::console::open_url(&url);

    // 和主控控制台同一个理由要同一套解法：`/api/status` 要跑 `scan_host()`，
    // Windows 上会拉起 ipconfig/netsh 一到两秒，单线程期间每 1.5 秒一次的
    // 活动轮询全在排队——页面每分钟停顿一次。状态页的共享状态本来就在
    // Mutex 后面，并发处理不需要额外同步。
    let server = Arc::new(server);
    let mut started = 0;
    for idx in 0..AGENT_UI_WORKERS {
        let server = Arc::clone(&server);
        let console = Arc::clone(&console);
        if std::thread::Builder::new()
            .name(format!("cpe-agent-webui-{idx}"))
            .spawn(move || {
                while let Ok(request) = server.recv() {
                    handle(request, &console);
                }
            })
            .is_ok()
        {
            started += 1;
        }
    }
    if started == 0 {
        println!("!! 状态页线程启动失败；agent 本身不受影响。");
    }
}

/// 状态页的并发处理线程数。比主控控制台少：这个页面只有两个接口，
/// 需要挡的只是「重扫网卡的那一两秒别把活动轮询堵住」。
const AGENT_UI_WORKERS: usize = 2;

fn header(name: &'static [u8], value: &'static [u8]) -> Header {
    Header::from_bytes(name, value).expect("static response header")
}

fn json_response(body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_header(header(b"Content-Type", b"application/json; charset=utf-8"))
        .with_header(header(b"Cache-Control", b"no-store"))
        .with_header(header(b"X-Content-Type-Options", b"nosniff"))
}

fn page_response() -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(PAGE)
        .with_header(header(b"Content-Type", b"text/html; charset=utf-8"))
        .with_header(header(b"Cache-Control", b"no-store"))
        .with_header(header(b"X-Content-Type-Options", b"nosniff"))
        .with_header(header(b"Referrer-Policy", b"no-referrer"))
        .with_header(header(
            b"Content-Security-Policy",
            b"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ))
}

fn handle(request: Request, console: &Arc<Console>) {
    let path = request.url().split('?').next().unwrap_or("/").to_string();
    if path == "/" || path == "/index.html" {
        let _ = request.respond(page_response());
        return;
    }
    // 全是只读 GET，没有任何会改状态的接口，因此不需要 CSRF 那一层；
    // 没有 CORS 头意味着别的站点即使发得出请求也读不到响应。
    let out = if *request.method() != Method::Get {
        Err("状态页只提供只读接口".to_string())
    } else if path == "/api/status" {
        api_status(console)
    } else if path == "/api/activity" {
        Ok(serde_json::to_value(console.activity.snapshot()).unwrap_or(serde_json::Value::Null))
    } else {
        Err("未知接口".to_string())
    };
    let body = match out {
        Ok(value) => crate::protocol::ok_json(value),
        Err(error) => crate::protocol::err_json(&error),
    };
    let _ = request.respond(json_response(body));
}

fn api_status(console: &Arc<Console>) -> Result<serde_json::Value, String> {
    serde_json::to_value(StatusOut {
        version: env!("CARGO_PKG_VERSION").into(),
        hostname: crate::util::hostname(),
        os: os_name(),
        bind: console.bind.clone(),
        port: console.port,
        token_configured: console.token_configured,
        iperf3: iperf3_version(),
        // 不按 ipv4_prefixes 过滤：要报给主控的那个地址常常就在被过滤掉的
        // 管理网段上，过滤过的表在这里等于把答案藏起来。
        nics: scan_host(&[]),
        uptime_secs: console.started.elapsed().as_secs(),
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_polling_collapses_into_one_line() {
        let activity = Activity::new();
        activity.record("10.0.0.9:5001", "/iperf/server/start", true);
        for _ in 0..150 {
            activity.record("10.0.0.9:5002", "/iperf/client/status", true);
        }
        activity.record("10.0.0.9:5003", "/monitor/stop", true);

        let out = activity.snapshot();
        assert_eq!(
            out.entries.len(),
            3,
            "150 次进度轮询必须折叠掉，否则起服务端那一行会被挤出屏幕"
        );
        assert_eq!(out.entries[1].count, 150);
        assert_eq!(out.entries[1].label, "查询灌包进度");
        assert_eq!(out.total, 152, "折叠只影响显示，不影响计数");
    }

    #[test]
    fn a_failure_never_hides_inside_a_run_of_successes() {
        let activity = Activity::new();
        activity.record("10.0.0.9:5001", "/iperf/client/status", true);
        activity.record("10.0.0.9:5001", "/iperf/client/status", false);
        activity.record("10.0.0.9:5001", "/iperf/client/status", true);

        let out = activity.snapshot();
        assert_eq!(out.entries.len(), 3, "成败不同就不能折叠成一行");
        assert!(!out.entries[1].ok);
    }

    #[test]
    fn the_activity_list_is_capped_but_keeps_the_newest() {
        let activity = Activity::new();
        for i in 0..(ACTIVITY_MAX + 50) {
            // 每条 path 都不同，强制不折叠。
            activity.record("10.0.0.9:5001", &format!("/ping?{i}"), true);
        }
        let out = activity.snapshot();
        assert_eq!(out.entries.len(), ACTIVITY_MAX);
        assert!(out.entries.last().expect("非空").path.ends_with("249"));
    }

    #[test]
    fn the_peer_shown_is_an_address_not_an_ephemeral_port() {
        assert_eq!(peer_ip("10.228.46.50:51314"), "10.228.46.50");
        assert_eq!(peer_ip("[fe80::1]:51314"), "[fe80::1]");
        // tiny_http 拿不到来源时给的是 "?"，别把它切碎。
        assert_eq!(peer_ip("?"), "?");
        assert_eq!(peer_ip("10.228.46.50"), "10.228.46.50");
    }

    /// agent 每加一个端点，状态页就得能说出它是在干什么。
    ///
    /// 漏一个的代价不是崩溃，而是一次真实测试在页面上显示成一串「未知请求」——
    /// 恰好是最需要这个页面的时候最没用。
    #[test]
    fn every_agent_endpoint_has_a_label() {
        let source = include_str!("server.rs");
        let mut routes: Vec<&str> = Vec::new();
        for rest in source.split("(Method::").skip(1) {
            let Some((_, after)) = rest.split_once(", \"") else {
                continue;
            };
            let Some((path, _)) = after.split_once('"') else {
                continue;
            };
            if path.starts_with('/') && !routes.contains(&path) {
                routes.push(path);
            }
        }
        assert!(
            routes.len() >= 15,
            "没抓到 agent 的路由表，解析方式该更新了: {routes:?}"
        );
        let unlabelled: Vec<&str> = routes
            .into_iter()
            .filter(|path| label_for(path) == "未知请求")
            .collect();
        assert!(
            unlabelled.is_empty(),
            "这些端点在状态页上会显示成「未知请求」，请到 label_for 里补上: {unlabelled:?}"
        );
    }
}
