//! 界面上的实时网卡速率曲线。
//!
//! 和测试执行完全无关：它跑的是独立的采样会话，专门用来在开跑之前肉眼确认
//! 「这块网卡现在有没有流量、跑在什么量级」。

use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct MonitorStartUiReq {
    /// "master" 或 "agent"。
    pub(super) side: String,
    pub(super) iface: String,
    #[serde(default)]
    pub(super) interval_ms: u64,
}

#[derive(Debug, Deserialize)]
pub(super) struct MonitorSessionReq {
    pub(super) session: String,
}

/// 一路监控的取样游标。
#[derive(Debug, Deserialize)]
pub(super) struct MonitorCursor {
    pub(super) session: String,
    #[serde(default)]
    pub(super) from: usize,
}

/// 一次问完全部在跑的监控。
///
/// 每路各发一次请求也能work，但浏览器对同一个源的并发连接就那么几条：
/// 8 路监控 + 运行进度轮询会把它占满，日志那一路开始一秒一顿。
#[derive(Debug, Deserialize)]
pub(super) struct MonitorPollReq {
    #[serde(default)]
    pub(super) cursors: Vec<MonitorCursor>,
}

#[derive(Debug, Serialize)]
pub(super) struct MonitorSeriesOut {
    pub(super) session: String,
    pub(super) side: String,
    pub(super) iface: String,
    pub(super) from: usize,
    pub(super) points: Vec<MonitorPoint>,
    pub(super) running: bool,
    pub(super) error: String,
}

