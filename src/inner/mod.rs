//! ADB 内环测速：发送端运行普通 client，接收端运行 server，上下行均不使用反向参数。
//!
//! 分层：[`config`] 定义配置与迁移，[`plan`] 把配置展开成**唯一一份**执行
//! 计划，[`adb`] 适配板侧设备与采样，[`measure`] 是纯粹的来源选择与判定，
//! 本模块只负责按计划把流打起来、把数据采下来，再交给 [`report`] 输出。
//! 判什么、按谁判一律不在这里决定。
mod adb;
mod adb_client;
pub(crate) mod config;
mod history;
mod measure;
pub(crate) mod plan;
mod receiver_server;
mod remote;
mod report;
pub(crate) mod webui;

use crate::clock::SystemClock;
use crate::cmd::iperf;
use crate::master::rate_window::{
    evaluate_rx_acceptance, monitor_rate_stats, EffectiveWindow, RateStats,
};
use crate::nic::counter::{NicCounterReader, SystemNicCounterReader};
use crate::nic::monitor::MonitorMgr;
use crate::protocol::{
    IperfClientOut, IperfClientReq, IperfFlowEvent, MonitorStartOut, MonitorStartReq,
    MonitorStopOut, MonitorStopReq,
};
use adb::{Adb, BoardCounters, BoardInterface, CounterSource, Server};
use config::{Direction, Flow, InnerConfig, Link, Measurement, Protocol};
use measure::{LegMeasurement, NicView, Source, ToolOrigin, ToolView};
use plan::{LegPlan, Unit};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// 独立根目录：子网控制台会枚举 runs 下所有目录，不能只换目录名前缀。
const RUNS_ROOT: &str = "inner_runs";
/// 起流前的静默期，用来量背景流量。
const BASELINE_MS: u64 = 2500;

// ---------------- 能力与设备 ----------------

/// 一台电脑在本轮里的处境。**未参与**和**连接失败**必须分开：
/// 上一次测过、这次拔掉的辅测机只要没被本轮引用，就不该拦住本机测试。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    Ready,
    Failed,
    NotParticipating,
}

#[derive(Serialize)]
struct AgentCapability {
    id: String,
    status: HostStatus,
    /// 连接失败时的原因。配置本身不会因为这次失败被删改。
    error: Option<String>,
    info: Option<crate::protocol::HostInfo>,
}

#[derive(Serialize)]
struct Capability {
    serial: String,
    board_version: String,
    board_addresses: String,
    board_counters: String,
    /// 板侧接口清单：地址、桥成员关系、两条读取路径各自是否可用。
    board_interfaces: Vec<BoardInterface>,
    /// 清单读取失败的原因。清单缺失只降级不致命，但降级的理由必须留痕——
    /// 否则后面「读不到字节计数」的结论会把人指向错误的方向。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    board_inventory_error: Option<String>,
    local: crate::protocol::HostInfo,
    agents: Vec<AgentCapability>,
}

impl Capability {
    fn host(&self, id: &str) -> Result<&crate::protocol::HostInfo, String> {
        if id == "master" {
            return Ok(&self.local);
        }
        let agent = self
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .ok_or_else(|| format!("未找到内环辅测机 {id}"))?;
        agent.info.as_ref().ok_or_else(|| {
            format!(
                "辅测机 {id} 未就绪：{}",
                agent.error.clone().unwrap_or_else(|| "未连接".into())
            )
        })
    }
}

// ---------------- 结果模型 ----------------

/// 接收端网卡 RX 的分布，用来区分「平均达标」和「全程稳定」。
#[derive(Serialize, Default)]
struct RxDistribution {
    p10_mbps: Option<f64>,
    median_mbps: Option<f64>,
    p95_mbps: Option<f64>,
    min_mbps: Option<f64>,
    max_mbps: Option<f64>,
    rolling_coverage: f64,
    stalled_ratio: f64,
}

impl From<&RateStats> for RxDistribution {
    fn from(stats: &RateStats) -> Self {
        Self {
            p10_mbps: stats.p10_mbps,
            median_mbps: stats.median_mbps,
            p95_mbps: stats.p95_mbps,
            min_mbps: stats.min_mbps,
            max_mbps: stats.max_mbps,
            rolling_coverage: stats.rolling_coverage,
            stalled_ratio: stats.stalled_ratio,
        }
    }
}

/// iperf3 自报的口径。`receiver_mbps` 是**严格**的 receiver 汇总，可以参与
/// 判定；`sender_mbps` 和 `receiver_note` 只作诊断。
#[derive(Serialize, Default)]
struct ToolReport {
    sender_mbps: Option<f64>,
    receiver_mbps: Option<f64>,
    /// 拿不到严格 receiver 汇总时的原因，或它取自哪份原文。
    receiver_note: String,
    udp_loss_pct: Option<f64>,
    udp_lost_datagrams: Option<u64>,
    udp_total_datagrams: Option<u64>,
}

#[derive(Serialize)]
struct LegRow {
    flow: Flow,
    port: u16,
    /// 接收端接口名。
    receiver: String,
    /// 接收端在哪一侧：`板侧` 或电脑标识。
    receiver_host: String,
    /// 板侧计数走哪条读取路径；PC 侧为 None。
    counter_source: Option<CounterSource>,
    /// 本腿最终采用的速率来源。
    source: Source,
    /// 采用来源给出的接收速率。
    mbps: Option<f64>,
    target_mbps: Option<f64>,
    /// 只有真的发生兜底才有值：原来源为什么不可信。
    fallback_reason: Option<String>,
    verdict: String,
    reason: String,
    detail: String,
    diagnostics: Vec<String>,
    /// 网卡口径**始终**单独保留一份，字段语义与子网完全一致。
    /// 兜底用了工具口径也不会改写它，更不会往里填造出来的样本。
    nic_rx_mbps: Option<f64>,
    nic_verdict: String,
    nic_reason: String,
    nic_target_mbps: Option<f64>,
    background_mbps: f64,
    coverage: f64,
    effective_secs: f64,
    required_secs: u64,
    rx: RxDistribution,
    tool: ToolReport,
    client: IperfClientOut,
    /// 接收端 iperf3 server 的原始输出。UDP 的 lost/total 汇总先落在这里，
    /// client 侧拿不到 server 报告时它是唯一的证据。
    server_log: String,
    rx_samples: Option<MonitorStopOut>,
}

#[derive(Serialize)]
struct UnitRow {
    /// 与计划同源的稳定身份，供后续内环 RESUME 匹配。
    id: String,
    index: usize,
    link: String,
    host: String,
    protocol: Protocol,
    direction: Direction,
    streams: u32,
    repeat: u32,
    measurement: Measurement,
    verdict: String,
    #[serde(default)]
    resumed: bool,
    reason: String,
    detail: String,
    diagnostics: Vec<String>,
    /// 双向合计（同来源两端接收速率之和）；单向单元为 None。
    total_mbps: Option<f64>,
    total_target_mbps: Option<f64>,
    /// 双向两条腿的共同有效重叠秒数。没有可证明的重叠就不是并发。
    overlap_secs: Option<f64>,
    legs: Vec<LegRow>,
}

