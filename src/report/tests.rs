//! `report` 的测试。
//!
//! 报告是这套工具唯一的对外产物，它的每一处措辞和结构都被人当作结论在读。
//! 这里累积的是一条条「报告曾经把某件事说错」的回归，和渲染代码的变更节奏
//! 不同，因此单独成文件。

/// 超长原始输出要掐头去尾，而不是只留开头。
///
/// iperf3 / ctsTraffic 的汇总行在**最后**，只留开头等于把结论截掉。
/// 同一份文本已经作为 raw_log 单独落盘，内嵌这份是给「点开就看」用的。
#[test]
fn an_oversized_raw_segment_keeps_both_ends() {
    let short = "一切正常";
    assert_eq!(super::embedded_raw(short), short, "没超限的原样保留");

    let body = "x".repeat(super::EMBEDDED_RAW_MAX_CHARS * 2);
    let text = format!("开头标记\n{body}\n结尾汇总行 receiver");
    let trimmed = super::embedded_raw(&text);

    assert!(trimmed.chars().count() < text.chars().count(), "必须变短");
    assert!(trimmed.starts_with("开头标记"), "开头要留");
    assert!(
        trimmed.ends_with("结尾汇总行 receiver"),
        "结尾的汇总行必须留住，那才是最常要看的一段"
    );
    assert!(trimmed.contains("中间省略"), "省略要说出来");
    assert!(trimmed.contains("独立原始记录"), "要指向完整那份");
}

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static REPORT_INDEX: AtomicUsize = AtomicUsize::new(0);

fn render(rows: Vec<Row>) -> String {
    render_with_meta(rows, &ReportMeta::default())
}

fn render_with_meta(mut rows: Vec<Row>, meta: &ReportMeta) -> String {
    let index = REPORT_INDEX.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("cpe_report_test_{}_{}", std::process::id(), index));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("report.html");
    write_report(&path, &mut rows, meta).unwrap();
    let html = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(dir);
    html
}

fn traffic_detail(unit: &str, sort_key: (usize, usize, usize, u8)) -> Row {
    Row {
        sort_key,
        time: "2026-07-04 12:00:00".into(),
        task_id: format!("{unit}-flow"),
        parent_id: unit.into(),
        task: "IPERF V4 TCP".into(),
        transport: "TCP".into(),
        param: "-P 1".into(),
        src_pc: "master".into(),
        src_iface: "eth0".into(),
        src_ip: "192.0.2.1".into(),
        dst_pc: "agent".into(),
        dst_iface: "eth1".into(),
        dst_ip: "192.0.2.2".into(),
        verdict: Verdict::Pass,
        execution_status: ExecutionStatus::Completed,
        kind_label: "灌包-ab".into(),
        requested_streams: 1,
        active_streams: 1,
        required_streams: 1,
        rx_avg: Some(930.125),
        rx_p10: Some(900.5),
        target_mbps: Some(850.0),
        sample_coverage: Some(0.98),
        tx_mbps: Some(940.0),
        rx_mbps: Some(920.0),
        ..Default::default()
    }
}

/// 指定单元序号的汇总行；默认的 `unit_summary` 恒为第 0 个单元。
fn unit_summary_at(unit: &str, verdict: Verdict, seq: usize) -> Row {
    Row {
        sort_key: (seq, usize::MAX, usize::MAX, u8::MAX),
        ..unit_summary(unit, verdict)
    }
}

fn unit_summary(unit: &str, verdict: Verdict) -> Row {
    Row {
        sort_key: (0, usize::MAX, usize::MAX, u8::MAX),
        task_id: unit.into(),
        parent_id: unit.into(),
        task: "IPERF V4 TCP".into(),
        verdict,
        execution_status: if verdict == Verdict::SetupError {
            ExecutionStatus::Error
        } else {
            ExecutionStatus::Completed
        },
        kind_label: "UNIT_SUMMARY_SENTINEL".into(),
        is_unit_summary: true,
        ..Default::default()
    }
}

/// 判定用到的样本必须都能从报告里点得到——**包括 TX**。
///
/// TX 采样是**否决性**门槛：`rate_window_coverage_sufficient` 要求 TX 滚动覆盖率
/// ≥0.95 且 `tx.p10` 在，否则整行判 NOT_EVALUATED；`tx_sufficient` 还决定会不会
/// 报 `OFFERED_LOAD_LOW`。可是 iperf/CTS 两条路径过去**从不落盘 TX 逐样本**
/// （`save_monitor_samples` 只传 dst/RX），`Row.nic_samples` 也是单字段、装的
/// 永远是 RX。结果是：一行被判 NOT_EVALUATED，理由是「发送端覆盖率不够」，
/// 而那份发送端样本谁也拿不到——「报告里的每个结论都要能回到某一行样本」
/// （`artifact.rs` 模块头自己的话）对 TX 不成立。
#[test]
fn both_directions_of_nic_samples_are_reachable_from_the_report() {
    let mut row = traffic_detail("unit-a", (0, 0, 0, 0));
    row.raw_log = "raw/iperf.log".into();
    row.nic_samples_rx = "raw/nic_samples_rx.csv".into();
    row.nic_samples_tx = "raw/nic_samples_tx.csv".into();
    let html = render(vec![row, unit_summary("unit-a", Verdict::Pass)]);

    assert!(html.contains("接收端逐样本 CSV"), "RX 样本链接丢了");
    assert!(html.contains("发送端逐样本 CSV"), "TX 样本链接丢了");
    assert!(html.contains("nic_samples_tx.csv"), "TX 文件路径没进报告");
    // 文件计数要把 TX 算进去，否则标题说 2 个而实际给了 3 个链接。
    assert!(
        html.contains("3 个原始文件"),
        "原始文件计数没算上 TX: {}",
        html.lines()
            .find(|line| line.contains("个原始文件"))
            .unwrap_or("<找不到那一行>")
    );
}

