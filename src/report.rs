//! HTML 测试报告生成（单文件、内嵌样式、含原始输出，拷走整个目录即可查看）

use std::path::Path;

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
    /// None = 跳过
    pub ok: Option<bool>,
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
    rows.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));
    let total = rows.iter().filter(|r| !r.is_grouptotal).count();
    let pass = rows
        .iter()
        .filter(|r| !r.is_grouptotal && r.ok == Some(true))
        .count();
    let fail = rows
        .iter()
        .filter(|r| !r.is_grouptotal && r.ok == Some(false))
        .count();
    let rate = if total > 0 {
        pass as f64 * 100.0 / total as f64
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
td.skip { color: #888; }
tr.grouptotal td { background: #fff3cd; font-weight: bold; }
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
        "<p class=\"sum\">总计: {total} &nbsp; <b class=\"p\">PASS: {pass}</b> &nbsp; <b class=\"f\">FAIL: {fail}</b> &nbsp; 通过率: {rate:.1}% &nbsp; 耗时: {}</p>\n",
        esc(&meta.elapsed)
    ));

    h.push_str("<table>\n<tr>");
    for th in [
        "时间", "Task ID", "Parent ID", "任务", "IP", "传输", "参数", "源 PC", "源接口",
        "源 IP", "目标 PC", "目标接口", "目标 IP", "结果", "类型", "接收网卡平均 Mbps",
        "对向接收 Mbps", "iperf 发送 Mbps", "iperf 接收 Mbps", "UDP 丢包率 %",
        "Ping 丢包率 %", "Ping 平均 ms", "主控截图", "辅测截图", "执行命令",
    ] {
        h.push_str(&format!("<th>{th}</th>"));
    }
    h.push_str("</tr>\n");

    for r in rows.iter() {
        let cls = if r.is_grouptotal {
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
        match r.ok {
            Some(true) => h.push_str("<td class=\"pass\">PASS</td>"),
            Some(false) => h.push_str("<td class=\"fail\">FAIL</td>"),
            None => h.push_str("<td class=\"skip\">SKIP</td>"),
        }
        h.push_str(&format!("<td>{}</td>", esc(&r.kind_label)));
        h.push_str(&format!(
            "<td class=\"num\"><b>{}</b></td>",
            fmt_f(r.rx_avg, 3)
        ));
        h.push_str(&format!("<td class=\"num\">{}</td>", esc(&r.peer_rx)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.tx_mbps, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.rx_mbps, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.udp_loss, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.ping_loss, 3)));
        h.push_str(&format!("<td class=\"num\">{}</td>", fmt_f(r.ping_avg, 1)));
        h.push_str(&format!("<td>{}</td>", screenshot_link(&r.screenshot_master)));
        h.push_str(&format!("<td>{}</td>", screenshot_link(&r.screenshot_agent)));
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
            match r.ok {
                Some(true) => "PASS",
                Some(false) => "FAIL",
                None => "SKIP",
            }
        ));
        for (title, text) in &r.raws {
            h.push_str(&format!("<h3>{}</h3><pre>{}</pre>\n", esc(title), esc(text)));
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
        let dir = std::env::temp_dir().join("cpe_report_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("r.html");
        let mut rows = vec![Row {
            time: "2026-07-04 12:00:00".into(),
            task: "IPERF V4 TCP".into(),
            ok: Some(true),
            rx_avg: Some(2379.123456),
            raws: vec![("client".into(), "<output>".into())],
            ..Default::default()
        }];
        write_report(&p, &mut rows, &ReportMeta::default()).unwrap();
        let html = std::fs::read_to_string(&p).unwrap();
        assert!(html.contains("PASS"));
        assert!(html.contains("2379.123"));
        assert!(html.contains("&lt;output&gt;"));
        let _ = std::fs::remove_file(&p);
    }
}
