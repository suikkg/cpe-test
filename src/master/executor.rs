//! 任务执行器：调度本地/远端的 ping、iperf、监控、截图，产出报告行

use crate::cmd::iperf::{self, IperfServerMgr};
use crate::config::{Config, RateCheckCfg, RateMode};
use crate::http_client;
use crate::master::builder::{v6_addrs, IperfTask, Leg, LegKind, PingTask, Side, Unit};
use crate::nic::monitor::{MonitorMgr, MIN_VALID_RX_MBPS};
use crate::ping;
use crate::protocol::*;
use crate::report::{ExecutionStatus, Row, Verdict};
use crate::util::{find_iperf3, logln, md5_hex, now_compact, now_full, sanitize};
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const UDP_SERVER_START_RETRIES: usize = 1;
const MIN_RATE_SAMPLE_COVERAGE: f64 = 0.95;
const ROLLING_RATE_WINDOW_MS: u64 = 5_000;
const ROLLING_COVERAGE_TOLERANCE_MS: u64 = 50;
const FLOW_TIMELINE_TOLERANCE_MS: u64 = 2_000;

pub struct Ctx {
    pub agent_host: String,
    pub agent_port: u16,
    pub cfg: Config,
    pub outdir: PathBuf,
    pub local_servers: IperfServerMgr,
    pub local_monitors: MonitorMgr,
    pub rows: Mutex<Vec<Row>>,
    pub db: Mutex<ResultDb>,
}

#[derive(Debug, Default, Clone)]
pub struct RunSummary {
    pub pass: usize,
    pub fail: usize,
    pub measured: usize,
    pub not_evaluated: usize,
    pub setup_error: usize,
    pub unstable: usize,
    pub skip: usize,
}

struct LegOutcome {
    verdict: Verdict,
    reason_code: String,
    reason_detail: String,
    rx_avg: Option<f64>,
    main_rows: Vec<usize>,
    tag: String,
}

#[derive(Clone)]
struct UdpLegPlan {
    lidx: usize,
    tag: String,
    name: String,
    streams: Vec<IperfTask>,
}

#[derive(Clone)]
struct PreparedUdpFlow {
    leg_pos: usize,
    stream_pos: usize,
    task: IperfTask,
    server_req: Option<IperfServerStartReq>,
    client_req: Option<IperfClientReq>,
    server_error: String,
    launch_delay_ms: u64,
}

struct UdpFlowRun {
    leg_pos: usize,
    stream_pos: usize,
    task: IperfTask,
    raw_ok: bool,
    parsed: iperf::IperfParsed,
    client: IperfClientOut,
    server_output: String,
    events: Vec<IperfFlowEvent>,
    retries: usize,
    error: String,
}

#[derive(Debug, Clone, Default)]
struct RateStats {
    avg_mbps: Option<f64>,
    p10_mbps: Option<f64>,
    median_mbps: Option<f64>,
    p95_mbps: Option<f64>,
    min_mbps: Option<f64>,
    max_mbps: Option<f64>,
    coverage: f64,
}

#[derive(Debug, Clone, Default)]
struct EffectiveWindow {
    start_ms: u64,
    end_ms: u64,
    available_secs: f64,
    required_secs: u64,
    complete: bool,
}

#[derive(Default)]
struct LiveUdpFlow {
    connected: bool,
    active: bool,
    ended: bool,
    last_mbps: Option<f64>,
    error: String,
    retries: usize,
}

impl Ctx {
    // ---------------- agent HTTP ----------------

    fn agent_post<TReq: Serialize, TOut: DeserializeOwned>(
        &self,
        path: &str,
        req: &TReq,
        timeout: Duration,
    ) -> Result<TOut, String> {
        let body = serde_json::to_string(req).map_err(|e| format!("序列化失败: {e}"))?;
        let (status, text) =
            http_client::post_json(&self.agent_host, self.agent_port, path, &body, timeout)
                .map_err(|e| format!("辅测机 {path} 调用失败: {e}"))?;
        if status != 200 {
            return Err(format!("辅测机 {path} 返回 HTTP {status}: {text}"));
        }
        let resp: Resp<TOut> =
            serde_json::from_str(&text).map_err(|e| format!("辅测机 {path} 响应解析失败: {e}"))?;
        if !resp.ok {
            return Err(resp
                .error
                .unwrap_or_else(|| format!("辅测机 {path} 返回未知错误")));
        }
        resp.data
            .ok_or_else(|| format!("辅测机 {path} 响应缺少 data"))
    }

    // ---------------- 双端统一操作 ----------------

    fn ping_at(&self, side: Side, req: &PingReq) -> Result<PingOut, String> {
        match side {
            Side::Master => Ok(ping::run(req)),
            Side::Agent => {
                self.agent_post("/ping", req, Duration::from_secs(req.count as u64 * 5 + 60))
            }
        }
    }

    fn server_start(&self, side: Side, req: &IperfServerStartReq) -> Result<String, String> {
        match side {
            Side::Master => {
                let bin = find_iperf3().ok_or("主控机未找到 iperf3，请把 iperf3 放到程序同目录")?;
                self.local_servers.start(&bin, req)
            }
            Side::Agent => self
                .agent_post::<_, IperfServerStartOut>(
                    "/iperf/server/start",
                    req,
                    Duration::from_secs(40),
                )
                .map(|o| o.cmd),
        }
    }

    fn server_stop(&self, side: Side, port: u16) -> IperfServerStopOut {
        match side {
            Side::Master => self.local_servers.stop(port, Duration::from_secs(3)),
            Side::Agent => self
                .agent_post(
                    "/iperf/server/stop",
                    &IperfServerStopReq { port, wait_secs: 3 },
                    Duration::from_secs(30),
                )
                .unwrap_or_else(|e| IperfServerStopOut {
                    existed: false,
                    output: format!("(获取 server 输出失败: {e})"),
                }),
        }
    }

