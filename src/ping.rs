//! ping 命令构造 + 执行 + 输出解析（Windows 中/英文、macOS/BSD 三种格式）

use crate::protocol::{PingOut, PingReq};
use crate::util::run_cmd;
use regex::Regex;
use std::time::Duration;

pub const MAX_PAYLOAD: u32 = 65500;

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
    let raw = if out.timed_out {
        format!("{}\n(ping 命令超时被强制结束)", out.merged())
    } else {
        out.merged()
    };
    let mut r = parse(&raw, req.count);
    r.cmd = cmd_str;
    r.raw = raw;
    r
}

/// 解析 ping 输出。全都匹配不上 => 按全丢处理
pub fn parse(text: &str, count: u32) -> PingOut {
    // 丢包统计
    let cn = Regex::new(
        r"(?s)已发送\s*=\s*(\d+).*?已接收\s*=\s*(\d+).*?丢失\s*=\s*(\d+).*?\((\d+(?:\.\d+)?)%\s*丢失\)",
    )
    .expect("regex");
    let en = Regex::new(
        r"(?si)Sent\s*=\s*(\d+).*?Received\s*=\s*(\d+).*?Lost\s*=\s*(\d+).*?\((\d+(?:\.\d+)?)%\s*loss\)",
    )
    .expect("regex");
    let bsd = Regex::new(
        r"(\d+)\s+packets transmitted,\s*(\d+)\s+(?:packets\s+)?received,\s*(\d+(?:\.\d+)?)%\s+packet loss",
    )
    .expect("regex");

    let (sent, received, lost, loss_pct) = if let Some(c) = cn.captures(text) {
        (
            c[1].parse().unwrap_or(count),
            c[2].parse().unwrap_or(0),
            c[3].parse().unwrap_or(count),
            c[4].parse().unwrap_or(100.0),
        )
    } else if let Some(c) = en.captures(text) {
        (
            c[1].parse().unwrap_or(count),
            c[2].parse().unwrap_or(0),
            c[3].parse().unwrap_or(count),
            c[4].parse().unwrap_or(100.0),
        )
    } else if let Some(c) = bsd.captures(text) {
        let sent: u32 = c[1].parse().unwrap_or(count);
        let recv: u32 = c[2].parse().unwrap_or(0);
        (
            sent,
            recv,
            sent.saturating_sub(recv),
            c[3].parse().unwrap_or(100.0),
        )
    } else {
        (count, 0, count, 100.0)
    };

    // RTT
    let rtt_cn =
        Regex::new(r"(?s)最短\s*=\s*(<?\d+)ms.*?最长\s*=\s*(<?\d+)ms.*?平均\s*=\s*(<?\d+)ms")
            .expect("regex");
    let rtt_en = Regex::new(
        r"(?si)Minimum\s*=\s*(<?\d+)ms.*?Maximum\s*=\s*(<?\d+)ms.*?Average\s*=\s*(<?\d+)ms",
    )
    .expect("regex");
    let rtt_bsd = Regex::new(
        r"(?:round-trip|rtt)[^=]*=\s*(\d+(?:\.\d+)?)/(\d+(?:\.\d+)?)/(\d+(?:\.\d+)?)",
    )
    .expect("regex");

    let (rtt_min, rtt_max, rtt_avg) = if let Some(c) = rtt_cn.captures(text) {
        (parse_ms(&c[1]), parse_ms(&c[2]), parse_ms(&c[3]))
    } else if let Some(c) = rtt_en.captures(text) {
        (parse_ms(&c[1]), parse_ms(&c[2]), parse_ms(&c[3]))
    } else if let Some(c) = rtt_bsd.captures(text) {
        // BSD 顺序是 min/avg/max
        (
            c[1].parse().ok(),
            c[3].parse().ok(),
            c[2].parse().ok(),
        )
    } else {
        (None, None, None)
    };

    let ok = received > 0 && loss_pct < 100.0;
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
64 bytes from 192.168.8.1: icmp_seq=0 ttl=64 time=1.605 ms

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
        let r = parse("Request timed out.\nGeneral failure.", 10);
        assert!(!r.ok);
        assert_eq!(r.sent, 10);
        assert_eq!(r.received, 0);
        assert_eq!(r.loss_pct, 100.0);
    }

    #[test]
    fn test_all_lost_cn() {
        let t = "数据包: 已发送 = 4，已接收 = 0，丢失 = 4 (100% 丢失)，";
        let r = parse(t, 4);
        assert!(!r.ok);
        assert_eq!(r.loss_pct, 100.0);
    }
}
