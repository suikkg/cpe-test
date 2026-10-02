/// iperf3 的两套单位不是同一个进制：bit 单位按 1000 进位，Byte 单位按 1024 进位。
///
/// 下面两段是同一条 100 Mbits/sec 的流在 `-f m` 和 `-f M` 下的**真实输出**
/// （iperf 3.18，回环，`-u -b 100M -t 2`）。两边解析出来必须是同一个速率；
/// 按 1000 进位算 Byte 单位会低报 4.6%——这个量级恰好像测量噪声，
/// 不会触发任何断言，却让工具自报速率和网卡口径互相矛盾。
///
/// **今天走不到这条分支**：`extra` 由 builder 从 `-w`/`-P`/`-b`/`-l`
/// 这些有类型的配置字段拼出来，配置里没有任何「原样透传参数」的口子，
/// 所以下发的永远是 `-f m`；直接调 agent 的 `/iperf/client` 接口这条路
/// 现在也被 `check_client_extra` 挡住了（`-f` 在受控参数黑名单里）。
/// 留着这条用例是因为解析器不该依赖调用方的自觉——黑名单是请求边界上的
/// 约束，解析器自己也得站得住。
/// `extra` 不许覆盖决定测量口径的那几个参数——三种写法都要认出来。
///
/// iperf3 对重复参数是后者覆盖前者，所以 `extra` 里一个 `-f M` 就能让解析器
/// 走进另一条进制分支，而调用方拿到的输出看起来一切正常。分开写、粘着写、
/// 长参数带等号是同一件事的三种拼法，只认第一种等于没拦。
#[test]
fn reserved_client_flags_are_caught_in_every_spelling() {
    for spelling in [
        "-f",
        "-fM",
        "--format",
        "--format=M",
        "-t",
        "-t30",
        "--time=30",
        "-i",
        "-i5",
        "-p",
        "-p5201",
        "-B",
        "-c",
        "-u",
        "-4",
        "-6",
        "--bind",
        "--udp",
    ] {
        let hits = reserved_flags_in_extra(&[spelling.to_string()]);
        assert_eq!(hits.len(), 1, "{spelling} 应当被挡下，实得 {hits:?}");
    }
}

/// 合法的档位参数一个都不许被误伤。
///
/// 这里最容易踩的是大小写：iperf3 的 `-b`（速率）和 `-B`（绑定地址）是两个
/// 不同的参数，黑名单折叠大小写就会把 builder 天天在用的 `-b` 一起挡掉。
#[test]
fn legitimate_profile_flags_are_never_mistaken_for_reserved_ones() {
    let extra: Vec<String> = [
        "-w",
        "64k",
        "-P",
        "10",
        "-b",
        "2500m",
        "-l",
        "14k",
        "-w",
        "256m",
        "-b",
        "1000000000",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert!(
        reserved_flags_in_extra(&extra).is_empty(),
        "builder 拼出来的档位参数被误伤了: {:?}",
        reserved_flags_in_extra(&extra)
    );
}

/// 报错要说清「被覆盖的是哪个参数」和「该走哪个字段」，否则调用方只会重试。
#[test]
fn the_reserved_flag_error_names_the_flag_and_the_way_out() {
    let req = IperfClientReq {
        dst: "127.0.0.1".into(),
        bind_ip: "127.0.0.1".into(),
        port: 5201,
        duration: 5,
        udp: false,
        v6: false,
        extra: vec!["-f".into(), "M".into()],
    };
    let error = check_client_extra(&req).expect_err("-f 必须被挡下");
    assert!(error.contains("-f"), "{error}");
    assert!(error.contains("duration"), "报错要指出正路: {error}");
    // 干净的请求不许被挡。
    let clean = IperfClientReq {
        extra: vec!["-w".into(), "4m".into(), "-P".into(), "10".into()],
        ..req
    };
    assert!(check_client_extra(&clean).is_ok());
}

#[test]
fn byte_formatted_rates_use_the_1024_base_iperf3_actually_prints() {
    let bits =
        "[  5]   0.00-2.00   sec  23.8 MBytes   100 Mbits/sec  0.000 ms  0/1531 (0%)  receiver";
    let bytes =
        "[  5]   0.00-2.00   sec  23.9 MBytes  11.9 MBytes/sec  0.009 ms  0/1534 (0%)  receiver";

    let from_bits = parse_output(bits)
        .receiver_mbps
        .expect("bit 格式要解析出速率");
    let from_bytes = parse_output(bytes)
        .receiver_mbps
        .expect("Byte 格式要解析出速率");

    assert!(
        (from_bits - 100.0).abs() < 0.001,
        "bit 格式不能被改坏: {from_bits}"
    );
    assert!(
        (from_bytes - 100.0).abs() < 0.5,
        "11.9 MBytes/sec 是 1024 进制，应约等于 100 Mbps，实得 {from_bytes}"
    );
    assert!(
        (from_bits - from_bytes).abs() < 0.5,
        "同一条流的两种打印格式必须解析成同一个速率：{from_bits} vs {from_bytes}"
    );
}

/// 换进制不能把常规的 bit 格式改坏——这是生产上唯一真正走到的那条。
#[test]
fn bit_formatted_rates_keep_their_decimal_base() {
    let cases = [
        ("[  5]  0.00-1.00  sec  1.09 GBytes  9350 Mbits/sec", 9350.0),
        ("[  5]  0.00-1.00  sec  1.09 GBytes  9.35 Gbits/sec", 9350.0),
        (
            "[  5]  0.00-1.00  sec  1.09 GBytes  935000 Kbits/sec",
            935.0,
        ),
        ("[  5]  0.00-1.00  sec  1.09 GBytes  1000000 bits/sec", 1.0),
    ];
    for (line, expected) in cases {
        let got = parse_output(line).last_mbps.expect(line);
        assert!(
            (got - expected).abs() < 0.001,
            "{line} 应解析成 {expected} Mbps，实得 {got}"
        );
    }
}

use super::args::*;
use super::client::*;
use super::jobs::*;
use super::parse::*;
use super::server::*;
use super::*;
use crate::protocol::{
    IperfClientOut, IperfClientReq, IperfClientStartReq, IperfClientStatusOut, IperfEventKind,
    IperfFlowEvent, IperfServerStartReq,
};
use crate::util::{BoundedOutput, CmdOut, OutputLimit, ProcessExecutor, ProcessSpec};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Condvar};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct FakeProcessExecutor {
    forceflush: bool,
    lines: Vec<String>,
    stream_out: Mutex<Option<CmdOut>>,
    streamed_specs: Mutex<Vec<ProcessSpec>>,
}

