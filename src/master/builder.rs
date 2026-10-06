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

mod cts;
mod diagnostics;
mod identity;
mod iperf_tcp;
mod iperf_udp;
mod ping;
mod policy;

use cts::*;
#[cfg(test)]
pub use diagnostics::build_iperf_failure_diagnostics;
pub use diagnostics::build_traffic_failure_diagnostics;
use identity::*;
use iperf_tcp::*;
use iperf_udp::*;
use ping::*;
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
    /// 网关诊断合成出来的目的端点（「<网卡> 的 IPv4 网关」），不是主机上的网卡。
    pub(crate) fn is_gateway_stand_in(&self) -> bool {
        self.nic.role == diagnostics::GATEWAY_STAND_IN_ROLE
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
    /// 单向单元专用的门限，按方向（ab/ba）。空则单向也走既有兜底链。
    pub rate_targets_single: RateTargets,
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
    /// 两轮对比的对齐键用的参数：**计划里请求的档位**（套件 / 全局档位的原始标签），
    /// 不含任何随运行条件变化的改写。
    ///
    /// `profile_label` 必须写实际下发的值（报表和命令行要对得上），所以它会带上：
    /// 路径上限的裁剪（「（按路径上限从 2500M 裁剪至 1000M）」，上限跟着协商速率走）、
    /// 按网口策略改写的 `-b` / `-l`（`by_role` 跟着由协商速率推出的角色走，`by_nic`
    /// 按 IPv4 匹配，DHCP 换址就变）。这些都进对齐键的话，一次降速或换址就把同一条
    /// 测试拆成「本轮缺失 + 本轮新增」，恰好藏住了对比要抓的那次掉速。
    /// TCP 不受链路策略和裁剪影响，两者相同。
    pub comparison_label: String,
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
    /// 两轮对比的对齐键用的参数，见 [`IperfTask::comparison_label`]。
    ///
    /// UDP 的 `profile_label` 里写着「×N流」，而 N 由 `policy::udp_leg_load`
    /// 按路径上限裁剪、随协商速率变化，所以这里不写流数。TCP 的连接数来自配置，
    /// 两者相同。
    pub comparison_label: String,
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
    /// 判定入口在 `executor::verdict_assembly::bidir_total`：在两条腿同时在跑的
    /// 那一段上重算两端 RX，**只比一次** `AB + BA >= 门限`。
    pub bidir_total_target_mbps: Option<f64>,
    /// 规范方向：`ab` / `ba` / `bidir`；诊断类单元为空。
    ///
    /// **只用于展示**，判定和 resume 都不读它。存在的理由是单向单元的
    /// `Leg.tag` 是空串（见 `dir_pairs`）——那个空串在执行侧有语义（「单向」），
    /// 不能为了显示去动它；于是预览里单向单元的参数行就没有方向，而双向单元
    /// 有，同一份清单两种样子。方向本身是用户在套件里勾的，理应逐行看得见。
    pub direction: String,
    /// 稳定性轮次（1-based）。不分轮的计划恒为 1。
    ///
    /// **是类型化字段而不是从 `title` 的「· 第 N 轮」后缀里搜出来的**：那个后缀
    /// 是展示串，而 `report::compare` 要拿轮次当对齐键的一部分。字符串推断在这个
    /// 仓库已经付过一次代价（`infer_direction_tag`），改一次文案就全体失效，
    /// 而失效的表现是「对比报告少了 19 轮」这种没人会去核对的安静错误。
    ///
    /// 不进 resume identity——那件事由 `round_scoped_id` 拌进 `id` 里完成。
    pub round: u32,
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
///
/// `pub(crate)` 是给 `master::plan` 用的：算计划指纹前要把「只影响显示」的
/// 协商速率归一掉，而那件事必须走**同一个**遍历，否则新加的任务类型会在
/// 刷新那边被想起、在指纹这边被漏掉。
pub(crate) fn for_each_endpoint_mut(unit: &mut Unit, mut f: impl FnMut(&mut Endpoint)) {
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
        // 网关诊断的目的端是合成出来的，重扫结果里永远没有一块叫
        // 「以太网 的 IPv4 网关」的网卡。按接口名去找它，两端都在线时每个网关
        // 诊断都会被判成「网卡已消失」而跳过——实机 A1-S09W 三条全军覆没。
        if ep.is_gateway_stand_in() {
            return;
        }
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
        rate_targets_single: t.rate_targets_single_mbps.clone().unwrap_or_default(),
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

/// 一份计划最多重复多少遍。
///
/// 上限 100 不是技术限制，是**防手滑**：一次全量跑 11.5 小时，输错一位就是
/// 一个月。真要跑更多遍，分多次跑再用 `cpe_test compare` 逐对对比。
pub const MAX_ROUNDS: u32 = 100;

/// 给第 `round` 轮的单元派生一个独立的稳定身份。
///
/// **第 1 轮逐字节不变**（直接返回原 id）。这是刻意的：轮次是新功能，不该让
/// 任何历史 `task_results.json` 的 RESUME 命中失效——不加轮次的老计划跑出来的
/// 身份和以前一模一样。
///
/// 第 2 轮起才把轮次拌进哈希。**必须拌**：端口不进身份（见
/// `push_iperf_task_identity`），所以同一份计划展开两遍拿到的是**同一个 id**，
/// 不区分的话第 2 轮会直接命中第 1 轮刚写进去的 PASS 而整轮跳过——
/// 那正好把「连跑 N 遍看有没有偶发」这个功能本身取消掉。
///
/// 派生是确定性的，所以跨运行仍然对得上：今天的第 3 轮命中昨天的第 3 轮。
fn round_scoped_id(base: &str, round: u32) -> String {
    if round <= 1 {
        return base.to_string();
    }
    md5_hex(&format!("{base}|repeat_round_v1|{round}"))
}

/// 给一个单元里的每条腿重新分配端口。
///
/// 复制出来的轮次**不能共用第 1 轮的端口**：小计划连着跑时，第 2 轮可能撞上
/// 第 1 轮刚释放、还在 TIME_WAIT 里的那个端口。端口本来就不进稳定身份
/// （见 `push_iperf_task_identity`），所以重分不影响 RESUME。
fn reallocate_ports(unit: &mut Unit, next_port: &mut u16) {
    for leg in &mut unit.legs {
        match &mut leg.kind {
            LegKind::IperfSingle(task) => task.port = alloc_port(next_port),
            LegKind::IperfGroup { streams, .. } => {
                for task in streams {
                    task.port = alloc_port(next_port);
                }
            }
            LegKind::CtsTraffic(task) => task.port = alloc_port(next_port),
            // ping 不占端口。
            LegKind::Ping(_) => {}
        }
    }
}

/// 把一份**已经展开好**的计划重复 `rounds` 遍，整套跑完再跑一遍。
///
/// 返回的第一段就是入参本身（第 1 轮），所以调用方可以按 `units.len()` 切出
/// 每一轮。命令行与控制台两条路共用这一个函数——各写一份的话，两边的轮次
/// 身份会漂，而那意味着控制台跑的第 2 轮和命令行跑的第 2 轮互相命中不了 RESUME。
/// 把整份计划重复 `rounds` 遍，**整套跑完再跑一遍**。
///
/// # 为什么轮次在最外层
///
/// 稳定性要回答的是「同一套用例连跑 20 遍，有没有哪一遍开始掉」。轮次放在最内层
/// （同一个单元连跑 N 次）测的是另一件事——热衰减——而那个用一个更长的 `duration`
/// 就够了。最外层才是「拷机」这个词的意思。
///
/// 内环（`inner::plan`）用的是最内层（网口 → 协议 → 方向 → 轮次），那是因为它的
/// 单元少、一轮就几分钟；子网这边一轮 11.5 小时，两者要的东西不一样。
///
/// # 端口
///
/// 每一轮都重新走一遍展开，`next_port` 一路往前推，所以各轮不共用端口。
///
/// # 提示
///
/// `rounds > 1` 且开着 RESUME 时会多给一条提示：每一轮身份不同，所以**每轮至少
/// 会跑一次**；但隔天重跑时，昨天已经 PASS 的那些轮次会被跳过。
pub fn repeat_units(
    units: Vec<Unit>,
    rounds: u32,
    next_port: &mut u16,
) -> (Vec<Unit>, Vec<String>) {
    let rounds = rounds.clamp(1, MAX_ROUNDS);
    if rounds <= 1 || units.is_empty() {
        return (units, Vec::new());
    }
    let per_round = units.len();
    let mut out = units;
    for round in 2..=rounds {
        for index in 0..per_round {
            let mut unit = out[index].clone();
            unit.id = round_scoped_id(&unit.id, round);
            unit.title = format!("{} · 第 {round} 轮", unit.title);
            unit.round = round;
            reallocate_ports(&mut unit, next_port);
            out.push(unit);
        }
    }
    // 第 1 轮的标题也要标出来，否则报告里前 N 个没有轮次、后面都有，
    // 读的人会以为前面那批是「不属于任何一轮」的东西。
    for unit in out.iter_mut().take(per_round) {
        unit.title = format!("{} · 第 1 轮", unit.title);
    }
    let notice = format!(
        "稳定性轮次：整份计划重复 {rounds} 遍（每轮 {per_round} 个单元，共 {} 个）。\
         每一轮有独立的稳定身份，所以同一次运行里不会互相命中 RESUME；\
         隔天重跑时，各轮各自命中自己昨天的结果。",
        out.len()
    );
    (out, vec![notice])
}

pub fn build_units_repeated(
    specs: &[SpecNorm],
    require_same_subnet: bool,
    next_port: &mut u16,
    rounds: u32,
) -> (Vec<Unit>, Vec<String>) {
    let (units, mut notices) = build_units(specs, require_same_subnet, next_port);
    let (units, round_notices) = repeat_units(units, rounds, next_port);
    notices.extend(round_notices);
    (units, notices)
}

/// 控制台套件计划的最终单元，以及每个单元来自哪条规格。
///
/// 预览、计划指纹和执行必须共用这一份展开规则。保留原来的端口分配顺序：
/// 先展开全部规格并派生轮次，再移除重复项；去重不会重新给留下的单元派端口。
/// 命令行显式列出的重复测试仍由 `build_units_repeated` 原样执行。
pub struct UiPlanUnits {
    pub units: Vec<Unit>,
    pub notices: Vec<PlanNotice>,
    pub spec_indices: Vec<usize>,
}

/// 一条计划提示的类别。控制台按类别处理，不再从文字里猜：
/// 「是不是跳过」以前靠文字以「跳过 」开头来认，改一次措辞就静默失效，
/// 任务名恰好以「跳过」开头又会被误判。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// 普通计划提示。
    Info,
    /// 计划里的某一项因此没有生成单元。控制台的套件计划把它当阻断项
    /// （`webui::plan::CompiledPlan::blocking_errors`）。
    Skipped,
    /// 底层排查信息：不影响判定，也不必改配置才能跑。控制台不在预览里展开，
    /// 命令行与运行日志照常打印。
    Diagnostic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanNotice {
    pub kind: NoticeKind,
    pub text: String,
}

pub fn build_ui_units_repeated(
    specs: &[SpecNorm],
    require_same_subnet: bool,
    next_port: &mut u16,
    rounds: u32,
) -> UiPlanUnits {
    let mut units = Vec::new();
    // 逐条规格分别展开，跨规格同样只说一遍。
    let mut notices = Notices::default();
    let mut spec_indices = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        let (built, build_notices) =
            expand_specs(std::slice::from_ref(spec), require_same_subnet, next_port);
        spec_indices.extend(std::iter::repeat_n(index, built.len()));
        units.extend(built);
        notices.absorb(build_notices);
    }
    let (repeated, round_notices) = repeat_units(units, rounds, next_port);
    let per_round = spec_indices.len();
    if per_round > 0 {
        spec_indices = spec_indices
            .iter()
            .copied()
            .cycle()
            .take(repeated.len())
            .collect();
    }
    notices.extend(round_notices);

    let original_count = repeated.len();
    let mut seen_ids = HashSet::new();
    let mut units = Vec::with_capacity(original_count);
    let mut unique_sources = Vec::with_capacity(original_count);
    for (unit, source) in repeated.into_iter().zip(spec_indices) {
        if seen_ids.insert(unit.id.clone()) {
            units.push(unit);
            unique_sources.push(source);
        }
    }
    let removed_count = original_count - units.len();
    if removed_count > 0 {
        notices.push(format!(
            "计划去重：移除了 {removed_count} 个最终参数完全相同的重复单元"
        ));
    }
    UiPlanUnits {
        units,
        notices: notices.list,
        spec_indices: unique_sources,
    }
}

/// 生成全部任务单元。返回 `(units, 提示信息列表)`。
///
/// 展开顺序是 方向 → IP 版本 → iperf/ping，**这个顺序进了稳定 ID**，改它会让
/// 历史 `task_results.json` 的 RESUME 不再命中。提示信息是那些「跳过了什么、
/// 为什么跳过」的话（同 /24 门禁、UDP 按链路速率裁流），它们必须走返回值
/// 而不是直接 `logln`——控制台那条路径没有终端可看。
///
/// 这里只管外层循环、两道公共门禁（缺 IPv6、跨机同 /24）和后端的先后顺序；
/// 每种后端怎么展开在各自的文件里（`iperf_tcp` / `iperf_udp` / `cts` / `ping`）。
/// 端口按展开顺序全局递增分配，所以后端的先后顺序同样不能改。
pub fn build_units(
    specs: &[SpecNorm],
    require_same_subnet: bool,
    next_port: &mut u16,
) -> (Vec<Unit>, Vec<String>) {
    let (units, notices) = expand_specs(specs, require_same_subnet, next_port);
    (units, notices.into_texts())
}

/// `build_units` 的本体，提示保留类别（控制台要按类别处理，命令行只要文字）。
fn expand_specs(
    specs: &[SpecNorm],
    require_same_subnet: bool,
    next_port: &mut u16,
) -> (Vec<Unit>, Notices) {
    let mut x = Expansion {
        units: Vec::new(),
        notices: Notices::default(),
        next_port,
    };

    for spec in specs {
        let spec = &canonical_axes(spec, &mut x);
        let cross = spec.src.side != spec.dst.side;
        let same_subnet_ok =
            !cross || !require_same_subnet || same_slash24(&spec.src.nic.ipv4, &spec.dst.nic.ipv4);
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
                if v6 && v6_addrs(&spec.src.nic, &spec.dst.nic).is_none() {
                    x.notices.push_skipped(format!(
                        "跳过 {} {} IPv6：两端缺少可用的 IPv6 地址",
                        spec.name, route_str
                    ));
                    continue;
                }
                let route = Route {
                    spec,
                    dir,
                    bidir,
                    pairs: &pairs,
                    route_str: &route_str,
                    v6,
                    ip_tag: if v6 { "V6" } else { "V4" },
                    same_subnet_ok,
                };

                // ---------- iperf ----------
                if spec.kinds.iter().any(|k| k == "iperf") {
                    if !v6 && !same_subnet_ok {
                        x.notices.push_skipped(format!(
                            "跳过 {} 的 iperf：两端 IPv4 不同网段 ({} vs {})，无法直连灌包（ping 不受限）",
                            spec.name, spec.src.nic.ipv4, spec.dst.nic.ipv4
                        ));
                    } else {
                        for transport in &spec.transports {
                            if transport == "tcp" {
                                expand_iperf_tcp(&mut x, &route);
                            } else if transport == "udp" {
                                expand_iperf_udp(&mut x, &route);
                            }
                        }
                    }
                }

                // ---------- Microsoft ctsTraffic（Windows 10+ 专用） ----------
                if spec.kinds.iter().any(|kind| kind == "ctstraffic") {
                    expand_cts(&mut x, &route);
                }

                // ---------- ping ----------
                if spec.kinds.iter().any(|k| k == "ping") {
                    expand_ping(&mut x, &route);
                }
            }
        }
    }
    (x.units, x.notices)
}