#[derive(Serialize)]
struct RunReport {
    schema_version: u32,
    current: String,
    created_at: String,
    config: InnerConfig,
    /// 本轮执行的计划预览，和页面看到的是同一份。
    plan: Option<plan::Preview>,
    probe_only: bool,
    capability: Option<Capability>,
    units: Vec<UnitRow>,
    error: Option<String>,
}

// ---------------- CLI ----------------

fn cli_options(args: &[String]) -> Result<(PathBuf, bool, bool), String> {
    let mut path = None;
    let mut probe = false;
    let mut resume = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" if path.is_none() => {
                let value = iter
                    .next()
                    .filter(|v| !v.starts_with('-') && !v.trim().is_empty())
                    .ok_or("--config 后必须跟文件路径")?;
                path = Some(PathBuf::from(value));
            }
            "--probe" if !probe => probe = true,
            "--resume" if !resume => resume = true,
            _ => return Err(format!("未知或重复的内环参数: {arg}")),
        }
    }
    Ok((
        path.ok_or("用法: cpe_test inner --config inner.example.json [--probe] [--resume]")?,
        probe,
        resume,
    ))
}

pub fn run_cli(args: &[String]) -> i32 {
    let parsed = cli_options(args).and_then(|(path, probe, resume)| {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
        let mut cfg = config::parse_config(&text)?;
        cfg.resume |= resume;
        if !probe && plan::build(&cfg)?.units.is_empty() {
            return Err("本轮没有勾选任何链路；只探测请加 --probe".into());
        }
        Ok((cfg, probe))
    });
    let (cfg, probe) = match parsed {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    crate::cancel::setup_cancel_handler();
    match perform(cfg, probe, crate::cancel::cancel_flag(), &|_, _| {}) {
        Ok((report, dir)) => {
            println!("内环报告: {}", dir.join("report.html").display());
            if report.error.is_some() || crate::cancel::is_cancelled() {
                2
            } else if report
                .units
                .iter()
                .any(|unit| !matches!(unit.verdict.as_str(), "PASS" | "MEASURED"))
            {
                1
            } else {
                0
            }
        }
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}

// ---------------- 执行 ----------------

type Observer<'a> = &'a dyn Fn(&RunReport, &Path);

fn perform(
    cfg: InnerConfig,
    probe_only: bool,
    cancel: &AtomicBool,
    observer: Observer<'_>,
) -> Result<(RunReport, PathBuf), String> {
    let resumed = cfg.resume.then(history::fresh_pass_ids).unwrap_or_default();
    let preview = plan::preview_with_resumed(&cfg, &resumed)?;
    if !probe_only && preview.units == 0 {
        return Err("本轮没有勾选任何链路，请至少勾选一条再开始".into());
    }
    let dir = PathBuf::from(format!(
        "{RUNS_ROOT}/inner_{}_{}_{}",
        crate::util::now_compact(),
        std::process::id(),
        chrono::Utc::now().timestamp_subsec_nanos()
    ));
    std::fs::create_dir_all(RUNS_ROOT).map_err(|e| e.to_string())?;
    let root_metadata = std::fs::symlink_metadata(RUNS_ROOT).map_err(|e| e.to_string())?;
    if !root_metadata.file_type().is_dir() {
        return Err("内环历史根目录不是普通目录".into());
    }
    std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
    let mut report = RunReport {
        schema_version: 2,
        current: "正在检查 ADB 和本轮引用的电脑".into(),
        created_at: crate::util::now_full(),
        config: cfg,
        plan: Some(preview),
        probe_only,
        capability: None,
        units: Vec::new(),
        error: None,
    };
    observer(&report, &dir);
    if let Err(error) = execute(&mut report, &dir, cancel, observer, &resumed) {
        report.error = Some(error);
    }
    report.current.clear();
    if cancel.load(Ordering::SeqCst) && report.error.is_none() {
        report.error = Some("用户已取消内环测试".into());
    }
    report::save(&dir, &report)?;
    observer(&report, &dir);
    Ok((report, dir))
}

fn pause(duration: Duration, cancel: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline && !cancel.load(Ordering::SeqCst) {
        std::thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

/// 板侧与各电脑的能力探测。
///
/// `required` 为空表示这是页面上的一次设备扫描：所有配置的辅测机都试着连，
/// 连不上只记状态，不让整次扫描失败。执行前的探测传入本轮实际引用的电脑，
/// 未被引用的辅测机连碰都不碰。
fn probe(adb: &Adb, cfg: &InnerConfig, required: Option<&[String]>) -> Result<Capability, String> {
    let version = adb.shell(&format!("'{}' --version", cfg.board_iperf))?;
    let help = adb.shell(&format!("'{}' --help", cfg.board_iperf))?;
    if !help.contains("--client") || !help.contains("--server") || !help.contains("--bind") {
        return Err("板侧工具缺少 iperf3 server/client/bind 能力，请检查 board_iperf 路径".into());
    }
    let addresses = adb.shell("ip -o -4 addr show")?;
    let counters = adb.shell("cat /proc/net/dev").unwrap_or_default();
    // 接口清单是「有更好、没有也能跑」：老板子没有 sysfs 时仍可只靠
    // /proc/net/dev 工作，不能因为清单读不出来就整轮不给测。
    //
    // 但**失败的原因要留下来**。清单脚本要为每个接口 cat 一遍，接口多或 adbd
    // 慢时会撞上 shell 预算；吞掉之后清单为空，每个接口都被标成
    // `sysfs_counters: false`，于是一块「/proc/net/dev 不可用、sysfs 正常」的
    // 板子最后报的是「读不到字节计数」——把人支去查板子的计数器，真因却是这里
    // 一次没人说过的超时。
    let (inventory, inventory_error) = match adb.shell(adb::INTERFACE_INVENTORY_SCRIPT) {
        Ok(text) => (text, None),
        Err(error) => {
            eprintln!("板侧接口清单读取失败（改用 /proc/net/dev 与地址推断）: {error}");
            (String::new(), Some(error))
        }
    };
    let agents = cfg
        .agents
        .iter()
        .map(|agent| {
            let participating = required.is_none_or(|ids| ids.iter().any(|id| id == &agent.id));
            if !participating {
                return AgentCapability {
                    id: agent.id.clone(),
                    status: HostStatus::NotParticipating,
                    error: None,
                    info: None,
                };
            }
            match remote::Remote::new(agent.clone()).info() {
                Ok(info) => AgentCapability {
                    id: agent.id.clone(),
                    status: HostStatus::Ready,
                    error: None,
                    info: Some(info),
                },
                Err(error) => AgentCapability {
                    id: agent.id.clone(),
                    status: HostStatus::Failed,
                    error: Some(error),
                    info: None,
                },
            }
        })
        .collect();
    Ok(Capability {
        serial: adb.serial.clone(),
        board_version: version,
        board_interfaces: adb::board_interfaces(&inventory, &addresses, &counters),
        board_inventory_error: inventory_error,
        board_addresses: addresses,
        board_counters: counters,
        local: crate::nic::scan_host(&[]),
        agents,
    })
}

/// 一条链路的预检结论：板侧统计接口和它的读取路径。
#[derive(Debug)]
struct LinkPreflight {
    board_iface: String,
    counter_source: Option<CounterSource>,
}

fn preflight_link(link: &Link, capability: &Capability) -> Result<LinkPreflight, String> {
    let host = capability
        .host(&link.host)
        .map_err(|error| format!("{}: {error}", link.name))?;
    let found = host
        .interfaces
        .iter()
        .filter(|nic| nic.name == link.local_interface && nic.ipv4 == link.local_ip.to_string())
        .count();
    if found != 1 {
        return Err(format!(
            "{}: 电脑 {} 的接口 {} 未唯一匹配 IP {}，请先扫描核实",
            link.name, link.host, link.local_interface, link.local_ip
        ));
    }
    // LAN 地址仍然必须唯一归属板侧某个接口——它是上行目标及下行源地址。
    // 但统计接口不必是同一个。
    let gateway_iface = adb::address_interface(&capability.board_addresses, link.gateway)
        .map_err(|error| format!("{}: {error}", link.name))?;
    let resolved = adb::resolve_rx_interface(
        &link.board_rx_interface,
        &gateway_iface,
        &capability.board_interfaces,
    );
    let (board_iface, counter_source) = match resolved {
        Ok((iface, source)) => (iface, Some(source)),
        Err(error) if link.measurement == Measurement::NicStrict => {
            return Err(format!("{}: {error}", link.name));
        }
        Err(_) => (
            if link.board_rx_interface.is_empty() {
                gateway_iface
            } else {
                link.board_rx_interface.clone()
            },
            None,
        ),
    };
    if link.host == "master" && link.measurement == Measurement::NicStrict {
        crate::nic::monitor::read_counters(&link.local_interface)
            .map_err(|error| format!("{}: {error}", link.name))?;
    }
    Ok(LinkPreflight {
        board_iface,
        counter_source,
    })
}

fn execute(
    report: &mut RunReport,
    dir: &Path,
    cancel: &AtomicBool,
    observer: Observer<'_>,
    resumed: &std::collections::HashSet<String>,
) -> Result<(), String> {
    let mut built = plan::build(&report.config)?;
    // 全部单元都已有新鲜 PASS 时，RESUME 的语义就是「不再触碰测试设备」；
    // 但配置没填 serial 时必须先用 `adb devices` 确认实际设备身份，不能为了
    // 省这一步把另一块板的历史 PASS 复用过来。显式 serial 才能在这里直接跳过。
    if !report.probe_only
        && !report.config.serial.is_empty()
        && can_skip_before_device_identification(&report.config, &built.units, resumed)
    {
        let mut appended = 0usize;
        let mut last_heavy = None;
        for unit in &built.units {
            if cancel.load(Ordering::SeqCst) {
                return Err("用户已取消；已完成的结果保留".into());
            }
            report.current = format!("{}/{} · {}", unit.index, built.units.len(), unit.title());
            observer(report, dir);
            report.units.push(resumed_row(unit));
            report::save_progress(dir, report, &mut appended, &mut last_heavy)?;
            observer(report, dir);
        }
        return Ok(());
    }
    let adb = Adb::connect(&report.config)?;
    let mut effective_resumed = resumed.clone();
    if !report.probe_only && report.config.serial.is_empty() {
        // 空 serial 表示「当时只有一台设备」，不能拿配置里的空字符串当设备身份。
        // 现在已经完成 adb devices，实际序列号才可用于重建稳定计划和匹配历史。
        let mut execution_cfg = report.config.clone();
        execution_cfg.serial = adb.serial.clone();
        built = plan::build(&execution_cfg)?;
        effective_resumed = if execution_cfg.resume {
            history::fresh_pass_ids()
        } else {
            std::collections::HashSet::new()
        };
        report.plan = Some(plan::preview_with_resumed(
            &execution_cfg,
            &effective_resumed,
        )?);
    }
    let resumed = &effective_resumed;
    if !report.probe_only && all_units_resumed(&built.units, resumed) {
        let mut appended = 0usize;
        let mut last_heavy = None;
        for unit in &built.units {
            if cancel.load(Ordering::SeqCst) {
                return Err("用户已取消；已完成的结果保留".into());
            }
            report.current = format!("{}/{} · {}", unit.index, built.units.len(), unit.title());
            observer(report, dir);
            report.units.push(resumed_row(unit));
            report::save_progress(dir, report, &mut appended, &mut last_heavy)?;
            observer(report, dir);
        }
        return Ok(());
    }
    let needed_links: std::collections::HashSet<usize> = built
        .units
        .iter()
        .filter(|unit| !resumed.contains(&unit.id))
        .map(|unit| unit.link)
        .collect();
    let required_agents: Vec<String> = built
        .units
        .iter()
        .filter(|unit| needed_links.contains(&unit.link))
        .map(|unit| report.config.links[unit.link].host.clone())
        .filter(|host| host != "master")
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let required = (!report.probe_only).then_some(required_agents.as_slice());
    let capability = probe(&adb, &report.config, required)?;
    println!(
        "ADB 设备: {}\n板侧工具: {}\n板侧地址:\n{}",
        capability.serial,
        capability.board_version.trim(),
        capability.board_addresses.trim()
    );
    println!(
        "{}",
        crate::nic::format_nic_table("【本机】", &capability.local)
    );
    report.capability = Some(capability);
    report::save(dir, report)?;
    observer(report, dir);
    if report.probe_only {
        return Ok(());
    }
    let capability = report.capability.as_ref().unwrap();
    // 只有本轮引用的电脑才是门禁。没被引用的辅测机哪怕离线也不拦。
    let broken: Vec<String> = capability
        .agents
        .iter()
        .filter(|agent| agent.status == HostStatus::Failed)
        .map(|agent| {
            format!(
                "{}（{}）",
                agent.id,
                agent.error.clone().unwrap_or_default()
            )
        })
        .collect();
    if !broken.is_empty() {
        return Err(format!(
            "本轮引用的辅测机未就绪：{}；配置已保留，取消勾选相关链路即可只测其他网口",
            broken.join("；")
        ));
    }
    let bin = crate::cmd::tools::find_iperf3().unwrap_or_default();
    let needs_master = built
        .units
        .iter()
        .any(|unit| !resumed.contains(&unit.id) && report.config.links[unit.link].host == "master");
    if bin.is_empty() && needs_master {
        return Err("本机未找到 iperf3".into());
    }
    // 先把所有参与链路验完，防止跑了一半才发现后面的网卡填错。
    let preflight: std::collections::HashMap<usize, LinkPreflight> = needed_links
        .iter()
        .map(|&index| {
            Ok((
                index,
                preflight_link(&report.config.links[index], capability)?,
            ))
        })
        .collect::<Result<_, String>>()?;

    // 追加游标与重产物节流的时间戳跨单元保留：前者保证 units.jsonl 只写新增
    // 的那几条，后者让整份重写按时间而不是按单元数发生。
    let mut appended = 0usize;
    let mut last_heavy = None;
    for unit in &built.units {
        if cancel.load(Ordering::SeqCst) {
            return Err("用户已取消；已完成的结果保留".into());
        }
        report.current = format!("{}/{} · {}", unit.index, built.units.len(), unit.title());
        observer(report, dir);
        let link = &report.config.links[unit.link];
        let preflight = &preflight[&unit.link];
        if resumed.contains(&unit.id) {
            let row = resumed_row(unit);
            report.units.push(row);
            report::save_progress(dir, report, &mut appended, &mut last_heavy)?;
            observer(report, dir);
            continue;
        }
        println!("{} · 第 {} 轮", unit.title(), unit.repeat);
        let row = run_unit(
            &UnitContext {
                adb: &adb,
                cfg: &report.config,
                link,
                preflight,
                bin: &bin,
                agent: report
                    .config
                    .agents
                    .iter()
                    .find(|agent| agent.id == link.host)
                    .cloned(),
                dir,
                cancel,
            },
            unit,
        )?;
        for leg in &row.legs {
            println!(
                "  {} {}  来源={} 速率={}  网卡={} 工具={}  门限={:?}",
                leg.flow.label(),
                leg.verdict,
                leg.source.label(),
                fmt_rate(leg.mbps),
                fmt_rate(leg.nic_rx_mbps),
                fmt_rate(leg.tool.receiver_mbps),
                leg.target_mbps
            );
        }
        println!("  单元判定 {}: {}", row.verdict, row.detail);
        report.units.push(row);
        report::save_progress(dir, report, &mut appended, &mut last_heavy)?;
        observer(report, dir);
    }
    Ok(())
}

fn all_units_resumed(units: &[Unit], resumed: &std::collections::HashSet<String>) -> bool {
    !units.is_empty() && units.iter().all(|unit| resumed.contains(&unit.id))
}

fn can_skip_before_device_identification(
    cfg: &InnerConfig,
    units: &[Unit],
    resumed: &std::collections::HashSet<String>,
) -> bool {
    !cfg.serial.trim().is_empty() && all_units_resumed(units, resumed)
}

fn resumed_row(unit: &Unit) -> UnitRow {
    // RESUME 摘要只保存「这个单元曾 PASS」，不保存当时 NicPreferred 最终
    // 选的是哪一层。NicPreferred 仍以网卡门限为主；明确 Tool 才能确定使用
    // 工具合计门限。不能让明确的 Tool 单元在报告里显示成网卡门限。
    let total_target = unit.total_target(unit.measurement == Measurement::Tool);
    UnitRow {
        id: unit.id.clone(),
        index: unit.index,
        link: unit.link_name.clone(),
        host: unit.host.clone(),
        protocol: unit.protocol,
        direction: unit.direction,
        streams: unit.streams,
        repeat: unit.repeat,
        measurement: unit.measurement,
        verdict: "PASS".into(),
        resumed: true,
        reason: "RESUME_SKIP".into(),
        detail: "24 小时内已有同一内环单元 PASS，本轮跳过（RESUME）".into(),
        diagnostics: vec!["复用历史 PASS；未重新起流".into()],
        total_mbps: None,
        total_target_mbps: total_target,
        overlap_secs: None,
        legs: Vec::new(),
    }
}

struct UnitContext<'a> {
    adb: &'a Adb,
    cfg: &'a InnerConfig,
    link: &'a Link,
    preflight: &'a LinkPreflight,
    bin: &'a str,
    agent: Option<config::AgentConfig>,
    dir: &'a Path,
    cancel: &'a AtomicBool,
}

/// 一条腿跑完之后拿到的原始数据，还没有经过来源选择。
struct LegRaw {
    plan: LegPlan,
    receiver: String,
    receiver_host: String,
    counter_source: Option<CounterSource>,
    client: IperfClientOut,
    events: Vec<IperfFlowEvent>,
    samples: MonitorStopOut,
    server_log: String,
}

fn run_unit(context: &UnitContext<'_>, unit: &Unit) -> Result<UnitRow, String> {
    let UnitContext {
        cfg,
        link,
        preflight,
        dir,
        cancel,
        ..
    } = context;
    let epoch = Instant::now();
    // 本单元自己的取消位：全局取消会被镜像进来，任何一条腿失败也会置位，
    // 让对向立刻停下来。停止一次作用于整个单元。
    let unit_cancel = AtomicBool::new(cancel.load(Ordering::SeqCst));
    let mut owners = Vec::new();
    let mut servers: Vec<receiver_server::ReceiverServer> = Vec::new();
    let mut logs = Vec::new();
    // 每条腿在实际接收端起 server：上行板侧，下行 PC。
    for leg in &unit.legs {
        let owner = crate::util::md5_hex(&format!(
            "{}-{}-{:?}-{:?}-{:?}",
            dir.display(),
            unit.index,
            unit.protocol,
            leg.flow,
            Instant::now()
        ));
        let log = dir.join(format!("unit{}-{:?}-server.log", unit.index, leg.flow));
        match receiver_server::ReceiverServer::start(context, leg, &owner, &log) {
            Ok(server) => servers.push(server),
            Err(error) => {
                // 先起来的那条腿要定向回收，不能留在板子上占端口。
                let cleanup = stop_servers(&mut servers);
                return Err(match cleanup {
                    Ok(()) => format!("{} {} 腿: {error}", unit.title(), leg.flow.label()),
                    Err(other) => format!(
                        "{} {} 腿: {error}；另有回收失败：{other}",
                        unit.title(),
                        leg.flow.label()
                    ),
                });
            }
        }
        owners.push(owner);
        logs.push(log);
    }

    let outcome = run_legs(context, unit, &owners, epoch, &unit_cancel);
    let server_cleanup = stop_servers(&mut servers);
    let raw = match outcome {
        Ok(raw) => raw,
        Err(error) => {
            return Err(match server_cleanup {
                Ok(()) => error,
                Err(other) => format!("{error}；另有回收失败：{other}"),
            })
        }
    };
    // server 停稳之后再读日志，否则拿到的是半截输出。
    let mut raw = raw;
    for (leg, log) in raw.iter_mut().zip(logs.iter()) {
        leg.server_log = clip_log(&std::fs::read_to_string(log).unwrap_or_default());
    }
    let mut row = assemble_unit(cfg, link, unit, preflight, raw);
    if let Err(error) = &server_cleanup {
        row.diagnostics.push(error.clone());
    }
    // 回收没确认就不再复用端口，也不改写已经形成的判定。
    server_cleanup?;
    Ok(row)
}

fn stop_servers(servers: &mut Vec<receiver_server::ReceiverServer>) -> Result<(), String> {
    let errors: Vec<String> = servers
        .iter_mut()
        .filter_map(|server| server.stop().err())
        .collect();
    servers.clear();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
}

/// 起监控、打流、收监控。双向的两条腿在这里**同时**跑。
fn run_legs(
    context: &UnitContext<'_>,
    unit: &Unit,
    owners: &[String],
    epoch: Instant,
    unit_cancel: &AtomicBool,
) -> Result<Vec<LegRaw>, String> {
    let UnitContext {
        cfg,
        link,
        preflight,
        bin,
        agent,
        cancel,
        ..
    } = context;
    let remote = agent.clone().map(remote::Remote::new);
    let mut leases: Vec<remote::RemoteLease> = Vec::new();
    let mut samplers = Vec::new();
    for (leg, owner) in unit.legs.iter().zip(owners) {
        if let Some(remote) = &remote {
            leases.push(remote::RemoteLease::new(remote.clone(), owner));
        }
        let iface = if leg.flow.receiver_is_board() {
            preflight.board_iface.clone()
        } else {
            link.local_interface.clone()
        };
        match start_sampler(context, leg.flow, &iface, owner, epoch, remote.as_ref()) {
            Ok(sampler) => samplers.push(sampler),
            Err(error) if link.measurement != Measurement::NicStrict => {
                samplers.push(Sampler::Unavailable(error));
            }
            Err(error) => {
                drop(samplers);
                let cleanup = close_leases(&mut leases);
                return Err(match cleanup {
                    Ok(()) => format!("{} 腿的接收端采样起不来: {error}", leg.flow.label()),
                    Err(other) => format!(
                        "{} 腿的接收端采样起不来: {error}；另有回收失败：{other}",
                        leg.flow.label()
                    ),
                });
            }
        }
    }
    pause(Duration::from_millis(BASELINE_MS), cancel);
    if cancel.load(Ordering::SeqCst) {
        drop(samplers);
        close_leases(&mut leases)?;
        return Err("用户已取消".into());
    }

    // 两条腿必须真的同时在跑，所以 client 放进作用域线程；再挂一个看门线程
    // 把全局取消镜像进单元取消位，让任一腿的失败也能停下对向。
    let done = AtomicBool::new(false);
    let results: Vec<Result<(IperfClientOut, Vec<IperfFlowEvent>), String>> =
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::SeqCst) {
                    if cancel.load(Ordering::SeqCst) {
                        unit_cancel.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
            let handles: Vec<_> = unit
                .legs
                .iter()
                .zip(owners)
                .map(|(leg, owner)| {
                    let request = client_request(cfg, link, unit.protocol, leg.flow, leg.port);
                    let remote = remote.clone();
                    scope.spawn(move || {
                        let out = if leg.flow.receiver_is_board() {
                            run_client(bin, remote.as_ref(), request, owner, epoch, unit_cancel)
                        } else {
                            adb_client::run(
                                context.adb,
                                &cfg.board_iperf,
                                &request,
                                owner,
                                epoch,
                                unit_cancel,
                            )
                        };
                        if out.as_ref().is_err_and(|_| true)
                            || out.as_ref().is_ok_and(|(client, _)| !client.ok)
                        {
                            // 一腿倒了就停对向：半条腿的双向数据没有意义。
                            unit_cancel.store(true, Ordering::SeqCst);
                        }
                        out
                    })
                })
                .collect();
            let results: Vec<_> = handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err("内环打流线程异常终止".into()))
                })
                .collect();
            done.store(true, Ordering::SeqCst);
            results
        });

    let mut raw = Vec::new();
    let mut failures = Vec::new();
    for ((leg, sampler), result) in unit.legs.iter().zip(samplers).zip(results) {
        let samples = sampler.stop().or_else(|error| {
            if link.measurement == Measurement::NicStrict {
                Err(error)
            } else {
                Ok(MonitorStopOut {
                    errors: vec![error],
                    ..Default::default()
                })
            }
        });
        let (client, events) = match result {
            Ok(value) => value,
            Err(error) => {
                failures.push(format!("{} 腿: {error}", leg.flow.label()));
                continue;
            }
        };
        let samples = match samples {
            Ok(samples) => samples,
            Err(error) => {
                failures.push(format!(
                    "{} 腿的接收端采样收不回来: {error}",
                    leg.flow.label()
                ));
                continue;
            }
        };
        let board = leg.flow.receiver_is_board();
        raw.push(LegRaw {
            plan: leg.clone(),
            receiver: if board {
                preflight.board_iface.clone()
            } else {
                link.local_interface.clone()
            },
            receiver_host: if board {
                "板侧".into()
            } else {
                link.host.clone()
            },
            counter_source: board.then_some(preflight.counter_source).flatten(),
            client,
            events,
            samples,
            server_log: String::new(),
        });
    }
    let cleanup = close_leases(&mut leases);
    if !failures.is_empty() {
        return Err(match cleanup {
            Ok(()) => failures.join("；"),
            Err(other) => format!("{}；另有回收失败：{other}", failures.join("；")),
        });
    }
    cleanup?;
    Ok(raw)
}

