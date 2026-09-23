//! 负载下时延探针：灌包**正在跑的时候**并发测一条 ICMP 往返。
//!
//! # 为什么需要它
//!
//! 在此之前 ping 和灌包是两种互斥的腿（`builder::LegKind`），而单元之间严格
//! 顺序执行。于是 ping 测到的永远是**空载** RTT。
//!
//! 空载 0.4ms 的设备，满载可能是 300ms——差两个数量级，而用户感知到的「卡」
//! 几乎全部落在后者。缓冲区膨胀（bufferbloat）是 CPE 上最能区分好坏、也最容易
//! 被吞吐数字掩盖的一项：一条链路可以同时「吞吐达标」和「打游戏没法玩」。
//!
//! # 为什么直接复用 `ping::run`
//!
//! Windows 的 `ping` 只报整数毫秒，这让**空载**有线时延那一档几乎失效
//! （实测 0.2~0.9ms 全部记成 0）。但这里恰恰不受影响：负载下的 RTT 是几十到
//! 几百毫秒的量级，1ms 分辨率绰绰有余。同一个工具在两种场景下的可用性完全不同，
//! 所以这里不另造轮子。
//!
//! 采样节奏也跟着 `ping` 走：Windows 的 `ping` 没有间隔参数，固定约 1 秒一次。
//! `count` 取灌包时长的秒数，于是探针和灌包**同起同落**。
//!
//! # 「同起同落」是要写代码保证的，不是自动成立的
//!
//! 探针线程在 `std::thread::scope` 一进去就 spawn，而灌包还要经过 server 启动、
//! TCP connect 就绪探测、每条流 200ms 错峰——一组 20 条流就是好几秒。
//! 立刻开始 ping 的话，这几秒**空载** RTT 混进平均值，恰好把探针要暴露的那件事
//! 稀释掉。所以起跑要等 [`Ctx::wait_for_traffic`]。
//!
//! 收尾同理，而且代价更大：`count` 是按**计划时长**算的，可灌包腿随时会提前结束
//! （server 起不来、连接被拒、预检拦截、操作员点了「停止」或「跳过当前单元」）。
//! 一次不可打断的 `ping::run` 会让一条 2 秒就失败的腿白等 178 秒，210 条就是
//! 十几个小时；点了「跳过」之后灌包作业被杀、探针还在 ping，那个按钮等于没按。
//! 所以整段探测切成 [`PROBE_CHUNK_SECS`] 秒一段：本机那条路直接杀子进程，
//! agent 那条路（同步 HTTP，没有取消通道）靠段边界兜上界。
//!
//! # 它绝不参与判定
//!
//! ADR-17：验收层只吃接收端 RX 平均和门限，`evaluate_rx_acceptance` 在类型上
//! 就收不到别的东西。这里产出的一切只经 `Row` 的展示字段和 `diagnostics` 走
//! 展示通道——**负载下时延再难看，也不会把一条达标的链路翻成 RATE_FAIL**。
//! 历史上正是「判定后再叠一层」让同一种故障在两条路径上得到相反结论。

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// 每段探测多长。
///
/// 它是「白等多久」的上界：灌包腿一旦提前结束，最多再等这么久探针就收手。
/// 10 秒是两头的折中——再短，段与段之间重启 `ping` 的空隙会在采样里占出可见
/// 比例；再长，一条秒级失败的腿要拖的时间就回到不可接受的量级。
pub(super) const PROBE_CHUNK_SECS: u32 = 10;

/// 等流量起来的上限。超过它说明这条腿已经出问题了，探一段空载 RTT 没有意义。
const TRAFFIC_START_TIMEOUT: Duration = Duration::from_secs(120);

/// 等流量起来时的轮询间隔。
const TRAFFIC_START_POLL: Duration = Duration::from_millis(100);

/// 一次负载下时延探针的结果。
#[derive(Debug, Clone, Default)]
pub(super) struct LoadLatency {
    /// 实际收到的回应数。0 表示整段灌包期间一个回应都没有。
    pub(super) received: u32,
    pub(super) sent: u32,
    pub(super) loss_pct: f64,
    pub(super) rtt_avg: Option<f64>,
    pub(super) rtt_max: Option<f64>,
}

