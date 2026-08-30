//! 判定原因在报告里的呈现，以及**与展示指标的一致性核对**。
//!
//! 核对这件事看着多余，其实是报告最后一道防线：判定链和渲染层各自演进，
//! 一旦某个码的前提变了而这里没跟上，报告就会同时印出「RX 平均 2014 达标」
//! 和「RX_BELOW_TARGET」。宁可印一句「判定原因与展示指标不一致」，也不能
//! 让两个互相矛盾的结论并排出现。

use super::*;

pub fn report_reason(code: ReasonCode, detail: &str) -> String {
    let code = code.as_str();
    match (code.is_empty(), detail.is_empty()) {
        (false, false) => format!("{code}: {detail}"),
        (false, true) => code.to_string(),
        (true, false) => detail.to_string(),
        (true, true) => NOT_APPLICABLE.to_string(),
    }
}

pub(super) fn traffic_pass_reason(direction: &DirectionSummary) -> String {
    match (direction.rx_avg, direction.rx_p10, direction.target_mbps) {
        (Some(avg), Some(p10), Some(target)) if avg >= target && p10 >= target => format!(
            "RX_TARGET_MET: RX 平均 {avg:.3} Mbps、RX-P10 {p10:.3} Mbps 均不低于目标 {target:.3} Mbps"
        ),
        (Some(avg), Some(p10), Some(target)) => format!(
            "判定结果与展示指标不一致: PASS；RX 平均/P10/目标为 {avg:.3}/{p10:.3}/{target:.3} Mbps"
        ),
        (Some(avg), None, Some(target)) if avg >= target => format!(
            "RX_TARGET_MET: RX 平均 {avg:.3} Mbps 不低于目标 {target:.3} Mbps；RX-P10 未采集"
        ),
        (Some(avg), None, Some(target)) => format!(
            "判定结果与展示指标不一致: PASS；RX 平均 {avg:.3} Mbps 低于目标 {target:.3} Mbps"
        ),
        _ => "PASS: 方向执行完成".into(),
    }
}

pub(super) fn direction_reason_text(direction: &DirectionSummary, fallback: &str) -> String {
    let explicit = if !direction.reason_code.is_empty() || !direction.reason_detail.is_empty() {
        report_reason(direction.reason_code, &direction.reason_detail)
    } else if !direction.reason.is_empty() {
        direction.reason.clone()
    } else {
        String::new()
    };
    let missing = explicit.is_empty() || explicit == NOT_APPLICABLE;
    let reason = if missing && direction.verdict == Verdict::Pass {
        if direction.ping_loss.is_some()
            || direction.ping_min.is_some()
            || direction.ping_avg.is_some()
            || direction.ping_max.is_some()
        {
            ping_pass_reason(
                direction.ping_loss,
                direction.ping_min,
                direction.ping_avg,
                direction.ping_max,
            )
        } else {
            traffic_pass_reason(direction)
        }
    } else if missing {
        fallback.to_string()
    } else {
        explicit
    };
    validate_rate_reason(
        &reason,
        direction.rx_avg,
        direction.rx_p10,
        direction.target_mbps,
    )
}

pub(super) fn metric_reason_mismatch(code: ReasonCode, observed: &str) -> String {
    format!("判定原因与展示指标不一致: {code}；{observed}")
}

