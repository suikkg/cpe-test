//! ctsTraffic 专属的执行事实与判定装配。
//!
//! ctsTraffic 与 iperf3 的失败形态完全不同（进程退出码、状态行节奏、
//! server 预停失败都自成一套），两边的判定不能混在一段 match 里各打各的补丁。

use super::*;

pub(super) struct CtsAttemptRun {
    pub(super) attempt: usize,
    pub(super) client: IperfClientOut,
    pub(super) server_output: String,
    pub(super) server_unexpected_failure: bool,
    pub(super) traffic_window: EffectiveWindow,
    pub(super) events: Vec<IperfFlowEvent>,
    pub(super) parsed: ctstraffic::CtsTrafficParsed,
    pub(super) traffic_established: bool,
    pub(super) full_attempt: bool,
    pub(super) cleanup_confirmed: bool,
    pub(super) setup_error: Option<(ReasonCode, String)>,
}

pub(super) struct CtsClientRun {
    pub(super) client: IperfClientOut,
    pub(super) started: bool,
    pub(super) cleanup_confirmed: bool,
    pub(super) setup_error: Option<(ReasonCode, String)>,
}

#[derive(Debug, Clone)]
pub(super) struct CtsMonitorIssue {
    pub(super) code: ReasonCode,
    pub(super) detail: String,
    pub(super) setup_error: bool,
    pub(super) affects_verdict: bool,
}

pub(super) fn cts_process_setup_error(client: &IperfClientOut) -> Option<(ReasonCode, String)> {
    if client.cancelled {
        return Some((
            ReasonCode::CtsClientCancelled,
            client
                .output
                .lines()
                .last()
                .unwrap_or("ctsTraffic client 被取消")
                .to_string(),
        ));
    }
    if client.timed_out {
        // 超时但 stop/join 已确认时，属于一次可安全重试的完整尝试，
        // 不能在这里预先降级成 setup error。
        return None;
    }

    let lower = client.output.to_ascii_lowercase();
    let code = if lower.contains("启动命令失败")
        || lower.contains("failed to spawn")
        || lower.contains("the system cannot find the file")
        || lower.contains("找不到指定的文件")
        || lower.contains("not recognized as an internal or external command")
        || lower.contains("不是内部或外部命令")
    {
        ReasonCode::CtsProcessStartFailed
    } else if lower.contains("invalid argument")
        || lower.contains("invalid option")
        || lower.contains("无效参数")
    {
        ReasonCode::CtsArgsInvalid
    } else if lower.contains("命令超时时间过大")
        || lower.contains("创建流式命令")
        || lower.contains("等待子进程失败")
        || lower.contains("回收子进程失败")
    {
        ReasonCode::CtsProcessControlFailed
    } else {
        return None;
    };
    Some((
        code,
        client
            .output
            .lines()
            .last()
            .unwrap_or("ctsTraffic 进程环境错误")
            .to_string(),
    ))
}

pub(super) fn format_ctstraffic_attempts(
    server_cmd: &str,
    attempts: &[CtsAttemptRun],
    final_error: &str,
) -> String {
    let mut out = String::new();
    for attempt in attempts {
        let attempt_error = attempt
            .setup_error
            .as_ref()
            .map(|(_, detail)| detail.as_str())
            .or_else(|| {
                attempt
                    .server_unexpected_failure
                    .then_some("ctsTraffic server 在停止请求前异常退出")
            })
            .or_else(|| {
                (!attempt.traffic_established).then_some("本轮未产生 ctsTraffic 自身吞吐测量")
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "=== attempt {} ===\n\
full_attempt={} cleanup_confirmed={} client_process_started={:?} client_process_cleanup={:?} tool_measurement={}\n\
\n=== SERVER COMMAND ===\n$ {}\n\
\n=== SERVER STDOUT+STDERR ===\n{}\n\
\n=== CLIENT COMMAND ===\n$ {}\n\
\n=== CLIENT STDOUT+STDERR ===\n{}\n\
\n=== FLOW EVENTS ===\n{}\n",
            attempt.attempt + 1,
            attempt.full_attempt,
            attempt.cleanup_confirmed,
            attempt.client.process_started,
            attempt.client.cleanup_confirmed,
            attempt.traffic_established,
            server_cmd,
            attempt.server_output,
            attempt.client.cmd,
            attempt.client.output,
            format_flow_events(&attempt.events, attempt_error),
        ));
    }
    if !final_error.is_empty() {
        out.push_str(&format!("\n=== FINAL ERROR ===\n{final_error}\n"));
    }
    out
}

/// 在网卡 RX 判定之上叠加 ctsTraffic 的 UDP 丢帧门槛。
///
/// 顺序对齐 iperf3 路径：只有当网卡侧已经完成一次真正的目标比对
/// （Pass/RateFail/Unstable）时才评估丢帧；采样不足、目标缺失或未知
/// （NotEvaluated/Measured）时原样返回，不把环境问题写成 CPE 丢帧超限。
/// 已配置门槛却缺少丢帧数据时，缺的是判定依据本身，因此优先于速率结论。
pub(super) fn cts_apply_udp_loss(
    nic: (Verdict, ReasonCode, String),
    is_udp: bool,
    loss_limit: Option<f64>,
    loss: Option<f64>,
) -> (Verdict, ReasonCode, String) {
    let (verdict, code, detail) = nic;
    if !is_udp || matches!(verdict, Verdict::NotEvaluated | Verdict::Measured) {
        return (verdict, code, detail);
    }
    let Some(limit) = loss_limit else {
        return (verdict, code, detail);
    };
    let Some(actual) = loss else {
        return (
            Verdict::NotEvaluated,
            ReasonCode::CtsUdpLossDataMissing,
            "已配置 UDP 丢帧门槛，但 ctsTraffic 输出缺少 dropped frames 数据".into(),
        );
    };
    if verdict == Verdict::Pass && actual > limit {
        return (
            Verdict::RateFail,
            ReasonCode::CtsUdpLossHigh,
            format!("CTS UDP 丢帧率 {actual:.3}% 超过限制 {limit:.3}%"),
        );
    }
    (verdict, code, detail)
}

