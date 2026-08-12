//! HTML 测试报告生成（单文件、内嵌样式、含原始输出，拷走整个目录即可查看）

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verdict {
    Pass,
    RateFail,
    Unstable,
    Measured,
    #[default]
    NotEvaluated,
    SetupError,
    Skip,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::RateFail => "RATE_FAIL",
            Verdict::Unstable => "UNSTABLE",
            Verdict::Measured => "MEASURED",
            Verdict::NotEvaluated => "NOT_EVALUATED",
            Verdict::SetupError => "SETUP_ERROR",
            Verdict::Skip => "SKIP",
        }
    }

    pub fn css(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::RateFail => "fail",
            Verdict::Unstable => "warn",
            Verdict::Measured => "measured",
            Verdict::NotEvaluated => "not-evaluated",
            Verdict::SetupError => "error",
            Verdict::Skip => "skip",
        }
    }

    pub fn is_pass(self) -> bool {
        self == Verdict::Pass
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionStatus {
    #[default]
    Completed,
    Partial,
    Error,
    TimedOut,
    Cancelled,
    Skipped,
}

impl ExecutionStatus {
    pub fn label(self) -> &'static str {
        match self {
            ExecutionStatus::Completed => "COMPLETED",
            ExecutionStatus::Partial => "PARTIAL",
            ExecutionStatus::Error => "ERROR",
            ExecutionStatus::TimedOut => "TIMEOUT",
            ExecutionStatus::Cancelled => "CANCELLED",
            ExecutionStatus::Skipped => "SKIPPED",
        }
    }
}

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

fn verdict_priority(verdict: Verdict) -> u8 {
    match verdict {
        Verdict::SetupError => 7,
        Verdict::NotEvaluated => 6,
        Verdict::RateFail => 5,
        Verdict::Unstable => 4,
        Verdict::Measured => 3,
        Verdict::Skip => 2,
        Verdict::Pass => 1,
    }
}

