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
    /// 按时间排列的完整 5 秒滚动窗口序列 `(窗口结束时刻ms, 均值Mbps)`。
    ///
    /// P10 排完序就没有顺序了，答不出「掉坑连续掉了多久」。判定需要的是
    /// 「有没有任何一个 5 秒掉到门限以下、掉了几个、连着掉了多长」，
    /// 这三件事都只能在有序序列上算。
    pub rolling_series: Vec<(u64, f64)>,
    /// 判定窗口内「计数器连续零增长」的最长一段占已覆盖时长的比例。
    ///
    /// 这是与采样覆盖率**正交**的一种不可信：样本采到了、`valid` 也是 true，
    /// 只是绝对计数器一个字节都没往前走。run_20260825_215915_7684 的
    /// unit-114-115 里 `rx_bytes` 在 elapsed 7078ms 之后 193 秒纹丝不动，
    /// 覆盖率却是 100%——光看覆盖率永远发现不了这件事。
    pub stalled_ratio: f64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EffectiveWindow {
    pub start_ms: u64,
    pub end_ms: u64,
    pub available_secs: f64,
    pub required_secs: u64,
    pub complete: bool,
}

/// 判定窗口内「掉到门限以下」的那些 5 秒滚动窗口。
///
/// 和 P10 是两件事：P10 只回答「有没有超过 10% 的窗口在门限以下」，一次
/// 5 秒的断流在 175 秒的测试里只占 3%，P10 完全看不见它。而对使用者来说，
/// 「全程平均 950、中间断了 5 秒」和「全程稳定 950」不是同一个结论。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RxDropout {
    /// 低于门限的窗口个数。
    pub windows: usize,
    /// 其中最低的一个窗口均值。
    pub lowest_mbps: f64,
    /// 连续掉在门限以下的最长一段时长（毫秒）。
    pub longest_ms: u64,
    /// 最长那一段的起始时刻（判定窗口内的相对毫秒）。
    pub started_at_ms: u64,
    /// 最低窗口是否已经接近零——这时该说「断流」而不是「掉坑」。
    pub stalled: bool,
}

impl RxDropout {
    /// 报告和日志里那句人话。
    pub fn describe(&self, target: f64) -> String {
        let kind = if self.stalled { "断流" } else { "掉坑" };
        format!(
            "{kind}：{} 个 5 秒窗口掉到门限 {target:.3}Mbps 以下，最低 {:.3}Mbps，\
             最长连续 {:.1} 秒（自判定窗口第 {:.1} 秒起）",
            self.windows,
            self.lowest_mbps,
            self.longest_ms as f64 / 1000.0,
            self.started_at_ms as f64 / 1000.0,
        )
    }
}

