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
