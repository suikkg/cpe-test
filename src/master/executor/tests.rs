//! `executor` 的测试。
//!
//! 单独成文件不是因为「太长」，而是因为它和产品码的变更节奏不同：这里累积的
//! 是一条条具体的现场回归（run_2026xxxx 的某个 unit 当时错在哪），而产品码
//! 改的是判定与执行结构。混在一个文件里，两种改动会互相制造无谓的冲突。

use super::*;
// 仅测试用到的采样统计层符号；产品码不需要，放这里避免非测试构建报未用导入。
use super::db::resume_age_is_fresh;
use crate::master::builder::{Endpoint, PingPurpose, PingTask};

use crate::master::rate_window::{
    evaluate_rx_acceptance, rate_window_coverage_sufficient, rolling_time_window_series, RateStats,
    MIN_RATE_SAMPLE_COVERAGE,
};

/// 测试里仍按老三元组读结论。
fn nic_rx(
    mode: crate::config::RateMode,
    target_mbps: Option<f64>,
    stats: &RateStats,
) -> (Verdict, ReasonCode, String) {
    let result = evaluate_rx_acceptance(mode, target_mbps, stats);
    (result.verdict, result.code, result.detail)
}
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
    assert_eq!(panic_outcomes[0].reason_code(), ReasonCode::UnitPanic);

    let next_outcomes = execute_unit_safely(
        || {
            vec![LegOutcome {
                judgement: VerdictResult::new(Verdict::Pass, ReasonCode::None, String::new()),
                rx_avg: None,
                main_rows: Vec::new(),
                tag: String::new(),
            }]
        },
        || Err("synthetic cleanup failure".into()),
    );
    assert_eq!(next_outcomes.len(), 2);
    assert_eq!(next_outcomes[0].verdict(), Verdict::Pass);
    assert_eq!(
        next_outcomes[1].reason_code(),
        ReasonCode::ResourceCleanupFailed
    );
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
        offered_total_mbps: udp.then_some(1_500.0),
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
        link_group: String::new(),
        bidir: false,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
        legs: vec![Leg {
            tag: "ab".into(),
            kind: LegKind::CtsTraffic(ctstraffic_task(udp)),
        }],
        est_secs: 25,
    }
}

