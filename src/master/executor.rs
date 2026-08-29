//! 任务执行器：调度本地/远端的 ping、iperf、监控、截图，产出报告行

use crate::clock::MonotonicClock;
#[cfg(test)]
use crate::clock::{ManualClock, SystemClock};
use crate::cmd::ctstraffic;
use crate::cmd::iperf::{self, IperfClientJobMgr, IperfServerMgr};
use crate::cmd::tools::{find_ctstraffic, find_iperf3};
use crate::config::{Config, RateCheckCfg, RateMode};
use crate::http_client;
use crate::master::builder::{
    v6_addrs, CtsTrafficTask, IperfTask, Leg, LegKind, PingPurpose, PingTask, Side, Unit,
    SINGLE_UDP_MIN_ATTEMPTS,
};
use crate::master::rate_window::{
    evaluate_nic_rx, monitor_rate_stats, nearest_valid_sample, percentile, rate_excursion,
    rate_sample_coverage_sufficient, rate_window_coverage_sufficient, EffectiveWindow, RateStats,
    MIN_RATE_SAMPLE_COVERAGE, MIN_VALID_RX_MBPS,
};
use crate::nic::monitor::MonitorMgr;
use crate::ping;
use crate::protocol::*;
use crate::reason::ReasonCode;
use crate::report::{report_endpoint, report_reason, DirectionSummary, Row, StreamCounts};
use crate::util::{lock_recover, logln, md5_hex, now_compact, now_full, sanitize};
use crate::verdict::{aggregate_verdict, ExecutionStatus, Verdict, VerdictResult};
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const UDP_SERVER_START_RETRIES: usize = 1;
const FLOW_TIMELINE_TOLERANCE_MS: u64 = 2_000;
const CTS_TIMELINE_TOLERANCE_MS: u64 = 100;
const RESOURCE_LEASE_GRACE_SECS: u64 = 300;
const RELIABLE_HTTP_ATTEMPTS: usize = 3;
const RELIABLE_HTTP_RETRY_DELAY: Duration = Duration::from_millis(250);
const RESOURCE_CLEANUP_WAIT_SECS: u64 = 10;
static RESOURCE_OWNER_SEQ: AtomicU64 = AtomicU64::new(1);

/// 双端网卡快照的来源。每个测试单元开始前调用一次。
///
/// 做成可注入而不是在执行器里硬编码一次 `/info`：执行器的单测用脚本化
/// transport 精确控制每一次 RPC 的时序与失败，硬加一次调用会把几十个
/// 与拓扑无关的用例全部拖下水。生产路径由 `ui.rs` 注入实现，
/// 测试里保持 `None` 即维持旧行为。
pub trait TopologySource: Send + Sync {
    /// 返回 (主控, 辅测) 的最新网卡快照。
    fn snapshot(&self) -> Result<(HostInfo, HostInfo), String>;
}

pub struct Ctx {
    pub agent_host: String,
    pub agent_port: u16,
    pub cfg: Config,
    pub outdir: PathBuf,
    /// 每个单元开始前重新拉取双端网卡；`None` 表示沿用计划时的快照。
    pub topology: Option<Arc<dyn TopologySource>>,
    /// Agent RPC transport. Production uses TCP; tests can inject a scripted
    /// transport to model loss, delay, truncation, and reordering.
    pub transport: Arc<dyn http_client::Transport>,
    pub clock: Arc<dyn MonotonicClock>,
    pub local_servers: IperfServerMgr,
    pub local_cts_jobs: IperfClientJobMgr,
    pub local_monitors: MonitorMgr,
    pub rows: Mutex<Vec<Row>>,
    pub db: Mutex<ResultDb>,
}

struct UnitResourceGuard<'a> {
    ctx: &'a Ctx,
    owner_id: String,
    remote_resources: bool,
    armed: bool,
}

#[derive(Clone, Copy)]
struct LifecycleLease<'a> {
    owner_id: &'a str,
    lease_secs: u64,
}

impl<'a> UnitResourceGuard<'a> {
    fn new(ctx: &'a Ctx, owner_id: String, remote_resources: bool) -> Self {
        Self {
            ctx,
            owner_id,
            remote_resources,
            armed: true,
        }
    }

