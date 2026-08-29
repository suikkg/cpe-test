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
    /// 网卡列表的 IPv4 前缀过滤；空列表 = 列出全部网卡。
    ///
    /// 必须能在界面上改：默认只放行 `192.168.`，在 10.x / 172.x 的实验网里
    /// 会把整张网卡表过滤成空，而控制台存在的意义就是让人不必回去手改
    /// config.json。
    #[serde(default)]
    pub(super) ipv4_prefixes: Option<Vec<String>>,
}

/// 本机网卡与工具链。连接辅测机之前就可用。
///
/// 有意**不按 `ipv4_prefixes` 过滤**，理由和辅测机状态页那份一样：要填给对面的
/// 那个地址常常就在被过滤掉的管理网段上，过滤过的表在这里等于把答案藏起来。
/// 「网口与策略」那张表才是按前缀筛过的测试口列表，两者用途不同。
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

/// 顶部参数区的回填值。打开页面（`/api/bootstrap`）和导入 config
/// （`/api/import`）共用这一份，两条路填出来的输入框必须一模一样。
pub(super) fn bootstrap_out(state: &UiState) -> BootstrapOut {
    // 默认组的 -P 档位 = **跑默认 -w 档位的那些 TCP test** 的流数集合。
    //
    // 和下面 udp_streams 同一个坑：不能把所有 TCP test 的流数并起来。矩阵里选了
    // 别的 TCP 参数组的行，它们的流数排在前面，会被当成默认组的填进执行区那一格
    // ——于是导进来的默认组变成另一份东西，而它管着所有没选组的行。默认组的
    // -w 存在 `iperf.tcp_windows` 里（附加组各带各的 tcp_windows），按它筛。
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
    // 默认组的流数 = **跑默认档位的那条 UDP test** 的流数。
    //
    // 不能只取「第一条带 udp_streams 的 test」：矩阵里某一行选了别的参数组时，
    // 它的 test 排在前面，那个组的流数会被当成默认组的填进执行区那一格——
    // 于是导进来的默认组变成另一份东西，而它管着所有没选组的行。
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
    // ping 的次数和包长和 tcp_streams 一样只落在 tests[] 上（界面就是这么写下去
    // 的），只读 cfg.ping 的话，一份「ping 50 次 × 64 字节」的配置回填出来是
    // 默认的「100 次 × 32/1600/65500」——三倍的单元数，而人看着框里的数字
    // 以为就是文件里的那份。
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
        // 和 -l / -w 一样要去重：一档 `-b` 会因为 `-l`/`-w` 的每个档位各生成
        // 一份 profile，照抄进输入框的话，「下载 → 导入」每走一轮档位就翻一倍。
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
        screenshot: state.cfg.screenshot,
        ui_plan_supported: true,
    }
}

/// 回显成用户当初的写法：绝对值回显数字，百分比回显 `90%`。
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
                    // 回显成用户当初的写法：绝对值回显数字，百分比回显 `90%`。
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
    // 页面不回显配置文件里的 token；输入留空时沿用已加载值，手工填写时覆盖。
    if !req.token.is_empty() {
        state.cfg.agent_token = req.token.clone();
    }
    // 前缀框清空是一个有意义的选择（= 列出全部网卡），所以用 Option 区分
    // 「没提交这个字段」和「提交了一个空列表」，不能用 is_empty 兜。
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
    // resume 开着时提示并扣除预判会跳过的单元；executor 运行时仍会再判一次。
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
    if console.running.swap(true, Ordering::SeqCst) {
        return Err("已经有一轮测试在跑了".into());
    }
    // 复核页确认过的执行计划哈希，随 run_master 一路带到真正开跑之前再核一次。
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

    // 界面状态先落成一份真实的临时 config，作为 run_master 的统一入口；
    // 需要长期保留的副本由 /api/config 下载，工作线程结束后会删除这里的文件。
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
    crate::cancel::reset();
    let worker_console = Arc::clone(console);
    let cleanup_path = path.clone();
    let config_path = path.to_string_lossy().to_string();
    let worker = std::thread::Builder::new()
        .name("cpe-test-webui-run".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_master(MasterOpts {
                    config_path: Some(config_path),
                    auto: true,
                    no_open: true,
                    // 复核页确认过什么就跑什么：执行端会自己再推导一次计划，
                    // 对不上这个哈希就拒绝开跑。
                    expected_plan_hash: confirmed_plan_hash,
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

/// 把临时 config 写成只有本人可读的文件。
///
/// 这份 config 里带着 `agent_token`，而 `std::env::temp_dir()` 在 Linux/macOS 上
/// 就是全局可读的 /tmp、`fs::write` 建出来的是 0644——同机的任何账号都能在这一轮
/// 测试期间把它 cat 出来。命令行那边特意支持用环境变量传 token，就是为了不让它
/// 落进 shell 历史和 ps 里给同机的人看见；这里不能又原样漏回去。
///
/// Windows 上 temp 目录本就是每个用户各一份，按默认 ACL 建文件即可。
pub(super) fn write_private_config(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    // `mode()` 只在**创建**那一刻生效。同 pid 的上一轮如果留下过一个 0644 的
    // 残file，truncate 会沿用它原来的权限，所以先删干净再 create_new。
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
    let (total, lines) = log_tail_since(from);
    // 报告路径从日志里捞：run_master 自己决定运行目录名，界面在点下
    // 「开始测试」的那一刻还不知道它叫什么。
    {
        let mut report = lock_recover(&console.report);
        if report.is_empty() {
            if let Some(found) = lines
                .iter()
                .find_map(|line| line.split_once("报告已生成: ").map(|(_, p)| p.trim()))
            {
                *report = found.to_string();
            }
        }
    }
    serde_json::to_value(ProgressOut {
        running: console.running.load(Ordering::SeqCst),
        from: total,
        lines,
        report: lock_recover(&console.report).clone(),
    })
    .unwrap_or(serde_json::Value::Null)
}
