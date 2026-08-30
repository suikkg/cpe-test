//! 单元格级别的文本与 HTML 片段。
//!
//! 全是纯函数：给一个值，给一段文本。放在一起是为了让「报告里这个数字为什么
//! 长这样」有唯一的去处。

use super::*;

pub(super) fn screenshot_link(path: &str, label: &str) -> String {
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
pub(super) fn status_label_html(verdict: Verdict) -> String {
    // 标签都是 ASCII 常量，不含需要转义的字符。
    verdict.label().replace('_', "_<wbr>")
}

/// 概览用的紧凑截图缩略图：与接收速率同一行，不必展开诊断面板。
pub(super) fn overview_shot(path: &str, label: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let path = esc(path);
    let label = esc(label);
    format!(
        "<a class=\"shot-mini\" href=\"{path}\" target=\"_blank\" rel=\"noopener\" title=\"查看{label}原图\" aria-label=\"打开{label}原图\"><img src=\"{path}\" alt=\"{label}缩略图\" loading=\"lazy\" decoding=\"async\"><span>{label}</span></a>"
    )
}

pub(super) fn overview_shot_cell(master: &str, agent: &str) -> String {
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

pub(super) fn artifact_link(path: &str, label: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("<a href=\"{}\">{}</a>", esc(path), esc(label))
    }
}

pub(super) fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

pub(super) fn rx_avg_text(value: Option<f64>, is_ping: bool) -> String {
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

/// 发送端 TX 平均，格式规则与 RX 完全相同（ping 无此概念记 N/A，灌包没采到
/// 记「未采集」）。单独开一个名字只是让调用点读得出这一列是 TX。
pub(super) fn tx_avg_text(value: Option<f64>, is_ping: bool) -> String {
    rx_avg_text(value, is_ping)
}

pub(super) fn rx_p10_text(value: Option<f64>, rx_avg: Option<f64>, is_ping: bool) -> String {
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

pub(super) fn target_text(value: Option<f64>) -> String {
    value.map_or_else(|| NOT_APPLICABLE.into(), |value| format!("{value:.3} Mbps"))
}

pub(super) fn coverage_text(value: Option<f64>, is_ping: bool) -> String {
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
pub(super) fn tool_rate_text(tx_mbps: Option<f64>, rx_mbps: Option<f64>) -> String {
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

pub(super) fn streams_text(value: Option<StreamCounts>) -> String {
    value.map_or_else(
        || NOT_APPLICABLE.into(),
        |counts| format!("{}/{}/{}", counts.requested, counts.active, counts.required),
    )
}

pub(super) fn quality_text(
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

/// 解析 `-w` 尺寸后缀。
///
/// 复用下发命令时用的那一个解析器，而不是在这里另写一遍：报告里这行估算
/// 必须和实际下发的 socket buffer 说的是同一个数。自带一份简化版会在
/// `2.5m`、`4mb` 这类合法写法上解析失败——校验放行、命令照发，只有报告里
/// 的估算无声消失。
pub(super) fn parse_iperf_size(value: &str) -> Option<u64> {
    crate::cmd::ctstraffic::parse_size_bytes(value)
        .ok()
        .map(u64::from)
}

/// 超长的原始输出取头尾两段。
///
/// 掐头去尾而不是只留开头：iperf3 和 ctsTraffic 的**汇总行在最后**，
/// 而那正是最常要看的一段；只留开头等于把结论截掉。
pub(super) fn embedded_raw(text: &str) -> String {
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

pub(super) fn raw_anchor(row: &Row) -> String {
    format!(
        "raw-{}-{}-{}-{}",
        row.sort_key.0, row.sort_key.1, row.sort_key.2, row.sort_key.3
    )
}

pub(super) fn nonempty_raw_count(row: &Row) -> usize {
    row.raws
        .iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .count()
}

/// 流明细行的网卡指标为空时，说明去向而不是留一个光秃秃的「未采集」。
/// 组合计行和 Ping 行有自己的真实取值/口径，不套用。
pub(super) fn nic_cell(row: &Row, is_ping: bool, rendered: String) -> String {
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
pub(super) fn transport_display(transport: &str) -> String {
    match transport {
        "" => String::new(),
        t if t.starts_with("CTS/") => {
            format!("ctsTraffic {}", t.trim_start_matches("CTS/"))
        }
        t => format!("iperf3 {t}"),
    }
}

pub(super) fn direction_endpoint_text(direction: &DirectionSummary) -> String {
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