impl FakeProcessExecutor {
    fn new(forceflush: bool, lines: Vec<String>, stream_out: CmdOut) -> Self {
        Self {
            forceflush,
            lines,
            stream_out: Mutex::new(Some(stream_out)),
            streamed_specs: Mutex::new(Vec::new()),
        }
    }
}

impl ProcessExecutor for FakeProcessExecutor {
    fn run(&self, _spec: &ProcessSpec, _timeout: Duration) -> CmdOut {
        CmdOut {
            ok: true,
            stdout: if self.forceflush { "--forceflush" } else { "" }.into(),
            ..Default::default()
        }
    }

    fn run_streaming(
        &self,
        spec: &ProcessSpec,
        _timeout: Duration,
        _cancel: Option<&AtomicBool>,
        on_line: &mut dyn FnMut(&str, Instant),
    ) -> CmdOut {
        self.streamed_specs.lock().unwrap().push(spec.clone());
        let started = Instant::now();
        for (index, line) in self.lines.iter().enumerate() {
            on_line(line, started + Duration::from_millis(index as u64));
        }
        self.stream_out
            .lock()
            .unwrap()
            .take()
            .expect("fake stream result already consumed")
    }
}

const TCP_SAMPLE: &str = r#"
Connecting to host 192.168.1.3, port 56000
[  5] local 192.168.1.2 port 52822 connected to 192.168.1.3 port 56000
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-1.00   sec   283 MBytes  2372 Mbits/sec
[  5]   1.00-2.00   sec   285 MBytes  2389 Mbits/sec
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-10.00  sec  2.77 GBytes  2379 Mbits/sec                  sender
[  5]   0.00-10.04  sec  2.77 GBytes  2368 Mbits/sec                  receiver

iperf Done.
"#;

#[test]
fn event_epoch_alignment_applies_runner_start_delay_only_once() {
    let mut origin_ms = None;
    let mut started = IperfFlowEvent {
        kind: IperfEventKind::Started,
        elapsed_ms: 0,
        ..Default::default()
    };
    align_event_to_epoch(&mut started, 5_000, &mut origin_ms);
    assert_eq!(origin_ms, Some(5_000));
    assert_eq!(started.elapsed_ms, 5_000);

    // 第二个回调即使因轮询/缓冲在 16s 才被读到，仍只能
    // 加入首次确定的 5s runner 启动偏移，不能重新用 16s 对齐。
    let mut traffic = IperfFlowEvent {
        kind: IperfEventKind::Traffic,
        elapsed_ms: 10_000,
        mbps: Some(100.0),
        ..Default::default()
    };
    align_event_to_epoch(&mut traffic, 16_000, &mut origin_ms);
    assert_eq!(traffic.elapsed_ms, 15_000);
}

#[test]
fn test_parse_tcp() {
    let p = parse_output(TCP_SAMPLE);
    assert_eq!(p.sender_mbps, Some(2379.0));
    assert_eq!(p.receiver_mbps, Some(2368.0));
    assert!(p.has_measurement());
    // 这段输出压根没有 Retr 列（`-w` 没触发拥塞信息时 iperf3 会留空）。
    // 「不知道」必须是 None——报成 0 等于告诉读报告的人这条链路一次没重传。
    assert_eq!(p.tcp_retransmits, None);
}

/// 带 `Retr` 列的 TCP 输出：单流取本流 sender 行，多流取 `[SUM]`。
const TCP_RETR_SAMPLE: &str = r#"
[ ID] Interval           Transfer     Bitrate         Retr  Cwnd
[  5]   0.00-1.00   sec   112 MBytes   939 Mbits/sec   17    412 KBytes
[  5]   1.00-2.00   sec   111 MBytes   933 Mbits/sec    9    398 KBytes
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate         Retr
[  5]   0.00-10.00  sec  1.09 GBytes   933 Mbits/sec  221             sender
[  5]   0.00-10.04  sec  1.09 GBytes   932 Mbits/sec                  receiver
"#;

#[test]
fn tcp_retransmits_come_from_the_sender_summary_line() {
    let p = parse_output(TCP_RETR_SAMPLE);
    assert_eq!(p.tcp_retransmits, Some(221));
    // 逐秒 interval 行的 17 / 9 不能盖掉汇总行：那是「这一秒重传了几次」，
    // 当全程重传数用会随最后一个采样周期上下跳。
    assert_eq!(p.sender_mbps, Some(933.0));
}

const TCP_RETR_SUM_SAMPLE: &str = r#"
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate         Retr
[  5]   0.00-10.00  sec   372 MBytes   312 Mbits/sec   80             sender
[  7]   0.00-10.00  sec   371 MBytes   311 Mbits/sec   64             sender
[SUM]   0.00-10.00  sec   743 MBytes   623 Mbits/sec  144             sender
[SUM]   0.00-10.04  sec   742 MBytes   620 Mbits/sec                  receiver
"#;

#[test]
fn multi_stream_retransmits_take_the_sum_line_not_the_last_flow() {
    let p = parse_output(TCP_RETR_SUM_SAMPLE);
    // 144 = 80 + 64。取到 64 就是只报了最后一条流的重传。
    assert_eq!(p.tcp_retransmits, Some(144));
}

#[test]
fn udp_never_reports_retransmits_even_though_a_number_follows_the_rate() {
    // UDP 的 sender 行同一个位置是抖动 `0.000 ms`。这条断言守的就是
    // 「别把抖动的整数部分当成重传数」——报出 `重传 0 次` 会让人以为
    // 这条 UDP 链路有重传统计，而 UDP 根本没有重传。
    assert_eq!(parse_output(UDP_SAMPLE).tcp_retransmits, None);
    assert_eq!(parse_output(UDP_FULL_LOSS_SAMPLE).tcp_retransmits, None);
}

const UDP_SAMPLE: &str = r#"
[  5]   0.00-1.00   sec  11.9 MBytes  99.9 Mbits/sec  8630
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate         Jitter    Lost/Total Datagrams
[  5]   0.00-10.00  sec   119 MBytes  100 Mbits/sec  0.000 ms  0/86380 (0%)  sender
[  5]   0.00-10.04  sec   119 MBytes  99.6 Mbits/sec  0.014 ms  312/86380 (0.36%)  receiver

