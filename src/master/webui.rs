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
use crate::util::{clear_log_mirror, lock_recover, log_tail_since, md5_hex};
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// 这一行要跑哪几组 UDP 参数。`0` = 默认组，`1..` 指 `RunRequest::udp_groups`
    /// 里的第 n-1 组。空列表 = 只跑默认组（老页面/手写请求不带这个字段时的行为）。
    ///
    /// 用「选组」而不是「逐格覆盖」：覆盖是差量语义，每个留空的格子都要回头
    /// 推理「这一格空着等于继承谁」，四个格子就是四次推理，而填错了在界面上
    /// 看不出来。一个组是一份完整定义——选中哪组，跑的就是那组里写着的东西。
    ///
    /// 能**多选**是因为「同一对网口既按常规档位跑一遍、又用 1m 单流跑一遍」是
    /// 一件正经事：矩阵里一对网口只有一行，不能多选就只能分两轮跑、出两份报告。
    /// 每多选一组就多一批单元。
    #[serde(default)]
    udp_groups: Vec<usize>,
    /// 这一行要跑哪几组 TCP 参数。语义和 `udp_groups` 一样：`0` = 默认组
    /// （执行区的 `tcp_windows` / `tcp_streams`），`1..` 指 `RunRequest::tcp_groups`
    /// 里的第 n-1 组。空列表 = 只跑默认组。
    #[serde(default)]
    tcp_groups: Vec<usize>,
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

/// 一组 UDP 参数。**自成一体，不继承默认组**：`-l` 留空就是不下发 `-l`，
/// 而不是「跟着执行区那格走」。
///
/// 「有几对带 `-l`、另外几对不带」就是靠这一点表达的：需要的那几行选一个填了
/// `-l` 的组，其余行留在默认组。反过来（默认组填了、某一行想明确不要）在这个
/// 模型里表达不了，也不需要——把它倒过来写就是了。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UdpGroup {
    /// 显示名。空则页面按序号叫「组 2」「组 3」。
    #[serde(default)]
    name: String,
    /// 单流带宽档位，逐档各跑一轮。新建的组必须填，否则这组一个单元都生成不出来。
    #[serde(default)]
    bandwidths: Vec<String>,
    #[serde(default)]
    lengths: Vec<String>,
    #[serde(default)]
    windows: Vec<String>,
    /// 并发流数；0 视作 1（不继承默认组，理由见结构体注释）。
    #[serde(default)]
    streams: u32,
}

/// 一组 TCP 参数。和 `UdpGroup` 一样自成一体、不继承默认组：`-w` 留空就是
/// **不下发 `-w`**（用 iperf3 默认窗口），不是「跟着执行区那格走」。
///
/// 两个轴 `-w × -P` 取叉积，各成一个测试单元——这和默认组（执行区的
/// `tcp_windows` × `tcp_streams`）是同一套展开，只是换了一份档位。没有像
/// `UdpGroup` 那样的必填项：`-w`、`-P` 都留空就是最朴素的一条 TCP（默认窗口、
/// 单流），仍是一个合法的组。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TcpGroup {
    /// 显示名。空则页面按序号叫「组 2」「组 3」。
    #[serde(default)]
    name: String,
    /// socket buffer 档位（`-w`），逐档各跑一轮。空列表 = 不下发 `-w`。
    #[serde(default)]
    windows: Vec<String>,
    /// 并发流数档位（`-P`），逐档各跑一轮。空列表按 `[1]` 处理（等价单流，
    /// 和默认组 `tcp_streams` 留空时一致）。
    #[serde(default)]
    streams: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Quick-plan (suite) request model
// ---------------------------------------------------------------------------
// The legacy matrix request above remains supported.  These DTOs model the
// lower-dimensional planner: concrete endpoint pairs are grouped into link
// sets, protocol tasks live in suites, and bindings assign suites to sets.

/// A scalar-or-array integer accepted by recipe JSON.  A scalar is convenient
/// for a single fixed profile; an array denotes a scan axis.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum UiU32Values {
    One(u32),
    Many(Vec<u32>),
}

impl Default for UiU32Values {
    fn default() -> Self {
        Self::Many(Vec::new())
    }
}

impl UiU32Values {
    fn values(&self) -> Vec<u32> {
        match self {
            Self::One(value) => vec![*value],
            Self::Many(values) => values.clone(),
        }
    }
}

/// One complete TCP/UDP recipe profile.  Irrelevant fields are ignored for a
/// given protocol.  `streams` can be scalar or array for fixed/scan recipes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct UiRecipeProfile {
    window: Option<String>,
    length: Option<String>,
    bandwidth: Option<String>,
    streams: UiU32Values,
    tcp_streams: Option<UiU32Values>,
    udp_streams: Option<UiU32Values>,
}