/// 找出掉到门限以下的滚动窗口；一个都没有就返回 `None`。
pub(crate) fn rx_dropout(series: &[(u64, f64)], target: f64) -> Option<RxDropout> {
    if !target.is_finite() || target <= 0.0 {
        return None;
    }
    let mut windows = 0usize;
    let mut lowest = f64::INFINITY;
    let mut longest_ms = 0u64;
    let mut longest_start = 0u64;
    // 连续段用「首个掉坑窗口的起点」到「最后一个掉坑窗口的终点」度量。
    // 每个窗口覆盖 ROLLING_RATE_WINDOW_MS，相邻窗口高度重叠，累加窗口数
    // 会把 5 秒的坑说成 175 秒。
    let mut run_start: Option<u64> = None;
    let mut run_end = 0u64;

    for (end_ms, rate) in series {
        if *rate < target {
            windows += 1;
            lowest = lowest.min(*rate);
            let start = end_ms.saturating_sub(ROLLING_RATE_WINDOW_MS);
            if run_start.is_none() {
                run_start = Some(start);
            }
            run_end = *end_ms;
        } else if let Some(start) = run_start.take() {
            let span = run_end.saturating_sub(start);
            if span > longest_ms {
                longest_ms = span;
                longest_start = start;
            }
        }
    }
    if let Some(start) = run_start {
        let span = run_end.saturating_sub(start);
        if span > longest_ms {
            longest_ms = span;
            longest_start = start;
        }
    }
    if windows == 0 {
        return None;
    }
    Some(RxDropout {
        windows,
        lowest_mbps: lowest,
        longest_ms,
        started_at_ms: longest_start,
        stalled: lowest <= MIN_VALID_RX_MBPS,
    })
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
    // 计数器停滞必须排在最前面：它命中的场景里 avg 通常也是 0，会被
    // NIC_RATE_MISSING 抢先吃掉，而「采到样本但计数器不动」比「没有可用速率」
    // 具体得多——前者直接指向链路或网卡侧，后者只说明这一行没结论。
    //
    // 门槛与采样覆盖率共用同一个常量：窗口里至少 95% 的时间要有真实推进的
    // 计数，剩下 5% 留给起流/收尾的空档。
    if stats.stalled_ratio > 1.0 - MIN_RATE_SAMPLE_COVERAGE {
        return (
            Verdict::NotEvaluated,
            "COUNTER_STALLED".into(),
            format!(
                "判定窗口内接收端 OS 网卡计数器有 {:.1}% 的时间零增长（采到了样本，\
                 但字节计数一直没推进），本轮平均速率不可信",
                stats.stalled_ratio * 100.0
            ),
        );
    }
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
    // 平均达标之后，还要求**判定窗口内每一个完整 5 秒都达标**。
    //
    // 这比 P10 严：一次 5 秒断流在 175 秒的测试里只占 3%，P10 看不见它，
    // 但「全程平均 950、中间断了 5 秒」和「全程稳定 950」对使用者不是同一个
    // 结论。掉坑一律判 FAIL，只在理由码上区分是大面积偏低还是偶发断流。
    if let Some(dropout) = rx_dropout(&stats.rolling_series, target) {
        let code = if rx_p10 < target {
            // 超过 10% 的窗口在门限以下：不是偶发，是整体抖。
            "RX_UNSTABLE"
        } else {
            "RX_DROPOUT"
        };
        return (
            Verdict::RateFail,
            code.into(),
            format!(
                "网卡 RX 平均 {rx_avg:.3}Mbps、P10 {rx_p10:.3}Mbps 均达标，但{}",
                dropout.describe(target)
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

/// 逐个完整滚动窗口的均值，按时间排列，带窗口结束时刻。
///
/// 保留顺序和时刻是必需的：「掉坑连续掉了多久」只能在有序序列上算，
/// P10 是排序后的分位数，天然丢掉了顺序。
pub(crate) fn rolling_time_window_series(
    samples: &[(u64, u64, f64)],
    range_start_ms: u64,
    window_ms: u64,
) -> Vec<(u64, f64)> {
    if window_ms == 0 {
        return samples
            .iter()
            .map(|(end_ms, _, rate)| (*end_ms, *rate))
            .collect();
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
            rolling.push((*window_end_ms, weighted_sum / covered_ms as f64));
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
    let rolling_series = rolling_time_window_series(
        &rolling_rate_samples,
        window.start_ms,
        ROLLING_RATE_WINDOW_MS,
    );
    rates.sort_by(|a, b| a.total_cmp(b));
    let mut rolling_sorted: Vec<f64> = rolling_series.iter().map(|(_, rate)| *rate).collect();
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
        rolling_series,
        baseline_mbps: baseline,
        stalled_ratio: (longest_zero_delta_run_ms(out, window, rx) as f64 / covered_ms as f64)
            .clamp(0.0, 1.0),
    }
}

/// 判定窗口内计数器**连续零增长**的最长一段时长（毫秒）。
///
/// 看的是 `*_delta_bytes == 0` 这个原始事实，而不是扣完背景之后的速率：
/// 速率为 0 可能只是背景扣除的结果，计数器零增长则是硬事实——这一秒里
/// 这块网卡一个字节都没进/出。
///
/// 取「最长连续一段」而不是零样本总数，是为了区分两种形态：
/// 起流前后各零几秒是正常的（分散的短段），而中途卡死不动是异常的（一整段）。
fn longest_zero_delta_run_ms(out: &MonitorStopOut, window: &EffectiveWindow, rx: bool) -> u64 {
    let mut longest = 0u64;
    let mut current = 0u64;
    for sample in &out.samples {
        if !sample.valid
            || sample.interval_ms == 0
            || sample.elapsed_ms <= window.start_ms
            || sample.elapsed_ms.saturating_sub(sample.interval_ms) >= window.end_ms
        {
            continue;
        }
        let delta = if rx {
            sample.rx_delta_bytes
        } else {
            sample.tx_delta_bytes
        };
        if delta == 0 {
            current = current.saturating_add(sample.interval_ms);
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(elapsed_ms: u64, rx_delta_bytes: u64) -> MonitorSample {
        MonitorSample {
            elapsed_ms,
            interval_ms: 1_000,
            rx_delta_bytes,
            rx_mbps: rx_delta_bytes as f64 * 8.0 / 1_000_000.0,
            valid: true,
            ..Default::default()
        }
    }

    /// 取自 run_20260825_215915_7684 的 unit-114-115：前 6 秒 rx_bytes 正常
    /// 推进（~530Mbps），此后 193 秒纹丝不动，`valid` 全程 true、`error` 全程
    /// 为空。当时报表对这一行写的是「采样覆盖率 100.0%」——覆盖率检查抓不到
    /// 计数器停滞，因为样本确实一条不缺。
    #[test]
    fn a_frozen_counter_is_caught_even_though_sample_coverage_is_perfect() {
        let mut samples: Vec<MonitorSample> =
            (1..=6).map(|i| sample(i * 1_000, 66_000_000)).collect();
        samples.extend((7..=200).map(|i| sample(i * 1_000, 0)));
        let out = MonitorStopOut {
            samples,
            ..Default::default()
        };
        let window = EffectiveWindow {
            start_ms: 0,
            end_ms: 200_000,
            available_secs: 200.0,
            required_secs: 180,
            complete: true,
        };
        let stats = monitor_rate_stats(&out, &window, true, 0);

        assert!(
            stats.coverage > 0.99,
            "样本一条不缺，覆盖率本来就该是满的: {}",
            stats.coverage
        );
        assert!(
            stats.stalled_ratio > 0.95,
            "194/200 秒计数器零增长必须被记下来: {}",
            stats.stalled_ratio
        );

        let (verdict, code, detail) =
            evaluate_nic_rx(RateMode::Observe, None, &stats, &RateStats::default());
        assert_eq!(verdict, Verdict::NotEvaluated);
        assert_eq!(
            code, "COUNTER_STALLED",
            "必须说清是计数器不动，而不是笼统的「没有可用速率」"
        );
        assert!(detail.contains("零增长"), "{detail}");
    }

    /// 起流前后各空几秒是正常的，不能把正常测量误判成停滞——所以取的是
    /// 「最长连续一段」而不是零样本总数。
    #[test]
    fn short_idle_gaps_at_both_ends_are_not_a_stall() {
        let mut samples = vec![sample(1_000, 0), sample(2_000, 0)];
        samples.extend((3..=198).map(|i| sample(i * 1_000, 120_000_000)));
        samples.push(sample(199_000, 0));
        samples.push(sample(200_000, 0));
        let out = MonitorStopOut {
            samples,
            ..Default::default()
        };
        let window = EffectiveWindow {
            start_ms: 0,
            end_ms: 200_000,
            available_secs: 200.0,
            required_secs: 180,
            complete: true,
        };
        let stats = monitor_rate_stats(&out, &window, true, 0);
        assert!(stats.stalled_ratio < 0.05, "{}", stats.stalled_ratio);
        let (verdict, _, _) =
            evaluate_nic_rx(RateMode::Observe, None, &stats, &RateStats::default());
        assert_eq!(verdict, Verdict::Measured);
    }

    /// 窗口没攒够要求时长时，判定确实不该给结论，但**速率必须照常算出来**。
    ///
    /// 「这一行不作数」和「这一行什么都没测到」是两回事：前者是判定的克制，
    /// 后者是数据的缺失。把两者混成一个「未采集」，读报告的人就没法判断
    /// 到底该重跑还是该查链路。
    #[test]
    fn a_short_window_still_produces_a_receive_rate() {
        let out = MonitorStopOut {
            samples: (1..=169).map(|i| sample(i * 1_000, 118_750_000)).collect(),
            ..Default::default()
        };
        let short = EffectiveWindow {
            start_ms: 0,
            end_ms: 169_000,
            available_secs: 169.0,
            required_secs: 180,
            complete: false,
        };
        let stats = monitor_rate_stats(&out, &short, true, 0);
        let avg = stats.avg_mbps.expect("窗口短不等于没速率");
        assert!((avg - 950.0).abs() < 1.0, "实际 {avg}");
        assert!(stats.coverage > 0.99);
        assert!(stats.p10_mbps.is_some(), "P10 同样要算");
    }

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
        // 全程稳定在 850：35 个 5 秒窗口一个都不掉到 800 以下。
        let complete = RateStats {
            avg_mbps: Some(850.0),
            p10_mbps: Some(820.0),
            coverage: 1.0,
            rolling_coverage: 1.0,
            rolling_series: (1..=35).map(|i| (i * 5_000, 850.0)).collect(),
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

        // 超过 10% 的窗口在门限以下：整体抖，不是偶发。
        let unstable = RateStats {
            p10_mbps: Some(799.0),
            rolling_series: (1..=35)
                .map(|i| (i * 5_000, if i <= 6 { 700.0 } else { 850.0 }))
                .collect(),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &unstable),
            (Verdict::RateFail, "RX_UNSTABLE".into())
        );

        // 平均和 P10 都达标，只有一个 5 秒窗口掉下去：仍然是 FAIL。
        // 一次 5 秒断流在 175 秒里只占 3%，P10 根本看不见它，但那一秒钟
        // 用户的视频就是卡住了。
        let dropout = RateStats {
            rolling_series: (1..=35)
                .map(|i| (i * 5_000, if i == 20 { 120.0 } else { 850.0 }))
                .collect(),
            ..complete.clone()
        };
        let (verdict, code, detail) =
            evaluate_nic_rx(RateMode::Verify, Some(800.0), &dropout, &healthy_tx);
        assert_eq!((verdict, code.as_str()), (Verdict::RateFail, "RX_DROPOUT"));
        assert!(detail.contains("掉坑"), "{detail}");
        assert!(detail.contains("120.000"), "要说出最低掉到多少: {detail}");

        // 掉到接近 0 要说「断流」，不能和轻微掉坑用同一个词。
        let stalled = RateStats {
            rolling_series: (1..=35)
                .map(|i| (i * 5_000, if (18..=20).contains(&i) { 0.0 } else { 850.0 }))
                .collect(),
            ..complete.clone()
        };
        let (verdict, code, detail) =
            evaluate_nic_rx(RateMode::Verify, Some(800.0), &stalled, &healthy_tx);
        assert_eq!((verdict, code.as_str()), (Verdict::RateFail, "RX_DROPOUT"));
        assert!(detail.contains("断流"), "{detail}");

        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &complete),
            (Verdict::Pass, String::new())
        );
    }

    /// 掉坑时长按「首个掉坑窗口的起点 -> 末个掉坑窗口的终点」算。
    ///
    /// 相邻 5 秒窗口高度重叠，按窗口个数乘 5 秒会把一次 5 秒的坑说成半分钟。
    #[test]
    fn a_dropout_reports_how_long_it_actually_lasted() {
        // 1 秒一个窗口；第 20~22 号窗口掉下去 = 覆盖 15..22 秒，共 7 秒。
        let series: Vec<(u64, f64)> = (1..=35)
            .map(|i| {
                (
                    i * 1_000,
                    if (20..=22).contains(&i) { 100.0 } else { 900.0 },
                )
            })
            .collect();
        let dropout = rx_dropout(&series, 800.0).expect("应检出掉坑");
        assert_eq!(dropout.windows, 3);
        assert_eq!(dropout.lowest_mbps, 100.0);
        assert_eq!(dropout.longest_ms, 7_000, "15s 起到 22s 止");
        assert_eq!(dropout.started_at_ms, 15_000);
        assert!(!dropout.stalled, "100Mbps 是掉坑不是断流");

        // 两段坑分开时取最长的那一段，不是加起来。
        let split: Vec<(u64, f64)> = (1..=35)
            .map(|i| {
                let low = i == 10 || (20..=24).contains(&i);
                (i * 1_000, if low { 100.0 } else { 900.0 })
            })
            .collect();
        let dropout = rx_dropout(&split, 800.0).expect("应检出掉坑");
        assert_eq!(dropout.windows, 6);
        assert_eq!(dropout.longest_ms, 9_000, "15s 起到 24s 止的那一段");

        assert!(rx_dropout(&series, 0.0).is_none(), "没有门限就没有掉坑一说");
        let steady: Vec<(u64, f64)> = (1..=35).map(|i| (i * 1_000, 900.0)).collect();
        assert!(rx_dropout(&steady, 800.0).is_none());
    }
}
