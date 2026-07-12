//! 网卡扫描（分平台实现）+ 角色分类 + RX 监控

pub mod classify;
pub mod monitor;

#[cfg(target_os = "macos")]
pub mod scan_macos;
#[cfg(windows)]
pub mod scan_windows;

use crate::protocol::HostInfo;
#[cfg(not(any(windows, target_os = "macos")))]
use crate::protocol::NicInfo;
use crate::util::{hostname, os_name};

/// 扫描本机网卡（按 IPv4 前缀过滤，按角色排序）
pub fn scan_host(ipv4_prefixes: &[String]) -> HostInfo {
    #[cfg(windows)]
    let mut ifs = scan_windows::scan_all(ipv4_prefixes);
    #[cfg(target_os = "macos")]
    let mut ifs = scan_macos::scan_all(ipv4_prefixes);
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut ifs: Vec<NicInfo> = {
        let _ = ipv4_prefixes;
        Vec::new()
    };

    ifs.sort_by(|a, b| {
        classify::role_rank(&a.role)
            .cmp(&classify::role_rank(&b.role))
            .then_with(|| a.name.cmp(&b.name))
    });
    HostInfo {
        hostname: hostname(),
        os: os_name(),
        interfaces: ifs,
    }
}

/// IPv4 是否匹配任一前缀（前缀列表为空 = 全放行）
#[cfg(any(windows, target_os = "macos"))]
pub fn ipv4_match(ip: &str, prefixes: &[String]) -> bool {
    if prefixes.is_empty() {
        return true;
    }
    prefixes.iter().any(|p| !p.is_empty() && ip.starts_with(p))
}

/// 打印网卡表（scan 子命令 / 菜单展示共用）
pub fn format_nic_table(side: &str, host: &HostInfo) -> String {
    let mut s = format!("{} {} ({})：\n", side, host.hostname, host.os);
    if host.interfaces.is_empty() {
        s.push_str("  (未发现匹配的网卡，请检查网线/WiFi连接 和 ipv4_prefixes 配置)\n");
        return s;
    }
    for (i, n) in host.interfaces.iter().enumerate() {
        let mut extra = Vec::new();
        if n.speed_mbps > 0 {
            extra.push(format!("{}Mbps", n.speed_mbps));
        }
        if !n.wifi_band.is_empty() {
            extra.push(n.wifi_band.clone());
        }
        if !n.gateway_v4.is_empty() {
            extra.push(format!("gw:{}", n.gateway_v4));
        }
        if !n.ipv6_ll.is_empty() {
            extra.push(format!("v6:{}", n.ipv6_ll));
        }
        s.push_str(&format!(
            "  [{}] {:<10} {:<16} {:<16} {}\n",
            i + 1,
            n.role,
            n.name,
            n.ipv4,
            extra.join(" | ")
        ));
    }
    s
}
