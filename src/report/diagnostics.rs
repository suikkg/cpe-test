//! 明细行下面那块诊断信息。
//!
//! 与判定无关：这里的一切都是给排查的人看的线索，不参与 PASS/FAIL。

use super::*;

pub(super) fn diagnostic_metric(value: Option<f64>, is_ping: bool) -> String {
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
pub(super) fn in_flight_buffer_estimate(row: &Row) -> Option<String> {
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

pub(super) fn diagnostic_item(h: &mut String, label: &str, value: &str) {
    h.push_str(&format!("<dt>{}</dt><dd>{}</dd>", esc(label), esc(value)));
}

pub(super) fn diagnostic_availability(row: &Row) -> String {
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
    if !row.nic_samples_rx.is_empty() {
        available.push("接收端网卡样本".into());
    }
    if !row.nic_samples_tx.is_empty() {
        available.push("发送端网卡样本".into());
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

pub(super) fn push_row_diagnostics(h: &mut String, row: &Row, is_ping: bool, aria_context: &str) {
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
        artifact_link(&row.nic_samples_rx, "接收端逐样本 CSV"),
        artifact_link(&row.nic_samples_tx, "发送端逐样本 CSV"),
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
