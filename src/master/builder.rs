//! spec -> 任务单元(Unit) 生成 + 端口分配 + IP 自适应解析
//!
//! 配置写 "master:SGMII2.5G" 这类角色引用，运行时解析成实际网卡/IP。
//! 换电脑不用改配置：角色识别对了，IP 自动跟着变。

use crate::cmd::ctstraffic::parse_size_bytes;
use crate::config::{
    Config, CtsTrafficCfg, LinkProfiles, ParsedBandwidth, RateCheckCfg, RateMode, RateTargets,
    TestSpec, UdpProfile,
};
use crate::nic::same_slash24;
use crate::protocol::{HostInfo, NicInfo};
use crate::rate;
use crate::util::md5_hex;
use std::collections::{BTreeMap, HashSet};

mod diagnostics;
mod identity;
mod policy;

#[cfg(test)]
pub use diagnostics::build_iperf_failure_diagnostics;
pub use diagnostics::build_traffic_failure_diagnostics;
use identity::*;
use policy::*;

pub const PORT_BASE: u16 = 56000;
pub const DIAGNOSTIC_PING_COUNT: u32 = 3;
pub const DIAGNOSTIC_SUBNET_PAYLOAD: u32 = 32;

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
    /// 报表分组键，来自 `TestSpec.link_group`（界面填的链路集合名）。
    /// 空表示没有分组信息，报表回落到物理网口对。
    pub link_group: String,
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
    pub tcp_streams: u32,
    pub udp_streams: u32,
    pub duration: u64,
    pub ping_count: u32,
    pub payload_sizes: Vec<u32>,
    pub tcp_windows: Vec<String>,
    pub udp_profiles: Vec<UdpProfile>,
    pub udp_limit: bool,
    pub rate_mode: RateMode,
    pub rate_targets: RateTargets,
    /// 双向并发单元专用的门限，按方向（ab/ba）。空则双向也走既有兜底链。
    pub rate_targets_bidir: RateTargets,
    /// 双向并发单元的「两端 RX 合计」门限。
    ///
    /// 配了它，这个双向单元就只按合计判定：两条腿各自只测量，单元级比一次
    /// 合计（见 [`crate::config::TestSpec::rate_target_bidir_total_mbps`]）。
    pub rate_target_bidir_total: Option<f64>,
    pub rate_check: RateCheckCfg,
    /// 两层链路策略（角色兜底 + 单口覆盖）；空则全部走内置推导。
    pub link_profiles: LinkProfiles,
    pub ctstraffic: CtsTrafficCfg,
    /// 配置层中 TCP/UDP 共用的非法 CTS 标量参数。协议流数错误由各自
    /// 的任务分支根据原始值生成，避免一方错误污染另一方。
    pub ctstraffic_config_error: Option<String>,
}

impl SpecNorm {
    fn stream_override(&self, udp: bool) -> u32 {
        if udp {
            self.udp_streams
        } else {
            self.tcp_streams
        }
    }

    fn requested_streams(&self, udp: bool) -> u32 {
        let protocol_streams = self.stream_override(udp);
        if protocol_streams > 0 {
            protocol_streams
        } else {
            self.streams
        }
    }

    fn effective_streams(&self, udp: bool) -> u32 {
        self.requested_streams(udp).clamp(1, 32)
    }

    pub fn effective_tcp_streams(&self) -> u32 {
        self.effective_streams(false)
    }

    pub fn effective_udp_streams(&self) -> u32 {
        self.effective_streams(true)
    }

    fn stream_config_error(&self, udp: bool) -> Option<String> {
        let override_value = self.stream_override(udp);
        let streams = self.requested_streams(udp);
        (!(1..=32).contains(&streams)).then(|| {
            let protocol = if udp { "UDP" } else { "TCP" };
            let source = if override_value > 0 {
                if udp {
                    "udp_streams"
                } else {
                    "tcp_streams"
                }
            } else {
                "streams"
            };
            format!("{protocol} streams 必须在 1..=32，当前为 {streams}（来源 {source}）")
        })
    }
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
    /// **每条流**下发的目标负载（`-b`）。
    ///
    /// 与 `CtsTrafficTask::offered_total_mbps` 语义**相反**：那边是整条腿的总量。
    /// 两个字段以前都叫 `offered_mbps`，把 4 条流 × 500Mbps 当成 500Mbps 总量
    /// （或反过来）编译器一句话都不会说。名字带上口径，让类型拦住误用。
    pub offered_per_stream_mbps: Option<f64>,
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
    /// 整条腿下发的目标负载**总量**。
    ///
    /// 与 `IperfTask::offered_per_stream_mbps` 语义**相反**：那边是每条流。
    pub offered_total_mbps: Option<f64>,
    /// builder 已识别的非法 CTS 配置；执行器不得启动进程，必须直接报告
    /// SETUP_ERROR / CTSTRAFFIC_ARGS_INVALID。
    pub setup_error: Option<String>,
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
    /// 报表分组键（`SpecNorm.link_group`）。**不进 resume identity、不进判定**，
    /// 纯粹是「这一批单元在报表里归到哪一组」。
    pub link_group: String,
    pub bidir: bool,
    /// 计划页显示的「每条腿最终按什么门限判、门限来自哪一层」。
    ///
    /// **只用于展示**，判定和 resume 都不读它。
    pub target_lines: Vec<String>,
    /// 双向单元的「两端 RX 合计」门限；`None` = 按每方向门限判定。
    ///
    /// 判定入口在 `executor::bidir_total_verdict`：两条腿都形成有效 RX 平均后，
    /// **只比一次** `AB.rx_avg + BA.rx_avg >= 门限`。
    pub bidir_total_target_mbps: Option<f64>,
    /// 规范方向：`ab` / `ba` / `bidir`；诊断类单元为空。
    ///
    /// **只用于展示**，判定和 resume 都不读它。存在的理由是单向单元的
    /// `Leg.tag` 是空串（见 `dir_pairs`）——那个空串在执行侧有语义（「单向」），
    /// 不能为了显示去动它；于是预览里单向单元的参数行就没有方向，而双向单元
    /// 有，同一份清单两种样子。方向本身是用户在套件里勾的，理应逐行看得见。
    pub direction: String,
    pub legs: Vec<Leg>,
    pub est_secs: u64,
}

/// 一块网卡在重扫后相对于「计划时快照」的变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NicDrift {
    /// 计划时存在的网卡，重扫后按接口名找不到了。
    Gone { pc: String, name: String },
    /// 还在，但关键字段变了（IPv4 / 接口索引 / 协商速率 / link-local）。
    Changed {
        pc: String,
        name: String,
        detail: String,
    },
}

