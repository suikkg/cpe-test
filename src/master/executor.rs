//! 任务执行器：调度本地/远端的 ping、iperf、监控、截图，产出报告行

use crate::clock::MonotonicClock;
#[cfg(test)]
use crate::clock::{ManualClock, SystemClock};
use crate::cmd::ctstraffic;
use crate::cmd::iperf::{self, IperfClientJobMgr, IperfServerMgr};
use crate::cmd::iperf_window::{
    iperf_active_interval, iperf_baseline_cutoff_ms, iperf_effective_window, iperf_interval_ms,
    WINDOW_COMPLETE_TOLERANCE_MS,
};
use crate::cmd::tools::{find_ctstraffic, find_iperf3};
use crate::config::{Config, RateCheckCfg, RateMode};
use crate::http_client;
use crate::master::builder::{
    v6_addrs, CtsTrafficTask, Endpoint, IperfTask, Leg, LegKind, PingPurpose, PingTask, Side, Unit,
};
#[cfg(test)]
use crate::master::rate_window::rate_excursion;
use crate::master::rate_window::{
    evaluate_rx_acceptance, monitor_rate_stats, nearest_valid_sample, percentile, EffectiveWindow,
    RateStats, MIN_VALID_RX_MBPS,
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
    /// 对端 agent 是否声明了 [`crate::protocol::PING_DF_CAPABILITY`]。
    ///
    /// 路径 MTU 探测必须先看它：旧版 agent 收到 `dont_fragment` 会**静默忽略**、
    /// 照常分片发出去并报成功，据此得到的「大包能过」是个听上去很确定的错答案。
    /// `false` 时探测不跑，诊断里写明原因——宁可没有结果，也不要一个不能信的数。
    pub agent_ping_df: bool,
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
    /// 判定 → 计数的**唯一映射**，与 `RunCounts::bump` 同名同义。
    ///
    /// 历史上这两处各写一份，`fail` 于是长成了两个意思：CLI 这边把
    /// NOT_EVALUATED / SETUP_ERROR 也累加进 `fail`（当成「没过」的汇总），
    /// 控制台那边只数 RATE_FAIL。同一轮运行，命令行报「FAIL: 2」而控制台报
    /// 「0 失败、2 搭建错误」——两个出口对同一个词给出不同的数。
    /// `counters_mean_the_same_thing_on_both_exits` 现在盯着这件事。
    pub fn bump(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Pass => self.pass += 1,
            Verdict::RateFail => self.fail += 1,
            Verdict::Measured => self.measured += 1,
            Verdict::NotEvaluated => self.not_evaluated += 1,
            Verdict::SetupError => self.setup_error += 1,
            Verdict::Skip => self.skip += 1,
        }
    }

    /// 「这一轮有没有出问题」——退出码用它。
    ///
    /// 刻意**不等于** `fail > 0`：把 NOT_EVALUATED / SETUP_ERROR 折进 `fail`
    /// 会让那个词说谎，但退出码本来就该对这三种一视同仁（跑坏了的一轮不能
    /// 因为「只是没判成」而返回 0）。所以分成两件事说。
    pub fn any_not_passed(&self) -> usize {
        self.fail + self.not_evaluated + self.setup_error
    }

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

/// 灌包「死流」的两层熔断计数器。**纯状态机**：不碰进程、不碰网络、不碰行。
///
/// 抽出来是因为分组这一层**测不到**：它与全局那一层的区别只在「有链路还活着」
/// 时才显现，而那需要真实流量。放在这里，它就是一个可以穷举的状态机。
///
/// 两层共用同一个阈值 `threshold`（`Config::abort_after_dead_traffic_units`，
/// `0` = 两层都关）：
///
/// - **全局**：连续 `threshold` 个灌包单元一条测量都没产生 → 中止整个剩余队列。
/// - **按链路**：某条链路连续 `threshold` 个 → 只放弃这条链路的剩余单元。
///
/// 只有全局那一层的时候这个功能基本不会触发：5 条链路交替排队、1 条彻底断掉
/// 而 4 条正常，任何一个跑出数的单元都会把全局计数清零。而「区分不了设备掉线
/// 和某一对网口本来就不通」正是默认值取 0 的理由——分组之后这两件事分得开了。
#[derive(Debug, Default)]
struct DeadTrafficBreaker {
    threshold: usize,
    global: usize,
    max_global: usize,
    per_group: HashMap<String, usize>,
    abandoned: HashSet<String>,
}

