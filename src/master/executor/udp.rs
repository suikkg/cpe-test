//! UDP 灌包专属：并发流计划、重试口径、丢包汇总。
//!
//! UDP 有一条 TCP 没有的判定分支——「发送端灌够了没有」。灌不够时接收端低于
//! 目标不能算被测设备不达标，必须判 NOT_EVALUATED；这条口径的谓词都在这里。

use super::*;

#[derive(Clone)]
pub(super) struct UdpLegPlan {
    pub(super) lidx: usize,
    pub(super) tag: String,
    pub(super) name: String,
    pub(super) streams: Vec<IperfTask>,
}

#[derive(Clone)]
pub(super) struct PreparedUdpFlow {
    pub(super) leg_pos: usize,
    pub(super) stream_pos: usize,
    /// 方向标签（ab/ba，单向为空）。双向两腿并行时，日志必须能区分是哪一腿的
    /// attempt/retry，否则 master.log 里两个 #1 完全分不开。
    pub(super) tag: String,
    pub(super) task: IperfTask,
    pub(super) server_req: Option<IperfServerStartReq>,
    pub(super) client_req: Option<IperfClientReq>,
    pub(super) server_error: String,
    pub(super) launch_delay_ms: u64,
    pub(super) strict_single_stream: bool,
}

pub(super) struct UdpFlowRun {
    pub(super) leg_pos: usize,
    pub(super) stream_pos: usize,
    pub(super) task: IperfTask,
    /// 本轮选中 attempt 是否有 iperf3 client/server 自身吞吐证据。
    pub(super) raw_ok: bool,
    /// 已有工具测量，但 client 非正常完成/超时；不能再伪装成“无测量”。
    pub(super) runtime_failed: bool,
    pub(super) parsed: iperf::IperfParsed,
    pub(super) client: IperfClientOut,
    pub(super) server_output: String,
    pub(super) events: Vec<IperfFlowEvent>,
    pub(super) retries: usize,
    /// 实际启动 client 的完整外层尝试次数（不含 iperf3 内部瞬态重试）。
    pub(super) full_attempts: usize,
    /// 单流方向已在每次资源清理均确认的前提下耗尽强制尝试预算。
    pub(super) single_stream_exhausted: bool,
    pub(super) error: String,
}

#[cfg(test)]
pub(super) fn count_retry_events(events: &[IperfFlowEvent]) -> usize {
    events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Retry)
        .count()
}

pub(super) fn should_retry_udp_flow(
    attempt: usize,
    max_retries: usize,
    elapsed: Duration,
    startup_timeout: Duration,
    client: &IperfClientOut,
) -> bool {
    attempt < max_retries && elapsed <= startup_timeout && !client.timed_out && !client.cancelled
}

pub(super) fn effective_udp_retries(
    configured_retries: usize,
    strict_single_stream: bool,
) -> usize {
    if strict_single_stream {
        configured_retries.max(SINGLE_UDP_MIN_ATTEMPTS.saturating_sub(1) as usize)
    } else {
        configured_retries
    }
}

pub(super) fn zero_udp_stream_verdict(requested: usize, attempts_exhausted: bool) -> Verdict {
    if requested == 1 && attempts_exhausted {
        Verdict::RateFail
    } else {
        Verdict::SetupError
    }
}

// 「灌够了没有」的口径现在是全仓共享的（ADR-12(c)）：定义搬到了
// `rate_window`，连同 `RX_TRACKS_TX_RATIO`（接收端跟得上发送端的比例，
// 与 `offered_headroom_pct` 无关，也不该跟着它走）。以前它们只存在于这里，
// 而 `evaluate_nic_rx` 只查 TX 覆盖率不查 TX 水平，于是 CTS 路径上零防护。
pub(super) use crate::master::rate_window::offered_shortfall_explains_rx;

