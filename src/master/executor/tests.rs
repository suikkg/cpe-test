//! `executor` 的测试。
//!
//! 单独成文件不是因为「太长」，而是因为它和产品码的变更节奏不同：这里累积的
//! 是一条条具体的现场回归（run_2026xxxx 的某个 unit 当时错在哪），而产品码
//! 改的是判定与执行结构。混在一个文件里，两种改动会互相制造无谓的冲突。

use super::*;
// 仅测试用到的采样统计层符号；产品码不需要，放这里避免非测试构建报未用导入。
use super::db::resume_age_is_fresh;
use crate::cmd::iperf_window::{iperf_effective_window, receiver_rate_over, ServerInterval};
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
                traffic: None,
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
        comparison_label: if udp {
            "CTS UDP -b 500m (每流)".into()
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
        round: 1,
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
        round: 1,
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
    let identity = row.comparison_identity.unwrap();
    assert!(!identity.legs.is_empty());
    assert_eq!(identity.legs[0].backend, RowBackend::CtsTraffic);
    assert_eq!(identity.round, 1);
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
    // 执行器会读进程级取消位（单元循环、腿内轮询、探针）并在单元边界吞掉跳过
    // 请求：凡是造了 Ctx 的用例都要和改这些标志的用例互斥，见 `cancel::test_guard`。
    crate::cancel::test_guard();
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
        agent_ping_df: true,
        agent_os: String::new(),
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

/// **IPv6 上不做路径 MTU 探测。**
///
/// IPv6 协议层面没有 DF 位，路由器一律不分片。`ping -f`（Windows，文档标注
/// IPv4-only）和 `ping6 -D`（macOS，ping6 不认）都给不出可信结果，据此得到的
/// 「大包能过」和旧 agent 静默忽略 DF 位是同一个错答案——而 `PING_DF_CAPABILITY`
/// 只认版本，认不出这一类。宁可没有结果。
#[test]
fn path_mtu_probing_refuses_ipv6_instead_of_returning_a_number_it_cannot_trust() {
    let (ctx, db_path) = isolated_ctx(1);
    let src = endpoint(Side::Master, "en0", "192.168.1.2");
    let dst = endpoint(Side::Agent, "en1", "192.168.1.3");
    // `capable = true`：能力标记这一关是过的，拦住它的只能是 IPv6 本身。
    let error = ctx
        .probe_path_mtu(&src, &dst, true, true)
        .expect_err("IPv6 不该给出一个 MTU 数字");
    assert!(error.contains("IPv6"), "错误要说清楚为什么不测：{error}");
    let _ = std::fs::remove_file(db_path);
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

#[test]
fn explicit_user_cancellation_survives_successful_remote_cleanup() {
    crate::cancel::test_guard();
    // 取消位是进程级共享状态；在独立测试进程驱动真实 RPC 分支，避免影响
    // 同时运行的吞吐测试，也不靠静态源码匹配代替行为验证。
    const CHILD_ENV: &str = "CPE_TEST_EXECUTOR_CANCEL_CHILD";
    if std::env::var(CHILD_ENV).as_deref() != Ok("1") {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "master::executor::tests::explicit_user_cancellation_survives_successful_remote_cleanup",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .expect("启动独立取消测试进程");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    for skip_current_unit in [false, true] {
        for cts in [false, true] {
            crate::cancel::reset();
            let clock = Arc::new(ManualClock::new());
            let transport =
                http_client::ScriptedTransport::with_handler(clock.clone(), move |request| {
                    let body = if request.path.ends_with("/start") {
                        if skip_current_unit {
                            crate::cancel::request_skip_unit();
                        } else {
                            crate::cancel::request_cancel();
                        }
                        ok_json(IperfClientStartOut {
                            id: "cancel-job".into(),
                            elapsed_ms: 0,
                        })
                    } else if request.path.ends_with("/stop") {
                        ok_json(IperfClientStopOut {
                            existed: true,
                            was_done: false,
                            terminated: true,
                            result: Some(IperfClientOut {
                                ok: true,
                                process_started: Some(true),
                                cleanup_confirmed: Some(true),
                                output: "已采集的部分输出".into(),
                                ..Default::default()
                            }),
                        })
                    } else {
                        return Err(format!("取消后不应继续调用 {}", request.path));
                    };
                    Ok(http_client::HttpResponse::new(200, body))
                });
            transport.push(http_client::ScriptedExchange::handler_response());
            transport.push(http_client::ScriptedExchange::handler_response());
            let (mut ctx, db_path) = isolated_ctx(0);
            ctx.transport = Arc::new(transport.clone());
            ctx.clock = clock;
            let client = if cts {
                let run = ctx.cts_client_run_tracked(
                    Side::Agent,
                    CtsTrafficStartReq {
                        request_id: "cancel-job".into(),
                        ..Default::default()
                    },
                    |_| {},
                );
                assert!(run.cleanup_confirmed);
                assert_eq!(
                    run.setup_error.unwrap().0,
                    ReasonCode::CtsClientUserCancelled
                );
                run.client
            } else {
                ctx.client_run_tracked(
                    Side::Agent,
                    &IperfClientReq::default(),
                    "cancel-owner",
                    "cancel-job",
                    0,
                    |_| {},
                )
            };
            assert!(client.cancelled, "cts={cts}, skip={skip_current_unit}");
            assert!(!client.ok);
            assert!(!client.timed_out, "显式取消不能冒充执行超时");
            assert_eq!(client.cleanup_confirmed, Some(true), "{client:?}");
            assert!(client.output.contains("已采集的部分输出"));
            assert!(client.output.contains("用户中断"));
            assert_eq!(transport.requests().len(), 2, "只允许启动和确认回收");
            assert_eq!(crate::cancel::is_stop_requested(), !skip_current_unit);
            if skip_current_unit {
                assert!(crate::cancel::take_skip_unit());
                assert!(crate::cancel::resume_after_skip());
            } else {
                assert!(!crate::cancel::resume_after_skip());
                assert!(crate::cancel::is_cancelled());
            }
            let _ = std::fs::remove_file(db_path);
            let _ = std::fs::remove_dir_all(&ctx.run_dir);
        }
    }
    crate::cancel::reset();
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
            comparison_label: "UDP -b 500m".into(),
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
        comparison_label: "TCP -w 64k -P 2".into(),
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
            traffic: None,
        },
        LegOutcome {
            judgement: VerdictResult::new(Verdict::RateFail, ReasonCode::RxBelowTarget, "ba low"),
            rx_avg: Some(780.0),
            main_rows: vec![ba_row],
            tag: "ba".into(),
            traffic: None,
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
        traffic: None,
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

#[cfg(unix)]
#[test]
fn result_db_save_does_not_follow_a_symlinked_temp_file() {
    let nonce = RESOURCE_OWNER_SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "cpe_db_symlink_test_{}_{}",
        std::process::id(),
        nonce
    ));
    let outside = std::env::temp_dir().join(format!(
        "cpe_db_symlink_outside_{}_{}",
        std::process::id(),
        nonce
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let path = dir.join("task_results.json");
    let target = outside.join("do-not-overwrite");
    std::fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, dir.join("task_results.tmp")).unwrap();

    let mut db = ResultDb::load(path.clone());
    db.set("agent", true, "test");
    db.save();

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    assert!(ResultDb::load(path).fresh_pass("agent").is_some());
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(outside);
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
    let window = cts_effective_window(&events, 10, 1_000, 0);
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
    let window = cts_effective_window(&events, 10, 1_000, 0);
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
    let window = cts_effective_window(&events, 10, 1_000, 0);
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
    let window = cts_effective_window(&events, 10, 1_000, 0);
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
    let server_window = cts_effective_window(&events, 10, 1_000, 0);
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
    let window = iperf_effective_window(&events, 180, 0, true);
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
                comparison_label: "UDP -b 500m".into(),
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
        round: 1,
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
    let rounded = cts_effective_window(&events, 10, 1_000, 0);
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
        0,
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
    let window = cts_effective_window(&events, 10, 1_000, 0);
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
    // 窗口内的读数失败按 RX 采样覆盖率处理，和 iperf 链同一口径，不单独否决；
    // 这一份样本全是无效的，覆盖率为零，验收层自己会判无法评价。
    assert!(cts_monitor_issue_verdict(&issue).is_none());
    let stats = monitor_rate_stats(&runtime, &window, true, 0);
    assert_eq!(
        evaluate_rx_acceptance(RateMode::Observe, None, &stats).verdict,
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

/// 窗口内一次读数失败：下一拍的字节差覆盖了缺口，覆盖率仍是 100%——和 iperf
/// 链一样照常判定，不再整行无法评价。
#[test]
fn a_single_failed_read_inside_the_cts_window_does_not_veto_the_verdict() {
    let window = EffectiveWindow {
        start_ms: 2_000,
        end_ms: 12_000,
        available_secs: 10.0,
        required_secs: 10,
        complete: true,
    };
    let samples: Vec<MonitorSample> = (3..=12)
        .map(|second| match second {
            6 => MonitorSample {
                elapsed_ms: 6_000,
                interval_ms: 1_000,
                valid: false,
                error: "GetIfTable2 transient failure".into(),
                ..Default::default()
            },
            // 恢复样本：字节差和时长都从上一次成功读数算起，覆盖 [5s, 7s)。
            7 => MonitorSample {
                elapsed_ms: 7_000,
                interval_ms: 2_000,
                rx_delta_bytes: 25_000_000,
                rx_mbps: 100.0,
                valid: true,
                ..Default::default()
            },
            _ => MonitorSample {
                elapsed_ms: second * 1_000,
                interval_ms: 1_000,
                rx_delta_bytes: 12_500_000,
                rx_mbps: 100.0,
                valid: true,
                ..Default::default()
            },
        })
        .collect();
    let output = MonitorStopOut {
        samples,
        errors: vec!["GetIfTable2 transient failure".into()],
        ..Default::default()
    };
    let issue = cts_monitor_runtime_issue(&output, &window).expect("runtime issue");
    assert!(cts_monitor_issue_verdict(&issue).is_none());
    let stats = monitor_rate_stats(&output, &window, true, 0);
    assert_eq!(stats.coverage, 1.0);
    assert_eq!(
        evaluate_rx_acceptance(RateMode::Verify, Some(90.0), &stats).verdict,
        Verdict::Pass
    );
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
        round: 1,
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
        Instant::now(),
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

    // 本机没有这个源地址：两种平台措辞都要认成执行环境问题（实机原文）。
    for local_bind_failure in [
        "iperf3: error - unable to connect to server - server may have stopped running or use a different port, firewall issue, etc.: Can't assign requested address",
        "iperf3: error - unable to connect to server - server may have stopped running or use a different port, firewall issue, etc.: Cannot assign requested address",
    ] {
        let client = IperfClientOut {
            process_started: Some(true),
            cleanup_confirmed: Some(true),
            output: local_bind_failure.into(),
            ..Default::default()
        };
        assert!(
            iperf_client_setup_error(&client).is_some(),
            "{local_bind_failure}"
        );
    }

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

/// 流数不足**不**让窗口作废（ADR-17，用户确认的口径 B）。
///
/// 窗口以前要求「同时活跃的流 ≥ 目标推算的必需流数」：2 条里掉 1 条，这条腿
/// 就判 EFFECTIVE_WINDOW_SHORT，哪怕剩下那条照样跑满、RX 也达标。TCP/CTS 对
/// 同一件事只记诊断——现在 UDP 也一样，掉流由 `udp_leg_diagnostics` 报出来。
#[test]
fn test_bidir_small_leg_keeps_its_window_when_one_of_its_streams_fails() {
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

    let fail = |results: &mut Vec<UdpFlowRun>, stream_pos: usize| {
        let flow = results
            .iter_mut()
            .find(|flow| flow.leg_pos == 1 && flow.stream_pos == stream_pos)
            .unwrap();
        flow.raw_ok = false;
        flow.events.clear();
    };
    fail(&mut results, 1);
    let windows =
        select_udp_effective_windows(&plans, &results, &monitors, &RateCheckCfg::default());
    // 2 条掉 1 条：剩下那条全程在跑，窗口照样完整，结论交给 RX。
    assert!(windows.per_leg[1].complete, "{:?}", windows.per_leg[1]);
    assert_eq!(windows.per_leg[1].available_secs, 184.0);
    assert_eq!(windows.concurrency_secs, 180.0);

    // 小腿两条全掉：这条腿一刻都没有流，没结论——这一条不变。
    fail(&mut results, 0);
    let windows =
        select_udp_effective_windows(&plans, &results, &monitors, &RateCheckCfg::default());
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
    let run = |ba_ends: [u64; 2]| {
        let mut results = Vec::new();
        for (leg_pos, plan) in plans.iter().enumerate() {
            for (stream_pos, task) in plan.streams.iter().enumerate() {
                let end_ms = if leg_pos == 1 {
                    ba_ends[stream_pos]
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
        select_udp_effective_windows(&plans, &results, &monitors, &RateCheckCfg::default())
    };
    // ba 腿两条流都在 175s 停了，只有这条腿的窗口被截短。
    let windows = run([175_000, 175_000]);
    assert!(!windows.per_leg[1].complete);
    assert_eq!(windows.per_leg[1].available_secs, 169.0);
    // ab 腿全程正常，不受影响。
    assert!(windows.per_leg[0].complete);
    assert_eq!(windows.per_leg[0].available_secs, 184.0);
    // 两条腿确实重叠过，重叠时长取交集。
    assert_eq!(windows.concurrency_secs, 169.0);

    // 只停一条：另一条还在跑，窗口不截短——掉流只作诊断。
    let windows = run([190_000, 175_000]);
    assert!(windows.per_leg[1].complete);
    assert_eq!(windows.per_leg[1].available_secs, 184.0);
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

#[test]
fn udp_effective_window_keeps_exact_flow_and_monitor_boundaries() {
    let master = endpoint(Side::Master, "master0", "192.168.1.2");
    let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
    let plans = vec![udp_plan(0, "ab", 1, &master, &agent, 10)];
    let task = &plans[0].streams[0];
    for (start, end, monitor_end, expected_start, expected_end, complete) in [
        // 差 200ms 确实短；不能通过 1 秒网格补成完整十秒。
        (1_000, 15_800, 20_000, 6_000, 15_800, false),
        // 差 50ms 可认完整，但测量终点仍不能超出实际起流区间。
        (1_000, 15_950, 20_000, 6_000, 15_950, true),
        // 起点同样保留毫秒，不丢掉起流后的前 750ms。
        (1_250, 17_100, 20_000, 6_250, 16_250, true),
        // monitor 已结束，不能凭空在最后一个样本后补一秒。
        (1_000, 19_000, 15_000, 6_000, 15_000, false),
    ] {
        let results = vec![udp_flow(0, 0, task, start, end, true)];
        let monitors = HashMap::from([(agent.key(), monitor_until(monitor_end, 1_000.0, 1_000.0))]);
        let windows =
            select_udp_effective_windows(&plans, &results, &monitors, &RateCheckCfg::default());
        let window = &windows.per_leg[0];
        assert_eq!(
            (window.start_ms, window.end_ms, window.complete),
            (expected_start, expected_end, complete),
            "flow={start}..{end}, monitor_end={monitor_end}"
        );
        assert!(window.end_ms <= end.min(monitor_end));
    }
}

#[test]
fn udp_window_sample_index_preserves_gaps_despite_duplicates_and_reordering() {
    let master = endpoint(Side::Master, "master0", "192.168.1.2");
    let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
    let plans = vec![udp_plan(0, "ab", 1, &master, &agent, 20)];
    let results = vec![udp_flow(0, 0, &plans[0].streams[0], 1_250, 32_100, true)];
    let mut ordered = monitor_until(35_000, 1_000.0, 1_000.0);
    for sample in &mut ordered.samples {
        sample.valid = !(10_000..=17_000).contains(&sample.elapsed_ms);
    }
    let mut duplicated_and_reordered = ordered.clone();
    duplicated_and_reordered
        .samples
        .extend(ordered.samples.clone());
    duplicated_and_reordered.samples.reverse();
    let mut absent = ordered.clone();
    absent.samples.retain(|sample| sample.valid);
    let empty = MonitorStopOut::default();
    for (monitor, expected) in [
        (ordered, (21_000, 32_100, 11.1)),
        (duplicated_and_reordered, (21_000, 32_100, 11.1)),
        (absent, (21_000, 32_100, 11.1)),
        (empty, (0, 0, 0.0)),
    ] {
        let monitors = HashMap::from([(agent.key(), monitor)]);
        let windows =
            select_udp_effective_windows(&plans, &results, &monitors, &RateCheckCfg::default());
        let window = &windows.per_leg[0];
        assert_eq!(
            (window.start_ms, window.end_ms, window.available_secs),
            expected,
            "9s 与 18s 样本之间的缺口超出 2s 容差；后半段从 16s 起、21s 完成 settle"
        );
        assert!(!window.complete);
    }
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
        window_start_ms: 0,
        baseline_mbps: 0.0,
        stalled_ratio: 0.0,
        ..Default::default()
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
        setup_error: None,
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
        setup_error: None,
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
        setup_error: None,
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
        .find("breaker.should_abort_all()")
        .expect("熔断检查必须在单元循环内");
    // 找不到 `continue;` 时不能取 usize::MAX：那样下面的断言恒真，循环结构一改，
    // 这条守卫就什么都不检查地通过了。提前退出的路径（resume 命中、前置拦截、
    // 网卡消失）一定在；不在了说明结构变了，这条断言要跟着改写。
    let first_continue = loop_body
        .find("continue;")
        .expect("单元循环里应当有提前 continue 的路径；循环结构变了就改写这条断言");
    assert!(
        gate < first_continue,
        "熔断检查必须排在任何 continue 之前，否则提前退出的路径会绕过它"
    );
    assert_eq!(
        loop_body.matches("breaker.should_abort_all()").count(),
        1,
        "只能有一处熔断检查；两处必然会漂移"
    );
    // 阈值只能从一处读进状态机。执行循环里再读一次 `self.cfg` 就是又开了一条
    // 旁路——这正是分组那一层被漏掉之前的形状。
    assert_eq!(
        source
            .matches("self.cfg.abort_after_dead_traffic_units")
            .count(),
        1,
        "阈值只能在构造 DeadTrafficBreaker 时读一次"
    );
}

/// 分组熔断的状态机穷举。
///
/// 这一层**行为上测不到**：它和全局那一层的区别只在「还有链路活着」时才显现，
/// 而那需要真实流量。所以把它做成纯状态机，在这里把四象限走一遍。
#[test]
fn the_dead_traffic_breaker_drops_one_link_without_stopping_the_others() {
    let mut breaker = DeadTrafficBreaker::new(2);

    // A 连着两个空跑 → 只放弃 A。B 一直在出数，全局计数被它清零。
    assert!(!breaker.record_dead("A"), "第一个空跑还不到阈值");
    breaker.record_usable("B");
    assert!(breaker.record_dead("A"), "A 的第二个空跑刚好越过阈值");
    assert!(breaker.is_abandoned("A"));
    assert!(!breaker.is_abandoned("B"), "B 一直在出数，不该被牵连");
    assert!(
        !breaker.should_abort_all(),
        "只有一条链路坏掉时，整队不许停——这正是分组要解决的那件事"
    );

    // 「刚好越过」只报一次：每个单元都刷一行日志等于没有日志。
    assert!(!breaker.record_dead("A"), "已经放弃的链路不再重复报告");

    // B 也跟着死够两个 → 全局那一层接住，整队停。
    breaker.record_dead("B");
    breaker.record_dead("B");
    assert!(breaker.is_abandoned("B"));
    assert!(
        breaker.should_abort_all(),
        "所有链路都死掉时，全局那一层必须中止整队"
    );
}

#[test]
fn a_zero_threshold_keeps_both_layers_off_and_still_counts_for_the_report() {
    // 默认值 0 = 只告警不中止。两层都不许动，但报告顶部的「最长空跑连击」
    // 仍然要数——那句话是靠它写出来的。
    let mut breaker = DeadTrafficBreaker::new(0);
    for _ in 0..10 {
        breaker.record_dead("A");
    }
    assert!(!breaker.should_abort_all());
    assert!(!breaker.is_abandoned("A"));
    assert_eq!(breaker.max_global_streak(), 10);
}

#[test]
fn an_empty_link_key_never_becomes_a_group_of_its_own() {
    // 键为空意味着分不出组。拿它当一个组，会把一批互不相干的链路一起放弃。
    let mut breaker = DeadTrafficBreaker::new(2);
    assert!(!breaker.record_dead(""));
    assert!(!breaker.record_dead(""));
    assert!(!breaker.is_abandoned(""), "空键永远不算被放弃");
    // 但全局那一层照数不误：整台设备掉线时，链路键是不是空的无关紧要。
    assert!(breaker.should_abort_all());
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
        setup_error: None,
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
        setup_error: None,
        rx_monitor: None,
    });
    let detail = judged.detail;
    assert!(!detail.contains("全程"), "{detail}");
}

/// 窗口没攒够就失败的，要分清两件事：
///
/// - 执行环境没搭起来（进程没起来、回收未确认、被取消、参数错误）→ SETUP_ERROR；
/// - 跑出过流量、环境也没问题，只是 iperf3 中途退出（被测设备重启、链路断开）→
///   有效窗口不足。以前这一类也判 SETUP_ERROR，而 UDP 与 CTS 对同一件事判的是
///   窗口不足；熔断计数还会把它记成「一个测量都没产生」。
#[test]
fn a_client_that_exits_after_traffic_is_a_short_window_not_a_setup_error() {
    let rx = healthy_stats(500.0);
    let short = EffectiveWindow {
        start_ms: 0,
        end_ms: 12_000,
        available_secs: 12.0,
        required_secs: 180,
        complete: false,
    };
    let judge = |setup_error: Option<String>| {
        iperf_flow_verdict(IperfFlowVerdictIn {
            raw_ok: false,
            measurement: true,
            effective_window: &short,
            required_secs: 180,
            rate_mode: RateMode::Observe,
            rx_target_mbps: None,
            rx_stats: &rx,
            tx_stats: &rx,
            offered_floor: None,
            client_tail: "iperf3: error - control socket has closed unexpectedly",
            setup_error,
            rx_monitor: None,
        })
    };

    let exited = judge(None);
    assert_eq!(exited.verdict, Verdict::NotEvaluated);
    assert_eq!(exited.code, ReasonCode::IperfEffectiveWindowShort);
    assert!(exited.detail.contains("中途退出"), "{}", exited.detail);

    let broken = judge(Some("client 进程回收未确认".into()));
    assert_eq!(broken.verdict, Verdict::SetupError);
    assert_eq!(broken.code, ReasonCode::IperfExecFailed);
    assert!(broken.detail.contains("回收未确认"), "{}", broken.detail);
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
    let window = iperf_effective_window(&flow.events, 180, 0, true);
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

/// 进程实际跑多久：多跑一段起流爬升（判定窗口扣掉它），配了双向合计门限的单元
/// 再多跑一小段凑交集；iperf UDP 组的爬升由组调度器自己扣，这里不重复。
#[test]
fn traffic_processes_run_past_the_required_duration_only_where_needed() {
    let cfg = Config::default();
    let settle = cfg.iperf.rate_check.settle_secs;
    let margin = crate::cmd::iperf_window::BIDIR_OVERLAP_MARGIN_SECS;
    assert!(settle > 0, "默认就要扣起流爬升");

    let mut total = ctstraffic_unit("margin", false);
    total.bidir = true;
    total.bidir_total_target_mbps = Some(1_500.0);
    let mut per_direction = total.clone();
    per_direction.bidir_total_target_mbps = None;
    let mut one_way = ctstraffic_unit("margin-one-way", false);
    one_way.bidir_total_target_mbps = Some(1_500.0);

    let secs = |unit: &Unit| {
        traffic_process_secs(180, traffic_settle_secs(&cfg), needs_overlap_margin(unit))
    };
    assert_eq!(secs(&total), 180 + settle + margin);
    assert_eq!(secs(&per_direction), 180 + settle);
    assert_eq!(secs(&one_way), 180 + settle);
}

/// TCP 起流爬升不进平均：窗口从真实流量起点扣掉 settle，再截出要求时长。
///
/// 以前 TCP 窗口从 iperf3 的 0.00 秒算起，慢启动和窗口增长那几秒的低速全进了
/// 平均，Wi-Fi 上能压低一两个百分点；UDP 早就扣了 settle，两条链口径不一。
#[test]
fn the_tcp_ramp_is_cut_from_the_window() {
    // client 跑 185 秒（要求 180 + settle 5），汇总行按时到达，偏移 1_000。
    let events = vec![
        IperfFlowEvent {
            kind: IperfEventKind::Started,
            elapsed_ms: 500,
            ..Default::default()
        },
        IperfFlowEvent {
            kind: IperfEventKind::Traffic,
            elapsed_ms: 2_000,
            mbps: Some(300.0),
            line: "[  5]   0.00-1.00   sec  35.8 MBytes   300 Mbits/sec".into(),
        },
        IperfFlowEvent {
            kind: IperfEventKind::Traffic,
            elapsed_ms: 186_000,
            mbps: Some(880.0),
            line: "[  5]   0.00-185.00 sec  19.0 GBytes   880 Mbits/sec   sender".into(),
        },
        IperfFlowEvent {
            kind: IperfEventKind::Ended,
            elapsed_ms: 186_300,
            ..Default::default()
        },
    ];
    // 前 5 秒爬升（300），之后稳定 900。
    let output = MonitorStopOut {
        samples: (1..=187)
            .map(|second| {
                let mbps = if second <= 6 { 300.0 } else { 900.0 };
                MonitorSample {
                    elapsed_ms: second * 1_000,
                    interval_ms: 1_000,
                    rx_delta_bytes: (mbps * 125_000.0) as u64,
                    rx_mbps: mbps,
                    valid: true,
                    ..Default::default()
                }
            })
            .collect(),
        ..Default::default()
    };

    let settled = iperf_effective_window(&events, 180, 5, true);
    assert!(settled.complete, "{settled:?}");
    assert_eq!((settled.start_ms, settled.end_ms), (6_000, 186_000));
    let stats = monitor_rate_stats(&output, &settled, true, 500);
    assert_eq!(stats.avg_mbps, Some(900.0));

    // 不扣的话窗口从 1_000 开始，爬升段进了平均。
    let raw = iperf_effective_window(&events, 180, 0, true);
    let with_ramp = monitor_rate_stats(&output, &raw, true, 500);
    assert!(with_ramp.avg_mbps.unwrap() < 900.0);

    // 进程没多跑（只跑了要求时长）就凑不够：扣掉爬升后窗口不完整。
    let mut short = events.clone();
    short[2].line = "[  5]   0.00-180.00 sec  18.5 GBytes   880 Mbits/sec   sender".into();
    short[2].elapsed_ms = 181_000;
    short[3].elapsed_ms = 181_300;
    assert!(!iperf_effective_window(&short, 180, 5, true).complete);
}

/// CTS TCP 同样扣起流爬升；事件证据要覆盖「要求时长 + 爬升」才算完整。
#[test]
fn the_cts_window_cuts_the_ramp_and_needs_it_covered() {
    let events = |ended_ms: u64| {
        vec![
            IperfFlowEvent {
                kind: IperfEventKind::Started,
                elapsed_ms: 500,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Connected,
                elapsed_ms: 1_000,
                ..Default::default()
            },
            IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: ended_ms,
                ..Default::default()
            },
        ]
    };
    let window = cts_effective_window(&events(16_000), 10, 1_000, 5);
    assert!(window.complete, "{window:?}");
    assert_eq!((window.start_ms, window.end_ms), (6_000, 16_000));

    let short = cts_effective_window(&events(12_000), 10, 1_000, 5);
    assert!(!short.complete, "{short:?}");
    assert_eq!(short.start_ms, 6_000);
}

/// `count` 条逐秒行（`--forceflush` 下按时到达），**不带**汇总行。
fn interval_only_events(count: u64, ended_at_ms: u64) -> Vec<IperfFlowEvent> {
    let mut events = vec![
        IperfFlowEvent {
            kind: IperfEventKind::Started,
            elapsed_ms: 1_000,
            ..Default::default()
        },
        IperfFlowEvent {
            kind: IperfEventKind::Connected,
            elapsed_ms: 1_100,
            ..Default::default()
        },
    ];
    events.extend((0..count).map(|second| IperfFlowEvent {
        kind: IperfEventKind::Traffic,
        elapsed_ms: 1_200 + (second + 1) * 1_000 + 30,
        mbps: Some(941.0),
        line: format!(
            "[  5] {second}.00-{}.00  sec   112 MBytes   941 Mbits/sec",
            second + 1
        ),
    }));
    events.push(IperfFlowEvent {
        kind: IperfEventKind::Ended,
        elapsed_ms: ended_at_ms,
        ..Default::default()
    });
    events
}

/// iperf3 在发 TEST_END 时连接被重置，会在打印汇总行**之前**退出。
///
/// 以前窗口取「最长的一行」，逐秒行每行 1 秒，跑满 180 秒的测量只剩最后
/// 1 秒——整段数据被当成窗口不足，`IPERF_SUMMARY_LOST` 那条保住网卡口径的
/// 路径在这种最常见的形态下永远走不到。
#[test]
fn a_full_run_without_a_summary_line_keeps_its_whole_window() {
    let events = interval_only_events(180, 1_200 + 180_000 + 400);
    let window = iperf_effective_window(&events, 180, 0, true);
    assert!(window.complete, "{window:?}");
    assert_eq!(window.end_ms - window.start_ms, 180_000);
    assert!((window.available_secs - 180.0).abs() < 1e-9);

    // 端到端：收尾失败、窗口完整 → 判定仍然只看网卡口径。
    let rx = healthy_stats(1_067.902);
    let judged = iperf_flow_verdict(IperfFlowVerdictIn {
        raw_ok: false,
        measurement: true,
        effective_window: &window,
        required_secs: 180,
        rate_mode: RateMode::Verify,
        rx_target_mbps: Some(1_000.0),
        rx_stats: &rx,
        tx_stats: &rx,
        offered_floor: None,
        client_tail: TAIL_HANDSHAKE_ERROR,
        setup_error: None,
        rx_monitor: None,
    });
    assert_eq!(judged.verdict, Verdict::Pass, "{}", judged.detail);
    assert!(
        judged.detail.contains("IPERF_SUMMARY_LOST"),
        "{}",
        judged.detail
    );
}

/// 中途退出的测量照样是短的：逐秒行只覆盖到第 10 秒，窗口就只有 10 秒。
#[test]
fn a_run_that_aborted_early_without_a_summary_stays_short() {
    let events = interval_only_events(10, 1_200 + 10_000 + 400);
    let window = iperf_effective_window(&events, 180, 0, true);
    assert!(!window.complete);
    assert!((window.available_secs - 10.0).abs() < 1e-9, "{window:?}");
}

/// server 输出的逐秒行投影到监控时间轴；同一个 server 上 client 重试之前那次
/// 测试的行要丢掉；UDP 不填发送端（恒速发包证明不了对端收没收到）。
#[test]
fn the_iperf_tool_trace_reads_the_server_timeline_of_the_last_test_only() {
    let events = interval_only_events(180, 1_200 + 180_000 + 400);
    let mut server = String::from(
        "Server listening on 5201\n\
         [  5]   0.00-1.00   sec  50.0 MBytes   419 Mbits/sec\n\
         [  5]   1.00-2.00   sec  50.0 MBytes   419 Mbits/sec\n\
         Accepted connection from 192.168.1.2\n",
    );
    for second in 0..180u64 {
        let rate = if (60..80).contains(&second) {
            "0.00 Bytes  0.00 bits/sec"
        } else {
            "112 MBytes   941 Mbits/sec"
        };
        server.push_str(&format!(
            "[  5] {second}.00-{}.00  sec  {rate}\n",
            second + 1
        ));
    }
    server.push_str("[  5]   0.00-180.04 sec  19.7 GBytes   941 Mbits/sec   receiver\n");

    // 每条逐秒行到达时刻 = 1_200 + 行终点 + 30，偏移就是 1_230。
    let server = server_intervals(&server);
    let tcp = iperf_tool_trace(&events, &server, 180, false);
    assert_eq!(
        tcp.receiver.reported.len(),
        180,
        "只留最后一次测试，汇总行不算"
    );
    assert_eq!(tcp.receiver.reported[0], (1_230, 2_230));
    assert_eq!(tcp.receiver.flowing.len(), 160);
    assert!(!tcp.receiver.flowing.contains(&(61_230, 62_230)));
    assert_eq!(tcp.sender.reported, vec![(1_230, 181_230)]);
    assert_eq!(tcp.sender.flowing.len(), 180);
    assert_eq!(
        tcp.stall_evidence((61_230, 81_230)),
        crate::master::rate_window::StallEvidence::TrafficStopped,
        "接收端说这 20 秒没收到，就是真断流"
    );

    // iperf3 3.1.x 的 UDP 汇总行不带 sender/receiver 字样：按区间长度认出来，
    // 不当成一条横跨全程的「逐秒行」。
    // 这一条从 0 秒开始，混进来还会触发「新测试从 0 重新计时」把前面的逐秒行清掉。
    let old_udp_summary = server_intervals(
        "[  5]   0.00-1.00   sec   112 MBytes   941 Mbits/sec  0.010 ms  0/80000 (0%)\n\
         [  5]   1.00-2.00   sec  0.00 Bytes  0.00 bits/sec  0.010 ms  0/0 (0%)\n\
         [  5]   2.00-3.00   sec   112 MBytes   941 Mbits/sec  0.010 ms  0/80000 (0%)\n\
         [  5]   0.00-3.00   sec   224 MBytes   627 Mbits/sec  0.010 ms  0/160000 (0%)\n",
    );
    assert_eq!(old_udp_summary.len(), 3, "{old_udp_summary:?}");
    assert!(old_udp_summary
        .iter()
        .all(|line| line.end_ms - line.start_ms <= 1_000));
    assert_eq!(old_udp_summary[1].mbps, 0.0, "断流那一秒必须留着");

    let udp = iperf_tool_trace(&events, &server, 180, true);
    assert!(udp.sender.reported.is_empty() && udp.sender.flowing.is_empty());
    assert_eq!(udp.receiver.flowing.len(), 160);
}

/// 工具口径能裁到任意时间段的只有接收端逐秒记录：在判定窗口上求时间加权平均，
/// 多流只认 `[SUM]`，覆盖不够或对不上时钟就拒绝，不猜。
#[test]
fn the_receiver_rate_over_a_window_comes_from_server_interval_lines() {
    let events = interval_only_events(30, 1_200 + 30_000 + 400);
    // 偏移 1_230（见 interval_only_events）；前 5 秒爬升 300，之后 900。
    let single: String = (0..30u64)
        .map(|second| {
            let rate = if second < 5 { "300" } else { "900" };
            format!(
                "[  5] {second}.00-{}.00  sec  100 MBytes  {rate} Mbits/sec\n",
                second + 1
            )
        })
        .collect();
    let lines = server_intervals(&single);
    let steady = (6_230, 26_230);
    assert_eq!(receiver_rate_over(&events, &lines, 1, steady), Ok(900.0));
    let with_ramp = receiver_rate_over(&events, &lines, 1, (1_230, 21_230)).unwrap();
    assert!(with_ramp < 900.0, "{with_ramp}");

    // 多流：只认 [SUM] 行，逐流行不重复计。
    let multi: String = (0..30u64)
        .map(|second| {
            let next = second + 1;
            format!(
                "[  5] {second}.00-{next}.00 sec 50 MBytes 450 Mbits/sec\n\
                 [  7] {second}.00-{next}.00 sec 50 MBytes 450 Mbits/sec\n\
                 [SUM] {second}.00-{next}.00 sec 100 MBytes 900 Mbits/sec\n"
            )
        })
        .collect();
    assert_eq!(
        receiver_rate_over(&events, &server_intervals(&multi), 2, steady),
        Ok(900.0)
    );
    // 多流却没有 [SUM] 行：不拿单流行冒充合计。
    assert!(receiver_rate_over(&events, &lines, 2, steady).is_err());

    // 中段缺了 10 秒记录：覆盖不足，拒绝。
    let gapped: Vec<ServerInterval> = lines
        .iter()
        .copied()
        .filter(|line| !(10_000..20_000).contains(&line.start_ms))
        .collect();
    let error = receiver_rate_over(&events, &gapped, 1, steady).unwrap_err();
    assert!(error.contains("覆盖"), "{error}");

    // client 一条逐秒行都没有：对不上时钟，拒绝。
    let no_clock = vec![events[0].clone(), events.last().unwrap().clone()];
    assert!(receiver_rate_over(&no_clock, &lines, 1, steady).is_err());
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
    let window = iperf_effective_window(&events, 10, 0, true);
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

    let missing = iperf_effective_window(&events, 10, 0, false);
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
            traffic: None,
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
            traffic: None,
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
            traffic: None,
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
            traffic: None,
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
            traffic: None,
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
            traffic: None,
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
        comparison_label: "TCP -w 64k".into(),
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
        round: 1,
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
        round: 1,
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
            traffic: None,
        },
        LegOutcome {
            judgement: VerdictResult::new(Verdict::Pass, ReasonCode::None, String::new()),
            rx_avg: Some(500.0),
            main_rows: vec![ba_row],
            tag: "ba".into(),
            traffic: None,
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
        round: 1,
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
        traffic: None,
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
        round: 1,
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
        traffic: None,
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
        round: 1,
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
        round: 1,
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
        round: 1,
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
    let persisted = std::fs::read_to_string(ctx.run_dir.join(crate::report::store::ROWS_FILE))
        .expect("RESUME 跳过也必须进入增量 JSONL");
    assert!(persisted.contains("RESUME_FRESH_PASS"));
    drop(rows);
    let _ = std::fs::remove_file(db_path);
}

#[test]
fn preflight_block_takes_priority_over_resume_pass() {
    let master = endpoint(Side::Master, "master0", "192.168.1.2");
    let agent = endpoint(Side::Agent, "agent0", "192.168.1.3");
    let unit = Unit {
        round: 1,
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
                comparison_label: "TCP -w 64k".into(),
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
    crate::cancel::test_guard();
    let ctx = Ctx {
        agent_ping_df: true,
        agent_os: String::new(),
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
        round: 1,
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
        round: 1,
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
        round: 1,
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
        round: 1,
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
                comparison_label: "TCP".into(),
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
        round: 1,
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
        traffic: None,
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
        comparison_label: "TCP -w 1m -P 5".into(),
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

#[cfg(unix)]
#[test]
fn output_artifact_does_not_follow_a_symlinked_temp_file() {
    let nonce = RESOURCE_OWNER_SEQ.fetch_add(1, Ordering::SeqCst);
    let run_dir = std::env::temp_dir().join(format!(
        "cpe_run_artifact_symlink_test_{}_{}",
        std::process::id(),
        nonce
    ));
    let outdir = run_dir.join("iperf_outputs");
    let outside = std::env::temp_dir().join(format!(
        "cpe_run_artifact_symlink_outside_{}_{}",
        std::process::id(),
        nonce
    ));
    std::fs::create_dir_all(&outdir).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("do-not-overwrite");
    std::fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, outdir.join(".artifact.log.tmp")).unwrap();

    let (mut ctx, db_path) = isolated_ctx(0);
    ctx.outdir = outdir.clone();
    let link = ctx.write_output_artifact("artifact.log", "new", "测试附件");

    assert_eq!(link, "./iperf_outputs/artifact.log");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    assert_eq!(
        std::fs::read_to_string(outdir.join("artifact.log")).unwrap(),
        "new"
    );
    let _ = std::fs::remove_dir_all(run_dir);
    let _ = std::fs::remove_dir_all(outside);
    let _ = std::fs::remove_file(db_path);
}

#[test]
fn screenshot_filenames_have_a_process_local_sequence_beyond_second_precision() {
    let first = screenshot_filename(
        "task",
        Side::Master,
        SCREENSHOT_SEQ.fetch_add(1, Ordering::Relaxed),
    );
    let second = screenshot_filename(
        "task",
        Side::Master,
        SCREENSHOT_SEQ.fetch_add(1, Ordering::Relaxed),
    );
    assert_ne!(first, second);
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
fn independent_monitor_snapshots_do_not_overwrite_saved_samples() {
    let (mut ctx, db_path) = isolated_ctx(0);
    let nonce = RESOURCE_OWNER_SEQ.fetch_add(1, Ordering::SeqCst);
    let run_dir = std::env::temp_dir().join(format!(
        "cpe_monitor_snapshot_test_{}_{}",
        std::process::id(),
        nonce
    ));
    ctx.outdir = run_dir.join("iperf_outputs");
    let first = MonitorStopOut {
        avg_mbps: 100.0,
        tx_avg_mbps: 90.0,
        seconds: 1.0,
        bytes: 12_500_000,
        tx_bytes: 11_250_000,
        samples: vec![],
        errors: vec![],
    };
    let mut second = first.clone();
    second.avg_mbps = 200.0;
    let first_link = ctx.save_monitor_samples(
        "bidir-unit",
        Side::Agent,
        "en0",
        "agent-endpoint",
        137,
        &first,
    );
    let second_link = ctx.save_monitor_samples(
        "bidir-unit",
        Side::Agent,
        "en0",
        "agent-endpoint",
        137,
        &second,
    );
    assert_ne!(first_link, second_link);
    assert_eq!(
        std::fs::read_to_string(run_dir.join(first_link.trim_start_matches("./"))).unwrap(),
        build_monitor_samples_csv(Side::Agent.cn(), "en0", 137, &first)
    );
    assert_eq!(
        std::fs::read_to_string(run_dir.join(second_link.trim_start_matches("./"))).unwrap(),
        build_monitor_samples_csv(Side::Agent.cn(), "en0", 137, &second)
    );
    let shifted_link = ctx.save_monitor_samples(
        "bidir-unit",
        Side::Agent,
        "en0",
        "agent-endpoint",
        138,
        &first,
    );
    assert_ne!(first_link, shifted_link);
    assert_eq!(
        first_link,
        ctx.save_monitor_samples(
            "bidir-unit",
            Side::Agent,
            "en0",
            "agent-endpoint",
            137,
            &first
        )
    );
    let _ = std::fs::remove_dir_all(run_dir);
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
    assert!(rate_excursion(&steady, target, 0).is_none());
    assert!(rate_excursion(&blip, target, 0).is_none());
    let excursion = rate_excursion(&dipped, target, 0).expect("UDP 侧也要检出同一个坑");
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

#[test]
fn socket_buffer_drain_does_not_drag_the_window_past_the_end_of_traffic() {
    // 现场回归：run_20260905_125327_5940 的 unit-112（★★双向 V4 TCP，
    // 主控 以太网 5 ↔ 辅测 以太网 18）的 ba 腿。
    //
    // `-w 256m -P 10` = 2.56GB socket 缓冲，client 的 `-t 60` 到点后还要十几秒
    // 排空；末尾两条逐秒行连同汇总行一起压到 74.635s 才吐出来。只用行内时长、
    // 拿汇总行到达时刻当锚点的话，窗口会变成 [14.625s, 74.625s]——比真实流量
    // 后移 12.4 秒，掐掉开头 1300~1840Mbps 的高速段、把结尾没有流量的尾巴收进
    // 来，RX 平均从 1036 被压到 705.1（iperf3 自报接收端 1017）。
    let mut events = vec![
        IperfFlowEvent {
            kind: IperfEventKind::Started,
            elapsed_ms: 656,
            line: "started".into(),
            ..Default::default()
        },
        IperfFlowEvent {
            kind: IperfEventKind::Connected,
            elapsed_ms: 2_252,
            line: "connected".into(),
            ..Default::default()
        },
    ];
    // 逐秒 interval 行按时到达：到达时刻 − 行内终点 ≈ 2.25s，就是真实偏移。
    events.push(IperfFlowEvent {
        kind: IperfEventKind::Traffic,
        elapsed_ms: 59_246,
        mbps: Some(194.0),
        line: "[SUM]  56.01-57.00  sec  23.0 MBytes   194 Mbits/sec".into(),
    });
    events.push(IperfFlowEvent {
        kind: IperfEventKind::Traffic,
        elapsed_ms: 60_253,
        mbps: Some(162.0),
        line: "[SUM]  57.00-58.00  sec  19.2 MBytes   162 Mbits/sec".into(),
    });
    // 排空期间的停顿：剩下的行全部在 74.635s 成块到达。
    for line in [
        "[SUM]  58.00-59.01  sec  28.1 MBytes   234 Mbits/sec",
        "[SUM]  59.01-60.00  sec  24.8 MBytes   209 Mbits/sec",
        "[SUM]   0.00-60.00  sec  9.48 GBytes  1357 Mbits/sec                  sender",
        "[SUM]   0.00-60.01  sec  7.11 GBytes  1017 Mbits/sec                  receiver",
    ] {
        events.push(IperfFlowEvent {
            kind: IperfEventKind::Traffic,
            elapsed_ms: 74_635,
            mbps: Some(1_017.0),
            line: line.into(),
        });
    }
    events.push(IperfFlowEvent {
        kind: IperfEventKind::Ended,
        elapsed_ms: 74_641,
        line: "ended".into(),
        ..Default::default()
    });

    let window = iperf_effective_window(&events, 60, 0, true);
    assert!(window.complete, "60 秒测量必须判成完整窗口: {window:?}");
    // 汇总行的行内区间 0.00-60.01 投影回监控时间轴 = [2.246s, 62.256s]，
    // 判定窗口取其中前 60 秒。
    assert_eq!((window.start_ms, window.end_ms), (2_246, 62_246));
    // 老口径会落在这里；它越过了 70s 的流量末端。
    assert_ne!(window.start_ms, 14_625);
    assert!(
        window.end_ms < 70_000,
        "判定窗口不能越过流量末端，否则末尾零增长会凑成 COUNTER_STALLED: {window:?}"
    );
}

#[test]
fn clock_offset_falls_back_to_arrival_time_when_every_line_arrives_in_one_block() {
    // 老版 iperf3 没有 --forceflush：全部输出在退出时一次性吐出，一条按时到达
    // 的行都没有。此时偏移只能由汇总行自己给出，口径与 v6.2.5 及以前一致。
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
        IperfFlowEvent {
            kind: IperfEventKind::Traffic,
            elapsed_ms: 12_400,
            mbps: Some(100.0),
            line: "[  5]   9.00-10.00 sec  11.9 MBytes  100 Mbits/sec".into(),
        },
        IperfFlowEvent {
            kind: IperfEventKind::Traffic,
            elapsed_ms: 12_400,
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
    let window = iperf_effective_window(&events, 10, 0, true);
    assert_eq!((window.start_ms, window.end_ms), (2_400, 12_400));
    assert!(window.complete);
}

/// **两个出口对同一个词必须给出同一个数**（回归方案 OUT-01）。
///
/// `RunCounts` 的文档注释写着「字段与 `RunSummary` 同名同义，不另起炉灶」，
/// 而实际上 `RunSummary::fail` 曾经是个汇总口径（RATE_FAIL + NOT_EVALUATED +
/// SETUP_ERROR），`RunCounts::fail` 只数 RATE_FAIL。现场实测撞上过：
/// 一轮 6 个单元里 RATE_FAIL 一条都没有，命令行却打印「FAIL: 2」——那 2 个
/// 是 SETUP_ERROR，在同一行里被数了两遍（自己一列、又并进 FAIL 一列）。
///
/// 做验收的人读到「FAIL: 2」会报给客户「两个吞吐不达标」，而控制台上同一轮
/// 显示 0 失败。判定层有「只有一份实现」的铁律，计数层同理。
#[test]
fn counters_mean_the_same_thing_on_both_exits() {
    use crate::master::run_status::RunCounts;

    for verdict in [
        Verdict::Pass,
        Verdict::RateFail,
        Verdict::Measured,
        Verdict::NotEvaluated,
        Verdict::SetupError,
        Verdict::Skip,
    ] {
        let mut summary = RunSummary::default();
        summary.bump(verdict);
        let mut counts = RunCounts::default();
        counts.bump(verdict);
        assert_eq!(
            (
                summary.pass,
                summary.fail,
                summary.measured,
                summary.not_evaluated,
                summary.setup_error,
                summary.skip
            ),
            (
                counts.pass,
                counts.fail,
                counts.measured,
                counts.not_evaluated,
                counts.setup_error,
                counts.skip
            ),
            "{verdict:?} 在命令行汇总和控制台进度上被数进了不同的格子"
        );
    }

    // 分区性：每个判定只落进**一格**，总数才等于单元数。
    // 命令行那行「单元总数」正是六格相加，多算一格就会大于真实单元数。
    for verdict in [
        Verdict::Pass,
        Verdict::RateFail,
        Verdict::Measured,
        Verdict::NotEvaluated,
        Verdict::SetupError,
        Verdict::Skip,
    ] {
        let mut summary = RunSummary::default();
        summary.bump(verdict);
        let total = summary.pass
            + summary.fail
            + summary.measured
            + summary.not_evaluated
            + summary.setup_error
            + summary.skip;
        assert_eq!(total, 1, "{verdict:?} 落进了不止一格");
    }

    // 退出码的口径**不能**跟着变严：跑坏了的一轮不许因为「只是没判成」返回 0。
    for verdict in [
        Verdict::RateFail,
        Verdict::NotEvaluated,
        Verdict::SetupError,
    ] {
        let mut summary = RunSummary::default();
        summary.bump(verdict);
        assert!(
            summary.any_not_passed() > 0,
            "{verdict:?} 必须让退出码非 0——这是脚本和 CI 唯一看得见的信号"
        );
    }
    for verdict in [Verdict::Pass, Verdict::Measured, Verdict::Skip] {
        let mut summary = RunSummary::default();
        summary.bump(verdict);
        assert_eq!(summary.any_not_passed(), 0, "{verdict:?} 不该让退出码非 0");
    }
}

/// 判定 → 计数的映射只能有两处定义（`RunSummary::bump` 与 `RunCounts::bump`），
/// 且已由 `counters_mean_the_same_thing_on_both_exits` 钉住两者等价。
///
/// 这条结构断言防的是**回退**：把 `match unit_verdict` 重新内联回 executor，
/// 上面那条等价测试照样全绿（它测的是 helper，不是调用点），而命令行汇总会
/// 悄悄退回「FAIL 把 SETUP_ERROR 也算进去」的老口径。判定优先级用同样的手法
/// 守着唯一入口，计数层同理。
#[test]
fn the_verdict_to_counter_mapping_has_no_third_copy() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let allowed = ["executor.rs", "run_status.rs"];
    let mut offenders = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if allowed.contains(&name) || name == "tests.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            // 「按判定分派到计数字段」的形状：同时出现这两个分支就是又抄了一份。
            if text.contains("Verdict::SetupError => ") && text.contains("setup_error += 1") {
                offenders.push(path.display().to_string());
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "判定→计数的映射又多了一份，命令行与控制台会再次对同一个词给出不同的数: {offenders:#?}"
    );

    // 调用点必须走 helper：executor 里不许再出现内联的 `sum.setup_error += 1`。
    let executor = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/master/executor.rs"),
    )
    .expect("read executor");
    let body = executor
        .split("impl RunSummary {")
        .nth(1)
        .and_then(|rest| rest.split("\n}").next())
        .unwrap_or("");
    let outside = executor.replace(body, "");
    for banned in [
        "sum.setup_error += 1",
        "sum.not_evaluated += 1",
        "sum.fail += 1",
    ] {
        assert!(
            !outside.contains(banned),
            "executor 里又出现了绕过 `RunSummary::bump` 的直接累加：{banned}"
        );
    }
}

/// **RESUME 命中条件的精确边界**（回归方案 RES-01）。
///
/// 命中一次的代价是**整个单元不跑**，报告里写 SKIP 并沿用上一次的 PASS。所以
/// 这道判据错在哪一侧不是对称的：错过（该命中没命中）只是多跑一轮，误中
/// （不该命中却命中）会让一份**过期结论**冒充本轮结果交出去。
///
/// 现有覆盖只到 `resume_age_is_fresh` 这个纯函数，和 `test_result_db` 的三个
/// 粗粒度情形。真正做决定的是 `fresh_pass`——它还要过 `ok` 和**时间字符串解析**
/// 两道，而那两道一条断言都没有。这里把整条路补齐。
#[test]
fn a_resume_hit_needs_a_pass_a_parseable_time_and_an_age_inside_the_window() {
    let dir = std::env::temp_dir().join("cpe_db_res01");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("task_results.json");
    let _ = std::fs::remove_file(&path);

    // 直接写库文件，才能把「多久以前」精确摆到边界上。
    let write = |entries: &[(&str, bool, chrono::Duration)]| {
        let now = chrono::Local::now().naive_local();
        let map: std::collections::HashMap<String, serde_json::Value> = entries
            .iter()
            .map(|(id, ok, age)| {
                (
                    id.to_string(),
                    serde_json::json!({
                        "ok": ok,
                        "time": (now - *age).format("%Y-%m-%d %H:%M:%S").to_string(),
                        "title": "t",
                    }),
                )
            })
            .collect();
        std::fs::write(&path, serde_json::to_string(&map).unwrap()).unwrap();
        ResultDb::load(path.clone())
    };

    let hour = chrono::Duration::hours(1);
    let db = write(&[
        ("just_now", true, chrono::Duration::zero()),
        (
            "almost_a_day",
            true,
            chrono::Duration::hours(23) + chrono::Duration::minutes(59),
        ),
        ("exactly_a_day", true, chrono::Duration::hours(24)),
        ("stale", true, chrono::Duration::hours(25)),
        // 负 age = 记录时间在「现在」之后。60 秒容差是给同一轮内写完立刻读的；
        // 再往前就是时钟真的错了，那份记录不能信。
        ("clock_skew_ok", true, chrono::Duration::seconds(-60)),
        ("clock_skew_bad", true, chrono::Duration::seconds(-61)),
        // 非 PASS：年龄再新也不能命中，否则一次失败会把自己跳过去。
        ("failed_but_fresh", false, hour),
    ]);

    for (id, want) in [
        ("just_now", true),
        ("almost_a_day", true),
        ("exactly_a_day", false),
        ("stale", false),
        ("clock_skew_ok", true),
        ("clock_skew_bad", false),
        ("failed_but_fresh", false),
        ("never_seen", false),
    ] {
        assert_eq!(
            db.fresh_pass(id).is_some(),
            want,
            "{id} 的 RESUME 命中判断错了。误中会让过期结论冒充本轮结果，\
             比多跑一轮贵得多"
        );
    }

    // **坏时间一律不命中**。库文件是人可以手改的，也可能被写坏一半；
    // 解析不出来时唯一安全的答案是「重跑」，不是「就当它很新」。
    let now = chrono::Local::now().naive_local();
    for bad_time in [
        "",
        "not-a-time",
        "2026-09-06",                                 // 缺时分秒
        "2026-09-06T12:00:00",                        // ISO 的 T，本格式不收
        "2026-13-45 99:99:99",                        // 结构像但值越界
        &now.format("%Y/%m/%d %H:%M:%S").to_string(), // 斜杠分隔
    ] {
        let map = serde_json::json!({ "x": { "ok": true, "time": bad_time, "title": "t" } });
        std::fs::write(&path, serde_json::to_string(&map).unwrap()).unwrap();
        assert!(
            ResultDb::load(path.clone()).fresh_pass("x").is_none(),
            "时间 {bad_time:?} 解析不出来时必须重跑，不能当成新鲜的 PASS"
        );
    }

    // 整个库文件坏掉时同样只能是「什么都没命中」，不能 panic、也不能全命中。
    std::fs::write(&path, "{ this is not json").unwrap();
    assert!(ResultDb::load(path.clone())
        .fresh_pass("just_now")
        .is_none());

    // 毫秒级邻界只能在纯函数上钉：落盘格式精确到秒（`%H:%M:%S`），
    // 24h−1ms 与 24h+1ms 存进去是同一个字符串，`fresh_pass` 分不出来。
    let day = chrono::Duration::hours(RESUME_MAX_AGE_HOURS);
    assert!(resume_age_is_fresh(day - chrono::Duration::milliseconds(1)));
    assert!(!resume_age_is_fresh(day));
    assert!(!resume_age_is_fresh(
        day + chrono::Duration::milliseconds(1)
    ));
    assert!(resume_age_is_fresh(chrono::Duration::seconds(-60)));
    assert!(!resume_age_is_fresh(
        chrono::Duration::seconds(-60) - chrono::Duration::milliseconds(1)
    ));

    let _ = std::fs::remove_file(&path);
}

/// **三条链路共用同一个「窗口够不够长」的容差**（回归方案 TIME-05）。
///
/// TCP、UDP、CTS 各有自己的窗口推导，但「差几毫秒算不算跑满」这件事对三者
/// 是同一个问题：毫秒取整、采样对不齐、进程收尾各差一点点。三处各写一个字面量
/// 的话，同一条链路上 TCP 判「完整」而 UDP 判「不足」是迟早的事——而两边给出的
/// 是**不同的 verdict**（PASS vs NOT_EVALUATED/EFFECTIVE_WINDOW_SHORT），
/// 不是显示差异。
///
/// 这条是结构断言：`window.rs` 里不许出现第二个「和 required 比较时加的毫秒
/// 字面量」。判定优先级、速率口径、判定→计数三处都用同样的手法守着唯一入口。
#[test]
fn every_transport_shares_one_window_completeness_tolerance() {
    // **两个文件都要扫**：iperf/TCP 那条窗口推导已经搬到
    // `src/cmd/iperf_window.rs`（子网与内环共用），只读 window.rs 的话，这条
    // 守卫看得见的就只剩 CTS 两处加 UDP 一处——`uses >= 3` 靠巧合继续通过，
    // 而它本来要护的 iperf 链一行都没被检查。往 iperf_window.rs 里写死一个
    // `saturating_add(150)`，整条断言照样绿。
    let sources = [
        include_str!("window.rs"),
        include_str!("../../cmd/iperf_window.rs"),
        // 第四处：内环双向单元的重叠窗口（`overlap_window`）。它和三条链路
        // 推出来的窗口拿同一把尺子量「跑满没有」，量歪了同样是 PASS 对
        // EFFECTIVE_WINDOW_SHORT 的差别，只不过发生在双向单元和它自己的
        // 两条腿之间。
        include_str!("../../inner/mod.rs"),
    ];
    let code: String = sources
        .iter()
        .map(|source| source.split("#[cfg(test)]").next().unwrap_or(source))
        .flat_map(|production| {
            production
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
        })
        .collect::<Vec<_>>()
        .join("\n");

    // 「跑满没有」的比较必须走那个常量。
    let uses = code.matches("WINDOW_COMPLETE_TOLERANCE_MS").count();
    assert!(
        uses >= 3,
        "只有 {uses} 处用到 WINDOW_COMPLETE_TOLERANCE_MS；\
         TCP/UDP/CTS 三条窗口推导本该共用它（window.rs + cmd/iperf_window.rs 一起数）"
    );

    // 不许有人另起一个数。只看**和 required 比较**的那些式子：
    // 别处的 `saturating_add(1_000)` 是扫描循环的一秒步进，与容差无关。
    //
    // 比较可能跨行写（rustfmt 会折），所以按语句而不是按行切。
    // 「和要求时长比较」不止 `required_ms` 一种写法：内环那处写的是
    // `required_secs.saturating_mul(1_000)`。只认前者的话，把文件加进来也白加。
    for statement in code.split(';') {
        let compares_required =
            statement.contains("required_ms") || statement.contains("required_secs");
        if !compares_required || !statement.contains("saturating_add(") {
            continue;
        }
        for piece in statement.split("saturating_add(").skip(1) {
            let arg: String = piece.chars().take_while(|c| *c != ')').collect();
            let is_literal = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit() || c == '_');
            assert!(
                !is_literal,
                "窗口推导里出现了写死的毫秒容差 `saturating_add({arg})`，\
                 而这条式子正在和 required_ms 比较。三条链路必须共用 \
                 WINDOW_COMPLETE_TOLERANCE_MS——各写一个数的话，同一条链路上 TCP 判\
                 「完整」而 UDP 判「不足」，两边给出的是不同的 verdict，不是显示差异。\
                 涉事语句：{}",
                statement.trim()
            );
        }
    }
}

/// 网卡在开跑前消失：判死、落盘、通知，三件事一件都不能少。
///
/// 这条路径此前**完全没有测试**——`isolated_ctx` 里 `topology: None`，整条拓扑
/// 刷新分支从来没被执行过。它和 RESUME 跳过是同一种形状：`continue` 掉了，
/// 绕过下面那个统一的单元收尾，所以增量落盘和 `unit_finished` 都得在原地补一次。
/// 少了落盘，长队列跑到一半崩溃时重放报告会漏掉这条「网卡消失」；
/// 少了通知，进度页会永远停在这个单元上。
#[test]
fn a_unit_whose_nic_vanished_is_judged_persisted_and_announced() {
    /// 拓扑里一块网卡都不剩：单元引用的每个端点都会被判成 Gone。
    struct EverythingGone;
    impl TopologySource for EverythingGone {
        fn snapshot(&self) -> Result<(HostInfo, HostInfo), String> {
            let empty = || HostInfo {
                hostname: "gone".into(),
                os: "test".into(),
                interfaces: Vec::new(),
            };
            Ok((empty(), empty()))
        }
    }

    #[derive(Debug, Default)]
    struct Seen {
        finished: Mutex<Vec<(String, String)>>,
    }
    impl RunObserver for Seen {
        fn unit_finished(&self, status: UnitStatus, _remaining_est_secs: u64) {
            lock_recover(&self.finished).push((status.verdict.clone(), status.reason_code.clone()));
        }
    }

    let unit = ctstraffic_unit("nic-gone", false);
    let (mut ctx, db_path) = isolated_ctx(0);
    ctx.topology = Some(Arc::new(EverythingGone));
    let seen = Arc::new(Seen::default());
    ctx.observer = Some(seen.clone());

    let summary = ctx.run_all_with_preflight_blocks(&[unit], &HashMap::new());

    // 1. 当场判死，不照跑。
    assert_eq!(
        summary.traffic_setup_errors, 1,
        "网卡消失必须记成 setup error"
    );
    assert_eq!(summary.skip, 0, "这不是跳过，是判死");
    let rows = ctx.rows.lock().unwrap();
    let dead = rows
        .iter()
        .find(|row| row.reason_code == ReasonCode::NicDisappeared)
        .expect("应有一行网卡消失");
    assert_eq!(dead.verdict, Verdict::SetupError);
    assert_eq!(dead.execution_status, ExecutionStatus::Error);
    assert!(dead.reason_detail.contains("已消失"));
    assert!(dead.reason_detail.contains("无法采样"));
    drop(rows);

    // 2. 已经落进增量 JSONL——这条 `continue` 绕过了统一收尾，落盘必须就地补。
    let persisted = std::fs::read_to_string(ctx.run_dir.join(crate::report::store::ROWS_FILE))
        .expect("网卡消失也必须进入增量 JSONL，否则中途崩溃后重放会漏掉它");
    assert!(
        persisted.contains("NIC_DISAPPEARED"),
        "增量 JSONL 里没有这条网卡消失记录：{persisted}"
    );

    // 3. 进度页收到了收尾通知，不会永远停在这个单元上。
    let finished = lock_recover(&seen.finished);
    assert_eq!(finished.len(), 1, "这个单元只该收尾一次");
    assert_eq!(finished[0].1, ReasonCode::NicDisappeared.as_str());

    let _ = std::fs::remove_file(db_path);
}

/// 产物文件名不能被用户配置撑爆 Windows 的 260 字符上限。
///
/// `label` 是 `unit.title`，由配置里的链路名拼出来，长度不设限；其余拼进文件名的
/// 部分（`owner_id`、方向 tag、网卡名）都是有界的。Windows 上没开长路径支持时，
/// 整条路径超过 260 个字符 `CreateFileW` 就失败——而 `write_output_artifact`
/// 只记一行日志、返回空串，判定照常。于是现场表现是「Windows 上截图少了几张」，
/// 而开发机是 macOS，`PATH_MAX` 1024，永远复现不出来。
#[test]
fn artifact_filenames_stay_short_enough_for_windows_max_path() {
    let monster = "链路".repeat(200);
    for side in [Side::Master, Side::Agent] {
        let name = screenshot_filename(&monster, side, 7);
        assert!(
            name.len() <= 140,
            "截图文件名 {} 字符，Windows 上留给目录的余量不够：{name}",
            name.len()
        );
        assert!(name.starts_with("screenshot_"));
        assert!(name.ends_with("_7.png"), "序号必须留着，截断不能造成撞名");
    }
    // 截断只砍标签，同一秒内不同 seq 仍然互不相同。
    assert_ne!(
        screenshot_filename(&monster, Side::Master, 1),
        screenshot_filename(&monster, Side::Master, 2)
    );

    // 其余几个输入本来就有界，这里把这个前提也钉住：owner_id 由
    // `unit_resource_owner` 生成（pid/序号/nonce/时间戳/8 位 md5），方向 tag 是
    // ab/ba/oneway。哪天它们也变成用户可控的，这条会先红。
    let unit = ctstraffic_unit("bounded", false);
    let owner = unit_resource_owner(&unit, 3);
    assert!(
        owner.len() <= 80,
        "owner_id 变长了（{}），它也进文件名：{owner}",
        owner.len()
    );
}

/// **「质量」列的三个指标必须一起上单元汇总行。**
///
/// `report::model::verdict_row` 优先返回单元汇总行，HTML 概览和 summary.xlsx
/// 的「质量」列都从它取数。汇总行只填 `udp_loss` / `ping_loss` 而漏掉
/// `tcp_retransmits` 的话，TCP 那一格对**每一行**都是空的——没有任何测试会红，
/// 用户打开 Excel 想回答「是在丢包还是窗口没喂饱」，看到的是一整列空白。
/// 这正是 ADR-7 为协议/后端两列记下的那类静默空列。
#[test]
fn the_unit_summary_row_carries_every_quality_metric_not_just_two_of_them() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/master/executor.rs"),
    )
    .expect("read executor.rs");
    // 汇总行是唯一一处从 `single_direction` 取 `ping_loss` 的地方。
    let at = text
        .find("ping_loss: single_direction")
        .expect("单元汇总行的构造点不见了——这条断言要跟着搬");
    let window = &text[at.saturating_sub(2_000)..at];
    for field in ["udp_loss", "tcp_retransmits"] {
        assert!(
            window.contains(&format!("{field}: single_direction")),
            "单元汇总行漏了 {field}：它和 udp_loss / ping_loss 平级，\
             一起进「质量」列，漏一个就是一整列空白"
        );
    }
}

/// **灌包单元的计数要在「链路已放弃」那条早退分支之前加。**
///
/// 放在后面的话，被放弃的单元只进 `traffic_setup_errors` 不进 `traffic_units`，
/// 两个计数器发散：50 个单元的链路被放弃时，`ui.rs` 的收尾文案会打出
/// 「本轮 2 个灌包单元没有产生任何有效速率测量（其中 SETUP_ERROR=50）」——
/// 一句自相矛盾的话。`needs_traffic_failure_diagnostics()` 也跟着少数，
/// 于是本该自动补跑的 Ping 诊断不跑了。
///
/// 断言写成「谁在前」而不是跑一轮看计数：那条分支要真实流量连续失败才进得去，
/// 行为上测不到。
#[test]
fn traffic_units_are_counted_before_the_abandoned_link_shortcut() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/master/executor.rs"),
    )
    .expect("read executor.rs");
    assert_eq!(
        text.matches("sum.traffic_units += 1;").count(),
        1,
        "这个计数只该有一处；多出一处就说明有人又在早退分支里补了一次"
    );
    let counted_at = text
        .find("sum.traffic_units += 1;")
        .expect("灌包单元计数不见了");
    let shortcut_at = text
        .find("breaker.is_abandoned(&link_key)")
        .expect("链路放弃分支不见了——这条断言要跟着搬");
    assert!(
        counted_at < shortcut_at,
        "灌包单元计数排在早退分支之后，被放弃的单元不会进 traffic_units"
    );
}

#[test]
fn comparison_identity_keeps_both_legs_without_changing_resume_or_leg_tags() {
    let single = ctstraffic_unit("original-id", false);
    let mut bidir = single.clone();
    bidir.bidir = true;
    let mut reverse = ctstraffic_task(false);
    std::mem::swap(&mut reverse.src, &mut reverse.dst);
    reverse.comparison_label = "other parameters".into();
    bidir.legs.push(Leg {
        tag: "ba".into(),
        kind: LegKind::CtsTraffic(reverse),
    });
    let make = |unit: &Unit| {
        crate::master::executor::row::unit_row(unit, 0, "汇总")
            .comparison_identity
            .unwrap()
    };
    let identity = make(&bidir);
    assert!(identity.bidir);
    assert_eq!(identity.legs.len(), 2);
    assert_ne!(identity, make(&single));
    assert_eq!(identity.legs[1].src_side, crate::report::RowSide::Agent);
    assert_eq!(identity.legs[1].parameters, ["other parameters"]);
    assert_eq!(bidir.id, "original-id");
    assert_eq!(bidir.legs[0].tag, single.legs[0].tag);
}

/// **流数不进对比身份。**
///
/// 开着「按链路上限裁剪」时，UDP 的流数是 `floor(路径上限 / -b)`，路径上限跟着
/// 协商速率走：同一条链路从 2.5G 降到 1G，4 条流可能变成 1 条，腿也从
/// `IperfGroup` 变成 `IperfSingle`。按流展开参数的话，这条测试在两轮里就是两把键，
/// 对比报告把一次掉速报成「本轮缺失 + 本轮新增」。
#[test]
fn the_comparison_identity_does_not_depend_on_how_many_udp_streams_survived_clamping() {
    let src = endpoint(Side::Master, "en0", "192.168.1.2");
    let dst = endpoint(Side::Agent, "en1", "192.168.1.3");
    let mut four = ctstraffic_unit("udp-unit", true);
    four.legs = vec![Leg {
        tag: String::new(),
        kind: LegKind::IperfGroup {
            name: "udp_b500m".into(),
            streams: udp_plan(0, "", 4, &src, &dst, 10).streams,
        },
    }];
    let mut one = four.clone();
    one.legs = vec![Leg {
        tag: String::new(),
        kind: LegKind::IperfSingle(udp_plan(0, "", 1, &src, &dst, 10).streams.remove(0)),
    }];
    let make = |unit: &Unit| {
        crate::master::executor::row::unit_row(unit, 0, "汇总")
            .comparison_identity
            .unwrap()
    };
    assert_eq!(make(&four), make(&one));
    assert_eq!(make(&four).legs[0].parameters, ["UDP -b 500m"]);
}

/// 原始记录落盘后，行里只留报告会嵌入的首尾版本；没落盘时行里那份是唯一的全文。
///
/// 每行挂着 client / server / 流事件三份原文，整轮留在内存、整份写进 rows.jsonl，
/// 一轮几十个长时长单元就是几百 MB——而报告本来就只嵌入首尾，全文在原始记录里。
#[test]
fn row_raws_keep_only_the_embedded_copy_once_the_raw_record_is_on_disk() {
    let long = format!(
        "开头\n{}\n结尾汇总 receiver",
        "[  5] 1.00-2.00 sec 112 MBytes 940 Mbits/sec\n".repeat(5_000)
    );
    let raws = vec![("iperf3 client 输出".to_string(), long.clone())];

    let saved = row_raws(true, raws.clone());
    assert_eq!(saved[0].0, "iperf3 client 输出");
    assert_eq!(saved[0].1, crate::report::embedded_raw(&long));
    assert!(saved[0].1.starts_with("开头") && saved[0].1.ends_with("结尾汇总 receiver"));
    assert!(saved[0].1.len() < long.len() / 5);

    assert_eq!(
        row_raws(false, raws.clone()),
        raws,
        "没落盘时不许丢任何内容"
    );
}

#[test]
fn link_local_zones_follow_the_platform_that_runs_the_command_not_the_master() {
    // Windows 的 iperf3 / ping 不认 `%xx`；macOS / Linux 不带 zone 绑不上 link-local。
    assert!(!os_needs_v6_zone("windows"));
    assert!(!os_needs_v6_zone(" Windows "));
    assert!(os_needs_v6_zone("macos"));
    assert!(os_needs_v6_zone("linux"));
    assert_eq!(with_zone("fe80::1", "en0", true), "fe80::1%en0");
    assert_eq!(with_zone("fe80::1", "en0", false), "fe80::1");
    assert_eq!(with_zone("fe80::1", "", true), "fe80::1");
    assert_eq!(with_zone("2408::5", "en0", true), "2408::5");

    let (mut ctx, _) = isolated_ctx(1);
    // 实机上撞到的组合：Windows 主控 + macOS 辅测，辅测端执行的命令必须带 zone。
    ctx.agent_os = "macos".into();
    assert_eq!(ctx.add_zone("fe80::28", "en0", Side::Agent), "fe80::28%en0");
    // 反过来：macOS 主控 + Windows 辅测，辅测端的命令不能带 zone。
    ctx.agent_os = "windows".into();
    assert_eq!(ctx.add_zone("fe80::ace1", "4", Side::Agent), "fe80::ace1");
    // 主控这一端只看本机平台，与对端是什么无关。
    let local_needs = os_needs_v6_zone(&crate::util::os_name());
    for agent in ["windows", "macos", ""] {
        ctx.agent_os = agent.into();
        assert_eq!(
            ctx.add_zone("fe80::1", "7", Side::Master),
            if local_needs { "fe80::1%7" } else { "fe80::1" }
        );
    }
    // 对端没报平台（agent_os 为空）时按主控本机处理。
    ctx.agent_os.clear();
    assert_eq!(
        ctx.add_zone("fe80::1", "7", Side::Agent),
        if local_needs { "fe80::1%7" } else { "fe80::1" }
    );
}

/// 实机 B1-F03：状态行按 TimeSlice 还原时刻之后，30 s 的 CTS 单元要能得到完整窗口。
/// 以前事件按到达时刻记，全部挤在第 27 s 与退出前，窗口只剩约 4 s，CTS 一律 NOT_EVALUATED。
#[test]
fn cts_status_lines_restored_from_time_slice_give_a_complete_window() {
    let started_ms = 752;
    let mut events = vec![IperfFlowEvent {
        kind: IperfEventKind::Started,
        elapsed_ms: started_ms,
        ..Default::default()
    }];
    for second in 1..=33u64 {
        events.push(IperfFlowEvent {
            kind: IperfEventKind::Traffic,
            elapsed_ms: started_ms + second * 1_000 + 14,
            mbps: Some(850.0),
            ..Default::default()
        });
    }
    events.push(IperfFlowEvent {
        kind: IperfEventKind::Ended,
        elapsed_ms: 34_103,
        ..Default::default()
    });
    let window = cts_effective_window(&events, 30, 1_000, 3);
    assert!(window.complete, "{window:?}");
    assert_eq!(window.end_ms - window.start_ms, 30_000);
    assert!(window.start_ms >= started_ms + 3_000, "起流爬升段要扣掉");
    assert!(window.end_ms <= 34_103);
}

/// 三条吞吐路径的腿级行都要带上 RX 分布四项，且取自同一个 `rx_stats`。
///
/// 实机 B1-F04：CTS 行的中位 / P95 / 最小 / 最大一直是空的——`rx_stats` 早就算好了，
/// 只是 CTS 构造行时没填，HTML 与 Excel 这四列对 CTS 永远空白，而 iperf 行有值。
#[test]
fn every_throughput_path_fills_the_rx_distribution_columns() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/master/executor");
    for file in ["iperf_leg.rs", "udp.rs", "cts.rs"] {
        let text = std::fs::read_to_string(root.join(file)).expect("read source");
        for field in [
            "rx_median: rx_stats.median_mbps",
            "rx_p95: rx_stats.p95_mbps",
            "rx_min: rx_stats.min_mbps",
            "rx_max: rx_stats.max_mbps",
        ] {
            assert!(text.contains(field), "{file} 构造报告行时漏了 `{field}`");
        }
    }
}

/// CTS 报告行的工具速率按承载那一列的一侧取（`ctstraffic::rates_by_side`）。
///
/// 实机 B3-C04：合并 client 与 server 输出再按列平均，TCP 接收列混进对端握手
/// 字节、UDP 接收列混进 server 的发送速率，报告的工具速率偏低 0.05–3.6%。
/// 这一步只在执行器里接线，删掉它不会让任何解析测试变红。
#[test]
fn cts_rows_take_tool_rates_from_the_carrying_side() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/master/executor/cts.rs");
    let text = std::fs::read_to_string(path).expect("read source");
    assert!(
        text.contains("ctstraffic::rates_by_side("),
        "cts.rs 不再按侧取工具速率，报告会回到合并平均"
    );
}
