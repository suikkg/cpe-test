//! 判定原因码。
//!
//! 这些码是**判定层写给人看的唯一契约**：报告的处置建议按它分流，双向单元
//! 的聚合按它决定谁能盖住谁，运维照着它决定是重跑还是查设备。
//!
//! 之所以是 enum 而不是 `String`：同一个码此前要在 5 个地方各写一遍字面量
//! （产出点、`disposition_advice` 的分支、硬失败码表、腿本地码表、报告的
//! 一致性核对），任何一处拼错都不会有编译期信号，只会在报告里静默变成
//! 「无建议」。v4.3.1 的 `RX_DROPOUT` 就这样当了一整个版本的哑巴码。
//! 换成 enum 之后，漏登记是编译错误，不是运行时的一片空白。
//!
//! 字符串表示保持不变：报告、CSV、JSON 里仍然是 `RX_BELOW_TARGET` 这样的
//! 大写下划线形式，历史报告和外部脚本不受影响。

use std::fmt;
use std::str::FromStr;

/// 判定原因码。`None` 表示「这一行没有原因码」，对应旧代码里的空字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ReasonCode {
    /// 没有原因码。序列化成空字符串，与历史报告一致。
    #[default]
    None,
    ActiveStreamsLow,
    ConfiguredLoadTooLow,
    CounterStalled,
    CtsArgsInvalid,
    CtsCleanupFailed,
    CtsClientAborted,
    CtsClientCancelled,
    CtsClientJobIdMismatch,
    CtsClientProcessCleanupUnconfirmed,
    CtsClientProcessNotStarted,
    CtsClientResultMissing,
    CtsClientStartFailed,
    CtsClientStatusFailed,
    CtsClientStopFailed,
    CtsClientUserCancelled,
    CtsClientWaitInvalid,
    CtsEffectiveWindowShort,
    CtsMonitorNoSamples,
    CtsMonitorRuntimeError,
    CtsMonitorStartFailed,
    CtsMonitorStopFailed,
    CtsNoMeasurement,
    CtsPreflightFailed,
    CtsProcessControlFailed,
    CtsProcessStartFailed,
    CtsRuntimeErrors,
    CtsServerCancelled,
    CtsServerFailed,
    CtsServerProcessCleanupUnconfirmed,
    CtsServerProcessNotStarted,
    CtsServerStopFailed,
    CtsSingleUdpStreamFailed,
    CtsUdpLossDataMissing,
    CtsUdpLossHigh,
    EffectiveWindowShort,
    FlowFailed,
    FlowMeasured,
    GatewayNotFound,
    IperfEffectiveWindowShort,
    IperfExecFailed,
    IperfPreflightFailed,
    IperfRuntimeErrors,
    IperfSummaryLost,
    LegThreadPanic,
    NicDisappeared,
    NicRateMissing,
    NoStreamStarted,
    NoValidMeasurement,
    OfferedLoadLow,
    Pass,
    PingExecError,
    PingGatewayUnreachable,
    PingOk,
    PingSubnetUnreachable,
    PingTimeout,
    PingUnreachable,
    RateWindowCoverageLow,
    ResourceCleanupFailed,
    ResumeFreshPass,
    RxBelowTarget,
    RxDropout,
    RxOutage,
    RxP10BelowTarget,
    RxTargetMet,
    RxUnstable,
    SampleCoverageLow,
    SingleUdpStreamFailed,
    TargetMissing,
    TargetUnknown,
    UdpGroupDispatchError,
    UdpLossDataMissing,
    UdpLossHigh,
    CtsInternalNoAttempt,
    CtsServerExitedEarly,
    CtsServerJobIdMismatch,
    CtsServerStartFailed,
    CtsServerStatusFailed,
    UnitDirectionResultMissing,
    UnitPanic,
}

