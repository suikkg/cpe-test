//! 主控交互流程：连接辅测 -> 扫描 -> 菜单选任务 -> 执行 -> 报告
//! 设计目标：小白一路回车就能跑起来。

use crate::cmd::iperf::IperfServerMgr;
use crate::config::{load_config, Config};
use crate::http_client;
use crate::master::builder::{self, build_units, Endpoint, LegKind, Side, SpecNorm, Unit};
use crate::master::executor::{Ctx, IperfPreflightBlock, ResultDb};
use crate::nic::monitor::MonitorMgr;
use crate::nic::{format_nic_table, scan_host};
use crate::protocol::{
    HealthOut, HostInfo, InfoReq, Resp, CTS_TRAFFIC_CAPABILITY, RELIABLE_LIFECYCLE_CAPABILITY,
};
use crate::report::{write_report, ReportMeta};
use crate::util::{
    ask, ctstraffic_platform_supported, ctstraffic_version, find_ctstraffic, find_iperf3,
    iperf3_version, log_to_file, logln, now_compact, now_full, open_path, parse_selection,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone)]
pub struct MasterOpts {
    pub agent_host: Option<String>,
    pub agent_port: Option<u16>,
    pub config_path: Option<String>,
    pub prefixes: Option<Vec<String>>,
    pub auto: bool,
    pub resume: bool,
    pub no_open: bool,
    pub screenshot: bool,
}

const LAST_AGENT_FILE: &str = ".cpe_last_agent";
const IPERF_PREFLIGHT_FAILED: &str = "IPERF_PREFLIGHT_FAILED";
const CTS_PREFLIGHT_FAILED: &str = "CTSTRAFFIC_PREFLIGHT_FAILED";