/// 起一路速率监控。
///
/// 有意**不看 `console.running`**：一轮测试跑着的时候正是最想盯速率的时候。
/// 辅测机侧用独立的 owner_id，所以测试收尾那次 owner 范围的清理
/// （executor 侧发 `/resources/cleanup`）不会顺手把它掐掉。
pub(super) fn api_monitor_start(
    console: &Arc<Console>,
    body: &str,
) -> Result<serde_json::Value, String> {
    let req: MonitorStartUiReq =
        serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let iface = req.iface.trim().to_string();
    if iface.is_empty() {
        return Err("先选一块网卡".into());
    }
    // 上限跟着监控端走。辅测机侧的实际采样在 agent 里被夹到 200–5000ms
    // （`MonitorMgr::start_owned`），这里不跟着夹的话，填 10 秒会变成
    // 「agent 按 5 秒采、这边按 10 秒只取最后一个样本」——一半样本无声丢掉。
    // 界面也做了同样的限制，这里是不走界面时的那道。
    let interval_ms = monitor_interval_ms(&req.side, req.interval_ms);
    // 先回收再看上限，且都放在起线程之前：撞上限时不该已经有一条线程
    // 在跑（本机那条会一直读计数器，辅测机那条还占着对面的 monitor 资源）。
    {
        let mut monitors = lock_recover(&console.monitors);
        reap_dead_monitors(&mut monitors);
        if monitors.len() >= MONITOR_MAX_SESSIONS {
            return Err(format!(
                "同时最多 {MONITOR_MAX_SESSIONS} 路监控；先停掉一路再开"
            ));
        }
    }

    let data = Arc::new(Mutex::new(MonitorData {
        running: true,
        ..Default::default()
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let session = format!("mon-{}-{}", std::process::id(), now_millis());

    match req.side.as_str() {
        "master" => spawn_local_monitor(iface.clone(), interval_ms, &stop, &data),
        "agent" => {
            let (host, port, token) = {
                let state = lock_recover(&console.state);
                if state.agent_host.is_empty() {
                    return Err("还没连上辅测机，先点「连接」".into());
                }
                (
                    state.agent_host.clone(),
                    state.cfg.agent_port,
                    state.cfg.agent_token.clone(),
                )
            };
            spawn_agent_monitor(
                host,
                port,
                token,
                iface.clone(),
                interval_ms,
                session.clone(),
                &stop,
                &data,
            );
        }
        other => return Err(format!("未知的监控端: {other}")),
    }

    lock_recover(&console.monitors).insert(
        session.clone(),
        MonitorSession {
            side: req.side.clone(),
            iface,
            stop,
            data,
            started: std::time::Instant::now(),
        },
    );
    Ok(serde_json::json!({ "session": session }))
}

/// 监控端能接受的采样间隔上限。辅测机侧受 agent 自身的夹紧约束。
pub(crate) fn monitor_interval_ms(side: &str, requested: u64) -> u64 {
    let max = if side == "agent" { 5_000 } else { 60_000 };
    if requested == 0 {
        return 1_000;
    }
    requested.clamp(200, max)
}

/// 批量取样。一路会话已经结束不影响其余各路：那一路自己带着 `running:false`
/// 和原因回去，页面把它从图上摘掉即可。整个请求报错的话，正在跑的曲线会一起
/// 断掉，而它们其实好好的。
pub(super) fn api_monitor_samples(
    console: &Arc<Console>,
    body: &str,
) -> Result<serde_json::Value, String> {
    let req: MonitorPollReq =
        serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    if req.cursors.len() > MONITOR_MAX_SESSIONS {
        return Err(format!("一次最多问 {MONITOR_MAX_SESSIONS} 路监控"));
    }
    let mut monitors = lock_recover(&console.monitors);
    // 顺手收摊。页面轮询是这张表唯一的常规活动，回收挂在这里才不会
    // 依赖「有人再开一路监控」才发生。
    reap_dead_monitors(&mut monitors);
    let series: Vec<MonitorSeriesOut> = req
        .cursors
        .iter()
        .map(|cursor| {
            let Some(entry) = monitors.get(&cursor.session) else {
                return MonitorSeriesOut {
                    session: cursor.session.clone(),
                    side: String::new(),
                    iface: String::new(),
                    from: cursor.from,
                    points: Vec::new(),
                    running: false,
                    error: "监控会话已结束".into(),
                };
            };
            let mut data = lock_recover(&entry.data);
            data.last_poll = Some(std::time::Instant::now());
            // 游标是绝对序号；被环形缓冲挤掉的部分直接跳过，不能装作它还在。
            let start = cursor.from.max(data.dropped) - data.dropped;
            let points: Vec<MonitorPoint> = data.points.iter().skip(start).cloned().collect();
            MonitorSeriesOut {
                session: cursor.session.clone(),
                side: entry.side.clone(),
                iface: entry.iface.clone(),
                from: data.dropped + data.points.len(),
                points,
                running: data.running,
                error: data.error.clone().unwrap_or_default(),
            }
        })
        .collect();
    serde_json::to_value(serde_json::json!({ "series": series })).map_err(|e| e.to_string())
}

pub(super) fn api_monitor_stop(
    console: &Arc<Console>,
    body: &str,
) -> Result<serde_json::Value, String> {
    let req: MonitorSessionReq =
        serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let entry = lock_recover(&console.monitors).remove(&req.session);
    let Some(entry) = entry else {
        return Ok(serde_json::json!({ "stopped": false }));
    };
    entry.stop.store(true, Ordering::SeqCst);
    lock_recover(&entry.data).running = false;
    Ok(serde_json::json!({ "stopped": true }))
}

pub(super) fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 本机采样。
///
/// 不能复用 `nic::monitor::run_continuous`：那是给命令行写的，自带阻塞循环、
/// `ctrlc` 处理器和 `println!`——在控制台进程里注册 ctrlc 会和既有的
/// `crate::cancel` 抢同一个信号。这里只用它底层的计数器读取。
pub(super) fn spawn_local_monitor(
    iface: String,
    interval_ms: u64,
    stop: &Arc<AtomicBool>,
    data: &Arc<Mutex<MonitorData>>,
) {
    let stop = Arc::clone(stop);
    let data = Arc::clone(data);
    let _ = std::thread::Builder::new()
        .name("cpe-ui-monitor-local".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let mut last = match crate::nic::monitor::read_counters(&iface) {
                Ok(counters) => (counters, std::time::Instant::now()),
                Err(error) => {
                    let mut d = lock_recover(&data);
                    d.error = Some(format!("读取网卡计数器失败：{error}"));
                    d.running = false;
                    return;
                }
            };
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(interval_ms));
                if stop.load(Ordering::SeqCst) || monitor_abandoned(&data, started) {
                    break;
                }
                let now = std::time::Instant::now();
                match crate::nic::monitor::read_counters(&iface) {
                    Ok((rx, tx)) => {
                        let secs = now.duration_since(last.1).as_secs_f64().max(1e-6);
                        // 计数器回绕/网卡重插会让差值变负，saturating_sub 会把它
                        // 压成 0——报 0 比报一个天文数字好，且下一拍就恢复。
                        let rx_mbps = (rx.saturating_sub(last.0 .0) as f64) * 8.0 / secs / 1e6;
                        let tx_mbps = (tx.saturating_sub(last.0 .1) as f64) * 8.0 / secs / 1e6;
                        last = ((rx, tx), now);
                        let mut d = lock_recover(&data);
                        d.error = None;
                        d.push(MonitorPoint {
                            t: started.elapsed().as_secs_f64(),
                            rx_mbps,
                            tx_mbps,
                        });
                    }
                    Err(error) => {
                        lock_recover(&data).error = Some(format!("采样失败：{error}"));
                    }
                }
            }
            lock_recover(&data).running = false;
        });
}

