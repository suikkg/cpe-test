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
#[allow(dead_code)]
pub fn scan() -> Vec<IpcfgAdapter> {
    scan_with_aliases(&[])
}

/// 扫描并允许调用方提供一份**已知接口别名**。
///
/// `adapter_name()` 原本只认中文「适配器 」和英文「 adapter 」两种写法。
/// 德语是 `Ethernet-Adapter`（没有前导空格）、日语是 `アダプター`、
/// 法语是 `Carte`——三者都识别不出来，而 `scan_all()` 的主循环挂在这份结果上，
/// 于是非中英文 Windows 上一块网卡都扫不到，屏幕上只有一句「没有扫到网卡」。
///
/// `GetIfTable2` 是语言无关的，它给得出每块网卡的别名，而 ipconfig 的适配器
/// 标题行正是以那个别名结尾。把别名传进来做兜底匹配，就不再依赖任何一种
/// 界面语言的措辞。
#[cfg(windows)]
pub fn scan_with_aliases(known_aliases: &[String]) -> Vec<IpcfgAdapter> {
    let out = run_cmd("ipconfig", &["/all"], Duration::from_secs(20));
    parse_with_aliases(&out.merged(), known_aliases)
}

/// 头部形如：
///   `以太网适配器 以太网:` / `无线局域网适配器 WLAN:` / `Ethernet adapter Ethernet 2:`
/// 字段行形如：
///   `   IPv4 地址 . . . . . . . . . . . . : 192.168.1.2(首选)`
///   `   本地链接 IPv6 地址. . . . . . . . : fe80::c4b:1234%12(首选)`
#[allow(dead_code)]
pub fn parse(text: &str) -> Vec<IpcfgAdapter> {
    parse_with_aliases(text, &[])
}

pub fn parse_with_aliases(text: &str, known_aliases: &[String]) -> Vec<IpcfgAdapter> {
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
            if let Some(name) = adapter_name(head, known_aliases) {
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

/// 从头部行提取适配器名。
///
/// 先按措辞找（中文「适配器 」/ 英文「 adapter 」/ 德语「-adapter 」），
/// 找不到再拿 `GetIfTable2` 给的别名兜底——标题行以别名结尾，这条路径
/// 不依赖任何界面语言。两者都不中就返回 `None`。
fn adapter_name(head: &str, known_aliases: &[String]) -> Option<String> {
    if let Some(idx) = head.find("适配器 ") {
        let name = head[idx + "适配器 ".len()..].trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    let low = head.to_lowercase();
    // 德语把连字符当分隔符（`Ethernet-Adapter Ethernet`），所以不能只认前导空格。
    for marker in [" adapter ", "-adapter "] {
        if let Some(idx) = low.find(marker) {
            let name = head[idx + marker.len()..].trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    // 兜底：标题行以接口别名结尾。取最长匹配，避免「以太网」把「以太网 6」截断。
    known_aliases
        .iter()
        .filter(|alias| !alias.is_empty() && head.ends_with(alias.as_str()))
        .max_by_key(|alias| alias.len())
        .map(|alias| alias.to_string())
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
    /// 非中英文 Windows 上，适配器标题行的措辞一个都对不上。
    ///
    /// `scan_all()` 的主循环挂在这份解析结果上，所以「一条都认不出来」
    /// 等于「一块网卡都扫不到」，而屏幕上只有一句「没有扫到网卡」——
    /// 没有任何线索指向语言。别名来自 GetIfTable2，它不受界面语言影响。
    #[test]
    fn adapter_headers_in_other_locales_are_recognised_by_interface_alias() {
        let japanese = "\
イーサネット アダプター イーサネット 2:

   IPv4 アドレス . . . . . . . . . . . .: 192.168.8.20(優先)
";
        assert!(
            parse(japanese).is_empty(),
            "措辞路径本来就认不出日语，这条用例的前提是它认不出"
        );

        let aliases = vec!["イーサネット 2".to_string(), "Wi-Fi".to_string()];
        let parsed = parse_with_aliases(japanese, &aliases);
        assert_eq!(parsed.len(), 1, "别名兜底必须认出来");
        assert_eq!(parsed[0].name, "イーサネット 2");
        assert_eq!(parsed[0].ipv4.as_deref(), Some("192.168.8.20"));
    }

    /// 德语用连字符连接（`Ethernet-Adapter`），没有前导空格，
    /// 所以只认 `" adapter "` 的写法会漏掉它。
    #[test]
    fn a_hyphenated_german_adapter_header_is_recognised_without_any_alias_hint() {
        let german = "\
Ethernet-Adapter Ethernet:

   IPv4-Adresse  . . . . . . . . . . : 192.168.8.30(Bevorzugt)
";
        let parsed = parse(german);
        assert_eq!(parsed.len(), 1, "德语写法要能直接认出来");
        assert_eq!(parsed[0].name, "Ethernet");
        assert_eq!(parsed[0].ipv4.as_deref(), Some("192.168.8.30"));
    }

    /// 别名兜底要取最长匹配，否则「以太网」会把「以太网 6」截断成另一块网卡。
    #[test]
    fn the_alias_fallback_prefers_the_longest_match() {
        let text = "\
以太网适配器 以太网 6:

   IPv4 地址 . . . . . . . . . . . . : 192.168.8.40(首选)
";
        let aliases = vec!["以太网".to_string(), "以太网 6".to_string()];
        let parsed = parse_with_aliases(text, &aliases);
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].name, "以太网 6",
            "中文措辞本来就能认出全名，别名兜底不能把它改短"
        );
    }

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
