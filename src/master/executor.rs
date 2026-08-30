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
    v6_addrs, CtsTrafficTask, Endpoint, IperfTask, Leg, LegKind, PingPurpose, PingTask, Side, Unit,
};
use crate::master::rate_window::{
    evaluate_nic_rx, monitor_rate_stats, nearest_valid_sample, percentile, rate_excursion,
    rate_sample_coverage_sufficient, rate_window_coverage_sufficient, EffectiveWindow, RateStats,
    MIN_RATE_SAMPLE_COVERAGE, MIN_VALID_RX_MBPS,
};
use crate::master::run_status::{CurrentUnit, RunObserver, UnitStatus};
use crate::nic::monitor::MonitorMgr;
use crate::ping;
use crate::protocol::*;
use crate::reason::ReasonCode;
use crate::report::{report_reason, DirectionSummary, Row, RowBackend, RowProtocol, StreamCounts};
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

/// 单流 UDP 是基础连通性硬门槛：初次尝试加至少两次重试。
///
/// 归在执行器而不是 builder：它描述的是**执行期的重试预算**，不是计划的形状。
/// 放在 builder 里会让人以为它参与单元展开或 resume identity（都不参与）。
const SINGLE_UDP_MIN_ATTEMPTS: u64 = 3;
const UDP_SERVER_START_RETRIES: usize = 1;
/// 认定「这条流还活着」时允许的事件间隔。与窗口完整性无关，别混用。
const FLOW_TIMELINE_TOLERANCE_MS: u64 = 2_000;
/// **有效窗口是否算完整**时允许的收尾误差，三条链共用（ADR-12）。
///
/// 名字里没有后端：它以前叫 `CTS_TIMELINE_TOLERANCE_MS`，而 iperf 路径也在用
/// 它——「iperf 用着一个名叫 CTS 的常量」本身就是这层已经分叉的症状。
///
/// 更要紧的是 UDP 路径**根本没用它**（零容差）：一条跑了 179.95 秒、要求 180 秒
/// 的 UDP 腿判 `EFFECTIVE_WINDOW_SHORT`，而同样的 TCP 腿 PASS。50 毫秒的收尾
/// 差异不是测量事实的差异，是三条链各自决定容差的结果。
const WINDOW_COMPLETE_TOLERANCE_MS: u64 = 100;
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
    /// 本次运行目录（`runs/run_...`）。`outdir` 是它下面的 `iperf_outputs/`。
    ///
    /// 结果落盘走这里而不是 `outdir`：`rows.jsonl` / `meta.json` 是**整个 run 的**
    /// 数据，和报告 HTML 平级；`iperf_outputs/` 装的是逐条工具输出与样本 CSV。
    /// `cpe_test report <run 目录>` 的入参就是这个目录。
    pub run_dir: PathBuf,
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
    /// 结构化运行状态的汇报口（ADR-2）。
    ///
    /// `None` = 没人要听（命令行直跑）。回调点全部挂在**既有的** `logln` 处，
    /// 所以这里不引入任何新状态机；`None` 时行为与加这个字段之前逐字节相同。
    pub observer: Option<Arc<dyn RunObserver>>,
    /// 已经追加进 `rows.jsonl` 的行数（ADR-3）。
    ///
    /// `rows` 只增不删，所以一个游标就够：每个单元结束时把 `rows[cursor..]`
    /// 追加落盘，再把游标推到末尾。
    pub persisted_rows: Mutex<usize>,
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

    /// 把上一次落盘之后新增的行追加进 `runs/<run>/rows.jsonl`。
    ///
    /// 在每个单元结束时调用（与 `db.save()` 同一时机、同一个理由）。
    /// **失败只告警不中断**：收尾动作不许弄死测试——磁盘满的时候，正在跑的
    /// 那一轮还有价值，不该因为写不了副本而中止。
    fn persist_new_rows(&self) {
        let (pending, next_cursor) = {
            let rows = lock_recover(&self.rows);
            let cursor = *lock_recover(&self.persisted_rows);
            if cursor >= rows.len() {
                return;
            }
            (rows[cursor..].to_vec(), rows.len())
        };
        match crate::report::store::append_rows(&self.run_dir, &pending) {
            Ok(()) => *lock_recover(&self.persisted_rows) = next_cursor,
            Err(error) => {
                // 游标**不推进**：下个单元会把这一批一起重试。
                logln(&format!("  (结果增量落盘失败，本轮继续: {error})"));
            }
        }
    }

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

    /// 有 observer 就调它，没有就什么都不做。
    ///
    /// 回调里 panic 不该弄死测试——这是「收尾/旁路动作不许弄死测试」这条既有
    /// 纪律的延续（Excel 生成失败、rows.jsonl 追加写失败都是同样的处理）。
    fn notify(&self, call: impl FnOnce(&dyn RunObserver)) {
        let Some(observer) = self.observer.as_ref() else {
            return;
        };
        if catch_unwind(AssertUnwindSafe(|| call(observer.as_ref()))).is_err() {
            logln("  (进度回调 panic，已忽略；测试继续)");
        }
    }

    /// 从第 `next_index` 个单元起，剩下的估算耗时之和。
    ///
    /// `est_secs` 的唯一实现在 builder，这里只做求和——前端不复算，免得出现
    /// 「界面说还剩 2 小时、日志说还剩 3 小时」这种两边各算一份的经典问题。
    fn remaining_est_secs(units: &[Unit], next_index: usize) -> u64 {
        units
            .iter()
            .skip(next_index)
            .map(|unit| unit.est_secs)
            .sum()
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
                self.notify(|observer| observer.run_aborted(i));
                break;
            }
            let useq = sequence_offset + i;
            let is_traffic_unit = unit_has_traffic(unit);
            if is_traffic_unit {
                sum.traffic_units += 1;
            }
            let blocked = preflight_blocks.and_then(|blocks| blocks.get(&unit.id));
            logln(&format!("\n[{}/{}] {}", i + 1, total, unit.title));
            // 结构化事件挂在这条 logln 旁边——同一个状态转移点，两个出口：
            // 文本给人看，`RunStatus` 给机器读。日志文案因此可以自由改。
            let unit_started_at = self.clock.now();
            self.notify(|observer| {
                observer.unit_started(CurrentUnit {
                    seq: useq + 1,
                    title: unit.title.clone(),
                    est_secs: unit.est_secs,
                    started_at: now_full(),
                    link_group: unit.link_group.clone(),
                })
            });

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
                                verdict: Verdict::SetupError,
                                execution_status: ExecutionStatus::Error,
                                reason_code: ReasonCode::NicDisappeared,
                                reason_detail: detail,
                                ..unit_row(
                                    unit,
                                    useq,
                                    "跳过(网卡已消失)",
                                    RowProtocol::None,
                                    RowBackend::None,
                                )
                            });
                            // 同上：这条 `continue` 也绕过了 unit_finished。
                            self.notify(|observer| {
                                observer.unit_finished(
                                    UnitStatus {
                                        seq: useq + 1,
                                        title: unit.title.clone(),
                                        verdict: Verdict::SetupError.label().to_string(),
                                        reason_code: ReasonCode::NicDisappeared
                                            .as_str()
                                            .to_string(),
                                        reason_detail: gone.describe(),
                                        skipped: false,
                                        secs: 0,
                                        link_group: unit.link_group.clone(),
                                    },
                                    Self::remaining_est_secs(units, i + 1),
                                )
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
                        verdict: Verdict::Skip,
                        execution_status: ExecutionStatus::Skipped,
                        reason_code: ReasonCode::ResumeFreshPass,
                        reason_detail: format!(
                            "复用 {t} 的正式 PASS；本轮启用 resume，且结果未超过 {RESUME_MAX_AGE_HOURS} 小时，因此跳过执行"
                        ),
                        ..unit_row(
                            unit,
                            useq,
                            format!("跳过(上次PASS: {t})"),
                            RowProtocol::None,
                            RowBackend::None,
                        )
                    });
                    // 这条路径 `continue` 掉了，不会走到下面那个 unit_finished，
                    // 所以在这里补一次——进度页上「跳过」也是一个已完成单元。
                    self.notify(|observer| {
                        observer.unit_finished(
                            UnitStatus {
                                seq: useq + 1,
                                title: unit.title.clone(),
                                verdict: Verdict::Skip.label().to_string(),
                                reason_code: ReasonCode::ResumeFreshPass.as_str().to_string(),
                                reason_detail: format!("复用 {t} 的正式 PASS"),
                                skipped: true,
                                secs: 0,
                                link_group: unit.link_group.clone(),
                            },
                            Self::remaining_est_secs(units, i + 1),
                        )
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
                // 单元汇总行永远排在本单元所有明细之后。
                sort_key: (useq, usize::MAX, usize::MAX, u8::MAX),
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
                ..unit_row(
                    unit,
                    useq,
                    if unit.bidir {
                        "测试单元汇总(双向)"
                    } else {
                        "测试单元汇总"
                    },
                    RowProtocol::None,
                    RowBackend::None,
                )
            });
            {
                let mut db = lock_recover(&self.db);
                db.set(&unit.id, unit_ok, &unit.title);
                db.save();
            }
            // 结果增量落盘：与 `db.save()` 同一时机。频率是分钟级、体量是 KB 级，
            // 对正在灌线速的机器没有可测量的影响；换来的是「崩溃 = 只损失未完成
            // 的单元」而不是「崩溃 = 整轮全损」。
            self.persist_new_rows();
            // 与 `db.set` 同一个转移点：单元有结论了。
            self.notify(|observer| {
                observer.unit_finished(
                    UnitStatus {
                        seq: useq + 1,
                        title: unit.title.clone(),
                        verdict: unit_verdict.label().to_string(),
                        reason_code: unit_reason
                            .map(|outcome| outcome.reason_code().as_str().to_string())
                            .unwrap_or_default(),
                        // 失败清单一行一条，多腿的原因用 " | " 连起来的整串
                        // 太长；这里只留第一段，完整的在报告里。
                        reason_detail: reasons.first().cloned().unwrap_or_default(),
                        skipped: false,
                        secs: self.clock.now().duration_since(unit_started_at).as_secs(),
                        link_group: unit.link_group.clone(),
                    },
                    Self::remaining_est_secs(units, i + 1),
                )
            });
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
                // 指标部分只有一份实现：`Row::direction_summary()`。这里只覆盖
                // 那四项执行侧更权威的字段——腿的判定结果比从行里反推准确
                // （行可能是组合计，也可能因为重试有多条）。
                let mut summary = row.direction_summary();
                summary.tag = if outcome.tag.is_empty() {
                    "单向".into()
                } else {
                    outcome.tag.to_ascii_uppercase()
                };
                summary.verdict = outcome.verdict();
                summary.reason_code = outcome.reason_code();
                summary.reason_detail = outcome.reason_detail().to_string();
                summary.reason = report_reason(outcome.reason_code(), outcome.reason_detail());
                Some(summary)
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
        // 端点不再从这里手抄成 6 个字符串：`base_row` 从 `Endpoint` 一次填齐，
        // 顺带把类型化的 `src_side`/`dst_side`/`link_group` 也带上。
        // 唯一取不到端点的情况是空的 UDP 组（理论上不该有），那时回落到单元的腿。
        let (backend, backend_kind, ip, transport, protocol, param, requested_streams) =
            match &leg.kind {
                LegKind::IperfSingle(task) => (
                    "iperf",
                    RowBackend::Iperf3,
                    if task.v6 { "V6" } else { "V4" }.to_string(),
                    if task.udp { "UDP" } else { "TCP" }.to_string(),
                    if task.udp {
                        RowProtocol::Udp
                    } else {
                        RowProtocol::Tcp
                    },
                    task.profile_label.clone(),
                    if task.udp {
                        1
                    } else {
                        tcp_parallel_streams(&task.extra)
                    },
                ),
                LegKind::IperfGroup { name, streams } => (
                    "iperf",
                    RowBackend::Iperf3,
                    match streams.first() {
                        Some(task) if task.v6 => "V6".to_string(),
                        Some(_) => "V4".to_string(),
                        None => String::new(),
                    },
                    "UDP".into(),
                    RowProtocol::Udp,
                    name.clone(),
                    streams.len(),
                ),
                LegKind::CtsTraffic(task) => (
                    "ctstraffic",
                    RowBackend::CtsTraffic,
                    if task.v6 { "V6" } else { "V4" }.to_string(),
                    if task.udp { "CTS/UDP" } else { "CTS/TCP" }.to_string(),
                    if task.udp {
                        RowProtocol::Udp
                    } else {
                        RowProtocol::Tcp
                    },
                    task.profile_label.clone(),
                    task.streams as usize,
                ),
                LegKind::Ping(_) => return None,
            };
        let endpoints = match &leg.kind {
            LegKind::IperfSingle(task) => Some((&task.src, &task.dst)),
            LegKind::IperfGroup { streams, .. } => {
                streams.first().map(|task| (&task.src, &task.dst))
            }
            LegKind::CtsTraffic(task) => Some((&task.src, &task.dst)),
            LegKind::Ping(_) => return None,
        };
        let Some((src, dst)) = endpoints else {
            // 空的 UDP 组没有端点可言，这一行也就没有可展示的链路。
            return None;
        };
        let tag = if leg.tag.is_empty() {
            "单向"
        } else {
            leg.tag.as_str()
        };
        let kind_label = if unit.bidir && backend == "ctstraffic" {
            format!("★★双向 CTS Traffic-{tag}")
        } else if unit.bidir {
            format!("★★双向灌包-{tag}")
        } else if backend == "ctstraffic" {
            "CTS Traffic 灌包".into()
        } else {
            "灌包".to_string()
        };
        Some(self.push_row(Row {
            // CTS 的可见 transport 列写成 `CTS/TCP`，后端信息在里面；
            // 类型化之后后端进了 `backend`，可见列保持不变。
            transport,
            verdict,
            execution_status: match verdict {
                Verdict::SetupError => ExecutionStatus::Error,
                Verdict::NotEvaluated => ExecutionStatus::Partial,
                _ => ExecutionStatus::Completed,
            },
            reason_code,
            reason_detail: reason_detail.into(),
            requested_streams,
            raws: vec![(
                format!("{tag} 方向执行诊断"),
                format!("[{reason_code}] {reason_detail}"),
            )],
            ..base_row(RowIdentity {
                unit_seq: useq,
                leg_index: lidx,
                stream_index: 0,
                group_flag: 0,
                unit,
                leg_tag: &leg.tag,
                src,
                dst,
                ip,
                protocol,
                backend: backend_kind,
                param,
                kind_label,
                task_id: md5_hex(&format!(
                    "{}|{}|{}|direction-result",
                    unit.id, leg.tag, backend
                )),
            })
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
mod row;

use row::{base_row, unit_row, RowIdentity};
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
