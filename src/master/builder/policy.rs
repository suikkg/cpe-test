//! 速率目标、链路策略与 CTS 参数解析。
//!
//! 从 `identity` 分出来的一组：它们回答的是「这条腿该按什么参数跑、目标是多少」，
//! 而不是「这条腿叫什么名字」。两件事混在一个文件里时，改目标推导的人会以为
//! 自己在动 resume identity（那是**不能碰**的），改 identity 的人又会顺手动到
//! 目标推导——所以按「改动的理由」分开。
use super::*;

pub(super) fn cts_window_bytes(value: &str) -> Result<Option<u32>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("auto")
        || trimmed.eq_ignore_ascii_case("default")
    {
        Ok(None)
    } else {
        parse_size_bytes(trimmed).map(Some)
    }
}

/// 解析这条 (src -> dst) 的两层链路策略。
///
/// 单独包一层是为了把 `Side -> 配置里的 host 字符串` 这个映射收在一处：
/// 四个任务分支都要解析策略，映射写错一次就会静默地让整类覆盖失效。
pub(super) fn link_policy(spec: &SpecNorm, src: &Endpoint, dst: &Endpoint) -> rate::LinkPolicy {
    rate::resolve_link_policy(
        &spec.link_profiles,
        host_key(src.side),
        &src.nic,
        host_key(dst.side),
        &dst.nic,
    )
}

/// 门限来自协商速率百分比时，把算式作为计划提示说出来（每条算式只说一次）。
///
/// 不说的话，同一份配置在 Wi-Fi 重新协商后跑出不同门限，报告上看不出为什么。
pub(super) fn note_rx_target(
    notices: &mut Vec<String>,
    seen: &mut HashSet<String>,
    spec_name: &str,
    policy: &rate::LinkPolicy,
) {
    if let Some(note) = &policy.rx_target_note {
        let line = format!("{spec_name}：{note}");
        if seen.insert(line.clone()) {
            notices.push(line);
        }
    }
}

/// 这条腿要用的 RX 门限。
///
/// 顺序：**双向配对门限 → 单口覆盖 → 场景 targets → 内置推导**。
///
/// 双向配对门限排在最前，因为它是唯一一个知道「这条腿属于哪一对网口、
/// 而且两个方向正在同时灌」的来源。半双工介质上这两件事缺一不可：
/// 同一块 RNDIS 口，和 Wi-Fi 组双向、和 SGMII 组双向，能收到的速率
/// 完全不是一个量级——挂在网卡上的那个数没法同时对这两组成立。
///
/// 只在 `bidir` 为真时参与，所以单向单元的判定一个字节都没变。
#[allow(clippy::too_many_arguments)]
pub(super) fn leg_rx_target(
    spec: &SpecNorm,
    policy: &rate::LinkPolicy,
    flow_direction: &str,
    bidir: bool,
    src: &NicInfo,
    dst: &NicInfo,
) -> Option<f64> {
    if bidir {
        // 配了「两端 RX 合计」门限时，这一腿**没有自己的门限**：判定在单元级
        // 比一次合计。给它留一个每方向门限，报告上就会出现「AB 判 RATE_FAIL、
        // 单元判 PASS」这种自相矛盾的两行。
        if spec.rate_target_bidir_total.is_some() {
            return None;
        }
        if let Some(target) = spec.rate_targets_bidir.for_direction(flow_direction) {
            return Some(target);
        }
    }
    policy.rx_target_mbps.or_else(|| {
        rate::resolve_target_mbps(
            spec.rate_mode,
            &spec.rate_targets,
            flow_direction,
            src,
            dst,
            &spec.rate_check,
        )
    })
}

/// 这一腿的门限**是从哪一层来的**。
///
/// 存在的理由是预览：「字段还在、实际却被另一条规则盖掉」这种事，光看请求体
/// 看不出来——`RateTargets::for_direction("ab")` 是 `ab.or(forward)`，任务里
/// 显式填的 `forward` 可以被一张频段表插进来的 `ab` 无声推翻。把最终数字和它
/// 的来源一起印在计划页上，是唯一能让人当场发现的办法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RxTargetSource {
    /// 双向单元按两端 RX 合计判定，这一腿只测量。
    BidirTotal,
    /// 双向单元的每方向门限（任务/配对填的，或旧频段规则迁移来的）。
    BidirDirection,
    /// 按网口门限与负载（含百分比换算）。
    NicPolicy,
    /// 任务 / Wi-Fi 频段表 / 全局门限——它们最终都落在 `rate_targets` 上。
    ScenarioTargets,
    /// 内置 EVB 推导。
    Derived,
    /// 这一腿没有门限，只记录实测能力。
    None,
}

impl RxTargetSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            RxTargetSource::BidirTotal => "双向 RX 合计门限（本腿只测量）",
            RxTargetSource::BidirDirection => "双向方向门限",
            RxTargetSource::NicPolicy => "按网口门限",
            RxTargetSource::ScenarioTargets => "任务/频段/全局门限",
            RxTargetSource::Derived => "内置推导",
            RxTargetSource::None => "未配置门限",
        }
    }
}

