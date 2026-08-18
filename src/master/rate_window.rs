//! 网卡采样统计与速率判定层。
//!
//! 从 `master::executor` 抽出的**纯函数层**：只依赖 OS 网卡计数器样本、有效
//! 时间窗口和目标速率，不接触任何执行状态（进程、端口、HTTP、线程）。
//!
//! 单独成模块的理由不是"文件太大"，而是这里是**正式判定口径的所在地**——
//! 验收文档反复强调的两条铁律都在这一层实现：
//!
//! 1. 正式口径永远是接收端 OS 网卡计数器，工具自报速率只作诊断；
//! 2. 采样不可信时必须产出 `NOT_EVALUATED`，绝不能写成 CPE 性能失败。
//!
//! 第 2 条曾经被违反：`evaluate_nic_rx` 一度把 `RX_BELOW_TARGET` 判在滚动窗口
//! 覆盖率检查之前，导致"网卡计数器中断"被报成"CPE 不达标"。把这一层单独隔出
//! 来，是为了让判定顺序成为一件能被单独审阅和测试的事。

use crate::config::RateMode;
// 「有效流量」下限与采样层共用同一个常量，避免两处阈值漂移。
pub use crate::nic::monitor::MIN_VALID_RX_MBPS;
use crate::protocol::{MonitorSample, MonitorStopOut};
use crate::verdict::Verdict;

/// 接收端网卡 RX 采样覆盖率下限；低于它不允许做正式性能判定。
pub const MIN_RATE_SAMPLE_COVERAGE: f64 = 0.95;
/// 稳定性判定使用的滚动窗口长度。
pub const ROLLING_RATE_WINDOW_MS: u64 = 5_000;
/// 毫秒取整造成的窗口拼接误差容忍；只容忍舍入，不容忍真正的漏采周期。
pub const ROLLING_COVERAGE_TOLERANCE_MS: u64 = 50;

