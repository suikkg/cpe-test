//! 任务执行器：调度本地/远端的 ping、iperf、监控、截图，产出报告行

use crate::cmd::iperf::{self, IperfServerMgr};
use crate::config::{Config, RateCheckCfg, RateMode};
use crate::http_client;
use crate::master::builder::{
    v6_addrs, IperfTask, Leg, LegKind, PingPurpose, PingTask, Side, Unit,
};
use crate::nic::monitor::{MonitorMgr, MIN_VALID_RX_MBPS};
use crate::ping;
use crate::protocol::*;
use crate::report::{ExecutionStatus, Row, Verdict};
use crate::util::{find_iperf3, logln, md5_hex, now_compact, now_full, sanitize};
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
const MIN_RATE_SAMPLE_COVERAGE: f64 = 0.95;
const ROLLING_RATE_WINDOW_MS: u64 = 5_000;
const ROLLING_COVERAGE_TOLERANCE_MS: u64 = 50;
const FLOW_TIMELINE_TOLERANCE_MS: u64 = 2_000;
const RESOURCE_LEASE_GRACE_SECS: u64 = 300;
const RELIABLE_HTTP_ATTEMPTS: usize = 3;
const RELIABLE_HTTP_RETRY_DELAY: Duration = Duration::from_millis(250);
const RESOURCE_CLEANUP_WAIT_SECS: u64 = 10;
static RESOURCE_OWNER_SEQ: AtomicU64 = AtomicU64::new(1);

