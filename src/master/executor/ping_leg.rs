//! Ping 腿的执行。
//!
//! 和灌包腿共用编排，但判定口径完全不同：Ping 看的是丢包与 RTT，
//! 没有速率目标，也就没有窗口和越界那一整套。

use super::*;

#[cfg(test)]
use crate::ping::ping_acceptance;
use crate::ping::ping_latency_policy;
#[cfg(test)]
use crate::ping::{classify_ping_payload, nic_looks_wifi, PingPayloadClass};

impl Ctx {
    pub(super) fn run_ping_leg(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        tag: &str,
        t: &PingTask,
    ) -> LegOutcome {
        let time = now_full();
        let latency_policy = ping_latency_policy(&t.src.nic, &t.dst.nic, t.payload, &self.cfg.ping);
        let max_rtt_ms = latency_policy.max_rtt_ms;
        let avg_rtt_ms = latency_policy.avg_rtt_ms;
        let medium = if latency_policy.wifi {
            "Wi-Fi"
        } else {
            "有线"
        };
        let class = latency_policy.class.label();
        let (src_addr, dst_addr) = if t.v6 {
            match v6_addrs(&t.src.nic, &t.dst.nic) {
                Some(v) => {
                    let bind = self.add_zone(&v.client_bind, &t.src.nic.zone, t.src.side);
                    let target = self.add_zone(&v.client_target, &t.src.nic.zone, t.src.side);
                    (bind, target)
                }
                None => (String::new(), String::new()),
            }
        } else {
            (t.src.nic.ipv4.clone(), t.dst.nic.ipv4.clone())
        };
        let req = PingReq {
            dst: dst_addr.clone(),
            src: src_addr.clone(),
            count: t.count,
            payload: t.payload,
            v6: t.v6,
            // 常规 ping 不带 DF：它测的是连通性和时延，分片与否不是结论。
            // 路径 MTU 探测走 `run_path_mtu_leg`，那条路自己带。
            dont_fragment: false,
        };
        let gateway_missing =
            t.purpose == PingPurpose::GatewayDiagnostic && dst_addr.trim().is_empty();
        if gateway_missing {
            logln(&format!(
                "  [ping{}] {} 未发现 IPv4 网关，无法执行绑定源地址的网关诊断。",
                fmt_tag(tag),
                src_addr
            ));
        } else {
            logln(&format!(
                "  [ping{}] {} -> {} (n={}, -l {}) 执行中...",
                fmt_tag(tag),
                src_addr,
                dst_addr,
                t.count,
                t.payload
            ));
        }
        let (out, transport_error) = if gateway_missing {
            (
                PingOut {
                    ok: false,
                    sent: 0,
                    received: 0,
                    lost: 0,
                    loss_pct: 0.0,
                    raw: "未发现该网卡的 IPv4 默认网关，未发送 Ping。".into(),
                    ..Default::default()
                },
                None,
            )
        } else {
            match self.ping_at(t.src.side, &req) {
                Ok(out) => (out, None),
                Err(error) => (
                    PingOut {
                        ok: false,
                        raw: format!("辅测机 Ping 请求执行失败: {error}"),
                        ..Default::default()
                    },
                    Some(error),
                ),
            }
        };
        let exec_kind = if transport_error.is_some() {
            Some(ping::PingExecErrorKind::Execution)
        } else if gateway_missing {
            None
        } else {
            ping::execution_error_kind(&out)
        };
        let exec_detail = transport_error.or_else(|| ping::execution_error(&out));

        let packet_loss_ok = out.sent > 0 && out.received == out.sent;
        let avg_rtt_ok = out
            .rtt_avg
            .is_some_and(|rtt| rtt.is_finite() && rtt <= avg_rtt_ms);
        let max_rtt_ok = out
            .rtt_max
            .is_some_and(|rtt| rtt.is_finite() && rtt <= max_rtt_ms);
        let (ping_verdict, ping_reason) = ping::acceptance_verdict(&out, latency_policy, exec_kind);
        let verdict = if gateway_missing {
            Verdict::NotEvaluated
        } else {
            ping_verdict
        };
        let execution_status = if gateway_missing {
            ExecutionStatus::Partial
        } else {
            match exec_kind {
                Some(ping::PingExecErrorKind::Timeout) => ExecutionStatus::TimedOut,
                Some(_) => ExecutionStatus::Error,
                None => ExecutionStatus::Completed,
            }
        };
        let reason_code = if gateway_missing {
            ReasonCode::GatewayNotFound
        } else if ping_reason == ReasonCode::PingUnreachable {
            match t.purpose {
                PingPurpose::SubnetTest => ping_reason,
                PingPurpose::SubnetDiagnostic => ReasonCode::PingSubnetUnreachable,
                PingPurpose::GatewayDiagnostic => ReasonCode::PingGatewayUnreachable,
            }
        } else {
            ping_reason
        };
        let reason_detail = if gateway_missing {
            format!(
                "网卡 {}({}) 没有发现 IPv4 默认网关；无法用网关 Ping 判断该网卡/载体状态",
                t.src.nic.name, t.src.nic.ipv4
            )
        } else if let Some(detail) = exec_detail {
            detail
        } else if !out.ok {
            format!(
                "Ping 命令正常完成，但未收到目标 Echo Reply（收/发={}/{}，丢包率 {:.1}%）",
                out.received, out.sent, out.loss_pct
            )
        } else if !packet_loss_ok {
            format!(
                "Ping 丢包不达标：要求 0% 丢包，实际收/发={}/{}, 丢包率 {:.1}%",
                out.received, out.sent, out.loss_pct
            )
        } else if out.rtt_avg.is_none() {
            format!(
                "Ping RTT 平均值缺失：{medium}/{class} 档，收/发={}/{}, 无法按平均 RTT <= {:.1} ms 验收",
                out.received, out.sent, avg_rtt_ms
            )
        } else if out.rtt_max.is_none() {
            format!(
                "Ping RTT 最大值缺失：{medium}/{class} 档，收/发={}/{}, 无法按最大 RTT <= {:.1} ms 验收",
                out.received, out.sent, max_rtt_ms
            )
        } else if !avg_rtt_ok {
            format!(
                "Ping 平均 RTT 超限：{medium}/{class} 档，平均 RTT={} ms，要求 <= {:.1} ms；最大 RTT={} ms（上限 {:.1} ms）",
                format_ping_rtt(out.rtt_avg),
                avg_rtt_ms,
                format_ping_rtt(out.rtt_max),
                max_rtt_ms
            )
        } else if !max_rtt_ok {
            format!(
                "Ping RTT 峰值超限：{medium}/{class} 档，最大 RTT={} ms，要求 <= {:.1} ms（最小/平均/最大={}/{}/{} ms）",
                format_ping_rtt(out.rtt_max),
                max_rtt_ms,
                format_ping_rtt(out.rtt_min),
                format_ping_rtt(out.rtt_avg),
                format_ping_rtt(out.rtt_max)
            )
        } else {
            format!(
                "Ping 达标：{medium}/{class} 档，发送/接收={}/{}，丢包率 {:.1}%，RTT 最小/平均/最大={}/{}/{} ms；平均 <= {:.1} ms 且最大 <= {:.1} ms",
                out.sent,
                out.received,
                out.loss_pct,
                format_ping_rtt(out.rtt_min),
                format_ping_rtt(out.rtt_avg),
                format_ping_rtt(out.rtt_max),
                avg_rtt_ms,
                max_rtt_ms
            )
        };
        logln(&format!(
            "    结果: {} 收/发={}/{} 丢包={} 平均={}ms{}",
            verdict.label(),
            out.received,
            out.sent,
            if gateway_missing || exec_kind.is_some() {
                "-".into()
            } else {
                format!("{:.1}%", out.loss_pct)
            },
            out.rtt_avg
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            if reason_detail.is_empty() {
                String::new()
            } else {
                format!(" ({reason_detail})")
            }
        ));
        let criteria = format!(
            "{medium}/{class}：0% 丢包，平均 RTT <= {:.0}ms，最大 RTT <= {:.0}ms",
            avg_rtt_ms, max_rtt_ms
        );
        let kind_label = match t.purpose {
            PingPurpose::SubnetTest if unit.bidir => format!("★双向子网PING-{tag}（{criteria}）"),
            PingPurpose::SubnetTest => format!("子网PING（{criteria}）"),
            PingPurpose::SubnetDiagnostic => "故障诊断-子网PING".into(),
            PingPurpose::GatewayDiagnostic => "故障诊断-网卡到网关PING".into(),
        };
        // 路径 MTU 探测挂在常规 ping 之后：它自己要发十几轮小包，和上面那次
        // ping 抢不到什么。只在开关打开、且这一腿真的跑起来了的时候做——
        // 一条压根不通的链路上再二分一遍毫无意义。
        let path_mtu_note: Option<String> = (self.cfg.ping.probe_path_mtu
            && !gateway_missing
            && exec_kind.is_none()
            && out.received > 0)
            .then(|| {
                match self.probe_path_mtu(&t.src, &t.dst, t.v6, self.agent_ping_df) {
                    Ok(found) => format!(
                        "路径 MTU {} 字节（DF 位实测最大载荷 {}，{} 轮二分）。\
                         **只作诊断**：要不要按它判合格由验收标准决定。",
                        found.mtu, found.payload, found.steps
                    ),
                    // 探测失败也要说出来：静默跳过的话，打开了开关却看不到结果，
                    // 分不清是「没测」还是「测了但这条链路正常」。
                    Err(error) => format!("路径 MTU 探测未完成：{error}"),
                }
            });

        let raw_text = if out.cmd.is_empty() {
            out.raw.clone()
        } else {
            format!("$ {}\n{}", out.cmd, out.raw)
        };
        let idx = self.push_row(Row {
            time,
            transport: String::new(),
            src_ip: src_addr,
            dst_ip: dst_addr,
            verdict,
            execution_status,
            reason_code,
            reason_detail: reason_detail.clone(),
            ping_loss: (!gateway_missing && exec_kind.is_none()).then_some(out.loss_pct),
            ping_min: (!gateway_missing && exec_kind.is_none())
                .then_some(out.rtt_min)
                .flatten(),
            ping_avg: (!gateway_missing && exec_kind.is_none())
                .then_some(out.rtt_avg)
                .flatten(),
            ping_max: (!gateway_missing && exec_kind.is_none())
                .then_some(out.rtt_max)
                .flatten(),
            // 两条诊断都走**诊断**通道，一条都不改判定（ADR-17）：
            //
            // - 分辨率警告说的是「这个读数能不能当时延用」。不说的话，
            //   报告上那个 `0.000 ms` 会被当成实测结果读。
            // - 路径 MTU 说的是「这条链路真正能过多大的包」。要不要拿 1492
            //   当不合格是业务判断，不是这个工具该替人下的结论。
            diagnostics: {
                let mut notes: Vec<String> = Vec::new();
                if !gateway_missing && exec_kind.is_none() {
                    if let Some(note) = crate::ping::resolution_caveat(&out) {
                        notes.push(note.to_string());
                    }
                }
                notes.extend(path_mtu_note);
                notes
            },
            command: out.cmd.clone(),
            raws: vec![(format!("ping{} 输出", fmt_tag(tag)), raw_text)],
            ..base_row(RowIdentity {
                unit_seq: useq,
                leg_index: lidx,
                stream_index: 0,
                group_flag: 0,
                unit,
                leg_tag: tag,
                src: &t.src,
                dst: &t.dst,
                ip: if t.v6 { "V6".into() } else { "V4".into() },
                protocol: RowProtocol::Icmp,
                backend: RowBackend::Ping,
                param: format!("-l {}", t.payload),
                kind_label,
                task_id: md5_hex(&format!("{}|{}|ping", unit.id, tag)),
            })
        });
        LegOutcome {
            judgement: VerdictResult::new(verdict, reason_code, reason_detail),
            rx_avg: None,
            main_rows: vec![idx],
            tag: tag.to_string(),
            traffic: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn out(received: u32, avg: f64, max: f64) -> PingOut {
        PingOut {
            ok: received > 0,
            sent: 180,
            received,
            lost: 180 - received,
            loss_pct: (180 - received) as f64 / 1.8,
            rtt_min: Some(2.0),
            rtt_avg: Some(avg),
            rtt_max: Some(max),
            ..Default::default()
        }
    }
    #[test]
    fn arbitrary_payloads_use_ranges() {
        let c = crate::config::PingCfg::default();
        assert_eq!(classify_ping_payload(&c, 32), PingPayloadClass::Small);
        assert_eq!(classify_ping_payload(&c, 128), PingPayloadClass::Small);
        assert_eq!(classify_ping_payload(&c, 129), PingPayloadClass::Medium);
        assert_eq!(classify_ping_payload(&c, 1472), PingPayloadClass::Medium);
        assert_eq!(classify_ping_payload(&c, 2000), PingPayloadClass::Medium);
        assert_eq!(classify_ping_payload(&c, 2001), PingPayloadClass::Large);
        assert_eq!(classify_ping_payload(&c, 65500), PingPayloadClass::Large);
    }
    #[test]
    fn wired_thresholds_scale_by_class() {
        let c = crate::config::PingCfg::default();
        let n = NicInfo::default();
        for (p, a, m) in [(32, 10.0, 30.0), (1600, 20.0, 50.0), (65500, 50.0, 100.0)] {
            let x = ping_latency_policy(&n, &n, p, &c);
            assert!(ping_acceptance(&out(180, a, m), x));
            assert!(!ping_acceptance(&out(180, a + 0.1, m), x));
            assert!(!ping_acceptance(&out(180, a, m + 0.1), x));
        }
    }
    #[test]
    fn wifi_thresholds_scale_by_class_and_require_zero_loss() {
        let c = crate::config::PingCfg::default();
        let w = NicInfo {
            is_wifi: true,
            ..Default::default()
        };
        let n = NicInfo::default();
        for (p, a, m) in [(32, 30.0, 80.0), (1600, 50.0, 100.0), (65500, 100.0, 200.0)] {
            let x = ping_latency_policy(&w, &n, p, &c);
            assert!(ping_acceptance(&out(180, a, m), x));
            assert!(!ping_acceptance(&out(179, 1.0, 2.0), x));
        }
    }
    #[test]
    fn old_agent_wifi_metadata_is_recognized() {
        for n in [
            NicInfo {
                role: "WIFI5G".into(),
                ..Default::default()
            },
            NicInfo {
                name: "WLAN 3".into(),
                ..Default::default()
            },
            NicInfo {
                description: "Intel Wireless Adapter".into(),
                ..Default::default()
            },
        ] {
            assert!(nic_looks_wifi(&n));
        }
    }
}
