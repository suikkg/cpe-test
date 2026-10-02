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
    /// UDP 的 `profile_label` 里写着「×N流」，而 N 由 `allowed_udp_streams_for_mbps`
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
    pub notices: Vec<String>,
    pub spec_indices: Vec<usize>,
}

pub fn build_ui_units_repeated(
    specs: &[SpecNorm],
    require_same_subnet: bool,
    next_port: &mut u16,
    rounds: u32,
) -> UiPlanUnits {
    let mut units = Vec::new();
    let mut notices = Vec::new();
    let mut spec_indices = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        let (built, build_notices) =
            build_units(std::slice::from_ref(spec), require_same_subnet, next_port);
        spec_indices.extend(std::iter::repeat_n(index, built.len()));
        units.extend(built);
        notices.extend(build_notices);
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
        notices,
        spec_indices: unique_sources,
    }
}

/// 生成全部任务单元。返回 `(units, 提示信息列表)`。
///
/// 展开顺序是 方向 → IP 版本 → iperf/ping，**这个顺序进了稳定 ID**，改它会让
/// 历史 `task_results.json` 的 RESUME 不再命中。提示信息是那些「跳过了什么、
/// 为什么跳过」的话（同 /24 门禁、UDP 按链路速率裁流），它们必须走返回值
/// 而不是直接 `logln`——控制台那条路径没有终端可看。
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
                                            comparison_label: plabel.clone(),
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
                                        // 轮次由 `repeat_units` 在最外层派生；这里展开的永远是第 1 轮。
                                        round: 1,
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
                                        // 对齐键只认档位本身：裁剪、按网口策略改写的 -b / -l
                                        // 都随协商速率或 IP 变化，见 `IperfTask::comparison_label`。
                                        let comparison_label = prof.label();
                                        let mk = |idx: usize, port: u16| IperfTask {
                                            v6,
                                            udp: true,
                                            profile_name: prof.name(),
                                            profile_label: leg_label.clone(),
                                            comparison_label: comparison_label.clone(),
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
                                        // **计划期就要说清「这几条流灌不到这个门限」。**
                                        //
                                        // 执行端要求「所有必需流并发活跃」才算有效判定窗口，
                                        // 而必需流数按 `target×(1+余量)/每流负载` 上取整。配少了
                                        // 就不是「勉强够呛」，而是那个窗口**永远形不成**：整条腿
                                        // 稳定判 NOT_EVALUATED/EFFECTIVE_WINDOW_SHORT。
                                        //
                                        // 现场代价是这条链路完全确定、却只能事后才知道：真机上
                                        // 一轮 180s 预设的 UDP 单元会**全部**这样跑完再报「无法
                                        // 评价」，而拿到的原因码指向采样窗口，不指向真因。
                                        // 公式复用执行端那一份，不在这里重写。
                                        if n > 1 {
                                            let required =
                                                crate::master::executor::required_udp_streams(
                                                    n as usize,
                                                    &spec.rate_check,
                                                    target,
                                                    offered_per_stream_mbps,
                                                );
                                            if required > n as usize {
                                                if let (Some(target), Some(per_stream)) =
                                                    (target, offered_per_stream_mbps)
                                                {
                                                    let msg = format!(
                                                        "{} UDP {n} 条流 × {per_stream:.0}Mbps 灌不到 {target:.0}Mbps 门限\
                                                         （含 {:.0}% 余量至少要 {required} 条并发流）。\
                                                         按当前配置这一腿的有效判定窗口永远形不成，结果会稳定落在\
                                                         「无法评价 / EFFECTIVE_WINDOW_SHORT」。把流数提到 {required}、\
                                                         调大每流 -b，或把门限降到 {:.0}Mbps 以下。",
                                                        leg_label,
                                                        spec.rate_check.offered_headroom_pct.max(0.0),
                                                        per_stream * n as f64
                                                            / (1.0 + spec.rate_check.offered_headroom_pct.max(0.0) / 100.0),
                                                    );
                                                    if rx_target_notes.insert(msg.clone()) {
                                                        notices.push(msg);
                                                    }
                                                }
                                            }
                                        }
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
                                        // 轮次由 `repeat_units` 在最外层派生；这里展开的永远是第 1 轮。
                                        round: 1,
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
                                            comparison_label: profile_label.clone(),
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
                                    // 轮次由 `repeat_units` 在最外层派生；这里展开的永远是第 1 轮。
                                    round: 1,
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
                                            comparison_label: format!(
                                                "CTS UDP {} (每流)",
                                                profile.label().trim_start_matches("UDP ")
                                            ),
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
                                    // 轮次由 `repeat_units` 在最外层派生；这里展开的永远是第 1 轮。
                                    round: 1,
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
                            // 轮次由 `repeat_units` 在最外层派生；这里展开的永远是第 1 轮。
                            round: 1,
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
mod tests;
