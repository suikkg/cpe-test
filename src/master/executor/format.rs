//! 报告与日志里的小格式化件。
//!
//! 单独成模块只有一个理由：它们**不依赖任何执行状态**。留在 executor 里会让
//! 「这个函数会不会碰到进程/端口/HTTP」这个问题每次都要重新读一遍才能回答。

/// 在 `os` 这台机器上执行的命令里，v6 link-local 要不要带 `%zone`。
///
/// 只看**执行命令的那一端**：Windows 的 iperf3 / ping 不接受 `%xx` 写法，
/// macOS / Linux 不带 zone 根本绑不上 link-local（`Can't assign requested address`）。
/// 以前按主控的编译平台一刀切：Windows 主控 + macOS 辅测时，辅测端的 ping6 和
/// iperf3（server 与 client）全部绑定失败；反过来又给 Windows 辅测的命令加上它不认的 zone。
pub(super) fn os_needs_v6_zone(os: &str) -> bool {
    !os.trim().eq_ignore_ascii_case("windows")
}

/// v6 link-local 地址按需加 zone；global 地址和空 zone 原样返回。
pub(super) fn with_zone(addr: &str, zone: &str, needs_zone: bool) -> String {
    if needs_zone && !zone.is_empty() && addr.starts_with("fe80") {
        format!("{}%{}", addr, zone)
    } else {
        addr.to_string()
    }
}

pub(super) fn fmt_tag(tag: &str) -> String {
    if tag.is_empty() {
        String::new()
    } else {
        format!("-{tag}")
    }
}

/// 日志用的方向前缀。双向单元两腿并行输出，缺了它就无法把 attempt/retry
/// 归属到 AB 还是 BA。
pub(super) fn fmt_tag_bracket(tag: &str) -> String {
    if tag.is_empty() {
        String::new()
    } else {
        format!("[{tag}]")
    }
}

pub(super) fn fmt_opt(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.3}Mbps"),
        None => "-".into(),
    }
}

pub(super) fn format_ping_rtt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.3}")).unwrap_or_else(|| "-".into())
}

pub(super) fn text_preview(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

pub(super) fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}
