use super::{LegRow, RunReport, UnitRow};
use std::fmt::Write;
use std::path::Path;

pub(super) fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(super) fn render(report: &RunReport) -> String {
    let mut html = String::from("<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\"><title>CPE 内环测试报告</title><style>");
    html.push_str(include_str!("report.css"));
    html.push_str(crate::report::RX_CHART_CSS);
    html.push_str("</style></head><body><main><header><h1>CPE 内环测试报告</h1>");
    let state = if report.probe_only {
        "仅能力探测，未灌包"
    } else if report.error.is_some() {
        "执行未完成"
    } else if !report.current.is_empty() {
        "测试进行中 · 当前为阶段结果"
    } else {
        "本轮已结束"
    };
    let _ = write!(
        html,
        "<p class=\"run-meta\">{} · {state}</p>",
        escape(&report.created_at)
    );
    if !report.current.is_empty() {
        let _ = write!(html, "<p>当前单元：{}</p>", escape(&report.current));
    }
    if let Some(error) = &report.error {
        let _ = write!(
            html,
            "<p class=\"error notice\">执行未完成：{}。已完成单元保留各自结果。</p>",
            escape(error)
        );
    }
    html.push_str("</header><nav aria-label=\"报告目录\"><a href=\"#overview\">测试概览</a><a href=\"#results\">结果清单</a><a href=\"#details\">单元明细</a><a href=\"#environment\">设备与配置</a></nav>");
    render_overview(&mut html, report);
    html.push_str("<section id=\"results\"><h2>结果清单</h2><p class=\"muted\">每行一个单元。点击序号查看详情。</p>");
    if report.units.is_empty() {
        html.push_str(if report.probe_only {
            "<p class=\"notice\">本次只检查设备能力，没有产生吞吐测试结果。</p>"
        } else {
            "<p class=\"notice\">尚无已完成单元，不能据此判断是否达标。</p>"
        });
    } else {
        let mut groups: Vec<(&str, &str)> = Vec::new();
        for unit in &report.units {
            if !groups.contains(&(unit.host.as_str(), unit.link.as_str())) {
                groups.push((&unit.host, &unit.link));
            }
        }
        for (host, link) in groups {
            let _ = write!(
                html,
                "<h3>{} · {}</h3>",
                escape(link),
                escape(host_label(host))
            );
            html.push_str("<div class=\"table-scroll\" tabindex=\"0\" role=\"region\" aria-label=\"网口结果清单\"><table class=\"results\"><thead><tr><th scope=\"col\">单元</th><th scope=\"col\">协议 / IP</th><th scope=\"col\">方向 / 轮次</th><th scope=\"col\">打流参数</th><th scope=\"col\">接收速率 / 门限</th><th scope=\"col\">判定</th></tr></thead><tbody>");
            for unit in report
                .units
                .iter()
                .filter(|u| u.host == host && u.link == link)
            {
                let _ = write!(html, "<tr><td><a href=\"#unit-{}\">#{}</a></td><td>{} / IPv{}</td><td>{}<small>第 {} 轮</small></td><td class=\"parameters\">{}</td><td>{}</td><td>{}<p class=\"reason\">{}</p></td></tr>", unit.index, unit.index, unit.protocol.label(), unit.ip_version, unit.direction.label(), unit.repeat, escape(&parameter_label(unit)), acceptance_summary(report, unit), unit_badge(unit), escape(&unit_explanation(unit)));
            }
            html.push_str("</tbody></table></div>");
        }
    }
    html.push_str("</section><section id=\"details\"><h2>单元明细</h2><p class=\"muted\">UDP 丢包、发送速率和曲线仅作诊断。</p>");
    for unit in &report.units {
        let _ = write!(html, "<article id=\"unit-{}\" class=\"unit\"><h3>#{unit_index} {} · {} / IPv{} · {} · 第 {} 轮 {}</h3><p class=\"parameters\">{}</p><div class=\"acceptance\">{}</div><p>{}</p>", unit.index, escape(&unit.link), unit.protocol.label(), unit.ip_version, unit.direction.label(), unit.repeat, unit_badge(unit), escape(&parameter_label(unit)), acceptance_summary(report, unit), escape(&unit_explanation(unit)), unit_index=unit.index);
        let _ = write!(
            html,
            "<p class=\"muted\">测量策略：{}。{}</p>",
            unit.measurement.label(),
            if unit.direction.is_bidir() {
                unit.overlap_secs
                    .map(|v| format!("共同有效重叠 {v:.2}s。"))
                    .unwrap_or_else(|| {
                        if unit.resumed {
                            "本轮未重新采样。".into()
                        } else {
                            "无可证明的共同测量窗口。".into()
                        }
                    })
            } else {
                String::new()
            }
        );
        if !unit.resumed && unit.legs.is_empty() {
            html.push_str("<p class=\"notice\">无测量结果，请检查起流和执行环境。</p>");
        }
        let _ = write!(
            html,
            "<p class=\"muted\">单元原因代码：{}</p>",
            escape(&unit.reason)
        );
        diagnostics(&mut html, &unit.diagnostics);
        for leg in &unit.legs {
            render_leg(&mut html, report, unit, leg);
        }
        if let Some(capture) = &unit.screenshot {
            html.push_str(&super::screenshot::render(capture));
        }
        html.push_str("</article>");
    }
    html.push_str("</section><section id=\"environment\"><h2>设备与配置</h2>");
    if let Some(capability) = &report.capability {
        let _ = write!(
            html,
            "<details><summary>设备能力与接口</summary><pre>{}</pre></details>",
            escape(&serde_json::to_string_pretty(capability).unwrap_or_default())
        );
    }
    let _ = write!(html, "<details><summary>本次配置</summary><pre>{}</pre></details></section><footer>上行采 CPE RX；下行采电脑网卡 RX。</footer></main></body></html>", escape(&serde_json::to_string_pretty(&report.config).unwrap_or_default()));
    html
}

