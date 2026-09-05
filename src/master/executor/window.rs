//! **有效判定窗口**的推导：从工具事件流里切出「这段时间的数据才作数」。
//!
//! 判定的一切都建立在这一层之上：窗口取错，平均值、P10、越界段全部跟着错，
//! 而且错得很难看出来——数字依然自洽，只是描述的不是同一段时间。所以窗口
//! 推导必须是能被单独审阅的一层，不能散落在各条执行路径里顺手算。

use super::*;

pub(super) fn cts_baseline_cutoff_ms(attempts: &[CtsAttemptRun]) -> u64 {
    attempts
        .iter()
        .flat_map(|attempt| attempt.events.iter())
        .filter(|event| event.kind == IperfEventKind::Started)
        .map(|event| event.elapsed_ms)
        .min()
        .unwrap_or(0)
}

pub(super) fn midpoint_ms(before_ms: u64, after_ms: u64) -> u64 {
    before_ms.saturating_add(after_ms.saturating_sub(before_ms) / 2)
}

pub(super) fn remote_job_origin_ms(response_elapsed_ms: u64, remote_elapsed_ms: u64) -> u64 {
    let latest_start_ms = if remote_elapsed_ms > 0 {
        response_elapsed_ms.saturating_sub(remote_elapsed_ms)
    } else {
        response_elapsed_ms
    };
    midpoint_ms(0, latest_start_ms)
}

pub(super) fn align_monitor_samples(out: &mut MonitorStopOut, start_offset_ms: u64) {
    for sample in &mut out.samples {
        sample.elapsed_ms = sample.elapsed_ms.saturating_add(start_offset_ms);
    }
}

pub(super) fn cts_effective_window(
    events: &[IperfFlowEvent],
    required_secs: u64,
    status_update_ms: u64,
) -> EffectiveWindow {
    let required_ms = required_secs.saturating_mul(1_000);
    let Some(end_ms) = events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Ended)
        .map(|event| event.elapsed_ms)
        .max()
    else {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    };
    let started_ms = events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Started && event.elapsed_ms < end_ms)
        .map(|event| event.elapsed_ms)
        .max();
    let attempt_floor = started_ms.unwrap_or(0);
    let first_traffic_ms = events
        .iter()
        .filter(|event| {
            event.kind == IperfEventKind::Traffic
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms < end_ms
                && event.mbps.unwrap_or(0.0) > 0.0
        })
        .map(|event| event.elapsed_ms)
        .min();
    let connected_ms = events
        .iter()
        .filter(|event| {
            event.kind == IperfEventKind::Connected
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms < end_ms
        })
        .map(|event| event.elapsed_ms)
        .max();
    let status_inferred_ms = first_traffic_ms.map(|traffic_ms| {
        traffic_ms
            .saturating_sub(status_update_ms)
            .max(attempt_floor)
    });
    let event_start_ms = match (connected_ms, status_inferred_ms) {
        (Some(connected), Some(status)) => Some(connected.max(status)),
        (connected, status) => connected.or(status),
    };
    let spans_required = |start_ms: u64| {
        end_ms > start_ms
            && end_ms
                .saturating_sub(start_ms)
                .saturating_add(WINDOW_COMPLETE_TOLERANCE_MS)
                >= required_ms
    };

    // 首条状态行表示前一个 StatusUpdate 周期，通常比 Connection 更接近数据起点。
    // Total Time、正常退出和疑似块缓冲都不能证明纯数据时长；事件证据不足时
    // 保守返回短窗口，避免把启动或握手空窗计入 NIC 平均值。
    let complete_start_ms = event_start_ms
        .filter(|start_ms| spans_required(*start_ms))
        .or_else(|| connected_ms.filter(|start_ms| spans_required(*start_ms)));
    let start_ms = complete_start_ms.unwrap_or_else(|| {
        first_traffic_ms
            .or(connected_ms)
            .or(started_ms)
            .unwrap_or(end_ms)
    });
    let available_ms = end_ms.saturating_sub(start_ms);
    let complete = complete_start_ms.is_some()
        && available_ms.saturating_add(WINDOW_COMPLETE_TOLERANCE_MS) >= required_ms;
    let scored_end_ms = if complete {
        start_ms.saturating_add(required_ms).min(end_ms)
    } else {
        end_ms
    };
    EffectiveWindow {
        start_ms,
        end_ms: scored_end_ms,
        available_secs: available_ms as f64 / 1_000.0,
        required_secs,
        complete,
    }
}

