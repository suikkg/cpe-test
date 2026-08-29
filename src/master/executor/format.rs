//! 报告与日志里的小格式化件。
//!
//! 单独成模块只有一个理由：它们**不依赖任何执行状态**。留在 executor 里会让
//! 「这个函数会不会碰到进程/端口/HTTP」这个问题每次都要重新读一遍才能回答。

use super::*;

/// v6 link-local 地址加 zone（仅 macOS 需要，Windows 不加）
pub(super) fn add_zone(addr: &str, zone: &str, _side: Side) -> String {
    if cfg!(target_os = "macos") && !zone.is_empty() && addr.starts_with("fe80") {
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
