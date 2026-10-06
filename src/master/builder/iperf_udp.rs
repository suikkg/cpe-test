//! iperf UDP：每个档位一个单元；每条腿按路径上限和链路策略各自决定 `-b` 与流数，
//! 多于 1 流时每流一个独立进程与端口。
use super::*;

/// 一个「规格 × 方向 × IP 版本」组合下的全部 iperf UDP 单元。
pub(super) fn expand_iperf_udp(x: &mut Expansion<'_>, route: &Route<'_>) {
    let Route {
        spec,
        dir,
        bidir,
        pairs,
        route_str,
        v6,
        ip_tag,
        ..
    } = *route;
    if let Some(error) = spec.stream_config_error(true) {
        x.notices.push(format!(
            "{} 的 iperf UDP 流数配置非法，将按兼容范围使用 {} 流: {error}",
            spec.name,
            spec.effective_udp_streams()
        ));
    }
    let udp_streams = spec.effective_udp_streams();
    for prof in &spec.udp_profiles {
        let parsed_bandwidth = match prof.parsed_bandwidth() {
            Ok(value) => value,
            Err(error) => {
                x.notices.push_skipped(format!(
                    "跳过 {} 的 iperf {}：{error}；带宽格式非法，未生成任务",
                    spec.name,
                    prof.label()
                ));
                continue;
            }
        };
        // 每个方向腿按 min(发送口, 接收口) 的路径上限
        // 各自决定 -b 与流数：同一条链路的两个方向
        // 能力可以差很多，共用一个 -b 没有物理依据。
        let leg_loads: Vec<UdpLoad> = pairs
            .iter()
            .map(|(s, d, _tag)| udp_leg_load(spec, s, d, parsed_bandwidth, udp_streams))
            .collect();
        let leg_profiles: Vec<UdpProfile> = pairs
            .iter()
            .map(|(s, d, _tag)| udp_leg_profile(spec, prof, s, d))
            .collect();
        for ((s, d, _tag), load) in pairs.iter().zip(leg_loads.iter()) {
            if let Some(notice) = udp_clip_notice(&spec.name, &prof.label(), s, d, load) {
                x.notices.push(notice);
            }
        }
        let mut legs = Vec::new();
        let mut target_lines: Vec<String> = Vec::new();
        let mut max_n = 1;
        for (leg_idx, ((s, d, tag), load)) in pairs.iter().zip(leg_loads.iter()).enumerate() {
            let n = load.streams;
            max_n = max_n.max(n);
            let leg_policy = link_policy(spec, s, d);
            let effective = &leg_profiles[leg_idx];
            let leg_label = udp_leg_label(effective, load, parsed_bandwidth);
            let mut extra: Vec<String> = vec!["-b".into(), load.iperf_arg()];
            if let Some(l) = &effective.length {
                extra.push("-l".into());
                extra.push(l.clone());
            }
            if let Some(w) = &effective.window {
                extra.push("-w".into());
                extra.push(w.clone());
            }
            let flow_direction = route.flow_direction(tag);
            let (effective_mode, target) =
                x.leg_rate(route, &leg_policy, &flow_direction, s, d, &mut target_lines);
            // offered 必须跟着实际下发的 -b 走，否则
            // 报表里的「请求负载」和命令行对不上。
            let offered_per_stream_mbps = Some(load.mbps);
            // 对齐键只认档位本身：裁剪、按网口策略改写的 -b / -l
            // 都随协商速率或 IP 变化，见 `IperfTask::comparison_label`。
            let comparison_label = prof.label();
            let mk = |idx: usize, port: u16| IperfTask {
                v6,
                udp: true,
                profile_name: prof.name(),
                profile_label: leg_label.clone(),
                comparison_label: comparison_label.clone(),
                src: (*s).clone(),
                dst: (*d).clone(),
                port,
                duration: spec.duration,
                extra: extra.clone(),
                stream_idx: idx,
                rate_mode: effective_mode,
                rx_target_mbps: target,
                offered_per_stream_mbps,
            };
            if let Some(msg) = unreachable_target_notice(
                &spec.name,
                &leg_label,
                s,
                d,
                n,
                target,
                offered_per_stream_mbps,
                &spec.rate_check,
            ) {
                x.notices.push(msg);
            }
            let kind = if n <= 1 {
                LegKind::IperfSingle(mk(0, x.port()))
            } else {
                let streams: Vec<IperfTask> = (0..n as usize).map(|i| mk(i, x.port())).collect();
                LegKind::IperfGroup {
                    name: prof.name(),
                    streams,
                }
            };
            legs.push(Leg {
                tag: tag.to_string(),
                kind,
            });
        }
        let stream_note = if max_n > 1 {
            format!(" ×{max_n}流")
        } else {
            String::new()
        };
        let profile_label = udp_unit_label(prof, parsed_bandwidth, &leg_loads, &leg_profiles);
        let title = format!(
            "{}IPERF {} {}{} | {}",
            if bidir { "★★双向 " } else { "" },
            ip_tag,
            profile_label,
            stream_note,
            route_str
        );
        let id = udp_resume_unit_id_v4(spec, ip_tag, dir, prof, &legs);
        // 错峰按单腿最大流数估算：双向双腿并行，不能把
        // 两条腿的流数相加。
        let est_secs = udp_estimated_secs(
            spec.duration,
            max_n as u64,
            spec.rate_mode,
            &spec.rate_check,
        );
        x.units
            .push(route.unit(id, title, target_lines, legs, est_secs));
    }
}

