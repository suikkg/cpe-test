//! 结果的落盘与读回：`runs/<run>/rows.jsonl` + `runs/<run>/meta.json`。
//!
//! # 为什么存在
//!
//! 在此之前，一轮测试的全部明细活在 `Ctx.rows` 这个内存 `Vec<Row>` 里，
//! 直到整轮结束才由 `write_report` 一次性落盘。一次 11.5 小时的灌包测试，
//! 主控在第 10 小时崩溃/断电/被 kill，剩下的只有 `task_results.json` 里的
//! **单元级 PASS 布尔**——十小时的测量数据、原因码、方向明细、逐样本 CSV 的
//! 引用全部蒸发。这是这个工具最大的单点风险（ADR-3）。
//!
//! 处置是最朴素的那种：**每个单元跑完就把它新增的行追加写进 JSONL**，
//! 报告改成可以从落盘数据重放。于是崩溃的损失从「整轮」变成「未完成的那些单元」。
//!
//! # 为什么是 JSONL 而不是数据库
//!
//! - 单 exe、运行期零第三方运行时是这个产品的硬约束，引数据库直接违反它；
//! - 这里没有任何查询需求，只有「顺序写、顺序读」；
//! - **追加写对原子性的要求极低**：进程在写一行的中途死掉，最多丢/损坏最后
//!   一行，也就是一个单元。读回时跳过解析失败的行即可（见 [`load_rows`]）。
//!   换成一个需要事务的存储，反而要处理「事务没提交所以整批丢」。
//!
//! # 兼容面
//!
//! 落盘的 JSON 字段名就是兼容面：改 `Row` 的字段名 = 旧的 run 目录读不回来。
//! `meta.json` 里写了 `schema_version`，重放器对未知字段宽容（`Row` 上是
//! `#[serde(default)]`），这样新版本能读旧数据、旧版本读新数据也只是缺字段。
use super::{ReportMeta, Row};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// rows.jsonl / meta.json 的结构版本。
///
/// 只在**不兼容**的形状变更时 +1（比如把 `Row` 拆成两种记录）。加字段不算：
/// `Row` 是 `#[serde(default)]`，旧文件缺的字段取默认值。
pub const SCHEMA_VERSION: u32 = 1;

pub const ROWS_FILE: &str = "rows.jsonl";
pub const META_FILE: &str = "meta.json";
/// 控制台发起这一轮时用的 `RunRequest` 原文。
///
/// `meta.json` 里只有 `plan_hash`——那是摘要，反推不出计划。存原文是为了让
/// 「把这一轮再跑一遍」成立：没有它，历史运行只能看不能复现，而「同一份计划
/// 隔天复测」是这个工具最日常的用法之一。
///
/// 命令行路径不写这个文件（那条路的输入就是 `config.json` 本身，本来就在手上）。
pub const REQUEST_FILE: &str = "request.json";

/// 一次运行的元信息。报告重放需要的全部「非行数据」都在这里。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunMeta {
    pub schema_version: u32,
    /// `runs/` 下的目录名。
    pub run_id: String,
    pub plan_hash: String,
    /// 报告抬头要用的那几项（主控/辅测名、起止时间、耗时、健康横幅）。
    pub report: ReportMetaRecord,
    /// 计划摘要，供重放时在报告里说清楚「这是哪一份计划」。
    pub total_units: usize,
    /// 本轮的单元级判定分布。
    ///
    /// 有了它，历史列表不用重放 `rows.jsonl` 就能说出通过率——在此之前
    /// `/api/runs` 只能给出目录名、修改时间和字节数，于是隔夜回来找报告
    /// 只能靠时间戳猜，`RunsView` 的注释自己承认了这一点。
    ///
    /// 与报告顶部那八个格子**同源**（`report::verdict_totals`）：两处各算一遍
    /// 的话，列表和报告会各说一个数而两边都没错。
    ///
    /// 旧目录没有这个字段，读回来全是 0——所以消费方要靠 `total_units`
    /// 判断「这轮真的一个单元都没有」还是「这份 meta 是旧版写的」。
    pub verdict_totals: crate::report::VerdictTotals,
}

