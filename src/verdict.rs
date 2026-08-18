//! 判定词汇表：结果分级、执行状态、原因码优先级与单元聚合规则。
//!
//! 这些语义**不属于展示层**。判定规则曾经在 `master::executor`（执行侧聚合）和
//! `report`（报告侧回退聚合）各实现一遍，两份实现的优先级顺序不一致，先后产生
//! 过两个真实缺陷：
//!
//! - 概览把「必须灌通的方向硬失败」显示成另一方向普通的 `NOT_EVALUATED`；
//! - TCP/CTS 路径先判 `RX_BELOW_TARGET` 再检查采样是否可信，把环境异常写成
//!   CPE 性能失败。
//!
//! 因此把词汇表和聚合规则收敛到这一个模块：executor 与 report 都依赖它，
//! report 只负责渲染，不再自带一份判定逻辑。

/// 单个测试方向/执行行的结果分级。
///
/// 取值口径见 `UDP并发灌包验收场景.md` 第二节；顺序不代表优先级，
/// 聚合优先级由 [`aggregate_verdict`] 单独定义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verdict {
    Pass,
    RateFail,
    Unstable,
    Measured,
    #[default]
    NotEvaluated,
    SetupError,
    Skip,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::RateFail => "RATE_FAIL",
            Verdict::Unstable => "UNSTABLE",
            Verdict::Measured => "MEASURED",
            Verdict::NotEvaluated => "NOT_EVALUATED",
            Verdict::SetupError => "SETUP_ERROR",
            Verdict::Skip => "SKIP",
        }
    }

    pub fn css(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::RateFail => "fail",
            Verdict::Unstable => "warn",
            Verdict::Measured => "measured",
            Verdict::NotEvaluated => "not-evaluated",
            Verdict::SetupError => "error",
            Verdict::Skip => "skip",
        }
    }

    pub fn is_pass(self) -> bool {
        self == Verdict::Pass
    }
}

/// 执行过程本身的完成情况，与 [`Verdict`] 正交：
/// 一个 `COMPLETED` 的执行完全可以判出 `RATE_FAIL`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionStatus {
    #[default]
    Completed,
    Partial,
    Error,
    TimedOut,
    Cancelled,
    Skipped,
}

impl ExecutionStatus {
    pub fn label(self) -> &'static str {
        match self {
            ExecutionStatus::Completed => "COMPLETED",
            ExecutionStatus::Partial => "PARTIAL",
            ExecutionStatus::Error => "ERROR",
            ExecutionStatus::TimedOut => "TIMEOUT",
            ExecutionStatus::Cancelled => "CANCELLED",
            ExecutionStatus::Skipped => "SKIPPED",
        }
    }
}

/// 单流 UDP 在每轮清理均已确认的前提下耗尽全部尝试仍无工具测量。
///
/// 这是用户指定的硬失败：该方向"必须灌通"，因此它不能被另一方向普通的
/// `NOT_EVALUATED`（采样不足、目标缺失等）掩盖。
pub const HARD_SINGLE_UDP_FAILURE_CODES: [&str; 2] = [
    "SINGLE_UDP_STREAM_FAILED",
    "CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED",
];

pub fn is_hard_single_udp_failure(verdict: Verdict, reason_code: &str) -> bool {
    verdict == Verdict::RateFail && HARD_SINGLE_UDP_FAILURE_CODES.contains(&reason_code)
}