fn close_leases(leases: &mut Vec<remote::RemoteLease>) -> Result<(), String> {
    let errors: Vec<String> = leases
        .iter_mut()
        .filter_map(|lease| lease.close().err())
        .collect();
    leases.clear();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
}

fn run_client(
    bin: &str,
    remote: Option<&remote::Remote>,
    request: IperfClientReq,
    owner: &str,
    epoch: Instant,
    cancel: &AtomicBool,
) -> Result<(IperfClientOut, Vec<IperfFlowEvent>), String> {
    if let Some(remote) = remote {
        return remote.client(request, owner, epoch, cancel);
    }
    let mut origin = None;
    let mut events = Vec::new();
    let client = iperf::run_client_controlled(
        bin,
        &request,
        Some(cancel),
        |_| {},
        |mut event| {
            iperf::align_event_to_epoch(
                &mut event,
                epoch.elapsed().as_millis() as u64,
                &mut origin,
            );
            events.push(event);
        },
    );
    Ok((client, events))
}

// ---------------- 采样 ----------------

/// 一路接收端采样。停不下来时 Drop 兜底，绝不把监控线程留在后台。
enum Sampler {
    Unavailable(String),
    Local {
        mgr: MonitorMgr,
        id: String,
        offset_ms: u64,
        stopped: bool,
    },
    Remote {
        remote: remote::Remote,
        id: String,
        offset_ms: u64,
        stopped: bool,
    },
}

