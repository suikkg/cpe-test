//! Ping 腿的执行。
//!
//! 和灌包腿共用编排，但判定口径完全不同：Ping 看的是丢包与 RTT，
//! 没有速率目标，也就没有窗口和越界那一整套。

use super::*;

/// Wi-Fi 空口允许正常的竞争/重传抖动，但不能让平均值掩盖严重尖峰。
/// 纯有线链路仍使用 config.json 的 `ping.max_rtt_ms` 严格峰值门限。
const WIFI_AVG_RTT_MS: f64 = 30.0;
const WIFI_MAX_RTT_MS: f64 = 100.0;

#[derive(Debug, Clone, Copy)]
struct PingLatencyPolicy {
    wifi: bool,
    avg_rtt_ms: Option<f64>,
    max_rtt_ms: f64,
}

fn nic_looks_wifi(nic: &NicInfo) -> bool {
    if nic.is_wifi || !nic.wifi_band.trim().is_empty() {
        return true;
    }
    // 兼容旧 agent：旧版协议没有 is_wifi/wifi_band 时 serde 会补默认值，
    // 继续从角色、接口名和描述识别，避免把旧辅测机的 Wi-Fi 误按有线验收。
    let role = nic.role.to_ascii_lowercase();
    let name = nic.name.to_ascii_lowercase();
    let description = nic.description.to_ascii_lowercase();
    role.contains("wifi")
        || name.contains("wi-fi")
        || name.contains("wifi")
        || name.contains("wlan")
        || description.contains("wi-fi")
        || description.contains("wifi")
        || description.contains("wireless")
}

fn ping_latency_policy(src: &NicInfo, dst: &NicInfo, wired_max_rtt_ms: f64) -> PingLatencyPolicy {
    let wifi = nic_looks_wifi(src) || nic_looks_wifi(dst);
    if wifi {
        PingLatencyPolicy {
            wifi: true,
            avg_rtt_ms: Some(WIFI_AVG_RTT_MS),
            max_rtt_ms: WIFI_MAX_RTT_MS,
        }
    } else {
        PingLatencyPolicy {
            wifi: false,
            avg_rtt_ms: None,
            max_rtt_ms: wired_max_rtt_ms,
        }
    }
}

