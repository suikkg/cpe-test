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
            // 诊断单元不是用户勾出来的链路，不进任何报表分组。
            link_group: String::new(),
            bidir: false,
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
            // 诊断单元不是用户勾出来的链路，不进任何报表分组。
            link_group: String::new(),
            bidir: false,
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
