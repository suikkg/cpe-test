//! iperf TCP：每个 `-w` 档位一个单元；每条腿一个 client，多流走 `-P`。
use super::*;

/// 一个「规格 × 方向 × IP 版本」组合下的全部 iperf TCP 单元。
pub(super) fn expand_iperf_tcp(x: &mut Expansion<'_>, route: &Route<'_>) {
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
    if let Some(error) = spec.stream_config_error(false) {
        x.notices.push(format!(
            "{} 的 iperf TCP 流数配置非法，将按兼容范围使用 {} 流: {error}",
            spec.name,
            spec.effective_tcp_streams()
        ));
    }
    let tcp_streams = spec.effective_tcp_streams();
    // 空的 -w 档位列表 = 跑一条不带 -w 的 TCP（附加 TCP
    // 参数组把 -w 留空时会这样）。默认组经过 non_empty
    // 兜底、老配置也总有窗口，都不会走到 None 这一支，
    // 行为与从前逐字一致。
    let windows: Vec<Option<&String>> = if spec.tcp_windows.is_empty() {
        vec![None]
    } else {
        spec.tcp_windows.iter().map(Some).collect()
    };
    for w in windows {
        let (pname, plabel) = match w {
            Some(w) => (
                format!("tcp_w{}_P{}", w, tcp_streams),
                format!("TCP -w {} -P {}", w, tcp_streams),
            ),
            None => (
                format!("tcp_noW_P{}", tcp_streams),
                format!("TCP -P {}", tcp_streams),
            ),
        };
        if let Some(w) = w {
            for (s, d, _tag) in pairs {
                if let Some(msg) = oversized_socket_buffer_notice(
                    &spec.name,
                    &plabel,
                    w,
                    tcp_streams,
                    spec.duration,
                    s,
                    d,
                    &spec.rate_check,
                ) {
                    x.notices.push(msg);
                }
            }
        }
        let mut legs = Vec::new();
        let mut target_lines: Vec<String> = Vec::new();
        for (s, d, tag) in pairs {
            let flow_direction = route.flow_direction(tag);
            let leg_policy = link_policy(spec, s, d);
            x.note_rx_target(&spec.name, &leg_policy);
            let (effective_mode, target) =
                x.leg_rate(route, &leg_policy, &flow_direction, s, d, &mut target_lines);
            let t = IperfTask {
                v6,
                udp: false,
                profile_name: pname.clone(),
                profile_label: plabel.clone(),
                comparison_label: plabel.clone(),
                src: (*s).clone(),
                dst: (*d).clone(),
                port: x.port(),
                duration: spec.duration,
                extra: match w {
                    Some(w) => vec!["-w".into(), w.clone(), "-P".into(), tcp_streams.to_string()],
                    None => {
                        vec!["-P".into(), tcp_streams.to_string()]
                    }
                },
                stream_idx: 0,
                rate_mode: effective_mode,
                rx_target_mbps: target,
                offered_per_stream_mbps: None,
            };
            legs.push(Leg {
                tag: tag.to_string(),
                kind: LegKind::IperfSingle(t),
            });
        }
        let title = format!(
            "{}IPERF {} {} | {}",
            if bidir { "★★双向 " } else { "" },
            ip_tag,
            plabel,
            route_str
        );
        let id = tcp_resume_unit_id_v2(spec, ip_tag, dir, &pname, &legs);
        x.units
            .push(route.unit(id, title, target_lines, legs, spec.duration + 10));
    }
}