/// 概览顶部的统计格必须覆盖 `Verdict` 的**每一个**取值。
///
/// 这条修的是一个会静默误导人的缺口：NOT_EVALUATED 与 SKIP 过去只出现在脚注的
/// 一行小字里，顶上没有格子。于是一轮**整轮 NOT_EVALUATED** 的报告（采样不可信、
/// 一条结论都不能下）顶部五格全是 0，看上去和「什么都没跑」一模一样。
///
/// 同一处还有两个自相矛盾：脚注写着「仅统计 PASS、RATE_FAIL、UNSTABLE」，而 8 行
/// 外的代码算的是 `judged = pass + rate_fail`（UNSTABLE 早就不产出了）；网格声明了
/// 7 列却只画 6 格。这条断言把「每个 verdict 都有自己的格子」变成结构约束——
/// 以后再加 verdict，漏了格子就在这里红，而不是等某一轮报告顶部全零才被发现。
#[test]
fn the_summary_grid_has_one_cell_for_every_verdict() {
    let rows = vec![
        unit_summary_at("unit-pass", Verdict::Pass, 0),
        unit_summary_at("unit-rate-fail", Verdict::RateFail, 1),
        unit_summary_at("unit-measured", Verdict::Measured, 2),
        unit_summary_at("unit-not-evaluated", Verdict::NotEvaluated, 3),
        unit_summary_at("unit-setup-error", Verdict::SetupError, 4),
        unit_summary_at("unit-skip", Verdict::Skip, 5),
    ];
    let html = render(rows);
    let start = html.find("<div class=\"summary-grid\">").expect("统计块");
    let end = html[start..]
        .find("</div>\n")
        .map(|o| start + o)
        .unwrap_or(html.len());
    let grid = &html[start..end];

    for verdict in [
        Verdict::Pass,
        Verdict::RateFail,
        Verdict::Measured,
        Verdict::NotEvaluated,
        Verdict::SetupError,
        Verdict::Skip,
    ] {
        assert!(
            grid.contains(&format!(
                "<span class=\"stat-label\">{}</span>",
                verdict.label()
            )),
            "{} 没有统计格：整轮都是这个判定时，顶部会全是 0",
            verdict.label()
        );
    }

    // 声明的列数要和真画出来的格数一致，否则最后一格会独自换行。
    let cells = grid.matches("<div class=\"stat ").count();
    let columns = html
        .find(".summary-grid { display: grid; grid-template-columns: repeat(")
        .map(|at| {
            let rest =
                &html[at + ".summary-grid { display: grid; grid-template-columns: repeat(".len()..];
            rest[..rest.find(',').expect("列数")]
                .parse::<usize>()
                .expect("列数是数字")
        })
        .expect("网格列数");
    assert_eq!(
        cells, columns,
        "统计格 {cells} 个，网格却声明了 {columns} 列"
    );

    // 脚注不许再宣称一个代码里根本不算的分母。
    assert!(
        !html.contains("仅统计 PASS、RATE_FAIL、UNSTABLE"),
        "脚注还在说 UNSTABLE 计入分母，而 judged = pass + rate_fail"
    );
    assert!(
        html.contains("分母只算 PASS 与 RATE_FAIL"),
        "脚注要说清通过率的分母是什么"
    );
}

/// 顶层区块要能折叠，原始输出要有序号，且有一次性全开/全关的入口。
///
/// 一轮 120 个单元的报告，「测试概览 / 逐行明细 / 原始输出」三块摊开是几屏，
/// 想只看结论就得一路滚。折叠用原生 `<details>`——脚本挂了也还能逐块手点，
/// 全局按钮只是省去点 120 次。
#[test]
fn report_sections_collapse_and_raw_outputs_are_numbered() {
    let mut first = traffic_detail("unit-a", (0, 0, 0, 0));
    first.raws = vec![("client".into(), "<output a>".into())];
    // 第二个单元的 sort_key.0 必须是 1：原始输出的 #N 是**单元序号**，
    // 两个 fixture 都留在 0 的话它们本来就该同号（见
    // `the_same_unit_carries_the_same_number_in_every_section`）。
    let mut second = traffic_detail("unit-b", (1, 0, 0, 0));
    second.raws = vec![("client".into(), "<output b>".into())];
    let html = render(vec![
        first,
        unit_summary_at("unit-a", Verdict::Pass, 0),
        second,
        unit_summary_at("unit-b", Verdict::Pass, 1),
    ]);

    // 三块顶层区块都包在可折叠的 <details> 里，标题本身就是开关。
    for heading in ["overview-heading", "details-heading", "raw-heading"] {
        let marker = format!("<h2 id=\"{heading}\"");
        let at = html.find(&marker).unwrap_or_else(|| panic!("缺 {heading}"));
        let before = &html[..at];
        assert!(
            before.rfind("<summary class=\"top-toggle\">").unwrap_or(0)
                > before.rfind("</summary>").unwrap_or(0),
            "{heading} 必须在 <summary class=\"top-toggle\"> 里，否则点标题不能折叠"
        );
    }
    // 默认展开：折叠是给「已经看过一遍、只想找某一块」的人用的，
    // 第一次打开就是收起状态会让人以为报告是空的。
    assert_eq!(
        html.matches("<details class=\"top-section\" open>").count(),
        3,
        "三块顶层区块都要默认展开"
    );

    // 原始输出逐条有序号，和「逐行明细」的 #N 是同一个读法。
    assert!(
        html.contains("<span class=\"raw-seq\">#1</span>"),
        "原始输出缺 #1"
    );
    assert!(
        html.contains("<span class=\"raw-seq\">#2</span>"),
        "原始输出缺 #2"
    );

    // 全局开关：两个按钮 + 一段不依赖外部资源的内嵌脚本。
    assert!(html.contains("data-toggle-all=\"open\">展开全部"));
    assert!(html.contains("data-toggle-all=\"close\">收起全部"));
    assert!(
        html.contains("<script>"),
        "报告要能离线单文件使用，脚本必须内嵌"
    );
    assert!(
        !html.contains("<script src="),
        "不能引外部脚本：报告是拷走整个目录离线看的"
    );
    // 交互控件不该出现在打印稿上。
    assert!(html.contains(".report-tools { display: none; }"));
}

