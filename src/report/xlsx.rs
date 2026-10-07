//! `summary.xlsx`：HTML 报告之外的第二个结果出口。
//!
//! # 纪律：只吃类型化字段
//!
//! 这个模块**只允许消费判定数据列和类型化的分组键**（`verdict`、`reason_code`、
//! 速率、覆盖率、`direction`/`protocol`/`backend`/`link_group`/`src_side`/`dst_side`），
//! **不许解析任何展示串**。
//!
//! 这不是风格要求。HTML 报告里那套「方向从 `kind_label` 搜 `-ab`、ping 看标题
//! 含不含 PING、UDP 看标题含不含 UDP」的字符串推断，是 ADR-7 点名要消灭的东西：
//! 一条名字里带 "UDP" 的 TCP 测试就能把整组带偏，而报表上看不出来带偏了。
//! Excel 是第二个消费者——在它落地之前把字段类型化，正是为了不让同一批脆弱性
//! 被复制一份。所以这里连 `kind_label`、`task`、`param` 都只当**展示文本**原样
//! 写进单元格，绝不从里面提取结构。
//!
//! # 数值就是数值
//!
//! 速率、丢包、覆盖率一律写成数字单元格而不是字符串。验收的人拿到 xlsx 是要
//! 排序、筛选、做透视表的；写成字符串的话「930.5」会排在「1000」前面。
use super::model::{
    bidirectional_rx_average_sum, direction_row_score, group_is_ping, group_rows, group_seq,
    group_verdict, verdict_row, UnitGroup,
};
use super::{ReportMeta, Row, RowBackend, RowDirection, RowProtocol, RowSide};
use crate::verdict::Verdict;
use rust_xlsxwriter::{Format, FormatAlign, Workbook, Worksheet};
use std::path::Path;

/// 表头样式：加粗 + 冻结首行，长表滚下去还知道每列是什么。
fn header_format() -> Format {
    Format::new()
        .set_bold()
        .set_align(FormatAlign::Left)
        .set_text_wrap()
        .set_background_color(0x00EDF2F6)
}

fn write_headers(
    sheet: &mut Worksheet,
    headers: &[&str],
) -> Result<(), rust_xlsxwriter::XlsxError> {
    let format = header_format();
    for (col, title) in headers.iter().enumerate() {
        sheet.write_string_with_format(0, col as u16, *title, &format)?;
        let width = if title.contains("明细") || title.contains("诊断") || title.contains("建议")
        {
            56.0
        } else if title.contains("标题") || title.contains("上下文") {
            40.0
        } else if title.contains("原因码") || title.contains("网口") || title.contains("参数")
        {
            24.0
        } else {
            18.0
        };
        sheet.set_column_width(col as u16, width)?;
        if title.contains("覆盖率") || *title == "通过率" {
            sheet.set_column_format(col as u16, &Format::new().set_num_format("0.0%"))?;
        }
    }
    sheet.set_row_height(0, 32.0)?;
    sheet.set_freeze_panes(1, 0)?;
    Ok(())
}

fn direction_label(direction: RowDirection) -> &'static str {
    direction.label()
}

fn protocol_label(protocol: RowProtocol) -> &'static str {
    protocol.label()
}

fn backend_label(backend: RowBackend) -> &'static str {
    backend.label()
}

fn side_label(side: RowSide) -> &'static str {
    side.label()
}

/// 写一个可选的数值单元格；`None` 留空而不是写 0。
///
/// 0 和「没测到」在验收里是完全不同的两件事：前者是设备真的没流量，后者是
/// 这一项压根没有测量。填 0 会让平均值和图表都变成谎话。
fn write_opt_number(
    sheet: &mut Worksheet,
    row: u32,
    col: u16,
    value: Option<f64>,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    if let Some(value) = value {
        sheet.write_number(row, col, value)?;
    }
    Ok(())
}

/// 生成 `summary.xlsx`。四张表：概览 / 逐行明细 / 按链路分组 / 失败清单。
pub fn write_xlsx(path: &Path, rows: &[Row], meta: &ReportMeta) -> Result<(), String> {
    let result = write_xlsx_inner(path, rows, meta);
    if let Err(error) = result {
        // 新 HTML 已发布时，旧 Excel 不能继续伪装成本次生成的结果。
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => {}
            Err(cleanup) => {
                return Err(format!("{error}；残留 Excel 清理失败: {cleanup}"));
            }
        }
        return Err(error);
    }
    Ok(())
}

fn write_xlsx_inner(path: &Path, rows: &[Row], meta: &ReportMeta) -> Result<(), String> {
    let mut workbook = Workbook::new();
    let groups = group_rows(rows);

    write_overview_sheet(&mut workbook, &groups, meta).map_err(|e| e.to_string())?;
    write_detail_sheet(&mut workbook, rows).map_err(|e| e.to_string())?;
    write_link_group_sheet(&mut workbook, &groups).map_err(|e| e.to_string())?;
    write_failures_sheet(&mut workbook, &groups).map_err(|e| e.to_string())?;

    workbook.save(path).map_err(|e| e.to_string())
}

