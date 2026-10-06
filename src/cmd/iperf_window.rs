//! iperf3 事件时间轴的纯计算；子网与内环共用，不启动进程、不持有运行状态。

use crate::master::rate_window::{
    EffectiveWindow, ToolTimeline, ToolTrace, MIN_RATE_SAMPLE_COVERAGE,
};
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
/// 双向两条腿各自比配置时长多跑多少秒。
///
/// 两条腿在两个线程里起跑，前后总差几百毫秒到一两秒（远端 RPC、server 就绪
/// 探测、一次瞬态重试）。双向的结论只能建立在两条腿的**交集**上，而交集够不够
/// 配置时长用的是 [`WINDOW_COMPLETE_TOLERANCE_MS`] 这把尺子；两条腿都只跑
/// 配置时长的话，交集几乎永远差那几百毫秒。多跑的这一段只用来凑交集，
/// 判定窗口仍然截到配置时长。子网与内环共用。
pub(crate) const BIDIR_OVERLAP_MARGIN_SECS: u64 = 5;

/// 灌包进程实际跑多久：要求时长 + 起流爬升的稳定等待 + 双向交集余量。
///
/// **全仓唯一的一份**：执行端拿它下发 `-t` / `TimeLimit`，builder 和内环计划拿它
/// 估时。两边各算一遍的话，预计耗时和实际跑的时长迟早会对不上。
///
/// - `settle_secs`：TCP 起流有慢启动和窗口爬升，前几秒的低速不该进平均；判定窗口
///   从起点往后扣掉这一段，进程就得多跑这一段，否则窗口凑不够要求时长。
/// - `overlap_margin`：双向结论只在两条腿的交集上算，两条腿各多跑
///   [`BIDIR_OVERLAP_MARGIN_SECS`]。
pub(crate) fn traffic_process_secs(duration: u64, settle_secs: u64, overlap_margin: bool) -> u64 {
    duration
        .saturating_add(settle_secs)
        .saturating_add(if overlap_margin {
            BIDIR_OVERLAP_MARGIN_SECS
        } else {
            0
        })
}

/// 从真实流量区间起点扣掉稳定等待（起流爬升段）。扣完不剩就是零长区间，
/// 下游按窗口不足处理。
pub(crate) fn settled_span(span: Option<(u64, u64)>, settle_secs: u64) -> Option<(u64, u64)> {
    span.map(|(start, end)| {
        (
            start
                .saturating_add(settle_secs.saturating_mul(1_000))
                .min(end),
            end,
        )
    })
}

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

/// iperf3 server（接收端）输出里的一条逐秒记录，时刻是 iperf 自己的时钟。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ServerInterval {
    pub start_ms: u64,
    pub end_ms: u64,
    pub mbps: f64,
    /// 多流时的 `[SUM]` 合计行；单流没有这种行。
    pub sum: bool,
}

/// 从 server 输出里取逐秒接收记录：不含收尾汇总行；同一个 server 上 client 重试
/// 之前那次测试的行（新测试从 0 重新计时）丢掉，只留最后一次。
pub(crate) fn server_intervals(server_output: &str) -> Vec<ServerInterval> {
    let mut out = Vec::new();
    let mut previous_start: Option<u64> = None;
    for line in server_output.lines() {
        let Some((start_ms, end_ms)) = per_second_interval(line) else {
            continue;
        };
        let Some(mbps) = crate::cmd::iperf::interval_rate_mbps(line) else {
            continue;
        };
        if previous_start.is_some_and(|previous| start_ms < previous) {
            out.clear();
        }
        previous_start = Some(start_ms);
        out.push(ServerInterval {
            start_ms,
            end_ms,
            mbps,
            sum: line.contains("[SUM]"),
        });
    }
    out
}

/// 最后一次尝试里 iperf 时钟相对监控时钟的偏移；对不出来是 `None`。
fn attempt_clock_offset_ms(events: &[IperfFlowEvent]) -> Option<u64> {
    iperf_clock_offset_ms(&last_attempt(events)?.traffic)
}