    fn client_run_tracked<F>(
        &self,
        side: Side,
        req: &IperfClientReq,
        mut on_event: F,
    ) -> IperfClientOut
    where
        F: FnMut(IperfFlowEvent),
    {
        match side {
            Side::Master => {
                let Some(bin) = find_iperf3() else {
                    return IperfClientOut {
                        ok: false,
                        timed_out: false,
                        cmd: String::new(),
                        output: "主控机未找到 iperf3，请把 iperf3 放到程序同目录".into(),
                        ..Default::default()
                    };
                };
                iperf::run_client_controlled(
                    &bin,
                    req,
                    None,
                    |line| {
                        if line.to_lowercase().contains("error") {
                            logln(&format!("      {line}"));
                        }
                    },
                    &mut on_event,
                )
            }
            Side::Agent => {
                let started: IperfClientStartOut = match self.agent_post(
                    "/iperf/client/start",
                    &IperfClientStartReq {
                        request: req.clone(),
                    },
                    Duration::from_secs(20),
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        return IperfClientOut {
                            output: format!("(远端异步作业启动失败: {e})"),
                            ..Default::default()
                        }
                    }
                };
                let deadline = std::time::Instant::now()
                    + Duration::from_secs(req.duration.saturating_add(180));
                let mut cursor = 0usize;
                loop {
                    if std::time::Instant::now() >= deadline {
                        let _ = self.agent_post::<_, IperfClientStopOut>(
                            "/iperf/client/stop",
                            &IperfClientStopReq {
                                id: started.id.clone(),
                            },
                            Duration::from_secs(10),
                        );
                        return IperfClientOut {
                            timed_out: true,
                            output: format!(
                                "(远端异步作业 {} 超过 {} 秒仍未结束)",
                                started.id,
                                req.duration.saturating_add(180)
                            ),
                            ..Default::default()
                        };
                    }
                    let status: IperfClientStatusOut = match self.agent_post(
                        "/iperf/client/status",
                        &IperfClientStatusReq {
                            id: started.id.clone(),
                            cursor,
                        },
                        Duration::from_secs(20),
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = self.agent_post::<_, IperfClientStopOut>(
                                "/iperf/client/stop",
                                &IperfClientStopReq {
                                    id: started.id.clone(),
                                },
                                Duration::from_secs(10),
                            );
                            return IperfClientOut {
                                output: format!("(远端异步作业查询失败: {e})"),
                                ..Default::default()
                            };
                        }
                    };
                    cursor = status.next_cursor;
                    for event in status.events {
                        if event.kind == IperfEventKind::Error {
                            logln(&format!("      [远端 {}] {}", started.id, event.line));
                        }
                        on_event(event);
                    }
                    if status.done {
                        let result = status.result.unwrap_or_else(|| IperfClientOut {
                            output: format!("(远端异步作业 {} 已结束但缺少结果)", started.id),
                            ..Default::default()
                        });
                        let _ = self.agent_post::<_, IperfClientStopOut>(
                            "/iperf/client/stop",
                            &IperfClientStopReq {
                                id: started.id.clone(),
                            },
                            Duration::from_secs(10),
                        );
                        return result;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }

    fn client_run(&self, side: Side, req: &IperfClientReq) -> IperfClientOut {
        self.client_run_tracked(side, req, |_| {})
    }

    fn mon_start(&self, side: Side, iface: &str) -> Result<String, String> {
        let interval_ms = self
            .cfg
            .iperf
            .rate_check
            .sample_interval_ms
            .clamp(200, 5_000);
        match side {
            Side::Master => self.local_monitors.start(iface, interval_ms),
            Side::Agent => self
                .agent_post::<_, MonitorStartOut>(
                    "/monitor/start",
                    &MonitorStartReq {
                        iface: iface.to_string(),
                        interval_ms,
                    },
                    Duration::from_secs(20),
                )
                .map(|o| o.id),
        }
    }

    fn mon_stop(&self, side: Side, id: &str) -> Result<MonitorStopOut, String> {
        match side {
            Side::Master => self.local_monitors.stop(id),
            Side::Agent => self.agent_post(
                "/monitor/stop",
                &MonitorStopReq { id: id.to_string() },
                Duration::from_secs(20),
            ),
        }
    }

    /// 两端都尝试截图，任一成功就保存。返回报告用相对路径（多个用分号隔开）
    fn take_screenshots(&self, sides: &[Side], label: &str) -> (String, String) {
        let mut master = String::new();
        let mut agent = String::new();
        for side in sides.iter() {
            let png: Vec<u8> = match side {
                Side::Master => match crate::screenshot::capture_png() {
                    Ok(p) => p,
                    Err(e) => {
                        logln(&format!("    [截图] 主控端截图失败，任务 [{}]: {e}", label));
                        continue;
                    }
                },
                Side::Agent => {
                    let body = match serde_json::to_string(&ScreenshotReq {
                        label: label.to_string(),
                    }) {
                        Ok(body) => body,
                        Err(e) => {
                            logln(&format!("    [截图] 辅测请求序列化失败: {e}"));
                            continue;
                        }
                    };
                    let timeout = Duration::from_secs(180);
                    let (status, text) = match crate::http_client::post_json(
                        &self.agent_host,
                        self.agent_port,
                        "/screenshot",
                        &body,
                        timeout,
                    ) {
                        Ok((s, t)) => {
                            logln(&format!("    [截图] 辅测响应: status={s}, len={}", t.len()));
                            (s, t)
                        }
                        Err(e) => {
                            logln(&format!("    [截图] 辅测请求失败: {e}"));
                            continue;
                        }
                    };
                    if status != 200 {
                        logln(&format!(
                            "    [截图] 辅测 HTTP {status}: {}",
                            text_preview(&text, 200)
                        ));
                        continue;
                    }
                    let resp: Resp<ScreenshotOut> = match serde_json::from_str(&text) {
                        Ok(r) => r,
                        Err(e) => {
                            logln(&format!(
                                "    [截图] JSON解析失败: {e}, raw前100字符: {}",
                                text_preview(&text, 100)
                            ));
                            continue;
                        }
                    };
                    if !resp.ok {
                        logln(&format!(
                            "    [截图] 辅测截图错误: {}",
                            resp.error.unwrap_or_default()
                        ));
                        continue;
                    }
                    let Some(data) = resp.data else {
                        logln("    [截图] 辅测响应缺data");
                        continue;
                    };
                    let b64_len = data.image_b64.len();
                    match base64::engine::general_purpose::STANDARD.decode(data.image_b64) {
                        Ok(p) => p,
                        Err(e) => {
                            logln(&format!(
                                "    [截图] 辅测 base64 解码失败: {e}, len={b64_len}"
                            ));
                            continue;
                        }
                    }
                }
            };
            let (tag, ref mut out_path) = match side {
                Side::Master => ("_master", &mut master),
                Side::Agent => ("_agent", &mut agent),
            };
            let fname = format!(
                "screenshot_{}{}_{}.png",
                sanitize(label),
                tag,
                now_compact()
            );
            let full = self.outdir.join(&fname);
            if let Err(e) = std::fs::write(&full, &png) {
                logln(&format!(
                    "    [截图] {}端截图写入失败 {}: {e}",
                    side.cn(),
                    full.display()
                ));
                continue;
            }
            if let Some(dir_name) = self.outdir.file_name() {
                out_path.clear();
                out_path.push_str(&format!("./{}/{}", dir_name.to_string_lossy(), fname));
                logln(&format!(
                    "    [截图] {}端截图已保存: {}",
                    side.cn(),
                    full.display()
                ));
            } else {
                logln(&format!(
                    "    [截图] {}端截图文件已写入，但输出目录缺少可用目录名: {}",
                    side.cn(),
                    full.display()
                ));
            }
        }
        (master, agent)
    }

    fn push_row(&self, row: Row) -> usize {
        let mut g = self.rows.lock().unwrap();
        g.push(row);
        g.len() - 1
    }

    fn udp_leg_plans(&self, unit: &Unit) -> Option<Vec<UdpLegPlan>> {
        let mut plans = Vec::new();
        for (lidx, leg) in unit.legs.iter().enumerate() {
            let (name, streams) = match &leg.kind {
                LegKind::IperfSingle(t) if t.udp => (t.profile_name.clone(), vec![t.clone()]),
                LegKind::IperfGroup { name, streams }
                    if !streams.is_empty() && streams.iter().all(|t| t.udp) =>
                {
                    (name.clone(), streams.clone())
                }
                _ => return None,
            };
            plans.push(UdpLegPlan {
                lidx,
                tag: leg.tag.clone(),
                name,
                streams,
            });
        }
        if plans.is_empty() {
            None
        } else {
            Some(plans)
        }
    }

    // ---------------- 执行入口 ----------------

    pub fn run_all(&self, units: &[Unit]) -> RunSummary {
        let mut sum = RunSummary::default();
        let total = units.len();
        for (i, unit) in units.iter().enumerate() {
            logln(&format!("\n[{}/{}] {}", i + 1, total, unit.title));
            if self.cfg.resume {
                let fresh = { self.db.lock().unwrap().fresh_pass(&unit.id) };
                if let Some(t) = fresh {
                    logln(&format!("  已PASS，上次时间: {t}，跳过 (RESUME)"));
                    sum.skip += 1;
                    self.push_row(Row {
                        sort_key: (i, 0, 0, 0),
                        time: now_full(),
                        task_id: unit.id.clone(),
                        parent_id: unit.id.clone(),
                        task: unit.title.clone(),
                        verdict: Verdict::Skip,
                        execution_status: ExecutionStatus::Skipped,
                        kind_label: format!("跳过(上次PASS: {t})"),
                        is_unit_summary: true,
                        ..Default::default()
                    });
                    continue;
                }
            }

            let outcomes: Vec<LegOutcome> = if let Some(plans) = self.udp_leg_plans(unit) {
                self.run_udp_unit(i, unit, &plans)
            } else if unit.legs.len() <= 1 {
                unit.legs
                    .iter()
                    .map(|leg| self.run_leg(i, unit, 0, leg))
                    .collect()
            } else {
                std::thread::scope(|s| {
                    let handles: Vec<_> = unit
                        .legs
                        .iter()
                        .enumerate()
                        .map(|(li, leg)| s.spawn(move || self.run_leg(i, unit, li, leg)))
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join().unwrap_or(LegOutcome {
                                verdict: Verdict::SetupError,
                                reason_code: "LEG_THREAD_PANIC".into(),
                                reason_detail: "方向执行线程 panic".into(),
                                rx_avg: None,
                                main_rows: vec![],
                                tag: String::new(),
                            })
                        })
                        .collect()
                })
            };

            // 双向：互填「对向接收 Mbps」
            if unit.bidir && outcomes.len() == 2 {
                let mut g = self.rows.lock().unwrap();
                for (me, other) in [(0usize, 1usize), (1, 0)] {
                    if let Some(rx) = outcomes[other].rx_avg {
                        for ri in &outcomes[me].main_rows {
                            if let Some(row) = g.get_mut(*ri) {
                                row.peer_rx = format!("{:.3}({})", rx, outcomes[other].tag);
                            }
                        }
                    }
                }
            }

            let unit_verdict = aggregate_unit_verdict(&outcomes);
            let unit_reason = outcome_matching_verdict(&outcomes, unit_verdict);
            let unit_ok = unit_verdict.is_pass();
            match unit_verdict {
                Verdict::Pass => sum.pass += 1,
                Verdict::Measured => sum.measured += 1,
                Verdict::NotEvaluated => {
                    sum.not_evaluated += 1;
                    sum.fail += 1;
                }
                Verdict::SetupError => {
                    sum.setup_error += 1;
                    sum.fail += 1;
                }
                Verdict::Unstable => {
                    sum.unstable += 1;
                    sum.fail += 1;
                }
                Verdict::RateFail => sum.fail += 1,
                Verdict::Skip => sum.skip += 1,
            }
            let reasons: Vec<String> = outcomes
                .iter()
                .filter(|outcome| outcome.verdict != Verdict::Pass)
                .map(|outcome| {
                    format!(
                        "{}:{} {}",
                        if outcome.tag.is_empty() {
                            "单向"
                        } else {
                            &outcome.tag
                        },
                        outcome.reason_code,
                        outcome.reason_detail
                    )
                })
                .collect();
            logln(&format!("  ==> 单元结果: {}", unit_verdict.label()));
            self.push_row(Row {
                sort_key: (i, 0, 0, 0),
                time: now_full(),
                task_id: unit.id.clone(),
                parent_id: unit.id.clone(),
                task: unit.title.clone(),
                verdict: unit_verdict,
                execution_status: match unit_verdict {
                    Verdict::SetupError => ExecutionStatus::Error,
                    Verdict::NotEvaluated => ExecutionStatus::Partial,
                    _ => ExecutionStatus::Completed,
                },
                reason_code: unit_reason
                    .map(|outcome| outcome.reason_code.clone())
                    .unwrap_or_default(),
                reason_detail: reasons.join(" | "),
                kind_label: if unit.bidir {
                    "测试单元汇总(双向)".into()
                } else {
                    "测试单元汇总".into()
                },
                is_unit_summary: true,
                ..Default::default()
            });
            {
                let mut db = self.db.lock().unwrap();
                db.set(&unit.id, unit_ok, &unit.title);
                db.save();
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        sum
    }

    fn run_leg(&self, useq: usize, unit: &Unit, lidx: usize, leg: &Leg) -> LegOutcome {
        match &leg.kind {
            LegKind::Ping(t) => self.run_ping_leg(useq, unit, lidx, &leg.tag, t),
            LegKind::IperfSingle(t) => self.run_iperf_single(useq, unit, lidx, &leg.tag, t),
            LegKind::IperfGroup { .. } => {
                let detail = "UDP 并发组未进入统一调度器（空流组、混合协议或内部任务结构异常）";
                logln(&format!("    [内部调度错误] {detail}"));
                LegOutcome {
                    verdict: Verdict::SetupError,
                    reason_code: "UDP_GROUP_DISPATCH_ERROR".into(),
                    reason_detail: detail.into(),
                    rx_avg: None,
                    main_rows: vec![],
                    tag: leg.tag.clone(),
                }
            }
        }
    }

    // ---------------- ping ----------------

    fn run_ping_leg(
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
        logln(&format!(
            "  [ping{}] {} -> {} (n={}, -l {}) 执行中...",
            fmt_tag(tag),
            src_addr,
            dst_addr,
            t.count,
            t.payload
        ));
        let out = match self.ping_at(t.src.side, &req) {
            Ok(o) => o,
            Err(e) => PingOut {
                ok: false,
                sent: t.count,
                received: 0,
                lost: t.count,
                loss_pct: 100.0,
                raw: format!("(执行失败: {e})"),
                ..Default::default()
            },
        };
        logln(&format!(
            "    结果: {} 收/发={}/{} 丢包={:.1}% 平均={}ms",
            if out.ok { "PASS" } else { "FAIL" },
            out.received,
            out.sent,
            out.loss_pct,
            out.rtt_avg
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        ));
        let kind_label = if unit.bidir {
            format!("★双向-{tag}")
        } else {
            "PING".into()
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
            verdict: if out.ok {
                Verdict::Pass
            } else {
                Verdict::RateFail
            },
            execution_status: ExecutionStatus::Completed,
            reason_code: if out.ok {
                String::new()
            } else {
                "PING_FAILED".into()
            },
            reason_detail: if out.ok {
                String::new()
            } else {
                format!("丢包率 {:.1}%", out.loss_pct)
            },
            kind_label,
            ping_loss: Some(out.loss_pct),
            ping_avg: out.rtt_avg,
            command: out.cmd.clone(),
            raws: vec![(
                format!("ping{} 输出", fmt_tag(tag)),
                format!("$ {}\n{}", out.cmd, out.raw),
            )],
            ..Default::default()
        });
        LegOutcome {
            verdict: if out.ok {
                Verdict::Pass
            } else {
                Verdict::RateFail
            },
            reason_code: if out.ok {
                String::new()
            } else {
                "PING_FAILED".into()
            },
            reason_detail: if out.ok {
                String::new()
            } else {
                format!("丢包率 {:.1}%", out.loss_pct)
            },
            rx_avg: None,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    // ---------------- iperf 单条 ----------------

    fn build_iperf_requests(
        &self,
        t: &IperfTask,
        duration: u64,
    ) -> Result<(IperfServerStartReq, IperfClientReq), String> {
        let (client_bind, client_target, server_bind) = if t.v6 {
            let v = v6_addrs(&t.src.nic, &t.dst.nic)
                .ok_or_else(|| "两端缺少可用 IPv6 地址".to_string())?;
            (
                add_zone(&v.client_bind, &t.src.nic.zone, t.src.side),
                add_zone(&v.client_target, &t.src.nic.zone, t.src.side),
                add_zone(&v.server_bind, &t.dst.nic.zone, t.dst.side),
            )
        } else {
            (
                t.src.nic.ipv4.clone(),
                t.dst.nic.ipv4.clone(),
                t.dst.nic.ipv4.clone(),
            )
        };
        Ok((
            IperfServerStartReq {
                bind_ip: server_bind,
                port: t.port,
                v6: t.v6,
            },
            IperfClientReq {
                dst: client_target,
                bind_ip: client_bind,
                port: t.port,
                duration,
                udp: t.udp,
                v6: t.v6,
                extra: t.extra.clone(),
            },
        ))
    }

    /// 核心执行：server(dst侧) -> client(src侧) -> 停 server。不含监控。
    fn exec_iperf_core(&self, t: &IperfTask) -> (bool, iperf::IperfParsed, IperfClientOut, String) {
        let (sreq, creq) = match self.build_iperf_requests(t, t.duration) {
            Ok(v) => v,
            Err(e) => {
                let out = IperfClientOut {
                    output: e,
                    ..Default::default()
                };
                return (false, iperf::IperfParsed::default(), out, String::new());
            }
        };
        if let Err(e) = self.server_start(t.dst.side, &sreq) {
            // 同时构造 client 命令供查错
            let cli_args = crate::cmd::iperf::client_args(&creq);
            let cli_cmd = format!("iperf3 {}", cli_args.join(" "));
            let out = IperfClientOut {
                ok: false,
                cmd: cli_cmd,
                output: format!("(iperf3 server 启动失败: {e})"),
                ..Default::default()
            };
            return (false, iperf::IperfParsed::default(), out, String::new());
        }
        let client = self.client_run(t.src.side, &creq);
        let server_out = self.server_stop(t.dst.side, t.port).output;
        let parsed = iperf::parse_output(&client.output);
        let raw_ok = client.ok && !client.timed_out;
        (raw_ok, parsed, client, server_out)
    }

    fn run_iperf_single(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        tag: &str,
        t: &IperfTask,
    ) -> LegOutcome {
        let time = now_full();
        logln(&format!(
            "  [iperf{}] {} {} -> {} 端口{} {}s...",
            fmt_tag(tag),
            t.profile_label,
            t.src.brief(),
            t.dst.brief(),
            t.port,
            t.duration
        ));
        let mon_id = match self.mon_start(t.dst.side, &t.dst.nic.name) {
            Ok(id) => Some(id),
            Err(e) => {
                logln(&format!("    (接收端网卡监控启动失败: {e})"));
                None
            }
        };
        let (raw_ok, parsed, client, server_out) = self.exec_iperf_core(t);
        let mon_out = mon_id.and_then(|id| self.mon_stop(t.dst.side, &id).ok());
        let rx_avg = mon_out.as_ref().map(|m| m.avg_mbps);

        let meas_ok =
            parsed.has_measurement() || rx_avg.map(|v| v > MIN_VALID_RX_MBPS).unwrap_or(false);
        let ok = raw_ok && meas_ok;
        let verdict = if ok {
            Verdict::Pass
        } else if !raw_ok {
            Verdict::SetupError
        } else {
            Verdict::RateFail
        };
        let reason_code = if ok {
            String::new()
        } else if !raw_ok {
            "IPERF_EXEC_FAILED".into()
        } else {
            "NO_VALID_MEASUREMENT".into()
        };

        logln(&format!(
            "    结果: {} 发送={} 接收={} 网卡实测={}",
            if ok { "PASS" } else { "FAIL" },
            fmt_opt(parsed.best_sender()),
            fmt_opt(parsed.best_receiver()),
            fmt_opt(rx_avg)
        ));

        let (screenshot_master, screenshot_agent) = if self.cfg.screenshot {
            self.take_screenshots(
                &[t.dst.side, t.src.side],
                &format!("{}_{}", unit.title, tag),
            )
        } else {
            (String::new(), String::new())
        };

        let kind_label = if unit.bidir {
            format!("★★双向灌包-{tag}")
        } else {
            "灌包".into()
        };
        let idx = self.push_row(Row {
            sort_key: (useq, lidx, 0, 0),
            time,
            task_id: md5_hex(&format!("{}|{}|{}", unit.id, tag, t.stream_idx)),
            parent_id: unit.id.clone(),
            task: unit.title.clone(),
            ip: if t.v6 { "V6".into() } else { "V4".into() },
            transport: if t.udp { "UDP".into() } else { "TCP".into() },
            param: t.profile_label.clone(),
            src_pc: t.src.pc.clone(),
            src_iface: t.src.nic.name.clone(),
            src_ip: t.src.nic.ipv4.clone(),
            dst_pc: t.dst.pc.clone(),
            dst_iface: t.dst.nic.name.clone(),
            dst_ip: t.dst.nic.ipv4.clone(),
            verdict,
            execution_status: if raw_ok {
                ExecutionStatus::Completed
            } else if client.timed_out {
                ExecutionStatus::TimedOut
            } else {
                ExecutionStatus::Error
            },
            reason_code: reason_code.clone(),
            reason_detail: if ok {
                String::new()
            } else {
                client.output.lines().last().unwrap_or_default().to_string()
            },
            kind_label,
            rx_avg,
            tx_mbps: parsed.best_sender(),
            rx_mbps: parsed.best_receiver(),
            udp_loss: if t.udp { parsed.udp_loss_pct } else { None },
            screenshot_master,
            screenshot_agent,
            command: client.cmd.clone(),
            raws: vec![
                (
                    format!("iperf3 client{} 输出", fmt_tag(tag)),
                    format!("$ {}\n{}", client.cmd, client.output),
                ),
                (format!("iperf3 server{} 输出", fmt_tag(tag)), server_out),
            ],
            ..Default::default()
        });
        LegOutcome {
            verdict,
            reason_code,
            reason_detail: if ok {
                String::new()
            } else {
                client.output.lines().last().unwrap_or_default().to_string()
            },
            rx_avg,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    // ---------------- UDP 单元统一调度 ----------------

    fn start_udp_server_with_retry(
        &self,
        task: &IperfTask,
        req: &IperfServerStartReq,
    ) -> Result<(), String> {
        let mut errors = Vec::new();
        for attempt in 0..=UDP_SERVER_START_RETRIES {
            match self.server_start(task.dst.side, req) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    errors.push(format!("第{}次: {e}", attempt + 1));
                    if attempt < UDP_SERVER_START_RETRIES {
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        }
        Err(errors.join("；"))
    }

    fn run_prepared_udp_flow(
        &self,
        prepared: PreparedUdpFlow,
        epoch: &Instant,
        live: &Arc<Mutex<HashMap<(usize, usize), LiveUdpFlow>>>,
    ) -> UdpFlowRun {
        if prepared.server_req.is_none() || prepared.client_req.is_none() {
            if let Ok(mut g) = live.lock() {
                let s = g
                    .entry((prepared.leg_pos, prepared.stream_pos))
                    .or_default();
                s.ended = true;
                s.error = prepared.server_error.clone();
            }
            return UdpFlowRun {
                leg_pos: prepared.leg_pos,
                stream_pos: prepared.stream_pos,
                task: prepared.task,
                raw_ok: false,
                parsed: iperf::IperfParsed::default(),
                client: IperfClientOut {
                    output: prepared.server_error.clone(),
                    ..Default::default()
                },
                server_output: String::new(),
                events: vec![],
                retries: 0,
                error: prepared.server_error,
            };
        }

        std::thread::sleep(Duration::from_millis(prepared.launch_delay_ms));
        let server_req = prepared.server_req.clone().unwrap();
        let client_req = prepared.client_req.clone().unwrap();
        let mut all_events = Vec::new();
        let mut all_server_output = Vec::new();
        let mut final_client = IperfClientOut::default();
        let mut final_parsed = iperf::IperfParsed::default();
        let mut final_ok = false;
        let mut retries = 0usize;
        let mut final_error = String::new();

        let max_flow_retries = self.cfg.iperf.rate_check.flow_retries as usize;
        let retry_cutoff =
            Duration::from_secs(self.cfg.iperf.rate_check.startup_timeout_secs.max(1));
        for attempt in 0..=max_flow_retries {
            let attempt_start_ms = epoch.elapsed().as_millis() as u64;
            let key = (prepared.leg_pos, prepared.stream_pos);
            let live_ref = Arc::clone(live);
            let mut attempt_events: Vec<IperfFlowEvent> = Vec::new();
            let attempt_started = Instant::now();
            let client =
                self.client_run_tracked(prepared.task.src.side, &client_req, |mut event| {
                    event.elapsed_ms = event.elapsed_ms.saturating_add(attempt_start_ms);
                    if let Ok(mut g) = live_ref.lock() {
                        let state = g.entry(key).or_default();
                        match event.kind {
                            IperfEventKind::Connected => state.connected = true,
                            IperfEventKind::Traffic => {
                                state.active = true;
                                state.last_mbps = event.mbps;
                            }
                            IperfEventKind::Retry => state.retries += 1,
                            IperfEventKind::Error => state.error = event.line.clone(),
                            IperfEventKind::Ended => state.ended = true,
                            IperfEventKind::Started => {}
                        }
                    }
                    attempt_events.push(event);
                });
            let parsed = iperf::parse_output(&client.output);
            let raw_ok =
                client.ok && !client.timed_out && !client.cancelled && parsed.has_measurement();

            all_events.extend(attempt_events);
            final_client = client;
            final_parsed = parsed;
            final_ok = raw_ok;
            let server_out = self
                .server_stop(prepared.task.dst.side, prepared.task.port)
                .output;
            all_server_output.push(format!("=== attempt {} ===\n{}", attempt + 1, server_out));
            if raw_ok {
                break;
            }

            final_error = if final_client.timed_out {
                "client 超时".into()
            } else if final_client.cancelled {
                "client 被取消".into()
            } else if final_client.output.trim().is_empty() {
                "client 未输出有效测量".into()
            } else {
                final_client
                    .output
                    .lines()
                    .find(|line| line.to_lowercase().contains("error"))
                    .unwrap_or("client 未产生有效测量")
                    .to_string()
            };

            let retryable = should_retry_udp_flow(
                attempt,
                max_flow_retries,
                attempt_started.elapsed(),
                retry_cutoff,
                &final_client,
            );
            if !retryable {
                break;
            }

            retries += 1;
            if let Ok(mut g) = live.lock() {
                let state = g
                    .entry((prepared.leg_pos, prepared.stream_pos))
                    .or_default();
                state.retries += 1;
                state.ended = false;
                state.active = false;
                state.connected = false;
            }
            logln(&format!(
                "    [UDP流重试] {}-#{} 首轮未跑通，重新启动 server/client（{}/{}）",
                if prepared.task.stream_idx == 0 && prepared.stream_pos == 0 {
                    "流"
                } else {
                    "并发流"
                },
                prepared.stream_pos + 1,
                retries,
                max_flow_retries
            ));
            all_events.push(IperfFlowEvent {
                kind: IperfEventKind::Retry,
                elapsed_ms: epoch.elapsed().as_millis() as u64,
                mbps: None,
                line: format!("group retry {retries}"),
            });
            if let Err(e) = self.start_udp_server_with_retry(&prepared.task, &server_req) {
                final_error = format!("重试时 server 启动失败: {e}");
                break;
            }
        }

        if let Ok(mut g) = live.lock() {
            let state = g
                .entry((prepared.leg_pos, prepared.stream_pos))
                .or_default();
            state.ended = true;
            if final_ok {
                state.error.clear();
            } else if !final_error.is_empty() {
                state.error = final_error.clone();
            }
        }

        let total_retries = count_retry_events(&all_events);
        UdpFlowRun {
            leg_pos: prepared.leg_pos,
            stream_pos: prepared.stream_pos,
            task: prepared.task,
            raw_ok: final_ok,
            parsed: final_parsed,
            client: final_client,
            server_output: all_server_output.join("\n"),
            events: all_events,
            retries: total_retries,
            error: final_error,
        }
    }

    fn run_udp_unit(&self, useq: usize, unit: &Unit, plans: &[UdpLegPlan]) -> Vec<LegOutcome> {
        let epoch = Instant::now();
        let total_flows: usize = plans.iter().map(|p| p.streams.len()).sum();
        logln(&format!(
            "  [UDP统一调度] {} 个方向，共 {} 条流：先准备全部 server，再交错起流",
            plans.len(),
            total_flows
        ));

        let max_streams = plans.iter().map(|p| p.streams.len()).max().unwrap_or(0);
        let rate_cfg = &self.cfg.iperf.rate_check;
        let mut launch_delays: HashMap<(usize, usize), u64> = HashMap::new();
        let mut slot = 0u64;
        for stream_pos in 0..max_streams {
            for (leg_pos, plan) in plans.iter().enumerate() {
                if stream_pos < plan.streams.len() {
                    let mode = plan.streams[stream_pos].rate_mode;
                    let stage_delay = if mode == RateMode::Discover {
                        discovery_stage(stream_pos, plan.streams.len())
                            * rate_cfg.discovery_step_secs
                            * 1000
                    } else {
                        0
                    };
                    launch_delays.insert(
                        (leg_pos, stream_pos),
                        stage_delay + slot * rate_cfg.launch_interval_ms.clamp(0, 1_000),
                    );
                    slot += 1;
                }
            }
        }
        let max_launch_delay_ms = launch_delays.values().copied().max().unwrap_or(0);

        let mut prepared: Vec<PreparedUdpFlow> = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            for (stream_pos, task) in plan.streams.iter().enumerate() {
                let launch_delay_ms = launch_delays
                    .get(&(leg_pos, stream_pos))
                    .copied()
                    .unwrap_or(0);
                let remaining_launch_secs = max_launch_delay_ms
                    .saturating_sub(launch_delay_ms)
                    .div_ceil(1000);
                // duration 对用户表示有效测量时长。更早启动的流自动多跑，
                // 让 discover 阶梯、错峰、settle 和一次快速重试后仍有共同窗口。
                let process_duration = task
                    .duration
                    .saturating_add(rate_cfg.startup_timeout_secs)
                    .saturating_add(rate_cfg.settle_secs)
                    .saturating_add(5)
                    .saturating_add(remaining_launch_secs);
                match self.build_iperf_requests(task, process_duration) {
                    Ok((server_req, client_req)) => prepared.push(PreparedUdpFlow {
                        leg_pos,
                        stream_pos,
                        task: task.clone(),
                        server_req: Some(server_req),
                        client_req: Some(client_req),
                        server_error: String::new(),
                        launch_delay_ms,
                    }),
                    Err(e) => prepared.push(PreparedUdpFlow {
                        leg_pos,
                        stream_pos,
                        task: task.clone(),
                        server_req: None,
                        client_req: None,
                        server_error: e,
                        launch_delay_ms: 0,
                    }),
                }
            }
        }

        prepared = std::thread::scope(|scope| {
            let handles: Vec<_> = prepared
                .into_iter()
                .map(|mut flow| {
                    scope.spawn(move || {
                        if let Some(req) = flow.server_req.as_ref() {
                            if let Err(e) = self.start_udp_server_with_retry(&flow.task, req) {
                                flow.server_error = e;
                                flow.server_req = None;
                                flow.client_req = None;
                            }
                        }
                        flow
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let server_ready = prepared
            .iter()
            .filter(|flow| flow.server_req.is_some())
            .count();
        logln(&format!(
            "    server 准备完成: {server_ready}/{total_flows}"
        ));

        let mut monitor_ids: HashMap<String, (Side, String, u64)> = HashMap::new();
        for plan in plans {
            for task in &plan.streams {
                for endpoint in [&task.src, &task.dst] {
                    let key = endpoint.key();
                    if monitor_ids.contains_key(&key) {
                        continue;
                    }
                    match self.mon_start(endpoint.side, &endpoint.nic.name) {
                        Ok(id) => {
                            monitor_ids.insert(
                                key,
                                (endpoint.side, id, epoch.elapsed().as_millis() as u64),
                            );
                        }
                        Err(e) => logln(&format!(
                            "    ({} 网卡连续监控启动失败: {e})",
                            endpoint.brief()
                        )),
                    }
                }
            }
        }
        // 采集空闲基线，后续统计会从 RX/TX 样本中扣除中位背景流量。
        let background_secs = self.cfg.iperf.rate_check.background_secs.min(30);
        if !monitor_ids.is_empty() && background_secs > 0 {
            logln(&format!("    网卡基线采样 {background_secs}s..."));
            std::thread::sleep(Duration::from_secs(background_secs));
        }

        let live: Arc<Mutex<HashMap<(usize, usize), LiveUdpFlow>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let results: Vec<UdpFlowRun> = std::thread::scope(|scope| {
            let handles: Vec<_> = prepared
                .into_iter()
                .map(|flow| {
                    let live = Arc::clone(&live);
                    scope.spawn(move || self.run_prepared_udp_flow(flow, &epoch, &live))
                })
                .collect();

            while handles.iter().any(|h| !h.is_finished()) {
                std::thread::sleep(Duration::from_secs(1));
                let g = live.lock().unwrap();
                let mut parts = Vec::new();
                for (leg_pos, plan) in plans.iter().enumerate() {
                    let mut connected = 0usize;
                    let mut active = 0usize;
                    let mut ended = 0usize;
                    let mut rate = 0.0;
                    let mut errors = 0usize;
                    for stream_pos in 0..plan.streams.len() {
                        if let Some(state) = g.get(&(leg_pos, stream_pos)) {
                            connected += usize::from(state.connected);
                            active += usize::from(state.active && !state.ended);
                            ended += usize::from(state.ended);
                            rate += state.last_mbps.unwrap_or(0.0);
                            errors += usize::from(!state.error.is_empty());
                        }
                    }
                    parts.push(format!(
                        "{} active={}/{} connected={} ended={} rate≈{:.1}Mbps err={}",
                        if plan.tag.is_empty() {
                            "单向"
                        } else {
                            &plan.tag
                        },
                        active,
                        plan.streams.len(),
                        connected,
                        ended,
                        rate,
                        errors
                    ));
                }
                logln(&format!("    [UDP进度] {}", parts.join(" | ")));
            }
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| UdpFlowRun {
                        leg_pos: 0,
                        stream_pos: 0,
                        task: plans[0].streams[0].clone(),
                        raw_ok: false,
                        parsed: iperf::IperfParsed::default(),
                        client: IperfClientOut {
                            output: "UDP 流线程 panic".into(),
                            ..Default::default()
                        },
                        server_output: String::new(),
                        events: vec![],
                        retries: 0,
                        error: "UDP 流线程 panic".into(),
                    })
                })
                .collect()
        });

        let mut monitor_outputs: HashMap<String, MonitorStopOut> = HashMap::new();
        for (key, (side, id, start_offset_ms)) in monitor_ids {
            match self.mon_stop(side, &id) {
                Ok(mut out) => {
                    for sample in &mut out.samples {
                        sample.elapsed_ms = sample.elapsed_ms.saturating_add(start_offset_ms);
                    }
                    monitor_outputs.insert(key, out);
                }
                Err(e) => logln(&format!("    (网卡监控停止失败: {e})")),
            }
        }

        let effective_window = select_udp_effective_window(
            plans,
            &results,
            &monitor_outputs,
            &self.cfg.iperf.rate_check,
        );
        logln(&format!(
            "    有效窗口: {:.1}s / {}s{}",
            effective_window.available_secs,
            effective_window.required_secs,
            if effective_window.complete {
                "（满足）"
            } else {
                "（不足，不能正式判定）"
            }
        ));

        let mut outcomes = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            let leg_flows: Vec<&UdpFlowRun> =
                results.iter().filter(|r| r.leg_pos == leg_pos).collect();
            let n = plan.streams.len();
            let success = leg_flows.iter().filter(|r| r.raw_ok).count();
            let first = &plan.streams[0];
            let required = required_udp_streams(
                n,
                &self.cfg.iperf.rate_check,
                first.rx_target_mbps,
                first.offered_mbps,
            );
            let first_active_ms = leg_flows
                .iter()
                .filter_map(|flow| flow_active_interval(flow).map(|v| v.0))
                .min()
                .unwrap_or(effective_window.start_ms);
            let rx_stats = monitor_outputs
                .get(&first.dst.key())
                .map(|out| monitor_rate_stats(out, &effective_window, true, first_active_ms))
                .unwrap_or_default();
            let tx_stats = monitor_outputs
                .get(&first.src.key())
                .map(|out| monitor_rate_stats(out, &effective_window, false, first_active_ms))
                .unwrap_or_default();
            let rx_avg = rx_stats.avg_mbps;
            let rate_present = rx_avg.map(|v| v > MIN_VALID_RX_MBPS).unwrap_or(false);
            let offered_floor = first.rx_target_mbps.map(|target| {
                target * (1.0 + self.cfg.iperf.rate_check.offered_headroom_pct.max(0.0) / 100.0)
            });
            let tx_sufficient = offered_floor
                .map(|floor| tx_stats.p10_mbps.map(|v| v >= floor).unwrap_or(false))
                .unwrap_or(true);
            let sample_coverage_sufficient = rate_sample_coverage_sufficient(
                &rx_stats,
                &tx_stats,
                first.rx_target_mbps.is_some(),
            );
            let rx_meets_target = first
                .rx_target_mbps
                .map(|target| {
                    rx_stats.avg_mbps.map(|v| v >= target).unwrap_or(false)
                        && rx_stats.p10_mbps.map(|v| v >= target).unwrap_or(false)
                })
                .unwrap_or(true);
            let udp_loss = aggregate_udp_loss(&leg_flows);
            let loss_ok = self
                .cfg
                .iperf
                .rate_check
                .max_udp_loss_pct
                .map(|limit| udp_loss.map(|value| value <= limit))
                .unwrap_or(Some(true));
            let (verdict, reason_code, reason_detail) = if success == 0 {
                (
                    Verdict::SetupError,
                    "NO_STREAM_STARTED".to_string(),
                    format!("0/{n} 条流产生有效测量"),
                )
            } else if required > n {
                (
                    Verdict::NotEvaluated,
                    "CONFIGURED_LOAD_TOO_LOW".to_string(),
                    format!("目标需要至少 {required} 条流，但只配置了 {n} 条"),
                )
            } else if success < required {
                (
                    Verdict::NotEvaluated,
                    "ACTIVE_STREAMS_LOW".to_string(),
                    format!("仅 {success}/{n} 条流成功，正式判定至少需要 {required} 条"),
                )
            } else if !effective_window.complete {
                (
                    Verdict::NotEvaluated,
                    "EFFECTIVE_WINDOW_SHORT".to_string(),
                    format!(
                        "共同有效窗口 {:.1}s，要求 {}s",
                        effective_window.available_secs, effective_window.required_secs
                    ),
                )
            } else if !rate_present || !sample_coverage_sufficient {
                (
                    Verdict::NotEvaluated,
                    "SAMPLE_COVERAGE_LOW".to_string(),
                    format!(
                        "RX采样覆盖率 {:.1}%，TX采样覆盖率 {:.1}%{}，或无有效接收速率",
                        rx_stats.coverage * 100.0,
                        tx_stats.coverage * 100.0,
                        if first.rx_target_mbps.is_some() {
                            "（有目标时两端均要求至少 95%）"
                        } else {
                            ""
                        }
                    ),
                )
            } else if first.rx_target_mbps.is_none() && first.rate_mode == RateMode::Verify {
                (
                    Verdict::NotEvaluated,
                    "TARGET_MISSING".to_string(),
                    "verify 模式必须配置有效的 rate_targets_mbps，且当前路径没有自动 EVB 目标"
                        .to_string(),
                )
            } else if first.rx_target_mbps.is_none() {
                (
                    Verdict::Measured,
                    "TARGET_UNKNOWN".to_string(),
                    format!("{:?} 模式仅记录实际能力，不伪造 PASS/FAIL", first.rate_mode),
                )
            } else if loss_ok.is_none() {
                (
                    Verdict::NotEvaluated,
                    "UDP_LOSS_DATA_MISSING".to_string(),
                    "已配置 UDP 丢包门槛，但 iperf3 输出缺少 lost/total 数据".to_string(),
                )
            } else if !tx_sufficient {
                (
                    Verdict::NotEvaluated,
                    "OFFERED_LOAD_LOW".to_string(),
                    format!(
                        "TX-P10 {}，验证目标所需负载至少 {}",
                        fmt_opt(tx_stats.p10_mbps),
                        fmt_opt(offered_floor)
                    ),
                )
            } else if !rx_meets_target {
                let target = first.rx_target_mbps.unwrap_or_default();
                if rx_stats.avg_mbps.map(|v| v >= target).unwrap_or(false) {
                    (
                        Verdict::Unstable,
                        "RX_UNSTABLE".to_string(),
                        format!(
                            "平均速率达到目标，但5秒滚动P10 {} 低于 {}Mbps",
                            fmt_opt(rx_stats.p10_mbps),
                            target
                        ),
                    )
                } else {
                    (
                        Verdict::RateFail,
                        "RX_BELOW_TARGET".to_string(),
                        format!(
                            "RX平均 {} 低于目标 {}Mbps",
                            fmt_opt(rx_stats.avg_mbps),
                            target
                        ),
                    )
                }
            } else if loss_ok == Some(false) {
                (
                    Verdict::RateFail,
                    "UDP_LOSS_HIGH".to_string(),
                    format!(
                        "UDP平均丢包率 {:.3}% 超过限制 {:.3}%",
                        udp_loss.unwrap_or_default(),
                        self.cfg
                            .iperf
                            .rate_check
                            .max_udp_loss_pct
                            .unwrap_or_default()
                    ),
                )
            } else {
                (Verdict::Pass, String::new(), String::new())
            };
            let discovery_table = if first.rate_mode == RateMode::Discover {
                monitor_outputs
                    .get(&first.dst.key())
                    .map(|out| active_rate_table(leg_pos, &leg_flows, out, first_active_ms))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            if !discovery_table.is_empty() {
                logln(&format!(
                    "    [{}] 负载阶梯观测:\n{}",
                    if plan.tag.is_empty() {
                        "UDP"
                    } else {
                        &plan.tag
                    },
                    discovery_table
                ));
            }
            logln(&format!(
                "    [{}] 模式={:?}，目标={}，流成功={success}/{n}，最低有效流数={required}，TX均值={}，TX-P10={}，RX均值={}，RX-P10={}，覆盖率={:.1}%，结果={}",
                if plan.tag.is_empty() {
                    "UDP"
                } else {
                    &plan.tag
                },
                first.rate_mode,
                fmt_opt(first.rx_target_mbps),
                fmt_opt(tx_stats.avg_mbps),
                fmt_opt(tx_stats.p10_mbps),
                fmt_opt(rx_avg),
                fmt_opt(rx_stats.p10_mbps),
                rx_stats.coverage * 100.0,
                verdict.label()
            ));

            for flow in &leg_flows {
                self.push_row(Row {
                    sort_key: (useq, plan.lidx, flow.stream_pos + 1, 0),
                    time: now_full(),
                    task_id: md5_hex(&format!("{}|{}|{}", unit.id, plan.tag, flow.stream_pos)),
                    parent_id: unit.id.clone(),
                    task: unit.title.clone(),
                    ip: if flow.task.v6 {
                        "V6".into()
                    } else {
                        "V4".into()
                    },
                    transport: "UDP".into(),
                    param: format!(
                        "{} (#{}; retry={})",
                        flow.task.profile_label,
                        flow.stream_pos + 1,
                        flow.retries
                    ),
                    src_pc: flow.task.src.pc.clone(),
                    src_iface: flow.task.src.nic.name.clone(),
                    src_ip: flow.task.src.nic.ipv4.clone(),
                    dst_pc: flow.task.dst.pc.clone(),
                    dst_iface: flow.task.dst.nic.name.clone(),
                    dst_ip: flow.task.dst.nic.ipv4.clone(),
                    verdict: if flow.raw_ok {
                        Verdict::Pass
                    } else {
                        Verdict::SetupError
                    },
                    execution_status: if flow.raw_ok {
                        ExecutionStatus::Completed
                    } else if flow.client.timed_out {
                        ExecutionStatus::TimedOut
                    } else if flow.client.cancelled {
                        ExecutionStatus::Cancelled
                    } else {
                        ExecutionStatus::Error
                    },
                    reason_code: if flow.raw_ok {
                        String::new()
                    } else {
                        "FLOW_FAILED".into()
                    },
                    reason_detail: flow.error.clone(),
                    kind_label: if unit.bidir {
                        format!("★★双向灌包-{}(流明细)", plan.tag)
                    } else {
                        "灌包(流明细)".into()
                    },
                    tx_mbps: flow.parsed.best_sender(),
                    rx_mbps: flow.parsed.best_receiver(),
                    udp_loss: flow.parsed.udp_loss_pct,
                    requested_streams: 1,
                    active_streams: usize::from(flow.raw_ok),
                    required_streams: 1,
                    retry_count: flow.retries,
                    command: flow.client.cmd.clone(),
                    raws: vec![
                        (
                            format!(
                                "iperf3 client{} 流#{} 输出",
                                fmt_tag(&plan.tag),
                                flow.stream_pos + 1
                            ),
                            format!("$ {}\n{}", flow.client.cmd, flow.client.output),
                        ),
                        (
                            format!(
                                "iperf3 server{} 流#{} 输出",
                                fmt_tag(&plan.tag),
                                flow.stream_pos + 1
                            ),
                            flow.server_output.clone(),
                        ),
                        (
                            format!("流事件{} #{}", fmt_tag(&plan.tag), flow.stream_pos + 1),
                            format_flow_events(&flow.events, &flow.error),
                        ),
                    ],
                    ..Default::default()
                });
            }

            let (screenshot_master, screenshot_agent) = if self.cfg.screenshot {
                self.take_screenshots(
                    &[first.dst.side, first.src.side],
                    &format!("{}_{}", unit.title, plan.tag),
                )
            } else {
                (String::new(), String::new())
            };
            let idx = self.push_row(Row {
                sort_key: (useq, plan.lidx, n + 1, 1),
                time: now_full(),
                task_id: md5_hex(&format!("{}|{}|grouptotal", unit.id, plan.tag)),
                parent_id: unit.id.clone(),
                task: unit.title.clone(),
                ip: if first.v6 { "V6".into() } else { "V4".into() },
                transport: "UDP".into(),
                param: format!(
                    "★组合计({} 共{}条流，成功{}，要求至少{})",
                    plan.name, n, success, required
                ),
                src_pc: first.src.pc.clone(),
                src_iface: first.src.nic.name.clone(),
                src_ip: first.src.nic.ipv4.clone(),
                dst_pc: first.dst.pc.clone(),
                dst_iface: first.dst.nic.name.clone(),
                dst_ip: first.dst.nic.ipv4.clone(),
                verdict,
                execution_status: if success == 0 {
                    ExecutionStatus::Error
                } else if success < n {
                    ExecutionStatus::Partial
                } else {
                    ExecutionStatus::Completed
                },
                reason_code: reason_code.clone(),
                reason_detail: reason_detail.clone(),
                kind_label: if unit.bidir {
                    format!("★组合计-{}", plan.tag)
                } else {
                    "★组合计".into()
                },
                rx_avg,
                requested_streams: n,
                active_streams: success,
                required_streams: required,
                retry_count: leg_flows.iter().map(|flow| flow.retries).sum(),
                target_mbps: first.rx_target_mbps,
                tx_avg: tx_stats.avg_mbps,
                tx_p10: tx_stats.p10_mbps,
                rx_p10: rx_stats.p10_mbps,
                rx_median: rx_stats.median_mbps,
                rx_p95: rx_stats.p95_mbps,
                rx_min: rx_stats.min_mbps,
                rx_max: rx_stats.max_mbps,
                effective_seconds: Some(
                    effective_window
                        .available_secs
                        .min(effective_window.required_secs as f64),
                ),
                required_seconds: Some(effective_window.required_secs as f64),
                sample_coverage: Some(rx_stats.coverage),
                udp_loss,
                screenshot_master,
                screenshot_agent,
                is_grouptotal: true,
                raws: if discovery_table.is_empty() {
                    vec![]
                } else {
                    vec![("streams_active -> RX 速率".into(), discovery_table)]
                },
                ..Default::default()
            });
            outcomes.push(LegOutcome {
                verdict,
                reason_code,
                reason_detail,
                rx_avg,
                main_rows: vec![idx],
                tag: plan.tag.clone(),
            });
        }
        outcomes
    }
}

/// v6 link-local 地址加 zone（仅 macOS 需要，Windows 不加）
fn add_zone(addr: &str, zone: &str, _side: Side) -> String {
    if cfg!(target_os = "macos") && !zone.is_empty() && addr.starts_with("fe80") {
        format!("{}%{}", addr, zone)
    } else {
        addr.to_string()
    }
}

fn fmt_tag(tag: &str) -> String {
    if tag.is_empty() {
        String::new()
    } else {
        format!("-{tag}")
    }
}

fn fmt_opt(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.3}Mbps"),
        None => "-".into(),
    }
}

fn text_preview(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn aggregate_unit_verdict(outcomes: &[LegOutcome]) -> Verdict {
    if outcomes.is_empty() {
        return Verdict::SetupError;
    }
    for verdict in [
        Verdict::SetupError,
        Verdict::NotEvaluated,
        Verdict::RateFail,
        Verdict::Unstable,
        Verdict::Measured,
    ] {
        if outcomes.iter().any(|outcome| outcome.verdict == verdict) {
            return verdict;
        }
    }
    if outcomes
        .iter()
        .all(|outcome| outcome.verdict == Verdict::Pass)
    {
        Verdict::Pass
    } else {
        Verdict::NotEvaluated
    }
}

fn outcome_matching_verdict(outcomes: &[LegOutcome], verdict: Verdict) -> Option<&LegOutcome> {
    outcomes.iter().find(|outcome| outcome.verdict == verdict)
}

fn count_retry_events(events: &[IperfFlowEvent]) -> usize {
    events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Retry)
        .count()
}

fn rate_sample_coverage_sufficient(
    rx_stats: &RateStats,
    tx_stats: &RateStats,
    target_present: bool,
) -> bool {
    rx_stats.coverage >= MIN_RATE_SAMPLE_COVERAGE
        && (!target_present || tx_stats.coverage >= MIN_RATE_SAMPLE_COVERAGE)
}

fn should_retry_udp_flow(
    attempt: usize,
    max_retries: usize,
    elapsed: Duration,
    startup_timeout: Duration,
    client: &IperfClientOut,
) -> bool {
    attempt < max_retries && elapsed <= startup_timeout && !client.timed_out && !client.cancelled
}

fn required_udp_streams(
    requested: usize,
    rate_cfg: &RateCheckCfg,
    target_mbps: Option<f64>,
    per_stream_mbps: Option<f64>,
) -> usize {
    if requested <= 1 {
        return requested;
    }
    let min_concurrent = (rate_cfg.min_concurrent_streams as usize).clamp(1, requested);
    // 用“允许失败数向上取整”体现用户容错：
    // ratio=0.90 时，5 条允许 1 条失败，20 条允许 2 条失败；
    // 2 条仍受 min_concurrent=2 约束，必须两条都通。
    let ratio = rate_cfg.min_active_ratio.clamp(0.0, 1.0);
    let allowed_failures = ((requested as f64) * (1.0 - ratio)).ceil() as usize;
    let fidelity_required = requested.saturating_sub(allowed_failures);
    let load_required = match (target_mbps, per_stream_mbps) {
        (Some(target), Some(per_stream)) if per_stream > 0.0 => {
            let offered = target * (1.0 + rate_cfg.offered_headroom_pct.max(0.0) / 100.0);
            (offered / per_stream).ceil() as usize
        }
        _ => 0,
    };
    min_concurrent.max(fidelity_required).max(load_required)
}

fn discovery_stage(stream_pos: usize, total: usize) -> u64 {
    if total <= 1 {
        return 0;
    }
    let ordinal = stream_pos + 1;
    let q1 = ((total as f64) * 0.25).ceil() as usize;
    let q2 = ((total as f64) * 0.50).ceil() as usize;
    let q3 = ((total as f64) * 0.75).ceil() as usize;
    if ordinal <= q1 {
        0
    } else if ordinal <= q2 {
        1
    } else if ordinal <= q3 {
        2
    } else {
        3
    }
}

fn format_flow_events(events: &[IperfFlowEvent], error: &str) -> String {
    let mut out = String::new();
    for event in events {
        out.push_str(&format!(
            "{:>8.3}s  {:?}{}  {}\n",
            event.elapsed_ms as f64 / 1000.0,
            event.kind,
            event
                .mbps
                .map(|v| format!(" {:.3}Mbps", v))
                .unwrap_or_default(),
            event.line
        ));
    }
    if !error.is_empty() {
        out.push_str(&format!("ERROR: {error}\n"));
    }
    out
}

fn iperf_interval_end_ms(line: &str) -> Option<u64> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    fields.windows(2).find_map(|pair| {
        if pair[1] != "sec" {
            return None;
        }
        let (_, end) = pair[0].split_once('-')?;
        let end = end
            .trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '.' && ch != ',')
            .replace(',', ".")
            .parse::<f64>()
            .ok()?;
        (end.is_finite() && end > 0.0).then_some((end * 1_000.0).round() as u64)
    })
}

fn flow_duration_is_plausible(start_ms: u64, end_ms: u64, expected_ms: u64) -> bool {
    end_ms > start_ms
        && end_ms
            .saturating_sub(start_ms)
            .saturating_add(FLOW_TIMELINE_TOLERANCE_MS)
            >= expected_ms
}

fn flow_active_interval(flow: &UdpFlowRun) -> Option<(u64, u64)> {
    if !flow.raw_ok {
        return None;
    }
    let latest_retry_ms = flow
        .events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Retry)
        .map(|event| event.elapsed_ms)
        .max();
    let retry_cutoff = latest_retry_ms.unwrap_or(0);
    let end = flow
        .events
        .iter()
        .rev()
        .find(|event| event.kind == IperfEventKind::Ended && event.elapsed_ms >= retry_cutoff)
        .map(|event| event.elapsed_ms)?;
    let expected_ms = flow.task.duration.saturating_mul(1_000);

    let started = flow
        .events
        .iter()
        .rev()
        .find(|event| {
            event.kind == IperfEventKind::Started
                && event.elapsed_ms >= retry_cutoff
                && event.elapsed_ms < end
        })
        .map(|event| event.elapsed_ms);
    let attempt_floor = started.unwrap_or(retry_cutoff);
    let connected = flow
        .events
        .iter()
        .find(|event| {
            event.kind == IperfEventKind::Connected
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms < end
        })
        .map(|event| event.elapsed_ms);
    let traffic_events: Vec<&IperfFlowEvent> = flow
        .events
        .iter()
        .filter(|event| {
            event.kind == IperfEventKind::Traffic
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms <= end
                && event.mbps.unwrap_or(0.0) > 0.0
        })
        .collect();
    let first_traffic = traffic_events.first().map(|event| event.elapsed_ms);

    // interval 行内的时间是 iperf 进程自己的测量时间，不受 stdout 块缓冲影响。
    // 优先用最终汇总区间反推起流时刻；只有区间覆盖了用户要求的有效时长才采用，
    // 避免把一次过早结束的短测量误扩成完整测试。
    let reported_duration_ms = traffic_events
        .iter()
        .filter_map(|event| iperf_interval_end_ms(&event.line))
        .max();
    if let Some(duration_ms) = reported_duration_ms
        .filter(|duration_ms| duration_ms.saturating_add(FLOW_TIMELINE_TOLERANCE_MS) >= expected_ms)
    {
        let start = end.saturating_sub(duration_ms).max(attempt_floor);
        if end > start {
            return Some((start, end));
        }
    }

    // 支持 --forceflush 时首条 Traffic 的到达时间接近真实时间；旧版会在退出时
    // 一次性吐出全部 interval，此时 active duration 会明显短于 task.duration。
    if let Some(start) =
        first_traffic.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }
    if let Some(start) =
        connected.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }
    if let Some(start) =
        started.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }
    if let Some(start) =
        latest_retry_ms.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }

    // 测试确实提前结束时保留最保守的可观察起点，使有效窗口保持不足。
    let start = first_traffic.or(connected).or(started)?;
    (end > start).then_some((start, end))
}