impl DeadTrafficBreaker {
    fn new(threshold: usize) -> Self {
        Self {
            threshold,
            ..Default::default()
        }
    }

    /// 整个剩余队列是否该停。`threshold == 0` 时恒为 false（只告警不中止）。
    fn should_abort_all(&self) -> bool {
        self.threshold > 0 && self.global >= self.threshold
    }

    /// 这条链路是否已经被放弃。空链路键**永远不算**——键为空意味着分不出组，
    /// 拿它当一个组会把一批互不相干的链路一起放弃。
    fn is_abandoned(&self, group: &str) -> bool {
        !group.is_empty() && self.abandoned.contains(group)
    }

    /// 这个灌包单元产生了可用测量：两层计数一起清零。
    fn record_usable(&mut self, group: &str) {
        self.global = 0;
        if !group.is_empty() {
            self.per_group.insert(group.to_string(), 0);
        }
    }

    /// 这个灌包单元一条测量都没产生。
    ///
    /// 返回 `true` 表示**这一次**让该链路刚好越过阈值（只在第一次返回 true，
    /// 供调用方打一条日志而不是每个单元都刷一遍）。
    fn record_dead(&mut self, group: &str) -> bool {
        self.global += 1;
        self.max_global = self.max_global.max(self.global);
        if self.threshold == 0 || group.is_empty() {
            return false;
        }
        let streak = self.per_group.entry(group.to_string()).or_insert(0);
        *streak += 1;
        *streak >= self.threshold && self.abandoned.insert(group.to_string())
    }

    fn global_streak(&self) -> usize {
        self.global
    }

    fn max_global_streak(&self) -> usize {
        self.max_global
    }

