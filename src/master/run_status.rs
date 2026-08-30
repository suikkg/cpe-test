//! 运行状态的**结构化**模型，以及 executor 向外汇报状态的回调口。
//!
//! # 为什么需要这个模块
//!
//! 在此之前，一轮测试的全部对外状态只有 `ProgressOut { running, from, lines, report }`
//! ——四个字段，其中 `lines` 是日志文本，`report` 是**在日志里搜「报告已生成: 」
//! 捞出来的**。于是：
//!
//! - 单元级进度（现在跑到第几个、PASS/FAIL 各几个、失败清单、ETA）要靠前端去
//!   解析 `[i/total]` 和「==> 单元结果:」两种日志行；一次 11.5 小时、210 单元的
//!   测试，其结构化状态要从三万行文本里反推。
//! - 浏览器一刷新就得把整份日志重放一遍才能重建进度。
//! - **日志文案变成了协议**：改一句提示语就是兼容性事件，还得配测试钉住格式。
//!
//! 而这些状态在 executor 里**本来就是结构化的**（`RunSummary`、每个单元的
//! `aggregate_unit_verdict`）。把结构化数据打平成文本、再在另一种语言里重新
//! 解析回来，是纯粹的自找麻烦。所以这里让 Rust 直接吐 [`RunStatus`]（ADR-2）。
//!
//! # 依赖方向
//!
//! executor 依赖 [`RunObserver`] 这个 trait，**不依赖 webui**；webui 提供实现。
//! 依赖方向和以前一样是单向的。CLI 不传 observer（`None`），行为逐字节不变。
//!
//! # 状态词汇表不发明第二套
//!
//! 单元状态直接复用 `Verdict` 的六个取值 + 一个 `skipped` 布尔。这是「判定口径
//! 只有一份实现」这条铁律在进度层的延伸——进度页上说 PASS 的那个单元，和报告里
//! 说 PASS 的必须是同一件事。
use crate::verdict::Verdict;
use serde::Serialize;

/// 一个已经跑完（或被跳过）的单元。
#[derive(Debug, Clone, Serialize)]
pub struct UnitStatus {
    /// 1-based，与日志里的 `[i/total]`、报告里的 `#N` 是同一个数。
    pub seq: usize,
    pub title: String,
    /// `Verdict::label()`，不另造词汇表。
    pub verdict: String,
    /// 空串 = 无原因码。
    pub reason_code: String,
    /// 已裁剪到一行，供失败清单直接显示。
    pub reason_detail: String,
    /// resume 命中而跳过。
    pub skipped: bool,
    /// 实际耗时（秒）。
    pub secs: u64,
    /// 报表分组键，来自 `Unit.link_group`。失败清单按链路分组要用它。
    pub link_group: String,
}

/// 正在跑的那个单元。
#[derive(Debug, Clone, Serialize)]
pub struct CurrentUnit {
    pub seq: usize,
    pub title: String,
    /// builder 估算的耗时（秒）；ETA 用它，前端不复算。
    pub est_secs: u64,
    /// 本单元开始的时刻（`now_full()` 格式）。
    pub started_at: String,
    pub link_group: String,
}

/// 各判定的计数。字段与 `RunSummary` 同名同义，不另起炉灶。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunCounts {
    pub pass: usize,
    pub fail: usize,
    pub measured: usize,
    pub not_evaluated: usize,
    pub setup_error: usize,
    pub skip: usize,
}

impl RunCounts {
    fn bump(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Pass => self.pass += 1,
            Verdict::RateFail => self.fail += 1,
            Verdict::Measured => self.measured += 1,
            Verdict::NotEvaluated => self.not_evaluated += 1,
            Verdict::SetupError => self.setup_error += 1,
            Verdict::Skip => self.skip += 1,
        }
    }
}

