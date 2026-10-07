//! 报告的数据模型：行、方向汇总、单元分组。
//!
//! 报告拿到的是一串平铺的 [`Row`]，而人读的是「一个测试单元里有哪几个方向、
//! 每个方向什么结论」。这一层负责的就是这个还原：分组、配对、补齐缺失字段。
//! 它不产出任何 HTML——渲染在 `html` 里，判定在 [`crate::verdict`] 里。

use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StreamCounts {
    pub requested: usize,
    pub active: usize,
    pub required: usize,
}

/// 它嵌在 `Row.direction_summaries` 里一起落进 `rows.jsonl`，所以和 `Row`
/// 一样是兼容面。`Row` 早就是 `#[serde(default)]`，这里以前不是——只要给
/// 本结构加一个字段，旧 run 目录就会以 `missing field` 整行读不回来。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectionSummary {
    pub tag: String,
    pub src: String,
    pub dst: String,
    pub verdict: Verdict,
    pub reason_code: ReasonCode,
    pub reason_detail: String,
    /// 兼容旧调用方；新代码优先填写 `reason_code` / `reason_detail`。
    pub reason: String,
    pub streams: Option<StreamCounts>,
    pub rx_avg: Option<f64>,
    pub rx_p10: Option<f64>,
    /// 发送端网卡 TX 平均。**不参与判定**——PASS/FAIL 只看接收端 RX。
    /// 摆在 RX 旁边是为了让「收不到」和「压根没发出去」当场分得开。
    pub tx_avg: Option<f64>,
    pub target_mbps: Option<f64>,
    pub sample_coverage: Option<f64>,
    pub udp_loss: Option<f64>,
    /// TCP 全程重传次数。与 `udp_loss` 平级：都是**只作诊断**的质量指标，
    /// 都不参与判定（ADR-17）。
    pub tcp_retransmits: Option<u64>,
    pub ping_loss: Option<f64>,
    pub ping_min: Option<f64>,
    pub ping_avg: Option<f64>,
    pub ping_max: Option<f64>,
    /// 该方向主行的截图路径；概览把接收速率和截图并排展示。
    pub screenshot_master: String,
    pub screenshot_agent: String,
    pub screenshot_errors: Vec<String>,
    /// 该方向主行的接收端逐样本 CSV 路径。概览的缩略曲线从它读。
    ///
    /// 和截图同一个道理：概览要展示的东西，得先在这一层能拿到。
    pub nic_samples_rx: String,
}

/// 当前写出的对齐身份版本。
///
/// - 0：6.5.1 写下的身份（没有这个字段）。参数是实际下发的标签，可能带着路径裁剪、
///   链路策略说明和 CTS 的「×N流」，多流 UDP 还按流重复。
/// - 2：参数是计划里请求的档位（`comparison_label`），每条腿一项。
///
/// 旧报告从明细行还原出来的身份同样按 0 处理。版本只决定读取时要不要做历史兜底，
/// 对齐键里一律写成当前版本，新旧两类身份才能落在同一把键上。
pub const COMPARISON_IDENTITY_VERSION: u32 = 2;

/// 跨轮对比身份：不含 IP 地址、协商速率、运行序号或测量结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonIdentity {
    /// 见 [`COMPARISON_IDENTITY_VERSION`]；6.5.1 的记录没有这个字段，读进来是 0。
    #[serde(default)]
    pub version: u32,
    pub bidir: bool,
    pub round: u32,
    pub legs: Vec<ComparisonLeg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonLeg {
    pub ip: String,
    pub protocol: RowProtocol,
    pub backend: RowBackend,
    pub src_side: RowSide,
    pub src_iface: String,
    pub dst_side: RowSide,
    pub dst_iface: String,
    /// 请求的参数。新记录每条腿一项（不按流展开、不含路径裁剪与按网口策略的改写）；
    /// 6.5.1 写下的记录由 `report::compare` 归一后再对齐，见 [`COMPARISON_IDENTITY_VERSION`]。
    pub parameters: Vec<String>,
    pub seconds: Option<u64>,
}

/// 这一行测的是**哪个方向**。
///
/// 报告过去是从 `kind_label` 里搜 `-ab`/`-ba` 反推的（`infer_direction_tag`）。
/// 那个 label 是给人看的展示串，一旦改文案（比如把「灌包-ab」换成「灌包 A→B」）
/// 方向就会集体退化成「单向」，而没有任何测试会红。Excel 出口一旦上线，
/// 同一份脆弱推断就要被复制第二遍——所以在那之前先把它变成类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowDirection {
    /// 单向单元。执行侧的 `Leg.tag` 对它是**空串**（见 `builder::dir_pairs`），
    /// 那个空串在执行侧有语义，不能为了显示去动它。
    #[default]
    Single,
    Ab,
    Ba,
}

