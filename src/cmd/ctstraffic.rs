//! Microsoft ctsTraffic 命令构造、文本解析和受控执行。
//!
//! ctsTraffic 仅支持 Windows 10+。这里把用户熟悉的“方向/流数/window/时长”
//! 映射到官方参数，但不伪装成 iperf3：TCP window/UDP window 实际是 Winsock
//! SO_SNDBUF/SO_RCVBUF，UDP 也固定由 server 发送、client 接收。

use crate::cmd::iperf::IperfClientJobMgr;
use crate::protocol::{
    CtsTrafficProtocol, CtsTrafficReq, CtsTrafficRole, CtsTrafficStartReq, IperfClientOut,
    IperfEventKind, IperfFlowEvent,
};
use crate::util::run_streaming_controlled;
use regex::Regex;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TCP_INFINITE_TRANSFER: &str = "0xffffffffffffffff";
const PROCESS_GRACE_SECS: u64 = 30;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CtsTrafficParsed {
    pub send_mbps: Option<f64>,
    pub recv_mbps: Option<f64>,
    pub successful_connections: Option<u64>,
    pub network_errors: Option<u64>,
    pub protocol_errors: Option<u64>,
    pub total_bytes_sent: Option<u64>,
    pub total_bytes_recv: Option<u64>,
    pub total_time_ms: Option<u64>,
    pub udp_successful_frames: Option<u64>,
    pub udp_dropped_frames: Option<u64>,
    pub udp_dropped_pct: Option<f64>,
    pub udp_duplicate_frames: Option<u64>,
    pub udp_error_frames: Option<u64>,
    pub max_active_streams: usize,
    pub status_network_errors: u64,
    pub status_protocol_errors: u64,
    pub time_limit_reached: bool,
}

impl CtsTrafficParsed {
    pub fn best_rate(&self, protocol: CtsTrafficProtocol) -> Option<f64> {
        match protocol {
            CtsTrafficProtocol::Tcp => self.send_mbps.or(self.recv_mbps),
            CtsTrafficProtocol::Udp => self.recv_mbps.or(self.send_mbps),
        }
        .filter(|value| value.is_finite() && *value > 0.0)
    }

    pub fn has_measurement(&self, protocol: CtsTrafficProtocol) -> bool {
        self.best_rate(protocol).is_some()
            || self.total_bytes_sent.unwrap_or(0) > 0
            || self.total_bytes_recv.unwrap_or(0) > 0
            || self.udp_successful_frames.unwrap_or(0) > 0
    }

    pub fn error_count(&self) -> u64 {
        // 状态行与最终摘要通常是同一组累计计数，取两者较大值既不会
        // 漏掉“已有状态测量、但进程异常结束而缺少摘要”的错误，也避免
        // 同一错误在状态行和摘要中各计一次。
        self.network_errors
            .unwrap_or(0)
            .max(self.status_network_errors)
            .saturating_add(
                self.protocol_errors
                    .unwrap_or(0)
                    .max(self.status_protocol_errors),
            )
            .saturating_add(self.udp_duplicate_frames.unwrap_or(0))
            .saturating_add(self.udp_error_frames.unwrap_or(0))
    }
}

pub fn parse_size_bytes(value: &str) -> Result<u32, String> {
    let text = value.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Err("socket buffer 不能为空".into());
    }
    let split = text
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(text.len());
    let number: f64 = text[..split]
        .parse()
        .map_err(|_| format!("无法解析 socket buffer: {value}"))?;
    let suffix = text[split..].trim();
    let multiplier = match suffix {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => return Err(format!("不支持的 socket buffer 单位: {value}")),
    };
    let bytes = number * multiplier;
    if !bytes.is_finite() || bytes < 1.0 || bytes > u32::MAX as f64 {
        return Err(format!("socket buffer 超出 1..={} 字节: {value}", u32::MAX));
    }
    Ok(bytes.round() as u32)
}