/// 一轮运行的结构化状态。
///
/// **不落盘**：它可以从 `rows.jsonl` 加上计划重新推出来，再存一份就是第二个
/// 事实源。崩溃恢复靠的是结果落盘（ADR-3），不是靠这个。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunStatus {
    /// `runs/` 下的目录名。
    pub run_id: String,
    pub plan_hash: String,
    pub started_at: String,
    pub total_units: usize,
    pub current: Option<CurrentUnit>,
    /// 已完成的单元。游标语义与日志一致：`units_from=N` 只取增量。
    pub done: Vec<UnitStatus>,
    pub counts: RunCounts,
    /// 剩余未执行单元的 `est_secs` 之和（含当前单元剩余）。
    ///
    /// 在 Rust 算，因为 `est_secs` 的唯一实现在 builder。前端复算一份就又是
    /// 一处「两边算出不同数字」的候选。
    pub eta_secs: Option<u64>,
    /// 因连续零测量而中止时的单元序号。
    pub aborted_at_unit: Option<usize>,
    /// 报告路径。由回调直接写入，**不再从日志里搜「报告已生成: 」**。
    pub report: String,
    /// 整轮是否已经结束（报告写完或被中止）。
    pub finished: bool,
}

impl RunStatus {
    /// 从 `units_from` 开始的增量快照，连同**实际生效的游标**一起返回。
    ///
    /// 1s 轮询的稳态负载因此回到常数级：稳定期每拍只回 0 或 1 个单元，而不是
    /// 把已经跑完的 200 个单元再传一遍。
    ///
    /// # 越界游标自愈
    ///
    /// `units_from > done.len()` 在正常协议下**不可能**发生：回给前端的游标就是
    /// 「当时已完成的总数」，而它只增不减。所以越界只有一个来源——浏览器还攥着
    /// **上一轮**的游标。旧实现对它返回空列表，于是新一轮的前 N 个单元对那个
    /// 浏览器永远不存在（`mergeUnits` 又按 seq 去重，上一轮的行还会赖着不走），
    /// 结果是计数格显示新一轮、单元列表和失败清单显示上一轮。
    ///
    /// 这里把越界当成「从头给我」：这是唯一能自愈的解释，代价是一次全量重传。
    ///
    /// # 为什么还要比 `run_id`
    ///
    /// 只看越界补不全。第二个标签页攥着上一轮的游标 250，而新一轮此刻已经跑完
    /// 300 个单元时，`250 <= 300` 根本不触发上面那条自愈，返回的是新一轮的
    /// `done[250..300]`——那个标签页于是**永久缺**新一轮的 0..249 号单元，而
    /// 计数格走的是全量 `counts`，两块显示对不上。
    ///
    /// 游标只在**一轮之内**有意义，所以让它跟着 `run_id` 一起失效：前端把自己
    /// 手上那个 `run_id` 一并送来，对不上就从头给。`None` = 调用方不参与这套
    /// 协议（CLI、老客户端、curl），保持原样只按越界判。
    pub fn since(&self, units_from: usize, client_run_id: Option<&str>) -> (usize, RunStatus) {
        let stale_run = client_run_id.is_some_and(|id| id != self.run_id);
        let units_from = if units_from > self.done.len() || stale_run {
            0
        } else {
            units_from
        };
        // 逐字段构造而不是 `self.clone()` 再覆盖 `done`：后者会先克隆一份完整的
        // `done` 再立刻丢掉——稳态下就是每秒一次 200 条记录的无用拷贝。
        // 顺带让新增字段在这里编译不过，而不是悄悄漏出快照。
        let out = RunStatus {
            run_id: self.run_id.clone(),
            plan_hash: self.plan_hash.clone(),
            started_at: self.started_at.clone(),
            total_units: self.total_units,
            current: self.current.clone(),
            done: self.done[units_from..].to_vec(),
            counts: self.counts.clone(),
            eta_secs: self.eta_secs,
            aborted_at_unit: self.aborted_at_unit,
            report: self.report.clone(),
            finished: self.finished,
        };
        (units_from, out)
    }
}

