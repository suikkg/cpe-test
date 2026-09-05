//! 网卡采样统计与速率判定层。
//!
//! 从 `master::executor` 抽出的**纯函数层**：只依赖 OS 网卡计数器样本、有效
//! 时间窗口和目标速率，不接触任何执行状态（进程、端口、HTTP、线程）。
//!
//! 单独成模块的理由不是"文件太大"，而是这里是**正式判定口径的所在地**——
//! 验收文档反复强调的两条铁律都在这一层实现：
//!
//! 1. 正式口径永远是接收端 OS 网卡计数器，工具自报速率只作诊断；
//! 2. 没有可用 RX 平均值时才产出 `NOT_EVALUATED`；一旦 RX 平均低于目标，
//!    TX/RX 侧问题按子网性能失败处理。
//!
//! 这一层仍先排除完全没有可用 RX 平均值、RX 基础采样覆盖不足等无法形成结论的
//! 情况；一旦 RX 平均值有效且低于目标，TX/RX 的速率问题统一作为子网失败。
//! 把这一层单独隔出来，是为了让这条判定顺序能被单独审阅和测试。

use crate::reason::ReasonCode;
use std::collections::HashSet;

use crate::config::RateMode;
// 「有效流量」下限与采样层共用同一个常量，避免两处阈值漂移。
pub use crate::nic::monitor::MIN_VALID_RX_MBPS;
use crate::protocol::{MonitorSample, MonitorStopOut};
use crate::verdict::VerdictResult;

/// 接收端网卡 RX 采样覆盖率下限；低于它不允许做正式性能判定。
pub const MIN_RATE_SAMPLE_COVERAGE: f64 = 0.95;
/// 稳定性判定使用的滚动窗口长度。
pub const ROLLING_RATE_WINDOW_MS: u64 = 5_000;
/// 毫秒取整造成的窗口拼接误差容忍；只容忍舍入，不容忍真正的漏采周期。
pub const ROLLING_COVERAGE_TOLERANCE_MS: u64 = 50;

/// 断流 / 掉坑要连续多久才算数。
///
/// 判据落在**原始逐样本序列**上，所以这就是字面意义的「连续 5 秒」，
/// 不再是「某个 5 秒滑动平均越界」。一个采样周期的抖动一律不算：它和
/// Wi-Fi 发 probe、信道扫描造成的掉一拍在网卡计数器上不可区分。
pub const MIN_RATE_EXCURSION_MS: u64 = 5_000;
/// 掉坑门限相对目标的容差：低于 `target * (1 - 它)` 才算掉坑。
///
/// 门限贴着目标比（老口径的 `rate < target`）会把噪声判成故障：
/// run_20260828_162822_17788 的 unit-109-110 就是被 1973.171 / 2000 这
/// 1.35% 的差判掉的。
pub const RATE_DROPOUT_TOLERANCE: f64 = 0.20;
/// 断流判据相对目标的比例：「灌包速率基本为 0」取目标的 1%。
///
/// 不取 `target * 0.1`——那个量级还有十分之一的目标速率在跑，属于掉坑
/// 而不是断流，两者的排查方向完全不同。
pub const RATE_OUTAGE_RATIO: f64 = 0.01;

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
    /// 判定窗口内**原始**逐样本序列 `(样本结束时刻ms, 独占时长ms, 速率Mbps)`。
    ///
    /// 断流/掉坑判在它上面，不判在 5 秒滑动平均上：滑动平均会把一个
    /// 采样周期的抖动摊成 5 个窗口，既制造误判也把时长报错（详见
    /// [`RateExcursion`]）。时长已去过重叠，累加即真实覆盖时长。
    pub series: Vec<(u64, u64, f64)>,
    /// 产出 `series` 的那个判定窗口的起点（监控时间轴毫秒）。
    ///
    /// `series` 存的是绝对时刻，报告里要印的却是「窗口内第几秒」。把窗口起点
    /// 跟着数据一起带走，折算才有唯一的依据；否则每个使用点都得自己去找窗口，
    /// 找漏一个就印出越界的秒数。
    pub window_start_ms: u64,
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

/// 越界的形态：断流 / 掉坑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExcursionKind {
    /// 灌包速率基本为 0——链路这几秒是真的断的。
    Outage,
    /// 没断，但持续掉到门限以下。
    Dropout,
    // 「持续高于门限」曾经也在这里占一档（RX_SPIKE，target*1.2 连续 5 秒）。
    // 拿 run_20260828_162822_17788 回放，它打中 24 行，没有一行是毛刺——
    // 全是「链路本来就比目标快」：2.5G 口配 2000 目标、稳定跑 2450，就会
    // 连续 200 秒「高于 target*1.2」。低于目标是缺陷，高于目标不是；而且
    // 毛刺按定义是短时突起，「连续 ≥5 秒」恰好把真毛刺排除、把稳态高速率
    // 全收进来。真要查异常抬升，判据得相对**链路自身的中位数**而不是目标。
}

impl ExcursionKind {
    /// 报告里的原因码。两种形态分开发码，读报告的人一眼就知道该查什么。
    pub fn reason_code(self) -> ReasonCode {
        match self {
            ExcursionKind::Outage => ReasonCode::RxOutage,
            ExcursionKind::Dropout => ReasonCode::RxDropout,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ExcursionKind::Outage => "断流",
            ExcursionKind::Dropout => "掉坑",
        }
    }
}

/// 判定窗口内一段**连续偏离门限**的实测区间。
///
/// v4.5.0 之前这里判的是「某个 5 秒滑动平均越过门限」，那和它自己注释宣称的
/// 「有完整 5 秒掉到门限以下」不是一回事。滑动平均会把一次 1 秒的掉速摊进
/// 5 个相邻窗口，于是：
///
/// 1. 一个采样周期的抖动就足以让整行 RATE_FAIL——Wi-Fi 发 probe 掉一拍就中招；
/// 2. 报出来的「最长连续」= 真实秒数 + 窗口长度 − 1，系统性虚报 4 秒；
/// 3. 灵敏度取决于**链路余量**而不是**掉速时长**：一次 1 秒断流在余量
///    25% 以内的链路上必挂，而余量 50% 的链路真断 1.7 秒却查不出来。
///
/// run_20260828_162822_17788 的 unit-109-110 是最干净的反例：整段 180 秒里
/// 只有 111.00-112.01 这一秒从 2270 掉到 1499Mbps，其余全程 2000 以上；
/// 5 秒窗口均值 1973.171 比 2000 门限低 1.35%，报告却写成「最长连续 5.0 秒」
/// 并建议「业务上那几秒就是断的」。对着 iperf 截图根本找不到这 5 秒。
///
/// 所以判据回到**原始逐样本序列**：越界必须自己连续够
/// [`MIN_RATE_EXCURSION_MS`] 才算数，报出来的秒数就是真秒数，能直接和截图
/// 对上；单个采样周期的抖动一律忽略——它和 probe / 信道扫描不可区分。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RateExcursion {
    pub kind: ExcursionKind,
    /// 判据门限，已按目标和容差折算。
    pub threshold_mbps: f64,
    /// 折算出这条门限的目标。
    pub target_mbps: f64,
    /// 最长一段连续越界的时长。判定看的就是它。
    pub longest_ms: u64,
    /// 最长那一段的起始时刻，**相对判定窗口起点**的毫秒。
    ///
    /// 必须是窗口相对量：`describe()` 印的就是「自判定窗口第 X 秒起」，而
    /// `RateStats::series` 里存的是监控起点的绝对时刻。两者差一个
    /// `window.start_ms`，直接拿绝对值去印，报告里就会出现
    /// 「60 秒窗口的第 69.6 秒」这种不可能的数——run_20260905_125327_5940
    /// 的 unit-112 就是这么印出来的。折算在 [`rate_excursion`] 里做一次。
    pub started_at_ms: u64,
    /// 最长那一段里的极值：该段最低掉到多少。
    pub extreme_mbps: f64,
    /// 全窗口所有越界段的合计时长，含没够判定时长的短段。
    pub total_ms: u64,
    /// 全窗口越界段的段数，含没够判定时长的短段。
    pub runs: usize,
}