impl LoadLatency {
    /// 报告里那一格的说法。
    ///
    /// 全丢和「没测」必须分得开：前者是**结论**（灌包期间这条链路的控制面
    /// 完全不通），后者只是这一轮没开探针。
    pub(super) fn describe(&self) -> String {
        if self.sent == 0 {
            return String::new();
        }
        if self.received == 0 {
            return format!("灌包期间 {} 次探测全部超时（100% 丢失）", self.sent);
        }
        let mut text = String::new();
        if let Some(avg) = self.rtt_avg {
            text.push_str(&format!("平均 {avg:.1} ms"));
        }
        if let Some(max) = self.rtt_max {
            if !text.is_empty() {
                text.push_str(" · ");
            }
            text.push_str(&format!("最大 {max:.1} ms"));
        }
        if !text.is_empty() {
            text.push_str(" · ");
        }
        text.push_str(&format!(
            "丢失 {:.1}%（{}/{}）",
            self.loss_pct, self.received, self.sent
        ));
        text
    }
}

impl Ctx {
    /// 在灌包期间跑一条低速 ICMP 探针。
    ///
    /// 从**发送端**发起、打到接收端地址：排队发生在数据流经过的那一段，
    /// 反着打测的是另一条路径。
    ///
    /// 返回 `None` 表示这一轮压根没探测：开关没开、拿不到地址、或者时长短到
    /// 采不出有意义的样本。**`None` 和「全丢」是两件事**，见 [`LoadLatency::describe`]。
    pub(super) fn probe_load_latency<F: Fn() -> bool>(
        &self,
        src: &Endpoint,
        dst: &Endpoint,
        v6: bool,
        duration_secs: u64,
        stopped: &AtomicBool,
        traffic_running: F,
    ) -> Option<LoadLatency> {
        if !self.cfg.ping.probe_during_traffic {
            return None;
        }
        // 少于 5 秒的灌包采不出能说明问题的分布，探针本身的启动开销反而占比更大。
        if duration_secs < 5 {
            return None;
        }
        let (src_addr, dst_addr) = if v6 {
            let addrs = v6_addrs(&src.nic, &dst.nic)?;
            (
                add_zone(&addrs.client_bind, &src.nic.zone, src.side),
                add_zone(&addrs.client_target, &src.nic.zone, src.side),
            )
        } else {
            (src.nic.ipv4.clone(), dst.nic.ipv4.clone())
        };
        if src_addr.trim().is_empty() || dst_addr.trim().is_empty() {
            return None;
        }
        // Windows 的 ping 固定约 1 秒一拍且没有间隔参数，所以 count 就是秒数。
        // 上限 3600：一条极长的灌包不该把探针的输出撑成几万行。
        let count = duration_secs.min(3_600) as u32;
        let req = PingReq {
            dst: dst_addr,
            src: src_addr,
            count,
            // 32 字节：只测排队时延，不制造额外负载。整段探针合计不到
            // 3 kbps，相对 Gbps 级的灌包是可以忽略的。
            payload: 32,
            v6,
            // 探针测的是排队时延，不是路径 MTU：32 字节永远不会被分片，
            // 带不带 DF 位都一样，所以不带——少一个变量。
            dont_fragment: false,
        };
        if !wait_for_traffic(stopped, traffic_running) {
            return None;
        }

        // 探针失败一律降级为「没探到」：它是诊断，不该让一条测出数的链路
        // 因为 ICMP 被墙而多出一条错误。
        let mut remaining = count;
        let mut sent = 0u32;
        let mut received = 0u32;
        // 加权累加：各段的 `rtt_avg` 是该段内**收到的包**的平均，段长可能不等
        // （最后一段是余数），直接对各段平均值再取平均会给短段过大的权重。
        let mut rtt_weighted_sum = 0.0f64;
        let mut rtt_max: Option<f64> = None;
        while remaining > 0 {
            if stopped.load(Ordering::SeqCst) || crate::cancel::is_cancelled() {
                break;
            }
            let chunk = remaining.min(PROBE_CHUNK_SECS);
            let chunk_req = PingReq {
                count: chunk,
                ..req.clone()
            };
            let Ok((out, cancelled)) = self.ping_at_cancellable(src.side, &chunk_req, stopped)
            else {
                break;
            };
            // 被打断的那一段没有统计行，解析出来是「全丢」。把它计进去等于
            // 凭空捏造一次 100% 丢包——宁可少一段样本。
            if cancelled {
                break;
            }
            sent = sent.saturating_add(out.sent);
            received = received.saturating_add(out.received);
            if let Some(avg) = out.rtt_avg.filter(|v| v.is_finite()) {
                rtt_weighted_sum += avg * f64::from(out.received);
            }
            rtt_max = match (rtt_max, out.rtt_max) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            remaining -= chunk;
        }
        if sent == 0 {
            return None;
        }
        Some(LoadLatency {
            received,
            sent,
            loss_pct: f64::from(sent - received.min(sent)) / f64::from(sent) * 100.0,
            rtt_avg: (received > 0).then(|| rtt_weighted_sum / f64::from(received)),
            rtt_max,
        })
    }
}