iperf Done.
"#;

#[test]
fn test_parse_udp() {
    let p = parse_output(UDP_SAMPLE);
    assert_eq!(p.sender_mbps, Some(100.0));
    assert_eq!(p.receiver_mbps, Some(99.6));
    assert_eq!(p.udp_lost_datagrams, Some(312));
    assert_eq!(p.udp_total_datagrams, Some(86380));
    // 从计数算，不取 iperf3 打印的 0.36。
    assert!((p.udp_loss_pct.unwrap() - 0.361_194_7).abs() < 1e-6);
}

/// 取自 run_20260825_215915_7684 的 unit-7684-33-34 udp_ab：
/// iperf3 在接近满丢包时把百分比打成 `1e+02%`，且 server 段最后一条
/// 是 0 字节的收尾残帧 `0/0 (0%)`。旧解析在整段文本上取最后一次匹配，
/// 于是把 99.97% 的丢包报成了 0.000%。
const UDP_FULL_LOSS_SAMPLE: &str = r#"
[  5] 203.01-204.01 sec  28.0 KBytes  0.23 Mbits/sec  3535.517 ms  37544/37546 (1e+02%)
[  5] 205.00-206.01 sec  14.0 KBytes  0.11 Mbits/sec  3590.606 ms  29235/29236 (1e+02%)
[  5] 206.01-206.56 sec  0.00 Bytes  0.00 Mbits/sec  3590.606 ms  0/0 (0%)
- - - - - - - - - - - - - - - - - - - - - - - - -
[  5]   0.00-206.00 sec  40.8 GBytes  1701 Mbits/sec  0.000 ms  0/3054905 (0%)  sender
[  5]   0.00-206.56 sec  14.4 MBytes  0.59 Mbits/sec  3590.606 ms  3035698/3036752 (1e+02%)  receiver
"#;

#[test]
fn scientific_notation_loss_is_read_from_the_receiver_summary_not_the_tail_interval() {
    let p = parse_output(UDP_FULL_LOSS_SAMPLE);
    assert_eq!(p.sender_mbps, Some(1701.0));
    assert_eq!(p.receiver_mbps, Some(0.59));
    assert_eq!(p.udp_lost_datagrams, Some(3_035_698));
    assert_eq!(p.udp_total_datagrams, Some(3_036_752));
    let loss = p.udp_loss_pct.expect("满丢包必须解析出丢包率");
    assert!(
        (99.9..=100.0).contains(&loss),
        "1e+02% 的 receiver 汇总行必须算出 ~100% 而不是尾部残帧的 0%，实际 {loss}"
    );
}

#[test]
fn sender_only_zero_loss_never_becomes_the_reported_loss() {
    // client 拿不到 server 汇总时只剩 sender 行。sender 的 0% 是
    // 「我全发出去了」，不是「没丢」——必须报未知。
    let p = parse_output(
        "[  5]   0.00-206.00 sec  40.8 GBytes  1701 Mbits/sec  0.000 ms  0/3054905 (0%)  sender\n",
    );
    assert_eq!(p.sender_mbps, Some(1701.0));
    assert_eq!(p.udp_loss_pct, None);
    assert_eq!(p.udp_total_datagrams, None);
}

#[test]
fn receiver_summary_without_any_datagram_reports_unknown_loss() {
    let p = parse_output(
        "[  5]   0.00-10.00 sec  0.00 Bytes  0.00 Mbits/sec  0.000 ms  0/0 (0%)  receiver\n",
    );
    assert_eq!(p.udp_total_datagrams, Some(0));
    assert_eq!(p.udp_loss_pct, None);
}

#[test]
fn last_receiver_summary_wins_across_retry_attempts() {
    let text = concat!(
        "[  5]   0.00-10.04 sec  119 MBytes  99.6 Mbits/sec  0.014 ms  312/86380 (0.36%)  receiver\n",
        "iperf3: error - unable to connect to server\n",
        "[  5]   0.00-10.04 sec  119 MBytes  50.0 Mbits/sec  0.014 ms  40000/80000 (50%)  receiver\n",
    );
    let p = parse_output(text);
    assert_eq!(p.udp_lost_datagrams, Some(40_000));
    assert_eq!(p.udp_loss_pct, Some(50.0));
}

#[test]
fn test_parse_gbits_and_bytes() {
    let p = parse_output("[  5]  0.00-10.00 sec  2.77 GBytes  2.38 Gbits/sec  sender\n");
    assert_eq!(p.sender_mbps, Some(2380.0));
    // 1 MBytes/sec 是 1 MiB/s = 8.388608 Mbps，不是 8。这条断言原先钉的是 8.0，
    // 等于把「Byte 单位按 1000 进位」这个错误锁进了回归防线。
    let p2 = parse_output("[  5]  0.0-1.0 sec  1.00 MBytes/sec\n");
    assert_eq!(p2.last_mbps, Some(8.388608));
}

#[test]
fn test_parse_empty() {
    let p = parse_output("iperf3: error - unable to connect to server\n");
    assert!(!p.has_measurement());
}

#[test]
fn retry_history_keeps_every_client_attempt() {
    let mut history = Vec::new();
    append_attempt_output(&mut history, 1, "first connection refused");
    append_attempt_output(&mut history, 2, "second measurement succeeded");
    let output = history.join("\n");
    assert!(output.contains("=== client attempt 1 ==="));
    assert!(output.contains("first connection refused"));
    assert!(output.contains("=== client attempt 2 ==="));
    assert!(output.contains("second measurement succeeded"));
}

#[test]
fn test_transient() {
    assert!(is_transient_error("iperf3: error - Connection refused"));
    assert!(is_transient_error(
        "iperf3: error - the server is busy running a test. try again later"
    ));
    assert!(!is_transient_error("iperf3: error - bad file descriptor"));
}