#[derive(Debug, Clone, Default)]
pub(crate) struct RateStats {
    pub avg_mbps: Option<f64>,
    pub p10_mbps: Option<f64>,
    pub median_mbps: Option<f64>,
    pub p95_mbps: Option<f64>,
    pub min_mbps: Option<f64>,
    pub max_mbps: Option<f64>,
    pub coverage: f64,
    /// 实际可形成的完整 5 秒滚动窗口占理论窗口数的比例。
    ///
    /// 总采样覆盖率高并不代表稳定性窗口也完整：一次跨越多个失败周期的
    /// 恢复样本可以补齐平均速率覆盖，却不能证明其中任意 5 秒都稳定。
    pub rolling_coverage: f64,
    /// 起流前空闲期的背景速率中位数，已从每个样本里扣除。
    ///
    /// 验收要求核对「原始网卡总流量与报告业务流量差值接近背景值」，
    /// 但差值只有把扣除量本身报出来才可核对。
    pub baseline_mbps: f64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EffectiveWindow {
    pub start_ms: u64,
    pub end_ms: u64,
    pub available_secs: f64,
    pub required_secs: u64,
    pub complete: bool,
}

/// 按接收端 OS 网卡 RX 做正式速率判定。
///
/// `tx_stats` 是**发送端**网卡的同窗口统计。验收文档 W08 要求「有明确目标时，
/// RX/TX 任一侧完整滚动窗口覆盖率低于 95% 均为 NOT_EVALUATED」——发送端采样
/// 塌了同样说明这一轮的时间轴不可信，不能拿去给 CPE 定性。没有目标时（observe
/// / discover / 目标未知）只记录实测能力，不需要双侧门槛。
pub(crate) fn evaluate_nic_rx(
    mode: RateMode,
    target_mbps: Option<f64>,
    stats: &RateStats,
    tx_stats: &RateStats,
) -> (Verdict, String, String) {
    let Some(rx_avg) = stats
        .avg_mbps
        .filter(|value| value.is_finite() && *value > MIN_VALID_RX_MBPS)
    else {
        return (
            Verdict::NotEvaluated,
            "NIC_RATE_MISSING".into(),
            "有效流量窗口内没有可用的接收端 OS 网卡 RX 速率".into(),
        );
    };
    if !stats.coverage.is_finite() || stats.coverage < MIN_RATE_SAMPLE_COVERAGE {
        return (
            Verdict::NotEvaluated,
            "SAMPLE_COVERAGE_LOW".into(),
            format!(
                "接收端网卡 RX 采样覆盖率 {:.1}%，低于 {:.1}%",
                stats.coverage * 100.0,
                MIN_RATE_SAMPLE_COVERAGE * 100.0
            ),
        );
    }
    let target_mbps = if matches!(mode, RateMode::Observe | RateMode::Discover) {
        None
    } else {
        target_mbps.filter(|value| value.is_finite() && *value > 0.0)
    };
    let Some(target) = target_mbps else {
        return if mode == RateMode::Verify {
            (
                Verdict::NotEvaluated,
                "TARGET_MISSING".into(),
                "verify 模式必须配置可信的接收端网卡 RX 目标".into(),
            )
        } else {
            (
                Verdict::Measured,
                "TARGET_UNKNOWN".into(),
                format!("接收端网卡 RX 已测得 {rx_avg:.3}Mbps；未配置可信目标，因此不标记 PASS"),
            )
        };
    };
    // 到这里已经有明确目标：采样门槛升级为双侧，与 UDP 路径共用同两个谓词，
    // 避免两条链再次分叉。
    if !rate_sample_coverage_sufficient(stats, tx_stats, true) {
        return (
            Verdict::NotEvaluated,
            "SAMPLE_COVERAGE_LOW".into(),
            format!(
                "发送端网卡 TX 采样覆盖率 {:.1}%，低于 {:.1}%；有目标时两端采样都必须完整",
                tx_stats.coverage * 100.0,
                MIN_RATE_SAMPLE_COVERAGE * 100.0
            ),
        );
    }

    // 采样是否可信必须先于任何 CPE 性能结论，顺序与 UDP 路径（run_udp_unit 的
    // 判定链）保持一致。
    //
    // 总覆盖率可以被一条跨越失败周期的长恢复样本补齐到 100%，但那段时间里
    // 任意一个完整 5 秒窗口都不成立，基于同一批样本算出的加权均值同样不可信。
    // 若先判 RX_BELOW_TARGET，就会把「网卡计数器中断」这种环境异常写成
    // 「CPE 不达标」的 RATE_FAIL —— 正是验收文档要求禁止的误判方向。
    let rx_p10 = stats
        .p10_mbps
        .filter(|value| value.is_finite() && *value >= 0.0);
    if !stats.rolling_coverage.is_finite()
        || rx_p10.is_none()
        || !rate_window_coverage_sufficient(stats, tx_stats, true)
    {
        return (
            Verdict::NotEvaluated,
            "RATE_WINDOW_COVERAGE_LOW".into(),
            format!(
                "完整 5 秒滚动窗口覆盖率 RX {:.1}% / TX {:.1}%，低于 {:.1}%，无法计算可信 P10；\
                 本轮采样不足以判定 CPE 性能",
                stats.rolling_coverage * 100.0,
                tx_stats.rolling_coverage * 100.0,
                MIN_RATE_SAMPLE_COVERAGE * 100.0
            ),
        );
    }
    let rx_p10 = rx_p10.unwrap_or_default();
    if rx_avg < target {
        return (
            Verdict::RateFail,
            "RX_BELOW_TARGET".into(),
            format!("网卡 RX 平均 {rx_avg:.3}Mbps 低于目标 {target:.3}Mbps"),
        );
    }
    if rx_p10 < target {
        return (
            Verdict::Unstable,
            "RX_UNSTABLE".into(),
            format!(
                "网卡 RX 平均 {rx_avg:.3}Mbps 已达标，但 RX-P10 {rx_p10:.3}Mbps 低于目标 {target:.3}Mbps"
            ),
        );
    }
    (Verdict::Pass, String::new(), String::new())
}

pub(crate) fn rate_sample_coverage_sufficient(
    rx_stats: &RateStats,
    tx_stats: &RateStats,
    target_present: bool,
) -> bool {
    rx_stats.coverage >= MIN_RATE_SAMPLE_COVERAGE
        && (!target_present || tx_stats.coverage >= MIN_RATE_SAMPLE_COVERAGE)
}

pub(crate) fn rate_window_coverage_sufficient(
    rx_stats: &RateStats,
    tx_stats: &RateStats,
    target_present: bool,
) -> bool {
    !target_present
        || (rx_stats.p10_mbps.is_some()
            && tx_stats.p10_mbps.is_some()
            && rx_stats.rolling_coverage >= MIN_RATE_SAMPLE_COVERAGE
            && tx_stats.rolling_coverage >= MIN_RATE_SAMPLE_COVERAGE)
}

pub(crate) fn nearest_valid_sample(
    out: &MonitorStopOut,
    elapsed_ms: u64,
    max_distance_ms: u64,
) -> Option<&MonitorSample> {
    out.samples
        .iter()
        .filter(|sample| sample.valid)
        .min_by_key(|sample| sample.elapsed_ms.abs_diff(elapsed_ms))
        .filter(|sample| sample.elapsed_ms.abs_diff(elapsed_ms) <= max_distance_ms)
}

pub(crate) fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = (((sorted.len() - 1) as f64) * q.clamp(0.0, 1.0)).round() as usize;
    sorted.get(idx).copied()
}

