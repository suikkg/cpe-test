//! iperf3 文本输出（`-f m`）解析：进程结束后的汇总，以及运行中逐行的实时事件。

use crate::protocol::{IperfEventKind, IperfFlowEvent};
use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Default, Clone)]
pub struct IperfParsed {
    pub sender_mbps: Option<f64>,
    pub receiver_mbps: Option<f64>,
    /// 兜底：最后一行出现的速率
    pub last_mbps: Option<f64>,
    pub udp_loss_pct: Option<f64>,
    pub udp_lost_datagrams: Option<u64>,
    pub udp_total_datagrams: Option<u64>,
    /// TCP 全程重传次数（iperf3 sender 汇总行的 `Retr` 列）。
    ///
    /// **只作诊断，不参与判定**（ADR-17）。它回答的是 TCP 没跑满时那个必答题：
    /// 是链路在丢包（重传高），还是窗口没喂饱（重传接近 0、只是发不出去）。
    /// 这两种结论对应的整改动作相反，而在此之前要拿到它只能去翻 raw log。
    ///
    /// `None` = 这段输出里没有 `Retr` 列。UDP 没有这一列，server 侧的
    /// receiver 汇总行也没有——「不知道」和「一次没重传」是两件事，
    /// 后者会让人以为链路是干净的。
    pub tcp_retransmits: Option<u64>,
}

impl IperfParsed {
    pub fn best_sender(&self) -> Option<f64> {
        self.sender_mbps.or(self.last_mbps)
    }
    pub fn best_receiver(&self) -> Option<f64> {
        self.receiver_mbps.or(self.last_mbps)
    }
    pub fn has_measurement(&self) -> bool {
        [self.sender_mbps, self.receiver_mbps, self.last_mbps]
            .iter()
            .any(|v| v.map(|x| x > 0.0).unwrap_or(false))
    }
}

