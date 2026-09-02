//! iperf3 灌包的执行。
//!
//! 只负责「把流跑起来、把事件流和原始输出收回来」。这里不下任何判定结论——
//! 判定在 [`super::verdict_assembly`] 和 [`crate::master::rate_window`]。

use super::*;

impl Ctx {
    pub(super) fn build_iperf_requests(
        &self,
        t: &IperfTask,
        duration: u64,
        owner_id: &str,
        lease_secs: u64,
        attempt: usize,
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
                request_id: lifecycle_request_id(owner_id, "server", t.port, attempt),
                owner_id: owner_id.to_string(),
                lease_secs,
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
    pub(super) fn exec_iperf_core<F>(
        &self,
        t: &IperfTask,
        owner_id: &str,
        lease_secs: u64,
        epoch: &Instant,
        mut on_event: F,
    ) -> (bool, iperf::IperfParsed, IperfClientOut, String)
    where
        F: FnMut(IperfFlowEvent),
    {
        let (sreq, creq) = match self.build_iperf_requests(t, t.duration, owner_id, lease_secs, 0) {
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
        let client_call_offset_ms = epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mut local_event_origin_ms = None::<u64>;
        let client = self.client_run_tracked(
            t.src.side,
            &creq,
            owner_id,
            &lifecycle_request_id(owner_id, "client", t.port, 0),
            lease_secs,
            |mut event| {
                if t.src.side == Side::Master {
                    // 本机首轮可能在 Started 事件前先执行
                    // `iperf3 --help` 能力探测。以首个回调的当前时刻
                    // 反推 job 零点，不把这段一次性等待计入数据窗口。
                    iperf::align_event_to_epoch(
                        &mut event,
                        epoch.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        &mut local_event_origin_ms,
                    );
                } else {
                    // 远端事件已在 client_run_tracked 中按 start RPC
                    // 与 job elapsed 对齐到本次调用零点。
                    event.elapsed_ms = event.elapsed_ms.saturating_add(client_call_offset_ms);
                }
                on_event(event);
            },
        );
        let stop = self.server_stop_confirmed(t.dst.side, t.port, &sreq.request_id, Duration::ZERO);
        let (server_out, stop_ok) = match stop {
            Ok(out) => (out.output, true),
            Err(e) => (format!("(iperf3 server 停止未确认: {e})"), false),
        };
        let parsed = iperf::parse_output(&client.output);
        let raw_ok = client.ok && !client.timed_out && !client.cancelled && stop_ok;
        (raw_ok, parsed, client, server_out)
    }

    pub(super) fn run_iperf_single(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        tag: &str,
        t: &IperfTask,
        lifecycle: LifecycleLease<'_>,
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
        // monitor 和 iperf client 事件必须对齐到同一个 leg epoch，
        // 否则 server 启动、RPC 延迟和停止清理都会混入 TCP 平均速率。
        // 远端 monitor 零点由响应 elapsed_ms 有界估计，不再用 RPC 中点猜测。
        let leg_epoch = Instant::now();
        let monitor_start_before_ms = leg_epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mon_id = match self.mon_start(
            t.dst.side,
            &t.dst.nic.name,
            lifecycle.owner_id,
            lifecycle.lease_secs,
        ) {
            Ok((id, call_origin_ms)) => Some((id, monitor_start_before_ms + call_origin_ms)),
            Err(e) => {
                logln(&format!("    (接收端网卡监控启动失败: {e})"));
                None
            }
        };
        // 发送端也要采样：有明确目标时 W08 要求 RX/TX 双侧滚动窗口都完整，
        // 发送端采样塌了同样说明这一轮时间轴不可信。同一块网卡就不重复起。
        let tx_mon_id = if t.src.key() == t.dst.key() {
            None
        } else {
            let before_ms = leg_epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
            match self.mon_start(
                t.src.side,
                &t.src.nic.name,
                lifecycle.owner_id,
                lifecycle.lease_secs,
            ) {
                Ok((id, call_origin_ms)) => Some((id, before_ms + call_origin_ms)),
                Err(e) => {
                    logln(&format!("    (发送端网卡监控启动失败: {e})"));
                    None
                }
            }
        };
        let live = Arc::new(Mutex::new(LiveFlowState::default()));
        let mut events = Vec::new();
        let parallel_streams = if t.udp {
            1
        } else {
            tcp_parallel_streams(&t.extra)
        };
        let mon_id_for_progress = mon_id.as_ref().map(|(id, _)| id.clone());
        let live_for_progress = Arc::clone(&live);
        let progress_tag = tag.to_string();
        let progress_protocol = if t.udp { "UDP" } else { "TCP" };
        let (raw_ok, parsed, client, server_out) = std::thread::scope(|scope| {
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            let progress = scope.spawn(move || {
                let mut monitor_enabled = mon_id_for_progress.is_some();
                loop {
                    match done_rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let state = live_for_progress
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    let mut monitor_error = String::new();
                    let nic_rx_mbps = if monitor_enabled {
                        match mon_id_for_progress.as_deref() {
                            Some(id) => match self.mon_status(t.dst.side, id) {
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
                                    monitor_enabled = false;
                                    monitor_error = error;
                                    None
                                }
                            },
                            None => None,
                        }
                    } else {
                        None
                    };
                    let active = usize::from(
                        (!state.ended && state.active)
                            || nic_rx_mbps.is_some_and(|rate| rate > MIN_VALID_RX_MBPS),
                    );
                    logln(&format_iperf_progress(&IperfProgressSnapshot {
                        protocol: progress_protocol,
                        tag: &progress_tag,
                        active,
                        total: 1,
                        connected: usize::from(state.connected),
                        ended: usize::from(state.ended),
                        nic_rx_mbps,
                        iperf_mbps: active_iperf_rate(&state),
                        errors: usize::from(!state.error.is_empty()),
                        monitor_error,
                    }));
                }
            });
            let result = self.exec_iperf_core(
                t,
                lifecycle.owner_id,
                lifecycle.lease_secs,
                &leg_epoch,
                |event| {
                    {
                        let mut state =
                            live.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        if event.kind != IperfEventKind::Traffic
                            || is_live_progress_rate_line(&event.line, parallel_streams)
                        {
                            apply_flow_event(&mut state, &event);
                        }
                    }
                    events.push(event);
                },
            );
            let _ = done_tx.send(());
            let _ = progress.join();
            result
        });
        let rx_origin_offset_ms = mon_id.as_ref().map(|(_, offset)| *offset).unwrap_or(0);
        let tx_origin_offset_ms = tx_mon_id.as_ref().map(|(_, offset)| *offset).unwrap_or(0);
        let mon_out =
            mon_id.and_then(
                |(id, start_offset_ms)| match self.mon_stop(t.dst.side, &id) {
                    Ok(mut output) => {
                        align_monitor_samples(&mut output, start_offset_ms);
                        Some(output)
                    }
                    Err(error) => {
                        logln(&format!("    (接收端网卡监控停止失败: {error})"));
                        None
                    }
                },
            );
        let tx_mon_out =
            tx_mon_id.and_then(
                |(id, start_offset_ms)| match self.mon_stop(t.src.side, &id) {
                    Ok(mut output) => {
                        align_monitor_samples(&mut output, start_offset_ms);
                        Some(output)
                    }
                    Err(error) => {
                        logln(&format!("    (发送端网卡监控停止失败: {error})"));
                        None
                    }
                },
            );
        let effective_window =
            iperf_effective_window(&events, t.duration, parsed.has_measurement());
        let baseline_cutoff_ms = iperf_baseline_cutoff_ms(&events);
        let rx_stats = mon_out
            .as_ref()
            .map(|output| monitor_rate_stats(output, &effective_window, true, baseline_cutoff_ms))
            .unwrap_or_default();
        // 同一块网卡时 TX 与 RX 取自同一份样本，只是读另一个计数器方向。
        let tx_stats = tx_mon_out
            .as_ref()
            .or(if t.src.key() == t.dst.key() {
                mon_out.as_ref()
            } else {
                None
            })
            .map(|output| monitor_rate_stats(output, &effective_window, false, baseline_cutoff_ms))
            .unwrap_or_default();
        let rx_avg = rx_stats.avg_mbps;
        let nic_samples_rx = mon_out
            .as_ref()
            .map(|out| {
                self.save_monitor_samples(
                    lifecycle.owner_id,
                    t.dst.side,
                    &t.dst.nic.name,
                    &t.dst.key(),
                    rx_origin_offset_ms,
                    out,
                )
            })
            .unwrap_or_default();
        // TX 逐样本也必须落盘：它是否决性门槛（覆盖率不够整行判 NOT_EVALUATED），
        // 而在此之前没有任何人能拿到那份样本去核对。同网卡的情况不重复落盘——
        // 那时 TX/RX 本来就是同一份样本，只是读另一个计数器方向。
        let nic_samples_tx = tx_mon_out
            .as_ref()
            .map(|out| {
                self.save_monitor_samples(
                    lifecycle.owner_id,
                    t.src.side,
                    &t.src.nic.name,
                    &t.src.key(),
                    tx_origin_offset_ms,
                    out,
                )
            })
            .unwrap_or_default();

        let measurement = parsed.has_measurement();
        let judgement = iperf_flow_verdict(IperfFlowVerdictIn {
            raw_ok,
            measurement,
            effective_window: &effective_window,
            required_secs: t.duration,
            rate_mode: t.rate_mode,
            rx_target_mbps: t.rx_target_mbps,
            rx_stats: &rx_stats,
            tx_stats: &tx_stats,
            offered_floor: crate::master::rate_window::offered_floor_mbps(
                t.rx_target_mbps,
                self.cfg.iperf.rate_check.offered_headroom_pct,
            ),
            client_tail: client.output.lines().last().unwrap_or_default(),
            rx_monitor: mon_out.as_ref(),
        });
        let (verdict, reason_code) = (judgement.verdict, judgement.code);
        let reason_detail = judgement.detail.clone();
        let raw_error = if raw_ok {
            String::new()
        } else {
            client.output.lines().last().unwrap_or_default().to_string()
        };
        let raw_log = self.save_iperf_raw_record(IperfRawArtifact {
            owner_id: lifecycle.owner_id,
            lidx,
            stream_pos: 0,
            tag,
            task: t,
            client: &client,
            server_output: &server_out,
            events: &events,
            error: &raw_error,
        });

        logln(&format!(
            "    结果: {} 发送={} 接收={} 网卡实测={}",
            verdict.label(),
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
            time,
            verdict,
            execution_status: if client.timed_out {
                ExecutionStatus::TimedOut
            } else if client.cancelled {
                ExecutionStatus::Cancelled
            } else if !raw_ok {
                ExecutionStatus::Error
            } else if verdict == Verdict::NotEvaluated {
                ExecutionStatus::Partial
            } else {
                ExecutionStatus::Completed
            },
            reason_code,
            reason_detail: reason_detail.clone(),
            diagnostics: judgement.diagnostics.clone(),
            rx_avg,
            tx_mbps: parsed.best_sender(),
            rx_mbps: parsed.best_receiver(),
            udp_loss: if t.udp { parsed.udp_loss_pct } else { None },
            screenshot_master,
            screenshot_agent,
            command: client.cmd.clone(),
            raw_log,
            nic_samples_rx,
            nic_samples_tx,
            requested_streams: parallel_streams,
            active_streams: if parsed.has_measurement() {
                parallel_streams
            } else {
                0
            },
            required_streams: parallel_streams,
            target_mbps: t.rx_target_mbps,
            tx_avg: tx_stats.avg_mbps,
            tx_p10: tx_stats.p10_mbps,
            rx_p10: rx_stats.p10_mbps,
            rx_median: rx_stats.median_mbps,
            rx_p95: rx_stats.p95_mbps,
            rx_min: rx_stats.min_mbps,
            rx_max: rx_stats.max_mbps,
            effective_seconds: Some(effective_window.available_secs),
            required_seconds: Some(t.duration as f64),
            sample_coverage: Some(rx_stats.coverage),
            window_start_ms: Some(effective_window.start_ms),
            window_end_ms: Some(effective_window.end_ms),
            baseline_mbps: Some(rx_stats.baseline_mbps),
            rolling_coverage: Some(rx_stats.rolling_coverage),
            raws: vec![
                (
                    format!("iperf3 client{} 输出", fmt_tag(tag)),
                    format!("$ {}\n{}", client.cmd, client.output),
                ),
                (format!("iperf3 server{} 输出", fmt_tag(tag)), server_out),
                (
                    format!("流事件{}", fmt_tag(tag)),
                    format_flow_events(&events, &raw_error),
                ),
            ],
            ..base_row(RowIdentity {
                unit_seq: useq,
                leg_index: lidx,
                stream_index: t.stream_idx,
                group_flag: 0,
                unit,
                leg_tag: tag,
                src: &t.src,
                dst: &t.dst,
                ip: if t.v6 { "V6".into() } else { "V4".into() },
                protocol: if t.udp {
                    RowProtocol::Udp
                } else {
                    RowProtocol::Tcp
                },
                backend: RowBackend::Iperf3,
                param: t.profile_label.clone(),
                kind_label,
                task_id: md5_hex(&format!("{}|{}|{}", unit.id, tag, t.stream_idx)),
            })
        });
        LegOutcome {
            judgement,
            rx_avg,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    // ---------------- UDP 单元统一调度 ----------------
}