/// `ip` 的规范值（`v4` / `v6`）。控制台（`webui::validate`）与配置文件共用这一张
/// 写法表：两边各认一套的话，同一个 `"ipv6"` 在控制台跑成 IPv6、在命令行跑成
/// 第二遍 IPv4。
pub(crate) fn canonical_ip_version(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "v4" | "ipv4" | "4" => Some("v4"),
        "v6" | "ipv6" | "6" => Some("v6"),
        _ => None,
    }
}

fn canonical_kind(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "iperf" => Some("iperf"),
        "ctstraffic" | "cts" => Some("ctstraffic"),
        "ping" => Some("ping"),
        _ => None,
    }
}

fn canonical_transport(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "tcp" => Some("tcp"),
        "udp" => Some("udp"),
        _ => None,
    }
}

/// 把规格里的 `ip` / `kinds` / `transports` 换成规范值（去重保序），认不出的值
/// 作为计划提示说出来。
///
/// 展开只认规范值：IP 版本只问「是不是 `v6`」，别的一律按 IPv4 展开，所以
/// `"ipv6"` 以前会被展开成第二份 IPv4——ID 相同、两份都跑、没有一句提示；后端与
/// 传输认不出的值则整类不生成，同样不说。配置文件 `tests[]` 只做了小写、`pairs`
/// 连小写都没做，所以收在展开入口：不管规格从哪条路来，都过这一道。
fn canonical_axes(spec: &SpecNorm, x: &mut Expansion<'_>) -> SpecNorm {
    let mut spec = spec.clone();
    spec.ipvers = canonical_values(
        x,
        &spec.name,
        "ip",
        &spec.ipvers,
        canonical_ip_version,
        "v4 / v6",
    );
    spec.kinds = canonical_values(
        x,
        &spec.name,
        "kinds",
        &spec.kinds,
        canonical_kind,
        "iperf / ctstraffic / ping",
    );
    // 只跑 ping 时传输协议用不上，写了什么都不必提示。
    if spec.kinds.iter().any(|kind| kind != "ping") {
        spec.transports = canonical_values(
            x,
            &spec.name,
            "transports",
            &spec.transports,
            canonical_transport,
            "tcp / udp",
        );
    }
    spec
}