fn validate_remote_monitor_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        Err("辅测机 monitor 启动响应缺少有效 ID".into())
    } else {
        Ok(())
    }
}

impl Sampler {
    fn stop(mut self) -> Result<MonitorStopOut, String> {
        let (mut samples, offset) = match &mut self {
            Self::Unavailable(error) => {
                return Ok(MonitorStopOut {
                    errors: vec![error.clone()],
                    ..Default::default()
                })
            }
            Self::Local {
                mgr,
                id,
                offset_ms,
                stopped,
            } => {
                // **先停，成功了再记**。反过来写的话，一次失败的停止会把
                // `stopped` 永久置真，而这个类型的契约正好是「停不下来时 Drop
                // 兜底，绝不把监控线程留在后台」——兜底被自己关掉了，采样线程
                // 就按整个租约继续跑下去。
                //
                // 这里和 `ReceiverServer`/`Lease` 的「失败也不再来第二遍」相反，
                // 是有意的：停止监控按 id 幂等，重来一次没有副作用，而留着不停
                // 是实打实的泄漏。
                let out = mgr.stop(id)?;
                *stopped = true;
                (out, *offset_ms)
            }
            Self::Remote {
                remote,
                id,
                offset_ms,
                stopped,
            } => {
                // 同上：先停成功再记。远端这条尤其要紧——停不下来时辅测机会按
                // 整个租约（duration_secs + 150 秒）继续每秒采一次那块网卡。
                let out = remote
                    .post::<MonitorStopOut>("/monitor/stop", &MonitorStopReq { id: id.clone() })?;
                *stopped = true;
                (out, *offset_ms)
            }
        };
        for sample in &mut samples.samples {
            sample.elapsed_ms = sample.elapsed_ms.saturating_add(offset);
        }
        Ok(samples)
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        match self {
            Self::Local { mgr, stopped, .. } if !*stopped => {
                let _ = mgr.stop_all();
            }
            Self::Remote {
                remote,
                id,
                stopped,
                ..
            } if !*stopped => {
                let _ = remote
                    .post::<MonitorStopOut>("/monitor/stop", &MonitorStopReq { id: id.clone() });
            }
            _ => {}
        }
    }
}