/// 接收端 server 逐秒记录在 `window`（监控时间轴）上的时间加权平均速率，Mbps。
///
/// 这是工具口径里唯一能裁到任意时间段的数：全程 receiver 汇总覆盖的是整次运行，
/// 含起流爬升，双向时也对不到两条腿的共同窗口。server 与 client 都在 TEST_START
/// 开始计时，逐秒行用 client 逐秒行算出的偏移投影到监控时间轴。多流只认 `[SUM]`
/// 行（逐流行相加容易漏行），单流认逐流行。记录要覆盖窗口的
/// [`MIN_RATE_SAMPLE_COVERAGE`] 以上，和网卡采样覆盖率同一条线。
pub(crate) fn receiver_rate_over(
    events: &[IperfFlowEvent],
    server: &[ServerInterval],
    streams: u32,
    window: (u64, u64),
) -> Result<f64, String> {
    let (start, end) = window;
    if end <= start {
        return Err("判定窗口为空".into());
    }
    let offset_ms = attempt_clock_offset_ms(events)
        .ok_or("client 逐秒行对不出时钟偏移，接收端逐秒记录无法对到判定窗口上")?;
    let want_sum = streams > 1;
    let mut covered_ms = 0u64;
    let mut weighted = 0.0;
    for line in server.iter().filter(|line| line.sum == want_sum) {
        let line_start = offset_ms.saturating_add(line.start_ms).max(start);
        let line_end = offset_ms.saturating_add(line.end_ms).min(end);
        if line_end > line_start {
            covered_ms += line_end - line_start;
            weighted += line.mbps * (line_end - line_start) as f64;
        }
    }
    let length_ms = end - start;
    if (covered_ms as f64) < length_ms as f64 * MIN_RATE_SAMPLE_COVERAGE {
        return Err(format!(
            "接收端{}逐秒记录只覆盖判定窗口的 {:.1}%",
            if want_sum { " [SUM] " } else { "" },
            covered_ms as f64 * 100.0 / length_ms as f64
        ));
    }
    let mbps = weighted / covered_ms as f64;
    if !mbps.is_finite() {
        return Err(format!("接收端逐秒速率非法: {mbps}"));
    }
    Ok(mbps)
}

/// 一条 iperf3 流的工具侧旁证，供「计数器零增长是不是真断流」使用。
///
/// 接收端取 server 输出里的逐秒行：server 和 client 都在 TEST_START 开始计时，
/// 行内时刻用 client 逐秒行算出的时钟偏移投影回监控时间轴（误差在一个 RTT
/// 以内，远小于 1 秒的汇报粒度）。发送端只对 TCP 填（见 [`ToolTrace::sender`]）。
/// 对不出时钟偏移就什么都不填——宁可「无法区分」，不拿错位的时间轴下判断。
pub(crate) fn iperf_tool_trace(
    events: &[IperfFlowEvent],
    server: &[ServerInterval],
    required_secs: u64,
    udp: bool,
) -> ToolTrace {
    let Some(attempt) = last_attempt(events) else {
        return ToolTrace::default();
    };
    let Some(offset_ms) = iperf_clock_offset_ms(&attempt.traffic) else {
        return ToolTrace::default();
    };
    let project = |(start, end): (u64, u64)| {
        (
            offset_ms.saturating_add(start),
            offset_ms.saturating_add(end),
        )
    };
    let sender = if udp {
        ToolTimeline::default()
    } else {
        // client 只为速率大于零的逐秒行发事件；没发事件的那几秒就是零。
        // 所以「汇报过的时间」取这次测量覆盖的整段，而不是只数有事件的行。
        ToolTimeline {
            reported: iperf_active_interval(events, required_secs)
                .into_iter()
                .collect(),
            flowing: attempt
                .traffic
                .iter()
                .filter_map(|event| per_second_interval(&event.line))
                .map(project)
                .collect(),
        }
    };
    let mut receiver = ToolTimeline::default();
    for line in server {
        let projected = project((line.start_ms, line.end_ms));
        receiver.reported.push(projected);
        if line.mbps > 0.0 {
            receiver.flowing.push(projected);
        }
    }
    ToolTrace { receiver, sender }
}