pub struct Ctx {
    pub agent_host: String,
    pub agent_port: u16,
    pub cfg: Config,
    pub outdir: PathBuf,
    pub local_servers: IperfServerMgr,
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

fn unit_uses_agent_resources(unit: &Unit) -> bool {
    unit.legs.iter().any(|leg| match &leg.kind {
        LegKind::IperfSingle(task) => task.src.side == Side::Agent || task.dst.side == Side::Agent,
        LegKind::IperfGroup { streams, .. } => streams
            .iter()
            .any(|task| task.src.side == Side::Agent || task.dst.side == Side::Agent),
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

#[derive(Debug, Default, Clone)]
pub struct RunSummary {
    pub pass: usize,
    pub fail: usize,
    pub measured: usize,
    pub not_evaluated: usize,
    pub setup_error: usize,
    pub unstable: usize,
    pub skip: usize,
    /// 本轮选择并处理的 iperf 测试单元数（包括被前置检查拦截的单元）。
    pub iperf_units: usize,
    /// 至少产生一项有效 iperf/NIC 速率测量的 iperf 单元数。
    pub iperf_usable_units: usize,
    /// 最终判为 SETUP_ERROR 的 iperf 单元数。
    pub iperf_setup_errors: usize,
}

impl RunSummary {
    pub fn merge(&mut self, other: RunSummary) {
        self.pass += other.pass;
        self.fail += other.fail;
        self.measured += other.measured;
        self.not_evaluated += other.not_evaluated;
        self.setup_error += other.setup_error;
        self.unstable += other.unstable;
        self.skip += other.skip;
        self.iperf_units += other.iperf_units;
        self.iperf_usable_units += other.iperf_usable_units;
        self.iperf_setup_errors += other.iperf_setup_errors;
    }

    /// 只要本轮确实选择了 iperf，但一项有效速率测量都没有，就需要追加
    /// 子网 Ping 与网卡到网关 Ping，区分网络/载体异常和 iperf 搭建异常。
    pub fn needs_iperf_failure_diagnostics(&self) -> bool {
        self.iperf_units > 0 && self.iperf_usable_units == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IperfPreflightBlock {
    pub reason_code: String,
    pub reason_detail: String,
}

struct LegOutcome {
    verdict: Verdict,
    reason_code: String,
    reason_detail: String,
    rx_avg: Option<f64>,
    main_rows: Vec<usize>,
    tag: String,
}

fn preflight_block_outcomes(unit: &Unit, block: &IperfPreflightBlock) -> Vec<LegOutcome> {
    let mut outcomes: Vec<LegOutcome> = unit
        .legs
        .iter()
        .filter_map(|leg| match &leg.kind {
            LegKind::IperfSingle(_) | LegKind::IperfGroup { .. } => Some(LegOutcome {
                verdict: Verdict::SetupError,
                reason_code: block.reason_code.clone(),
                reason_detail: block.reason_detail.clone(),
                rx_avg: None,
                main_rows: Vec::new(),
                tag: leg.tag.clone(),
            }),
            LegKind::Ping(_) => None,
        })
        .collect();
    if outcomes.is_empty() {
        outcomes.push(LegOutcome {
            verdict: Verdict::SetupError,
            reason_code: block.reason_code.clone(),
            reason_detail: block.reason_detail.clone(),
            rx_avg: None,
            main_rows: Vec::new(),
            tag: String::new(),
        });
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
    task: IperfTask,
    server_req: Option<IperfServerStartReq>,
    client_req: Option<IperfClientReq>,
    server_error: String,
    launch_delay_ms: u64,
}

struct UdpFlowRun {
    leg_pos: usize,
    stream_pos: usize,
    task: IperfTask,
    raw_ok: bool,
    parsed: iperf::IperfParsed,
    client: IperfClientOut,
    server_output: String,
    events: Vec<IperfFlowEvent>,
    retries: usize,
    error: String,
}

#[derive(Debug, Clone, Default)]
struct RateStats {
    avg_mbps: Option<f64>,
    p10_mbps: Option<f64>,
    median_mbps: Option<f64>,
    p95_mbps: Option<f64>,
    min_mbps: Option<f64>,
    max_mbps: Option<f64>,
    coverage: f64,
    /// 实际可形成的完整 5 秒滚动窗口占理论窗口数的比例。
    ///
    /// 总采样覆盖率高并不代表稳定性窗口也完整：一次跨越多个失败周期的
    /// 恢复样本可以补齐平均速率覆盖，却不能证明其中任意 5 秒都稳定。
    rolling_coverage: f64,
}

#[derive(Debug, Clone, Default)]
struct EffectiveWindow {
    start_ms: u64,
    end_ms: u64,
    available_secs: f64,
    required_secs: u64,
    complete: bool,
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
        let (status, text) =
            http_client::post_json(&self.agent_host, self.agent_port, path, &body, timeout)
                .map_err(|e| format!("辅测机 {path} 调用失败: {e}"))?;
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
        let mut errors = Vec::new();
        for attempt in 1..=RELIABLE_HTTP_ATTEMPTS {
            match self.agent_post(path, req, timeout) {
                Ok(out) => return Ok(out),
                Err(e) => {
                    errors.push(format!("第{attempt}次: {e}"));
                    if attempt < RELIABLE_HTTP_ATTEMPTS {
                        std::thread::sleep(RELIABLE_HTTP_RETRY_DELAY);
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
                        cmd: String::new(),
                        output: "主控机未找到 iperf3，请把 iperf3 放到程序同目录".into(),
                        ..Default::default()
                    };
                };
                iperf::run_client_controlled(
                    &bin,
                    req,
                    None,
                    |line| {
                        if line.to_lowercase().contains("error") {
                            logln(&format!("      {line}"));
                        }
                    },
                    &mut on_event,
                )
            }
            Side::Agent => {
                let start_call = Instant::now();
                let start_req = IperfClientStartReq {
                    request: req.clone(),
                    request_id: request_id.to_string(),
                    owner_id: owner_id.to_string(),
                    lease_secs,
                };
                let started: IperfClientStartOut = match self.agent_post_reliable(
                    "/iperf/client/start",
                    &start_req,
                    Duration::from_secs(20),
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        let cleanup = self.client_stop_confirmed(request_id);
                        return IperfClientOut {
                            cancelled: cleanup.is_err(),
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
                    let _ = self.client_stop_confirmed(&started.id);
                    let _ = self.client_stop_confirmed(request_id);
                    return IperfClientOut {
                        cancelled: true,
                        output: format!(
                            "远端 client 返回了非预期 job id：期望 {request_id}，实际 {}",
                            started.id
                        ),
                        ..Default::default()
                    };
                }
                let response_elapsed_ms = start_call.elapsed().as_millis() as u64;
                let remote_origin_ms = if started.elapsed_ms > 0 {
                    response_elapsed_ms.saturating_sub(started.elapsed_ms)
                } else {
                    response_elapsed_ms / 2
                };
                let max_remote_secs = req.duration.saturating_add(180);
                let Some(deadline) =
                    std::time::Instant::now().checked_add(Duration::from_secs(max_remote_secs))
                else {
                    let cleanup = self.client_stop_confirmed(&started.id);
                    return IperfClientOut {
                        cancelled: cleanup.is_err(),
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
                    if std::time::Instant::now() >= deadline {
                        let cleanup = self.client_stop_confirmed(&started.id);
                        return IperfClientOut {
                            timed_out: true,
                            output: format!(
                                "(远端异步作业 {} 超过 {} 秒仍未结束；停止确认: {})",
                                started.id,
                                max_remote_secs,
                                cleanup.map(|_| "成功".to_string()).unwrap_or_else(|e| e)
                            ),
                            ..Default::default()
                        };
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
                            return IperfClientOut {
                                cancelled: cleanup.is_err(),
                                output: format!(
                                    "(远端异步作业查询失败: {e}; 停止确认: {})",
                                    cleanup
                                        .map(|_| "成功".to_string())
                                        .unwrap_or_else(|cleanup_error| cleanup_error)
                                ),
                                ..Default::default()
                            };
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
                        let mut result = status.result.unwrap_or_else(|| IperfClientOut {
                            output: format!("(远端异步作业 {} 已结束但缺少结果)", started.id),
                            ..Default::default()
                        });
                        if let Err(e) = self.client_stop_confirmed(&started.id) {
                            result.ok = false;
                            result.cancelled = true;
                            if !result.output.ends_with('\n') && !result.output.is_empty() {
                                result.output.push('\n');
                            }
                            result
                                .output
                                .push_str(&format!("远端 client 结束后清理未确认: {e}"));
                        }
                        return result;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }

    fn mon_start(
        &self,
        side: Side,
        iface: &str,
        owner_id: &str,
        lease_secs: u64,
    ) -> Result<String, String> {
        let interval_ms = self
            .cfg
            .iperf
            .rate_check
            .sample_interval_ms
            .clamp(200, 5_000);
        match side {
            Side::Master => {
                self.local_monitors
                    .start_owned(iface, interval_ms, owner_id, lease_secs)
            }
            Side::Agent => self
                .agent_post::<_, MonitorStartOut>(
                    "/monitor/start",
                    &MonitorStartReq {
                        iface: iface.to_string(),
                        interval_ms,
                        owner_id: owner_id.to_string(),
                        lease_secs,
                    },
                    Duration::from_secs(20),
                )
                .map(|o| o.id),
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
        let contents = build_monitor_samples_csv(side.cn(), iface, out);
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
                    let (status, text) = match crate::http_client::post_json(
                        &self.agent_host,
                        self.agent_port,
                        "/screenshot",
                        &body,
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
        let mut g = self.rows.lock().unwrap();
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

    pub fn run_all_with_preflight(
        &self,
        units: &[Unit],
        block: Option<&IperfPreflightBlock>,
    ) -> RunSummary {
        self.run_all_internal(units, 0, block)
    }

    fn run_all_internal(
        &self,
        units: &[Unit],
        sequence_offset: usize,
        iperf_block: Option<&IperfPreflightBlock>,
    ) -> RunSummary {
        let mut sum = RunSummary::default();
        let total = units.len();
        for (i, unit) in units.iter().enumerate() {
            let useq = sequence_offset + i;
            let is_iperf_unit = unit_has_iperf(unit);
            if is_iperf_unit {
                sum.iperf_units += 1;
            }
            let blocked = is_iperf_unit.then_some(()).and(iperf_block);
            logln(&format!("\n[{}/{}] {}", i + 1, total, unit.title));
            if self.cfg.resume && blocked.is_none() {
                let fresh = { self.db.lock().unwrap().fresh_pass(&unit.id) };
                if let Some(t) = fresh {
                    logln(&format!("  已PASS，上次时间: {t}，跳过 (RESUME)"));
                    sum.skip += 1;
                    if is_iperf_unit {
                        // 24 小时内已有 PASS 结果时，不因本轮 resume 跳过而重复触发故障诊断。
                        sum.iperf_usable_units += 1;
                    }
                    self.push_row(Row {
                        sort_key: (useq, 0, 0, 0),
                        time: now_full(),
                        task_id: unit.id.clone(),
                        parent_id: unit.id.clone(),
                        task: unit.title.clone(),
                        verdict: Verdict::Skip,
                        execution_status: ExecutionStatus::Skipped,
                        kind_label: format!("跳过(上次PASS: {t})"),
                        is_unit_summary: true,
                        ..Default::default()
                    });
                    continue;
                }
            }

            if let Some(block) = blocked {
                logln(&format!(
                    "  [iperf 前置检查拦截] {}: {}",
                    block.reason_code, block.reason_detail
                ));
            }

            let owner_id = unit_resource_owner(unit, useq);
            let lease_secs = unit_resource_lease_secs(unit);
            let mut resource_guard = (is_iperf_unit && blocked.is_none()).then(|| {
                UnitResourceGuard::new(self, owner_id.clone(), unit_uses_agent_resources(unit))
            });
            let outcomes = execute_unit_safely(
                || {
                    if let Some(block) = blocked {
                        preflight_block_outcomes(unit, block)
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
                                .map(|h| {
                                    h.join().unwrap_or(LegOutcome {
                                        verdict: Verdict::SetupError,
                                        reason_code: "LEG_THREAD_PANIC".into(),
                                        reason_detail: "方向执行线程 panic".into(),
                                        rx_avg: None,
                                        main_rows: vec![],
                                        tag: String::new(),
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

            // 双向：互填「对向接收 Mbps」
            if unit.bidir && outcomes.len() == 2 {
                let mut g = self.rows.lock().unwrap();
                for (me, other) in [(0usize, 1usize), (1, 0)] {
                    if let Some(rx) = outcomes[other].rx_avg {
                        for ri in &outcomes[me].main_rows {
                            if let Some(row) = g.get_mut(*ri) {
                                row.peer_rx = format!("{:.3}({})", rx, outcomes[other].tag);
                            }
                        }
                    }
                }
            }

            let unit_verdict = aggregate_unit_verdict(&outcomes);
            if is_iperf_unit {
                if blocked.is_none() && self.outcomes_have_usable_iperf_measurement(&outcomes) {
                    sum.iperf_usable_units += 1;
                }
                if unit_verdict == Verdict::SetupError {
                    sum.iperf_setup_errors += 1;
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
                Verdict::Unstable => {
                    sum.unstable += 1;
                    sum.fail += 1;
                }
                Verdict::RateFail => sum.fail += 1,
                Verdict::Skip => sum.skip += 1,
            }
            let reasons: Vec<String> = outcomes
                .iter()
                .filter(|outcome| outcome.verdict != Verdict::Pass)
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
            logln(&format!("  ==> 单元结果: {}", unit_verdict.label()));
            self.push_row(Row {
                sort_key: (useq, 0, 0, 0),
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
                is_unit_summary: true,
                ..Default::default()
            });
            {
                let mut db = self.db.lock().unwrap();
                db.set(&unit.id, unit_ok, &unit.title);
                db.save();
            }
            if blocked.is_none() && is_iperf_unit {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        sum
    }

    fn outcomes_have_usable_iperf_measurement(&self, outcomes: &[LegOutcome]) -> bool {
        let rows = self.rows.lock().unwrap();
        outcomes.iter().any(|outcome| {
            outcome.main_rows.iter().any(|index| {
                rows.get(*index)
                    .map(row_has_usable_iperf_measurement)
                    .unwrap_or(false)
            })
        })
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
            ""
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
            String::new()
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
            ping_avg: (!gateway_missing && exec_kind.is_none())
                .then_some(out.rtt_avg)
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
        on_event: F,
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
        let client = self.client_run_tracked(
            t.src.side,
            &creq,
            owner_id,
            &lifecycle_request_id(owner_id, "client", t.port, 0),
            lease_secs,
            on_event,
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
        let mon_id = match self.mon_start(
            t.dst.side,
            &t.dst.nic.name,
            lifecycle.owner_id,
            lifecycle.lease_secs,
        ) {
            Ok(id) => Some(id),
            Err(e) => {
                logln(&format!("    (接收端网卡监控启动失败: {e})"));
                None
            }
        };
        let live = Arc::new(Mutex::new(LiveFlowState::default()));
        let mut events = Vec::new();
        let parallel_streams = if t.udp {
            1
        } else {
            tcp_parallel_streams(&t.extra)
        };
        let mon_id_for_progress = mon_id.clone();
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
            let result =
                self.exec_iperf_core(t, lifecycle.owner_id, lifecycle.lease_secs, |event| {
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
                });
            let _ = done_tx.send(());
            let _ = progress.join();
            result
        });
        let mon_out = mon_id
            .as_deref()
            .and_then(|id| self.mon_stop(t.dst.side, id).ok());
        let rx_avg = mon_out.as_ref().map(|m| m.avg_mbps);
        let nic_samples = mon_out
            .as_ref()
            .map(|out| {
                self.save_monitor_samples(
                    lifecycle.owner_id,
                    t.dst.side,
                    &t.dst.nic.name,
                    &t.dst.key(),
                    out,
                )
            })
            .unwrap_or_default();

        let meas_ok =
            parsed.has_measurement() || rx_avg.map(|v| v > MIN_VALID_RX_MBPS).unwrap_or(false);
        let ok = raw_ok && meas_ok;
        let verdict = if ok {
            Verdict::Pass
        } else if !raw_ok {
            Verdict::SetupError
        } else {
            Verdict::RateFail
        };
        let reason_code = if ok {
            String::new()
        } else if !raw_ok {
            "IPERF_EXEC_FAILED".into()
        } else {
            "NO_VALID_MEASUREMENT".into()
        };
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
            if ok { "PASS" } else { "FAIL" },
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
            execution_status: if raw_ok {
                ExecutionStatus::Completed
            } else if client.timed_out {
                ExecutionStatus::TimedOut
            } else {
                ExecutionStatus::Error
            },
            reason_code: reason_code.clone(),
            reason_detail: if ok {
                String::new()
            } else {
                client.output.lines().last().unwrap_or_default().to_string()
            },
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
            reason_detail: if ok {
                String::new()
            } else {
                client.output.lines().last().unwrap_or_default().to_string()
            },
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
    ) -> Result<IperfServerStartReq, String> {
        let mut errors = Vec::new();
        for attempt in 0..=UDP_SERVER_START_RETRIES {
            let mut req = base_req.clone();
            if attempt > 0 {
                req.request_id = format!("{}-start{attempt}", base_req.request_id);
            }
            match self.server_start(task.dst.side, &req) {
                Ok(_) => return Ok(req),
                Err(e) => {
                    errors.push(format!("第{}次: {e}", attempt + 1));
                    if attempt < UDP_SERVER_START_RETRIES {
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
                parsed: iperf::IperfParsed::default(),
                client: IperfClientOut {
                    output: prepared.server_error.clone(),
                    ..Default::default()
                },
                server_output: String::new(),
                events: vec![],
                retries: 0,
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
        let mut retries = 0usize;
        let mut final_error = String::new();

        let max_flow_retries = self.cfg.iperf.rate_check.flow_retries as usize;
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
            let parsed = iperf::parse_output(&client.output);
            let raw_ok =
                client.ok && !client.timed_out && !client.cancelled && parsed.has_measurement();

            all_events.extend(attempt_events);
            all_client_output.push(format!(
                "=== group attempt {} ===\n{}",
                attempt + 1,
                client.output
            ));
            final_client = client;
            final_parsed = parsed;
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
            final_ok = raw_ok && stop_ok;
            all_server_output.push(format!("=== attempt {} ===\n{}", attempt + 1, server_out));
            if !stop_ok {
                final_error = "server 停止未确认，禁止在同端口继续重试".into();
                break;
            }
            if final_ok {
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

            let retryable = should_retry_udp_flow(
                attempt,
                max_flow_retries,
                attempt_started.elapsed(),
                retry_cutoff,
                &final_client,
            );
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
                "    [UDP流重试] {}-#{} 首轮未跑通，重新启动 server/client（{}/{}）",
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
            match self.start_udp_server_with_retry(&prepared.task, &next_server_req) {
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

        let total_retries = count_retry_events(&all_events);
        UdpFlowRun {
            leg_pos: prepared.leg_pos,
            stream_pos: prepared.stream_pos,
            task: prepared.task,
            raw_ok: final_ok,
            parsed: final_parsed,
            client: final_client,
            server_output: all_server_output.join("\n"),
            events: all_events,
            retries: total_retries,
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
                let launch_delay_ms = launch_delays
                    .get(&(leg_pos, stream_pos))
                    .copied()
                    .unwrap_or(0);
                let remaining_launch_secs = max_launch_delay_ms
                    .saturating_sub(launch_delay_ms)
                    .div_ceil(1000);
                // duration 对用户表示有效测量时长。更早启动的流自动多跑，
                // 让 discover 阶梯、错峰、settle 和一次快速重试后仍有共同窗口。
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
                        task: task.clone(),
                        server_req: Some(server_req),
                        client_req: Some(client_req),
                        server_error: String::new(),
                        launch_delay_ms,
                    }),
                    Err(e) => prepared.push(PreparedUdpFlow {
                        leg_pos,
                        stream_pos,
                        task: task.clone(),
                        server_req: None,
                        client_req: None,
                        server_error: e,
                        launch_delay_ms: 0,
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
                            match catch_unwind(AssertUnwindSafe(|| {
                                self.start_udp_server_with_retry(&flow.task, &req)
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
                        Ok(id) => {
                            let after_ms = epoch.elapsed().as_millis() as u64;
                            monitor_ids.insert(
                                key,
                                (
                                    endpoint.side,
                                    id,
                                    before_ms + (after_ms - before_ms) / 2,
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
                    let sample_file = self.save_monitor_samples(owner_id, side, &iface, &key, &out);
                    monitor_sample_files.insert(key.clone(), sample_file);
                    monitor_outputs.insert(key, out);
                }
                Err(e) => logln(&format!("    (网卡监控停止失败: {e})")),
            }
        }

        let effective_window = select_udp_effective_window(
            plans,
            &results,
            &monitor_outputs,
            &self.cfg.iperf.rate_check,
        );
        logln(&format!(
            "    有效窗口: {:.1}s / {}s{}",
            effective_window.available_secs,
            effective_window.required_secs,
            if effective_window.complete {
                "（满足）"
            } else {
                "（不足，不能正式判定）"
            }
        ));

        let mut outcomes = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            let leg_flows: Vec<&UdpFlowRun> =
                results.iter().filter(|r| r.leg_pos == leg_pos).collect();
            let n = plan.streams.len();
            let success = leg_flows.iter().filter(|r| r.raw_ok).count();
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
            let rx_stats = monitor_outputs
                .get(&first.dst.key())
                .map(|out| monitor_rate_stats(out, &effective_window, true, first_active_ms))
                .unwrap_or_default();
            let tx_stats = monitor_outputs
                .get(&first.src.key())
                .map(|out| monitor_rate_stats(out, &effective_window, false, first_active_ms))
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
                (
                    Verdict::SetupError,
                    "NO_STREAM_STARTED".to_string(),
                    format!("0/{n} 条流产生有效测量"),
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
                        "共同有效窗口 {:.1}s，要求 {}s",
                        effective_window.available_secs, effective_window.required_secs
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
                    (
                        Verdict::Unstable,
                        "RX_UNSTABLE".to_string(),
                        format!(
                            "平均速率达到目标，但5秒滚动P10 {} 低于 {}Mbps",
                            fmt_opt(rx_stats.p10_mbps),
                            target
                        ),
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

            for flow in &leg_flows {
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
                    verdict: if flow.raw_ok {
                        Verdict::Pass
                    } else {
                        Verdict::SetupError
                    },
                    execution_status: if flow.raw_ok {
                        ExecutionStatus::Completed
                    } else if flow.client.timed_out {
                        ExecutionStatus::TimedOut
                    } else if flow.client.cancelled {
                        ExecutionStatus::Cancelled
                    } else {
                        ExecutionStatus::Error
                    },
                    reason_code: if flow.raw_ok {
                        String::new()
                    } else {
                        "FLOW_FAILED".into()
                    },
                    reason_detail: flow.error.clone(),
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

fn fmt_opt(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.3}Mbps"),
        None => "-".into(),
    }
}

fn text_preview(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn row_has_usable_iperf_measurement(row: &Row) -> bool {
    if row.verdict == Verdict::SetupError
        || matches!(
            row.execution_status,
            ExecutionStatus::Error | ExecutionStatus::TimedOut | ExecutionStatus::Cancelled
        )
    {
        return false;
    }
    let usable_rate =
        |value: Option<f64>| value.is_some_and(|rate| rate.is_finite() && rate > MIN_VALID_RX_MBPS);
    usable_rate(row.rx_avg)
        || usable_rate(row.tx_mbps)
        || usable_rate(row.rx_mbps)
        || usable_rate(row.tx_avg)
        || row.active_streams > 0
}

fn aggregate_unit_verdict(outcomes: &[LegOutcome]) -> Verdict {
    if outcomes.is_empty() {
        return Verdict::SetupError;
    }
    for verdict in [
        Verdict::SetupError,
        Verdict::NotEvaluated,
        Verdict::RateFail,
        Verdict::Unstable,
        Verdict::Measured,
    ] {
        if outcomes.iter().any(|outcome| outcome.verdict == verdict) {
            return verdict;
        }
    }
    if outcomes
        .iter()
        .all(|outcome| outcome.verdict == Verdict::Pass)
    {
        Verdict::Pass
    } else {
        Verdict::NotEvaluated
    }
}

fn outcome_matching_verdict(outcomes: &[LegOutcome], verdict: Verdict) -> Option<&LegOutcome> {
    outcomes.iter().find(|outcome| outcome.verdict == verdict)
}

fn count_retry_events(events: &[IperfFlowEvent]) -> usize {
    events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Retry)
        .count()
}

fn rate_sample_coverage_sufficient(
    rx_stats: &RateStats,
    tx_stats: &RateStats,
    target_present: bool,
) -> bool {
    rx_stats.coverage >= MIN_RATE_SAMPLE_COVERAGE
        && (!target_present || tx_stats.coverage >= MIN_RATE_SAMPLE_COVERAGE)
}

fn rate_window_coverage_sufficient(
    rx_stats: &RateStats,
    tx_stats: &RateStats,
    target_present: bool,
) -> bool {
    !target_present
        || (rx_stats.p10_mbps.is_some()
            && tx_stats.p10_mbps.is_some()
            && rx_stats.rolling_coverage >= MIN_RATE_SAMPLE_COVERAGE
            && tx_stats.rolling_coverage >= MIN_RATE_SAMPLE_COVERAGE)
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

fn build_monitor_samples_csv(endpoint: &str, iface: &str, out: &MonitorStopOut) -> String {
    let mut csv = format!(
        "# CPE OS NIC counter samples\n\
# endpoint,{}\n\
# interface,{}\n\
# seconds,{:.6}\n\
# average_rx_mbps,{:.6}\n\
# average_tx_mbps,{:.6}\n\
elapsed_ms,interval_ms,rx_bytes,tx_bytes,rx_delta_bytes,tx_delta_bytes,rx_mbps,tx_mbps,valid,error\n",
        csv_field(endpoint),
        csv_field(iface),
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

fn flow_active_interval(flow: &UdpFlowRun) -> Option<(u64, u64)> {
    if !flow.raw_ok {
        return None;
    }
    let latest_retry_ms = flow
        .events
        .iter()
        .filter(|event| event.kind == IperfEventKind::Retry)
        .map(|event| event.elapsed_ms)
        .max();
    let retry_cutoff = latest_retry_ms.unwrap_or(0);
    let end = flow
        .events
        .iter()
        .rev()
        .find(|event| event.kind == IperfEventKind::Ended && event.elapsed_ms >= retry_cutoff)
        .map(|event| event.elapsed_ms)?;
    let expected_ms = flow.task.duration.saturating_mul(1_000);

    let started = flow
        .events
        .iter()
        .rev()
        .find(|event| {
            event.kind == IperfEventKind::Started
                && event.elapsed_ms >= retry_cutoff
                && event.elapsed_ms < end
        })
        .map(|event| event.elapsed_ms);
    let attempt_floor = started.unwrap_or(retry_cutoff);
    let connected = flow
        .events
        .iter()
        .find(|event| {
            event.kind == IperfEventKind::Connected
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms < end
        })
        .map(|event| event.elapsed_ms);
    let traffic_events: Vec<&IperfFlowEvent> = flow
        .events
        .iter()
        .filter(|event| {
            event.kind == IperfEventKind::Traffic
                && event.elapsed_ms >= attempt_floor
                && event.elapsed_ms <= end
                && event.mbps.unwrap_or(0.0) > 0.0
        })
        .collect();
    let first_traffic = traffic_events.first().map(|event| event.elapsed_ms);

    // interval 行内的时间是 iperf 进程自己的测量时间，不受 stdout 块缓冲影响。
    // 优先用最终汇总区间反推起流时刻；只有区间覆盖了用户要求的有效时长才采用，
    // 避免把一次过早结束的短测量误扩成完整测试。
    let reported_duration_ms = traffic_events
        .iter()
        .filter_map(|event| iperf_interval_ms(&event.line))
        .map(|(start_ms, end_ms)| end_ms - start_ms)
        .max();
    if let Some(duration_ms) = reported_duration_ms
        .filter(|duration_ms| duration_ms.saturating_add(FLOW_TIMELINE_TOLERANCE_MS) >= expected_ms)
    {
        let start = end.saturating_sub(duration_ms).max(attempt_floor);
        if flow_duration_is_plausible(start, end, expected_ms) {
            return Some((start, end));
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

fn nearest_valid_sample(
    out: &MonitorStopOut,
    elapsed_ms: u64,
    max_distance_ms: u64,
) -> Option<&MonitorSample> {
    out.samples
        .iter()
        .filter(|sample| sample.valid)
        .min_by_key(|sample| sample.elapsed_ms.abs_diff(elapsed_ms))
        .filter(|sample| sample.elapsed_ms.abs_diff(elapsed_ms) <= max_distance_ms)
}

fn select_udp_effective_window(
    plans: &[UdpLegPlan],
    results: &[UdpFlowRun],
    monitors: &HashMap<String, MonitorStopOut>,
    rate_cfg: &RateCheckCfg,
) -> EffectiveWindow {
    let required_secs = plans
        .iter()
        .flat_map(|plan| plan.streams.iter().map(|task| task.duration))
        .max()
        .unwrap_or(0);
    let mut lower = 0u64;
    let mut upper = u64::MAX;
    for plan in plans {
        let Some(first) = plan.streams.first() else {
            continue;
        };
        let Some(out) = monitors.get(&first.dst.key()) else {
            return EffectiveWindow {
                required_secs,
                ..Default::default()
            };
        };
        let Some(first_sample) = out.samples.iter().find(|sample| sample.valid) else {
            return EffectiveWindow {
                required_secs,
                ..Default::default()
            };
        };
        let Some(last_sample) = out.samples.iter().rev().find(|sample| sample.valid) else {
            return EffectiveWindow {
                required_secs,
                ..Default::default()
            };
        };
        lower = lower.max(first_sample.elapsed_ms);
        upper = upper.min(last_sample.elapsed_ms);
    }
    if upper <= lower || upper == u64::MAX {
        return EffectiveWindow {
            required_secs,
            ..Default::default()
        };
    }
    let sample_tolerance_ms = rate_cfg
        .sample_interval_ms
        .clamp(200, 5_000)
        .saturating_mul(2)
        .max(1_500);

    let eligible = |t: u64| -> bool {
        plans.iter().enumerate().all(|(leg_pos, plan)| {
            let first = plan.streams.first();
            let required = required_udp_streams(
                plan.streams.len(),
                rate_cfg,
                first.and_then(|task| task.rx_target_mbps),
                first.and_then(|task| task.offered_mbps),
            );
            let active = results
                .iter()
                .filter(|flow| flow.leg_pos == leg_pos)
                .filter_map(flow_active_interval)
                .filter(|(start, end)| *start <= t && t < *end)
                .count();
            if active < required {
                return false;
            }
            let Some(first) = first else {
                return false;
            };
            monitors
                .get(&first.dst.key())
                .and_then(|out| nearest_valid_sample(out, t, sample_tolerance_ms))
                .is_some()
        })
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

fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = (((sorted.len() - 1) as f64) * q.clamp(0.0, 1.0)).round() as usize;
    sorted.get(idx).copied()
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
    if counts.len() == successful.len() {
        let lost: u64 = counts.iter().map(|(lost, _)| *lost).sum();
        let total: u64 = counts.iter().map(|(_, total)| *total).sum();
        if total > 0 {
            return Some(lost as f64 * 100.0 / total as f64);
        }
    }

    let percentages: Vec<f64> = successful
        .iter()
        .filter_map(|flow| flow.parsed.udp_loss_pct)
        .collect();
    (percentages.len() == successful.len())
        .then(|| percentages.iter().sum::<f64>() / percentages.len() as f64)
}

fn rolling_time_window_averages(
    samples: &[(u64, u64, f64)],
    range_start_ms: u64,
    window_ms: u64,
) -> Vec<f64> {
    if window_ms == 0 {
        return samples.iter().map(|(_, _, rate)| *rate).collect();
    }

    let mut rolling = Vec::new();
    for (window_end_ms, _, _) in samples {
        let window_start_ms = window_end_ms.saturating_sub(window_ms);
        if window_start_ms < range_start_ms
            || window_end_ms.saturating_sub(window_start_ms) < window_ms
        {
            continue;
        }

        let mut weighted_sum = 0.0;
        let mut covered_ms = 0u64;
        for (sample_end_ms, interval_ms, rate) in samples {
            if *interval_ms == 0 || *sample_end_ms <= window_start_ms {
                continue;
            }
            if *sample_end_ms > *window_end_ms {
                break;
            }
            let sample_start_ms = sample_end_ms
                .saturating_sub(*interval_ms)
                .max(range_start_ms);
            let overlap_start = sample_start_ms.max(window_start_ms);
            let overlap_end = (*sample_end_ms).min(*window_end_ms);
            let overlap_ms = overlap_end.saturating_sub(overlap_start);
            if overlap_ms > 0 {
                weighted_sum += *rate * overlap_ms as f64;
                covered_ms = covered_ms.saturating_add(overlap_ms);
            }
        }
        // 只把实际样本完整覆盖的五秒区间纳入稳定性判定；缺口由 coverage
        // 另行约束，不能用相邻样本跨越缺口拼出一个虚假的五秒窗口。
        // elapsed_ms/interval_ms 均由 Duration 向下取整为毫秒，多个样本边界可能
        // 累积出数毫秒的舍入缝隙；只容忍极小误差，不能容忍真正的漏采周期。
        if covered_ms.saturating_add(ROLLING_COVERAGE_TOLERANCE_MS) >= window_ms {
            rolling.push(weighted_sum / covered_ms as f64);
        }
    }
    rolling
}

fn nominal_monitor_interval_ms(out: &MonitorStopOut, window: &EffectiveWindow) -> Option<u64> {
    let mut all = Vec::new();
    let mut interior = Vec::new();
    for sample in &out.samples {
        if sample.interval_ms == 0
            || sample.elapsed_ms <= window.start_ms
            || sample.elapsed_ms.saturating_sub(sample.interval_ms) >= window.end_ms
        {
            continue;
        }
        all.push(sample.interval_ms);
        // stop 唤醒产生的最后一个样本通常短于正常周期，优先用完全处于
        // 窗口内部的周期推断 nominal interval，避免边界样本拉低结果。
        if sample.elapsed_ms.saturating_sub(sample.interval_ms) >= window.start_ms
            && sample.elapsed_ms < window.end_ms
        {
            interior.push(sample.interval_ms);
        }
    }
    let intervals = if interior.is_empty() {
        &mut all
    } else {
        &mut interior
    };
    if intervals.is_empty() {
        return None;
    }
    intervals.sort_unstable();
    // 取较保守的下中位数，避免“一个正常周期 + 一个跨周期恢复样本”把
    // nominal interval 放大到足以让恢复样本伪装成稳定窗口。MonitorMgr
    // 的真实配置上限为 5 秒，额外封顶也能识别线程长时间失调度的样本。
    Some(intervals[(intervals.len() - 1) / 2].min(ROLLING_RATE_WINDOW_MS))
}

fn monitor_rate_stats(
    out: &MonitorStopOut,
    window: &EffectiveWindow,
    rx: bool,
    first_active_ms: u64,
) -> RateStats {
    if window.end_ms <= window.start_ms {
        return RateStats::default();
    }
    let mut baseline_values: Vec<f64> = out
        .samples
        .iter()
        .filter(|sample| {
            sample.valid
                && sample.interval_ms > 0
                && sample.elapsed_ms > 0
                && sample.elapsed_ms <= first_active_ms
                && (if rx { sample.rx_mbps } else { sample.tx_mbps }).is_finite()
        })
        .map(|sample| if rx { sample.rx_mbps } else { sample.tx_mbps })
        .collect();
    baseline_values.sort_by(|a, b| a.total_cmp(b));
    let baseline = percentile(&baseline_values, 0.5).unwrap_or(0.0);
    let nominal_interval_ms = nominal_monitor_interval_ms(out, window);
    let max_rolling_sample_ms = nominal_interval_ms.map(|nominal| {
        nominal
            .saturating_mul(3)
            .saturating_div(2)
            .saturating_add(ROLLING_COVERAGE_TOLERANCE_MS)
    });

    // 每个速率样本代表 [elapsed-interval, elapsed) 的一段时间，而不是一个
    // 等权点。先裁到正式判定窗口，再去掉因毫秒取整或异常输入造成的重叠。
    let mut clipped_samples: Vec<(u64, u64, f64, bool)> = out
        .samples
        .iter()
        .filter(|sample| {
            sample.valid
                && sample.interval_ms > 0
                && sample.elapsed_ms > window.start_ms
                && sample.elapsed_ms.saturating_sub(sample.interval_ms) < window.end_ms
                && (if rx { sample.rx_mbps } else { sample.tx_mbps }).is_finite()
        })
        .filter_map(|sample| {
            let value = if rx { sample.rx_mbps } else { sample.tx_mbps };
            let start_ms = sample
                .elapsed_ms
                .saturating_sub(sample.interval_ms)
                .max(window.start_ms);
            let end_ms = sample.elapsed_ms.min(window.end_ms);
            (end_ms > start_ms).then_some((
                start_ms,
                end_ms,
                (value - baseline).max(0.0),
                max_rolling_sample_ms
                    .is_some_and(|max_interval| sample.interval_ms <= max_interval),
            ))
        })
        .collect();
    clipped_samples.sort_by_key(|(start_ms, end_ms, _, _)| (*start_ms, *end_ms));

    let mut rate_samples: Vec<(u64, u64, f64)> = Vec::with_capacity(clipped_samples.len());
    let mut rolling_rate_samples: Vec<(u64, u64, f64)> = Vec::with_capacity(clipped_samples.len());
    let mut covered_until_ms = window.start_ms;
    let mut rolling_covered_until_ms = window.start_ms;
    for (sample_start_ms, sample_end_ms, rate, rolling_eligible) in clipped_samples {
        let non_overlapping_start_ms = sample_start_ms.max(covered_until_ms);
        if sample_end_ms > non_overlapping_start_ms {
            rate_samples.push((
                sample_end_ms,
                sample_end_ms - non_overlapping_start_ms,
                rate,
            ));
            covered_until_ms = sample_end_ms;
        }
        if rolling_eligible {
            let rolling_start_ms = sample_start_ms.max(rolling_covered_until_ms);
            if sample_end_ms > rolling_start_ms {
                rolling_rate_samples.push((sample_end_ms, sample_end_ms - rolling_start_ms, rate));
                rolling_covered_until_ms = sample_end_ms;
            }
        }
    }

    let mut rates: Vec<f64> = rate_samples.iter().map(|(_, _, rate)| *rate).collect();
    if rates.is_empty() {
        return RateStats::default();
    }
    let covered_ms: u64 = rate_samples
        .iter()
        .map(|(_, interval_ms, _)| *interval_ms)
        .sum();
    if covered_ms == 0 {
        return RateStats::default();
    }
    let avg = rate_samples
        .iter()
        .map(|(_, interval_ms, rate)| *rate * *interval_ms as f64)
        .sum::<f64>()
        / covered_ms as f64;
    let min = rates.iter().copied().fold(f64::INFINITY, f64::min);
    let max = rates.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let rolling = rolling_time_window_averages(
        &rolling_rate_samples,
        window.start_ms,
        ROLLING_RATE_WINDOW_MS,
    );
    rates.sort_by(|a, b| a.total_cmp(b));
    let mut rolling_sorted = rolling;
    rolling_sorted.sort_by(|a, b| a.total_cmp(b));
    let window_ms = window.end_ms - window.start_ms;
    let expected_rolling_windows = nominal_interval_ms
        .filter(|nominal| *nominal > 0 && window_ms >= ROLLING_RATE_WINDOW_MS)
        .map(|nominal| {
            window_ms
                .saturating_sub(ROLLING_RATE_WINDOW_MS)
                .saturating_div(nominal)
                .saturating_add(1)
        })
        .unwrap_or(0);
    let rolling_coverage = if expected_rolling_windows == 0 {
        0.0
    } else {
        (rolling_sorted.len() as f64 / expected_rolling_windows as f64).min(1.0)
    };
    RateStats {
        avg_mbps: Some(avg),
        p10_mbps: percentile(&rolling_sorted, 0.10),
        median_mbps: percentile(&rates, 0.50),
        p95_mbps: percentile(&rates, 0.95),
        min_mbps: Some(min),
        max_mbps: Some(max),
        coverage: (covered_ms as f64 / window_ms as f64).min(1.0),
        rolling_coverage,
    }
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
        if age.num_hours() <= RESUME_MAX_AGE_HOURS && age.num_seconds() >= -60 {
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
    use crate::master::builder::{Endpoint, PingPurpose, PingTask};
    use crate::protocol::NicInfo;

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

    fn isolated_ctx(agent_port: u16) -> (Ctx, PathBuf) {
        let db_path = std::env::temp_dir().join(format!(
            "cpe_test_executor_{}_{}.json",
            std::process::id(),
            RESOURCE_OWNER_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let ctx = Ctx {
            agent_host: "127.0.0.1".into(),
            agent_port,
            cfg: Config {
                screenshot: false,
                open_report: false,
                ..Default::default()
            },
            outdir: std::env::temp_dir(),
            local_servers: IperfServerMgr::new(),
            local_monitors: MonitorMgr::new(),
            rows: Mutex::new(Vec::new()),
            db: Mutex::new(ResultDb::load(db_path.clone())),
        };
        (ctx, db_path)
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
    fn test_required_udp_stream_quorum() {
        let cfg = RateCheckCfg::default();
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
        let window =
            select_udp_effective_window(&plans, &results, &monitors, &RateCheckCfg::default());
        assert!(window.complete);
        assert_eq!(window.start_ms, 6_000);
        assert_eq!(window.end_ms, 186_000);
        assert_eq!(window.available_secs, 184.0);

        let failed_small_leg_flow = results
            .iter_mut()
            .find(|flow| flow.leg_pos == 1 && flow.stream_pos == 1)
            .unwrap();
        failed_small_leg_flow.raw_ok = false;
        failed_small_leg_flow.events.clear();
        let no_common_window =
            select_udp_effective_window(&plans, &results, &monitors, &RateCheckCfg::default());
        assert!(!no_common_window.complete);
        assert_eq!(no_common_window.available_secs, 0.0);
        assert_eq!(no_common_window.start_ms, 0);
        assert_eq!(no_common_window.end_ms, 0);
    }

    #[test]
    fn test_effective_window_short_when_one_direction_drops_early() {
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
        let window =
            select_udp_effective_window(&plans, &results, &monitors, &RateCheckCfg::default());
        assert!(!window.complete);
        assert_eq!(window.available_secs, 169.0);
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
        let window = select_udp_effective_window(&plans, &results, &monitors, &cfg);
        assert!(window.complete);
        assert_eq!(window.end_ms - window.start_ms, 180_000);
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
            rolling_time_window_averages(&rounded_intervals, 0, 5_000),
            vec![100.0]
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

        second.parsed.udp_lost_datagrams = None;
        second.parsed.udp_total_datagrams = None;
        assert_eq!(aggregate_udp_loss(&[&first, &second]), Some(5.0));

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
            agent_host: "127.0.0.1".into(),
            agent_port: 1,
            cfg,
            outdir: std::env::temp_dir(),
            local_servers: IperfServerMgr::new(),
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
        assert_eq!(summary.iperf_units, 1);
        assert_eq!(summary.iperf_usable_units, 0);
        assert!(summary.needs_iperf_failure_diagnostics());
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
        assert_eq!(summary.iperf_units, 1);
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
    fn diagnostics_trigger_only_when_every_iperf_unit_has_no_measurement() {
        let mut summary = RunSummary {
            iperf_units: 3,
            iperf_setup_errors: 3,
            ..Default::default()
        };
        assert!(summary.needs_iperf_failure_diagnostics());

        summary.iperf_usable_units = 1;
        assert!(!summary.needs_iperf_failure_diagnostics());

        let ping_only = RunSummary::default();
        assert!(!ping_only.needs_iperf_failure_diagnostics());
    }

    #[test]
    fn usable_iperf_measurement_requires_real_rate_or_active_stream() {
        assert!(!row_has_usable_iperf_measurement(&Row::default()));
        assert!(!row_has_usable_iperf_measurement(&Row {
            rx_mbps: Some(0.0),
            ..Default::default()
        }));
        assert!(!row_has_usable_iperf_measurement(&Row {
            verdict: Verdict::SetupError,
            execution_status: ExecutionStatus::Error,
            rx_avg: Some(500.0),
            active_streams: 1,
            ..Default::default()
        }));
        assert!(row_has_usable_iperf_measurement(&Row {
            rx_mbps: Some(100.0),
            ..Default::default()
        }));
        assert!(row_has_usable_iperf_measurement(&Row {
            active_streams: 1,
            ..Default::default()
        }));
    }

    #[test]
    fn run_summary_merge_keeps_iperf_diagnostic_counters() {
        let mut left = RunSummary {
            pass: 1,
            iperf_units: 2,
            iperf_usable_units: 0,
            iperf_setup_errors: 2,
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
        assert_eq!(left.iperf_units, 2);
        assert_eq!(left.iperf_setup_errors, 2);
        assert!(left.needs_iperf_failure_diagnostics());
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
        let csv = build_monitor_samples_csv("agent", "Ethernet 2", &out);
        assert!(csv.contains("elapsed_ms,interval_ms,rx_bytes,tx_bytes"));
        assert!(csv.contains("1000,1000,1012500000,2011250000,12500000,11250000,100.000000,90.000000,false,counter reset"));
        assert!(csv.contains("# endpoint,agent"));
        assert!(csv.contains("# interface,Ethernet 2"));
    }
}
