//! 报告行的**唯一**构造入口。
//!
//! 全仓有 10 处生产代码在造 `Row`（`executor.rs` 4 处、`cts.rs` 2、`udp.rs` 2、
//! `iperf_leg.rs` 1、`ping_leg.rs` 1）。每一处都要手抄同样的 13 个身份字段
//! （sort_key、task_id、parent_id、task、ip、transport、param、src_pc/iface/ip、
//! dst_pc/iface/ip），再加上 ADR-7 新增的 7 个类型化字段。
//!
//! 手抄的代价 AGENTS.md §3 已经登记在案：**改报告列必须联检 executor 全部 Row
//! 构造点，漏一个就是空列**。空列不会让任何测试变红——它只是在用户的报告里
//! 少一格，而那一格恰好是他要拿去验收的那个数。
//!
//! 所以身份部分收敛成 [`RowIdentity`]：它没有 `Default`，每个字段都必须显式给，
//! 造 `Row` 时用 `Row { ...测量字段..., ..base_row(identity) }`。新增身份字段时，
//! 编译器会把 10 个构造点全都指出来——从「运行期空列」变成「编译期错误」。
use super::*;
use crate::report::{RowBackend, RowDirection, RowProtocol, RowSide};

/// 一条报告行的身份：它是**哪个单元、哪条腿、哪条流**，跑在哪两个端点之间。
///
/// 这里只放「不看测量结果就能确定」的东西。速率、判定、原因码属于测量结果，
/// 由各构造点自己填——那些字段每一处都不一样，收敛没有意义。
pub(super) struct RowIdentity<'a> {
    /// 单元序号（1-based），与日志的 `[i/total]` 和报告的 `#N` 同源。
    pub(super) unit_seq: usize,
    /// 腿序号；同一单元内 A→B / B→A 各占一个。
    pub(super) leg_index: usize,
    /// 流序号；单流恒为 0。
    pub(super) stream_index: usize,
    /// 排序键的第四位：组合计行要排在同组明细之后。
    pub(super) group_flag: u8,
    pub(super) unit: &'a Unit,
    /// 执行侧的腿标签：`""`（单向）/ `"ab"` / `"ba"`。
    pub(super) leg_tag: &'a str,
    pub(super) src: &'a Endpoint,
    pub(super) dst: &'a Endpoint,
    /// `"V4"` / `"V6"`；诊断行可以是空串。
    pub(super) ip: String,
    pub(super) protocol: RowProtocol,
    pub(super) backend: RowBackend,
    /// 展示用的参数摘要（`-P 10`、`-b 2500m -l 14k` …）。
    pub(super) param: String,
    /// 展示用的行类型标签（`灌包`、`★★双向灌包-ab`、`PING` …）。
    pub(super) kind_label: String,
    /// 本行在单元内的唯一 id；单元汇总行传单元 id 本身。
    pub(super) task_id: String,
}

impl RowIdentity<'_> {
    /// 报表分组键，按 ADR-7 定下的优先级取。
    ///
    /// 1. **链路集合名**——用户自己起的名字，最贴近他心里的分组；
    /// 2. **物理网口对**——没有集合名时退到这里；
    /// 3. **角色对**——网口名也拿不到时的最后一档。
    ///
    /// **永远不用主机名**：Arch 机自报 `UNKNOWN-PC`，拿它当键会把一整批不相干的
    /// 链路并成一组，而报表上看不出来是并错了。
    fn link_group(&self) -> String {
        if !self.unit.link_group.trim().is_empty() {
            return self.unit.link_group.trim().to_string();
        }
        let ifaces = (self.src.nic.name.trim(), self.dst.nic.name.trim());
        if !ifaces.0.is_empty() && !ifaces.1.is_empty() {
            return format!("{} ↔ {}", ifaces.0, ifaces.1);
        }
        let roles = (self.src.nic.role.trim(), self.dst.nic.role.trim());
        if !roles.0.is_empty() && !roles.1.is_empty() {
            return format!("{} ↔ {}", roles.0, roles.1);
        }
        String::new()
    }
}

pub(super) fn row_side(side: Side) -> RowSide {
    match side {
        Side::Master => RowSide::Master,
        Side::Agent => RowSide::Agent,
    }
}

