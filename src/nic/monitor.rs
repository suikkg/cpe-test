//! 网卡 RX/TX 连续监控。
//! Windows 用 GetIfTable2，macOS 用 netstat -ibn，Linux 用 sysfs。
//!
//! 独立连续监控模式：`run_continuous` 按可配置间隔采样，Ctrl+C 时输出
//! 平均/峰值并写 CSV（不依赖 agent/master 子网测试流程）。

use crate::protocol::{MonitorSample, MonitorStatusOut, MonitorStopOut};
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// 有效测量的最小阈值（Mbps），低于它视为没有流量
pub const MIN_VALID_RX_MBPS: f64 = 0.01;

/// 读某接口 RX/TX 累计字节。
pub fn read_counters(iface: &str) -> Result<(u64, u64), String> {
    #[cfg(windows)]
    {
        super::scan_windows::counters(iface)
    }
    #[cfg(target_os = "macos")]
    {
        counters_macos(iface)
    }
    #[cfg(target_os = "linux")]
    {
        let base = std::path::Path::new("/sys/class/net")
            .join(iface)
            .join("statistics");
        let rx = std::fs::read_to_string(base.join("rx_bytes"))
            .map_err(|e| format!("读取 {iface} rx_bytes 失败: {e}"))?
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("解析 {iface} rx_bytes 失败: {e}"))?;
        let tx = std::fs::read_to_string(base.join("tx_bytes"))
            .map_err(|e| format!("读取 {iface} tx_bytes 失败: {e}"))?
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("解析 {iface} tx_bytes 失败: {e}"))?;
        Ok((rx, tx))
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = iface;
        Err("平台不支持网卡计数器".into())
    }
}

pub fn read_rx_bytes(iface: &str) -> Result<u64, String> {
    read_counters(iface).map(|(rx, _)| rx)
}

#[cfg(target_os = "macos")]
fn counters_macos(iface: &str) -> Result<(u64, u64), String> {
    use crate::util::run_cmd;
    use std::time::Duration;
    let out = run_cmd("netstat", &["-ibn"], Duration::from_secs(10));
    parse_netstat_counters(&out.stdout, iface)
}

/// netstat -ibn 的 <Link#N> 行含全接口计数；
/// 列可能因 Address 空缺而移位，取尾部固定位置：
/// ... Ipkts Ierrs Ibytes Opkts Oerrs Obytes Coll => Ibytes = cols[len-5]
#[cfg(test)]
pub fn parse_netstat_ib(text: &str, iface: &str) -> Result<u64, String> {
    parse_netstat_counters(text, iface).map(|(rx, _)| rx)
}

#[cfg(any(target_os = "macos", test))]
pub fn parse_netstat_counters(text: &str, iface: &str) -> Result<(u64, u64), String> {
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 8 {
            continue;
        }
        if cols[0] != iface || !line.contains("<Link#") {
            continue;
        }
        let rx_idx = cols.len() - 5;
        let tx_idx = cols.len() - 2;
        let rx = cols[rx_idx]
            .parse::<u64>()
            .map_err(|_| format!("解析 Ibytes 失败: {line}"));
        let tx = cols[tx_idx]
            .parse::<u64>()
            .map_err(|_| format!("解析 Obytes 失败: {line}"));
        return Ok((rx?, tx?));
    }
    Err(format!("netstat 输出中找不到接口 {iface}"))
}

struct StopSignal {
    requested_at: Mutex<Option<Instant>>,
    wake: Condvar,
}

impl StopSignal {
    fn new() -> Self {
        Self {
            requested_at: Mutex::new(None),
            wake: Condvar::new(),
        }
    }

    fn request_stop(&self, now: Instant) -> Instant {
        let mut state = self.requested_at.lock().unwrap_or_else(|e| e.into_inner());
        let requested_at = *state.get_or_insert(now);
        drop(state);
        self.wake.notify_all();
        requested_at
    }

    /// 等待下一个采样周期；被停止请求唤醒时返回报告截止时刻。
    fn wait_timeout(&self, timeout: Duration) -> Option<Instant> {
        let requested_at = self.requested_at.lock().unwrap_or_else(|e| e.into_inner());
        if requested_at.is_some() {
            return *requested_at;
        }
        let (requested_at, _) = self
            .wake
            .wait_timeout_while(requested_at, timeout, |requested_at| requested_at.is_none())
            .unwrap_or_else(|e| e.into_inner());
        *requested_at
    }