impl NicDrift {
    pub fn is_gone(&self) -> bool {
        matches!(self, NicDrift::Gone { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            NicDrift::Gone { pc, name } => format!("{pc} / {name} 已消失"),
            NicDrift::Changed { pc, name, detail } => format!("{pc} / {name} {detail}"),
        }
    }
}

/// 遍历单元里所有端点。任务类型增加时这里必须跟着加，否则新类型的端点
/// 会静默漏掉刷新。
fn for_each_endpoint_mut(unit: &mut Unit, mut f: impl FnMut(&mut Endpoint)) {
    for leg in &mut unit.legs {
        match &mut leg.kind {
            LegKind::IperfSingle(task) => {
                f(&mut task.src);
                f(&mut task.dst);
            }
            LegKind::IperfGroup { streams, .. } => {
                for task in streams {
                    f(&mut task.src);
                    f(&mut task.dst);
                }
            }
            LegKind::CtsTraffic(task) => {
                f(&mut task.src);
                f(&mut task.dst);
            }
            LegKind::Ping(task) => {
                f(&mut task.src);
                f(&mut task.dst);
            }
        }
    }
}

/// 用最新一次双端扫描的结果刷新单元里所有端点的网卡信息，并报告发生了什么变化。
///
/// 计划阶段的网卡快照在运行开始时取一次，之后就一路按值拷进每个 `Unit`。
/// 一轮 120 个单元要跑近 7 小时，这段时间里 WiFi 会重新协商、USB 网卡会重新
/// 枚举、DHCP 会换租约——用开跑那一刻的 `2882Mbps` 去推导后面几十个单元的
/// `-b` 与门限，基准从中途就是错的，而报告里印的也是那份旧快照，
/// 错误完全不可见（见 .ai/DESIGN-v4.3.0.md F1）。
///
/// 按**接口名**匹配：这是 monitor 采样时用的同一个标识（`MonitorStartReq.iface`），
/// 用别的键匹配会出现「刷新了地址却采着另一块网卡」的错位。
pub fn refresh_unit_endpoints(
    unit: &mut Unit,
    master: &HostInfo,
    agent: &HostInfo,
) -> Vec<NicDrift> {
    let mut drifts: Vec<NicDrift> = Vec::new();
    for_each_endpoint_mut(unit, |ep| {
        let host = match ep.side {
            Side::Master => master,
            Side::Agent => agent,
        };
        let Some(fresh) = host.interfaces.iter().find(|nic| nic.name == ep.nic.name) else {
            let drift = NicDrift::Gone {
                pc: ep.pc.clone(),
                name: ep.nic.name.clone(),
            };
            if !drifts.contains(&drift) {
                drifts.push(drift);
            }
            return;
        };
        let mut changes: Vec<String> = Vec::new();
        if fresh.ipv4 != ep.nic.ipv4 {
            changes.push(format!("IPv4 {} → {}", ep.nic.ipv4, fresh.ipv4));
        }
        if fresh.ipv6_ll != ep.nic.ipv6_ll {
            changes.push(format!("link-local {} → {}", ep.nic.ipv6_ll, fresh.ipv6_ll));
        }
        if fresh.ifindex != ep.nic.ifindex {
            changes.push(format!("接口索引 {} → {}", ep.nic.ifindex, fresh.ifindex));
        }
        if fresh.speed_mbps != ep.nic.speed_mbps {
            changes.push(format!(
                "协商速率 {} → {}Mbps",
                ep.nic.speed_mbps, fresh.speed_mbps
            ));
        }
        if !changes.is_empty() {
            let drift = NicDrift::Changed {
                pc: ep.pc.clone(),
                name: ep.nic.name.clone(),
                detail: changes.join("，"),
            };
            if !drifts.contains(&drift) {
                drifts.push(drift);
            }
        }
        ep.nic = fresh.clone();
    });
    drifts
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

/// 返回 TCP/UDP 共用的 CTS 配置错误。协议流数由任务分支按原始值分别校验。
pub(crate) fn ctstraffic_common_config_error(duration: u64) -> Option<String> {
    let mut errors = Vec::new();
    if !(1..=86_400).contains(&duration) {
        errors.push(format!(
            "ctsTraffic 自动化 duration 必须在 1..=86400 秒，当前为 {duration}；无限测试请使用原生命令并手动停止"
        ));
    }
    (!errors.is_empty()).then(|| errors.join("；"))
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
    let configured_streams = t.streams;
    let configured_duration = t.iperf_duration.unwrap_or(cfg.iperf.duration);
    Ok(SpecNorm {
        name: if t.name.is_empty() {
            format!("{}->{}", t.src, t.dst)
        } else {
            t.name.clone()
        },
        link_group: t.link_group.clone().unwrap_or_default(),
        src,
        dst,
        directions: t.direction.directions(),
        kinds: t.kinds.iter().map(|k| k.to_lowercase()).collect(),
        transports: t.transports.iter().map(|k| k.to_lowercase()).collect(),
        ipvers: t.ip.iter().map(|k| k.to_lowercase()).collect(),
        streams: configured_streams,
        tcp_streams: t.tcp_streams.unwrap_or(0),
        udp_streams: t.udp_streams.unwrap_or(0),
        duration: configured_duration.clamp(1, 86400),
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
        rate_targets_bidir: t.rate_targets_bidir_mbps.clone().unwrap_or_default(),
        rate_target_bidir_total: t
            .rate_target_bidir_total_mbps
            .filter(|value| value.is_finite() && *value > 0.0),
        rate_check: cfg.iperf.rate_check.clone(),
        link_profiles: cfg.link_profiles.clone(),
        ctstraffic: cfg.ctstraffic.clone(),
        ctstraffic_config_error: ctstraffic_common_config_error(configured_duration),
    })
}

/// UDP 按整条路径的可信负载上限裁剪流数。
/// RNDIS 3.7G 协商按约 2.5G，10GUSB 的 4.2G 已知显示 bug 不按 4.2G 裁剪。
fn allowed_udp_streams_for_mbps(
    sender: &Endpoint,
    receiver: &Endpoint,
    bandwidth_mbps: f64,
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
    let bw = bandwidth_mbps;
    if bw <= 0.0 {
        return want;
    }
    let max_n = (speed / bw).floor() as u32;
    max_n.min(want)
}

/// 一条方向腿实际下发的 UDP 负载：单流 `-b` 与流数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct UdpLoad {
    pub bits_per_second: u64,
    pub mbps: f64,
    pub streams: u32,
    /// 单流带宽被路径上限压低时，记下原始请求值，供任务标签与报表说明。
    pub clipped_from_mbps: Option<f64>,
}

impl UdpLoad {
    /// iperf3 的无后缀带宽值按 bit/s 解释。传精确整数可避免依赖它对
    /// `Gbps` 等长后缀的非文档兼容行为。
    pub(crate) fn iperf_arg(self) -> String {
        self.bits_per_second.to_string()
    }
}

/// 按整条路径的可信负载上限决定这条腿的 `-b` 和流数。
///
/// 优先降流数（保持单流带宽不变），流数已经降到 1 仍然超限时才压 `-b`。
///
/// 旧行为在「单流带宽就已经超过路径上限」时返回 0 流，调用方据此把任务整个
/// 跳过。run_20260825_215915_7684 里 80 条 UDP 命令全部带着同一个
/// `-b 2600000000`，其中相当一部分打向 1Gbps 收端，制造出 60~99% 的丢包——
/// 那是配置出来的丢包，不是测出来的。给 1Gbps 收端灌 1Gbps 拿到一个真实
/// 结论，永远好过跳过或者灌 2.6G 拿到一个必然失败的结论。
/// 详见 .ai/DESIGN-v4.3.0.md D4。
pub(crate) fn udp_load_for_leg(
    sender: &Endpoint,
    receiver: &Endpoint,
    requested: ParsedBandwidth,
    want_streams: u32,
    limit: bool,
    explicit: bool,
    rate_cfg: &RateCheckCfg,
) -> UdpLoad {
    let want = want_streams.max(1);
    let as_requested = |streams: u32| UdpLoad {
        bits_per_second: requested.bits_per_second,
        mbps: requested.mbps,
        streams,
        clipped_from_mbps: None,
    };
    // `explicit` = 这条链路在 link_profiles 里被专门指定过带宽。
    // 那是操作者对这条链路的明确判断，自动裁剪不该覆盖它——裁剪是给
    // 没配过的链路兜底用的安全网，不是用来推翻人的决定的。
    if explicit || !limit || requested.mbps <= 0.0 {
        return as_requested(want);
    }
    let Some(ceiling) = rate::path_payload_ceiling_mbps(&sender.nic, &receiver.nic, rate_cfg)
    else {
        return as_requested(want);
    };
    let fit = (ceiling / requested.mbps).floor();
    if fit >= 1.0 {
        return as_requested((fit as u32).clamp(1, want));
    }
    // 单流就已经超过整条路径的可信上限：压 -b，而不是放弃这条腿。
    let bits_per_second = (ceiling * 1_000_000.0).round().max(1.0) as u64;
    UdpLoad {
        bits_per_second,
        mbps: bits_per_second as f64 / 1_000_000.0,
        streams: 1,
        clipped_from_mbps: Some(requested.mbps),
    }
}

/// iperf UDP 单元的“预计总耗时”（秒），按典型成功路径估算：
/// 第一次完整尝试的时长 + 启动/收尾/错峰开销。
///
/// 单流 UDP 的重试只在“当次尝试没有产生任何有效测量”时发生，属于异常路径；
/// 若按最坏情况（最多 3 次完整尝试 × 每次再附加 130s 宽限）累加，
/// 180s 的单流 UDP 项会被估成 14+ 分钟，开始前的总耗时规划会严重偏大。
/// 因此这里统一按一次尝试估算，与多流 UDP / TCP 口径一致。
///
/// 错峰只按单腿最大流数计算：双向 AB/BA 腿是并行执行的，
/// 不能把两条腿的流数相加，否则双向会凭空多出毫秒级错峰取整。
fn udp_estimated_secs(
    duration: u64,
    max_leg_streams: u64,
    mode: RateMode,
    rate_cfg: &RateCheckCfg,
) -> u64 {
    let stagger_ms = max_leg_streams
        .saturating_sub(1)
        .saturating_mul(rate_cfg.launch_interval_ms.clamp(0, 1_000));
    let discovery_ms = if mode == RateMode::Discover {
        3_u64
            .saturating_mul(rate_cfg.discovery_step_secs)
            .saturating_mul(1_000)
    } else {
        0
    };
    duration
        .saturating_add(rate_cfg.background_secs.min(30))
        .saturating_add(rate_cfg.startup_timeout_secs)
        .saturating_add(rate_cfg.settle_secs)
        .saturating_add(5)
        .saturating_add(stagger_ms.saturating_add(discovery_ms).div_ceil(1_000))
}

/// 计划页要显示的一行「这条腿最终按什么门限判」。
///
/// 预览必须直接给出**最终生效值**，而不是把请求体里的字段原样铺出来：
/// `RateTargets::for_direction("ab")` 是 `ab.or(forward)`，任务里显式填的
/// `forward` 可以被频段表插进来的 `ab` 无声推翻，两个字段都还在，人看不出来。
fn target_line(direction: &str, target: Option<f64>, source: RxTargetSource) -> String {
    let prefix = match direction {
        "ab" => "A→B ",
        "ba" => "B→A ",
        "bidir" => "双向 ",
        _ => "",
    };
    match target {
        Some(value) => format!("{prefix}门限 {value:.0}Mbps（{}）", source.label()),
        None => format!("{prefix}{}", source.label()),
    }
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

/// 生成全部任务单元。返回 (units, 提示信息列表)
pub fn build_units(
    specs: &[SpecNorm],
    require_same_subnet: bool,
    next_port: &mut u16,
) -> (Vec<Unit>, Vec<String>) {
    let mut units: Vec<Unit> = Vec::new();
    let mut notices: Vec<String> = Vec::new();
    // 同一条门限算式会在每个档位 × 每条腿上重复解析出来，去重后只提示一次。
    let mut rx_target_notes: HashSet<String> = HashSet::new();

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
                                if let Some(error) = spec.stream_config_error(false) {
                                    notices.push(format!(
                                        "{} 的 iperf TCP 流数配置非法，将按兼容范围使用 {} 流: {error}",
                                        spec.name,
                                        spec.effective_tcp_streams()
                                    ));
                                }
                                let tcp_streams = spec.effective_tcp_streams();
                                // 空的 -w 档位列表 = 跑一条不带 -w 的 TCP（附加 TCP
                                // 参数组把 -w 留空时会这样）。默认组经过 non_empty
                                // 兜底、老配置也总有窗口，都不会走到 None 这一支，
                                // 行为与从前逐字一致。
                                let windows: Vec<Option<&String>> = if spec.tcp_windows.is_empty() {
                                    vec![None]
                                } else {
                                    spec.tcp_windows.iter().map(Some).collect()
                                };
                                for w in windows {
                                    let (pname, plabel) = match w {
                                        Some(w) => (
                                            format!("tcp_w{}_P{}", w, tcp_streams),
                                            format!("TCP -w {} -P {}", w, tcp_streams),
                                        ),
                                        None => (
                                            format!("tcp_noW_P{}", tcp_streams),
                                            format!("TCP -P {}", tcp_streams),
                                        ),
                                    };
                                    if let Some(w) = w {
                                        for (s, d, _tag) in &pairs {
                                            if let Some(msg) = oversized_socket_buffer_notice(
                                                &spec.name,
                                                &plabel,
                                                w,
                                                tcp_streams,
                                                spec.duration,
                                                s,
                                                d,
                                                &spec.rate_check,
                                            ) {
                                                notices.push(msg);
                                            }
                                        }
                                    }
                                    let mut legs = Vec::new();
                                    // Ping 单元没有速率门限：RTT 与丢包的判定在别处。
                                    let mut target_lines: Vec<String> = Vec::new();
                                    for (s, d, tag) in &pairs {
                                        let flow_direction =
                                            if bidir { tag.to_string() } else { dir.clone() };
                                        let leg_policy = link_policy(spec, s, d);
                                        note_rx_target(
                                            &mut notices,
                                            &mut rx_target_notes,
                                            &spec.name,
                                            &leg_policy,
                                        );
                                        let rate_plan = leg_rate_plan(
                                            spec,
                                            &leg_policy,
                                            &flow_direction,
                                            bidir,
                                            &s.nic,
                                            &d.nic,
                                        );
                                        note_target_cap(
                                            &mut notices,
                                            &mut rx_target_notes,
                                            &spec.name,
                                            &rate_plan,
                                        );
                                        let (effective_mode, target) =
                                            (rate_plan.mode, rate_plan.target_mbps);
                                        target_lines.push(target_line(
                                            &flow_direction,
                                            target,
                                            rate_plan.source,
                                        ));
                                        let t = IperfTask {
                                            v6,
                                            udp: false,
                                            profile_name: pname.clone(),
                                            profile_label: plabel.clone(),
                                            src: (*s).clone(),
                                            dst: (*d).clone(),
                                            port: alloc_port(next_port),
                                            duration: spec.duration,
                                            extra: match w {
                                                Some(w) => vec![
                                                    "-w".into(),
                                                    w.clone(),
                                                    "-P".into(),
                                                    tcp_streams.to_string(),
                                                ],
                                                None => {
                                                    vec!["-P".into(), tcp_streams.to_string()]
                                                }
                                            },
                                            stream_idx: 0,
                                            rate_mode: effective_mode,
                                            rx_target_mbps: target,
                                            offered_per_stream_mbps: None,
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
                                    let id =
                                        tcp_resume_unit_id_v2(spec, ip_tag, dir, &pname, &legs);
                                    units.push(Unit {
                                        id,
                                        title,
                                        link_group: spec.link_group.clone(),
                                        bidir,
                                        target_lines,
                                        bidir_total_target_mbps: bidir
                                            .then_some(spec.rate_target_bidir_total)
                                            .flatten(),
                                        direction: dir.to_string(),
                                        legs,
                                        est_secs: spec.duration + 10,
                                    });
                                }
                            } else if tr == "udp" {
                                if let Some(error) = spec.stream_config_error(true) {
                                    notices.push(format!(
                                        "{} 的 iperf UDP 流数配置非法，将按兼容范围使用 {} 流: {error}",
                                        spec.name,
                                        spec.effective_udp_streams()
                                    ));
                                }
                                let udp_streams = spec.effective_udp_streams();
                                for prof in &spec.udp_profiles {
                                    let parsed_bandwidth = match prof.parsed_bandwidth() {
                                        Ok(value) => value,
                                        Err(error) => {
                                            notices.push(format!(
                                                "跳过 {} 的 iperf UDP profile {}：{error}；带宽格式非法，未生成任务",
                                                spec.name,
                                                prof.label()
                                            ));
                                            continue;
                                        }
                                    };
                                    // 每个方向腿按 min(发送口, 接收口) 的路径上限
                                    // 各自决定 -b 与流数：同一条链路的两个方向
                                    // 能力可以差很多，共用一个 -b 没有物理依据。
                                    let leg_loads: Vec<UdpLoad> = pairs
                                        .iter()
                                        .map(|(s, d, _tag)| {
                                            // 单口覆盖 / 角色配对可以改写这条腿的
                                            // 单流带宽；解析不了就退回全局档位，
                                            // 绝不因为一个笔误让任务凭空消失。
                                            let configured = link_policy(spec, s, d)
                                                .udp_bandwidth
                                                .and_then(|value| {
                                                    UdpProfile::bw(&value).parsed_bandwidth().ok()
                                                });
                                            udp_load_for_leg(
                                                s,
                                                d,
                                                configured.unwrap_or(parsed_bandwidth),
                                                udp_streams,
                                                spec.udp_limit,
                                                configured.is_some(),
                                                &spec.rate_check,
                                            )
                                        })
                                        .collect();
                                    // 发送口可以单独覆盖 `-l`：同一条用例在不同网口上
                                    // 要用不同报文长度是常见需求。按腿算一次，标签和
                                    // 命令都从这里取，免得两边各算一遍再对不上。
                                    let leg_profiles: Vec<UdpProfile> = pairs
                                        .iter()
                                        .map(|(s, d, _tag)| UdpProfile {
                                            bandwidth: prof.bandwidth.clone(),
                                            length: link_policy(spec, s, d)
                                                .udp_length
                                                .or_else(|| prof.length.clone()),
                                            window: prof.window.clone(),
                                        })
                                        .collect();
                                    for ((s, d, _tag), load) in pairs.iter().zip(leg_loads.iter()) {
                                        if let Some(from) = load.clipped_from_mbps {
                                            notices.push(format!(
                                                "{} {}：{} -> {} 路径上限不足，-b 由 {:.0}Mbps 裁剪到 {:.0}Mbps",
                                                spec.name,
                                                prof.label(),
                                                s.nic.name,
                                                d.nic.name,
                                                from,
                                                load.mbps
                                            ));
                                        }
                                    }
                                    let mut legs = Vec::new();
                                    // Ping 单元没有速率门限：RTT 与丢包的判定在别处。
                                    let mut target_lines: Vec<String> = Vec::new();
                                    let mut max_n = 1;
                                    for (leg_idx, ((s, d, tag), load)) in
                                        pairs.iter().zip(leg_loads.iter()).enumerate()
                                    {
                                        let n = load.streams;
                                        max_n = max_n.max(n);
                                        // 标签必须反映**实际下发**的 -b。链路策略
                                        // 覆盖和路径裁剪都会改它，而报表里的
                                        // 「类型 / 参数」列是很多人唯一会看的地方——
                                        // 那里印着 2.6G、命令行却是 1G，比不印更糟。
                                        // 裁剪与否只能问 clipped_from_mbps：链路策略
                                        // 先把 2.5G 改成 2.6G、路径上限再裁回 2500，
                                        // 拿全局档位去比会得出「没变」，把两次改写
                                        // 一起抹掉。
                                        // 标签必须反映**实际下发**的 -l，不是档位里那个。
                                        let leg_policy = link_policy(spec, s, d);
                                        let effective = &leg_profiles[leg_idx];
                                        let leg_label = if let Some(from) = load.clipped_from_mbps {
                                            format!(
                                                "{}（按路径上限从 {:.0}M 裁剪至 {:.0}M）",
                                                effective.label(),
                                                from,
                                                load.mbps
                                            )
                                        } else if (load.mbps - parsed_bandwidth.mbps).abs()
                                            >= f64::EPSILON
                                        {
                                            format!(
                                                "{}（按链路策略至 {:.0}M）",
                                                effective.label(),
                                                load.mbps
                                            )
                                        } else {
                                            effective.label()
                                        };
                                        let mut extra: Vec<String> =
                                            vec!["-b".into(), load.iperf_arg()];
                                        if let Some(l) = &effective.length {
                                            extra.push("-l".into());
                                            extra.push(l.clone());
                                        }
                                        if let Some(w) = &effective.window {
                                            extra.push("-w".into());
                                            extra.push(w.clone());
                                        }
                                        let flow_direction =
                                            if bidir { tag.to_string() } else { dir.clone() };
                                        note_rx_target(
                                            &mut notices,
                                            &mut rx_target_notes,
                                            &spec.name,
                                            &leg_policy,
                                        );
                                        let rate_plan = leg_rate_plan(
                                            spec,
                                            &leg_policy,
                                            &flow_direction,
                                            bidir,
                                            &s.nic,
                                            &d.nic,
                                        );
                                        note_target_cap(
                                            &mut notices,
                                            &mut rx_target_notes,
                                            &spec.name,
                                            &rate_plan,
                                        );
                                        let (effective_mode, target) =
                                            (rate_plan.mode, rate_plan.target_mbps);
                                        target_lines.push(target_line(
                                            &flow_direction,
                                            target,
                                            rate_plan.source,
                                        ));
                                        // offered 必须跟着实际下发的 -b 走，否则
                                        // 报表里的「请求负载」和命令行对不上。
                                        let offered_per_stream_mbps = Some(load.mbps);
                                        let mk = |idx: usize, port: u16| IperfTask {
                                            v6,
                                            udp: true,
                                            profile_name: prof.name(),
                                            profile_label: leg_label.clone(),
                                            src: (*s).clone(),
                                            dst: (*d).clone(),
                                            port,
                                            duration: spec.duration,
                                            extra: extra.clone(),
                                            stream_idx: idx,
                                            rate_mode: effective_mode,
                                            rx_target_mbps: target,
                                            offered_per_stream_mbps,
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
                                    // 标题里的 -b 必须是**实际下发**的值。链路策略和
                                    // 路径裁剪都会改它，而任务清单（控制台的「预览
                                    // 任务」、日志开头的编号列表）是很多人唯一会看
                                    // 的地方——那里印着全局档位、命令行却是别的数，
                                    // 会让人以为自己填的值没生效。
                                    //
                                    // 两条腿取值不同时退回档位标签：一个标题写不下
                                    // 两个方向，逐行的 profile_label 里各自写着准确值。
                                    let uniform = leg_loads.first().is_some_and(|first| {
                                        leg_loads.iter().all(|load| {
                                            (load.mbps - first.mbps).abs() < f64::EPSILON
                                        })
                                    });
                                    let effective = leg_loads
                                        .first()
                                        .map(|first| first.mbps)
                                        .unwrap_or(parsed_bandwidth.mbps);
                                    // `-l` 被发送口改写时，标题同样不能再印档位里的原值。
                                    let leg_lengths: Vec<Option<String>> =
                                        leg_profiles.iter().map(|p| p.length.clone()).collect();
                                    let length_changed =
                                        leg_lengths.iter().any(|length| *length != prof.length);
                                    let changed = length_changed
                                        || leg_loads.iter().any(|load| {
                                            (load.mbps - parsed_bandwidth.mbps).abs()
                                                >= f64::EPSILON
                                        });
                                    let profile_label = if !changed {
                                        prof.label()
                                    } else {
                                        // 两条腿取值不同就两个都印（顺序即腿序 ab/ba）：
                                        // 退回全局档位会显示一个谁都没在用的数。
                                        let bw = if uniform {
                                            format!("{effective:.0}m")
                                        } else {
                                            leg_loads
                                                .iter()
                                                .map(|load| format!("{:.0}m", load.mbps))
                                                .collect::<Vec<_>>()
                                                .join("/")
                                        };
                                        let mut label = format!("UDP -b {bw}");
                                        let uniform_length =
                                            leg_lengths.first().is_some_and(|first| {
                                                leg_lengths.iter().all(|length| length == first)
                                            });
                                        if uniform_length {
                                            if let Some(Some(l)) = leg_lengths.first() {
                                                label.push_str(&format!(" -l {l}"));
                                            }
                                        } else {
                                            let shown = leg_lengths
                                                .iter()
                                                .map(|length| length.as_deref().unwrap_or("默认"))
                                                .collect::<Vec<_>>()
                                                .join("/");
                                            label.push_str(&format!(" -l {shown}"));
                                        }
                                        if let Some(w) = &prof.window {
                                            label.push_str(&format!(" -w {w}"));
                                        }
                                        label
                                    };
                                    let title = format!(
                                        "{}IPERF {} {}{} | {}",
                                        if bidir { "★★双向 " } else { "" },
                                        ip_tag,
                                        profile_label,
                                        stream_note,
                                        route_str
                                    );
                                    let id = udp_resume_unit_id_v4(spec, ip_tag, dir, prof, &legs);
                                    // 错峰按单腿最大流数估算：双向双腿并行，不能把
                                    // 两条腿的流数相加。
                                    units.push(Unit {
                                        id,
                                        title,
                                        link_group: spec.link_group.clone(),
                                        bidir,
                                        target_lines,
                                        bidir_total_target_mbps: bidir
                                            .then_some(spec.rate_target_bidir_total)
                                            .flatten(),
                                        direction: dir.to_string(),
                                        legs,
                                        est_secs: udp_estimated_secs(
                                            spec.duration,
                                            max_n as u64,
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
                    let topology_blocked = !v6 && !same24_ok;
                    let mut topology_notice_emitted = false;
                    for transport in &spec.transports {
                        if transport == "tcp" {
                            let tcp_streams = spec.effective_tcp_streams();
                            for window in &spec.tcp_windows {
                                let mut setup_errors = cts_task_config_errors(spec, false);
                                let mut window_invalid = false;
                                let window_bytes = match cts_window_bytes(window) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        window_invalid = true;
                                        setup_errors.push(format!(
                                            "CTS TCP socket buffer {window:?} 非法: {error}"
                                        ));
                                        None
                                    }
                                };
                                let setup_error =
                                    (!setup_errors.is_empty()).then(|| setup_errors.join("；"));
                                if topology_blocked && setup_error.is_none() {
                                    if !topology_notice_emitted {
                                        notices.push(format!(
                                                "跳过 {} 的 ctsTraffic：两端 IPv4 不同 /24 ({} vs {})，无法直连灌包",
                                                spec.name, spec.src.nic.ipv4, spec.dst.nic.ipv4
                                            ));
                                        topology_notice_emitted = true;
                                    }
                                    continue;
                                }
                                if let Some(error) = &setup_error {
                                    notices.push(format!(
                                        "{} CTS TCP 配置非法，将记录 SETUP_ERROR: {error}",
                                        spec.name
                                    ));
                                }
                                let window_label = if window_invalid {
                                    format!("socket-buffer {window}（非法）")
                                } else {
                                    window_bytes
                                        .map(|bytes| format!("socket-buffer {window} ({bytes}B)"))
                                        .unwrap_or_else(|| "socket-buffer 自动".into())
                                };
                                let profile_name = format!(
                                    "cts_tcp_w{}_c{}",
                                    if window.trim().is_empty() {
                                        "auto"
                                    } else {
                                        window
                                    },
                                    tcp_streams
                                );
                                let profile_label =
                                    format!("CTS TCP {window_label} ×{}连接", tcp_streams);
                                let mut legs = Vec::new();
                                // Ping 单元没有速率门限：RTT 与丢包的判定在别处。
                                let mut target_lines: Vec<String> = Vec::new();
                                for (src, dst, tag) in &pairs {
                                    let flow_direction =
                                        if bidir { tag.to_string() } else { dir.clone() };
                                    let rate_plan = leg_rate_plan(
                                        spec,
                                        &link_policy(spec, src, dst),
                                        &flow_direction,
                                        bidir,
                                        &src.nic,
                                        &dst.nic,
                                    );
                                    note_target_cap(
                                        &mut notices,
                                        &mut rx_target_notes,
                                        &spec.name,
                                        &rate_plan,
                                    );
                                    let (effective_mode, target) =
                                        (rate_plan.mode, rate_plan.target_mbps);
                                    target_lines.push(target_line(
                                        &flow_direction,
                                        target,
                                        rate_plan.source,
                                    ));
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
                                            streams: tcp_streams,
                                            window_bytes,
                                            bits_per_second: None,
                                            datagram_bytes: None,
                                            frame_rate: spec.ctstraffic.udp_frame_rate,
                                            buffer_depth_secs: spec
                                                .ctstraffic
                                                .udp_buffer_depth_secs,
                                            status_update_ms: spec.ctstraffic.status_update_ms,
                                            rate_mode: effective_mode,
                                            rx_target_mbps: target,
                                            offered_total_mbps: None,
                                            setup_error: setup_error.clone(),
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
                                    link_group: spec.link_group.clone(),
                                    bidir,
                                    target_lines,
                                    bidir_total_target_mbps: bidir
                                        .then_some(spec.rate_target_bidir_total)
                                        .flatten(),
                                    direction: dir.to_string(),
                                    legs,
                                    est_secs: if setup_error.is_some() {
                                        1
                                    } else {
                                        spec.duration.saturating_add(15)
                                    },
                                });
                            }
                        } else if transport == "udp" {
                            let udp_streams = spec.effective_udp_streams();
                            for profile in &spec.udp_profiles {
                                let mut setup_errors = cts_task_config_errors(spec, true);
                                let window_bytes = match profile
                                    .window
                                    .as_deref()
                                    .map(cts_window_bytes)
                                    .transpose()
                                {
                                    Ok(value) => value.flatten(),
                                    Err(error) => {
                                        setup_errors.push(format!(
                                            "CTS UDP socket buffer {:?} 非法: {error}",
                                            profile.window.as_deref().unwrap_or_default()
                                        ));
                                        None
                                    }
                                };
                                let bandwidth = match cts_udp_bandwidth(profile) {
                                    Ok(value) => Some(value),
                                    Err(error) => {
                                        setup_errors.push(error);
                                        None
                                    }
                                };
                                let datagram_bytes = match cts_datagram_bytes(profile) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        setup_errors.push(error);
                                        None
                                    }
                                };
                                let setup_error =
                                    (!setup_errors.is_empty()).then(|| setup_errors.join("；"));
                                if topology_blocked && setup_error.is_none() {
                                    if !topology_notice_emitted {
                                        notices.push(format!(
                                                "跳过 {} 的 ctsTraffic：两端 IPv4 不同 /24 ({} vs {})，无法直连灌包",
                                                spec.name, spec.src.nic.ipv4, spec.dst.nic.ipv4
                                            ));
                                        topology_notice_emitted = true;
                                    }
                                    continue;
                                }
                                if let Some(error) = &setup_error {
                                    notices.push(format!(
                                        "{} CTS UDP {} 配置非法，将记录 SETUP_ERROR: {error}",
                                        spec.name,
                                        profile.label()
                                    ));
                                }
                                let mut legs = Vec::new();
                                // Ping 单元没有速率门限：RTT 与丢包的判定在别处。
                                let mut target_lines: Vec<String> = Vec::new();
                                let mut max_streams = 1u32;
                                for (src, dst, tag) in &pairs {
                                    let streams = if setup_error.is_some() {
                                        udp_streams
                                    } else {
                                        allowed_udp_streams_for_mbps(
                                            src,
                                            dst,
                                            bandwidth
                                                .expect("合法 CTS UDP 配置必须有严格带宽值")
                                                .mbps,
                                            udp_streams,
                                            spec.udp_limit,
                                            &spec.rate_check,
                                        )
                                    };
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
                                    let flow_direction =
                                        if bidir { tag.to_string() } else { dir.clone() };
                                    let rate_plan = leg_rate_plan(
                                        spec,
                                        &link_policy(spec, src, dst),
                                        &flow_direction,
                                        bidir,
                                        &src.nic,
                                        &dst.nic,
                                    );
                                    note_target_cap(
                                        &mut notices,
                                        &mut rx_target_notes,
                                        &spec.name,
                                        &rate_plan,
                                    );
                                    let (effective_mode, target) =
                                        (rate_plan.mode, rate_plan.target_mbps);
                                    target_lines.push(target_line(
                                        &flow_direction,
                                        target,
                                        rate_plan.source,
                                    ));
                                    // 每流带宽 × 流数 = 整条腿的总量。CTS 侧的
                                    // 字段是**总量**口径，与 iperf 的每流口径相反。
                                    let offered_total_mbps =
                                        bandwidth.map(|value| value.mbps * streams as f64);
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
                                            bits_per_second: bandwidth
                                                .map(|value| value.bits_per_second),
                                            datagram_bytes,
                                            frame_rate: spec.ctstraffic.udp_frame_rate,
                                            buffer_depth_secs: spec
                                                .ctstraffic
                                                .udp_buffer_depth_secs,
                                            status_update_ms: spec.ctstraffic.status_update_ms,
                                            rate_mode: effective_mode,
                                            rx_target_mbps: target,
                                            offered_total_mbps,
                                            setup_error: setup_error.clone(),
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
                                    link_group: spec.link_group.clone(),
                                    bidir,
                                    target_lines,
                                    bidir_total_target_mbps: bidir
                                        .then_some(spec.rate_target_bidir_total)
                                        .flatten(),
                                    direction: dir.to_string(),
                                    legs,
                                    est_secs: if setup_error.is_some() {
                                        1
                                    } else {
                                        spec.duration.saturating_add(15)
                                    },
                                });
                            }
                        }
                    }
                }

                // ---------- ping ----------
                if spec.kinds.iter().any(|k| k == "ping") {
                    for payload in &spec.payload_sizes {
                        let mut legs = Vec::new();
                        // Ping 单元没有速率门限：RTT 与丢包的判定在别处。
                        let target_lines: Vec<String> = Vec::new();
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
                            link_group: spec.link_group.clone(),
                            bidir,
                            target_lines,
                            // Ping 不是吞吐测试，没有 RX 合计门限这回事。
                            bidir_total_target_mbps: None,
                            direction: dir.to_string(),
                            legs,
                            est_secs: ping_estimated_secs(spec.ping_count),
                        });
                    }
                }
            }
        }
    }
    (units, notices)
}