/// 三块区块的 `#N` 必须是**同一个数**：单元执行序号（= 控制台的 `[N/总数]`）。
///
/// 这条守的是一个已经犯过的错：原始输出那一段列的是**执行行**不是单元，
/// 一个双向单元出两行，按「本段里排第几」编号的话，5 个单元能排到 #10——
/// 概览说 #4、明细说 #4、原始输出说 #7，指的却是同一件事。而这三块存在的
/// 全部意义，就是让人拿着一个号在它们之间来回对（还要对控制台和文件名）。
#[test]
fn the_same_unit_carries_the_same_number_in_every_section() {
    // 三个单元，其中第二个是双向（两条腿 → 原始输出会出两条）。
    let mut u1 = traffic_detail("unit-a", (0, 0, 0, 0));
    u1.raws = vec![("client".into(), "<a>".into())];
    let mut u2ab = traffic_detail("unit-b", (1, 0, 0, 0));
    u2ab.kind_label = "灌包-ab".into();
    u2ab.raws = vec![("client".into(), "<b-ab>".into())];
    let mut u2ba = traffic_detail("unit-b", (1, 1, 0, 0));
    u2ba.kind_label = "灌包-ba".into();
    u2ba.raws = vec![("client".into(), "<b-ba>".into())];
    let mut u3 = traffic_detail("unit-c", (2, 0, 0, 0));
    u3.raws = vec![("client".into(), "<c>".into())];

    let html = render(vec![
        u1,
        unit_summary_at("unit-a", Verdict::Pass, 0),
        u2ab,
        u2ba,
        unit_summary_at("unit-b", Verdict::Pass, 1),
        u3,
        unit_summary_at("unit-c", Verdict::Pass, 2),
    ]);

    // 第 3 个单元在「逐行明细」和「原始输出」里都是 #3。
    assert!(
        html.contains("<span class=\"unit-seq\">#3</span>"),
        "逐行明细里第三个单元应当是 #3"
    );
    assert!(
        html.contains("<span class=\"raw-seq\">#3</span>"),
        "原始输出里第三个单元也应当是 #3，而不是按本段位置排到 #4"
    );
    // 双向单元的两条腿共用 #2，靠方向区分。
    assert_eq!(
        html.matches("<span class=\"raw-seq\">#2</span>").count(),
        2,
        "双向单元两条腿共用单元号"
    );
    // 区分标是 `kind_label`——和「逐行明细」那张表「类型」列一字不差。
    // 一个双向单元每条腿还会分「流明细」和「组合计」，光标 AB/BA 会出现
    // 四条一模一样的标题。
    assert!(
        html.contains("<span class=\"raw-dir\">灌包-ab</span>"),
        "{html:.0}"
    );
    assert!(html.contains("<span class=\"raw-dir\">灌包-ba</span>"));
    // 原始输出一共 4 条执行行，但最大号只能到 3（单元数），不能到 4。
    assert!(
        !html.contains("<span class=\"raw-seq\">#4</span>"),
        "原始输出的号是单元号，不是行号"
    );
}

#[test]
fn report_renders_compact_accessible_structure_and_diagnostics() {
    let mut detail = traffic_detail("unit-a", (0, 0, 0, 0));
    detail.raw_log = "./iperf_outputs/iperf_tcp.log".into();
    detail.nic_samples_rx = "./iperf_outputs/nic.csv".into();
    detail.screenshot_master = "./iperf_outputs/master.png".into();
    detail.screenshot_agent = "./iperf_outputs/agent.png".into();
    detail.command = "iperf3 -c <target>".into();
    detail.peer_rx = "950.000 Mbps (BA)".into();
    detail.raws = vec![("client".into(), "<output>".into())];
    let html = render(vec![detail, unit_summary("unit-a", Verdict::Pass)]);

    assert!(html.contains("测试概览"));
    assert!(html.contains("逐行明细（1 行）"));
    assert!(html.contains("<caption class=\"sr-only\">"));
    assert!(html.contains("<th scope=\"col\">网卡 RX 平均</th>"));
    assert!(html.contains("role=\"region\""));
    assert!(html.contains("tabindex=\"0\""));
    assert!(html.contains("max-height: 68vh"));
    assert!(html.contains("overflow: auto"));
    assert!(html.contains("position: sticky"));
    assert!(html.contains("<details class=\"unit-section\""));
    // 工具自报速率是「流确实建立了」的唯一证据，且单条流明细行没有网卡数据，
    // 必须留在表内可见，不能只藏在折叠的诊断面板里。
    assert!(html.contains("<th scope=\"col\">流量工具自报（非判定口径）</th>"));
    assert!(html.contains("发 940.000 / 收 920.000 Mbps"));
    assert!(!html.contains("流量工具 sender 汇总（诊断）"));
    assert!(!html.contains("流量工具 receiver 汇总（诊断）"));
    assert_eq!(tool_rate_text(None, None), "未采集");
    // 网卡计数器按方向采，拆不到单条流上；流明细行必须写明去组合计行看，
    // 不能只留「未采集」让人以为这条流没测到。
    let mut bare = traffic_detail("unit-bare", (0, 0, 0, 0));
    bare.rx_avg = None;
    bare.rx_p10 = None;
    bare.sample_coverage = None;
    bare.tx_mbps = Some(500.0);
    bare.rx_mbps = Some(498.2);
    let bare_html = render(vec![bare, unit_summary("unit-bare", Verdict::Measured)]);
    assert!(bare_html.contains("—（按方向统计，见组合计行）"));
    assert!(bare_html.contains("发 500.000 / 收 498.200 Mbps"));
    assert_eq!(
        tool_rate_text(Some(500.0), None),
        "发 500.000 / 收 未采集 Mbps"
    );
    assert!(html.contains("950.000 Mbps (BA)"));
    assert!(html.contains("的诊断详情\"><span>诊断</span>"));
    for label in [
        "主控截图",
        "辅测截图",
        "灌包命令",
        "原始记录",
        "网卡样本",
        "内嵌原始输出（非空 1/1）",
    ] {
        assert!(html.contains(label), "missing diagnostic label: {label}");
    }
    assert!(html.contains("主控截图 · 查看原图"));
    assert!(html.contains("辅测截图 · 查看原图"));
    assert!(html.contains("实际灌包命令"));
    assert!(html.contains("独立原始记录（raw_log）"));
    assert!(html.contains("href=\"#raw-0-0-0-0\""));
    assert!(!html.contains("aria-label=\"展开"));
    assert!(!html.contains("后端发送 Mbps"));
    assert!(!html.contains("后端接收 Mbps"));
    let main_header = html
        .split("<table class=\"results-table\">")
        .nth(1)
        .unwrap()
        .split("</thead>")
        .next()
        .unwrap();
    assert!(!main_header.contains("流量工具 sender"));
    assert!(!main_header.contains("流量工具 receiver"));
    assert!(html.contains("&lt;output&gt;"));
    assert!(html.contains("iperf3 -c &lt;target&gt;"));
    assert!(html.contains("独立原始记录"));
    // RX/TX 两份样本必须能分辨：TX 覆盖率是否决性门槛（不够就整行 NOT_EVALUATED），
    // 只给一个笼统的「网卡逐样本 CSV」链接，看报告的人分不清判定依据是哪一份。
    assert!(html.contains("接收端逐样本 CSV"));
    assert!(html.contains("原始输出（1 条执行记录，3 项内容，内嵌文本非空 1 段）"));
    assert!(html.contains("1 段内嵌输出（非空 1） · 2 个原始文件"));
    assert!(html.contains(".raw-section, .row-diagnostics { display: block; }"));
    assert!(html.contains(".shot { display: inline-flex; }"));
}

