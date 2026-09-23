//! 路径 MTU 探测：带「不分片」位二分逼近，找出这条链路真正能过多大的包。
//!
//! # 为什么需要
//!
//! 现有 ping 的包长档位（32 / 1600 / 65500）测的是**分片行为**——不带 DF 位，
//! 超长的包会被拆开发过去，照样通。而 1500 与 1492 的差别（PPPoE 封装）
//! 是 CPE 桥接场景里最常见的一类现场故障，那需要 DF 位才看得见。
//!
//! # 它是纯诊断，不做判定项
//!
//! 这是个**刻意的决定**。这个工具的判定层现在只有一个权威——接收端 RX 平均
//! 对门限（ADR-17）。给路径 MTU 开第二条判定路径，就是再造一个「说了算的地方」，
//! 而那正是这套判定一路在收敛的东西。
//!
//! 所以探测的结果是 [`Verdict::Measured`]（「值可信，但没有验收门限」）加一条
//! 写清楚数字的原因。要不要拿 1492 当不合格，由看报告的人决定——
//! 那本来也是个业务判断，不是这个工具该替人下的结论。
//!
//! # 为什么必须先看能力标记
//!
//! 旧版 agent 收到 `dont_fragment` 会**静默忽略**它，照常分片发出去并报成功。
//! 主控据此会得出「1500 能过」——一个听上去很确定的错答案，比拿不到结果糟得多。
//! 没有 [`PING_DF_CAPABILITY`] 时这条腿直接判 `SETUP_ERROR`。
//!
//! # 为什么只做 IPv4
//!
//! **IPv6 里没有 DF 位可设**：分片在 IPv6 中只由源主机做，路由器一律不分片，
//! 协议层面就没有那一位。于是 `ping` 的「不分片」开关在各平台上表现如下——
//!
//! - Windows：`ping -f` 微软文档明确标注 **IPv4-only**，配 `-6` 时被忽略；
//! - macOS：`-D` 是 `ping` 的选项，`ping6` 不认，命令直接失败；
//! - Linux：`-M do` 对 v6 确实有效（内核 `IPV6_MTU_DISCOVER`）。
//!
//! 三条里有两条会给出「大包能过」——**和旧 agent 静默忽略 DF 位是同一个错答案**，
//! 而 [`PING_DF_CAPABILITY`] 只认版本、认不出这一类。主战场又恰恰是 Windows。
//!
//! 所以规则取最简单也最诚实的那条：**路径 MTU 探测只做 IPv4**。按平台分叉的话，
//! 同一份报告在 Linux 主控上有 v6 结果、在 Windows 主控上没有，而两者都不报错。

use super::*;

/// IPv4 头 20 + ICMP 头 8。`ping` 的 `-l` / `-s` 给的是**载荷**长度，
/// 而路径 MTU 说的是整个 IP 包，两者差这些字节。
pub(super) const IPV4_ICMP_OVERHEAD: u32 = 28;

/// 二分的上界（载荷字节）。1500 - 28 = 1472，也就是标准以太网上不分片时
/// ICMP 能带的最大载荷。**不从更大的值起步**：巨帧链路上的路径 MTU 不是
/// 这条探测要回答的问题，而从 9000 起步会让每次探测多跑几拍。
pub(super) const MAX_PROBE_PAYLOAD: u32 = 1472;

/// 每个候选长度试几个包。
///
/// 3 个而不是 1 个：偶发丢包会把一个「其实能过」的长度误判成过不去，
/// 而二分一旦在某一步走错，后面全错。只要有**一个**回应就算这个长度能过。
const PROBES_PER_STEP: u32 = 3;

/// 一次路径 MTU 探测的结果。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PathMtu {
    /// 实测能通过的最大 ICMP 载荷（字节）。
    pub(super) payload: u32,
    /// 换算出来的路径 MTU（载荷 + IP/ICMP 头）。
    pub(super) mtu: u32,
    /// 二分一共发了几轮探测，供排查。
    pub(super) steps: u32,
}

