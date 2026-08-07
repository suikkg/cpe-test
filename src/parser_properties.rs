//! Deterministic property/fuzz smoke tests for text parsers.
//!
//! The generator deliberately uses arbitrary bytes and bounded input sizes so
//! this suite runs in the normal cross-platform CI without a VM or a network.
//! Long-running libFuzzer targets can reuse the same parser calls later.

#![cfg(test)]

use crate::cmd::ctstraffic;
use crate::cmd::ipconfig;
use crate::cmd::iperf;
use crate::http_client;
use crate::ping;
use crate::protocol::CtsTrafficProtocol;
use std::io::{BufReader, Cursor};
use std::panic::{catch_unwind, AssertUnwindSafe};
struct ByteGenerator(u64);

impl ByteGenerator {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 16) as u32
    }

    fn input(&mut self) -> String {
        let len = (self.next_u32() as usize) % 384;
        let mut bytes = Vec::with_capacity(len + 96);
        for _ in 0..len {
            bytes.push(self.next_u32() as u8);
        }
        // Mix in protocol-shaped fragments often enough to exercise numeric,
        // ANSI, status, and truncated-line branches rather than only garbage.
        match self.next_u32() % 5 {
            0 => bytes.extend_from_slice(b"[ 5] 0.00-1.00 sec 1.00 MBytes 8.00 Mbits/sec\n"),
            1 => bytes.extend_from_slice(b"Total Bytes Recv : 1000\nTotal Time : 10 ms.\n"),
            2 => bytes.extend_from_slice(
                "时间<1ms\nReply from 192.0.2.1: bytes=32 time=1ms\n".as_bytes(),
            ),
            3 => bytes.extend_from_slice(b"\x1b[31m[10.000] 2.6x^9 1.2x^6 8 1 0 0\x1b[0m\n"),
            _ => bytes.extend_from_slice(b"SuccessfulConnections [1] NetworkErrors [0]\n"),
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn parser_snapshot(input: &str) -> (String, String, String, (u32, u32, u32, bool, bool)) {
    let iperf = iperf::parse_output(input);
    let cts_tcp = ctstraffic::parse_output(input, CtsTrafficProtocol::Tcp);
    let cts_udp = ctstraffic::parse_output(input, CtsTrafficProtocol::Udp);
    let ping = ping::parse(input, 32);
    (
        format!("{iperf:?}"),
        format!("{cts_tcp:?}"),
        format!("{cts_udp:?}"),
        (
            ping.sent,
            ping.received,
            ping.lost,
            ping.ok,
            ping.loss_pct.is_finite(),
        ),
    )
}

#[test]
fn parser_fuzz_smoke_is_deterministic_and_never_panics() {
    for seed in 1..=2_000u64 {
        let mut generator = ByteGenerator::new(seed);
        let input = generator.input();
        let first = catch_unwind(AssertUnwindSafe(|| parser_snapshot(&input)));
        assert!(first.is_ok(), "parser panic for seed {seed}");
        let second = catch_unwind(AssertUnwindSafe(|| parser_snapshot(&input)))
            .expect("second parser invocation must not panic");
        assert_eq!(
            first.unwrap(),
            second,
            "parser is not deterministic for seed {seed}"
        );

        let (_, _, _, (sent, received, lost, _, finite_loss)) = second;
        assert!(received <= sent, "received exceeds sent for seed {seed}");
        assert!(lost <= sent, "lost exceeds sent for seed {seed}");
        assert!(
            finite_loss,
            "loss percentage became non-finite for seed {seed}"
        );
    }
}

#[test]
fn parser_property_valid_fixtures_preserve_basic_protocol_invariants() {
    let ping = ping::parse(
        "Reply from 192.0.2.2: bytes=32 time=1ms\n\
         Reply from 192.0.2.2: bytes=32 time=2ms\n\
         Packets: Sent = 2, Received = 2, Lost = 0 (0% loss)",
        2,
    );
    assert!(ping.ok);
    assert_eq!((ping.sent, ping.received, ping.lost), (2, 2, 0));
    assert!((0.0..=100.0).contains(&ping.loss_pct));

    let iperf = iperf::parse_output(
        "[ 5] 0.00-1.00 sec 100 MBytes 800 Mbits/sec sender\n\
         [ 5] 0.00-1.00 sec 99 MBytes 792 Mbits/sec receiver",
    );
    assert_eq!(iperf.best_sender(), Some(800.0));
    assert_eq!(iperf.best_receiver(), Some(792.0));
    assert!(iperf.has_measurement());

    let cts = ctstraffic::parse_output(
        "[10.000] 24999840 1 300 0 0 0\nTotal Bytes Recv : 187498800\n",
        CtsTrafficProtocol::Udp,
    );
    assert!(cts.best_rate(CtsTrafficProtocol::Udp).unwrap().is_finite());
    assert!(cts.has_measurement(CtsTrafficProtocol::Udp));
    assert!(cts.best_rate(CtsTrafficProtocol::Udp).unwrap().is_finite());
    assert!(cts.has_measurement(CtsTrafficProtocol::Udp));
}

// ---------------- 速率不变量 ----------------

/// 任意输入解析出的速率都必须是有限、非负的（不能是 NaN/无穷/负数）。
/// 这同时守护 ctsTraffic 的 `x^N` 指数记号：垃圾输入里的超大指数
/// 不得把速率推成 `inf`。
#[test]
fn parser_rates_are_finite_and_non_negative() {
    for seed in 1..=1_000u64 {
        let mut generator = ByteGenerator::new(seed);
        let input = generator.input();

        let iperf = iperf::parse_output(&input);
        for rate in [iperf.sender_mbps, iperf.receiver_mbps, iperf.last_mbps]
            .into_iter()
            .flatten()
        {
            assert!(
                rate.is_finite() && rate >= 0.0,
                "iperf 速率非法 {rate} (seed {seed})"
            );
        }
        let cts_tcp = ctstraffic::parse_output(&input, CtsTrafficProtocol::Tcp);
        for rate in [cts_tcp.send_mbps, cts_tcp.recv_mbps].into_iter().flatten() {
            assert!(
                rate.is_finite() && rate >= 0.0,
                "cts 速率非法 {rate} (seed {seed})"
            );
        }
        if let Some(pct) = cts_tcp.udp_dropped_pct {
            assert!(
                pct.is_finite() && pct >= 0.0,
                "cts 丢包率非法 {pct} (seed {seed})"
            );
        }
    }
}

// ---------------- 无关日志行不改变结果 ----------------

fn iperf_fixture() -> &'static str {
    "[ 5] 0.00-1.00 sec 100 MBytes 800 Mbits/sec sender\n\
     [ 5] 0.00-1.00 sec 99 MBytes 792 Mbits/sec receiver\n\
     [ 5] 1.00-2.00 sec 200 MBytes 1600 Mbits/sec sender\n\
     [ 5] 1.00-2.00 sec 198 MBytes 1584 Mbits/sec receiver"
}

fn cts_fixture() -> &'static str {
    "[10.000] 24999840 1 300 0 0 0\n\
     Total Bytes Recv : 187498800\n\
     Total Bytes Sent : 100000\n\
     Total Time : 10 ms.\n\
     SuccessfulConnections [1] NetworkErrors [0] ProtocolErrors [0]"
}