impl RateExcursion {
    /// 报告和日志里那句人话。
    pub fn describe(&self) -> String {
        let ratio = if self.target_mbps > 0.0 {
            self.threshold_mbps / self.target_mbps * 100.0
        } else {
            0.0
        };
        format!(
            "{}：连续 {:.1} 秒低于门限 {:.3}Mbps（目标 {:.3}Mbps 的 {:.0}%），\
             自判定窗口第 {:.1} 秒起，该段最低掉到 {:.3}Mbps；\
             全窗口共 {} 段越界、合计 {:.1} 秒（含未够 {:.0} 秒的短段）",
            self.kind.label(),
            self.longest_ms as f64 / 1000.0,
            self.threshold_mbps,
            self.target_mbps,
            ratio,
            self.started_at_ms as f64 / 1000.0,
            self.extreme_mbps,
            self.runs,
            self.total_ms as f64 / 1000.0,
            MIN_RATE_EXCURSION_MS as f64 / 1000.0,
        )
    }

    pub fn reason_code(&self) -> ReasonCode {
        self.kind.reason_code()
    }
}

/// 在原始逐样本序列上扫一种越界形态。
///
/// `series` 的元素是 `(样本结束时刻ms, 该样本独占的时长ms, 速率Mbps)`——
/// 时长已经去过重叠，所以直接累加就是真实覆盖时长，不需要假设采样周期。
fn scan_excursion(
    series: &[(u64, u64, f64)],
    kind: ExcursionKind,
    threshold: f64,
    target: f64,
) -> Option<RateExcursion> {
    if !threshold.is_finite() {
        return None;
    }
    struct Run {
        start_ms: u64,
        span_ms: u64,
        extreme: f64,
    }
    let mut runs: Vec<Run> = Vec::new();
    let mut current: Option<Run> = None;
    for (end_ms, interval_ms, rate) in series {
        if *interval_ms == 0 || !rate.is_finite() {
            continue;
        }
        if *rate < threshold {
            match current.as_mut() {
                Some(run) => {
                    run.span_ms = run.span_ms.saturating_add(*interval_ms);
                    run.extreme = run.extreme.min(*rate);
                }
                None => {
                    current = Some(Run {
                        start_ms: end_ms.saturating_sub(*interval_ms),
                        span_ms: *interval_ms,
                        extreme: *rate,
                    })
                }
            }
        } else if let Some(run) = current.take() {
            runs.push(run);
        }
    }
    if let Some(run) = current.take() {
        runs.push(run);
    }

    let longest = runs.iter().max_by_key(|run| run.span_ms)?;
    // 采样周期是 ~1005ms 而不是整 1000ms，5 个连续样本会累到 5030ms；反过来
    // 偶尔也会差几毫秒。判定时长的边界不该由毫秒级抖动决定，容忍量与滚动
    // 窗口拼接用的是同一个常量。
    if longest
        .span_ms
        .saturating_add(ROLLING_COVERAGE_TOLERANCE_MS)
        < MIN_RATE_EXCURSION_MS
    {
        return None;
    }
    Some(RateExcursion {
        kind,
        threshold_mbps: threshold,
        target_mbps: target,
        longest_ms: longest.span_ms,
        started_at_ms: longest.start_ms,
        extreme_mbps: longest.extreme,
        total_ms: runs.iter().map(|run| run.span_ms).sum(),
        runs: runs.len(),
    })
}

/// 找出判定窗口里够时长的越界段；两种形态都没有就返回 `None`。
///
/// 顺序是**由重到轻**：断流的样本必然也满足掉坑判据，先报断流才说得清
/// 「这几秒是真断了」还是「只是掉下去了」。
pub(crate) fn rate_excursion(
    series: &[(u64, u64, f64)],
    target: f64,
    // 判定窗口起点（监控时间轴的毫秒）。`series` 用的是同一条绝对时间轴，
    // 减掉它才是 `describe()` 承诺的「自判定窗口第 X 秒起」。
    window_start_ms: u64,
) -> Option<RateExcursion> {
    if !target.is_finite() || target <= 0.0 {
        return None;
    }
    // 「速率基本为 0」不能直接拿 0 比：背景扣除、ARP/重传这类零星帧都会留下
    // 零点几到几十 Mbps 的残值。取目标的 1%，并以有效流量下限兜底。
    let outage_floor = (target * RATE_OUTAGE_RATIO).max(MIN_VALID_RX_MBPS);
    let mut excursion = scan_excursion(series, ExcursionKind::Outage, outage_floor, target)
        .or_else(|| {
            scan_excursion(
                series,
                ExcursionKind::Dropout,
                target * (1.0 - RATE_DROPOUT_TOLERANCE),
                target,
            )
        })?;
    excursion.started_at_ms = excursion.started_at_ms.saturating_sub(window_start_ms);
    Some(excursion)
}

/// 验证目标所需的最低发送负载（目标 + 余量）。
///
/// 三条链共用同一个算法，免得「余量」在不同后端上是不同的数。
pub(crate) fn offered_floor_mbps(target_mbps: Option<f64>, headroom_pct: f64) -> Option<f64> {
    target_mbps.map(|target| target * (1.0 + headroom_pct.max(0.0) / 100.0))
}

