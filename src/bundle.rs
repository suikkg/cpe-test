//! 两种测试模式共用流式离线产物打包与临时文件回收。
use std::io::Write;
use std::path::PathBuf;
fn regular_dir(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}
fn collect_files(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, PathBuf)>) {
    // 递归入口也要重新确认：上一级枚举之后，目录项可能已经被替换成链接。
    // `read_dir` 会跟随这种链接；不在这里挡住，下面的「不打包符号链接」只
    // 保护了静态快照，没有保护目录替换发生在两次枚举之间的窗口。
    if !regular_dir(dir) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // 同 `resolve_run_dir`：run 目录**里面**的符号链接也不打包。跟着走会把
    // 链接目标塞进 zip，而 zip 里的路径仍是 `run_xxx/...`，看不出内容来自别处。
    // 特殊文件（FIFO、设备节点等）也跳过：它们不是工具会写出的报告产物，
    // 对它们调用 `File::open` 可能阻塞下载线程，甚至把设备内容读进包里。
    let mut paths: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let kind = entry.file_type().ok()?;
            (kind.is_dir() || kind.is_file()).then_some((entry.path(), kind.is_dir()))
        })
        .collect();
    paths.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (path, is_dir) in paths {
        if is_dir {
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
pub(crate) struct Bundle {
    pub(crate) path: PathBuf,
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
pub(crate) fn build_bundle(dir: &std::path::Path, run_id: &str) -> std::io::Result<Bundle> {
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