fn nearest_valid_sample(
    out: &MonitorStopOut,
    elapsed_ms: u64,
    max_distance_ms: u64,
) -> Option<&MonitorSample> {
    out.samples
        .iter()
        .filter(|sample| sample.valid)
        .min_by_key(|sample| sample.elapsed_ms.abs_diff(elapsed_ms))
        .filter(|sample| sample.elapsed_ms.abs_diff(elapsed_ms) <= max_distance_ms)
}

fn select_udp_effective_window(
    plans: &[UdpLegPlan],
    results: &[UdpFlowRun],
    monitors: &HashMap<String, MonitorStopOut>,
    rate_cfg: &RateCheckCfg,
) -> EffectiveWindow {
    let required_secs = plans
        .iter()
        .flat_map(|plan| plan.streams.iter().map(|task| task.duration))
        .max()
        .unwrap_or(0);
    let mut lower = 0u64;
    let mut upper = u64::MAX;
    for plan in plans {
        let Some(first) = plan.streams.first() else {
            continue;
        };
        let Some(out) = monitors.get(&first.dst.key()) else {
            return EffectiveWindow {
                required_secs,
                ..Default::default()
            };
        };
        let Some(first_sample) = out.samples.iter().find(|sample| sample.valid) else {
            return EffectiveWindow {
                required_secs,
                ..Default::default()
            };
        };
        let Some(last_sample) = out.samples.iter().rev().find(|sample| sample.valid) else {
            return EffectiveWindow {
                required_secs,
                ..Default::default()
            };
        };
        lower = lower.max(first_sample.elapsed_ms);
        upper = upper.min(last_sample.elapsed_ms);
    }
    if upper <= lower || upper == u64::MAX {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    }
    let sample_tolerance_ms = rate_cfg
        .sample_interval_ms
        .clamp(200, 5_000)
        .saturating_mul(2)
        .max(1_500);

    let eligible = |t: u64| -> bool {
        plans.iter().enumerate().all(|(leg_pos, plan)| {
            let first = plan.streams.first();
            let required = required_udp_streams(
                plan.streams.len(),
                rate_cfg,
                first.and_then(|task| task.rx_target_mbps),
                first.and_then(|task| task.offered_mbps),
            );
            let active = results
                .iter()
                .filter(|flow| flow.leg_pos == leg_pos)
                .filter_map(flow_active_interval)
                .filter(|(start, end)| *start <= t && t < *end)
                .count();
            if active < required {
                return false;
            }
            let Some(first) = first else {
                return false;
            };
            monitors
                .get(&first.dst.key())
                .and_then(|out| nearest_valid_sample(out, t, sample_tolerance_ms))
                .is_some()
        })
    };

    let mut best_start = 0u64;
    let mut best_end = 0u64;
    let mut current_start: Option<u64> = None;
    let mut t = lower;
    while t <= upper {
        if eligible(t) {
            if current_start.is_none() {
                current_start = Some(t);
            }
        } else if let Some(start) = current_start.take() {
            if t.saturating_sub(start) > best_end.saturating_sub(best_start) {
                best_start = start;
                best_end = t;
            }
        }
        t = t.saturating_add(1_000);
    }
    if let Some(start) = current_start {
        let end = upper.saturating_add(1_000);
        if end.saturating_sub(start) > best_end.saturating_sub(best_start) {
            best_start = start;
            best_end = end;
        }
    }

    if best_end <= best_start {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    }

    let scored_start = best_start.saturating_add(rate_cfg.settle_secs.saturating_mul(1_000));
    let available_ms = best_end.saturating_sub(scored_start);
    let available_secs = available_ms as f64 / 1000.0;
    let complete = available_ms >= required_secs.saturating_mul(1_000);
    let scored_end = if complete {
        scored_start.saturating_add(required_secs.saturating_mul(1_000))
    } else {
        best_end
    };
    EffectiveWindow {
        start_ms: scored_start,
        end_ms: scored_end,
        available_secs,
        required_secs,
        complete,
    }
}

fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = (((sorted.len() - 1) as f64) * q.clamp(0.0, 1.0)).round() as usize;
    sorted.get(idx).copied()
}

fn aggregate_udp_loss(flows: &[&UdpFlowRun]) -> Option<f64> {
    let successful: Vec<&UdpFlowRun> = flows.iter().copied().filter(|flow| flow.raw_ok).collect();
    if successful.is_empty() {
        return None;
    }
    let counts: Vec<(u64, u64)> = successful
        .iter()
        .filter_map(|flow| {
            Some((
                flow.parsed.udp_lost_datagrams?,
                flow.parsed.udp_total_datagrams?,
            ))
        })
        .collect();
    if counts.len() == successful.len() {
        let lost: u64 = counts.iter().map(|(lost, _)| *lost).sum();
        let total: u64 = counts.iter().map(|(_, total)| *total).sum();
        if total > 0 {
            return Some(lost as f64 * 100.0 / total as f64);
        }
    }

    let percentages: Vec<f64> = successful
        .iter()
        .filter_map(|flow| flow.parsed.udp_loss_pct)
        .collect();
    (percentages.len() == successful.len())
        .then(|| percentages.iter().sum::<f64>() / percentages.len() as f64)
}

fn rolling_time_window_averages(
    samples: &[(u64, u64, f64)],
    range_start_ms: u64,
    window_ms: u64,
) -> Vec<f64> {
    if window_ms == 0 {
        return samples.iter().map(|(_, _, rate)| *rate).collect();
    }

    let mut rolling = Vec::new();
    for (window_end_ms, _, _) in samples {
        let window_start_ms = window_end_ms.saturating_sub(window_ms);
        if window_start_ms < range_start_ms
            || window_end_ms.saturating_sub(window_start_ms) < window_ms
        {
            continue;
        }

        let mut weighted_sum = 0.0;
        let mut covered_ms = 0u64;
        for (sample_end_ms, interval_ms, rate) in samples {
            if *interval_ms == 0 || *sample_end_ms <= window_start_ms {
                continue;
            }
            if *sample_end_ms > *window_end_ms {
                break;
            }
            let sample_start_ms = sample_end_ms
                .saturating_sub(*interval_ms)
                .max(range_start_ms);
            let overlap_start = sample_start_ms.max(window_start_ms);
            let overlap_end = (*sample_end_ms).min(*window_end_ms);
            let overlap_ms = overlap_end.saturating_sub(overlap_start);
            if overlap_ms > 0 {
                weighted_sum += *rate * overlap_ms as f64;
                covered_ms = covered_ms.saturating_add(overlap_ms);
            }
        }
        // 只把实际样本完整覆盖的五秒区间纳入稳定性判定；缺口由 coverage
        // 另行约束，不能用相邻样本跨越缺口拼出一个虚假的五秒窗口。
        // elapsed_ms/interval_ms 均由 Duration 向下取整为毫秒，多个样本边界可能
        // 累积出数毫秒的舍入缝隙；只容忍极小误差，不能容忍真正的漏采周期。
        if covered_ms.saturating_add(ROLLING_COVERAGE_TOLERANCE_MS) >= window_ms {
            rolling.push(weighted_sum / covered_ms as f64);
        }
    }
    rolling
}