#[test]
fn test_args() {
    let req = IperfClientReq {
        dst: "192.168.1.3".into(),
        bind_ip: "192.168.1.2".into(),
        port: 56001,
        duration: 120,
        udp: true,
        v6: false,
        extra: vec!["-b".into(), "500m".into()],
    };
    let a = client_args(&req);
    assert_eq!(
        a.join(" "),
        "-c 192.168.1.3 -B 192.168.1.2 -p 56001 -t 120 -i 1 -f m -4 -u -b 500m"
    );
    let sreq = IperfServerStartReq {
        bind_ip: "fe80::1%12".into(),
        port: 56001,
        v6: true,
        ..Default::default()
    };
    let sa = server_args(&sreq);
    assert_eq!(sa.join(" "), "-s -B fe80::1%12 -p 56001 -i 1 -f m -6");
}

#[test]
fn injected_process_executor_drives_client_events_without_iperf_binary() {
    let lines = vec![
        "[  5] local 192.0.2.1 port 50000 connected to 192.0.2.2 port 56000".into(),
        "[  5]   0.00-1.00 sec  100 MBytes  800 Mbits/sec".into(),
    ];
    let fake = FakeProcessExecutor::new(
        true,
        lines.clone(),
        CmdOut {
            ok: true,
            stdout: lines.join("\n"),
            ..Default::default()
        },
    );
    let req = IperfClientReq {
        dst: "192.0.2.2".into(),
        bind_ip: "192.0.2.1".into(),
        port: 56_000,
        duration: 1,
        ..Default::default()
    };
    let mut events = Vec::new();
    let out = run_client_controlled_with_executor(
        &fake,
        "iperf3-not-installed",
        &req,
        None,
        |_line| {},
        |event| events.push(event),
    );
    assert!(out.ok);
    assert!(out.cmd.contains("--forceflush"));
    assert!(events
        .iter()
        .any(|event| event.kind == IperfEventKind::Connected));
    assert!(events
        .iter()
        .any(|event| event.kind == IperfEventKind::Traffic));
}

#[test]
fn injected_refused_reap_stops_retry_and_preserves_cleanup_evidence() {
    let fake = FakeProcessExecutor::new(
        false,
        Vec::new(),
        CmdOut {
            cancelled: true,
            stderr: "回收子进程失败: fake refuses exit".into(),
            ..Default::default()
        },
    );
    let req = IperfClientReq {
        dst: "192.0.2.2".into(),
        bind_ip: "192.0.2.1".into(),
        port: 56_000,
        duration: 1,
        ..Default::default()
    };
    let out = run_client_controlled_with_executor(
        &fake,
        "iperf3-not-installed",
        &req,
        None,
        |_line| {},
        |_event| {},
    );
    assert!(out.cancelled);
    assert_eq!(out.cleanup_confirmed, Some(false));
    assert!(out.output.contains("回收子进程失败"));
}

#[test]
fn test_job_manager_allows_32_concurrent_clients() {
    const JOBS: usize = 32;
    let mgr = IperfClientJobMgr::new();
    let active = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (started_tx, started_rx) = mpsc::channel();
    let mut ids = Vec::new();

    for _ in 0..JOBS {
        let active = Arc::clone(&active);
        let gate = Arc::clone(&gate);
        let started_tx = started_tx.clone();
        ids.push(mgr.start_job(move |_cancel, events| {
            active.fetch_add(1, Ordering::SeqCst);
            events.lock().unwrap().push(IperfFlowEvent {
                kind: IperfEventKind::Started,
                line: "fake client started".into(),
                ..Default::default()
            });
            let _ = started_tx.send(());

            let (lock, cv) = &*gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cv.wait(released).unwrap();
            }
            active.fetch_sub(1, Ordering::SeqCst);
            IperfClientOut {
                ok: true,
                output: "fake client completed".into(),
                ..Default::default()
            }
        }));
    }
    drop(started_tx);

    let mut all_started = true;
    for _ in 0..JOBS {
        if started_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            all_started = false;
            break;
        }
    }
    let active_at_barrier = active.load(Ordering::SeqCst);
    {
        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }

    assert!(all_started, "32 个异步 client 未能及时全部启动");
    assert_eq!(active_at_barrier, JOBS);

    let deadline = Instant::now() + Duration::from_secs(2);
    for id in &ids {
        loop {
            let status = mgr.status(id, 0).unwrap();
            if status.done {
                assert_eq!(status.events.len(), 1);
                assert!(status.result.unwrap().ok);
                break;
            }
            assert!(Instant::now() < deadline, "job {id} 未及时结束");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    for id in ids {
        assert_eq!(mgr.stop(&id).unwrap(), (true, true));
    }
}

/// 子进程模式下只负责占住一个真实 TCP 监听端口，供父测试验证 kill+wait
/// 返回后端口确实已经可以重新绑定。普通测试进程中该测试立即返回。
#[test]
fn helper_tcp_listener_process() {
    if std::env::var("CPE_TEST_LISTENER_HELPER").as_deref() != Ok("1") {
        return;
    }
    let port: u16 = std::env::var("CPE_TEST_LISTENER_PORT")
        .expect("helper port")
        .parse()
        .expect("numeric helper port");
    let host = std::env::var("CPE_TEST_LISTENER_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let _listener = std::net::TcpListener::bind((host.as_str(), port))
        .expect("helper must bind requested port");
    std::thread::sleep(Duration::from_secs(60));
}

fn spawn_test_listener() -> (u16, Child) {
    let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cmd::iperf::tests::helper_tcp_listener_process",
            "--nocapture",
        ])
        .env("CPE_TEST_LISTENER_HELPER", "1")
        .env("CPE_TEST_LISTENER_PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "helper 未及时监听端口 {port}");
        std::thread::sleep(Duration::from_millis(10));
    }
    (port, child)
}

fn register_test_server(mgr: &IperfServerMgr, req: &IperfServerStartReq, child: Child) {
    lock_recover(&mgr.inner).insert(
        req.port,
        SrvEntry {
            child,
            watchdog: None,
            output: Arc::new(Mutex::new(BoundedOutput::new(Some(OUTPUT_LIMIT)))),
            readers: Vec::new(),
            started: Instant::now(),
            expires_at: lease_deadline(req.lease_secs).unwrap(),
            dynamic_lease: req.lease_secs > 0,
            cmd: "test-listener".into(),
            request_id: req.request_id.clone(),
            owner_id: req.owner_id.clone(),
            fingerprint: IperfServerMgr::server_fingerprint(req),
            ready: true,
        },
    );
}

