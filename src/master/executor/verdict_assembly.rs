//! 把**执行事实**装配成**判定结论**。
//!
//! 这一层只读已经拿到的测量与进程证据，不发起任何执行。它和
//! [`crate::master::rate_window`] 的分工是：那边定速率口径，这边定
//! 「进程/流/腿的证据够不够格用那个口径」。

use super::*;

pub(super) fn iperf_client_setup_error(client: &IperfClientOut) -> Option<String> {
    let detail = || {
        client
            .output
            .lines()
            .last()
            .filter(|line| !line.trim().is_empty())
            .unwrap_or("iperf3 client 执行环境错误")
            .to_string()
    };
    if client.cancelled {
        return Some(detail());
    }
    if client.process_started != Some(true) {
        return Some(format!("client 进程未确认启动：{}", detail()));
    }
    if client.cleanup_confirmed != Some(true) {
        return Some(format!("client 进程回收未确认：{}", detail()));
    }
    if client.timed_out {
        // 已确认进程启动和回收的 timeout 是一次完整、安全的无测量尝试。
        return None;
    }

    let lower = client.output.to_ascii_lowercase();
    let setup_marker = [
        "主控机未找到 iperf3",
        "远端异步作业启动失败",
        "远端异步作业查询失败",
        "非预期 job id",
        "已结束但缺少结果",
        "duration=",
        "启动命令失败",
        "创建流式命令",
        "等待子进程失败",
        "回收子进程失败",
        "parameter error",
        "invalid argument",
        "invalid option",
        "unrecognized option",
        "option requires an argument",
        "unable to parse",
        "cannot assign requested address",
        "unable to bind",
        "no such device",
        "无法识别的选项",
        "无法分配请求的地址",
        "unable to set socket buffer",
        "bad format",
    ]
    .iter()
    .any(|marker| lower.contains(&marker.to_ascii_lowercase()));
    setup_marker.then(detail)
}

pub(super) fn row_has_usable_traffic_measurement(row: &Row) -> bool {
    if row.verdict == Verdict::SetupError
        || matches!(
            row.execution_status,
            ExecutionStatus::Error | ExecutionStatus::TimedOut | ExecutionStatus::Cancelled
        )
    {
        return false;
    }
    if crate::verdict::HARD_SINGLE_UDP_FAILURE_CODES.contains(&row.reason_code) {
        // 这两个专用硬失败的定义就是“工具自身没有任何吞吐证据”；即使
        // 同网卡存在背景流量，也必须继续触发故障诊断。
        return false;
    }
    let usable_rate =
        |value: Option<f64>| value.is_some_and(|rate| rate.is_finite() && rate > MIN_VALID_RX_MBPS);
    let tool_measurement =
        usable_rate(row.tx_mbps) || usable_rate(row.rx_mbps) || row.active_streams > 0;
    if row.transport.starts_with("CTS/") {
        // CTS 是否起流只认工具自身 rate/bytes/frame 派生出的字段；NIC RX
        // 只用于已起流后的产品目标验证，不能把背景流量补成 CTS 测量。
        return tool_measurement;
    }
    usable_rate(row.rx_avg) || tool_measurement || usable_rate(row.tx_avg)
}

pub(super) fn aggregate_unit_verdict(outcomes: &[LegOutcome]) -> Verdict {
    // 优先级的唯一定义在 crate::verdict::aggregate_verdict —— 报告侧的回退聚合
    // 走同一个函数，两边不会再分叉。
    aggregate_verdict(
        outcomes
            .iter()
            .map(|outcome| (outcome.verdict(), outcome.reason_code())),
    )
}

pub(super) fn aggregate_direction_streams(directions: &[DirectionSummary]) -> Option<StreamCounts> {
    directions
        .iter()
        .filter_map(|direction| direction.streams)
        .fold(None, |total: Option<StreamCounts>, counts| {
            Some(match total {
                Some(total) => StreamCounts {
                    requested: total.requested.saturating_add(counts.requested),
                    active: total.active.saturating_add(counts.active),
                    required: total.required.saturating_add(counts.required),
                },
                None => counts,
            })
        })
}