#[test]
fn test_report_counts_units_instead_of_flow_details() {
    let mut rows: Vec<Row> = (0..20)
        .map(|idx| {
            let mut row = traffic_detail("udp-unit", (0, 0, idx, 0));
            row.task_id = format!("flow-{idx}");
            row.task = "IPERF V4 UDP".into();
            row.transport = "UDP".into();
            row
        })
        .collect();
    let mut summary = unit_summary("udp-unit", Verdict::Pass);
    summary.task = "IPERF V4 UDP".into();
    rows.push(summary);
    let html = render(rows);

    assert!(html.contains("测试单元: 1"));
    assert!(html
        .contains("<span class=\"stat-label\">PASS</span><strong class=\"stat-value\">1</strong>"));
    assert!(html.contains("逐行明细（20 行）"));
    assert!(html.contains("20 条 UDP 流执行行"));
    assert!(!html.contains("测试单元: 21"));
}

#[test]
fn legacy_group_without_summary_matches_executor_verdict_priority() {
    let mut rate_fail = traffic_detail("legacy-unit", (0, 0, 0, 0));
    rate_fail.verdict = Verdict::RateFail;
    let mut not_evaluated = traffic_detail("legacy-unit", (0, 0, 1, 0));
    not_evaluated.verdict = Verdict::NotEvaluated;

    let rows = vec![rate_fail, not_evaluated];
    let groups = group_rows(&rows);
    assert_eq!(groups.len(), 1);
    assert_eq!(group_verdict(&groups[0]), Verdict::NotEvaluated);
}

#[test]
fn legacy_group_without_summary_keeps_single_udp_hard_failure_visible() {
    for code in [
        ReasonCode::SingleUdpStreamFailed,
        ReasonCode::CtsSingleUdpStreamFailed,
    ] {
        let mut hard_failure = traffic_detail("legacy-hard", (0, 0, 0, 0));
        hard_failure.verdict = Verdict::RateFail;
        hard_failure.reason_code = code;
        let mut not_evaluated = traffic_detail("legacy-hard", (0, 1, 0, 0));
        not_evaluated.verdict = Verdict::NotEvaluated;
        not_evaluated.reason_code = ReasonCode::SampleCoverageLow;

        let rows = vec![hard_failure, not_evaluated];
        let groups = group_rows(&rows);
        // 必须灌通的方向硬失败不能被另一腿普通的 NOT_EVALUATED 掩盖。
        assert_eq!(group_verdict(&groups[0]), Verdict::RateFail, "code={code}");
    }

    // SetupError 仍然优先于硬失败，与 executor 的聚合顺序一致。
    let mut hard_failure = traffic_detail("legacy-setup", (0, 0, 0, 0));
    hard_failure.verdict = Verdict::RateFail;
    hard_failure.reason_code = ReasonCode::SingleUdpStreamFailed;
    let mut setup_error = traffic_detail("legacy-setup", (0, 1, 0, 0));
    setup_error.verdict = Verdict::SetupError;
    let rows = vec![hard_failure, setup_error];
    let groups = group_rows(&rows);
    assert_eq!(group_verdict(&groups[0]), Verdict::SetupError);
}

#[test]
fn overview_keeps_scenario_columns_and_screenshots_reachable_without_page_scroll() {
    let mut summary = unit_summary("unit-shot", Verdict::Pass);
    summary.task = "★双向 IPERF V4 UDP -b 500m".into();
    summary.direction_summaries = vec![
        DirectionSummary {
            tag: "AB".into(),
            src: "master/eth0".into(),
            dst: "agent/eth1".into(),
            verdict: Verdict::Pass,
            rx_avg: Some(8500.0),
            rx_p10: Some(8450.0),
            target_mbps: Some(8400.0),
            screenshot_master: "./iperf_outputs/ab_master.png".into(),
            screenshot_agent: "./iperf_outputs/ab_agent.png".into(),
            ..Default::default()
        },
        DirectionSummary {
            tag: "BA".into(),
            src: "agent/eth1".into(),
            dst: "master/eth0".into(),
            verdict: Verdict::Pass,
            rx_avg: Some(6500.0),
            rx_p10: Some(6450.0),
            target_mbps: Some(6400.0),
            screenshot_master: "./iperf_outputs/ba_master.png".into(),
            ..Default::default()
        },
    ];
    let html = render(vec![summary]);

    // 横向滚动条留在视口内，而不是被推到整页最底部。
    assert!(html.contains(".overview-scroll { max-width: 100%; max-height: 72vh; overflow: auto;"));
    // 左侧「序号 / 结果 / 测试单元 / 方向」四列冻结，右拖看速率时仍知道是哪个测试项。
    assert!(html.contains(".overview-table th:nth-child(-n+4), .overview-table tr:not(.reason-row) > td:nth-child(-n+4) { position: sticky;"));
    // 冻结偏移必须与 colgroup 列宽一致：48 + 116 + 250 = 414。
    assert!(html.contains(
        ".overview-table th:nth-child(4), .overview-table tr:not(.reason-row) > td:nth-child(4) { left: 414px;"
    ));
    // 基准 1432px 下 3.352% = 48px、8.101% = 116px、17.459% = 250px。
    assert!(html.contains(".overview-table { min-width: 1432px; table-layout: fixed; }"));
    assert!(html.contains(".overview-table col.c-seq { width: 3.352%; }"));
    assert!(html.contains(".overview-table col.c-verdict { width: 8.101%; }"));
    assert!(html.contains(".overview-table col.c-unit { width: 17.459%; }"));
    // 屏幕装不下时改为等比压缩，不让用户去页面底部找横向滚动条。
    assert!(html.contains("@media (max-width: 1460px)"));
    // 窄列里最长的结果标签必须能在下划线处折行，不能溢出盖住相邻列。
    assert_eq!(
        status_label_html(Verdict::NotEvaluated),
        "NOT_<wbr>EVALUATED"
    );
    assert_eq!(status_label_html(Verdict::SetupError), "SETUP_<wbr>ERROR");
    assert_eq!(status_label_html(Verdict::Pass), "PASS");
    assert!(html.contains(
        ".overview-table tr:not(.reason-row) > td:nth-child(2) .status, .unit-verdict-note .status { white-space: normal; overflow-wrap: normal;"
    ));
    // 两张缩略图必须并排，换行堆叠会把行高翻倍。
    assert!(html.contains(".shot-cell { display: flex; flex-wrap: nowrap;"));
    assert!(html.contains(".shot-mini img { display: block; width: 100%; max-width: 80px;"));
    assert!(html.contains("<th scope=\"col\">接收端 RX 平均</th>"));
    // 判定原因独占整行，不再挤在定宽列里被截断。
    assert!(!html.contains("<th scope=\"col\">原因</th>"));
    assert!(html.contains("<tr class=\"reason-row\""));
    assert!(html.contains("colspan=\"12\""));
    // 截图列与接收速率同排，不必展开诊断面板。
    assert!(html.contains("<th scope=\"col\">截图</th>"));
    assert!(html.contains("<col class=\"c-shot\">"));
    for path in [
        "./iperf_outputs/ab_master.png",
        "./iperf_outputs/ab_agent.png",
        "./iperf_outputs/ba_master.png",
    ] {
        assert!(html.contains(path), "missing overview screenshot: {path}");
    }
    // BA 只有主控截图时，辅测不能渲染成空缩略图。
    assert_eq!(html.matches("class=\"shot-mini\"").count(), 6);
}

