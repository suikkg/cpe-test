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
pub(super) fn note_rx_target(notices: &mut Notices, spec_name: &str, policy: &rate::LinkPolicy) {
    if let Some(note) = &policy.rx_target_note {
        notices.push(format!("{spec_name}：{note}"));
    }
}

/// 这条腿要用的 RX 门限。
///
/// 顺序：**配对门限（按单向/双向各取一套）→ 单口覆盖 → 场景 targets → 内置推导**。
///
/// 配对门限排在最前，因为它是唯一一个知道「这条腿属于哪一对网口」的来源。
/// 挂在网卡上的那个数没法同时对这块口的所有对端成立：同一块 RNDIS 口，和
/// Wi-Fi 组、和 SGMII 组，能收到的速率完全不是一个量级；同一块 SGMII2.5G 口，
/// 对端是 1G 口时，收口上挂的 1800/2000 在这条路径上物理上就跑不到——
/// `cap_rx_target_to_link_speed` 只能把它压到线速的 95%，压不出「这条链路
/// 该验收多少」。
///
/// 单向与双向各有一套，不共用一个数：双向同时灌包时两个方向互相抢，每个方向
/// 拿到的只有单向时的一部分，拿单向门限去卡双向必然判 `RATE_FAIL`。
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
    } else if let Some(target) = spec.rate_targets_single.for_direction(flow_direction) {
        return Some(target);
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
    /// 单向单元的每方向门限（任务/配对填的）。
    SingleDirection,
    /// 按网口门限与负载（含百分比换算）。
    NicPolicy,
    /// Wi-Fi 频段表 / 全局门限 / 旧项目带来的任务 targets——它们最终都落在
    /// `rate_targets` 上（界面上那两格单向门限走的是 `SingleDirection`）。
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
            RxTargetSource::SingleDirection => "单向方向门限",
            RxTargetSource::NicPolicy => "按网口门限",
            RxTargetSource::ScenarioTargets => "任务/频段/全局门限",
            RxTargetSource::Derived => "内置推导",
            RxTargetSource::None => "未配置门限",
        }
    }
}

/// 这一腿最终的判定参数。
///
/// 单独成结构而不是元组，是因为它有四件必须一起读的事：门限、门限的来源、
/// 判定模式，以及**这个门限有没有被改写过**。少读最后一条，报告上就会出现
/// 「配置里 1800、判定按 950」而没人说得清是谁改的。
#[derive(Debug, Clone)]
pub(super) struct LegRatePlan {
    pub mode: RateMode,
    pub target_mbps: Option<f64>,
    pub source: RxTargetSource,
    /// 「最终判定用的门限，为什么不是你在配置里填的那个」。
    ///
    /// 只装这一类话：被路径上限折算了、被合计门限盖掉了。这类改写不说出来
    /// 就是无声的——配置里的字段原样躺着，报告里印的却是另一个数，而两边
    /// 都不会提示读的人去看另一边。
    pub notes: Vec<String>,
}

