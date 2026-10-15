//! Excel 直接消费类型化结果，和 HTML 使用同一份测量与判定。
use super::*;
use rust_xlsxwriter::{Format, Workbook};

// Excel 单元格限制按 UTF-16 代码单元计数；完整日志仍保留在 JSON/HTML。
fn cell_text(value: &str) -> String {
    let mut used = 0;
    value
        .chars()
        .take_while(|ch| {
            used += ch.len_utf16();
            used <= 32767
        })
        .collect()
}

pub(super) fn bytes(report: &RunReport) -> Result<Vec<u8>, String> {
    let mut workbook = Workbook::new();
    let header = Format::new().set_bold().set_background_color(0xEDF2F6);
    let sheet = workbook.add_worksheet();
    sheet.set_name("单元结果").map_err(|e| e.to_string())?;
    let titles = [
        "序号",
        "电脑",
        "网口",
        "IP版本",
        "协议",
        "方向",
        "整计划轮次",
        "重复次数",
        "集合",
        "套件",
        "任务",
        "参数",
        "判定",
        "复用历史",
        "接收合计Mbps",
        "合计门限Mbps",
        "原因",
        "诊断",
    ];
    for (i, title) in titles.iter().enumerate() {
        sheet
            .write_string_with_format(0, i as u16, *title, &header)
            .map_err(|e| e.to_string())?;
    }
    sheet.set_freeze_panes(1, 0).map_err(|e| e.to_string())?;
    sheet
        .set_column_range_width(0, 17, 20)
        .map_err(|e| e.to_string())?;
    for (i, unit) in report.units.iter().enumerate() {
        let row = (i + 1) as u32;
        for (col, value) in [
            (0, unit.index as f64),
            (3, unit.ip_version as f64),
            (6, unit.round as f64),
            (7, unit.repeat as f64),
        ] {
            sheet
                .write_number(row, col, value)
                .map_err(|e| e.to_string())?;
        }
        let origin = unit.origin.as_ref();
        let values = [
            (1, unit.host.clone()),
            (2, unit.link.clone()),
            (4, unit.protocol.label().into()),
            (5, unit.direction.label().into()),
            (8, origin.map(|o| o.group.clone()).unwrap_or_default()),
            (9, origin.map(|o| o.suite.clone()).unwrap_or_default()),
            (10, origin.map(|o| o.task.clone()).unwrap_or_default()),
            (11, super::report::parameter_label(unit)),
            (
                12,
                if unit.resumed {
                    "SKIP".into()
                } else {
                    unit.verdict.clone()
                },
            ),
            (13, unit.resumed.to_string()),
            (16, unit.detail.clone()),
            (17, unit.diagnostics.join("；")),
        ];
        for (col, value) in values {
            sheet
                .write_string(row, col, cell_text(&value))
                .map_err(|e| e.to_string())?;
        }
        for (col, value) in [(14, unit.total_mbps), (15, unit.total_target_mbps)] {
            if let Some(value) = value {
                sheet
                    .write_number(row, col, value)
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    sheet
        .autofilter(0, 0, report.units.len() as u32, 17)
        .map_err(|e| e.to_string())?;
    let sheet = workbook.add_worksheet();
    sheet.set_name("方向明细").map_err(|e| e.to_string())?;
    for (col, title) in [
        "单元序号",
        "方向",
        "接收端",
        "接收速率Mbps",
        "门限Mbps",
        "网卡RX Mbps",
        "工具RX Mbps",
        "判定",
        "UDP丢包百分比",
        "采样覆盖",
        "有效秒数",
    ]
    .iter()
    .enumerate()
    {
        sheet
            .write_string_with_format(0, col as u16, *title, &header)
            .map_err(|e| e.to_string())?;
    }
    sheet.set_freeze_panes(1, 0).map_err(|e| e.to_string())?;
    sheet
        .set_column_range_width(0, 10, 22)
        .map_err(|e| e.to_string())?;
    let mut row = 1;
    for unit in &report.units {
        for leg in &unit.legs {
            sheet
                .write_number(row, 0, unit.index as f64)
                .map_err(|e| e.to_string())?;
            for (col, value) in [
                (1, leg.flow.label().to_string()),
                (2, format!("{} {}", leg.receiver_host, leg.receiver)),
                (7, leg.verdict.clone()),
            ] {
                sheet
                    .write_string(row, col, cell_text(&value))
                    .map_err(|e| e.to_string())?;
            }
            for (col, value) in [
                (3, leg.mbps),
                (4, leg.target_mbps),
                (5, leg.nic_rx_mbps),
                (6, leg.tool.receiver_mbps),
                (8, leg.tool.udp_loss_pct),
                (9, Some(leg.coverage)),
                (10, Some(leg.scored_secs())),
            ] {
                if let Some(value) = value {
                    sheet
                        .write_number(row, col, value)
                        .map_err(|e| e.to_string())?;
                }
            }
            row += 1;
        }
    }
    sheet
        .autofilter(0, 0, row.saturating_sub(1), 10)
        .map_err(|e| e.to_string())?;
    let sheet = workbook.add_worksheet();
    sheet.set_name("Ping明细").map_err(|e| e.to_string())?;
    for (col, title) in [
        "单元序号",
        "方向",
        "发送",
        "接收",
        "丢包百分比",
        "平均RTT ms",
        "最大RTT ms",
        "命令",
        "原始输出",
    ]
    .iter()
    .enumerate()
    {
        sheet
            .write_string_with_format(0, col as u16, *title, &header)
            .map_err(|e| e.to_string())?;
    }
    sheet.set_freeze_panes(1, 0).map_err(|e| e.to_string())?;
    let mut row = 1;
    for unit in &report.units {
        for ping in &unit.ping {
            for (col, value) in [
                (0, Some(unit.index as f64)),
                (2, Some(ping.result.sent as f64)),
                (3, Some(ping.result.received as f64)),
                (4, Some(ping.result.loss_pct)),
                (5, ping.result.rtt_avg),
                (6, ping.result.rtt_max),
            ] {
                if let Some(value) = value {
                    sheet
                        .write_number(row, col, value)
                        .map_err(|e| e.to_string())?;
                }
            }
            for (col, value) in [
                (1, ping.flow.label()),
                (7, ping.result.cmd.as_str()),
                (8, ping.result.raw.as_str()),
            ] {
                sheet
                    .write_string(row, col, cell_text(value))
                    .map_err(|e| e.to_string())?;
            }
            row += 1;
        }
    }
    sheet
        .autofilter(0, 0, row.saturating_sub(1), 8)
        .map_err(|e| e.to_string())?;
    let sheet = workbook.add_worksheet();
    sheet.set_name("概览").map_err(|e| e.to_string())?;
    for (row, (label, value)) in [
        ("测试时间", report.created_at.clone()),
        ("完成单元", report.units.len().to_string()),
        (
            "运行状态",
            report.error.clone().unwrap_or_else(|| {
                if report.current.is_empty() {
                    "已结束".into()
                } else {
                    "进行中".into()
                }
            }),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        sheet
            .write_string_with_format(row as u32, 0, label, &header)
            .map_err(|e| e.to_string())?;
        sheet
            .write_string(row as u32, 1, cell_text(&value))
            .map_err(|e| e.to_string())?;
    }
    for (i, verdict) in [
        "PASS",
        "RATE_FAIL",
        "MEASURED",
        "NOT_EVALUATED",
        "SETUP_ERROR",
        "SKIP",
    ]
    .iter()
    .enumerate()
    {
        let row = (i + 4) as u32;
        sheet
            .write_string(row, 0, *verdict)
            .map_err(|e| e.to_string())?;
        sheet
            .write_number(
                row,
                1,
                report
                    .units
                    .iter()
                    .filter(|u| {
                        if *verdict == "SKIP" {
                            u.resumed || u.verdict == "SKIP"
                        } else {
                            !u.resumed && u.verdict == *verdict
                        }
                    })
                    .count() as f64,
            )
            .map_err(|e| e.to_string())?;
    }
    sheet.set_column_width(0, 24).map_err(|e| e.to_string())?;
    sheet.set_column_width(1, 72).map_err(|e| e.to_string())?;
    let sheet = workbook.add_worksheet();
    sheet.set_name("异常单元").map_err(|e| e.to_string())?;
    for (col, title) in ["序号", "电脑 / 网口", "任务", "判定", "原因", "诊断"]
        .iter()
        .enumerate()
    {
        sheet
            .write_string_with_format(0, col as u16, *title, &header)
            .map_err(|e| e.to_string())?;
    }
    let mut row = 1_u32;
    for unit in report
        .units
        .iter()
        .filter(|u| !u.resumed && !matches!(u.verdict.as_str(), "PASS" | "MEASURED" | "SKIP"))
    {
        sheet
            .write_number(row, 0, unit.index as f64)
            .map_err(|e| e.to_string())?;
        for (col, value) in [
            (1, format!("{} / {}", unit.host, unit.link)),
            (
                2,
                unit.origin
                    .as_ref()
                    .map(|o| o.task.clone())
                    .unwrap_or_default(),
            ),
            (3, unit.verdict.clone()),
            (4, unit.detail.clone()),
            (5, unit.diagnostics.join("；")),
        ] {
            sheet
                .write_string(row, col, cell_text(&value))
                .map_err(|e| e.to_string())?;
        }
        row += 1;
    }
    sheet
        .autofilter(0, 0, row.saturating_sub(1), 5)
        .map_err(|e| e.to_string())?;
    sheet.set_freeze_panes(1, 0).map_err(|e| e.to_string())?;
    sheet
        .set_column_range_width(0, 5, 24)
        .map_err(|e| e.to_string())?;
    let sheet = workbook.add_worksheet();
    sheet.set_name("网口汇总").map_err(|e| e.to_string())?;
    for (col, title) in [
        "电脑",
        "网口",
        "IP版本",
        "协议",
        "方向",
        "单元",
        "PASS",
        "RATE_FAIL",
        "MEASURED",
        "NOT_EVALUATED",
        "SETUP_ERROR",
        "历史复用",
        "跳过",
    ]
    .iter()
    .enumerate()
    {
        sheet
            .write_string_with_format(0, col as u16, *title, &header)
            .map_err(|e| e.to_string())?;
    }
    let mut keys = Vec::new();
    for unit in &report.units {
        let key = (
            &unit.host,
            &unit.link,
            unit.ip_version,
            unit.protocol,
            unit.direction,
        );
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    for (index, (host, link, ip, protocol, direction)) in keys.iter().enumerate() {
        let row = index as u32 + 1;
        let members: Vec<_> = report
            .units
            .iter()
            .filter(|u| {
                &u.host == *host
                    && &u.link == *link
                    && u.ip_version == *ip
                    && u.protocol == *protocol
                    && u.direction == *direction
            })
            .collect();
        for (col, value) in [
            (0, host.as_str()),
            (1, link.as_str()),
            (3, protocol.label()),
            (4, direction.label()),
        ] {
            sheet
                .write_string(row, col, cell_text(value))
                .map_err(|e| e.to_string())?;
        }
        sheet
            .write_number(row, 2, *ip as f64)
            .map_err(|e| e.to_string())?;
        sheet
            .write_number(row, 5, members.len() as f64)
            .map_err(|e| e.to_string())?;
        for (col, verdict) in [
            (6, "PASS"),
            (7, "RATE_FAIL"),
            (8, "MEASURED"),
            (9, "NOT_EVALUATED"),
            (10, "SETUP_ERROR"),
            (12, "SKIP"),
        ] {
            sheet
                .write_number(
                    row,
                    col,
                    members
                        .iter()
                        .filter(|u| !u.resumed && u.verdict == verdict)
                        .count() as f64,
                )
                .map_err(|e| e.to_string())?;
        }
        sheet
            .write_number(row, 11, members.iter().filter(|u| u.resumed).count() as f64)
            .map_err(|e| e.to_string())?;
    }
    sheet
        .autofilter(0, 0, keys.len() as u32, 12)
        .map_err(|e| e.to_string())?;
    sheet.set_freeze_panes(1, 0).map_err(|e| e.to_string())?;
    sheet
        .set_column_range_width(0, 12, 18)
        .map_err(|e| e.to_string())?;
    workbook.save_to_buffer().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn long_logs_fit_excel_without_splitting_unicode() {
        let value = super::cell_text(&"测😀".repeat(20000));
        assert!(value.encode_utf16().count() <= 32767);
        assert!(value.starts_with("测😀"));
    }
}
