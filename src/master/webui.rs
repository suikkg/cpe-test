//! 本地 Web 控制台：勾选执行 + 实时进度。
//!
//! 形态选择的理由写在 .ai/DESIGN-v4.3.0.md F3：界面主体是「配对 × 方向」的
//! 勾选矩阵、一张可编辑的门限/带宽表、一条实时进度流。这三样在 HTML 里都是
//! 原生控件，在 egui 或裸 Win32 里都要手搓；而 `tiny_http` 本来就是依赖
//! （agent 一直在用），单 exe 和三平台 CI 都不受影响。
//!
//! **这里不是第二条执行路径。** 「开始测试」做的事就是把界面状态序列化成一份
//! config，然后调用同一个 `run_master()`。CI 的 `--auto` 回归防线、既有的
//! configs.json 用法、resume 断点续跑全都不动，控制台只是 config 的图形编辑器
//! 加进度视图——多一条执行路径就多一处会和判定口径分叉的地方。

use crate::config::{load_config, Config, OneOrMany, TestSpec, UdpProfile};
use crate::http_client;
use crate::master::builder::{self, build_units};
use crate::master::ui::{run_master, MasterOpts};
use crate::protocol::{HealthOut, HostInfo, InfoReq, Resp};
use crate::util::{clear_log_mirror, lock_recover, log_tail_since};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

const PAGE: &str = include_str!("webui.html");
const MAX_BODY_BYTES: u64 = 1_048_576;

#[derive(Default)]
struct UiState {
    cfg: Config,
    agent_host: String,
    master: HostInfo,
    agent: HostInfo,
}

struct Console {
    state: Mutex<UiState>,
    running: AtomicBool,
    report: Mutex<String>,
}

/// 界面提交回来的一条配对选择。
#[derive(Debug, Clone, Deserialize)]
struct PairSelection {
    /// `master:NAME=以太网 6`
    src: String,
    dst: String,
    #[serde(default)]
    directions: Vec<String>,
    #[serde(default)]
    transports: Vec<String>,
    #[serde(default)]
    ip: Vec<String>,
}

/// 一块网卡在所有配对中共用的判定/负载策略。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct NicPolicySelection {
    /// `master:NAME=以太网 6`
    endpoint: String,
    /// 这块网卡作为接收端时的 RX 通过门限。
    ///
    /// 两种写法共用一个输入框：`1800` = 绝对 1800Mbps，`90%` = 协商速率的
    /// 90%。分成两个框会逼着人先想清楚用哪种，而这两种本来就是二选一。
    #[serde(default)]
    rx_target: String,
    /// 这块网卡作为发送端时的 UDP 单流带宽；留空表示走全局档位。
    #[serde(default)]
    udp_bandwidth: String,
    /// 这块网卡作为发送端时的 UDP 报文长度（`-l`）；留空表示走全局档位。
    #[serde(default)]
    udp_length: String,
}

/// RX 门限输入框的两种写法。
#[derive(Debug, Clone, Copy, PartialEq)]
enum RxTarget {
    Mbps(f64),
    /// 协商速率的百分比，`90.0` = 90%。
    Percent(f64),
}

/// 解析 `1800` / `1800.5` / `90%`。空串返回 `Ok(None)`。
fn parse_rx_target(raw: &str) -> Result<Option<RxTarget>, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let (number, is_percent) = match text.strip_suffix('%') {
        Some(rest) => (rest.trim(), true),
        None => (text, false),
    };
    let value: f64 = number
        .parse()
        .map_err(|_| format!("看不懂的门限写法 {raw:?}，请填 1800 或 90%"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("门限必须是大于 0 的有限值，当前 {raw:?}"));
    }
    if is_percent {
        // 上限放到 200%：聚合口、多流叠加确实可能超过单口协商速率，
        // 但一个三位数以上的百分比几乎一定是手滑。
        if value > 200.0 {
            return Err(format!("百分比门限 {raw:?} 超过 200%，请确认是不是写错了"));
        }
        Ok(Some(RxTarget::Percent(value)))
    } else {
        Ok(Some(RxTarget::Mbps(value)))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RunRequest {
    #[serde(default)]
    pairs: Vec<PairSelection>,
    #[serde(default)]
    nic_policies: Vec<NicPolicySelection>,
    #[serde(default = "default_duration")]
    duration: u64,
    /// TCP socket buffer 档位，逐档各跑一轮（`-w`）。
    #[serde(default)]
    tcp_windows: Vec<String>,
    /// TCP 并发流数档位，逐档各跑一轮（`-P`）。
    #[serde(default)]
    tcp_streams: Vec<u32>,
    /// UDP 单流带宽档位，逐档各跑一轮（`-b`）。
    #[serde(default)]
    udp_bandwidths: Vec<String>,
    /// UDP 报文长度档位（`-l`）。空列表表示不下发 `-l`，用 iperf3 默认。
    #[serde(default)]
    udp_lengths: Vec<String>,
    /// UDP socket buffer 档位（`-w`）。空列表表示不下发 `-w`。
    ///
    /// 和 TCP 的 `-w` 是两个独立的输入：UDP 的 `-w` 挂在每个 udp_profile 上，
    /// TCP 的挂在 `iperf.tcp_windows` 上，共用一个框会让两边互相污染。
    #[serde(default)]
    udp_windows: Vec<String>,
    #[serde(default = "default_streams")]
    udp_streams: u32,
    /// 是否按整条路径的可信上限裁剪 UDP `-b`。
    ///
    /// 界面默认关：控制台上填多少就发多少，超额灌包本来就是要看的场景之一。
    /// 配置文件里的 `limit_udp_by_link_speed` 只作用于命令行路径，不回填到这里，
    /// 否则同一个勾选框在不同机器上含义不同。
    #[serde(default)]
    limit_udp_by_link_speed: bool,
    #[serde(default)]
    screenshot: bool,
}

fn default_duration() -> u64 {
    180
}
fn default_streams() -> u32 {
    1
}

#[derive(Debug, Serialize)]
struct ConnectOut {
    health: HealthOut,
    master: HostInfo,
    agent: HostInfo,
    nic_policies: Vec<NicPolicySelection>,
}

#[derive(Debug, Serialize)]
struct BootstrapOut {
    agent_host: String,
    agent_port: u16,
    token_configured: bool,
    ipv4_prefixes: Vec<String>,
    duration: u64,
    tcp_windows: Vec<String>,
    tcp_streams: Vec<u32>,
    udp_bandwidths: Vec<String>,
    udp_lengths: Vec<String>,
    udp_windows: Vec<String>,
    udp_streams: u32,
    screenshot: bool,
}

#[derive(Debug, Serialize)]
struct PlannedUnit {
    seq: usize,
    title: String,
    est_secs: u64,
}

#[derive(Debug, Serialize)]
struct PlanOut {
    units: Vec<PlannedUnit>,
    est_total_secs: u64,
    notices: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ProgressOut {
    running: bool,
    from: usize,
    lines: Vec<String>,
    report: String,
}

/// 启动控制台，阻塞直到进程结束。
pub fn run(port: u16, config_path: Option<String>) -> i32 {
    let (cfg, _) = load_config(config_path.as_deref());
    let agent_host = cfg.agent_host.clone();
    let console = Arc::new(Console {
        state: Mutex::new(UiState {
            cfg,
            agent_host,
            ..Default::default()
        }),
        running: AtomicBool::new(false),
        report: Mutex::new(String::new()),
    });

    // 只监听回环：控制台能改配置并启动测试，暴露到网络上等于把这台机器的
    // 测试控制权交出去。需要远程操作应当用 SSH 转发，而不是放开监听地址。
    let addr = format!("127.0.0.1:{port}");
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("!! 控制台无法监听 {addr}: {e}");
            eprintln!("!! 端口可能被占用，换一个：cpe_test ui --port 28900");
            return 2;
        }
    };
    let url = format!("http://{addr}");
    println!("控制台已启动: {url}");
    println!("（浏览器没自动弹出的话，手动复制上面这个地址打开）");
    crate::console::open_url(&url);

    for request in server.incoming_requests() {
        let console = Arc::clone(&console);
        handle(request, &console);
    }
    0
}

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

/// 自定义头会让跨站 fetch 先触发 CORS 预检；本服务不开放 CORS，因此网页不能
/// 趁用户开着本地控制台时从别的站点静默发起测试。原生程序仍可显式带头调用。
fn has_console_request_header(request: &Request) -> bool {
    request
        .headers()
        .iter()
        .any(|h| h.field.equiv("X-CPE-Console") && h.value.as_str() == "1")
}