fn group_verdict(group: &UnitGroup<'_>) -> Verdict {
    group.summary.map(|row| row.verdict).unwrap_or_else(|| {
        group
            .details
            .iter()
            .map(|row| row.verdict)
            .max_by_key(|verdict| verdict_priority(*verdict))
            .unwrap_or(Verdict::NotEvaluated)
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
        Verdict::RateFail | Verdict::Unstable | Verdict::NotEvaluated | Verdict::SetupError
    )
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
    h.push_str(
        "<section class=\"overview-section\" aria-labelledby=\"overview-heading\"><h2 id=\"overview-heading\">测试概览</h2>\n",
    );
    h.push_str(
        "<div class=\"overview-scroll\" role=\"region\" aria-labelledby=\"overview-heading\" tabindex=\"0\"><table class=\"overview-table\"><caption class=\"sr-only\">按测试单元和方向展示网卡 RX 判定指标</caption><thead><tr><th scope=\"col\">结果</th><th scope=\"col\">测试单元</th><th scope=\"col\">方向</th><th scope=\"col\">源端 → 接收端</th><th scope=\"col\">请求/活跃/要求流</th><th scope=\"col\">网卡 RX 平均</th><th scope=\"col\">网卡 RX-P10</th><th scope=\"col\">目标</th><th scope=\"col\">采样覆盖率</th><th scope=\"col\">质量指标</th><th scope=\"col\">原因</th></tr></thead><tbody>\n",
    );

    for group in groups {
        let unit_verdict = group_verdict(group);
        let execution_status = group_execution_status(group);
        let is_ping = group_is_ping(group);
        let unit_reason = group_reason(group);
        let mut directions = group_direction_summaries(group);
        if directions.is_empty() {
            directions.push(DirectionSummary {
                tag: NOT_APPLICABLE.into(),
                verdict: unit_verdict,
                ..Default::default()
            });
        }
        for direction in directions {
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
            let mut reason = direction_reason_text(&direction, &unit_reason);
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
            h.push_str(&format!(
                "<tr data-unit-id=\"{}\" data-direction=\"{}\"><td><strong class=\"status {}\">{}</strong><br><small>{}</small></td><td>{}</td><td><strong>{}</strong></td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td><td class=\"reason-cell\">{}</td></tr>\n",
                esc(&group.key),
                esc(&tag),
                direction.verdict.css(),
                direction.verdict.label(),
                execution_status.label(),
                esc(group_title(group)),
                esc(&tag),
                esc(&endpoints),
                esc(&streams_text(direction.streams)),
                esc(&rx_avg_text(direction.rx_avg, is_ping)),
                esc(&rx_p10_text(direction.rx_p10, direction.rx_avg, is_ping)),
                esc(&target_text(direction.target_mbps)),
                esc(&coverage_text(direction.sample_coverage, is_ping)),
                esc(&quality_text(
                    direction.udp_loss,
                    direction.ping_loss,
                    direction.ping_min,
                    direction.ping_avg,
                    direction.ping_max,
                    is_ping,
                )),
                esc(&reason),
            ));
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

fn diagnostic_item(h: &mut String, label: &str, value: &str) {
    h.push_str(&format!("<dt>{}</dt><dd>{}</dd>", esc(label), esc(value)));
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
    diagnostic_item(
        h,
        "流量工具 sender 汇总（诊断）",
        &diagnostic_metric(row.tx_mbps, is_ping),
    );
    diagnostic_item(
        h,
        "流量工具 receiver 汇总（诊断）",
        &diagnostic_metric(row.rx_mbps, is_ping),
    );
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
        row.transport.clone()
    } else {
        format!("{} / {}", row.transport, row.ip)
    };
    let reason = direction_reason_text(&direction_from_row(row), NOT_APPLICABLE);
    let row_class = if row.is_grouptotal {
        " class=\"grouptotal\""
    } else {
        ""
    };
    h.push_str(&format!(
        "<tr{row_class} data-detail-row=\"true\"><td><span class=\"status {}\">{}</span></td><td>{}</td><td>{}</td><td><strong>{}</strong><br><small>{}</small></td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td><td class=\"reason-cell\">{}</td><td>",
        row.verdict.css(),
        row.verdict.label(),
        esc(&row.time),
        esc(&direction),
        esc(kind),
        esc(&row.param),
        esc(&protocol),
        esc(&streams_text(stream_counts(row))),
        esc(&rx_avg_text(row.rx_avg, is_ping)),
        esc(&rx_p10_text(row.rx_p10, row.rx_avg, is_ping)),
        esc(&target_text(row.target_mbps)),
        esc(&coverage_text(row.sample_coverage, is_ping)),
        esc(&quality_text(
            row.udp_loss,
            row.ping_loss,
            row.ping_min,
            row.ping_avg,
            row.ping_max,
            is_ping,
        )),
        esc(&reason),
    ));
    push_row_diagnostics(h, row, is_ping, &format!("{group_title} {direction}"));
    h.push_str("</td></tr>\n");
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
    h.push_str(&format!(
        "<div class=\"direction-summary-row\"><span class=\"direction-summary-tag\">{}</span><span class=\"direction-summary-endpoints\">{}</span><span>接收端 RX 平均 <strong>{}</strong></span><span>RX-P10 <strong>{}</strong></span><span>目标 <strong>{}</strong></span><span class=\"direction-summary-reason\">判定：{}</span></div>",
        esc(&tag),
        esc(&direction_endpoint_text(direction)),
        esc(&rx_avg_text(direction.rx_avg, false)),
        esc(&rx_p10_text(direction.rx_p10, direction.rx_avg, false)),
        esc(&target_text(direction.target_mbps)),
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
        "<div class=\"direction-summary\" role=\"group\" aria-label=\"双向方向汇总\"><strong class=\"direction-summary-title\">双向方向汇总</strong>",
    );
    push_bidirectional_direction(h, ab, &fallback_reason);
    push_bidirectional_direction(h, ba, &fallback_reason);
    let combined = match (ab.rx_avg, ba.rx_avg) {
        (Some(ab_avg), Some(ba_avg)) => format!("{:.3} Mbps", ab_avg + ba_avg),
        _ => format!("{NOT_COLLECTED}（至少一个方向缺少 RX 平均）"),
    };
    h.push_str(&format!(
        "<p class=\"direction-summary-total\"><strong>AB + BA 接收端 RX 平均合计</strong>：{}</p></div>",
        esc(&combined),
    ));
}

fn push_unit_details(h: &mut String, groups: &[UnitGroup<'_>]) {
    let detail_count: usize = groups.iter().map(|group| group.details.len()).sum();
    h.push_str(&format!(
        "<section class=\"details-section\" aria-labelledby=\"details-heading\"><h2 id=\"details-heading\">逐行明细（{detail_count} 行）</h2><div class=\"unit-list\">\n"
    ));
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
            "<details class=\"unit-section\" data-unit-id=\"{}\"{open}><summary class=\"unit-toggle\" id=\"unit-toggle-{index}\"><span class=\"status {}\">{}</span><span class=\"unit-title\">{}</span><span class=\"unit-meta\">{} · {}</span></summary>",
            esc(&group.key),
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
                "<div class=\"table-scroll\" role=\"region\" aria-labelledby=\"unit-toggle-{index}\" tabindex=\"0\"><table class=\"results-table\"><caption class=\"sr-only\">{}的逐行执行明细</caption><thead><tr><th scope=\"col\">结果</th><th scope=\"col\">时间</th><th scope=\"col\">方向</th><th scope=\"col\">类型 / 参数</th><th scope=\"col\">传输</th><th scope=\"col\">请求/活跃/要求流</th><th scope=\"col\">网卡 RX 平均</th><th scope=\"col\">网卡 RX-P10</th><th scope=\"col\">目标</th><th scope=\"col\">采样覆盖率</th><th scope=\"col\">质量指标</th><th scope=\"col\">原因</th><th scope=\"col\">诊断详情</th></tr></thead><tbody>\n",
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
    let unstable = groups
        .iter()
        .filter(|group| group_verdict(group) == Verdict::Unstable)
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
    let judged = pass + rate_fail + unstable;
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
.overview-scroll { max-width: 100%; overflow: auto; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.overview-table, .results-table { border-collapse: separate; border-spacing: 0; width: 100%; background: var(--surface); font-size: 12px; }
.overview-table { min-width: 1120px; table-layout: fixed; }
.overview-table th, .overview-table td, .results-table th, .results-table td { border-right: 1px solid var(--line); border-bottom: 1px solid var(--line); padding: 6px 8px; text-align: left; vertical-align: top; }
.overview-table th:last-child, .overview-table td:last-child, .results-table th:last-child, .results-table td:last-child { border-right: 0; }
.overview-table th { background: var(--head); white-space: nowrap; }
.overview-table td { overflow-wrap: anywhere; }
.overview-table td.num, .results-table td.num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
.overview-table tr:last-child td, .results-table tr:last-child td { border-bottom: 0; }
.reason-cell { color: var(--muted); overflow-wrap: anywhere; }
.unit-list { display: grid; gap: 8px; }
.unit-section { min-width: 0; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.unit-toggle { display: grid; grid-template-columns: auto minmax(0, 1fr) auto; align-items: start; gap: 10px; padding: 9px 11px; cursor: pointer; font-weight: 700; }
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
.direction-summary-total { margin: 5px 0 0; }
.table-scroll { max-width: 100%; max-height: 68vh; overflow: auto; overscroll-behavior: contain; scrollbar-gutter: stable; background: var(--surface); }
.results-table { min-width: 1120px; }
.results-table th { position: sticky; top: 0; z-index: 3; background: var(--head); white-space: nowrap; }
.results-table th:first-child, .results-table td:first-child { position: sticky; left: 0; z-index: 2; background: var(--surface); box-shadow: 2px 0 0 rgba(23, 32, 42, .08); }
.results-table th:first-child { z-index: 4; background: var(--head); }
.results-table tr:nth-child(even) td { background: #fbfcfd; }
.results-table tr:nth-child(even) td:first-child { background: #fbfcfd; }
.results-table tr.grouptotal td, .results-table tr.grouptotal td:first-child { background: var(--yellow); font-weight: 700; }
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
.raw-empty { margin: 8px 0; padding: 8px 10px; border-left: 3px solid #8a5200; background: #fff8e6; color: #5e430b; }
.raw-links { margin: 8px 0; }
.raw-links a, .artifact-list a { color: #145a94; }
summary:focus-visible, a:focus-visible, .table-scroll:focus-visible, .overview-scroll:focus-visible { outline: 3px solid #2b78b8; outline-offset: 2px; }
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
    .unit-toggle { grid-template-columns: auto minmax(0, 1fr); }
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
        "<div class=\"summary-grid\"><div class=\"stat neutral\"><span class=\"stat-label\">测试单元</span><strong class=\"stat-value\">{total}</strong></div><div class=\"stat pass\"><span class=\"stat-label\">PASS</span><strong class=\"stat-value\">{pass}</strong></div><div class=\"stat fail\"><span class=\"stat-label\">RATE_FAIL</span><strong class=\"stat-value\">{rate_fail}</strong></div><div class=\"stat neutral\"><span class=\"stat-label\">UNSTABLE</span><strong class=\"stat-value\">{unstable}</strong></div><div class=\"stat measured\"><span class=\"stat-label\">MEASURED</span><strong class=\"stat-value\">{measured}</strong></div><div class=\"stat neutral\"><span class=\"stat-label\">SETUP_ERROR</span><strong class=\"stat-value\">{setup_error}</strong></div><div class=\"stat neutral\"><span class=\"stat-label\">耗时</span><strong class=\"stat-value\">{}</strong></div></div>\n",
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
                esc(text)
            ));
        }
        h.push_str("</details>\n");
    }
    h.push_str("</main></body></html>\n");

    std::fs::write(path, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static REPORT_INDEX: AtomicUsize = AtomicUsize::new(0);

    fn render(mut rows: Vec<Row>) -> String {
        let index = REPORT_INDEX.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("cpe_report_test_{}_{}", std::process::id(), index));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.html");
        write_report(&path, &mut rows, &ReportMeta::default()).unwrap();
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
        assert!(html.contains("流量工具 sender 汇总（诊断）"));
        assert!(html.contains("流量工具 receiver 汇总（诊断）"));
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
        assert!(html.contains("AB + BA 接收端 RX 平均合计</strong>：1700.000 Mbps"));
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
}