/// iperf3 收尾时的汇总行（`sender` / `receiver`）；其余带区间的都是逐秒行。
pub(crate) fn is_summary_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains(" sender") || lower.contains(" receiver")
}

/// 逐秒行的区间不会比它长（`-i 1`，最后一行是不足 1 秒的残帧）。
///
/// 更长的只可能是汇总行：iperf3 3.1.x 的 UDP 汇总行**不带** `sender` /
/// `receiver` 字样，[`is_summary_line`] 认不出来。它横跨整段、速率大于零，
/// 混进逐秒记录的话，真实断流会被当成「工具同期仍在收」，窗口覆盖率也会被
/// 这一条凑满。
const MAX_INTERVAL_LINE_MS: u64 = 2_000;

/// 一条逐秒行（不是汇总行）的区间；不是逐秒行就是 `None`。
fn per_second_interval(line: &str) -> Option<(u64, u64)> {
    if is_summary_line(line) {
        return None;
    }
    iperf_interval_ms(line).filter(|(start, end)| end - start <= MAX_INTERVAL_LINE_MS)
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

/// 最后一次尝试的事件视图。重试之前的事件一律不算：失败那次的逐秒行
/// 投影到这次的时间轴上，只会制造一段不存在的流量。
struct Attempt<'a> {
    end: u64,
    started: Option<u64>,
    latest_retry: Option<u64>,
    floor: u64,
    connected: Option<u64>,
    traffic: Vec<&'a IperfFlowEvent>,
}

fn last_attempt(events: &[IperfFlowEvent]) -> Option<Attempt<'_>> {
    let latest_retry = events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Retry)
        .map(|event| event.elapsed_ms)
        .max();
    let retry_cutoff = latest_retry.unwrap_or(0);
    let end = events
        .iter()
        .rev()
        .find(|event| event.kind == IperfEventKind::Ended && event.elapsed_ms >= retry_cutoff)
        .map(|event| event.elapsed_ms)?;
    let started = events
        .iter()
        .rev()
        .find(|event| {
            event.kind == IperfEventKind::Started
                && event.elapsed_ms >= retry_cutoff
                && event.elapsed_ms < end
        })
        .map(|event| event.elapsed_ms);
    let floor = started.unwrap_or(retry_cutoff);
    let connected = events
        .iter()
        .find(|event| {
            event.kind == IperfEventKind::Connected
                && event.elapsed_ms >= floor
                && event.elapsed_ms < end
        })
        .map(|event| event.elapsed_ms);
    let traffic = events
        .iter()
        .filter(|event| {
            event.kind == IperfEventKind::Traffic
                && event.elapsed_ms >= floor
                && event.elapsed_ms <= end
                && event.mbps.unwrap_or(0.0) > 0.0
        })
        .collect();
    Some(Attempt {
        end,
        started,
        latest_retry,
        floor,
        connected,
        traffic,
    })
}