/// 在有效输出前后插入无关日志行、ANSI 垃圾和空行，解析结果必须不变。
#[test]
fn extra_noise_lines_do_not_change_results() {
    let iperf_base = iperf::parse_output(iperf_fixture());
    let iperf_noisy = iperf::parse_output(&format!(
        "=== run starting ===\n\
         [INFO] connecting to 10.0.0.2:5201\n\
         {}\n\
         \x1b[31m some ansi garbage \x1b[0m\n\
         server exited with code 0\n",
        iperf_fixture()
    ));
    assert_eq!(iperf_noisy.best_sender(), iperf_base.best_sender());
    assert_eq!(iperf_noisy.best_receiver(), iperf_base.best_receiver());

    let cts_base = ctstraffic::parse_output(cts_fixture(), CtsTrafficProtocol::Udp);
    let cts_noisy = ctstraffic::parse_output(
        &format!(
            "noise noise noise\n{}\n[12.500] 0 0 0 0 0 0\n",
            cts_fixture()
        ),
        CtsTrafficProtocol::Udp,
    );
    assert_eq!(cts_noisy.total_bytes_recv, cts_base.total_bytes_recv);
    assert_eq!(
        cts_noisy.successful_connections,
        cts_base.successful_connections
    );
    assert_eq!(
        cts_noisy.best_rate(CtsTrafficProtocol::Udp),
        cts_base.best_rate(CtsTrafficProtocol::Udp)
    );
}