/// **计划期就要说清「这几条流灌不到这个门限」。**
///
/// 必需流数按 `target×(1+余量)/每流负载` 上取整。配少了，发出去的
/// 总负载本身就低于门限，接收端 RX 几乎必然跟着低于门限、判 RATE_FAIL——
/// 那是配置决定的结果，不是设备测出来的。执行端只把「流数不足」记成
/// 诊断（ADR-17），不会改判，所以这句话必须在跑之前说。
/// 公式复用执行端那一份，不在这里重写。
#[allow(clippy::too_many_arguments)]
fn unreachable_target_notice(
    spec_name: &str,
    leg_label: &str,
    sender: &Endpoint,
    receiver: &Endpoint,
    n: u32,
    target: Option<f64>,
    offered_per_stream_mbps: Option<f64>,
    rate_check: &RateCheckCfg,
) -> Option<String> {
    if n <= 1 {
        return None;
    }
    let required = crate::master::executor::required_udp_streams(
        n as usize,
        rate_check,
        target,
        offered_per_stream_mbps,
    );
    if required <= n as usize {
        return None;
    }
    let (Some(target), Some(per_stream)) = (target, offered_per_stream_mbps) else {
        return None;
    };
    Some(format!(
        "{spec_name} {leg_label}：{} -> {} {n} 条流 × {per_stream:.0}Mbps 灌不到 {target:.0}Mbps 门限\
         （含 {:.0}% 余量至少要 {required} 条并发流）。\
         按当前配置发出去的总负载就低于门限，接收端 RX 大概率达不到，\
         会判「速率不达标 / RX_BELOW_TARGET」。把流数提到 {required}、\
         调大每流 -b，或把门限降到 {:.0}Mbps 以下。",
        sender.nic.name,
        receiver.nic.name,
        rate_check.offered_headroom_pct.max(0.0),
        per_stream * n as f64 / (1.0 + rate_check.offered_headroom_pct.max(0.0) / 100.0),
    ))
}

/// iperf UDP 单元的“预计总耗时”（秒），按典型成功路径估算：
/// 第一次完整尝试的时长 + 启动/收尾/错峰开销。
///
/// 单流 UDP 的重试只在“当次尝试没有产生任何有效测量”时发生，属于异常路径；
/// 若按最坏情况（最多 3 次完整尝试 × 每次再附加 130s 宽限）累加，
/// 180s 的单流 UDP 项会被估成 14+ 分钟，开始前的总耗时规划会严重偏大。
/// 因此这里统一按一次尝试估算，与多流 UDP / TCP 口径一致。
///
/// 错峰只按单腿最大流数计算：双向 AB/BA 腿是并行执行的，
/// 不能把两条腿的流数相加，否则双向会凭空多出毫秒级错峰取整。
pub(super) fn udp_estimated_secs(
    duration: u64,
    max_leg_streams: u64,
    mode: RateMode,
    rate_cfg: &RateCheckCfg,
) -> u64 {
    let stagger_ms = max_leg_streams
        .saturating_sub(1)
        .saturating_mul(rate_cfg.launch_interval_ms.clamp(0, 1_000));
    let discovery_ms = if mode == RateMode::Discover {
        3_u64
            .saturating_mul(rate_cfg.discovery_step_secs)
            .saturating_mul(1_000)
    } else {
        0
    };
    duration
        .saturating_add(rate_cfg.background_secs.min(30))
        .saturating_add(rate_cfg.startup_timeout_secs)
        .saturating_add(rate_cfg.settle_secs)
        .saturating_add(5)
        .saturating_add(stagger_ms.saturating_add(discovery_ms).div_ceil(1_000))
}
