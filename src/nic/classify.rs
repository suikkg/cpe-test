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
///   4. 速率 9000-12000 + 描述含 usb → 10GUSB（兼容 4.2G 驱动 bug）
///   5. 速率 9000-12000 → 10GETH
///   6. 速率 2400-2600 → SGMII2.5G
///   7. 速率 900-1100 → SGMII1G
///   8. 速率 3400-4000 → RNDIS（RNDIS 协商 ~3.7G 兜底）
///   9. → UNKNOWN
pub fn classify_role(description: &str, speed_mbps: u64, is_wifi: bool, band: &str) -> String {
    if is_wifi {
        return match band {
            "5GHz" => "WIFI5G",
            "2.4GHz" => "WIFI2.4G",
            "6GHz" => "WIFI6G",
            _ => "WIFI",
        }
        .to_string();
    }
    let desc_l = description.to_lowercase();
    // EVB：10GUSB 优先匹配（描述含 10GbE + USB，或 10G USB 网卡）
    if desc_l.contains("10g") && desc_l.contains("usb") {
        return "10GUSB".to_string();
    }
    if desc_l.contains("rndis") || desc_l.contains("remote ndis") {
        return "RNDIS".to_string();
    }
    match speed_mbps {
        // EVB：10G 口（纯以太 10G 或 USB 10G 显示正确速率）
        9_000..=12_000 => {
            if desc_l.contains("usb") {
                "10GUSB".to_string()
            } else {
                "10GETH".to_string()
            }
        }
        2400..=2600 => "SGMII2.5G".to_string(),
        900..=1100 => "SGMII1G".to_string(),
        // RNDIS 实测协商 ~3.7Gbps，描述没写 rndis 时按速率兜底
        3400..=4000 => "RNDIS".to_string(),
        // EVB：10GUSB 驱动显示 4.2G，描述含 USB 关键字
        4001..=9000 => {
            if desc_l.contains("usb") {
                "10GUSB".to_string()
            } else {
                "UNKNOWN".to_string()
            }
        }
        _ => "UNKNOWN".to_string(),
    }
}

pub fn role_rank(role: &str) -> usize {
    ROLE_ORDER
        .iter()
        .position(|r| *r == role)
        .unwrap_or(ROLE_ORDER.len())
}

/// 按名称关键字判断是否 WiFi 接口（兜底）
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

    #[test]
    fn test_classify() {
        // WiFi
        assert_eq!(classify_role("Intel AX201", 866, true, "5GHz"), "WIFI5G");
        assert_eq!(classify_role("Intel AX201", 100, true, "2.4GHz"), "WIFI2.4G");
        assert_eq!(classify_role("Intel AX201", 100, true, ""), "WIFI");
        // RNDIS
        assert_eq!(classify_role("Remote NDIS based Device", 0, false, ""), "RNDIS");
        assert_eq!(classify_role("USB RNDIS Adapter", 3700, false, ""), "RNDIS");
        assert_eq!(classify_role("Some USB NIC", 3700, false, ""), "RNDIS");
        // 10G
        assert_eq!(classify_role("Realtek 10GbE USB Family Controller", 4200, false, ""), "10GUSB");
        assert_eq!(classify_role("Realtek USB 10/100/1G/2.5G/5GbE/10GbE Family Controller", 4200, false, ""), "10GUSB");
        assert_eq!(classify_role("Some USB NIC", 4200, false, ""), "10GUSB");
        assert_eq!(classify_role("AQC113 10G Ethernet", 10000, false, ""), "10GETH");
        assert_eq!(classify_role("Intel X710 10G SFP+", 10000, false, ""), "10GETH");
        assert_eq!(classify_role("Realtek USB 10GbE", 10000, false, ""), "10GUSB");
        // 标准口
        assert_eq!(classify_role("Realtek GbE", 1000, false, ""), "SGMII1G");
        assert_eq!(classify_role("Realtek 2.5GbE", 2500, false, ""), "SGMII2.5G");
        // unknown
        assert_eq!(classify_role("Some NIC", 100, false, ""), "UNKNOWN");
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