// ---------------- 换行/分块不变性 ----------------

/// 同一份输出用 `\n`、`\r\n` 或行间空行表示，解析结果必须一致。
#[test]
fn line_endings_and_blank_lines_preserve_results() {
    let lf = iperf_fixture();
    let crlf = lf.replace('\n', "\r\n");
    let blank_lines = lf
        .lines()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    for variant in [lf, crlf.as_str(), blank_lines.as_str()] {
        let parsed = iperf::parse_output(variant);
        assert_eq!(parsed.best_sender(), Some(1600.0), "变体: {variant:?}");
        assert_eq!(parsed.best_receiver(), Some(1584.0), "变体: {variant:?}");
    }

    let cts = cts_fixture();
    let cts_crlf = cts.replace('\n', "\r\n");
    let cts_blank = cts
        .lines()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    for variant in [cts, cts_crlf.as_str(), cts_blank.as_str()] {
        let parsed = ctstraffic::parse_output(variant, CtsTrafficProtocol::Udp);
        assert_eq!(
            parsed.total_bytes_recv,
            Some(187_498_800),
            "变体: {variant:?}"
        );
        assert_eq!(parsed.successful_connections, Some(1));
    }
}

// ---------------- 重复 summary 不重复累计 ----------------

/// 同一份 summary 重复出现三次，字节数/错误数/速率不得累计翻倍。
#[test]
fn repeated_summary_does_not_accumulate() {
    let once = iperf::parse_output(iperf_fixture());
    let thrice = format!("{}{}{}", iperf_fixture(), iperf_fixture(), iperf_fixture());
    let repeated = iperf::parse_output(&thrice);
    assert_eq!(repeated.best_sender(), once.best_sender());
    assert_eq!(repeated.best_receiver(), once.best_receiver());

    let cts_once = ctstraffic::parse_output(cts_fixture(), CtsTrafficProtocol::Udp);
    let cts_thrice_text = format!("{}{}{}", cts_fixture(), cts_fixture(), cts_fixture());
    let cts_thrice = ctstraffic::parse_output(&cts_thrice_text, CtsTrafficProtocol::Udp);
    assert_eq!(
        cts_thrice.total_bytes_recv, cts_once.total_bytes_recv,
        "重复 summary 不得累计字节数"
    );
    assert_eq!(
        cts_thrice.successful_connections,
        cts_once.successful_connections
    );
    assert_eq!(cts_thrice.network_errors, cts_once.network_errors);
    assert_eq!(cts_thrice.protocol_errors, cts_once.protocol_errors);
}

// ---------------- HTTP chunked body 解析器 ----------------