fn host_label(host: &str) -> &str {
    if host == "master" {
        "主控本机"
    } else {
        host
    }
}

fn verdict_label(verdict: &str) -> &str {
    match verdict {
        "PASS" => "达标",
        "RATE_FAIL" => "未达标",
        "MEASURED" => "仅测量",
        "NOT_EVALUATED" => "无法评价",
        "SETUP_ERROR" => "执行失败",
        "SKIP" => "跳过",
        _ => "未知状态",
    }
}
fn badge(verdict: &str) -> String {
    let tone = match verdict {
        "PASS" => "pass",
        "RATE_FAIL" => "fail",
        "MEASURED" => "measured",
        "NOT_EVALUATED" => "unknown",
        "SETUP_ERROR" => "error",
        _ => "unknown",
    };
    format!(
        "<span class=\"badge {tone}\">{} <small>{}</small></span>",
        verdict_label(verdict),
        escape(verdict)
    )
}
fn unit_badge(unit: &UnitRow) -> String {
    if unit.resumed {
        "<span class=\"badge resumed\">复用历史 PASS</span>".into()
    } else {
        badge(&unit.verdict)
    }
}
fn parameter_label(unit: &UnitRow) -> String {
    match unit.protocol {
        super::config::Protocol::Tcp => format!(
            "{} 条并发流；TCP 窗口 {}（-P {} / -w {}）",
            unit.parameters.streams,
            unit.parameters.tcp_window.as_deref().unwrap_or("系统默认"),
            unit.parameters.streams,
            unit.parameters.tcp_window.as_deref().unwrap_or("默认")
        ),
        super::config::Protocol::Udp => format!(
            "{} 条并发流；每流 {} Mbps；包长 {}（-P {} / -b {} Mbps / -l {}）",
            unit.parameters.streams,
            rate(unit.parameters.udp_mbps),
            unit.parameters.udp_length.as_deref().unwrap_or("工具默认"),
            unit.parameters.streams,
            rate(unit.parameters.udp_mbps),
            unit.parameters.udp_length.as_deref().unwrap_or("默认")
        ),
    }
}
fn unit_explanation(unit: &UnitRow) -> String {
    if unit.resumed {
        return "复用 24 小时内的 PASS，本轮未重新测试。".into();
    }
    let prefix = match unit.verdict.as_str() {
        "PASS" => "接收速率达到门限。",
        "RATE_FAIL" => "接收速率低于门限。",
        "MEASURED" => "未设门限，仅记录速率。",
        "NOT_EVALUATED" => "无有效验收结果。",
        "SETUP_ERROR" => "起流或执行环境失败。",
        _ => "请查看本单元记录的判定原因。",
    };
    if unit.detail.is_empty() {
        prefix.into()
    } else {
        format!("{prefix} {}", unit.detail)
    }
}
/// 只解释已记录的判定；配置仅用来说明双向缺少有效合计时的验收方式。
fn uses_total(report: &RunReport, unit: &UnitRow) -> bool {
    unit.direction.is_bidir()
        && (unit.total_target_mbps.is_some()
            || unit.bidir_targets.is_some()
            || report
                .config
                .links
                .iter()
                .find(|link| link.host == unit.host && link.name == unit.link)
                .is_some_and(|link| {
                    let link = link.for_protocol(unit.protocol);
                    link.bidir_total_min_mbps.is_some()
                        || (link.measurement.uses_tool()
                            && link.tool_bidir_total_min_mbps.is_some())
                }))
}
fn total_target_description(report: &RunReport, unit: &UnitRow) -> String {
    if unit.total_target_mbps.is_some() {
        return target(unit.total_target_mbps);
    }
    if let Some(t) = &unit.bidir_targets {
        return format!(
            "未能应用（配置网卡合计 {}；工具合计 {}）",
            target(t.nic_mbps),
            target(t.tool_mbps)
        );
    }
    if let Some(link) = report
        .config
        .links
        .iter()
        .find(|link| link.host == unit.host && link.name == unit.link)
    {
        let link = link.for_protocol(unit.protocol);
        return format!(
            "未能应用（配置网卡合计 {}；工具合计 {}）",
            target(link.bidir_total_min_mbps),
            target(link.tool_bidir_total_min_mbps)
        );
    }
    "未能应用".into()
}
fn acceptance_summary(report: &RunReport, unit: &UnitRow) -> String {
    if unit.resumed {
        return "<p class=\"muted\">本轮未重新测试</p>".into();
    }
    let mut out = String::new();
    if uses_total(report, unit) {
        let source = unit
            .legs
            .first()
            .filter(|first| {
                unit.legs.len() == 2
                    && first.source != super::measure::Source::None
                    && unit.legs.iter().all(|leg| leg.source == first.source)
            })
            .map(|leg| leg.source.label())
            .unwrap_or("未形成同来源合计");
        let _ = write!(out, "<p class=\"total\"><strong>双向合计 {} Mbps</strong> / 合计门限 {}</p><small>按两端 RX 合计判定一次 · {}</small>", rate(unit.total_mbps), total_target_description(report, unit), source);
    } else if unit.direction.is_bidir() {
        out.push_str("<p><strong>上下行分别判定</strong>，未设置双向合计门限</p>");
    }
    for leg in &unit.legs {
        let _ = write!(out, "<p class=\"direction-rate\"><strong>{} {} Mbps</strong> / {}<small>{} · 接收端 {} {}</small></p>", leg.flow.label(), rate(leg.mbps), if uses_total(report, unit) { "仅测量，按合计验收".into() } else { if leg.source == super::measure::Source::None { "未形成有效验收".into() } else { format!("门限 {}", target(leg.target_mbps)) } }, leg.source.label(), escape(host_label(&leg.receiver_host)), escape(&leg.receiver));
    }
    if unit.legs.is_empty() {
        out.push_str("<p class=\"muted\">无测量结果</p>");
    }
    out
}
fn render_overview(html: &mut String, report: &RunReport) {
    let count = |label: &str| {
        report
            .units
            .iter()
            .filter(|unit| !unit.resumed && unit.verdict == label)
            .count()
    };
    let resumed = report.units.iter().filter(|unit| unit.resumed).count();
    let pass = count("PASS");
    let fail = count("RATE_FAIL");
    html.push_str("<section id=\"overview\"><h2>测试概览</h2><dl class=\"totals\">");
    for (label, value) in [
        ("已记录单元", report.units.len()),
        ("本轮达标", pass),
        ("未达标", fail),
        ("仅测量", count("MEASURED")),
        ("无法评价", count("NOT_EVALUATED")),
        ("执行失败", count("SETUP_ERROR")),
        ("复用历史", resumed),
    ] {
        let _ = write!(html, "<div><dt>{label}</dt><dd>{value}</dd></div>");
    }
    html.push_str("</dl>");
    if pass + fail > 0 {
        let _ = write!(html, "<p>本轮吞吐验收通过率 <strong>{:.1}%</strong>（{pass} / {}）。只统计本轮 PASS 与 RATE_FAIL；仅测量、无法评价、执行失败和历史复用不计入。</p>", pass as f64 * 100.0 / (pass+fail) as f64, pass+fail);
    } else {
        html.push_str("<p>本轮暂无吞吐验收结论。</p>");
    }
    if let Some(plan) = &report.plan {
        let _ = write!(html, "<p class=\"muted\">计划 {} 条网口 / {} 个单元（双向 {} 个）/ {} 个传输方向；报告已记录 {} 个单元。</p>", plan.links, plan.units, plan.bidir_units, plan.legs, report.units.len());
        if !plan.skipped.is_empty() {
            let _ = write!(
                html,
                "<p class=\"muted\">未参与本轮：{}（配置已保留）</p>",
                escape(&plan.skipped.join("、"))
            );
        }
    }
    html.push_str("<details class=\"reading-guide\"><summary>如何读这份报告</summary><ul><li>按网口、协议和方向查看接收速率与门限。</li><li>上行采 CPE RX；下行采电脑网卡 RX。</li><li>网卡和工具门限独立。采样不可信时才可使用工具兜底。</li><li>未设门限：只测量；未获取：无测量值；无法评价：无有效验收结果。</li><li>双向按同来源 RX 合计判定；未设合计门限时按上下行分别判定。</li></ul></details></section>");
}
fn diagnostics(html: &mut String, values: &[String]) {
    if values.is_empty() {
        return;
    }
    html.push_str("<details class=\"diagnostics\"><summary>诊断信息</summary><ul>");
    for value in values {
        let _ = write!(html, "<li>{}</li>", escape(value));
    }
    html.push_str("</ul></details>");
}
fn render_leg(html: &mut String, report: &RunReport, unit: &UnitRow, leg: &LegRow) {
    let _ = write!(html, "<details class=\"leg-detail\"><summary>{} · 接收端 {} {} · {} Mbps · {}</summary><p><strong>数据方向：{}</strong>；接收端 {} {}，端口 {}。</p>", leg.flow.label(), escape(host_label(&leg.receiver_host)), escape(&leg.receiver), rate(leg.mbps), if uses_total(report, unit) { "仅测量，按合计判定".into() } else { format!("{}（{}）", verdict_label(&leg.verdict), escape(&leg.verdict)) }, if leg.flow.receiver_is_board() { "电脑 → CPE" } else { "CPE → 电脑" }, escape(host_label(&leg.receiver_host)), escape(&leg.receiver), leg.port);
    let _ = write!(
        html,
        "<p>采用来源：<strong>{}</strong>；接收速率 {} Mbps；{}。</p><p>{}</p>",
        leg.source.label(),
        rate(leg.mbps),
        if uses_total(report, unit) {
            "按双向合计验收".into()
        } else {
            format!("验收门限 {}", target(leg.target_mbps))
        },
        escape(&leg.detail)
    );
    if let Some(reason) = &leg.fallback_reason {
        let _ = write!(
            html,
            "<p class=\"notice\">工具兜底原因：{}</p>",
            escape(reason)
        );
    }
    html.push_str("<h4>两套测量口径</h4><div class=\"table-scroll\" tabindex=\"0\"><table><thead><tr><th scope=\"col\">口径</th><th scope=\"col\">接收速率 Mbps</th><th scope=\"col\">门限 / 判定</th><th scope=\"col\">来源说明</th></tr></thead><tbody>");
    let _ = write!(html, "<tr><th scope=\"row\">网卡 RX</th><td class=\"num\">{}</td><td>{} / {}</td><td>{}</td></tr><tr><th scope=\"row\">工具接收</th><td class=\"num\">{}</td><td>{}</td><td>{}</td></tr></tbody></table></div>", rate(leg.nic_rx_mbps), target(leg.nic_target_mbps), badge(&leg.nic_verdict), leg.counter_source.map(|s| escape(s.label())).unwrap_or_else(|| "电脑网卡计数".into()), rate(leg.tool.receiver_mbps), if leg.source == super::measure::Source::Tool && !uses_total(report, unit) { format!("采用工具口径 / 门限 {}", target(leg.target_mbps)) } else { "参考值，采用来源见判定依据".into() }, escape(&leg.tool.receiver_note));
    let _ = write!(html, "<h4>采样质量与诊断</h4><dl class=\"quality\"><div><dt>有效测量时长</dt><dd>{:.2}s / {}s</dd></div><div><dt>采样覆盖率</dt><dd>{:.1}%</dd></div><div><dt>背景扣除</dt><dd>{:.2} Mbps</dd></div><div><dt>工具发送（仅诊断）</dt><dd>{} Mbps</dd></div><div><dt>UDP 丢包（仅诊断）</dt><dd>{}</dd></div></dl><p class=\"muted\">接收端 RX 分布 Mbps：P10 {} · 中位 {} · P95 {} · 最小 {} · 最大 {}；滚动窗口覆盖 {:.1}%，计数器零增长占比 {:.1}%。</p>", leg.effective_secs, leg.required_secs, leg.coverage*100.0, leg.background_mbps, rate(leg.tool.sender_mbps), loss(leg, unit), rate(leg.rx.p10_mbps), rate(leg.rx.median_mbps), rate(leg.rx.p95_mbps), rate(leg.rx.min_mbps), rate(leg.rx.max_mbps), leg.rx.rolling_coverage*100.0, leg.rx.stalled_ratio*100.0);
    if let Some(samples) = &leg.rx_samples {
        let chart = crate::report::render_monitor_rx_chart(
            &samples.samples,
            "接收端网卡原始 RX（含背景流量，仅供诊断）",
        );
        if !chart.is_empty() {
            let _ = write!(html, "<figure>{chart}<figcaption>原始 RX 曲线，仅作诊断。含背景和起流阶段；断口为无效采样。验收使用有效窗口平均速率。</figcaption></figure>");
        }
    }
    diagnostics(html, &leg.diagnostics);
    let _ = write!(html, "<p class=\"muted\">原因代码：{}；网卡独立判定：{}。</p><details class=\"raw\"><summary>原始输出：{} iperf3 client / {} iperf3 server</summary><h4>客户端命令与输出</h4><pre>{}\n{}</pre><h4>接收端 server 输出</h4><pre>{}</pre></details></details>", escape(&leg.reason), escape(&leg.nic_reason), if leg.flow.receiver_is_board() { "PC 侧" } else { "板侧" }, if leg.flow.receiver_is_board() { "板侧" } else { "PC 侧" }, escape(&leg.client.cmd), escape(&leg.client.output), escape(if leg.server_log.trim().is_empty() { "（接收端 server 未产生输出）" } else { &leg.server_log }));
}
fn rate(value: Option<f64>) -> String {
    value
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "未获取".into())
}
fn target(value: Option<f64>) -> String {
    value
        .map(|v| format!("{v:.2} Mbps"))
        .unwrap_or_else(|| "未设门限".into())
}