pub(crate) fn rolling_time_window_averages(
    samples: &[(u64, u64, f64)],
    range_start_ms: u64,
    window_ms: u64,
) -> Vec<f64> {
    if window_ms == 0 {
        return samples.iter().map(|(_, _, rate)| *rate).collect();
    }

    let mut rolling = Vec::new();
    for (window_end_ms, _, _) in samples {
        let window_start_ms = window_end_ms.saturating_sub(window_ms);
        if window_start_ms < range_start_ms
            || window_end_ms.saturating_sub(window_start_ms) < window_ms
        {
            continue;
        }

        let mut weighted_sum = 0.0;
        let mut covered_ms = 0u64;
        for (sample_end_ms, interval_ms, rate) in samples {
            if *interval_ms == 0 || *sample_end_ms <= window_start_ms {
                continue;
            }
            if *sample_end_ms > *window_end_ms {
                break;
            }
            let sample_start_ms = sample_end_ms
                .saturating_sub(*interval_ms)
                .max(range_start_ms);
            let overlap_start = sample_start_ms.max(window_start_ms);
            let overlap_end = (*sample_end_ms).min(*window_end_ms);
            let overlap_ms = overlap_end.saturating_sub(overlap_start);
            if overlap_ms > 0 {
                weighted_sum += *rate * overlap_ms as f64;
                covered_ms = covered_ms.saturating_add(overlap_ms);
            }
        }
        // 只把实际样本完整覆盖的五秒区间纳入稳定性判定；缺口由 coverage
        // 另行约束，不能用相邻样本跨越缺口拼出一个虚假的五秒窗口。
        // elapsed_ms/interval_ms 均由 Duration 向下取整为毫秒，多个样本边界可能
        // 累积出数毫秒的舍入缝隙；只容忍极小误差，不能容忍真正的漏采周期。
        if covered_ms.saturating_add(ROLLING_COVERAGE_TOLERANCE_MS) >= window_ms {
            rolling.push(weighted_sum / covered_ms as f64);
        }
    }
    rolling
}

