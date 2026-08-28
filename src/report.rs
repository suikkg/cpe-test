//! HTML 测试报告生成（单文件、内嵌样式、含原始输出，拷走整个目录即可查看）

use crate::verdict::{aggregate_verdict, disposition_advice};
pub use crate::verdict::{ExecutionStatus, Verdict};
use std::path::Path;

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
    pub reason_code: String,
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
    pub reason_code: String,
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

fn screenshot_link(path: &str, label: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        let path = esc(path);
        let label = esc(label);
        format!(
            "<figure class=\"shot\"><a href=\"{path}\" target=\"_blank\" rel=\"noopener\" title=\"查看{label}\" aria-label=\"打开{label}原图\"><img src=\"{path}\" alt=\"{label}缩略图\" loading=\"lazy\" decoding=\"async\"><span>{label} · 查看原图</span></a></figure>"
        )
    }
}

/// 结果标签在窄列里必须能换行，否则最长的 NOT_EVALUATED 会顶到相邻列上去。
/// 在下划线后插入 `<wbr>`，让浏览器优先断在 `NOT_` / `EVALUATED` 这种语义边界，
/// 而不是 `overflow-wrap: anywhere` 那样断成 `NOT_EVALUAT` / `ED`。
fn status_label_html(verdict: Verdict) -> String {
    // 标签都是 ASCII 常量，不含需要转义的字符。
    verdict.label().replace('_', "_<wbr>")
}

/// 概览用的紧凑截图缩略图：与接收速率同一行，不必展开诊断面板。
fn overview_shot(path: &str, label: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let path = esc(path);
    let label = esc(label);
    format!(
        "<a class=\"shot-mini\" href=\"{path}\" target=\"_blank\" rel=\"noopener\" title=\"查看{label}原图\" aria-label=\"打开{label}原图\"><img src=\"{path}\" alt=\"{label}缩略图\" loading=\"lazy\" decoding=\"async\"><span>{label}</span></a>"
    )
}

fn overview_shot_cell(master: &str, agent: &str) -> String {
    let shots = [overview_shot(master, "主控"), overview_shot(agent, "辅测")]
        .into_iter()
        .filter(|shot| !shot.is_empty())
        .collect::<Vec<_>>();
    if shots.is_empty() {
        NOT_COLLECTED.to_string()
    } else {
        format!("<div class=\"shot-cell\">{}</div>", shots.join(""))
    }
}

fn artifact_link(path: &str, label: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("<a href=\"{}\">{}</a>", esc(path), esc(label))
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const NOT_APPLICABLE: &str = "—（不适用）";
/// 网卡计数器是按接口（即按方向）采的，拆不到单条流上：一个方向无论跑 1 条还是
/// 20 条流，都只有一个网卡 RX 序列，只挂在该方向的组合计行。流明细行的网卡列
/// 因此永远是空的——但空着不解释会被读成「这条流没测到」，必须写明去哪看。
const NIC_ON_GROUPTOTAL: &str = "—（按方向统计，见组合计行）";
const NOT_COLLECTED: &str = "未采集";
const INSUFFICIENT_SAMPLES: &str = "样本不足";

struct UnitGroup<'a> {
    key: String,
    summary: Option<&'a Row>,
    details: Vec<&'a Row>,
}

fn row_unit_key(row: &Row) -> String {
    if !row.parent_id.is_empty() {
        row.parent_id.clone()
    } else if row.is_unit_summary && !row.task_id.is_empty() {
        row.task_id.clone()
    } else {
        format!("unit-{}", row.sort_key.0)
    }
}

fn group_rows(rows: &[Row]) -> Vec<UnitGroup<'_>> {
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

fn group_verdict(group: &UnitGroup<'_>) -> Verdict {
    // 有单元汇总行时直接采信 executor 的聚合结果；没有（旧报告数据、被中断的
    // 运行）时用同一个 aggregate_verdict 复算，绝不在这里另写一套优先级。
    group.summary.map(|row| row.verdict).unwrap_or_else(|| {
        aggregate_verdict(
            group
                .details
                .iter()
                .map(|row| (row.verdict, row.reason_code.as_str())),
        )
    })
}

fn group_execution_status(group: &UnitGroup<'_>) -> ExecutionStatus {
    group
        .summary
        .map(|row| row.execution_status)
        .or_else(|| group.details.last().map(|row| row.execution_status))
        .unwrap_or_default()
}