pub(crate) fn iperf_active_interval(
    events: &[IperfFlowEvent],
    required_secs: u64,
) -> Option<(u64, u64)> {
    let Attempt {
        end,
        started,
        latest_retry: latest_retry_ms,
        floor: attempt_floor,
        connected,
        traffic: traffic_events,
    } = last_attempt(events)?;
    let expected_ms = required_secs.saturating_mul(1_000);
    let first_traffic = traffic_events.first().map(|event| event.elapsed_ms);

    // interval 行内的时间是 iperf 进程自己的测量时间，不受 stdout 块缓冲影响，
    // 是最可信的活跃区间来源，因此优先于任何事件到达时间。
    //
    // 即使行内区间短于用户要求的时长也必须采用：短就是短，应当由下游按
    // 「共同有效窗口不足」判定。若因为“不够长”而丢弃它，回退项反而是更长的
    // client 进程寿命（含 startup/settle/退出收尾），会把一次只测到 175 秒的
    // 短测量补成完整 180 秒窗口，并把启动爬升算进 RX 平均。
    let clock_offset_ms = iperf_clock_offset_ms(&traffic_events);
    let lines: Vec<(u64, u64, u64, u64, bool)> = traffic_events
        .iter()
        .filter_map(|event| {
            iperf_interval_ms(&event.line).map(|(line_start_ms, line_end_ms)| {
                (
                    line_end_ms.saturating_sub(line_start_ms),
                    event.elapsed_ms,
                    line_start_ms,
                    line_end_ms,
                    is_summary_line(&event.line),
                )
            })
        })
        .collect();
    // 最终汇总行覆盖的区间最长，正常也最后到达；按时长优先排序，避免
    // 逐秒 interval 行恰好排在汇总行之后时被当成整段测量。
    let summary = lines
        .iter()
        .filter(|line| line.4)
        .max_by_key(|(duration_ms, event_elapsed_ms, ..)| (*duration_ms, *event_elapsed_ms))
        .map(|&(duration_ms, event_elapsed_ms, start, end, _)| {
            (duration_ms, event_elapsed_ms, start, end)
        });
    // 没有汇总行时，逐秒行**合起来**才是测量证据。
    //
    // iperf3 在结果交换之前就失败（发 TEST_END 时连接被重置，报
    // `unable to send control message`）时根本不打印汇总行。以前这里照样取
    // 「最长的一行」，而逐秒行每行都是 1 秒，于是跑满 180 秒的测量只剩最后
    // 1 秒的窗口：`IPERF_SUMMARY_LOST` 那条保住网卡口径的路径永远走不到，
    // 整行被判成 SETUP_ERROR；内环则在这 1 秒上直接下了结论。
    // 取逐秒行覆盖的整段；中途提前退出的测量照样是短的，由下游判窗口不足。
    let reported_interval = summary.or_else(|| {
        let start = lines.iter().map(|line| line.2).min()?;
        let last = lines
            .iter()
            .max_by_key(|(_, event_elapsed_ms, _, line_end_ms, _)| {
                (*line_end_ms, *event_elapsed_ms)
            })?;
        Some((last.3.saturating_sub(start), last.1, start, last.3))
    });
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

/// 一条 iperf3 流可以拿来判定的整段：真实流量区间扣掉起流爬升，**没有**截到
/// 要求时长。没有吞吐测量就是 `None`。
pub(crate) fn iperf_measured_span(
    events: &[IperfFlowEvent],
    required_secs: u64,
    settle_secs: u64,
    has_measurement: bool,
) -> Option<(u64, u64)> {
    has_measurement
        .then(|| settled_span(iperf_active_interval(events, required_secs), settle_secs))
        .flatten()
}

/// [`iperf_measured_span`] 截成判定窗口；生产路径要同时用到区间本身，直接组合那两步。
#[cfg(test)]
pub(crate) fn iperf_effective_window(
    events: &[IperfFlowEvent],
    required_secs: u64,
    settle_secs: u64,
    has_measurement: bool,
) -> EffectiveWindow {
    window_from_span(
        iperf_measured_span(events, required_secs, settle_secs, has_measurement),
        required_secs,
    )
}

/// 把一段真实流量区间变成判定窗口：够长就截到要求时长，不够就原样标成不完整。
///
/// iperf 单腿、UDP 腿、双向合计的共同重叠段都走这一份——「跑满没有」只有一把尺子
/// （ADR-12），截断的方向也只有一种：从区间起点往后数要求时长。
pub(crate) fn window_from_span(span: Option<(u64, u64)>, required_secs: u64) -> EffectiveWindow {
    let Some((start_ms, end_ms)) = span.filter(|(start, end)| end >= start) else {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    };
    let available_ms = end_ms - start_ms;
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

/// 两条腿真实流量区间的交集；任一缺失或不相交就是 `None`。
pub(crate) fn overlap_span(spans: &[Option<(u64, u64)>]) -> Option<(u64, u64)> {
    let mut start = 0u64;
    let mut end = u64::MAX;
    if spans.is_empty() {
        return None;
    }
    for span in spans {
        let (s, e) = (*span)?;
        start = start.max(s);
        end = end.min(e);
    }
    (end > start).then_some((start, end))
}