/// 概览和明细都必须带执行序号，且与控制台打印的 `[N/总数]` 一致。
///
/// 120 个单元里「主控 以太网 6 -> 辅测 以太网」这类标题会重复十几次，
/// 只有标题的话，拿着控制台记录去报告里找对应项根本定位不到。
#[test]
fn every_unit_carries_the_console_sequence_number() {
    let mut first = unit_summary("unit-a", Verdict::Measured);
    first.sort_key = (0, usize::MAX, usize::MAX, u8::MAX);
    first.task = "IPERF V4 TCP | 主控 以太网 6 -> 辅测 以太网".into();
    let mut later = unit_summary("unit-b", Verdict::Measured);
    later.sort_key = (113, usize::MAX, usize::MAX, u8::MAX);
    later.task = "IPERF V4 TCP | 主控 以太网 6 -> 辅测 以太网".into();

    let html = render(vec![first, later]);

    // 序号从 1 开始，与控制台的 [1/120] / [114/120] 对齐。
    assert!(html.contains("<th scope=\"col\">#</th>"));
    assert!(html.contains(">1</td>"), "第一个单元应显示 1");
    assert!(
        html.contains(">114</td>"),
        "sort_key.0=113 的单元应显示 114"
    );
    // 明细区用同一个数，两个区之间才对得上。
    assert!(html.contains("<span class=\"unit-seq\">#1</span>"));
    assert!(html.contains("<span class=\"unit-seq\">#114</span>"));
}

#[test]
fn overview_drops_the_screenshot_column_when_nothing_was_captured() {
    let mut summary = unit_summary("unit-noshot", Verdict::Pass);
    summary.task = "IPERF V4 UDP".into();
    summary.direction_summaries = vec![DirectionSummary {
        tag: "单向".into(),
        src: "master/eth0".into(),
        dst: "agent/eth1".into(),
        verdict: Verdict::Measured,
        reason_code: ReasonCode::TargetUnknown,
        reason_detail: "Observe 模式仅记录实际能力".into(),
        rx_avg: Some(940.0),
        ..Default::default()
    }];
    let html = render(vec![summary]);

    // 关掉截图时整列都会是「未采集」，白占约 190px，应该整列不渲染。
    assert!(!html.contains("<th scope=\"col\">截图</th>"));
    assert!(!html.contains("<col class=\"c-shot\">"));
    // 序号列让整表多一列，原因行的 colspan 必须跟着走。
    assert!(html.contains("colspan=\"11\""));
    assert!(html.contains("TARGET_UNKNOWN: Observe 模式仅记录实际能力"));
}

#[test]
fn diagnostics_expose_the_window_baseline_and_rolling_coverage_used_for_judgement() {
    let mut detail = traffic_detail("unit-window", (0, 0, 0, 0));
    detail.is_grouptotal = true;
    detail.window_start_ms = Some(8_000);
    detail.window_end_ms = Some(188_000);
    detail.baseline_mbps = Some(12.5);
    detail.rolling_coverage = Some(0.978);
    detail.effective_seconds = Some(180.0);
    detail.required_seconds = Some(180.0);
    let html = render(vec![detail, unit_summary("unit-window", Verdict::Pass)]);

    // 判定窗口的两个端点必须能直接对到网卡逐样本 CSV 的 elapsed_ms 列。
    assert!(html.contains("判定窗口区间"));
    assert!(html.contains("8000 ms – 188000 ms（对应网卡样本 elapsed_ms）"));
    // 验收要求核对「原始总流量 − 业务流量 ≈ 背景值」，扣除量必须报出来。
    assert!(html.contains("12.500 Mbps（起流前空闲期中位数）"));
    // 滚动窗口覆盖率与总采样覆盖率是两个门槛，不能只报一个。
    assert!(html.contains("5 秒滚动窗口覆盖率"));
    assert!(html.contains("97.8%"));
}

#[test]
fn transport_column_names_the_backend_and_warns_when_both_are_mixed() {
    assert_eq!(transport_display("UDP"), "iperf3 UDP");
    assert_eq!(transport_display("TCP"), "iperf3 TCP");
    assert_eq!(transport_display("CTS/UDP"), "ctsTraffic UDP");
    assert_eq!(transport_display("CTS/TCP"), "ctsTraffic TCP");
    assert_eq!(transport_display(""), "");

    // 只有 iperf3 时不必提示口径差异。
    let only_iperf = traffic_detail("unit-iperf", (0, 0, 0, 0));
    let html = render(vec![only_iperf, unit_summary("unit-iperf", Verdict::Pass)]);
    assert!(html.contains("iperf3 TCP"));
    assert!(!html.contains("二者的 UDP 语义不等价"));

    // 两种后端同时出现时必须写明不可直接互比。
    let mut iperf = traffic_detail("unit-mix", (0, 0, 0, 0));
    iperf.transport = "UDP".into();
    let mut cts = traffic_detail("unit-mix", (0, 0, 1, 0));
    cts.transport = "CTS/UDP".into();
    let mixed = render(vec![iperf, cts, unit_summary("unit-mix", Verdict::Pass)]);
    assert!(mixed.contains("iperf3 UDP"));
    assert!(mixed.contains("ctsTraffic UDP"));
    assert!(mixed.contains("不应直接互比"));
}

#[test]
fn sampling_caveat_is_shown_only_when_the_platform_actually_differs() {
    let detail = traffic_detail("unit-caveat", (0, 0, 0, 0));
    let quiet = render_with_meta(
        vec![detail.clone(), unit_summary("unit-caveat", Verdict::Pass)],
        &ReportMeta::default(),
    );
    assert!(!quiet.contains("采样口径提示"));

    let noisy = render_with_meta(
        vec![detail, unit_summary("unit-caveat", Verdict::Pass)],
        &ReportMeta {
            counter_source_caveat: "本机为 macOS：网卡计数器经由 netstat 子进程逐次采样".into(),
            ..Default::default()
        },
    );
    assert!(noisy.contains("采样口径提示"));
    assert!(noisy.contains("netstat 子进程"));
}