    fn group_streak(&self, group: &str) -> usize {
        self.per_group.get(group).copied().unwrap_or(0)
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
        // 熔断的全部状态都在这个纯状态机里（两层、共用一个阈值），见
        // [`DeadTrafficBreaker`]。执行循环只负责「问它」和「告诉它结果」。
        let mut breaker = DeadTrafficBreaker::new(self.cfg.abort_after_dead_traffic_units);
        for (i, unit) in units.iter().enumerate() {
            // 单元边界：先看是不是「只跳过刚才那一个」。
            //
            // 跳过复用整轮取消那套收尾路径（停远端作业、回收端口、收日志），
            // 所以它也设了取消位；在这里把它清掉队列才能继续。
            // `resume_after_skip` 里「停止和进程退出优先」那一条挡住了竞态：
            // 跳过之后紧接着点停止，不会被这次清零抹掉。
            // 兜底：跳过请求在单元收尾**之后**才落地（操作员点得晚了一拍）时，
            // 上面那次取走会落空，取消位就留到了这里。在开跑下一个之前清掉它，
            // 否则一次点晚了的「跳过」会把整个队列停掉。
            if crate::cancel::take_skip_unit() && crate::cancel::resume_after_skip() {
                logln("  (已按请求跳过，队列继续)");
            }
            if crate::cancel::is_cancelled() {
                logln("\n!! 用户中断 (Ctrl+C)，正在生成部分报告...");
                break;
            }
            // 熔断检查放在循环开头而不是结尾：单元有多条 `continue` 提前退出的
            // 路径（resume 命中、前置拦截、网卡消失），放在结尾时那些路径会
            // 整个跳过它。而「网卡消失」恰恰是本设置最该拦住的场景——被测设备
            // 掉线后每个单元的开跑前重扫都会看到网卡不见了，队列会一路空转到底。
            // 日志里要报出阈值，从状态机自己身上取——执行循环再读一次
            // `self.cfg` 就是又开了一条旁路，分组那一层当初就是这么漏掉的。
            let abort_at = breaker.threshold;
            if breaker.should_abort_all() {
                logln(&format!(
                    "\n!! 连续 {} 个灌包单元没有产生任何测量，按 abort_after_dead_traffic_units={abort_at} 中止剩余 {} 个单元。\n\
                     !! 请先确认被测设备是否掉线或重启，再重跑剩余项；已完成的部分会照常出报告。",
                    breaker.global_streak(),
                    total.saturating_sub(i)
                ));
                // 中止点必须是全局序号：诊断补跑那一趟的 `sequence_offset` 是
                // 主队列长度，用局部 `i` 会把「第 147 个单元后中止」写成「第 2 个」，
                // 进度页和报告横幅会一起指错位置。
                let aborted_at = sequence_offset + i;
                sum.aborted_at_unit = Some(aborted_at);
                self.notify(|observer| observer.run_aborted(aborted_at));
                break;
            }
            let useq = sequence_offset + i;
            let is_traffic_unit = unit_has_traffic(unit);
            // 灌包单元的计数必须在「链路已放弃」那条早退分支**之前**加。
            // 放在后面的话，被放弃的单元只进 `traffic_setup_errors` 不进
            // `traffic_units`，两个计数器就发散了——`ui.rs` 的收尾文案会打出
            // 「本轮 2 个灌包单元没有产生任何有效速率测量（其中 SETUP_ERROR=50）」
            // 这种自相矛盾的一行，`needs_traffic_failure_diagnostics()` 也跟着少数。
            if is_traffic_unit {
                sum.traffic_units += 1;
            }
            let link_key = crate::master::executor::row::link_group_key(unit, None);
            if is_traffic_unit && breaker.is_abandoned(&link_key) {
                let detail = format!(
                    "链路「{link_key}」已连续 {abort_at} 个灌包单元没有产生任何测量，\
                     本轮不再对它起流；其余链路照常继续"
                );
                logln(&format!(
                    "\n[{}/{}] {}\n  !! {detail}",
                    i + 1,
                    total,
                    unit.title
                ));
                sum.bump(Verdict::SetupError);
                sum.traffic_setup_errors += 1;
                self.push_row(Row {
                    verdict: Verdict::SetupError,
                    execution_status: ExecutionStatus::Error,
                    reason_code: ReasonCode::LinkAbandoned,
                    reason_detail: detail.clone(),
                    ..unit_row(unit, useq, "跳过(链路已放弃)")
                });
                self.persist_new_rows();
                self.notify(|observer| {
                    observer.unit_finished(
                        UnitStatus {
                            seq: useq + 1,
                            title: unit.title.clone(),
                            verdict: Verdict::SetupError.label().to_string(),
                            reason_code: ReasonCode::LinkAbandoned.as_str().to_string(),
                            reason_detail: detail.clone(),
                            skipped: false,
                            secs: 0,
                            link_group: unit.link_group.clone(),
                            // 没起过流，没有实测值可言。
                            rx_avg: None,
                            target_mbps: None,
                        },
                        Self::remaining_est_secs(units, i + 1),
                    )
                });
                continue;
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
                            sum.bump(Verdict::SetupError);
                            if is_traffic_unit {
                                sum.traffic_setup_errors += 1;
                                // 网卡消失也是「这条链路过不去流量」的一种，
                                // 两层计数都要跟着走：否则一条被拔线的链路
                                // 会一路空转到队列结束。
                                breaker.record_dead(&link_key);
                                sum.max_dead_traffic_streak = breaker.max_global_streak();
                            }
                            self.push_row(Row {
                                verdict: Verdict::SetupError,
                                execution_status: ExecutionStatus::Error,
                                reason_code: ReasonCode::NicDisappeared,
                                reason_detail: detail,
                                ..unit_row(unit, useq, "跳过(网卡已消失)")
                            });
                            // 这条路径不会进入下面的普通 unit 收尾，但它已经是一个
                            // 完整的处理结果；否则进程若此刻退出，增量 JSONL 会漏掉
                            // 这条「网卡消失」记录，重放报告与进度页不一致。
                            self.persist_new_rows();
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
                                        // 这条路径压根没起过流，没有实测值可言。
                                        rx_avg: None,
                                        target_mbps: None,
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
                        ..unit_row(unit, useq, format!("跳过(上次PASS: {t})"))
                    });
                    // RESUME 跳过也是已经处理完的单元；不能因为没有起流就让
                    // 增量 JSONL 少这一行，尤其是长队列中途崩溃时。
                    self.persist_new_rows();
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
                                // 这条路径压根没起过流，没有实测值可言。
                                rx_avg: None,
                                target_mbps: None,
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

            // 双向合计门限存在时，**判定在单元级只做一次**：AB 接收端 RX +
            // BA 接收端 RX 与门限比一次。两条腿此时本来就没有各自的门限
            // （builder 的 `leg_rate_plan` 已经把它们落到 Observe），所以按腿
            // 聚合出来的只会是 MEASURED——真正的结论必须在这里给。
            //
            // 合计形不成时（有一条腿是 SETUP_ERROR / 采样不可信）退回按腿聚合：
            // 那条链能说出到底是哪条腿、什么原因，比一句「合计缺数据」有用。
            // 腿级聚合永远算一遍：合计判定要不要让位给它，取决于它是不是更具体。
            let aggregated = aggregate_unit_verdict(&outcomes);
            let bidir_total = unit
                .bidir_total_target_mbps
                .map(|target| bidir_total_verdict(&outcomes, target))
                // 腿级的 SETUP_ERROR / NOT_EVALUATED 说得出「哪条腿、什么原因」，
                // 比一句「合计缺数据」有用，所以让它说话。除此之外一律由合计拍板
                // ——包括合计自己判 NOT_EVALUATED（缺一个方向就是形不成合计，
                // 这时**不许**退回两条腿各自的 MEASURED 假装一切正常）。
                .filter(|_| !matches!(aggregated, Verdict::SetupError | Verdict::NotEvaluated));
            let unit_verdict = bidir_total
                .as_ref()
                .map(|judgement| judgement.verdict)
                .unwrap_or(aggregated);
            if is_traffic_unit {
                let usable =
                    blocked.is_none() && self.outcomes_have_usable_traffic_measurement(&outcomes);
                if usable {
                    sum.traffic_usable_units += 1;
                    // 两层计数一起清零。别的链路不受影响——那正是分组的意义。
                    breaker.record_usable(&link_key);
                } else {
                    // 「一条测量都没产生」和「测出来不达标」是两回事，这里只数前者。
                    let newly_abandoned = breaker.record_dead(&link_key);
                    sum.max_dead_traffic_streak = breaker.max_global_streak();
                    if newly_abandoned {
                        logln(&format!(
                            "  !! 链路「{link_key}」已连续 {} 个灌包单元没有产生任何测量，\
                             按 abort_after_dead_traffic_units={abort_at} 放弃这条链路的剩余单元；\
                             其余链路继续。请单独确认这一对网口的连通性。",
                            breaker.group_streak(&link_key)
                        ));
                    }
                    if breaker.global_streak() >= DEAD_TRAFFIC_STREAK_WARN {
                        logln(&format!(
                            "  !! 连续 {} 个灌包单元没有产生任何测量——被测设备可能已掉线。\
                             后续单元大概率也是空跑；要自动中止请设 abort_after_dead_traffic_units。",
                            breaker.global_streak()
                        ));
                    }
                }
                if unit_verdict == Verdict::SetupError {
                    sum.traffic_setup_errors += 1;
                }
            }
            let unit_reason = outcome_matching_verdict(&outcomes, unit_verdict);
            let bidir_total_target = unit.bidir_total_target_mbps;
            let unit_ok = unit_verdict.is_pass();
            sum.bump(unit_verdict);
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
            // 诊断按腿汇总到单元行：判定只有一份，排障线索要能在概览上一次看全。
            let mut unit_diagnostics: Vec<String> = outcomes
                .iter()
                .flat_map(|outcome| {
                    let tag = outcome.tag.clone();
                    outcome.judgement.diagnostics.iter().map(move |line| {
                        if tag.is_empty() {
                            line.clone()
                        } else {
                            format!("{tag}: {line}")
                        }
                    })
                })
                .collect();
            if let Some(judgement) = &bidir_total {
                unit_diagnostics.extend(judgement.diagnostics.clone());
            }
            // 「人按了跳过」和「设备真的不行」在报告上必须分得开：这一行的
            // SETUP_ERROR 全部来自被主动掐断的作业，不是被测设备的结论。
            //
            // 走**诊断**通道而不是改写判定：判定说的是「这次跑出了什么」，
            // 而这一行确实什么都没跑出来。改成 PASS/SKIP 就是在判定之后再叠
            // 一层，正是 ADR-17 一直在防的方向。
            if crate::cancel::take_skip_unit() && crate::cancel::resume_after_skip() {
                unit_diagnostics.insert(
                    0,
                    "本单元被操作员手动跳过：下面的失败来自被主动掐断的作业，\
                     不是被测设备的结论。要复测请单独重跑这一条。"
                        .into(),
                );
                logln("  (已按请求跳过本单元，队列继续)");
            }
            // 单元级「结论的理由」只算一次，报告行和进度页共用。
            //
            // 这两处以前各算各的：报告行走合计判定，进度页走 `unit_reason` /
            // `reasons.first()` 的腿级理由。真机联调当场撞上——双向 UDP 单元
            // 判定是 PASS，进度页却写着「ab:TARGET_UNKNOWN …因此不标记 PASS」。
            // 腿本来就不该有目标（合计门限存在时 `leg_rate_plan` 把两条腿都落到
            // Observe），那句话在单元这一层是自相矛盾的。
            let unit_reason_code = bidir_total
                .as_ref()
                .map(|judgement| judgement.code)
                .or_else(|| unit_reason.map(|outcome| outcome.reason_code()))
                .unwrap_or_default();
            let bidir_reason_detail = bidir_total.as_ref().map(|judgement| {
                if judgement.detail.is_empty() {
                    judgement.diagnostics.join("；")
                } else {
                    judgement.detail.clone()
                }
            });
            let direction_summaries = self.direction_summaries(&outcomes);
            let single_direction = (direction_summaries.len() == 1)
                .then(|| direction_summaries.first())
                .flatten();
            let stream_counts = aggregate_direction_streams(&direction_summaries);
            // 单元级的「实测 / 目标」在这里算一次，报告汇总行和进度页共用。
            //
            // 进度页此前只有 verdict 和原因码：盯着一轮 11.5 小时的测试，屏幕上
            // 是一串 PASS/PASS/PASS，看不到「第 47 个单元 1850 对 1800」。而
            // 「数值在一路往下滑」（热衰减、Wi-Fi 退避）恰恰是要当场发现、当场
            // 停下来的那类现象——等出了报告再看出来，已经白跑了十几个小时。
            //
            // 两处各算一遍的话，进度页和报告会对同一个单元报两个数，
            // 而那种不一致没人会去核对。
            let unit_rx_avg = bidir_total
                .is_some()
                .then(|| bidir_total_rx_avg(&outcomes))
                .flatten()
                .or_else(|| single_direction.and_then(|direction| direction.rx_avg));
            let unit_target_mbps = bidir_total_target
                .or_else(|| single_direction.and_then(|direction| direction.target_mbps));
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
                reason_code: unit_reason_code,
                reason_detail: bidir_reason_detail
                    .clone()
                    .unwrap_or_else(|| reasons.join(" | ")),
                diagnostics: unit_diagnostics,
                requested_streams: stream_counts.map_or(0, |counts| counts.requested),
                active_streams: stream_counts.map_or(0, |counts| counts.active),
                required_streams: stream_counts.map_or(0, |counts| counts.required),
                // 双向合计单元的「RX 平均」就是判定用的那个合计值。填
                // `single_direction`（双向恒为 None）会让报告出现「目标 1000 /
                // RX 平均 空」这种自相矛盾的一行。
                rx_avg: unit_rx_avg,
                rx_p10: single_direction.and_then(|direction| direction.rx_p10),
                // 双向合计单元的「目标」就是那个合计门限——两条腿各自没有目标，
                // 报告上必须能看到判定用的是哪个数。
                target_mbps: unit_target_mbps,
                sample_coverage: single_direction.and_then(|direction| direction.sample_coverage),
                udp_loss: single_direction.and_then(|direction| direction.udp_loss),
                // TCP 重传和 udp_loss / ping_loss 是同一类东西（「质量」列的三选一），
                // 必须跟着一起上汇总行：`report::model::verdict_row` 优先返回汇总行，
                // 所以只填在方向明细上等于 HTML 概览和 summary.xlsx 的「TCP 重传」
                // 列对每一行都空着——正是 ADR-7 记下的那类静默空列。
                tcp_retransmits: single_direction.and_then(|direction| direction.tcp_retransmits),
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
                        reason_code: unit_reason_code.as_str().to_string(),
                        // 失败清单一行一条，多腿的原因用 " | " 连起来的整串
                        // 太长；这里只留第一段，完整的在报告里。合计拍板时那句
                        // 话本身就是完整的一条，不再截取。
                        reason_detail: bidir_reason_detail
                            .clone()
                            .unwrap_or_else(|| reasons.first().cloned().unwrap_or_default()),
                        skipped: false,
                        secs: self.clock.now().duration_since(unit_started_at).as_secs(),
                        link_group: unit.link_group.clone(),
                        rx_avg: unit_rx_avg,
                        target_mbps: unit_target_mbps,
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
use self::latency::LoadLatency;

mod iperf_leg;
mod latency;
mod ping_leg;
mod pmtu;
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
pub(crate) use udp::required_udp_streams;
use udp::*;
use verdict_assembly::*;
use window::*;

#[cfg(test)]
mod tests;
