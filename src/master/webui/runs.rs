//! 历史运行的列举与打包下载（ADR-15、§13.3）。
//!
//! # 补的是哪个洞
//!
//! `/api/open-report` 调的是 `console::open_path`——它在**跑控制台的那台机器上**
//! 用系统程序打开报告。于是 `--ui-bind` 放开之后，远程浏览器访问者**永远拿不到
//! 报告**，而报告就是这个工具的产物本身。整个 HTTP 面此前没有任何文件通道。
//!
//! # 为什么不能「把报告当页面服务出来」
//!
//! 报告 HTML 里的截图/CSV 是**相对路径子资源**。浏览器加载它们时不带自定义头，
//! 相对 URL 也不继承查询串——撞的是和控制台页面同一堵「鉴权先于路由」的墙。
//! 给报告开子资源白名单等于在铁律上开口子（ADR-5 已经否决过同构方案）。
//!
//! 所以走**打包下载**：一次带 token 的 GET 把整个 run 目录取回本地，解开就是
//! 完整可读的报告。
//!
//! # 为什么是 store 模式（不压缩）
//!
//! 测试数据以文本和 PNG 为主，压不压缩对内网下载没有意义；关掉压缩后 `zip`
//! 的全部编解码后端都不用编进来。
//!
//! 这里曾经是**手写**的 zip writer，理由写的是「零新依赖」——那条理由不成立：
//! `rust_xlsxwriter` 早就把同一个 `zip` crate 编进二进制了，省下的只是
//! `Cargo.toml` 里的一行。代价则是真的：条目数写死 `u16`、大小与偏移写死
//! `u32`、没有 zip64，超过 65535 个文件或 4 GiB 时产出的是**结构损坏但看起来
//! 正常**的包，而且不报错。换成 crate 之后这些边界要么被 zip64 正确处理，
//! 要么是一个明确的 `Err`。
use super::*;

/// `runs/` 目录名。与 `master/ui.rs` 的 `RUNS_DIR` 是同一个约定。
const RUNS_DIR: &str = "runs";