/// 二分逼近的**纯逻辑**部分：给定「某个长度能不能过」的判定器，返回能过的最大长度。
///
/// 抽出来是因为这一层值得单独测：二分的边界条件（全过、全不过、只差一个字节）
/// 靠跑真链路验证不了——真链路的路径 MTU 是个固定值，测不出这几种情形。
///
/// 返回 `(最大可过载荷, 探测轮数)`；下界都过不去时返回 `None`。
pub(super) fn binary_search<F>(lo: u32, hi: u32, mut passes: F) -> Option<(u32, u32)>
where
    F: FnMut(u32) -> bool,
{
    let mut steps = 0u32;
    // 先确认上界能不能过：整条链路都是标准以太网时这一步就结束了，
    // 不必为常见情形付十几轮探测。
    steps += 1;
    if passes(hi) {
        return Some((hi, steps));
    }
    // 再确认下界。下界都过不去说明这条链路根本不通（或者 ICMP 被墙），
    // 那不是「路径 MTU 很小」，是探测本身没有意义。
    steps += 1;
    if !passes(lo) {
        return None;
    }
    let (mut low, mut high) = (lo, hi);
    // 不变式：`low` 能过，`high` 过不去。收敛到相邻即止。
    while high - low > 1 {
        let mid = low + (high - low) / 2;
        steps += 1;
        if passes(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    Some((low, steps))
}

impl Ctx {
    /// 对一条链路做路径 MTU 探测。
    ///
    /// `capable` 是对端 agent 有没有 [`crate::protocol::PING_DF_CAPABILITY`]。
    /// 没有就直接返回错误——**宁可没有结果，也不要一个错答案**。
    pub(super) fn probe_path_mtu(
        &self,
        src: &Endpoint,
        dst: &Endpoint,
        v6: bool,
        capable: bool,
    ) -> Result<PathMtu, String> {
        if !capable {
            return Err("对端 agent 不支持带 DF 位的 ping（缺少 ping_df_v1 能力）。\
                 旧版 agent 会忽略 DF 位、照常分片发出去并报成功，据此得到的\
                 「大包能过」是个错答案——所以这里不测，而不是给一个不能信的数。\
                 请把两端升级到同一版本。"
                .into());
        }
        if v6 {
            return Err("IPv6 不做路径 MTU 探测：IPv6 协议层面没有 DF 位，\
                 路由器一律不分片，`ping -f`（Windows，文档标注 IPv4-only）和 \
                 `ping6 -D`（macOS，ping6 不认这个选项）都给不出可信结果——\
                 得到的「大包能过」和旧 agent 忽略 DF 位是同一个错答案。\
                 这一腿的 IPv4 侧仍会正常探测。"
                .into());
        }
        let (src_addr, dst_addr) = (src.nic.ipv4.clone(), dst.nic.ipv4.clone());
        if src_addr.trim().is_empty() || dst_addr.trim().is_empty() {
            return Err("两端缺少可用地址".into());
        }
        let overhead = IPV4_ICMP_OVERHEAD;

        let mut probe_error: Option<String> = None;
        let result = binary_search(0, MAX_PROBE_PAYLOAD, |payload| {
            if probe_error.is_some() || crate::cancel::is_cancelled() {
                return false;
            }
            let req = PingReq {
                dst: dst_addr.clone(),
                src: src_addr.clone(),
                count: PROBES_PER_STEP,
                payload,
                v6,
                dont_fragment: true,
            };
            match self.ping_at(src.side, &req) {
                // 只要有一个回应就算这个长度能过：偶发丢包会把一个「其实能过」
                // 的长度误判成过不去，而二分一旦走错一步，后面全错。
                Ok(out) => out.received > 0,
                Err(error) => {
                    // RPC 层面的错误和「这个长度过不去」是两件事。记下来，
                    // 让整次探测失败，而不是把它当成一个小 MTU 报出去。
                    probe_error = Some(error);
                    false
                }
            }
        });
        if let Some(error) = probe_error {
            return Err(format!("探测过程中断：{error}"));
        }
        if crate::cancel::is_cancelled() {
            return Err("探测被取消".into());
        }
        let (payload, steps) = result.ok_or_else(|| {
            format!(
                "连 0 字节载荷的 DF 包都没有回应（{src_addr} → {dst_addr}）：\
                 这条链路要么不通，要么 ICMP 被中间设备拦了。\
                 这不是「路径 MTU 很小」，是探测本身没有意义。"
            )
        })?;
        Ok(PathMtu {
            payload,
            mtu: payload + overhead,
            steps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_ethernet_path_is_answered_in_one_probe() {
        // 常见情形不该付十几轮探测的代价：上界能过就直接结束。
        let mut seen = Vec::new();
        let found = binary_search(0, MAX_PROBE_PAYLOAD, |n| {
            seen.push(n);
            n <= MAX_PROBE_PAYLOAD
        });
        assert_eq!(found, Some((MAX_PROBE_PAYLOAD, 1)));
        assert_eq!(seen, vec![MAX_PROBE_PAYLOAD]);
    }

    #[test]
    fn a_pppoe_path_converges_on_the_exact_boundary() {
        // PPPoE：MTU 1492 → 最大载荷 1464。二分必须**正好**停在它上面，
        // 差一个字节就是差一个 MTU 值，而 1492 与 1500 正是要区分的那两个。
        let limit = 1492 - IPV4_ICMP_OVERHEAD;
        let found = binary_search(0, MAX_PROBE_PAYLOAD, |n| n <= limit);
        let (payload, steps) = found.expect("链路是通的");
        assert_eq!(payload, limit);
        assert_eq!(payload + IPV4_ICMP_OVERHEAD, 1492);
        assert!(steps < 16, "1472 的区间不该超过 16 轮，实得 {steps}");
    }

    #[test]
    fn a_link_that_answers_nothing_is_not_reported_as_a_tiny_mtu() {
        // 「ICMP 被墙」和「路径 MTU 很小」是两件事。把前者报成后者，
        // 会让人拿着一个 28 的 MTU 去查设备。
        assert_eq!(binary_search(0, MAX_PROBE_PAYLOAD, |_| false), None);
    }

    #[test]
    fn only_the_lower_bound_getting_through_still_yields_a_number() {
        // 只有 0 能过：这是一条通、但一个字节载荷都过不去的链路。仍然要给出
        // 一个数（0），而不是和「完全不通」混成同一个 None——那两件事的
        // 下一步不一样。二分照样要从 1472 收敛到相邻，所以轮数不是 2。
        let found = binary_search(0, MAX_PROBE_PAYLOAD, |n| n == 0);
        let (payload, steps) = found.expect("0 能过就必须给出结果");
        assert_eq!(payload, 0);
        assert!(steps <= 16, "1472 的区间不该超过 16 轮，实得 {steps}");
    }

    #[test]
    fn the_search_never_reports_a_length_it_did_not_verify() {
        // 二分的不变式：返回值必须是**实际试过并通过**的那个长度。
        for limit in [0, 1, 137, 1000, 1463, 1464, 1471, 1472] {
            let mut verified = std::collections::HashSet::new();
            let found = binary_search(0, MAX_PROBE_PAYLOAD, |n| {
                let ok = n <= limit;
                if ok {
                    verified.insert(n);
                }
                ok
            });
            let (payload, _) = found.expect("0 总是能过");
            assert_eq!(payload, limit, "limit={limit}");
            assert!(verified.contains(&payload), "返回的长度必须是真的试过的");
        }
    }
}