#[test]
fn screenshot_thumbnail_and_text_link_to_original_image() {
    let html = screenshot_link("./iperf_outputs/shot&1.png", "主控截图");

    assert!(html.contains("<figure class=\"shot\">"));
    assert!(html.contains("<a href=\"./iperf_outputs/shot&amp;1.png\""));
    assert!(html.contains("<img src=\"./iperf_outputs/shot&amp;1.png\""));
    assert!(html.contains("target=\"_blank\""));
    assert!(html.contains("rel=\"noopener\""));
    assert!(html.contains("title=\"查看主控截图\""));
    assert!(html.contains("主控截图 · 查看原图"));
    assert_eq!(html.matches("./iperf_outputs/shot&amp;1.png").count(), 2);
    assert!(screenshot_link("", "主控截图").is_empty());
}

#[test]
fn bidirectional_overview_keeps_ab_and_ba_separate() {
    let mut summary = unit_summary("unit-bidir", Verdict::RateFail);
    summary.task = "双向 TCP".into();
    summary.direction_summaries = vec![
        DirectionSummary {
            tag: "AB".into(),
            src: "master/eth0".into(),
            dst: "agent/eth1".into(),
            verdict: Verdict::Pass,
            streams: Some(StreamCounts {
                requested: 1,
                active: 1,
                required: 1,
            }),
            rx_avg: Some(900.0),
            rx_p10: Some(880.0),
            target_mbps: Some(850.0),
            sample_coverage: Some(1.0),
            ..Default::default()
        },
        DirectionSummary {
            tag: "BA".into(),
            src: "agent/eth1".into(),
            dst: "master/eth0".into(),
            verdict: Verdict::RateFail,
            reason: "RX_P10_BELOW_TARGET".into(),
            streams: Some(StreamCounts {
                requested: 1,
                active: 1,
                required: 1,
            }),
            rx_avg: Some(800.0),
            rx_p10: Some(760.0),
            target_mbps: Some(850.0),
            sample_coverage: Some(0.99),
            ..Default::default()
        },
    ];
    let html = render(vec![summary]);

    assert!(html.contains("data-direction=\"AB\""));
    assert!(html.contains("data-direction=\"BA\""));
    assert!(html.contains("900.000 Mbps"));
    assert!(html.contains("800.000 Mbps"));
    assert!(html.contains("RX_P10_BELOW_TARGET"));
    assert!(html.contains("双向方向汇总"));
    assert!(html.contains("2 个方向执行行（AB / BA）"));
    // 双向只按各自方向的接收端速率判定；任何形式的 AB+BA 相加都会被误读成
    // 整机吞吐，尤其是一个方向未评价时。
    assert!(!html.contains("AB + BA"));
    assert!(!html.contains("RX 平均合计"));
    assert!(!html.contains("1700.000 Mbps"));
    assert!(!html.contains("1640.000 Mbps"));
    assert!(html.contains("data-unit-id=\"unit-bidir\" open"));
}

#[test]
fn passing_direction_does_not_inherit_failure_reason_from_other_direction() {
    let mut summary = unit_summary("unit-mixed", Verdict::RateFail);
    summary.task = "双向 TCP".into();
    summary.reason_code = ReasonCode::RxBelowTarget;
    summary.reason_detail = "BA: RX 平均 790.000 Mbps 低于目标 800.000 Mbps".into();
    summary.direction_summaries = vec![
        DirectionSummary {
            tag: "AB".into(),
            verdict: Verdict::Pass,
            rx_avg: Some(875.0),
            rx_p10: Some(850.0),
            target_mbps: Some(800.0),
            ..Default::default()
        },
        DirectionSummary {
            tag: "BA".into(),
            verdict: Verdict::RateFail,
            reason_code: ReasonCode::RxBelowTarget,
            reason_detail: "RX 平均 790.000 Mbps 低于目标 800.000 Mbps".into(),
            rx_avg: Some(790.0),
            rx_p10: Some(770.0),
            target_mbps: Some(800.0),
            ..Default::default()
        },
    ];
    let html = render(vec![summary]);

    assert!(html.contains(
        "RX_TARGET_MET: RX 平均 875.000 Mbps、RX-P10 850.000 Mbps 均不低于目标 800.000 Mbps"
    ));
    assert!(html.contains("RX_BELOW_TARGET: RX 平均 790.000 Mbps &lt; 目标 800.000 Mbps"));
    assert!(!html.contains("RX 平均 875.000 Mbps &gt;= 目标 800.000 Mbps"));
}

#[test]
fn bidirectional_udp_meta_explains_flow_and_group_rows() {
    let mut rows = Vec::new();
    for (leg, tag) in [(0, "AB"), (1, "BA")] {
        for stream in 0..2 {
            let mut row = traffic_detail("udp-bidir-meta", (0, leg, stream, 0));
            row.task = "双向 UDP".into();
            row.transport = "UDP".into();
            row.kind_label = format!("灌包-{tag}");
            rows.push(row);
        }
        let mut total = traffic_detail("udp-bidir-meta", (0, leg, 2, 1));
        total.task = "双向 UDP".into();
        total.transport = "UDP".into();
        total.kind_label = format!("★组合计-{tag}");
        total.is_grouptotal = true;
        rows.push(total);
    }
    let mut summary = unit_summary("udp-bidir-meta", Verdict::Pass);
    summary.task = "双向 UDP".into();
    summary.direction_summaries = vec![
        DirectionSummary {
            tag: "AB".into(),
            verdict: Verdict::Pass,
            ..Default::default()
        },
        DirectionSummary {
            tag: "BA".into(),
            verdict: Verdict::Pass,
            ..Default::default()
        },
    ];
    rows.push(summary);
    let html = render(rows);

    assert!(html.contains("2 个方向 · 4 条 UDP 流明细 · 2 条方向组合计"));
}

#[test]
fn missing_nic_rx_is_not_filled_from_tool_receiver() {
    let mut summary = unit_summary("unit-no-nic", Verdict::NotEvaluated);
    summary.rx_mbps = Some(777.777);
    let html = render(vec![summary]);

    assert!(html.contains(NOT_COLLECTED));
    assert!(!html.contains("777.777"));
}

#[test]
fn ping_uses_not_applicable_instead_of_zero_stream_counts() {
    let detail = Row {
        sort_key: (0, 0, 0, 0),
        time: "2026-07-04 12:00:00".into(),
        task_id: "ping-flow".into(),
        parent_id: "ping-unit".into(),
        task: "PING V4".into(),
        verdict: Verdict::Pass,
        execution_status: ExecutionStatus::Completed,
        kind_label: "PING".into(),
        ping_loss: Some(0.0),
        ping_min: Some(1.25),
        ping_avg: Some(2.5),
        ping_max: Some(4.75),
        ..Default::default()
    };
    let mut summary = unit_summary("ping-unit", Verdict::Pass);
    summary.task = "PING V4".into();
    let html = render(vec![detail, summary]);

    assert!(html.contains(NOT_APPLICABLE));
    assert!(html.contains("丢包率 0.0%"));
    assert!(html.contains("RTT 最小/平均/最大 1.250/2.500/4.750 ms"));
    assert!(html.contains("PING_OK:"));
    assert!(html.contains("1 条 Ping 执行行"));
    assert!(!html.contains("0/0/0"));
}