fn monitor_rate_stats(
    out: &MonitorStopOut,
    window: &EffectiveWindow,
    rx: bool,
    first_active_ms: u64,
) -> RateStats {
    if window.end_ms <= window.start_ms {
        return RateStats::default();
    }
    let mut baseline_values: Vec<f64> = out
        .samples
        .iter()
        .filter(|sample| sample.valid && sample.elapsed_ms < first_active_ms)
        .map(|sample| if rx { sample.rx_mbps } else { sample.tx_mbps })
        .collect();
    baseline_values.sort_by(|a, b| a.total_cmp(b));
    let baseline = percentile(&baseline_values, 0.5).unwrap_or(0.0);

    let mut rate_samples: Vec<(u64, u64, f64)> = out
        .samples
        .iter()
        .filter(|sample| {
            sample.valid
                && sample.elapsed_ms >= window.start_ms
                && sample.elapsed_ms <= window.end_ms
        })
        .map(|sample| {
            let value = if rx { sample.rx_mbps } else { sample.tx_mbps };
            (
                sample.elapsed_ms,
                sample.interval_ms,
                (value - baseline).max(0.0),
            )
        })
        .collect();
    rate_samples.sort_by_key(|(elapsed_ms, _, _)| *elapsed_ms);
    let mut rates: Vec<f64> = rate_samples.iter().map(|(_, _, rate)| *rate).collect();
    if rates.is_empty() {
        return RateStats::default();
    }
    let avg = rates.iter().sum::<f64>() / rates.len() as f64;
    let min = rates.iter().copied().fold(f64::INFINITY, f64::min);
    let max = rates.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let rolling = {
        let timed =
            rolling_time_window_averages(&rate_samples, window.start_ms, ROLLING_RATE_WINDOW_MS);
        if timed.is_empty() {
            rates.clone()
        } else {
            timed
        }
    };
    rates.sort_by(|a, b| a.total_cmp(b));
    let mut rolling_sorted = rolling;
    rolling_sorted.sort_by(|a, b| a.total_cmp(b));
    let mut sample_intervals_ms: Vec<f64> = out
        .samples
        .iter()
        .filter(|sample| {
            sample.interval_ms > 0
                && sample.elapsed_ms >= window.start_ms
                && sample.elapsed_ms <= window.end_ms
        })
        .map(|sample| sample.interval_ms as f64)
        .collect();
    sample_intervals_ms.sort_by(|a, b| a.total_cmp(b));
    let nominal_interval_ms = percentile(&sample_intervals_ms, 0.50)
        .unwrap_or(1_000.0)
        .max(1.0);
    // 窗口两端均包含样本，因此完整覆盖的期望数量为 duration/interval + 1。
    // 不能写死 1 秒，否则 500ms 会掩盖丢样，2s/5s 会被误判覆盖不足。
    let expected_samples =
        ((window.end_ms - window.start_ms) as f64 / nominal_interval_ms).floor() + 1.0;
    RateStats {
        avg_mbps: Some(avg),
        p10_mbps: percentile(&rolling_sorted, 0.10),
        median_mbps: percentile(&rates, 0.50),
        p95_mbps: percentile(&rates, 0.95),
        min_mbps: Some(min),
        max_mbps: Some(max),
        coverage: (rates.len() as f64 / expected_samples).min(1.0),
    }
}