#[test]
fn server_stop_confirms_process_exit_and_releases_port() {
    // 其他并行测试大量使用 IPv4 回环随机端口；单独用 IPv6 回环，避免
    // helper 被杀掉后，恰好被别的测试抢走同一个刚释放的 IPv4 端口。
    let host = "::1";
    let reservation = std::net::TcpListener::bind((host, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);

    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cmd::iperf::tests::helper_tcp_listener_process",
            "--nocapture",
        ])
        .env("CPE_TEST_LISTENER_HELPER", "1")
        .env("CPE_TEST_LISTENER_PORT", port.to_string())
        .env("CPE_TEST_LISTENER_HOST", host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect((host, port)).is_err() {
        assert!(Instant::now() < deadline, "helper 未及时监听端口 {port}");
        std::thread::sleep(Duration::from_millis(10));
    }

    let mgr = IperfServerMgr::new();
    lock_recover(&mgr.inner).insert(
        port,
        SrvEntry {
            child,
            watchdog: None,
            output: Arc::new(Mutex::new(BoundedOutput::new(Some(OUTPUT_LIMIT)))),
            readers: Vec::new(),
            started: Instant::now(),
            expires_at: lease_deadline(60).unwrap(),
            dynamic_lease: true,
            cmd: "test-listener".into(),
            request_id: "server-stop-test".into(),
            owner_id: "owner-stop-test".into(),
            fingerprint: "test-listener-fingerprint".into(),
            ready: true,
        },
    );

    let stopped = mgr
        .stop_checked(port, "server-stop-test", Duration::ZERO)
        .unwrap();
    assert!(stopped.existed);
    assert!(stopped.terminated);
    let rebound = std::net::TcpListener::bind((host, port));
    assert!(
        rebound.is_ok(),
        "stop 成功返回后端口 {port} 仍不可重新绑定: {:?}",
        rebound.err()
    );

    // 丢失第一次 stop 响应后重放同一 request，仍应幂等成功。
    let replay = mgr
        .stop_checked(port, "server-stop-test", Duration::ZERO)
        .unwrap();
    assert_eq!(replay.existed, stopped.existed);
    assert!(replay.terminated);
}

#[test]
fn real_iperf_server_start_replay_stop_and_rebind_when_available() {
    let Some(bin) = crate::cmd::tools::find_iperf3() else {
        return;
    };
    let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let mgr = IperfServerMgr::new();
    let req = IperfServerStartReq {
        bind_ip: "127.0.0.1".into(),
        port,
        v6: false,
        request_id: "real-iperf-server".into(),
        owner_id: "owner-real-iperf-server".into(),
        lease_secs: 60,
    };

    let first = mgr.start(&bin, &req).unwrap();
    let replay = mgr.start(&bin, &req).unwrap();
    assert_eq!(first, replay);
    let stopped = mgr
        .stop_checked(port, &req.request_id, Duration::ZERO)
        .unwrap();
    assert!(stopped.existed && stopped.terminated);
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
        "真实 iperf3 stop 返回后端口必须立即可重绑"
    );
}

#[test]
fn real_iperf_client_cancel_waits_for_process_reap_when_available() {
    let Some(bin) = crate::cmd::tools::find_iperf3() else {
        return;
    };
    let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let servers = IperfServerMgr::new();
    let server_req = IperfServerStartReq {
        bind_ip: "127.0.0.1".into(),
        port,
        v6: false,
        request_id: "real-client-server".into(),
        owner_id: "owner-real-client".into(),
        lease_secs: 60,
    };
    servers.start(&bin, &server_req).unwrap();

    let clients = IperfClientJobMgr::new();
    let id = clients
        .start_request(
            bin,
            IperfClientStartReq {
                request: IperfClientReq {
                    dst: "127.0.0.1".into(),
                    bind_ip: "127.0.0.1".into(),
                    port,
                    duration: 30,
                    udp: false,
                    v6: false,
                    extra: Vec::new(),
                },
                request_id: "real-iperf-client".into(),
                owner_id: "owner-real-client".into(),
                lease_secs: 60,
            },
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let stopped = clients.stop_checked(&id, Duration::from_secs(5)).unwrap();
    assert!(stopped.existed && stopped.terminated);
    assert!(clients.status(&id, 0).is_err());

    servers
        .stop_checked(port, &server_req.request_id, Duration::ZERO)
        .unwrap();
    assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
}

#[test]
fn stale_server_stop_never_kills_new_request_and_request_id_is_global() {
    let (port, child) = spawn_test_listener();
    let mgr = IperfServerMgr::new();
    let req = IperfServerStartReq {
        bind_ip: "127.0.0.1".into(),
        port,
        v6: false,
        request_id: "new-server-request".into(),
        owner_id: "owner-new-server".into(),
        lease_secs: 60,
    };
    register_test_server(&mgr, &req, child);

    let stale = mgr
        .stop_checked(port, "old-server-request", Duration::ZERO)
        .unwrap();
    assert!(!stale.existed);
    assert!(stale.terminated);
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "迟到的旧 request stop 不得关闭当前 listener"
    );

    // 相同 request + 相同参数的 start 只续租并复用，不再 spawn。
    assert_eq!(mgr.start("unused-binary", &req).unwrap(), "test-listener");

    let mut conflicting = req.clone();
    conflicting.port = if port == u16::MAX { port - 1 } else { port + 1 };
    assert!(
        mgr.start("unused-binary", &conflicting).is_err(),
        "同一 request_id 不能同时代表另一个端口"
    );

    let stopped = mgr
        .stop_checked(port, &req.request_id, Duration::ZERO)
        .unwrap();
    assert!(stopped.existed && stopped.terminated);
}

#[test]
fn replay_never_reuses_a_live_but_unready_server_entry() {
    let (port, child) = spawn_test_listener();
    let mgr = IperfServerMgr::new();
    let req = IperfServerStartReq {
        bind_ip: "127.0.0.1".into(),
        port,
        v6: false,
        request_id: "unready-server-request".into(),
        owner_id: "owner-unready-server".into(),
        lease_secs: 60,
    };
    register_test_server(&mgr, &req, child);
    lock_recover(&mgr.inner).get_mut(&port).unwrap().ready = false;

    let replay = mgr.start("binary-that-must-not-exist", &req);
    assert!(replay.is_err(), "未就绪 Child 不能被重放 start 当成成功");
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "重放 start 前必须先回收未就绪 Child"
    );
    assert!(!lock_recover(&mgr.inner).contains_key(&port));
}

