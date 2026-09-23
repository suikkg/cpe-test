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
    let mut html = String::from("<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\"><title>CPE 内环测速</title><style>body{font:16px/1.6 'Segoe UI',sans-serif;max-width:1240px;margin:32px auto;padding:0 20px;color:#172033}table{border-collapse:collapse;width:100%}th,td{padding:10px;border-bottom:1px solid #ccc;text-align:left}th{white-space:nowrap}pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#f4f5f7;padding:16px}details{margin:16px 0}.error{color:#a32020}.table{overflow:auto}.num{text-align:right}.leg{color:#4a5568}</style><h1>CPE 内环测速</h1>");
    let _ = write!(
        html,
        "<p>{} · {}</p><p>PC 被测网卡 ↔ 板侧 LAN 地址；ADB 负责控制及板侧采样。\
         接收速率优先取可信的接收接口字节计数；配置了兜底策略时才改用工具 receiver 汇总，\
         并单独标注来源。工具口径与网卡口径的门限互相独立，工具数字不会套用网卡门限。</p>",
        escape(&report.created_at),
        if report.probe_only {
            "仅能力探测，未灌包"
        } else {
            "按网口顺序串行执行，只有双向单元内部并发"
        }
    );
    if let Some(preview) = &report.plan {
        let _ = write!(
            html,
            "<p>本轮计划：{} 条网口 · {} 个单元（其中双向 {} 个）· {} 条数据腿 · 预估 {} 分钟。{}</p>",
            preview.links,
            preview.units,
            preview.bidir_units,
            preview.legs,
            preview.estimated_secs.div_ceil(60),
            if preview.skipped.is_empty() {
                String::new()
            } else {
                format!("未参与本轮：{}（配置已保留）", escape(&preview.skipped.join("、")))
            }
        );
    }
    if let Some(error) = &report.error {
        let _ = write!(html, "<p class=\"error\">执行未完成：{}</p>", escape(error));
    }
    html.push_str("<div class=\"table\"><table><tr><th>#</th><th>电脑 / 网口</th><th>协议</th><th>方向</th><th>轮次</th><th>接收端</th><th>速率 Mbps</th><th>来源</th><th>门限 Mbps</th><th>网卡 RX</th><th>工具接收</th><th>UDP 丢包</th><th>判定</th></tr>");
    for unit in &report.units {
        let span = unit.legs.len().max(1);
        // 一条腿都没有的单元也要有一行。`assemble_unit` 专门为这种情况留了
        // `UnitDirectionResultMissing` 的判定，而它同时计入 `summary.units` 和
        // `summary.not_evaluated`；只按 legs 渲染的话，那个单元在表里根本不出现，
        // 计数和看得见的行数对不上，还没有任何一处解释差在哪。
        if unit.legs.is_empty() {
            let _ = write!(
                html,
                "<tr><td class=\"num\">{}</td><td>{}<br>{}</td><td>IPv{} / {}</td><td>{}</td><td class=\"num\">{}</td><td colspan=\"7\">本单元没有产生任何一条腿的结果</td><td><strong>{}</strong><br>{}</td></tr>",
                unit.index,
                escape(&unit.host),
                escape(&unit.link),
                unit.ip_version,
                unit.protocol.label(),
                unit.direction.label(),
                unit.repeat,
                escape(&unit.verdict),
                escape(&unit.reason)
            );
            continue;
        }
        for (index, leg) in unit.legs.iter().enumerate() {
            html.push_str("<tr>");
            if index == 0 {
                let _ = write!(
                    html,
                    "<td rowspan=\"{span}\">{}</td><td rowspan=\"{span}\">{}<br>{}</td><td rowspan=\"{span}\">IPv{} / {}</td><td rowspan=\"{span}\">{}</td><td rowspan=\"{span}\" class=\"num\">{}</td>",
                    unit.index,
                    escape(&unit.host),
                    escape(&unit.link),
                    unit.ip_version,
                unit.protocol.label(),
                    unit.direction.label(),
                    unit.repeat
                );
            }
            let _ = write!(
                html,
                "<td class=\"leg\">{} · {} {}</td><td class=\"num\">{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>",
                leg.flow.label(),
                escape(&leg.receiver_host),
                escape(&leg.receiver),
                rate(leg.mbps),
                escape(leg.source.label()),
                rate(leg.target_mbps),
                rate(leg.nic_rx_mbps),
                rate(leg.tool.receiver_mbps),
                loss(leg, unit)
            );
            if index == 0 {
                let _ = write!(
                    html,
                    "<td rowspan=\"{span}\"><strong>{}</strong><br>{}</td>",
                    escape(&unit.verdict),
                    escape(&unit.reason)
                );
            }
            html.push_str("</tr>");
        }
    }
    html.push_str("</table></div>");
    for unit in &report.units {
        let _ = write!(
            html,
            "<details><summary>#{} {} / {} · IPv{} / {} · {} · 第 {} 轮 · {}</summary><p>{}: {}</p>",
            unit.index,
            escape(&unit.host),
            escape(&unit.link),
            unit.ip_version,
                unit.protocol.label(),
            unit.direction.label(),
            unit.repeat,
            escape(&unit.verdict),
            escape(&unit.reason),
            escape(&unit.detail)
        );
        let _ = write!(
            html,
            "<p>测量策略：{}。{}</p>",
            escape(unit.measurement.label()),
            match (unit.total_mbps, unit.total_target_mbps, unit.overlap_secs) {
                (Some(total), target, overlap) => format!(
                    "双向合计 {total:.3} Mbps，门限 {}；两条腿共同有效重叠 {}。",
                    rate(target),
                    overlap
                        .map(|value| format!("{value:.2}s"))
                        .unwrap_or_else(|| "无".into())
                ),
                (None, _, Some(overlap)) =>
                    format!("双向两条腿共同有效重叠 {overlap:.2}s；未形成同来源合计。"),
                _ => String::new(),
            }
        );
        if !unit.diagnostics.is_empty() {
            let _ = write!(html, "<pre>{}</pre>", escape(&unit.diagnostics.join("\n")));
        }
        for leg in &unit.legs {
            render_leg(&mut html, leg);
        }
        html.push_str("</details>");
    }
    if let Some(capability) = &report.capability {
        let _ = write!(
            html,
            "<details open><summary>设备能力与接口</summary><pre>{}</pre></details>",
            escape(&serde_json::to_string_pretty(capability).unwrap_or_default())
        );
    }
    let _ = write!(
        html,
        "<details><summary>本次配置</summary><pre>{}</pre></details></html>",
        escape(&serde_json::to_string_pretty(&report.config).unwrap_or_default())
    );
    html
}

