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
use crate::master::executor::{ResultDb, RESUME_MAX_AGE_HOURS};
use crate::master::ui::{run_master, MasterOpts};
use crate::protocol::{HealthOut, HostInfo, InfoReq, Resp};
use crate::util::{clear_log_mirror, lock_recover, log_tail_since};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    /// 空串 = 不启用认证（只监听回环时的默认）。
    ///
    /// **有意不进 config.json**：控制台自己就提供「下载 config.json」，
    /// 把它的访问口令写进那份可下载、可传阅的文件里等于当场泄露。
    ui_token: String,
    /// 速率监控会话。**不受 `running` 约束**：监控和一轮测试是正交的两件事，
    /// 边跑边看正是它最有用的场景。
    monitors: Mutex<HashMap<String, MonitorSession>>,
}

/// 环形缓冲上限：1 秒一个点约等于 2 小时。再长就该用 `cpe_test monitor --csv`，
/// 网页不该变成一个无上限的数据库。
const MONITOR_MAX_POINTS: usize = 7200;

#[derive(Debug, Clone, Serialize)]
struct MonitorPoint {
    /// 会话开始后的秒数。用相对时间而不是墙钟：主控和辅测机的系统时钟
    /// 不保证同步，两条曲线放在一起看时相对时间才对得上。
    t: f64,
    rx_mbps: f64,
    tx_mbps: f64,
}

#[derive(Default)]
struct MonitorData {
    points: std::collections::VecDeque<MonitorPoint>,
    /// 已经被挤出缓冲的点数，游标是绝对序号，靠它换算。
    dropped: usize,
    error: Option<String>,
    running: bool,
    /// 页面最后一次来取样本的时刻；`None` = 一次都没来过。
    ///
    /// 采样线程靠它自己收摊。关掉浏览器标签页不会通知服务端，没有这道
    /// 兜底的话，本机那条线程会一直读计数器，辅测机那条还会一直占着
    /// agent 上的 monitor 资源直到租约到期。
    last_poll: Option<std::time::Instant>,
}

/// 页面静默多久之后采样线程自行收摊。页面每秒来取一次，90 秒没动静
/// 只能是标签页已经不在了。
const MONITOR_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// 同时存在的监控会话上限。
///
/// 页面一次只开一路，这里的余量是留给「多开了几个标签页」和还没轮到回收的
/// 旧会话的。撞上限只可能是有人在刷接口——控制台一旦 `--ui-bind` 出去，
/// 一个拿到口令的客户端循环调 /api/monitor/start 就能一路撑起线程和
/// 辅测机侧的 monitor 资源。
///
/// 这是个软上限：查上限和插表之间隔着起线程那一步，几条工作线程同时进来时
/// 最多会多出 `UI_WORKERS - 1` 条。这不影响它要挡的事——表满之后每一次
/// start 都会被拒，涨不上去。
const MONITOR_MAX_SESSIONS: usize = 8;

/// 辅测机侧监控的租约秒数。
///
/// **这是一个心跳周期，不是监控时长上限**：`/monitor/status` 每次都会给
/// agent 那边续期。所以它要回答的问题只有一个——「控制台没了之后，
/// 辅测机最多替它白跑多久」。180 秒足够扛过最大轮询间隔（60s）加网络抖动，
/// 又不会让一个孤儿监控在对面挂上小时级。
const UI_MONITOR_LEASE_SECS: u64 = 180;

struct MonitorSession {
    side: String,
    iface: String,
    stop: Arc<AtomicBool>,
    data: Arc<Mutex<MonitorData>>,
    /// 会话建立的时刻。回收要用它：页面刚开就被关掉的会话一次样本都没取过，
    /// 没有 `last_poll` 可看。
    started: std::time::Instant,
}

/// 回收已经收摊、页面也不再来取的会话。
///
/// 采样线程自己会在 `MONITOR_IDLE_TIMEOUT` 之后停掉（见 `monitor_abandoned`），
/// 但停掉的只是线程：会话连同它那个最多 `MONITOR_MAX_POINTS` 点的缓冲还留在
/// 表里。关掉浏览器标签页不会通知服务端，所以「显式 /api/monitor/stop」不能
/// 是唯一的出口——否则每刷新一次页面就多留一条，永远不掉。
fn reap_dead_monitors(monitors: &mut HashMap<String, MonitorSession>) {
    monitors.retain(|_, session| {
        let (running, last_poll) = {
            let data = lock_recover(&session.data);
            (data.running, data.last_poll)
        };
        if running {
            return true;
        }
        // 线程已停：再留一个空闲窗口，让还在的页面把「已停止」读走、正常收尾。
        // 过了这个窗口无论它还在不在，这条都该消失。
        match last_poll {
            Some(seen) => seen.elapsed() <= MONITOR_IDLE_TIMEOUT,
            None => session.started.elapsed() <= MONITOR_IDLE_TIMEOUT,
        }
    });
}

/// 页面已经不在了吗？两条采样线程共用同一判据。
fn monitor_abandoned(data: &Arc<Mutex<MonitorData>>, started: std::time::Instant) -> bool {
    let last_poll = lock_recover(data).last_poll;
    match last_poll {
        Some(seen) => seen.elapsed() > MONITOR_IDLE_TIMEOUT,
        None => started.elapsed() > MONITOR_IDLE_TIMEOUT,
    }
}

impl MonitorData {
    fn push(&mut self, point: MonitorPoint) {
        self.points.push_back(point);
        while self.points.len() > MONITOR_MAX_POINTS {
            self.points.pop_front();
            self.dropped += 1;
        }
    }
}

/// 界面提交回来的一条配对选择。
#[derive(Debug, Clone, Deserialize)]
struct PairSelection {
    /// `master:NAME=以太网 6`
    src: String,
    dst: String,
    #[serde(default)]
    directions: Vec<String>,
    /// 这一对网口在**双向并发**单元里的接收门限，按方向分开填。
    ///
    /// 只在勾了「双向」时生效；留空 = 双向也走既有的兜底链。
    ///
    /// 为什么按配对而不是按网卡：同一块 RNDIS 口，和 Wi-Fi 组双向、和 SGMII
    /// 组双向，能收到的速率完全不是一个量级。门限挂在网卡上只能填一个数，
    /// 必然有一组是错的——受限的是这条链路，不是某一端的网卡。
    ///
    /// 两个方向分开是因为半双工链路的两个方向本来就可以差很远
    /// （同一次运行里见过 1821Mbps 对 17Mbps）。
    #[serde(default)]
    rx_target_bidir_ab: String,
    #[serde(default)]
    rx_target_bidir_ba: String,
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
    /// ping 次数；0 = 沿用配置里的 `ping.count`。
    #[serde(default)]
    ping_count: u32,
    /// ping 包长档位（每个档位单独成一个测试单元）；空 = 沿用 `ping.payload_sizes`。
    #[serde(default)]
    ping_payload_sizes: Vec<u32>,
    /// 是否按整条路径的可信上限裁剪 UDP `-b`。
    ///
    /// 界面默认关：控制台上填多少就发多少，超额灌包本来就是要看的场景之一。
    /// 配置文件里的 `limit_udp_by_link_speed` 只作用于命令行路径，不回填到这里，
    /// 否则同一个勾选框在不同机器上含义不同。
    #[serde(default)]
    limit_udp_by_link_speed: bool,
    /// 24 小时内已有正式 PASS 的单元直接跳过。
    ///
    /// 和 `limit_udp_by_link_speed` 一样由界面覆盖配置文件：同一个勾选框
    /// 在不同机器上必须是同一个意思。
    #[serde(default)]
    resume: bool,
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
    ping_count: u32,
    ping_payload_sizes: Vec<u32>,
    screenshot: bool,
}

/// 本机信息。**不需要连上辅测机**——这是控制台打开就能给出的东西。
#[derive(Debug, Serialize)]
struct LocalOut {
    host: HostInfo,
    iperf3: Option<String>,
    version: String,
}

#[derive(Debug, Serialize)]
struct PlannedUnit {
    seq: usize,
    title: String,
    est_secs: u64,
    /// 本轮开了 resume，且这个单元在 24 小时内已有 PASS——会被跳过。
    resumed: bool,
}

