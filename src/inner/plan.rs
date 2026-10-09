//! 内环执行计划。
//!
//! **全仓唯一的笛卡尔积**：页面预览、执行器、进度计数和报告都消费这一份
//! 计划。此前四处各自算「链路 × 协议 × 方向」，只要有一处漏了新维度，
//! 预览说 8 个单元、执行器跑 12 个、进度条却停在 8/8。
use super::config::{Direction, Flow, InnerConfig, Link, Measurement, Parameters, Protocol};
use serde::Serialize;
use std::collections::HashSet;

/// 单元之间的固定开销：起接收端 server、等就绪、背景采样、停流回收。
/// 只用于预估时间，不参与任何判定。
pub const UNIT_OVERHEAD_SECS: u64 = 20;
/// 双向单元要多起一套 server、多回收一条腿。
pub const BIDIR_EXTRA_OVERHEAD_SECS: u64 = 8;
/// 起流头这几秒不计入平均：TCP 的慢启动和窗口增长、Wi-Fi 的速率自适应都要先
/// 收敛一会儿。判定窗口从流量起点扣掉这一段，client 跟着多跑这一段。与子网
/// `iperf.rate_check.settle_secs` 的默认值一致、TCP/UDP 同样扣；内环配置刻意不带
/// 子网的判定参数块，所以这里是常量。
pub const SETTLE_SECS: u64 = 5;

/// client 实际跑多久：配置时长 + 起流爬升 + 双向交集余量（判定窗口仍截到
/// 配置时长）。算法与子网共用 `cmd::iperf_window::traffic_process_secs`。
pub fn client_secs(cfg: &InnerConfig, unit: &Unit) -> u64 {
    crate::cmd::iperf_window::traffic_process_secs(cfg.duration_secs, SETTLE_SECS, unit.is_bidir())
}

/// 一个单元的预计耗时；计划预览和「去掉 RESUME 命中后还剩多久」共用。
fn unit_estimated_secs(cfg: &InnerConfig, unit: &Unit) -> u64 {
    client_secs(cfg, unit)
        + UNIT_OVERHEAD_SECS
        + if unit.is_bidir() {
            BIDIR_EXTRA_OVERHEAD_SECS
        } else {
            0
        }
}

/// 一条腿：一个数据走向、一个板侧端口、一个接收端。
#[derive(Debug, Clone, Serialize)]
pub struct LegPlan {
    pub flow: Flow,
    /// 接收端 server 端口。双向两条腿必须落在不同端口上，否则两股流会
    /// 撞进同一个 server，谁的字节算谁的就说不清了。
    pub port: u16,
    /// 网卡口径的本腿门限。
    pub nic_target_mbps: Option<f64>,
    /// 工具口径的本腿门限，与网卡口径完全独立。
    pub tool_target_mbps: Option<f64>,
}

/// 记录本单元配置的双向合计门限，结果展示不能回读后来编辑过的配置。
#[derive(Debug, Clone, Serialize)]
pub struct BidirTargets {
    pub nic_mbps: Option<f64>,
    pub tool_mbps: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Unit {
    /// 与端口、执行序号和链路列表位置无关的稳定身份，供内环 RESUME 使用。
    pub id: String,
    /// 执行序号，从 1 开始，与页面预览和报告里的编号一致。
    pub index: usize,
    /// 在 `cfg.links` 中的下标。
    pub link: usize,
    pub link_name: String,
    pub host: String,
    pub ip_version: u8,
    pub protocol: Protocol,
    pub direction: Direction,
    pub streams: u32,
    pub parameters: Parameters,
    pub measurement: Measurement,
    /// 第几轮重复，从 1 开始。
    pub repeat: u32,
    pub legs: Vec<LegPlan>,
    /// 双向合计门限。`Some` 时本单元按两端接收速率之和判定一次、两条腿
    /// 只测量；`None` 时两条腿各自按方向门限判定。**绝不**由单向门限
    /// 除以二推出来。
    pub nic_total_target_mbps: Option<f64>,
    pub tool_total_target_mbps: Option<f64>,
}

impl Unit {
    pub fn bidir_targets(&self) -> Option<BidirTargets> {
        (self.is_bidir()
            && (self.nic_total_target_mbps.is_some() || self.tool_total_target_mbps.is_some()))
        .then_some(BidirTargets {
            nic_mbps: self.nic_total_target_mbps,
            tool_mbps: self.tool_total_target_mbps,
        })
    }