impl RowDirection {
    pub fn from_leg_tag(tag: &str) -> Self {
        if tag.eq_ignore_ascii_case("ab") {
            RowDirection::Ab
        } else if tag.eq_ignore_ascii_case("ba") {
            RowDirection::Ba
        } else {
            RowDirection::Single
        }
    }

    /// 报告里显示的方向标签。与 `normalized_direction_tag` 的取值一致。
    pub fn label(self) -> &'static str {
        match self {
            RowDirection::Single => "单向",
            RowDirection::Ab => "AB",
            RowDirection::Ba => "BA",
        }
    }
}

/// 这一行跑的是哪种传输协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowProtocol {
    /// 诊断行、单元汇总行这类不绑定协议的行。
    #[default]
    None,
    Tcp,
    Udp,
    Icmp,
}

impl RowProtocol {
    pub fn label(self) -> &'static str {
        match self {
            RowProtocol::None => "",
            RowProtocol::Tcp => "TCP",
            RowProtocol::Udp => "UDP",
            RowProtocol::Icmp => "ICMP",
        }
    }
}

/// 这一行是哪个工具跑出来的。
///
/// 报告过去靠标题里含不含 "PING"/"UDP" 猜（`group_is_ping`/`group_is_udp`）——
/// 一条名字里带 "UDP" 的 TCP 测试就能把整组带偏。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowBackend {
    #[default]
    None,
    Iperf3,
    CtsTraffic,
    Ping,
}

impl RowBackend {
    /// 目前没有本地消费者：HTML 把后端信息混在 `transport` 列里（`CTS/TCP`）。
    /// Excel 出口（R3）会用它——那正是 ADR-7 要求「赶在第二个消费者之前类型化」
    /// 的原因，所以这里先把口径定下来。
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            RowBackend::None => "",
            RowBackend::Iperf3 => "iperf3",
            RowBackend::CtsTraffic => "ctsTraffic",
            RowBackend::Ping => "ping",
        }
    }
}

/// 端点在哪一台机器上。
///
/// 与 `builder::Side` 同构，但**不复用它**：`report` 是纯消费端，让它反过来依赖
/// `master::builder` 会把「报告只读结果」这条边弄脏。转换发生在 executor 侧
/// （那里两个类型都在手边）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowSide {
    #[default]
    Unknown,
    Master,
    Agent,
}

impl RowSide {
    /// 同 [`RowBackend::label`]：留给 Excel 出口（R3）的「端」列。
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            RowSide::Unknown => "",
            RowSide::Master => "主控",
            RowSide::Agent => "辅测",
        }
    }
}

