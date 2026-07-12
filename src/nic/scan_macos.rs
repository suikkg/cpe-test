//! macOS 网卡扫描：ifconfig(IP/速率) + networksetup(WiFi判定) + system_profiler(频段)
//! macOS 仅用于开发/模拟测试，最终生产环境是两台 Windows。

#![cfg(target_os = "macos")]

use super::classify::classify_role;
use super::ipv4_match;
use crate::protocol::NicInfo;
use crate::util::run_cmd;
use regex::Regex;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
struct Block {
    name: String,
    ipv4: Option<String>,
    ipv6_ll: Option<String>,
    ipv6_global: Option<String>,
    inactive: bool,
}

fn parse_ifconfig(text: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut cur: Option<Block> = None;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') && line.contains(": flags=") {
            if let Some(b) = cur.take() {
                out.push(b);
            }
            let name = line.split(':').next().unwrap_or("").trim().to_string();
            cur = Some(Block {
                name,
                ..Default::default()
            });
            continue;
        }
        let Some(b) = cur.as_mut() else { continue };
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("inet ") {
            let ip = rest.split_whitespace().next().unwrap_or("");
            if b.ipv4.is_none() && !ip.is_empty() {
                b.ipv4 = Some(ip.to_string());
            }
        } else if let Some(rest) = t.strip_prefix("inet6 ") {
            let addr = rest.split_whitespace().next().unwrap_or("");
            let low = addr.to_lowercase();
            if low.starts_with("fe80") {
                if b.ipv6_ll.is_none() {
                    b.ipv6_ll = Some(low.split('%').next().unwrap_or(&low).to_string());
                }
            } else if (low.starts_with('2') || low.starts_with('3')) && b.ipv6_global.is_none() {
                b.ipv6_global = Some(low.split('%').next().unwrap_or(&low).to_string());
            }
        } else if t.starts_with("status:") {
            b.inactive = t.contains("inactive");
        }
    }
    if let Some(b) = cur.take() {
        out.push(b);
    }
    out
}

/// networksetup -listallhardwareports：device -> 端口名（判定 Wi-Fi）
fn hardware_ports() -> HashMap<String, String> {
    let out = run_cmd(
        "networksetup",
        &["-listallhardwareports"],
        Duration::from_secs(10),
    );
    let mut map = HashMap::new();
    let mut port = String::new();
    for line in out.stdout.lines() {
        let t = line.trim();
        if let Some(p) = t.strip_prefix("Hardware Port:") {
            port = p.trim().to_string();
        } else if let Some(d) = t.strip_prefix("Device:") {
            map.insert(d.trim().to_string(), port.clone());
        }
    }
    map
}

