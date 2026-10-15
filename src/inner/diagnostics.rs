//! 可选诊断只写测量旁证，绝不覆盖接收速率判定。
use super::*;
use crate::protocol::{PingOut, PingReq};

pub(super) fn request(
    context: &UnitContext<'_>,
    unit: &Unit,
    leg: &LegPlan,
    count: u32,
    payload: u32,
    df: bool,
) -> Result<PingOut, String> {
    let a = &context.preflight.addresses;
    let req = PingReq {
        src: if leg.flow == Flow::Up {
            a.pc_bind.clone()
        } else {
            a.board_bind.clone()
        },
        dst: if leg.flow == Flow::Up {
            a.up_target.clone()
        } else {
            a.down_target.clone()
        },
        count,
        payload,
        dont_fragment: df,
        v6: unit.ip_version == 6,
    };
    Ok(ping_unit::request(context, unit, leg, req)?.result)
}
pub(super) type ClientRun = (IperfClientOut, Vec<IperfFlowEvent>, Vec<String>);
struct Stop<'a>(&'a AtomicBool);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub(super) fn with_load_latency<F>(
    context: &UnitContext<'_>,
    unit: &Unit,
    leg: &LegPlan,
    work: F,
) -> Result<ClientRun, String>
where
    F: FnOnce(&AtomicBool) -> Result<(IperfClientOut, Vec<IperfFlowEvent>), String>,
{
    let traffic = AtomicBool::new(false);
    if !context.cfg.run_overrides.probe_during_traffic || unit.duration_secs < 5 {
        let (out, events) = work(&traffic)?;
        return Ok((out, events, Vec::new()));
    }
    let stopped = AtomicBool::new(context.cancel.load(Ordering::SeqCst));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stopped.load(Ordering::SeqCst) {
                if context.cancel.load(Ordering::SeqCst) {
                    stopped.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let probe_context = UnitContext {
            cancel: &stopped,
            ..context.clone()
        };
        let traffic_ref = &traffic;
        let probe = scope.spawn(move || load_latency(&probe_context, unit, leg, traffic_ref));
        let finish = Stop(&stopped);
        let result = work(&traffic);
        drop(finish);
        let diagnostics = probe
            .join()
            .map_err(|_| "负载时延探测线程异常".to_string())??;
        let (out, events) = result?;
        Ok((out, events, diagnostics.into_iter().collect()))
    })
}
fn load_latency(
    context: &UnitContext<'_>,
    unit: &Unit,
    leg: &LegPlan,
    traffic: &AtomicBool,
) -> Result<Option<String>, String> {
    let started = Instant::now();
    while !traffic.load(Ordering::SeqCst) {
        if context.cancel.load(Ordering::SeqCst) || started.elapsed() >= Duration::from_secs(120) {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut remaining = unit.duration_secs.min(3600) as u32;
    let (mut sent, mut received, mut avg_weight) = (0_u32, 0_u32, 0_u32);
    let mut weighted = 0.0;
    let mut max: Option<f64> = None;
    while remaining > 0 && !context.cancel.load(Ordering::SeqCst) {
        let count = remaining.min(10);
        let out = match request(context, unit, leg, count, 32, false) {
            Ok(out) => out,
            Err(error) if error == "用户已取消" => break,
            Err(error) => return Err(error),
        };
        if let Some(error) = crate::ping::execution_error(&out) {
            return Ok(Some(format!(
                "{}负载时延未取得有效数据：{error}",
                leg.flow.label()
            )));
        }
        sent = sent.saturating_add(out.sent);
        received = received.saturating_add(out.received);
        if let Some(avg) = out.rtt_avg.filter(|v| v.is_finite()) {
            weighted += avg * out.received as f64;
            avg_weight = avg_weight.saturating_add(out.received);
        }
        if let Some(value) = out.rtt_max.filter(|v| v.is_finite()) {
            max = Some(max.map_or(value, |m| m.max(value)));
        }
        remaining -= count;
    }
    if sent == 0 {
        return Ok(None);
    }
    Ok(Some(format!(
        "{}负载时延：平均 {} ms，最大 {} ms，丢包 {:.1}%（{received}/{sent}）；仅诊断",
        leg.flow.label(),
        if avg_weight > 0 {
            format!("{:.1}", weighted / avg_weight as f64)
        } else {
            "未知".into()
        },
        max.map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "未知".into()),
        sent.saturating_sub(received) as f64 / sent as f64 * 100.0
    )))
}

pub(super) fn path_mtu(
    context: &UnitContext<'_>,
    unit: &Unit,
    leg: &LegPlan,
) -> Result<String, String> {
    if unit.ip_version == 6 {
        return Ok(format!("{}路径 MTU：IPv6 未探测", leg.flow.label()));
    }
    if leg.flow == Flow::Up {
        if let Some(agent) = &context.agent {
            let supported = match remote::Remote::new(agent.clone()).supports_ping_df() {
                Ok(value) => value,
                Err(error) => {
                    return Ok(format!("{}路径 MTU 能力未确认：{error}", leg.flow.label()))
                }
            };
            if !supported {
                return Ok(format!("{}路径 MTU：辅测机缺少 DF 能力", leg.flow.label()));
            }
        }
    } else {
        let spec = crate::util::ProcessSpec::new("ping", &["-h"]);
        let out = adb_client::run_command(
            context.adb,
            &crate::util::md5_hex(&format!("{}-df-help-{}", context.dir.display(), unit.index)),
            &spec,
            Duration::from_secs(10),
            context.cancel,
        );
        if !out.cleanup_confirmed() {
            return Err(format!("板侧 Ping 能力检查回收未确认：{}", out.merged()));
        }
        if out.cancelled {
            return Err("用户已取消".into());
        }
        let help = out.merged();
        if !help.contains("-M") || !help.contains("do") {
            return Ok(format!(
                "{}路径 MTU：板侧 Ping 缺少 DF 能力",
                leg.flow.label()
            ));
        }
    }
    let mut error = None;
    let mut fatal = None;
    let found = crate::ping::payload_search(0, 1472, |payload| {
        if error.is_some() || fatal.is_some() || context.cancel.load(Ordering::SeqCst) {
            return false;
        }
        match request(context, unit, leg, 3, payload, true) {
            Ok(out) => {
                if let Some(detail) = crate::ping::execution_error(&out) {
                    error = Some(detail);
                    false
                } else {
                    out.received > 0
                }
            }
            Err(detail) => {
                fatal = Some(detail);
                false
            }
        }
    });
    if context.cancel.load(Ordering::SeqCst) {
        return Err("用户已取消".into());
    }
    if let Some(error) = fatal {
        return Err(error);
    }
    if let Some(error) = error {
        return Ok(format!("{}路径 MTU 未探测成功：{error}", leg.flow.label()));
    }
    Ok(match found {
        Some((payload, steps)) => format!(
            "{}路径 MTU：{} 字节（ICMP 载荷 {payload} 字节，{steps} 轮）；仅诊断",
            leg.flow.label(),
            payload + 28
        ),
        None => format!("{}路径 MTU：无有效 DF 回应", leg.flow.label()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ending_before_positive_traffic_does_not_launch_ping_or_leave_a_probe() {
        let mut cfg = super::super::tests::example();
        cfg.links.truncate(1);
        cfg.run_overrides.probe_during_traffic = true;
        let unit = plan::build(&cfg).unwrap().units.remove(0);
        let link = &cfg.links[0];
        let preflight = LinkPreflight {
            board_iface: "br0".into(),
            counter_source: None,
            addresses: TrafficAddresses::ipv4(link),
        };
        let adb = Adb {
            program: "this-program-must-never-be-started".into(),
            serial: "test".into(),
        };
        let cancel = AtomicBool::new(false);
        let context = UnitContext {
            adb: &adb,
            cfg: &cfg,
            link,
            preflight: &preflight,
            bin: "this-program-must-never-be-started",
            agent: None,
            dir: Path::new("."),
            cancel: &cancel,
            capability: None,
        };
        let (_, _, diagnostics) = with_load_latency(&context, &unit, &unit.legs[0], |traffic| {
            assert!(!traffic.load(Ordering::SeqCst));
            Ok((
                IperfClientOut {
                    ok: false,
                    ..Default::default()
                },
                Vec::new(),
            ))
        })
        .unwrap();
        assert!(diagnostics.is_empty());
        let stop = AtomicBool::new(true);
        let stopped = UnitContext {
            cancel: &stop,
            ..context.clone()
        };
        assert!(
            load_latency(&stopped, &unit, &unit.legs[0], &AtomicBool::new(false))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn ipv6_pmtu_is_skipped_without_starting_a_df_process() {
        let mut cfg = super::super::tests::example();
        cfg.links.truncate(1);
        let mut unit = plan::build(&cfg).unwrap().units.remove(0);
        unit.ip_version = 6;
        let link = &cfg.links[0];
        let preflight = LinkPreflight {
            board_iface: "br0".into(),
            counter_source: None,
            addresses: TrafficAddresses::ipv4(link),
        };
        let adb = Adb {
            program: "this-program-must-never-be-started".into(),
            serial: "test".into(),
        };
        let cancel = AtomicBool::new(false);
        let context = UnitContext {
            adb: &adb,
            cfg: &cfg,
            link,
            preflight: &preflight,
            bin: "unused",
            agent: None,
            dir: Path::new("."),
            cancel: &cancel,
            capability: None,
        };
        assert!(path_mtu(&context, &unit, &unit.legs[0])
            .unwrap()
            .contains("IPv6 未探测"));
    }
}
