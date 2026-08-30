//! HTML 测试报告生成（单文件、内嵌样式、含原始输出，拷走整个目录即可查看）

use crate::reason::ReasonCode;
use crate::verdict::{aggregate_verdict, disposition_advice};
pub use crate::verdict::{ExecutionStatus, Verdict};
use std::path::Path;

const NOT_APPLICABLE: &str = "—（不适用）";
/// 网卡计数器是按接口（即按方向）采的，拆不到单条流上：一个方向无论跑 1 条还是
/// 20 条流，都只有一个网卡 RX 序列，只挂在该方向的组合计行。流明细行的网卡列
/// 因此永远是空的——但空着不解释会被读成「这条流没测到」，必须写明去哪看。
const NIC_ON_GROUPTOTAL: &str = "—（按方向统计，见组合计行）";
const NOT_COLLECTED: &str = "未采集";
const INSUFFICIENT_SAMPLES: &str = "样本不足";

/// 概览按 Ping / 灌包性能(UDP、TCP) 分节，每节一张表。
///
/// 分节而不是加一列「协议」：读报告的人是按「这次 UDP 怎么样」来找的，
/// 而不是先扫完 120 行再自己过滤。空分类不出现，见 [`sectioned`]。
fn push_overview(h: &mut String, groups: &[UnitGroup<'_>]) {
    h.push_str(
        "<section class=\"overview-section\" aria-labelledby=\"overview-heading\">\
         <details class=\"top-section\" open><summary class=\"top-toggle\">\
         <h2 id=\"overview-heading\">测试概览</h2></summary>\n",
    );
    for (section, picked) in sectioned(groups) {
        h.push_str(&format!(
            "<h3 class=\"section-heading\" id=\"overview-{}\">{}（{} 个单元）</h3>\n",
            section.anchor(),
            esc(section.title()),
            picked.len()
        ));
        push_overview_table(h, &picked);
    }
    h.push_str("</details></section>\n");
}

fn push_overview_table(h: &mut String, groups: &[&UnitGroup<'_>]) {
    // 先把方向汇总物化一遍：既用来决定是否需要「截图」列，也避免在渲染循环里
    // 重复推导。整表宽度按“常见 1440 宽屏不横向滚动”来配，判定原因单独占一行。
    let rendered: Vec<(&UnitGroup<'_>, Verdict, Vec<DirectionSummary>)> = groups
        .iter()
        .map(|group| {
            let group = *group;
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
                let advice = disposition_advice(ReasonCode::parse_prefix(&reason))
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
    h.push_str("</tbody></table></div>\n");
}

/// 「展开全部 / 收起全部」。
///
/// 报告是单文件、离线打开的，所以这段脚本必须内嵌且不依赖任何外部资源。
/// 折叠本身用的是原生 `<details>`——**脚本挂了也不影响逐块手动开合**，
/// 这里只是给一次性全开/全关加一个入口：一轮 120 个单元时，逐块点是不可行的。
const EXPAND_COLLAPSE_SCRIPT: &str = r#"<script>
(function () {
  var buttons = document.querySelectorAll('[data-toggle-all]');
  if (!buttons.length) return;
  Array.prototype.forEach.call(buttons, function (button) {
    button.addEventListener('click', function () {
      var open = button.getAttribute('data-toggle-all') === 'open';
      var all = document.querySelectorAll('details');
      Array.prototype.forEach.call(all, function (node) { node.open = open; });
    });
  });
})();
</script>
"#;

/// 内嵌进报告的单段原始输出上限（字符）。
///
/// 一条 `-P 8 -i 1 -t 180` 的 iperf3 流会打出一千多行；一次 120 单元的运行
/// 把每条流的 client + server 输出全文内嵌，单个 HTML 能涨到几十 MB。
/// 而**同一份文本已经作为 `raw_log` 单独落盘**并在上面给了链接——
/// 内嵌这一份是给「点开就看」用的，不是存档。
const EMBEDDED_RAW_MAX_CHARS: usize = 20_000;

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
        "<section class=\"details-section\" aria-labelledby=\"details-heading\">\
         <details class=\"top-section\" open><summary class=\"top-toggle\">\
         <h2 id=\"details-heading\">逐行明细（{detail_count} 行）</h2></summary>"
    ));
    if report_mixes_traffic_backends(groups) {
        h.push_str(
            "<p class=\"backend-note\">本次报告同时包含 iperf3 与 ctsTraffic 两种后端。二者的 UDP 语义不等价：iperf3 以 <code>-b</code> 恒定速率发送，ctsTraffic 使用 MediaStream 模型（每秒 FrameRate 帧、每帧再拆成 datagram），突发形态与排队行为不同。<strong>同一条链路上两者的 RX 曲线可以明显不同，不应直接互比</strong>；各自与自己的目标比较才有意义。</p>",
        );
    }
    for (section, picked) in sectioned(groups) {
        h.push_str(&format!(
            "<h3 class=\"section-heading\" id=\"details-{}\">{}（{} 个单元）</h3>\n",
            section.anchor(),
            esc(section.title()),
            picked.len()
        ));
        push_unit_list(h, &picked);
    }
    h.push_str("</details></section>\n");
}

fn push_unit_list(h: &mut String, groups: &[&UnitGroup<'_>]) {
    h.push_str("<div class=\"unit-list\">\n");
    for group in groups {
        let group = *group;
        // 分节之后每节的序号都从 0 起，拿它做 DOM id 会撞车；unit key 本来
        // 就是全局唯一的。
        let index = &group.key;
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
    h.push_str("</div>\n");
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
/* 分节标题：比 h2 轻，但要能一眼把 Ping / UDP / TCP 三块切开。 */
.section-heading { margin: 18px 0 8px; padding-left: 9px; border-left: 3px solid #1769aa; font-size: 15px; line-height: 1.3; }
.section-heading:first-of-type { margin-top: 10px; }
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
/* 顶层区块折叠：<summary> 里直接放 <h2>，标题本身就是开关。
   h2 默认是 block，放进 summary 会把三角标记挤到上一行，所以改成 inline。 */
details.top-section > summary.top-toggle { cursor: pointer; list-style-position: outside; }
details.top-section > summary.top-toggle > h2 { display: inline; }
details.top-section > summary.top-toggle::marker { color: #1769aa; }
.report-tools { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin: 14px 0 4px; }
.report-tools button { padding: 5px 12px; border: 1px solid var(--line); border-radius: 4px;
    background: var(--surface); color: inherit; font: inherit; font-size: 13px; cursor: pointer; }
.report-tools button:hover { border-color: #1769aa; color: #145a94; }
.tools-hint { color: var(--muted); font-size: 12px; }
.raw-seq { color: var(--muted); font-variant-numeric: tabular-nums; margin-right: 8px; }
.raw-dir { display: inline-block; margin-right: 8px; padding: 0 6px; border-radius: 3px;
    background: var(--panel-2); color: var(--muted); font-size: 11px; font-weight: 400; white-space: nowrap; }
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
    /* 「展开全部/收起全部」是交互控件，纸上没有意义。 */
    .report-tools { display: none; }
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

    h.push_str(
        "<div class=\"report-tools\"><button type=\"button\" data-toggle-all=\"open\">展开全部</button>\
         <button type=\"button\" data-toggle-all=\"close\">收起全部</button>\
         <span class=\"tools-hint\">对本页所有可折叠区块生效（测试概览 / 逐行明细 / 每个单元 / 原始输出）</span></div>\n",
    );
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
        "<section class=\"raw-output-section\" aria-labelledby=\"raw-heading\">\
         <details class=\"top-section\" open><summary class=\"top-toggle\">\
         <h2 id=\"raw-heading\">原始输出（{} 条执行记录，{raw_item_count} 项内容，内嵌文本非空 {raw_nonempty_count} 段）</h2>\
         </summary>\n",
        raw_rows.len(),
    ));
    if raw_rows.is_empty() {
        h.push_str(
            "<p class=\"raw-empty\">本次报告没有可用的内嵌原始输出、独立原始记录或网卡样本文件。</p>\n",
        );
    }
    for r in &raw_rows {
        let file_count =
            usize::from(!r.raw_log.is_empty()) + usize::from(!r.nic_samples.is_empty());
        // 序号必须是**单元执行序号**，和「测试概览」「逐行明细」的 `#N` 以及
        // 控制台的 `[N/总数]` 是同一个数（都来自 `sort_key.0 + 1`）。
        //
        // 不能用「在本段里排第几」：这一段列的是**执行行**不是单元，一个双向
        // 单元会出两行，于是 5 个单元能排到 #10——三块的编号各说各的，
        // 而这三块存在的全部意义就是让人拿着一个号在它们之间来回对。
        //
        // 同一单元会出多条：双向两条腿，每条腿还分「流明细」和「组合计」。
        // 用 `kind_label` 当区分标——它就是「逐行明细」那张表里「类型」列的
        // 同一个串（`灌包-ab(流明细)` / `组合计-ab`），方向也在里面。只标
        // AB/BA 的话，一个双向单元会出现四条一模一样的标题，只有行尾的
        // 「N 段内嵌输出 · M 个原始文件」不同——那不是给人扫的。
        let row_kind = if r.kind_label.is_empty() {
            infer_direction_tag(r)
        } else {
            r.kind_label.clone()
        };
        h.push_str(&format!(
            "<details class=\"raw-section\" id=\"{}\"><summary><span class=\"raw-seq\">#{}</span><span class=\"raw-dir\">{}</span>{} — {} [{}] · {} 段内嵌输出（非空 {}） · {file_count} 个原始文件</summary>\n",
            raw_anchor(r),
            r.sort_key.0.saturating_add(1),
            esc(&row_kind),
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
    h.push_str("</details></section>\n");
    h.push_str(EXPAND_COLLAPSE_SCRIPT);
    h.push_str("</main></body></html>\n");

    std::fs::write(path, h)
}

mod diagnostics;
mod format;
mod model;
mod reason;

// 对外只露报告的数据模型和两个组装文本的入口；其余是渲染内部的事。
pub use format::report_endpoint;
pub use model::{DirectionSummary, ReportMeta, Row, StreamCounts};
pub use reason::report_reason;

use diagnostics::*;
use format::*;
use model::*;
use reason::*;

#[cfg(test)]
mod tests;
