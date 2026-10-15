//! 内环可配置的公共 Ping 门限与本轮参数覆盖。
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunOverrides {
    pub limit_udp_by_link_speed: bool,
    pub probe_during_traffic: bool,
    pub probe_path_mtu: bool,
    pub tcp_window: Option<String>,
    pub udp_mbps: Option<f64>,
}
impl RunOverrides {
    pub fn validate(&self) -> Result<(), String> {
        if self
            .tcp_window
            .as_ref()
            .is_some_and(|v| !super::config::size_token(v))
        {
            return Err("本轮 TCP 窗口格式无效".into());
        }
        if self
            .udp_mbps
            .is_some_and(|v| !v.is_finite() || v <= 0.0 || v > 1e6)
        {
            return Err("本轮 UDP 每流速率须为正数 Mbps（上限 1000000）".into());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PingPolicy {
    pub small_max_bytes: u32,
    pub medium_max_bytes: u32,
    pub wired_small_avg_rtt_ms: f64,
    pub wired_small_max_rtt_ms: f64,
    pub wired_medium_avg_rtt_ms: f64,
    pub wired_medium_max_rtt_ms: f64,
    pub wired_large_avg_rtt_ms: f64,
    pub wired_large_max_rtt_ms: f64,
    pub wifi_small_avg_rtt_ms: f64,
    pub wifi_small_max_rtt_ms: f64,
    pub wifi_medium_avg_rtt_ms: f64,
    pub wifi_medium_max_rtt_ms: f64,
    pub wifi_large_avg_rtt_ms: f64,
    pub wifi_large_max_rtt_ms: f64,
}
impl Default for PingPolicy {
    fn default() -> Self {
        let cfg = crate::config::PingCfg::default();
        Self {
            small_max_bytes: cfg.small_max_bytes,
            medium_max_bytes: cfg.medium_max_bytes,
            wired_small_avg_rtt_ms: cfg.wired_small_avg_rtt_ms,
            wired_small_max_rtt_ms: cfg.max_rtt_ms,
            wired_medium_avg_rtt_ms: cfg.wired_medium_avg_rtt_ms,
            wired_medium_max_rtt_ms: cfg.wired_medium_max_rtt_ms,
            wired_large_avg_rtt_ms: cfg.wired_large_avg_rtt_ms,
            wired_large_max_rtt_ms: cfg.wired_large_max_rtt_ms,
            wifi_small_avg_rtt_ms: cfg.wifi_small_avg_rtt_ms,
            wifi_small_max_rtt_ms: cfg.wifi_small_max_rtt_ms,
            wifi_medium_avg_rtt_ms: cfg.wifi_medium_avg_rtt_ms,
            wifi_medium_max_rtt_ms: cfg.wifi_medium_max_rtt_ms,
            wifi_large_avg_rtt_ms: cfg.wifi_large_avg_rtt_ms,
            wifi_large_max_rtt_ms: cfg.wifi_large_max_rtt_ms,
        }
    }
}
impl PingPolicy {
    pub fn to_config(&self) -> crate::config::PingCfg {
        crate::config::PingCfg {
            small_max_bytes: self.small_max_bytes,
            medium_max_bytes: self.medium_max_bytes,
            wired_small_avg_rtt_ms: self.wired_small_avg_rtt_ms,
            max_rtt_ms: self.wired_small_max_rtt_ms,
            wired_medium_avg_rtt_ms: self.wired_medium_avg_rtt_ms,
            wired_medium_max_rtt_ms: self.wired_medium_max_rtt_ms,
            wired_large_avg_rtt_ms: self.wired_large_avg_rtt_ms,
            wired_large_max_rtt_ms: self.wired_large_max_rtt_ms,
            wifi_small_avg_rtt_ms: self.wifi_small_avg_rtt_ms,
            wifi_small_max_rtt_ms: self.wifi_small_max_rtt_ms,
            wifi_medium_avg_rtt_ms: self.wifi_medium_avg_rtt_ms,
            wifi_medium_max_rtt_ms: self.wifi_medium_max_rtt_ms,
            wifi_large_avg_rtt_ms: self.wifi_large_avg_rtt_ms,
            wifi_large_max_rtt_ms: self.wifi_large_max_rtt_ms,
            ..Default::default()
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.small_max_bytes == 0
            || self.medium_max_bytes <= self.small_max_bytes
            || self.medium_max_bytes > 65500
        {
            return Err("Ping 包长分档须为 0 < small < medium <= 65500".into());
        }
        for (avg, max) in [
            (self.wired_small_avg_rtt_ms, self.wired_small_max_rtt_ms),
            (self.wired_medium_avg_rtt_ms, self.wired_medium_max_rtt_ms),
            (self.wired_large_avg_rtt_ms, self.wired_large_max_rtt_ms),
            (self.wifi_small_avg_rtt_ms, self.wifi_small_max_rtt_ms),
            (self.wifi_medium_avg_rtt_ms, self.wifi_medium_max_rtt_ms),
            (self.wifi_large_avg_rtt_ms, self.wifi_large_max_rtt_ms),
        ] {
            if !avg.is_finite() || !max.is_finite() || avg <= 0.0 || max <= 0.0 || avg > max {
                return Err("Ping RTT 门限须为正数，平均门限不能大于最大门限".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PcCapacity {
    pub role: String,
    pub speed_mbps: u64,
}
impl PcCapacity {
    pub fn validate(&self) -> Result<(), String> {
        if self.role.len() > 64
            || self.role.chars().any(char::is_control)
            || self.speed_mbps > 1_000_000
        {
            return Err("网口角色或协商速率格式无效".into());
        }
        Ok(())
    }
    pub fn ceiling(&self) -> Option<f64> {
        crate::rate::nic_payload_ceiling_mbps(
            &crate::protocol::NicInfo {
                role: self.role.clone(),
                speed_mbps: self.speed_mbps,
                ..Default::default()
            },
            &crate::config::RateCheckCfg::default(),
        )
    }
}
pub fn clipped_parameters(
    link: &super::config::Link,
    requested: &super::config::Parameters,
) -> super::config::Parameters {
    let mut actual = requested.clone();
    let Some(mbps) = requested.udp_mbps else {
        return actual;
    };
    let ceiling = link.pc_capacity.as_ref().and_then(PcCapacity::ceiling);
    let load = crate::master::builder::udp_load_for_ceiling(
        crate::config::ParsedBandwidth {
            mbps,
            bits_per_second: (mbps * 1_000_000.0).round() as u64,
        },
        requested.streams,
        ceiling,
    );
    actual.streams = load.streams;
    actual.udp_mbps = Some(load.mbps);
    actual
}