pub fn run_master(opts: MasterOpts) -> i32 {
    let (mut cfg, cfg_path) = load_config(opts.config_path.as_deref());
    if let Some(h) = &opts.agent_host {
        cfg.agent_host = h.clone();
    }
    if let Some(p) = opts.agent_port {
        cfg.agent_port = p;
    }
    if let Some(p) = &opts.prefixes {
        cfg.ipv4_prefixes = p.clone();
    }
    if opts.resume {
        cfg.resume = true;
    }
    if opts.screenshot {
        cfg.screenshot = true;
    }
    if opts.no_open {
        cfg.open_report = false;
    }

    let ts = now_compact();
    log_to_file(&PathBuf::from(format!("master_{ts}.log")));

    logln("==============================================");
    logln(&format!(
        "  CPE 子网测试工具 v{} — 主控",
        env!("CARGO_PKG_VERSION")
    ));
    logln("==============================================");
    if let Some(p) = &cfg_path {
        logln(&format!("已加载配置文件: {}", p.display()));
    } else {
        logln("未找到 config.json，使用默认配置（可交互输入）");
    }

    // ---- 确定辅测机地址 ----
    let mut agent_host = cfg.agent_host.trim().to_string();
    if agent_host.is_empty() {
        if let Ok(last) = std::fs::read_to_string(LAST_AGENT_FILE) {
            let last = last.trim().to_string();
            if !last.is_empty() {
                let inp = ask(&format!("请输入辅测机 IP (回车={last}): "));
                agent_host = if inp.is_empty() { last } else { inp };
            }
        }
        while agent_host.is_empty() {
            agent_host = ask("请输入辅测机 IP (辅测机 agent 窗口里显示的地址): ");
            if agent_host.is_empty() && opts.auto {
                logln("!! --auto 模式必须提供 agent_host（配置文件或 --agent-host）");
                return 2;
            }
        }
    }
    let _ = std::fs::write(LAST_AGENT_FILE, &agent_host);

    // ---- 连接辅测机 ----
    let health = loop {
        logln(&format!(
            "正在连接辅测机 {}:{} ...",
            agent_host, cfg.agent_port
        ));
        match agent_health(&agent_host, cfg.agent_port) {
            Ok(h) => break h,
            Err(e) => {
                logln(&format!("!! 连接失败: {e}"));
                logln("!! 请检查: 1) 辅测机是否已双击运行 start_agent.bat  2) IP 是否输对  3) 防火墙是否放行");
                if opts.auto {
                    return 2;
                }
                let r = ask("回车重试，输入 n 退出: ");
                if r.eq_ignore_ascii_case("n") {
                    return 2;
                }
            }
        }
    };
    logln(&format!(
        "辅测机已连接: {} ({}) agent v{} iperf3: {} ctsTraffic: {}",
        health.hostname,
        health.os,
        health.version,
        health.iperf3.clone().unwrap_or_else(|| "未找到!".into()),
        health
            .ctstraffic
            .clone()
            .unwrap_or_else(|| "未找到/不支持".into())
    ));
    logln(&format!(
        "辅测机协议能力: {}",
        if health.capabilities.is_empty() {
            "未声明（旧版 agent）".into()
        } else {
            health.capabilities.join(", ")
        }
    ));

    // ---- 双端扫描 ----
    logln("正在扫描本机网卡...");
    let master_info = scan_host(&cfg.ipv4_prefixes);
    logln("正在获取辅测机网卡...");
    let agent_info = match agent_info(&agent_host, cfg.agent_port, &cfg.ipv4_prefixes) {
        Ok(i) => i,
        Err(e) => {
            logln(&format!("!! 获取辅测机网卡失败: {e}"));
            return 2;
        }
    };
    logln("");
    logln(&format_nic_table("【主控】", &master_info));
    logln(&format_nic_table("【辅测】", &agent_info));

    if master_info.interfaces.is_empty() || agent_info.interfaces.is_empty() {
        logln(&format!(
            "!! 有一端没有发现符合前缀 {:?} 的网卡。请检查网线/WiFi，或修改 config.json 里的 ipv4_prefixes。",
            cfg.ipv4_prefixes
        ));
        return 2;
    }

    // ---- iperf3 预检 ----
    let local_iperf = find_iperf3();
    match &local_iperf {
        Some(b) => logln(&format!(
            "主控 iperf3: {} ({})",
            b,
            iperf3_version().unwrap_or_default()
        )),
        None => logln("!! 主控未找到 iperf3：ping 可测，灌包会失败。请把 iperf3 放到程序同目录。"),
    }
    if health.iperf3.is_none() {
        logln("!! 辅测机未找到 iperf3：ping 可测，灌包会失败。");
    }
    let local_cts_platform_supported = ctstraffic_platform_supported();
    let local_cts = find_ctstraffic();
    if !local_cts_platform_supported {
        logln("主控 ctsTraffic: 当前平台或系统版本不支持（仅 Windows 10+）");
    } else {
        match ctstraffic_version() {
            Some(version) => logln(&format!("主控 ctsTraffic: {version}")),
            None => {
                logln("!! 主控未找到 ctsTraffic.exe：CTS 单元会失败；iperf3 和 ping 不受影响。")
            }
        }
    }

    // ---- 生成测试规格 ----
    let specs: Vec<SpecNorm> = if !cfg.tests.is_empty() {
        let use_cfg = if opts.auto {
            true
        } else {
            let c = choose_single(
                "检测到配置文件里有 tests，选择配置方式:",
                &["按配置文件执行(推荐)".into(), "交互式选择".into()],
                0,
            );
            c == 0
        };
        if use_cfg {
            let mut out = Vec::new();
            for t in &cfg.tests {
                match builder::spec_from_config(t, &cfg, &master_info, &agent_info) {
                    Ok(s) => out.push(s),
                    Err(e) => logln(&format!("!! 跳过测试 [{}]: {e}", t.name)),
                }
            }
            out
        } else {
            interactive_build_specs(&cfg, &master_info, &agent_info)
        }
    } else if let Some(ref pairs) = cfg.pairs {
        // pairs 模式：从角色对自动生成全量测试
        generate_specs_from_pairs(pairs, &cfg, &master_info, &agent_info)
    } else {
        if opts.auto {
            logln("!! --auto 模式需要配置 tests[] 或 pairs");
            return 2;
        }
        interactive_build_specs(&cfg, &master_info, &agent_info)
    };
    if specs.is_empty() {
        logln("没有可执行的测试，退出。");
        return 1;
    }

    // ---- 生成任务单元 ----
    let mut next_port = builder::PORT_BASE;
    let (mut units, notices) =
        build_units(&specs, cfg.require_same_subnet_for_iperf, &mut next_port);
    for n in &notices {
        logln(&format!("提示: {n}"));
    }
    if units.is_empty() {
        logln("没有生成任何任务（可能全部被跳过），退出。");
        return 1;
    }

    // ---- 勾选任务 ----
    if !opts.auto {
        logln(&format!("\n共生成 {} 个任务:", units.len()));
        for (i, u) in units.iter().enumerate() {
            logln(&format!("  [{}] {}", i + 1, u.title));
        }
        let est: u64 = units.iter().map(|u| u.est_secs).sum();
        logln(&format!("预计总耗时约 {} 分钟", est / 60 + 1));
        loop {
            let inp = ask("输入要执行的任务序号(逗号/连字符, 如 1-5,8; 回车=全部): ");
            match parse_selection(&inp, units.len()) {
                Ok(sel) => {
                    let mut picked: Vec<Unit> = Vec::new();
                    for i in &sel {
                        picked.push(units[*i - 1].clone());
                    }
                    units = picked;
                    break;
                }
                Err(e) => logln(&format!("!! {e}")),
            }
        }
    } else {
        logln(&format!("\n[auto] 共 {} 个任务，直接开始执行", units.len()));
        for (i, u) in units.iter().enumerate() {
            logln(&format!("  [{}] {}", i + 1, u.title));
        }
    }

    // 按后端独立前置检查：缺 ctsTraffic 只拦 CTS，不能连带拦截可用的 iperf/ping。
    let iperf_preflight = iperf_preflight_block(&units, &health, local_iperf.is_some());
    let cts_preflight = ctstraffic_preflight_block(
        &units,
        &health,
        local_cts.is_some(),
        local_cts_platform_supported,
    );
    let mut preflight_blocks: HashMap<String, IperfPreflightBlock> = HashMap::new();
    if let Some(block) = &iperf_preflight {
        let blocked_units: Vec<&Unit> = units.iter().filter(|unit| unit_has_iperf(unit)).collect();
        for unit in &blocked_units {
            preflight_blocks.insert(unit.id.clone(), block.clone());
        }
        logln(&format!(
            "!! iperf3 前置检查未通过：{}: {}",
            block.reason_code, block.reason_detail
        ));
        logln(&format!(
            "!! {} 个 iperf3 单元将标记为 SETUP_ERROR，且不会启动进程。",
            blocked_units.len()
        ));
    }
    if let Some(block) = &cts_preflight {
        let blocked_units: Vec<&Unit> = units
            .iter()
            .filter(|unit| unit_has_ctstraffic(unit))
            .collect();
        for unit in &blocked_units {
            preflight_blocks.insert(unit.id.clone(), block.clone());
        }
        logln(&format!(
            "!! ctsTraffic 前置检查未通过：{}: {}",
            block.reason_code, block.reason_detail
        ));
        logln(&format!(
            "!! {} 个 CTS 单元将标记为 SETUP_ERROR；可用的 iperf3/Ping 单元继续执行。",
            blocked_units.len()
        ));
    }
    if !opts.auto {
        let c = ask(&format!(
            "已选 {} 个任务，确认执行? (回车=开始, n=取消): ",
            units.len()
        ));
        if c.eq_ignore_ascii_case("n") {
            logln("已取消。");
            return 1;
        }
    }

    // ---- 执行 ----
    let outdir = PathBuf::from("iperf_outputs");
    let _ = std::fs::create_dir_all(&outdir);
    let started = now_full();
    let t0 = Instant::now();
    let ctx = Ctx {
        agent_host: agent_host.clone(),
        agent_port: cfg.agent_port,
        cfg: cfg.clone(),
        outdir,
        local_servers: IperfServerMgr::new(),
        local_cts_jobs: crate::cmd::iperf::IperfClientJobMgr::new(),
        local_monitors: MonitorMgr::new(),
        rows: Mutex::new(Vec::new()),
        db: Mutex::new(ResultDb::load(PathBuf::from("task_results.json"))),
    };
    let mut sum = ctx.run_all_with_preflight_blocks(&units, &preflight_blocks);
    if sum.needs_traffic_failure_diagnostics() {
        let diagnostics = builder::build_traffic_failure_diagnostics(&units);
        logln(&format!(
            "\n!! 本轮 {} 个灌包单元没有产生任何有效速率测量（其中 SETUP_ERROR={}）。",
            sum.traffic_units, sum.traffic_setup_errors
        ));
        if diagnostics.is_empty() {
            logln("!! 无法从所选灌包任务提取可用 IPv4 端点，未生成自动 Ping 诊断。");
        } else {
            logln(&format!(
                "开始追加 {} 个故障诊断：每个唯一 IPv4 方向使用 32 字节短 Ping，并对涉及网卡绑定源地址 Ping IPv4 网关。",
                diagnostics.len()
            ));
            for (index, unit) in diagnostics.iter().enumerate() {
                logln(&format!("  [诊断 {}] {}", index + 1, unit.title));
            }
            let diagnostic_summary = ctx.run_all_from(&diagnostics, units.len());
            sum.merge(diagnostic_summary);
        }
    }
    let mut final_cleanup_errors = ctx.local_servers.stop_all();
    final_cleanup_errors.extend(ctx.local_cts_jobs.stop_all(Duration::from_secs(10)).errors);
    for error in &final_cleanup_errors {
        logln(&format!("!! 主控最终资源清理未确认: {error}"));
    }

    // ---- 报告 ----
    let elapsed_s = t0.elapsed().as_secs();
    let elapsed = format!(
        "{}:{:02}:{:02}",
        elapsed_s / 3600,
        elapsed_s % 3600 / 60,
        elapsed_s % 60
    );
    let meta = ReportMeta {
        master_pc: master_info.hostname.clone(),
        agent_pc: agent_info.hostname.clone(),
        agent_host: agent_host.clone(),
        started,
        finished: now_full(),
        elapsed: elapsed.clone(),
    };
    let report_path = PathBuf::from(format!("report_{}.html", now_compact()));
    {
        let mut rows = ctx.rows.lock().unwrap();
        match write_report(&report_path, &mut rows, &meta) {
            Ok(_) => logln(&format!("\n报告已生成: {}", report_path.display())),
            Err(e) => logln(&format!("!! 报告写入失败: {e}")),
        }
    }

    logln(&format!(
        "\n========== 全部完成 ==========\n单元总数: {}  PASS: {}  FAIL: {}  UNSTABLE: {}  MEASURED: {}  NOT_EVALUATED: {}  SETUP_ERROR: {}  跳过: {}  最终清理错误: {}  耗时: {}",
        sum.pass + sum.fail + sum.measured + sum.skip,
        sum.pass,
        sum.fail,
        sum.unstable,
        sum.measured,
        sum.not_evaluated,
        sum.setup_error,
        sum.skip,
        final_cleanup_errors.len(),
        elapsed
    ));
    if cfg.open_report && report_path.exists() {
        open_path(&report_path);
    }
    if sum.fail > 0 || !final_cleanup_errors.is_empty() {
        1
    } else {
        0
    }
}