    fn cleanup_now(&mut self) -> Result<(), String> {
        match self.cleanup_attempt() {
            Ok(()) => {
                self.armed = false;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn cleanup_attempt(&self) -> Result<(), String> {
        catch_unwind(AssertUnwindSafe(|| {
            self.ctx
                .cleanup_owner_resources(&self.owner_id, self.remote_resources)
        }))
        .unwrap_or_else(|payload| {
            Err(format!(
                "owner={} 资源清理 panic: {}",
                self.owner_id,
                panic_text(payload.as_ref())
            ))
        })
    }
}

impl Drop for UnitResourceGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Err(e) = self.cleanup_attempt() {
                logln(&format!(
                    "    [资源兜底清理失败] owner={}：{}",
                    self.owner_id, e
                ));
            }
        }
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

fn unit_has_traffic(unit: &Unit) -> bool {
    unit_has_iperf(unit) || unit_has_ctstraffic(unit)
}

fn unit_uses_agent_resources(unit: &Unit) -> bool {
    unit.legs.iter().any(|leg| match &leg.kind {
        LegKind::IperfSingle(task) => task.src.side == Side::Agent || task.dst.side == Side::Agent,
        LegKind::IperfGroup { streams, .. } => streams
            .iter()
            .any(|task| task.src.side == Side::Agent || task.dst.side == Side::Agent),
        LegKind::CtsTraffic(task) => task.src.side == Side::Agent || task.dst.side == Side::Agent,
        LegKind::Ping(_) => false,
    })
}

fn unit_resource_owner(unit: &Unit, sequence: usize) -> String {
    let nonce = RESOURCE_OWNER_SEQ.fetch_add(1, Ordering::SeqCst);
    format!(
        "unit-{}-{sequence}-{nonce}-{}-{}",
        std::process::id(),
        now_compact(),
        &md5_hex(&unit.id)[..8]
    )
}

fn unit_resource_lease_secs(unit: &Unit) -> u64 {
    unit.est_secs
        .saturating_add(RESOURCE_LEASE_GRACE_SECS)
        .max(RESOURCE_LEASE_GRACE_SECS)
}

fn lifecycle_request_id(owner_id: &str, kind: &str, port: u16, attempt: usize) -> String {
    format!("{owner_id}:{kind}:{port}:{attempt}")
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "未知 panic".into())
}

/// 连续多少个「零测量」灌包单元开始告警。只影响提示，不影响是否中止。
pub const DEAD_TRAFFIC_STREAK_WARN: usize = 2;

#[derive(Debug, Default, Clone)]
pub struct RunSummary {
    pub pass: usize,
    pub fail: usize,
    pub measured: usize,
    pub not_evaluated: usize,
    pub setup_error: usize,
    pub skip: usize,
    /// 本轮选择并处理的灌包单元数（iperf3 + ctsTraffic，包括前置拦截）。
    pub traffic_units: usize,
    /// 至少产生一项有效工具/NIC 速率测量的灌包单元数。
    pub traffic_usable_units: usize,
    /// 最终判为 SETUP_ERROR 的灌包单元数。
    pub traffic_setup_errors: usize,
    /// 本轮出现过的「连续零测量灌包单元」最长连击。
    ///
    /// run_20260825_215915_7684 的尾部有 6 个单元一条测量都没产生、白跑了
    /// 21 分钟，而工具全程没有任何提示——这个数就是为了让那件事在报告里
    /// 留下痕迹（见 .ai/DESIGN-v4.3.0.md D6）。
    pub max_dead_traffic_streak: usize,
    /// 因连续零测量而主动中止剩余队列时，记录中止点（已执行的单元序号）。
    pub aborted_at_unit: Option<usize>,
}

impl RunSummary {
    pub fn merge(&mut self, other: RunSummary) {
        self.pass += other.pass;
        self.fail += other.fail;
        self.measured += other.measured;
        self.not_evaluated += other.not_evaluated;
        self.setup_error += other.setup_error;
        self.skip += other.skip;
        self.traffic_units += other.traffic_units;
        self.traffic_usable_units += other.traffic_usable_units;
        self.traffic_setup_errors += other.traffic_setup_errors;
        self.max_dead_traffic_streak = self
            .max_dead_traffic_streak
            .max(other.max_dead_traffic_streak);
        self.aborted_at_unit = self.aborted_at_unit.or(other.aborted_at_unit);
    }

    /// 报告顶部的「运行健康」横幅文案；一切正常时为空。
    pub fn run_health_banner(&self) -> String {
        if let Some(at) = self.aborted_at_unit {
            return format!(
                "本轮在第 {at} 个单元后主动中止：连续 {} 个灌包单元一条测量都没产生，\
                 继续跑下去只会产生更多空数据。请先确认被测设备是否掉线或重启，再重跑剩余项。",
                self.max_dead_traffic_streak
            );
        }
        if self.max_dead_traffic_streak >= DEAD_TRAFFIC_STREAK_WARN {
            return format!(
                "本轮出现过连续 {} 个灌包单元一条测量都没产生。这通常意味着测试中途链路或\
                 被测设备失联，这些单元的结论不代表设备性能。",
                self.max_dead_traffic_streak
            );
        }
        String::new()
    }