    pub fn is_bidir(&self) -> bool {
        self.direction.is_bidir()
    }
    /// 本单元在指定口径下是否按合计判定。
    pub fn total_target(&self, tool: bool) -> Option<f64> {
        if !self.is_bidir() {
            return None;
        }
        if tool {
            self.tool_total_target_mbps
        } else {
            self.nic_total_target_mbps
        }
    }
    pub fn title(&self) -> String {
        format!(
            "{} / {} / IPv{} / {} / {}",
            self.host,
            self.link_name,
            self.ip_version,
            self.protocol.label(),
            self.direction.label()
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub units: Vec<Unit>,
    /// 参与本轮的链路下标，保持用户排定的顺序。
    pub links: Vec<usize>,
    /// 本轮实际被引用的辅测机 id。没被引用的辅测机不连接、不阻断。
    pub agents: Vec<String>,
    /// 本机是否参与打流（有链路挂在 master 上）。
    pub uses_master: bool,
    pub estimated_secs: u64,
}

impl Plan {
    pub fn unit_count(&self) -> usize {
        self.units.len()
    }
    pub fn leg_count(&self) -> usize {
        self.units.iter().map(|unit| unit.legs.len()).sum()
    }
    pub fn bidir_units(&self) -> usize {
        self.units.iter().filter(|unit| unit.is_bidir()).count()
    }
}

fn leg_plan(link: &Link, flow: Flow, port: u16) -> LegPlan {
    LegPlan {
        flow,
        port,
        nic_target_mbps: link.leg_target(flow, false),
        // 严格模式下工具口径不参与判定，门限本身也已被 validate 拒绝；
        // 这里再兜一次，保证计划里不会出现一个永远用不上的门限。
        tool_target_mbps: link
            .measurement
            .uses_tool()
            .then(|| link.leg_target(flow, true))
            .flatten(),
    }
}

fn push_id_field(out: &mut String, name: &str, value: impl std::fmt::Display) {
    let value = value.to_string();
    out.push_str(name);
    out.push('=');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(&value);
    out.push('|');
}

/// 所有会改变实际流量或验收口径的字段都进入身份；端口和执行序号是临时资源，
/// 不进入身份，这样调整起始端口或启用链路顺序不会把同一单元的 PASS 清空。
fn unit_id(
    cfg: &InnerConfig,
    link: &Link,
    ip_version: u8,
    protocol: Protocol,
    direction: Direction,
    repeat: u32,
    streams: u32,
) -> String {
    let mut raw = String::from("inner-resume-v1|");
    push_id_field(&mut raw, "link", &link.name);
    push_id_field(&mut raw, "host", &link.host);
    push_id_field(&mut raw, "iface", &link.local_interface);
    push_id_field(&mut raw, "local_ip", link.local_ip);
    push_id_field(&mut raw, "gateway", link.gateway);
    push_id_field(&mut raw, "board_rx", &link.board_rx_interface);
    // 逻辑链路字段相同不代表还是同一台被测设备：ADB 可以切到另一块板，
    // 辅测机也可能换地址/端口。设备身份必须进入 RESUME，否则换机后会静默
    // 复用旧 PASS。令牌不进身份，轮换凭据不应让同一设备的测量失效。
    push_id_field(&mut raw, "adb_path", &cfg.adb_path);
    push_id_field(&mut raw, "adb_serial", &cfg.serial);
    push_id_field(&mut raw, "board_iperf", &cfg.board_iperf);
    if link.host != "master" {
        if let Some(agent) = cfg.agents.iter().find(|agent| agent.id == link.host) {
            push_id_field(&mut raw, "agent_id", &agent.id);
            push_id_field(&mut raw, "agent_address", &agent.address);
            push_id_field(&mut raw, "agent_port", agent.port);
        }
    }
    // v1/v2 曾将完整 Link 的 Debug 文本混进身份。新增字段不能使既有 IPv4
    // PASS 全失效，因此固定旧字段和顺序；IPv6 单独加入实际地址与版本。
    push_id_field(&mut raw, "measurement", legacy_link_identity(link));
    if ip_version == 6 {
        push_id_field(&mut raw, "ip_version", ip_version);
        push_id_field(&mut raw, "local_ipv6", link.local_address(ip_version));
        push_id_field(&mut raw, "gateway_ipv6", link.board_address(ip_version));
    }
    push_id_field(&mut raw, "protocol", format!("{protocol:?}"));
    push_id_field(&mut raw, "direction", format!("{direction:?}"));
    push_id_field(&mut raw, "repeat", repeat);
    push_id_field(&mut raw, "streams", streams);
    push_id_field(&mut raw, "duration", cfg.duration_secs);
    push_id_field(
        &mut raw,
        "tcp_window",
        cfg.tcp_window.as_deref().unwrap_or(""),
    );
    push_id_field(
        &mut raw,
        "udp_mbps",
        cfg.udp_mbps.map(f64::to_bits).unwrap_or_default(),
    );
    push_id_field(
        &mut raw,
        "udp_length",
        cfg.udp_length.as_deref().unwrap_or(""),
    );
    push_id_field(
        &mut raw,
        "udp_loss",
        cfg.max_udp_loss_pct.map(f64::to_bits).unwrap_or_default(),
    );
    for (name, value) in [
        ("upload", link.upload_min_mbps),
        ("download", link.download_min_mbps),
        ("bidir_total", link.bidir_total_min_mbps),
        ("tool_upload", link.tool_upload_min_mbps),
        ("tool_download", link.tool_download_min_mbps),
        ("tool_bidir_total", link.tool_bidir_total_min_mbps),
    ] {
        push_id_field(&mut raw, name, value.map(f64::to_bits).unwrap_or_default());
    }
    crate::util::md5_hex(&raw)
}

fn legacy_link_identity(link: &Link) -> String {
    format!(
        "Link {{ name: {:?}, host: {:?}, enabled: {:?}, local_interface: {:?}, local_ip: {:?}, gateway: {:?}, board_rx_interface: {:?}, measurement: {:?}, upload_min_mbps: {:?}, download_min_mbps: {:?}, bidir_total_min_mbps: {:?}, tool_upload_min_mbps: {:?}, tool_download_min_mbps: {:?}, tool_bidir_total_min_mbps: {:?} }}",
        link.name, link.host, link.enabled, link.local_interface, link.local_ip, link.gateway,
        link.board_rx_interface, link.measurement, link.upload_min_mbps, link.download_min_mbps,
        link.bidir_total_min_mbps, link.tool_upload_min_mbps, link.tool_download_min_mbps,
        link.tool_bidir_total_min_mbps,
    )
}

/// 展开计划。顺序即执行顺序：**网口 → IP 版本 → 协议 → 方向 → 重复轮次**。
///
/// 外层永远按用户排定的网口顺序串行，只有双向单元内部并发。不同网口
/// 绝不同时灌包——板侧桥计数器会把两条链路的流量混在一起。
pub fn build(cfg: &InnerConfig) -> Result<Plan, String> {
    cfg.validate()?;
    let links = cfg.active_links();
    let mut units = Vec::new();
    for &link_index in &links {
        let link = &cfg.links[link_index];
        for &ip_version in &cfg.ip_versions {
            for &protocol in &cfg.protocols {
                let effective_link = link.for_protocol(protocol);
                let link = &effective_link;
                for parameters in cfg.parameter_variants(protocol) {
                    let effective_cfg = parameters.apply(cfg, protocol);
                    let cfg = &effective_cfg;
                    for &direction in &cfg.directions {
                        for repeat in 1..=cfg.repeats {
                            let legs = direction
                                .flows()
                                .iter()
                                .enumerate()
                                .map(|(offset, &flow)| {
                                    let port =
                                        cfg.port.checked_add(offset as u16).ok_or_else(|| {
                                            "板侧端口加一后越界，请把起始端口调低".to_string()
                                        })?;
                                    Ok(leg_plan(link, flow, port))
                                })
                                .collect::<Result<Vec<_>, String>>()?;
                            units.push(Unit {
                                id: unit_id(
                                    cfg,
                                    link,
                                    ip_version,
                                    protocol,
                                    direction,
                                    repeat,
                                    cfg.streams(protocol),
                                ),
                                index: units.len() + 1,
                                link: link_index,
                                link_name: link.name.clone(),
                                host: link.host.clone(),
                                ip_version,
                                protocol,
                                direction,
                                streams: cfg.streams(protocol),
                                parameters: parameters.clone(),
                                measurement: link.measurement,
                                repeat,
                                legs,
                                nic_total_target_mbps: direction
                                    .is_bidir()
                                    .then(|| link.total_target(false))
                                    .flatten(),
                                tool_total_target_mbps: (direction.is_bidir()
                                    && link.measurement.uses_tool())
                                .then(|| link.total_target(true))
                                .flatten(),
                            });
                        }
                    }
                }
            }
        }
    }
    let estimated_secs = units
        .iter()
        .map(|unit| unit_estimated_secs(cfg, unit))
        .sum();
    Ok(Plan {
        uses_master: links.iter().any(|&i| cfg.links[i].host == "master"),
        agents: cfg
            .referenced_agents()
            .into_iter()
            .map(|agent| agent.id.clone())
            .collect(),
        links,
        units,
        estimated_secs,
    })
}

/// 页面预览。执行前用它回答「这一轮到底要跑什么」，配置一改就重新算。
#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub links: usize,
    pub units: usize,
    pub legs: usize,
    pub bidir_units: usize,
    pub estimated_secs: u64,
    pub uses_master: bool,
    pub agents: Vec<String>,
    /// 未参与本轮的链路名，明确告诉用户它们只是没勾选，配置还在。
    pub skipped: Vec<String>,
    /// 逐单元一行的人话说明，含接收端和门限来源。
    pub rows: Vec<PreviewRow>,
    /// 本轮按历史 PASS 可跳过的单元数。
    pub resumed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewRow {
    pub index: usize,
    pub id: String,
    pub link: String,
    pub host: String,
    pub ip_version: u8,
    pub protocol: Protocol,
    pub direction: Direction,
    pub repeat: u32,
    pub measurement: Measurement,
    pub parameters: Parameters,
    pub parameter_label: String,
    pub legs: Vec<PreviewLeg>,
    /// 判定依据的人话描述，例如「网卡口径合计门限 900.000 Mbps」。
    pub verdict_basis: String,
    pub resumed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewLeg {
    pub flow: Flow,
    pub port: u16,
    /// 接收端在哪一侧。板侧统计接口要等预检拿到板侧地址表才能定，
    /// 因此这里只说「板侧」还是具体的电脑网卡名。
    pub receiver: String,
    pub nic_target_mbps: Option<f64>,
    pub tool_target_mbps: Option<f64>,
}

fn describe_target(target: Option<f64>) -> String {
    target
        .map(|value| format!("{value:.3} Mbps"))
        .unwrap_or_else(|| "未设置".into())
}

/// 判定依据。**只描述计划里已经确定的部分**：具体用网卡还是工具口径要等
/// 跑完看计数是否可信，这里说的是「配了哪些门限、按合计还是按方向」。
fn verdict_basis(unit: &Unit) -> String {
    let tool = unit.measurement.uses_tool();
    if unit.is_bidir() {
        return match (unit.nic_total_target_mbps, unit.tool_total_target_mbps) {
            (None, None) => format!(
                "按逐方向门限判定（{}）；未设置双向合计门限，不由单向门限折半推算",
                unit.measurement.label()
            ),
            (nic, tool_total) => format!(
                "按两端接收速率合计判定一次，两个方向只测量：网卡口径合计 {}，工具口径合计 {}",
                describe_target(nic),
                describe_target(tool_total)
            ),
        };
    }
    let leg = &unit.legs[0];
    if tool {
        format!(
            "{}：网卡口径门限 {}，工具口径门限 {}",
            unit.measurement.label(),
            describe_target(leg.nic_target_mbps),
            describe_target(leg.tool_target_mbps)
        )
    } else {
        format!(
            "{}：网卡口径门限 {}",
            unit.measurement.label(),
            describe_target(leg.nic_target_mbps)
        )
    }
}

#[allow(dead_code)]
pub fn preview(cfg: &InnerConfig) -> Result<Preview, String> {
    preview_with_resumed(cfg, &HashSet::new())
}

/// 预览时标记可恢复单元。历史扫描由调用方完成，避免让纯计划模块隐式读盘。
pub fn preview_with_resumed(
    cfg: &InnerConfig,
    resumed: &HashSet<String>,
) -> Result<Preview, String> {
    let plan = build(cfg)?;
    let rows = plan
        .units
        .iter()
        .map(|unit| {
            let link = &cfg.links[unit.link];
            PreviewRow {
                index: unit.index,
                id: unit.id.clone(),
                link: unit.link_name.clone(),
                host: unit.host.clone(),
                ip_version: unit.ip_version,
                protocol: unit.protocol,
                direction: unit.direction,
                repeat: unit.repeat,
                measurement: unit.measurement,
                verdict_basis: verdict_basis(unit),
                parameters: unit.parameters.clone(),
                parameter_label: unit.parameters.label(unit.protocol),
                resumed: resumed.contains(&unit.id),
                legs: unit
                    .legs
                    .iter()
                    .map(|leg| PreviewLeg {
                        flow: leg.flow,
                        port: leg.port,
                        receiver: if leg.flow.receiver_is_board() {
                            if link.board_rx_interface.is_empty() {
                                format!(
                                    "板侧（按 {} 归属自动识别）",
                                    link.board_address(unit.ip_version)
                                )
                            } else {
                                format!("板侧 {}", link.board_rx_interface)
                            }
                        } else {
                            format!("{} · {}", link.host, link.local_interface)
                        },
                        nic_target_mbps: leg.nic_target_mbps,
                        tool_target_mbps: leg.tool_target_mbps,
                    })
                    .collect(),
            }
        })
        .collect();
    let resumed_count = plan
        .units
        .iter()
        .filter(|unit| resumed.contains(&unit.id))
        .count();
    let resumed_secs = plan
        .units
        .iter()
        .filter(|unit| resumed.contains(&unit.id))
        .map(|unit| unit_estimated_secs(cfg, unit))
        .sum::<u64>();
    Ok(Preview {
        links: plan.links.len(),
        units: plan.unit_count(),
        legs: plan.leg_count(),
        bidir_units: plan.bidir_units(),
        estimated_secs: plan.estimated_secs.saturating_sub(resumed_secs),
        uses_master: plan.uses_master,
        agents: plan.agents.clone(),
        skipped: cfg
            .links
            .iter()
            .filter(|link| !link.enabled)
            .map(|link| link.name.clone())
            .collect(),
        rows,
        resumed: resumed_count,
    })
}