fn unit_has_iperf(unit: &Unit) -> bool {
    unit.legs.iter().any(|leg| {
        matches!(
            &leg.kind,
            LegKind::IperfSingle(_) | LegKind::IperfGroup { .. }
        )
    })
}

fn unit_has_ctstraffic(unit: &Unit) -> bool {
    unit.legs
        .iter()
        .any(|leg| matches!(&leg.kind, LegKind::CtsTraffic(_)))
}

fn units_have_iperf(units: &[Unit]) -> bool {
    units.iter().any(unit_has_iperf)
}

fn iperf_preflight_block(
    units: &[Unit],
    health: &HealthOut,
    local_iperf_available: bool,
) -> Option<IperfPreflightBlock> {
    if !units_have_iperf(units) {
        return None;
    }

    let mut reasons = Vec::new();
    if !agent_supports_reliable_lifecycle(health) {
        let version = if health.version.trim().is_empty() {
            "未知版本"
        } else {
            health.version.as_str()
        };
        reasons.push(format!(
            "辅测机 agent {version} 缺少可靠灌包生命周期能力 {RELIABLE_LIFECYCLE_CAPABILITY}，请替换为与主控配套的新版 agent"
        ));
    }
    if !local_iperf_available {
        reasons.push("主控机未找到 iperf3，请安装或将 iperf3 放到主控程序同目录".into());
    }
    if health.iperf3.is_none() {
        reasons
            .push("辅测机未找到 iperf3，请安装或将 iperf3.exe 及配套 DLL 放到 agent 同目录".into());
    }

    (!reasons.is_empty()).then(|| IperfPreflightBlock {
        reason_code: IPERF_PREFLIGHT_FAILED.into(),
        reason_detail: reasons.join("；"),
    })
}

fn agent_supports_reliable_lifecycle(health: &HealthOut) -> bool {
    health
        .capabilities
        .iter()
        .any(|capability| capability == RELIABLE_LIFECYCLE_CAPABILITY)
}

fn agent_supports_ctstraffic(health: &HealthOut) -> bool {
    health
        .capabilities
        .iter()
        .any(|capability| capability == CTS_TRAFFIC_CAPABILITY)
}