/// 解析 iperf3 文本输出（-f m）
///
/// UDP 丢包只认 server 汇总行（含 `receiver`）里的 `lost/total` 计数，
/// 并且用两个整数自己算，不解析 iperf3 打印的百分比。三条理由：
///
/// 1. 逐秒 interval 行的最后一条常常是不足 1 秒的收尾残帧（`0/0 (0%)`），
///    在整段文本上取「最后一次匹配」会把它当成全程丢包率；
/// 2. iperf3 在接近满丢包时把百分比打成科学计数法（`(1e+02%)`），
///    任何 `\d+(\.\d+)?%` 的正则都匹配不上，真正的汇总行反而被跳过；
/// 3. sender 行的 `0/N (0%)` 是「我全发出去了」，物理上恒为 0，
///    当接收侧丢包率用永远是错的。
///
/// 三条叠加曾把 99.98% 的丢包报成 0.000%（见 .ai/DESIGN-v4.3.0.md D3）。
/// 拿不到 receiver 汇总行时返回 `None` 而不是 0——「不知道」和「没丢」
/// 是两件事，后者会让判定误判为合格。
pub fn parse_output(text: &str) -> IperfParsed {
    let ansi = Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("regex");
    let rate_re = Regex::new(r"(\d+(?:[.,]\d+)?)\s*([KMGT]?)(bits|Bytes)/sec").expect("regex");
    // 只取计数，百分比连捕获都不做：格式由 iperf3 决定，计数不会有歧义。
    let loss_count_re = Regex::new(r"(\d+)\s*/\s*(\d+)\s*\(").expect("regex");
    // TCP 的 `Retr` 列**紧跟在速率单位后面**，而且必须是一个完整的整数：
    //   [  5]  0.00-10.00  sec  1.09 GBytes   933 Mbits/sec  221   sender
    // 锚在单位上是为了排掉 UDP。UDP 的 sender 行同一个位置是抖动
    //   ... 1.05 Mbits/sec  0.000 ms  0/906 (0%)  sender
    // `\d+` 能吃下 `0`，但后面跟的是 `.` 而不是空白，整条匹配就落空——
    // 于是 UDP 永远拿不到 Retr，而不是拿到一个假的 0。
    let retr_re = Regex::new(r"(?:bits|Bytes)/sec\s+(\d+)(?:\s|$)").expect("regex");

    let mut p = IperfParsed::default();
    for raw_line in text.lines() {
        let line = ansi.replace_all(raw_line, "");
        let is_receiver = line.contains("receiver");
        let mut last: Option<f64> = None;
        for cap in rate_re.captures_iter(&line) {
            let num: f64 = cap[1].replace(',', ".").parse().unwrap_or(0.0);
            let unit = &cap[2];
            let kind = &cap[3];
            // iperf3 的两套单位不是同一个进制：bit 单位（Kbits/Mbits/Gbits）
            // 按 1000 进位，Byte 单位（KBytes/MBytes/GBytes）按 1024 进位。
            // 实测：一条 100 Mbits/sec 的流，`-f M` 打印成 11.9 MBytes/sec
            // （100e6/8/1048576 = 11.92）。两边都按 1000 算的话，
            // M 档要低报 4.6%、G 档低报 7.0%——这个量级恰好像测量噪声，
            // 不会触发任何断言，却会让工具自报速率和网卡口径互相矛盾。
            let scale = if kind == "Bytes" { 1024.0 } else { 1000.0 };
            let mut mbps = match unit {
                "K" => num * scale / 1_000_000.0,
                "M" => num * scale * scale / 1_000_000.0,
                "G" => num * scale * scale * scale / 1_000_000.0,
                "T" => num * scale * scale * scale * scale / 1_000_000.0,
                _ => num / 1_000_000.0,
            };
            if kind == "Bytes" {
                mbps *= 8.0;
            }
            last = Some(mbps);
        }
        // 多流时 `[SUM]` 行排在各流之后，多次 attempt 时后一次排在前一次之后，
        // 「后出现的覆盖前面的」因此与速率、丢包两处的取值规则完全一致。
        if line.contains("sender") {
            if let Some(cap) = retr_re.captures(&line) {
                p.tcp_retransmits = cap[1].parse().ok();
            }
        }
        if let Some(v) = last {
            if line.contains("sender") {
                p.sender_mbps = Some(v);
            } else if is_receiver {
                p.receiver_mbps = Some(v);
            } else {
                p.last_mbps = Some(v);
            }
        }
        // 多次 attempt / 多流 `[SUM]` 时，后出现的 receiver 汇总行覆盖前面的，
        // 与上面速率字段的取值规则保持一致。
        if is_receiver {
            if let Some(cap) = loss_count_re.captures_iter(&line).last() {
                let lost: Option<u64> = cap[1].parse().ok();
                let total: Option<u64> = cap[2].parse().ok();
                p.udp_lost_datagrams = lost;
                p.udp_total_datagrams = total;
                p.udp_loss_pct = match (lost, total) {
                    // server 一个数据报都没收到时 total 为 0；此时丢包率无从谈起，
                    // 返回 None 让上层报「未知」，不能算成 0%。
                    (Some(lost), Some(total)) if total > 0 => {
                        Some(lost as f64 * 100.0 / total as f64)
                    }
                    _ => None,
                };
            }
        }
    }
    p
}

fn live_rate(line: &str) -> Option<f64> {
    static RATE_RE: OnceLock<Regex> = OnceLock::new();
    let re = RATE_RE.get_or_init(|| {
        Regex::new(r"(\d+(?:[.,]\d+)?)\s*([KMGT]?)(bits|Bytes)/sec").expect("regex")
    });
    let cap = re.captures_iter(line).last()?;
    let num: f64 = cap[1].replace(',', ".").parse().ok()?;
    let mut mbps = match &cap[2] {
        "K" => num / 1000.0,
        "M" => num,
        "G" => num * 1000.0,
        "T" => num * 1_000_000.0,
        _ => num / 1_000_000.0,
    };
    if &cap[3] == "Bytes" {
        mbps *= 8.0;
    }
    Some(mbps)
}

pub(super) fn classify_live_line(line: &str, elapsed_ms: u64) -> Option<IperfFlowEvent> {
    let lower = line.to_lowercase();
    if lower.contains("connected to") {
        return Some(IperfFlowEvent {
            kind: IperfEventKind::Connected,
            elapsed_ms,
            mbps: None,
            line: line.to_string(),
        });
    }
    if lower.contains("error") || lower.contains("failed") || lower.contains("unable to") {
        return Some(IperfFlowEvent {
            kind: IperfEventKind::Error,
            elapsed_ms,
            mbps: None,
            line: line.to_string(),
        });
    }
    let mbps = live_rate(line)?;
    if mbps > 0.0 {
        return Some(IperfFlowEvent {
            kind: IperfEventKind::Traffic,
            elapsed_ms,
            mbps: Some(mbps),
            line: line.to_string(),
        });
    }
    None
}
