//! Ping：每个包长一个单元。不受同 /24 门禁限制，也没有速率门限。
use super::*;

/// 一个「规格 × 方向 × IP 版本」组合下的全部 ping 单元。
pub(super) fn expand_ping(x: &mut Expansion<'_>, route: &Route<'_>) {
    let Route {
        spec,
        dir,
        bidir,
        pairs,
        route_str,
        v6,
        ip_tag,
        ..
    } = *route;
    for payload in &spec.payload_sizes {
        let mut legs = Vec::new();
        // Ping 单元没有速率门限：RTT 与丢包的判定在别处。
        let target_lines: Vec<String> = Vec::new();
        for (s, d, tag) in pairs {
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
        let est_secs = ping_estimated_secs(spec.ping_count);
        x.units.push(Unit {
            // Ping 不是吞吐测试，没有 RX 合计门限这回事。
            bidir_total_target_mbps: None,
            ..route.unit(id, title, target_lines, legs, est_secs)
        });
    }
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
pub(super) fn ping_estimated_secs(count: u32) -> u64 {
    count as u64 + 12
}
