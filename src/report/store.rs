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
        }
    }
}

pub fn rows_path(dir: &Path) -> PathBuf {
    dir.join(ROWS_FILE)
}

pub fn meta_path(dir: &Path) -> PathBuf {
    dir.join(META_FILE)
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
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(rows_path(dir))?;
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
    let file = std::fs::File::open(rows_path(dir))?;
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
    std::fs::write(meta_path(dir), text)
}

pub fn load_meta(dir: &Path) -> std::io::Result<RunMeta> {
    let text = std::fs::read_to_string(meta_path(dir))?;
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
                ..Default::default()
            },
            total_units: 42,
        };
        write_meta(&dir, &meta).expect("write");
        let back = load_meta(&dir).expect("load");
        assert_eq!(back.run_id, meta.run_id);
        assert_eq!(back.plan_hash, meta.plan_hash);
        assert_eq!(back.total_units, 42);
        assert_eq!(back.report.master_pc, "MASTER");

        // 未来版本多写了字段：旧版本必须还能读，而不是整份报废。
        std::fs::write(
            meta_path(&dir),
            r#"{"schema_version":1,"run_id":"r","future_field":{"x":1}}"#,
        )
        .expect("write future");
        let forward = load_meta(&dir).expect("未知字段不该让 meta.json 读不出来");
        assert_eq!(forward.run_id, "r");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