fn render_leg(html: &mut String, leg: &LegRow) {
    let _ = write!(
        html,
        "<h4>{} 腿 · 端口 {} · 接收端 {} {}</h4><p>{}: {}</p>",
        leg.flow.label(),
        leg.port,
        escape(&leg.receiver_host),
        escape(&leg.receiver),
        escape(&leg.reason),
        escape(&leg.detail)
    );
    let _ = write!(
        html,
        "<p>采用来源：{}{}。网卡口径独立留存：RX 平均 {} Mbps，验收 {}（{}），门限 {}。工具口径：接收 {} Mbps（{}），发送 {} Mbps（仅诊断）。</p>",
        escape(leg.source.label()),
        leg.counter_source
            .map(|source| format!("（读取路径 {}）", escape(source.label())))
            .unwrap_or_default(),
        rate(leg.nic_rx_mbps),
        escape(&leg.nic_verdict),
        escape(&leg.nic_reason),
        rate(leg.nic_target_mbps),
        rate(leg.tool.receiver_mbps),
        escape(&leg.tool.receiver_note),
        rate(leg.tool.sender_mbps)
    );
    let _ = write!(
        html,
        "<p>接收端 RX 分布 Mbps：P10 {} · 中位 {} · P95 {} · 最小 {} · 最大 {}；滚动窗口覆盖 {:.1}%，计数器零增长占比 {:.1}%。</p><p>有效时长 {:.2}s / 配置 {}s；采样覆盖 {:.1}%；背景扣除 {:.2} Mbps。</p>",
        rate(leg.rx.p10_mbps),
        rate(leg.rx.median_mbps),
        rate(leg.rx.p95_mbps),
        rate(leg.rx.min_mbps),
        rate(leg.rx.max_mbps),
        leg.rx.rolling_coverage * 100.0,
        leg.rx.stalled_ratio * 100.0,
        leg.effective_secs,
        leg.required_secs,
        leg.coverage * 100.0,
        leg.background_mbps
    );
    let _ = write!(html, "<pre>{}</pre>", escape(&leg.diagnostics.join("\n")));
    let _ = write!(
        html,
        "<p>{} iperf3 client：</p><pre>{}\n{}</pre><p>{} iperf3 server：</p><pre>{}</pre>",
        if leg.flow.receiver_is_board() {
            "PC 侧"
        } else {
            "板侧"
        },
        escape(&leg.client.cmd),
        escape(&leg.client.output),
        if leg.flow.receiver_is_board() {
            "板侧"
        } else {
            "PC 侧"
        },
        escape(if leg.server_log.trim().is_empty() {
            "（接收端 server 未产生输出）"
        } else {
            &leg.server_log
        })
    );
}

fn rate(value: Option<f64>) -> String {
    value
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "未设置 / 未获取".into())
}

/// 丢包只有拿到 receiver 汇总行才印数字。拿不到就印「未知」而不是 0%——
/// 后者会让人以为这一档 UDP 是无损转发的。
fn loss(leg: &LegRow, unit: &UnitRow) -> String {
    match (
        unit.protocol.is_udp(),
        leg.tool.udp_loss_pct,
        leg.tool.udp_lost_datagrams,
        leg.tool.udp_total_datagrams,
    ) {
        (false, ..) => "—".into(),
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