#[test]
fn server_owner_cleanup_and_dynamic_lease_are_isolated_and_idempotent() {
    let mgr = IperfServerMgr::new();
    let (port_a, child_a) = spawn_test_listener();
    let req_a = IperfServerStartReq {
        bind_ip: "127.0.0.1".into(),
        port: port_a,
        v6: false,
        request_id: "server-owner-a".into(),
        owner_id: "owner-a".into(),
        lease_secs: 60,
    };
    register_test_server(&mgr, &req_a, child_a);
    let (port_b, child_b) = spawn_test_listener();
    let req_b = IperfServerStartReq {
        bind_ip: "127.0.0.1".into(),
        port: port_b,
        v6: false,
        request_id: "server-owner-b".into(),
        owner_id: "owner-b".into(),
        lease_secs: 60,
    };
    register_test_server(&mgr, &req_b, child_b);

    let cleanup_a = mgr.stop_owner("owner-a", Duration::ZERO);
    assert_eq!(cleanup_a.stopped, 1);
    assert!(cleanup_a.errors.is_empty());
    assert!(TcpStream::connect(("127.0.0.1", port_a)).is_err());
    assert!(TcpStream::connect(("127.0.0.1", port_b)).is_ok());
    let replay_a = mgr.stop_owner("owner-a", Duration::ZERO);
    assert_eq!(replay_a.stopped, 0);
    assert!(replay_a.errors.is_empty());

    lock_recover(&mgr.inner)
        .get_mut(&port_b)
        .unwrap()
        .expires_at = Some(Instant::now());
    assert!(mgr.sweep(Duration::MAX).is_empty());
    assert!(!lock_recover(&mgr.inner).contains_key(&port_b));
    assert!(TcpStream::connect(("127.0.0.1", port_b)).is_err());
}

#[test]
fn client_stop_waits_for_worker_and_is_idempotent() {
    let mgr = IperfClientJobMgr::new();
    let active = Arc::new(AtomicUsize::new(0));
    let active_runner = Arc::clone(&active);
    let id = mgr
        .start_job_managed(
            "client-stop-test".into(),
            "owner-client-test".into(),
            60,
            "fingerprint".into(),
            move |cancel, _events, _job_epoch| {
                active_runner.store(1, Ordering::SeqCst);
                while !cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                // 模拟子进程 kill/wait 与 reader join 的收尾延迟。
                std::thread::sleep(Duration::from_millis(40));
                active_runner.store(0, Ordering::SeqCst);
                IperfClientOut {
                    cancelled: true,
                    output: "cancelled and reaped".into(),
                    ..Default::default()
                }
            },
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while active.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }

    let stopped = mgr.stop_checked(&id, Duration::from_secs(2)).unwrap();
    assert!(stopped.existed);
    assert!(stopped.terminated);
    let result = stopped.result.as_ref().expect("stop 应回传最终输出");
    assert!(result.cancelled);
    assert_eq!(result.output, "cancelled and reaped");
    assert_eq!(active.load(Ordering::SeqCst), 0);

    let replay = mgr.stop_checked(&id, Duration::from_secs(2)).unwrap();
    assert_eq!(replay.existed, stopped.existed);
    assert!(replay.terminated);
    assert_eq!(
        replay.result.as_ref().map(|result| result.output.as_str()),
        Some("cancelled and reaped")
    );
}

#[test]
fn status_done_snapshot_includes_events_pushed_before_completion_set() {
    // 回归：status() 必须先读完成状态、再复制事件。
    // 若先复制事件，合法的并发交错（worker 在两次读取之间推送尾事件
    // 并写 completion）会让调用方看到 done=true 却缺少 Traffic/Ended。
    let mgr = IperfClientJobMgr::new();
    let (pushed_tx, pushed_rx) = mpsc::channel::<()>();
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let id = mgr
        .start_job_managed(
            "status-tail-race".into(),
            "owner-status-tail".into(),
            60,
            "fingerprint".into(),
            move |cancel, events, _job_epoch| {
                // worker 先推送 Started，再阻塞，等待测试释放。
                events.lock().unwrap().push(IperfFlowEvent {
                    kind: IperfEventKind::Started,
                    elapsed_ms: 0,
                    ..Default::default()
                });
                pushed_tx.send(()).unwrap();
                while !cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(2));
                }
                IperfClientOut {
                    cancelled: true,
                    output: "status-tail-race".into(),
                    ..Default::default()
                }
            },
        )
        .unwrap();
    pushed_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    // 通过内部 registry 拿到同一个 entry，模拟 worker 在
    // status() 的“复制事件→读完成”之间完成收尾。
    let entry = lock_recover(&mgr.inner)
        .jobs
        .get(&id)
        .cloned()
        .expect("job registered");

    // 测试线程先持有 completion 锁：
    //  - 旧实现先复制事件（此时只有 Started），再阻塞在 completion 锁；
    //  - 新实现先读 completion，会阻塞在 completion 锁且尚未复制事件。
    let mut completion_guard = lock_recover(&entry.completion.result);
    let (status_tx, status_rx) = mpsc::channel::<IperfClientStatusOut>();
    let mgr_for_thread = &mgr;
    let id_for_thread = id.clone();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            entered_tx.send(()).unwrap();
            let out = mgr_for_thread.status(&id_for_thread, 0).unwrap();
            status_tx.send(out).unwrap();
        });
        // 等 status 线程进入并阻塞在 completion 锁。
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        // 在 status() 卡在 completion 锁期间，worker 补推尾事件
        // 并写完成状态——即报告描述的合法并发交错。
        {
            let mut guard = lock_recover(&entry.events);
            guard.push(IperfFlowEvent {
                kind: IperfEventKind::Traffic,
                elapsed_ms: 10,
                mbps: Some(100.0),
                ..Default::default()
            });
            guard.push(IperfFlowEvent {
                kind: IperfEventKind::Ended,
                elapsed_ms: 20,
                ..Default::default()
            });
        }
        *completion_guard = Some(IperfClientOut {
            ok: true,
            output: "completed".into(),
            ..Default::default()
        });
        drop(completion_guard);

        let status = status_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("status() 应返回");
        assert!(status.done, "完成状态必须可见");
        // 尾事件不能因为 done=true 快照截断而丢失：
        // 一旦 done=true，快照必须包含 completion 写入前推送的全部事件。
        let kinds: Vec<_> = status
            .events
            .iter()
            .map(|event| event.kind.clone())
            .collect();
        assert_eq!(
            kinds,
            vec![
                IperfEventKind::Started,
                IperfEventKind::Traffic,
                IperfEventKind::Ended,
            ]
        );
    });

    // 触发 worker 退出并回收线程，避免泄漏。
    entry.cancel.store(true, Ordering::SeqCst);
    let _ = mgr.stop_checked(&id, Duration::from_secs(2));
}