fn validate(req: &CtsTrafficReq) -> Result<(), String> {
    if req.bind_ip.trim().is_empty() {
        return Err("ctsTraffic bind/listen IP 不能为空".into());
    }
    if req.role == CtsTrafficRole::Client && req.target_ip.trim().is_empty() {
        return Err("ctsTraffic client target IP 不能为空".into());
    }
    if req.port == 0 {
        return Err("ctsTraffic port 不能为 0".into());
    }
    if req.duration_secs == 0 {
        return Err(
            "自动化 ctsTraffic duration 必须大于 0；无限测试请使用原生命令并手动停止".into(),
        );
    }
    if !(1..=32).contains(&req.streams) {
        return Err("ctsTraffic streams 必须在 1..=32".into());
    }
    if req.window_bytes == Some(0) {
        return Err("ctsTraffic socket buffer 不能为 0".into());
    }
    if req.protocol == CtsTrafficProtocol::Udp {
        if req.bits_per_second.unwrap_or(0) == 0 {
            return Err("ctsTraffic UDP bits_per_second 必须大于 0".into());
        }
        if req.frame_rate == 0 {
            return Err("ctsTraffic UDP frame_rate 必须大于 0".into());
        }
        if req.buffer_depth_secs == 0 {
            return Err("ctsTraffic UDP buffer_depth_secs 必须大于 0".into());
        }
        if req
            .datagram_bytes
            .is_some_and(|size| size == 0 || size > 65_507)
        {
            return Err("ctsTraffic UDP datagram_bytes 必须在 1..=65507".into());
        }
    }
    if req.status_update_ms < 100 || req.status_update_ms > 60_000 {
        return Err("ctsTraffic status_update_ms 必须在 100..=60000".into());
    }
    Ok(())
}

pub fn build_args(req: &CtsTrafficReq) -> Result<Vec<String>, String> {
    validate(req)?;
    let mut args = Vec::new();
    match req.role {
        CtsTrafficRole::Server => args.push(format!("-Listen:{}", req.bind_ip)),
        CtsTrafficRole::Client => {
            args.push(format!("-Target:{}", req.target_ip));
            args.push(format!("-Bind:{}", req.bind_ip));
        }
    }
    args.push(format!("-Port:{}", req.port));
    args.push(format!(
        "-Protocol:{}",
        match req.protocol {
            CtsTrafficProtocol::Tcp => "TCP",
            CtsTrafficProtocol::Udp => "UDP",
        }
    ));
    args.push("-ConsoleVerbosity:1".into());
    args.push(format!("-StatusUpdate:{}", req.status_update_ms));

    if let Some(window) = req.window_bytes {
        // 当前所有自动任务都是单向数据流。TCP: client 发/server 收；
        // UDP: server 发/client 收。只设置真正承载数据的一侧 socket buffer。
        let option = match (req.protocol, req.role) {
            (CtsTrafficProtocol::Tcp, CtsTrafficRole::Client)
            | (CtsTrafficProtocol::Udp, CtsTrafficRole::Server) => "-SendBufValue",
            (CtsTrafficProtocol::Tcp, CtsTrafficRole::Server)
            | (CtsTrafficProtocol::Udp, CtsTrafficRole::Client) => "-RecvBufValue",
        };
        args.push(format!("{option}:{window}"));
    }

    match req.protocol {
        CtsTrafficProtocol::Tcp => {
            args.push("-Pattern:Push".into());
            // 固定连接贯穿整个测量窗口，避免默认每传 1GB 就重建连接。
            args.push("-Verify:connection".into());
            args.push(format!("-Transfer:{TCP_INFINITE_TRANSFER}"));
            if req.role == CtsTrafficRole::Client {
                args.push(format!("-Connections:{}", req.streams));
                // Iterations=0 是官方“无限维持连接”语义；TimeLimit 是有界自动化
                // 的硬截止，输出中的 time-limit 提示不单独视为网络错误。
                args.push("-Iterations:0".into());
                let millis = req
                    .duration_secs
                    .checked_mul(1_000)
                    .ok_or_else(|| "ctsTraffic TCP duration 毫秒换算溢出".to_string())?;
                if millis > u32::MAX as u64 {
                    return Err("ctsTraffic TCP duration 超出 TimeLimit 上限".into());
                }
                args.push(format!("-TimeLimit:{millis}"));
            }
        }
        CtsTrafficProtocol::Udp => {
            args.push(format!(
                "-BitsPerSecond:{}",
                req.bits_per_second.unwrap_or_default()
            ));
            args.push(format!("-FrameRate:{}", req.frame_rate));
            args.push(format!("-StreamLength:{}", req.duration_secs));
            args.push(format!("-BufferDepth:{}", req.buffer_depth_secs));
            if let Some(size) = req.datagram_bytes {
                args.push(format!("-DatagramByteSize:{size}"));
            }
            if req.role == CtsTrafficRole::Client {
                args.push(format!("-Connections:{}", req.streams));
                args.push("-Iterations:1".into());
            }
        }
    }
    Ok(args)
}