fn regular_dir(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

#[derive(Debug, Serialize)]
pub(super) struct RunEntry {
    /// 目录名，同时也是 `bundle.zip` 的入参和 `cpe_test report` 的入参。
    pub(super) id: String,
    /// 目录的修改时间（`YYYY-MM-DD HH:MM:SS`），拿不到时为空。
    pub(super) modified: String,
    pub(super) has_report: bool,
    /// 有 rows.jsonl 就能重放报告，即使 report.html 没写出来（崩溃场景）。
    pub(super) has_rows: bool,
    pub(super) has_xlsx: bool,
    /// 有 `request.json` 就能把这一轮的计划装载回控制台再跑一遍。
    ///
    /// 命令行跑出来的目录没有它（那条路的输入就是 `config.json` 本身），
    /// 界面上「重新执行」因此按目录逐个决定显不显示，而不是一律给个会报错的按钮。
    pub(super) has_request: bool,
    /// 整个目录的字节数——下载前让人知道要拉多大。
    pub(super) bytes: u64,
    /// 本轮开始时刻，取自 `meta.json`；旧目录或半截运行为空。
    ///
    /// **不能拿 `modified` 顶替**：那是目录的修改时刻，重放一次报告它就会变，
    /// 于是隔夜回来看到的「时间」是自己上午点重放的那一下。
    pub(super) started: String,
    pub(super) elapsed: String,
    /// 参与统计的单元数（不含 SKIP）。`0` 且 `total_units` 也为 0 时，
    /// 多半是升级前写的旧 `meta.json`，前端按「未知」显示而不是「全 0」。
    pub(super) totals: crate::report::VerdictTotals,
    /// 计划里一共有多少个单元。用来区分「旧 meta」和「真的没跑」。
    pub(super) total_units: usize,
    /// 有没有读到 `meta.json`；没有就是命令行旧目录或崩在写它之前。
    pub(super) has_meta: bool,
}

/// 校验并解析 run id。
///
/// **真的是白名单**：枚举 `runs/` 下的目录，拿 `file_name()` 逐个精确比对，
/// 命中了才用**那个 `DirEntry` 自己的路径**。请求串从头到尾没有参与任何路径
/// 拼接，所以「能表示上级目录的写法」这个问题面根本不存在。
///
/// 这里以前是黑名单（拒 `/`、`\`、`..`、前导 `.`）加 `Path::join`，而黑名单
/// 漏了盘符：Windows 上 `Path::new("runs").join("C:")` 会被 `C:` 的盘符前缀
/// **整个替换**成 `C:`（`PathBuf::push` 对带前缀的路径就是这个语义），
/// `is_dir()` 为真，于是 `/api/runs/C:/bundle.zip` 打包的是进程当前目录——
/// 里面有 `config.json`（含 `agent_token`）、`task_results.json` 和全部历史
/// run。`--ui-bind` 之后控制台就在局域网上，只隔着一道口令。
///
/// 这正是「穷举危险写法」这条路的失败方式：本模块和它的测试当时都已经把
/// 「盘符」写进注释当反例，实现里却没有它。所以改成按名字精确比对——不需要
/// 知道有哪些危险写法。
///
/// 名字对上还不够，**那个名字得真的是这里的一个目录**。符号链接名字一样能命中、
/// `is_dir()` 也为真（它跟着走），于是 `runs/x -> /etc` 会被当成一次运行打包出去。
/// 单看不是提权——能在 `runs/` 里建链接的人本来就读得到那些文件；危险在于它跨了
/// 信任边界：`--ui-bind` 之后控制台在局域网上，「本地写一个链接」被放大成
/// 「远程凭口令读任意文件」。这个工具自己从不在 `runs/` 里建链接，挡它零代价。
pub(super) fn resolve_run_dir(id: &str) -> Option<std::path::PathBuf> {
    if id.is_empty() || !regular_dir(std::path::Path::new(RUNS_DIR)) {
        return None;
    }
    std::fs::read_dir(RUNS_DIR)
        .ok()?
        .flatten()
        .find(|entry| {
            entry.file_name() == std::ffi::OsStr::new(id)
            // `file_type()` 直接来自目录项且不跟随链接。
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| entry.path())
}

fn regular_file(dir: &std::path::Path, name: &str) -> bool {
    std::fs::symlink_metadata(dir.join(name)).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn dir_size(dir: &std::path::Path) -> u64 {
    // 与 collect_files 一样，递归入口不能跟随目录项在两次枚举之间被替换
    // 成的符号链接；否则历史列表的大小也会把 run 外的目录算进去。
    if !regular_dir(dir) {
        return 0;
    }
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.file_type().is_dir() {
            total = total.saturating_add(dir_size(&entry.path()));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

fn modified_label(dir: &std::path::Path) -> String {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return String::new();
    };
    if !meta.file_type().is_dir() {
        return String::new();
    }
    let Ok(time) = meta.modified() else {
        return String::new();
    };
    let Ok(since) = time.duration_since(std::time::UNIX_EPOCH) else {
        return String::new();
    };
    crate::util::format_unix_seconds(since.as_secs())
}

/// 列出 `runs/` 下的历史运行。一次目录扫描，无状态。
///
/// 不做列表页的话远程用户拿不到 run id，`bundle.zip` 形同虚设——这两者是
/// 一个功能的两半（ADR-15）。
pub(super) fn api_runs() -> Result<serde_json::Value, String> {
    let root = std::path::Path::new(RUNS_DIR);
    if !regular_dir(root) {
        return serde_json::to_value(Vec::<RunEntry>::new()).map_err(|e| e.to_string());
    }
    let mut entries: Vec<RunEntry> = std::fs::read_dir(root)
        .map_err(|error| format!("读不到 {RUNS_DIR}/：{error}"))?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| {
            let dir = entry.path();
            // `meta.json` 是小文件，读它比同一趟里那次递归 `dir_size` 便宜得多。
            // 读不到就按「旧目录」处理：**不猜**，让前端显示「未知」。
            let meta = crate::report::store::load_meta(&dir).ok();
            RunEntry {
                id: entry.file_name().to_string_lossy().into_owned(),
                modified: modified_label(&dir),
                has_report: regular_file(&dir, "report.html"),
                has_rows: regular_file(&dir, crate::report::store::ROWS_FILE),
                has_xlsx: regular_file(&dir, "summary.xlsx"),
                has_request: regular_file(&dir, crate::report::store::REQUEST_FILE),
                bytes: dir_size(&dir),
                started: meta
                    .as_ref()
                    .map(|m| m.report.started.clone())
                    .unwrap_or_default(),
                elapsed: meta
                    .as_ref()
                    .map(|m| m.report.elapsed.clone())
                    .unwrap_or_default(),
                totals: meta.as_ref().map(|m| m.verdict_totals).unwrap_or_default(),
                total_units: meta.as_ref().map(|m| m.total_units).unwrap_or_default(),
                has_meta: meta.is_some(),
            }
        })
        .collect();
    // 新的在前：隔夜回来找报告是常态。
    entries.sort_by(|a, b| b.id.cmp(&a.id));
    serde_json::to_value(entries).map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub(super) struct RunIdReq {
    pub(super) id: String,
}

/// 取回某一轮的计划原文，供「重新执行」把它装载回控制台。
///
/// **只回计划，不直接开跑**：真正开跑仍然要走 `/api/run` 的 `plan_hash` 闸门
/// （「界面上确认的东西 == 实际跑的东西」唯一的强制点）。隔了一夜的网口拓扑
/// 可能已经变了，老计划里的端点未必还存在——那时该看到的是复核页上的差异，
/// 而不是一轮悄悄少跑了几条链路的测试。
pub(super) fn api_run_request(body: &str) -> Result<serde_json::Value, String> {
    let req: RunIdReq = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let Some(dir) = resolve_run_dir(req.id.trim()) else {
        return Err("找不到这个运行目录".into());
    };
    let Some(text) = crate::report::store::load_console_request(&dir) else {
        return Err("这一轮没有留下计划原文（多半是命令行跑的，或者是升级前的旧目录）".into());
    };
    let request: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("request.json 解析失败: {error}"))?;
    Ok(serde_json::json!({ "id": req.id, "request": request }))
}

/// 从 `rows.jsonl` 重放报告与 Excel。
///
/// **不要求「必须没有报告才能恢复」**：崩溃留下的 report.html 可能是上一次写到
/// 一半的，或者是补跑之前的旧版本；「已经有报告」恰恰是最需要用新数据盖掉它的
/// 情形之一。重放本身是幂等的——同一批行放几次都是同一份报告。
///
/// 唯一挡住的是**正在跑的那一轮**：它的 rows.jsonl 还在被追加、report.html 还
/// 会被收尾流程写一次，这时插一脚只会产出一份谁也说不清是哪个时刻的报告。
pub(super) fn api_run_replay(
    console: &Arc<Console>,
    body: &str,
) -> Result<serde_json::Value, String> {
    let req: RunIdReq = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let id = req.id.trim().to_string();
    let Some(dir) = resolve_run_dir(&id) else {
        return Err("找不到这个运行目录".into());
    };
    if console.running.load(Ordering::SeqCst) && console.run_status.snapshot(0, None).1.run_id == id
    {
        return Err("这一轮正在跑，等它结束会自动出报告；现在重放只会得到一份半截的".into());
    }
    let outcome = crate::master::ui::replay_report_into(&dir)?;
    Ok(serde_json::json!({
        "id": id,
        "report": outcome.report.display().to_string(),
        "xlsx": outcome.xlsx.map(|path| path.display().to_string()),
        "rows": outcome.rows,
        "skipped": outcome.skipped,
        "warnings": outcome.warnings,
    }))
}

#[derive(Debug, Deserialize)]
pub(super) struct CompareReq {
    /// 基线（旧的那一轮）目录名。
    pub(super) baseline: String,
    /// 本轮（新的那一轮）目录名。
    pub(super) current: String,
}

/// 两轮对比。差异报告写进**本轮**目录，跟着它一起归档和打包。
///
/// 两个目录名都走 `resolve_run_dir` 的白名单（枚举 `runs/` 逐个精确比对，
/// 请求串从头到尾不参与路径拼接），和 bundle/replay 是同一道门。
pub(super) fn api_run_compare(
    console: &Arc<Console>,
    body: &str,
) -> Result<serde_json::Value, String> {
    let req: CompareReq = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let (baseline_id, current_id) = (req.baseline.trim(), req.current.trim());
    if baseline_id == current_id {
        return Err("基线和本轮是同一个目录，没有可比的东西".into());
    }
    let Some(baseline_dir) = resolve_run_dir(baseline_id) else {
        return Err("找不到基线运行目录".into());
    };
    let Some(current_dir) = resolve_run_dir(current_id) else {
        return Err("找不到本轮运行目录".into());
    };
    // 正在跑的那一轮不许参与对比：`rows.jsonl` 还在增量追加，比出来的
    // 「缺失」其实是「还没跑到」。
    if console.running.load(Ordering::SeqCst) {
        let live = console.run_status.snapshot(0, None).1.run_id;
        if live == baseline_id || live == current_id {
            return Err(
                "其中一轮正在跑，等它结束再对比；现在比会把「还没跑到」当成「缺失」".into(),
            );
        }
    }
    crate::master::ui::compare_runs_into(&baseline_dir, &current_dir).map(|outcome| {
        serde_json::json!({
            "baseline": baseline_id,
            "current": current_id,
            "report": outcome.report.display().to_string(),
            "same_plan": outcome.same_plan,
            "has_regression": outcome.has_regression,
            "regressed": outcome.regressed,
            "slower": outcome.slower,
            "fixed": outcome.fixed,
            "added": outcome.added,
            "disappeared": outcome.disappeared,
            "unchanged": outcome.unchanged,
            "ambiguous": outcome.ambiguous,
            "resumed": outcome.resumed,
        })
    })
}

// ---------------------------------------------------------------------------
// 打包
// ---------------------------------------------------------------------------

pub(super) use crate::bundle::build_bundle;
