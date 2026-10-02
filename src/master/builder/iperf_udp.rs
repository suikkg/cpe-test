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
                x.notices.push(format!(
                    "跳过 {} 的 iperf UDP profile {}：{error}；带宽格式非法，未生成任务",
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
            .map(|(s, d, _tag)| {
                // 单口覆盖 / 角色配对可以改写这条腿的
                // 单流带宽；解析不了就退回全局档位，
                // 绝不因为一个笔误让任务凭空消失。
                let configured = link_policy(spec, s, d)
                    .udp_bandwidth
                    .and_then(|value| UdpProfile::bw(&value).parsed_bandwidth().ok());
                udp_load_for_leg(
                    s,
                    d,
                    configured.unwrap_or(parsed_bandwidth),
                    udp_streams,
                    spec.udp_limit,
                    configured.is_some(),
                    &spec.rate_check,
                )
            })
            .collect();
        // 发送口可以单独覆盖 `-l`：同一条用例在不同网口上
        // 要用不同报文长度是常见需求。按腿算一次，标签和
        // 命令都从这里取，免得两边各算一遍再对不上。
        let leg_profiles: Vec<UdpProfile> = pairs
            .iter()
            .map(|(s, d, _tag)| UdpProfile {
                bandwidth: prof.bandwidth.clone(),
                length: link_policy(spec, s, d)
                    .udp_length
                    .or_else(|| prof.length.clone()),
                window: prof.window.clone(),
            })
            .collect();
        for ((s, d, _tag), load) in pairs.iter().zip(leg_loads.iter()) {
            if let Some(from) = load.clipped_from_mbps {
                x.notices.push(format!(
                    "{} {}：{} -> {} 路径上限不足，-b 由 {:.0}Mbps 裁剪到 {:.0}Mbps",
                    spec.name,
                    prof.label(),
                    s.nic.name,
                    d.nic.name,
                    from,
                    load.mbps
                ));
            }
        }
        let mut legs = Vec::new();
        let mut target_lines: Vec<String> = Vec::new();
        let mut max_n = 1;
        for (leg_idx, ((s, d, tag), load)) in pairs.iter().zip(leg_loads.iter()).enumerate() {
            let n = load.streams;
            max_n = max_n.max(n);
            // 标签必须反映**实际下发**的 -b。链路策略
            // 覆盖和路径裁剪都会改它，而报表里的
            // 「类型 / 参数」列是很多人唯一会看的地方——
            // 那里印着 2.6G、命令行却是 1G，比不印更糟。
            // 裁剪与否只能问 clipped_from_mbps：链路策略
            // 先把 2.5G 改成 2.6G、路径上限再裁回 2500，
            // 拿全局档位去比会得出「没变」，把两次改写
            // 一起抹掉。
            // 标签必须反映**实际下发**的 -l，不是档位里那个。
            let leg_policy = link_policy(spec, s, d);
            let effective = &leg_profiles[leg_idx];
            let leg_label = if let Some(from) = load.clipped_from_mbps {
                format!(
                    "{}（按路径上限从 {:.0}M 裁剪至 {:.0}M）",
                    effective.label(),
                    from,
                    load.mbps
                )
            } else if (load.mbps - parsed_bandwidth.mbps).abs() >= f64::EPSILON {
                format!("{}（按链路策略至 {:.0}M）", effective.label(), load.mbps)
            } else {
                effective.label()
            };
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
                &leg_label,
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
/// 执行端要求「所有必需流并发活跃」才算有效判定窗口，
/// 而必需流数按 `target×(1+余量)/每流负载` 上取整。配少了
/// 就不是「勉强够呛」，而是那个窗口**永远形不成**：整条腿
/// 稳定判 NOT_EVALUATED/EFFECTIVE_WINDOW_SHORT。
///
/// 现场代价是这条链路完全确定、却只能事后才知道：真机上
/// 一轮 180s 预设的 UDP 单元会**全部**这样跑完再报「无法
/// 评价」，而拿到的原因码指向采样窗口，不指向真因。
/// 公式复用执行端那一份，不在这里重写。
fn unreachable_target_notice(
    leg_label: &str,
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
        "{} UDP {n} 条流 × {per_stream:.0}Mbps 灌不到 {target:.0}Mbps 门限\
         （含 {:.0}% 余量至少要 {required} 条并发流）。\
         按当前配置这一腿的有效判定窗口永远形不成，结果会稳定落在\
         「无法评价 / EFFECTIVE_WINDOW_SHORT」。把流数提到 {required}、\
         调大每流 -b，或把门限降到 {:.0}Mbps 以下。",
        leg_label,
        rate_check.offered_headroom_pct.max(0.0),
        per_stream * n as f64 / (1.0 + rate_check.offered_headroom_pct.max(0.0) / 100.0),
    ))
}

/// 单元标题里的档位标签。
///
/// 标题里的 -b 必须是**实际下发**的值。链路策略和
/// 路径裁剪都会改它，而任务清单（控制台的「预览
/// 任务」、日志开头的编号列表）是很多人唯一会看
/// 的地方——那里印着全局档位、命令行却是别的数，
/// 会让人以为自己填的值没生效。
///
/// 两条腿取值不同时退回档位标签：一个标题写不下
/// 两个方向，逐行的 profile_label 里各自写着准确值。
fn udp_unit_label(
    prof: &UdpProfile,
    parsed_bandwidth: ParsedBandwidth,
    leg_loads: &[UdpLoad],
    leg_profiles: &[UdpProfile],
) -> String {
    let uniform = leg_loads.first().is_some_and(|first| {
        leg_loads
            .iter()
            .all(|load| (load.mbps - first.mbps).abs() < f64::EPSILON)
    });
    let effective = leg_loads
        .first()
        .map(|first| first.mbps)
        .unwrap_or(parsed_bandwidth.mbps);
    // `-l` 被发送口改写时，标题同样不能再印档位里的原值。
    let leg_lengths: Vec<Option<String>> = leg_profiles.iter().map(|p| p.length.clone()).collect();
    let length_changed = leg_lengths.iter().any(|length| *length != prof.length);
    let changed = length_changed
        || leg_loads
            .iter()
            .any(|load| (load.mbps - parsed_bandwidth.mbps).abs() >= f64::EPSILON);
    if !changed {
        return prof.label();
    }
    // 两条腿取值不同就两个都印（顺序即腿序 ab/ba）：
    // 退回全局档位会显示一个谁都没在用的数。
    let bw = if uniform {
        format!("{effective:.0}m")
    } else {
        leg_loads
            .iter()
            .map(|load| format!("{:.0}m", load.mbps))
            .collect::<Vec<_>>()
            .join("/")
    };
    let mut label = format!("UDP -b {bw}");
    let uniform_length = leg_lengths
        .first()
        .is_some_and(|first| leg_lengths.iter().all(|length| length == first));
    if uniform_length {
        if let Some(Some(l)) = leg_lengths.first() {
            label.push_str(&format!(" -l {l}"));
        }
    } else {
        let shown = leg_lengths
            .iter()
            .map(|length| length.as_deref().unwrap_or("默认"))
            .collect::<Vec<_>>()
            .join("/");
        label.push_str(&format!(" -l {shown}"));
    }
    if let Some(w) = &prof.window {
        label.push_str(&format!(" -w {w}"));
    }
    label
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