/// 一个 PING 单元的预计墙钟秒数。
///
/// `ping` 每秒发一个包，主体就是 `count - 1` 个间隔。原来的 `count + 5` 漏的是
/// **收尾等待**：最后一个包没回来时，BSD ping 还要再等约 10 秒才收摊。实测
/// （macOS，65500 字节打网关，全程无回包）：
///
/// | count | 实测 | 旧公式 `count+5` |
/// |-------|------|------------------|
/// | 5     | 15.0s| 10s              |
/// | 20    | 30.1s| 25s              |
/// | 40    | 50.2s| 45s              |
///
/// 三档都正好是 `count + 10`，即旧公式稳定少算 5 秒。这里取 `+12`，多出的 2 秒
/// 留给进程启动和一次 RPC 往返。包能正常回来时实际约 `count - 1` 秒，估算偏
/// 保守——预计耗时宁可报多不报少。
///
/// 这条估算只覆盖「包基本能回来」和「最后一个包丢了」两种形态。Windows 的
/// `ping` 对**每一个**没回来的包都要等满 `-w` 的 4 秒，一个 100% 丢包的单元实际
/// 会跑到 `count × 4` 秒。那是故障路径、事前无法预测，估算里不假装知道；执行侧
/// 的超时预算（`count * 5 + 60`）本来就按这个上限留的，不会被误杀。
fn ping_estimated_secs(count: u32) -> u64 {
    count as u64 + 12
}