fn ping_acceptance(out: &PingOut, policy: PingLatencyPolicy) -> bool {
    let avg_ok = policy.avg_rtt_ms.is_none_or(|limit| {
        out.rtt_avg
            .is_some_and(|rtt| rtt.is_finite() && rtt <= limit)
    });
    out.ok
        && out.sent > 0
        && out.received == out.sent
        && avg_ok
        && out
            .rtt_max
            .is_some_and(|rtt| rtt.is_finite() && rtt <= policy.max_rtt_ms)
}

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
        let latency_policy =
            ping_latency_policy(&t.src.nic, &t.dst.nic, self.cfg.ping.max_rtt_ms);
        let max_rtt_ms = latency_policy.max_rtt_ms;
        let avg_rtt_ms = latency_policy.avg_rtt_ms;
        let (src_addr, dst_addr) = if t.v6 {
            match v6_addrs(&t.src.nic, &t.dst.nic) {
                Some(v) => {
                    let bind = add_zone(&v.client_bind, &t.src.nic.zone, t.src.side);
                    let target = add_zone(&v.client_target, &t.src.nic.zone, t.src.side);
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
        let avg_rtt_ok = avg_rtt_ms.is_none_or(|limit| {
            out.rtt_avg
                .is_some_and(|rtt| rtt.is_finite() && rtt <= limit)
        });
        let max_rtt_ok = out
            .rtt_max
            .is_some_and(|rtt| rtt.is_finite() && rtt <= max_rtt_ms);
        let acceptance_ok = ping_acceptance(&out, latency_policy);

        let verdict = if gateway_missing {
            Verdict::NotEvaluated
        } else if exec_kind.is_some() {
            Verdict::SetupError
        } else if acceptance_ok {
            Verdict::Pass
        } else {
            Verdict::RateFail
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
        } else if exec_kind == Some(ping::PingExecErrorKind::Timeout) {
            ReasonCode::PingTimeout
        } else if exec_kind.is_some() {
            ReasonCode::PingExecError
        } else if acceptance_ok {
            ReasonCode::PingOk
        } else {
            match t.purpose {
                PingPurpose::SubnetTest => ReasonCode::PingUnreachable,
                PingPurpose::SubnetDiagnostic => ReasonCode::PingSubnetUnreachable,
                PingPurpose::GatewayDiagnostic => ReasonCode::PingGatewayUnreachable,
            }
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
        } else if latency_policy.wifi && out.rtt_avg.is_none() {
            format!(
                "Wi-Fi Ping RTT 平均值缺失：收/发={}/{}, 无法按平均 RTT <= {:.1} ms 验收",
                out.received,
                out.sent,
                avg_rtt_ms.unwrap_or_default()
            )
        } else if out.rtt_max.is_none() {
            format!(
                "Ping RTT 最大值缺失：收/发={}/{}, 无法按最大 RTT <= {:.1} ms 验收",
                out.received, out.sent, max_rtt_ms
            )
        } else if !avg_rtt_ok {
            format!(
                "Wi-Fi Ping 平均 RTT 超限：平均 RTT={} ms，要求 <= {:.1} ms；最大 RTT={} ms（上限 {:.1} ms）",
                format_ping_rtt(out.rtt_avg),
                avg_rtt_ms.unwrap_or_default(),
                format_ping_rtt(out.rtt_max),
                max_rtt_ms
            )
        } else if !max_rtt_ok {
            format!(
                "Ping RTT 峰值超限：最大 RTT={} ms，要求 <= {:.1} ms（最小/平均/最大={}/{}/{} ms）",
                format_ping_rtt(out.rtt_max),
                max_rtt_ms,
                format_ping_rtt(out.rtt_min),
                format_ping_rtt(out.rtt_avg),
                format_ping_rtt(out.rtt_max)
            )
        } else if latency_policy.wifi {
            format!(
                "Wi-Fi Ping 达标：发送/接收={}/{}，丢包率 {:.1}%，RTT 最小/平均/最大={}/{}/{} ms；平均 <= {:.1} ms 且最大 <= {:.1} ms",
                out.sent,
                out.received,
                out.loss_pct,
                format_ping_rtt(out.rtt_min),
                format_ping_rtt(out.rtt_avg),
                format_ping_rtt(out.rtt_max),
                avg_rtt_ms.unwrap_or_default(),
                max_rtt_ms
            )
        } else {
            format!(
                "有线 Ping 达标：发送/接收={}/{}，丢包率 {:.1}%，RTT 最小/平均/最大={}/{}/{} ms；最大 RTT <= {:.1} ms",
                out.sent,
                out.received,
                out.loss_pct,
                format_ping_rtt(out.rtt_min),
                format_ping_rtt(out.rtt_avg),
                format_ping_rtt(out.rtt_max),
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
        let kind_label = match t.purpose {
            PingPurpose::SubnetTest if unit.bidir => format!("★双向子网PING-{tag}"),
            PingPurpose::SubnetTest if latency_policy.wifi => format!(
                "子网PING（Wi-Fi：0% 丢包，平均 RTT <= {:.0}ms，最大 RTT <= {:.0}ms）",
                avg_rtt_ms.unwrap_or_default(),
                max_rtt_ms
            ),
            PingPurpose::SubnetTest => {
                format!("子网PING（有线：0% 丢包且最大 RTT <= {max_rtt_ms:.0}ms）")
            }
            PingPurpose::SubnetDiagnostic => "故障诊断-子网PING".into(),
            PingPurpose::GatewayDiagnostic => "故障诊断-网卡到网关PING".into(),
        };
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(received: u32, avg: Option<f64>, max: Option<f64>) -> PingOut {
        PingOut {
            ok: received > 0,
            sent: 180,
            received,
            lost: 180 - received,
            loss_pct: (180 - received) as f64 / 1.8,
            rtt_min: Some(2.0),
            rtt_avg: avg,
            rtt_max: max,
            ..Default::default()
        }
    }

    #[test]
    fn wired_ping_uses_the_strict_peak_gate() {
        let nic = NicInfo::default();
        let policy = ping_latency_policy(&nic, &nic, 20.0);
        assert!(!policy.wifi);
        assert!(ping_acceptance(
            &out(180, Some(5.0), Some(20.0)),
            policy
        ));
        assert!(!ping_acceptance(
            &out(180, Some(5.0), Some(20.1)),
            policy
        ));
    }

    #[test]
    fn wifi_ping_requires_zero_loss_good_average_and_a_bounded_spike() {
        let wifi = NicInfo {
            is_wifi: true,
            ..Default::default()
        };
        let wired = NicInfo::default();
        let policy = ping_latency_policy(&wifi, &wired, 20.0);
        assert!(policy.wifi);
        assert!(ping_acceptance(
            &out(180, Some(30.0), Some(100.0)),
            policy
        ));
        assert!(!ping_acceptance(
            &out(179, Some(5.0), Some(10.0)),
            policy
        ));
        assert!(!ping_acceptance(
            &out(180, Some(30.1), Some(80.0)),
            policy
        ));
        assert!(!ping_acceptance(
            &out(180, Some(10.0), Some(100.1)),
            policy
        ));
        assert!(!ping_acceptance(&out(180, None, Some(20.0)), policy));
    }

    #[test]
    fn old_agent_wifi_metadata_is_still_recognized() {
        for nic in [
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
            assert!(
                nic_looks_wifi(&nic),
                "旧 agent 的 Wi-Fi 不该被当成有线: {nic:?}"
            );
        }
    }
}
