//! spec -> 任务单元(Unit) 生成 + 端口分配 + IP 自适应解析
//!
//! 配置写 "master:SGMII2.5G" 这类角色引用，运行时解析成实际网卡/IP。
//! 换电脑不用改配置：角色识别对了，IP 自动跟着变。

use crate::cmd::ctstraffic::parse_size_bytes;
use crate::config::{
    Config, CtsTrafficCfg, RateCheckCfg, RateMode, RateTargets, TestSpec, UdpProfile,
};
use crate::protocol::{HostInfo, NicInfo};
use crate::rate;
use crate::util::{md5_hex, same_slash24};
use std::collections::{BTreeMap, HashSet};

pub const PORT_BASE: u16 = 56000;
pub const DIAGNOSTIC_PING_COUNT: u32 = 3;
pub const DIAGNOSTIC_SUBNET_PAYLOAD: u32 = 32;
/// 单流 UDP 是基础连通性硬门槛：初次尝试加至少两次重试。
pub const SINGLE_UDP_MIN_ATTEMPTS: u64 = 3;
/// iperf3 每轮的 client 进程超时、回收、server 重建与轮间等待预算。
const IPERF_SINGLE_UDP_ATTEMPT_GRACE_SECS: u64 = 130;
/// ctsTraffic manager 每轮最多等待 duration+60 秒，再留少量停止/轮间预算。
const CTS_SINGLE_UDP_ATTEMPT_GRACE_SECS: u64 = 65;

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
    /// iperf / ctstraffic / ping
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
    pub rate_mode: RateMode,
    pub rate_targets: RateTargets,
    pub rate_check: RateCheckCfg,
    pub ctstraffic: CtsTrafficCfg,
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
    pub rate_mode: RateMode,
    pub rx_target_mbps: Option<f64>,
    pub offered_mbps: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct CtsTrafficTask {
    pub v6: bool,
    pub udp: bool,
    pub profile_name: String,
    pub profile_label: String,
    /// 数据方向始终是 src -> dst；UDP 的进程角色会在执行器中反转。
    pub src: Endpoint,
    pub dst: Endpoint,
    pub port: u16,
    pub duration: u64,
    pub streams: u32,
    pub window_bytes: Option<u32>,
    pub bits_per_second: Option<u64>,
    pub datagram_bytes: Option<u32>,
    pub frame_rate: u32,
    pub buffer_depth_secs: u32,
    pub status_update_ms: u32,
    pub rate_mode: RateMode,
    pub rx_target_mbps: Option<f64>,
    pub offered_mbps: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct PingTask {
    pub v6: bool,
    pub src: Endpoint,
    pub dst: Endpoint,
    pub count: u32,
    pub payload: u32,
    pub purpose: PingPurpose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PingPurpose {
    /// 配置/交互菜单明确选择的常规子网连通性测试。
    SubnetTest,
    /// 所有吞吐后端均无有效测量时自动追加的子网诊断。
    SubnetDiagnostic,
    /// 异常网卡绑定源地址到该接口 IPv4 网关的载体诊断。
    GatewayDiagnostic,
}

#[derive(Clone, Debug)]
pub enum LegKind {
    IperfSingle(IperfTask),
    IperfGroup {
        name: String,
        streams: Vec<IperfTask>,
    },
    CtsTraffic(CtsTrafficTask),
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

fn subnet_ping_key(src: &Endpoint, dst: &Endpoint, payload: u32) -> String {
    format!("{}|{}|{payload}", src.key(), dst.key())
}

/// 当本轮所有吞吐后端都没有产生有效测量时，按失败任务涉及的方向和网卡
/// 构造一组短时、去重的诊断任务：
///
/// - 每个唯一 IPv4 方向固定使用 32 字节短 Ping；
/// - 每块涉及网卡绑定自己的 IPv4 源地址 Ping 自己的 IPv4 网关；
/// - 已经在本轮选择中的同方向 32 字节常规 Ping 不重复执行；
/// - 网关为空也保留诊断单元，由执行器报告 GATEWAY_NOT_FOUND，而不是伪装成丢包。
pub fn build_traffic_failure_diagnostics(selected_units: &[Unit]) -> Vec<Unit> {
    let mut traffic_pairs: Vec<(Endpoint, Endpoint)> = Vec::new();
    let mut existing_subnet_pings = HashSet::new();
    for unit in selected_units {
        for leg in &unit.legs {
            match &leg.kind {
                LegKind::IperfSingle(task) => {
                    traffic_pairs.push((task.src.clone(), task.dst.clone()))
                }
                LegKind::IperfGroup { streams, .. } => {
                    traffic_pairs.extend(
                        streams
                            .iter()
                            .map(|task| (task.src.clone(), task.dst.clone())),
                    );
                }
                LegKind::CtsTraffic(task) => {
                    traffic_pairs.push((task.src.clone(), task.dst.clone()))
                }
                LegKind::Ping(task)
                    if !task.v6 && task.purpose != PingPurpose::GatewayDiagnostic =>
                {
                    existing_subnet_pings.insert(subnet_ping_key(
                        &task.src,
                        &task.dst,
                        task.payload,
                    ));
                }
                LegKind::Ping(_) => {}
            }
        }
    }
    if traffic_pairs.is_empty() {
        return Vec::new();
    }

    let mut directions: BTreeMap<String, (Endpoint, Endpoint)> = BTreeMap::new();
    let mut endpoints: BTreeMap<String, Endpoint> = BTreeMap::new();
    for (src, dst) in traffic_pairs {
        if !src.nic.ipv4.is_empty() && !dst.nic.ipv4.is_empty() {
            let direction_key = format!("{}|{}", src.key(), dst.key());
            directions
                .entry(direction_key)
                .or_insert_with(|| (src.clone(), dst.clone()));
        }
        for endpoint in [&src, &dst] {
            if !endpoint.nic.ipv4.is_empty() {
                endpoints
                    .entry(endpoint.key())
                    .or_insert_with(|| endpoint.clone());
            }
        }
    }

    let mut diagnostics = Vec::new();
    for (src, dst) in directions.into_values() {
        if existing_subnet_pings.contains(&subnet_ping_key(&src, &dst, DIAGNOSTIC_SUBNET_PAYLOAD)) {
            continue;
        }
        let title = format!(
            "[故障诊断] 子网 PING V4 -l {} n={} | {} -> {}",
            DIAGNOSTIC_SUBNET_PAYLOAD,
            DIAGNOSTIC_PING_COUNT,
            src.brief(),
            dst.brief()
        );
        let id = md5_hex(&format!(
            "iperf_failure_subnet_ping_v1|{}|{}|{}",
            src.key(),
            dst.key(),
            DIAGNOSTIC_SUBNET_PAYLOAD
        ));
        diagnostics.push(Unit {
            id,
            title,
            bidir: false,
            legs: vec![Leg {
                tag: "subnet-diagnostic".into(),
                kind: LegKind::Ping(PingTask {
                    v6: false,
                    src,
                    dst,
                    count: DIAGNOSTIC_PING_COUNT,
                    payload: DIAGNOSTIC_SUBNET_PAYLOAD,
                    purpose: PingPurpose::SubnetDiagnostic,
                }),
            }],
            est_secs: DIAGNOSTIC_PING_COUNT as u64 + 5,
        });
    }

    for endpoint in endpoints.into_values() {
        let gateway = endpoint.nic.gateway_v4.trim().to_string();
        let gateway_label = if gateway.is_empty() {
            "未发现 IPv4 网关".to_string()
        } else {
            gateway.clone()
        };
        let gateway_endpoint = Endpoint {
            side: endpoint.side,
            pc: endpoint.pc.clone(),
            nic: NicInfo {
                name: format!("{} 的 IPv4 网关", endpoint.nic.name),
                description: "IPv4 默认网关".into(),
                role: "GATEWAY".into(),
                ipv4: gateway.clone(),
                ..Default::default()
            },
        };
        let title = format!(
            "[故障诊断] 网卡/载体 PING 网关 V4 -l 32 n={} | {} -> {}",
            DIAGNOSTIC_PING_COUNT,
            endpoint.brief(),
            gateway_label
        );
        let id = md5_hex(&format!(
            "iperf_failure_gateway_ping_v1|{}|{}",
            endpoint.key(),
            gateway
        ));
        diagnostics.push(Unit {
            id,
            title,
            bidir: false,
            legs: vec![Leg {
                tag: "gateway-diagnostic".into(),
                kind: LegKind::Ping(PingTask {
                    v6: false,
                    src: endpoint,
                    dst: gateway_endpoint,
                    count: DIAGNOSTIC_PING_COUNT,
                    payload: 32,
                    purpose: PingPurpose::GatewayDiagnostic,
                }),
            }],
            est_secs: DIAGNOSTIC_PING_COUNT as u64 + 5,
        });
    }

    diagnostics
}

/// 兼容旧测试/调用名称；诊断范围现已覆盖 iperf3 与 ctsTraffic。
#[cfg(test)]
pub fn build_iperf_failure_diagnostics(selected_units: &[Unit]) -> Vec<Unit> {
    build_traffic_failure_diagnostics(selected_units)
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
    let nic = if let Some(name) = rest
        .strip_prefix("NAME=")
        .or_else(|| rest.strip_prefix("name="))
    {
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
        duration: t
            .iperf_duration
            .unwrap_or(cfg.iperf.duration)
            .clamp(1, 86400),
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
        rate_mode: t.rate_mode.unwrap_or(cfg.iperf.rate_check.mode),
        rate_targets: t.rate_targets_mbps.clone().unwrap_or_default(),
        rate_check: cfg.iperf.rate_check.clone(),
        ctstraffic: cfg.ctstraffic.clone(),
    })
}

/// UDP 按整条路径的可信负载上限裁剪流数。
/// RNDIS 3.7G 协商按约 2.5G，10GUSB 的 4.2G 已知显示 bug 不按 4.2G 裁剪。
fn allowed_udp_streams(
    sender: &Endpoint,
    receiver: &Endpoint,
    prof: &UdpProfile,
    want: u32,
    limit: bool,
    rate_cfg: &RateCheckCfg,
) -> u32 {
    if !limit {
        return want;
    }
    let Some(speed) = rate::path_payload_ceiling_mbps(&sender.nic, &receiver.nic, rate_cfg) else {
        return want;
    };
    let Some(bw) = prof.bandwidth_mbps() else {
        return want;
    };
    if bw <= 0.0 {
        return want;
    }
    let max_n = (speed / bw).floor() as u32;
    max_n.min(want)
}

fn udp_estimated_secs(
    duration: u64,
    total_streams: u64,
    has_single_stream_leg: bool,
    mode: RateMode,
    rate_cfg: &RateCheckCfg,
) -> u64 {
    let stagger_ms = total_streams
        .saturating_sub(1)
        .saturating_mul(rate_cfg.launch_interval_ms.clamp(0, 1_000));
    let discovery_ms = if mode == RateMode::Discover {
        3_u64
            .saturating_mul(rate_cfg.discovery_step_secs)
            .saturating_mul(1_000)
    } else {
        0
    };
    let base = duration
        .saturating_add(rate_cfg.background_secs.min(30))
        .saturating_add(rate_cfg.startup_timeout_secs)
        .saturating_add(rate_cfg.settle_secs)
        .saturating_add(5)
        .saturating_add(stagger_ms.saturating_add(discovery_ms).div_ceil(1_000));
    if !has_single_stream_leg {
        return base;
    }
    let attempts = single_udp_attempts(rate_cfg);
    let per_retry = duration
        .saturating_add(rate_cfg.startup_timeout_secs)
        .saturating_add(rate_cfg.settle_secs)
        .saturating_add(5)
        .saturating_add(IPERF_SINGLE_UDP_ATTEMPT_GRACE_SECS);
    base.saturating_add(per_retry.saturating_mul(attempts.saturating_sub(1)))
}

fn single_udp_attempts(rate_cfg: &RateCheckCfg) -> u64 {
    (rate_cfg.flow_retries as u64)
        .saturating_add(1)
        .max(SINGLE_UDP_MIN_ATTEMPTS)
}

fn ctstraffic_udp_estimated_secs(
    duration: u64,
    has_single_stream_leg: bool,
    rate_cfg: &RateCheckCfg,
) -> u64 {
    if !has_single_stream_leg {
        return duration.saturating_add(15);
    }
    duration
        .saturating_add(CTS_SINGLE_UDP_ATTEMPT_GRACE_SECS)
        .saturating_mul(single_udp_attempts(rate_cfg))
        .saturating_add(5)
}

fn dir_pairs<'a>(spec: &'a SpecNorm, dir: &str) -> Vec<(&'a Endpoint, &'a Endpoint, &'static str)> {
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

/// 向 resume 语义串写入一个长度编码字段。
///
/// 不能只用 `|` 拼接：主机名、接口名等外部字符串本身可能包含分隔符，进而让两组
/// 不同参数得到同一个待哈希字符串。字段名固定、值带字节长度后，编码可以无歧义解析。
fn push_resume_field(identity: &mut String, name: &str, value: &str) {
    identity.push('|');
    identity.push_str(name);
    identity.push('=');
    identity.push_str(&value.len().to_string());
    identity.push(':');
    identity.push_str(value);
}

fn rate_mode_identity(mode: RateMode) -> &'static str {
    match mode {
        RateMode::Auto => "auto",
        RateMode::Verify => "verify",
        RateMode::Observe => "observe",
        RateMode::Discover => "discover",
    }
}

/// 使用 IEEE-754 位模式记录浮点配置，避免显示精度或 locale 改变 resume ID。
fn f64_identity(value: f64) -> String {
    format!("{:016x}", value.to_bits())
}

fn option_f64_identity(value: Option<f64>) -> String {
    value
        .map(f64_identity)
        .unwrap_or_else(|| "none".to_string())
}

fn option_str_identity(value: Option<&str>) -> String {
    value
        .map(|text| format!("some:{}:{text}", text.len()))
        .unwrap_or_else(|| "none".to_string())
}

fn push_rate_targets_identity(identity: &mut String, prefix: &str, targets: &RateTargets) {
    push_resume_field(
        identity,
        &format!("{prefix}.forward"),
        &option_f64_identity(targets.forward),
    );
    push_resume_field(
        identity,
        &format!("{prefix}.ab"),
        &option_f64_identity(targets.ab),
    );
    push_resume_field(
        identity,
        &format!("{prefix}.ba"),
        &option_f64_identity(targets.ba),
    );
}

/// 记录所有会改变 UDP 执行或正式 verdict 的全局参数。
///
/// 这里有意记录原始配置而不是只记录最终目标：例如 `offered_headroom_pct` 同时改变
/// 最低发送负载和所需成功流数，`sample_interval_ms`/`settle_secs` 会改变可判定窗口，
/// `max_udp_loss_pct` 会直接改变 PASS/FAIL。新验收字段加入 RateCheckCfg 时也应同步加入。
fn push_rate_check_identity(identity: &mut String, cfg: &RateCheckCfg) {
    push_resume_field(identity, "rate_check.mode", rate_mode_identity(cfg.mode));
    push_rate_targets_identity(identity, "rate_check.targets", &cfg.targets_mbps);
    for (name, value) in [
        ("sample_interval_ms", cfg.sample_interval_ms),
        ("background_secs", cfg.background_secs),
        ("startup_timeout_secs", cfg.startup_timeout_secs),
        ("settle_secs", cfg.settle_secs),
        ("launch_interval_ms", cfg.launch_interval_ms),
        ("min_concurrent_streams", cfg.min_concurrent_streams as u64),
        ("flow_retries", cfg.flow_retries as u64),
        ("discovery_step_secs", cfg.discovery_step_secs),
    ] {
        push_resume_field(identity, &format!("rate_check.{name}"), &value.to_string());
    }
    for (name, value) in [
        ("min_active_ratio", cfg.min_active_ratio),
        ("offered_headroom_pct", cfg.offered_headroom_pct),
        ("evb_usb_to_eth_target_mbps", cfg.evb_usb_to_eth_target_mbps),
        ("evb_eth_to_usb_target_mbps", cfg.evb_eth_to_usb_target_mbps),
        ("cpe_path_ceiling_mbps", cfg.cpe_path_ceiling_mbps),
    ] {
        push_resume_field(
            identity,
            &format!("rate_check.{name}"),
            &f64_identity(value),
        );
    }
    push_resume_field(
        identity,
        "rate_check.max_udp_loss_pct",
        &option_f64_identity(cfg.max_udp_loss_pct),
    );
}

fn push_endpoint_identity(identity: &mut String, prefix: &str, endpoint: &Endpoint) {
    let side = match endpoint.side {
        Side::Master => "master",
        Side::Agent => "agent",
    };
    for (name, value) in [
        ("side", side),
        ("pc", endpoint.pc.as_str()),
        ("name", endpoint.nic.name.as_str()),
        ("role", endpoint.nic.role.as_str()),
        ("ipv4", endpoint.nic.ipv4.as_str()),
        ("ipv6_ll", endpoint.nic.ipv6_ll.as_str()),
        ("ipv6_global", endpoint.nic.ipv6_global.as_str()),
    ] {
        push_resume_field(identity, &format!("{prefix}.{name}"), value);
    }
    push_resume_field(
        identity,
        &format!("{prefix}.speed_mbps"),
        &endpoint.nic.speed_mbps.to_string(),
    );
}

fn push_iperf_task_identity(identity: &mut String, prefix: &str, task: &IperfTask) {
    push_resume_field(
        identity,
        &format!("{prefix}.v6"),
        if task.v6 { "true" } else { "false" },
    );
    push_resume_field(
        identity,
        &format!("{prefix}.udp"),
        if task.udp { "true" } else { "false" },
    );
    push_resume_field(identity, &format!("{prefix}.profile"), &task.profile_name);
    push_endpoint_identity(identity, &format!("{prefix}.src"), &task.src);
    push_endpoint_identity(identity, &format!("{prefix}.dst"), &task.dst);
    push_resume_field(
        identity,
        &format!("{prefix}.duration"),
        &task.duration.to_string(),
    );
    push_resume_field(
        identity,
        &format!("{prefix}.stream_idx"),
        &task.stream_idx.to_string(),
    );
    push_resume_field(
        identity,
        &format!("{prefix}.rate_mode"),
        rate_mode_identity(task.rate_mode),
    );
    push_resume_field(
        identity,
        &format!("{prefix}.rx_target_mbps"),
        &option_f64_identity(task.rx_target_mbps),
    );
    push_resume_field(
        identity,
        &format!("{prefix}.offered_mbps"),
        &option_f64_identity(task.offered_mbps),
    );
    push_resume_field(
        identity,
        &format!("{prefix}.extra_count"),
        &task.extra.len().to_string(),
    );
    for (idx, arg) in task.extra.iter().enumerate() {
        push_resume_field(identity, &format!("{prefix}.extra.{idx}"), arg);
    }
    // `port` 是构建顺序决定的临时资源，不属于测试/验收语义，不能写入 resume ID。
}

fn udp_resume_unit_id_with_schema(
    schema: &str,
    include_profile_window: bool,
    spec: &SpecNorm,
    ip_tag: &str,
    direction: &str,
    profile: &UdpProfile,
    legs: &[Leg],
) -> String {
    let mut identity = schema.to_string();
    push_resume_field(&mut identity, "transport", "udp");
    push_resume_field(&mut identity, "ip", ip_tag);
    push_resume_field(&mut identity, "direction", direction);
    push_resume_field(&mut identity, "duration", &spec.duration.to_string());
    push_resume_field(
        &mut identity,
        "requested_streams",
        &spec.streams.to_string(),
    );
    push_resume_field(
        &mut identity,
        "udp_limit",
        if spec.udp_limit { "true" } else { "false" },
    );
    push_resume_field(&mut identity, "profile.bandwidth", &profile.bandwidth);
    push_resume_field(
        &mut identity,
        "profile.length",
        &option_str_identity(profile.length.as_deref()),
    );
    if include_profile_window {
        push_resume_field(
            &mut identity,
            "profile.window",
            &option_str_identity(profile.window.as_deref()),
        );
    }
    push_resume_field(
        &mut identity,
        "configured_rate_mode",
        rate_mode_identity(spec.rate_mode),
    );
    push_rate_targets_identity(&mut identity, "scenario_targets", &spec.rate_targets);
    push_rate_check_identity(&mut identity, &spec.rate_check);
    push_endpoint_identity(&mut identity, "spec.src", &spec.src);
    push_endpoint_identity(&mut identity, "spec.dst", &spec.dst);
    push_resume_field(&mut identity, "leg_count", &legs.len().to_string());

    for (leg_idx, leg) in legs.iter().enumerate() {
        let prefix = format!("leg.{leg_idx}");
        push_resume_field(&mut identity, &format!("{prefix}.tag"), &leg.tag);
        match &leg.kind {
            LegKind::IperfSingle(task) => {
                push_resume_field(&mut identity, &format!("{prefix}.kind"), "single");
                push_resume_field(&mut identity, &format!("{prefix}.stream_count"), "1");
                push_iperf_task_identity(&mut identity, &format!("{prefix}.stream.0"), task);
            }
            LegKind::IperfGroup { streams, .. } => {
                push_resume_field(&mut identity, &format!("{prefix}.kind"), "group");
                push_resume_field(
                    &mut identity,
                    &format!("{prefix}.stream_count"),
                    &streams.len().to_string(),
                );
                for (stream_idx, task) in streams.iter().enumerate() {
                    push_iperf_task_identity(
                        &mut identity,
                        &format!("{prefix}.stream.{stream_idx}"),
                        task,
                    );
                }
            }
            LegKind::CtsTraffic(_) => {
                push_resume_field(&mut identity, &format!("{prefix}.kind"), "cts-invalid");
            }
            LegKind::Ping(_) => {
                // 本函数仅由 UDP 构建分支调用；保留类型标记可防未来误用时发生碰撞。
                push_resume_field(&mut identity, &format!("{prefix}.kind"), "ping-invalid");
            }
        }
    }

    md5_hex(&identity)
}

/// UDP resume ID schema v3：覆盖实际 offered load、裁剪后的流数、方向目标、模式、
/// socket buffer 和全部验收阈值。v1/v2 历史 PASS 因 schema 前缀变化不会再被错误复用。
fn udp_resume_unit_id_v3(
    spec: &SpecNorm,
    ip_tag: &str,
    direction: &str,
    profile: &UdpProfile,
    legs: &[Leg],
) -> String {
    udp_resume_unit_id_with_schema("iperf_v3", true, spec, ip_tag, direction, profile, legs)
}

fn cts_window_bytes(value: &str) -> Result<Option<u32>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("auto")
        || trimmed.eq_ignore_ascii_case("default")
    {
        Ok(None)
    } else {
        parse_size_bytes(trimmed).map(Some)
    }
}