pub(crate) fn nominal_monitor_interval_ms(
    out: &MonitorStopOut,
    window: &EffectiveWindow,
) -> Option<u64> {
    let mut all = Vec::new();
    let mut interior = Vec::new();
    for sample in &out.samples {
        if sample.interval_ms == 0
            || sample.elapsed_ms <= window.start_ms
            || sample.elapsed_ms.saturating_sub(sample.interval_ms) >= window.end_ms
        {
            continue;
        }
        all.push(sample.interval_ms);
        // stop 唤醒产生的最后一个样本通常短于正常周期，优先用完全处于
        // 窗口内部的周期推断 nominal interval，避免边界样本拉低结果。
        if sample.elapsed_ms.saturating_sub(sample.interval_ms) >= window.start_ms
            && sample.elapsed_ms < window.end_ms
        {
            interior.push(sample.interval_ms);
        }
    }
    let intervals = if interior.is_empty() {
        &mut all
    } else {
        &mut interior
    };
    if intervals.is_empty() {
        return None;
    }
    intervals.sort_unstable();
    // 取较保守的下中位数，避免“一个正常周期 + 一个跨周期恢复样本”把
    // nominal interval 放大到足以让恢复样本伪装成稳定窗口。MonitorMgr
    // 的真实配置上限为 5 秒，额外封顶也能识别线程长时间失调度的样本。
    Some(intervals[(intervals.len() - 1) / 2].min(ROLLING_RATE_WINDOW_MS))
}