/// 单元汇总行的「协议」「后端」两列不许再是空的。
///
/// 三处 `unit_row` 调用点以前一律传 `RowProtocol::None, RowBackend::None`，
/// 而 Excel「概览」表的粒度就是单元、数据源就是汇总行——那两列于是**每一行
/// 都空着**。空列不会让任何测试变红，只是在用户拿去验收的表里少两格。
/// 现在协议/后端由 `unit_protocol_and_backend` 从腿的类型推导，调用方传不了。
#[test]
fn a_unit_summary_row_carries_the_protocol_and_backend_of_its_legs() {
    use crate::master::executor::row::unit_protocol_and_backend;

    assert_eq!(
        unit_protocol_and_backend(&ctstraffic_unit("cts-udp", true)),
        (RowProtocol::Udp, RowBackend::CtsTraffic)
    );
    assert_eq!(
        unit_protocol_and_backend(&ctstraffic_unit("cts-tcp", false)),
        (RowProtocol::Tcp, RowBackend::CtsTraffic)
    );

    let ping = Unit {
        id: "ping".into(),
        title: "PING".into(),
        link_group: String::new(),
        bidir: false,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
        legs: vec![Leg {
            tag: String::new(),
            kind: LegKind::Ping(PingTask {
                v6: false,
                src: endpoint(Side::Master, "master0", "192.168.1.2"),
                dst: endpoint(Side::Agent, "agent0", "192.168.1.3"),
                count: 4,
                payload: 32,
                purpose: PingPurpose::SubnetTest,
            }),
        }],
        est_secs: 5,
    };
    assert_eq!(
        unit_protocol_and_backend(&ping),
        (RowProtocol::Icmp, RowBackend::Ping)
    );

    // 汇总行本身也要带上，这是 Excel 概览那两列唯一的数据源。
    let row = crate::master::executor::row::unit_row(
        &ctstraffic_unit("cts-udp", true),
        0,
        "测试单元汇总",
    );
    assert_eq!(row.protocol, RowProtocol::Udp);
    assert_eq!(row.backend, RowBackend::CtsTraffic);
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
    let seq = RESOURCE_OWNER_SEQ.fetch_add(1, Ordering::SeqCst);
    let db_path = std::env::temp_dir().join(format!(
        "cpe_test_executor_{}_{}.json",
        std::process::id(),
        seq
    ));
    // 每个 Ctx 一个独立 run 目录：`persist_new_rows` 会往 run_dir 追加
    // rows.jsonl，共用一个临时目录会让所有用例往同一个文件里叠加。
    let run_dir = std::env::temp_dir().join(format!("cpe_test_run_{}_{}", std::process::id(), seq));
    let _ = std::fs::create_dir_all(&run_dir);
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
        run_dir: run_dir.clone(),
        transport: Arc::new(http_client::TcpTransport),
        clock: Arc::new(SystemClock),
        local_servers: IperfServerMgr::new(),
        local_cts_jobs: IperfClientJobMgr::new(),
        local_monitors: MonitorMgr::new(),
        rows: Mutex::new(Vec::new()),
        observer: None,
        persisted_rows: Mutex::new(0),
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
            offered_per_stream_mbps: Some(500.0),
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
        offered_per_stream_mbps: None,
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
    assert_eq!(code, ReasonCode::FlowMeasured);
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
            judgement: VerdictResult::new(Verdict::Pass, ReasonCode::None, String::new()),
            rx_avg: Some(950.0),
            main_rows: vec![ab_row],
            tag: "ab".into(),
        },
        LegOutcome {
            judgement: VerdictResult::new(Verdict::RateFail, ReasonCode::RxBelowTarget, "ba low"),
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
        judgement: VerdictResult::new(Verdict::Pass, ReasonCode::None, String::new()),
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
                            line: "[  5]   0.00-10.00 sec  600 MBytes  500 Mbits/sec sender".into(),
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
                offered_per_stream_mbps: Some(500.0),
            })
            .collect(),
    };
    let plans = vec![mk(0, "ab", &a, &b, ab_port), mk(1, "ba", &b, &a, ba_port)];
    let unit = Unit {
        id: format!("udp-orch-{ab_port}-{ba_port}"),
        title: "★双向 IPERF V4 UDP -b 500m".into(),
        link_group: String::new(),
        bidir: true,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
        ab.reason_code(),
        ReasonCode::SingleUdpStreamFailed,
        "AB 第三轮已灌通"
    );
    // BA 是必须灌通却没灌通的硬失败。
    assert_eq!(ba.verdict(), Verdict::RateFail, "BA 应为硬失败: {ba:?}");
    assert_eq!(ba.reason_code(), ReasonCode::SingleUdpStreamFailed);

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
            outcome.verdict(),
            Verdict::RateFail,
            "{} 方向应为 RATE_FAIL: {outcome:?}",
            outcome.tag
        );
        assert_eq!(outcome.reason_code(), ReasonCode::SingleUdpStreamFailed);
        assert_ne!(outcome.reason_code(), ReasonCode::ActiveStreamsLow);
        assert_ne!(outcome.reason_code(), ReasonCode::NoStreamStarted);
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
        ab.verdict(),
        Verdict::SetupError,
        "清理未确认必须是 SETUP_ERROR，不能伪装成单流硬失败: {ab:?}"
    );
    assert_ne!(ab.reason_code(), ReasonCode::SingleUdpStreamFailed);

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
    assert_ne!(
        ba.reason_code(),
        ReasonCode::SingleUdpStreamFailed,
        "BA 首轮即灌通"
    );
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
///
/// 运行时错误本身现在**只进诊断**（ADR-17）：它描述的是 iperf3 自己跑得干不
/// 干净，不是这条链路的接收能力。
#[test]
fn udp_keeps_the_real_runtime_error_once_a_measurement_exists() {
    let scripts = HashMap::from([
        (57_800, FlowScript::measured_but_runtime_failed(0)),
        (57_900, FlowScript::at(0)),
    ]);
    let (outcomes, agent, _) = run_udp_orchestration(scripts, 57_800, 57_900, 1);

    let ab = outcomes.iter().find(|o| o.tag == "ab").expect("AB 结果");
    assert_ne!(
        ab.reason_code(),
        ReasonCode::SingleUdpStreamFailed,
        "已有测量时不能改写成 SINGLE_UDP_STREAM_FAILED：{ab:?}"
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
            outcome.reason_code(),
            ReasonCode::SingleUdpStreamFailed,
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
            outcome.verdict(),
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
            outcome.verdict(),
            Verdict::SetupError,
            "{} 方向 server 起不来必须是 SETUP_ERROR: {outcome:?}",
            outcome.tag
        );
        assert_ne!(outcome.reason_code(), ReasonCode::SingleUdpStreamFailed);
    }
}

