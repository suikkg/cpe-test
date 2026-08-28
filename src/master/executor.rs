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
    evaluate_nic_rx, monitor_rate_stats, nearest_valid_sample, percentile,
    rate_sample_coverage_sufficient, rate_window_coverage_sufficient, rx_dropout, EffectiveWindow,
    RateStats, MIN_RATE_SAMPLE_COVERAGE, MIN_VALID_RX_MBPS,
};
use crate::nic::monitor::MonitorMgr;
use crate::ping;
use crate::protocol::*;
use crate::report::{report_endpoint, report_reason, DirectionSummary, Row, StreamCounts};
use crate::util::{lock_recover, logln, md5_hex, now_compact, now_full, sanitize};
use crate::verdict::{aggregate_verdict, ExecutionStatus, Verdict};
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
    pub reason_code: String,
    pub reason_detail: String,
}

#[derive(Debug)]
struct LegOutcome {
    verdict: Verdict,
    reason_code: String,
    reason_detail: String,
    rx_avg: Option<f64>,
    main_rows: Vec<usize>,
    tag: String,
}

fn preflight_block_outcome(tag: &str, block: &IperfPreflightBlock) -> LegOutcome {
    LegOutcome {
        verdict: Verdict::SetupError,
        reason_code: block.reason_code.clone(),
        reason_detail: block.reason_detail.clone(),
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
                verdict: Verdict::SetupError,
                reason_code: "UNIT_PANIC".into(),
                reason_detail: detail,
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
            verdict: Verdict::SetupError,
            reason_code: "RESOURCE_CLEANUP_FAILED".into(),
            reason_detail: error,
            rx_avg: None,
            main_rows: vec![],
            tag: "cleanup".into(),
        });
    }
    outcomes
}

#[derive(Clone)]
struct UdpLegPlan {
    lidx: usize,
    tag: String,
    name: String,
    streams: Vec<IperfTask>,
}

#[derive(Clone)]
struct PreparedUdpFlow {
    leg_pos: usize,
    stream_pos: usize,
    /// 方向标签（ab/ba，单向为空）。双向两腿并行时，日志必须能区分是哪一腿的
    /// attempt/retry，否则 master.log 里两个 #1 完全分不开。
    tag: String,
    task: IperfTask,
    server_req: Option<IperfServerStartReq>,
    client_req: Option<IperfClientReq>,
    server_error: String,
    launch_delay_ms: u64,
    strict_single_stream: bool,
}

struct UdpFlowRun {
    leg_pos: usize,
    stream_pos: usize,
    task: IperfTask,
    /// 本轮选中 attempt 是否有 iperf3 client/server 自身吞吐证据。
    raw_ok: bool,
    /// 已有工具测量，但 client 非正常完成/超时；不能再伪装成“无测量”。
    runtime_failed: bool,
    parsed: iperf::IperfParsed,
    client: IperfClientOut,
    server_output: String,
    events: Vec<IperfFlowEvent>,
    retries: usize,
    /// 实际启动 client 的完整外层尝试次数（不含 iperf3 内部瞬态重试）。
    full_attempts: usize,
    /// 单流方向已在每次资源清理均确认的前提下耗尽强制尝试预算。
    single_stream_exhausted: bool,
    error: String,
}

struct CtsAttemptRun {
    attempt: usize,
    client: IperfClientOut,
    server_output: String,
    server_unexpected_failure: bool,
    traffic_window: EffectiveWindow,
    events: Vec<IperfFlowEvent>,
    parsed: ctstraffic::CtsTrafficParsed,
    traffic_established: bool,
    full_attempt: bool,
    cleanup_confirmed: bool,
    setup_error: Option<(String, String)>,
}

struct CtsClientRun {
    client: IperfClientOut,
    started: bool,
    cleanup_confirmed: bool,
    setup_error: Option<(String, String)>,
}

#[derive(Debug, Clone)]
struct CtsMonitorIssue {
    code: String,
    detail: String,
    setup_error: bool,
    affects_verdict: bool,
}

#[derive(Debug, Clone, Default)]
struct LiveFlowState {
    connected: bool,
    active: bool,
    ended: bool,
    last_mbps: Option<f64>,
    error: String,
    retries: usize,
}

struct IperfProgressSnapshot<'a> {
    protocol: &'a str,
    tag: &'a str,
    active: usize,
    total: usize,
    connected: usize,
    ended: usize,
    nic_rx_mbps: Option<f64>,
    iperf_mbps: Option<f64>,
    errors: usize,
    monitor_error: String,
}

struct IperfRawArtifact<'a> {
    owner_id: &'a str,
    lidx: usize,
    stream_pos: usize,
    tag: &'a str,
    task: &'a IperfTask,
    client: &'a IperfClientOut,
    server_output: &'a str,
    events: &'a [IperfFlowEvent],
    error: &'a str,
}

impl Ctx {
    // ---------------- agent HTTP ----------------

    fn agent_post<TReq: Serialize, TOut: DeserializeOwned>(
        &self,
        path: &str,
        req: &TReq,
        timeout: Duration,
    ) -> Result<TOut, String> {
        let body = serde_json::to_string(req).map_err(|e| format!("序列化失败: {e}"))?;
        let (status, text) = http_client::post_json_auth_with_transport(
            self.transport.as_ref(),
            &self.agent_host,
            self.agent_port,
            path,
            &body,
            &self.cfg.agent_token,
            timeout,
        )
        .map_err(|e| format!("辅测机 {path} 调用失败: {e}"))?;
        if status == 401 {
            return Err(format!(
                "辅测机 {path} 拒绝访问(401)：agent 已启用令牌认证，请在本机 config.json 配置相同 agent_token"
            ));
        }
        if status != 200 {
            return Err(format!("辅测机 {path} 返回 HTTP {status}: {text}"));
        }
        let resp: Resp<TOut> =
            serde_json::from_str(&text).map_err(|e| format!("辅测机 {path} 响应解析失败: {e}"))?;
        if !resp.ok {
            return Err(resp
                .error
                .unwrap_or_else(|| format!("辅测机 {path} 返回未知错误")));
        }
        resp.data
            .ok_or_else(|| format!("辅测机 {path} 响应缺少 data"))
    }

    fn agent_post_reliable<TReq: Serialize, TOut: DeserializeOwned>(
        &self,
        path: &str,
        req: &TReq,
        timeout: Duration,
    ) -> Result<TOut, String> {
        self.agent_post_reliable_timed(path, req, timeout)
            .map(|(out, _)| out)
    }

    /// 与 `agent_post_reliable` 相同的重试语义，但额外返回成功那次调用
    /// 自身消耗的耗时（不含前几次失败的重试等待）。
    ///
    /// 时间轴对齐必须使用“成功调用自身的耗时”：若把三次可靠调用前的
    /// 起点到成功返回的总时长都算进去，首次连接超时、第二次成功时，
    /// 前一次失败的重试延时会被整体混入远端 job 起点估计，
    /// 真实流量窗口会整体偏移数秒。
    fn agent_post_reliable_timed<TReq: Serialize, TOut: DeserializeOwned>(
        &self,
        path: &str,
        req: &TReq,
        timeout: Duration,
    ) -> Result<(TOut, Duration), String> {
        let mut errors = Vec::new();
        for attempt in 1..=RELIABLE_HTTP_ATTEMPTS {
            let attempt_started = self.clock.now();
            match self.agent_post(path, req, timeout) {
                Ok(out) => {
                    return Ok((
                        out,
                        self.clock.now().saturating_duration_since(attempt_started),
                    ))
                }
                Err(e) => {
                    errors.push(format!("第{attempt}次: {e}"));
                    if attempt < RELIABLE_HTTP_ATTEMPTS {
                        self.clock.sleep(RELIABLE_HTTP_RETRY_DELAY);
                    }
                }
            }
        }
        Err(errors.join("；"))
    }

    // ---------------- 双端统一操作 ----------------

    fn ping_at(&self, side: Side, req: &PingReq) -> Result<PingOut, String> {
        match side {
            Side::Master => Ok(ping::run(req)),
            Side::Agent => {
                let mut out: PingOut =
                    self.agent_post("/ping", req, Duration::from_secs(req.count as u64 * 5 + 60))?;
                // 旧版 agent 可能仍把 ICMP Redirect/不可达计入 received。
                // 主控拿到完整 raw 后统一按当前规则重解析，既兼容旧协议字段，
                // 也保证本地与远端 Ping 使用同一套 Echo Reply 证据口径。
                if !out.raw.trim().is_empty() {
                    let parsed = ping::parse(&out.raw, req.count);
                    out.ok = parsed.ok;
                    out.sent = parsed.sent;
                    out.received = parsed.received;
                    out.lost = parsed.lost;
                    out.loss_pct = parsed.loss_pct;
                    out.rtt_min = parsed.rtt_min;
                    out.rtt_avg = parsed.rtt_avg;
                    out.rtt_max = parsed.rtt_max;
                }
                Ok(out)
            }
        }
    }

    fn cleanup_owner_resources(
        &self,
        owner_id: &str,
        remote_resources: bool,
    ) -> Result<(), String> {
        let mut errors = Vec::new();

        if remote_resources {
            match self.agent_post_reliable::<_, ResourceCleanupOut>(
                "/resources/cleanup",
                &ResourceCleanupReq {
                    owner_id: owner_id.to_string(),
                    wait_secs: RESOURCE_CLEANUP_WAIT_SECS,
                },
                Duration::from_secs(30),
            ) {
                Ok(out) => errors.extend(out.errors),
                Err(e) => errors.push(format!("辅测机 owner 清理未确认: {e}")),
            }
        }

        let local_servers = self.local_servers.stop_owner(owner_id, Duration::ZERO);
        errors.extend(local_servers.errors);
        for (id, result) in self.local_monitors.stop_owner(owner_id) {
            if let Err(e) = result {
                errors.push(format!("主控 monitor {id} 清理失败: {e}"));
            }
        }
        let cts_jobs = self
            .local_cts_jobs
            .stop_owner(owner_id, Duration::from_secs(RESOURCE_CLEANUP_WAIT_SECS));
        errors.extend(cts_jobs.errors);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("；"))
        }
    }

    fn server_start(&self, side: Side, req: &IperfServerStartReq) -> Result<String, String> {
        match side {
            Side::Master => {
                let bin = find_iperf3().ok_or("主控机未找到 iperf3，请把 iperf3 放到程序同目录")?;
                self.local_servers.start(&bin, req)
            }
            Side::Agent => match self.agent_post_reliable::<_, IperfServerStartOut>(
                "/iperf/server/start",
                req,
                Duration::from_secs(40),
            ) {
                Ok(out) => Ok(out.cmd),
                Err(start_error) => {
                    // start 的响应可能丢失，而进程其实已经启动。用同一个
                    // request_id 做补偿 stop；精确 stop 不会误杀后续新实例。
                    let cleanup = if req.request_id.is_empty() {
                        Ok(IperfServerStopOut::default())
                    } else {
                        self.server_stop_confirmed(side, req.port, &req.request_id, Duration::ZERO)
                    };
                    Err(match cleanup {
                        Ok(_) => {
                            format!("{start_error}（已补偿清理 request_id={}）", req.request_id)
                        }
                        Err(cleanup_error) => format!(
                            "{start_error}；补偿清理 request_id={} 也失败: {cleanup_error}",
                            req.request_id
                        ),
                    })
                }
            },
        }
    }

    fn server_stop_confirmed(
        &self,
        side: Side,
        port: u16,
        request_id: &str,
        wait: Duration,
    ) -> Result<IperfServerStopOut, String> {
        match side {
            Side::Master => self
                .local_servers
                .stop_checked(port, request_id, wait)
                .and_then(|out| {
                    if out.terminated {
                        Ok(out)
                    } else {
                        Err(format!("主控 server 端口 {port} 停止未确认"))
                    }
                }),
            Side::Agent => self
                .agent_post_reliable(
                    "/iperf/server/stop",
                    &IperfServerStopReq {
                        port,
                        wait_secs: wait.as_secs(),
                        request_id: request_id.to_string(),
                    },
                    Duration::from_secs(30),
                )
                .and_then(|out: IperfServerStopOut| {
                    if out.terminated {
                        Ok(out)
                    } else {
                        Err(format!("辅测机 server 端口 {port} 停止未确认"))
                    }
                }),
        }
    }

    fn client_stop_confirmed(&self, id: &str) -> Result<IperfClientStopOut, String> {
        self.agent_post_reliable(
            "/iperf/client/stop",
            &IperfClientStopReq {
                id: id.to_string(),
                wait_secs: RESOURCE_CLEANUP_WAIT_SECS,
            },
            Duration::from_secs(20),
        )
        .and_then(|out: IperfClientStopOut| {
            if out.terminated {
                Ok(out)
            } else {
                Err(format!("远端 client job {id} 停止未确认"))
            }
        })
    }

    fn client_run_tracked<F>(
        &self,
        side: Side,
        req: &IperfClientReq,
        owner_id: &str,
        request_id: &str,
        lease_secs: u64,
        mut on_event: F,
    ) -> IperfClientOut
    where
        F: FnMut(IperfFlowEvent),
    {
        match side {
            Side::Master => {
                let Some(bin) = find_iperf3() else {
                    return IperfClientOut {
                        ok: false,
                        timed_out: false,
                        process_started: Some(false),
                        cleanup_confirmed: Some(true),
                        cmd: String::new(),
                        output: "主控机未找到 iperf3，请把 iperf3 放到程序同目录".into(),
                        ..Default::default()
                    };
                };
                iperf::run_client_controlled(
                    &bin,
                    req,
                    Some(crate::cancel::cancel_flag()),
                    |line| {
                        if line.to_lowercase().contains("error") {
                            logln(&format!("      {line}"));
                        }
                    },
                    &mut on_event,
                )
            }
            Side::Agent => {
                let start_req = IperfClientStartReq {
                    request: req.clone(),
                    request_id: request_id.to_string(),
                    owner_id: owner_id.to_string(),
                    lease_secs,
                };
                let (started, start_attempt_elapsed): (IperfClientStartOut, Duration) = match self
                    .agent_post_reliable_timed(
                        "/iperf/client/start",
                        &start_req,
                        Duration::from_secs(20),
                    ) {
                    Ok((v, attempt_elapsed)) => (v, attempt_elapsed),
                    Err(e) => {
                        let cleanup = self.client_stop_confirmed(request_id);
                        let cleanup_confirmed = cleanup.is_ok();
                        return IperfClientOut {
                            cancelled: !cleanup_confirmed,
                            process_started: Some(false),
                            cleanup_confirmed: Some(cleanup_confirmed),
                            output: format!(
                                "(远端异步作业启动失败: {e}; 补偿清理: {})",
                                cleanup
                                    .map(|_| "已确认".to_string())
                                    .unwrap_or_else(|cleanup_error| cleanup_error)
                            ),
                            ..Default::default()
                        };
                    }
                };
                if !request_id.is_empty() && started.id != request_id {
                    let actual_cleanup = self.client_stop_confirmed(&started.id);
                    let expected_cleanup = self.client_stop_confirmed(request_id);
                    let cleanup_confirmed = actual_cleanup.is_ok() && expected_cleanup.is_ok();
                    return IperfClientOut {
                        cancelled: true,
                        process_started: Some(false),
                        cleanup_confirmed: Some(cleanup_confirmed),
                        output: format!(
                            "远端 client 返回了非预期 job id：期望 {request_id}，实际 {}；实际 ID 清理={}；期望 ID 清理={}",
                            started.id,
                            actual_cleanup
                                .map(|_| "已确认".to_string())
                                .unwrap_or_else(|error| error),
                            expected_cleanup
                                .map(|_| "已确认".to_string())
                                .unwrap_or_else(|error| error)
                        ),
                        ..Default::default()
                    };
                }
                // 只统计成功那次 start 调用自身的耗时：重试失败 + 等待
                // 属于远端 job 开始之前的编排开销，不能混入 job 零点估计。
                let response_elapsed_ms = start_attempt_elapsed.as_millis() as u64;
                let remote_origin_ms =
                    remote_job_origin_ms(response_elapsed_ms, started.elapsed_ms);
                let max_remote_secs = req.duration.saturating_add(180);
                let Some(deadline) =
                    std::time::Instant::now().checked_add(Duration::from_secs(max_remote_secs))
                else {
                    let cleanup = self.client_stop_confirmed(&started.id);
                    let cleanup_confirmed = cleanup.is_ok();
                    return IperfClientOut {
                        cancelled: !cleanup_confirmed,
                        cleanup_confirmed: Some(cleanup_confirmed),
                        output: format!(
                            "远端 client duration={} 秒过大，无法建立等待截止时间；停止确认: {}",
                            req.duration,
                            cleanup.map(|_| "成功".to_string()).unwrap_or_else(|e| e)
                        ),
                        ..Default::default()
                    };
                };
                let mut cursor = 0usize;
                loop {
                    if crate::cancel::is_cancelled() {
                        // 用户第一次 Ctrl+C：立即回收远端异步作业并返回，
                        // 主循环随后生成部分报告，不必等整段 duration 跑完。
                        let cleanup = self.client_stop_confirmed(&started.id);
                        let cleanup_confirmed = cleanup.is_ok();
                        let mut result = cleanup
                            .as_ref()
                            .ok()
                            .and_then(|output| output.result.clone())
                            .unwrap_or_default();
                        result.ok = false;
                        result.cancelled = !cleanup_confirmed;
                        result.cleanup_confirmed =
                            Some(cleanup_confirmed && result.cleanup_confirmed == Some(true));
                        if !result.output.is_empty() && !result.output.ends_with('\n') {
                            result.output.push('\n');
                        }
                        result.output.push_str(&format!(
                            "(用户中断，远端异步作业 {} 已停止确认: {})",
                            started.id,
                            cleanup.map(|_| "成功".to_string()).unwrap_or_else(|e| e)
                        ));
                        return result;
                    }
                    if std::time::Instant::now() >= deadline {
                        let cleanup = self.client_stop_confirmed(&started.id);
                        let cleanup_confirmed = cleanup.is_ok();
                        let mut result = cleanup
                            .as_ref()
                            .ok()
                            .and_then(|output| output.result.clone())
                            .unwrap_or_default();
                        let detail = format!(
                            "(远端异步作业 {} 超过 {} 秒仍未结束；停止确认: {})",
                            started.id,
                            max_remote_secs,
                            cleanup
                                .as_ref()
                                .map(|_| "成功".to_string())
                                .unwrap_or_else(|error| error.clone())
                        );
                        result.ok = false;
                        result.timed_out = true;
                        result.cancelled = !cleanup_confirmed;
                        result.cleanup_confirmed =
                            Some(cleanup_confirmed && result.cleanup_confirmed == Some(true));
                        if !result.output.is_empty() && !result.output.ends_with('\n') {
                            result.output.push('\n');
                        }
                        result.output.push_str(&detail);
                        return result;
                    }
                    let status: IperfClientStatusOut = match self.agent_post_reliable(
                        "/iperf/client/status",
                        &IperfClientStatusReq {
                            id: started.id.clone(),
                            cursor,
                        },
                        Duration::from_secs(20),
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            let cleanup = self.client_stop_confirmed(&started.id);
                            let cleanup_confirmed = cleanup.is_ok();
                            let mut result = cleanup
                                .as_ref()
                                .ok()
                                .and_then(|output| output.result.clone())
                                .unwrap_or_default();
                            result.ok = false;
                            result.cancelled = !cleanup_confirmed;
                            result.cleanup_confirmed =
                                Some(cleanup_confirmed && result.cleanup_confirmed == Some(true));
                            result.output = format!(
                                "(远端异步作业查询失败: {e}; 停止确认: {})",
                                cleanup
                                    .as_ref()
                                    .map(|_| "成功".to_string())
                                    .unwrap_or_else(|cleanup_error| cleanup_error.clone())
                            );
                            return result;
                        }
                    };
                    cursor = status.next_cursor;
                    for mut event in status.events {
                        event.elapsed_ms = event.elapsed_ms.saturating_add(remote_origin_ms);
                        if event.kind == IperfEventKind::Error {
                            logln(&format!("      [远端 {}] {}", started.id, event.line));
                        }
                        on_event(event);
                    }
                    if status.done {
                        let result_missing = status.result.is_none();
                        let stop = self.client_stop_confirmed(&started.id);
                        let mut result = status
                            .result
                            .or_else(|| stop.as_ref().ok().and_then(|output| output.result.clone()))
                            .unwrap_or_default();
                        if result_missing {
                            result.ok = false;
                            result.output =
                                format!("(远端异步作业 {} 已结束但缺少结果)", started.id);
                        }
                        if let Err(e) = stop {
                            result.ok = false;
                            result.cancelled = true;
                            result.cleanup_confirmed = Some(false);
                            if !result.output.ends_with('\n') && !result.output.is_empty() {
                                result.output.push('\n');
                            }
                            result
                                .output
                                .push_str(&format!("远端 client 结束后清理未确认: {e}"));
                        } else {
                            result.cleanup_confirmed = Some(result.cleanup_confirmed == Some(true));
                        }
                        return result;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }

    fn cts_job_start(
        &self,
        side: Side,
        start: CtsTrafficStartReq,
    ) -> Result<CtsTrafficStartOut, String> {
        self.cts_job_start_timed(side, start).map(|(out, _)| out)
    }

    /// 与 `cts_job_start` 相同的语义，额外返回成功那次 start 调用自身耗时
    /// （不含重试等待），用于把远端 job 零点对齐到真实启动时刻。
    fn cts_job_start_timed(
        &self,
        side: Side,
        start: CtsTrafficStartReq,
    ) -> Result<(CtsTrafficStartOut, Duration), String> {
        match side {
            Side::Master => {
                let bin = find_ctstraffic().ok_or_else(|| {
                    if cfg!(windows) {
                        "主控机未找到 ctsTraffic.exe，请放到程序同目录或 PATH".to_string()
                    } else {
                        "ctsTraffic 仅支持 Windows 10+，当前主控平台不支持".to_string()
                    }
                })?;
                let id = ctstraffic::start_managed_job(&self.local_cts_jobs, bin, start)?;
                let elapsed_ms = self.local_cts_jobs.elapsed_ms(&id).unwrap_or(0);
                Ok((CtsTrafficStartOut { id, elapsed_ms }, Duration::ZERO))
            }
            Side::Agent => {
                self.agent_post_reliable_timed("/ctstraffic/start", &start, Duration::from_secs(20))
            }
        }
    }

    fn cts_job_status(
        &self,
        side: Side,
        id: &str,
        cursor: usize,
    ) -> Result<CtsTrafficStatusOut, String> {
        match side {
            Side::Master => self.local_cts_jobs.status(id, cursor),
            Side::Agent => self.agent_post_reliable(
                "/ctstraffic/status",
                &CtsTrafficStatusReq {
                    id: id.to_string(),
                    cursor,
                },
                Duration::from_secs(10),
            ),
        }
    }

    fn cts_job_stop_confirmed(&self, side: Side, id: &str) -> Result<CtsTrafficStopOut, String> {
        let out = match side {
            Side::Master => self
                .local_cts_jobs
                .stop_checked(id, Duration::from_secs(RESOURCE_CLEANUP_WAIT_SECS)),
            Side::Agent => self.agent_post_reliable(
                "/ctstraffic/stop",
                &CtsTrafficStopReq {
                    id: id.to_string(),
                    wait_secs: RESOURCE_CLEANUP_WAIT_SECS,
                },
                Duration::from_secs(20),
            ),
        }?;
        if out.terminated {
            Ok(out)
        } else {
            Err(format!("ctsTraffic 作业 {id} 停止未确认"))
        }
    }

    fn cts_client_run_tracked<F>(
        &self,
        side: Side,
        start: CtsTrafficStartReq,
        mut on_event: F,
    ) -> CtsClientRun
    where
        F: FnMut(IperfFlowEvent),
    {
        let expected_id = start.request_id.clone();
        let duration = start.request.duration_secs;
        let (started, start_attempt_elapsed) = match self.cts_job_start_timed(side, start) {
            Ok((value, attempt_elapsed)) => (value, attempt_elapsed),
            Err(error) => {
                let cleanup = if expected_id.is_empty() {
                    Ok(CtsTrafficStopOut::default())
                } else {
                    self.cts_job_stop_confirmed(side, &expected_id)
                };
                let cleanup_confirmed = cleanup.is_ok();
                let detail = format!(
                    "ctsTraffic client 启动失败: {error}；补偿清理: {}",
                    cleanup
                        .map(|_| "已确认".to_string())
                        .unwrap_or_else(|cleanup_error| cleanup_error)
                );
                return CtsClientRun {
                    client: IperfClientOut {
                        cancelled: !cleanup_confirmed,
                        output: detail.clone(),
                        ..Default::default()
                    },
                    started: false,
                    cleanup_confirmed,
                    setup_error: Some(("CTSTRAFFIC_CLIENT_START_FAILED".into(), detail)),
                };
            }
        };
        if !expected_id.is_empty() && started.id != expected_id {
            let actual_cleanup = self.cts_job_stop_confirmed(side, &started.id);
            let expected_cleanup = self.cts_job_stop_confirmed(side, &expected_id);
            let cleanup_confirmed = actual_cleanup.is_ok() && expected_cleanup.is_ok();
            let detail = format!(
                "ctsTraffic 返回非预期 job id：期望 {expected_id}，实际 {}；实际 ID 清理={}；期望 ID 清理={}",
                started.id,
                actual_cleanup
                    .map(|_| "已确认".to_string())
                    .unwrap_or_else(|error| error),
                expected_cleanup
                    .map(|_| "已确认".to_string())
                    .unwrap_or_else(|error| error)
            );
            return CtsClientRun {
                client: IperfClientOut {
                    cancelled: true,
                    output: detail.clone(),
                    ..Default::default()
                },
                started: false,
                cleanup_confirmed,
                setup_error: Some(("CTSTRAFFIC_CLIENT_JOB_ID_MISMATCH".into(), detail)),
            };
        }
        // 只统计成功那次 start 调用自身的耗时，避免重试等待混入 CTS job 零点。
        let response_elapsed_ms = start_attempt_elapsed.as_millis() as u64;
        let origin_ms = remote_job_origin_ms(response_elapsed_ms, started.elapsed_ms);
        let max_wait = duration.saturating_add(60);
        let Some(deadline) = Instant::now().checked_add(Duration::from_secs(max_wait)) else {
            let cleanup = self.cts_job_stop_confirmed(side, &started.id);
            let cleanup_confirmed = cleanup.is_ok();
            let detail = format!(
                "ctsTraffic duration 过大，无法建立等待截止时间；停止确认: {}",
                cleanup
                    .map(|_| "成功".to_string())
                    .unwrap_or_else(|error| error)
            );
            return CtsClientRun {
                client: IperfClientOut {
                    cancelled: !cleanup_confirmed,
                    output: detail.clone(),
                    ..Default::default()
                },
                started: true,
                cleanup_confirmed,
                setup_error: Some(("CTSTRAFFIC_CLIENT_WAIT_INVALID".into(), detail)),
            };
        };
        let mut cursor = 0usize;
        loop {
            if crate::cancel::is_cancelled() {
                // 用户第一次 Ctrl+C：立即回收 CTS 异步作业并返回，
                // 主循环随后生成部分报告，不必等整段 duration 跑完。
                let cleanup = self.cts_job_stop_confirmed(side, &started.id);
                let mut client = cleanup
                    .as_ref()
                    .ok()
                    .and_then(|output| output.result.clone())
                    .unwrap_or_default();
                let process_cleanup_confirmed = client.cleanup_confirmed == Some(true);
                let cleanup_confirmed = cleanup.is_ok() && process_cleanup_confirmed;
                client.ok = false;
                client.cancelled = !cleanup_confirmed;
                if !client.output.is_empty() && !client.output.ends_with('\n') {
                    client.output.push('\n');
                }
                client.output.push_str(&format!(
                    "(用户中断，ctsTraffic 作业 {} 已停止确认: {})",
                    started.id,
                    cleanup.map(|_| "成功".to_string()).unwrap_or_else(|e| e)
                ));
                let cancel_detail = client.output.clone();
                return CtsClientRun {
                    client,
                    started: true,
                    cleanup_confirmed,
                    setup_error: Some(("CTSTRAFFIC_CLIENT_USER_CANCELLED".into(), cancel_detail)),
                };
            }
            if Instant::now() >= deadline {
                let cleanup = self.cts_job_stop_confirmed(side, &started.id);
                let mut client = cleanup
                    .as_ref()
                    .ok()
                    .and_then(|output| output.result.clone())
                    .unwrap_or_default();
                let process_started_confirmed = client.process_started == Some(true);
                let process_cleanup_confirmed = client.cleanup_confirmed == Some(true);
                let cleanup_confirmed = cleanup.is_ok() && process_cleanup_confirmed;
                let detail = format!(
                    "ctsTraffic client 超过 {} 秒仍未结束；停止确认: {}",
                    max_wait,
                    cleanup
                        .as_ref()
                        .map(|_| "成功".to_string())
                        .unwrap_or_else(|error| error.clone())
                );
                client.ok = false;
                client.timed_out = true;
                // 这里的 cancel 是 controller 为回收超时进程主动发出的。只要
                // 底层进程 wait/reap 与 job stop 都已确认，就保留 timed_out
                // 而不标成“显式取消”，从而允许单流安全进入下一轮。
                client.cancelled = !cleanup_confirmed;
                if !client.output.is_empty() && !client.output.ends_with('\n') {
                    client.output.push('\n');
                }
                client.output.push_str(&detail);
                let setup_error = if cleanup.is_err() {
                    Some(("CTSTRAFFIC_CLIENT_STOP_FAILED".into(), detail))
                } else if !process_started_confirmed {
                    Some((
                        "CTSTRAFFIC_CLIENT_PROCESS_NOT_STARTED".into(),
                        "ctsTraffic client 超时回收时未确认底层进程曾成功启动".into(),
                    ))
                } else if !process_cleanup_confirmed {
                    Some((
                        "CTSTRAFFIC_CLIENT_PROCESS_CLEANUP_UNCONFIRMED".into(),
                        "ctsTraffic client 超时后未确认底层进程已 wait/reap".into(),
                    ))
                } else {
                    None
                };
                return CtsClientRun {
                    client,
                    started: true,
                    cleanup_confirmed,
                    setup_error,
                };
            }
            let status = match self.cts_job_status(side, &started.id, cursor) {
                Ok(value) => value,
                Err(error) => {
                    let cleanup = self.cts_job_stop_confirmed(side, &started.id);
                    let cleanup_confirmed = cleanup.is_ok();
                    let detail = format!(
                        "ctsTraffic client 状态查询失败: {error}；停止确认: {}",
                        cleanup
                            .map(|_| "成功".to_string())
                            .unwrap_or_else(|cleanup_error| cleanup_error)
                    );
                    return CtsClientRun {
                        client: IperfClientOut {
                            cancelled: !cleanup_confirmed,
                            output: detail.clone(),
                            ..Default::default()
                        },
                        started: true,
                        cleanup_confirmed,
                        setup_error: Some(("CTSTRAFFIC_CLIENT_STATUS_FAILED".into(), detail)),
                    };
                }
            };
            cursor = status.next_cursor;
            for mut event in status.events {
                event.elapsed_ms = event.elapsed_ms.saturating_add(origin_ms);
                on_event(event);
            }
            if status.done {
                let result_missing = status.result.is_none();
                let mut result = status.result.unwrap_or_else(|| IperfClientOut {
                    output: "ctsTraffic client 已结束但缺少结果".into(),
                    ..Default::default()
                });
                let cleanup = self.cts_job_stop_confirmed(side, &started.id);
                let cleanup_confirmed = cleanup.is_ok();
                if let Err(error) = cleanup {
                    result.ok = false;
                    result.cancelled = true;
                    if !result.output.is_empty() && !result.output.ends_with('\n') {
                        result.output.push('\n');
                    }
                    result
                        .output
                        .push_str(&format!("ctsTraffic client 清理未确认: {error}"));
                }
                let setup_error = if !cleanup_confirmed {
                    Some((
                        "CTSTRAFFIC_CLIENT_STOP_FAILED".into(),
                        result.output.clone(),
                    ))
                } else if result_missing {
                    Some((
                        "CTSTRAFFIC_CLIENT_RESULT_MISSING".into(),
                        result.output.clone(),
                    ))
                } else {
                    cts_process_setup_error(&result)
                };
                return CtsClientRun {
                    client: result,
                    started: true,
                    cleanup_confirmed,
                    setup_error,
                };
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// 启动接收端网卡监控，返回 (id, 相对本次调用起点的零点偏移毫秒)。
    ///
    /// 远端 monitor 的零点用响应中的 `elapsed_ms` 和成功那次调用自身的
    /// 耗时做有界估计（`remote_job_origin_ms`），不再用 RPC 往返中点猜测：
    /// 非对称网络延迟会把空闲时间混入正式窗口。
    fn mon_start(
        &self,
        side: Side,
        iface: &str,
        owner_id: &str,
        lease_secs: u64,
    ) -> Result<(String, u64), String> {
        let interval_ms = self
            .cfg
            .iperf
            .rate_check
            .sample_interval_ms
            .clamp(200, 5_000);
        match side {
            Side::Master => {
                let before = Instant::now();
                let id =
                    self.local_monitors
                        .start_owned(iface, interval_ms, owner_id, lease_secs)?;
                let call_elapsed_ms = before.elapsed().as_millis() as u64;
                // 本地启动无网络往返，零点就是调用起点（本地开销可忽略）。
                Ok((id, midpoint_ms(0, call_elapsed_ms)))
            }
            Side::Agent => {
                let (out, attempt_elapsed) = self.agent_post_reliable_timed::<_, MonitorStartOut>(
                    "/monitor/start",
                    &MonitorStartReq {
                        iface: iface.to_string(),
                        interval_ms,
                        owner_id: owner_id.to_string(),
                        lease_secs,
                    },
                    Duration::from_secs(20),
                )?;
                let origin =
                    remote_job_origin_ms(attempt_elapsed.as_millis() as u64, out.elapsed_ms);
                Ok((out.id, origin))
            }
        }
    }

    fn mon_stop(&self, side: Side, id: &str) -> Result<MonitorStopOut, String> {
        match side {
            Side::Master => self.local_monitors.stop(id),
            Side::Agent => self.agent_post(
                "/monitor/stop",
                &MonitorStopReq { id: id.to_string() },
                Duration::from_secs(20),
            ),
        }
    }

    fn mon_status(&self, side: Side, id: &str) -> Result<MonitorStatusOut, String> {
        match side {
            Side::Master => self.local_monitors.status(id),
            Side::Agent => self.agent_post(
                "/monitor/status",
                &MonitorStatusReq { id: id.to_string() },
                Duration::from_secs(3),
            ),
        }
    }

    fn write_output_artifact(&self, filename: &str, contents: &str, label: &str) -> String {
        if let Err(error) = std::fs::create_dir_all(&self.outdir) {
            logln(&format!(
                "    [{label}] 无法创建输出目录 {}: {error}",
                self.outdir.display()
            ));
            return String::new();
        }
        let full = self.outdir.join(filename);
        let tmp = self.outdir.join(format!(".{filename}.tmp"));
        if let Err(error) =
            std::fs::write(&tmp, contents).and_then(|_| std::fs::rename(&tmp, &full))
        {
            let _ = std::fs::remove_file(&tmp);
            logln(&format!(
                "    [{label}] 写入失败 {}: {error}",
                full.display()
            ));
            return String::new();
        }
        logln(&format!("    [{label}] 已保存: {}", full.display()));
        self.outdir
            .file_name()
            .map(|dir| format!("./{}/{}", dir.to_string_lossy(), filename))
            .unwrap_or_else(|| full.to_string_lossy().into_owned())
    }

    fn save_iperf_raw_record(&self, artifact: IperfRawArtifact<'_>) -> String {
        let filename = raw_iperf_filename(
            artifact.owner_id,
            artifact.lidx,
            artifact.stream_pos,
            artifact.tag,
            artifact.task,
        );
        let contents = build_iperf_raw_record(
            artifact.task,
            artifact.client,
            artifact.server_output,
            artifact.events,
            artifact.error,
        );
        self.write_output_artifact(&filename, &contents, "原始记录")
    }

    fn save_monitor_samples(
        &self,
        owner_id: &str,
        side: Side,
        iface: &str,
        endpoint_identity: &str,
        origin_offset_ms: u64,
        out: &MonitorStopOut,
    ) -> String {
        let side_slug = match side {
            Side::Master => "master",
            Side::Agent => "agent",
        };
        let filename = format!(
            "nic_samples_{}_{}_{}_{}.csv",
            sanitize(owner_id),
            side_slug,
            sanitize(iface),
            &md5_hex(endpoint_identity)[..8]
        );
        let contents = build_monitor_samples_csv(side.cn(), iface, origin_offset_ms, out);
        self.write_output_artifact(&filename, &contents, "网卡原始样本")
    }

    /// 两端都尝试截图，任一成功就保存。返回报告用相对路径（多个用分号隔开）
    fn take_screenshots(&self, sides: &[Side], label: &str) -> (String, String) {
        let mut master = String::new();
        let mut agent = String::new();
        for side in sides.iter() {
            let png: Vec<u8> = match side {
                Side::Master => match crate::screenshot::capture_png() {
                    Ok(p) => p,
                    Err(e) => {
                        logln(&format!("    [截图] 主控端截图失败，任务 [{}]: {e}", label));
                        continue;
                    }
                },
                Side::Agent => {
                    let body = match serde_json::to_string(&ScreenshotReq {
                        label: label.to_string(),
                    }) {
                        Ok(body) => body,
                        Err(e) => {
                            logln(&format!("    [截图] 辅测请求序列化失败: {e}"));
                            continue;
                        }
                    };
                    let timeout = Duration::from_secs(180);
                    let (status, text) = match crate::http_client::post_json_auth(
                        &self.agent_host,
                        self.agent_port,
                        "/screenshot",
                        &body,
                        &self.cfg.agent_token,
                        timeout,
                    ) {
                        Ok((s, t)) => {
                            logln(&format!("    [截图] 辅测响应: status={s}, len={}", t.len()));
                            (s, t)
                        }
                        Err(e) => {
                            logln(&format!("    [截图] 辅测请求失败: {e}"));
                            continue;
                        }
                    };
                    if status != 200 {
                        logln(&format!(
                            "    [截图] 辅测 HTTP {status}: {}",
                            text_preview(&text, 200)
                        ));
                        continue;
                    }
                    let resp: Resp<ScreenshotOut> = match serde_json::from_str(&text) {
                        Ok(r) => r,
                        Err(e) => {
                            logln(&format!(
                                "    [截图] JSON解析失败: {e}, raw前100字符: {}",
                                text_preview(&text, 100)
                            ));
                            continue;
                        }
                    };
                    if !resp.ok {
                        logln(&format!(
                            "    [截图] 辅测截图错误: {}",
                            resp.error.unwrap_or_default()
                        ));
                        continue;
                    }
                    let Some(data) = resp.data else {
                        logln("    [截图] 辅测响应缺data");
                        continue;
                    };
                    let b64_len = data.image_b64.len();
                    match base64::engine::general_purpose::STANDARD.decode(data.image_b64) {
                        Ok(p) => p,
                        Err(e) => {
                            logln(&format!(
                                "    [截图] 辅测 base64 解码失败: {e}, len={b64_len}"
                            ));
                            continue;
                        }
                    }
                }
            };
            let (tag, ref mut out_path) = match side {
                Side::Master => ("_master", &mut master),
                Side::Agent => ("_agent", &mut agent),
            };
            let fname = format!(
                "screenshot_{}{}_{}.png",
                sanitize(label),
                tag,
                now_compact()
            );
            let full = self.outdir.join(&fname);
            if let Err(e) = std::fs::write(&full, &png) {
                logln(&format!(
                    "    [截图] {}端截图写入失败 {}: {e}",
                    side.cn(),
                    full.display()
                ));
                continue;
            }
            if let Some(dir_name) = self.outdir.file_name() {
                out_path.clear();
                out_path.push_str(&format!("./{}/{}", dir_name.to_string_lossy(), fname));
                logln(&format!(
                    "    [截图] {}端截图已保存: {}",
                    side.cn(),
                    full.display()
                ));
            } else {
                logln(&format!(
                    "    [截图] {}端截图文件已写入，但输出目录缺少可用目录名: {}",
                    side.cn(),
                    full.display()
                ));
            }
        }
        (master, agent)
    }

    fn push_row(&self, row: Row) -> usize {
        let mut g = lock_recover(&self.rows);
        g.push(row);
        g.len() - 1
    }

    fn udp_leg_plans(&self, unit: &Unit) -> Option<Vec<UdpLegPlan>> {
        let mut plans = Vec::new();
        for (lidx, leg) in unit.legs.iter().enumerate() {
            let (name, streams) = match &leg.kind {
                LegKind::IperfSingle(t) if t.udp => (t.profile_name.clone(), vec![t.clone()]),
                LegKind::IperfGroup { name, streams }
                    if !streams.is_empty() && streams.iter().all(|t| t.udp) =>
                {
                    (name.clone(), streams.clone())
                }
                _ => return None,
            };
            plans.push(UdpLegPlan {
                lidx,
                tag: leg.tag.clone(),
                name,
                streams,
            });
        }
        if plans.is_empty() {
            None
        } else {
            Some(plans)
        }
    }

    // ---------------- 执行入口 ----------------

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

    /// 平台/能力/二进制预检会阻止实际启动流量进程，但 builder 已识别出的
    /// CTS 参数错误必须保留更精确的 `CTSTRAFFIC_ARGS_INVALID`。这里逐 leg
    /// 处理，避免将来一个双向单元中只有一条 leg 非法时误放行另一条 leg。
    fn preflight_block_outcomes_with_cts_args(
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
                                reason_code: "NIC_DISAPPEARED".into(),
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
                        reason_code: "RESUME_FRESH_PASS".into(),
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
                                        verdict: Verdict::SetupError,
                                        reason_code: "LEG_THREAD_PANIC".into(),
                                        reason_detail: format!(
                                            "{} 方向执行线程 panic: {}",
                                            if leg.tag.is_empty() {
                                                "单向"
                                            } else {
                                                leg.tag.as_str()
                                            },
                                            panic_text(payload.as_ref())
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
                    outcome.verdict != Verdict::Pass
                        || !outcome.reason_code.is_empty()
                        || !outcome.reason_detail.is_empty()
                })
                .map(|outcome| {
                    format!(
                        "{}:{} {}",
                        if outcome.tag.is_empty() {
                            "单向"
                        } else {
                            &outcome.tag
                        },
                        outcome.reason_code,
                        outcome.reason_detail
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
                    .map(|outcome| outcome.reason_code.clone())
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
                    verdict: outcome.verdict,
                    reason_code: outcome.reason_code.clone(),
                    reason_detail: outcome.reason_detail.clone(),
                    reason: report_reason(&outcome.reason_code, &outcome.reason_detail),
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
                        outcome.verdict,
                        outcome.reason_code.clone(),
                        outcome.reason_detail.clone(),
                    )
                })
                .unwrap_or_else(|| {
                    (
                        Verdict::SetupError,
                        "UNIT_DIRECTION_RESULT_MISSING".into(),
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
                        verdict,
                        reason_code,
                        reason_detail,
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
                &reason_code,
                &reason_detail,
            );
            let Some(row) = row else {
                continue;
            };
            if let Some(index) = matched {
                outcomes[index].main_rows.push(row);
            } else {
                outcomes.push(LegOutcome {
                    verdict,
                    reason_code,
                    reason_detail,
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
        reason_code: &str,
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
            reason_code: reason_code.into(),
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
                    verdict: Verdict::SetupError,
                    reason_code: "UDP_GROUP_DISPATCH_ERROR".into(),
                    reason_detail: detail.into(),
                    rx_avg: None,
                    main_rows: vec![],
                    tag: leg.tag.clone(),
                }
            }
        }
    }

    // ---------------- ping ----------------

    fn run_ping_leg(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        tag: &str,
        t: &PingTask,
    ) -> LegOutcome {
        let time = now_full();
        let (src_addr, dst_addr) = if t.v6 {
            match v6_addrs(&t.src.nic, &t.dst.nic) {
                Some(v) => {
                    let bind = add_zone(&v.client_bind, &t.src.nic.zone, t.src.side);
                    let target = add_zone(&v.client_target, &t.src.nic.zone, t.src.side);
                    (bind, target)
                }
                None => (String::new(), String::new()),
            }
        } else {
            (t.src.nic.ipv4.clone(), t.dst.nic.ipv4.clone())
        };
        let req = PingReq {
            dst: dst_addr.clone(),
            src: src_addr.clone(),
            count: t.count,
            payload: t.payload,
            v6: t.v6,
        };
        let gateway_missing =
            t.purpose == PingPurpose::GatewayDiagnostic && dst_addr.trim().is_empty();
        if gateway_missing {
            logln(&format!(
                "  [ping{}] {} 未发现 IPv4 网关，无法执行绑定源地址的网关诊断。",
                fmt_tag(tag),
                src_addr
            ));
        } else {
            logln(&format!(
                "  [ping{}] {} -> {} (n={}, -l {}) 执行中...",
                fmt_tag(tag),
                src_addr,
                dst_addr,
                t.count,
                t.payload
            ));
        }
        let (out, transport_error) = if gateway_missing {
            (
                PingOut {
                    ok: false,
                    sent: 0,
                    received: 0,
                    lost: 0,
                    loss_pct: 0.0,
                    raw: "未发现该网卡的 IPv4 默认网关，未发送 Ping。".into(),
                    ..Default::default()
                },
                None,
            )
        } else {
            match self.ping_at(t.src.side, &req) {
                Ok(out) => (out, None),
                Err(error) => (
                    PingOut {
                        ok: false,
                        raw: format!("辅测机 Ping 请求执行失败: {error}"),
                        ..Default::default()
                    },
                    Some(error),
                ),
            }
        };
        let exec_kind = if transport_error.is_some() {
            Some(ping::PingExecErrorKind::Execution)
        } else if gateway_missing {
            None
        } else {
            ping::execution_error_kind(&out)
        };
        let exec_detail = transport_error.or_else(|| ping::execution_error(&out));
        let verdict = if gateway_missing {
            Verdict::NotEvaluated
        } else if exec_kind.is_some() {
            Verdict::SetupError
        } else if out.ok {
            Verdict::Pass
        } else {
            Verdict::RateFail
        };
        let execution_status = if gateway_missing {
            ExecutionStatus::Partial
        } else {
            match exec_kind {
                Some(ping::PingExecErrorKind::Timeout) => ExecutionStatus::TimedOut,
                Some(_) => ExecutionStatus::Error,
                None => ExecutionStatus::Completed,
            }
        };
        let reason_code = if gateway_missing {
            "GATEWAY_NOT_FOUND"
        } else if exec_kind == Some(ping::PingExecErrorKind::Timeout) {
            "PING_TIMEOUT"
        } else if exec_kind.is_some() {
            "PING_EXEC_ERROR"
        } else if out.ok {
            "PING_OK"
        } else {
            match t.purpose {
                PingPurpose::SubnetTest => "PING_UNREACHABLE",
                PingPurpose::SubnetDiagnostic => "PING_SUBNET_UNREACHABLE",
                PingPurpose::GatewayDiagnostic => "PING_GATEWAY_UNREACHABLE",
            }
        }
        .to_string();
        let reason_detail = if gateway_missing {
            format!(
                "网卡 {}({}) 没有发现 IPv4 默认网关；无法用网关 Ping 判断该网卡/载体状态",
                t.src.nic.name, t.src.nic.ipv4
            )
        } else if let Some(detail) = exec_detail {
            detail
        } else if out.ok {
            format!(
                "Ping 连通：发送/接收={}/{}，丢包率 {:.1}%，RTT 最小/平均/最大={}/{}/{} ms",
                out.sent,
                out.received,
                out.loss_pct,
                format_ping_rtt(out.rtt_min),
                format_ping_rtt(out.rtt_avg),
                format_ping_rtt(out.rtt_max)
            )
        } else {
            format!(
                "Ping 命令正常完成，但未收到目标 Echo Reply（收/发={}/{}，丢包率 {:.1}%）",
                out.received, out.sent, out.loss_pct
            )
        };
        logln(&format!(
            "    结果: {} 收/发={}/{} 丢包={} 平均={}ms{}",
            verdict.label(),
            out.received,
            out.sent,
            if gateway_missing || exec_kind.is_some() {
                "-".into()
            } else {
                format!("{:.1}%", out.loss_pct)
            },
            out.rtt_avg
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            if reason_detail.is_empty() {
                String::new()
            } else {
                format!(" ({reason_detail})")
            }
        ));
        let kind_label = match t.purpose {
            PingPurpose::SubnetTest if unit.bidir => format!("★双向子网PING-{tag}"),
            PingPurpose::SubnetTest => "子网PING（收到至少一个 Echo Reply 即连通）".into(),
            PingPurpose::SubnetDiagnostic => "故障诊断-子网PING".into(),
            PingPurpose::GatewayDiagnostic => "故障诊断-网卡到网关PING".into(),
        };
        let raw_text = if out.cmd.is_empty() {
            out.raw.clone()
        } else {
            format!("$ {}\n{}", out.cmd, out.raw)
        };
        let idx = self.push_row(Row {
            sort_key: (useq, lidx, 0, 0),
            time,
            task_id: md5_hex(&format!("{}|{}|ping", unit.id, tag)),
            parent_id: unit.id.clone(),
            task: unit.title.clone(),
            ip: if t.v6 { "V6".into() } else { "V4".into() },
            transport: String::new(),
            param: format!("-l {}", t.payload),
            src_pc: t.src.pc.clone(),
            src_iface: t.src.nic.name.clone(),
            src_ip: src_addr,
            dst_pc: t.dst.pc.clone(),
            dst_iface: t.dst.nic.name.clone(),
            dst_ip: dst_addr,
            verdict,
            execution_status,
            reason_code: reason_code.clone(),
            reason_detail: reason_detail.clone(),
            kind_label,
            ping_loss: (!gateway_missing && exec_kind.is_none()).then_some(out.loss_pct),
            ping_min: (!gateway_missing && exec_kind.is_none())
                .then_some(out.rtt_min)
                .flatten(),
            ping_avg: (!gateway_missing && exec_kind.is_none())
                .then_some(out.rtt_avg)
                .flatten(),
            ping_max: (!gateway_missing && exec_kind.is_none())
                .then_some(out.rtt_max)
                .flatten(),
            command: out.cmd.clone(),
            raws: vec![(format!("ping{} 输出", fmt_tag(tag)), raw_text)],
            ..Default::default()
        });
        LegOutcome {
            verdict,
            reason_code,
            reason_detail,
            rx_avg: None,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    // ---------------- ctsTraffic ----------------

    fn build_cts_requests(
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
    fn save_ctstraffic_raw_record(
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
    fn run_ctstraffic_attempt(
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
                             code: &str,
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
            setup_error: Some((code.to_string(), detail)),
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
                    "CTSTRAFFIC_SERVER_START_FAILED",
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
                "CTSTRAFFIC_SERVER_JOB_ID_MISMATCH",
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
                        "CTSTRAFFIC_SERVER_EXITED_EARLY"
                    } else {
                        "CTSTRAFFIC_SERVER_STOP_FAILED"
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
                        "CTSTRAFFIC_SERVER_STATUS_FAILED"
                    } else {
                        "CTSTRAFFIC_SERVER_STOP_FAILED"
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
                "CTSTRAFFIC_SERVER_STOP_FAILED".into(),
                format!("ctsTraffic server 停止未确认，禁止复用端口: {error}"),
            ))
        } else if client_run.setup_error.is_some() {
            client_run.setup_error
        } else if server_cancelled_before_stop {
            Some((
                "CTSTRAFFIC_SERVER_CANCELLED".into(),
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
                "CTSTRAFFIC_SERVER_PROCESS_NOT_STARTED".into(),
                "ctsTraffic server 未明确证明底层进程已成功启动（process_started != true）".into(),
            ))
        } else if !server_process_cleanup_confirmed {
            Some((
                "CTSTRAFFIC_SERVER_PROCESS_CLEANUP_UNCONFIRMED".into(),
                "ctsTraffic server 未明确证明底层进程已 wait/reap（cleanup_confirmed != true）"
                    .into(),
            ))
        } else if !process_started_confirmed {
            Some((
                "CTSTRAFFIC_CLIENT_PROCESS_NOT_STARTED".into(),
                "ctsTraffic client 未明确证明底层进程已成功启动（process_started != true）".into(),
            ))
        } else if !process_cleanup_confirmed {
            Some((
                "CTSTRAFFIC_CLIENT_PROCESS_CLEANUP_UNCONFIRMED".into(),
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

    fn run_ctstraffic_leg(
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
                "CTSTRAFFIC_ARGS_INVALID",
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
                    "CTSTRAFFIC_ARGS_INVALID",
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
                    "CTSTRAFFIC_ARGS_INVALID",
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
                    code: "CTSTRAFFIC_MONITOR_START_FAILED".into(),
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
                        code: "CTSTRAFFIC_MONITOR_STOP_FAILED".into(),
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
                "CTSTRAFFIC_INTERNAL_NO_ATTEMPT",
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
        let nic_samples = mon_out
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
                            "CTSTRAFFIC_CLEANUP_FAILED".to_string(),
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
                            "CTSTRAFFIC_CLIENT_CANCELLED".to_string(),
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
                "CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED".to_string(),
                format!(
                    "CTS 单流 UDP 在 {full_attempts} 次完整 server/client 尝试且每轮双端清理均确认后，仍无 ctsTraffic 自身 rate/bytes/successful frames 测量；该方向必须灌通"
                ),
            )
        } else if !measurement && (selected.client.timed_out || selected.client.cancelled) {
            (
                Verdict::SetupError,
                "CTSTRAFFIC_CLIENT_ABORTED".to_string(),
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
                "CTSTRAFFIC_NO_MEASUREMENT".to_string(),
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
                "CTSTRAFFIC_EFFECTIVE_WINDOW_SHORT".to_string(),
                format!(
                    "CTS 真实流量事件窗口仅 {:.3}s，短于要求的 {}s；未把启动、握手、轮询或清理时间计入有效窗口",
                    selected.traffic_window.available_secs, task.duration
                ),
            )
        } else if required_streams > requested_streams {
            (
                Verdict::NotEvaluated,
                "CONFIGURED_LOAD_TOO_LOW".to_string(),
                format!(
                    "目标与余量要求至少 {required_streams} 条流，但只配置了 {requested_streams} 条"
                ),
            )
        } else if active_streams < required_streams {
            (
                Verdict::NotEvaluated,
                "ACTIVE_STREAMS_LOW".to_string(),
                format!(
                    "ctsTraffic 最多观测到 {active_streams}/{requested_streams} 条活跃连接，正式判定至少需要 {required_streams} 条"
                ),
            )
        } else {
            // 丢帧判定必须排在网卡采样/目标可信度之后，与 iperf3 路径的判定链
            // 一致：采样不足或目标未知时先产出 NOT_EVALUATED / MEASURED，不能
            // 拿一个无法核对的窗口去判 RATE_FAIL。
            let nic = evaluate_nic_rx(task.rate_mode, task.rx_target_mbps, &rx_stats, &tx_stats);
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
            sort_key: (useq, lidx, 0, 0),
            time,
            task_id: md5_hex(&format!("{}|{}|ctstraffic", unit.id, tag)),
            parent_id: unit.id.clone(),
            task: unit.title.clone(),
            ip: if task.v6 { "V6".into() } else { "V4".into() },
            transport: if task.udp {
                "CTS/UDP".into()
            } else {
                "CTS/TCP".into()
            },
            param: task.profile_label.clone(),
            src_pc: task.src.pc.clone(),
            src_iface: task.src.nic.name.clone(),
            src_ip: task.src.nic.ipv4.clone(),
            dst_pc: task.dst.pc.clone(),
            dst_iface: task.dst.nic.name.clone(),
            dst_ip: task.dst.nic.ipv4.clone(),
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
            reason_code: reason_code.clone(),
            reason_detail: reason_detail.clone(),
            kind_label: if unit.bidir {
                format!("★★双向 CTS Traffic-{tag}")
            } else {
                "CTS Traffic 灌包".into()
            },
            rx_avg,
            tx_mbps: parsed.send_mbps,
            rx_mbps: parsed.recv_mbps,
            udp_loss: loss,
            command: selected.client.cmd.clone(),
            raw_log,
            nic_samples,
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
            ..Default::default()
        });
        LegOutcome {
            verdict,
            reason_code,
            reason_detail,
            rx_avg,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_cts_setup_error_row(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        tag: &str,
        task: &CtsTrafficTask,
        time: String,
        reason_code: &str,
        reason_detail: String,
    ) -> LegOutcome {
        let idx = self.push_row(Row {
            sort_key: (useq, lidx, 0, 0),
            time,
            task_id: md5_hex(&format!("{}|{}|ctstraffic", unit.id, tag)),
            parent_id: unit.id.clone(),
            task: unit.title.clone(),
            ip: if task.v6 { "V6".into() } else { "V4".into() },
            transport: if task.udp {
                "CTS/UDP".into()
            } else {
                "CTS/TCP".into()
            },
            param: task.profile_label.clone(),
            src_pc: task.src.pc.clone(),
            src_iface: task.src.nic.name.clone(),
            src_ip: task.src.nic.ipv4.clone(),
            dst_pc: task.dst.pc.clone(),
            dst_iface: task.dst.nic.name.clone(),
            dst_ip: task.dst.nic.ipv4.clone(),
            verdict: Verdict::SetupError,
            execution_status: ExecutionStatus::Error,
            reason_code: reason_code.into(),
            reason_detail: reason_detail.clone(),
            kind_label: if unit.bidir {
                format!("★★双向 CTS Traffic-{tag}")
            } else {
                "CTS Traffic 灌包".into()
            },
            requested_streams: task.streams as usize,
            raws: vec![("ctsTraffic 启动错误".into(), reason_detail.clone())],
            ..Default::default()
        });
        LegOutcome {
            verdict: Verdict::SetupError,
            reason_code: reason_code.into(),
            reason_detail,
            rx_avg: None,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    // ---------------- iperf 单条 ----------------

    fn build_iperf_requests(
        &self,
        t: &IperfTask,
        duration: u64,
        owner_id: &str,
        lease_secs: u64,
        attempt: usize,
    ) -> Result<(IperfServerStartReq, IperfClientReq), String> {
        let (client_bind, client_target, server_bind) = if t.v6 {
            let v = v6_addrs(&t.src.nic, &t.dst.nic)
                .ok_or_else(|| "两端缺少可用 IPv6 地址".to_string())?;
            (
                add_zone(&v.client_bind, &t.src.nic.zone, t.src.side),
                add_zone(&v.client_target, &t.src.nic.zone, t.src.side),
                add_zone(&v.server_bind, &t.dst.nic.zone, t.dst.side),
            )
        } else {
            (
                t.src.nic.ipv4.clone(),
                t.dst.nic.ipv4.clone(),
                t.dst.nic.ipv4.clone(),
            )
        };
        Ok((
            IperfServerStartReq {
                bind_ip: server_bind,
                port: t.port,
                v6: t.v6,
                request_id: lifecycle_request_id(owner_id, "server", t.port, attempt),
                owner_id: owner_id.to_string(),
                lease_secs,
            },
            IperfClientReq {
                dst: client_target,
                bind_ip: client_bind,
                port: t.port,
                duration,
                udp: t.udp,
                v6: t.v6,
                extra: t.extra.clone(),
            },
        ))
    }

    /// 核心执行：server(dst侧) -> client(src侧) -> 停 server。不含监控。
    fn exec_iperf_core<F>(
        &self,
        t: &IperfTask,
        owner_id: &str,
        lease_secs: u64,
        epoch: &Instant,
        mut on_event: F,
    ) -> (bool, iperf::IperfParsed, IperfClientOut, String)
    where
        F: FnMut(IperfFlowEvent),
    {
        let (sreq, creq) = match self.build_iperf_requests(t, t.duration, owner_id, lease_secs, 0) {
            Ok(v) => v,
            Err(e) => {
                let out = IperfClientOut {
                    output: e,
                    ..Default::default()
                };
                return (false, iperf::IperfParsed::default(), out, String::new());
            }
        };
        if let Err(e) = self.server_start(t.dst.side, &sreq) {
            // 同时构造 client 命令供查错
            let cli_args = crate::cmd::iperf::client_args(&creq);
            let cli_cmd = format!("iperf3 {}", cli_args.join(" "));
            let out = IperfClientOut {
                ok: false,
                cmd: cli_cmd,
                output: format!("(iperf3 server 启动失败: {e})"),
                ..Default::default()
            };
            return (false, iperf::IperfParsed::default(), out, String::new());
        }
        let client_call_offset_ms = epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mut local_event_origin_ms = None::<u64>;
        let client = self.client_run_tracked(
            t.src.side,
            &creq,
            owner_id,
            &lifecycle_request_id(owner_id, "client", t.port, 0),
            lease_secs,
            |mut event| {
                if t.src.side == Side::Master {
                    // 本机首轮可能在 Started 事件前先执行
                    // `iperf3 --help` 能力探测。以首个回调的当前时刻
                    // 反推 job 零点，不把这段一次性等待计入数据窗口。
                    iperf::align_event_to_epoch(
                        &mut event,
                        epoch.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        &mut local_event_origin_ms,
                    );
                } else {
                    // 远端事件已在 client_run_tracked 中按 start RPC
                    // 与 job elapsed 对齐到本次调用零点。
                    event.elapsed_ms = event.elapsed_ms.saturating_add(client_call_offset_ms);
                }
                on_event(event);
            },
        );
        let stop = self.server_stop_confirmed(t.dst.side, t.port, &sreq.request_id, Duration::ZERO);
        let (server_out, stop_ok) = match stop {
            Ok(out) => (out.output, true),
            Err(e) => (format!("(iperf3 server 停止未确认: {e})"), false),
        };
        let parsed = iperf::parse_output(&client.output);
        let raw_ok = client.ok && !client.timed_out && !client.cancelled && stop_ok;
        (raw_ok, parsed, client, server_out)
    }

    fn run_iperf_single(
        &self,
        useq: usize,
        unit: &Unit,
        lidx: usize,
        tag: &str,
        t: &IperfTask,
        lifecycle: LifecycleLease<'_>,
    ) -> LegOutcome {
        let time = now_full();
        logln(&format!(
            "  [iperf{}] {} {} -> {} 端口{} {}s...",
            fmt_tag(tag),
            t.profile_label,
            t.src.brief(),
            t.dst.brief(),
            t.port,
            t.duration
        ));
        // monitor 和 iperf client 事件必须对齐到同一个 leg epoch，
        // 否则 server 启动、RPC 延迟和停止清理都会混入 TCP 平均速率。
        // 远端 monitor 零点由响应 elapsed_ms 有界估计，不再用 RPC 中点猜测。
        let leg_epoch = Instant::now();
        let monitor_start_before_ms = leg_epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mon_id = match self.mon_start(
            t.dst.side,
            &t.dst.nic.name,
            lifecycle.owner_id,
            lifecycle.lease_secs,
        ) {
            Ok((id, call_origin_ms)) => Some((id, monitor_start_before_ms + call_origin_ms)),
            Err(e) => {
                logln(&format!("    (接收端网卡监控启动失败: {e})"));
                None
            }
        };
        // 发送端也要采样：有明确目标时 W08 要求 RX/TX 双侧滚动窗口都完整，
        // 发送端采样塌了同样说明这一轮时间轴不可信。同一块网卡就不重复起。
        let tx_mon_id = if t.src.key() == t.dst.key() {
            None
        } else {
            let before_ms = leg_epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
            match self.mon_start(
                t.src.side,
                &t.src.nic.name,
                lifecycle.owner_id,
                lifecycle.lease_secs,
            ) {
                Ok((id, call_origin_ms)) => Some((id, before_ms + call_origin_ms)),
                Err(e) => {
                    logln(&format!("    (发送端网卡监控启动失败: {e})"));
                    None
                }
            }
        };
        let live = Arc::new(Mutex::new(LiveFlowState::default()));
        let mut events = Vec::new();
        let parallel_streams = if t.udp {
            1
        } else {
            tcp_parallel_streams(&t.extra)
        };
        let mon_id_for_progress = mon_id.as_ref().map(|(id, _)| id.clone());
        let live_for_progress = Arc::clone(&live);
        let progress_tag = tag.to_string();
        let progress_protocol = if t.udp { "UDP" } else { "TCP" };
        let (raw_ok, parsed, client, server_out) = std::thread::scope(|scope| {
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            let progress = scope.spawn(move || {
                let mut monitor_enabled = mon_id_for_progress.is_some();
                loop {
                    match done_rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let state = live_for_progress
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    let mut monitor_error = String::new();
                    let nic_rx_mbps = if monitor_enabled {
                        match mon_id_for_progress.as_deref() {
                            Some(id) => match self.mon_status(t.dst.side, id) {
                                Ok(status) => match status.latest_sample {
                                    Some(sample) if sample.valid => Some(sample.rx_mbps),
                                    Some(sample) => {
                                        monitor_error = if sample.error.is_empty() {
                                            "网卡样本无效".into()
                                        } else {
                                            sample.error
                                        };
                                        None
                                    }
                                    None => {
                                        monitor_error = "等待首个网卡样本".into();
                                        None
                                    }
                                },
                                Err(error) => {
                                    monitor_enabled = false;
                                    monitor_error = error;
                                    None
                                }
                            },
                            None => None,
                        }
                    } else {
                        None
                    };
                    let active = usize::from(
                        (!state.ended && state.active)
                            || nic_rx_mbps.is_some_and(|rate| rate > MIN_VALID_RX_MBPS),
                    );
                    logln(&format_iperf_progress(&IperfProgressSnapshot {
                        protocol: progress_protocol,
                        tag: &progress_tag,
                        active,
                        total: 1,
                        connected: usize::from(state.connected),
                        ended: usize::from(state.ended),
                        nic_rx_mbps,
                        iperf_mbps: active_iperf_rate(&state),
                        errors: usize::from(!state.error.is_empty()),
                        monitor_error,
                    }));
                }
            });
            let result = self.exec_iperf_core(
                t,
                lifecycle.owner_id,
                lifecycle.lease_secs,
                &leg_epoch,
                |event| {
                    {
                        let mut state =
                            live.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        if event.kind != IperfEventKind::Traffic
                            || is_live_progress_rate_line(&event.line, parallel_streams)
                        {
                            apply_flow_event(&mut state, &event);
                        }
                    }
                    events.push(event);
                },
            );
            let _ = done_tx.send(());
            let _ = progress.join();
            result
        });
        let rx_origin_offset_ms = mon_id.as_ref().map(|(_, offset)| *offset).unwrap_or(0);
        let mon_out =
            mon_id.and_then(
                |(id, start_offset_ms)| match self.mon_stop(t.dst.side, &id) {
                    Ok(mut output) => {
                        align_monitor_samples(&mut output, start_offset_ms);
                        Some(output)
                    }
                    Err(error) => {
                        logln(&format!("    (接收端网卡监控停止失败: {error})"));
                        None
                    }
                },
            );
        let tx_mon_out =
            tx_mon_id.and_then(
                |(id, start_offset_ms)| match self.mon_stop(t.src.side, &id) {
                    Ok(mut output) => {
                        align_monitor_samples(&mut output, start_offset_ms);
                        Some(output)
                    }
                    Err(error) => {
                        logln(&format!("    (发送端网卡监控停止失败: {error})"));
                        None
                    }
                },
            );
        let effective_window =
            iperf_effective_window(&events, t.duration, parsed.has_measurement());
        let baseline_cutoff_ms = iperf_baseline_cutoff_ms(&events);
        let rx_stats = mon_out
            .as_ref()
            .map(|output| monitor_rate_stats(output, &effective_window, true, baseline_cutoff_ms))
            .unwrap_or_default();
        // 同一块网卡时 TX 与 RX 取自同一份样本，只是读另一个计数器方向。
        let tx_stats = tx_mon_out
            .as_ref()
            .or(if t.src.key() == t.dst.key() {
                mon_out.as_ref()
            } else {
                None
            })
            .map(|output| monitor_rate_stats(output, &effective_window, false, baseline_cutoff_ms))
            .unwrap_or_default();
        let rx_avg = rx_stats.avg_mbps;
        let nic_samples = mon_out
            .as_ref()
            .map(|out| {
                self.save_monitor_samples(
                    lifecycle.owner_id,
                    t.dst.side,
                    &t.dst.nic.name,
                    &t.dst.key(),
                    rx_origin_offset_ms,
                    out,
                )
            })
            .unwrap_or_default();

        let measurement = parsed.has_measurement();
        let (verdict, reason_code, reason_detail) = iperf_flow_verdict(IperfFlowVerdictIn {
            raw_ok,
            measurement,
            effective_window: &effective_window,
            required_secs: t.duration,
            rate_mode: t.rate_mode,
            rx_target_mbps: t.rx_target_mbps,
            rx_stats: &rx_stats,
            tx_stats: &tx_stats,
            client_tail: client.output.lines().last().unwrap_or_default(),
            rx_monitor: mon_out.as_ref(),
        });
        let raw_error = if raw_ok {
            String::new()
        } else {
            client.output.lines().last().unwrap_or_default().to_string()
        };
        let raw_log = self.save_iperf_raw_record(IperfRawArtifact {
            owner_id: lifecycle.owner_id,
            lidx,
            stream_pos: 0,
            tag,
            task: t,
            client: &client,
            server_output: &server_out,
            events: &events,
            error: &raw_error,
        });

        logln(&format!(
            "    结果: {} 发送={} 接收={} 网卡实测={}",
            verdict.label(),
            fmt_opt(parsed.best_sender()),
            fmt_opt(parsed.best_receiver()),
            fmt_opt(rx_avg)
        ));

        let (screenshot_master, screenshot_agent) = if self.cfg.screenshot {
            self.take_screenshots(
                &[t.dst.side, t.src.side],
                &format!("{}_{}", unit.title, tag),
            )
        } else {
            (String::new(), String::new())
        };

        let kind_label = if unit.bidir {
            format!("★★双向灌包-{tag}")
        } else {
            "灌包".into()
        };
        let idx = self.push_row(Row {
            sort_key: (useq, lidx, 0, 0),
            time,
            task_id: md5_hex(&format!("{}|{}|{}", unit.id, tag, t.stream_idx)),
            parent_id: unit.id.clone(),
            task: unit.title.clone(),
            ip: if t.v6 { "V6".into() } else { "V4".into() },
            transport: if t.udp { "UDP".into() } else { "TCP".into() },
            param: t.profile_label.clone(),
            src_pc: t.src.pc.clone(),
            src_iface: t.src.nic.name.clone(),
            src_ip: t.src.nic.ipv4.clone(),
            dst_pc: t.dst.pc.clone(),
            dst_iface: t.dst.nic.name.clone(),
            dst_ip: t.dst.nic.ipv4.clone(),
            verdict,
            execution_status: if client.timed_out {
                ExecutionStatus::TimedOut
            } else if client.cancelled {
                ExecutionStatus::Cancelled
            } else if !raw_ok {
                ExecutionStatus::Error
            } else if verdict == Verdict::NotEvaluated {
                ExecutionStatus::Partial
            } else {
                ExecutionStatus::Completed
            },
            reason_code: reason_code.clone(),
            reason_detail: reason_detail.clone(),
            kind_label,
            rx_avg,
            tx_mbps: parsed.best_sender(),
            rx_mbps: parsed.best_receiver(),
            udp_loss: if t.udp { parsed.udp_loss_pct } else { None },
            screenshot_master,
            screenshot_agent,
            command: client.cmd.clone(),
            raw_log,
            nic_samples,
            requested_streams: parallel_streams,
            active_streams: if parsed.has_measurement() {
                parallel_streams
            } else {
                0
            },
            required_streams: parallel_streams,
            target_mbps: t.rx_target_mbps,
            tx_avg: tx_stats.avg_mbps,
            tx_p10: tx_stats.p10_mbps,
            rx_p10: rx_stats.p10_mbps,
            rx_median: rx_stats.median_mbps,
            rx_p95: rx_stats.p95_mbps,
            rx_min: rx_stats.min_mbps,
            rx_max: rx_stats.max_mbps,
            effective_seconds: Some(effective_window.available_secs),
            required_seconds: Some(t.duration as f64),
            sample_coverage: Some(rx_stats.coverage),
            window_start_ms: Some(effective_window.start_ms),
            window_end_ms: Some(effective_window.end_ms),
            baseline_mbps: Some(rx_stats.baseline_mbps),
            rolling_coverage: Some(rx_stats.rolling_coverage),
            raws: vec![
                (
                    format!("iperf3 client{} 输出", fmt_tag(tag)),
                    format!("$ {}\n{}", client.cmd, client.output),
                ),
                (format!("iperf3 server{} 输出", fmt_tag(tag)), server_out),
                (
                    format!("流事件{}", fmt_tag(tag)),
                    format_flow_events(&events, &raw_error),
                ),
            ],
            ..Default::default()
        });
        LegOutcome {
            verdict,
            reason_code,
            reason_detail,
            rx_avg,
            main_rows: vec![idx],
            tag: tag.to_string(),
        }
    }

    // ---------------- UDP 单元统一调度 ----------------

    fn start_udp_server_with_retry(
        &self,
        task: &IperfTask,
        base_req: &IperfServerStartReq,
        max_retries: usize,
    ) -> Result<IperfServerStartReq, String> {
        let mut errors = Vec::new();
        for attempt in 0..=max_retries {
            let mut req = base_req.clone();
            if attempt > 0 {
                req.request_id = format!("{}-start{attempt}", base_req.request_id);
            }
            match self.server_start(task.dst.side, &req) {
                Ok(_) => return Ok(req),
                Err(e) => {
                    errors.push(format!("第{}次: {e}", attempt + 1));
                    if attempt < max_retries {
                        // server_start 的各实现本身会做失败补偿；这里再用同一
                        // request_id 做一次幂等确认，作为“允许占用同端口的新
                        // request 开始”的硬门槛。无法确认退出时绝不盲目重试。
                        if let Err(cleanup_error) = self.server_stop_confirmed(
                            task.dst.side,
                            req.port,
                            &req.request_id,
                            Duration::ZERO,
                        ) {
                            errors.push(format!(
                                "第{}次失败后的 server 清理未确认，禁止继续占用端口 {} 重试: {}",
                                attempt + 1,
                                req.port,
                                cleanup_error
                            ));
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        }
        Err(errors.join("；"))
    }

    fn run_prepared_udp_flow(
        &self,
        prepared: PreparedUdpFlow,
        epoch: &Instant,
        live: &Arc<Mutex<HashMap<(usize, usize), LiveFlowState>>>,
    ) -> UdpFlowRun {
        if prepared.server_req.is_none() || prepared.client_req.is_none() {
            if let Ok(mut g) = live.lock() {
                let s = g
                    .entry((prepared.leg_pos, prepared.stream_pos))
                    .or_default();
                s.ended = true;
                s.error = prepared.server_error.clone();
            }
            return UdpFlowRun {
                leg_pos: prepared.leg_pos,
                stream_pos: prepared.stream_pos,
                task: prepared.task,
                raw_ok: false,
                runtime_failed: false,
                parsed: iperf::IperfParsed::default(),
                client: IperfClientOut {
                    output: prepared.server_error.clone(),
                    ..Default::default()
                },
                server_output: String::new(),
                events: vec![],
                retries: 0,
                full_attempts: 0,
                single_stream_exhausted: false,
                error: prepared.server_error,
            };
        }

        std::thread::sleep(Duration::from_millis(prepared.launch_delay_ms));
        let mut current_server_req = prepared.server_req.clone().unwrap();
        let client_req = prepared.client_req.clone().unwrap();
        let mut all_events = Vec::new();
        let mut all_client_output = Vec::new();
        let mut all_server_output = Vec::new();
        let mut final_client = IperfClientOut::default();
        let mut final_parsed = iperf::IperfParsed::default();
        let mut final_ok = false;
        let mut final_runtime_failed = false;
        let mut retries = 0usize;
        let mut full_attempts = 0usize;
        let mut cleanup_confirmed = false;
        let mut setup_error_seen = false;
        let mut final_error = String::new();

        let max_flow_retries = effective_udp_retries(
            self.cfg.iperf.rate_check.flow_retries as usize,
            prepared.strict_single_stream,
        );
        let retry_cutoff =
            Duration::from_secs(self.cfg.iperf.rate_check.startup_timeout_secs.max(1));
        for attempt in 0..=max_flow_retries {
            let attempt_start_ms = epoch.elapsed().as_millis() as u64;
            let key = (prepared.leg_pos, prepared.stream_pos);
            let live_ref = Arc::clone(live);
            let mut attempt_events: Vec<IperfFlowEvent> = Vec::new();
            let attempt_started = Instant::now();
            let client_request_id = lifecycle_request_id(
                &current_server_req.owner_id,
                "client",
                prepared.task.port,
                attempt,
            );
            let client = self.client_run_tracked(
                prepared.task.src.side,
                &client_req,
                &current_server_req.owner_id,
                &client_request_id,
                current_server_req.lease_secs,
                |mut event| {
                    event.elapsed_ms = event.elapsed_ms.saturating_add(attempt_start_ms);
                    if let Ok(mut g) = live_ref.lock() {
                        let state = g.entry(key).or_default();
                        apply_flow_event(state, &event);
                    }
                    attempt_events.push(event);
                },
            );
            all_events.extend(attempt_events);
            all_client_output.push(format!(
                "=== attempt {} ===\n{}",
                attempt + 1,
                client.output
            ));
            let stop = self.server_stop_confirmed(
                prepared.task.dst.side,
                prepared.task.port,
                &current_server_req.request_id,
                Duration::ZERO,
            );
            let (server_out, stop_ok) = match stop {
                Ok(out) => (out.output, true),
                Err(e) => (format!("server 停止未确认: {e}"), false),
            };
            let parsed = iperf::parse_output(&format!("{}\n{}", client.output, server_out));
            let tool_measurement = parsed.has_measurement();
            let client_setup_error = iperf_client_setup_error(&client);
            let process_started = client.process_started == Some(true);
            let client_cleanup_confirmed = client.cleanup_confirmed == Some(true);
            let safe_full_attempt = process_started
                && client_cleanup_confirmed
                && stop_ok
                && client_setup_error.is_none()
                && !client.cancelled;
            if safe_full_attempt {
                full_attempts += 1;
            }
            cleanup_confirmed = stop_ok && client_cleanup_confirmed;
            final_ok = tool_measurement && safe_full_attempt;
            final_runtime_failed = final_ok && (!client.ok || client.timed_out);
            final_client = client;
            final_parsed = parsed;
            all_server_output.push(format!("=== attempt {} ===\n{}", attempt + 1, server_out));
            if !stop_ok {
                setup_error_seen = true;
                final_error = "server 停止未确认，禁止在同端口继续重试".into();
                break;
            }
            if let Some(error) = client_setup_error {
                setup_error_seen = true;
                final_error = error;
                break;
            }
            if !process_started {
                setup_error_seen = true;
                final_error = "client 未明确证明底层进程已成功启动".into();
                break;
            }
            if !client_cleanup_confirmed {
                setup_error_seen = true;
                final_error = "client 未明确证明底层进程已 wait/reap，禁止复用端口".into();
                break;
            }
            // 只要本轮已有 iperf3 自身测量，就已经证明该方向灌通；后续由
            // runtime/loss/目标判定真实结果，不能继续重试并声称“无测量”。
            if tool_measurement {
                final_error = if final_runtime_failed {
                    final_client
                        .output
                        .lines()
                        .find(|line| line.to_ascii_lowercase().contains("error"))
                        .unwrap_or("iperf3 已有吞吐测量，但 client 未正常完成")
                        .to_string()
                } else {
                    String::new()
                };
                break;
            }

            final_error = if final_client.timed_out {
                "client 超时".into()
            } else if final_client.cancelled {
                "client 被取消".into()
            } else if final_client.output.trim().is_empty() {
                "client 未输出有效测量".into()
            } else {
                final_client
                    .output
                    .lines()
                    .find(|line| line.to_lowercase().contains("error"))
                    .unwrap_or("client 未产生有效测量")
                    .to_string()
            };

            let retryable = if prepared.strict_single_stream {
                // 单流硬门槛必须完成至少三次安全尝试；不受普通 startup
                // 截止或单次命令超时影响。显式取消/清理不确定时仍立即停下。
                attempt < max_flow_retries && safe_full_attempt
            } else {
                safe_full_attempt
                    && should_retry_udp_flow(
                        attempt,
                        max_flow_retries,
                        attempt_started.elapsed(),
                        retry_cutoff,
                        &final_client,
                    )
            };
            if !retryable {
                break;
            }

            retries += 1;
            if let Ok(mut g) = live.lock() {
                let state = g
                    .entry((prepared.leg_pos, prepared.stream_pos))
                    .or_default();
                state.retries += 1;
                state.ended = false;
                state.active = false;
                state.connected = false;
            }
            logln(&format!(
                "    [UDP流重试]{} {}-#{} 本轮未跑通，重新启动 server/client（{}/{}）",
                fmt_tag_bracket(&prepared.tag),
                if prepared.task.stream_idx == 0 && prepared.stream_pos == 0 {
                    "流"
                } else {
                    "并发流"
                },
                prepared.stream_pos + 1,
                retries,
                max_flow_retries
            ));
            all_events.push(IperfFlowEvent {
                kind: IperfEventKind::Retry,
                elapsed_ms: epoch.elapsed().as_millis() as u64,
                mbps: None,
                line: format!("group retry {retries}"),
            });
            let mut next_server_req = current_server_req.clone();
            next_server_req.request_id = lifecycle_request_id(
                &current_server_req.owner_id,
                "server",
                prepared.task.port,
                attempt + 1,
            );
            let server_retries =
                effective_udp_retries(UDP_SERVER_START_RETRIES, prepared.strict_single_stream);
            match self.start_udp_server_with_retry(&prepared.task, &next_server_req, server_retries)
            {
                Ok(started_req) => current_server_req = started_req,
                Err(e) => {
                    final_error = format!("重试时 server 启动失败: {e}");
                    break;
                }
            }
        }

        final_client.output = all_client_output.join("\n");
        if let Ok(mut g) = live.lock() {
            let state = g
                .entry((prepared.leg_pos, prepared.stream_pos))
                .or_default();
            state.ended = true;
            if final_ok {
                state.error.clear();
            } else if !final_error.is_empty() {
                state.error = final_error.clone();
            }
        }

        let single_stream_exhausted = prepared.strict_single_stream
            && !final_ok
            && !final_parsed.has_measurement()
            && full_attempts == max_flow_retries.saturating_add(1)
            && cleanup_confirmed
            && !final_client.cancelled
            && !setup_error_seen;
        UdpFlowRun {
            leg_pos: prepared.leg_pos,
            stream_pos: prepared.stream_pos,
            task: prepared.task,
            raw_ok: final_ok,
            runtime_failed: final_runtime_failed,
            parsed: final_parsed,
            client: final_client,
            server_output: all_server_output.join("\n"),
            events: all_events,
            retries: full_attempts.saturating_sub(1),
            full_attempts,
            single_stream_exhausted,
            error: final_error,
        }
    }

    fn run_udp_unit(
        &self,
        useq: usize,
        unit: &Unit,
        plans: &[UdpLegPlan],
        owner_id: &str,
        lease_secs: u64,
    ) -> Vec<LegOutcome> {
        let epoch = Instant::now();
        let total_flows: usize = plans.iter().map(|p| p.streams.len()).sum();
        logln(&format!(
            "  [UDP统一调度] {} 个方向，共 {} 条流：先准备全部 server，再交错起流",
            plans.len(),
            total_flows
        ));

        let max_streams = plans.iter().map(|p| p.streams.len()).max().unwrap_or(0);
        let rate_cfg = &self.cfg.iperf.rate_check;
        let mut launch_delays: HashMap<(usize, usize), u64> = HashMap::new();
        let mut slot = 0u64;
        for stream_pos in 0..max_streams {
            for (leg_pos, plan) in plans.iter().enumerate() {
                if stream_pos < plan.streams.len() {
                    let mode = plan.streams[stream_pos].rate_mode;
                    let stage_delay = if mode == RateMode::Discover {
                        discovery_stage(stream_pos, plan.streams.len())
                            .saturating_mul(rate_cfg.discovery_step_secs)
                            .saturating_mul(1_000)
                    } else {
                        0
                    };
                    launch_delays.insert(
                        (leg_pos, stream_pos),
                        stage_delay.saturating_add(
                            slot.saturating_mul(rate_cfg.launch_interval_ms.clamp(0, 1_000)),
                        ),
                    );
                    slot += 1;
                }
            }
        }
        let max_launch_delay_ms = launch_delays.values().copied().max().unwrap_or(0);

        let mut prepared: Vec<PreparedUdpFlow> = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            for (stream_pos, task) in plan.streams.iter().enumerate() {
                let strict_single_stream = plan.streams.len() == 1;
                let launch_delay_ms = launch_delays
                    .get(&(leg_pos, stream_pos))
                    .copied()
                    .unwrap_or(0);
                let remaining_launch_secs = max_launch_delay_ms
                    .saturating_sub(launch_delay_ms)
                    .div_ceil(1000);
                // duration 对用户表示有效测量时长。更早启动的流自动多跑，
                // 让 discover 阶梯、错峰、settle 和配置的快速重试后仍有共同窗口。
                let process_duration = task
                    .duration
                    .saturating_add(rate_cfg.startup_timeout_secs)
                    .saturating_add(rate_cfg.settle_secs)
                    .saturating_add(5)
                    .saturating_add(remaining_launch_secs);
                match self.build_iperf_requests(task, process_duration, owner_id, lease_secs, 0) {
                    Ok((server_req, client_req)) => prepared.push(PreparedUdpFlow {
                        leg_pos,
                        stream_pos,
                        tag: plan.tag.clone(),
                        task: task.clone(),
                        server_req: Some(server_req),
                        client_req: Some(client_req),
                        server_error: String::new(),
                        launch_delay_ms,
                        strict_single_stream,
                    }),
                    Err(e) => prepared.push(PreparedUdpFlow {
                        leg_pos,
                        stream_pos,
                        tag: plan.tag.clone(),
                        task: task.clone(),
                        server_req: None,
                        client_req: None,
                        server_error: e,
                        launch_delay_ms: 0,
                        strict_single_stream,
                    }),
                }
            }
        }

        prepared = std::thread::scope(|scope| {
            let handles: Vec<_> = prepared
                .into_iter()
                .map(|mut flow| {
                    scope.spawn(move || {
                        if let Some(req) = flow.server_req.clone() {
                            let server_retries = effective_udp_retries(
                                UDP_SERVER_START_RETRIES,
                                flow.strict_single_stream,
                            );
                            match catch_unwind(AssertUnwindSafe(|| {
                                self.start_udp_server_with_retry(&flow.task, &req, server_retries)
                            })) {
                                Ok(Ok(started_req)) => flow.server_req = Some(started_req),
                                Ok(Err(e)) => {
                                    flow.server_error = e;
                                    flow.server_req = None;
                                    flow.client_req = None;
                                }
                                Err(payload) => {
                                    flow.server_error = format!(
                                        "server 准备线程 panic: {}",
                                        panic_text(payload.as_ref())
                                    );
                                    flow.server_req = None;
                                    flow.client_req = None;
                                }
                            }
                        }
                        flow
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| unreachable!("准备线程已内部隔离 panic"))
                })
                .collect()
        });

        let server_ready = prepared
            .iter()
            .filter(|flow| flow.server_req.is_some())
            .count();
        logln(&format!(
            "    server 准备完成: {server_ready}/{total_flows}"
        ));

        let mut monitor_ids: HashMap<String, (Side, String, u64, String)> = HashMap::new();
        for plan in plans {
            for task in &plan.streams {
                for endpoint in [&task.src, &task.dst] {
                    let key = endpoint.key();
                    if monitor_ids.contains_key(&key) {
                        continue;
                    }
                    let before_ms = epoch.elapsed().as_millis() as u64;
                    match self.mon_start(endpoint.side, &endpoint.nic.name, owner_id, lease_secs) {
                        Ok((id, call_origin_ms)) => {
                            monitor_ids.insert(
                                key,
                                (
                                    endpoint.side,
                                    id,
                                    before_ms + call_origin_ms,
                                    endpoint.nic.name.clone(),
                                ),
                            );
                        }
                        Err(e) => logln(&format!(
                            "    ({} 网卡连续监控启动失败: {e})",
                            endpoint.brief()
                        )),
                    }
                }
            }
        }
        // 采集空闲基线，后续统计会从 RX/TX 样本中扣除中位背景流量。
        let background_secs = self.cfg.iperf.rate_check.background_secs.min(30);
        if !monitor_ids.is_empty() && background_secs > 0 {
            logln(&format!("    网卡基线采样 {background_secs}s..."));
            std::thread::sleep(Duration::from_secs(background_secs));
        }

        let live: Arc<Mutex<HashMap<(usize, usize), LiveFlowState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let results: Vec<UdpFlowRun> = std::thread::scope(|scope| {
            let handles: Vec<_> = prepared
                .into_iter()
                .map(|flow| {
                    let live = Arc::clone(&live);
                    let fallback = (
                        flow.leg_pos,
                        flow.stream_pos,
                        flow.task.clone(),
                        flow.server_req.clone(),
                    );
                    scope.spawn(move || {
                        catch_unwind(AssertUnwindSafe(|| {
                            self.run_prepared_udp_flow(flow, &epoch, &live)
                        }))
                        .unwrap_or_else(|payload| {
                            if let Some(req) = &fallback.3 {
                                let _ = self.server_stop_confirmed(
                                    fallback.2.dst.side,
                                    fallback.2.port,
                                    &req.request_id,
                                    Duration::ZERO,
                                );
                            }
                            UdpFlowRun {
                                leg_pos: fallback.0,
                                stream_pos: fallback.1,
                                task: fallback.2,
                                raw_ok: false,
                                runtime_failed: false,
                                parsed: iperf::IperfParsed::default(),
                                client: IperfClientOut {
                                    output: format!(
                                        "UDP 流线程 panic: {}",
                                        panic_text(payload.as_ref())
                                    ),
                                    ..Default::default()
                                },
                                server_output: String::new(),
                                events: vec![],
                                retries: 0,
                                full_attempts: 0,
                                single_stream_exhausted: false,
                                error: "UDP 流线程 panic".into(),
                            }
                        })
                    })
                })
                .collect();

            let mut monitor_status_disabled = HashSet::new();
            while handles.iter().any(|h| !h.is_finished()) {
                std::thread::sleep(Duration::from_secs(1));
                for (leg_pos, plan) in plans.iter().enumerate() {
                    let (connected, active, ended, iperf_mbps, errors) = {
                        let g = live.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        let mut connected = 0usize;
                        let mut active = 0usize;
                        let mut ended = 0usize;
                        let mut rate = 0.0;
                        let mut has_rate = false;
                        let mut errors = 0usize;
                        for stream_pos in 0..plan.streams.len() {
                            if let Some(state) = g.get(&(leg_pos, stream_pos)) {
                                connected += usize::from(state.connected);
                                active += usize::from(state.active && !state.ended);
                                ended += usize::from(state.ended);
                                if let Some(value) = active_iperf_rate(state) {
                                    rate += value;
                                    has_rate = true;
                                }
                                errors += usize::from(!state.error.is_empty());
                            }
                        }
                        (connected, active, ended, has_rate.then_some(rate), errors)
                    };
                    let mut monitor_error = String::new();
                    let nic_rx_mbps = plan.streams.first().and_then(|task| {
                        let key = task.dst.key();
                        let (side, id, _, _) = monitor_ids.get(&key)?;
                        if monitor_status_disabled.contains(&key) {
                            return None;
                        }
                        match self.mon_status(*side, id) {
                            Ok(status) => match status.latest_sample {
                                Some(sample) if sample.valid => Some(sample.rx_mbps),
                                Some(sample) => {
                                    monitor_error = if sample.error.is_empty() {
                                        "网卡样本无效".into()
                                    } else {
                                        sample.error
                                    };
                                    None
                                }
                                None => {
                                    monitor_error = "等待首个网卡样本".into();
                                    None
                                }
                            },
                            Err(error) => {
                                monitor_status_disabled.insert(key);
                                monitor_error = error;
                                None
                            }
                        }
                    });
                    logln(&format_iperf_progress(&IperfProgressSnapshot {
                        protocol: "UDP",
                        tag: &plan.tag,
                        active,
                        connected,
                        total: plan.streams.len(),
                        ended,
                        nic_rx_mbps,
                        iperf_mbps,
                        errors,
                        monitor_error,
                    }));
                }
            }
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| unreachable!("流线程已内部隔离 panic"))
                })
                .collect()
        });

        let mut monitor_outputs: HashMap<String, MonitorStopOut> = HashMap::new();
        let mut monitor_sample_files: HashMap<String, String> = HashMap::new();
        for (key, (side, id, start_offset_ms, iface)) in monitor_ids {
            match self.mon_stop(side, &id) {
                Ok(mut out) => {
                    for sample in &mut out.samples {
                        sample.elapsed_ms = sample.elapsed_ms.saturating_add(start_offset_ms);
                    }
                    let sample_file = self.save_monitor_samples(
                        owner_id,
                        side,
                        &iface,
                        &key,
                        start_offset_ms,
                        &out,
                    );
                    monitor_sample_files.insert(key.clone(), sample_file);
                    monitor_outputs.insert(key, out);
                }
                Err(e) => logln(&format!("    (网卡监控停止失败: {e})")),
            }
        }

        let windows = select_udp_effective_windows(
            plans,
            &results,
            &monitor_outputs,
            &self.cfg.iperf.rate_check,
        );
        for (leg_pos, window) in windows.per_leg.iter().enumerate() {
            logln(&format!(
                "    有效窗口[{}]: {:.1}s / {}s{}",
                plans
                    .get(leg_pos)
                    .map(|plan| plan.tag.as_str())
                    .unwrap_or("?"),
                window.available_secs,
                window.required_secs,
                if window.complete {
                    "（满足）"
                } else {
                    "（不足，不能正式判定）"
                }
            ));
        }
        if plans.len() > 1 {
            logln(&format!(
                "    双向并发重叠: {:.1}s{}",
                windows.concurrency_secs,
                if windows.concurrency_secs <= 0.0 {
                    "（两条腿没有真正同时在跑，各腿结论只代表单向条件）"
                } else {
                    ""
                }
            ));
        }

        let mut outcomes = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            let effective_window =
                windows
                    .per_leg
                    .get(leg_pos)
                    .cloned()
                    .unwrap_or_else(|| EffectiveWindow {
                        required_secs: plan.streams.first().map(|t| t.duration).unwrap_or(0),
                        ..Default::default()
                    });
            let leg_flows: Vec<&UdpFlowRun> =
                results.iter().filter(|r| r.leg_pos == leg_pos).collect();
            let n = plan.streams.len();
            let success = leg_flows.iter().filter(|r| r.raw_ok).count();
            let runtime_failures = leg_flows.iter().filter(|r| r.runtime_failed).count();
            let single_stream_exhausted = n == 1
                && leg_flows
                    .first()
                    .is_some_and(|flow| flow.single_stream_exhausted);
            let single_attempts = leg_flows
                .first()
                .map(|flow| flow.full_attempts)
                .unwrap_or(0);
            let first = &plan.streams[0];
            let required = required_udp_streams(
                n,
                &self.cfg.iperf.rate_check,
                first.rx_target_mbps,
                first.offered_mbps,
            );
            let first_active_ms = leg_flows
                .iter()
                .filter_map(|flow| flow_active_interval(flow).map(|v| v.0))
                .min()
                .unwrap_or(effective_window.start_ms);
            let baseline_cutoff_ms =
                iperf_baseline_cutoff_ms(leg_flows.iter().flat_map(|flow| flow.events.iter()));
            let rx_stats = monitor_outputs
                .get(&first.dst.key())
                .map(|out| monitor_rate_stats(out, &effective_window, true, baseline_cutoff_ms))
                .unwrap_or_default();
            let tx_stats = monitor_outputs
                .get(&first.src.key())
                .map(|out| monitor_rate_stats(out, &effective_window, false, baseline_cutoff_ms))
                .unwrap_or_default();
            let rx_avg = rx_stats.avg_mbps;
            let rate_present = rx_avg.map(|v| v > MIN_VALID_RX_MBPS).unwrap_or(false);
            let offered_floor = first.rx_target_mbps.map(|target| {
                target * (1.0 + self.cfg.iperf.rate_check.offered_headroom_pct.max(0.0) / 100.0)
            });
            let tx_sufficient = offered_floor
                .map(|floor| tx_stats.p10_mbps.map(|v| v >= floor).unwrap_or(false))
                .unwrap_or(true);
            let sample_coverage_sufficient = rate_sample_coverage_sufficient(
                &rx_stats,
                &tx_stats,
                first.rx_target_mbps.is_some(),
            );
            let rate_window_coverage_sufficient = rate_window_coverage_sufficient(
                &rx_stats,
                &tx_stats,
                first.rx_target_mbps.is_some(),
            );
            let rx_meets_target = first
                .rx_target_mbps
                .map(|target| {
                    rx_stats.avg_mbps.map(|v| v >= target).unwrap_or(false)
                        && rx_stats.p10_mbps.map(|v| v >= target).unwrap_or(false)
                })
                .unwrap_or(true);
            let udp_loss = aggregate_udp_loss(&leg_flows);
            let loss_ok = self
                .cfg
                .iperf
                .rate_check
                .max_udp_loss_pct
                .map(|limit| udp_loss.map(|value| value <= limit))
                .unwrap_or(Some(true));
            let (verdict, reason_code, reason_detail) = if success == 0 {
                let verdict = zero_udp_stream_verdict(n, single_stream_exhausted);
                if verdict == Verdict::RateFail {
                    (
                        verdict,
                        "SINGLE_UDP_STREAM_FAILED".to_string(),
                        format!(
                            "单流 UDP 在 {single_attempts} 次 client 尝试后仍未产生有效测量；该方向必须灌通"
                        ),
                    )
                } else {
                    (
                        verdict,
                        "NO_STREAM_STARTED".to_string(),
                        format!("0/{n} 条流产生有效测量；执行环境未完成 client 尝试"),
                    )
                }
            } else if runtime_failures > 0 {
                (
                    Verdict::RateFail,
                    "IPERF_RUNTIME_ERRORS".to_string(),
                    format!(
                        "{runtime_failures} 条流已有 iperf3 自身吞吐测量，但 client 非正常完成或超时"
                    ),
                )
            } else if required > n {
                (
                    Verdict::NotEvaluated,
                    "CONFIGURED_LOAD_TOO_LOW".to_string(),
                    format!("目标需要至少 {required} 条流，但只配置了 {n} 条"),
                )
            } else if success < required {
                (
                    Verdict::NotEvaluated,
                    "ACTIVE_STREAMS_LOW".to_string(),
                    format!("仅 {success}/{n} 条流成功，正式判定至少需要 {required} 条"),
                )
            } else if !effective_window.complete {
                (
                    Verdict::NotEvaluated,
                    "EFFECTIVE_WINDOW_SHORT".to_string(),
                    format!(
                        "本方向有效窗口 {:.1}s，要求 {}s{}",
                        effective_window.available_secs,
                        effective_window.required_secs,
                        lifecycle_rx_hint(monitor_outputs.get(&first.dst.key()))
                    ),
                )
            } else if rx_stats.stalled_ratio > 1.0 - MIN_RATE_SAMPLE_COVERAGE {
                // 与 evaluate_nic_rx 的同名判据保持一致：两条判定链在
                // 「采样是否可信」上必须给出相同结论，否则同一种故障在
                // TCP 和 UDP 路径上会被写成两种不同的原因码。
                (
                    Verdict::NotEvaluated,
                    "COUNTER_STALLED".to_string(),
                    format!(
                        "判定窗口内接收端 OS 网卡计数器有 {:.1}% 的时间零增长（采到了样本，\
                         但字节计数一直没推进），本轮平均速率不可信",
                        rx_stats.stalled_ratio * 100.0
                    ),
                )
            } else if !rate_present || !sample_coverage_sufficient {
                (
                    Verdict::NotEvaluated,
                    "SAMPLE_COVERAGE_LOW".to_string(),
                    format!(
                        "RX采样覆盖率 {:.1}%，TX采样覆盖率 {:.1}%{}，或无有效接收速率",
                        rx_stats.coverage * 100.0,
                        tx_stats.coverage * 100.0,
                        if first.rx_target_mbps.is_some() {
                            "（有目标时两端均要求至少 95%）"
                        } else {
                            ""
                        }
                    ),
                )
            } else if !rate_window_coverage_sufficient {
                (
                    Verdict::NotEvaluated,
                    "RATE_WINDOW_COVERAGE_LOW".to_string(),
                    format!(
                        "完整5秒滚动窗口覆盖不足（RX {:.1}%/P10={}，TX {:.1}%/P10={}，要求均至少95%），不能用少量窗口或跨周期恢复样本替代稳定性判定",
                        rx_stats.rolling_coverage * 100.0,
                        fmt_opt(rx_stats.p10_mbps),
                        tx_stats.rolling_coverage * 100.0,
                        fmt_opt(tx_stats.p10_mbps)
                    ),
                )
            } else if first.rx_target_mbps.is_none() && first.rate_mode == RateMode::Verify {
                (
                    Verdict::NotEvaluated,
                    "TARGET_MISSING".to_string(),
                    "verify 模式必须配置有效的 rate_targets_mbps，且当前路径没有自动 EVB 目标"
                        .to_string(),
                )
            } else if first.rx_target_mbps.is_none() {
                (
                    Verdict::Measured,
                    "TARGET_UNKNOWN".to_string(),
                    format!("{:?} 模式仅记录实际能力，不伪造 PASS/FAIL", first.rate_mode),
                )
            } else if loss_ok.is_none() {
                (
                    Verdict::NotEvaluated,
                    "UDP_LOSS_DATA_MISSING".to_string(),
                    "已配置 UDP 丢包门槛，但 iperf3 输出缺少 lost/total 数据".to_string(),
                )
            } else if !tx_sufficient {
                (
                    Verdict::NotEvaluated,
                    "OFFERED_LOAD_LOW".to_string(),
                    format!(
                        "TX-P10 {}，验证目标所需负载至少 {}",
                        fmt_opt(tx_stats.p10_mbps),
                        fmt_opt(offered_floor)
                    ),
                )
            } else if !rx_meets_target {
                let target = first.rx_target_mbps.unwrap_or_default();
                if rx_stats.avg_mbps.map(|v| v >= target).unwrap_or(false) {
                    // 与 TCP 路径同一口径：平均达标之后，判定窗口里每一个完整
                    // 5 秒都必须达标，掉坑一律 FAIL。两条链的结论不能分叉。
                    let detail = match rx_dropout(&rx_stats.rolling_series, target) {
                        Some(dropout) => dropout.describe(target),
                        None => format!(
                            "5秒滚动P10 {} 低于 {target}Mbps",
                            fmt_opt(rx_stats.p10_mbps)
                        ),
                    };
                    (
                        Verdict::RateFail,
                        "RX_UNSTABLE".to_string(),
                        format!("平均速率达到目标，但{detail}"),
                    )
                } else {
                    (
                        Verdict::RateFail,
                        "RX_BELOW_TARGET".to_string(),
                        format!(
                            "RX平均 {} 低于目标 {}Mbps",
                            fmt_opt(rx_stats.avg_mbps),
                            target
                        ),
                    )
                }
            } else if let Some(dropout) = first
                .rx_target_mbps
                .and_then(|target| rx_dropout(&rx_stats.rolling_series, target))
            {
                // 平均和 P10 都达标，但中间掉过坑。P10 看不见一次 5 秒断流
                // （175 秒里只占 3%），使用者却看得见。
                (
                    Verdict::RateFail,
                    "RX_DROPOUT".to_string(),
                    format!(
                        "平均与P10均达标，但{}",
                        dropout.describe(first.rx_target_mbps.unwrap_or_default())
                    ),
                )
            } else if loss_ok == Some(false) {
                (
                    Verdict::RateFail,
                    "UDP_LOSS_HIGH".to_string(),
                    format!(
                        "UDP平均丢包率 {:.3}% 超过限制 {:.3}%",
                        udp_loss.unwrap_or_default(),
                        self.cfg
                            .iperf
                            .rate_check
                            .max_udp_loss_pct
                            .unwrap_or_default()
                    ),
                )
            } else {
                (Verdict::Pass, String::new(), String::new())
            };
            // 「这条腿测到了多少」和「两条腿有没有真正并发」是两件事，必须
            // 分别说清楚。腿级窗口让前者不再被后者连坐，但如果不把后者显式
            // 写出来，读报告的人会把单向条件下的数字当成双向并发结果。
            let reason_detail = if plans.len() > 1 && windows.concurrency_secs <= 0.0 {
                let peers: Vec<&str> = plans
                    .iter()
                    .enumerate()
                    .filter(|(pos, _)| *pos != leg_pos)
                    .map(|(_, other)| other.tag.as_str())
                    .collect();
                let head = format!(
                    "并发重叠 0.0s（对向 {} 没有同时跑通，本行只代表单向条件下的实测）",
                    peers.join("/")
                );
                if reason_detail.is_empty() {
                    head
                } else {
                    format!("{head}；{reason_detail}")
                }
            } else {
                reason_detail
            };
            let discovery_table = if first.rate_mode == RateMode::Discover {
                monitor_outputs
                    .get(&first.dst.key())
                    .map(|out| active_rate_table(leg_pos, &leg_flows, out, first_active_ms))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            if !discovery_table.is_empty() {
                logln(&format!(
                    "    [{}] 负载阶梯观测:\n{}",
                    if plan.tag.is_empty() {
                        "UDP"
                    } else {
                        &plan.tag
                    },
                    discovery_table
                ));
            }
            logln(&format!(
                "    [{}] 模式={:?}，目标={}，流成功={success}/{n}，最低有效流数={required}，TX均值={}，TX-P10={}，RX均值={}，RX-P10={}，覆盖率={:.1}%，结果={}",
                if plan.tag.is_empty() {
                    "UDP"
                } else {
                    &plan.tag
                },
                first.rate_mode,
                fmt_opt(first.rx_target_mbps),
                fmt_opt(tx_stats.avg_mbps),
                fmt_opt(tx_stats.p10_mbps),
                fmt_opt(rx_avg),
                fmt_opt(rx_stats.p10_mbps),
                rx_stats.coverage * 100.0,
                verdict.label()
            ));

            let strict_single_failed =
                n == 1 && verdict == Verdict::RateFail && reason_code == "SINGLE_UDP_STREAM_FAILED";
            for flow in &leg_flows {
                let (flow_verdict, flow_reason_code, flow_reason_detail) =
                    udp_flow_detail_outcome(flow, strict_single_failed);
                let raw_log = self.save_iperf_raw_record(IperfRawArtifact {
                    owner_id,
                    lidx: plan.lidx,
                    stream_pos: flow.stream_pos,
                    tag: &plan.tag,
                    task: &flow.task,
                    client: &flow.client,
                    server_output: &flow.server_output,
                    events: &flow.events,
                    error: &flow.error,
                });
                let nic_samples = monitor_sample_files
                    .get(&flow.task.dst.key())
                    .cloned()
                    .unwrap_or_default();
                self.push_row(Row {
                    sort_key: (useq, plan.lidx, flow.stream_pos + 1, 0),
                    time: now_full(),
                    task_id: md5_hex(&format!("{}|{}|{}", unit.id, plan.tag, flow.stream_pos)),
                    parent_id: unit.id.clone(),
                    task: unit.title.clone(),
                    ip: if flow.task.v6 {
                        "V6".into()
                    } else {
                        "V4".into()
                    },
                    transport: "UDP".into(),
                    param: format!(
                        "{} (#{}; retry={})",
                        flow.task.profile_label,
                        flow.stream_pos + 1,
                        flow.retries
                    ),
                    src_pc: flow.task.src.pc.clone(),
                    src_iface: flow.task.src.nic.name.clone(),
                    src_ip: flow.task.src.nic.ipv4.clone(),
                    dst_pc: flow.task.dst.pc.clone(),
                    dst_iface: flow.task.dst.nic.name.clone(),
                    dst_ip: flow.task.dst.nic.ipv4.clone(),
                    verdict: flow_verdict,
                    execution_status: if flow.client.timed_out {
                        ExecutionStatus::TimedOut
                    } else if flow.client.cancelled {
                        ExecutionStatus::Cancelled
                    } else if flow.raw_ok || strict_single_failed {
                        ExecutionStatus::Completed
                    } else {
                        ExecutionStatus::Error
                    },
                    reason_code: flow_reason_code,
                    reason_detail: flow_reason_detail,
                    kind_label: if unit.bidir {
                        format!("★★双向灌包-{}(流明细)", plan.tag)
                    } else {
                        "灌包(流明细)".into()
                    },
                    tx_mbps: flow.parsed.best_sender(),
                    rx_mbps: flow.parsed.best_receiver(),
                    udp_loss: flow.parsed.udp_loss_pct,
                    requested_streams: 1,
                    active_streams: usize::from(flow.raw_ok),
                    required_streams: 1,
                    retry_count: flow.retries,
                    command: flow.client.cmd.clone(),
                    raw_log,
                    nic_samples,
                    raws: vec![
                        (
                            format!(
                                "iperf3 client{} 流#{} 输出",
                                fmt_tag(&plan.tag),
                                flow.stream_pos + 1
                            ),
                            format!("$ {}\n{}", flow.client.cmd, flow.client.output),
                        ),
                        (
                            format!(
                                "iperf3 server{} 流#{} 输出",
                                fmt_tag(&plan.tag),
                                flow.stream_pos + 1
                            ),
                            flow.server_output.clone(),
                        ),
                        (
                            format!("流事件{} #{}", fmt_tag(&plan.tag), flow.stream_pos + 1),
                            format_flow_events(&flow.events, &flow.error),
                        ),
                    ],
                    ..Default::default()
                });
            }

            let (screenshot_master, screenshot_agent) = if self.cfg.screenshot {
                self.take_screenshots(
                    &[first.dst.side, first.src.side],
                    &format!("{}_{}", unit.title, plan.tag),
                )
            } else {
                (String::new(), String::new())
            };
            let idx = self.push_row(Row {
                sort_key: (useq, plan.lidx, n + 1, 1),
                time: now_full(),
                task_id: md5_hex(&format!("{}|{}|grouptotal", unit.id, plan.tag)),
                parent_id: unit.id.clone(),
                task: unit.title.clone(),
                ip: if first.v6 { "V6".into() } else { "V4".into() },
                transport: "UDP".into(),
                param: format!(
                    "★组合计({} 共{}条流，成功{}，要求至少{})",
                    plan.name, n, success, required
                ),
                src_pc: first.src.pc.clone(),
                src_iface: first.src.nic.name.clone(),
                src_ip: first.src.nic.ipv4.clone(),
                dst_pc: first.dst.pc.clone(),
                dst_iface: first.dst.nic.name.clone(),
                dst_ip: first.dst.nic.ipv4.clone(),
                verdict,
                execution_status: if success == 0 {
                    ExecutionStatus::Error
                } else if success < n {
                    ExecutionStatus::Partial
                } else {
                    ExecutionStatus::Completed
                },
                reason_code: reason_code.clone(),
                reason_detail: reason_detail.clone(),
                kind_label: if unit.bidir {
                    format!("★组合计-{}", plan.tag)
                } else {
                    "★组合计".into()
                },
                rx_avg,
                requested_streams: n,
                active_streams: success,
                required_streams: required,
                retry_count: leg_flows.iter().map(|flow| flow.retries).sum(),
                target_mbps: first.rx_target_mbps,
                tx_avg: tx_stats.avg_mbps,
                tx_p10: tx_stats.p10_mbps,
                rx_p10: rx_stats.p10_mbps,
                rx_median: rx_stats.median_mbps,
                rx_p95: rx_stats.p95_mbps,
                rx_min: rx_stats.min_mbps,
                rx_max: rx_stats.max_mbps,
                effective_seconds: Some(
                    effective_window
                        .available_secs
                        .min(effective_window.required_secs as f64),
                ),
                required_seconds: Some(effective_window.required_secs as f64),
                sample_coverage: Some(rx_stats.coverage),
                window_start_ms: Some(effective_window.start_ms),
                window_end_ms: Some(effective_window.end_ms),
                baseline_mbps: Some(rx_stats.baseline_mbps),
                rolling_coverage: Some(rx_stats.rolling_coverage),
                udp_loss,
                screenshot_master,
                screenshot_agent,
                is_grouptotal: true,
                nic_samples: monitor_sample_files
                    .get(&first.dst.key())
                    .cloned()
                    .unwrap_or_default(),
                raws: if discovery_table.is_empty() {
                    vec![]
                } else {
                    vec![("streams_active -> RX 速率".into(), discovery_table)]
                },
                ..Default::default()
            });
            outcomes.push(LegOutcome {
                verdict,
                reason_code,
                reason_detail,
                rx_avg,
                main_rows: vec![idx],
                tag: plan.tag.clone(),
            });
        }
        outcomes
    }
}

/// v6 link-local 地址加 zone（仅 macOS 需要，Windows 不加）
fn add_zone(addr: &str, zone: &str, _side: Side) -> String {
    if cfg!(target_os = "macos") && !zone.is_empty() && addr.starts_with("fe80") {
        format!("{}%{}", addr, zone)
    } else {
        addr.to_string()
    }
}

fn fmt_tag(tag: &str) -> String {
    if tag.is_empty() {
        String::new()
    } else {
        format!("-{tag}")
    }
}

/// 日志用的方向前缀。双向单元两腿并行输出，缺了它就无法把 attempt/retry
/// 归属到 AB 还是 BA。
fn fmt_tag_bracket(tag: &str) -> String {
    if tag.is_empty() {
        String::new()
    } else {
        format!("[{tag}]")
    }
}

fn fmt_opt(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.3}Mbps"),
        None => "-".into(),
    }
}

fn format_ping_rtt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.3}")).unwrap_or_else(|| "-".into())
}

fn iperf_client_setup_error(client: &IperfClientOut) -> Option<String> {
    let detail = || {
        client
            .output
            .lines()
            .last()
            .filter(|line| !line.trim().is_empty())
            .unwrap_or("iperf3 client 执行环境错误")
            .to_string()
    };
    if client.cancelled {
        return Some(detail());
    }
    if client.process_started != Some(true) {
        return Some(format!("client 进程未确认启动：{}", detail()));
    }
    if client.cleanup_confirmed != Some(true) {
        return Some(format!("client 进程回收未确认：{}", detail()));
    }
    if client.timed_out {
        // 已确认进程启动和回收的 timeout 是一次完整、安全的无测量尝试。
        return None;
    }

    let lower = client.output.to_ascii_lowercase();
    let setup_marker = [
        "主控机未找到 iperf3",
        "远端异步作业启动失败",
        "远端异步作业查询失败",
        "非预期 job id",
        "已结束但缺少结果",
        "duration=",
        "启动命令失败",
        "创建流式命令",
        "等待子进程失败",
        "回收子进程失败",
        "parameter error",
        "invalid argument",
        "invalid option",
        "unrecognized option",
        "option requires an argument",
        "unable to parse",
        "cannot assign requested address",
        "unable to bind",
        "no such device",
        "无法识别的选项",
        "无法分配请求的地址",
        "unable to set socket buffer",
        "bad format",
    ]
    .iter()
    .any(|marker| lower.contains(&marker.to_ascii_lowercase()));
    setup_marker.then(detail)
}

fn cts_process_setup_error(client: &IperfClientOut) -> Option<(String, String)> {
    if client.cancelled {
        return Some((
            "CTSTRAFFIC_CLIENT_CANCELLED".into(),
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
        "CTSTRAFFIC_PROCESS_START_FAILED"
    } else if lower.contains("invalid argument")
        || lower.contains("invalid option")
        || lower.contains("无效参数")
    {
        "CTSTRAFFIC_ARGS_INVALID"
    } else if lower.contains("命令超时时间过大")
        || lower.contains("创建流式命令")
        || lower.contains("等待子进程失败")
        || lower.contains("回收子进程失败")
    {
        "CTSTRAFFIC_PROCESS_CONTROL_FAILED"
    } else {
        return None;
    };
    Some((
        code.into(),
        client
            .output
            .lines()
            .last()
            .unwrap_or("ctsTraffic 进程环境错误")
            .to_string(),
    ))
}

fn format_ctstraffic_attempts(
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

fn text_preview(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn row_has_usable_traffic_measurement(row: &Row) -> bool {
    if row.verdict == Verdict::SetupError
        || matches!(
            row.execution_status,
            ExecutionStatus::Error | ExecutionStatus::TimedOut | ExecutionStatus::Cancelled
        )
    {
        return false;
    }
    if crate::verdict::HARD_SINGLE_UDP_FAILURE_CODES.contains(&row.reason_code.as_str()) {
        // 这两个专用硬失败的定义就是“工具自身没有任何吞吐证据”；即使
        // 同网卡存在背景流量，也必须继续触发故障诊断。
        return false;
    }
    let usable_rate =
        |value: Option<f64>| value.is_some_and(|rate| rate.is_finite() && rate > MIN_VALID_RX_MBPS);
    let tool_measurement =
        usable_rate(row.tx_mbps) || usable_rate(row.rx_mbps) || row.active_streams > 0;
    if row.transport.starts_with("CTS/") {
        // CTS 是否起流只认工具自身 rate/bytes/frame 派生出的字段；NIC RX
        // 只用于已起流后的产品目标验证，不能把背景流量补成 CTS 测量。
        return tool_measurement;
    }
    usable_rate(row.rx_avg) || tool_measurement || usable_rate(row.tx_avg)
}

fn aggregate_unit_verdict(outcomes: &[LegOutcome]) -> Verdict {
    // 优先级的唯一定义在 crate::verdict::aggregate_verdict —— 报告侧的回退聚合
    // 走同一个函数，两边不会再分叉。
    aggregate_verdict(
        outcomes
            .iter()
            .map(|outcome| (outcome.verdict, outcome.reason_code.as_str())),
    )
}

fn aggregate_direction_streams(directions: &[DirectionSummary]) -> Option<StreamCounts> {
    directions
        .iter()
        .filter_map(|direction| direction.streams)
        .fold(None, |total: Option<StreamCounts>, counts| {
            Some(match total {
                Some(total) => StreamCounts {
                    requested: total.requested.saturating_add(counts.requested),
                    active: total.active.saturating_add(counts.active),
                    required: total.required.saturating_add(counts.required),
                },
                None => counts,
            })
        })
}

fn populate_peer_rx(rows: &mut [Row], outcomes: &[LegOutcome]) {
    let ab = outcomes
        .iter()
        .position(|outcome| outcome.tag.eq_ignore_ascii_case("ab"));
    let ba = outcomes
        .iter()
        .position(|outcome| outcome.tag.eq_ignore_ascii_case("ba"));
    if let (Some(ab), Some(ba)) = (ab, ba) {
        for (me, other) in [(ab, ba), (ba, ab)] {
            if let Some(rx) = outcomes[other].rx_avg {
                for row_index in &outcomes[me].main_rows {
                    if let Some(row) = rows.get_mut(*row_index) {
                        row.peer_rx = format!(
                            "{rx:.3} Mbps ({})",
                            outcomes[other].tag.to_ascii_uppercase()
                        );
                    }
                }
            }
        }
    }
}

fn outcome_matching_verdict(outcomes: &[LegOutcome], verdict: Verdict) -> Option<&LegOutcome> {
    if verdict == Verdict::SetupError {
        if let Some(outcome) = outcomes
            .iter()
            .find(|outcome| outcome.reason_code == "CTSTRAFFIC_ARGS_INVALID")
        {
            return Some(outcome);
        }
    }
    if verdict == Verdict::RateFail {
        if let Some(outcome) = outcomes
            .iter()
            .find(|outcome| is_hard_single_udp_failure(outcome))
        {
            return Some(outcome);
        }
    }
    outcomes.iter().find(|outcome| outcome.verdict == verdict)
}

fn is_hard_single_udp_failure(outcome: &LegOutcome) -> bool {
    crate::verdict::is_hard_single_udp_failure(outcome.verdict, &outcome.reason_code)
}

/// 在网卡 RX 判定之上叠加 ctsTraffic 的 UDP 丢帧门槛。
///
/// 顺序对齐 iperf3 路径：只有当网卡侧已经完成一次真正的目标比对
/// （Pass/RateFail/Unstable）时才评估丢帧；采样不足、目标缺失或未知
/// （NotEvaluated/Measured）时原样返回，不把环境问题写成 CPE 丢帧超限。
/// 已配置门槛却缺少丢帧数据时，缺的是判定依据本身，因此优先于速率结论。
fn cts_apply_udp_loss(
    nic: (Verdict, String, String),
    is_udp: bool,
    loss_limit: Option<f64>,
    loss: Option<f64>,
) -> (Verdict, String, String) {
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
            "CTSTRAFFIC_UDP_LOSS_DATA_MISSING".to_string(),
            "已配置 UDP 丢帧门槛，但 ctsTraffic 输出缺少 dropped frames 数据".into(),
        );
    };
    if verdict == Verdict::Pass && actual > limit {
        return (
            Verdict::RateFail,
            "CTSTRAFFIC_UDP_LOSS_HIGH".to_string(),
            format!("CTS UDP 丢帧率 {actual:.3}% 超过限制 {limit:.3}%"),
        );
    }
    (verdict, code, detail)
}

#[cfg(test)]
fn count_retry_events(events: &[IperfFlowEvent]) -> usize {
    events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Retry)
        .count()
}

fn should_retry_udp_flow(
    attempt: usize,
    max_retries: usize,
    elapsed: Duration,
    startup_timeout: Duration,
    client: &IperfClientOut,
) -> bool {
    attempt < max_retries && elapsed <= startup_timeout && !client.timed_out && !client.cancelled
}

fn effective_udp_retries(configured_retries: usize, strict_single_stream: bool) -> usize {
    if strict_single_stream {
        configured_retries.max(SINGLE_UDP_MIN_ATTEMPTS.saturating_sub(1) as usize)
    } else {
        configured_retries
    }
}

fn cts_attempt_budget(configured_retries: usize, strict_single_udp: bool) -> usize {
    if strict_single_udp {
        effective_udp_retries(configured_retries, true).saturating_add(1)
    } else {
        1
    }
}

fn cts_baseline_cutoff_ms(attempts: &[CtsAttemptRun]) -> u64 {
    attempts
        .iter()
        .flat_map(|attempt| attempt.events.iter())
        .filter(|event| event.kind == IperfEventKind::Started)
        .map(|event| event.elapsed_ms)
        .min()
        .unwrap_or(0)
}

fn midpoint_ms(before_ms: u64, after_ms: u64) -> u64 {
    before_ms.saturating_add(after_ms.saturating_sub(before_ms) / 2)
}

fn remote_job_origin_ms(response_elapsed_ms: u64, remote_elapsed_ms: u64) -> u64 {
    let latest_start_ms = if remote_elapsed_ms > 0 {
        response_elapsed_ms.saturating_sub(remote_elapsed_ms)
    } else {
        response_elapsed_ms
    };
    midpoint_ms(0, latest_start_ms)
}

fn align_monitor_samples(out: &mut MonitorStopOut, start_offset_ms: u64) {
    for sample in &mut out.samples {
        sample.elapsed_ms = sample.elapsed_ms.saturating_add(start_offset_ms);
    }
}

fn cts_monitor_runtime_issue(
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
            code: "CTSTRAFFIC_MONITOR_NO_SAMPLES".into(),
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
            code: "CTSTRAFFIC_MONITOR_RUNTIME_ERROR".into(),
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

fn cts_monitor_issue_verdict(issue: &CtsMonitorIssue) -> Option<(Verdict, String, String)> {
    issue.affects_verdict.then(|| {
        (
            if issue.setup_error {
                Verdict::SetupError
            } else {
                Verdict::NotEvaluated
            },
            issue.code.clone(),
            issue.detail.clone(),
        )
    })
}

fn cts_effective_window(
    events: &[IperfFlowEvent],
    required_secs: u64,
    status_update_ms: u64,
) -> EffectiveWindow {
    let required_ms = required_secs.saturating_mul(1_000);
    let Some(end_ms) = events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Ended)
        .map(|event| event.elapsed_ms)
        .max()
    else {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    };
    let started_ms = events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Started && event.elapsed_ms < end_ms)
        .map(|event| event.elapsed_ms)
        .max();
    let attempt_floor = started_ms.unwrap_or(0);
    let first_traffic_ms = events
        .iter()
        .filter(|event| {
            event.kind == IperfEventKind::Traffic
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms < end_ms
                && event.mbps.unwrap_or(0.0) > 0.0
        })
        .map(|event| event.elapsed_ms)
        .min();
    let connected_ms = events
        .iter()
        .filter(|event| {
            event.kind == IperfEventKind::Connected
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms < end_ms
        })
        .map(|event| event.elapsed_ms)
        .max();
    let status_inferred_ms = first_traffic_ms.map(|traffic_ms| {
        traffic_ms
            .saturating_sub(status_update_ms)
            .max(attempt_floor)
    });
    let event_start_ms = match (connected_ms, status_inferred_ms) {
        (Some(connected), Some(status)) => Some(connected.max(status)),
        (connected, status) => connected.or(status),
    };
    let spans_required = |start_ms: u64| {
        end_ms > start_ms
            && end_ms
                .saturating_sub(start_ms)
                .saturating_add(CTS_TIMELINE_TOLERANCE_MS)
                >= required_ms
    };

    // 首条状态行表示前一个 StatusUpdate 周期，通常比 Connection 更接近数据起点。
    // Total Time、正常退出和疑似块缓冲都不能证明纯数据时长；事件证据不足时
    // 保守返回短窗口，避免把启动或握手空窗计入 NIC 平均值。
    let complete_start_ms = event_start_ms
        .filter(|start_ms| spans_required(*start_ms))
        .or_else(|| connected_ms.filter(|start_ms| spans_required(*start_ms)));
    let start_ms = complete_start_ms.unwrap_or_else(|| {
        first_traffic_ms
            .or(connected_ms)
            .or(started_ms)
            .unwrap_or(end_ms)
    });
    let available_ms = end_ms.saturating_sub(start_ms);
    let complete = complete_start_ms.is_some()
        && available_ms.saturating_add(CTS_TIMELINE_TOLERANCE_MS) >= required_ms;
    let scored_end_ms = if complete {
        start_ms.saturating_add(required_ms).min(end_ms)
    } else {
        end_ms
    };
    EffectiveWindow {
        start_ms,
        end_ms: scored_end_ms,
        available_secs: available_ms as f64 / 1_000.0,
        required_secs,
        complete,
    }
}

fn cts_stop_process_evidence(stop: &Result<CtsTrafficStopOut, String>) -> (bool, bool) {
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
fn cts_server_pre_stop_failures(stop: &Result<CtsTrafficStopOut, String>) -> (bool, bool) {
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

fn cts_attempt_is_safe_full(attempt: &CtsAttemptRun) -> bool {
    attempt.full_attempt
        && attempt.client.process_started == Some(true)
        && attempt.client.cleanup_confirmed == Some(true)
        && attempt.cleanup_confirmed
        && attempt.setup_error.is_none()
        && !attempt.client.cancelled
        && !attempt.server_unexpected_failure
}

fn cts_should_retry_after_last(
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

fn select_cts_attempt_index(attempts: &[CtsAttemptRun]) -> Option<usize> {
    attempts
        .iter()
        .position(|attempt| attempt.traffic_established)
        .or_else(|| attempts.len().checked_sub(1))
}

fn cts_full_attempts(attempts: &[CtsAttemptRun]) -> usize {
    attempts
        .iter()
        .filter(|attempt| cts_attempt_is_safe_full(attempt))
        .count()
}

fn cts_retry_count(attempts: &[CtsAttemptRun]) -> usize {
    cts_full_attempts(attempts).saturating_sub(1)
}

fn cts_single_udp_exhausted(
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

fn cts_server_unexpected_setup_error(
    server_unexpected_failure: bool,
    traffic_established: bool,
    server_output: &str,
) -> Option<(String, String)> {
    (server_unexpected_failure && !traffic_established).then(|| {
        (
            "CTSTRAFFIC_SERVER_FAILED".into(),
            server_output
                .lines()
                .last()
                .filter(|line| !line.trim().is_empty())
                .unwrap_or("ctsTraffic server 在停止请求前异常退出")
                .to_string(),
        )
    })
}

fn cts_runtime_failure_verdict(
    attempt: &CtsAttemptRun,
    runtime_errors: u64,
    client_expected_completion: bool,
) -> Option<(Verdict, String, String)> {
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
    Some((
        Verdict::RateFail,
        "CTSTRAFFIC_RUNTIME_ERRORS".into(),
        detail,
    ))
}

fn zero_udp_stream_verdict(requested: usize, attempts_exhausted: bool) -> Verdict {
    if requested == 1 && attempts_exhausted {
        Verdict::RateFail
    } else {
        Verdict::SetupError
    }
}

fn required_udp_streams(
    requested: usize,
    rate_cfg: &RateCheckCfg,
    target_mbps: Option<f64>,
    per_stream_mbps: Option<f64>,
) -> usize {
    if requested <= 1 {
        return requested;
    }
    let min_concurrent = (rate_cfg.min_concurrent_streams as usize).clamp(1, requested);
    // 用“允许失败数向上取整”体现用户容错：
    // ratio=0.90 时，5 条允许 1 条失败，20 条允许 2 条失败；
    // 2 条仍受 min_concurrent=2 约束，必须两条都通。
    let ratio = rate_cfg.min_active_ratio.clamp(0.0, 1.0);
    let allowed_failures = ((requested as f64) * (1.0 - ratio)).ceil() as usize;
    let fidelity_required = requested.saturating_sub(allowed_failures);
    let load_required = match (target_mbps, per_stream_mbps) {
        (Some(target), Some(per_stream)) if per_stream > 0.0 => {
            let offered = target * (1.0 + rate_cfg.offered_headroom_pct.max(0.0) / 100.0);
            (offered / per_stream).ceil() as usize
        }
        _ => 0,
    };
    min_concurrent.max(fidelity_required).max(load_required)
}

fn discovery_stage(stream_pos: usize, total: usize) -> u64 {
    if total <= 1 {
        return 0;
    }
    let ordinal = stream_pos + 1;
    let q1 = ((total as f64) * 0.25).ceil() as usize;
    let q2 = ((total as f64) * 0.50).ceil() as usize;
    let q3 = ((total as f64) * 0.75).ceil() as usize;
    if ordinal <= q1 {
        0
    } else if ordinal <= q2 {
        1
    } else if ordinal <= q3 {
        2
    } else {
        3
    }
}

fn format_flow_events(events: &[IperfFlowEvent], error: &str) -> String {
    let mut out = String::new();
    for event in events {
        out.push_str(&format!(
            "{:>8.3}s  {:?}{}  {}\n",
            event.elapsed_ms as f64 / 1000.0,
            event.kind,
            event
                .mbps
                .map(|v| format!(" {:.3}Mbps", v))
                .unwrap_or_default(),
            event.line
        ));
    }
    if !error.is_empty() {
        out.push_str(&format!("ERROR: {error}\n"));
    }
    out
}

fn apply_flow_event(state: &mut LiveFlowState, event: &IperfFlowEvent) {
    match event.kind {
        IperfEventKind::Connected => state.connected = true,
        IperfEventKind::Traffic => {
            state.active = true;
            state.last_mbps = event.mbps;
        }
        IperfEventKind::Retry => state.retries += 1,
        IperfEventKind::Error => state.error = event.line.clone(),
        IperfEventKind::Ended => {
            state.ended = true;
            state.active = false;
        }
        IperfEventKind::Started => {}
    }
}

fn active_iperf_rate(state: &LiveFlowState) -> Option<f64> {
    (state.active && !state.ended)
        .then_some(state.last_mbps)
        .flatten()
}

fn format_iperf_progress(snapshot: &IperfProgressSnapshot<'_>) -> String {
    let tag = if snapshot.tag.is_empty() {
        "单向"
    } else {
        snapshot.tag
    };
    let rate = |value: Option<f64>| {
        value
            .map(|value| format!("{value:.1}Mbps"))
            .unwrap_or_else(|| "-".into())
    };
    let mut line = format!(
        "    [灌包进度][{}][{}] active={}/{} connected={} ended={} nic-rx={} iperf={} err={}",
        snapshot.protocol,
        tag,
        snapshot.active,
        snapshot.total,
        snapshot.connected,
        snapshot.ended,
        rate(snapshot.nic_rx_mbps),
        rate(snapshot.iperf_mbps),
        snapshot.errors
    );
    if !snapshot.monitor_error.is_empty() {
        line.push_str(&format!(
            " monitor={}",
            snapshot.monitor_error.replace(['\r', '\n'], " ")
        ));
    }
    line
}

fn is_live_progress_rate_line(line: &str, parallel_streams: usize) -> bool {
    let lower = line.to_lowercase();
    if lower.contains(" sender") || lower.contains(" receiver") {
        return false;
    }
    iperf_interval_ms(line).is_some() && (parallel_streams <= 1 || lower.contains("[sum]"))
}

fn tcp_parallel_streams(extra: &[String]) -> usize {
    extra
        .windows(2)
        .find_map(|pair| {
            pair[0]
                .eq_ignore_ascii_case("-p")
                .then(|| pair[1].parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(1)
        .max(1)
}

fn raw_iperf_filename(
    owner_id: &str,
    lidx: usize,
    stream_pos: usize,
    tag: &str,
    task: &IperfTask,
) -> String {
    format!(
        "iperf_raw_{}_l{:02}_s{:02}_{}_{}_p{}.log",
        sanitize(owner_id),
        lidx,
        stream_pos,
        if task.udp { "udp" } else { "tcp" },
        sanitize(if tag.is_empty() { "oneway" } else { tag }),
        task.port
    )
}

fn build_iperf_raw_record(
    task: &IperfTask,
    client: &IperfClientOut,
    server_output: &str,
    events: &[IperfFlowEvent],
    error: &str,
) -> String {
    format!(
        "# CPE iperf3 raw record\n\
# saved_at,{}\n\
# transport,{}\n\
# profile,{}\n\
# source,{} / {} / {}\n\
# destination,{} / {} / {}\n\
# port,{}\n\
# duration_secs,{}\n\
# client_ok,{}\n\
# client_timed_out,{}\n\
# client_cancelled,{}\n\
# error,{}\n\
\n=== CLIENT COMMAND ===\n$ {}\n\
\n=== CLIENT STDOUT+STDERR / ALL ATTEMPTS ===\n{}\n\
\n=== SERVER STDOUT+STDERR / ALL ATTEMPTS ===\n{}\n\
\n=== FLOW EVENTS ===\n{}",
        now_full(),
        if task.udp { "UDP" } else { "TCP" },
        task.profile_label,
        task.src.side.cn(),
        task.src.nic.name,
        task.src.nic.ipv4,
        task.dst.side.cn(),
        task.dst.nic.name,
        task.dst.nic.ipv4,
        task.port,
        task.duration,
        client.ok,
        client.timed_out,
        client.cancelled,
        error.replace(['\r', '\n'], " "),
        client.cmd,
        client.output,
        server_output,
        format_flow_events(events, error)
    )
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// `origin_offset_ms` 是把远端（或本地）采样零点对齐到本测试单元时间轴的偏移量。
///
/// 两台机器的系统时钟不要求同步，零点用 RPC 往返做有界估计：真实启动落在
/// `[0, latest_start]` 区间内，取中点，因此**不确定度的半宽正好等于该偏移本身**。
/// 共同有效窗口卡在 180.0/180.0 边界时，没有这个数就无法判断是真够还是对齐
/// 误差凑够的——所以把估计值和它的半宽一起写进表头。
fn build_monitor_samples_csv(
    endpoint: &str,
    iface: &str,
    origin_offset_ms: u64,
    out: &MonitorStopOut,
) -> String {
    let mut csv = format!(
        "# CPE OS NIC counter samples\n\
# endpoint,{}\n\
# interface,{}\n\
# origin_offset_ms,{}\n\
# origin_uncertainty_half_width_ms,{}\n\
# full_lifecycle_seconds,{:.6}\n\
# full_lifecycle_average_rx_mbps,{:.6}\n\
# full_lifecycle_average_tx_mbps,{:.6}\n\
elapsed_ms,interval_ms,rx_bytes,tx_bytes,rx_delta_bytes,tx_delta_bytes,rx_mbps,tx_mbps,valid,error\n",
        csv_field(endpoint),
        csv_field(iface),
        origin_offset_ms,
        origin_offset_ms,
        out.seconds,
        out.avg_mbps,
        out.tx_avg_mbps
    );
    for sample in &out.samples {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{:.6},{:.6},{},{}\n",
            sample.elapsed_ms,
            sample.interval_ms,
            sample.rx_bytes,
            sample.tx_bytes,
            sample.rx_delta_bytes,
            sample.tx_delta_bytes,
            sample.rx_mbps,
            sample.tx_mbps,
            sample.valid,
            csv_field(&sample.error)
        ));
    }
    if !out.errors.is_empty() {
        csv.push_str("# monitor_errors\n");
        for error in &out.errors {
            csv.push_str(&format!("# {}\n", csv_field(error)));
        }
    }
    csv
}

fn iperf_interval_ms(line: &str) -> Option<(u64, u64)> {
    fn seconds_to_ms(raw: &str) -> Option<u64> {
        if raw.is_empty()
            || !raw
                .chars()
                .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == ',')
        {
            return None;
        }
        let seconds = raw.replace(',', ".").parse::<f64>().ok()?;
        if !seconds.is_finite() || !(0.0..=u64::MAX as f64 / 1_000.0).contains(&seconds) {
            return None;
        }
        Some((seconds * 1_000.0).round() as u64)
    }

    let fields: Vec<&str> = line.split_whitespace().collect();
    fields.windows(2).find_map(|pair| {
        if pair[1] != "sec" {
            return None;
        }
        let (start, end) = pair[0].split_once('-')?;
        let start_ms = seconds_to_ms(start)?;
        let end_ms = seconds_to_ms(end)?;
        (end_ms > start_ms).then_some((start_ms, end_ms))
    })
}

fn flow_duration_is_plausible(start_ms: u64, end_ms: u64, expected_ms: u64) -> bool {
    end_ms > start_ms
        && end_ms
            .saturating_sub(start_ms)
            .saturating_add(FLOW_TIMELINE_TOLERANCE_MS)
            >= expected_ms
}

/// Return the earliest client-start boundary across the included attempts or
/// flows. Only samples that ended before any client could send traffic are
/// eligible as idle background. A retry boundary or an inferred traffic
/// window can both occur after traffic has already flowed, so neither may be
/// reused as the background cutoff.
fn iperf_baseline_cutoff_ms<'a>(events: impl IntoIterator<Item = &'a IperfFlowEvent>) -> u64 {
    events
        .into_iter()
        .filter(|event| event.kind == IperfEventKind::Started)
        .map(|event| event.elapsed_ms)
        .min()
        .unwrap_or(0)
}

fn iperf_active_interval(events: &[IperfFlowEvent], required_secs: u64) -> Option<(u64, u64)> {
    let latest_retry_ms = events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Retry)
        .map(|event| event.elapsed_ms)
        .max();
    let retry_cutoff = latest_retry_ms.unwrap_or(0);
    let end = events
        .iter()
        .rev()
        .find(|event| event.kind == IperfEventKind::Ended && event.elapsed_ms >= retry_cutoff)
        .map(|event| event.elapsed_ms)?;
    let expected_ms = required_secs.saturating_mul(1_000);

    let started = events
        .iter()
        .rev()
        .find(|event| {
            event.kind == IperfEventKind::Started
                && event.elapsed_ms >= retry_cutoff
                && event.elapsed_ms < end
        })
        .map(|event| event.elapsed_ms);
    let attempt_floor = started.unwrap_or(retry_cutoff);
    let connected = events
        .iter()
        .find(|event| {
            event.kind == IperfEventKind::Connected
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms < end
        })
        .map(|event| event.elapsed_ms);
    let traffic_events: Vec<&IperfFlowEvent> = events
        .iter()
        .filter(|event| {
            event.kind == IperfEventKind::Traffic
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms <= end
                && event.mbps.unwrap_or(0.0) > 0.0
        })
        .collect();
    let first_traffic = traffic_events.first().map(|event| event.elapsed_ms);

    // interval 行内的时间是 iperf 进程自己的测量时间，不受 stdout 块缓冲影响，
    // 是最可信的活跃区间来源，因此优先于任何事件到达时间。
    //
    // 即使行内区间短于用户要求的时长也必须采用：短就是短，应当由下游按
    // 「共同有效窗口不足」判定。若因为“不够长”而丢弃它，回退项反而是更长的
    // client 进程寿命（含 startup/settle/退出收尾），会把一次只测到 175 秒的
    // 短测量补成完整 180 秒窗口，并把启动爬升算进 RX 平均。
    let reported_interval = traffic_events
        .iter()
        .filter_map(|event| {
            iperf_interval_ms(&event.line)
                .map(|(start_ms, end_ms)| (end_ms.saturating_sub(start_ms), event.elapsed_ms))
        })
        // 最终汇总行覆盖的区间最长，正常也最后到达；按时长优先排序，避免
        // 逐秒 interval 行恰好排在汇总行之后时被当成整段测量。
        .max_by_key(|(duration_ms, event_elapsed_ms)| (*duration_ms, *event_elapsed_ms));
    if let Some((duration_ms, event_elapsed_ms)) = reported_interval {
        // 最终汇总行已经证明吞吐测量结束；它之后到 Ended 之间只剩
        // child wait、stdout reader join 等退出收尾，不能纳入网卡平均。
        let measured_end = event_elapsed_ms.min(end);
        let start = measured_end.saturating_sub(duration_ms).max(attempt_floor);
        if measured_end > start {
            return Some((start, measured_end));
        }
    }

    // 支持 --forceflush 时首条 Traffic 的到达时间接近真实时间；旧版会在退出时
    // 一次性吐出全部 interval，此时 active duration 会明显短于 task.duration。
    if let Some(start) =
        first_traffic.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }
    if let Some(start) =
        connected.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }
    if let Some(start) =
        started.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }
    if let Some(start) =
        latest_retry_ms.filter(|start| flow_duration_is_plausible(*start, end, expected_ms))
    {
        return Some((start, end));
    }

    // 测试确实提前结束时保留最保守的可观察起点，使有效窗口保持不足。
    let start = first_traffic.or(connected).or(started)?;
    (end > start).then_some((start, end))
}

fn iperf_effective_window(
    events: &[IperfFlowEvent],
    required_secs: u64,
    has_measurement: bool,
) -> EffectiveWindow {
    if !has_measurement {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    }
    let Some((start_ms, end_ms)) = iperf_active_interval(events, required_secs) else {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    };
    let available_ms = end_ms.saturating_sub(start_ms);
    let required_ms = required_secs.saturating_mul(1_000);
    let complete = available_ms.saturating_add(CTS_TIMELINE_TOLERANCE_MS) >= required_ms;
    EffectiveWindow {
        start_ms,
        end_ms: if complete {
            start_ms.saturating_add(required_ms).min(end_ms)
        } else {
            end_ms
        },
        available_secs: available_ms as f64 / 1_000.0,
        required_secs,
        complete,
    }
}

fn flow_active_interval(flow: &UdpFlowRun) -> Option<(u64, u64)> {
    if !flow.raw_ok {
        return None;
    }
    iperf_active_interval(&flow.events, flow.task.duration)
}

fn udp_flow_detail_outcome(
    flow: &UdpFlowRun,
    strict_single_failed: bool,
) -> (Verdict, String, String) {
    if flow.runtime_failed {
        (
            Verdict::RateFail,
            "IPERF_RUNTIME_ERRORS".into(),
            flow.error.clone(),
        )
    } else if flow.raw_ok {
        (
            Verdict::Measured,
            "FLOW_MEASURED".into(),
            "流量工具已产生吞吐测量；此行仅记录单流执行，单元验收以接收端 OS 网卡 RX 组合计为准"
                .into(),
        )
    } else if strict_single_failed {
        (
            Verdict::RateFail,
            "SINGLE_UDP_STREAM_FAILED".into(),
            flow.error.clone(),
        )
    } else {
        (
            Verdict::SetupError,
            "FLOW_FAILED".into(),
            flow.error.clone(),
        )
    }
}

/// 一个 UDP 单元里各条方向腿的判定窗口。
pub(crate) struct UdpUnitWindows {
    /// 每条腿各自的有效窗口，下标与 `plans` 对齐。
    pub per_leg: Vec<EffectiveWindow>,
    /// 各腿窗口的交集时长（秒）。
    ///
    /// **只用于说明「双向并发」这件事到底成没成立，不参与任何一条腿的判定。**
    pub concurrency_secs: f64,
}

/// 逐腿计算判定窗口，并另外给出各腿的重叠时长。
///
/// 这里曾经只产出**一个**全单元共用的窗口，做法是把所有腿的采样区间求交集
/// （`lower.max` / `upper.min`），再要求每个时刻**每条腿**都有足够活跃流。
/// 于是双向单元里只要有一条腿没跑通，交集就是空的，另一条腿哪怕整整三分钟
/// 都在满速跑，也会被写成「RX均值=- 覆盖率=0.0% NOT_EVALUATED」。
///
/// run_20260825_215915_7684 的任务 10/12/34/36 就是这样丢掉了 8 行数据，
/// 其中 unit-33-34 的接收端网卡 205/208 个样本有流量、均值 923.08Mbps，
/// CSV 就在盘上，报表却说没采到（见 .ai/DESIGN-v4.3.0.md D1）。
///
/// 一条腿失败是一条腿的事实，不能抹掉另一条腿测到的真实速率；而「两条腿没有
/// 真正并发」是另一件需要单独说清楚的事实——所以拆成两个返回值，而不是让前者
/// 沉默地吃掉后者。
fn select_udp_effective_windows(
    plans: &[UdpLegPlan],
    results: &[UdpFlowRun],
    monitors: &HashMap<String, MonitorStopOut>,
    rate_cfg: &RateCheckCfg,
) -> UdpUnitWindows {
    let required_secs = plans
        .iter()
        .flat_map(|plan| plan.streams.iter().map(|task| task.duration))
        .max()
        .unwrap_or(0);
    let per_leg: Vec<EffectiveWindow> = plans
        .iter()
        .enumerate()
        .map(|(leg_pos, plan)| {
            leg_effective_window(leg_pos, plan, results, monitors, rate_cfg, required_secs)
        })
        .collect();

    // 交集：任一腿窗口为空则并发时长为 0。
    let mut overlap_start = 0u64;
    let mut overlap_end = u64::MAX;
    for window in &per_leg {
        if window.end_ms <= window.start_ms {
            overlap_end = 0;
            break;
        }
        overlap_start = overlap_start.max(window.start_ms);
        overlap_end = overlap_end.min(window.end_ms);
    }
    let concurrency_secs = if per_leg.is_empty() || overlap_end <= overlap_start {
        0.0
    } else {
        overlap_end.saturating_sub(overlap_start) as f64 / 1000.0
    };

    UdpUnitWindows {
        per_leg,
        concurrency_secs,
    }
}

/// 单条方向腿的有效窗口：只看这条腿自己的活跃流和自己的接收端采样。
fn leg_effective_window(
    leg_pos: usize,
    plan: &UdpLegPlan,
    results: &[UdpFlowRun],
    monitors: &HashMap<String, MonitorStopOut>,
    rate_cfg: &RateCheckCfg,
    required_secs: u64,
) -> EffectiveWindow {
    let empty = EffectiveWindow {
        required_secs,
        ..Default::default()
    };
    let Some(first) = plan.streams.first() else {
        return empty;
    };
    // 这条腿的接收端 monitor 缺失，只让这条腿没结论；旧代码在这里直接
    // `return` 整个单元的零窗口，于是 mon11 那次监控丢失连带废掉了对向腿。
    let Some(out) = monitors.get(&first.dst.key()) else {
        return empty;
    };
    let Some(first_sample) = out.samples.iter().find(|sample| sample.valid) else {
        return empty;
    };
    let Some(last_sample) = out.samples.iter().rev().find(|sample| sample.valid) else {
        return empty;
    };
    let lower = first_sample.elapsed_ms;
    let upper = last_sample.elapsed_ms;
    if upper <= lower {
        return empty;
    }
    let sample_tolerance_ms = rate_cfg
        .sample_interval_ms
        .clamp(200, 5_000)
        .saturating_mul(2)
        .max(1_500);

    let required = required_udp_streams(
        plan.streams.len(),
        rate_cfg,
        first.rx_target_mbps,
        first.offered_mbps,
    );
    let eligible = |t: u64| -> bool {
        let active = results
            .iter()
            .filter(|flow| flow.leg_pos == leg_pos)
            .filter_map(flow_active_interval)
            .filter(|(start, end)| *start <= t && t < *end)
            .count();
        active >= required && nearest_valid_sample(out, t, sample_tolerance_ms).is_some()
    };

    let mut best_start = 0u64;
    let mut best_end = 0u64;
    let mut current_start: Option<u64> = None;
    let mut t = lower;
    while t <= upper {
        if eligible(t) {
            if current_start.is_none() {
                current_start = Some(t);
            }
        } else if let Some(start) = current_start.take() {
            if t.saturating_sub(start) > best_end.saturating_sub(best_start) {
                best_start = start;
                best_end = t;
            }
        }
        t = t.saturating_add(1_000);
    }
    if let Some(start) = current_start {
        let end = upper.saturating_add(1_000);
        if end.saturating_sub(start) > best_end.saturating_sub(best_start) {
            best_start = start;
            best_end = end;
        }
    }

    if best_end <= best_start {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    }

    let scored_start = best_start.saturating_add(rate_cfg.settle_secs.saturating_mul(1_000));
    let available_ms = best_end.saturating_sub(scored_start);
    let available_secs = available_ms as f64 / 1000.0;
    let complete = available_ms >= required_secs.saturating_mul(1_000);
    let scored_end = if complete {
        scored_start.saturating_add(required_secs.saturating_mul(1_000))
    } else {
        best_end
    };
    EffectiveWindow {
        start_ms: scored_start,
        end_ms: scored_end,
        available_secs,
        required_secs,
        complete,
    }
}

pub(crate) struct IperfFlowVerdictIn<'a> {
    pub raw_ok: bool,
    pub measurement: bool,
    pub effective_window: &'a EffectiveWindow,
    pub required_secs: u64,
    pub rate_mode: RateMode,
    pub rx_target_mbps: Option<f64>,
    pub rx_stats: &'a RateStats,
    pub tx_stats: &'a RateStats,
    /// client 输出的最后一行，用作 setup 错误的可读细节。
    pub client_tail: &'a str,
    /// 接收端 monitor 的完整采样输出，仅用于窗口不足时给一个定位数字。
    pub rx_monitor: Option<&'a MonitorStopOut>,
}

/// 有效窗口不足时补一句定位信息：接收端网卡在**整个采样生命周期**
/// （含起流前后）的平均速率。
///
/// 它绝不能进判定。生命周期含 startup / settle / 退出收尾，会把一次只测到
/// 175 秒的短测量补成完整窗口，并把启动爬升算进平均——这正是本项目明确
/// 放弃 process-lifetime 回退的原因，那条边界不能动。
///
/// 但「这一行没结论」和「这块网卡一个字节都没收到」是两件完全不同的事。
/// run_20260825_215915_7684 的任务 97 里，接收网卡 202/202 个样本都有流量、
/// 全程均值 487.1Mbps、峰值 1582.4Mbps，报表却只写「未采集」，读的人无从
/// 判断到底是没测到还是真的没流量。判定可以拒绝下结论，但不该把已经看到的
/// 东西藏起来。
fn lifecycle_rx_hint(out: Option<&MonitorStopOut>) -> String {
    let Some(out) = out.filter(|out| out.seconds > 0.0 && out.avg_mbps.is_finite()) else {
        return String::new();
    };
    format!(
        "；接收端网卡全程（{:.1}s，含起停）平均 {:.3}Mbps，仅供定位，不作判定依据",
        out.seconds, out.avg_mbps
    )
}

/// 单条 iperf3 流的判定链。
///
/// 抽成纯函数是为了让下面这个区分可以被单独测试：**「环境没搭起来」和
/// 「跑完了但最后一次结果交换失败」不是一回事**。
///
/// iperf3 经常在完整跑完全程之后，才在结果交换阶段报
/// `unable to send control message … Connection reset by peer`。此时接收端
/// 网卡计数器已经拿到了完整的正式口径，把它判成 `SETUP_ERROR` 等于让诊断
/// 口径的故障否决正式口径的结论——run_20260825_215915_7684 里 9 行
/// 125~1067Mbps 的实测就是这么丢的（见 .ai/DESIGN-v4.3.0.md D2）。
///
/// 判据用「有没有攒够要求时长的有效吞吐窗口」而不是匹配错误文本：窗口本身
/// 就是「这一轮到底测没测成」的既有权威答案，既不需要引入新的阈值常量，
/// 也不会随 iperf3 的措辞变化而失效。
pub(crate) fn iperf_flow_verdict(input: IperfFlowVerdictIn<'_>) -> (Verdict, String, String) {
    let IperfFlowVerdictIn {
        raw_ok,
        measurement,
        effective_window,
        required_secs,
        rate_mode,
        rx_target_mbps,
        rx_stats,
        tx_stats,
        client_tail,
        rx_monitor,
    } = input;

    let summary_lost_after_full_run = !raw_ok && measurement && effective_window.complete;

    if !raw_ok && !summary_lost_after_full_run {
        return (
            Verdict::SetupError,
            "IPERF_EXEC_FAILED".to_string(),
            client_tail.to_string(),
        );
    }
    if !measurement {
        return (
            Verdict::RateFail,
            "NO_VALID_MEASUREMENT".to_string(),
            "iperf3 已结束，但没有 rate/bytes 吞吐测量".into(),
        );
    }
    if !effective_window.complete {
        return (
            Verdict::NotEvaluated,
            "IPERF_EFFECTIVE_WINDOW_SHORT".to_string(),
            format!(
                "iperf3 真实流量事件窗口仅 {:.3}s，短于要求的 {}s；未把 server 启动、连接或清理时间计入平均速率{}",
                effective_window.available_secs,
                required_secs,
                lifecycle_rx_hint(rx_monitor)
            ),
        );
    }

    let (verdict, code, detail) = evaluate_nic_rx(rate_mode, rx_target_mbps, rx_stats, tx_stats);
    if !summary_lost_after_full_run {
        return (verdict, code, detail);
    }
    // 判定本身仍然完全由网卡口径决定——RX 低于目标照样 RATE_FAIL，RX 缺失
    // 照样 NOT_EVALUATED。这里只把「工具自报速率不可用」记进原因，并保留原始
    // rate 结论的 reason_code，别让 RX_BELOW_TARGET 这类信息被覆盖掉。
    // 该行的执行状态仍是 ExecutionStatus::Error，概览上显示成
    // 「MEASURED · ERROR」，不会看起来一切正常。
    (
        verdict,
        code,
        format!(
            "IPERF_SUMMARY_LOST: iperf3 已完成全程灌包，仅最后的结果交换失败，\
             接收端网卡口径有效、工具自报速率不可用（{}）。{detail}",
            client_tail.trim()
        ),
    )
}

fn aggregate_udp_loss(flows: &[&UdpFlowRun]) -> Option<f64> {
    let successful: Vec<&UdpFlowRun> = flows.iter().copied().filter(|flow| flow.raw_ok).collect();
    if successful.is_empty() {
        return None;
    }
    let counts: Vec<(u64, u64)> = successful
        .iter()
        .filter_map(|flow| {
            Some((
                flow.parsed.udp_lost_datagrams?,
                flow.parsed.udp_total_datagrams?,
            ))
        })
        .collect();
    if counts.len() != successful.len() {
        // 任何一条流缺计数就不给聚合值。此前这里回退到「对各流百分比取算术
        // 平均」，那是错误加权：100 个数据报丢 10% 和 900 个数据报丢 0%，
        // 真实聚合是 1%，平均出来却是 5%。宁可报「未知」也不报错的数。
        return None;
    }
    let lost: u64 = counts.iter().map(|(lost, _)| *lost).sum();
    let total: u64 = counts.iter().map(|(_, total)| *total).sum();
    (total > 0).then(|| lost as f64 * 100.0 / total as f64)
}

fn active_rate_table(
    leg_pos: usize,
    flows: &[&UdpFlowRun],
    out: &MonitorStopOut,
    first_active_ms: u64,
) -> String {
    let mut baseline_values: Vec<f64> = out
        .samples
        .iter()
        .filter(|sample| sample.valid && sample.elapsed_ms < first_active_ms)
        .map(|sample| sample.rx_mbps)
        .collect();
    baseline_values.sort_by(|a, b| a.total_cmp(b));
    let baseline = percentile(&baseline_values, 0.5).unwrap_or(0.0);
    let mut groups: HashMap<usize, Vec<f64>> = HashMap::new();
    for sample in out.samples.iter().filter(|sample| sample.valid) {
        let active = flows
            .iter()
            .filter(|flow| flow.leg_pos == leg_pos)
            .filter_map(|flow| flow_active_interval(flow))
            .filter(|(start, end)| *start <= sample.elapsed_ms && sample.elapsed_ms < *end)
            .count();
        if active > 0 {
            groups
                .entry(active)
                .or_default()
                .push((sample.rx_mbps - baseline).max(0.0));
        }
    }
    let mut keys: Vec<usize> = groups.keys().copied().collect();
    keys.sort_unstable();
    let mut lines = vec!["active_streams,samples,avg_rx_mbps,p10_rx_mbps".to_string()];
    for active in keys {
        let mut values = groups.remove(&active).unwrap_or_default();
        if values.is_empty() {
            continue;
        }
        let avg = values.iter().sum::<f64>() / values.len() as f64;
        values.sort_by(|a, b| a.total_cmp(b));
        let p10 = percentile(&values, 0.10).unwrap_or(0.0);
        lines.push(format!("{active},{},{avg:.3},{p10:.3}", values.len()));
    }
    lines.join("\n")
}

// ---------------- 结果库（RESUME 用） ----------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DbEnt {
    pub ok: bool,
    pub time: String,
    pub title: String,
}

pub struct ResultDb {
    path: PathBuf,
    map: HashMap<String, DbEnt>,
}

pub const RESUME_MAX_AGE_HOURS: i64 = 24;

fn resume_age_is_fresh(age: chrono::Duration) -> bool {
    age >= chrono::Duration::seconds(-60) && age < chrono::Duration::hours(RESUME_MAX_AGE_HOURS)
}

impl ResultDb {
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        ResultDb { path, map }
    }

    /// 24 小时内 PASS 过则返回该次时间
    pub fn fresh_pass(&self, id: &str) -> Option<String> {
        let e = self.map.get(id)?;
        if !e.ok {
            return None;
        }
        let t = chrono::NaiveDateTime::parse_from_str(&e.time, "%Y-%m-%d %H:%M:%S").ok()?;
        let now = chrono::Local::now().naive_local();
        let age = now.signed_duration_since(t);
        if resume_age_is_fresh(age) {
            Some(e.time.clone())
        } else {
            None
        }
    }

    pub fn set(&mut self, id: &str, ok: bool, title: &str) {
        self.map.insert(
            id.to_string(),
            DbEnt {
                ok,
                time: now_full(),
                title: title.to_string(),
            },
        );
    }

    /// 原子写（tmp + rename）
    pub fn save(&self) {
        let tmp = self.path.with_extension("tmp");
        if let Ok(text) = serde_json::to_string_pretty(&self.map) {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 仅测试用到的采样统计层符号；产品码不需要，放这里避免非测试构建报未用导入。
    use crate::master::builder::{Endpoint, PingPurpose, PingTask};
    use crate::master::rate_window::{
        rolling_time_window_series, RateStats, MIN_RATE_SAMPLE_COVERAGE,
    };
    use crate::protocol::NicInfo;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn unit_panic_is_converted_cleanup_runs_and_next_unit_can_continue() {
        let cleaned = std::sync::atomic::AtomicBool::new(false);
        let panic_outcomes = execute_unit_safely(
            || panic!("synthetic unit panic"),
            || {
                cleaned.store(true, Ordering::SeqCst);
                Ok(())
            },
        );
        assert!(cleaned.load(Ordering::SeqCst));
        assert_eq!(panic_outcomes.len(), 1);
        assert_eq!(panic_outcomes[0].reason_code, "UNIT_PANIC");

        let next_outcomes = execute_unit_safely(
            || {
                vec![LegOutcome {
                    verdict: Verdict::Pass,
                    reason_code: String::new(),
                    reason_detail: String::new(),
                    rx_avg: None,
                    main_rows: Vec::new(),
                    tag: String::new(),
                }]
            },
            || Err("synthetic cleanup failure".into()),
        );
        assert_eq!(next_outcomes.len(), 2);
        assert_eq!(next_outcomes[0].verdict, Verdict::Pass);
        assert_eq!(next_outcomes[1].reason_code, "RESOURCE_CLEANUP_FAILED");
    }

    fn endpoint(side: Side, name: &str, ip: &str) -> Endpoint {
        Endpoint {
            side,
            pc: side.cn().into(),
            nic: NicInfo {
                name: name.into(),
                role: "SGMII2.5G".into(),
                ipv4: ip.into(),
                speed_mbps: 2500,
                ..Default::default()
            },
        }
    }

    fn ctstraffic_task(udp: bool) -> CtsTrafficTask {
        CtsTrafficTask {
            v6: false,
            udp,
            profile_name: if udp {
                "cts_udp_b500m_c3".into()
            } else {
                "cts_tcp_w64k_c3".into()
            },
            profile_label: if udp {
                "CTS UDP -b 500m ×3流 (每流)".into()
            } else {
                "CTS TCP socket-buffer 64k ×3连接".into()
            },
            src: endpoint(Side::Master, "master0", "192.168.1.2"),
            dst: endpoint(Side::Agent, "agent0", "192.168.1.3"),
            port: 56_000,
            duration: 10,
            streams: 3,
            window_bytes: Some(64 * 1024),
            bits_per_second: udp.then_some(500_000_000),
            datagram_bytes: udp.then_some(1200),
            frame_rate: 100,
            buffer_depth_secs: 1,
            status_update_ms: 1_000,
            rate_mode: RateMode::Observe,
            rx_target_mbps: None,
            offered_mbps: udp.then_some(1_500.0),
            setup_error: None,
        }
    }

    fn ctstraffic_unit(id: &str, udp: bool) -> Unit {
        Unit {
            id: id.into(),
            title: if udp {
                "CTS UDP test".into()
            } else {
                "CTS TCP test".into()
            },
            bidir: false,
            legs: vec![Leg {
                tag: "ab".into(),
                kind: LegKind::CtsTraffic(ctstraffic_task(udp)),
            }],
            est_secs: 25,
        }
    }

    fn ctstraffic_attempt(attempt: usize, traffic_established: bool) -> CtsAttemptRun {
        CtsAttemptRun {
            attempt,
            client: IperfClientOut {
                ok: true,
                process_started: Some(true),
                cleanup_confirmed: Some(true),
                cmd: format!("ctsTraffic client attempt {}", attempt + 1),
                output: format!("CLIENT ATTEMPT {}", attempt + 1),
                ..Default::default()
            },
            server_output: format!("SERVER ATTEMPT {}", attempt + 1),
            server_unexpected_failure: false,
            traffic_window: EffectiveWindow {
                start_ms: attempt as u64 * 10_000 + 1_000,
                end_ms: attempt as u64 * 10_000 + 11_000,
                available_secs: 10.0,
                required_secs: 10,
                complete: true,
            },
            events: Vec::new(),
            parsed: ctstraffic::CtsTrafficParsed {
                recv_mbps: traffic_established.then_some(500.0),
                udp_successful_frames: traffic_established.then_some(1_000),
                ..Default::default()
            },
            traffic_established,
            full_attempt: true,
            cleanup_confirmed: true,
            setup_error: None,
        }
    }

    fn isolated_ctx(agent_port: u16) -> (Ctx, PathBuf) {
        let db_path = std::env::temp_dir().join(format!(
            "cpe_test_executor_{}_{}.json",
            std::process::id(),
            RESOURCE_OWNER_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let ctx = Ctx {
            topology: None,
            agent_host: "127.0.0.1".into(),
            agent_port,
            cfg: Config {
                screenshot: false,
                open_report: false,
                ..Default::default()
            },
            outdir: std::env::temp_dir(),
            transport: Arc::new(http_client::TcpTransport),
            clock: Arc::new(SystemClock),
            local_servers: IperfServerMgr::new(),
            local_cts_jobs: IperfClientJobMgr::new(),
            local_monitors: MonitorMgr::new(),
            rows: Mutex::new(Vec::new()),
            db: Mutex::new(ResultDb::load(db_path.clone())),
        };
        (ctx, db_path)
    }

    #[test]
    fn reliable_retry_elapsed_excludes_failed_attempts() {
        // 回归：start 时间轴只统计成功那次调用的耗时。
        // 若把三次可靠调用（含失败重试与 250ms 等待）的总时长都算进
        // response_elapsed，远端 job 零点会被整体偏移数秒。
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_worker = Arc::clone(&attempts);
        std::thread::spawn(move || {
            for rq in server.incoming_requests() {
                let n = attempts_worker.fetch_add(1, Ordering::SeqCst);
                let body = if n == 0 {
                    // 第一次调用模拟失败（连接被拒/超时由客户端侧体现）；
                    // 这里直接返回 500，让 agent_post 走 Err 分支进入重试。
                    "boom".to_string()
                } else {
                    ok_json(MonitorStartOut {
                        id: "mon-retry".into(),
                        elapsed_ms: 5,
                    })
                };
                let status_code = if n == 0 { 500 } else { 200 };
                let resp = tiny_http::Response::from_string(body).with_status_code(status_code);
                let _ = rq.respond(resp);
            }
        });

        let (ctx, db_path) = isolated_ctx(port);
        let t0 = Instant::now();
        let (out, attempt_elapsed) = ctx
            .agent_post_reliable_timed::<_, MonitorStartOut>(
                "/monitor/start",
                &MonitorStartReq {
                    iface: "retry-iface".into(),
                    interval_ms: 1000,
                    owner_id: "owner-retry".into(),
                    lease_secs: 0,
                },
                Duration::from_secs(5),
            )
            .expect("第二次调用应成功");
        let total_elapsed = t0.elapsed();
        assert_eq!(out.id, "mon-retry");
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "必须真的发生过一次重试");
        // 成功那次调用自身耗时必须远小于含重试等待的总时长。
        assert!(
            attempt_elapsed < total_elapsed - RELIABLE_HTTP_RETRY_DELAY,
            "成功调用耗时 {attempt_elapsed:?} 不应包含 {RELIABLE_HTTP_RETRY_DELAY:?} 的重试等待（总耗时 {total_elapsed:?}）"
        );
        // 且成功调用自身耗时应是亚秒级（第二次立刻成功）。
        assert!(attempt_elapsed < Duration::from_millis(200));
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn scripted_transport_retries_dropped_and_truncated_responses_with_fake_time() {
        let transport = Arc::new(http_client::ScriptedTransport::new());
        transport.push_for_path(
            "/monitor/start",
            http_client::ScriptedExchange::drop_response(),
        );
        transport.push_for_path(
            "/monitor/start",
            http_client::ScriptedExchange::truncated(200, r#"{"ok":true"#, 64),
        );
        transport.push_for_path(
            "/monitor/start",
            http_client::ScriptedExchange::response(
                200,
                ok_json(MonitorStartOut {
                    id: "mon-scripted".into(),
                    elapsed_ms: 37,
                }),
            ),
        );
        let clock = Arc::new(ManualClock::new());
        let (mut ctx, db_path) = isolated_ctx(1);
        ctx.transport = transport.clone();
        ctx.clock = clock.clone();

        let (out, successful_attempt_elapsed) = ctx
            .agent_post_reliable_timed::<_, MonitorStartOut>(
                "/monitor/start",
                &MonitorStartReq {
                    iface: "fake0".into(),
                    interval_ms: 1_000,
                    owner_id: "owner-scripted".into(),
                    lease_secs: 60,
                },
                Duration::from_secs(5),
            )
            .unwrap();

        assert_eq!(out.id, "mon-scripted");
        assert_eq!(successful_attempt_elapsed, Duration::ZERO);
        assert_eq!(clock.elapsed(), Duration::from_millis(500));
        let requests = transport.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests.windows(2).all(|pair| pair[0].body == pair[1].body));
        assert_eq!(transport.remaining(), 0);
        let _ = std::fs::remove_file(db_path);
    }

    // ---------------- P1 step 2：服务端副作用 + 丢响应幂等验收 ----------------

    /// 假 agent：按 request_id 幂等的 client job 注册表，镜像真实
    /// [`IperfClientJobMgr::start_request`] 的契约：
    /// 相同 request_id + 相同参数 → 复用同一 job（不重复创建）；
    /// 相同 request_id + 不同参数 → 拒绝；stop 幂等。
    /// 同时记录服务端副作用计数：spawned 是“实际创建 job 的次数”，
    /// 丢响应场景下响应被丢弃但副作用必须已经发生。
    #[derive(Default)]
    struct FakeClientAgent {
        spawned: AtomicUsize,
        start_attempts: AtomicUsize,
        statuses: AtomicUsize,
        stops: AtomicUsize,
        jobs: Mutex<HashMap<String, String>>,
    }

    impl FakeClientAgent {
        fn handle(
            &self,
            request: &http_client::HttpRequest,
        ) -> Result<http_client::HttpResponse, String> {
            let respond = |body: String| http_client::HttpResponse::new(200, body);
            match request.path.as_str() {
                "/iperf/client/start" => {
                    self.start_attempts.fetch_add(1, Ordering::SeqCst);
                    let start: IperfClientStartReq = serde_json::from_str(&request.body)
                        .map_err(|e| format!("start 请求解析失败: {e}"))?;
                    let fingerprint = format!(
                        "{}|{}",
                        start.owner_id,
                        serde_json::to_string(&start.request).map_err(|e| e.to_string())?
                    );
                    let mut jobs = self
                        .jobs
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if let Some(existing) = jobs.get(&start.request_id) {
                        if existing != &fingerprint {
                            return Ok(respond(err_json(&format!(
                                "iperf client request_id {} 的重复 start 参数不一致",
                                start.request_id
                            ))));
                        }
                        // 相同参数重复 start：复用，不创建新 job。
                        return Ok(respond(ok_json(IperfClientStartOut {
                            id: start.request_id.clone(),
                            elapsed_ms: 5,
                        })));
                    }
                    self.spawned.fetch_add(1, Ordering::SeqCst);
                    jobs.insert(start.request_id.clone(), fingerprint);
                    Ok(respond(ok_json(IperfClientStartOut {
                        id: start.request_id.clone(),
                        elapsed_ms: 5,
                    })))
                }
                "/iperf/client/status" => {
                    self.statuses.fetch_add(1, Ordering::SeqCst);
                    let req: IperfClientStatusReq = serde_json::from_str(&request.body)
                        .map_err(|e| format!("status 请求解析失败: {e}"))?;
                    Ok(respond(ok_json(IperfClientStatusOut {
                        id: req.id,
                        done: true,
                        next_cursor: 0,
                        events: vec![IperfFlowEvent {
                            kind: IperfEventKind::Ended,
                            elapsed_ms: 10_000,
                            ..Default::default()
                        }],
                        result: Some(IperfClientOut {
                            ok: true,
                            cleanup_confirmed: Some(true),
                            cmd: "fake client".into(),
                            output: "fake client ok".into(),
                            ..Default::default()
                        }),
                    })))
                }
                "/iperf/client/stop" => {
                    self.stops.fetch_add(1, Ordering::SeqCst);
                    let _req: IperfClientStopReq = serde_json::from_str(&request.body)
                        .map_err(|e| format!("stop 请求解析失败: {e}"))?;
                    Ok(respond(ok_json(IperfClientStopOut {
                        existed: true,
                        was_done: false,
                        terminated: true,
                        result: Some(IperfClientOut {
                            ok: true,
                            cleanup_confirmed: Some(true),
                            cmd: "fake client".into(),
                            output: "fake stop ok".into(),
                            ..Default::default()
                        }),
                    })))
                }
                _ => Err(format!("fake agent 未知路径 {}", request.path)),
            }
        }
    }

    /// 构造与测试共享虚拟时钟的脚本 transport，handler 即假 agent。
    fn fake_client_agent_transport(
        clock: &Arc<ManualClock>,
        agent: &Arc<FakeClientAgent>,
    ) -> http_client::ScriptedTransport {
        let agent = Arc::clone(agent);
        http_client::ScriptedTransport::with_handler(clock.clone(), move |request| {
            agent.handle(request)
        })
    }

    fn acc_start_req(request_id: &str, port: u16) -> IperfClientStartReq {
        IperfClientStartReq {
            request: IperfClientReq {
                dst: "10.0.0.2".into(),
                bind_ip: "10.0.0.1".into(),
                port,
                duration: 10,
                ..Default::default()
            },
            request_id: request_id.to_string(),
            owner_id: "owner-acc".into(),
            lease_secs: 0,
        }
    }

    /// P1 第一条验收测试：丢 start 响应不能重复创建 job。
    ///
    /// 同时验证三个契约：
    /// 1. Transport —— 响应在返回路径丢失，但请求已送达并产生服务端副作用；
    /// 2. 重试幂等 —— 相同 request_id 的可靠重试必须复用同一个 job，spawn 次数=1；
    /// 3. 资源清理 —— stop 可回收；重复 stop 幂等；不同参数的重复 start 必须拒绝。
    #[test]
    fn dropped_start_response_retries_idempotently_and_stop_reclaims() {
        let clock = Arc::new(ManualClock::new());
        let agent = Arc::new(FakeClientAgent::default());
        let transport = fake_client_agent_transport(&clock, &agent);
        // 第一次 start 响应在返回路径丢失（请求已送达，副作用已发生）；
        // 之后三次调用都直接交付 handler 的结果。
        transport.push_for_path(
            "/iperf/client/start",
            http_client::ScriptedExchange::drop_response(),
        );
        transport.push_for_path(
            "/iperf/client/start",
            http_client::ScriptedExchange::handler_response(),
        );
        transport.push_for_path(
            "/iperf/client/start",
            http_client::ScriptedExchange::handler_response(),
        );
        transport.push_for_path(
            "/iperf/client/start",
            http_client::ScriptedExchange::handler_response(),
        );

        // 两次 stop 各需一次脚本。
        transport.push_for_path(
            "/iperf/client/stop",
            http_client::ScriptedExchange::handler_response(),
        );
        transport.push_for_path(
            "/iperf/client/stop",
            http_client::ScriptedExchange::handler_response(),
        );
        let (mut ctx, db_path) = isolated_ctx(1);
        ctx.transport = Arc::new(transport.clone());
        ctx.clock = clock.clone();

        let start_req = acc_start_req("acc-start-1", 5201);
        let (out, attempt_elapsed) = ctx
            .agent_post_reliable_timed::<_, IperfClientStartOut>(
                "/iperf/client/start",
                &start_req,
                Duration::from_secs(5),
            )
            .expect("响应丢失后重试必须成功");
        assert_eq!(out.id, "acc-start-1", "重试必须返回同一个 job ID");
        assert_eq!(
            agent.spawned.load(Ordering::SeqCst),
            1,
            "spawn 次数必须是 1，不是 2"
        );
        assert_eq!(
            agent.start_attempts.load(Ordering::SeqCst),
            2,
            "第一次响应丢失后必须真的重试"
        );
        assert_eq!(
            attempt_elapsed,
            Duration::ZERO,
            "成功那次调用自身耗时不能计入失败等待"
        );
        // 丢响应耗尽 5s 虚拟超时 + 一次 250ms 重试等待，全程零真实 sleep。
        assert_eq!(
            clock.elapsed(),
            Duration::from_secs(5) + RELIABLE_HTTP_RETRY_DELAY
        );
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests.windows(2).all(|pair| pair[0].body == pair[1].body),
            "重试必须携带相同 request_id/body"
        );

        // 相同参数重复 start 是复用：直接返回同一 job，不再创建。
        let again = ctx
            .agent_post::<_, IperfClientStartOut>(
                "/iperf/client/start",
                &start_req,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(again.id, "acc-start-1");
        assert_eq!(agent.spawned.load(Ordering::SeqCst), 1);

        // 不同参数必须拒绝。
        let mut conflict = start_req.clone();
        conflict.request.port = 5202;
        let conflict_err = ctx
            .agent_post::<_, IperfClientStartOut>(
                "/iperf/client/start",
                &conflict,
                Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(
            conflict_err.contains("重复 start 参数不一致"),
            "不同参数的重复 start 必须拒绝: {conflict_err}"
        );

        // stop 回收资源。
        let stop = ctx
            .client_stop_confirmed("acc-start-1")
            .expect("stop 必须被确认");
        assert!(stop.terminated);
        assert_eq!(agent.stops.load(Ordering::SeqCst), 1);

        // 再次 stop 幂等：不产生新的资源错误。
        let stop_again = ctx
            .client_stop_confirmed("acc-start-1")
            .expect("重复 stop 必须仍然成功");
        assert!(stop_again.terminated);
        assert_eq!(agent.stops.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_file(db_path);
    }

    /// 全部 start 响应都丢失：主控必须明确失败（不能假成功），
    /// 幂等 agent 只创建一个 job，补偿清理仍能按 request_id 回收。
    #[test]
    fn all_start_responses_dropped_fails_explicitly_without_false_pass() {
        let clock = Arc::new(ManualClock::new());
        let agent = Arc::new(FakeClientAgent::default());
        let transport = fake_client_agent_transport(&clock, &agent);
        for _ in 0..RELIABLE_HTTP_ATTEMPTS {
            transport.push_for_path(
                "/iperf/client/start",
                http_client::ScriptedExchange::drop_response(),
            );
        }
        transport.push_for_path(
            "/iperf/client/stop",
            http_client::ScriptedExchange::handler_response(),
        );

        let (mut ctx, db_path) = isolated_ctx(1);
        ctx.transport = Arc::new(transport.clone());
        ctx.clock = clock.clone();

        let start_req = acc_start_req("acc-start-2", 5203);
        let err = ctx
            .agent_post_reliable_timed::<_, IperfClientStartOut>(
                "/iperf/client/start",
                &start_req,
                Duration::from_secs(5),
            )
            .expect_err("全部响应丢失必须明确失败，不能产生假成功");
        assert!(
            err.contains("第1次") && err.contains("第3次"),
            "错误必须列出每次重试: {err}"
        );
        assert_eq!(
            agent.spawned.load(Ordering::SeqCst),
            1,
            "三次丢响应也只创建一个 job（request_id 幂等）"
        );
        assert_eq!(
            agent.start_attempts.load(Ordering::SeqCst),
            RELIABLE_HTTP_ATTEMPTS
        );
        assert_eq!(
            clock.elapsed(),
            Duration::from_secs(5) * 3 + RELIABLE_HTTP_RETRY_DELAY * 2,
            "三次尝试之间有两次重试等待，全程虚拟"
        );

        // 主控补偿清理：按 request_id 直接 stop 依然能回收资源。
        let stop = ctx
            .client_stop_confirmed("acc-start-2")
            .expect("补偿清理 stop 必须被确认");
        assert!(stop.terminated);
        assert_eq!(agent.stops.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_file(db_path);
    }

    /// 丢请求：请求根本没送达 agent，因此不产生任何服务端副作用；
    /// 主控可靠重试后成功，spawn 恰好一次。
    #[test]
    fn dropped_start_request_leaves_no_side_effect_and_retry_succeeds() {
        let clock = Arc::new(ManualClock::new());
        let agent = Arc::new(FakeClientAgent::default());
        let transport = fake_client_agent_transport(&clock, &agent);
        transport.push_for_path(
            "/iperf/client/start",
            http_client::ScriptedExchange::drop_request(),
        );
        transport.push_for_path(
            "/iperf/client/start",
            http_client::ScriptedExchange::handler_response(),
        );

        let (mut ctx, db_path) = isolated_ctx(1);
        ctx.transport = Arc::new(transport.clone());
        ctx.clock = clock.clone();

        let start_req = acc_start_req("acc-start-3", 5204);
        let (out, _) = ctx
            .agent_post_reliable_timed::<_, IperfClientStartOut>(
                "/iperf/client/start",
                &start_req,
                Duration::from_secs(5),
            )
            .expect("丢请求重试后必须成功");
        assert_eq!(out.id, "acc-start-3");
        assert_eq!(
            agent.spawned.load(Ordering::SeqCst),
            1,
            "只有成功那次才创建 job"
        );
        assert_eq!(
            agent.start_attempts.load(Ordering::SeqCst),
            1,
            "丢请求时 handler 不应被调用（请求未送达）"
        );
        let _ = std::fs::remove_file(db_path);
    }

    /// 非对称延迟：请求 20ms、响应 900ms。时间轴必须用 agent 上报的 elapsed_ms
    /// 反推 job 起点，而不是用 RTT 中点（460ms）当作起点。
    #[test]
    fn asymmetric_delay_origin_uses_agent_elapsed_not_rtt_midpoint() {
        let clock = Arc::new(ManualClock::new());
        let transport = http_client::ScriptedTransport::with_clock(clock.clone());
        transport.push_for_path(
            "/monitor/start",
            http_client::ScriptedExchange::with_delays(
                Duration::from_millis(20),
                Duration::from_millis(900),
                http_client::ScriptedOutcome::Response(http_client::HttpResponse::new(
                    200,
                    ok_json(MonitorStartOut {
                        id: "mon-asym".into(),
                        elapsed_ms: 900,
                    }),
                )),
            ),
        );

        let (mut ctx, db_path) = isolated_ctx(1);
        ctx.transport = Arc::new(transport);
        ctx.clock = clock.clone();

        let (out, attempt_elapsed) = ctx
            .agent_post_reliable_timed::<_, MonitorStartOut>(
                "/monitor/start",
                &MonitorStartReq {
                    iface: "fake0".into(),
                    interval_ms: 1_000,
                    owner_id: "owner-asym".into(),
                    lease_secs: 0,
                },
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(attempt_elapsed, Duration::from_millis(920));
        let origin = remote_job_origin_ms(attempt_elapsed.as_millis() as u64, out.elapsed_ms);
        assert_eq!(
            origin, 10,
            "job 起点应接近请求到达时刻(20ms)，而不是 RTT 中点 460ms"
        );
        let _ = std::fs::remove_file(db_path);
    }

    /// 完整主控 client 流程：start（首次丢响应 → 幂等重试）→ status(done)
    /// → stop。最终报告必须为 ok（资源真实创建且清理确认），spawn 恰一次，
    /// 事件不因丢响应而丢失。
    #[test]
    fn full_scripted_client_flow_reports_ok_and_reclaims() {
        let clock = Arc::new(ManualClock::new());
        let agent = Arc::new(FakeClientAgent::default());
        let transport = fake_client_agent_transport(&clock, &agent);
        transport.push_for_path(
            "/iperf/client/start",
            http_client::ScriptedExchange::drop_response(),
        );
        transport.push_for_path(
            "/iperf/client/start",
            http_client::ScriptedExchange::handler_response(),
        );
        transport.push_for_path(
            "/iperf/client/status",
            http_client::ScriptedExchange::handler_response(),
        );
        transport.push_for_path(
            "/iperf/client/stop",
            http_client::ScriptedExchange::handler_response(),
        );

        let (mut ctx, db_path) = isolated_ctx(1);
        ctx.transport = Arc::new(transport.clone());
        ctx.clock = clock.clone();

        let events = Arc::new(Mutex::new(Vec::<IperfFlowEvent>::new()));
        let events_sink = Arc::clone(&events);
        let out = ctx.client_run_tracked(
            Side::Agent,
            &IperfClientReq {
                dst: "10.0.0.2".into(),
                bind_ip: "10.0.0.1".into(),
                port: 5205,
                duration: 10,
                ..Default::default()
            },
            "owner-full",
            "full-1",
            0,
            move |event| {
                events_sink
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event);
            },
        );

        assert!(out.ok, "资源真实创建并确认，报告必须为 PASS");
        assert_eq!(out.cleanup_confirmed, Some(true));
        assert_eq!(
            agent.spawned.load(Ordering::SeqCst),
            1,
            "start 首次丢响应后重试不能重复创建 job"
        );
        assert_eq!(agent.stops.load(Ordering::SeqCst), 1);
        assert_eq!(agent.statuses.load(Ordering::SeqCst), 1);
        let delivered = events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(delivered.len(), 1, "done 时尾部事件必须全部可见");
        assert_eq!(delivered[0].kind, IperfEventKind::Ended);
        assert_eq!(
            clock.elapsed(),
            Duration::from_secs(20) + RELIABLE_HTTP_RETRY_DELAY,
            "只有 start 丢响应那次耗尽虚拟超时(client_run 使用 20s 超时)"
        );
        let _ = std::fs::remove_file(db_path);
    }
    fn udp_plan(
        lidx: usize,
        tag: &str,
        count: usize,
        src: &Endpoint,
        dst: &Endpoint,
        duration: u64,
    ) -> UdpLegPlan {
        let streams = (0..count)
            .map(|stream_idx| IperfTask {
                v6: false,
                udp: true,
                profile_name: "udp_b500m".into(),
                profile_label: "UDP -b 500m".into(),
                src: src.clone(),
                dst: dst.clone(),
                port: 56_000 + (lidx * 100 + stream_idx) as u16,
                duration,
                extra: vec!["-b".into(), "500m".into()],
                stream_idx,
                rate_mode: RateMode::Observe,
                rx_target_mbps: None,
                offered_mbps: Some(500.0),
            })
            .collect();
        UdpLegPlan {
            lidx,
            tag: tag.into(),
            name: "udp_b500m".into(),
            streams,
        }
    }

    fn tcp_task(src: &Endpoint, dst: &Endpoint, port: u16) -> IperfTask {
        IperfTask {
            v6: false,
            udp: false,
            profile_name: "tcp_w64k_P2".into(),
            profile_label: "TCP -w 64k -P 2".into(),
            src: src.clone(),
            dst: dst.clone(),
            port,
            duration: 10,
            extra: vec!["-w".into(), "64k".into(), "-P".into(), "2".into()],
            stream_idx: 0,
            rate_mode: RateMode::Observe,
            rx_target_mbps: None,
            offered_mbps: None,
        }
    }

    fn udp_flow(
        leg_pos: usize,
        stream_pos: usize,
        task: &IperfTask,
        start_ms: u64,
        end_ms: u64,
        raw_ok: bool,
    ) -> UdpFlowRun {
        UdpFlowRun {
            leg_pos,
            stream_pos,
            task: task.clone(),
            raw_ok,
            runtime_failed: false,
            parsed: iperf::IperfParsed::default(),
            client: IperfClientOut::default(),
            server_output: String::new(),
            events: if raw_ok {
                vec![
                    IperfFlowEvent {
                        kind: IperfEventKind::Traffic,
                        elapsed_ms: start_ms,
                        mbps: Some(500.0),
                        line: "traffic".into(),
                    },
                    IperfFlowEvent {
                        kind: IperfEventKind::Ended,
                        elapsed_ms: end_ms,
                        line: "ended".into(),
                        ..Default::default()
                    },
                ]
            } else {
                vec![]
            },
            retries: 0,
            full_attempts: usize::from(raw_ok),
            single_stream_exhausted: false,
            error: String::new(),
        }
    }

    fn monitor_until(end_ms: u64, rx_mbps: f64, tx_mbps: f64) -> MonitorStopOut {
        MonitorStopOut {
            samples: (0..=end_ms / 1_000)
                .map(|second| MonitorSample {
                    elapsed_ms: second * 1_000,
                    interval_ms: 1_000,
                    rx_mbps,
                    tx_mbps,
                    valid: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn successful_udp_flow_detail_is_measured_while_unit_owns_acceptance() {
        let src = endpoint(Side::Master, "master0", "192.168.1.2");
        let dst = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let task = udp_plan(0, "", 1, &src, &dst, 10)
            .streams
            .into_iter()
            .next()
            .unwrap();
        let flow = udp_flow(0, 0, &task, 1_000, 11_000, true);

        let (verdict, code, detail) = udp_flow_detail_outcome(&flow, false);
        assert_eq!(verdict, Verdict::Measured);
        assert_eq!(code, "FLOW_MEASURED");
        assert!(detail.contains("单元验收"));
        assert_ne!(verdict, Verdict::Pass);
    }

    #[test]
    fn unit_summary_metrics_preserve_single_and_bidirectional_nic_rx() {
        let (ctx, db_path) = isolated_ctx(0);
        let ab_row = ctx.push_row(Row {
            task_id: "ab-flow".into(),
            parent_id: "bidir-unit".into(),
            kind_label: "★★双向灌包-ab".into(),
            src_pc: "master".into(),
            src_iface: "eth0".into(),
            src_ip: "192.168.1.2".into(),
            dst_pc: "agent".into(),
            dst_iface: "eth1".into(),
            dst_ip: "192.168.1.3".into(),
            verdict: Verdict::Pass,
            requested_streams: 3,
            active_streams: 3,
            required_streams: 2,
            rx_avg: Some(950.0),
            rx_p10: Some(940.0),
            target_mbps: Some(900.0),
            sample_coverage: Some(0.99),
            is_grouptotal: true,
            ..Default::default()
        });
        let ba_row = ctx.push_row(Row {
            task_id: "ba-flow".into(),
            parent_id: "bidir-unit".into(),
            kind_label: "★★双向灌包-ba".into(),
            src_pc: "agent".into(),
            src_iface: "eth1".into(),
            src_ip: "192.168.1.3".into(),
            dst_pc: "master".into(),
            dst_iface: "eth0".into(),
            dst_ip: "192.168.1.2".into(),
            verdict: Verdict::RateFail,
            requested_streams: 2,
            active_streams: 2,
            required_streams: 2,
            rx_avg: Some(780.0),
            rx_p10: Some(760.0),
            target_mbps: Some(900.0),
            sample_coverage: Some(0.98),
            is_grouptotal: true,
            ..Default::default()
        });
        let outcomes = vec![
            LegOutcome {
                verdict: Verdict::Pass,
                reason_code: String::new(),
                reason_detail: String::new(),
                rx_avg: Some(950.0),
                main_rows: vec![ab_row],
                tag: "ab".into(),
            },
            LegOutcome {
                verdict: Verdict::RateFail,
                reason_code: "RX_BELOW_TARGET".into(),
                reason_detail: "ba low".into(),
                rx_avg: Some(780.0),
                main_rows: vec![ba_row],
                tag: "ba".into(),
            },
        ];
        {
            let mut rows = ctx.rows.lock().unwrap();
            populate_peer_rx(&mut rows, &outcomes);
            assert_eq!(rows[ab_row].peer_rx, "780.000 Mbps (BA)");
            assert_eq!(rows[ba_row].peer_rx, "950.000 Mbps (AB)");
        }
        let directions = ctx.direction_summaries(&outcomes);
        assert_eq!(directions.len(), 2);
        assert_eq!(directions[0].tag, "AB");
        assert_eq!(directions[0].rx_avg, Some(950.0));
        assert_eq!(directions[1].tag, "BA");
        assert_eq!(directions[1].rx_p10, Some(760.0));
        let total = aggregate_direction_streams(&directions).unwrap();
        assert_eq!(
            (total.requested, total.active, total.required),
            (5, 5, 4),
            "双向单元的流数必须来自实际方向，而不是 Default::default() 的 0/0/0"
        );

        let ping_row = ctx.push_row(Row {
            task_id: "ping-flow".into(),
            parent_id: "ping-unit".into(),
            task: "PING V4".into(),
            kind_label: "PING".into(),
            verdict: Verdict::Pass,
            ping_loss: Some(0.0),
            ping_min: Some(1.25),
            ping_avg: Some(2.5),
            ping_max: Some(3.75),
            ..Default::default()
        });
        let ping_directions = ctx.direction_summaries(&[LegOutcome {
            verdict: Verdict::Pass,
            reason_code: String::new(),
            reason_detail: String::new(),
            rx_avg: None,
            main_rows: vec![ping_row],
            tag: String::new(),
        }]);
        assert_eq!(ping_directions.len(), 1);
        assert_eq!(ping_directions[0].streams, None);
        assert_eq!(ping_directions[0].ping_min, Some(1.25));
        assert_eq!(ping_directions[0].ping_avg, Some(2.5));
        assert_eq!(ping_directions[0].ping_max, Some(3.75));
        assert_eq!(aggregate_direction_streams(&ping_directions), None);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn test_result_db() {
        let dir = std::env::temp_dir().join("cpe_db_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("task_results.json");
        let _ = std::fs::remove_file(&p);
        let mut db = ResultDb::load(p.clone());
        db.set("abc", true, "t1");
        db.save();
        let db2 = ResultDb::load(p.clone());
        assert!(db2.fresh_pass("abc").is_some());
        assert!(db2.fresh_pass("nope").is_none());
        let mut db3 = ResultDb::load(p.clone());
        db3.set("abc", false, "t1");
        db3.save();
        let db4 = ResultDb::load(p.clone());
        assert!(db4.fresh_pass("abc").is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn resume_freshness_uses_exact_24_hour_boundary() {
        assert!(resume_age_is_fresh(
            chrono::Duration::hours(23) + chrono::Duration::minutes(59)
        ));
        assert!(!resume_age_is_fresh(chrono::Duration::hours(24)));
        assert!(!resume_age_is_fresh(
            chrono::Duration::hours(24) + chrono::Duration::minutes(1)
        ));
        assert!(resume_age_is_fresh(chrono::Duration::seconds(-60)));
        assert!(!resume_age_is_fresh(chrono::Duration::seconds(-61)));
    }

    #[test]
    fn ctstraffic_tcp_requests_map_src_to_client_and_dst_to_server() {
        let (ctx, db_path) = isolated_ctx(0);
        let task = ctstraffic_task(false);
        let (server, client) = ctx.build_cts_requests(&task).unwrap();

        assert_eq!(server.role, CtsTrafficRole::Server);
        assert_eq!(server.protocol, CtsTrafficProtocol::Tcp);
        assert_eq!(server.bind_ip, task.dst.nic.ipv4);
        assert!(server.target_ip.is_empty());
        assert_eq!(client.role, CtsTrafficRole::Client);
        assert_eq!(client.protocol, CtsTrafficProtocol::Tcp);
        assert_eq!(client.bind_ip, task.src.nic.ipv4);
        assert_eq!(client.target_ip, task.dst.nic.ipv4);
        assert_eq!(client.streams, 3);
        assert_eq!(client.window_bytes, Some(64 * 1024));
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn ctstraffic_udp_requests_reverse_process_roles_but_keep_src_to_dst_data_flow() {
        let (ctx, db_path) = isolated_ctx(0);
        let task = ctstraffic_task(true);
        let (server, client) = ctx.build_cts_requests(&task).unwrap();

        assert_eq!(server.role, CtsTrafficRole::Server);
        assert_eq!(server.protocol, CtsTrafficProtocol::Udp);
        assert_eq!(server.bind_ip, task.src.nic.ipv4, "UDP server 是实际发送端");
        assert!(server.target_ip.is_empty());
        assert_eq!(client.role, CtsTrafficRole::Client);
        assert_eq!(client.protocol, CtsTrafficProtocol::Udp);
        assert_eq!(client.bind_ip, task.dst.nic.ipv4, "UDP client 是实际接收端");
        assert_eq!(client.target_ip, task.src.nic.ipv4);
        assert_eq!(client.bits_per_second, Some(500_000_000));
        assert_eq!(client.datagram_bytes, Some(1200));
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn cts_monitor_and_client_start_delays_share_one_leg_epoch() {
        let monitor_offset_ms = midpoint_ms(200, 800);
        assert_eq!(monitor_offset_ms, 500);
        let client_call_offset_ms = 900;
        let client_origin_ms = remote_job_origin_ms(900, 300);
        assert_eq!(client_origin_ms, 300);
        let client_job_offset_ms = client_call_offset_ms + client_origin_ms;
        let actual_traffic_start_ms = 2_500;
        let actual_traffic_end_ms = 12_500;
        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: client_job_offset_ms,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: client_job_offset_ms + 1_300,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: client_job_offset_ms + 2_300,
                mbps: Some(100.0),
                line: "status".into(),
            },
            IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: client_job_offset_ms + 12_300,
                ..Default::default()
            },
        ];
        let window = cts_effective_window(&events, 10, 1_000);
        assert_eq!(window.start_ms, 2_500);
        assert_eq!(window.end_ms, 12_500);
        assert_eq!(window.available_secs, 11.0);
        assert!(window.complete);

        let mut monitor = MonitorStopOut {
            samples: (1..=14)
                .map(|second| {
                    let remote_end_ms = second * 1_000;
                    let leg_end_ms = remote_end_ms + monitor_offset_ms;
                    let leg_start_ms = leg_end_ms - 1_000;
                    MonitorSample {
                        elapsed_ms: remote_end_ms,
                        interval_ms: 1_000,
                        rx_mbps: if leg_start_ms >= actual_traffic_start_ms
                            && leg_end_ms <= actual_traffic_end_ms
                        {
                            100.0
                        } else {
                            0.0
                        },
                        valid: true,
                        ..Default::default()
                    }
                })
                .collect(),
            ..Default::default()
        };
        align_monitor_samples(&mut monitor, monitor_offset_ms);
        let stats = monitor_rate_stats(&monitor, &window, true, window.start_ms);
        assert_eq!(stats.avg_mbps, Some(100.0));
        assert_eq!(stats.coverage, 1.0);
    }

    #[test]
    fn tcp_remote_job_origin_uses_rpc_midpoint_not_the_latest_bound() {
        let response_elapsed_ms = 900;
        let remote_job_age_ms = 300;
        let latest_possible_origin_ms = response_elapsed_ms - remote_job_age_ms;

        assert_eq!(latest_possible_origin_ms, 600);
        assert_eq!(
            remote_job_origin_ms(response_elapsed_ms, remote_job_age_ms),
            300
        );
    }

    #[test]
    fn remote_monitor_origin_uses_agent_elapsed_not_rpc_midpoint() {
        // 回归：远端 monitor 零点必须由 start 响应里的 elapsed_ms 与
        // 成功调用自身耗时做有界估计；若退化为“请求前后中点”，
        // 非对称网络延迟会把空闲时间混入正式窗口，覆盖率仍可能 100%。
        // 模拟：RPC 总耗时 900ms（含 retry 等待），远端 monitor 已运行 300ms，
        // 与 iperf client start 走完全相同的 remote_job_origin_ms 路径。
        let attempt_elapsed_ms = 900;
        let monitor_elapsed_ms = 300;
        let origin = remote_job_origin_ms(attempt_elapsed_ms, monitor_elapsed_ms);
        assert_eq!(origin, 300);
        // 零点必须落进 [0, 成功调用耗时] 的可证明区间，不能是调用前中点。
        assert!(origin <= attempt_elapsed_ms);

        // 与旧实现对比：旧实现用调用前后中点（例如 before=200, after=1100
        // → midpoint 650），把 350ms 空闲时间混入窗口。
        let legacy_rpc_midpoint = midpoint_ms(200, 1_100);
        assert_eq!(legacy_rpc_midpoint, 650);
        assert!(origin < legacy_rpc_midpoint, "零点估计必须优于 RPC 中点");

        // 本地 monitor 无网络往返：起点就是调用起点（偏移≈0）。
        let local_origin = midpoint_ms(0, 2);
        assert_eq!(local_origin, 1);
        assert!(local_origin <= 2);
    }

    #[test]
    fn cts_effective_window_does_not_guess_a_buffered_output_window() {
        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 1_000,
                ..Default::default()
            },
            // 模拟 stdout 在进程结束前才刷出 Connection/Status 行。
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 12_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 12_100,
                mbps: Some(100.0),
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: 12_500,
                ..Default::default()
            },
        ];
        let window = cts_effective_window(&events, 10, 1_000);
        assert_eq!((window.start_ms, window.end_ms), (12_100, 12_500));
        assert_eq!(window.available_secs, 0.4);
        assert!(!window.complete);
    }

    #[test]
    fn cts_effective_window_does_not_treat_a_long_handshake_as_buffered_output() {
        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 1_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 7_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 8_000,
                mbps: Some(100.0),
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: 13_000,
                ..Default::default()
            },
        ];

        // client 正常结束且有工具测量，也只能证明进程完整运行；Connection/Traffic
        // 并未集中在退出前，不能用 Ended-duration 把前面的握手空窗扩成数据窗口。
        let window = cts_effective_window(&events, 10, 1_000);
        assert_eq!((window.start_ms, window.end_ms), (8_000, 13_000));
        assert_eq!(window.available_secs, 5.0);
        assert!(!window.complete);
    }

    #[test]
    fn cts_effective_window_prefers_status_period_after_connection_handshake() {
        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 1_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 1_500,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 3_500,
                mbps: Some(100.0),
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: 12_500,
                ..Default::default()
            },
        ];
        let window = cts_effective_window(&events, 10, 1_000);
        assert_eq!((window.start_ms, window.end_ms), (2_500, 12_500));
        assert!(window.complete);
    }

    #[test]
    fn cts_total_time_is_not_used_as_data_window_evidence() {
        let client_output = "Total Time : 10000 ms.";
        let server_output = "Total Time : 61273 ms.";
        let client_duration =
            ctstraffic::parse_output(client_output, CtsTrafficProtocol::Udp).total_time_ms;
        let combined = ctstraffic::parse_output(
            &format!("{client_output}\n{server_output}"),
            CtsTrafficProtocol::Udp,
        );
        assert_eq!(client_duration, Some(10_000));
        assert_eq!(combined.total_time_ms, Some(61_273));

        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 1_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 12_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 12_100,
                mbps: Some(100.0),
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: 12_500,
                ..Default::default()
            },
        ];
        // client 的 Total Time 与合并摘要中的 server 生命周期都不是纯数据时长，
        // 不能用来补齐事件证据只有 0.4 秒的窗口。
        let server_window = cts_effective_window(&events, 10, 1_000);
        assert_eq!(
            (server_window.start_ms, server_window.end_ms),
            (12_100, 12_500)
        );
        assert!(!server_window.complete);
    }

    #[test]
    fn cts_retry_traffic_is_never_used_as_monitor_baseline() {
        let mut attempts = vec![
            ctstraffic_attempt(0, false),
            ctstraffic_attempt(1, false),
            ctstraffic_attempt(2, true),
        ];
        attempts[0].events = vec![IperfFlowEvent {
            kind: IperfEventKind::Started,
            elapsed_ms: 1_000,
            ..Default::default()
        }];
        attempts[0].traffic_window = EffectiveWindow {
            start_ms: 11_000,
            end_ms: 12_000,
            available_secs: 1.0,
            required_secs: 10,
            complete: false,
        };
        attempts[1].events = vec![IperfFlowEvent {
            kind: IperfEventKind::Started,
            elapsed_ms: 13_000,
            ..Default::default()
        }];
        attempts[2].events = vec![IperfFlowEvent {
            kind: IperfEventKind::Started,
            elapsed_ms: 22_000,
            ..Default::default()
        }];
        attempts[2].traffic_window = EffectiveWindow {
            start_ms: 23_000,
            end_ms: 33_000,
            available_secs: 10.0,
            required_secs: 10,
            complete: true,
        };

        let selected_idx = select_cts_attempt_index(&attempts).unwrap();
        let selected = &attempts[selected_idx];
        assert_eq!(selected_idx, 2);
        let cutoff_ms = cts_baseline_cutoff_ms(&attempts);
        assert_eq!(cutoff_ms, 1_000);

        let monitor = MonitorStopOut {
            samples: (1..=33)
                .map(|second| MonitorSample {
                    elapsed_ms: second * 1_000,
                    interval_ms: 1_000,
                    rx_mbps: if (2..=11).contains(&second) || (24..=33).contains(&second) {
                        100.0
                    } else {
                        0.0
                    },
                    valid: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };

        let stats = monitor_rate_stats(&monitor, &selected.traffic_window, true, cutoff_ms);
        assert_eq!(stats.avg_mbps, Some(100.0));
        assert_eq!(stats.coverage, 1.0);

        let wrong_stats = monitor_rate_stats(
            &monitor,
            &selected.traffic_window,
            true,
            attempts[0].traffic_window.start_ms,
        );
        assert_eq!(
            wrong_stats.avg_mbps,
            Some(0.0),
            "若把首轮流量窗口末端之前的样本当 baseline，后续结果会被固定扣低"
        );
    }

    #[test]
    fn cts_baseline_without_started_evidence_is_fail_safe() {
        let mut attempt = ctstraffic_attempt(0, true);
        attempt.events = vec![IperfFlowEvent {
            kind: IperfEventKind::Connected,
            elapsed_ms: 5_000,
            ..Default::default()
        }];
        attempt.traffic_window.start_ms = 6_000;

        assert_eq!(
            cts_baseline_cutoff_ms(std::slice::from_ref(&attempt)),
            0,
            "缺失 Started 时不能把反推流量窗口之前的样本误当 idle baseline"
        );
    }

    #[test]
    fn artifact_tcp_rx_baseline_uses_client_start_not_inferred_window() {
        // 复现 run_20260811_152635_20728 首个 TCP 的关键时间线：client 在
        // 551ms 启动，最终 receiver 区间从 184678ms 反推正式窗口从 2898ms
        // 开始。2898ms 前两个样本已经包含真实流量，绝不能作为背景基线。
        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 551,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 1_874,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 184_678,
                mbps: Some(935.0),
                line: "[SUM] 0.00-181.78 sec 19.8 GBytes 935 Mbits/sec receiver".into(),
            },
            IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: 184_707,
                ..Default::default()
            },
        ];
        let window = iperf_effective_window(&events, 180, true);
        assert_eq!((window.start_ms, window.end_ms), (2_898, 182_898));
        assert_eq!(iperf_baseline_cutoff_ms(&events), 551);

        let mut samples = vec![
            MonitorSample {
                elapsed_ms: 1_014,
                interval_ms: 1_011,
                rx_mbps: 131.208_970,
                valid: true,
                ..Default::default()
            },
            MonitorSample {
                elapsed_ms: 2_025,
                interval_ms: 1_011,
                rx_mbps: 956.586_137,
                valid: true,
                ..Default::default()
            },
        ];
        for index in 3_u64..=184 {
            samples.push(MonitorSample {
                elapsed_ms: 2_025 + (index - 2) * 1_010,
                interval_ms: 1_010,
                // 代表原样本中约 952-957Mbps 的持续 RX；连续低段也确保
                // 错误扣基线时 RX-P10 会退化为 0。
                rx_mbps: if index % 20 < 7 { 952.0 } else { 956.875 },
                valid: true,
                ..Default::default()
            });
        }
        let monitor = MonitorStopOut {
            samples,
            ..Default::default()
        };

        let fixed = monitor_rate_stats(&monitor, &window, true, iperf_baseline_cutoff_ms(&events));
        assert!(fixed.avg_mbps.is_some_and(|value| value > 950.0));
        assert!(fixed.p10_mbps.is_some_and(|value| value > 950.0));
        assert_eq!(fixed.coverage, 1.0);

        let contaminated = monitor_rate_stats(&monitor, &window, true, window.start_ms);
        assert!(contaminated.avg_mbps.is_some_and(|value| value < 1.0));
        assert_eq!(contaminated.p10_mbps, Some(0.0));

        let retry_events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 551,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Retry,
                elapsed_ms: 4_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 5_000,
                ..Default::default()
            },
        ];
        assert_eq!(
            iperf_baseline_cutoff_ms(&retry_events),
            551,
            "重试不能把可能已含首轮流量的样本重新定义为背景"
        );
    }

    // ---------------- P1：run_udp_unit 编排层验收（U00C / U00D / W09） ----------------

    /// 单条流在假 agent 上的剧本：每一轮 client attempt 是否产生工具测量。
    #[derive(Clone)]
    struct FlowScript {
        /// 第 N 轮（0 起）是否产出 iperf3 自身的 rate/bytes 测量。
        measured_at_attempt: Option<usize>,
        /// server stop 是否确认成功；false 用于 W09「清理未确认禁止复用端口」。
        server_stop_confirmed: bool,
        /// client 进程是否正常结束；false 模拟"有测量但运行时出错"。
        client_ok: bool,
    }

    impl FlowScript {
        fn never() -> Self {
            Self {
                measured_at_attempt: None,
                server_stop_confirmed: true,
                client_ok: true,
            }
        }
        fn at(attempt: usize) -> Self {
            Self {
                measured_at_attempt: Some(attempt),
                server_stop_confirmed: true,
                client_ok: true,
            }
        }
        fn stop_unconfirmed() -> Self {
            Self {
                measured_at_attempt: None,
                server_stop_confirmed: false,
                client_ok: true,
            }
        }
        /// 已有工具测量，但 client 非正常结束：U00G 要求按真实 runtime error 判定，
        /// 不能再为了争取更好结果继续重试、更不能改写成"未灌通"。
        fn measured_but_runtime_failed(attempt: usize) -> Self {
            Self {
                measured_at_attempt: Some(attempt),
                server_stop_confirmed: true,
                client_ok: false,
            }
        }
    }

    /// 覆盖 server / client / monitor 全部路由的假 agent，用于驱动 `run_udp_unit`
    /// 这一层的真实状态机（交错起流、attempt 循环、清理门禁、并行两腿）。
    ///
    /// 剧本按端口索引，因此可以让 AB、BA 两个方向各自独立地成功或失败。
    struct FakeUdpAgent {
        scripts: HashMap<u16, FlowScript>,
        /// 每个端口已经启动过的 client attempt 次数。
        client_attempts: Mutex<HashMap<u16, usize>>,
        /// 按到达顺序记录 (路径, 端口, request_id)，用于断言"没有在未确认清理后复用端口"。
        calls: Mutex<Vec<(String, u16, String)>>,
    }

    impl FakeUdpAgent {
        fn new(scripts: HashMap<u16, FlowScript>) -> Self {
            Self {
                scripts,
                client_attempts: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn script(&self, port: u16) -> FlowScript {
            self.scripts
                .get(&port)
                .cloned()
                .unwrap_or_else(FlowScript::never)
        }

        fn record(&self, path: &str, port: u16, request_id: &str) {
            lock_recover(&self.calls).push((path.to_string(), port, request_id.to_string()));
        }

        fn calls_for(&self, path: &str) -> Vec<(u16, String)> {
            lock_recover(&self.calls)
                .iter()
                .filter(|(p, _, _)| p == path)
                .map(|(_, port, id)| (*port, id.clone()))
                .collect()
        }

        /// 端口是 client 请求里的目的端口，client_start 用它索引剧本。
        fn handle(
            &self,
            request: &http_client::HttpRequest,
        ) -> Result<http_client::HttpResponse, String> {
            let respond = |body: String| http_client::HttpResponse::new(200, body);
            match request.path.as_str() {
                "/iperf/server/start" => {
                    let req: IperfServerStartReq = serde_json::from_str(&request.body)
                        .map_err(|e| format!("server start 解析失败: {e}"))?;
                    self.record("server/start", req.port, &req.request_id);
                    Ok(respond(ok_json(IperfServerStartOut {
                        cmd: format!("fake iperf3 -s -p {}", req.port),
                    })))
                }
                "/iperf/server/stop" => {
                    let req: IperfServerStopReq = serde_json::from_str(&request.body)
                        .map_err(|e| format!("server stop 解析失败: {e}"))?;
                    self.record("server/stop", req.port, &req.request_id);
                    if !self.script(req.port).server_stop_confirmed {
                        return Ok(respond(err_json("server 停止未确认：进程未回收")));
                    }
                    Ok(respond(ok_json(IperfServerStopOut {
                        existed: true,
                        terminated: true,
                        output: format!("fake server output port {}", req.port),
                    })))
                }
                "/iperf/client/start" => {
                    let start: IperfClientStartReq = serde_json::from_str(&request.body)
                        .map_err(|e| format!("client start 解析失败: {e}"))?;
                    let port = start.request.port;
                    self.record("client/start", port, &start.request_id);
                    *lock_recover(&self.client_attempts).entry(port).or_insert(0) += 1;
                    Ok(respond(ok_json(IperfClientStartOut {
                        id: start.request_id.clone(),
                        elapsed_ms: 5,
                    })))
                }
                "/iperf/client/status" => {
                    let req: IperfClientStatusReq = serde_json::from_str(&request.body)
                        .map_err(|e| format!("client status 解析失败: {e}"))?;
                    // request_id 形如 "<owner>:client:<port>:<attempt>"
                    let (port, attempt) = parse_client_request_id(&req.id);
                    let script = self.script(port);
                    let measured = script.measured_at_attempt == Some(attempt);
                    let events = if measured {
                        vec![
                            IperfFlowEvent {
                                kind: IperfEventKind::Started,
                                elapsed_ms: 0,
                                ..Default::default()
                            },
                            IperfFlowEvent {
                                kind: IperfEventKind::Traffic,
                                elapsed_ms: 10_000,
                                mbps: Some(500.0),
                                line: "[  5]   0.00-10.00 sec  600 MBytes  500 Mbits/sec sender"
                                    .into(),
                            },
                            IperfFlowEvent {
                                kind: IperfEventKind::Ended,
                                elapsed_ms: 10_050,
                                ..Default::default()
                            },
                        ]
                    } else {
                        vec![
                            IperfFlowEvent {
                                kind: IperfEventKind::Started,
                                elapsed_ms: 0,
                                ..Default::default()
                            },
                            IperfFlowEvent {
                                kind: IperfEventKind::Ended,
                                elapsed_ms: 1_000,
                                ..Default::default()
                            },
                        ]
                    };
                    let output = if measured {
                        "[  5]   0.00-10.00 sec  600 MBytes  500 Mbits/sec sender".to_string()
                    } else {
                        "iperf3: no measurement in this attempt".to_string()
                    };
                    Ok(respond(ok_json(IperfClientStatusOut {
                        id: req.id,
                        done: true,
                        next_cursor: 0,
                        events,
                        result: Some(IperfClientOut {
                            ok: script.client_ok,
                            process_started: Some(true),
                            cleanup_confirmed: Some(true),
                            cmd: format!("fake iperf3 client port {port}"),
                            output,
                            ..Default::default()
                        }),
                    })))
                }
                "/iperf/client/stop" => Ok(respond(ok_json(IperfClientStopOut {
                    existed: true,
                    was_done: true,
                    terminated: true,
                    result: None,
                }))),
                "/monitor/start" => {
                    let req: MonitorStartReq = serde_json::from_str(&request.body)
                        .map_err(|e| format!("monitor start 解析失败: {e}"))?;
                    Ok(respond(ok_json(MonitorStartOut {
                        id: format!("mon-{}", req.iface),
                        elapsed_ms: 1,
                    })))
                }
                "/monitor/status" => Ok(respond(ok_json(MonitorStatusOut {
                    id: "mon".into(),
                    iface: "fake".into(),
                    sample_count: 1,
                    latest_sample: Some(fake_sample(1_000, 500.0)),
                    error_count: 0,
                    latest_error: String::new(),
                }))),
                "/monitor/stop" => Ok(respond(ok_json(MonitorStopOut {
                    avg_mbps: 500.0,
                    tx_avg_mbps: 520.0,
                    seconds: 40.0,
                    bytes: 0,
                    tx_bytes: 0,
                    samples: (1..=40).map(|s| fake_sample(s * 1_000, 500.0)).collect(),
                    errors: vec![],
                }))),
                other => Err(format!("fake udp agent 未知路径 {other}")),
            }
        }
    }

    fn fake_sample(elapsed_ms: u64, mbps: f64) -> MonitorSample {
        MonitorSample {
            elapsed_ms,
            interval_ms: 1_000,
            rx_mbps: mbps,
            tx_mbps: mbps * 1.05,
            valid: true,
            ..Default::default()
        }
    }

    /// `lifecycle_request_id` 的逆运算：`<owner>:client:<port>:<attempt>`。
    fn parse_client_request_id(id: &str) -> (u16, usize) {
        let mut parts = id.rsplit(':');
        let attempt = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let port = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        (port, attempt)
    }

    /// 构造一个两端都在 agent 侧的双向 UDP 单元，让整条链路都走假 transport。
    fn bidir_udp_unit(ab_port: u16, ba_port: u16, streams: usize) -> (Unit, Vec<UdpLegPlan>) {
        let a = endpoint(Side::Agent, "eth0", "192.168.1.2");
        let b = endpoint(Side::Agent, "eth1", "192.168.1.3");
        let mk = |lidx: usize, tag: &str, src: &Endpoint, dst: &Endpoint, base: u16| UdpLegPlan {
            lidx,
            tag: tag.into(),
            name: "udp_b500m".into(),
            streams: (0..streams)
                .map(|stream_idx| IperfTask {
                    v6: false,
                    udp: true,
                    profile_name: "udp_b500m".into(),
                    profile_label: "UDP -b 500m".into(),
                    src: src.clone(),
                    dst: dst.clone(),
                    port: base + stream_idx as u16,
                    duration: 10,
                    extra: vec!["-b".into(), "500m".into()],
                    stream_idx,
                    rate_mode: RateMode::Observe,
                    rx_target_mbps: None,
                    offered_mbps: Some(500.0),
                })
                .collect(),
        };
        let plans = vec![mk(0, "ab", &a, &b, ab_port), mk(1, "ba", &b, &a, ba_port)];
        let unit = Unit {
            id: format!("udp-orch-{ab_port}-{ba_port}"),
            title: "★双向 IPERF V4 UDP -b 500m".into(),
            bidir: true,
            legs: vec![],
            est_secs: 60,
        };
        (unit, plans)
    }

    /// 假 agent 直接作为 transport：这些用例的故障由 `FlowScript` 注入，
    /// 不需要 `ScriptedTransport` 的丢包/截断脚本（那套要求逐条预排队列）。
    impl http_client::Transport for FakeUdpAgent {
        fn send(
            &self,
            request: &http_client::HttpRequest,
            _timeout: Duration,
        ) -> Result<http_client::HttpResponse, String> {
            self.handle(request)
        }
    }

    fn run_udp_orchestration(
        scripts: HashMap<u16, FlowScript>,
        ab_port: u16,
        ba_port: u16,
        streams: usize,
    ) -> (Vec<LegOutcome>, Arc<FakeUdpAgent>, Vec<Row>) {
        let agent = Arc::new(FakeUdpAgent::new(scripts));
        let (mut ctx, db_path) = isolated_ctx(1);
        ctx.transport = Arc::clone(&agent) as Arc<dyn http_client::Transport>;
        // 基线采样会真实 sleep，测试里压到 0 秒。
        ctx.cfg.iperf.rate_check.background_secs = 0;
        ctx.cfg.iperf.rate_check.settle_secs = 0;
        ctx.cfg.iperf.rate_check.launch_interval_ms = 0;
        ctx.cfg.iperf.duration = 10;

        let (unit, plans) = bidir_udp_unit(ab_port, ba_port, streams);
        let outcomes = ctx.run_udp_unit(0, &unit, &plans, "owner-orch", 0);
        let rows = lock_recover(&ctx.rows).clone();
        let _ = std::fs::remove_file(db_path);
        (outcomes, agent, rows)
    }

    /// U00D：双向每方向 1 流，各自拥有独立的三轮预算并行执行。
    ///
    /// 这条同时锁住四个历史易碎点：独立预算（不能两腿合计三次）、并行执行、
    /// 单流硬失败不被另一腿的普通 NOT_EVALUATED 掩盖、每方向 retry 独立计数。
    #[test]
    fn udp_bidirectional_single_stream_legs_get_independent_three_attempt_budgets() {
        let scripts = HashMap::from([
            // AB：前两轮无测量，第三轮灌通。
            (57_000, FlowScript::at(2)),
            // BA：三轮都没有工具测量 → 单流硬失败。
            (57_100, FlowScript::never()),
        ]);
        let (outcomes, agent, rows) = run_udp_orchestration(scripts, 57_000, 57_100, 1);

        let ab = outcomes
            .iter()
            .find(|o| o.tag == "ab")
            .expect("AB 方向结果");
        let ba = outcomes
            .iter()
            .find(|o| o.tag == "ba")
            .expect("BA 方向结果");

        // AB 用成功轮判定，不是硬失败。
        assert_ne!(
            ab.reason_code, "SINGLE_UDP_STREAM_FAILED",
            "AB 第三轮已灌通"
        );
        // BA 是必须灌通却没灌通的硬失败。
        assert_eq!(ba.verdict, Verdict::RateFail, "BA 应为硬失败: {ba:?}");
        assert_eq!(ba.reason_code, "SINGLE_UDP_STREAM_FAILED");

        // 两方向各自跑满 3 次 client attempt —— 不是合计 3 次。
        let starts = agent.calls_for("client/start");
        let ab_attempts = starts.iter().filter(|(port, _)| *port == 57_000).count();
        let ba_attempts = starts.iter().filter(|(port, _)| *port == 57_100).count();
        assert_eq!(ab_attempts, 3, "AB 应有 3 次完整尝试，实际 {ab_attempts}");
        assert_eq!(ba_attempts, 3, "BA 应有 3 次完整尝试，实际 {ba_attempts}");

        // 每轮必须用新的 request ID，前两轮的原文不能被覆盖。
        let ab_ids: Vec<&String> = starts
            .iter()
            .filter(|(port, _)| *port == 57_000)
            .map(|(_, id)| id)
            .collect();
        let unique: std::collections::HashSet<&&String> = ab_ids.iter().collect();
        assert_eq!(unique.len(), 3, "三轮必须使用不同 request ID: {ab_ids:?}");

        // 单元汇总不能被 BA 之外的任何普通结果掩盖硬失败。
        assert_eq!(aggregate_unit_verdict(&outcomes), Verdict::RateFail);

        // 报告里 BA 的组合计行保留完整尝试数（retry_count = 尝试数 - 1）。
        let ba_total = rows
            .iter()
            .find(|r| r.is_grouptotal && r.kind_label.contains("ba"))
            .expect("BA 组合计行");
        assert_eq!(ba_total.retry_count, 2, "BA retry_count 应为 2");
    }

    /// U00C：单流三轮安全耗尽后是硬失败，不能降级成 ACTIVE_STREAMS_LOW，
    /// 也不能因为"0 流"笼统改写成 SETUP_ERROR。
    #[test]
    fn udp_single_stream_safe_exhaustion_is_rate_fail_not_active_streams_low() {
        let scripts = HashMap::from([(57_200, FlowScript::never()), (57_300, FlowScript::never())]);
        let (outcomes, agent, _) = run_udp_orchestration(scripts, 57_200, 57_300, 1);

        for outcome in &outcomes {
            assert_eq!(
                outcome.verdict,
                Verdict::RateFail,
                "{} 方向应为 RATE_FAIL: {outcome:?}",
                outcome.tag
            );
            assert_eq!(outcome.reason_code, "SINGLE_UDP_STREAM_FAILED");
            assert_ne!(outcome.reason_code, "ACTIVE_STREAMS_LOW");
            assert_ne!(outcome.reason_code, "NO_STREAM_STARTED");
        }
        // 两个方向各自安全跑满预算。
        assert_eq!(agent.calls_for("client/start").len(), 6);
    }

    /// W09：某轮 server stop 未确认时，禁止在同端口用新 request 继续重试，
    /// 必须以 SETUP_ERROR 报告资源清理问题，且不得计入"安全耗尽"。
    #[test]
    fn udp_flow_stops_retrying_when_server_cleanup_is_unconfirmed() {
        let scripts = HashMap::from([
            // AB 的 server stop 永远返回未确认。
            (57_400, FlowScript::stop_unconfirmed()),
            (57_500, FlowScript::at(0)),
        ]);
        let (outcomes, agent, _) = run_udp_orchestration(scripts, 57_400, 57_500, 1);

        let ab = outcomes.iter().find(|o| o.tag == "ab").expect("AB 结果");
        assert_eq!(
            ab.verdict,
            Verdict::SetupError,
            "清理未确认必须是 SETUP_ERROR，不能伪装成单流硬失败: {ab:?}"
        );
        assert_ne!(ab.reason_code, "SINGLE_UDP_STREAM_FAILED");

        // 关键断言：未确认之后不能再有第二次 client start 打到同一端口。
        let ab_starts = agent
            .calls_for("client/start")
            .into_iter()
            .filter(|(port, _)| *port == 57_400)
            .count();
        assert_eq!(
            ab_starts, 1,
            "清理未确认后禁止复用端口 57400 重试，实际启动 {ab_starts} 次"
        );

        // 另一方向不受影响，正常灌通。
        let ba = outcomes.iter().find(|o| o.tag == "ba").expect("BA 结果");
        assert_ne!(ba.reason_code, "SINGLE_UDP_STREAM_FAILED", "BA 首轮即灌通");
    }

    /// 多流方向：只重启没跑通的那条流，已经稳定的流不重启（U02 的核心不变量）。
    #[test]
    fn udp_group_retry_only_restarts_the_flow_that_failed() {
        let scripts = HashMap::from([
            // AB 两条流：#0 首轮即通，#1 从不通。
            (57_600, FlowScript::at(0)),
            (57_601, FlowScript::never()),
            (57_700, FlowScript::at(0)),
            (57_701, FlowScript::at(0)),
        ]);
        let (_, agent, _) = run_udp_orchestration(scripts, 57_600, 57_700, 2);

        let starts = agent.calls_for("client/start");
        let flow0 = starts.iter().filter(|(port, _)| *port == 57_600).count();
        assert_eq!(flow0, 1, "已跑通的流不能被重启，实际启动 {flow0} 次");
        // 未跑通的流按 flow_retries 预算重试（多流不套用单流三轮硬门槛）。
        let flow1 = starts.iter().filter(|(port, _)| *port == 57_601).count();
        assert!(flow1 >= 1, "失败流应至少执行一次");
        assert!(flow1 <= 3, "重试必须有限，不允许无限循环，实际 {flow1} 次");
    }

    /// U00G：已有工具测量后按真实结果判定，不再为争取更好结果继续重试，
    /// 也不得把真实的运行时错误改写成「未灌通」。
    #[test]
    fn udp_keeps_the_real_runtime_error_once_a_measurement_exists() {
        let scripts = HashMap::from([
            (57_800, FlowScript::measured_but_runtime_failed(0)),
            (57_900, FlowScript::at(0)),
        ]);
        let (outcomes, agent, _) = run_udp_orchestration(scripts, 57_800, 57_900, 1);

        let ab = outcomes.iter().find(|o| o.tag == "ab").expect("AB 结果");
        assert_eq!(ab.verdict, Verdict::RateFail, "{ab:?}");
        assert_eq!(
            ab.reason_code, "IPERF_RUNTIME_ERRORS",
            "已有测量时必须保留真实的 runtime error，不能改写成 SINGLE_UDP_STREAM_FAILED"
        );
        // 已有测量就不该再重试去"碰运气"。
        let ab_attempts = agent
            .calls_for("client/start")
            .into_iter()
            .filter(|(port, _)| *port == 57_800)
            .count();
        assert_eq!(
            ab_attempts, 1,
            "已有测量后不得继续重试，实际 {ab_attempts} 次"
        );
    }

    /// U00F：背景网卡流量不能把"没有工具测量"补成一条成功的流。
    ///
    /// 假 monitor 恒定返回 500 Mbps 的 RX（远高于最低有效速率），但工具三轮
    /// 都没有 rate/bytes 测量——active stream 必须仍然是 0。
    #[test]
    fn background_nic_traffic_never_counts_as_an_established_flow() {
        let scripts = HashMap::from([(58_000, FlowScript::never()), (58_100, FlowScript::never())]);
        let (outcomes, _, rows) = run_udp_orchestration(scripts, 58_000, 58_100, 1);

        for outcome in &outcomes {
            assert_eq!(
                outcome.reason_code, "SINGLE_UDP_STREAM_FAILED",
                "{} 方向应为单流硬失败: {outcome:?}",
                outcome.tag
            );
        }
        // 组合计行的活跃流数必须是 0——网卡上有 500Mbps 背景流量也不能补上。
        for total in rows.iter().filter(|r| r.is_grouptotal) {
            assert_eq!(
                total.active_streams, 0,
                "背景网卡流量把 active 补成了 {}",
                total.active_streams
            );
        }
    }

    /// U01：双向不对称流数（5 流 / 2 流）统一调度，两个方向都能正常起流并判定。
    #[test]
    fn udp_bidirectional_asymmetric_stream_counts_are_scheduled_together() {
        let mut scripts = HashMap::new();
        for i in 0..5u16 {
            scripts.insert(58_200 + i, FlowScript::at(0));
        }
        for i in 0..5u16 {
            scripts.insert(58_300 + i, FlowScript::at(0));
        }
        let (outcomes, agent, rows) = run_udp_orchestration(scripts, 58_200, 58_300, 5);

        assert_eq!(outcomes.len(), 2, "两个方向各自一个结果");
        for outcome in &outcomes {
            assert_ne!(
                outcome.verdict,
                Verdict::RateFail,
                "{} 方向全部灌通不应失败: {outcome:?}",
                outcome.tag
            );
        }
        // 10 条流各起一次，一次不多一次不少。
        assert_eq!(agent.calls_for("client/start").len(), 10);
        for total in rows.iter().filter(|r| r.is_grouptotal) {
            assert_eq!(total.requested_streams, 5);
            assert_eq!(total.active_streams, 5, "5 条流应全部活跃");
            // 5 条流按默认 90% 容错要求 4 条。
            assert_eq!(total.required_streams, 4);
        }
    }

    /// U00E：server 起不来属于确定性环境错误，必须是 SETUP_ERROR，
    /// 不能伪装成单流硬失败去指责被测设备。
    #[test]
    fn udp_server_start_failure_stays_a_setup_error() {
        let agent = Arc::new(FakeUdpAgent::new(HashMap::new()));
        // 让 server/start 始终失败：剧本之外的端口一律 never，但这里直接
        // 用一个不存在的路由制造启动失败。
        let (mut ctx, db_path) = isolated_ctx(1);
        struct RefusingAgent;
        impl http_client::Transport for RefusingAgent {
            fn send(
                &self,
                request: &http_client::HttpRequest,
                _timeout: Duration,
            ) -> Result<http_client::HttpResponse, String> {
                if request.path == "/iperf/server/start" {
                    return Ok(http_client::HttpResponse::new(
                        200,
                        err_json("辅测机端口被占用，server 无法启动"),
                    ));
                }
                Ok(http_client::HttpResponse::new(
                    200,
                    ok_json(serde_json::json!({})),
                ))
            }
        }
        ctx.transport = Arc::new(RefusingAgent);
        ctx.cfg.iperf.rate_check.background_secs = 0;
        ctx.cfg.iperf.rate_check.settle_secs = 0;
        ctx.cfg.iperf.rate_check.launch_interval_ms = 0;
        ctx.cfg.iperf.duration = 10;
        let (unit, plans) = bidir_udp_unit(58_400, 58_500, 1);
        let outcomes = ctx.run_udp_unit(0, &unit, &plans, "owner-setup", 0);
        let _ = std::fs::remove_file(db_path);
        drop(agent);

        for outcome in &outcomes {
            assert_eq!(
                outcome.verdict,
                Verdict::SetupError,
                "{} 方向 server 起不来必须是 SETUP_ERROR: {outcome:?}",
                outcome.tag
            );
            assert_ne!(outcome.reason_code, "SINGLE_UDP_STREAM_FAILED");
        }
    }

    #[test]
    fn cts_udp_loss_is_evaluated_after_nic_sampling_and_target_gates() {
        let pass = || (Verdict::Pass, String::new(), String::new());
        let not_evaluated = || {
            (
                Verdict::NotEvaluated,
                "RATE_WINDOW_COVERAGE_LOW".to_string(),
                "采样不足".to_string(),
            )
        };
        let measured = || {
            (
                Verdict::Measured,
                "TARGET_UNKNOWN".to_string(),
                "observe".to_string(),
            )
        };

        // 采样不足时不能被改写成「丢帧超限」：环境问题不背 CPE 的锅。
        let (verdict, code, _) = cts_apply_udp_loss(not_evaluated(), true, Some(1.0), Some(9.0));
        assert_eq!(
            (verdict, code.as_str()),
            (Verdict::NotEvaluated, "RATE_WINDOW_COVERAGE_LOW")
        );
        // 目标未知时同样保持 MEASURED，不产出丢帧失败。
        let (verdict, code, _) = cts_apply_udp_loss(measured(), true, Some(1.0), Some(9.0));
        assert_eq!(
            (verdict, code.as_str()),
            (Verdict::Measured, "TARGET_UNKNOWN")
        );
        // 已配置门槛但缺数据：缺的是判定依据本身，优先于速率结论。
        let (verdict, code, _) = cts_apply_udp_loss(pass(), true, Some(1.0), None);
        assert_eq!(
            (verdict, code.as_str()),
            (Verdict::NotEvaluated, "CTSTRAFFIC_UDP_LOSS_DATA_MISSING")
        );
        // 速率达标但丢帧超限：真实的 RATE_FAIL。
        let (verdict, code, _) = cts_apply_udp_loss(pass(), true, Some(1.0), Some(9.0));
        assert_eq!(
            (verdict, code.as_str()),
            (Verdict::RateFail, "CTSTRAFFIC_UDP_LOSS_HIGH")
        );
        // 门槛内、TCP、未配置门槛都保持原判定。
        assert_eq!(
            cts_apply_udp_loss(pass(), true, Some(10.0), Some(9.0)).0,
            Verdict::Pass
        );
        assert_eq!(
            cts_apply_udp_loss(pass(), false, Some(1.0), Some(9.0)).0,
            Verdict::Pass
        );
        assert_eq!(
            cts_apply_udp_loss(pass(), true, None, Some(9.0)).0,
            Verdict::Pass
        );
    }

    #[test]
    fn cts_effective_window_tolerates_millisecond_rounding_only() {
        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 1_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 2_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 3_000,
                mbps: Some(100.0),
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: 11_999,
                ..Default::default()
            },
        ];
        let rounded = cts_effective_window(&events, 10, 1_000);
        assert_eq!((rounded.start_ms, rounded.end_ms), (2_000, 11_999));
        assert_eq!(rounded.available_secs, 9.999);
        assert!(rounded.complete);

        let clearly_short = cts_effective_window(
            &[
                events[0].clone(),
                events[1].clone(),
                events[2].clone(),
                IperfFlowEvent {
                    kind: IperfEventKind::Ended,
                    elapsed_ms: 11_500,
                    ..Default::default()
                },
            ],
            10,
            1_000,
        );
        assert!(!clearly_short.complete);
    }

    #[test]
    fn cts_effective_window_does_not_expand_an_early_exit() {
        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 1_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 1_500,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 2_500,
                mbps: Some(100.0),
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: 8_000,
                ..Default::default()
            },
        ];
        let window = cts_effective_window(&events, 10, 1_000);
        assert_eq!((window.start_ms, window.end_ms), (2_500, 8_000));
        assert_eq!(window.available_secs, 5.5);
        assert!(!window.complete);
    }

    #[test]
    fn cts_monitor_failures_keep_specific_result_semantics() {
        let window = EffectiveWindow {
            start_ms: 0,
            end_ms: 2_000,
            available_secs: 2.0,
            required_secs: 2,
            complete: true,
        };
        let no_samples = MonitorStopOut {
            avg_mbps: 2_800.0,
            seconds: 12.0,
            ..Default::default()
        };
        let issue = cts_monitor_runtime_issue(&no_samples, &window).expect("missing samples issue");
        assert_eq!(issue.code, "CTSTRAFFIC_MONITOR_NO_SAMPLES");
        assert!(issue.detail.contains("全生命周期平均值不能用于"));
        assert_eq!(
            cts_monitor_issue_verdict(&issue).unwrap().0,
            Verdict::NotEvaluated
        );

        let runtime = MonitorStopOut {
            samples: vec![MonitorSample {
                elapsed_ms: 1_000,
                interval_ms: 1_000,
                valid: false,
                error: "counter reset".into(),
                ..Default::default()
            }],
            errors: vec!["counter reset".into()],
            ..Default::default()
        };
        let issue = cts_monitor_runtime_issue(&runtime, &window).expect("runtime issue");
        assert_eq!(issue.code, "CTSTRAFFIC_MONITOR_RUNTIME_ERROR");
        assert!(issue.detail.contains("counter reset"));
        assert_eq!(
            cts_monitor_issue_verdict(&issue).unwrap().0,
            Verdict::NotEvaluated
        );

        let startup = CtsMonitorIssue {
            code: "CTSTRAFFIC_MONITOR_START_FAILED".into(),
            detail: "interface not found".into(),
            setup_error: true,
            affects_verdict: true,
        };
        let (verdict, code, detail) = cts_monitor_issue_verdict(&startup).unwrap();
        assert_eq!(verdict, Verdict::SetupError);
        assert_eq!(code, "CTSTRAFFIC_MONITOR_START_FAILED");
        assert_eq!(detail, "interface not found");
    }

    #[test]
    fn cts_monitor_error_outside_effective_window_is_diagnostic_only() {
        let window = EffectiveWindow {
            start_ms: 2_000,
            end_ms: 12_000,
            available_secs: 10.0,
            required_secs: 10,
            complete: true,
        };
        let mut samples = vec![MonitorSample {
            elapsed_ms: 1_000,
            interval_ms: 1_000,
            valid: false,
            error: "startup read failed".into(),
            ..Default::default()
        }];
        samples.extend((3..=12).map(|second| MonitorSample {
            elapsed_ms: second * 1_000,
            interval_ms: 1_000,
            rx_mbps: 100.0,
            valid: true,
            ..Default::default()
        }));
        let output = MonitorStopOut {
            samples,
            errors: vec!["startup read failed".into()],
            ..Default::default()
        };

        let issue = cts_monitor_runtime_issue(&output, &window).expect("diagnostic issue");
        assert_eq!(issue.code, "CTSTRAFFIC_MONITOR_RUNTIME_ERROR");
        assert!(issue.detail.contains("不影响本轮主判定"));
        assert!(cts_monitor_issue_verdict(&issue).is_none());

        let stats = monitor_rate_stats(&output, &window, true, window.start_ms);
        assert_eq!(stats.avg_mbps, Some(100.0));
        assert_eq!(stats.coverage, 1.0);

        let errors_only = MonitorStopOut {
            samples: (3..=12)
                .map(|second| MonitorSample {
                    elapsed_ms: second * 1_000,
                    interval_ms: 1_000,
                    rx_mbps: 100.0,
                    valid: true,
                    ..Default::default()
                })
                .collect(),
            errors: vec!["sampling thread exited after the scored window".into()],
            ..Default::default()
        };
        let issue =
            cts_monitor_runtime_issue(&errors_only, &window).expect("unlocated diagnostic issue");
        assert!(issue.detail.contains("不影响本轮主判定"));
        assert!(cts_monitor_issue_verdict(&issue).is_none());
        assert_eq!(
            monitor_rate_stats(&errors_only, &window, true, window.start_ms).coverage,
            1.0
        );
    }

    #[test]
    fn ctstraffic_builder_setup_error_returns_before_agent_or_cts_start() {
        let (ctx, db_path) = isolated_ctx(0);
        let mut task = ctstraffic_task(true);
        // UDP server 在 src 端；放到 Agent 且使用不可连接的 agent_port=0。
        // 若没有在 run_ctstraffic_leg 最前置返回，就会进入
        // /ctstraffic/start 并丢失 builder 给出的精确错误。
        task.src = endpoint(Side::Agent, "agent0", "192.168.1.3");
        task.dst = endpoint(Side::Master, "master0", "192.168.1.2");
        let builder_error = "CTS UDP socket buffer synthetic-invalid 无法解析";
        task.setup_error = Some(builder_error.into());
        let unit = Unit {
            id: "cts-builder-setup-error".into(),
            title: "CTS builder setup error".into(),
            bidir: false,
            legs: Vec::new(),
            est_secs: 1,
        };

        let outcome = ctx.run_ctstraffic_leg(
            0,
            &unit,
            0,
            "ab",
            &task,
            LifecycleLease {
                owner_id: "cts-builder-setup-owner",
                lease_secs: 1,
            },
        );

        assert_eq!(outcome.verdict, Verdict::SetupError);
        assert_eq!(outcome.reason_code, "CTSTRAFFIC_ARGS_INVALID");
        assert_eq!(outcome.reason_detail, builder_error);
        assert_eq!(outcome.main_rows, vec![0]);
        let rows = ctx.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, Verdict::SetupError);
        assert_eq!(rows[0].execution_status, ExecutionStatus::Error);
        assert_eq!(rows[0].reason_code, "CTSTRAFFIC_ARGS_INVALID");
        assert_eq!(rows[0].reason_detail, builder_error);
        assert_eq!(
            rows[0].raws,
            vec![("ctsTraffic 启动错误".into(), builder_error.into())]
        );
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn test_required_udp_stream_quorum() {
        let cfg = RateCheckCfg::default();
        assert_eq!(required_udp_streams(1, &cfg, None, Some(500.0)), 1);
        assert_eq!(required_udp_streams(2, &cfg, None, Some(500.0)), 2);
        assert_eq!(required_udp_streams(5, &cfg, None, Some(500.0)), 4);
        assert_eq!(
            required_udp_streams(20, &cfg, Some(8400.0), Some(500.0)),
            18
        );
        assert_eq!(
            required_udp_streams(20, &cfg, Some(6400.0), Some(500.0)),
            18
        );
    }

    #[test]
    fn single_udp_stream_gets_three_total_attempts_and_hard_failure_after_execution() {
        assert_eq!(effective_udp_retries(0, true), 2);
        assert_eq!(effective_udp_retries(1, true), 2);
        assert_eq!(effective_udp_retries(4, true), 4);
        assert_eq!(effective_udp_retries(1, false), 1);

        assert_eq!(zero_udp_stream_verdict(1, true), Verdict::RateFail);
        assert_eq!(zero_udp_stream_verdict(1, false), Verdict::SetupError);
        assert_eq!(zero_udp_stream_verdict(2, true), Verdict::SetupError);
    }

    #[test]
    fn iperf_single_udp_only_counts_started_and_reaped_processes_as_safe_attempts() {
        let missing_tool = IperfClientOut {
            output: "主控机未找到 iperf3".into(),
            process_started: Some(false),
            cleanup_confirmed: Some(true),
            ..Default::default()
        };
        assert!(iperf_client_setup_error(&missing_tool).is_some());

        let invalid_window = IperfClientOut {
            output: "iperf3: error - unable to set socket buffer size: Invalid argument".into(),
            process_started: Some(true),
            cleanup_confirmed: Some(true),
            ..Default::default()
        };
        assert!(iperf_client_setup_error(&invalid_window).is_some());

        let timeout_reaped = IperfClientOut {
            timed_out: true,
            process_started: Some(true),
            cleanup_confirmed: Some(true),
            output: "timed out and reaped".into(),
            ..Default::default()
        };
        assert_eq!(iperf_client_setup_error(&timeout_reaped), None);

        let connection_refused = IperfClientOut {
            process_started: Some(true),
            cleanup_confirmed: Some(true),
            output: "iperf3: error - unable to connect to server: Connection refused".into(),
            ..Default::default()
        };
        assert_eq!(iperf_client_setup_error(&connection_refused), None);

        let cleanup_unknown = IperfClientOut {
            process_started: Some(true),
            cleanup_confirmed: None,
            ..Default::default()
        };
        assert!(iperf_client_setup_error(&cleanup_unknown).is_some());
    }

    #[test]
    fn iperf_tool_measurement_can_come_from_server_output_without_merging_attempts() {
        let client_output = "iperf3: error - control socket closed";
        let server_output =
            "[  5]   0.00-10.04 sec  119 MBytes  99.6 Mbits/sec  0.014 ms  312/86380 (0.36%) receiver";
        let parsed = iperf::parse_output(&format!("{client_output}\n{server_output}"));
        assert!(parsed.has_measurement());
        // 312/86380 —— 由计数算出，比 iperf3 打印的 0.36 精确。
        assert!((parsed.udp_loss_pct.unwrap() - 0.361_194_7).abs() < 1e-6);

        let next_attempt = iperf::parse_output("iperf3: error - unable to connect to server");
        assert!(!next_attempt.has_measurement());
    }

    #[test]
    fn ctstraffic_single_udp_attempt_budget_has_a_three_attempt_floor() {
        assert_eq!(cts_attempt_budget(0, true), 3);
        assert_eq!(cts_attempt_budget(1, true), 3);
        assert_eq!(cts_attempt_budget(2, true), 3);
        assert_eq!(cts_attempt_budget(4, true), 5);
        assert_eq!(cts_attempt_budget(4, false), 1);
    }

    #[test]
    fn ctstraffic_measured_timeout_or_abnormal_exit_is_a_runtime_failure() {
        let mut timed_out = ctstraffic_attempt(0, true);
        timed_out.client = IperfClientOut {
            timed_out: true,
            output: "manager timeout; process reaped".into(),
            process_started: Some(true),
            cleanup_confirmed: Some(true),
            ..Default::default()
        };
        let (timeout_verdict, timeout_code, timeout_detail) =
            cts_runtime_failure_verdict(&timed_out, 0, false).unwrap();
        assert_eq!(timeout_verdict, Verdict::RateFail);
        assert_eq!(timeout_code, "CTSTRAFFIC_RUNTIME_ERRORS");
        assert!(timeout_detail.contains("client 超时"));

        let mut abnormal_exit = ctstraffic_attempt(0, true);
        abnormal_exit.client = IperfClientOut {
            output: "ctsTraffic exited with code 7".into(),
            process_started: Some(true),
            cleanup_confirmed: Some(true),
            ..Default::default()
        };
        let (_, exit_code, exit_detail) =
            cts_runtime_failure_verdict(&abnormal_exit, 0, false).unwrap();
        assert_eq!(exit_code, "CTSTRAFFIC_RUNTIME_ERRORS");
        assert!(exit_detail.contains("未正常完成"));

        let (_, counted_code, counted_error) =
            cts_runtime_failure_verdict(&abnormal_exit, 3, false).unwrap();
        assert_eq!(counted_code, "CTSTRAFFIC_RUNTIME_ERRORS");
        assert!(counted_error.contains("3 个网络/协议/数据错误"));

        let normal = ctstraffic_attempt(0, true);
        assert!(cts_runtime_failure_verdict(&normal, 0, true).is_none());
    }

    #[test]
    fn ctstraffic_measured_server_failure_is_runtime_but_unmeasured_is_setup() {
        let mut measured = ctstraffic_attempt(0, true);
        measured.server_unexpected_failure = true;
        measured.server_output = "server statistics: 500 Mbps\nserver timed out".into();

        assert!(cts_server_unexpected_setup_error(
            measured.server_unexpected_failure,
            measured.traffic_established,
            &measured.server_output,
        )
        .is_none());
        let (verdict, code, detail) = cts_runtime_failure_verdict(&measured, 0, true).unwrap();
        assert_eq!(verdict, Verdict::RateFail);
        assert_eq!(code, "CTSTRAFFIC_RUNTIME_ERRORS");
        assert!(detail.contains("server 在显式停止前异常退出或超时"));
        assert!(!cts_should_retry_after_last(
            std::slice::from_ref(&measured),
            3,
            true
        ));
        assert!(!cts_single_udp_exhausted(
            std::slice::from_ref(&measured),
            1,
            true
        ));

        let mut unmeasured = ctstraffic_attempt(0, false);
        unmeasured.server_unexpected_failure = true;
        unmeasured.server_output = "server exited with code 7".into();
        let (setup_code, setup_detail) = cts_server_unexpected_setup_error(
            unmeasured.server_unexpected_failure,
            unmeasured.traffic_established,
            &unmeasured.server_output,
        )
        .unwrap();
        assert_eq!(setup_code, "CTSTRAFFIC_SERVER_FAILED");
        assert_eq!(setup_detail, "server exited with code 7");
        assert!(cts_runtime_failure_verdict(&unmeasured, 0, false).is_none());
        assert!(!cts_should_retry_after_last(
            std::slice::from_ref(&unmeasured),
            3,
            true
        ));

        let all_safe_misses = vec![
            ctstraffic_attempt(0, false),
            ctstraffic_attempt(1, false),
            ctstraffic_attempt(2, false),
        ];
        assert!(cts_single_udp_exhausted(&all_safe_misses, 3, true));
    }

    #[test]
    fn ctstraffic_server_requires_explicit_process_start_and_reap_evidence() {
        let confirmed = Ok(CtsTrafficStopOut {
            terminated: true,
            result: Some(IperfClientOut {
                process_started: Some(true),
                cleanup_confirmed: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(cts_stop_process_evidence(&confirmed), (true, true));

        let legacy_unknown = Ok(CtsTrafficStopOut {
            terminated: true,
            result: Some(IperfClientOut::default()),
            ..Default::default()
        });
        assert_eq!(cts_stop_process_evidence(&legacy_unknown), (false, false));

        let reap_failed = Ok(CtsTrafficStopOut {
            terminated: true,
            result: Some(IperfClientOut {
                process_started: Some(true),
                cleanup_confirmed: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(cts_stop_process_evidence(&reap_failed), (true, false));
        assert_eq!(
            cts_stop_process_evidence(&Err("stop failed".into())),
            (false, false)
        );
    }

    #[test]
    fn ctstraffic_server_pre_stop_state_distinguishes_runtime_failure_and_cancel() {
        let timed_out_before_stop = Ok(CtsTrafficStopOut {
            was_done: true,
            terminated: true,
            result: Some(IperfClientOut {
                timed_out: true,
                process_started: Some(true),
                cleanup_confirmed: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            cts_server_pre_stop_failures(&timed_out_before_stop),
            (false, true)
        );

        let abnormal_exit_before_stop = Ok(CtsTrafficStopOut {
            was_done: true,
            terminated: true,
            result: Some(IperfClientOut {
                output: "server exited with code 7".into(),
                process_started: Some(true),
                cleanup_confirmed: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            cts_server_pre_stop_failures(&abnormal_exit_before_stop),
            (false, true)
        );

        let cancelled_before_stop = Ok(CtsTrafficStopOut {
            was_done: true,
            terminated: true,
            result: Some(IperfClientOut {
                cancelled: true,
                process_started: Some(true),
                cleanup_confirmed: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            cts_server_pre_stop_failures(&cancelled_before_stop),
            (true, false)
        );

        let cancelled_by_this_stop = Ok(CtsTrafficStopOut {
            was_done: false,
            terminated: true,
            result: Some(IperfClientOut {
                cancelled: true,
                process_started: Some(true),
                cleanup_confirmed: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            cts_server_pre_stop_failures(&cancelled_by_this_stop),
            (false, false),
            "controller 本轮发出的正常 server stop 不是异常"
        );

        let timed_out_between_snapshot_and_cancel = Ok(CtsTrafficStopOut {
            was_done: false,
            terminated: true,
            result: Some(IperfClientOut {
                timed_out: true,
                process_started: Some(true),
                cleanup_confirmed: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            cts_server_pre_stop_failures(&timed_out_between_snapshot_and_cancel),
            (false, true),
            "快照后自行 timeout 且未确认 cancelled 仍是 runtime failure"
        );

        let failed_between_snapshot_and_cancel = Ok(CtsTrafficStopOut {
            was_done: false,
            terminated: true,
            result: Some(IperfClientOut {
                output: "server exited with code 7".into(),
                process_started: Some(true),
                cleanup_confirmed: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            cts_server_pre_stop_failures(&failed_between_snapshot_and_cancel),
            (false, true),
            "快照后自行异常退出且未确认 cancelled 仍是 runtime failure"
        );
    }

    #[test]
    fn ctstraffic_selects_first_measured_attempt_and_only_exhausts_all_safe_misses() {
        let mut first_two_miss_then_success = vec![
            ctstraffic_attempt(0, false),
            ctstraffic_attempt(1, false),
            ctstraffic_attempt(2, true),
        ];
        first_two_miss_then_success[0].parsed.network_errors = Some(99);
        assert!(cts_should_retry_after_last(
            &first_two_miss_then_success[..1],
            3,
            true
        ));
        assert!(cts_should_retry_after_last(
            &first_two_miss_then_success[..2],
            3,
            true
        ));
        assert!(!cts_should_retry_after_last(
            &first_two_miss_then_success,
            3,
            true
        ));
        assert_eq!(
            select_cts_attempt_index(&first_two_miss_then_success),
            Some(2)
        );
        assert!(!cts_single_udp_exhausted(
            &first_two_miss_then_success,
            3,
            true
        ));
        assert_eq!(cts_retry_count(&first_two_miss_then_success), 2);
        let selected = select_cts_attempt_index(&first_two_miss_then_success).unwrap();
        assert_eq!(selected, 2);
        assert_eq!(
            first_two_miss_then_success[selected].parsed.error_count(),
            0,
            "前两轮错误不能污染第三轮成功结果"
        );
        let raw = format_ctstraffic_attempts(
            "ctsTraffic.exe -Listen:192.0.2.1",
            &first_two_miss_then_success,
            "",
        );
        assert!(raw.contains("=== attempt 1 ==="));
        assert!(raw.contains("=== attempt 2 ==="));
        assert!(raw.contains("=== attempt 3 ==="));
        assert!(raw.contains("CLIENT ATTEMPT 1"));
        assert!(raw.contains("CLIENT ATTEMPT 3"));

        let all_miss = vec![
            ctstraffic_attempt(0, false),
            ctstraffic_attempt(1, false),
            ctstraffic_attempt(2, false),
        ];
        assert_eq!(select_cts_attempt_index(&all_miss), Some(2));
        assert!(cts_single_udp_exhausted(&all_miss, 3, true));
        assert_eq!(cts_retry_count(&all_miss), 2);
    }

    #[test]
    fn ctstraffic_setup_cancel_or_unconfirmed_cleanup_never_retries_or_exhausts() {
        let mut setup = ctstraffic_attempt(0, false);
        setup.setup_error = Some(("CTSTRAFFIC_SETUP".into(), "setup".into()));
        setup.full_attempt = false;

        let mut cancelled = ctstraffic_attempt(0, false);
        cancelled.client.cancelled = true;
        cancelled.full_attempt = false;

        let mut cleanup_failed = ctstraffic_attempt(0, false);
        cleanup_failed.cleanup_confirmed = false;
        cleanup_failed.client.cleanup_confirmed = Some(false);
        cleanup_failed.full_attempt = false;

        let mut legacy_unknown = ctstraffic_attempt(0, false);
        legacy_unknown.client.process_started = None;
        legacy_unknown.client.cleanup_confirmed = None;
        legacy_unknown.full_attempt = false;

        for blocked in [setup, cancelled, cleanup_failed, legacy_unknown] {
            assert!(!cts_should_retry_after_last(
                std::slice::from_ref(&blocked),
                3,
                true
            ));
            let attempts = vec![
                ctstraffic_attempt(0, false),
                ctstraffic_attempt(1, false),
                blocked,
            ];
            assert!(!cts_single_udp_exhausted(&attempts, 3, true));
        }
    }

    #[test]
    fn test_two_stream_direction_retries_but_never_degrades_to_one_stream_verdict() {
        let cfg = RateCheckCfg::default();
        let client = IperfClientOut::default();
        assert!(should_retry_udp_flow(
            0,
            cfg.flow_retries as usize,
            Duration::from_secs(2),
            Duration::from_secs(cfg.startup_timeout_secs),
            &client,
        ));
        assert_eq!(required_udp_streams(2, &cfg, None, Some(500.0)), 2);

        let timed_out = IperfClientOut {
            timed_out: true,
            ..Default::default()
        };
        assert!(!should_retry_udp_flow(
            0,
            1,
            Duration::from_secs(2),
            Duration::from_secs(15),
            &timed_out,
        ));
        assert!(!should_retry_udp_flow(
            0,
            1,
            Duration::from_secs(16),
            Duration::from_secs(15),
            &client,
        ));
    }

    #[test]
    fn test_discovery_stages_are_quartered() {
        let stages_20: Vec<u64> = (0..20).map(|idx| discovery_stage(idx, 20)).collect();
        assert_eq!(&stages_20[0..5], &[0; 5]);
        assert_eq!(&stages_20[5..10], &[1; 5]);
        assert_eq!(&stages_20[10..15], &[2; 5]);
        assert_eq!(&stages_20[15..20], &[3; 5]);
        assert_eq!(
            (0..5)
                .map(|idx| discovery_stage(idx, 5))
                .collect::<Vec<_>>(),
            vec![0, 0, 1, 2, 3]
        );
    }

    #[test]
    fn test_bidir_5_and_2_streams_require_both_streams_on_small_leg() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plans = vec![
            udp_plan(0, "ab", 5, &master, &agent, 180),
            udp_plan(1, "ba", 2, &agent, &master, 180),
        ];
        let mut results = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            for (stream_pos, task) in plan.streams.iter().enumerate() {
                results.push(udp_flow(leg_pos, stream_pos, task, 1_000, 190_000, true));
            }
        }
        let monitors = HashMap::from([
            (agent.key(), monitor_until(190_000, 2_000.0, 2_000.0)),
            (master.key(), monitor_until(190_000, 2_000.0, 2_000.0)),
        ]);
        let windows =
            select_udp_effective_windows(&plans, &results, &monitors, &RateCheckCfg::default());
        for window in &windows.per_leg {
            assert!(window.complete);
            assert_eq!(window.start_ms, 6_000);
            assert_eq!(window.end_ms, 186_000);
            assert_eq!(window.available_secs, 184.0);
        }
        assert_eq!(windows.concurrency_secs, 180.0);

        let failed_small_leg_flow = results
            .iter_mut()
            .find(|flow| flow.leg_pos == 1 && flow.stream_pos == 1)
            .unwrap();
        failed_small_leg_flow.raw_ok = false;
        failed_small_leg_flow.events.clear();
        let windows =
            select_udp_effective_windows(&plans, &results, &monitors, &RateCheckCfg::default());

        // 小腿的流数不够，这条腿没结论——这一条不变。
        assert!(!windows.per_leg[1].complete);
        assert_eq!(windows.per_leg[1].available_secs, 0.0);

        // 但另一条腿整整 184 秒都在满速跑，它的数据必须留着。
        // 旧实现在这里把两条腿一起归零，run_20260825_215915_7684 的任务
        // 10/12/34/36 就是这样丢掉了 8 行 493~923Mbps 的实测。
        assert!(
            windows.per_leg[0].complete,
            "对向腿失败不得连坐抹掉本腿的有效窗口"
        );
        assert_eq!(windows.per_leg[0].available_secs, 184.0);

        // 并发确实没成立，这件事单独报，不混进腿的判定。
        assert_eq!(windows.concurrency_secs, 0.0);
    }

    #[test]
    fn test_leg_window_shortens_only_for_the_direction_that_dropped_early() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plans = vec![
            udp_plan(0, "ab", 2, &master, &agent, 180),
            udp_plan(1, "ba", 2, &agent, &master, 180),
        ];
        let mut results = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            for (stream_pos, task) in plan.streams.iter().enumerate() {
                let end_ms = if leg_pos == 1 && stream_pos == 1 {
                    175_000
                } else {
                    190_000
                };
                results.push(udp_flow(leg_pos, stream_pos, task, 1_000, end_ms, true));
            }
        }
        let monitors = HashMap::from([
            (agent.key(), monitor_until(190_000, 2_000.0, 2_000.0)),
            (master.key(), monitor_until(190_000, 2_000.0, 2_000.0)),
        ]);
        let windows =
            select_udp_effective_windows(&plans, &results, &monitors, &RateCheckCfg::default());
        // ba 腿有一条流 175s 就停了，只有这条腿的窗口被截短。
        assert!(!windows.per_leg[1].complete);
        assert_eq!(windows.per_leg[1].available_secs, 169.0);
        // ab 腿全程正常，不受影响。
        assert!(windows.per_leg[0].complete);
        assert_eq!(windows.per_leg[0].available_secs, 184.0);
        // 两条腿确实重叠过，重叠时长取交集。
        assert_eq!(windows.concurrency_secs, 169.0);
    }

    #[test]
    fn test_effective_window_supports_five_second_monitor_interval() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plans = vec![udp_plan(0, "ab", 2, &master, &agent, 180)];
        let results: Vec<UdpFlowRun> = plans[0]
            .streams
            .iter()
            .enumerate()
            .map(|(stream_pos, task)| udp_flow(0, stream_pos, task, 1_000, 190_000, true))
            .collect();
        let monitors = HashMap::from([(
            agent.key(),
            MonitorStopOut {
                samples: (0..=38)
                    .map(|idx| MonitorSample {
                        elapsed_ms: idx * 5_000,
                        interval_ms: 5_000,
                        rx_mbps: 1_000.0,
                        valid: true,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
        )]);
        let cfg = RateCheckCfg {
            sample_interval_ms: 5_000,
            ..Default::default()
        };
        let windows = select_udp_effective_windows(&plans, &results, &monitors, &cfg);
        assert!(windows.per_leg[0].complete);
        assert_eq!(
            windows.per_leg[0].end_ms - windows.per_leg[0].start_ms,
            180_000
        );
    }

    /// 接收端 monitor 缺失只能让**这一条腿**没结论。
    ///
    /// run_20260825_215915_7684 的任务 10 里，辅测端采样会话丢了
    /// （`网卡监控停止失败: 监控 ID 不存在: mon11`），旧实现在那里直接
    /// `return` 整个单元的零窗口，于是对向腿——主控网卡实时打印了一路
    /// 975.7Mbps——也一起被写成「未采集」。
    #[test]
    fn a_missing_monitor_only_blanks_its_own_leg() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plans = vec![
            udp_plan(0, "ab", 1, &master, &agent, 180),
            udp_plan(1, "ba", 1, &agent, &master, 180),
        ];
        let mut results = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            for (stream_pos, task) in plan.streams.iter().enumerate() {
                results.push(udp_flow(leg_pos, stream_pos, task, 1_000, 190_000, true));
            }
        }
        // 只有 master 侧（ba 腿的接收端）有采样；agent 侧的 monitor 丢了。
        let monitors = HashMap::from([(master.key(), monitor_until(190_000, 2_000.0, 2_000.0))]);
        let windows =
            select_udp_effective_windows(&plans, &results, &monitors, &RateCheckCfg::default());

        assert!(!windows.per_leg[0].complete, "ab 腿没有采样，无从判定");
        assert_eq!(windows.per_leg[0].available_secs, 0.0);
        assert!(
            windows.per_leg[1].complete,
            "ba 腿的采样是完整的，不能被对向的监控丢失连累"
        );
        assert_eq!(windows.concurrency_secs, 0.0);
    }

    #[test]
    fn test_rate_stats_subtract_background_and_report_p10() {
        let out = MonitorStopOut {
            samples: vec![
                (0, 100.0),
                (1_000, 100.0),
                (2_000, 100.0),
                (3_000, 1_100.0),
                (4_000, 1_000.0),
                (5_000, 1_200.0),
                (6_000, 1_100.0),
            ]
            .into_iter()
            .map(|(elapsed_ms, rx_mbps)| MonitorSample {
                elapsed_ms,
                interval_ms: 1_000,
                rx_mbps,
                valid: true,
                ..Default::default()
            })
            .collect(),
            ..Default::default()
        };
        let window = EffectiveWindow {
            start_ms: 3_000,
            end_ms: 6_000,
            available_secs: 3.0,
            required_secs: 3,
            complete: true,
        };
        let stats = monitor_rate_stats(&out, &window, true, 3_000);
        assert_eq!(stats.avg_mbps, Some(1_000.0));
        assert_eq!(stats.p10_mbps, None);
        assert_eq!(stats.median_mbps, Some(1_000.0));
        assert_eq!(stats.coverage, 1.0);
    }

    #[test]
    fn test_sample_coverage_uses_actual_monitor_interval() {
        let window = EffectiveWindow {
            start_ms: 0,
            end_ms: 10_000,
            available_secs: 10.0,
            required_secs: 10,
            complete: true,
        };
        let mut out = MonitorStopOut {
            samples: (0..=5)
                .map(|idx| MonitorSample {
                    elapsed_ms: idx * 2_000,
                    interval_ms: 2_000,
                    rx_mbps: 1_000.0,
                    valid: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let complete = monitor_rate_stats(&out, &window, true, 0);
        assert_eq!(complete.coverage, 1.0);

        out.samples[2].valid = false;
        let missing_one = monitor_rate_stats(&out, &window, true, 0);
        assert!((missing_one.coverage - 0.8).abs() < f64::EPSILON);

        // 读取失败后恢复的有效样本会用同一段完整时间计算字节差和速率；
        // interval_ms 跨过失败周期时，应恢复这段时间的覆盖，而不是按样本数扣分。
        out.samples[2].valid = false;
        out.samples[3].interval_ms = 4_000;
        let recovered = monitor_rate_stats(&out, &window, true, 0);
        assert_eq!(recovered.coverage, 1.0);
    }

    #[test]
    fn test_rate_average_is_weighted_by_valid_time_and_clipped_to_window() {
        let out = MonitorStopOut {
            samples: vec![
                MonitorSample {
                    elapsed_ms: 1_000,
                    interval_ms: 1_000,
                    rx_mbps: 100.0,
                    valid: true,
                    ..Default::default()
                },
                MonitorSample {
                    elapsed_ms: 4_000,
                    interval_ms: 3_000,
                    rx_mbps: 300.0,
                    valid: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let full = EffectiveWindow {
            start_ms: 0,
            end_ms: 4_000,
            available_secs: 4.0,
            required_secs: 4,
            complete: true,
        };
        let full_stats = monitor_rate_stats(&out, &full, true, 0);
        assert_eq!(full_stats.avg_mbps, Some(250.0));
        assert_eq!(full_stats.coverage, 1.0);
        assert_eq!(full_stats.p10_mbps, None);

        // 第二个样本横跨窗口两端，只有 [2s, 3s) 的一秒应纳入统计。
        let clipped = EffectiveWindow {
            start_ms: 2_000,
            end_ms: 3_000,
            available_secs: 1.0,
            required_secs: 1,
            complete: true,
        };
        let clipped_stats = monitor_rate_stats(&out, &clipped, true, 0);
        assert_eq!(clipped_stats.avg_mbps, Some(300.0));
        assert_eq!(clipped_stats.coverage, 1.0);

        // 异常/合成输入可能乱序且区间嵌套；覆盖率必须按区间并集计算，
        // 不能因为先看到内层区间而丢掉外层区间的前半段。
        let nested_out = MonitorStopOut {
            samples: vec![
                MonitorSample {
                    elapsed_ms: 2_000,
                    interval_ms: 1_000,
                    rx_mbps: 300.0,
                    valid: true,
                    ..Default::default()
                },
                MonitorSample {
                    elapsed_ms: 4_000,
                    interval_ms: 4_000,
                    rx_mbps: 100.0,
                    valid: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let nested_stats = monitor_rate_stats(&nested_out, &full, true, 0);
        assert_eq!(nested_stats.avg_mbps, Some(100.0));
        assert_eq!(nested_stats.coverage, 1.0);
    }

    #[test]
    fn test_target_requires_tx_and_rx_sample_coverage() {
        let rx_stats = RateStats {
            coverage: 1.0,
            ..Default::default()
        };
        let sparse_tx_stats = RateStats {
            coverage: 0.2,
            p10_mbps: Some(10_000.0),
            ..Default::default()
        };
        assert!(!rate_sample_coverage_sufficient(
            &rx_stats,
            &sparse_tx_stats,
            true
        ));
        assert!(rate_sample_coverage_sufficient(
            &rx_stats,
            &sparse_tx_stats,
            false
        ));

        let complete_tx_stats = RateStats {
            coverage: MIN_RATE_SAMPLE_COVERAGE,
            ..Default::default()
        };
        assert!(rate_sample_coverage_sufficient(
            &rx_stats,
            &complete_tx_stats,
            true
        ));

        let missing_p10 = RateStats {
            coverage: 1.0,
            ..Default::default()
        };
        let complete_p10 = RateStats {
            coverage: 1.0,
            p10_mbps: Some(10_000.0),
            rolling_coverage: 1.0,
            ..Default::default()
        };
        assert!(!rate_window_coverage_sufficient(
            &missing_p10,
            &complete_p10,
            true
        ));
        assert!(!rate_window_coverage_sufficient(
            &complete_p10,
            &missing_p10,
            true
        ));
        assert!(rate_window_coverage_sufficient(
            &missing_p10,
            &missing_p10,
            false
        ));

        let sparse_rolling = RateStats {
            coverage: 1.0,
            p10_mbps: Some(10_000.0),
            rolling_coverage: MIN_RATE_SAMPLE_COVERAGE - 0.01,
            ..Default::default()
        };
        assert!(!rate_window_coverage_sufficient(
            &sparse_rolling,
            &complete_p10,
            true
        ));
    }

    #[test]
    fn test_five_second_rolling_p10_uses_sample_time_coverage() {
        let fast_out = MonitorStopOut {
            samples: (0..=50)
                .map(|idx| MonitorSample {
                    elapsed_ms: idx * 200,
                    interval_ms: 200,
                    rx_mbps: if (21..=25).contains(&idx) { 0.0 } else { 100.0 },
                    valid: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let fast_window = EffectiveWindow {
            start_ms: 0,
            end_ms: 10_000,
            available_secs: 10.0,
            required_secs: 10,
            complete: true,
        };
        let fast_stats = monitor_rate_stats(&fast_out, &fast_window, true, 0);
        let fast_p10 = fast_stats.p10_mbps.unwrap();
        assert!(
            (80.0..90.0).contains(&fast_p10),
            "200ms 采样应将 1 秒掉速按五秒窗口摊薄，实际 P10={fast_p10}"
        );

        let rounded_intervals: Vec<(u64, u64, f64)> =
            (1..=5).map(|second| (second * 1_000, 999, 100.0)).collect();
        assert_eq!(
            rolling_time_window_series(&rounded_intervals, 0, 5_000),
            vec![(5_000, 100.0)]
        );

        let slow_out = MonitorStopOut {
            samples: [0.0, 100.0, 100.0, 100.0, 100.0]
                .into_iter()
                .enumerate()
                .map(|(idx, rx_mbps)| MonitorSample {
                    elapsed_ms: (idx as u64 + 1) * 5_000,
                    interval_ms: 5_000,
                    rx_mbps,
                    valid: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let slow_window = EffectiveWindow {
            start_ms: 0,
            end_ms: 25_000,
            available_secs: 25.0,
            required_secs: 25,
            complete: true,
        };
        let slow_stats = monitor_rate_stats(&slow_out, &slow_window, true, 0);
        assert_eq!(slow_stats.p10_mbps, Some(0.0));

        let short_window = EffectiveWindow {
            start_ms: 0,
            end_ms: 4_800,
            available_secs: 4.8,
            required_secs: 4,
            complete: true,
        };
        let short_stats = monitor_rate_stats(&fast_out, &short_window, true, 0);
        assert_eq!(short_stats.coverage, 1.0);
        assert_eq!(short_stats.p10_mbps, None);

        let fragmented_out = MonitorStopOut {
            samples: vec![
                MonitorSample {
                    elapsed_ms: 4_900,
                    interval_ms: 4_900,
                    rx_mbps: 100.0,
                    valid: true,
                    ..Default::default()
                },
                MonitorSample {
                    elapsed_ms: 9_900,
                    interval_ms: 4_900,
                    rx_mbps: 100.0,
                    valid: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let fragmented_window = EffectiveWindow {
            start_ms: 0,
            end_ms: 10_000,
            available_secs: 10.0,
            required_secs: 10,
            complete: true,
        };
        let fragmented_stats = monitor_rate_stats(&fragmented_out, &fragmented_window, true, 0);
        assert!((fragmented_stats.coverage - 0.98).abs() < f64::EPSILON);
        assert_eq!(fragmented_stats.p10_mbps, None);
    }

    #[test]
    fn test_recovery_sample_restores_average_but_not_rolling_window_coverage() {
        let out = MonitorStopOut {
            samples: (1..=20)
                .map(|second| {
                    if second == 6 {
                        MonitorSample {
                            elapsed_ms: second * 1_000,
                            interval_ms: 1_000,
                            valid: false,
                            ..Default::default()
                        }
                    } else {
                        MonitorSample {
                            elapsed_ms: second * 1_000,
                            // 第 7 秒恢复时，字节差/速率正确覆盖 [5s, 7s)，
                            // 可用于总平均值，但不能证明其中任一 5 秒窗口稳定。
                            interval_ms: if second == 7 { 2_000 } else { 1_000 },
                            rx_mbps: 100.0,
                            valid: true,
                            ..Default::default()
                        }
                    }
                })
                .collect(),
            ..Default::default()
        };
        let window = EffectiveWindow {
            start_ms: 0,
            end_ms: 20_000,
            available_secs: 20.0,
            required_secs: 20,
            complete: true,
        };
        let stats = monitor_rate_stats(&out, &window, true, 0);
        assert_eq!(stats.avg_mbps, Some(100.0));
        assert_eq!(stats.coverage, 1.0);
        assert_eq!(stats.p10_mbps, Some(100.0));
        assert!((stats.rolling_coverage - 0.625).abs() < f64::EPSILON);
        assert!(!rate_window_coverage_sufficient(&stats, &stats, true));
    }

    /// 构造一份「采样完整、RX 稳定在 rx_mbps」的统计，用于单独验证判定链。
    fn healthy_stats(rx_mbps: f64) -> RateStats {
        RateStats {
            avg_mbps: Some(rx_mbps),
            p10_mbps: Some(rx_mbps),
            median_mbps: Some(rx_mbps),
            p95_mbps: Some(rx_mbps),
            min_mbps: Some(rx_mbps),
            max_mbps: Some(rx_mbps),
            coverage: 1.0,
            rolling_coverage: 1.0,
            // 全程稳定在 rx_mbps：35 个 5 秒窗口一个都不掉。
            rolling_series: (1..=35).map(|i| (i * 5_000, rx_mbps)).collect(),
            baseline_mbps: 0.0,
            stalled_ratio: 0.0,
        }
    }

    fn full_window(secs: f64) -> EffectiveWindow {
        EffectiveWindow {
            start_ms: 0,
            end_ms: (secs * 1000.0) as u64,
            available_secs: secs,
            required_secs: secs as u64,
            complete: true,
        }
    }

    const TAIL_HANDSHAKE_ERROR: &str = "iperf3: error - unable to send control message - port may not be available, the other side may have stopped running, etc.: Connection reset by peer";

    /// run_20260825_215915_7684 任务 103：主控 WLAN → 以太网 5 完整跑满
    /// 180s，接收端网卡实测 1067.902Mbps，只有最后的结果交换失败。
    /// 旧代码把它判成 SETUP_ERROR / 接收=0，等于用诊断口径的故障
    /// 否决了正式口径已经拿到的结论。
    #[test]
    fn client_tail_failure_after_full_window_keeps_nic_verdict() {
        let rx = healthy_stats(1067.902);
        let window = full_window(180.0);
        let (verdict, code, detail) = iperf_flow_verdict(IperfFlowVerdictIn {
            raw_ok: false,
            measurement: true,
            effective_window: &window,
            required_secs: 180,
            rate_mode: RateMode::Observe,
            rx_target_mbps: None,
            rx_stats: &rx,
            tx_stats: &rx,
            client_tail: TAIL_HANDSHAKE_ERROR,
            rx_monitor: None,
        });
        assert_eq!(
            verdict,
            Verdict::Measured,
            "跑满全程只是收尾握手失败，不能判成环境错误"
        );
        assert_eq!(
            code, "TARGET_UNKNOWN",
            "网卡口径的原始 reason_code 必须保留"
        );
        assert!(
            detail.contains("IPERF_SUMMARY_LOST"),
            "必须写明工具自报不可用: {detail}"
        );
        assert!(detail.contains("1067.902"), "必须保留网卡实测值: {detail}");
    }

    /// 同一条降级路径不能变成「有网卡数就一律放行」：RX 低于目标仍要 RATE_FAIL，
    /// RX 缺失仍要 NOT_EVALUATED。
    #[test]
    fn tail_failure_downgrade_never_upgrades_a_failing_rate() {
        let window = full_window(180.0);

        let below = healthy_stats(400.0);
        let (verdict, code, _) = iperf_flow_verdict(IperfFlowVerdictIn {
            raw_ok: false,
            measurement: true,
            effective_window: &window,
            required_secs: 180,
            rate_mode: RateMode::Verify,
            rx_target_mbps: Some(900.0),
            rx_stats: &below,
            tx_stats: &below,
            client_tail: TAIL_HANDSHAKE_ERROR,
            rx_monitor: None,
        });
        assert_eq!(verdict, Verdict::RateFail);
        assert_eq!(code, "RX_BELOW_TARGET");

        // 任务 115 那种「链路已断、网卡全零、iperf 仍自报 136Mbps」的形态：
        // 降级路径必须交给 evaluate_nic_rx 判成 NOT_EVALUATED，
        // 绝不能因为拿到了 sender 数字就算测到了。
        let dead = RateStats {
            avg_mbps: Some(0.0),
            coverage: 1.0,
            rolling_coverage: 1.0,
            ..Default::default()
        };
        let (verdict, code, _) = iperf_flow_verdict(IperfFlowVerdictIn {
            raw_ok: false,
            measurement: true,
            effective_window: &window,
            required_secs: 180,
            rate_mode: RateMode::Observe,
            rx_target_mbps: None,
            rx_stats: &dead,
            tx_stats: &dead,
            client_tail: TAIL_HANDSHAKE_ERROR,
            rx_monitor: None,
        });
        assert_eq!(verdict, Verdict::NotEvaluated);
        assert_eq!(code, "NIC_RATE_MISSING");
    }

    /// 链路中途失联是横跨一整段单元的事实，逐行看永远拼不出来，
    /// 必须在报告最顶上单独说一次。
    /// 结构断言：熔断检查必须在单元循环**开头**，不能落在结尾。
    ///
    /// 单元有多条提前 `continue` 的路径（resume 命中、前置拦截、网卡消失），
    /// 检查放在结尾时那些路径会整个跳过它。而「网卡消失」恰恰是这个设置最该
    /// 拦住的场景——被测设备掉线后，每个单元开跑前的重扫都会看到网卡不见了，
    /// 队列一路空转到底，`aborted_at_unit` 也永远是 None。
    ///
    /// 这类「代码位置决定行为」的约束普通单测抓不到（把检查挪回结尾，所有
    /// 现有用例依然全绿），所以在源码层面把门关上。
    #[test]
    fn the_abort_gate_runs_before_any_early_continue() {
        let source = include_str!("executor.rs");
        let loop_start = source
            .find("for (i, unit) in units.iter().enumerate() {")
            .expect("单元循环");
        // 只截到函数结束，别把本用例自己的字符串字面量也数进去。
        let loop_end = source[loop_start..]
            .find("\n    fn ")
            .map(|offset| loop_start + offset)
            .unwrap_or(source.len());
        let loop_body = &source[loop_start..loop_end];

        let gate = loop_body
            .find("self.cfg.abort_after_dead_traffic_units")
            .expect("熔断检查必须在单元循环内");
        let first_continue = loop_body.find("continue;").unwrap_or(usize::MAX);
        assert!(
            gate < first_continue,
            "熔断检查必须排在任何 continue 之前，否则提前退出的路径会绕过它"
        );
        assert_eq!(
            loop_body
                .matches("self.cfg.abort_after_dead_traffic_units")
                .count(),
            1,
            "只能有一处熔断检查；两处必然会漂移"
        );
    }

    #[test]
    fn run_health_banner_surfaces_a_dead_link_streak() {
        let healthy = RunSummary {
            max_dead_traffic_streak: 1,
            ..Default::default()
        };
        assert!(
            healthy.run_health_banner().is_empty(),
            "偶发一个空单元不值得惊动读报告的人"
        );

        let dead = RunSummary {
            max_dead_traffic_streak: 6,
            ..Default::default()
        };
        let banner = dead.run_health_banner();
        assert!(banner.contains('6'), "{banner}");
        assert!(banner.contains("不代表设备性能"), "{banner}");

        let aborted = RunSummary {
            max_dead_traffic_streak: 2,
            aborted_at_unit: Some(114),
            ..Default::default()
        };
        let banner = aborted.run_health_banner();
        assert!(banner.contains("114"), "必须写清在哪里停的: {banner}");
        assert!(banner.contains("中止"), "{banner}");
    }

    /// 切不出有效窗口时，判定保持 NOT_EVALUATED，但必须把「这块网卡到底
    /// 收到了多少」说出来。
    ///
    /// 任务 97 的接收网卡 202/202 个样本有流量、全程均值 487.1Mbps，
    /// 报表却只有一个「未采集」——那既不是没测到，也不是没流量。
    #[test]
    fn an_unusable_window_still_reports_what_the_nic_actually_saw() {
        let empty_window = EffectiveWindow {
            required_secs: 180,
            ..Default::default()
        };
        let monitor = MonitorStopOut {
            seconds: 205.8,
            avg_mbps: 487.125_869,
            ..Default::default()
        };
        let (verdict, code, detail) = iperf_flow_verdict(IperfFlowVerdictIn {
            raw_ok: true,
            measurement: true,
            effective_window: &empty_window,
            required_secs: 180,
            rate_mode: RateMode::Observe,
            rx_target_mbps: None,
            rx_stats: &RateStats::default(),
            tx_stats: &RateStats::default(),
            client_tail: "",
            rx_monitor: Some(&monitor),
        });
        assert_eq!(verdict, Verdict::NotEvaluated, "窗口切不出来就是没结论");
        assert_eq!(code, "IPERF_EFFECTIVE_WINDOW_SHORT");
        assert!(detail.contains("487.126"), "必须给出全程实测值: {detail}");
        assert!(
            detail.contains("不作判定依据"),
            "同时必须写明它不是判定口径: {detail}"
        );
    }

    /// 没有采样数据时不能凭空编一个数出来——「未采集」在这种情况下是对的。
    #[test]
    fn an_unusable_window_without_samples_stays_silent() {
        let empty_window = EffectiveWindow {
            required_secs: 180,
            ..Default::default()
        };
        let (_, _, detail) = iperf_flow_verdict(IperfFlowVerdictIn {
            raw_ok: true,
            measurement: true,
            effective_window: &empty_window,
            required_secs: 180,
            rate_mode: RateMode::Observe,
            rx_target_mbps: None,
            rx_stats: &RateStats::default(),
            tx_stats: &RateStats::default(),
            client_tail: "",
            rx_monitor: None,
        });
        assert!(!detail.contains("全程"), "{detail}");
    }

    /// 窗口没攒够就失败的，仍然是环境错误——降级只对「已经跑满」生效。
    #[test]
    fn client_failure_before_a_full_window_is_still_a_setup_error() {
        let rx = healthy_stats(500.0);
        let short = EffectiveWindow {
            start_ms: 0,
            end_ms: 12_000,
            available_secs: 12.0,
            required_secs: 180,
            complete: false,
        };
        let (verdict, code, _) = iperf_flow_verdict(IperfFlowVerdictIn {
            raw_ok: false,
            measurement: true,
            effective_window: &short,
            required_secs: 180,
            rate_mode: RateMode::Observe,
            rx_target_mbps: None,
            rx_stats: &rx,
            tx_stats: &rx,
            client_tail: "iperf3: error - unable to connect to server",
            rx_monitor: None,
        });
        assert_eq!(verdict, Verdict::SetupError);
        assert_eq!(code, "IPERF_EXEC_FAILED");
    }

    #[test]
    fn test_udp_loss_uses_complete_weighted_datagram_counts() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plan = udp_plan(0, "ab", 2, &master, &agent, 10);
        let mut first = udp_flow(0, 0, &plan.streams[0], 0, 10_000, true);
        first.parsed.udp_lost_datagrams = Some(10);
        first.parsed.udp_total_datagrams = Some(100);
        first.parsed.udp_loss_pct = Some(10.0);
        let mut second = udp_flow(0, 1, &plan.streams[1], 0, 10_000, true);
        second.parsed.udp_lost_datagrams = Some(0);
        second.parsed.udp_total_datagrams = Some(900);
        second.parsed.udp_loss_pct = Some(0.0);
        assert_eq!(aggregate_udp_loss(&[&first, &second]), Some(1.0));

        // 缺计数就是「未知」。绝不能回退成对百分比取平均：那会把真实的
        // 1.0% 报成 5.0%，且流数越不均衡错得越离谱。
        second.parsed.udp_lost_datagrams = None;
        second.parsed.udp_total_datagrams = None;
        assert_eq!(aggregate_udp_loss(&[&first, &second]), None);

        second.parsed.udp_loss_pct = None;
        assert_eq!(aggregate_udp_loss(&[&first, &second]), None);
    }

    #[test]
    fn test_flow_interval_uses_traffic_after_latest_retry() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plan = udp_plan(0, "ab", 1, &master, &agent, 180);
        let mut flow = udp_flow(0, 0, &plan.streams[0], 1_000, 10_000, true);
        flow.events.insert(
            1,
            IperfFlowEvent {
                kind: IperfEventKind::Retry,
                elapsed_ms: 2_000,
                line: "retry".into(),
                ..Default::default()
            },
        );
        flow.events.insert(
            2,
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 3_000,
                mbps: Some(500.0),
                line: "traffic after retry".into(),
            },
        );
        assert_eq!(flow_active_interval(&flow), Some((3_000, 10_000)));
    }

    #[test]
    fn test_flow_interval_falls_back_to_connected_for_buffered_output() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plan = udp_plan(0, "ab", 1, &master, &agent, 180);
        let mut flow = udp_flow(0, 0, &plan.streams[0], 179_000, 180_000, true);
        flow.events.insert(
            0,
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 1_000,
                line: "connected".into(),
                ..Default::default()
            },
        );
        // Traffic 虽存在，但到达时刻只比 Ended 早 1 秒，不能代表 180 秒测试的起流时刻。
        assert_eq!(flow_active_interval(&flow), Some((1_000, 180_000)));

        flow.events
            .retain(|event| event.kind != IperfEventKind::Traffic);
        assert_eq!(flow_active_interval(&flow), Some((1_000, 180_000)));
    }

    #[test]
    fn test_flow_interval_uses_iperf_interval_when_all_output_is_buffered() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plan = udp_plan(0, "ab", 1, &master, &agent, 180);
        // 块缓冲刷新和 Ended 可能落在同一毫秒；仍应使用行内 205 秒区间反推。
        let mut flow = udp_flow(0, 0, &plan.streams[0], 215_000, 215_000, true);
        flow.events[0].line = "[  5]   0.00-205.00 sec  12.0 GBytes  500 Mbits/sec sender".into();
        assert_eq!(flow_active_interval(&flow), Some((10_000, 215_000)));
    }

    #[test]
    fn test_iperf_interval_parser_returns_start_and_end() {
        assert_eq!(
            iperf_interval_ms("[  5]   5.00-180.00 sec  12.0 GBytes  500 Mbits/sec sender"),
            Some((5_000, 180_000))
        );
        assert_eq!(
            iperf_interval_ms("[  5]   0,25-1,75 sec  100 MBytes  500 Mbits/sec"),
            Some((250, 1_750))
        );
        assert_eq!(iperf_interval_ms("[  5] 1.00-1.00 sec"), None);
        assert_eq!(iperf_interval_ms("[  5] 2.00-1.00 sec"), None);
        assert_eq!(iperf_interval_ms("[  5] invalid sec"), None);
    }

    #[test]
    fn test_flow_interval_uses_iperf_end_minus_start_duration() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plan = udp_plan(0, "ab", 1, &master, &agent, 175);
        let mut flow = udp_flow(0, 0, &plan.streams[0], 200_000, 200_000, true);
        flow.events[0].line = "[  5]   5.00-180.00 sec  12.0 GBytes  500 Mbits/sec sender".into();

        // 行内真正覆盖 175 秒；不能把区间终点 180 秒误当成持续时间。
        assert_eq!(flow_active_interval(&flow), Some((25_000, 200_000)));
    }

    #[test]
    fn short_reported_interval_stays_short_instead_of_falling_back_to_process_lifetime() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        // 要求 180 秒，但 iperf 行内区间只覆盖 175 秒。
        let plan = udp_plan(0, "ab", 1, &master, &agent, 180);
        // 块缓冲：全部 interval 在进程退出时集中到达。
        let mut flow = udp_flow(0, 0, &plan.streams[0], 199_990, 200_000, true);
        flow.events[0].line = "[  5]   5.00-180.00 sec  12.0 GBytes  500 Mbits/sec sender".into();
        flow.events.insert(
            0,
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 10_000,
                line: "started".into(),
                ..Default::default()
            },
        );

        // 必须按行内 175 秒裁剪，而不是回退成 client 进程寿命 190 秒 —— 后者会把
        // 短测量补成完整窗口，还把 startup 爬升算进 RX 平均。
        assert_eq!(flow_active_interval(&flow), Some((24_990, 199_990)));
        let window = iperf_effective_window(&flow.events, 180, true);
        assert!(
            !window.complete,
            "175 秒测量不能被判成完整 180 秒窗口: {window:?}"
        );
        assert_eq!(window.available_secs, 175.0);
        // 集中到达的毫秒级 Traffic 时间不能成为活跃时长。
        assert!(window.available_secs > 1.0);
    }

    #[test]
    fn longest_reported_interval_wins_over_a_later_per_second_interval_line() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let plan = udp_plan(0, "ab", 1, &master, &agent, 180);
        let mut flow = udp_flow(0, 0, &plan.streams[0], 200_000, 200_500, true);
        flow.events[0].line = "[  5]   0.00-180.00 sec  10.5 GBytes  500 Mbits/sec sender".into();
        // 逐秒 interval 行排在汇总行之后到达，不能被当成整段测量。
        flow.events.insert(
            1,
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 200_100,
                mbps: Some(500.0),
                line: "[  5] 179.00-180.00 sec  59.6 MBytes  500 Mbits/sec".into(),
            },
        );

        assert_eq!(flow_active_interval(&flow), Some((20_000, 200_000)));
    }

    #[test]
    fn tcp_rate_uses_only_the_event_proven_effective_window() {
        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 500,
                line: "started".into(),
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 2_000,
                line: "connected".into(),
                ..Default::default()
            },
            // 模拟旧版 iperf3 到结束时才刷出汇总行；行内区间仍能
            // 证明真实的 10 秒数据窗口为 [2s, 12s)。
            IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 12_000,
                mbps: Some(100.0),
                line: "[SUM] 0.00-10.00 sec 125 MBytes 100 Mbits/sec receiver".into(),
            },
            IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: 12_500,
                line: "ended".into(),
                ..Default::default()
            },
        ];
        let window = iperf_effective_window(&events, 10, true);
        assert_eq!(window.start_ms, 2_000);
        assert_eq!(window.end_ms, 12_000);
        assert_eq!(window.available_secs, 10.0);
        assert!(window.complete);

        let mut samples = vec![
            MonitorSample {
                elapsed_ms: 1_000,
                interval_ms: 1_000,
                rx_mbps: 10.0,
                valid: true,
                ..Default::default()
            },
            MonitorSample {
                elapsed_ms: 2_000,
                interval_ms: 1_000,
                rx_mbps: 10.0,
                valid: true,
                ..Default::default()
            },
        ];
        samples.extend((3..=12).map(|second| MonitorSample {
            elapsed_ms: second * 1_000,
            interval_ms: 1_000,
            rx_mbps: 110.0,
            valid: true,
            ..Default::default()
        }));
        // 最终汇总行回调之后的 client wait/reader join 样本必须被裁掉。
        samples.push(MonitorSample {
            elapsed_ms: 12_500,
            interval_ms: 500,
            rx_mbps: 10.0,
            valid: true,
            ..Default::default()
        });
        // 这个 stop/清理阶段样本必须被窗口裁掉。
        samples.push(MonitorSample {
            elapsed_ms: 13_500,
            interval_ms: 1_000,
            rx_mbps: 10.0,
            valid: true,
            ..Default::default()
        });
        let output = MonitorStopOut {
            avg_mbps: 42.0,
            samples,
            ..Default::default()
        };
        let stats = monitor_rate_stats(&output, &window, true, window.start_ms);
        assert_eq!(stats.avg_mbps, Some(100.0));
        assert_eq!(stats.coverage, 1.0);
        assert_eq!(stats.p10_mbps, Some(100.0));
        assert_ne!(stats.avg_mbps, Some(output.avg_mbps));

        let missing = iperf_effective_window(&events, 10, false);
        assert_eq!(missing.available_secs, 0.0);
        assert!(!missing.complete);
    }

    #[test]
    fn test_retry_count_includes_client_and_group_retry_events() {
        let events = vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Retry,
                line: "client retry".into(),
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Retry,
                line: "group retry".into(),
                ..Default::default()
            },
        ];
        assert_eq!(count_retry_events(&events), 2);
    }

    #[test]
    fn test_unit_reason_matches_aggregate_verdict_priority() {
        let outcomes = vec![
            LegOutcome {
                verdict: Verdict::RateFail,
                reason_code: "RX_BELOW_TARGET".into(),
                reason_detail: "AB rate failed".into(),
                rx_avg: None,
                main_rows: vec![],
                tag: "AB".into(),
            },
            LegOutcome {
                verdict: Verdict::SetupError,
                reason_code: "NO_STREAM_STARTED".into(),
                reason_detail: "BA setup failed".into(),
                rx_avg: None,
                main_rows: vec![],
                tag: "BA".into(),
            },
        ];
        let verdict = aggregate_unit_verdict(&outcomes);
        assert_eq!(verdict, Verdict::SetupError);
        assert_eq!(
            outcome_matching_verdict(&outcomes, verdict)
                .unwrap()
                .reason_code,
            "NO_STREAM_STARTED"
        );
    }

    #[test]
    fn hard_single_udp_failure_beats_other_direction_not_evaluated() {
        let outcomes = vec![
            LegOutcome {
                verdict: Verdict::RateFail,
                reason_code: "SINGLE_UDP_STREAM_FAILED".into(),
                reason_detail: "AB exhausted three attempts".into(),
                rx_avg: None,
                main_rows: vec![],
                tag: "ab".into(),
            },
            LegOutcome {
                verdict: Verdict::NotEvaluated,
                reason_code: "SAMPLE_COVERAGE_LOW".into(),
                reason_detail: "BA monitor incomplete".into(),
                rx_avg: Some(100.0),
                main_rows: vec![],
                tag: "ba".into(),
            },
        ];
        let verdict = aggregate_unit_verdict(&outcomes);
        assert_eq!(verdict, Verdict::RateFail);
        assert_eq!(
            outcome_matching_verdict(&outcomes, verdict)
                .unwrap()
                .reason_code,
            "SINGLE_UDP_STREAM_FAILED"
        );

        let cts_outcomes = vec![
            LegOutcome {
                verdict: Verdict::RateFail,
                reason_code: "CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED".into(),
                reason_detail: "AB exhausted three CTS attempts".into(),
                rx_avg: Some(700.0),
                main_rows: vec![],
                tag: "ab".into(),
            },
            LegOutcome {
                verdict: Verdict::NotEvaluated,
                reason_code: "TARGET_MISSING".into(),
                reason_detail: "BA measured independently".into(),
                rx_avg: Some(700.0),
                main_rows: vec![],
                tag: "ba".into(),
            },
        ];
        let verdict = aggregate_unit_verdict(&cts_outcomes);
        assert_eq!(verdict, Verdict::RateFail);
        assert_eq!(
            outcome_matching_verdict(&cts_outcomes, verdict)
                .unwrap()
                .reason_code,
            "CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED"
        );
    }

    #[test]
    fn preflight_block_marks_iperf_without_touching_ping_legs() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let iperf = IperfTask {
            v6: false,
            udp: false,
            profile_name: "tcp_w64k".into(),
            profile_label: "TCP -w 64k".into(),
            src: master,
            dst: agent,
            port: 56_000,
            duration: 1,
            extra: vec!["-w".into(), "64k".into()],
            stream_idx: 0,
            rate_mode: RateMode::Observe,
            rx_target_mbps: None,
            offered_mbps: None,
        };
        let unit = Unit {
            id: "blocked".into(),
            title: "blocked".into(),
            bidir: false,
            legs: vec![Leg {
                tag: "ab".into(),
                kind: LegKind::IperfSingle(iperf),
            }],
            est_secs: 1,
        };
        let block = IperfPreflightBlock {
            reason_code: "IPERF_PREFLIGHT_FAILED".into(),
            reason_detail: "两端缺少 iperf3".into(),
        };
        let outcomes = preflight_block_outcomes(&unit, &block);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].verdict, Verdict::SetupError);
        assert_eq!(outcomes[0].reason_code, "IPERF_PREFLIGHT_FAILED");
        assert_eq!(outcomes[0].tag, "ab");
        assert!(outcomes[0].main_rows.is_empty());
    }

    #[test]
    fn missing_ab_row_is_restored_without_duplicating_existing_ba_row() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let unit = Unit {
            id: "partial-bidir-tcp".into(),
            title: "partial bidirectional TCP".into(),
            bidir: true,
            legs: vec![
                Leg {
                    tag: "ab".into(),
                    kind: LegKind::IperfSingle(tcp_task(&master, &agent, 56_000)),
                },
                Leg {
                    tag: "ba".into(),
                    kind: LegKind::IperfSingle(tcp_task(&agent, &master, 56_001)),
                },
            ],
            est_secs: 20,
        };
        let (ctx, db_path) = isolated_ctx(0);
        let ba_row = ctx.push_row(Row {
            sort_key: (0, 1, 0, 0),
            task: unit.title.clone(),
            transport: "TCP".into(),
            kind_label: "★★双向灌包-ba".into(),
            verdict: Verdict::Pass,
            rx_avg: Some(500.0),
            ..Default::default()
        });
        let mut outcomes = vec![
            LegOutcome {
                verdict: Verdict::SetupError,
                reason_code: "LEG_THREAD_PANIC".into(),
                reason_detail: "ab 方向执行线程 panic: synthetic".into(),
                rx_avg: None,
                main_rows: vec![],
                tag: "ab".into(),
            },
            LegOutcome {
                verdict: Verdict::Pass,
                reason_code: String::new(),
                reason_detail: String::new(),
                rx_avg: Some(500.0),
                main_rows: vec![ba_row],
                tag: "ba".into(),
            },
        ];

        ctx.ensure_traffic_outcome_rows(0, &unit, &mut outcomes);
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].main_rows.len(), 1);
        assert_eq!(outcomes[1].main_rows, vec![ba_row]);
        let rows = ctx.rows.lock().unwrap();
        assert_eq!(rows.len(), 2);
        let ab = rows
            .iter()
            .find(|row| row.kind_label.ends_with("-ab"))
            .expect("restored AB detail row");
        assert_eq!(ab.reason_code, "LEG_THREAD_PANIC");
        assert_eq!(ab.src_ip, "192.168.1.2");
        assert_eq!(ab.dst_ip, "192.168.1.3");
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn unit_panic_is_expanded_to_both_direction_rows_without_generic_duplicate() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let unit = Unit {
            id: "panic-bidir-tcp".into(),
            title: "panic bidirectional TCP".into(),
            bidir: true,
            legs: vec![
                Leg {
                    tag: "ab".into(),
                    kind: LegKind::IperfSingle(tcp_task(&master, &agent, 56_000)),
                },
                Leg {
                    tag: "ba".into(),
                    kind: LegKind::IperfSingle(tcp_task(&agent, &master, 56_001)),
                },
            ],
            est_secs: 20,
        };
        let (ctx, db_path) = isolated_ctx(0);
        let mut outcomes = vec![LegOutcome {
            verdict: Verdict::SetupError,
            reason_code: "UNIT_PANIC".into(),
            reason_detail: "synthetic unit panic".into(),
            rx_avg: None,
            main_rows: vec![],
            tag: String::new(),
        }];

        ctx.ensure_traffic_outcome_rows(0, &unit, &mut outcomes);
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().any(|outcome| outcome.tag == "ab"));
        assert!(outcomes.iter().any(|outcome| outcome.tag == "ba"));
        assert!(outcomes
            .iter()
            .all(|outcome| outcome.reason_code == "UNIT_PANIC" && outcome.main_rows.len() == 1));
        let rows = ctx.rows.lock().unwrap();
        assert_eq!(rows.len(), 2);
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn unit_panic_reuses_a_committed_ab_row_and_only_fills_missing_ba() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let unit = Unit {
            id: "partial-row-then-panic".into(),
            title: "partial row then unit panic".into(),
            bidir: true,
            legs: vec![
                Leg {
                    tag: "ab".into(),
                    kind: LegKind::IperfSingle(tcp_task(&master, &agent, 56_000)),
                },
                Leg {
                    tag: "ba".into(),
                    kind: LegKind::IperfSingle(tcp_task(&agent, &master, 56_001)),
                },
            ],
            est_secs: 20,
        };
        let (ctx, db_path) = isolated_ctx(0);
        let ab_row = ctx.push_row(Row {
            sort_key: (0, 0, 0, 0),
            parent_id: unit.id.clone(),
            task: unit.title.clone(),
            transport: "TCP".into(),
            kind_label: "★★双向灌包-ab".into(),
            verdict: Verdict::Pass,
            rx_avg: Some(420.0),
            ..Default::default()
        });
        let mut outcomes = vec![LegOutcome {
            verdict: Verdict::SetupError,
            reason_code: "UNIT_PANIC".into(),
            reason_detail: "panic after AB row commit".into(),
            rx_avg: None,
            main_rows: vec![],
            tag: String::new(),
        }];

        ctx.ensure_traffic_outcome_rows(0, &unit, &mut outcomes);

        assert_eq!(outcomes.len(), 2);
        let ab = outcomes.iter().find(|outcome| outcome.tag == "ab").unwrap();
        let ba = outcomes.iter().find(|outcome| outcome.tag == "ba").unwrap();
        assert_eq!(ab.main_rows, vec![ab_row]);
        assert_eq!(ab.rx_avg, Some(420.0));
        assert_eq!(ba.main_rows.len(), 1);
        assert_eq!(ba.reason_code, "UNIT_PANIC");
        let rows = ctx.rows.lock().unwrap();
        assert_eq!(rows.len(), 2, "已有 AB 不能再被补成重复方向行");
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind_label.ends_with("-ab"))
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind_label.ends_with("-ba"))
                .count(),
            1
        );
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn bidirectional_preflight_keeps_both_ab_and_ba_detail_rows() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let unit = Unit {
            id: "blocked-bidir-tcp".into(),
            title: "blocked bidirectional TCP".into(),
            bidir: true,
            legs: vec![
                Leg {
                    tag: "ab".into(),
                    kind: LegKind::IperfSingle(tcp_task(&master, &agent, 56_000)),
                },
                Leg {
                    tag: "ba".into(),
                    kind: LegKind::IperfSingle(tcp_task(&agent, &master, 56_001)),
                },
            ],
            est_secs: 20,
        };
        let block = IperfPreflightBlock {
            reason_code: "IPERF_PREFLIGHT_FAILED".into(),
            reason_detail: "两端缺少 iperf3".into(),
        };
        let (ctx, db_path) = isolated_ctx(0);
        let summary = ctx.run_all_with_preflight(&[unit], Some(&block));
        assert_eq!(summary.setup_error, 1);

        let rows = ctx.rows.lock().unwrap();
        let detail_rows: Vec<_> = rows.iter().filter(|row| !row.is_unit_summary).collect();
        assert_eq!(detail_rows.len(), 2);
        assert!(detail_rows
            .iter()
            .all(|row| row.reason_code == "IPERF_PREFLIGHT_FAILED"));
        assert!(detail_rows
            .iter()
            .any(|row| row.src_ip == "192.168.1.2" && row.dst_ip == "192.168.1.3"));
        assert!(detail_rows
            .iter()
            .any(|row| row.src_ip == "192.168.1.3" && row.dst_ip == "192.168.1.2"));
        assert!(detail_rows
            .iter()
            .any(|row| row.kind_label.ends_with("-ab")));
        assert!(detail_rows
            .iter()
            .any(|row| row.kind_label.ends_with("-ba")));
        let unit_summary = rows.iter().find(|row| row.is_unit_summary).unwrap();
        assert!(detail_rows
            .iter()
            .all(|row| row.sort_key < unit_summary.sort_key));
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn ctstraffic_preflight_block_becomes_setup_error_and_triggers_diagnostics() {
        let unit = ctstraffic_unit("cts-blocked", true);
        let block = IperfPreflightBlock {
            reason_code: "CTSTRAFFIC_PREFLIGHT_FAILED".into(),
            reason_detail: "当前平台缺少 ctsTraffic".into(),
        };
        let outcomes = preflight_block_outcomes(&unit, &block);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].verdict, Verdict::SetupError);
        assert_eq!(outcomes[0].reason_code, "CTSTRAFFIC_PREFLIGHT_FAILED");
        assert_eq!(outcomes[0].tag, "ab");

        let (ctx, db_path) = isolated_ctx(0);
        let mut blocks = HashMap::new();
        blocks.insert(unit.id.clone(), block);
        let summary = ctx.run_all_with_preflight_blocks(&[unit], &blocks);
        assert_eq!(summary.setup_error, 1);
        assert_eq!(summary.traffic_units, 1);
        assert_eq!(summary.traffic_setup_errors, 1);
        assert_eq!(summary.traffic_usable_units, 0);
        assert!(summary.needs_traffic_failure_diagnostics());
        let rows = ctx.rows.lock().unwrap();
        let summary_row = rows
            .iter()
            .find(|row| row.is_unit_summary)
            .expect("blocked CTS unit summary row");
        assert_eq!(summary_row.verdict, Verdict::SetupError);
        assert_eq!(summary_row.reason_code, "CTSTRAFFIC_PREFLIGHT_FAILED");
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn ctstraffic_args_error_takes_priority_over_preflight_without_starting_agent() {
        let mut unit = ctstraffic_unit("cts-args-before-preflight", true);
        let LegKind::CtsTraffic(task) = &mut unit.legs[0].kind else {
            panic!("expect CTS task");
        };
        task.src = endpoint(Side::Agent, "agent0", "192.168.1.3");
        task.dst = endpoint(Side::Master, "master0", "192.168.1.2");
        task.setup_error = Some("builder rejected duration=0".into());

        let block = IperfPreflightBlock {
            reason_code: "CTSTRAFFIC_PREFLIGHT_FAILED".into(),
            reason_detail: "当前平台缺少 ctsTraffic".into(),
        };
        let (ctx, db_path) = isolated_ctx(0);
        let mut blocks = HashMap::new();
        blocks.insert(unit.id.clone(), block);
        let summary = ctx.run_all_with_preflight_blocks(&[unit], &blocks);
        assert_eq!(summary.setup_error, 1);

        let rows = ctx.rows.lock().unwrap();
        let detail_rows: Vec<_> = rows.iter().filter(|row| !row.is_unit_summary).collect();
        assert_eq!(detail_rows.len(), 1);
        assert_eq!(detail_rows[0].reason_code, "CTSTRAFFIC_ARGS_INVALID");
        assert_eq!(detail_rows[0].reason_detail, "builder rejected duration=0");
        let summary_row = rows.iter().find(|row| row.is_unit_summary).unwrap();
        assert_eq!(summary_row.reason_code, "CTSTRAFFIC_ARGS_INVALID");
        assert!(summary_row
            .reason_detail
            .contains("CTSTRAFFIC_ARGS_INVALID"));
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn ctstraffic_preflight_remains_per_leg_when_only_one_direction_has_args_error() {
        let mut invalid = ctstraffic_task(true);
        invalid.src = endpoint(Side::Agent, "agent0", "192.168.1.3");
        invalid.dst = endpoint(Side::Master, "master0", "192.168.1.2");
        invalid.setup_error = Some("invalid ab socket buffer".into());
        let mut normal = invalid.clone();
        normal.port += 1;
        normal.setup_error = None;
        let unit = Unit {
            id: "cts-mixed-args-preflight".into(),
            title: "CTS mixed args/preflight".into(),
            bidir: true,
            legs: vec![
                Leg {
                    tag: "ab".into(),
                    kind: LegKind::CtsTraffic(invalid),
                },
                Leg {
                    tag: "ba".into(),
                    kind: LegKind::CtsTraffic(normal),
                },
            ],
            est_secs: 1,
        };
        let block = IperfPreflightBlock {
            reason_code: "CTSTRAFFIC_PREFLIGHT_FAILED".into(),
            reason_detail: "当前平台缺少 ctsTraffic".into(),
        };
        let (ctx, db_path) = isolated_ctx(0);
        let mut blocks = HashMap::new();
        blocks.insert(unit.id.clone(), block);
        let summary = ctx.run_all_with_preflight_blocks(&[unit], &blocks);
        assert_eq!(summary.setup_error, 1);

        let rows = ctx.rows.lock().unwrap();
        let detail_rows: Vec<_> = rows.iter().filter(|row| !row.is_unit_summary).collect();
        assert_eq!(
            detail_rows.len(),
            2,
            "两个方向都必须保留明细，且正常方向仍必须停在 preflight"
        );
        assert!(detail_rows
            .iter()
            .any(|row| row.reason_code == "CTSTRAFFIC_ARGS_INVALID"
                && row.kind_label.ends_with("-ab")));
        assert!(detail_rows
            .iter()
            .any(|row| row.reason_code == "CTSTRAFFIC_PREFLIGHT_FAILED"
                && row.kind_label.ends_with("-ba")));
        assert!(detail_rows
            .iter()
            .all(|row| row.kind_label.contains("CTS Traffic")));
        let summary_row = rows.iter().find(|row| row.is_unit_summary).unwrap();
        assert_eq!(summary_row.reason_code, "CTSTRAFFIC_ARGS_INVALID");
        assert!(summary_row
            .reason_detail
            .contains("ab:CTSTRAFFIC_ARGS_INVALID"));
        assert!(summary_row
            .reason_detail
            .contains("ba:CTSTRAFFIC_PREFLIGHT_FAILED"));
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn ctstraffic_two_invalid_directions_keep_two_detail_rows_under_preflight() {
        let mut ab = ctstraffic_task(true);
        ab.setup_error = Some("invalid ab".into());
        let mut ba = ab.clone();
        ba.port += 1;
        ba.setup_error = Some("invalid ba".into());
        let unit = Unit {
            id: "cts-two-invalid-preflight".into(),
            title: "CTS two invalid directions".into(),
            bidir: true,
            legs: vec![
                Leg {
                    tag: "ab".into(),
                    kind: LegKind::CtsTraffic(ab),
                },
                Leg {
                    tag: "ba".into(),
                    kind: LegKind::CtsTraffic(ba),
                },
            ],
            est_secs: 1,
        };
        let block = IperfPreflightBlock {
            reason_code: "CTSTRAFFIC_PREFLIGHT_FAILED".into(),
            reason_detail: "当前平台缺少 ctsTraffic".into(),
        };
        let (ctx, db_path) = isolated_ctx(0);
        let mut blocks = HashMap::new();
        blocks.insert(unit.id.clone(), block);
        let summary = ctx.run_all_with_preflight_blocks(&[unit], &blocks);
        assert_eq!(summary.setup_error, 1);

        let rows = ctx.rows.lock().unwrap();
        let detail_rows: Vec<_> = rows.iter().filter(|row| !row.is_unit_summary).collect();
        assert_eq!(detail_rows.len(), 2);
        assert!(detail_rows
            .iter()
            .all(|row| row.reason_code == "CTSTRAFFIC_ARGS_INVALID"));
        let summary_row = rows.iter().find(|row| row.is_unit_summary).unwrap();
        assert_eq!(summary_row.reason_code, "CTSTRAFFIC_ARGS_INVALID");
        assert!(summary_row.reason_detail.contains("invalid ab"));
        assert!(summary_row.reason_detail.contains("invalid ba"));
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn resumed_ctstraffic_pass_counts_as_usable_traffic_measurement() {
        let unit = ctstraffic_unit("cts-resume-pass", false);
        let (mut ctx, db_path) = isolated_ctx(0);
        ctx.cfg.resume = true;
        {
            let mut db = ctx.db.lock().unwrap();
            db.set(&unit.id, true, &unit.title);
            db.save();
        }

        let summary = ctx.run_all_with_preflight_blocks(&[unit], &HashMap::new());
        assert_eq!(summary.skip, 1);
        assert_eq!(summary.traffic_units, 1);
        assert_eq!(summary.traffic_usable_units, 1);
        assert_eq!(summary.traffic_setup_errors, 0);
        assert!(!summary.needs_traffic_failure_diagnostics());
        let rows = ctx.rows.lock().unwrap();
        let skip = rows
            .iter()
            .find(|row| row.verdict == Verdict::Skip)
            .expect("CTS resume skip row");
        assert_eq!(skip.execution_status, ExecutionStatus::Skipped);
        assert_eq!(skip.reason_code, "RESUME_FRESH_PASS");
        assert!(skip.reason_detail.contains("正式 PASS"));
        assert!(skip.reason_detail.contains("resume"));
        assert!(skip.reason_detail.contains("24 小时"));
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn preflight_block_takes_priority_over_resume_pass() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let unit = Unit {
            id: "blocked-resume".into(),
            title: "blocked-resume".into(),
            bidir: false,
            legs: vec![Leg {
                tag: String::new(),
                kind: LegKind::IperfSingle(IperfTask {
                    v6: false,
                    udp: false,
                    profile_name: "tcp_w64k".into(),
                    profile_label: "TCP -w 64k".into(),
                    src: master,
                    dst: agent,
                    port: 56_000,
                    duration: 1,
                    extra: vec![],
                    stream_idx: 0,
                    rate_mode: RateMode::Observe,
                    rx_target_mbps: None,
                    offered_mbps: None,
                }),
            }],
            est_secs: 1,
        };
        let db_path = std::env::temp_dir().join(format!(
            "cpe_test_preflight_resume_{}_{}.json",
            std::process::id(),
            RESOURCE_OWNER_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let mut db = ResultDb::load(db_path.clone());
        db.set(&unit.id, true, &unit.title);
        db.save();
        let cfg = Config {
            resume: true,
            ..Default::default()
        };
        let ctx = Ctx {
            topology: None,
            agent_host: "127.0.0.1".into(),
            agent_port: 1,
            cfg,
            outdir: std::env::temp_dir(),
            transport: Arc::new(http_client::TcpTransport),
            clock: Arc::new(SystemClock),
            local_servers: IperfServerMgr::new(),
            local_cts_jobs: IperfClientJobMgr::new(),
            local_monitors: MonitorMgr::new(),
            rows: Mutex::new(Vec::new()),
            db: Mutex::new(ResultDb::load(db_path.clone())),
        };
        let block = IperfPreflightBlock {
            reason_code: "IPERF_PREFLIGHT_FAILED".into(),
            reason_detail: "缺少 iperf3".into(),
        };
        let summary = ctx.run_all_with_preflight(&[unit], Some(&block));
        assert_eq!(summary.skip, 0);
        assert_eq!(summary.setup_error, 1);
        assert_eq!(summary.traffic_units, 1);
        assert_eq!(summary.traffic_usable_units, 0);
        assert!(summary.needs_traffic_failure_diagnostics());
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn successful_ping_records_reason_and_all_rtt_metrics() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let responder = std::thread::spawn(move || {
            let request = server
                .incoming_requests()
                .next()
                .expect("receive agent ping request");
            assert_eq!(request.url(), "/ping");
            let raw = r#"PING 192.168.1.2 (192.168.1.2): 56 data bytes
64 bytes from 192.168.1.2: icmp_seq=0 ttl=64 time=1.250 ms
64 bytes from 192.168.1.2: icmp_seq=1 ttl=64 time=2.500 ms
64 bytes from 192.168.1.2: icmp_seq=2 ttl=64 time=3.750 ms

--- 192.168.1.2 ping statistics ---
3 packets transmitted, 3 packets received, 0.0% packet loss
round-trip min/avg/max/stddev = 1.250/2.500/3.750/1.021 ms
"#;
            let response = tiny_http::Response::from_string(ok_json(PingOut {
                ok: true,
                sent: 3,
                received: 3,
                lost: 0,
                loss_pct: 0.0,
                rtt_min: Some(1.25),
                rtt_avg: Some(2.5),
                rtt_max: Some(3.75),
                cmd: "ping -c 3 192.168.1.2".into(),
                raw: raw.into(),
            }));
            request.respond(response).expect("respond to agent ping");
        });
        let unit = Unit {
            id: "agent-ping-success".into(),
            title: "PING V4 -l 1400 n=3".into(),
            bidir: false,
            legs: vec![Leg {
                tag: String::new(),
                kind: LegKind::Ping(PingTask {
                    v6: false,
                    src: endpoint(Side::Agent, "agent0", "192.168.1.3"),
                    dst: endpoint(Side::Master, "master0", "192.168.1.2"),
                    count: 3,
                    payload: 1400,
                    purpose: PingPurpose::SubnetTest,
                }),
            }],
            est_secs: 1,
        };
        let (ctx, db_path) = isolated_ctx(port);

        let summary = ctx.run_all_with_preflight(&[unit], None);

        assert_eq!(summary.pass, 1);
        responder.join().expect("agent ping responder");
        let rows = ctx.rows.lock().unwrap();
        let detail = rows.iter().find(|row| !row.is_unit_summary).unwrap();
        assert_eq!(detail.verdict, Verdict::Pass);
        assert_eq!(detail.execution_status, ExecutionStatus::Completed);
        assert_eq!(detail.reason_code, "PING_OK");
        assert!(detail.reason_detail.contains("发送/接收=3/3"));
        assert!(detail.reason_detail.contains("丢包率 0.0%"));
        assert!(detail
            .reason_detail
            .contains("RTT 最小/平均/最大=1.250/2.500/3.750 ms"));
        assert_eq!(detail.ping_loss, Some(0.0));
        assert_eq!(detail.ping_min, Some(1.25));
        assert_eq!(detail.ping_avg, Some(2.5));
        assert_eq!(detail.ping_max, Some(3.75));

        let unit_summary = rows.iter().find(|row| row.is_unit_summary).unwrap();
        assert_eq!(unit_summary.reason_code, "PING_OK");
        assert!(unit_summary.reason_detail.contains("PING_OK"));
        assert!(unit_summary.reason_detail.contains("发送/接收=3/3"));
        assert_eq!(unit_summary.ping_min, Some(1.25));
        assert_eq!(unit_summary.ping_avg, Some(2.5));
        assert_eq!(unit_summary.ping_max, Some(3.75));
        assert_eq!(unit_summary.direction_summaries.len(), 1);
        assert_eq!(unit_summary.direction_summaries[0].ping_min, Some(1.25));
        assert_eq!(unit_summary.direction_summaries[0].ping_avg, Some(2.5));
        assert_eq!(unit_summary.direction_summaries[0].ping_max, Some(3.75));
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn missing_gateway_is_not_reported_as_network_packet_loss() {
        let src = endpoint(Side::Master, "eth0", "192.168.1.2");
        let dst = Endpoint {
            side: Side::Master,
            pc: "主控".into(),
            nic: NicInfo {
                name: "eth0 的 IPv4 网关".into(),
                role: "GATEWAY".into(),
                ipv4: String::new(),
                ..Default::default()
            },
        };
        let unit = Unit {
            id: "gateway-missing".into(),
            title: "gateway-missing".into(),
            bidir: false,
            legs: vec![Leg {
                tag: "gateway-diagnostic".into(),
                kind: LegKind::Ping(PingTask {
                    v6: false,
                    src,
                    dst,
                    count: 3,
                    payload: 32,
                    purpose: PingPurpose::GatewayDiagnostic,
                }),
            }],
            est_secs: 1,
        };
        let (ctx, db_path) = isolated_ctx(0);
        let summary = ctx.run_all_with_preflight(&[unit], None);
        assert_eq!(summary.not_evaluated, 1);
        assert_eq!(summary.setup_error, 0);
        let rows = ctx.rows.lock().unwrap();
        let detail = rows.iter().find(|row| !row.is_unit_summary).unwrap();
        assert_eq!(detail.verdict, Verdict::NotEvaluated);
        assert_eq!(detail.execution_status, ExecutionStatus::Partial);
        assert_eq!(detail.reason_code, "GATEWAY_NOT_FOUND");
        assert_eq!(detail.ping_loss, None);
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn agent_ping_http_failure_is_setup_error_not_one_hundred_percent_loss() {
        let unit = Unit {
            id: "agent-ping-http-error".into(),
            title: "agent-ping-http-error".into(),
            bidir: false,
            legs: vec![Leg {
                tag: String::new(),
                kind: LegKind::Ping(PingTask {
                    v6: false,
                    src: endpoint(Side::Agent, "agent0", "192.168.1.3"),
                    dst: endpoint(Side::Master, "master0", "192.168.1.2"),
                    count: 1,
                    payload: 32,
                    purpose: PingPurpose::SubnetDiagnostic,
                }),
            }],
            est_secs: 1,
        };
        let (ctx, db_path) = isolated_ctx(0);
        let summary = ctx.run_all_with_preflight(&[unit], None);
        assert_eq!(summary.setup_error, 1);
        let rows = ctx.rows.lock().unwrap();
        let detail = rows.iter().find(|row| !row.is_unit_summary).unwrap();
        assert_eq!(detail.verdict, Verdict::SetupError);
        assert_eq!(detail.execution_status, ExecutionStatus::Error);
        assert_eq!(detail.reason_code, "PING_EXEC_ERROR");
        assert_eq!(detail.ping_loss, None);
        assert!(detail.reason_detail.contains("辅测机 /ping 调用失败"));
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn mixed_preflight_failure_still_runs_independent_ping_unit() {
        let iperf_unit = Unit {
            id: "mixed-iperf".into(),
            title: "mixed-iperf".into(),
            bidir: false,
            legs: vec![Leg {
                tag: String::new(),
                kind: LegKind::IperfSingle(IperfTask {
                    v6: false,
                    udp: false,
                    profile_name: "tcp".into(),
                    profile_label: "TCP".into(),
                    src: endpoint(Side::Master, "master0", "192.168.1.2"),
                    dst: endpoint(Side::Agent, "agent0", "192.168.1.3"),
                    port: 56_000,
                    duration: 1,
                    extra: vec![],
                    stream_idx: 0,
                    rate_mode: RateMode::Observe,
                    rx_target_mbps: None,
                    offered_mbps: None,
                }),
            }],
            est_secs: 1,
        };
        let ping_unit = Unit {
            id: "mixed-ping".into(),
            title: "mixed-ping".into(),
            bidir: false,
            legs: vec![Leg {
                tag: "gateway-diagnostic".into(),
                kind: LegKind::Ping(PingTask {
                    v6: false,
                    src: endpoint(Side::Master, "master0", "192.168.1.2"),
                    dst: Endpoint {
                        side: Side::Master,
                        pc: "主控".into(),
                        nic: NicInfo {
                            name: "网关".into(),
                            role: "GATEWAY".into(),
                            ipv4: String::new(),
                            ..Default::default()
                        },
                    },
                    count: 3,
                    payload: 32,
                    purpose: PingPurpose::GatewayDiagnostic,
                }),
            }],
            est_secs: 1,
        };
        let block = IperfPreflightBlock {
            reason_code: "IPERF_PREFLIGHT_FAILED".into(),
            reason_detail: "缺少 iperf3".into(),
        };
        let (ctx, db_path) = isolated_ctx(0);
        let summary = ctx.run_all_with_preflight(&[iperf_unit, ping_unit], Some(&block));
        assert_eq!(summary.setup_error, 1);
        assert_eq!(summary.not_evaluated, 1);
        assert_eq!(summary.traffic_units, 1);
        let rows = ctx.rows.lock().unwrap();
        assert!(rows
            .iter()
            .any(|row| row.reason_code == "IPERF_PREFLIGHT_FAILED"));
        assert!(rows
            .iter()
            .any(|row| row.reason_code == "GATEWAY_NOT_FOUND"));
        drop(rows);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn diagnostics_trigger_only_when_every_traffic_unit_has_no_measurement() {
        let mut summary = RunSummary {
            traffic_units: 3,
            traffic_setup_errors: 3,
            ..Default::default()
        };
        assert!(summary.needs_traffic_failure_diagnostics());

        summary.traffic_usable_units = 1;
        assert!(!summary.needs_traffic_failure_diagnostics());

        let ping_only = RunSummary::default();
        assert!(!ping_only.needs_traffic_failure_diagnostics());
    }

    #[test]
    fn usable_traffic_measurement_requires_real_rate_or_active_stream() {
        assert!(!row_has_usable_traffic_measurement(&Row::default()));
        assert!(!row_has_usable_traffic_measurement(&Row {
            rx_mbps: Some(0.0),
            ..Default::default()
        }));
        assert!(!row_has_usable_traffic_measurement(&Row {
            verdict: Verdict::SetupError,
            execution_status: ExecutionStatus::Error,
            rx_avg: Some(500.0),
            active_streams: 1,
            ..Default::default()
        }));
        assert!(row_has_usable_traffic_measurement(&Row {
            rx_mbps: Some(100.0),
            ..Default::default()
        }));
        assert!(row_has_usable_traffic_measurement(&Row {
            active_streams: 1,
            ..Default::default()
        }));
        assert!(!row_has_usable_traffic_measurement(&Row {
            transport: "CTS/UDP".into(),
            verdict: Verdict::RateFail,
            execution_status: ExecutionStatus::Completed,
            rx_avg: Some(900.0),
            reason_code: "CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED".into(),
            ..Default::default()
        }));
        assert!(!row_has_usable_traffic_measurement(&Row {
            transport: "CTS/UDP".into(),
            verdict: Verdict::NotEvaluated,
            execution_status: ExecutionStatus::Partial,
            rx_avg: Some(900.0),
            ..Default::default()
        }));
        assert!(!row_has_usable_traffic_measurement(&Row {
            transport: "UDP".into(),
            verdict: Verdict::RateFail,
            execution_status: ExecutionStatus::Completed,
            rx_avg: Some(900.0),
            reason_code: "SINGLE_UDP_STREAM_FAILED".into(),
            ..Default::default()
        }));
    }

    #[test]
    fn ctstraffic_row_is_counted_as_a_usable_traffic_measurement() {
        let (ctx, db_path) = isolated_ctx(0);
        let row_index = ctx.push_row(Row {
            transport: "CTS/UDP".into(),
            verdict: Verdict::Measured,
            execution_status: ExecutionStatus::Completed,
            rx_mbps: Some(1_420.0),
            active_streams: 3,
            requested_streams: 3,
            ..Default::default()
        });
        let outcomes = vec![LegOutcome {
            verdict: Verdict::Measured,
            reason_code: "TARGET_UNKNOWN".into(),
            reason_detail: String::new(),
            rx_avg: None,
            main_rows: vec![row_index],
            tag: "ab".into(),
        }];

        assert!(ctx.outcomes_have_usable_traffic_measurement(&outcomes));
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn run_summary_merge_keeps_traffic_diagnostic_counters() {
        let mut left = RunSummary {
            pass: 1,
            traffic_units: 2,
            traffic_usable_units: 0,
            traffic_setup_errors: 2,
            ..Default::default()
        };
        left.merge(RunSummary {
            fail: 1,
            not_evaluated: 1,
            ..Default::default()
        });
        assert_eq!(left.pass, 1);
        assert_eq!(left.fail, 1);
        assert_eq!(left.not_evaluated, 1);
        assert_eq!(left.traffic_units, 2);
        assert_eq!(left.traffic_setup_errors, 2);
        assert!(left.needs_traffic_failure_diagnostics());
    }

    #[test]
    fn test_text_preview_is_utf8_safe() {
        assert_eq!(text_preview("截图失败：权限不足", 4), "截图失败");
        assert_eq!(text_preview("short", 100), "short");
    }

    #[test]
    fn progress_line_uses_nic_rate_and_only_active_iperf_rates() {
        let line = format_iperf_progress(&IperfProgressSnapshot {
            protocol: "TCP",
            tag: "ab",
            active: 1,
            total: 1,
            connected: 1,
            ended: 0,
            nic_rx_mbps: Some(2368.4),
            iperf_mbps: Some(2379.0),
            errors: 0,
            monitor_error: String::new(),
        });
        assert!(line.contains("[灌包进度][TCP][ab]"));
        assert!(line.contains("nic-rx=2368.4Mbps"));
        assert!(line.contains("iperf=2379.0Mbps"));

        // 双向两腿并行输出重试日志，缺了方向前缀就无法把 attempt/retry 归到
        // AB 还是 BA —— master.log 里两条 #1 会完全分不开。
        assert_eq!(fmt_tag_bracket("ab"), "[ab]");
        assert_eq!(fmt_tag_bracket("ba"), "[ba]");
        assert_eq!(fmt_tag_bracket(""), "");

        let mut state = LiveFlowState::default();
        apply_flow_event(
            &mut state,
            &IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                mbps: Some(500.0),
                ..Default::default()
            },
        );
        assert_eq!(active_iperf_rate(&state), Some(500.0));
        apply_flow_event(
            &mut state,
            &IperfFlowEvent {
                kind: IperfEventKind::Ended,
                ..Default::default()
            },
        );
        assert_eq!(active_iperf_rate(&state), None);
    }

    #[test]
    fn tcp_parallel_progress_uses_sum_and_ignores_final_summary() {
        assert!(is_live_progress_rate_line(
            "[SUM]   0.00-1.00 sec  280 MBytes  2348 Mbits/sec",
            5
        ));
        assert!(!is_live_progress_rate_line(
            "[  5]   0.00-1.00 sec  56 MBytes  470 Mbits/sec",
            5
        ));
        assert!(!is_live_progress_rate_line(
            "[SUM]   0.00-180.00 sec  50 GBytes  2379 Mbits/sec sender",
            5
        ));
        assert!(is_live_progress_rate_line(
            "[  5]   0.00-1.00 sec  56 MBytes  470 Mbits/sec",
            1
        ));
    }

    #[test]
    fn raw_iperf_record_contains_both_sides_events_and_error() {
        let master = endpoint(Side::Master, "master0", "192.168.1.2");
        let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
        let task = IperfTask {
            v6: false,
            udp: false,
            profile_name: "tcp_w1m_P5".into(),
            profile_label: "TCP -w 1m -P 5".into(),
            src: master,
            dst: agent,
            port: 56_000,
            duration: 180,
            extra: vec!["-P".into(), "5".into()],
            stream_idx: 0,
            rate_mode: RateMode::Observe,
            rx_target_mbps: None,
            offered_mbps: None,
        };
        let client = IperfClientOut {
            cmd: "iperf3 -c 192.168.1.3".into(),
            output: "CLIENT RAW".into(),
            ..Default::default()
        };
        let events = vec![IperfFlowEvent {
            kind: IperfEventKind::Traffic,
            elapsed_ms: 1_000,
            mbps: Some(123.0),
            line: "EVENT RAW".into(),
        }];
        let text = build_iperf_raw_record(&task, &client, "SERVER RAW", &events, "sample error");
        assert!(text.contains("CLIENT RAW"));
        assert!(text.contains("SERVER RAW"));
        assert!(text.contains("EVENT RAW"));
        assert!(text.contains("sample error"));

        let filename = raw_iperf_filename("unit:1", 2, 3, "ab", &task);
        assert!(filename.ends_with(".log"));
        assert!(!filename.contains(':'));
        assert!(filename.contains("tcp"));
        assert!(filename.contains("p56000"));
    }

    #[test]
    fn nested_run_artifact_keeps_report_relative_link() {
        let nonce = RESOURCE_OWNER_SEQ.fetch_add(1, Ordering::SeqCst);
        let run_dir = std::env::temp_dir().join(format!(
            "cpe_run_artifact_test_{}_{}",
            std::process::id(),
            nonce
        ));
        let outdir = run_dir.join("iperf_outputs");
        let (mut ctx, db_path) = isolated_ctx(0);
        ctx.outdir = outdir.clone();

        let link = ctx.write_output_artifact("artifact.log", "artifact", "测试附件");

        assert_eq!(link, "./iperf_outputs/artifact.log");
        assert_eq!(
            std::fs::read_to_string(outdir.join("artifact.log")).unwrap(),
            "artifact"
        );
        let _ = std::fs::remove_dir_all(run_dir);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn ctstraffic_raw_record_contains_server_client_events_and_error() {
        let nonce = RESOURCE_OWNER_SEQ.fetch_add(1, Ordering::SeqCst);
        let outdir =
            std::env::temp_dir().join(format!("cpe_test_cts_raw_{}_{}", std::process::id(), nonce));
        let (mut ctx, db_path) = isolated_ctx(0);
        ctx.outdir = outdir.clone();
        let task = ctstraffic_task(true);
        let event = IperfFlowEvent {
            kind: IperfEventKind::Traffic,
            elapsed_ms: 1_000,
            mbps: Some(1_500.0),
            line: "EVENT RAW".into(),
        };
        let mut first = ctstraffic_attempt(0, false);
        first.client.output = "CLIENT RAW 1".into();
        first.server_output = "SERVER RAW 1".into();
        first.events = vec![event.clone()];
        first.setup_error = Some(("ATTEMPT_ONE".into(), "attempt-one-error".into()));
        first.full_attempt = false;
        let mut second = ctstraffic_attempt(1, false);
        second.client.output = "CLIENT RAW 2".into();
        second.server_output = "SERVER RAW 2".into();
        let mut third = ctstraffic_attempt(2, true);
        third.client.output = "CLIENT RAW 3".into();
        third.server_output = "SERVER RAW 3".into();
        third.events = vec![event];
        let attempts = vec![first, second, third];
        let link = ctx.save_ctstraffic_raw_record(
            "cts:raw-owner",
            0,
            "ab",
            &task,
            "ctsTraffic.exe -Listen:192.168.1.2",
            &attempts,
            "sample error",
        );
        assert!(!link.is_empty());
        let file = std::fs::read_dir(&outdir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "log"))
            .expect("CTS raw log");
        let text = std::fs::read_to_string(file).unwrap();
        assert!(text.contains("SERVER RAW 1"));
        assert!(text.contains("SERVER RAW 2"));
        assert!(text.contains("SERVER RAW 3"));
        assert!(text.contains("CLIENT RAW 1"));
        assert!(text.contains("CLIENT RAW 2"));
        assert!(text.contains("CLIENT RAW 3"));
        assert!(text.contains("EVENT RAW"));
        assert!(text.contains("sample error"));
        assert!(text.contains("UDP MediaStream"));
        assert!(text.contains("=== attempt 1 ==="));
        assert!(text.contains("=== attempt 2 ==="));
        assert!(text.contains("=== attempt 3 ==="));
        let attempt_1 = text.find("=== attempt 1 ===").unwrap();
        let attempt_2 = text.find("=== attempt 2 ===").unwrap();
        let attempt_3 = text.find("=== attempt 3 ===").unwrap();
        assert!(attempt_1 < attempt_2 && attempt_2 < attempt_3);
        assert!(text[attempt_1..attempt_2].contains("attempt-one-error"));
        assert!(!text[attempt_2..attempt_3].contains("attempt-one-error"));

        let _ = std::fs::remove_dir_all(outdir);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn nic_sample_csv_keeps_counter_deltas_rates_validity_and_errors() {
        let out = MonitorStopOut {
            avg_mbps: 100.0,
            tx_avg_mbps: 90.0,
            seconds: 1.0,
            bytes: 12_500_000,
            tx_bytes: 11_250_000,
            samples: vec![MonitorSample {
                elapsed_ms: 1_000,
                interval_ms: 1_000,
                rx_bytes: 1_012_500_000,
                tx_bytes: 2_011_250_000,
                rx_delta_bytes: 12_500_000,
                tx_delta_bytes: 11_250_000,
                rx_mbps: 100.0,
                tx_mbps: 90.0,
                valid: false,
                error: "counter reset".into(),
            }],
            errors: vec!["counter reset".into()],
        };
        let csv = build_monitor_samples_csv("agent", "Ethernet 2", 137, &out);
        // 零点估计是 [0, latest_start] 的中点，所以不确定度半宽等于偏移本身；
        // 共同窗口卡在边界时，靠这两行才能判断是真够还是对齐误差凑够的。
        assert!(csv.contains("# origin_offset_ms,137"));
        assert!(csv.contains("# origin_uncertainty_half_width_ms,137"));
        assert!(csv.contains("elapsed_ms,interval_ms,rx_bytes,tx_bytes"));
        assert!(csv.contains("1000,1000,1012500000,2011250000,12500000,11250000,100.000000,90.000000,false,counter reset"));
        assert!(csv.contains("# endpoint,agent"));
        assert!(csv.contains("# interface,Ethernet 2"));
        assert!(csv.contains("# full_lifecycle_seconds,1.000000"));
        assert!(csv.contains("# full_lifecycle_average_rx_mbps,100.000000"));
        assert!(csv.contains("# full_lifecycle_average_tx_mbps,90.000000"));
        assert!(!csv.contains("\n# average_rx_mbps,"));
    }

    /// UDP 路径必须和 TCP 路径同一口径：平均和 P10 都达标，但中间掉过坑，
    /// 一样是 FAIL。两条链的结论分叉过一次（D2），不能再分叉第二次。
    #[test]
    fn a_dropout_fails_the_same_way_on_both_transports() {
        let target = 800.0;
        let steady: Vec<(u64, f64)> = (1..=35).map(|i| (i * 5_000, 850.0)).collect();
        let dipped: Vec<(u64, f64)> = (1..=35)
            .map(|i| (i * 5_000, if i == 20 { 120.0 } else { 850.0 }))
            .collect();

        // TCP 路径
        let tx = healthy_stats(900.0);
        let pass = RateStats {
            rolling_series: steady.clone(),
            ..healthy_stats(850.0)
        };
        let (verdict, _, _) = evaluate_nic_rx(RateMode::Verify, Some(target), &pass, &tx);
        assert_eq!(verdict, Verdict::Pass, "全程稳定应当 PASS");

        let fails = RateStats {
            rolling_series: dipped.clone(),
            ..healthy_stats(850.0)
        };
        let (verdict, code, _) = evaluate_nic_rx(RateMode::Verify, Some(target), &fails, &tx);
        assert_eq!((verdict, code.as_str()), (Verdict::RateFail, "RX_DROPOUT"));

        // UDP 路径用的是同一个 rx_dropout 谓词，直接验证它在同样输入上给出
        // 同样的结论——两处判定链各自成文，共用的必须是这一个事实来源。
        assert!(rx_dropout(&steady, target).is_none());
        let dropout = rx_dropout(&dipped, target).expect("UDP 侧也要检出同一个坑");
        assert_eq!(dropout.windows, 1);
        assert_eq!(dropout.lowest_mbps, 120.0);
    }
}
