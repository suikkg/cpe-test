use super::*;
use crate::protocol::{PingOut, PingReq};

pub(super) fn run(context: &UnitContext<'_>, unit: &Unit) -> Result<UnitRow, String> {
    let nic = context.cfg.links.get(unit.link).ok_or("Ping 网口不存在")?;
    let host = context
        .capability
        .ok_or("Ping 缺少扫描结果")?
        .host(&nic.host)?;
    let pc = host
        .interfaces
        .iter()
        .find(|n| {
            n.name == nic.local_interface
                && (unit.ip_version == 6 || n.ipv4 == nic.local_ip.to_string())
        })
        .ok_or("Ping 网口已变更，请重新扫描")?;
    let policy = crate::ping::ping_latency_policy(
        pc,
        pc,
        unit.parameters
            .udp_length
            .as_deref()
            .unwrap_or("32")
            .parse()
            .map_err(|_| "Ping 包长无效")?,
        &context
            .cfg
            .ping_policy
            .as_ref()
            .map(|p| p.to_config())
            .unwrap_or_default(),
    );
    let results = std::thread::scope(|scope| {
        let jobs: Vec<_> = unit
            .legs
            .iter()
            .map(|leg| scope.spawn(move || run_leg(context, unit, leg)))
            .collect();
        jobs.into_iter()
            .map(|job| job.join().map_err(|_| "Ping 工作线程异常".to_string())?)
            .collect::<Result<Vec<_>, String>>()
    })?;
    let evaluations: Vec<_> = results
        .iter()
        .map(|ping| {
            crate::ping::acceptance_verdict(
                &ping.result,
                policy,
                crate::ping::execution_error_kind(&ping.result),
            )
        })
        .collect();
    let verdict = crate::verdict::aggregate_verdict(evaluations.iter().copied());
    let reason = evaluations
        .iter()
        .find(|(v, _)| *v == verdict)
        .map(|(_, r)| *r)
        .unwrap_or(crate::reason::ReasonCode::PingExecError);
    let mut row = resumed_row(unit);
    row.resumed = false;
    row.total_mbps = None;
    row.total_target_mbps = None;
    row.verdict = verdict.label().into();
    row.reason = reason.to_string();
    row.detail = format!(
        "Ping 丢包门限 0%；平均 RTT 门限 {} ms；最大 RTT 门限 {} ms",
        policy.avg_rtt_ms, policy.max_rtt_ms
    );
    row.diagnostics = ping_diagnostics(&results);
    row.ping = results;
    if context.cfg.run_overrides.probe_path_mtu {
        for leg in &unit.legs {
            row.diagnostics
                .push(super::diagnostics::path_mtu(context, unit, leg)?);
        }
    }
    Ok(row)
}
/// 每个方向的执行错误，以及读数被工具分辨率卡住时的说明。
///
/// 分辨率说明和子网 `ping_leg` 是同一条规则、同一份文案（`ping::resolution_caveat`）：
/// Windows `ping` 只报整毫秒，`time<1ms` 记成 0，报告上的 `0 ms` 会被当成实测时延读。
/// 只走诊断，不改判定；执行失败的方向没有读数，不加这条。
fn ping_diagnostics(results: &[PingRow]) -> Vec<String> {
    let mut notes: Vec<String> = results
        .iter()
        .filter_map(|ping| crate::ping::execution_error(&ping.result))
        .collect();
    for ping in results {
        if crate::ping::execution_error_kind(&ping.result).is_none() {
            if let Some(note) = crate::ping::resolution_caveat(&ping.result) {
                notes.push(format!("{}：{note}", ping.flow.label()));
            }
        }
    }
    notes
}
fn run_leg(context: &UnitContext<'_>, unit: &Unit, leg: &LegPlan) -> Result<PingRow, String> {
    if context.cancel.load(Ordering::SeqCst) {
        return Err("用户已取消".into());
    }
    let addresses = &context.preflight.addresses;
    let req = PingReq {
        src: if leg.flow == Flow::Up {
            addresses.pc_bind.clone()
        } else {
            addresses.board_bind.clone()
        },
        dst: if leg.flow == Flow::Up {
            addresses.up_target.clone()
        } else {
            addresses.down_target.clone()
        },
        count: unit.parameters.streams,
        payload: unit
            .parameters
            .udp_length
            .as_deref()
            .unwrap_or("32")
            .parse()
            .map_err(|_| "Ping 包长无效")?,
        dont_fragment: false,
        v6: unit.ip_version == 6,
    };
    request(context, unit, leg, req)
}

