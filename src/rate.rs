//! UDP 速率目标与路径负载策略。
//!
//! 这里只把“明确已知”的 EVB 10GUSB<->10GETH 方向自动转成验收目标。
//! SGMII/RNDIS/WiFi/CPE 子网只提供安全负载上限；实际能力未知时进入
//! observe/discover，避免把协商速率误当成 PASS 门槛。

use crate::config::{LinkProfiles, NicProfile, RateCheckCfg, RateMode, RateTargets};
use crate::protocol::NicInfo;

pub fn nic_payload_ceiling_mbps(nic: &NicInfo, cfg: &RateCheckCfg) -> Option<f64> {
    let role = nic.role.to_uppercase();
    let negotiated = (nic.speed_mbps > 0).then_some(nic.speed_mbps as f64);
    let cap = match role.as_str() {
        "SGMII1G" => Some(1000.0),
        "SGMII2.5G" => Some(cfg.cpe_path_ceiling_mbps),
        // RNDIS 跟协商速率：那块口报什么就按什么裁（实测 3700）。它和下面的
        // 10GUSB 不一样——10GUSB(NCM) 报的 4.2G 是已知的驱动显示问题，
        // 而 RNDIS 报的值是可用的。跟协商速率的代价是「驱动报错了就裁错了」，
        // 收益是不必为每种 USB 网卡在这张表里各钉一个数。
        "RNDIS" => negotiated,
        // WiFi 不跟协商速率：那是 PHY 速率，会在一轮测试里反复跳
        // （同一块 Wi-Fi 7 网卡 2402 / 2882 来回），拿它裁 -b 会让相邻两个
        // 单元的灌包强度都不一样。固定档位才能横向比较。
        //
        // 但 2.4G 必须单独一档：那个频段只有 3 个不重叠信道、最多 40MHz，
        // 和 5G/6G 共用 2800 等于对 2.4G 口根本不裁剪。
        "WIFI2.4G" => Some(cfg.wifi_24g_payload_ceiling_mbps),
        // 频段没识别出来时按 5G 档，不按 2.4G 档：Windows 上 netsh 正常会报出
        // 频段，落到这里多半是 macOS/Linux 的扫描拿不到；把它压到 2.4G 档会让
        // 一堆真正的 5G 口被误裁，比不裁更糟。
        "WIFI" | "WIFI5G" | "WIFI6G" => Some(cfg.wifi_payload_ceiling_mbps),
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

/// 一条链路、一个方向解析出来的策略。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkPolicy {
    /// 接收端网卡 RX 门限；`None` 表示这一层没给，交由下游继续兜底。
    pub rx_target_mbps: Option<f64>,
    /// 覆盖全局 `udp_profiles` 档位的单流带宽；`None` 表示沿用档位本身。
    pub udp_bandwidth: Option<String>,
    /// 覆盖全局 `udp_profiles` 档位的报文长度（`-l`）；`None` 表示沿用档位本身。
    pub udp_length: Option<String>,
    /// 门限是按协商速率百分比算出来的时，记下算式（或算不出来的原因）。
    ///
    /// 协商速率会变，尤其 Wi-Fi。同一份配置两次跑出不同门限时，把算式摆在
    /// 计划提示里是唯一能让人看懂的办法。
    pub rx_target_note: Option<String>,
}

/// 把一条单口覆盖的门限设置换算成绝对 Mbps。
///
/// 绝对值优先于百分比：两个都填时按绝对值，不去猜哪个更"新"。
fn nic_rx_target(profile: &NicProfile, nic: &NicInfo) -> (Option<f64>, Option<String>) {
    rx_target_from(
        profile.rx_target_mbps,
        profile.rx_target_percent,
        nic,
        "门限",
    )
}

fn rx_target_from(
    absolute: Option<f64>,
    percent: Option<f64>,
    nic: &NicInfo,
    label: &str,
) -> (Option<f64>, Option<String>) {
    if let Some(absolute) = absolute.filter(|value| value.is_finite() && *value > 0.0) {
        return (Some(absolute), None);
    }
    let Some(percent) = percent.filter(|value| value.is_finite() && *value > 0.0) else {
        return (None, None);
    };
    if nic.speed_mbps == 0 {
        // 换算不出来就退回下游兜底，但必须说出来：静默地把「按 90% 判」变成
        // 「按内置推导判」，报告上看不出门限换过。
        return (
            None,
            Some(format!(
                "接收口 {} 协商速率未知，{percent}% {label}无法换算，本条回退到默认推导",
                nic.name
            )),
        );
    }
    let speed = nic.speed_mbps as f64;
    let target = speed * percent / 100.0;
    (
        Some(target),
        Some(format!(
            "接收口 {} {label}按协商速率换算：{speed:.0}Mbps × {percent}% = {target:.0}Mbps",
            nic.name
        )),
    )
}

/// 判断一块网卡是否命中某条单口覆盖。
/// 一条 `by_nic` 覆盖项是否落在这块网卡上。
///
/// 判定路径和 Web 控制台的回填共用这一份规则：两边各写一份时，界面显示的
/// 「这块网卡已有策略」和运行时真正生效的策略会在规则变动那天悄悄分叉。
pub(crate) fn nic_profile_matches(profile: &NicProfile, host: &str, nic: &NicInfo) -> bool {
    profile.host.eq_ignore_ascii_case(host)
        && profile.name == nic.name
        && (profile.ipv4.is_empty() || profile.ipv4 == nic.ipv4)
}

/// 角色配对匹配：配对串左边是 A、右边是 B。
///
/// 返回这条 (src -> dst) 在该配对里对应的方向键。同角色互测时两个方向
/// 都落在 `ab` 上，这是刻意的——那种配对本来就没有 A/B 之分。
fn role_pair_direction(pair: &str, src: &NicInfo, dst: &NicInfo) -> Option<&'static str> {
    let (left, right) = pair.split_once("<->")?;
    let (left, right) = (left.trim(), right.trim());
    let src_role = src.role.trim();
    let dst_role = dst.role.trim();
    if left.eq_ignore_ascii_case(src_role) && right.eq_ignore_ascii_case(dst_role) {
        Some("ab")
    } else if right.eq_ignore_ascii_case(src_role) && left.eq_ignore_ascii_case(dst_role) {
        Some("ba")
    } else {
        None
    }
}