fn handle(mut request: Request, console: &Arc<Console>) {
    let path = request.url().split('?').next().unwrap_or("/").to_string();
    let query = request
        .url()
        .split_once('?')
        .map(|(_, q)| q.to_string())
        .unwrap_or_default();
    let trusted_post = *request.method() != Method::Post || has_console_request_header(&request);
    let mut body = String::new();
    if *request.method() == Method::Post {
        let mut limited = request.as_reader().take(MAX_BODY_BYTES);
        let _ = limited.read_to_string(&mut body);
    }

    if path == "/" || path == "/index.html" {
        let _ = request.respond(page_response());
        return;
    }

    let is_post = *request.method() == Method::Post;
    let is_get = *request.method() == Method::Get;
    let out = if !trusted_post {
        Err("拒绝跨站请求：缺少 X-CPE-Console 请求头".to_string())
    } else if is_get && path == "/api/bootstrap" {
        api_bootstrap(console)
    } else if is_post && path == "/api/connect" {
        api_connect(console, &body)
    } else if is_post && path == "/api/plan" {
        api_plan(console, &body)
    } else if is_post && path == "/api/config" {
        api_config(console, &body)
    } else if is_post && path == "/api/run" {
        api_run(console, &body)
    } else if is_post && path == "/api/stop" {
        api_stop(console)
    } else if is_post && path == "/api/open-report" {
        api_open_report(console)
    } else if is_get && path == "/api/progress" {
        Ok(api_progress(console, &query))
    } else {
        Err("未知接口或请求方法".to_string())
    };
    let body = match out {
        Ok(value) => crate::protocol::ok_json(value),
        Err(error) => crate::protocol::err_json(&error),
    };
    let _ = request.respond(json_response(body));
}

#[derive(Debug, Deserialize)]
struct ConnectReq {
    host: String,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    token: String,
    /// 网卡列表的 IPv4 前缀过滤；空列表 = 列出全部网卡。
    ///
    /// 必须能在界面上改：默认只放行 `192.168.`，在 10.x / 172.x 的实验网里
    /// 会把整张网卡表过滤成空，而控制台存在的意义就是让人不必回去手改
    /// config.json。
    #[serde(default)]
    ipv4_prefixes: Option<Vec<String>>,
}

fn api_bootstrap(console: &Arc<Console>) -> Result<serde_json::Value, String> {
    let state = lock_recover(&console.state);
    let mut tcp_streams: Vec<u32> = state
        .cfg
        .tests
        .iter()
        .filter_map(|test| test.tcp_streams)
        .filter(|value| *value > 0)
        .collect();
    tcp_streams.sort_unstable();
    tcp_streams.dedup();
    if tcp_streams.is_empty() {
        tcp_streams.push(10);
    }
    let udp_streams = state
        .cfg
        .tests
        .iter()
        .filter_map(|test| test.udp_streams)
        .find(|value| *value > 0)
        .unwrap_or(1);
    serde_json::to_value(BootstrapOut {
        agent_host: state.agent_host.clone(),
        agent_port: state.cfg.agent_port,
        token_configured: !state.cfg.agent_token.is_empty(),
        ipv4_prefixes: state.cfg.ipv4_prefixes.clone(),
        duration: state.cfg.iperf.duration,
        tcp_windows: state.cfg.iperf.tcp_windows.clone(),
        tcp_streams,
        udp_bandwidths: state
            .cfg
            .iperf
            .udp_profiles
            .iter()
            .map(|profile| profile.bandwidth.clone())
            .collect(),
        udp_lengths: distinct(
            state
                .cfg
                .iperf
                .udp_profiles
                .iter()
                .filter_map(|profile| profile.length.clone()),
        ),
        udp_windows: distinct(
            state
                .cfg
                .iperf
                .udp_profiles
                .iter()
                .filter_map(|profile| profile.window.clone()),
        ),
        udp_streams,
        screenshot: state.cfg.screenshot,
    })
    .map_err(|error| error.to_string())
}

fn configured_nic_policies(
    cfg: &Config,
    master: &HostInfo,
    agent: &HostInfo,
) -> Vec<NicPolicySelection> {
    let mut policies = Vec::new();
    for (host, info) in [("master", master), ("agent", agent)] {
        for nic in &info.interfaces {
            if let Some(profile) = cfg
                .link_profiles
                .by_nic
                .iter()
                .find(|profile| crate::rate::nic_profile_matches(profile, host, nic))
            {
                policies.push(NicPolicySelection {
                    endpoint: format!("{host}:NAME={}", nic.name),
                    // 回显成用户当初的写法：绝对值回显数字，百分比回显 `90%`。
                    rx_target: match (profile.rx_target_mbps, profile.rx_target_percent) {
                        (Some(mbps), _) => format!("{mbps}"),
                        (None, Some(percent)) => format!("{percent}%"),
                        (None, None) => String::new(),
                    },
                    udp_bandwidth: profile.udp_bandwidth.clone().unwrap_or_default(),
                    udp_length: profile.udp_length.clone().unwrap_or_default(),
                });
            }
        }
    }
    policies
}

fn api_connect(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: ConnectReq = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let mut state = lock_recover(&console.state);
    if !req.host.trim().is_empty() {
        state.agent_host = req.host.trim().to_string();
    }
    if req.port > 0 {
        state.cfg.agent_port = req.port;
    }
    // 页面不回显配置文件里的 token；输入留空时沿用已加载值，手工填写时覆盖。
    if !req.token.is_empty() {
        state.cfg.agent_token = req.token.clone();
    }
    // 前缀框清空是一个有意义的选择（= 列出全部网卡），所以用 Option 区分
    // 「没提交这个字段」和「提交了一个空列表」，不能用 is_empty 兜。
    if let Some(prefixes) = &req.ipv4_prefixes {
        state.cfg.ipv4_prefixes = cleaned_list(prefixes);
    }
    if state.agent_host.is_empty() {
        return Err("请先填辅测机 IP（辅测机 agent 窗口里显示的那个地址）".into());
    }

    let health: HealthOut = post(
        &state.agent_host,
        state.cfg.agent_port,
        "/health",
        "{}",
        &state.cfg.agent_token,
    )
    .map_err(|e| {
        format!(
            "辅测机 {}:{} 连不上。请确认对方已双击 start_agent.bat，且 {} 端口在防火墙放行（{e}）",
            state.agent_host, state.cfg.agent_port, state.cfg.agent_port
        )
    })?;
    let info_body = serde_json::to_string(&InfoReq {
        ipv4_prefixes: state.cfg.ipv4_prefixes.clone(),
    })
    .unwrap_or_else(|_| "{}".into());
    let agent: HostInfo = post(
        &state.agent_host,
        state.cfg.agent_port,
        "/info",
        &info_body,
        &state.cfg.agent_token,
    )
    .map_err(|e| format!("已连上辅测机，但获取网卡失败: {e}"))?;

    let master = crate::nic::scan_host(&state.cfg.ipv4_prefixes);
    let nic_policies = configured_nic_policies(&state.cfg, &master, &agent);
    state.master = master.clone();
    state.agent = agent.clone();
    serde_json::to_value(ConnectOut {
        health,
        master,
        agent,
        nic_policies,
    })
    .map_err(|e| e.to_string())
}

fn post<T: serde::de::DeserializeOwned>(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    token: &str,
) -> Result<T, String> {
    let (status, text) =
        http_client::post_json_auth(host, port, path, body, token, Duration::from_secs(60))?;
    if status == 401 {
        return Err("agent 返回 401：已启用令牌认证，请填写相同的 token".into());
    }
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    let resp: Resp<T> = serde_json::from_str(&text).map_err(|e| format!("响应解析失败: {e}"))?;
    if !resp.ok {
        return Err(resp.error.unwrap_or_else(|| "未知错误".into()));
    }
    resp.data.ok_or_else(|| "响应缺 data".into())
}

fn endpoint_exists(state: &UiState, endpoint: &str) -> bool {
    let Some((host, selector)) = endpoint.split_once(':') else {
        return false;
    };
    let Some(name) = selector.strip_prefix("NAME=") else {
        return false;
    };
    let interfaces = match host {
        "master" => &state.master.interfaces,
        "agent" => &state.agent.interfaces,
        _ => return false,
    };
    interfaces.iter().any(|nic| nic.name == name)
}

