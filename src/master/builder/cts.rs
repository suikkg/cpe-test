//! Microsoft ctsTraffic（Windows 10+ 专用）：TCP 每个 socket buffer 档位一个单元，
//! UDP 每个档位一个单元。配置非法的单元照常进计划，执行器直接报 SETUP_ERROR。
use super::*;

/// 一个「规格 × 方向 × IP 版本」组合下的全部 ctsTraffic 单元。
pub(super) fn expand_cts(x: &mut Expansion<'_>, route: &Route<'_>) {
    let mut gate = TopologyGate {
        blocked: !route.v6 && !route.same_subnet_ok,
        notice_emitted: false,
    };
    for transport in &route.spec.transports {
        if transport == "tcp" {
            expand_cts_tcp(x, route, &mut gate);
        } else if transport == "udp" {
            expand_cts_udp(x, route, &mut gate);
        }
    }
}

/// 跨机 IPv4 不同 /24 的门禁，TCP 与 UDP 两种传输共用一份「提示过没有」。
struct TopologyGate {
    blocked: bool,
    notice_emitted: bool,
}

impl TopologyGate {
    /// 这个单元要不要因为拓扑跳过。配置非法的单元不在这里跳过，照常进计划报
    /// SETUP_ERROR；跳过的提示每个组合只说一次。
    fn skips(
        &mut self,
        x: &mut Expansion<'_>,
        spec: &SpecNorm,
        setup_error: &Option<String>,
    ) -> bool {
        if !self.blocked || setup_error.is_some() {
            return false;
        }
        if !self.notice_emitted {
            x.notices.push_skipped(format!(
                "跳过 {} 的 ctsTraffic：两端 IPv4 不同 /24 ({} vs {})，无法直连灌包",
                spec.name, spec.src.nic.ipv4, spec.dst.nic.ipv4
            ));
            self.notice_emitted = true;
        }
        true
    }
}

fn expand_cts_tcp(x: &mut Expansion<'_>, route: &Route<'_>, gate: &mut TopologyGate) {
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
    let tcp_streams = spec.effective_tcp_streams();
    for window in &spec.tcp_windows {
        let mut setup_errors = cts_task_config_errors(spec, false);
        let mut window_invalid = false;
        let window_bytes = match cts_window_bytes(window) {
            Ok(value) => value,
            Err(error) => {
                window_invalid = true;
                setup_errors.push(format!("CTS TCP socket buffer {window:?} 非法: {error}"));
                None
            }
        };
        let setup_error = (!setup_errors.is_empty()).then(|| setup_errors.join("；"));
        if gate.skips(x, spec, &setup_error) {
            continue;
        }
        if let Some(error) = &setup_error {
            x.notices.push(format!(
                "{} CTS TCP 配置非法，将记录 SETUP_ERROR: {error}",
                spec.name
            ));
        }
        let window_label = if window_invalid {
            format!("socket-buffer {window}（非法）")
        } else {
            window_bytes
                .map(|bytes| format!("socket-buffer {window} ({bytes}B)"))
                .unwrap_or_else(|| "socket-buffer 自动".into())
        };
        let profile_name = format!(
            "cts_tcp_w{}_c{}",
            if window.trim().is_empty() {
                "auto"
            } else {
                window
            },
            tcp_streams
        );
        let profile_label = format!("CTS TCP {window_label} ×{}连接", tcp_streams);
        let mut legs = Vec::new();
        let mut target_lines: Vec<String> = Vec::new();
        for (src, dst, tag) in pairs {
            let flow_direction = route.flow_direction(tag);
            let (effective_mode, target) = x.leg_rate(
                route,
                &link_policy(spec, src, dst),
                &flow_direction,
                src,
                dst,
                &mut target_lines,
            );
            legs.push(Leg {
                tag: tag.to_string(),
                kind: LegKind::CtsTraffic(CtsTrafficTask {
                    v6,
                    udp: false,
                    profile_name: profile_name.clone(),
                    profile_label: profile_label.clone(),
                    comparison_label: profile_label.clone(),
                    src: (*src).clone(),
                    dst: (*dst).clone(),
                    port: x.port(),
                    duration: spec.duration,
                    streams: tcp_streams,
                    window_bytes,
                    bits_per_second: None,
                    datagram_bytes: None,
                    frame_rate: spec.ctstraffic.udp_frame_rate,
                    buffer_depth_secs: spec.ctstraffic.udp_buffer_depth_secs,
                    status_update_ms: spec.ctstraffic.status_update_ms,
                    rate_mode: effective_mode,
                    rx_target_mbps: target,
                    offered_total_mbps: None,
                    setup_error: setup_error.clone(),
                }),
            });
        }
        let title = format!(
            "{}CTS TRAFFIC {} {} | {}",
            if bidir { "★★双向 " } else { "" },
            ip_tag,
            profile_label,
            route_str
        );
        let id = cts_resume_unit_id(spec, ip_tag, dir, &legs);
        let est_secs = cts_estimated_secs(route, &setup_error);
        x.units
            .push(route.unit(id, title, target_lines, legs, est_secs));
    }
}