pub(super) fn populate_peer_rx(rows: &mut [Row], outcomes: &[LegOutcome]) {
    let ab = outcomes
        .iter()
        .position(|outcome| outcome.tag.eq_ignore_ascii_case("ab"));
    let ba = outcomes
        .iter()
        .position(|outcome| outcome.tag.eq_ignore_ascii_case("ba"));
    if let (Some(ab), Some(ba)) = (ab, ba) {
        for (me, other) in [(ab, ba), (ba, ab)] {
            if let Some(rx) = outcomes[other].rx_avg {
                for row_index in &outcomes[me].main_rows {
                    if let Some(row) = rows.get_mut(*row_index) {
                        row.peer_rx = format!(
                            "{rx:.3} Mbps ({})",
                            outcomes[other].tag.to_ascii_uppercase()
                        );
                    }
                }
            }
        }
    }
}

pub(super) fn outcome_matching_verdict(
    outcomes: &[LegOutcome],
    verdict: Verdict,
) -> Option<&LegOutcome> {
    if verdict == Verdict::SetupError {
        if let Some(outcome) = outcomes
            .iter()
            .find(|outcome| outcome.reason_code() == ReasonCode::CtsArgsInvalid)
        {
            return Some(outcome);
        }
    }
    if verdict == Verdict::RateFail {
        if let Some(outcome) = outcomes
            .iter()
            .find(|outcome| is_hard_single_udp_failure(outcome))
        {
            return Some(outcome);
        }
    }
    outcomes.iter().find(|outcome| outcome.verdict() == verdict)
}

pub(super) fn is_hard_single_udp_failure(outcome: &LegOutcome) -> bool {
    crate::verdict::is_hard_single_udp_failure(outcome.verdict(), outcome.reason_code())
}

pub(crate) struct IperfFlowVerdictIn<'a> {
    pub raw_ok: bool,
    pub measurement: bool,
    pub effective_window: &'a EffectiveWindow,
    pub required_secs: u64,
    pub rate_mode: RateMode,
    pub rx_target_mbps: Option<f64>,
    pub rx_stats: &'a RateStats,
    pub tx_stats: &'a RateStats,
    /// client 输出的最后一行，用作 setup 错误的可读细节。
    pub client_tail: &'a str,
    /// 接收端 monitor 的完整采样输出，仅用于窗口不足时给一个定位数字。
    pub rx_monitor: Option<&'a MonitorStopOut>,
}

/// 有效窗口不足时补一句定位信息：接收端网卡在**整个采样生命周期**
/// （含起流前后）的平均速率。
///
/// 它绝不能进判定。生命周期含 startup / settle / 退出收尾，会把一次只测到
/// 175 秒的短测量补成完整窗口，并把启动爬升算进平均——这正是本项目明确
/// 放弃 process-lifetime 回退的原因，那条边界不能动。
///
/// 但「这一行没结论」和「这块网卡一个字节都没收到」是两件完全不同的事。
/// run_20260825_215915_7684 的任务 97 里，接收网卡 202/202 个样本都有流量、
/// 全程均值 487.1Mbps、峰值 1582.4Mbps，报表却只写「未采集」，读的人无从
/// 判断到底是没测到还是真的没流量。判定可以拒绝下结论，但不该把已经看到的
/// 东西藏起来。
pub(super) fn lifecycle_rx_hint(out: Option<&MonitorStopOut>) -> String {
    let Some(out) = out.filter(|out| out.seconds > 0.0 && out.avg_mbps.is_finite()) else {
        return String::new();
    };
    format!(
        "；接收端网卡全程（{:.1}s，含起停）平均 {:.3}Mbps，仅供定位，不作判定依据",
        out.seconds, out.avg_mbps
    )
}