/// ifconfig -m 探测速率，最多 3 次（协商值可能抖动，取最大）
fn probe_speed(iface: &str) -> u64 {
    let re = Regex::new(r"\((\d+)(G?)[Bb]ase").expect("regex");
    let mut best: u64 = 0;
    for i in 0..3 {
        let out = run_cmd("ifconfig", &["-m", iface], Duration::from_secs(5));
        for cap in re.captures_iter(&out.stdout) {
            let v: u64 = cap[1].parse().unwrap_or(0);
            let v = if &cap[2] == "G" { v * 1000 } else { v };
            // 只看当前 media: 行的第一个匹配就够，取全局最大做兜底
            if v > best {
                best = v;
            }
        }
        if best > 0 {
            break;
        }
        if i < 2 {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    best
}

/// system_profiler 拿 WiFi 频段与 PHY 速率（慢，几秒；仅有 WiFi 候选时调用）
fn airport_info() -> (String, u64) {
    let out = run_cmd(
        "system_profiler",
        &["SPAirPortDataType"],
        Duration::from_secs(25),
    );
    let mut band = String::new();
    let mut rate: u64 = 0;
    let mut in_current = false;
    for line in out.stdout.lines() {
        let t = line.trim();
        if t.starts_with("Current Network Information:") {
            in_current = true;
            continue;
        }
        if in_current {
            if t.starts_with("Channel:") {
                if t.contains("5GHz") {
                    band = "5GHz".into();
                } else if t.contains("2.4GHz") || t.contains("2,4GHz") {
                    band = "2.4GHz".into();
                } else if t.contains("6GHz") {
                    band = "6GHz".into();
                }
            } else if let Some(r) = t.strip_prefix("Transmit Rate:") {
                rate = r.trim().parse::<f64>().unwrap_or(0.0) as u64;
            } else if t.starts_with("Other Local Wi-Fi Networks:") {
                break;
            }
        }
    }
    (band, rate)
}

fn parse_route_gateway_v4(text: &str) -> String {
    text.lines()
        .find_map(|line| {
            let gateway = line.trim().strip_prefix("gateway:")?.trim();
            let addr = gateway.parse::<Ipv4Addr>().ok()?;
            (!addr.is_unspecified()).then(|| addr.to_string())
        })
        .unwrap_or_default()
}

/// 按 BSD 接口作用域获取默认网关，避免把另一张同时在线网卡的默认路由误配过来。
fn probe_gateway_v4(iface: &str) -> String {
    let out = run_cmd(
        "route",
        &["-n", "get", "-inet", "-ifscope", iface, "default"],
        Duration::from_secs(5),
    );
    if out.ok {
        parse_route_gateway_v4(&out.stdout)
    } else {
        String::new()
    }
}

pub fn scan_all(prefixes: &[String]) -> Vec<NicInfo> {
    let text = run_cmd("ifconfig", &["-a"], Duration::from_secs(10)).stdout;
    let blocks = parse_ifconfig(&text);
    let ports = hardware_ports();

    // 先筛出候选，避免对无关接口做慢探测
    let cands: Vec<&Block> = blocks
        .iter()
        .filter(|b| {
            b.name != "lo0"
                && !b.inactive
                && b.ipv4
                    .as_deref()
                    .map(|ip| ipv4_match(ip, prefixes))
                    .unwrap_or(false)
        })
        .collect();

    let any_wifi = cands.iter().any(|b| {
        ports
            .get(&b.name)
            .map(|p| p.contains("Wi-Fi") || p.contains("AirPort"))
            .unwrap_or(false)
    });
    let (wifi_band, wifi_rate) = if any_wifi {
        airport_info()
    } else {
        (String::new(), 0)
    };

    let mut out = Vec::new();
    for b in cands {
        let port_name = ports.get(&b.name).cloned().unwrap_or_default();
        let is_wifi = port_name.contains("Wi-Fi") || port_name.contains("AirPort");
        let speed = if is_wifi {
            wifi_rate
        } else {
            probe_speed(&b.name)
        };
        let band = if is_wifi {
            wifi_band.clone()
        } else {
            String::new()
        };
        let role = classify_role(&port_name, speed, is_wifi, &band);
        out.push(NicInfo {
            name: b.name.clone(),
            description: port_name,
            role,
            ipv4: b.ipv4.clone().unwrap_or_default(),
            gateway_v4: probe_gateway_v4(&b.name),
            ipv6_ll: b.ipv6_ll.clone().unwrap_or_default(),
            ipv6_global: b.ipv6_global.clone().unwrap_or_default(),
            zone: b.name.clone(), // macOS 的 v6 zone 就是接口名
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

    const SAMPLE: &str = r#"lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384
	inet 127.0.0.1 netmask 0xff000000
	inet6 ::1 prefixlen 128
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	inet6 fe80::1c2e:aabb:ccdd:eeff%en0 prefixlen 64 secured scopeid 0xe
	inet 192.168.8.100 netmask 0xffffff00 broadcast 192.168.8.255
	inet6 2408:8207:aabb:ccdd::1 prefixlen 64 autoconf secured
	status: active
en5: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	inet 192.168.1.50 netmask 0xffffff00 broadcast 192.168.1.255
	status: inactive
"#;

    #[test]
    fn test_parse_ifconfig() {
        let v = parse_ifconfig(SAMPLE);
        assert_eq!(v.len(), 3);
        assert_eq!(v[1].name, "en0");
        assert_eq!(v[1].ipv4.as_deref(), Some("192.168.8.100"));
        assert_eq!(v[1].ipv6_ll.as_deref(), Some("fe80::1c2e:aabb:ccdd:eeff"));
        assert_eq!(v[1].ipv6_global.as_deref(), Some("2408:8207:aabb:ccdd::1"));
        assert!(!v[1].inactive);
        assert!(v[2].inactive);
    }

    #[test]
    fn test_parse_route_gateway_v4() {
        let route = r#"   route to: default
destination: default
       mask: default
    gateway: 192.168.8.1
  interface: en0
"#;
        assert_eq!(parse_route_gateway_v4(route), "192.168.8.1");
        assert!(parse_route_gateway_v4("gateway: 0.0.0.0\n").is_empty());
        assert!(parse_route_gateway_v4("gateway: fe80::1%en0\n").is_empty());
        assert!(parse_route_gateway_v4("route: not in table\n").is_empty());
    }
}