#[test]
fn client_stop_all_reaps_every_registered_external_job() {
    let mgr = IperfClientJobMgr::new();
    let active = Arc::new(AtomicUsize::new(0));
    let mut ids = Vec::new();
    for index in 0..2 {
        let active_runner = Arc::clone(&active);
        ids.push(
            mgr.start_job_managed(
                format!("client-stop-all-{index}"),
                format!("owner-stop-all-{index}"),
                60,
                format!("fingerprint-{index}"),
                move |cancel, _events, _job_epoch| {
                    active_runner.fetch_add(1, Ordering::SeqCst);
                    while !cancel.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    active_runner.fetch_sub(1, Ordering::SeqCst);
                    IperfClientOut {
                        cancelled: true,
                        output: format!("stopped-{index}"),
                        ..Default::default()
                    }
                },
            )
            .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while active.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }

    let stopped = mgr.stop_all(Duration::from_secs(2));
    assert_eq!(stopped.stopped, 2);
    assert!(stopped.errors.is_empty());
    assert_eq!(active.load(Ordering::SeqCst), 0);
    for id in ids {
        assert!(mgr.status(&id, 0).is_err());
    }
}

#[test]
fn client_request_id_is_idempotent_and_stop_before_start_blocks_revival() {
    let mgr = IperfClientJobMgr::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));

    let runs_first = Arc::clone(&runs);
    let gate_first = Arc::clone(&gate);
    let first = mgr
        .start_job_managed(
            "same-client-request".into(),
            "owner-idempotent".into(),
            60,
            "same-fingerprint".into(),
            move |_cancel, _events, _job_epoch| {
                runs_first.fetch_add(1, Ordering::SeqCst);
                let (lock, cv) = &*gate_first;
                let mut released = lock_recover(lock);
                while !*released {
                    released = cv
                        .wait(released)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                IperfClientOut::default()
            },
        )
        .unwrap();
    let second = mgr
        .start_job_managed(
            "same-client-request".into(),
            "owner-idempotent".into(),
            60,
            "same-fingerprint".into(),
            move |_cancel, _events, _job_epoch| {
                panic!("幂等 start 不应启动第二个 runner");
            },
        )
        .unwrap();
    assert_eq!(first, second);

    let deadline = Instant::now() + Duration::from_secs(2);
    while runs.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    {
        let (lock, cv) = &*gate;
        *lock_recover(lock) = true;
        cv.notify_all();
    }
    mgr.stop_checked(&first, Duration::from_secs(2)).unwrap();

    let stopped_unknown = mgr
        .stop_checked("late-client-request", Duration::from_secs(1))
        .unwrap();
    assert!(!stopped_unknown.existed);
    assert!(stopped_unknown.terminated);
    let late_start = mgr.start_job_managed(
        "late-client-request".into(),
        "owner-idempotent".into(),
        60,
        "late-fingerprint".into(),
        move |_cancel, _events, _job_epoch| IperfClientOut::default(),
    );
    assert!(
        late_start.is_err(),
        "stop-before-start 后不允许迟到请求复活"
    );
}

#[test]
fn idempotent_client_start_renews_its_dynamic_lease() {
    let mgr = IperfClientJobMgr::new();
    let id = mgr
        .start_job_managed(
            "client-lease-renew".into(),
            "owner-lease-renew".into(),
            1,
            "lease-fingerprint".into(),
            move |cancel, _events, _job_epoch| {
                while !cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                IperfClientOut::default()
            },
        )
        .unwrap();
    let before = {
        let registry = lock_recover(&mgr.inner);
        let value = *lock_recover(&registry.jobs[&id].expires_at);
        value
    };
    std::thread::sleep(Duration::from_millis(2));
    let replay = mgr
        .start_job_managed(
            id.clone(),
            "owner-lease-renew".into(),
            60,
            "lease-fingerprint".into(),
            move |_cancel, _events, _job_epoch| panic!("幂等续租不应启动第二个 worker"),
        )
        .unwrap();
    let after = {
        let registry = lock_recover(&mgr.inner);
        let value = *lock_recover(&registry.jobs[&id].expires_at);
        value
    };
    assert_eq!(replay, id);
    assert!(after > before);
    mgr.stop_checked(&id, Duration::from_secs(2)).unwrap();
}

#[test]
fn client_owner_cleanup_and_dynamic_lease_are_isolated_and_idempotent() {
    let mgr = IperfClientJobMgr::new();
    let start = |id: &str, owner: &str| {
        mgr.start_job_managed(
            id.into(),
            owner.into(),
            60,
            format!("fingerprint-{id}"),
            move |cancel, _events, _job_epoch| {
                while !cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                IperfClientOut {
                    cancelled: true,
                    ..Default::default()
                }
            },
        )
        .unwrap()
    };
    let id_a = start("client-owner-a", "owner-a");
    let id_b = start("client-owner-b", "owner-b");

    let cleanup_a = mgr.stop_owner("owner-a", Duration::from_secs(2));
    assert_eq!(cleanup_a.stopped, 1);
    assert!(cleanup_a.errors.is_empty());
    assert!(mgr.status(&id_a, 0).is_err());
    assert!(mgr.status(&id_b, 0).is_ok());
    let replay_a = mgr.stop_owner("owner-a", Duration::from_secs(2));
    assert_eq!(replay_a.stopped, 0);
    assert!(replay_a.errors.is_empty());

    {
        let registry = lock_recover(&mgr.inner);
        *lock_recover(&registry.jobs[&id_b].expires_at) = Some(Instant::now());
    }
    assert!(mgr.sweep(Duration::MAX).is_empty());
    assert!(mgr.status(&id_b, 0).is_err());
}

