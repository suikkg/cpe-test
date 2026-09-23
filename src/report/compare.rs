//! 两轮运行的对比：同一套计划，这次比上次差在哪。
//!
//! # 为什么存在
//!
//! 「B12 固件比 B11 掉了多少」是版本验收唯一要回答的问题。在此之前它只能靠
//! 开两个浏览器窗口、人眼比对 210 个单元——一次 11.5 小时的回归跑完，
//! 结论却卡在这一步上。
//!
//! 数据侧的前提本来就齐了：`rows.jsonl` 是结构化的，`Row` 上有类型化的方向 /
//! 协议 / 后端，`plan_hash` 判断得出两轮是不是同一套计划。缺的只是一个消费它们
//! 的出口。
//!
//! # 对齐键不能用 `Unit.id`，也不能用序号
//!
//! **不能用 `unit_seq`**：两轮之间只要有一条链路被跳过或新增，后面所有序号
//! 整体错位，于是每一行都被报成「变了」，而实际上什么都没变。
//!
//! **也不能用 `Unit.id`（RESUME 那个稳定身份）**，尽管它看起来正好合适。
//! 它把网卡的**协商速率**算进了身份（`push_endpoint_identity` 里的
//! `speed_mbps`），这对 RESUME 是对的——一条重新协商到 286Mbps 的 Wi-Fi 链路
//! 不该复用它在 2401Mbps 时拿到的 PASS。但对**对比**恰恰相反：Wi-Fi 一重协商，
//! 同一条测试在两轮里就成了两个 ID，整张表变成「全部新增 + 全部缺失」。
//! 这是拿真实历史数据跑出来的：两轮 `plan_hash` 相同，却一条都对不上。
//!
//! 所以这里自己拼一把**只含不变条件**的键：IP 版本、协议、后端、方向、
//! 两端的「机器 + 网口名」、下发参数、要求时长。它回答的是「这是不是同一个
//! 测试」，而不是「这一轮的条件是不是和上一轮完全一样」。
//!
//! # 判定翻转比数字变化更重要
//!
//! 一条从 1850 掉到 1802 的链路（门限 1800）和一条从 1810 掉到 1790 的，
//! 数字上差得差不多，但后者跨过了门限。所以排序按**先翻转、后跌幅**。

use super::{group_rows, group_verdict, Row, Verdict};

/// 一个单元在两轮之间的变化。
#[derive(Debug, Clone, PartialEq)]
pub struct UnitDelta {
    /// 两轮对齐用的键，见 [`comparison_key`]。**不是** `Unit.id`。
    pub unit_id: String,
    /// 展示标题，优先取新的那一轮。
    pub title: String,
    pub link_group: String,
    pub before: Option<UnitSnapshot>,
    pub after: Option<UnitSnapshot>,
}

/// 一轮里某个单元的结论。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnitSnapshot {
    pub verdict: Verdict,
    pub rx_avg: Option<f64>,
    pub target_mbps: Option<f64>,
}

/// 这一条在两轮之间**发生了什么**。
///
/// 顺序就是严重程度：判定翻坏 → 掉速 → 消失 → 新增 → 提升 → 没变。
/// 报告按它排序，读的人从上往下看就是「先处理最要紧的」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeltaKind {
    /// 上一轮 PASS，这一轮不是了。**回归测试要找的就是这一类。**
    Regressed,
    /// 判定没变，但接收速率明显下降。
    SlowerButStillSameVerdict,
    /// 这一轮没有这个单元（计划改了，或者跑到一半停了）。
    Disappeared,
    /// 上一轮没有这个单元。
    Added,
    /// 上一轮不是 PASS，这一轮是了。
    Fixed,
    /// 判定和速率都没有实质变化。
    Unchanged,
}

impl DeltaKind {
    pub fn label(self) -> &'static str {
        match self {
            DeltaKind::Regressed => "判定变坏",
            DeltaKind::SlowerButStillSameVerdict => "速率下降",
            DeltaKind::Disappeared => "本轮缺失",
            DeltaKind::Added => "本轮新增",
            DeltaKind::Fixed => "判定转好",
            DeltaKind::Unchanged => "无实质变化",
        }
    }
}

/// 速率变化到多少才算「明显」。
///
/// 5% 是测量噪声之上的一条线：同一条链路连跑两次，网卡计数器口径下的差异
/// 通常在 1~2%。取 5% 是为了让这张表**不刷屏**——把每一次 0.3% 的抖动都报成
/// 「下降」，等于没有这张表。真要看逐条数字，表里每一行都带着两轮的原值。
pub const SIGNIFICANT_RATE_CHANGE: f64 = 0.05;