fn ctstraffic_preflight_block(
    units: &[Unit],
    health: &HealthOut,
    local_cts_available: bool,
    local_platform_supported: bool,
) -> Option<IperfPreflightBlock> {
    if !units.iter().any(unit_has_ctstraffic) {
        return None;
    }
    let mut reasons = Vec::new();
    if !local_platform_supported {
        reasons.push(
            "主控平台不是 Windows 10+ 或系统版本无法确认；ctsTraffic 不支持旧版 Windows/macOS/Linux"
                .into(),
        );
    }
    if !health.os.eq_ignore_ascii_case("windows") {
        reasons.push(format!(
            "辅测机平台为 {}；ctsTraffic 仅支持 Windows 10+",
            if health.os.trim().is_empty() {
                "未知"
            } else {
                &health.os
            }
        ));
    }
    if !agent_supports_ctstraffic(health) {
        reasons.push(format!(
            "辅测机 agent 未声明 CTS 生命周期能力 {CTS_TRAFFIC_CAPABILITY}；请确认辅测机为 Windows 10+，并将两端同时升级"
        ));
    }
    if local_platform_supported && !local_cts_available {
        reasons.push("主控机未找到 ctsTraffic.exe，请放到 cpe_test.exe 同目录或 PATH".into());
    }
    if agent_supports_ctstraffic(health) && health.ctstraffic.is_none() {
        reasons.push("辅测机未找到 ctsTraffic.exe，请放到 agent 同目录或 PATH".into());
    }
    (!reasons.is_empty()).then(|| IperfPreflightBlock {
        reason_code: CTS_PREFLIGHT_FAILED.into(),
        reason_detail: reasons.join("；"),
    })
}

// ---------------- agent 通讯（连接阶段） ----------------

fn agent_health(host: &str, port: u16) -> Result<HealthOut, String> {
    let (st, body) = http_client::get(host, port, "/health", Duration::from_secs(10))?;
    if st != 200 {
        return Err(format!("HTTP {st}"));
    }
    let r: Resp<HealthOut> =
        serde_json::from_str(&body).map_err(|e| format!("响应解析失败: {e}"))?;
    r.data.ok_or_else(|| "响应缺 data".into())
}

fn agent_info(host: &str, port: u16, prefixes: &[String]) -> Result<HostInfo, String> {
    let req = InfoReq {
        ipv4_prefixes: prefixes.to_vec(),
    };
    let body = serde_json::to_string(&req).unwrap_or_default();
    let (st, text) = http_client::post_json(host, port, "/info", &body, Duration::from_secs(60))?;
    if st != 200 {
        return Err(format!("HTTP {st}"));
    }
    let r: Resp<HostInfo> =
        serde_json::from_str(&text).map_err(|e| format!("响应解析失败: {e}"))?;
    if !r.ok {
        return Err(r.error.unwrap_or_else(|| "未知错误".into()));
    }
    r.data.ok_or_else(|| "响应缺 data".into())
}

// ---------------- 交互式任务构建 ----------------

fn interactive_build_specs(cfg: &Config, master: &HostInfo, agent: &HostInfo) -> Vec<SpecNorm> {
    let mode = choose_single(
        "选择配置方式:",
        &[
            "全部任务勾选(自动生成全部任务，勾选要测的，推荐)".into(),
            "批量模式(按角色配对，统一参数)".into(),
            "精细模式(逐对选源/目标，可逐对不同参数)".into(),
        ],
        0,
    );
    match mode {
        0 | 1 => {
            let pairs = enumerate_pairs(master, agent);
            if pairs.is_empty() {
                logln("!! 没有可配对的网口");
                return vec![];
            }
            logln("\n可测试的网口配对:");
            for (i, (a, b, desc)) in pairs.iter().enumerate() {
                logln(&format!(
                    "  [{}] {}: {} <-> {}",
                    i + 1,
                    desc,
                    a.brief(),
                    b.brief()
                ));
            }
            let sel = loop {
                let inp = ask("选择要测的配对(逗号/连字符, 回车=全部): ");
                match parse_selection(&inp, pairs.len()) {
                    Ok(s) => break s,
                    Err(e) => logln(&format!("!! {e}")),
                }
            };
            let params = ask_universal_params(cfg, mode);
            sel.iter()
                .map(|i| {
                    let (a, b, desc) = &pairs[*i - 1];
                    spec_from_params(desc, a.clone(), b.clone(), &params, cfg)
                })
                .collect()
        }
        _ => {
            // 精细模式
            let mut eps: Vec<Endpoint> = Vec::new();
            for n in &master.interfaces {
                eps.push(Endpoint {
                    side: Side::Master,
                    pc: master.hostname.clone(),
                    nic: n.clone(),
                });
            }
            for n in &agent.interfaces {
                eps.push(Endpoint {
                    side: Side::Agent,
                    pc: agent.hostname.clone(),
                    nic: n.clone(),
                });
            }
            let mut specs = Vec::new();
            loop {
                logln("\n可用网口:");
                for (i, e) in eps.iter().enumerate() {
                    logln(&format!("  [{}] {} {}", i + 1, e.side.cn(), e.nic.brief()));
                }
                let src = pick_one("选择源网口序号: ", &eps);
                let Some(src) = src else { break };
                let dst = pick_one("选择目标网口序号: ", &eps);
                let Some(dst) = dst else { break };
                if src.key() == dst.key() {
                    logln("!! 源和目标不能是同一个网口");
                    continue;
                }
                let params = ask_universal_params(cfg, 2);
                specs.push(spec_from_params(
                    &format!("{}->{}", src.nic.name, dst.nic.name),
                    src,
                    dst,
                    &params,
                    cfg,
                ));
                let more = ask("继续添加测试对? (y=继续, 回车=完成): ");
                if !more.eq_ignore_ascii_case("y") {
                    break;
                }
            }
            specs
        }
    }
}

/// 配对枚举：跨机同角色 + 主控同机两两 + 辅测同机两两
fn enumerate_pairs(master: &HostInfo, agent: &HostInfo) -> Vec<(Endpoint, Endpoint, String)> {
    let mut out = Vec::new();
    let mep = |n: &crate::protocol::NicInfo| Endpoint {
        side: Side::Master,
        pc: master.hostname.clone(),
        nic: n.clone(),
    };
    let aep = |n: &crate::protocol::NicInfo| Endpoint {
        side: Side::Agent,
        pc: agent.hostname.clone(),
        nic: n.clone(),
    };
    // 跨机全部组合（不限同角色）
    for m in &master.interfaces {
        for a in &agent.interfaces {
            if m.role == "UNKNOWN" && a.role == "UNKNOWN" {
                continue;
            }
            out.push((mep(m), aep(a), format!("跨机 {}<->{}", m.role, a.role)));
        }
    }
    // 主控同机
    for i in 0..master.interfaces.len() {
        for j in (i + 1)..master.interfaces.len() {
            let (a, b) = (&master.interfaces[i], &master.interfaces[j]);
            out.push((mep(a), mep(b), format!("主控同机 {}<->{}", a.role, b.role)));
        }
    }
    // 辅测同机
    for i in 0..agent.interfaces.len() {
        for j in (i + 1)..agent.interfaces.len() {
            let (a, b) = (&agent.interfaces[i], &agent.interfaces[j]);
            out.push((aep(a), aep(b), format!("辅测同机 {}<->{}", a.role, b.role)));
        }
    }
    out
}