fn canonical_values(
    x: &mut Expansion<'_>,
    spec_name: &str,
    field: &str,
    raw: &[String],
    canonical: fn(&str) -> Option<&'static str>,
    accepted: &str,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in raw {
        match canonical(value) {
            Some(value) => {
                if !out.iter().any(|seen| seen == value) {
                    out.push(value.to_string());
                }
            }
            None => x.notices.push_skipped(format!(
                "{spec_name}：{field} 取值 {value:?} 无法识别，已忽略（可选 {accepted}）"
            )),
        }
    }
    out
}

/// 计划提示，按第一次出现的顺序每句只记一遍。
///
/// 同一句话会在每个档位 × 每条腿 × 方向 × IP 版本上各算出来一遍（流数非法、`-w`
/// 排空、路径裁剪……），以前原样重复：一条「流数配置非法」按方向 × IP 版本印六遍，
/// `-w` 过大印十遍，命令行逐条打印、控制台逐条列出，真正要看的那几句被淹没。
/// 去重收在这里的几个 push 里，调用方没有绕过它的写法；类别见 `NoticeKind`。
#[derive(Default)]
struct Notices {
    list: Vec<PlanNotice>,
    seen: HashSet<String>,
}

impl Notices {
    fn push(&mut self, text: String) {
        self.push_kind(NoticeKind::Info, text);
    }