fn active_rate_table(
    leg_pos: usize,
    flows: &[&UdpFlowRun],
    out: &MonitorStopOut,
    first_active_ms: u64,
) -> String {
    let mut baseline_values: Vec<f64> = out
        .samples
        .iter()
        .filter(|sample| sample.valid && sample.elapsed_ms < first_active_ms)
        .map(|sample| sample.rx_mbps)
        .collect();
    baseline_values.sort_by(|a, b| a.total_cmp(b));
    let baseline = percentile(&baseline_values, 0.5).unwrap_or(0.0);
    let mut groups: HashMap<usize, Vec<f64>> = HashMap::new();
    for sample in out.samples.iter().filter(|sample| sample.valid) {
        let active = flows
            .iter()
            .filter(|flow| flow.leg_pos == leg_pos)
            .filter_map(|flow| flow_active_interval(flow))
            .filter(|(start, end)| *start <= sample.elapsed_ms && sample.elapsed_ms < *end)
            .count();
        if active > 0 {
            groups
                .entry(active)
                .or_default()
                .push((sample.rx_mbps - baseline).max(0.0));
        }
    }
    let mut keys: Vec<usize> = groups.keys().copied().collect();
    keys.sort_unstable();
    let mut lines = vec!["active_streams,samples,avg_rx_mbps,p10_rx_mbps".to_string()];
    for active in keys {
        let mut values = groups.remove(&active).unwrap_or_default();
        if values.is_empty() {
            continue;
        }
        let avg = values.iter().sum::<f64>() / values.len() as f64;
        values.sort_by(|a, b| a.total_cmp(b));
        let p10 = percentile(&values, 0.10).unwrap_or(0.0);
        lines.push(format!("{active},{},{avg:.3},{p10:.3}", values.len()));
    }
    lines.join("\n")
}

