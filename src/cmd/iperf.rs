//! iperf3 命令构造、文本输出解析、server 进程管理、client 执行（带重试）
//!
//! 说明：统一用文本输出（-f m -i 1）而不是 --json，
//! 原因：--json 要等进程结束才输出（无实时速率），且旧版 Windows iperf3(3.1.x)
//! 不支持 --json-stream。文本模式对所有版本都稳定，且能实时逐行读速率。
//!
//! 按职责分文件，外部一律经本模块的 `pub use` 取用：
//!
//! - `args`：server/client 命令行构造，`extra` 的受控参数黑名单；
//! - `parse`：汇总输出解析（`parse_output`）与逐行实时事件；
//! - `server`：`IperfServerMgr`，server 进程注册、就绪探测、停止回收；
//! - `client`：同步 client 执行，重试、取消与事件时间轴对齐；
//! - `jobs`：`IperfClientJobMgr`，异步 client 作业、租约与 owner 清理。
//!
//! 本文件只留 server 与作业两套注册表共用的生命周期工具。

use crate::util::OutputLimit;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

mod args;
mod client;
mod jobs;
mod parse;
mod server;

pub use args::{check_client_extra, client_args};
// 模块外只有 builder 的测试直接用它，核对生成的 `extra` 不踩受控参数。
#[cfg(test)]
pub(crate) use args::reserved_flags_in_extra;
pub(crate) use args::supports_forceflush_with;
pub(crate) use client::{align_event_to_epoch, run_client_controlled_inner};
pub use client::{run_client, run_client_controlled};
pub use jobs::IperfClientJobMgr;
pub use parse::{parse_output, IperfParsed};
pub use server::IperfServerMgr;

/// iperf3 client / server 的文本输出各自最多保留多少（开头 + 结尾，中间按行省略）。
///
/// `-i 1` 每秒每流一行，`-P 32` 每秒 33 行、约 3KB。不设上限时跑到 9 小时左右，
/// 一份输出就超过主控读响应的 `http_client::MAX_RESPONSE_BYTES`：server 停止与
/// client 结果的响应读不回来，被当成「停止未确认」；而长时长单元本来就是测热衰减的
/// 预期用法。保住的是首尾——判定只读末尾的汇总行，开头是连接信息；中间的逐秒行
/// 在 client 一侧另有流事件逐条记录（判定窗口用的就是它），server 一侧只作排障参考。
/// 结尾 8 MiB 能原样留下 `-P 32` 约 45 分钟、单流约 24 小时的逐秒输出。
pub(crate) const OUTPUT_LIMIT: OutputLimit = OutputLimit {
    head_bytes: 256 * 1024,
    tail_bytes: 8 * 1024 * 1024,
};

// client 最多三次尝试的输出拼在同一份结果里，仍要远小于响应上限。
const _: () = assert!(
    3 * (OUTPUT_LIMIT.head_bytes + OUTPUT_LIMIT.tail_bytes)
        < crate::http_client::MAX_RESPONSE_BYTES / 2
);

const LIFECYCLE_TOMBSTONE_TTL: Duration = Duration::from_secs(10 * 60);
const LIFECYCLE_LOCK_STRIPES: usize = 64;

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lifecycle_id_ok(id: &str) -> bool {
    id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn validate_lifecycle_id(label: &str, id: &str) -> Result<(), String> {
    if id.is_empty() || lifecycle_id_ok(id) {
        Ok(())
    } else {
        Err(format!(
            "{label} 非法：只允许 160 字节以内的字母、数字、-_.:"
        ))
    }
}

fn lease_deadline(lease_secs: u64) -> Result<Option<Instant>, String> {
    if lease_secs == 0 {
        return Ok(None);
    }
    Instant::now()
        .checked_add(Duration::from_secs(lease_secs))
        .map(Some)
        .ok_or_else(|| format!("资源 lease_secs={lease_secs} 过大，无法表示截止时间"))
}

fn lifecycle_lock_index(id: &str) -> usize {
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    hasher.finish() as usize % LIFECYCLE_LOCK_STRIPES
}

#[derive(Debug, Default)]
pub struct LifecycleCleanupResult {
    pub stopped: usize,
    pub errors: Vec<String>,
}

#[cfg(test)]
mod tests;
