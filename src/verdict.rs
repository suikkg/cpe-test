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

use crate::reason::ReasonCode;

/// 单个测试方向/执行行的结果分级。
///
/// 取值口径见 `UDP并发灌包验收场景.md` 第二节；顺序不代表优先级，
/// 聚合优先级由 [`aggregate_verdict`] 单独定义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verdict {
    Pass,
    RateFail,
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

/// 一次判定的完整结论：**判什么 + 为什么 + 说给人听的那句话**。
///
/// 这三样此前是一个 `(Verdict, ReasonCode, String)` 裸元组，在七八个函数之间
/// 按位置传递。位置传参在这里特别危险：三个字段里有两个是判定语义，写反了
/// 编译器不会拦，报告上却会变成「原因码和明细对不上」——报告层为此专门有一
/// 层 `metric_reason_mismatch` 兜底，兜的正是这个错。
///
/// 它只描述**结论**，不含任何测量值和执行状态：产出它的函数因此可以是纯函数，
/// 拿一份「已经确定的事实」就能单测，不需要起进程、连对端。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerdictResult {
    pub verdict: Verdict,
    pub code: ReasonCode,
    pub detail: String,
}

impl VerdictResult {
    pub fn new(verdict: Verdict, code: ReasonCode, detail: impl Into<String>) -> Self {
        Self {
            verdict,
            code,
            detail: detail.into(),
        }
    }

    /// 通过。通过不需要原因码，也不需要解释。
    pub fn pass() -> Self {
        Self {
            verdict: Verdict::Pass,
            code: ReasonCode::None,
            detail: String::new(),
        }
    }

    pub fn rate_fail(code: ReasonCode, detail: impl Into<String>) -> Self {
        Self::new(Verdict::RateFail, code, detail)
    }

    pub fn not_evaluated(code: ReasonCode, detail: impl Into<String>) -> Self {
        Self::new(Verdict::NotEvaluated, code, detail)
    }

    pub fn measured(code: ReasonCode, detail: impl Into<String>) -> Self {
        Self::new(Verdict::Measured, code, detail)
    }

    pub fn setup_error(code: ReasonCode, detail: impl Into<String>) -> Self {
        Self::new(Verdict::SetupError, code, detail)
    }
}

/// 单流 UDP 在每轮清理均已确认的前提下耗尽全部尝试仍无工具测量。
///
/// 这是用户指定的硬失败：该方向"必须灌通"，因此它不能被另一方向普通的
/// `NOT_EVALUATED`（采样不足、目标缺失等）掩盖。
pub const HARD_SINGLE_UDP_FAILURE_CODES: [ReasonCode; 2] = [
    ReasonCode::SingleUdpStreamFailed,
    ReasonCode::CtsSingleUdpStreamFailed,
];

pub fn is_hard_single_udp_failure(verdict: Verdict, reason_code: ReasonCode) -> bool {
    verdict == Verdict::RateFail && HARD_SINGLE_UDP_FAILURE_CODES.contains(&reason_code)
}

/// 只说明**这条腿自己**判定前提不成立的 `NOT_EVALUATED`。
///
/// 它们不该盖住同一单元里另一条腿确凿的 `RATE_FAIL`：负载没配够、缺目标，
/// 都是这条腿的配置问题，不让另一条腿测出来的数变得可疑。盖住的后果是一个
/// 真实的不达标从概览里消失——双向单元里 B→A 明明判了 FAIL，单元却显示
/// 「无法评价」。
///
/// 反过来，采样/时间轴类的判不了（覆盖率不足、计数器停滞、有效窗口太短、
/// 网卡消失）**必须**继续盖住：双向的两条腿跑在同一段时间窗里，那段时间的
/// 采样塌了，另一条腿的数同样不可信，拿它判 FAIL 就是把环境异常写成 CPE
/// 性能失败——正是这套判定一直在防的误判方向。
///
/// 名单只放"确定安全"的三个，其余一律按老行为盖住：这里放宽一个码，
/// 对应的就是一批历史上判「无法评价」的单元变成 `RATE_FAIL`。
pub const LEG_LOCAL_NOT_EVALUATED_CODES: [ReasonCode; 3] = [
    ReasonCode::ConfiguredLoadTooLow,
    ReasonCode::OfferedLoadLow,
    ReasonCode::TargetMissing,
];