#[derive(Debug, Serialize)]
struct PlanOut {
    units: Vec<PlannedUnit>,
    /// 预计跳过的都真跳过时的耗时。
    est_total_secs: u64,
    /// 一个都不跳时的耗时。开着 resume 时页面按区间显示，理由见 `api_plan`。
    est_full_secs: u64,
    notices: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ProgressOut {
    running: bool,
    from: usize,
    lines: Vec<String>,
    report: String,
}

/// 控制台启动参数。
pub struct UiOpts {
    pub bind: String,
    pub port: u16,
    pub config_path: Option<String>,
    /// 主控访问辅测机用的共享口令；`None` = 沿用配置文件里的值。
    pub agent_token: Option<String>,
    /// 浏览器访问控制台需要的口令；空串 = 不认证。
    ///
    /// 和 `agent_token` 是两件事：前者是主控→辅测机，后者是浏览器→控制台。
    pub ui_token: String,
}

/// 绑定地址是否只有本机能连上。
///
/// 判据放宽一点没关系（把某个实际可路由的地址误判成回环才危险，反过来
/// 只是多要一个 token），所以这里只认明确的回环写法。
pub(crate) fn bind_is_loopback(bind: &str) -> bool {
    let bind = bind.trim();
    bind.eq_ignore_ascii_case("localhost")
        || bind == "::1"
        || bind == "[::1]"
        || bind
            .parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// 把绑定地址和端口拼成 `Server::http` 能解析的监听地址。
///
/// 裸 IPv6 必须补方括号，否则 `"::1:28800"` 里的冒号无从区分地址和端口，
/// 解析直接失败——而 `bind_is_loopback` 是认 `"::1"` 的，不补的话
/// 「判定放行 → 监听失败」这条路走得通，人只会看到一句莫名其妙的启动错误。
pub(crate) fn listen_addr(bind: &str, port: u16) -> String {
    let bind = bind.trim();
    if bind.contains(':') && !bind.starts_with('[') {
        format!("[{bind}]:{port}")
    } else {
        format!("{bind}:{port}")
    }
}

/// 同时处理请求的线程数。
///
/// 单线程轮询在这里是会被人看见的卡顿：`/api/local` 要跑一次 `scan_host()`，
/// 在 Windows 上会拉起 ipconfig/netsh，一到两秒；这期间页面每秒一次的日志轮询
/// 和速率采样轮询全在排队，日志停住、曲线断一截。控制台的共享状态本来就都在
/// Mutex 后面，并发处理不需要额外的同步。
const UI_WORKERS: usize = 4;

/// 取请求循环的空转周期：没有请求进来时，隔这么久回头查一次取消标志。
/// 取小一点没有代价（`recv_timeout` 超时是纯等待），但要足够小，
/// 让 Ctrl+C 之后的退出感觉是「立刻」。
const SHUTDOWN_POLL: Duration = Duration::from_millis(200);

/// 启动控制台，阻塞直到进程结束。
pub fn run(opts: UiOpts) -> i32 {
    let UiOpts {
        bind,
        port,
        config_path,
        agent_token,
        ui_token,
    } = opts;
    // 控制台能改配置、能启动测试、能下载 config——放到回环之外而不设口令，
    // 等于把这台机器的测试控制权交给同网段的任何人。这里直接不启动，
    // 而不是打印一行警告了事：警告会被划过去，开着的洞不会自己关上。
    if !bind_is_loopback(&bind) && ui_token.is_empty() {
        eprintln!("!! 拒绝在 {bind} 上启动无口令的控制台。");
        eprintln!("!! 控制台可以改配置并发起测试，暴露到网络上必须设访问口令：");
        eprintln!("!!   cpe_test ui --ui-bind {bind} --ui-token 你的口令");
        eprintln!("!! 或者用 SSH 转发，把控制台留在回环上：");
        eprintln!("!!   ssh -L {port}:127.0.0.1:{port} 你@这台机器");
        return 2;
    }
    let (mut cfg, _) = load_config(config_path.as_deref());
    if let Some(token) = agent_token {
        cfg.agent_token = token;
    }
    // 配置文件没写地址时回落到上次连上的那台：控制台每跑完一轮都会经由
    // run_master 把它记下来，只写不读的话等于每次打开都从零开始。
    let agent_host = if cfg.agent_host.trim().is_empty() {
        crate::master::ui::last_agent_host().unwrap_or_default()
    } else {
        cfg.agent_host.clone()
    };
    let console = Arc::new(Console {
        state: Mutex::new(UiState {
            cfg,
            agent_host,
            ..Default::default()
        }),
        running: AtomicBool::new(false),
        report: Mutex::new(String::new()),
        ui_token: ui_token.clone(),
        monitors: Mutex::new(HashMap::new()),
    });

    // 默认仍只监听回环。放开要靠显式的 --ui-bind，且上面已经拦掉了
    // 「非回环 + 无口令」这种组合。
    let addr = listen_addr(&bind, port);
    let server = match Server::http(addr.as_str()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("!! 控制台无法监听 {addr}: {e}");
            eprintln!("!! 端口可能被占用，换一个：cpe_test ui --port 28900");
            return 2;
        }
    };
    // 口令放在 URL 的查询串里，页面加载后会把它从地址栏抹掉（见 webui.html）。
    // 这样「打开控制台」仍然是复制粘贴一个地址，不用先教人怎么加请求头。
    let url = if ui_token.is_empty() {
        format!("http://{addr}")
    } else {
        format!("http://{addr}?token={}", urlencode(&ui_token))
    };
    println!("控制台已启动: {url}");
    println!("（浏览器没自动弹出的话，手动复制上面这个地址打开）");
    if !bind_is_loopback(&bind) {
        println!("注意：控制台正监听在 {bind}，同网段能访问到它；口令泄露即等于测试控制权泄露。");
    }
    crate::console::open_url(&url);

    // 控制台自己就要装 Ctrl+C 处理器，不能等第一轮测试跑起来才由
    // `run_master()` 顺手装上。`cancel` 用 `Once` 注册且**永不撤销**，
    // 而非 Windows 分支的 handler 只置标志、不退出进程——一旦它在别处装好，
    // 下面这个循环又从不查取消标志，SIGINT 就被永久吃掉了：跑过一轮测试之后
    // Ctrl+C 再也关不掉控制台，只能另开终端 kill。
    crate::cancel::setup_cancel_handler();

    let server = Arc::new(server);
    let shutdown = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (1..UI_WORKERS)
        .filter_map(|idx| {
            let server = Arc::clone(&server);
            let console = Arc::clone(&console);
            let shutdown = Arc::clone(&shutdown);
            std::thread::Builder::new()
                .name(format!("cpe-ui-http-{idx}"))
                .spawn(move || serve(&server, &console, &shutdown))
                .ok()
        })
        .collect();
    serve(&server, &console, &shutdown);
    // 让还堵在 recv_timeout 里的工作线程立刻收场，而不是各自再等一个超时。
    server.unblock();
    for worker in workers {
        let _ = worker.join();
    }
    // 退出前把监控会话收干净：辅测机侧那路要 POST /monitor/stop，
    // 否则它会一直占着对面的采样线程直到租约到期。
    stop_all_monitors(&console);
    println!("控制台已退出。");
    0
}

/// 收到 Ctrl+C 之后是否该收摊。
///
/// 一轮测试正在跑时**不能**退：那次 Ctrl+C 的语义是「优雅结束当前单元并出报告」，
/// 控制台进程得活到 `run_master()` 把报告写完。等它收完尾、`running` 落回 false，
/// 下一拍才轮到控制台自己退出——和命令行主控按一次 Ctrl+C 的行为对齐。
fn should_shut_down(cancelled: bool, run_in_flight: bool) -> bool {
    cancelled && !run_in_flight
}

/// 停掉全部监控会话。进程退出前调用，也可被显式关停复用。
fn stop_all_monitors(console: &Arc<Console>) {
    let sessions: Vec<String> = lock_recover(&console.monitors).keys().cloned().collect();
    for session in sessions {
        let body = serde_json::json!({ "session": session }).to_string();
        let _ = api_monitor_stop(console, &body);
    }
}

/// 一条工作线程的取请求-处理循环。`recv_timeout` 在多线程间是安全的，
/// tiny_http 自己排队分发。
///
/// 用 `recv_timeout` 而不是 `recv()`：后者没有出口，取消标志永远查不到。
fn serve(server: &Server, console: &Arc<Console>, shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::SeqCst) {
        match server.recv_timeout(SHUTDOWN_POLL) {
            Ok(Some(request)) => handle(request, console),
            Ok(None) => {
                if should_shut_down(
                    crate::cancel::is_cancelled(),
                    console.running.load(Ordering::SeqCst),
                ) {
                    shutdown.store(true, Ordering::SeqCst);
                }
            }
            Err(_) => break,
        }
    }
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

fn header_value(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

/// 只处理会出现在 token 里的那些字符；这里不需要一个通用的 URL 编码器。
fn urlencode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// 请求是否带对了控制台口令。
///
/// 三种带法都认：浏览器第一次打开只能靠查询串（地址栏里输不了请求头），
/// 页面之后的 API 调用走请求头，`Authorization: Bearer` 则是为了和 agent
/// 协议侧保持一致、也方便 curl 复现问题。
pub(crate) fn request_is_authorized(
    token: &str,
    query: &str,
    header_token: Option<&str>,
    bearer: Option<&str>,
) -> bool {
    if token.is_empty() {
        return true;
    }
    if header_token.is_some_and(|value| secret_eq(value, token))
        || bearer.is_some_and(|value| secret_eq(value, token))
    {
        return true;
    }
    query
        .split('&')
        .filter_map(|kv| kv.strip_prefix("token="))
        .any(|value| secret_eq(&urldecode(value), token))
}

/// 口令比较，不因第一个不同的字节提前返回。
///
/// `--ui-bind` 之后控制台就在局域网上了，而这里既没有失败限速也没有锁定：
/// 普通的 `==` 会在第一个不匹配的字节上返回，攻击者可以不限次数地量响应时间，
/// 一个字节一个字节把口令试出来。长度仍然会泄露，那是口令强度的事，
/// 不是能逐位收敛的信道。
fn secret_eq(given: &str, expected: &str) -> bool {
    let (given, expected) = (given.as_bytes(), expected.as_bytes());
    if given.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in given.iter().zip(expected) {
        diff |= a ^ b;
    }
    diff == 0
}

fn urldecode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            // 按**字节**取那两位十六进制，不能对 &str 下标切片：`%` 后面
            // 跟着多字节字符时（比如 "%中"），字符串切片会切在字符中间
            // 直接 panic——而这段输入来自网络，谁都能构造。
            b'%' if idx + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[idx + 1..idx + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        idx += 3;
                    }
                    None => {
                        out.push(b'%');
                        idx += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            byte => {
                out.push(byte);
                idx += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

    // 鉴权先于一切，页面本身也不例外：页面里带着给 API 用的口令，
    // 放行未认证的 GET / 等于把口令发给任何来问的人。
    let header_token = header_value(&request, "X-CPE-Token");
    let bearer = header_value(&request, "Authorization").and_then(|value| {
        value
            .strip_prefix("Bearer ")
            .map(|token| token.trim().to_string())
    });
    if !request_is_authorized(
        &console.ui_token,
        &query,
        header_token.as_deref(),
        bearer.as_deref(),
    ) {
        let body = crate::protocol::err_json(
            "未认证：控制台已启用访问口令，请用启动时打印的完整地址（带 ?token=）打开",
        );
        let _ = request.respond(json_response(body).with_status_code(401));
        return;
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
    } else if is_get && path == "/api/local" {
        api_local()
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
    } else if is_post && path == "/api/monitor/start" {
        api_monitor_start(console, &body)
    } else if is_get && path == "/api/monitor/samples" {
        api_monitor_samples(console, &query)
    } else if is_post && path == "/api/monitor/stop" {
        api_monitor_stop(console, &body)
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

/// 本机网卡与工具链。连接辅测机之前就可用。
///
/// 有意**不按 `ipv4_prefixes` 过滤**，理由和辅测机状态页那份一样：要填给对面的
/// 那个地址常常就在被过滤掉的管理网段上，过滤过的表在这里等于把答案藏起来。
/// 「网口与策略」那张表才是按前缀筛过的测试口列表，两者用途不同。
fn api_local() -> Result<serde_json::Value, String> {
    serde_json::to_value(LocalOut {
        host: crate::nic::scan_host(&[]),
        iperf3: crate::cmd::tools::iperf3_version(),
        version: env!("CARGO_PKG_VERSION").into(),
    })
    .map_err(|error| error.to_string())
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
        ping_count: state.cfg.ping.count,
        ping_payload_sizes: state.cfg.ping.payload_sizes.clone(),
        screenshot: state.cfg.screenshot,
    })
    .map_err(|error| error.to_string())
}

/// 回显成用户当初的写法：绝对值回显数字，百分比回显 `90%`。
fn rx_target_text(mbps: Option<f64>, percent: Option<f64>) -> String {
    match (mbps, percent) {
        (Some(mbps), _) => format!("{mbps}"),
        (None, Some(percent)) => format!("{percent}%"),
        (None, None) => String::new(),
    }
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
                    rx_target: rx_target_text(profile.rx_target_mbps, profile.rx_target_percent),
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
    // ping 的包长和次数同样要在这里挡住，理由和上面那条 UDP -l 一样：下游只会
    // **静默夹紧**（`ping::build` 把包长压到 MAX_PAYLOAD，`spec_from_config` 把
    // 次数压到 100000），而夹紧发生在分单元之后——两个越界档位各自成一个单元、
    // 各自算一个 resume id，跑出来却是同一次测试，报告上还写着两个不同的 -l。
    for size in &req.ping_payload_sizes {
        if *size > crate::ping::MAX_PAYLOAD {
            return Err(format!(
                "ping 包长档位 {size} 超过单包上限 {} 字节",
                crate::ping::MAX_PAYLOAD
            ));
        }
    }
    if req.ping_count > 100_000 {
        return Err(format!("ping 次数 {} 超过上限 100000", req.ping_count));
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
        // PING 和 TCP/UDP 并排放在界面的「协议」列，白名单里也必须一起放行。
        // 漏掉它不只是「PING 跑不了」：整条请求会被判非法，连同一配对里本来
        // 能跑的 TCP/UDP 一起废掉，而错误文案还在让人去勾 TCP 或 UDP。
        // 双向门限只收绝对值，且只有勾了「双向」才有意义。填了却没勾双向要报错
        // 而不是静默忽略——静默忽略的话，人会以为门限放低了、看到 FAIL 去查链路。
        let bidir_selected = pair.directions.iter().any(|d| d == "bidir");
        for (label, raw) in [
            ("A→B", &pair.rx_target_bidir_ab),
            ("B→A", &pair.rx_target_bidir_ba),
        ] {
            if raw.trim().is_empty() {
                continue;
            }
            if !bidir_selected {
                return Err(format!(
                    "配对 {} / {} 填了 {label} 双向门限，却没有勾「双向」。\
                     双向门限只作用于双向并发单元；单向的门限在「网口与策略」里改",
                    pair.src, pair.dst
                ));
            }
            match parse_rx_target(raw).map_err(|error| {
                format!(
                    "配对 {} / {} 的 {label} 双向门限：{error}",
                    pair.src, pair.dst
                )
            })? {
                Some(RxTarget::Mbps(_)) => {}
                Some(RxTarget::Percent(_)) => {
                    return Err(format!(
                        "配对 {} / {} 的 {label} 双向门限只能填绝对 Mbps。\
                         百分比要按单块网卡的协商速率换算，而双向门限说的是这两块口\
                         并发时的能力，两者不成比例",
                        pair.src, pair.dst
                    ))
                }
                None => {}
            }
        }
        if !values_are_allowed(&pair.transports, &["tcp", "udp", "ping"]) {
            return Err(format!(
                "配对 {} / {} 至少勾 TCP / UDP / PING 之一",
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
    cfg.resume = req.resume;
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
    // 保序去重。`dedup()` 只合并相邻项，「32 1600 32」会留下两个 32——两个单元
    // 标题和 resume id 完全一样，在 task_results.json 里互相覆盖（后写的那条
    // 赢），resume 于是可能跳过一个其实 FAIL 了的单元，还白跑一遍全程。
    // 也不能先排序：档位顺序是用户自己排的，跑的顺序就该照他写的来。
    let mut seen_sizes = HashSet::new();
    let ping_sizes: Vec<u32> = req
        .ping_payload_sizes
        .iter()
        .copied()
        .filter(|size| *size > 0 && seen_sizes.insert(*size))
        .collect();
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
        // ping 在配置模型里是 `kinds` 而不是 `transports`——界面把它和 TCP/UDP
        // 并排放在「协议」列只是给人看的，落到 config 上必须分开：ping 单元
        // 不带 transport，走 builder 里那条独立分支。
        let want_ping = wants("ping");

        // 双向门限只有勾了「双向」才有意义；没勾时不写进 config，
        // 免得它出现在下载下来的 config.json 里让人以为在生效。
        let bidir_targets = directions
            .iter()
            .any(|d| d == "bidir")
            .then(|| crate::config::RateTargets {
                forward: None,
                ab: parse_rx_target(&pair.rx_target_bidir_ab)
                    .ok()
                    .flatten()
                    .and_then(rx_target_mbps),
                ba: parse_rx_target(&pair.rx_target_bidir_ba)
                    .ok()
                    .flatten()
                    .and_then(rx_target_mbps),
            })
            .filter(|targets| targets.ab.is_some() || targets.ba.is_some());

        let base = |name: String, transports: Vec<String>| TestSpec {
            name,
            rate_targets_bidir_mbps: bidir_targets.clone(),
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
        if want_ping {
            // 每个包长档位在 builder 里各成一个单元，所以这里必须让界面把
            // 次数和包长填全：不填就回落到 ping.count=100 × 三档包长，
            // 每个配对每个方向平白多出三个各一百多秒的单元，而这件事要到
            // 「预览任务」才看得见，太晚了。
            let mut spec = base(format!("ui-{}-ping", idx + 1), Vec::new());
            spec.kinds = vec!["ping".into()];
            spec.ping_count = (req.ping_count > 0).then_some(req.ping_count);
            if !ping_sizes.is_empty() {
                spec.ping_payload_sizes = Some(ping_sizes.clone());
            }
            tests.push(spec);
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

/// 配对门限只收绝对 Mbps。
///
/// 百分比要拿接收端网卡的协商速率来换算，而这个值每个单元开跑前才重扫；
/// 配对门限是「这两块口凑在一起、并发时的能力」，跟单独一块口的协商速率
/// 不成比例——`WIFI5G 2882Mbps × 50%` 和「和 RNDIS 组双向时能收到多少」
/// 没有关系。收百分比只会给出一个看着有依据、其实是瞎算的数。
fn rx_target_mbps(target: RxTarget) -> Option<f64> {
    match target {
        RxTarget::Mbps(value) => Some(value),
        RxTarget::Percent(_) => None,
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
    let mbps = |value: Option<RxTarget>| match value {
        Some(RxTarget::Mbps(value)) => Some(value),
        _ => None,
    };
    let percent = |value: Option<RxTarget>| match value {
        Some(RxTarget::Percent(value)) => Some(value),
        _ => None,
    };
    Some(crate::config::NicProfile {
        host: host.to_string(),
        name: name.to_string(),
        ipv4: String::new(),
        rx_target_mbps: mbps(target),
        rx_target_percent: percent(target),
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
    // resume 开着时只提示「会跳过一些」是不够的：勾了 resume 却看到满满一屏
    // 计划，人会以为没生效。这里直接查同一个结果库，把会被跳过的单元标出来，
    // 并把它们从预计耗时里扣掉——否则那个数字会比实际多出好几倍。
    //
    // 但这只是**预判**，不能当成承诺：executor 那边的跳过还要求
    // `blocked.is_none()`（流量后端前置检查没被拦），而且是在刷新过网卡快照、
    // 单元 id 可能已经变了之后才查的。所以两个耗时都报出去，页面按区间显示，
    // 免得人照着一个偏小的数字安排时间。
    let resumed: Vec<bool> = if cfg.resume {
        let db = ResultDb::load(std::path::PathBuf::from("task_results.json"));
        units
            .iter()
            .map(|u| db.fresh_pass(&u.id).is_some())
            .collect()
    } else {
        vec![false; units.len()]
    };
    let skip_count = resumed.iter().filter(|skipped| **skipped).count();
    if cfg.resume {
        notices.push(if skip_count == 0 {
            format!(
                "resume 已开启，但 {RESUME_MAX_AGE_HOURS} 小时内没有可复用的 PASS，{} 个单元全部实跑",
                units.len()
            )
        } else {
            format!(
                "resume 已开启：{skip_count}/{} 个单元在 {RESUME_MAX_AGE_HOURS} 小时内已 PASS，预计跳过。执行时还会再判一次——前置检查被拦或网卡快照变了的单元仍要实跑，所以耗时给的是区间",
                units.len()
            )
        });
    }
    let est_total_secs = units
        .iter()
        .zip(&resumed)
        .filter(|(_, skipped)| !**skipped)
        .map(|(u, _)| u.est_secs)
        .sum();
    let est_full_secs = units.iter().map(|u| u.est_secs).sum();
    let units = units
        .iter()
        .zip(&resumed)
        .enumerate()
        .map(|(idx, (unit, skipped))| PlannedUnit {
            seq: idx + 1,
            title: unit.title.clone(),
            est_secs: unit.est_secs,
            resumed: *skipped,
        })
        .collect();
    serde_json::to_value(PlanOut {
        units,
        est_total_secs,
        est_full_secs,
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
    if let Err(error) = write_private_config(&path, &json) {
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

/// 把临时 config 写成只有本人可读的文件。
///
/// 这份 config 里带着 `agent_token`，而 `std::env::temp_dir()` 在 Linux/macOS 上
/// 就是全局可读的 /tmp、`fs::write` 建出来的是 0644——同机的任何账号都能在这一轮
/// 测试期间把它 cat 出来。命令行那边特意支持用环境变量传 token，就是为了不让它
/// 落进 shell 历史和 ps 里给同机的人看见；这里不能又原样漏回去。
///
/// Windows 上 temp 目录本就是每个用户各一份，按默认 ACL 建文件即可。
fn write_private_config(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    // `mode()` 只在**创建**那一刻生效。同 pid 的上一轮如果留下过一个 0644 的
    // 残file，truncate 会沿用它原来的权限，所以先删干净再 create_new。
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())
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

#[derive(Debug, Deserialize)]
struct MonitorStartUiReq {
    /// "master" 或 "agent"。
    side: String,
    iface: String,
    #[serde(default)]
    interval_ms: u64,
}

#[derive(Debug, Deserialize)]
struct MonitorSessionReq {
    session: String,
}

#[derive(Debug, Serialize)]
struct MonitorSamplesOut {
    side: String,
    iface: String,
    from: usize,
    points: Vec<MonitorPoint>,
    running: bool,
    error: String,
}

/// 起一路速率监控。
///
/// 有意**不看 `console.running`**：一轮测试跑着的时候正是最想盯速率的时候。
/// 辅测机侧用独立的 owner_id，所以测试收尾那次 owner 范围的清理
/// （executor 侧发 `/resources/cleanup`）不会顺手把它掐掉。
fn api_monitor_start(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: MonitorStartUiReq =
        serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let iface = req.iface.trim().to_string();
    if iface.is_empty() {
        return Err("先选一块网卡".into());
    }
    // 上限跟着监控端走。辅测机侧的实际采样在 agent 里被夹到 200–5000ms
    // （`MonitorMgr::start_owned`），这里不跟着夹的话，填 10 秒会变成
    // 「agent 按 5 秒采、这边按 10 秒只取最后一个样本」——一半样本无声丢掉。
    // 界面也做了同样的限制，这里是不走界面时的那道。
    let interval_ms = monitor_interval_ms(&req.side, req.interval_ms);
    // 先回收再看上限，且都放在起线程之前：撞上限时不该已经有一条线程
    // 在跑（本机那条会一直读计数器，辅测机那条还占着对面的 monitor 资源）。
    {
        let mut monitors = lock_recover(&console.monitors);
        reap_dead_monitors(&mut monitors);
        if monitors.len() >= MONITOR_MAX_SESSIONS {
            return Err(format!(
                "同时最多 {MONITOR_MAX_SESSIONS} 路监控；先停掉一路再开"
            ));
        }
    }

    let data = Arc::new(Mutex::new(MonitorData {
        running: true,
        ..Default::default()
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let session = format!("mon-{}-{}", std::process::id(), now_millis());

    match req.side.as_str() {
        "master" => spawn_local_monitor(iface.clone(), interval_ms, &stop, &data),
        "agent" => {
            let (host, port, token) = {
                let state = lock_recover(&console.state);
                if state.agent_host.is_empty() {
                    return Err("还没连上辅测机，先点「连接」".into());
                }
                (
                    state.agent_host.clone(),
                    state.cfg.agent_port,
                    state.cfg.agent_token.clone(),
                )
            };
            spawn_agent_monitor(
                host,
                port,
                token,
                iface.clone(),
                interval_ms,
                session.clone(),
                &stop,
                &data,
            );
        }
        other => return Err(format!("未知的监控端: {other}")),
    }

    lock_recover(&console.monitors).insert(
        session.clone(),
        MonitorSession {
            side: req.side.clone(),
            iface,
            stop,
            data,
            started: std::time::Instant::now(),
        },
    );
    Ok(serde_json::json!({ "session": session }))
}

/// 监控端能接受的采样间隔上限。辅测机侧受 agent 自身的夹紧约束。
pub(crate) fn monitor_interval_ms(side: &str, requested: u64) -> u64 {
    let max = if side == "agent" { 5_000 } else { 60_000 };
    if requested == 0 {
        return 1_000;
    }
    requested.clamp(200, max)
}

fn api_monitor_samples(console: &Arc<Console>, query: &str) -> Result<serde_json::Value, String> {
    let session = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("session="))
        .map(urldecode)
        .ok_or("缺少 session")?;
    let from: usize = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("from="))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut monitors = lock_recover(&console.monitors);
    // 顺手收摊。页面轮询是这张表唯一的常规活动，回收挂在这里才不会
    // 依赖「有人再开一路监控」才发生。
    reap_dead_monitors(&mut monitors);
    let entry = monitors.get(&session).ok_or("监控会话已结束")?;
    let mut data = lock_recover(&entry.data);
    data.last_poll = Some(std::time::Instant::now());
    // 游标是绝对序号；被环形缓冲挤掉的部分直接跳过，不能装作它还在。
    let start = from.max(data.dropped) - data.dropped;
    let points: Vec<MonitorPoint> = data.points.iter().skip(start).cloned().collect();
    serde_json::to_value(MonitorSamplesOut {
        side: entry.side.clone(),
        iface: entry.iface.clone(),
        from: data.dropped + data.points.len(),
        points,
        running: data.running,
        error: data.error.clone().unwrap_or_default(),
    })
    .map_err(|e| e.to_string())
}

fn api_monitor_stop(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: MonitorSessionReq =
        serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let entry = lock_recover(&console.monitors).remove(&req.session);
    let Some(entry) = entry else {
        return Ok(serde_json::json!({ "stopped": false }));
    };
    entry.stop.store(true, Ordering::SeqCst);
    lock_recover(&entry.data).running = false;
    Ok(serde_json::json!({ "stopped": true }))
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 本机采样。
///
/// 不能复用 `nic::monitor::run_continuous`：那是给命令行写的，自带阻塞循环、
/// `ctrlc` 处理器和 `println!`——在控制台进程里注册 ctrlc 会和既有的
/// `crate::cancel` 抢同一个信号。这里只用它底层的计数器读取。
fn spawn_local_monitor(
    iface: String,
    interval_ms: u64,
    stop: &Arc<AtomicBool>,
    data: &Arc<Mutex<MonitorData>>,
) {
    let stop = Arc::clone(stop);
    let data = Arc::clone(data);
    let _ = std::thread::Builder::new()
        .name("cpe-ui-monitor-local".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let mut last = match crate::nic::monitor::read_counters(&iface) {
                Ok(counters) => (counters, std::time::Instant::now()),
                Err(error) => {
                    let mut d = lock_recover(&data);
                    d.error = Some(format!("读取网卡计数器失败：{error}"));
                    d.running = false;
                    return;
                }
            };
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(interval_ms));
                if stop.load(Ordering::SeqCst) || monitor_abandoned(&data, started) {
                    break;
                }
                let now = std::time::Instant::now();
                match crate::nic::monitor::read_counters(&iface) {
                    Ok((rx, tx)) => {
                        let secs = now.duration_since(last.1).as_secs_f64().max(1e-6);
                        // 计数器回绕/网卡重插会让差值变负，saturating_sub 会把它
                        // 压成 0——报 0 比报一个天文数字好，且下一拍就恢复。
                        let rx_mbps = (rx.saturating_sub(last.0 .0) as f64) * 8.0 / secs / 1e6;
                        let tx_mbps = (tx.saturating_sub(last.0 .1) as f64) * 8.0 / secs / 1e6;
                        last = ((rx, tx), now);
                        let mut d = lock_recover(&data);
                        d.error = None;
                        d.push(MonitorPoint {
                            t: started.elapsed().as_secs_f64(),
                            rx_mbps,
                            tx_mbps,
                        });
                    }
                    Err(error) => {
                        lock_recover(&data).error = Some(format!("采样失败：{error}"));
                    }
                }
            }
            lock_recover(&data).running = false;
        });
}

/// 辅测机采样：复用 agent 已有的 `/monitor/*`，只是换一个独立的 owner_id。
#[allow(clippy::too_many_arguments)]
fn spawn_agent_monitor(
    host: String,
    port: u16,
    token: String,
    iface: String,
    interval_ms: u64,
    session: String,
    stop: &Arc<AtomicBool>,
    data: &Arc<Mutex<MonitorData>>,
) {
    let stop = Arc::clone(stop);
    let data = Arc::clone(data);
    let _ = std::thread::Builder::new()
        .name("cpe-ui-monitor-agent".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            // owner_id 必须和测试用的那套区分开：主控收尾时按 owner 清理，
            // 共用一个 owner 就会在每轮测试结束时被顺手停掉。
            let owner_id = format!("ui-{session}");
            let start_body = serde_json::json!({
                "iface": iface,
                "interval_ms": interval_ms,
                "owner_id": owner_id,
                // 租约短，靠轮询续。agent 那边每次 /monitor/status 都会刷新
                // last_touch，所以只要这条线程还在轮询就续得上；控制台被 kill
                // 之后再没人来问，辅测机在一个租约周期内自己回收。
                // 给足余量：轮询间隔最大 60s，网络抖动再叠几拍也够不到 180s。
                "lease_secs": UI_MONITOR_LEASE_SECS,
            })
            .to_string();
            let id = match post::<crate::protocol::MonitorStartOut>(
                &host,
                port,
                "/monitor/start",
                &start_body,
                &token,
            ) {
                Ok(out) => out.id,
                Err(error) => {
                    let mut d = lock_recover(&data);
                    d.error = Some(format!("辅测机启动采样失败：{error}"));
                    d.running = false;
                    return;
                }
            };

            let status_body = serde_json::json!({ "id": id }).to_string();
            let mut last_elapsed_ms = u64::MAX;
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(interval_ms));
                if stop.load(Ordering::SeqCst) || monitor_abandoned(&data, started) {
                    break;
                }
                match post::<crate::protocol::MonitorStatusOut>(
                    &host,
                    port,
                    "/monitor/status",
                    &status_body,
                    &token,
                ) {
                    Ok(out) => {
                        let mut d = lock_recover(&data);
                        d.error = None;
                        if let Some(sample) = out.latest_sample {
                            // agent 自己按固定周期采样，这里的轮询和它并不同步，
                            // 同一个样本会被读到两次——按 elapsed_ms 去重。
                            if sample.elapsed_ms != last_elapsed_ms {
                                last_elapsed_ms = sample.elapsed_ms;
                                d.push(MonitorPoint {
                                    t: started.elapsed().as_secs_f64(),
                                    rx_mbps: sample.rx_mbps,
                                    tx_mbps: sample.tx_mbps,
                                });
                            }
                        }
                    }
                    Err(error) => {
                        lock_recover(&data).error = Some(format!("查询辅测机采样失败：{error}"));
                    }
                }
            }
            let stop_body = serde_json::json!({ "id": id }).to_string();
            let _ = post::<crate::protocol::MonitorStopOut>(
                &host,
                port,
                "/monitor/stop",
                &stop_body,
                &token,
            );
            lock_recover(&data).running = false;
        });
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
                rx_target_bidir_ab: String::new(),
                rx_target_bidir_ba: String::new(),
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
            ping_count: 0,
            ping_payload_sizes: Vec::new(),
            limit_udp_by_link_speed: false,
            resume: false,
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
            .contains("至少勾 TCP / UDP / PING"));
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
            ui_token: String::new(),
            monitors: Mutex::new(HashMap::new()),
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

    fn console_for_monitor_tests() -> Arc<Console> {
        Arc::new(Console {
            state: Mutex::new(state_with_pair()),
            running: AtomicBool::new(false),
            report: Mutex::new(String::new()),
            ui_token: String::new(),
            monitors: Mutex::new(HashMap::new()),
        })
    }

    /// 环形缓冲挤掉旧点之后，游标必须还指得对。
    ///
    /// 游标是**绝对**序号（和 /api/progress 的 from 一个语义）。前端拿着一个
    /// 早于缓冲起点的 from 回来时，正确做法是从现存最早的点接着给，
    /// 而不是把 from 当成数组下标去切——那会静默错位，曲线看着还挺像样。
    #[test]
    fn monitor_cursor_survives_the_ring_buffer_dropping_old_points() {
        let console = console_for_monitor_tests();
        let data = Arc::new(Mutex::new(MonitorData {
            running: true,
            ..Default::default()
        }));
        {
            let mut d = lock_recover(&data);
            for i in 0..(MONITOR_MAX_POINTS + 120) {
                d.push(MonitorPoint {
                    t: i as f64,
                    rx_mbps: i as f64,
                    tx_mbps: 0.0,
                });
            }
            assert_eq!(d.dropped, 120, "超出上限的点必须被挤掉并记数");
            assert_eq!(d.points.len(), MONITOR_MAX_POINTS);
        }
        lock_recover(&console.monitors).insert(
            "s1".into(),
            MonitorSession {
                side: "master".into(),
                iface: "eth0".into(),
                stop: Arc::new(AtomicBool::new(false)),
                data,
                started: std::time::Instant::now(),
            },
        );

        // from=0 的落后游标：从现存最早的点开始给，而不是从数组第 0 个。
        let out = api_monitor_samples(&console, "session=s1&from=0").unwrap();
        assert_eq!(
            out["points"][0]["rx_mbps"], 120.0,
            "第一个点应是未被挤掉的最早点"
        );
        assert_eq!(out["from"], (MONITOR_MAX_POINTS + 120) as u64);
        assert_eq!(
            out["points"].as_array().unwrap().len(),
            MONITOR_MAX_POINTS,
            "落后游标应拿到缓冲里现有的全部"
        );

        // 追平之后再问，应该一个点都没有。
        let out = api_monitor_samples(
            &console,
            &format!("session=s1&from={}", MONITOR_MAX_POINTS + 120),
        )
        .unwrap();
        assert!(
            out["points"].as_array().unwrap().is_empty(),
            "追平后不该重发"
        );

        api_monitor_stop(&console, r#"{"session":"s1"}"#).unwrap();
        assert!(
            api_monitor_samples(&console, "session=s1&from=0").is_err(),
            "停掉的会话应当直接报错，而不是给一份空数据装作还活着"
        );
        // 再停一次不能 panic：页面上快速点两下停止是常事。
        let again = api_monitor_stop(&console, r#"{"session":"s1"}"#).unwrap();
        assert_eq!(again["stopped"], false);
    }

    /// 采样间隔的上限跟着监控端走。
    ///
    /// agent 自己会把间隔夹到 200–5000ms，这边不跟着夹的话，选 10 秒会变成
    /// 「对面按 5 秒采、这边按 10 秒只取最后一个样本」——一半样本无声丢掉，
    /// 而同样选 10 秒监控本机却是对的。同一个输入框不能有两种语义。
    #[test]
    fn the_sampling_interval_ceiling_follows_which_side_is_being_watched() {
        assert_eq!(monitor_interval_ms("master", 0), 1_000, "0 = 用默认值");
        assert_eq!(monitor_interval_ms("agent", 0), 1_000);

        assert_eq!(
            monitor_interval_ms("master", 10_000),
            10_000,
            "本机自己 sleep，给多少是多少"
        );
        assert_eq!(
            monitor_interval_ms("agent", 10_000),
            5_000,
            "辅测机侧不能超过 agent 自己的夹紧上限"
        );
        assert_eq!(
            monitor_interval_ms("agent", 5_000),
            5_000,
            "正好在上限上要放行"
        );

        assert_eq!(monitor_interval_ms("master", 10), 200, "下限两端一致");
        assert_eq!(monitor_interval_ms("agent", 10), 200);
        assert_eq!(monitor_interval_ms("master", 999_999), 60_000);
    }

    /// 监控不受「有没有测试在跑」约束——边跑边看正是它最有用的场景。
    /// 同时确认网卡名不存在时是一条错误信息，不是 panic、也不是假装在测。
    #[test]
    fn monitoring_starts_while_a_run_is_in_flight_and_reports_a_bad_interface() {
        let console = console_for_monitor_tests();
        console.running.store(true, Ordering::SeqCst);

        let started = api_monitor_start(
            &console,
            r#"{"side":"master","iface":"cpe-no-such-iface","interval_ms":200}"#,
        )
        .expect("测试在跑也必须能起监控");
        let session = started["session"].as_str().unwrap().to_string();

        // 采样线程读不到计数器会立刻收摊并写下错误。
        let mut error = String::new();
        for _ in 0..50 {
            let out = api_monitor_samples(&console, &format!("session={session}&from=0")).unwrap();
            error = out["error"].as_str().unwrap_or_default().to_string();
            if !error.is_empty() && out["running"] == false {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        assert!(!error.is_empty(), "网卡名不存在必须给出可读的错误");

        api_monitor_stop(&console, &format!("{{\"session\":\"{session}\"}}")).unwrap();
    }

    /// 用真实 tiny_http + handle() 把口令闸门跑一遍。
    ///
    /// 重点是**页面本身也要挡**：`/` 返回的 HTML 里没有口令，但放行未认证的
    /// 页面请求就等于把控制台的整个界面（以及它能做什么）展示给任何来问的人，
    /// 而 API 401 之后界面只会是一屏报错——不如在门口就说清楚。
    #[test]
    fn the_console_token_gate_covers_both_the_page_and_the_api() {
        let console = Arc::new(Console {
            state: Mutex::new(state_with_pair()),
            running: AtomicBool::new(false),
            report: Mutex::new(String::new()),
            ui_token: "unit-secret".into(),
            monitors: Mutex::new(HashMap::new()),
        });
        // Server 要留在外面：incoming_requests() 会一直阻塞，只有 unblock()
        // 能让它收场。整个 move 进线程就再也够不着它了，端口和线程会挂到
        // 测试进程结束。
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let worker = Arc::clone(&console);
        let worker_server = Arc::clone(&server);
        let thread = std::thread::spawn(move || {
            for request in worker_server.incoming_requests() {
                handle(request, &worker);
            }
        });
        let wait = Duration::from_secs(5);

        let (status, _) = crate::http_client::get("127.0.0.1", port, "/", wait).unwrap();
        assert_eq!(status, 401, "页面本身也必须要口令");

        let (status, _) = crate::http_client::get("127.0.0.1", port, "/api/local", wait).unwrap();
        assert_eq!(status, 401, "不带口令的 API 必须 401");

        let (status, _) =
            crate::http_client::get_auth("127.0.0.1", port, "/api/local", "wrong", wait).unwrap();
        assert_eq!(status, 401, "口令错必须 401");

        let (status, body) =
            crate::http_client::get("127.0.0.1", port, "/api/local?token=unit-secret", wait)
                .unwrap();
        assert_eq!(status, 200, "查询串带对口令必须放行：这是浏览器唯一的入口");
        assert!(body.contains("\"ok\":true"), "{body}");

        let (status, _) =
            crate::http_client::get_auth("127.0.0.1", port, "/api/local", "unit-secret", wait)
                .unwrap();
        assert_eq!(status, 200, "Bearer 带对口令必须放行");

        server.unblock();
        thread.join().expect("请求线程正常收场");
    }

    /// Ctrl+C 之后要不要退，取决于「这一刻有没有测试在跑」。
    ///
    /// 跑着的时候退掉就是把报告扔了：那次 Ctrl+C 的语义是「优雅结束当前单元
    /// 并出报告」，控制台得活到 run_master 写完。等它收完尾再退。
    #[test]
    fn the_console_only_quits_once_the_run_it_was_hosting_has_wound_down() {
        assert!(!should_shut_down(false, false), "没按 Ctrl+C 就不该退");
        assert!(!should_shut_down(false, true), "没按 Ctrl+C 更不该退");
        assert!(
            !should_shut_down(true, true),
            "测试还在跑：先让它把报告写完，这一拍不能退"
        );
        assert!(should_shut_down(true, false), "空闲时按 Ctrl+C 必须退出");
    }

    /// 取请求循环必须有出口。
    ///
    /// 原来是 `while let Ok(request) = server.recv()`——没有超时、不查任何标志，
    /// 于是 `run_master()` 一旦把 ctrlc handler 装上（它只置标志、不退进程），
    /// SIGINT 就被永久吃掉，控制台再也关不掉。这条用真 server 跑一遍，
    /// 确认置位之后循环真的会返回，而不是靠「应该会吧」。
    #[test]
    fn the_request_loop_returns_once_the_shutdown_flag_is_set() {
        let console = console_for_monitor_tests();
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_server = Arc::clone(&server);
        let worker_console = Arc::clone(&console);
        let worker_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            serve(&worker_server, &worker_console, &worker_shutdown);
        });

        // 没有任何请求进来，循环应当停在 recv_timeout 上空转而不是退出。
        std::thread::sleep(SHUTDOWN_POLL * 3);
        assert!(!thread.is_finished(), "没置位就不该自己退出");

        shutdown.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !thread.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(thread.is_finished(), "置位后必须在一个轮询周期内返回");
        thread.join().expect("请求线程正常收场");
    }

    /// 进程退出前要把监控会话收干净，尤其是辅测机侧那路。
    #[test]
    fn shutting_down_stops_every_monitor_session() {
        let console = console_for_monitor_tests();
        for name in ["a", "b"] {
            lock_recover(&console.monitors).insert(
                name.into(),
                MonitorSession {
                    side: "master".into(),
                    iface: "eth0".into(),
                    stop: Arc::new(AtomicBool::new(false)),
                    data: Arc::new(Mutex::new(MonitorData {
                        running: true,
                        ..Default::default()
                    })),
                    started: std::time::Instant::now(),
                },
            );
        }

        stop_all_monitors(&console);

        assert!(
            lock_recover(&console.monitors).is_empty(),
            "退出时不能留下任何会话"
        );
    }

    /// 只有明确写成回环的地址才算回环——判错方向要往「多要一个口令」偏，
    /// 反过来把一个真的可路由的地址当成回环，就是无声开洞。
    #[test]
    fn only_explicit_loopback_addresses_count_as_local() {
        for local in [
            "127.0.0.1",
            "127.1.2.3",
            "localhost",
            "LOCALHOST",
            "::1",
            " 127.0.0.1 ",
        ] {
            assert!(bind_is_loopback(local), "{local} 应判为回环");
        }
        for exposed in ["0.0.0.0", "192.168.8.101", "::", "10.0.0.1", ""] {
            assert!(!bind_is_loopback(exposed), "{exposed} 不该判为回环");
        }
    }

    /// 三种带口令的方式都要认；没配口令时一律放行（回环下的默认形态）。
    #[test]
    fn the_console_accepts_its_token_from_query_header_or_bearer() {
        assert!(
            request_is_authorized("", "", None, None),
            "没设口令就不该拦任何人"
        );

        let ok = |query: &str, header: Option<&str>, bearer: Option<&str>| {
            request_is_authorized("s3cr3t", query, header, bearer)
        };
        assert!(ok("token=s3cr3t", None, None), "地址栏里只能靠查询串");
        assert!(ok("from=3&token=s3cr3t", None, None), "查询串里位置不固定");
        assert!(ok("", Some("s3cr3t"), None), "页面之后走请求头");
        assert!(ok("", None, Some("s3cr3t")), "curl 复现问题时走 Bearer");

        assert!(!ok("", None, None), "什么都不带必须拒绝");
        assert!(!ok("token=wrong", None, None), "口令错必须拒绝");
        assert!(!ok("", Some("wrong"), None));
        assert!(!ok("mytoken=s3cr3t", None, None), "后缀撞名不算带对口令");
    }

    /// 畸形的百分号转义不能让控制台崩掉——这段输入来自网络。
    ///
    /// `%` 后面跟多字节字符时，按 `&str` 下标切那两位会切在字符中间直接 panic；
    /// 这里逐条钉住几种畸形写法都只是「原样留下那个 %」。
    #[test]
    fn a_malformed_percent_escape_never_panics_the_query_parser() {
        for raw in ["%", "%4", "%中文", "%zz", "abc%", "%%41", "中%文字"] {
            let decoded = urldecode(raw);
            assert!(!decoded.is_empty(), "{raw} 不该解出空串");
        }
        assert_eq!(urldecode("%41%42"), "AB");
        assert_eq!(urldecode("a+b"), "a b");
        assert_eq!(urldecode("%zz"), "%zz", "解不动的转义原样保留");
    }

    /// 口令里的特殊字符经过 URL 编码往返后必须还是同一个串，
    /// 否则「照着打印的地址打开」会打不开。
    #[test]
    fn a_token_with_awkward_characters_survives_the_printed_url() {
        let token = "a b&c=d%e/f中文";
        let encoded = urlencode(token);
        assert!(!encoded.contains(' ') && !encoded.contains('&'));
        assert!(request_is_authorized(
            token,
            &format!("token={encoded}"),
            None,
            None
        ));
    }

    /// 矩阵里勾 PING 必须真的产出 ping 单元，而且次数/包长走界面填的值。
    ///
    /// 界面把 PING 和 TCP/UDP 并排放在「协议」列，但配置模型里它是 `kinds`
    /// 不是 `transports`；这条用例同时钉住这层映射和「只勾 PING 时不冒出
    /// iperf 单元」。
    #[test]
    fn checking_ping_in_the_matrix_produces_ping_units_with_the_typed_budget() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].transports = vec!["ping".into()];
        req.ping_count = 5;
        req.ping_payload_sizes = vec![64, 1400];

        // 走完整链路而不是直接调 config_from_request：这条测试曾经是绿的，
        // 而 PING 在 validate_request 那一关整个被挡住——绕过校验的测试
        // 保不住「勾了 PING 真的能跑」这件事。
        let cfg = validated_config_from_request(&state, &req).expect("勾 PING 必须能过校验");
        let ping: Vec<_> = cfg
            .tests
            .iter()
            .filter(|t| t.kinds.iter().any(|k| k == "ping"))
            .collect();
        assert_eq!(ping.len(), 1, "勾了 PING 就该有一个 ping 测试项");
        assert_eq!(ping[0].ping_count, Some(5), "次数必须用界面填的");
        assert_eq!(
            ping[0].ping_payload_sizes.as_deref(),
            Some(&[64u32, 1400][..]),
            "包长档位必须用界面填的，回落到默认的三档会平白多跑几分钟"
        );
        assert!(ping[0].transports.is_empty(), "ping 单元不带 transport");
        assert!(
            cfg.tests
                .iter()
                .all(|t| !t.kinds.iter().any(|k| k == "iperf")),
            "只勾 PING 时不该冒出 iperf 单元"
        );
        ensure_config_builds_units(&cfg, &state).expect("ping 选择必须能构建出单元");
    }

    /// 双向门限按配对填，只在勾了「双向」时落进 config。
    ///
    /// 按网卡填是不够的：同一块 RNDIS 口，和 Wi-Fi 组双向、和 SGMII 组双向，
    /// 能收到的速率完全不是一个量级——一个数没法同时对两组成立。
    #[test]
    fn the_bidirectional_threshold_is_per_pair_and_only_applies_to_bidirectional_units() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].directions = vec!["ab".into(), "bidir".into()];
        req.pairs[0].rx_target_bidir_ab = "1000".into();
        req.pairs[0].rx_target_bidir_ba = "800".into();

        let cfg = validated_config_from_request(&state, &req).expect("应能过校验");
        let targets = cfg.tests[0]
            .rate_targets_bidir_mbps
            .as_ref()
            .expect("勾了双向且填了值就该落进 config");
        assert_eq!(targets.ab, Some(1000.0));
        assert_eq!(targets.ba, Some(800.0));
        assert_eq!(targets.forward, None, "双向门限没有 forward 这个概念");

        // 没勾双向时不写进 config——否则它会出现在下载的 config.json 里，
        // 让人以为在生效。
        let mut one_way = request();
        one_way.pairs[0].directions = vec!["ab".into()];
        assert!(config_from_request(&state, &one_way).tests[0]
            .rate_targets_bidir_mbps
            .is_none());
    }

    /// 填了双向门限却没勾双向，要当场报错而不是静默忽略。
    ///
    /// 静默忽略的后果是：人以为门限已经放低，看到 FAIL 就去查链路，
    /// 而真正的原因是那个数从头到尾没生效过。
    #[test]
    fn a_bidirectional_threshold_without_the_bidirectional_box_is_rejected() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].directions = vec!["ab".into()];
        req.pairs[0].rx_target_bidir_ab = "1000".into();

        let error = validate_request(&state, &req).expect_err("必须报错");
        assert!(error.contains("双向"), "{error}");
    }

    /// 双向门限只收绝对 Mbps：百分比按单块网卡的协商速率换算，
    /// 而它说的是两块口并发时的能力，两者不成比例。
    #[test]
    fn a_percentage_bidirectional_threshold_is_rejected_with_the_reason() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].directions = vec!["bidir".into()];
        req.pairs[0].rx_target_bidir_ab = "50%".into();

        let error = validate_request(&state, &req).expect_err("百分比必须被拒");
        assert!(error.contains("绝对 Mbps"), "错误要说清为什么：{error}");

        req.pairs[0].rx_target_bidir_ab = "1000".into();
        assert!(validate_request(&state, &req).is_ok(), "绝对值要放行");
    }

    /// PING 必须能过 `validate_request`——单独勾、和 TCP/UDP 一起勾都算。
    ///
    /// 白名单曾经只写了 tcp/udp，而 `values_are_allowed` 要求**每一项**都在集合里：
    /// 勾上 PING 不是「PING 跑不了」，是整份请求作废，连同一配对里本来能跑的
    /// TCP/UDP 一起废掉，页面上还提示人去勾 TCP 或 UDP。
    #[test]
    fn checking_ping_passes_validation_alone_and_alongside_tcp_udp() {
        let state = state_with_pair();
        for transports in [
            vec!["ping".to_string()],
            vec!["tcp".to_string(), "udp".to_string(), "ping".to_string()],
        ] {
            let mut req = request();
            req.pairs[0].transports = transports.clone();
            if let Err(error) = validate_request(&state, &req) {
                panic!("{transports:?} 必须通过校验，却被拒：{error}");
            }
        }

        let mut bogus = request();
        bogus.pairs[0].transports = vec!["icmp".into()];
        assert!(
            validate_request(&state, &bogus).is_err(),
            "白名单之外的写法仍要挡住"
        );
    }

    /// 包长档位要保序去重。`dedup()` 只合并相邻项：「32 1600 32」会漏过去，
    /// 两个 32 的单元标题和 resume id 一模一样，在结果库里互相覆盖，
    /// 还白跑一遍全程。
    #[test]
    fn repeated_ping_payload_sizes_collapse_even_when_not_adjacent() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].transports = vec!["ping".into()];
        req.ping_payload_sizes = vec![1600, 32, 1600, 0, 32];

        let cfg = config_from_request(&state, &req);
        let ping = cfg
            .tests
            .iter()
            .find(|t| t.kinds.iter().any(|k| k == "ping"))
            .expect("应有 ping 测试项");
        assert_eq!(
            ping.ping_payload_sizes.as_deref(),
            Some(&[1600u32, 32][..]),
            "重复档位只留一份，且保持用户填的顺序"
        );
    }

    /// 越界包长要当场拒绝，不能留给 `ping::build` 悄悄夹紧。
    ///
    /// 夹紧发生在分单元之后：65500 和 100000 会变成两个 resume id 不同、
    /// 跑起来完全一样的单元，报告上却各自写着自己那个 `-l`。
    #[test]
    fn an_oversized_ping_budget_is_rejected_before_starting_a_run() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].transports = vec!["ping".into()];

        req.ping_payload_sizes = vec![32, crate::ping::MAX_PAYLOAD + 1];
        let error = validate_request(&state, &req).expect_err("越界包长必须被拒");
        assert!(error.contains("65500"), "错误里要写清上限：{error}");

        req.ping_payload_sizes = vec![crate::ping::MAX_PAYLOAD];
        assert!(validate_request(&state, &req).is_ok(), "正好在上限上要放行");

        req.ping_count = 100_001;
        assert!(
            validate_request(&state, &req).is_err(),
            "次数同样会被 builder 静默夹紧，也要在这里挡住"
        );
    }

    /// 裸 IPv6 要补方括号才拼得出监听地址。
    ///
    /// `bind_is_loopback` 是认 `"::1"` 的，不补的话「判定放行 → 监听失败」
    /// 这条路走得通，人只会看到一句莫名其妙的启动错误。
    #[test]
    fn ipv6_binds_get_bracketed_before_they_reach_the_listener() {
        assert_eq!(listen_addr("127.0.0.1", 28800), "127.0.0.1:28800");
        assert_eq!(listen_addr("0.0.0.0", 28800), "0.0.0.0:28800");
        assert_eq!(listen_addr("::1", 28800), "[::1]:28800");
        assert_eq!(
            listen_addr("[::1]", 28800),
            "[::1]:28800",
            "已经带括号的不重复加"
        );
        assert_eq!(listen_addr("::", 28800), "[::]:28800");
        assert_eq!(listen_addr(" ::1 ", 28800), "[::1]:28800");
        for bind in ["127.0.0.1", "0.0.0.0", "::1", "[::1]", "::"] {
            let addr = listen_addr(bind, 28800);
            addr.parse::<std::net::SocketAddr>()
                .unwrap_or_else(|error| panic!("{bind} 拼出的 {addr} 必须能解析：{error}"));
        }
    }

    /// 定长时间比较不能顺手把「相等」判错。
    #[test]
    fn the_constant_time_compare_still_agrees_with_equality() {
        assert!(secret_eq("s3cret", "s3cret"));
        assert!(!secret_eq("s3cret", "s3creT"));
        assert!(!secret_eq("s3cret", "s3cre"), "短一截也不算对");
        assert!(!secret_eq("s3cret", ""));
        assert!(secret_eq("", ""));
        assert!(secret_eq("口令", "口令"), "多字节口令按字节比也要相等");
    }

    /// 关掉浏览器标签页不会通知服务端，所以「显式 stop」不能是会话表唯一的出口。
    ///
    /// 采样线程自己会收摊，但它只结束线程；会话连同那个 7200 点的缓冲会一直
    /// 留在表里，刷新一次页面就多一条。
    #[test]
    fn monitor_sessions_whose_page_went_away_get_reaped() {
        let stale = std::time::Instant::now() - (MONITOR_IDLE_TIMEOUT + Duration::from_secs(1));
        let now = std::time::Instant::now();
        let session =
            |running: bool, last_poll: Option<std::time::Instant>, started| MonitorSession {
                side: "master".into(),
                iface: "eth0".into(),
                stop: Arc::new(AtomicBool::new(false)),
                data: Arc::new(Mutex::new(MonitorData {
                    running,
                    last_poll,
                    ..Default::default()
                })),
                started,
            };
        let mut monitors: HashMap<String, MonitorSession> = HashMap::new();
        // 线程还在跑：哪怕页面很久没来取，也由采样线程自己决定什么时候停。
        monitors.insert("live".into(), session(true, Some(stale), stale));
        // 线程刚停，页面还在轮询：留着让它把「已停止」读走并正常收尾。
        monitors.insert("just-stopped".into(), session(false, Some(now), now));
        // 线程停了、页面也早就不来了：这条只剩内存占用。
        monitors.insert("abandoned".into(), session(false, Some(stale), stale));
        // 一次都没被取过样本，且开出来已经很久：页面开完就被关掉了。
        monitors.insert("never-polled".into(), session(false, None, stale));
        // 刚刚开出来还没轮到第一次轮询：不能误伤。
        monitors.insert("starting".into(), session(false, None, now));

        reap_dead_monitors(&mut monitors);

        let mut left: Vec<&str> = monitors.keys().map(|k| k.as_str()).collect();
        left.sort_unstable();
        assert_eq!(left, ["just-stopped", "live", "starting"]);
    }

    /// 会话数有上限，而且要在**起线程之前**判。
    ///
    /// 控制台一旦 `--ui-bind` 出去，一个拿到口令的客户端循环调
    /// /api/monitor/start 就能一路撑起线程和辅测机侧的 monitor 资源。
    #[test]
    fn the_console_refuses_to_pile_up_monitor_sessions() {
        let console = console_for_monitor_tests();
        {
            let mut monitors = lock_recover(&console.monitors);
            for idx in 0..MONITOR_MAX_SESSIONS {
                monitors.insert(
                    format!("s{idx}"),
                    MonitorSession {
                        side: "master".into(),
                        iface: "eth0".into(),
                        stop: Arc::new(AtomicBool::new(false)),
                        data: Arc::new(Mutex::new(MonitorData {
                            running: true,
                            ..Default::default()
                        })),
                        started: std::time::Instant::now(),
                    },
                );
            }
        }
        let error = api_monitor_start(&console, r#"{"side":"master","iface":"eth0"}"#)
            .expect_err("撞上限必须直接拒绝，而不是再起一条线程");
        assert!(error.contains("最多"), "{error}");
        assert_eq!(
            lock_recover(&console.monitors).len(),
            MONITOR_MAX_SESSIONS,
            "被拒的那次不能留下任何痕迹"
        );
    }

    /// 临时 config 里带着 agent_token，而 /tmp 是全局可读的。
    #[cfg(unix)]
    #[test]
    fn the_temp_run_config_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "cpe_ui_private_{}_{}.json",
            std::process::id(),
            now_millis()
        ));
        // 先摆一个 0644 的残file：`mode()` 只在创建时生效，沿用旧权限就等于没修。
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private_config(&path, r#"{"agent_token":"s3cret"}"#).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let body = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(mode, 0o600, "同机的别人不能读到 agent_token");
        assert_eq!(
            body, r#"{"agent_token":"s3cret"}"#,
            "权限之外内容要原样写进去"
        );
    }

    /// resume 的跳过只是预判，页面要同时拿得到「全跳过」和「全实跑」两个数。
    #[test]
    fn the_plan_reports_both_ends_of_the_resume_estimate() {
        let console = console_for_monitor_tests();
        let body = serde_json::json!({
            "pairs": [{
                "src": "master:NAME=以太网 6",
                "dst": "agent:NAME=WLAN 3",
                "directions": ["ab"],
                "transports": ["tcp"],
                "ip": ["v4"],
            }],
            "duration": 60,
            "tcp_windows": ["2m"],
            "tcp_streams": [1],
            "udp_streams": 1,
            "resume": false,
        })
        .to_string();

        let out = api_plan(&console, &body).expect("计划必须生成");
        let total = out["est_total_secs"].as_u64().expect("est_total_secs");
        let full = out["est_full_secs"].as_u64().expect("est_full_secs");
        assert!(total > 0, "至少要有一个单元");
        assert_eq!(total, full, "没开 resume 时两个数必须一致");
    }

    /// resume 和裁剪开关同理：界面上的勾选是唯一来源，配置文件里的值不参与。
    ///
    /// 这一条以前是控制台唯一没暴露、却又会悄悄生效的配置项——config.json 里
    /// 写了 `resume: true`，界面上既看不见也关不掉。
    #[test]
    fn the_console_decides_resume_regardless_of_the_config_file() {
        let mut state = state_with_pair();
        state.cfg.resume = true;

        let req = request();
        assert!(
            !config_from_request(&state, &req).resume,
            "界面没勾就不跳过，配置文件里的 true 不能悄悄生效"
        );

        let mut on = request();
        on.resume = true;
        assert!(config_from_request(&state, &on).resume);
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