/// A recipe may use complete `profiles`, or the axis fields below.  The
/// compiler expands axes explicitly and never crosses TCP with UDP.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct UiRecipe {
    id: String,
    name: String,
    mode: String,
    profiles: Vec<UiRecipeProfile>,
    tcp_windows: Vec<String>,
    tcp_streams: Vec<u32>,
    bandwidths: Vec<String>,
    lengths: Vec<String>,
    windows: Vec<String>,
    udp_streams: Vec<u32>,
    udp_profiles: Vec<UdpProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct UiRecipes {
    tcp: Vec<UiRecipe>,
    udp: Vec<UiRecipe>,
    #[serde(alias = "pings")]
    ping: Vec<UiRecipe>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct UiPairRef {
    id: String,
    src: String,
    dst: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct UiLinkSet {
    id: String,
    name: String,
    #[serde(alias = "pairs")]
    pair_refs: Vec<UiPairRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct UiTask {
    id: String,
    name: String,
    protocol: String,
    #[serde(alias = "transport")]
    transports: Vec<String>,
    directions: Vec<String>,
    ip: Vec<String>,
    #[serde(alias = "recipe_ids", alias = "recipes")]
    recipe_ids: Vec<String>,
    rx_target_bidir_ab: String,
    rx_target_bidir_ba: String,
    rate_targets_mbps: Option<crate::config::RateTargets>,
    rate_mode: Option<crate::config::RateMode>,
    duration: Option<u64>,
    ping_count: Option<u32>,
    ping_payload_sizes: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct UiSuite {
    id: String,
    name: String,
    #[serde(default)]
    note: String,
    execution: String,
    #[serde(alias = "lane_order", alias = "task_order")]
    order: Vec<String>,
    #[serde(alias = "lanes")]
    tasks: Vec<UiTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct UiBinding {
    id: String,
    link_set_id: String,
    suite_id: String,
    mode: String,
    order: i64,
    #[serde(alias = "pair_ids", alias = "pair_ref_ids")]
    pair_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct UiPlan {
    #[serde(alias = "version")]
    ui_plan_version: u32,
    link_sets: Vec<UiLinkSet>,
    recipes: UiRecipes,
    suites: Vec<UiSuite>,
    bindings: Vec<UiBinding>,
    /// Hash returned by `/api/plan`; excluded while calculating a fresh hash.
    plan_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// 默认组之外的 UDP 参数组。矩阵里 `udp_group = 1` 指的是这里的第 0 项。
    ///
    /// 默认组不放进这个列表：它就是执行区那几个输入框，页面上一直都在，
    /// 单独存一份只会多一处要保持同步的地方。
    #[serde(default)]
    udp_groups: Vec<UdpGroup>,
    /// 默认组之外的 TCP 参数组。矩阵里 `tcp_group = 1` 指的是这里的第 0 项。
    ///
    /// 默认组不放进这个列表：它就是 `tcp_windows` / `tcp_streams` 那两个框，
    /// 和 UDP 默认组同理，单独存一份只会多一处要保持同步的地方。
    #[serde(default)]
    tcp_groups: Vec<TcpGroup>,
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
    /// New suite-plan request.  It is mutually exclusive with legacy `pairs`.
    #[serde(default)]
    ui_plan: Option<UiPlan>,
    /// Optional hash returned by `/api/plan`; checked by `/api/run`.
    #[serde(default)]
    plan_hash: Option<String>,
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
    /// Feature flag for pages that can send the suite-oriented `ui_plan` DTO.
    ui_plan_supported: bool,
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
    /// 这个单元每条腿**最终**下发的参数。
    ///
    /// 「网口固定值 > 参数组 > 默认组」这条优先级，与其让人背下来，不如在跑
    /// 之前把每条腿的最终数字摆出来：填错了当场看得见，比任何校验都直接。
    /// 而且这里是裁剪之后的值——勾了「按链路上限裁剪」时 `-b` 和流数都可能
    /// 和填进去的不一样。
    load: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PlanOut {
    units: Vec<PlannedUnit>,
    /// 预计跳过的都真跳过时的耗时。
    est_total_secs: u64,
    /// 一个都不跳时的耗时。开着 resume 时页面按区间显示，理由见 `api_plan`。
    est_full_secs: u64,
    notices: Vec<String>,
    /// Hierarchical source information for the quick planner.  Empty for a
    /// legacy matrix request; the flat `units` field remains the compatibility
    /// contract used by the original page.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sections: Vec<PlanSection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trace: Vec<PlanTrace>,
    /// Stable hash of the request, effective config and current topology.
    #[serde(skip_serializing_if = "Option::is_none")]
    plan_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    topology_fingerprint: Option<String>,
    /// Lets newer pages feature-detect the suite planner without probing a
    /// deliberately invalid request.
    ui_plan_supported: bool,
}

#[derive(Debug, Clone, Serialize)]
struct PlanTrace {
    seq: usize,
    pair_id: Option<String>,
    link_set_id: Option<String>,
    suite_id: Option<String>,
    task_id: Option<String>,
    /// Alias used by the initial design terminology; equal to `task_id`.
    lane_id: Option<String>,
    recipe_id: Option<String>,
    protocol: Option<String>,
    direction: Option<String>,
    ip: Option<String>,
    requested_args: Vec<String>,
    effective_args: Vec<String>,
    value_sources: Vec<String>,
    skipped_reason: Option<String>,
    resumed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct PlanSection {
    link_set_id: Option<String>,
    suite_id: Option<String>,
    task_id: Option<String>,
    title: String,
    unit_seqs: Vec<usize>,
}

#[derive(Debug, Clone)]
struct UiSource {
    pair_id: String,
    link_set_id: String,
    suite_id: String,
    task_id: String,
    recipe_id: String,
    protocol: String,
}

struct CompiledPlan {
    cfg: Config,
    units: Vec<builder::Unit>,
    notices: Vec<String>,
    resumed: Vec<bool>,
    trace: Vec<PlanTrace>,
    sections: Vec<PlanSection>,
    plan_hash: String,
    topology_fingerprint: String,
    spec_errors: Vec<String>,
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

/// 把绑定地址拼成**能在浏览器里打开**的地址。
///
/// 监听地址不等于访问地址：`0.0.0.0` 和 `::` 是「所有网卡」的通配写法，不是
/// 一个能连的目的地址。此前打印和自动弹出的都是监听地址原文，于是 `--ui-bind
/// 0.0.0.0` 弹出来的是 `http://0.0.0.0:28800?token=…`——Chrome 133 起为堵
/// 「0.0.0.0 day」直接拦掉对该地址的请求，其余浏览器靠「碰巧路由到回环」才打
/// 得开；而口令就在那串 URL 里，人得先看懂要把主机名换掉才能进得去。
///
/// 通配地址换成对应的回环，其余原样保留（绑到某块网卡的 IP 时，那个 IP 本来
/// 就是该用的访问地址）。
pub(crate) fn display_addr(bind: &str, port: u16) -> String {
    let host = match bind.trim() {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        other => other,
    };
    listen_addr(host, port)
}

/// 绑定地址是否是「所有网卡」的通配写法。
pub(crate) fn bind_is_wildcard(bind: &str) -> bool {
    matches!(bind.trim(), "0.0.0.0" | "::" | "[::]")
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
    //
    // 地址用 `display_addr` 而不是监听地址原文：`0.0.0.0` 打不开（见那个函数
    // 的注释），而这一行同时是自动弹窗和「手动复制」两条路的唯一出处。
    let open_addr = display_addr(&bind, port);
    let query = if ui_token.is_empty() {
        String::new()
    } else {
        format!("?token={}", urlencode(&ui_token))
    };
    let url = format!("http://{open_addr}{query}");
    println!("控制台已启动: {url}");
    println!("（浏览器没自动弹出的话，手动复制上面这个地址打开）");
    if bind_is_wildcard(&bind) {
        // 通配绑定的用意基本都是「让别的电脑连过来」，所以把远端要用的写法
        // 一起给出来：上面那个回环地址只在本机有效，照抄到别的电脑上打不开。
        println!("从别的电脑访问：把上面地址里的主机名换成本机的测试网 IP，端口和 ?token= 照抄。");
    }
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
    } else if is_post && path == "/api/import" {
        api_import(console, &body)
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
    } else if is_post && path == "/api/monitor/samples" {
        api_monitor_samples(console, &body)
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
    serde_json::to_value(bootstrap_out(&state)).map_err(|error| error.to_string())
}

/// 顶部参数区的回填值。打开页面（`/api/bootstrap`）和导入 config
/// （`/api/import`）共用这一份，两条路填出来的输入框必须一模一样。
fn bootstrap_out(state: &UiState) -> BootstrapOut {
    // 默认组的 -P 档位 = **跑默认 -w 档位的那些 TCP test** 的流数集合。
    //
    // 和下面 udp_streams 同一个坑：不能把所有 TCP test 的流数并起来。矩阵里选了
    // 别的 TCP 参数组的行，它们的流数排在前面，会被当成默认组的填进执行区那一格
    // ——于是导进来的默认组变成另一份东西，而它管着所有没选组的行。默认组的
    // -w 存在 `iperf.tcp_windows` 里（附加组各带各的 tcp_windows），按它筛。
    let default_windows = &state.cfg.iperf.tcp_windows;
    let mut tcp_streams: Vec<u32> = state
        .cfg
        .tests
        .iter()
        .filter(|test| test.transports.iter().any(|t| t.trim() == "tcp"))
        .filter(|test| {
            test.tcp_windows
                .as_ref()
                .is_none_or(|windows| windows == default_windows)
        })
        .filter_map(|test| test.tcp_streams)
        .filter(|value| *value > 0)
        .collect();
    tcp_streams.sort_unstable();
    tcp_streams.dedup();
    if tcp_streams.is_empty() {
        tcp_streams.push(10);
    }
    // 默认组的流数 = **跑默认档位的那条 UDP test** 的流数。
    //
    // 不能只取「第一条带 udp_streams 的 test」：矩阵里某一行选了别的参数组时，
    // 它的 test 排在前面，那个组的流数会被当成默认组的填进执行区那一格——
    // 于是导进来的默认组变成另一份东西，而它管着所有没选组的行。
    let udp_tests = state
        .cfg
        .tests
        .iter()
        .filter(|test| test.transports.iter().any(|t| t.trim() == "udp"));
    let default_profiles = &state.cfg.iperf.udp_profiles;
    let udp_streams = udp_tests
        .clone()
        .find(|test| {
            test.udp_profiles
                .as_ref()
                .is_none_or(|profiles| profiles == default_profiles)
        })
        .or_else(|| udp_tests.clone().next())
        .and_then(|test| test.udp_streams)
        .filter(|value| *value > 0)
        .unwrap_or(1);
    // ping 的次数和包长和 tcp_streams 一样只落在 tests[] 上（界面就是这么写下去
    // 的），只读 cfg.ping 的话，一份「ping 50 次 × 64 字节」的配置回填出来是
    // 默认的「100 次 × 32/1600/65500」——三倍的单元数，而人看着框里的数字
    // 以为就是文件里的那份。
    let ping_count = state
        .cfg
        .tests
        .iter()
        .filter_map(|test| test.ping_count)
        .find(|value| *value > 0)
        .unwrap_or(state.cfg.ping.count);
    let ping_payload_sizes = state
        .cfg
        .tests
        .iter()
        .filter_map(|test| test.ping_payload_sizes.clone())
        .find(|sizes| !sizes.is_empty())
        .unwrap_or_else(|| state.cfg.ping.payload_sizes.clone());
    BootstrapOut {
        agent_host: state.agent_host.clone(),
        agent_port: state.cfg.agent_port,
        token_configured: !state.cfg.agent_token.is_empty(),
        ipv4_prefixes: state.cfg.ipv4_prefixes.clone(),
        duration: state.cfg.iperf.duration,
        tcp_windows: state.cfg.iperf.tcp_windows.clone(),
        tcp_streams,
        // 和 -l / -w 一样要去重：一档 `-b` 会因为 `-l`/`-w` 的每个档位各生成
        // 一份 profile，照抄进输入框的话，「下载 → 导入」每走一轮档位就翻一倍。
        udp_bandwidths: distinct(
            state
                .cfg
                .iperf
                .udp_profiles
                .iter()
                .map(|profile| profile.bandwidth.clone()),
        ),
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
        ping_count,
        ping_payload_sizes,
        screenshot: state.cfg.screenshot,
        ui_plan_supported: true,
    }
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

fn ui_endpoint_exists(state: &UiState, endpoint: &str) -> bool {
    endpoint_exists(state, endpoint)
        || builder::resolve_endpoint(endpoint, &state.master, &state.agent).is_ok()
}

fn values_are_allowed(values: &[String], allowed: &[&str]) -> bool {
    !values.is_empty()
        && values
            .iter()
            .all(|value| allowed.iter().any(|candidate| value == candidate))
}

/// 浏览器控件不是信任边界：即使页面会过滤，后端仍需拒绝空选择、越界数值和
/// 无效档位。尤其不能把“用户把整列取消勾选”静默解释成默认 AB/TCP/IPv4。
///
/// 拆成三段是因为这三段的判据来源不同：全局档位只看 `req` 自己，逐对检查还要
/// 看网口覆盖，网口策略要看当前扫到的网口表。混在一个函数里时，读到一半分不清
/// 手上这个 `pair` 到底受哪些外部状态影响。
fn validate_request(state: &UiState, req: &RunRequest) -> Result<(), String> {
    if let Some(plan) = req.ui_plan.as_ref() {
        if !req.pairs.is_empty() {
            return Err("ui_plan 与 legacy pairs 不能同时提交".into());
        }
        validate_global_values(req)?;
        validate_ui_plan(state, plan)?;
    } else {
        validate_global_sweeps(req)?;
    }
    for (index, group) in req.udp_groups.iter().enumerate() {
        validate_udp_group(index + 1, group)?;
    }
    for (index, group) in req.tcp_groups.iter().enumerate() {
        validate_tcp_group(index + 1, group)?;
    }
    if req.ui_plan.is_none() {
        for pair in &req.pairs {
            validate_pair(state, pair, req.udp_groups.len(), req.tcp_groups.len())?;
        }
    }
    validate_nic_policies(state, req)
}

/// 一个附加的 UDP 参数组。默认组的那几格由 `validate_global_sweeps` 管。
fn validate_udp_group(index: usize, group: &UdpGroup) -> Result<(), String> {
    let label = if group.name.trim().is_empty() {
        format!("UDP 参数组 {index}")
    } else {
        format!("UDP 参数组「{}」", group.name.trim())
    };
    // 组不继承默认组，所以 `-b` 空着不是「跟着全局」而是「一个档位都没有」，
    // 那一组生成不出任何单元。这里挡住，比让人在「预览任务」里数不到强。
    if cleaned_list(&group.bandwidths).is_empty() {
        return Err(format!("{label} 没填 -b：组是完整定义，不继承默认组的档位"));
    }
    for bandwidth in cleaned_list(&group.bandwidths) {
        check_udp_bandwidth(&bandwidth, &label)?;
    }
    for length in cleaned_list(&group.lengths) {
        let bytes = crate::cmd::ctstraffic::parse_size_bytes(&length)
            .map_err(|error| format!("{label} 的 -l {length:?} 无效：{error}"))?;
        if bytes > 65_507 {
            return Err(format!(
                "{label} 的 -l {length:?} 超过单个 UDP 报文上限 65507 字节"
            ));
        }
    }
    for window in cleaned_list(&group.windows) {
        crate::cmd::ctstraffic::parse_size_bytes(&window)
            .map_err(|error| format!("{label} 的 -w {window:?} 无效：{error}"))?;
    }
    if group.streams > MAX_UDP_STREAMS {
        return Err(format!(
            "{label} 的流数 {} 超过上限 {MAX_UDP_STREAMS}",
            group.streams
        ));
    }
    Ok(())
}

/// 一个附加的 TCP 参数组。默认组的 `-w` / `-P` 那两个框由 `validate_global_sweeps`
/// 管。TCP 组没有 UDP 那样的必填项（`-b`）：`-w`、`-P` 都可留空。
fn validate_tcp_group(index: usize, group: &TcpGroup) -> Result<(), String> {
    let label = if group.name.trim().is_empty() {
        format!("TCP 参数组 {index}")
    } else {
        format!("TCP 参数组「{}」", group.name.trim())
    };
    for window in cleaned_list(&group.windows) {
        crate::cmd::ctstraffic::parse_size_bytes(&window)
            .map_err(|error| format!("{label} 的 -w {window:?} 无效：{error}"))?;
    }
    if group.streams.iter().any(|value| !(1..=32).contains(value)) {
        return Err(format!("{label} 的 -P 每一档都必须在 1..=32 之间"));
    }
    Ok(())
}

/// 执行区那些「所有配对共用」的档位与数值。
fn validate_global_sweeps(req: &RunRequest) -> Result<(), String> {
    if req.pairs.is_empty() {
        return Err("一个测试项都没勾".into());
    }
    validate_global_values(req)
}

/// 全局时长、参数档位和 ping 边界检查，共用于 legacy matrix 与 suite plan。
fn validate_global_values(req: &RunRequest) -> Result<(), String> {
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
        check_udp_bandwidth(bandwidth.trim(), "默认组")?;
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
    Ok(())
}

fn canonical_ui_direction(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "ab" | "a->b" | "a>b" | "a_to_b" => Some("ab"),
        "ba" | "b->a" | "b>a" | "b_to_a" => Some("ba"),
        "bidir" | "both-way" | "a<->b" | "双向" => Some("bidir"),
        // `both` is the legacy spelling for two independent one-way legs.
        "both" => Some("both"),
        _ => None,
    }
}

fn canonical_ui_ip(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "v4" | "ipv4" | "4" => Some("v4"),
        "v6" | "ipv6" | "6" => Some("v6"),
        _ => None,
    }
}

fn ui_task_protocol(task: &UiTask) -> Option<String> {
    let raw = if !task.protocol.trim().is_empty() {
        task.protocol.trim().to_ascii_lowercase()
    } else if task.transports.len() == 1 {
        task.transports[0].trim().to_ascii_lowercase()
    } else {
        String::new()
    };
    match raw.as_str() {
        "tcp" | "udp" | "ping" => Some(raw),
        _ => None,
    }
}

fn validate_ui_recipe(protocol: &str, recipe: &UiRecipe, index: usize) -> Result<(), String> {
    if recipe.id.trim().is_empty() {
        return Err(format!("{protocol} 配方 {} 缺少稳定 id", index + 1));
    }
    if recipe.mode.trim().is_empty()
        || matches!(
            recipe.mode.trim().to_ascii_lowercase().as_str(),
            "fixed" | "scan"
        )
    {
        // Empty mode is the default fixed mode.  Other modes are rejected
        // below, but keep this branch explicit for readable diagnostics.
    } else {
        return Err(format!(
            "{protocol} 配方 {} 的 mode 只支持 fixed 或 scan",
            recipe.id
        ));
    }
    if protocol == "tcp" {
        for window in recipe
            .tcp_windows
            .iter()
            .chain(recipe.windows.iter())
            .filter(|v| !v.trim().is_empty())
        {
            crate::cmd::ctstraffic::parse_size_bytes(window.trim())
                .map_err(|e| format!("TCP 配方 {} 的 -w {:?} 无效：{e}", recipe.id, window))?;
        }
        for profile in &recipe.profiles {
            if let Some(window) = profile.window.as_deref().filter(|v| !v.trim().is_empty()) {
                crate::cmd::ctstraffic::parse_size_bytes(window.trim()).map_err(|e| {
                    format!(
                        "TCP 配方 {} 的 profile -w {:?} 无效：{e}",
                        recipe.id, window
                    )
                })?;
            }
            for streams in profile
                .tcp_streams
                .as_ref()
                .unwrap_or(&profile.streams)
                .values()
            {
                if !(1..=32).contains(&streams) {
                    return Err(format!("TCP 配方 {} 的 -P 必须在 1..=32 之间", recipe.id));
                }
            }
        }
        if recipe.tcp_streams.iter().any(|v| !(1..=32).contains(v)) {
            return Err(format!("TCP 配方 {} 的 -P 必须在 1..=32 之间", recipe.id));
        }
    } else if protocol == "udp" {
        for bandwidth in recipe.bandwidths.iter().filter(|v| !v.trim().is_empty()) {
            check_udp_bandwidth(bandwidth.trim(), &format!("UDP 配方 {}", recipe.id))?;
        }
        for length in recipe.lengths.iter().filter(|v| !v.trim().is_empty()) {
            let bytes = crate::cmd::ctstraffic::parse_size_bytes(length.trim())
                .map_err(|e| format!("UDP 配方 {} 的 -l {:?} 无效：{e}", recipe.id, length))?;
            if bytes > 65_507 {
                return Err(format!("UDP 配方 {} 的 -l 超过 65507 字节", recipe.id));
            }
        }
        for window in recipe.windows.iter().filter(|v| !v.trim().is_empty()) {
            crate::cmd::ctstraffic::parse_size_bytes(window.trim())
                .map_err(|e| format!("UDP 配方 {} 的 -w {:?} 无效：{e}", recipe.id, window))?;
        }
        for profile in &recipe.profiles {
            if let Some(bandwidth) = profile
                .bandwidth
                .as_deref()
                .filter(|v| !v.trim().is_empty())
            {
                check_udp_bandwidth(bandwidth.trim(), &format!("UDP 配方 {}", recipe.id))?;
            }
            if let Some(length) = profile.length.as_deref().filter(|v| !v.trim().is_empty()) {
                let bytes =
                    crate::cmd::ctstraffic::parse_size_bytes(length.trim()).map_err(|e| {
                        format!(
                            "UDP 配方 {} 的 profile -l {:?} 无效：{e}",
                            recipe.id, length
                        )
                    })?;
                if bytes > 65_507 {
                    return Err(format!(
                        "UDP 配方 {} 的 profile -l 超过 65507 字节",
                        recipe.id
                    ));
                }
            }
            if let Some(window) = profile.window.as_deref().filter(|v| !v.trim().is_empty()) {
                crate::cmd::ctstraffic::parse_size_bytes(window.trim()).map_err(|e| {
                    format!(
                        "UDP 配方 {} 的 profile -w {:?} 无效：{e}",
                        recipe.id, window
                    )
                })?;
            }
            let streams = profile
                .udp_streams
                .as_ref()
                .unwrap_or(&profile.streams)
                .values();
            if streams.iter().any(|v| !(1..=32).contains(v)) {
                return Err(format!("UDP 配方 {} 的流数必须在 1..=32 之间", recipe.id));
            }
        }
        if recipe.udp_streams.iter().any(|v| !(1..=32).contains(v)) {
            return Err(format!("UDP 配方 {} 的流数必须在 1..=32 之间", recipe.id));
        }
        for profile in &recipe.udp_profiles {
            check_udp_bandwidth(profile.bandwidth.trim(), &format!("UDP 配方 {}", recipe.id))?;
            if let Some(length) = profile.length.as_deref().filter(|v| !v.trim().is_empty()) {
                let bytes =
                    crate::cmd::ctstraffic::parse_size_bytes(length.trim()).map_err(|e| {
                        format!(
                            "UDP 配方 {} 的 profile -l {:?} 无效：{e}",
                            recipe.id, length
                        )
                    })?;
                if bytes > 65_507 {
                    return Err(format!(
                        "UDP 配方 {} 的 profile -l 超过 65507 字节",
                        recipe.id
                    ));
                }
            }
            if let Some(window) = profile.window.as_deref().filter(|v| !v.trim().is_empty()) {
                crate::cmd::ctstraffic::parse_size_bytes(window.trim()).map_err(|e| {
                    format!(
                        "UDP 配方 {} 的 profile -w {:?} 无效：{e}",
                        recipe.id, window
                    )
                })?;
            }
        }
        // An explicitly defined UDP recipe must expand to at least one
        // profile. Without this guard a card containing only empty fields is
        // accepted and silently contributes no test units.
        let has_bandwidth = if !recipe.udp_profiles.is_empty() {
            recipe
                .udp_profiles
                .iter()
                .any(|profile| !profile.bandwidth.trim().is_empty())
        } else if !recipe.profiles.is_empty() {
            let recipe_fallback = recipe
                .bandwidths
                .iter()
                .any(|value| !value.trim().is_empty());
            recipe.profiles.iter().any(|profile| {
                profile
                    .bandwidth
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                    || recipe_fallback
            })
        } else {
            recipe
                .bandwidths
                .iter()
                .any(|value| !value.trim().is_empty())
        };
        // A completely empty recipe means "use the request/config default";
        // that is useful when a suite intentionally wants the shared default
        // without duplicating its axes.  Reject only an explicitly populated
        // recipe whose fields all resolve to empty values, because that shape
        // otherwise looks configured while producing zero units.
        let explicitly_configured = !recipe.udp_profiles.is_empty()
            || !recipe.profiles.is_empty()
            || !recipe.bandwidths.is_empty()
            || !recipe.lengths.is_empty()
            || !recipe.windows.is_empty()
            || !recipe.udp_streams.is_empty();
        if explicitly_configured && !has_bandwidth {
            return Err(format!(
                "UDP 配方 {} 没有有效的 -b 档位，无法生成测试单元",
                recipe.id
            ));
        }
    }
    Ok(())
}

/// 校验任务级的显式验收目标。
///
/// `RateTargets::for_direction` 会把非法值当成“未配置”并继续走自动推导；
/// 对来自浏览器的计划来说这会把一个明显的输入错误静默吞掉，最终报告看起来
/// 像是用户根本没有填写目标。因此 UI 计划要在边界处拒绝非有限值和非正值。
fn validate_ui_rate_targets(
    label: &str,
    targets: &crate::config::RateTargets,
) -> Result<(), String> {
    for (direction, value) in [
        ("forward", targets.forward),
        ("ab", targets.ab),
        ("ba", targets.ba),
    ] {
        if let Some(value) = value {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!(
                    "{label} 的 {direction} 目标必须是大于 0 的有限 Mbps"
                ));
            }
        }
    }
    Ok(())
}

/// Validate a suite plan without touching the legacy matrix checks.
fn validate_ui_plan(state: &UiState, plan: &UiPlan) -> Result<(), String> {
    if plan.ui_plan_version > 1 {
        return Err(format!(
            "不支持的 ui_plan_version={}（当前支持 1）",
            plan.ui_plan_version
        ));
    }
    if plan.link_sets.is_empty() {
        return Err("ui_plan 至少需要一个 link_set".into());
    }
    if plan.suites.is_empty() {
        return Err("ui_plan 至少需要一个 suite".into());
    }
    if plan.bindings.is_empty() {
        return Err("ui_plan 至少需要一个 binding".into());
    }

    let mut ids = HashSet::new();
    for (index, set) in plan.link_sets.iter().enumerate() {
        if set.id.trim().is_empty() || !ids.insert(set.id.clone()) {
            return Err(format!("link_set {} 的 id 缺失或重复", index + 1));
        }
        let mut pair_ids = HashSet::new();
        let mut pair_endpoints = HashSet::new();
        for (pair_index, pair) in set.pair_refs.iter().enumerate() {
            if pair.id.trim().is_empty() || !pair_ids.insert(pair.id.clone()) {
                return Err(format!("link_set {} 的 pair_ref id 缺失或重复", set.id));
            }
            if pair.src.trim().is_empty()
                || pair.dst.trim().is_empty()
                || pair.src == pair.dst
                || !ui_endpoint_exists(state, &pair.src)
                || !ui_endpoint_exists(state, &pair.dst)
            {
                return Err(format!(
                    "link_set {} 的 pair_ref {} 已失效：{} -> {}",
                    set.id,
                    pair_index + 1,
                    pair.src,
                    pair.dst
                ));
            }
            // NAME= and role selectors can spell the same physical interface in
            // different ways. Resolve both before comparing; a raw-string check
            // alone would let a self-link through and only fail much later in the
            // builder, after the preview had already been shown.
            let src_endpoint = builder::resolve_endpoint(&pair.src, &state.master, &state.agent)
                .map_err(|error| {
                    format!(
                        "link_set {} 的 pair_ref {} 源端点无效：{error}",
                        set.id,
                        pair_index + 1
                    )
                })?;
            let dst_endpoint = builder::resolve_endpoint(&pair.dst, &state.master, &state.agent)
                .map_err(|error| {
                    format!(
                        "link_set {} 的 pair_ref {} 目标端点无效：{error}",
                        set.id,
                        pair_index + 1
                    )
                })?;
            if src_endpoint.key() == dst_endpoint.key() {
                return Err(format!(
                    "link_set {} 的 pair_ref {} 源和目标不能是同一块网口",
                    set.id,
                    pair_index + 1
                ));
            }
            let mut endpoint_key = [src_endpoint.key(), dst_endpoint.key()];
            endpoint_key.sort();
            if !pair_endpoints.insert(endpoint_key) {
                return Err(format!(
                    "link_set {} 包含重复的网口对：{} -> {}",
                    set.id, pair.src, pair.dst
                ));
            }
        }
        // An empty set is allowed as an unbound draft.  The quick workspace
        // lets users create a collection before selecting concrete NIC pairs,
        // and execution requests can also contain a stale-only collection
        // after the browser filters invalid endpoints.  A set that is actually
        // referenced by a binding is checked below and must still contain at
        // least one pair; keeping the distinction here avoids rejecting an
        // otherwise runnable plan merely because an unused draft is present.
    }

    // Recipe IDs are global across protocol buckets so a binding remains
    // stable even if the UI reorders TCP and UDP cards.  They are a separate
    // namespace from link-set IDs: a project is perfectly entitled to call a
    // set and a recipe both "default" because references always carry the
    // owning field (link_set_id vs recipe_ids).  Reusing the top-level `ids`
    // set here would reject that harmless, and common, naming pattern.
    let mut recipe_ids = HashSet::new();
    for (protocol, recipes) in [
        ("tcp", &plan.recipes.tcp),
        ("udp", &plan.recipes.udp),
        ("ping", &plan.recipes.ping),
    ] {
        for (index, recipe) in recipes.iter().enumerate() {
            if recipe.id.trim().is_empty() || !recipe_ids.insert(recipe.id.clone()) {
                return Err(format!("{protocol} 配方 id 缺失或重复：{}", recipe.id));
            }
            validate_ui_recipe(protocol, recipe, index)?;
        }
    }

    let mut suite_ids = HashSet::new();
    for suite in &plan.suites {
        if suite.id.trim().is_empty() || !suite_ids.insert(suite.id.clone()) {
            return Err(format!("suite id 缺失或重复：{}", suite.id));
        }
        if !suite.execution.trim().is_empty() && !suite.execution.eq_ignore_ascii_case("sequential")
        {
            return Err(format!("suite {} 只支持 execution=sequential", suite.id));
        }
        if suite.tasks.is_empty() {
            return Err(format!("suite {} 没有任务", suite.id));
        }
        let mut task_ids = HashSet::new();
        for task in &suite.tasks {
            if task.id.trim().is_empty() || !task_ids.insert(task.id.clone()) {
                return Err(format!("suite {} 的 task id 缺失或重复", suite.id));
            }
            let protocol = ui_task_protocol(task)
                .ok_or_else(|| format!("suite {} 的 task {} 协议无效", suite.id, task.id))?;
            if task.transports.iter().any(|transport| {
                let transport = transport.trim().to_ascii_lowercase();
                !transport.is_empty() && transport != protocol
            }) {
                return Err(format!(
                    "suite {} 的 task {} protocol 与 transports 不一致",
                    suite.id, task.id
                ));
            }
            if task.directions.is_empty()
                || task
                    .directions
                    .iter()
                    .any(|direction| canonical_ui_direction(direction).is_none())
            {
                return Err(format!("suite {} 的 task {} 方向无效", suite.id, task.id));
            }
            if task.ip.is_empty() || task.ip.iter().any(|ip| canonical_ui_ip(ip).is_none()) {
                return Err(format!(
                    "suite {} 的 task {} IP 版本无效",
                    suite.id, task.id
                ));
            }
            let recipe_ids = &task.recipe_ids;
            // PING currently takes its count and payload sizes from the task
            // (or the request-wide controls).  `UiRecipe` has no ping-specific
            // fields, and the compiler only used a referenced id for naming,
            // which made a non-empty PING recipe look configurable while its
            // parameters were silently ignored.  Reject that ambiguous shape
            // until a recipe schema with explicit ping semantics is added.
            if protocol == "ping" && !recipe_ids.is_empty() {
                return Err(format!(
                    "suite {} 的 task {} 暂不支持 PING 配方引用，请直接填写 ping 次数和包长",
                    suite.id, task.id
                ));
            }
            let recipes = match protocol.as_str() {
                "tcp" => &plan.recipes.tcp,
                "udp" => &plan.recipes.udp,
                _ => &plan.recipes.ping,
            };
            let mut seen_recipe_ids = HashSet::new();
            for recipe_id in recipe_ids {
                if !seen_recipe_ids.insert(recipe_id) {
                    return Err(format!(
                        "suite {} 的 task {} 重复引用 {} 配方 {}",
                        suite.id, task.id, protocol, recipe_id
                    ));
                }
                if !recipes.iter().any(|recipe| recipe.id == *recipe_id) {
                    return Err(format!(
                        "suite {} 的 task {} 引用了不存在的 {} 配方 {}",
                        suite.id, task.id, protocol, recipe_id
                    ));
                }
            }
            if let Some(duration) = task.duration {
                if !(1..=86_400).contains(&duration) {
                    return Err(format!(
                        "suite {} 的 task {} 时长必须在 1..=86400 秒之间",
                        suite.id, task.id
                    ));
                }
            }
            if let Some(targets) = &task.rate_targets_mbps {
                validate_ui_rate_targets(
                    &format!("suite {} 的 task {} rate_targets_mbps", suite.id, task.id),
                    targets,
                )?;
            }
            if protocol == "ping" && task.ping_count.is_some_and(|v| v > 100_000) {
                return Err(format!("suite {} 的 ping 次数超过 100000", suite.id));
            }
            if task
                .ping_payload_sizes
                .as_ref()
                .is_some_and(|sizes| sizes.iter().any(|v| *v > crate::ping::MAX_PAYLOAD))
            {
                return Err(format!("suite {} 的 ping 包长超过上限", suite.id));
            }
            if protocol == "ping" && task.ping_payload_sizes.as_ref().is_some_and(Vec::is_empty) {
                return Err(format!(
                    "suite {} 的 task {} 至少需要一个 ping 包长",
                    suite.id, task.id
                ));
            }
            for (label, raw) in [
                ("A→B", &task.rx_target_bidir_ab),
                ("B→A", &task.rx_target_bidir_ba),
            ] {
                if raw.trim().is_empty() {
                    continue;
                }
                if !task
                    .directions
                    .iter()
                    .filter_map(|d| canonical_ui_direction(d))
                    .any(|d| d == "bidir")
                {
                    return Err(format!(
                        "suite {} 的 task {} 填了 {label} 双向门限但未选择双向",
                        suite.id, task.id
                    ));
                }
                if let Some(RxTarget::Percent(_)) = parse_rx_target(raw)? {
                    return Err(format!(
                        "suite {} 的 task {} 双向门限只能填绝对 Mbps",
                        suite.id, task.id
                    ));
                }
            }
        }
        if !suite.order.is_empty() {
            let mut seen_order = HashSet::new();
            for task_id in &suite.order {
                if !task_ids.contains(task_id) || !seen_order.insert(task_id) {
                    return Err(format!(
                        "suite {} 的 order 引用了无效或重复 task {}",
                        suite.id, task_id
                    ));
                }
            }
        }
    }

    let set_ids: HashSet<&str> = plan.link_sets.iter().map(|s| s.id.as_str()).collect();
    let mut binding_ids = HashSet::new();
    for binding in &plan.bindings {
        if binding.id.trim().is_empty() || !binding_ids.insert(binding.id.clone()) {
            return Err(format!("binding id 缺失或重复：{}", binding.id));
        }
        if !set_ids.contains(binding.link_set_id.as_str()) {
            return Err(format!(
                "binding {} 引用了不存在的 link_set {}",
                binding.id, binding.link_set_id
            ));
        }
        if !suite_ids.contains(binding.suite_id.as_str()) {
            return Err(format!(
                "binding {} 引用了不存在的 suite {}",
                binding.id, binding.suite_id
            ));
        }
        // `append` has no defined merge semantics in the current planner:
        // bindings already select an explicit set of pair refs and a suite,
        // while the compiler treats every binding as a complete replacement
        // assignment.  Accepting it here would therefore make a hand-written
        // project appear to request append while silently executing replace.
        // Keep the omitted/replace spellings (both mean the current behavior)
        // and reject the unsupported mode at the API boundary.
        if !binding.mode.trim().is_empty() && !binding.mode.trim().eq_ignore_ascii_case("replace") {
            return Err(format!(
                "binding {} 的 mode 只支持 replace（append 尚未支持）",
                binding.id
            ));
        }
        let set = plan
            .link_sets
            .iter()
            .find(|s| s.id == binding.link_set_id)
            .expect("set id checked above");
        if !binding.pair_ids.is_empty() {
            let mut seen_pair_ids = HashSet::new();
            for pair_id in &binding.pair_ids {
                if !seen_pair_ids.insert(pair_id) {
                    return Err(format!(
                        "binding {} 重复引用了 pair_ref {}",
                        binding.id, pair_id
                    ));
                }
                if !set.pair_refs.iter().any(|pair| pair.id == *pair_id) {
                    return Err(format!(
                        "binding {} 引用了不存在的 pair_ref {}",
                        binding.id, pair_id
                    ));
                }
            }
        }
        // A binding must always resolve to at least one concrete pair.  This
        // check deliberately happens after validating `pair_ids`, so an
        // unknown ID still gets the more useful "不存在" diagnostic above.
        let has_effective_pairs = if binding.pair_ids.is_empty() {
            !set.pair_refs.is_empty()
        } else {
            binding
                .pair_ids
                .iter()
                .any(|pair_id| set.pair_refs.iter().any(|pair| pair.id == *pair_id))
        };
        if !has_effective_pairs {
            return Err(format!(
                "binding {} 没有可执行的 pair_ref；请先为链路集合添加网口对",
                binding.id
            ));
        }
    }
    Ok(())
}

/// 矩阵里的一行。`pinned` 是在「网口与策略」里单独指定了 UDP `-b` 的网口，
/// 逐对档位能不能生效要看它。
fn validate_pair(
    state: &UiState,
    pair: &PairSelection,
    udp_group_count: usize,
    tcp_group_count: usize,
) -> Result<(), String> {
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
    // 选了默认组之外的组，却没勾 UDP：那几组一个单元都不会跑。和双向门限
    // 同一条规矩——选了却不生效要当场说，静默忽略的话人会以为跑的是那组。
    let udp_selected = pair.transports.iter().any(|t| t == "udp");
    if !udp_selected && pair.udp_groups.iter().any(|index| *index > 0) {
        return Err(format!(
            "配对 {} / {} 选了 UDP 参数组，却没有勾 UDP",
            pair.src, pair.dst
        ));
    }
    for index in &pair.udp_groups {
        if *index > udp_group_count {
            return Err(format!(
                "配对 {} / {} 选的 UDP 参数组不存在（共 {udp_group_count} 个附加组）",
                pair.src, pair.dst
            ));
        }
    }
    // TCP 参数组同 UDP：选了默认组之外的组却没勾 TCP，那几组一个单元都不跑，
    // 当场说清楚，别静默忽略。
    let tcp_selected = pair.transports.iter().any(|t| t == "tcp");
    if !tcp_selected && pair.tcp_groups.iter().any(|index| *index > 0) {
        return Err(format!(
            "配对 {} / {} 选了 TCP 参数组，却没有勾 TCP",
            pair.src, pair.dst
        ));
    }
    for index in &pair.tcp_groups {
        if *index > tcp_group_count {
            return Err(format!(
                "配对 {} / {} 选的 TCP 参数组不存在（共 {tcp_group_count} 个附加组）",
                pair.src, pair.dst
            ));
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
    Ok(())
}

/// 「网口与策略」那张表。
fn validate_nic_policies(state: &UiState, req: &RunRequest) -> Result<(), String> {
    let mut seen = HashSet::new();
    for policy in &req.nic_policies {
        if !endpoint_exists(state, &policy.endpoint) {
            return Err(format!(
                "网口策略已失效：{}。请刷新网口后重新填写",
                policy.endpoint
            ));
        }
        if !seen.insert(policy.endpoint.clone()) {
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
            check_udp_bandwidth(policy.udp_bandwidth.trim(), &policy.endpoint)?;
        }
    }
    Ok(())
}

/// 界面上 UDP 并发流数的上限，和输入框的 `max` 对齐。
const MAX_UDP_STREAMS: u32 = 32;

/// `-b` 的量纲护栏。
///
/// 输入框里的裸数字按 **Mbps** 算（`UdpProfile::parsed_bandwidth` 里无后缀时
/// 乘 10^6），而「预览任务」以前打印的是 bit/s 整数——把 `1000000000` 抄回输入框
/// 就变成 10^9 Mbps，解析得过、校验得过，然后拿着一个天文数字去灌包。
///
/// 400Gbps 远高于这套工具面对的任何链路（最快 10GETH），又远低于那种手滑，
/// 挡在这里能把「填错单位」变成一句能读懂的话。
const MAX_UDP_BANDWIDTH_MBPS: f64 = 400_000.0;

/// 解析并检查一个 `-b` 档位。`label` 用来说清是哪一格填错了。
fn check_udp_bandwidth(raw: &str, label: &str) -> Result<(), String> {
    let parsed = UdpProfile::bw(raw)
        .parsed_bandwidth()
        .map_err(|error| format!("{label} 的 UDP -b {raw:?} 无效：{error}"))?;
    if parsed.mbps > MAX_UDP_BANDWIDTH_MBPS {
        return Err(format!(
            "{label} 的 UDP -b {raw:?} 折合 {:.0} Mbps，超出这套工具面对的任何链路。\
             输入框里的裸数字按 Mbps 算（`1000` = 1000Mbps），要写 bit/s 请加后缀：\
             `1000m` 或 `1G` 都是 1000Mbps",
            parsed.mbps
        ));
    }
    Ok(())
}

/// 在「网口与策略」里单独指定了 UDP `-b` 的那些网口。
///
/// 这个覆盖按**发送腿**生效（见 builder 里的 `link_policy(...).udp_bandwidth`），
/// 所以它同时决定了「全局/逐对档位对这条腿还有没有意义」。
fn udp_pinned_senders(req: &RunRequest) -> HashSet<String> {
    req.nic_policies
        .iter()
        .filter(|policy| !policy.udp_bandwidth.trim().is_empty())
        .map(|policy| policy.endpoint.clone())
        .collect()
}

#[allow(dead_code)]
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

/// 一轮里所有配对共用的档位。
///
/// 逐对覆盖只在这几项上做减法（某一行自己的 `-b`、自己的流数），所以把它们收成
/// 一个东西传给 `specs_for_pair`，而不是把七八个列表一路传参——那样每加一档
/// 扫描维度就要改三处签名。
struct Sweeps {
    /// 第 0 项是默认组（执行区的 `-w` / `-P` 两个框），其余是附加组。
    tcp_groups: Vec<ResolvedTcpGroup>,
    /// 第 0 项是默认组（执行区那几个框），其余是附加组。
    udp_groups: Vec<ResolvedUdpGroup>,
    ping_sizes: Vec<u32>,
    duration: u64,
    /// 在「网口与策略」里单独指定了 UDP `-b` 的网口。
    pinned_senders: HashSet<String>,
}

/// 一组 UDP 参数展开成「跑什么」。
#[derive(Debug, Clone, Default)]
struct ResolvedUdpGroup {
    bandwidths: Vec<String>,
    lengths: Vec<String>,
    windows: Vec<String>,
    streams: u32,
    /// 只有默认组会用到：执行区的 `-b` 留空时，沿用配置文件里那份 profile
    /// **原样**。那份不一定是整齐的叉积（可以是 `1m/64` + `500m/1400`），
    /// 拆成三个轴再乘回去会把它变成另一组档位。
    verbatim: Option<Vec<UdpProfile>>,
}

impl ResolvedUdpGroup {
    fn profiles(&self) -> Vec<UdpProfile> {
        if let Some(profiles) = &self.verbatim {
            return profiles.clone();
        }
        self.bandwidths
            .iter()
            .flat_map(|bandwidth| udp_profiles_for(bandwidth, &self.lengths, &self.windows))
            .collect()
    }
}

/// 一组 TCP 参数展开成「跑什么」：`-w × -P` 两个轴。第 0 组是默认组。
#[derive(Debug, Clone, Default)]
struct ResolvedTcpGroup {
    /// socket buffer 档位。默认组经过 `non_empty` 兜底不会为空；附加组留空
    /// 表示这一维不下发 `-w`（builder 见到空列表跑一条不带 `-w` 的）。
    windows: Vec<String>,
    /// 并发流数档位；空按 `[1]`（builder 那边 -P 恒发，和 UDP 流数同理）。
    stream_steps: Vec<u32>,
}

impl Sweeps {
    /// 选中的那一组。越界回落到默认组——校验已经挡过一次，这里不该再 panic。
    fn udp_group(&self, index: usize) -> &ResolvedUdpGroup {
        self.udp_groups.get(index).unwrap_or(&self.udp_groups[0])
    }
    fn tcp_group(&self, index: usize) -> &ResolvedTcpGroup {
        self.tcp_groups.get(index).unwrap_or(&self.tcp_groups[0])
    }
}

/// 把界面状态翻译成一份 config。规划和执行都走这一个函数，
/// 保证「预计耗时」和真正跑的是同一份东西。
fn config_from_request(state: &UiState, req: &RunRequest) -> Config {
    if let Some(plan) = req.ui_plan.as_ref() {
        return config_from_ui_plan(state, req, plan);
    }
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
    // 默认组的档位同时写回 `iperf.udp_profiles`：下载出来的 config 交给
    // `master --auto` 跑时，没有「组」这个概念，读的就是这一份。
    let global_udp: Vec<UdpProfile> = req
        .udp_bandwidths
        .iter()
        .filter(|b| !b.trim().is_empty())
        .flat_map(|b| udp_profiles_for(b.trim(), &lengths, &udp_windows))
        .collect();
    if !global_udp.is_empty() {
        cfg.iperf.udp_profiles = global_udp;
    }
    cfg.iperf.tcp_windows = windows.clone();

    for policy in &req.nic_policies {
        if let Some(profile) = nic_profile(policy) {
            cfg.link_profiles.by_nic.push(profile);
        }
    }

    // 默认组 = 执行区那几个框；`-b` 留空时沿用配置文件里那份 profile 原样。
    let bandwidths = cleaned_list(&req.udp_bandwidths);
    let mut udp_groups = vec![ResolvedUdpGroup {
        verbatim: bandwidths
            .is_empty()
            .then(|| cfg.iperf.udp_profiles.clone()),
        bandwidths,
        lengths,
        windows: udp_windows,
        streams: req.udp_streams.max(1),
    }];
    udp_groups.extend(req.udp_groups.iter().map(|group| ResolvedUdpGroup {
        bandwidths: cleaned_list(&group.bandwidths),
        lengths: cleaned_list(&group.lengths),
        windows: cleaned_list(&group.windows),
        streams: group.streams.max(1),
        verbatim: None,
    }));

    // 默认 TCP 组 = 执行区的 `-w` / `-P`。`windows` 已经过 `non_empty` 兜底
    // （空则回落到配置里的 tcp_windows），`stream_steps` 空则是 `[1]`。
    let mut tcp_groups = vec![ResolvedTcpGroup {
        windows: windows.clone(),
        stream_steps: stream_steps.clone(),
    }];
    // 附加组不兜底：`-w` 留空就是那一维不下发 `-w`；`-P` 留空按 `[1]`。
    tcp_groups.extend(req.tcp_groups.iter().map(|group| {
        let steps: Vec<u32> = group.streams.iter().copied().filter(|n| *n > 0).collect();
        ResolvedTcpGroup {
            windows: cleaned_list(&group.windows),
            stream_steps: if steps.is_empty() { vec![1] } else { steps },
        }
    }));

    let sweeps = Sweeps {
        tcp_groups,
        udp_groups,
        ping_sizes,
        duration: req.duration.clamp(1, 86_400),
        pinned_senders: udp_pinned_senders(req),
    };
    cfg.tests = req
        .pairs
        .iter()
        .enumerate()
        .flat_map(|(idx, pair)| specs_for_pair(idx, pair, req, &sweeps))
        .collect();
    cfg
}

/// Apply the request-wide settings shared by legacy and suite requests.  The
/// suite compiler calls this directly so it does not have to manufacture a
/// `PairSelection` (which would re-introduce the old shared TCP/UDP fields).
fn ui_request_base_config(state: &UiState, req: &RunRequest) -> Config {
    let mut cfg = state.cfg.clone();
    cfg.agent_host = state.agent_host.clone();
    cfg.screenshot = req.screenshot;
    cfg.limit_udp_by_link_speed = req.limit_udp_by_link_speed;
    cfg.resume = req.resume;
    cfg.iperf.duration = req.duration.clamp(1, 86_400);
    cfg.pairs = None;
    cfg.universal_params = None;
    cfg.link_profiles.by_nic.clear();

    // Quick-plan tasks intentionally keep protocol-specific knobs on the
    // task, but PING's convenient default controls still live at the request
    // level (the same controls used by the legacy matrix).  Carry them into
    // the compiled config before a task falls back to cfg.ping; otherwise a
    // user changing "5 次 / 64 字节" in the quick workbench would silently
    // execute the values from the loaded config instead.
    if req.ping_count > 0 {
        cfg.ping.count = req.ping_count;
    }
    if !req.ping_payload_sizes.is_empty() {
        let mut seen = HashSet::new();
        cfg.ping.payload_sizes = req
            .ping_payload_sizes
            .iter()
            .copied()
            .filter(|size| *size > 0 && seen.insert(*size))
            .collect();
    }

    let tcp_windows = non_empty(&req.tcp_windows, &cfg.iperf.tcp_windows);
    cfg.iperf.tcp_windows = tcp_windows;
    let udp_bandwidths = cleaned_list(&req.udp_bandwidths);
    if !udp_bandwidths.is_empty() {
        let lengths = cleaned_list(&req.udp_lengths);
        let windows = cleaned_list(&req.udp_windows);
        cfg.iperf.udp_profiles = udp_bandwidths
            .iter()
            .flat_map(|b| udp_profiles_for(b, &lengths, &windows))
            .collect();
    }
    for policy in &req.nic_policies {
        if let Some(profile) = nic_profile(policy) {
            cfg.link_profiles.by_nic.push(profile);
        }
    }
    cfg
}

#[derive(Debug, Clone)]
struct UiTcpProfile {
    recipe_id: String,
    window: Option<String>,
    streams: u32,
}

#[derive(Debug, Clone)]
struct UiUdpProfile {
    recipe_id: String,
    profile: UdpProfile,
    streams: u32,
}

fn first_or_one(values: Vec<u32>, fallback: u32) -> Vec<u32> {
    let values: Vec<u32> = values.into_iter().filter(|v| *v > 0).collect();
    if values.is_empty() {
        vec![fallback.max(1)]
    } else {
        values
    }
}

fn recipe_tcp_profiles(recipe: &UiRecipe, fallback_streams: &[u32]) -> Vec<UiTcpProfile> {
    let mut out = Vec::new();
    if !recipe.profiles.is_empty() {
        for profile in &recipe.profiles {
            let streams = profile
                .tcp_streams
                .as_ref()
                .unwrap_or(&profile.streams)
                .values();
            let streams = first_or_one(streams, fallback_streams.first().copied().unwrap_or(1));
            let windows = profile
                .window
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| vec![Some(value.to_string())])
                .unwrap_or_else(|| vec![None]);
            for window in windows {
                for stream in &streams {
                    out.push(UiTcpProfile {
                        recipe_id: recipe.id.clone(),
                        window: window.clone(),
                        streams: *stream,
                    });
                }
            }
        }
        return out;
    }

    let windows = cleaned_list(if !recipe.tcp_windows.is_empty() {
        &recipe.tcp_windows
    } else {
        &recipe.windows
    });
    let windows: Vec<Option<String>> = if windows.is_empty() {
        vec![None]
    } else {
        windows.into_iter().map(Some).collect()
    };
    let streams = first_or_one(
        recipe.tcp_streams.clone(),
        fallback_streams.first().copied().unwrap_or(1),
    );
    for window in windows {
        for stream in &streams {
            out.push(UiTcpProfile {
                recipe_id: recipe.id.clone(),
                window: window.clone(),
                streams: *stream,
            });
        }
    }
    // An entirely empty recipe is a valid fixed recipe: one TCP stream and no
    // explicit socket window.
    if out.is_empty() {
        out.push(UiTcpProfile {
            recipe_id: recipe.id.clone(),
            window: None,
            streams: 1,
        });
    }
    out
}

fn recipe_udp_profiles(
    recipe: &UiRecipe,
    fallback_bandwidths: &[String],
    fallback_streams: u32,
) -> Vec<UiUdpProfile> {
    let mut out = Vec::new();
    if !recipe.udp_profiles.is_empty() {
        let streams = first_or_one(recipe.udp_streams.clone(), fallback_streams);
        for profile in &recipe.udp_profiles {
            for stream in &streams {
                out.push(UiUdpProfile {
                    recipe_id: recipe.id.clone(),
                    profile: UdpProfile {
                        bandwidth: profile.bandwidth.trim().to_string(),
                        length: profile
                            .length
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string),
                        window: profile
                            .window
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string),
                    },
                    streams: *stream,
                });
            }
        }
        return out;
    }
    if !recipe.profiles.is_empty() {
        for profile in &recipe.profiles {
            let bandwidths: Vec<String> = profile
                .bandwidth
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(|value| vec![value.trim().to_string()])
                .unwrap_or_else(|| cleaned_list(&recipe.bandwidths));
            if bandwidths.is_empty() {
                continue;
            }
            let streams = profile
                .udp_streams
                .as_ref()
                .unwrap_or(&profile.streams)
                .values();
            let streams = first_or_one(streams, fallback_streams);
            for bandwidth in bandwidths {
                for stream in &streams {
                    out.push(UiUdpProfile {
                        recipe_id: recipe.id.clone(),
                        profile: UdpProfile {
                            bandwidth: bandwidth.clone(),
                            length: profile
                                .length
                                .as_deref()
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(str::to_string),
                            window: profile
                                .window
                                .as_deref()
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(str::to_string),
                        },
                        streams: *stream,
                    });
                }
            }
        }
        return out;
    }

    let bandwidths = cleaned_list(&recipe.bandwidths);
    let bandwidths = if bandwidths.is_empty() {
        cleaned_list(fallback_bandwidths)
    } else {
        bandwidths
    };
    let lengths = cleaned_list(&recipe.lengths);
    let windows = cleaned_list(&recipe.windows);
    let lengths: Vec<Option<String>> = if lengths.is_empty() {
        vec![None]
    } else {
        lengths.into_iter().map(Some).collect()
    };
    let windows: Vec<Option<String>> = if windows.is_empty() {
        vec![None]
    } else {
        windows.into_iter().map(Some).collect()
    };
    let streams = first_or_one(recipe.udp_streams.clone(), fallback_streams);
    for bandwidth in bandwidths {
        for length in &lengths {
            for window in &windows {
                for stream in &streams {
                    out.push(UiUdpProfile {
                        recipe_id: recipe.id.clone(),
                        profile: UdpProfile {
                            bandwidth: bandwidth.clone(),
                            length: length.clone(),
                            window: window.clone(),
                        },
                        streams: *stream,
                    });
                }
            }
        }
    }
    out
}

fn normalized_ui_directions(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for value in raw {
        match canonical_ui_direction(value) {
            Some("both") => {
                for direction in ["ab", "ba"] {
                    if !out.iter().any(|v| v == direction) {
                        out.push(direction.to_string());
                    }
                }
            }
            Some(direction) if !out.iter().any(|v| v == direction) => {
                out.push(direction.to_string())
            }
            _ => {}
        }
    }
    out
}

fn normalized_ui_ips(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for value in raw {
        if let Some(ip) = canonical_ui_ip(value) {
            if !out.iter().any(|v| v == ip) {
                out.push(ip.to_string());
            }
        }
    }
    out
}

fn ui_task_targets(task: &UiTask) -> Option<crate::config::RateTargets> {
    let ab = parse_rx_target(&task.rx_target_bidir_ab)
        .ok()
        .flatten()
        .and_then(rx_target_mbps);
    let ba = parse_rx_target(&task.rx_target_bidir_ba)
        .ok()
        .flatten()
        .and_then(rx_target_mbps);
    (ab.is_some() || ba.is_some()).then_some(crate::config::RateTargets {
        forward: None,
        ab,
        ba,
    })
}

fn ui_task_base_spec(
    name: String,
    pair: &UiPairRef,
    task: &UiTask,
    protocol: &str,
    directions: &[String],
    ips: &[String],
    duration: u64,
) -> TestSpec {
    TestSpec {
        name,
        src: pair.src.clone(),
        dst: pair.dst.clone(),
        direction: OneOrMany::Many(directions.to_vec()),
        kinds: if protocol == "ping" {
            vec!["ping".into()]
        } else {
            vec!["iperf".into()]
        },
        transports: if protocol == "ping" {
            Vec::new()
        } else {
            vec![protocol.to_string()]
        },
        ip: ips.to_vec(),
        streams: 1,
        tcp_streams: None,
        udp_streams: None,
        iperf_duration: Some(task.duration.unwrap_or(duration).clamp(1, 86_400)),
        ping_count: task.ping_count.filter(|value| *value > 0),
        ping_payload_sizes: task.ping_payload_sizes.clone(),
        tcp_windows: None,
        udp_profiles: None,
        rate_mode: task.rate_mode,
        rate_targets_mbps: task.rate_targets_mbps.clone(),
        rate_targets_bidir_mbps: ui_task_targets(task),
    }
}

#[allow(clippy::too_many_arguments)]
fn ui_specs_for_task(
    pair: &UiPairRef,
    suite: &UiSuite,
    task: &UiTask,
    recipes: &UiRecipes,
    req: &RunRequest,
    cfg: &Config,
    binding_id: &str,
    link_set_id: &str,
) -> Vec<TestSpec> {
    let Some(protocol) = ui_task_protocol(task) else {
        return Vec::new();
    };
    let directions = normalized_ui_directions(&task.directions);
    let ips = normalized_ui_ips(&task.ip);
    let mut out = Vec::new();
    match protocol.as_str() {
        "tcp" => {
            let selected: Vec<&UiRecipe> = if task.recipe_ids.is_empty() {
                Vec::new()
            } else {
                task.recipe_ids
                    .iter()
                    .filter_map(|id| recipes.tcp.iter().find(|recipe| recipe.id == *id))
                    .collect()
            };
            let fallback_streams: Vec<u32> = req
                .tcp_streams
                .iter()
                .copied()
                .filter(|value| *value > 0)
                .collect();
            let fallback_windows = non_empty(&req.tcp_windows, &cfg.iperf.tcp_windows);
            let fallback = UiRecipe {
                id: "default".into(),
                name: "默认 TCP".into(),
                tcp_windows: fallback_windows.clone(),
                tcp_streams: fallback_streams.clone(),
                ..Default::default()
            };
            let recipes: Vec<&UiRecipe> = if selected.is_empty() {
                vec![&fallback]
            } else {
                selected
            };
            for recipe in recipes {
                for profile in recipe_tcp_profiles(recipe, &fallback_streams) {
                    let suffix = format!(
                        "{}/{}/{}/{}/{}/{}",
                        ui_name_segment(link_set_id),
                        ui_name_segment(binding_id),
                        ui_name_segment(&pair.id),
                        ui_name_segment(&suite.id),
                        ui_name_segment(&task.id),
                        ui_name_segment(&profile.recipe_id)
                    );
                    let mut spec = ui_task_base_spec(
                        format!("ui-plan/{suffix}/tcp-P{}", profile.streams),
                        pair,
                        task,
                        "tcp",
                        &directions,
                        &ips,
                        req.duration,
                    );
                    spec.tcp_streams = Some(profile.streams);
                    spec.tcp_windows = Some(profile.window.into_iter().collect());
                    out.push(spec);
                }
            }
        }
        "udp" => {
            let selected: Vec<&UiRecipe> = if task.recipe_ids.is_empty() {
                Vec::new()
            } else {
                task.recipe_ids
                    .iter()
                    .filter_map(|id| recipes.udp.iter().find(|recipe| recipe.id == *id))
                    .collect()
            };
            let fallback_bandwidths = if req.udp_bandwidths.is_empty() {
                cfg.iperf
                    .udp_profiles
                    .iter()
                    .map(|profile| profile.bandwidth.clone())
                    .collect::<Vec<_>>()
            } else {
                req.udp_bandwidths.clone()
            };
            let mut fallback = UiRecipe {
                id: "default".into(),
                name: "默认 UDP".into(),
                bandwidths: fallback_bandwidths.clone(),
                lengths: req.udp_lengths.clone(),
                windows: req.udp_windows.clone(),
                udp_streams: vec![req.udp_streams.max(1)],
                ..Default::default()
            };
            // With no suite recipe and no request-wide UDP axes, preserve the
            // configured profile list verbatim (it may be intentionally
            // non-Cartesian) instead of reconstructing it from bandwidths.
            if req.udp_bandwidths.is_empty()
                && req.udp_lengths.is_empty()
                && req.udp_windows.is_empty()
            {
                fallback.udp_profiles = cfg.iperf.udp_profiles.clone();
            }
            let recipes: Vec<&UiRecipe> = if selected.is_empty() {
                vec![&fallback]
            } else {
                selected
            };
            let src_pinned = req.nic_policies.iter().any(|policy| {
                policy.endpoint == pair.src && !policy.udp_bandwidth.trim().is_empty()
            });
            let dst_pinned = req.nic_policies.iter().any(|policy| {
                policy.endpoint == pair.dst && !policy.udp_bandwidth.trim().is_empty()
            });
            // A pinned sending leg does not depend on the recipe bandwidth.
            // Collapse such profiles by their remaining dimensions so a scan
            // over 1G/2G/3G does not run the exact same pinned command three
            // times.  Keep stream count in the key because it is an actual
            // execution dimension even when `-b` is overridden.
            let mut pinned_profiles_seen: HashSet<String> = HashSet::new();
            for recipe in recipes {
                for profile in recipe_udp_profiles(recipe, &fallback_bandwidths, req.udp_streams) {
                    let pinned_direction = |direction: &String| match direction.as_str() {
                        "ab" => src_pinned,
                        "ba" => dst_pinned,
                        "bidir" => src_pinned && dst_pinned,
                        _ => false,
                    };
                    let (pinned, swept): (Vec<String>, Vec<String>) =
                        directions.iter().cloned().partition(pinned_direction);
                    let suffix = format!(
                        "{}/{}/{}/{}/{}/{}",
                        ui_name_segment(link_set_id),
                        ui_name_segment(binding_id),
                        ui_name_segment(&pair.id),
                        ui_name_segment(&suite.id),
                        ui_name_segment(&task.id),
                        ui_name_segment(&profile.recipe_id)
                    );
                    if !pinned.is_empty() {
                        let pinned_key = format!(
                            "{:?}|{:?}|{}",
                            profile.profile.length, profile.profile.window, profile.streams
                        );
                        if !pinned_profiles_seen.insert(pinned_key) {
                            // The same pinned profile was already emitted for
                            // this task/recipe.  Swept directions still need
                            // every profile and are handled below.
                        } else {
                            let mut spec = ui_task_base_spec(
                                format!("ui-plan/{suffix}/udp-pinned"),
                                pair,
                                task,
                                "udp",
                                &pinned,
                                &ips,
                                req.duration,
                            );
                            let placeholder = req
                                .nic_policies
                                .iter()
                                .find(|policy| {
                                    (policy.endpoint == pair.src || policy.endpoint == pair.dst)
                                        && !policy.udp_bandwidth.trim().is_empty()
                                })
                                .map(|policy| policy.udp_bandwidth.trim().to_string())
                                .unwrap_or_else(|| profile.profile.bandwidth.clone());
                            let mut pinned_profile = profile.profile.clone();
                            pinned_profile.bandwidth = placeholder;
                            spec.udp_streams = Some(profile.streams);
                            spec.udp_profiles = Some(vec![pinned_profile]);
                            out.push(spec);
                        }
                    }
                    if !swept.is_empty() {
                        let mut spec = ui_task_base_spec(
                            format!("ui-plan/{suffix}/udp"),
                            pair,
                            task,
                            "udp",
                            &swept,
                            &ips,
                            req.duration,
                        );
                        spec.udp_streams = Some(profile.streams);
                        spec.udp_profiles = Some(vec![profile.profile.clone()]);
                        out.push(spec);
                    }
                }
            }
        }
        "ping" => {
            let selected: Vec<String> = if task.recipe_ids.is_empty() {
                vec!["default".into()]
            } else {
                task.recipe_ids.clone()
            };
            for recipe_id in selected {
                let suffix = format!(
                    "{}/{}/{}/{}/{}/{}",
                    ui_name_segment(link_set_id),
                    ui_name_segment(binding_id),
                    ui_name_segment(&pair.id),
                    ui_name_segment(&suite.id),
                    ui_name_segment(&task.id),
                    ui_name_segment(&recipe_id)
                );
                out.push(ui_task_base_spec(
                    format!("ui-plan/{suffix}/ping"),
                    pair,
                    task,
                    "ping",
                    &directions,
                    &ips,
                    req.duration,
                ));
            }
        }
        _ => {}
    }
    out
}

fn config_from_ui_plan(state: &UiState, req: &RunRequest, plan: &UiPlan) -> Config {
    let mut cfg = ui_request_base_config(state, req);
    let mut bindings: Vec<(usize, &UiBinding)> = plan.bindings.iter().enumerate().collect();
    bindings.sort_by_key(|(index, binding)| (binding.order, *index));
    let mut tests = Vec::new();
    for (_, binding) in bindings {
        let Some(set) = plan
            .link_sets
            .iter()
            .find(|set| set.id == binding.link_set_id)
        else {
            continue;
        };
        let Some(suite) = plan
            .suites
            .iter()
            .find(|suite| suite.id == binding.suite_id)
        else {
            continue;
        };
        let pairs: Vec<&UiPairRef> = if binding.pair_ids.is_empty() {
            set.pair_refs.iter().collect()
        } else {
            binding
                .pair_ids
                .iter()
                .filter_map(|id| set.pair_refs.iter().find(|pair| pair.id == *id))
                .collect()
        };
        let mut tasks: Vec<&UiTask> = Vec::new();
        if suite.order.is_empty() {
            tasks.extend(suite.tasks.iter());
        } else {
            for task_id in &suite.order {
                if let Some(task) = suite.tasks.iter().find(|task| task.id == *task_id) {
                    tasks.push(task);
                }
            }
            // Validation permits a partial order for forward compatibility;
            // append unmentioned tasks in declaration order.
            for task in &suite.tasks {
                if !suite.order.iter().any(|id| id == &task.id) {
                    tasks.push(task);
                }
            }
        }
        for pair in pairs {
            for task in &tasks {
                tests.extend(ui_specs_for_task(
                    pair,
                    suite,
                    task,
                    &plan.recipes,
                    req,
                    &cfg,
                    &binding.id,
                    &set.id,
                ));
            }
        }
    }
    cfg.tests = tests;
    cfg
}

/// 这一行要跑哪几组：去重保序，空列表按「只跑默认组」解读。
///
/// 去重是必须的：同一组选两次会生成两批同名单元，resume 里互相覆盖，
/// 后写的那条赢——于是可能跳过一个其实 FAIL 了的单元。
fn selected_udp_groups(pair: &PairSelection) -> Vec<usize> {
    if pair.udp_groups.is_empty() {
        return vec![0];
    }
    let mut seen = HashSet::new();
    pair.udp_groups
        .iter()
        .copied()
        .filter(|index| seen.insert(*index))
        .collect()
}

/// TCP 版的同一件事：去重保序，空列表按「只跑默认组」解读。
fn selected_tcp_groups(pair: &PairSelection) -> Vec<usize> {
    if pair.tcp_groups.is_empty() {
        return vec![0];
    }
    let mut seen = HashSet::new();
    pair.tcp_groups
        .iter()
        .copied()
        .filter(|index| seen.insert(*index))
        .collect()
}

/// 矩阵里的一行 -> 若干条 TestSpec。
///
/// 一行会被拆开是因为配置模型里 `tcp_streams` 是标量、ping 挂在 `kinds` 上、
/// 而 UDP 的「被网口钉死的方向」和「还要扫档位的方向」用的是两份不同的档位。
fn specs_for_pair(
    idx: usize,
    pair: &PairSelection,
    req: &RunRequest,
    sweeps: &Sweeps,
) -> Vec<TestSpec> {
    let mut tests: Vec<TestSpec> = Vec::new();
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
        // UDP 流数只写在 UDP 单元上。写在 TCP/ping 单元上既没有意义，又会让
        // 回填时分不清「默认组的流数」是哪一个（那边是按 tests[] 反推的）。
        udp_streams: None,
        iperf_duration: Some(sweeps.duration),
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
    // 选中的每一组各生成一批 TCP 单元（`-w × -P`）。同一行选两组 = 这一对
    // 跑两遍，参数各按各的组来——和 UDP 的多组展开一模一样。
    if want_tcp {
        for group_index in selected_tcp_groups(pair) {
            let tcp = sweeps.tcp_group(group_index);
            // 默认组沿用原来的单元名（`ui-N-tcp-P{P}`），改名会改掉 resume id
            // ——虽然 TCP 的 resume id 只认 profile（-w/-P），不认 spec.name，
            // 这里保持一致仍是对的。别的组各带一个后缀。
            let suffix = if group_index == 0 {
                String::new()
            } else {
                format!("-g{}", group_index + 1)
            };
            for streams in &tcp.stream_steps {
                let mut spec = base(
                    format!("ui-{}-tcp{suffix}-P{streams}", idx + 1),
                    vec!["tcp".into()],
                );
                spec.tcp_streams = Some(*streams);
                // 空列表原样传给 builder：它把「没有 -w 档位」跑成一条不带 -w
                // 的 TCP。默认组经过 non_empty 兜底不会走到这一支。
                spec.tcp_windows = Some(tcp.windows.clone());
                tests.push(spec);
            }
        }
    }
    // 选中的每一组各生成一批 UDP 单元。同一行选两组 = 这一对跑两遍，
    // 参数各按各的组来。
    for group_index in selected_udp_groups(pair) {
        if !want_udp {
            break;
        }
        let udp = sweeps.udp_group(group_index);
        let udp_streams = udp.streams;
        // 第 0 组沿用原来的单元名：改名会改掉 resume id，让历史 PASS 全部失效。
        // 别的组各带一个后缀，否则同一对的两批单元同名、resume 里互相覆盖。
        let suffix = if group_index == 0 {
            String::new()
        } else {
            format!("-g{}", group_index + 1)
        };
        let src_pinned = sweeps.pinned_senders.contains(&pair.src);
        let dst_pinned = sweeps.pinned_senders.contains(&pair.dst);
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
            let mut spec = base(
                format!("ui-{}-udp{suffix}-pinned", idx + 1),
                vec!["udp".into()],
            );
            spec.direction = OneOrMany::Many(pinned);
            spec.udp_streams = Some(udp_streams);
            // -b 被网口钉死，但 -l 档位仍要逐档跑：钉住的是带宽，不是报文长度。
            spec.udp_profiles = Some(udp_profiles_for(placeholder, &udp.lengths, &udp.windows));
            tests.push(spec);
        }
        if !swept.is_empty() {
            // 还有腿没被覆盖的方向照常逐档扫描；已覆盖的那条腿在每个单元里
            // 保持固定值（双向单元里一钉一扫就是这种情况）。
            let mut spec = base(format!("ui-{}-udp{suffix}", idx + 1), vec!["udp".into()]);
            spec.direction = OneOrMany::Many(swept);
            spec.udp_streams = Some(udp_streams);
            spec.udp_profiles = Some(udp.profiles());
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
        if !sweeps.ping_sizes.is_empty() {
            spec.ping_payload_sizes = Some(sweeps.ping_sizes.clone());
        }
        tests.push(spec);
    }
    tests
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

/// 一个单元里每条腿最终下发的参数，一行一条腿。
///
/// 直接读 `IperfTask.extra`——那就是要交给 iperf3 的东西，不是这里再算一遍。
/// 再算一遍就会有第二份口径，两份迟早对不上，而这行字存在的意义正是「所见即
/// 所跑」。
fn unit_load_lines(unit: &builder::Unit) -> Vec<String> {
    unit.legs
        .iter()
        .filter_map(|leg| {
            let (task, streams) = match &leg.kind {
                builder::LegKind::IperfSingle(task) => (task, 1),
                builder::LegKind::IperfGroup { streams, .. } => (streams.first()?, streams.len()),
                // ctsTraffic 和 ping 的参数不在这套 -b/-l/-w 里，标题已经说清了。
                _ => return None,
            };
            let mut text = String::new();
            if !leg.tag.is_empty() {
                text.push_str(match leg.tag.as_str() {
                    "ab" => "A→B ",
                    "ba" => "B→A ",
                    other => other,
                });
            }
            text.push_str(&readable_args(&task.extra));
            // iperf3 的 `-P` 由它自己开流，UDP 这边是我们逐流起进程，
            // 两种「流数」在命令里长得不一样，所以只给后者补一句。
            if task.udp && streams > 1 {
                text.push_str(&format!(" ×{streams} 流"));
            }
            Some(text)
        })
        .collect()
}

/// 命令参数照抄，只把 `-b` 那个数换成 Mbps 写法。
///
/// 下发的 `-b` 是精确的 bit/s 整数（`UdpLoad::iperf_arg`，为的是不依赖 iperf3
/// 对 `Gbps` 这类长后缀的非文档行为）。原样打印出来是 `-b 1000000000`——十个零
/// 要一个个数，而这一行存在的意义是"跟你填的那个数对得上"。换算成 Mbps 是同一个
/// 数字换个写法，不是重算，所以"所见即所跑"没有被破坏。
///
/// 顺带避免一个真实的坑：把 `1000000000` 抄回 `-b` 输入框，那里的裸数字按 **Mbps**
/// 算（见 `UdpProfile::parsed_bandwidth`），于是变成 10^9 Mbps。
fn readable_args(extra: &[String]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(extra.len());
    let mut iter = extra.iter().peekable();
    while let Some(arg) = iter.next() {
        out.push(arg.clone());
        if arg != "-b" {
            continue;
        }
        let Some(value) = iter.peek() else { continue };
        let Ok(bits) = value.parse::<u64>() else {
            continue;
        };
        iter.next();
        let mbps = bits as f64 / 1_000_000.0;
        out.push(if (mbps.fract()).abs() < f64::EPSILON {
            format!("{mbps:.0} Mbps")
        } else {
            format!("{mbps:.1} Mbps")
        });
    }
    out.join(" ")
}

/// Encode an arbitrary user/project ID before embedding it in the internal
/// slash-delimited TestSpec name.  UI IDs are normally generated as hex, but
/// the HTTP API and imported project files are allowed to carry human IDs such
/// as `wifi/a`; letting those raw slashes through shifts every following trace
/// field and makes the preview point at the wrong suite/task.  Percent-escape
/// every byte outside the URI unreserved set so the transform is reversible
/// for UTF-8 as well as punctuation.
fn ui_name_segment(raw: &str) -> String {
    urlencode(raw)
}

fn ui_name_segment_decode(raw: &str) -> String {
    urldecode(raw)
}

fn topology_fingerprint(state: &UiState) -> String {
    let value = serde_json::json!({
        "master": state.master,
        "agent": state.agent,
    });
    md5_hex(&serde_json::to_string(&value).unwrap_or_default())
}

fn request_plan_hash(req: &RunRequest, cfg: &Config, state: &UiState) -> String {
    let mut normalized = req.clone();
    normalized.plan_hash = None;
    if let Some(plan) = normalized.ui_plan.as_mut() {
        plan.plan_hash = None;
    }
    let request_json = serde_json::to_string(&normalized).unwrap_or_default();
    let config_json = serde_json::to_string(cfg).unwrap_or_default();
    let topology = topology_fingerprint(state);
    md5_hex(&format!(
        "ui-plan-v1|{topology}|{request_json}|{config_json}"
    ))
}

fn ui_source_from_test_name(name: &str) -> Option<UiSource> {
    let mut parts = name.split('/');
    if parts.next()? != "ui-plan" {
        return None;
    }
    let link_set_id = ui_name_segment_decode(parts.next()?);
    let _binding_id = ui_name_segment_decode(parts.next()?);
    Some(UiSource {
        pair_id: ui_name_segment_decode(parts.next()?),
        link_set_id,
        suite_id: ui_name_segment_decode(parts.next()?),
        task_id: ui_name_segment_decode(parts.next()?),
        recipe_id: ui_name_segment_decode(parts.next()?),
        protocol: parts.next()?.split('-').next()?.to_string(),
    })
}

fn unit_protocol(unit: &builder::Unit) -> Option<String> {
    unit.legs.first().map(|leg| match &leg.kind {
        builder::LegKind::IperfSingle(task) => {
            if task.udp {
                "udp".to_string()
            } else {
                "tcp".to_string()
            }
        }
        builder::LegKind::IperfGroup { streams, .. } => {
            if streams.first().is_some_and(|task| task.udp) {
                "udp".to_string()
            } else {
                "tcp".to_string()
            }
        }
        builder::LegKind::CtsTraffic(task) => {
            if task.udp {
                "udp".to_string()
            } else {
                "tcp".to_string()
            }
        }
        builder::LegKind::Ping(_) => "ping".to_string(),
    })
}

fn unit_effective_args(unit: &builder::Unit) -> Vec<String> {
    unit.legs
        .iter()
        .flat_map(|leg| match &leg.kind {
            builder::LegKind::IperfSingle(task) => task.extra.clone(),
            builder::LegKind::IperfGroup { streams, .. } => streams
                .first()
                .map(|task| task.extra.clone())
                .unwrap_or_default(),
            _ => Vec::new(),
        })
        .collect()
}

/// Return the concrete endpoints carried by a unit's first leg.
///
/// `builder::Leg::tag` is intentionally empty for a one-way leg (the tag is
/// reserved for the two legs inside a bidirectional unit), so it cannot be
/// used as the direction source for the quick-plan trace.  Looking at the
/// resolved endpoints keeps the trace correct for both A→B and B→A without
/// changing the executor/reporting semantics of `Leg::tag`.
fn leg_endpoints(leg: &builder::Leg) -> Option<(&builder::Endpoint, &builder::Endpoint)> {
    match &leg.kind {
        builder::LegKind::IperfSingle(task) => Some((&task.src, &task.dst)),
        builder::LegKind::IperfGroup { streams, .. } => {
            streams.first().map(|task| (&task.src, &task.dst))
        }
        builder::LegKind::CtsTraffic(task) => Some((&task.src, &task.dst)),
        builder::LegKind::Ping(task) => Some((&task.src, &task.dst)),
    }
}

/// Resolve the direction represented by a built unit relative to its source
/// `TestSpec`.  Bidirectional units are one concurrent unit with two legs;
/// one-way units have an empty leg tag, so compare endpoint keys instead.
fn unit_direction_for_spec(unit: &builder::Unit, spec: &builder::SpecNorm) -> Option<String> {
    if unit.bidir {
        return Some("bidir".into());
    }
    let (src, dst) = leg_endpoints(unit.legs.first()?)?;
    if src.key() == spec.src.key() && dst.key() == spec.dst.key() {
        Some("ab".into())
    } else if src.key() == spec.dst.key() && dst.key() == spec.src.key() {
        Some("ba".into())
    } else {
        None
    }
}

fn compile_request(state: &UiState, req: &RunRequest) -> Result<CompiledPlan, String> {
    validate_request(state, req)?;
    let cfg = config_from_request(state, req);
    let problems = cfg.validate();
    if !problems.is_empty() {
        return Err(format!("配置项异常：{}", problems.join("；")));
    }
    let mut notices = Vec::new();
    let mut spec_errors = Vec::new();
    let mut units = Vec::new();
    let mut sources: Vec<Option<UiSource>> = Vec::new();
    let mut source_directions: Vec<Option<String>> = Vec::new();
    let mut port = builder::PORT_BASE;

    if req.ui_plan.is_some() {
        // Build each spec separately so every generated unit can be traced back
        // to its suite task.  Port allocation remains global and deterministic.
        for test in &cfg.tests {
            match builder::spec_from_config(test, &cfg, &state.master, &state.agent) {
                Ok(spec) => {
                    let (mut built, build_notices) = build_units(
                        std::slice::from_ref(&spec),
                        cfg.require_same_subnet_for_iperf,
                        &mut port,
                    );
                    notices.extend(build_notices);
                    let source = ui_source_from_test_name(&test.name);
                    // `Leg::tag` is intentionally empty for one-way units, so
                    // retain the concrete A→B/B→A direction while the named
                    // source spec is still available.  Bidirectional units
                    // are represented by a single unit and remain `bidir`.
                    for unit in &built {
                        sources.push(source.clone());
                        source_directions.push(unit_direction_for_spec(unit, &spec));
                    }
                    units.append(&mut built);
                }
                Err(error) => {
                    spec_errors.push(format!("{} 无法生成任务：{error}", test.name));
                    notices.push(format!("跳过 {}: {error}", test.name));
                }
            }
        }
    } else {
        let mut specs = Vec::new();
        for test in &cfg.tests {
            match builder::spec_from_config(test, &cfg, &state.master, &state.agent) {
                Ok(spec) => specs.push(spec),
                Err(error) => {
                    spec_errors.push(format!("{} 无法生成任务：{error}", test.name));
                    notices.push(format!("跳过 {}: {error}", test.name));
                }
            }
        }
        let (built, build_notices) =
            build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
        notices.extend(build_notices);
        units = built;
        sources.resize(units.len(), None);
        source_directions.resize(units.len(), None);
    }

    if req.ui_plan.is_some() {
        // Stable builder IDs include the effective protocol/profile/endpoint
        // shape.  If two bindings accidentally describe that same shape, keep
        // one execution unit and make the reduction visible to the caller.
        let mut seen_ids = HashSet::new();
        let mut unique_units = Vec::with_capacity(units.len());
        let mut unique_sources = Vec::with_capacity(sources.len());
        let mut unique_directions = Vec::with_capacity(source_directions.len());
        for (index, unit) in units.into_iter().enumerate() {
            if seen_ids.insert(unit.id.clone()) {
                unique_units.push(unit);
                unique_sources.push(sources.get(index).cloned().flatten());
                unique_directions.push(source_directions.get(index).cloned().flatten());
            }
        }
        let removed_count = sources.len().saturating_sub(unique_units.len());
        if removed_count > 0 {
            notices.push(format!(
                "计划去重：移除了 {removed_count} 个最终参数完全相同的重复单元"
            ));
        }
        units = unique_units;
        sources = unique_sources;
        source_directions = unique_directions;
    }

    let resumed = if cfg.resume {
        let db = ResultDb::load(std::path::PathBuf::from("task_results.json"));
        units
            .iter()
            .map(|unit| db.fresh_pass(&unit.id).is_some())
            .collect()
    } else {
        vec![false; units.len()]
    };
    let plan_hash = request_plan_hash(req, &cfg, state);
    let topology_fingerprint = topology_fingerprint(state);
    let mut trace = Vec::with_capacity(units.len());
    let mut sections = Vec::new();
    for (index, unit) in units.iter().enumerate() {
        let source = sources.get(index).and_then(|source| source.clone());
        let (pair_id, link_set_id, suite_id, task_id, recipe_id) = source
            .as_ref()
            .map(|source| {
                (
                    Some(source.pair_id.clone()),
                    (!source.link_set_id.is_empty()).then(|| source.link_set_id.clone()),
                    Some(source.suite_id.clone()),
                    Some(source.task_id.clone()),
                    Some(source.recipe_id.clone()),
                )
            })
            .unwrap_or((None, None, None, None, None));
        let protocol = source
            .as_ref()
            .map(|source| source.protocol.clone())
            .or_else(|| unit_protocol(unit));
        let direction = source_directions.get(index).cloned().flatten().or_else(|| {
            (!unit.legs.is_empty()).then(|| {
                unit.legs
                    .iter()
                    .map(|leg| {
                        if leg.tag.is_empty() {
                            "ab"
                        } else {
                            leg.tag.as_str()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
        });
        let ip = if unit.title.contains(" V6 ") {
            Some("v6".into())
        } else if unit.title.contains(" V4 ") {
            Some("v4".into())
        } else {
            None
        };
        let effective_args = unit_effective_args(unit);
        trace.push(PlanTrace {
            seq: index + 1,
            pair_id: pair_id.clone(),
            link_set_id: link_set_id.clone(),
            suite_id: suite_id.clone(),
            task_id: task_id.clone(),
            lane_id: task_id.clone(),
            recipe_id: recipe_id.clone(),
            protocol: protocol.clone(),
            direction,
            ip,
            requested_args: effective_args.clone(),
            effective_args,
            value_sources: if req.ui_plan.is_some() {
                vec!["suite recipe（网口策略/链路裁剪由 builder 最终决定）".into()]
            } else {
                vec!["legacy matrix".into()]
            },
            skipped_reason: None,
            resumed: resumed[index],
        });
        let key = (link_set_id.clone(), suite_id.clone(), task_id.clone());
        if let Some(section) = sections.iter_mut().find(|section: &&mut PlanSection| {
            (
                section.link_set_id.clone(),
                section.suite_id.clone(),
                section.task_id.clone(),
            ) == key
        }) {
            section.unit_seqs.push(index + 1);
        } else {
            sections.push(PlanSection {
                link_set_id,
                suite_id,
                task_id,
                title: unit.title.clone(),
                unit_seqs: vec![index + 1],
            });
        }
    }
    if req.ui_plan.is_none() {
        // Keep the legacy response compact and backwards-compatible; hierarchy
        // is only meaningful for the suite planner.
        trace.clear();
        sections.clear();
    }
    Ok(CompiledPlan {
        cfg,
        units,
        notices,
        resumed,
        trace,
        sections,
        plan_hash,
        topology_fingerprint,
        spec_errors,
    })
}

fn api_plan(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: RunRequest = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let state = lock_recover(&console.state);
    if state.master.interfaces.is_empty() || state.agent.interfaces.is_empty() {
        return Err("还没连上辅测机，先点「连接」".into());
    }
    let mut compiled = compile_request(&state, &req)?;
    // resume 开着时提示并扣除预判会跳过的单元；executor 运行时仍会再判一次。
    let skip_count = compiled.resumed.iter().filter(|skipped| **skipped).count();
    if compiled.cfg.resume {
        compiled.notices.push(if skip_count == 0 {
            format!(
                "resume 已开启，但 {RESUME_MAX_AGE_HOURS} 小时内没有可复用的 PASS，{} 个单元全部实跑",
                compiled.units.len()
            )
        } else {
            format!(
                "resume 已开启：{skip_count}/{} 个单元在 {RESUME_MAX_AGE_HOURS} 小时内已 PASS，预计跳过。执行时还会再判一次",
                compiled.units.len()
            )
        });
    }
    let est_total_secs = compiled
        .units
        .iter()
        .zip(&compiled.resumed)
        .filter(|(_, skipped)| !**skipped)
        .map(|(u, _)| u.est_secs)
        .sum();
    let est_full_secs = compiled.units.iter().map(|u| u.est_secs).sum();
    let units = compiled
        .units
        .iter()
        .zip(&compiled.resumed)
        .enumerate()
        .map(|(idx, (unit, skipped))| PlannedUnit {
            seq: idx + 1,
            title: unit.title.clone(),
            est_secs: unit.est_secs,
            resumed: *skipped,
            load: unit_load_lines(unit),
        })
        .collect();
    serde_json::to_value(PlanOut {
        units,
        est_total_secs,
        est_full_secs,
        notices: compiled.notices,
        sections: compiled.sections,
        trace: compiled.trace,
        plan_hash: Some(compiled.plan_hash),
        topology_fingerprint: Some(compiled.topology_fingerprint),
        ui_plan_supported: true,
    })
    .map_err(|e| e.to_string())
}

fn api_config(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: RunRequest = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let state = lock_recover(&console.state);
    if state.master.interfaces.is_empty() || state.agent.interfaces.is_empty() {
        return Err("还没连上辅测机，先点「连接」".into());
    }
    let compiled = compile_request(&state, &req)?;
    serde_json::to_value(compiled.cfg).map_err(|error| format!("生成配置失败: {error}"))
}

/// 一行矩阵勾选的回填值。`PairSelection` 只有 `Deserialize`——它是请求方向的
/// 类型，回填是相反方向，两者字段名必须一致但生命周期不同，分开写比给请求类型
/// 加一个只在这里用的 `Serialize` 更不容易在改动时互相带偏。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct PairImport {
    src: String,
    dst: String,
    directions: Vec<String>,
    rx_target_bidir_ab: String,
    rx_target_bidir_ba: String,
    udp_groups: Vec<usize>,
    tcp_groups: Vec<usize>,
    transports: Vec<String>,
    ip: Vec<String>,
}

/// 导入时从 `tests[]` 里认出来的 UDP 参数组。字段和 `UdpGroup` 一致，
/// 方向相反（那个是请求，这个是回填）。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct UdpGroupOut {
    name: String,
    bandwidths: Vec<String>,
    lengths: Vec<String>,
    windows: Vec<String>,
    streams: u32,
}

/// 导入时从 `tests[]` 里认出来的 TCP 参数组。字段和 `TcpGroup` 一致。
///
/// 一个 TCP 组会被 `config_from_request` 拆成好几条 TestSpec（每个 `-P` 一条，
/// 都带着这组的那份 `-w` 列表），所以回填时按「相同的 `-w` 列表」把它们并回一组，
/// 把各条的 `-P` 收成这一组的流数档位。两组恰好用同一份 `-w` 时会被并成一组，
/// 但跑出来的单元完全一样，不影响结果。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct TcpGroupOut {
    name: String,
    windows: Vec<String>,
    streams: Vec<u32>,
}

#[derive(Debug, Serialize)]
struct ImportOut {
    /// 顶部参数区，字段和 `/api/bootstrap` 完全一致——页面用同一段代码回填，
    /// 免得「导入」和「打开页面」两条路把同一个输入框填成两种样子。
    settings: BootstrapOut,
    /// 这两项 `/api/bootstrap` 有意不回填（见 `RunRequest` 上的注释：不能让
    /// 同一个勾选框在不同机器上悄悄变成不同含义）。导入是人明确要求「按这份
    /// 文件来」，回填它们是对的，但要在 `notices` 里说一声。
    limit_udp_by_link_speed: bool,
    resume: bool,
    pairs: Vec<PairImport>,
    /// 默认组之外的组；矩阵行上的 `udp_group` 按 1 起指向它们。
    udp_groups: Vec<UdpGroupOut>,
    /// 默认组之外的 TCP 组；矩阵行上的 `tcp_group` 按 1 起指向它们。
    tcp_groups: Vec<TcpGroupOut>,
    nic_policies: Vec<NicPolicySelection>,
    /// 导入过程中丢掉或改写了什么。空列表 = 这份文件被完整表示了。
    notices: Vec<String>,
}

/// 导入一份 config.json，回填成界面状态。
///
/// 「下载 config.json」一直是单向的：改完一堆门限和档位，下次打开控制台又得
/// 从头点一遍，而那份文件里明明什么都有。这里做的是它的逆运算——把 config
/// 翻回界面选择，**不执行任何东西**。
///
/// 有意不要求先连上辅测机：全局参数和网口策略不依赖连接，配对选择留给页面在
/// 连上之后按端点名匹配（对不上的行会在 `notices` 里点名）。
fn api_import(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let incoming: Config = serde_json::from_str(body)
        .map_err(|error| format!("这不是一份能解析的 config.json：{error}"))?;
    let problems = incoming.validate();
    if !problems.is_empty() {
        return Err(format!("配置项异常，已拒绝导入：{}", problems.join("；")));
    }

    let mut state = lock_recover(&console.state);
    let mut notices = Vec::new();
    // 连接身份单独处理：token 空着时保留当前值。下载下来的 config 里带着
    // agent_token，但人手写的那份多半没有——用文件里的空串把已经连上的
    // 令牌冲掉，表现是导入之后「连接」突然 401。
    if incoming.agent_token.trim().is_empty() && !state.cfg.agent_token.trim().is_empty() {
        notices.push("文件里没有 agent_token，沿用当前已加载的令牌。".into());
    } else {
        state.cfg.agent_token = incoming.agent_token.clone();
    }
    let agent_token = state.cfg.agent_token.clone();
    let master = state.master.clone();
    let agent = state.agent.clone();

    state.cfg = Config {
        agent_token,
        ..incoming
    };
    if !state.cfg.agent_host.trim().is_empty() {
        state.agent_host = state.cfg.agent_host.trim().to_string();
    }

    if state.cfg.pairs.is_some() || state.cfg.universal_params.is_some() {
        notices.push(
            "文件用的是 pairs/universal_params 自动配对，界面矩阵是逐对勾选的，表示不了；\
             全局参数已导入，配对请在矩阵里自己勾。"
                .into(),
        );
    }
    let connected = !master.interfaces.is_empty() && !agent.interfaces.is_empty();
    if !connected && !state.cfg.tests.is_empty() {
        notices.push("还没连上辅测机，配对选择先存着；点「连接」扫到网口后会自动勾上。".into());
    }
    // settings 要先算：逐对的 UDP 覆盖是「和全局不一样的那部分」，
    // 没有全局值就判不出哪些该回填到行上。
    let settings = bootstrap_out(&state);
    // 默认组 = 执行区那几个框。文件里和它不一样的 UDP 参数会被认成附加组。
    let default_group = UdpGroupOut {
        name: "默认".into(),
        bandwidths: settings.udp_bandwidths.clone(),
        lengths: settings.udp_lengths.clone(),
        windows: settings.udp_windows.clone(),
        streams: settings.udp_streams,
    };
    // TCP 默认组 = 执行区的 `-w` / `-P`；文件里和它不一样的 TCP 参数认成附加组。
    let default_tcp_group = TcpGroupOut {
        name: "默认".into(),
        windows: settings.tcp_windows.clone(),
        streams: settings.tcp_streams.clone(),
    };
    let (pairs, udp_groups, tcp_groups, pair_notices) = pairs_from_tests(
        &state.cfg,
        &master,
        &agent,
        &default_group,
        &default_tcp_group,
    );
    notices.extend(pair_notices);
    if state.cfg.limit_udp_by_link_speed || state.cfg.resume {
        notices.push("「按链路上限裁剪」和「resume」按文件里的值勾上了，跑之前确认一眼。".into());
    }

    let nic_policies = configured_nic_policies(&state.cfg, &master, &agent);
    serde_json::to_value(ImportOut {
        settings,
        udp_groups,
        tcp_groups,
        limit_udp_by_link_speed: state.cfg.limit_udp_by_link_speed,
        resume: state.cfg.resume,
        pairs,
        nic_policies,
        notices,
    })
    .map_err(|error| format!("回填界面失败: {error}"))
}

/// `tests[]` -> 矩阵行。
///
/// 一行矩阵会被 `config_from_request` 拆成好几条 TestSpec（TCP 的每个 `-P`
/// 档位一条、UDP 钉死/扫描各一条、ping 一条），所以这里按端点对合并回去，
/// 方向、协议、IP 版本取并集。
///
/// 反向的那条（`dst`/`src` 调过来写）合并进同一行并把方向对调：矩阵一行代表的
/// 是一对网口，A、B 谁在左边由界面的枚举顺序决定，不由文件决定。
fn pairs_from_tests(
    cfg: &Config,
    master: &HostInfo,
    agent: &HostInfo,
    default_group: &UdpGroupOut,
    default_tcp_group: &TcpGroupOut,
) -> (
    Vec<PairImport>,
    Vec<UdpGroupOut>,
    Vec<TcpGroupOut>,
    Vec<String>,
) {
    let mut out: Vec<PairImport> = Vec::new();
    let mut groups: Vec<UdpGroupOut> = Vec::new();
    let mut tcp_groups: Vec<TcpGroupOut> = Vec::new();
    // 与 `out` 同序：每行按「相同 -w 列表」聚起它跑过的 TCP 档位（-w 列表 -> 各 -P）。
    let mut tcp_accum: Vec<std::collections::HashMap<Vec<String>, Vec<u32>>> = Vec::new();
    let mut notices = Vec::new();
    let mut ragged = false;
    let mut unresolved: Vec<String> = Vec::new();
    for test in &cfg.tests {
        let (Some(src), Some(dst)) = (
            canonical_endpoint(&test.src, master, agent),
            canonical_endpoint(&test.dst, master, agent),
        ) else {
            for raw in [&test.src, &test.dst] {
                if canonical_endpoint(raw, master, agent).is_none()
                    && !unresolved.iter().any(|seen| seen == raw)
                {
                    unresolved.push(raw.clone());
                }
            }
            continue;
        };
        let directions = test.direction.directions();
        let mut transports: Vec<String> = test
            .transports
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| t == "tcp" || t == "udp")
            .collect();
        let transports_have_udp = transports.iter().any(|t| t == "udp");
        let transports_have_tcp = transports.iter().any(|t| t == "tcp");
        // ping 在配置模型里挂在 kinds 上，界面把它和 TCP/UDP 并排放在「协议」
        // 列——回填时要走相反的那一步，否则纯 ping 的配置导进来是一行空协议。
        if test.kinds.iter().any(|kind| kind.trim() == "ping") {
            transports.push("ping".into());
        }
        let ip: Vec<String> = test
            .ip
            .iter()
            .map(|v| v.trim().to_lowercase())
            .filter(|v| v == "v4" || v == "v6")
            .collect();
        let bidir = test.rate_targets_bidir_mbps.clone().unwrap_or_default();

        let (idx, flip) =
            if let Some(idx) = out.iter().position(|row| row.src == src && row.dst == dst) {
                (idx, false)
            } else if let Some(idx) = out.iter().position(|row| row.src == dst && row.dst == src) {
                (idx, true)
            } else {
                out.push(PairImport {
                    src: src.clone(),
                    dst: dst.clone(),
                    ..Default::default()
                });
                (out.len() - 1, false)
            };
        // tcp_accum 与 out 对齐：新行出现就补一份空表（未解析的 test 在 idx
        // 之前就 continue 了，不会打乱对齐）。
        while tcp_accum.len() < out.len() {
            tcp_accum.push(std::collections::HashMap::new());
        }
        if transports_have_tcp {
            // 一条 TCP test 带着这一组的整份 -w 列表和它自己那一个 -P。手写配置
            // 可能没写 -w（None）——按默认组的窗口回填；-P 缺省按单流。
            let windows = test
                .tcp_windows
                .clone()
                .unwrap_or_else(|| default_tcp_group.windows.clone());
            let stream = test.tcp_streams.filter(|value| *value > 0).unwrap_or(1);
            let steps = tcp_accum[idx].entry(windows).or_default();
            if !steps.contains(&stream) {
                steps.push(stream);
            }
        }
        let row = &mut out[idx];
        for direction in directions {
            let direction = if flip {
                match direction.as_str() {
                    "ab" => "ba".to_string(),
                    "ba" => "ab".to_string(),
                    other => other.to_string(),
                }
            } else {
                direction
            };
            if !row.directions.contains(&direction) {
                row.directions.push(direction);
            }
        }
        for transport in transports {
            if !row.transports.contains(&transport) {
                row.transports.push(transport);
            }
        }
        for version in ip {
            if !row.ip.contains(&version) {
                row.ip.push(version);
            }
        }
        // 这条 test 的 UDP 参数和默认组一样吗？不一样就认成一个附加组，
        // 同样的参数只认一次（几十条 test 常常只有两三种打法）。
        //
        // 同一对可以有好几条 UDP test（一行选了多组），所以是**往这一行的组
        // 列表里加**，不是只认第一条。
        //
        // 发送端在 `by_nic` 里另有 `-b` 时跳过：那种情况下文件里的 profile 是
        // 占位值（见 `config_from_request` 的 pinned 分支），不是人填的选择。
        if transports_have_udp && !test_udp_all_directions_pinned(cfg, test, &src, &dst) {
            if let Some(profiles) = &test.udp_profiles {
                let (group, exact) = udp_group_from_profiles(
                    profiles,
                    test.udp_streams
                        .filter(|v| *v > 0)
                        .unwrap_or(default_group.streams),
                );
                ragged |= !exact;
                let selected = if group.bandwidths.is_empty() || group.same_run_as(default_group) {
                    0
                } else {
                    groups
                        .iter()
                        .position(|known| known.same_run_as(&group))
                        .unwrap_or_else(|| {
                            let mut named = group.clone();
                            named.name = format!("组 {}", groups.len() + 2);
                            groups.push(named);
                            groups.len() - 1
                        })
                        + 1
                };
                if !row.udp_groups.contains(&selected) {
                    row.udp_groups.push(selected);
                }
            }
        }

        let (ab, ba) = if flip {
            (bidir.ba, bidir.ab)
        } else {
            (bidir.ab, bidir.ba)
        };
        for (slot, value) in [
            (&mut row.rx_target_bidir_ab, ab),
            (&mut row.rx_target_bidir_ba, ba),
        ] {
            if let Some(value) = value.filter(|v| v.is_finite() && *v > 0.0) {
                if slot.is_empty() {
                    *slot = format_mbps(value);
                }
            }
        }
    }
    // 一条 UDP test 都没认出来的行（纯 TCP/ping，或者被网口值钉死的那种）
    // 明确写成「默认组」，别留一个空列表让页面去猜。
    for row in &mut out {
        if row.udp_groups.is_empty() {
            row.udp_groups.push(0);
        }
    }
    // TCP 组回填：按「相同 -w 列表」把同一行的 TCP test 并回一组，各条的 -P 收成
    // 这组的流数档位。两组恰好共用一份 -w 会被并成一组，但跑出来的单元一样。
    for (idx, accum) in tcp_accum.iter().enumerate() {
        // 稳定顺序：按 -w 列表排一下，免得每次导入组的编号乱跳。
        let mut entries: Vec<(&Vec<String>, &Vec<u32>)> = accum.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (windows, streams) in entries {
            let mut streams = streams.clone();
            streams.sort_unstable();
            let candidate = TcpGroupOut {
                name: String::new(),
                windows: windows.clone(),
                streams,
            };
            let selected = if candidate.same_run_as(default_tcp_group) {
                0
            } else {
                tcp_groups
                    .iter()
                    .position(|known| known.same_run_as(&candidate))
                    .unwrap_or_else(|| {
                        let mut named = candidate.clone();
                        named.name = format!("TCP 组 {}", tcp_groups.len() + 2);
                        tcp_groups.push(named);
                        tcp_groups.len() - 1
                    })
                    + 1
            };
            if !out[idx].tcp_groups.contains(&selected) {
                out[idx].tcp_groups.push(selected);
            }
        }
    }
    // 没认出任何 TCP test 的行（纯 UDP/ping）也写上默认组：矩阵行总有个选择。
    for row in &mut out {
        if row.tcp_groups.is_empty() {
            row.tcp_groups.push(0);
        }
    }
    if !unresolved.is_empty() {
        notices.push(format!(
            "这些端点在当前网口表里找不到，相关配对没有导入：{}",
            unresolved.join("、")
        ));
    }
    if ragged {
        notices.push(
            "文件里有 UDP 档位不是「每档 -b × 每档 -l × 每档 -w」的整齐组合（手写配置\
             常见）。参数组按三个轴各取一次去重来表示，导入后跑的档位会比文件里多；\
             要原样跑请直接 `master --auto --config 那个文件`。"
                .into(),
        );
    }
    if !tcp_groups.is_empty() {
        notices.push(format!(
            "文件里有 {} 组和默认组不同的 TCP 参数，已建成附加组并按行选好。",
            tcp_groups.len()
        ));
    }
    if !groups.is_empty() {
        notices.push(format!(
            "文件里有 {} 组和默认组不同的 UDP 参数，已建成附加组并按行选好。",
            groups.len()
        ));
    }
    (out, groups, tcp_groups, notices)
}

/// 一组 profile + 流数 -> 界面上的参数组。
///
/// 第二个返回值表示这份 profile 是不是一个整齐的叉积。界面上的组只能表达
/// 「每档 -b × 每档 -l × 每档 -w」，手写的配置可以不是那样（`1m/64` 加
/// `500m/1400`），那时按三个轴去重会**多**出组合，必须说出来。
fn udp_group_from_profiles(profiles: &[UdpProfile], streams: u32) -> (UdpGroupOut, bool) {
    let bandwidths = distinct(profiles.iter().map(|profile| profile.bandwidth.clone()));
    let lengths = distinct(profiles.iter().filter_map(|profile| profile.length.clone()));
    let windows = distinct(profiles.iter().filter_map(|profile| profile.window.clone()));
    let combinations = bandwidths.len().max(1) * lengths.len().max(1) * windows.len().max(1);
    let exact = combinations == profiles.len();
    (
        UdpGroupOut {
            name: String::new(),
            bandwidths,
            lengths,
            windows,
            streams,
        },
        exact,
    )
}

impl UdpGroupOut {
    /// 两组会不会跑出同一批单元。名字不算——它只是给人看的。
    fn same_run_as(&self, other: &UdpGroupOut) -> bool {
        self.bandwidths == other.bandwidths
            && self.lengths == other.lengths
            && self.windows == other.windows
            && self.streams == other.streams
    }
}

impl TcpGroupOut {
    /// 两组会不会跑出同一批单元。流数比之前先排序去重：回填时是从各条 -P
    /// 收集起来的，顺序和重复都可能和默认组那份不一样；-w 档位保持原序比较。
    fn same_run_as(&self, other: &TcpGroupOut) -> bool {
        let norm = |values: &[u32]| {
            let mut out = values.to_vec();
            out.sort_unstable();
            out.dedup();
            out
        };
        self.windows == other.windows && norm(&self.streams) == norm(&other.streams)
    }
}

/// 把 config 里的端点写法统一成矩阵用的 `master:NAME=以太网 6`。
///
/// 没连上辅测机时解析不了 `master:SGMII2.5G` 这种按角色写的端点（角色到网卡
/// 名的映射来自实扫），但已经是 `NAME=` 写法的可以原样用——先连接再导入和
/// 先导入再连接都得能走通。
fn canonical_endpoint(raw: &str, master: &HostInfo, agent: &HostInfo) -> Option<String> {
    if let Ok(endpoint) = builder::resolve_endpoint(raw, master, agent) {
        let side = match endpoint.side {
            builder::Side::Master => "master",
            builder::Side::Agent => "agent",
        };
        return Some(format!("{side}:NAME={}", endpoint.nic.name));
    }
    let (side, rest) = raw.split_once(':')?;
    let side = match side.trim().to_lowercase().as_str() {
        "master" | "local" | "主控" => "master",
        "agent" | "remote" | "辅测" => "agent",
        _ => return None,
    };
    let name = rest
        .trim()
        .strip_prefix("NAME=")
        .or_else(|| rest.trim().strip_prefix("name="))?
        .trim();
    (!name.is_empty()).then(|| format!("{side}:NAME={name}"))
}

/// 这个端点在「网口与策略」里单独指定了 UDP `-b` 吗。
///
/// 它决定文件里那条 test 的 profile 带宽是「人填的档位」还是「占位值」。
fn endpoint_pins_udp_bandwidth(cfg: &Config, endpoint: &str) -> bool {
    let Some((host, rest)) = endpoint.split_once(':') else {
        return false;
    };
    let Some(name) = rest.strip_prefix("NAME=") else {
        return false;
    };
    cfg.link_profiles.by_nic.iter().any(|profile| {
        profile
            .udp_bandwidth
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
            && profile.host.eq_ignore_ascii_case(host)
            && profile.name.eq_ignore_ascii_case(name)
    })
}

/// Whether every UDP sending leg represented by a test is pinned to a
/// per-NIC bandwidth override.
///
/// A config generated from the matrix may split one logical pair into two UDP
/// tests when only one endpoint is pinned: the pinned direction carries a
/// placeholder profile, while the unpinned direction still carries the
/// user's sweep.  Treating the pair as pinned merely because *either*
/// endpoint has an override would make `api_import` discard that sweep and
/// silently turn the row back into the default UDP group.  Decide per test,
/// using its concrete directions, so only a test whose every sending leg is
/// pinned is ignored during group reconstruction.
fn test_udp_all_directions_pinned(cfg: &Config, test: &TestSpec, src: &str, dst: &str) -> bool {
    let src_pinned = endpoint_pins_udp_bandwidth(cfg, src);
    let dst_pinned = endpoint_pins_udp_bandwidth(cfg, dst);
    let directions = test.direction.directions();
    !directions.is_empty()
        && directions.iter().all(|direction| match direction.as_str() {
            "ab" => src_pinned,
            "ba" => dst_pinned,
            "bidir" => src_pinned && dst_pinned,
            _ => false,
        })
}

/// 门限回填成人写得出来的样子：整数不带小数点，其余保留一位。
fn format_mbps(value: f64) -> String {
    if (value.fract()).abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

#[allow(dead_code)]
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
        match compile_request(&state, &req) {
            Ok(compiled) => {
                if let Some(error) = compiled.spec_errors.first() {
                    console.running.store(false, Ordering::SeqCst);
                    return Err(error.clone());
                }
                if req.ui_plan.is_some()
                    && compiled
                        .notices
                        .iter()
                        .any(|notice| notice.trim_start().starts_with("跳过 "))
                {
                    console.running.store(false, Ordering::SeqCst);
                    return Err("计划包含不可执行的链路或 IP 版本，请在复核页排除后重新预览".into());
                }
                if req.ui_plan.is_some() {
                    let supplied = req.plan_hash.as_deref().or_else(|| {
                        req.ui_plan
                            .as_ref()
                            .and_then(|plan| plan.plan_hash.as_deref())
                    });
                    let Some(supplied) = supplied.filter(|value| !value.trim().is_empty()) else {
                        console.running.store(false, Ordering::SeqCst);
                        return Err("请先预览任务并携带 plan_hash 后再开始测试".into());
                    };
                    if supplied != compiled.plan_hash {
                        console.running.store(false, Ordering::SeqCst);
                        return Err("计划已过期或网口拓扑已变化，请重新预览任务".into());
                    }
                }
                if compiled.units.is_empty() {
                    console.running.store(false, Ordering::SeqCst);
                    let detail = if compiled.notices.is_empty() {
                        String::new()
                    } else {
                        format!("：{}", compiled.notices.join("；"))
                    };
                    return Err(format!("所选配置最终没有生成任何测试单元{detail}"));
                }
                compiled.cfg
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

/// 一路监控的取样游标。
#[derive(Debug, Deserialize)]
struct MonitorCursor {
    session: String,
    #[serde(default)]
    from: usize,
}

/// 一次问完全部在跑的监控。
///
/// 每路各发一次请求也能work，但浏览器对同一个源的并发连接就那么几条：
/// 8 路监控 + 运行进度轮询会把它占满，日志那一路开始一秒一顿。
#[derive(Debug, Deserialize)]
struct MonitorPollReq {
    #[serde(default)]
    cursors: Vec<MonitorCursor>,
}

#[derive(Debug, Serialize)]
struct MonitorSeriesOut {
    session: String,
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

/// 批量取样。一路会话已经结束不影响其余各路：那一路自己带着 `running:false`
/// 和原因回去，页面把它从图上摘掉即可。整个请求报错的话，正在跑的曲线会一起
/// 断掉，而它们其实好好的。
fn api_monitor_samples(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: MonitorPollReq =
        serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    if req.cursors.len() > MONITOR_MAX_SESSIONS {
        return Err(format!("一次最多问 {MONITOR_MAX_SESSIONS} 路监控"));
    }
    let mut monitors = lock_recover(&console.monitors);
    // 顺手收摊。页面轮询是这张表唯一的常规活动，回收挂在这里才不会
    // 依赖「有人再开一路监控」才发生。
    reap_dead_monitors(&mut monitors);
    let series: Vec<MonitorSeriesOut> = req
        .cursors
        .iter()
        .map(|cursor| {
            let Some(entry) = monitors.get(&cursor.session) else {
                return MonitorSeriesOut {
                    session: cursor.session.clone(),
                    side: String::new(),
                    iface: String::new(),
                    from: cursor.from,
                    points: Vec::new(),
                    running: false,
                    error: "监控会话已结束".into(),
                };
            };
            let mut data = lock_recover(&entry.data);
            data.last_poll = Some(std::time::Instant::now());
            // 游标是绝对序号；被环形缓冲挤掉的部分直接跳过，不能装作它还在。
            let start = cursor.from.max(data.dropped) - data.dropped;
            let points: Vec<MonitorPoint> = data.points.iter().skip(start).cloned().collect();
            MonitorSeriesOut {
                session: cursor.session.clone(),
                side: entry.side.clone(),
                iface: entry.iface.clone(),
                from: data.dropped + data.points.len(),
                points,
                running: data.running,
                error: data.error.clone().unwrap_or_default(),
            }
        })
        .collect();
    serde_json::to_value(serde_json::json!({ "series": series })).map_err(|e| e.to_string())
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
    use serde_json::json;

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
                udp_groups: Vec::new(),
                tcp_groups: Vec::new(),
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
            udp_groups: Vec::new(),
            tcp_groups: Vec::new(),
            ping_count: 0,
            ping_payload_sizes: Vec::new(),
            limit_udp_by_link_speed: false,
            resume: false,
            screenshot: false,
            ui_plan: None,
            plan_hash: None,
        }
    }

    fn suite_request() -> RunRequest {
        let mut req = request();
        req.pairs.clear();
        req.nic_policies.clear();
        req.tcp_windows.clear();
        req.tcp_streams.clear();
        req.udp_bandwidths.clear();
        req.udp_lengths.clear();
        req.udp_windows.clear();
        req.udp_streams = 1;
        req.ui_plan = Some(UiPlan {
            ui_plan_version: 1,
            link_sets: vec![UiLinkSet {
                id: "set-a".into(),
                name: "A".into(),
                pair_refs: vec![UiPairRef {
                    id: "pair-a".into(),
                    src: "master:NAME=以太网 6".into(),
                    dst: "agent:NAME=WLAN 3".into(),
                }],
            }],
            recipes: UiRecipes {
                tcp: vec![UiRecipe {
                    id: "tcp-r".into(),
                    name: "TCP".into(),
                    profiles: vec![UiRecipeProfile {
                        window: Some("4m".into()),
                        streams: UiU32Values::One(10),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                udp: vec![UiRecipe {
                    id: "udp-r".into(),
                    name: "UDP".into(),
                    profiles: vec![UiRecipeProfile {
                        bandwidth: Some("100m".into()),
                        length: Some("1200".into()),
                        streams: UiU32Values::One(1),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ping: Vec::new(),
            },
            suites: vec![UiSuite {
                id: "suite-a".into(),
                name: "TCP UDP".into(),
                note: String::new(),
                execution: "sequential".into(),
                order: vec!["task-tcp".into(), "task-udp".into()],
                tasks: vec![
                    UiTask {
                        id: "task-tcp".into(),
                        name: "TCP".into(),
                        protocol: "tcp".into(),
                        directions: vec!["ab".into()],
                        ip: vec!["v4".into()],
                        recipe_ids: vec!["tcp-r".into()],
                        ..Default::default()
                    },
                    UiTask {
                        id: "task-udp".into(),
                        name: "UDP".into(),
                        protocol: "udp".into(),
                        directions: vec!["ba".into()],
                        ip: vec!["v4".into()],
                        recipe_ids: vec!["udp-r".into()],
                        ..Default::default()
                    },
                ],
            }],
            bindings: vec![UiBinding {
                id: "bind-a".into(),
                link_set_id: "set-a".into(),
                suite_id: "suite-a".into(),
                mode: "replace".into(),
                order: 1,
                pair_ids: Vec::new(),
            }],
            plan_hash: None,
        });
        req.plan_hash = None;
        req
    }

    #[test]
    fn suite_plan_keeps_tcp_and_udp_as_independent_specs_in_suite_order() {
        let state = state_with_pair();
        let req = suite_request();
        let cfg =
            validated_config_from_request(&state, &req).expect("suite request should validate");
        assert_eq!(
            cfg.tests.len(),
            2,
            "one TCP and one UDP spec, no protocol cross product"
        );
        assert_eq!(cfg.tests[0].transports, vec!["tcp"]);
        assert_eq!(cfg.tests[1].transports, vec!["udp"]);
        assert_eq!(cfg.tests[0].direction.directions(), vec!["ab"]);
        assert_eq!(cfg.tests[1].direction.directions(), vec!["ba"]);

        let compiled = compile_request(&state, &req).expect("compile suite plan");
        assert_eq!(
            compiled.units.len(),
            2,
            "TCP and UDP each produce one independent unit"
        );
        assert_eq!(compiled.trace.len(), compiled.units.len());
        assert_eq!(compiled.trace[0].protocol.as_deref(), Some("tcp"));
        assert_eq!(compiled.trace[1].protocol.as_deref(), Some("udp"));
        assert_eq!(compiled.trace[0].direction.as_deref(), Some("ab"));
        assert_eq!(compiled.trace[1].direction.as_deref(), Some("ba"));
        assert!(!compiled.plan_hash.is_empty());
        assert!(!compiled.topology_fingerprint.is_empty());
    }

    #[test]
    fn suite_trace_distinguishes_both_from_a_bidirectional_unit() {
        let state = state_with_pair();
        let mut req = suite_request();
        let plan = req.ui_plan.as_mut().unwrap();
        // `both` is the legacy spelling for two independent one-way legs.  It
        // must not be collapsed into the single concurrent `bidir` unit: the
        // trace is consumed by the review UI and needs to identify each leg.
        plan.suites[0].tasks.retain(|task| task.id == "task-tcp");
        plan.suites[0].order = vec!["task-tcp".into()];
        plan.suites[0].tasks[0].directions = vec!["both".into()];

        let compiled = compile_request(&state, &req).expect("both should compile");
        assert_eq!(compiled.units.len(), 2);
        assert!(compiled.units.iter().all(|unit| !unit.bidir));
        assert_eq!(
            compiled
                .trace
                .iter()
                .map(|trace| trace.direction.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("ab"), Some("ba")]
        );

        // The concurrent spelling is the opposite contract: one unit with
        // two tagged legs, represented by a single `bidir` trace direction.
        let mut req = suite_request();
        let plan = req.ui_plan.as_mut().unwrap();
        plan.suites[0].tasks.retain(|task| task.id == "task-tcp");
        plan.suites[0].order = vec!["task-tcp".into()];
        plan.suites[0].tasks[0].directions = vec!["bidir".into()];
        let compiled = compile_request(&state, &req).expect("bidir should compile");
        assert_eq!(compiled.units.len(), 1);
        assert!(compiled.units[0].bidir);
        assert_eq!(compiled.trace[0].direction.as_deref(), Some("bidir"));
    }

    #[test]
    fn suite_plan_rejects_legacy_pairs_and_parallel_execution() {
        let state = state_with_pair();
        let mut req = suite_request();
        req.pairs = request().pairs;
        let error = validate_request(&state, &req).expect_err("mixed request formats must fail");
        assert!(error.contains("不能同时"), "{error}");

        let mut req = suite_request();
        req.ui_plan.as_mut().unwrap().suites[0].execution = "parallel".into();
        let error = validate_request(&state, &req).expect_err("parallel suites are not supported");
        assert!(error.contains("sequential"), "{error}");
    }

    #[test]
    fn quick_plan_applies_request_level_ping_defaults() {
        let state = state_with_pair();
        let mut req = suite_request();
        req.ping_count = 5;
        req.ping_payload_sizes = vec![64, 1400];
        let plan = req.ui_plan.as_mut().unwrap();
        plan.recipes.ping.clear();
        plan.suites[0].order.push("task-ping".into());
        plan.suites[0].tasks.push(UiTask {
            id: "task-ping".into(),
            name: "Ping".into(),
            protocol: "ping".into(),
            directions: vec!["ab".into()],
            ip: vec!["v4".into()],
            ..Default::default()
        });

        let compiled = compile_request(&state, &req).expect("ping suite should validate");
        assert_eq!(compiled.cfg.ping.count, 5);
        assert_eq!(compiled.cfg.ping.payload_sizes, vec![64, 1400]);
        let ping = compiled
            .cfg
            .tests
            .iter()
            .find(|test| test.kinds.iter().any(|kind| kind == "ping"))
            .expect("ping task should compile");
        assert_eq!(
            ping.ping_count, None,
            "task should inherit request defaults"
        );
        assert_eq!(ping.ping_payload_sizes, None);
    }

    #[test]
    fn quick_plan_rejects_ping_recipe_references_until_recipe_fields_exist() {
        let state = state_with_pair();
        let mut req = suite_request();
        let plan = req.ui_plan.as_mut().expect("suite plan");
        plan.suites[0].tasks.retain(|task| task.id == "task-tcp");
        plan.suites[0].order = vec!["task-tcp".into()];
        plan.recipes.ping.push(UiRecipe {
            id: "ping-r".into(),
            name: "PING recipe".into(),
            ..Default::default()
        });
        plan.suites[0].tasks.push(UiTask {
            id: "task-ping".into(),
            name: "PING".into(),
            protocol: "ping".into(),
            directions: vec!["ab".into()],
            ip: vec!["v4".into()],
            recipe_ids: vec!["ping-r".into()],
            ..Default::default()
        });
        plan.suites[0].order.push("task-ping".into());

        let error = validate_request(&state, &req)
            .expect_err("PING recipe references must not be silently ignored");
        assert!(error.contains("暂不支持 PING 配方"), "{error}");
    }

    #[test]
    fn quick_plan_rejects_append_binding_mode_without_silent_replace() {
        let state = state_with_pair();
        let mut req = suite_request();
        req.ui_plan.as_mut().expect("suite plan").bindings[0].mode = "append".into();

        let error = validate_request(&state, &req)
            .expect_err("unsupported append mode must fail at validation");
        assert!(error.contains("append 尚未支持"), "{error}");
    }

    #[test]
    fn quick_plan_ignores_unbound_empty_link_set_but_rejects_bound_empty_set() {
        let state = state_with_pair();
        let mut req = suite_request();
        {
            let plan = req.ui_plan.as_mut().expect("suite plan");
            // The UI permits creating a draft collection before selecting pairs.
            // An unrelated empty collection must not prevent another valid binding
            // from being previewed.
            plan.link_sets.push(UiLinkSet {
                id: "empty-draft".into(),
                name: "待填写".into(),
                pair_refs: Vec::new(),
            });
        }
        let compiled = compile_request(&state, &req).expect("unbound draft is harmless");
        assert_eq!(compiled.cfg.tests.len(), 2);

        // Once a suite is assigned to that collection, silently producing no
        // units would be much worse than an actionable validation error.
        req.ui_plan.as_mut().expect("suite plan").bindings[0].link_set_id = "empty-draft".into();
        let error = validate_request(&state, &req).expect_err("bound empty set must fail");
        assert!(error.contains("没有可执行的 pair_ref"), "{error}");

        // A non-empty set with an explicit subset remains valid when the
        // selected reference exists; the effective-pair check must not confuse
        // `pair_ids` with an instruction to run the whole set.
        let mut req = suite_request();
        req.ui_plan.as_mut().expect("suite plan").bindings[0].pair_ids = vec!["pair-a".into()];
        assert!(
            validate_request(&state, &req).is_ok(),
            "an existing pair_ids subset should remain executable"
        );
    }

    #[test]
    fn quick_plan_rejects_empty_udp_recipe_that_would_emit_no_units() {
        let state = state_with_pair();
        let mut req = suite_request();
        let recipe = req.ui_plan.as_mut().unwrap().recipes.udp[0].clone();
        let empty = UiRecipe {
            id: recipe.id,
            name: recipe.name,
            mode: recipe.mode,
            profiles: vec![UiRecipeProfile::default()],
            ..Default::default()
        };
        req.ui_plan.as_mut().unwrap().recipes.udp[0] = empty;
        let error = validate_request(&state, &req).expect_err("empty UDP recipe must fail");
        assert!(error.contains("-b") || error.contains("有效"), "{error}");
    }

    #[test]
    fn quick_plan_validates_task_duration_and_profile_dimensions() {
        let state = state_with_pair();
        let mut req = suite_request();
        req.ui_plan.as_mut().unwrap().suites[0].tasks[0].duration = Some(0);
        let error = validate_request(&state, &req).expect_err("zero task duration must fail");
        assert!(error.contains("时长"), "{error}");

        let mut req = suite_request();
        req.ui_plan.as_mut().unwrap().recipes.udp[0].profiles[0].length = Some("65508".into());
        let error = validate_request(&state, &req).expect_err("oversized UDP profile must fail");
        assert!(error.contains("65507"), "{error}");

        let mut req = suite_request();
        req.ui_plan.as_mut().unwrap().recipes.udp[0].profiles[0].window = Some("not-size".into());
        let error =
            validate_request(&state, &req).expect_err("invalid UDP profile window must fail");
        assert!(error.contains("profile -w"), "{error}");
    }

    #[test]
    fn quick_plan_preserves_slashes_in_trace_ids() {
        let state = state_with_pair();
        let mut req = suite_request();
        let plan = req.ui_plan.as_mut().unwrap();
        plan.link_sets[0].id = "set/a".into();
        plan.link_sets[0].pair_refs[0].id = "pair/a".into();
        plan.recipes.tcp[0].id = "tcp/recipe".into();
        plan.recipes.udp[0].id = "udp/recipe".into();
        plan.suites[0].id = "suite/a".into();
        plan.suites[0].tasks[0].id = "task/tcp".into();
        plan.suites[0].tasks[1].id = "task/udp".into();
        plan.suites[0].order = vec!["task/tcp".into(), "task/udp".into()];
        plan.suites[0].tasks[0].recipe_ids = vec!["tcp/recipe".into()];
        plan.suites[0].tasks[1].recipe_ids = vec!["udp/recipe".into()];
        plan.bindings[0].id = "binding/a".into();
        plan.bindings[0].link_set_id = "set/a".into();
        plan.bindings[0].suite_id = "suite/a".into();

        let compiled = compile_request(&state, &req).expect("slash IDs should be valid");
        assert_eq!(compiled.trace[0].link_set_id.as_deref(), Some("set/a"));
        assert_eq!(compiled.trace[0].pair_id.as_deref(), Some("pair/a"));
        assert_eq!(compiled.trace[0].suite_id.as_deref(), Some("suite/a"));
        assert_eq!(compiled.trace[0].task_id.as_deref(), Some("task/tcp"));
        assert_eq!(compiled.trace[0].recipe_id.as_deref(), Some("tcp/recipe"));
        assert_eq!(compiled.trace[1].task_id.as_deref(), Some("task/udp"));
        assert_eq!(compiled.trace[1].recipe_id.as_deref(), Some("udp/recipe"));
    }

    #[test]
    fn quick_plan_rejects_duplicate_pair_ids_in_a_binding() {
        let state = state_with_pair();
        let mut req = suite_request();
        req.ui_plan.as_mut().unwrap().bindings[0].pair_ids = vec!["pair-a".into(), "pair-a".into()];
        let error = validate_request(&state, &req).expect_err("duplicate pair refs must fail");
        assert!(error.contains("重复引用"), "{error}");
    }

    #[test]
    fn quick_plan_allows_link_set_and_recipe_ids_to_share_a_namespace_name() {
        let state = state_with_pair();
        let mut req = suite_request();
        let plan = req.ui_plan.as_mut().unwrap();
        // IDs are scoped by the field that owns them.  A human-authored
        // project commonly calls both its first link set and its first recipe
        // "default"; that must not be mistaken for a duplicate reference.
        plan.link_sets[0].id = "default".into();
        plan.recipes.tcp[0].id = "default".into();
        plan.bindings[0].link_set_id = "default".into();
        plan.suites[0].tasks[0].recipe_ids = vec!["default".into()];

        let compiled = compile_request(&state, &req)
            .expect("link-set and recipe IDs may match across namespaces");
        assert_eq!(compiled.cfg.tests.len(), 2);
    }

    #[test]
    fn quick_plan_honors_stream_axes_on_legacy_udp_profiles() {
        let state = state_with_pair();
        let mut req = suite_request();
        let recipe = &mut req.ui_plan.as_mut().unwrap().recipes.udp[0];
        recipe.profiles.clear();
        recipe.bandwidths.clear();
        recipe.lengths.clear();
        recipe.windows.clear();
        recipe.udp_profiles = vec![UdpProfile::bw("100m")];
        recipe.udp_streams = vec![2, 3];
        let cfg = validated_config_from_request(&state, &req).expect("legacy UDP recipe valid");
        let streams: Vec<u32> = cfg
            .tests
            .iter()
            .filter(|test| test.transports.iter().any(|transport| transport == "udp"))
            .filter_map(|test| test.udp_streams)
            .collect();
        assert_eq!(streams, vec![2, 3]);
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

    fn console_with(state: UiState) -> Arc<Console> {
        Arc::new(Console {
            state: Mutex::new(state),
            running: AtomicBool::new(false),
            report: Mutex::new(String::new()),
            ui_token: String::new(),
            monitors: Mutex::new(HashMap::new()),
        })
    }

    fn console_for_monitor_tests() -> Arc<Console> {
        console_with(state_with_pair())
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
        let out =
            api_monitor_samples(&console, r#"{"cursors":[{"session":"s1","from":0}]}"#).unwrap();
        let first = &out["series"][0];
        assert_eq!(
            first["points"][0]["rx_mbps"], 120.0,
            "第一个点应是未被挤掉的最早点"
        );
        assert_eq!(first["from"], (MONITOR_MAX_POINTS + 120) as u64);
        assert_eq!(
            first["points"].as_array().unwrap().len(),
            MONITOR_MAX_POINTS,
            "落后游标应拿到缓冲里现有的全部"
        );

        // 追平之后再问，应该一个点都没有。
        let out = api_monitor_samples(
            &console,
            &format!(
                r#"{{"cursors":[{{"session":"s1","from":{}}}]}}"#,
                MONITOR_MAX_POINTS + 120
            ),
        )
        .unwrap();
        assert!(
            out["series"][0]["points"].as_array().unwrap().is_empty(),
            "追平后不该重发"
        );

        api_monitor_stop(&console, r#"{"session":"s1"}"#).unwrap();
        // 停掉的那一路只报自己那一条，不能让整次批量取样失败——同一次请求里
        // 还有别的曲线好好地在跑。
        let out =
            api_monitor_samples(&console, r#"{"cursors":[{"session":"s1","from":0}]}"#).unwrap();
        assert_eq!(out["series"][0]["running"], false);
        assert_eq!(out["series"][0]["error"], "监控会话已结束");
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
            let out = api_monitor_samples(
                &console,
                &format!(r#"{{"cursors":[{{"session":"{session}","from":0}}]}}"#),
            )
            .unwrap();
            let series = &out["series"][0];
            error = series["error"].as_str().unwrap_or_default().to_string();
            if !error.is_empty() && series["running"] == false {
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
        // 预算必须大于**被测端点自己允许的耗时**，否则这条测试迟早会因为
        // 环境慢而不是闸门坏而红。`/api/local` 带对口令那一发会真的去扫本机：
        // Windows 上 `ipconfig /all` 允许 20s、每块 Wi-Fi 卡的 `netsh` 10s、
        // `iperf3 --version` 8s，加起来远超原来写的 5s——之前一直绿只是因为
        // CI 机器上没有 iperf3、扫描又快，5s 侥幸够用。Windows runner 上并行
        // 跑测试把扫描拖过 5s 时，它就报成「读头失败 (os error 10060)」，
        // 看起来像闸门坏了。这里给的是超时上限，正常路径仍然是毫秒级返回。
        let wait = Duration::from_secs(60);

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

    fn state_with_two_pairs() -> UiState {
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
                interfaces: vec![
                    nic("以太网 6", "SGMII2.5G", "192.168.0.101"),
                    nic("以太网 7", "SGMII1G", "192.168.0.102"),
                ],
            },
            agent: HostInfo {
                hostname: "a".into(),
                os: "test".into(),
                interfaces: vec![
                    nic("WLAN 3", "WIFI5G", "192.168.0.104"),
                    nic("USB 4", "RNDIS", "192.168.0.105"),
                ],
            },
        }
    }

    /// `-b` 在预览里按 Mbps 显示，别的参数照抄。
    ///
    /// 下发的是精确 bit/s 整数，原样打印是 `-b 1000000000`——十个零要一个个数，
    /// 而且抄回输入框会变成 10^9 Mbps（那里的裸数字按 Mbps 算）。
    #[test]
    fn the_plan_renders_bandwidth_in_mbps() {
        let args =
            |items: &[&str]| -> Vec<String> { items.iter().map(|item| item.to_string()).collect() };
        assert_eq!(
            readable_args(&args(&["-b", "1000000000", "-l", "1200"])),
            "-b 1000 Mbps -l 1200"
        );
        // 裁剪之后常常不是整 Mbps，别把它抹成整数。
        assert_eq!(
            readable_args(&args(&["-b", "2600500000"])),
            "-b 2600.5 Mbps"
        );
        // TCP 那条没有 -b，原样照抄。
        assert_eq!(
            readable_args(&args(&["-w", "4m", "-P", "10"])),
            "-w 4m -P 10"
        );
    }

    /// 把 bit/s 填进按 Mbps 解释的输入框，要当场说清楚，而不是拿着
    /// 10^9 Mbps 去灌包。
    #[test]
    fn a_bandwidth_that_is_off_by_a_million_is_rejected() {
        let state = state_with_pair();
        let mut req = request();
        req.udp_bandwidths = vec!["1000000000".into()];
        let error = validate_request(&state, &req).expect_err("必须报错");
        assert!(
            error.contains("按 Mbps 算"),
            "错误要说清是单位填错了：{error}"
        );

        // 加了后缀就是正常的 1000Mbps，必须放行。
        for ok in ["1000m", "1G", "1000mbps"] {
            let mut req = request();
            req.udp_bandwidths = vec![ok.into()];
            assert!(validate_request(&state, &req).is_ok(), "{ok} 应当合法");
        }
    }

    /// 「预览任务」要把每条腿最终下发的参数摆出来。
    ///
    /// 优先级（网口固定值 > 参数组 > 默认组）和链路裁剪都会改写这几个数字，
    /// 而这两件事都发生在人看不见的地方。摆出来之后，填错了在跑之前就能发现。
    #[test]
    fn the_plan_shows_the_parameters_each_leg_will_actually_use() {
        let state = state_with_pair();
        let mut req = request();
        req.nic_policies.clear();
        req.udp_bandwidths = vec!["500m".into()];
        req.udp_lengths = vec!["1200".into()];
        req.udp_streams = 3;
        req.pairs[0].directions = vec!["ab".into()];
        req.pairs[0].transports = vec!["udp".into()];
        let cfg = validated_config_from_request(&state, &req).unwrap();

        let specs: Vec<_> = cfg
            .tests
            .iter()
            .map(|test| builder::spec_from_config(test, &cfg, &state.master, &state.agent).unwrap())
            .collect();
        let mut port = builder::PORT_BASE;
        let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
        let lines = unit_load_lines(&units[0]);

        assert_eq!(lines.len(), 1, "单向单元只有一条腿");
        assert!(lines[0].contains("-b 500 Mbps"), "{lines:?}");
        assert!(lines[0].contains("-l 1200"), "{lines:?}");
        assert!(lines[0].contains("×3 流"), "{lines:?}");
    }

    /// 每一行选哪一组 UDP 参数，就跑那一组里写着的东西。
    ///
    /// 「这几对 2500m 单流、那几对 1000m/500m 四流、还有几对带 -l」是一轮里
    /// 最常见的安排，而执行区那份档位是所有勾中的配对共用的，表达不了。
    #[test]
    fn each_row_runs_the_udp_group_it_points_at() {
        let state = state_with_two_pairs();
        let mut req = request();
        req.nic_policies.clear();
        // 默认组：-b 1m、不带 -l、2 流。
        req.udp_bandwidths = vec!["1m".into()];
        req.udp_lengths = Vec::new();
        req.udp_streams = 2;
        req.udp_groups = vec![
            UdpGroup {
                name: "单流打满".into(),
                bandwidths: vec!["2500m".into()],
                lengths: vec!["64".into()],
                windows: Vec::new(),
                streams: 1,
            },
            UdpGroup {
                name: "多流".into(),
                bandwidths: vec!["1000m".into(), "500m".into()],
                lengths: Vec::new(),
                windows: Vec::new(),
                streams: 4,
            },
        ];
        req.pairs[0].directions = vec!["ab".into()];
        req.pairs[0].udp_groups = vec![1];
        let mut second = req.pairs[0].clone();
        second.src = "master:NAME=以太网 7".into();
        second.dst = "agent:NAME=USB 4".into();
        second.udp_groups = vec![2];
        let mut third = req.pairs[0].clone();
        third.src = "master:NAME=以太网 6".into();
        third.dst = "agent:NAME=USB 4".into();
        third.udp_groups = vec![0];
        req.pairs.push(second);
        req.pairs.push(third);

        let cfg = validated_config_from_request(&state, &req).expect("三行都该合法");
        // 单元名带着组号：同一对选两组时两批单元必须区分得开，否则 resume id
        // 撞车、互相覆盖。默认组沿用原名，改名会让历史 PASS 全部失效。
        let spec = |name: &str| {
            cfg.tests
                .iter()
                .find(|test| test.name == name)
                .unwrap_or_else(|| panic!("找不到单元 {name}"))
        };
        let profiles = |name: &str| -> Vec<(String, Option<String>)> {
            spec(name)
                .udp_profiles
                .as_ref()
                .unwrap()
                .iter()
                .map(|profile| (profile.bandwidth.clone(), profile.length.clone()))
                .collect()
        };

        assert_eq!(
            profiles("ui-1-udp-g2"),
            vec![("2500m".into(), Some("64".into()))]
        );
        assert_eq!(spec("ui-1-udp-g2").udp_streams, Some(1));
        assert_eq!(
            profiles("ui-2-udp-g3"),
            vec![("1000m".into(), None), ("500m".into(), None)],
            "组里没填 -l 就是不下发，不继承默认组"
        );
        assert_eq!(spec("ui-2-udp-g3").udp_streams, Some(4));
        assert_eq!(
            profiles("ui-3-udp"),
            vec![("1m".into(), None)],
            "没选组的行跑默认组，单元名不带后缀"
        );
        assert_eq!(spec("ui-3-udp").udp_streams, Some(2));
        // 默认组的档位仍然写回 iperf.udp_profiles：下载出来的 config 交给
        // master --auto 时读的是这一份。
        assert_eq!(
            cfg.iperf
                .udp_profiles
                .iter()
                .map(|profile| profile.bandwidth.clone())
                .collect::<Vec<_>>(),
            vec!["1m"]
        );
    }

    /// 同一行选两组 = 这一对跑两批，参数各按各的组来。
    ///
    /// 矩阵里一对网口只有一行，不能多选的话「既按常规档位跑一遍、又用 1m 单流
    /// 跑一遍」只能分两轮、出两份报告。
    #[test]
    fn one_row_can_run_several_groups() {
        let state = state_with_pair();
        let mut req = request();
        req.nic_policies.clear();
        req.udp_bandwidths = vec!["1000m".into()];
        req.udp_streams = 4;
        req.udp_groups = vec![UdpGroup {
            name: "慢速单流".into(),
            bandwidths: vec!["1m".into()],
            lengths: Vec::new(),
            windows: Vec::new(),
            streams: 1,
        }];
        req.pairs[0].directions = vec!["ab".into()];
        req.pairs[0].transports = vec!["udp".into()];
        req.pairs[0].udp_groups = vec![0, 1];

        let cfg = validated_config_from_request(&state, &req).unwrap();
        let udp: Vec<&TestSpec> = cfg
            .tests
            .iter()
            .filter(|test| test.transports.iter().any(|t| t == "udp"))
            .collect();
        assert_eq!(udp.len(), 2, "两组 = 两批单元");
        assert_eq!(udp[0].name, "ui-1-udp");
        assert_eq!(udp[0].udp_streams, Some(4));
        assert_eq!(
            udp[1].name, "ui-1-udp-g2",
            "组号进单元名，resume id 才不撞车"
        );
        assert_eq!(udp[1].udp_streams, Some(1));

        // 同一组选两次不该跑两遍：两批同名单元在 resume 里会互相覆盖。
        req.pairs[0].udp_groups = vec![1, 1, 0];
        let cfg = validated_config_from_request(&state, &req).unwrap();
        assert_eq!(
            cfg.tests
                .iter()
                .filter(|test| test.transports.iter().any(|t| t == "udp"))
                .count(),
            2
        );

        // 不带这个字段（老页面、手写请求）= 只跑默认组。
        req.pairs[0].udp_groups = Vec::new();
        let cfg = validated_config_from_request(&state, &req).unwrap();
        let udp: Vec<&TestSpec> = cfg
            .tests
            .iter()
            .filter(|test| test.transports.iter().any(|t| t == "udp"))
            .collect();
        assert_eq!(udp.len(), 1);
        assert_eq!(udp[0].name, "ui-1-udp");
    }

    /// 组是完整定义，不继承默认组——空的 `-b` 生成不出任何单元，要当场挡住。
    /// 选了组却没勾 UDP 同理：那一组一个单元都不会跑。
    #[test]
    fn a_group_that_would_run_nothing_is_rejected() {
        let state = state_with_pair();

        let mut req = request();
        req.udp_groups = vec![UdpGroup {
            name: String::new(),
            bandwidths: Vec::new(),
            lengths: vec!["64".into()],
            windows: Vec::new(),
            streams: 1,
        }];
        let error = validate_request(&state, &req).expect_err("没填 -b 必须报错");
        assert!(error.contains("没填 -b"), "{error}");

        let mut req = request();
        req.udp_groups = vec![UdpGroup {
            name: String::new(),
            bandwidths: vec!["2500m".into()],
            lengths: Vec::new(),
            windows: Vec::new(),
            streams: 1,
        }];
        req.pairs[0].udp_groups = vec![1];
        req.pairs[0].transports = vec!["tcp".into()];
        let error = validate_request(&state, &req).expect_err("没勾 UDP 必须报错");
        assert!(error.contains("没有勾 UDP"), "{error}");

        // 指向一个不存在的组：页面删组时没同步过来才会出现，静默按默认组跑
        // 等于跑了另一件事。
        let mut req = request();
        req.pairs[0].udp_groups = vec![3];
        let error = validate_request(&state, &req).expect_err("越界必须报错");
        assert!(error.contains("不存在"), "{error}");

        // 组里的档位写错要指名是哪一组。
        let mut req = request();
        req.udp_groups = vec![UdpGroup {
            name: "很快组".into(),
            bandwidths: vec!["很快".into()],
            lengths: Vec::new(),
            windows: Vec::new(),
            streams: 1,
        }];
        let error = validate_request(&state, &req).expect_err("必须报错");
        assert!(error.contains("很快组"), "{error}");
    }

    /// 清空 `-l` / `-w` 就是真的不下发它们，而不是替人填一个 iperf3 默认值。
    ///
    /// 「不指定」和「指定成某个具体值」在报告里读起来完全不同，不能混。
    #[test]
    fn clearing_udp_length_and_window_emits_no_such_flags() {
        let state = state_with_pair();
        let mut req = request();
        req.udp_lengths = Vec::new();
        req.udp_windows = Vec::new();
        req.udp_bandwidths = vec!["1000m".into()];
        req.nic_policies.clear();

        let cfg = validated_config_from_request(&state, &req).unwrap();
        assert!(
            cfg.iperf
                .udp_profiles
                .iter()
                .all(|profile| profile.length.is_none() && profile.window.is_none()),
            "全局档位不该凭空长出 -l / -w"
        );
        let pair_profiles = cfg
            .tests
            .iter()
            .filter_map(|test| test.udp_profiles.as_ref())
            .flatten();
        for profile in pair_profiles {
            assert!(
                profile.length.is_none() && profile.window.is_none(),
                "逐对档位也不该凭空长出 -l / -w：{profile:?}"
            );
        }
    }

    /// 「下载 config.json」再导入回来，界面上的勾选必须原样回到原处。
    ///
    /// 导入是下载的逆运算，这条测试是它唯一的判据：两边任何一处口径不一样，
    /// 表现都是「导进来看着差不多、跑出来不是那份配置」——比报错难查得多。
    #[test]
    fn downloading_then_importing_restores_the_same_selection() {
        let state = state_with_pair();
        let req = request();
        let cfg = config_from_request(&state, &req);
        let file = serde_json::to_string(&cfg).unwrap();

        let console = console_with(state_with_pair());
        let out = api_import(&console, &file).expect("自己下载的配置必须能导回来");

        let pair = &out["pairs"][0];
        assert_eq!(pair["src"], "master:NAME=以太网 6");
        assert_eq!(pair["dst"], "agent:NAME=WLAN 3");
        assert_eq!(
            pair["directions"].as_array().unwrap(),
            &vec![json!("ab"), json!("bidir")],
            "方向要按原样回来，不能被 TCP/UDP 那几条 TestSpec 拆散"
        );
        assert_eq!(
            pair["transports"].as_array().unwrap(),
            &vec![json!("tcp"), json!("udp")]
        );
        assert_eq!(pair["ip"].as_array().unwrap(), &vec![json!("v4")]);
        // 网口上钉了 -b 的那种配置，文件里的 profile 带宽是占位值，不能被
        // 当成「这一行自己选的组」读回来。
        assert_eq!(
            pair["udp_groups"].as_array().unwrap(),
            &vec![json!(0)],
            "网口钉死时不认成附加组"
        );
        assert!(
            out["udp_groups"].as_array().unwrap().is_empty(),
            "不该凭空多出一个由占位值拼成的组"
        );

        let settings = &out["settings"];
        assert_eq!(settings["duration"], 60);
        assert_eq!(
            settings["tcp_windows"].as_array().unwrap(),
            &vec![json!("2m"), json!("4m"), json!("256m")]
        );
        assert_eq!(
            settings["tcp_streams"].as_array().unwrap(),
            &vec![json!(1), json!(5), json!(10)],
            "每个 -P 档位是一条 TestSpec，回填时要合回一个列表"
        );
        assert_eq!(
            settings["udp_bandwidths"].as_array().unwrap(),
            &vec![json!("1m"), json!("500m"), json!("1G")]
        );

        // 网口策略是另一半：门限和按口 -b 都在 link_profiles 里，漏了它
        // 「导入成功」就是一句空话。
        let policies = out["nic_policies"].as_array().unwrap();
        let master = policies
            .iter()
            .find(|policy| policy["endpoint"] == "master:NAME=以太网 6")
            .expect("主控网口策略");
        assert_eq!(master["rx_target"], "1800");
        assert_eq!(master["udp_bandwidth"], "2.6G");
    }

    /// 文件里不同的 UDP 参数要被认成组，并按行选回去。
    #[test]
    fn importing_rebuilds_the_udp_groups_from_the_tests() {
        let state = state_with_two_pairs();
        let mut req = request();
        req.nic_policies.clear();
        req.udp_bandwidths = vec!["1m".into()];
        req.udp_streams = 2;
        req.udp_groups = vec![UdpGroup {
            name: "多流".into(),
            bandwidths: vec!["1000m".into(), "500m".into()],
            lengths: Vec::new(),
            windows: Vec::new(),
            streams: 4,
        }];
        req.pairs[0].udp_groups = vec![1];
        let mut second = req.pairs[0].clone();
        second.src = "master:NAME=以太网 7".into();
        second.dst = "agent:NAME=USB 4".into();
        second.udp_groups = vec![0];
        req.pairs.push(second);
        let cfg = config_from_request(&state, &req);

        let console = console_with(state_with_two_pairs());
        let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
        let groups = out["udp_groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "只有一种和默认组不同的打法");
        assert_eq!(
            groups[0]["bandwidths"].as_array().unwrap(),
            &vec![json!("1000m"), json!("500m")]
        );
        assert_eq!(groups[0]["streams"], 4);
        assert_eq!(
            out["pairs"][0]["udp_groups"].as_array().unwrap(),
            &vec![json!(1)],
            "第一行选那一组"
        );
        assert_eq!(
            out["pairs"][1]["udp_groups"].as_array().unwrap(),
            &vec![json!(0)],
            "第二行留在默认组"
        );
        // 默认组还是执行区那份，不该被某一行的组顶掉。
        assert_eq!(
            out["settings"]["udp_bandwidths"].as_array().unwrap(),
            &vec![json!("1m")]
        );
        assert_eq!(out["settings"]["udp_streams"], 2);
    }

    /// 当一端按网口固定 `-b`、另一端仍扫 UDP 档位时，导入不能把未固定方向
    /// 的附加组误判成占位值而丢掉。矩阵编译会把这种组合拆成两个 test：
    /// pinned 的一条只用于固定方向，swept 的一条仍带用户选择的 profiles。
    #[test]
    fn importing_keeps_udp_group_for_unpinned_direction() {
        let state = state_with_pair();
        let mut req = request();
        req.nic_policies = vec![NicPolicySelection {
            endpoint: "master:NAME=以太网 6".into(),
            rx_target: String::new(),
            udp_bandwidth: "3m".into(),
            udp_length: String::new(),
        }];
        req.udp_bandwidths = vec!["1m".into()];
        req.udp_lengths = vec!["1200".into()];
        req.udp_streams = 1;
        req.udp_groups = vec![UdpGroup {
            name: "高带宽".into(),
            bandwidths: vec!["500m".into()],
            lengths: vec!["1200".into()],
            windows: Vec::new(),
            streams: 1,
        }];
        req.pairs[0].transports = vec!["udp".into()];
        req.pairs[0].directions = vec!["ab".into(), "ba".into()];
        req.pairs[0].udp_groups = vec![1];

        let cfg = validated_config_from_request(&state, &req).expect("原始配置必须合法");
        assert!(cfg.tests.iter().any(|test| {
            test.direction.directions() == ["ab"]
                && test
                    .udp_profiles
                    .as_ref()
                    .is_some_and(|profiles| profiles[0].bandwidth == "3m")
        }));
        assert!(cfg.tests.iter().any(|test| {
            test.direction.directions() == ["ba"]
                && test
                    .udp_profiles
                    .as_ref()
                    .is_some_and(|profiles| profiles[0].bandwidth == "500m")
        }));

        let console = console_with(state_with_pair());
        let out = api_import(&console, &serde_json::to_string(&cfg).unwrap())
            .expect("混合固定/扫描方向必须能导入");
        assert_eq!(
            out["udp_groups"][0]["bandwidths"].as_array().unwrap(),
            &vec![json!("500m")],
            "未固定的 B→A 方向仍应恢复附加 UDP 组"
        );
        assert_eq!(out["pairs"][0]["udp_groups"], json!([1]));
    }

    /// 下载 -> 导入 -> 再下载，两份配置**跑出来的单元必须一模一样**。
    ///
    /// 这是导入功能真正要保证的东西，比逐个字段对更硬：任何一处回填走样，
    /// 单元列表就变了。而「走样」在实际使用里不报错——它安静地按另一份配置
    /// 跑完一整轮。
    ///
    /// 比单元而不是比 config 的字节，是因为同一件事在 config 里可以有两种写法：
    /// 界面没填 ping 次数时写的是 `null`（执行时回落到 `ping.count`），
    /// 回填之后那一格会是回落出来的 100，两份 JSON 因此不同、跑的却是同一件事。
    /// 单元列表是这两种写法的公共下游，也正是「跑什么」的定义。
    #[test]
    fn download_import_download_runs_the_same_units() {
        let state = state_with_two_pairs();
        let mut req = request();
        req.nic_policies.clear();
        req.udp_bandwidths = vec!["1m".into()];
        req.udp_lengths = vec!["1200".into()];
        req.udp_streams = 2;
        req.udp_groups = vec![UdpGroup {
            name: "单流".into(),
            bandwidths: vec!["2500m".into()],
            lengths: vec!["1200".into()],
            windows: Vec::new(),
            streams: 1,
        }];
        req.pairs[0].udp_groups = vec![1];
        req.pairs[0].rx_target_bidir_ab = "1000".into();
        let mut second = req.pairs[0].clone();
        second.src = "master:NAME=以太网 7".into();
        second.dst = "agent:NAME=USB 4".into();
        req.udp_groups.push(UdpGroup {
            name: "多流".into(),
            bandwidths: vec!["1000m".into(), "500m".into()],
            lengths: vec!["1200".into()],
            windows: Vec::new(),
            streams: 4,
        });
        second.udp_groups = vec![2];
        second.rx_target_bidir_ab = String::new();
        second.transports = vec!["udp".into(), "ping".into()];
        req.pairs.push(second);

        let first = validated_config_from_request(&state, &req).expect("原始配置必须合法");
        let file = serde_json::to_string(&first).unwrap();

        let console = console_with(state_with_two_pairs());
        let out = api_import(&console, &file).expect("必须能导回来");
        let replayed = request_from_import(&out);
        let second_pass = {
            let state = lock_recover(&console.state);
            validated_config_from_request(&state, &replayed).expect("回填出来的必须仍然合法")
        };

        assert_eq!(
            units_debug(&first, &state),
            units_debug(&second_pass, &state),
            "导入一轮之后跑的必须还是同一批单元"
        );
        // 顺带钉住这一轮里真正在意的那几个值，免得两边一起错还对得上。
        let dump = units_debug(&first, &state);
        assert!(dump.contains("2500m"), "第一行的逐对档位");
        assert!(
            dump.contains("1000m") && dump.contains("500m"),
            "第二行的两档"
        );
    }

    /// TCP 参数组和 UDP 一样要能下载再导回、跑出同一批单元；顺带盖住「附加组把
    /// `-w` 留空 = 跑一条不带 `-w` 的 TCP」这条新路径，和「一行选多组 TCP」。
    #[test]
    fn tcp_groups_download_import_runs_the_same_units() {
        let state = state_with_two_pairs();
        let mut req = request();
        req.nic_policies.clear();
        // 默认 TCP 组：-w 两档 × -P 两档。
        req.tcp_windows = vec!["4m".into(), "256m".into()];
        req.tcp_streams = vec![1, 10];
        // 组1：单独的 -w、单流。组2：-w 留空（不下发 -w）、-P 扫两档——走 builder
        // 的 no-window 分支。
        req.tcp_groups = vec![
            TcpGroup {
                name: "大窗".into(),
                windows: vec!["512m".into()],
                streams: vec![1],
            },
            TcpGroup {
                name: "裸窗".into(),
                windows: Vec::new(),
                streams: vec![1, 4],
            },
        ];
        // 第一行只跑 TCP，选默认组 + 组1；第二行只跑 TCP，选组2（裸窗）。
        req.pairs[0].transports = vec!["tcp".into()];
        req.pairs[0].directions = vec!["ab".into()];
        req.pairs[0].rx_target_bidir_ab = String::new();
        req.pairs[0].udp_groups = Vec::new();
        req.pairs[0].tcp_groups = vec![0, 1];
        let mut second = req.pairs[0].clone();
        second.src = "master:NAME=以太网 7".into();
        second.dst = "agent:NAME=USB 4".into();
        second.tcp_groups = vec![2];
        req.pairs.push(second);

        let first = validated_config_from_request(&state, &req).expect("原始配置必须合法");
        let file = serde_json::to_string(&first).unwrap();

        let console = console_with(state_with_two_pairs());
        let out = api_import(&console, &file).expect("必须能导回来");
        let replayed = request_from_import(&out);
        let second_pass = {
            let state = lock_recover(&console.state);
            validated_config_from_request(&state, &replayed).expect("回填出来的必须仍然合法")
        };

        assert_eq!(
            units_debug(&first, &state),
            units_debug(&second_pass, &state),
            "TCP 组导入一轮之后跑的必须还是同一批单元"
        );
        let dump = units_debug(&first, &state);
        assert!(dump.contains("512m"), "组1 的 -w 档位应出现在单元里");
        // 裸窗组：应有不带 -w 的 TCP 单元（标签是 `TCP -P n` 而不是 `TCP -w .. -P n`）。
        assert!(
            dump.contains("TCP -P 4"),
            "裸窗组应生成一条不带 -w 的 TCP 单元"
        );
    }

    /// 一份 config 会生成哪些单元。Debug 里带着方向、协议、档位、流数和端口，
    /// 「跑什么」的每一个可见维度都在。
    fn units_debug(cfg: &Config, state: &UiState) -> String {
        let specs: Vec<_> = cfg
            .tests
            .iter()
            .map(|test| {
                builder::spec_from_config(test, cfg, &state.master, &state.agent)
                    .unwrap_or_else(|error| panic!("{} 生成任务失败：{error}", test.name))
            })
            .collect();
        let mut port = builder::PORT_BASE;
        let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
        assert!(!units.is_empty(), "这份配置一个单元都没生成");
        format!("{units:#?}")
    }

    /// 把 `/api/import` 的回包重新组装成一次「开始测试」的请求，
    /// 也就是页面拿到它之后会做的事。
    fn request_from_import(out: &serde_json::Value) -> RunRequest {
        let settings = &out["settings"];
        let list = |value: &serde_json::Value| -> Vec<String> {
            value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let numbers = |value: &serde_json::Value| -> Vec<u32> {
            value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_u64())
                        .map(|v| v as u32)
                        .collect()
                })
                .unwrap_or_default()
        };
        let pairs = out["pairs"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|pair| PairSelection {
                src: pair["src"].as_str().unwrap_or_default().to_string(),
                dst: pair["dst"].as_str().unwrap_or_default().to_string(),
                directions: list(&pair["directions"]),
                rx_target_bidir_ab: pair["rx_target_bidir_ab"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                rx_target_bidir_ba: pair["rx_target_bidir_ba"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                udp_groups: pair["udp_groups"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_u64())
                            .map(|value| value as usize)
                            .collect()
                    })
                    .unwrap_or_default(),
                tcp_groups: pair["tcp_groups"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_u64())
                            .map(|value| value as usize)
                            .collect()
                    })
                    .unwrap_or_default(),
                transports: list(&pair["transports"]),
                ip: list(&pair["ip"]),
            })
            .collect();
        RunRequest {
            pairs,
            nic_policies: serde_json::from_value(out["nic_policies"].clone()).unwrap_or_default(),
            duration: settings["duration"].as_u64().unwrap_or(180),
            tcp_windows: list(&settings["tcp_windows"]),
            tcp_streams: numbers(&settings["tcp_streams"]),
            udp_bandwidths: list(&settings["udp_bandwidths"]),
            udp_lengths: list(&settings["udp_lengths"]),
            udp_windows: list(&settings["udp_windows"]),
            udp_streams: settings["udp_streams"].as_u64().unwrap_or(1) as u32,
            udp_groups: out["udp_groups"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(|group| UdpGroup {
                    name: group["name"].as_str().unwrap_or_default().to_string(),
                    bandwidths: list(&group["bandwidths"]),
                    lengths: list(&group["lengths"]),
                    windows: list(&group["windows"]),
                    streams: group["streams"].as_u64().unwrap_or(1) as u32,
                })
                .collect(),
            tcp_groups: out["tcp_groups"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(|group| TcpGroup {
                    name: group["name"].as_str().unwrap_or_default().to_string(),
                    windows: list(&group["windows"]),
                    streams: numbers(&group["streams"]),
                })
                .collect(),
            ping_count: settings["ping_count"].as_u64().unwrap_or(0) as u32,
            ping_payload_sizes: numbers(&settings["ping_payload_sizes"]),
            limit_udp_by_link_speed: out["limit_udp_by_link_speed"].as_bool().unwrap_or(false),
            resume: out["resume"].as_bool().unwrap_or(false),
            screenshot: settings["screenshot"].as_bool().unwrap_or(false),
            ui_plan: None,
            plan_hash: None,
        }
    }

    /// 一档 `-b` 会因为每个 `-l` 档位各生成一份 profile；回填时不去重的话，
    /// 「下载 → 导入」每走一轮档位就翻一倍。
    #[test]
    fn importing_does_not_multiply_the_udp_bandwidth_steps() {
        let state = state_with_pair();
        let mut req = request();
        req.udp_lengths = vec!["1200".into(), "1400".into()];
        let cfg = config_from_request(&state, &req);
        assert_eq!(cfg.iperf.udp_profiles.len(), 6, "3 档 -b × 2 档 -l");

        let console = console_with(state_with_pair());
        let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            out["settings"]["udp_bandwidths"].as_array().unwrap(),
            &vec![json!("1m"), json!("500m"), json!("1G")]
        );
        assert_eq!(
            out["settings"]["udp_lengths"].as_array().unwrap(),
            &vec![json!("1200"), json!("1400")]
        );
    }

    /// ping 的次数和包长只落在 tests[] 上，回填要从那里读。
    ///
    /// 只读 cfg.ping 的话，一份「50 次 × 64 字节」的配置会回填成默认的
    /// 「100 次 × 三档包长」：单元数变三倍，而框里的数字看着像是文件里的。
    #[test]
    fn importing_reads_the_ping_settings_off_the_tests() {
        let state = state_with_pair();
        let mut req = request();
        req.pairs[0].transports = vec!["ping".into()];
        req.ping_count = 50;
        req.ping_payload_sizes = vec![64];
        let cfg = config_from_request(&state, &req);

        let console = console_with(state_with_pair());
        let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(out["settings"]["ping_count"], 50);
        assert_eq!(
            out["settings"]["ping_payload_sizes"].as_array().unwrap(),
            &vec![json!(64)]
        );
        assert_eq!(
            out["pairs"][0]["transports"].as_array().unwrap(),
            &vec![json!("ping")],
            "ping 在配置里挂 kinds、在界面上挂协议列，回填要走相反那一步"
        );
    }

    /// 文件把一对网口写反了（`src`/`dst` 调过来），要合进同一行并把方向对调。
    ///
    /// 矩阵一行代表的是**一对**网口。同一对口在文件里正着写一条、反着写一条是
    /// 完全合法的；不合并的话它会占两行，而界面只画得出一行——另一行的勾选
    /// 就此消失，人看不出少了什么。
    #[test]
    fn importing_folds_a_reversed_pair_into_one_row() {
        let state = state_with_pair();
        let mut cfg = config_from_request(&state, &request());
        // 只把 UDP 那条掉个头，TCP 三条保持原样：合并要发生在两种写法之间。
        let udp = cfg
            .tests
            .iter_mut()
            .find(|test| test.transports.iter().any(|t| t == "udp"))
            .expect("UDP 那条");
        std::mem::swap(&mut udp.src, &mut udp.dst);
        udp.direction = OneOrMany::Many(vec!["A->B".into()]);
        udp.rate_targets_bidir_mbps = Some(crate::config::RateTargets {
            forward: None,
            ab: Some(900.0),
            ba: None,
        });

        let console = console_with(state_with_pair());
        let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            out["pairs"].as_array().unwrap().len(),
            1,
            "同一对口只占一行"
        );
        let pair = &out["pairs"][0];
        assert_eq!(
            pair["src"], "master:NAME=以太网 6",
            "行的朝向按先出现的那条"
        );
        assert_eq!(
            pair["directions"].as_array().unwrap(),
            &vec![json!("ab"), json!("bidir"), json!("ba")],
            "反着写的那条里的 A→B，在这一行是 B→A"
        );
        assert_eq!(pair["rx_target_bidir_ba"], "900", "双向门限跟着方向一起翻");
        assert_eq!(pair["rx_target_bidir_ab"], "");
    }

    /// 还没连上辅测机也要能导入：全局参数当场生效，配对留给页面在连上之后按
    /// 端点名匹配。按角色写的端点（`master:SGMII2.5G`）这时解析不了，得点名。
    #[test]
    fn importing_before_connecting_keeps_the_named_pairs() {
        let mut cfg = config_from_request(&state_with_pair(), &request());
        cfg.tests.push(TestSpec {
            name: "by-role".into(),
            src: "master:SGMII2.5G".into(),
            dst: "agent:WIFI5G".into(),
            ..cfg.tests[0].clone()
        });

        let console = console_with(UiState {
            cfg: Config::default(),
            agent_host: String::new(),
            master: HostInfo::default(),
            agent: HostInfo::default(),
        });
        let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            out["pairs"].as_array().unwrap().len(),
            1,
            "NAME= 写法不需要实扫就能认"
        );
        let notices = out["notices"].as_array().unwrap();
        assert!(
            notices
                .iter()
                .any(|n| n.as_str().unwrap().contains("SGMII2.5G")),
            "认不出来的端点必须点名，不能默默少一行：{notices:?}"
        );
    }

    /// 文件里没有 agent_token 时不能把已经加载的令牌冲掉。
    ///
    /// 手写的 config 多半不带令牌；用空串覆盖的表现是导入之后点「连接」突然
    /// 401，而人刚做的事看起来和连接毫无关系。
    #[test]
    fn importing_a_file_without_a_token_keeps_the_loaded_one() {
        let mut state = state_with_pair();
        state.cfg.agent_token = "loaded-secret".into();
        let console = console_with(state);

        let cfg = config_from_request(&state_with_pair(), &request());
        let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(out["settings"]["token_configured"], true);
        assert_eq!(
            lock_recover(&console.state).cfg.agent_token,
            "loaded-secret"
        );
        assert!(
            out["notices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("agent_token")),
            "沿用旧令牌要说一声"
        );

        // 文件里带着令牌时以文件为准：那才是这份配置连得上的那台。
        let mut cfg = cfg;
        cfg.agent_token = "from-file".into();
        api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(lock_recover(&console.state).cfg.agent_token, "from-file");
    }

    /// 导入的是**配置**，不是「一份差不多的 JSON」。看不懂要当场说清。
    #[test]
    fn importing_rubbish_says_so_instead_of_half_applying_it() {
        let console = console_with(state_with_pair());
        let error = api_import(&console, "{ 这不是 json }").expect_err("必须报错");
        assert!(error.contains("config.json"), "{error}");

        let mut cfg = config_from_request(&state_with_pair(), &request());
        cfg.iperf.duration = 0;
        let error = api_import(&console, &serde_json::to_string(&cfg).unwrap())
            .expect_err("过不了 validate 的配置不能导进来");
        assert!(error.contains("duration"), "{error}");
        assert_eq!(
            lock_recover(&console.state).cfg.iperf.duration,
            Config::default().iperf.duration,
            "被拒的导入不能改动任何现有状态"
        );
    }

    /// 监听地址不是访问地址：`0.0.0.0` 弹给浏览器打不开。
    #[test]
    fn the_printed_address_is_one_a_browser_can_actually_open() {
        assert_eq!(display_addr("0.0.0.0", 28800), "127.0.0.1:28800");
        assert_eq!(display_addr("::", 28800), "[::1]:28800");
        assert_eq!(display_addr("[::]", 28800), "[::1]:28800");
        assert_eq!(display_addr(" 0.0.0.0 ", 28800), "127.0.0.1:28800");
        // 绑到具体地址时那个地址本来就是该用的访问地址，不能改写。
        assert_eq!(display_addr("127.0.0.1", 28800), "127.0.0.1:28800");
        assert_eq!(display_addr("192.168.8.101", 28800), "192.168.8.101:28800");
        assert_eq!(display_addr("::1", 28800), "[::1]:28800");
        assert!(bind_is_wildcard("0.0.0.0") && bind_is_wildcard("::"));
        assert!(!bind_is_wildcard("127.0.0.1") && !bind_is_wildcard("192.168.8.101"));
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