/// **吞吐验收的唯一入口**（ADR-17）。
///
/// 它只回答一个问题，并且只允许四种答案：
///
/// ```text
/// 形不成有效 RX 平均值 → NOT_EVALUATED
/// 没有可信门限         → MEASURED
/// RX 平均 >= 门限      → PASS
/// RX 平均 <  门限      → RATE_FAIL
/// ```
///
/// 「有效 RX 平均值」只有三条门槛，全部落在**接收端**：计数器不能整窗停滞、
/// 平均值要存在且高于有效流量下限、RX 采样覆盖率要够。三条都过了就必须给出
/// PASS/FAIL——这是用户确认过的验收规则：**RX 平均达到门限就是 PASS，不看其他
/// 指标**。
///
/// 因此这里**看不到**发送端：TX 覆盖率、TX-P10、offered 负载、滚动窗口覆盖、
/// UDP 丢包、工具退出状态一律不进这个函数，它们由
/// [`rx_acceptance_diagnostics`] 收集成诊断行挂在结论旁边。历史上正是这些
/// 指标在 RX 已经达标之后把结论改写成 RATE_FAIL / NOT_EVALUATED，让同一条
/// 链路在 TCP 和 UDP 两条路径上得到相反的结论。
pub(crate) fn evaluate_rx_acceptance(
    mode: RateMode,
    target_mbps: Option<f64>,
    stats: &RateStats,
) -> VerdictResult {
    // 没有可用平均值时不能形成速率结论；计数器停滞仍优先报告具体原因。
    // 它命中的场景里 avg 通常也是 0，会被
    // NIC_RATE_MISSING 抢先吃掉，而「采到样本但计数器不动」比「没有可用速率」
    // 具体得多——前者直接指向链路或网卡侧，后者只说明这一行没结论。
    //
    // 门槛与采样覆盖率共用同一个常量：窗口里至少 95% 的时间要有真实推进的
    // 计数，剩下 5% 留给起流/收尾的空档。
    if stats.stalled_ratio > 1.0 - MIN_RATE_SAMPLE_COVERAGE {
        return VerdictResult::not_evaluated(
            ReasonCode::CounterStalled,
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
        return VerdictResult::not_evaluated(
            ReasonCode::NicRateMissing,
            "有效流量窗口内没有可用的接收端 OS 网卡 RX 速率",
        );
    };
    if !stats.coverage.is_finite() || stats.coverage < MIN_RATE_SAMPLE_COVERAGE {
        return VerdictResult::not_evaluated(
            ReasonCode::SampleCoverageLow,
            format!(
                "接收端网卡 RX 采样覆盖率 {:.1}%，低于 {:.1}%",
                stats.coverage * 100.0,
                MIN_RATE_SAMPLE_COVERAGE * 100.0
            ),
        );
    }
    let target_mbps = crate::rate::effective_rate_target(mode, target_mbps);
    let Some(target) = target_mbps else {
        return if mode == RateMode::Verify {
            VerdictResult::not_evaluated(
                ReasonCode::TargetMissing,
                "verify 模式必须配置可信的接收端网卡 RX 目标",
            )
        } else {
            VerdictResult::measured(
                ReasonCode::TargetUnknown,
                format!("接收端网卡 RX 已测得 {rx_avg:.3}Mbps；未配置可信目标，因此不标记 PASS"),
            )
        };
    };
    if rx_avg >= target {
        return VerdictResult::pass();
    }
    VerdictResult::rate_fail(
        ReasonCode::RxBelowTarget,
        format!("网卡 RX 平均 {rx_avg:.3}Mbps 低于目标 {target:.3}Mbps"),
    )
}

/// 收集**不参与判定**的排障线索。
///
/// 这里的每一条都曾经是判定分支。它们全部保留下来是因为排查时确实要看，
/// 但它们回答的是「为什么会这样」，而不是「这条链路合格没有」——
/// 后者只有 [`evaluate_rx_acceptance`] 一个答案来源。
pub(crate) fn rx_acceptance_diagnostics(
    stats: &RateStats,
    tx_stats: &RateStats,
    // **已经过 `effective_rate_target` 折算**的判定目标；`None` = 这一腿没有门限。
    // 收目标本身而不是 `bool`：中途掉坑/断流的判据要拿它当参照。
    target: Option<f64>,
    // 验证目标所需的最低发送负载（目标 + 余量）；`None` = 没有 offered 参照。
    offered_floor: Option<f64>,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(target) = target else {
        return out;
    };
    let fmt = |value: Option<f64>| {
        value
            .map(|v| format!("{v:.3}Mbps"))
            .unwrap_or_else(|| "缺失".into())
    };
    if !tx_stats.coverage.is_finite() || tx_stats.coverage < MIN_RATE_SAMPLE_COVERAGE {
        out.push(format!(
            "发送端网卡 TX 采样覆盖率 {:.1}%，低于 {:.1}%；只影响这行诊断的可信度，不改写判定",
            tx_stats.coverage * 100.0,
            MIN_RATE_SAMPLE_COVERAGE * 100.0
        ));
    }
    if !rate_window_coverage_sufficient(stats, tx_stats, true) {
        out.push(format!(
            "完整 5 秒滚动窗口覆盖率 RX {:.1}%（P10 {}）/ TX {:.1}%（P10 {}），低于 {:.1}%；\
             稳定性无法核对，不改写判定",
            stats.rolling_coverage * 100.0,
            fmt(stats.p10_mbps),
            tx_stats.rolling_coverage * 100.0,
            fmt(tx_stats.p10_mbps),
            MIN_RATE_SAMPLE_COVERAGE * 100.0
        ));
    }
    if let Some(floor) = offered_floor {
        let tx_sufficient = tx_stats.p10_mbps.map(|v| v >= floor).unwrap_or(false);
        if !tx_sufficient {
            out.push(format!(
                "发送端 TX-P10 {} 未达到验证目标所需负载 {}；灌包强度不足只作诊断，\
                 达标与否仍只看接收端 RX 平均",
                fmt(tx_stats.p10_mbps),
                fmt(Some(floor))
            ));
        }
    }
    // 中途断流 / 掉坑。
    //
    // 平均值答不出「中间断没断过」：全程平均 2200、中间断 6 秒，和全程稳定
    // 2200，对使用者不是同一个结论。它**不再改写判定**（RX 平均达标就是
    // PASS），但必须留在报告里——否则一条中途断了一分钟的链路会报出一个
    // 干干净净的 PASS，报告上一个字都看不到。
    if let Some(excursion) = rate_excursion(&stats.series, target, stats.window_start_ms) {
        // 尾巴那句话必须跟着**这一行自己**的平均值走。
        //
        // 它以前是写死的「平均值已达标，故不改写判定」，而这个函数在 RX 平均
        // 不达标时同样会跑：run_20260905_125327_5940 里 116 条掉坑诊断有 110
        // 条挂在 RATE_FAIL 行上，和同一行的「网卡 RX 平均 948.658Mbps 低于目标
        // 1800.000Mbps」正面打架。判定没错，但读报告的人会先怀疑数据。
        let verdict_note = if stats
            .avg_mbps
            .is_some_and(|avg| avg.is_finite() && avg >= target)
        {
            "平均值已达标，故不改写判定，但这段时间的业务是真的受影响了"
        } else {
            "本行平均值本就不达标，掉坑只是补充说明它掉在哪几秒"
        };
        // 带上原因码：它是报告里通用的词汇，读的人能直接和 `report/reason.rs`
        // 的说明对上，也能在一堆诊断行里检索出「断流」和「掉坑」两类。
        out.push(format!(
            "{}: {}；{verdict_note}",
            excursion.reason_code(),
            excursion.describe()
        ));
    }
    out
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
    // 「周期偏长」和「跨过一次漏采」是两件事，过去被同一个上限一起挡掉。
    //
    // 采样线程被 OS 抢占是常态，不是数据缺失：run_20260828_162822_17788 的
    // unit-257-258 里 154 个样本有 11 个周期落在 1660~1993ms（标称 1059ms），
    // 每一条的计数器 delta 都是完整的。老上限 nominal*1.5+50=1638ms 把它们
    // 全踢出滚动序列，每踢一个废掉约 5 个窗口，覆盖率被压到 63.6%，整行
    // 误判成 RATE_WINDOW_COVERAGE_LOW，还建议去查「是不是重启/切换过网卡」。
    //
    // 真正不能信的是**跨过一个无效样本**的恢复样本：那一段里有多久是零、
    // 多久是满速无从得知，拿它拼出来的 5 秒窗口证明不了任何事。它有独立
    // 且确切的特征——前一条样本 `valid` 为假——不必靠周期长度去猜。
    let mut follows_gap: HashSet<u64> = HashSet::new();
    let mut previous_broken = false;
    for sample in &out.samples {
        if previous_broken {
            follows_gap.insert(sample.elapsed_ms);
        }
        previous_broken = !sample.valid || sample.interval_ms == 0;
    }
    // 周期上限只留着兜住「线程失调度几十秒后补一条」这类没有无效样本作伴的
    // 情形；日常抖动（实测最高 1.88 倍标称）要放过去。
    let max_rolling_sample_ms = nominal_interval_ms.map(|nominal| {
        nominal
            .saturating_mul(2)
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
                    .is_some_and(|max_interval| sample.interval_ms <= max_interval)
                    && !follows_gap.contains(&sample.elapsed_ms),
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
        series: rate_samples,
        window_start_ms: window.start_ms,
        p10_mbps: percentile(&rolling_sorted, 0.10),
        median_mbps: percentile(&rates, 0.50),
        p95_mbps: percentile(&rates, 0.95),
        min_mbps: Some(min),
        max_mbps: Some(max),
        coverage: (covered_ms as f64 / window_ms as f64).min(1.0),
        rolling_coverage,
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
    use crate::verdict::Verdict;

    /// 测试里仍按老三元组读结论，省得每条断言都改成字段访问。
    fn nic_rx(
        mode: RateMode,
        target_mbps: Option<f64>,
        stats: &RateStats,
    ) -> (Verdict, ReasonCode, String) {
        let result = evaluate_rx_acceptance(mode, target_mbps, stats);
        (result.verdict, result.code, result.detail)
    }

    /// **中途断流/掉坑必须出现在报告里，哪怕平均值达标。**
    ///
    /// 这条守的是一个真实的回归：把「掉坑」从判定分支降级为诊断时，它被
    /// `#[allow(dead_code)]` 直接消音了——整套越界判据（`rate_excursion`、
    /// `ExcursionKind`、`RateStats::series`）在生产代码里一个调用点都没有，
    /// 只剩测试在读。结果是一条全程平均 2200Mbps、**中间整整断了一分钟**的
    /// 链路报出一个干干净净的 PASS，报告上一个字都看不到。
    ///
    /// 降级的意思是「不改写判定」，不是「不再告诉任何人」。
    #[test]
    fn a_mid_run_outage_still_reaches_the_report_even_though_the_average_passed() {
        let target = 2_000.0;
        // 前 60 秒正常、中间断 20 秒、后 100 秒正常：平均仍在门限之上。
        let series: Vec<(u64, u64, f64)> = (1..=180)
            .map(|i| {
                let rate = if (61..=80).contains(&i) { 0.0 } else { 2_600.0 };
                (i * 1_000, 1_000, rate)
            })
            .collect();
        let avg = series.iter().map(|(_, _, r)| r).sum::<f64>() / series.len() as f64;
        assert!(avg > target, "这条用例的前提就是平均值达标：{avg}");

        let rx = RateStats {
            avg_mbps: Some(avg),
            p10_mbps: Some(0.0),
            coverage: 1.0,
            rolling_coverage: 1.0,
            series: series.clone(),
            ..Default::default()
        };
        let tx = RateStats {
            avg_mbps: Some(2_600.0),
            p10_mbps: Some(2_600.0),
            coverage: 1.0,
            rolling_coverage: 1.0,
            ..Default::default()
        };

        // 判定不变：RX 平均达标就是 PASS。
        let (verdict, _, _) = nic_rx(RateMode::Verify, Some(target), &rx);
        assert_eq!(verdict, Verdict::Pass, "平均值达标仍然必须 PASS");

        // 但断流这件事必须留下痕迹。
        let notes = rx_acceptance_diagnostics(&rx, &tx, Some(target), None);
        assert!(
            notes.iter().any(|line| line.contains("秒")),
            "中途断流要出现在诊断里，否则报告上看不出这一分钟：{notes:?}"
        );

        // 全程平稳的同一条链路不该凭空多出这条诊断。
        let steady_series: Vec<(u64, u64, f64)> =
            (1..=180).map(|i| (i * 1_000, 1_000, 2_600.0)).collect();
        let steady = RateStats {
            avg_mbps: Some(2_600.0),
            p10_mbps: Some(2_600.0),
            coverage: 1.0,
            rolling_coverage: 1.0,
            series: steady_series,
            ..Default::default()
        };
        assert!(
            rx_acceptance_diagnostics(&steady, &tx, Some(target), None).is_empty(),
            "平稳链路不该有诊断"
        );
    }

    /// 发送端没灌够、RX 平均也没达到目标时，仍然直接判子网失败；
    /// 「没灌够」只出现在**诊断**里（ADR-17）。
    #[test]
    fn an_underfilled_sender_is_a_subnet_failure_and_only_a_diagnostic() {
        let series: Vec<(u64, u64, f64)> = (1..=180).map(|i| (i * 1_000, 1_000, 500.0)).collect();
        let stats = |avg: f64| RateStats {
            avg_mbps: Some(avg),
            p10_mbps: Some(avg),
            coverage: 1.0,
            rolling_coverage: 1.0,
            series: series.iter().map(|(t, d, _)| (*t, *d, avg)).collect(),
            ..Default::default()
        };
        // 发送端只灌了 500，目标 1000、门槛 1050：接收端收到 495，
        // 基本等于发出的量；按当前验收口径，RX 没达到目标就判子网失败。
        let rx = stats(495.0);
        let tx = stats(500.0);

        let (verdict, code, detail) = nic_rx(RateMode::Verify, Some(1_000.0), &rx);
        assert_eq!(
            verdict,
            Verdict::RateFail,
            "RX 低于目标时，即使发送端也没灌满仍应判 FAIL，实得 {verdict:?}/{code}"
        );
        assert_eq!(code, ReasonCode::RxBelowTarget);
        assert!(
            detail.contains("495.000") && detail.contains("1000.000"),
            "判定明细只讲 RX 平均与门限: {detail}"
        );
        // 发送端的背景交给诊断，不再混进判定明细。
        let notes = rx_acceptance_diagnostics(&rx, &tx, Some(1_000.0), Some(1_050.0));
        assert!(
            notes.iter().any(|line| line.contains("TX-P10")),
            "没灌够要留在诊断里: {notes:?}"
        );

        // 反面：发送端灌足了、接收端仍然差得远，这才是真的 CPE 不达标。
        let rx_low = stats(400.0);
        let (verdict, code, _) = nic_rx(RateMode::Verify, Some(1_000.0), &rx_low);
        assert_eq!(verdict, Verdict::RateFail, "灌足了还不达标必须判 FAIL");
        assert_eq!(code, ReasonCode::RxBelowTarget);
        // 灌足了就不该有 offered 诊断。
        assert!(
            !rx_acceptance_diagnostics(&rx_low, &stats(1_100.0), Some(1_000.0), Some(1_050.0))
                .iter()
                .any(|line| line.contains("TX-P10"))
        );
    }

    /// **RX 已经达标的腿，永远不许被 offered 闸降级。**
    ///
    /// offered 闸的全部理由是「解释缺口」。R6 最初把它架在 `rx_avg < target`
    /// **外面**，于是它在没有缺口时也开火：TCP 不限速，链路上限贴着目标时
    /// TX-P10 落在「目标 ~ 目标+余量」之间是常态，一条 RX 达标的腿就被判成
    /// NOT_EVALUATED / OFFERED_LOAD_LOW。
    ///
    /// 下面这组数就是 `evaluate_nic_rx` 自己注释里引用的那次 run：主控 WLAN
    /// 全场上限 2102、目标 2000、余量 5%（floor 2100）。R6 拆掉了 P10 在这个
    /// 场景上造成的误判，又在同一个场景上装回了一条新的——所以这条测试用
    /// 同一组数守着，防止再装回去。
    #[test]
    fn a_leg_that_met_its_target_is_never_downgraded_by_the_offered_gate() {
        let stats = |avg: f64| RateStats {
            avg_mbps: Some(avg),
            p10_mbps: Some(avg),
            coverage: 1.0,
            rolling_coverage: 1.0,
            series: (1..=180).map(|i| (i * 1_000, 1_000, avg)).collect(),
            ..Default::default()
        };
        // TX-P10 2005 < floor 2100（没灌到目标+余量），但 RX 平均 2014 ≥ 目标 2000。
        let (verdict, code, detail) = nic_rx(RateMode::Verify, Some(2_000.0), &stats(2_014.0));
        assert_eq!(
            verdict,
            Verdict::Pass,
            "RX 达标就没有缺口要解释，offered 闸不该开火，实得 {verdict:?}/{code}：{detail}"
        );
        assert_ne!(code, ReasonCode::OfferedLoadLow);

        // 边界：RX 正好等于目标也算达标（合格线是 `rx_avg < target` 才失败）。
        let (verdict, _, _) = nic_rx(RateMode::Verify, Some(2_000.0), &stats(2_000.0));
        assert_eq!(verdict, Verdict::Pass, "RX 正好等于目标仍是达标");
    }

    /// 结构断言：装配层的三条契约各只能有一处定义（ADR-12）。
    ///
    /// 铁律 2 说「速率判定口径只有一份实现 = `master::rate_window`」，字面上一直
    /// 成立——但 `rate_window` **之上**还有一层「腿级装配」：把窗口、覆盖率、
    /// 目标、offered 负载、丢包组合成一条腿的结论。那一层曾经有三份实现
    /// （`udp_leg_verdict` / `iperf_flow_verdict` / CTS 的内联 if-else），
    /// 并且已经对同一事实给出过不同结论：
    ///
    /// - Observe/Discover 下 UDP 链拿目标判 FAIL，TCP/CTS 链不会；
    /// - 「灌够了没有」的防误判口径**只有 UDP 链有**。
    ///
    /// 这与历史上两次静默错判同型：语义重复靠普通测试发现不了，两份实现各自
    /// 都能通过自己的用例。所以照 `verdict_priority_has_exactly_one_definition_in_the_tree`
    /// 的样子，在源码层面把门关上。
    #[test]
    fn the_leg_assembly_contracts_have_exactly_one_definition_in_the_tree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                // 定义就在本文件里。四个标记分居两处：`effective_rate_target`
                // 在 `rate.rs`（连同它清目标的那条 `Observe | Discover` 匹配），
                // offered 那两条在本文件。这两个文件是**唯一**允许出现它们的
                // 地方——扫描按文件名跳过，粒度到文件为止，不再细分哪个标记
                // 属于哪个文件：真正要挡的是**第三处**实现，而不是这两处互相
                // 串门。
                if matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("rate_window.rs") | Some("rate.rs")
                ) {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read source");
                // 只看生产代码：测试可以自由构造这些场景。
                let production = text
                    .split_once("#[cfg(test)]")
                    .map(|(head, _)| head)
                    .unwrap_or(&text);
                for (marker, why) in [
                    (
                        "RateMode::Observe | RateMode::Discover",
                        "「Observe/Discover 不比目标」只能定义在 rate::effective_rate_target",
                    ),
                    (
                        "fn evaluate_rx_acceptance",
                        "吞吐验收（RX 平均 vs 门限）只能定义在 rate_window",
                    ),
                    (
                        "fn rx_acceptance_diagnostics",
                        "「哪些事实只作诊断」的清单只能定义在 rate_window",
                    ),
                    (
                        "fn effective_rate_target",
                        "生效目标的推导只能定义在 rate（rate_window 调它，不自带一份）",
                    ),
                ] {
                    if production.contains(marker) {
                        offenders.push(format!("{}: {marker} —— {why}", path.display()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "腿级判定装配层出现了第二份实现，这正是历史上两次静默错判的形状：{offenders:#?}"
        );
    }

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

        let (verdict, code, detail) = nic_rx(RateMode::Observe, None, &stats);
        assert_eq!(verdict, Verdict::NotEvaluated);
        assert_eq!(
            code,
            ReasonCode::CounterStalled,
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
        let (verdict, _, _) = nic_rx(RateMode::Observe, None, &stats);
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

    /// RX 平均达标时，发送端采样是否完整不再改写 PASS。
    #[test]
    fn a_collapsed_sender_side_sampling_is_ignored_when_rx_average_meets_target() {
        let healthy = RateStats {
            avg_mbps: Some(900.0),
            p10_mbps: Some(880.0),
            coverage: 1.0,
            rolling_coverage: 1.0,
            ..Default::default()
        };
        let target = Some(800.0);

        // 基线：两侧都完好 → 正常给出 PASS。
        assert_eq!(nic_rx(RateMode::Verify, target, &healthy).0, Verdict::Pass);

        // 发送端总采样覆盖率塌了，但 RX 平均已达标，仍然 PASS——只多一条诊断。
        let tx_low_coverage = RateStats {
            coverage: 0.80,
            ..healthy.clone()
        };
        let (verdict, code, detail) = nic_rx(RateMode::Verify, target, &healthy);
        assert_eq!((verdict, code), (Verdict::Pass, ReasonCode::None));
        assert!(detail.is_empty());
        assert!(
            rx_acceptance_diagnostics(&healthy, &tx_low_coverage, Some(1_000.0), None)
                .iter()
                .any(|line| line.contains("TX 采样覆盖率"))
        );

        // 发送端滚动窗口覆盖率塌了，仍然 PASS。
        let tx_low_rolling = RateStats {
            rolling_coverage: 0.70,
            ..healthy.clone()
        };
        let (verdict, code, _) = nic_rx(RateMode::Verify, target, &healthy);
        assert_eq!((verdict, code), (Verdict::Pass, ReasonCode::None));
        assert!(
            rx_acceptance_diagnostics(&healthy, &tx_low_rolling, Some(1_000.0), None)
                .iter()
                .any(|line| line.contains("滚动窗口"))
        );

        // 发送端根本没采到样本。
        let (verdict, _, _) = nic_rx(RateMode::Verify, target, &healthy);
        assert_eq!(verdict, Verdict::Pass);
        assert!(
            !rx_acceptance_diagnostics(&healthy, &RateStats::default(), Some(1_000.0), None)
                .is_empty()
        );

        // 目标未知时不做双侧要求：只记录实测能力。
        assert_eq!(
            nic_rx(RateMode::Observe, None, &healthy).0,
            Verdict::Measured
        );
    }

    #[test]
    fn rx_below_target_is_a_failure_even_when_sampling_has_other_problems() {
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

                    let (verdict, code, _) = nic_rx(RateMode::Verify, Some(target), &rx);

                    if coverage < MIN_RATE_SAMPLE_COVERAGE {
                        assert_eq!(
                            (verdict, code),
                            (Verdict::NotEvaluated, ReasonCode::SampleCoverageLow),
                            "RX 采样覆盖率不足仍需保持不可评价（coverage={coverage}）"
                        );
                    } else {
                        // 接收端 RX 平均一旦可信，滚动覆盖率和 P10 是否可得都不再
                        // 改变结论：低于目标就是低于目标（ADR-17）。
                        assert_eq!(
                            (verdict, code),
                            (Verdict::RateFail, ReasonCode::RxBelowTarget),
                            "RX 平均低于目标必须直接 RATE_FAIL \
                             (coverage={coverage}, rolling={rolling}, p10={p10:?})"
                        );
                        // 诊断仍要把滚动窗口那件事说出来。
                        let notes = rx_acceptance_diagnostics(&rx, &tx, Some(target), None);
                        if !rate_window_coverage_sufficient(&rx, &tx, true) {
                            assert!(
                                notes.iter().any(|line| line.contains("滚动窗口")),
                                "滚动窗口不完整要留在诊断里: {notes:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rx_verdict_matrix_keeps_missing_rx_data_unrated_but_fails_low_rx() {
        // 全程稳定在 850：35 个 5 秒窗口一个都不掉到 800 以下。
        let complete = RateStats {
            avg_mbps: Some(850.0),
            p10_mbps: Some(820.0),
            coverage: 1.0,
            rolling_coverage: 1.0,
            series: raw_series(180, |_| 850.0),
            ..Default::default()
        };
        // 接收端是唯一的判定输入（ADR-17），发送端的事另有专门用例。
        let decision = |mode, target, stats: &RateStats| {
            let (verdict, code, _) = nic_rx(mode, target, stats);
            (verdict, code)
        };

        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &RateStats::default()),
            (Verdict::NotEvaluated, ReasonCode::NicRateMissing)
        );
        let zero = RateStats {
            avg_mbps: Some(0.0),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &zero),
            (Verdict::NotEvaluated, ReasonCode::NicRateMissing)
        );

        assert_eq!(
            decision(RateMode::Observe, Some(800.0), &complete),
            (Verdict::Measured, ReasonCode::TargetUnknown),
            "observe 即使收到意外目标也只能记录测量值"
        );
        assert_eq!(
            decision(RateMode::Discover, Some(800.0), &complete),
            (Verdict::Measured, ReasonCode::TargetUnknown)
        );
        assert_eq!(
            decision(RateMode::Auto, None, &complete),
            (Verdict::Measured, ReasonCode::TargetUnknown)
        );
        assert_eq!(
            decision(RateMode::Verify, None, &complete),
            (Verdict::NotEvaluated, ReasonCode::TargetMissing)
        );
        assert_eq!(
            decision(RateMode::Verify, Some(f64::NAN), &complete),
            (Verdict::NotEvaluated, ReasonCode::TargetMissing)
        );

        let low_coverage = RateStats {
            coverage: 0.94,
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &low_coverage),
            (Verdict::NotEvaluated, ReasonCode::SampleCoverageLow)
        );
        let nan_coverage = RateStats {
            coverage: f64::NAN,
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &nan_coverage),
            (Verdict::NotEvaluated, ReasonCode::SampleCoverageLow)
        );

        let below_target = RateStats {
            avg_mbps: Some(799.0),
            p10_mbps: Some(790.0),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &below_target),
            (Verdict::RateFail, ReasonCode::RxBelowTarget)
        );

        // RX 平均低于目标时，滚动窗口是否完整不再改变原因码：结论只有一条
        // 「RX 低于门限」，滚动窗口的事去诊断里说（ADR-17）。
        let below_target_but_unreliable = RateStats {
            avg_mbps: Some(799.0),
            p10_mbps: Some(700.0),
            coverage: 1.0,
            rolling_coverage: 0.86,
            ..Default::default()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &below_target_but_unreliable),
            (Verdict::RateFail, ReasonCode::RxBelowTarget),
            "RX 低于目标就是 RX_BELOW_TARGET，不再被滚动窗口改写原因码"
        );
        let unreliable_p10_missing = RateStats {
            avg_mbps: Some(799.0),
            p10_mbps: None,
            coverage: 1.0,
            rolling_coverage: 1.0,
            ..Default::default()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &unreliable_p10_missing),
            (Verdict::RateFail, ReasonCode::RxBelowTarget),
            "P10 缺失同样不改写原因码"
        );
        let missing_p10 = RateStats {
            p10_mbps: None,
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &missing_p10),
            (Verdict::Pass, ReasonCode::None)
        );
        let nan_p10 = RateStats {
            p10_mbps: Some(f64::NAN),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &nan_p10),
            (Verdict::Pass, ReasonCode::None)
        );
        let low_rolling_coverage = RateStats {
            rolling_coverage: 0.94,
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &low_rolling_coverage),
            (Verdict::Pass, ReasonCode::None)
        );

        // P10 低于目标、但平均达标且没有连续够 5 秒的越界段：**不再** FAIL。
        //
        // P10 当过判据（`RX_UNSTABLE`），可门限贴着链路上限设时它几乎必挂：
        // run_20260828_162822_17788 的 unit-7-8 是 avg 2014 达标、P10 1996
        // 差 0.2% 被判 FAIL，而那条用例的本意只是横比两块 Wi-Fi 的协商速率。
        let jittery = RateStats {
            p10_mbps: Some(799.0),
            // 每 3 秒掉一拍到 700（仍在 640 门限之上），连一段都不成立。
            series: raw_series(180, |i| if i % 3 == 0 { 700.0 } else { 850.0 }),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &jittery),
            (Verdict::Pass, ReasonCode::None),
            "P10 已退回诊断指标，不该再单独否决一行"
        );

        // 平均达标，即使连续 6 秒掉到门限 80% 以下也 PASS。
        let dropout = RateStats {
            series: raw_series(180, |i| if (20..=25).contains(&i) { 120.0 } else { 850.0 }),
            ..complete.clone()
        };
        let (verdict, code, detail) = nic_rx(RateMode::Verify, Some(800.0), &dropout);
        assert_eq!((verdict, code), (Verdict::Pass, ReasonCode::None));
        assert!(detail.is_empty());

        // 掉到接近 0 但平均仍达标，也不改变 PASS。
        let outage = RateStats {
            series: raw_series(180, |i| if (18..=23).contains(&i) { 0.0 } else { 850.0 }),
            ..complete.clone()
        };
        let (verdict, code, detail) = nic_rx(RateMode::Verify, Some(800.0), &outage);
        assert_eq!((verdict, code), (Verdict::Pass, ReasonCode::None));
        assert!(detail.is_empty());

        // 链路比目标快不是缺陷：稳定跑在门限的 1.5 倍照样 PASS。
        // 「连续 5 秒高于 target*1.2」当过一档（RX_SPIKE），拿
        // run_20260828_162822_17788 回放打中 24 行，全是 2.5G 口配 2000
        // 目标稳定跑 2450 这种好结果，已经撤掉。
        let fast = RateStats {
            avg_mbps: Some(1_200.0),
            series: raw_series(180, |_| 1_200.0),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &fast),
            (Verdict::Pass, ReasonCode::None),
            "跑得比目标快不该判 FAIL"
        );

        // 单个采样周期掉到 0（Wi-Fi 发 probe 就是这个形态）：不判 FAIL。
        let blip = RateStats {
            series: raw_series(180, |i| if i == 60 { 0.0 } else { 850.0 }),
            ..complete.clone()
        };
        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &blip),
            (Verdict::Pass, ReasonCode::None),
            "一个采样周期的掉拍和 probe/信道扫描不可区分，不能判 FAIL"
        );

        assert_eq!(
            decision(RateMode::Verify, Some(800.0), &complete),
            (Verdict::Pass, ReasonCode::None)
        );
    }

    /// 造 1 秒一个的原始样本序列；`rate_at(i)` 给第 i 秒（从 1 起）的速率。
    fn raw_series(secs: u64, rate_at: impl Fn(u64) -> f64) -> Vec<(u64, u64, f64)> {
        (1..=secs).map(|i| (i * 1_000, 1_000, rate_at(i))).collect()
    }

    /// 报出来的秒数必须是**真秒数**。
    ///
    /// 老口径判在 5 秒滑动平均上，一次 n 秒的掉速会命中 n+4 个窗口，报出的
    /// 「最长连续」= n + 4 秒。run_20260828_162822_17788 里 7 条 RX_DROPOUT
    /// 全部虚报了这 4 秒：unit-109-110 实际只掉了 1 秒（2270 → 1499Mbps），
    /// 报告写的是「最长连续 5.0 秒」，对着 iperf 截图根本找不到。
    #[test]
    fn an_excursion_reports_how_long_it_actually_lasted() {
        let target = 800.0;
        // 第 20~26 秒掉到 100Mbps：整整 7 秒，不多不少。
        let series = raw_series(35, |i| if (20..=26).contains(&i) { 100.0 } else { 900.0 });
        let excursion = rate_excursion(&series, target, 0).expect("应检出掉坑");
        assert_eq!(excursion.kind, ExcursionKind::Dropout);
        assert_eq!(excursion.longest_ms, 7_000, "19s 起掉了 7 秒");
        assert_eq!(excursion.started_at_ms, 19_000);
        assert_eq!(excursion.extreme_mbps, 100.0);
        assert!(
            excursion.describe().contains("连续 7.0 秒"),
            "{}",
            excursion.describe()
        );

        // 两段分开时取最长的那一段，不是加起来。
        let split = raw_series(35, |i| {
            if (5..=10).contains(&i) || (20..=28).contains(&i) {
                100.0
            } else {
                900.0
            }
        });
        let excursion = rate_excursion(&split, target, 0).expect("应检出掉坑");
        assert_eq!(excursion.longest_ms, 9_000, "19s 起到 28s 止的那一段");
        assert_eq!(excursion.started_at_ms, 19_000);
        assert_eq!(excursion.runs, 2, "短的那段也要算进段数");
        assert_eq!(excursion.total_ms, 15_000);
    }

    /// 不够 [`MIN_RATE_EXCURSION_MS`] 的越界一律不算——Wi-Fi 发 probe、信道
    /// 扫描在网卡计数器上就是掉一两拍，和真故障不可区分。
    #[test]
    fn a_short_blip_is_not_an_excursion() {
        let target = 800.0;
        for blip_secs in 1..=4u64 {
            let series = raw_series(35, |i| {
                if (20..20 + blip_secs).contains(&i) {
                    0.0
                } else {
                    900.0
                }
            });
            assert!(
                rate_excursion(&series, target, 0).is_none(),
                "{blip_secs} 秒的掉拍不该判 FAIL"
            );
        }
        // 够 5 秒就必须检出。
        let series = raw_series(35, |i| if (20..25).contains(&i) { 0.0 } else { 900.0 });
        assert_eq!(
            rate_excursion(&series, target, 0).map(|e| e.kind),
            Some(ExcursionKind::Outage)
        );
    }

    /// 断流和掉坑各判各的：「基本为 0」比「掉到 10%」严得多。
    #[test]
    fn an_outage_is_told_apart_from_a_dropout() {
        let target = 1_000.0;
        let long = |rate: f64| {
            raw_series(35, move |i| {
                if (10..=20).contains(&i) {
                    rate
                } else {
                    1_100.0
                }
            })
        };

        // 0Mbps：断流。
        assert_eq!(
            rate_excursion(&long(0.0), target, 0).map(|e| e.kind),
            Some(ExcursionKind::Outage)
        );
        // 目标的 10%：还有流量在跑，算掉坑不算断流。
        assert_eq!(
            rate_excursion(&long(100.0), target, 0).map(|e| e.kind),
            Some(ExcursionKind::Dropout)
        );
        // 目标的 85%：在 80% 容差之内，不判。
        assert!(rate_excursion(&long(850.0), target, 0).is_none());
        // 高于目标一律不判：链路比目标快不是缺陷。
        assert!(rate_excursion(&long(1_300.0), target, 0).is_none());
        assert!(rate_excursion(&long(5_000.0), target, 0).is_none());
    }

    /// 采样周期是 ~1005ms 而不是整 1000ms，5 个连续样本累到 5030ms；
    /// 判定时长的边界不能由毫秒级抖动决定。
    #[test]
    fn excursion_duration_tolerates_millisecond_jitter() {
        let target = 800.0;
        // 5 个 995ms 的样本 = 4975ms，差 25ms 不到 5 秒，仍要算数。
        let series: Vec<(u64, u64, f64)> = (1..=35)
            .map(|i| {
                let end = i * 995;
                (end, 995, if (20..25).contains(&i) { 100.0 } else { 900.0 })
            })
            .collect();
        let excursion = rate_excursion(&series, target, 0).expect("4975ms 属于舍入误差");
        assert_eq!(excursion.longest_ms, 4_975);

        // 4 个样本 = 3980ms，差得远，不算。
        let series: Vec<(u64, u64, f64)> = (1..=35)
            .map(|i| {
                let end = i * 995;
                (end, 995, if (20..24).contains(&i) { 100.0 } else { 900.0 })
            })
            .collect();
        assert!(rate_excursion(&series, target, 0).is_none());
    }

    /// 没有可信目标时无从谈越界。
    #[test]
    fn an_excursion_needs_a_target() {
        let series = raw_series(35, |i| if (20..=26).contains(&i) { 0.0 } else { 900.0 });
        assert!(rate_excursion(&series, 0.0, 0).is_none());
        assert!(rate_excursion(&series, f64::NAN, 0).is_none());
        let steady = raw_series(35, |_| 900.0);
        assert!(rate_excursion(&steady, 800.0, 0).is_none());
    }

    /// `describe()` 承诺的是「自判定窗口第 X 秒起」，`series` 存的却是监控
    /// 起点的绝对时刻。run_20260905_125327_5940 的 unit-112 就因此印出了
    /// 「60 秒窗口的第 69.6 秒」——一个窗口里根本不存在的时刻。
    #[test]
    fn excursion_start_is_relative_to_the_judgement_window_not_the_monitor_origin() {
        let target = 1_800.0;
        // 监控第 20~30 秒掉到门限以下；判定窗口从第 15 秒开始，
        // 所以报出来必须是「窗口第 5 秒起」。
        let series: Vec<(u64, u64, f64)> = (15..=75)
            .map(|second| {
                let rate = if (20..=30).contains(&second) {
                    200.0
                } else {
                    1_900.0
                };
                (second * 1_000, 1_000, rate)
            })
            .collect();
        let excursion = rate_excursion(&series, target, 15_000).expect("应检出掉坑");
        assert_eq!(excursion.started_at_ms, 4_000);
        let text = excursion.describe();
        assert!(
            text.contains("自判定窗口第 4.0 秒起"),
            "越界起点必须是窗口内相对秒数: {text}"
        );
        // 绝对时刻是 19 秒；它不能出现在这句话里。
        assert!(
            !text.contains("第 19.0 秒"),
            "不能印监控起点的绝对时刻: {text}"
        );
    }

    /// 掉坑诊断尾巴那句话必须跟着**这一行自己**的平均值走。写死「平均值已达标」
    /// 会让 RATE_FAIL 行同时印出「RX 平均 948 低于目标 1800」和「平均值已达标」。
    #[test]
    fn dropout_note_does_not_claim_the_average_passed_on_a_failing_row() {
        let target = 1_800.0;
        let series: Vec<(u64, u64, f64)> = (1..=60)
            .map(|second| {
                let rate = if (10..=25).contains(&second) {
                    300.0
                } else {
                    1_100.0
                };
                (second * 1_000, 1_000, rate)
            })
            .collect();
        let avg = series.iter().map(|(_, _, rate)| rate).sum::<f64>() / series.len() as f64;
        let failing = RateStats {
            avg_mbps: Some(avg),
            p10_mbps: Some(avg),
            coverage: 1.0,
            rolling_coverage: 1.0,
            series: series.clone(),
            ..Default::default()
        };
        assert!(avg < target, "这份数据的平均值本来就不达标");
        let notes = rx_acceptance_diagnostics(&failing, &failing, Some(target), None);
        let dropout = notes
            .iter()
            .find(|line| line.contains("RX_DROPOUT"))
            .expect("应有掉坑诊断");
        assert!(
            !dropout.contains("平均值已达标"),
            "平均值没达标就不能说达标了: {dropout}"
        );
        assert!(dropout.contains("本行平均值本就不达标"), "{dropout}");

        // 平均值确实达标时，原来那句话一个字不改。
        let passing_series: Vec<(u64, u64, f64)> = (1..=60)
            .map(|second| {
                let rate = if (10..=20).contains(&second) {
                    300.0
                } else {
                    2_300.0
                };
                (second * 1_000, 1_000, rate)
            })
            .collect();
        let passing_avg = passing_series.iter().map(|(_, _, rate)| rate).sum::<f64>()
            / passing_series.len() as f64;
        assert!(passing_avg >= target);
        let passing = RateStats {
            avg_mbps: Some(passing_avg),
            p10_mbps: Some(passing_avg),
            coverage: 1.0,
            rolling_coverage: 1.0,
            series: passing_series,
            ..Default::default()
        };
        let notes = rx_acceptance_diagnostics(&passing, &passing, Some(target), None);
        assert!(
            notes
                .iter()
                .any(|line| line.contains("RX_DROPOUT") && line.contains("平均值已达标")),
            "{notes:?}"
        );
    }
}
