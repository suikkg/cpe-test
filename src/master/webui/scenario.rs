//! 子网→内环组合场景。
//!
//! 组合场景只负责顺序和历史，不复制两套执行器：第一阶段调用现有 `/api/run`
//! 的同一条编译/RESUME 路径，第二阶段调用内环 Controller。场景清单保存两份
//! 原始配置，历史重跑时可以重新预览子网计划，再按各自的规则恢复。

use super::api;
use super::model::RunRequest;
use super::state::Console;
use crate::inner::config;
use crate::util::lock_recover;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ROOT: &str = "scenarios";

fn regular_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn resolve(id: &str) -> Option<PathBuf> {
    if id.is_empty() || !regular_dir(Path::new(ROOT)) {
        return None;
    }
    std::fs::read_dir(ROOT)
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

fn write_record(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} 不是普通文件", path.display()),
            ));
        }
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    std::fs::write(path, bytes)
}

fn update_manifest_phase(id: &str, phase: &str) {
    update_manifest_phase_at(&Path::new(ROOT).join(id).join("request.json"), phase);
}

fn update_manifest_phase_at(path: &Path, phase: &str) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    value["phase"] = Value::String(phase.into());
    if let Err(error) = write_record(path, &value) {
        eprintln!("!! 更新组合场景阶段失败：{error}");
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct Request {
    pub(super) subnet: RunRequest,
    pub(super) inner: Value,
    #[serde(default)]
    pub(super) resume_subnet: bool,
    #[serde(default)]
    pub(super) resume_inner: bool,
}

#[derive(Debug, Default, Serialize)]
struct State {
    running: bool,
    id: String,
    phase: String,
    error: Option<String>,
    subnet_run_id: String,
    inner_run_id: String,
}

#[derive(Default)]
pub(super) struct Controller {
    state: Arc<Mutex<State>>,
    cancel: Arc<AtomicBool>,
}

impl Controller {
    pub(super) fn is_running(&self) -> bool {
        lock_recover(&self.state).running
    }

    pub(super) fn status(&self) -> Value {
        serde_json::to_value(&*lock_recover(&self.state)).unwrap_or(Value::Null)
    }

    pub(super) fn stop(&self, console: &Arc<Console>) -> Value {
        // `/api/scenario/run` 在创建场景前持有同一把 gate。停止也要先拿它，
        // 否则请求可能恰好落在 start 已通过互斥检查、尚未把 state 标成 running
        // 的窗口，stop() 返回 false 后启动线程仍会继续执行。
        let _run_gate = lock_recover(&console.run_gate);
        let running = lock_recover(&self.state).running;
        if running {
            self.cancel.store(true, Ordering::SeqCst);
            if console.running.load(Ordering::SeqCst) {
                // 这里已经持有 run_gate，不能再调用 api_stop()；它会二次加锁。
                crate::cancel::request_cancel();
            } else if console.inner.is_running() {
                let _ = console.inner.stop();
            }
        }
        serde_json::json!({"stopping": running})
    }

    pub(super) fn start(&self, console: &Arc<Console>, body: &str) -> Result<Value, String> {
        let request: Request =
            serde_json::from_str(body).map_err(|e| format!("场景参数解析失败: {e}"))?;
        let inner_text = serde_json::to_string(&request.inner).map_err(|e| e.to_string())?;
        let mut inner_cfg = config::parse_config(&inner_text)?;
        inner_cfg.resume |= request.resume_inner;
        let mut subnet = request.subnet.clone();
        subnet.resume |= request.resume_subnet;
        let subnet_body =
            serde_json::to_string(&subnet).map_err(|e| format!("子网场景参数编码失败: {e}"))?;
        // `InnerConfig::token` 刻意 skip_serializing，不能拿序列化后的配置作为
        // 运行体：那份格式适合写 manifest，却会让第二阶段连远端辅测机时拿到空口令。
        // 运行体沿用已校验的原始请求，只在正确的层级写回 resume，因此保留令牌，
        // 同时仍让 manifest 走下面的脱敏序列化。
        let inner_body = runtime_inner_body(&request.inner, inner_cfg.resume)?;
        if self.is_running() || console.running.load(Ordering::SeqCst) || console.inner.is_running()
        {
            return Err("已有测试或组合场景正在运行".into());
        }
        // 子网阶段的“停止”和 Ctrl+C 共用当前测试取消位；它是单向信号，
        // 一轮结束后不会自动清零。场景线程在启动第一阶段前也会检查这枚位，
        // 所以必须在互斥检查通过、正式受理新场景时清掉上一轮的取消状态。
        // `reset()` 不会清除进程退出位，控制台正在退出时仍由上层门禁拒绝启动。
        crate::cancel::reset();

        std::fs::create_dir_all(ROOT).map_err(|e| format!("创建场景目录失败: {e}"))?;
        if !regular_dir(Path::new(ROOT)) {
            return Err("场景根目录不是普通目录".into());
        }
        let id = format!(
            "scenario_{}_{}_{}",
            crate::util::now_compact(),
            std::process::id(),
            chrono::Utc::now().timestamp_subsec_nanos()
        );
        let dir = PathBuf::from(ROOT).join(&id);
        std::fs::create_dir(&dir).map_err(|e| format!("创建场景记录失败: {e}"))?;
        let manifest = serde_json::json!({
            "id": id,
            "created_at": crate::util::now_full(),
            "finished": false,
            "phase": "subnet",
            "error": Value::Null,
            "resume_subnet": subnet.resume,
            "resume_inner": inner_cfg.resume,
            "request": { "subnet": subnet, "inner": inner_cfg }
        });
        write_record(&dir.join("request.json"), &manifest)
            .map_err(|e| format!("写入场景记录失败: {e}"))?;

        let mut state = lock_recover(&self.state);
        *state = State {
            running: true,
            id: id.clone(),
            phase: "subnet".into(),
            ..Default::default()
        };
        self.cancel.store(false, Ordering::SeqCst);
        let shared = Arc::clone(&self.state);
        let cancel = Arc::clone(&self.cancel);
        let console = Arc::clone(console);
        std::thread::Builder::new()
            .name("cpe-scenario".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if scenario_cancelled(&cancel) {
                        return Err("组合场景已取消".into());
                    }
                    if let Err(error) = api::api_run_for_scenario(&console, &subnet_body) {
                        cancel.store(true, Ordering::SeqCst);
                        return Err(error);
                    }
                    while console.running.load(Ordering::SeqCst) {
                        if scenario_cancelled(&cancel) {
                            let _ = api::api_stop(&console);
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    if scenario_cancelled(&cancel) {
                        return Err("组合场景已取消".into());
                    }
                    let subnet_status = console.run_status.snapshot(0, None).1;
                    if !subnet_stage_finished(&subnet_status) {
                        return Err("子网阶段未正常完成，未启动内环阶段；请查看子网进度日志".into());
                    }
                    if scenario_cancelled(&cancel) {
                        return Err("组合场景已取消".into());
                    }
                    let scenario_id = {
                        let mut state = lock_recover(&shared);
                        state.phase = "inner".into();
                        state.id.clone()
                    };
                    // 历史列表直接读 manifest；阶段切换不能只留在内存里，
                    // 否则一个长时间运行的内环阶段会在历史页上一直显示“子网”。
                    update_manifest_phase(&scenario_id, "inner");
                    if scenario_cancelled(&cancel) {
                        return Err("组合场景已取消".into());
                    }
                    console.inner.start(&inner_body)?;
                    // stop() 可能恰好落在上一个取消检查之后、inner.start() 之前：
                    // 那一刻子网和内环都还没在跑，stop() 没有可调用的执行器，
                    // 但当前线程随后仍会把内环拉起来。启动后再检查一次，并把
                    // 这条刚启动的内环收掉，才能让“停止”在阶段交界处也有确定语义。
                    if scenario_cancelled(&cancel) {
                        let _ = console.inner.stop();
                        return Err("组合场景已取消".into());
                    }
                    while console.inner.is_running() {
                        if scenario_cancelled(&cancel) {
                            let _ = console.inner.stop();
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    if scenario_cancelled(&cancel) {
                        return Err("组合场景已取消".into());
                    }
                    if let Some(error) = inner_stage_error(&console) {
                        return Err(format!("内环阶段失败：{error}"));
                    }
                    Ok::<(), String>(())
                }));
                let mut state = lock_recover(&shared);
                state.running = false;
                match result {
                    Ok(Ok(())) => state.phase = "finished".into(),
                    Ok(Err(error)) => {
                        state.phase = "failed".into();
                        state.error = Some(error);
                    }
                    Err(_) => {
                        state.phase = "failed".into();
                        state.error = Some("组合场景工作线程异常终止".into());
                    }
                }
                let path = Path::new(ROOT).join(&state.id).join("request.json");
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(mut value) = serde_json::from_str::<Value>(&text) {
                        value["finished"] = Value::Bool(true);
                        value["phase"] = Value::String(state.phase.clone());
                        value["error"] = state.error.clone().map_or(Value::Null, Value::String);
                        let _ = write_record(&path, &value);
                    }
                }
            })
            .map_err(|e| {
                let message = format!("启动组合场景线程失败: {e}");
                // 目录已经创建并写入 request.json；线程创建失败也要把这条
                // 记录收尾，否则历史页会永久显示“未收尾”，但控制器实际已
                // 回到 idle，下一次重跑也无法从记录状态判断发生了什么。
                if let Ok(text) = std::fs::read_to_string(dir.join("request.json")) {
                    if let Ok(mut value) = serde_json::from_str::<Value>(&text) {
                        value["finished"] = Value::Bool(true);
                        value["phase"] = Value::String("failed".into());
                        value["error"] = Value::String(message.clone());
                        let _ = write_record(&dir.join("request.json"), &value);
                    }
                }
                *state = State::default();
                message
            })?;
        Ok(serde_json::json!({"started": true, "id": id}))
    }

    pub(super) fn runs(&self) -> Result<Value, String> {
        let root = Path::new(ROOT);
        if !regular_dir(root) {
            return Ok(Value::Array(Vec::new()));
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(root)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(path) = regular_file(&entry.path(), "request.json") else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            out.push(serde_json::json!({
                "id": entry.file_name().to_string_lossy(),
                "created_at": value["created_at"],
                "finished": value["finished"],
                "phase": value["phase"],
                "error": value["error"],
                "resume_subnet": value["resume_subnet"],
                "resume_inner": value["resume_inner"],
            }));
        }
        out.sort_by(|a, b| b["id"].as_str().cmp(&a["id"].as_str()));
        Ok(Value::Array(out))
    }

    pub(super) fn request(&self, body: &str) -> Result<Value, String> {
        #[derive(Deserialize)]
        struct Id {
            id: String,
        }
        let id: Id = serde_json::from_str(body).map_err(|e| format!("参数解析失败: {e}"))?;
        if id.id.is_empty()
            || id.id.contains('/')
            || id.id.contains('\\')
            || id.id == "."
            || id.id == ".."
        {
            return Err("场景标识无效".into());
        }
        let dir = resolve(&id.id).ok_or_else(|| "找不到这个场景记录".to_string())?;
        let path =
            regular_file(&dir, "request.json").ok_or_else(|| "场景记录不是普通文件".to_string())?;
        let text = std::fs::read_to_string(path).map_err(|e| format!("读不到场景记录: {e}"))?;
        let value: Value = serde_json::from_str(&text).map_err(|e| format!("场景记录无效: {e}"))?;
        value
            .get("request")
            .cloned()
            .ok_or_else(|| "场景记录缺少原始请求".into())
    }
}