pub(super) fn request(
    context: &UnitContext<'_>,
    unit: &Unit,
    leg: &LegPlan,
    req: PingReq,
) -> Result<PingRow, String> {
    if context.cancel.load(Ordering::SeqCst) {
        return Err("用户已取消".into());
    }
    let out: PingOut = if leg.flow == Flow::Down {
        let mut args = vec![
            if req.v6 {
                "-6".to_string()
            } else {
                "-4".to_string()
            },
            "-c".into(),
            req.count.to_string(),
            "-I".into(),
            req.src.clone(),
            "-s".into(),
            req.payload.to_string(),
            req.dst.clone(),
        ];
        if req.dont_fragment {
            args.splice(args.len() - 1..args.len() - 1, ["-M".into(), "do".into()]);
        }
        let refs: Vec<_> = args.iter().map(String::as_str).collect();
        let spec = crate::util::ProcessSpec::new("ping", &refs);
        let raw = adb_client::run_command(
            context.adb,
            &crate::util::md5_hex(&format!("{}-ping-{}", context.dir.display(), unit.index)),
            &spec,
            Duration::from_secs(req.count as u64 * 5 + 30),
            context.cancel,
        );
        if !raw.cleanup_confirmed() {
            return Err(format!("板侧 Ping 资源回收未确认：{}", raw.merged()));
        }
        if raw.cancelled {
            return Err("用户已取消".into());
        }
        crate::ping::finish_run(
            format!("ping {}", args.join(" ")),
            req.count,
            Duration::from_secs(req.count as u64 * 5 + 30),
            raw,
        )
    } else if let Some(agent) = &context.agent {
        remote::Remote::new(agent.clone())
            .post_with_timeout(
                "/ping",
                &req,
                Duration::from_secs(req.count as u64 * 5 + 30),
            )
            .unwrap_or_else(|error| {
                crate::ping::finish_run(
                    "agent /ping".into(),
                    req.count,
                    Duration::from_secs(req.count as u64 * 5 + 30),
                    crate::util::CmdOut {
                        ok: false,
                        timed_out: false,
                        cancelled: false,
                        stdout: String::new(),
                        stderr: error,
                    },
                )
            })
    } else {
        let (out, cancelled, cleanup) =
            crate::ping::run_cancellable_with_cleanup(&req, Some(context.cancel));
        if !cleanup {
            return Err(format!("本机 Ping 资源回收未确认：{}", out.raw));
        }
        if cancelled {
            return Err("用户已取消".into());
        }
        out
    };
    Ok(PingRow {
        flow: leg.flow,
        result: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-14 Windows 实机内环：上行是 Windows `ping`（三次 `time<1ms`），
    /// 下行是板侧 BusyBox（三位小数）。只有上行该带分辨率说明。
    #[test]
    fn a_sub_millisecond_windows_reading_is_flagged_but_busybox_is_not() {
        // 和 `ping::finish_run` 一样：解析结果带着原文，执行错误分类读的是原文。
        let parsed = |text: &str| crate::protocol::PingOut {
            raw: text.into(),
            ..crate::ping::parse(text, 3)
        };
        let up = parsed(
            "\r\nPinging 192.168.8.1 from 192.168.8.103 with 32 bytes of data:\r\n\
             Reply from 192.168.8.1: bytes=32 time<1ms TTL=64\r\n\
             Reply from 192.168.8.1: bytes=32 time<1ms TTL=64\r\n\
             Reply from 192.168.8.1: bytes=32 time<1ms TTL=64\r\n\r\n\
             Ping statistics for 192.168.8.1:\r\n\
             \x20   Packets: Sent = 3, Received = 3, Lost = 0 (0% loss),\r\n\
             Approximate round trip times in milli-seconds:\r\n\
             \x20   Minimum = 0ms, Maximum = 0ms, Average = 0ms\r\n",
        );
        let down = parsed(
            "PING 192.168.8.103 (192.168.8.103) from 192.168.8.1: 32 data bytes\r\n\
             40 bytes from 192.168.8.103: seq=0 ttl=128 time=0.740 ms\r\n\
             40 bytes from 192.168.8.103: seq=1 ttl=128 time=0.722 ms\r\n\
             40 bytes from 192.168.8.103: seq=2 ttl=128 time=0.729 ms\r\n\r\n\
             --- 192.168.8.103 ping statistics ---\r\n\
             3 packets transmitted, 3 packets received, 0% packet loss\r\n\
             round-trip min/avg/max = 0.722/0.730/0.740 ms\r\n",
        );
        assert_eq!((up.received, down.received), (3, 3));
        let notes = ping_diagnostics(&[
            PingRow {
                flow: Flow::Up,
                result: up,
            },
            PingRow {
                flow: Flow::Down,
                result: down,
            },
        ]);
        let caveat = crate::ping::resolution_caveat(&crate::protocol::PingOut {
            rtt_min: Some(0.0),
            rtt_avg: Some(0.0),
            rtt_max: Some(0.0),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(notes, vec![format!("上行：{caveat}")]);
    }
}