pub(super) fn cts_attempt_budget(configured_retries: usize, strict_single_udp: bool) -> usize {
    if strict_single_udp {
        effective_udp_retries(configured_retries, true).saturating_add(1)
    } else {
        1
    }
}

pub(super) fn cts_monitor_runtime_issue(
    out: &MonitorStopOut,
    window: &EffectiveWindow,
) -> Option<CtsMonitorIssue> {
    let mut details = Vec::<String>::new();
    for error in &out.errors {
        if !error.trim().is_empty() && !details.iter().any(|detail| detail == error) {
            details.push(error.clone());
        }
    }
    let mut window_details = Vec::<String>::new();
    for sample in &out.samples {
        if sample.valid {
            continue;
        }
        let detail = if sample.error.trim().is_empty() {
            format!("elapsed={}ms 的监控样本无效", sample.elapsed_ms)
        } else {
            sample.error.clone()
        };
        if !details.iter().any(|existing| existing == &detail) {
            details.push(detail.clone());
        }
        let sample_start_ms = sample.elapsed_ms.saturating_sub(sample.interval_ms);
        let overlaps_window = window.end_ms > window.start_ms
            && sample.interval_ms > 0
            && sample.elapsed_ms > window.start_ms
            && sample_start_ms < window.end_ms;
        if overlaps_window && !window_details.iter().any(|existing| existing == &detail) {
            window_details.push(detail);
        }
    }
    if out.samples.is_empty() {
        let mut detail =
            "CTS 接收端网卡监控未返回可裁剪的采样序列；全生命周期平均值不能用于 CTS 有效流量窗口"
                .to_string();
        if !details.is_empty() {
            detail.push_str(&format!("；监控错误: {}", details.join("；")));
        }
        return Some(CtsMonitorIssue {
            code: ReasonCode::CtsMonitorNoSamples,
            detail,
            setup_error: false,
            affects_verdict: true,
        });
    }
    (!details.is_empty()).then(|| {
        let affects_verdict = !window_details.is_empty();
        let diagnostic_only_details: Vec<&str> = details
            .iter()
            .filter(|detail| !window_details.iter().any(|window| window == *detail))
            .map(String::as_str)
            .collect();
        CtsMonitorIssue {
            code: ReasonCode::CtsMonitorRuntimeError,
            detail: if affects_verdict {
                let mut detail = format!(
                    "CTS 接收端网卡监控在有效流量窗口内运行异常: {}",
                    window_details.join("；")
                );
                if !diagnostic_only_details.is_empty() {
                    detail.push_str(&format!(
                        "；窗口外或无法定位时间的监控异常（仅诊断）: {}",
                        diagnostic_only_details.join("；")
                    ));
                }
                detail
            } else {
                format!(
                    "CTS 接收端网卡监控在有效流量窗口外记录到异常，不影响本轮主判定: {}",
                    details.join("；")
                )
            },
            setup_error: false,
            affects_verdict,
        }
    })
}

pub(super) fn cts_monitor_issue_verdict(
    issue: &CtsMonitorIssue,
) -> Option<(Verdict, ReasonCode, String)> {
    issue.affects_verdict.then(|| {
        (
            if issue.setup_error {
                Verdict::SetupError
            } else {
                Verdict::NotEvaluated
            },
            issue.code,
            issue.detail.clone(),
        )
    })
}

pub(super) fn cts_stop_process_evidence(stop: &Result<CtsTrafficStopOut, String>) -> (bool, bool) {
    let result = stop.as_ref().ok().and_then(|output| output.result.as_ref());
    (
        result.and_then(|value| value.process_started) == Some(true),
        result.and_then(|value| value.cleanup_confirmed) == Some(true),
    )
}

/// 区分本轮 controller 发出的正常 stop 与 server 自身失败。
/// 返回 `(pre_stop_cancelled, server_runtime_failure)`：只有 stop 快照前已经完成且
/// 明确带 cancelled 才视为外部显式取消；任何未带 cancelled 的异常退出/timeout
/// 都是 server runtime failure，包括快照与 cancel 生效之间的窄竞争窗口。
pub(super) fn cts_server_pre_stop_failures(
    stop: &Result<CtsTrafficStopOut, String>,
) -> (bool, bool) {
    let Some(output) = stop.as_ref().ok() else {
        return (false, false);
    };
    let Some(result) = output.result.as_ref() else {
        return (false, false);
    };
    (
        output.was_done && result.cancelled,
        !result.cancelled && (!result.ok || result.timed_out),
    )
}

pub(super) fn cts_attempt_is_safe_full(attempt: &CtsAttemptRun) -> bool {
    attempt.full_attempt
        && attempt.client.process_started == Some(true)
        && attempt.client.cleanup_confirmed == Some(true)
        && attempt.cleanup_confirmed
        && attempt.setup_error.is_none()
        && !attempt.client.cancelled
        && !attempt.server_unexpected_failure
}

pub(super) fn cts_should_retry_after_last(
    attempts: &[CtsAttemptRun],
    max_attempts: usize,
    strict_single_udp: bool,
) -> bool {
    let Some(last) = attempts.last() else {
        return false;
    };
    strict_single_udp
        && attempts.len() < max_attempts
        && !last.traffic_established
        && cts_attempt_is_safe_full(last)
}

pub(super) fn select_cts_attempt_index(attempts: &[CtsAttemptRun]) -> Option<usize> {
    attempts
        .iter()
        .position(|attempt| attempt.traffic_established)
        .or_else(|| attempts.len().checked_sub(1))
}

pub(super) fn cts_full_attempts(attempts: &[CtsAttemptRun]) -> usize {
    attempts
        .iter()
        .filter(|attempt| cts_attempt_is_safe_full(attempt))
        .count()
}

pub(super) fn cts_retry_count(attempts: &[CtsAttemptRun]) -> usize {
    cts_full_attempts(attempts).saturating_sub(1)
}

pub(super) fn cts_single_udp_exhausted(
    attempts: &[CtsAttemptRun],
    max_attempts: usize,
    strict_single_udp: bool,
) -> bool {
    strict_single_udp
        && max_attempts > 0
        && attempts.len() == max_attempts
        && attempts
            .iter()
            .all(|attempt| cts_attempt_is_safe_full(attempt) && !attempt.traffic_established)
}