fn subnet_stage_finished(status: &crate::master::run_status::RunStatus) -> bool {
    // `run_finished` 也会在熔断后调用；仅看 finished 会把「剩余队列已中止」
    // 误当成成功，组合场景随后还会启动内环，掩盖真正的子网故障。
    status.finished && status.aborted_at_unit.is_none()
}

fn inner_stage_error(console: &Arc<Console>) -> Option<String> {
    inner_stage_error_status(&console.inner.status(0, None))
}

fn inner_stage_error_status(status: &Value) -> Option<String> {
    status
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn scenario_cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::SeqCst)
        || crate::cancel::is_cancelled()
        || crate::cancel::is_shutdown_requested()
}

fn runtime_inner_body(raw: &Value, resume: bool) -> Result<String, String> {
    let mut value = raw.clone();
    if value.get("kind").is_some() {
        value
            .get_mut("config")
            .and_then(Value::as_object_mut)
            .ok_or("内环场景配置缺少 config 对象")?
            .insert("resume".into(), Value::Bool(resume));
    } else {
        value
            .as_object_mut()
            .ok_or("内环场景配置必须是 JSON 对象")?
            .insert("resume".into(), Value::Bool(resume));
    }
    serde_json::to_string(&value).map_err(|e| format!("内环场景参数编码失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::{
        inner_stage_error_status, runtime_inner_body, subnet_stage_finished,
        update_manifest_phase_at,
    };
    use crate::master::run_status::RunStatus;
    use serde_json::json;

    #[test]
    fn runtime_body_keeps_agent_tokens_but_updates_nested_resume() {
        let raw = json!({
            "kind": "cpe-inner-project",
            "version": 2,
            "config": {
                "agents": [{"id": "agent1", "token": "private-token"}]
            }
        });
        let body: serde_json::Value =
            serde_json::from_str(&runtime_inner_body(&raw, true).unwrap()).unwrap();
        assert_eq!(body["config"]["agents"][0]["token"], "private-token");
        assert_eq!(body["config"]["resume"], true);
    }

    #[test]
    fn a_scenario_does_not_treat_an_unfinished_subnet_stage_as_success() {
        assert!(!subnet_stage_finished(&RunStatus::default()));
    }

    #[test]
    fn a_scenario_does_not_start_inner_after_subnet_circuit_breaker() {
        let status = RunStatus {
            finished: true,
            aborted_at_unit: Some(7),
            ..Default::default()
        };
        assert!(!subnet_stage_finished(&status));
    }

    #[test]
    fn a_scenario_surfaces_inner_stage_errors() {
        assert_eq!(inner_stage_error_status(&json!({"error": null})), None);
        assert_eq!(
            inner_stage_error_status(&json!({"error": "设备未就绪"})),
            Some("设备未就绪".into())
        );
    }

    #[test]
    fn a_scenario_persists_the_current_phase_for_history_listing() {
        let root = std::env::temp_dir().join(format!(
            "cpe_scenario_phase_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("request.json");
        std::fs::write(&path, r#"{"phase":"subnet","finished":false,"error":null}"#).unwrap();

        update_manifest_phase_at(&path, "inner");

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["phase"], "inner");
        assert_eq!(value["finished"], false);
        let _ = std::fs::remove_dir_all(root);
    }
}
