//! Linux 网卡扫描：`ip -j addr` + `ip -j route` + sysfs。
//!
//! 结构化解析 JSON（不解析本地化文本）：
//! - `ip -j -4 addr` / `ip -j -6 addr`：接口名、flags、operstate、IPv4/IPv6 地址；
//! - `ip -j route`：default 路由的 gateway，按 dev 归属到接口；
//! - `/sys/class/net/<iface>/speed`：以太网协商速率（Mbps）；
//! - `/sys/class/net/<iface>/wireless/`：Wi-Fi 判定；
//! - `/sys/class/net/<iface>/wireless/link`：Wi-Fi 当前链路速率（Mb/s）。
//!
//! 过滤规则：loopback、非 UP、operstate=down、无 IPv4、IPv4 前缀不匹配。

use super::classify::classify_role;
use super::ipv4_match;
use crate::protocol::NicInfo;
use crate::util::run_cmd;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
struct IfBlock {
    name: String,
    flags: Vec<String>,
    operstate: String,
    ipv4: Option<String>,
    ipv6_ll: Option<String>,
    ipv6_global: Option<String>,
}

/// 解析 `ip -j -4 addr` / `ip -j -6 addr` 的 JSON 数组，返回 ifname -> block。
fn parse_addr_json(text: &str, v6: bool) -> HashMap<String, IfBlock> {
    let mut map: HashMap<String, IfBlock> = HashMap::new();
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return map,
    };
    let Some(arr) = value.as_array() else {
        return map;
    };
    for iface in arr {
        let Some(name) = iface.get("ifname").and_then(|v| v.as_str()) else {
            continue;
        };
        let block = map.entry(name.to_string()).or_insert_with(|| IfBlock {
            name: name.to_string(),
            ..Default::default()
        });
        if let Some(flags) = iface.get("flags").and_then(|v| v.as_array()) {
            for f in flags {
                if let Some(s) = f.as_str() {
                    if !block.flags.iter().any(|x| x == s) {
                        block.flags.push(s.to_string());
                    }
                }
            }
        }
        if let Some(st) = iface.get("operstate").and_then(|v| v.as_str()) {
            if block.operstate.is_empty() {
                block.operstate = st.to_string();
            }
        }
        if let Some(addrs) = iface.get("addr_info").and_then(|v| v.as_array()) {
            for ai in addrs {
                let Some(local) = ai.get("local").and_then(|v| v.as_str()) else {
                    continue;
                };
                let low = local.to_lowercase();
                let stripped = low.split('%').next().unwrap_or(&low).to_string();
                if v6 {
                    if stripped.starts_with("fe80") {
                        if block.ipv6_ll.is_none() {
                            block.ipv6_ll = Some(stripped);
                        }
                    } else if (stripped.starts_with('2') || stripped.starts_with('3'))
                        && block.ipv6_global.is_none()
                    {
                        block.ipv6_global = Some(stripped);
                    }
                } else if block.ipv4.is_none() {
                    block.ipv4 = Some(stripped);
                }
            }
        }
    }
    map
}

/// 解析 `ip -j route`：返回 dev -> default gateway（IPv4）。
fn parse_gateway_v4(text: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return map,
    };
    let Some(arr) = value.as_array() else {
        return map;
    };
    for route in arr {
        let is_default = route
            .get("dst")
            .and_then(|v| v.as_str())
            .map(|d| d == "default")
            .unwrap_or(false);
        if !is_default {
            continue;
        }
        let Some(gw) = route.get("gateway").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(dev) = route.get("dev").and_then(|v| v.as_str()) else {
            continue;
        };
        map.entry(dev.to_string()).or_insert_with(|| gw.to_string());
    }
    map
}

/// 判断是否为 Wi-Fi 接口：存在 /sys/class/net/<iface>/wireless 目录。
fn is_wifi_iface(iface: &str) -> bool {
    std::path::Path::new("/sys/class/net")
        .join(iface)
        .join("wireless")
        .is_dir()
}

/// 读取接口速率（Mbps）：
/// - 以太网：/sys/class/net/<iface>/speed；
/// - Wi-Fi：/sys/class/net/<iface>/wireless/link（Mb/s）；
/// - 读不到或为 -1 时返回 0。
fn read_speed(iface: &str, is_wifi: bool) -> u64 {
    let base = std::path::Path::new("/sys/class/net").join(iface);
    let path = if is_wifi {
        base.join("wireless").join("link")
    } else {
        base.join("speed")
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let v: i64 = match raw.trim().parse() {
        Ok(v) => v,
        Err(_) => return 0,
    };
    if v <= 0 {
        0
    } else {
        v as u64
    }
}

/// 读取 Wi-Fi 频段：`iw dev <iface> info` 的 channel 行（如 "channel 36 (5180 MHz)"）。
/// `iw` 不存在或解析失败时返回空串（角色回退为 WIFI）。
fn wifi_band(iface: &str) -> String {
    let out = run_cmd("iw", &["dev", iface, "info"], Duration::from_secs(5));
    if !out.ok {
        return String::new();
    }
    for line in out.stdout.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("channel ") {
            if rest.contains("(5") || rest.contains("(6") {
                // 例: "36 (5180 MHz)" -> 5GHz；"1 (2412 MHz)" -> 2.4GHz
                if rest.contains("(6") {
                    return "6GHz".to_string();
                }
                return "5GHz".to_string();
            } else if rest.contains("(2") {
                return "2.4GHz".to_string();
            }
        }
    }
    String::new()
}

