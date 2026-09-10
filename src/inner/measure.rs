//! 接收速率的**来源选择与判定**，全部是纯函数。
//!
//! 这一层存在的唯一理由：来源和门限的配对只能有一个说法。散在执行器、
//! 页面和报告里各写一遍，迟早会出现「报告说工具口径 PASS、判定却按网卡
//! 口径 NOT_EVALUATED」。执行器只负责把数据采上来，判什么、按谁判，
//! 从这里出。
//!
//! 两条不可逾越的底线：
//!
//! 1. **可信的低速不触发兜底**。计数器可信却只有 12 Mbps，就是 RATE_FAIL；
//!    工具那边显示 940 也不改判——那说明两个口径有一个不可信，不是达标了。
//! 2. **工具口径不继承网卡门限**。没有明确配置的工具门限就只出 MEASURED，
//!    并说明网卡验收没有形成，绝不拿网卡门限去比工具数字凑一个 PASS。
use super::config::Measurement;
use crate::reason::ReasonCode;
use crate::verdict::{Verdict, VerdictResult};
use serde::Serialize;

/// 本腿速率实际取自哪一层。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// 接收接口字节计数（与子网网卡 RX 同一口径）。
    Nic,
    /// iperf3 接收端汇总。
    Tool,
    /// 两路都没有可信结果。
    #[default]
    None,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nic => "网卡字节计数",
            Self::Tool => "工具接收汇总",
            Self::None => "无可信来源",
        }
    }
}

/// 工具接收汇总取自哪份原文。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOrigin {
    /// 发送端 client 输出里的 receiver 汇总行（含 server 回传的接收结果）。
    ClientSummary,
    /// 本单元独占的板侧 server 原文。仅在 client 侧汇总丢失时使用。
    BoardServerLog,
    /// 下行 PC 接收端 server 原文。
    PcServerLog,
}

impl ToolOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::ClientSummary => "发送端 client 的 receiver 汇总行",
            Self::BoardServerLog => "本单元独占的板侧 server 原文",
            Self::PcServerLog => "本单元独占的 PC server 原文",
        }
    }
}

/// 多流时汇总是怎么来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    /// 单流的唯一一条 receiver 汇总行。
    Single,
    /// 明确的 `[SUM]` receiver 汇总行。
    Sum,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolRate {
    pub mbps: f64,
    pub aggregate: Aggregate,
    pub origin: ToolOrigin,
}

