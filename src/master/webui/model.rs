//! 界面与后端之间的报文形状。
//!
//! 全是 serde 结构，没有行为。单独成模块是因为它们同时是**对外契约**：
//! 前端 webui.html 按这些字段名读写，改一个名字就是一次前后端同时的变更。

use super::*;

/// 界面提交回来的一条配对选择。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PairSelection {
    /// `master:NAME=以太网 6`
    pub(super) src: String,
    pub(super) dst: String,
    #[serde(default)]
    pub(super) directions: Vec<String>,
    /// 这一对网口在**双向并发**单元里的接收门限，按方向分开填。
    ///
    /// 只在勾了「双向」时生效；留空 = 双向也走既有的兜底链。
    ///
    /// 为什么按配对而不是按网卡：同一块 RNDIS 口，和 Wi-Fi 组双向、和 SGMII
    /// 组双向，能收到的速率完全不是一个量级。门限挂在网卡上只能填一个数，
    /// 必然有一组是错的——受限的是这条链路，不是某一端的网卡。
    ///
    /// 两个方向分开是因为双向并发时两个方向本来就可以差很远
    /// （同一次运行里见过 1821Mbps 对 17Mbps）。
    #[serde(default)]
    pub(super) rx_target_bidir_ab: String,
    #[serde(default)]
    pub(super) rx_target_bidir_ba: String,
    /// **双向并发**下「两端 RX 合计」门限（Mbps 或百分比文本）。
    ///
    /// 配了它，这个双向单元只按合计判定：`AB 接收端 RX + BA 接收端 RX >= 门限`，
    /// 两条腿各自只测量。Wi-Fi↔Wi-Fi 抢的是同一段空口时间，要求两个方向各达到
    /// 一半没有物理依据；用户要验收的是双向并发下这条链路总共还能过多少。
    /// 留空 = 走上面的每方向门限。
    #[serde(default)]
    pub(super) rx_target_bidir_total: String,
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
    pub(super) udp_groups: Vec<usize>,
    /// 这一行要跑哪几组 TCP 参数。语义和 `udp_groups` 一样：`0` = 默认组
    /// （执行区的 `tcp_windows` / `tcp_streams`），`1..` 指 `RunRequest::tcp_groups`
    /// 里的第 n-1 组。空列表 = 只跑默认组。
    #[serde(default)]
    pub(super) tcp_groups: Vec<usize>,
    #[serde(default)]
    pub(super) transports: Vec<String>,
    #[serde(default)]
    pub(super) ip: Vec<String>,
}

/// 一块网卡在所有配对中共用的判定/负载策略。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct NicPolicySelection {
    /// `master:NAME=以太网 6`
    pub(super) endpoint: String,
    /// 这块网卡作为接收端时的 RX 通过门限。
    ///
    /// 两种写法共用一个输入框：`1800` = 绝对 1800Mbps，`90%` = 协商速率的
    /// 90%。分成两个框会逼着人先想清楚用哪种，而这两种本来就是二选一。
    #[serde(default)]
    pub(super) rx_target: String,
    /// 这块网卡作为发送端时的 UDP 单流带宽；留空表示走全局档位。
    #[serde(default)]
    pub(super) udp_bandwidth: String,
    /// 这块网卡作为发送端时的 UDP 报文长度（`-l`）；留空表示走全局档位。
    #[serde(default)]
    pub(super) udp_length: String,
}

/// 一组 UDP 参数。**自成一体，不继承默认组**：`-l` 留空就是不下发 `-l`，
/// 而不是「跟着执行区那格走」。
///
/// 「有几对带 `-l`、另外几对不带」就是靠这一点表达的：需要的那几行选一个填了
/// `-l` 的组，其余行留在默认组。反过来（默认组填了、某一行想明确不要）在这个
/// 模型里表达不了，也不需要——把它倒过来写就是了。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct UdpGroup {
    /// 显示名。空则页面按序号叫「组 2」「组 3」。
    #[serde(default)]
    pub(super) name: String,
    /// 单流带宽档位，逐档各跑一轮。新建的组必须填，否则这组一个单元都生成不出来。
    #[serde(default)]
    pub(super) bandwidths: Vec<String>,
    #[serde(default)]
    pub(super) lengths: Vec<String>,
    #[serde(default)]
    pub(super) windows: Vec<String>,
    /// 并发流数；0 视作 1（不继承默认组，理由见结构体注释）。
    #[serde(default)]
    pub(super) streams: u32,
}