/// 丢包只有拿到 receiver 汇总行才印数字，缺项明确为未知。
fn loss(leg: &LegRow, unit: &UnitRow) -> String {
    match (
        unit.protocol.is_udp(),
        leg.tool.udp_loss_pct,
        leg.tool.udp_lost_datagrams,
        leg.tool.udp_total_datagrams,
    ) {
        (false, ..) => "—（不适用）".into(),
        (true, Some(pct), Some(lost), Some(total)) => format!("{pct:.3}%<br>{lost}/{total}"),
        (true, ..) => "未知".into(),
    }
}

/// 重产物（`result.json` + `report.html`）两次重写之间的最小间隔。
///
/// 这两样都是**整份**重写：结果带着每条腿的逐秒 rx_samples，报告内联每条腿的
/// 完整客户端输出。按单元写的话，第 N 个单元写的量是第 1 个的 N 倍，一轮下来
/// 总工作量随单元数平方增长。
///
/// 按时间节流而不是按单元数：单元本身要跑几分钟时，每单元都会触发一次重写，
/// 和以前没有区别；只有单元很快、数量很多——正是平方项真正咬人的场景——才会
/// 攒着一起写。更新只在单元边界检查：下一单元很长时，下载的报告可能落后
/// 超过 30 秒；`units.jsonl` 保留尚未进入重产物的已完成单元。
const HEAVY_WRITE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// 单元之间的落盘。
///
/// 每次都写的只有两样便宜的：把**新增**的单元追加进 `units.jsonl`（一次一条，
/// 与已完成数量无关），以及体积恒定的 `summary.json`（历史列表要读它，不该为了
/// 显示一行摘要去解析带逐秒采样的 `result.json`）。
///
/// `result.json` 和 `report.html` 按 [`HEAVY_WRITE_INTERVAL`] 节流。
/// `config.json` 从第一次写完就再没变过，只在 [`save`] 里写。
pub(super) fn save_progress(
    dir: &Path,
    report: &RunReport,
    appended: &mut usize,
    last_heavy: &mut Option<std::time::Instant>,
) -> Result<(), String> {
    if let Some(new_units) = report.units.get(*appended..) {
        if !new_units.is_empty() {
            use std::io::Write;
            let units_path = dir.join("units.jsonl");
            if let Ok(metadata) = std::fs::symlink_metadata(&units_path) {
                if !metadata.file_type().is_file() {
                    return Err(format!("写入 {} 失败：不是普通文件", units_path.display()));
                }
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(units_path)
                .map_err(|e| e.to_string())?;
            for unit in new_units {
                let line = serde_json::to_string(unit).map_err(|e| e.to_string())?;
                writeln!(file, "{line}").map_err(|e| e.to_string())?;
            }
            *appended = report.units.len();
        }
    }
    let summary =
        serde_json::to_vec_pretty(&super::history::summarize(report)).map_err(|e| e.to_string())?;
    write_atomic(&dir.join("summary.json"), &summary)?;

    let due = last_heavy.is_none_or(|at| at.elapsed() >= HEAVY_WRITE_INTERVAL);
    if due {
        write_heavy(dir, report)?;
        *last_heavy = Some(std::time::Instant::now());
    }
    Ok(())
}

fn write_heavy(dir: &Path, report: &RunReport) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
    write_atomic(&dir.join("result.json"), &json)?;
    write_atomic(&dir.join("report.html"), render(report).as_bytes())
}