/// 一行结果。**这是全仓唯一的结果模型**（ADR-7）。
///
/// serde 派生是给 `runs/<run>/rows.jsonl` 用的（ADR-3）：每个单元跑完就把该
/// 单元新增的行追加落盘，报告因此可以从落盘数据**重放**出来。在此之前结果一直
/// 活在 `Ctx.rows` 这个内存 `Vec` 里、直到整轮结束才写报告——主控在第 10 小时
/// 崩溃/断电/被 kill，十小时的测量数据、原因码、方向明细全部蒸发，只剩
/// `task_results.json` 里的单元级 PASS 布尔。
///
/// **落盘形状因此成为兼容面**：字段名进了文件，改名字等于让旧 run 目录读不回来。
/// 版本号写在 `meta.json` 里，重放器容忍未知字段（`#[serde(default)]` 靠 Default）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Row {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison_identity: Option<ComparisonIdentity>,
    /// (unit序, leg序, 流序, 组合计标记) 用于稳定排序
    pub sort_key: (usize, usize, usize, u8),
    pub time: String,
    pub task_id: String,
    pub parent_id: String,
    pub task: String,
    pub ip: String,
    pub transport: String,
    pub param: String,
    pub src_pc: String,
    pub src_iface: String,
    pub src_ip: String,
    pub dst_pc: String,
    pub dst_iface: String,
    pub dst_ip: String,
    /// 本行跑的时候，源端网口的无线上下文（SSID / 信道 / 信号 / 无线电类型）。
    ///
    /// 非 Wi-Fi 口是空串。取的是**这一单元开跑前那次重扫**看到的值，所以
    /// Wi-Fi 重新协商之后前后两个单元可以不一样——那正是要记下来的东西。
    pub src_wifi: String,
    /// 同 [`Row::src_wifi`]，目标端网口。
    pub dst_wifi: String,
    pub verdict: Verdict,
    pub execution_status: ExecutionStatus,
    pub reason_code: ReasonCode,
    pub reason_detail: String,
    /// **不参与判定**的排障线索（ADR-17）。
    ///
    /// UDP 丢包、发送端负载、滚动窗口覆盖、工具退出状态这些事实以前是判定
    /// 分支，会在接收端 RX 已经达标之后把 PASS 翻成 RATE_FAIL。现在它们走
    /// 这条通道：报告里照样看得见，但 `verdict` 只由接收端 RX 平均与门限决定。
    pub diagnostics: Vec<String>,
    pub kind_label: String,
    /// 稳定性轮次（1-based）。`0` = 历史数据，那时还没有这个字段；不分轮的计划是 `1`。
    ///
    /// 存在的理由只有一个：`report::compare` 的对齐键要能把第 3 轮和第 4 轮分开。
    /// 那个键刻意不用 `Unit.id`（里面拌了 `speed_mbps`，Wi-Fi 一重协商同一条测试
    /// 就成了两个 id），于是轮次必须单独有一条通路——否则 20 轮的计划在
    /// `HashMap` 里互相覆盖，只剩最后一轮，而对比报告不会有任何异常提示。
    pub round: u32,
    pub rx_avg: Option<f64>,
    pub peer_rx: String,
    pub tx_mbps: Option<f64>,
    pub rx_mbps: Option<f64>,
    pub udp_loss: Option<f64>,
    /// 灌包**期间**并发探到的往返时延，已经格式化成一句人话；空串 = 这一轮
    /// 没开探针（`ping.probe_during_traffic`）或压根没探到。
    ///
    /// 空载 RTT 和负载下 RTT 差两个数量级，而用户感知到的「卡」几乎全部落在
    /// 后者。**只作诊断**：负载下时延再难看也不改写判定（ADR-17）。
    ///
    /// 存成展示串而不是三个数：它进的是诊断面板的一格，没有第二个消费者要拿它
    /// 做算术。真出现了再类型化——那正是 ADR-7 说的「赶在第二个消费者之前」。
    pub load_latency: String,
    /// TCP 全程重传次数（iperf3 sender 汇总行的 `Retr`）。
    ///
    /// 报告的「质量」列此前对 TCP 恒为 `—`：UDP 有丢包、ping 有 RTT，
    /// 唯独 TCP 那一格什么都不说。重传正是这一格该有的东西——TCP 没跑满时，
    /// 它区分「链路在丢包」和「窗口没喂饱」，而这两种结论的整改动作相反。
    ///
    /// **只作诊断，不参与判定**：达标与否仍然只看接收端 RX 平均。
    /// `None` = 这一行不是 TCP，或这段输出没有 `Retr` 列。
    pub tcp_retransmits: Option<u64>,
    pub ping_loss: Option<f64>,
    pub ping_min: Option<f64>,
    pub ping_avg: Option<f64>,
    pub ping_max: Option<f64>,
    /// 主控端截图路径
    pub screenshot_master: String,
    /// 辅测端截图路径
    pub screenshot_agent: String,
    /// 按端保存截图失败原因；空列表兼容旧报告与未尝试截图的行。
    pub screenshot_errors: Vec<String>,
    pub command: String,
    /// 独立落盘的 iperf client/server/事件原始记录。
    pub raw_log: String,
    /// 接收端网卡累计计数器的逐样本 CSV（独立落盘）。
    pub nic_samples_rx: String,
    /// **发送端**网卡的逐样本 CSV。
    ///
    /// TX 采样是否决性门槛：`rate_window_coverage_sufficient` 要求 TX 滚动覆盖率
    /// ≥0.95 且 `tx.p10` 在，否则整行判 NOT_EVALUATED；`tx_sufficient` 还决定
    /// 会不会报 `OFFERED_LOAD_LOW`。可是在此之前 iperf/CTS 两条路径**从不落盘
    /// TX 逐样本**——`save_monitor_samples` 只传 dst/RX。于是
    /// 「报告里的每个结论都要能回到某一行样本」（`artifact.rs` 模块头自己的话）
    /// 对 TX 不成立：判 NOT_EVALUATED 的理由是 TX 覆盖率不够，而那份 TX 样本
    /// 谁也拿不到。
    pub nic_samples_tx: String,
    /// (标题, 原始输出)
    pub raws: Vec<(String, String)>,
    pub is_grouptotal: bool,
    pub is_unit_summary: bool,
    pub requested_streams: usize,
    pub active_streams: usize,
    pub required_streams: usize,
    pub retry_count: usize,
    pub tx_avg: Option<f64>,
    pub tx_p10: Option<f64>,
    pub rx_p10: Option<f64>,
    pub rx_median: Option<f64>,
    pub rx_p95: Option<f64>,
    pub rx_min: Option<f64>,
    pub rx_max: Option<f64>,
    pub target_mbps: Option<f64>,
    pub effective_seconds: Option<f64>,
    pub required_seconds: Option<f64>,
    pub sample_coverage: Option<f64>,
    /// 本行判定实际使用的网卡样本区间（相对该测试单元 epoch 的毫秒）。
    ///
    /// 报告里已经有逐样本 CSV、采样覆盖率和有效/要求时长，但三者对不上号：
    /// 看不出判定窗口是 CSV 里的哪一段。验收要求核对背景扣除是否合理，
    /// 没有这两个端点就只能自己反推。
    pub window_start_ms: Option<u64>,
    pub window_end_ms: Option<u64>,
    /// 已从每个样本中扣除的背景速率中位数。
    pub baseline_mbps: Option<f64>,
    /// 完整 5 秒滚动窗口的覆盖率；与总采样覆盖率是两个不同的门槛。
    pub rolling_coverage: Option<f64>,
    /// 每个测试方向的判定指标；报告概览优先使用该字段。
    pub direction_summaries: Vec<DirectionSummary>,

    // ---- 类型化的结构字段（ADR-7）----
    //
    // 下面这些以前全靠从展示串里推断：方向搜 `kind_label` 里的 `-ab`/`-ba`、
    // ping 看标题含不含 "PING"、UDP 看标题含不含 "UDP"。HTML、Excel、API 三个
    // 出口即将并存，字符串推断会被复制三份，所以在第二个消费者落地之前先类型化。
    // 推断函数降级为**兜底**：只有历史数据（没有这些字段的 rows.jsonl）才走它们。
    /// 单元序号，与日志里的 `[i/total]` 和报告里的 `#N` 同源。
    pub unit_seq: usize,
    // 下面三个字段目前只被**写入**：它们是给 rows.jsonl 落盘、Excel 出口和
    // `/api` 的运行状态用的（R3）。ADR-7 要求赶在第二个消费者落地**之前**
    // 把它们类型化，否则 `group_is_udp` 那类字符串推断会被复制第二遍——
    // 所以先有字段、后有消费者是这里刻意的顺序，不是遗留。
    pub direction: RowDirection,
    pub protocol: RowProtocol,
    pub backend: RowBackend,
    /// 报表分组键。来源优先级：链路集合名 → 物理网口对 → `role_a ↔ role_b`。
    /// **永不用主机名**（Arch 机自报 `UNKNOWN-PC`）。
    #[allow(dead_code)]
    pub link_group: String,
    #[allow(dead_code)]
    pub src_side: RowSide,
    #[allow(dead_code)]
    pub dst_side: RowSide,
}