impl ReasonCode {
    /// 报告/CSV/JSON 里的字符串表示。
    pub const fn as_str(self) -> &'static str {
        match self {
            ReasonCode::None => "",
            ReasonCode::ActiveStreamsLow => "ACTIVE_STREAMS_LOW",
            ReasonCode::ConfiguredLoadTooLow => "CONFIGURED_LOAD_TOO_LOW",
            ReasonCode::CounterStalled => "COUNTER_STALLED",
            ReasonCode::CtsArgsInvalid => "CTSTRAFFIC_ARGS_INVALID",
            ReasonCode::CtsCleanupFailed => "CTSTRAFFIC_CLEANUP_FAILED",
            ReasonCode::CtsClientAborted => "CTSTRAFFIC_CLIENT_ABORTED",
            ReasonCode::CtsClientCancelled => "CTSTRAFFIC_CLIENT_CANCELLED",
            ReasonCode::CtsClientJobIdMismatch => "CTSTRAFFIC_CLIENT_JOB_ID_MISMATCH",
            ReasonCode::CtsClientProcessCleanupUnconfirmed => {
                "CTSTRAFFIC_CLIENT_PROCESS_CLEANUP_UNCONFIRMED"
            }
            ReasonCode::CtsClientProcessNotStarted => "CTSTRAFFIC_CLIENT_PROCESS_NOT_STARTED",
            ReasonCode::CtsClientResultMissing => "CTSTRAFFIC_CLIENT_RESULT_MISSING",
            ReasonCode::CtsClientStartFailed => "CTSTRAFFIC_CLIENT_START_FAILED",
            ReasonCode::CtsClientStatusFailed => "CTSTRAFFIC_CLIENT_STATUS_FAILED",
            ReasonCode::CtsClientStopFailed => "CTSTRAFFIC_CLIENT_STOP_FAILED",
            ReasonCode::CtsClientUserCancelled => "CTSTRAFFIC_CLIENT_USER_CANCELLED",
            ReasonCode::CtsClientWaitInvalid => "CTSTRAFFIC_CLIENT_WAIT_INVALID",
            ReasonCode::CtsEffectiveWindowShort => "CTSTRAFFIC_EFFECTIVE_WINDOW_SHORT",
            ReasonCode::CtsMonitorNoSamples => "CTSTRAFFIC_MONITOR_NO_SAMPLES",
            ReasonCode::CtsMonitorRuntimeError => "CTSTRAFFIC_MONITOR_RUNTIME_ERROR",
            ReasonCode::CtsMonitorStartFailed => "CTSTRAFFIC_MONITOR_START_FAILED",
            ReasonCode::CtsMonitorStopFailed => "CTSTRAFFIC_MONITOR_STOP_FAILED",
            ReasonCode::CtsNoMeasurement => "CTSTRAFFIC_NO_MEASUREMENT",
            ReasonCode::CtsPreflightFailed => "CTSTRAFFIC_PREFLIGHT_FAILED",
            ReasonCode::CtsProcessControlFailed => "CTSTRAFFIC_PROCESS_CONTROL_FAILED",
            ReasonCode::CtsProcessStartFailed => "CTSTRAFFIC_PROCESS_START_FAILED",
            ReasonCode::CtsRuntimeErrors => "CTSTRAFFIC_RUNTIME_ERRORS",
            ReasonCode::CtsServerCancelled => "CTSTRAFFIC_SERVER_CANCELLED",
            ReasonCode::CtsServerFailed => "CTSTRAFFIC_SERVER_FAILED",
            ReasonCode::CtsServerProcessCleanupUnconfirmed => {
                "CTSTRAFFIC_SERVER_PROCESS_CLEANUP_UNCONFIRMED"
            }
            ReasonCode::CtsServerProcessNotStarted => "CTSTRAFFIC_SERVER_PROCESS_NOT_STARTED",
            ReasonCode::CtsServerStopFailed => "CTSTRAFFIC_SERVER_STOP_FAILED",
            ReasonCode::CtsSingleUdpStreamFailed => "CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED",
            ReasonCode::CtsUdpLossDataMissing => "CTSTRAFFIC_UDP_LOSS_DATA_MISSING",
            ReasonCode::CtsUdpLossHigh => "CTSTRAFFIC_UDP_LOSS_HIGH",
            ReasonCode::EffectiveWindowShort => "EFFECTIVE_WINDOW_SHORT",
            ReasonCode::FlowFailed => "FLOW_FAILED",
            ReasonCode::FlowMeasured => "FLOW_MEASURED",
            ReasonCode::GatewayNotFound => "GATEWAY_NOT_FOUND",
            ReasonCode::IperfEffectiveWindowShort => "IPERF_EFFECTIVE_WINDOW_SHORT",
            ReasonCode::IperfExecFailed => "IPERF_EXEC_FAILED",
            ReasonCode::IperfPreflightFailed => "IPERF_PREFLIGHT_FAILED",
            ReasonCode::IperfRuntimeErrors => "IPERF_RUNTIME_ERRORS",
            ReasonCode::IperfSummaryLost => "IPERF_SUMMARY_LOST",
            ReasonCode::LegThreadPanic => "LEG_THREAD_PANIC",
            ReasonCode::NicDisappeared => "NIC_DISAPPEARED",
            ReasonCode::NicRateMissing => "NIC_RATE_MISSING",
            ReasonCode::NoStreamStarted => "NO_STREAM_STARTED",
            ReasonCode::NoValidMeasurement => "NO_VALID_MEASUREMENT",
            ReasonCode::OfferedLoadLow => "OFFERED_LOAD_LOW",
            ReasonCode::Pass => "PASS",
            ReasonCode::PingExecError => "PING_EXEC_ERROR",
            ReasonCode::PingGatewayUnreachable => "PING_GATEWAY_UNREACHABLE",
            ReasonCode::PingOk => "PING_OK",
            ReasonCode::PingSubnetUnreachable => "PING_SUBNET_UNREACHABLE",
            ReasonCode::PingTimeout => "PING_TIMEOUT",
            ReasonCode::PingUnreachable => "PING_UNREACHABLE",
            ReasonCode::RateWindowCoverageLow => "RATE_WINDOW_COVERAGE_LOW",
            ReasonCode::ResourceCleanupFailed => "RESOURCE_CLEANUP_FAILED",
            ReasonCode::ResumeFreshPass => "RESUME_FRESH_PASS",
            ReasonCode::RxBelowTarget => "RX_BELOW_TARGET",
            ReasonCode::RxDropout => "RX_DROPOUT",
            ReasonCode::RxOutage => "RX_OUTAGE",
            ReasonCode::RxP10BelowTarget => "RX_P10_BELOW_TARGET",
            ReasonCode::RxTargetMet => "RX_TARGET_MET",
            ReasonCode::RxUnstable => "RX_UNSTABLE",
            ReasonCode::SampleCoverageLow => "SAMPLE_COVERAGE_LOW",
            ReasonCode::SingleUdpStreamFailed => "SINGLE_UDP_STREAM_FAILED",
            ReasonCode::TargetMissing => "TARGET_MISSING",
            ReasonCode::TargetUnknown => "TARGET_UNKNOWN",
            ReasonCode::UdpGroupDispatchError => "UDP_GROUP_DISPATCH_ERROR",
            ReasonCode::UdpLossDataMissing => "UDP_LOSS_DATA_MISSING",
            ReasonCode::UdpLossHigh => "UDP_LOSS_HIGH",
            ReasonCode::CtsInternalNoAttempt => "CTSTRAFFIC_INTERNAL_NO_ATTEMPT",
            ReasonCode::CtsServerExitedEarly => "CTSTRAFFIC_SERVER_EXITED_EARLY",
            ReasonCode::CtsServerJobIdMismatch => "CTSTRAFFIC_SERVER_JOB_ID_MISMATCH",
            ReasonCode::CtsServerStartFailed => "CTSTRAFFIC_SERVER_START_FAILED",
            ReasonCode::CtsServerStatusFailed => "CTSTRAFFIC_SERVER_STATUS_FAILED",
            ReasonCode::UnitDirectionResultMissing => "UNIT_DIRECTION_RESULT_MISSING",
            ReasonCode::UnitPanic => "UNIT_PANIC",
        }
    }

    /// 这一行有没有原因码。名字与旧的 `String::is_empty` 一致，调用点不必改。
    pub const fn is_empty(self) -> bool {
        matches!(self, ReasonCode::None)
    }

    /// 从形如 `"RX_BELOW_TARGET: RX 平均 ..."` 的原因文本里取出码。
    ///
    /// 报告里码和明细是同一串文本，取码不能靠「找第一个冒号」——明细自己
    /// 也带冒号。只认开头那一段全大写下划线的前缀。
    pub fn parse_prefix(reason: &str) -> Self {
        let head = reason
            .split_once(':')
            .map(|(head, _)| head)
            .unwrap_or(reason)
            .trim();
        head.parse().unwrap_or(ReasonCode::None)
    }
}

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReasonCode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "" => ReasonCode::None,
            "ACTIVE_STREAMS_LOW" => ReasonCode::ActiveStreamsLow,
            "CONFIGURED_LOAD_TOO_LOW" => ReasonCode::ConfiguredLoadTooLow,
            "COUNTER_STALLED" => ReasonCode::CounterStalled,
            "CTSTRAFFIC_ARGS_INVALID" => ReasonCode::CtsArgsInvalid,
            "CTSTRAFFIC_CLEANUP_FAILED" => ReasonCode::CtsCleanupFailed,
            "CTSTRAFFIC_CLIENT_ABORTED" => ReasonCode::CtsClientAborted,
            "CTSTRAFFIC_CLIENT_CANCELLED" => ReasonCode::CtsClientCancelled,
            "CTSTRAFFIC_CLIENT_JOB_ID_MISMATCH" => ReasonCode::CtsClientJobIdMismatch,
            "CTSTRAFFIC_CLIENT_PROCESS_CLEANUP_UNCONFIRMED" => {
                ReasonCode::CtsClientProcessCleanupUnconfirmed
            }
            "CTSTRAFFIC_CLIENT_PROCESS_NOT_STARTED" => ReasonCode::CtsClientProcessNotStarted,
            "CTSTRAFFIC_CLIENT_RESULT_MISSING" => ReasonCode::CtsClientResultMissing,
            "CTSTRAFFIC_CLIENT_START_FAILED" => ReasonCode::CtsClientStartFailed,
            "CTSTRAFFIC_CLIENT_STATUS_FAILED" => ReasonCode::CtsClientStatusFailed,
            "CTSTRAFFIC_CLIENT_STOP_FAILED" => ReasonCode::CtsClientStopFailed,
            "CTSTRAFFIC_CLIENT_USER_CANCELLED" => ReasonCode::CtsClientUserCancelled,
            "CTSTRAFFIC_CLIENT_WAIT_INVALID" => ReasonCode::CtsClientWaitInvalid,
            "CTSTRAFFIC_EFFECTIVE_WINDOW_SHORT" => ReasonCode::CtsEffectiveWindowShort,
            "CTSTRAFFIC_MONITOR_NO_SAMPLES" => ReasonCode::CtsMonitorNoSamples,
            "CTSTRAFFIC_MONITOR_RUNTIME_ERROR" => ReasonCode::CtsMonitorRuntimeError,
            "CTSTRAFFIC_MONITOR_START_FAILED" => ReasonCode::CtsMonitorStartFailed,
            "CTSTRAFFIC_MONITOR_STOP_FAILED" => ReasonCode::CtsMonitorStopFailed,
            "CTSTRAFFIC_NO_MEASUREMENT" => ReasonCode::CtsNoMeasurement,
            "CTSTRAFFIC_PREFLIGHT_FAILED" => ReasonCode::CtsPreflightFailed,
            "CTSTRAFFIC_PROCESS_CONTROL_FAILED" => ReasonCode::CtsProcessControlFailed,
            "CTSTRAFFIC_PROCESS_START_FAILED" => ReasonCode::CtsProcessStartFailed,
            "CTSTRAFFIC_RUNTIME_ERRORS" => ReasonCode::CtsRuntimeErrors,
            "CTSTRAFFIC_SERVER_CANCELLED" => ReasonCode::CtsServerCancelled,
            "CTSTRAFFIC_SERVER_FAILED" => ReasonCode::CtsServerFailed,
            "CTSTRAFFIC_SERVER_PROCESS_CLEANUP_UNCONFIRMED" => {
                ReasonCode::CtsServerProcessCleanupUnconfirmed
            }
            "CTSTRAFFIC_SERVER_PROCESS_NOT_STARTED" => ReasonCode::CtsServerProcessNotStarted,
            "CTSTRAFFIC_SERVER_STOP_FAILED" => ReasonCode::CtsServerStopFailed,
            "CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED" => ReasonCode::CtsSingleUdpStreamFailed,
            "CTSTRAFFIC_UDP_LOSS_DATA_MISSING" => ReasonCode::CtsUdpLossDataMissing,
            "CTSTRAFFIC_UDP_LOSS_HIGH" => ReasonCode::CtsUdpLossHigh,
            "EFFECTIVE_WINDOW_SHORT" => ReasonCode::EffectiveWindowShort,
            "FLOW_FAILED" => ReasonCode::FlowFailed,
            "FLOW_MEASURED" => ReasonCode::FlowMeasured,
            "GATEWAY_NOT_FOUND" => ReasonCode::GatewayNotFound,
            "IPERF_EFFECTIVE_WINDOW_SHORT" => ReasonCode::IperfEffectiveWindowShort,
            "IPERF_EXEC_FAILED" => ReasonCode::IperfExecFailed,
            "IPERF_PREFLIGHT_FAILED" => ReasonCode::IperfPreflightFailed,
            "IPERF_RUNTIME_ERRORS" => ReasonCode::IperfRuntimeErrors,
            "IPERF_SUMMARY_LOST" => ReasonCode::IperfSummaryLost,
            "LEG_THREAD_PANIC" => ReasonCode::LegThreadPanic,
            "NIC_DISAPPEARED" => ReasonCode::NicDisappeared,
            "NIC_RATE_MISSING" => ReasonCode::NicRateMissing,
            "NO_STREAM_STARTED" => ReasonCode::NoStreamStarted,
            "NO_VALID_MEASUREMENT" => ReasonCode::NoValidMeasurement,
            "OFFERED_LOAD_LOW" => ReasonCode::OfferedLoadLow,
            "PASS" => ReasonCode::Pass,
            "PING_EXEC_ERROR" => ReasonCode::PingExecError,
            "PING_GATEWAY_UNREACHABLE" => ReasonCode::PingGatewayUnreachable,
            "PING_OK" => ReasonCode::PingOk,
            "PING_SUBNET_UNREACHABLE" => ReasonCode::PingSubnetUnreachable,
            "PING_TIMEOUT" => ReasonCode::PingTimeout,
            "PING_UNREACHABLE" => ReasonCode::PingUnreachable,
            "RATE_WINDOW_COVERAGE_LOW" => ReasonCode::RateWindowCoverageLow,
            "RESOURCE_CLEANUP_FAILED" => ReasonCode::ResourceCleanupFailed,
            "RESUME_FRESH_PASS" => ReasonCode::ResumeFreshPass,
            "RX_BELOW_TARGET" => ReasonCode::RxBelowTarget,
            "RX_DROPOUT" => ReasonCode::RxDropout,
            "RX_OUTAGE" => ReasonCode::RxOutage,
            "RX_P10_BELOW_TARGET" => ReasonCode::RxP10BelowTarget,
            "RX_TARGET_MET" => ReasonCode::RxTargetMet,
            "RX_UNSTABLE" => ReasonCode::RxUnstable,
            "SAMPLE_COVERAGE_LOW" => ReasonCode::SampleCoverageLow,
            "SINGLE_UDP_STREAM_FAILED" => ReasonCode::SingleUdpStreamFailed,
            "TARGET_MISSING" => ReasonCode::TargetMissing,
            "TARGET_UNKNOWN" => ReasonCode::TargetUnknown,
            "UDP_GROUP_DISPATCH_ERROR" => ReasonCode::UdpGroupDispatchError,
            "UDP_LOSS_DATA_MISSING" => ReasonCode::UdpLossDataMissing,
            "UDP_LOSS_HIGH" => ReasonCode::UdpLossHigh,
            "CTSTRAFFIC_INTERNAL_NO_ATTEMPT" => ReasonCode::CtsInternalNoAttempt,
            "CTSTRAFFIC_SERVER_EXITED_EARLY" => ReasonCode::CtsServerExitedEarly,
            "CTSTRAFFIC_SERVER_JOB_ID_MISMATCH" => ReasonCode::CtsServerJobIdMismatch,
            "CTSTRAFFIC_SERVER_START_FAILED" => ReasonCode::CtsServerStartFailed,
            "CTSTRAFFIC_SERVER_STATUS_FAILED" => ReasonCode::CtsServerStatusFailed,
            "UNIT_DIRECTION_RESULT_MISSING" => ReasonCode::UnitDirectionResultMissing,
            "UNIT_PANIC" => ReasonCode::UnitPanic,
            _ => return Err(()),
        })
    }
}