/// 等到流量真的起来了才返回 `true`。
///
/// 探针线程是在 `std::thread::scope` 一进去就 spawn 的，而那时灌包还要经过
/// server 启动、TCP connect 就绪探测、每条流 200ms 错峰——一组 20 条流就是
/// 好几秒。立刻开始 ping 的话，这几秒的**空载** RTT 会混进平均值里，
/// 把探针要暴露的那件事（负载下排队）稀释掉。
///
/// 返回 `false` 表示不必探了：这条腿已经结束（失败/取消/跳过），或者流量
/// 压根没起来。
///
/// 写成自由函数而不是 `Ctx` 的方法：它一个字段都不用，而挂在 `Ctx` 上就只能
/// 连着一整个执行器上下文才测得到。
fn wait_for_traffic<F: Fn() -> bool>(stopped: &AtomicBool, traffic_running: F) -> bool {
    // 上限是兜底：流量在这么久之后还没起来，这条腿已经出问题了，
    // 探一段空载 RTT 对判断没有任何帮助。
    let deadline = Instant::now() + TRAFFIC_START_TIMEOUT;
    loop {
        if stopped.load(Ordering::SeqCst) || crate::cancel::is_cancelled() {
            return false;
        }
        if traffic_running() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(TRAFFIC_START_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_that_never_got_a_reply_says_so_instead_of_looking_unmeasured() {
        // 「灌包期间 ICMP 完全不通」是一个结论，不是「没测」。屏幕上这两件事
        // 长得一样，而下一步完全相反：前者要去查设备，后者只是没开开关。
        let dead = LoadLatency {
            sent: 180,
            received: 0,
            loss_pct: 100.0,
            ..Default::default()
        };
        assert!(dead.describe().contains("全部超时"));

        let unmeasured = LoadLatency::default();
        assert!(unmeasured.describe().is_empty(), "没探测就是空串，不留占位");
    }

    #[test]
    fn the_description_carries_average_peak_and_loss_together() {
        // 平均值答不出「有没有卡过」：平均 8ms、峰值 900ms 是典型的缓冲区膨胀，
        // 而只报平均的话它看起来完全正常。
        let bloated = LoadLatency {
            sent: 180,
            received: 176,
            loss_pct: 2.222,
            rtt_avg: Some(8.4),
            rtt_max: Some(912.0),
        };
        let text = bloated.describe();
        assert!(text.contains("平均 8.4 ms"));
        assert!(text.contains("最大 912.0 ms"));
        assert!(text.contains("176/180"));
    }

    /// **流量还没起来就不该开始 ping。**
    ///
    /// 起流前那几秒（server 启动、就绪探测、每条流 200ms 错峰）是空载的，
    /// 混进平均值就把探针要暴露的排队时延稀释掉了。
    #[test]
    fn the_probe_waits_until_traffic_is_actually_flowing() {
        let stopped = AtomicBool::new(false);
        let ticks = std::sync::atomic::AtomicU32::new(0);
        // 前三次问的时候流量还没起来，第四次才起。
        let started = wait_for_traffic(&stopped, || ticks.fetch_add(1, Ordering::SeqCst) >= 3);
        assert!(started);
        assert!(
            ticks.load(Ordering::SeqCst) >= 4,
            "应当反复确认到流量真的起来为止"
        );
    }

    /// **这条腿已经结束就立刻收手，一个包都不发。**
    ///
    /// 这是「跳过当前单元」和「秒级失败的腿」两件事共用的出口：灌包作业被杀了
    /// 之后探针还在 ping，那个按钮就等于没按。
    #[test]
    fn a_leg_that_is_already_over_never_starts_the_probe() {
        let stopped = AtomicBool::new(true);
        assert!(
            !wait_for_traffic(&stopped, || true),
            "收尾标志优先于「流量还在跑」"
        );
    }
}