pub(super) fn cts_server_unexpected_setup_error(
    server_unexpected_failure: bool,
    traffic_established: bool,
    server_output: &str,
) -> Option<(ReasonCode, String)> {
    (server_unexpected_failure && !traffic_established).then(|| {
        (
            ReasonCode::CtsServerFailed,
            server_output
                .lines()
                .last()
                .filter(|line| !line.trim().is_empty())
                .unwrap_or("ctsTraffic server 在停止请求前异常退出")
                .to_string(),
        )
    })
}

pub(super) fn cts_runtime_failure_verdict(
    attempt: &CtsAttemptRun,
    runtime_errors: u64,
    client_expected_completion: bool,
) -> Option<(Verdict, ReasonCode, String)> {
    if !attempt.traffic_established {
        return None;
    }
    let detail = if attempt.server_unexpected_failure {
        attempt
            .server_output
            .lines()
            .last()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                format!("ctsTraffic 已产生工具测量，但 server 在显式停止前异常退出或超时: {line}")
            })
            .unwrap_or_else(|| {
                "ctsTraffic 已产生工具测量，但 server 在显式停止前异常退出或超时".into()
            })
    } else if runtime_errors > 0 {
        format!("ctsTraffic 记录到 {runtime_errors} 个网络/协议/数据错误")
    } else if attempt.client.timed_out {
        attempt
            .client
            .output
            .lines()
            .last()
            .filter(|line| !line.trim().is_empty())
            .map(|line| format!("ctsTraffic 已产生工具测量，但 client 超时: {line}"))
            .unwrap_or_else(|| "ctsTraffic 已产生工具测量，但 client 超时".into())
    } else if !client_expected_completion {
        attempt
            .client
            .output
            .lines()
            .last()
            .filter(|line| !line.trim().is_empty())
            .map(|line| format!("ctsTraffic 已产生工具测量，但 client 未正常完成: {line}"))
            .unwrap_or_else(|| "ctsTraffic 已产生工具测量，但 client 未正常完成".into())
    } else {
        return None;
    };
    Some((Verdict::RateFail, ReasonCode::CtsRuntimeErrors, detail))
}

impl Ctx {
    /// 平台/能力/二进制预检会阻止实际启动流量进程，但 builder 已识别出的
    /// CTS 参数错误必须保留更精确的 `CTSTRAFFIC_ARGS_INVALID`。这里逐 leg
    /// 处理，避免将来一个双向单元中只有一条 leg 非法时误放行另一条 leg。
    pub(super) fn preflight_block_outcomes_with_cts_args(
        &self,
        useq: usize,
        unit: &Unit,
        block: &IperfPreflightBlock,
        owner_id: &str,
        lease_secs: u64,
    ) -> Vec<LegOutcome> {
        let has_cts_args_error = unit.legs.iter().any(|leg| {
            matches!(
                &leg.kind,
                LegKind::CtsTraffic(task) if task.setup_error.is_some()
            )
        });
        if !has_cts_args_error {
            return preflight_block_outcomes(unit, block);
        }

        let mut outcomes = Vec::new();
        for (lidx, leg) in unit.legs.iter().enumerate() {
            match &leg.kind {
                LegKind::CtsTraffic(task) if task.setup_error.is_some() => {
                    outcomes.push(self.run_ctstraffic_leg(
                        useq,
                        unit,
                        lidx,
                        &leg.tag,
                        task,
                        LifecycleLease {
                            owner_id,
                            lease_secs,
                        },
                    ));
                }
                LegKind::IperfSingle(_) | LegKind::IperfGroup { .. } | LegKind::CtsTraffic(_) => {
                    outcomes.push(preflight_block_outcome(&leg.tag, block));
                }
                LegKind::Ping(_) => {}
            }
        }
        if outcomes.is_empty() {
            outcomes.push(preflight_block_outcome("", block));
        }
        outcomes
    }