fn unit_open_by_default(verdict: Verdict) -> bool {
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
fn group_seq(group: &UnitGroup<'_>) -> usize {
    group
        .summary
        .map(|row| row.sort_key.0)
        .or_else(|| group.details.first().map(|row| row.sort_key.0))
        .unwrap_or(0)
        .saturating_add(1)
}

fn group_title<'a>(group: &'a UnitGroup<'_>) -> &'a str {
    group
        .summary
        .map(|row| row.task.as_str())
        .or_else(|| group.details.first().map(|row| row.task.as_str()))
        .unwrap_or("未命名测试单元")
}

pub fn report_reason(code: &str, detail: &str) -> String {
    match (code.is_empty(), detail.is_empty()) {
        (false, false) => format!("{code}: {detail}"),
        (false, true) => code.to_string(),
        (true, false) => detail.to_string(),
        (true, true) => NOT_APPLICABLE.to_string(),
    }
}

pub fn report_endpoint(pc: &str, iface: &str, ip: &str) -> String {
    let mut identity = [pc, iface]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    if !ip.is_empty() {
        if !identity.is_empty() {
            identity.push(' ');
        }
        identity.push('(');
        identity.push_str(ip);
        identity.push(')');
    }
    if identity.is_empty() {
        NOT_APPLICABLE.to_string()
    } else {
        identity
    }
}

fn infer_direction_tag(row: &Row) -> String {
    let label = row.kind_label.to_ascii_lowercase();
    if label.contains("-ab") {
        "AB".into()
    } else if label.contains("-ba") {
        "BA".into()
    } else {
        "单向".into()
    }
}

fn normalized_direction_tag(tag: &str) -> String {
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

fn row_is_ping(row: &Row) -> bool {
    row.ping_loss.is_some()
        || row.ping_min.is_some()
        || row.ping_avg.is_some()
        || row.ping_max.is_some()
        || row.kind_label.to_ascii_uppercase().contains("PING")
        || row.task.to_ascii_uppercase().contains("PING")
}

fn group_is_ping(group: &UnitGroup<'_>) -> bool {
    group.summary.is_some_and(row_is_ping) || group.details.iter().any(|row| row_is_ping(row))
}

fn stream_counts(row: &Row) -> Option<StreamCounts> {
    (row.requested_streams > 0 || row.active_streams > 0 || row.required_streams > 0).then_some(
        StreamCounts {
            requested: row.requested_streams,
            active: row.active_streams,
            required: row.required_streams,
        },
    )
}

fn direction_from_row(row: &Row) -> DirectionSummary {
    DirectionSummary {
        tag: infer_direction_tag(row),
        src: report_endpoint(&row.src_pc, &row.src_iface, &row.src_ip),
        dst: report_endpoint(&row.dst_pc, &row.dst_iface, &row.dst_ip),
        verdict: row.verdict,
        reason_code: row.reason_code.clone(),
        reason_detail: row.reason_detail.clone(),
        reason: if row.reason_code.is_empty() && row.reason_detail.is_empty() {
            String::new()
        } else {
            report_reason(&row.reason_code, &row.reason_detail)
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

fn direction_row_score(row: &Row) -> u8 {
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

fn fallback_direction_summaries(group: &UnitGroup<'_>) -> Vec<DirectionSummary> {
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

fn merge_missing_direction_fields(target: &mut DirectionSummary, fallback: &DirectionSummary) {
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

fn group_direction_summaries(group: &UnitGroup<'_>) -> Vec<DirectionSummary> {
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

fn rx_avg_text(value: Option<f64>, is_ping: bool) -> String {
    value.map_or_else(
        || {
            if is_ping {
                NOT_APPLICABLE.into()
            } else {
                NOT_COLLECTED.into()
            }
        },
        |value| format!("{value:.3} Mbps"),
    )
}

fn rx_p10_text(value: Option<f64>, rx_avg: Option<f64>, is_ping: bool) -> String {
    value.map_or_else(
        || {
            if is_ping {
                NOT_APPLICABLE.into()
            } else if rx_avg.is_some() {
                INSUFFICIENT_SAMPLES.into()
            } else {
                NOT_COLLECTED.into()
            }
        },
        |value| format!("{value:.3} Mbps"),
    )
}

fn target_text(value: Option<f64>) -> String {
    value.map_or_else(|| NOT_APPLICABLE.into(), |value| format!("{value:.3} Mbps"))
}

fn coverage_text(value: Option<f64>, is_ping: bool) -> String {
    value.map_or_else(
        || {
            if is_ping {
                NOT_APPLICABLE.into()
            } else {
                NOT_COLLECTED.into()
            }
        },
        |value| format!("{:.1}%", value * 100.0),
    )
}

/// 流量工具（iperf3 / ctsTraffic）自报的发送/接收速率。
///
/// 它不是正式判定口径——正式口径永远是接收端 OS 网卡 RX——但它是「这条流确实
/// 建立了」的唯一证据。单条流明细行本来就没有网卡数据（网卡计数器是按方向采的，
/// 只挂在组合计行上），把它藏进折叠的诊断面板，流明细整行就只剩「未采集」。
fn tool_rate_text(tx_mbps: Option<f64>, rx_mbps: Option<f64>) -> String {
    match (tx_mbps, rx_mbps) {
        (None, None) => NOT_COLLECTED.into(),
        (tx, rx) => {
            let fmt = |value: Option<f64>| {
                value.map_or_else(|| NOT_COLLECTED.to_string(), |value| format!("{value:.3}"))
            };
            format!("发 {} / 收 {} Mbps", fmt(tx), fmt(rx))
        }
    }
}

fn streams_text(value: Option<StreamCounts>) -> String {
    value.map_or_else(
        || NOT_APPLICABLE.into(),
        |counts| format!("{}/{}/{}", counts.requested, counts.active, counts.required),
    )
}

fn quality_text(
    udp_loss: Option<f64>,
    ping_loss: Option<f64>,
    ping_min: Option<f64>,
    ping_avg: Option<f64>,
    ping_max: Option<f64>,
    is_ping: bool,
) -> String {
    if let Some(loss) = udp_loss {
        return format!("UDP 丢包 {loss:.3}%");
    }
    if is_ping {
        let mut parts = Vec::new();
        if let Some(loss) = ping_loss {
            parts.push(format!("丢包率 {loss:.1}%"));
        }
        match (ping_min, ping_avg, ping_max) {
            (Some(min), Some(avg), Some(max)) => {
                parts.push(format!("RTT 最小/平均/最大 {min:.3}/{avg:.3}/{max:.3} ms"))
            }
            (min, avg, max) => {
                if let Some(min) = min {
                    parts.push(format!("RTT 最小 {min:.3} ms"));
                }
                if let Some(avg) = avg {
                    parts.push(format!("RTT 平均 {avg:.3} ms"));
                }
                if let Some(max) = max {
                    parts.push(format!("RTT 最大 {max:.3} ms"));
                }
            }
        }
        return if parts.is_empty() {
            NOT_COLLECTED.into()
        } else {
            parts.join(" · ")
        };
    }
    NOT_APPLICABLE.into()
}

fn traffic_pass_reason(direction: &DirectionSummary) -> String {
    match (direction.rx_avg, direction.rx_p10, direction.target_mbps) {
        (Some(avg), Some(p10), Some(target)) if avg >= target && p10 >= target => format!(
            "RX_TARGET_MET: RX 平均 {avg:.3} Mbps、RX-P10 {p10:.3} Mbps 均不低于目标 {target:.3} Mbps"
        ),
        (Some(avg), Some(p10), Some(target)) => format!(
            "判定结果与展示指标不一致: PASS；RX 平均/P10/目标为 {avg:.3}/{p10:.3}/{target:.3} Mbps"
        ),
        (Some(avg), None, Some(target)) if avg >= target => format!(
            "RX_TARGET_MET: RX 平均 {avg:.3} Mbps 不低于目标 {target:.3} Mbps；RX-P10 未采集"
        ),
        (Some(avg), None, Some(target)) => format!(
            "判定结果与展示指标不一致: PASS；RX 平均 {avg:.3} Mbps 低于目标 {target:.3} Mbps"
        ),
        _ => "PASS: 方向执行完成".into(),
    }
}

fn direction_reason_text(direction: &DirectionSummary, fallback: &str) -> String {
    let explicit = if !direction.reason_code.is_empty() || !direction.reason_detail.is_empty() {
        report_reason(&direction.reason_code, &direction.reason_detail)
    } else if !direction.reason.is_empty() {
        direction.reason.clone()
    } else {
        String::new()
    };
    let missing = explicit.is_empty() || explicit == NOT_APPLICABLE;
    let reason = if missing && direction.verdict == Verdict::Pass {
        if direction.ping_loss.is_some()
            || direction.ping_min.is_some()
            || direction.ping_avg.is_some()
            || direction.ping_max.is_some()
        {
            ping_pass_reason(
                direction.ping_loss,
                direction.ping_min,
                direction.ping_avg,
                direction.ping_max,
            )
        } else {
            traffic_pass_reason(direction)
        }
    } else if missing {
        fallback.to_string()
    } else {
        explicit
    };
    validate_rate_reason(
        &reason,
        direction.rx_avg,
        direction.rx_p10,
        direction.target_mbps,
    )
}

fn reason_code(reason: &str) -> &str {
    reason
        .split_once(':')
        .map_or(reason, |(code, _)| code)
        .trim()
}

fn metric_reason_mismatch(code: &str, observed: &str) -> String {
    format!("判定原因与展示指标不一致: {code}；{observed}")
}

fn validate_rate_reason(
    reason: &str,
    rx_avg: Option<f64>,
    rx_p10: Option<f64>,
    target_mbps: Option<f64>,
) -> String {
    let code = reason_code(reason);
    match code {
        "RX_BELOW_TARGET" => match (rx_avg, target_mbps) {
            (Some(rx), Some(target)) if rx < target => format!(
                "RX_BELOW_TARGET: RX 平均 {rx:.3} Mbps < 目标 {target:.3} Mbps"
            ),
            (Some(rx), Some(target)) => metric_reason_mismatch(
                code,
                &format!("RX 平均 {rx:.3} Mbps >= 目标 {target:.3} Mbps"),
            ),
            _ => metric_reason_mismatch(code, "缺少 RX 平均或目标，无法核对该判定"),
        },
        "RX_P10_BELOW_TARGET" => match (rx_p10, target_mbps) {
            (Some(rx_p10), Some(target)) if rx_p10 < target => format!(
                "RX_P10_BELOW_TARGET: RX-P10 {rx_p10:.3} Mbps < 目标 {target:.3} Mbps"
            ),
            (Some(rx_p10), Some(target)) => metric_reason_mismatch(
                code,
                &format!("RX-P10 {rx_p10:.3} Mbps >= 目标 {target:.3} Mbps"),
            ),
            _ => metric_reason_mismatch(code, "缺少 RX-P10 或目标，无法核对该判定"),
        },
        // RX_DROPOUT 的成立条件是「平均达标 **且** P10 达标，但有完整 5 秒窗口掉下去」
        // （见 rate_window::rate_verdict）。掉坑本身靠原因文本里的窗口统计说明，
        // 这里核对的是那两个前提——它们要是不成立，说明判定和展示的指标对不上。
        "RX_DROPOUT" => match (rx_avg, rx_p10, target_mbps) {
            (Some(rx), Some(rx_p10), Some(target)) if rx >= target && rx_p10 >= target => {
                reason.to_string()
            }
            (Some(rx), Some(rx_p10), Some(target)) => metric_reason_mismatch(
                code,
                &format!(
                    "要求 RX 平均与 RX-P10 都 >= 目标；当前为 {rx:.3}/{rx_p10:.3}/{target:.3} Mbps"
                ),
            ),
            _ => metric_reason_mismatch(code, "缺少 RX 平均、RX-P10 或目标，无法核对该判定"),
        },
        "RX_UNSTABLE" => match (rx_avg, rx_p10, target_mbps) {
            (Some(rx), Some(rx_p10), Some(target)) if rx >= target && rx_p10 < target => format!(
                "RX_UNSTABLE: RX 平均 {rx:.3} Mbps >= 目标 {target:.3} Mbps，RX-P10 {rx_p10:.3} Mbps < 目标 {target:.3} Mbps"
            ),
            (Some(rx), Some(rx_p10), Some(target)) => metric_reason_mismatch(
                code,
                &format!(
                    "要求 RX 平均 >= 目标且 RX-P10 < 目标；当前为 {rx:.3}/{rx_p10:.3}/{target:.3} Mbps"
                ),
            ),
            _ => metric_reason_mismatch(
                code,
                "缺少 RX 平均、RX-P10 或目标，无法核对该判定",
            ),
        },
        _ => reason.to_string(),
    }
}

fn ping_pass_reason(
    ping_loss: Option<f64>,
    ping_min: Option<f64>,
    ping_avg: Option<f64>,
    ping_max: Option<f64>,
) -> String {
    let quality = quality_text(None, ping_loss, ping_min, ping_avg, ping_max, true);
    if quality == NOT_COLLECTED {
        "PING_OK: Ping 执行完成".into()
    } else {
        format!("PING_OK: {quality}")
    }
}

fn group_reason(group: &UnitGroup<'_>) -> String {
    let reason = group
        .summary
        .filter(|row| !row.reason_code.is_empty() || !row.reason_detail.is_empty())
        .map(|row| report_reason(&row.reason_code, &row.reason_detail))
        .or_else(|| {
            group
                .details
                .iter()
                .find(|row| !row.reason_code.is_empty() || !row.reason_detail.is_empty())
                .map(|row| report_reason(&row.reason_code, &row.reason_detail))
        });
    reason.unwrap_or_else(|| NOT_APPLICABLE.into())
}

fn push_overview(h: &mut String, groups: &[UnitGroup<'_>]) {
    // 先把方向汇总物化一遍：既用来决定是否需要「截图」列，也避免在渲染循环里
    // 重复推导。整表宽度按“常见 1440 宽屏不横向滚动”来配，判定原因单独占一行。
    let rendered: Vec<(&UnitGroup<'_>, Verdict, Vec<DirectionSummary>)> = groups
        .iter()
        .map(|group| {
            let unit_verdict = group_verdict(group);
            let mut directions = group_direction_summaries(group);
            if directions.is_empty() {
                directions.push(DirectionSummary {
                    tag: NOT_APPLICABLE.into(),
                    verdict: unit_verdict,
                    ..Default::default()
                });
            }
            (group, unit_verdict, directions)
        })
        .collect();
    // 截图默认开启，但关掉截图或纯 Ping 报告里整列都是「未采集」；那种情况下
    // 这一列只会白占约 190px，直接不渲染。
    let has_shots = rendered.iter().any(|(_, _, directions)| {
        directions.iter().any(|direction| {
            !direction.screenshot_master.is_empty() || !direction.screenshot_agent.is_empty()
        })
    });
    let column_count = if has_shots { 12 } else { 11 };

    h.push_str(
        "<section class=\"overview-section\" aria-labelledby=\"overview-heading\"><h2 id=\"overview-heading\">测试概览</h2>\n",
    );
    h.push_str(
        "<div class=\"overview-scroll\" role=\"region\" aria-labelledby=\"overview-heading\" tabindex=\"0\"><table class=\"overview-table\"><caption class=\"sr-only\">按测试单元和方向展示接收端网卡 RX 判定指标、截图与判定原因</caption><colgroup><col class=\"c-seq\"><col class=\"c-verdict\"><col class=\"c-unit\"><col class=\"c-dir\"><col class=\"c-endpoints\"><col class=\"c-streams\"><col class=\"c-rate\"><col class=\"c-rate\"><col class=\"c-target\">",
    );
    if has_shots {
        h.push_str("<col class=\"c-shot\">");
    }
    h.push_str(
        "<col class=\"c-coverage\"><col class=\"c-quality\"></colgroup><thead><tr><th scope=\"col\">#</th><th scope=\"col\">结果</th><th scope=\"col\">测试单元</th><th scope=\"col\">方向</th><th scope=\"col\">源端 → 接收端</th><th scope=\"col\">请求/活跃/要求流</th><th scope=\"col\">接收端 RX 平均</th><th scope=\"col\">接收端 RX-P10</th><th scope=\"col\">目标</th>",
    );
    if has_shots {
        h.push_str("<th scope=\"col\">截图</th>");
    }
    h.push_str(
        "<th scope=\"col\">采样覆盖率</th><th scope=\"col\">质量指标</th></tr></thead><tbody>\n",
    );

    for (group, unit_verdict, directions) in &rendered {
        let unit_verdict = *unit_verdict;
        let execution_status = group_execution_status(group);
        let is_ping = group_is_ping(group);
        let unit_reason = group_reason(group);
        for (index, direction) in directions.iter().enumerate() {
            let tag = normalized_direction_tag(&direction.tag);
            let src = if direction.src.is_empty() {
                NOT_APPLICABLE
            } else {
                &direction.src
            };
            let dst = if direction.dst.is_empty() {
                NOT_APPLICABLE
            } else {
                &direction.dst
            };
            let endpoints = format!("{src} → {dst}");
            let mut reason = direction_reason_text(direction, &unit_reason);
            if is_ping
                && direction.verdict == Verdict::Pass
                && (reason.is_empty() || reason == NOT_APPLICABLE)
            {
                reason = ping_pass_reason(
                    direction.ping_loss,
                    direction.ping_min,
                    direction.ping_avg,
                    direction.ping_max,
                );
            }
            // 单元判定只写在该单元的第一行：概览首列是方向判定，双向单元里
            // 一个 PASS 方向很容易被当成整个测试项通过。冻结的「测试单元」列
            // 里始终带上单元级结论，才是记录时真正要抄的那一个。
            let unit_cell = if index == 0 {
                format!(
                    "{}<br><small class=\"unit-verdict-note\">单元判定 <strong class=\"status {}\">{}</strong></small>",
                    esc(group_title(group)),
                    unit_verdict.css(),
                    status_label_html(unit_verdict),
                )
            } else {
                esc(group_title(group))
            };
            let shot_cell = if has_shots {
                format!(
                    "<td class=\"shot-col\">{}</td>",
                    overview_shot_cell(&direction.screenshot_master, &direction.screenshot_agent)
                )
            } else {
                String::new()
            };
            h.push_str(&format!(
                "<tr class=\"{}\" data-unit-id=\"{}\" data-direction=\"{}\"><td class=\"num seq-col\">{}</td><td><strong class=\"status {}\">{}</strong><br><small>{}</small></td><td>{}</td><td><strong>{}</strong></td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>{}<td class=\"num\">{}</td><td>{}</td></tr>\n",
                if index == 0 { "unit-first" } else { "unit-cont" },
                esc(&group.key),
                esc(&tag),
                if index == 0 {
                    group_seq(group).to_string()
                } else {
                    String::new()
                },
                direction.verdict.css(),
                status_label_html(direction.verdict),
                execution_status.label(),
                unit_cell,
                esc(&tag),
                esc(&endpoints),
                esc(&streams_text(direction.streams)),
                esc(&rx_avg_text(direction.rx_avg, is_ping)),
                esc(&rx_p10_text(direction.rx_p10, direction.rx_avg, is_ping)),
                esc(&target_text(direction.target_mbps)),
                shot_cell,
                esc(&coverage_text(direction.sample_coverage, is_ping)),
                esc(&quality_text(
                    direction.udp_loss,
                    direction.ping_loss,
                    direction.ping_min,
                    direction.ping_avg,
                    direction.ping_max,
                    is_ping,
                )),
            ));
            // 判定原因是最长的一段文字。放进定宽列里只会被截断，所以让它独占
            // 整行宽度紧跟在指标行下面——扫指标、看原因，不用左右拖。
            if !reason.is_empty() && reason != NOT_APPLICABLE {
                // 处置建议是正文（用户要的是"下一步干什么"），原因码降级为小字
                // （开发者要的是精确定位）。两者各得其位，谁都不必迁就谁。
                let advice = disposition_advice(reason_code(&reason))
                    .map(|advice| format!("<span class=\"advice\">{}</span>", esc(advice)))
                    .unwrap_or_default();
                h.push_str(&format!(
                    "<tr class=\"reason-row\" data-unit-id=\"{}\" data-direction=\"{}\"><td colspan=\"{column_count}\"><span class=\"reason-tag\">{} 判定</span>{}<span class=\"reason-code-detail\">{}</span></td></tr>\n",
                    esc(&group.key),
                    esc(&tag),
                    esc(&tag),
                    advice,
                    esc(&reason),
                ));
            }
        }
    }
    h.push_str("</tbody></table></div></section>\n");
}

fn diagnostic_metric(value: Option<f64>, is_ping: bool) -> String {
    value.map_or_else(
        || {
            if is_ping {
                NOT_APPLICABLE.into()
            } else {
                NOT_COLLECTED.into()
            }
        },
        |value| format!("{value:.3} Mbps"),
    )
}

/// 从实际执行的命令里估算「被 socket 缓冲吃掉、未必上线」的字节数。
///
/// iperf3 的 `-w` 是 socket 缓冲，塞进去的字节会被算进 sender 汇总，但可能
/// 一个都没上线。run_20260825_215915_7684 的 65 条 TCP 记录里，
/// 「发 − 收」稳定在 118.92 ± 1.90 Mbps，而 `-w 256m × 10 流 ÷ 180s`
/// 正好是 119.3Mbps——那个差值整个就是缓冲。
///
/// 把这个数印出来，是为了让读报告的人能自己核对「发送」列虚高了多少，
/// 而不是对着一个对不上的数字猜。判定口径一直是接收端网卡，不受影响。
fn in_flight_buffer_estimate(row: &Row) -> Option<String> {
    let mut args = row.command.split_whitespace();
    let mut window: Option<&str> = None;
    let mut streams: u32 = 1;
    while let Some(arg) = args.next() {
        match arg {
            "-w" => window = args.next(),
            "-P" => streams = args.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            _ => {}
        }
    }
    let bytes = parse_iperf_size(window?)? * u64::from(streams.max(1));
    let secs = row.required_seconds.filter(|v| *v > 0.0)?;
    Some(format!(
        "{:.2} GB（-w × {streams} 流）≈ {:.0} Mbps 计入「发送」但未必上线",
        bytes as f64 / 1e9,
        bytes as f64 * 8.0 / 1e6 / secs
    ))
}

/// 解析 `-w` 尺寸后缀。
///
/// 复用下发命令时用的那一个解析器，而不是在这里另写一遍：报告里这行估算
/// 必须和实际下发的 socket buffer 说的是同一个数。自带一份简化版会在
/// `2.5m`、`4mb` 这类合法写法上解析失败——校验放行、命令照发，只有报告里
/// 的估算无声消失。
fn parse_iperf_size(value: &str) -> Option<u64> {
    crate::cmd::ctstraffic::parse_size_bytes(value)
        .ok()
        .map(u64::from)
}

fn diagnostic_item(h: &mut String, label: &str, value: &str) {
    h.push_str(&format!("<dt>{}</dt><dd>{}</dd>", esc(label), esc(value)));
}

/// 内嵌进报告的单段原始输出上限（字符）。
///
/// 一条 `-P 8 -i 1 -t 180` 的 iperf3 流会打出一千多行；一次 120 单元的运行
/// 把每条流的 client + server 输出全文内嵌，单个 HTML 能涨到几十 MB。
/// 而**同一份文本已经作为 `raw_log` 单独落盘**并在上面给了链接——
/// 内嵌这一份是给「点开就看」用的，不是存档。
const EMBEDDED_RAW_MAX_CHARS: usize = 20_000;

/// 超长的原始输出取头尾两段。
///
/// 掐头去尾而不是只留开头：iperf3 和 ctsTraffic 的**汇总行在最后**，
/// 而那正是最常要看的一段；只留开头等于把结论截掉。
fn embedded_raw(text: &str) -> String {
    let total = text.chars().count();
    if total <= EMBEDDED_RAW_MAX_CHARS {
        return text.to_string();
    }
    let keep = EMBEDDED_RAW_MAX_CHARS / 2;
    let head: String = text.chars().take(keep).collect();
    let tail: String = text.chars().skip(total - keep).collect();
    format!(
        "{head}\n\n……（中间省略 {} 个字符；完整内容见上方「独立原始记录」链接）……\n\n{tail}",
        total - keep * 2
    )
}

fn raw_anchor(row: &Row) -> String {
    format!(
        "raw-{}-{}-{}-{}",
        row.sort_key.0, row.sort_key.1, row.sort_key.2, row.sort_key.3
    )
}

fn nonempty_raw_count(row: &Row) -> usize {
    row.raws
        .iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .count()
}

fn diagnostic_availability(row: &Row) -> String {
    let mut available: Vec<String> = Vec::new();
    if !row.screenshot_master.is_empty() {
        available.push("主控截图".into());
    }
    if !row.screenshot_agent.is_empty() {
        available.push("辅测截图".into());
    }
    if !row.command.is_empty() {
        available.push("灌包命令".into());
    }
    if !row.raw_log.is_empty() {
        available.push("原始记录".into());
    }
    if !row.nic_samples.is_empty() {
        available.push("网卡样本".into());
    }
    if !row.raws.is_empty() {
        available.push(format!(
            "内嵌原始输出（非空 {}/{}）",
            nonempty_raw_count(row),
            row.raws.len()
        ));
    }
    if available.is_empty() {
        "指标详情".into()
    } else {
        available.join(" · ")
    }
}

fn push_row_diagnostics(h: &mut String, row: &Row, is_ping: bool, aria_context: &str) {
    h.push_str(&format!(
        "<details class=\"row-diagnostics\"><summary aria-label=\"{}的诊断详情\"><span>诊断</span><small class=\"diagnostic-availability\">{}</small></summary><div class=\"diagnostic-panel\"><dl class=\"diagnostic-grid\">",
        esc(aria_context),
        esc(&diagnostic_availability(row)),
    ));
    diagnostic_item(
        h,
        "Task ID",
        if row.task_id.is_empty() {
            NOT_APPLICABLE
        } else {
            &row.task_id
        },
    );
    diagnostic_item(
        h,
        "Parent ID",
        if row.parent_id.is_empty() {
            NOT_APPLICABLE
        } else {
            &row.parent_id
        },
    );
    diagnostic_item(
        h,
        "源端",
        &report_endpoint(&row.src_pc, &row.src_iface, &row.src_ip),
    );
    diagnostic_item(
        h,
        "接收端",
        &report_endpoint(&row.dst_pc, &row.dst_iface, &row.dst_ip),
    );
    diagnostic_item(h, "源网卡 TX 平均", &diagnostic_metric(row.tx_avg, is_ping));
    diagnostic_item(h, "源网卡 TX-P10", &diagnostic_metric(row.tx_p10, is_ping));
    diagnostic_item(
        h,
        "网卡 RX 中位数",
        &diagnostic_metric(row.rx_median, is_ping),
    );
    diagnostic_item(h, "网卡 RX-P95", &diagnostic_metric(row.rx_p95, is_ping));
    let rx_range = match (row.rx_min, row.rx_max) {
        (Some(min), Some(max)) => format!("{min:.3}–{max:.3} Mbps"),
        (None, None) if is_ping => NOT_APPLICABLE.into(),
        _ => NOT_COLLECTED.into(),
    };
    diagnostic_item(h, "网卡 RX 范围", &rx_range);
    // 流量工具自报速率已提升为表内独立列，这里不再重复。
    diagnostic_item(
        h,
        "对向网卡 RX 平均",
        if row.peer_rx.is_empty() {
            NOT_APPLICABLE
        } else {
            &row.peer_rx
        },
    );
    diagnostic_item(h, "重试次数", &row.retry_count.to_string());
    let window = match (row.effective_seconds, row.required_seconds) {
        (Some(effective), Some(required)) => format!("{effective:.1}/{required:.1} 秒"),
        (None, None) if is_ping => NOT_APPLICABLE.into(),
        _ => NOT_COLLECTED.into(),
    };
    diagnostic_item(h, "有效/要求时长", &window);
    // 判定窗口的两个端点相对该单元 epoch，可直接对到网卡逐样本 CSV 的 elapsed_ms 列。
    let window_span = match (row.window_start_ms, row.window_end_ms) {
        (Some(start), Some(end)) => format!("{start} ms – {end} ms（对应网卡样本 elapsed_ms）"),
        _ if is_ping => NOT_APPLICABLE.into(),
        _ => NOT_COLLECTED.into(),
    };
    diagnostic_item(h, "判定窗口区间", &window_span);
    if let Some(estimate) = in_flight_buffer_estimate(row) {
        diagnostic_item(h, "估算在途缓冲", &estimate);
    }
    diagnostic_item(
        h,
        "已扣除背景速率",
        &row.baseline_mbps.map_or_else(
            || {
                if is_ping {
                    NOT_APPLICABLE.to_string()
                } else {
                    NOT_COLLECTED.to_string()
                }
            },
            |value| format!("{value:.3} Mbps（起流前空闲期中位数）"),
        ),
    );
    diagnostic_item(
        h,
        "5 秒滚动窗口覆盖率",
        &row.rolling_coverage.map_or_else(
            || {
                if is_ping {
                    NOT_APPLICABLE.to_string()
                } else {
                    NOT_COLLECTED.to_string()
                }
            },
            |value| format!("{:.1}%", value * 100.0),
        ),
    );
    h.push_str("</dl>");

    let artifacts = [
        artifact_link(&row.raw_log, "独立原始记录（raw_log）"),
        artifact_link(&row.nic_samples, "网卡逐样本 CSV"),
    ]
    .into_iter()
    .filter(|link| !link.is_empty())
    .collect::<Vec<_>>();
    if !artifacts.is_empty()
        || !row.screenshot_master.is_empty()
        || !row.screenshot_agent.is_empty()
    {
        h.push_str("<div class=\"artifact-list\">");
        for artifact in artifacts {
            h.push_str(&artifact);
        }
        h.push_str(&screenshot_link(&row.screenshot_master, "主控截图"));
        h.push_str(&screenshot_link(&row.screenshot_agent, "辅测截图"));
        if !row.raws.is_empty() {
            h.push_str(&format!(
                "<a href=\"#{}\">内嵌原始输出（非空 {}/{} 段）</a>",
                raw_anchor(row),
                nonempty_raw_count(row),
                row.raws.len()
            ));
        }
        h.push_str("</div>");
    } else if !row.raws.is_empty() {
        h.push_str(&format!(
            "<div class=\"artifact-list\"><a href=\"#{}\">内嵌原始输出（非空 {}/{} 段）</a></div>",
            raw_anchor(row),
            nonempty_raw_count(row),
            row.raws.len()
        ));
    }
    if !row.command.is_empty() {
        h.push_str(&format!(
            "<div class=\"command-block\"><strong>实际灌包命令</strong><code class=\"command\" title=\"{}\">{}</code></div>",
            esc(&row.command),
            esc(&row.command)
        ));
    }
    h.push_str("</div></details>");
}

/// 流明细行的网卡指标为空时，说明去向而不是留一个光秃秃的「未采集」。
/// 组合计行和 Ping 行有自己的真实取值/口径，不套用。
fn nic_cell(row: &Row, is_ping: bool, rendered: String) -> String {
    if !row.is_grouptotal && !is_ping && rendered == NOT_COLLECTED {
        NIC_ON_GROUPTOTAL.to_string()
    } else {
        rendered
    }
}

/// 明细行的「传输」显示文本。
///
/// `Row::transport` 是逻辑字段（`row_has_usable_traffic_measurement` 等按
/// `CTS/` 前缀分支），不能动；这里只改展示：把后端名摆出来。
///
/// 必要性：两个后端的 UDP 语义不等价——iperf3 用 `-b` 恒定速率发送，
/// ctsTraffic 用 MediaStream 模型（每秒 FrameRate 帧，每帧再拆成 datagram），
/// 突发形态和排队行为不同。报告把两者的结果放在同一个「接收端 RX 平均」列里，
/// 不写清来源就会被当成可直接互比的数。
fn transport_display(transport: &str) -> String {
    match transport {
        "" => String::new(),
        t if t.starts_with("CTS/") => {
            format!("ctsTraffic {}", t.trim_start_matches("CTS/"))
        }
        t => format!("iperf3 {t}"),
    }
}

/// 报告是否同时包含两种吞吐后端；只有同时出现时才值得提示口径差异。
fn report_mixes_traffic_backends(groups: &[UnitGroup<'_>]) -> bool {
    let mut iperf = false;
    let mut cts = false;
    for group in groups {
        for row in group.details.iter().copied().chain(group.summary) {
            if row.transport.is_empty() {
                continue;
            }
            if row.transport.starts_with("CTS/") {
                cts = true;
            } else {
                iperf = true;
            }
        }
    }
    iperf && cts
}

fn push_detail_row(h: &mut String, row: &Row, group_title: &str) {
    let is_ping = row_is_ping(row);
    let direction = infer_direction_tag(row);
    let kind = if row.kind_label.is_empty() {
        NOT_APPLICABLE
    } else {
        &row.kind_label
    };
    let protocol = if is_ping {
        if row.ip.is_empty() {
            "PING".to_string()
        } else {
            format!("PING / {}", row.ip)
        }
    } else if row.transport.is_empty() {
        NOT_APPLICABLE.to_string()
    } else if row.ip.is_empty() {
        transport_display(&row.transport)
    } else {
        format!("{} / {}", transport_display(&row.transport), row.ip)
    };
    let reason = direction_reason_text(&direction_from_row(row), NOT_APPLICABLE);
    let row_class = if row.is_grouptotal {
        " class=\"grouptotal\""
    } else {
        ""
    };
    h.push_str(&format!(
        "<tr{row_class} data-detail-row=\"true\"><td><span class=\"status {}\">{}</span></td><td>{}</td><td>{}</td><td><strong>{}</strong><br><small>{}</small></td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num tool-rate\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td><td>",
        row.verdict.css(),
        row.verdict.label(),
        esc(&row.time),
        esc(&direction),
        esc(kind),
        esc(&row.param),
        esc(&protocol),
        esc(&streams_text(stream_counts(row))),
        esc(&nic_cell(row, is_ping, rx_avg_text(row.rx_avg, is_ping))),
        esc(&nic_cell(
            row,
            is_ping,
            rx_p10_text(row.rx_p10, row.rx_avg, is_ping),
        )),
        esc(&tool_rate_text(row.tx_mbps, row.rx_mbps)),
        esc(&target_text(row.target_mbps)),
        esc(&nic_cell(
            row,
            is_ping,
            coverage_text(row.sample_coverage, is_ping),
        )),
        esc(&quality_text(
            row.udp_loss,
            row.ping_loss,
            row.ping_min,
            row.ping_avg,
            row.ping_max,
            is_ping,
        )),
    ));
    push_row_diagnostics(h, row, is_ping, &format!("{group_title} {direction}"));
    h.push_str("</td></tr>\n");
    // 与概览同一处理：判定原因是最长的一段文字，挤进定宽列会被压成一列一个字。
    if !reason.is_empty() && reason != NOT_APPLICABLE {
        h.push_str(&format!(
            "<tr class=\"reason-row\" data-reason-row=\"true\"><td colspan=\"13\"><span class=\"reason-tag\">{} 判定</span>{}</td></tr>\n",
            esc(&direction),
            esc(&reason),
        ));
    }
}

fn group_is_udp(group: &UnitGroup<'_>) -> bool {
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

fn group_is_bidirectional(group: &UnitGroup<'_>) -> bool {
    let directions = group_direction_summaries(group);
    let has_ab = directions
        .iter()
        .any(|direction| direction.tag.eq_ignore_ascii_case("AB"));
    let has_ba = directions
        .iter()
        .any(|direction| direction.tag.eq_ignore_ascii_case("BA"));
    (has_ab && has_ba) || group_title(group).contains("双向")
}

fn unit_execution_meta(group: &UnitGroup<'_>) -> String {
    if group_is_ping(group) {
        let count = if group.details.is_empty() {
            1
        } else {
            group.details.len()
        };
        return format!("{} 条 Ping 执行行", count);
    }
    if group_is_bidirectional(group) && group_is_udp(group) {
        let flow_count = group
            .details
            .iter()
            .filter(|row| !row.is_grouptotal)
            .count();
        let total_count = group.details.iter().filter(|row| row.is_grouptotal).count();
        return format!("2 个方向 · {flow_count} 条 UDP 流明细 · {total_count} 条方向组合计");
    }
    if group_is_bidirectional(group) {
        return "2 个方向执行行（AB / BA）".into();
    }
    if group_is_udp(group) {
        let detail_flow_count = group
            .details
            .iter()
            .filter(|row| !row.is_grouptotal)
            .count();
        let flow_count = if detail_flow_count > 0 {
            detail_flow_count
        } else {
            group
                .summary
                .map(|row| row.requested_streams.max(row.active_streams))
                .unwrap_or(0)
        };
        return format!("{flow_count} 条 UDP 流执行行");
    }
    format!("{} 条执行记录", group.details.len())
}

fn direction_endpoint_text(direction: &DirectionSummary) -> String {
    let src = if direction.src.is_empty() {
        NOT_APPLICABLE
    } else {
        &direction.src
    };
    let dst = if direction.dst.is_empty() {
        NOT_APPLICABLE
    } else {
        &direction.dst
    };
    format!("{src} → {dst}")
}

fn push_bidirectional_direction(
    h: &mut String,
    direction: &DirectionSummary,
    fallback_reason: &str,
) {
    let tag = normalized_direction_tag(&direction.tag);
    let reason = direction_reason_text(direction, fallback_reason);
    let shots = if direction.screenshot_master.is_empty() && direction.screenshot_agent.is_empty() {
        String::new()
    } else {
        format!(
            "<span class=\"direction-summary-shots\">{}</span>",
            overview_shot_cell(&direction.screenshot_master, &direction.screenshot_agent)
        )
    };
    h.push_str(&format!(
        "<div class=\"direction-summary-row\"><span class=\"direction-summary-tag\">{}</span><span class=\"direction-summary-endpoints\">{}</span><span>接收端 RX 平均 <strong>{}</strong></span><span>RX-P10 <strong>{}</strong></span><span>目标 <strong>{}</strong></span>{}<span class=\"direction-summary-reason\">判定：{}</span></div>",
        esc(&tag),
        esc(&direction_endpoint_text(direction)),
        esc(&rx_avg_text(direction.rx_avg, false)),
        esc(&rx_p10_text(direction.rx_p10, direction.rx_avg, false)),
        esc(&target_text(direction.target_mbps)),
        shots,
        esc(&reason),
    ));
}

fn push_bidirectional_summary(h: &mut String, group: &UnitGroup<'_>) {
    let directions = group_direction_summaries(group);
    let Some(ab) = directions
        .iter()
        .find(|direction| direction.tag.eq_ignore_ascii_case("AB"))
    else {
        return;
    };
    let Some(ba) = directions
        .iter()
        .find(|direction| direction.tag.eq_ignore_ascii_case("BA"))
    else {
        return;
    };
    let fallback_reason = group_reason(group);
    h.push_str(
        "<div class=\"direction-summary\" role=\"group\" aria-label=\"双向方向汇总\"><strong class=\"direction-summary-title\">双向方向汇总（每个方向各自按接收端 RX 判定）</strong>",
    );
    push_bidirectional_direction(h, ab, &fallback_reason);
    push_bidirectional_direction(h, ba, &fallback_reason);
    h.push_str("</div>");
}

fn push_unit_details(h: &mut String, groups: &[UnitGroup<'_>]) {
    let detail_count: usize = groups.iter().map(|group| group.details.len()).sum();
    h.push_str(&format!(
        "<section class=\"details-section\" aria-labelledby=\"details-heading\"><h2 id=\"details-heading\">逐行明细（{detail_count} 行）</h2>"
    ));
    if report_mixes_traffic_backends(groups) {
        h.push_str(
            "<p class=\"backend-note\">本次报告同时包含 iperf3 与 ctsTraffic 两种后端。二者的 UDP 语义不等价：iperf3 以 <code>-b</code> 恒定速率发送，ctsTraffic 使用 MediaStream 模型（每秒 FrameRate 帧、每帧再拆成 datagram），突发形态与排队行为不同。<strong>同一条链路上两者的 RX 曲线可以明显不同，不应直接互比</strong>；各自与自己的目标比较才有意义。</p>",
        );
    }
    h.push_str("<div class=\"unit-list\">\n");
    for (index, group) in groups.iter().enumerate() {
        let verdict = group_verdict(group);
        let execution_status = group_execution_status(group);
        let open = if unit_open_by_default(verdict) {
            " open"
        } else {
            ""
        };
        let title = group_title(group);
        let execution_meta = unit_execution_meta(group);
        h.push_str(&format!(
            "<details class=\"unit-section\" data-unit-id=\"{}\"{open}><summary class=\"unit-toggle\" id=\"unit-toggle-{index}\"><span class=\"unit-seq\">#{}</span><span class=\"status {}\">{}</span><span class=\"unit-title\">{}</span><span class=\"unit-meta\">{} · {}</span></summary>",
            esc(&group.key),
            group_seq(group),
            verdict.css(),
            verdict.label(),
            esc(title),
            execution_status.label(),
            esc(&execution_meta),
        ));
        push_bidirectional_summary(h, group);
        if group.details.is_empty() {
            h.push_str("<p class=\"summary-note\">本次没有执行行；请结合单元状态和原因查看。</p>");
        } else {
            h.push_str(&format!(
                "<div class=\"table-scroll\" role=\"region\" aria-labelledby=\"unit-toggle-{index}\" tabindex=\"0\"><table class=\"results-table\"><caption class=\"sr-only\">{}的逐行执行明细</caption><thead><tr><th scope=\"col\">结果</th><th scope=\"col\">时间</th><th scope=\"col\">方向</th><th scope=\"col\">类型 / 参数</th><th scope=\"col\">传输</th><th scope=\"col\">请求/活跃/要求流</th><th scope=\"col\">网卡 RX 平均</th><th scope=\"col\">网卡 RX-P10</th><th scope=\"col\">流量工具自报（非判定口径）</th><th scope=\"col\">目标</th><th scope=\"col\">采样覆盖率</th><th scope=\"col\">质量指标</th><th scope=\"col\">诊断详情</th></tr></thead><tbody>\n",
                esc(title),
            ));
            for row in &group.details {
                push_detail_row(h, row, title);
            }
            h.push_str("</tbody></table></div>");
        }
        h.push_str("</details>\n");
    }
    h.push_str("</div></section>\n");
}

pub fn write_report(path: &Path, rows: &mut [Row], meta: &ReportMeta) -> std::io::Result<()> {
    rows.sort_by_key(|row| row.sort_key);
    let groups = group_rows(rows);
    let total = groups
        .iter()
        .filter(|group| group_verdict(group) != Verdict::Skip)
        .count();
    let pass = groups
        .iter()
        .filter(|group| group_verdict(group) == Verdict::Pass)
        .count();
    let rate_fail = groups
        .iter()
        .filter(|group| group_verdict(group) == Verdict::RateFail)
        .count();
    let measured = groups
        .iter()
        .filter(|group| group_verdict(group) == Verdict::Measured)
        .count();
    let not_evaluated = groups
        .iter()
        .filter(|group| group_verdict(group) == Verdict::NotEvaluated)
        .count();
    let setup_error = groups
        .iter()
        .filter(|group| group_verdict(group) == Verdict::SetupError)
        .count();
    let skipped = groups
        .iter()
        .filter(|group| group_verdict(group) == Verdict::Skip)
        .count();
    // UNSTABLE 已经不再产出（掉速统一归 RATE_FAIL，靠原因码区分严重程度），
    // 概览里那格恒为 0 的统计块跟着一起删了——一格永远是 0 的指标会被读成
    // 「这轮没有不稳定的」，而实际上是「这个分类已经不存在了」。
    let judged = pass + rate_fail;
    let rate = if judged > 0 {
        pass as f64 * 100.0 / judged as f64
    } else {
        0.0
    };

    let mut h = String::with_capacity(80 * 1024);
    h.push_str(
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>CPE 子网测试报告</title>
<style>
:root { color-scheme: light; --ink:#17202a; --muted:#5f6b76; --line:#d8dee4; --surface:#fff; --canvas:#f4f6f8; --head:#edf2f6; --yellow:#fff3cd; }
* { box-sizing: border-box; }
html, body { max-width: 100%; overflow-x: hidden; }
body { margin: 0; padding: 20px; color: var(--ink); background: var(--canvas); font-family: "Microsoft YaHei", "PingFang SC", sans-serif; font-size: 14px; line-height: 1.45; }
.report { width: min(100%, 1800px); min-width: 0; margin: 0 auto; }
h1 { margin: 0 0 12px; font-size: 22px; line-height: 1.25; }
h2 { margin: 28px 0 10px; font-size: 17px; line-height: 1.3; }
.sr-only { position: absolute; width: 1px; height: 1px; padding: 0; overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0; }
.meta { display: grid; grid-template-columns: repeat(5, minmax(0, 1fr)); gap: 1px; margin: 0 0 14px; overflow: hidden; border: 1px solid var(--line); border-radius: 6px; background: var(--line); }
.meta-item { min-width: 0; padding: 9px 12px; background: var(--surface); }
.meta-label { display: block; color: var(--muted); font-size: 11px; }
.meta-value { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-weight: 600; }
.summary-grid { display: grid; grid-template-columns: repeat(7, minmax(100px, 1fr)); gap: 8px; margin: 0 0 8px; }
.stat { min-height: 70px; padding: 9px 11px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.stat-label { display: block; color: var(--muted); font-size: 11px; }
.stat-value { display: block; margin-top: 3px; font-size: 20px; font-weight: 700; line-height: 1.1; }
.stat.pass .stat-value, .status.pass { color: #087f3e; }
.stat.fail .stat-value, .status.fail { color: #bd2c2c; }
.stat.measured .stat-value, .status.measured { color: #1769aa; }
.stat.neutral .stat-value { color: #394854; }
.summary-note { margin: 8px 0 0; color: var(--muted); }
.summary-note strong { color: var(--ink); }
.status { font-weight: 700; white-space: nowrap; }
.status.warn { color: #8a5200; }
.status.not-evaluated { color: #7542a8; }
.status.error { color: #a42121; }
.status.skip { color: #59636c; }
/* 横向滚动条必须始终留在视口内：限制容器高度，否则宽表的滚动条会被推到
   页面最底部，只有把整页滚到底才能左右拖动。 */
.overview-scroll { max-width: 100%; max-height: 72vh; overflow: auto; overscroll-behavior: contain; scrollbar-gutter: stable; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.overview-table, .results-table { border-collapse: separate; border-spacing: 0; width: 100%; background: var(--surface); font-size: 12px; }
/* 基准宽度 1432px（序号列 + 截图列），留出 1440 宽屏的 body padding 和纵向滚动条槽，
   常见笔记本屏幕即可完整看完不必横向拖。
   列宽写成百分比而不是 px：table-layout:fixed 下 px 列宽是硬约束，撤掉 min-width
   也不会压缩，窄屏只会继续溢出。百分比按 min-width 或容器宽度解析，因此
   窄屏能等比压缩，而横向真的溢出时（min-width 生效）又正好还原成下面
   冻结列偏移所依赖的 48 / 116 / 250 px。
   c-verdict 必须容得下最长的 NOT_EVALUATED，否则结果列会被裁成 NOT_EVALUATE。 */
.overview-table { min-width: 1432px; table-layout: fixed; }
/* 序号列按 48px 折算；其余列等比缩，冻结列偏移随之改为 0/48/164/414。 */
.overview-table col.c-seq { width: 3.352%; }
.overview-table col.c-verdict { width: 8.101%; }
.overview-table col.c-unit { width: 17.459%; }
.overview-table col.c-dir { width: 3.352%; }
.overview-table col.c-endpoints { width: 13.268%; }
.overview-table col.c-streams { width: 6.145%; }
.overview-table col.c-rate { width: 7.402%; }
.overview-table col.c-target { width: 6.844%; }
.overview-table col.c-shot { width: 12.709%; }
.overview-table col.c-coverage { width: 5.447%; }
.overview-table col.c-quality { width: 8.520%; }
/* 序号只写在单元首行，续行留空；加粗让它在长表里可扫。 */
.overview-table td.seq-col { font-weight: 700; color: var(--muted); }
.overview-table th, .overview-table td, .results-table th, .results-table td { border-right: 1px solid var(--line); border-bottom: 1px solid var(--line); padding: 6px 8px; text-align: left; vertical-align: top; }
.overview-table th:last-child, .overview-table td:last-child, .results-table th:last-child, .results-table td:last-child { border-right: 0; }
/* 列变窄后表头必须允许换行，否则「请求/活跃/要求流」这种标题会被裁掉。 */
.overview-table th { position: sticky; top: 0; z-index: 3; background: var(--head); white-space: normal; line-height: 1.25; vertical-align: bottom; }
.overview-table td { overflow-wrap: anywhere; }
/* 每个测试单元成块：扫描和抄记录时不会把上一单元的方向行读串。 */
.overview-table tr.unit-first > td { border-top: 2px solid #c3ccd4; }
.overview-table tbody tr:first-child > td { border-top: 0; }
.unit-verdict-note { display: block; margin-top: 2px; color: var(--muted); font-size: 11px; }
/* 结果列很窄，标签必须允许在 <wbr> 处折行，不能 nowrap 溢出到相邻列。
   同时要盖掉单元格上的 overflow-wrap: anywhere，否则会断成 NOT_/EVALUATE/D。 */
.overview-table tr:not(.reason-row) > td:nth-child(2) .status, .unit-verdict-note .status { white-space: normal; overflow-wrap: normal; word-break: keep-all; }
/* 判定原因独占整行：文字最长，定宽列里必被截断。 */
.overview-table tr.reason-row > td { padding: 4px 10px 7px; color: var(--muted); background: #fafbfc; overflow-wrap: anywhere; }
.advice { display: inline; color: var(--ink); font-weight: 600; }
.reason-code-detail { display: block; margin-top: 3px; color: var(--muted); font-size: 11px; }
.reason-tag { margin-right: 6px; padding: 0 5px; border: 1px solid var(--line); border-radius: 3px; background: var(--surface); color: #145a94; font-size: 11px; font-weight: 700; }
/* 左侧「测试场景」四列冻结（序号 + 结果 + 单元 + 方向）：窄屏横向拖到接收速率/截图时仍看得到是哪个测试项。
   原因行是 colspan 整行，绝不能被当成第一列跟着冻结。 */
.overview-table th:nth-child(-n+4), .overview-table tr:not(.reason-row) > td:nth-child(-n+4) { position: sticky; z-index: 2; background: var(--surface); }
.overview-table th:nth-child(-n+4) { z-index: 4; background: var(--head); }
.overview-table th:nth-child(1), .overview-table tr:not(.reason-row) > td:nth-child(1) { left: 0; }
.overview-table th:nth-child(2), .overview-table tr:not(.reason-row) > td:nth-child(2) { left: 48px; }
.overview-table th:nth-child(3), .overview-table tr:not(.reason-row) > td:nth-child(3) { left: 164px; }
.overview-table th:nth-child(4), .overview-table tr:not(.reason-row) > td:nth-child(4) { left: 414px; box-shadow: 2px 0 0 rgba(23, 32, 42, .08); }
.overview-table td.num, .results-table td.num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
.overview-table tr:last-child td, .results-table tr:last-child td { border-bottom: 0; }
.reason-cell { color: var(--muted); overflow-wrap: anywhere; }
.unit-list { display: grid; gap: 8px; }
.unit-section { min-width: 0; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.unit-toggle { display: grid; grid-template-columns: auto auto minmax(0, 1fr) auto; align-items: start; gap: 10px; padding: 9px 11px; cursor: pointer; font-weight: 700; }
/* 明细区的序号与概览首列是同一个数，两个区之间靠它对应。 */
.unit-seq { color: var(--muted); font-variant-numeric: tabular-nums; }
.unit-toggle::marker { color: #1769aa; }
.unit-title { min-width: 0; overflow-wrap: anywhere; }
.unit-meta { color: var(--muted); font-size: 12px; font-weight: 400; white-space: nowrap; }
.unit-section[open] > .unit-toggle { border-bottom: 1px solid var(--line); }
.direction-summary { padding: 9px 11px; border-bottom: 1px solid var(--line); background: #f7f9fb; }
.direction-summary-title { display: block; margin-bottom: 5px; }
.direction-summary-row { display: flex; flex-wrap: wrap; gap: 4px 14px; padding: 5px 0; border-top: 1px solid var(--line); }
.direction-summary-tag { min-width: 24px; color: #145a94; font-weight: 700; }
.direction-summary-endpoints { flex: 1 1 260px; min-width: 0; overflow-wrap: anywhere; }
.direction-summary-reason { flex: 1 1 100%; color: var(--muted); overflow-wrap: anywhere; }
.direction-summary-shots { flex: 0 0 auto; }
.shot-col { vertical-align: middle; }
/* 两张缩略图必须并排放进 182px 的截图列（80+80+6+2×8 内边距 = 182）；一旦换行
   堆叠，行高会从约 75px 涨到约 175px，一屏就只剩三四个测试项。窄屏等比压缩时
   两张图跟着一起缩，绝不能溢出去盖住右边的采样覆盖率。 */
.shot-col { overflow: hidden; }
.shot-cell { display: flex; flex-wrap: nowrap; gap: 6px; }
.shot-mini { flex: 1 1 0; min-width: 0; display: inline-flex; flex-direction: column; align-items: center; gap: 1px; color: #145a94; font-size: 11px; font-weight: 600; text-decoration: none; }
.shot-mini img { display: block; width: 100%; max-width: 80px; height: auto; aspect-ratio: 40 / 23; border: 1px solid #b8c2cb; border-radius: 3px; background: #eef2f5; object-fit: cover; }
.shot-mini:hover img, .shot-mini:focus-visible img { border-color: #1769aa; outline: 2px solid #9dc6e8; outline-offset: 1px; }
.table-scroll { max-width: 100%; max-height: 68vh; overflow: auto; overscroll-behavior: contain; scrollbar-gutter: stable; background: var(--surface); }
.results-table { min-width: 1480px; }
/* 时间戳不能被 overflow-wrap: anywhere 拆成一列一个字符，那会把行高撑到十几行。 */
.results-table td:nth-child(2), .results-table th:nth-child(2) { white-space: nowrap; }
/* 工具自报速率是「流确实建立了」的证据，不是判定口径；弱化显示以免和左边的
   网卡 RX 抢注意力。 */
.results-table td.tool-rate { color: var(--muted); white-space: normal; }
.results-table th { position: sticky; top: 0; z-index: 3; background: var(--head); white-space: nowrap; }
.results-table th:first-child, .results-table tr:not(.reason-row) > td:first-child { position: sticky; left: 0; z-index: 2; background: var(--surface); box-shadow: 2px 0 0 rgba(23, 32, 42, .08); }
.results-table th:first-child { z-index: 4; background: var(--head); }
.results-table tr.grouptotal td, .results-table tr.grouptotal td:first-child { background: var(--yellow); font-weight: 700; }
.results-table tr.reason-row > td { padding: 4px 10px 7px; color: var(--muted); background: #fafbfc; overflow-wrap: anywhere; }
.results-table td { overflow-wrap: anywhere; }
.row-diagnostics { min-width: 90px; }
.row-diagnostics > summary { color: #145a94; cursor: pointer; font-weight: 700; }
.diagnostic-availability { display: block; max-width: 180px; margin-top: 2px; color: var(--muted); font-size: 11px; font-weight: 400; line-height: 1.3; overflow-wrap: anywhere; }
.diagnostic-panel { width: min(760px, 78vw); max-width: 100%; padding: 8px 0 2px; }
.diagnostic-grid { display: grid; grid-template-columns: minmax(130px, auto) minmax(0, 1fr); gap: 1px; margin: 0; background: var(--line); border: 1px solid var(--line); }
.diagnostic-grid dt, .diagnostic-grid dd { min-width: 0; margin: 0; padding: 5px 7px; background: var(--surface); overflow-wrap: anywhere; }
.diagnostic-grid dt { color: var(--muted); font-weight: 600; }
.artifact-list { display: flex; flex-wrap: wrap; gap: 8px 12px; margin-top: 8px; }
.command-block { margin-top: 8px; }
.command { display: block; max-width: 100%; margin-top: 4px; padding: 7px; overflow-wrap: anywhere; border: 1px solid var(--line); background: #f7f9fb; white-space: pre-wrap; }
.shot { display: inline-flex; flex-direction: column; gap: 3px; min-width: 126px; margin: 0; }
.shot a { display: inline-flex; flex-direction: column; align-items: flex-start; gap: 3px; color: #145a94; font-weight: 600; text-decoration: none; }
.shot img { display: block; width: 120px; height: 68px; border: 1px solid #b8c2cb; border-radius: 4px; background: #eef2f5; object-fit: cover; }
.shot a:hover img, .shot a:focus-visible img { border-color: #1769aa; outline: 2px solid #9dc6e8; outline-offset: 1px; }
pre { max-height: 420px; margin: 8px 0 0; padding: 10px; overflow: auto; border-radius: 4px; background: #182027; color: #d7ffd7; font-size: 12px; line-height: 1.4; }
details.raw-section { margin: 8px 0; }
details.raw-section > summary { cursor: pointer; font-weight: 700; overflow-wrap: anywhere; }
.sampling-caveat { margin: 8px 0 0; padding: 8px 10px; border-left: 3px solid #8a5200; background: #fff8e6; color: #5e430b; }
/* 链路失联这类横跨一整段单元的事实必须比逐行原因更显眼，用 fail 色而不是警告色。 */
.run-health { margin: 8px 0 0; padding: 8px 10px; border-left: 3px solid #bd2c2c; background: #fdecec; color: #7d1d1d; font-weight: 600; }
.backend-note { margin: 8px 0 12px; padding: 8px 10px; border-left: 3px solid #1769aa; background: #eef5fb; color: #16405e; }
.raw-empty { margin: 8px 0; padding: 8px 10px; border-left: 3px solid #8a5200; background: #fff8e6; color: #5e430b; }
.raw-links { margin: 8px 0; }
.raw-links a, .artifact-list a { color: #145a94; }
summary:focus-visible, a:focus-visible, .table-scroll:focus-visible, .overview-scroll:focus-visible { outline: 3px solid #2b78b8; outline-offset: 2px; }
/* 概览整表需要 1432px。屏幕更窄时不要求用户去找横向滚动条：撤掉 min-width，
   让定宽列按比例压缩、数字换行，一屏内全部看完。滚动条即使限制了容器高度，
   也可能因为上方 meta/统计块把容器底部推到视口之外，所以能不横向滚动最好。 */
@media (max-width: 1460px) {
    .overview-table { min-width: 0; }
    .overview-table td.num, .overview-table th { white-space: normal; }
    /* 等比压缩后不再横向溢出，冻结列没有意义；而 sticky 的 left 约束即使
       scrollLeft=0 也会把单元格顶到 116/366px，把后面几列压重叠。 */
    .overview-table th:nth-child(-n+4), .overview-table tr:not(.reason-row) > td:nth-child(-n+4) { position: static; box-shadow: none; }
}
/* 再窄就压不动了：恢复横向滚动 + 冻结列，并压低容器高度让滚动条尽量在视口内。 */
@media (max-width: 1000px) {
    /* 必须与基准一致：sticky 的 left 偏移是按 1432px 基准算出的 0/48/164/414 px
       字面量，这里若还写 1384px，列宽会等比缩小而偏移不变，冻结列直接压到相邻列上。 */
    .overview-table { min-width: 1432px; }
    /* 窄屏上方的 meta/统计块会换行变高，把表格推得更靠下；容器再压低一点，
       横向滚动条才不至于又跑到视口外面。 */
    .overview-scroll { max-height: 50vh; }
    .overview-table th:nth-child(-n+4), .overview-table tr:not(.reason-row) > td:nth-child(-n+4) { position: sticky; }
    /* 分隔阴影落在冻结块的最后一列（方向列）。写 nth-child(3) 会在冻结块中间
       多画一道线，而基准规则里第 4 列的那道并不会因此消失。 */
    .overview-table tr:not(.reason-row) > td:nth-child(4), .overview-table th:nth-child(4) { box-shadow: 2px 0 0 rgba(23, 32, 42, .08); }
}
@media (max-width: 1100px) {
    body { padding: 12px; }
    .meta { grid-template-columns: repeat(2, minmax(0, 1fr)); }
    .summary-grid { grid-template-columns: repeat(4, minmax(100px, 1fr)); }
}
@media (max-width: 620px) {
    body { padding: 8px; font-size: 13px; }
    h1 { font-size: 19px; }
    .meta { grid-template-columns: 1fr; }
    .summary-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 6px; }
    .stat-value { font-size: 18px; }
    .unit-toggle { grid-template-columns: auto auto minmax(0, 1fr); }
    .unit-meta { grid-column: 2; white-space: normal; }
    .diagnostic-panel { width: 720px; max-width: 72vw; }
    .raw-section, .row-diagnostics { display: block; }
    .shot { display: inline-flex; }
}
@media print {
    html, body { overflow: visible; }
    body { padding: 0; background: #fff; }
    .table-scroll, .overview-scroll { max-height: none; overflow: visible; border: 0; }
    .overview-table, .results-table { min-width: 0; font-size: 8px; }
    .overview-table th, .overview-table td, .results-table th, .results-table td { padding: 2px 3px; }
    /* 打印时表格不再横向滚动，冻结列会错位；缩略图也放不下，只留链接文字。 */
    .overview-table th, .overview-table td, .results-table th, .results-table td { position: static; box-shadow: none; }
    .shot-mini img { display: none; }
    .raw-section, .shot, .row-diagnostics { display: none; }
}
</style></head><body><main class="report">
<h1>CPE 子网测试报告</h1>
"#,
    );
    h.push_str(&format!(
        "<div class=\"meta\"><div class=\"meta-item\"><span class=\"meta-label\">主控</span><span class=\"meta-value\" title=\"{}\">{}</span></div><div class=\"meta-item\"><span class=\"meta-label\">辅测</span><span class=\"meta-value\" title=\"{}\">{}</span></div><div class=\"meta-item\"><span class=\"meta-label\">辅测地址</span><span class=\"meta-value\" title=\"{}\">{}</span></div><div class=\"meta-item\"><span class=\"meta-label\">开始</span><span class=\"meta-value\">{}</span></div><div class=\"meta-item\"><span class=\"meta-label\">结束</span><span class=\"meta-value\">{}</span></div></div>\n",
        esc(&meta.master_pc),
        esc(&meta.master_pc),
        esc(&meta.agent_pc),
        esc(&meta.agent_pc),
        esc(&meta.agent_host),
        esc(&meta.agent_host),
        esc(&meta.started),
        esc(&meta.finished)
    ));
    h.push_str(&format!(
        "<div class=\"summary-grid\"><div class=\"stat neutral\"><span class=\"stat-label\">测试单元</span><strong class=\"stat-value\">{total}</strong></div><div class=\"stat pass\"><span class=\"stat-label\">PASS</span><strong class=\"stat-value\">{pass}</strong></div><div class=\"stat fail\"><span class=\"stat-label\">RATE_FAIL</span><strong class=\"stat-value\">{rate_fail}</strong></div><div class=\"stat measured\"><span class=\"stat-label\">MEASURED</span><strong class=\"stat-value\">{measured}</strong></div><div class=\"stat neutral\"><span class=\"stat-label\">SETUP_ERROR</span><strong class=\"stat-value\">{setup_error}</strong></div><div class=\"stat neutral\"><span class=\"stat-label\">耗时</span><strong class=\"stat-value\">{}</strong></div></div>\n",
        esc(&meta.elapsed)
    ));
    h.push_str(&format!(
        "<p class=\"summary-note\">测试单元: {total}（不含 SKIP） · 判定通过率: <strong>{pass}/{judged} = {rate:.1}%</strong>（仅统计 PASS、RATE_FAIL、UNSTABLE）；NOT_EVALUATED: {not_evaluated}，SKIP: {skipped}</p>\n",
        total = total,
        pass = pass,
        judged = judged,
        rate = rate,
        not_evaluated = not_evaluated,
        skipped = skipped,
    ));

    if !meta.run_health.is_empty() {
        h.push_str(&format!(
            "<p class=\"run-health\" role=\"alert\"><strong>运行健康</strong>：{}</p>\n",
            esc(&meta.run_health)
        ));
    }

    if !meta.counter_source_caveat.is_empty() {
        h.push_str(&format!(
            "<p class=\"sampling-caveat\"><strong>采样口径提示</strong>：{}</p>\n",
            esc(&meta.counter_source_caveat)
        ));
    }

    push_overview(&mut h, &groups);
    push_unit_details(&mut h, &groups);

    let raw_rows = rows
        .iter()
        .filter(|row| {
            !row.is_unit_summary
                && (!row.raws.is_empty() || !row.raw_log.is_empty() || !row.nic_samples.is_empty())
        })
        .collect::<Vec<_>>();
    let raw_item_count: usize = raw_rows
        .iter()
        .map(|row| {
            row.raws.len()
                + usize::from(!row.raw_log.is_empty())
                + usize::from(!row.nic_samples.is_empty())
        })
        .sum();
    let raw_nonempty_count: usize = raw_rows.iter().map(|row| nonempty_raw_count(row)).sum();
    h.push_str(&format!(
        "<h2 id=\"raw-heading\">原始输出（{} 条执行记录，{raw_item_count} 项内容，内嵌文本非空 {raw_nonempty_count} 段）</h2>\n",
        raw_rows.len(),
    ));
    if raw_rows.is_empty() {
        h.push_str(
            "<p class=\"raw-empty\">本次报告没有可用的内嵌原始输出、独立原始记录或网卡样本文件。</p>\n",
        );
    }
    for r in raw_rows {
        let file_count =
            usize::from(!r.raw_log.is_empty()) + usize::from(!r.nic_samples.is_empty());
        h.push_str(&format!(
            "<details class=\"raw-section\" id=\"{}\"><summary>{} — {} [{}] · {} 段内嵌输出（非空 {}） · {file_count} 个原始文件</summary>\n",
            raw_anchor(r),
            esc(&r.time),
            esc(&r.task),
            r.verdict.label(),
            r.raws.len(),
            nonempty_raw_count(r),
        ));
        if !r.raw_log.is_empty() || !r.nic_samples.is_empty() {
            let links = [
                artifact_link(&r.raw_log, "独立原始记录"),
                artifact_link(&r.nic_samples, "网卡逐样本 CSV"),
            ]
            .into_iter()
            .filter(|link| !link.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
            h.push_str(&format!("<p class=\"raw-links\">{links}</p>\n"));
        }
        for (title, text) in &r.raws {
            h.push_str(&format!(
                "<h3>{}</h3><pre>{}</pre>\n",
                esc(title),
                esc(&embedded_raw(text))
            ));
        }
        h.push_str("</details>\n");
    }
    h.push_str("</main></body></html>\n");

    std::fs::write(path, h)
}

#[cfg(test)]
mod tests {
    /// 超长原始输出要掐头去尾，而不是只留开头。
    ///
    /// iperf3 / ctsTraffic 的汇总行在**最后**，只留开头等于把结论截掉。
    /// 同一份文本已经作为 raw_log 单独落盘，内嵌这份是给「点开就看」用的。
    #[test]
    fn an_oversized_raw_segment_keeps_both_ends() {
        let short = "一切正常";
        assert_eq!(super::embedded_raw(short), short, "没超限的原样保留");

        let body = "x".repeat(super::EMBEDDED_RAW_MAX_CHARS * 2);
        let text = format!("开头标记\n{body}\n结尾汇总行 receiver");
        let trimmed = super::embedded_raw(&text);

        assert!(trimmed.chars().count() < text.chars().count(), "必须变短");
        assert!(trimmed.starts_with("开头标记"), "开头要留");
        assert!(
            trimmed.ends_with("结尾汇总行 receiver"),
            "结尾的汇总行必须留住，那才是最常要看的一段"
        );
        assert!(trimmed.contains("中间省略"), "省略要说出来");
        assert!(trimmed.contains("独立原始记录"), "要指向完整那份");
    }

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static REPORT_INDEX: AtomicUsize = AtomicUsize::new(0);

    fn render(rows: Vec<Row>) -> String {
        render_with_meta(rows, &ReportMeta::default())
    }

    fn render_with_meta(mut rows: Vec<Row>, meta: &ReportMeta) -> String {
        let index = REPORT_INDEX.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("cpe_report_test_{}_{}", std::process::id(), index));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.html");
        write_report(&path, &mut rows, meta).unwrap();
        let html = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(dir);
        html
    }

    fn traffic_detail(unit: &str, sort_key: (usize, usize, usize, u8)) -> Row {
        Row {
            sort_key,
            time: "2026-07-04 12:00:00".into(),
            task_id: format!("{unit}-flow"),
            parent_id: unit.into(),
            task: "IPERF V4 TCP".into(),
            transport: "TCP".into(),
            param: "-P 1".into(),
            src_pc: "master".into(),
            src_iface: "eth0".into(),
            src_ip: "192.0.2.1".into(),
            dst_pc: "agent".into(),
            dst_iface: "eth1".into(),
            dst_ip: "192.0.2.2".into(),
            verdict: Verdict::Pass,
            execution_status: ExecutionStatus::Completed,
            kind_label: "灌包-ab".into(),
            requested_streams: 1,
            active_streams: 1,
            required_streams: 1,
            rx_avg: Some(930.125),
            rx_p10: Some(900.5),
            target_mbps: Some(850.0),
            sample_coverage: Some(0.98),
            tx_mbps: Some(940.0),
            rx_mbps: Some(920.0),
            ..Default::default()
        }
    }

    fn unit_summary(unit: &str, verdict: Verdict) -> Row {
        Row {
            sort_key: (0, usize::MAX, usize::MAX, u8::MAX),
            task_id: unit.into(),
            parent_id: unit.into(),
            task: "IPERF V4 TCP".into(),
            verdict,
            execution_status: if verdict == Verdict::SetupError {
                ExecutionStatus::Error
            } else {
                ExecutionStatus::Completed
            },
            kind_label: "UNIT_SUMMARY_SENTINEL".into(),
            is_unit_summary: true,
            ..Default::default()
        }
    }

    #[test]
    fn report_renders_compact_accessible_structure_and_diagnostics() {
        let mut detail = traffic_detail("unit-a", (0, 0, 0, 0));
        detail.raw_log = "./iperf_outputs/iperf_tcp.log".into();
        detail.nic_samples = "./iperf_outputs/nic.csv".into();
        detail.screenshot_master = "./iperf_outputs/master.png".into();
        detail.screenshot_agent = "./iperf_outputs/agent.png".into();
        detail.command = "iperf3 -c <target>".into();
        detail.peer_rx = "950.000 Mbps (BA)".into();
        detail.raws = vec![("client".into(), "<output>".into())];
        let html = render(vec![detail, unit_summary("unit-a", Verdict::Pass)]);

        assert!(html.contains("测试概览"));
        assert!(html.contains("逐行明细（1 行）"));
        assert!(html.contains("<caption class=\"sr-only\">"));
        assert!(html.contains("<th scope=\"col\">网卡 RX 平均</th>"));
        assert!(html.contains("role=\"region\""));
        assert!(html.contains("tabindex=\"0\""));
        assert!(html.contains("max-height: 68vh"));
        assert!(html.contains("overflow: auto"));
        assert!(html.contains("position: sticky"));
        assert!(html.contains("<details class=\"unit-section\""));
        // 工具自报速率是「流确实建立了」的唯一证据，且单条流明细行没有网卡数据，
        // 必须留在表内可见，不能只藏在折叠的诊断面板里。
        assert!(html.contains("<th scope=\"col\">流量工具自报（非判定口径）</th>"));
        assert!(html.contains("发 940.000 / 收 920.000 Mbps"));
        assert!(!html.contains("流量工具 sender 汇总（诊断）"));
        assert!(!html.contains("流量工具 receiver 汇总（诊断）"));
        assert_eq!(tool_rate_text(None, None), "未采集");
        // 网卡计数器按方向采，拆不到单条流上；流明细行必须写明去组合计行看，
        // 不能只留「未采集」让人以为这条流没测到。
        let mut bare = traffic_detail("unit-bare", (0, 0, 0, 0));
        bare.rx_avg = None;
        bare.rx_p10 = None;
        bare.sample_coverage = None;
        bare.tx_mbps = Some(500.0);
        bare.rx_mbps = Some(498.2);
        let bare_html = render(vec![bare, unit_summary("unit-bare", Verdict::Measured)]);
        assert!(bare_html.contains("—（按方向统计，见组合计行）"));
        assert!(bare_html.contains("发 500.000 / 收 498.200 Mbps"));
        assert_eq!(
            tool_rate_text(Some(500.0), None),
            "发 500.000 / 收 未采集 Mbps"
        );
        assert!(html.contains("950.000 Mbps (BA)"));
        assert!(html.contains("的诊断详情\"><span>诊断</span>"));
        for label in [
            "主控截图",
            "辅测截图",
            "灌包命令",
            "原始记录",
            "网卡样本",
            "内嵌原始输出（非空 1/1）",
        ] {
            assert!(html.contains(label), "missing diagnostic label: {label}");
        }
        assert!(html.contains("主控截图 · 查看原图"));
        assert!(html.contains("辅测截图 · 查看原图"));
        assert!(html.contains("实际灌包命令"));
        assert!(html.contains("独立原始记录（raw_log）"));
        assert!(html.contains("href=\"#raw-0-0-0-0\""));
        assert!(!html.contains("aria-label=\"展开"));
        assert!(!html.contains("后端发送 Mbps"));
        assert!(!html.contains("后端接收 Mbps"));
        let main_header = html
            .split("<table class=\"results-table\">")
            .nth(1)
            .unwrap()
            .split("</thead>")
            .next()
            .unwrap();
        assert!(!main_header.contains("流量工具 sender"));
        assert!(!main_header.contains("流量工具 receiver"));
        assert!(html.contains("&lt;output&gt;"));
        assert!(html.contains("iperf3 -c &lt;target&gt;"));
        assert!(html.contains("独立原始记录"));
        assert!(html.contains("网卡逐样本 CSV"));
        assert!(html.contains("原始输出（1 条执行记录，3 项内容，内嵌文本非空 1 段）"));
        assert!(html.contains("1 段内嵌输出（非空 1） · 2 个原始文件"));
        assert!(html.contains(".raw-section, .row-diagnostics { display: block; }"));
        assert!(html.contains(".shot { display: inline-flex; }"));
    }

    #[test]
    fn test_report_counts_units_instead_of_flow_details() {
        let mut rows: Vec<Row> = (0..20)
            .map(|idx| {
                let mut row = traffic_detail("udp-unit", (0, 0, idx, 0));
                row.task_id = format!("flow-{idx}");
                row.task = "IPERF V4 UDP".into();
                row.transport = "UDP".into();
                row
            })
            .collect();
        let mut summary = unit_summary("udp-unit", Verdict::Pass);
        summary.task = "IPERF V4 UDP".into();
        rows.push(summary);
        let html = render(rows);

        assert!(html.contains("测试单元: 1"));
        assert!(html.contains(
            "<span class=\"stat-label\">PASS</span><strong class=\"stat-value\">1</strong>"
        ));
        assert!(html.contains("逐行明细（20 行）"));
        assert!(html.contains("20 条 UDP 流执行行"));
        assert!(!html.contains("测试单元: 21"));
    }

    #[test]
    fn legacy_group_without_summary_matches_executor_verdict_priority() {
        let mut rate_fail = traffic_detail("legacy-unit", (0, 0, 0, 0));
        rate_fail.verdict = Verdict::RateFail;
        let mut not_evaluated = traffic_detail("legacy-unit", (0, 0, 1, 0));
        not_evaluated.verdict = Verdict::NotEvaluated;

        let rows = vec![rate_fail, not_evaluated];
        let groups = group_rows(&rows);
        assert_eq!(groups.len(), 1);
        assert_eq!(group_verdict(&groups[0]), Verdict::NotEvaluated);
    }

    #[test]
    fn legacy_group_without_summary_keeps_single_udp_hard_failure_visible() {
        for code in [
            "SINGLE_UDP_STREAM_FAILED",
            "CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED",
        ] {
            let mut hard_failure = traffic_detail("legacy-hard", (0, 0, 0, 0));
            hard_failure.verdict = Verdict::RateFail;
            hard_failure.reason_code = code.into();
            let mut not_evaluated = traffic_detail("legacy-hard", (0, 1, 0, 0));
            not_evaluated.verdict = Verdict::NotEvaluated;
            not_evaluated.reason_code = "SAMPLE_COVERAGE_LOW".into();

            let rows = vec![hard_failure, not_evaluated];
            let groups = group_rows(&rows);
            // 必须灌通的方向硬失败不能被另一腿普通的 NOT_EVALUATED 掩盖。
            assert_eq!(group_verdict(&groups[0]), Verdict::RateFail, "code={code}");
        }

        // SetupError 仍然优先于硬失败，与 executor 的聚合顺序一致。
        let mut hard_failure = traffic_detail("legacy-setup", (0, 0, 0, 0));
        hard_failure.verdict = Verdict::RateFail;
        hard_failure.reason_code = "SINGLE_UDP_STREAM_FAILED".into();
        let mut setup_error = traffic_detail("legacy-setup", (0, 1, 0, 0));
        setup_error.verdict = Verdict::SetupError;
        let rows = vec![hard_failure, setup_error];
        let groups = group_rows(&rows);
        assert_eq!(group_verdict(&groups[0]), Verdict::SetupError);
    }

    #[test]
    fn overview_keeps_scenario_columns_and_screenshots_reachable_without_page_scroll() {
        let mut summary = unit_summary("unit-shot", Verdict::Pass);
        summary.task = "★双向 IPERF V4 UDP -b 500m".into();
        summary.direction_summaries = vec![
            DirectionSummary {
                tag: "AB".into(),
                src: "master/eth0".into(),
                dst: "agent/eth1".into(),
                verdict: Verdict::Pass,
                rx_avg: Some(8500.0),
                rx_p10: Some(8450.0),
                target_mbps: Some(8400.0),
                screenshot_master: "./iperf_outputs/ab_master.png".into(),
                screenshot_agent: "./iperf_outputs/ab_agent.png".into(),
                ..Default::default()
            },
            DirectionSummary {
                tag: "BA".into(),
                src: "agent/eth1".into(),
                dst: "master/eth0".into(),
                verdict: Verdict::Pass,
                rx_avg: Some(6500.0),
                rx_p10: Some(6450.0),
                target_mbps: Some(6400.0),
                screenshot_master: "./iperf_outputs/ba_master.png".into(),
                ..Default::default()
            },
        ];
        let html = render(vec![summary]);

        // 横向滚动条留在视口内，而不是被推到整页最底部。
        assert!(
            html.contains(".overview-scroll { max-width: 100%; max-height: 72vh; overflow: auto;")
        );
        // 左侧「序号 / 结果 / 测试单元 / 方向」四列冻结，右拖看速率时仍知道是哪个测试项。
        assert!(html.contains(".overview-table th:nth-child(-n+4), .overview-table tr:not(.reason-row) > td:nth-child(-n+4) { position: sticky;"));
        // 冻结偏移必须与 colgroup 列宽一致：48 + 116 + 250 = 414。
        assert!(html.contains(
            ".overview-table th:nth-child(4), .overview-table tr:not(.reason-row) > td:nth-child(4) { left: 414px;"
        ));
        // 基准 1432px 下 3.352% = 48px、8.101% = 116px、17.459% = 250px。
        assert!(html.contains(".overview-table { min-width: 1432px; table-layout: fixed; }"));
        assert!(html.contains(".overview-table col.c-seq { width: 3.352%; }"));
        assert!(html.contains(".overview-table col.c-verdict { width: 8.101%; }"));
        assert!(html.contains(".overview-table col.c-unit { width: 17.459%; }"));
        // 屏幕装不下时改为等比压缩，不让用户去页面底部找横向滚动条。
        assert!(html.contains("@media (max-width: 1460px)"));
        // 窄列里最长的结果标签必须能在下划线处折行，不能溢出盖住相邻列。
        assert_eq!(
            status_label_html(Verdict::NotEvaluated),
            "NOT_<wbr>EVALUATED"
        );
        assert_eq!(status_label_html(Verdict::SetupError), "SETUP_<wbr>ERROR");
        assert_eq!(status_label_html(Verdict::Pass), "PASS");
        assert!(html.contains(
            ".overview-table tr:not(.reason-row) > td:nth-child(2) .status, .unit-verdict-note .status { white-space: normal; overflow-wrap: normal;"
        ));
        // 两张缩略图必须并排，换行堆叠会把行高翻倍。
        assert!(html.contains(".shot-cell { display: flex; flex-wrap: nowrap;"));
        assert!(html.contains(".shot-mini img { display: block; width: 100%; max-width: 80px;"));
        assert!(html.contains("<th scope=\"col\">接收端 RX 平均</th>"));
        // 判定原因独占整行，不再挤在定宽列里被截断。
        assert!(!html.contains("<th scope=\"col\">原因</th>"));
        assert!(html.contains("<tr class=\"reason-row\""));
        assert!(html.contains("colspan=\"12\""));
        // 截图列与接收速率同排，不必展开诊断面板。
        assert!(html.contains("<th scope=\"col\">截图</th>"));
        assert!(html.contains("<col class=\"c-shot\">"));
        for path in [
            "./iperf_outputs/ab_master.png",
            "./iperf_outputs/ab_agent.png",
            "./iperf_outputs/ba_master.png",
        ] {
            assert!(html.contains(path), "missing overview screenshot: {path}");
        }
        // BA 只有主控截图时，辅测不能渲染成空缩略图。
        assert_eq!(html.matches("class=\"shot-mini\"").count(), 6);
    }

    /// 概览和明细都必须带执行序号，且与控制台打印的 `[N/总数]` 一致。
    ///
    /// 120 个单元里「主控 以太网 6 -> 辅测 以太网」这类标题会重复十几次，
    /// 只有标题的话，拿着控制台记录去报告里找对应项根本定位不到。
    #[test]
    fn every_unit_carries_the_console_sequence_number() {
        let mut first = unit_summary("unit-a", Verdict::Measured);
        first.sort_key = (0, usize::MAX, usize::MAX, u8::MAX);
        first.task = "IPERF V4 TCP | 主控 以太网 6 -> 辅测 以太网".into();
        let mut later = unit_summary("unit-b", Verdict::Measured);
        later.sort_key = (113, usize::MAX, usize::MAX, u8::MAX);
        later.task = "IPERF V4 TCP | 主控 以太网 6 -> 辅测 以太网".into();

        let html = render(vec![first, later]);

        // 序号从 1 开始，与控制台的 [1/120] / [114/120] 对齐。
        assert!(html.contains("<th scope=\"col\">#</th>"));
        assert!(html.contains(">1</td>"), "第一个单元应显示 1");
        assert!(
            html.contains(">114</td>"),
            "sort_key.0=113 的单元应显示 114"
        );
        // 明细区用同一个数，两个区之间才对得上。
        assert!(html.contains("<span class=\"unit-seq\">#1</span>"));
        assert!(html.contains("<span class=\"unit-seq\">#114</span>"));
    }

    #[test]
    fn overview_drops_the_screenshot_column_when_nothing_was_captured() {
        let mut summary = unit_summary("unit-noshot", Verdict::Pass);
        summary.task = "IPERF V4 UDP".into();
        summary.direction_summaries = vec![DirectionSummary {
            tag: "单向".into(),
            src: "master/eth0".into(),
            dst: "agent/eth1".into(),
            verdict: Verdict::Measured,
            reason_code: "TARGET_UNKNOWN".into(),
            reason_detail: "Observe 模式仅记录实际能力".into(),
            rx_avg: Some(940.0),
            ..Default::default()
        }];
        let html = render(vec![summary]);

        // 关掉截图时整列都会是「未采集」，白占约 190px，应该整列不渲染。
        assert!(!html.contains("<th scope=\"col\">截图</th>"));
        assert!(!html.contains("<col class=\"c-shot\">"));
        // 序号列让整表多一列，原因行的 colspan 必须跟着走。
        assert!(html.contains("colspan=\"11\""));
        assert!(html.contains("TARGET_UNKNOWN: Observe 模式仅记录实际能力"));
    }

    #[test]
    fn diagnostics_expose_the_window_baseline_and_rolling_coverage_used_for_judgement() {
        let mut detail = traffic_detail("unit-window", (0, 0, 0, 0));
        detail.is_grouptotal = true;
        detail.window_start_ms = Some(8_000);
        detail.window_end_ms = Some(188_000);
        detail.baseline_mbps = Some(12.5);
        detail.rolling_coverage = Some(0.978);
        detail.effective_seconds = Some(180.0);
        detail.required_seconds = Some(180.0);
        let html = render(vec![detail, unit_summary("unit-window", Verdict::Pass)]);

        // 判定窗口的两个端点必须能直接对到网卡逐样本 CSV 的 elapsed_ms 列。
        assert!(html.contains("判定窗口区间"));
        assert!(html.contains("8000 ms – 188000 ms（对应网卡样本 elapsed_ms）"));
        // 验收要求核对「原始总流量 − 业务流量 ≈ 背景值」，扣除量必须报出来。
        assert!(html.contains("12.500 Mbps（起流前空闲期中位数）"));
        // 滚动窗口覆盖率与总采样覆盖率是两个门槛，不能只报一个。
        assert!(html.contains("5 秒滚动窗口覆盖率"));
        assert!(html.contains("97.8%"));
    }

    #[test]
    fn transport_column_names_the_backend_and_warns_when_both_are_mixed() {
        assert_eq!(transport_display("UDP"), "iperf3 UDP");
        assert_eq!(transport_display("TCP"), "iperf3 TCP");
        assert_eq!(transport_display("CTS/UDP"), "ctsTraffic UDP");
        assert_eq!(transport_display("CTS/TCP"), "ctsTraffic TCP");
        assert_eq!(transport_display(""), "");

        // 只有 iperf3 时不必提示口径差异。
        let only_iperf = traffic_detail("unit-iperf", (0, 0, 0, 0));
        let html = render(vec![only_iperf, unit_summary("unit-iperf", Verdict::Pass)]);
        assert!(html.contains("iperf3 TCP"));
        assert!(!html.contains("二者的 UDP 语义不等价"));

        // 两种后端同时出现时必须写明不可直接互比。
        let mut iperf = traffic_detail("unit-mix", (0, 0, 0, 0));
        iperf.transport = "UDP".into();
        let mut cts = traffic_detail("unit-mix", (0, 0, 1, 0));
        cts.transport = "CTS/UDP".into();
        let mixed = render(vec![iperf, cts, unit_summary("unit-mix", Verdict::Pass)]);
        assert!(mixed.contains("iperf3 UDP"));
        assert!(mixed.contains("ctsTraffic UDP"));
        assert!(mixed.contains("不应直接互比"));
    }

    #[test]
    fn sampling_caveat_is_shown_only_when_the_platform_actually_differs() {
        let detail = traffic_detail("unit-caveat", (0, 0, 0, 0));
        let quiet = render_with_meta(
            vec![detail.clone(), unit_summary("unit-caveat", Verdict::Pass)],
            &ReportMeta::default(),
        );
        assert!(!quiet.contains("采样口径提示"));

        let noisy = render_with_meta(
            vec![detail, unit_summary("unit-caveat", Verdict::Pass)],
            &ReportMeta {
                counter_source_caveat: "本机为 macOS：网卡计数器经由 netstat 子进程逐次采样".into(),
                ..Default::default()
            },
        );
        assert!(noisy.contains("采样口径提示"));
        assert!(noisy.contains("netstat 子进程"));
    }

    #[test]
    fn screenshot_thumbnail_and_text_link_to_original_image() {
        let html = screenshot_link("./iperf_outputs/shot&1.png", "主控截图");

        assert!(html.contains("<figure class=\"shot\">"));
        assert!(html.contains("<a href=\"./iperf_outputs/shot&amp;1.png\""));
        assert!(html.contains("<img src=\"./iperf_outputs/shot&amp;1.png\""));
        assert!(html.contains("target=\"_blank\""));
        assert!(html.contains("rel=\"noopener\""));
        assert!(html.contains("title=\"查看主控截图\""));
        assert!(html.contains("主控截图 · 查看原图"));
        assert_eq!(html.matches("./iperf_outputs/shot&amp;1.png").count(), 2);
        assert!(screenshot_link("", "主控截图").is_empty());
    }

    #[test]
    fn bidirectional_overview_keeps_ab_and_ba_separate() {
        let mut summary = unit_summary("unit-bidir", Verdict::RateFail);
        summary.task = "双向 TCP".into();
        summary.direction_summaries = vec![
            DirectionSummary {
                tag: "AB".into(),
                src: "master/eth0".into(),
                dst: "agent/eth1".into(),
                verdict: Verdict::Pass,
                streams: Some(StreamCounts {
                    requested: 1,
                    active: 1,
                    required: 1,
                }),
                rx_avg: Some(900.0),
                rx_p10: Some(880.0),
                target_mbps: Some(850.0),
                sample_coverage: Some(1.0),
                ..Default::default()
            },
            DirectionSummary {
                tag: "BA".into(),
                src: "agent/eth1".into(),
                dst: "master/eth0".into(),
                verdict: Verdict::RateFail,
                reason: "RX_P10_BELOW_TARGET".into(),
                streams: Some(StreamCounts {
                    requested: 1,
                    active: 1,
                    required: 1,
                }),
                rx_avg: Some(800.0),
                rx_p10: Some(760.0),
                target_mbps: Some(850.0),
                sample_coverage: Some(0.99),
                ..Default::default()
            },
        ];
        let html = render(vec![summary]);

        assert!(html.contains("data-direction=\"AB\""));
        assert!(html.contains("data-direction=\"BA\""));
        assert!(html.contains("900.000 Mbps"));
        assert!(html.contains("800.000 Mbps"));
        assert!(html.contains("RX_P10_BELOW_TARGET"));
        assert!(html.contains("双向方向汇总"));
        assert!(html.contains("2 个方向执行行（AB / BA）"));
        // 双向只按各自方向的接收端速率判定；任何形式的 AB+BA 相加都会被误读成
        // 整机吞吐，尤其是一个方向未评价时。
        assert!(!html.contains("AB + BA"));
        assert!(!html.contains("RX 平均合计"));
        assert!(!html.contains("1700.000 Mbps"));
        assert!(!html.contains("1640.000 Mbps"));
        assert!(html.contains("data-unit-id=\"unit-bidir\" open"));
    }

    #[test]
    fn passing_direction_does_not_inherit_failure_reason_from_other_direction() {
        let mut summary = unit_summary("unit-mixed", Verdict::RateFail);
        summary.task = "双向 TCP".into();
        summary.reason_code = "RX_BELOW_TARGET".into();
        summary.reason_detail = "BA: RX 平均 790.000 Mbps 低于目标 800.000 Mbps".into();
        summary.direction_summaries = vec![
            DirectionSummary {
                tag: "AB".into(),
                verdict: Verdict::Pass,
                rx_avg: Some(875.0),
                rx_p10: Some(850.0),
                target_mbps: Some(800.0),
                ..Default::default()
            },
            DirectionSummary {
                tag: "BA".into(),
                verdict: Verdict::RateFail,
                reason_code: "RX_BELOW_TARGET".into(),
                reason_detail: "RX 平均 790.000 Mbps 低于目标 800.000 Mbps".into(),
                rx_avg: Some(790.0),
                rx_p10: Some(770.0),
                target_mbps: Some(800.0),
                ..Default::default()
            },
        ];
        let html = render(vec![summary]);

        assert!(html.contains(
            "RX_TARGET_MET: RX 平均 875.000 Mbps、RX-P10 850.000 Mbps 均不低于目标 800.000 Mbps"
        ));
        assert!(html.contains("RX_BELOW_TARGET: RX 平均 790.000 Mbps &lt; 目标 800.000 Mbps"));
        assert!(!html.contains("RX 平均 875.000 Mbps &gt;= 目标 800.000 Mbps"));
    }

    #[test]
    fn bidirectional_udp_meta_explains_flow_and_group_rows() {
        let mut rows = Vec::new();
        for (leg, tag) in [(0, "AB"), (1, "BA")] {
            for stream in 0..2 {
                let mut row = traffic_detail("udp-bidir-meta", (0, leg, stream, 0));
                row.task = "双向 UDP".into();
                row.transport = "UDP".into();
                row.kind_label = format!("灌包-{tag}");
                rows.push(row);
            }
            let mut total = traffic_detail("udp-bidir-meta", (0, leg, 2, 1));
            total.task = "双向 UDP".into();
            total.transport = "UDP".into();
            total.kind_label = format!("★组合计-{tag}");
            total.is_grouptotal = true;
            rows.push(total);
        }
        let mut summary = unit_summary("udp-bidir-meta", Verdict::Pass);
        summary.task = "双向 UDP".into();
        summary.direction_summaries = vec![
            DirectionSummary {
                tag: "AB".into(),
                verdict: Verdict::Pass,
                ..Default::default()
            },
            DirectionSummary {
                tag: "BA".into(),
                verdict: Verdict::Pass,
                ..Default::default()
            },
        ];
        rows.push(summary);
        let html = render(rows);

        assert!(html.contains("2 个方向 · 4 条 UDP 流明细 · 2 条方向组合计"));
    }

    #[test]
    fn missing_nic_rx_is_not_filled_from_tool_receiver() {
        let mut summary = unit_summary("unit-no-nic", Verdict::NotEvaluated);
        summary.rx_mbps = Some(777.777);
        let html = render(vec![summary]);

        assert!(html.contains(NOT_COLLECTED));
        assert!(!html.contains("777.777"));
    }

    #[test]
    fn ping_uses_not_applicable_instead_of_zero_stream_counts() {
        let detail = Row {
            sort_key: (0, 0, 0, 0),
            time: "2026-07-04 12:00:00".into(),
            task_id: "ping-flow".into(),
            parent_id: "ping-unit".into(),
            task: "PING V4".into(),
            verdict: Verdict::Pass,
            execution_status: ExecutionStatus::Completed,
            kind_label: "PING".into(),
            ping_loss: Some(0.0),
            ping_min: Some(1.25),
            ping_avg: Some(2.5),
            ping_max: Some(4.75),
            ..Default::default()
        };
        let mut summary = unit_summary("ping-unit", Verdict::Pass);
        summary.task = "PING V4".into();
        let html = render(vec![detail, summary]);

        assert!(html.contains(NOT_APPLICABLE));
        assert!(html.contains("丢包率 0.0%"));
        assert!(html.contains("RTT 最小/平均/最大 1.250/2.500/4.750 ms"));
        assert!(html.contains("PING_OK:"));
        assert!(html.contains("1 条 Ping 执行行"));
        assert!(!html.contains("0/0/0"));
    }

    #[test]
    fn unit_summary_is_excluded_from_detail_rows_and_raw_output() {
        let detail = traffic_detail("unit-exclude", (0, 0, 0, 0));
        let mut summary = unit_summary("unit-exclude", Verdict::Pass);
        summary.raw_log = "summary-raw-must-not-render.log".into();
        summary.raws = vec![("summary".into(), "summary raw".into())];
        let html = render(vec![detail, summary]);

        assert!(html.contains("逐行明细（1 行）"));
        assert_eq!(html.matches("data-detail-row=\"true\"").count(), 1);
        assert!(!html.contains("UNIT_SUMMARY_SENTINEL"));
        assert!(!html.contains("summary-raw-must-not-render.log"));
        assert!(!html.contains("summary raw"));
    }

    #[test]
    fn actionable_failures_open_by_default_but_pass_measured_and_skip_do_not() {
        let mut fail = unit_summary("unit-fail", Verdict::SetupError);
        fail.sort_key.0 = 1;
        let mut measured = unit_summary("unit-measured", Verdict::Measured);
        measured.sort_key.0 = 2;
        let mut skipped = unit_summary("unit-skip", Verdict::Skip);
        skipped.sort_key.0 = 3;
        let html = render(vec![
            unit_summary("unit-pass", Verdict::Pass),
            fail,
            measured,
            skipped,
        ]);

        assert!(html.contains("data-unit-id=\"unit-fail\" open"));
        assert!(html.contains("data-unit-id=\"unit-pass\"><summary"));
        assert!(!html.contains("data-unit-id=\"unit-pass\" open"));
        assert!(!html.contains("data-unit-id=\"unit-measured\" open"));
        assert!(!html.contains("data-unit-id=\"unit-skip\" open"));
    }

    #[test]
    fn raw_text_and_artifact_paths_are_escaped() {
        let mut detail = traffic_detail("escape-unit", (0, 0, 0, 0));
        detail.raw_log = "./iperf_outputs/a&b.log".into();
        detail.raws = vec![("client<&".into(), "<output>&".into())];
        let html = render(vec![detail, unit_summary("escape-unit", Verdict::Pass)]);

        assert!(html.contains("a&amp;b.log"));
        assert!(html.contains("client&lt;&amp;"));
        assert!(html.contains("&lt;output&gt;&amp;"));
    }

    #[test]
    fn summary_with_explicit_direction_can_render_all_primary_metrics() {
        let mut summary = unit_summary("metrics-unit", Verdict::Pass);
        summary.direction_summaries = vec![DirectionSummary {
            tag: "AB".into(),
            src: "master".into(),
            dst: "agent".into(),
            verdict: Verdict::Pass,
            reason: "OK".into(),
            streams: Some(StreamCounts {
                requested: 4,
                active: 4,
                required: 3,
            }),
            rx_avg: Some(2379.123456),
            rx_p10: Some(2300.0),
            target_mbps: Some(2200.0),
            sample_coverage: Some(0.975),
            udp_loss: Some(0.125),
            ..Default::default()
        }];
        let html = render(vec![summary]);

        assert!(html.contains("4/4/3"));
        assert!(html.contains("2379.123 Mbps"));
        assert!(html.contains("2300.000 Mbps"));
        assert!(html.contains("2200.000 Mbps"));
        assert!(html.contains("97.5%"));
        assert!(html.contains("UDP 丢包 0.125%"));
    }

    #[test]
    fn fallback_directions_prefer_group_totals_and_keep_both_legs() {
        let mut ab = traffic_detail("fallback-bidir", (0, 0, 0, 0));
        ab.kind_label = "★★双向灌包-ab".into();
        ab.rx_avg = Some(100.0);
        let mut ab_total = traffic_detail("fallback-bidir", (0, 0, 2, 1));
        ab_total.kind_label = "★组合计-ab".into();
        ab_total.is_grouptotal = true;
        ab_total.rx_avg = Some(200.0);
        let mut ba = traffic_detail("fallback-bidir", (0, 1, 0, 0));
        ba.kind_label = "★★双向灌包-ba".into();
        ba.rx_avg = Some(300.0);
        let mut summary = unit_summary("fallback-bidir", Verdict::Pass);
        summary.task = "双向 TCP".into();
        let html = render(vec![ab, ab_total, ba, summary]);

        assert!(html.contains("data-direction=\"AB\""));
        assert!(html.contains("data-direction=\"BA\""));
        assert!(html.contains("200.000 Mbps"));
        assert!(html.contains("300.000 Mbps"));
    }

    #[test]
    fn rate_reason_validation_uses_the_metrics_shown_to_the_user() {
        assert_eq!(
            validate_rate_reason(
                "RX_BELOW_TARGET: stale",
                Some(799.0),
                Some(790.0),
                Some(800.0)
            ),
            "RX_BELOW_TARGET: RX 平均 799.000 Mbps < 目标 800.000 Mbps"
        );
        assert_eq!(
            validate_rate_reason(
                "RX_P10_BELOW_TARGET: stale",
                Some(875.0),
                Some(850.0),
                Some(800.0),
            ),
            "判定原因与展示指标不一致: RX_P10_BELOW_TARGET；RX-P10 850.000 Mbps >= 目标 800.000 Mbps"
        );
        assert_eq!(
            validate_rate_reason(
                "RX_UNSTABLE: stale",
                Some(875.0),
                Some(790.0),
                Some(800.0),
            ),
            "RX_UNSTABLE: RX 平均 875.000 Mbps >= 目标 800.000 Mbps，RX-P10 790.000 Mbps < 目标 800.000 Mbps"
        );
    }

    #[test]
    fn contradictory_direction_reason_is_flagged_in_overview_and_bidir_summary() {
        let mut summary = unit_summary("contradictory-bidir", Verdict::RateFail);
        summary.task = "IPERF V4 TCP 双向".into();
        summary.direction_summaries = vec![
            DirectionSummary {
                tag: "AB".into(),
                verdict: Verdict::Pass,
                rx_avg: Some(860.0),
                rx_p10: Some(830.0),
                target_mbps: Some(800.0),
                ..Default::default()
            },
            DirectionSummary {
                tag: "BA".into(),
                verdict: Verdict::RateFail,
                reason_code: "RX_P10_BELOW_TARGET".into(),
                reason_detail: "旧汇总原因".into(),
                rx_avg: Some(875.0),
                rx_p10: Some(850.0),
                target_mbps: Some(800.0),
                ..Default::default()
            },
        ];

        let html = render(vec![summary]);

        assert!(html.contains("判定原因与展示指标不一致"));
        assert!(html.contains("RX-P10 850.000 Mbps &gt;= 目标 800.000 Mbps"));
        assert!(!html.contains("RX_P10_BELOW_TARGET: 旧汇总原因"));
    }

    #[test]
    fn explicit_ping_direction_merges_min_avg_and_max_from_detail_row() {
        let detail = Row {
            sort_key: (0, 0, 0, 0),
            parent_id: "ping-merge".into(),
            task: "PING V6".into(),
            kind_label: "PING".into(),
            verdict: Verdict::Pass,
            ping_loss: Some(0.0),
            ping_min: Some(0.75),
            ping_avg: Some(1.5),
            ping_max: Some(3.25),
            ..Default::default()
        };
        let mut summary = unit_summary("ping-merge", Verdict::Pass);
        summary.task = "PING V6".into();
        summary.direction_summaries = vec![DirectionSummary {
            tag: "单向".into(),
            verdict: Verdict::Pass,
            ping_loss: Some(0.0),
            ..Default::default()
        }];

        let html = render(vec![detail, summary]);

        assert!(html.contains("RTT 最小/平均/最大 0.750/1.500/3.250 ms"));
        assert!(html.contains("PING_OK: 丢包率 0.0%"));
    }

    #[test]
    fn raw_output_section_reports_empty_state_instead_of_looking_blank() {
        let html = render(vec![unit_summary("no-raw", Verdict::Pass)]);

        assert!(html.contains("原始输出（0 条执行记录，0 项内容，内嵌文本非空 0 段）"));
        assert!(html.contains("本次报告没有可用的内嵌原始输出、独立原始记录或网卡样本文件。"));
    }

    /// `-w` 的合法写法不止 `4m`：下发命令用的解析器接受 `2.5m` / `4mb` /
    /// `1gib`，报告这边必须用同一个，否则校验放行、命令照发，只有报告里的
    /// 「估算在途缓冲」无声消失。
    #[test]
    fn the_in_flight_estimate_accepts_every_socket_buffer_spelling_the_command_does() {
        let row = |command: &str| Row {
            command: command.into(),
            required_seconds: Some(180.0),
            ..Default::default()
        };
        for window in ["4m", "2.5m", "4mb", "1gib", "64k"] {
            let command = format!("iperf3 -c 10.0.0.2 -w {window} -P 4 -t 180");
            assert!(
                in_flight_buffer_estimate(&row(&command)).is_some(),
                "-w {window} 能下发就必须能估算"
            );
        }
        // 解析不了的写法保持沉默，不要报一个假数。
        assert!(in_flight_buffer_estimate(&row("iperf3 -c 10.0.0.2 -w 猫 -P 4")).is_none());
        assert!(in_flight_buffer_estimate(&row("iperf3 -c 10.0.0.2 -P 4")).is_none());
    }
}