fn cts_bits_per_second(profile: &UdpProfile) -> Result<u64, String> {
    let mbps = profile
        .bandwidth_mbps()
        .ok_or_else(|| format!("无法解析 UDP 带宽 {}", profile.bandwidth))?;
    let bps = mbps * 1_000_000.0;
    if !bps.is_finite() || bps < 1.0 || bps > u64::MAX as f64 {
        return Err(format!(
            "UDP 带宽超出 ctsTraffic 范围: {}",
            profile.bandwidth
        ));
    }
    Ok(bps.round() as u64)
}

fn cts_datagram_bytes(profile: &UdpProfile) -> Result<Option<u32>, String> {
    profile
        .length
        .as_deref()
        .map(parse_size_bytes)
        .transpose()
        .and_then(|value| {
            if value.is_some_and(|size| size > 65_507) {
                Err("ctsTraffic UDP datagram 必须不大于 65507 字节".into())
            } else {
                Ok(value)
            }
        })
}

fn cts_task_identity(identity: &mut String, prefix: &str, task: &CtsTrafficTask) {
    for (name, value) in [
        ("v6", if task.v6 { "true" } else { "false" }.to_string()),
        ("udp", if task.udp { "true" } else { "false" }.to_string()),
        ("profile", task.profile_name.clone()),
        ("duration", task.duration.to_string()),
        ("streams", task.streams.to_string()),
        (
            "window_bytes",
            task.window_bytes
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".into()),
        ),
        (
            "bits_per_second",
            task.bits_per_second
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".into()),
        ),
        (
            "datagram_bytes",
            task.datagram_bytes
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".into()),
        ),
        ("frame_rate", task.frame_rate.to_string()),
        ("buffer_depth_secs", task.buffer_depth_secs.to_string()),
        ("status_update_ms", task.status_update_ms.to_string()),
        ("rate_mode", rate_mode_identity(task.rate_mode).to_string()),
        ("rx_target_mbps", option_f64_identity(task.rx_target_mbps)),
        ("offered_mbps", option_f64_identity(task.offered_mbps)),
    ] {
        push_resume_field(identity, &format!("{prefix}.{name}"), &value);
    }
    push_endpoint_identity(identity, &format!("{prefix}.src"), &task.src);
    push_endpoint_identity(identity, &format!("{prefix}.dst"), &task.dst);
    // port 是临时资源，故意不进入 resume ID。
}

