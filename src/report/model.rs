//! 报告的数据模型：行、方向汇总、单元分组。
//!
//! 报告拿到的是一串平铺的 [`Row`]，而人读的是「一个测试单元里有哪几个方向、
//! 每个方向什么结论」。这一层负责的就是这个还原：分组、配对、补齐缺失字段。
//! 它不产出任何 HTML——渲染在 `html` 里，判定在 [`crate::verdict`] 里。

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamCounts {
    pub requested: usize,
    pub active: usize,
    pub required: usize,
}

#[derive(Debug, Clone, Default)]
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

#[derive(Debug, Clone, Default)]
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
    /// 独立落盘的 OS 网卡累计计数器逐样本 CSV。
    pub nic_samples: String,
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
        format!("unit-{}", row.sort_key.0)
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
    row.ping_loss.is_some()
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

pub(super) fn direction_from_row(row: &Row) -> DirectionSummary {
    DirectionSummary {
        tag: infer_direction_tag(row),
        src: report_endpoint(&row.src_pc, &row.src_iface, &row.src_ip),
        dst: report_endpoint(&row.dst_pc, &row.dst_iface, &row.dst_ip),
        verdict: row.verdict,
        reason_code: row.reason_code,
        reason_detail: row.reason_detail.clone(),
        reason: if row.reason_code.is_empty() && row.reason_detail.is_empty() {
            String::new()
        } else {
            report_reason(row.reason_code, &row.reason_detail)
        },
        streams: stream_counts(row),
        rx_avg: row.rx_avg,
        rx_p10: row.rx_p10,
        target_mbps: row.target_mbps,
        sample_coverage: row.sample_coverage,
        udp_loss: row.udp_loss,
        ping_loss: row.ping_loss,
        ping_min: row.ping_min,
        ping_avg: row.ping_avg,
        ping_max: row.ping_max,
        screenshot_master: row.screenshot_master.clone(),
        screenshot_agent: row.screenshot_agent.clone(),
    }
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

pub(super) fn group_is_udp(group: &UnitGroup<'_>) -> bool {
    group_title(group).to_ascii_uppercase().contains("UDP")
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
