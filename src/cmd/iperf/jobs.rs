//! 异步 iperf client 作业：HTTP 只负责建 / 查 / 停，作业在独立线程里跑完整个 `-t`。

use super::client::{align_event_to_epoch, run_client_controlled};
use super::{
    lease_deadline, lock_recover, validate_lifecycle_id, LifecycleCleanupResult,
    LIFECYCLE_TOMBSTONE_TTL,
};
use crate::protocol::{
    IperfClientOut, IperfClientReq, IperfClientStartReq, IperfClientStatusOut, IperfClientStopOut,
    IperfFlowEvent,
};
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_CLIENT_STOP_WAIT: Duration = Duration::from_secs(10);

pub(super) struct ClientJobEntry {
    pub(super) events: Arc<Mutex<Vec<IperfFlowEvent>>>,
    pub(super) completion: Arc<ClientCompletion>,
    pub(super) cancel: Arc<AtomicBool>,
    started: Instant,
    pub(super) expires_at: Mutex<Option<Instant>>,
    dynamic_lease: AtomicBool,
    owner_id: String,
    fingerprint: String,
    thread: Mutex<ClientThreadState>,
    thread_cv: Condvar,
}

pub(super) struct ClientCompletion {
    pub(super) result: Mutex<Option<IperfClientOut>>,
    cv: Condvar,
}

#[derive(Default)]
struct ClientThreadState {
    installed: bool,
    handle: Option<std::thread::JoinHandle<()>>,
    joining: bool,
    joined: bool,
}

#[derive(Clone)]
struct ClientTombstone {
    stopped_at: Instant,
    out: IperfClientStopOut,
}

#[derive(Default)]
pub(super) struct ClientRegistry {
    pub(super) jobs: HashMap<String, Arc<ClientJobEntry>>,
    tombstones: HashMap<String, ClientTombstone>,
}

/// 异步 iperf client 作业管理器。
///
/// HTTP 请求只负责创建/查询/停止 job，不再占住 agent 的固定 worker
/// 直到 -t 结束，因此 20/32 条远端流可以真正同时运行。
pub struct IperfClientJobMgr {
    pub(super) inner: Mutex<ClientRegistry>,
    /// 串行化同一 job ID 的 start/stop，避免 spawn 安装窗口内重复 start
    /// 先返回成功、随后首个 spawn 却失败的竞态；不同 ID 仍可并发。
    job_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    seq: AtomicU64,
}

impl Default for IperfClientJobMgr {
    fn default() -> Self {
        Self::new()
    }
}

