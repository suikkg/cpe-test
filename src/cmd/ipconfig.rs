//! 解析 `ipconfig /all` 输出（Windows，中英文兼容，GBK 已在 run_cmd 解码）

#[cfg(windows)]
use crate::util::run_cmd;
use regex::Regex;
#[cfg(windows)]
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct IpcfgAdapter {
    pub name: String,
    pub ipv4: Option<String>,
    /// fe80::（不带 %zone）
    pub ipv6_ll: Option<String>,
    /// fe80 的 zone（%后面的接口索引数字）
    pub zone: String,
    pub ipv6_global: Option<String>,
    pub disconnected: bool,
}

#[cfg(windows)]
pub fn scan() -> Vec<IpcfgAdapter> {
    let out = run_cmd("ipconfig", &["/all"], Duration::from_secs(20));
    parse(&out.merged())
}

/// 头部形如：
///   `以太网适配器 以太网:` / `无线局域网适配器 WLAN:` / `Ethernet adapter Ethernet 2:`
/// 字段行形如：
///   `   IPv4 地址 . . . . . . . . . . . . : 192.168.1.2(首选)`
///   `   本地链接 IPv6 地址. . . . . . . . : fe80::c4b:1234%12(首选)`
pub fn parse(text: &str) -> Vec<IpcfgAdapter> {
    let field_re = Regex::new(r"^\s{2,}(.+?)[\s.]*:\s*(.*)$").expect("regex");
    let mut out: Vec<IpcfgAdapter> = Vec::new();
    let mut cur: Option<IpcfgAdapter> = None;

    for line in text.lines() {
        let lt = line.trim_end();
        // 适配器头：非缩进行、以冒号结尾
        if !lt.is_empty() && !line.starts_with(' ') && !line.starts_with('\t') && lt.ends_with(':')
        {
            if let Some(a) = cur.take() {
                out.push(a);
            }
            let head = lt.trim_end_matches(':').trim();
            if let Some(name) = adapter_name(head) {
                cur = Some(IpcfgAdapter {
                    name,
                    ..Default::default()
                });
            }
            continue;
        }
        let Some(a) = cur.as_mut() else { continue };
        let Some(cap) = field_re.captures(line) else {
            continue;
        };
        let key = cap.get(1).map(|m| m.as_str()).unwrap_or("").trim();
        let val = cap.get(2).map(|m| m.as_str()).unwrap_or("").trim();
        if val.is_empty() {
            continue;
        }
        let key_l = key.to_lowercase();
        if key.contains("IPv4") || key_l == "ip address" {
            if a.ipv4.is_none() {
                let v = strip_paren(val);
                if looks_ipv4(&v) {
                    a.ipv4 = Some(v);
                }
            }
        } else if key.contains("IPv6") {
            let v = strip_paren(val);
            let vl = v.to_lowercase();
            if vl.starts_with("fe80") {
                if a.ipv6_ll.is_none() {
                    if let Some((addr, zone)) = vl.split_once('%') {
                        a.ipv6_ll = Some(addr.to_string());
                        a.zone = zone
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric())
                            .collect();
                    } else {
                        a.ipv6_ll = Some(vl.clone());
                    }
                }
            } else if (vl.starts_with('2') || vl.starts_with('3'))
                && vl.contains(':')
                && a.ipv6_global.is_none()
            {
                a.ipv6_global = Some(vl.split('%').next().unwrap_or(&vl).to_string());
            }
        } else if (key.contains("媒体状态") || key_l.contains("media state"))
            && (val.contains("已断开") || val.to_lowercase().contains("disconnected"))
        {
            a.disconnected = true;
        }
    }
    if let Some(a) = cur.take() {
        out.push(a);
    }
    out
}

/// 从头部行提取适配器名
fn adapter_name(head: &str) -> Option<String> {
    if let Some(idx) = head.find("适配器 ") {
        let name = &head[idx + "适配器 ".len()..];
        let n = name.trim();
        if !n.is_empty() {
            return Some(n.to_string());
        }
    }
    let low = head.to_lowercase();
    if let Some(idx) = low.find(" adapter ") {
        let name = &head[idx + " adapter ".len()..];
        let n = name.trim();
        if !n.is_empty() {
            return Some(n.to_string());
        }
    }
    None
}

fn strip_paren(v: &str) -> String {
    v.split('(').next().unwrap_or(v).trim().to_string()
}

fn looks_ipv4(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CN: &str = r#"
Windows IP 配置

   主机名  . . . . . . . . . . . . . : DESKTOP-ABC

以太网适配器 以太网:

   连接特定的 DNS 后缀 . . . . . . . :
   描述. . . . . . . . . . . . . . . : Realtek PCIe 2.5GbE Family Controller
   物理地址. . . . . . . . . . . . . : 00-11-22-33-44-55
   本地链接 IPv6 地址. . . . . . . . : fe80::c4b:1a2b:3c4d:5e6f%12(首选)
   IPv4 地址 . . . . . . . . . . . . : 192.168.1.2(首选)
   子网掩码  . . . . . . . . . . . . : 255.255.255.0

以太网适配器 以太网 2:

   媒体状态  . . . . . . . . . . . . : 媒体已断开连接
   连接特定的 DNS 后缀 . . . . . . . :

无线局域网适配器 WLAN:

   IPv6 地址 . . . . . . . . . . . . : 240e:aaaa:bbbb::1234(首选)
   本地链接 IPv6 地址. . . . . . . . : fe80::aaaa:bbbb:cccc:dddd%8(首选)
   IPv4 地址 . . . . . . . . . . . . : 192.168.1.5(首选)

隧道适配器 Teredo Tunneling Pseudo-Interface:

   媒体状态  . . . . . . . . . . . . : 媒体已断开连接
"#;

    #[test]
    fn test_parse_cn() {
        let v = parse(SAMPLE_CN);
        assert_eq!(v.len(), 4);
        let eth = &v[0];
        assert_eq!(eth.name, "以太网");
        assert_eq!(eth.ipv4.as_deref(), Some("192.168.1.2"));
        assert_eq!(eth.ipv6_ll.as_deref(), Some("fe80::c4b:1a2b:3c4d:5e6f"));
        assert_eq!(eth.zone, "12");
        assert!(!eth.disconnected);
        let eth2 = &v[1];
        assert_eq!(eth2.name, "以太网 2");
        assert!(eth2.disconnected);
        let wlan = &v[2];
        assert_eq!(wlan.name, "WLAN");
        assert_eq!(wlan.ipv4.as_deref(), Some("192.168.1.5"));
        assert_eq!(wlan.ipv6_global.as_deref(), Some("240e:aaaa:bbbb::1234"));
        assert_eq!(wlan.zone, "8");
    }

    const SAMPLE_EN: &str = r#"
Windows IP Configuration

Ethernet adapter Ethernet 3:

   Connection-specific DNS Suffix  . :
   Link-local IPv6 Address . . . . . : fe80::1111:2222:3333:4444%15(Preferred)
   IPv4 Address. . . . . . . . . . . : 192.168.8.100(Preferred)

Wireless LAN adapter Wi-Fi:

   Media State . . . . . . . . . . . : Media disconnected
"#;

    #[test]
    fn test_parse_en() {
        let v = parse(SAMPLE_EN);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "Ethernet 3");
        assert_eq!(v[0].ipv4.as_deref(), Some("192.168.8.100"));
        assert_eq!(v[0].zone, "15");
        assert_eq!(v[1].name, "Wi-Fi");
        assert!(v[1].disconnected);
    }
}