/// 这一腿要用的 `(判定模式, RX 门限, 门限来源)`。
///
/// 单独包一层的理由是「配了合计门限的双向腿」必须同时改两件事：门限清空，
/// **并且**模式落到 `Observe`。只清门限的话，显式配 `verify` 的用户会拿到
/// 一整轮 `NOT_EVALUATED / TARGET_MISSING`——腿本来就不该有目标，这不是缺配置。
pub(super) fn leg_rate_plan(
    spec: &SpecNorm,
    policy: &rate::LinkPolicy,
    flow_direction: &str,
    bidir: bool,
    src: &NicInfo,
    dst: &NicInfo,
) -> (RateMode, Option<f64>, RxTargetSource) {
    if bidir && spec.rate_target_bidir_total.is_some() {
        return (RateMode::Observe, None, RxTargetSource::BidirTotal);
    }
    if bidir
        && spec
            .rate_targets_bidir
            .for_direction(flow_direction)
            .is_some()
    {
        let target = leg_rx_target(spec, policy, flow_direction, bidir, src, dst);
        return (
            rate::effective_mode(spec.rate_mode, target),
            target,
            RxTargetSource::BidirDirection,
        );
    }
    let target = leg_rx_target(spec, policy, flow_direction, bidir, src, dst);
    let source = if policy.rx_target_mbps.is_some() {
        RxTargetSource::NicPolicy
    } else if spec.rate_targets.for_direction(flow_direction).is_some() {
        RxTargetSource::ScenarioTargets
    } else if target.is_some() {
        RxTargetSource::Derived
    } else {
        RxTargetSource::None
    };
    (rate::effective_mode(spec.rate_mode, target), target, source)
}

/// `-w × 流数` 大到这条链路要花多少秒才排空；超过它就提示。
///
/// 2 秒是个够宽松的界：正常的 BDP 档位（64k~4m × 10 流）在 1G 上只有几十
/// 毫秒，而一旦到了「几秒钟的链路时间」，socket 缓冲本身就变成了测量对象。
pub(super) const SOCKET_BUFFER_DRAIN_WARN_SECS: f64 = 2.0;

/// `-w` 开得过大时给一条提示。
///
/// iperf3 的 `-w` 是 socket 缓冲，被塞进去的字节算进「发送」但可能一个都没
/// 上线。run_20260825_215915_7684 用的是 `-w 256m -P 10`，等于 2.56GB 的
/// 发送缓冲；65 条 TCP 记录的「发 − 收」差值稳定在 118.92 ± 1.90 Mbps，
/// 而 `2.56GB ÷ 180s = 119.3Mbps`——那个差值整个就是缓冲，不是链路。
/// 首秒打出的 `22271Mbps` 同样来自这里（见 .ai/DESIGN-v4.3.0.md D5）。
///
/// 只提示不改写：`-w` 是用户明确填的参数，工具不该背着人改测试条件。
#[allow(clippy::too_many_arguments)]
pub(super) fn oversized_socket_buffer_notice(
    spec_name: &str,
    profile_label: &str,
    window: &str,
    streams: u32,
    duration_secs: u64,
    sender: &Endpoint,
    receiver: &Endpoint,
    rate_cfg: &RateCheckCfg,
) -> Option<String> {
    let window_bytes = cts_window_bytes(window).ok().flatten()? as f64;
    let ceiling_mbps = rate::path_payload_ceiling_mbps(&sender.nic, &receiver.nic, rate_cfg)?;
    if ceiling_mbps <= 0.0 {
        return None;
    }
    let total_bytes = window_bytes * streams.max(1) as f64;
    let drain_secs = total_bytes * 8.0 / (ceiling_mbps * 1_000_000.0);
    if drain_secs <= SOCKET_BUFFER_DRAIN_WARN_SECS {
        return None;
    }
    // 虚高幅度必须按本次实际时长折算。写死 180 的话，同一段文字里的
    // 「总缓冲 X GB」「排空 Y 秒」和这个 Mbps 会自相矛盾；报告里的
    // `in_flight_buffer_estimate` 用的是 required_seconds，两处也会对不上。
    let inflation_mbps = total_bytes * 8.0 / 1e6 / duration_secs.max(1) as f64;
    Some(format!(
        "{spec_name} {profile_label}：-w {window} × {streams} 流 = {:.2}GB socket 缓冲，\
         相当于这条链路 {drain_secs:.1} 秒的流量。这些字节会被算进「工具自报发送」但未必上线，\
         使 {duration_secs}s 的测试里「发送−接收」出现约 {inflation_mbps:.0}Mbps 的恒定虚高；\
         判定用的接收端网卡口径不受影响。",
        total_bytes / 1e9,
    ))
}

pub(super) fn cts_task_config_errors(spec: &SpecNorm, udp: bool) -> Vec<String> {
    let mut errors = spec
        .ctstraffic_config_error
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    if let Some(error) = spec.stream_config_error(udp) {
        errors.push(error);
    }
    if !(100..=60_000).contains(&spec.ctstraffic.status_update_ms) {
        errors.push(format!(
            "ctsTraffic status_update_ms 必须在 100..=60000，当前为 {}",
            spec.ctstraffic.status_update_ms
        ));
    }
    if udp {
        if spec.ctstraffic.udp_frame_rate == 0 {
            errors.push("ctsTraffic udp_frame_rate 必须大于 0，当前为 0".into());
        }
        if spec.ctstraffic.udp_buffer_depth_secs == 0 {
            errors.push("ctsTraffic udp_buffer_depth_secs 必须大于 0，当前为 0".into());
        }
    }
    errors
}

pub(super) fn cts_udp_bandwidth(profile: &UdpProfile) -> Result<ParsedBandwidth, String> {
    profile.parsed_bandwidth()
}

pub(super) fn cts_datagram_bytes(profile: &UdpProfile) -> Result<Option<u32>, String> {
    profile
        .length
        .as_deref()
        .map(parse_size_bytes)
        .transpose()
        .and_then(|value| {
            if value.is_some_and(|size| size > 65_507) {
                Err("ctsTraffic UDP datagram 必须不大于 65507 字节".into())
            } else {
                Ok(value)
            }
        })
}