    fn push_skipped(&mut self, text: String) {
        self.push_kind(NoticeKind::Skipped, text);
    }

    fn push_diagnostic(&mut self, text: String) {
        self.push_kind(NoticeKind::Diagnostic, text);
    }

    fn push_kind(&mut self, kind: NoticeKind, text: String) {
        if self.seen.insert(text.clone()) {
            self.list.push(PlanNotice { kind, text });
        }
    }

    fn extend(&mut self, texts: impl IntoIterator<Item = String>) {
        for text in texts {
            self.push(text);
        }
    }

    fn absorb(&mut self, other: Notices) {
        for notice in other.list {
            self.push_kind(notice.kind, notice.text);
        }
    }

    fn into_texts(self) -> Vec<String> {
        self.list.into_iter().map(|notice| notice.text).collect()
    }
}

/// `build_units` 一路累加的结果，四种后端的展开函数共用。
struct Expansion<'p> {
    units: Vec<Unit>,
    /// 给人看的计划提示：跳过了什么、为什么跳过、门限从哪来。
    notices: Notices,
    next_port: &'p mut u16,
}

impl Expansion<'_> {
    fn port(&mut self) -> u16 {
        alloc_port(self.next_port)
    }

    /// 一条腿的判定模式与门限。
    ///
    /// 四种吞吐后端共用这一段：门限来自协商速率百分比时先把算式说出来，按
    /// `leg_rate_plan` 定门限，把「最终门限为什么不是配置里那个」作为提示说出来，
    /// 再给预览补一行最终生效的门限。它以前在 `build_units` 里逐字抄了四份，
    /// 改一份漏三份：ctsTraffic 那两份就漏了算式提示，同一个按百分比得出的门限，
    /// iperf 单元说得出来历、CTS 单元说不出。
    fn leg_rate(
        &mut self,
        route: &Route<'_>,
        policy: &rate::LinkPolicy,
        flow_direction: &str,
        src: &Endpoint,
        dst: &Endpoint,
        target_lines: &mut Vec<String>,
    ) -> (RateMode, Option<f64>) {
        note_rx_target(&mut self.notices, &route.spec.name, policy);
        let plan = leg_rate_plan(
            route.spec,
            policy,
            flow_direction,
            route.bidir,
            &src.nic,
            &dst.nic,
        );
        note_target_cap(&mut self.notices, &route.spec.name, &plan);
        target_lines.push(target_line(flow_direction, plan.target_mbps, plan.source));
        (plan.mode, plan.target_mbps)
    }
}