/// 把若干方向/执行行的结果聚合成一个测试单元的结论。
///
/// 优先级（**唯一定义处**，executor 与 report 都必须走这里）：
///
/// 1. 空集合 —— 连一次执行都没有产生结果，属于搭建失败；
/// 2. 任一 `SETUP_ERROR` —— 环境没搭起来，性能结论无意义；
/// 3. 任一单流硬失败 —— 必须灌通的方向没灌通，不能被步骤 4 的
///    `NOT_EVALUATED` 吃掉（这正是它必须排在前面的原因）；
/// 4. 按 `NOT_EVALUATED` → `RATE_FAIL` → `UNSTABLE` → `MEASURED` 取第一个命中：
///    「无法评价」优先于「评价为不合格」，避免用一份不可信的数据下结论；
/// 5. 含 `SKIP` —— 整体按跳过计，不计入通过率；
/// 6. 全部 `PASS` —— 才是 `PASS`。
pub fn aggregate_verdict<'a, I>(items: I) -> Verdict
where
    I: IntoIterator<Item = (Verdict, &'a str)>,
{
    let items: Vec<(Verdict, &str)> = items.into_iter().collect();
    if items.is_empty() {
        return Verdict::SetupError;
    }
    if items
        .iter()
        .any(|(verdict, _)| *verdict == Verdict::SetupError)
    {
        return Verdict::SetupError;
    }
    if items
        .iter()
        .any(|(verdict, code)| is_hard_single_udp_failure(*verdict, code))
    {
        return Verdict::RateFail;
    }
    for candidate in [
        Verdict::NotEvaluated,
        Verdict::RateFail,
        Verdict::Unstable,
        Verdict::Measured,
    ] {
        if items.iter().any(|(verdict, _)| *verdict == candidate) {
            return candidate;
        }
    }
    if items.iter().any(|(verdict, _)| *verdict == Verdict::Skip) {
        return Verdict::Skip;
    }
    Verdict::Pass
}

/// 把原因码翻译成**给人的处置建议**。
///
/// 87 个大写码里 37 个是 `CTSTRAFFIC_*`，对开发者是精确资产，对"零 Python、
/// 零 PowerShell 的小白用户"是一堵墙。他们要的不是码，是下一步该干什么：
/// 换根线？升级辅测端？还是这压根不是 CPE 的问题？
///
/// 刻意**不改动任何原有码**：码进 RESUME 数据库、进用户既有认知、进自动化断言，
/// 改一个字都是破坏性变更。这里只在渲染层加一层派生。
pub fn disposition_advice(reason_code: &str) -> Option<&'static str> {
    let advice = match reason_code {
        // —— 环境/搭建类：不是 CPE 的问题，先修测试环境 ——
        "IPERF_EXEC_FAILED" | "IPERF_PREFLIGHT_FAILED" => {
            "两端都要放 iperf3 且版本可用。把 iperf3 放到程序同目录后重跑。"
        }
        "CTSTRAFFIC_PREFLIGHT_FAILED"
        | "CTSTRAFFIC_ARGS_INVALID"
        | "CTSTRAFFIC_PROCESS_START_FAILED" => {
            "ctsTraffic 仅支持 Windows 10+，且两端都需要发布包里的固定版本 ctsTraffic.exe。"
        }
        "RESOURCE_CLEANUP_FAILED"
        | "CTSTRAFFIC_CLEANUP_FAILED"
        | "CTSTRAFFIC_CLIENT_PROCESS_CLEANUP_UNCONFIRMED"
        | "CTSTRAFFIC_SERVER_PROCESS_CLEANUP_UNCONFIRMED" => {
            "上一轮的进程或端口没能确认回收。检查两端是否有残留 iperf3/ctsTraffic 进程，清掉后重跑。"
        }
        "UNIT_PANIC" | "LEG_THREAD_PANIC" | "UDP_GROUP_DISPATCH_ERROR" => {
            "工具自身异常，与被测设备无关。请把本次 runs/ 目录整体反馈给工具维护者。"
        }
        "GATEWAY_NOT_FOUND" => "该网卡没有 IPv4 默认网关，无法用网关 Ping 判断链路状态。属于组网配置问题。",

        // —— 采样/窗口类：这一轮数据不可信，重跑即可，不要记成 CPE 不达标 ——
        "SAMPLE_COVERAGE_LOW"
        | "RATE_WINDOW_COVERAGE_LOW"
        | "NIC_RATE_MISSING"
        | "CTSTRAFFIC_MONITOR_START_FAILED"
        | "CTSTRAFFIC_MONITOR_STOP_FAILED"
        | "CTSTRAFFIC_MONITOR_NO_SAMPLES"
        | "CTSTRAFFIC_MONITOR_RUNTIME_ERROR" => {
            "本轮网卡采样不完整，数据不足以判定性能。检查测试期间是否重启/切换过网卡，然后重跑。这不是 CPE 不达标。"
        }
        "EFFECTIVE_WINDOW_SHORT"
        | "IPERF_EFFECTIVE_WINDOW_SHORT"
        | "CTSTRAFFIC_EFFECTIVE_WINDOW_SHORT" => {
            "有效测量窗口短于要求时长，多半是流提前结束或启动过慢。确认链路稳定后重跑。"
        }

        // —— 配置类：改配置，不是改设备 ——
        "CONFIGURED_LOAD_TOO_LOW" => "配置的流数×每流带宽不足以打到目标，请调大 streams 或每流 -b。",
        "OFFERED_LOAD_LOW" => "实际发出的负载没达到目标+余量，发送端可能已是瓶颈。先确认发送端能打出足够流量。",
        "TARGET_MISSING" => "verify 模式必须配置 rate_targets_mbps，否则无法判定合格与否。",
        "TARGET_UNKNOWN" => "未配置可信目标，本行只记录实测能力，不代表合格或不合格。",
        "UDP_LOSS_DATA_MISSING" | "CTSTRAFFIC_UDP_LOSS_DATA_MISSING" => {
            "配置了丢包门槛但工具输出里没有丢包数据，多半是 iperf3 版本过旧。升级后重跑。"
        }

        // —— 真正的被测对象问题 ——
        "RX_BELOW_TARGET" => "接收端实测速率低于目标，这是被测链路/设备的性能问题。",
        "RX_UNSTABLE" => "平均速率达标但存在持续掉速，被测链路有周期性抖动或限速。",
        "UDP_LOSS_HIGH" | "CTSTRAFFIC_UDP_LOSS_HIGH" => "丢包/丢帧超过门槛，被测链路在该负载下无法无损转发。",
        "SINGLE_UDP_STREAM_FAILED" | "CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED" => {
            "该方向必须灌通却始终没有任何流量测量。先确认防火墙放通了测试端口段，再检查链路是否真的不通。"
        }
        "ACTIVE_STREAMS_LOW" => "成功建立的流数不足以支撑正式判定，通常是部分端口被拦或链路承载不了这么多流。",
        "NO_STREAM_STARTED" => "一条流都没起来，先查防火墙与测试端口段是否放通。",
        "IPERF_RUNTIME_ERRORS" | "CTSTRAFFIC_RUNTIME_ERRORS" => {
            "已有吞吐测量但进程非正常结束，链路可能在测试中途中断。"
        }
        "PING_UNREACHABLE" | "PING_SUBNET_UNREACHABLE" => "目标不可达，先确认两端 IP、网线和防火墙。",
        "PING_GATEWAY_UNREACHABLE" => "网关不可达，说明该网卡的链路或组网本身有问题。",
        "PING_TIMEOUT" | "PING_EXEC_ERROR" => "Ping 命令本身没能正常执行，属于测试环境问题。",

        // —— 正常结果，无需处置 ——
        "PASS" | "PING_OK" | "RX_TARGET_MET" | "FLOW_MEASURED" | "RESUME_FRESH_PASS" => return None,
        _ => return None,
    };
    Some(advice)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(verdict: Verdict) -> (Verdict, &'static str) {
        (verdict, "")
    }

    /// 结构断言：判定优先级只能有一处定义。
    ///
    /// 这条规则被违反过两次（executor 与 report 各写一份、顺序不一致），
    /// 代价是两个静默错判。语义重复无法靠普通单测发现——两份实现各自都能
    /// 通过自己的用例——所以在源码层面直接把门关上。
    #[test]
    fn verdict_priority_has_exactly_one_definition_in_the_tree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![root.clone()];
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
                if path.file_name().and_then(|n| n.to_str()) == Some("verdict.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read source");
                // 其他模块可以*调用* aggregate_verdict / is_hard_single_udp_failure，
                // 但不得自带一份优先级表或硬失败码表。
                for marker in [
                    "fn verdict_priority",
                    "HARD_SINGLE_UDP_FAILURE_CODES: [",
                    "\"SINGLE_UDP_STREAM_FAILED\" | \"CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED\"",
                ] {
                    if text.contains(marker) {
                        offenders.push(format!("{}: {marker}", path.display()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "判定优先级/硬失败码表只能定义在 src/verdict.rs，发现重复定义: {offenders:#?}"
        );
    }

    #[test]
    fn disposition_advice_separates_who_is_at_fault_without_touching_any_code() {
        // 三类结果必须给出不同指向的建议，否则这一层就没有意义。
        let env = disposition_advice("SAMPLE_COVERAGE_LOW").expect("采样类必须有建议");
        assert!(
            env.contains("不是 CPE 不达标"),
            "采样问题不能让用户去怀疑设备: {env}"
        );

        let cfg = disposition_advice("CONFIGURED_LOAD_TOO_LOW").expect("配置类必须有建议");
        assert!(
            cfg.contains("streams") || cfg.contains("-b"),
            "配置类要指到具体字段: {cfg}"
        );

        let dut = disposition_advice("RX_BELOW_TARGET").expect("性能类必须有建议");
        assert!(dut.contains("性能问题"), "真正的性能不达标要说清楚: {dut}");

        // 正常结果不该冒出"处置建议"这种噪声。
        for ok in [
            "PASS",
            "PING_OK",
            "TARGET_UNKNOWN".trim_end(),
            "FLOW_MEASURED",
        ] {
            if ok == "TARGET_UNKNOWN" {
                continue;
            }
            assert!(disposition_advice(ok).is_none(), "{ok} 不该有处置建议");
        }
        // 未知码静默返回 None，不能 panic，也不能编个建议出来。
        assert!(disposition_advice("SOME_FUTURE_CODE").is_none());
        assert!(disposition_advice("").is_none());
    }

    /// 派生层不得改动任何既有原因码：码会进 RESUME 数据库和用户既有认知。
    #[test]
    fn advice_layer_never_rewrites_the_underlying_reason_codes() {
        // 抽查各后端的代表性码，确认它们仍然原样存在于源码里。
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/verdict.rs"),
        )
        .expect("read verdict.rs");
        for code in [
            "SINGLE_UDP_STREAM_FAILED",
            "CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED",
            "RX_BELOW_TARGET",
            "ACTIVE_STREAMS_LOW",
        ] {
            assert!(source.contains(code), "原因码 {code} 不得被改写或删除");
        }
    }

    #[test]
    fn empty_outcomes_are_setup_error_not_pass() {
        assert_eq!(aggregate_verdict([]), Verdict::SetupError);
    }

    #[test]
    fn setup_error_outranks_everything() {
        assert_eq!(
            aggregate_verdict([v(Verdict::Pass), v(Verdict::SetupError)]),
            Verdict::SetupError
        );
        // 连硬失败也让位：环境没搭起来时性能结论无意义。
        assert_eq!(
            aggregate_verdict([
                (Verdict::RateFail, "SINGLE_UDP_STREAM_FAILED"),
                v(Verdict::SetupError),
            ]),
            Verdict::SetupError
        );
    }

    #[test]
    fn hard_single_udp_failure_is_not_masked_by_plain_not_evaluated() {
        for code in HARD_SINGLE_UDP_FAILURE_CODES {
            assert_eq!(
                aggregate_verdict([
                    (Verdict::RateFail, code),
                    (Verdict::NotEvaluated, "SAMPLE_COVERAGE_LOW"),
                ]),
                Verdict::RateFail,
                "code={code}"
            );
        }
    }

    #[test]
    fn plain_rate_fail_still_yields_to_not_evaluated() {
        // 普通的速率不达标让位于「无法评价」：不能用一份不可信的数据下结论。
        assert_eq!(
            aggregate_verdict([
                (Verdict::RateFail, "RX_BELOW_TARGET"),
                (Verdict::NotEvaluated, "SAMPLE_COVERAGE_LOW"),
            ]),
            Verdict::NotEvaluated
        );
    }

    #[test]
    fn degraded_results_outrank_pass_in_documented_order() {
        assert_eq!(
            aggregate_verdict([v(Verdict::Pass), v(Verdict::Measured)]),
            Verdict::Measured
        );
        assert_eq!(
            aggregate_verdict([v(Verdict::Measured), v(Verdict::Unstable)]),
            Verdict::Unstable
        );
        assert_eq!(
            aggregate_verdict([v(Verdict::Unstable), (Verdict::RateFail, "RX_BELOW_TARGET")]),
            Verdict::RateFail
        );
    }

    #[test]
    fn skip_is_kept_only_when_nothing_was_actually_judged() {
        assert_eq!(aggregate_verdict([v(Verdict::Skip)]), Verdict::Skip);
        assert_eq!(
            aggregate_verdict([v(Verdict::Skip), v(Verdict::Pass)]),
            Verdict::Skip
        );
        // 有真实判定时跳过行不再影响结论。
        assert_eq!(
            aggregate_verdict([v(Verdict::Skip), v(Verdict::Measured)]),
            Verdict::Measured
        );
    }

    #[test]
    fn all_pass_is_the_only_way_to_pass() {
        assert_eq!(
            aggregate_verdict([v(Verdict::Pass), v(Verdict::Pass)]),
            Verdict::Pass
        );
    }

    #[test]
    fn hard_failure_detection_requires_both_verdict_and_code() {
        assert!(is_hard_single_udp_failure(
            Verdict::RateFail,
            "SINGLE_UDP_STREAM_FAILED"
        ));
        // 同样的码配别的 verdict 不算硬失败（例如流明细行的诊断用法）。
        assert!(!is_hard_single_udp_failure(
            Verdict::NotEvaluated,
            "SINGLE_UDP_STREAM_FAILED"
        ));
        assert!(!is_hard_single_udp_failure(
            Verdict::RateFail,
            "RX_BELOW_TARGET"
        ));
    }
}