/// 单条 iperf3 流的判定链。
///
/// 抽成纯函数是为了让下面这个区分可以被单独测试：**「环境没搭起来」和
/// 「跑完了但最后一次结果交换失败」不是一回事**。
///
/// iperf3 经常在完整跑完全程之后，才在结果交换阶段报
/// `unable to send control message … Connection reset by peer`。此时接收端
/// 网卡计数器已经拿到了完整的正式口径，把它判成 `SETUP_ERROR` 等于让诊断
/// 口径的故障否决正式口径的结论——run_20260825_215915_7684 里 9 行
/// 125~1067Mbps 的实测就是这么丢的（见 .ai/DESIGN-v4.3.0.md D2）。
///
/// 判据用「有没有攒够要求时长的有效吞吐窗口」而不是匹配错误文本：窗口本身
/// 就是「这一轮到底测没测成」的既有权威答案，既不需要引入新的阈值常量，
/// 也不会随 iperf3 的措辞变化而失效。
pub(crate) fn iperf_flow_verdict(input: IperfFlowVerdictIn<'_>) -> (Verdict, ReasonCode, String) {
    let IperfFlowVerdictIn {
        raw_ok,
        measurement,
        effective_window,
        required_secs,
        rate_mode,
        rx_target_mbps,
        rx_stats,
        tx_stats,
        client_tail,
        rx_monitor,
    } = input;

    let summary_lost_after_full_run = !raw_ok && measurement && effective_window.complete;

    if !raw_ok && !summary_lost_after_full_run {
        return (
            Verdict::SetupError,
            ReasonCode::IperfExecFailed,
            client_tail.to_string(),
        );
    }
    if !measurement {
        return (
            Verdict::RateFail,
            ReasonCode::NoValidMeasurement,
            "iperf3 已结束，但没有 rate/bytes 吞吐测量".into(),
        );
    }
    if !effective_window.complete {
        return (
            Verdict::NotEvaluated,
            ReasonCode::IperfEffectiveWindowShort,
            format!(
                "iperf3 真实流量事件窗口仅 {:.3}s，短于要求的 {}s；未把 server 启动、连接或清理时间计入平均速率{}",
                effective_window.available_secs,
                required_secs,
                lifecycle_rx_hint(rx_monitor)
            ),
        );
    }

    let (verdict, code, detail) = evaluate_nic_rx(rate_mode, rx_target_mbps, rx_stats, tx_stats);
    if !summary_lost_after_full_run {
        return (verdict, code, detail);
    }
    // 判定本身仍然完全由网卡口径决定——RX 低于目标照样 RATE_FAIL，RX 缺失
    // 照样 NOT_EVALUATED。这里只把「工具自报速率不可用」记进原因，并保留原始
    // rate 结论的 reason_code，别让 RX_BELOW_TARGET 这类信息被覆盖掉。
    // 该行的执行状态仍是 ExecutionStatus::Error，概览上显示成
    // 「MEASURED · ERROR」，不会看起来一切正常。
    (
        verdict,
        code,
        format!(
            "IPERF_SUMMARY_LOST: iperf3 已完成全程灌包，仅最后的结果交换失败，\
             接收端网卡口径有效、工具自报速率不可用（{}）。{detail}",
            client_tail.trim()
        ),
    )
}

pub(super) fn active_rate_table(
    leg_pos: usize,
    flows: &[&UdpFlowRun],
    out: &MonitorStopOut,
    first_active_ms: u64,
) -> String {
    let mut baseline_values: Vec<f64> = out
        .samples
        .iter()
        .filter(|sample| sample.valid && sample.elapsed_ms < first_active_ms)
        .map(|sample| sample.rx_mbps)
        .collect();
    baseline_values.sort_by(|a, b| a.total_cmp(b));
    let baseline = percentile(&baseline_values, 0.5).unwrap_or(0.0);
    let mut groups: HashMap<usize, Vec<f64>> = HashMap::new();
    for sample in out.samples.iter().filter(|sample| sample.valid) {
        let active = flows
            .iter()
            .filter(|flow| flow.leg_pos == leg_pos)
            .filter_map(|flow| flow_active_interval(flow))
            .filter(|(start, end)| *start <= sample.elapsed_ms && sample.elapsed_ms < *end)
            .count();
        if active > 0 {
            groups
                .entry(active)
                .or_default()
                .push((sample.rx_mbps - baseline).max(0.0));
        }
    }
    let mut keys: Vec<usize> = groups.keys().copied().collect();
    keys.sort_unstable();
    let mut lines = vec!["active_streams,samples,avg_rx_mbps,p10_rx_mbps".to_string()];
    for active in keys {
        let mut values = groups.remove(&active).unwrap_or_default();
        if values.is_empty() {
            continue;
        }
        let avg = values.iter().sum::<f64>() / values.len() as f64;
        values.sort_by(|a, b| a.total_cmp(b));
        let p10 = percentile(&values, 0.10).unwrap_or(0.0);
        lines.push(format!("{active},{},{avg:.3},{p10:.3}", values.len()));
    }
    lines.join("\n")
}

