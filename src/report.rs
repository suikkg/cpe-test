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

fn screenshot_link(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("<a href=\"{}\">查看截图</a>", esc(path))
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

    let mut h = String::with_capacity(64 * 1024);
    h.push_str(
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<title>CPE 子网测试报告</title>
<style>
body { font-family: "Microsoft YaHei", "PingFang SC", sans-serif; margin: 16px; background:#fafafa; }
h1 { font-size: 20px; }
h2 { font-size: 16px; margin-top: 28px; }
table { border-collapse: collapse; width: 100%; background: #fff; font-size: 12px; }
th, td { border: 1px solid #ccc; padding: 4px 6px; text-align: left; white-space: nowrap; }
th { background: #eef2f7; position: sticky; top: 0; }
tr:nth-child(even) { background: #f7f9fb; }
td.pass { color: #0a7d28; font-weight: bold; }
td.fail { color: #c62828; font-weight: bold; }
td.warn { color:#b26a00; font-weight:bold; }
td.measured { color:#1565c0; font-weight:bold; }
td.not-evaluated { color:#6a1b9a; font-weight:bold; }
td.error { color:#b71c1c; background:#ffebee; font-weight:bold; }
td.skip { color: #888; }
tr.grouptotal td { background: #fff3cd; font-weight: bold; }
tr.unit-summary td { background:#dfefff; font-weight:bold; border-top:2px solid #6d8fb3; }
td.num { text-align: right; }
pre { background: #111; color: #d7ffd7; padding: 10px; overflow-x: auto; font-size: 12px; }
details { margin: 8px 0; }
summary { cursor: pointer; font-weight: bold; }
.meta { background:#fff; border:1px solid #ccc; padding:10px 14px; display:inline-block; }
.sum { font-size: 14px; margin: 12px 0; }
.sum b.p { color:#0a7d28; } .sum b.f { color:#c62828; }
</style></head><body>
<h1>CPE 子网测试报告</h1>
"#,
    );
    h.push_str(&format!(
        "<div class=\"meta\">主控: {} &nbsp;|&nbsp; 辅测: {} ({}) &nbsp;|&nbsp; 开始: {} &nbsp;|&nbsp; 结束: {}</div>\n",
        esc(&meta.master_pc),
        esc(&meta.agent_pc),
        esc(&meta.agent_host),
        esc(&meta.started),
        esc(&meta.finished)
    ));
    h.push_str(&format!(
        "<p class=\"sum\">测试单元: {total} &nbsp; <b class=\"p\">PASS: {pass}</b> &nbsp; <b class=\"f\">RATE_FAIL: {rate_fail}</b> &nbsp; UNSTABLE: {unstable} &nbsp; MEASURED: {measured} &nbsp; NOT_EVALUATED: {not_evaluated} &nbsp; SETUP_ERROR: {setup_error} &nbsp; SKIP: {skipped} &nbsp; 有效判定通过率: {rate:.1}% &nbsp; 耗时: {}</p>\n",
        esc(&meta.elapsed)
    ));

    h.push_str("<table>\n<tr>");
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
        "iperf 发送 Mbps",
        "iperf 接收 Mbps",
        "UDP 丢包率 %",
        "Ping 丢包率 %",
        "Ping 平均 ms",
        "主控截图",
        "辅测截图",
        "执行命令",
    ] {
        h.push_str(&format!("<th>{th}</th>"));
    }
    h.push_str("</tr>\n");

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
        h.push_str(&format!("<td>{}</td>", esc(&short8(&r.task_id))));
        h.push_str(&format!("<td>{}</td>", esc(&short8(&r.parent_id))));
        h.push_str(&format!("<td>{}</td>", esc(&r.task)));
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
        h.push_str(&format!("<td>{}</td>", esc(&r.reason_code)));
        h.push_str(&format!("<td>{}</td>", esc(&r.reason_detail)));
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
            "<td>{}</td>",
            screenshot_link(&r.screenshot_master)
        ));
        h.push_str(&format!(
            "<td>{}</td>",
            screenshot_link(&r.screenshot_agent)
        ));
        h.push_str(&format!("<td>{}</td>", esc(&r.command)));
        h.push_str("</tr>\n");
    }
    h.push_str("</table>\n");

    h.push_str("<h2>原始输出</h2>\n");
    for r in rows.iter() {
        if r.raws.is_empty() {
            continue;
        }
        h.push_str(&format!(
            "<details><summary>{} — {} [{}]</summary>\n",
            esc(&r.time),
            esc(&r.task),
            r.verdict.label()
        ));
        for (title, text) in &r.raws {
            h.push_str(&format!(
                "<h3>{}</h3><pre>{}</pre>\n",
                esc(title),
                esc(text)
            ));
        }
        h.push_str("</details>\n");
    }
    h.push_str("</body></html>\n");

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
            raws: vec![("client".into(), "<output>".into())],
            ..Default::default()
        }];
        write_report(&p, &mut rows, &ReportMeta::default()).unwrap();
        let html = std::fs::read_to_string(&p).unwrap();
        assert!(html.contains("PASS"));
        assert!(html.contains("测试单元: 1"));
        assert!(html.contains("PASS: 1"));
        assert!(html.contains("2379.123"));
        assert!(html.contains("&lt;output&gt;"));
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
        assert!(html.contains("PASS: 1"));
        assert!(!html.contains("测试单元: 21"));

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }
}