/// 辅测机采样：复用 agent 已有的 `/monitor/*`，只是换一个独立的 owner_id。
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_agent_monitor(
    host: String,
    port: u16,
    token: String,
    iface: String,
    interval_ms: u64,
    session: String,
    stop: &Arc<AtomicBool>,
    data: &Arc<Mutex<MonitorData>>,
) {
    let stop = Arc::clone(stop);
    let data = Arc::clone(data);
    let _ = std::thread::Builder::new()
        .name("cpe-ui-monitor-agent".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            // owner_id 必须和测试用的那套区分开：主控收尾时按 owner 清理，
            // 共用一个 owner 就会在每轮测试结束时被顺手停掉。
            let owner_id = format!("ui-{session}");
            let start_body = serde_json::json!({
                "iface": iface,
                "interval_ms": interval_ms,
                "owner_id": owner_id,
                // 租约短，靠轮询续。agent 那边每次 /monitor/status 都会刷新
                // last_touch，所以只要这条线程还在轮询就续得上；控制台被 kill
                // 之后再没人来问，辅测机在一个租约周期内自己回收。
                // 给足余量：轮询间隔最大 60s，网络抖动再叠几拍也够不到 180s。
                "lease_secs": UI_MONITOR_LEASE_SECS,
            })
            .to_string();
            let id = match post::<crate::protocol::MonitorStartOut>(
                &host,
                port,
                "/monitor/start",
                &start_body,
                &token,
            ) {
                Ok(out) => out.id,
                Err(error) => {
                    let mut d = lock_recover(&data);
                    d.error = Some(format!("辅测机启动采样失败：{error}"));
                    d.running = false;
                    return;
                }
            };

            let status_body = serde_json::json!({ "id": id }).to_string();
            let mut last_elapsed_ms = u64::MAX;
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(interval_ms));
                if stop.load(Ordering::SeqCst) || monitor_abandoned(&data, started) {
                    break;
                }
                match post::<crate::protocol::MonitorStatusOut>(
                    &host,
                    port,
                    "/monitor/status",
                    &status_body,
                    &token,
                ) {
                    Ok(out) => {
                        let mut d = lock_recover(&data);
                        d.error = None;
                        if let Some(sample) = out.latest_sample {
                            // agent 自己按固定周期采样，这里的轮询和它并不同步，
                            // 同一个样本会被读到两次——按 elapsed_ms 去重。
                            if sample.elapsed_ms != last_elapsed_ms {
                                last_elapsed_ms = sample.elapsed_ms;
                                d.push(MonitorPoint {
                                    t: started.elapsed().as_secs_f64(),
                                    rx_mbps: sample.rx_mbps,
                                    tx_mbps: sample.tx_mbps,
                                });
                            }
                        }
                    }
                    Err(error) => {
                        lock_recover(&data).error = Some(format!("查询辅测机采样失败：{error}"));
                    }
                }
            }
            let stop_body = serde_json::json!({ "id": id }).to_string();
            let _ = post::<crate::protocol::MonitorStopOut>(
                &host,
                port,
                "/monitor/stop",
                &stop_body,
                &token,
            );
            lock_recover(&data).running = false;
        });
}
