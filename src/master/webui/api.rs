//! `/api/*` 各个端点。
//!
//! 每个端点都薄：解析请求、调校验、调编译、返回。真正的规则在
//! [`super::validate`] 和 [`super::plan`] 里——端点自己不该有判断力，
//! 否则同一条规则会在 CLI 和 WebUI 两条路上各长一份。

use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct ConnectReq {
    pub(super) host: String,
    #[serde(default)]
    pub(super) port: u16,
    #[serde(default)]
    pub(super) token: String,
    #[serde(default)]
    pub(super) ipv4_prefixes: Option<Vec<String>>,
}

pub(super) fn api_local() -> Result<serde_json::Value, String> {
    serde_json::to_value(LocalOut {
        host: crate::nic::scan_host(&[]),
        iperf3: crate::cmd::tools::iperf3_version(),
        version: env!("CARGO_PKG_VERSION").into(),
    })
    .map_err(|error| error.to_string())
}

pub(super) fn api_bootstrap(console: &Arc<Console>) -> Result<serde_json::Value, String> {
    let state = lock_recover(&console.state);
    serde_json::to_value(bootstrap_out(&state)).map_err(|error| error.to_string())
}

pub(super) fn bootstrap_out(state: &UiState) -> BootstrapOut {
    let default_windows = &state.cfg.iperf.tcp_windows;
    let mut tcp_streams: Vec<u32> = state
        .cfg
        .tests
        .iter()
        .filter(|test| test.transports.iter().any(|t| t.trim() == "tcp"))
        .filter(|test| {
            test.tcp_windows
                .as_ref()
                .is_none_or(|windows| windows == default_windows)
        })
        .filter_map(|test| test.tcp_streams)
        .filter(|value| *value > 0)
        .collect();
    tcp_streams.sort_unstable();
    tcp_streams.dedup();
    if tcp_streams.is_empty() {
        tcp_streams.push(10);
    }
    let udp_tests = state
        .cfg
        .tests
        .iter()
        .filter(|test| test.transports.iter().any(|t| t.trim() == "udp"));
    let default_profiles = &state.cfg.iperf.udp_profiles;
    let udp_streams = udp_tests
        .clone()
        .find(|test| {
            test.udp_profiles
                .as_ref()
                .is_none_or(|profiles| profiles == default_profiles)
        })
        .or_else(|| udp_tests.clone().next())
        .and_then(|test| test.udp_streams)
        .filter(|value| *value > 0)
        .unwrap_or(1);
    let ping_count = state
        .cfg
        .tests
        .iter()
        .filter_map(|test| test.ping_count)
        .find(|value| *value > 0)
        .unwrap_or(state.cfg.ping.count);
    let ping_payload_sizes = state
        .cfg
        .tests
        .iter()
        .filter_map(|test| test.ping_payload_sizes.clone())
        .find(|sizes| !sizes.is_empty())
        .unwrap_or_else(|| state.cfg.ping.payload_sizes.clone());
    BootstrapOut {
        agent_host: state.agent_host.clone(),
        agent_port: state.cfg.agent_port,
        token_configured: !state.cfg.agent_token.is_empty(),
        ipv4_prefixes: state.cfg.ipv4_prefixes.clone(),
        duration: state.cfg.iperf.duration,
        tcp_windows: state.cfg.iperf.tcp_windows.clone(),
        tcp_streams,
        udp_bandwidths: distinct(
            state
                .cfg
                .iperf
                .udp_profiles
                .iter()
                .map(|profile| profile.bandwidth.clone()),
        ),
        udp_lengths: distinct(
            state
                .cfg
                .iperf
                .udp_profiles
                .iter()
                .filter_map(|profile| profile.length.clone()),
        ),
        udp_windows: distinct(
            state
                .cfg
                .iperf
                .udp_profiles
                .iter()
                .filter_map(|profile| profile.window.clone()),
        ),
        udp_streams,
        ping_count,
        ping_payload_sizes,
        ping_max_rtt_ms: state.cfg.ping.max_rtt_ms,
        screenshot: state.cfg.screenshot,
        ui_plan_supported: true,
    }
}

