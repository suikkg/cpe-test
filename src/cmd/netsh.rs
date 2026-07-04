//! 解析 `netsh wlan show interfaces`（Windows WiFi 频段/SSID/状态，中英文兼容）

use crate::util::run_cmd;
use regex::Regex;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct WlanInfo {
    /// 接口名（与 ipconfig 适配器名对应，如 WLAN）
    pub name: String,
    /// 规范化频段："2.4GHz" / "5GHz" / "6GHz" / ""
    pub band: String,
    pub ssid: String,
    pub connected: bool,
}

pub fn scan() -> Vec<WlanInfo> {
    let out = run_cmd(
        "netsh",
        &["wlan", "show", "interfaces"],
        Duration::from_secs(10),
    );
    parse(&out.merged())
}

pub fn parse(text: &str) -> Vec<WlanInfo> {
    let kv = Regex::new(r"^\s*(.+?)\s*:\s*(.*)$").expect("regex");
    let mut out: Vec<WlanInfo> = Vec::new();
    let mut cur: Option<WlanInfo> = None;
    for line in text.lines() {
        let Some(cap) = kv.captures(line) else {
            continue;
        };
        let key = cap.get(1).map(|m| m.as_str()).unwrap_or("").trim();
        let val = cap.get(2).map(|m| m.as_str()).unwrap_or("").trim();
        let key_l = key.to_lowercase();
        if key_l == "name" || key == "名称" {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            cur = Some(WlanInfo {
                name: val.to_string(),
                ..Default::default()
            });
            continue;
        }
        let Some(w) = cur.as_mut() else { continue };
        if key_l == "ssid" {
            // 排除 BSSID（key 精确等于 SSID 才算）
            w.ssid = val.to_string();
        } else if key_l == "band" || key.contains("频带") || key.contains("带区") || key.contains("波段")
        {
            w.band = normalize_band(val);
        } else if key_l == "state" || key == "状态" {
            let vl = val.to_lowercase();
            w.connected = val.contains("已连接")
                || (vl.contains("connected") && !vl.contains("disconnected"));
        }
    }
    if let Some(w) = cur.take() {
        out.push(w);
    }
    out
}

/// "5 GHz"/"5GHz"/"2.4 GHz"/"6 GHz" -> 规范值
pub fn normalize_band(raw: &str) -> String {
    let s = raw.to_lowercase().replace(' ', "");
    if s.is_empty() {
        return String::new();
    }
    if s.contains("2.4") {
        "2.4GHz".into()
    } else if s.contains('5') {
        "5GHz".into()
    } else if s.contains('6') {
        "6GHz".into()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CN: &str = r#"
系统上有 1 个接口:

    名称                   : WLAN
    描述                   : Intel(R) Wi-Fi 6 AX201 160MHz
    GUID                   : xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
    物理地址               : aa:bb:cc:dd:ee:ff
    状态                   : 已连接
    SSID                   : CPE_TEST_5G
    BSSID                  : 11:22:33:44:55:66
    网络类型               : 结构
    无线电类型             : 802.11ax
    身份验证               : WPA2 - 个人
    密码                   : CCMP
    连接模式               : 配置文件
    频带                   : 5 GHz
    信道                   : 149
    接收速率(Mbps)         : 866.7
    传输速率(Mbps)         : 866.7
    信号                   : 99%
"#;

    #[test]
    fn test_parse_cn() {
        let v = parse(SAMPLE_CN);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "WLAN");
        assert!(v[0].connected);
        assert_eq!(v[0].band, "5GHz");
        assert_eq!(v[0].ssid, "CPE_TEST_5G");
    }

    const SAMPLE_EN: &str = r#"
There is 1 interface on the system:

    Name                   : Wi-Fi
    Description            : Intel(R) Wireless-AC 9560
    GUID                   : xxxxxxxx
    Physical address       : aa:bb:cc:dd:ee:ff
    State                  : connected
    SSID                   : MyAP24
    BSSID                  : 11:22:33:44:55:66
    Radio type             : 802.11n
    Band                   : 2.4 GHz
    Channel                : 6
"#;

    #[test]
    fn test_parse_en() {
        let v = parse(SAMPLE_EN);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Wi-Fi");
        assert!(v[0].connected);
        assert_eq!(v[0].band, "2.4GHz");
    }

    #[test]
    fn test_disconnected_en() {
        let t = "    Name : Wi-Fi\n    State : disconnected\n";
        let v = parse(t);
        assert!(!v[0].connected);
    }

    #[test]
    fn test_normalize_band() {
        assert_eq!(normalize_band("5 GHz"), "5GHz");
        assert_eq!(normalize_band("2.4 GHz"), "2.4GHz");
        assert_eq!(normalize_band("6 GHz"), "6GHz");
        assert_eq!(normalize_band(""), "");
    }
}
