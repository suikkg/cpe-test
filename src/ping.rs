//! ping 命令构造 + 执行 + 输出解析（Windows 中/英文、macOS/BSD 三种格式）

use crate::protocol::{PingOut, PingReq};
use crate::util::{run_cmd, CmdOut};
use regex::Regex;
use std::time::Duration;

pub const MAX_PAYLOAD: u32 = 65500;
const EXEC_ERROR_PREFIX: &str = "[CPE_PING_EXEC_ERROR:";

/// PingOut 为兼容旧 agent 不增加协议字段；执行层错误以稳定标记写入 raw，
/// 主控收到本地或远端结果后都可用 execution_error() 还原分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingExecErrorKind {
    Spawn,
    Timeout,
    Execution,
}

impl PingExecErrorKind {
    fn code(self) -> &'static str {
        match self {
            Self::Spawn => "SPAWN",
            Self::Timeout => "TIMEOUT",
            Self::Execution => "EXECUTION",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "SPAWN" => Some(Self::Spawn),
            "TIMEOUT" => Some(Self::Timeout),
            "EXECUTION" => Some(Self::Execution),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PingExecFailure {
    pub kind: PingExecErrorKind,
    pub detail: String,
}

/// 构造 ping 命令（跨平台）
pub fn build(req: &PingReq) -> (String, Vec<String>) {
    let count = req.count.max(1).to_string();
    let payload = req.payload.min(MAX_PAYLOAD).to_string();
    if cfg!(windows) {
        let mut a = vec![
            req.dst.clone(),
            "-S".into(),
            req.src.clone(),
            "-n".into(),
            count,
            "-l".into(),
            payload,
        ];
        a.push(if req.v6 { "-6".into() } else { "-4".into() });
        ("ping".into(), a)
    } else {
        // macOS：v4 用 ping，v6 用 ping6（-c 计数，-s 负载，-S 源绑定）
        let prog = if req.v6 { "ping6" } else { "ping" };
        let a = vec![
            "-c".into(),
            count,
            "-S".into(),
            req.src.clone(),
            "-s".into(),
            payload,
            req.dst.clone(),
        ];
        (prog.into(), a)
    }
}

/// 执行 ping 并解析
pub fn run(req: &PingReq) -> PingOut {
    let (prog, args) = build(req);
    let cmd_str = format!("{} {}", prog, args.join(" "));
    let timeout = Duration::from_secs(req.count.max(1) as u64 * 5 + 30);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let out = run_cmd(&prog, &args_ref, timeout);
    finish_run(cmd_str, req.count, timeout, out)
}

fn finish_run(cmd: String, count: u32, timeout: Duration, out: CmdOut) -> PingOut {
    let raw = out.merged();
    let mut r = parse(&raw, count);
    let exec_error = classify_execution_error(&out, &raw, timeout);
    r.cmd = cmd;
    r.raw = match exec_error {
        Some(error) => append_execution_error(raw, &error),
        None => raw,
    };
    r
}

fn classify_execution_error(out: &CmdOut, raw: &str, timeout: Duration) -> Option<PingExecFailure> {
    if out.timed_out {
        return Some(PingExecFailure {
            kind: PingExecErrorKind::Timeout,
            detail: format!("ping 命令超过 {} 秒未结束，已强制终止", timeout.as_secs()),
        });
    }
    if out.stderr.trim_start().starts_with("启动命令失败:") {
        return Some(PingExecFailure {
            kind: PingExecErrorKind::Spawn,
            detail: "无法启动 ping 子进程".into(),
        });
    }
    if let Some(detail) = explicit_execution_error(raw) {
        return Some(PingExecFailure {
            kind: PingExecErrorKind::Execution,
            detail,
        });
    }
    // ping 在正常的部分丢包/全丢时通常以非零状态退出，因此退出状态本身
    // 不能当作执行故障。只要存在完整统计行，就属于已正常执行的网络结果。
    if !has_packet_summary(raw) {
        return Some(PingExecFailure {
            kind: PingExecErrorKind::Execution,
            detail: if out.ok {
                "ping 命令结束，但没有返回可识别的数据包统计".into()
            } else {
                "ping 命令异常退出，且没有返回可识别的数据包统计".into()
            },
        });
    }
    None
}

fn explicit_execution_error(raw: &str) -> Option<String> {
    let lower = raw.to_lowercase();
    let markers = [
        "ping: transmit failed",
        "ping: 传输失败",
        "ping：传输失败",
        "general failure",
        "常规故障",
        "一般故障",
        "can't assign requested address",
        "cannot assign requested address",
        "can't bind",
        "cannot bind",
        "invalid source address",
        "bad value for option -s",
        "operation not permitted",
        "permission denied",
        "network is unreachable",
        "no route to host",
        "sendto: message too long",
        "message too long",
        "消息太长",
    ];
    markers.iter().find_map(|marker| {
        if !lower.contains(*marker) {
            return None;
        }
        Some(
            raw.lines()
                .find(|line| line.to_lowercase().contains(*marker))
                .or_else(|| raw.lines().find(|line| !line.trim().is_empty()))
                .unwrap_or("ping 命令执行失败")
                .trim()
                .to_string(),
        )
    })
}

fn append_execution_error(mut raw: String, error: &PingExecFailure) -> String {
    if !raw.is_empty() && !raw.ends_with('\n') {
        raw.push('\n');
    }
    raw.push_str(&format!(
        "{EXEC_ERROR_PREFIX}{}] {}",
        error.kind.code(),
        error.detail
    ));
    raw
}

fn execution_failure(out: &PingOut) -> Option<PingExecFailure> {
    if let Some(marked) = out.raw.lines().rev().find_map(|line| {
        let rest = line.strip_prefix(EXEC_ERROR_PREFIX)?;
        let (kind, detail) = rest.split_once("] ")?;
        Some(PingExecFailure {
            kind: PingExecErrorKind::from_code(kind)?,
            detail: detail.to_string(),
        })
    }) {
        return Some(marked);
    }

    // 兼容未写稳定标记的旧 agent。旧版会保留这些原始错误文字，且正常
    // 0 回复仍有数据包统计行，因此不会与网络全丢混淆。
    if out.raw.contains("(ping 命令超时被强制结束)") {
        return Some(PingExecFailure {
            kind: PingExecErrorKind::Timeout,
            detail: "ping 命令超时被强制结束".into(),
        });
    }
    if out.raw.contains("启动命令失败:") {
        return Some(PingExecFailure {
            kind: PingExecErrorKind::Spawn,
            detail: "无法启动 ping 子进程".into(),
        });
    }
    if let Some(detail) = explicit_execution_error(&out.raw) {
        return Some(PingExecFailure {
            kind: PingExecErrorKind::Execution,
            detail,
        });
    }
    if !has_packet_summary(&out.raw) {
        return Some(PingExecFailure {
            kind: PingExecErrorKind::Execution,
            detail: "ping 输出缺少可识别的数据包统计".into(),
        });
    }
    None
}

/// 从兼容版 PingOut.raw 读取稳定的执行错误类型。None 表示命令正常完成；
/// 此时 received=0/loss_pct=100 是真实的“发出但没有 Echo Reply”。
pub fn execution_error_kind(out: &PingOut) -> Option<PingExecErrorKind> {
    execution_failure(out).map(|failure| failure.kind)
}

/// 返回执行错误说明，供现有调用点直接写入报告；稳定分类请使用
/// execution_error_kind()，避免依赖本地化文字。
pub fn execution_error(out: &PingOut) -> Option<String> {
    execution_failure(out).map(|failure| failure.detail)
}

fn packet_summary(text: &str, fallback_count: u32) -> Option<(u32, u32)> {
    let cn = Regex::new(
        r"(?s)已发送\s*=\s*(\d+).*?已接收\s*=\s*(\d+).*?丢失\s*=\s*(\d+).*?\((\d+(?:\.\d+)?)%\s*丢失\)",
    )
    .expect("regex");
    let en = Regex::new(
        r"(?si)Sent\s*=\s*(\d+).*?Received\s*=\s*(\d+).*?Lost\s*=\s*(\d+).*?\((\d+(?:\.\d+)?)%\s*loss\)",
    )
    .expect("regex");
    let bsd = Regex::new(
        r"(?i)(\d+)\s+packets transmitted,\s*(\d+)\s+(?:packets\s+)?received,\s*(\d+(?:\.\d+)?)%\s+packet loss",
    )
    .expect("regex");

    if let Some(c) = cn.captures(text).or_else(|| en.captures(text)) {
        Some((
            c[1].parse().unwrap_or(fallback_count),
            c[2].parse().unwrap_or(0),
        ))
    } else {
        bsd.captures(text).map(|c| {
            (
                c[1].parse().unwrap_or(fallback_count),
                c[2].parse().unwrap_or(0),
            )
        })
    }
}

fn has_packet_summary(text: &str) -> bool {
    packet_summary(text, 0).is_some()
}

/// 只统计确实带 RTT 的 Echo Reply 行：
/// - Windows 中文/英文的不可达、TTL 超时等行没有 RTT，不计入；
/// - BSD/macOS 的 Redirect 可能包含 "bytes from"，但没有 icmp_seq + RTT，
///   不计入，也不会再从统计行 received 中误减；
/// - Linux/BSD 的 Destination Unreachable 同样没有 RTT，不计入。
fn count_echo_replies(text: &str) -> u32 {
    let win_reply = Regex::new(r"(?i)(?:的回复[:：]|Reply from\b)").expect("regex");
    let bsd_reply = Regex::new(r"(?i)\bbytes from\b.*\bicmp_seq[=:]\s*\d+").expect("regex");
    let has_rtt = Regex::new(r"(?i)(?:时间|time)\s*[<=]\s*\d+(?:[\.,]\d+)?").expect("regex");

    text.lines()
        .filter(|line| {
            has_rtt.is_match(line) && (win_reply.is_match(line) || bsd_reply.is_match(line))
        })
        .count() as u32
}

/// 解析 ping 输出。全都匹配不上 => 按全丢处理
pub fn parse(text: &str, count: u32) -> PingOut {
    let echo_replies = count_echo_replies(text);
    let summary = packet_summary(text, count);
    let (sent, summary_received) = summary.unwrap_or((count, echo_replies));
    // Windows 会把不可达等 ICMP 错误应答算进 received；BSD 输出中还可能
    // 混有 Redirect。正式执行会完整采集逐包行，所以接收数必须以带 RTT 的
    // Echo Reply 为准；统计行只用于约束上限和取得实际发送数。
    let received = echo_replies.min(summary_received).min(sent);
    let lost = sent.saturating_sub(received);
    let loss_pct = if sent > 0 {
        lost as f64 / sent as f64 * 100.0
    } else {
        100.0
    };

    // RTT
    let rtt_cn =
        Regex::new(r"(?s)最短\s*=\s*(<?\d+)ms.*?最长\s*=\s*(<?\d+)ms.*?平均\s*=\s*(<?\d+)ms")
            .expect("regex");
    let rtt_en = Regex::new(
        r"(?si)Minimum\s*=\s*(<?\d+)ms.*?Maximum\s*=\s*(<?\d+)ms.*?Average\s*=\s*(<?\d+)ms",
    )
    .expect("regex");
    let rtt_bsd =
        Regex::new(r"(?:round-trip|rtt)[^=]*=\s*(\d+(?:\.\d+)?)/(\d+(?:\.\d+)?)/(\d+(?:\.\d+)?)")
            .expect("regex");

    let (rtt_min, rtt_max, rtt_avg) = if let Some(c) = rtt_cn.captures(text) {
        (parse_ms(&c[1]), parse_ms(&c[2]), parse_ms(&c[3]))
    } else if let Some(c) = rtt_en.captures(text) {
        (parse_ms(&c[1]), parse_ms(&c[2]), parse_ms(&c[3]))
    } else if let Some(c) = rtt_bsd.captures(text) {
        // BSD 顺序是 min/avg/max
        (c[1].parse().ok(), c[3].parse().ok(), c[2].parse().ok())
    } else {
        (None, None, None)
    };

    let ok = received > 0 && loss_pct < 100.0;
    let (rtt_min, rtt_max, rtt_avg) = if ok {
        (rtt_min, rtt_max, rtt_avg)
    } else {
        (None, None, None)
    };
    PingOut {
        ok,
        sent,
        received,
        lost,
        loss_pct,
        rtt_min,
        rtt_avg,
        rtt_max,
        cmd: String::new(),
        raw: String::new(),
    }
}

/// "<1" -> 0
fn parse_ms(s: &str) -> Option<f64> {
    let t = s.trim();
    if let Some(stripped) = t.strip_prefix('<') {
        let _ = stripped;
        return Some(0.0);
    }
    t.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CN: &str = r#"
正在 Ping 192.168.1.3 具有 32 字节的数据:
来自 192.168.1.3 的回复: 字节=32 时间<1ms TTL=64
来自 192.168.1.3 的回复: 字节=32 时间=1ms TTL=64
来自 192.168.1.3 的回复: 字节=32 时间<1ms TTL=64
来自 192.168.1.3 的回复: 字节=32 时间<1ms TTL=64

192.168.1.3 的 Ping 统计信息:
    数据包: 已发送 = 4，已接收 = 4，丢失 = 0 (0% 丢失)，
往返行程的估计时间(以毫秒为单位):
    最短 = 0ms，最长 = 1ms，平均 = 0ms
"#;

    #[test]
    fn test_cn() {
        let r = parse(CN, 4);
        assert!(r.ok);
        assert_eq!((r.sent, r.received, r.lost), (4, 4, 0));
        assert_eq!(r.loss_pct, 0.0);
        assert_eq!(r.rtt_avg, Some(0.0));
        assert_eq!(r.rtt_max, Some(1.0));
    }

    const EN: &str = r#"
Pinging 192.168.1.3 with 32 bytes of data:
Reply from 192.168.1.3: bytes=32 time<1ms TTL=64
Reply from 192.168.1.3: bytes=32 time=1ms TTL=64
Reply from 192.168.1.3: bytes=32 time=2ms TTL=64

Ping statistics for 192.168.1.3:
    Packets: Sent = 4, Received = 3, Lost = 1 (25% loss),
Approximate round trip times in milli-seconds:
    Minimum = 0ms, Maximum = 2ms, Average = 1ms
"#;

    #[test]
    fn test_en() {
        let r = parse(EN, 4);
        assert!(r.ok);
        assert_eq!((r.sent, r.received, r.lost), (4, 3, 1));
        assert_eq!(r.loss_pct, 25.0);
        assert_eq!(r.rtt_avg, Some(1.0));
    }

    const MAC: &str = r#"
PING 192.168.8.1 (192.168.8.1) from 192.168.8.100: 56 data bytes
64 bytes from 192.168.8.1: icmp_seq=0 ttl=64 time=1.312 ms
64 bytes from 192.168.8.1: icmp_seq=1 ttl=64 time=1.605 ms
64 bytes from 192.168.8.1: icmp_seq=2 ttl=64 time=1.998 ms

--- 192.168.8.1 ping statistics ---
3 packets transmitted, 3 packets received, 0.0% packet loss
round-trip min/avg/max/stddev = 1.312/1.605/1.998/0.281 ms
"#;

    #[test]
    fn test_mac() {
        let r = parse(MAC, 3);
        assert!(r.ok);
        assert_eq!((r.sent, r.received, r.lost), (3, 3, 0));
        assert_eq!(r.rtt_min, Some(1.312));
        assert_eq!(r.rtt_avg, Some(1.605));
        assert_eq!(r.rtt_max, Some(1.998));
    }

    #[test]
    fn test_no_match_means_all_lost() {
        let r = parse("Request timed out.", 10);
        assert!(!r.ok);
        assert_eq!(r.sent, 10);
        assert_eq!(r.received, 0);
        assert_eq!(r.loss_pct, 100.0);
    }

    #[test]
    fn test_all_lost_cn() {
        let t = r#"
正在 Ping 192.168.1.99 具有 32 字节的数据:
请求超时。
请求超时。
请求超时。
请求超时。
数据包: 已发送 = 4，已接收 = 0，丢失 = 4 (100% 丢失)，
"#;
        let r = parse(t, 4);
        assert!(!r.ok);
        assert_eq!(r.loss_pct, 100.0);
    }

    #[test]
    fn test_summary_received_without_echo_evidence_is_not_success() {
        let text = r#"
Ping statistics for 192.168.1.99:
    Packets: Sent = 4, Received = 4, Lost = 0 (0% loss),
"#;
        let r = parse(text, 4);
        assert!(!r.ok);
        assert_eq!((r.sent, r.received, r.lost), (4, 0, 4));
        assert_eq!(r.loss_pct, 100.0);
    }

    // --- 回归测试：目标不可达等 ICMP 错误应答被系统误计为"已接收" ---

    const CN_UNREACHABLE: &str = r#"
正在 Ping 192.168.1.99 具有 32 字节的数据:
来自 192.168.1.5 的回复: 无法访问目标主机。
来自 192.168.1.5 的回复: 无法访问目标主机。
来自 192.168.1.5 的回复: 无法访问目标主机。
来自 192.168.1.5 的回复: 无法访问目标主机。

192.168.1.99 的 Ping 统计信息:
    数据包: 已发送 = 4，已接收 = 4，丢失 = 0 (0% 丢失)，
"#;

    #[test]
    fn test_cn_unreachable_is_not_success() {
        let r = parse(CN_UNREACHABLE, 4);
        assert!(!r.ok, "目标不可达不应判定为 ping 成功");
        assert_eq!(r.sent, 4);
        assert_eq!(r.received, 0);
        assert_eq!(r.lost, 4);
        assert_eq!(r.loss_pct, 100.0);
    }

    const EN_UNREACHABLE: &str = r#"
Pinging 192.168.1.99 with 32 bytes of data:
Reply from 192.168.1.5: Destination host unreachable.
Reply from 192.168.1.5: Destination host unreachable.
Reply from 192.168.1.5: Destination host unreachable.
Reply from 192.168.1.5: Destination host unreachable.

Ping statistics for 192.168.1.99:
    Packets: Sent = 4, Received = 4, Lost = 0 (0% loss),
"#;

    #[test]
    fn test_en_unreachable_is_not_success() {
        let r = parse(EN_UNREACHABLE, 4);
        assert!(!r.ok);
        assert_eq!(r.received, 0);
        assert_eq!(r.lost, 4);
        assert_eq!(r.loss_pct, 100.0);
    }

    const CN_PARTIAL_UNREACHABLE: &str = r#"
正在 Ping 192.168.1.99 具有 32 字节的数据:
来自 192.168.1.99 的回复: 字节=32 时间=1ms TTL=64
来自 192.168.1.5 的回复: 无法访问目标主机。
来自 192.168.1.99 的回复: 字节=32 时间=1ms TTL=64
来自 192.168.1.5 的回复: 无法访问目标主机。

192.168.1.99 的 Ping 统计信息:
    数据包: 已发送 = 4，已接收 = 4，丢失 = 0 (0% 丢失)，
"#;

    #[test]
    fn test_cn_partial_unreachable_counts_real_success_only() {
        let r = parse(CN_PARTIAL_UNREACHABLE, 4);
        assert!(r.ok);
        assert_eq!(r.received, 2);
        assert_eq!(r.lost, 2);
        assert_eq!(r.loss_pct, 50.0);
    }

    const EN_PARTIAL_UNREACHABLE: &str = r#"
Pinging 192.168.1.99 with 32 bytes of data:
Reply from 192.168.1.99: bytes=32 time=1ms TTL=64
Reply from 192.168.1.5: Destination host unreachable.
Reply from 192.168.1.99: bytes=32 time=2ms TTL=64
Reply from 192.168.1.5: Destination host unreachable.

Ping statistics for 192.168.1.99:
    Packets: Sent = 4, Received = 4, Lost = 0 (0% loss),
Approximate round trip times in milli-seconds:
    Minimum = 1ms, Maximum = 2ms, Average = 1ms
"#;

    #[test]
    fn test_en_partial_unreachable_counts_real_success_only() {
        let r = parse(EN_PARTIAL_UNREACHABLE, 4);
        assert!(r.ok);
        assert_eq!((r.sent, r.received, r.lost), (4, 2, 2));
        assert_eq!(r.loss_pct, 50.0);
        assert_eq!(r.rtt_avg, Some(1.0));
    }

    const MAC_REDIRECT_AND_REPLIES: &str = r#"
PING 192.168.8.1 (192.168.8.1): 56 data bytes
36 bytes from 192.168.8.254: Redirect Host(New addr: 192.168.8.1)
Vr HL TOS  Len   ID Flg  off TTL Pro  cks      Src      Dst
 4  5  00 0054 0000   0 0000  40  01 f70d 192.168.8.100  192.168.8.1
64 bytes from 192.168.8.1: icmp_seq=0 ttl=64 time=1.100 ms
36 bytes from 192.168.8.254: Redirect Host(New addr: 192.168.8.1)
64 bytes from 192.168.8.1: icmp_seq=1 ttl=64 time=1.300 ms

--- 192.168.8.1 ping statistics ---
2 packets transmitted, 2 packets received, 0.0% packet loss
round-trip min/avg/max/stddev = 1.100/1.200/1.300/0.100 ms
"#;

    #[test]
    fn test_macos_redirect_does_not_subtract_real_echo_replies() {
        let r = parse(MAC_REDIRECT_AND_REPLIES, 2);
        assert!(r.ok);
        assert_eq!((r.sent, r.received, r.lost), (2, 2, 0));
        assert_eq!(r.loss_pct, 0.0);
        assert_eq!(r.rtt_avg, Some(1.2));
    }

    const MAC_REDIRECT_WITH_PARTIAL_REPLY: &str = r#"
PING 192.168.8.1 (192.168.8.1): 56 data bytes
36 bytes from 192.168.8.254: Redirect Host(New addr: 192.168.8.1)
64 bytes from 192.168.8.1: icmp_seq=0 ttl=64 time=1.100 ms
36 bytes from 192.168.8.254: Redirect Host(New addr: 192.168.8.1)

--- 192.168.8.1 ping statistics ---
2 packets transmitted, 1 packets received, 50.0% packet loss
round-trip min/avg/max/stddev = 1.100/1.100/1.100/0.000 ms
"#;

    #[test]
    fn test_macos_redirect_with_partial_reply_counts_only_echo_reply() {
        let r = parse(MAC_REDIRECT_WITH_PARTIAL_REPLY, 2);
        assert!(r.ok);
        assert_eq!((r.sent, r.received, r.lost), (2, 1, 1));
        assert_eq!(r.loss_pct, 50.0);
    }

    fn cmd_out(ok: bool, timed_out: bool, stdout: &str, stderr: &str) -> CmdOut {
        CmdOut {
            ok,
            timed_out,
            cancelled: false,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn test_normal_zero_reply_is_not_execution_error() {
        let stdout = r#"
PING 192.0.2.1 (192.0.2.1): 56 data bytes

--- 192.0.2.1 ping statistics ---
3 packets transmitted, 0 packets received, 100.0% packet loss
"#;
        let out = finish_run(
            "ping 192.0.2.1".into(),
            3,
            Duration::from_secs(45),
            cmd_out(false, false, stdout, ""),
        );
        assert!(!out.ok);
        assert_eq!((out.sent, out.received, out.lost), (3, 0, 3));
        assert_eq!(execution_error_kind(&out), None);
        assert_eq!(execution_error(&out), None);
    }

    #[test]
    fn test_spawn_error_has_stable_compatible_marker() {
        let out = finish_run(
            "ping 127.0.0.1".into(),
            3,
            Duration::from_secs(45),
            cmd_out(false, false, "", "启动命令失败: ping (not found)"),
        );
        assert_eq!(execution_error_kind(&out), Some(PingExecErrorKind::Spawn));
        assert_eq!(
            execution_error(&out).as_deref(),
            Some("无法启动 ping 子进程")
        );
        assert!(out.raw.contains("[CPE_PING_EXEC_ERROR:SPAWN]"));
        assert_eq!((out.sent, out.received, out.lost), (3, 0, 3));
    }

    #[test]
    fn test_timeout_has_stable_compatible_marker() {
        let out = finish_run(
            "ping 127.0.0.1".into(),
            3,
            Duration::from_secs(45),
            cmd_out(false, true, "partial output", ""),
        );
        assert_eq!(execution_error_kind(&out), Some(PingExecErrorKind::Timeout));
        assert!(execution_error(&out).unwrap().contains("45 秒"));
        assert!(out.raw.contains("[CPE_PING_EXEC_ERROR:TIMEOUT]"));
    }

    #[test]
    fn test_bind_or_general_failure_is_execution_error_even_with_summary() {
        let windows = r#"
Pinging 192.0.2.1 with 32 bytes of data:
PING: transmit failed. General failure.

Ping statistics for 192.0.2.1:
    Packets: Sent = 1, Received = 0, Lost = 1 (100% loss),
"#;
        let out = finish_run(
            "ping 192.0.2.1".into(),
            1,
            Duration::from_secs(35),
            cmd_out(false, false, windows, ""),
        );
        assert_eq!(
            execution_error_kind(&out),
            Some(PingExecErrorKind::Execution)
        );
        assert!(out.raw.contains("[CPE_PING_EXEC_ERROR:EXECUTION]"));
    }

    #[test]
    fn test_local_payload_too_large_is_execution_error_not_packet_loss() {
        let stdout = r#"
PING 192.168.8.100 (192.168.8.100): 65500 data bytes

--- 192.168.8.100 ping statistics ---
1 packets transmitted, 0 packets received, 100.0% packet loss
"#;
        let out = finish_run(
            "ping -s 65500 192.168.8.100".into(),
            1,
            Duration::from_secs(35),
            cmd_out(false, false, stdout, "ping: sendto: Message too long"),
        );
        assert_eq!(
            execution_error_kind(&out),
            Some(PingExecErrorKind::Execution)
        );
        assert!(execution_error(&out).unwrap().contains("Message too long"));
    }

    #[test]
    fn test_nonzero_exit_with_valid_partial_loss_is_not_execution_error() {
        let stdout = r#"
Pinging 192.0.2.1 with 32 bytes of data:
Reply from 192.0.2.1: bytes=32 time=2ms TTL=64
Request timed out.

Ping statistics for 192.0.2.1:
    Packets: Sent = 2, Received = 1, Lost = 1 (50% loss),
Approximate round trip times in milli-seconds:
    Minimum = 2ms, Maximum = 2ms, Average = 2ms
"#;
        let out = finish_run(
            "ping 192.0.2.1".into(),
            2,
            Duration::from_secs(40),
            cmd_out(false, false, stdout, ""),
        );
        assert!(out.ok);
        assert_eq!((out.sent, out.received, out.lost), (2, 1, 1));
        assert_eq!(execution_error_kind(&out), None);
    }

    #[test]
    fn test_legacy_raw_error_markers_remain_classifiable() {
        let timed_out = PingOut {
            raw: "partial\n(ping 命令超时被强制结束)".into(),
            ..Default::default()
        };
        assert_eq!(
            execution_error_kind(&timed_out),
            Some(PingExecErrorKind::Timeout)
        );

        let spawn = PingOut {
            raw: "启动命令失败: ping (not found)".into(),
            ..Default::default()
        };
        assert_eq!(execution_error_kind(&spawn), Some(PingExecErrorKind::Spawn));
    }
}