/// 一个「规格 × 方向 × IP 版本」组合：四种后端展开时读的同一组参数。
#[derive(Clone, Copy)]
struct Route<'a> {
    spec: &'a SpecNorm,
    /// `ab` / `ba` / `bidir`
    dir: &'a str,
    bidir: bool,
    /// 方向腿，见 `dir_pairs`：单向一条（tag 为空），双向 `[ab, ba]` 两条。
    pairs: &'a [(&'a Endpoint, &'a Endpoint, &'static str)],
    /// 标题里的「源 -> 目标」。
    route_str: &'a str,
    v6: bool,
    ip_tag: &'static str,
    /// 同机、没开门禁、或两端同 /24。只约束 IPv4 灌包，ping 不受限。
    same_subnet_ok: bool,
}

impl Route<'_> {
    /// 这个组合是不是「按两端 RX 合计判定」的双向单元。和
    /// `executor::needs_overlap_margin` 读的是同一件事：`Unit::bidir_total_target_mbps`
    /// 就是由下面 `unit()` 按这个条件填的。
    fn needs_overlap_margin(&self) -> bool {
        self.bidir && self.spec.rate_target_bidir_total.is_some()
    }

    /// 单进程灌包（iperf TCP、CTS）的进程时长；iperf UDP 组有自己的估时。
    fn single_process_secs(&self) -> u64 {
        crate::cmd::iperf_window::traffic_process_secs(
            self.spec.duration,
            self.spec.rate_check.settle_secs,
            self.needs_overlap_margin(),
        )
    }

    /// 这条腿的流向：双向取腿标签（ab / ba），单向就是单元方向。
    fn flow_direction(&self, tag: &str) -> String {
        if self.bidir {
            tag.to_string()
        } else {
            self.dir.to_string()
        }
    }

    /// 本组合下一个单元的公共字段。
    fn unit(
        &self,
        id: String,
        title: String,
        target_lines: Vec<String>,
        legs: Vec<Leg>,
        est_secs: u64,
    ) -> Unit {
        Unit {
            id,
            title,
            link_group: self.spec.link_group.clone(),
            bidir: self.bidir,
            target_lines,
            bidir_total_target_mbps: self
                .bidir
                .then_some(self.spec.rate_target_bidir_total)
                .flatten(),
            direction: self.dir.to_string(),
            // 轮次由 `repeat_units` 在最外层派生；这里展开的永远是第 1 轮。
            round: 1,
            legs,
            est_secs,
        }
    }
}

fn alloc_port(next: &mut u16) -> u16 {
    let p = *next;
    *next = next.wrapping_add(1).max(PORT_BASE);
    p
}

#[cfg(test)]
mod tests;
