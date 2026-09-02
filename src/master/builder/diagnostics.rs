//! 自动故障诊断单元的生成。
//!
//! 一轮测试里连续多个灌包单元一条测量都没产生时，`RunSummary` 会要求追加一批
//! 诊断单元（同 /24 的 ping、网关 ping），把「设备掉线了还是参数配错了」这个
//! 问题当场问清楚。run_20260825_215915_7684 的尾部有 6 个单元白跑了 21 分钟
//! 而工具全程没有任何提示——这批诊断就是为了不再出现那种情况。
//!
//! 它们是**追加**在正常队列之后的，不参与 resume，也不进用户的计划。
use super::*;

pub(super) fn subnet_ping_key(src: &Endpoint, dst: &Endpoint, payload: u32) -> String {
    format!("{}|{}|{payload}", src.key(), dst.key())
}

/// 诊断单元在报表里的分组键前缀。
///
/// 诊断单元不是用户勾出来的链路，所以**不能落进用户的链路组**——那会让
/// 「SGMII ↔ WLAN 这一组的通过率」把几条 4 包短 Ping 也算进去。但它们过去
/// 是 `link_group: String::new()`，于是在 Excel 的「按链路分组」里全部挤进
/// 「(未分组)」一行：链路组一多，报表上就分不出这条诊断说的是哪条链路了，
/// 而诊断存在的全部意义就是指认「哪条链路断了」。
///
/// 折中是**带命名空间的键**：既和真实链路组分得开，又保留了它从哪来。
const DIAGNOSTIC_GROUP_PREFIX: &str = "[故障诊断]";

fn diagnostic_link_group(origin: &str, src: &Endpoint, dst: &Endpoint) -> String {
    let origin = origin.trim();
    if !origin.is_empty() {
        return format!("{DIAGNOSTIC_GROUP_PREFIX} {origin}");
    }
    // 源单元没有链路组名（矩阵路径、命令行路径）时退到物理网口对——和
    // `executor/row.rs::link_group` 的第二档同一个口径。**永不用主机名**。
    let ifaces = (src.nic.name.trim(), dst.nic.name.trim());
    if !ifaces.0.is_empty() && !ifaces.1.is_empty() {
        return format!("{DIAGNOSTIC_GROUP_PREFIX} {} ↔ {}", ifaces.0, ifaces.1);
    }
    DIAGNOSTIC_GROUP_PREFIX.to_string()
}

/// 当本轮所有吞吐后端都没有产生有效测量时，按失败任务涉及的方向和网卡
/// 构造一组短时、去重的诊断任务：
///
/// - 每个唯一 IPv4 方向固定使用 32 字节短 Ping；
/// - 每块涉及网卡绑定自己的 IPv4 源地址 Ping 自己的 IPv4 网关；
/// - 已经在本轮选择中的同方向 32 字节常规 Ping 不重复执行；
/// - 网关为空也保留诊断单元，由执行器报告 GATEWAY_NOT_FOUND，而不是伪装成丢包。
pub fn build_traffic_failure_diagnostics(selected_units: &[Unit]) -> Vec<Unit> {
    // 第三项是**源单元的链路组名**：诊断单元要能说出自己在替哪条链路做体检。
    let mut traffic_pairs: Vec<(Endpoint, Endpoint, String)> = Vec::new();
    let mut existing_subnet_pings = HashSet::new();
    for unit in selected_units {
        let origin = unit.link_group.clone();
        for leg in &unit.legs {
            match &leg.kind {
                LegKind::IperfSingle(task) => {
                    traffic_pairs.push((task.src.clone(), task.dst.clone(), origin.clone()))
                }
                LegKind::IperfGroup { streams, .. } => {
                    traffic_pairs.extend(
                        streams
                            .iter()
                            .map(|task| (task.src.clone(), task.dst.clone(), origin.clone())),
                    );
                }
                LegKind::CtsTraffic(task) => {
                    traffic_pairs.push((task.src.clone(), task.dst.clone(), origin.clone()))
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

    let mut directions: BTreeMap<String, (Endpoint, Endpoint, String)> = BTreeMap::new();
    let mut endpoints: BTreeMap<String, (Endpoint, Endpoint, String)> = BTreeMap::new();
    for (src, dst, origin) in traffic_pairs {
        if !src.nic.ipv4.is_empty() && !dst.nic.ipv4.is_empty() {
            let direction_key = format!("{}|{}", src.key(), dst.key());
            directions
                .entry(direction_key)
                .or_insert_with(|| (src.clone(), dst.clone(), origin.clone()));
        }
        // 网关诊断的分组键要跟着**这块网卡参与的那条链路**走，所以对端也一起
        // 记下来——只有网卡自己时，退化档给不出「↔」那一对。
        for (endpoint, peer) in [(&src, &dst), (&dst, &src)] {
            if !endpoint.nic.ipv4.is_empty() {
                endpoints
                    .entry(endpoint.key())
                    .or_insert_with(|| (endpoint.clone(), peer.clone(), origin.clone()));
            }
        }
    }

    let mut diagnostics = Vec::new();
    for (src, dst, origin) in directions.into_values() {
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
            // 诊断单元不进用户的链路组，但要带上自己替哪条链路做体检——
            // 见 `diagnostic_link_group`。
            link_group: diagnostic_link_group(&origin, &src, &dst),
            title,
            bidir: false,
            bidir_total_target_mbps: None,
            target_lines: Vec::new(),
            // 诊断单元不是用户勾出来的方向，留空即可（展示层会跳过）。
            direction: String::new(),
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
            est_secs: ping_estimated_secs(DIAGNOSTIC_PING_COUNT),
        });
    }

    for (endpoint, peer, origin) in endpoints.into_values() {
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
            // 分组键跟着**这块网卡参与的那条链路**走：网关 ping 的对端是一个
            // 合成出来的「网关」端点，拿它去推分组只会得到一批各不相同、
            // 又都对不上真实链路的名字。
            link_group: diagnostic_link_group(&origin, &endpoint, &peer),
            title,
            bidir: false,
            bidir_total_target_mbps: None,
            target_lines: Vec::new(),
            // 诊断单元不是用户勾出来的方向，留空即可（展示层会跳过）。
            direction: String::new(),
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
            est_secs: ping_estimated_secs(DIAGNOSTIC_PING_COUNT),
        });
    }

    diagnostics
}

/// 兼容旧测试/调用名称；诊断范围现已覆盖 iperf3 与 ctsTraffic。
#[cfg(test)]
pub fn build_iperf_failure_diagnostics(selected_units: &[Unit]) -> Vec<Unit> {
    build_traffic_failure_diagnostics(selected_units)
}