    pub(super) fn build_cts_requests(
        &self,
        task: &CtsTrafficTask,
    ) -> Result<(CtsTrafficReq, CtsTrafficReq), String> {
        let (client_endpoint, server_endpoint) = if task.udp {
            // ctsTraffic UDP 固定 server 发、client 收；数据方向仍保持 src -> dst。
            (&task.dst, &task.src)
        } else {
            // TCP Push 固定 client 发、server 收。
            (&task.src, &task.dst)
        };
        let (client_bind, client_target, server_bind) = if task.v6 {
            let addrs = v6_addrs(&client_endpoint.nic, &server_endpoint.nic)
                .ok_or_else(|| "ctsTraffic 两端缺少可用 IPv6 地址".to_string())?;
            (
                add_zone(
                    &addrs.client_bind,
                    &client_endpoint.nic.zone,
                    client_endpoint.side,
                ),
                add_zone(
                    &addrs.client_target,
                    &client_endpoint.nic.zone,
                    client_endpoint.side,
                ),
                add_zone(
                    &addrs.server_bind,
                    &server_endpoint.nic.zone,
                    server_endpoint.side,
                ),
            )
        } else {
            (
                client_endpoint.nic.ipv4.clone(),
                server_endpoint.nic.ipv4.clone(),
                server_endpoint.nic.ipv4.clone(),
            )
        };
        let protocol = if task.udp {
            CtsTrafficProtocol::Udp
        } else {
            CtsTrafficProtocol::Tcp
        };
        let common = CtsTrafficReq {
            protocol,
            port: task.port,
            duration_secs: task.duration,
            streams: task.streams,
            window_bytes: task.window_bytes,
            bits_per_second: task.bits_per_second,
            datagram_bytes: task.datagram_bytes,
            frame_rate: task.frame_rate,
            buffer_depth_secs: task.buffer_depth_secs,
            status_update_ms: task.status_update_ms,
            ..Default::default()
        };
        Ok((
            CtsTrafficReq {
                role: CtsTrafficRole::Server,
                bind_ip: server_bind,
                ..common.clone()
            },
            CtsTrafficReq {
                role: CtsTrafficRole::Client,
                bind_ip: client_bind,
                target_ip: client_target,
                ..common
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn save_ctstraffic_raw_record(
        &self,
        owner_id: &str,
        lidx: usize,
        tag: &str,
        task: &CtsTrafficTask,
        server_cmd: &str,
        attempts: &[CtsAttemptRun],
        error: &str,
    ) -> String {
        let filename = format!(
            "ctstraffic_raw_{}_l{:02}_{}_{}_p{}.log",
            sanitize(owner_id),
            lidx,
            if task.udp { "udp" } else { "tcp" },
            sanitize(if tag.is_empty() { "oneway" } else { tag }),
            task.port
        );
        let selected = attempts
            .iter()
            .find(|attempt| attempt.traffic_established)
            .or_else(|| attempts.last());
        let contents = format!(
            "# CPE ctsTraffic raw record\n\
# saved_at,{}\n\
# transport,{}\n\
# profile,{}\n\
# source,{} / {} / {}\n\
# destination,{} / {} / {}\n\
# port,{}\n\
# duration_secs,{}\n\
# requested_connections,{}\n\
# attempts,{}\n\
# client_ok,{}\n\
# client_timed_out,{}\n\
# client_cancelled,{}\n\
# error,{}\n\
\n{}",
            now_full(),
            if task.udp {
                "UDP MediaStream"
            } else {
                "TCP Push"
            },
            task.profile_label,
            task.src.side.cn(),
            task.src.nic.name,
            task.src.nic.ipv4,
            task.dst.side.cn(),
            task.dst.nic.name,
            task.dst.nic.ipv4,
            task.port,
            task.duration,
            task.streams,
            attempts.len(),
            selected.map(|attempt| attempt.client.ok).unwrap_or(false),
            selected
                .map(|attempt| attempt.client.timed_out)
                .unwrap_or(false),
            selected
                .map(|attempt| attempt.client.cancelled)
                .unwrap_or(false),
            error.replace(['\r', '\n'], " "),
            format_ctstraffic_attempts(server_cmd, attempts, error),
        );
        self.write_output_artifact(&filename, &contents, "ctsTraffic 原始记录")
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_ctstraffic_attempt(
        &self,
        task: &CtsTrafficTask,
        server_req: &CtsTrafficReq,
        client_req: &CtsTrafficReq,
        server_side: Side,
        client_side: Side,
        lifecycle: LifecycleLease<'_>,
        attempt: usize,
        leg_epoch: &Instant,
    ) -> CtsAttemptRun {
        let protocol = if task.udp {
            CtsTrafficProtocol::Udp
        } else {
            CtsTrafficProtocol::Tcp
        };
        let setup_failure = |server_output: String,
                             cleanup_confirmed: bool,
                             code: ReasonCode,
                             detail: String| CtsAttemptRun {
            attempt,
            client: IperfClientOut {
                cancelled: !cleanup_confirmed,
                output: detail.clone(),
                ..Default::default()
            },
            server_output,
            server_unexpected_failure: false,
            traffic_window: EffectiveWindow {
                required_secs: task.duration,
                ..Default::default()
            },
            events: Vec::new(),
            parsed: ctstraffic::CtsTrafficParsed::default(),
            traffic_established: false,
            full_attempt: false,
            cleanup_confirmed,
            setup_error: Some((code, detail)),
        };

        let server_request_id =
            lifecycle_request_id(lifecycle.owner_id, "cts-server", task.port, attempt);
        let server_started = match self.cts_job_start(
            server_side,
            CtsTrafficStartReq {
                request: server_req.clone(),
                request_id: server_request_id.clone(),
                owner_id: lifecycle.owner_id.to_string(),
                lease_secs: lifecycle.lease_secs,
            },
        ) {
            Ok(value) => value,
            Err(error) => {
                let cleanup = self.cts_job_stop_confirmed(server_side, &server_request_id);
                let cleanup_confirmed = cleanup.is_ok();
                let detail = format!(
                    "ctsTraffic server 启动失败: {error}；补偿清理: {}",
                    cleanup
                        .map(|_| "已确认".to_string())
                        .unwrap_or_else(|cleanup_error| cleanup_error)
                );
                return setup_failure(
                    detail.clone(),
                    cleanup_confirmed,
                    ReasonCode::CtsServerStartFailed,
                    detail,
                );
            }
        };
        if server_started.id != server_request_id {
            let actual_cleanup = self.cts_job_stop_confirmed(server_side, &server_started.id);
            let expected_cleanup = self.cts_job_stop_confirmed(server_side, &server_request_id);
            let cleanup_confirmed = actual_cleanup.is_ok() && expected_cleanup.is_ok();
            let detail = format!(
                "ctsTraffic server 返回非预期 job id：期望 {server_request_id}，实际 {}；实际 ID 清理={}；期望 ID 清理={}",
                server_started.id,
                actual_cleanup
                    .map(|_| "已确认".to_string())
                    .unwrap_or_else(|error| error),
                expected_cleanup
                    .map(|_| "已确认".to_string())
                    .unwrap_or_else(|error| error)
            );
            return setup_failure(
                detail.clone(),
                cleanup_confirmed,
                ReasonCode::CtsServerJobIdMismatch,
                detail,
            );
        }

        std::thread::sleep(Duration::from_millis(750));
        match self.cts_job_status(server_side, &server_started.id, 0) {
            Ok(status) if status.done => {
                let result = status.result.unwrap_or_default();
                let cleanup = self.cts_job_stop_confirmed(server_side, &server_started.id);
                let cleanup_confirmed = cleanup.is_ok();
                let detail = format!(
                    "ctsTraffic server 在 client 启动前退出；停止确认: {}；输出: {}",
                    cleanup
                        .map(|_| "成功".to_string())
                        .unwrap_or_else(|error| error),
                    result.output.lines().last().unwrap_or_default()
                );
                return setup_failure(
                    result.output,
                    cleanup_confirmed,
                    if cleanup_confirmed {
                        ReasonCode::CtsServerExitedEarly
                    } else {
                        ReasonCode::CtsServerStopFailed
                    },
                    detail,
                );
            }
            Ok(_) => {}
            Err(error) => {
                let cleanup = self.cts_job_stop_confirmed(server_side, &server_started.id);
                let cleanup_confirmed = cleanup.is_ok();
                let detail = format!(
                    "ctsTraffic server 启动后状态查询失败: {error}；停止确认: {}",
                    cleanup
                        .map(|_| "成功".to_string())
                        .unwrap_or_else(|cleanup_error| cleanup_error)
                );
                return setup_failure(
                    detail.clone(),
                    cleanup_confirmed,
                    if cleanup_confirmed {
                        ReasonCode::CtsServerStatusFailed
                    } else {
                        ReasonCode::CtsServerStopFailed
                    },
                    detail,
                );
            }
        }

        let client_call_offset_ms = leg_epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mut events = Vec::<IperfFlowEvent>::new();
        let client_run = self.cts_client_run_tracked(
            client_side,
            CtsTrafficStartReq {
                request: client_req.clone(),
                request_id: lifecycle_request_id(
                    lifecycle.owner_id,
                    "cts-client",
                    task.port,
                    attempt,
                ),
                owner_id: lifecycle.owner_id.to_string(),
                lease_secs: lifecycle.lease_secs,
            },
            |mut event| {
                event.elapsed_ms = event.elapsed_ms.saturating_add(client_call_offset_ms);
                events.push(event);
            },
        );
        let server_stop = self.cts_job_stop_confirmed(server_side, &server_started.id);
        let server_output = server_stop
            .as_ref()
            .ok()
            .and_then(|output| output.result.as_ref())
            .map(|result| result.output.clone())
            .unwrap_or_else(|| {
                server_stop
                    .as_ref()
                    .err()
                    .map(|error| format!("ctsTraffic server 停止未确认: {error}"))
                    .unwrap_or_default()
            });
        let (server_cancelled_before_stop, server_unexpected_failure) =
            cts_server_pre_stop_failures(&server_stop);
        let combined_output = format!("{}\n{}", client_run.client.output, server_output);
        let parsed = ctstraffic::parse_output(&combined_output, protocol);
        let traffic_established = parsed.has_measurement(protocol);
        let traffic_window =
            cts_effective_window(&events, task.duration, u64::from(task.status_update_ms));
        let process_started_confirmed = client_run.client.process_started == Some(true);
        let process_cleanup_confirmed = client_run.client.cleanup_confirmed == Some(true);
        let (server_process_started_confirmed, server_process_cleanup_confirmed) =
            cts_stop_process_evidence(&server_stop);
        let cleanup_confirmed = client_run.cleanup_confirmed
            && process_cleanup_confirmed
            && server_stop.is_ok()
            && server_process_cleanup_confirmed;
        let setup_error = if let Err(error) = &server_stop {
            Some((
                ReasonCode::CtsServerStopFailed,
                format!("ctsTraffic server 停止未确认，禁止复用端口: {error}"),
            ))
        } else if client_run.setup_error.is_some() {
            client_run.setup_error
        } else if server_cancelled_before_stop {
            Some((
                ReasonCode::CtsServerCancelled,
                server_stop
                    .as_ref()
                    .ok()
                    .and_then(|output| output.result.as_ref())
                    .and_then(|result| result.output.lines().last())
                    .unwrap_or("ctsTraffic server 在本次显式停止前已被取消")
                    .to_string(),
            ))
        } else if let Some(error) = cts_server_unexpected_setup_error(
            server_unexpected_failure,
            traffic_established,
            &server_output,
        ) {
            Some(error)
        } else if !server_process_started_confirmed {
            Some((
                ReasonCode::CtsServerProcessNotStarted,
                "ctsTraffic server 未明确证明底层进程已成功启动（process_started != true）".into(),
            ))
        } else if !server_process_cleanup_confirmed {
            Some((
                ReasonCode::CtsServerProcessCleanupUnconfirmed,
                "ctsTraffic server 未明确证明底层进程已 wait/reap（cleanup_confirmed != true）"
                    .into(),
            ))
        } else if !process_started_confirmed {
            Some((
                ReasonCode::CtsClientProcessNotStarted,
                "ctsTraffic client 未明确证明底层进程已成功启动（process_started != true）".into(),
            ))
        } else if !process_cleanup_confirmed {
            Some((
                ReasonCode::CtsClientProcessCleanupUnconfirmed,
                "ctsTraffic client 未明确证明底层进程已 wait/reap（cleanup_confirmed != true）"
                    .into(),
            ))
        } else {
            None
        };
        let full_attempt = client_run.started
            && process_started_confirmed
            && process_cleanup_confirmed
            && server_process_started_confirmed
            && server_process_cleanup_confirmed
            && cleanup_confirmed
            && setup_error.is_none()
            && !client_run.client.cancelled;

        CtsAttemptRun {
            attempt,
            client: client_run.client,
            server_output,
            server_unexpected_failure,
            traffic_window,
            events,
            parsed,
            traffic_established,
            full_attempt,
            cleanup_confirmed,
            setup_error,
        }
    }

    pub(super) fn run_ctstraffic_leg(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        tag: &str,
        task: &CtsTrafficTask,
        lifecycle: LifecycleLease<'_>,
    ) -> LegOutcome {
        let time = now_full();
        if let Some(error) = &task.setup_error {
            return self.push_cts_setup_error_row(
                useq,
                unit,
                lidx,
                tag,
                task,
                time,
                ReasonCode::CtsArgsInvalid,
                error.clone(),
            );
        }
        logln(&format!(
            "  [ctsTraffic{}] {} {} -> {} 端口{} {}s...",
            fmt_tag(tag),
            task.profile_label,
            task.src.brief(),
            task.dst.brief(),
            task.port,
            task.duration
        ));
        let (server_req, client_req) = match self.build_cts_requests(task) {
            Ok(value) => value,
            Err(error) => {
                return self.push_cts_setup_error_row(
                    useq,
                    unit,
                    lidx,
                    tag,
                    task,
                    time,
                    ReasonCode::CtsArgsInvalid,
                    error,
                );
            }
        };
        let (server_side, client_side) = if task.udp {
            (task.src.side, task.dst.side)
        } else {
            (task.dst.side, task.src.side)
        };
        let server_args = match ctstraffic::build_args(&server_req) {
            Ok(args) => args,
            Err(error) => {
                return self.push_cts_setup_error_row(
                    useq,
                    unit,
                    lidx,
                    tag,
                    task,
                    time,
                    ReasonCode::CtsArgsInvalid,
                    error,
                );
            }
        };
        let server_cmd = ctstraffic::command_string("ctsTraffic.exe", &server_args);
        let strict_single_udp = task.udp && task.streams == 1;
        let max_attempts = cts_attempt_budget(
            self.cfg.iperf.rate_check.flow_retries as usize,
            strict_single_udp,
        );

        // 所有 CTS 事件和网卡样本都对齐到同一个 leg epoch。远端 monitor
        // 的真实启动由响应中的 elapsed_ms 与成功调用自身耗时做有界估计，
        // 不再用 RPC 往返中点猜测零点。
        let leg_epoch = Instant::now();
        let monitor_start_before_ms = leg_epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mut monitor_issue = None::<CtsMonitorIssue>;
        let mon_id = match self.mon_start(
            task.dst.side,
            &task.dst.nic.name,
            lifecycle.owner_id,
            lifecycle.lease_secs,
        ) {
            Ok((id, call_origin_ms)) => Some((id, monitor_start_before_ms + call_origin_ms)),
            Err(error) => {
                let detail = format!("CTS 接收端网卡监控启动失败: {error}");
                logln(&format!("    ({detail})"));
                monitor_issue = Some(CtsMonitorIssue {
                    code: ReasonCode::CtsMonitorStartFailed,
                    detail,
                    setup_error: true,
                    affects_verdict: true,
                });
                None
            }
        };
        // 发送端采样：有目标时 W08 要求双侧滚动窗口都完整。启动失败只记诊断，
        // 不像接收端那样直接影响 verdict——接收端才是正式判定口径。
        let tx_mon_id = if task.src.key() == task.dst.key() {
            None
        } else {
            let before_ms = leg_epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
            match self.mon_start(
                task.src.side,
                &task.src.nic.name,
                lifecycle.owner_id,
                lifecycle.lease_secs,
            ) {
                Ok((id, call_origin_ms)) => Some((id, before_ms + call_origin_ms)),
                Err(error) => {
                    logln(&format!("    (CTS 发送端网卡监控启动失败: {error})"));
                    None
                }
            }
        };

        let mut attempts = Vec::with_capacity(max_attempts);
        for attempt in 0..max_attempts {
            let run = self.run_ctstraffic_attempt(
                task,
                &server_req,
                &client_req,
                server_side,
                client_side,
                lifecycle,
                attempt,
                &leg_epoch,
            );
            attempts.push(run);

            if !cts_should_retry_after_last(&attempts, max_attempts, strict_single_udp) {
                break;
            }

            let retry_no = attempt + 1;
            if let Some(previous) = attempts.last_mut() {
                previous.events.push(IperfFlowEvent {
                    kind: IperfEventKind::Retry,
                    elapsed_ms: leg_epoch.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    mbps: None,
                    line: format!(
                        "ctsTraffic single UDP retry {retry_no}/{retries}",
                        retries = max_attempts.saturating_sub(1)
                    ),
                });
            }
            logln(&format!(
                "    [CTS UDP 单流重试]{} 第 {} 次完整尝试无工具测量，双端清理已确认，将重启 server/client（{retry_no}/{}）",
                fmt_tag_bracket(tag),
                attempt + 1,
                max_attempts.saturating_sub(1)
            ));
            std::thread::sleep(Duration::from_millis(500));
        }

        let rx_origin_offset_ms = mon_id.as_ref().map(|(_, offset)| *offset).unwrap_or(0);
        let tx_origin_offset_ms = tx_mon_id.as_ref().map(|(_, offset)| *offset).unwrap_or(0);
        let mon_out = match mon_id {
            Some((id, start_offset_ms)) => match self.mon_stop(task.dst.side, &id) {
                Ok(mut output) => {
                    align_monitor_samples(&mut output, start_offset_ms);
                    Some(output)
                }
                Err(error) => {
                    let detail = format!("CTS 接收端网卡监控停止失败: {error}");
                    logln(&format!("    ({detail})"));
                    monitor_issue = Some(CtsMonitorIssue {
                        code: ReasonCode::CtsMonitorStopFailed,
                        detail,
                        setup_error: false,
                        affects_verdict: true,
                    });
                    None
                }
            },
            None => None,
        };
        let tx_mon_out =
            tx_mon_id.and_then(
                |(id, start_offset_ms)| match self.mon_stop(task.src.side, &id) {
                    Ok(mut output) => {
                        align_monitor_samples(&mut output, start_offset_ms);
                        Some(output)
                    }
                    Err(error) => {
                        logln(&format!("    (CTS 发送端网卡监控停止失败: {error})"));
                        None
                    }
                },
            );
        let Some(selected_idx) = select_cts_attempt_index(&attempts) else {
            return self.push_cts_setup_error_row(
                useq,
                unit,
                lidx,
                tag,
                task,
                time,
                ReasonCode::CtsInternalNoAttempt,
                "ctsTraffic 执行器未产生任何尝试记录".into(),
            );
        };
        let selected = &attempts[selected_idx];
        if monitor_issue.is_none() {
            monitor_issue = mon_out
                .as_ref()
                .and_then(|output| cts_monitor_runtime_issue(output, &selected.traffic_window));
        }
        let baseline_cutoff_ms = cts_baseline_cutoff_ms(&attempts);
        let rx_stats = mon_out
            .as_ref()
            .map(|output| {
                monitor_rate_stats(output, &selected.traffic_window, true, baseline_cutoff_ms)
            })
            .unwrap_or_default();
        let tx_stats = tx_mon_out
            .as_ref()
            .or(if task.src.key() == task.dst.key() {
                mon_out.as_ref()
            } else {
                None
            })
            .map(|output| {
                monitor_rate_stats(output, &selected.traffic_window, false, baseline_cutoff_ms)
            })
            .unwrap_or_default();
        let rx_avg = rx_stats.avg_mbps;
        let nic_samples_rx = mon_out
            .as_ref()
            .map(|output| {
                self.save_monitor_samples(
                    lifecycle.owner_id,
                    task.dst.side,
                    &task.dst.nic.name,
                    &task.dst.key(),
                    rx_origin_offset_ms,
                    output,
                )
            })
            .unwrap_or_default();
        // 同 iperf 路径：TX 采样是否决性门槛，样本必须能被回查。
        let nic_samples_tx = tx_mon_out
            .as_ref()
            .map(|output| {
                self.save_monitor_samples(
                    lifecycle.owner_id,
                    task.src.side,
                    &task.src.nic.name,
                    &task.src.key(),
                    tx_origin_offset_ms,
                    output,
                )
            })
            .unwrap_or_default();
        let parsed = &selected.parsed;
        let measurement = selected.traffic_established;
        let runtime_errors = if !task.udp && parsed.time_limit_reached {
            parsed.status_network_errors + parsed.status_protocol_errors
        } else {
            parsed.error_count()
        };
        let requested_streams = task.streams as usize;
        let summary_streams = parsed
            .successful_connections
            .unwrap_or(0)
            .min(task.streams as u64) as usize;
        let active_streams = parsed
            .max_active_streams
            .max(summary_streams)
            .max(usize::from(measurement && requested_streams == 1));
        let per_stream_mbps = task
            .bits_per_second
            .map(|bits_per_second| bits_per_second as f64 / 1_000_000.0);
        let required_streams = required_udp_streams(
            requested_streams,
            &self.cfg.iperf.rate_check,
            task.rx_target_mbps,
            per_stream_mbps,
        );
        let loss = task.udp.then_some(parsed.udp_dropped_pct).flatten();
        let loss_limit = self.cfg.iperf.rate_check.max_udp_loss_pct;
        let client_expected_completion = selected.client.ok
            || (!task.udp && parsed.time_limit_reached && !selected.client.timed_out);
        let full_attempts = cts_full_attempts(&attempts);
        let single_stream_exhausted =
            cts_single_udp_exhausted(&attempts, max_attempts, strict_single_udp);
        let setup_error = attempts
            .iter()
            .find_map(|attempt| attempt.setup_error.clone())
            .or_else(|| {
                attempts
                    .iter()
                    .find(|attempt| !attempt.cleanup_confirmed)
                    .map(|_| {
                        (
                            ReasonCode::CtsCleanupFailed,
                            "ctsTraffic server/client 清理未全部确认，禁止复用端口".to_string(),
                        )
                    })
            })
            .or_else(|| {
                attempts
                    .iter()
                    .find(|attempt| attempt.client.cancelled)
                    .map(|attempt| {
                        (
                            ReasonCode::CtsClientCancelled,
                            attempt
                                .client
                                .output
                                .lines()
                                .last()
                                .unwrap_or("ctsTraffic client 被取消")
                                .to_string(),
                        )
                    })
            })
            .or_else(|| {
                attempts.iter().find_map(|attempt| {
                    cts_server_unexpected_setup_error(
                        attempt.server_unexpected_failure,
                        attempt.traffic_established,
                        &attempt.server_output,
                    )
                })
            });
        let (verdict, reason_code, reason_detail) = if let Some((code, detail)) = setup_error {
            (Verdict::SetupError, code, detail)
        } else if single_stream_exhausted {
            (
                Verdict::RateFail,
                ReasonCode::CtsSingleUdpStreamFailed,
                format!(
                    "CTS 单流 UDP 在 {full_attempts} 次完整 server/client 尝试且每轮双端清理均确认后，仍无 ctsTraffic 自身 rate/bytes/successful frames 测量；该方向必须灌通"
                ),
            )
        } else if !measurement && (selected.client.timed_out || selected.client.cancelled) {
            (
                Verdict::SetupError,
                ReasonCode::CtsClientAborted,
                selected
                    .client
                    .output
                    .lines()
                    .last()
                    .unwrap_or_default()
                    .to_string(),
            )
        } else if !measurement {
            (
                Verdict::SetupError,
                ReasonCode::CtsNoMeasurement,
                selected
                    .client
                    .output
                    .lines()
                    .last()
                    .unwrap_or("没有吞吐测量")
                    .to_string(),
            )
        } else if let Some(runtime_failure) =
            cts_runtime_failure_verdict(selected, runtime_errors, client_expected_completion)
        {
            runtime_failure
        } else if let Some(monitor_verdict) =
            monitor_issue.as_ref().and_then(cts_monitor_issue_verdict)
        {
            monitor_verdict
        } else if !selected.traffic_window.complete {
            (
                Verdict::NotEvaluated,
                ReasonCode::CtsEffectiveWindowShort,
                format!(
                    "CTS 真实流量事件窗口仅 {:.3}s，短于要求的 {}s；未把启动、握手、轮询或清理时间计入有效窗口",
                    selected.traffic_window.available_secs, task.duration
                ),
            )
        } else if required_streams > requested_streams {
            (
                Verdict::NotEvaluated,
                ReasonCode::ConfiguredLoadTooLow,
                format!(
                    "目标与余量要求至少 {required_streams} 条流，但只配置了 {requested_streams} 条"
                ),
            )
        } else if active_streams < required_streams {
            (
                Verdict::NotEvaluated,
                ReasonCode::ActiveStreamsLow,
                format!(
                    "ctsTraffic 最多观测到 {active_streams}/{requested_streams} 条活跃连接，正式判定至少需要 {required_streams} 条"
                ),
            )
        } else {
            // 丢帧判定必须排在网卡采样/目标可信度之后，与 iperf3 路径的判定链
            // 一致：采样不足或目标未知时先产出 NOT_EVALUATED / MEASURED，不能
            // 拿一个无法核对的窗口去判 RATE_FAIL。
            // offered 门槛此前**只有 UDP 链有**：CTS UDP 单流灌不满时
            // `RX < target` 直接判 RX_BELOW_TARGET，正是 UDP 链两个单测拼命
            // 要防的「把发送端瓶颈写成 CPE 性能失败」，在这条路上零防护。
            let nic = evaluate_nic_rx(
                task.rate_mode,
                task.rx_target_mbps,
                &rx_stats,
                &tx_stats,
                crate::master::rate_window::offered_floor_mbps(
                    task.rx_target_mbps,
                    self.cfg.iperf.rate_check.offered_headroom_pct,
                ),
            );
            cts_apply_udp_loss(nic, task.udp, loss_limit, loss)
        };
        let mut raw_diagnostics = Vec::new();
        if !reason_code.is_empty() {
            raw_diagnostics.push(format!("[{reason_code}] {reason_detail}"));
        }
        if let Some(issue) = &monitor_issue {
            if issue.code != reason_code {
                raw_diagnostics.push(format!("[{}] {}", issue.code, issue.detail));
            }
        }
        let raw_error = raw_diagnostics.join("；");
        let raw_log = self.save_ctstraffic_raw_record(
            lifecycle.owner_id,
            lidx,
            tag,
            task,
            &server_cmd,
            &attempts,
            &raw_error,
        );
        let (screenshot_master, screenshot_agent) = if self.cfg.screenshot {
            self.take_screenshots(
                &[task.dst.side, task.src.side],
                &format!("{}_{}", unit.title, tag),
            )
        } else {
            (String::new(), String::new())
        };
        logln(&format!(
            "    结果: {} CTS自报发送={} 接收={} 网卡实测={} 活跃流={}/{}",
            verdict.label(),
            fmt_opt(parsed.send_mbps),
            fmt_opt(parsed.recv_mbps),
            fmt_opt(rx_avg),
            active_streams,
            task.streams
        ));
        let mut raws = vec![(
            format!("ctsTraffic{} 全部尝试输出", fmt_tag(tag)),
            format_ctstraffic_attempts(&server_cmd, &attempts, &raw_error),
        )];
        if let Some(issue) = &monitor_issue {
            raws.push((
                "CTS 接收端网卡监控错误".into(),
                format!("[{}] {}", issue.code, issue.detail),
            ));
        }
        let idx = self.push_row(Row {
            time,
            // CTS 的 transport 列一直写成 `CTS/TCP` / `CTS/UDP`（后端信息混在里面）。
            // 类型化之后后端进了 `backend`，但可见列保持原样，不借重构改报告。
            transport: if task.udp {
                "CTS/UDP".into()
            } else {
                "CTS/TCP".into()
            },
            verdict,
            execution_status: if verdict == Verdict::SetupError {
                if selected.client.cancelled {
                    ExecutionStatus::Cancelled
                } else if selected.client.timed_out {
                    ExecutionStatus::TimedOut
                } else {
                    ExecutionStatus::Error
                }
            } else if verdict == Verdict::NotEvaluated {
                ExecutionStatus::Partial
            } else {
                ExecutionStatus::Completed
            },
            reason_code,
            reason_detail: reason_detail.clone(),
            rx_avg,
            tx_mbps: parsed.send_mbps,
            rx_mbps: parsed.recv_mbps,
            udp_loss: loss,
            command: selected.client.cmd.clone(),
            raw_log,
            nic_samples_rx,
            nic_samples_tx,
            requested_streams,
            active_streams,
            required_streams,
            retry_count: cts_retry_count(&attempts),
            target_mbps: task.rx_target_mbps,
            tx_avg: tx_stats.avg_mbps,
            tx_p10: tx_stats.p10_mbps,
            rx_p10: rx_stats.p10_mbps,
            effective_seconds: Some(selected.traffic_window.available_secs),
            required_seconds: Some(task.duration as f64),
            sample_coverage: Some(rx_stats.coverage),
            window_start_ms: Some(selected.traffic_window.start_ms),
            window_end_ms: Some(selected.traffic_window.end_ms),
            baseline_mbps: Some(rx_stats.baseline_mbps),
            rolling_coverage: Some(rx_stats.rolling_coverage),
            screenshot_master,
            screenshot_agent,
            raws,
            ..base_row(RowIdentity {
                unit_seq: useq,
                leg_index: lidx,
                stream_index: 0,
                group_flag: 0,
                unit,
                leg_tag: tag,
                src: &task.src,
                dst: &task.dst,
                ip: if task.v6 { "V6".into() } else { "V4".into() },
                protocol: if task.udp {
                    RowProtocol::Udp
                } else {
                    RowProtocol::Tcp
                },
                backend: RowBackend::CtsTraffic,
                param: task.profile_label.clone(),
                kind_label: if unit.bidir {
                    format!("★★双向 CTS Traffic-{tag}")
                } else {
                    "CTS Traffic 灌包".into()
                },
                task_id: md5_hex(&format!("{}|{}|ctstraffic", unit.id, tag)),
            })
        });
        LegOutcome {
            judgement: VerdictResult::new(verdict, reason_code, reason_detail),
            rx_avg,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn push_cts_setup_error_row(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        tag: &str,
        task: &CtsTrafficTask,
        time: String,
        reason_code: ReasonCode,
        reason_detail: String,
    ) -> LegOutcome {
        let idx = self.push_row(Row {
            time,
            // CTS 的 transport 列一直写成 `CTS/TCP` / `CTS/UDP`（后端信息混在里面）。
            // 类型化之后后端进了 `backend`，但可见列保持原样，不借重构改报告。
            transport: if task.udp {
                "CTS/UDP".into()
            } else {
                "CTS/TCP".into()
            },
            verdict: Verdict::SetupError,
            execution_status: ExecutionStatus::Error,
            reason_code,
            reason_detail: reason_detail.clone(),
            requested_streams: task.streams as usize,
            raws: vec![("ctsTraffic 启动错误".into(), reason_detail.clone())],
            ..base_row(RowIdentity {
                unit_seq: useq,
                leg_index: lidx,
                stream_index: 0,
                group_flag: 0,
                unit,
                leg_tag: tag,
                src: &task.src,
                dst: &task.dst,
                ip: if task.v6 { "V6".into() } else { "V4".into() },
                protocol: if task.udp {
                    RowProtocol::Udp
                } else {
                    RowProtocol::Tcp
                },
                backend: RowBackend::CtsTraffic,
                param: task.profile_label.clone(),
                kind_label: if unit.bidir {
                    format!("★★双向 CTS Traffic-{tag}")
                } else {
                    "CTS Traffic 灌包".into()
                },
                task_id: md5_hex(&format!("{}|{}|ctstraffic", unit.id, tag)),
            })
        });
        LegOutcome {
            judgement: VerdictResult::setup_error(reason_code, reason_detail),
            rx_avg: None,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    // ---------------- iperf 单条 ----------------
}