fn cts_resume_unit_id(spec: &SpecNorm, ip_tag: &str, direction: &str, legs: &[Leg]) -> String {
    let mut identity = "ctstraffic_v1".to_string();
    push_resume_field(&mut identity, "ip", ip_tag);
    push_resume_field(&mut identity, "direction", direction);
    push_resume_field(
        &mut identity,
        "configured_rate_mode",
        rate_mode_identity(spec.rate_mode),
    );
    push_rate_targets_identity(&mut identity, "scenario_targets", &spec.rate_targets);
    push_rate_check_identity(&mut identity, &spec.rate_check);
    push_resume_field(&mut identity, "leg_count", &legs.len().to_string());
    for (index, leg) in legs.iter().enumerate() {
        let prefix = format!("leg.{index}");
        push_resume_field(&mut identity, &format!("{prefix}.tag"), &leg.tag);
        match &leg.kind {
            LegKind::CtsTraffic(task) => cts_task_identity(&mut identity, &prefix, task),
            _ => push_resume_field(&mut identity, &format!("{prefix}.kind"), "invalid"),
        }
    }
    md5_hex(&identity)
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
            let route_str = format!("{} {} {}", pairs[0].0.brief(), arrow, pairs[0].1.brief());

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
                                    let plabel = format!("TCP -w {} -P {}", w, spec.streams);
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
                                            rate_mode: spec.rate_mode,
                                            rx_target_mbps: None,
                                            offered_mbps: None,
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
                                            _d,
                                            prof,
                                            spec.streams,
                                            spec.udp_limit,
                                            &spec.rate_check,
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
                                    for ((s, d, tag), n) in pairs.iter().zip(leg_streams.iter()) {
                                        let n = *n;
                                        max_n = max_n.max(n);
                                        let mut extra: Vec<String> =
                                            vec!["-b".into(), prof.bandwidth.clone()];
                                        if let Some(l) = &prof.length {
                                            extra.push("-l".into());
                                            extra.push(l.clone());
                                        }
                                        if let Some(w) = &prof.window {
                                            extra.push("-w".into());
                                            extra.push(w.clone());
                                        }
                                        let flow_direction =
                                            if bidir { tag.to_string() } else { dir.clone() };
                                        let target = rate::resolve_target_mbps(
                                            spec.rate_mode,
                                            &spec.rate_targets,
                                            &flow_direction,
                                            &s.nic,
                                            &d.nic,
                                            &spec.rate_check,
                                        );
                                        let effective_mode =
                                            rate::effective_mode(spec.rate_mode, target);
                                        let offered_mbps = prof.bandwidth_mbps();
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
                                            rate_mode: effective_mode,
                                            rx_target_mbps: target,
                                            offered_mbps,
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
                                    let id = udp_resume_unit_id_v3(spec, ip_tag, dir, prof, &legs);
                                    let total_streams =
                                        leg_streams.iter().map(|count| *count as u64).sum();
                                    let has_single_stream_leg = leg_streams.contains(&1);
                                    units.push(Unit {
                                        id,
                                        title,
                                        bidir,
                                        legs,
                                        est_secs: udp_estimated_secs(
                                            spec.duration,
                                            total_streams,
                                            has_single_stream_leg,
                                            spec.rate_mode,
                                            &spec.rate_check,
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }

                // ---------- Microsoft ctsTraffic（Windows 10+ 专用） ----------
                if spec
                    .kinds
                    .iter()
                    .any(|kind| kind == "ctstraffic" || kind == "cts")
                {
                    let cross = spec.src.side != spec.dst.side;
                    let same24_ok = !cross
                        || !require_same_subnet
                        || same_slash24(&spec.src.nic.ipv4, &spec.dst.nic.ipv4);
                    if !v6 && !same24_ok {
                        notices.push(format!(
                            "跳过 {} 的 ctsTraffic：两端 IPv4 不同 /24 ({} vs {})，无法直连灌包",
                            spec.name, spec.src.nic.ipv4, spec.dst.nic.ipv4
                        ));
                    } else {
                        for transport in &spec.transports {
                            if transport == "tcp" {
                                for window in &spec.tcp_windows {
                                    let window_bytes = match cts_window_bytes(window) {
                                        Ok(value) => value,
                                        Err(error) => {
                                            notices.push(format!(
                                                "跳过 {} CTS TCP window={window}: {error}",
                                                spec.name
                                            ));
                                            continue;
                                        }
                                    };
                                    let window_label = window_bytes
                                        .map(|bytes| format!("socket-buffer {window} ({bytes}B)"))
                                        .unwrap_or_else(|| "socket-buffer 自动".into());
                                    let profile_name = format!(
                                        "cts_tcp_w{}_c{}",
                                        if window.trim().is_empty() {
                                            "auto"
                                        } else {
                                            window
                                        },
                                        spec.streams
                                    );
                                    let profile_label =
                                        format!("CTS TCP {window_label} ×{}连接", spec.streams);
                                    let mut legs = Vec::new();
                                    for (src, dst, tag) in &pairs {
                                        legs.push(Leg {
                                            tag: tag.to_string(),
                                            kind: LegKind::CtsTraffic(CtsTrafficTask {
                                                v6,
                                                udp: false,
                                                profile_name: profile_name.clone(),
                                                profile_label: profile_label.clone(),
                                                src: (*src).clone(),
                                                dst: (*dst).clone(),
                                                port: alloc_port(next_port),
                                                duration: spec.duration,
                                                streams: spec.streams,
                                                window_bytes,
                                                bits_per_second: None,
                                                datagram_bytes: None,
                                                frame_rate: spec.ctstraffic.udp_frame_rate,
                                                buffer_depth_secs: spec
                                                    .ctstraffic
                                                    .udp_buffer_depth_secs,
                                                status_update_ms: spec.ctstraffic.status_update_ms,
                                                rate_mode: spec.rate_mode,
                                                rx_target_mbps: None,
                                                offered_mbps: None,
                                            }),
                                        });
                                    }
                                    let title = format!(
                                        "{}CTS TRAFFIC {} {} | {}",
                                        if bidir { "★★双向 " } else { "" },
                                        ip_tag,
                                        profile_label,
                                        route_str
                                    );
                                    units.push(Unit {
                                        id: cts_resume_unit_id(spec, ip_tag, dir, &legs),
                                        title,
                                        bidir,
                                        legs,
                                        est_secs: spec.duration.saturating_add(15),
                                    });
                                }
                            } else if transport == "udp" {
                                for profile in &spec.udp_profiles {
                                    let window_bytes = match profile
                                        .window
                                        .as_deref()
                                        .map(cts_window_bytes)
                                        .transpose()
                                    {
                                        Ok(value) => value.flatten(),
                                        Err(error) => {
                                            notices.push(format!(
                                                "跳过 {} CTS UDP {}: {error}",
                                                spec.name,
                                                profile.label()
                                            ));
                                            continue;
                                        }
                                    };
                                    let bits_per_second = match cts_bits_per_second(profile) {
                                        Ok(value) => value,
                                        Err(error) => {
                                            notices.push(format!(
                                                "跳过 {} CTS UDP: {error}",
                                                spec.name
                                            ));
                                            continue;
                                        }
                                    };
                                    let datagram_bytes = match cts_datagram_bytes(profile) {
                                        Ok(value) => value,
                                        Err(error) => {
                                            notices.push(format!(
                                                "跳过 {} CTS UDP: {error}",
                                                spec.name
                                            ));
                                            continue;
                                        }
                                    };
                                    let mut legs = Vec::new();
                                    let mut max_streams = 1u32;
                                    let mut has_single_stream_leg = false;
                                    for (src, dst, tag) in &pairs {
                                        let streams = allowed_udp_streams(
                                            src,
                                            dst,
                                            profile,
                                            spec.streams,
                                            spec.udp_limit,
                                            &spec.rate_check,
                                        );
                                        if streams == 0 {
                                            notices.push(format!(
                                                "跳过 {} CTS UDP {}：路径上限不足以承载单流",
                                                spec.name,
                                                profile.label()
                                            ));
                                            legs.clear();
                                            break;
                                        }
                                        max_streams = max_streams.max(streams);
                                        has_single_stream_leg |= streams == 1;
                                        let flow_direction =
                                            if bidir { tag.to_string() } else { dir.clone() };
                                        let target = rate::resolve_target_mbps(
                                            spec.rate_mode,
                                            &spec.rate_targets,
                                            &flow_direction,
                                            &src.nic,
                                            &dst.nic,
                                            &spec.rate_check,
                                        );
                                        let effective_mode =
                                            rate::effective_mode(spec.rate_mode, target);
                                        let offered_mbps = profile
                                            .bandwidth_mbps()
                                            .map(|value| value * streams as f64);
                                        let profile_label = format!(
                                            "CTS UDP {} ×{}流 (每流)",
                                            profile.label().trim_start_matches("UDP "),
                                            streams
                                        );
                                        legs.push(Leg {
                                            tag: tag.to_string(),
                                            kind: LegKind::CtsTraffic(CtsTrafficTask {
                                                v6,
                                                udp: true,
                                                profile_name: format!(
                                                    "cts_{}_c{}",
                                                    profile.name(),
                                                    streams
                                                ),
                                                profile_label,
                                                src: (*src).clone(),
                                                dst: (*dst).clone(),
                                                port: alloc_port(next_port),
                                                duration: spec.duration,
                                                streams,
                                                window_bytes,
                                                bits_per_second: Some(bits_per_second),
                                                datagram_bytes,
                                                frame_rate: spec.ctstraffic.udp_frame_rate,
                                                buffer_depth_secs: spec
                                                    .ctstraffic
                                                    .udp_buffer_depth_secs,
                                                status_update_ms: spec.ctstraffic.status_update_ms,
                                                rate_mode: effective_mode,
                                                rx_target_mbps: target,
                                                offered_mbps,
                                            }),
                                        });
                                    }
                                    if legs.is_empty() {
                                        continue;
                                    }
                                    let title = format!(
                                        "{}CTS TRAFFIC {} UDP {} ×{}流 | {}",
                                        if bidir { "★★双向 " } else { "" },
                                        ip_tag,
                                        profile.label().trim_start_matches("UDP "),
                                        max_streams,
                                        route_str
                                    );
                                    units.push(Unit {
                                        id: cts_resume_unit_id(spec, ip_tag, dir, &legs),
                                        title,
                                        bidir,
                                        legs,
                                        est_secs: ctstraffic_udp_estimated_secs(
                                            spec.duration,
                                            has_single_stream_leg,
                                            &spec.rate_check,
                                        ),
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
                                    purpose: PingPurpose::SubnetTest,
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
            rate_mode: RateMode::Auto,
            rate_targets: RateTargets::default(),
            rate_check: RateCheckCfg::default(),
            ctstraffic: CtsTrafficCfg::default(),
        }
    }

    fn cts_spec(transport: &str) -> SpecNorm {
        let mut spec = base_spec();
        spec.kinds = vec!["ctstraffic".into()];
        spec.transports = vec![transport.into()];
        spec.streams = 3;
        spec
    }

    fn build_single_cts_id(spec: SpecNorm, first_port: u16) -> String {
        let mut port = first_port;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        units[0].id.clone()
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
    fn ctstraffic_tcp_keeps_connections_in_one_task() {
        let spec = cts_spec("tcp");
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].legs.len(), 1);
        assert_eq!(port, PORT_BASE + 1, "CTS 的 3 条连接只占用一个进程端口");
        let LegKind::CtsTraffic(task) = &units[0].legs[0].kind else {
            panic!("expect ctsTraffic task");
        };
        assert!(!task.udp);
        assert_eq!(task.streams, 3);
        assert_eq!(task.window_bytes, Some(64 * 1024));
        assert_eq!(task.port, PORT_BASE);
        assert_eq!(task.src.side, Side::Master);
        assert_eq!(task.dst.side, Side::Agent);
        assert!(units[0].title.contains("×3连接"));
    }

    #[test]
    fn ctstraffic_udp_keeps_streams_in_one_task_and_preserves_data_direction() {
        let mut spec = cts_spec("udp");
        spec.udp_profiles = vec![UdpProfile {
            bandwidth: "500m".into(),
            length: Some("1200".into()),
            window: Some("4m".into()),
        }];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].legs.len(), 1);
        assert_eq!(port, PORT_BASE + 1, "CTS UDP 流不应展开成多个进程");
        let LegKind::CtsTraffic(task) = &units[0].legs[0].kind else {
            panic!("expect ctsTraffic task");
        };
        assert!(task.udp);
        assert_eq!(task.streams, 3);
        assert_eq!(task.bits_per_second, Some(500_000_000));
        assert_eq!(task.datagram_bytes, Some(1200));
        assert_eq!(task.window_bytes, Some(4 * 1024 * 1024));
        assert_eq!(task.src.side, Side::Master, "src 始终表示实际发送端");
        assert_eq!(task.dst.side, Side::Agent, "dst 始终表示实际接收端");
        assert_eq!(task.src.nic.ipv4, "192.168.1.2");
        assert_eq!(task.dst.nic.ipv4, "192.168.1.3");
    }

    #[test]
    fn ctstraffic_bidir_builds_two_legs_with_distinct_ports() {
        let mut spec = cts_spec("tcp");
        spec.directions = vec!["bidir".into()];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        assert!(units[0].bidir);
        assert_eq!(units[0].legs.len(), 2);
        assert_eq!(port, PORT_BASE + 2);

        let LegKind::CtsTraffic(ab) = &units[0].legs[0].kind else {
            panic!("expect ab ctsTraffic task");
        };
        let LegKind::CtsTraffic(ba) = &units[0].legs[1].kind else {
            panic!("expect ba ctsTraffic task");
        };
        assert_eq!(units[0].legs[0].tag, "ab");
        assert_eq!(units[0].legs[1].tag, "ba");
        assert_eq!((ab.port, ba.port), (PORT_BASE, PORT_BASE + 1));
        assert_eq!(ab.src.side, Side::Master);
        assert_eq!(ab.dst.side, Side::Agent);
        assert_eq!(ba.src.side, Side::Agent);
        assert_eq!(ba.dst.side, Side::Master);
        assert_eq!(ab.streams, 3);
        assert_eq!(ba.streams, 3);
    }

    #[test]
    fn ctstraffic_resume_id_ignores_port_and_tracks_udp_execution_semantics() {
        let mut base = cts_spec("udp");
        base.udp_profiles[0].window = Some("1m".into());
        let base_id = build_single_cts_id(base.clone(), PORT_BASE);
        assert_eq!(
            base_id,
            build_single_cts_id(base.clone(), PORT_BASE + 1000),
            "临时端口变化不应破坏 CTS resume"
        );

        let assert_id_changed = |name: &str, change: fn(&mut SpecNorm)| {
            let mut changed = base.clone();
            change(&mut changed);
            assert_ne!(
                base_id,
                build_single_cts_id(changed, PORT_BASE),
                "{name} 必须使旧 PASS 失效"
            );
        };
        assert_id_changed("socket buffer", |spec| {
            spec.udp_profiles[0].window = Some("2m".into())
        });
        assert_id_changed("frame rate", |spec| spec.ctstraffic.udp_frame_rate = 200);
        assert_id_changed("buffer depth", |spec| {
            spec.ctstraffic.udp_buffer_depth_secs = 2
        });
        assert_id_changed("status interval", |spec| {
            spec.ctstraffic.status_update_ms = 500
        });
    }

    #[test]
    fn ctstraffic_and_iperf_resume_ids_do_not_collide() {
        let mut spec = base_spec();
        spec.kinds = vec!["iperf".into(), "ctstraffic".into()];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert!(notices.is_empty());
        assert_eq!(units.len(), 2);
        let iperf_id = units
            .iter()
            .find(|unit| {
                unit.legs.iter().any(|leg| {
                    matches!(
                        &leg.kind,
                        LegKind::IperfSingle(_) | LegKind::IperfGroup { .. }
                    )
                })
            })
            .map(|unit| unit.id.as_str())
            .expect("iperf unit");
        let cts_id = units
            .iter()
            .find(|unit| {
                unit.legs
                    .iter()
                    .any(|leg| matches!(&leg.kind, LegKind::CtsTraffic(_)))
            })
            .map(|unit| unit.id.as_str())
            .expect("ctsTraffic unit");
        assert_ne!(iperf_id, cts_id);
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
        assert_eq!(units[0].est_secs, 39);
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
    fn test_udp_window_is_forwarded_to_iperf_and_report_identity() {
        let mut spec = base_spec();
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile {
            bandwidth: "1000m".into(),
            length: Some("64".into()),
            window: Some("4m".into()),
        }];

        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        assert!(units[0].title.contains("UDP -b 1000m -l 64 -w 4m"));

        let LegKind::IperfSingle(task) = &units[0].legs[0].kind else {
            panic!("expect single UDP task");
        };
        assert_eq!(task.extra, vec!["-b", "1000m", "-l", "64", "-w", "4m"]);
        assert_eq!(task.profile_name, "udp_b1000m_l64_w4m");
        assert_eq!(task.profile_label, "UDP -b 1000m -l 64 -w 4m");
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
    fn test_udp_limit_wifi_uses_path_ceiling() {
        let mut spec = base_spec();
        let mut e = ep(Side::Master, "wlan", "WIFI5G", "192.168.1.5", 866);
        e.nic.is_wifi = true;
        spec.src = e;
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("2500m")];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(units.is_empty());
        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn test_rndis_3700_is_capped_to_2500_payload() {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "usb", "RNDIS", "192.168.1.2", 3700);
        spec.dst = ep(Side::Agent, "10g", "10GETH", "192.168.1.3", 10000);
        spec.transports = vec!["udp".into()];
        spec.streams = 20;
        spec.udp_profiles = vec![UdpProfile::bw("500m")];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());
        match &units[0].legs[0].kind {
            LegKind::IperfGroup { streams, .. } => assert_eq!(streams.len(), 5),
            _ => panic!("expect group"),
        }
    }

    #[test]
    fn single_udp_estimate_covers_three_attempts_per_direction_without_double_counting_bidir() {
        let mut oneway = base_spec();
        oneway.transports = vec!["udp".into()];
        oneway.streams = 1;
        let mut port = PORT_BASE;
        let (oneway_units, notices) = build_units(&[oneway.clone()], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(oneway_units.len(), 1);
        let oneway_estimate = oneway_units[0].est_secs;
        assert_eq!(oneway_estimate, 368);

        oneway.directions = vec!["bidir".into()];
        let mut port = PORT_BASE;
        let (bidir_units, notices) = build_units(&[oneway], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(bidir_units.len(), 1);
        assert_eq!(bidir_units[0].legs.len(), 2);
        assert_eq!(
            bidir_units[0].est_secs,
            oneway_estimate + 1,
            "AB/BA 并行只增加一次毫秒级错峰取整，不应按六轮墙钟时间重复累计"
        );
    }

    #[test]
    fn single_udp_estimate_honors_retry_budget_above_minimum() {
        let mut spec = base_spec();
        spec.transports = vec!["udp".into()];
        spec.streams = 1;
        spec.rate_check.flow_retries = 4;
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].est_secs, 698);
    }

    #[test]
    fn ctstraffic_single_udp_estimate_covers_three_attempts_and_bidir_is_parallel() {
        let mut spec = cts_spec("udp");
        spec.streams = 1;
        let mut port = PORT_BASE;
        let (oneway_units, notices) = build_units(&[spec.clone()], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(oneway_units.len(), 1);
        assert_eq!(oneway_units[0].est_secs, 230);

        spec.directions = vec!["bidir".into()];
        let mut port = PORT_BASE;
        let (bidir_units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(bidir_units.len(), 1);
        assert_eq!(bidir_units[0].legs.len(), 2);
        assert_eq!(bidir_units[0].est_secs, oneway_units[0].est_secs);
    }

    #[test]
    fn test_evb_auto_direction_targets() {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "usb", "10GUSB", "192.168.1.2", 4200);
        spec.dst = ep(Side::Agent, "10g", "10GETH", "192.168.1.3", 10000);
        spec.directions = vec!["bidir".into()];
        spec.transports = vec!["udp".into()];
        spec.streams = 20;
        spec.udp_profiles = vec![UdpProfile::bw("500m")];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        for leg in &units[0].legs {
            let first = match &leg.kind {
                LegKind::IperfGroup { streams, .. } => &streams[0],
                _ => panic!("expect group"),
            };
            if leg.tag == "ab" {
                assert_eq!(first.rx_target_mbps, Some(6400.0));
            } else {
                assert_eq!(first.rx_target_mbps, Some(8400.0));
            }
            assert_eq!(first.rate_mode, RateMode::Verify);
        }
    }

    fn build_single_udp_id(spec: SpecNorm, first_port: u16) -> String {
        let mut port = first_port;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        units[0].id.clone()
    }

    fn evb_udp_spec() -> SpecNorm {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "usb", "10GUSB", "192.168.1.2", 4200);
        spec.dst = ep(Side::Agent, "10g", "10GETH", "192.168.1.3", 10000);
        spec.transports = vec!["udp".into()];
        spec.streams = 20;
        spec.udp_profiles = vec![UdpProfile::bw("500m")];
        spec
    }

    #[test]
    fn test_udp_resume_v3_ignores_runtime_port_but_tracks_verdict_semantics() {
        let base = evb_udp_spec();
        let base_id = build_single_udp_id(base.clone(), PORT_BASE);
        let mut legacy_port = PORT_BASE;
        let (legacy_units, legacy_notices) =
            build_units(std::slice::from_ref(&base), true, &mut legacy_port);
        assert!(legacy_notices.is_empty());
        let legacy_v2_id = udp_resume_unit_id_with_schema(
            "iperf_v2",
            false,
            &base,
            "V4",
            "ab",
            &base.udp_profiles[0],
            &legacy_units[0].legs,
        );
        assert_ne!(
            base_id, legacy_v2_id,
            "v3 必须让 v2 schema 下缓存的 PASS 无条件失效"
        );
        let legacy_v1_id = md5_hex(&format!(
            "iperf_v1|V4|udp|{}|{}|{}|{}|{}|ab",
            base.udp_profiles[0].name(),
            base.duration,
            base.streams,
            ep_id(&base.src),
            ep_id(&base.dst),
        ));
        assert_ne!(
            base_id, legacy_v1_id,
            "v3 必须让 v1 schema 下缓存的 PASS 无条件失效"
        );
        assert_eq!(
            base_id,
            build_single_udp_id(base.clone(), PORT_BASE + 1000),
            "临时端口变化不应让相同测试失去 resume 能力"
        );

        let assert_id_changed = |name: &str, change: fn(&mut SpecNorm)| {
            let mut changed = base.clone();
            change(&mut changed);
            assert_ne!(
                base_id,
                build_single_udp_id(changed, PORT_BASE),
                "{name} 必须使旧 PASS 失效"
            );
        };

        // 即使 Auto 和 Verify 最终都解析为 Verify，也不能复用不同配置模式下的 PASS。
        assert_id_changed("rate_mode", |spec| spec.rate_mode = RateMode::Verify);
        assert_id_changed("scenario target", |spec| {
            spec.rate_targets.ab = Some(6200.0)
        });
        assert_id_changed("global target", |spec| {
            spec.rate_check.targets_mbps.ab = Some(6200.0)
        });
        assert_id_changed("offered load", |spec| {
            spec.udp_profiles = vec![UdpProfile::bw("400m")]
        });
        assert_id_changed("UDP socket buffer", |spec| {
            spec.udp_profiles[0].window = Some("4m".into())
        });
        assert_id_changed("sample interval", |spec| {
            spec.rate_check.sample_interval_ms = 500
        });
        assert_id_changed("background window", |spec| {
            spec.rate_check.background_secs = 5
        });
        assert_id_changed("startup timeout", |spec| {
            spec.rate_check.startup_timeout_secs = 20
        });
        assert_id_changed("settle window", |spec| spec.rate_check.settle_secs = 8);
        assert_id_changed("launch interval", |spec| {
            spec.rate_check.launch_interval_ms = 100
        });
        assert_id_changed("minimum streams", |spec| {
            spec.rate_check.min_concurrent_streams = 3
        });
        assert_id_changed("active ratio", |spec| {
            spec.rate_check.min_active_ratio = 0.8
        });
        assert_id_changed("offered headroom", |spec| {
            spec.rate_check.offered_headroom_pct = 10.0
        });
        assert_id_changed("flow retries", |spec| spec.rate_check.flow_retries = 2);
        assert_id_changed("discovery step", |spec| {
            spec.rate_check.discovery_step_secs = 15
        });
        assert_id_changed("EVB target", |spec| {
            spec.rate_check.evb_usb_to_eth_target_mbps = 6300.0
        });
        assert_id_changed("path ceiling", |spec| {
            spec.rate_check.cpe_path_ceiling_mbps = 2200.0
        });
        assert_id_changed("loss threshold", |spec| {
            spec.rate_check.max_udp_loss_pct = Some(0.1)
        });
    }

    #[test]
    fn test_udp_resume_v3_tracks_effective_leg_shape() {
        let mut base = evb_udp_spec();
        base.src = ep(Side::Master, "rndis", "RNDIS", "192.168.1.2", 3700);
        base.rate_mode = RateMode::Observe;
        let five_stream_id = build_single_udp_id(base.clone(), PORT_BASE);

        base.rate_check.cpe_path_ceiling_mbps = 2000.0;
        let four_stream_id = build_single_udp_id(base, PORT_BASE);
        assert_ne!(five_stream_id, four_stream_id);
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
        spec.payload_sizes = vec![32, 1600, 65500];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        // 2 方向 × 3 payload
        assert_eq!(units.len(), 6);
        let bidirs: Vec<_> = units.iter().filter(|u| u.bidir).collect();
        assert_eq!(bidirs.len(), 3);
        assert_eq!(bidirs[0].legs.len(), 2);
        let payloads: Vec<u32> = units
            .iter()
            .filter_map(|unit| match &unit.legs[0].kind {
                LegKind::Ping(task) => Some(task.payload),
                _ => None,
            })
            .collect();
        assert_eq!(payloads, vec![32, 1600, 65500, 32, 1600, 65500]);
    }

    #[test]
    fn iperf_failure_diagnostics_use_32_bytes_and_both_gateways() {
        let mut spec = base_spec();
        spec.src.nic.gateway_v4 = "192.168.1.1".into();
        spec.dst.nic.gateway_v4 = "192.168.1.254".into();
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let diagnostics = build_iperf_failure_diagnostics(&units);

        assert_eq!(diagnostics.len(), 3, "1 个子网 Ping + 两端网关");
        let mut subnet_payloads = Vec::new();
        let mut gateways = Vec::new();
        for unit in &diagnostics {
            let LegKind::Ping(task) = &unit.legs[0].kind else {
                panic!("诊断单元必须是 Ping");
            };
            assert_eq!(task.count, DIAGNOSTIC_PING_COUNT);
            match task.purpose {
                PingPurpose::SubnetDiagnostic => {
                    subnet_payloads.push(task.payload);
                    assert_eq!(task.src.nic.ipv4, "192.168.1.2");
                    assert_eq!(task.dst.nic.ipv4, "192.168.1.3");
                }
                PingPurpose::GatewayDiagnostic => {
                    assert_eq!(task.payload, 32);
                    assert_eq!(task.src.side, task.dst.side);
                    gateways.push((task.src.nic.ipv4.clone(), task.dst.nic.ipv4.clone()));
                }
                PingPurpose::SubnetTest => panic!("自动诊断不应标记为常规 Ping"),
            }
        }
        assert_eq!(subnet_payloads, vec![DIAGNOSTIC_SUBNET_PAYLOAD]);
        assert!(gateways.contains(&("192.168.1.2".into(), "192.168.1.1".into())));
        assert!(gateways.contains(&("192.168.1.3".into(), "192.168.1.254".into())));
    }

    #[test]
    fn ctstraffic_failure_diagnostics_collects_data_endpoints_and_gateways() {
        let mut spec = cts_spec("udp");
        spec.src.nic.gateway_v4 = "192.168.1.1".into();
        spec.dst.nic.gateway_v4 = "192.168.1.254".into();
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());

        let diagnostics = build_traffic_failure_diagnostics(&units);
        assert_eq!(diagnostics.len(), 3, "CTS 失败也要诊断数据路径与两端网关");
        let subnet = diagnostics
            .iter()
            .find_map(|unit| match &unit.legs[0].kind {
                LegKind::Ping(task) if task.purpose == PingPurpose::SubnetDiagnostic => Some(task),
                _ => None,
            })
            .expect("CTS src->dst subnet diagnostic");
        assert_eq!(subnet.src.nic.ipv4, "192.168.1.2");
        assert_eq!(subnet.dst.nic.ipv4, "192.168.1.3");

        let gateway_targets: Vec<&str> = diagnostics
            .iter()
            .filter_map(|unit| match &unit.legs[0].kind {
                LegKind::Ping(task) if task.purpose == PingPurpose::GatewayDiagnostic => {
                    Some(task.dst.nic.ipv4.as_str())
                }
                _ => None,
            })
            .collect();
        assert!(gateway_targets.contains(&"192.168.1.1"));
        assert!(gateway_targets.contains(&"192.168.1.254"));
    }

    #[test]
    fn iperf_failure_diagnostics_keep_missing_gateway_for_not_evaluated_report() {
        let mut spec = base_spec();
        spec.src.nic.gateway_v4.clear();
        spec.dst.nic.gateway_v4.clear();
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let diagnostics = build_iperf_failure_diagnostics(&units);

        let gateway_tasks: Vec<&PingTask> = diagnostics
            .iter()
            .filter_map(|unit| match &unit.legs[0].kind {
                LegKind::Ping(task) if task.purpose == PingPurpose::GatewayDiagnostic => Some(task),
                _ => None,
            })
            .collect();
        assert_eq!(gateway_tasks.len(), 2);
        assert!(gateway_tasks
            .iter()
            .all(|task| task.dst.nic.ipv4.is_empty()));
    }

    #[test]
    fn existing_subnet_ping_is_not_duplicated_by_failure_diagnostics() {
        let mut spec = base_spec();
        spec.kinds = vec!["iperf".into(), "ping".into()];
        spec.payload_sizes = vec![32, 1600, 65500];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let diagnostics = build_iperf_failure_diagnostics(&units);

        assert_eq!(
            diagnostics
                .iter()
                .filter(|unit| matches!(
                    &unit.legs[0].kind,
                    LegKind::Ping(PingTask {
                        purpose: PingPurpose::SubnetDiagnostic,
                        ..
                    })
                ))
                .count(),
            0
        );
        assert_eq!(diagnostics.len(), 2, "仍需检查两端网卡网关");
    }

    #[test]
    fn non_32_regular_ping_does_not_suppress_32_byte_failure_diagnostic() {
        let mut spec = base_spec();
        spec.kinds = vec!["iperf".into(), "ping".into()];
        spec.payload_sizes = vec![1600, 65500];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let diagnostics = build_iperf_failure_diagnostics(&units);

        let subnet_payloads: Vec<u32> = diagnostics
            .iter()
            .filter_map(|unit| match &unit.legs[0].kind {
                LegKind::Ping(PingTask {
                    payload,
                    purpose: PingPurpose::SubnetDiagnostic,
                    ..
                }) => Some(*payload),
                _ => None,
            })
            .collect();
        assert_eq!(subnet_payloads, vec![DIAGNOSTIC_SUBNET_PAYLOAD]);
        assert_eq!(diagnostics.len(), 3, "32 字节子网 Ping + 两端网关");
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
