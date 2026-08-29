//! 灌包过程中的**实时**状态：控制台进度行、事件流折叠。
//!
//! 与判定无关——这里的一切只影响人看到什么，不影响任何 PASS/FAIL。分出来是
//! 为了让「改进度显示」不必打开判定链所在的文件。

use super::*;

#[derive(Debug, Clone, Default)]
pub(super) struct LiveFlowState {
    pub(super) connected: bool,
    pub(super) active: bool,
    pub(super) ended: bool,
    pub(super) last_mbps: Option<f64>,
    pub(super) error: String,
    pub(super) retries: usize,
}

pub(super) struct IperfProgressSnapshot<'a> {
    pub(super) protocol: &'a str,
    pub(super) tag: &'a str,
    pub(super) active: usize,
    pub(super) total: usize,
    pub(super) connected: usize,
    pub(super) ended: usize,
    pub(super) nic_rx_mbps: Option<f64>,
    pub(super) iperf_mbps: Option<f64>,
    pub(super) errors: usize,
    pub(super) monitor_error: String,
}

pub(super) fn discovery_stage(stream_pos: usize, total: usize) -> u64 {
    if total <= 1 {
        return 0;
    }
    let ordinal = stream_pos + 1;
    let q1 = ((total as f64) * 0.25).ceil() as usize;
    let q2 = ((total as f64) * 0.50).ceil() as usize;
    let q3 = ((total as f64) * 0.75).ceil() as usize;
    if ordinal <= q1 {
        0
    } else if ordinal <= q2 {
        1
    } else if ordinal <= q3 {
        2
    } else {
        3
    }
}

pub(super) fn format_flow_events(events: &[IperfFlowEvent], error: &str) -> String {
    let mut out = String::new();
    for event in events {
        out.push_str(&format!(
            "{:>8.3}s  {:?}{}  {}\n",
            event.elapsed_ms as f64 / 1000.0,
            event.kind,
            event
                .mbps
                .map(|v| format!(" {:.3}Mbps", v))
                .unwrap_or_default(),
            event.line
        ));
    }
    if !error.is_empty() {
        out.push_str(&format!("ERROR: {error}\n"));
    }
    out
}

pub(super) fn apply_flow_event(state: &mut LiveFlowState, event: &IperfFlowEvent) {
    match event.kind {
        IperfEventKind::Connected => state.connected = true,
        IperfEventKind::Traffic => {
            state.active = true;
            state.last_mbps = event.mbps;
        }
        IperfEventKind::Retry => state.retries += 1,
        IperfEventKind::Error => state.error = event.line.clone(),
        IperfEventKind::Ended => {
            state.ended = true;
            state.active = false;
        }
        IperfEventKind::Started => {}
    }
}

pub(super) fn active_iperf_rate(state: &LiveFlowState) -> Option<f64> {
    (state.active && !state.ended)
        .then_some(state.last_mbps)
        .flatten()
}

pub(super) fn format_iperf_progress(snapshot: &IperfProgressSnapshot<'_>) -> String {
    let tag = if snapshot.tag.is_empty() {
        "单向"
    } else {
        snapshot.tag
    };
    let rate = |value: Option<f64>| {
        value
            .map(|value| format!("{value:.1}Mbps"))
            .unwrap_or_else(|| "-".into())
    };
    let mut line = format!(
        "    [灌包进度][{}][{}] active={}/{} connected={} ended={} nic-rx={} iperf={} err={}",
        snapshot.protocol,
        tag,
        snapshot.active,
        snapshot.total,
        snapshot.connected,
        snapshot.ended,
        rate(snapshot.nic_rx_mbps),
        rate(snapshot.iperf_mbps),
        snapshot.errors
    );
    if !snapshot.monitor_error.is_empty() {
        line.push_str(&format!(
            " monitor={}",
            snapshot.monitor_error.replace(['\r', '\n'], " ")
        ));
    }
    line
}

pub(super) fn is_live_progress_rate_line(line: &str, parallel_streams: usize) -> bool {
    let lower = line.to_lowercase();
    if lower.contains(" sender") || lower.contains(" receiver") {
        return false;
    }
    iperf_interval_ms(line).is_some() && (parallel_streams <= 1 || lower.contains("[sum]"))
}

pub(super) fn tcp_parallel_streams(extra: &[String]) -> usize {
    extra
        .windows(2)
        .find_map(|pair| {
            pair[0]
                .eq_ignore_ascii_case("-p")
                .then(|| pair[1].parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(1)
        .max(1)
}
