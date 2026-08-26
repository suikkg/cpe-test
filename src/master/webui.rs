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
#[derive(Debug, Clone, Serialize, Deserialize)]
struct NicPolicySelection {
    /// `master:NAME=以太网 6`
    endpoint: String,
    /// 这块网卡作为接收端时的 RX 通过门限（Mbps）。
    #[serde(default)]
    rx_target_mbps: Option<f64>,
    /// 这块网卡作为发送端时的 UDP 单流带宽；留空表示走全局档位。
    #[serde(default)]
    udp_bandwidth: String,
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
    #[serde(default = "default_streams")]
    udp_streams: u32,
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
    duration: u64,
    tcp_windows: Vec<String>,
    tcp_streams: Vec<u32>,
    udp_bandwidths: Vec<String>,
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
            if let Some(profile) = cfg.link_profiles.by_nic.iter().find(|profile| {
                profile.host.eq_ignore_ascii_case(host)
                    && profile.name == nic.name
                    && (profile.ipv4.is_empty() || profile.ipv4 == nic.ipv4)
            }) {
                policies.push(NicPolicySelection {
                    endpoint: format!("{host}:NAME={}", nic.name),
                    rx_target_mbps: profile.rx_target_mbps,
                    udp_bandwidth: profile.udp_bandwidth.clone().unwrap_or_default(),
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
        if let Some(target) = policy.rx_target_mbps {
            if !target.is_finite() || target <= 0.0 {
                return Err(format!(
                    "{} 的 RX 门限必须是大于 0 的有限值",
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
    let global_udp: Vec<UdpProfile> = req
        .udp_bandwidths
        .iter()
        .filter(|b| !b.trim().is_empty())
        .map(|b| UdpProfile::bw(b.trim()))
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
        if let Some(profile) = nic_profile(
            &policy.endpoint,
            &policy.udp_bandwidth,
            policy.rx_target_mbps,
        ) {
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
            let mut spec = base(format!("ui-{}-udp", idx + 1), vec!["udp".into()]);
            let sends_ab = directions.iter().any(|d| d == "ab" || d == "bidir");
            let sends_ba = directions.iter().any(|d| d == "ba" || d == "bidir");
            let all_senders_explicit = (!sends_ab || explicit_udp_senders.contains(&pair.src))
                && (!sends_ba || explicit_udp_senders.contains(&pair.dst));

            spec.udp_profiles = Some(if all_senders_explicit {
                // 所有会发送的腿都有按网口覆盖时，扫全局档位只会生成重复单元。
                // 用任一实际覆盖值作占位；builder 会按腿替换成各自的精确值。
                let placeholder = req
                    .nic_policies
                    .iter()
                    .find(|policy| {
                        (policy.endpoint == pair.src || policy.endpoint == pair.dst)
                            && !policy.udp_bandwidth.trim().is_empty()
                    })
                    .map(|policy| policy.udp_bandwidth.trim())
                    .unwrap_or("1m");
                vec![UdpProfile::bw(placeholder)]
            } else {
                // 只覆盖了一条腿时，未覆盖的腿仍逐个扫描全局 -b 档位；已覆盖的腿
                // 在每个单元里保持固定值。不能因为“任一腿有覆盖”就压成一档。
                if global_udp.is_empty() {
                    cfg.iperf.udp_profiles.clone()
                } else {
                    global_udp.clone()
                }
            });
            tests.push(spec);
        }
    }
    cfg.tests = tests;
    cfg
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

/// `master:NAME=以太网 6` -> 一条 by_nic 覆盖。
fn nic_profile(
    endpoint: &str,
    bandwidth: &str,
    rx_target: Option<f64>,
) -> Option<crate::config::NicProfile> {
    if bandwidth.trim().is_empty() && rx_target.is_none() {
        return None;
    }
    let (host, rest) = endpoint.split_once(':')?;
    let name = rest.strip_prefix("NAME=")?;
    Some(crate::config::NicProfile {
        host: host.to_string(),
        name: name.to_string(),
        ipv4: String::new(),
        rx_target_mbps: rx_target.filter(|v| v.is_finite() && *v > 0.0),
        udp_bandwidth: (!bandwidth.trim().is_empty()).then(|| bandwidth.trim().to_string()),
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
                    rx_target_mbps: Some(1800.0),
                    udp_bandwidth: "2.6G".into(),
                },
                NicPolicySelection {
                    endpoint: "agent:NAME=WLAN 3".into(),
                    rx_target_mbps: Some(1600.0),
                    udp_bandwidth: "2.8G".into(),
                },
            ],
            duration: 60,
            tcp_windows: vec!["2m".into(), "4m".into(), "256m".into()],
            tcp_streams: vec![1, 5, 10],
            udp_bandwidths: vec!["1m".into(), "500m".into(), "1G".into()],
            udp_streams: 1,
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

    /// 一边按网口固定、另一边留空时，留空腿仍需扫描全部全局档位。
    #[test]
    fn a_one_sided_bandwidth_override_keeps_the_sweep_for_the_other_leg() {
        let mut req = request();
        req.nic_policies[1].udp_bandwidth.clear();
        let cfg = config_from_request(&state_with_pair(), &req);
        let udp = cfg
            .tests
            .iter()
            .find(|test| test.transports.contains(&"udp".to_string()))
            .expect("应有 UDP spec");
        assert_eq!(
            udp.udp_profiles.as_ref().map(Vec::len),
            Some(3),
            "未覆盖的反向发送腿仍要跑 1m/500m/1G 三档"
        );
    }

    /// 没填就不生成覆盖项，避免用一堆空条目盖掉配置文件里原有的策略。
    #[test]
    fn blank_inputs_produce_no_overrides() {
        let mut req = request();
        for policy in &mut req.nic_policies {
            policy.rx_target_mbps = None;
            policy.udp_bandwidth.clear();
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
}
