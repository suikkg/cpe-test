//! 内环历史。
//!
//! 独立于子网的 `runs/`：目录、列举、下载和「装载回控制台」全部只认
//! [`super::RUNS_ROOT`]，两边的历史不会互相看见，也不会互相污染。
//!
//! 目录名的解析是**白名单**，和子网 `runs.rs` 同一条路子：枚举目录、拿
//! `file_name()` 精确比对，命中了才用那个 `DirEntry` 自己的路径。请求串从头到尾
//! 不参与任何路径拼接，所以「能表示上级目录的写法」「Windows 盘符会替换整条
//! 路径」这类问题面根本不存在。名字对上还不够，那个名字得真的是这里的一个
//! 目录——目录项的 `file_type()` 不跟随链接，`inner_runs/x -> /etc` 不会被当成
//! 一次运行读出去。
use super::config::InnerConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 一次内环运行的摘要，跑完就写在运行目录里。
///
/// 单独一份而不是现读 `result.json`：后者带着逐秒采样，列个历史列表要把
/// 每一轮的几十 MB 全解析一遍。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct Summary {
    pub created_at: String,
    pub probe_only: bool,
    /// None 为旧记录；不能仅凭没有 error 推断已经收尾。
    #[serde(default)]
    pub finished: Option<bool>,
    pub units: usize,
    pub passed: usize,
    pub rate_failed: usize,
    pub not_evaluated: usize,
    /// 参与本轮的网口名，按执行顺序。
    pub links: Vec<String>,
    pub error: Option<String>,
}

pub(super) fn summarize(report: &super::RunReport) -> Summary {
    let count = |label: &str| {
        report
            .units
            .iter()
            .filter(|unit| unit.verdict == label)
            .count()
    };
    let mut links: Vec<String> = Vec::new();
    for unit in &report.units {
        if !links.contains(&unit.link) {
            links.push(unit.link.clone());
        }
    }
    Summary {
        created_at: report.created_at.clone(),
        probe_only: report.probe_only,
        // `perform` 在错误收尾时也会清空 current；只看 current 会把半轮失败
        // 伪装成「已完成」，进而让下一轮 RESUME 复用其中的 PASS 单元。
        finished: Some(report.error.is_none() && report.current.is_empty()),
        units: report.units.len(),
        passed: count("PASS"),
        rate_failed: count("RATE_FAIL"),
        not_evaluated: count("NOT_EVALUATED"),
        links,
        error: report.error.clone(),
    }
}

pub(super) fn reusable_summary(summary: &Summary) -> bool {
    // 旧摘要没有 finished 字段，不能把「未知是否收尾」当成完整证据；带错误
    // 的摘要也不应提供 RESUME，即使旧版本曾把它错误写成 finished=true。
    summary.finished == Some(true) && !summary.probe_only && summary.error.is_none()
}

#[derive(Debug, Serialize)]
struct Entry {
    id: String,
    #[serde(flatten)]
    summary: Summary,
    has_report: bool,
    /// 有 config.json 才谈得上「按原配置重新生成计划」。
    has_config: bool,
    bytes: u64,
}

fn resolve(id: &str) -> Option<PathBuf> {
    if id.is_empty() || !regular_dir(Path::new(super::RUNS_ROOT)) {
        return None;
    }
    std::fs::read_dir(super::RUNS_ROOT)
        .ok()?
        .flatten()
        .find(|entry| {
            entry.file_name() == std::ffi::OsStr::new(id)
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| entry.path())
}

fn regular_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = dir.join(name);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    metadata.file_type().is_file().then_some(path)
}

fn regular_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
            metadata.file_type().is_file().then_some(metadata)
        })
        .fold(0u64, |total, meta| total.saturating_add(meta.len()))
}