/// 扫描本机网卡（Linux）。
pub fn scan_all(prefixes: &[String]) -> Vec<NicInfo> {
    let v4 = run_cmd("ip", &["-j", "-4", "addr"], Duration::from_secs(10));
    let v6 = run_cmd("ip", &["-j", "-6", "addr"], Duration::from_secs(10));
    let routes = run_cmd("ip", &["-j", "route"], Duration::from_secs(10));

    let mut blocks = parse_addr_json(&v4.stdout, false);
    for (name, b6) in parse_addr_json(&v6.stdout, true) {
        let b = blocks.entry(name.clone()).or_insert_with(|| IfBlock {
            name: name.clone(),
            ..Default::default()
        });
        if b.ipv6_ll.is_none() {
            b.ipv6_ll = b6.ipv6_ll;
        }
        if b.ipv6_global.is_none() {
            b.ipv6_global = b6.ipv6_global;
        }
        if b.operstate.is_empty() {
            b.operstate = b6.operstate;
        }
    }
    let gateways = parse_gateway_v4(&routes.stdout);

    let mut out = Vec::new();
    for b in blocks.values() {
        if b.name == "lo" || b.name.starts_with("lo") {
            continue;
        }
        if !b.flags.iter().any(|f| f == "UP") {
            continue;
        }
        if b.operstate.eq_ignore_ascii_case("down") {
            continue;
        }
        let Some(ipv4) = b.ipv4.as_deref() else {
            continue;
        };
        if !ipv4_match(ipv4, prefixes) {
            continue;
        }
        let is_wifi = is_wifi_iface(&b.name);
        let speed = read_speed(&b.name, is_wifi);
        let band = if is_wifi {
            wifi_band(&b.name)
        } else {
            String::new()
        };
        let description = if is_wifi {
            "wireless".to_string()
        } else {
            "ethernet".to_string()
        };
        let role = classify_role(&description, speed, is_wifi, &band);
        out.push(NicInfo {
            name: b.name.clone(),
            description,
            role,
            ipv4: ipv4.to_string(),
            gateway_v4: gateways.get(&b.name).cloned().unwrap_or_default(),
            ipv6_ll: b.ipv6_ll.clone().unwrap_or_default(),
            ipv6_global: b.ipv6_global.clone().unwrap_or_default(),
            zone: b.name.clone(), // Linux 的 v6 zone 是接口名
            speed_mbps: speed,
            is_wifi,
            wifi_band: band,
            ifindex: 0,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR4: &str = r#"[
  {"ifindex":1,"ifname":"lo","flags":["LOOPBACK","UP","LOWER_UP"],"operstate":"UNKNOWN","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host"}]},
  {"ifindex":2,"ifname":"enp2s0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"operstate":"UP","addr_info":[{"family":"inet","local":"192.168.8.101","prefixlen":24,"scope":"global"}]},
  {"ifindex":3,"ifname":"wlp3s0","flags":["BROADCAST","MULTICAST"],"operstate":"DOWN","addr_info":[]},
  {"ifindex":6,"ifname":"docker0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"operstate":"UP","addr_info":[{"family":"inet","local":"172.17.0.1","prefixlen":16,"scope":"global"}]}
]"#;

    const ADDR6: &str = r#"[
  {"ifindex":2,"ifname":"enp2s0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"operstate":"UP","addr_info":[
    {"family":"inet6","local":"fe80::8f99:ff69:dc91:d879","prefixlen":64,"scope":"link"},
    {"family":"inet6","local":"2408:843e:ca0:5c91::3","prefixlen":128,"scope":"global"}
  ]}
]"#;

    const ROUTES: &str = r#"[
  {"dst":"default","gateway":"192.168.8.1","dev":"enp2s0","protocol":"dhcp","prefsrc":"192.168.8.101","metric":100},
  {"dst":"192.168.8.0/24","dev":"enp2s0","protocol":"kernel","scope":"link","prefsrc":"192.168.8.101"},
  {"dst":"172.17.0.0/16","dev":"docker0","protocol":"kernel","scope":"link","prefsrc":"172.17.0.1"}
]"#;

    #[test]
    fn test_parse_addr_v4() {
        let m = parse_addr_json(ADDR4, false);
        assert_eq!(m.len(), 4);
        let e = &m["enp2s0"];
        assert_eq!(e.ipv4.as_deref(), Some("192.168.8.101"));
        assert!(e.flags.iter().any(|f| f == "UP"));
        assert_eq!(e.operstate, "UP");
        assert_eq!(m["wlp3s0"].operstate, "DOWN");
        assert!(m["wlp3s0"].ipv4.is_none());
        assert_eq!(m["docker0"].ipv4.as_deref(), Some("172.17.0.1"));
    }

    #[test]
    fn test_parse_addr_v6() {
        let m = parse_addr_json(ADDR6, true);
        let e = &m["enp2s0"];
        assert_eq!(e.ipv6_ll.as_deref(), Some("fe80::8f99:ff69:dc91:d879"));
        assert_eq!(e.ipv6_global.as_deref(), Some("2408:843e:ca0:5c91::3"));
    }

    #[test]
    fn test_parse_gateway() {
        let g = parse_gateway_v4(ROUTES);
        assert_eq!(g.get("enp2s0").map(|s| s.as_str()), Some("192.168.8.1"));
        assert!(!g.contains_key("docker0"));
    }

    #[test]
    fn test_parse_bad_json_is_empty() {
        assert!(parse_addr_json("not json", false).is_empty());
        assert!(parse_gateway_v4("").is_empty());
    }
}