/// 从 config.json 的 pairs 字段自动生成测试规格
fn generate_specs_from_pairs(
    pairs: &crate::config::Pairs,
    cfg: &Config,
    master: &HostInfo,
    agent: &HostInfo,
) -> Vec<SpecNorm> {
    use crate::config::PairSpec as Ps;
    let pair_list: Vec<Ps> = match pairs {
        crate::config::Pairs::All(_) => {
            // 枚举全部跨机配对
            let mut v = Vec::new();
            for m in &master.interfaces {
                for a in &agent.interfaces {
                    if m.role == "UNKNOWN" && a.role == "UNKNOWN" {
                        continue;
                    }
                    v.push(Ps {
                        master: format!("NAME={}", m.name),
                        agent: format!("NAME={}", a.name),
                    });
                }
            }
            v
        }
        crate::config::Pairs::List(list) => list.clone(),
    };

    let default_params = cfg.universal_params.clone();
    let mut out = Vec::new();
    for p in &pair_list {
        let src = match builder::resolve_endpoint(&format!("master:{}", p.master), master, agent) {
            Ok(e) => e,
            Err(e) => {
                logln(&format!("!! 跳过配对 master:{}: {e}", p.master));
                continue;
            }
        };
        let dst = match builder::resolve_endpoint(&format!("agent:{}", p.agent), master, agent) {
            Ok(e) => e,
            Err(e) => {
                logln(&format!("!! 跳过配对 agent:{}: {e}", p.agent));
                continue;
            }
        };
        let directions = default_params
            .as_ref()
            .map(|p| p.directions.directions())
            .unwrap_or_else(|| vec!["ab".into()]);
        let kinds = default_params
            .as_ref()
            .map(|p| p.kinds.clone())
            .unwrap_or_else(|| vec!["iperf".into()]);
        let transports = default_params
            .as_ref()
            .and_then(|p| {
                if !p.transports.is_empty() {
                    Some(p.transports.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| vec!["tcp".into()]);
        let ipvers = default_params
            .as_ref()
            .map(|p| p.ip.clone())
            .unwrap_or_else(|| vec!["v4".into()]);
        let streams = default_params.as_ref().map(|p| p.streams).unwrap_or(1);
        let duration = default_params
            .as_ref()
            .and_then(|p| p.iperf_duration)
            .unwrap_or(cfg.iperf.duration);
        let ping_count = default_params
            .as_ref()
            .and_then(|p| p.ping_count)
            .unwrap_or(cfg.ping.count);
        let payload_sizes = default_params
            .as_ref()
            .and_then(|p| p.ping_payload_sizes.clone())
            .unwrap_or_else(|| cfg.ping.payload_sizes.clone());
        let tcp_windows = default_params
            .as_ref()
            .and_then(|p| p.tcp_windows.clone())
            .unwrap_or_else(|| cfg.iperf.tcp_windows.clone());
        let udp_profiles = default_params
            .as_ref()
            .and_then(|p| p.udp_profiles.clone())
            .unwrap_or_else(|| cfg.iperf.udp_profiles.clone());
        let rate_mode = default_params
            .as_ref()
            .and_then(|p| p.rate_mode)
            .unwrap_or(cfg.iperf.rate_check.mode);
        let rate_targets = default_params
            .as_ref()
            .and_then(|p| p.rate_targets_mbps.clone())
            .unwrap_or_default();

        out.push(SpecNorm {
            name: format!("{}<->{}", p.master, p.agent),
            src,
            dst,
            directions,
            kinds,
            transports,
            ipvers,
            streams: streams.clamp(1, 32),
            duration: duration.clamp(1, 86_400),
            ping_count,
            payload_sizes,
            tcp_windows,
            udp_profiles,
            udp_limit: cfg.limit_udp_by_link_speed,
            rate_mode,
            rate_targets,
            rate_check: cfg.iperf.rate_check.clone(),
            ctstraffic: cfg.ctstraffic.clone(),
            ctstraffic_config_error: builder::ctstraffic_config_error(streams, duration),
        });
    }
    out
}

struct UniversalParams {
    directions: Vec<String>,
    kinds: Vec<String>,
    transports: Vec<String>,
    ipvers: Vec<String>,
    streams: u32,
    duration: u64,
    ping_count: u32,
    payload_sizes: Vec<u32>,
    udp_limit: bool,
}

fn validate_traffic_scalar(label: &str, value: u64, min: u64, max: u64) -> Result<u64, String> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{label} 必须在 {min}..={max}，当前为 {value}"))
    }
}

fn ask_traffic_scalar(prompt: &str, default: u64, label: &str, min: u64, max: u64) -> u64 {
    loop {
        let value = ask_int(prompt, default);
        match validate_traffic_scalar(label, value, min, max) {
            Ok(value) => return value,
            Err(error) => logln(&format!(
                "!! {error}，请重新输入；CTS 自动化不会静默修正该值。"
            )),
        }
    }
}

fn ask_universal_params(cfg: &Config, mode: usize) -> UniversalParams {
    let dir_default: Vec<usize> = match mode {
        0 => vec![0, 1, 2],
        1 => vec![0, 1],
        _ => vec![0],
    };
    let dirs = choose_multi(
        "方向(可多选):",
        &[
            "A->B 单向".into(),
            "B->A 单向".into(),
            "双向并发(A<->B)".into(),
        ],
        &dir_default,
    );
    let directions: Vec<String> = dirs
        .iter()
        .map(|i| match i {
            0 => "ab".to_string(),
            1 => "ba".to_string(),
            _ => "bidir".to_string(),
        })
        .collect();

    let kind_sel = choose_single(
        "测试类型:",
        &[
            "灌包 iperf3（跨平台）".into(),
            "灌包 ctsTraffic（仅 Windows 10+）".into(),
            "iperf3 + ctsTraffic 对比".into(),
            "ping 连通".into(),
            "iperf3 + ctsTraffic + ping".into(),
        ],
        0,
    );
    let kinds: Vec<String> = match kind_sel {
        0 => vec!["iperf".into()],
        1 => vec!["ctstraffic".into()],
        2 => vec!["iperf".into(), "ctstraffic".into()],
        3 => vec!["ping".into()],
        _ => vec!["iperf".into(), "ctstraffic".into(), "ping".into()],
    };

    let transports: Vec<String> = if kinds
        .iter()
        .any(|kind| kind == "iperf" || kind == "ctstraffic")
    {
        match choose_single(
            "传输协议:",
            &["TCP+UDP".into(), "仅TCP".into(), "仅UDP".into()],
            0,
        ) {
            1 => vec!["tcp".into()],
            2 => vec!["udp".into()],
            _ => vec!["tcp".into(), "udp".into()],
        }
    } else {
        vec![]
    };

    let ipvers: Vec<String> = match choose_single(
        "IP 版本:",
        &["仅IPv4".into(), "仅IPv6".into(), "IPv4+IPv6".into()],
        0,
    ) {
        1 => vec!["v6".into()],
        2 => vec!["v4".into(), "v6".into()],
        _ => vec!["v4".into()],
    };

    let udp_limit = if transports.iter().any(|t| t == "udp") {
        choose_single(
            "UDP 按网口协商速率限制(协商不准如 4.2G 实际 10G 请关闭):",
            &["开启(默认)".into(), "关闭(不限速)".into()],
            0,
        ) == 0
    } else {
        cfg.limit_udp_by_link_speed
    };

    let has_traffic = kinds
        .iter()
        .any(|kind| kind == "iperf" || kind == "ctstraffic");
    let streams = if has_traffic {
        ask_traffic_scalar(
            "并发流数(iperf3 TCP=-P、UDP=多进程；CTS=-Connections，默认1): ",
            1,
            "并发流数",
            1,
            32,
        ) as u32
    } else {
        1
    };
    let duration = if has_traffic {
        ask_traffic_scalar(
            &format!("灌包时长秒(默认{}): ", cfg.iperf.duration),
            cfg.iperf.duration,
            "灌包时长",
            1,
            86_400,
        )
    } else {
        cfg.iperf.duration
    };
    let (ping_count, payload_sizes) = if kinds.iter().any(|k| k == "ping") {
        let c = ask_int(
            &format!("ping 包数(默认{}): ", cfg.ping.count),
            cfg.ping.count as u64,
        )
        .clamp(1, 100_000) as u32;
        let p = ask_ints_csv(
            &format!(
                "ping -l 负载字节数逗号分隔(默认{}): ",
                cfg.ping
                    .payload_sizes
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            &cfg.ping.payload_sizes,
        );
        (c, p)
    } else {
        (cfg.ping.count, cfg.ping.payload_sizes.clone())
    };

    UniversalParams {
        directions,
        kinds,
        transports,
        ipvers,
        streams,
        duration,
        ping_count,
        payload_sizes,
        udp_limit,
    }
}

fn spec_from_params(
    name: &str,
    src: Endpoint,
    dst: Endpoint,
    p: &UniversalParams,
    cfg: &Config,
) -> SpecNorm {
    SpecNorm {
        name: name.to_string(),
        src,
        dst,
        directions: p.directions.clone(),
        kinds: p.kinds.clone(),
        transports: p.transports.clone(),
        ipvers: p.ipvers.clone(),
        streams: p.streams.clamp(1, 32),
        duration: p.duration.clamp(1, 86_400),
        ping_count: p.ping_count,
        payload_sizes: p.payload_sizes.clone(),
        tcp_windows: cfg.iperf.tcp_windows.clone(),
        udp_profiles: cfg.iperf.udp_profiles.clone(),
        udp_limit: p.udp_limit,
        rate_mode: cfg.iperf.rate_check.mode,
        rate_targets: cfg.iperf.rate_check.targets_mbps.clone(),
        rate_check: cfg.iperf.rate_check.clone(),
        ctstraffic: cfg.ctstraffic.clone(),
        ctstraffic_config_error: builder::ctstraffic_config_error(p.streams, p.duration),
    }
}

fn pick_one(prompt: &str, eps: &[Endpoint]) -> Option<Endpoint> {
    loop {
        let inp = ask(prompt);
        if inp.is_empty() {
            return None;
        }
        match inp.parse::<usize>() {
            Ok(i) if i >= 1 && i <= eps.len() => return Some(eps[i - 1].clone()),
            _ => logln(&format!("!! 请输入 1-{}", eps.len())),
        }
    }
}

// ---------------- 菜单小工具 ----------------

fn choose_single(title: &str, options: &[String], default: usize) -> usize {
    logln(&format!("\n{title}"));
    for (i, o) in options.iter().enumerate() {
        logln(&format!(
            "  [{}] {}{}",
            i + 1,
            o,
            if i == default { " *" } else { "" }
        ));
    }
    loop {
        let inp = ask(&format!("选择(回车=默认{}): ", default + 1));
        if inp.is_empty() {
            return default;
        }
        match inp.parse::<usize>() {
            Ok(v) if v >= 1 && v <= options.len() => return v - 1,
            _ => logln(&format!("!! 请输入 1-{}", options.len())),
        }
    }
}

fn choose_multi(title: &str, options: &[String], defaults: &[usize]) -> Vec<usize> {
    logln(&format!("\n{title}"));
    for (i, o) in options.iter().enumerate() {
        logln(&format!(
            "  [{}] {}{}",
            i + 1,
            o,
            if defaults.contains(&i) { " *" } else { "" }
        ));
    }
    let def_str: Vec<String> = defaults.iter().map(|d| (d + 1).to_string()).collect();
    loop {
        let inp = ask(&format!(
            "多选(逗号分隔, 回车=默认[{}]): ",
            def_str.join(",")
        ));
        if inp.trim().is_empty() {
            return defaults.to_vec();
        }
        match parse_selection(&inp, options.len()) {
            Ok(v) => return v.iter().map(|i| i - 1).collect(),
            Err(e) => logln(&format!("!! {e}")),
        }
    }
}

fn ask_int(prompt: &str, default: u64) -> u64 {
    loop {
        let inp = ask(prompt);
        if inp.is_empty() {
            return default;
        }
        match inp.parse::<u64>() {
            Ok(v) => return v,
            Err(_) => logln("!! 请输入数字"),
        }
    }
}

fn ask_ints_csv(prompt: &str, default: &[u32]) -> Vec<u32> {
    loop {
        let inp = ask(prompt);
        if inp.trim().is_empty() {
            return default.to_vec();
        }
        let parsed: Result<Vec<u32>, _> = inp.split(',').map(|s| s.trim().parse::<u32>()).collect();
        match parsed {
            Ok(v) if !v.is_empty() => return v,
            _ => logln("!! 请输入逗号分隔的数字，如 32,1600,65500"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RateMode;
    use crate::master::builder::{CtsTrafficTask, IperfTask, Leg, PingPurpose, PingTask};
    use crate::protocol::NicInfo;

    fn endpoint(side: Side, name: &str) -> Endpoint {
        Endpoint {
            side,
            pc: side.cn().into(),
            nic: NicInfo {
                name: name.into(),
                ipv4: "127.0.0.1".into(),
                ..Default::default()
            },
        }
    }

    fn ping_unit() -> Unit {
        let master = endpoint(Side::Master, "master0");
        let agent = endpoint(Side::Agent, "agent0");
        Unit {
            id: "ping-only".into(),
            title: "ping-only".into(),
            bidir: false,
            est_secs: 1,
            legs: vec![Leg {
                tag: String::new(),
                kind: LegKind::Ping(PingTask {
                    v6: false,
                    src: master.clone(),
                    dst: agent.clone(),
                    count: 1,
                    payload: 32,
                    purpose: PingPurpose::SubnetTest,
                }),
            }],
        }
    }

    fn iperf_unit() -> Unit {
        let master = endpoint(Side::Master, "master0");
        let agent = endpoint(Side::Agent, "agent0");
        Unit {
            id: "iperf".into(),
            title: "iperf".into(),
            bidir: false,
            est_secs: 1,
            legs: vec![Leg {
                tag: String::new(),
                kind: LegKind::IperfSingle(IperfTask {
                    v6: false,
                    udp: true,
                    profile_name: "udp".into(),
                    profile_label: "UDP".into(),
                    src: master,
                    dst: agent,
                    port: 56_000,
                    duration: 1,
                    extra: Vec::new(),
                    stream_idx: 0,
                    rate_mode: RateMode::Auto,
                    rx_target_mbps: None,
                    offered_mbps: None,
                }),
            }],
        }
    }

    fn ctstraffic_unit() -> Unit {
        let master = endpoint(Side::Master, "master0");
        let agent = endpoint(Side::Agent, "agent0");
        Unit {
            id: "ctstraffic".into(),
            title: "ctstraffic".into(),
            bidir: false,
            est_secs: 1,
            legs: vec![Leg {
                tag: "ab".into(),
                kind: LegKind::CtsTraffic(CtsTrafficTask {
                    v6: false,
                    udp: true,
                    profile_name: "cts_udp_b500m_c3".into(),
                    profile_label: "CTS UDP -b 500m ×3流 (每流)".into(),
                    src: master,
                    dst: agent,
                    port: 56_001,
                    duration: 10,
                    streams: 3,
                    window_bytes: Some(1024 * 1024),
                    bits_per_second: Some(500_000_000),
                    datagram_bytes: Some(1200),
                    frame_rate: 100,
                    buffer_depth_secs: 1,
                    status_update_ms: 1_000,
                    rate_mode: RateMode::Observe,
                    rx_target_mbps: None,
                    offered_mbps: Some(1_500.0),
                    setup_error: None,
                }),
            }],
        }
    }

    fn ready_health() -> HealthOut {
        HealthOut {
            version: "4.0.0".into(),
            iperf3: Some("iperf 3.x".into()),
            capabilities: vec![RELIABLE_LIFECYCLE_CAPABILITY.into()],
            ..Default::default()
        }
    }

    fn ready_ctstraffic_health() -> HealthOut {
        HealthOut {
            os: "windows".into(),
            version: "4.2.0".into(),
            ctstraffic: Some("ctsTraffic 2.0.4.0".into()),
            capabilities: vec![CTS_TRAFFIC_CAPABILITY.into()],
            ..Default::default()
        }
    }

    #[test]
    fn interactive_traffic_scalars_are_rejected_instead_of_silently_clamped() {
        assert_eq!(validate_traffic_scalar("streams", 1, 1, 32), Ok(1));
        assert_eq!(validate_traffic_scalar("streams", 32, 1, 32), Ok(32));
        assert!(validate_traffic_scalar("streams", 0, 1, 32).is_err());
        assert!(validate_traffic_scalar("streams", 33, 1, 32).is_err());
        assert!(validate_traffic_scalar("duration", 0, 1, 86_400).is_err());
        assert!(validate_traffic_scalar("duration", 86_401, 1, 86_400).is_err());

        let params = UniversalParams {
            directions: vec!["ab".into()],
            kinds: vec!["ctstraffic".into()],
            transports: vec!["udp".into()],
            ipvers: vec!["v4".into()],
            streams: 0,
            duration: 0,
            ping_count: 1,
            payload_sizes: vec![32],
            udp_limit: true,
        };
        let cfg = Config::default();
        let spec = spec_from_params(
            "interactive-invalid",
            endpoint(Side::Master, "master0"),
            endpoint(Side::Agent, "agent0"),
            &params,
            &cfg,
        );
        assert_eq!(spec.streams, 1, "iperf execution remains safely normalized");
        assert_eq!(
            spec.duration, 1,
            "iperf execution remains safely normalized"
        );
        let error = spec.ctstraffic_config_error.unwrap();
        assert!(error.contains("streams 必须在 1..=32"));
        assert!(error.contains("duration 必须在 1..=86400"));
    }

    #[test]
    fn iperf_capability_gate_distinguishes_ping_only_and_old_agents() {
        let ping_only = ping_unit();
        let iperf = iperf_unit();
        assert!(!units_have_iperf(std::slice::from_ref(&ping_only)));
        assert!(units_have_iperf(&[ping_only, iperf]));

        let old_agent = HealthOut::default();
        assert!(!agent_supports_reliable_lifecycle(&old_agent));
        let current_agent = ready_health();
        assert!(agent_supports_reliable_lifecycle(&current_agent));
    }

    #[test]
    fn ping_only_is_never_blocked_by_iperf_preflight() {
        let decision = iperf_preflight_block(&[ping_unit()], &HealthOut::default(), false);
        assert_eq!(decision, None);
        let cts_decision =
            ctstraffic_preflight_block(&[ping_unit()], &HealthOut::default(), false, false);
        assert_eq!(cts_decision, None);
    }

    #[test]
    fn mixed_selection_collects_all_iperf_preflight_failures() {
        let health = HealthOut {
            version: "3.0.0".into(),
            ..Default::default()
        };
        let decision = iperf_preflight_block(&[ping_unit(), iperf_unit()], &health, false)
            .expect("混合任务中的 iperf 应被前置阻断");

        assert_eq!(decision.reason_code, IPERF_PREFLIGHT_FAILED);
        assert!(decision
            .reason_detail
            .contains(RELIABLE_LIFECYCLE_CAPABILITY));
        assert!(decision.reason_detail.contains("主控机未找到 iperf3"));
        assert!(decision.reason_detail.contains("辅测机未找到 iperf3"));
    }

    #[test]
    fn pure_iperf_preflight_failure_produces_an_explicit_block() {
        let decision = iperf_preflight_block(&[iperf_unit()], &HealthOut::default(), false)
            .expect("纯 iperf 也必须产生明确失败，不能被当成成功批次");
        assert_eq!(decision.reason_code, IPERF_PREFLIGHT_FAILED);
        assert!(!decision.reason_detail.is_empty());
    }

    #[test]
    fn ready_iperf_selection_has_no_preflight_block() {
        let decision = iperf_preflight_block(&[iperf_unit()], &ready_health(), true);
        assert_eq!(decision, None);
    }

    #[test]
    fn ready_ctstraffic_selection_has_no_preflight_block() {
        let decision = ctstraffic_preflight_block(
            &[ctstraffic_unit()],
            &ready_ctstraffic_health(),
            true,
            true,
        );
        assert_eq!(decision, None);
    }

    #[test]
    fn ctstraffic_preflight_rejects_old_agent_without_capability() {
        let mut health = ready_ctstraffic_health();
        health.version = "4.1.0".into();
        health.capabilities.clear();
        let decision = ctstraffic_preflight_block(&[ctstraffic_unit()], &health, true, true)
            .expect("旧 agent 必须被明确阻断");

        assert_eq!(decision.reason_code, CTS_PREFLIGHT_FAILED);
        assert!(decision.reason_detail.contains(CTS_TRAFFIC_CAPABILITY));
        assert!(decision.reason_detail.contains("两端同时升级"));
    }

    #[test]
    fn ctstraffic_preflight_rejects_non_windows_master_and_agent() {
        let health = HealthOut {
            os: "linux".into(),
            version: "4.2.0".into(),
            ctstraffic: Some("unexpected".into()),
            capabilities: vec![CTS_TRAFFIC_CAPABILITY.into()],
            ..Default::default()
        };
        let decision = ctstraffic_preflight_block(&[ctstraffic_unit()], &health, true, false)
            .expect("非 Windows 两端必须被明确阻断");

        assert_eq!(decision.reason_code, CTS_PREFLIGHT_FAILED);
        assert!(decision.reason_detail.contains("主控平台不是 Windows 10+"));
        assert!(decision.reason_detail.contains("辅测机平台为 linux"));
    }

    #[test]
    fn ctstraffic_preflight_reports_each_missing_binary_location() {
        let health = ready_ctstraffic_health();
        let local_missing =
            ctstraffic_preflight_block(&[ctstraffic_unit()], &health, false, true).unwrap();
        assert!(local_missing
            .reason_detail
            .contains("主控机未找到 ctsTraffic.exe"));
        assert!(!local_missing
            .reason_detail
            .contains("辅测机未找到 ctsTraffic.exe"));

        let mut agent_missing_health = health;
        agent_missing_health.ctstraffic = None;
        let agent_missing =
            ctstraffic_preflight_block(&[ctstraffic_unit()], &agent_missing_health, true, true)
                .unwrap();
        assert!(!agent_missing
            .reason_detail
            .contains("主控机未找到 ctsTraffic.exe"));
        assert!(agent_missing
            .reason_detail
            .contains("辅测机未找到 ctsTraffic.exe"));
    }

    #[test]
    fn mixed_backend_preflight_blocks_only_the_unavailable_ctstraffic_unit() {
        let units = vec![iperf_unit(), ctstraffic_unit(), ping_unit()];
        assert_eq!(
            iperf_preflight_block(&units, &ready_health(), true),
            None,
            "可用的 iperf3 后端不应被 CTS 状态连带阻断"
        );
        let cts_block = ctstraffic_preflight_block(&units, &ready_ctstraffic_health(), false, true)
            .expect("缺少主控 CTS binary 时只应阻断 CTS");
        assert_eq!(cts_block.reason_code, CTS_PREFLIGHT_FAILED);

        let blocked_ids: Vec<&str> = units
            .iter()
            .filter(|unit| unit_has_ctstraffic(unit))
            .map(|unit| unit.id.as_str())
            .collect();
        assert_eq!(blocked_ids, vec!["ctstraffic"]);
        assert!(unit_has_iperf(&units[0]));
        assert!(!unit_has_ctstraffic(&units[0]));
        assert!(!unit_has_iperf(&units[2]));
        assert!(!unit_has_ctstraffic(&units[2]));
    }
}
