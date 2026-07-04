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

/// GetIfTable2 一行的关键信息
#[derive(Debug, Clone, Default)]
pub struct IfRow {
    pub alias: String,
    pub desc: String,
    pub ifindex: u32,
    pub speed_mbps: u64,
    pub is_wifi: bool,
    pub up: bool,
    pub in_octets: u64,
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
            let up = r.OperStatus.0 == 1;
            out.push(IfRow {
                alias,
                desc,
                ifindex: r.InterfaceIndex,
                speed_mbps,
                is_wifi,
                up,
                in_octets: r.InOctets,
            });
        }
        FreeMibTable(table as *const _);
    }
    out
}

fn u16z(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 按接口名读当前 RX 累计字节（监控用）
pub fn rx_bytes(iface: &str) -> Result<u64, String> {
    let rows = if_rows();
    rows.iter()
        .find(|r| r.alias == iface)
        .map(|r| r.in_octets)
        .ok_or_else(|| format!("接口不存在: {iface}"))
}

/// 全量扫描（带前缀过滤），返回完整 NicInfo 列表
pub fn scan_all(prefixes: &[String]) -> Vec<NicInfo> {
    let adapters = ipconfig::scan();
    let rows = if_rows();
    let rows_by_alias: HashMap<String, &IfRow> =
        rows.iter().map(|r| (r.alias.clone(), r)).collect();
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
        let row = rows_by_alias.get(&a.name).copied().or_else(|| {
            rows.iter()
                .find(|r| r.alias.eq_ignore_ascii_case(&a.name))
        });
        let wlan = wlans.iter().find(|w| w.name == a.name);
        let is_wifi = row.map(|r| r.is_wifi).unwrap_or(false)
            || is_wifi_name(&a.name)
            || wlan.is_some();
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