pub(super) fn required_udp_streams(
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

pub(super) fn udp_flow_detail_outcome(
    flow: &UdpFlowRun,
    strict_single_failed: bool,
) -> (Verdict, ReasonCode, String) {
    if flow.runtime_failed {
        (
            Verdict::RateFail,
            ReasonCode::IperfRuntimeErrors,
            flow.error.clone(),
        )
    } else if flow.raw_ok {
        (
            Verdict::Measured,
            ReasonCode::FlowMeasured,
            "流量工具已产生吞吐测量；此行仅记录单流执行，单元验收以接收端 OS 网卡 RX 组合计为准"
                .into(),
        )
    } else if strict_single_failed {
        (
            Verdict::RateFail,
            ReasonCode::SingleUdpStreamFailed,
            flow.error.clone(),
        )
    } else {
        (
            Verdict::SetupError,
            ReasonCode::FlowFailed,
            flow.error.clone(),
        )
    }
}

pub(super) fn aggregate_udp_loss(flows: &[&UdpFlowRun]) -> Option<f64> {
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
    if counts.len() != successful.len() {
        // 任何一条流缺计数就不给聚合值。此前这里回退到「对各流百分比取算术
        // 平均」，那是错误加权：100 个数据报丢 10% 和 900 个数据报丢 0%，
        // 真实聚合是 1%，平均出来却是 5%。宁可报「未知」也不报错的数。
        return None;
    }
    let lost: u64 = counts.iter().map(|(lost, _)| *lost).sum();
    let total: u64 = counts.iter().map(|(_, total)| *total).sum();
    (total > 0).then(|| lost as f64 * 100.0 / total as f64)
}

impl Ctx {
    pub(super) fn udp_leg_plans(&self, unit: &Unit) -> Option<Vec<UdpLegPlan>> {
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

    pub(super) fn start_udp_server_with_retry(
        &self,
        task: &IperfTask,
        base_req: &IperfServerStartReq,
        max_retries: usize,
    ) -> Result<IperfServerStartReq, String> {
        let mut errors = Vec::new();
        for attempt in 0..=max_retries {
            let mut req = base_req.clone();
            if attempt > 0 {
                req.request_id = format!("{}-start{attempt}", base_req.request_id);
            }
            match self.server_start(task.dst.side, &req) {
                Ok(_) => return Ok(req),
                Err(e) => {
                    errors.push(format!("第{}次: {e}", attempt + 1));
                    if attempt < max_retries {
                        // server_start 的各实现本身会做失败补偿；这里再用同一
                        // request_id 做一次幂等确认，作为“允许占用同端口的新
                        // request 开始”的硬门槛。无法确认退出时绝不盲目重试。
                        if let Err(cleanup_error) = self.server_stop_confirmed(
                            task.dst.side,
                            req.port,
                            &req.request_id,
                            Duration::ZERO,
                        ) {
                            errors.push(format!(
                                "第{}次失败后的 server 清理未确认，禁止继续占用端口 {} 重试: {}",
                                attempt + 1,
                                req.port,
                                cleanup_error
                            ));
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        }
        Err(errors.join("；"))
    }

    pub(super) fn run_prepared_udp_flow(
        &self,
        prepared: PreparedUdpFlow,
        epoch: &Instant,
        live: &Arc<Mutex<HashMap<(usize, usize), LiveFlowState>>>,
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
                runtime_failed: false,
                parsed: iperf::IperfParsed::default(),
                client: IperfClientOut {
                    output: prepared.server_error.clone(),
                    ..Default::default()
                },
                server_output: String::new(),
                events: vec![],
                retries: 0,
                full_attempts: 0,
                single_stream_exhausted: false,
                error: prepared.server_error,
            };
        }

        std::thread::sleep(Duration::from_millis(prepared.launch_delay_ms));
        let mut current_server_req = prepared.server_req.clone().unwrap();
        let client_req = prepared.client_req.clone().unwrap();
        let mut all_events = Vec::new();
        let mut all_client_output = Vec::new();
        let mut all_server_output = Vec::new();
        let mut final_client = IperfClientOut::default();
        let mut final_parsed = iperf::IperfParsed::default();
        let mut final_ok = false;
        let mut final_runtime_failed = false;
        let mut retries = 0usize;
        let mut full_attempts = 0usize;
        let mut cleanup_confirmed = false;
        let mut setup_error_seen = false;
        let mut final_error = String::new();

        let max_flow_retries = effective_udp_retries(
            self.cfg.iperf.rate_check.flow_retries as usize,
            prepared.strict_single_stream,
        );
        let retry_cutoff =
            Duration::from_secs(self.cfg.iperf.rate_check.startup_timeout_secs.max(1));
        for attempt in 0..=max_flow_retries {
            let attempt_start_ms = epoch.elapsed().as_millis() as u64;
            let key = (prepared.leg_pos, prepared.stream_pos);
            let live_ref = Arc::clone(live);
            let mut attempt_events: Vec<IperfFlowEvent> = Vec::new();
            let attempt_started = Instant::now();
            let client_request_id = lifecycle_request_id(
                &current_server_req.owner_id,
                "client",
                prepared.task.port,
                attempt,
            );
            let client = self.client_run_tracked(
                prepared.task.src.side,
                &client_req,
                &current_server_req.owner_id,
                &client_request_id,
                current_server_req.lease_secs,
                |mut event| {
                    event.elapsed_ms = event.elapsed_ms.saturating_add(attempt_start_ms);
                    if let Ok(mut g) = live_ref.lock() {
                        let state = g.entry(key).or_default();
                        apply_flow_event(state, &event);
                    }
                    attempt_events.push(event);
                },
            );
            all_events.extend(attempt_events);
            all_client_output.push(format!(
                "=== attempt {} ===\n{}",
                attempt + 1,
                client.output
            ));
            let stop = self.server_stop_confirmed(
                prepared.task.dst.side,
                prepared.task.port,
                &current_server_req.request_id,
                Duration::ZERO,
            );
            let (server_out, stop_ok) = match stop {
                Ok(out) => (out.output, true),
                Err(e) => (format!("server 停止未确认: {e}"), false),
            };
            let parsed = iperf::parse_output(&format!("{}\n{}", client.output, server_out));
            let tool_measurement = parsed.has_measurement();
            let client_setup_error = iperf_client_setup_error(&client);
            let process_started = client.process_started == Some(true);
            let client_cleanup_confirmed = client.cleanup_confirmed == Some(true);
            let safe_full_attempt = process_started
                && client_cleanup_confirmed
                && stop_ok
                && client_setup_error.is_none()
                && !client.cancelled;
            if safe_full_attempt {
                full_attempts += 1;
            }
            cleanup_confirmed = stop_ok && client_cleanup_confirmed;
            final_ok = tool_measurement && safe_full_attempt;
            final_runtime_failed = final_ok && (!client.ok || client.timed_out);
            final_client = client;
            final_parsed = parsed;
            all_server_output.push(format!("=== attempt {} ===\n{}", attempt + 1, server_out));
            if !stop_ok {
                setup_error_seen = true;
                final_error = "server 停止未确认，禁止在同端口继续重试".into();
                break;
            }
            if let Some(error) = client_setup_error {
                setup_error_seen = true;
                final_error = error;
                break;
            }
            if !process_started {
                setup_error_seen = true;
                final_error = "client 未明确证明底层进程已成功启动".into();
                break;
            }
            if !client_cleanup_confirmed {
                setup_error_seen = true;
                final_error = "client 未明确证明底层进程已 wait/reap，禁止复用端口".into();
                break;
            }
            // 只要本轮已有 iperf3 自身测量，就已经证明该方向灌通；后续由
            // runtime/loss/目标判定真实结果，不能继续重试并声称“无测量”。
            if tool_measurement {
                final_error = if final_runtime_failed {
                    final_client
                        .output
                        .lines()
                        .find(|line| line.to_ascii_lowercase().contains("error"))
                        .unwrap_or("iperf3 已有吞吐测量，但 client 未正常完成")
                        .to_string()
                } else {
                    String::new()
                };
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

            let retryable = if prepared.strict_single_stream {
                // 单流硬门槛必须完成至少三次安全尝试；不受普通 startup
                // 截止或单次命令超时影响。显式取消/清理不确定时仍立即停下。
                attempt < max_flow_retries && safe_full_attempt
            } else {
                safe_full_attempt
                    && should_retry_udp_flow(
                        attempt,
                        max_flow_retries,
                        attempt_started.elapsed(),
                        retry_cutoff,
                        &final_client,
                    )
            };
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
                "    [UDP流重试]{} {}-#{} 本轮未跑通，重新启动 server/client（{}/{}）",
                fmt_tag_bracket(&prepared.tag),
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
            let mut next_server_req = current_server_req.clone();
            next_server_req.request_id = lifecycle_request_id(
                &current_server_req.owner_id,
                "server",
                prepared.task.port,
                attempt + 1,
            );
            let server_retries =
                effective_udp_retries(UDP_SERVER_START_RETRIES, prepared.strict_single_stream);
            match self.start_udp_server_with_retry(&prepared.task, &next_server_req, server_retries)
            {
                Ok(started_req) => current_server_req = started_req,
                Err(e) => {
                    final_error = format!("重试时 server 启动失败: {e}");
                    break;
                }
            }
        }

        final_client.output = all_client_output.join("\n");
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

        let single_stream_exhausted = prepared.strict_single_stream
            && !final_ok
            && !final_parsed.has_measurement()
            && full_attempts == max_flow_retries.saturating_add(1)
            && cleanup_confirmed
            && !final_client.cancelled
            && !setup_error_seen;
        UdpFlowRun {
            leg_pos: prepared.leg_pos,
            stream_pos: prepared.stream_pos,
            task: prepared.task,
            raw_ok: final_ok,
            runtime_failed: final_runtime_failed,
            parsed: final_parsed,
            client: final_client,
            server_output: all_server_output.join("\n"),
            events: all_events,
            retries: full_attempts.saturating_sub(1),
            full_attempts,
            single_stream_exhausted,
            error: final_error,
        }
    }

    pub(super) fn run_udp_unit(
        &self,
        useq: usize,
        unit: &Unit,
        plans: &[UdpLegPlan],
        owner_id: &str,
        lease_secs: u64,
    ) -> Vec<LegOutcome> {
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
                            .saturating_mul(rate_cfg.discovery_step_secs)
                            .saturating_mul(1_000)
                    } else {
                        0
                    };
                    launch_delays.insert(
                        (leg_pos, stream_pos),
                        stage_delay.saturating_add(
                            slot.saturating_mul(rate_cfg.launch_interval_ms.clamp(0, 1_000)),
                        ),
                    );
                    slot += 1;
                }
            }
        }
        let max_launch_delay_ms = launch_delays.values().copied().max().unwrap_or(0);

        let mut prepared: Vec<PreparedUdpFlow> = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            for (stream_pos, task) in plan.streams.iter().enumerate() {
                let strict_single_stream = plan.streams.len() == 1;
                let launch_delay_ms = launch_delays
                    .get(&(leg_pos, stream_pos))
                    .copied()
                    .unwrap_or(0);
                let remaining_launch_secs = max_launch_delay_ms
                    .saturating_sub(launch_delay_ms)
                    .div_ceil(1000);
                // duration 对用户表示有效测量时长。更早启动的流自动多跑，
                // 让 discover 阶梯、错峰、settle 和配置的快速重试后仍有共同窗口。
                let process_duration = task
                    .duration
                    .saturating_add(rate_cfg.startup_timeout_secs)
                    .saturating_add(rate_cfg.settle_secs)
                    .saturating_add(5)
                    .saturating_add(remaining_launch_secs);
                match self.build_iperf_requests(task, process_duration, owner_id, lease_secs, 0) {
                    Ok((server_req, client_req)) => prepared.push(PreparedUdpFlow {
                        leg_pos,
                        stream_pos,
                        tag: plan.tag.clone(),
                        task: task.clone(),
                        server_req: Some(server_req),
                        client_req: Some(client_req),
                        server_error: String::new(),
                        launch_delay_ms,
                        strict_single_stream,
                    }),
                    Err(e) => prepared.push(PreparedUdpFlow {
                        leg_pos,
                        stream_pos,
                        tag: plan.tag.clone(),
                        task: task.clone(),
                        server_req: None,
                        client_req: None,
                        server_error: e,
                        launch_delay_ms: 0,
                        strict_single_stream,
                    }),
                }
            }
        }

        prepared = std::thread::scope(|scope| {
            let handles: Vec<_> = prepared
                .into_iter()
                .map(|mut flow| {
                    scope.spawn(move || {
                        if let Some(req) = flow.server_req.clone() {
                            let server_retries = effective_udp_retries(
                                UDP_SERVER_START_RETRIES,
                                flow.strict_single_stream,
                            );
                            match catch_unwind(AssertUnwindSafe(|| {
                                self.start_udp_server_with_retry(&flow.task, &req, server_retries)
                            })) {
                                Ok(Ok(started_req)) => flow.server_req = Some(started_req),
                                Ok(Err(e)) => {
                                    flow.server_error = e;
                                    flow.server_req = None;
                                    flow.client_req = None;
                                }
                                Err(payload) => {
                                    flow.server_error = format!(
                                        "server 准备线程 panic: {}",
                                        panic_text(payload.as_ref())
                                    );
                                    flow.server_req = None;
                                    flow.client_req = None;
                                }
                            }
                        }
                        flow
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| unreachable!("准备线程已内部隔离 panic"))
                })
                .collect()
        });

        let server_ready = prepared
            .iter()
            .filter(|flow| flow.server_req.is_some())
            .count();
        logln(&format!(
            "    server 准备完成: {server_ready}/{total_flows}"
        ));

        let mut monitor_ids: HashMap<String, (Side, String, u64, String)> = HashMap::new();
        for plan in plans {
            for task in &plan.streams {
                for endpoint in [&task.src, &task.dst] {
                    let key = endpoint.key();
                    if monitor_ids.contains_key(&key) {
                        continue;
                    }
                    let before_ms = epoch.elapsed().as_millis() as u64;
                    match self.mon_start(endpoint.side, &endpoint.nic.name, owner_id, lease_secs) {
                        Ok((id, call_origin_ms)) => {
                            monitor_ids.insert(
                                key,
                                (
                                    endpoint.side,
                                    id,
                                    before_ms + call_origin_ms,
                                    endpoint.nic.name.clone(),
                                ),
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

        let live: Arc<Mutex<HashMap<(usize, usize), LiveFlowState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let results: Vec<UdpFlowRun> = std::thread::scope(|scope| {
            let handles: Vec<_> = prepared
                .into_iter()
                .map(|flow| {
                    let live = Arc::clone(&live);
                    let fallback = (
                        flow.leg_pos,
                        flow.stream_pos,
                        flow.task.clone(),
                        flow.server_req.clone(),
                    );
                    scope.spawn(move || {
                        catch_unwind(AssertUnwindSafe(|| {
                            self.run_prepared_udp_flow(flow, &epoch, &live)
                        }))
                        .unwrap_or_else(|payload| {
                            if let Some(req) = &fallback.3 {
                                let _ = self.server_stop_confirmed(
                                    fallback.2.dst.side,
                                    fallback.2.port,
                                    &req.request_id,
                                    Duration::ZERO,
                                );
                            }
                            UdpFlowRun {
                                leg_pos: fallback.0,
                                stream_pos: fallback.1,
                                task: fallback.2,
                                raw_ok: false,
                                runtime_failed: false,
                                parsed: iperf::IperfParsed::default(),
                                client: IperfClientOut {
                                    output: format!(
                                        "UDP 流线程 panic: {}",
                                        panic_text(payload.as_ref())
                                    ),
                                    ..Default::default()
                                },
                                server_output: String::new(),
                                events: vec![],
                                retries: 0,
                                full_attempts: 0,
                                single_stream_exhausted: false,
                                error: "UDP 流线程 panic".into(),
                            }
                        })
                    })
                })
                .collect();

            let mut monitor_status_disabled = HashSet::new();
            while handles.iter().any(|h| !h.is_finished()) {
                std::thread::sleep(Duration::from_secs(1));
                for (leg_pos, plan) in plans.iter().enumerate() {
                    let (connected, active, ended, iperf_mbps, errors) = {
                        let g = live.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        let mut connected = 0usize;
                        let mut active = 0usize;
                        let mut ended = 0usize;
                        let mut rate = 0.0;
                        let mut has_rate = false;
                        let mut errors = 0usize;
                        for stream_pos in 0..plan.streams.len() {
                            if let Some(state) = g.get(&(leg_pos, stream_pos)) {
                                connected += usize::from(state.connected);
                                active += usize::from(state.active && !state.ended);
                                ended += usize::from(state.ended);
                                if let Some(value) = active_iperf_rate(state) {
                                    rate += value;
                                    has_rate = true;
                                }
                                errors += usize::from(!state.error.is_empty());
                            }
                        }
                        (connected, active, ended, has_rate.then_some(rate), errors)
                    };
                    let mut monitor_error = String::new();
                    let nic_rx_mbps = plan.streams.first().and_then(|task| {
                        let key = task.dst.key();
                        let (side, id, _, _) = monitor_ids.get(&key)?;
                        if monitor_status_disabled.contains(&key) {
                            return None;
                        }
                        match self.mon_status(*side, id) {
                            Ok(status) => match status.latest_sample {
                                Some(sample) if sample.valid => Some(sample.rx_mbps),
                                Some(sample) => {
                                    monitor_error = if sample.error.is_empty() {
                                        "网卡样本无效".into()
                                    } else {
                                        sample.error
                                    };
                                    None
                                }
                                None => {
                                    monitor_error = "等待首个网卡样本".into();
                                    None
                                }
                            },
                            Err(error) => {
                                monitor_status_disabled.insert(key);
                                monitor_error = error;
                                None
                            }
                        }
                    });
                    logln(&format_iperf_progress(&IperfProgressSnapshot {
                        protocol: "UDP",
                        tag: &plan.tag,
                        active,
                        connected,
                        total: plan.streams.len(),
                        ended,
                        nic_rx_mbps,
                        iperf_mbps,
                        errors,
                        monitor_error,
                    }));
                }
            }
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| unreachable!("流线程已内部隔离 panic"))
                })
                .collect()
        });

        let mut monitor_outputs: HashMap<String, MonitorStopOut> = HashMap::new();
        let mut monitor_sample_files: HashMap<String, String> = HashMap::new();
        for (key, (side, id, start_offset_ms, iface)) in monitor_ids {
            match self.mon_stop(side, &id) {
                Ok(mut out) => {
                    for sample in &mut out.samples {
                        sample.elapsed_ms = sample.elapsed_ms.saturating_add(start_offset_ms);
                    }
                    let sample_file = self.save_monitor_samples(
                        owner_id,
                        side,
                        &iface,
                        &key,
                        start_offset_ms,
                        &out,
                    );
                    monitor_sample_files.insert(key.clone(), sample_file);
                    monitor_outputs.insert(key, out);
                }
                Err(e) => logln(&format!("    (网卡监控停止失败: {e})")),
            }
        }

        let windows = select_udp_effective_windows(
            plans,
            &results,
            &monitor_outputs,
            &self.cfg.iperf.rate_check,
        );
        for (leg_pos, window) in windows.per_leg.iter().enumerate() {
            logln(&format!(
                "    有效窗口[{}]: {:.1}s / {}s{}",
                plans
                    .get(leg_pos)
                    .map(|plan| plan.tag.as_str())
                    .unwrap_or("?"),
                window.available_secs,
                window.required_secs,
                if window.complete {
                    "（满足）"
                } else {
                    "（不足，不能正式判定）"
                }
            ));
        }
        if plans.len() > 1 {
            logln(&format!(
                "    双向并发重叠: {:.1}s{}",
                windows.concurrency_secs,
                if windows.concurrency_secs <= 0.0 {
                    "（两条腿没有真正同时在跑，各腿结论只代表单向条件）"
                } else {
                    ""
                }
            ));
        }

        let mut outcomes = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            let effective_window =
                windows
                    .per_leg
                    .get(leg_pos)
                    .cloned()
                    .unwrap_or_else(|| EffectiveWindow {
                        required_secs: plan.streams.first().map(|t| t.duration).unwrap_or(0),
                        ..Default::default()
                    });
            let leg_flows: Vec<&UdpFlowRun> =
                results.iter().filter(|r| r.leg_pos == leg_pos).collect();
            let n = plan.streams.len();
            let success = leg_flows.iter().filter(|r| r.raw_ok).count();
            let runtime_failures = leg_flows.iter().filter(|r| r.runtime_failed).count();
            let single_stream_exhausted = n == 1
                && leg_flows
                    .first()
                    .is_some_and(|flow| flow.single_stream_exhausted);
            let single_attempts = leg_flows
                .first()
                .map(|flow| flow.full_attempts)
                .unwrap_or(0);
            let first = &plan.streams[0];
            let required = required_udp_streams(
                n,
                &self.cfg.iperf.rate_check,
                first.rx_target_mbps,
                first.offered_per_stream_mbps,
            );
            let first_active_ms = leg_flows
                .iter()
                .filter_map(|flow| flow_active_interval(flow).map(|v| v.0))
                .min()
                .unwrap_or(effective_window.start_ms);
            let baseline_cutoff_ms =
                iperf_baseline_cutoff_ms(leg_flows.iter().flat_map(|flow| flow.events.iter()));
            let rx_stats = monitor_outputs
                .get(&first.dst.key())
                .map(|out| monitor_rate_stats(out, &effective_window, true, baseline_cutoff_ms))
                .unwrap_or_default();
            let tx_stats = monitor_outputs
                .get(&first.src.key())
                .map(|out| monitor_rate_stats(out, &effective_window, false, baseline_cutoff_ms))
                .unwrap_or_default();
            let rx_avg = rx_stats.avg_mbps;
            let offered_floor = crate::master::rate_window::offered_floor_mbps(
                first.rx_target_mbps,
                self.cfg.iperf.rate_check.offered_headroom_pct,
            );
            let udp_loss = aggregate_udp_loss(&leg_flows);
            let judgement = udp_leg_verdict(&UdpLegFacts {
                streams_total: n,
                streams_success: success,
                streams_required: required,
                runtime_failures,
                single_stream_exhausted,
                single_attempts,
                window: &effective_window,
                rx: &rx_stats,
                tx: &tx_stats,
                rate_mode: first.rate_mode,
                rx_target_mbps: first.rx_target_mbps,
                offered_floor,
                udp_loss,
                max_udp_loss_pct: self.cfg.iperf.rate_check.max_udp_loss_pct,
                rx_lifecycle_hint: &lifecycle_rx_hint(monitor_outputs.get(&first.dst.key())),
            });
            let (verdict, reason_code, reason_detail) =
                (judgement.verdict, judgement.code, judgement.detail);
            // 「这条腿测到了多少」和「两条腿有没有真正并发」是两件事，必须
            // 分别说清楚。腿级窗口让前者不再被后者连坐，但如果不把后者显式
            // 写出来，读报告的人会把单向条件下的数字当成双向并发结果。
            let reason_detail = if plans.len() > 1 && windows.concurrency_secs <= 0.0 {
                let peers: Vec<&str> = plans
                    .iter()
                    .enumerate()
                    .filter(|(pos, _)| *pos != leg_pos)
                    .map(|(_, other)| other.tag.as_str())
                    .collect();
                let head = format!(
                    "并发重叠 0.0s（对向 {} 没有同时跑通，本行只代表单向条件下的实测）",
                    peers.join("/")
                );
                if reason_detail.is_empty() {
                    head
                } else {
                    format!("{head}；{reason_detail}")
                }
            } else {
                reason_detail
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

            let strict_single_failed = n == 1
                && verdict == Verdict::RateFail
                && reason_code == ReasonCode::SingleUdpStreamFailed;
            for flow in &leg_flows {
                let (flow_verdict, flow_reason_code, flow_reason_detail) =
                    udp_flow_detail_outcome(flow, strict_single_failed);
                let raw_log = self.save_iperf_raw_record(IperfRawArtifact {
                    owner_id,
                    lidx: plan.lidx,
                    stream_pos: flow.stream_pos,
                    tag: &plan.tag,
                    task: &flow.task,
                    client: &flow.client,
                    server_output: &flow.server_output,
                    events: &flow.events,
                    error: &flow.error,
                });
                let nic_samples_rx = monitor_sample_files
                    .get(&flow.task.dst.key())
                    .cloned()
                    .unwrap_or_default();
                self.push_row(Row {
                    verdict: flow_verdict,
                    execution_status: if flow.client.timed_out {
                        ExecutionStatus::TimedOut
                    } else if flow.client.cancelled {
                        ExecutionStatus::Cancelled
                    } else if flow.raw_ok || strict_single_failed {
                        ExecutionStatus::Completed
                    } else {
                        ExecutionStatus::Error
                    },
                    reason_code: flow_reason_code,
                    reason_detail: flow_reason_detail,
                    tx_mbps: flow.parsed.best_sender(),
                    rx_mbps: flow.parsed.best_receiver(),
                    udp_loss: flow.parsed.udp_loss_pct,
                    requested_streams: 1,
                    active_streams: usize::from(flow.raw_ok),
                    required_streams: 1,
                    retry_count: flow.retries,
                    command: flow.client.cmd.clone(),
                    raw_log,
                    nic_samples_rx,
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
                    ..base_row(RowIdentity {
                        unit_seq: useq,
                        leg_index: plan.lidx,
                        stream_index: flow.stream_pos + 1,
                        group_flag: 0,
                        unit,
                        leg_tag: &plan.tag,
                        src: &flow.task.src,
                        dst: &flow.task.dst,
                        ip: if flow.task.v6 {
                            "V6".into()
                        } else {
                            "V4".into()
                        },
                        protocol: RowProtocol::Udp,
                        backend: RowBackend::Iperf3,
                        param: format!(
                            "{} (#{}; retry={})",
                            flow.task.profile_label,
                            flow.stream_pos + 1,
                            flow.retries
                        ),
                        kind_label: if unit.bidir {
                            format!("★★双向灌包-{}(流明细)", plan.tag)
                        } else {
                            "灌包(流明细)".into()
                        },
                        task_id: md5_hex(&format!("{}|{}|{}", unit.id, plan.tag, flow.stream_pos)),
                    })
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
                verdict,
                execution_status: if success == 0 {
                    ExecutionStatus::Error
                } else if success < n {
                    ExecutionStatus::Partial
                } else {
                    ExecutionStatus::Completed
                },
                reason_code,
                reason_detail: reason_detail.clone(),
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
                window_start_ms: Some(effective_window.start_ms),
                window_end_ms: Some(effective_window.end_ms),
                baseline_mbps: Some(rx_stats.baseline_mbps),
                rolling_coverage: Some(rx_stats.rolling_coverage),
                udp_loss,
                screenshot_master,
                screenshot_agent,
                is_grouptotal: true,
                nic_samples_rx: monitor_sample_files
                    .get(&first.dst.key())
                    .cloned()
                    .unwrap_or_default(),
                raws: if discovery_table.is_empty() {
                    vec![]
                } else {
                    vec![("streams_active -> RX 速率".into(), discovery_table)]
                },
                ..base_row(RowIdentity {
                    unit_seq: useq,
                    leg_index: plan.lidx,
                    // 组合计排在同组明细之后：第三位取 n+1，第四位置 1。
                    stream_index: n + 1,
                    group_flag: 1,
                    unit,
                    leg_tag: &plan.tag,
                    src: &first.src,
                    dst: &first.dst,
                    ip: if first.v6 { "V6".into() } else { "V4".into() },
                    protocol: RowProtocol::Udp,
                    backend: RowBackend::Iperf3,
                    param: format!(
                        "★组合计({} 共{}条流，成功{}，要求至少{})",
                        plan.name, n, success, required
                    ),
                    kind_label: if unit.bidir {
                        format!("★组合计-{}", plan.tag)
                    } else {
                        "★组合计".into()
                    },
                    task_id: md5_hex(&format!("{}|{}|grouptotal", unit.id, plan.tag)),
                })
            });
            outcomes.push(LegOutcome {
                judgement: VerdictResult::new(verdict, reason_code, reason_detail),
                rx_avg,
                main_rows: vec![idx],
                tag: plan.tag.clone(),
            });
        }
        outcomes
    }
}
