//! spec -> 任务单元(Unit) 生成 + 端口分配 + IP 自适应解析
//!
//! 配置写 "master:SGMII2.5G" 这类角色引用，运行时解析成实际网卡/IP。
//! 换电脑不用改配置：角色识别对了，IP 自动跟着变。

use crate::config::{Config, TestSpec, UdpProfile};
use crate::protocol::{HostInfo, NicInfo};
use crate::util::{md5_hex, same_slash24};

pub const PORT_BASE: u16 = 56000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Master,
    Agent,
}

impl Side {
    pub fn cn(&self) -> &'static str {
        match self {
            Side::Master => "主控",
            Side::Agent => "辅测",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub side: Side,
    pub pc: String,
    pub nic: NicInfo,
}

impl Endpoint {
    pub fn brief(&self) -> String {
        format!("{} {}", self.side.cn(), self.nic.brief())
    }
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.side.cn(), self.nic.name, self.nic.ipv4)
    }
}

/// 规范化后的测试规格（配置文件 tests[] 与交互菜单都产出它）
#[derive(Clone, Debug)]
pub struct SpecNorm {
    pub name: String,
    pub src: Endpoint,
    pub dst: Endpoint,
    /// ab / ba / bidir
    pub directions: Vec<String>,
    /// iperf / ping
    pub kinds: Vec<String>,
    /// tcp / udp
    pub transports: Vec<String>,
    /// v4 / v6
    pub ipvers: Vec<String>,
    pub streams: u32,
    pub duration: u64,
    pub ping_count: u32,
    pub payload_sizes: Vec<u32>,
    pub tcp_windows: Vec<String>,
    pub udp_profiles: Vec<UdpProfile>,
    pub udp_limit: bool,
}

#[derive(Clone, Debug)]
pub struct IperfTask {
    pub v6: bool,
    pub udp: bool,
    pub profile_name: String,
    pub profile_label: String,
    pub src: Endpoint,
    pub dst: Endpoint,
    pub port: u16,
    pub duration: u64,
    pub extra: Vec<String>,
    pub stream_idx: usize,
}

#[derive(Clone, Debug)]
pub struct PingTask {
    pub v6: bool,
    pub src: Endpoint,
    pub dst: Endpoint,
    pub count: u32,
    pub payload: u32,
}

#[derive(Clone, Debug)]
pub enum LegKind {
    IperfSingle(IperfTask),
    IperfGroup { name: String, streams: Vec<IperfTask> },
    Ping(PingTask),
}

#[derive(Clone, Debug)]
pub struct Leg {
    /// "" / "ab" / "ba"
    pub tag: String,
    pub kind: LegKind,
}

#[derive(Clone, Debug)]
pub struct Unit {
    pub id: String,
    pub title: String,
    pub bidir: bool,
    pub legs: Vec<Leg>,
    pub est_secs: u64,
}

/// v6 地址三元组（client 绑定 / client 目标 / server 绑定），link-local 自动带 zone
#[derive(Clone, Debug)]
pub struct V6Addrs {
    pub client_bind: String,
    pub client_target: String,
    pub server_bind: String,
}

/// 选 v6 地址：两端都有 fe80 优先用 fe80（CPE 局域网标准场景），否则都有全局地址用全局
/// v6 地址一律不带 %zone：Windows iperf3/ping 都不接受 %xx 语法
pub fn v6_addrs(src: &NicInfo, dst: &NicInfo) -> Option<V6Addrs> {
    if !src.ipv6_ll.is_empty() && !dst.ipv6_ll.is_empty() {
        Some(V6Addrs {
            client_bind: src.ipv6_ll.clone(),
            client_target: dst.ipv6_ll.clone(),
            server_bind: dst.ipv6_ll.clone(),
        })
    } else if !src.ipv6_global.is_empty() && !dst.ipv6_global.is_empty() {
        Some(V6Addrs {
            client_bind: src.ipv6_global.clone(),
            client_target: dst.ipv6_global.clone(),
            server_bind: dst.ipv6_global.clone(),
        })
    } else {
        None
    }
}