/// 采样放在**接收端所在的机器**上，不看谁跑 client。
///
/// 上行的接收端在板侧，由主控通过 ADB 读；下行的接收端是网口所在电脑，
/// 本机就本地读、辅测机就远端读。
fn start_sampler(
    context: &UnitContext<'_>,
    flow: Flow,
    iface: &str,
    owner: &str,
    epoch: Instant,
    remote: Option<&remote::Remote>,
) -> Result<Sampler, String> {
    let lease = context.cfg.duration_secs + 150;
    if let Some(remote) = remote.filter(|_| !flow.receiver_is_board()) {
        let started: MonitorStartOut = remote.post(
            "/monitor/start",
            &MonitorStartReq {
                iface: iface.into(),
                interval_ms: 1000,
                owner_id: owner.into(),
                lease_secs: lease,
            },
        )?;
        // 没有 ID 就无法向辅测机发出对应的 stop；当前腿已经登记了
        // owner，调用方会沿 close_leases 走 owner cleanup，但这里仍要把
        // malformed response 判成启动失败，不能让一个不可停止的 monitor
        // 进入正常采样路径。
        validate_remote_monitor_id(&started.id)?;
        return Ok(Sampler::Remote {
            remote: remote.clone(),
            id: started.id,
            offset_ms: (epoch.elapsed().as_millis() as u64).saturating_sub(started.elapsed_ms),
            stopped: false,
        });
    }
    let reader: Arc<dyn NicCounterReader> = if flow.receiver_is_board() {
        Arc::new(BoardCounters {
            adb: context.adb.clone(),
            source: context
                .preflight
                .counter_source
                .ok_or("板侧接口没有可读的字节计数来源")?,
        })
    } else {
        Arc::new(SystemNicCounterReader)
    };
    let mgr = MonitorMgr::with_dependencies(Arc::new(SystemClock), reader);
    let id = mgr.start_owned(iface, 1000, owner, lease)?;
    let offset_ms =
        (epoch.elapsed().as_millis() as u64).saturating_sub(mgr.elapsed_ms(&id).unwrap_or(0));
    Ok(Sampler::Local {
        mgr,
        id,
        offset_ms,
        stopped: false,
    })
}