/// 这一条 `NOT_EVALUATED` 会不会盖住别的腿的结论。
fn blocks_other_legs(verdict: Verdict, reason_code: ReasonCode) -> bool {
    verdict == Verdict::NotEvaluated && !LEG_LOCAL_NOT_EVALUATED_CODES.contains(&reason_code)
}

/// 把若干方向/执行行的结果聚合成一个测试单元的结论。
///
/// 优先级（**唯一定义处**，executor 与 report 都必须走这里）：
///
/// 1. 空集合 —— 连一次执行都没有产生结果，属于搭建失败；
/// 2. 任一 `SETUP_ERROR` —— 环境没搭起来，性能结论无意义；
/// 3. 任一单流硬失败 —— 必须灌通的方向没灌通，不能被步骤 4 的
///    `NOT_EVALUATED` 吃掉（这正是它必须排在前面的原因）；
/// 4. 任一**会盖住别的腿**的 `NOT_EVALUATED`（采样/时间轴不可信，见
///    [`blocks_other_legs`]）—— 数据不可信时不拿它下任何结论；
/// 5. 任一 `RATE_FAIL` —— 到这里剩下的判不了都只是那条腿自己的配置问题
///    （负载没配够、缺目标），盖住另一条腿确凿的不达标就是丢结论；
/// 6. 任一 `NOT_EVALUATED`（腿内局部的那几种）；
/// 7. 任一 `MEASURED`；
/// 8. 含 `SKIP` —— 整体按跳过计，不计入通过率；
/// 9. 全部 `PASS` —— 才是 `PASS`。
pub fn aggregate_verdict<I>(items: I) -> Verdict
where
    I: IntoIterator<Item = (Verdict, ReasonCode)>,
{
    let items: Vec<(Verdict, ReasonCode)> = items.into_iter().collect();
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
        .any(|(verdict, code)| is_hard_single_udp_failure(*verdict, *code))
    {
        return Verdict::RateFail;
    }
    if items
        .iter()
        .any(|(verdict, code)| blocks_other_legs(*verdict, *code))
    {
        return Verdict::NotEvaluated;
    }
    for candidate in [Verdict::RateFail, Verdict::NotEvaluated, Verdict::Measured] {
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
pub fn disposition_advice(reason_code: ReasonCode) -> Option<&'static str> {
    let advice = match reason_code {
        // —— 环境/搭建类：不是 CPE 的问题，先修测试环境 ——
        ReasonCode::IperfExecFailed | ReasonCode::IperfPreflightFailed => {
            "windows未设置iperf3环境变量；两端都要放 iperf3 且版本可用。把 iperf3 放到程序同目录后重跑。"
        }
        ReasonCode::CtsPreflightFailed
        | ReasonCode::CtsArgsInvalid
        | ReasonCode::CtsProcessStartFailed => {
            "ctsTraffic 仅支持 Windows 10+，且两端都需要发布包里的固定版本 ctsTraffic.exe。"
        }
        ReasonCode::ResourceCleanupFailed
        | ReasonCode::CtsCleanupFailed
        | ReasonCode::CtsClientProcessCleanupUnconfirmed
        | ReasonCode::CtsServerProcessCleanupUnconfirmed => {
            "上一轮的进程或端口没能确认回收。检查两端是否有残留 iperf3/ctsTraffic 进程，清掉后重跑。"
        }
        ReasonCode::UnitPanic | ReasonCode::LegThreadPanic | ReasonCode::UdpGroupDispatchError => {
            "工具自身异常，与被测设备无关。请把本次 runs/ 目录整体反馈给工具维护者。"
        }
        ReasonCode::GatewayNotFound => "该网卡没有 IPv4 默认网关，无法用网关 Ping 判断链路状态。属于组网配置问题。",

        // —— 采样/窗口类：这一轮数据不可信，重跑即可，不要记成速率不达标 ——
        ReasonCode::SampleCoverageLow
        | ReasonCode::RateWindowCoverageLow
        | ReasonCode::NicRateMissing
        | ReasonCode::CtsMonitorStartFailed
        | ReasonCode::CtsMonitorStopFailed
        | ReasonCode::CtsMonitorNoSamples
        | ReasonCode::CtsMonitorRuntimeError => {
            "本轮网卡采样不完整，数据不足以判定性能。检查测试期间是否重启/切换过网卡，然后重跑。这不是速率不达标。"
        }
        ReasonCode::CounterStalled => {
            "样本采齐了但网卡字节计数长时间不动，说明测试中途链路已经没有流量。先确认被测设备是否掉线或重启，再重跑；这一轮的平均速率不能当结论。"
        }
        ReasonCode::EffectiveWindowShort
        | ReasonCode::IperfEffectiveWindowShort
        | ReasonCode::CtsEffectiveWindowShort => {
            "有效测量窗口短于要求时长，多半是流提前结束或启动过慢。确认链路稳定后重跑。"
        }

        // —— 配置类：改配置，不是改设备 ——
        ReasonCode::ConfiguredLoadTooLow => "配置的流数×每流带宽不足以打到目标，请调大 streams 或每流 -b。",
        ReasonCode::OfferedLoadLow => "实际发出的负载没达到目标+余量，发送端可能已是瓶颈。先确认发送端能打出足够流量。",
        ReasonCode::TargetMissing => "verify 模式必须配置 rate_targets_mbps，否则无法判定合格与否。",
        ReasonCode::TargetUnknown => "未配置可信目标，本行只记录实测能力，不代表PASS或FAIL。",
        ReasonCode::UdpLossDataMissing | ReasonCode::CtsUdpLossDataMissing => {
            "配置了丢包门槛但工具输出里没有丢包数据，多半是 iperf3 版本过旧。升级后重跑。"
        }

        // —— 真正的被测对象问题 ——
        ReasonCode::RxBelowTarget => "接收端实测速率低于目标，请检查被测链路/设备的性能问题。",
        ReasonCode::RxUnstable => "平均速率达标但存在持续掉速，被测链路有周期性抖动或限速。",
        ReasonCode::RxOutage => {
            "平均速率达标，但判定窗口里有连续 5 秒以上灌包速率基本为 0——那几秒链路是真的断的。\
             按原因里写的起点和连续秒数，去网卡逐样本 CSV 对同一时刻发生了什么（漫游、信道切换、\
             对端重启、链路 down/up）。这个秒数是在原始逐样本序列上量出来的，可以直接和 iperf \
             截图的同一时刻对上。"
        }
        ReasonCode::RxDropout => {
            "平均速率达标，但判定窗口里有连续 5 秒以上掉到门限的 80% 以下。没断，但业务上那几秒\
             明显不够用：按原因里写的起点和连续秒数，去网卡逐样本 CSV 对同一时刻发生了什么。\
             不够 5 秒的单点抖动不会判到这里——它和 Wi-Fi 发 probe、信道扫描造成的掉一拍在\
             网卡计数器上不可区分。"
        }
        ReasonCode::RxP10BelowTarget => {
            "接收端速率的低十分位低于目标：不是偶发掉坑，是有相当一部分时间都没达标。按被测链路性能问题处理。"
        }
        // —— ctsTraffic 生命周期没确认：不是被测设备的问题，是这一轮没跑成 ——
        ReasonCode::CtsClientStartFailed
        | ReasonCode::CtsServerStartFailed
        | ReasonCode::CtsClientStatusFailed
        | ReasonCode::CtsServerStatusFailed
        | ReasonCode::CtsClientStopFailed
        | ReasonCode::CtsServerStopFailed
        | ReasonCode::CtsClientJobIdMismatch
        | ReasonCode::CtsServerJobIdMismatch
        | ReasonCode::CtsClientWaitInvalid
        | ReasonCode::CtsClientResultMissing
        | ReasonCode::CtsClientProcessNotStarted
        | ReasonCode::CtsServerProcessNotStarted
        | ReasonCode::CtsServerExitedEarly
        | ReasonCode::CtsServerFailed
        | ReasonCode::CtsProcessControlFailed => {
            "ctsTraffic 作业的启动/查询/停止没有得到确认，这一轮没有可信的执行过程。\
             先确认辅测端还在线、ctsTraffic.exe 可执行、测试端口段没被防火墙拦，再重跑。\
             不要把它当成被测设备不达标。"
        }
        // —— 被取消：人为中止或上层撤单，不是失败 ——
        ReasonCode::CtsClientCancelled
        | ReasonCode::CtsClientUserCancelled
        | ReasonCode::CtsServerCancelled
        | ReasonCode::CtsClientAborted => {
            "这一轮被取消了（手动停止或上层撤单），没有产生可判定的数据。重跑即可。"
        }
        // —— 内部错误：程序自身的问题，报 issue 比重跑有用 ——
        ReasonCode::CtsInternalNoAttempt | ReasonCode::UnitDirectionResultMissing => {
            "程序内部状态异常：该跑的尝试一次都没记录下来。这是工具自身的缺陷，\
             请连同本次 run 目录一起反馈，重跑多半会复现。"
        }
        // —— 单条流没跑通：这一条流的事，别的流的结论仍然作数 ——
        ReasonCode::FlowFailed => {
            "这一条流没跑通。先看同一腿其余流的结果：多数流正常说明是偶发，\
             全部失败才需要怀疑链路或端口。"
        }

        ReasonCode::UdpLossHigh | ReasonCode::CtsUdpLossHigh => "丢包/丢帧超过门槛，被测链路在该负载下无法无损转发。",
        ReasonCode::SingleUdpStreamFailed | ReasonCode::CtsSingleUdpStreamFailed => {
            "该方向必须灌通却始终没有任何流量测量。先确认防火墙放通了测试端口段，再检查链路是否真的不通。"
        }
        ReasonCode::ActiveStreamsLow => "成功建立的流数不足以支撑正式判定，通常是部分端口被拦或链路承载不了这么多流。",
        ReasonCode::NoStreamStarted => "一条流都没起来，先查防火墙与测试端口段是否放通；请检查被测链路/设备的是否存在问题",
        ReasonCode::IperfRuntimeErrors | ReasonCode::CtsRuntimeErrors => {
            "已有吞吐测量但进程非正常结束，链路可能在测试中途中断。"
        }
        ReasonCode::IperfSummaryLost => {
            "灌包已经跑完，但 iperf3 收尾交换结果时连接断了，工具自报速率取不到。\
             判定已改用接收端网卡口径，这一行的结论仍然有效；工具自报那几列是空的属正常。"
        }
        ReasonCode::NoValidMeasurement | ReasonCode::CtsNoMeasurement => {
            "整轮没有产生任何可用的吞吐测量。先确认防火墙放通了测试端口段、两端工具版本可用，再重跑。"
        }
        ReasonCode::NicDisappeared => {
            "测试期间接收端网卡从系统里消失了，请检查被测链路/设备的是否存在问题；恢复网卡后重跑。"
        }
        ReasonCode::PingUnreachable | ReasonCode::PingSubnetUnreachable => "目标不可达，先确认两端 IP、网线和防火墙；请检查被测链路/设备的是否存在问题",
        ReasonCode::PingGatewayUnreachable => "网关不可达，说明该网卡的链路或组网本身有问题。",
        ReasonCode::PingTimeout | ReasonCode::PingExecError => "Ping 命令本身没能正常执行，属于测试环境问题。",

        // —— 正常结果，无需处置 ——
        ReasonCode::Pass | ReasonCode::PingOk | ReasonCode::RxTargetMet | ReasonCode::FlowMeasured | ReasonCode::ResumeFreshPass => return None,
        _ => return None,
    };
    Some(advice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reason::ALL_REASON_CODES;

    /// 速率判定新增的原因码，必须同时在**消费侧**登记。
    ///
    /// `disposition_advice` 的兜底是 `_ => return None`，漏登记永远不会有信号：
    /// v4.3.1 加的 `RX_DROPOUT` 就这么在报告里当了一整个版本的「无建议」，
    /// 而它取代的 `RX_UNSTABLE` 两处都有。
    ///
    /// 这条把 `rate_window.rs` 整个文件在**编译期**读进来，把里面出现的
    /// 大写原因码literal 全捞出来逐个核对。选它是因为速率判定的码全在这一个
    /// 文件里产出，而且它干净——没有环境变量名之类的同形噪声，不需要维护
    /// 一张越滚越大的豁免表。以后在别处新增码时，照这个样子再加一条。
    /// **每一个**原因码都必须有处置建议，或者被显式豁免。
    ///
    /// `disposition_advice` 的兜底是 `_ => return None`，漏登记永远不会有信号：
    /// v4.3.1 加的 `RX_DROPOUT` 就这么在报告里当了一整个版本的「无建议」。
    ///
    /// 老版本靠扫 `rate_window.rs` 的大写字面量来近似这件事——只覆盖得到那
    /// 一个文件，而且码一换成 enum 就什么都扫不到了。现在直接对
    /// [`ALL_REASON_CODES`] 穷举：新增一个码却忘了写建议，这条就红。
    #[test]
    fn every_reason_code_has_a_disposition() {
        // 正常结果不需要处置建议，但必须**显式**列出来，不能靠兜底静默通过。
        const NEEDS_NO_ADVICE: [ReasonCode; 5] = [
            ReasonCode::Pass,
            ReasonCode::PingOk,
            ReasonCode::RxTargetMet,
            ReasonCode::FlowMeasured,
            ReasonCode::ResumeFreshPass,
        ];

        let missing: Vec<&str> = ALL_REASON_CODES
            .iter()
            .filter(|code| !NEEDS_NO_ADVICE.contains(code))
            .filter(|code| disposition_advice(**code).is_none())
            .map(|code| code.as_str())
            .collect();
        assert!(
            missing.is_empty(),
            "这些原因码没有处置建议，报告里会是一片空白：{missing:?}。\
             在 disposition_advice 里补上，或显式加进 NEEDS_NO_ADVICE"
        );

        // 反过来：豁免名单里的码不该有建议，否则「正常结果」也会冒出噪声。
        for code in NEEDS_NO_ADVICE {
            assert!(
                disposition_advice(code).is_none(),
                "{code} 是正常结果，不该有处置建议"
            );
        }
        // 没有码时静默返回 None，不能 panic，也不能编一条出来。
        assert!(disposition_advice(ReasonCode::None).is_none());
    }

    /// RX_DROPOUT 是 v4.3.1 的头牌能力，两张消费侧的表都要认它。
    #[test]
    fn the_dropout_code_is_registered_in_both_consumer_tables() {
        let advice = disposition_advice(ReasonCode::RxDropout).expect("必须有处置建议");
        assert!(advice.contains("5 秒"), "建议要说清它判的是什么：{advice}");
        assert!(
            disposition_advice(ReasonCode::RxUnstable).is_some(),
            "它取代的那个码不能因此掉队"
        );
    }

    fn v(verdict: Verdict) -> (Verdict, ReasonCode) {
        (verdict, ReasonCode::None)
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
                    "LEG_LOCAL_NOT_EVALUATED_CODES: [",
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
        let env = disposition_advice(ReasonCode::SampleCoverageLow).expect("采样类必须有建议");
        assert!(
            env.contains("不是速率不达标"),
            "采样问题不能让用户去怀疑设备: {env}"
        );

        let cfg = disposition_advice(ReasonCode::ConfiguredLoadTooLow).expect("配置类必须有建议");
        assert!(
            cfg.contains("streams") || cfg.contains("-b"),
            "配置类要指到具体字段: {cfg}"
        );

        let dut = disposition_advice(ReasonCode::RxBelowTarget).expect("性能类必须有建议");
        assert!(dut.contains("性能问题"), "真正的性能不达标要说清楚: {dut}");

        // 正常结果不该冒出"处置建议"这种噪声。
        for ok in [
            ReasonCode::Pass,
            ReasonCode::PingOk,
            ReasonCode::FlowMeasured,
            ReasonCode::RxTargetMet,
            ReasonCode::ResumeFreshPass,
        ] {
            assert!(disposition_advice(ok).is_none(), "{ok} 不该有处置建议");
        }
        // 没有码时静默返回 None，不能 panic，也不能编个建议出来。
        assert!(disposition_advice(ReasonCode::None).is_none());
    }

    /// 派生层不得改动任何既有原因码：码会进 RESUME 数据库和用户既有认知。
    #[test]
    fn advice_layer_never_rewrites_the_underlying_reason_codes() {
        // 抽查各后端的代表性码，确认它们的**字符串表示**仍然原样存在。
        // 码进过 RESUME 数据库、进过用户既有认知、进过自动化断言，改一个字
        // 都是破坏性变更——换成 enum 之后守的仍然是同一件事。
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/reason.rs"),
        )
        .expect("read reason.rs");
        for code in [
            ReasonCode::SingleUdpStreamFailed,
            ReasonCode::CtsSingleUdpStreamFailed,
            ReasonCode::RxBelowTarget,
            ReasonCode::ActiveStreamsLow,
        ] {
            assert!(
                source.contains(code.as_str()),
                "原因码 {code} 不得被改写或删除"
            );
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
                (Verdict::RateFail, ReasonCode::SingleUdpStreamFailed),
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
                    (Verdict::NotEvaluated, ReasonCode::SampleCoverageLow),
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
                (Verdict::RateFail, ReasonCode::RxBelowTarget),
                (Verdict::NotEvaluated, ReasonCode::SampleCoverageLow),
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
            aggregate_verdict([
                v(Verdict::Measured),
                (Verdict::RateFail, ReasonCode::RxDropout)
            ]),
            Verdict::RateFail,
            "掉速统一归 RATE_FAIL，不能被 MEASURED 盖住"
        );
        assert_eq!(
            aggregate_verdict([v(Verdict::Measured), v(Verdict::NotEvaluated)]),
            Verdict::NotEvaluated,
            "「无法评价」优先于「评价为不合格」"
        );
    }

    /// 双向单元里一条腿判不了，能不能盖住另一条腿的 FAIL，取决于**为什么**判不了。
    #[test]
    fn only_untrustworthy_data_may_hide_the_other_leg_failure() {
        // 采样/时间轴不可信：两条腿跑在同一段时间窗里，那条 FAIL 同样可疑。
        for code in [
            ReasonCode::SampleCoverageLow,
            ReasonCode::RateWindowCoverageLow,
            ReasonCode::CounterStalled,
            ReasonCode::NicRateMissing,
            ReasonCode::EffectiveWindowShort,
            ReasonCode::ActiveStreamsLow,
        ] {
            assert_eq!(
                aggregate_verdict([
                    (Verdict::RateFail, ReasonCode::RxBelowTarget),
                    (Verdict::NotEvaluated, code),
                ]),
                Verdict::NotEvaluated,
                "{code}：数据不可信时不能拿另一条腿下结论"
            );
        }

        // 腿内局部的配置问题：不让另一条腿的测量变可疑，FAIL 必须留下来。
        for code in LEG_LOCAL_NOT_EVALUATED_CODES {
            assert_eq!(
                aggregate_verdict([
                    (Verdict::RateFail, ReasonCode::RxBelowTarget),
                    (Verdict::NotEvaluated, code),
                ]),
                Verdict::RateFail,
                "{code}：这是那条腿自己的配置问题，不该把确凿的不达标藏起来"
            );
        }

        // 没有 FAIL 时，腿内局部的判不了仍然是判不了，不能升格成 MEASURED。
        assert_eq!(
            aggregate_verdict([
                (Verdict::Measured, ReasonCode::TargetUnknown),
                (Verdict::NotEvaluated, ReasonCode::OfferedLoadLow),
            ]),
            Verdict::NotEvaluated
        );
        // SETUP_ERROR 仍然压过一切。
        assert_eq!(
            aggregate_verdict([
                (Verdict::RateFail, ReasonCode::RxBelowTarget),
                (Verdict::NotEvaluated, ReasonCode::OfferedLoadLow),
                (Verdict::SetupError, ReasonCode::NoStreamStarted),
            ]),
            Verdict::SetupError
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
            ReasonCode::SingleUdpStreamFailed
        ));
        // 同样的码配别的 verdict 不算硬失败（例如流明细行的诊断用法）。
        assert!(!is_hard_single_udp_failure(
            Verdict::NotEvaluated,
            ReasonCode::SingleUdpStreamFailed
        ));
        assert!(!is_hard_single_udp_failure(
            Verdict::RateFail,
            ReasonCode::RxBelowTarget
        ));
    }
}