/// [`ReportMeta`] 的可序列化镜像。
///
/// 不直接给 `ReportMeta` 加 serde，是因为它是渲染层的入参、字段随渲染需求变；
/// 落盘的形状要稳。两者之间只有一次显式转换，加字段时编译器会指出来。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportMetaRecord {
    pub master_pc: String,
    pub agent_pc: String,
    pub agent_host: String,
    pub started: String,
    pub finished: String,
    pub elapsed: String,
    pub counter_source_caveat: String,
    pub run_health: String,
    /// 旧目录没有这个字段，读回来是空列表（报告里就不出这一段）。
    pub plan_notices: Vec<String>,
}

impl From<&ReportMeta> for ReportMetaRecord {
    fn from(meta: &ReportMeta) -> Self {
        ReportMetaRecord {
            master_pc: meta.master_pc.clone(),
            agent_pc: meta.agent_pc.clone(),
            agent_host: meta.agent_host.clone(),
            started: meta.started.clone(),
            finished: meta.finished.clone(),
            elapsed: meta.elapsed.clone(),
            counter_source_caveat: meta.counter_source_caveat.clone(),
            run_health: meta.run_health.clone(),
            plan_notices: meta.plan_notices.clone(),
        }
    }
}

impl From<ReportMetaRecord> for ReportMeta {
    fn from(record: ReportMetaRecord) -> Self {
        ReportMeta {
            master_pc: record.master_pc,
            agent_pc: record.agent_pc,
            agent_host: record.agent_host,
            started: record.started,
            finished: record.finished,
            elapsed: record.elapsed,
            counter_source_caveat: record.counter_source_caveat,
            run_health: record.run_health,
            plan_notices: record.plan_notices,
        }
    }
}

pub fn rows_path(dir: &Path) -> PathBuf {
    dir.join(ROWS_FILE)
}

pub fn meta_path(dir: &Path) -> PathBuf {
    dir.join(META_FILE)
}

pub fn request_path(dir: &Path) -> PathBuf {
    dir.join(REQUEST_FILE)
}

fn existing_regular_file(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} 不是普通文件", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// 落一份控制台请求原文。**调用方把失败降级成警告**：写不出它不该弄死测试。
pub fn write_console_request(dir: &Path, body: &str) -> std::io::Result<()> {
    existing_regular_file(&request_path(dir))?;
    std::fs::write(request_path(dir), body)
}

/// 读回控制台请求原文；没有这个文件时返回 `None`（命令行跑出来的目录就是这样）。
pub fn load_console_request(dir: &Path) -> Option<String> {
    let path = request_path(dir);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    metadata
        .file_type()
        .is_file()
        .then(|| std::fs::read_to_string(path).ok())?
}

/// 把若干行追加进 `rows.jsonl`。
///
/// **失败只返回错误，由调用方降级成警告**：收尾/旁路动作不许弄死测试，这是
/// 既有纪律（Excel 生成失败、截图失败都是同样处理）。磁盘满的时候，正在跑的
/// 那一轮测试还有价值，不该因为写不了副本而中断。
pub fn append_rows(dir: &Path, rows: &[Row]) -> std::io::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let path = rows_path(dir);
    existing_regular_file(&path)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // 一次拼好再写：中途 panic 顶多让最后一行不完整，而不是让几行交错。
    let mut buf = String::new();
    for row in rows {
        match serde_json::to_string(row) {
            Ok(line) => {
                buf.push_str(&line);
                buf.push('\n');
            }
            // 单行序列化失败不该拖垮整批（理论上不可达：Row 全是普通类型）。
            Err(error) => {
                buf.push_str(&format!(
                    "{{\"__unserializable\":true,\"error\":{}}}\n",
                    serde_json::to_string(&error.to_string()).unwrap_or_else(|_| "\"?\"".into())
                ));
            }
        }
    }
    file.write_all(buf.as_bytes())?;
    Ok(())
}