/// executor 汇报状态的回调口。
///
/// 每个方法都挂在**既有的状态转移点**上（那些地方本来就在 `logln`），所以这里
/// 没有引入任何新状态机——等于在打日志的旁边多写一行结构化事件。
///
/// 实现必须是线程安全且不阻塞的：executor 在跑测试的主循环里调它。
pub trait RunObserver: Send + Sync + std::fmt::Debug {
    /// 计划已确定，准备开跑。
    fn run_started(&self, _run_id: &str, _plan_hash: &str, _total_units: usize, _eta_secs: u64) {}
    /// 一个单元开始（对应 `logln("[i/total] title")` 那一处）。
    fn unit_started(&self, _unit: CurrentUnit) {}
    /// 一个单元结束（对应 `logln("==> 单元结果")` + `db.set` 那一处）。
    fn unit_finished(&self, _unit: UnitStatus, _remaining_est_secs: u64) {}
    /// 因连续零测量中止剩余队列。
    fn run_aborted(&self, _at_unit: usize) {}
    /// 报告已落盘（对应「报告已生成: 」那一处）。
    fn report_written(&self, _path: &str) {}
    /// 整轮结束。
    fn run_finished(&self) {}
}

/// 一个把回调直接记进 [`RunStatus`] 的实现。
///
/// WebUI 用它；CLI 不用（`observer = None`，行为零变化）。
#[derive(Debug, Default)]
pub struct RunStatusRecorder {
    status: std::sync::Mutex<RunStatus>,
}

impl RunStatusRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 返回「实际生效的游标 + 该游标之后的增量」。越界游标与跨轮次游标的
    /// 处理见 [`RunStatus::since`]。
    pub fn snapshot(&self, units_from: usize, client_run_id: Option<&str>) -> (usize, RunStatus) {
        crate::util::lock_recover(&self.status).since(units_from, client_run_id)
    }

    /// 丢弃上一轮的状态。
    ///
    /// `/api/run` 接受请求时就要调，**不能等到 worker 线程里的 `run_started`**：
    /// 那之间要读配置、扫拓扑、建计划，够 1s 轮询打好几拍，而那几拍回的是
    /// 上一轮**已完成**的全套单元。前端把它们攒进列表、把游标推到上一轮的
    /// 长度，之后新一轮的单元就再也进不来了。
    pub fn reset(&self) {
        *crate::util::lock_recover(&self.status) = RunStatus::default();
    }
}

impl RunObserver for RunStatusRecorder {
    fn run_started(&self, run_id: &str, plan_hash: &str, total_units: usize, eta_secs: u64) {
        let mut status = crate::util::lock_recover(&self.status);
        *status = RunStatus {
            run_id: run_id.to_string(),
            plan_hash: plan_hash.to_string(),
            started_at: crate::util::now_full(),
            total_units,
            eta_secs: Some(eta_secs),
            ..Default::default()
        };
    }

    fn unit_started(&self, unit: CurrentUnit) {
        crate::util::lock_recover(&self.status).current = Some(unit);
    }

    fn unit_finished(&self, unit: UnitStatus, remaining_est_secs: u64) {
        let mut status = crate::util::lock_recover(&self.status);
        let verdict = crate::verdict::Verdict::from_label(&unit.verdict);
        if let Some(verdict) = verdict {
            status.counts.bump(verdict);
        }
        status.done.push(unit);
        status.current = None;
        status.eta_secs = Some(remaining_est_secs);
    }

    fn run_aborted(&self, at_unit: usize) {
        let mut status = crate::util::lock_recover(&self.status);
        status.aborted_at_unit = Some(at_unit);
        status.current = None;
    }

    fn report_written(&self, path: &str) {
        crate::util::lock_recover(&self.status).report = path.to_string();
    }