fn expand_cts_udp(x: &mut Expansion<'_>, route: &Route<'_>, gate: &mut TopologyGate) {
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
    let udp_streams = spec.effective_udp_streams();
    for profile in &spec.udp_profiles {
        let mut setup_errors = cts_task_config_errors(spec, true);
        let window_bytes = match profile.window.as_deref().map(cts_window_bytes).transpose() {
            Ok(value) => value.flatten(),
            Err(error) => {
                setup_errors.push(format!(
                    "CTS UDP socket buffer {:?} 非法: {error}",
                    profile.window.as_deref().unwrap_or_default()
                ));
                None
            }
        };
        let bandwidth = match cts_udp_bandwidth(profile) {
            Ok(value) => Some(value),
            Err(error) => {
                setup_errors.push(error);
                None
            }
        };
        // 发送口可以单独覆盖报文长度，与 iperf 的 `-l` 同一套规则（`udp_leg_profile`）。
        // 按腿解析；两条腿报同一个错只记一次。
        let leg_profiles: Vec<UdpProfile> = pairs
            .iter()
            .map(|(src, dst, _tag)| udp_leg_profile(spec, profile, src, dst))
            .collect();
        let mut leg_datagrams = Vec::with_capacity(leg_profiles.len());
        for leg_profile in &leg_profiles {
            match cts_datagram_bytes(leg_profile) {
                Ok(value) => leg_datagrams.push(value),
                Err(error) => {
                    if !setup_errors.contains(&error) {
                        setup_errors.push(error);
                    }
                    leg_datagrams.push(None);
                }
            }
        }
        let setup_error = (!setup_errors.is_empty()).then(|| setup_errors.join("；"));
        if gate.skips(x, spec, &setup_error) {
            continue;
        }
        if let Some(error) = &setup_error {
            x.notices.push(format!(
                "{} CTS {} 配置非法，将记录 SETUP_ERROR: {error}",
                spec.name,
                profile.label()
            ));
        }
        // 每腿负载与 iperf UDP 同一份规则（`udp_leg_load`）：先降流数，单流仍超过
        // 路径上限才压每流带宽，链路策略明确给了带宽的不裁。配置非法的单元不起
        // 进程，参数按配置原样记下，不参与裁剪。
        let leg_loads: Option<Vec<UdpLoad>> = match (&setup_error, bandwidth) {
            (None, Some(requested)) => Some(
                pairs
                    .iter()
                    .map(|(src, dst, _tag)| udp_leg_load(spec, src, dst, requested, udp_streams))
                    .collect(),
            ),
            _ => None,
        };
        if let Some(loads) = &leg_loads {
            let label = format!("CTS {}", profile.label());
            for ((src, dst, _tag), load) in pairs.iter().zip(loads) {
                if let Some(notice) = udp_clip_notice(&spec.name, &label, src, dst, load) {
                    x.notices.push(notice);
                }
            }
        }
        let mut legs = Vec::new();
        let mut target_lines: Vec<String> = Vec::new();
        let mut max_streams = 1u32;
        for (leg_idx, (src, dst, tag)) in pairs.iter().enumerate() {
            let load = leg_loads.as_ref().map(|loads| loads[leg_idx]);
            let streams = load.map_or(udp_streams, |load| load.streams);
            max_streams = max_streams.max(streams);
            let flow_direction = route.flow_direction(tag);
            let (effective_mode, target) = x.leg_rate(
                route,
                &link_policy(spec, src, dst),
                &flow_direction,
                src,
                dst,
                &mut target_lines,
            );
            // 每流带宽 × 流数 = 整条腿的总量。CTS 侧的
            // 字段是**总量**口径，与 iperf 的每流口径相反。
            let (bits_per_second, per_stream_mbps) = match load {
                Some(load) => (Some(load.bits_per_second), Some(load.mbps)),
                None => (
                    bandwidth.map(|value| value.bits_per_second),
                    bandwidth.map(|value| value.mbps),
                ),
            };
            let offered_total_mbps = per_stream_mbps.map(|mbps| mbps * streams as f64);
            let leg_label = match (load, bandwidth) {
                (Some(load), Some(requested)) => {
                    udp_leg_label(&leg_profiles[leg_idx], &load, requested)
                }
                _ => profile.label(),
            };
            let profile_label = format!(
                "CTS UDP {} ×{}流 (每流)",
                leg_label.trim_start_matches("UDP "),
                streams
            );
            legs.push(Leg {
                tag: tag.to_string(),
                kind: LegKind::CtsTraffic(CtsTrafficTask {
                    v6,
                    udp: true,
                    profile_name: format!("cts_{}_c{}", profile.name(), streams),
                    profile_label,
                    // 对齐键只认档位本身：裁剪与按网口改写都随协商速率或 IP 变化。
                    comparison_label: format!(
                        "CTS UDP {} (每流)",
                        profile.label().trim_start_matches("UDP ")
                    ),
                    src: (*src).clone(),
                    dst: (*dst).clone(),
                    port: x.port(),
                    duration: spec.duration,
                    streams,
                    window_bytes,
                    bits_per_second,
                    datagram_bytes: leg_datagrams[leg_idx],
                    frame_rate: spec.ctstraffic.udp_frame_rate,
                    buffer_depth_secs: spec.ctstraffic.udp_buffer_depth_secs,
                    status_update_ms: spec.ctstraffic.status_update_ms,
                    rate_mode: effective_mode,
                    rx_target_mbps: target,
                    offered_total_mbps,
                    setup_error: setup_error.clone(),
                }),
            });
        }
        let unit_label = match (&leg_loads, bandwidth) {
            (Some(loads), Some(requested)) => {
                udp_unit_label(profile, requested, loads, &leg_profiles)
            }
            _ => profile.label(),
        };
        let title = format!(
            "{}CTS TRAFFIC {} UDP {} ×{}流 | {}",
            if bidir { "★★双向 " } else { "" },
            ip_tag,
            unit_label.trim_start_matches("UDP "),
            max_streams,
            route_str
        );
        let id = cts_resume_unit_id(spec, ip_tag, dir, &legs);
        let est_secs = cts_estimated_secs(route, &setup_error);
        x.units
            .push(route.unit(id, title, target_lines, legs, est_secs));
    }
}

/// 配置非法的单元不起进程，执行器立刻报 SETUP_ERROR，按 1 秒估。
///
/// 进程要多跑起流爬升段，双向合计单元再多跑交集余量（与执行端同一份算法）。
fn cts_estimated_secs(route: &Route<'_>, setup_error: &Option<String>) -> u64 {
    if setup_error.is_some() {
        1
    } else {
        route.single_process_secs().saturating_add(15)
    }
}