pub(crate) fn monitor_rate_stats(
    out: &MonitorStopOut,
    window: &EffectiveWindow,
    rx: bool,
    first_active_ms: u64,
) -> RateStats {
    if window.end_ms <= window.start_ms {
        return RateStats::default();
    }
    let mut baseline_values: Vec<f64> = out
        .samples
        .iter()
        .filter(|sample| {
            sample.valid
                && sample.interval_ms > 0
                && sample.elapsed_ms > 0
                && sample.elapsed_ms <= first_active_ms
                && (if rx { sample.rx_mbps } else { sample.tx_mbps }).is_finite()
        })
        .map(|sample| if rx { sample.rx_mbps } else { sample.tx_mbps })
        .collect();
    baseline_values.sort_by(|a, b| a.total_cmp(b));
    let baseline = percentile(&baseline_values, 0.5).unwrap_or(0.0);
    let nominal_interval_ms = nominal_monitor_interval_ms(out, window);
    let max_rolling_sample_ms = nominal_interval_ms.map(|nominal| {
        nominal
            .saturating_mul(3)
            .saturating_div(2)
            .saturating_add(ROLLING_COVERAGE_TOLERANCE_MS)
    });

    // 每个速率样本代表 [elapsed-interval, elapsed) 的一段时间，而不是一个
    // 等权点。先裁到正式判定窗口，再去掉因毫秒取整或异常输入造成的重叠。
    let mut clipped_samples: Vec<(u64, u64, f64, bool)> = out
        .samples
        .iter()
        .filter(|sample| {
            sample.valid
                && sample.interval_ms > 0
                && sample.elapsed_ms > window.start_ms
                && sample.elapsed_ms.saturating_sub(sample.interval_ms) < window.end_ms
                && (if rx { sample.rx_mbps } else { sample.tx_mbps }).is_finite()
        })
        .filter_map(|sample| {
            let value = if rx { sample.rx_mbps } else { sample.tx_mbps };
            let start_ms = sample
                .elapsed_ms
                .saturating_sub(sample.interval_ms)
                .max(window.start_ms);
            let end_ms = sample.elapsed_ms.min(window.end_ms);
            (end_ms > start_ms).then_some((
                start_ms,
                end_ms,
                (value - baseline).max(0.0),
                max_rolling_sample_ms
                    .is_some_and(|max_interval| sample.interval_ms <= max_interval),
            ))
        })
        .collect();
    clipped_samples.sort_by_key(|(start_ms, end_ms, _, _)| (*start_ms, *end_ms));

    let mut rate_samples: Vec<(u64, u64, f64)> = Vec::with_capacity(clipped_samples.len());
    let mut rolling_rate_samples: Vec<(u64, u64, f64)> = Vec::with_capacity(clipped_samples.len());
    let mut covered_until_ms = window.start_ms;
    let mut rolling_covered_until_ms = window.start_ms;
    for (sample_start_ms, sample_end_ms, rate, rolling_eligible) in clipped_samples {
        let non_overlapping_start_ms = sample_start_ms.max(covered_until_ms);
        if sample_end_ms > non_overlapping_start_ms {
            rate_samples.push((
                sample_end_ms,
                sample_end_ms - non_overlapping_start_ms,
                rate,
            ));
            covered_until_ms = sample_end_ms;
        }
        if rolling_eligible {
            let rolling_start_ms = sample_start_ms.max(rolling_covered_until_ms);
            if sample_end_ms > rolling_start_ms {
                rolling_rate_samples.push((sample_end_ms, sample_end_ms - rolling_start_ms, rate));
                rolling_covered_until_ms = sample_end_ms;
            }
        }
    }

    let mut rates: Vec<f64> = rate_samples.iter().map(|(_, _, rate)| *rate).collect();
    if rates.is_empty() {
        return RateStats::default();
    }
    let covered_ms: u64 = rate_samples
        .iter()
        .map(|(_, interval_ms, _)| *interval_ms)
        .sum();
    if covered_ms == 0 {
        return RateStats::default();
    }
    let avg = rate_samples
        .iter()
        .map(|(_, interval_ms, rate)| *rate * *interval_ms as f64)
        .sum::<f64>()
        / covered_ms as f64;
    let min = rates.iter().copied().fold(f64::INFINITY, f64::min);
    let max = rates.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let rolling = rolling_time_window_averages(
        &rolling_rate_samples,
        window.start_ms,
        ROLLING_RATE_WINDOW_MS,
    );
    rates.sort_by(|a, b| a.total_cmp(b));
    let mut rolling_sorted = rolling;
    rolling_sorted.sort_by(|a, b| a.total_cmp(b));
    let window_ms = window.end_ms - window.start_ms;
    let expected_rolling_windows = nominal_interval_ms
        .filter(|nominal| *nominal > 0 && window_ms >= ROLLING_RATE_WINDOW_MS)
        .map(|nominal| {
            window_ms
                .saturating_sub(ROLLING_RATE_WINDOW_MS)
                .saturating_div(nominal)
                .saturating_add(1)
        })
        .unwrap_or(0);
    let rolling_coverage = if expected_rolling_windows == 0 {
        0.0
    } else {
        (rolling_sorted.len() as f64 / expected_rolling_windows as f64).min(1.0)
    };
    RateStats {
        avg_mbps: Some(avg),
        p10_mbps: percentile(&rolling_sorted, 0.10),
        median_mbps: percentile(&rates, 0.50),
        p95_mbps: percentile(&rates, 0.95),
        min_mbps: Some(min),
        max_mbps: Some(max),
        coverage: (covered_ms as f64 / window_ms as f64).min(1.0),
        rolling_coverage,
        baseline_mbps: baseline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两条判定链的一致性属性：**采样不可信时，谁都不许对 CPE 下结论**。
    ///
    /// UDP 走 `run_udp_unit` 的内联判定链（用下面两个谓词做门禁），TCP/CTS 走
    /// `evaluate_nic_rx`。两者曾经在这一点上分叉：`evaluate_nic_rx` 把
    /// `RX_BELOW_TARGET` 判在滚动窗口覆盖率之前，于是"网卡计数器中断"被写成
    /// "CPE 不达标"。分叉之所以能长期存在，是因为两条链各自的用例都是绿的——
    /// 只有把它们放在同一组输入下对比，才看得出来。
    /// W08：**有明确目标时**，RX/TX 任一侧采样塌了都不能对 CPE 定性。
    ///
    /// 这条曾经只在 UDP 路径成立——TCP/CTS 压根不采样发送端网卡，于是发送端
    /// 计数器失效时它们照样给出 PASS/RATE_FAIL。目标未知时不需要双侧门槛：
    /// 那时只记录实测能力，不做合格性承诺。
    #[test]
    fn a_collapsed_sender_side_sampling_also_blocks_any_cpe_verdict() {
        let healthy = RateStats {
            avg_mbps: Some(900.0),
            p10_mbps: Some(880.0),
            coverage: 1.0,
            rolling_coverage: 1.0,
            ..Default::default()
        };
        let target = Some(800.0);

        // 基线：两侧都完好 → 正常给出 PASS。
        assert_eq!(
            evaluate_nic_rx(RateMode::Verify, target, &healthy, &healthy).0,
            Verdict::Pass
        );

        // 发送端总采样覆盖率塌了。
        let tx_low_coverage = RateStats {
            coverage: 0.80,
            ..healthy.clone()
        };
        let (verdict, code, detail) =
            evaluate_nic_rx(RateMode::Verify, target, &healthy, &tx_low_coverage);
        assert_eq!(
            (verdict, code.as_str()),
            (Verdict::NotEvaluated, "SAMPLE_COVERAGE_LOW")
        );
        assert!(detail.contains("发送端"), "原因必须指明是发送端: {detail}");

        // 发送端滚动窗口覆盖率塌了（总覆盖率仍满，典型的跨周期恢复样本场景）。
        let tx_low_rolling = RateStats {
            rolling_coverage: 0.70,
            ..healthy.clone()
        };
        let (verdict, code, _) =
            evaluate_nic_rx(RateMode::Verify, target, &healthy, &tx_low_rolling);
        assert_eq!(
            (verdict, code.as_str()),
            (Verdict::NotEvaluated, "RATE_WINDOW_COVERAGE_LOW")
        );

        // 发送端根本没采到样本。
        let (verdict, _, _) =
            evaluate_nic_rx(RateMode::Verify, target, &healthy, &RateStats::default());
        assert_eq!(verdict, Verdict::NotEvaluated);

        // 目标未知时不做双侧要求：只记录实测能力。
        assert_eq!(
            evaluate_nic_rx(RateMode::Observe, None, &healthy, &RateStats::default()).0,
            Verdict::Measured
        );
    }

    #[test]
    fn neither_backend_blames_the_dut_while_sampling_is_untrustworthy() {
        let target = 800.0;
        // 覆盖率 × 滚动覆盖率 × P10 是否可得，遍历"可信/不可信"的各种组合。
        for &coverage in &[1.0_f64, 0.99, 0.94, 0.0] {
            for &rolling in &[1.0_f64, 0.96, 0.86, 0.0] {
                for &p10 in &[Some(700.0_f64), Some(820.0), None] {
                    // 均值刻意压在目标之下：这正是最容易被误判成 RATE_FAIL 的输入。
                    let rx = RateStats {
                        avg_mbps: Some(799.0),
                        p10_mbps: p10,
                        coverage,
                        rolling_coverage: rolling,
                        ..Default::default()
                    };
                    // 发送端采样始终完好，隔离出接收端可信度这一个变量。
                    let tx = RateStats {
                        avg_mbps: Some(840.0),
                        p10_mbps: Some(830.0),
                        coverage: 1.0,
                        rolling_coverage: 1.0,
                        ..Default::default()
                    };

                    let udp_trusts_sampling = rate_sample_coverage_sufficient(&rx, &tx, true)
                        && rate_window_coverage_sufficient(&rx, &tx, true);
                    let (verdict, code, _) =
                        evaluate_nic_rx(RateMode::Verify, Some(target), &rx, &tx);

                    if !udp_trusts_sampling {
                        assert_eq!(
                            verdict,
                            Verdict::NotEvaluated,
                            "UDP 链认为采样不可信（coverage={coverage}, rolling={rolling}, \
                             p10={p10:?}），TCP/CTS 链却给出了 {verdict:?}/{code}"
                        );
                    } else {
                        // 采样可信且均值低于目标时，两条链都必须落到真正的性能结论。
                        assert_eq!(
                            (verdict, code.as_str()),
                            (Verdict::RateFail, "RX_BELOW_TARGET"),
                            "采样可信时应产出真实的性能判定 \
                             (coverage={coverage}, rolling={rolling}, p10={p10:?})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn tcp_nic_rx_verdict_matrix_never_passes_without_complete_authoritative_data() {
        let complete = RateStats {
            avg_mbps: Some(850.0),
            p10_mbps: Some(820.0),
            coverage: 1.0,
            rolling_coverage: 1.0,
            ..Default::default()
        };
        // 发送端采样默认完好，把变量隔离在接收端；TX 侧门槛另有专门用例。
        let healthy_tx = RateStats {
            avg_mbps: Some(900.0),
            p10_mbps: Some(880.0),
            coverage: 1.0,
            rolling_coverage: 1.0,
            ..Default::default()
        };
        let decision = |mode, target, stats: &RateStats| {
            let (verdict, code, _) = evaluate_nic_rx(mode, target, stats, &healthy_tx);
            (verdict, code)
        };

        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &RateStats::default()),
            (Verdict::NotEvaluated, "NIC_RATE_MISSING".into())
        );
        let zero = RateStats {
            avg_mbps: Some(0.0),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &zero),
            (Verdict::NotEvaluated, "NIC_RATE_MISSING".into())
        );

        assert_eq!(
            decision(RateMode::Observe, Some(800.0), &complete),
            (Verdict::Measured, "TARGET_UNKNOWN".into()),
            "observe 即使收到意外目标也只能记录测量值"
        );
        assert_eq!(
            decision(RateMode::Discover, Some(800.0), &complete),
            (Verdict::Measured, "TARGET_UNKNOWN".into())
        );
        assert_eq!(
            decision(RateMode::Auto, None, &complete),
            (Verdict::Measured, "TARGET_UNKNOWN".into())
        );
        assert_eq!(
            decision(RateMode::Verify, None, &complete),
            (Verdict::NotEvaluated, "TARGET_MISSING".into())
        );
        assert_eq!(
            decision(RateMode::Verify, Some(f64::NAN), &complete),
            (Verdict::NotEvaluated, "TARGET_MISSING".into())
        );

        let low_coverage = RateStats {
            coverage: 0.94,
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &low_coverage),
            (Verdict::NotEvaluated, "SAMPLE_COVERAGE_LOW".into())
        );
        let nan_coverage = RateStats {
            coverage: f64::NAN,
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &nan_coverage),
            (Verdict::NotEvaluated, "SAMPLE_COVERAGE_LOW".into())
        );

        let below_target = RateStats {
            avg_mbps: Some(799.0),
            p10_mbps: Some(790.0),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &below_target),
            (Verdict::RateFail, "RX_BELOW_TARGET".into())
        );

        // 采样不可信时不能给 CPE 扣帽子：总覆盖率被一条跨周期恢复样本补到
        // 100%，但完整 5 秒滚动窗口只有 86%，此时的加权均值同样不可信。
        // TCP/CTS 路径必须和 UDP 路径一样先判 RATE_WINDOW_COVERAGE_LOW。
        let below_target_but_unreliable = RateStats {
            avg_mbps: Some(799.0),
            p10_mbps: Some(700.0),
            coverage: 1.0,
            rolling_coverage: 0.86,
            ..Default::default()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &below_target_but_unreliable),
            (Verdict::NotEvaluated, "RATE_WINDOW_COVERAGE_LOW".into()),
            "滚动窗口覆盖不足时禁止产出 RATE_FAIL"
        );
        // 同样的输入在 UDP 路径上也是 RATE_WINDOW_COVERAGE_LOW，两条路径口径一致。
        let unreliable_p10_missing = RateStats {
            avg_mbps: Some(799.0),
            p10_mbps: None,
            coverage: 1.0,
            rolling_coverage: 1.0,
            ..Default::default()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &unreliable_p10_missing),
            (Verdict::NotEvaluated, "RATE_WINDOW_COVERAGE_LOW".into()),
            "窗口不足 5 秒导致 P10 缺失时同样不能判 RATE_FAIL"
        );
        let missing_p10 = RateStats {
            p10_mbps: None,
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &missing_p10),
            (Verdict::NotEvaluated, "RATE_WINDOW_COVERAGE_LOW".into())
        );
        let nan_p10 = RateStats {
            p10_mbps: Some(f64::NAN),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &nan_p10),
            (Verdict::NotEvaluated, "RATE_WINDOW_COVERAGE_LOW".into())
        );
        let low_rolling_coverage = RateStats {
            rolling_coverage: 0.94,
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &low_rolling_coverage),
            (Verdict::NotEvaluated, "RATE_WINDOW_COVERAGE_LOW".into())
        );

        let unstable = RateStats {
            p10_mbps: Some(799.0),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &unstable),
            (Verdict::Unstable, "RX_UNSTABLE".into())
        );
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &complete),
            (Verdict::Pass, String::new())
        );
    }
}