/// 两层链路策略解析：**单口覆盖 > 角色配对**，都没命中就返回空，
/// 由调用方继续按既有顺序兜底（全局 targets → 内置 EVB 推导 → 无目标）。
///
/// 两个值都按方向独立解析。同一条链路两个方向的能力可以差很多——
/// run_20260825_215915_7684 里 `以太网6 → WLAN3` 是 1821Mbps、
/// 反向只有 17Mbps——用一个门限卡两个方向没有物理意义
/// （见 .ai/DESIGN-v4.3.0.md F2）。
pub fn resolve_link_policy(
    profiles: &LinkProfiles,
    src_host: &str,
    src: &NicInfo,
    dst_host: &str,
    dst: &NicInfo,
) -> LinkPolicy {
    // 门限看接收端，带宽看发送端：两者约束的是链路的不同侧。
    let receiver_profile = profiles
        .by_nic
        .iter()
        .find(|profile| nic_profile_matches(profile, dst_host, dst));
    let (nic_target, rx_target_note) = receiver_profile
        .map(|profile| nic_rx_target(profile, dst))
        .unwrap_or((None, None));
    let rx_target_mbps = nic_target
        .or_else(|| {
            profiles.by_role.iter().find_map(|profile| {
                let direction = role_pair_direction(&profile.pair, src, dst)?;
                profile.rx_target_mbps.for_direction(direction)
            })
        })
        .filter(|value| value.is_finite() && *value > 0.0);

    let sender_profile = profiles
        .by_nic
        .iter()
        .find(|profile| nic_profile_matches(profile, src_host, src));
    let udp_bandwidth = sender_profile
        .and_then(|profile| profile.udp_bandwidth.clone())
        .or_else(|| {
            profiles.by_role.iter().find_map(|profile| {
                let direction = role_pair_direction(&profile.pair, src, dst)?;
                profile
                    .udp_bandwidth
                    .for_direction(direction)
                    .map(str::to_string)
            })
        })
        .filter(|value| !value.trim().is_empty());

    // `-l` 只有单口覆盖一层：报文长度是发送口的属性，没有「这个角色配对该用
    // 多大报文」这种说法。
    let udp_length = sender_profile
        .and_then(|profile| profile.udp_length.clone())
        .filter(|value| !value.trim().is_empty());

    LinkPolicy {
        rx_target_mbps,
        udp_bandwidth,
        udp_length,
        rx_target_note,
    }
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
    // 仅测试用到的配置类型；产品码只经由 LinkProfiles 间接接触它们。
    use crate::config::{DirectionalBandwidth, RoleProfile};

    fn nic(role: &str, speed: u64) -> NicInfo {
        NicInfo {
            role: role.into(),
            speed_mbps: speed,
            ..Default::default()
        }
    }

    fn named_nic(name: &str, role: &str, ip: &str, speed: u64) -> NicInfo {
        NicInfo {
            name: name.into(),
            role: role.into(),
            ipv4: ip.into(),
            speed_mbps: speed,
            ..Default::default()
        }
    }

    fn role_profile(pair: &str, ab: f64, ba: f64, ab_bw: &str, ba_bw: &str) -> RoleProfile {
        RoleProfile {
            pair: pair.into(),
            rx_target_mbps: RateTargets {
                ab: Some(ab),
                ba: Some(ba),
                forward: None,
            },
            udp_bandwidth: DirectionalBandwidth {
                ab: Some(ab_bw.into()),
                ba: Some(ba_bw.into()),
                forward: None,
            },
        }
    }

    /// 配对串左边是 A、右边是 B，两个方向各取各的值。
    /// 同一条链路两个方向能力可以差 100 倍，用一个门限卡两边没有物理意义。
    #[test]
    fn role_pairs_resolve_each_direction_independently() {
        let profiles = LinkProfiles {
            by_role: vec![role_profile(
                "SGMII2.5G<->WIFI5G",
                1600.0,
                40.0,
                "2.6G",
                "500m",
            )],
            by_nic: Vec::new(),
        };
        let eth = named_nic("以太网 6", "SGMII2.5G", "192.168.0.101", 2500);
        let wifi = named_nic("WLAN 3", "WIFI5G", "192.168.0.104", 2882);

        let forward = resolve_link_policy(&profiles, "master", &eth, "agent", &wifi);
        assert_eq!(forward.rx_target_mbps, Some(1600.0));
        assert_eq!(forward.udp_bandwidth.as_deref(), Some("2.6G"));

        // 反过来接的时候必须落到 ba，而不是复用 ab。
        let reverse = resolve_link_policy(&profiles, "agent", &wifi, "master", &eth);
        assert_eq!(reverse.rx_target_mbps, Some(40.0));
        assert_eq!(reverse.udp_bandwidth.as_deref(), Some("500m"));
    }

    /// 角色对不上就当没配，交给下游继续兜底，不能瞎套一个别的配对。
    #[test]
    fn an_unrelated_pair_resolves_to_nothing() {
        let profiles = LinkProfiles {
            by_role: vec![role_profile(
                "SGMII2.5G<->WIFI5G",
                1600.0,
                40.0,
                "2.6G",
                "500m",
            )],
            by_nic: Vec::new(),
        };
        let a = named_nic("以太网", "SGMII1G", "192.168.0.102", 1000);
        let b = named_nic("以太网 5", "RNDIS", "192.168.0.100", 3750);
        assert_eq!(
            resolve_link_policy(&profiles, "agent", &a, "master", &b),
            LinkPolicy::default()
        );
    }

    /// 单口覆盖压过角色兜底：同为 WIFI5G，Wi-Fi 7 BE200 和普通 5G 网卡
    /// 不是一回事，角色层给不了这个区分。
    #[test]
    fn a_single_nic_override_beats_the_role_default() {
        let profiles = LinkProfiles {
            by_role: vec![role_profile(
                "SGMII2.5G<->WIFI5G",
                1600.0,
                40.0,
                "2.6G",
                "500m",
            )],
            by_nic: vec![NicProfile {
                host: "agent".into(),
                name: "WLAN 3".into(),
                ipv4: "192.168.0.104".into(),
                rx_target_mbps: Some(1800.0),
                udp_bandwidth: Some("2.8G".into()),
                ..Default::default()
            }],
        };
        let eth = named_nic("以太网 6", "SGMII2.5G", "192.168.0.101", 2500);
        let wifi = named_nic("WLAN 3", "WIFI5G", "192.168.0.104", 2882);

        // WLAN 3 作接收端 -> 用它自己的门限；发送端是以太网，带宽仍走角色层。
        let forward = resolve_link_policy(&profiles, "master", &eth, "agent", &wifi);
        assert_eq!(forward.rx_target_mbps, Some(1800.0));
        assert_eq!(forward.udp_bandwidth.as_deref(), Some("2.6G"));

        // WLAN 3 作发送端 -> 用它自己的带宽；接收端是以太网，门限走角色层。
        let reverse = resolve_link_policy(&profiles, "agent", &wifi, "master", &eth);
        assert_eq!(reverse.udp_bandwidth.as_deref(), Some("2.8G"));
        assert_eq!(reverse.rx_target_mbps, Some(40.0));
    }

    /// 覆盖项写了 ipv4 就必须一起对上，否则同名接口会张冠李戴。
    #[test]
    fn a_single_nic_override_respects_the_optional_ipv4_guard() {
        let profiles = LinkProfiles {
            by_role: Vec::new(),
            by_nic: vec![NicProfile {
                host: "agent".into(),
                name: "WLAN 3".into(),
                ipv4: "192.168.0.104".into(),
                rx_target_mbps: Some(1800.0),
                udp_bandwidth: None,
                ..Default::default()
            }],
        };
        let eth = named_nic("以太网 6", "SGMII2.5G", "192.168.0.101", 2500);
        let moved = named_nic("WLAN 3", "WIFI5G", "192.168.0.150", 2882);
        assert_eq!(
            resolve_link_policy(&profiles, "master", &eth, "agent", &moved).rx_target_mbps,
            None
        );

        // host 也要对：主控和辅测各有一块 WLAN 3 的情况是存在的。
        let matching = named_nic("WLAN 3", "WIFI5G", "192.168.0.104", 2882);
        assert_eq!(
            resolve_link_policy(&profiles, "master", &eth, "master", &matching).rx_target_mbps,
            None
        );
        assert_eq!(
            resolve_link_policy(&profiles, "master", &eth, "agent", &matching).rx_target_mbps,
            Some(1800.0)
        );
    }

    #[test]
    fn an_empty_profile_set_changes_nothing() {
        let eth = named_nic("以太网 6", "SGMII2.5G", "192.168.0.101", 2500);
        let wifi = named_nic("WLAN 3", "WIFI5G", "192.168.0.104", 2882);
        assert_eq!(
            resolve_link_policy(&LinkProfiles::default(), "master", &eth, "agent", &wifi),
            LinkPolicy::default()
        );
    }

    /// 每种口的裁剪上限，逐条钉住。这张表是「按链路上限裁剪」勾上之后
    /// 唯一决定灌多猛的东西，改动必须是明确的，不能被某次重构顺手带偏。
    #[test]
    fn test_cpe_path_ceiling() {
        let cfg = RateCheckCfg::default();
        // 10GUSB(NCM) 报的 4.2G 是驱动显示问题，按 10G 算；这条路径的瓶颈
        // 因此落在对端 SGMII2.5G 的 2600 上。
        assert_eq!(
            path_payload_ceiling_mbps(&nic("10GUSB", 4200), &nic("SGMII2.5G", 2500), &cfg),
            Some(2600.0)
        );
        // RNDIS 跟协商速率：3700 就是 3700，不再压到 CPE 子网那档。
        assert_eq!(
            path_payload_ceiling_mbps(&nic("RNDIS", 3700), &nic("10GETH", 10000), &cfg),
            Some(3700.0)
        );
        assert_eq!(
            path_payload_ceiling_mbps(&nic("SGMII1G", 1000), &nic("10GETH", 10000), &cfg),
            Some(1000.0)
        );
        assert_eq!(
            path_payload_ceiling_mbps(&nic("10GETH", 10000), &nic("10GUSB", 4200), &cfg),
            Some(10_000.0)
        );
        // SGMII2.5G 的上限恰好容得下这类口的常规档位 -b 2.6G，
        // 和 Wi-Fi 那档 2800 是同一个用意。
        assert_eq!(
            path_payload_ceiling_mbps(&nic("SGMII2.5G", 2500), &nic("10GETH", 10000), &cfg),
            Some(2600.0)
        );
        // 协商速率读不出来时退回对端，而不是当成 0 把这条腿裁没。
        assert_eq!(
            path_payload_ceiling_mbps(&nic("RNDIS", 0), &nic("SGMII1G", 1000), &cfg),
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

    /// 2.4G 和 5G 必须走不同的上限。共用 2800 等于对 2.4G 口完全不裁剪。
    #[test]
    fn the_24g_band_does_not_borrow_the_5g_ceiling() {
        let cfg = RateCheckCfg::default();
        let ceiling = |role: &str, speed: u64| {
            nic_payload_ceiling_mbps(&named_nic("wlan", role, "192.168.1.9", speed), &cfg)
        };

        assert_eq!(ceiling("WIFI5G", 2882), Some(2800.0));
        assert_eq!(ceiling("WIFI6G", 5760), Some(2800.0));
        // 协商值特意不取 574：上限恰好等于 802.11ax 2SS 的 PHY 峰值，
        // 若拿 574 当输入，「返回配置的上限」和「返回协商速率」两种实现
        // 都能过这条断言，这个用例就不再有区分力了。
        assert_eq!(
            ceiling("WIFI2.4G", 286),
            Some(574.0),
            "2.4GHz 只有 3 个不重叠信道、最多 40MHz，够不到 5G 的档"
        );

        // 频段没识别出来时按 5G 档：落到 WIFI 的多半是 macOS/Linux 扫不到频段，
        // 把它压到 2.4G 档会让一堆真正的 5G 口被误裁。
        assert_eq!(ceiling("WIFI", 866), Some(2800.0));
    }

    /// 三个频段的上限都不跟协商速率走——协商值只是 PHY 速率，会来回跳。
    #[test]
    fn no_wifi_band_follows_the_negotiated_phy_rate() {
        let cfg = RateCheckCfg::default();
        for role in ["WIFI", "WIFI2.4G", "WIFI5G", "WIFI6G"] {
            let low = nic_payload_ceiling_mbps(&named_nic("w", role, "192.168.1.9", 72), &cfg);
            let high = nic_payload_ceiling_mbps(&named_nic("w", role, "192.168.1.9", 5760), &cfg);
            assert_eq!(low, high, "{role} 的上限被协商速率带偏了");
        }
    }
}