#[derive(Debug, Clone, Default)]
pub struct ReportMeta {
    pub master_pc: String,
    pub agent_pc: String,
    pub agent_host: String,
    pub started: String,
    pub finished: String,
    pub elapsed: String,
    /// 本机网卡采样口径的已知差异（例如 macOS 经由 netstat 子进程采样）。
    /// 空表示采样方式与主要目标平台一致，不必额外提示。
    pub counter_source_caveat: String,
    /// 本轮运行健康横幅：链路中途失联、队列被中止之类必须在最顶上
    /// 说清楚的事实。空表示没有需要提示的异常。
    ///
    /// 之所以放在报告最顶而不是混进某一行的原因里：链路失联影响的是
    /// 一整段单元，逐行看的人永远拼不出「从某一刻起后面全是空跑」这件事。
    pub run_health: String,
    /// 这一轮计划展开时给出的提示（门限按链路上限折算、UDP 被路径上限裁剪、
    /// 被跳过的项目……），与运行日志开头逐条一致。
    ///
    /// 以前只进日志：配 1180 被折算成 950 判 PASS，报告上只看得到 950，读报告的人
    /// 无从知道门限被改过。
    pub plan_notices: Vec<String>,
}

pub struct UnitGroup<'a> {
    pub(super) key: String,
    pub(super) summary: Option<&'a Row>,
    pub(super) details: Vec<&'a Row>,
}

pub(super) fn row_unit_key(row: &Row) -> String {
    if !row.parent_id.is_empty() {
        row.parent_id.clone()
    } else if row.is_unit_summary && !row.task_id.is_empty() {
        row.task_id.clone()
    } else {
        // 与报告的展示序号同源；旧 rows.jsonl 的 unit_seq 可能尚未落盘。
        format!("unit-{}", row.sort_key.0)
    }
}

