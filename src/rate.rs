//! UDP 速率目标与路径负载策略。
//!
//! 这里只把“明确已知”的 EVB 10GUSB<->10GETH 方向自动转成验收目标。
//! SGMII/RNDIS/WiFi/CPE 子网只提供安全负载上限；实际能力未知时进入
//! observe/discover，避免把协商速率误当成 PASS 门槛。

use crate::config::{RateCheckCfg, RateMode, RateTargets};
use crate::protocol::NicInfo;

pub fn nic_payload_ceiling_mbps(nic: &NicInfo, cfg: &RateCheckCfg) -> Option<f64> {
    let role = nic.role.to_uppercase();
    let negotiated = (nic.speed_mbps > 0).then_some(nic.speed_mbps as f64);
    let cap = match role.as_str() {
        "SGMII1G" => Some(1000.0),
        "SGMII2.5G" | "RNDIS" => Some(cfg.cpe_path_ceiling_mbps),
        "WIFI" | "WIFI2.4G" | "WIFI5G" | "WIFI6G" => Some(
            negotiated
                .unwrap_or(cfg.cpe_path_ceiling_mbps)
                .min(cfg.cpe_path_ceiling_mbps),
        ),
        // 10GUSB 的 4.2G 协商值是已知驱动显示问题，不能按 4.2G 裁剪。
        "10GUSB" | "10GETH" => Some(10_000.0),
        _ => negotiated,
    }?;
    Some(cap.max(1.0))
}

pub fn path_payload_ceiling_mbps(src: &NicInfo, dst: &NicInfo, cfg: &RateCheckCfg) -> Option<f64> {
    match (
        nic_payload_ceiling_mbps(src, cfg),
        nic_payload_ceiling_mbps(dst, cfg),
    ) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

pub fn auto_evb_target_mbps(src: &NicInfo, dst: &NicInfo, cfg: &RateCheckCfg) -> Option<f64> {
    let src_role = src.role.to_ascii_uppercase();
    let dst_role = dst.role.to_ascii_uppercase();
    let target = match (src_role.as_str(), dst_role.as_str()) {
        ("10GUSB", "10GETH") => Some(cfg.evb_usb_to_eth_target_mbps),
        ("10GETH", "10GUSB") => Some(cfg.evb_eth_to_usb_target_mbps),
        _ => None,
    };
    target.filter(|value| value.is_finite() && *value > 0.0)
}

pub fn resolve_target_mbps(
    mode: RateMode,
    targets: &RateTargets,
    direction: &str,
    src: &NicInfo,
    dst: &NicInfo,
    cfg: &RateCheckCfg,
) -> Option<f64> {
    let explicit = targets
        .for_direction(direction)
        .or_else(|| cfg.targets_mbps.for_direction(direction));
    match mode {
        RateMode::Observe | RateMode::Discover => None,
        RateMode::Verify => explicit.or_else(|| auto_evb_target_mbps(src, dst, cfg)),
        RateMode::Auto => explicit.or_else(|| auto_evb_target_mbps(src, dst, cfg)),
    }
}

pub fn effective_mode(mode: RateMode, target_mbps: Option<f64>) -> RateMode {
    match mode {
        RateMode::Auto if target_mbps.is_some() => RateMode::Verify,
        RateMode::Auto => RateMode::Observe,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(role: &str, speed: u64) -> NicInfo {
        NicInfo {
            role: role.into(),
            speed_mbps: speed,
            ..Default::default()
        }
    }

    #[test]
    fn test_cpe_path_ceiling() {
        let cfg = RateCheckCfg::default();
        assert_eq!(
            path_payload_ceiling_mbps(&nic("10GUSB", 4200), &nic("SGMII2.5G", 2500), &cfg),
            Some(2500.0)
        );
        assert_eq!(
            path_payload_ceiling_mbps(&nic("RNDIS", 3700), &nic("10GETH", 10000), &cfg),
            Some(2500.0)
        );
        assert_eq!(
            path_payload_ceiling_mbps(&nic("SGMII1G", 1000), &nic("10GETH", 10000), &cfg),
            Some(1000.0)
        );
    }

    #[test]
    fn test_evb_direction_targets() {
        let cfg = RateCheckCfg::default();
        assert_eq!(
            auto_evb_target_mbps(&nic("10GUSB", 4200), &nic("10GETH", 10000), &cfg),
            Some(6400.0)
        );
        assert_eq!(
            auto_evb_target_mbps(&nic("10GETH", 10000), &nic("10GUSB", 10000), &cfg),
            Some(8400.0)
        );
        assert_eq!(
            auto_evb_target_mbps(&nic("10GUSB", 10000), &nic("SGMII2.5G", 2500), &cfg),
            None
        );
        assert_eq!(
            auto_evb_target_mbps(&nic("10gusb", 4200), &nic("10geth", 10000), &cfg),
            Some(6400.0)
        );
    }

    #[test]
    fn test_explicit_targets_override_evb_defaults() {
        let usb = nic("10GUSB", 4200);
        let eth = nic("10GETH", 10000);
        let mut cfg = RateCheckCfg::default();
        cfg.targets_mbps.ab = Some(6200.0);
        assert_eq!(
            resolve_target_mbps(
                RateMode::Auto,
                &RateTargets::default(),
                "ab",
                &usb,
                &eth,
                &cfg,
            ),
            Some(6200.0)
        );

        let scenario_targets = RateTargets {
            ab: Some(6100.0),
            ba: Some(8300.0),
            ..Default::default()
        };
        assert_eq!(
            resolve_target_mbps(RateMode::Auto, &scenario_targets, "ab", &usb, &eth, &cfg,),
            Some(6100.0)
        );
        assert_eq!(
            resolve_target_mbps(RateMode::Auto, &scenario_targets, "ba", &eth, &usb, &cfg,),
            Some(8300.0)
        );
    }

    #[test]
    fn test_modes_do_not_turn_observation_into_acceptance() {
        let usb = nic("10GUSB", 4200);
        let eth = nic("10GETH", 10000);
        let targets = RateTargets {
            forward: Some(6000.0),
            ..Default::default()
        };
        let cfg = RateCheckCfg::default();

        assert_eq!(
            resolve_target_mbps(RateMode::Observe, &targets, "ab", &usb, &eth, &cfg),
            None
        );
        assert_eq!(
            resolve_target_mbps(RateMode::Discover, &targets, "ab", &usb, &eth, &cfg),
            None
        );
        assert_eq!(
            effective_mode(RateMode::Auto, Some(6000.0)),
            RateMode::Verify
        );
        assert_eq!(effective_mode(RateMode::Auto, None), RateMode::Observe);
    }

    #[test]
    fn test_invalid_automatic_target_is_not_accepted() {
        let usb = nic("10GUSB", 4200);
        let eth = nic("10GETH", 10000);
        let cfg = RateCheckCfg {
            evb_usb_to_eth_target_mbps: 0.0,
            ..Default::default()
        };
        assert_eq!(auto_evb_target_mbps(&usb, &eth, &cfg), None);

        let cfg = RateCheckCfg {
            evb_usb_to_eth_target_mbps: f64::INFINITY,
            ..Default::default()
        };
        assert_eq!(auto_evb_target_mbps(&usb, &eth, &cfg), None);
    }
}
