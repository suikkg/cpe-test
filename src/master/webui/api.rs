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

/// 控制台开局回填的默认值。
///
/// 读 `state.cfg`——而它在启动时已经被 `webui::console_baseline_config` 过滤过：
/// exe 旁边那份 `config.json` 只留下连接信息，判定与档位一律是内置默认值。
/// 用户**显式**点「导入 config.json」时 `state.cfg` 会被整份替换，那时这里
/// 回填的就是他自己挑的那份文件——这正是两者该有的区别。
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
        ping_small_max_bytes: state.cfg.ping.small_max_bytes,
        ping_medium_max_bytes: state.cfg.ping.medium_max_bytes,
        ping_wired_small_avg_rtt_ms: state.cfg.ping.wired_small_avg_rtt_ms,
        ping_wired_small_max_rtt_ms: state.cfg.ping.max_rtt_ms,
        ping_wired_medium_avg_rtt_ms: state.cfg.ping.wired_medium_avg_rtt_ms,
        ping_wired_medium_max_rtt_ms: state.cfg.ping.wired_medium_max_rtt_ms,
        ping_wired_large_avg_rtt_ms: state.cfg.ping.wired_large_avg_rtt_ms,
        ping_wired_large_max_rtt_ms: state.cfg.ping.wired_large_max_rtt_ms,
        ping_wifi_small_avg_rtt_ms: state.cfg.ping.wifi_small_avg_rtt_ms,
        ping_wifi_small_max_rtt_ms: state.cfg.ping.wifi_small_max_rtt_ms,
        ping_wifi_medium_avg_rtt_ms: state.cfg.ping.wifi_medium_avg_rtt_ms,
        ping_wifi_medium_max_rtt_ms: state.cfg.ping.wifi_medium_max_rtt_ms,
        ping_wifi_large_avg_rtt_ms: state.cfg.ping.wifi_large_avg_rtt_ms,
        ping_wifi_large_max_rtt_ms: state.cfg.ping.wifi_large_max_rtt_ms,
        master_config: super::plan::master_config_snapshot(&state.cfg),
        screenshot: state.cfg.screenshot,
        probe_during_traffic: state.cfg.ping.probe_during_traffic,
        probe_path_mtu: state.cfg.ping.probe_path_mtu,
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
    crate::config::validate_agent_address_for_http(req.host.trim())?;
    crate::config::validate_agent_token_for_http(&req.token)?;
    let mut state = lock_recover(&console.state);
    // 连接参数和网卡清单必须来自同一次成功扫描。先改地址再联网会在
    // health/info 失败时留下「新地址 + 旧网卡」，让后续计划指向错误的机器。
    let mut cfg = state.cfg.clone();
    let agent_host = if req.host.trim().is_empty() {
        state.agent_host.clone()
    } else {
        req.host.trim().to_string()
    };
    if req.port > 0 {
        cfg.agent_port = req.port;
    }
    if !req.token.is_empty() {
        cfg.agent_token = req.token.clone();
    }
    if let Some(prefixes) = &req.ipv4_prefixes {
        cfg.ipv4_prefixes = cleaned_list(prefixes);
    }
    if agent_host.is_empty() {
        return Err("请先填辅测机 IP（辅测机 agent 窗口里显示的那个地址）".into());
    }

    let health: HealthOut = post(
        &agent_host,
        cfg.agent_port,
        "/health",
        "{}",
        &cfg.agent_token,
    )
    .map_err(|e| {
        format!(
            "辅测机 {}:{} 连不上。请确认对方已双击 start_agent.bat，且 {} 端口在防火墙放行（{e}）",
            agent_host, cfg.agent_port, cfg.agent_port
        )
    })?;
    let info_request = InfoReq::for_scan(&cfg.ipv4_prefixes);
    info_request.check_capabilities(&health.capabilities)?;
    let info_body = serde_json::to_string(&info_request).unwrap_or_else(|_| "{}".into());
    let agent: HostInfo = post(
        &agent_host,
        cfg.agent_port,
        "/info",
        &info_body,
        &cfg.agent_token,
    )
    .map_err(|e| format!("已连上辅测机，但获取网卡失败: {e}"))?;

    let master = crate::nic::scan_host(&cfg.ipv4_prefixes);
    let nic_policies = configured_nic_policies(&cfg, &master, &agent);
    state.agent_host = agent_host;
    state.cfg = cfg;
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
    let blocking_errors = compiled.blocking_errors(req.ui_plan.is_some());
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
            targets: unit_target_lines(unit),
        })
        .collect();
    serde_json::to_value(PlanOut {
        units,
        est_total_secs,
        est_full_secs,
        notices: compiled.notices,
        blocking_errors,
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
    api_run_impl(console, body, false)
}

/// 组合场景的第一阶段复用同一套编译、计划哈希和 executor 路径；唯一额外
/// 的区别是它已经持有组合场景的状态闸门，不能被普通 `/api/run` 再挡一次。
pub(super) fn api_run_for_scenario(
    console: &Arc<Console>,
    body: &str,
) -> Result<serde_json::Value, String> {
    api_run_impl(console, body, true)
}

fn api_run_impl(
    console: &Arc<Console>,
    body: &str,
    from_scenario: bool,
) -> Result<serde_json::Value, String> {
    let req: RunRequest = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let run_gate = lock_recover(&console.run_gate);
    if console.scenario.is_running() && !from_scenario {
        return Err("组合场景正在运行，请结束后再启动子网测试".into());
    }
    if console.inner.is_running() {
        return Err("内环测试正在运行，请结束后再启动子网测试".into());
    }
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
                if let Some(error) = compiled
                    .blocking_errors(req.ui_plan.is_some())
                    .into_iter()
                    .next()
                {
                    console.running.store(false, Ordering::SeqCst);
                    return Err(error);
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

/// 跳过当前正在跑的那个单元，队列继续。
///
/// 设计文档 v4.3.0 把「暂停 / 跳过当前」记成「留待后续版本」。这里只做
/// **跳过**：11.5 小时的队列里发现第 3 个单元参数配错，此前只有两个选择——
/// 让它跑完，或者停掉重来（RESUME 救得回已 PASS 的，救不回 RATE_FAIL 的）。
///
/// 和 `/api/stop` 同属 gated：它掐断正在跑的作业，动的是被测资源。
#[derive(Deserialize)]
struct SkipUnitReq {
    run_id: String,
    unit_seq: usize,
}

pub(super) fn api_skip_unit(
    console: &Arc<Console>,
    body: &str,
) -> Result<serde_json::Value, String> {
    let request: SkipUnitReq = serde_json::from_str(body)
        .map_err(|_| "跳过请求必须包含 run_id 和 unit_seq，请刷新页面后重试".to_string())?;
    let _run_gate = lock_recover(&console.run_gate);
    if !console.running.load(Ordering::SeqCst) {
        return Err("当前没有正在运行的测试".into());
    }
    if crate::cancel::is_stop_requested() {
        return Err("已经请求停止整轮了，跳过没有意义".into());
    }
    console
        .run_status
        .request_skip(&request.run_id, request.unit_seq)?;
    Ok(
        serde_json::json!({ "skipping": true, "run_id": request.run_id, "unit_seq": request.unit_seq }),
    )
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
