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
            x.notices.push(format!(
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
        let est_secs = cts_estimated_secs(spec, &setup_error);
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
        let datagram_bytes = match cts_datagram_bytes(profile) {
            Ok(value) => value,
            Err(error) => {
                setup_errors.push(error);
                None
            }
        };
        let setup_error = (!setup_errors.is_empty()).then(|| setup_errors.join("；"));
        if gate.skips(x, spec, &setup_error) {
            continue;
        }
        if let Some(error) = &setup_error {
            x.notices.push(format!(
                "{} CTS UDP {} 配置非法，将记录 SETUP_ERROR: {error}",
                spec.name,
                profile.label()
            ));
        }
        let mut legs = Vec::new();
        let mut target_lines: Vec<String> = Vec::new();
        let mut max_streams = 1u32;
        for (src, dst, tag) in pairs {
            let streams = if setup_error.is_some() {
                udp_streams
            } else {
                allowed_udp_streams_for_mbps(
                    src,
                    dst,
                    bandwidth.expect("合法 CTS UDP 配置必须有严格带宽值").mbps,
                    udp_streams,
                    spec.udp_limit,
                    &spec.rate_check,
                )
            };
            if streams == 0 {
                x.notices.push(format!(
                    "跳过 {} CTS UDP {}：路径上限不足以承载单流",
                    spec.name,
                    profile.label()
                ));
                legs.clear();
                break;
            }
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
            let offered_total_mbps = bandwidth.map(|value| value.mbps * streams as f64);
            let profile_label = format!(
                "CTS UDP {} ×{}流 (每流)",
                profile.label().trim_start_matches("UDP "),
                streams
            );
            legs.push(Leg {
                tag: tag.to_string(),
                kind: LegKind::CtsTraffic(CtsTrafficTask {
                    v6,
                    udp: true,
                    profile_name: format!("cts_{}_c{}", profile.name(), streams),
                    profile_label,
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
                    bits_per_second: bandwidth.map(|value| value.bits_per_second),
                    datagram_bytes,
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
        if legs.is_empty() {
            continue;
        }
        let title = format!(
            "{}CTS TRAFFIC {} UDP {} ×{}流 | {}",
            if bidir { "★★双向 " } else { "" },
            ip_tag,
            profile.label().trim_start_matches("UDP "),
            max_streams,
            route_str
        );
        let id = cts_resume_unit_id(spec, ip_tag, dir, &legs);
        let est_secs = cts_estimated_secs(spec, &setup_error);
        x.units
            .push(route.unit(id, title, target_lines, legs, est_secs));
    }
}

/// 配置非法的单元不起进程，执行器立刻报 SETUP_ERROR，按 1 秒估。
fn cts_estimated_secs(spec: &SpecNorm, setup_error: &Option<String>) -> u64 {
    if setup_error.is_some() {
        1
    } else {
        spec.duration.saturating_add(15)
    }
}