// ---------------- 组装结论 ----------------

/// 双向两条腿的**共同有效重叠窗口**。
///
/// 双向并发的结论只能建立在两条腿真的同时在跑的那一段上。没有交集就说明
/// 这不是并发——那时两条腿都拿不到有效窗口，判定自然落到 NOT_EVALUATED。
fn overlap_window(windows: &[EffectiveWindow]) -> Option<EffectiveWindow> {
    if windows.len() != 2 {
        return None;
    }
    let start = windows.iter().map(|w| w.start_ms).max()?;
    let end = windows.iter().map(|w| w.end_ms).min()?;
    if end <= start {
        return None;
    }
    let required_secs = windows.first().map(|w| w.required_secs).unwrap_or_default();
    let available_ms = end - start;
    Some(EffectiveWindow {
        start_ms: start,
        end_ms: end,
        available_secs: available_ms as f64 / 1_000.0,
        required_secs,
        complete: available_ms
            .saturating_add(crate::cmd::iperf_window::WINDOW_COMPLETE_TOLERANCE_MS)
            >= required_secs.saturating_mul(1_000),
    })
}

fn assemble_unit(
    cfg: &InnerConfig,
    link: &Link,
    unit: &Unit,
    preflight: &LinkPreflight,
    raw: Vec<LegRaw>,
) -> UnitRow {
    let windows: Vec<EffectiveWindow> = raw
        .iter()
        .map(|leg| {
            let parsed = iperf::parse_output(&leg.client.output);
            crate::cmd::iperf_window::iperf_effective_window(
                &leg.events,
                cfg.duration_secs,
                parsed.has_measurement(),
            )
        })
        .collect();
    // 双向：两条腿共用同一段重叠窗口；单向：各用自己的。
    let shared = unit.is_bidir().then(|| overlap_window(&windows)).flatten();
    let overlap_secs = shared.as_ref().map(|window| window.available_secs);
    let mut legs = Vec::new();
    let mut measurements = Vec::new();
    for (index, item) in raw.iter().enumerate() {
        let window = if unit.is_bidir() {
            shared.clone().unwrap_or(EffectiveWindow {
                required_secs: cfg.duration_secs,
                ..Default::default()
            })
        } else {
            windows[index].clone()
        };
        // 本腿自己的窗口在上面已经算过一次（`windows[index]`）；再算一遍要重新
        // 解析一遍客户端输出，而那份输出在 3600 秒 `-i 1` 的一轮里是几千行。
        let (row, measurement) =
            assemble_leg(cfg, link, unit, preflight, item, &window, &windows[index]);
        legs.push(row);
        measurements.push(measurement);
    }
    let mut diagnostics = Vec::new();
    if unit.is_bidir() {
        match overlap_secs {
            Some(secs) => diagnostics.push(format!(
                "双向两条腿的共同有效重叠窗口 {secs:.2}s，两条腿的速率都只取这一段。"
            )),
            None => diagnostics.push(
                "双向两条腿没有可证明的时间重叠，本单元不构成并发，两条腿都不形成有效结论。".into(),
            ),
        }
    }
    let (verdict, total_mbps, total_target) = if unit.is_bidir() {
        let refs: Vec<&LegMeasurement> = measurements.iter().collect();
        let judged = measure::total_verdict(
            &refs,
            unit.nic_total_target_mbps,
            unit.tool_total_target_mbps,
        );
        let same_source =
            refs.len() == 2 && refs[0].source == refs[1].source && refs[0].source != Source::None;
        let total = same_source
            .then(|| Some(refs[0].mbps? + refs[1].mbps?))
            .flatten();
        // 门限和合计值走**同一个判据**：两条腿来源层次不同的时候
        // `total_verdict` 已经判了 NOT_EVALUATED、`total` 也置了 None，此时再
        // 按 leg[0] 的来源报一个门限，报告和 result.json 里就会出现
        // `total_mbps: null, total_target_mbps: 900.0`——给一个明确拒绝了的
        // 测量摆上验收线，读的人无从判断这行到底该不该看。
        let target = same_source
            .then(|| refs.first())
            .flatten()
            .and_then(|leg| unit.total_target(leg.source == Source::Tool));
        // 配了合计门限就按合计判一次；没配就回落到逐腿聚合，两条腿各自
        // 按方向门限出结论——不由单向门限折半推算合计。
        if unit.nic_total_target_mbps.is_none() && unit.tool_total_target_mbps.is_none() {
            (aggregate(&legs, &judged), total, None)
        } else {
            (judged, total, target)
        }
    } else {
        (
            legs.first().map(verdict_of).unwrap_or_else(|| {
                crate::verdict::VerdictResult::not_evaluated(
                    crate::reason::ReasonCode::UnitDirectionResultMissing,
                    "本单元没有产生任何一条腿的结果",
                )
            }),
            None,
            None,
        )
    };
    diagnostics.extend(verdict.diagnostics.clone());
    UnitRow {
        id: unit.id.clone(),
        index: unit.index,
        link: unit.link_name.clone(),
        host: unit.host.clone(),
        protocol: unit.protocol,
        direction: unit.direction,
        streams: unit.streams,
        repeat: unit.repeat,
        measurement: unit.measurement,
        verdict: verdict.verdict.label().into(),
        resumed: false,
        reason: verdict.code.to_string(),
        detail: verdict.detail,
        diagnostics,
        total_mbps,
        total_target_mbps: total_target,
        overlap_secs,
        legs,
    }
}