/// 一组 TCP 参数。和 `UdpGroup` 一样自成一体、不继承默认组：`-w` 留空就是
/// **不下发 `-w`**（用 iperf3 默认窗口），不是「跟着执行区那格走」。
///
/// 两个轴 `-w × -P` 取叉积，各成一个测试单元——这和默认组（执行区的
/// `tcp_windows` × `tcp_streams`）是同一套展开，只是换了一份档位。没有像
/// `UdpGroup` 那样的必填项：`-w`、`-P` 都留空就是最朴素的一条 TCP（默认窗口、
/// 单流），仍是一个合法的组。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TcpGroup {
    /// 显示名。空则页面按序号叫「组 2」「组 3」。
    #[serde(default)]
    pub(super) name: String,
    /// socket buffer 档位（`-w`），逐档各跑一轮。空列表 = 不下发 `-w`。
    #[serde(default)]
    pub(super) windows: Vec<String>,
    /// 并发流数档位（`-P`），逐档各跑一轮。空列表按 `[1]` 处理（等价单流，
    /// 和默认组 `tcp_streams` 留空时一致）。
    #[serde(default)]
    pub(super) streams: Vec<u32>,
}

/// 一组主控/辅测 Wi-Fi 频段门限。单向与双向并发都按两个方向独立配置。
///
/// `src_band`/`dst_band` 与两个无方向值只用于读取 v6.2.2 试验版 request.json；
/// 新前端只写 `master_band`/`agent_band` 与四个方向值。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct WifiBandThreshold {
    pub(super) master_band: String,
    pub(super) agent_band: String,
    pub(super) rx_target_master_to_agent_mbps: f64,
    pub(super) rx_target_agent_to_master_mbps: f64,
    /// 双向并发下**两端 RX 合计**的门限。
    ///
    /// 取代了曾经的两个「每方向双向门限」：Wi-Fi↔Wi-Fi 抢同一段空口时间，
    /// 两个方向怎么分完全取决于调度，要求各自达到一半没有物理依据。
    pub(super) bidir_total_rx_target_mbps: f64,
    /// 兼容旧版按方向填的双向门限；导入时按两者之和迁移到合计。
    pub(super) bidir_rx_target_master_to_agent_mbps: f64,
    pub(super) bidir_rx_target_agent_to_master_mbps: f64,
    /// 兼容旧版有方向的频段规则。
    pub(super) src_band: String,
    pub(super) dst_band: String,
    pub(super) rx_target_mbps: f64,
    pub(super) bidir_rx_target_mbps: f64,
}

/// 兼容旧项目的一对具体 Wi-Fi 网口覆盖；新前端不再创建这类规则。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct WifiPairThreshold {
    pub(super) src_endpoint: String,
    pub(super) dst_endpoint: String,
    pub(super) rx_target_ab_mbps: f64,
    pub(super) rx_target_ba_mbps: f64,
    pub(super) bidir_rx_target_ab_mbps: f64,
    pub(super) bidir_rx_target_ba_mbps: f64,
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
pub(super) enum UiU32Values {
    One(u32),
    Many(Vec<u32>),
}