/// 读回一个 run 目录里的全部结果行。
///
/// **坏行跳过而不是整体失败**：崩溃留下的文件最后一行很可能是半截 JSON，
/// 而前面那些行是完好的十小时测量数据。为一行不完整的记录放弃全部，
/// 恰好是这个模块想避免的那种损失。返回值第二项是被跳过的行数。
pub fn load_rows(dir: &Path) -> std::io::Result<(Vec<Row>, usize)> {
    let path = rows_path(dir);
    existing_regular_file(&path)?;
    let file = std::fs::File::open(path)?;
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Row>(&line) {
            Ok(row) => rows.push(row),
            Err(_) => skipped += 1,
        }
    }
    Ok((rows, skipped))
}

pub fn write_meta(dir: &Path, meta: &RunMeta) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(meta)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let path = meta_path(dir);
    existing_regular_file(&path)?;
    std::fs::write(path, text)
}

pub fn load_meta(dir: &Path) -> std::io::Result<RunMeta> {
    let path = meta_path(dir);
    existing_regular_file(&path)?;
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{RowBackend, RowDirection, RowProtocol, RowSide};
    use crate::verdict::{ExecutionStatus, Verdict};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cpe_store_test_{}_{}_{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn sample_row(seq: usize, verdict: Verdict) -> Row {
        Row {
            sort_key: (seq, 0, 0, 0),
            task_id: format!("task-{seq}"),
            parent_id: format!("unit-{seq}"),
            task: format!("IPERF V4 TCP #{seq}"),
            verdict,
            execution_status: ExecutionStatus::Completed,
            reason_code: crate::reason::ReasonCode::RxTargetMet,
            reason_detail: "达标".into(),
            rx_avg: Some(930.5),
            rx_p10: Some(900.25),
            target_mbps: Some(850.0),
            sample_coverage: Some(0.98),
            unit_seq: seq,
            direction: RowDirection::Ab,
            protocol: RowProtocol::Tcp,
            backend: RowBackend::Iperf3,
            link_group: "SGMII ↔ WLAN".into(),
            src_side: RowSide::Master,
            dst_side: RowSide::Agent,
            nic_samples_rx: "raw/rx.csv".into(),
            nic_samples_tx: "raw/tx.csv".into(),
            ..Default::default()
        }
    }

    /// 写出去再读回来，判定相关的东西必须一个不差。
    ///
    /// 这条是 ADR-3 的核心保证：崩溃之后拿 rows.jsonl 重放出来的报告，其结论
    /// 必须和崩溃前那份一模一样。判定、原因码、速率、覆盖率、类型化的方向/协议
    /// 任何一项在往返中丢掉，重放报告就是一份**看起来正常但结论不同**的东西——
    /// 那比没有报告更糟。
    #[test]
    fn rows_survive_the_round_trip_with_every_judgement_field_intact() {
        let dir = temp_dir("roundtrip");
        let written = vec![
            sample_row(1, Verdict::Pass),
            sample_row(2, Verdict::RateFail),
            sample_row(3, Verdict::NotEvaluated),
        ];
        append_rows(&dir, &written).expect("append");

        let (read, skipped) = load_rows(&dir).expect("load");
        assert_eq!(skipped, 0);
        assert_eq!(read.len(), written.len());
        for (before, after) in written.iter().zip(read.iter()) {
            assert_eq!(after.verdict, before.verdict, "判定必须原样回来");
            assert_eq!(after.reason_code, before.reason_code);
            assert_eq!(after.execution_status, before.execution_status);
            assert_eq!(after.rx_avg, before.rx_avg);
            assert_eq!(after.rx_p10, before.rx_p10);
            assert_eq!(after.target_mbps, before.target_mbps);
            assert_eq!(after.sample_coverage, before.sample_coverage);
            // 类型化字段：Excel 出口要靠它们，不能在往返里退化成默认值。
            assert_eq!(after.direction, before.direction);
            assert_eq!(after.protocol, before.protocol);
            assert_eq!(after.backend, before.backend);
            assert_eq!(after.link_group, before.link_group);
            assert_eq!(after.src_side, before.src_side);
            assert_eq!(after.dst_side, before.dst_side);
            assert_eq!(after.unit_seq, before.unit_seq);
            // 两份样本 CSV 的引用都要在，否则重放报告点不开证据。
            assert_eq!(after.nic_samples_rx, before.nic_samples_rx);
            assert_eq!(after.nic_samples_tx, before.nic_samples_tx);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧 run 目录里的 `direction_summaries` 缺新字段时，整行仍要读得回来。
    ///
    /// `Row` 早就是 `#[serde(default)]`，但嵌在里面的 `DirectionSummary` 以前
    /// 不是——给它加一个字段，旧文件就会在 `serde` 那里报 `missing field`，
    /// 而 `load_rows` 把解析失败**静默计进 skipped**。结果不是报错，是重放报告
    /// 悄悄少了一批行：看起来正常，结论却和崩溃前那份不一样。
    #[test]
    fn an_old_direction_summary_without_the_newer_metrics_still_loads() {
        let dir = temp_dir("compat");
        // 手写一行「旧版本」记录：方向摘要里没有 tx_avg。
        let legacy = r#"{"task_id":"legacy-1","verdict":"PASS","rx_avg":930.5,"direction_summaries":[{"tag":"AB","src":"master/eth0","dst":"agent/eth1","rx_avg":930.5,"rx_p10":900.25}]}"#;
        std::fs::write(rows_path(&dir), format!("{legacy}\n")).expect("write");

        let (rows, skipped) = load_rows(&dir).expect("load");
        assert_eq!(skipped, 0, "旧行不该被当成坏行丢掉");
        assert_eq!(rows.len(), 1);
        let direction = &rows[0].direction_summaries[0];
        assert_eq!(direction.tag, "AB");
        assert_eq!(direction.rx_avg, Some(930.5), "旧字段照常读回");
        assert_eq!(direction.tx_avg, None, "缺的新字段取默认值，不是解析失败");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 判定按 label 字符串落盘，不是按枚举变体名。
    ///
    /// `RATE_FAIL` 这个拼法已经出现在报告 HTML、`task_results.json` 和
    /// `/api/progress` 里了。rows.jsonl 再写成 `RateFail`，同一个概念在同一个
    /// 产品里就有了两个名字，而且外部工具（用户拿 jq 扒数据）会两边都要处理。
    #[test]
    fn judgements_are_stored_as_the_labels_users_already_see() {
        let dir = temp_dir("labels");
        append_rows(&dir, &[sample_row(1, Verdict::RateFail)]).expect("append");
        let text = std::fs::read_to_string(rows_path(&dir)).expect("read");
        assert!(text.contains("\"RATE_FAIL\""), "判定要写成 label: {text}");
        assert!(
            text.contains("\"RX_TARGET_MET\""),
            "原因码要写成 label: {text}"
        );
        assert!(!text.contains("RateFail"), "不许出现枚举变体名: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 崩溃时写了一半的最后一行，不该让前面完好的十小时数据全部读不出来。
    ///
    /// 这正是选 JSONL 而不是「一个大 JSON 数组」的理由：后者少一个 `]` 就整份
    /// 报废。追加写的原子性要求本来就该压到最低。
    #[test]
    fn a_truncated_last_line_only_costs_that_one_row() {
        let dir = temp_dir("truncated");
        append_rows(
            &dir,
            &[sample_row(1, Verdict::Pass), sample_row(2, Verdict::Pass)],
        )
        .expect("append");
        // 模拟断电：追加半行 JSON。
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(rows_path(&dir))
            .expect("open");
        file.write_all("{\"sort_key\":[3,0,0,0],\"task\":\"半截的".as_bytes())
            .expect("write");
        drop(file);

        let (rows, skipped) = load_rows(&dir).expect("load");
        assert_eq!(rows.len(), 2, "完好的两行必须还在");
        assert_eq!(skipped, 1, "坏行要被计数并报出来");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 多次追加 = 一个连续的文件；每个单元结束写一次就是这个形状。
    #[test]
    fn appending_unit_by_unit_builds_one_continuous_file() {
        let dir = temp_dir("append");
        for seq in 1..=5 {
            append_rows(&dir, &[sample_row(seq, Verdict::Pass)]).expect("append");
        }
        let (rows, _) = load_rows(&dir).expect("load");
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows.iter().map(|row| row.unit_seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5],
            "顺序就是写入顺序"
        );
        // 空批次不该创建文件也不该报错。
        append_rows(&dir, &[]).expect("empty append");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 计划原文的往返，以及「命令行跑出来的目录没有它」这一档。
    ///
    /// 这份文件是「重新执行这一轮」唯一的输入：`meta.json` 里只有 `plan_hash`，
    /// 那是摘要，反推不出计划。读不出来时必须是一个明确的 `None`，让界面能
    /// 决定「这一行不显示重新执行」，而不是给个点了才报错的按钮。
    #[test]
    fn the_console_request_round_trips_and_is_absent_for_cli_runs() {
        let dir = temp_dir("request");
        assert!(
            load_console_request(&dir).is_none(),
            "还没写过就该是 None（命令行跑出来的目录就是这样）"
        );
        let body = r#"{"duration":180,"ui_plan":{"suites":[]}}"#;
        write_console_request(&dir, body).expect("写 request.json");
        assert_eq!(load_console_request(&dir).as_deref(), Some(body));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn history_store_never_follows_symlinked_request_rows_or_meta_files() {
        let dir = temp_dir("symlink");
        let outside = temp_dir("symlink-outside");
        std::fs::write(outside.join(REQUEST_FILE), r#"{"secret":true}"#).unwrap();
        std::fs::write(outside.join(ROWS_FILE), "{}").unwrap();
        std::fs::write(outside.join(META_FILE), r#"{"run_id":"outside"}"#).unwrap();

        std::os::unix::fs::symlink(outside.join(REQUEST_FILE), request_path(&dir)).unwrap();
        std::os::unix::fs::symlink(outside.join(ROWS_FILE), rows_path(&dir)).unwrap();
        std::os::unix::fs::symlink(outside.join(META_FILE), meta_path(&dir)).unwrap();

        assert!(load_console_request(&dir).is_none());
        assert!(write_console_request(&dir, "{}").is_err());
        assert!(load_rows(&dir).is_err());
        assert!(append_rows(&dir, &[sample_row(1, Verdict::Pass)]).is_err());
        assert!(load_meta(&dir).is_err());
        assert!(write_meta(&dir, &RunMeta::default()).is_err());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// meta.json 往返；未知字段不该让读取失败。
    #[test]
    fn meta_round_trips_and_tolerates_unknown_fields() {
        let dir = temp_dir("meta");
        let meta = RunMeta {
            schema_version: SCHEMA_VERSION,
            run_id: "run_20260830_101112_1234".into(),
            plan_hash: "abc123".into(),
            report: ReportMetaRecord {
                master_pc: "MASTER".into(),
                agent_pc: "AGENT".into(),
                started: "2026-08-30 10:11:12".into(),
                plan_notices: vec![
                    "门限 1180Mbps 超过这条链路的物理上限；按 95% 折算到 950Mbps 判定".into(),
                ],
                ..Default::default()
            },
            total_units: 42,
            verdict_totals: crate::report::VerdictTotals {
                total: 40,
                pass: 33,
                rate_fail: 7,
                skipped: 2,
                ..Default::default()
            },
        };
        write_meta(&dir, &meta).expect("write");
        let back = load_meta(&dir).expect("load");
        assert_eq!(back.run_id, meta.run_id);
        assert_eq!(back.plan_hash, meta.plan_hash);
        assert_eq!(back.total_units, 42);
        assert_eq!(back.report.master_pc, "MASTER");
        // 计划提示跟着 meta 走，重放出的报告才能说清门限被折算过。
        assert_eq!(back.report.plan_notices, meta.report.plan_notices);
        // 判定计数必须完整往返：历史列表就是靠它显示通过率的，丢一个字段
        // 那一列就会集体变成 0，而 0 和「没跑过」在屏幕上长得一样。
        assert_eq!(back.verdict_totals, meta.verdict_totals);
        assert!((back.verdict_totals.pass_rate_pct() - 82.5).abs() < 1e-9);

        // 未来版本多写了字段：旧版本必须还能读，而不是整份报废。
        std::fs::write(
            meta_path(&dir),
            r#"{"schema_version":1,"run_id":"r","future_field":{"x":1}}"#,
        )
        .expect("write future");
        let forward = load_meta(&dir).expect("未知字段不该让 meta.json 读不出来");
        assert_eq!(forward.run_id, "r");
        // 升级前写的 meta 没有这一块，读回来是全 0。消费方要靠 `total_units`
        // 区分「旧 meta」和「真的一个单元都没跑」，不能把 0 当成结论。
        assert_eq!(
            forward.verdict_totals,
            crate::report::VerdictTotals::default()
        );
        assert_eq!(forward.total_units, 0);
        // 升级前的 meta 没有计划提示，读回来是空列表，报告里不出这一段。
        assert!(forward.report.plan_notices.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **崩溃时写了一半的最后一行，不能带走前面那些完整的**（回归方案 RES-04）。
    ///
    /// `rows.jsonl` 是**增量**落盘的：跑完一个单元追加一行。进程被杀 / 掉电时，
    /// 最后一次写有很大概率停在半路。这时候唯一可接受的行为是：前面那些完整的
    /// 行一条不少地读回来，半条丢掉并**把丢了几条说出来**。
    ///
    /// 两个失败方向都很难查：整份文件因为最后一行坏掉而读不出来，等于一次
    /// 11 小时的运行白跑；反过来，静默丢掉半条又不吭声，重放出的报告会比崩溃前
    /// 那份少几行，而每一行看着都正常——拿两份报告对数量的人会以为自己记错了。
    #[test]
    fn a_half_written_last_line_does_not_take_the_finished_rows_with_it() {
        let dir = temp_dir("crash");
        append_rows(
            &dir,
            &[
                sample_row(1, Verdict::Pass),
                sample_row(2, Verdict::RateFail),
                sample_row(3, Verdict::Pass),
            ],
        )
        .expect("append");

        // 模拟崩溃：在完整的三行之后追加半条 JSON（没有闭合的 `}`，也没有换行）。
        let mut text = std::fs::read_to_string(rows_path(&dir)).expect("read");
        text.push_str(r#"{"task_id":"unit-4","verdict":"PA"#);
        std::fs::write(rows_path(&dir), &text).expect("write");

        let (rows, skipped) = load_rows(&dir).expect("坏的最后一行不该让整份文件读不出来");
        assert_eq!(
            rows.len(),
            3,
            "完整的三行必须全部读回来——因为最后一行坏掉就丢掉整份文件，\
             等于一次跑了十几小时的运行白跑"
        );
        assert_eq!(
            skipped, 1,
            "半条必须被计进 skipped 并报给用户（`replay_report` 会把它打出来）；\
             静默丢掉的话，重放报告会比崩溃前那份少几行而看不出来"
        );
        assert_eq!(rows[1].verdict, Verdict::RateFail, "判定不能在重放里变样");

        // 只有半条、一条完整的都没有：明确是空结果，而不是假装读到了什么。
        let empty = temp_dir("crash-only");
        std::fs::write(rows_path(&empty), r#"{"task_id":"x","verd"#).expect("write");
        let (rows, skipped) = load_rows(&empty).expect("load");
        assert!(rows.is_empty());
        assert_eq!(skipped, 1);

        // 空行不算坏行：追加时的换行、编辑器补的尾行都不该被记成丢数据。
        let blanks = temp_dir("crash-blank");
        append_rows(&blanks, &[sample_row(1, Verdict::Pass)]).expect("append");
        let mut text = std::fs::read_to_string(rows_path(&blanks)).expect("read");
        text.push_str("\n\n   \n");
        std::fs::write(rows_path(&blanks), &text).expect("write");
        let (rows, skipped) = load_rows(&blanks).expect("load");
        assert_eq!((rows.len(), skipped), (1, 0), "空行不是坏行");

        for d in [dir, empty, blanks] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// 历史目录的类型判断**只有一种形状**：不跟随符号链接的那种。
    ///
    /// 这条不是风格偏好，是一次横扫的固化。历史上这些模块里散落着
    /// `path.is_dir()` / `path.is_file()` / `fs::metadata()`——它们全都跟随链接，
    /// 于是「已经确认过是普通目录」之后再复判一次，等于把刚关上的替换窗口
    /// 重新打开；`dir_size` 这类递归入口更会直接把外部目录统计进 run 大小。
    /// 逐处改完之后，真正的风险是**下一处**：新加一个 `entry.path().is_dir()`
    /// 不会有任何测试变红，因为它在正常情况下行为完全一致。
    ///
    /// 所以照 `verdict_priority_has_exactly_one_definition_in_the_tree` 的样子，
    /// 在源码层面把门关上，粒度到文件为止。
    ///
    /// 不在这张表里的地方仍可以用跟随版本，那是**有意**的：
    /// `master::ui::replay_report_into` 收的是人在命令行上自己敲的目录，
    /// 「把 run 目录做成软链再重放」是合法用法；`webui::api_open_report`
    /// 打开的是本程序自己刚写出来的报告路径。两者都不枚举历史目录。
    #[test]
    fn history_modules_never_use_link_following_path_checks() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // 枚举/打包/落盘历史目录的全部模块。新增同类模块请加进来。
        let watched = [
            "report/store.rs",
            "master/webui/runs.rs",
            "master/webui/scenario.rs",
            "inner/history.rs",
            "inner/webui.rs",
        ];
        // 只跟随链接的写法要禁；带 `file_type()` 快照的写法是允许的那一种。
        let allowed = [
            "file_type().is_dir()",
            "file_type().is_file()",
            "kind.is_dir()",
            "kind.is_file()",
        ];
        let banned = [
            (
                ".is_dir()",
                "会跟随符号链接，用 symlink_metadata()?.file_type().is_dir()",
            ),
            (
                ".is_file()",
                "会跟随符号链接，用 symlink_metadata()?.file_type().is_file()",
            ),
            ("fs::metadata(", "会跟随符号链接，用 fs::symlink_metadata()"),
        ];

        let mut offenders = Vec::new();
        for rel in watched {
            let path = src.join(rel);
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("{} 读不到：{e}（模块改名了就同步这张表）", path.display())
            });
            // 只看生产代码：测试可以自由构造这些场景。
            let production = text
                .split_once("#[cfg(test)]")
                .map(|(head, _)| head)
                .unwrap_or(&text);
            // rustfmt 会把 `x.file_type()` 和 `.is_file()` 折成两行，所以先把以 `.`
            // 开头的续行折回它所属的逻辑行，否则允许的写法会被误判成违规。
            let mut logical: Vec<(usize, String)> = Vec::new();
            for (lineno, line) in production.lines().enumerate() {
                if line.trim_start().starts_with('.') {
                    if let Some(last) = logical.last_mut() {
                        last.1.push_str(line.trim_start());
                        continue;
                    }
                }
                logical.push((lineno + 1, line.to_string()));
            }
            for (lineno, line) in logical {
                let mut rest = line.clone();
                for ok in allowed {
                    rest = rest.replace(ok, "");
                }
                for (marker, why) in banned {
                    if rest.contains(marker) {
                        offenders.push(format!("{rel}:{lineno}: {marker} —— {why}"));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "历史目录里出现了跟随符号链接的类型判断：{offenders:#?}"
        );
    }
}