pub(super) fn group_rows(rows: &[Row]) -> Vec<UnitGroup<'_>> {
    let mut groups: Vec<UnitGroup<'_>> = Vec::new();
    let mut sequences =
        std::collections::BTreeMap::<String, std::collections::BTreeSet<usize>>::new();
    for row in rows {
        sequences
            .entry(row_unit_key(row))
            .or_default()
            .insert(row.sort_key.0);
    }
    // HTML、Excel、两轮对比都接受落盘顺序，统一按实际执行序还原。
    // 只排序引用，避免复制每行携带的原始输出。
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by_key(|row| row.sort_key);
    for row in ordered {
        let identity = row_unit_key(row);
        // 保持普通历史报告的锚点；只有身份重复执行时才按运行序拆分。
        let key = if sequences[&identity].len() > 1 {
            format!("{identity}:seq={}", row.sort_key.0)
        } else {
            identity
        };
        let index = groups
            .iter()
            .position(|group| group.key == key)
            .unwrap_or_else(|| {
                groups.push(UnitGroup {
                    key: key.clone(),
                    summary: None,
                    details: Vec::new(),
                });
                groups.len() - 1
            });
        if row.is_unit_summary {
            groups[index].summary = Some(row);
        } else {
            groups[index].details.push(row);
        }
    }
    groups
}

pub(super) fn group_verdict(group: &UnitGroup<'_>) -> Verdict {
    // 有单元汇总行时直接采信 executor 的聚合结果；没有（旧报告数据、被中断的
    // 运行）时用同一个 aggregate_verdict 复算，绝不在这里另写一套优先级。
    group.summary.map(|row| row.verdict).unwrap_or_else(|| {
        aggregate_verdict(
            group
                .details
                .iter()
                .map(|row| (row.verdict, row.reason_code)),
        )
    })
}

/// 汇总缺失时，选取与聚合判定一致的明细作为原因和指标的来源。
pub(super) fn verdict_row<'a>(group: &UnitGroup<'a>) -> Option<&'a Row> {
    group.summary.or_else(|| {
        let verdict = group_verdict(group);
        group
            .details
            .iter()
            .copied()
            .find(|row| row.verdict == verdict)
    })
}

pub(super) fn group_execution_status(group: &UnitGroup<'_>) -> ExecutionStatus {
    group
        .summary
        .map(|row| row.execution_status)
        .or_else(|| group.details.last().map(|row| row.execution_status))
        .unwrap_or_default()
}

pub(super) fn unit_open_by_default(verdict: Verdict) -> bool {
    matches!(
        verdict,
        Verdict::RateFail | Verdict::NotEvaluated | Verdict::SetupError
    )
}

/// 测试单元的执行序号，与控制台打印的 `[N/总数]` 完全一致。
///
/// 报告和控制台是同一次运行的两份记录，抄结果的人要在两边来回对。
/// 概览里只有标题的话，「主控 以太网 6 -> 辅测 以太网」这类标题在
/// 120 个单元里会重复出现十几次，光靠标题根本定位不到是哪一条。
pub(super) fn group_seq(group: &UnitGroup<'_>) -> usize {
    group
        .summary
        .map(|row| row.sort_key.0)
        .or_else(|| group.details.first().map(|row| row.sort_key.0))
        .unwrap_or(0)
        .saturating_add(1)
}

pub(super) fn group_title<'a>(group: &'a UnitGroup<'_>) -> &'a str {
    group
        .summary
        .map(|row| row.task.as_str())
        .or_else(|| group.details.first().map(|row| row.task.as_str()))
        .unwrap_or("未命名测试单元")
}

/// 这一行的方向标签。**先看类型化字段，推断只是兜底。**
///
/// `RowDirection::Single` 既是「真的单向」，也是历史数据（rows.jsonl 里没有
/// 类型化字段的老行）反序列化出来的默认值。这两种情况可以共用兜底而不冲突：
/// 真单向的 `kind_label` 里本来就没有 `-ab`/`-ba`，推断出来还是「单向」。
pub(super) fn direction_tag(row: &Row) -> String {
    match row.direction {
        RowDirection::Ab | RowDirection::Ba => row.direction.label().to_string(),
        RowDirection::Single => infer_direction_tag(row),
    }
}

/// 从展示串里猜方向。**只作兜底**，见 [`direction_tag`]。
///
/// 它读的是 `kind_label`——一个给人看的字符串。把「灌包-ab」改成「灌包 A→B」
/// 之类的文案调整，会让这里集体退化成「单向」，而没有任何测试会红。
pub(super) fn infer_direction_tag(row: &Row) -> String {
    let label = row.kind_label.to_ascii_lowercase();
    if label.contains("-ab") {
        "AB".into()
    } else if label.contains("-ba") {
        "BA".into()
    } else {
        "单向".into()
    }
}

