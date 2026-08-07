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
    pub ping_avg: Option<f64>,
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
            "<figure class=\"shot\"><a href=\"{path}\" target=\"_blank\" rel=\"noopener\" title=\"查看截图\" aria-label=\"打开{label}原图\"><img src=\"{path}\" alt=\"{label}缩略图\" loading=\"lazy\" decoding=\"async\"><span>查看原图</span></a></figure>"
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

fn fmt_f(v: Option<f64>, prec: usize) -> String {
    match v {
        Some(x) => format!("{x:.prec$}"),
        None => String::new(),
    }
}

fn representative_row<'a>(summary: &Row, rows: &'a [Row]) -> Option<&'a Row> {
    let parent_id = if summary.parent_id.is_empty() {
        &summary.task_id
    } else {
        &summary.parent_id
    };
    let matches_unit = |row: &&Row| {
        !row.is_unit_summary
            && (!parent_id.is_empty() && row.parent_id == *parent_id
                || !summary.task_id.is_empty() && row.parent_id == summary.task_id)
    };
    rows.iter()
        .filter(matches_unit)
        .find(|row| row.is_grouptotal)
        .or_else(|| rows.iter().find(matches_unit))
}

fn overview_metrics(row: &Row) -> String {
    let mut metrics = Vec::new();
    if let Some(value) = row.rx_avg.or(row.rx_mbps) {
        metrics.push(format!("RX {value:.3} Mbps"));
    }
    if let Some(value) = row.tx_avg.or(row.tx_mbps) {
        metrics.push(format!("TX {value:.3} Mbps"));
    }
    if let Some(value) = row.udp_loss {
        metrics.push(format!("UDP 丢包 {value:.3}%"));
    }
    if let Some(value) = row.ping_loss {
        metrics.push(format!("Ping 丢包 {value:.1}%"));
    }
    if let Some(value) = row.ping_avg {
        metrics.push(format!("RTT {value:.1} ms"));
    }
    if let Some(value) = row.sample_coverage {
        metrics.push(format!("覆盖率 {:.1}%", value * 100.0));
    }
    if metrics.is_empty() {
        "—".into()
    } else {
        metrics.join(" · ")
    }
}

