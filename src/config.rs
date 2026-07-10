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
    /// 跨机 iperf 要求两端同 /24（ping 不受限）
    pub require_same_subnet_for_iperf: bool,
    /// UDP 按整条路径的可信负载上限裁剪档位/流数。
    pub limit_udp_by_link_speed: bool,
    /// 每个 iperf 任务结束后在接收端截图
    pub screenshot: bool,
    /// 24 小时内已 PASS 的任务跳过
    pub resume: bool,
    /// 测试完自动打开 HTML 报告
    pub open_report: bool,
    pub iperf: IperfCfg,
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
            ping: PingCfg::default(),
            pairs: None,
            universal_params: None,
            tests: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IperfCfg {
    /// 全局默认灌包秒数
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
}

impl UdpProfile {
    pub fn bw(b: &str) -> Self {
        UdpProfile {
            bandwidth: b.into(),
            length: None,
        }
    }

    /// 带宽数值 Mbps（"500m"->500, "1g"->1000；解析失败 None）
    pub fn bandwidth_mbps(&self) -> Option<f64> {
        let s = self.bandwidth.trim().to_lowercase().replace(',', ".");
        let num: String = s
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let v: f64 = num.parse().ok()?;
        if s.ends_with('g') {
            Some(v * 1000.0)
        } else if s.ends_with('k') {
            Some(v / 1000.0)
        } else {
            // 默认按 m
            Some(v)
        }
    }

    pub fn name(&self) -> String {
        match &self.length {
            Some(l) => format!("udp_b{}_l{}", self.bandwidth, l),
            None => format!("udp_b{}", self.bandwidth),
        }
    }

    pub fn label(&self) -> String {
        match &self.length {
            Some(l) => format!("UDP -b {} -l {}", self.bandwidth, l),
            None => format!("UDP -b {}", self.bandwidth),
        }
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
            payload_sizes: vec![32],
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
    /// ["iperf","ping"]
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
        assert_eq!(c.ping.count, 100);
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
            "ping": {"count": 10, "payload_sizes": [32, 1400]},
            "tests": [
                {"name":"t1","src":"master:SGMII2.5G","dst":"agent:SGMII2.5G",
                 "direction":"bidir","kinds":["iperf","ping"],"transports":["tcp","udp"],
                 "ip":["v4","v6"],"streams":5,"iperf_duration":300},
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
        assert_eq!(c.tests[1].direction.directions(), vec!["ab", "ba"]);
        assert_eq!(c.tests[1].kinds, vec!["iperf"]);
    }

    #[test]
    fn test_direction_both() {
        let d = OneOrMany::One("both".into());
        assert_eq!(d.directions(), vec!["ab", "ba"]);
    }

    #[test]
    fn test_udp_profile() {
        assert_eq!(UdpProfile::bw("500m").bandwidth_mbps(), Some(500.0));
        assert_eq!(UdpProfile::bw("1g").bandwidth_mbps(), Some(1000.0));
        assert_eq!(UdpProfile::bw("2500m").name(), "udp_b2500m");
        let p = UdpProfile {
            bandwidth: "1000m".into(),
            length: Some("64".into()),
        };
        assert_eq!(p.label(), "UDP -b 1000m -l 64");
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