/// 解析 "master:SGMII2.5G" / "agent:NAME=以太网 2" 为具体端点
pub fn resolve_endpoint(
    sel: &str,
    master: &HostInfo,
    agent: &HostInfo,
) -> Result<Endpoint, String> {
    let (side_s, rest) = sel
        .split_once(':')
        .ok_or_else(|| format!("端点格式错误(应为 side:ROLE 或 side:NAME=接口名): {sel}"))?;
    let (side, host) = match side_s.trim().to_lowercase().as_str() {
        "master" | "local" | "主控" => (Side::Master, master),
        "agent" | "remote" | "辅测" => (Side::Agent, agent),
        other => return Err(format!("端点侧别无效(master/agent): {other}")),
    };
    let rest = rest.trim();
    let nic = if let Some(name) = rest.strip_prefix("NAME=").or_else(|| rest.strip_prefix("name=")) {
        let n = name.trim();
        host.interfaces
            .iter()
            .find(|i| i.name == n)
            .or_else(|| {
                host.interfaces
                    .iter()
                    .find(|i| i.name.eq_ignore_ascii_case(n))
            })
            .cloned()
            .ok_or_else(|| {
                format!(
                    "{}侧找不到接口名 {}。可用: {}",
                    side.cn(),
                    n,
                    host.interfaces
                        .iter()
                        .map(|i| i.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?
    } else {
        let role = rest.to_uppercase();
        host.interfaces
            .iter()
            .find(|i| i.role.eq_ignore_ascii_case(&role))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "{}侧找不到角色 {}。可用: {}",
                    side.cn(),
                    role,
                    host.interfaces
                        .iter()
                        .map(|i| format!("{}({})", i.role, i.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?
    };
    Ok(Endpoint {
        side,
        pc: host.hostname.clone(),
        nic,
    })
}

/// 配置文件 TestSpec -> SpecNorm
pub fn spec_from_config(
    t: &TestSpec,
    cfg: &Config,
    master: &HostInfo,
    agent: &HostInfo,
) -> Result<SpecNorm, String> {
    let src = resolve_endpoint(&t.src, master, agent)?;
    let dst = resolve_endpoint(&t.dst, master, agent)?;
    if src.key() == dst.key() {
        return Err(format!("测试 {} 的源和目标是同一个网口", t.name));
    }
    Ok(SpecNorm {
        name: if t.name.is_empty() {
            format!("{}->{}", t.src, t.dst)
        } else {
            t.name.clone()
        },
        src,
        dst,
        directions: t.direction.directions(),
        kinds: t.kinds.iter().map(|k| k.to_lowercase()).collect(),
        transports: t.transports.iter().map(|k| k.to_lowercase()).collect(),
        ipvers: t.ip.iter().map(|k| k.to_lowercase()).collect(),
        streams: t.streams.clamp(1, 32),
        duration: t.iperf_duration.unwrap_or(cfg.iperf.duration).clamp(1, 86400),
        ping_count: t.ping_count.unwrap_or(cfg.ping.count).clamp(1, 100_000),
        payload_sizes: t
            .ping_payload_sizes
            .clone()
            .unwrap_or_else(|| cfg.ping.payload_sizes.clone()),
        tcp_windows: t
            .tcp_windows
            .clone()
            .unwrap_or_else(|| cfg.iperf.tcp_windows.clone()),
        udp_profiles: t
            .udp_profiles
            .clone()
            .unwrap_or_else(|| cfg.iperf.udp_profiles.clone()),
        udp_limit: cfg.limit_udp_by_link_speed,
    })
}

/// UDP 按发送口速率允许的流数（WiFi 发送口/未知速率不裁剪）
fn allowed_udp_streams(sender: &Endpoint, prof: &UdpProfile, want: u32, limit: bool) -> u32 {
    if !limit || sender.nic.is_wifi || sender.nic.role.starts_with("WIFI") {
        return want;
    }
    let speed = sender.nic.speed_mbps;
    if speed == 0 {
        return want;
    }
    let Some(bw) = prof.bandwidth_mbps() else {
        return want;
    };
    if bw <= 0.0 {
        return want;
    }
    let max_n = (speed as f64 / bw).floor() as u32;
    max_n.min(want)
}

fn dir_pairs<'a>(
    spec: &'a SpecNorm,
    dir: &str,
) -> Vec<(&'a Endpoint, &'a Endpoint, &'static str)> {
    match dir {
        "ab" => vec![(&spec.src, &spec.dst, "")],
        "ba" => vec![(&spec.dst, &spec.src, "")],
        "bidir" => vec![(&spec.src, &spec.dst, "ab"), (&spec.dst, &spec.src, "ba")],
        _ => vec![],
    }
}

fn ep_id(e: &Endpoint) -> String {
    format!("{}|{}|{}", e.pc, e.nic.name, e.nic.ipv4)
}

/// 生成全部任务单元。返回 (units, 提示信息列表)
pub fn build_units(
    specs: &[SpecNorm],
    require_same_subnet: bool,
    next_port: &mut u16,
) -> (Vec<Unit>, Vec<String>) {
    let mut units: Vec<Unit> = Vec::new();
    let mut notices: Vec<String> = Vec::new();

    for spec in specs {
        for dir in &spec.directions {
            let bidir = dir == "bidir";
            let pairs = dir_pairs(spec, dir);
            if pairs.is_empty() {
                continue;
            }
            let arrow = if bidir { "<->" } else { "->" };
            let route_str = format!(
                "{} {} {}",
                pairs[0].0.brief(),
                arrow,
                if bidir {
                    pairs[0].1.brief()
                } else {
                    pairs[0].1.brief()
                }
            );

            for ipver in &spec.ipvers {
                let v6 = ipver == "v6";
                let ip_tag = if v6 { "V6" } else { "V4" };
                if v6 && v6_addrs(&spec.src.nic, &spec.dst.nic).is_none() {
                    notices.push(format!(
                        "跳过 {} {} IPv6：两端缺少可用的 IPv6 地址",
                        spec.name, route_str
                    ));
                    continue;
                }

                // ---------- iperf ----------
                if spec.kinds.iter().any(|k| k == "iperf") {
                    let cross = spec.src.side != spec.dst.side;
                    let same24_ok = !cross
                        || !require_same_subnet
                        || same_slash24(&spec.src.nic.ipv4, &spec.dst.nic.ipv4);
                    if !v6 && !same24_ok {
                        notices.push(format!(
                            "跳过 {} 的 iperf：两端 IPv4 不同网段 ({} vs {})，无法直连灌包（ping 不受限）",
                            spec.name, spec.src.nic.ipv4, spec.dst.nic.ipv4
                        ));
                    } else {
                        for tr in &spec.transports {
                            if tr == "tcp" {
                                for w in &spec.tcp_windows {
                                    let pname = format!("tcp_w{}_P{}", w, spec.streams);
                                    let plabel =
                                        format!("TCP -w {} -P {}", w, spec.streams);
                                    let mut legs = Vec::new();
                                    for (s, d, tag) in &pairs {
                                        let t = IperfTask {
                                            v6,
                                            udp: false,
                                            profile_name: pname.clone(),
                                            profile_label: plabel.clone(),
                                            src: (*s).clone(),
                                            dst: (*d).clone(),
                                            port: alloc_port(next_port),
                                            duration: spec.duration,
                                            extra: vec![
                                                "-w".into(),
                                                w.clone(),
                                                "-P".into(),
                                                spec.streams.to_string(),
                                            ],
                                            stream_idx: 0,
                                        };
                                        legs.push(Leg {
                                            tag: tag.to_string(),
                                            kind: LegKind::IperfSingle(t),
                                        });
                                    }
                                    let title = format!(
                                        "{}IPERF {} {} | {}",
                                        if bidir { "★★双向 " } else { "" },
                                        ip_tag,
                                        plabel,
                                        route_str
                                    );
                                    let id = md5_hex(&format!(
                                        "iperf_v1|{}|tcp|{}|{}|{}|{}|{}",
                                        ip_tag,
                                        pname,
                                        spec.duration,
                                        ep_id(&spec.src),
                                        ep_id(&spec.dst),
                                        dir
                                    ));
                                    units.push(Unit {
                                        id,
                                        title,
                                        bidir,
                                        legs,
                                        est_secs: spec.duration + 10,
                                    });
                                }
                            } else if tr == "udp" {
                                for prof in &spec.udp_profiles {
                                    // 每个方向腿按各自发送口限流
                                    let mut leg_streams: Vec<u32> = Vec::new();
                                    let mut blocked: Option<String> = None;
                                    for (s, _d, _tag) in &pairs {
                                        let n = allowed_udp_streams(
                                            s,
                                            prof,
                                            spec.streams,
                                            spec.udp_limit,
                                        );
                                        if n == 0 {
                                            blocked = Some(format!(
                                                "跳过 {} {}：发送口 {} 速率 {}Mbps 不足以承载 {}",
                                                spec.name,
                                                prof.label(),
                                                s.nic.name,
                                                s.nic.speed_mbps,
                                                prof.label()
                                            ));
                                        }
                                        leg_streams.push(n);
                                    }
                                    if let Some(msg) = blocked {
                                        notices.push(msg);
                                        continue;
                                    }
                                    let mut legs = Vec::new();
                                    let mut max_n = 1;
                                    for ((s, d, tag), n) in
                                        pairs.iter().zip(leg_streams.iter())
                                    {
                                        let n = *n;
                                        max_n = max_n.max(n);
                                        let mut extra: Vec<String> =
                                            vec!["-b".into(), prof.bandwidth.clone()];
                                        if let Some(l) = &prof.length {
                                            extra.push("-l".into());
                                            extra.push(l.clone());
                                        }
                                        let mk = |idx: usize, port: u16| IperfTask {
                                            v6,
                                            udp: true,
                                            profile_name: prof.name(),
                                            profile_label: prof.label(),
                                            src: (*s).clone(),
                                            dst: (*d).clone(),
                                            port,
                                            duration: spec.duration,
                                            extra: extra.clone(),
                                            stream_idx: idx,
                                        };
                                        let kind = if n <= 1 {
                                            LegKind::IperfSingle(mk(0, alloc_port(next_port)))
                                        } else {
                                            let streams: Vec<IperfTask> = (0..n as usize)
                                                .map(|i| mk(i, alloc_port(next_port)))
                                                .collect();
                                            LegKind::IperfGroup {
                                                name: prof.name(),
                                                streams,
                                            }
                                        };
                                        legs.push(Leg {
                                            tag: tag.to_string(),
                                            kind,
                                        });
                                    }
                                    let stream_note = if max_n > 1 {
                                        format!(" ×{max_n}流")
                                    } else {
                                        String::new()
                                    };
                                    let title = format!(
                                        "{}IPERF {} {}{} | {}",
                                        if bidir { "★★双向 " } else { "" },
                                        ip_tag,
                                        prof.label(),
                                        stream_note,
                                        route_str
                                    );
                                    let id = md5_hex(&format!(
                                        "iperf_v1|{}|udp|{}|{}|{}|{}|{}|{}",
                                        ip_tag,
                                        prof.name(),
                                        spec.duration,
                                        spec.streams,
                                        ep_id(&spec.src),
                                        ep_id(&spec.dst),
                                        dir
                                    ));
                                    units.push(Unit {
                                        id,
                                        title,
                                        bidir,
                                        legs,
                                        est_secs: spec.duration + 10,
                                    });
                                }
                            }
                        }
                    }
                }

                // ---------- ping ----------
                if spec.kinds.iter().any(|k| k == "ping") {
                    for payload in &spec.payload_sizes {
                        let mut legs = Vec::new();
                        for (s, d, tag) in &pairs {
                            legs.push(Leg {
                                tag: tag.to_string(),
                                kind: LegKind::Ping(PingTask {
                                    v6,
                                    src: (*s).clone(),
                                    dst: (*d).clone(),
                                    count: spec.ping_count,
                                    payload: *payload,
                                }),
                            });
                        }
                        let title = format!(
                            "{}PING {} -l {} n={} | {}",
                            if bidir { "★双向 " } else { "" },
                            ip_tag,
                            payload,
                            spec.ping_count,
                            route_str
                        );
                        let id = md5_hex(&format!(
                            "ping_v1|{}|{}|{}|{}|{}|{}",
                            spec.ping_count,
                            payload,
                            ip_tag,
                            ep_id(&spec.src),
                            ep_id(&spec.dst),
                            dir
                        ));
                        units.push(Unit {
                            id,
                            title,
                            bidir,
                            legs,
                            est_secs: spec.ping_count as u64 + 5,
                        });
                    }
                }
            }
        }
    }
    (units, notices)
}

fn alloc_port(next: &mut u16) -> u16 {
    let p = *next;
    *next = next.wrapping_add(1).max(PORT_BASE);
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UdpProfile;

    fn nic(name: &str, role: &str, ip: &str, speed: u64) -> NicInfo {
        NicInfo {
            name: name.into(),
            role: role.into(),
            ipv4: ip.into(),
            ipv6_ll: "fe80::1".into(),
            zone: "12".into(),
            speed_mbps: speed,
            ..Default::default()
        }
    }

    fn ep(side: Side, name: &str, role: &str, ip: &str, speed: u64) -> Endpoint {
        Endpoint {
            side,
            pc: "PC".into(),
            nic: nic(name, role, ip, speed),
        }
    }

    fn base_spec() -> SpecNorm {
        SpecNorm {
            name: "t".into(),
            src: ep(Side::Master, "eth0", "SGMII2.5G", "192.168.1.2", 2500),
            dst: ep(Side::Agent, "eth0", "SGMII2.5G", "192.168.1.3", 2500),
            directions: vec!["ab".into()],
            kinds: vec!["iperf".into()],
            transports: vec!["tcp".into()],
            ipvers: vec!["v4".into()],
            streams: 1,
            duration: 10,
            ping_count: 4,
            payload_sizes: vec![32],
            tcp_windows: vec!["64k".into()],
            udp_profiles: vec![UdpProfile::bw("500m")],
            udp_limit: true,
        }
    }

    #[test]
    fn test_tcp_single() {
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[base_spec()], true, &mut port);
        assert_eq!(units.len(), 1);
        assert!(notices.is_empty());
        assert_eq!(units[0].legs.len(), 1);
        match &units[0].legs[0].kind {
            LegKind::IperfSingle(t) => {
                assert_eq!(t.port, PORT_BASE);
                assert_eq!(t.extra, vec!["-w", "64k", "-P", "1"]);
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn test_bidir_udp_group() {
        let mut spec = base_spec();
        spec.directions = vec!["bidir".into()];
        spec.transports = vec!["udp".into()];
        spec.streams = 3;
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        assert_eq!(units.len(), 1);
        assert!(units[0].bidir);
        assert_eq!(units[0].legs.len(), 2);
        // 2500/500 = 5 >= 3 允许 3 流
        for leg in &units[0].legs {
            match &leg.kind {
                LegKind::IperfGroup { streams, .. } => assert_eq!(streams.len(), 3),
                _ => panic!("expect group"),
            }
        }
        // 端口不重复
        assert_eq!(port, PORT_BASE + 6);
    }

    #[test]
    fn test_udp_limit() {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "eth1", "SGMII1G", "192.168.1.2", 1000);
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("2500m")];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert_eq!(units.len(), 0);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("跳过"));
    }

    #[test]
    fn test_udp_limit_wifi_unrestricted() {
        let mut spec = base_spec();
        let mut e = ep(Side::Master, "wlan", "WIFI5G", "192.168.1.5", 866);
        e.nic.is_wifi = true;
        spec.src = e;
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("2500m")];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert_eq!(units.len(), 1);
        assert!(notices.is_empty());
    }

    #[test]
    fn test_same24_gate() {
        let mut spec = base_spec();
        spec.dst = ep(Side::Agent, "eth0", "SGMII2.5G", "192.168.2.3", 2500);
        spec.kinds = vec!["iperf".into(), "ping".into()];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        // iperf 被拦，ping 保留
        assert_eq!(units.len(), 1);
        assert!(units[0].title.contains("PING"));
        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn test_ping_bidir_and_payloads() {
        let mut spec = base_spec();
        spec.kinds = vec!["ping".into()];
        spec.directions = vec!["ab".into(), "bidir".into()];
        spec.payload_sizes = vec![32, 1400];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        // 2 方向 × 2 payload
        assert_eq!(units.len(), 4);
        let bidirs: Vec<_> = units.iter().filter(|u| u.bidir).collect();
        assert_eq!(bidirs.len(), 2);
        assert_eq!(bidirs[0].legs.len(), 2);
    }

    #[test]
    fn test_v6_addrs_zone() {
        let a = nic("eth0", "SGMII1G", "192.168.1.2", 1000);
        let mut b = nic("eth0", "SGMII1G", "192.168.1.3", 1000);
        b.zone = "8".into();
        b.ipv6_ll = "fe80::2".into();
        let v = v6_addrs(&a, &b).unwrap();
        assert_eq!(v.client_bind, "fe80::1");
        assert_eq!(v.client_target, "fe80::2");
        assert_eq!(v.server_bind, "fe80::2");
    }

    #[test]
    fn test_v6_missing() {
        let mut a = nic("eth0", "SGMII1G", "192.168.1.2", 1000);
        a.ipv6_ll = String::new();
        let b = nic("eth0", "SGMII1G", "192.168.1.3", 1000);
        assert!(v6_addrs(&a, &b).is_none());
    }
}