fn alloc_port(next: &mut u16) -> u16 {
    let p = *next;
    *next = next.wrapping_add(1).max(PORT_BASE);
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DirectionalBandwidth, NicProfile, RoleProfile, UdpProfile};

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

    fn host(hostname: &str, name: &str, role: &str, ip: &str) -> HostInfo {
        HostInfo {
            hostname: hostname.into(),
            os: "test".into(),
            interfaces: vec![nic(name, role, ip, 2500)],
        }
    }

    fn base_spec() -> SpecNorm {
        SpecNorm {
            name: "t".into(),
            link_group: String::new(),
            src: ep(Side::Master, "eth0", "SGMII2.5G", "192.168.1.2", 2500),
            dst: ep(Side::Agent, "eth0", "SGMII2.5G", "192.168.1.3", 2500),
            directions: vec!["ab".into()],
            kinds: vec!["iperf".into()],
            transports: vec!["tcp".into()],
            ipvers: vec!["v4".into()],
            streams: 1,
            tcp_streams: 0,
            udp_streams: 0,
            duration: 10,
            ping_count: 4,
            payload_sizes: vec![32],
            tcp_windows: vec!["64k".into()],
            udp_profiles: vec![UdpProfile::bw("500m")],
            udp_limit: true,
            rate_mode: RateMode::Auto,
            rate_targets: RateTargets::default(),
            rate_targets_bidir: RateTargets::default(),
            rate_target_bidir_total: None,
            rate_check: RateCheckCfg::default(),
            link_profiles: LinkProfiles::default(),
            ctstraffic: CtsTrafficCfg::default(),
            ctstraffic_config_error: None,
        }
    }

    /// builder 永远不该把「受控参数」拼进 `IperfTask.extra`。
    ///
    /// `cmd::iperf::client_args` 把 `extra` 原样接在自己拼好的参数后面，而 iperf3
    /// 对重复参数是**后者覆盖前者**。真出现一个 `-f M`/`-t 30`，解析器和有效窗口
    /// 会各自走进另一条分支，而输出看起来一切正常——这类错只会表现为「速率莫名
    /// 低了 4.6%」。今天 builder 只从有类型的配置字段拼 `-w`/`-P`/`-b`/`-l`，
    /// 所以这条断言现在是免费的；它挡的是以后有人往 extra 里加透传口子。
    /// agent 侧另有 `check_client_extra` 在请求边界上挡同一件事。
    #[test]
    fn the_builder_never_emits_iperf_flags_that_would_change_the_measurement() {
        fn collect(unit: &Unit, into: &mut Vec<(String, Vec<String>)>) {
            for leg in &unit.legs {
                match &leg.kind {
                    LegKind::IperfSingle(task) => {
                        into.push((unit.id.clone(), task.extra.clone()));
                    }
                    LegKind::IperfGroup { streams, .. } => {
                        for task in streams {
                            into.push((unit.id.clone(), task.extra.clone()));
                        }
                    }
                    LegKind::CtsTraffic(_) | LegKind::Ping(_) => {}
                }
            }
        }

        let mut specs = Vec::new();
        // TCP：多档窗口 × 多流，双向。
        let mut tcp = base_spec();
        tcp.directions = vec!["ab".into(), "ba".into(), "bidir".into()];
        tcp.transports = vec!["tcp".into()];
        tcp.tcp_windows = vec!["64k".into(), "4m".into()];
        tcp.streams = 10;
        tcp.ipvers = vec!["v4".into(), "v6".into()];
        specs.push(tcp);
        // UDP：三条轴都有值、多流（走 IperfGroup）、开按链路上限裁剪。
        let mut udp = base_spec();
        udp.directions = vec!["ab".into(), "ba".into(), "bidir".into()];
        udp.transports = vec!["udp".into()];
        udp.udp_profiles = vec![
            UdpProfile {
                bandwidth: "1000m".into(),
                length: Some("14k".into()),
                window: Some("256m".into()),
            },
            UdpProfile {
                bandwidth: "2500m".into(),
                length: Some("64".into()),
                window: Some("4m".into()),
            },
        ];
        udp.udp_streams = 4;
        udp.streams = 4;
        udp.udp_limit = true;
        specs.push(udp);

        let mut port = 45000u16;
        let (units, _notices) = build_units(&specs, true, &mut port);
        assert!(!units.is_empty(), "这组 spec 应当展开出单元");

        let mut tasks = Vec::new();
        for unit in &units {
            collect(unit, &mut tasks);
        }
        assert!(!tasks.is_empty(), "应当有 iperf 腿");
        for (unit_id, extra) in &tasks {
            let hits = crate::cmd::iperf::reserved_flags_in_extra(extra);
            assert!(
                hits.is_empty(),
                "单元 {unit_id} 的 extra 里出现了受控参数 {hits:?}（extra={extra:?}）"
            );
        }
    }

    /// **稳定 ID / 端口顺序 / 单元展开的全量快照。**
    ///
    /// 这条测试守的是这个仓库里最贵的一条不变量：`Unit.id` 是 RESUME 的
    /// identity。它变了，用户所有的历史 PASS 记录当场全部失效——24 小时内本该
    /// 跳过的单元会全部重跑，一次 11.5 小时的验收变成两次，而且**没有任何报错**，
    /// 只是「怎么又从头跑了」。端口顺序同理：它进 identity，也决定并发资源分配。
    ///
    /// 快照是**内联的字面量**而不是外部文件：拆分 `builder.rs`（R4）时，任何
    /// 一处顺序、拼接、命名的手滑都会让这里逐字段报出差异，而不是等到用户
    /// 现场发现 resume 不命中。
    ///
    /// 如果这条测试红了，先问「我是不是改了不该改的东西」，而不是更新快照。
    /// 真要改 identity 模板，那是一次**需要说明的兼容性事件**（会清空所有人的
    /// resume 缓存），不是顺手改一行。
    #[test]
    fn the_full_unit_expansion_is_byte_stable() {
        // 一份把主要维度都摊开的 spec：双向 + 单向、V4+V6、TCP 多窗口多流、
        // UDP 多档位多流。拆文件之前之后必须逐字节一致。
        let mut tcp = base_spec();
        tcp.name = "snapshot-tcp".into();
        tcp.directions = vec!["ab".into(), "ba".into(), "bidir".into()];
        tcp.transports = vec!["tcp".into()];
        tcp.ipvers = vec!["v4".into(), "v6".into()];
        tcp.tcp_windows = vec!["64k".into(), "4m".into()];
        tcp.streams = 10;

        let mut udp = base_spec();
        udp.name = "snapshot-udp".into();
        udp.directions = vec!["ab".into(), "bidir".into()];
        udp.transports = vec!["udp".into()];
        udp.ipvers = vec!["v4".into()];
        udp.udp_profiles = vec![
            UdpProfile {
                bandwidth: "1000m".into(),
                length: Some("14k".into()),
                window: Some("256m".into()),
            },
            UdpProfile {
                bandwidth: "2500m".into(),
                length: None,
                window: None,
            },
        ];
        udp.udp_streams = 4;
        udp.streams = 4;

        let mut ping = base_spec();
        ping.name = "snapshot-ping".into();
        ping.kinds = vec!["ping".into()];
        ping.transports = Vec::new();
        ping.directions = vec!["ab".into(), "ba".into()];
        ping.payload_sizes = vec![32, 1400];

        let mut port = PORT_BASE;
        let (units, _notices) = build_units(&[tcp, udp, ping], true, &mut port);

        // 指纹里放进所有会影响 resume 命中与执行顺序的东西。
        let fingerprint: Vec<String> = units
            .iter()
            .map(|unit| {
                let legs: Vec<String> = unit
                    .legs
                    .iter()
                    .map(|leg| {
                        let ports: Vec<String> = match &leg.kind {
                            LegKind::IperfSingle(task) => vec![task.port.to_string()],
                            LegKind::IperfGroup { streams, .. } => {
                                streams.iter().map(|task| task.port.to_string()).collect()
                            }
                            LegKind::CtsTraffic(task) => vec![task.port.to_string()],
                            LegKind::Ping(_) => vec!["-".into()],
                        };
                        format!("{}:{}", leg.tag, ports.join("+"))
                    })
                    .collect();
                format!(
                    "{}|{}|bidir={}|dir={}|est={}|legs={}",
                    unit.id,
                    unit.title,
                    unit.bidir,
                    unit.direction,
                    unit.est_secs,
                    legs.join(",")
                )
            })
            .collect();

        // 端口分配器的终点也钉住：它是全局递增的，顺序变了就是资源分配变了。
        let snapshot = format!("{}\n--- next_port={port}", fingerprint.join("\n"));

        // 首次运行时用下面这行把实际值打出来再粘回来；平时它必须原样通过。
        //   println!("{snapshot}");
        // Windows 工作区可能按 core.autocrlf 检出快照为 CRLF；快照钉的是
        // 单元内容与顺序，不应把平台换行符差异误报成展开变化。
        let expected = include_str!("builder_snapshot.txt").replace("\r\n", "\n");
        assert_eq!(
            snapshot.trim_end(),
            expected.trim_end(),
            "\n单元展开发生了变化。\n\
             如果这是 builder 拆文件（R4）过程中出现的，说明搬运没有保持等价，\
             **不要更新快照**——去找搬错的那一处。\n\
             如果是有意改 identity 模板，那会清空所有用户的 resume 缓存，\
             属于需要单独说明的兼容性事件。\n"
        );
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

    fn build_single_iperf_unit(spec: SpecNorm, first_port: u16) -> Unit {
        let mut port = first_port;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(
            notices.is_empty(),
            "unexpected builder notices: {notices:?}"
        );
        assert_eq!(units.len(), 1);
        units.into_iter().next().expect("iperf unit")
    }

    fn build_single_cts_unit(spec: SpecNorm, first_port: u16) -> Unit {
        let mut port = first_port;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(
            notices.is_empty(),
            "unexpected builder notices: {notices:?}"
        );
        assert_eq!(units.len(), 1);
        units.into_iter().next().expect("CTS unit")
    }

    fn build_single_iperf_id(spec: SpecNorm, first_port: u16) -> String {
        build_single_iperf_unit(spec, first_port).id
    }

    fn iperf_single_task(unit: &Unit) -> &IperfTask {
        let LegKind::IperfSingle(task) = &unit.legs[0].kind else {
            panic!("expect single iperf task")
        };
        task
    }

    fn cts_task(unit: &Unit) -> &CtsTrafficTask {
        let LegKind::CtsTraffic(task) = &unit.legs[0].kind else {
            panic!("expect single ctsTraffic task")
        };
        task
    }

    fn set_evb_endpoints(spec: &mut SpecNorm) {
        spec.src = ep(Side::Master, "usb", "10GUSB", "192.168.1.2", 4200);
        spec.dst = ep(Side::Agent, "10g", "10GETH", "192.168.1.3", 10000);
    }

    fn evb_tcp_spec() -> SpecNorm {
        let mut spec = base_spec();
        set_evb_endpoints(&mut spec);
        spec
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
    fn tcp_and_cts_rate_modes_resolve_targets_consistently() {
        // 2321 而不是随手一个大数：这条用例查的是「模式与门限怎么传递」，
        // 门限必须落在 `base_spec()` 那条 2.5G 链路的物理上限（2600 × 95%
        // = 2470）以内，否则会被 `cap_rx_target_to_link_speed` 折算走，
        // 断言到的就不再是传递本身。
        let cases = [
            (RateMode::Auto, None, RateMode::Observe, None),
            (RateMode::Auto, Some(2321.0), RateMode::Verify, Some(2321.0)),
            (RateMode::Verify, None, RateMode::Verify, None),
            (
                RateMode::Verify,
                Some(2321.0),
                RateMode::Verify,
                Some(2321.0),
            ),
            (RateMode::Observe, Some(2321.0), RateMode::Observe, None),
            (RateMode::Discover, Some(2321.0), RateMode::Discover, None),
        ];

        for (configured_mode, configured_target, expected_mode, expected_target) in cases {
            let mut iperf = base_spec();
            iperf.rate_mode = configured_mode;
            iperf.rate_targets.forward = configured_target;
            let iperf_unit = build_single_iperf_unit(iperf, PORT_BASE);
            let iperf_task = iperf_single_task(&iperf_unit);
            assert_eq!(iperf_task.rate_mode, expected_mode);
            assert_eq!(iperf_task.rx_target_mbps, expected_target);

            let mut cts = cts_spec("tcp");
            cts.rate_mode = configured_mode;
            cts.rate_targets.forward = configured_target;
            let cts_unit = build_single_cts_unit(cts, PORT_BASE);
            let cts_task_ref = cts_task(&cts_unit);
            assert_eq!(cts_task_ref.rate_mode, expected_mode);
            assert_eq!(cts_task_ref.rx_target_mbps, expected_target);
        }
    }

    /// 合计门限继续优先（判定口径不变），但「逐方向门限被它盖掉了」必须进
    /// 计划提示。run_20260905_125327_5940 里套件写了 ab/ba 各 900Mbps，频段表
    /// 里一条 bidir_total=900 就把两条腿的门限清空，单元按合计判成 PASS——
    /// 两处配置都在，报告上却看不出是哪一处生效了。
    #[test]
    fn a_bidir_total_that_shadows_per_direction_targets_says_so_in_the_plan() {
        let mut spec = base_spec();
        spec.directions = vec!["bidir".into()];
        spec.rate_mode = RateMode::Verify;
        spec.rate_targets_bidir.ab = Some(900.0);
        spec.rate_targets_bidir.ba = Some(900.0);
        spec.rate_target_bidir_total = Some(900.0);

        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        // 判定口径一个字节都没改：两条腿仍然只测量，合计仍然是唯一结论。
        for leg in &units[0].legs {
            let LegKind::IperfSingle(task) = &leg.kind else {
                panic!("expected iperf legs");
            };
            assert_eq!(task.rx_target_mbps, None);
            assert_eq!(task.rate_mode, RateMode::Observe);
        }
        assert_eq!(units[0].bidir_total_target_mbps, Some(900.0));
        // 变的只是「说不说」。两个方向各一条。
        let shadow: Vec<&String> = notices
            .iter()
            .filter(|line| line.contains("已盖掉逐方向门限"))
            .collect();
        assert_eq!(shadow.len(), 2, "ab/ba 各说一次: {notices:?}");
        assert!(shadow.iter().any(|line| line.contains("ab")));
        assert!(shadow.iter().any(|line| line.contains("ba")));
    }

    /// 没配合计门限时不能凭空冒出这条提示。
    #[test]
    fn per_direction_targets_alone_do_not_trigger_the_shadow_notice() {
        let mut spec = base_spec();
        spec.directions = vec!["bidir".into()];
        spec.rate_mode = RateMode::Verify;
        spec.rate_targets_bidir.ab = Some(900.0);
        spec.rate_targets_bidir.ba = Some(900.0);
        let unit = build_single_iperf_unit(spec, PORT_BASE);
        for leg in &unit.legs {
            let LegKind::IperfSingle(task) = &leg.kind else {
                panic!("expected iperf legs");
            };
            assert_eq!(task.rx_target_mbps, Some(900.0));
        }
    }

    /// 现场回归：run_20260905_125327_5940 的 `以太网 6`（SGMII1G，协商 1000Mbps）
    /// 做发送口时，门限取的是接收口策略的 1800/2000（`resolve_link_policy` 的
    /// 「门限看接收端」），16 个单元实测 934~984——就是 1G 线速——全判 RATE_FAIL。
    /// 那是门限配错了，不是设备不达标。
    #[test]
    fn a_target_above_the_path_ceiling_is_capped_and_the_formula_is_reported() {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "以太网 6", "SGMII1G", "192.168.0.101", 1000);
        spec.dst = ep(Side::Agent, "以太网 18", "SGMII2.5G", "192.168.0.105", 2500);
        spec.rate_mode = RateMode::Verify;
        spec.rate_targets.forward = Some(1_800.0);

        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        let task = iperf_single_task(&units[0]);
        // 1G 口 × 95%：1800 是这条路径上跑不到的数。
        assert_eq!(task.rx_target_mbps, Some(950.0));
        assert_eq!(task.rate_mode, RateMode::Verify);
        // 折算过就必须说出来，否则报告上「门限 950」和配置里「1800」对不上。
        assert!(
            notices
                .iter()
                .any(|line| line.contains("超过这条链路的物理上限")
                    && line.contains("950")
                    && line.contains("以太网 6")),
            "封顶算式必须进计划提示: {notices:?}"
        );
    }

    /// 反过来：门限在路径上限之内时一个字节都不能动，提示也不能冒出来。
    #[test]
    fn a_reachable_target_is_left_alone_by_the_path_ceiling() {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "以太网 5", "RNDIS", "192.168.0.100", 3750);
        spec.dst = ep(Side::Agent, "以太网 18", "SGMII2.5G", "192.168.0.105", 2500);
        spec.rate_mode = RateMode::Verify;
        spec.rate_targets.forward = Some(1_800.0);
        let unit = build_single_iperf_unit(spec, PORT_BASE);
        assert_eq!(iperf_single_task(&unit).rx_target_mbps, Some(1_800.0));
    }

    /// 10GUSB(NCM) 报的 4.2G 是**已知的驱动显示问题**，那块口跑的是 10G。
    /// 封顶必须问 role 表而不是协商速率，否则 EVB 那条 6400Mbps 的已知目标
    /// 会被压成 3990，凭空制造一批 PASS。
    #[test]
    fn the_path_ceiling_does_not_trust_the_10gusb_negotiated_speed() {
        let mut spec = base_spec();
        set_evb_endpoints(&mut spec);
        spec.rate_mode = RateMode::Verify;
        let unit = build_single_iperf_unit(spec, PORT_BASE);
        let target = iperf_single_task(&unit)
            .rx_target_mbps
            .expect("EVB 有已知目标");
        assert!(
            target > 4_000.0,
            "10GUSB 的 4200Mbps 协商值不能用来封顶: {target}"
        );
    }

    #[test]
    fn tcp_and_cts_tcp_resolve_evb_targets_per_bidir_direction() {
        let mut iperf = base_spec();
        set_evb_endpoints(&mut iperf);
        iperf.directions = vec!["bidir".into()];
        let iperf_unit = build_single_iperf_unit(iperf, PORT_BASE);
        assert_eq!(iperf_unit.legs.len(), 2);
        for leg in &iperf_unit.legs {
            let LegKind::IperfSingle(task) = &leg.kind else {
                panic!("expect TCP single leg")
            };
            let expected = if leg.tag == "ab" { 6400.0 } else { 8400.0 };
            assert_eq!(task.rx_target_mbps, Some(expected), "{} target", leg.tag);
            assert_eq!(task.rate_mode, RateMode::Verify, "{} mode", leg.tag);
        }

        let mut cts = cts_spec("tcp");
        set_evb_endpoints(&mut cts);
        cts.directions = vec!["bidir".into()];
        let cts_unit = build_single_cts_unit(cts, PORT_BASE);
        assert_eq!(cts_unit.legs.len(), 2);
        for leg in &cts_unit.legs {
            let LegKind::CtsTraffic(task) = &leg.kind else {
                panic!("expect CTS TCP leg")
            };
            let expected = if leg.tag == "ab" { 6400.0 } else { 8400.0 };
            assert_eq!(task.rx_target_mbps, Some(expected), "{} target", leg.tag);
            assert_eq!(task.rate_mode, RateMode::Verify, "{} mode", leg.tag);
        }
    }

    #[test]
    fn tcp_and_cts_tcp_one_way_ba_uses_ba_target_over_forward() {
        let mut iperf = base_spec();
        iperf.directions = vec!["ba".into()];
        iperf.rate_targets.forward = Some(1111.0);
        iperf.rate_targets.ba = Some(2222.0);
        let iperf_unit = build_single_iperf_unit(iperf, PORT_BASE);
        let iperf_task = iperf_single_task(&iperf_unit);
        assert_eq!(iperf_unit.legs[0].tag, "");
        assert_eq!(iperf_task.rx_target_mbps, Some(2222.0));
        assert_eq!(iperf_task.rate_mode, RateMode::Verify);

        let mut cts = cts_spec("tcp");
        cts.directions = vec!["ba".into()];
        cts.rate_targets.forward = Some(1111.0);
        cts.rate_targets.ba = Some(2222.0);
        let cts_unit = build_single_cts_unit(cts, PORT_BASE);
        let cts_task_ref = cts_task(&cts_unit);
        assert_eq!(cts_unit.legs[0].tag, "");
        assert_eq!(cts_task_ref.rx_target_mbps, Some(2222.0));
        assert_eq!(cts_task_ref.rate_mode, RateMode::Verify);
    }

    /// PASS 条件变严时，旧 schema 的缓存 PASS 必须失效。
    ///
    /// 本版给 TCP/CTS 加了「有目标时 RX/TX 双侧采样与滚动覆盖率都要达标」的
    /// 门槛——此前这两条路径压根不采发送端网卡。一个在 v4.2.6 下拿到 PASS 的
    /// 单元，在新规则下未必还能 PASS；若 resume identity 不变，`--resume`
    /// 会直接跳过它，等于用旧语义的结论冒充新语义的验收。
    #[test]
    fn stricter_two_sided_sampling_invalidates_previous_resume_schemas() {
        let tcp = {
            let mut spec = evb_tcp_spec();
            spec.rate_mode = RateMode::Auto;
            spec
        };
        let tcp_now = build_single_iperf_id(tcp.clone(), PORT_BASE);
        let legacy_profile = format!(
            "tcp_w{}_P{}",
            tcp.tcp_windows[0],
            tcp.effective_tcp_streams()
        );
        // 直接复刻 v2 的 identity 前缀：只要 schema 串没变，其余输入相同就会撞上。
        let legacy_v2_prefix = "iperf_tcp_v2";
        assert!(
            !tcp_now.is_empty(),
            "TCP resume identity 不能为空: profile={legacy_profile}"
        );
        assert_ne!(
            tcp_now,
            md5_hex(legacy_v2_prefix),
            "TCP schema 必须已从 v2 升级"
        );

        // CTS 同理：v3 缓存不能跨双侧采样语义复用。
        let cts = cts_spec("udp");
        let cts_now = build_single_cts_id(cts.clone(), PORT_BASE);
        let mut legacy_port = PORT_BASE;
        let (legacy_units, _) = build_units(std::slice::from_ref(&cts), true, &mut legacy_port);
        let legacy_v3_id = cts_resume_unit_id_with_schema(
            "ctstraffic_v3",
            &cts,
            "V4",
            "ab",
            &legacy_units[0].legs,
        );
        assert_ne!(
            cts_now, legacy_v3_id,
            "CTS 双侧采样门槛上线后不能复用旧 ctstraffic_v3 PASS"
        );
    }

    #[test]
    fn tcp_resume_v2_ignores_port_and_invalidates_legacy_and_verdict_semantics() {
        let base = {
            let mut spec = evb_tcp_spec();
            spec.rate_mode = RateMode::Auto;
            spec
        };
        let base_id = build_single_iperf_id(base.clone(), PORT_BASE);
        let legacy_profile = format!(
            "tcp_w{}_P{}",
            base.tcp_windows[0],
            base.effective_tcp_streams()
        );
        let legacy_v1_id = md5_hex(&format!(
            "iperf_v1|V4|tcp|{}|{}|{}|{}|ab",
            legacy_profile,
            base.duration,
            ep_id(&base.src),
            ep_id(&base.dst),
        ));
        assert_ne!(
            base_id, legacy_v1_id,
            "TCP RX 目标判定上线后不能复用旧 iperf_v1 PASS"
        );
        assert_eq!(
            base_id,
            build_single_iperf_id(base.clone(), PORT_BASE + 1000),
            "临时端口变化不应破坏 TCP resume"
        );

        let assert_id_changed = |name: &str, change: fn(&mut SpecNorm)| {
            let mut changed = base.clone();
            change(&mut changed);
            assert_ne!(
                base_id,
                build_single_iperf_id(changed, PORT_BASE),
                "{name} 必须使旧 TCP PASS 失效"
            );
        };
        assert_id_changed("scenario target", |spec| {
            spec.rate_targets.ab = Some(6200.0)
        });
        assert_id_changed("configured mode", |spec| spec.rate_mode = RateMode::Verify);
        assert_id_changed("sample interval", |spec| {
            spec.rate_check.sample_interval_ms = 500
        });
        assert_id_changed("EVB target", |spec| {
            spec.rate_check.evb_usb_to_eth_target_mbps = 6300.0
        });
        assert_id_changed("TCP window", |spec| spec.tcp_windows = vec!["128k".into()]);
    }

    #[test]
    fn tests_config_maps_protocol_streams_and_builds_iperf_and_cts_independently() {
        let test: TestSpec = serde_json::from_str(
            r#"{
                "name": "split-streams",
                "src": "master:SGMII2.5G",
                "dst": "agent:SGMII2.5G",
                "kinds": ["iperf", "ctstraffic"],
                "transports": ["tcp", "udp"],
                "streams": 1,
                "tcp_streams": 4,
                "udp_streams": 2,
                "tcp_windows": ["64k"],
                "udp_profiles": [{"bandwidth": "500m"}]
            }"#,
        )
        .unwrap();
        let cfg = Config {
            limit_udp_by_link_speed: false,
            ..Config::default()
        };
        let spec = spec_from_config(
            &test,
            &cfg,
            &host("master", "m0", "SGMII2.5G", "192.168.1.2"),
            &host("agent", "a0", "SGMII2.5G", "192.168.1.3"),
        )
        .unwrap();

        assert_eq!(spec.streams, 1);
        assert_eq!(spec.tcp_streams, 4);
        assert_eq!(spec.udp_streams, 2);
        assert_eq!(spec.effective_tcp_streams(), 4);
        assert_eq!(spec.effective_udp_streams(), 2);

        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(units.len(), 4);

        let mut saw_iperf_tcp = false;
        let mut saw_iperf_udp = false;
        let mut saw_cts_tcp = false;
        let mut saw_cts_udp = false;
        for leg in units.iter().flat_map(|unit| &unit.legs) {
            match &leg.kind {
                LegKind::IperfSingle(task) if !task.udp => {
                    assert_eq!(task.extra, vec!["-w", "64k", "-P", "4"]);
                    saw_iperf_tcp = true;
                }
                LegKind::IperfGroup { streams, .. } => {
                    assert_eq!(streams.len(), 2);
                    assert!(streams.iter().all(|task| task.udp));
                    saw_iperf_udp = true;
                }
                LegKind::CtsTraffic(task) if !task.udp => {
                    assert_eq!(task.streams, 4);
                    assert_eq!(task.setup_error, None);
                    saw_cts_tcp = true;
                }
                LegKind::CtsTraffic(task) => {
                    assert_eq!(task.streams, 2);
                    assert_eq!(task.setup_error, None);
                    saw_cts_udp = true;
                }
                _ => {}
            }
        }
        assert!(saw_iperf_tcp && saw_iperf_udp && saw_cts_tcp && saw_cts_udp);
    }

    #[test]
    fn tests_config_zero_or_missing_protocol_streams_fall_back_to_legacy_streams() {
        let test: TestSpec = serde_json::from_str(
            r#"{
                "src": "master:SGMII2.5G",
                "dst": "agent:SGMII2.5G",
                "streams": 6,
                "tcp_streams": 0,
                "transports": ["tcp", "udp"]
            }"#,
        )
        .unwrap();
        let spec = spec_from_config(
            &test,
            &Config::default(),
            &host("master", "m0", "SGMII2.5G", "192.168.1.2"),
            &host("agent", "a0", "SGMII2.5G", "192.168.1.3"),
        )
        .unwrap();

        assert_eq!(test.tcp_streams, Some(0));
        assert_eq!(test.udp_streams, None);
        assert_eq!(spec.effective_tcp_streams(), 6);
        assert_eq!(spec.effective_udp_streams(), 6);
        assert_eq!(spec.stream_config_error(false), None);
        assert_eq!(spec.stream_config_error(true), None);
    }

    #[test]
    fn protocol_stream_errors_are_selected_per_transport_for_cts() {
        let mut spec = cts_spec("tcp");
        spec.transports = vec!["tcp".into(), "udp".into()];
        spec.tcp_streams = 33;
        spec.udp_streams = 2;
        spec.udp_limit = false;
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert_eq!(units.len(), 2);
        assert_eq!(
            notices
                .iter()
                .filter(|notice| notice.contains("SETUP_ERROR"))
                .count(),
            1,
            "TCP 的非法覆盖值不能污染 UDP"
        );
        let mut saw_tcp = false;
        let mut saw_udp = false;
        for leg in units.iter().flat_map(|unit| &unit.legs) {
            let LegKind::CtsTraffic(task) = &leg.kind else {
                continue;
            };
            if task.udp {
                assert_eq!(task.streams, 2);
                assert_eq!(task.setup_error, None);
                saw_udp = true;
            } else {
                assert_eq!(task.streams, 32, "执行值仍需保持在 CTS 支持范围内");
                let error = task.setup_error.as_deref().unwrap();
                assert!(error.contains("TCP streams 必须在 1..=32"));
                assert!(error.contains("当前为 33"));
                assert!(error.contains("tcp_streams"));
                saw_tcp = true;
            }
        }
        assert!(saw_tcp && saw_udp);
    }

    #[test]
    fn valid_protocol_override_ignores_invalid_legacy_streams_for_cts() {
        let mut spec = cts_spec("tcp");
        spec.streams = 0;
        spec.tcp_streams = 4;
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        let LegKind::CtsTraffic(task) = &units[0].legs[0].kind else {
            panic!("expect CTS TCP task");
        };
        assert_eq!(task.streams, 4);
        assert_eq!(task.setup_error, None);
    }

    #[test]
    fn invalid_iperf_streams_are_reported_and_normalized_for_execution() {
        let mut spec = base_spec();
        spec.tcp_streams = 33;
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert_eq!(units.len(), 1);
        assert!(notices.iter().any(|notice| {
            notice.contains("iperf TCP 流数配置非法") && notice.contains("使用 32 流")
        }));
        let LegKind::IperfSingle(task) = &units[0].legs[0].kind else {
            panic!("expect iperf TCP task");
        };
        assert_eq!(task.extra, vec!["-w", "64k", "-P", "32"]);
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
        assert_eq!(task.setup_error, None);
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
        assert_eq!(task.setup_error, None);
    }

    #[test]
    fn ctstraffic_udp_bandwidth_accepts_only_documented_complete_formats() {
        for (value, expected_mbps, expected_bps) in [
            ("500", 500.0, 500_000_000),
            ("250000k", 250.0, 250_000_000),
            ("250000Kbps", 250.0, 250_000_000),
            ("500m", 500.0, 500_000_000),
            ("500Mbps", 500.0, 500_000_000),
            ("1,5g", 1_500.0, 1_500_000_000),
            ("1.5GbPs", 1_500.0, 1_500_000_000),
            ("2.8G", 2_800.0, 2_800_000_000),
            ("2.8Gbps", 2_800.0, 2_800_000_000),
        ] {
            let parsed = cts_udp_bandwidth(&UdpProfile::bw(value)).unwrap();
            assert_eq!(parsed.mbps, expected_mbps, "value={value}");
            assert_eq!(parsed.bits_per_second, expected_bps, "value={value}");
        }

        for value in [
            "",
            "500mbps trailing",
            "500mbpsx",
            "2.8oopsGbps",
            "1e3m",
            "1mkbps",
            "1gmbps",
            "1.2,3g",
            "1.",
            "+1m",
            "0m",
        ] {
            assert!(
                cts_udp_bandwidth(&UdpProfile::bw(value)).is_err(),
                "CTS 必须拒绝非完整或超范围带宽 value={value:?}"
            );
        }
    }

    #[test]
    fn ctstraffic_udp_uses_one_strict_bandwidth_for_bps_stream_limit_and_offered_rate() {
        let mut spec = cts_spec("udp");
        spec.streams = 3;
        spec.udp_profiles = vec![UdpProfile::bw("1,5GbPs")];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        let LegKind::CtsTraffic(task) = &units[0].legs[0].kind else {
            panic!("expect CTS UDP task");
        };
        assert_eq!(task.bits_per_second, Some(1_500_000_000));
        assert_eq!(
            task.streams, 1,
            "2500 Mbps 路径只能承载一条 1500 Mbps CTS 流"
        );
        assert_eq!(task.offered_total_mbps, Some(1_500.0));
        assert_eq!(task.setup_error, None);
    }

    #[test]
    fn ctstraffic_udp_uses_rounded_bps_as_the_canonical_planning_rate() {
        let mut spec = cts_spec("udp");
        spec.streams = 3;
        spec.udp_profiles = vec![UdpProfile::bw("833.3333334m")];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        let LegKind::CtsTraffic(task) = &units[0].legs[0].kind else {
            panic!("expect CTS UDP task");
        };
        assert_eq!(task.bits_per_second, Some(833_333_333));
        assert_eq!(
            task.streams, 3,
            "2500 Mbps 路径应按真实取整后的 833333333 bps 承载三条流"
        );
        let offered = task.offered_total_mbps.unwrap();
        assert!((offered - 2_499.999_999).abs() < 1e-9);
    }

    #[test]
    fn ctstraffic_invalid_builder_parameters_create_explicit_setup_error_tasks() {
        let mut tcp = cts_spec("tcp");
        tcp.tcp_windows = vec!["not-a-size".into()];
        let mut port = PORT_BASE;
        let (tcp_units, tcp_notices) = build_units(&[tcp], true, &mut port);
        assert_eq!(tcp_units.len(), 1, "非法 CTS TCP 参数不能把任务静默跳过");
        assert!(tcp_notices
            .iter()
            .any(|notice| notice.contains("将记录 SETUP_ERROR")));
        let LegKind::CtsTraffic(tcp_task) = &tcp_units[0].legs[0].kind else {
            panic!("expect CTS TCP setup-error task");
        };
        assert!(tcp_task
            .setup_error
            .as_deref()
            .is_some_and(|error| error.contains("socket buffer")));
        assert_eq!(tcp_units[0].est_secs, 1);

        let mut udp = cts_spec("udp");
        udp.udp_profiles = vec![UdpProfile {
            bandwidth: "bad-rate".into(),
            length: Some("70000".into()),
            window: Some("0".into()),
        }];
        udp.ctstraffic_config_error = ctstraffic_common_config_error(0);
        udp.streams = 0;
        udp.duration = 1;
        let mut port = PORT_BASE;
        let (udp_units, udp_notices) = build_units(&[udp], true, &mut port);
        assert_eq!(udp_units.len(), 1, "非法 CTS UDP 参数不能把任务静默跳过");
        assert!(udp_notices
            .iter()
            .any(|notice| notice.contains("将记录 SETUP_ERROR")));
        let LegKind::CtsTraffic(udp_task) = &udp_units[0].legs[0].kind else {
            panic!("expect CTS UDP setup-error task");
        };
        let error = udp_task.setup_error.as_deref().unwrap();
        assert!(error.contains("streams 必须在 1..=32"));
        assert!(error.contains("duration 必须在 1..=86400"));
        assert!(error.contains("socket buffer"));
        assert!(error.contains("无法解析 UDP 带宽"));
        assert!(error.contains("datagram"));
        assert_eq!(udp_units[0].est_secs, 1);
    }

    #[test]
    fn ctstraffic_different_slash24_does_not_hide_global_or_profile_errors() {
        let different_subnet = ep(Side::Agent, "eth0", "SGMII2.5G", "192.168.2.3", 2500);

        let mut global_invalid = cts_spec("tcp");
        global_invalid.dst = different_subnet.clone();
        global_invalid.ctstraffic_config_error = Some("global CTS 参数非法".into());
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[global_invalid], true, &mut port);
        assert_eq!(units.len(), 1, "不同 /24 不能隐藏全局 CTS 配置错误");
        assert!(notices
            .iter()
            .any(|notice| notice.contains("将记录 SETUP_ERROR")));
        let LegKind::CtsTraffic(task) = &units[0].legs[0].kind else {
            panic!("expect global setup-error task");
        };
        assert_eq!(task.setup_error.as_deref(), Some("global CTS 参数非法"));

        let mut profile_invalid = cts_spec("udp");
        profile_invalid.dst = different_subnet.clone();
        profile_invalid.udp_profiles = vec![UdpProfile::bw("500mbps trailing")];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[profile_invalid], true, &mut port);
        assert_eq!(units.len(), 1, "不同 /24 不能隐藏 CTS profile 配置错误");
        assert!(notices
            .iter()
            .any(|notice| notice.contains("将记录 SETUP_ERROR")));
        let LegKind::CtsTraffic(task) = &units[0].legs[0].kind else {
            panic!("expect profile setup-error task");
        };
        assert!(task
            .setup_error
            .as_deref()
            .is_some_and(|error| error.contains("无法解析 UDP 带宽")));

        let mut status_invalid = cts_spec("tcp");
        status_invalid.dst = different_subnet.clone();
        status_invalid.ctstraffic.status_update_ms = 0;
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[status_invalid], true, &mut port);
        assert_eq!(units.len(), 1, "不同 /24 不能隐藏 status_update_ms 错误");
        assert!(notices
            .iter()
            .any(|notice| notice.contains("将记录 SETUP_ERROR")));
        let LegKind::CtsTraffic(task) = &units[0].legs[0].kind else {
            panic!("expect status setup-error task");
        };
        assert!(task
            .setup_error
            .as_deref()
            .is_some_and(|error| error.contains("status_update_ms")));

        let mut udp_tuning_invalid = cts_spec("udp");
        udp_tuning_invalid.dst = different_subnet;
        udp_tuning_invalid.ctstraffic.udp_frame_rate = 0;
        udp_tuning_invalid.ctstraffic.udp_buffer_depth_secs = 0;
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[udp_tuning_invalid], true, &mut port);
        assert_eq!(units.len(), 1, "不同 /24 不能隐藏 UDP CTS 调优参数错误");
        assert!(notices
            .iter()
            .any(|notice| notice.contains("将记录 SETUP_ERROR")));
        let LegKind::CtsTraffic(task) = &units[0].legs[0].kind else {
            panic!("expect UDP tuning setup-error task");
        };
        let error = task.setup_error.as_deref().unwrap();
        assert!(error.contains("udp_frame_rate"));
        assert!(error.contains("udp_buffer_depth_secs"));
    }

    #[test]
    fn ctstraffic_different_slash24_still_skips_valid_tasks() {
        let mut spec = cts_spec("udp");
        spec.dst = ep(Side::Agent, "eth0", "SGMII2.5G", "192.168.2.3", 2500);
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert!(units.is_empty());
        assert_eq!(port, PORT_BASE, "拓扑跳过前不应分配 CTS 端口");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("两端 IPv4 不同 /24"));
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
        let mut legacy_port = PORT_BASE;
        let (legacy_units, legacy_notices) =
            build_units(std::slice::from_ref(&base), true, &mut legacy_port);
        assert!(legacy_notices.is_empty());
        let legacy_v2_id = cts_resume_unit_id_with_schema(
            "ctstraffic_v2",
            &base,
            "V4",
            "ab",
            &legacy_units[0].legs,
        );
        assert_ne!(
            base_id, legacy_v2_id,
            "CTS P10/rolling coverage 判定上线后必须让 v2 PASS 无条件失效"
        );
        let legacy_v1_id = cts_resume_unit_id_with_schema(
            "ctstraffic_v1",
            &base,
            "V4",
            "ab",
            &legacy_units[0].legs,
        );
        assert_ne!(
            base_id, legacy_v1_id,
            "CTS 统计窗口语义变化后必须让 v1 PASS 无条件失效"
        );
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
                LegKind::IperfGroup { streams, .. } => {
                    assert_eq!(streams.len(), 3);
                    for stream in streams {
                        assert_eq!(stream.extra, vec!["-b", "500000000"]);
                        assert!(
                            !stream.extra.iter().any(|arg| arg == "-P"),
                            "UDP 并发通过独立 client 实现，单个 client 不得使用 -P"
                        );
                    }
                }
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
        assert_eq!(task.extra, vec!["-b", "1000000000", "-l", "64", "-w", "4m"]);
        assert_eq!(task.profile_name, "udp_b1000m_l64_w4m");
        assert_eq!(task.profile_label, "UDP -b 1000m -l 64 -w 4m");
    }

    #[test]
    fn udp_length_14k_maps_to_iperf_and_cts_without_unit_drift() {
        let mut spec = base_spec();
        spec.kinds = vec!["iperf".into(), "ctstraffic".into()];
        spec.transports = vec!["udp".into()];
        spec.udp_limit = false;
        spec.udp_profiles = vec![UdpProfile {
            bandwidth: "500m".into(),
            length: Some("14k".into()),
            window: None,
        }];

        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(units.len(), 2);

        let iperf = units
            .iter()
            .flat_map(|unit| &unit.legs)
            .find_map(|leg| match &leg.kind {
                LegKind::IperfSingle(task) => Some(task),
                _ => None,
            })
            .expect("iperf UDP task");
        assert_eq!(iperf.extra, vec!["-b", "500000000", "-l", "14k"]);

        let cts = units
            .iter()
            .flat_map(|unit| &unit.legs)
            .find_map(|leg| match &leg.kind {
                LegKind::CtsTraffic(task) => Some(task),
                _ => None,
            })
            .expect("CTS UDP task");
        assert_eq!(cts.datagram_bytes, Some(14 * 1024));
        assert_eq!(cts.setup_error, None);
    }

    #[test]
    fn iperf_udp_canonicalizes_gigabit_suffixes_to_exact_bps() {
        for configured in ["2.8G", "2.8Gbps"] {
            let mut spec = base_spec();
            spec.transports = vec!["udp".into()];
            spec.udp_limit = false;
            spec.udp_profiles = vec![UdpProfile::bw(configured)];

            let mut port = PORT_BASE;
            let (units, notices) = build_units(&[spec], true, &mut port);
            assert!(notices.is_empty());
            let LegKind::IperfSingle(task) = &units[0].legs[0].kind else {
                panic!("expect single UDP task");
            };
            assert_eq!(task.extra, vec!["-b", "2800000000"]);
            assert_eq!(task.offered_per_stream_mbps, Some(2800.0));
            assert!(task.profile_name.contains(configured));
            assert!(task.profile_label.contains(configured));
        }
    }

    #[test]
    fn invalid_iperf_udp_bandwidth_skips_profile_before_execution() {
        for invalid in ["2.8oopsGbps", "2.8Gjunk"] {
            let mut spec = base_spec();
            spec.transports = vec!["udp".into()];
            spec.streams = 4;
            spec.udp_profiles = vec![UdpProfile::bw(invalid)];

            let mut port = PORT_BASE;
            let (units, notices) = build_units(&[spec], true, &mut port);
            assert!(units.is_empty(), "非法带宽不能生成 iperf 任务");
            assert_eq!(port, PORT_BASE, "跳过 profile 不应消耗端口");
            assert!(notices.iter().any(|notice| {
                notice.contains("跳过")
                    && notice.contains("iperf UDP profile")
                    && notice.contains(invalid)
                    && notice.contains("带宽格式非法")
                    && notice.contains("未生成任务")
            }));
        }
    }

    /// 造一个跨机 iperf 单元，用来验证端点刷新。
    fn refreshable_unit() -> Unit {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "以太网 6", "SGMII2.5G", "192.168.0.101", 2500);
        spec.dst = ep(Side::Agent, "WLAN 3", "WIFI5G", "192.168.0.104", 2882);
        spec.transports = vec!["tcp".into()];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        units.into_iter().next().expect("应生成一个单元")
    }

    fn host_with(hostname: &str, nics: Vec<NicInfo>) -> HostInfo {
        HostInfo {
            hostname: hostname.into(),
            os: "test".into(),
            interfaces: nics,
        }
    }

    /// WiFi 在一轮 7 小时的测试里会重新协商，DHCP 会换租约。用开跑那一刻的
    /// 快照跑完全程，后面几十个单元的基准从中途起就是错的，而报告里印的也是
    /// 那份旧快照，错误完全不可见。
    #[test]
    fn refreshing_endpoints_reports_and_applies_what_changed() {
        let mut unit = refreshable_unit();
        let master = host_with(
            "master",
            vec![nic("以太网 6", "SGMII2.5G", "192.168.0.101", 2500)],
        );
        // 辅测 WiFi 换了 IP、重新协商到一半速率、接口索引也变了。
        let mut moved = nic("WLAN 3", "WIFI5G", "192.168.0.150", 1441);
        moved.ifindex = 27;
        let agent = host_with("agent", vec![moved]);

        let drifts = refresh_unit_endpoints(&mut unit, &master, &agent);
        assert_eq!(drifts.len(), 1, "同一块网卡只报一次: {drifts:?}");
        let detail = drifts[0].describe();
        assert!(detail.contains("192.168.0.104 → 192.168.0.150"), "{detail}");
        assert!(detail.contains("2882 → 1441Mbps"), "{detail}");
        assert!(detail.contains("接口索引"), "{detail}");
        assert!(!drifts[0].is_gone());

        // 变更必须真的落到任务上，否则 iperf 会继续连旧地址。
        let task = match &unit.legs[0].kind {
            LegKind::IperfSingle(task) => task,
            LegKind::IperfGroup { streams, .. } => &streams[0],
            _ => panic!("expect iperf leg"),
        };
        assert_eq!(task.dst.nic.ipv4, "192.168.0.150");
        assert_eq!(task.dst.nic.speed_mbps, 1441);
        assert_eq!(task.dst.nic.ifindex, 27);
        assert_eq!(task.src.nic.ipv4, "192.168.0.101", "没变的那端不该被动");
    }

    /// 网卡整块消失时必须报出来：对着不存在的接口起 monitor，
    /// 采到的要么是别的网卡，要么静默全零——两种都比直接判死更糟。
    #[test]
    fn a_vanished_nic_is_reported_as_gone() {
        let mut unit = refreshable_unit();
        let master = host_with(
            "master",
            vec![nic("以太网 6", "SGMII2.5G", "192.168.0.101", 2500)],
        );
        let agent = host_with(
            "agent",
            vec![nic("以太网", "SGMII1G", "192.168.0.102", 1000)],
        );

        let drifts = refresh_unit_endpoints(&mut unit, &master, &agent);
        assert_eq!(drifts.len(), 1);
        assert!(drifts[0].is_gone(), "{drifts:?}");
        assert!(drifts[0].describe().contains("WLAN 3"));
    }

    #[test]
    fn an_unchanged_topology_produces_no_noise() {
        let mut unit = refreshable_unit();
        let master = host_with(
            "master",
            vec![nic("以太网 6", "SGMII2.5G", "192.168.0.101", 2500)],
        );
        let agent = host_with(
            "agent",
            vec![nic("WLAN 3", "WIFI5G", "192.168.0.104", 2882)],
        );
        assert!(refresh_unit_endpoints(&mut unit, &master, &agent).is_empty());
    }

    /// 从生成的单元里取出第一条 iperf 流实际下发的 `-b` 值（bit/s 字符串）。
    fn first_bandwidth_arg(unit: &Unit) -> String {
        let task = match &unit.legs[0].kind {
            LegKind::IperfSingle(task) => task,
            LegKind::IperfGroup { streams, .. } => &streams[0],
            _ => panic!("expect iperf leg"),
        };
        let pos = task
            .extra
            .iter()
            .position(|arg| arg == "-b")
            .expect("UDP 任务必须带 -b");
        task.extra[pos + 1].clone()
    }

    /// 单流带宽超过路径上限时，压 `-b` 而不是把任务整个跳过。
    /// 旧行为会让「1G 收端 + -b 2.5G」这类组合完全没有测量结果；
    /// 而实际发生的是它根本没触发，80 条命令全用了同一个超限的 -b。
    #[test]
    fn test_udp_over_path_ceiling_clips_bandwidth_instead_of_skipping() {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "eth1", "SGMII1G", "192.168.1.2", 1000);
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("2500m")];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert_eq!(units.len(), 1, "不能因为超限就没有任务");
        assert_eq!(
            first_bandwidth_arg(&units[0]),
            "1000000000",
            "-b 必须压到 min(发送口, 接收口) = 1000Mbps"
        );
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("裁剪") && notices[0].contains("1000"),
            "裁剪必须在提示里说明: {}",
            notices[0]
        );
        // 标题印的是实际下发的值，不是被裁之前的档位——任务清单是很多人
        // 唯一会看的地方，那里写 2500m 会让人以为真的在灌 2.5G。
        assert!(
            units[0].title.contains("-b 1000m"),
            "标题要反映实际下发的 -b: {}",
            units[0].title
        );
    }

    /// WiFi 的负载上限**不跟协商速率**。
    ///
    /// 协商值是 PHY 速率，同一块 Wi-Fi 7 网卡会在一轮测试里于 2402 / 2882
    /// 之间来回跳；跟着它裁 -b，相邻两个单元的灌包强度都不一样，结果没法
    /// 横向比较。实践中 WiFi 一律按固定档灌（协商到 2.4G 还是 2.8G 都用
    /// -b 2.6G），所以这里用 wifi_payload_ceiling_mbps 而不是 866。
    #[test]
    fn wifi_ceiling_ignores_the_fluctuating_negotiated_rate() {
        let mut spec = base_spec();
        let mut e = ep(Side::Master, "wlan", "WIFI5G", "192.168.1.5", 866);
        e.nic.is_wifi = true;
        spec.src = e;
        spec.dst = ep(Side::Agent, "wlan3", "WIFI5G", "192.168.1.6", 2402);
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("2.6G")];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert_eq!(units.len(), 1);
        assert_eq!(
            first_bandwidth_arg(&units[0]),
            "2600000000",
            "2.6G 在默认 2882 的 WiFi 上限内，不该被协商到的 866/2402 裁掉"
        );
        assert!(notices.is_empty(), "{notices:?}");

        // 把上限调低才裁——这条依然由配置说了算。
        let mut strict = base_spec();
        strict.src = ep(Side::Master, "wlan", "WIFI5G", "192.168.1.5", 866);
        strict.dst = ep(Side::Agent, "wlan3", "WIFI5G", "192.168.1.6", 2402);
        strict.transports = vec!["udp".into()];
        strict.udp_profiles = vec![UdpProfile::bw("2.6G")];
        strict.rate_check.wifi_payload_ceiling_mbps = 1000.0;
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[strict], true, &mut port);
        assert_eq!(first_bandwidth_arg(&units[0]), "1000000000");
    }

    /// 端到端串一遍两层策略：单口覆盖改写 -b 和门限，最后仍要过路径裁剪。
    #[test]
    fn link_profiles_drive_both_bandwidth_and_target_end_to_end() {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "以太网 6", "SGMII2.5G", "192.168.0.101", 2500);
        spec.dst = ep(Side::Agent, "WLAN 3", "WIFI5G", "192.168.0.104", 2882);
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("2500m")];
        spec.link_profiles = LinkProfiles {
            by_role: vec![RoleProfile {
                pair: "SGMII2.5G<->WIFI5G".into(),
                rx_target_mbps: RateTargets {
                    ab: Some(1600.0),
                    ..Default::default()
                },
                udp_bandwidth: DirectionalBandwidth {
                    ab: Some("2.6G".into()),
                    ..Default::default()
                },
            }],
            by_nic: vec![NicProfile {
                host: "agent".into(),
                name: "WLAN 3".into(),
                ipv4: "192.168.0.104".into(),
                rx_target_mbps: Some(1800.0),
                udp_bandwidth: None,
                ..Default::default()
            }],
        };
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let task = match &units[0].legs[0].kind {
            LegKind::IperfSingle(task) => task,
            LegKind::IperfGroup { streams, .. } => &streams[0],
            _ => panic!("expect iperf leg"),
        };

        // 带宽：角色层的 2.6G 覆盖全局的 2500m，并且**不再被自动裁剪**——
        // 在 link_profiles 里专门为这条链路写下的值是明确判断，
        // 安全网不该推翻它。
        let pos = task.extra.iter().position(|arg| arg == "-b").unwrap();
        assert_eq!(task.extra[pos + 1], "2600000000");
        assert!(
            task.profile_label.contains("链路策略至 2600M"),
            "{}",
            task.profile_label
        );

        // 门限：单口覆盖压过角色层。
        assert_eq!(task.rx_target_mbps, Some(1800.0));
        assert_eq!(task.rate_mode, RateMode::Verify, "有目标就该进 verify");
    }

    /// 关掉 limit_udp_by_link_speed 时不得擅自改写用户填的 -b。
    #[test]
    fn test_udp_bandwidth_is_untouched_when_limit_is_off() {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "eth1", "SGMII1G", "192.168.1.2", 1000);
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("2500m")];
        spec.udp_limit = false;
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert_eq!(units.len(), 1);
        assert_eq!(first_bandwidth_arg(&units[0]), "2500000000");
        assert!(notices.is_empty());
    }

    /// 双向单元的两条腿各按自己的路径上限裁剪：同一条链路两个方向的
    /// 能力可以差很多，共用一个 -b 没有物理依据。
    #[test]
    fn bidirectional_udp_clips_each_leg_against_its_own_path_ceiling() {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "eth0", "SGMII2.5G", "192.168.1.2", 2500);
        spec.dst = ep(Side::Agent, "eth1", "SGMII1G", "192.168.1.3", 1000);
        spec.directions = vec!["bidir".into()];
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("2500m")];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].legs.len(), 2);
        // 两条腿都受 1G 那一端约束，都要被压到 1000Mbps。
        for leg in &units[0].legs {
            let task = match &leg.kind {
                LegKind::IperfSingle(task) => task,
                LegKind::IperfGroup { streams, .. } => &streams[0],
                _ => panic!("expect iperf leg"),
            };
            let pos = task.extra.iter().position(|arg| arg == "-b").unwrap();
            assert_eq!(task.extra[pos + 1], "1000000000", "腿 {} 未裁剪", leg.tag);
            assert_eq!(
                task.offered_per_stream_mbps,
                Some(1000.0),
                "offered 必须跟着实际 -b 走"
            );
            assert!(
                task.profile_label.contains("裁剪"),
                "报表标签要说明裁剪: {}",
                task.profile_label
            );
        }
    }

    /// `-w 256m -P 10` = 2.56GB 发送缓冲，等于 1G 链路 20 秒的流量。
    /// 这些字节会被算进「工具自报发送」，让「发−收」出现约 119Mbps 的恒定
    /// 虚高——这不是链路特性，是参数造出来的。
    #[test]
    fn an_oversized_socket_buffer_is_flagged_but_not_rewritten() {
        let mut spec = base_spec();
        spec.dst = ep(Side::Agent, "eth1", "SGMII1G", "192.168.1.3", 1000);
        spec.transports = vec!["tcp".into()];
        spec.tcp_streams = 10;
        spec.tcp_windows = vec!["256m".into()];
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);

        assert_eq!(units.len(), 1, "只提示，不能把任务砍掉");
        let task = match &units[0].legs[0].kind {
            LegKind::IperfSingle(task) => task,
            LegKind::IperfGroup { streams, .. } => &streams[0],
            _ => panic!("expect iperf leg"),
        };
        let pos = task.extra.iter().position(|arg| arg == "-w").unwrap();
        assert_eq!(
            task.extra[pos + 1],
            "256m",
            "-w 是用户明确填的参数，工具不该背着人改测试条件"
        );

        let notice = notices
            .iter()
            .find(|n| n.contains("socket 缓冲"))
            .unwrap_or_else(|| panic!("应提示缓冲过大: {notices:?}"));
        assert!(
            notice.contains("2.68GB") || notice.contains("2.6"),
            "{notice}"
        );
        assert!(
            notice.contains("119") || notice.contains("虚高"),
            "{notice}"
        );
    }

    /// 常规档位不该产生噪音提示。
    #[test]
    fn a_normal_socket_buffer_is_silent() {
        let mut spec = base_spec();
        spec.dst = ep(Side::Agent, "eth1", "SGMII1G", "192.168.1.3", 1000);
        spec.transports = vec!["tcp".into()];
        spec.tcp_streams = 10;
        spec.tcp_windows = vec!["4m".into()];
        let mut port = PORT_BASE;
        let (_, notices) = build_units(&[spec], true, &mut port);
        assert!(
            !notices.iter().any(|n| n.contains("socket 缓冲")),
            "{notices:?}"
        );
    }

    /// 档位里没有 `-l` / `-w` 时，命令里就不该出现它们。
    ///
    /// 「不指定」和「指定成 iperf3 的默认值」在报告里读起来是两件事：前者说明
    /// 这一轮没碰报文长度，后者是一个具体的测试条件。替人填一个默认值，等于
    /// 把没做过的选择写成做过。
    #[test]
    fn a_profile_without_length_or_window_emits_no_such_flags() {
        let mut spec = base_spec();
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("500m")];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let extra = udp_extra(&units[0].legs[0].kind);
        assert!(extra.contains(&"-b".to_string()), "{extra:?}");
        assert!(!extra.contains(&"-l".to_string()), "{extra:?}");
        assert!(!extra.contains(&"-w".to_string()), "{extra:?}");

        // 填了就要原样出现，别在「不下发」的实现里把「下发」一起弄丢。
        let mut spec = base_spec();
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile {
            bandwidth: "500m".into(),
            length: Some("1200".into()),
            window: Some("1m".into()),
        }];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let extra = udp_extra(&units[0].legs[0].kind);
        assert!(extra.windows(2).any(|w| w == ["-l", "1200"]), "{extra:?}");
        assert!(extra.windows(2).any(|w| w == ["-w", "1m"]), "{extra:?}");
    }

    /// 单流走 `IperfSingle`、多流走 `IperfGroup`，取参数时别只认其中一种。
    fn udp_extra(kind: &LegKind) -> Vec<String> {
        match kind {
            LegKind::IperfSingle(task) => task.extra.clone(),
            LegKind::IperfGroup { streams, .. } => streams[0].extra.clone(),
            other => panic!("expect an iperf leg, got {other:?}"),
        }
    }

    /// RNDIS 按它自己报的协商速率裁，不再压到 CPE 子网那一档。
    ///
    /// 3700 / 500 = 7.4 -> 7 条流。压到 2500 会得到 5 条——那是把一块能跑
    /// 3.7G 的口当成 2.5G 用，灌包强度凭空少三分之一。
    #[test]
    fn rndis_is_clipped_by_its_own_negotiated_rate() {
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
            LegKind::IperfGroup { streams, .. } => assert_eq!(streams.len(), 7),
            _ => panic!("expect group"),
        }
    }

    /// 10GUSB(NCM) 报的 4.2G 是驱动显示问题，仍按 10G 裁——它和 RNDIS
    /// 走的是两条规则，别在重构里被合并成一条。
    #[test]
    fn ncm_keeps_the_ten_gig_ceiling_despite_its_bogus_negotiated_rate() {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "usb", "10GUSB", "192.168.1.2", 4200);
        spec.dst = ep(Side::Agent, "10g", "10GETH", "192.168.1.3", 10000);
        spec.transports = vec!["udp".into()];
        spec.streams = 12;
        spec.udp_profiles = vec![UdpProfile::bw("1000m")];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        match &units[0].legs[0].kind {
            LegKind::IperfGroup { streams, .. } => {
                assert_eq!(streams.len(), 10, "按 4200 裁会只剩 4 条流")
            }
            _ => panic!("expect group"),
        }
    }

    /// PING 单元的预计耗时按「每秒一个包 + 收尾等待」算，且必须随 count 走。
    ///
    /// 这条钉的是一个实测出来的缺口：旧公式 `count + 5` 在最后一个包丢了的时候
    /// 稳定少算 5 秒（实测 count=5/20/40 分别是 15.0/30.1/50.2 秒，正好 count+10）。
    /// ping 次数默认从 100 提到 180 之后，估算漏的绝对值也跟着放大。
    #[test]
    fn the_ping_estimate_covers_the_trailing_wait_and_scales_with_the_count() {
        // 实测形态：count + 10 是下限，估算不能比它还小。
        for count in [3_u32, 5, 20, 40, 100, 180] {
            let measured_floor = count as u64 + 10;
            assert!(
                ping_estimated_secs(count) >= measured_floor,
                "count={count} 的估算 {} 低于实测的 {measured_floor} 秒",
                ping_estimated_secs(count)
            );
        }
        // 但也不能离谱地虚高：包正常回来时实际约 count - 1 秒。
        assert!(ping_estimated_secs(180) < 180 + 30);

        // 单元里真的用上了它，而且随 ping_count 变化。
        let unit_est = |count: u32| {
            let mut spec = base_spec();
            spec.kinds = vec!["ping".into()];
            spec.transports = vec![];
            spec.directions = vec!["ab".into()];
            spec.ping_count = count;
            spec.payload_sizes = vec![32];
            let mut port = PORT_BASE;
            let (units, _) = build_units(&[spec], true, &mut port);
            assert_eq!(units.len(), 1, "count={count} 应当只有一个 PING 单元");
            units[0].est_secs
        };
        assert_eq!(unit_est(180), ping_estimated_secs(180));
        assert!(
            unit_est(180) > unit_est(100),
            "ping 次数翻倍，预计耗时必须跟着涨"
        );
    }

    #[test]
    fn single_udp_estimate_matches_one_attempt_and_bidir_is_parallel() {
        // 预计总耗时按典型成功路径估算：单流 UDP 第一次尝试通常就能测出速率，
        // 不再按最坏 3 次尝试累加（旧行为会把 10s 项估成 368s）。
        let mut oneway = base_spec();
        oneway.transports = vec!["udp".into()];
        oneway.streams = 1;
        let mut port = PORT_BASE;
        let (oneway_units, notices) = build_units(&[oneway.clone()], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(oneway_units.len(), 1);
        let oneway_estimate = oneway_units[0].est_secs;
        assert_eq!(oneway_estimate, 38);

        oneway.directions = vec!["bidir".into()];
        let mut port = PORT_BASE;
        let (bidir_units, notices) = build_units(&[oneway], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(bidir_units.len(), 1);
        assert_eq!(bidir_units[0].legs.len(), 2);
        assert_eq!(
            bidir_units[0].est_secs, oneway_estimate,
            "AB/BA 双腿并行，估算不得按两条腿重复累计"
        );
    }

    #[test]
    fn single_udp_estimate_ignores_retry_budget_since_retries_are_failure_path() {
        // 重试只在当次尝试无有效测量时发生，是异常路径；预计总耗时按一次尝试估算，
        // flow_retries 配置不应把开始前的规划时间放大到 698s。
        let mut spec = base_spec();
        spec.transports = vec!["udp".into()];
        spec.streams = 1;
        spec.rate_check.flow_retries = 4;
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].est_secs, 38);
    }

    #[test]
    fn ctstraffic_single_udp_estimate_matches_one_attempt_and_bidir_is_parallel() {
        let mut spec = cts_spec("udp");
        spec.streams = 1;
        let mut port = PORT_BASE;
        let (oneway_units, notices) = build_units(&[spec.clone()], true, &mut port);
        assert!(notices.is_empty());
        assert_eq!(oneway_units.len(), 1);
        assert_eq!(oneway_units[0].est_secs, 25);

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

    /// 钉住 `push_rate_check_identity` 里那条「两个 WiFi 上限有意不记」的取舍：
    /// 上限真正改变了下发的负载时，identity 必须跟着变（经由裁剪后的 `-b`）；
    /// 裁剪关掉、上限对执行毫无影响时，identity 不该平白变化。
    /// 双向门限按**配对**配置，且要一路走到下发的 task 上。
    ///
    /// 按网卡配是不够的：同一块 RNDIS 口，和 Wi-Fi 组双向、和 SGMII 组双向，
    /// 能收到的速率完全不是一个量级——挂在网卡上的那一个数没法同时对两组成立。
    /// 这条从 `build_units` 走完整链路，中间隔着 `leg_rx_target()` 和四个调用点。
    #[test]
    fn a_bidirectional_unit_uses_the_per_pair_threshold_and_a_one_way_unit_does_not() {
        let targets = |direction: &str| -> Vec<Option<f64>> {
            let mut spec = base_spec();
            spec.directions = vec![direction.into()];
            spec.rate_targets = RateTargets {
                forward: Some(2000.0),
                ..Default::default()
            };
            spec.rate_targets_bidir = RateTargets {
                forward: None,
                ab: Some(1000.0),
                ba: Some(800.0),
            };
            let mut port = PORT_BASE;
            let (units, _) = build_units(&[spec], true, &mut port);
            units
                .iter()
                .flat_map(|unit| unit.legs.iter())
                .filter_map(|leg| match &leg.kind {
                    LegKind::IperfSingle(task) => Some(task.rx_target_mbps),
                    _ => None,
                })
                .collect()
        };

        assert_eq!(targets("ab"), vec![Some(2000.0)], "单向仍按单向门限判");
        assert_eq!(
            targets("bidir"),
            vec![Some(1000.0), Some(800.0)],
            "双向两条腿各取各的方向门限——双向并发时两个方向本来就能差很远"
        );
    }

    /// 双向门限没填的方向要回落，不能变成「没有目标」。
    #[test]
    fn a_direction_without_a_bidirectional_threshold_falls_back_to_the_normal_chain() {
        let mut spec = base_spec();
        spec.directions = vec!["bidir".into()];
        spec.rate_targets = RateTargets {
            forward: Some(2000.0),
            ..Default::default()
        };
        // 只配 ab，ba 留空。
        spec.rate_targets_bidir = RateTargets {
            forward: None,
            ab: Some(900.0),
            ba: None,
        };
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let targets: Vec<Option<f64>> = units
            .iter()
            .flat_map(|unit| unit.legs.iter())
            .filter_map(|leg| match &leg.kind {
                LegKind::IperfSingle(task) => Some(task.rx_target_mbps),
                _ => None,
            })
            .collect();
        assert_eq!(
            targets,
            vec![Some(900.0), Some(2000.0)],
            "没配双向门限的那个方向要回到既有兜底链，而不是丢掉目标"
        );
    }

    /// 配了「双向 RX 合计」门限时，两条腿**没有自己的门限**，也不许因此变成
    /// `TARGET_MISSING`。
    ///
    /// 判定在单元级只做一次合计比对（`executor::bidir_total_verdict`）。给腿
    /// 留一个每方向门限，报告上会出现「AB 判 RATE_FAIL、单元判 PASS」这种自相
    /// 矛盾的两行；只清门限不改模式，显式配 `verify` 的用户会拿到一整轮
    /// `NOT_EVALUATED / TARGET_MISSING`——腿本来就不该有目标，这不是缺配置。
    #[test]
    fn a_bidirectional_total_threshold_turns_both_legs_into_pure_measurement() {
        let mut spec = base_spec();
        spec.directions = vec!["bidir".into()];
        spec.rate_mode = RateMode::Verify;
        // 每方向门限和全局门限都在，但合计门限必须压过它们。
        spec.rate_targets_bidir = RateTargets {
            forward: None,
            ab: Some(1_000.0),
            ba: Some(800.0),
        };
        spec.rate_targets = RateTargets {
            forward: Some(2_000.0),
            ab: None,
            ba: None,
        };
        spec.rate_target_bidir_total = Some(1_500.0);

        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let unit = units.first().expect("双向单元");
        assert_eq!(unit.bidir_total_target_mbps, Some(1_500.0));
        for leg in &unit.legs {
            match &leg.kind {
                LegKind::IperfSingle(task) => {
                    assert_eq!(task.rx_target_mbps, None, "{} 腿不该有自己的门限", leg.tag);
                    assert_eq!(
                        task.rate_mode,
                        RateMode::Observe,
                        "{} 腿必须落到 Observe，否则 verify 会判 TARGET_MISSING",
                        leg.tag
                    );
                }
                other => panic!("预期 iperf 单流腿，实得 {other:?}"),
            }
        }
    }

    /// 合计门限**必须**进 resume identity。
    ///
    /// 腿的 `rx_target_mbps` 现在是 `None`，那条既有的「门限变了 identity 就变」
    /// 的通路在这里断了：不显式记的话，把合计从 900 改成 1200 之后 resume 会拿
    /// 按 900 判过的 PASS 顶掉这一轮。
    #[test]
    fn changing_the_bidirectional_total_threshold_invalidates_the_resume_identity() {
        let id_with = |total: Option<f64>| {
            let mut spec = base_spec();
            spec.directions = vec!["bidir".into()];
            spec.rate_target_bidir_total = total;
            let mut port = PORT_BASE;
            let (units, _) = build_units(&[spec], true, &mut port);
            units[0].id.clone()
        };
        assert_ne!(id_with(Some(900.0)), id_with(Some(1_200.0)));
        assert_ne!(id_with(Some(900.0)), id_with(None));
        assert_eq!(id_with(None), id_with(None), "没配时 identity 要稳定");

        // 单向单元不受影响：合计门限对它没有意义，identity 一个字节都不该变。
        let single = |total: Option<f64>| {
            let mut spec = base_spec();
            spec.directions = vec!["ab".into()];
            spec.rate_target_bidir_total = total;
            let mut port = PORT_BASE;
            let (units, _) = build_units(&[spec], true, &mut port);
            units[0].id.clone()
        };
        assert_eq!(single(None), single(Some(900.0)));
    }

    /// 门限变了旧 PASS 就得失效——否则开 resume 会拿按 2000 判过的结果
    /// 去顶一个现在按 1000 判的单元。
    ///
    /// 不需要把 `rate_targets_bidir` 单独塞进 identity：解析出来的
    /// `task.rx_target_mbps` 本来就在 identity 里（见 `push_resume_field`
    /// 对 `rx_target_mbps` 的处理），而那正是这个配置唯一影响执行的通路。
    /// 再记一遍只会让所有人的 resume 缓存白白清空一次。
    #[test]
    fn changing_the_bidirectional_threshold_invalidates_the_resume_identity() {
        let id_with = |ab: Option<f64>| {
            let mut spec = base_spec();
            spec.directions = vec!["bidir".into()];
            spec.rate_targets_bidir = RateTargets {
                forward: None,
                ab,
                ba: Some(800.0),
            };
            let mut port = PORT_BASE;
            let (units, _) = build_units(&[spec], true, &mut port);
            units[0].id.clone()
        };

        assert_ne!(id_with(Some(1000.0)), id_with(Some(1200.0)));
        assert_eq!(id_with(None), id_with(None), "没配时 identity 要稳定");
    }

    #[test]
    fn the_24g_ceiling_reaches_resume_identity_through_the_clipped_load() {
        // 不用 build_single_udp_id：裁剪会产生提示行，那个辅助函数要求提示为空。
        let udp_id = |spec: SpecNorm| {
            let mut port = PORT_BASE;
            let (units, _) = build_units(&[spec], true, &mut port);
            assert_eq!(units.len(), 1);
            units[0].id.clone()
        };

        let mut base = base_spec();
        base.src = ep(Side::Master, "wlan", "WIFI2.4G", "192.168.1.2", 286);
        base.dst = ep(Side::Agent, "eth0", "10GETH", "192.168.1.3", 10000);
        base.transports = vec!["udp".into()];
        base.udp_profiles = vec![UdpProfile::bw("1000m")];
        base.udp_limit = true;
        let base_id = udp_id(base.clone());

        let mut raised = base.clone();
        raised.rate_check.wifi_24g_payload_ceiling_mbps = 900.0;
        assert_ne!(
            base_id,
            udp_id(raised),
            "2.4G 上限改变了实际下发的 -b，旧 PASS 必须失效"
        );

        let mut unlimited = base.clone();
        unlimited.udp_limit = false;
        let unlimited_id = udp_id(unlimited.clone());
        unlimited.rate_check.wifi_24g_payload_ceiling_mbps = 900.0;
        assert_eq!(
            unlimited_id,
            udp_id(unlimited),
            "没开裁剪时上限不参与任何计算，不该让缓存无谓失效"
        );
    }

    #[test]
    fn udp_resume_id_is_independent_of_tcp_stream_configuration() {
        let mut base = evb_udp_spec();
        base.streams = 20;
        base.tcp_streams = 20;
        base.udp_streams = 4;
        let base_id = build_single_udp_id(base.clone(), PORT_BASE);

        let mut tcp_changed = base.clone();
        tcp_changed.tcp_streams = 7;
        // 模拟交互路径中 legacy streams 曾取两种协议的最大值。
        tcp_changed.streams = tcp_changed.tcp_streams.max(tcp_changed.udp_streams);
        assert_eq!(
            base_id,
            build_single_udp_id(tcp_changed, PORT_BASE),
            "只改变 TCP 流数不能让未变化的 UDP PASS 缓存失效"
        );

        let mut udp_changed = base;
        udp_changed.udp_streams = 3;
        assert_ne!(
            base_id,
            build_single_udp_id(udp_changed, PORT_BASE),
            "UDP 请求流数变化必须进入 resume identity"
        );
    }

    #[test]
    fn test_udp_resume_v4_ignores_runtime_port_but_tracks_verdict_semantics() {
        let base = evb_udp_spec();
        let base_id = build_single_udp_id(base.clone(), PORT_BASE);
        let mut legacy_port = PORT_BASE;
        let (legacy_units, legacy_notices) =
            build_units(std::slice::from_ref(&base), true, &mut legacy_port);
        assert!(legacy_notices.is_empty());
        let legacy_v3_id = udp_resume_unit_id_with_schema(
            "iperf_v3",
            true,
            &base,
            "V4",
            "ab",
            &base.udp_profiles[0],
            &legacy_units[0].legs,
        );
        assert_ne!(
            base_id, legacy_v3_id,
            "Started 基线语义上线后，v4 必须让 v3 PASS 无条件失效"
        );
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
            "v4 必须让 v2 schema 下缓存的 PASS 无条件失效"
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
            "v4 必须让 v1 schema 下缓存的 PASS 无条件失效"
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
    fn test_udp_resume_v4_tracks_effective_leg_shape() {
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

    /// 诊断单元要能说出自己在替**哪条链路**做体检。
    ///
    /// 它们过去一律 `link_group: ""`，于是 Excel 的「按链路分组」把全部诊断挤进
    /// 「(未分组)」一行——链路组一多就分不出这条诊断说的是哪条链路，而诊断存在
    /// 的全部意义就是指认「哪条链路断了」。命名空间前缀同时保证它们**不会**
    /// 混进用户的真实链路组去污染那一组的通过率。
    #[test]
    fn failure_diagnostics_name_the_link_they_are_diagnosing() {
        let mut spec = base_spec();
        spec.link_group = "SGMII ↔ WLAN".into();
        spec.src.nic.gateway_v4 = "192.168.1.1".into();
        spec.dst.nic.gateway_v4 = "192.168.1.254".into();
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let diagnostics = build_traffic_failure_diagnostics(&units);

        assert!(!diagnostics.is_empty());
        for unit in &diagnostics {
            assert_eq!(
                unit.link_group, "[故障诊断] SGMII ↔ WLAN",
                "诊断单元要带上源链路组，且带命名空间前缀"
            );
            assert_ne!(
                unit.link_group, "SGMII ↔ WLAN",
                "诊断单元不许混进用户的真实链路组"
            );
        }

        // 源单元没有链路组名（矩阵/命令行路径）时退到物理网口对，仍然带前缀。
        let mut plain = base_spec();
        plain.src.nic.gateway_v4 = "192.168.1.1".into();
        plain.dst.nic.gateway_v4 = "192.168.1.254".into();
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[plain], true, &mut port);
        for unit in build_traffic_failure_diagnostics(&units) {
            assert_eq!(unit.link_group, "[故障诊断] eth0 ↔ eth0");
        }
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