/// 造一行「只有身份、没有测量」的 `Row`，供构造点用 `..base_row(id)` 补齐。
pub(super) fn base_row(id: RowIdentity<'_>) -> Row {
    let link_group = id.link_group();
    Row {
        sort_key: (id.unit_seq, id.leg_index, id.stream_index, id.group_flag),
        time: now_full(),
        task_id: id.task_id,
        parent_id: id.unit.id.clone(),
        task: id.unit.title.clone(),
        ip: id.ip,
        transport: id.protocol.label().to_string(),
        param: id.param,
        src_pc: id.src.pc.clone(),
        src_iface: id.src.nic.name.clone(),
        src_ip: id.src.nic.ipv4.clone(),
        dst_pc: id.dst.pc.clone(),
        dst_iface: id.dst.nic.name.clone(),
        dst_ip: id.dst.nic.ipv4.clone(),
        kind_label: id.kind_label,
        unit_seq: id.unit_seq,
        direction: RowDirection::from_leg_tag(id.leg_tag),
        protocol: id.protocol,
        backend: id.backend,
        link_group,
        src_side: row_side(id.src.side),
        dst_side: row_side(id.dst.side),
        ..Default::default()
    }
}

/// 一个单元的两个端点。
///
/// 单元级的行（resume 跳过、网卡消失、单元汇总）手上没有具体的腿，但报表仍然
/// 要知道这一单元跑在哪条链路上——否则「跳过 12 个单元」在报表里就是 12 行
/// 没有归属的空记录，既进不了链路分组，也看不出跳过的是哪条链路。
pub(super) fn unit_endpoints(unit: &Unit) -> Option<(&Endpoint, &Endpoint)> {
    unit.legs.iter().find_map(|leg| match &leg.kind {
        LegKind::IperfSingle(task) => Some((&task.src, &task.dst)),
        LegKind::IperfGroup { streams, .. } => streams.first().map(|task| (&task.src, &task.dst)),
        LegKind::CtsTraffic(task) => Some((&task.src, &task.dst)),
        LegKind::Ping(task) => Some((&task.src, &task.dst)),
    })
}

/// 单元级行（汇总 / resume 跳过 / 网卡消失）的身份。
///
/// 与 [`base_row`] 的区别只有一处：端点从单元的第一条腿上取，取不到就留空。
/// 其余身份字段（分组键、单元序号、标题）走同一条路径，所以单元级行和明细行
/// 在报表里能落进同一个链路分组。
pub(super) fn unit_row(
    unit: &Unit,
    unit_seq: usize,
    kind_label: impl Into<String>,
    protocol: RowProtocol,
    backend: RowBackend,
) -> Row {
    let kind_label = kind_label.into();
    match unit_endpoints(unit) {
        Some((src, dst)) => Row {
            // 单元级行的 transport 列一直是空的（协议写在标题里），保持原样。
            transport: String::new(),
            is_unit_summary: true,
            ..base_row(RowIdentity {
                unit_seq,
                leg_index: 0,
                stream_index: 0,
                group_flag: 0,
                unit,
                leg_tag: "",
                src,
                dst,
                ip: String::new(),
                protocol,
                backend,
                param: String::new(),
                kind_label,
                task_id: unit.id.clone(),
            })
        },
        // 没有腿的单元（理论上不该有）：至少把序号、分组键和标题带上。
        //
        // **这一支也必须走 `base_row`。** 它以前是手写的一份
        // `Row { …, ..Default::default() }`，于是新增身份字段时它是全仓唯一
        // 不会编译失败的构造点——正好是本模块头注释承诺会失败的地方，也正好是
        // 「运行期空列」这个失败模式的原样复刻。
        //
        // 端点用一个空壳：`pc`/`nic` 全空，`link_group()` 因此会正确地退到
        // `unit.link_group`（网口名和角色都是空串，两档都跳过）。`side` 是
        // 唯一没法用空值表达的字段——`Side` 没有「未知」这一档——所以下面把
        // 两个 `*_side` 显式盖成 `RowSide::Unknown`，空壳里填的 `Side` 取值
        // 因此不影响任何输出。
        None => {
            let blank = Endpoint {
                side: Side::Master,
                pc: String::new(),
                nic: Default::default(),
            };
            Row {
                transport: String::new(),
                is_unit_summary: true,
                src_side: RowSide::Unknown,
                dst_side: RowSide::Unknown,
                ..base_row(RowIdentity {
                    unit_seq,
                    leg_index: 0,
                    stream_index: 0,
                    group_flag: 0,
                    unit,
                    leg_tag: "",
                    src: &blank,
                    dst: &blank,
                    ip: String::new(),
                    protocol,
                    backend,
                    param: String::new(),
                    kind_label,
                    task_id: unit.id.clone(),
                })
            }
        }
    }
}