fn values_are_allowed(values: &[String], allowed: &[&str]) -> bool {
    !values.is_empty()
        && values
            .iter()
            .all(|value| allowed.iter().any(|candidate| value == candidate))
}

/// 浏览器控件不是信任边界：即使页面会过滤，后端仍需拒绝空选择、越界数值和
/// 无效档位。尤其不能把“用户把整列取消勾选”静默解释成默认 AB/TCP/IPv4。
fn validate_request(state: &UiState, req: &RunRequest) -> Result<(), String> {
    if req.pairs.is_empty() {
        return Err("一个测试项都没勾".into());
    }
    if !(1..=86_400).contains(&req.duration) {
        return Err("时长必须在 1..=86400 秒之间".into());
    }
    if req
        .tcp_streams
        .iter()
        .any(|value| !(1..=32).contains(value))
    {
        return Err("TCP -P 每一档都必须在 1..=32 之间".into());
    }
    if !(1..=32).contains(&req.udp_streams) {
        return Err("UDP 流数必须在 1..=32 之间".into());
    }
    for window in req
        .tcp_windows
        .iter()
        .filter(|value| !value.trim().is_empty())
    {
        crate::cmd::ctstraffic::parse_size_bytes(window.trim())
            .map_err(|error| format!("TCP -w 档位 {window:?} 无效：{error}"))?;
    }
    for bandwidth in req
        .udp_bandwidths
        .iter()
        .filter(|value| !value.trim().is_empty())
    {
        UdpProfile::bw(bandwidth.trim())
            .parsed_bandwidth()
            .map_err(|error| format!("UDP -b 档位 {bandwidth:?} 无效：{error}"))?;
    }
    for window in req.udp_windows.iter().filter(|v| !v.trim().is_empty()) {
        crate::cmd::ctstraffic::parse_size_bytes(window.trim())
            .map_err(|error| format!("UDP -w 档位 {window:?} 无效：{error}"))?;
    }
    for length in req.udp_lengths.iter().filter(|v| !v.trim().is_empty()) {
        // iperf3 的 -l 收字节数，也收 k/m 后缀；和下发命令用同一个解析器，
        // 免得界面放行的写法到了命令行上才炸。
        let bytes = crate::cmd::ctstraffic::parse_size_bytes(length.trim())
            .map_err(|error| format!("UDP -l 档位 {length:?} 无效：{error}"))?;
        if bytes > 65_507 {
            return Err(format!(
                "UDP -l 档位 {length:?} 超过单个 UDP 报文上限 65507 字节"
            ));
        }
    }

    for pair in &req.pairs {
        if pair.src == pair.dst
            || !endpoint_exists(state, &pair.src)
            || !endpoint_exists(state, &pair.dst)
        {
            return Err(format!(
                "测试配对已失效：{} -> {}。请刷新网口后重新选择",
                pair.src, pair.dst
            ));
        }
        if !values_are_allowed(&pair.directions, &["ab", "ba", "bidir"]) {
            return Err(format!(
                "配对 {} / {} 至少勾一个有效方向",
                pair.src, pair.dst
            ));
        }
        if !values_are_allowed(&pair.transports, &["tcp", "udp"]) {
            return Err(format!(
                "配对 {} / {} 至少勾 TCP 或 UDP",
                pair.src, pair.dst
            ));
        }
        if !values_are_allowed(&pair.ip, &["v4", "v6"]) {
            return Err(format!(
                "配对 {} / {} 至少勾 IPv4 或 IPv6",
                pair.src, pair.dst
            ));
        }
    }

    let mut seen = HashSet::new();
    for policy in &req.nic_policies {
        if !endpoint_exists(state, &policy.endpoint) {
            return Err(format!(
                "网口策略已失效：{}。请刷新网口后重新填写",
                policy.endpoint
            ));
        }
        if !seen.insert(policy.endpoint.as_str()) {
            return Err(format!("网口策略重复：{}", policy.endpoint));
        }
        parse_rx_target(&policy.rx_target)
            .map_err(|error| format!("{} 的 RX 门限：{error}", policy.endpoint))?;
        if !policy.udp_length.trim().is_empty() {
            let bytes = crate::cmd::ctstraffic::parse_size_bytes(policy.udp_length.trim())
                .map_err(|error| format!("{} 的 UDP -l 无效：{error}", policy.endpoint))?;
            if bytes > 65_507 {
                return Err(format!(
                    "{} 的 UDP -l 超过单个 UDP 报文上限 65507 字节",
                    policy.endpoint
                ));
            }
        }
        if !policy.udp_bandwidth.trim().is_empty() {
            UdpProfile::bw(policy.udp_bandwidth.trim())
                .parsed_bandwidth()
                .map_err(|error| format!("{} 的 UDP -b 无效：{error}", policy.endpoint))?;
        }
    }
    Ok(())
}

fn validated_config_from_request(state: &UiState, req: &RunRequest) -> Result<Config, String> {
    validate_request(state, req)?;
    let cfg = config_from_request(state, req);
    let problems = cfg.validate();
    if problems.is_empty() {
        Ok(cfg)
    } else {
        Err(format!("配置项异常：{}", problems.join("；")))
    }
}

/// 把界面状态翻译成一份 config。规划和执行都走这一个函数，
/// 保证「预计耗时」和真正跑的是同一份东西。
fn config_from_request(state: &UiState, req: &RunRequest) -> Config {
    let mut cfg = state.cfg.clone();
    cfg.agent_host = state.agent_host.clone();
    cfg.screenshot = req.screenshot;
    cfg.limit_udp_by_link_speed = req.limit_udp_by_link_speed;
    cfg.iperf.duration = req.duration.clamp(1, 86_400);
    cfg.pairs = None;
    cfg.universal_params = None;
    cfg.link_profiles.by_nic.clear();

    let windows = non_empty(&req.tcp_windows, &cfg.iperf.tcp_windows);
    let stream_steps: Vec<u32> = {
        let picked: Vec<u32> = req.tcp_streams.iter().copied().filter(|n| *n > 0).collect();
        if picked.is_empty() {
            vec![1]
        } else {
            picked
        }
    };
    let lengths = cleaned_list(&req.udp_lengths);
    let udp_windows = cleaned_list(&req.udp_windows);
    let global_udp: Vec<UdpProfile> = req
        .udp_bandwidths
        .iter()
        .filter(|b| !b.trim().is_empty())
        .flat_map(|b| udp_profiles_for(b.trim(), &lengths, &udp_windows))
        .collect();
    // 全局档位保留一份，供没有逐对覆盖时使用；builder 会在这一层之上再做
    // 路径上限裁剪。
    if !global_udp.is_empty() {
        cfg.iperf.udp_profiles = global_udp.clone();
    }
    cfg.iperf.tcp_windows = windows.clone();

    let explicit_udp_senders: HashSet<String> = req
        .nic_policies
        .iter()
        .filter(|policy| !policy.udp_bandwidth.trim().is_empty())
        .map(|policy| policy.endpoint.clone())
        .collect();
    for policy in &req.nic_policies {
        if let Some(profile) = nic_profile(policy) {
            cfg.link_profiles.by_nic.push(profile);
        }
    }

    let mut tests: Vec<TestSpec> = Vec::new();
    for (idx, pair) in req.pairs.iter().enumerate() {
        let directions = pair.directions.clone();
        let ip = pair.ip.clone();
        let wants = |t: &str| pair.transports.iter().any(|x| x == t);
        let (want_tcp, want_udp) = (wants("tcp"), wants("udp"));

        let base = |name: String, transports: Vec<String>| TestSpec {
            name,
            src: pair.src.clone(),
            dst: pair.dst.clone(),
            direction: OneOrMany::Many(directions.clone()),
            kinds: vec!["iperf".into()],
            transports,
            ip: ip.clone(),
            streams: 1,
            tcp_streams: None,
            udp_streams: Some(req.udp_streams.max(1)),
            iperf_duration: Some(req.duration.clamp(1, 86_400)),
            ping_count: None,
            ping_payload_sizes: None,
            tcp_windows: None,
            udp_profiles: None,
            rate_mode: None,
            rate_targets_mbps: None,
        };

        // TCP 每个 -P 档位独立成一份 TestSpec：`tcp_streams` 在配置模型里是标量，
        // 而 -w 本来就是数组，由 builder 自己展开。TCP/UDP 也必须拆开，否则
        // 「3 个 -P 档位」会把与 -P 无关的 UDP 单元复制三遍。
        if want_tcp {
            for streams in &stream_steps {
                let mut spec = base(format!("ui-{}-tcp-P{streams}", idx + 1), vec!["tcp".into()]);
                spec.tcp_streams = Some(*streams);
                spec.tcp_windows = Some(windows.clone());
                tests.push(spec);
            }
        }
        if want_udp {
            let src_pinned = explicit_udp_senders.contains(&pair.src);
            let dst_pinned = explicit_udp_senders.contains(&pair.dst);
            // 一个方向的每条发送腿都有按网口覆盖时，全局 -b 档位对它不起作用：
            // builder 会把每一档都替换回那个覆盖值，扫 N 档就得到 N 个完全相同
            // 的单元。必须**逐方向**判断而不是整对判断——「ab 被发送端钉死、
            // 反向 ba 仍要扫档位」是最常见的组合，按整对判断时那三个 ab 单元
            // 会一模一样地各跑一遍全程。
            let pinned_direction = |d: &String| match d.as_str() {
                "ab" => src_pinned,
                "ba" => dst_pinned,
                "bidir" => src_pinned && dst_pinned,
                _ => false,
            };
            let (pinned, swept): (Vec<String>, Vec<String>) =
                directions.iter().cloned().partition(pinned_direction);

            if !pinned.is_empty() {
                // 占位值：builder 会按腿替换成各自的精确覆盖值，这里填什么都行，
                // 取一个真实值只是为了万一覆盖项被后续校验剔除时不至于离谱。
                let placeholder = req
                    .nic_policies
                    .iter()
                    .find(|policy| {
                        (policy.endpoint == pair.src || policy.endpoint == pair.dst)
                            && !policy.udp_bandwidth.trim().is_empty()
                    })
                    .map(|policy| policy.udp_bandwidth.trim())
                    .unwrap_or("1m");
                let mut spec = base(format!("ui-{}-udp-pinned", idx + 1), vec!["udp".into()]);
                spec.direction = OneOrMany::Many(pinned);
                // -b 被网口钉死，但 -l 档位仍要逐档跑：钉住的是带宽，不是报文长度。
                spec.udp_profiles = Some(udp_profiles_for(placeholder, &lengths, &udp_windows));
                tests.push(spec);
            }
            if !swept.is_empty() {
                // 还有腿没被覆盖的方向照常逐档扫描；已覆盖的那条腿在每个单元里
                // 保持固定值（双向单元里一钉一扫就是这种情况）。
                let mut spec = base(format!("ui-{}-udp", idx + 1), vec!["udp".into()]);
                spec.direction = OneOrMany::Many(swept);
                spec.udp_profiles = Some(if global_udp.is_empty() {
                    cfg.iperf.udp_profiles.clone()
                } else {
                    global_udp.clone()
                });
                tests.push(spec);
            }
        }
    }
    cfg.tests = tests;
    cfg
}