    /// 只要本轮确实选择了流量测试，但一项有效速率测量都没有，就需要追加
    /// 子网 Ping 与网卡到网关 Ping，区分网络/载体异常和后端搭建异常。
    pub fn needs_traffic_failure_diagnostics(&self) -> bool {
        self.traffic_units > 0 && self.traffic_usable_units == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IperfPreflightBlock {
    pub reason_code: ReasonCode,
    pub reason_detail: String,
}

#[derive(Debug)]
/// 一条腿跑完之后留下的东西：**判定结论 + 测到的量 + 落到报表哪几行**。
///
/// 三者分开摆是有意的。判定结论（`judgement`）由纯函数从「已确定的事实」
/// 算出，不含执行状态；测量值（`rx_avg`）是执行留下的事实本身；`main_rows`
/// 只是报表索引。此前判定的三个字段和测量值平铺在一起，读代码的人分不清
/// 哪些是「测到的」哪些是「判出来的」，改判定口径时也就分不清该动哪儿。
struct LegOutcome {
    judgement: VerdictResult,
    rx_avg: Option<f64>,
    main_rows: Vec<usize>,
    tag: String,
}

impl LegOutcome {
    fn verdict(&self) -> Verdict {
        self.judgement.verdict
    }

    fn reason_code(&self) -> ReasonCode {
        self.judgement.code
    }

    fn reason_detail(&self) -> &str {
        &self.judgement.detail
    }
}

fn preflight_block_outcome(tag: &str, block: &IperfPreflightBlock) -> LegOutcome {
    LegOutcome {
        judgement: VerdictResult::new(
            Verdict::SetupError,
            block.reason_code,
            block.reason_detail.clone(),
        ),
        rx_avg: None,
        main_rows: Vec::new(),
        tag: tag.to_string(),
    }
}

fn preflight_block_outcomes(unit: &Unit, block: &IperfPreflightBlock) -> Vec<LegOutcome> {
    let mut outcomes: Vec<LegOutcome> = unit
        .legs
        .iter()
        .filter_map(|leg| match &leg.kind {
            LegKind::IperfSingle(_) | LegKind::IperfGroup { .. } | LegKind::CtsTraffic(_) => {
                Some(preflight_block_outcome(&leg.tag, block))
            }
            LegKind::Ping(_) => None,
        })
        .collect();
    if outcomes.is_empty() {
        outcomes.push(preflight_block_outcome("", block));
    }
    outcomes
}

fn execute_unit_safely<F, C>(execute: F, cleanup: C) -> Vec<LegOutcome>
where
    F: FnOnce() -> Vec<LegOutcome>,
    C: FnOnce() -> Result<(), String>,
{
    let mut outcomes = match catch_unwind(AssertUnwindSafe(execute)) {
        Ok(outcomes) => outcomes,
        Err(payload) => {
            let detail = format!("测试单元执行 panic: {}", panic_text(payload.as_ref()));
            logln(&format!("    [单元异常隔离] {detail}"));
            vec![LegOutcome {
                judgement: VerdictResult::setup_error(ReasonCode::UnitPanic, detail),
                rx_avg: None,
                main_rows: vec![],
                tag: String::new(),
            }]
        }
    };
    let cleanup_result = catch_unwind(AssertUnwindSafe(cleanup)).unwrap_or_else(|payload| {
        Err(format!(
            "测试单元资源清理 panic: {}",
            panic_text(payload.as_ref())
        ))
    });
    if let Err(error) = cleanup_result {
        logln(&format!("    [资源清理未确认] {error}"));
        outcomes.push(LegOutcome {
            judgement: VerdictResult::new(
                Verdict::SetupError,
                ReasonCode::ResourceCleanupFailed,
                error,
            ),
            rx_avg: None,
            main_rows: vec![],
            tag: "cleanup".into(),
        });
    }
    outcomes
}

impl Ctx {
    // ---------------- agent HTTP ----------------

    fn push_row(&self, row: Row) -> usize {
        let mut g = lock_recover(&self.rows);
        g.push(row);
        g.len() - 1
    }

    pub fn run_all_from(&self, units: &[Unit], sequence_offset: usize) -> RunSummary {
        self.run_all_internal(units, sequence_offset, None)
    }

    #[cfg(test)]
    pub fn run_all_with_preflight(
        &self,
        units: &[Unit],
        block: Option<&IperfPreflightBlock>,
    ) -> RunSummary {
        let blocks: HashMap<String, IperfPreflightBlock> = block
            .map(|block| {
                units
                    .iter()
                    .filter(|unit| unit_has_iperf(unit))
                    .map(|unit| (unit.id.clone(), block.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.run_all_internal(units, 0, Some(&blocks))
    }

    pub fn run_all_with_preflight_blocks(
        &self,
        units: &[Unit],
        blocks: &HashMap<String, IperfPreflightBlock>,
    ) -> RunSummary {
        self.run_all_internal(units, 0, Some(blocks))
    }

    fn run_all_internal(
        &self,
        units: &[Unit],
        sequence_offset: usize,
        preflight_blocks: Option<&HashMap<String, IperfPreflightBlock>>,
    ) -> RunSummary {
        let mut sum = RunSummary::default();
        let total = units.len();
        let mut dead_streak = 0usize;
        for (i, unit) in units.iter().enumerate() {
            if crate::cancel::is_cancelled() {
                logln("\n!! 用户中断 (Ctrl+C)，正在生成部分报告...");
                break;
            }
            // 熔断检查放在循环开头而不是结尾：单元有多条 `continue` 提前退出的
            // 路径（resume 命中、前置拦截、网卡消失），放在结尾时那些路径会
            // 整个跳过它。而「网卡消失」恰恰是本设置最该拦住的场景——被测设备
            // 掉线后每个单元的开跑前重扫都会看到网卡不见了，队列会一路空转到底。
            let abort_at = self.cfg.abort_after_dead_traffic_units;
            if abort_at > 0 && dead_streak >= abort_at {
                logln(&format!(
                    "\n!! 连续 {dead_streak} 个灌包单元没有产生任何测量，按 abort_after_dead_traffic_units={abort_at} 中止剩余 {} 个单元。\n\
                     !! 请先确认被测设备是否掉线或重启，再重跑剩余项；已完成的部分会照常出报告。",
                    total.saturating_sub(i)
                ));
                sum.aborted_at_unit = Some(i);
                break;
            }
            let useq = sequence_offset + i;
            let is_traffic_unit = unit_has_traffic(unit);
            if is_traffic_unit {
                sum.traffic_units += 1;
            }
            let blocked = preflight_blocks.and_then(|blocks| blocks.get(&unit.id));
            logln(&format!("\n[{}/{}] {}", i + 1, total, unit.title));

            // 用最新一次双端扫描刷新本单元的网卡信息。拉不到就沿用计划时的
            // 快照继续跑——一次 RPC 抖动不该废掉整轮测试。
            let refreshed;
            let mut unit = unit;
            if let Some(source) = &self.topology {
                match source.snapshot() {
                    Ok((master, agent)) => {
                        let mut patched = unit.clone();
                        let drifts = crate::master::builder::refresh_unit_endpoints(
                            &mut patched,
                            &master,
                            &agent,
                        );
                        for drift in &drifts {
                            logln(&format!("  [拓扑变更] {}", drift.describe()));
                        }
                        if let Some(gone) = drifts.iter().find(|drift| drift.is_gone()) {
                            // 对着一块已经不存在的网卡起 monitor 只会采到别的东西
                            // 或者静默采空，这种单元必须当场判死而不是照跑。
                            let detail = format!(
                                "{}；本单元用到的网卡在开始前已不存在，无法采样",
                                gone.describe()
                            );
                            logln(&format!("  !! {detail}"));
                            sum.setup_error += 1;
                            sum.fail += 1;
                            if is_traffic_unit {
                                sum.traffic_setup_errors += 1;
                                dead_streak += 1;
                                sum.max_dead_traffic_streak =
                                    sum.max_dead_traffic_streak.max(dead_streak);
                            }
                            self.push_row(Row {
                                sort_key: (useq, 0, 0, 0),
                                time: now_full(),
                                task_id: unit.id.clone(),
                                parent_id: unit.id.clone(),
                                task: unit.title.clone(),
                                verdict: Verdict::SetupError,
                                execution_status: ExecutionStatus::Error,
                                reason_code: ReasonCode::NicDisappeared,
                                reason_detail: detail,
                                kind_label: "跳过(网卡已消失)".into(),
                                is_unit_summary: true,
                                ..Default::default()
                            });
                            continue;
                        }
                        refreshed = patched;
                        unit = &refreshed;
                    }
                    Err(error) => {
                        logln(&format!(
                            "  (网卡快照刷新失败，沿用计划时的信息继续: {error})"
                        ));
                    }
                }
            }

            if self.cfg.resume && blocked.is_none() {
                let fresh = { lock_recover(&self.db).fresh_pass(&unit.id) };
                if let Some(t) = fresh {
                    logln(&format!("  已PASS，上次时间: {t}，跳过 (RESUME)"));
                    sum.skip += 1;
                    if is_traffic_unit {
                        // 24 小时内已有 PASS 结果时，不因本轮 resume 跳过而重复触发故障诊断。
                        sum.traffic_usable_units += 1;
                    }
                    self.push_row(Row {
                        sort_key: (useq, 0, 0, 0),
                        time: now_full(),
                        task_id: unit.id.clone(),
                        parent_id: unit.id.clone(),
                        task: unit.title.clone(),
                        verdict: Verdict::Skip,
                        execution_status: ExecutionStatus::Skipped,
                        reason_code: ReasonCode::ResumeFreshPass,
                        reason_detail: format!(
                            "复用 {t} 的正式 PASS；本轮启用 resume，且结果未超过 {RESUME_MAX_AGE_HOURS} 小时，因此跳过执行"
                        ),
                        kind_label: format!("跳过(上次PASS: {t})"),
                        is_unit_summary: true,
                        ..Default::default()
                    });
                    continue;
                }
            }

            if let Some(block) = blocked {
                logln(&format!(
                    "  [流量后端前置检查拦截] {}: {}",
                    block.reason_code, block.reason_detail
                ));
            }

            let owner_id = unit_resource_owner(unit, useq);
            let lease_secs = unit_resource_lease_secs(unit);
            let mut resource_guard = (is_traffic_unit && blocked.is_none()).then(|| {
                UnitResourceGuard::new(self, owner_id.clone(), unit_uses_agent_resources(unit))
            });
            let mut outcomes = execute_unit_safely(
                || {
                    if let Some(block) = blocked {
                        self.preflight_block_outcomes_with_cts_args(
                            useq, unit, block, &owner_id, lease_secs,
                        )
                    } else if let Some(plans) = self.udp_leg_plans(unit) {
                        self.run_udp_unit(useq, unit, &plans, &owner_id, lease_secs)
                    } else if unit.legs.len() <= 1 {
                        unit.legs
                            .iter()
                            .map(|leg| self.run_leg(useq, unit, 0, leg, &owner_id, lease_secs))
                            .collect()
                    } else {
                        std::thread::scope(|s| {
                            let handles: Vec<_> = unit
                                .legs
                                .iter()
                                .enumerate()
                                .map(|(li, leg)| {
                                    let owner_id = owner_id.clone();
                                    s.spawn(move || {
                                        self.run_leg(useq, unit, li, leg, &owner_id, lease_secs)
                                    })
                                })
                                .collect();
                            handles
                                .into_iter()
                                .zip(unit.legs.iter())
                                .map(|(handle, leg)| {
                                    handle.join().unwrap_or_else(|payload| LegOutcome {
                                        judgement: VerdictResult::new(
                                            Verdict::SetupError,
                                            ReasonCode::LegThreadPanic,
                                            format!(
                                                "{} 方向执行线程 panic: {}",
                                                if leg.tag.is_empty() {
                                                    "单向"
                                                } else {
                                                    leg.tag.as_str()
                                                },
                                                panic_text(payload.as_ref())
                                            ),
                                        ),
                                        rx_avg: None,
                                        main_rows: vec![],
                                        tag: leg.tag.clone(),
                                    })
                                })
                                .collect()
                        })
                    }
                },
                || {
                    resource_guard
                        .as_mut()
                        .map(UnitResourceGuard::cleanup_now)
                        .unwrap_or(Ok(()))
                },
            );
            // cleanup_now 失败时 guard 仍保持 armed；立即 drop 再做一次兜底，
            // 不把可能残留的端口/进程拖到报告生成和下一测试单元。
            drop(resource_guard);

            // 执行线程 panic、前置拦截或内部调度异常都可能只返回
            // LegOutcome 而没有写入方向明细。报告必须始终保留每个预期流量方向，
            // 否则双向测试会出现只有 BA 而 AB 整行消失的误导性结果。
            if is_traffic_unit {
                self.ensure_traffic_outcome_rows(useq, unit, &mut outcomes);
            }

            // 双向：互填「对向接收 Mbps」
            if unit.bidir {
                let mut g = lock_recover(&self.rows);
                populate_peer_rx(&mut g, &outcomes);
            }

            let unit_verdict = aggregate_unit_verdict(&outcomes);
            if is_traffic_unit {
                let usable =
                    blocked.is_none() && self.outcomes_have_usable_traffic_measurement(&outcomes);
                if usable {
                    sum.traffic_usable_units += 1;
                    dead_streak = 0;
                } else {
                    // 「一条测量都没产生」和「测出来不达标」是两回事，这里只数前者。
                    dead_streak += 1;
                    sum.max_dead_traffic_streak = sum.max_dead_traffic_streak.max(dead_streak);
                    if dead_streak >= DEAD_TRAFFIC_STREAK_WARN {
                        logln(&format!(
                            "  !! 连续 {dead_streak} 个灌包单元没有产生任何测量——被测设备可能已掉线。\
                             后续单元大概率也是空跑；要自动中止请设 abort_after_dead_traffic_units。"
                        ));
                    }
                }
                if unit_verdict == Verdict::SetupError {
                    sum.traffic_setup_errors += 1;
                }
            }
            let unit_reason = outcome_matching_verdict(&outcomes, unit_verdict);
            let unit_ok = unit_verdict.is_pass();
            match unit_verdict {
                Verdict::Pass => sum.pass += 1,
                Verdict::Measured => sum.measured += 1,
                Verdict::NotEvaluated => {
                    sum.not_evaluated += 1;
                    sum.fail += 1;
                }
                Verdict::SetupError => {
                    sum.setup_error += 1;
                    sum.fail += 1;
                }
                Verdict::RateFail => sum.fail += 1,
                Verdict::Skip => sum.skip += 1,
            }
            let reasons: Vec<String> = outcomes
                .iter()
                .filter(|outcome| {
                    outcome.verdict() != Verdict::Pass
                        || !outcome.reason_code().is_empty()
                        || !outcome.reason_detail().is_empty()
                })
                .map(|outcome| {
                    format!(
                        "{}:{} {}",
                        if outcome.tag.is_empty() {
                            "单向"
                        } else {
                            &outcome.tag
                        },
                        outcome.reason_code(),
                        outcome.reason_detail()
                    )
                })
                .collect();
            let direction_summaries = self.direction_summaries(&outcomes);
            let single_direction = (direction_summaries.len() == 1)
                .then(|| direction_summaries.first())
                .flatten();
            let stream_counts = aggregate_direction_streams(&direction_summaries);
            logln(&format!("  ==> 单元结果: {}", unit_verdict.label()));
            self.push_row(Row {
                sort_key: (useq, usize::MAX, usize::MAX, u8::MAX),
                time: now_full(),
                task_id: unit.id.clone(),
                parent_id: unit.id.clone(),
                task: unit.title.clone(),
                verdict: unit_verdict,
                execution_status: match unit_verdict {
                    Verdict::SetupError => ExecutionStatus::Error,
                    Verdict::NotEvaluated => ExecutionStatus::Partial,
                    _ => ExecutionStatus::Completed,
                },
                reason_code: unit_reason
                    .map(|outcome| outcome.reason_code())
                    .unwrap_or_default(),
                reason_detail: reasons.join(" | "),
                kind_label: if unit.bidir {
                    "测试单元汇总(双向)".into()
                } else {
                    "测试单元汇总".into()
                },
                requested_streams: stream_counts.map_or(0, |counts| counts.requested),
                active_streams: stream_counts.map_or(0, |counts| counts.active),
                required_streams: stream_counts.map_or(0, |counts| counts.required),
                rx_avg: single_direction.and_then(|direction| direction.rx_avg),
                rx_p10: single_direction.and_then(|direction| direction.rx_p10),
                target_mbps: single_direction.and_then(|direction| direction.target_mbps),
                sample_coverage: single_direction.and_then(|direction| direction.sample_coverage),
                udp_loss: single_direction.and_then(|direction| direction.udp_loss),
                ping_loss: single_direction.and_then(|direction| direction.ping_loss),
                ping_min: single_direction.and_then(|direction| direction.ping_min),
                ping_avg: single_direction.and_then(|direction| direction.ping_avg),
                ping_max: single_direction.and_then(|direction| direction.ping_max),
                direction_summaries,
                is_unit_summary: true,
                ..Default::default()
            });
            {
                let mut db = lock_recover(&self.db);
                db.set(&unit.id, unit_ok, &unit.title);
                db.save();
            }
            if blocked.is_none() && is_traffic_unit {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        sum
    }

    fn outcomes_have_usable_traffic_measurement(&self, outcomes: &[LegOutcome]) -> bool {
        let rows = lock_recover(&self.rows);
        outcomes.iter().any(|outcome| {
            outcome.main_rows.iter().any(|index| {
                rows.get(*index)
                    .map(row_has_usable_traffic_measurement)
                    .unwrap_or(false)
            })
        })
    }

    fn direction_summaries(&self, outcomes: &[LegOutcome]) -> Vec<DirectionSummary> {
        let rows = lock_recover(&self.rows);
        outcomes
            .iter()
            .filter_map(|outcome| {
                let row = outcome
                    .main_rows
                    .iter()
                    .filter_map(|index| rows.get(*index))
                    .max_by_key(|row| {
                        u8::from(row.is_grouptotal) * 8
                            + u8::from(row.rx_p10.is_some()) * 4
                            + u8::from(row.rx_avg.is_some()) * 2
                            + u8::from(row.sample_coverage.is_some())
                    })?;
                let streams = (row.requested_streams > 0
                    || row.active_streams > 0
                    || row.required_streams > 0)
                    .then_some(StreamCounts {
                        requested: row.requested_streams,
                        active: row.active_streams,
                        required: row.required_streams,
                    });
                Some(DirectionSummary {
                    tag: if outcome.tag.is_empty() {
                        "单向".into()
                    } else {
                        outcome.tag.to_ascii_uppercase()
                    },
                    src: report_endpoint(&row.src_pc, &row.src_iface, &row.src_ip),
                    dst: report_endpoint(&row.dst_pc, &row.dst_iface, &row.dst_ip),
                    verdict: outcome.verdict(),
                    reason_code: outcome.reason_code(),
                    reason_detail: outcome.reason_detail().to_string(),
                    reason: report_reason(outcome.reason_code(), outcome.reason_detail()),
                    streams,
                    rx_avg: row.rx_avg,
                    rx_p10: row.rx_p10,
                    target_mbps: row.target_mbps,
                    sample_coverage: row.sample_coverage,
                    udp_loss: row.udp_loss,
                    ping_loss: row.ping_loss,
                    ping_min: row.ping_min,
                    ping_avg: row.ping_avg,
                    ping_max: row.ping_max,
                    screenshot_master: row.screenshot_master.clone(),
                    screenshot_agent: row.screenshot_agent.clone(),
                })
            })
            .collect()
    }

    fn ensure_traffic_outcome_rows(
        &self,
        useq: usize,
        unit: &Unit,
        outcomes: &mut Vec<LegOutcome>,
    ) {
        for (lidx, leg) in unit.legs.iter().enumerate() {
            if matches!(&leg.kind, LegKind::Ping(_)) {
                continue;
            }
            if outcomes
                .iter()
                .any(|outcome| outcome.tag == leg.tag && !outcome.main_rows.is_empty())
            {
                continue;
            }

            let matched = outcomes.iter().position(|outcome| outcome.tag == leg.tag);
            let inherited = matched.or_else(|| {
                outcomes
                    .iter()
                    .position(|outcome| outcome.tag.is_empty() && outcome.main_rows.is_empty())
            });
            let (verdict, reason_code, reason_detail) = inherited
                .map(|index| {
                    let outcome = &outcomes[index];
                    (
                        outcome.verdict(),
                        outcome.reason_code(),
                        outcome.reason_detail().to_string(),
                    )
                })
                .unwrap_or_else(|| {
                    (
                        Verdict::SetupError,
                        ReasonCode::UnitDirectionResultMissing,
                        format!(
                            "{} 方向执行未产生结果，已补入错误明细以保持报表完整",
                            if leg.tag.is_empty() {
                                "单向"
                            } else {
                                leg.tag.as_str()
                            }
                        ),
                    )
                });
            // push_row 先于 LegOutcome 返回；若随后外层 unit panic，outcomes 会被
            // UNIT_PANIC 替换，但已写入的方向 Row 仍然有效。先按稳定排序键复用这些
            // Row，避免再生成同方向占位而得到“原 AB + 补 AB + 补 BA”。
            let (committed_rows, committed_rx_avg) = {
                let rows = lock_recover(&self.rows);
                let indices: Vec<usize> = rows
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| {
                        !row.is_unit_summary
                            && row.parent_id == unit.id
                            && row.sort_key.0 == useq
                            && row.sort_key.1 == lidx
                    })
                    .map(|(index, _)| index)
                    .collect();
                let rx_avg = indices.iter().find_map(|index| rows[*index].rx_avg);
                (indices, rx_avg)
            };
            if !committed_rows.is_empty() {
                if let Some(index) = matched {
                    outcomes[index].main_rows.extend(committed_rows);
                    if outcomes[index].rx_avg.is_none() {
                        outcomes[index].rx_avg = committed_rx_avg;
                    }
                } else {
                    outcomes.push(LegOutcome {
                        judgement: VerdictResult::new(verdict, reason_code, reason_detail),
                        rx_avg: committed_rx_avg,
                        main_rows: committed_rows,
                        tag: leg.tag.clone(),
                    });
                }
                continue;
            }
            let row = self.push_traffic_outcome_row(
                useq,
                unit,
                lidx,
                leg,
                verdict,
                reason_code,
                &reason_detail,
            );
            let Some(row) = row else {
                continue;
            };
            if let Some(index) = matched {
                outcomes[index].main_rows.push(row);
            } else {
                outcomes.push(LegOutcome {
                    judgement: VerdictResult::new(verdict, reason_code, reason_detail),
                    rx_avg: None,
                    main_rows: vec![row],
                    tag: leg.tag.clone(),
                });
            }
        }
        // 整个单元 panic 时 execute_unit_safely 只能生成无 tag 结果。
        // 已将同一错误分发到所有有 tag 的方向明细后，删掉这个
        // 临时结果，避免汇总里同时出现“单向”与 AB/BA 重复原因。
        if unit
            .legs
            .iter()
            .filter(|leg| !matches!(&leg.kind, LegKind::Ping(_)))
            .all(|leg| !leg.tag.is_empty())
        {
            outcomes.retain(|outcome| !(outcome.tag.is_empty() && outcome.main_rows.is_empty()));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_traffic_outcome_row(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        leg: &Leg,
        verdict: Verdict,
        reason_code: ReasonCode,
        reason_detail: &str,
    ) -> Option<usize> {
        let (
            backend,
            ip,
            transport,
            param,
            src_pc,
            src_iface,
            src_ip,
            dst_pc,
            dst_iface,
            dst_ip,
            requested_streams,
        ) = match &leg.kind {
            LegKind::IperfSingle(task) => (
                "iperf",
                if task.v6 { "V6" } else { "V4" }.to_string(),
                if task.udp { "UDP" } else { "TCP" }.to_string(),
                task.profile_label.clone(),
                task.src.pc.clone(),
                task.src.nic.name.clone(),
                task.src.nic.ipv4.clone(),
                task.dst.pc.clone(),
                task.dst.nic.name.clone(),
                task.dst.nic.ipv4.clone(),
                if task.udp {
                    1
                } else {
                    tcp_parallel_streams(&task.extra)
                },
            ),
            LegKind::IperfGroup { name, streams } => {
                if let Some(task) = streams.first() {
                    (
                        "iperf",
                        if task.v6 { "V6" } else { "V4" }.to_string(),
                        "UDP".into(),
                        name.clone(),
                        task.src.pc.clone(),
                        task.src.nic.name.clone(),
                        task.src.nic.ipv4.clone(),
                        task.dst.pc.clone(),
                        task.dst.nic.name.clone(),
                        task.dst.nic.ipv4.clone(),
                        streams.len(),
                    )
                } else {
                    (
                        "iperf",
                        String::new(),
                        "UDP".into(),
                        name.clone(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        0,
                    )
                }
            }
            LegKind::CtsTraffic(task) => (
                "ctstraffic",
                if task.v6 { "V6" } else { "V4" }.to_string(),
                if task.udp { "CTS/UDP" } else { "CTS/TCP" }.to_string(),
                task.profile_label.clone(),
                task.src.pc.clone(),
                task.src.nic.name.clone(),
                task.src.nic.ipv4.clone(),
                task.dst.pc.clone(),
                task.dst.nic.name.clone(),
                task.dst.nic.ipv4.clone(),
                task.streams as usize,
            ),
            LegKind::Ping(_) => return None,
        };
        let tag = if leg.tag.is_empty() {
            "单向"
        } else {
            leg.tag.as_str()
        };
        Some(self.push_row(Row {
            sort_key: (useq, lidx, 0, 0),
            time: now_full(),
            task_id: md5_hex(&format!(
                "{}|{}|{}|direction-result",
                unit.id, leg.tag, backend
            )),
            parent_id: unit.id.clone(),
            task: unit.title.clone(),
            ip,
            transport,
            param,
            src_pc,
            src_iface,
            src_ip,
            dst_pc,
            dst_iface,
            dst_ip,
            verdict,
            execution_status: match verdict {
                Verdict::SetupError => ExecutionStatus::Error,
                Verdict::NotEvaluated => ExecutionStatus::Partial,
                _ => ExecutionStatus::Completed,
            },
            reason_code,
            reason_detail: reason_detail.into(),
            kind_label: if unit.bidir && backend == "ctstraffic" {
                format!("★★双向 CTS Traffic-{tag}")
            } else if unit.bidir {
                format!("★★双向灌包-{tag}")
            } else if backend == "ctstraffic" {
                "CTS Traffic 灌包".into()
            } else {
                "灌包".into()
            },
            requested_streams,
            raws: vec![(
                format!("{tag} 方向执行诊断"),
                format!("[{reason_code}] {reason_detail}"),
            )],
            ..Default::default()
        }))
    }

    fn run_leg(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        leg: &Leg,
        owner_id: &str,
        lease_secs: u64,
    ) -> LegOutcome {
        match &leg.kind {
            LegKind::Ping(t) => self.run_ping_leg(useq, unit, lidx, &leg.tag, t),
            LegKind::IperfSingle(t) => self.run_iperf_single(
                useq,
                unit,
                lidx,
                &leg.tag,
                t,
                LifecycleLease {
                    owner_id,
                    lease_secs,
                },
            ),
            LegKind::CtsTraffic(t) => self.run_ctstraffic_leg(
                useq,
                unit,
                lidx,
                &leg.tag,
                t,
                LifecycleLease {
                    owner_id,
                    lease_secs,
                },
            ),
            LegKind::IperfGroup { .. } => {
                let detail = "UDP 并发组未进入统一调度器（空流组、混合协议或内部任务结构异常）";
                logln(&format!("    [内部调度错误] {detail}"));
                LegOutcome {
                    judgement: VerdictResult::new(
                        Verdict::SetupError,
                        ReasonCode::UdpGroupDispatchError,
                        detail,
                    ),
                    rx_avg: None,
                    main_rows: vec![],
                    tag: leg.tag.clone(),
                }
            }
        }
    }

    // ---------------- ping ----------------
}

mod agent;
mod artifact;
mod cts;
mod db;
mod format;
mod iperf_leg;
mod ping_leg;
mod progress;
mod udp;
mod verdict_assembly;
mod window;

use artifact::*;
use cts::*;
pub use db::{ResultDb, RESUME_MAX_AGE_HOURS};
use format::*;
use progress::*;
use udp::*;
use verdict_assembly::*;
use window::*;

#[cfg(test)]
mod tests;
