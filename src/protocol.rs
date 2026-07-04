//! 主控 <-> 辅测 HTTP 接口的请求/响应类型定义（JSON）

use serde::{Deserialize, Serialize};

/// 一张网卡的信息（两端通用）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NicInfo {
    /// 接口名（Windows: 连接名如 "以太网"；macOS: en0）
    pub name: String,
    /// 驱动描述（如 "Realtek USB 5GbE Family Controller"）
    #[serde(default)]
    pub description: String,
    /// 角色：SGMII1G / SGMII2.5G / RNDIS / WIFI5G / WIFI2.4G / WIFI / UNKNOWN
    pub role: String,
    /// IPv4 地址（必有，扫描时按前缀过滤）
    pub ipv4: String,
    /// IPv6 link-local（fe80::，不带 %zone）
    #[serde(default)]
    pub ipv6_ll: String,
    /// IPv6 全局地址（2xxx/3xxx）
    #[serde(default)]
    pub ipv6_global: String,
    /// fe80 的 zone：Windows 为接口索引数字，macOS 为接口名
    #[serde(default)]
    pub zone: String,
    /// 协商速率 Mbps（未知为 0）
    #[serde(default)]
    pub speed_mbps: u64,
    #[serde(default)]
    pub is_wifi: bool,
    /// WiFi 频段："2.4GHz" / "5GHz" / "6GHz" / ""
    #[serde(default)]
    pub wifi_band: String,
    #[serde(default)]
    pub ifindex: u32,
}

impl NicInfo {
    /// 简短展示，如 "以太网(192.168.1.2, 2500Mbps)"
    pub fn brief(&self) -> String {
        let mut extra = String::new();
        if self.speed_mbps > 0 {
            extra.push_str(&format!(", {}Mbps", self.speed_mbps));
        }
        if !self.wifi_band.is_empty() {
            extra.push_str(&format!(", {}", self.wifi_band));
        }
        format!("{}({}{})", self.name, self.ipv4, extra)
    }
}

/// 一台机器的信息（/info 返回）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub interfaces: Vec<NicInfo>,
}

/// 统一响应包装：{"ok":bool, "error":..., "data":{...}}
#[derive(Debug, Serialize, Deserialize)]
pub struct Resp<T> {
    pub ok: bool,
    pub error: Option<String>,
    pub data: Option<T>,
}

pub fn ok_json<T: Serialize>(data: T) -> String {
    serde_json::to_string(&Resp {
        ok: true,
        error: None,
        data: Some(data),
    })
    .unwrap_or_else(|e| err_json(&format!("序列化失败: {e}")))
}

pub fn err_json(msg: &str) -> String {
    format!(
        "{{\"ok\":false,\"error\":{},\"data\":null}}",
        serde_json::to_string(msg).unwrap_or_else(|_| "\"error\"".into())
    )
}

// ---------- /info ----------
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InfoReq {
    /// 主控下发的 IPv4 前缀过滤（空则用 agent 本地默认）
    #[serde(default)]
    pub ipv4_prefixes: Vec<String>,
}

// ---------- /ping ----------
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PingReq {
    /// 目标地址（v6 link-local 时已带 %zone）
    pub dst: String,
    /// 源地址绑定（-S）
    pub src: String,
    pub count: u32,
    /// 负载字节数（-l / -s）
    pub payload: u32,
    #[serde(default)]
    pub v6: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PingOut {
    pub ok: bool,
    pub sent: u32,
    pub received: u32,
    pub lost: u32,
    pub loss_pct: f64,
    pub rtt_min: Option<f64>,
    pub rtt_avg: Option<f64>,
    pub rtt_max: Option<f64>,
    pub cmd: String,
    pub raw: String,
}

// ---------- /iperf/server ----------
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfServerStartReq {
    /// 绑定地址（v6 link-local 已带 %zone）
    pub bind_ip: String,
    pub port: u16,
    #[serde(default)]
    pub v6: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfServerStartOut {
    pub cmd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfServerStopReq {
    pub port: u16,
    /// 停止前最多等 server 自然退出秒数
    #[serde(default)]
    pub wait_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfServerStopOut {
    pub existed: bool,
    pub output: String,
}

// ---------- /iperf/client ----------
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfClientReq {
    /// 对端 server 地址（v6 link-local 已带 %zone）
    pub dst: String,
    /// 本端绑定地址（-B）
    pub bind_ip: String,
    pub port: u16,
    pub duration: u64,
    #[serde(default)]
    pub udp: bool,
    #[serde(default)]
    pub v6: bool,
    /// 额外参数：如 ["-w","64k","-P","5"] 或 ["-b","500m","-l","64"]
    #[serde(default)]
    pub extra: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfClientOut {
    pub ok: bool,
    pub timed_out: bool,
    pub cmd: String,
    pub output: String,
}

// ---------- /monitor ----------
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorStartReq {
    pub iface: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorStartOut {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorStopReq {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorStopOut {
    pub avg_mbps: f64,
    pub seconds: f64,
    pub bytes: u64,
}

// ---------- /screenshot ----------
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScreenshotReq {
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScreenshotOut {
    pub image_b64: String,
    pub format: String,
}

// ---------- /health ----------
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthOut {
    pub hostname: String,
    pub os: String,
    pub version: String,
    /// iperf3 版本信息，None 表示未找到
    pub iperf3: Option<String>,
}
