//! 网卡角色分类（纯逻辑，不分平台）

/// 角色排序权重（越小越靠前）
pub const ROLE_ORDER: [&str; 10] = [
    "10GETH",
    "10GUSB",
    "SGMII2.5G",
    "SGMII1G",
    "RNDIS",
    "WIFI5G",
    "WIFI6G",
    "WIFI2.4G",
    "WIFI",
    "UNKNOWN",
];

/// 按 描述/速率/WiFi频段 分类角色
///
/// 优先级：
///   1. WiFi 频段 → WIFI5G/2.4G/6G
///   2. 描述含 "10g" + "usb" → 10GUSB
///   3. 描述含 rndis/remote ndis → RNDIS
///   4. 速率 4001-8999 + 描述含 usb → 10GUSB（兼容 4.2G 驱动 bug）
///   5. 速率 9000-12000 → 10GETH
///   6. 速率 2400-2600 → SGMII2.5G
///   7. 速率 900-1100 → SGMII1G
///   8. 速率 3400-4000 → RNDIS（RNDIS 协商 ~3.7G 兜底）
///   9. → UNKNOWN
pub fn classify_role(description: &str, speed_mbps: u64, is_wifi: bool, band: &str) -> String {
    let desc_l = description.to_lowercase();
    let is_usb = desc_l.contains("usb");
    let role = if is_wifi {
        match band {
            "5GHz" => "WIFI5G",
            "2.4GHz" => "WIFI2.4G",
            "6GHz" => "WIFI6G",
            _ => "WIFI",
        }
    } else if desc_l.contains("10g") && is_usb {
        // EVB：描述含 10GbE + USB，或 10G USB 网卡
        "10GUSB"
    } else if desc_l.contains("rndis") || desc_l.contains("remote ndis") {
        "RNDIS"
    } else {
        match speed_mbps {
            // EVB：10G 口，以及驱动只显示 4.2G 的 10GUSB
            4001..=12_000 => match (is_usb, speed_mbps >= 9_000) {
                (true, _) => "10GUSB",
                (false, true) => "10GETH",
                _ => "UNKNOWN",
            },
            2400..=2600 => "SGMII2.5G",
            900..=1100 => "SGMII1G",
            // RNDIS 实测协商 ~3.7Gbps，描述没写 rndis 时按速率兜底
            3400..=4000 => "RNDIS",
            _ => "UNKNOWN",
        }
    };
    role.to_string()
}

pub fn role_rank(role: &str) -> usize {
    ROLE_ORDER
        .iter()
        .position(|r| *r == role)
        .unwrap_or(ROLE_ORDER.len())
}

/// 按名称关键字判断是否 WiFi 接口（兜底）
#[cfg(any(windows, test))]
pub fn is_wifi_name(name: &str) -> bool {
    let l = name.to_lowercase();
    ["wi-fi", "wifi", "wireless", "wlan", "802.11"]
        .iter()
        .any(|k| l.contains(k))
        || name.contains("无线")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(description: &str, speed: u64, wifi: bool, band: &str) -> String {
        classify_role(description, speed, wifi, band)
    }

    #[test]
    fn test_classify() {
        // WiFi
        assert_eq!(role("Intel AX201", 866, true, "5GHz"), "WIFI5G");
        assert_eq!(role("Intel AX201", 100, true, "2.4GHz"), "WIFI2.4G");
        assert_eq!(role("Intel AX201", 100, true, ""), "WIFI");
        // RNDIS
        assert_eq!(role("Remote NDIS based Device", 0, false, ""), "RNDIS");
        assert_eq!(role("USB RNDIS Adapter", 3700, false, ""), "RNDIS");
        assert_eq!(role("Some USB NIC", 3700, false, ""), "RNDIS");
        // 10G
        assert_eq!(role("Realtek 10GbE USB", 100, false, ""), "10GUSB");
        // 标准口
        assert_eq!(role("Realtek GbE", 1000, false, ""), "SGMII1G");
        assert_eq!(role("Realtek 2.5GbE", 2500, false, ""), "SGMII2.5G");
        // unknown
        assert_eq!(role("Some NIC", 100, false, ""), "UNKNOWN");
    }

    #[test]
    fn test_10g_speed_boundaries() {
        for (description, speed, expected) in [
            ("USB NIC", 4000, "RNDIS"),
            ("USB NIC", 4001, "10GUSB"),
            ("USB NIC", 8999, "10GUSB"),
            ("Ethernet", 8999, "UNKNOWN"),
            ("Ethernet", 9000, "10GETH"),
            ("USB NIC", 12_000, "10GUSB"),
            ("Ethernet", 12_001, "UNKNOWN"),
        ] {
            assert_eq!(role(description, speed, false, ""), expected);
        }
    }

    #[test]
    fn test_rank() {
        assert!(role_rank("10GETH") < role_rank("10GUSB"));
        assert!(role_rank("SGMII2.5G") < role_rank("SGMII1G"));
        assert!(role_rank("10GUSB") < role_rank("SGMII2.5G"));
        assert!(role_rank("WIFI5G") < role_rank("WIFI2.4G"));
        assert!(role_rank("nonsense") >= ROLE_ORDER.len());
    }

    #[test]
    fn test_wifi_name() {
        assert!(is_wifi_name("WLAN"));
        assert!(is_wifi_name("Wi-Fi 2"));
        assert!(is_wifi_name("无线网络连接"));
        assert!(!is_wifi_name("以太网"));
    }
}
