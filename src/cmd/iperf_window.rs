//! iperf3 事件时间轴的纯计算；子网与内环共用，不启动进程、不持有运行状态。

use crate::master::rate_window::EffectiveWindow;
use crate::protocol::{IperfEventKind, IperfFlowEvent};

/// 认定「这条流还活着」时允许的事件间隔。与窗口完整性无关，别混用。
const FLOW_TIMELINE_TOLERANCE_MS: u64 = 2_000;
/// **有效窗口是否算完整**时允许的收尾误差，三条链共用（ADR-12）。
///
/// 名字里没有后端：它以前叫 `CTS_TIMELINE_TOLERANCE_MS`，而 iperf 路径也在用
/// 它——「iperf 用着一个名叫 CTS 的常量」本身就是这层已经分叉的症状。
///
/// 更要紧的是 UDP 路径**根本没用它**（零容差）：一条跑了 179.95 秒、要求 180 秒
/// 的 UDP 腿判 `EFFECTIVE_WINDOW_SHORT`，而同样的 TCP 腿 PASS。50 毫秒的收尾
/// 差异不是测量事实的差异，是三条链各自决定容差的结果。
pub(crate) const WINDOW_COMPLETE_TOLERANCE_MS: u64 = 100;

pub(crate) fn iperf_interval_ms(line: &str) -> Option<(u64, u64)> {
    pub(crate) fn seconds_to_ms(raw: &str) -> Option<u64> {
        if raw.is_empty()
            || !raw
                .chars()
                .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == ',')
        {
            return None;
        }
        let seconds = raw.replace(',', ".").parse::<f64>().ok()?;
        if !seconds.is_finite() || !(0.0..=u64::MAX as f64 / 1_000.0).contains(&seconds) {
            return None;
        }
        Some((seconds * 1_000.0).round() as u64)
    }

    let fields: Vec<&str> = line.split_whitespace().collect();
    fields.windows(2).find_map(|pair| {
        if pair[1] != "sec" {
            return None;
        }
        let (start, end) = pair[0].split_once('-')?;
        let start_ms = seconds_to_ms(start)?;
        let end_ms = seconds_to_ms(end)?;
        (end_ms > start_ms).then_some((start_ms, end_ms))
    })
}

pub(crate) fn flow_duration_is_plausible(start_ms: u64, end_ms: u64, expected_ms: u64) -> bool {
    end_ms > start_ms
        && end_ms
            .saturating_sub(start_ms)
            .saturating_add(FLOW_TIMELINE_TOLERANCE_MS)
            >= expected_ms
}

/// Return the earliest client-start boundary across the included attempts or
/// flows. Only samples that ended before any client could send traffic are
/// eligible as idle background. A retry boundary or an inferred traffic
/// window can both occur after traffic has already flowed, so neither may be
/// reused as the background cutoff.
pub(crate) fn iperf_baseline_cutoff_ms<'a>(
    events: impl IntoIterator<Item = &'a IperfFlowEvent>,
) -> u64 {
    events
        .into_iter()
        .filter(|event| event.kind == IperfEventKind::Started)
        .map(|event| event.elapsed_ms)
        .min()
        .unwrap_or(0)
}

/// iperf3 自己的测量时钟相对监控时钟的偏移：iperf 的 `t=0` 落在监控的第几毫秒。
///
/// 每条 interval 行都带两个时刻：行内区间终点（iperf 自己的测量时钟）和事件
/// 到达时刻（监控时钟）。两者之差就是偏移的一个估计。到达**只会被推迟、
/// 不会提前**（stdout 缓冲、线程调度、进程排空缓冲期间的停顿都只加不减），
/// 所以每个估计都是偏移的上界，**取最小值**就是最紧的那个上界。
///
/// 取最小值同时也是这段代码的抗扰点：`-w` 开大时，末尾几行连同汇总行会在
/// 排空结束后成块吐出，那几条的估计会比真值大十几秒；只要前面有任何一条
/// 按时到达的逐秒行，最小值就不受影响。全部成块到达（老版 iperf3 无
/// `--forceflush`）时，最小值退化成「汇总行到达时刻 − 行内终点」，与旧口径
/// 一致——不会更好，但也不会更差。
fn iperf_clock_offset_ms(traffic_events: &[&IperfFlowEvent]) -> Option<u64> {
    traffic_events
        .iter()
        .filter_map(|event| {
            let (_, line_end_ms) = iperf_interval_ms(&event.line)?;
            // 到达早于行内终点只可能是解析到了不属于本次测量的行；宁可丢掉
            // 这个估计，也不能让它把偏移拉成负数再饱和成 0。
            event.elapsed_ms.checked_sub(line_end_ms)
        })
        .min()
}