impl UnitDelta {
    /// 相对变化率（`after / before - 1`）。任一轮没有速率时为 `None`。
    pub fn rate_change(&self) -> Option<f64> {
        let before = self.before?.rx_avg?;
        let after = self.after?.rx_avg?;
        if !before.is_finite() || !after.is_finite() || before <= 0.0 {
            return None;
        }
        Some(after / before - 1.0)
    }

    pub fn kind(&self) -> DeltaKind {
        let (Some(before), Some(after)) = (self.before, self.after) else {
            return if self.after.is_some() {
                DeltaKind::Added
            } else {
                DeltaKind::Disappeared
            };
        };
        // 判定翻转优先于数字：跨过门限和没跨过，是两件不同性质的事。
        if before.verdict == Verdict::Pass && after.verdict != Verdict::Pass {
            return DeltaKind::Regressed;
        }
        if before.verdict != Verdict::Pass && after.verdict == Verdict::Pass {
            return DeltaKind::Fixed;
        }
        match self.rate_change() {
            Some(change) if change <= -SIGNIFICANT_RATE_CHANGE => {
                DeltaKind::SlowerButStillSameVerdict
            }
            _ => DeltaKind::Unchanged,
        }
    }

    /// 排序键：先按严重程度，同级里跌得多的排前面。
    fn sort_key(&self) -> (DeltaKind, i64, String) {
        let change = self.rate_change().unwrap_or(0.0);
        // 浮点不能直接进排序键；放大成整数，跌幅越大越靠前。
        let scaled = (change * 10_000.0).round().clamp(-1e9, 1e9) as i64;
        (self.kind(), scaled, self.unit_id.clone())
    }
}

/// 一次对比的完整结果。
#[derive(Debug, Clone, Default)]
pub struct RunComparison {
    pub deltas: Vec<UnitDelta>,
    /// 两轮的 `plan_hash` 是否一致。
    ///
    /// **不一致不是错误**，只是意味着「不能比总数，只能逐条看」：计划变了以后，
    /// 「新增」和「缺失」说的是计划差异而不是设备表现。报告顶部必须把这句话
    /// 说出来，否则读的人会把计划差异当成回归。
    pub same_plan: bool,
}

impl RunComparison {
    pub fn count(&self, kind: DeltaKind) -> usize {
        self.deltas.iter().filter(|d| d.kind() == kind).count()
    }

    /// 有没有值得拦下来的变化（判定变坏或明显掉速）。
    pub fn has_regression(&self) -> bool {
        self.count(DeltaKind::Regressed) > 0 || self.count(DeltaKind::SlowerButStillSameVerdict) > 0
    }
}

/// 一个单元的**跨轮对齐键**。
///
/// 只由「不会随这一轮的现场条件变化」的东西拼成：IP 版本、协议、后端、方向、
/// 两端的机器与网口名、下发参数、要求时长。
///
/// 参数与 IP 版本取自本单元**排序最靠前的那条明细行**——单元汇总行的 `param`
/// 恒为空串（协议写在标题里），而标题里带着网卡协商速率这种每轮都会变的数。
///
/// **含稳定性轮次**：`rounds > 1` 时整份计划会重复 N 遍，上面那些字段在各轮之间
/// 逐字相同。不把轮次拌进来的话，`snapshots()` 的 `HashMap` 会让同一条测试的
/// N 轮互相覆盖、只留最后一轮，而对比报告不会报任何错——「第 13 轮开始掉速」
/// 恰恰是轮次这个功能存在的理由。
///
/// 轮次 `<= 1` 时**一个字节都不加**：历史 run 目录里的行没有 `round` 字段
/// （serde 默认 0），不分轮的新计划是 1，两者都必须和加这个字段之前算出同一把键，
/// 否则昨天的报告和今天的报告全体对不上。
///
/// 刻意**不含**：IP 地址（DHCP 会变）、协商速率（Wi-Fi 每轮都在变）、
/// 序号、时间、`Unit.id`。理由见模块文档。
fn comparison_key(group: &super::model::UnitGroup<'_>) -> Option<String> {
    let summary = group.summary.or_else(|| group.details.first().copied())?;
    // 明细行按 sort_key 排过，第一条就是确定的那条。
    let detail = group.details.first().copied().unwrap_or(summary);
    let endpoint = |side: super::RowSide, iface: &str| {
        format!(
            "{}:{}",
            match side {
                super::RowSide::Master => "master",
                super::RowSide::Agent => "agent",
                super::RowSide::Unknown => "?",
            },
            iface.trim()
        )
    };
    let round = summary.round.max(detail.round);
    let round_part = if round > 1 {
        format!("|r{round}")
    } else {
        String::new()
    };
    Some(format!(
        "{}|{}|{}|{}|{}|{}|{}|{}{round_part}",
        if detail.ip.is_empty() {
            summary.ip.trim()
        } else {
            detail.ip.trim()
        },
        summary.protocol.label(),
        summary.backend.label(),
        summary.direction.label(),
        endpoint(summary.src_side, &summary.src_iface),
        endpoint(summary.dst_side, &summary.dst_iface),
        detail.param.trim(),
        detail
            .required_seconds
            .map(|secs| format!("{secs:.0}s"))
            .unwrap_or_default(),
    ))
}