pub(super) fn rx_target_text(mbps: Option<f64>, percent: Option<f64>) -> String {
    match (mbps, percent) {
        (Some(mbps), _) => format!("{mbps}"),
        (None, Some(percent)) => format!("{percent}%"),
        (None, None) => String::new(),
    }
}

pub(super) fn configured_nic_policies(
    cfg: &Config,
    master: &HostInfo,
    agent: &HostInfo,
) -> Vec<NicPolicySelection> {
    let mut policies = Vec::new();
    for (host, info) in [("master", master), ("agent", agent)] {
        for nic in &info.interfaces {
            if let Some(profile) = cfg
                .link_profiles
                .by_nic
                .iter()
                .find(|profile| crate::rate::nic_profile_matches(profile, host, nic))
            {
                policies.push(NicPolicySelection {
                    endpoint: format!("{host}:NAME={}", nic.name),
                    rx_target: rx_target_text(profile.rx_target_mbps, profile.rx_target_percent),
                    udp_bandwidth: profile.udp_bandwidth.clone().unwrap_or_default(),
                    udp_length: profile.udp_length.clone().unwrap_or_default(),
                });
            }
        }
    }
    policies
}

pub(super) fn api_connect(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: ConnectReq = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let mut state = lock_recover(&console.state);
    if !req.host.trim().is_empty() {
        state.agent_host = req.host.trim().to_string();
    }
    if req.port > 0 {
        state.cfg.agent_port = req.port;
    }
    if !req.token.is_empty() {
        state.cfg.agent_token = req.token.clone();
    }
    if let Some(prefixes) = &req.ipv4_prefixes {
        state.cfg.ipv4_prefixes = cleaned_list(prefixes);
    }
    if state.agent_host.is_empty() {
        return Err("请先填辅测机 IP（辅测机 agent 窗口里显示的那个地址）".into());
    }

    let health: HealthOut = post(
        &state.agent_host,
        state.cfg.agent_port,
        "/health",
        "{}",
        &state.cfg.agent_token,
    )
    .map_err(|e| {
        format!(
            "辅测机 {}:{} 连不上。请确认对方已双击 start_agent.bat，且 {} 端口在防火墙放行（{e}）",
            state.agent_host, state.cfg.agent_port, state.cfg.agent_port
        )
    })?;
    let info_body = serde_json::to_string(&InfoReq {
        ipv4_prefixes: state.cfg.ipv4_prefixes.clone(),
    })
    .unwrap_or_else(|_| "{}".into());
    let agent: HostInfo = post(
        &state.agent_host,
        state.cfg.agent_port,
        "/info",
        &info_body,
        &state.cfg.agent_token,
    )
    .map_err(|e| format!("已连上辅测机，但获取网卡失败: {e}"))?;

    let master = crate::nic::scan_host(&state.cfg.ipv4_prefixes);
    let nic_policies = configured_nic_policies(&state.cfg, &master, &agent);
    state.master = master.clone();
    state.agent = agent.clone();
    serde_json::to_value(ConnectOut {
        health,
        master,
        agent,
        nic_policies,
    })
    .map_err(|e| e.to_string())
}