/// 表一：每个测试单元一行——和 HTML 报告的「测试概览」同一个粒度。
fn write_overview_sheet(
    workbook: &mut Workbook,
    groups: &[UnitGroup<'_>],
    meta: &ReportMeta,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    let sheet = workbook.add_worksheet();
    sheet.set_name("概览")?;
    write_headers(
        sheet,
        &[
            "序号",
            "判定",
            "原因码",
            "链路组",
            "标题",
            "协议",
            "后端",
            "方向",
            "源端",
            "源网口",
            "目标端",
            "目标网口",
            "RX 平均(Mbps)",
            "双向 RX 平均合计(Mbps)",
            // TX 紧挨着 RX：一眼看出「发出去多少 / 收到多少」。判定口径仍然
            // 只有 RX，TX 是解释性的——RX 不达标时先看这一列是不是压根没发够。
            "TX 平均(Mbps)",
            "目标(Mbps)",
            "采样覆盖率",
            "UDP 丢包(%)",
            "TCP 重传(次)",
            "Ping 丢包(%)",
            "原因明细",
            "诊断(不参与判定)",
            "Ping RTT 最小(ms)",
            "Ping RTT 平均(ms)",
            "Ping RTT 最大(ms)",
        ],
    )?;

    let mut line = 1u32;
    for group in groups {
        // 没有汇总行时选与聚合判定一致的明细，避免失败单元引用通过原因。
        let Some(row) = verdict_row(group) else {
            continue;
        };
        let verdict = group_verdict(group);
        sheet.write_number(line, 0, group_seq(group) as f64)?;
        sheet.write_string(line, 1, verdict.label())?;
        sheet.write_string(line, 2, row.reason_code.as_str())?;
        sheet.write_string(line, 3, &row.link_group)?;
        sheet.write_string(line, 4, &row.task)?;
        sheet.write_string(line, 5, protocol_label(row.protocol))?;
        sheet.write_string(line, 6, backend_label(row.backend))?;
        sheet.write_string(line, 7, direction_label(row.direction))?;
        sheet.write_string(line, 8, side_label(row.src_side))?;
        sheet.write_string(line, 9, &row.src_iface)?;
        sheet.write_string(line, 10, side_label(row.dst_side))?;
        sheet.write_string(line, 11, &row.dst_iface)?;
        write_opt_number(sheet, line, 12, row.rx_avg)?;
        write_opt_number(sheet, line, 13, bidirectional_rx_average_sum(group))?;
        write_opt_number(sheet, line, 14, row.tx_avg)?;
        write_opt_number(sheet, line, 15, row.target_mbps)?;
        write_opt_number(sheet, line, 16, row.sample_coverage)?;
        write_opt_number(sheet, line, 17, row.udp_loss)?;
        write_opt_number(sheet, line, 18, row.tcp_retransmits.map(|v| v as f64))?;
        write_opt_number(sheet, line, 19, row.ping_loss)?;
        sheet.write_string(line, 20, &row.reason_detail)?;
        sheet.write_string(line, 21, row.diagnostics.join("；"))?;
        write_opt_number(sheet, line, 22, row.ping_min)?;
        write_opt_number(sheet, line, 23, row.ping_avg)?;
        write_opt_number(sheet, line, 24, row.ping_max)?;
        line += 1;
    }
    if line > 1 {
        sheet.autofilter(0, 0, line - 1, 24)?;
    }

    // 「运行健康」在 HTML 报告里是一条红色横幅，正常时**不出现**——横幅缺席
    // 本身就是「一切正常」。表格里没有这个语义：一个写着标题、值却是空的格子，
    // 读起来是「这项没算出来」而不是「这项没问题」。所以这里显式写出正常态。
    let run_health = if meta.run_health.trim().is_empty() {
        "正常：本轮没有出现连续多个灌包单元一条测量都没产生的情况".to_string()
    } else {
        meta.run_health.clone()
    };
    let plan_notices = if meta.plan_notices.is_empty() {
        "无".to_string()
    } else {
        meta.plan_notices.join("；")
    };
    // 抬头信息放在数据下方，不占用可筛选的行区。
    let info = [
        ("主控", meta.master_pc.as_str()),
        ("辅测", meta.agent_pc.as_str()),
        ("辅测地址", meta.agent_host.as_str()),
        ("开始", meta.started.as_str()),
        ("结束", meta.finished.as_str()),
        ("耗时", meta.elapsed.as_str()),
        ("运行健康", run_health.as_str()),
        ("计划提示", plan_notices.as_str()),
    ];
    let format = header_format();
    for (offset, (label, value)) in info.iter().enumerate() {
        let at = line + 2 + offset as u32;
        sheet.write_string_with_format(at, 0, *label, &format)?;
        sheet.write_string(at, 1, *value)?;
    }
    Ok(())
}

/// 表二：逐行明细——每条流、每个方向一行。
fn write_detail_sheet(
    workbook: &mut Workbook,
    rows: &[Row],
) -> Result<(), rust_xlsxwriter::XlsxError> {
    let sheet = workbook.add_worksheet();
    sheet.set_name("逐行明细")?;
    write_headers(
        sheet,
        &[
            "单元序号",
            "判定",
            "原因码",
            "链路组",
            "类型",
            "协议",
            "后端",
            "方向",
            "参数",
            "源网口",
            "源 IP",
            "目标网口",
            "目标 IP",
            "工具发送(Mbps)",
            "工具接收(Mbps)",
            "RX 平均(Mbps)",
            "RX-P10(Mbps)",
            // 网卡 TX 侧的两个数只用于诊断；判定仍然只看接收端 RX。
            "TX 平均(Mbps)",
            "TX-P10(Mbps)",
            "目标(Mbps)",
            "采样覆盖率",
            "滚动覆盖率",
            "有效秒",
            "要求秒",
            "UDP 丢包(%)",
            "TCP 重传(次)",
            "源端无线上下文",
            "接收端无线上下文",
            "执行状态",
            "原因明细",
            "诊断(不参与判定)",
            "Ping RTT 最小(ms)",
            "Ping RTT 平均(ms)",
            "Ping RTT 最大(ms)",
        ],
    )?;

    let mut line = 1u32;
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by_key(|row| row.sort_key);
    for row in ordered {
        if row.is_unit_summary {
            // 汇总行在「概览」表里，这里只放真正的测量行。
            continue;
        }
        sheet.write_number(line, 0, row.sort_key.0.saturating_add(1) as f64)?;
        sheet.write_string(line, 1, row.verdict.label())?;
        sheet.write_string(line, 2, row.reason_code.as_str())?;
        sheet.write_string(line, 3, &row.link_group)?;
        sheet.write_string(line, 4, &row.kind_label)?;
        sheet.write_string(line, 5, protocol_label(row.protocol))?;
        sheet.write_string(line, 6, backend_label(row.backend))?;
        sheet.write_string(line, 7, direction_label(row.direction))?;
        sheet.write_string(line, 8, &row.param)?;
        sheet.write_string(line, 9, &row.src_iface)?;
        sheet.write_string(line, 10, &row.src_ip)?;
        sheet.write_string(line, 11, &row.dst_iface)?;
        sheet.write_string(line, 12, &row.dst_ip)?;
        write_opt_number(sheet, line, 13, row.tx_mbps)?;
        write_opt_number(sheet, line, 14, row.rx_mbps)?;
        write_opt_number(sheet, line, 15, row.rx_avg)?;
        write_opt_number(sheet, line, 16, row.rx_p10)?;
        write_opt_number(sheet, line, 17, row.tx_avg)?;
        write_opt_number(sheet, line, 18, row.tx_p10)?;
        write_opt_number(sheet, line, 19, row.target_mbps)?;
        write_opt_number(sheet, line, 20, row.sample_coverage)?;
        write_opt_number(sheet, line, 21, row.rolling_coverage)?;
        write_opt_number(sheet, line, 22, row.effective_seconds)?;
        write_opt_number(sheet, line, 23, row.required_seconds)?;
        write_opt_number(sheet, line, 24, row.udp_loss)?;
        write_opt_number(sheet, line, 25, row.tcp_retransmits.map(|v| v as f64))?;
        sheet.write_string(line, 26, &row.src_wifi)?;
        sheet.write_string(line, 27, &row.dst_wifi)?;
        sheet.write_string(line, 28, row.execution_status.label())?;
        sheet.write_string(line, 29, &row.reason_detail)?;
        sheet.write_string(line, 30, row.diagnostics.join("；"))?;
        write_opt_number(sheet, line, 31, row.ping_min)?;
        write_opt_number(sheet, line, 32, row.ping_avg)?;
        write_opt_number(sheet, line, 33, row.ping_max)?;
        line += 1;
    }
    if line > 1 {
        sheet.autofilter(0, 0, line - 1, 33)?;
    }
    Ok(())
}

/// 一条「某个方向、某块接收网卡」的观测。
///
/// 表三的聚合粒度就是它。**不是**「一个单元一条」：一个双向灌包单元有两个方向、
/// 两块接收网卡，两边的可达速率可以差一个数量级（同一次运行里见过 1821Mbps 对
/// 17Mbps）。把它们压成一行，那一行的「RX 平均最小值」就是两条链路里更差的那条，
/// 而表上看不出它说的是哪一条——另一条的数字则整个消失。
struct LinkObservation<'a> {
    protocol: RowProtocol,
    backend: RowBackend,
    direction: RowDirection,
    /// 接收端网口。判定口径只看接收端 RX，所以聚合键取的是**收的那一块**。
    receiver: &'a str,
    sender: &'a str,
    receiver_side: RowSide,
    sender_side: RowSide,
    verdict: Verdict,
    rx_avg: Option<f64>,
}

