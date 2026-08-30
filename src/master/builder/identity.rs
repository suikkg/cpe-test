//! RESUME identity：单元稳定 ID 的生成。
//!
//! # 为什么单独成文件
//!
//! `Unit.id` 是这个仓库里最贵的一条不变量：它是 RESUME 的 identity。改了它，
//! 用户所有的历史 PASS 记录当场全部失效——24 小时内本该跳过的单元会全部重跑，
//! 一次 11.5 小时的验收变成两次，而且**没有任何报错**，只是「怎么又从头跑了」。
//!
//! 这一层的所有细节（长度编码、字段顺序、schema 版本号）都是承重的，
//! 不许「顺手清理」。哪个字段进 identity、哪个不进，每一条都对应过一次真实
//! 的误命中或误失效。由 `the_full_unit_expansion_is_byte_stable` 逐字节钉住。
use super::*;

/// 向 resume 语义串写入一个长度编码字段。
///
/// 不能只用 `|` 拼接：主机名、接口名等外部字符串本身可能包含分隔符，进而让两组
/// 不同参数得到同一个待哈希字符串。字段名固定、值带字节长度后，编码可以无歧义解析。
pub(super) fn push_resume_field(identity: &mut String, name: &str, value: &str) {
    identity.push('|');
    identity.push_str(name);
    identity.push('=');
    identity.push_str(&value.len().to_string());
    identity.push(':');
    identity.push_str(value);
}

pub(super) fn rate_mode_identity(mode: RateMode) -> &'static str {
    match mode {
        RateMode::Auto => "auto",
        RateMode::Verify => "verify",
        RateMode::Observe => "observe",
        RateMode::Discover => "discover",
    }
}

/// 使用 IEEE-754 位模式记录浮点配置，避免显示精度或 locale 改变 resume ID。
pub(super) fn f64_identity(value: f64) -> String {
    format!("{:016x}", value.to_bits())
}

pub(super) fn option_f64_identity(value: Option<f64>) -> String {
    value
        .map(f64_identity)
        .unwrap_or_else(|| "none".to_string())
}

pub(super) fn option_str_identity(value: Option<&str>) -> String {
    value
        .map(|text| format!("some:{}:{text}", text.len()))
        .unwrap_or_else(|| "none".to_string())
}

pub(super) fn push_rate_targets_identity(
    identity: &mut String,
    prefix: &str,
    targets: &RateTargets,
) {
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
///
/// **两个 WiFi 上限有意不在这里。** 它们影响执行的唯一通路是裁剪后的 `-b` 和流数，
/// 而这两样已经分别经由 `task.extra` 和 `stream_count` 进了 identity；`udp_limit`
/// 关掉时它们对执行更是毫无影响。再记一遍不会多拦住任何一次错误复用，却会让
/// iperf / tcp / ctstraffic 三条 schema 的哈希同时改变，把所有人的 resume 缓存
/// 白白清空一次。哪天它们开始参与 RX 门限或 verdict（而不只是裁 `-b`），
/// 就必须补进来并同步升 schema 版本。
/// 覆盖由 `the_24g_ceiling_reaches_resume_identity_through_the_clipped_load` 钉住。
pub(super) fn push_rate_check_identity(identity: &mut String, cfg: &RateCheckCfg) {
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

pub(super) fn push_endpoint_identity(identity: &mut String, prefix: &str, endpoint: &Endpoint) {
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

pub(super) fn push_iperf_task_identity(identity: &mut String, prefix: &str, task: &IperfTask) {
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
        &option_f64_identity(task.offered_per_stream_mbps),
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

pub(super) fn udp_resume_unit_id_with_schema(
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
        &spec.requested_streams(true).to_string(),
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

/// UDP resume ID schema v4：除覆盖实际 offered load、裁剪后的流数、方向目标、模式、
/// socket buffer 和全部验收阈值外，也隔离使用旧流量窗口作为背景截止点的 v3 结果。
pub(super) fn udp_resume_unit_id_v4(
    spec: &SpecNorm,
    ip_tag: &str,
    direction: &str,
    profile: &UdpProfile,
    legs: &[Leg],
) -> String {
    udp_resume_unit_id_with_schema("iperf_v4", true, spec, ip_tag, direction, profile, legs)
}

/// TCP resume identity includes the resolved RX target and rate policy.  The
/// v1 identity predates NIC-RX validation, so reusing it could silently skip
/// a result produced under the old, tool-only PASS rule.
///
/// v3 隔离 v2：v2 时代的 PASS 只校验了接收端网卡采样，发送端根本没采。现在
/// 有明确目标时 RX/TX 双侧的采样与 5 秒滚动窗口覆盖率都必须达标，PASS 变严了，
/// 因此 v2 缓存的 PASS 不能跨语义复用——否则会静默跳过一个在新规则下未必
/// 通得过的测试。
pub(super) fn tcp_resume_unit_id_v2(
    spec: &SpecNorm,
    ip_tag: &str,
    direction: &str,
    profile: &str,
    legs: &[Leg],
) -> String {
    let mut identity = "iperf_tcp_v3".to_string();
    push_resume_field(&mut identity, "transport", "tcp");
    push_resume_field(&mut identity, "ip", ip_tag);
    push_resume_field(&mut identity, "direction", direction);
    push_resume_field(&mut identity, "duration", &spec.duration.to_string());
    push_resume_field(&mut identity, "profile", profile);
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
                push_iperf_task_identity(&mut identity, &format!("{prefix}.stream.0"), task);
            }
            LegKind::IperfGroup { streams, .. } => {
                push_resume_field(&mut identity, &format!("{prefix}.kind"), "group");
                for (stream_idx, task) in streams.iter().enumerate() {
                    push_iperf_task_identity(
                        &mut identity,
                        &format!("{prefix}.stream.{stream_idx}"),
                        task,
                    );
                }
            }
            _ => push_resume_field(&mut identity, &format!("{prefix}.kind"), "invalid"),
        }
    }
    md5_hex(&identity)
}

/// 配置里 `by_nic.host` 用的主机键。
pub(super) fn host_key(side: Side) -> &'static str {
    match side {
        Side::Master => "master",
        Side::Agent => "agent",
    }
}

pub(super) fn cts_task_identity(identity: &mut String, prefix: &str, task: &CtsTrafficTask) {
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
        ("offered_mbps", option_f64_identity(task.offered_total_mbps)),
        (
            "setup_error",
            task.setup_error.clone().unwrap_or_else(|| "none".into()),
        ),
    ] {
        push_resume_field(identity, &format!("{prefix}.{name}"), &value);
    }
    push_endpoint_identity(identity, &format!("{prefix}.src"), &task.src);
    push_endpoint_identity(identity, &format!("{prefix}.dst"), &task.dst);
    // port 是临时资源，故意不进入 resume ID。
}

pub(super) fn cts_resume_unit_id_with_schema(
    schema: &str,
    spec: &SpecNorm,
    ip_tag: &str,
    direction: &str,
    legs: &[Leg],
) -> String {
    let mut identity = schema.to_string();
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

pub(super) fn cts_resume_unit_id(
    spec: &SpecNorm,
    ip_tag: &str,
    direction: &str,
    legs: &[Leg],
) -> String {
    // v3 吸收共享 RX-P10/rolling coverage 判定；v2 结果不能跨判定语义复用。
    // v4 再加一道：有目标时发送端网卡的采样与滚动覆盖率同样要达标（此前 CTS
    // 压根不采发送端）。PASS 条件变严，v3 缓存同样不能复用。
    cts_resume_unit_id_with_schema("ctstraffic_v4", spec, ip_tag, direction, legs)
}