pub fn write_report(path: &Path, rows: &mut [Row], meta: &ReportMeta) -> std::io::Result<()> {
    rows.sort_by_key(|row| row.sort_key);
    let unit_rows: Vec<&Row> = rows.iter().filter(|r| r.is_unit_summary).collect();
    let total = unit_rows
        .iter()
        .filter(|r| r.verdict != Verdict::Skip)
        .count();
    let pass = unit_rows
        .iter()
        .filter(|r| r.verdict == Verdict::Pass)
        .count();
    let rate_fail = unit_rows
        .iter()
        .filter(|r| r.verdict == Verdict::RateFail)
        .count();
    let unstable = unit_rows
        .iter()
        .filter(|r| r.verdict == Verdict::Unstable)
        .count();
    let measured = unit_rows
        .iter()
        .filter(|r| r.verdict == Verdict::Measured)
        .count();
    let not_evaluated = unit_rows
        .iter()
        .filter(|r| r.verdict == Verdict::NotEvaluated)
        .count();
    let setup_error = unit_rows
        .iter()
        .filter(|r| r.verdict == Verdict::SetupError)
        .count();
    let skipped = unit_rows
        .iter()
        .filter(|r| r.verdict == Verdict::Skip)
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
:root { color-scheme: light; --ink:#17202a; --muted:#5f6b76; --line:#d8dee4; --surface:#fff; --canvas:#f4f6f8; --blue:#dfefff; --yellow:#fff3cd; }
* { box-sizing: border-box; }
body { margin: 0; padding: 20px; color: var(--ink); background: var(--canvas); font-family: "Microsoft YaHei", "PingFang SC", sans-serif; font-size: 14px; line-height: 1.45; }
.report { max-width: 1800px; margin: 0 auto; }
h1 { margin: 0 0 12px; font-size: 22px; line-height: 1.25; }
h2 { margin: 28px 0 10px; font-size: 17px; line-height: 1.3; }
.meta { display: grid; grid-template-columns: repeat(5, minmax(0, 1fr)); gap: 1px; margin: 0 0 14px; overflow: hidden; border: 1px solid var(--line); border-radius: 6px; background: var(--line); }
.meta-item { min-width: 0; padding: 9px 12px; background: var(--surface); }
.meta-label { display: block; color: var(--muted); font-size: 11px; }
.meta-value { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-weight: 600; }
.summary-grid { display: grid; grid-template-columns: repeat(7, minmax(100px, 1fr)); gap: 8px; margin: 0 0 8px; }
.stat { min-height: 70px; padding: 9px 11px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.stat-label { display: block; color: var(--muted); font-size: 11px; }
.stat-value { display: block; margin-top: 3px; font-size: 20px; font-weight: 700; line-height: 1.1; }
.stat.pass .stat-value { color: #087f3e; }
.stat.fail .stat-value { color: #bd2c2c; }
.stat.measured .stat-value { color: #1769aa; }
.stat.neutral .stat-value { color: #394854; }
.summary-note { margin: 8px 0 0; color: var(--muted); }
.summary-note strong { color: var(--ink); }
.section-toggle { display: block; padding: 9px 11px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); cursor: pointer; font-weight: 700; }
.section-toggle::marker { color: #1769aa; }
.overview-scroll, .table-scroll { max-width: 100%; overflow: auto; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.overview-table, .results-table { border-collapse: separate; border-spacing: 0; width: 100%; background: var(--surface); font-size: 12px; }
.overview-table { table-layout: fixed; }
.overview-table th, .overview-table td, .results-table th, .results-table td { border-right: 1px solid var(--line); border-bottom: 1px solid var(--line); padding: 6px 8px; text-align: left; vertical-align: top; }
.overview-table th:last-child, .overview-table td:last-child, .results-table th:last-child, .results-table td:last-child { border-right: 0; }
.overview-table th { background: #edf2f6; }
.overview-table th:nth-child(1) { width: 14%; }
.overview-table th:nth-child(2) { width: 38%; }
.overview-table th:nth-child(3) { width: 28%; }
.overview-table th:nth-child(4) { width: 20%; }
.overview-table td.task-overview { overflow-wrap: anywhere; }
.overview-table td.metric-overview { color: #263746; }
.overview-table td.reason-overview { overflow-wrap: anywhere; color: var(--muted); }
.overview-table tr:last-child td { border-bottom: 0; }
.results-section { margin-top: 12px; }
.results-section[open] > .section-toggle { border-bottom-left-radius: 0; border-bottom-right-radius: 0; }
.results-section[open] > .table-scroll { border-top: 0; border-top-left-radius: 0; border-top-right-radius: 0; }
.results-table { min-width: 2200px; }
.results-table th { position: sticky; top: 0; z-index: 2; background: #edf2f6; white-space: nowrap; }
.results-table th:first-child { left: 0; z-index: 3; }
.results-table tr:nth-child(even) { background: #fbfcfd; }
.results-table td { white-space: nowrap; }
.results-table td.task-cell { max-width: 390px; overflow: hidden; text-overflow: ellipsis; }
.results-table td.reason-cell { max-width: 320px; overflow: hidden; text-overflow: ellipsis; }
.results-table td.command-cell { max-width: 440px; overflow: hidden; text-overflow: ellipsis; }
.results-table td.num { text-align: right; font-variant-numeric: tabular-nums; }
td.pass { color: #087f3e; font-weight: 700; }
td.fail { color: #bd2c2c; font-weight: 700; }
td.warn { color: #9a5b00; font-weight: 700; }
td.measured { color: #1769aa; font-weight: 700; }
td.not-evaluated { color: #7542a8; font-weight: 700; }
td.error { color: #a42121; background: #ffebee; font-weight: 700; }
td.skip { color: #6b747c; }
tr.grouptotal td { background: var(--yellow); font-weight: 700; }
tr.unit-summary td { background: var(--blue); font-weight: 700; border-top: 2px solid #6d8fb3; }
.shot { display: inline-flex; flex-direction: column; gap: 3px; min-width: 126px; margin: 0; }
.shot a { display: inline-flex; flex-direction: column; align-items: flex-start; gap: 3px; color: #145a94; font-weight: 600; text-decoration: none; }
.shot img { display: block; width: 120px; height: 68px; border: 1px solid #b8c2cb; border-radius: 4px; background: #eef2f5; object-fit: cover; }
.shot a:hover img, .shot a:focus-visible img { border-color: #1769aa; outline: 2px solid #9dc6e8; outline-offset: 1px; }
pre { max-height: 420px; margin: 8px 0 0; padding: 10px; overflow: auto; border-radius: 4px; background: #182027; color: #d7ffd7; font-size: 12px; line-height: 1.4; }
details.raw-section { margin: 8px 0; }
details.raw-section > summary { cursor: pointer; font-weight: 700; }
.raw-links { margin: 8px 0; }
.raw-links a { color: #145a94; }
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
    .overview-table { min-width: 620px; }
    .results-table { min-width: 2200px; }
}
@media print {
    body { padding: 0; background: #fff; }
    .table-scroll, .overview-scroll { overflow: visible; border: 0; }
    .results-table { min-width: 0; font-size: 8px; }
    .results-table th, .results-table td { padding: 2px 3px; }
    .command-cell, .raw-section, .shot { display: none; }
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

    h.push_str("<section class=\"overview-section\"><h2>测试概览</h2>\n");
    h.push_str(
        "<div class=\"overview-scroll\"><table class=\"overview-table\" aria-label=\"测试单元概览\"><thead><tr><th>结果</th><th>测试单元</th><th>核心指标</th><th>原因</th></tr></thead><tbody>\n",
    );
    for summary in &unit_rows {
        let detail = representative_row(summary, rows);
        let summary_metrics = overview_metrics(summary);
        let metrics = if summary_metrics == "—" {
            detail.map(overview_metrics).unwrap_or(summary_metrics)
        } else {
            summary_metrics
        };
        let reason = if !summary.reason_code.is_empty() || !summary.reason_detail.is_empty() {
            [summary.reason_code.as_str(), summary.reason_detail.as_str()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(": ")
        } else {
            detail
                .map(|row| {
                    [row.reason_code.as_str(), row.reason_detail.as_str()]
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(": ")
                })
                .unwrap_or_default()
        };
        let reason = if reason.is_empty() {
            "—".to_string()
        } else {
            reason
        };
        h.push_str(&format!(
            "<tr><td class=\"{}\"><strong>{}</strong><br><small>{}</small></td><td class=\"task-overview\" title=\"{}\">{}</td><td class=\"metric-overview\">{}</td><td class=\"reason-overview\" title=\"{}\">{}</td></tr>\n",
            summary.verdict.css(),
            summary.verdict.label(),
            summary.execution_status.label(),
            esc(&summary.task),
            esc(&summary.task),
            esc(&metrics),
            esc(&reason),
            esc(&reason),
        ));
    }
    h.push_str("</tbody></table></div></section>\n");

    h.push_str(&format!(
        "<details class=\"results-section\" open><summary class=\"section-toggle\">逐行明细（{} 行）</summary><div class=\"table-scroll\"><table class=\"results-table\" aria-label=\"逐行测试明细\"><thead><tr>",
        rows.len()
    ));
    for th in [
        "时间",
        "Task ID",
        "Parent ID",
        "任务",
        "IP",
        "传输",
        "参数",
        "源 PC",
        "源接口",
        "源 IP",
        "目标 PC",
        "目标接口",
        "目标 IP",
        "结果",
        "执行状态",
        "原因码",
        "原因详情",
        "类型",
        "请求/活跃/要求流",
        "重试",
        "目标 Mbps",
        "TX均值",
        "TX-P10",
        "接收网卡平均 Mbps",
        "RX-P10",
        "RX中位",
        "RX-P95",
        "RX最低",
        "RX最高",
        "有效/要求秒",
        "采样覆盖率",
        "对向接收 Mbps",
        "后端发送 Mbps",
        "后端接收 Mbps",
        "UDP 丢包率 %",
        "Ping 丢包率 %",
        "Ping 平均 ms",
        "主控截图",
        "辅测截图",
        "执行命令",
    ] {
        h.push_str(&format!("<th>{th}</th>"));
    }
    h.push_str("</tr></thead><tbody>\n");

    for r in rows.iter() {
        let cls = if r.is_unit_summary {
            " class=\"unit-summary\""
        } else if r.is_grouptotal {
            " class=\"grouptotal\""
        } else {
            ""
        };
        h.push_str(&format!("<tr{cls}>"));
        h.push_str(&format!("<td>{}</td>", esc(&r.time)));
        h.push_str(&format!(
            "<td title=\"{}\">{}</td>",
            esc(&r.task_id),
            esc(&short8(&r.task_id))
        ));
        h.push_str(&format!(
            "<td title=\"{}\">{}</td>",
            esc(&r.parent_id),
            esc(&short8(&r.parent_id))
        ));
        h.push_str(&format!(
            "<td class=\"task-cell\" title=\"{}\">{}</td>",
            esc(&r.task),
            esc(&r.task)
        ));
        h.push_str(&format!("<td>{}</td>", esc(&r.ip)));
        h.push_str(&format!("<td>{}</td>", esc(&r.transport)));
        h.push_str(&format!("<td>{}</td>", esc(&r.param)));
        h.push_str(&format!("<td>{}</td>", esc(&r.src_pc)));
        h.push_str(&format!("<td>{}</td>", esc(&r.src_iface)));
        h.push_str(&format!("<td>{}</td>", esc(&r.src_ip)));
        h.push_str(&format!("<td>{}</td>", esc(&r.dst_pc)));
        h.push_str(&format!("<td>{}</td>", esc(&r.dst_iface)));
        h.push_str(&format!("<td>{}</td>", esc(&r.dst_ip)));
        h.push_str(&format!(
            "<td class=\"{}\">{}</td>",
            r.verdict.css(),
            r.verdict.label()
        ));
        h.push_str(&format!("<td>{}</td>", r.execution_status.label()));
        h.push_str(&format!(
            "<td class=\"reason-cell\" title=\"{}\">{}</td>",
            esc(&r.reason_code),
            esc(&r.reason_code)
        ));
        h.push_str(&format!(
            "<td class=\"reason-cell\" title=\"{}\">{}</td>",
            esc(&r.reason_detail),
            esc(&r.reason_detail)
        ));
        h.push_str(&format!("<td>{}</td>", esc(&r.kind_label)));
        h.push_str(&format!(
            "<td class=\"num\">{}/{}/{}</td>",
            r.requested_streams, r.active_streams, r.required_streams
        ));
        h.push_str(&format!("<td class=\"num\">{}</td>", r.retry_count));
        h.push_str(&format!(
            "<td class=\"num\">{}</td>",
            fmt_f(r.target_mbps, 3)
        ));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.tx_avg, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.tx_p10, 3)));
        h.push_str(&format!(
            "<td class=\"num\"><b>{}</b></td>",
            fmt_f(r.rx_avg, 3)
        ));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.rx_p10, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.rx_median, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.rx_p95, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.rx_min, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.rx_max, 3)));
        h.push_str(&format!(
            "<td class=\"num\">{}/{}</td>",
            fmt_f(r.effective_seconds, 1),
            fmt_f(r.required_seconds, 1)
        ));
        h.push_str(&format!(
            "<td class=\"num\">{}</td>",
            r.sample_coverage
                .map(|v| format!("{:.1}%", v * 100.0))
                .unwrap_or_default()
        ));
        h.push_str(&format!("<td class=\"num\">{}</td>", esc(&r.peer_rx)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.tx_mbps, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.rx_mbps, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.udp_loss, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.ping_loss, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.ping_avg, 1)));
        h.push_str(&format!(
            "<td class=\"artifact-cell\">{}</td>",
            screenshot_link(&r.screenshot_master, "主控截图")
        ));
        h.push_str(&format!(
            "<td class=\"artifact-cell\">{}</td>",
            screenshot_link(&r.screenshot_agent, "辅测截图")
        ));
        h.push_str(&format!(
            "<td class=\"command-cell\" title=\"{}\">{}</td>",
            esc(&r.command),
            esc(&r.command)
        ));
        h.push_str("</tr>\n");
    }
    h.push_str("</tbody></table></div></details>\n");

    h.push_str("<h2>原始输出</h2>\n");
    for r in rows.iter() {
        if r.raws.is_empty() && r.raw_log.is_empty() && r.nic_samples.is_empty() {
            continue;
        }
        h.push_str(&format!(
            "<details class=\"raw-section\"><summary>{} — {} [{}]</summary>\n",
            esc(&r.time),
            esc(&r.task),
            r.verdict.label()
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

fn short8(s: &str) -> String {
    s.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_report() {
        let dir = std::env::temp_dir().join(format!("cpe_report_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("r.html");
        let mut rows = vec![Row {
            time: "2026-07-04 12:00:00".into(),
            task: "IPERF V4 TCP".into(),
            verdict: Verdict::Pass,
            execution_status: ExecutionStatus::Completed,
            is_unit_summary: true,
            rx_avg: Some(2379.123456),
            raw_log: "./iperf_outputs/iperf_tcp.log".into(),
            raws: vec![("client".into(), "<output>".into())],
            ..Default::default()
        }];
        write_report(&p, &mut rows, &ReportMeta::default()).unwrap();
        let html = std::fs::read_to_string(&p).unwrap();
        assert!(html.contains("PASS"));
        assert!(html.contains("测试单元: 1"));
        assert!(html.contains(
            "<span class=\"stat-label\">PASS</span><strong class=\"stat-value\">1</strong>"
        ));
        assert!(html.contains("测试概览"));
        assert!(html.contains("逐行明细（1 行）"));
        assert!(html.contains("判定通过率"));
        assert!(html.contains("2379.123"));
        assert!(html.contains("&lt;output&gt;"));
        assert!(html.contains("./iperf_outputs/iperf_tcp.log"));
        assert!(html.contains("独立原始记录"));
        assert!(html.contains("后端发送 Mbps"));
        assert!(html.contains("后端接收 Mbps"));
        assert!(!html.contains("iperf 发送 Mbps"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_report_counts_unit_summary_instead_of_flow_details() {
        let dir =
            std::env::temp_dir().join(format!("cpe_report_unit_count_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("r.html");
        let mut rows: Vec<Row> = (0..20)
            .map(|idx| Row {
                sort_key: (0, 0, idx, 0),
                task: format!("UDP 流 #{}", idx + 1),
                verdict: Verdict::Pass,
                execution_status: ExecutionStatus::Completed,
                requested_streams: 1,
                active_streams: 1,
                required_streams: 1,
                ..Default::default()
            })
            .collect();
        rows.push(Row {
            sort_key: (0, 0, 21, 1),
            task: "UDP 20 流测试单元".into(),
            verdict: Verdict::Pass,
            execution_status: ExecutionStatus::Completed,
            is_unit_summary: true,
            requested_streams: 20,
            active_streams: 20,
            required_streams: 18,
            ..Default::default()
        });

        write_report(&p, &mut rows, &ReportMeta::default()).unwrap();
        let html = std::fs::read_to_string(&p).unwrap();
        assert!(html.contains("测试单元: 1"));
        assert!(html.contains(
            "<span class=\"stat-label\">PASS</span><strong class=\"stat-value\">1</strong>"
        ));
        assert!(!html.contains("测试单元: 21"));

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn screenshot_thumbnail_and_text_link_to_original_image() {
        let html = screenshot_link("./iperf_outputs/shot&1.png", "主控截图");

        assert!(html.contains("<figure class=\"shot\">"));
        assert!(html.contains("<a href=\"./iperf_outputs/shot&amp;1.png\""));
        assert!(html.contains("<img src=\"./iperf_outputs/shot&amp;1.png\""));
        assert!(html.contains("target=\"_blank\""));
        assert!(html.contains("rel=\"noopener\""));
        assert!(html.contains("title=\"查看截图\""));
        assert!(html.contains("查看原图"));
        assert_eq!(html.matches("./iperf_outputs/shot&amp;1.png").count(), 2);
        assert!(screenshot_link("", "主控截图").is_empty());
    }

    #[test]
    fn bidirectional_error_report_keeps_ab_ba_and_summary_rows() {
        let dir =
            std::env::temp_dir().join(format!("cpe_report_bidir_rows_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("r.html");
        let mut rows = vec![
            Row {
                sort_key: (0, usize::MAX, usize::MAX, u8::MAX),
                task: "双向 TCP".into(),
                verdict: Verdict::SetupError,
                execution_status: ExecutionStatus::Error,
                kind_label: "测试单元汇总(双向)".into(),
                is_unit_summary: true,
                ..Default::default()
            },
            Row {
                sort_key: (0, 1, 0, 0),
                task: "双向 TCP".into(),
                verdict: Verdict::Pass,
                execution_status: ExecutionStatus::Completed,
                kind_label: "★★双向灌包-ba".into(),
                rx_avg: Some(500.0),
                ..Default::default()
            },
            Row {
                sort_key: (0, 0, 0, 0),
                task: "双向 TCP".into(),
                verdict: Verdict::SetupError,
                execution_status: ExecutionStatus::Error,
                reason_code: "LEG_THREAD_PANIC".into(),
                kind_label: "★★双向灌包-ab".into(),
                ..Default::default()
            },
        ];

        write_report(&p, &mut rows, &ReportMeta::default()).unwrap();
        let html = std::fs::read_to_string(&p).unwrap();
        assert_eq!(html.matches("★★双向灌包-ab").count(), 1);
        assert_eq!(html.matches("★★双向灌包-ba").count(), 1);
        assert_eq!(html.matches("测试单元汇总(双向)").count(), 1);
        assert!(html.contains("LEG_THREAD_PANIC"));
        assert!(html.contains("<b>500.000</b>"));

        let ab = html.find("★★双向灌包-ab").unwrap();
        let ba = html.find("★★双向灌包-ba").unwrap();
        let summary = html.find("测试单元汇总(双向)").unwrap();
        assert!(ab < ba && ba < summary);

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }
}