pub(super) fn api_plan(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: RunRequest = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let state = lock_recover(&console.state);
    if state.master.interfaces.is_empty() || state.agent.interfaces.is_empty() {
        return Err("还没连上辅测机，先点「连接」".into());
    }
    let mut compiled = compile_request(&state, &req)?;
    let skip_count = compiled.resumed.iter().filter(|skipped| **skipped).count();
    if compiled.cfg.resume {
        compiled.notices.push(if skip_count == 0 {
            format!(
                "resume 已开启，但 {RESUME_MAX_AGE_HOURS} 小时内没有可复用的 PASS，{} 个单元全部实跑",
                compiled.units.len()
            )
        } else {
            format!(
                "resume 已开启：{skip_count}/{} 个单元在 {RESUME_MAX_AGE_HOURS} 小时内已 PASS，预计跳过。执行时还会再判一次",
                compiled.units.len()
            )
        });
    }
    let est_total_secs = compiled
        .units
        .iter()
        .zip(&compiled.resumed)
        .filter(|(_, skipped)| !**skipped)
        .map(|(u, _)| u.est_secs)
        .sum();
    let est_full_secs = compiled.units.iter().map(|u| u.est_secs).sum();
    let units = compiled
        .units
        .iter()
        .zip(&compiled.resumed)
        .enumerate()
        .map(|(idx, (unit, skipped))| PlannedUnit {
            seq: idx + 1,
            title: unit.title.clone(),
            est_secs: unit.est_secs,
            resumed: *skipped,
            load: unit_load_lines(unit),
        })
        .collect();
    serde_json::to_value(PlanOut {
        units,
        est_total_secs,
        est_full_secs,
        notices: compiled.notices,
        sections: compiled.sections,
        trace: compiled.trace,
        plan_hash: Some(compiled.plan_hash),
        topology_fingerprint: Some(compiled.topology_fingerprint),
        ui_plan_supported: true,
    })
    .map_err(|e| e.to_string())
}

pub(super) fn api_config(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: RunRequest = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let state = lock_recover(&console.state);
    if state.master.interfaces.is_empty() || state.agent.interfaces.is_empty() {
        return Err("还没连上辅测机，先点「连接」".into());
    }
    let compiled = compile_request(&state, &req)?;
    serde_json::to_value(compiled.cfg).map_err(|error| format!("生成配置失败: {error}"))
}