/// 去空白、丢空项。手抄进来的参数列表和网段前缀共用这一份清洗。
///
/// 只清洗，不替换成默认值：清洗后剩下空列表在两处都是有意义的选择
/// （前缀清空 = 列出全部网口，`-l` 清空 = 不下发 `-l`）。
fn cleaned_list(raw: &[String]) -> Vec<String> {
    raw.iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// 一个 `-b` 档位 × 全部 `-l` 档位 × 全部 `-w` 档位。
///
/// 某一项留空就在那一维退化成一档、且**完全不下发该参数**——不能拿 iperf3 的
/// 默认值写死进命令，那会把「没指定」变成「指定了某个具体值」，两者在报告里
/// 读起来完全不同。
fn udp_profiles_for(bandwidth: &str, lengths: &[String], windows: &[String]) -> Vec<UdpProfile> {
    let one_none = [None];
    let lengths: Vec<Option<String>> = if lengths.is_empty() {
        one_none.to_vec()
    } else {
        lengths.iter().cloned().map(Some).collect()
    };
    let windows: Vec<Option<String>> = if windows.is_empty() {
        one_none.to_vec()
    } else {
        windows.iter().cloned().map(Some).collect()
    };
    let mut out = Vec::with_capacity(lengths.len() * windows.len());
    for length in &lengths {
        for window in &windows {
            out.push(UdpProfile {
                bandwidth: bandwidth.to_string(),
                length: length.clone(),
                window: window.clone(),
            });
        }
    }
    out
}

/// 保序去重。配置文件里同一个 `-l` / `-w` 常在多个档位上重复出现，
/// 回填到界面时得压成一份，否则一打开页面档位就自己翻倍。
fn distinct(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values.filter(|value| seen.insert(value.clone())).collect()
}

/// 界面没填就退回配置文件里的既有值，不要用空列表把它清掉。
fn non_empty(picked: &[String], fallback: &[String]) -> Vec<String> {
    let cleaned: Vec<String> = picked
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    if cleaned.is_empty() {
        fallback.to_vec()
    } else {
        cleaned
    }
}

/// `master:NAME=以太网 6` -> 一条 by_nic 覆盖。三项全空就不生成覆盖项。
fn nic_profile(policy: &NicPolicySelection) -> Option<crate::config::NicProfile> {
    let target = parse_rx_target(&policy.rx_target).ok().flatten();
    let bandwidth = policy.udp_bandwidth.trim();
    let length = policy.udp_length.trim();
    if target.is_none() && bandwidth.is_empty() && length.is_empty() {
        return None;
    }
    let (host, rest) = policy.endpoint.split_once(':')?;
    let name = rest.strip_prefix("NAME=")?;
    Some(crate::config::NicProfile {
        host: host.to_string(),
        name: name.to_string(),
        ipv4: String::new(),
        rx_target_mbps: match target {
            Some(RxTarget::Mbps(value)) => Some(value),
            _ => None,
        },
        rx_target_percent: match target {
            Some(RxTarget::Percent(value)) => Some(value),
            _ => None,
        },
        udp_bandwidth: (!bandwidth.is_empty()).then(|| bandwidth.to_string()),
        udp_length: (!length.is_empty()).then(|| length.to_string()),
    })
}

fn api_plan(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: RunRequest = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let state = lock_recover(&console.state);
    if state.master.interfaces.is_empty() || state.agent.interfaces.is_empty() {
        return Err("还没连上辅测机，先点「连接」".into());
    }
    let cfg = validated_config_from_request(&state, &req)?;
    let mut specs = Vec::new();
    let mut notices = Vec::new();
    for test in &cfg.tests {
        match builder::spec_from_config(test, &cfg, &state.master, &state.agent) {
            Ok(spec) => specs.push(spec),
            Err(e) => notices.push(format!("跳过 {}: {e}", test.name)),
        }
    }
    let mut port = builder::PORT_BASE;
    let (units, build_notices) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
    notices.extend(build_notices);
    let est_total_secs = units.iter().map(|u| u.est_secs).sum();
    let units = units
        .iter()
        .enumerate()
        .map(|(idx, unit)| PlannedUnit {
            seq: idx + 1,
            title: unit.title.clone(),
            est_secs: unit.est_secs,
        })
        .collect();
    serde_json::to_value(PlanOut {
        units,
        est_total_secs,
        notices,
    })
    .map_err(|e| e.to_string())
}

fn api_config(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: RunRequest = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let state = lock_recover(&console.state);
    if state.master.interfaces.is_empty() || state.agent.interfaces.is_empty() {
        return Err("还没连上辅测机，先点「连接」".into());
    }
    let cfg = validated_config_from_request(&state, &req)?;
    serde_json::to_value(cfg).map_err(|error| format!("生成配置失败: {error}"))
}

fn ensure_config_builds_units(cfg: &Config, state: &UiState) -> Result<(), String> {
    let mut specs = Vec::new();
    for test in &cfg.tests {
        let spec = builder::spec_from_config(test, cfg, &state.master, &state.agent)
            .map_err(|error| format!("{} 无法生成任务：{error}", test.name))?;
        specs.push(spec);
    }

    let mut port = builder::PORT_BASE;
    let (units, notices) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
    if units.is_empty() {
        let detail = if notices.is_empty() {
            String::new()
        } else {
            format!("：{}", notices.join("；"))
        };
        return Err(format!("所选配置最终没有生成任何测试单元{detail}"));
    }
    Ok(())
}

fn api_run(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: RunRequest = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    if console.running.swap(true, Ordering::SeqCst) {
        return Err("已经有一轮测试在跑了".into());
    }
    let cfg = {
        let state = lock_recover(&console.state);
        if state.master.interfaces.is_empty() || state.agent.interfaces.is_empty() {
            console.running.store(false, Ordering::SeqCst);
            return Err("还没连上辅测机，先点「连接」".into());
        }
        match validated_config_from_request(&state, &req) {
            Ok(cfg) => {
                // “开始测试”不允许把无效选择或最终为空的计划静默跳过。
                if let Err(error) = ensure_config_builds_units(&cfg, &state) {
                    console.running.store(false, Ordering::SeqCst);
                    return Err(error);
                }
                cfg
            }
            Err(error) => {
                console.running.store(false, Ordering::SeqCst);
                return Err(error);
            }
        }
    };
    if cfg.tests.is_empty() {
        console.running.store(false, Ordering::SeqCst);
        return Err("一个测试项都没勾".into());
    }

    // 界面状态先落成一份真实的临时 config，作为 run_master 的统一入口；
    // 需要长期保留的副本由 /api/config 下载，工作线程结束后会删除这里的文件。
    let path = std::env::temp_dir().join(format!("cpe_test_ui_{}.json", std::process::id()));
    let json = match serde_json::to_string_pretty(&cfg) {
        Ok(json) => json,
        Err(error) => {
            console.running.store(false, Ordering::SeqCst);
            return Err(format!("生成临时配置失败: {error}"));
        }
    };
    if let Err(error) = std::fs::write(&path, json) {
        console.running.store(false, Ordering::SeqCst);
        return Err(format!("写临时配置失败: {error}"));
    }

    clear_log_mirror();
    lock_recover(&console.report).clear();
    crate::cancel::reset();
    let worker_console = Arc::clone(console);
    let cleanup_path = path.clone();
    let config_path = path.to_string_lossy().to_string();
    let worker = std::thread::Builder::new()
        .name("cpe-test-webui-run".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_master(MasterOpts {
                    config_path: Some(config_path),
                    auto: true,
                    no_open: true,
                    ..Default::default()
                })
            }));
            match result {
                Ok(0) => {}
                Ok(code) => crate::util::logln(&format!("!! 测试流程以状态码 {code} 结束")),
                Err(_) => crate::util::logln("!! 测试主线程异常退出；已保留现有日志和部分结果"),
            }
            let _ = std::fs::remove_file(path);
            worker_console.running.store(false, Ordering::SeqCst);
        });
    if let Err(error) = worker {
        let _ = std::fs::remove_file(cleanup_path);
        console.running.store(false, Ordering::SeqCst);
        return Err(format!("无法启动测试线程: {error}"));
    }
    Ok(serde_json::json!({ "started": true }))
}

