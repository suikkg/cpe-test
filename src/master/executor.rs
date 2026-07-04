//! 任务执行器：调度本地/远端的 ping、iperf、监控、截图，产出报告行

use crate::cmd::iperf::{self, IperfServerMgr};
use crate::config::Config;
use crate::http_client;
use crate::master::builder::{v6_addrs, IperfTask, Leg, LegKind, PingTask, Side, Unit};
use crate::nic::monitor::{MonitorMgr, MIN_VALID_RX_MBPS};
use crate::ping;
use crate::protocol::*;
use crate::report::Row;
use crate::util::{find_iperf3, logln, md5_hex, now_compact, now_full, sanitize};
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

pub struct Ctx {
    pub agent_host: String,
    pub agent_port: u16,
    pub cfg: Config,
    pub master_pc: String,
    pub agent_pc: String,
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
    pub skip: usize,
}

struct LegOutcome {
    ok: bool,
    rx_avg: Option<f64>,
    main_rows: Vec<usize>,
    tag: String,
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
        let resp: Resp<TOut> = serde_json::from_str(&text)
            .map_err(|e| format!("辅测机 {path} 响应解析失败: {e}"))?;
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
            Side::Agent => self.agent_post(
                "/ping",
                req,
                Duration::from_secs(req.count as u64 * 5 + 60),
            ),
        }
    }

    fn server_start(&self, side: Side, req: &IperfServerStartReq) -> Result<String, String> {
        match side {
            Side::Master => {
                let bin = find_iperf3().ok_or("主控机未找到 iperf3，请把 iperf3 放到程序同目录")?;
                self.local_servers.start(&bin, req)
            }
            Side::Agent => self
                .agent_post::<_, IperfServerStartOut>("/iperf/server/start", req, Duration::from_secs(40))
                .map(|o| o.cmd),
        }
    }

    fn server_stop(&self, side: Side, port: u16) -> IperfServerStopOut {
        match side {
            Side::Master => self.local_servers.stop(port, Duration::from_secs(3)),
            Side::Agent => self
                .agent_post(
                    "/iperf/server/stop",
                    &IperfServerStopReq {
                        port,
                        wait_secs: 3,
                    },
                    Duration::from_secs(30),
                )
                .unwrap_or_else(|e| IperfServerStopOut {
                    existed: false,
                    output: format!("(获取 server 输出失败: {e})"),
                }),
        }
    }

    fn client_run(&self, side: Side, req: &IperfClientReq) -> IperfClientOut {
        match side {
            Side::Master => {
                let Some(bin) = find_iperf3() else {
                    return IperfClientOut {
                        ok: false,
                        timed_out: false,
                        cmd: String::new(),
                        output: "主控机未找到 iperf3，请把 iperf3 放到程序同目录".into(),
                    };
                };
                iperf::run_client(&bin, req, |line| {
                    if line.contains("/sec") || line.to_lowercase().contains("error") {
                        logln(&format!("      {line}"));
                    }
                })
            }
            Side::Agent => self
                .agent_post(
                    "/iperf/client/run",
                    req,
                    Duration::from_secs(req.duration + 150),
                )
                .unwrap_or_else(|e| IperfClientOut {
                    ok: false,
                    timed_out: false,
                    cmd: String::new(),
                    output: format!("(远端执行失败: {e})"),
                }),
        }
    }

    fn mon_start(&self, side: Side, iface: &str) -> Result<String, String> {
        match side {
            Side::Master => self.local_monitors.start(iface),
            Side::Agent => self
                .agent_post::<_, MonitorStartOut>(
                    "/monitor/start",
                    &MonitorStartReq {
                        iface: iface.to_string(),
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
                    Err(_) => continue,
                },
                Side::Agent => {
                    let body = serde_json::to_string(&ScreenshotReq { label: label.to_string() }).unwrap_or_default();
                    let timeout = Duration::from_secs(180);
                    let (status, text) = match crate::http_client::post_json(
                        &self.agent_host, self.agent_port, "/screenshot", &body, timeout,
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
                        logln(&format!("    [截图] 辅测 HTTP {status}: {}", &text[..std::cmp::min(200, text.len())]));
                        continue;
                    }
                    let resp: Resp<ScreenshotOut> = match serde_json::from_str(&text) {
                        Ok(r) => r,
                        Err(e) => {
                            logln(&format!("    [截图] JSON解析失败: {e}, raw前100字节: {}", &text[..std::cmp::min(100, text.len())]));
                            continue;
                        }
                    };
                    if !resp.ok {
                        logln(&format!("    [截图] 辅测截图错误: {}", resp.error.unwrap_or_default()));
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
                            logln(&format!("    [截图] 辅测 base64 解码失败: {e}, len={b64_len}"));
                            continue;
                        }
                    }
                }
            };
            let (tag, ref mut out_path) = match side {
                Side::Master => ("_master", &mut master),
                Side::Agent => ("_agent", &mut agent),
            };
            let fname = format!("screenshot_{}{}_{}.png", sanitize(label), tag, now_compact());
            let full = self.outdir.join(&fname);
            if std::fs::write(&full, &png).is_err() {
                continue;
            }
            if let Some(dir_name) = self.outdir.file_name() {
                out_path.clear();
                out_path.push_str(&format!("./{}/{}", dir_name.to_string_lossy(), fname));
            }
        }
        (master, agent)
    }

    fn push_row(&self, row: Row) -> usize {
        let mut g = self.rows.lock().unwrap();
        g.push(row);
        g.len() - 1
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
                        ok: None,
                        kind_label: format!("跳过(上次PASS: {t})"),
                        ..Default::default()
                    });
                    continue;
                }
            }

            let outcomes: Vec<LegOutcome> = if unit.legs.len() <= 1 {
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
                        .map(|(li, leg)| {
                            s.spawn(move || self.run_leg(i, unit, li, leg))
                        })
                        .collect();
                    handles.into_iter().map(|h| h.join().unwrap_or(LegOutcome {
                        ok: false,
                        rx_avg: None,
                        main_rows: vec![],
                        tag: String::new(),
                    })).collect()
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

            let unit_ok = !outcomes.is_empty() && outcomes.iter().all(|o| o.ok);
            if unit_ok {
                sum.pass += 1;
            } else {
                sum.fail += 1;
            }
            logln(&format!(
                "  ==> 单元结果: {}",
                if unit_ok { "PASS" } else { "FAIL" }
            ));
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
            LegKind::IperfGroup { name, streams } => {
                self.run_iperf_group(useq, unit, lidx, &leg.tag, name, streams)
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
            out.rtt_avg.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
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
            ok: Some(out.ok),
            kind_label,
            ping_loss: Some(out.loss_pct),
            ping_avg: out.rtt_avg,
            command: out.cmd.clone(),
            raws: vec![(format!("ping{} 输出", fmt_tag(tag)), format!("$ {}\n{}", out.cmd, out.raw))],
            ..Default::default()
        });
        LegOutcome {
            ok: out.ok,
            rx_avg: None,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    // ---------------- iperf 单条 ----------------

    /// 核心执行：server(dst侧) -> client(src侧) -> 停 server。不含监控。
    fn exec_iperf_core(&self, t: &IperfTask) -> (bool, iperf::IperfParsed, IperfClientOut, String) {
        let (client_bind, client_target, server_bind) = if t.v6 {
            match v6_addrs(&t.src.nic, &t.dst.nic) {
                Some(v) => {
                    let bind = add_zone(&v.client_bind, &t.src.nic.zone, t.src.side);
                    let target = add_zone(&v.client_target, &t.src.nic.zone, t.src.side);
                    let srv = add_zone(&v.server_bind, &t.dst.nic.zone, t.dst.side);
                    (bind, target, srv)
                }
                None => {
                    let out = IperfClientOut {
                        ok: false,
                        output: "两端缺少可用 IPv6 地址".into(),
                        ..Default::default()
                    };
                    return (false, iperf::IperfParsed::default(), out, String::new());
                }
            }
        } else {
            (
                t.src.nic.ipv4.clone(),
                t.dst.nic.ipv4.clone(),
                t.dst.nic.ipv4.clone(),
            )
        };

        let sreq = IperfServerStartReq {
            bind_ip: server_bind,
            port: t.port,
            v6: t.v6,
        };
        if let Err(e) = self.server_start(t.dst.side, &sreq) {
            let srv_cmd = format!("iperf3 -s -B {} -p {} {}",
                sreq.bind_ip, sreq.port, if sreq.v6 { "-6" } else { "-4" });
            // 同时构造 client 命令供查错
            let creq = IperfClientReq {
                dst: client_target.clone(),
                bind_ip: client_bind.clone(),
                port: t.port,
                duration: t.duration,
                udp: t.udp,
                v6: t.v6,
                extra: t.extra.clone(),
            };
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

        let creq = IperfClientReq {
            dst: client_target,
            bind_ip: client_bind,
            port: t.port,
            duration: t.duration,
            udp: t.udp,
            v6: t.v6,
            extra: t.extra.clone(),
        };
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

        let meas_ok = parsed.has_measurement()
            || rx_avg.map(|v| v > MIN_VALID_RX_MBPS).unwrap_or(false);
        let ok = raw_ok && meas_ok;

        logln(&format!(
            "    结果: {} 发送={} 接收={} 网卡实测={}",
            if ok { "PASS" } else { "FAIL" },
            fmt_opt(parsed.best_sender()),
            fmt_opt(parsed.best_receiver()),
            fmt_opt(rx_avg)
        ));

        let (screenshot_master, screenshot_agent) = if self.cfg.screenshot {
            self.take_screenshots(&[t.dst.side, t.src.side], &format!("{}_{}", unit.title, tag))
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
            ok: Some(ok),
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
            ok,
            rx_avg,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    // ---------------- iperf 并发组（UDP 多流） ----------------

    fn run_iperf_group(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        tag: &str,
        name: &str,
        streams: &[IperfTask],
    ) -> LegOutcome {
        let n = streams.len();
        let first = &streams[0];
        logln(&format!(
            "  [组{} {}] {} 条流并发: {} -> {} ...",
            fmt_tag(tag),
            name,
            n,
            first.src.brief(),
            first.dst.brief()
        ));
        let mon_id = match self.mon_start(first.dst.side, &first.dst.nic.name) {
            Ok(id) => Some(id),
            Err(e) => {
                logln(&format!("    (接收端网卡监控启动失败: {e})"));
                None
            }
        };

        let results: Vec<(bool, iperf::IperfParsed)> = std::thread::scope(|s| {
            let handles: Vec<_> = streams
                .iter()
                .enumerate()
                .map(|(si, t)| {
                    s.spawn(move || {
                        // 错峰起流，避免同时抢 server 启动
                        std::thread::sleep(Duration::from_millis(200 * si as u64));
                        let time = now_full();
                        let (raw_ok, parsed, client, server_out) = self.exec_iperf_core(t);
                        let ok = raw_ok && parsed.has_measurement();
                        let kind_label = if unit.bidir {
                            format!("★★双向灌包-{tag}(并发组共享网卡采样)")
                        } else {
                            "灌包(并发组共享网卡采样)".into()
                        };
                        self.push_row(Row {
                            sort_key: (useq, lidx, si + 1, 0),
                            time,
                            task_id: md5_hex(&format!("{}|{}|{}", unit.id, tag, si)),
                            parent_id: unit.id.clone(),
                            task: unit.title.clone(),
                            ip: if t.v6 { "V6".into() } else { "V4".into() },
                            transport: "UDP".into(),
                            param: format!("{} (#{})", t.profile_label, si + 1),
                            src_pc: t.src.pc.clone(),
                            src_iface: t.src.nic.name.clone(),
                            src_ip: t.src.nic.ipv4.clone(),
                            dst_pc: t.dst.pc.clone(),
                            dst_iface: t.dst.nic.name.clone(),
                            dst_ip: t.dst.nic.ipv4.clone(),
                            ok: Some(ok),
                            kind_label,
                            tx_mbps: parsed.best_sender(),
                            rx_mbps: parsed.best_receiver(),
                            udp_loss: parsed.udp_loss_pct,
                            command: client.cmd.clone(),
                            raws: vec![
                                (
                                    format!("iperf3 client{} 流#{} 输出", fmt_tag(tag), si + 1),
                                    format!("$ {}\n{}", client.cmd, client.output),
                                ),
                                (
                                    format!("iperf3 server{} 流#{} 输出", fmt_tag(tag), si + 1),
                                    server_out,
                                ),
                            ],
                            ..Default::default()
                        });
                        (ok, parsed)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or((false, iperf::IperfParsed::default())))
                .collect()
        });

        let mon_out = mon_id.and_then(|id| self.mon_stop(first.dst.side, &id).ok());
        let rx_avg = mon_out.as_ref().map(|m| m.avg_mbps);
        let all_ok = results.iter().all(|(ok, _)| *ok);
        let group_ok = rx_avg.map(|v| v > MIN_VALID_RX_MBPS).unwrap_or(false);
        let ok = all_ok && group_ok;

        logln(&format!(
            "    组结果: {} 网卡实测总吞吐={} ({}条流)",
            if ok { "PASS" } else { "FAIL" },
            fmt_opt(rx_avg),
            n
        ));

        let (screenshot_master, screenshot_agent) = if self.cfg.screenshot {
            self.take_screenshots(&[first.dst.side, first.src.side], &format!("{}_{}", unit.title, tag))
        } else {
            (String::new(), String::new())
        };

        let idx = self.push_row(Row {
            sort_key: (useq, lidx, n + 1, 1),
            time: now_full(),
            task_id: md5_hex(&format!("{}|{}|grouptotal", unit.id, tag)),
            parent_id: unit.id.clone(),
            task: unit.title.clone(),
            ip: if first.v6 { "V6".into() } else { "V4".into() },
            transport: "UDP".into(),
            param: format!("★组合计({} 共{}条流)", name, n),
            src_pc: first.src.pc.clone(),
            src_iface: first.src.nic.name.clone(),
            src_ip: first.src.nic.ipv4.clone(),
            dst_pc: first.dst.pc.clone(),
            dst_iface: first.dst.nic.name.clone(),
            dst_ip: first.dst.nic.ipv4.clone(),
            ok: Some(ok),
            kind_label: if unit.bidir {
                format!("★组合计-{tag}")
            } else {
                "★组合计".into()
            },
            rx_avg,
            screenshot_master,
            screenshot_agent,
            is_grouptotal: true,
            ..Default::default()
        });
        LegOutcome {
            ok,
            rx_avg,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
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
}