#[test]
fn client_stop_timeout_keeps_entry_for_later_confirmation() {
    let mgr = IperfClientJobMgr::new();
    let release = Arc::new(AtomicBool::new(false));
    let release_runner = Arc::clone(&release);
    let id = mgr
        .start_job_managed(
            "client-stop-timeout".into(),
            "owner-timeout".into(),
            60,
            "timeout-fingerprint".into(),
            move |_cancel, _events, _job_epoch| {
                while !release_runner.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                IperfClientOut::default()
            },
        )
        .unwrap();

    assert!(mgr.stop_checked(&id, Duration::from_millis(20)).is_err());
    assert!(mgr.status(&id, 0).is_ok(), "未确认停止时必须保留 entry");
    release.store(true, Ordering::SeqCst);
    let stopped = mgr.stop_checked(&id, Duration::from_secs(2)).unwrap();
    assert!(stopped.terminated);
}

#[test]
fn client_worker_panic_still_notifies_and_can_be_reaped() {
    let mgr = IperfClientJobMgr::new();
    let id = mgr
        .start_job_managed(
            "client-panic".into(),
            "owner-panic".into(),
            60,
            "panic-fingerprint".into(),
            move |_cancel, _events, _job_epoch| panic!("synthetic runner panic"),
        )
        .unwrap();
    let stopped = mgr.stop_checked(&id, Duration::from_secs(2)).unwrap();
    assert!(stopped.terminated);
}

/// client 流式执行时带着输出上限：不带的话 `-P 32` 跑到 9 小时左右，一份输出就
/// 超过主控读响应的上限，结果读不回来（见 `OUTPUT_LIMIT`）。
#[test]
fn the_client_streams_iperf_with_the_bounded_output_limit() {
    let executor = FakeProcessExecutor::new(
        false,
        vec!["[  5]   0.00-1.00   sec   283 MBytes  2372 Mbits/sec".into()],
        CmdOut {
            ok: true,
            stdout: TCP_SAMPLE.into(),
            ..Default::default()
        },
    );
    let req = IperfClientReq {
        dst: "192.168.1.3".into(),
        bind_ip: "192.168.1.2".into(),
        port: 56000,
        duration: 10,
        ..Default::default()
    };
    let out = run_client_controlled_inner(&executor, false, "iperf3", &req, None, |_| {}, |_| {});
    assert!(out.ok, "{}", out.output);
    let specs = executor.streamed_specs.lock().unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].stdout_limit, Some(OUTPUT_LIMIT));
}

/// 封顶只省略中间的逐秒行：判定读的汇总行在末尾，省略后解析结果与完整文本逐项相同。
#[test]
fn a_bounded_iperf_output_still_yields_the_same_summary() {
    let limit = OutputLimit {
        head_bytes: 512,
        tail_bytes: 2048,
    };
    for sample in [TCP_RETR_SUM_SAMPLE, UDP_SAMPLE] {
        let (head, summary) = sample
            .split_once("- - - - - - - - - - - - - - - - - - - - - - - - -")
            .expect("样本里有汇总分隔线");
        let mut full = head.to_string();
        for second in 0..50_000 {
            full.push_str(&format!(
                "[  5] {second:>5}.00-{:>5}.00 sec   112 MBytes   940 Mbits/sec\n",
                second + 1
            ));
        }
        full.push_str("- - - - - - - - - - - - - - - - - - - - - - - - -");
        full.push_str(summary);

        let mut bounded = BoundedOutput::new(Some(limit));
        for line in full.split_inclusive('\n') {
            bounded.push(line);
        }
        let rendered = bounded.render();
        assert!(rendered.len() < limit.head_bytes + limit.tail_bytes + 200);
        assert!(rendered.contains("中间省略"), "{rendered}");

        let (whole, kept) = (parse_output(&full), parse_output(&rendered));
        assert_eq!(kept.sender_mbps, whole.sender_mbps);
        assert_eq!(kept.receiver_mbps, whole.receiver_mbps);
        assert_eq!(kept.tcp_retransmits, whole.tcp_retransmits);
        assert_eq!(kept.udp_lost_datagrams, whole.udp_lost_datagrams);
        assert_eq!(kept.udp_total_datagrams, whole.udp_total_datagrams);
        assert!(kept.receiver_mbps.is_some());
    }
}

/// 实时事件与汇总解析对同一行文本换算出同一个速率。
///
/// 实时那份以前按 1000 进位算 Byte 单位：`11.9 MBytes/sec` 在汇总里约 100 Mbps、
/// 在流事件里只有 95.2 Mbps，进度页和报告的工具自报速率对不上。
#[test]
fn live_events_and_the_summary_convert_rates_identically() {
    for line in [
        "[  5]   1.00-2.00   sec  11.9 MBytes  11.9 MBytes/sec",
        "[  5]   1.00-2.00   sec  1.09 GBytes  9.35 Gbits/sec",
        "[  5]   1.00-2.00   sec   112 MBytes   940 Mbits/sec",
        "[  5]   1.00-2.00   sec  1.20 MBytes  9600 Kbits/sec",
    ] {
        let live = classify_live_line(line, 0)
            .and_then(|event| event.mbps)
            .expect(line);
        let summary = parse_output(line).last_mbps.expect(line);
        assert_eq!(live, summary, "{line}");
    }
}

/// 就绪探测：有人监听就立刻返回；没人监听时按给定时限报超时，不提前误报、也不卡住。
#[test]
fn server_readiness_probe_succeeds_on_a_listener_and_times_out_without_one() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let started = Instant::now();
    wait_server_tcp_ready("127.0.0.1".into(), port, Duration::from_secs(5), || Ok(()))
        .expect("有监听时应当就绪");
    assert!(started.elapsed() < Duration::from_secs(2));

    drop(listener);
    let started = Instant::now();
    let error = wait_server_tcp_ready("127.0.0.1".into(), port, Duration::from_millis(600), || {
        Ok(())
    })
    .expect_err("没人监听时必须超时");
    assert!(error.contains("未响应 TCP connect"), "{error}");
    assert!(started.elapsed() >= Duration::from_millis(600));

    // 子进程先退出时立刻把原因交回去，不等满时限。
    let error = wait_server_tcp_ready("127.0.0.1".into(), port, Duration::from_secs(5), || {
        Err("server 已退出".into())
    })
    .expect_err("子进程退出必须立刻报错");
    assert_eq!(error, "server 已退出");
}