/// 列出内环历史。一次目录扫描，无状态；新的在前。
pub(super) fn list() -> Result<serde_json::Value, String> {
    let root = Path::new(super::RUNS_ROOT);
    if !regular_dir(root) {
        return serde_json::to_value(Vec::<Entry>::new()).map_err(|e| e.to_string());
    }
    let mut entries: Vec<Entry> = std::fs::read_dir(root)
        .map_err(|error| format!("读不到 {}/：{error}", super::RUNS_ROOT))?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| {
            let dir = entry.path();
            Entry {
                id: entry.file_name().to_string_lossy().into_owned(),
                summary: regular_file(&dir, "summary.json")
                    .and_then(|path| std::fs::read_to_string(path).ok())
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or_default(),
                has_report: regular_file(&dir, "report.html").is_some(),
                has_config: regular_file(&dir, "config.json").is_some(),
                bytes: dir_size(&dir),
            }
        })
        .collect();
    // 目录名以时间戳开头，倒序即最新在前：隔夜回来找报告是常态。
    entries.sort_by(|a, b| b.id.cmp(&a.id));
    serde_json::to_value(entries).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct IdReq {
    id: String,
}

fn dir_of(body: &str) -> Result<(String, PathBuf), String> {
    let req: IdReq = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
    let id = req.id.trim().to_string();
    let dir = resolve(&id).ok_or("找不到这一轮内环运行")?;
    Ok((id, dir))
}

pub(super) fn report(body: &str) -> Result<serde_json::Value, String> {
    let (id, dir) = dir_of(body)?;
    let path = regular_file(&dir, "report.html").ok_or("这一轮的报告不是普通文件")?;
    let html =
        std::fs::read_to_string(path).map_err(|error| format!("读不到这一轮的报告：{error}"))?;
    Ok(serde_json::json!({"name": format!("cpe-inner-{id}.html"), "html": html}))
}

/// 取回某一轮的配置，供「按原配置重新生成计划」装载回控制台。
///
/// **只回配置，不直接开跑**：隔了一夜的网口拓扑可能已经变了，老配置里的网卡
/// 未必还在。该看到的是重新预览时的差异，而不是一轮悄悄少跑了几条网口的测试。
/// 令牌不在文件里（`AgentConfig::token` 是 `skip_serializing`），装载回来后
/// 仍要手工重填。
pub(super) fn config(body: &str) -> Result<serde_json::Value, String> {
    let (_, dir) = dir_of(body)?;
    let path = regular_file(&dir, "config.json").ok_or("这一轮的配置不是普通文件")?;
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("读不到这一轮的配置：{error}"))?;
    // 读回来仍走同一条解析/迁移链：老版本目录里的配置也能装载。
    let cfg: InnerConfig = super::config::parse_config(&text)?;
    serde_json::to_value(cfg).map_err(|e| e.to_string())
}

/// 找出 24 小时内已经完整落盘且判定为 PASS 的内环单元。
///
/// 只读取历史的 `result.json`，不把当前运行目录或半截 `units.jsonl` 当成可复用
/// 证据；单元身份由计划模块生成，端口和执行序号变化不会影响命中。
pub(super) fn fresh_pass_ids() -> HashSet<String> {
    let mut ids = HashSet::new();
    if !regular_dir(Path::new(super::RUNS_ROOT)) {
        return ids;
    }
    let Ok(entries) = std::fs::read_dir(super::RUNS_ROOT) else {
        return ids;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(summary_path) = regular_file(&dir, "summary.json") else {
            continue;
        };
        let Ok(summary_text) = std::fs::read_to_string(summary_path) else {
            continue;
        };
        let Ok(summary) = serde_json::from_str::<Summary>(&summary_text) else {
            continue;
        };
        if !reusable_summary(&summary) {
            continue;
        }
        let Some(result_path) = regular_file(&dir, "result.json") else {
            continue;
        };
        let Ok(metadata) = std::fs::symlink_metadata(&result_path) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let age = match now.duration_since(modified) {
            Ok(age) => age,
            Err(error) if error.duration() <= Duration::from_secs(60) => Duration::ZERO,
            Err(_) => continue,
        };
        if age >= Duration::from_secs(24 * 60 * 60) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(result_path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(units) = value.get("units").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for unit in units {
            if unit.get("verdict").and_then(serde_json::Value::as_str) == Some("PASS") {
                if let Some(id) = unit.get("id").and_then(serde_json::Value::as_str) {
                    ids.insert(id.to_string());
                }
            }
        }
    }
    ids
}
