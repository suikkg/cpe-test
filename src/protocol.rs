//! 主控 <-> 辅测 HTTP 接口的请求/响应类型定义（JSON）

use serde::{Deserialize, Serialize};

/// 表示 agent 支持 request-id 幂等、同步 stop、owner 批量清理和动态租约。
pub const RELIABLE_LIFECYCLE_CAPABILITY: &str = "reliable_lifecycle_v1";
pub const LIVE_NIC_PROGRESS_CAPABILITY: &str = "live_nic_progress_v1";

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
    /// 该 IPv4 接口的默认网关；无默认路由或旧版 agent 未提供时为空。
    #[serde(default)]
    pub gateway_v4: String,
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
    /// 单次 server 生命周期的幂等键。空值保留旧版按端口管理语义。
    #[serde(default)]
    pub request_id: String,
    /// 一次自动化运行的资源所有者，用于异常路径批量清理。
    #[serde(default)]
    pub owner_id: String,
    /// server 租约秒数；超时后 agent 可自动清理。0 表示使用兼容默认值。
    #[serde(default)]
    pub lease_secs: u64,
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
    /// 精确停止对应 start 的 server。空值保留旧版按端口停止语义。
    #[serde(default)]
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfServerStopOut {
    pub existed: bool,
    /// true 表示目标进程已确认退出并完成 wait 回收，或目标原本就不存在。
    #[serde(default)]
    pub terminated: bool,
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
    #[serde(default)]
    pub cancelled: bool,
    pub cmd: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IperfEventKind {
    #[default]
    Started,
    Connected,
    Traffic,
    Retry,
    Error,
    Ended,
}

/// iperf3 实时事件。elapsed_ms 以单个 client job 启动为零点，
/// 主控可叠加本地 launch offset，避免直接比较两台机器的系统时钟。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfFlowEvent {
    pub kind: IperfEventKind,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub mbps: Option<f64>,
    #[serde(default)]
    pub line: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfClientStartReq {
    pub request: IperfClientReq,
    /// 幂等 job ID；非空时 agent 使用该值作为实际 job ID。
    #[serde(default)]
    pub request_id: String,
    /// 一次自动化运行的资源所有者，用于异常路径批量清理。
    #[serde(default)]
    pub owner_id: String,
    /// client job 租约秒数；0 表示使用兼容的 agent 旧作业上限。
    #[serde(default)]
    pub lease_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfClientStartOut {
    pub id: String,
    /// agent 返回响应时，该 job 自创建起已经经过的毫秒数；用于主控对齐时间轴。
    #[serde(default)]
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfClientStatusReq {
    pub id: String,
    /// 从该事件下标开始返回，避免长测试反复传输全部 interval。
    #[serde(default)]
    pub cursor: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfClientStatusOut {
    pub id: String,
    pub done: bool,
    pub next_cursor: usize,
    #[serde(default)]
    pub events: Vec<IperfFlowEvent>,
    /// 仅 done=true 时返回最终结果。
    #[serde(default)]
    pub result: Option<IperfClientOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfClientStopReq {
    pub id: String,
    /// 最多等待 client 子进程确认退出的秒数；0 表示使用 agent 默认值。
    #[serde(default)]
    pub wait_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfClientStopOut {
    pub existed: bool,
    pub was_done: bool,
    /// true 表示 worker 与 client 子进程已确认结束，或目标原本就不存在。
    #[serde(default)]
    pub terminated: bool,
}

// ---------- /resources/cleanup ----------
/// 按 owner 清理一次自动化运行遗留的所有远端资源。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourceCleanupReq {
    pub owner_id: String,
    /// client 批量取消与回收的总等待预算；0 表示使用 agent 默认值。
    #[serde(default)]
    pub wait_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourceCleanupOut {
    pub servers: usize,
    pub clients: usize,
    pub monitors: usize,
    #[serde(default)]
    pub errors: Vec<String>,
}

// ---------- /monitor ----------
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorStartReq {
    pub iface: String,
    /// 连续采样周期。默认 1000ms，Windows 可按需降低到 500ms。
    #[serde(default = "default_monitor_interval_ms")]
    pub interval_ms: u64,
    /// 一次自动化运行的资源所有者，用于异常路径批量清理。
    #[serde(default)]
    pub owner_id: String,
    /// monitor 租约秒数；0 表示使用兼容的 agent 默认上限。
    #[serde(default)]
    pub lease_secs: u64,
}

fn default_monitor_interval_ms() -> u64 {
    1000
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorStartOut {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorStatusReq {
    pub id: String,
}

/// 运行中的网卡监控快照。读取该接口不会停止 monitor；主控用它按秒打印
/// OS 网卡计数器口径的实际 RX/TX，最终判定仍使用 stop 返回的完整样本序列。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorStatusOut {
    pub id: String,
    pub iface: String,
    pub sample_count: usize,
    #[serde(default)]
    pub latest_sample: Option<MonitorSample>,
    pub error_count: usize,
    #[serde(default)]
    pub latest_error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorStopReq {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorStopOut {
    pub avg_mbps: f64,
    #[serde(default)]
    pub tx_avg_mbps: f64,
    pub seconds: f64,
    pub bytes: u64,
    #[serde(default)]
    pub tx_bytes: u64,
    #[serde(default)]
    pub samples: Vec<MonitorSample>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorSample {
    /// 以 monitor start 为零点，避免依赖两台机器系统时钟同步。
    pub elapsed_ms: u64,
    pub interval_ms: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_delta_bytes: u64,
    pub tx_delta_bytes: u64,
    pub rx_mbps: f64,
    pub tx_mbps: f64,
    pub valid: bool,
    #[serde(default)]
    pub error: String,
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
    /// 可选协议能力；旧 agent 缺少该字段时按空列表处理。
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct LegacyServerStartReq {
        bind_ip: String,
        port: u16,
        v6: bool,
    }

    #[derive(Debug, Deserialize)]
    struct LegacyClientStartReq {
        request: IperfClientReq,
    }

    #[test]
    fn legacy_json_defaults_new_lifecycle_fields() {
        let nic: NicInfo =
            serde_json::from_str(r#"{"name":"Ethernet","role":"UNKNOWN","ipv4":"192.0.2.10"}"#)
                .unwrap();
        assert!(nic.gateway_v4.is_empty());

        let server: IperfServerStartReq =
            serde_json::from_str(r#"{"bind_ip":"127.0.0.1","port":56000,"v6":false}"#).unwrap();
        assert!(server.request_id.is_empty());
        assert!(server.owner_id.is_empty());
        assert_eq!(server.lease_secs, 0);

        let client: IperfClientStartReq = serde_json::from_str(
            r#"{"request":{"dst":"127.0.0.1","bind_ip":"127.0.0.1","port":56000,"duration":1,"udp":true,"v6":false,"extra":[]}}"#,
        )
        .unwrap();
        assert!(client.request_id.is_empty());
        assert!(client.owner_id.is_empty());
        assert_eq!(client.lease_secs, 0);

        let monitor: MonitorStartReq =
            serde_json::from_str(r#"{"iface":"Ethernet","interval_ms":1000}"#).unwrap();
        assert!(monitor.owner_id.is_empty());
        assert_eq!(monitor.lease_secs, 0);

        let health: HealthOut = serde_json::from_str(
            r#"{"hostname":"old-agent","os":"windows","version":"3.0.0","iperf3":null}"#,
        )
        .unwrap();
        assert!(health.capabilities.is_empty());
    }

    #[test]
    fn legacy_structs_ignore_new_lifecycle_fields() {
        let server_json = serde_json::to_string(&IperfServerStartReq {
            bind_ip: "127.0.0.1".into(),
            port: 56_000,
            v6: false,
            request_id: "server-1".into(),
            owner_id: "unit-1".into(),
            lease_secs: 300,
        })
        .unwrap();
        let legacy_server: LegacyServerStartReq = serde_json::from_str(&server_json).unwrap();
        assert_eq!(legacy_server.bind_ip, "127.0.0.1");
        assert_eq!(legacy_server.port, 56_000);
        assert!(!legacy_server.v6);

        let client_json = serde_json::to_string(&IperfClientStartReq {
            request: IperfClientReq {
                dst: "127.0.0.1".into(),
                bind_ip: "127.0.0.1".into(),
                port: 56_000,
                duration: 1,
                udp: true,
                ..Default::default()
            },
            request_id: "client-1".into(),
            owner_id: "unit-1".into(),
            lease_secs: 300,
        })
        .unwrap();
        let legacy_client: LegacyClientStartReq = serde_json::from_str(&client_json).unwrap();
        assert_eq!(legacy_client.request.port, 56_000);
    }
}