/// CTS 的 UDP 丢帧**不再改写判定**（ADR-17）。
///
/// 这条测试以前锁的是相反的行为：RX 已经达标的一轮会因为丢帧超限被翻成
/// `RATE_FAIL`，缺丢帧数据还会被翻成 `NOT_EVALUATED`。用户确认的验收规则是
/// 「接收端 RX 平均达到门限必定 PASS」，所以丢帧降为诊断——数值和限制一个
/// 都不少，只是不决定 PASS/FAIL。
#[test]
fn cts_udp_loss_is_a_diagnostic_and_never_overturns_the_rx_verdict() {
    // 丢帧超限：只出诊断。
    let over = cts_udp_loss_diagnostics(true, Some(1.0), Some(9.0));
    assert_eq!(over.len(), 1, "{over:?}");
    assert!(
        over[0].contains("9.000%") && over[0].contains("1.000%"),
        "实测值和限制都要留在诊断里: {over:?}"
    );
    // 已配置门槛却缺数据：同样只是诊断，不再吃掉速率结论。
    let missing = cts_udp_loss_diagnostics(true, Some(1.0), None);
    assert_eq!(missing.len(), 1, "{missing:?}");
    assert!(missing[0].contains("缺少 dropped frames"), "{missing:?}");
    // 门槛内、TCP、未配置门槛：一条诊断都不该有。
    assert!(cts_udp_loss_diagnostics(true, Some(10.0), Some(9.0)).is_empty());
    assert!(cts_udp_loss_diagnostics(false, Some(1.0), Some(9.0)).is_empty());
    assert!(cts_udp_loss_diagnostics(true, None, Some(9.0)).is_empty());
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
    assert_eq!(issue.code, ReasonCode::CtsMonitorNoSamples);
    assert!(issue.detail.contains("全生命周期平均值不能用于"));
    assert_eq!(
        cts_monitor_issue_verdict(&issue).unwrap().verdict,
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
    assert_eq!(issue.code, ReasonCode::CtsMonitorRuntimeError);
    assert!(issue.detail.contains("counter reset"));
    assert_eq!(
        cts_monitor_issue_verdict(&issue).unwrap().verdict,
        Verdict::NotEvaluated
    );

    let startup = CtsMonitorIssue {
        code: ReasonCode::CtsMonitorStartFailed,
        detail: "interface not found".into(),
        setup_error: true,
        affects_verdict: true,
    };
    let judgement = cts_monitor_issue_verdict(&startup).unwrap();
    assert_eq!(judgement.verdict, Verdict::SetupError);
    assert_eq!(judgement.code, ReasonCode::CtsMonitorStartFailed);
    assert_eq!(judgement.detail, "interface not found");
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
    assert_eq!(issue.code, ReasonCode::CtsMonitorRuntimeError);
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
        link_group: String::new(),
        bidir: false,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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

    assert_eq!(outcome.verdict(), Verdict::SetupError);
    assert_eq!(outcome.reason_code(), ReasonCode::CtsArgsInvalid);
    assert_eq!(outcome.reason_detail(), builder_error);
    assert_eq!(outcome.main_rows, vec![0]);
    let rows = ctx.rows.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].verdict, Verdict::SetupError);
    assert_eq!(rows[0].execution_status, ExecutionStatus::Error);
    assert_eq!(rows[0].reason_code, ReasonCode::CtsArgsInvalid);
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

/// ctsTraffic 跑得不干净只留一句诊断（ADR-17）。
///
/// 以前这里返回 `RATE_FAIL / CTS_RUNTIME_ERRORS`：接收端网卡已经收满速率的
/// 一轮，会因为 server 收尾时的一条错误被判失败。线索一个字不少地保留，
/// 但判定只由 RX 平均与门限决定。
#[test]
fn ctstraffic_measured_timeout_or_abnormal_exit_is_only_a_diagnostic() {
    let mut timed_out = ctstraffic_attempt(0, true);
    timed_out.client = IperfClientOut {
        timed_out: true,
        output: "manager timeout; process reaped".into(),
        process_started: Some(true),
        cleanup_confirmed: Some(true),
        ..Default::default()
    };
    let timeout_detail = cts_runtime_diagnostic(&timed_out, 0, false).unwrap();
    assert!(timeout_detail.contains("client 超时"));

    let mut abnormal_exit = ctstraffic_attempt(0, true);
    abnormal_exit.client = IperfClientOut {
        output: "ctsTraffic exited with code 7".into(),
        process_started: Some(true),
        cleanup_confirmed: Some(true),
        ..Default::default()
    };
    let exit_detail = cts_runtime_diagnostic(&abnormal_exit, 0, false).unwrap();
    assert!(exit_detail.contains("未正常完成"));

    let counted_error = cts_runtime_diagnostic(&abnormal_exit, 3, false).unwrap();
    assert!(counted_error.contains("3 个网络/协议/数据错误"));

    let normal = ctstraffic_attempt(0, true);
    assert!(cts_runtime_diagnostic(&normal, 0, true).is_none());
}

