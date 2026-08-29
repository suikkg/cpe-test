//! Ping 腿的执行。
//!
//! 和灌包腿共用编排，但判定口径完全不同：Ping 看的是通不通和 RTT 分布，
//! 没有速率目标，也就没有窗口和越界那一整套。

use super::*;

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
        let verdict = if gateway_missing {
            Verdict::NotEvaluated
        } else if exec_kind.is_some() {
            Verdict::SetupError
        } else if out.ok {
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
        } else if out.ok {
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
        } else if out.ok {
            format!(
                "Ping 连通：发送/接收={}/{}，丢包率 {:.1}%，RTT 最小/平均/最大={}/{}/{} ms",
                out.sent,
                out.received,
                out.loss_pct,
                format_ping_rtt(out.rtt_min),
                format_ping_rtt(out.rtt_avg),
                format_ping_rtt(out.rtt_max)
            )
        } else {
            format!(
                "Ping 命令正常完成，但未收到目标 Echo Reply（收/发={}/{}，丢包率 {:.1}%）",
                out.received, out.sent, out.loss_pct
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
            PingPurpose::SubnetTest => "子网PING（收到至少一个 Echo Reply 即连通）".into(),
            PingPurpose::SubnetDiagnostic => "故障诊断-子网PING".into(),
            PingPurpose::GatewayDiagnostic => "故障诊断-网卡到网关PING".into(),
        };
        let raw_text = if out.cmd.is_empty() {
            out.raw.clone()
        } else {
            format!("$ {}\n{}", out.cmd, out.raw)
        };
        let idx = self.push_row(Row {
            sort_key: (useq, lidx, 0, 0),
            time,
            task_id: md5_hex(&format!("{}|{}|ping", unit.id, tag)),
            parent_id: unit.id.clone(),
            task: unit.title.clone(),
            ip: if t.v6 { "V6".into() } else { "V4".into() },
            transport: String::new(),
            param: format!("-l {}", t.payload),
            src_pc: t.src.pc.clone(),
            src_iface: t.src.nic.name.clone(),
            src_ip: src_addr,
            dst_pc: t.dst.pc.clone(),
            dst_iface: t.dst.nic.name.clone(),
            dst_ip: dst_addr,
            verdict,
            execution_status,
            reason_code,
            reason_detail: reason_detail.clone(),
            kind_label,
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
            ..Default::default()
        });
        LegOutcome {
            judgement: VerdictResult::new(verdict, reason_code, reason_detail),
            rx_avg: None,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    // ---------------- ctsTraffic ----------------
}