pub(super) fn iperf_interval_ms(line: &str) -> Option<(u64, u64)> {
    pub(super) fn seconds_to_ms(raw: &str) -> Option<u64> {
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

pub(super) fn flow_duration_is_plausible(start_ms: u64, end_ms: u64, expected_ms: u64) -> bool {
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
pub(super) fn iperf_baseline_cutoff_ms<'a>(
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

pub(super) fn iperf_active_interval(
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

pub(super) fn iperf_effective_window(
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

pub(super) fn flow_active_interval(flow: &UdpFlowRun) -> Option<(u64, u64)> {
    if !flow.raw_ok {
        return None;
    }
    iperf_active_interval(&flow.events, flow.task.duration)
}

/// 一个 UDP 单元里各条方向腿的判定窗口。
pub(crate) struct UdpUnitWindows {
    /// 每条腿各自的有效窗口，下标与 `plans` 对齐。
    pub per_leg: Vec<EffectiveWindow>,
    /// 各腿窗口的交集时长（秒）。
    ///
    /// **只用于说明「双向并发」这件事到底成没成立，不参与任何一条腿的判定。**
    pub concurrency_secs: f64,
}

/// 逐腿计算判定窗口，并另外给出各腿的重叠时长。
///
/// 这里曾经只产出**一个**全单元共用的窗口，做法是把所有腿的采样区间求交集
/// （`lower.max` / `upper.min`），再要求每个时刻**每条腿**都有足够活跃流。
/// 于是双向单元里只要有一条腿没跑通，交集就是空的，另一条腿哪怕整整三分钟
/// 都在满速跑，也会被写成「RX均值=- 覆盖率=0.0% NOT_EVALUATED」。
///
/// run_20260825_215915_7684 的任务 10/12/34/36 就是这样丢掉了 8 行数据，
/// 其中 unit-33-34 的接收端网卡 205/208 个样本有流量、均值 923.08Mbps，
/// CSV 就在盘上，报表却说没采到（见 .ai/DESIGN-v4.3.0.md D1）。
///
/// 一条腿失败是一条腿的事实，不能抹掉另一条腿测到的真实速率；而「两条腿没有
/// 真正并发」是另一件需要单独说清楚的事实——所以拆成两个返回值，而不是让前者
/// 沉默地吃掉后者。
pub(super) fn select_udp_effective_windows(
    plans: &[UdpLegPlan],
    results: &[UdpFlowRun],
    monitors: &HashMap<String, MonitorStopOut>,
    rate_cfg: &RateCheckCfg,
) -> UdpUnitWindows {
    let required_secs = plans
        .iter()
        .flat_map(|plan| plan.streams.iter().map(|task| task.duration))
        .max()
        .unwrap_or(0);
    let per_leg: Vec<EffectiveWindow> = plans
        .iter()
        .enumerate()
        .map(|(leg_pos, plan)| {
            leg_effective_window(leg_pos, plan, results, monitors, rate_cfg, required_secs)
        })
        .collect();

    // 交集：任一腿窗口为空则并发时长为 0。
    let mut overlap_start = 0u64;
    let mut overlap_end = u64::MAX;
    for window in &per_leg {
        if window.end_ms <= window.start_ms {
            overlap_end = 0;
            break;
        }
        overlap_start = overlap_start.max(window.start_ms);
        overlap_end = overlap_end.min(window.end_ms);
    }
    let concurrency_secs = if per_leg.is_empty() || overlap_end <= overlap_start {
        0.0
    } else {
        overlap_end.saturating_sub(overlap_start) as f64 / 1000.0
    };

    UdpUnitWindows {
        per_leg,
        concurrency_secs,
    }
}

/// 单条方向腿的有效窗口：只看这条腿自己的活跃流和自己的接收端采样。
pub(super) fn leg_effective_window(
    leg_pos: usize,
    plan: &UdpLegPlan,
    results: &[UdpFlowRun],
    monitors: &HashMap<String, MonitorStopOut>,
    rate_cfg: &RateCheckCfg,
    required_secs: u64,
) -> EffectiveWindow {
    let empty = EffectiveWindow {
        required_secs,
        ..Default::default()
    };
    let Some(first) = plan.streams.first() else {
        return empty;
    };
    // 这条腿的接收端 monitor 缺失，只让这条腿没结论；旧代码在这里直接
    // `return` 整个单元的零窗口，于是 mon11 那次监控丢失连带废掉了对向腿。
    let Some(out) = monitors.get(&first.dst.key()) else {
        return empty;
    };
    let Some(first_sample) = out.samples.iter().find(|sample| sample.valid) else {
        return empty;
    };
    let Some(last_sample) = out.samples.iter().rev().find(|sample| sample.valid) else {
        return empty;
    };
    let lower = first_sample.elapsed_ms;
    let upper = last_sample.elapsed_ms;
    if upper <= lower {
        return empty;
    }
    let sample_tolerance_ms = rate_cfg
        .sample_interval_ms
        .clamp(200, 5_000)
        .saturating_mul(2)
        .max(1_500);

    let required = required_udp_streams(
        plan.streams.len(),
        rate_cfg,
        first.rx_target_mbps,
        first.offered_per_stream_mbps,
    );
    let eligible = |t: u64| -> bool {
        let active = results
            .iter()
            .filter(|flow| flow.leg_pos == leg_pos)
            .filter_map(flow_active_interval)
            .filter(|(start, end)| *start <= t && t < *end)
            .count();
        active >= required && nearest_valid_sample(out, t, sample_tolerance_ms).is_some()
    };

    let mut best_start = 0u64;
    let mut best_end = 0u64;
    let mut current_start: Option<u64> = None;
    let mut t = lower;
    while t <= upper {
        if eligible(t) {
            if current_start.is_none() {
                current_start = Some(t);
            }
        } else if let Some(start) = current_start.take() {
            if t.saturating_sub(start) > best_end.saturating_sub(best_start) {
                best_start = start;
                best_end = t;
            }
        }
        t = t.saturating_add(1_000);
    }
    if let Some(start) = current_start {
        let end = upper.saturating_add(1_000);
        if end.saturating_sub(start) > best_end.saturating_sub(best_start) {
            best_start = start;
            best_end = end;
        }
    }

    if best_end <= best_start {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    }

    let scored_start = best_start.saturating_add(rate_cfg.settle_secs.saturating_mul(1_000));
    let available_ms = best_end.saturating_sub(scored_start);
    let available_secs = available_ms as f64 / 1000.0;
    // 与 iperf/CTS 同一个容差（ADR-12）。此前这里是零容差，于是 179.95 秒的
    // UDP 腿判 EFFECTIVE_WINDOW_SHORT，而同样的 TCP 腿 PASS——同一件事在两条
    // 链上两个结论。
    let complete = available_ms.saturating_add(WINDOW_COMPLETE_TOLERANCE_MS)
        >= required_secs.saturating_mul(1_000);
    let scored_end = if complete {
        scored_start.saturating_add(required_secs.saturating_mul(1_000))
    } else {
        best_end
    };
    EffectiveWindow {
        start_ms: scored_start,
        end_ms: scored_end,
        available_secs,
        required_secs,
        complete,
    }
}