pub(super) fn api_run(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let req: RunRequest = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let run_gate = lock_recover(&console.run_gate);
    if crate::cancel::is_shutdown_requested() {
        return Err("控制台正在退出，不能开始新的测试".into());
    }
    if console.running.swap(true, Ordering::SeqCst) {
        return Err("已经有一轮测试在跑了".into());
    }
    crate::cancel::reset();
    drop(run_gate);
    let confirmed_plan_hash;
    let cfg = {
        let state = lock_recover(&console.state);
        if state.master.interfaces.is_empty() || state.agent.interfaces.is_empty() {
            console.running.store(false, Ordering::SeqCst);
            return Err("还没连上辅测机，先点「连接」".into());
        }
        match compile_request(&state, &req) {
            Ok(compiled) => {
                if let Some(error) = compiled.spec_errors.first() {
                    console.running.store(false, Ordering::SeqCst);
                    return Err(error.clone());
                }
                if req.ui_plan.is_some()
                    && compiled
                        .notices
                        .iter()
                        .any(|notice| notice.trim_start().starts_with("跳过 "))
                {
                    console.running.store(false, Ordering::SeqCst);
                    return Err("计划包含不可执行的链路或 IP 版本，请在复核页排除后重新预览".into());
                }
                if req.ui_plan.is_some() {
                    let supplied = req.plan_hash.as_deref().or_else(|| {
                        req.ui_plan
                            .as_ref()
                            .and_then(|plan| plan.plan_hash.as_deref())
                    });
                    let Some(supplied) = supplied.filter(|value| !value.trim().is_empty()) else {
                        console.running.store(false, Ordering::SeqCst);
                        return Err("请先预览任务并携带 plan_hash 后再开始测试".into());
                    };
                    if supplied != compiled.plan_hash {
                        console.running.store(false, Ordering::SeqCst);
                        return Err("计划已过期或网口拓扑已变化，请重新预览任务".into());
                    }
                }
                if compiled.units.is_empty() {
                    console.running.store(false, Ordering::SeqCst);
                    let detail = if compiled.notices.is_empty() {
                        String::new()
                    } else {
                        format!("：{}", compiled.notices.join("；"))
                    };
                    return Err(format!("所选配置最终没有生成任何测试单元{detail}"));
                }
                confirmed_plan_hash = Some(compiled.plan_hash.clone());
                compiled.cfg
            }
            Err(error) => {
                console.running.store(false, Ordering::SeqCst);
                return Err(error);
            }
        }
    };
    if cfg.tests.is_empty() {
        console.running.store(false, Ordering::SeqCst);
        return Err("一个测试项都没勾".into());
    }

    let path = std::env::temp_dir().join(format!("cpe_test_ui_{}.json", std::process::id()));
    let json = match serde_json::to_string_pretty(&cfg) {
        Ok(json) => json,
        Err(error) => {
            console.running.store(false, Ordering::SeqCst);
            return Err(format!("生成临时配置失败: {error}"));
        }
    };
    if let Err(error) = write_private_config(&path, &json) {
        console.running.store(false, Ordering::SeqCst);
        return Err(format!("写临时配置失败: {error}"));
    }

    clear_log_mirror();
    lock_recover(&console.report).clear();
    console.run_status.reset();
    let worker_console = Arc::clone(console);
    let request_snapshot = body.to_string();
    let cleanup_path = path.clone();
    let config_path = path.to_string_lossy().to_string();
    let run_observer: std::sync::Arc<dyn crate::master::run_status::RunObserver> =
        console.run_status.clone();
    let worker = std::thread::Builder::new()
        .name("cpe-test-webui-run".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_master(MasterOpts {
                    config_path: Some(config_path),
                    auto: true,
                    no_open: true,
                    expected_plan_hash: confirmed_plan_hash,
                    observer: Some(run_observer),
                    console_request: Some(request_snapshot),
                    ..Default::default()
                })
            }));
            match result {
                Ok(0) => {}
                Ok(code) => crate::util::logln(&format!("!! 测试流程以状态码 {code} 结束")),
                Err(_) => crate::util::logln("!! 测试主线程异常退出；已保留现有日志和部分结果"),
            }
            let _ = std::fs::remove_file(path);
            worker_console.running.store(false, Ordering::SeqCst);
        });
    if let Err(error) = worker {
        let _ = std::fs::remove_file(cleanup_path);
        console.running.store(false, Ordering::SeqCst);
        return Err(format!("无法启动测试线程: {error}"));
    }
    Ok(serde_json::json!({ "started": true }))
}

pub(super) fn write_private_config(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())
}

pub(super) fn api_stop(console: &Arc<Console>) -> Result<serde_json::Value, String> {
    let _run_gate = lock_recover(&console.run_gate);
    if !console.running.load(Ordering::SeqCst) {
        return Err("当前没有正在运行的测试".into());
    }
    crate::cancel::request_cancel();
    Ok(serde_json::json!({ "stopping": true }))
}

pub(super) fn api_open_report(console: &Arc<Console>) -> Result<serde_json::Value, String> {
    let report = lock_recover(&console.report).clone();
    if report.is_empty() {
        return Err("报告尚未生成".into());
    }
    let path = Path::new(&report);
    if !path.is_file() {
        return Err(format!("报告文件不存在：{}", path.display()));
    }
    crate::console::open_path(path);
    Ok(serde_json::json!({ "opened": true }))
}

pub(super) fn api_progress(console: &Arc<Console>, query: &str) -> serde_json::Value {
    let from = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("from="))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let units_from = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("units_from="))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let client_run_id = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("run_id="))
        .map(urldecode);
    let (total, lines) = log_tail_since(from);
    let (units_from, run) = console
        .run_status
        .snapshot(units_from, client_run_id.as_deref());
    {
        let mut report = lock_recover(&console.report);
        if report.is_empty() && !run.report.is_empty() {
            *report = run.report.clone();
        }
    }
    serde_json::to_value(ProgressOut {
        running: console.running.load(Ordering::SeqCst),
        from: total,
        lines,
        report: lock_recover(&console.report).clone(),
        units_from: units_from + run.done.len(),
        run,
    })
    .unwrap_or(serde_json::Value::Null)
}