/// 把一个单元摊成若干条方向观测。
///
/// 单向单元只有一条，此时判定用**单元判定**（`group_verdict` → `aggregate_verdict`，
/// 判定优先级全仓唯一的那一份），而不是代表行自己的 verdict——单元结论可能来自
/// 多条行的聚合。双向单元每个方向各一条，判定取该方向的行，这与 HTML 报告的
/// 「双向方向汇总（未设置合计门限，每个方向各自按接收端 RX 判定）」是同一个读法。
fn link_observations<'a>(group: &UnitGroup<'a>) -> Vec<LinkObservation<'a>> {
    let mut picked: Vec<(RowDirection, &'a Row)> = Vec::new();
    for row in &group.details {
        match picked.iter_mut().find(|(dir, _)| *dir == row.direction) {
            None => picked.push((row.direction, row)),
            Some((_, best)) => {
                if direction_row_score(row) > direction_row_score(best) {
                    *best = row;
                }
            }
        }
    }
    let unit_verdict = group_verdict(group);
    if picked.is_empty() {
        // 没有明细行的单元（resume 跳过、网卡消失、启动失败）仍然要进表：
        // 「这条链路这次一个单元都没跑成」也是验收结论的一部分。
        let Some(row) = group.summary else {
            return Vec::new();
        };
        return vec![LinkObservation {
            protocol: row.protocol,
            backend: row.backend,
            direction: row.direction,
            receiver: row.dst_iface.as_str(),
            sender: row.src_iface.as_str(),
            receiver_side: row.dst_side,
            sender_side: row.src_side,
            verdict: unit_verdict,
            rx_avg: row.rx_avg,
        }];
    }
    let single = picked.len() == 1;
    picked
        .into_iter()
        .map(|(direction, row)| LinkObservation {
            protocol: row.protocol,
            backend: row.backend,
            direction,
            receiver: row.dst_iface.as_str(),
            sender: row.src_iface.as_str(),
            receiver_side: row.dst_side,
            sender_side: row.src_side,
            verdict: if single { unit_verdict } else { row.verdict },
            rx_avg: row.rx_avg,
        })
        .collect()
}