/// 这一腿要用的判定参数。
///
/// 单独包一层的理由是「配了合计门限的双向腿」必须同时改两件事：门限清空，
/// **并且**模式落到 `Observe`。只清门限的话，显式配 `verify` 的用户会拿到
/// 一整轮 `NOT_EVALUATED / TARGET_MISSING`——腿本来就不该有目标，这不是缺配置。
///
/// 协商速率封顶也收在这里，而不是散在四个调用点：它是**全仓唯一**一处把
/// 「配置里的门限」换算成「实际判定的门限」的地方，多一处就会有一条腿按
/// 未封顶的值判。
pub(super) fn leg_rate_plan(
    spec: &SpecNorm,
    policy: &rate::LinkPolicy,
    flow_direction: &str,
    bidir: bool,
    src: &NicInfo,
    dst: &NicInfo,
) -> LegRatePlan {
    let capped = |target: Option<f64>, source: RxTargetSource| {
        let (target_mbps, cap_note) =
            rate::cap_rx_target_to_link_speed(target, src, dst, &spec.rate_check);
        LegRatePlan {
            mode: rate::effective_mode(spec.rate_mode, target_mbps),
            target_mbps,
            source,
            notes: cap_note.into_iter().collect(),
        }
    };
    if let Some(total) = spec.rate_target_bidir_total.filter(|_| bidir) {
        // 合计门限继续优先——判定口径不变。但它把两条腿的门限清空这件事必须
        // 说出来：run_20260905_125327_5940 里套件明明写了 ab/ba 各 900Mbps，
        // 频段表里一条 `bidir_total = 900` 就把它们整个吞掉，单元按
        // 「AB + BA ≥ 900」判成 PASS（522.9 + 440.5 = 963.4），而逐方向判的话
        // 两条腿都不达标。两处配置都在，报告上却看不出是哪一处生效了。
        let shadowed = spec
            .rate_targets_bidir
            .for_direction(flow_direction)
            .map(|per_direction| {
                format!(
                    "双向 RX 合计门限 {total:.0}Mbps 已盖掉逐方向门限 {flow_direction} \
                     {per_direction:.0}Mbps：本单元只比一次合计，两条腿各自不再判定。\
                     要逐方向把关，请清掉合计门限。"
                )
            });
        return LegRatePlan {
            mode: RateMode::Observe,
            target_mbps: None,
            source: RxTargetSource::BidirTotal,
            notes: shadowed.into_iter().collect(),
        };
    }
    if bidir
        && spec
            .rate_targets_bidir
            .for_direction(flow_direction)
            .is_some()
    {
        let target = leg_rx_target(spec, policy, flow_direction, bidir, src, dst);
        return capped(target, RxTargetSource::BidirDirection);
    }
    if !bidir
        && spec
            .rate_targets_single
            .for_direction(flow_direction)
            .is_some()
    {
        let target = leg_rx_target(spec, policy, flow_direction, bidir, src, dst);
        return capped(target, RxTargetSource::SingleDirection);
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
    capped(target, source)
}

/// 把「最终门限为什么不是配置里那个」作为计划提示说出来（每条只说一次）。
pub(super) fn note_target_cap(notices: &mut Notices, spec_name: &str, plan: &LegRatePlan) {
    for note in &plan.notes {
        notices.push(format!("{spec_name}：{note}"));
    }
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

/// UDP 按整条路径的可信负载上限裁剪流数。
/// RNDIS 3.7G 协商按约 2.5G，10GUSB 的 4.2G 已知显示 bug 不按 4.2G 裁剪。
pub(super) fn allowed_udp_streams_for_mbps(
    sender: &Endpoint,
    receiver: &Endpoint,
    bandwidth_mbps: f64,
    want: u32,
    limit: bool,
    rate_cfg: &RateCheckCfg,
) -> u32 {
    if !limit {
        return want;
    }
    let Some(speed) = rate::path_payload_ceiling_mbps(&sender.nic, &receiver.nic, rate_cfg) else {
        return want;
    };
    let bw = bandwidth_mbps;
    if bw <= 0.0 {
        return want;
    }
    let max_n = (speed / bw).floor() as u32;
    max_n.min(want)
}

/// 一条方向腿实际下发的 UDP 负载：单流 `-b` 与流数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct UdpLoad {
    pub bits_per_second: u64,
    pub mbps: f64,
    pub streams: u32,
    /// 单流带宽被路径上限压低时，记下原始请求值，供任务标签与报表说明。
    pub clipped_from_mbps: Option<f64>,
}

impl UdpLoad {
    /// iperf3 的无后缀带宽值按 bit/s 解释。传精确整数可避免依赖它对
    /// `Gbps` 等长后缀的非文档兼容行为。
    pub(crate) fn iperf_arg(self) -> String {
        self.bits_per_second.to_string()
    }
}

/// 按整条路径的可信负载上限决定这条腿的 `-b` 和流数。
///
/// 优先降流数（保持单流带宽不变），流数已经降到 1 仍然超限时才压 `-b`。
///
/// 旧行为在「单流带宽就已经超过路径上限」时返回 0 流，调用方据此把任务整个
/// 跳过。run_20260825_215915_7684 里 80 条 UDP 命令全部带着同一个
/// `-b 2600000000`，其中相当一部分打向 1Gbps 收端，制造出 60~99% 的丢包——
/// 那是配置出来的丢包，不是测出来的。给 1Gbps 收端灌 1Gbps 拿到一个真实
/// 结论，永远好过跳过或者灌 2.6G 拿到一个必然失败的结论。
/// 详见 .ai/DESIGN-v4.3.0.md D4。
pub(crate) fn udp_load_for_leg(
    sender: &Endpoint,
    receiver: &Endpoint,
    requested: ParsedBandwidth,
    want_streams: u32,
    limit: bool,
    explicit: bool,
    rate_cfg: &RateCheckCfg,
) -> UdpLoad {
    let want = want_streams.max(1);
    let as_requested = |streams: u32| UdpLoad {
        bits_per_second: requested.bits_per_second,
        mbps: requested.mbps,
        streams,
        clipped_from_mbps: None,
    };
    // `explicit` = 这条链路在 link_profiles 里被专门指定过带宽。
    // 那是操作者对这条链路的明确判断，自动裁剪不该覆盖它——裁剪是给
    // 没配过的链路兜底用的安全网，不是用来推翻人的决定的。
    if explicit || !limit || requested.mbps <= 0.0 {
        return as_requested(want);
    }
    let Some(ceiling) = rate::path_payload_ceiling_mbps(&sender.nic, &receiver.nic, rate_cfg)
    else {
        return as_requested(want);
    };
    let fit = (ceiling / requested.mbps).floor();
    if fit >= 1.0 {
        return as_requested((fit as u32).clamp(1, want));
    }
    // 单流就已经超过整条路径的可信上限：压 -b，而不是放弃这条腿。
    let bits_per_second = (ceiling * 1_000_000.0).round().max(1.0) as u64;
    UdpLoad {
        bits_per_second,
        mbps: bits_per_second as f64 / 1_000_000.0,
        streams: 1,
        clipped_from_mbps: Some(requested.mbps),
    }
}