/// 所有码，供穷举式测试使用。
#[cfg(test)]
pub const ALL_REASON_CODES: [ReasonCode; 79] = [
    ReasonCode::ActiveStreamsLow,
    ReasonCode::ConfiguredLoadTooLow,
    ReasonCode::CounterStalled,
    ReasonCode::CtsArgsInvalid,
    ReasonCode::CtsCleanupFailed,
    ReasonCode::CtsClientAborted,
    ReasonCode::CtsClientCancelled,
    ReasonCode::CtsClientJobIdMismatch,
    ReasonCode::CtsClientProcessCleanupUnconfirmed,
    ReasonCode::CtsClientProcessNotStarted,
    ReasonCode::CtsClientResultMissing,
    ReasonCode::CtsClientStartFailed,
    ReasonCode::CtsClientStatusFailed,
    ReasonCode::CtsClientStopFailed,
    ReasonCode::CtsClientUserCancelled,
    ReasonCode::CtsClientWaitInvalid,
    ReasonCode::CtsEffectiveWindowShort,
    ReasonCode::CtsMonitorNoSamples,
    ReasonCode::CtsMonitorRuntimeError,
    ReasonCode::CtsMonitorStartFailed,
    ReasonCode::CtsMonitorStopFailed,
    ReasonCode::CtsNoMeasurement,
    ReasonCode::CtsPreflightFailed,
    ReasonCode::CtsProcessControlFailed,
    ReasonCode::CtsProcessStartFailed,
    ReasonCode::CtsRuntimeErrors,
    ReasonCode::CtsServerCancelled,
    ReasonCode::CtsServerFailed,
    ReasonCode::CtsServerProcessCleanupUnconfirmed,
    ReasonCode::CtsServerProcessNotStarted,
    ReasonCode::CtsServerStopFailed,
    ReasonCode::CtsSingleUdpStreamFailed,
    ReasonCode::CtsUdpLossDataMissing,
    ReasonCode::CtsUdpLossHigh,
    ReasonCode::EffectiveWindowShort,
    ReasonCode::FlowFailed,
    ReasonCode::FlowMeasured,
    ReasonCode::GatewayNotFound,
    ReasonCode::IperfEffectiveWindowShort,
    ReasonCode::IperfExecFailed,
    ReasonCode::IperfPreflightFailed,
    ReasonCode::IperfRuntimeErrors,
    ReasonCode::IperfSummaryLost,
    ReasonCode::LegThreadPanic,
    ReasonCode::NicDisappeared,
    ReasonCode::NicRateMissing,
    ReasonCode::NoStreamStarted,
    ReasonCode::NoValidMeasurement,
    ReasonCode::OfferedLoadLow,
    ReasonCode::Pass,
    ReasonCode::PingExecError,
    ReasonCode::PingGatewayUnreachable,
    ReasonCode::PingOk,
    ReasonCode::PingSubnetUnreachable,
    ReasonCode::PingTimeout,
    ReasonCode::PingUnreachable,
    ReasonCode::RateWindowCoverageLow,
    ReasonCode::ResourceCleanupFailed,
    ReasonCode::ResumeFreshPass,
    ReasonCode::RxBelowTarget,
    ReasonCode::RxDropout,
    ReasonCode::RxOutage,
    ReasonCode::RxP10BelowTarget,
    ReasonCode::RxTargetMet,
    ReasonCode::RxUnstable,
    ReasonCode::SampleCoverageLow,
    ReasonCode::SingleUdpStreamFailed,
    ReasonCode::TargetMissing,
    ReasonCode::TargetUnknown,
    ReasonCode::UdpGroupDispatchError,
    ReasonCode::UdpLossDataMissing,
    ReasonCode::UdpLossHigh,
    ReasonCode::CtsInternalNoAttempt,
    ReasonCode::CtsServerExitedEarly,
    ReasonCode::CtsServerJobIdMismatch,
    ReasonCode::CtsServerStartFailed,
    ReasonCode::CtsServerStatusFailed,
    ReasonCode::UnitDirectionResultMissing,
    ReasonCode::UnitPanic,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// 字符串表示必须与历史报告一字不差，且能原样解回来。
    #[test]
    fn every_code_round_trips_through_its_string_form() {
        for code in ALL_REASON_CODES {
            let text = code.as_str();
            assert!(!text.is_empty(), "{code:?} 的字符串表示不能为空");
            assert!(
                text.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "{text} 不是大写下划线形式"
            );
            assert_eq!(text.parse::<ReasonCode>(), Ok(code), "{text} 解不回来");
        }
        assert_eq!("".parse::<ReasonCode>(), Ok(ReasonCode::None));
        assert_eq!("NOT_A_REAL_CODE".parse::<ReasonCode>(), Err(()));
    }

    /// 两个码不能映射到同一个字符串，否则报告里就分不开了。
    #[test]
    fn no_two_codes_share_a_string() {
        let mut seen = std::collections::HashSet::new();
        for code in ALL_REASON_CODES {
            assert!(seen.insert(code.as_str()), "{} 重复", code.as_str());
        }
    }

    /// 报告里码和明细同处一串文本，取码只认开头那段大写前缀。
    #[test]
    fn the_code_is_read_from_the_head_of_the_reason_text() {
        assert_eq!(
            ReasonCode::parse_prefix("RX_BELOW_TARGET: RX 平均 808 Mbps < 目标 2000 Mbps"),
            ReasonCode::RxBelowTarget
        );
        // 明细里自带冒号，不能被当成分隔符再切一次。
        assert_eq!(
            ReasonCode::parse_prefix("PING_OK: 丢包率 0.0%，RTT 最小/平均: 1/2 ms"),
            ReasonCode::PingOk
        );
        assert_eq!(ReasonCode::parse_prefix("没有码的一句话"), ReasonCode::None);
        assert_eq!(ReasonCode::parse_prefix(""), ReasonCode::None);
    }
}
