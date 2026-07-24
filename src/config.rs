//! 配置文件（config.json）加载。所有字段都真正生效。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 辅测机管理口 IP（留空则交互询问）
    pub agent_host: String,
    pub agent_port: u16,
    /// 测试子网 IPv4 前缀过滤
    pub ipv4_prefixes: Vec<String>,
    /// 跨机 iperf3/ctsTraffic 要求两端同 /24（历史字段名保持兼容；ping 不受限）
    pub require_same_subnet_for_iperf: bool,
    /// UDP 按整条路径的可信负载上限裁剪档位/流数。
    pub limit_udp_by_link_speed: bool,
    /// 每个吞吐任务结束后在涉及端截图
    pub screenshot: bool,
    /// 24 小时内已 PASS 的任务跳过
    pub resume: bool,
    /// 测试完自动打开 HTML 报告
    pub open_report: bool,
    pub iperf: IperfCfg,
    /// Windows 专用 ctsTraffic 后端的简化默认参数。
    pub ctstraffic: CtsTrafficCfg,
    pub ping: PingCfg,
    /// 自动配对生成测试：字符串 "all" 或具体角色对列表
    #[serde(default)]
    pub pairs: Option<Pairs>,
    /// pairs 模式下的统一测试参数
    #[serde(default)]
    pub universal_params: Option<UniversalParams>,
    pub tests: Vec<TestSpec>,
}