fn api_stop(console: &Arc<Console>) -> Result<serde_json::Value, String> {
    if !console.running.load(Ordering::SeqCst) {
        return Err("当前没有正在运行的测试".into());
    }
    crate::cancel::request_cancel();
    Ok(serde_json::json!({ "stopping": true }))
}

fn api_open_report(console: &Arc<Console>) -> Result<serde_json::Value, String> {
    let report = lock_recover(&console.report).clone();
    if report.is_empty() {
        return Err("报告尚未生成".into());
    }
    let path = Path::new(&report);
    if !path.is_file() {
        return Err(format!("报告文件不存在：{}", path.display()));
    }
    crate::console::open_path(path);
    Ok(serde_json::json!({ "opened": true }))
}

fn api_progress(console: &Arc<Console>, query: &str) -> serde_json::Value {
    let from = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("from="))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let (total, lines) = log_tail_since(from);
    // 报告路径从日志里捞：run_master 自己决定运行目录名，界面在点下
    // 「开始测试」的那一刻还不知道它叫什么。
    {
        let mut report = lock_recover(&console.report);
        if report.is_empty() {
            if let Some(found) = lines
                .iter()
                .find_map(|line| line.split_once("报告已生成: ").map(|(_, p)| p.trim()))
            {
                *report = found.to_string();
            }
        }
    }
    serde_json::to_value(ProgressOut {
        running: console.running.load(Ordering::SeqCst),
        from: total,
        lines,
        report: lock_recover(&console.report).clone(),
    })
    .unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::NicInfo;

    fn state_with_pair() -> UiState {
        let nic = |name: &str, role: &str, ip: &str| NicInfo {
            name: name.into(),
            role: role.into(),
            ipv4: ip.into(),
            speed_mbps: 2500,
            ..Default::default()
        };
        UiState {
            cfg: Config::default(),
            agent_host: "10.0.0.2".into(),
            master: HostInfo {
                hostname: "m".into(),
                os: "test".into(),
                interfaces: vec![nic("以太网 6", "SGMII2.5G", "192.168.0.101")],
            },
            agent: HostInfo {
                hostname: "a".into(),
                os: "test".into(),
                interfaces: vec![nic("WLAN 3", "WIFI5G", "192.168.0.104")],
            },
        }
    }

    fn request() -> RunRequest {
        RunRequest {
            pairs: vec![PairSelection {
                src: "master:NAME=以太网 6".into(),
                dst: "agent:NAME=WLAN 3".into(),
                directions: vec!["ab".into(), "bidir".into()],
                transports: vec!["tcp".into(), "udp".into()],
                ip: vec!["v4".into()],
            }],
            nic_policies: vec![
                NicPolicySelection {
                    endpoint: "master:NAME=以太网 6".into(),
                    rx_target: "1800".into(),
                    udp_bandwidth: "2.6G".into(),
                    udp_length: String::new(),
                },
                NicPolicySelection {
                    endpoint: "agent:NAME=WLAN 3".into(),
                    rx_target: "1600".into(),
                    udp_bandwidth: "2.8G".into(),
                    udp_length: String::new(),
                },
            ],
            duration: 60,
            tcp_windows: vec!["2m".into(), "4m".into(), "256m".into()],
            tcp_streams: vec![1, 5, 10],
            udp_bandwidths: vec!["1m".into(), "500m".into(), "1G".into()],
            udp_lengths: Vec::new(),
            udp_windows: Vec::new(),
            udp_streams: 1,
            limit_udp_by_link_speed: false,
            screenshot: false,
        }
    }

    /// 界面上填的门限/带宽必须真的变成 link_profiles，否则勾了等于没勾。
    #[test]
    fn ui_selection_becomes_a_real_config() {
        let cfg = config_from_request(&state_with_pair(), &request());
        assert_eq!(cfg.iperf.tcp_windows, vec!["2m", "4m", "256m"]);

        // 发送端网卡带的是它作为发送端时的带宽；接收端网卡带的是对向门限。
        let master_nic = cfg
            .link_profiles
            .by_nic
            .iter()
            .find(|p| p.name == "以太网 6")
            .expect("主控网卡应有覆盖项");
        assert_eq!(master_nic.host, "master");
        assert_eq!(master_nic.udp_bandwidth.as_deref(), Some("2.6G"));
        assert_eq!(master_nic.rx_target_mbps, Some(1800.0));

        let agent_nic = cfg
            .link_profiles
            .by_nic
            .iter()
            .find(|p| p.name == "WLAN 3")
            .expect("辅测网卡应有覆盖项");
        assert_eq!(agent_nic.udp_bandwidth.as_deref(), Some("2.8G"));
        assert_eq!(agent_nic.rx_target_mbps, Some(1600.0));
    }

    /// `-P` 在配置模型里是标量，多档位只能在界面层展开成多份 TestSpec；
    /// TCP / UDP 必须拆开，否则「3 个 -P 档位」会把与 -P 无关的 UDP 单元复制三遍。
    #[test]
    fn stream_steps_expand_into_separate_specs_without_duplicating_udp() {
        let cfg = config_from_request(&state_with_pair(), &request());
        let tcp: Vec<&TestSpec> = cfg
            .tests
            .iter()
            .filter(|t| t.transports.contains(&"tcp".to_string()))
            .collect();
        let udp: Vec<&TestSpec> = cfg
            .tests
            .iter()
            .filter(|t| t.transports.contains(&"udp".to_string()))
            .collect();

        assert_eq!(tcp.len(), 3, "三个 -P 档位各一份");
        let mut steps: Vec<u32> = tcp.iter().filter_map(|t| t.tcp_streams).collect();
        steps.sort_unstable();
        assert_eq!(steps, vec![1, 5, 10]);
        for spec in &tcp {
            // -w 本来就是数组，交给 builder 展开，不在这里乘一遍。
            assert_eq!(
                spec.tcp_windows.as_deref(),
                Some(["2m".to_string(), "4m".to_string(), "256m".to_string()].as_slice())
            );
        }
        assert_eq!(udp.len(), 1, "UDP 不该被 -P 档位复制");
    }

    /// 某对填了 -b 覆盖，就只按它跑一档，不再参与全局档位扫描——
    /// 否则「档位 1m/500m/1G」×「覆盖 1G」会跑出三个一模一样的单元。
    #[test]
    fn explicit_bandwidth_on_every_sending_nic_opts_out_of_the_global_sweep() {
        let with_override = config_from_request(&state_with_pair(), &request());
        let udp = with_override
            .tests
            .iter()
            .find(|t| t.transports.contains(&"udp".to_string()))
            .expect("应有 UDP spec");
        assert_eq!(udp.udp_profiles.as_ref().map(|v| v.len()), Some(1));

        let mut req = request();
        for policy in &mut req.nic_policies {
            policy.udp_bandwidth.clear();
        }
        let swept = config_from_request(&state_with_pair(), &req);
        let udp = swept
            .tests
            .iter()
            .find(|t| t.transports.contains(&"udp".to_string()))
            .expect("应有 UDP spec");
        assert_eq!(
            udp.udp_profiles.as_ref().map(|v| v.len()),
            Some(3),
            "没有覆盖时按全局三个档位扫描"
        );
    }

    /// 一边按网口固定、另一边留空时，留空腿仍需扫描全部全局档位；
    /// 而被固定的那个方向不能跟着扫。
    ///
    /// 这两件事必须**逐方向**判断。按整对判断时，只要有一条腿没被覆盖就整对
    /// 去扫档位，于是「ab 被发送端钉死」的那个方向会被复制成 N 个一模一样的
    /// 单元——3 档 × 180s 就是 6 分钟白跑，报告里还多出两行看着像 bug 的重复项。
    #[test]
    fn a_one_sided_bandwidth_override_sweeps_only_the_unpinned_direction() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].directions = vec!["ab".into(), "ba".into()];
        req.pairs[0].transports = vec!["udp".into()];
        // 发送端 master 钉死在 2.6G，反向发送端 agent 留空。
        req.nic_policies[1].udp_bandwidth.clear();

        let cfg = config_from_request(&state, &req);
        let pinned = cfg
            .tests
            .iter()
            .find(|test| test.direction.directions() == ["ab"])
            .expect("被钉死的 ab 方向应单独成一份 spec");
        assert_eq!(
            pinned.udp_profiles.as_ref().map(Vec::len),
            Some(1),
            "ab 的发送腿已被覆盖，扫档位只会生成重复单元"
        );
        let swept = cfg
            .tests
            .iter()
            .find(|test| test.direction.directions() == ["ba"])
            .expect("未覆盖的 ba 方向应保留档位扫描");
        assert_eq!(
            swept.udp_profiles.as_ref().map(Vec::len),
            Some(3),
            "未覆盖的反向发送腿仍要跑 1m/500m/1G 三档"
        );

        // 真正要防的是队列里出现重复单元，所以一路建到 unit 再查。
        let specs: Vec<_> = cfg
            .tests
            .iter()
            .map(|test| {
                builder::spec_from_config(test, &cfg, &state.master, &state.agent).expect("建 spec")
            })
            .collect();
        let mut port = builder::PORT_BASE;
        let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
        let titles: Vec<&str> = units.iter().map(|unit| unit.title.as_str()).collect();
        let unique: HashSet<&str> = titles.iter().copied().collect();
        assert_eq!(
            unique.len(),
            titles.len(),
            "同一条命令不该排进队列两次: {titles:?}"
        );
        assert_eq!(titles.len(), 4, "ab 一个 + ba 三档: {titles:?}");
    }

    /// 没填就不生成覆盖项，避免用一堆空条目盖掉配置文件里原有的策略。
    #[test]
    fn blank_inputs_produce_no_overrides() {
        let mut req = request();
        for policy in &mut req.nic_policies {
            policy.rx_target.clear();
            policy.udp_bandwidth.clear();
            policy.udp_length.clear();
        }
        let cfg = config_from_request(&state_with_pair(), &req);
        assert!(cfg.link_profiles.by_nic.is_empty());
    }

    #[test]
    fn an_empty_checkbox_group_is_rejected_instead_of_silently_defaulting() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].directions.clear();
        assert!(validate_request(&state, &req)
            .unwrap_err()
            .contains("至少勾一个有效方向"));

        let mut req = request();
        req.pairs[0].transports.clear();
        assert!(validate_request(&state, &req)
            .unwrap_err()
            .contains("至少勾 TCP 或 UDP"));
    }

    #[test]
    fn invalid_sweep_values_are_rejected_before_starting_a_run() {
        let state = state_with_pair();
        let mut req = request();
        req.tcp_streams = vec![0, 33];
        assert!(validate_request(&state, &req)
            .unwrap_err()
            .contains("TCP -P"));

        let mut req = request();
        req.udp_bandwidths = vec!["500m-junk".into()];
        assert!(validate_request(&state, &req)
            .unwrap_err()
            .contains("UDP -b"));
    }

    #[test]
    fn bootstrap_reports_token_presence_without_exposing_the_secret() {
        let mut state = state_with_pair();
        state.cfg.agent_token = "do-not-send-to-the-page".into();
        let console = Arc::new(Console {
            state: Mutex::new(state),
            running: AtomicBool::new(false),
            report: Mutex::new(String::new()),
        });
        let value = api_bootstrap(&console).expect("bootstrap");
        assert_eq!(value["token_configured"], true);
        assert!(value.get("agent_token").is_none());
        assert!(!value.to_string().contains("do-not-send-to-the-page"));
    }

    /// 界面留空不能把配置文件里的既有档位清成空列表。
    #[test]
    fn empty_lists_fall_back_to_the_configured_values() {
        let mut req = request();
        req.tcp_windows.clear();
        req.tcp_streams.clear();
        req.udp_bandwidths.clear();
        let state = state_with_pair();
        let cfg = config_from_request(&state, &req);
        assert_eq!(cfg.iperf.tcp_windows, state.cfg.iperf.tcp_windows);
        let tcp: Vec<&TestSpec> = cfg
            .tests
            .iter()
            .filter(|t| t.transports.contains(&"tcp".to_string()))
            .collect();
        assert_eq!(tcp.len(), 1, "没填 -P 时按单档跑");
        assert_eq!(tcp[0].tcp_streams, Some(1));
    }

    /// 界面产出的 config 必须能被 builder 直接消化——控制台不是第二条
    /// 执行路径，它只是 config 的图形编辑器。
    #[test]
    fn the_generated_config_builds_real_units() {
        let state = state_with_pair();
        let cfg = config_from_request(&state, &request());
        let spec = builder::spec_from_config(&cfg.tests[0], &cfg, &state.master, &state.agent)
            .expect("界面生成的 TestSpec 必须可解析");
        let mut port = builder::PORT_BASE;
        let (units, _) = build_units(&[spec], cfg.require_same_subnet_for_iperf, &mut port);
        assert!(!units.is_empty(), "应生成任务");
        assert!(units.iter().any(|u| u.bidir), "勾了双向就该有双向单元");
    }

    #[test]
    fn a_selection_that_builds_zero_units_is_rejected_before_run() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].ip = vec!["v6".into()];
        let cfg = validated_config_from_request(&state, &req).expect("请求字段本身有效");
        let error = ensure_config_builds_units(&cfg, &state).unwrap_err();
        assert!(error.contains("没有生成任何测试单元"));
        assert!(error.contains("缺少可用的 IPv6 地址"));
    }

    /// 网段前缀必须能在界面上改。默认只放行 `192.168.`，在 10.x / 172.x 的
    /// 实验网里会把整张网卡表过滤成空——而控制台存在的意义正是让人不必回去
    /// 手改 config.json。清空 = 列出全部网口，也必须是一个能表达的选择。
    #[test]
    fn the_console_can_change_which_subnets_show_up() {
        let parse = |body: &str| serde_json::from_str::<ConnectReq>(body).expect("解析连接参数");

        let req = parse(r#"{"host":"10.0.0.2","ipv4_prefixes":[" 10.228. ","172.16.",""]}"#);
        assert_eq!(
            cleaned_list(req.ipv4_prefixes.as_deref().expect("提交了前缀")),
            vec!["10.228.", "172.16."],
            "手抄进来的空白和空项要清掉"
        );

        // 提交空列表（用户把框清空）和根本没提交这个字段，是两件事。
        let emptied = parse(r#"{"host":"10.0.0.2","ipv4_prefixes":[]}"#).ipv4_prefixes;
        assert_eq!(
            emptied.as_deref().map(cleaned_list),
            Some(Vec::new()),
            "清空 = 显式要求列出全部网口"
        );
        assert_eq!(
            parse(r#"{"host":"10.0.0.2"}"#).ipv4_prefixes,
            None,
            "没提交就沿用已加载的配置，不能被当成清空"
        );
    }

    /// 界面上填的网段前缀要一路带进真正下发的 config，否则改了等于没改。
    #[test]
    fn the_chosen_subnets_reach_the_config_that_actually_runs() {
        let mut state = state_with_pair();
        state.cfg.ipv4_prefixes = vec!["10.228.".into()];
        let cfg = config_from_request(&state, &request());
        assert_eq!(cfg.ipv4_prefixes, vec!["10.228."]);
    }

    /// `-l` 档位要和 `-b` 取组合，并且真的变成命令行上的 `-l`。
    #[test]
    fn udp_datagram_size_steps_cross_with_bandwidth_steps() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].transports = vec!["udp".into()];
        req.pairs[0].directions = vec!["ab".into()];
        req.nic_policies
            .iter_mut()
            .for_each(|p| p.udp_bandwidth.clear());
        req.udp_bandwidths = vec!["100m".into(), "500m".into()];
        req.udp_lengths = vec!["64".into(), "1400".into()];

        let cfg = config_from_request(&state, &req);
        let udp = cfg
            .tests
            .iter()
            .find(|t| t.transports.contains(&"udp".to_string()))
            .expect("应有 UDP spec");
        let profiles = udp.udp_profiles.as_ref().expect("应有档位");
        let mut combos: Vec<(String, Option<String>)> = profiles
            .iter()
            .map(|p| (p.bandwidth.clone(), p.length.clone()))
            .collect();
        combos.sort();
        assert_eq!(
            combos,
            vec![
                ("100m".to_string(), Some("64".to_string())),
                ("100m".to_string(), Some("1400".to_string())),
                ("500m".to_string(), Some("64".to_string())),
                ("500m".to_string(), Some("1400".to_string())),
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
            "2 个 -b × 2 个 -l = 4 档"
        );

        // 一路建到真实命令，确认 -l 没有在中途被丢掉。
        let specs: Vec<_> = cfg
            .tests
            .iter()
            .map(|t| {
                builder::spec_from_config(t, &cfg, &state.master, &state.agent).expect("建 spec")
            })
            .collect();
        let mut port = builder::PORT_BASE;
        let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
        let mut sent: Vec<Vec<String>> = Vec::new();
        for unit in &units {
            for leg in &unit.legs {
                match &leg.kind {
                    builder::LegKind::IperfSingle(task) => sent.push(task.extra.clone()),
                    builder::LegKind::IperfGroup { streams, .. } => {
                        sent.extend(streams.iter().map(|task| task.extra.clone()))
                    }
                    _ => {}
                }
            }
        }
        assert!(!sent.is_empty(), "应当建出 iperf 任务");
        for extra in &sent {
            let at = extra
                .iter()
                .position(|arg| arg == "-l")
                .unwrap_or_else(|| panic!("每条 UDP 命令都要带 -l: {extra:?}"));
            assert!(
                matches!(
                    extra.get(at + 1).map(String::as_str),
                    Some("64") | Some("1400")
                ),
                "{extra:?}"
            );
        }
    }

    /// `-l` 留空时不能凭空写一个值进去：「没指定」和「指定成某个数」
    /// 在报告里是两回事。
    #[test]
    fn a_blank_datagram_size_sends_no_l_flag_at_all() {
        let mut req = request();
        req.nic_policies
            .iter_mut()
            .for_each(|p| p.udp_bandwidth.clear());
        req.udp_lengths = vec!["  ".into(), String::new()];
        let cfg = config_from_request(&state_with_pair(), &req);
        let udp = cfg
            .tests
            .iter()
            .find(|t| t.transports.contains(&"udp".to_string()))
            .expect("应有 UDP spec");
        let profiles = udp.udp_profiles.as_ref().expect("应有档位");
        assert_eq!(profiles.len(), 3, "只有三个 -b 档位");
        assert!(profiles.iter().all(|p| p.length.is_none()));
    }

    /// 按网口钉死 -b 的方向，-l 档位仍要逐档跑：钉住的是带宽不是报文长度。
    #[test]
    fn pinning_the_bandwidth_does_not_pin_the_datagram_size() {
        let mut req = request();
        req.pairs[0].transports = vec!["udp".into()];
        req.pairs[0].directions = vec!["ab".into()];
        req.udp_lengths = vec!["64".into(), "1400".into()];
        let cfg = config_from_request(&state_with_pair(), &req);
        let pinned = cfg
            .tests
            .iter()
            .find(|t| t.direction.directions() == ["ab"])
            .expect("ab 被钉死");
        let profiles = pinned.udp_profiles.as_ref().expect("应有档位");
        assert_eq!(profiles.len(), 2, "两个 -l 档位各一份");
        assert!(profiles.iter().all(|p| p.length.is_some()));
    }

    /// 控制台默认不裁剪 -b，勾上才裁剪；配置文件里的值不参与。
    #[test]
    fn the_console_decides_clipping_regardless_of_the_config_file() {
        let mut state = state_with_pair();
        state.cfg.limit_udp_by_link_speed = true;

        let req = request();
        assert!(
            !config_from_request(&state, &req).limit_udp_by_link_speed,
            "界面没勾就不裁剪，配置文件里的 true 不能悄悄生效"
        );

        let mut on = request();
        on.limit_udp_by_link_speed = true;
        assert!(config_from_request(&state, &on).limit_udp_by_link_speed);
    }

    /// -l 必须能塞进一个 UDP 报文。
    #[test]
    fn an_impossible_datagram_size_is_rejected_before_starting_a_run() {
        let state = state_with_pair();
        let mut req = request();
        req.udp_lengths = vec!["70000".into()];
        let error = validated_config_from_request(&state, &req).unwrap_err();
        assert!(error.contains("65507"), "{error}");

        req.udp_lengths = vec!["1400x".into()];
        let error = validated_config_from_request(&state, &req).unwrap_err();
        assert!(error.contains("UDP -l"), "{error}");
    }

    /// `-b` × `-l` × `-w` 三维取组合，每一项留空就在那一维退化成「不下发」。
    #[test]
    fn udp_socket_buffer_steps_join_the_same_cross_product() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].transports = vec!["udp".into()];
        req.pairs[0].directions = vec!["ab".into()];
        req.nic_policies
            .iter_mut()
            .for_each(|p| p.udp_bandwidth.clear());
        req.udp_bandwidths = vec!["500m".into()];
        req.udp_lengths = vec!["64".into(), "1400".into()];
        req.udp_windows = vec!["2m".into(), "8m".into()];

        let cfg = config_from_request(&state, &req);
        let udp = cfg
            .tests
            .iter()
            .find(|t| t.transports.contains(&"udp".to_string()))
            .expect("应有 UDP spec");
        let mut labels: Vec<String> = udp
            .udp_profiles
            .as_ref()
            .expect("应有档位")
            .iter()
            .map(|p| p.label())
            .collect();
        labels.sort();
        assert_eq!(
            labels,
            vec![
                "UDP -b 500m -l 1400 -w 2m",
                "UDP -b 500m -l 1400 -w 8m",
                "UDP -b 500m -l 64 -w 2m",
                "UDP -b 500m -l 64 -w 8m",
            ],
            "1 档 -b × 2 档 -l × 2 档 -w = 4 档"
        );

        // UDP 的 -w 不能顺手改写 TCP 的 -w：两者是两个独立输入。
        assert_eq!(cfg.iperf.tcp_windows, vec!["2m", "4m", "256m"]);

        // 一路建到真实命令，确认 -w 跟着下发。
        let specs: Vec<_> = cfg
            .tests
            .iter()
            .map(|t| {
                builder::spec_from_config(t, &cfg, &state.master, &state.agent).expect("建 spec")
            })
            .collect();
        let mut port = builder::PORT_BASE;
        let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
        let mut seen_windows: Vec<String> = Vec::new();
        for unit in &units {
            for leg in &unit.legs {
                let tasks: Vec<&builder::IperfTask> = match &leg.kind {
                    builder::LegKind::IperfSingle(task) => vec![task],
                    builder::LegKind::IperfGroup { streams, .. } => streams.iter().collect(),
                    _ => Vec::new(),
                };
                for task in tasks {
                    let at = task
                        .extra
                        .iter()
                        .position(|arg| arg == "-w")
                        .unwrap_or_else(|| panic!("每条 UDP 命令都要带 -w: {:?}", task.extra));
                    seen_windows.push(task.extra[at + 1].clone());
                }
            }
        }
        seen_windows.sort();
        seen_windows.dedup();
        assert_eq!(seen_windows, vec!["2m", "8m"]);
    }

    /// 三项都留空时，UDP 命令上一个 `-l` / `-w` 都不该出现。
    #[test]
    fn blank_udp_extras_add_no_flags_to_the_command() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].transports = vec!["udp".into()];
        req.pairs[0].directions = vec!["ab".into()];
        req.nic_policies
            .iter_mut()
            .for_each(|p| p.udp_bandwidth.clear());
        req.udp_bandwidths = vec!["500m".into()];
        req.udp_lengths = Vec::new();
        req.udp_windows = Vec::new();

        let cfg = config_from_request(&state, &req);
        let profiles = cfg
            .tests
            .iter()
            .find(|t| t.transports.contains(&"udp".to_string()))
            .and_then(|t| t.udp_profiles.clone())
            .expect("应有档位");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].label(), "UDP -b 500m");
        assert!(profiles[0].length.is_none() && profiles[0].window.is_none());
    }

    /// 配置文件里重复出现的 `-l` / `-w` 回填到界面时要压成一份，
    /// 否则打开页面档位就自己翻倍。
    #[test]
    fn repeated_profile_extras_collapse_when_filling_the_form() {
        assert_eq!(
            distinct(["2m", "8m", "2m", "8m"].iter().map(|v| v.to_string())),
            vec!["2m", "8m"]
        );
    }

    /// UDP 的 -w 和 TCP 一样按尺寸解析，写错要在开跑前拦下。
    #[test]
    fn an_invalid_udp_socket_buffer_is_rejected_before_starting_a_run() {
        let state = state_with_pair();
        let mut req = request();
        req.udp_windows = vec!["8毫米".into()];
        let error = validated_config_from_request(&state, &req).unwrap_err();
        assert!(error.contains("UDP -w"), "{error}");
    }

    /// 门限输入框要同时收下绝对值和百分比两种写法。
    #[test]
    fn the_threshold_field_takes_both_mbps_and_percent() {
        assert_eq!(parse_rx_target("1800"), Ok(Some(RxTarget::Mbps(1800.0))));
        assert_eq!(
            parse_rx_target(" 1800.5 "),
            Ok(Some(RxTarget::Mbps(1800.5)))
        );
        assert_eq!(parse_rx_target("90%"), Ok(Some(RxTarget::Percent(90.0))));
        assert_eq!(parse_rx_target("90 %"), Ok(Some(RxTarget::Percent(90.0))));
        assert_eq!(parse_rx_target(""), Ok(None));
        assert_eq!(parse_rx_target("   "), Ok(None));

        assert!(parse_rx_target("0").is_err(), "0 不是门限");
        assert!(parse_rx_target("-5").is_err());
        assert!(parse_rx_target("很快").is_err());
        assert!(
            parse_rx_target("900%").is_err(),
            "三位数百分比几乎一定是把 Mbps 写成了百分号"
        );
    }

    /// 百分比要落到 by_nic.rx_target_percent，绝对值落到 rx_target_mbps，
    /// 两者不能互相串。
    #[test]
    fn percent_and_absolute_thresholds_land_in_different_fields() {
        let mut req = request();
        req.nic_policies[0].rx_target = "90%".into();
        req.nic_policies[1].rx_target = "1600".into();
        let cfg = config_from_request(&state_with_pair(), &req);

        let by_percent = cfg
            .link_profiles
            .by_nic
            .iter()
            .find(|p| p.name == "以太网 6")
            .expect("主控网卡");
        assert_eq!(by_percent.rx_target_percent, Some(90.0));
        assert_eq!(by_percent.rx_target_mbps, None);

        let absolute = cfg
            .link_profiles
            .by_nic
            .iter()
            .find(|p| p.name == "WLAN 3")
            .expect("辅测网卡");
        assert_eq!(absolute.rx_target_mbps, Some(1600.0));
        assert_eq!(absolute.rx_target_percent, None);
    }

    /// 按网口填的 `-l` 要覆盖全局档位，且只作用于这块网卡作发送端的那条腿。
    #[test]
    fn a_per_nic_datagram_size_overrides_the_global_step() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].transports = vec!["udp".into()];
        req.pairs[0].directions = vec!["ab".into(), "ba".into()];
        req.udp_bandwidths = vec!["100m".into()];
        req.udp_lengths = vec!["1400".into()];
        // 只有主控口指定 -l 64；辅测口留空，走全局的 1400。
        req.nic_policies[0].udp_length = "64".into();
        req.nic_policies[1].udp_length.clear();

        let cfg = config_from_request(&state, &req);
        let specs: Vec<_> = cfg
            .tests
            .iter()
            .map(|t| {
                builder::spec_from_config(t, &cfg, &state.master, &state.agent).expect("建 spec")
            })
            .collect();
        let mut port = builder::PORT_BASE;
        let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);

        let mut by_sender: Vec<(String, String)> = Vec::new();
        for unit in &units {
            for leg in &unit.legs {
                let tasks: Vec<&builder::IperfTask> = match &leg.kind {
                    builder::LegKind::IperfSingle(task) => vec![task],
                    builder::LegKind::IperfGroup { streams, .. } => streams.iter().collect(),
                    _ => Vec::new(),
                };
                for task in tasks {
                    let at = task
                        .extra
                        .iter()
                        .position(|arg| arg == "-l")
                        .unwrap_or_else(|| panic!("应带 -l: {:?}", task.extra));
                    by_sender.push((task.src.nic.name.clone(), task.extra[at + 1].clone()));
                }
            }
        }
        by_sender.sort();
        by_sender.dedup();
        assert_eq!(
            by_sender,
            vec![
                ("WLAN 3".to_string(), "1400".to_string()),
                ("以太网 6".to_string(), "64".to_string()),
            ],
            "发送口填了 -l 就用它的，没填的那条腿仍走全局档位"
        );

        // 标签必须跟着实际下发值走，不然报表里印的 -l 和命令行对不上。
        assert!(
            units.iter().any(|u| u.title.contains("-l 64")),
            "{:?}",
            units.iter().map(|u| &u.title).collect::<Vec<_>>()
        );
    }

    /// 只填 `-l`、不填 `-b` 的网口不算「带宽被钉死」，仍要扫全局 -b 档位。
    #[test]
    fn a_per_nic_datagram_size_alone_does_not_pin_the_bandwidth() {
        let mut req = request();
        req.pairs[0].transports = vec!["udp".into()];
        req.pairs[0].directions = vec!["ab".into()];
        req.nic_policies
            .iter_mut()
            .for_each(|p| p.udp_bandwidth.clear());
        req.nic_policies[0].udp_length = "64".into();
        req.udp_bandwidths = vec!["1m".into(), "500m".into(), "1G".into()];

        let cfg = config_from_request(&state_with_pair(), &req);
        let udp = cfg
            .tests
            .iter()
            .find(|t| t.transports.contains(&"udp".to_string()))
            .expect("应有 UDP spec");
        assert_eq!(
            udp.udp_profiles.as_ref().map(Vec::len),
            Some(3),
            "-b 没被覆盖，三个档位都要跑"
        );
    }

    /// 三项全空才不生成覆盖项；只填 `-l` 也要生成。
    #[test]
    fn a_lone_datagram_size_still_produces_an_override() {
        let mut req = request();
        for policy in &mut req.nic_policies {
            policy.rx_target.clear();
            policy.udp_bandwidth.clear();
            policy.udp_length.clear();
        }
        req.nic_policies[0].udp_length = "64".into();
        let cfg = config_from_request(&state_with_pair(), &req);
        assert_eq!(cfg.link_profiles.by_nic.len(), 1);
        assert_eq!(
            cfg.link_profiles.by_nic[0].udp_length.as_deref(),
            Some("64")
        );
    }

    /// 按网口的 -l 同样不能超过一个 UDP 报文装得下的大小。
    #[test]
    fn a_per_nic_datagram_size_is_bounded_too() {
        let state = state_with_pair();
        let mut req = request();
        req.nic_policies[0].udp_length = "70000".into();
        let error = validated_config_from_request(&state, &req).unwrap_err();
        assert!(error.contains("65507"), "{error}");
    }
}