fn verdict_of(leg: &LegRow) -> crate::verdict::VerdictResult {
    crate::verdict::VerdictResult::new(
        crate::verdict::Verdict::from_label(&leg.verdict).unwrap_or_default(),
        crate::reason::ReasonCode::parse_prefix(&leg.reason),
        leg.detail.clone(),
    )
}

/// 没有合计门限时的双向单元结论：逐腿聚合，优先级用全仓唯一的
/// [`crate::verdict::aggregate_verdict`]。
fn aggregate(
    legs: &[LegRow],
    fallback: &crate::verdict::VerdictResult,
) -> crate::verdict::VerdictResult {
    // 双向是一个并发单元，缺一条腿就没有「逐腿聚合」的语义。不能把现存的
    // PASS 当作整单元 PASS；`total_verdict` 已经给出了缺腿的封闭结论和原因。
    if legs.len() != 2 {
        return fallback.clone();
    }
    let verdict = crate::verdict::aggregate_verdict(legs.iter().filter_map(|leg| {
        Some((
            crate::verdict::Verdict::from_label(&leg.verdict)?,
            crate::reason::ReasonCode::parse_prefix(&leg.reason),
        ))
    }));
    let detail = legs
        .iter()
        .map(|leg| format!("{} {}", leg.flow.label(), leg.verdict))
        .collect::<Vec<_>>()
        .join("，");
    crate::verdict::VerdictResult::new(
        verdict,
        legs.iter()
            .find(|leg| crate::verdict::Verdict::from_label(&leg.verdict) == Some(verdict))
            .map(|leg| crate::reason::ReasonCode::parse_prefix(&leg.reason))
            .unwrap_or(fallback.code),
        format!("未设置双向合计门限，按逐方向门限聚合：{detail}"),
    )
}

fn assemble_leg(
    cfg: &InnerConfig,
    link: &Link,
    unit: &Unit,
    preflight: &LinkPreflight,
    raw: &LegRaw,
    window: &EffectiveWindow,
    own_window: &EffectiveWindow,
) -> (LegRow, LegMeasurement) {
    // 客户端输出只解析一次：这里以前解析三遍（本腿窗口一次、工具速率一次、
    // 诊断一次），三份结果完全相同。
    let parsed = iperf::parse_output(&raw.client.output);
    let cutoff = crate::cmd::iperf_window::iperf_baseline_cutoff_ms(&raw.events);
    let stats = monitor_rate_stats(&raw.samples, window, true, cutoff);
    let nic_target = raw.plan.nic_target_mbps;
    let nic = NicView {
        avg_mbps: stats.avg_mbps,
        // Auto 保留显式门限；无门限只测量，不进入 Verify 的 TARGET_MISSING 分支。
        acceptance: evaluate_rx_acceptance(crate::config::RateMode::Auto, nic_target, &stats),
    };
    // 工具口径：先认发送端 client 回传的 receiver 汇总；丢失时再取本腿接收端 server 日志。
    let streams = cfg.streams(unit.protocol);
    let mut tool_rate =
        measure::parse_receiver_summary(&raw.client.output, streams, ToolOrigin::ClientSummary);
    if tool_rate.is_err() && !raw.server_log.trim().is_empty() {
        if let Ok(rate) = measure::parse_receiver_summary(
            &raw.server_log,
            streams,
            if raw.plan.flow.receiver_is_board() {
                ToolOrigin::BoardServerLog
            } else {
                ToolOrigin::PcServerLog
            },
        ) {
            tool_rate = Ok(rate);
        }
    }
    let receiver_note = match &tool_rate {
        Ok(rate) => format!("取自{}", rate.origin.label()),
        Err(error) => format!("未取到可信的 receiver 汇总：{error}"),
    };
    let tool = ToolView { rate: tool_rate };
    // 全程 receiver 汇总无法裁成重叠区间。只在两腿窗口与汇总窗口对齐时
    // 允许它参与双向验收；原始工具速率仍保留在诊断字段。
    let tolerance = crate::cmd::iperf_window::WINDOW_COMPLETE_TOLERANCE_MS;
    let tool_for_verdict = if unit.is_bidir()
        && (window.end_ms <= window.start_ms
            || own_window.start_ms.abs_diff(window.start_ms) > tolerance
            || own_window.end_ms.abs_diff(window.end_ms) > tolerance)
    {
        ToolView {
            rate: Err("工具 receiver 汇总覆盖的是本腿全程，不能代表双向共同重叠窗口".into()),
        }
    } else {
        tool.clone()
    };
    let measurement = measure::select_leg(
        link.measurement,
        &nic,
        &tool_for_verdict,
        nic_target,
        raw.plan.tool_target_mbps,
        unit.total_target(false).is_some() || unit.total_target(true).is_some(),
    );

    let mut diagnostics = measurement.verdict.diagnostics.clone();
    diagnostics.push(
        "接收速率为接口总流量减去起流前背景中位数；请保持其他业务空闲，多网卡同网段时核实路由。"
            .into(),
    );
    if let Some(reason) = &measurement.fallback_reason {
        diagnostics.push(format!(
            "MEASUREMENT_SOURCE_FALLBACK: 已改用{}，原因是网卡字节计数不可信（{reason}）。\
             工具口径与网卡口径不是同一层数值，门限也各自独立。",
            measurement.source.label()
        ));
    }
    if raw.plan.flow.receiver_is_board() {
        diagnostics.push(format!(
            "板侧 {} 通过 ADB 每秒读取 {}；网桥计数器可能合并成员口流量或受硬件卸载影响，本框架逐网口串行执行。",
            preflight.board_iface,
            preflight.counter_source.map(|source| source.label()).unwrap_or("不可用的字节计数来源")
        ));
    } else if link.host == "master" {
        if let Some(caveat) = crate::nic::monitor::counter_source_caveat() {
            diagnostics.push(caveat.into());
        }
    }
    if let Some(hint) =
        measure::counter_mismatch_hint(&nic, &tool, &raw.receiver_host, &raw.receiver)
    {
        diagnostics.push(hint);
    }
    if !raw.client.ok {
        diagnostics.push(format!(
            "IPERF_EXEC_FAILED: 工具执行未正常完成；{}",
            raw.client.output
        ));
    }
    if !window.complete {
        diagnostics.push(format!(
            "IPERF_EFFECTIVE_WINDOW_SHORT: 有效时长 {:.2}s，配置 {}s",
            window.available_secs, window.required_secs
        ));
    }
    diagnostics.extend(raw.samples.errors.clone());
    diagnostics.extend(udp_loss_diagnostics(
        cfg,
        unit.protocol,
        parsed.udp_loss_pct,
        parsed.udp_lost_datagrams,
        parsed.udp_total_datagrams,
        &raw.server_log,
    ));
    let row = LegRow {
        flow: raw.plan.flow,
        port: raw.plan.port,
        receiver: raw.receiver.clone(),
        receiver_host: raw.receiver_host.clone(),
        counter_source: raw.counter_source,
        source: measurement.source,
        mbps: measurement.mbps,
        target_mbps: measurement.target_mbps,
        fallback_reason: measurement.fallback_reason.clone(),
        verdict: measurement.verdict.verdict.label().into(),
        reason: measurement.verdict.code.to_string(),
        detail: measurement.verdict.detail.clone(),
        diagnostics,
        nic_rx_mbps: stats.avg_mbps,
        nic_verdict: nic.acceptance.verdict.label().into(),
        nic_reason: nic.acceptance.code.to_string(),
        nic_target_mbps: nic_target,
        background_mbps: stats.baseline_mbps,
        coverage: stats.coverage,
        effective_secs: window.available_secs,
        required_secs: window.required_secs,
        rx: RxDistribution::from(&stats),
        tool: ToolReport {
            // 发送端只作诊断，永远不能顶替接收端。
            sender_mbps: parsed.best_sender(),
            receiver_mbps: tool.mbps(),
            receiver_note,
            udp_loss_pct: unit
                .protocol
                .is_udp()
                .then_some(parsed.udp_loss_pct)
                .flatten(),
            udp_lost_datagrams: unit
                .protocol
                .is_udp()
                .then_some(parsed.udp_lost_datagrams)
                .flatten(),
            udp_total_datagrams: unit
                .protocol
                .is_udp()
                .then_some(parsed.udp_total_datagrams)
                .flatten(),
        },
        client: raw.client.clone(),
        server_log: raw.server_log.clone(),
        rx_samples: Some(raw.samples.clone()),
    };
    (row, measurement)
}