// ---------------- 结果库（RESUME 用） ----------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DbEnt {
    pub ok: bool,
    pub time: String,
    pub title: String,
}

pub struct ResultDb {
    path: PathBuf,
    map: HashMap<String, DbEnt>,
}

pub const RESUME_MAX_AGE_HOURS: i64 = 24;

impl ResultDb {
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        ResultDb { path, map }
    }

    /// 24 小时内 PASS 过则返回该次时间
    pub fn fresh_pass(&self, id: &str) -> Option<String> {
        let e = self.map.get(id)?;
        if !e.ok {
            return None;
        }
        let t = chrono::NaiveDateTime::parse_from_str(&e.time, "%Y-%m-%d %H:%M:%S").ok()?;
        let now = chrono::Local::now().naive_local();
        let age = now.signed_duration_since(t);
        if age.num_hours() <= RESUME_MAX_AGE_HOURS && age.num_seconds() >= -60 {
            Some(e.time.clone())
        } else {
            None
        }
    }

    pub fn set(&mut self, id: &str, ok: bool, title: &str) {
        self.map.insert(
            id.to_string(),
            DbEnt {
                ok,
                time: now_full(),
                title: title.to_string(),
            },
        );
    }

    /// 原子写（tmp + rename）
    pub fn save(&self) {
        let tmp = self.path.with_extension("tmp");
        if let Ok(text) = serde_json::to_string_pretty(&self.map) {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master::builder::Endpoint;
    use crate::protocol::NicInfo;

    fn endpoint(side: Side, name: &str, ip: &str) -> Endpoint {
        Endpoint {
            side,
            pc: side.cn().into(),
            nic: NicInfo {
                name: name.into(),
                role: "SGMII2.5G".into(),
                ipv4: ip.into(),
                speed_mbps: 2500,
                ..Default::default()
            },
        }
    }

    fn udp_plan(
        lidx: usize,
        tag: &str,
        count: usize,
        src: &Endpoint,
        dst: &Endpoint,
        duration: u64,
    ) -> UdpLegPlan {
        let streams = (0..count)
            .map(|stream_idx| IperfTask {
                v6: false,
                udp: true,
                profile_name: "udp_b500m".into(),
                profile_label: "UDP -b 500m".into(),
                src: src.clone(),
                dst: dst.clone(),
                port: 56_000 + (lidx * 100 + stream_idx) as u16,
                duration,
                extra: vec!["-b".into(), "500m".into()],
                stream_idx,
                rate_mode: RateMode::Observe,
                rx_target_mbps: None,
                offered_mbps: Some(500.0),
            })
            .collect();
        UdpLegPlan {
            lidx,
            tag: tag.into(),
            name: "udp_b500m".into(),
            streams,
        }
    }

    fn udp_flow(
        leg_pos: usize,
        stream_pos: usize,
        task: &IperfTask,
        start_ms: u64,
        end_ms: u64,
        raw_ok: bool,
    ) -> UdpFlowRun {
        UdpFlowRun {
            leg_pos,
            stream_pos,
            task: task.clone(),
            raw_ok,
            parsed: iperf::IperfParsed::default(),
            client: IperfClientOut::default(),
            server_output: String::new(),
            events: if raw_ok {
                vec![
                    IperfFlowEvent {
                        kind: IperfEventKind::Traffic,
                        elapsed_ms: start_ms,
                        mbps: Some(500.0),
                        line: "traffic".into(),
                    },
                    IperfFlowEvent {
                        kind: IperfEventKind::Ended,
                        elapsed_ms: end_ms,
                        line: "ended".into(),
                        ..Default::default()
                    },
                ]
            } else {
                vec![]
            },
            retries: 0,
            error: String::new(),
        }
    }

    fn monitor_until(end_ms: u64, rx_mbps: f64, tx_mbps: f64) -> MonitorStopOut {
        MonitorStopOut {
            samples: (0..=end_ms / 1_000)
                .map(|second| MonitorSample {
                    elapsed_ms: second * 1_000,
                    interval_ms: 1_000,
                    rx_mbps,
                    tx_mbps,
                    valid: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_result_db() {
        let dir = std::env::temp_dir().join("cpe_db_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("task_results.json");
        let _ = std::fs::remove_file(&p);
        let mut db = ResultDb::load(p.clone());
        db.set("abc", true, "t1");
        db.save();
        let db2 = ResultDb::load(p.clone());
        assert!(db2.fresh_pass("abc").is_some());
        assert!(db2.fresh_pass("nope").is_none());
        let mut db3 = ResultDb::load(p.clone());
        db3.set("abc", false, "t1");
        db3.save();
        let db4 = ResultDb::load(p.clone());
        assert!(db4.fresh_pass("abc").is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_required_udp_stream_quorum() {
        let cfg = RateCheckCfg::default();
        assert_eq!(required_udp_streams(2, &cfg, None, Some(500.0)), 2);
        assert_eq!(required_udp_streams(5, &cfg, None, Some(500.0)), 4);
        assert_eq!(
            required_udp_streams(20, &cfg, Some(8400.0), Some(500.0)),
            18
        );
        assert_eq!(
            required_udp_streams(20, &cfg, Some(6400.0), Some(500.0)),
            18
        );
    }

    #[test]
    fn test_two_stream_direction_retries_but_never_degrades_to_one_stream_verdict() {
        let cfg = RateCheckCfg::default();
        let client = IperfClientOut::default();
        assert!(should_retry_udp_flow(
            0,
            cfg.flow_retries as usize,
            Duration::from_secs(2),
            Duration::from_secs(cfg.startup_timeout_secs),
            &client,
        ));
        assert_eq!(required_udp_streams(2, &cfg, None, Some(500.0)), 2);

        let timed_out = IperfClientOut {
            timed_out: true,
            ..Default::default()
        };
        assert!(!should_retry_udp_flow(
            0,
            1,
            Duration::from_secs(2),
            Duration::from_secs(15),
            &timed_out,
        ));
        assert!(!should_retry_udp_flow(
            0,
            1,
            Duration::from_secs(16),
            Duration::from_secs(15),
            &client,
        ));
    }

    #[test]
    fn test_discovery_stages_are_quartered() {
        let stages_20: Vec<u64> = (0..20).map(|idx| discovery_stage(idx, 20)).collect();
        assert_eq!(&stages_20[0..5], &[0; 5]);
        assert_eq!(&stages_20[5..10], &[1; 5]);
        assert_eq!(&stages_20[10..15], &[2; 5]);
        assert_eq!(&stages_20[15..20], &[3; 5]);
        assert_eq!(
            (0..5)
                .map(|idx| discovery_stage(idx, 5))
                .collect::<Vec<_>>(),
            vec![0, 0, 1, 2, 3]
        );
    }

    #[test]
    fn test_bidir_5_and_2_streams_require_both_streams_on_small_leg() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plans = vec![
            udp_plan(0, "ab", 5, &master, &agent, 180),
            udp_plan(1, "ba", 2, &agent, &master, 180),
        ];
        let mut results = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            for (stream_pos, task) in plan.streams.iter().enumerate() {
                results.push(udp_flow(leg_pos, stream_pos, task, 1_000, 190_000, true));
            }
        }
        let monitors = HashMap::from([
            (agent.key(), monitor_until(190_000, 2_000.0, 2_000.0)),
            (master.key(), monitor_until(190_000, 2_000.0, 2_000.0)),
        ]);
        let window =
            select_udp_effective_window(&plans, &results, &monitors, &RateCheckCfg::default());
        assert!(window.complete);
        assert_eq!(window.start_ms, 6_000);
        assert_eq!(window.end_ms, 186_000);
        assert_eq!(window.available_secs, 184.0);

        let failed_small_leg_flow = results
            .iter_mut()
            .find(|flow| flow.leg_pos == 1 && flow.stream_pos == 1)
            .unwrap();
        failed_small_leg_flow.raw_ok = false;
        failed_small_leg_flow.events.clear();
        let no_common_window =
            select_udp_effective_window(&plans, &results, &monitors, &RateCheckCfg::default());
        assert!(!no_common_window.complete);
        assert_eq!(no_common_window.available_secs, 0.0);
        assert_eq!(no_common_window.start_ms, 0);
        assert_eq!(no_common_window.end_ms, 0);
    }

    #[test]
    fn test_effective_window_short_when_one_direction_drops_early() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plans = vec![
            udp_plan(0, "ab", 2, &master, &agent, 180),
            udp_plan(1, "ba", 2, &agent, &master, 180),
        ];
        let mut results = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            for (stream_pos, task) in plan.streams.iter().enumerate() {
                let end_ms = if leg_pos == 1 && stream_pos == 1 {
                    175_000
                } else {
                    190_000
                };
                results.push(udp_flow(leg_pos, stream_pos, task, 1_000, end_ms, true));
            }
        }
        let monitors = HashMap::from([
            (agent.key(), monitor_until(190_000, 2_000.0, 2_000.0)),
            (master.key(), monitor_until(190_000, 2_000.0, 2_000.0)),
        ]);
        let window =
            select_udp_effective_window(&plans, &results, &monitors, &RateCheckCfg::default());
        assert!(!window.complete);
        assert_eq!(window.available_secs, 169.0);
    }

    #[test]
    fn test_effective_window_supports_five_second_monitor_interval() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plans = vec![udp_plan(0, "ab", 2, &master, &agent, 180)];
        let results: Vec<UdpFlowRun> = plans[0]
            .streams
            .iter()
            .enumerate()
            .map(|(stream_pos, task)| udp_flow(0, stream_pos, task, 1_000, 190_000, true))
            .collect();
        let monitors = HashMap::from([(
            agent.key(),
            MonitorStopOut {
                samples: (0..=38)
                    .map(|idx| MonitorSample {
                        elapsed_ms: idx * 5_000,
                        interval_ms: 5_000,
                        rx_mbps: 1_000.0,
                        valid: true,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
        )]);
        let cfg = RateCheckCfg {
            sample_interval_ms: 5_000,
            ..Default::default()
        };
        let window = select_udp_effective_window(&plans, &results, &monitors, &cfg);
        assert!(window.complete);
        assert_eq!(window.end_ms - window.start_ms, 180_000);
    }

    #[test]
    fn test_rate_stats_subtract_background_and_report_p10() {
        let out = MonitorStopOut {
            samples: vec![
                (0, 100.0),
                (1_000, 100.0),
                (2_000, 100.0),
                (3_000, 1_100.0),
                (4_000, 1_000.0),
                (5_000, 1_200.0),
                (6_000, 1_100.0),
            ]
            .into_iter()
            .map(|(elapsed_ms, rx_mbps)| MonitorSample {
                elapsed_ms,
                interval_ms: 1_000,
                rx_mbps,
                valid: true,
                ..Default::default()
            })
            .collect(),
            ..Default::default()
        };
        let window = EffectiveWindow {
            start_ms: 3_000,
            end_ms: 6_000,
            available_secs: 3.0,
            required_secs: 3,
            complete: true,
        };
        let stats = monitor_rate_stats(&out, &window, true, 3_000);
        assert_eq!(stats.avg_mbps, Some(1_000.0));
        assert_eq!(stats.p10_mbps, Some(900.0));
        assert_eq!(stats.median_mbps, Some(1_000.0));
        assert_eq!(stats.coverage, 1.0);
    }

    #[test]
    fn test_sample_coverage_uses_actual_monitor_interval() {
        let window = EffectiveWindow {
            start_ms: 0,
            end_ms: 10_000,
            available_secs: 10.0,
            required_secs: 10,
            complete: true,
        };
        let mut out = MonitorStopOut {
            samples: (0..=5)
                .map(|idx| MonitorSample {
                    elapsed_ms: idx * 2_000,
                    interval_ms: 2_000,
                    rx_mbps: 1_000.0,
                    valid: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let complete = monitor_rate_stats(&out, &window, true, 0);
        assert_eq!(complete.coverage, 1.0);

        out.samples[2].valid = false;
        let missing_one = monitor_rate_stats(&out, &window, true, 0);
        assert!((missing_one.coverage - 5.0 / 6.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_target_requires_tx_and_rx_sample_coverage() {
        let rx_stats = RateStats {
            coverage: 1.0,
            ..Default::default()
        };
        let sparse_tx_stats = RateStats {
            coverage: 0.2,
            p10_mbps: Some(10_000.0),
            ..Default::default()
        };
        assert!(!rate_sample_coverage_sufficient(
            &rx_stats,
            &sparse_tx_stats,
            true
        ));
        assert!(rate_sample_coverage_sufficient(
            &rx_stats,
            &sparse_tx_stats,
            false
        ));

        let complete_tx_stats = RateStats {
            coverage: MIN_RATE_SAMPLE_COVERAGE,
            ..Default::default()
        };
        assert!(rate_sample_coverage_sufficient(
            &rx_stats,
            &complete_tx_stats,
            true
        ));
    }

    #[test]
    fn test_five_second_rolling_p10_uses_sample_time_coverage() {
        let fast_out = MonitorStopOut {
            samples: (0..=50)
                .map(|idx| MonitorSample {
                    elapsed_ms: idx * 200,
                    interval_ms: 200,
                    rx_mbps: if (21..=25).contains(&idx) { 0.0 } else { 100.0 },
                    valid: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let fast_window = EffectiveWindow {
            start_ms: 0,
            end_ms: 10_000,
            available_secs: 10.0,
            required_secs: 10,
            complete: true,
        };
        let fast_stats = monitor_rate_stats(&fast_out, &fast_window, true, 0);
        let fast_p10 = fast_stats.p10_mbps.unwrap();
        assert!(
            (80.0..90.0).contains(&fast_p10),
            "200ms 采样应将 1 秒掉速按五秒窗口摊薄，实际 P10={fast_p10}"
        );

        let rounded_intervals: Vec<(u64, u64, f64)> =
            (1..=5).map(|second| (second * 1_000, 999, 100.0)).collect();
        assert_eq!(
            rolling_time_window_averages(&rounded_intervals, 0, 5_000),
            vec![100.0]
        );

        let slow_out = MonitorStopOut {
            samples: [0.0, 100.0, 100.0, 100.0, 100.0]
                .into_iter()
                .enumerate()
                .map(|(idx, rx_mbps)| MonitorSample {
                    elapsed_ms: (idx as u64 + 1) * 5_000,
                    interval_ms: 5_000,
                    rx_mbps,
                    valid: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let slow_window = EffectiveWindow {
            start_ms: 0,
            end_ms: 25_000,
            available_secs: 25.0,
            required_secs: 25,
            complete: true,
        };
        let slow_stats = monitor_rate_stats(&slow_out, &slow_window, true, 0);
        assert_eq!(slow_stats.p10_mbps, Some(0.0));
    }

    #[test]
    fn test_udp_loss_uses_complete_weighted_datagram_counts() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plan = udp_plan(0, "ab", 2, &master, &agent, 10);
        let mut first = udp_flow(0, 0, &plan.streams[0], 0, 10_000, true);
        first.parsed.udp_lost_datagrams = Some(10);
        first.parsed.udp_total_datagrams = Some(100);
        first.parsed.udp_loss_pct = Some(10.0);
        let mut second = udp_flow(0, 1, &plan.streams[1], 0, 10_000, true);
        second.parsed.udp_lost_datagrams = Some(0);
        second.parsed.udp_total_datagrams = Some(900);
        second.parsed.udp_loss_pct = Some(0.0);
        assert_eq!(aggregate_udp_loss(&[&first, &second]), Some(1.0));

        second.parsed.udp_lost_datagrams = None;
        second.parsed.udp_total_datagrams = None;
        assert_eq!(aggregate_udp_loss(&[&first, &second]), Some(5.0));

        second.parsed.udp_loss_pct = None;
        assert_eq!(aggregate_udp_loss(&[&first, &second]), None);
    }

    #[test]
    fn test_flow_interval_uses_traffic_after_latest_retry() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plan = udp_plan(0, "ab", 1, &master, &agent, 180);
        let mut flow = udp_flow(0, 0, &plan.streams[0], 1_000, 10_000, true);
        flow.events.insert(
            1,
            IperfFlowEvent {
                kind: IperfEventKind::Retry,
                elapsed_ms: 2_000,
                line: "retry".into(),
                ..Default::default()
            },
        );
        flow.events.insert(
            2,
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 3_000,
                mbps: Some(500.0),
                line: "traffic after retry".into(),
            },
        );
        assert_eq!(flow_active_interval(&flow), Some((3_000, 10_000)));
    }

    #[test]
    fn test_flow_interval_falls_back_to_connected_for_buffered_output() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plan = udp_plan(0, "ab", 1, &master, &agent, 180);
        let mut flow = udp_flow(0, 0, &plan.streams[0], 179_000, 180_000, true);
        flow.events.insert(
            0,
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 1_000,
                line: "connected".into(),
                ..Default::default()
            },
        );
        // Traffic 虽存在，但到达时刻只比 Ended 早 1 秒，不能代表 180 秒测试的起流时刻。
        assert_eq!(flow_active_interval(&flow), Some((1_000, 180_000)));

        flow.events
            .retain(|event| event.kind != IperfEventKind::Traffic);
        assert_eq!(flow_active_interval(&flow), Some((1_000, 180_000)));
    }

    #[test]
    fn test_flow_interval_uses_iperf_interval_when_all_output_is_buffered() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plan = udp_plan(0, "ab", 1, &master, &agent, 180);
        // 块缓冲刷新和 Ended 可能落在同一毫秒；仍应使用行内 205 秒区间反推。
        let mut flow = udp_flow(0, 0, &plan.streams[0], 215_000, 215_000, true);
        flow.events[0].line = "[  5]   0.00-205.00 sec  12.0 GBytes  500 Mbits/sec sender".into();
        assert_eq!(flow_active_interval(&flow), Some((10_000, 215_000)));
    }

    #[test]
    fn test_retry_count_includes_client_and_group_retry_events() {
        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Retry,
                line: "client retry".into(),
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Retry,
                line: "group retry".into(),
                ..Default::default()
            },
        ];
        assert_eq!(count_retry_events(&events), 2);
    }

    #[test]
    fn test_unit_reason_matches_aggregate_verdict_priority() {
        let outcomes = vec![
            LegOutcome {
                verdict: Verdict::RateFail,
                reason_code: "RX_BELOW_TARGET".into(),
                reason_detail: "AB rate failed".into(),
                rx_avg: None,
                main_rows: vec![],
                tag: "AB".into(),
            },
            LegOutcome {
                verdict: Verdict::SetupError,
                reason_code: "NO_STREAM_STARTED".into(),
                reason_detail: "BA setup failed".into(),
                rx_avg: None,
                main_rows: vec![],
                tag: "BA".into(),
            },
        ];
        let verdict = aggregate_unit_verdict(&outcomes);
        assert_eq!(verdict, Verdict::SetupError);
        assert_eq!(
            outcome_matching_verdict(&outcomes, verdict)
                .unwrap()
                .reason_code,
            "NO_STREAM_STARTED"
        );
    }

    #[test]
    fn test_text_preview_is_utf8_safe() {
        assert_eq!(text_preview("截图失败：权限不足", 4), "截图失败");
        assert_eq!(text_preview("short", 100), "short");
    }
}