#[test]
fn unit_summary_is_excluded_from_detail_rows_and_raw_output() {
    let detail = traffic_detail("unit-exclude", (0, 0, 0, 0));
    let mut summary = unit_summary("unit-exclude", Verdict::Pass);
    summary.raw_log = "summary-raw-must-not-render.log".into();
    summary.raws = vec![("summary".into(), "summary raw".into())];
    let html = render(vec![detail, summary]);

    assert!(html.contains("逐行明细（1 行）"));
    assert_eq!(html.matches("data-detail-row=\"true\"").count(), 1);
    assert!(!html.contains("UNIT_SUMMARY_SENTINEL"));
    assert!(!html.contains("summary-raw-must-not-render.log"));
    assert!(!html.contains("summary raw"));
}

#[test]
fn actionable_failures_open_by_default_but_pass_measured_and_skip_do_not() {
    let mut fail = unit_summary("unit-fail", Verdict::SetupError);
    fail.sort_key.0 = 1;
    let mut measured = unit_summary("unit-measured", Verdict::Measured);
    measured.sort_key.0 = 2;
    let mut skipped = unit_summary("unit-skip", Verdict::Skip);
    skipped.sort_key.0 = 3;
    let html = render(vec![
        unit_summary("unit-pass", Verdict::Pass),
        fail,
        measured,
        skipped,
    ]);

    assert!(html.contains("data-unit-id=\"unit-fail\" open"));
    assert!(html.contains("data-unit-id=\"unit-pass\"><summary"));
    assert!(!html.contains("data-unit-id=\"unit-pass\" open"));
    assert!(!html.contains("data-unit-id=\"unit-measured\" open"));
    assert!(!html.contains("data-unit-id=\"unit-skip\" open"));
}

#[test]
fn raw_text_and_artifact_paths_are_escaped() {
    let mut detail = traffic_detail("escape-unit", (0, 0, 0, 0));
    detail.raw_log = "./iperf_outputs/a&b.log".into();
    detail.raws = vec![("client<&".into(), "<output>&".into())];
    let html = render(vec![detail, unit_summary("escape-unit", Verdict::Pass)]);

    assert!(html.contains("a&amp;b.log"));
    assert!(html.contains("client&lt;&amp;"));
    assert!(html.contains("&lt;output&gt;&amp;"));
}

#[test]
fn summary_with_explicit_direction_can_render_all_primary_metrics() {
    let mut summary = unit_summary("metrics-unit", Verdict::Pass);
    summary.direction_summaries = vec![DirectionSummary {
        tag: "AB".into(),
        src: "master".into(),
        dst: "agent".into(),
        verdict: Verdict::Pass,
        reason: "OK".into(),
        streams: Some(StreamCounts {
            requested: 4,
            active: 4,
            required: 3,
        }),
        rx_avg: Some(2379.123456),
        rx_p10: Some(2300.0),
        target_mbps: Some(2200.0),
        sample_coverage: Some(0.975),
        udp_loss: Some(0.125),
        ..Default::default()
    }];
    let html = render(vec![summary]);

    assert!(html.contains("4/4/3"));
    assert!(html.contains("2379.123 Mbps"));
    assert!(html.contains("2300.000 Mbps"));
    assert!(html.contains("2200.000 Mbps"));
    assert!(html.contains("97.5%"));
    assert!(html.contains("UDP 丢包 0.125%"));
}

#[test]
fn fallback_directions_prefer_group_totals_and_keep_both_legs() {
    let mut ab = traffic_detail("fallback-bidir", (0, 0, 0, 0));
    ab.kind_label = "★★双向灌包-ab".into();
    ab.rx_avg = Some(100.0);
    let mut ab_total = traffic_detail("fallback-bidir", (0, 0, 2, 1));
    ab_total.kind_label = "★组合计-ab".into();
    ab_total.is_grouptotal = true;
    ab_total.rx_avg = Some(200.0);
    let mut ba = traffic_detail("fallback-bidir", (0, 1, 0, 0));
    ba.kind_label = "★★双向灌包-ba".into();
    ba.rx_avg = Some(300.0);
    let mut summary = unit_summary("fallback-bidir", Verdict::Pass);
    summary.task = "双向 TCP".into();
    let html = render(vec![ab, ab_total, ba, summary]);

    assert!(html.contains("data-direction=\"AB\""));
    assert!(html.contains("data-direction=\"BA\""));
    assert!(html.contains("200.000 Mbps"));
    assert!(html.contains("300.000 Mbps"));
}

#[test]
fn rate_reason_validation_uses_the_metrics_shown_to_the_user() {
    assert_eq!(
        validate_rate_reason(
            "RX_BELOW_TARGET: stale",
            Some(799.0),
            Some(790.0),
            Some(800.0)
        ),
        "RX_BELOW_TARGET: RX 平均 799.000 Mbps < 目标 800.000 Mbps"
    );
    assert_eq!(
        validate_rate_reason(
            "RX_P10_BELOW_TARGET: stale",
            Some(875.0),
            Some(850.0),
            Some(800.0),
        ),
        "判定原因与展示指标不一致: RX_P10_BELOW_TARGET；RX-P10 850.000 Mbps >= 目标 800.000 Mbps"
    );
    assert_eq!(
        validate_rate_reason(
            "RX_UNSTABLE: stale",
            Some(875.0),
            Some(790.0),
            Some(800.0),
        ),
        "RX_UNSTABLE: RX 平均 875.000 Mbps >= 目标 800.000 Mbps，RX-P10 790.000 Mbps < 目标 800.000 Mbps"
    );
}

#[test]
fn contradictory_direction_reason_is_flagged_in_overview_and_bidir_summary() {
    let mut summary = unit_summary("contradictory-bidir", Verdict::RateFail);
    summary.task = "IPERF V4 TCP 双向".into();
    summary.direction_summaries = vec![
        DirectionSummary {
            tag: "AB".into(),
            verdict: Verdict::Pass,
            rx_avg: Some(860.0),
            rx_p10: Some(830.0),
            target_mbps: Some(800.0),
            ..Default::default()
        },
        DirectionSummary {
            tag: "BA".into(),
            verdict: Verdict::RateFail,
            reason_code: ReasonCode::RxP10BelowTarget,
            reason_detail: "旧汇总原因".into(),
            rx_avg: Some(875.0),
            rx_p10: Some(850.0),
            target_mbps: Some(800.0),
            ..Default::default()
        },
    ];

    let html = render(vec![summary]);

    assert!(html.contains("判定原因与展示指标不一致"));
    assert!(html.contains("RX-P10 850.000 Mbps &gt;= 目标 800.000 Mbps"));
    assert!(!html.contains("RX_P10_BELOW_TARGET: 旧汇总原因"));
}