/// 接收端 server 日志按「首尾各留一段」裁剪后才进报告。
///
/// 一条 3600 秒的链路每秒一行，一份报告里又有 网口 × 协议 × 方向 × 轮次 份，
/// 整段照抄能把 HTML 顶到几十 MB。要看的两头都在边上：开头的 accept /
/// connect 说明 client 到底有没有打到板子，结尾的汇总行带 UDP 的 lost/total。
/// 中间那几千行等速率对排障没有增量，砍掉时留一行明说砍了多少。
fn clip_log(text: &str) -> String {
    const KEEP: usize = 40;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= KEEP * 2 + 1 {
        return text.to_string();
    }
    let dropped = lines.len() - KEEP * 2;
    let mut out = lines[..KEEP].join("\n");
    out.push_str(&format!("\n……（省略中间 {dropped} 行）……\n"));
    out.push_str(&lines[lines.len() - KEEP..].join("\n"));
    out
}

fn fmt_rate(value: Option<f64>) -> String {
    value
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "未获取".into())
}

/// UDP 丢包诊断。和子网同一口径：超限只写诊断，不推翻速率判定——
/// 达标与否只由本腿选中的接收速率来源决定。
fn udp_loss_diagnostics(
    cfg: &InnerConfig,
    protocol: Protocol,
    loss_pct: Option<f64>,
    lost: Option<u64>,
    total: Option<u64>,
    server_log: &str,
) -> Vec<String> {
    if !protocol.is_udp() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let counters_complete = loss_pct.is_some() && lost.is_some() && total.is_some();
    match (loss_pct, lost, total) {
        (Some(pct), Some(lost), Some(total)) => {
            out.push(format!(
                "UDP 丢包 {lost}/{total} 数据报（{pct:.3}%），取自 receiver 汇总行。"
            ));
        }
        (Some(pct), ..) => out.push(format!(
            "UDP 丢包率解析为 {pct:.3}%，但 lost/total 计数不完整；丢包统计未知，不能把它当成 0%。"
        )),
        _ if server_log.contains("receiver") => out.push(
            "UDP 丢包未知：client 输出里没有 receiver 汇总行，请看下方接收端 server 原始输出。"
                .into(),
        ),
        _ => out.push(
            "UDP 丢包未知：两端都没拿到 receiver 汇总行；丢包率按「不知道」处理，不能当成 0%。"
                .into(),
        ),
    }
    if let Some(limit) = cfg.max_udp_loss_pct {
        match (loss_pct, counters_complete) {
            (Some(pct), true) if pct > limit => out.push(format!(
                "UDP_LOSS_HIGH: 丢包率 {pct:.3}% 超过门槛 {limit:.3}%；丢包只作诊断，达标与否只看接收端速率。"
            )),
            _ if !counters_complete => out.push(format!(
                "已配置 UDP 丢包门槛 {limit:.3}%，但没有完整的 lost/total 计数，门槛无法核验。"
            )),
            _ => {}
        }
    }
    out
}

fn client_request(
    cfg: &InnerConfig,
    link: &Link,
    protocol: Protocol,
    flow: Flow,
    port: u16,
) -> IperfClientReq {
    let mut extra = vec!["-P".into(), cfg.streams(protocol).to_string()];
    match protocol {
        Protocol::Tcp => {
            if let Some(window) = &cfg.tcp_window {
                extra.extend(["-w".into(), window.clone()]);
            }
        }
        Protocol::Udp => {
            // 校验保证测 UDP 时 udp_mbps 必有值；缺了宁可不带 -b 让 iperf3 报错，
            // 也不悄悄按它 1 Mbps 的默认速率跑出一份看着正常的报告。
            if let Some(mbps) = cfg.udp_mbps {
                extra.extend(["-b".into(), format!("{mbps}M")]);
            }
            if let Some(length) = &cfg.udp_length {
                extra.extend(["-l".into(), length.clone()]);
            }
        }
    }
    IperfClientReq {
        dst: if flow.receiver_is_board() {
            link.gateway
        } else {
            link.local_ip
        }
        .to_string(),
        bind_ip: if flow.receiver_is_board() {
            link.local_ip
        } else {
            link.gateway
        }
        .to_string(),
        port,
        duration: cfg.duration_secs,
        udp: protocol.is_udp(),
        v6: false,
        extra,
    }
}

#[cfg(test)]
mod tests;