// ---------------- 结果库（RESUME 用） ----------------

/// 一条 UDP 腿在**判定时刻**已经确定的全部事实。
///
/// 判定需要的东西到这里全部是值：没有进程句柄、没有对端连接、没有 `&self`。
/// 这不是为了好看——`run_udp_unit` 里那条一百多行的判定链此前和执行代码
/// 缠在一起，想验证「灌不够时不能判 CPE 不达标」这类口径，就得先起一整套
/// 假执行环境；现在给一份事实就能单测。
pub(super) struct UdpLegFacts<'a> {
    /// 配置了几条流。
    pub streams_total: usize,
    /// 其中几条真的产出了有效测量。
    pub streams_success: usize,
    /// 达到目标至少需要几条。
    pub streams_required: usize,
    /// 有 iperf3 自报测量、但 client 非正常结束的流数。
    pub runtime_failures: usize,
    /// 单流 UDP 是否已把重试次数用尽。
    pub single_stream_exhausted: bool,
    /// 单流 UDP 实际尝试了几次 client。
    pub single_attempts: usize,
    /// 本腿的有效判定窗口。
    pub window: &'a EffectiveWindow,
    /// 接收端网卡统计——正式口径。
    pub rx: &'a RateStats,
    /// 发送端网卡统计——用来回答「灌够了没有」。
    pub tx: &'a RateStats,
    pub rate_mode: RateMode,
    pub rx_target_mbps: Option<f64>,
    /// 验证目标所需的最低发送负载（目标 + 余量）。
    pub offered_floor: Option<f64>,
    pub udp_loss: Option<f64>,
    pub max_udp_loss_pct: Option<f64>,
    /// 窗口不足时补充的一句生命周期线索，已由调用方渲染好。
    pub rx_lifecycle_hint: &'a str,
}

