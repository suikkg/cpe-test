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

/// 逐行核查"未经证实的回复"：只要一行看起来像"逐包回复"（带来源地址），
/// 却没有带上真实的 RTT 时间，就不算数——不管它的错误提示是
/// "无法访问目标主机""TTL 超时"，还是以后 Windows 更新后出现的、
/// 从没见过的新措辞。比起维护一份"所有已知错误提示语"的黑名单
/// （永远不可能穷举，新措辞出现就会漏判为成功），反过来只认
/// "确实测到时间"这一条硬指标更可靠，也不用管 Windows 版本/
/// 语言包的用词差异。
fn count_fake_success_replies(text: &str) -> u32 {
    let reply_marker = Regex::new(r"(?i)的回复[:：]|Reply from\b|bytes from\b").expect("regex");
    let has_rtt = Regex::new(r"(?i)时间[<=]\s*\d|time[<=]\s*\d").expect("regex");

    text.lines()
        .filter(|line| reply_marker.is_match(line) && !has_rtt.is_match(line))
        .count() as u32
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

    // 统计行本身会说谎：Windows/BSD 把"目标不可达""TTL 超时""需要分片"等
    // ICMP 错误应答也计入"已接收"，因为本机确实收到了一个回复包，只是
    // 不是来自目标主机的 echo reply。所以 sent/received/loss% 全部正确
    // 却仍可能 100% 都没 ping 通，必须逐行核查回复内容来修正。
    let fake = count_fake_success_replies(text);
    let (received, lost, loss_pct) = if fake > 0 {
        let received = received.saturating_sub(fake);
        let lost = sent.saturating_sub(received);
        let loss_pct = if sent > 0 {
            lost as f64 / sent as f64 * 100.0
        } else {
            100.0
        };
        (received, lost, loss_pct)
    } else {
        (received, lost, loss_pct)
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
}