pub(super) fn normalized_direction_tag(tag: &str) -> String {
    if tag.eq_ignore_ascii_case("ab") {
        "AB".into()
    } else if tag.eq_ignore_ascii_case("ba") {
        "BA".into()
    } else if tag.is_empty() {
        "单向".into()
    } else {
        tag.to_string()
    }
}

pub(super) fn row_is_ping(row: &Row) -> bool {
    if row.protocol != RowProtocol::None {
        return row.protocol == RowProtocol::Icmp;
    }
    if row.backend != RowBackend::None {
        return row.backend == RowBackend::Ping;
    }
    // 类型化字段优先；下面那串是历史数据的兜底（标题里含 "PING" 的 TCP 测试
    // 会被它误判，这正是 ADR-7 要把它降级的原因）。
    row.backend == RowBackend::Ping
        || row.ping_loss.is_some()
        || row.ping_min.is_some()
        || row.ping_avg.is_some()
        || row.ping_max.is_some()
        || row.kind_label.to_ascii_uppercase().contains("PING")
        || row.task.to_ascii_uppercase().contains("PING")
}

pub(super) fn group_is_ping(group: &UnitGroup<'_>) -> bool {
    let typed: Vec<_> = group
        .summary
        .into_iter()
        .chain(group.details.iter().copied())
        .filter(|row| row.protocol != RowProtocol::None || row.backend != RowBackend::None)
        .collect();
    if !typed.is_empty() {
        return typed.into_iter().any(row_is_ping);
    }
    group.summary.is_some_and(row_is_ping) || group.details.iter().any(|row| row_is_ping(row))
}

pub(super) fn stream_counts(row: &Row) -> Option<StreamCounts> {
    (row.requested_streams > 0 || row.active_streams > 0 || row.required_streams > 0).then_some(
        StreamCounts {
            requested: row.requested_streams,
            active: row.active_streams,
            required: row.required_streams,
        },
    )
}

impl Row {
    /// 把这一行折成一个方向摘要。
    ///
    /// **这是 `Row` → `DirectionSummary` 的唯一映射。** 在此之前它有两份逐字段
    /// 手抄的实现（`report::model::direction_from_row` 与
    /// `executor::direction_summaries`），互相之间没有任何同步机制——两边各搬了
    /// 14 个字段，谁也不保证搬的是同一批。合并之后，概览想多显示一个指标就是
    /// 「`DirectionSummary` 加一个字段、这里填一次」，而不是「记得两处都改」。
    ///
    /// 执行侧的调用点在此基础上覆盖 `tag`/`verdict`/`reason_*` 四项：那四项它有
    /// 更权威的来源（腿的判定结果），其余指标一律共用这里这一份。
    pub fn direction_summary(&self) -> DirectionSummary {
        DirectionSummary {
            tag: direction_tag(self),
            src: report_endpoint(&self.src_pc, &self.src_iface, &self.src_ip, &self.src_wifi),
            dst: report_endpoint(&self.dst_pc, &self.dst_iface, &self.dst_ip, &self.dst_wifi),
            verdict: self.verdict,
            reason_code: self.reason_code,
            reason_detail: self.reason_detail.clone(),
            reason: if self.reason_code.is_empty() && self.reason_detail.is_empty() {
                String::new()
            } else {
                report_reason(self.reason_code, &self.reason_detail)
            },
            streams: stream_counts(self),
            rx_avg: self.rx_avg,
            rx_p10: self.rx_p10,
            tx_avg: self.tx_avg,
            target_mbps: self.target_mbps,
            sample_coverage: self.sample_coverage,
            udp_loss: self.udp_loss,
            tcp_retransmits: self.tcp_retransmits,
            ping_loss: self.ping_loss,
            ping_min: self.ping_min,
            ping_avg: self.ping_avg,
            ping_max: self.ping_max,
            screenshot_master: self.screenshot_master.clone(),
            screenshot_agent: self.screenshot_agent.clone(),
            screenshot_errors: self.screenshot_errors.clone(),
            nic_samples_rx: self.nic_samples_rx.clone(),
        }
    }
}

pub(super) fn direction_from_row(row: &Row) -> DirectionSummary {
    row.direction_summary()
}

pub(super) fn direction_row_score(row: &Row) -> u8 {
    u8::from(row.is_grouptotal) * 16
        + u8::from(row.rx_p10.is_some()) * 8
        + u8::from(row.rx_avg.is_some()) * 4
        + u8::from(row.sample_coverage.is_some()) * 2
        + u8::from(
            row.ping_loss.is_some()
                || row.ping_min.is_some()
                || row.ping_avg.is_some()
                || row.ping_max.is_some(),
        )
}