#[test]
fn ctstraffic_measured_server_failure_is_a_diagnostic_but_unmeasured_is_setup() {
    let mut measured = ctstraffic_attempt(0, true);
    measured.server_unexpected_failure = true;
    measured.server_output = "server statistics: 500 Mbps\nserver timed out".into();

    assert!(cts_server_unexpected_setup_error(
        measured.server_unexpected_failure,
        measured.traffic_established,
        &measured.server_output,
    )
    .is_none());
    let detail = cts_runtime_diagnostic(&measured, 0, true).unwrap();
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
    assert_eq!(setup_code, ReasonCode::CtsServerFailed);
    assert_eq!(setup_detail, "server exited with code 7");
    assert!(cts_runtime_diagnostic(&unmeasured, 0, false).is_none());
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
    setup.setup_error = Some((ReasonCode::CtsPreflightFailed, "setup".into()));
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

/// 发送端采样覆盖率只进诊断（ADR-17）。
///
/// 它以前是否决性门槛：TX 覆盖率不够就把整行判成 `NOT_EVALUATED` /
/// `RATE_FAIL`。可这块数据描述的是**发送端**，接收端 RX 平均是否达到门限
/// 与它无关。
#[test]
fn a_sparse_tx_sample_series_is_reported_but_never_judged() {
    let rx_stats = RateStats {
        coverage: 1.0,
        p10_mbps: Some(10_000.0),
        rolling_coverage: 1.0,
        ..Default::default()
    };
    let sparse_tx_stats = RateStats {
        coverage: 0.2,
        p10_mbps: Some(10_000.0),
        rolling_coverage: 1.0,
        ..Default::default()
    };
    let with_target = crate::master::rate_window::rx_acceptance_diagnostics(
        &rx_stats,
        &sparse_tx_stats,
        Some(1_000.0),
        None,
    );
    assert!(
        with_target
            .iter()
            .any(|line| line.contains("TX 采样覆盖率")),
        "TX 覆盖率不足要说出来: {with_target:?}"
    );
    // 没有目标就没有验收，也就没有「诊断为什么不达标」这回事。
    assert!(crate::master::rate_window::rx_acceptance_diagnostics(
        &rx_stats,
        &sparse_tx_stats,
        None,
        None
    )
    .is_empty());

    let complete_tx_stats = RateStats {
        coverage: MIN_RATE_SAMPLE_COVERAGE,
        p10_mbps: Some(10_000.0),
        rolling_coverage: 1.0,
        ..Default::default()
    };
    assert!(crate::master::rate_window::rx_acceptance_diagnostics(
        &rx_stats,
        &complete_tx_stats,
        Some(1_000.0),
        None
    )
    .is_empty());
}

#[test]
fn test_rolling_window_coverage_requires_both_sides() {
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

/// 采样线程被抢占产生的**周期偏长**样本，不是漏采，不能当恢复样本剔掉。
///
/// run_20260828_162822_17788 的 unit-257-258：154 个样本全部 `valid`、
/// 计数器 delta 全部完整，只有 11 个周期落在 1660~1993ms（标称 1059ms）。
/// 老口径按「周期 > nominal*1.5」把它们踢出滚动序列，一条废掉约 5 个窗口，
/// 覆盖率被压到 63.6%，unit-257~260 四行全被误判成 NOT_EVALUATED，处置
/// 建议还让人去查「是不是重启/切换过网卡」——而那里什么都没发生。
#[test]
fn jittery_sample_periods_are_not_treated_as_a_sampling_gap() {
    let out = MonitorStopOut {
        samples: (1..=60)
            .map(|second| MonitorSample {
                elapsed_ms: second * 1_000,
                // 每 5 秒抖一次到 1.9 倍标称周期，但一条样本都没丢。
                interval_ms: if second % 5 == 0 { 1_900 } else { 1_000 },
                rx_mbps: 100.0,
                valid: true,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let window = EffectiveWindow {
        start_ms: 0,
        end_ms: 60_000,
        available_secs: 60.0,
        required_secs: 60,
        complete: true,
    };
    let stats = monitor_rate_stats(&out, &window, true, 0);
    assert_eq!(stats.avg_mbps, Some(100.0));
    assert!(
        stats.rolling_coverage > 0.95,
        "周期抖动不是漏采，不该压垮滚动窗口覆盖率: {}",
        stats.rolling_coverage
    );
    assert!(rate_window_coverage_sufficient(&stats, &stats, true));
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
        // 全程稳定在 rx_mbps：180 个 1 秒样本一个都不越界。
        series: (1..=180).map(|i| (i * 1_000, 1_000, rx_mbps)).collect(),
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
    let judged = iperf_flow_verdict(IperfFlowVerdictIn {
        raw_ok: false,
        measurement: true,
        effective_window: &window,
        required_secs: 180,
        rate_mode: RateMode::Observe,
        rx_target_mbps: None,
        rx_stats: &rx,
        tx_stats: &rx,
        offered_floor: None,
        client_tail: TAIL_HANDSHAKE_ERROR,
        rx_monitor: None,
    });
    let (verdict, code, detail) = (judged.verdict, judged.code, judged.detail);
    assert_eq!(
        verdict,
        Verdict::Measured,
        "跑满全程只是收尾握手失败，不能判成环境错误"
    );
    assert_eq!(
        code,
        ReasonCode::TargetUnknown,
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
    let judged = iperf_flow_verdict(IperfFlowVerdictIn {
        raw_ok: false,
        measurement: true,
        effective_window: &window,
        required_secs: 180,
        rate_mode: RateMode::Verify,
        rx_target_mbps: Some(900.0),
        rx_stats: &below,
        tx_stats: &below,
        offered_floor: None,
        client_tail: TAIL_HANDSHAKE_ERROR,
        rx_monitor: None,
    });
    let (verdict, code) = (judged.verdict, judged.code);
    assert_eq!(verdict, Verdict::RateFail);
    assert_eq!(code, ReasonCode::RxBelowTarget);

    // 任务 115 那种「链路已断、网卡全零、iperf 仍自报 136Mbps」的形态：
    // 降级路径必须交给 evaluate_nic_rx 判成 NOT_EVALUATED，
    // 绝不能因为拿到了 sender 数字就算测到了。
    let dead = RateStats {
        avg_mbps: Some(0.0),
        coverage: 1.0,
        rolling_coverage: 1.0,
        ..Default::default()
    };
    let judged = iperf_flow_verdict(IperfFlowVerdictIn {
        raw_ok: false,
        measurement: true,
        effective_window: &window,
        required_secs: 180,
        rate_mode: RateMode::Observe,
        rx_target_mbps: None,
        rx_stats: &dead,
        tx_stats: &dead,
        offered_floor: None,
        client_tail: TAIL_HANDSHAKE_ERROR,
        rx_monitor: None,
    });
    let (verdict, code) = (judged.verdict, judged.code);
    assert_eq!(verdict, Verdict::NotEvaluated);
    assert_eq!(code, ReasonCode::NicRateMissing);
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
/// **报告行和进度页必须说同一句话。**
///
/// 这条是真机联调当场抓到的：双向 UDP 单元判定 PASS，报告里写「双向 RX 合计
/// 1852.734Mbps…门限 1500」，进度页却写「ab:TARGET_UNKNOWN 接收端网卡 RX 已测得
/// 926.140Mbps；未配置可信目标，**因此不标记 PASS**」——判定是 PASS，理由说不
/// 标记 PASS。原因是两处各算各的：`Row` 走合计判定，`UnitStatus` 走腿级的
/// `unit_reason` / `reasons.first()`。合计门限存在时 `leg_rate_plan` 已经把两条
/// 腿都落到 Observe，腿本来就不该有目标，那句话在单元这一层是自相矛盾的。
///
/// 普通单测抓不到：两边分别断言各自的字段都会绿。所以在源码层面钉住「只有一处
/// 计算，两个消费者」。
#[test]
fn the_unit_reason_has_one_source_for_both_the_report_and_the_progress_page() {
    let source = include_str!("../executor.rs");
    for name in ["let unit_reason_code", "let bidir_reason_detail"] {
        assert_eq!(
            source.matches(name).count(),
            1,
            "{name} 必须只算一次；出现两处就是报告和进度页又分叉了"
        );
    }
    // 两个消费者：`Row` 一次、`UnitStatus` 一次（加上定义本身共 2 次以上）。
    for name in ["unit_reason_code", "bidir_reason_detail"] {
        assert!(
            source.matches(name).count() >= 3,
            "{name} 应当被报告行和进度页同时消费，实得 {} 处",
            source.matches(name).count()
        );
    }
    // 出过问题的那一行：进度页直接取腿级理由，不看合计判定。
    assert!(
        !source.contains("reason_detail: reasons.first().cloned().unwrap_or_default()"),
        "进度页不能再绕过合计判定直接取腿级理由"
    );
}

#[test]
fn the_abort_gate_runs_before_any_early_continue() {
    let source = include_str!("../executor.rs");
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

/// 结构断言：三条灌包路径挂 RX 样本的地方，都要同样挂上 TX 样本。
///
/// TX 覆盖率和 `tx_p10` 是**否决性**门槛：覆盖率不够整行判 NOT_EVALUATED，
/// `tx_p10` 不足则报 OFFERED_LOAD_LOW。判定理由引用的数据，报告里就必须能
/// 点回到那一行样本——否则「每个结论都要能回到某一行样本」对 TX 不成立。
///
/// UDP 组曾经就是这样漏的：iperf 单腿和 CTS 都挂了 `nic_samples_tx`，只有
/// UDP 那条链忘了，而 TX 样本其实早就落盘了、只是没人链接。三条路径各写各的
/// `push_row`，漏一条不会有任何用例变红，所以在源码层面数一遍。
#[test]
fn every_traffic_path_links_the_tx_samples_next_to_the_rx_ones() {
    for (name, source) in [
        ("udp.rs", include_str!("udp.rs")),
        ("iperf_leg.rs", include_str!("iperf_leg.rs")),
        ("cts.rs", include_str!("cts.rs")),
    ] {
        let rx = source.matches("nic_samples_rx").count();
        let tx = source.matches("nic_samples_tx").count();
        assert!(rx > 0, "{name} 应该有接收端样本引用");
        assert_eq!(
            rx, tx,
            "{name} 里 RX 样本被引用 {rx} 次、TX 只有 {tx} 次：两边必须成对出现"
        );
    }
}

/// 结构断言：中止点必须是**全局**序号，不能是循环的局部下标。
///
/// 诊断补跑那一趟走的是 `run_all_from(&diagnostics, units.len())`，
/// `sequence_offset` 等于主队列长度。用局部 `i` 的话，「第 147 个单元后中止」
/// 会同时在报告横幅和进度页上写成「第 2 个」——两个出口一起指错位置，而且
/// 因为两边一致，看上去完全正常。
///
/// 循环里其余地方一律用 `useq`，普通用例（offset 恒为 0）抓不到这个偏差，
/// 所以在源码层面钉住。
#[test]
fn the_abort_point_is_recorded_in_the_global_sequence() {
    let source = include_str!("../executor.rs");
    let loop_start = source
        .find("for (i, unit) in units.iter().enumerate() {")
        .expect("单元循环");
    let loop_end = source[loop_start..]
        .find("\n    fn ")
        .map(|offset| loop_start + offset)
        .unwrap_or(source.len());
    let loop_body = &source[loop_start..loop_end];

    assert!(
        !loop_body.contains("aborted_at_unit = Some(i)"),
        "中止点不能记局部下标，必须叠加 sequence_offset"
    );
    assert!(
        !loop_body.contains("observer.run_aborted(i)"),
        "进度页拿到的中止点同样必须是全局序号"
    );
    assert!(
        loop_body.contains("let aborted_at = sequence_offset + i;"),
        "中止点应由 sequence_offset + i 算出，报告与进度页共用同一个数"
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
    let judged = iperf_flow_verdict(IperfFlowVerdictIn {
        raw_ok: true,
        measurement: true,
        effective_window: &empty_window,
        required_secs: 180,
        rate_mode: RateMode::Observe,
        rx_target_mbps: None,
        rx_stats: &RateStats::default(),
        tx_stats: &RateStats::default(),
        offered_floor: None,
        client_tail: "",
        rx_monitor: Some(&monitor),
    });
    let (verdict, code, detail) = (judged.verdict, judged.code, judged.detail);
    assert_eq!(verdict, Verdict::NotEvaluated, "窗口切不出来就是没结论");
    assert_eq!(code, ReasonCode::IperfEffectiveWindowShort);
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
    let judged = iperf_flow_verdict(IperfFlowVerdictIn {
        raw_ok: true,
        measurement: true,
        effective_window: &empty_window,
        required_secs: 180,
        rate_mode: RateMode::Observe,
        rx_target_mbps: None,
        rx_stats: &RateStats::default(),
        tx_stats: &RateStats::default(),
        offered_floor: None,
        client_tail: "",
        rx_monitor: None,
    });
    let detail = judged.detail;
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
    let judged = iperf_flow_verdict(IperfFlowVerdictIn {
        raw_ok: false,
        measurement: true,
        effective_window: &short,
        required_secs: 180,
        rate_mode: RateMode::Observe,
        rx_target_mbps: None,
        rx_stats: &rx,
        tx_stats: &rx,
        offered_floor: None,
        client_tail: "iperf3: error - unable to connect to server",
        rx_monitor: None,
    });
    let (verdict, code) = (judged.verdict, judged.code);
    assert_eq!(verdict, Verdict::SetupError);
    assert_eq!(code, ReasonCode::IperfExecFailed);
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
            judgement: VerdictResult::new(
                Verdict::RateFail,
                ReasonCode::RxBelowTarget,
                "AB rate failed",
            ),
            rx_avg: None,
            main_rows: vec![],
            tag: "AB".into(),
        },
        LegOutcome {
            judgement: VerdictResult::new(
                Verdict::SetupError,
                ReasonCode::NoStreamStarted,
                "BA setup failed",
            ),
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
            .reason_code(),
        ReasonCode::NoStreamStarted
    );
}

#[test]
fn hard_single_udp_failure_beats_other_direction_not_evaluated() {
    let outcomes = vec![
        LegOutcome {
            judgement: VerdictResult::new(
                Verdict::RateFail,
                ReasonCode::SingleUdpStreamFailed,
                "AB exhausted three attempts",
            ),
            rx_avg: None,
            main_rows: vec![],
            tag: "ab".into(),
        },
        LegOutcome {
            judgement: VerdictResult::new(
                Verdict::NotEvaluated,
                ReasonCode::SampleCoverageLow,
                "BA monitor incomplete",
            ),
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
            .reason_code(),
        ReasonCode::SingleUdpStreamFailed
    );

    let cts_outcomes = vec![
        LegOutcome {
            judgement: VerdictResult::new(
                Verdict::RateFail,
                ReasonCode::CtsSingleUdpStreamFailed,
                "AB exhausted three CTS attempts",
            ),
            rx_avg: Some(700.0),
            main_rows: vec![],
            tag: "ab".into(),
        },
        LegOutcome {
            judgement: VerdictResult::new(
                Verdict::NotEvaluated,
                ReasonCode::TargetMissing,
                "BA measured independently",
            ),
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
            .reason_code(),
        ReasonCode::CtsSingleUdpStreamFailed
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
        offered_per_stream_mbps: None,
    };
    let unit = Unit {
        id: "blocked".into(),
        title: "blocked".into(),
        link_group: String::new(),
        bidir: false,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
        legs: vec![Leg {
            tag: "ab".into(),
            kind: LegKind::IperfSingle(iperf),
        }],
        est_secs: 1,
    };
    let block = IperfPreflightBlock {
        reason_code: ReasonCode::IperfPreflightFailed,
        reason_detail: "两端缺少 iperf3".into(),
    };
    let outcomes = preflight_block_outcomes(&unit, &block);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].verdict(), Verdict::SetupError);
    assert_eq!(outcomes[0].reason_code(), ReasonCode::IperfPreflightFailed);
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
        link_group: String::new(),
        bidir: true,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
            judgement: VerdictResult::new(
                Verdict::SetupError,
                ReasonCode::LegThreadPanic,
                "ab 方向执行线程 panic: synthetic",
            ),
            rx_avg: None,
            main_rows: vec![],
            tag: "ab".into(),
        },
        LegOutcome {
            judgement: VerdictResult::new(Verdict::Pass, ReasonCode::None, String::new()),
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
    assert_eq!(ab.reason_code, ReasonCode::LegThreadPanic);
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
        link_group: String::new(),
        bidir: true,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
        judgement: VerdictResult::new(
            Verdict::SetupError,
            ReasonCode::UnitPanic,
            "synthetic unit panic",
        ),
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
        .all(|outcome| outcome.reason_code() == ReasonCode::UnitPanic
            && outcome.main_rows.len() == 1));
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
        link_group: String::new(),
        bidir: true,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
        judgement: VerdictResult::new(
            Verdict::SetupError,
            ReasonCode::UnitPanic,
            "panic after AB row commit",
        ),
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
    assert_eq!(ba.reason_code(), ReasonCode::UnitPanic);
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
        link_group: String::new(),
        bidir: true,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
        reason_code: ReasonCode::IperfPreflightFailed,
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
        .all(|row| row.reason_code == ReasonCode::IperfPreflightFailed));
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
        reason_code: ReasonCode::CtsPreflightFailed,
        reason_detail: "当前平台缺少 ctsTraffic".into(),
    };
    let outcomes = preflight_block_outcomes(&unit, &block);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].verdict(), Verdict::SetupError);
    assert_eq!(outcomes[0].reason_code(), ReasonCode::CtsPreflightFailed);
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
    assert_eq!(summary_row.reason_code, ReasonCode::CtsPreflightFailed);
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
        reason_code: ReasonCode::CtsPreflightFailed,
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
    assert_eq!(detail_rows[0].reason_code, ReasonCode::CtsArgsInvalid);
    assert_eq!(detail_rows[0].reason_detail, "builder rejected duration=0");
    let summary_row = rows.iter().find(|row| row.is_unit_summary).unwrap();
    assert_eq!(summary_row.reason_code, ReasonCode::CtsArgsInvalid);
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
        link_group: String::new(),
        bidir: true,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
        reason_code: ReasonCode::CtsPreflightFailed,
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
    assert!(detail_rows.iter().any(
        |row| row.reason_code == ReasonCode::CtsArgsInvalid && row.kind_label.ends_with("-ab")
    ));
    assert!(detail_rows
        .iter()
        .any(|row| row.reason_code == ReasonCode::CtsPreflightFailed
            && row.kind_label.ends_with("-ba")));
    assert!(detail_rows
        .iter()
        .all(|row| row.kind_label.contains("CTS Traffic")));
    let summary_row = rows.iter().find(|row| row.is_unit_summary).unwrap();
    assert_eq!(summary_row.reason_code, ReasonCode::CtsArgsInvalid);
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
        link_group: String::new(),
        bidir: true,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
        reason_code: ReasonCode::CtsPreflightFailed,
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
        .all(|row| row.reason_code == ReasonCode::CtsArgsInvalid));
    let summary_row = rows.iter().find(|row| row.is_unit_summary).unwrap();
    assert_eq!(summary_row.reason_code, ReasonCode::CtsArgsInvalid);
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
    assert_eq!(skip.reason_code, ReasonCode::ResumeFreshPass);
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
        link_group: String::new(),
        bidir: false,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
                offered_per_stream_mbps: None,
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
        run_dir: std::env::temp_dir(),
        transport: Arc::new(http_client::TcpTransport),
        clock: Arc::new(SystemClock),
        local_servers: IperfServerMgr::new(),
        local_cts_jobs: IperfClientJobMgr::new(),
        local_monitors: MonitorMgr::new(),
        rows: Mutex::new(Vec::new()),
        observer: None,
        persisted_rows: Mutex::new(0),
        db: Mutex::new(ResultDb::load(db_path.clone())),
    };
    let block = IperfPreflightBlock {
        reason_code: ReasonCode::IperfPreflightFailed,
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
        link_group: String::new(),
        bidir: false,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
    assert_eq!(detail.reason_code, ReasonCode::PingOk);
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
    assert_eq!(unit_summary.reason_code, ReasonCode::PingOk);
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
        link_group: String::new(),
        bidir: false,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
    assert_eq!(detail.reason_code, ReasonCode::GatewayNotFound);
    assert_eq!(detail.ping_loss, None);
    drop(rows);
    let _ = std::fs::remove_file(db_path);
}

#[test]
fn agent_ping_http_failure_is_setup_error_not_one_hundred_percent_loss() {
    let unit = Unit {
        id: "agent-ping-http-error".into(),
        title: "agent-ping-http-error".into(),
        link_group: String::new(),
        bidir: false,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
    assert_eq!(detail.reason_code, ReasonCode::PingExecError);
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
        link_group: String::new(),
        bidir: false,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
                offered_per_stream_mbps: None,
            }),
        }],
        est_secs: 1,
    };
    let ping_unit = Unit {
        id: "mixed-ping".into(),
        title: "mixed-ping".into(),
        link_group: String::new(),
        bidir: false,
        bidir_total_target_mbps: None,
        target_lines: Vec::new(),
        direction: String::new(),
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
        reason_code: ReasonCode::IperfPreflightFailed,
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
        .any(|row| row.reason_code == ReasonCode::IperfPreflightFailed));
    assert!(rows
        .iter()
        .any(|row| row.reason_code == ReasonCode::GatewayNotFound));
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
        reason_code: ReasonCode::CtsSingleUdpStreamFailed,
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
        reason_code: ReasonCode::SingleUdpStreamFailed,
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
        judgement: VerdictResult::new(Verdict::Measured, ReasonCode::TargetUnknown, String::new()),
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
        offered_per_stream_mbps: None,
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
    first.setup_error = Some((
        ReasonCode::CtsProcessStartFailed,
        "attempt-one-error".into(),
    ));
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