pub(super) fn validate_rate_reason(
    reason: &str,
    rx_avg: Option<f64>,
    rx_p10: Option<f64>,
    target_mbps: Option<f64>,
) -> String {
    let code = ReasonCode::parse_prefix(reason);
    match code {
        ReasonCode::RxBelowTarget => match (rx_avg, target_mbps) {
            (Some(rx), Some(target)) if rx < target => format!(
                "RX_BELOW_TARGET: RX 平均 {rx:.3} Mbps < 目标 {target:.3} Mbps"
            ),
            (Some(rx), Some(target)) => metric_reason_mismatch(
                code,
                &format!("RX 平均 {rx:.3} Mbps >= 目标 {target:.3} Mbps"),
            ),
            _ => metric_reason_mismatch(code, "缺少 RX 平均或目标，无法核对该判定"),
        },
        ReasonCode::RxP10BelowTarget => match (rx_p10, target_mbps) {
            (Some(rx_p10), Some(target)) if rx_p10 < target => format!(
                "RX_P10_BELOW_TARGET: RX-P10 {rx_p10:.3} Mbps < 目标 {target:.3} Mbps"
            ),
            (Some(rx_p10), Some(target)) => metric_reason_mismatch(
                code,
                &format!("RX-P10 {rx_p10:.3} Mbps >= 目标 {target:.3} Mbps"),
            ),
            _ => metric_reason_mismatch(code, "缺少 RX-P10 或目标，无法核对该判定"),
        },
        // 断流/掉坑的共同前提是「**平均速率达标**，但窗口里有连续够 5 秒的
        // 越界段」（见 rate_window::rate_excursion）。越界段本身靠原因文本里的
        // 起点和连续秒数说明，这里核对的是那个前提——它要是不成立，说明判定和
        // 展示的指标对不上。
        //
        // P10 不在核对之列：它已经退回诊断指标，不再参与 PASS/FAIL，拿它做
        // 前提会把正确的判定标成「不一致」。
        ReasonCode::RxOutage | ReasonCode::RxDropout => match (rx_avg, target_mbps) {
            (Some(rx), Some(target)) if rx >= target => reason.to_string(),
            (Some(rx), Some(target)) => metric_reason_mismatch(
                code,
                &format!("要求 RX 平均 >= 目标；当前为 {rx:.3}/{target:.3} Mbps"),
            ),
            _ => metric_reason_mismatch(code, "缺少 RX 平均或目标，无法核对该判定"),
        },
        // 已退役的码：执行侧不再产出，这条分支只服务于「拿历史数据重渲染」。
        // 详见 `reason::ReasonCode::RxUnstable` 的注释。
        ReasonCode::RxUnstable => match (rx_avg, rx_p10, target_mbps) {
            (Some(rx), Some(rx_p10), Some(target)) if rx >= target && rx_p10 < target => format!(
                "RX_UNSTABLE: RX 平均 {rx:.3} Mbps >= 目标 {target:.3} Mbps，RX-P10 {rx_p10:.3} Mbps < 目标 {target:.3} Mbps"
            ),
            (Some(rx), Some(rx_p10), Some(target)) => metric_reason_mismatch(
                code,
                &format!(
                    "要求 RX 平均 >= 目标且 RX-P10 < 目标；当前为 {rx:.3}/{rx_p10:.3}/{target:.3} Mbps"
                ),
            ),
            _ => metric_reason_mismatch(
                code,
                "缺少 RX 平均、RX-P10 或目标，无法核对该判定",
            ),
        },
        _ => reason.to_string(),
    }
}

pub(super) fn ping_pass_reason(
    ping_loss: Option<f64>,
    ping_min: Option<f64>,
    ping_avg: Option<f64>,
    ping_max: Option<f64>,
) -> String {
    let quality = quality_text(None, ping_loss, ping_min, ping_avg, ping_max, true);
    if quality == NOT_COLLECTED {
        "PING_OK: Ping 执行完成".into()
    } else {
        format!("PING_OK: {quality}")
    }
}

pub(super) fn group_reason(group: &UnitGroup<'_>) -> String {
    let reason = group
        .summary
        .filter(|row| !row.reason_code.is_empty() || !row.reason_detail.is_empty())
        .map(|row| report_reason(row.reason_code, &row.reason_detail))
        .or_else(|| {
            group
                .details
                .iter()
                .find(|row| !row.reason_code.is_empty() || !row.reason_detail.is_empty())
                .map(|row| report_reason(row.reason_code, &row.reason_detail))
        });
    reason.unwrap_or_else(|| NOT_APPLICABLE.into())
}