#[test]
fn explicit_ping_direction_merges_min_avg_and_max_from_detail_row() {
    let detail = Row {
        sort_key: (0, 0, 0, 0),
        parent_id: "ping-merge".into(),
        task: "PING V6".into(),
        kind_label: "PING".into(),
        verdict: Verdict::Pass,
        ping_loss: Some(0.0),
        ping_min: Some(0.75),
        ping_avg: Some(1.5),
        ping_max: Some(3.25),
        ..Default::default()
    };
    let mut summary = unit_summary("ping-merge", Verdict::Pass);
    summary.task = "PING V6".into();
    summary.direction_summaries = vec![DirectionSummary {
        tag: "单向".into(),
        verdict: Verdict::Pass,
        ping_loss: Some(0.0),
        ..Default::default()
    }];

    let html = render(vec![detail, summary]);

    assert!(html.contains("RTT 最小/平均/最大 0.750/1.500/3.250 ms"));
    assert!(html.contains("PING_OK: 丢包率 0.0%"));
}

#[test]
fn raw_output_section_reports_empty_state_instead_of_looking_blank() {
    let html = render(vec![unit_summary("no-raw", Verdict::Pass)]);

    assert!(html.contains("原始输出（0 条执行记录，0 项内容，内嵌文本非空 0 段）"));
    assert!(html.contains("本次报告没有可用的内嵌原始输出、独立原始记录或网卡样本文件。"));
}

/// `-w` 的合法写法不止 `4m`：下发命令用的解析器接受 `2.5m` / `4mb` /
/// `1gib`，报告这边必须用同一个，否则校验放行、命令照发，只有报告里的
/// 「估算在途缓冲」无声消失。
#[test]
fn the_in_flight_estimate_accepts_every_socket_buffer_spelling_the_command_does() {
    let row = |command: &str| Row {
        command: command.into(),
        required_seconds: Some(180.0),
        ..Default::default()
    };
    for window in ["4m", "2.5m", "4mb", "1gib", "64k"] {
        let command = format!("iperf3 -c 10.0.0.2 -w {window} -P 4 -t 180");
        assert!(
            in_flight_buffer_estimate(&row(&command)).is_some(),
            "-w {window} 能下发就必须能估算"
        );
    }
    // 解析不了的写法保持沉默，不要报一个假数。
    assert!(in_flight_buffer_estimate(&row("iperf3 -c 10.0.0.2 -w 猫 -P 4")).is_none());
    assert!(in_flight_buffer_estimate(&row("iperf3 -c 10.0.0.2 -P 4")).is_none());
}

/// 报告固定按 Ping / 灌包性能(UDP、TCP) 分节，**没跑的分类整个不出现**。
///
/// 「这次没跑 TCP」和「这次 TCP 全挂了」必须一眼分得开：留一个空标题会让人
/// 以为报告漏了东西，而真的全挂时必须显眼。
#[test]
fn the_report_is_sectioned_by_protocol_and_hides_empty_sections() {
    let mut udp = traffic_detail("u-udp", (1, 0, 0, 0));
    udp.task = "IPERF V4 UDP -b 1000m".into();
    udp.transport = "UDP".into();

    let mut ping = traffic_detail("u-ping", (2, 0, 0, 0));
    ping.task = "PING 192.0.2.2".into();
    ping.kind_label = "Ping".into();
    ping.transport = String::new();
    ping.ping_loss = Some(0.0);

    // 只有 UDP + Ping：TCP 这一节整个不该出现。
    let html = render(vec![udp.clone(), ping.clone()]);
    assert!(html.contains("灌包性能 · UDP"), "缺 UDP 分节");
    assert!(html.contains("Ping（1 个单元）"), "缺 Ping 分节");
    assert!(
        !html.contains("灌包性能 · TCP"),
        "本次没跑 TCP，不该留一个空标题"
    );

    // 补一条 TCP 进去，三节都要在。
    let html = render(vec![udp, ping, traffic_detail("u-tcp", (3, 0, 0, 0))]);
    for title in ["Ping", "灌包性能 · UDP", "灌包性能 · TCP"] {
        assert!(html.contains(title), "缺分节 {title}");
    }
    // 分节顺序固定：Ping 在最前，UDP 在 TCP 之前。
    let ping_at = html.find("overview-ping").expect("Ping 锚点");
    let udp_at = html.find("overview-udp").expect("UDP 锚点");
    let tcp_at = html.find("overview-tcp").expect("TCP 锚点");
    assert!(ping_at < udp_at && udp_at < tcp_at, "分节顺序不对");
}

/// ctsTraffic 不单独占一层：它是 TCP/UDP 的执行引擎，按**协议**归类。
#[test]
fn ctstraffic_is_grouped_by_protocol_not_by_tool() {
    let mut cts = traffic_detail("u-cts", (1, 0, 0, 0));
    cts.task = "CTS Traffic 灌包".into();
    cts.transport = "TCP".into();
    cts.kind_label = "CTS Traffic 灌包".into();

    let html = render(vec![cts]);
    assert!(html.contains("灌包性能 · TCP"), "ctsTraffic TCP 应归到 TCP");
    assert!(
        !html.contains("灌包性能 · ctsTraffic"),
        "ctsTraffic 不该自成一节"
    );
}

/// 分节后每节的序号都从 0 重新起，DOM id 不能再用它，否则跨节撞车。
#[test]
fn unit_toggle_ids_stay_unique_across_sections() {
    let mut udp = traffic_detail("u-udp", (1, 0, 0, 0));
    udp.task = "IPERF V4 UDP".into();
    udp.transport = "UDP".into();
    let html = render(vec![udp, traffic_detail("u-tcp", (2, 0, 0, 0))]);

    let ids: Vec<&str> = html
        .match_indices("id=\"unit-toggle-")
        .map(|(at, _)| {
            let rest = &html[at + "id=\"unit-toggle-".len()..];
            &rest[..rest.find('"').expect("id 结尾")]
        })
        .collect();
    assert_eq!(ids.len(), 2, "两个单元两个 id");
    assert_ne!(ids[0], ids[1], "跨节的 unit-toggle id 撞了: {ids:?}");
}