/// UDP 路径必须和 TCP 路径同一口径：RX 平均达标就是 PASS，不被中间掉速
/// 或 TX 诊断指标改写；RX 平均不达标则按子网问题 FAIL。
#[test]
fn rx_average_is_the_only_rate_threshold_on_both_transports() {
    let target = 800.0;
    let raw = |rate_at: fn(u64) -> f64| -> Vec<(u64, u64, f64)> {
        (1..=180).map(|i| (i * 1_000, 1_000, rate_at(i))).collect()
    };
    let steady = raw(|_| 850.0);
    let dipped = raw(|i| if (20..=25).contains(&i) { 120.0 } else { 850.0 });
    let blip = raw(|i| if i == 20 { 0.0 } else { 850.0 });

    // TCP 路径
    let pass = RateStats {
        series: steady.clone(),
        ..healthy_stats(850.0)
    };
    let (verdict, _, _) = nic_rx(RateMode::Verify, Some(target), &pass);
    assert_eq!(verdict, Verdict::Pass, "全程稳定应当 PASS");

    let fails = RateStats {
        series: dipped.clone(),
        ..healthy_stats(850.0)
    };
    let (verdict, code, _) = nic_rx(RateMode::Verify, Some(target), &fails);
    assert_eq!((verdict, code), (Verdict::Pass, ReasonCode::None));

    let tolerated = RateStats {
        series: blip.clone(),
        ..healthy_stats(850.0)
    };
    let (verdict, _, _) = nic_rx(RateMode::Verify, Some(target), &tolerated);
    assert_eq!(verdict, Verdict::Pass, "一个采样周期的掉拍不该判 FAIL");

    // `rate_excursion` 仍保留为诊断函数，但不再参与正式 verdict。
    assert!(rate_excursion(&steady, target).is_none());
    assert!(rate_excursion(&blip, target).is_none());
    let excursion = rate_excursion(&dipped, target).expect("UDP 侧也要检出同一个坑");
    assert_eq!(excursion.reason_code(), ReasonCode::RxDropout);
    assert_eq!(excursion.longest_ms, 6_000);
    assert_eq!(excursion.extreme_mbps, 120.0);
}