fn quote_arg(arg: &str) -> String {
    if arg.chars().any(char::is_whitespace) {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

pub fn command_string(bin: &str, args: &[String]) -> String {
    std::iter::once(quote_arg(bin))
        .chain(args.iter().map(|arg| quote_arg(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn capture_u64(regex: &Regex, text: &str) -> Option<u64> {
    regex
        .captures_iter(text)
        .filter_map(|capture| capture.get(1))
        .filter_map(|value| value.as_str().replace(',', "").parse().ok())
        // client/server 输出合并解析时，同一摘要可能各出现一次。取最大值可
        // 捕获任一端的错误或字节数，同时避免把同一连接的双端计数相加。
        .max()
}

fn summary_regex(pattern: &'static str) -> &'static Regex {
    static SUCCESS: OnceLock<Regex> = OnceLock::new();
    static NET: OnceLock<Regex> = OnceLock::new();
    static PROTO: OnceLock<Regex> = OnceLock::new();
    static SENT: OnceLock<Regex> = OnceLock::new();
    static RECV: OnceLock<Regex> = OnceLock::new();
    static TIME: OnceLock<Regex> = OnceLock::new();
    static UDP_OK: OnceLock<Regex> = OnceLock::new();
    static UDP_DROP: OnceLock<Regex> = OnceLock::new();
    static UDP_DUP: OnceLock<Regex> = OnceLock::new();
    static UDP_ERR: OnceLock<Regex> = OnceLock::new();
    let slot = match pattern {
        "success" => &SUCCESS,
        "net" => &NET,
        "proto" => &PROTO,
        "sent" => &SENT,
        "recv" => &RECV,
        "time" => &TIME,
        "udp_ok" => &UDP_OK,
        "udp_drop" => &UDP_DROP,
        "udp_dup" => &UDP_DUP,
        "udp_err" => &UDP_ERR,
        _ => unreachable!(),
    };
    slot.get_or_init(|| {
        let source = match pattern {
            "success" => r"(?i)SuccessfulConnections\s*\[\s*([\d,]+)\s*\]",
            "net" => r"(?i)NetworkErrors\s*\[\s*([\d,]+)\s*\]",
            "proto" => r"(?i)ProtocolErrors\s*\[\s*([\d,]+)\s*\]",
            "sent" => r"(?i)Total\s+Bytes\s+Sent\s*:\s*([\d,]+)",
            "recv" => r"(?i)Total\s+Bytes\s+Recv\s*:\s*([\d,]+)",
            "time" => r"(?i)Total\s+Time\s*:\s*([\d,]+)\s*ms",
            "udp_ok" => r"(?i)Total\s+Successful\s+Frames\s*:\s*([\d,]+)",
            "udp_drop" => r"(?i)Total\s+Dropped\s+Frames\s*:\s*([\d,]+)\s*\(([\d.,]+)\s*%?\)",
            "udp_dup" => r"(?i)Total\s+Duplicate\s+Frames\s*:\s*([\d,]+)",
            "udp_err" => r"(?i)Total\s+Error\s+Frames\s*:\s*([\d,]+)",
            _ => unreachable!(),
        };
        Regex::new(source).expect("ctsTraffic summary regex")
    })
}

fn parse_status_number(token: &str) -> Option<f64> {
    static NUMBER: OnceLock<Regex> = OnceLock::new();
    let regex = NUMBER.get_or_init(|| {
        Regex::new(r"^\[?(\d+(?:[.,]\d+)?)(?:x\^(\d+))?\]?$")
            .expect("ctsTraffic status number regex")
    });
    let capture = regex.captures(token.trim())?;
    let base: f64 = capture.get(1)?.as_str().replace(',', ".").parse().ok()?;
    let exponent: i32 = capture
        .get(2)
        .map(|value| value.as_str().parse().ok())
        .unwrap_or(Some(0))?;
    Some(base * 10_f64.powi(exponent))
}

fn status_values(line: &str) -> Option<Vec<f64>> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 7 || !tokens[0].contains(['.', ',']) {
        return None;
    }
    tokens
        .iter()
        .take(7)
        .map(|token| parse_status_number(token))
        .collect()
}

pub fn parse_output(text: &str, protocol: CtsTrafficProtocol) -> CtsTrafficParsed {
    let mut parsed = CtsTrafficParsed {
        successful_connections: capture_u64(summary_regex("success"), text),
        network_errors: capture_u64(summary_regex("net"), text),
        protocol_errors: capture_u64(summary_regex("proto"), text),
        total_bytes_sent: capture_u64(summary_regex("sent"), text),
        total_bytes_recv: capture_u64(summary_regex("recv"), text),
        total_time_ms: capture_u64(summary_regex("time"), text),
        udp_successful_frames: capture_u64(summary_regex("udp_ok"), text),
        udp_dropped_frames: capture_u64(summary_regex("udp_drop"), text),
        udp_duplicate_frames: capture_u64(summary_regex("udp_dup"), text),
        udp_error_frames: capture_u64(summary_regex("udp_err"), text),
        ..Default::default()
    };
    parsed.udp_dropped_pct = summary_regex("udp_drop")
        .captures_iter(text)
        .filter_map(|capture| capture.get(2))
        .filter_map(|value| value.as_str().replace(',', ".").parse::<f64>().ok())
        .max_by(f64::total_cmp);

    for line in text.lines() {
        let Some(values) = status_values(line) else {
            continue;
        };
        match protocol {
            CtsTrafficProtocol::Tcp if values.len() >= 3 => {
                let send = values[1] * 8.0 / 1_000_000.0;
                let recv = values[2] * 8.0 / 1_000_000.0;
                parsed.send_mbps = Some(parsed.send_mbps.unwrap_or(0.0).max(send));
                parsed.recv_mbps = Some(parsed.recv_mbps.unwrap_or(0.0).max(recv));
                if values.len() >= 7 {
                    parsed.max_active_streams = parsed.max_active_streams.max(values[3] as usize);
                    parsed.status_network_errors =
                        parsed.status_network_errors.max(values[5] as u64);
                    parsed.status_protocol_errors =
                        parsed.status_protocol_errors.max(values[6] as u64);
                }
            }
            CtsTrafficProtocol::Udp if values.len() >= 2 => {
                let recv = values[1] / 1_000_000.0;
                parsed.recv_mbps = Some(parsed.recv_mbps.unwrap_or(0.0).max(recv));
                if values.len() >= 7 {
                    parsed.max_active_streams = parsed.max_active_streams.max(values[2] as usize);
                    parsed.status_protocol_errors =
                        parsed.status_protocol_errors.max(values[6] as u64);
                }
            }
            _ => {}
        }
    }

    if let Some(time_ms) = parsed.total_time_ms.filter(|value| *value > 0) {
        let seconds = time_ms as f64 / 1_000.0;
        if parsed.send_mbps.unwrap_or(0.0) <= 0.0 {
            parsed.send_mbps = parsed
                .total_bytes_sent
                .map(|bytes| bytes as f64 * 8.0 / seconds / 1_000_000.0);
        }
        if parsed.recv_mbps.unwrap_or(0.0) <= 0.0 {
            parsed.recv_mbps = parsed
                .total_bytes_recv
                .map(|bytes| bytes as f64 * 8.0 / seconds / 1_000_000.0);
        }
    }
    let lower = text.to_ascii_lowercase();
    parsed.time_limit_reached = lower.contains("time-limit of")
        || lower.contains("time limit of")
        || lower.contains("timelimit reached");
    parsed
}

fn classify_line(line: &str, req: &CtsTrafficReq, elapsed_ms: u64) -> Option<IperfFlowEvent> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("connection established") {
        return Some(IperfFlowEvent {
            kind: IperfEventKind::Connected,
            elapsed_ms,
            line: line.to_string(),
            ..Default::default()
        });
    }
    if (lower.contains("failed")
        || lower.contains("invalid argument")
        || lower.contains("exception"))
        && !lower.contains("time-limit")
    {
        return Some(IperfFlowEvent {
            kind: IperfEventKind::Error,
            elapsed_ms,
            line: line.to_string(),
            ..Default::default()
        });
    }
    let values = status_values(line)?;
    let mbps = match req.protocol {
        CtsTrafficProtocol::Tcp if values.len() >= 3 => values[1] * 8.0 / 1_000_000.0,
        CtsTrafficProtocol::Udp if values.len() >= 2 => values[1] / 1_000_000.0,
        _ => return None,
    };
    (mbps > 0.0).then(|| IperfFlowEvent {
        kind: IperfEventKind::Traffic,
        elapsed_ms,
        mbps: Some(mbps),
        line: line.to_string(),
    })
}

pub fn run_controlled<F>(
    bin: &str,
    req: &CtsTrafficReq,
    cancel: Option<&AtomicBool>,
    mut on_event: F,
) -> IperfClientOut
where
    F: FnMut(IperfFlowEvent),
{
    let args = match build_args(req) {
        Ok(args) => args,
        Err(error) => {
            return IperfClientOut {
                output: error,
                ..Default::default()
            }
        }
    };
    let cmd = command_string(bin, &args);
    let started = Instant::now();
    on_event(IperfFlowEvent {
        kind: IperfEventKind::Started,
        line: cmd.clone(),
        ..Default::default()
    });
    let timeout_secs = if req.role == CtsTrafficRole::Server {
        // server 正常由 owner/显式 stop 回收；lease 外再留少量进程回收余量。
        req.duration_secs
            .saturating_add(PROCESS_GRACE_SECS)
            .saturating_add(300)
    } else {
        req.duration_secs.saturating_add(PROCESS_GRACE_SECS)
    };
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_streaming_controlled(
        bin,
        &arg_refs,
        Duration::from_secs(timeout_secs),
        cancel,
        |line| {
            if let Some(event) = classify_line(line, req, started.elapsed().as_millis() as u64) {
                on_event(event);
            }
        },
    );
    let output = out.merged();
    let parsed = parse_output(&output, req.protocol);
    let expected_time_limit = req.role == CtsTrafficRole::Client
        && req.protocol == CtsTrafficProtocol::Tcp
        && parsed.time_limit_reached
        && !out.cancelled
        && !out.timed_out;
    if !out.ok && !expected_time_limit && !out.cancelled && !out.timed_out {
        on_event(IperfFlowEvent {
            kind: IperfEventKind::Error,
            elapsed_ms: started.elapsed().as_millis() as u64,
            line: output
                .lines()
                .last()
                .unwrap_or("ctsTraffic 执行失败")
                .to_string(),
            ..Default::default()
        });
    }
    on_event(IperfFlowEvent {
        kind: IperfEventKind::Ended,
        elapsed_ms: started.elapsed().as_millis() as u64,
        mbps: parsed.best_rate(req.protocol),
        line: if out.ok {
            "ctsTraffic completed".into()
        } else if expected_time_limit {
            "ctsTraffic completed at configured TimeLimit".into()
        } else if out.cancelled {
            "ctsTraffic cancelled".into()
        } else if out.timed_out {
            "ctsTraffic timed out".into()
        } else {
            "ctsTraffic failed".into()
        },
    });
    IperfClientOut {
        ok: out.ok || expected_time_limit,
        timed_out: out.timed_out,
        cancelled: out.cancelled,
        process_started: Some(out.process_started()),
        cleanup_confirmed: Some(out.cleanup_confirmed()),
        cmd,
        output,
    }
}

pub fn start_managed_job(
    manager: &IperfClientJobMgr,
    bin: String,
    start: CtsTrafficStartReq,
) -> Result<String, String> {
    let fingerprint = serde_json::to_string(&start.request)
        .map_err(|error| format!("序列化 ctsTraffic 请求失败: {error}"))?;
    let request = start.request;
    manager.start_external_request(
        start.request_id,
        start.owner_id,
        start.lease_secs,
        format!("ctstraffic|{fingerprint}"),
        move |cancel: Arc<AtomicBool>, events: Arc<Mutex<Vec<IperfFlowEvent>>>| {
            let sink = Arc::clone(&events);
            run_controlled(&bin, &request, Some(cancel.as_ref()), move |event| {
                if let Ok(mut guard) = sink.lock() {
                    guard.push(event);
                }
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(role: CtsTrafficRole, protocol: CtsTrafficProtocol) -> CtsTrafficReq {
        CtsTrafficReq {
            role,
            protocol,
            bind_ip: "192.0.2.10".into(),
            target_ip: "192.0.2.20".into(),
            port: 56_000,
            duration_secs: 30,
            streams: 4,
            window_bytes: Some(1_048_576),
            bits_per_second: Some(100_000_000),
            datagram_bytes: Some(1_400),
            frame_rate: 100,
            buffer_depth_secs: 1,
            status_update_ms: 1_000,
        }
    }

    #[test]
    fn parses_friendly_socket_buffer_sizes() {
        assert_eq!(parse_size_bytes("64k").unwrap(), 65_536);
        assert_eq!(parse_size_bytes("1m").unwrap(), 1_048_576);
        assert_eq!(parse_size_bytes("1.5m").unwrap(), 1_572_864);
        assert!(parse_size_bytes("0").is_err());
        assert!(parse_size_bytes("5g").is_err());
    }

    #[test]
    fn tcp_push_maps_streams_time_and_directional_socket_buffers() {
        let client = build_args(&base(CtsTrafficRole::Client, CtsTrafficProtocol::Tcp)).unwrap();
        assert!(client.contains(&"-Target:192.0.2.20".into()));
        assert!(client.contains(&"-Bind:192.0.2.10".into()));
        assert!(client.contains(&"-Connections:4".into()));
        assert!(client.contains(&"-Iterations:0".into()));
        assert!(client.contains(&"-TimeLimit:30000".into()));
        assert!(client.contains(&"-SendBufValue:1048576".into()));
        assert!(!client.iter().any(|arg| arg.starts_with("-RecvBufValue:")));

        let server = build_args(&base(CtsTrafficRole::Server, CtsTrafficProtocol::Tcp)).unwrap();
        assert!(server.contains(&"-Listen:192.0.2.10".into()));
        assert!(server.contains(&"-RecvBufValue:1048576".into()));
        assert!(!server.iter().any(|arg| arg.starts_with("-TimeLimit:")));
    }

    #[test]
    fn udp_maps_streams_rate_length_and_reverses_buffer_roles() {
        let client = build_args(&base(CtsTrafficRole::Client, CtsTrafficProtocol::Udp)).unwrap();
        for expected in [
            "-Protocol:UDP",
            "-BitsPerSecond:100000000",
            "-FrameRate:100",
            "-StreamLength:30",
            "-DatagramByteSize:1400",
            "-Connections:4",
            "-Iterations:1",
            "-RecvBufValue:1048576",
        ] {
            assert!(client.contains(&expected.to_string()), "missing {expected}");
        }
        let server = build_args(&base(CtsTrafficRole::Server, CtsTrafficProtocol::Udp)).unwrap();
        assert!(server.contains(&"-SendBufValue:1048576".into()));
    }

    #[test]
    fn parses_tcp_and_udp_summaries() {
        let tcp = parse_output(
            r#"
[5.002] 2635357062 124 8 8 0 0
SuccessfulConnections [59] NetworkErrors [0] ProtocolErrors [0]
Total Bytes Recv : 5194
Total Bytes Sent : 67358818304
Total Time : 26357 ms.
"#,
            CtsTrafficProtocol::Tcp,
        );
        assert!(tcp.send_mbps.unwrap() > 20_000.0);
        assert_eq!(tcp.successful_connections, Some(59));
        assert_eq!(tcp.error_count(), 0);

        let udp = parse_output(
            r#"
[10.000] 24999840 1 300 0 0 0
SuccessfulConnections [1] NetworkErrors [0] ProtocolErrors [0]
Total Bytes Recv : 187498800
Total Successful Frames : 3599 (99.972222)
Total Dropped Frames : 1 (0.027778)
Total Duplicate Frames : 0 (0.000000)
Total Error Frames : 0 (0.000000)
Total Time : 61273 ms.
"#,
            CtsTrafficProtocol::Udp,
        );
        assert!((udp.recv_mbps.unwrap() - 24.99984).abs() < 0.001);
        assert_eq!(udp.udp_dropped_frames, Some(1));
        assert_eq!(udp.udp_dropped_pct, Some(0.027778));
    }

    #[test]
    fn parses_console_exponent_notation() {
        let parsed = parse_output(" 5.000 2.6x^9 1.2x^6 8 1 0 0\n", CtsTrafficProtocol::Tcp);
        assert_eq!(parsed.send_mbps, Some(20_800.0));
        assert_eq!(parsed.recv_mbps, Some(9.6));
    }

    #[test]
    fn merged_client_server_output_keeps_largest_counters_and_percent_suffix() {
        let parsed = parse_output(
            r#"
SuccessfulConnections [4] NetworkErrors [0] ProtocolErrors [0]
Total Bytes Sent : 1,000
Total Dropped Frames : 1 (0.25%)
Total Duplicate Frames : 0 (0.0%)
Total Error Frames : 0 (0.0%)
SuccessfulConnections [4] NetworkErrors [2] ProtocolErrors [1]
Total Bytes Sent : 2,000
Total Dropped Frames : 2 (0.50%)
Total Duplicate Frames : 3 (0.75%)
Total Error Frames : 4 (1.0%)
"#,
            CtsTrafficProtocol::Udp,
        );
        assert_eq!(parsed.total_bytes_sent, Some(2_000));
        assert_eq!(parsed.network_errors, Some(2));
        assert_eq!(parsed.protocol_errors, Some(1));
        assert_eq!(parsed.udp_dropped_frames, Some(2));
        assert_eq!(parsed.udp_dropped_pct, Some(0.5));
        assert_eq!(parsed.error_count(), 10);
    }

    #[test]
    fn status_errors_survive_a_missing_or_stale_final_summary_without_double_counting() {
        let parsed = parse_output(
            r#"
[10.000] 24999840 1 300 0 0 3
SuccessfulConnections [1] NetworkErrors [0] ProtocolErrors [2]
Total Successful Frames : 3599 (99.972222)
"#,
            CtsTrafficProtocol::Udp,
        );
        assert_eq!(parsed.status_protocol_errors, 3);
        assert_eq!(parsed.protocol_errors, Some(2));
        assert_eq!(parsed.error_count(), 3);
    }
}