/// UDP 腿的判定链。
///
/// 顺序是这套判定的核心资产，不是实现细节：**采样/负载是否可信，永远排在
/// 任何 CPE 性能结论之前**。灌不够、窗口不完整、覆盖率不足时必须产出
/// `NOT_EVALUATED`，绝不能写成 `RATE_FAIL`——把环境异常记成被测设备不达标，
/// 是这套判定一直在防的误判方向。
pub(super) fn udp_leg_verdict(facts: &UdpLegFacts<'_>) -> VerdictResult {
    let &UdpLegFacts {
        streams_total: n,
        streams_success: success,
        streams_required: required,
        runtime_failures,
        single_stream_exhausted,
        single_attempts,
        window: effective_window,
        rx: rx_stats,
        tx: tx_stats,
        rate_mode,
        rx_target_mbps,
        offered_floor,
        udp_loss,
        max_udp_loss_pct,
        rx_lifecycle_hint,
    } = facts;

    let rate_present = rx_stats
        .avg_mbps
        .map(|v| v > MIN_VALID_RX_MBPS)
        .unwrap_or(false);
    let tx_sufficient = offered_floor
        .map(|floor| tx_stats.p10_mbps.map(|v| v >= floor).unwrap_or(false))
        .unwrap_or(true);
    let sample_coverage_sufficient =
        rate_sample_coverage_sufficient(rx_stats, tx_stats, rx_target_mbps.is_some());
    let rate_window_coverage_sufficient =
        rate_window_coverage_sufficient(rx_stats, tx_stats, rx_target_mbps.is_some());
    // 合格线只有平均速率一条，与 TCP/CTS 路径（`evaluate_nic_rx`）同口径。
    // P10 退回诊断指标，不再参与 PASS/FAIL。
    let rx_meets_target = rx_target_mbps
        .map(|target| rx_stats.avg_mbps.map(|v| v >= target).unwrap_or(false))
        .unwrap_or(true);
    let loss_ok = max_udp_loss_pct
        .map(|limit| udp_loss.map(|value| value <= limit))
        .unwrap_or(Some(true));

    if success == 0 {
        let verdict = zero_udp_stream_verdict(n, single_stream_exhausted);
        if verdict == Verdict::RateFail {
            VerdictResult::new(
                verdict,
                ReasonCode::SingleUdpStreamFailed,
                format!(
                    "单流 UDP 在 {single_attempts} 次 client 尝试后仍未产生有效测量；该方向必须灌通"
                ),
            )
        } else {
            VerdictResult::new(
                verdict,
                ReasonCode::NoStreamStarted,
                format!("0/{n} 条流产生有效测量；执行环境未完成 client 尝试"),
            )
        }
    } else if runtime_failures > 0 {
        VerdictResult::rate_fail(
            ReasonCode::IperfRuntimeErrors,
            format!("{runtime_failures} 条流已有 iperf3 自身吞吐测量，但 client 非正常完成或超时"),
        )
    } else if required > n {
        VerdictResult::not_evaluated(
            ReasonCode::ConfiguredLoadTooLow,
            format!("目标需要至少 {required} 条流，但只配置了 {n} 条"),
        )
    } else if success < required {
        VerdictResult::not_evaluated(
            ReasonCode::ActiveStreamsLow,
            format!("仅 {success}/{n} 条流成功，正式判定至少需要 {required} 条"),
        )
    } else if !effective_window.complete {
        VerdictResult::not_evaluated(
            ReasonCode::EffectiveWindowShort,
            format!(
                "本方向有效窗口 {:.1}s，要求 {}s{}",
                effective_window.available_secs, effective_window.required_secs, rx_lifecycle_hint
            ),
        )
    } else if rx_stats.stalled_ratio > 1.0 - MIN_RATE_SAMPLE_COVERAGE {
        // 与 evaluate_nic_rx 的同名判据保持一致：两条判定链在
        // 「采样是否可信」上必须给出相同结论，否则同一种故障在
        // TCP 和 UDP 路径上会被写成两种不同的原因码。
        VerdictResult::not_evaluated(
            ReasonCode::CounterStalled,
            format!(
                "判定窗口内接收端 OS 网卡计数器有 {:.1}% 的时间零增长（采到了样本，\
                 但字节计数一直没推进），本轮平均速率不可信",
                rx_stats.stalled_ratio * 100.0
            ),
        )
    } else if !rate_present || !sample_coverage_sufficient {
        VerdictResult::not_evaluated(
            ReasonCode::SampleCoverageLow,
            format!(
                "RX采样覆盖率 {:.1}%，TX采样覆盖率 {:.1}%{}，或无有效接收速率",
                rx_stats.coverage * 100.0,
                tx_stats.coverage * 100.0,
                if rx_target_mbps.is_some() {
                    "（有目标时两端均要求至少 95%）"
                } else {
                    ""
                }
            ),
        )
    } else if !rate_window_coverage_sufficient {
        VerdictResult::not_evaluated(
            ReasonCode::RateWindowCoverageLow,
            format!(
                "完整5秒滚动窗口覆盖不足（RX {:.1}%/P10={}，TX {:.1}%/P10={}，要求均至少95%），不能用少量窗口或跨周期恢复样本替代稳定性判定",
                rx_stats.rolling_coverage * 100.0,
                fmt_opt(rx_stats.p10_mbps),
                tx_stats.rolling_coverage * 100.0,
                fmt_opt(tx_stats.p10_mbps)
            ),
        )
    } else if rx_target_mbps.is_none() && rate_mode == RateMode::Verify {
        VerdictResult::not_evaluated(
            ReasonCode::TargetMissing,
            "verify 模式必须配置有效的 rate_targets_mbps，且当前路径没有自动 EVB 目标".to_string(),
        )
    } else if rx_target_mbps.is_none() {
        VerdictResult::measured(
            ReasonCode::TargetUnknown,
            format!("{:?} 模式仅记录实际能力，不伪造 PASS/FAIL", rate_mode),
        )
    } else if loss_ok.is_none() {
        VerdictResult::not_evaluated(
            ReasonCode::UdpLossDataMissing,
            "已配置 UDP 丢包门槛，但 iperf3 输出缺少 lost/total 数据".to_string(),
        )
    } else if !tx_sufficient && offered_shortfall_explains_rx(rx_stats.avg_mbps, tx_stats.p10_mbps)
    {
        VerdictResult::not_evaluated(
            ReasonCode::OfferedLoadLow,
            format!(
                "TX-P10 {}，验证目标所需负载至少 {}；接收端 {} 基本等于发出的量，\
                 无法判断被测设备还能不能再多送",
                fmt_opt(tx_stats.p10_mbps),
                fmt_opt(offered_floor),
                fmt_opt(rx_stats.avg_mbps)
            ),
        )
    } else if !rx_meets_target {
        let target = rx_target_mbps.unwrap_or_default();
        // 没灌够却仍然判 FAIL 时，把理由说全：不然读报告的人会
        // 拿「TX 没到门限+余量」来质疑这个结论。
        let offered_note = if tx_sufficient {
            String::new()
        } else {
            format!(
                "（发送端 TX-P10 {} 未达目标+余量 {}，但接收端只有 {}，\
                 缺口远大于发送端少灌的部分，补足负载也补不回来）",
                fmt_opt(tx_stats.p10_mbps),
                fmt_opt(offered_floor),
                fmt_opt(rx_stats.avg_mbps)
            )
        };
        VerdictResult::rate_fail(
            ReasonCode::RxBelowTarget,
            format!(
                "RX平均 {} 低于目标 {}Mbps{offered_note}",
                fmt_opt(rx_stats.avg_mbps),
                target
            ),
        )
    } else if let Some(excursion) =
        rx_target_mbps.and_then(|target| rate_excursion(&rx_stats.series, target))
    {
        // 平均达标，但中间连续断/掉/冲过。平均值答不出这件事，
        // 使用者却看得见。与 TCP/CTS 路径同一判据、同一口径。
        VerdictResult::rate_fail(
            excursion.reason_code(),
            format!("平均速率达标，但{}", excursion.describe()),
        )
    } else if loss_ok == Some(false) {
        VerdictResult::rate_fail(
            ReasonCode::UdpLossHigh,
            format!(
                "UDP平均丢包率 {:.3}% 超过限制 {:.3}%",
                udp_loss.unwrap_or_default(),
                max_udp_loss_pct.unwrap_or_default()
            ),
        )
    } else {
        VerdictResult::pass()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 判定链现在是纯函数，给一份事实就能验口径——不必起进程、不必连对端。
    /// 这正是把它从 `run_udp_unit` 里抽出来的目的：口径是这套工具最贵的资产，
    /// 却曾经只能靠端到端用例间接覆盖。
    fn facts<'a>(
        rx: &'a RateStats,
        tx: &'a RateStats,
        window: &'a EffectiveWindow,
    ) -> UdpLegFacts<'a> {
        UdpLegFacts {
            streams_total: 1,
            streams_success: 1,
            streams_required: 1,
            runtime_failures: 0,
            single_stream_exhausted: false,
            single_attempts: 1,
            window,
            rx,
            tx,
            rate_mode: RateMode::Verify,
            rx_target_mbps: Some(1_000.0),
            offered_floor: Some(1_050.0),
            udp_loss: Some(0.0),
            max_udp_loss_pct: None,
            rx_lifecycle_hint: "",
        }
    }

    fn healthy(avg: f64) -> RateStats {
        RateStats {
            avg_mbps: Some(avg),
            p10_mbps: Some(avg),
            coverage: 1.0,
            rolling_coverage: 1.0,
            series: (1..=180).map(|i| (i * 1_000, 1_000, avg)).collect(),
            ..Default::default()
        }
    }

    fn full_window() -> EffectiveWindow {
        EffectiveWindow {
            start_ms: 0,
            end_ms: 180_000,
            available_secs: 180.0,
            required_secs: 180,
            complete: true,
        }
    }

    #[test]
    fn a_clean_run_passes() {
        let (rx, tx, w) = (healthy(1_200.0), healthy(1_200.0), full_window());
        assert_eq!(udp_leg_verdict(&facts(&rx, &tx, &w)), VerdictResult::pass());
    }

    /// 灌不够时接收端低于目标**不能**算被测设备不达标。
    ///
    /// 这是整条链最容易写反的一处：发送端只灌了 800，接收端收到 790，
    /// 「接收端低于 1000」是事实，但它证明不了设备送不出 1000。
    #[test]
    fn an_offered_load_shortfall_is_not_a_device_failure() {
        let rx = healthy(790.0);
        let tx = healthy(800.0);
        let w = full_window();
        let judgement = udp_leg_verdict(&facts(&rx, &tx, &w));
        assert_eq!(judgement.verdict, Verdict::NotEvaluated);
        assert_eq!(judgement.code, ReasonCode::OfferedLoadLow);
    }

    /// 反过来：发送端少灌 50、接收端却只有 580，缺口远大于少灌的部分，
    /// FAIL 在这一轮就已经成立，不能拿「没灌够」把它藏起来。
    #[test]
    fn a_receiver_that_lost_far_more_than_the_shortfall_still_fails() {
        let rx = healthy(580.0);
        let tx = healthy(1_000.0);
        let w = full_window();
        let judgement = udp_leg_verdict(&facts(&rx, &tx, &w));
        assert_eq!(judgement.verdict, Verdict::RateFail);
        assert_eq!(judgement.code, ReasonCode::RxBelowTarget);
    }

    /// 采样不可信时永远不许对被测设备下结论——顺序铁律。
    #[test]
    fn untrustworthy_sampling_never_becomes_a_device_failure() {
        let w = full_window();
        let tx = healthy(1_200.0);

        let mut low_coverage = healthy(500.0);
        low_coverage.coverage = 0.5;
        let judgement = udp_leg_verdict(&facts(&low_coverage, &tx, &w));
        assert_eq!(
            judgement.verdict,
            Verdict::NotEvaluated,
            "覆盖率不足却判了 {judgement:?}"
        );

        let mut stalled = healthy(500.0);
        stalled.stalled_ratio = 0.9;
        let judgement = udp_leg_verdict(&facts(&stalled, &tx, &w));
        assert_eq!(judgement.code, ReasonCode::CounterStalled);
    }

    /// 平均达标但中途连续掉坑：FAIL，且报的是真秒数。
    #[test]
    fn a_real_dropout_still_fails_a_run_whose_average_is_fine() {
        let mut rx = healthy(1_200.0);
        // 第 30~36 秒掉到门限 80% 以下，连续 7 秒。
        rx.series = (1..=180)
            .map(|i| {
                let rate = if (30..=36).contains(&i) {
                    200.0
                } else {
                    1_200.0
                };
                (i * 1_000, 1_000, rate)
            })
            .collect();
        let tx = healthy(1_200.0);
        let w = full_window();
        let judgement = udp_leg_verdict(&facts(&rx, &tx, &w));
        assert_eq!(judgement.verdict, Verdict::RateFail);
        assert_eq!(judgement.code, ReasonCode::RxDropout);
        assert!(judgement.detail.contains("连续 7.0 秒"), "{judgement:?}");
    }

    /// 一个采样周期的掉拍不算数：Wi-Fi 发 probe 就是这个形态。
    #[test]
    fn a_single_period_blip_does_not_fail_the_leg() {
        let mut rx = healthy(1_200.0);
        rx.series = (1..=180)
            .map(|i| (i * 1_000, 1_000, if i == 60 { 0.0 } else { 1_200.0 }))
            .collect();
        let tx = healthy(1_200.0);
        let w = full_window();
        assert_eq!(udp_leg_verdict(&facts(&rx, &tx, &w)).verdict, Verdict::Pass);
    }
}
