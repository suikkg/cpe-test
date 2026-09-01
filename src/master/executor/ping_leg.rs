//! Ping 腿的执行。
//!
//! 和灌包腿共用编排，但判定口径完全不同：Ping 看的是丢包与 RTT，
//! 没有速率目标，也就没有窗口和越界那一整套。

use super::*;

/// 子网 Ping 的默认 RTT 上限。
///
/// 这里测的是 CPE 本地链路，不是公网时延。PASS 必须同时满足零丢包和最大 RTT
/// 不超过该值；用最大值而不是平均值，避免偶发的严重抖动被平均数掩盖。
const PING_MAX_RTT_MS: f64 = 20.0;

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

        // `PingOut.ok` 仍只表达 agent 是否收到了至少一个 Echo Reply，保持 RPC
        // 协议兼容。正式验收在主控这里收紧：必须全部收到，并且最大 RTT 达标。
        let packet_loss_ok = out.sent > 0 && out.received == out.sent;
        let rtt_ok = out
            .rtt_max
            .map(|rtt| rtt.is_finite() && rtt <= PING_MAX_RTT_MS)
            .unwrap_or(false);
        let acceptance_ok = out.ok && packet_loss_ok && rtt_ok;

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
        } else if out.rtt_max.is_none() {
            format!(
                "Ping RTT 数据缺失：收/发={}/{}, 无法按最大 RTT <= {:.1} ms 验收",
                out.received, out.sent, PING_MAX_RTT_MS
            )
        } else if !rtt_ok {
            format!(
                "Ping RTT 超限：最大 RTT={} ms，要求 <= {:.1} ms（最小/平均/最大={}/{}/{} ms）",
                format_ping_rtt(out.rtt_max),
                PING_MAX_RTT_MS,
                format_ping_rtt(out.rtt_min),
                format_ping_rtt(out.rtt_avg),
                format_ping_rtt(out.rtt_max)
            )
        } else {
            format!(
                "Ping 达标：发送/接收={}/{}，丢包率 {:.1}%，RTT 最小/平均/最大={}/{}/{} ms，最大 RTT 门限 {:.1} ms",
                out.sent,
                out.received,
                out.loss_pct,
                format_ping_rtt(out.rtt_min),
                format_ping_rtt(out.rtt_avg),
                format_ping_rtt(out.rtt_max),
                PING_MAX_RTT_MS
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
            PingPurpose::SubnetTest => format!("子网PING（0% 丢包且最大 RTT <= {PING_MAX_RTT_MS:.0}ms）"),
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
            // 展示用的 transport 列对 ping 一直是空的（协议写在标题里），
            // 类型化的 `protocol` 才是 ICMP。这里不借类型化顺手改报告的可见列。
            transport: String::new(),
            // ping 用的是实际解析出来的地址（v6 带 %zone），不是网卡的 ipv4。
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

    // ---------------- ctsTraffic ----------------
}