/// 从 iperf3 文本里取**接收端**汇总速率。
///
/// 和 [`crate::cmd::iperf::parse_output`] 的 `best_receiver` 不是一回事：
/// 那个允许回落到「最后一行速率」，用来在诊断栏里填个数；这里的结果要
/// 参与判定，因此只认真正的 receiver 汇总行，并且：
///
/// * 多流必须有明确的 `[SUM]` 汇总行——取最后一条流会把 N 分之一当成总速率；
/// * 一条都没有就返回 `Err`，不拿 sender 行、不拿 interval 末行顶替；
/// * UDP 的 offered 速率（`-b`）从来不出现在 receiver 行上，天然被排除。
pub fn parse_receiver_summary(
    text: &str,
    streams: u32,
    origin: ToolOrigin,
) -> Result<ToolRate, String> {
    let mut sum: Option<f64> = None;
    let mut singles: Vec<f64> = Vec::new();
    for raw in text.lines() {
        let line = strip_ansi(raw);
        if !line.contains("receiver") {
            continue;
        }
        let Some(mbps) = last_rate_mbps(&line) else {
            continue;
        };
        if line.contains("[SUM]") {
            sum = Some(mbps);
        } else {
            singles.push(mbps);
        }
    }
    let (mbps, aggregate) = match (sum, singles.len()) {
        (Some(value), _) => (value, Aggregate::Sum),
        (None, 0) => return Err("原文里没有 receiver 汇总行".into()),
        (None, 1) if streams == 1 => (singles[0], Aggregate::Single),
        (None, found) => {
            return Err(format!(
                "{streams} 条并发流找到 {found} 条 receiver 汇总行，却没有 [SUM] 合计行；\
                 取其中任意一条都只是单流速率，不能当成本腿的接收速率"
            ));
        }
    };
    if !mbps.is_finite() || mbps < 0.0 {
        return Err(format!("receiver 汇总速率非法: {mbps}"));
    }
    Ok(ToolRate {
        mbps,
        aggregate,
        origin,
    })
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// 取一行里最后一个速率，单位换算与 [`crate::cmd::iperf::parse_output`] 同源：
/// bit 单位按 1000 进位，Byte 单位按 1024 进位。
fn last_rate_mbps(line: &str) -> Option<f64> {
    let parsed = crate::cmd::iperf::parse_output(line);
    parsed.receiver_mbps.or(parsed.last_mbps)
}

/// 网卡口径的观测结果。`acceptance` 由 `evaluate_rx_acceptance` 产出，
/// 语义和字段与子网完全一致——内环不新造一套验收规则。
#[derive(Debug, Clone)]
pub struct NicView {
    pub avg_mbps: Option<f64>,
    pub acceptance: VerdictResult,
}

impl NicView {
    /// 这一腿是否形成了可信的网卡 RX 平均值。
    ///
    /// 判据只有一个来源：验收有没有走到「能给出速率结论」那一步。
    /// PASS / RATE_FAIL / MEASURED 都意味着三道门槛（计数器没停滞、平均值
    /// 有效、覆盖率够）都过了；NOT_EVALUATED 意味着没过。与子网
    /// `bidir_total_rx_avg` 用的是同一条判据。
    pub fn trusted(&self) -> bool {
        matches!(
            self.acceptance.verdict,
            Verdict::Pass | Verdict::RateFail | Verdict::Measured
        ) && self.avg_mbps.is_some_and(f64::is_finite)
    }
    /// 不可信时的原因，用来写进兜底记录。
    pub fn unusable_reason(&self) -> String {
        format!(
            "{}: {}",
            self.acceptance.code.as_str(),
            self.acceptance.detail
        )
    }
}

/// 工具口径的观测结果。
#[derive(Debug, Clone)]
pub struct ToolView {
    pub rate: Result<ToolRate, String>,
}

impl ToolView {
    pub fn mbps(&self) -> Option<f64> {
        self.rate.as_ref().ok().map(|rate| rate.mbps)
    }
}

/// 一条腿最终采用的测量结果。
#[derive(Debug, Clone)]
pub struct LegMeasurement {
    pub source: Source,
    pub mbps: Option<f64>,
    pub target_mbps: Option<f64>,
    pub verdict: VerdictResult,
    /// 为什么没用原本优先的来源。只有真的发生了兜底才有值。
    pub fallback_reason: Option<String>,
}

fn tool_detail(rate: &ToolRate) -> String {
    let aggregate = match rate.aggregate {
        Aggregate::Sum => "[SUM] 合计行",
        Aggregate::Single => "单流 receiver 汇总行",
    };
    format!(
        "工具口径接收速率 {:.3}Mbps，取自{}的{aggregate}",
        rate.mbps,
        rate.origin.label()
    )
}

/// 工具口径的判定。**没有工具门限就只出 MEASURED**，并明说网卡验收没有形成。
fn tool_verdict(rate: &ToolRate, target: Option<f64>) -> VerdictResult {
    let detail = tool_detail(rate);
    let Some(target) = target else {
        return VerdictResult::measured(
            ReasonCode::TargetUnknown,
            format!(
                "{detail}；未配置工具口径门限，本腿只测量。\
                 网卡口径验收未形成，不能视为通过网卡 RX 验收"
            ),
        );
    };
    if rate.mbps >= target {
        VerdictResult::pass()
            .with_diagnostics(vec![format!("{detail}，达到工具口径门限 {target:.3}Mbps")])
    } else {
        VerdictResult::rate_fail(
            ReasonCode::RxBelowTarget,
            format!("{detail}，低于工具口径门限 {target:.3}Mbps"),
        )
    }
}

/// 本腿在合计判定的单元里只测量，不单独出结论。
fn measured_for_total(source: Source, mbps: f64) -> VerdictResult {
    VerdictResult::measured(
        ReasonCode::TargetUnknown,
        format!(
            "本腿接收速率 {mbps:.3}Mbps（{}）；本单元按双向合计判定一次，单腿不单独判定",
            source.label()
        ),
    )
}

/// 选择来源并给出本腿结论。
///
/// `total_mode` = 所属单元按双向合计判定，本腿只测量。
pub fn select_leg(
    strategy: Measurement,
    nic: &NicView,
    tool: &ToolView,
    nic_target: Option<f64>,
    tool_target: Option<f64>,
    total_mode: bool,
) -> LegMeasurement {
    let mut fallback_reason = None;
    let (source, mbps, target, verdict) = match strategy {
        // 严格模式：只认网卡计数，不可信就 NOT_EVALUATED。原样沿用
        // evaluate_rx_acceptance 的结论，字段语义和子网一模一样。
        Measurement::NicStrict => nic_only(nic, nic_target),
        Measurement::NicPreferred => {
            if nic.trusted() {
                nic_only(nic, nic_target)
            } else {
                match &tool.rate {
                    Ok(rate) => {
                        fallback_reason = Some(nic.unusable_reason());
                        (
                            Source::Tool,
                            Some(rate.mbps),
                            tool_target,
                            tool_verdict(rate, tool_target),
                        )
                    }
                    Err(error) => (
                        Source::None,
                        None,
                        None,
                        VerdictResult::not_evaluated(
                            ReasonCode::NicRateMissing,
                            format!(
                                "网卡字节计数不可信（{}），工具接收汇总也拿不到（{error}）；\
                                 两路都没有可信的接收端结果，不用发送端顶替",
                                nic.unusable_reason()
                            ),
                        ),
                    ),
                }
            }
        }
        // 明确使用工具口径：字节计数即便可用也只并列作诊断，不改判。
        Measurement::Tool => match &tool.rate {
            Ok(rate) => (
                Source::Tool,
                Some(rate.mbps),
                tool_target,
                tool_verdict(rate, tool_target),
            ),
            Err(error) => (
                Source::None,
                None,
                None,
                VerdictResult::not_evaluated(
                    ReasonCode::IperfSummaryLost,
                    format!(
                        "本策略明确使用工具接收速率，但拿不到 receiver 汇总（{error}）；\
                         不改用网卡计数，也不拿 sender 顶替"
                    ),
                ),
            ),
        },
    };
    let verdict = match (total_mode, mbps) {
        (true, Some(value)) if source != Source::None => measured_for_total(source, value),
        _ => verdict,
    };
    LegMeasurement {
        source,
        mbps,
        target_mbps: if total_mode { None } else { target },
        verdict,
        fallback_reason,
    }
}

fn nic_only(
    nic: &NicView,
    nic_target: Option<f64>,
) -> (Source, Option<f64>, Option<f64>, VerdictResult) {
    if nic.trusted() {
        (
            Source::Nic,
            nic.avg_mbps,
            nic_target,
            nic.acceptance.clone(),
        )
    } else {
        (Source::None, None, None, nic.acceptance.clone())
    }
}

/// 双向合计。
///
/// 只允许**同一来源层次**的两端接收速率相加：一腿网卡、一腿工具的「合计」
/// 没有物理意义——字节计数记的是接口总流量，工具汇总记的是这条测试流的
/// 应用层接收吞吐，把它们加起来得到的数不对应任何东西。
pub fn total_verdict(
    legs: &[&LegMeasurement],
    nic_total: Option<f64>,
    tool_total: Option<f64>,
) -> VerdictResult {
    let [a, b] = legs else {
        return VerdictResult::not_evaluated(
            ReasonCode::UnitDirectionResultMissing,
            "双向合计需要上行和下行两条腿的结果，本单元缺少其中一条",
        );
    };
    if a.source == Source::None || b.source == Source::None {
        return VerdictResult::not_evaluated(
            ReasonCode::NicRateMissing,
            format!(
                "双向合计需要两条腿都有可信的接收速率：上行 {}，下行 {}",
                a.source.label(),
                b.source.label()
            ),
        );
    }
    if a.source != b.source {
        return VerdictResult::not_evaluated(
            ReasonCode::NicRateMissing,
            format!(
                "两条腿的来源层次不同（上行 {}，下行 {}）；\
                 接口字节计数与工具接收汇总不是同一层的数值，不能相加",
                a.source.label(),
                b.source.label()
            ),
        );
    }
    let (Some(up), Some(down)) = (a.mbps, b.mbps) else {
        return VerdictResult::not_evaluated(
            ReasonCode::NicRateMissing,
            "双向合计缺少某条腿的接收速率",
        );
    };
    let total = up + down;
    if !total.is_finite() {
        return VerdictResult::not_evaluated(
            ReasonCode::NoValidMeasurement,
            format!(
                "双向接收速率合计不是有限值（上行 {up:.3} + 下行 {down:.3}）；拒绝把溢出值当作通过"
            ),
        );
    }
    let source = a.source;
    let target = if source == Source::Tool {
        tool_total
    } else {
        nic_total
    };
    let detail = format!(
        "双向接收速率合计 {total:.3}Mbps（上行 {up:.3} + 下行 {down:.3}，同为{}）",
        source.label()
    );
    let Some(target) = target else {
        return VerdictResult::measured(
            ReasonCode::TargetUnknown,
            format!("{detail}；未配置该口径的双向合计门限，本单元只测量"),
        );
    };
    if total >= target {
        VerdictResult::pass().with_diagnostics(vec![format!("{detail}，门限 {target:.3}Mbps")])
    } else {
        VerdictResult::rate_fail(
            ReasonCode::RxBelowTarget,
            format!("{detail}，低于门限 {target:.3}Mbps"),
        )
    }
}

/// 「计数器不适配」的证据。
///
/// 单纯 RX 低于门限**不是**证据——那可能就是链路真的慢。只有计数器全程
/// 冻结、同时工具侧收到了可信的非零流量，才说明这块计数器没记这条路径上
/// 的包。这条只写进诊断，不参与判定，也不会自动改口径。
pub fn counter_mismatch_hint(
    nic: &NicView,
    tool: &ToolView,
    host: &str,
    iface: &str,
) -> Option<String> {
    let stalled = nic.acceptance.code == ReasonCode::CounterStalled;
    let tool_mbps = tool.mbps().filter(|value| *value > 0.0)?;
    stalled.then(|| {
        format!(
            "COUNTER_SOURCE_SUSPECT: {host} {iface} 的字节计数在整个判定窗口内零增长，\
             而工具侧收到 {tool_mbps:.3}Mbps 的可信流量；这是该接口计数器不适配本路径的证据，\
             可考虑改用其他统计接口或工具口径策略。本条只作诊断，不改变已形成的判定。"
        )
    })
}