impl IperfClientJobMgr {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ClientRegistry::default()),
            job_locks: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
        }
    }

    fn job_lock(&self, id: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            lock_recover(&self.job_locks)
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// 旧调用点兼容包装；新协议必须使用带 request_id/owner_id 的 start_request。
    #[allow(dead_code)]
    pub fn start(&self, bin: String, req: IperfClientReq) -> String {
        self.start_request(
            bin,
            IperfClientStartReq {
                request: req,
                request_id: String::new(),
                owner_id: String::new(),
                lease_secs: 0,
            },
        )
        .expect("legacy client job id generation must not conflict")
    }

    /// 使用 request_id 幂等创建远端 client job。非空 request_id 同时就是实际 job id，
    /// 因而即使 start 响应丢失，调用方仍可精确 stop。
    pub fn start_request(&self, bin: String, start: IperfClientStartReq) -> Result<String, String> {
        validate_lifecycle_id("request_id", &start.request_id)?;
        validate_lifecycle_id("owner_id", &start.owner_id)?;
        let fingerprint = format!(
            "{}|{}",
            start.owner_id,
            serde_json::to_string(&start.request)
                .map_err(|e| format!("序列化 client 请求失败: {e}"))?
        );
        let request_id = start.request_id;
        let owner_id = start.owner_id;
        let lease_secs = start.lease_secs;
        let req = start.request;
        self.start_job_managed(
            request_id,
            owner_id,
            lease_secs,
            fingerprint,
            move |cancel, events, job_epoch| {
                let event_sink = Arc::clone(&events);
                let mut event_origin_ms = None;
                run_client_controlled(
                    &bin,
                    &req,
                    Some(cancel.as_ref()),
                    |_| {},
                    move |mut event| {
                        align_event_to_epoch(
                            &mut event,
                            job_epoch.elapsed().as_millis().min(u64::MAX as u128) as u64,
                            &mut event_origin_ms,
                        );
                        if let Ok(mut g) = event_sink.lock() {
                            g.push(event);
                        }
                    },
                )
            },
        )
    }

    /// 为其他受控灌包后端复用同一套幂等异步作业、租约、stop/join 与 owner 清理。
    /// runner 必须在 cancel=true 时终止并回收其子进程后再返回。
    pub(crate) fn start_external_request<F>(
        &self,
        request_id: String,
        owner_id: String,
        lease_secs: u64,
        fingerprint: String,
        runner: F,
    ) -> Result<String, String>
    where
        F: FnOnce(Arc<AtomicBool>, Arc<Mutex<Vec<IperfFlowEvent>>>, Instant) -> IperfClientOut
            + Send
            + 'static,
    {
        validate_lifecycle_id("request_id", &request_id)?;
        validate_lifecycle_id("owner_id", &owner_id)?;
        self.start_job_managed(
            request_id,
            owner_id,
            lease_secs,
            format!("external|{fingerprint}"),
            runner,
        )
    }

    #[cfg(test)]
    pub(super) fn start_job<F>(&self, runner: F) -> String
    where
        F: FnOnce(Arc<AtomicBool>, Arc<Mutex<Vec<IperfFlowEvent>>>) -> IperfClientOut
            + Send
            + 'static,
    {
        self.start_job_managed(
            String::new(),
            String::new(),
            0,
            String::new(),
            move |cancel, events, _job_epoch| runner(cancel, events),
        )
        .expect("legacy client job id generation must not conflict")
    }

    fn next_job_id(&self) -> String {
        loop {
            let id = format!("cli{}", self.seq.fetch_add(1, Ordering::SeqCst));
            let registry = lock_recover(&self.inner);
            if !registry.jobs.contains_key(&id) && !registry.tombstones.contains_key(&id) {
                return id;
            }
        }
    }

    fn prune_client_tombstones(&self) {
        let live_ids: HashSet<String> = {
            let mut registry = lock_recover(&self.inner);
            registry
                .tombstones
                .retain(|_, tombstone| tombstone.stopped_at.elapsed() <= LIFECYCLE_TOMBSTONE_TTL);
            registry
                .jobs
                .keys()
                .chain(registry.tombstones.keys())
                .cloned()
                .collect()
        };
        lock_recover(&self.job_locks)
            .retain(|id, lock| live_ids.contains(id) || Arc::strong_count(lock) > 1);
    }

    pub(super) fn start_job_managed<F>(
        &self,
        request_id: String,
        owner_id: String,
        lease_secs: u64,
        fingerprint: String,
        runner: F,
    ) -> Result<String, String>
    where
        F: FnOnce(Arc<AtomicBool>, Arc<Mutex<Vec<IperfFlowEvent>>>, Instant) -> IperfClientOut
            + Send
            + 'static,
    {
        self.prune_client_tombstones();
        let requested_deadline = lease_deadline(lease_secs)?;
        let id = if request_id.is_empty() {
            self.next_job_id()
        } else {
            request_id
        };
        let job_lock = self.job_lock(&id);
        let _job_guard = lock_recover(&job_lock);

        {
            let registry = lock_recover(&self.inner);
            if registry.tombstones.contains_key(&id) {
                return Err(format!(
                    "iperf client request_id {id} 已停止，拒绝迟到 start"
                ));
            }
            if let Some(entry) = registry.jobs.get(&id) {
                if entry.fingerprint == fingerprint {
                    *lock_recover(&entry.expires_at) = requested_deadline;
                    entry.dynamic_lease.store(lease_secs > 0, Ordering::SeqCst);
                    return Ok(id);
                }
                return Err(format!(
                    "iperf client request_id {id} 的重复 start 参数不一致"
                ));
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let completion = Arc::new(ClientCompletion {
            result: Mutex::new(None),
            cv: Condvar::new(),
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let job_epoch = Instant::now();
        let entry = Arc::new(ClientJobEntry {
            events: Arc::clone(&events),
            completion: Arc::clone(&completion),
            cancel: Arc::clone(&cancel),
            started: job_epoch,
            expires_at: Mutex::new(requested_deadline),
            dynamic_lease: AtomicBool::new(lease_secs > 0),
            owner_id,
            fingerprint,
            thread: Mutex::new(ClientThreadState::default()),
            thread_cv: Condvar::new(),
        });
        {
            let mut registry = lock_recover(&self.inner);
            // 与 stop-before-start 串行：若未知 ID 已被 stop 建 tombstone，就不能复活。
            if registry.tombstones.contains_key(&id) {
                return Err(format!(
                    "iperf client request_id {id} 已停止，拒绝迟到 start"
                ));
            }
            if let Some(existing) = registry.jobs.get(&id) {
                if existing.fingerprint == entry.fingerprint {
                    *lock_recover(&existing.expires_at) = requested_deadline;
                    existing
                        .dynamic_lease
                        .store(lease_secs > 0, Ordering::SeqCst);
                    return Ok(id);
                }
                return Err(format!(
                    "iperf client request_id {id} 的重复 start 参数不一致"
                ));
            }
            registry.jobs.insert(id.clone(), Arc::clone(&entry));
        }

        let id_for_error = id.clone();
        let handle = match std::thread::Builder::new()
            .name(format!("iperf-client-{id}"))
            .spawn(move || {
                let out = catch_unwind(AssertUnwindSafe(|| runner(cancel, events, job_epoch)))
                    .unwrap_or_else(|panic_value| IperfClientOut {
                        ok: false,
                        cancelled: false,
                        output: format!(
                            "iperf client worker panic: {}",
                            panic_message(panic_value.as_ref())
                        ),
                        ..Default::default()
                    });
                *lock_recover(&completion.result) = Some(out);
                completion.cv.notify_all();
            }) {
            Ok(handle) => handle,
            Err(e) => {
                *lock_recover(&entry.completion.result) = Some(IperfClientOut {
                    ok: false,
                    output: format!("创建 iperf client worker 失败: {e}"),
                    ..Default::default()
                });
                entry.completion.cv.notify_all();
                {
                    let mut thread = lock_recover(&entry.thread);
                    thread.installed = true;
                    thread.joined = true;
                    entry.thread_cv.notify_all();
                }
                let mut registry = lock_recover(&self.inner);
                if registry
                    .jobs
                    .get(&id_for_error)
                    .map(|current| Arc::ptr_eq(current, &entry))
                    .unwrap_or(false)
                {
                    registry.jobs.remove(&id_for_error);
                }
                return Err(format!("创建 iperf client worker 失败: {e}"));
            }
        };
        {
            let mut thread = lock_recover(&entry.thread);
            thread.handle = Some(handle);
            thread.installed = true;
            entry.thread_cv.notify_all();
        }
        Ok(id)
    }

    pub fn status(&self, id: &str, cursor: usize) -> Result<IperfClientStatusOut, String> {
        let entry = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| format!("iperf client job 不存在: {id}"))?;
        // 先读完成状态、再复制事件：worker 总是先推送全部事件、再写 completion。
        // 若先复制事件，合法的并发交错会让调用方看到 done=true 却缺少
        // 尾部 Traffic/Ended 事件；主控看到 done 后立即停止轮询，尾事件永久丢失。
        let result = lock_recover(&entry.completion.result).clone();
        let events_guard = lock_recover(&entry.events);
        let from = cursor.min(events_guard.len());
        let events = events_guard[from..].to_vec();
        let next_cursor = events_guard.len();
        drop(events_guard);
        Ok(IperfClientStatusOut {
            id: id.to_string(),
            done: result.is_some(),
            next_cursor,
            events,
            result,
        })
    }

    pub fn elapsed_ms(&self, id: &str) -> Option<u64> {
        let registry = lock_recover(&self.inner);
        registry
            .jobs
            .get(id)
            .map(|entry| entry.started.elapsed().as_millis().min(u64::MAX as u128) as u64)
    }

    #[cfg(test)]
    pub fn stop(&self, id: &str) -> Result<(bool, bool), String> {
        let out = self.stop_checked(id, DEFAULT_CLIENT_STOP_WAIT)?;
        Ok((out.existed, out.was_done))
    }

    /// cancel 后等待 worker 返回并 join；成功即表示底层 client 子进程已经回收。
    pub fn stop_checked(&self, id: &str, wait: Duration) -> Result<IperfClientStopOut, String> {
        validate_lifecycle_id("client id", id)?;
        if id.is_empty() {
            return Err("client id 不能为空".into());
        }
        self.prune_client_tombstones();
        let job_lock = self.job_lock(id);
        let _job_guard = lock_recover(&job_lock);
        let entry = {
            let mut registry = lock_recover(&self.inner);
            if let Some(tombstone) = registry.tombstones.get(id) {
                return Ok(tombstone.out.clone());
            }
            let Some(entry) = registry.jobs.get(id).cloned() else {
                let out = IperfClientStopOut {
                    existed: false,
                    was_done: false,
                    terminated: true,
                    result: None,
                };
                registry.tombstones.insert(
                    id.to_string(),
                    ClientTombstone {
                        stopped_at: Instant::now(),
                        out: out.clone(),
                    },
                );
                return Ok(out);
            };
            entry
        };

        let was_done = lock_recover(&entry.completion.result).is_some();
        entry.cancel.store(true, Ordering::SeqCst);
        let deadline = Instant::now()
            .checked_add(wait)
            .ok_or_else(|| format!("client stop 等待时间 {} 秒过大", wait.as_secs()))?;
        wait_for_client_result(&entry, Some(deadline), id)?;
        join_client_thread(&entry, Some(deadline), id)?;
        let result = lock_recover(&entry.completion.result).clone();

        let out = IperfClientStopOut {
            existed: true,
            was_done,
            terminated: true,
            result,
        };
        let mut registry = lock_recover(&self.inner);
        if registry
            .jobs
            .get(id)
            .map(|current| Arc::ptr_eq(current, &entry))
            .unwrap_or(false)
        {
            registry.jobs.remove(id);
            registry.tombstones.insert(
                id.to_string(),
                ClientTombstone {
                    stopped_at: Instant::now(),
                    out: out.clone(),
                },
            );
        }
        Ok(registry
            .tombstones
            .get(id)
            .map(|tombstone| tombstone.out.clone())
            .unwrap_or(out))
    }

    pub fn stop_owner(&self, owner_id: &str, wait: Duration) -> LifecycleCleanupResult {
        let mut result = LifecycleCleanupResult::default();
        if owner_id.is_empty() {
            result.errors.push("owner_id 不能为空".into());
            return result;
        }
        if let Err(e) = validate_lifecycle_id("owner_id", owner_id) {
            result.errors.push(e);
            return result;
        }
        let targets: Vec<(String, Arc<ClientJobEntry>)> = {
            let registry = lock_recover(&self.inner);
            registry
                .jobs
                .iter()
                .filter(|(_, entry)| entry.owner_id == owner_id)
                .map(|(id, entry)| (id.clone(), Arc::clone(entry)))
                .collect()
        };
        // 先同时发出取消，再逐项等待；大量并发流异常清理时不会串行多等
        // 一个轮询周期，后续 stop_checked 只负责确认和 join。
        for (_, entry) in &targets {
            entry.cancel.store(true, Ordering::SeqCst);
        }
        let deadline = Instant::now().checked_add(wait);
        for (id, _) in targets {
            let remaining = remaining_until(deadline).unwrap_or(Duration::ZERO);
            match self.stop_checked(&id, remaining) {
                Ok(out) if out.existed && out.terminated => result.stopped += 1,
                Ok(_) => {}
                Err(e) => result.errors.push(format!("client job {id} 清理失败: {e}")),
            }
        }
        result
    }

    /// 主控退出前的最后兜底：同时取消仍登记的全部异步 client/外部作业，
    /// 再在同一总截止时间内逐项确认 worker 与子进程均已回收。
    pub fn stop_all(&self, wait: Duration) -> LifecycleCleanupResult {
        let mut result = LifecycleCleanupResult::default();
        let targets: Vec<(String, Arc<ClientJobEntry>)> = {
            let registry = lock_recover(&self.inner);
            registry
                .jobs
                .iter()
                .map(|(id, entry)| (id.clone(), Arc::clone(entry)))
                .collect()
        };
        for (_, entry) in &targets {
            entry.cancel.store(true, Ordering::SeqCst);
        }
        let deadline = Instant::now().checked_add(wait);
        for (id, _) in targets {
            let remaining = remaining_until(deadline).unwrap_or(Duration::ZERO);
            match self.stop_checked(&id, remaining) {
                Ok(out) if out.existed && out.terminated => result.stopped += 1,
                Ok(_) => {}
                Err(error) => result
                    .errors
                    .push(format!("client job {id} 最终清理失败: {error}")),
            }
        }
        result
    }

    /// 返回指定 owner 当前登记的 client/job ID，供统一资源清单快照使用。
    pub fn resource_ids_for_owner(&self, owner_id: &str) -> Vec<String> {
        let registry = lock_recover(&self.inner);
        let mut ids: Vec<String> = registry
            .jobs
            .iter()
            .filter(|(_, entry)| entry.owner_id == owner_id)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    pub fn sweep(&self, max_age: Duration) -> Vec<String> {
        self.prune_client_tombstones();
        let expired: Vec<String> = {
            let registry = lock_recover(&self.inner);
            registry
                .jobs
                .iter()
                .filter(|(_, entry)| {
                    if entry.dynamic_lease.load(Ordering::SeqCst) {
                        lock_recover(&entry.expires_at)
                            .map(|deadline| Instant::now() >= deadline)
                            .unwrap_or(false)
                    } else {
                        entry.started.elapsed() > max_age
                    }
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut errors = Vec::new();
        for id in expired {
            if let Err(e) = self.stop_checked(&id, DEFAULT_CLIENT_STOP_WAIT) {
                let message = format!("清理超龄 iperf client job {id} 失败: {e}");
                eprintln!("[iperf] {message}");
                errors.push(message);
            }
        }
        errors
    }
}

fn panic_message(value: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = value.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = value.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".into()
    }
}

fn remaining_until(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

fn wait_for_client_result(
    entry: &ClientJobEntry,
    deadline: Option<Instant>,
    id: &str,
) -> Result<(), String> {
    let mut result = lock_recover(&entry.completion.result);
    while result.is_none() {
        let remaining = remaining_until(deadline).unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return Err(format!("等待 iperf client job {id} 退出超时"));
        }
        let waited = entry.completion.cv.wait_timeout(result, remaining);
        let (next, timeout) = match waited {
            Ok(pair) => pair,
            Err(poisoned) => poisoned.into_inner(),
        };
        result = next;
        if timeout.timed_out() && result.is_none() {
            return Err(format!("等待 iperf client job {id} 退出超时"));
        }
    }
    Ok(())
}

fn join_client_thread(
    entry: &ClientJobEntry,
    deadline: Option<Instant>,
    id: &str,
) -> Result<(), String> {
    let handle = loop {
        let mut thread = lock_recover(&entry.thread);
        if thread.joined {
            return Ok(());
        }
        if thread.installed && !thread.joining {
            thread.joining = true;
            break thread.handle.take();
        }
        let remaining = remaining_until(deadline).unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return Err(format!("等待 iperf client job {id} worker 回收超时"));
        }
        let waited = entry.thread_cv.wait_timeout(thread, remaining);
        let (next, timeout) = match waited {
            Ok(pair) => pair,
            Err(poisoned) => poisoned.into_inner(),
        };
        if timeout.timed_out() && !next.joined && (!next.installed || next.joining) {
            return Err(format!("等待 iperf client job {id} worker 回收超时"));
        }
    };

    let join_error = handle.and_then(|handle| handle.join().err());
    let mut thread = lock_recover(&entry.thread);
    thread.joining = false;
    thread.joined = true;
    entry.thread_cv.notify_all();
    if let Some(panic_value) = join_error {
        Err(format!(
            "iperf client job {id} worker join 发现 panic: {}",
            panic_message(panic_value.as_ref())
        ))
    } else {
        Ok(())
    }
}
