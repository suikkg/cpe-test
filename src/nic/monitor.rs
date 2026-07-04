//! 接收端网卡 RX 字节监控：start 记起点，stop 记终点，算平均 Mbps。
//! Windows 用 GetIfTable2（不丢包、比 psutil 准），macOS 用 netstat -ibn。

use crate::protocol::MonitorStopOut;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// 有效测量的最小阈值（Mbps），低于它视为没有流量
pub const MIN_VALID_RX_MBPS: f64 = 0.01;

/// 读某接口 RX 累计字节
pub fn read_rx_bytes(iface: &str) -> Result<u64, String> {
    #[cfg(windows)]
    {
        super::scan_windows::rx_bytes(iface)
    }
    #[cfg(target_os = "macos")]
    {
        rx_bytes_macos(iface)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = iface;
        Err("平台不支持".into())
    }
}

#[cfg(target_os = "macos")]
fn rx_bytes_macos(iface: &str) -> Result<u64, String> {
    use crate::util::run_cmd;
    use std::time::Duration;
    let out = run_cmd("netstat", &["-ibn"], Duration::from_secs(10));
    parse_netstat_ib(&out.stdout, iface)
}

/// netstat -ibn 的 <Link#N> 行含全接口计数；
/// 列可能因 Address 空缺而移位，取尾部固定位置：
/// ... Ipkts Ierrs Ibytes Opkts Oerrs Obytes Coll => Ibytes = cols[len-5]
#[cfg(any(target_os = "macos", test))]
pub fn parse_netstat_ib(text: &str, iface: &str) -> Result<u64, String> {
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 8 {
            continue;
        }
        if cols[0] != iface || !line.contains("<Link#") {
            continue;
        }
        let idx = cols.len() - 5;
        return cols[idx]
            .parse::<u64>()
            .map_err(|_| format!("解析 Ibytes 失败: {line}"));
    }
    Err(format!("netstat 输出中找不到接口 {iface}"))
}

struct MonEntry {
    iface: String,
    start_bytes: u64,
    t0: Instant,
}

/// RX 监控注册表（agent 端与主控本地共用）
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

    pub fn start(&self, iface: &str) -> Result<String, String> {
        let bytes = read_rx_bytes(iface)?;
        let id = format!("mon{}", self.seq.fetch_add(1, Ordering::SeqCst));
        self.inner.lock().unwrap().insert(
            id.clone(),
            MonEntry {
                iface: iface.to_string(),
                start_bytes: bytes,
                t0: Instant::now(),
            },
        );
        Ok(id)
    }

    pub fn stop(&self, id: &str) -> Result<MonitorStopOut, String> {
        let e = self
            .inner
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| format!("监控 ID 不存在: {id}"))?;
        let end = read_rx_bytes(&e.iface)?;
        let secs = e.t0.elapsed().as_secs_f64().max(0.001);
        let delta = end.saturating_sub(e.start_bytes);
        Ok(MonitorStopOut {
            avg_mbps: delta as f64 * 8.0 / secs / 1_000_000.0,
            seconds: secs,
            bytes: delta,
        })
    }

    /// 清理超龄监控
    pub fn sweep(&self, max_age: std::time::Duration) {
        let mut g = self.inner.lock().unwrap();
        g.retain(|_, e| e.t0.elapsed() <= max_age);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(parse_netstat_ib(NETSTAT, "en9").is_err());
    }
}