/// 一个单元在对比报告里要展示的身份：标题和链路组名。
///
/// **只留这两个 String，不留整行 `Row`**：`Row` 挂着 `raws`（整份 iperf3 /
/// ctsTraffic 原文）、`diagnostics` 和 `direction_summaries`，一轮 210 个单元
/// 就是几十 MB。对比只读 `task` 和 `link_group` 两个字段，克隆整行等于把两轮
/// 的全部原始日志同时按在内存里。
struct UnitLabel {
    title: String,
    link_group: String,
}

/// 从一轮的行里抽出「每个单元的结论」，按 [`comparison_key`] 索引。
fn snapshots(rows: &[Row]) -> std::collections::HashMap<String, (UnitSnapshot, UnitLabel)> {
    let mut out = std::collections::HashMap::new();
    for group in group_rows(rows) {
        let Some(row) = group.summary.or_else(|| group.details.first().copied()) else {
            continue;
        };
        let Some(key) = comparison_key(&group) else {
            continue;
        };
        out.insert(
            key,
            (
                UnitSnapshot {
                    verdict: group_verdict(&group),
                    rx_avg: row.rx_avg,
                    target_mbps: row.target_mbps,
                },
                UnitLabel {
                    title: row.task.clone(),
                    link_group: row.link_group.clone(),
                },
            ),
        );
    }
    out
}

/// 对比两轮。`before` 是基线（旧的那一轮），`after` 是这一轮。
pub fn compare(before: &[Row], after: &[Row], same_plan: bool) -> RunComparison {
    let mut old = snapshots(before);
    let new = snapshots(after);

    let mut deltas: Vec<UnitDelta> = Vec::new();
    for (unit_id, (after_snap, after_label)) in &new {
        let removed = old.remove(unit_id);
        deltas.push(UnitDelta {
            unit_id: unit_id.clone(),
            title: after_label.title.clone(),
            link_group: after_label.link_group.clone(),
            before: removed.map(|(snap, _)| snap),
            after: Some(*after_snap),
        });
    }
    // `old` 里剩下的就是这一轮没有的。
    for (unit_id, (before_snap, before_label)) in old {
        deltas.push(UnitDelta {
            unit_id,
            title: before_label.title,
            link_group: before_label.link_group,
            before: Some(before_snap),
            after: None,
        });
    }
    deltas.sort_by_key(|delta| delta.sort_key());
    RunComparison { deltas, same_plan }
}

// ---------------- HTML 出口 ----------------

use super::format::esc;
use super::ReportMeta;

fn rate_text(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), |v| format!("{v:.1}"))
}

fn change_cell(delta: &UnitDelta) -> String {
    match delta.rate_change() {
        Some(change) => {
            let class = if change <= -SIGNIFICANT_RATE_CHANGE {
                "down"
            } else if change >= SIGNIFICANT_RATE_CHANGE {
                "up"
            } else {
                "flat"
            };
            format!("<td class=\"num {class}\">{:+.1}%</td>", change * 100.0)
        }
        // 「算不出」和「没变化」必须分得开：一轮 NOT_EVALUATED 没有速率，
        // 报成 0% 会让人以为这一条稳如泰山。
        None => "<td class=\"num flat\">—</td>".to_string(),
    }
}

fn verdict_cell(snapshot: Option<UnitSnapshot>) -> String {
    match snapshot {
        Some(snapshot) => format!(
            "<td><span class=\"status {}\">{}</span></td>",
            snapshot.verdict.css(),
            snapshot.verdict.label()
        ),
        None => "<td class=\"absent\">未执行</td>".to_string(),
    }
}

