//! 主控交互流程：连接辅测 -> 扫描 -> 菜单选任务 -> 执行 -> 报告
//! 设计目标：小白一路回车就能跑起来。

use crate::cmd::iperf::IperfServerMgr;
use crate::config::{load_config, Config};
use crate::http_client;
use crate::master::builder::{self, build_units, Endpoint, Side, SpecNorm};
use crate::master::executor::{Ctx, ResultDb};
use crate::nic::monitor::MonitorMgr;
use crate::nic::{format_nic_table, scan_host};
use crate::protocol::{HealthOut, HostInfo, InfoReq, Resp};
use crate::report::{write_report, ReportMeta};
use crate::util::{
    ask, find_iperf3, iperf3_version, log_to_file, logln, now_compact, now_full, open_path,
    parse_selection,
};
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
        "辅测机已连接: {} ({}) agent v{} iperf3: {}",
        health.hostname,
        health.os,
        health.version,
        health.iperf3.clone().unwrap_or_else(|| "未找到!".into())
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

    // ---- 生成测试规格 ----
    let specs: Vec<SpecNorm> = if !cfg.tests.is_empty() {
        let use_cfg = if opts.auto {
            true
        } else {
            let c = choose_single(
                "检测到配置文件里有 tests，选择配置方式:",
                &["按配置文件执行(推荐)", "交互式选择"],
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
    } else {
        if opts.auto {
            logln("!! --auto 模式需要配置文件里有 tests[]");
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
                    units = sel.into_iter().map(|i| units[i - 1].clone()).collect();
                    break;
                }
                Err(e) => logln(&format!("!! {e}")),
            }
        }
        let c = ask(&format!(
            "已选 {} 个任务，确认执行? (回车=开始, n=取消): ",
            units.len()
        ));
        if c.eq_ignore_ascii_case("n") {
            logln("已取消。");
            return 1;
        }
    } else {
        logln(&format!("\n[auto] 共 {} 个任务，直接开始执行", units.len()));
        for (i, u) in units.iter().enumerate() {
            logln(&format!("  [{}] {}", i + 1, u.title));
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
        local_monitors: MonitorMgr::new(),
        rows: Mutex::new(Vec::new()),
        db: Mutex::new(ResultDb::load(PathBuf::from("task_results.json"))),
    };
    let sum = ctx.run_all(&units);
    ctx.local_servers.stop_all();

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
        "\n========== 全部完成 ==========\n单元总数: {}  PASS: {}  FAIL: {}  跳过: {}  耗时: {}",
        sum.pass + sum.fail + sum.skip,
        sum.pass,
        sum.fail,
        sum.skip,
        elapsed
    ));
    if cfg.open_report && report_path.exists() {
        open_path(&report_path);
    }
    if sum.fail > 0 {
        1
    } else {
        0
    }
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
            "全部任务勾选(自动生成全部任务，勾选要测的，推荐)",
            "批量模式(按角色配对，统一参数)",
            "精细模式(逐对选源/目标，可逐对不同参数)",
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
            let eps: Vec<Endpoint> = master
                .interfaces
                .iter()
                .map(|n| endpoint(Side::Master, master, n))
                .chain(
                    agent
                        .interfaces
                        .iter()
                        .map(|n| endpoint(Side::Agent, agent, n)),
                )
                .collect();
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
    let mep = |n| endpoint(Side::Master, master, n);
    let aep = |n| endpoint(Side::Agent, agent, n);
    // 跨机全部组合（不限同角色）
    for m in &master.interfaces {
        for a in &agent.interfaces {
            if m.role == "UNKNOWN" && a.role == "UNKNOWN" {
                continue;
            }
            out.push((mep(m), aep(a), format!("跨机 {}<->{}", m.role, a.role)));
        }
    }
    push_local_pairs(&mut out, master, Side::Master, "主控");
    push_local_pairs(&mut out, agent, Side::Agent, "辅测");
    out
}

fn endpoint(side: Side, host: &HostInfo, nic: &crate::protocol::NicInfo) -> Endpoint {
    Endpoint {
        side,
        pc: host.hostname.clone(),
        nic: nic.clone(),
    }
}

fn push_local_pairs(
    out: &mut Vec<(Endpoint, Endpoint, String)>,
    host: &HostInfo,
    side: Side,
    side_name: &str,
) {
    for i in 0..host.interfaces.len() {
        for b in &host.interfaces[i + 1..] {
            let a = &host.interfaces[i];
            out.push((
                endpoint(side, host, a),
                endpoint(side, host, b),
                format!("{side_name}同机 {}<->{}", a.role, b.role),
            ));
        }
    }
}

/// 从 config.json 的 pairs 字段自动生成测试规格
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

