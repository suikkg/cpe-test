//! 和辅测端说话的那一层：HTTP 往返、生命周期确认、资源清理。
//!
//! 上面的执行逻辑不该知道「这是一次 HTTP POST」——它只该知道「让对端起一个
//! iperf server / 停一个作业 / 交出网卡样本」，以及**这件事到底确认成功了没有**。
//! 后半句才是这一层存在的理由：确认失败和执行失败是两回事，混为一谈就会
//! 把环境问题写成被测设备不达标。

use super::*;

impl Ctx {
    pub(super) fn agent_post<TReq: Serialize, TOut: DeserializeOwned>(
        &self,
        path: &str,
        req: &TReq,
        timeout: Duration,
    ) -> Result<TOut, String> {
        let body = serde_json::to_string(req).map_err(|e| format!("序列化失败: {e}"))?;
        let (status, text) = http_client::post_json_auth_with_transport(
            self.transport.as_ref(),
            &self.agent_host,
            self.agent_port,
            path,
            &body,
            &self.cfg.agent_token,
            timeout,
        )
        .map_err(|e| format!("辅测机 {path} 调用失败: {e}"))?;
        if status == 401 {
            return Err(format!(
                "辅测机 {path} 拒绝访问(401)：agent 已启用令牌认证，请在本机 config.json 配置相同 agent_token"
            ));
        }
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

    pub(super) fn agent_post_reliable<TReq: Serialize, TOut: DeserializeOwned>(
        &self,
        path: &str,
        req: &TReq,
        timeout: Duration,
    ) -> Result<TOut, String> {
        self.agent_post_reliable_timed(path, req, timeout)
            .map(|(out, _)| out)
    }

    /// 与 `agent_post_reliable` 相同的重试语义，但额外返回成功那次调用
    /// 自身消耗的耗时（不含前几次失败的重试等待）。
    ///
    /// 时间轴对齐必须使用“成功调用自身的耗时”：若把三次可靠调用前的
    /// 起点到成功返回的总时长都算进去，首次连接超时、第二次成功时，
    /// 前一次失败的重试延时会被整体混入远端 job 起点估计，
    /// 真实流量窗口会整体偏移数秒。
    pub(super) fn agent_post_reliable_timed<TReq: Serialize, TOut: DeserializeOwned>(
        &self,
        path: &str,
        req: &TReq,
        timeout: Duration,
    ) -> Result<(TOut, Duration), String> {
        let mut errors = Vec::new();
        for attempt in 1..=RELIABLE_HTTP_ATTEMPTS {
            let attempt_started = self.clock.now();
            match self.agent_post(path, req, timeout) {
                Ok(out) => {
                    return Ok((
                        out,
                        self.clock.now().saturating_duration_since(attempt_started),
                    ))
                }
                Err(e) => {
                    errors.push(format!("第{attempt}次: {e}"));
                    if attempt < RELIABLE_HTTP_ATTEMPTS {
                        self.clock.sleep(RELIABLE_HTTP_RETRY_DELAY);
                    }
                }
            }
        }
        Err(errors.join("；"))
    }

    // ---------------- 双端统一操作 ----------------

    pub(super) fn ping_at(&self, side: Side, req: &PingReq) -> Result<PingOut, String> {
        match side {
            Side::Master => Ok(ping::run(req)),
            Side::Agent => {
                let mut out: PingOut =
                    self.agent_post("/ping", req, Duration::from_secs(req.count as u64 * 5 + 60))?;
                // 旧版 agent 可能仍把 ICMP Redirect/不可达计入 received。
                // 主控拿到完整 raw 后统一按当前规则重解析，既兼容旧协议字段，
                // 也保证本地与远端 Ping 使用同一套 Echo Reply 证据口径。
                if !out.raw.trim().is_empty() {
                    let parsed = ping::parse(&out.raw, req.count);
                    out.ok = parsed.ok;
                    out.sent = parsed.sent;
                    out.received = parsed.received;
                    out.lost = parsed.lost;
                    out.loss_pct = parsed.loss_pct;
                    out.rtt_min = parsed.rtt_min;
                    out.rtt_avg = parsed.rtt_avg;
                    out.rtt_max = parsed.rtt_max;
                }
                Ok(out)
            }
        }
    }

    pub(super) fn cleanup_owner_resources(
        &self,
        owner_id: &str,
        remote_resources: bool,
    ) -> Result<(), String> {
        let mut errors = Vec::new();

        if remote_resources {
            match self.agent_post_reliable::<_, ResourceCleanupOut>(
                "/resources/cleanup",
                &ResourceCleanupReq {
                    owner_id: owner_id.to_string(),
                    wait_secs: RESOURCE_CLEANUP_WAIT_SECS,
                },
                Duration::from_secs(30),
            ) {
                Ok(out) => errors.extend(out.errors),
                Err(e) => errors.push(format!("辅测机 owner 清理未确认: {e}")),
            }
        }

        let local_servers = self.local_servers.stop_owner(owner_id, Duration::ZERO);
        errors.extend(local_servers.errors);
        for (id, result) in self.local_monitors.stop_owner(owner_id) {
            if let Err(e) = result {
                errors.push(format!("主控 monitor {id} 清理失败: {e}"));
            }
        }
        let cts_jobs = self
            .local_cts_jobs
            .stop_owner(owner_id, Duration::from_secs(RESOURCE_CLEANUP_WAIT_SECS));
        errors.extend(cts_jobs.errors);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("；"))
        }
    }

    pub(super) fn server_start(
        &self,
        side: Side,
        req: &IperfServerStartReq,
    ) -> Result<String, String> {
        match side {
            Side::Master => {
                let bin = find_iperf3().ok_or("主控机未找到 iperf3，请把 iperf3 放到程序同目录")?;
                self.local_servers.start(&bin, req)
            }
            Side::Agent => match self.agent_post_reliable::<_, IperfServerStartOut>(
                "/iperf/server/start",
                req,
                Duration::from_secs(40),
            ) {
                Ok(out) => Ok(out.cmd),
                Err(start_error) => {
                    // start 的响应可能丢失，而进程其实已经启动。用同一个
                    // request_id 做补偿 stop；精确 stop 不会误杀后续新实例。
                    let cleanup = if req.request_id.is_empty() {
                        Ok(IperfServerStopOut::default())
                    } else {
                        self.server_stop_confirmed(side, req.port, &req.request_id, Duration::ZERO)
                    };
                    Err(match cleanup {
                        Ok(_) => {
                            format!("{start_error}（已补偿清理 request_id={}）", req.request_id)
                        }
                        Err(cleanup_error) => format!(
                            "{start_error}；补偿清理 request_id={} 也失败: {cleanup_error}",
                            req.request_id
                        ),
                    })
                }
            },
        }
    }

    pub(super) fn server_stop_confirmed(
        &self,
        side: Side,
        port: u16,
        request_id: &str,
        wait: Duration,
    ) -> Result<IperfServerStopOut, String> {
        match side {
            Side::Master => self
                .local_servers
                .stop_checked(port, request_id, wait)
                .and_then(|out| {
                    if out.terminated {
                        Ok(out)
                    } else {
                        Err(format!("主控 server 端口 {port} 停止未确认"))
                    }
                }),
            Side::Agent => self
                .agent_post_reliable(
                    "/iperf/server/stop",
                    &IperfServerStopReq {
                        port,
                        wait_secs: wait.as_secs(),
                        request_id: request_id.to_string(),
                    },
                    Duration::from_secs(30),
                )
                .and_then(|out: IperfServerStopOut| {
                    if out.terminated {
                        Ok(out)
                    } else {
                        Err(format!("辅测机 server 端口 {port} 停止未确认"))
                    }
                }),
        }
    }

    pub(super) fn client_stop_confirmed(&self, id: &str) -> Result<IperfClientStopOut, String> {
        self.agent_post_reliable(
            "/iperf/client/stop",
            &IperfClientStopReq {
                id: id.to_string(),
                wait_secs: RESOURCE_CLEANUP_WAIT_SECS,
            },
            Duration::from_secs(20),
        )
        .and_then(|out: IperfClientStopOut| {
            if out.terminated {
                Ok(out)
            } else {
                Err(format!("远端 client job {id} 停止未确认"))
            }
        })
    }

    pub(super) fn client_run_tracked<F>(
        &self,
        side: Side,
        req: &IperfClientReq,
        owner_id: &str,
        request_id: &str,
        lease_secs: u64,
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
                        process_started: Some(false),
                        cleanup_confirmed: Some(true),
                        cmd: String::new(),
                        output: "主控机未找到 iperf3，请把 iperf3 放到程序同目录".into(),
                        ..Default::default()
                    };
                };
                iperf::run_client_controlled(
                    &bin,
                    req,
                    Some(crate::cancel::cancel_flag()),
                    |line| {
                        if line.to_lowercase().contains("error") {
                            logln(&format!("      {line}"));
                        }
                    },
                    &mut on_event,
                )
            }
            Side::Agent => {
                let start_req = IperfClientStartReq {
                    request: req.clone(),
                    request_id: request_id.to_string(),
                    owner_id: owner_id.to_string(),
                    lease_secs,
                };
                let (started, start_attempt_elapsed): (IperfClientStartOut, Duration) = match self
                    .agent_post_reliable_timed(
                        "/iperf/client/start",
                        &start_req,
                        Duration::from_secs(20),
                    ) {
                    Ok((v, attempt_elapsed)) => (v, attempt_elapsed),
                    Err(e) => {
                        let cleanup = self.client_stop_confirmed(request_id);
                        let cleanup_confirmed = cleanup.is_ok();
                        return IperfClientOut {
                            cancelled: !cleanup_confirmed,
                            process_started: Some(false),
                            cleanup_confirmed: Some(cleanup_confirmed),
                            output: format!(
                                "(远端异步作业启动失败: {e}; 补偿清理: {})",
                                cleanup
                                    .map(|_| "已确认".to_string())
                                    .unwrap_or_else(|cleanup_error| cleanup_error)
                            ),
                            ..Default::default()
                        };
                    }
                };
                if !request_id.is_empty() && started.id != request_id {
                    let actual_cleanup = self.client_stop_confirmed(&started.id);
                    let expected_cleanup = self.client_stop_confirmed(request_id);
                    let cleanup_confirmed = actual_cleanup.is_ok() && expected_cleanup.is_ok();
                    return IperfClientOut {
                        cancelled: true,
                        process_started: Some(false),
                        cleanup_confirmed: Some(cleanup_confirmed),
                        output: format!(
                            "远端 client 返回了非预期 job id：期望 {request_id}，实际 {}；实际 ID 清理={}；期望 ID 清理={}",
                            started.id,
                            actual_cleanup
                                .map(|_| "已确认".to_string())
                                .unwrap_or_else(|error| error),
                            expected_cleanup
                                .map(|_| "已确认".to_string())
                                .unwrap_or_else(|error| error)
                        ),
                        ..Default::default()
                    };
                }
                // 只统计成功那次 start 调用自身的耗时：重试失败 + 等待
                // 属于远端 job 开始之前的编排开销，不能混入 job 零点估计。
                let response_elapsed_ms = start_attempt_elapsed.as_millis() as u64;
                let remote_origin_ms =
                    remote_job_origin_ms(response_elapsed_ms, started.elapsed_ms);
                let max_remote_secs = req.duration.saturating_add(180);
                let Some(deadline) =
                    std::time::Instant::now().checked_add(Duration::from_secs(max_remote_secs))
                else {
                    let cleanup = self.client_stop_confirmed(&started.id);
                    let cleanup_confirmed = cleanup.is_ok();
                    return IperfClientOut {
                        cancelled: !cleanup_confirmed,
                        cleanup_confirmed: Some(cleanup_confirmed),
                        output: format!(
                            "远端 client duration={} 秒过大，无法建立等待截止时间；停止确认: {}",
                            req.duration,
                            cleanup.map(|_| "成功".to_string()).unwrap_or_else(|e| e)
                        ),
                        ..Default::default()
                    };
                };
                let mut cursor = 0usize;
                loop {
                    if crate::cancel::is_cancelled() {
                        // 用户第一次 Ctrl+C：立即回收远端异步作业并返回，
                        // 主循环随后生成部分报告，不必等整段 duration 跑完。
                        let cleanup = self.client_stop_confirmed(&started.id);
                        let cleanup_confirmed = cleanup.is_ok();
                        let mut result = cleanup
                            .as_ref()
                            .ok()
                            .and_then(|output| output.result.clone())
                            .unwrap_or_default();
                        result.ok = false;
                        result.cancelled = !cleanup_confirmed;
                        result.cleanup_confirmed =
                            Some(cleanup_confirmed && result.cleanup_confirmed == Some(true));
                        if !result.output.is_empty() && !result.output.ends_with('\n') {
                            result.output.push('\n');
                        }
                        result.output.push_str(&format!(
                            "(用户中断，远端异步作业 {} 已停止确认: {})",
                            started.id,
                            cleanup.map(|_| "成功".to_string()).unwrap_or_else(|e| e)
                        ));
                        return result;
                    }
                    if std::time::Instant::now() >= deadline {
                        let cleanup = self.client_stop_confirmed(&started.id);
                        let cleanup_confirmed = cleanup.is_ok();
                        let mut result = cleanup
                            .as_ref()
                            .ok()
                            .and_then(|output| output.result.clone())
                            .unwrap_or_default();
                        let detail = format!(
                            "(远端异步作业 {} 超过 {} 秒仍未结束；停止确认: {})",
                            started.id,
                            max_remote_secs,
                            cleanup
                                .as_ref()
                                .map(|_| "成功".to_string())
                                .unwrap_or_else(|error| error.clone())
                        );
                        result.ok = false;
                        result.timed_out = true;
                        result.cancelled = !cleanup_confirmed;
                        result.cleanup_confirmed =
                            Some(cleanup_confirmed && result.cleanup_confirmed == Some(true));
                        if !result.output.is_empty() && !result.output.ends_with('\n') {
                            result.output.push('\n');
                        }
                        result.output.push_str(&detail);
                        return result;
                    }
                    let status: IperfClientStatusOut = match self.agent_post_reliable(
                        "/iperf/client/status",
                        &IperfClientStatusReq {
                            id: started.id.clone(),
                            cursor,
                        },
                        Duration::from_secs(20),
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            let cleanup = self.client_stop_confirmed(&started.id);
                            let cleanup_confirmed = cleanup.is_ok();
                            let mut result = cleanup
                                .as_ref()
                                .ok()
                                .and_then(|output| output.result.clone())
                                .unwrap_or_default();
                            result.ok = false;
                            result.cancelled = !cleanup_confirmed;
                            result.cleanup_confirmed =
                                Some(cleanup_confirmed && result.cleanup_confirmed == Some(true));
                            result.output = format!(
                                "(远端异步作业查询失败: {e}; 停止确认: {})",
                                cleanup
                                    .as_ref()
                                    .map(|_| "成功".to_string())
                                    .unwrap_or_else(|cleanup_error| cleanup_error.clone())
                            );
                            return result;
                        }
                    };
                    cursor = status.next_cursor;
                    for mut event in status.events {
                        event.elapsed_ms = event.elapsed_ms.saturating_add(remote_origin_ms);
                        if event.kind == IperfEventKind::Error {
                            logln(&format!("      [远端 {}] {}", started.id, event.line));
                        }
                        on_event(event);
                    }
                    if status.done {
                        let result_missing = status.result.is_none();
                        let stop = self.client_stop_confirmed(&started.id);
                        let mut result = status
                            .result
                            .or_else(|| stop.as_ref().ok().and_then(|output| output.result.clone()))
                            .unwrap_or_default();
                        if result_missing {
                            result.ok = false;
                            result.output =
                                format!("(远端异步作业 {} 已结束但缺少结果)", started.id);
                        }
                        if let Err(e) = stop {
                            result.ok = false;
                            result.cancelled = true;
                            result.cleanup_confirmed = Some(false);
                            if !result.output.ends_with('\n') && !result.output.is_empty() {
                                result.output.push('\n');
                            }
                            result
                                .output
                                .push_str(&format!("远端 client 结束后清理未确认: {e}"));
                        } else {
                            result.cleanup_confirmed = Some(result.cleanup_confirmed == Some(true));
                        }
                        return result;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }

    pub(super) fn cts_job_start(
        &self,
        side: Side,
        start: CtsTrafficStartReq,
    ) -> Result<CtsTrafficStartOut, String> {
        self.cts_job_start_timed(side, start).map(|(out, _)| out)
    }

    /// 与 `cts_job_start` 相同的语义，额外返回成功那次 start 调用自身耗时
    /// （不含重试等待），用于把远端 job 零点对齐到真实启动时刻。
    pub(super) fn cts_job_start_timed(
        &self,
        side: Side,
        start: CtsTrafficStartReq,
    ) -> Result<(CtsTrafficStartOut, Duration), String> {
        match side {
            Side::Master => {
                let bin = find_ctstraffic().ok_or_else(|| {
                    if cfg!(windows) {
                        "主控机未找到 ctsTraffic.exe，请放到程序同目录或 PATH".to_string()
                    } else {
                        "ctsTraffic 仅支持 Windows 10+，当前主控平台不支持".to_string()
                    }
                })?;
                let id = ctstraffic::start_managed_job(&self.local_cts_jobs, bin, start)?;
                let elapsed_ms = self.local_cts_jobs.elapsed_ms(&id).unwrap_or(0);
                Ok((CtsTrafficStartOut { id, elapsed_ms }, Duration::ZERO))
            }
            Side::Agent => {
                self.agent_post_reliable_timed("/ctstraffic/start", &start, Duration::from_secs(20))
            }
        }
    }

    pub(super) fn cts_job_status(
        &self,
        side: Side,
        id: &str,
        cursor: usize,
    ) -> Result<CtsTrafficStatusOut, String> {
        match side {
            Side::Master => self.local_cts_jobs.status(id, cursor),
            Side::Agent => self.agent_post_reliable(
                "/ctstraffic/status",
                &CtsTrafficStatusReq {
                    id: id.to_string(),
                    cursor,
                },
                Duration::from_secs(10),
            ),
        }
    }

    pub(super) fn cts_job_stop_confirmed(
        &self,
        side: Side,
        id: &str,
    ) -> Result<CtsTrafficStopOut, String> {
        let out = match side {
            Side::Master => self
                .local_cts_jobs
                .stop_checked(id, Duration::from_secs(RESOURCE_CLEANUP_WAIT_SECS)),
            Side::Agent => self.agent_post_reliable(
                "/ctstraffic/stop",
                &CtsTrafficStopReq {
                    id: id.to_string(),
                    wait_secs: RESOURCE_CLEANUP_WAIT_SECS,
                },
                Duration::from_secs(20),
            ),
        }?;
        if out.terminated {
            Ok(out)
        } else {
            Err(format!("ctsTraffic 作业 {id} 停止未确认"))
        }
    }

    pub(super) fn cts_client_run_tracked<F>(
        &self,
        side: Side,
        start: CtsTrafficStartReq,
        mut on_event: F,
    ) -> CtsClientRun
    where
        F: FnMut(IperfFlowEvent),
    {
        let expected_id = start.request_id.clone();
        let duration = start.request.duration_secs;
        let (started, start_attempt_elapsed) = match self.cts_job_start_timed(side, start) {
            Ok((value, attempt_elapsed)) => (value, attempt_elapsed),
            Err(error) => {
                let cleanup = if expected_id.is_empty() {
                    Ok(CtsTrafficStopOut::default())
                } else {
                    self.cts_job_stop_confirmed(side, &expected_id)
                };
                let cleanup_confirmed = cleanup.is_ok();
                let detail = format!(
                    "ctsTraffic client 启动失败: {error}；补偿清理: {}",
                    cleanup
                        .map(|_| "已确认".to_string())
                        .unwrap_or_else(|cleanup_error| cleanup_error)
                );
                return CtsClientRun {
                    client: IperfClientOut {
                        cancelled: !cleanup_confirmed,
                        output: detail.clone(),
                        ..Default::default()
                    },
                    started: false,
                    cleanup_confirmed,
                    setup_error: Some((ReasonCode::CtsClientStartFailed, detail)),
                };
            }
        };
        if !expected_id.is_empty() && started.id != expected_id {
            let actual_cleanup = self.cts_job_stop_confirmed(side, &started.id);
            let expected_cleanup = self.cts_job_stop_confirmed(side, &expected_id);
            let cleanup_confirmed = actual_cleanup.is_ok() && expected_cleanup.is_ok();
            let detail = format!(
                "ctsTraffic 返回非预期 job id：期望 {expected_id}，实际 {}；实际 ID 清理={}；期望 ID 清理={}",
                started.id,
                actual_cleanup
                    .map(|_| "已确认".to_string())
                    .unwrap_or_else(|error| error),
                expected_cleanup
                    .map(|_| "已确认".to_string())
                    .unwrap_or_else(|error| error)
            );
            return CtsClientRun {
                client: IperfClientOut {
                    cancelled: true,
                    output: detail.clone(),
                    ..Default::default()
                },
                started: false,
                cleanup_confirmed,
                setup_error: Some((ReasonCode::CtsClientJobIdMismatch, detail)),
            };
        }
        // 只统计成功那次 start 调用自身的耗时，避免重试等待混入 CTS job 零点。
        let response_elapsed_ms = start_attempt_elapsed.as_millis() as u64;
        let origin_ms = remote_job_origin_ms(response_elapsed_ms, started.elapsed_ms);
        let max_wait = duration.saturating_add(60);
        let Some(deadline) = Instant::now().checked_add(Duration::from_secs(max_wait)) else {
            let cleanup = self.cts_job_stop_confirmed(side, &started.id);
            let cleanup_confirmed = cleanup.is_ok();
            let detail = format!(
                "ctsTraffic duration 过大，无法建立等待截止时间；停止确认: {}",
                cleanup
                    .map(|_| "成功".to_string())
                    .unwrap_or_else(|error| error)
            );
            return CtsClientRun {
                client: IperfClientOut {
                    cancelled: !cleanup_confirmed,
                    output: detail.clone(),
                    ..Default::default()
                },
                started: true,
                cleanup_confirmed,
                setup_error: Some((ReasonCode::CtsClientWaitInvalid, detail)),
            };
        };
        let mut cursor = 0usize;
        loop {
            if crate::cancel::is_cancelled() {
                // 用户第一次 Ctrl+C：立即回收 CTS 异步作业并返回，
                // 主循环随后生成部分报告，不必等整段 duration 跑完。
                let cleanup = self.cts_job_stop_confirmed(side, &started.id);
                let mut client = cleanup
                    .as_ref()
                    .ok()
                    .and_then(|output| output.result.clone())
                    .unwrap_or_default();
                let process_cleanup_confirmed = client.cleanup_confirmed == Some(true);
                let cleanup_confirmed = cleanup.is_ok() && process_cleanup_confirmed;
                client.ok = false;
                client.cancelled = !cleanup_confirmed;
                if !client.output.is_empty() && !client.output.ends_with('\n') {
                    client.output.push('\n');
                }
                client.output.push_str(&format!(
                    "(用户中断，ctsTraffic 作业 {} 已停止确认: {})",
                    started.id,
                    cleanup.map(|_| "成功".to_string()).unwrap_or_else(|e| e)
                ));
                let cancel_detail = client.output.clone();
                return CtsClientRun {
                    client,
                    started: true,
                    cleanup_confirmed,
                    setup_error: Some((ReasonCode::CtsClientUserCancelled, cancel_detail)),
                };
            }
            if Instant::now() >= deadline {
                let cleanup = self.cts_job_stop_confirmed(side, &started.id);
                let mut client = cleanup
                    .as_ref()
                    .ok()
                    .and_then(|output| output.result.clone())
                    .unwrap_or_default();
                let process_started_confirmed = client.process_started == Some(true);
                let process_cleanup_confirmed = client.cleanup_confirmed == Some(true);
                let cleanup_confirmed = cleanup.is_ok() && process_cleanup_confirmed;
                let detail = format!(
                    "ctsTraffic client 超过 {} 秒仍未结束；停止确认: {}",
                    max_wait,
                    cleanup
                        .as_ref()
                        .map(|_| "成功".to_string())
                        .unwrap_or_else(|error| error.clone())
                );
                client.ok = false;
                client.timed_out = true;
                // 这里的 cancel 是 controller 为回收超时进程主动发出的。只要
                // 底层进程 wait/reap 与 job stop 都已确认，就保留 timed_out
                // 而不标成“显式取消”，从而允许单流安全进入下一轮。
                client.cancelled = !cleanup_confirmed;
                if !client.output.is_empty() && !client.output.ends_with('\n') {
                    client.output.push('\n');
                }
                client.output.push_str(&detail);
                let setup_error = if cleanup.is_err() {
                    Some((ReasonCode::CtsClientStopFailed, detail))
                } else if !process_started_confirmed {
                    Some((
                        ReasonCode::CtsClientProcessNotStarted,
                        "ctsTraffic client 超时回收时未确认底层进程曾成功启动".into(),
                    ))
                } else if !process_cleanup_confirmed {
                    Some((
                        ReasonCode::CtsClientProcessCleanupUnconfirmed,
                        "ctsTraffic client 超时后未确认底层进程已 wait/reap".into(),
                    ))
                } else {
                    None
                };
                return CtsClientRun {
                    client,
                    started: true,
                    cleanup_confirmed,
                    setup_error,
                };
            }
            let status = match self.cts_job_status(side, &started.id, cursor) {
                Ok(value) => value,
                Err(error) => {
                    let cleanup = self.cts_job_stop_confirmed(side, &started.id);
                    let cleanup_confirmed = cleanup.is_ok();
                    let detail = format!(
                        "ctsTraffic client 状态查询失败: {error}；停止确认: {}",
                        cleanup
                            .map(|_| "成功".to_string())
                            .unwrap_or_else(|cleanup_error| cleanup_error)
                    );
                    return CtsClientRun {
                        client: IperfClientOut {
                            cancelled: !cleanup_confirmed,
                            output: detail.clone(),
                            ..Default::default()
                        },
                        started: true,
                        cleanup_confirmed,
                        setup_error: Some((ReasonCode::CtsClientStatusFailed, detail)),
                    };
                }
            };
            cursor = status.next_cursor;
            for mut event in status.events {
                event.elapsed_ms = event.elapsed_ms.saturating_add(origin_ms);
                on_event(event);
            }
            if status.done {
                let result_missing = status.result.is_none();
                let mut result = status.result.unwrap_or_else(|| IperfClientOut {
                    output: "ctsTraffic client 已结束但缺少结果".into(),
                    ..Default::default()
                });
                let cleanup = self.cts_job_stop_confirmed(side, &started.id);
                let cleanup_confirmed = cleanup.is_ok();
                if let Err(error) = cleanup {
                    result.ok = false;
                    result.cancelled = true;
                    if !result.output.is_empty() && !result.output.ends_with('\n') {
                        result.output.push('\n');
                    }
                    result
                        .output
                        .push_str(&format!("ctsTraffic client 清理未确认: {error}"));
                }
                let setup_error = if !cleanup_confirmed {
                    Some((ReasonCode::CtsClientStopFailed, result.output.clone()))
                } else if result_missing {
                    Some((ReasonCode::CtsClientResultMissing, result.output.clone()))
                } else {
                    cts_process_setup_error(&result)
                };
                return CtsClientRun {
                    client: result,
                    started: true,
                    cleanup_confirmed,
                    setup_error,
                };
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// 启动接收端网卡监控，返回 (id, 相对本次调用起点的零点偏移毫秒)。
    ///
    /// 远端 monitor 的零点用响应中的 `elapsed_ms` 和成功那次调用自身的
    /// 耗时做有界估计（`remote_job_origin_ms`），不再用 RPC 往返中点猜测：
    /// 非对称网络延迟会把空闲时间混入正式窗口。
    pub(super) fn mon_start(
        &self,
        side: Side,
        iface: &str,
        owner_id: &str,
        lease_secs: u64,
    ) -> Result<(String, u64), String> {
        let interval_ms = self
            .cfg
            .iperf
            .rate_check
            .sample_interval_ms
            .clamp(200, 5_000);
        match side {
            Side::Master => {
                let before = Instant::now();
                let id =
                    self.local_monitors
                        .start_owned(iface, interval_ms, owner_id, lease_secs)?;
                let call_elapsed_ms = before.elapsed().as_millis() as u64;
                // 本地启动无网络往返，零点就是调用起点（本地开销可忽略）。
                Ok((id, midpoint_ms(0, call_elapsed_ms)))
            }
            Side::Agent => {
                let (out, attempt_elapsed) = self.agent_post_reliable_timed::<_, MonitorStartOut>(
                    "/monitor/start",
                    &MonitorStartReq {
                        iface: iface.to_string(),
                        interval_ms,
                        owner_id: owner_id.to_string(),
                        lease_secs,
                    },
                    Duration::from_secs(20),
                )?;
                let origin =
                    remote_job_origin_ms(attempt_elapsed.as_millis() as u64, out.elapsed_ms);
                Ok((out.id, origin))
            }
        }
    }

    pub(super) fn mon_stop(&self, side: Side, id: &str) -> Result<MonitorStopOut, String> {
        match side {
            Side::Master => self.local_monitors.stop(id),
            Side::Agent => self.agent_post(
                "/monitor/stop",
                &MonitorStopReq { id: id.to_string() },
                Duration::from_secs(20),
            ),
        }
    }

    pub(super) fn mon_status(&self, side: Side, id: &str) -> Result<MonitorStatusOut, String> {
        match side {
            Side::Master => self.local_monitors.status(id),
            Side::Agent => self.agent_post(
                "/monitor/status",
                &MonitorStatusReq { id: id.to_string() },
                Duration::from_secs(3),
            ),
        }
    }
}
