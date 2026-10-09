//! 独立内环控制器。原子网 Console 仅持有实例、鉴权后转发命名空间请求。
use super::*;
use serde_json::{json, Value};
use std::sync::Mutex;

fn regular_file(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn leg_json(leg: &LegRow) -> Value {
    json!({
        "flow": leg.flow,
        "port": leg.port,
        "receiver": leg.receiver,
        "receiver_host": leg.receiver_host,
        "counter_source": leg.counter_source,
        "source": leg.source,
        "mbps": leg.mbps,
        "target_mbps": leg.target_mbps,
        "fallback_reason": leg.fallback_reason,
        "verdict": leg.verdict,
        "reason": leg.reason,
        "detail": leg.detail,
        "diagnostics": leg.diagnostics,
        "nic_rx_mbps": leg.nic_rx_mbps,
        "nic_verdict": leg.nic_verdict,
        "nic_reason": leg.nic_reason,
        "nic_target_mbps": leg.nic_target_mbps,
        "coverage": leg.coverage,
        "effective_secs": leg.effective_secs,
        "required_secs": leg.required_secs,
        "tool_sender_mbps": leg.tool.sender_mbps,
        "tool_receiver_mbps": leg.tool.receiver_mbps,
        "tool_receiver_note": leg.tool.receiver_note,
        "udp_loss_pct": leg.tool.udp_loss_pct,
        "udp_lost_datagrams": leg.tool.udp_lost_datagrams,
        "udp_total_datagrams": leg.tool.udp_total_datagrams,
    })
}

/// 状态接口的一行 = 一个测试单元。双向的两条腿挂在同一行下面，页面不必
/// 自己拼「哪两行其实是一个单元」——那正是此前把顺序单向误当成双向的来源。
fn unit_json(unit: &UnitRow) -> Value {
    json!({
        "id": unit.id,
        "index": unit.index,
        "link": unit.link,
        "host": unit.host,
        "ip_version": unit.ip_version,
        "protocol": unit.protocol,
        "direction": unit.direction,
        "streams": unit.streams,
        "repeat": unit.repeat,
        "measurement": unit.measurement,
        "parameters": unit.parameters,
        "bidir_targets": unit.bidir_targets,
        "verdict": unit.verdict,
        "resumed": unit.resumed,
        "screenshot": unit.screenshot,
        "reason": unit.reason,
        "detail": unit.detail,
        "diagnostics": unit.diagnostics,
        "total_mbps": unit.total_mbps,
        "total_target_mbps": unit.total_target_mbps,
        "overlap_secs": unit.overlap_secs,
        "legs": unit.legs.iter().map(leg_json).collect::<Vec<_>>(),
    })
}

#[derive(Default)]
struct State {
    running: bool,
    run_id: String,
    current: String,
    error: Option<String>,
    path: Option<PathBuf>,
    units: Vec<Value>,
    total: usize,
}

#[derive(Default)]
pub(crate) struct Controller {
    state: Arc<Mutex<State>>,
    cancel: Arc<AtomicBool>,
}
impl Controller {
    #[cfg(test)]
    pub(crate) fn set_running_for_test(&self, running: bool) {
        crate::util::lock_recover(&self.state).running = running;
    }
    pub(crate) fn is_running(&self) -> bool {
        crate::util::lock_recover(&self.state).running
    }
    pub(crate) fn stop(&self) -> Value {
        let state = crate::util::lock_recover(&self.state);
        if state.running {
            self.cancel.store(true, Ordering::SeqCst);
        }
        json!({"stopping": state.running})
    }
    /// 运行状态。`units_from` 是游标，只回它之后新增的单元。
    ///
    /// 页面每秒问一次，而单元数只增不减：不带游标的话，同一份越来越大的数组
    /// 每秒重新序列化、重新传一遍，一轮几小时里一直如此，而且每次都在锁里做。
    /// 子网侧的 `/api/progress?from=` 早就是这个形状，内环这边把进度上报重写了
    /// 一遍却没带上它。
    ///
    /// 游标必须携带同一轮的 `run_id`；跨轮、缺少标识或游标越界时从 0 返回。
    /// 回包里带上 `units_from`：等于 0 说明这是整份（新一轮或首次拉取），
    /// 页面直接替换；大于 0 则是增量，页面往后接。
    pub(crate) fn status(&self, units_from: usize, run_id: Option<&str>) -> Value {
        let s = crate::util::lock_recover(&self.state);
        let from = if run_id == Some(s.run_id.as_str()) && units_from <= s.units.len() {
            units_from
        } else {
            0
        };
        json!({
            "running": s.running,
            "run_id": s.run_id,
            "current": s.current,
            "error": s.error,
            "completed": s.units.len(),
            "total": s.total,
            "units_from": from,
            "units": s.units[from..],
            "has_report": s
                .path
                .as_ref()
                .is_some_and(|p| regular_file(&p.join("report.html"))),
        })
    }
    /// 计划预览。页面、执行器和报告消费的是同一个 [`plan`] 模块，
    /// 不存在「预览说 8 个单元、实际跑 12 个」这种事。
    pub(crate) fn plan(&self, body: &str) -> Result<Value, String> {
        let cfg = config::parse_config(body)?;
        let resumed = cfg.resume.then(history::fresh_pass_ids).unwrap_or_default();
        serde_json::to_value(plan::preview_with_resumed(&cfg, &resumed)?).map_err(|e| e.to_string())
    }
    pub(crate) fn probe(&self, body: &str) -> Result<Value, String> {
        if self.is_running() {
            return Err("内环测试正在运行，完成后再检查设备".into());
        }
        let cfg = config::parse_config(body)?;
        let adb = Adb::connect(&cfg)?;
        // 页面上的扫描要看到所有已配置的电脑；连不上的只记状态，不让整次扫描失败。
        serde_json::to_value(super::probe(&adb, &cfg, None)?).map_err(|e| e.to_string())
    }
    pub(crate) fn start(&self, body: &str) -> Result<Value, String> {
        let cfg = config::parse_config(body)?;
        let resumed = cfg.resume.then(history::fresh_pass_ids).unwrap_or_default();
        let preview = plan::preview_with_resumed(&cfg, &resumed)?;
        if preview.units == 0 {
            return Err("本轮没有勾选任何链路，请至少勾选一条再开始".into());
        }
        let mut state = crate::util::lock_recover(&self.state);
        if state.running {
            return Err("已有内环测试正在运行".into());
        }
        *state = State {
            running: true,
            run_id: format!(
                "{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ),
            total: preview.units,
            current: "准备检查设备".into(),
            ..Default::default()
        };
        self.cancel.store(false, Ordering::SeqCst);
        let shared = Arc::clone(&self.state);
        let cancel = Arc::clone(&self.cancel);
        let spawned = std::thread::Builder::new()
            .name("inner-run".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    perform(cfg, false, &cancel, &|report, dir| {
                        let mut s = crate::util::lock_recover(&shared);
                        s.current = report.current.clone();
                        s.error = report.error.clone();
                        s.path = Some(dir.to_path_buf());
                        // 只补新的：整份重建是 O(N)，每单元一次就成了 O(N^2)，
                        // 而且发生在锁里，把每秒一次的 status 轮询一起拖住。
                        // 单元只增不减，已经序列化过的不会再变。
                        if report.units.len() < s.units.len() {
                            s.units.clear();
                        }
                        for unit in &report.units[s.units.len()..] {
                            s.units.push(unit_json(unit));
                        }
                    })
                }));
                let mut s = crate::util::lock_recover(&shared);
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => s.error = Some(e),
                    Err(_) => {
                        s.error = Some("内环工作线程异常终止，请检查板侧资源租约与日志".into())
                    }
                }
                s.running = false;
                s.current.clear();
            });
        if let Err(e) = spawned {
            // 整个 State 复位，不能只把 `running` 抹掉：上面已经把
            // `current`/`total` 填成「准备检查设备 / 0 之 N」了，只清 running 的话
            // 页面会永远停在「空闲 · 0 / 144 个单元」旁边挂着一句正在进行的提示，
            // 除非再起一轮，否则清不掉。
            *state = State::default();
            return Err(format!("启动内环线程失败: {e}"));
        }
        Ok(json!({"started":true}))
    }
    /// 内环历史，独立于子网的 runs/。
    pub(crate) fn runs(&self) -> Result<Value, String> {
        history::list()
    }
    pub(crate) fn run_report(&self, body: &str) -> Result<Value, String> {
        history::report(body)
    }
    /// 取回某一轮的配置装载回控制台。只回配置，不直接开跑——隔夜的网口拓扑
    /// 可能已经变了，该看到的是重新预览时的差异。
    pub(crate) fn run_config(&self, body: &str) -> Result<Value, String> {
        history::config(body)
    }
    pub(crate) fn report(&self) -> Result<Value, String> {
        // 先取出路径再放锁，**不要抱着锁去读文件**：report.html 内联了每条腿的
        // 完整客户端输出和裁剪后的 server 日志，动辄几 MB。跑到一半点「下载内环
        // 报告」时，这一读会把每单元一次的 observer 和每秒一次的 status 轮询
        // 全堵在同一把锁上。
        let path = {
            let state = crate::util::lock_recover(&self.state);
            state.path.clone().ok_or("还没有内环报告")?
        };
        let report = path.join("report.html");
        if !regular_file(&report) {
            return Err("当前内环报告不是普通文件".into());
        }
        let html = std::fs::read_to_string(report).map_err(|e| e.to_string())?;
        Ok(json!({"name":"cpe-inner-report.html","html":html}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_carries_actual_parameters_and_frozen_bidirectional_targets() {
        let mut row = crate::inner::tests::unit_row(1);
        row.parameters.tcp_window = Some("4m".into());
        row.bidir_targets = Some(super::super::plan::BidirTargets {
            nic_mbps: Some(1800.0),
            tool_mbps: Some(1700.0),
        });
        let data = unit_json(&row);
        assert_eq!(data["parameters"]["tcp_window"], "4m");
        assert_eq!(data["bidir_targets"]["nic_mbps"], 1800.0);
        assert_eq!(data["bidir_targets"]["tool_mbps"], 1700.0);
    }

    #[test]
    fn status_cursor_cannot_mix_runs_or_skip_missing_rows() {
        let controller = Controller::default();
        {
            let mut state = crate::util::lock_recover(&controller.state);
            state.run_id = "new-run".into();
            state.units = vec![json!({"index":1,"link":"new"}), json!({"index":2})];
        }
        for id in [None, Some("old-run")] {
            let status = controller.status(1, id);
            assert_eq!(status["units_from"], 0);
            assert_eq!(status["units"][0]["link"], "new");
            assert_eq!(status["run_id"], "new-run");
        }
        assert_eq!(controller.status(9, Some("new-run"))["units_from"], 0);
        let delta = controller.status(1, Some("new-run"));
        assert_eq!(delta["units_from"], 1);
        assert_eq!(delta["units"].as_array().unwrap().len(), 1);
        assert_eq!(delta["units"][0]["index"], 2);
    }

    #[test]
    fn inner_stop_is_local_to_the_controller_and_idle_stop_is_a_noop() {
        let a = Controller::default();
        let b = Controller::default();
        a.stop();
        assert!(!a.cancel.load(Ordering::SeqCst));
        a.set_running_for_test(true);
        a.stop();
        assert!(a.cancel.load(Ordering::SeqCst));
        assert!(!b.cancel.load(Ordering::SeqCst));
        assert!(!std::ptr::eq(
            a.cancel.as_ref(),
            crate::cancel::cancel_flag()
        ));
    }
}