    fn run_finished(&self) {
        let mut status = crate::util::lock_recover(&self.status);
        status.current = None;
        status.eta_secs = Some(0);
        status.finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(seq: usize, verdict: Verdict) -> UnitStatus {
        UnitStatus {
            seq,
            title: format!("unit {seq}"),
            verdict: verdict.label().to_string(),
            reason_code: String::new(),
            reason_detail: String::new(),
            skipped: verdict == Verdict::Skip,
            secs: 7,
            link_group: "SGMII ↔ WLAN".into(),
        }
    }

    /// 计数按 `Verdict` 的六个取值走，一个都不许漏。
    ///
    /// 进度页说 PASS 的那个单元，和报告里说 PASS 的必须是同一件事——所以这里
    /// 不发明第二套状态词汇表，直接用 `Verdict`。漏一个分支的后果是进度页上的
    /// 计数和报告顶部的统计对不上，而两边都「看起来没错」。
    #[test]
    fn every_verdict_lands_in_its_own_counter() {
        let recorder = RunStatusRecorder::new();
        recorder.run_started("run_x", "hash", 6, 600);
        for (index, verdict) in [
            Verdict::Pass,
            Verdict::RateFail,
            Verdict::Measured,
            Verdict::NotEvaluated,
            Verdict::SetupError,
            Verdict::Skip,
        ]
        .into_iter()
        .enumerate()
        {
            recorder.unit_finished(unit(index + 1, verdict), 0);
        }
        let (_, status) = recorder.snapshot(0, None);
        assert_eq!(status.counts.pass, 1);
        assert_eq!(status.counts.fail, 1);
        assert_eq!(status.counts.measured, 1);
        assert_eq!(status.counts.not_evaluated, 1);
        assert_eq!(status.counts.setup_error, 1);
        assert_eq!(status.counts.skip, 1);
        assert_eq!(status.done.len(), 6);
    }

    /// `units_from` 游标只回增量。
    ///
    /// 这是 1s 轮询能一直跑 11.5 小时的前提：不加游标的话，跑到第 200 个单元时
    /// 每一拍都要把前面 199 个再传一遍——稳态负载随进度线性增长，而机器这时
    /// 正在灌线速。
    #[test]
    fn the_unit_cursor_only_returns_new_units() {
        let recorder = RunStatusRecorder::new();
        recorder.run_started("run_x", "hash", 3, 300);
        recorder.unit_finished(unit(1, Verdict::Pass), 200);
        recorder.unit_finished(unit(2, Verdict::Pass), 100);

        let (_, first) = recorder.snapshot(0, None);
        assert_eq!(first.done.len(), 2);

        // 前端把游标推到 2，下一拍应当什么都不收。
        let (_, idle) = recorder.snapshot(2, None);
        assert!(idle.done.is_empty(), "稳态每拍不该重传已完成单元");
        // 计数是全量的：它不受游标影响，否则刷新一次页面计数就归零了。
        assert_eq!(idle.counts.pass, 2);
        assert_eq!(idle.total_units, 3);

        recorder.unit_finished(unit(3, Verdict::RateFail), 0);
        let (_, next) = recorder.snapshot(2, None);
        assert_eq!(next.done.len(), 1, "只该收到新完成的那一个");
        assert_eq!(next.done[0].seq, 3);

        // 游标越界 = 浏览器攥着上一轮的游标回来了。唯一能自愈的解释是
        // 「从头给我」，所以要拿到全量而不是空列表——返回空的话，新一轮的
        // 单元对那个浏览器永远不存在。生效游标也要跟着退回 0，否则前端
        // 下一拍还会带着越界的那个值回来。
        let (effective, healed) = recorder.snapshot(99, None);
        assert_eq!(effective, 0, "越界游标要自愈成 0");
        assert_eq!(healed.done.len(), 3, "自愈之后必须拿到全量");
    }

    /// **第二轮不许显示第一轮的结果。**
    ///
    /// `/api/run` 一被接受就 `reset()`，而不是等 worker 线程里的 `run_started`：
    /// 那之间要读配置、扫拓扑、建计划，够 1s 轮询打好几拍。少了这一步，那几拍
    /// 回的是上一轮**已完成**的全套单元，浏览器把它们攒进列表、把游标推到上一轮
    /// 的长度；等新一轮真的开始，服务端按那个游标只回增量，新一轮的前 N 个单元
    /// 就再也进不来了。表现是计数格显示新一轮、单元列表和失败清单显示上一轮——
    /// 两块都"看起来正常"，只是说的不是同一轮。
    #[test]
    fn a_second_run_never_serves_the_previous_runs_units() {
        let recorder = RunStatusRecorder::new();
        recorder.run_started("run_first", "hash1", 3, 300);
        for seq in 1..=3 {
            recorder.unit_finished(unit(seq, Verdict::Pass), 0);
        }
        recorder.run_finished();
        let (cursor, first) = recorder.snapshot(0, None);
        assert_eq!((cursor, first.done.len()), (0, 3));
        let stale_cursor = cursor + first.done.len();

        // 「开始测试」被接受的那一刻：计划还没建完，run_started 还没到。
        recorder.reset();
        let (cursor, between) = recorder.snapshot(stale_cursor, None);
        assert!(between.done.is_empty(), "重置之后不该还端着上一轮的单元");
        assert_eq!(cursor, 0, "游标要跟着退回去，否则前端下一拍还带着 3 回来");
        assert_eq!(between.counts.pass, 0, "计数也是上一轮的，一起清掉");
        assert!(between.run_id.is_empty());

        // 新一轮真的开始，跑完两个单元。
        recorder.run_started("run_second", "hash2", 3, 300);
        recorder.unit_finished(unit(1, Verdict::RateFail), 0);
        recorder.unit_finished(unit(2, Verdict::Pass), 0);
        let (_, second) = recorder.snapshot(cursor, None);
        assert_eq!(second.run_id, "run_second");
        assert_eq!(
            second.done.iter().map(|u| u.seq).collect::<Vec<_>>(),
            vec![1, 2],
            "新一轮的单元必须能收到"
        );
        assert_eq!(second.counts.fail, 1);
    }

    /// **新一轮已经跑过陈旧游标时，越界判据救不了场——要靠 `run_id`。**
    ///
    /// 上一条用例走的是「reset 之后 done 是空的」，越界判据（`游标 > done.len()`）
    /// 恰好成立。但第二个标签页并不总是那么幸运：它攥着上一轮的游标 3 回来时，
    /// 新一轮可能**已经跑完 4 个单元**了。这时 `3 <= 4` 不越界，服务端安安静静
    /// 地回 `done[3..4]`，那个标签页于是永久缺新一轮的 1..3 号单元——而计数格
    /// 走的是全量 `counts`，两块显示对不上，且都"看起来正常"。
    ///
    /// 游标只在一轮之内有意义，所以让它跟着 `run_id` 一起失效。
    #[test]
    fn a_cursor_from_the_previous_run_is_dropped_even_when_it_is_in_range() {
        let recorder = RunStatusRecorder::new();
        recorder.run_started("run_first", "hash1", 5, 500);
        for seq in 1..=3 {
            recorder.unit_finished(unit(seq, Verdict::Pass), 0);
        }
        let stale_cursor = recorder.snapshot(0, None).1.done.len();
        assert_eq!(stale_cursor, 3);

        // 新一轮，而且已经跑得比那个陈旧游标更远——越界判据在这里不成立。
        recorder.reset();
        recorder.run_started("run_second", "hash2", 5, 500);
        for seq in 1..=4 {
            recorder.unit_finished(unit(seq, Verdict::Pass), 0);
        }

        // 不带 run_id（老客户端）：维持原状，只按越界判，于是漏掉开头三个。
        let (legacy_cursor, legacy) = recorder.snapshot(stale_cursor, None);
        assert_eq!(legacy_cursor, 3);
        assert_eq!(
            legacy.done.iter().map(|u| u.seq).collect::<Vec<_>>(),
            vec![4],
            "这正是要修的行为：不带 run_id 时只能按越界判"
        );

        // 带着上一轮的 run_id 回来：整份重发。
        let (healed_cursor, healed) = recorder.snapshot(stale_cursor, Some("run_first"));
        assert_eq!(healed_cursor, 0, "跨轮次的游标要自愈成 0");
        assert_eq!(
            healed.done.iter().map(|u| u.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4],
            "自愈之后必须拿到新一轮的全量"
        );

        // run_id 对得上就照常走增量，不能因为多了这个参数就退化成每拍全量。
        let (kept_cursor, kept) = recorder.snapshot(stale_cursor, Some("run_second"));
        assert_eq!(kept_cursor, 3, "同一轮之内游标仍然有效");
        assert_eq!(kept.done.len(), 1, "同一轮之内只回增量");
    }

    /// 刷新页面 = 用 `units_from=0` 再要一次，必须拿回全量快照。
    ///
    /// 旧的做法是把整份日志重放一遍再解析出进度；一次 11.5 小时的测试有三万行
    /// 日志，而其中真正有用的结构化状态就是下面这几个字段。
    #[test]
    fn a_page_refresh_gets_the_whole_snapshot_without_replaying_logs() {
        let recorder = RunStatusRecorder::new();
        recorder.run_started("run_20260830_101112_1234", "plan-hash", 10, 1000);
        recorder.unit_finished(unit(1, Verdict::Pass), 900);
        recorder.unit_started(CurrentUnit {
            seq: 2,
            title: "unit 2".into(),
            est_secs: 100,
            started_at: "2026-08-30 10:12:00".into(),
            link_group: "SGMII ↔ WLAN".into(),
        });

        let (_, full) = recorder.snapshot(0, None);
        assert_eq!(full.run_id, "run_20260830_101112_1234");
        assert_eq!(full.plan_hash, "plan-hash");
        assert_eq!(full.total_units, 10);
        assert_eq!(full.done.len(), 1);
        assert_eq!(full.current.as_ref().map(|c| c.seq), Some(2));
        assert_eq!(full.eta_secs, Some(900));
        assert!(!full.finished);
    }

    /// 报告路径由回调送达，不再从日志文本里捞。
    #[test]
    fn the_report_path_arrives_through_the_callback() {
        let recorder = RunStatusRecorder::new();
        recorder.run_started("run_x", "hash", 1, 10);
        assert!(recorder.snapshot(0, None).1.report.is_empty());
        recorder.report_written("runs/run_x/report.html");
        recorder.run_finished();
        let (_, status) = recorder.snapshot(0, None);
        assert_eq!(status.report, "runs/run_x/report.html");
        assert!(status.finished, "整轮结束要能被前端看出来");
        assert_eq!(status.eta_secs, Some(0));
        assert!(status.current.is_none(), "结束后不该还挂着一个当前单元");
    }

    /// `RunStatus` 的 JSON 形状就是前端的契约，字段名不许悄悄改。
    ///
    /// 这条替代了原计划里那个「钉住日志行格式」的测试（PLAN v5.0 §5.4）：
    /// 现在被钉住的是 DTO，而不是给人看的提示语——后者可以自由改。
    #[test]
    fn the_progress_dto_keeps_the_field_names_the_console_reads() {
        let recorder = RunStatusRecorder::new();
        recorder.run_started("run_x", "hash", 2, 20);
        recorder.unit_finished(unit(1, Verdict::RateFail), 10);
        let json = serde_json::to_value(recorder.snapshot(0, None).1).expect("序列化");

        for key in [
            "run_id",
            "plan_hash",
            "started_at",
            "total_units",
            "current",
            "done",
            "counts",
            "eta_secs",
            "aborted_at_unit",
            "report",
            "finished",
        ] {
            assert!(json.get(key).is_some(), "RunStatus 少了字段 {key}");
        }
        let done = &json["done"][0];
        for key in [
            "seq",
            "title",
            "verdict",
            "reason_code",
            "reason_detail",
            "skipped",
            "secs",
            "link_group",
        ] {
            assert!(done.get(key).is_some(), "UnitStatus 少了字段 {key}");
        }
        // 判定用 label 字符串，与报告、rows.jsonl 同一套词汇表。
        assert_eq!(done["verdict"], "RATE_FAIL");
        for key in [
            "pass",
            "fail",
            "measured",
            "not_evaluated",
            "setup_error",
            "skip",
        ] {
            assert!(
                json["counts"].get(key).is_some(),
                "RunCounts 少了字段 {key}"
            );
        }
    }
}