pub(super) fn save(dir: &Path, report: &RunReport) -> Result<(), String> {
    // 收尾必写：节流只作用于跑动过程中的中间态。
    write_heavy(dir, report)?;
    let summary =
        serde_json::to_vec_pretty(&super::history::summarize(report)).map_err(|e| e.to_string())?;
    write_atomic(&dir.join("summary.json"), &summary)?;
    // 令牌不进文件：AgentConfig::token 是 skip_serializing。
    let config = serde_json::to_vec_pretty(&serde_json::json!({
        "kind": super::config::PROJECT_KIND,
        "version": super::config::PROJECT_VERSION,
        "config": &report.config,
    }))
    .map_err(|e| e.to_string())?;
    write_atomic(&dir.join("config.json"), &config)
}

/// 同目录完整写入后替换，下载与历史读取只能看到完整的旧版或新版。
/// 内环执行互斥，每个运行目录只有一个写入方。
pub(super) fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let temp = path.with_extension("pending");
    // 临时名是固定的；删除旧临时项后用 create_new 建立新文件，既不跟随
    // 符号链接，也不覆盖可能指向别处的硬链接。竞争时宁可本次旁路写入失败，
    // 也不能把报告数据写到运行目录之外。
    let _ = std::fs::remove_file(&temp);
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(data)
        })
        .and_then(|()| replace_file(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

/// 覆盖式替换。**两个平台都用 `std::fs::rename`，不要在这里手写 `MoveFileExW`。**
///
/// 这里曾经有一份手写的 `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING)`，理由写的是
/// 「Windows 对已有目标返回 AlreadyExists」。那条前提是**假的**：std 在
/// `library/std/src/sys/fs/windows.rs` 里做的第一件事就是同一个
/// `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING)`，逐字一样。
///
/// 手写版真正的区别是**少了 std 的兜底**：`MoveFileExW` 返回
/// `ERROR_ACCESS_DENIED` 时，std 会改用
/// `SetFileInformationByHandle(FileRenameInfoEx)`，带
/// `FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS` 重试；
/// POSIX 语义那一位正好能在**目标仍被别人打开着**的时候完成替换。
///
/// 代价不是抽象的：内环每跑完一个单元都要重写 `report.html` / `result.json`，
/// 而这些文件恰恰会被人下载、被浏览器打开、被杀软扫描。手写版在 Windows CI 上
/// 就是这么挂的——`os error 5` 即 `ERROR_ACCESS_DENIED`，写进度直接失败。
///
/// 同样的假前提在 `master::executor::db::save` 上出现过一次，这是第二次。
/// 由 `no_hand_rolled_move_file_ex_in_the_tree` 看着，别再写第三次。
fn replace_file(temp: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::rename(temp, path)
}