fn ask_universal_params(cfg: &Config, mode: usize) -> UniversalParams {
    let dir_default: &[usize] = match mode {
        0 => &[0, 1, 2],
        1 => &[0, 1],
        _ => &[0],
    };
    let dirs = choose_multi(
        "方向(可多选):",
        &["A->B 单向", "B->A 单向", "双向并发(A<->B)"],
        dir_default,
    );
    let directions: Vec<String> = dirs
        .iter()
        .map(|i| match i {
            0 => "ab".to_string(),
            1 => "ba".to_string(),
            _ => "bidir".to_string(),
        })
        .collect();

    let kind_sel = choose_single("测试类型:", &["灌包 iperf3", "ping 连通", "灌包+ping"], 0);
    let kinds: Vec<String> = match kind_sel {
        0 => vec!["iperf".into()],
        1 => vec!["ping".into()],
        _ => vec!["iperf".into(), "ping".into()],
    };

    let transports: Vec<String> = if kinds.iter().any(|k| k == "iperf") {
        match choose_single("传输协议:", &["TCP+UDP", "仅TCP", "仅UDP"], 0) {
            1 => vec!["tcp".into()],
            2 => vec!["udp".into()],
            _ => vec!["tcp".into(), "udp".into()],
        }
    } else {
        vec![]
    };

    let ipvers: Vec<String> = match choose_single("IP 版本:", &["仅IPv4", "仅IPv6", "IPv4+IPv6"], 0)
    {
        1 => vec!["v6".into()],
        2 => vec!["v4".into(), "v6".into()],
        _ => vec!["v4".into()],
    };

    let udp_limit = if transports.iter().any(|t| t == "udp") {
        choose_single(
            "UDP 按网口协商速率限制(协商不准如 4.2G 实际 10G 请关闭):",
            &["开启(默认)", "关闭(不限速)"],
            0,
        ) == 0
    } else {
        cfg.limit_udp_by_link_speed
    };

    let streams = if kinds.iter().any(|k| k == "iperf") {
        ask_int("并发流数(TCP用-P, UDP为多进程并发, 默认1): ", 1).clamp(1, 32) as u32
    } else {
        1
    };
    let duration = if kinds.iter().any(|k| k == "iperf") {
        ask_int(
            &format!("灌包时长秒(默认{}): ", cfg.iperf.duration),
            cfg.iperf.duration,
        )
        .clamp(1, 86400)
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
        streams: p.streams,
        duration: p.duration,
        ping_count: p.ping_count,
        payload_sizes: p.payload_sizes.clone(),
        tcp_windows: cfg.iperf.tcp_windows.clone(),
        udp_profiles: cfg.iperf.udp_profiles.clone(),
        udp_limit: p.udp_limit,
    }
}

fn pick_one(prompt: &str, eps: &[Endpoint]) -> Option<Endpoint> {
    ask_index(prompt, eps.len(), None).map(|i| eps[i].clone())
}

// ---------------- 菜单小工具 ----------------

fn choose_single(title: &str, options: &[&str], default: usize) -> usize {
    logln(&format!("\n{title}"));
    for (i, o) in options.iter().enumerate() {
        logln(&format!(
            "  [{}] {}{}",
            i + 1,
            o,
            if i == default { " *" } else { "" }
        ));
    }
    ask_index(
        &format!("选择(回车=默认{}): ", default + 1),
        options.len(),
        Some(default),
    )
    .expect("default index")
}

fn choose_multi(title: &str, options: &[&str], defaults: &[usize]) -> Vec<usize> {
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

fn ask_index(prompt: &str, max: usize, default: Option<usize>) -> Option<usize> {
    loop {
        let input = ask(prompt);
        if input.is_empty() {
            return default;
        }
        match input.parse::<usize>() {
            Ok(value) if (1..=max).contains(&value) => return Some(value - 1),
            _ => logln(&format!("!! 请输入 1-{max}")),
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
            _ => logln("!! 请输入逗号分隔的数字，如 32,1400"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::NicInfo;

    fn host(name: &str, roles: &[&str]) -> HostInfo {
        HostInfo {
            hostname: name.into(),
            interfaces: roles
                .iter()
                .enumerate()
                .map(|(i, role)| NicInfo {
                    name: format!("nic{i}"),
                    role: (*role).into(),
                    ipv4: format!("192.168.1.{}", i + 1),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn pair_enumeration_keeps_cross_then_local_order() {
        let pairs = enumerate_pairs(
            &host("master", &["UNKNOWN", "SGMII1G"]),
            &host("agent", &["UNKNOWN", "WIFI5G"]),
        );
        let descriptions: Vec<_> = pairs.iter().map(|p| p.2.as_str()).collect();
        assert_eq!(
            descriptions,
            [
                "跨机 UNKNOWN<->WIFI5G",
                "跨机 SGMII1G<->UNKNOWN",
                "跨机 SGMII1G<->WIFI5G",
                "主控同机 UNKNOWN<->SGMII1G",
                "辅测同机 UNKNOWN<->WIFI5G",
            ]
        );
    }
}