pub(super) fn fallback_direction_summaries(group: &UnitGroup<'_>) -> Vec<DirectionSummary> {
    let mut selected: Vec<(String, &Row)> = Vec::new();
    for row in &group.details {
        let tag = direction_tag(row);
        if let Some((_, current)) = selected.iter_mut().find(|(current, _)| *current == tag) {
            if direction_row_score(row) > direction_row_score(current) {
                *current = row;
            }
        } else {
            selected.push((tag, row));
        }
    }
    if selected.is_empty() {
        group.summary.map(direction_from_row).into_iter().collect()
    } else {
        selected
            .into_iter()
            .map(|(_, row)| direction_from_row(row))
            .collect()
    }
}

pub(super) fn merge_missing_direction_fields(
    target: &mut DirectionSummary,
    fallback: &DirectionSummary,
) {
    if target.src.is_empty() {
        target.src.clone_from(&fallback.src);
    }
    if target.dst.is_empty() {
        target.dst.clone_from(&fallback.dst);
    }
    if target.reason_code.is_empty() && target.reason_detail.is_empty() && target.reason.is_empty()
    {
        target.reason_code.clone_from(&fallback.reason_code);
        target.reason_detail.clone_from(&fallback.reason_detail);
        target.reason.clone_from(&fallback.reason);
    }
    if target.streams.is_none() {
        target.streams = fallback.streams;
    }
    if target.rx_avg.is_none() {
        target.rx_avg = fallback.rx_avg;
    }
    if target.rx_p10.is_none() {
        target.rx_p10 = fallback.rx_p10;
    }
    if target.tx_avg.is_none() {
        target.tx_avg = fallback.tx_avg;
    }
    if target.target_mbps.is_none() {
        target.target_mbps = fallback.target_mbps;
    }
    if target.sample_coverage.is_none() {
        target.sample_coverage = fallback.sample_coverage;
    }
    if target.udp_loss.is_none() {
        target.udp_loss = fallback.udp_loss;
    }
    if target.tcp_retransmits.is_none() {
        target.tcp_retransmits = fallback.tcp_retransmits;
    }
    if target.ping_loss.is_none() {
        target.ping_loss = fallback.ping_loss;
    }
    if target.ping_min.is_none() {
        target.ping_min = fallback.ping_min;
    }
    if target.ping_avg.is_none() {
        target.ping_avg = fallback.ping_avg;
    }
    if target.ping_max.is_none() {
        target.ping_max = fallback.ping_max;
    }
    if target.screenshot_master.is_empty() {
        target
            .screenshot_master
            .clone_from(&fallback.screenshot_master);
    }
    if target.screenshot_agent.is_empty() {
        target
            .screenshot_agent
            .clone_from(&fallback.screenshot_agent);
    }
    if target.screenshot_errors.is_empty() {
        target
            .screenshot_errors
            .clone_from(&fallback.screenshot_errors);
    }
    // 逐样本 CSV 路径和截图路径同一个道理：升级前落盘的 `direction_summaries`
    // 里没有这个字段，读回来是空串。不从明细行回填的话，**重放旧 run 目录时
    // 概览的曲线列会整列消失**——而那些目录里 CSV 明明还在。
    if target.nic_samples_rx.is_empty() {
        target.nic_samples_rx.clone_from(&fallback.nic_samples_rx);
    }
}

pub(super) fn group_direction_summaries(group: &UnitGroup<'_>) -> Vec<DirectionSummary> {
    let fallback = fallback_direction_summaries(group);
    let Some(summary) = group.summary else {
        return fallback;
    };
    if summary.direction_summaries.is_empty() {
        return fallback;
    }

    let mut directions = summary.direction_summaries.clone();
    for direction in &mut directions {
        if let Some(detail) = fallback
            .iter()
            .find(|detail| detail.tag.eq_ignore_ascii_case(&direction.tag))
        {
            merge_missing_direction_fields(direction, detail);
        }
    }
    directions
}