impl UiU32Values {
    pub(super) fn values(&self) -> Vec<u32> {
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
pub(super) struct UiRecipeProfile {
    pub(super) window: Option<String>,
    pub(super) length: Option<String>,
    pub(super) bandwidth: Option<String>,
    pub(super) streams: UiU32Values,
    pub(super) tcp_streams: Option<UiU32Values>,
    pub(super) udp_streams: Option<UiU32Values>,
}

/// A recipe may use complete `profiles`, or the axis fields below.  The
/// compiler expands axes explicitly and never crosses TCP with UDP.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct UiRecipe {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) mode: String,
    pub(super) profiles: Vec<UiRecipeProfile>,
    pub(super) tcp_windows: Vec<String>,
    pub(super) tcp_streams: Vec<u32>,
    pub(super) bandwidths: Vec<String>,
    pub(super) lengths: Vec<String>,
    pub(super) windows: Vec<String>,
    pub(super) udp_streams: Vec<u32>,
    pub(super) udp_profiles: Vec<UdpProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct UiRecipes {
    pub(super) tcp: Vec<UiRecipe>,
    pub(super) udp: Vec<UiRecipe>,
    #[serde(alias = "pings")]
    pub(super) ping: Vec<UiRecipe>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct UiPairRef {
    pub(super) id: String,
    pub(super) src: String,
    pub(super) dst: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct UiLinkSet {
    pub(super) id: String,
    pub(super) name: String,
    #[serde(alias = "pairs")]
    pub(super) pair_refs: Vec<UiPairRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct UiTask {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) protocol: String,
    #[serde(alias = "transport")]
    pub(super) transports: Vec<String>,
    pub(super) directions: Vec<String>,
    pub(super) ip: Vec<String>,
    #[serde(alias = "recipe_ids", alias = "recipes")]
    pub(super) recipe_ids: Vec<String>,
    pub(super) rx_target_bidir_ab: String,
    pub(super) rx_target_bidir_ba: String,
    /// 双向并发下「两端 RX 合计」门限；配了它这个任务的双向单元只按合计判定。
    pub(super) rx_target_bidir_total: String,
    pub(super) rate_targets_mbps: Option<crate::config::RateTargets>,
    pub(super) rate_mode: Option<crate::config::RateMode>,
    pub(super) duration: Option<u64>,
    pub(super) ping_count: Option<u32>,
    pub(super) ping_payload_sizes: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct UiSuite {
    pub(super) id: String,
    pub(super) name: String,
    #[serde(default)]
    pub(super) note: String,
    pub(super) execution: String,
    #[serde(alias = "lane_order", alias = "task_order")]
    pub(super) order: Vec<String>,
    #[serde(alias = "lanes")]
    pub(super) tasks: Vec<UiTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct UiBinding {
    pub(super) id: String,
    pub(super) link_set_id: String,
    pub(super) suite_id: String,
    pub(super) mode: String,
    pub(super) order: i64,
    #[serde(alias = "pair_ids", alias = "pair_ref_ids")]
    pub(super) pair_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct UiPlan {
    #[serde(alias = "version")]
    pub(super) ui_plan_version: u32,
    pub(super) link_sets: Vec<UiLinkSet>,
    pub(super) recipes: UiRecipes,
    pub(super) suites: Vec<UiSuite>,
    pub(super) bindings: Vec<UiBinding>,
    /// Hash returned by `/api/plan`; excluded while calculating a fresh hash.
    pub(super) plan_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RunRequest {
    #[serde(default)]
    pub(super) pairs: Vec<PairSelection>,
    #[serde(default)]
    pub(super) nic_policies: Vec<NicPolicySelection>,
    #[serde(default = "default_duration")]
    pub(super) duration: u64,
    /// TCP socket buffer 档位，逐档各跑一轮（`-w`）。
    #[serde(default)]
    pub(super) tcp_windows: Vec<String>,
    /// TCP 并发流数档位，逐档各跑一轮（`-P`）。
    #[serde(default)]
    pub(super) tcp_streams: Vec<u32>,
    /// UDP 单流带宽档位，逐档各跑一轮（`-b`）。
    #[serde(default)]
    pub(super) udp_bandwidths: Vec<String>,
    /// UDP 报文长度档位（`-l`）。空列表表示不下发 `-l`，用 iperf3 默认。
    #[serde(default)]
    pub(super) udp_lengths: Vec<String>,
    /// UDP socket buffer 档位（`-w`）。空列表表示不下发 `-w`。
    ///
    /// 和 TCP 的 `-w` 是两个独立的输入：UDP 的 `-w` 挂在每个 udp_profile 上，
    /// TCP 的挂在 `iperf.tcp_windows` 上，共用一个框会让两边互相污染。
    #[serde(default)]
    pub(super) udp_windows: Vec<String>,
    #[serde(default = "default_streams")]
    pub(super) udp_streams: u32,
    /// 默认组之外的 UDP 参数组。矩阵里 `udp_group = 1` 指的是这里的第 0 项。
    #[serde(default)]
    pub(super) udp_groups: Vec<UdpGroup>,
    /// 默认组之外的 TCP 参数组。矩阵里 `tcp_group = 1` 指的是这里的第 0 项。
    #[serde(default)]
    pub(super) tcp_groups: Vec<TcpGroup>,
    /// ping 次数；0 = 不覆盖当前测试配置。
    #[serde(default)]
    pub(super) ping_count: u32,
    /// ping 包长档位（每个档位单独成一个测试单元）；空 = 不覆盖当前测试配置。
    #[serde(default)]
    pub(super) ping_payload_sizes: Vec<u32>,
    /// 兼容旧前端：有线 small 最大 RTT；0 = 沿用配置。
    #[serde(default)]
    pub(super) ping_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_small_max_bytes: u32,
    #[serde(default)]
    pub(super) ping_medium_max_bytes: u32,
    #[serde(default)]
    pub(super) ping_wired_small_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wired_small_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wired_medium_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wired_medium_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wired_large_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wired_large_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_small_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_small_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_medium_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_medium_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_large_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_large_max_rtt_ms: f64,
    /// 兼容旧项目：两端都是 Wi-Fi 时的单向 RX 门限。
    #[serde(default)]
    pub(super) wifi_pair_rx_target_mbps: f64,
    /// 兼容旧项目：两端都是 Wi-Fi 且双向并发时的统一门限。
    #[serde(default)]
    pub(super) wifi_pair_bidir_rx_target_mbps: f64,
    /// 全局 Wi-Fi 双向 **RX 合计** 门限；频段表没填时的最后一层兜底。
    #[serde(default)]
    pub(super) wifi_pair_bidir_total_rx_target_mbps: f64,
    /// 按主控频段 × 辅测频段配置的四向 Wi-Fi 门限。
    #[serde(default)]
    pub(super) wifi_band_thresholds: Vec<WifiBandThreshold>,
    /// 兼容旧项目的具体主控/辅测网口对门限；新前端不再创建。
    #[serde(default)]
    pub(super) wifi_pair_thresholds: Vec<WifiPairThreshold>,
    /// 是否按整条路径的可信上限裁剪 UDP `-b`。
    #[serde(default)]
    pub(super) limit_udp_by_link_speed: bool,
    /// 24 小时内已有正式 PASS 的单元直接跳过。
    #[serde(default)]
    pub(super) resume: bool,
    #[serde(default)]
    pub(super) screenshot: bool,
    /// New suite-plan request.  It is mutually exclusive with legacy `pairs`.
    #[serde(default)]
    pub(super) ui_plan: Option<UiPlan>,
    /// 项目快照带来的**解析后主控配置**：判定与灌包参数的完整基线。
    ///
    /// 只保留 `link_profiles` / `iperf` / `ctstraffic` / `ping` 四块——
    /// 它们合起来就是「这一轮怎么跑、怎么判」的全部。连接身份（辅测机地址、
    /// 口令、网段前缀）、本机运行偏好（`resume` / `screenshot` / `open_report`）
    /// 和命令行的测试矩阵（`tests` / `pairs`）**不在**里面：前者跟着机器走，
    /// 后者控制台用自己的 `ui_plan`。
    ///
    /// 为什么是整块而不是逐字段：`rate_check` 的负载上限、`link_profiles.by_role`
    /// 的角色配对门限、`ctstraffic` 的帧率与缓冲深度，界面上**根本没有输入框**，
    /// 逐字段加通道永远追不完——只要漏一个，同一份项目换台机器就换一套参数，
    /// 而项目文件里一个字都看不出来。
    ///
    /// 合并语义是**深合并**：项目里写了的键覆盖基线，没写的保留基线值。
    /// 老项目文件缺这一块时行为与从前一致。
    #[serde(default)]
    pub(super) master_config: Option<serde_json::Value>,
    /// Optional hash returned by `/api/plan`; checked by `/api/run`.
    #[serde(default)]
    pub(super) plan_hash: Option<String>,
}

pub(super) fn default_duration() -> u64 {
    180
}

pub(super) fn default_streams() -> u32 {
    1
}

#[derive(Debug, Serialize)]
pub(super) struct ConnectOut {
    pub(super) health: HealthOut,
    pub(super) master: HostInfo,
    pub(super) agent: HostInfo,
    pub(super) nic_policies: Vec<NicPolicySelection>,
}

#[derive(Debug, Serialize)]
pub(super) struct BootstrapOut {
    pub(super) agent_host: String,
    pub(super) agent_port: u16,
    pub(super) token_configured: bool,
    pub(super) ipv4_prefixes: Vec<String>,
    pub(super) duration: u64,
    pub(super) tcp_windows: Vec<String>,
    pub(super) tcp_streams: Vec<u32>,
    pub(super) udp_bandwidths: Vec<String>,
    pub(super) udp_lengths: Vec<String>,
    pub(super) udp_windows: Vec<String>,
    pub(super) udp_streams: u32,
    pub(super) ping_count: u32,
    pub(super) ping_payload_sizes: Vec<u32>,
    pub(super) ping_max_rtt_ms: f64,
    pub(super) ping_small_max_bytes: u32,
    pub(super) ping_medium_max_bytes: u32,
    pub(super) ping_wired_small_avg_rtt_ms: f64,
    pub(super) ping_wired_small_max_rtt_ms: f64,
    pub(super) ping_wired_medium_avg_rtt_ms: f64,
    pub(super) ping_wired_medium_max_rtt_ms: f64,
    pub(super) ping_wired_large_avg_rtt_ms: f64,
    pub(super) ping_wired_large_max_rtt_ms: f64,
    pub(super) ping_wifi_small_avg_rtt_ms: f64,
    pub(super) ping_wifi_small_max_rtt_ms: f64,
    pub(super) ping_wifi_medium_avg_rtt_ms: f64,
    pub(super) ping_wifi_medium_max_rtt_ms: f64,
    pub(super) ping_wifi_large_avg_rtt_ms: f64,
    pub(super) ping_wifi_large_max_rtt_ms: f64,
    /// 主控当前生效的**解析后配置**：判定与灌包参数的完整基线。
    ///
    /// 导出项目时原样固化（见 [`RunRequest::master_config`]）。只含
    /// `link_profiles` / `iperf` / `ctstraffic` / `ping` 四块，
    /// **不含任何连接身份**——这份数据会进项目文件，而项目文件是要传阅的。
    pub(super) master_config: serde_json::Value,
    pub(super) screenshot: bool,
    /// Feature flag for pages that can send the suite-oriented `ui_plan` DTO.
    pub(super) ui_plan_supported: bool,
}

/// 本机信息。**不需要连上辅测机**——这是控制台打开就能给出的东西。
#[derive(Debug, Serialize)]
pub(super) struct LocalOut {
    pub(super) host: HostInfo,
    pub(super) iperf3: Option<String>,
    pub(super) version: String,
}

#[derive(Debug, Serialize)]
pub(super) struct PlannedUnit {
    pub(super) seq: usize,
    pub(super) title: String,
    pub(super) est_secs: u64,
    /// 本轮开了 resume，且这个单元在 24 小时内已有 PASS——会被跳过。
    pub(super) resumed: bool,
    /// 这个单元每条腿**最终**下发的参数。
    pub(super) load: Vec<String>,
    /// 这个单元每条腿**最终**按什么门限判、门限来自哪一层。
    ///
    /// 「字段还在、实际却被另一条规则盖掉」光看请求体看不出来，所以预览必须
    /// 直接把最终数字和来源摆出来。
    pub(super) targets: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct PlanOut {
    pub(super) units: Vec<PlannedUnit>,
    pub(super) est_total_secs: u64,
    pub(super) est_full_secs: u64,
    pub(super) notices: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) sections: Vec<PlanSection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) trace: Vec<PlanTrace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) plan_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) topology_fingerprint: Option<String>,
    pub(super) ui_plan_supported: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct PlanTrace {
    pub(super) seq: usize,
    pub(super) pair_id: Option<String>,
    pub(super) link_set_id: Option<String>,
    pub(super) suite_id: Option<String>,
    pub(super) task_id: Option<String>,
    pub(super) lane_id: Option<String>,
    pub(super) recipe_id: Option<String>,
    pub(super) protocol: Option<String>,
    pub(super) direction: Option<String>,
    pub(super) ip: Option<String>,
    pub(super) requested_args: Vec<String>,
    pub(super) effective_args: Vec<String>,
    pub(super) value_sources: Vec<String>,
    pub(super) skipped_reason: Option<String>,
    pub(super) resumed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct PlanSection {
    pub(super) link_set_id: Option<String>,
    pub(super) suite_id: Option<String>,
    pub(super) task_id: Option<String>,
    pub(super) title: String,
    pub(super) unit_seqs: Vec<usize>,
}

#[derive(Debug, Clone)]
pub(super) struct UiSource {
    pub(super) pair_id: String,
    pub(super) link_set_id: String,
    pub(super) suite_id: String,
    pub(super) task_id: String,
    pub(super) recipe_id: String,
    pub(super) protocol: String,
}

pub(super) struct CompiledPlan {
    pub(super) cfg: Config,
    pub(super) units: Vec<builder::Unit>,
    pub(super) notices: Vec<String>,
    pub(super) resumed: Vec<bool>,
    pub(super) trace: Vec<PlanTrace>,
    pub(super) sections: Vec<PlanSection>,
    pub(super) plan_hash: String,
    pub(super) topology_fingerprint: String,
    pub(super) spec_errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct ProgressOut {
    pub(super) running: bool,
    pub(super) from: usize,
    pub(super) lines: Vec<String>,
    pub(super) report: String,
    pub(super) run: RunStatus,
    pub(super) units_from: usize,
}