    fn requested_at(&self) -> Option<Instant> {
        *self.requested_at.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct CounterState {
    rx: u64,
    tx: u64,
    /// 字节差的基准时刻，只能在成功读取计数器后推进。
    baseline_at: Instant,
    /// 最近一次读取尝试的时刻，用于描述失败样本自身的采样间隔。
    attempted_at: Instant,
}

struct MonitorLoopContext {
    stop: Arc<StopSignal>,
    samples: Arc<Mutex<Vec<MonitorSample>>>,
    errors: Arc<Mutex<Vec<String>>>,
}

fn millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

/// 将一次计数器读取转换为样本。
///
/// 读取失败时只推进 `attempted_at`，保留字节和 `baseline_at`。这样恢复读取
/// 的字节差与时间差覆盖相同的完整区间，不会把多个周期的流量除以单个周期。
fn record_counter_result(
    result: Result<(u64, u64), String>,
    now: Instant,
    t0: Instant,
    state: &mut CounterState,
) -> (MonitorSample, Option<String>) {
    let elapsed_ms = millis_u64(now.duration_since(t0));
    match result {
        Ok((rx, tx)) => {
            let measured = now.duration_since(state.baseline_at);
            let dt = measured.as_secs_f64().max(0.001);
            let counters_ok = rx >= state.rx && tx >= state.tx;
            let rx_delta = if counters_ok { rx - state.rx } else { 0 };
            let tx_delta = if counters_ok { tx - state.tx } else { 0 };
            let error = if counters_ok {
                String::new()
            } else {
                format!(
                    "接口计数器回退/reset: RX {}->{}, TX {}->{}",
                    state.rx, rx, state.tx, tx
                )
            };

            // 即使计数器发生 reset，本次读取仍可作为下一次采样的新基准。
            state.rx = rx;
            state.tx = tx;
            state.baseline_at = now;
            state.attempted_at = now;

            let reported_error = (!error.is_empty()).then(|| error.clone());
            (
                MonitorSample {
                    elapsed_ms,
                    interval_ms: millis_u64(measured).max(1),
                    rx_bytes: rx,
                    tx_bytes: tx,
                    rx_delta_bytes: rx_delta,
                    tx_delta_bytes: tx_delta,
                    rx_mbps: rx_delta as f64 * 8.0 / dt / 1_000_000.0,
                    tx_mbps: tx_delta as f64 * 8.0 / dt / 1_000_000.0,
                    valid: counters_ok,
                    error,
                },
                reported_error,
            )
        }
        Err(error) => {
            let attempted_interval = now.duration_since(state.attempted_at);
            state.attempted_at = now;
            (
                MonitorSample {
                    elapsed_ms,
                    interval_ms: millis_u64(attempted_interval).max(1),
                    valid: false,
                    error: error.clone(),
                    ..Default::default()
                },
                Some(error),
            )
        }
    }
}

fn run_monitor_loop<F>(
    context: MonitorLoopContext,
    start_rx: u64,
    start_tx: u64,
    t0: Instant,
    interval: Duration,
    mut reader: F,
) where
    F: FnMut() -> Result<(u64, u64), String>,
{
    let mut state = CounterState {
        rx: start_rx,
        tx: start_tx,
        baseline_at: t0,
        attempted_at: t0,
    };

    loop {
        let stop_on_wake = context.stop.wait_timeout(interval);
        // 停止唤醒也进行一次读取，结算尚未满一个周期的最后部分区间。
        let result = reader();
        let observed_at = Instant::now();
        let stop_after_read = stop_on_wake.or_else(|| context.stop.requested_at());
        // 终采样以 stop 请求时刻为截止，读取计数器和 join 的开销不进入样本时长。
        let sample_at = stop_after_read
            .filter(|requested_at| *requested_at <= observed_at)
            .unwrap_or(observed_at);
        let (sample, error) = record_counter_result(result, sample_at, t0, &mut state);
        if let Some(error) = error {
            if let Ok(mut errors) = context.errors.lock() {
                errors.push(error);
            }
        }
        if let Ok(mut samples) = context.samples.lock() {
            samples.push(sample);
        }

        // 停止发生在本次读取完成之前时，当前读取就是终采样。若停止恰好
        // 发生在读取完成之后，则再循环一次，由已置位信号触发真正的终采样。
        let requested_at = stop_after_read.or_else(|| context.stop.requested_at());
        if requested_at.is_some_and(|requested_at| requested_at <= observed_at) {
            break;
        }
    }
}

struct MonEntry {
    iface: String,
    /// 资源归属。空字符串表示旧调用点创建的无 owner 监控。
    owner_id: String,
    /// 此 entry 自身的租约；0 表示由 `sweep` 传入的 legacy 上限决定。
    lease_secs: u64,
    start_rx: u64,
    start_tx: u64,
    t0: Instant,
    stop: Arc<StopSignal>,
    samples: Arc<Mutex<Vec<MonitorSample>>>,
    errors: Arc<Mutex<Vec<String>>>,
    handle: Option<JoinHandle<()>>,
}

/// RX/TX 连续监控注册表（agent 端与主控本地共用）
pub struct MonitorMgr {
    inner: Mutex<HashMap<String, MonEntry>>,
    seq: AtomicU64,
}

impl Default for MonitorMgr {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorMgr {
    pub fn new() -> Self {
        MonitorMgr {
            inner: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
        }
    }

    /// 旧调用点兼容包装；自动化路径使用带 owner/lease 的 start_owned。
    #[allow(dead_code)]
    pub fn start(&self, iface: &str, interval_ms: u64) -> Result<String, String> {
        self.start_owned(iface, interval_ms, "", 0)
    }

    /// 启动带资源归属和动态租约的连续监控。
    ///
    /// `owner_id` 用于一个测试单元结束或异常退出时批量回收监控；`lease_secs`
    /// 为 0 时保持旧行为，由 agent 调用 `sweep` 时传入的 legacy 上限兜底。
    pub fn start_owned(
        &self,
        iface: &str,
        interval_ms: u64,
        owner_id: &str,
        lease_secs: u64,
    ) -> Result<String, String> {
        if lease_secs > 0
            && Instant::now()
                .checked_add(Duration::from_secs(lease_secs))
                .is_none()
        {
            return Err(format!(
                "monitor lease_secs={lease_secs} 过大，无法表示截止时间"
            ));
        }
        let (start_rx, start_tx) = read_counters(iface)?;
        let interval_ms = interval_ms.clamp(200, 5_000);
        let id = format!("mon{}", self.seq.fetch_add(1, Ordering::SeqCst));
        let stop = Arc::new(StopSignal::new());
        let samples = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let stop_thread = Arc::clone(&stop);
        let samples_thread = Arc::clone(&samples);
        let errors_thread = Arc::clone(&errors);
        let iface_thread = iface.to_string();
        let t0 = Instant::now();
        let handle = std::thread::spawn(move || {
            run_monitor_loop(
                MonitorLoopContext {
                    stop: stop_thread,
                    samples: samples_thread,
                    errors: errors_thread,
                },
                start_rx,
                start_tx,
                t0,
                Duration::from_millis(interval_ms),
                || read_counters(&iface_thread),
            );
        });
        self.inner.lock().unwrap().insert(
            id.clone(),
            MonEntry {
                iface: iface.to_string(),
                owner_id: owner_id.to_string(),
                lease_secs,
                start_rx,
                start_tx,
                t0,
                stop,
                samples,
                errors,
                handle: Some(handle),
            },
        );
        Ok(id)
    }

    /// 返回运行中 monitor 的最近一次样本，不停止采样线程。
    ///
    /// `latest_sample` 直接来自 OS 累计字节计数器的相邻差值，可用于实时日志；
    /// 完整统计和覆盖率仍应以 `stop` 返回的全部样本为准。
    pub fn status(&self, id: &str) -> Result<MonitorStatusOut, String> {
        let entries = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries
            .get(id)
            .ok_or_else(|| format!("监控 ID 不存在: {id}"))?;
        let samples = entry.samples.lock().unwrap_or_else(|e| e.into_inner());
        let errors = entry.errors.lock().unwrap_or_else(|e| e.into_inner());
        Ok(MonitorStatusOut {
            id: id.to_string(),
            iface: entry.iface.clone(),
            sample_count: samples.len(),
            latest_sample: samples.last().cloned(),
            error_count: errors.len(),
            latest_error: errors.last().cloned().unwrap_or_default(),
        })
    }

    pub fn stop(&self, id: &str) -> Result<MonitorStopOut, String> {
        let mut e = self
            .inner
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| format!("监控 ID 不存在: {id}"))?;
        // 以调用 stop 的时刻作为报告截止点，终采样读取和 join 的开销不计入时长。
        let stopped_at = e.stop.request_stop(Instant::now());
        let thread_panicked = e.handle.take().is_some_and(|handle| handle.join().is_err());
        let secs = stopped_at.duration_since(e.t0).as_secs_f64().max(0.001);
        let samples = e.samples.lock().unwrap().clone();
        let mut errors = e.errors.lock().unwrap().clone();
        if thread_panicked {
            errors.push(format!("{} 采样线程异常退出", e.iface));
        }
        let mut rx_delta: u64 = samples
            .iter()
            .filter(|s| s.valid)
            .map(|s| s.rx_delta_bytes)
            .sum();
        let mut tx_delta: u64 = samples
            .iter()
            .filter(|s| s.valid)
            .map(|s| s.tx_delta_bytes)
            .sum();
        // 正常路径由线程终采样结算；仅在线程未产出任何样本时兜底直读。
        if samples.is_empty() {
            match read_counters(&e.iface) {
                Ok((rx, tx)) => {
                    rx_delta = rx.saturating_sub(e.start_rx);
                    tx_delta = tx.saturating_sub(e.start_tx);
                }
                Err(error) => {
                    errors.push(format!("{} 停止时无法读取计数器: {error}", e.iface));
                }
            }
        }
        Ok(MonitorStopOut {
            avg_mbps: rx_delta as f64 * 8.0 / secs / 1_000_000.0,
            tx_avg_mbps: tx_delta as f64 * 8.0 / secs / 1_000_000.0,
            seconds: secs,
            bytes: rx_delta,
            tx_bytes: tx_delta,
            samples,
            errors,
        })
    }

    /// 停止指定 owner 的全部监控。
    ///
    /// 先同时唤醒全部匹配监控，再逐项 join/结算，因此多个端点不会串行各等
    /// 一个采样周期；一个 entry 的失败也不会阻止其余资源回收。
    pub fn stop_owner(&self, owner_id: &str) -> Vec<(String, Result<MonitorStopOut, String>)> {
        let mut targets: Vec<(String, Arc<StopSignal>)> = {
            let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.iter()
                .filter(|(_, entry)| entry.owner_id == owner_id)
                .map(|(id, entry)| (id.clone(), Arc::clone(&entry.stop)))
                .collect()
        };
        targets.sort_by(|(left, _), (right, _)| left.cmp(right));
        let stopped_at = Instant::now();
        for (_, stop) in &targets {
            stop.request_stop(stopped_at);
        }
        targets
            .into_iter()
            .map(|(id, _)| id)
            .map(|id| {
                let result = self.stop(&id);
                (id, result)
            })
            .collect()
    }

    /// 清理租约过期的监控。
    ///
    /// 新 entry 使用自身 `lease_secs`；旧调用点以 0 表示未设置租约，继续使用
    /// 调用方传入的 `legacy_max_age`，避免固定短 TTL 误杀合法的长时间测试。
    pub fn sweep(&self, legacy_max_age: std::time::Duration) {
        let expired: Vec<String> = {
            let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.iter()
                .filter(|(_, e)| {
                    let max_age = if e.lease_secs == 0 {
                        legacy_max_age
                    } else {
                        Duration::from_secs(e.lease_secs)
                    };
                    e.t0.elapsed() >= max_age
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in expired {
            let _ = self.stop(&id);
        }
    }
}

// ---------------- 独立连续监控（不依赖 agent/master 子网测试流程） ----------------

pub struct ContinuousOpts<'a> {
    pub iface: &'a str,
    pub interval_secs: u64,
    pub duration_secs: u64,
    pub csv_path: Option<&'a str>,
}

pub fn run_continuous(opts: &ContinuousOpts) -> Result<(), String> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .map_err(|e| format!("设置 Ctrl+C 处理器失败: {e}"))?;

    let t_start = Instant::now();
    let mut old_bytes = read_rx_bytes(opts.iface)?;
    let mut old_time = t_start;
    let wait = Duration::from_secs(opts.interval_secs);
    let mut records: Vec<(String, f64)> = Vec::new();

    if let Some(p) = opts.csv_path {
        if !std::path::Path::new(p).exists() {
            std::fs::write(p, "\u{FEFF}Time,Speed(Mbps)\n")
                .map_err(|e| format!("创建CSV失败: {e}"))?;
        }
    }

    println!(
        "\n网卡: [{}]  间隔: {}s  按 Ctrl+C 停止\n",
        opts.iface, opts.interval_secs
    );
    println!("{:<12} {:>12}", "时间", "速率(Mbps)");
    println!("{}", "-".repeat(26));

    loop {
        std::thread::sleep(wait);

        if opts.duration_secs > 0 && t_start.elapsed().as_secs() >= opts.duration_secs {
            running.store(false, Ordering::SeqCst);
        }
        if !running.load(Ordering::SeqCst) {
            break;
        }

        match read_rx_bytes(opts.iface) {
            Ok(new_bytes) => {
                let now = Instant::now();
                let dt = (now - old_time).as_secs_f64().max(0.001);
                let delta = new_bytes.saturating_sub(old_bytes);
                let mbps = delta as f64 * 8.0 / dt / 1_000_000.0;

                let t = chrono::Local::now().format("%H:%M:%S").to_string();
                println!("{:<12} {:>12.2}", t, mbps);
                records.push((t.clone(), mbps));

                if let Some(p) = opts.csv_path {
                    let mut f = std::fs::OpenOptions::new()
                        .append(true)
                        .open(p)
                        .map_err(|e| format!("打开CSV失败: {e}"))?;
                    writeln!(f, "{},{:.2}", t, mbps).map_err(|e| format!("写入CSV失败: {e}"))?;
                }

                old_bytes = new_bytes;
                old_time = now;
            }
            Err(e) => eprintln!("读取网卡数据失败: {e}"),
        }
    }

    if records.is_empty() {
        println!("\n未捕获到数据");
        return Ok(());
    }

    let speeds: Vec<f64> = records.iter().map(|r| r.1).collect();
    let avg = speeds.iter().sum::<f64>() / speeds.len() as f64;
    let max = speeds.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));
    let min = speeds.iter().fold(f64::INFINITY, |a, &b| a.min(b));
    let elapsed = t_start.elapsed().as_secs();

    println!("\n{}", "=".repeat(50));
    println!("网卡: {}", opts.iface);
    println!("时长: {}s ({} 次采样)", elapsed, records.len());
    println!("平均: {:.2} Mbps", avg);
    println!("峰值: {:.2} Mbps", max);
    println!("最低: {:.2} Mbps", min);

    if let Some(p) = opts.csv_path {
        rewrite_csv_with_header(
            p,
            opts.iface,
            opts.interval_secs,
            elapsed,
            avg,
            max,
            &records,
        )?;
        println!("CSV : {}", p);
    }

    Ok(())
}

fn rewrite_csv_with_header(
    path: &str,
    iface: &str,
    interval: u64,
    duration: u64,
    avg: f64,
    max: f64,
    records: &[(String, f64)],
) -> Result<(), String> {
    let mut f = std::fs::File::create(path).map_err(|e| format!("重写CSV失败: {e}"))?;
    writeln!(f, "\u{FEFF}# === CPE NIC Monitor Report ===").map_err(|e| format!("{e}"))?;
    writeln!(f, "# Interface,{}", iface).map_err(|e| format!("{e}"))?;
    writeln!(f, "# Interval,{}s", interval).map_err(|e| format!("{e}"))?;
    writeln!(f, "# Duration,{}s", duration).map_err(|e| format!("{e}"))?;
    writeln!(f, "# Average (Mbps),{:.2}", avg).map_err(|e| format!("{e}"))?;
    writeln!(f, "# Peak (Mbps),{:.2}", max).map_err(|e| format!("{e}"))?;
    writeln!(f, "# ================================").map_err(|e| format!("{e}"))?;
    writeln!(f, "Time,Speed(Mbps)").map_err(|e| format!("{e}"))?;
    for (t, s) in records {
        writeln!(f, "{},{:.2}", t, s).map_err(|e| format!("{e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_entry(owner_id: &str, lease_secs: u64, age: Duration) -> MonEntry {
        let t0 = Instant::now() - age;
        MonEntry {
            iface: "fake0".into(),
            owner_id: owner_id.into(),
            lease_secs,
            start_rx: 1_000,
            start_tx: 2_000,
            t0,
            stop: Arc::new(StopSignal::new()),
            samples: Arc::new(Mutex::new(vec![MonitorSample {
                elapsed_ms: millis_u64(age),
                interval_ms: millis_u64(age).max(1),
                rx_bytes: 1_100,
                tx_bytes: 2_200,
                rx_delta_bytes: 100,
                tx_delta_bytes: 200,
                valid: true,
                ..Default::default()
            }])),
            errors: Arc::new(Mutex::new(Vec::new())),
            handle: None,
        }
    }

    const NETSTAT: &str = r#"Name       Mtu   Network       Address            Ipkts Ierrs     Ibytes    Opkts Oerrs     Obytes  Coll
lo0        16384 <Link#1>                         269362     0   88650008   269362     0   88650008     0
lo0        16384 127           127.0.0.1          269362     -   88650008   269362     -   88650008     -
en0        1500  <Link#14>   aa:bb:cc:dd:ee:ff   9219567     0 9083840014  5296269     0  749169011     0
en0        1500  192.168.8     192.168.8.100     9219567     - 9083840014  5296269     -  749169011     -
"#;

    #[test]
    fn test_parse_netstat() {
        assert_eq!(parse_netstat_ib(NETSTAT, "en0").unwrap(), 9083840014);
        assert_eq!(parse_netstat_ib(NETSTAT, "lo0").unwrap(), 88650008);
        assert_eq!(
            parse_netstat_counters(NETSTAT, "en0").unwrap(),
            (9083840014, 749169011)
        );
        assert_eq!(
            parse_netstat_counters(NETSTAT, "lo0").unwrap(),
            (88650008, 88650008)
        );
        assert!(parse_netstat_ib(NETSTAT, "en9").is_err());
    }

    #[test]
    fn stop_signal_interrupts_a_long_sampling_wait() {
        let stop = Arc::new(StopSignal::new());
        let thread_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let started = Instant::now();
            let stopped = thread_stop.wait_timeout(Duration::from_secs(30));
            (stopped, started.elapsed())
        });

        ready_rx.recv().unwrap();
        stop.request_stop(Instant::now());
        let (stopped, elapsed) = handle.join().unwrap();

        assert!(stopped.is_some());
        assert!(
            elapsed < Duration::from_secs(2),
            "停止信号未及时中断等待: {elapsed:?}"
        );
    }

    #[test]
    fn stopped_monitor_records_exactly_one_final_partial_sample() {
        let stop = Arc::new(StopSignal::new());
        let samples = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let t0 = Instant::now() - Duration::from_millis(250);
        let stopped_at = stop.request_stop(Instant::now());
        let mut reader_calls = 0;

        run_monitor_loop(
            MonitorLoopContext {
                stop,
                samples: Arc::clone(&samples),
                errors: Arc::clone(&errors),
            },
            1_000,
            2_000,
            t0,
            Duration::from_secs(30),
            || {
                reader_calls += 1;
                Ok((1_250, 2_500))
            },
        );

        let samples = samples.lock().unwrap();
        assert_eq!(reader_calls, 1, "停止时不应先多做一次周期采样");
        assert_eq!(samples.len(), 1);
        assert!(samples[0].valid);
        assert_eq!(
            samples[0].elapsed_ms,
            millis_u64(stopped_at.duration_since(t0)),
            "终采样时间应固定在 stop 请求时刻"
        );
        assert_eq!(samples[0].rx_delta_bytes, 250);
        assert_eq!(samples[0].tx_delta_bytes, 500);
        assert!(samples[0].interval_ms >= 200);
        assert!(samples[0].interval_ms < 1_000);
        assert!(errors.lock().unwrap().is_empty());
    }

    #[test]
    fn recovery_sample_uses_last_successful_counter_and_time() {
        let t0 = Instant::now();
        let mut state = CounterState {
            rx: 1_000_000,
            tx: 2_000_000,
            baseline_at: t0,
            attempted_at: t0,
        };

        let (failed, error) = record_counter_result(
            Err("temporary read error".to_string()),
            t0 + Duration::from_secs(1),
            t0,
            &mut state,
        );
        assert!(!failed.valid);
        assert_eq!(failed.interval_ms, 1_000);
        assert_eq!(error.as_deref(), Some("temporary read error"));
        assert_eq!(state.rx, 1_000_000);
        assert_eq!(state.tx, 2_000_000);
        assert_eq!(state.baseline_at, t0);
        assert_eq!(state.attempted_at, t0 + Duration::from_secs(1));

        let (recovered, error) = record_counter_result(
            Ok((1_200_000, 2_100_000)),
            t0 + Duration::from_secs(2),
            t0,
            &mut state,
        );
        assert!(error.is_none());
        assert!(recovered.valid);
        assert_eq!(recovered.interval_ms, 2_000);
        assert_eq!(recovered.rx_delta_bytes, 200_000);
        assert_eq!(recovered.tx_delta_bytes, 100_000);
        assert!((recovered.rx_mbps - 0.8).abs() < 1e-12);
        assert!((recovered.tx_mbps - 0.4).abs() < 1e-12);
    }

    #[test]
    fn stop_owner_stops_only_matching_entries_and_returns_each_result() {
        let mgr = MonitorMgr::new();
        {
            let mut g = mgr.inner.lock().unwrap();
            g.insert(
                "mon-owner-a-2".into(),
                fake_entry("owner-a", 60, Duration::from_secs(2)),
            );
            g.insert(
                "mon-owner-b".into(),
                fake_entry("owner-b", 60, Duration::from_secs(2)),
            );
            g.insert(
                "mon-owner-a-1".into(),
                fake_entry("owner-a", 60, Duration::from_secs(2)),
            );
        }

        let stopped = mgr.stop_owner("owner-a");
        assert_eq!(
            stopped
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["mon-owner-a-1", "mon-owner-a-2"]
        );
        assert!(stopped.iter().all(|(_, result)| result.is_ok()));

        let g = mgr.inner.lock().unwrap();
        assert_eq!(g.len(), 1);
        assert!(g.contains_key("mon-owner-b"));
    }

    #[test]
    fn sweep_uses_entry_lease_and_legacy_fallback_independently() {
        let mgr = MonitorMgr::new();
        {
            let mut g = mgr.inner.lock().unwrap();
            g.insert(
                "dynamic-expired".into(),
                fake_entry("dynamic", 1, Duration::from_secs(2)),
            );
            g.insert(
                "dynamic-live".into(),
                fake_entry("dynamic", 60, Duration::from_secs(2)),
            );
            g.insert(
                "legacy-live".into(),
                fake_entry("", 0, Duration::from_secs(2)),
            );
            g.insert(
                "legacy-expired".into(),
                fake_entry("", 0, Duration::from_secs(20)),
            );
        }

        mgr.sweep(Duration::from_secs(10));

        let g = mgr.inner.lock().unwrap();
        assert!(!g.contains_key("dynamic-expired"));
        assert!(g.contains_key("dynamic-live"));
        assert!(g.contains_key("legacy-live"));
        assert!(!g.contains_key("legacy-expired"));
    }

    #[test]
    fn monitor_rejects_unrepresentable_dynamic_lease_before_touching_interface() {
        let mgr = MonitorMgr::new();
        let error = mgr
            .start_owned("interface-must-not-be-read", 1_000, "owner", u64::MAX)
            .unwrap_err();
        assert!(error.contains("lease_secs"));
        assert!(error.contains("过大"));
    }

    #[test]
    fn status_returns_latest_sample_without_stopping_monitor() {
        let mgr = MonitorMgr::new();
        {
            let mut g = mgr.inner.lock().unwrap();
            g.insert(
                "mon-live".into(),
                fake_entry("owner-live", 60, Duration::from_secs(2)),
            );
        }

        let status = mgr.status("mon-live").unwrap();
        assert_eq!(status.id, "mon-live");
        assert_eq!(status.iface, "fake0");
        assert_eq!(status.sample_count, 1);
        let latest = status.latest_sample.unwrap();
        assert!(latest.valid);
        assert_eq!(latest.rx_delta_bytes, 100);
        assert_eq!(latest.tx_delta_bytes, 200);
        assert!(mgr.inner.lock().unwrap().contains_key("mon-live"));

        assert!(mgr.status("missing-monitor").is_err());
    }
}