/// pairs 字段：可以是 "all" 字符串，也可以是角色对数组
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Pairs {
    All(String),
    List(Vec<PairSpec>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairSpec {
    /// master 侧的角色 或 NAME=接口名
    pub master: String,
    /// agent 侧的角色 或 NAME=接口名
    pub agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniversalParams {
    #[serde(default = "default_direction")]
    pub directions: OneOrMany,
    #[serde(default = "default_kinds")]
    pub kinds: Vec<String>,
    #[serde(default = "default_transports")]
    pub transports: Vec<String>,
    #[serde(default = "default_ip")]
    pub ip: Vec<String>,
    #[serde(default = "default_streams")]
    pub streams: u32,
    /// 可选：覆盖 streams 的 TCP 并发流数（0/缺省时沿用 streams）。
    #[serde(default)]
    pub tcp_streams: Option<u32>,
    /// 可选：覆盖 streams 的 UDP 并发流数（0/缺省时沿用 streams）。
    #[serde(default)]
    pub udp_streams: Option<u32>,
    /// 历史字段名；当前供 iperf3 与 ctsTraffic 共用。
    #[serde(default)]
    pub iperf_duration: Option<u64>,
    #[serde(default)]
    pub ping_count: Option<u32>,
    #[serde(default)]
    pub ping_payload_sizes: Option<Vec<u32>>,
    #[serde(default)]
    pub tcp_windows: Option<Vec<String>>,
    #[serde(default)]
    pub udp_profiles: Option<Vec<UdpProfile>>,
    /// auto / verify / observe / discover
    #[serde(default)]
    pub rate_mode: Option<RateMode>,
    /// 双向可分别配置 ab/ba；单向可用 forward。
    #[serde(default)]
    pub rate_targets_mbps: Option<RateTargets>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            agent_host: String::new(),
            agent_port: 28801,
            ipv4_prefixes: vec!["192.168.".into()],
            require_same_subnet_for_iperf: true,
            limit_udp_by_link_speed: true,
            screenshot: true,
            resume: false,
            open_report: true,
            iperf: IperfCfg::default(),
            ctstraffic: CtsTrafficCfg::default(),
            ping: PingCfg::default(),
            pairs: None,
            universal_params: None,
            tests: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CtsTrafficCfg {
    /// ctsTraffic UDP MediaStream 每秒媒体帧数；每帧再拆成 datagram。
    pub udp_frame_rate: u32,
    /// UDP client 应用层缓冲深度（秒），不是 socket buffer。
    pub udp_buffer_depth_secs: u32,
    /// 控制台聚合状态输出周期（毫秒）。
    pub status_update_ms: u32,
}

impl Default for CtsTrafficCfg {
    fn default() -> Self {
        Self {
            udp_frame_rate: 100,
            udp_buffer_depth_secs: 1,
            status_update_ms: 1_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IperfCfg {
    /// 两种吞吐后端共用的全局默认灌包秒数（历史上位于 iperf 节点）
    pub duration: u64,
    /// TCP window 档位
    pub tcp_windows: Vec<String>,
    /// UDP 带宽档位
    pub udp_profiles: Vec<UdpProfile>,
    pub rate_check: RateCheckCfg,
}

impl Default for IperfCfg {
    fn default() -> Self {
        IperfCfg {
            duration: 180,
            tcp_windows: vec!["64k".into(), "1m".into(), "4m".into()],
            udp_profiles: vec![
                UdpProfile::bw("1m"),
                UdpProfile::bw("100m"),
                UdpProfile::bw("500m"),
                UdpProfile {
                    bandwidth: "1000m".into(),
                    length: Some("64".into()),
                    window: None,
                },
                UdpProfile::bw("2500m"),
            ],
            rate_check: RateCheckCfg::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateMode {
    #[default]
    Auto,
    Verify,
    Observe,
    Discover,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RateTargets {
    pub forward: Option<f64>,
    pub ab: Option<f64>,
    pub ba: Option<f64>,
}

impl RateTargets {
    pub fn for_direction(&self, direction: &str) -> Option<f64> {
        match direction {
            "ab" => self.ab.or(self.forward),
            "ba" => self.ba.or(self.forward),
            _ => self.forward,
        }
        .filter(|v| v.is_finite() && *v > 0.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateCheckCfg {
    pub mode: RateMode,
    pub targets_mbps: RateTargets,
    pub sample_interval_ms: u64,
    pub background_secs: u64,
    pub startup_timeout_secs: u64,
    pub settle_secs: u64,
    pub launch_interval_ms: u64,
    pub min_concurrent_streams: u32,
    pub min_active_ratio: f64,
    pub offered_headroom_pct: f64,
    /// UDP 完整 server/client 额外尝试预算；单流/单连接每方向总尝试数至少为 3。
    pub flow_retries: u32,
    pub discovery_step_secs: u64,
    /// EVB 10GUSB/NCM -> 10GETH 的已知接收目标。
    /// 兼容旧字段 evb_usb_tx_target_mbps（以 USB 发送方向命名）。
    #[serde(alias = "evb_usb_tx_target_mbps")]
    pub evb_usb_to_eth_target_mbps: f64,
    /// EVB 10GETH -> 10GUSB/NCM 的已知接收目标。
    /// 兼容旧字段 evb_usb_rx_target_mbps（以 USB 接收方向命名）。
    #[serde(alias = "evb_usb_rx_target_mbps")]
    pub evb_eth_to_usb_target_mbps: f64,
    /// RNDIS/SGMII2.5G/受限 CPE 子网的默认负载上限，不直接作为 PASS 目标。
    pub cpe_path_ceiling_mbps: f64,
    pub max_udp_loss_pct: Option<f64>,
}

impl Default for RateCheckCfg {
    fn default() -> Self {
        Self {
            mode: RateMode::Auto,
            targets_mbps: RateTargets::default(),
            sample_interval_ms: 1000,
            background_secs: 3,
            startup_timeout_secs: 15,
            settle_secs: 5,
            launch_interval_ms: 50,
            min_concurrent_streams: 2,
            min_active_ratio: 0.90,
            offered_headroom_pct: 5.0,
            flow_retries: 1,
            discovery_step_secs: 10,
            evb_usb_to_eth_target_mbps: 6400.0,
            evb_eth_to_usb_target_mbps: 8400.0,
            cpe_path_ceiling_mbps: 2500.0,
            max_udp_loss_pct: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpProfile {
    pub bandwidth: String,
    #[serde(default)]
    pub length: Option<String>,
    /// iperf3 UDP socket buffer（`-w`）；省略时保持旧配置行为。
    #[serde(default)]
    pub window: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ParsedBandwidth {
    pub mbps: f64,
    pub bits_per_second: u64,
}

impl ParsedBandwidth {
    /// iperf3 的无后缀带宽值按 bit/s 解释。传精确整数可避免依赖其对
    /// `Gbps` 等长后缀的非文档兼容行为。
    pub fn iperf_arg(self) -> String {
        self.bits_per_second.to_string()
    }
}

impl UdpProfile {
    pub fn bw(b: &str) -> Self {
        UdpProfile {
            bandwidth: b.into(),
            length: None,
            window: None,
        }
    }

    /// 严格解析完整带宽字符串。支持十进制数值加 `k/m/g` 或
    /// `kbps/mbps/gbps`（大小写不敏感），逗号也可作小数点；裸数为
    /// 兼容旧配置仍按 Mbps 解释。
    pub(crate) fn parsed_bandwidth(&self) -> Result<ParsedBandwidth, String> {
        let raw = self.bandwidth.trim();
        let lower = raw.to_ascii_lowercase();
        let (number, bps_multiplier) = [
            ("kbps", 1_000.0),
            ("mbps", 1_000_000.0),
            ("gbps", 1_000_000_000.0),
            ("k", 1_000.0),
            ("m", 1_000_000.0),
            ("g", 1_000_000_000.0),
        ]
        .into_iter()
        .find_map(|(suffix, multiplier)| {
            lower
                .strip_suffix(suffix)
                .map(|number| (number, multiplier))
        })
        .unwrap_or((lower.as_str(), 1_000_000.0));

        let mut separator_seen = false;
        let mut digits_before_separator = 0usize;
        let mut digits_after_separator = 0usize;
        for byte in number.bytes() {
            if byte.is_ascii_digit() {
                if separator_seen {
                    digits_after_separator += 1;
                } else {
                    digits_before_separator += 1;
                }
            } else if matches!(byte, b'.' | b',') && !separator_seen {
                separator_seen = true;
            } else {
                return Err(format!("无法解析 UDP 带宽 {}", self.bandwidth));
            }
        }
        if digits_before_separator == 0 || (separator_seen && digits_after_separator == 0) {
            return Err(format!("无法解析 UDP 带宽 {}", self.bandwidth));
        }

        let number = number.replace(',', ".");
        let value = number
            .parse::<f64>()
            .map_err(|_| format!("无法解析 UDP 带宽 {}", self.bandwidth))?;
        let bps = value * bps_multiplier;
        let rounded_bps = bps.round();
        // `u64::MAX as f64` 会舍入为 2^64；必须在转换前拒绝等于该
        // 边界的值，否则 `as u64` 会饱和成一个并非用户所写的速率。
        if !rounded_bps.is_finite() || rounded_bps < 1.0 || rounded_bps >= u64::MAX as f64 {
            return Err(format!("UDP 带宽超出有效范围: {}", self.bandwidth));
        }

        let bits_per_second = rounded_bps as u64;
        Ok(ParsedBandwidth {
            // 规划流数、报告 offered rate 与命令参数都基于同一个整数 bps，
            // 避免小数边界造成三者不一致。
            mbps: bits_per_second as f64 / 1_000_000.0,
            bits_per_second,
        })
    }

    pub fn name(&self) -> String {
        let mut name = format!("udp_b{}", self.bandwidth);
        if let Some(length) = &self.length {
            name.push_str(&format!("_l{length}"));
        }
        if let Some(window) = &self.window {
            name.push_str(&format!("_w{window}"));
        }
        name
    }

    pub fn label(&self) -> String {
        let mut label = format!("UDP -b {}", self.bandwidth);
        if let Some(length) = &self.length {
            label.push_str(&format!(" -l {length}"));
        }
        if let Some(window) = &self.window {
            label.push_str(&format!(" -w {window}"));
        }
        label
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PingCfg {
    pub count: u32,
    pub payload_sizes: Vec<u32>,
}

impl Default for PingCfg {
    fn default() -> Self {
        PingCfg {
            count: 100,
            payload_sizes: vec![32, 1600, 65500],
        }
    }
}

/// 单个测试项（config.json 的 tests[]）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSpec {
    #[serde(default)]
    pub name: String,
    /// "master:SGMII2.5G" / "agent:WIFI5G" / "master:NAME=以太网 2"
    pub src: String,
    pub dst: String,
    /// "A->B" / "B->A" / "bidir" / "both"(旧值,展开为前两个)；可以是字符串或数组
    #[serde(default = "default_direction")]
    pub direction: OneOrMany,
    /// ["iperf","ctstraffic","ping"]，可任选或组合
    #[serde(default = "default_kinds")]
    pub kinds: Vec<String>,
    /// ["tcp","udp"]
    #[serde(default = "default_transports")]
    pub transports: Vec<String>,
    /// ["v4","v6"]
    #[serde(default = "default_ip")]
    pub ip: Vec<String>,
    #[serde(default = "default_streams")]
    pub streams: u32,
    /// 可选：覆盖 streams 的 TCP 并发流数（0/缺省时沿用 streams）。
    #[serde(default)]
    pub tcp_streams: Option<u32>,
    /// 可选：覆盖 streams 的 UDP 并发流数（0/缺省时沿用 streams）。
    #[serde(default)]
    pub udp_streams: Option<u32>,
    /// 历史字段名；当前供 iperf3 与 ctsTraffic 共用。
    #[serde(default)]
    pub iperf_duration: Option<u64>,
    #[serde(default)]
    pub ping_count: Option<u32>,
    #[serde(default)]
    pub ping_payload_sizes: Option<Vec<u32>>,
    #[serde(default)]
    pub tcp_windows: Option<Vec<String>>,
    #[serde(default)]
    pub udp_profiles: Option<Vec<UdpProfile>>,
    #[serde(default)]
    pub rate_mode: Option<RateMode>,
    #[serde(default)]
    pub rate_targets_mbps: Option<RateTargets>,
}

fn default_direction() -> OneOrMany {
    OneOrMany::One("A->B".into())
}
fn default_kinds() -> Vec<String> {
    vec!["iperf".into()]
}
fn default_transports() -> Vec<String> {
    vec!["tcp".into()]
}
fn default_ip() -> Vec<String> {
    vec!["v4".into()]
}
fn default_streams() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    /// 展开为规范方向列表：ab / ba / bidir（去重保序）
    pub fn directions(&self) -> Vec<String> {
        let raw: Vec<String> = match self {
            OneOrMany::One(s) => vec![s.clone()],
            OneOrMany::Many(v) => v.clone(),
        };
        let mut out: Vec<String> = Vec::new();
        for r in raw {
            let n = r.trim().to_uppercase();
            let mapped: Vec<&str> = match n.as_str() {
                "A->B" | "AB" | "A>B" => vec!["ab"],
                "B->A" | "BA" | "B>A" => vec!["ba"],
                "BIDIR" | "A<->B" | "双向" => vec!["bidir"],
                "BOTH" => vec!["ab", "ba"],
                _ => vec![],
            };
            for m in mapped {
                if !out.iter().any(|x| x == m) {
                    out.push(m.to_string());
                }
            }
        }
        if out.is_empty() {
            out.push("ab".into());
        }
        out
    }
}

/// 加载配置：--config 指定 > ./config.json > 程序同目录 config.json > 默认
pub fn load_config(explicit: Option<&str>) -> (Config, Option<PathBuf>) {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        candidates.push(PathBuf::from(p));
    } else {
        candidates.push(PathBuf::from("config.json"));
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("config.json"));
            }
        }
    }
    for p in candidates {
        if p.exists() {
            match load_from(&p) {
                Ok(c) => return (c, Some(p)),
                Err(e) => {
                    eprintln!("!! 配置文件 {} 解析失败: {e}", p.display());
                    eprintln!("!! 将使用默认配置继续");
                    return (Config::default(), None);
                }
            }
        }
    }
    let mut cfg = Config::default();
    // 兼容旧版环境变量
    if let Ok(v) = std::env::var("AUTOTEST_IPV4_PREFIXES") {
        let list: Vec<String> = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !list.is_empty() {
            cfg.ipv4_prefixes = list;
        }
    }
    if let Ok(v) = std::env::var("AUTOTEST_AGENT_HOST") {
        if !v.trim().is_empty() {
            cfg.agent_host = v.trim().to_string();
        }
    }
    (cfg, None)
}

fn load_from(p: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(p).map_err(|e| e.to_string())?;
    // 容忍 UTF-8 BOM
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str::<Config>(text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let c = Config::default();
        assert_eq!(c.agent_port, 28801);
        assert_eq!(c.iperf.duration, 180);
        assert_eq!(c.iperf.tcp_windows, vec!["64k", "1m", "4m"]);
        assert_eq!(c.iperf.udp_profiles.len(), 5);
        assert!(c.iperf.udp_profiles.iter().all(|p| p.window.is_none()));
        assert_eq!(c.ping.count, 100);
        assert_eq!(c.ping.payload_sizes, vec![32, 1600, 65500]);
        assert_eq!(c.iperf.rate_check.mode, RateMode::Auto);
        assert_eq!(c.iperf.rate_check.evb_usb_to_eth_target_mbps, 6400.0);
        assert_eq!(c.iperf.rate_check.evb_eth_to_usb_target_mbps, 8400.0);
    }

    #[test]
    fn test_parse_full() {
        let j = r#"{
            "agent_host": "10.228.46.50",
            "ipv4_prefixes": ["192.168.", "10.10."],
            "iperf": {"duration": 60},
            "ping": {"count": 10, "payload_sizes": [32, 1600, 65500]},
            "tests": [
                {"name":"t1","src":"master:SGMII2.5G","dst":"agent:SGMII2.5G",
                 "direction":"bidir","kinds":["iperf","ping"],"transports":["tcp","udp"],
                 "ip":["v4","v6"],"streams":5,"tcp_streams":7,"udp_streams":3,
                 "iperf_duration":300},
                {"name":"t2","src":"master:SGMII1G","dst":"agent:SGMII1G",
                 "direction":["A->B","B->A"]}
            ]
        }"#;
        let c: Config = serde_json::from_str(j).unwrap();
        assert_eq!(c.agent_host, "10.228.46.50");
        assert_eq!(c.iperf.duration, 60);
        // 未写的字段用默认
        assert_eq!(c.iperf.tcp_windows.len(), 3);
        assert_eq!(c.tests.len(), 2);
        assert_eq!(c.tests[0].direction.directions(), vec!["bidir"]);
        assert_eq!(c.tests[0].iperf_duration, Some(300));
        assert_eq!(c.tests[0].tcp_streams, Some(7));
        assert_eq!(c.tests[0].udp_streams, Some(3));
        assert_eq!(c.tests[1].direction.directions(), vec!["ab", "ba"]);
        assert_eq!(c.tests[1].kinds, vec!["iperf"]);
        assert_eq!(c.tests[1].tcp_streams, None);
        assert_eq!(c.tests[1].udp_streams, None);
    }

    #[test]
    fn test_direction_both() {
        let d = OneOrMany::One("both".into());
        assert_eq!(d.directions(), vec!["ab", "ba"]);
    }

    #[test]
    fn test_udp_profile() {
        let mbps = |bandwidth: &str| {
            UdpProfile::bw(bandwidth)
                .parsed_bandwidth()
                .ok()
                .map(|value| value.mbps)
        };
        assert_eq!(mbps("500m"), Some(500.0));
        assert_eq!(mbps("1g"), Some(1000.0));
        assert_eq!(mbps("2.8G"), Some(2800.0));
        assert_eq!(mbps("2.8Gbps"), Some(2800.0));
        assert_eq!(mbps("2,8gBpS"), Some(2800.0));
        let parsed = UdpProfile::bw("2.8Gbps").parsed_bandwidth().unwrap();
        assert_eq!(parsed.bits_per_second, 2_800_000_000);
        assert_eq!(parsed.iperf_arg(), "2800000000");
        for invalid in [
            "",
            "2.8oopsGbps",
            "2.8Gbps trailing",
            "2.8mbpsx",
            "1e3m",
            "1.2,3g",
            "1.",
            "+1m",
            "0m",
            "18446744073709.551616",
        ] {
            assert_eq!(mbps(invalid), None, "必须拒绝非完整带宽 value={invalid:?}");
        }
        assert_eq!(UdpProfile::bw("2500m").name(), "udp_b2500m");
        let p = UdpProfile {
            bandwidth: "1000m".into(),
            length: Some("64".into()),
            window: Some("4m".into()),
        };
        assert_eq!(p.name(), "udp_b1000m_l64_w4m");
        assert_eq!(p.label(), "UDP -b 1000m -l 64 -w 4m");
    }

    #[test]
    fn test_udp_profile_window_parse_is_backward_compatible() {
        let legacy: UdpProfile = serde_json::from_str(r#"{"bandwidth":"500m"}"#).unwrap();
        assert_eq!(legacy.bandwidth, "500m");
        assert_eq!(legacy.length, None);
        assert_eq!(legacy.window, None);

        let configured: UdpProfile =
            serde_json::from_str(r#"{"bandwidth":"1000m","length":"64","window":"4m"}"#).unwrap();
        assert_eq!(configured.length.as_deref(), Some("64"));
        assert_eq!(configured.window.as_deref(), Some("4m"));
        assert_eq!(configured.name(), "udp_b1000m_l64_w4m");
        assert_eq!(configured.label(), "UDP -b 1000m -l 64 -w 4m");
    }

    #[test]
    fn test_rate_check_parse() {
        let j = r#"{
            "iperf": {
                "rate_check": {
                    "mode": "verify",
                    "targets_mbps": {"ab": 6400, "ba": 8400},
                    "min_active_ratio": 0.8,
                    "flow_retries": 2
                }
            }
        }"#;
        let c: Config = serde_json::from_str(j).unwrap();
        assert_eq!(c.iperf.rate_check.mode, RateMode::Verify);
        assert_eq!(c.iperf.rate_check.targets_mbps.ab, Some(6400.0));
        assert_eq!(c.iperf.rate_check.targets_mbps.ba, Some(8400.0));
        assert_eq!(c.iperf.rate_check.min_active_ratio, 0.8);
        assert_eq!(c.iperf.rate_check.flow_retries, 2);
    }

    #[test]
    fn test_per_scenario_rate_mode_and_targets_parse() {
        let j = r#"{
            "universal_params": {
                "rate_mode": "discover",
                "rate_targets_mbps": {"forward": 2500}
            },
            "tests": [{
                "name": "evb",
                "src": "master:10GUSB",
                "dst": "agent:10GETH",
                "rate_mode": "verify",
                "rate_targets_mbps": {"ab": 6400, "ba": 8400}
            }]
        }"#;
        let c: Config = serde_json::from_str(j).unwrap();
        let universal = c.universal_params.unwrap();
        assert_eq!(universal.rate_mode, Some(RateMode::Discover));
        assert_eq!(universal.rate_targets_mbps.unwrap().forward, Some(2500.0));
        assert_eq!(c.tests[0].rate_mode, Some(RateMode::Verify));
        assert_eq!(
            c.tests[0].rate_targets_mbps.as_ref().unwrap().ab,
            Some(6400.0)
        );
        assert_eq!(
            c.tests[0].rate_targets_mbps.as_ref().unwrap().ba,
            Some(8400.0)
        );
    }

    #[test]
    fn test_evb_direction_target_names_and_legacy_aliases() {
        let current: Config = serde_json::from_str(
            r#"{
                "iperf": {"rate_check": {
                    "evb_usb_to_eth_target_mbps": 6100,
                    "evb_eth_to_usb_target_mbps": 8300
                }}
            }"#,
        )
        .unwrap();
        assert_eq!(current.iperf.rate_check.evb_usb_to_eth_target_mbps, 6100.0);
        assert_eq!(current.iperf.rate_check.evb_eth_to_usb_target_mbps, 8300.0);

        let legacy: Config = serde_json::from_str(
            r#"{
                "iperf": {"rate_check": {
                    "evb_usb_tx_target_mbps": 6200,
                    "evb_usb_rx_target_mbps": 8200
                }}
            }"#,
        )
        .unwrap();
        assert_eq!(legacy.iperf.rate_check.evb_usb_to_eth_target_mbps, 6200.0);
        assert_eq!(legacy.iperf.rate_check.evb_eth_to_usb_target_mbps, 8200.0);
    }
}