/// 双向单元的两个接收方向 RX 平均合计。
///
/// 配了合计门限时只取执行器保存的共同窗口判定值，缺失时不得以腿平均之和替代。
/// 未配合计门限时，两个方向各自窗口的平均之和只作诊断。
pub(super) fn bidirectional_rx_average_sum(group: &UnitGroup<'_>) -> Option<f64> {
    if let Some(summary) = group
        .summary
        .filter(|row| row.target_mbps.is_some() && group_is_bidirectional(group))
    {
        return summary.rx_avg.filter(|value| value.is_finite());
    }
    let directions = group_direction_summaries(group);
    let ab = directions
        .iter()
        .find(|direction| direction.tag.eq_ignore_ascii_case("AB"))?
        .rx_avg?;
    let ba = directions
        .iter()
        .find(|direction| direction.tag.eq_ignore_ascii_case("BA"))?
        .rx_avg?;
    (ab.is_finite() && ba.is_finite()).then_some(ab + ba)
}

pub(super) fn group_is_udp(group: &UnitGroup<'_>) -> bool {
    let protocols: Vec<_> = group
        .summary
        .into_iter()
        .chain(group.details.iter().copied())
        .map(|row| row.protocol)
        .filter(|protocol| *protocol != RowProtocol::None)
        .collect();
    if !protocols.is_empty() {
        return protocols.contains(&RowProtocol::Udp);
    }
    // 类型化字段优先。标题匹配是历史数据的兜底：一条名字里带 "UDP" 的 TCP
    // 测试就能把整组带偏，而报表上看不出来是带偏了。
    group
        .summary
        .is_some_and(|row| row.protocol == RowProtocol::Udp)
        || group
            .details
            .iter()
            .any(|row| row.protocol == RowProtocol::Udp)
        || group_title(group).to_ascii_uppercase().contains("UDP")
        || group.summary.is_some_and(|row| {
            row.transport.eq_ignore_ascii_case("UDP")
                || row.task.to_ascii_uppercase().contains("UDP")
        })
        || group.details.iter().any(|row| {
            row.transport.eq_ignore_ascii_case("UDP")
                || row.task.to_ascii_uppercase().contains("UDP")
        })
}

pub(super) fn group_is_bidirectional(group: &UnitGroup<'_>) -> bool {
    let directions = group_direction_summaries(group);
    let has_ab = directions
        .iter()
        .any(|direction| direction.tag.eq_ignore_ascii_case("AB"));
    let has_ba = directions
        .iter()
        .any(|direction| direction.tag.eq_ignore_ascii_case("BA"));
    (has_ab && has_ba) || group_title(group).contains("双向")
}

/// 报告的顶层分类。
///
/// 按**协议**分，不按工具分。ctsTraffic 只是 TCP/UDP 的一种执行引擎，它和
/// iperf3 回答的是同一个问题——「这条链路跑这个协议能到多少」——只是过程指标
/// 不同。过程差异写在明细里就够了，在目录上再分一层只会让人以为那是两类结论。
///
/// 反过来，UDP 和 TCP 必须分开：两者的失败形态、要看的指标、以及「不达标」
/// 意味着什么都不一样（UDP 要同时看丢包和灌够没有，TCP 不用），并排列在一张
/// 表里会诱导人横向比较两个不可比的数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReportSection {
    Ping,
    Udp,
    Tcp,
}

impl ReportSection {
    pub(super) fn title(self) -> &'static str {
        match self {
            ReportSection::Ping => "Ping",
            ReportSection::Udp => "灌包性能 · UDP",
            ReportSection::Tcp => "灌包性能 · TCP",
        }
    }

    pub(super) fn anchor(self) -> &'static str {
        match self {
            ReportSection::Ping => "ping",
            ReportSection::Udp => "udp",
            ReportSection::Tcp => "tcp",
        }
    }
}

pub(super) fn group_section(group: &UnitGroup<'_>) -> ReportSection {
    if group_is_ping(group) {
        ReportSection::Ping
    } else if group_is_udp(group) {
        ReportSection::Udp
    } else {
        ReportSection::Tcp
    }
}

/// 按分类切开，**空分类不出现**。
///
/// 「这次没跑 TCP」和「这次 TCP 全挂了」必须一眼能分开：前者不该在报告里留下
/// 一个空标题让人以为漏了，后者必须显眼。所以这里返回的分类一定非空。
/// 组内顺序保持原样——那是执行顺序，报告不该重排。
pub(super) fn sectioned<'a, 'r>(
    groups: &'a [UnitGroup<'r>],
) -> Vec<(ReportSection, Vec<&'a UnitGroup<'r>>)> {
    let mut out: Vec<(ReportSection, Vec<&'a UnitGroup<'r>>)> = Vec::new();
    for section in [ReportSection::Ping, ReportSection::Udp, ReportSection::Tcp] {
        let picked: Vec<&'a UnitGroup<'r>> = groups
            .iter()
            .filter(|group| group_section(group) == section)
            .collect();
        if !picked.is_empty() {
            out.push((section, picked));
        }
    }
    out
}