/// 渲染成一份自包含的对比报告。
///
/// 和主报告同样的约束：单文件、零外部资源、所有来自行数据的文本都转义。
pub fn render_html(diff: &RunComparison, before: &ReportMeta, after: &ReportMeta) -> String {
    let mut h = String::with_capacity(32 * 1024);
    h.push_str(
        r##"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>CPE 子网测试 · 两轮对比</title>
<style>
:root { color-scheme: light; --ink:#17202a; --muted:#5f6b76; --line:#d8dee4; --surface:#fff; --canvas:#f4f6f8; --head:#edf2f6; }
* { box-sizing: border-box; }
body { margin: 0; padding: 20px; color: var(--ink); background: var(--canvas); font-family: "Segoe UI", "Microsoft YaHei", "PingFang SC", sans-serif; font-size: 14px; line-height: 1.45; }
main { width: min(100%, 1400px); margin: 0 auto; }
h1 { margin: 0 0 12px; font-size: 22px; }
.meta { display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); gap: 1px; margin: 0 0 14px; border: 1px solid var(--line); background: var(--line); }
.meta-item { padding: 9px 12px; background: var(--surface); }
.meta-label { display: block; color: var(--muted); font-size: 11px; }
.meta-value { display: block; font-weight: 600; overflow-wrap: anywhere; }
.tally { display: grid; grid-template-columns: repeat(auto-fit, minmax(120px, 1fr)); gap: 8px; margin: 0 0 14px; }
.stat { padding: 9px 11px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.stat-label { display: block; color: var(--muted); font-size: 11px; }
.stat-value { font-size: 20px; font-weight: 700; }
.stat.bad .stat-value { color: #b3261e; }
.stat.good .stat-value { color: #1b5e20; }
.warn { padding: 10px 12px; margin: 0 0 14px; border: 1px solid #e0c068; background: #fff3cd; border-radius: 6px; }
.scroll { overflow-x: auto; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
table { width: 100%; min-width: 900px; border-collapse: collapse; }
th, td { padding: 7px 10px; text-align: left; border-bottom: 1px solid var(--line); vertical-align: top; }
th { background: var(--head); white-space: nowrap; }
td.num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
td.down { color: #b3261e; font-weight: 700; }
td.up { color: #1b5e20; }
td.flat { color: var(--muted); }
td.absent { color: var(--muted); }
.kind { white-space: nowrap; font-weight: 600; }
.kind-regressed { color: #b3261e; }
.kind-slower { color: #8a5a00; }
.kind-fixed { color: #1b5e20; }
.kind-added, .kind-disappeared, .kind-unchanged { color: var(--muted); }
.status { display: inline-block; padding: 1px 6px; border-radius: 3px; font-weight: 700; font-size: 12px; white-space: nowrap; }
/* 六个判定各自的颜色，取值与主报告逐字相同（`report.rs` 的 `.status.*`）。
   这里以前只有上面那条基类规则，于是 `Verdict::css()` 发出的 pass/fail/…
   全部渲染成同一个墨色——而「基线判定 / 本轮判定」两列并排的**全部意义**
   就是一眼看出哪一行翻了。两份报告的同一个 PASS 长得不一样，比没有颜色更糟。 */
.status.pass { color: #087f3e; }
.status.fail { color: #bd2c2c; }
.status.measured { color: #1769aa; }
.status.not-evaluated { color: #7542a8; }
.status.error { color: #a42121; }
.status.skip { color: #59636c; }
.unit-id { color: var(--muted); font-size: 11px; overflow-wrap: anywhere; }
.note { color: var(--muted); margin: 10px 0 0; }
</style></head><body><main>
<h1>CPE 子网测试 · 两轮对比</h1>
"##,
    );

    h.push_str(&format!(
        "<div class=\"meta\">\
         <div class=\"meta-item\"><span class=\"meta-label\">基线（旧）</span><span class=\"meta-value\">{}</span></div>\
         <div class=\"meta-item\"><span class=\"meta-label\">本轮（新）</span><span class=\"meta-value\">{}</span></div>\
         <div class=\"meta-item\"><span class=\"meta-label\">主控 / 辅测</span><span class=\"meta-value\">{} / {}</span></div>\
         </div>\n",
        esc(if before.started.is_empty() { "未知" } else { &before.started }),
        esc(if after.started.is_empty() { "未知" } else { &after.started }),
        esc(&after.master_pc),
        esc(&after.agent_pc),
    ));

    if !diff.same_plan {
        h.push_str(
            "<p class=\"warn\"><strong>这两轮不是同一套计划</strong>（plan_hash 不同）。\
             逐条的判定和速率仍然可比，但「本轮新增 / 本轮缺失」说的是<strong>计划差异</strong>，\
             不是设备表现——别把它当成回归。</p>\n",
        );
    }

    let tally = [
        (DeltaKind::Regressed, "判定变坏", "bad"),
        (DeltaKind::SlowerButStillSameVerdict, "速率下降", "bad"),
        (DeltaKind::Fixed, "判定转好", "good"),
        (DeltaKind::Added, "本轮新增", ""),
        (DeltaKind::Disappeared, "本轮缺失", ""),
        (DeltaKind::Unchanged, "无实质变化", ""),
    ];
    h.push_str("<div class=\"tally\">");
    for (kind, label, class) in tally {
        h.push_str(&format!(
            "<div class=\"stat {class}\"><span class=\"stat-label\">{label}</span><span class=\"stat-value\">{}</span></div>",
            diff.count(kind)
        ));
    }
    h.push_str("</div>\n");

    h.push_str(
        "<div class=\"scroll\"><table><thead><tr>\
         <th scope=\"col\">变化</th><th scope=\"col\">测试单元</th><th scope=\"col\">链路集合</th>\
         <th scope=\"col\">基线判定</th><th scope=\"col\">本轮判定</th>\
         <th scope=\"col\">基线 RX</th><th scope=\"col\">本轮 RX</th><th scope=\"col\">变化</th>\
         <th scope=\"col\">门限</th></tr></thead><tbody>\n",
    );
    for delta in &diff.deltas {
        let kind = delta.kind();
        let kind_class = match kind {
            DeltaKind::Regressed => "kind-regressed",
            DeltaKind::SlowerButStillSameVerdict => "kind-slower",
            DeltaKind::Fixed => "kind-fixed",
            DeltaKind::Added => "kind-added",
            DeltaKind::Disappeared => "kind-disappeared",
            DeltaKind::Unchanged => "kind-unchanged",
        };
        let target = delta
            .after
            .and_then(|s| s.target_mbps)
            .or_else(|| delta.before.and_then(|s| s.target_mbps));
        h.push_str(&format!(
            "<tr><td class=\"kind {kind_class}\">{}</td>\
             <td>{}<br><span class=\"unit-id\">{}</span></td><td>{}</td>{}{}\
             <td class=\"num\">{}</td><td class=\"num\">{}</td>{}<td class=\"num\">{}</td></tr>\n",
            kind.label(),
            esc(&delta.title),
            esc(&delta.unit_id),
            esc(&delta.link_group),
            verdict_cell(delta.before),
            verdict_cell(delta.after),
            rate_text(delta.before.and_then(|s| s.rx_avg)),
            rate_text(delta.after.and_then(|s| s.rx_avg)),
            change_cell(delta),
            rate_text(target),
        ));
    }
    h.push_str("</tbody></table></div>\n");
    h.push_str(&format!(
        "<p class=\"note\">速率单位 Mbps，口径是接收端网卡 RX 平均——和两份报告里的判定口径完全一致。\
         「变化」超过 ±{:.0}% 才被标成上升/下降：同一条链路连跑两次，网卡口径下 1~2% 的差异是常态。\
         对齐**不用**单元稳定 ID（RESUME 那个身份把网卡协商速率算了进去，Wi-Fi 一重协商\
         同一条测试就成了两个 ID），而是按「IP 版本 + 协议 + 后端 + 方向 + 两端网口 + 参数 + 时长\
         （+ 稳定性轮次）」对齐，所以中间插一条不会让后面整体错位。</p>\n",
        SIGNIFICANT_RATE_CHANGE * 100.0
    ));
    h.push_str("</main></body></html>\n");
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{ReportMeta, RowBackend, RowDirection, RowProtocol, RowSide};

    /// 同一条测试的第 `round` 轮。除了轮次，每个字段都和 [`unit`] 逐字相同——
    /// 这正是「不把轮次拌进对齐键就会互相覆盖」的形状。
    fn unit_in_round(id: &str, verdict: Verdict, rx: Option<f64>, round: u32) -> Vec<Row> {
        unit(id, verdict, rx)
            .into_iter()
            .map(|mut row| {
                row.round = round;
                row.task = format!("{} · 第 {round} 轮", row.task);
                // 生产里每轮有独立的稳定身份（`round_scoped_id`），`parent_id`
                // 因此各不相同——分组是按它做的，fixture 不能偷懒共用一个。
                row.parent_id = format!("{}-r{round}", row.parent_id);
                row.task_id = format!("{}-r{round}", row.task_id);
                row
            })
            .collect()
    }

    /// 造一个「一条明细 + 一条汇总」的单元。
    ///
    /// `id` 只用来区分**参数档位**（真实报告里就是 `-w 4m -P 2` 这类），
    /// 两端网口固定——对齐键读的正是这几样。
    fn unit(id: &str, verdict: Verdict, rx: Option<f64>) -> Vec<Row> {
        let base = Row {
            parent_id: format!("run-local-{id}"),
            link_group: "SGMII ↔ WLAN".into(),
            ip: "V4".into(),
            protocol: RowProtocol::Tcp,
            backend: RowBackend::Iperf3,
            direction: RowDirection::Single,
            src_side: RowSide::Master,
            src_iface: "en0".into(),
            dst_side: RowSide::Agent,
            dst_iface: "en1".into(),
            required_seconds: Some(180.0),
            ..Default::default()
        };
        vec![
            Row {
                param: format!("-w {id}"),
                task_id: format!("run-local-{id}-flow"),
                ..base.clone()
            },
            Row {
                task: format!("IPERF V4 TCP -w {id}"),
                task_id: format!("run-local-{id}"),
                verdict,
                rx_avg: rx,
                target_mbps: Some(1800.0),
                is_unit_summary: true,
                ..base
            },
        ]
    }

    fn rows(items: &[(&str, Verdict, Option<f64>)]) -> Vec<Row> {
        let mut out = Vec::new();
        for (index, (id, verdict, rx)) in items.iter().enumerate() {
            let mut unit = unit(id, *verdict, *rx);
            unit[0].sort_key = (index, 0, 0, 0);
            unit[0].unit_seq = index;
            unit[1].sort_key = (index, usize::MAX, 0, 0);
            unit[1].unit_seq = index;
            out.extend(unit);
        }
        out
    }

    #[test]
    fn a_pass_that_stopped_passing_is_the_first_thing_you_see() {
        // 回归测试要找的就是这一类，所以它必须排在最前面——不管它跌了多少。
        let before = rows(&[
            ("a", Verdict::Pass, Some(1850.0)),
            ("b", Verdict::Pass, Some(2000.0)),
        ]);
        let after = rows(&[
            ("a", Verdict::Pass, Some(1840.0)),
            ("b", Verdict::RateFail, Some(1790.0)),
        ]);
        let diff = compare(&before, &after, true);
        assert!(
            diff.deltas[0].unit_id.contains("-w b"),
            "最靠前的应当是 b: {}",
            diff.deltas[0].unit_id
        );
        assert_eq!(diff.deltas[0].kind(), DeltaKind::Regressed);
        assert_eq!(diff.count(DeltaKind::Regressed), 1);
        assert!(diff.has_regression());
    }

    #[test]
    fn alignment_survives_an_inserted_unit_instead_of_shifting_everything() {
        // 用序号对齐的话，中间插一条会让后面每一行都被报成「变了」。
        let before = rows(&[
            ("a", Verdict::Pass, Some(1000.0)),
            ("c", Verdict::Pass, Some(3000.0)),
        ]);
        let after = rows(&[
            ("a", Verdict::Pass, Some(1000.0)),
            ("b", Verdict::Pass, Some(2000.0)),
            ("c", Verdict::Pass, Some(3000.0)),
        ]);
        let diff = compare(&before, &after, true);
        assert_eq!(diff.count(DeltaKind::Added), 1);
        assert_eq!(diff.count(DeltaKind::Unchanged), 2, "a 和 c 都没变");
        assert_eq!(diff.count(DeltaKind::Regressed), 0);
    }

    #[test]
    fn small_swings_are_noise_and_do_not_fill_the_table() {
        // 同一条链路连跑两次，网卡口径下 1~2% 的差异是常态。把每一次都报成
        // 「下降」，这张表就没用了。
        let before = rows(&[("a", Verdict::Pass, Some(2000.0))]);
        let quiet = rows(&[("a", Verdict::Pass, Some(1960.0))]);
        assert_eq!(
            compare(&before, &quiet, true).deltas[0].kind(),
            DeltaKind::Unchanged
        );

        let loud = rows(&[("a", Verdict::Pass, Some(1700.0))]);
        let diff = compare(&before, &loud, true);
        assert_eq!(diff.deltas[0].kind(), DeltaKind::SlowerButStillSameVerdict);
        assert!((diff.deltas[0].rate_change().unwrap() + 0.15).abs() < 1e-9);
    }

    #[test]
    fn a_unit_that_started_passing_is_reported_as_fixed_not_as_noise() {
        let before = rows(&[("a", Verdict::RateFail, Some(1200.0))]);
        let after = rows(&[("a", Verdict::Pass, Some(1900.0))]);
        let diff = compare(&before, &after, true);
        assert_eq!(diff.deltas[0].kind(), DeltaKind::Fixed);
        assert!(!diff.has_regression());
    }

    #[test]
    fn missing_rates_never_turn_into_a_fake_change() {
        // 一轮 NOT_EVALUATED 没有 rx_avg。拿它和有数的那轮算变化率会得出
        // 「跌了 100%」，而实际情况是「这一轮没测出来」。
        let before = rows(&[("a", Verdict::Pass, Some(2000.0))]);
        let after = rows(&[("a", Verdict::NotEvaluated, None)]);
        let diff = compare(&before, &after, true);
        assert_eq!(diff.deltas[0].rate_change(), None);
        // 判定确实从 PASS 掉下来了，这一条仍然要报。
        assert_eq!(diff.deltas[0].kind(), DeltaKind::Regressed);
    }

    #[test]
    fn a_different_plan_is_flagged_rather_than_silently_compared() {
        // 计划变了以后，「新增」和「缺失」说的是计划差异而不是设备表现。
        // 不说这句话，读的人会把它当成回归。
        let diff = compare(&rows(&[]), &rows(&[("a", Verdict::Pass, Some(1.0))]), false);
        assert!(!diff.same_plan);
        assert!(
            render_html(&diff, &ReportMeta::default(), &ReportMeta::default())
                .contains("不是同一套计划")
        );
    }

    /// **协商速率变了，对齐不许散架。**
    ///
    /// 这条是拿真实历史数据（`run_20260909_213024` vs `run_20260909_220554`，
    /// 两轮 `plan_hash` 相同）跑出来的：Wi-Fi 从 2401Mbps 重新协商到 286Mbps，
    /// 而 `Unit.id` 把 `speed_mbps` 算进了身份，于是同一条测试成了两个 ID，
    /// 整张表变成「全部新增 + 全部缺失」，一条都对不上。
    ///
    /// 对齐键因此**不含任何现场条件**——那正是要对比的东西本身。
    #[test]
    fn a_renegotiated_wifi_rate_must_not_split_one_test_into_two_rows() {
        let mut before = rows(&[("4m", Verdict::Pass, Some(1900.0))]);
        let mut after = rows(&[("4m", Verdict::RateFail, Some(260.0))]);
        // 标题里带着协商速率（真实报告就是这样），两轮不同。
        before[1].task = "IPERF V4 TCP | 主控 en0(1000Mbps) -> 辅测 en1(2401Mbps, 5GHz)".into();
        after[1].task = "IPERF V4 TCP | 主控 en0(1000Mbps) -> 辅测 en1(286Mbps, 5GHz)".into();
        // `Unit.id` 也因此不同——正是真实数据里发生的事。
        before[1].task_id = "5e03cc8abbb9a77a6dec60f7b3aff298".into();
        after[1].task_id = "5f2c6c2a33e1867fe9642ed3a62313e4".into();

        let diff = compare(&before, &after, true);
        assert_eq!(diff.deltas.len(), 1, "同一条测试只能出现一行");
        assert_eq!(diff.count(DeltaKind::Added), 0);
        assert_eq!(diff.count(DeltaKind::Disappeared), 0);
        assert_eq!(diff.deltas[0].kind(), DeltaKind::Regressed);
        assert!(diff.has_regression());
    }

    /// IP 地址变了（DHCP 续租）同样不许拆散对齐。
    #[test]
    fn a_new_dhcp_address_still_lines_the_same_test_up() {
        let mut before = rows(&[("4m", Verdict::Pass, Some(1900.0))]);
        let mut after = rows(&[("4m", Verdict::Pass, Some(1880.0))]);
        before[0].src_ip = "192.168.8.100".into();
        before[1].src_ip = "192.168.8.100".into();
        after[0].src_ip = "192.168.8.137".into();
        after[1].src_ip = "192.168.8.137".into();
        let diff = compare(&before, &after, true);
        assert_eq!(diff.deltas.len(), 1);
        assert_eq!(diff.deltas[0].kind(), DeltaKind::Unchanged);
    }

    /// 换了网口、换了档位、换了时长**必须**被看成不同的测试。
    #[test]
    fn a_different_port_profile_or_duration_is_genuinely_a_different_test() {
        let before = rows(&[("4m", Verdict::Pass, Some(1900.0))]);

        let mut other_iface = rows(&[("4m", Verdict::Pass, Some(1900.0))]);
        other_iface[0].dst_iface = "en2".into();
        other_iface[1].dst_iface = "en2".into();
        assert_eq!(compare(&before, &other_iface, true).deltas.len(), 2);

        // 档位（下发参数）不同。
        assert_eq!(
            compare(
                &before,
                &rows(&[("64k", Verdict::Pass, Some(1900.0))]),
                true
            )
            .deltas
            .len(),
            2
        );

        // 时长不同。
        let mut other_duration = rows(&[("4m", Verdict::Pass, Some(1900.0))]);
        other_duration[0].required_seconds = Some(60.0);
        other_duration[1].required_seconds = Some(60.0);
        assert_eq!(compare(&before, &other_duration, true).deltas.len(), 2);
    }

    #[test]
    fn the_html_is_self_contained_and_escapes_everything_from_the_rows() {
        let before = rows(&[("a", Verdict::Pass, Some(2000.0))]);
        let mut after = rows(&[("a", Verdict::RateFail, Some(900.0))]);
        // 标题优先取新的那一轮，所以敌意字符串要放在 after 上才测得到转义。
        after[1].task = "<script>alert(1)</script>".into();
        let html = render_html(
            &compare(&before, &after, true),
            &ReportMeta::default(),
            &ReportMeta::default(),
        );
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
        // 单文件离线：不许有任何外部引用。
        assert!(!html.contains("http://") && !html.contains("https://"));
        assert!(html.contains("判定变坏"));
        assert!(html.contains("-55.0%"), "跌幅要写出来");
    }

    /// **每一轮都要单独出现在对比表里。**
    ///
    /// 轮次不进对齐键的话，`snapshots()` 的 `HashMap` 会让 N 轮互相覆盖、只剩
    /// 最后一轮，而对比报告不报任何错——「第 13 轮开始掉速」正是轮次这个功能
    /// 存在的理由，也正是这种覆盖会吃掉的东西。
    #[test]
    fn every_round_gets_its_own_row_instead_of_overwriting_the_previous_one() {
        let mut before = Vec::new();
        let mut after = Vec::new();
        for round in 1..=3u32 {
            before.extend(unit_in_round("4m", Verdict::Pass, Some(1850.0), round));
        }
        // 第 2 轮开始掉速，另外两轮照旧。
        after.extend(unit_in_round("4m", Verdict::Pass, Some(1850.0), 1));
        after.extend(unit_in_round("4m", Verdict::RateFail, Some(1200.0), 2));
        after.extend(unit_in_round("4m", Verdict::Pass, Some(1840.0), 3));

        let cmp = compare(&before, &after, true);
        assert_eq!(cmp.deltas.len(), 3, "三轮就该有三行：{:?}", cmp.deltas);
        assert_eq!(cmp.count(DeltaKind::Regressed), 1);
        assert_eq!(cmp.count(DeltaKind::Disappeared), 0);
        assert_eq!(cmp.count(DeltaKind::Added), 0);
    }

    /// 不分轮的计划（以及历史数据里 `round` 缺省的 0）必须算出**加轮次之前
    /// 那把一模一样的键**，否则昨天的报告和今天的报告全体对不上。
    #[test]
    fn a_single_round_plan_keys_exactly_like_it_did_before_rounds_existed() {
        let legacy = unit("4m", Verdict::Pass, Some(1850.0)); // round 恒为 0
        let single = unit_in_round("4m", Verdict::Pass, Some(1850.0), 1);
        let cmp = compare(&legacy, &single, true);
        assert_eq!(cmp.deltas.len(), 1, "应当对齐成同一条：{:?}", cmp.deltas);
        assert_eq!(cmp.count(DeltaKind::Unchanged), 1);
    }

    /// 报告脚注**不许**说对齐用的是单元稳定 ID。
    ///
    /// 那句话曾经印在用户读的那份产物里，而它和本模块顶上的说明正好相反。
    /// 按 CLAUDE.md 的标准，一条听上去很确定的错误指引比没有指引更危险。
    #[test]
    fn the_footer_does_not_claim_the_alignment_uses_the_resume_identity() {
        let html = render_html(
            &compare(
                &unit("4m", Verdict::Pass, Some(1850.0)),
                &unit("4m", Verdict::Pass, Some(1840.0)),
                true,
            ),
            &ReportMeta::default(),
            &ReportMeta::default(),
        );
        assert!(
            !html.contains("对齐用的是单元稳定 ID"),
            "脚注仍在宣称用稳定 ID 对齐"
        );
        assert!(html.contains("对齐**不用**单元稳定 ID") || html.contains("对齐"));
    }

    /// 六个判定各自都要有颜色规则。
    ///
    /// 只有 `.status` 基类时，`Verdict::css()` 发出的类全部渲染成同一个墨色，
    /// 而「基线判定 / 本轮判定」两列并排的全部意义就是一眼看出哪一行翻了。
    #[test]
    fn the_comparison_stylesheet_colours_every_verdict() {
        let html = render_html(
            &compare(
                &unit("4m", Verdict::Pass, Some(1850.0)),
                &unit("4m", Verdict::RateFail, Some(900.0)),
                true,
            ),
            &ReportMeta::default(),
            &ReportMeta::default(),
        );
        for verdict in [
            Verdict::Pass,
            Verdict::RateFail,
            Verdict::Measured,
            Verdict::NotEvaluated,
            Verdict::SetupError,
            Verdict::Skip,
        ] {
            let rule = format!(".status.{} {{ color:", verdict.css());
            assert!(
                html.contains(&rule),
                "{} 少一条配色规则（{rule}）",
                verdict.label()
            );
        }
    }
}