/// 同一 body 按任意 chunk 大小分块，解码结果必须与原始内容一致（无损）。
/// 使用单字节 ASCII 正文，确保任意字节边界分块都不会跨 UTF-8 字符；
/// 非 UTF-8 垃圾输入的健壮性由 malformed 用例覆盖。
#[test]
fn chunked_body_chunk_splitting_is_lossless() {
    for seed in 1..=300u64 {
        let mut generator = ByteGenerator::new(seed);
        let len = (generator.next_u32() as usize) % 512;
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            // 单字节 ASCII：chunk 边界永远对齐字符。
            bytes.push((generator.next_u32() % 0x80) as u8);
        }
        let expected = String::from_utf8(bytes).expect("ASCII 永远合法 UTF-8");
        let mut wire = String::new();
        let mut remaining = expected.as_str();
        while !remaining.is_empty() {
            let chunk = (generator.next_u32() as usize) % 128 + 1;
            let take = chunk.min(remaining.len());
            wire.push_str(&format!("{take:x}\r\n"));
            wire.push_str(&remaining[..take]);
            wire.push_str("\r\n");
            remaining = &remaining[take..];
        }
        wire.push_str("0\r\n\r\n");

        let decoded = catch_unwind(AssertUnwindSafe(|| {
            http_client::read_chunked_body(&mut BufReader::new(Cursor::new(wire.clone())))
        }))
        .expect("chunked 解码不得 panic");
        assert_eq!(
            decoded.as_deref(),
            Ok(expected.as_str()),
            "分块解码不一致 (seed {seed})"
        );
    }
}

/// 损坏的 chunked wire 必须返回 Err 而不是 panic，也不能把垃圾解码成
/// “看似成功”的非空正文。
#[test]
fn chunked_body_rejects_malformed_wire_without_panic() {
    let malformed = [
        "zz\r\nhello\r\n0\r\n\r\n",
        "5\r\nabc",
        "5\r\nabcde",
        "0",
        "",
        "5\r\nabcde\r\n",
        "0\r\n\r\n",
    ];
    for wire in malformed {
        let result = catch_unwind(AssertUnwindSafe(|| {
            http_client::read_chunked_body(&mut BufReader::new(Cursor::new(wire.to_string())))
        }));
        assert!(result.is_ok(), "malformed chunked 不得 panic: {wire:?}");
        if let Ok(body) = result.unwrap() {
            assert!(
                body.is_empty(),
                "损坏 wire 不得解码出非空正文: {wire:?} -> {body:?}"
            );
        } // Err 表示拒绝损坏输入，是预期行为
    }
}

// ---------------- 网卡输出解析器（ipconfig /all） ----------------

fn ipconfig_fixture() -> &'static str {
    "以太网适配器 以太网:\n   IPv4 地址 . . . . . . . . . . . . : 192.168.1.2(首选)\n   本地链接 IPv6 地址. . . . . . . . : fe80::c4b:1234%12(首选)\n   子网掩码  . . . . . . . . . . . . : 255.255.255.0\n\n无线局域网适配器 WLAN:\n   媒体状态 . . . . . . . . . . . . : 媒体已断开连接\n   IPv4 地址 . . . . . . . . . . . . : 10.0.0.5"
}

/// ipconfig 解析器对任意输入不 panic；有效 fixture 前后插入噪音行，
/// 适配器与 IP 解析结果不变。
#[test]
fn ipconfig_parser_never_panics_and_noise_is_inert() {
    for seed in 1..=500u64 {
        let mut generator = ByteGenerator::new(seed);
        let input = generator.input();
        let parsed = catch_unwind(AssertUnwindSafe(|| ipconfig::parse(&input)));
        assert!(parsed.is_ok(), "ipconfig parse panic for seed {seed}");
    }

    let base = ipconfig::parse(ipconfig_fixture());
    let base_ips: Vec<Option<String>> = base.iter().map(|adapter| adapter.ipv4.clone()).collect();
    assert_eq!(
        base_ips,
        vec![Some("192.168.1.2".into()), Some("10.0.0.5".into())]
    );

    let noisy = format!(
        "boot log line\n\n{}\n\nunrelated tail\n",
        ipconfig_fixture()
    );
    let with_noise = ipconfig::parse(&noisy);
    let noise_ips: Vec<Option<String>> = with_noise
        .iter()
        .map(|adapter| adapter.ipv4.clone())
        .collect();
    assert_eq!(base_ips, noise_ips, "噪音行不得改变适配器/IP 解析");
}
