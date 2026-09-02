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
    pub ping_loss: Option<f64>,
    pub ping_min: Option<f64>,
    pub ping_avg: Option<f64>,
    pub ping_max: Option<f64>,
    /// 该方向主行的截图路径；概览把接收速率和截图并排展示。
    pub screenshot_master: String,
    pub screenshot_agent: String,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
    pub rx_avg: Option<f64>,
    pub peer_rx: String,
    pub tx_mbps: Option<f64>,
    pub rx_mbps: Option<f64>,
    pub udp_loss: Option<f64>,
    pub ping_loss: Option<f64>,
    pub ping_min: Option<f64>,
    pub ping_avg: Option<f64>,
    pub ping_max: Option<f64>,
    /// 主控端截图路径
    pub screenshot_master: String,
    /// 辅测端截图路径
    pub screenshot_agent: String,
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
}

pub(super) struct UnitGroup<'a> {
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
        // 单元序号有 `sort_key.0` 和 `unit_seq` 两处表示，取类型化的那个。
        format!("unit-{}", row.unit_seq)
    }
}

pub(super) fn group_rows(rows: &[Row]) -> Vec<UnitGroup<'_>> {
    let mut groups: Vec<UnitGroup<'_>> = Vec::new();
    for row in rows {
        let key = row_unit_key(row);
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
            src: report_endpoint(&self.src_pc, &self.src_iface, &self.src_ip),
            dst: report_endpoint(&self.dst_pc, &self.dst_iface, &self.dst_ip),
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
            ping_loss: self.ping_loss,
            ping_min: self.ping_min,
            ping_avg: self.ping_avg,
            ping_max: self.ping_max,
            screenshot_master: self.screenshot_master.clone(),
            screenshot_agent: self.screenshot_agent.clone(),
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
        let tag = infer_direction_tag(row);
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
    if target.target_mbps.is_none() {
        target.target_mbps = fallback.target_mbps;
    }
    if target.sample_coverage.is_none() {
        target.sample_coverage = fallback.sample_coverage;
    }
    if target.udp_loss.is_none() {
        target.udp_loss = fallback.udp_loss;
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
/// HTML 与 Excel 都展示这个诊断值，判定仍然逐方向进行；把合计放在模型层，
/// 避免两个结果出口各自挑 AB/BA 或各自处理缺失值。
pub(super) fn bidirectional_rx_average_sum(group: &UnitGroup<'_>) -> Option<f64> {
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