/// 表三：按**链路 × 协议 × 接收方向**汇总。
///
/// 这是 `link_group` 存在的理由：验收要回答的是「这条链路行不行」，
/// 而不是「第 137 号单元行不行」。分组键的来源优先级在 `executor/row.rs`
/// 里定死（链路集合名 → 物理网口对 → 角色对，**永不用主机名**）。
///
/// 键上除了链路组还带协议与接收端，理由是这张表以前只有链路组一维，于是：
/// TCP 和 UDP 的结果被并成一行（两者的达标线本来就不是一个量级），
/// 而一条双向链路两块接收网卡只留下一个「RX 平均最小值」——更差的那块把另一块
/// 盖掉了，表上还看不出被盖掉的是谁。
fn write_link_group_sheet(
    workbook: &mut Workbook,
    groups: &[UnitGroup<'_>],
) -> Result<(), rust_xlsxwriter::XlsxError> {
    let sheet = workbook.add_worksheet();
    sheet.set_name("按链路分组")?;
    write_headers(
        sheet,
        &[
            "链路组",
            "协议",
            "后端",
            "方向",
            "发送端网口",
            "接收端网口",
            // 双向单元在这张表里占两行（每个方向各自判定），所以计的是
            // 「方向」不是「单元」——列名必须说清楚，否则总数对不上概览页。
            "方向执行数",
            "PASS",
            "RATE_FAIL",
            "MEASURED",
            "NOT_EVALUATED",
            "SETUP_ERROR",
            "SKIP",
            "通过率",
            "RX 平均最小值(Mbps)",
            "RX 平均最大值(Mbps)",
            "发送端",
            "接收端",
        ],
    )?;

    type Key = (
        String,
        RowProtocol,
        RowBackend,
        RowDirection,
        String,
        String,
        RowSide,
        RowSide,
    );
    struct Stats {
        counts: [usize; 6],
        rx_min: Option<f64>,
        rx_max: Option<f64>,
    }

    // 保序聚合：按第一次出现的顺序排，和报告里的顺序一致。
    let mut order: Vec<Key> = Vec::new();
    let mut stats: std::collections::HashMap<Key, Stats> = std::collections::HashMap::new();
    for group in groups {
        for observation in link_observations(group) {
            let link_group = group
                .summary
                .or_else(|| group.details.first().copied())
                .map(|row| row.link_group.clone())
                .unwrap_or_default();
            let key: Key = (
                if link_group.is_empty() {
                    "(未分组)".to_string()
                } else {
                    link_group
                },
                observation.protocol,
                observation.backend,
                observation.direction,
                observation.sender.to_string(),
                observation.receiver.to_string(),
                observation.sender_side,
                observation.receiver_side,
            );
            if !stats.contains_key(&key) {
                order.push(key.clone());
            }
            let entry = stats.entry(key).or_insert(Stats {
                counts: [0; 6],
                rx_min: None,
                rx_max: None,
            });
            let index = match observation.verdict {
                Verdict::Pass => 0,
                Verdict::RateFail => 1,
                Verdict::Measured => 2,
                Verdict::NotEvaluated => 3,
                Verdict::SetupError => 4,
                Verdict::Skip => 5,
            };
            entry.counts[index] += 1;
            if let Some(rx) = observation.rx_avg {
                entry.rx_min = Some(entry.rx_min.map_or(rx, |current: f64| current.min(rx)));
                entry.rx_max = Some(entry.rx_max.map_or(rx, |current: f64| current.max(rx)));
            }
        }
    }

    for (line, key) in order.iter().enumerate() {
        let line = line as u32 + 1;
        let entry = &stats[key];
        let total: usize = entry.counts.iter().sum();
        sheet.write_string(line, 0, &key.0)?;
        sheet.write_string(line, 1, protocol_label(key.1))?;
        sheet.write_string(line, 2, backend_label(key.2))?;
        sheet.write_string(line, 3, direction_label(key.3))?;
        sheet.write_string(line, 4, &key.4)?;
        sheet.write_string(line, 5, &key.5)?;
        sheet.write_number(line, 6, total as f64)?;
        for (offset, count) in entry.counts.iter().enumerate() {
            sheet.write_number(line, 7 + offset as u16, *count as f64)?;
        }
        // 通过率的分母与 HTML 报告一致：只算 PASS 与 RATE_FAIL。
        let judged = entry.counts[0] + entry.counts[1];
        if judged > 0 {
            sheet.write_number(line, 13, entry.counts[0] as f64 / judged as f64)?;
        }
        write_opt_number(sheet, line, 14, entry.rx_min)?;
        write_opt_number(sheet, line, 15, entry.rx_max)?;
        sheet.write_string(line, 16, side_label(key.6))?;
        sheet.write_string(line, 17, side_label(key.7))?;
    }
    if !order.is_empty() {
        sheet.autofilter(0, 0, order.len() as u32, 17)?;
    }
    Ok(())
}

/// 表四：失败清单——只有需要处置的行。
///
/// 判定为 PASS / MEASURED / SKIP 的不进这张表：验收现场先看的是「哪些不行、
/// 该找谁」，把 200 行全列出来等于没有这张表。
fn write_failures_sheet(
    workbook: &mut Workbook,
    groups: &[UnitGroup<'_>],
) -> Result<(), rust_xlsxwriter::XlsxError> {
    let sheet = workbook.add_worksheet();
    sheet.set_name("失败清单")?;
    write_headers(
        sheet,
        &[
            "序号",
            "判定",
            "原因码",
            "链路组",
            "标题",
            "方向",
            "是否 Ping",
            "RX 平均(Mbps)",
            "目标(Mbps)",
            "原因明细",
            "处置建议",
        ],
    )?;

    let mut line = 1u32;
    for group in groups {
        let verdict = group_verdict(group);
        if !matches!(
            verdict,
            Verdict::RateFail | Verdict::NotEvaluated | Verdict::SetupError
        ) {
            continue;
        }
        let Some(row) = verdict_row(group) else {
            continue;
        };
        sheet.write_number(line, 0, group_seq(group) as f64)?;
        sheet.write_string(line, 1, verdict.label())?;
        sheet.write_string(line, 2, row.reason_code.as_str())?;
        sheet.write_string(line, 3, &row.link_group)?;
        sheet.write_string(line, 4, &row.task)?;
        sheet.write_string(line, 5, direction_label(row.direction))?;
        // 「是不是 ping」走类型化的 backend，不看标题里有没有 "PING"。
        sheet.write_string(line, 6, if group_is_ping(group) { "是" } else { "否" })?;
        write_opt_number(sheet, line, 7, row.rx_avg)?;
        write_opt_number(sheet, line, 8, row.target_mbps)?;
        sheet.write_string(line, 9, &row.reason_detail)?;
        sheet.write_string(
            line,
            10,
            crate::verdict::disposition_advice(row.reason_code).unwrap_or(""),
        )?;
        line += 1;
    }
    if line > 1 {
        sheet.autofilter(0, 0, line - 1, 10)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reason::ReasonCode;
    use crate::verdict::ExecutionStatus;

    #[test]
    fn failed_regeneration_removes_the_old_workbook() {
        let path = temp_path("stale");
        let mut rows = vec![summary(0, Verdict::Pass, "A")];
        write_xlsx(&path, &rows, &ReportMeta::default()).unwrap();
        rows[0].reason_detail = "x".repeat(32_768);
        assert!(write_xlsx(&path, &rows, &ReportMeta::default()).is_err());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ping_rtt_is_numeric_in_both_overview_and_details() {
        let path = temp_path("rtt");
        let mut detail = detail(0, Verdict::RateFail, "A");
        detail.protocol = RowProtocol::Icmp;
        detail.backend = RowBackend::Ping;
        detail.ping_min = Some(1.25);
        detail.ping_avg = Some(2.5);
        detail.ping_max = Some(12.0);
        let overview = Row {
            is_unit_summary: true,
            ..detail.clone()
        };
        write_xlsx(&path, &[detail, overview], &ReportMeta::default()).unwrap();
        for (sheet, columns) in [
            ("sheet1", ["W2", "X2", "Y2"]),
            ("sheet2", ["AF2", "AG2", "AH2"]),
        ] {
            let xml = part_xml(&path, &format!("xl/worksheets/{sheet}.xml"));
            for (address, expected) in columns.into_iter().zip(["1.25", "2.5", "12"]) {
                let value = cell(&xml, address).unwrap();
                assert!(value.contains(&format!("<v>{expected}</v>")));
                assert!(!value.contains("t=\"s\""));
            }
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cpe_xlsx_test_{}_{}_{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("summary.xlsx")
    }

    fn part_xml(path: &Path, name: &str) -> String {
        use std::io::Read;
        let file = std::fs::File::open(path).expect("打开工作簿");
        let mut archive = zip::ZipArchive::new(file).expect("有效 xlsx 包");
        let mut xml = String::new();
        archive
            .by_name(name)
            .expect("工作簿部件存在")
            .read_to_string(&mut xml)
            .expect("解压 XML");
        xml
    }

    fn cell<'a>(xml: &'a str, address: &str) -> Option<&'a str> {
        let start = xml.find(&format!("<c r=\"{address}\""))?;
        let end = xml[start..].find("</c>")? + start + "</c>".len();
        Some(&xml[start..end])
    }

    fn detail(unit: usize, verdict: Verdict, link_group: &str) -> Row {
        Row {
            sort_key: (unit, 0, 0, 0),
            task_id: format!("t{unit}"),
            parent_id: format!("unit-{unit}"),
            task: format!("IPERF V4 TCP #{unit}"),
            kind_label: "灌包-ab".into(),
            verdict,
            execution_status: ExecutionStatus::Completed,
            reason_code: if verdict == Verdict::RateFail {
                ReasonCode::RxBelowTarget
            } else {
                ReasonCode::RxTargetMet
            },
            reason_detail: "明细".into(),
            rx_avg: Some(930.5),
            rx_p10: Some(900.0),
            target_mbps: Some(850.0),
            sample_coverage: Some(0.98),
            unit_seq: unit,
            direction: RowDirection::Ab,
            protocol: RowProtocol::Tcp,
            backend: RowBackend::Iperf3,
            link_group: link_group.into(),
            src_side: RowSide::Master,
            dst_side: RowSide::Agent,
            src_iface: "eth0".into(),
            dst_iface: "eth1".into(),
            ..Default::default()
        }
    }

    fn summary(unit: usize, verdict: Verdict, link_group: &str) -> Row {
        Row {
            sort_key: (unit, usize::MAX, usize::MAX, u8::MAX),
            is_unit_summary: true,
            ..detail(unit, verdict, link_group)
        }
    }

    #[test]
    fn interrupted_unit_uses_the_failing_detail_for_its_reason() {
        let pass = detail(0, Verdict::Pass, "A");
        let mut fail = detail(0, Verdict::RateFail, "A");
        fail.sort_key = (0, 1, 0, 0);
        fail.rx_avg = Some(10.0);
        let rows = vec![pass, fail];
        let groups = group_rows(&rows);
        let row = verdict_row(&groups[0]).unwrap();
        assert_eq!(row.verdict, Verdict::RateFail);
        assert_eq!(row.rx_avg, Some(10.0));
        assert!(super::super::reason::group_reason(&groups[0]).contains("RX_BELOW_TARGET"));
        let path = temp_path("interrupted");
        write_xlsx(&path, &rows, &ReportMeta::default()).unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 四张表都要在，而且能被真正的 xlsx 读者打开。
    ///
    /// 这里用 `zip` 直接看包内结构：xlsx 就是一个 zip，表名写在
    /// `xl/workbook.xml` 里。断言到这一层是为了挡住「文件生成了但是空的/坏的」
    /// ——那种失败在 CI 上是绿的，只有用户双击的时候才发现。
    #[test]
    fn the_workbook_has_the_four_sheets_acceptance_actually_uses() {
        let path = temp_path("sheets");
        let rows = vec![
            detail(0, Verdict::Pass, "SGMII ↔ WLAN"),
            summary(0, Verdict::Pass, "SGMII ↔ WLAN"),
            detail(1, Verdict::RateFail, "SGMII ↔ WLAN"),
            summary(1, Verdict::RateFail, "SGMII ↔ WLAN"),
        ];
        write_xlsx(&path, &rows, &ReportMeta::default()).expect("写 xlsx");

        let bytes = std::fs::read(&path).expect("读回");
        assert!(
            bytes.len() > 1000,
            "产物太小，多半是空的: {} 字节",
            bytes.len()
        );
        // xlsx = zip：前两个字节是 PK。
        assert_eq!(&bytes[..2], b"PK", "不是合法的 xlsx/zip");

        let workbook = part_xml(&path, "xl/workbook.xml");
        for name in ["概览", "逐行明细", "按链路分组", "失败清单"] {
            assert!(workbook.contains(&format!("name=\"{name}\"")));
        }
        for (index, end) in [(1, "Y3"), (2, "AH3"), (3, "R2"), (4, "K2")] {
            let sheet = part_xml(&path, &format!("xl/worksheets/sheet{index}.xml"));
            assert!(sheet.contains(&format!("<autoFilter ref=\"A1:{end}\"")));
            assert!(sheet.contains("state=\"frozen\""));
            assert!(sheet.contains("customWidth=\"1\""));
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Excel 出口只吃类型化字段，不许解析展示串。
    ///
    /// 这是 ADR-7 的落点：HTML 报告里那套「方向搜 `kind_label` 的 `-ab`、
    /// ping 看标题含不含 PING、UDP 看标题含不含 UDP」的推断，一条名字里带
    /// "UDP" 的 TCP 测试就能把整组带偏。Excel 是第二个消费者——字段类型化
    /// 就是为了不让同一批脆弱性被复制一份。这条扫源码把它钉住。
    #[test]
    fn the_excel_writer_never_infers_structure_from_display_strings() {
        let source = include_str!("xlsx.rs");
        // 只扫**生产代码**：注释里正要讲这些名字，而这条测试自己的禁用词清单
        // 也在下面的字符串里——把它们算进去就是自己咬自己。
        let production = source
            .split_once("#[cfg(test)]")
            .map(|(head, _)| head)
            .unwrap_or(source);
        let code: String = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "infer_direction_tag",
            "group_is_udp",
            "kind_label.contains",
            "task.contains",
            "to_ascii_uppercase",
        ] {
            assert!(
                !code.contains(banned),
                "Excel 出口用了字符串推断 {banned}：结构信息要走 Row 的类型化字段"
            );
        }
    }

    /// 速率/覆盖率必须是数值单元格，不能是字符串。
    ///
    /// 验收的人拿到 xlsx 是要排序、筛选、做透视表的。写成字符串的话
    /// 「930.5」会排在「1000」前面，而这种错只有在现场排序时才会被发现。
    #[test]
    fn rates_are_written_as_numbers_so_sorting_works() {
        let path = temp_path("numbers");
        let rows = vec![
            detail(0, Verdict::Pass, "A"),
            summary(0, Verdict::Pass, "A"),
        ];
        write_xlsx(&path, &rows, &ReportMeta::default()).expect("写 xlsx");
        let sheet = part_xml(&path, "xl/worksheets/sheet1.xml");
        let rx = cell(&sheet, "M2").expect("RX 平均单元格");
        assert!(rx.contains("<v>930.5</v>"));
        assert!(!rx.contains("t=\"s\""));
        let coverage = cell(&sheet, "Q2").expect("采样覆盖率单元格");
        assert!(coverage.contains("<v>0.98</v>"));
        assert!(coverage.contains(" s=\""), "覆盖率保留数字并应用百分比格式");
        assert!(part_xml(&path, "xl/styles.xml").contains("0.0%"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 双向单元在「按链路分组」里必须一个接收网口一行。
    ///
    /// 这张表以前只按 `link_group` 一维聚合：一条双向链路的两块接收网卡被压成
    /// 一行，「RX 平均最小值」于是只剩两者里更差的那个，而表上看不出它说的是哪
    /// 一块——另一块的数字整个消失。双向链路两个方向差一个数量级是常态
    /// （同一次运行里见过 1821Mbps 对 17Mbps），压成一行等于把结论抹平。
    #[test]
    fn a_bidirectional_unit_gets_one_row_per_receiving_nic() {
        let mut ab = detail(0, Verdict::Pass, "SGMII ↔ WLAN");
        ab.direction = RowDirection::Ab;
        ab.is_grouptotal = true;
        ab.rx_avg = Some(1821.0);
        let mut ba = Row {
            sort_key: (0, 1, 0, 0),
            direction: RowDirection::Ba,
            is_grouptotal: true,
            rx_avg: Some(17.0),
            src_iface: "eth1".into(),
            dst_iface: "eth0".into(),
            verdict: Verdict::RateFail,
            ..detail(0, Verdict::RateFail, "SGMII ↔ WLAN")
        };
        ba.src_iface = "eth1".into();
        ba.dst_iface = "eth0".into();
        ba.kind_label = "灌包-ba".into();
        let rows = vec![ab, ba, summary(0, Verdict::RateFail, "SGMII ↔ WLAN")];
        let groups = group_rows(&rows);
        let observations = link_observations(&groups[0]);

        assert_eq!(observations.len(), 2, "两个方向要各成一条观测");
        let mut seen: Vec<(&str, Option<f64>, Verdict)> = observations
            .iter()
            .map(|o| (o.receiver, o.rx_avg, o.verdict))
            .collect();
        seen.sort_by(|a, b| a.0.cmp(b.0));
        assert_eq!(seen[0], ("eth0", Some(17.0), Verdict::RateFail));
        assert_eq!(seen[1], ("eth1", Some(1821.0), Verdict::Pass));
        assert_eq!(
            bidirectional_rx_average_sum(&groups[0]),
            Some(930.5),
            "配合计门限时概览必须使用汇总行保存的共同窗口 RX"
        );
    }

    /// 单向单元的判定要用**单元判定**，不是代表行自己的 verdict。
    ///
    /// 判定优先级全仓只有一份实现（`verdict::aggregate_verdict`，由
    /// `group_verdict` 转调）。这里若图省事直接读行，就是第二份聚合。
    #[test]
    fn a_single_direction_unit_reports_the_unit_verdict() {
        let mut row = detail(0, Verdict::Measured, "A");
        row.direction = RowDirection::Single;
        let rows = vec![row, summary(0, Verdict::RateFail, "A")];
        let groups = group_rows(&rows);
        let observations = link_observations(&groups[0]);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].verdict, Verdict::RateFail);
    }

    /// 没有明细行的单元（resume 跳过、网卡消失）也要出现在链路表里。
    ///
    /// 「这条链路这次一个单元都没跑成」同样是验收结论，掉出表外等于悄悄变好看。
    #[test]
    fn a_unit_without_detail_rows_still_lands_in_the_link_table() {
        let rows = vec![summary(0, Verdict::Skip, "A")];
        let groups = group_rows(&rows);
        let observations = link_observations(&groups[0]);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].verdict, Verdict::Skip);
    }

    /// 没有测量就留空，不许填 0。
    ///
    /// 0 和「没测到」在验收里是两回事：前者是设备真的没流量，后者是这一项压根
    /// 没测。填 0 会让平均值和图表都变成谎话。
    #[test]
    fn missing_measurements_stay_empty_instead_of_becoming_zero() {
        let path = temp_path("empty");
        let mut row = detail(0, Verdict::NotEvaluated, "A");
        row.rx_avg = None;
        row.target_mbps = None;
        row.sample_coverage = None;
        let rows = vec![
            row.clone(),
            Row {
                is_unit_summary: true,
                ..row
            },
        ];
        write_xlsx(&path, &rows, &ReportMeta::default()).expect("写 xlsx");
        let sheet = part_xml(&path, "xl/worksheets/sheet1.xml");
        for address in ["M2", "P2", "Q2"] {
            assert!(
                cell(&sheet, address).is_none(),
                "{address} 的未采集值必须为空"
            );
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn all_excel_unit_numbers_and_order_match_the_html_report() {
        let path = temp_path("sequence");
        let mut rows = vec![
            detail(36, Verdict::RateFail, "A"),
            summary(36, Verdict::RateFail, "A"),
            detail(1, Verdict::RateFail, "A"),
            summary(1, Verdict::RateFail, "A"),
        ];
        // 历史记录里 unit_seq 缺省为 0，sort_key 仍保存执行顺序。
        for row in &mut rows {
            row.unit_seq = 0;
        }
        write_xlsx(&path, &rows, &ReportMeta::default()).unwrap();
        for index in [1, 2, 4] {
            let sheet = part_xml(&path, &format!("xl/worksheets/sheet{index}.xml"));
            assert!(cell(&sheet, "A2").unwrap().contains("<v>2</v>"));
            assert!(cell(&sheet, "A3").unwrap().contains("<v>37</v>"));
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn identical_windows_nic_names_on_opposite_hosts_do_not_merge_directions() {
        let path = temp_path("same_nic_names");
        let mut forward = detail(0, Verdict::Pass, "以太网 ↔ 以太网");
        forward.direction = RowDirection::Single;
        forward.src_iface = "以太网".into();
        forward.dst_iface = "以太网".into();
        forward.rx_avg = Some(900.0);
        let reverse = Row {
            sort_key: (1, 0, 0, 0),
            parent_id: "unit-1".into(),
            src_side: RowSide::Agent,
            dst_side: RowSide::Master,
            verdict: Verdict::RateFail,
            rx_avg: Some(100.0),
            ..forward.clone()
        };
        write_xlsx(&path, &[forward, reverse], &ReportMeta::default()).unwrap();
        let sheet = part_xml(&path, "xl/worksheets/sheet3.xml");
        for address in ["G2", "G3"] {
            assert!(cell(&sheet, address).unwrap().contains("<v>1</v>"));
        }
        assert!(cell(&sheet, "O2").unwrap().contains("<v>900</v>"));
        assert!(cell(&sheet, "O3").unwrap().contains("<v>100</v>"));
        let value = |address| {
            cell(&sheet, address)
                .unwrap()
                .split_once("<v>")
                .unwrap()
                .1
                .split_once("</v>")
                .unwrap()
                .0
        };
        assert_ne!(value("Q2"), value("Q3"));
        assert_eq!(value("Q2"), value("R3"));
        assert_eq!(value("Q3"), value("R2"));
        assert!(sheet.contains("<autoFilter ref=\"A1:R3\""));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// **以 `=` `+` `-` `@` 开头的文本必须是文本，不是公式**（回归方案 OUT-03）。
    ///
    /// 表格软件里最有名的一类注入：单元格内容以这四个字符之一开头时，某些导入
    /// 路径会把它当公式求值。落到这个工具上，能被人控制又会原样写进 xlsx 的
    /// 字段有一串——链路分组名、任务名、网口名、原因文本，全部来自配置文件或
    /// 网卡自己上报的名字。
    ///
    /// 这里的答案是**结构性**的而不是靠转义：`rust_xlsxwriter` 的
    /// `write_string` 写出的是显式的字符串型单元格，公式要走 `write_formula`
    /// 才写得出来。所以这条测试守两件事——
    /// 1. 生成的工作簿里**一个 `<f>` 元素都没有**（`<f>` 就是公式）；
    /// 2. 生产代码里不出现 `write_formula` / 会自行推断类型的 `write(`。
    ///
    /// 第 2 条是必要的：`rust_xlsxwriter` 的泛型 `write()` 对以 `=` 开头的
    /// 字符串会推断成公式，哪一天有人为了省事换过去，第 1 条要靠恰好构造到
    /// 那个字段才抓得住，第 2 条则当场就红。
    #[test]
    fn text_that_looks_like_a_formula_is_still_written_as_text() {
        let path = temp_path("formula");
        let nasty = "=1+1";
        let mut row = detail(0, Verdict::RateFail, "=cmd|' /c calc'!A1");
        row.task = "+SUM(1,1)".into();
        row.src_iface = "-2+3".into();
        row.dst_iface = "@SUM(A1)".into();
        row.reason_detail = format!("{nasty} 与引号 \" 和 Unicode ✓ 以及 <b>标签</b>");
        let rows = vec![
            row.clone(),
            Row {
                sort_key: (0, usize::MAX, usize::MAX, u8::MAX),
                is_unit_summary: true,
                ..row
            },
        ];
        write_xlsx(&path, &rows, &ReportMeta::default()).expect("写 xlsx");

        let file = std::fs::File::open(&path).expect("打得开");
        let mut zip = zip::ZipArchive::new(file).expect("xlsx 就是一个 zip");
        let mut sheet_xml = String::new();
        let mut saw_payload = false;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).expect("条目");
            let name = entry.name().to_string();
            if !name.ends_with(".xml") {
                continue;
            }
            let mut text = String::new();
            use std::io::Read;
            if entry.read_to_string(&mut text).is_err() {
                continue;
            }
            if text.contains("SUM(A1)") || text.contains("1+1") {
                saw_payload = true;
            }
            if name.contains("sheet") {
                sheet_xml.push_str(&text);
            }
            assert!(
                !text.contains("<f>") && !text.contains("<f "),
                "{name} 里出现了公式元素 <f>——以 = 开头的文本被当成公式写出去了"
            );
        }
        assert!(
            saw_payload,
            "工作簿里没找到那几个构造的字符串，这条测试没验到任何东西"
        );
        assert!(!sheet_xml.is_empty(), "至少要读到一张 sheet");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        // 生产代码不许用会自行推断类型的写法。
        let source = include_str!("xlsx.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .map(|(head, _)| head)
            .unwrap_or(source);
        let code: String = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in ["write_formula", "write_dynamic_formula", "sheet.write("] {
            assert!(
                !code.contains(banned),
                "Excel 出口用了 {banned}：以 = 开头的字符串会被推断成公式"
            );
        }
    }
}