pub(crate) fn iperf_active_interval(
    events: &[IperfFlowEvent],
    required_secs: u64,
) -> Option<(u64, u64)> {
    let latest_retry_ms = events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Retry)
        .map(|event| event.elapsed_ms)
        .max();
    let retry_cutoff = latest_retry_ms.unwrap_or(0);
    let end = events
        .iter()
        .rev()
        .find(|event| event.kind == IperfEventKind::Ended && event.elapsed_ms >= retry_cutoff)
        .map(|event| event.elapsed_ms)?;
    let expected_ms = required_secs.saturating_mul(1_000);

    let started = events
        .iter()
        .rev()
        .find(|event| {
            event.kind == IperfEventKind::Started
                && event.elapsed_ms >= retry_cutoff
                && event.elapsed_ms < end
        })
        .map(|event| event.elapsed_ms);
    let attempt_floor = started.unwrap_or(retry_cutoff);
    let connected = events
        .iter()
        .find(|event| {
            event.kind == IperfEventKind::Connected
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms < end
        })
        .map(|event| event.elapsed_ms);
    let traffic_events: Vec<&IperfFlowEvent> = events
        .iter()
        .filter(|event| {
            event.kind == IperfEventKind::Traffic
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms <= end
                && event.mbps.unwrap_or(0.0) > 0.0
        })
        .collect();
    let first_traffic = traffic_events.first().map(|event| event.elapsed_ms);

    // interval 行内的时间是 iperf 进程自己的测量时间，不受 stdout 块缓冲影响，
    // 是最可信的活跃区间来源，因此优先于任何事件到达时间。
    //
    // 即使行内区间短于用户要求的时长也必须采用：短就是短，应当由下游按
    // 「共同有效窗口不足」判定。若因为“不够长”而丢弃它，回退项反而是更长的
    // client 进程寿命（含 startup/settle/退出收尾），会把一次只测到 175 秒的
    // 短测量补成完整 180 秒窗口，并把启动爬升算进 RX 平均。
    let clock_offset_ms = iperf_clock_offset_ms(&traffic_events);
    let reported_interval = traffic_events
        .iter()
        .filter_map(|event| {
            iperf_interval_ms(&event.line).map(|(line_start_ms, line_end_ms)| {
                (
                    line_end_ms.saturating_sub(line_start_ms),
                    event.elapsed_ms,
                    line_start_ms,
                    line_end_ms,
                )
            })
        })
        // 最终汇总行覆盖的区间最长，正常也最后到达；按时长优先排序，避免
        // 逐秒 interval 行恰好排在汇总行之后时被当成整段测量。
        .max_by_key(|(duration_ms, event_elapsed_ms, _, _)| (*duration_ms, *event_elapsed_ms));
    if let Some((duration_ms, event_elapsed_ms, line_start_ms, line_end_ms)) = reported_interval {
        // 首选：把行内区间按两条时钟的偏移投影回监控时间轴。
        //
        // 只用行内**时长**、拿汇总行的到达时刻当锚点是不行的：`-w` 开大时
        // client 的 `-t` 到点后还要几秒到十几秒排空 socket 缓冲，汇总行压在
        // 排空之后才吐出来，整个窗口就跟着后移那么多秒——掐掉开头的高速段、
        // 把结尾没有流量的尾巴收进来。run_20260905_125327_5940 的 unit-112
        // 后移 12.4 秒，RX 平均从 1036 被压到 705；unit-113 更是让窗口越过
        // 流量末端，末尾 3 秒零增长凑够 5%，整条腿判成 COUNTER_STALLED。
        if let Some(offset_ms) = clock_offset_ms {
            let start = offset_ms.saturating_add(line_start_ms).max(attempt_floor);
            let measured_end = offset_ms.saturating_add(line_end_ms).min(end);
            if measured_end > start {
                return Some((start, measured_end));
            }
        }
        // 退化路径：一条 interval 行都对不出偏移（老版 iperf3 在退出时才一次性
        // 吐出全部输出）。此时只剩到达时刻可用，行为与 v6.2.5 及以前一致。
        //
        // 最终汇总行已经证明吞吐测量结束；它之后到 Ended 之间只剩
        // child wait、stdout reader join 等退出收尾，不能纳入网卡平均。
        let measured_end = event_elapsed_ms.min(end);
        let start = measured_end.saturating_sub(duration_ms).max(attempt_floor);
        if measured_end > start {
            return Some((start, measured_end));
        }
    }

    // 支持 --forceflush 时首条 Traffic 的到达时间接近真实时间；旧版会在退出时
    // 一次性吐出全部 interval，此时 active duration 会明显短于 task.duration。
    if let Some(start) =
        first_traffic.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }
    if let Some(start) =
        connected.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }
    if let Some(start) =
        started.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }
    if let Some(start) =
        latest_retry_ms.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }

    // 测试确实提前结束时保留最保守的可观察起点，使有效窗口保持不足。
    let start = first_traffic.or(connected).or(started)?;
    (end > start).then_some((start, end))
}

pub(crate) fn iperf_effective_window(
    events: &[IperfFlowEvent],
    required_secs: u64,
    has_measurement: bool,
) -> EffectiveWindow {
    if !has_measurement {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    }
    let Some((start_ms, end_ms)) = iperf_active_interval(events, required_secs) else {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    };
    let available_ms = end_ms.saturating_sub(start_ms);
    let required_ms = required_secs.saturating_mul(1_000);
    let complete = available_ms.saturating_add(WINDOW_COMPLETE_TOLERANCE_MS) >= required_ms;
    EffectiveWindow {
        start_ms,
        end_ms: if complete {
            start_ms.saturating_add(required_ms).min(end_ms)
        } else {
            end_ms
        },
        available_secs: available_ms as f64 / 1_000.0,
        required_secs,
        complete,
    }
}