/// 生产代码里造 `Row` 只能走 `base_row` / `unit_row`，不许再 `..Default::default()`。
///
/// 这条守的是 AGENTS.md §3 里那句「改报告列必须联检 executor **全部** Row 构造点，
/// 漏一个就是空列」。空列不会让任何测试变红——它只是在用户的报告里少一格，
/// 而那一格恰好是他要拿去验收的那个数。历史上报告加列就是这么漏过的。
///
/// `..Default::default()` 正是让「漏填」变得无声的那个语法：新字段自动取零值，
/// 编译器一句话都不说。改成走构造函数之后，新增身份字段会让 10 个构造点全部
/// 编译失败——从「运行期空列」变成「编译期错误」。
///
/// 照 `verdict_priority_has_exactly_one_definition_in_the_tree` 的样子写：
/// 扫源码，而不是靠人记得。
#[test]
fn every_production_row_is_built_through_the_shared_constructor() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/master");
    let mut offenders = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // 测试自己可以随便造 Row：它们是被测数据，不是产物。
            if path.file_name().and_then(|n| n.to_str()) == Some("tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            for (index, _) in text.match_indices("push_row(Row {") {
                let tail = &text[index..];
                let mut depth = 0usize;
                let mut end = tail.len();
                for (offset, ch) in tail.char_indices() {
                    match ch {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = offset;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let body = &tail[..end];
                let built_by_constructor =
                    body.contains("..base_row(") || body.contains("..unit_row(");
                if !built_by_constructor {
                    let line = text[..index].matches('\n').count() + 1;
                    offenders.push(format!("{}:{line}", path.display()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "这些 Row 构造点绕过了 base_row/unit_row，新增报告列时会变成空列：{offenders:#?}"
    );
}
