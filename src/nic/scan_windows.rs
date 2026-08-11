//! Windows 网卡扫描：ipconfig(IP) + GetIfTable2(速率/类型/索引) + netsh wlan(频段)
//!
//! 注意：不用 wmic —— Win11 24H2 起系统默认不再预装 wmic；
//! GetIfTable2 是 Vista 以来内置的 Win32 API，永远可用，也是 PROJECT_PLAN
//! 中 NIC 监控选定的方案，扫描/监控统一用它。

#![cfg(windows)]

use super::classify::{classify_role, is_wifi_name};
use super::ipv4_match;
use crate::cmd::{ipconfig, netsh};
use crate::protocol::NicInfo;
use std::collections::HashMap;
use std::net::Ipv4Addr;

/// GetIfTable2 一行的关键信息
#[derive(Debug, Clone, Default)]
pub struct IfRow {
    pub alias: String,
    pub desc: String,
    pub ifindex: u32,
    pub speed_mbps: u64,
    pub is_wifi: bool,
    pub in_octets: u64,
    pub out_octets: u64,
}

/// 读取接口表（扫描 + RX 监控共用）
pub fn if_rows() -> Vec<IfRow> {
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};

    let mut out = Vec::new();
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        let err = GetIfTable2(&mut table);
        if err.is_err() || table.is_null() {
            return out;
        }
        let t = &*table;
        let rows = std::slice::from_raw_parts(t.Table.as_ptr(), t.NumEntries as usize);
        for r in rows {
            let alias = u16z(&r.Alias);
            let desc = u16z(&r.Description);
            let speed_bps = r.ReceiveLinkSpeed.max(r.TransmitLinkSpeed);
            // 有的驱动会报 int64 max 之类的离谱值，过滤掉
            let speed_mbps = if speed_bps > 0 && speed_bps < 1_000_000_000_000 {
                speed_bps / 1_000_000
            } else {
                0
            };
            // ifType 71 = IEEE 802.11 无线
            let is_wifi = r.Type == 71;
            out.push(IfRow {
                alias,
                desc,
                ifindex: r.InterfaceIndex,
                speed_mbps,
                is_wifi,
                in_octets: r.InOctets,
                out_octets: r.OutOctets,
            });
        }
        FreeMibTable(table as *const _);
    }
    out
}

/// 读取 IPv4 默认路由，并按接口索引保留 metric 最小的网关。
///
/// 使用 IP Helper API，避免解析受系统语言影响的 `route print` / `ipconfig` 文本。
fn select_default_gateways_v4<I>(candidates: I) -> HashMap<u32, String>
where
    I: IntoIterator<Item = (u32, u32, Ipv4Addr, Ipv4Addr)>,
{
    let mut selected: HashMap<u32, (u32, Ipv4Addr)> = HashMap::new();
    for (ifindex, metric, destination, gateway) in candidates {
        if ifindex == 0 || !destination.is_unspecified() || gateway.is_unspecified() {
            continue;
        }
        selected
            .entry(ifindex)
            .and_modify(|current| {
                if metric < current.0 {
                    *current = (metric, gateway);
                }
            })
            .or_insert((metric, gateway));
    }
    selected
        .into_iter()
        .map(|(ifindex, (_, gateway))| (ifindex, gateway.to_string()))
        .collect()
}

fn default_gateways_v4() -> HashMap<u32, String> {
    use windows::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIpForwardTable2, MIB_IPFORWARD_TABLE2,
    };
    use windows::Win32::Networking::WinSock::AF_INET;

    unsafe {
        let mut table: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
        let err = GetIpForwardTable2(AF_INET, &mut table);
        if err.is_err() || table.is_null() {
            return HashMap::new();
        }

        let t = &*table;
        let rows = std::slice::from_raw_parts(t.Table.as_ptr(), t.NumEntries as usize);
        let gateways = select_default_gateways_v4(rows.iter().filter_map(|row| {
            (row.DestinationPrefix.PrefixLength == 0).then(|| {
                (
                    row.InterfaceIndex,
                    row.Metric,
                    Ipv4Addr::from(row.DestinationPrefix.Prefix.Ipv4.sin_addr),
                    Ipv4Addr::from(row.NextHop.Ipv4.sin_addr),
                )
            })
        }));
        FreeMibTable(table as *const _);
        gateways
    }
}

fn u16z(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 按接口名读取 RX/TX 累计字节。
pub fn counters(iface: &str) -> Result<(u64, u64), String> {
    let rows = if_rows();
    rows.iter()
        .find(|r| r.alias == iface)
        .or_else(|| rows.iter().find(|r| r.alias.eq_ignore_ascii_case(iface)))
        .map(|r| (r.in_octets, r.out_octets))
        .ok_or_else(|| format!("接口不存在: {iface}"))
}

/// 全量扫描（带前缀过滤），返回完整 NicInfo 列表
pub fn scan_all(prefixes: &[String]) -> Vec<NicInfo> {
    let adapters = ipconfig::scan();
    let rows = if_rows();
    let rows_by_alias: HashMap<String, &IfRow> =
        rows.iter().map(|r| (r.alias.clone(), r)).collect();
    let gateways = default_gateways_v4();
    let wlans = netsh::scan();

    let mut out = Vec::new();
    for a in adapters {
        if a.disconnected {
            continue;
        }
        let Some(ipv4) = a.ipv4.clone() else { continue };
        if !ipv4_match(&ipv4, prefixes) {
            continue;
        }
        let row = rows_by_alias
            .get(&a.name)
            .copied()
            .or_else(|| rows.iter().find(|r| r.alias.eq_ignore_ascii_case(&a.name)));
        let wlan = wlans.iter().find(|w| w.name == a.name);
        let is_wifi =
            row.map(|r| r.is_wifi).unwrap_or(false) || is_wifi_name(&a.name) || wlan.is_some();
        // WiFi 已断开的不要（有 IP 残留的情况）
        if is_wifi {
            if let Some(w) = wlan {
                if !w.connected {
                    continue;
                }
            }
        }
        let band = wlan.map(|w| w.band.clone()).unwrap_or_default();
        let speed = row.map(|r| r.speed_mbps).unwrap_or(0);
        let desc = row.map(|r| r.desc.clone()).unwrap_or_default();
        let ifindex = row.map(|r| r.ifindex).unwrap_or(0);
        let zone = if !a.zone.is_empty() {
            a.zone.clone()
        } else {
            ifindex.to_string()
        };
        let role = classify_role(&desc, speed, is_wifi, &band);
        out.push(NicInfo {
            name: a.name,
            description: desc,
            role,
            ipv4,
            gateway_v4: gateways.get(&ifindex).cloned().unwrap_or_default(),
            ipv6_ll: a.ipv6_ll.unwrap_or_default(),
            ipv6_global: a.ipv6_global.unwrap_or_default(),
            zone,
            speed_mbps: speed,
            is_wifi,
            wifi_band: band,
            ifindex,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_lowest_metric_default_gateway_per_interface() {
        let gateways = select_default_gateways_v4([
            (7, 50, Ipv4Addr::UNSPECIFIED, Ipv4Addr::new(192, 168, 1, 1)),
            (
                7,
                10,
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::new(192, 168, 1, 254),
            ),
            (9, 5, Ipv4Addr::new(10, 0, 0, 0), Ipv4Addr::new(10, 0, 0, 1)),
            (11, 1, Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED),
        ]);

        assert_eq!(gateways.get(&7).map(String::as_str), Some("192.168.1.254"));
        assert!(!gateways.contains_key(&9));
        assert!(!gateways.contains_key(&11));
    }
}
