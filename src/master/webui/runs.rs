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
use std::io::Write;
use std::path::PathBuf;

/// `runs/` 目录名。与 `master/ui.rs` 的 `RUNS_DIR` 是同一个约定。
const RUNS_DIR: &str = "runs";

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
    /// 整个目录的字节数——下载前让人知道要拉多大。
    pub(super) bytes: u64,
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
pub(super) fn resolve_run_dir(id: &str) -> Option<std::path::PathBuf> {
    if id.is_empty() {
        return None;
    }
    std::fs::read_dir(RUNS_DIR)
        .ok()?
        .flatten()
        .find(|entry| entry.file_name() == std::ffi::OsStr::new(id) && entry.path().is_dir())
        .map(|entry| entry.path())
}

fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total = total.saturating_add(dir_size(&entry.path()));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

fn modified_label(dir: &std::path::Path) -> String {
    let Ok(meta) = std::fs::metadata(dir) else {
        return String::new();
    };
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
    if !root.is_dir() {
        return serde_json::to_value(Vec::<RunEntry>::new()).map_err(|e| e.to_string());
    }
    let mut entries: Vec<RunEntry> = std::fs::read_dir(root)
        .map_err(|error| format!("读不到 {RUNS_DIR}/：{error}"))?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            let dir = entry.path();
            RunEntry {
                id: entry.file_name().to_string_lossy().into_owned(),
                modified: modified_label(&dir),
                has_report: dir.join("report.html").is_file(),
                has_rows: dir.join(crate::report::store::ROWS_FILE).is_file(),
                has_xlsx: dir.join("summary.xlsx").is_file(),
                bytes: dir_size(&dir),
            }
        })
        .collect();
    // 新的在前：隔夜回来找报告是常态。
    entries.sort_by(|a, b| b.id.cmp(&a.id));
    serde_json::to_value(entries).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 打包
// ---------------------------------------------------------------------------

fn collect_files(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_files(&path, base, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            // zip 内路径一律用 `/`，Windows 上解压才不会出一层怪目录。
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((name, path.clone()));
        }
    }
}

fn zip_err(error: zip::result::ZipError) -> std::io::Error {
    match error {
        zip::result::ZipError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}

/// 打包产物：一个临时 zip 文件，`Drop` 时自删。
///
/// 用 RAII 而不是在各条路径上手写 `remove_file`：中间任何一步出错都要删，
/// 漏一条就是在用户的临时目录里堆下一个和 run 目录同样大的文件。
pub(super) struct Bundle {
    pub(super) path: PathBuf,
}

impl Drop for Bundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 同 pid 内的序号，避免两个并发下载撞同一个临时文件名（`UI_WORKERS` = 4）。
static BUNDLE_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 把整个 run 目录打成一个 store 模式的 zip，**落到临时文件**。
///
/// 顶层套一个和 run 同名的目录，解开就是 `run_xxx/report.html`——而不是把十几个
/// 文件散进用户的下载目录。
///
/// # 为什么不在内存里拼
///
/// 这里曾经是「整包 `Vec<u8>` + 每个文件先 `read_to_end`」，峰值内存约等于
/// 「目录大小 + 最大单文件」。而 store 模式不压缩，zip 大小就是目录大小：一次
/// 11.5 小时运行的逐样本 CSV 和原始输出可以到 GB 级——上面那句 `large_file(true)`
/// 的注释自己就在说条目可能逼近 4 GiB。更要命的是**这个进程同时正在跑测试**，
/// 一次下载把它 OOM 掉，赔进去的是整轮测量。
///
/// 换成 `io::copy` 到文件后，峰值内存与目录大小无关。
pub(super) fn build_bundle(dir: &std::path::Path, run_id: &str) -> std::io::Result<Bundle> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files);

    let seq = BUNDLE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("cpe_bundle_{}_{seq}.zip", std::process::id()));
    // 先删再 `create_new`：`create_new` 遇到已存在的路径（含符号链接）直接
    // 报错而不是跟过去，同 pid 复用序号时也不会沿用别人留下的文件。
    let _ = std::fs::remove_file(&path);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    // 从这里开始，任何一条 `?` 提前返回都由 `Bundle` 负责把文件删掉。
    let bundle = Bundle { path };

    let mut writer = zip::ZipWriter::new(file);
    // `large_file(true)`：单个条目超过 4 GiB 时走 zip64 而不是把长度截断。
    // 一次 11.5 小时的运行，逐样本 CSV 和原始输出加起来到不了这个量级，但
    // 「到不了」和「到了会坏掉」是两回事——手写版就是在这里静默截断的。
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(true);

    for (name, path) in &files {
        writer
            .start_file(format!("{run_id}/{name}"), options)
            .map_err(zip_err)?;
        let mut source = std::fs::File::open(path)?;
        std::io::copy(&mut source, &mut writer)?;
    }

    let mut file = writer.finish().map_err(zip_err)?;
    file.flush()?;
    Ok(bundle)
}
