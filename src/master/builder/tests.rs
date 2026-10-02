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
        rate_targets_single: RateTargets::default(),
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

/// **对比用的参数标签不随协商速率变。**
///
/// 同一份 UDP 计划，链路从 2.5G 降到 1G 时：`-b 500m × 4 流` 被裁成 2 条流，
/// `-b 2500m` 单流被压到 1000M 并在 `profile_label` 里写明裁剪。报表必须写实际
/// 下发的值，所以 `profile_label` 变了是对的；但对比报告要靠 `comparison_label`
/// 认出「这还是同一条测试」，才能把这次掉速报成退化，而不是「缺失 + 新增」。
#[test]
fn the_comparison_label_survives_a_renegotiation_that_clamps_streams_and_bandwidth() {
    fn udp_legs(units: &[Unit]) -> Vec<(usize, String, String)> {
        let mut out = Vec::new();
        for unit in units {
            for leg in &unit.legs {
                match &leg.kind {
                    LegKind::IperfSingle(t) => {
                        out.push((1, t.profile_label.clone(), t.comparison_label.clone()))
                    }
                    LegKind::IperfGroup { streams, .. } => out.push((
                        streams.len(),
                        streams[0].profile_label.clone(),
                        streams[0].comparison_label.clone(),
                    )),
                    _ => {}
                }
            }
        }
        out
    }
    let plan = |src_role: &str, speed: u64| {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "eth0", src_role, "192.168.1.2", speed);
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("500m"), UdpProfile::bw("2500m")];
        spec.udp_streams = 4;
        spec.streams = 4;
        spec.udp_limit = true;
        let mut port = 45000u16;
        udp_legs(&build_units(&[spec], true, &mut port).0)
    };
    let fast = plan("SGMII2.5G", 2500);
    let slow = plan("SGMII1G", 1000);
    assert_eq!(fast.len(), 2);
    assert_eq!(slow.len(), 2);
    assert_ne!(
        fast[0].0, slow[0].0,
        "降速后 -b 500m 的流数被裁剪：{fast:?} / {slow:?}"
    );
    assert_ne!(
        fast[1].1, slow[1].1,
        "降速后 -b 2500m 被压到路径上限，标签写明裁剪"
    );
    for (fast, slow) in fast.iter().zip(&slow) {
        assert_eq!(fast.2, slow.2, "对比标签不能跟着协商速率变");
        assert!(
            !fast.2.contains("裁剪"),
            "对比标签里不能出现裁剪说明：{}",
            fast.2
        );
    }
}

/// **历史标签归一的规则和 builder 的标签文案是同一件事的两半。**
///
/// 6.5.1 及更早的对比身份里存的是 `profile_label`（实际下发的值），读取时由
/// `report::compare::legacy_parameter` 去掉随运行条件生成的说明，还原成
/// `comparison_label`。这里拿 builder 真实产出的三类标签逐个过一遍：
/// 路径裁剪、按角色策略改写的 `-b`、CTS 被裁剪的流数。以后改标签文案而忘了
/// 改归一规则，这条先红，而不是让历史对比悄悄对不上。
#[test]
fn legacy_labels_normalize_to_the_comparison_label_the_builder_now_writes() {
    use crate::report::compare::legacy_parameter;
    fn labels(units: &[Unit]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for unit in units {
            for leg in &unit.legs {
                match &leg.kind {
                    LegKind::IperfSingle(t) => {
                        out.push((t.profile_label.clone(), t.comparison_label.clone()))
                    }
                    LegKind::IperfGroup { streams, .. } => out.push((
                        streams[0].profile_label.clone(),
                        streams[0].comparison_label.clone(),
                    )),
                    LegKind::CtsTraffic(t) => {
                        out.push((t.profile_label.clone(), t.comparison_label.clone()))
                    }
                    LegKind::Ping(_) => {}
                }
            }
        }
        out
    }
    let build = |spec: SpecNorm| {
        let mut port = 45000u16;
        labels(&build_units(&[spec], true, &mut port).0)
    };
    let fast_src = ep(Side::Master, "eth0", "SGMII2.5G", "192.168.1.2", 2500);
    let slow_src = ep(Side::Master, "eth0", "SGMII1G", "192.168.1.2", 1000);

    // ① 路径裁剪：1G 上的 -b 2500m 被压到 1000M。
    let clipped = |src: &Endpoint| {
        let mut spec = base_spec();
        spec.src = src.clone();
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("2500m")];
        spec.udp_limit = true;
        spec
    };
    // ② 按角色策略：降速后角色变成 SGMII1G，命中策略，-b 被改写成 800M。
    let policed = |src: &Endpoint| {
        let mut spec = base_spec();
        spec.src = src.clone();
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("500m")];
        spec.link_profiles = LinkProfiles {
            by_role: vec![RoleProfile {
                pair: "SGMII1G<->SGMII2.5G".into(),
                rx_target_mbps: RateTargets::default(),
                udp_bandwidth: DirectionalBandwidth {
                    ab: Some("800m".into()),
                    ..Default::default()
                },
            }],
            by_nic: Vec::new(),
        };
        spec
    };
    // ③ CTS UDP：3 条流在 1G 上被裁成 2 条。
    let cts = |src: &Endpoint| {
        let mut spec = cts_spec("udp");
        spec.src = src.clone();
        spec.udp_limit = true;
        spec
    };

    for (case, make) in [
        ("路径裁剪", &clipped as &dyn Fn(&Endpoint) -> SpecNorm),
        ("按角色策略", &policed),
        ("CTS 流数", &cts),
    ] {
        let fast = build(make(&fast_src));
        let slow = build(make(&slow_src));
        assert_eq!(fast.len(), 1, "{case}: {fast:?}");
        assert_eq!(slow.len(), 1, "{case}: {slow:?}");
        assert_ne!(fast[0].0, slow[0].0, "{case}: 降速后实际下发的标签应当变了");
        assert_eq!(fast[0].1, slow[0].1, "{case}: 对比标签不能跟着降速变");
        for (profile_label, comparison_label) in fast.iter().chain(&slow) {
            assert_eq!(
                &legacy_parameter(profile_label),
                comparison_label,
                "{case}: 历史标签 {profile_label:?} 归一后应当等于对比标签"
            );
        }
    }
}

/// **稳定 ID / 端口顺序 / 单元展开的全量快照。**
///
/// 这条测试守的是这个仓库里最贵的一条不变量：`Unit.id` 是 RESUME 的
/// identity。它变了，用户所有的历史 PASS 记录当场全部失效——24 小时内本该
/// 跳过的单元会全部重跑，一次 11.5 小时的验收变成两次，而且**没有任何报错**，
/// 只是「怎么又从头跑了」。端口顺序同理：它进 identity，也决定并发资源分配。
///
/// 快照在 `builder_snapshot.txt`，逐字比对：拆分 `builder.rs`（R4）时，任何
/// 一处顺序、拼接、命名的手滑都会让这里逐字段报出差异，而不是等到用户
/// 现场发现 resume 不命中。它只钉身份与端口；命令参数、门限和提示由
/// `the_full_plan_expansion_including_commands_and_targets_is_stable` 钉。
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

/// 计划快照里一条 iperf 流的全部字段。解构是穷举的：给 `IperfTask` 加字段会在
/// 这里编译失败，逼人决定新字段算不算「builder 展开出来的东西」。
fn iperf_task_fingerprint(task: &IperfTask) -> String {
    let IperfTask {
        v6,
        udp,
        profile_name,
        profile_label,
        comparison_label,
        src,
        dst,
        port,
        duration,
        extra,
        stream_idx,
        rate_mode,
        rx_target_mbps,
        offered_per_stream_mbps,
    } = task;
    format!(
        "v6={v6} udp={udp} name={profile_name} label={profile_label} cmp={comparison_label} \
         {}->{} port={port} dur={duration} extra=[{}] idx={stream_idx} mode={rate_mode:?} \
         target={rx_target_mbps:?} offered={offered_per_stream_mbps:?}",
        src.key(),
        dst.key(),
        extra.join(" ")
    )
}

/// 计划快照里的一条腿。端点只记 `Endpoint::key()`，不展开 `NicInfo`：网卡字段的
/// 增减与展开逻辑无关，不该让这份快照红。
fn leg_fingerprint(leg: &Leg) -> String {
    let body = match &leg.kind {
        LegKind::IperfSingle(task) => format!("single {}", iperf_task_fingerprint(task)),
        LegKind::IperfGroup { name, streams } => format!(
            "group {name}\n{}",
            streams
                .iter()
                .map(|task| format!("      {}", iperf_task_fingerprint(task)))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        LegKind::CtsTraffic(task) => {
            let CtsTrafficTask {
                v6,
                udp,
                profile_name,
                profile_label,
                comparison_label,
                src,
                dst,
                port,
                duration,
                streams,
                window_bytes,
                bits_per_second,
                datagram_bytes,
                frame_rate,
                buffer_depth_secs,
                status_update_ms,
                rate_mode,
                rx_target_mbps,
                offered_total_mbps,
                setup_error,
            } = task;
            format!(
                "cts v6={v6} udp={udp} name={profile_name} label={profile_label} \
                 cmp={comparison_label} {}->{} port={port} dur={duration} streams={streams} \
                 window={window_bytes:?} bps={bits_per_second:?} datagram={datagram_bytes:?} \
                 fps={frame_rate} depth={buffer_depth_secs} status={status_update_ms} \
                 mode={rate_mode:?} target={rx_target_mbps:?} offered={offered_total_mbps:?} \
                 setup_error={setup_error:?}",
                src.key(),
                dst.key()
            )
        }
        LegKind::Ping(task) => {
            let PingTask {
                v6,
                src,
                dst,
                count,
                payload,
                purpose,
            } = task;
            format!(
                "ping v6={v6} {}->{} count={count} payload={payload} purpose={purpose:?}",
                src.key(),
                dst.key()
            )
        }
    };
    format!("    leg tag={:?} {body}", leg.tag)
}

/// **展开结果的全量快照：命令参数、速率模式、门限、预览行与计划提示。**
///
/// `the_full_unit_expansion_is_byte_stable` 只钉 RESUME 身份与端口——它红了意味着
/// 用户的历史 PASS 全部失效。这一条钉的是「builder 到底展开出了什么」：每条腿
/// 下发的 `-w/-P/-b/-l`、CTS 的全部参数、判定模式与门限、`target_lines`、
/// 以及提示信息的内容与顺序。两条分开，是因为它们红了的含义不同：这一条在
/// 有意改文案或改参数时就该红，而那一条不该。
///
/// 输入刻意覆盖各个分支：四种后端、三种方向、双栈、空 `-w` 档位、超大 `-w`、
/// 非法流数、非法带宽、路径裁剪、按角色/按网口改写、多流组灌不到门限、合计门限
/// 盖掉逐方向门限、CTS 的非法参数与拓扑门禁、缺 IPv6 与跨 /24 的跳过。
///
/// 重构（R4 拆 `build_units`）时它红了：**不要更新快照**，去找搬错的那一处。
/// 有意改了展开结果：核对差异确实是想要的，再更新 `builder_plan_snapshot.txt`。
#[test]
fn the_full_plan_expansion_including_commands_and_targets_is_stable() {
    let mut tcp = base_spec();
    tcp.name = "plan-tcp".into();
    tcp.directions = vec!["ab".into(), "ba".into(), "bidir".into()];
    tcp.ipvers = vec!["v4".into(), "v6".into()];
    tcp.tcp_windows = vec!["64k".into(), "256m".into()];
    tcp.tcp_streams = 40;
    tcp.duration = 30;
    tcp.rate_targets_single = RateTargets {
        ab: Some(2000.0),
        ..Default::default()
    };
    tcp.rate_targets_bidir = RateTargets {
        ab: Some(900.0),
        ba: Some(900.0),
        ..Default::default()
    };
    tcp.rate_target_bidir_total = Some(1800.0);

    let mut tcp_no_window = base_spec();
    tcp_no_window.name = "plan-tcp-no-window".into();
    tcp_no_window.directions = vec!["bidir".into()];
    tcp_no_window.tcp_windows = Vec::new();
    tcp_no_window.rate_targets_bidir = RateTargets {
        forward: Some(1000.0),
        ..Default::default()
    };

    let mut udp = base_spec();
    udp.name = "plan-udp".into();
    udp.dst = ep(Side::Agent, "eth1", "SGMII1G", "192.168.1.3", 1000);
    udp.directions = vec!["ab".into(), "bidir".into()];
    udp.transports = vec!["udp".into()];
    udp.udp_streams = 4;
    udp.udp_profiles = vec![
        UdpProfile {
            bandwidth: "2500m".into(),
            length: Some("1400".into()),
            window: None,
        },
        UdpProfile::bw("100m"),
        UdpProfile::bw("abc"),
        UdpProfile {
            bandwidth: "300m".into(),
            length: None,
            window: Some("4m".into()),
        },
    ];
    udp.rate_targets = RateTargets {
        forward: Some(900.0),
        ..Default::default()
    };
    udp.link_profiles = LinkProfiles {
        by_role: vec![RoleProfile {
            pair: "SGMII2.5G<->SGMII1G".into(),
            rx_target_mbps: RateTargets {
                ba: Some(800.0),
                ..Default::default()
            },
            udp_bandwidth: DirectionalBandwidth {
                ba: Some("600m".into()),
                ..Default::default()
            },
        }],
        by_nic: vec![NicProfile {
            host: "master".into(),
            name: "eth0".into(),
            ipv4: "192.168.1.2".into(),
            rx_target_percent: Some(90.0),
            udp_length: Some("1200".into()),
            ..Default::default()
        }],
    };

    let mut cts = base_spec();
    cts.name = "plan-cts".into();
    cts.kinds = vec!["ctstraffic".into()];
    cts.transports = vec!["tcp".into(), "udp".into()];
    cts.directions = vec!["ab".into(), "bidir".into()];
    cts.tcp_streams = 2;
    cts.udp_streams = 3;
    cts.tcp_windows = vec!["auto".into(), "1m".into(), "bogus".into()];
    cts.udp_profiles = vec![
        UdpProfile {
            bandwidth: "500m".into(),
            length: Some("1372".into()),
            window: Some("2m".into()),
        },
        UdpProfile::bw("3000m"),
    ];

    let mut cts_blocked = cts.clone();
    cts_blocked.name = "plan-cts-cross-subnet".into();
    cts_blocked.kinds = vec!["cts".into()];
    cts_blocked.directions = vec!["ab".into()];
    cts_blocked.dst = ep(Side::Agent, "eth0", "SGMII2.5G", "192.168.9.3", 2500);

    let mut cts_bad = base_spec();
    cts_bad.name = "plan-cts-invalid".into();
    cts_bad.kinds = vec!["ctstraffic".into()];
    cts_bad.transports = vec!["udp".into()];
    cts_bad.ctstraffic_config_error = Some("duration 超出 ctsTraffic 允许范围".into());
    cts_bad.ctstraffic.udp_frame_rate = 0;

    let mut ping = base_spec();
    ping.name = "plan-ping".into();
    ping.kinds = vec!["ping".into()];
    ping.transports = Vec::new();
    ping.directions = vec!["ab".into(), "bidir".into()];
    ping.ipvers = vec!["v4".into(), "v6".into()];
    ping.payload_sizes = vec![32, 1472];

    let mut no_v6 = base_spec();
    no_v6.name = "plan-no-v6".into();
    no_v6.ipvers = vec!["v6".into()];
    no_v6.dst.nic.ipv6_ll = String::new();

    let mut cross = base_spec();
    cross.name = "plan-cross-subnet".into();
    cross.kinds = vec!["iperf".into(), "ping".into()];
    cross.dst = ep(Side::Agent, "eth0", "SGMII2.5G", "192.168.9.3", 2500);

    let specs = [
        tcp,
        tcp_no_window,
        udp,
        cts,
        cts_blocked,
        cts_bad,
        ping,
        no_v6,
        cross,
    ];
    let mut port = PORT_BASE;
    let (units, notices) = build_units(&specs, true, &mut port);

    let mut lines = Vec::new();
    for unit in &units {
        let Unit {
            id,
            title,
            link_group,
            bidir,
            target_lines,
            bidir_total_target_mbps,
            direction,
            round,
            legs,
            est_secs,
        } = unit;
        lines.push(format!(
            "{id}|{title}|group={link_group:?}|bidir={bidir}|dir={direction}|round={round}\
             |est={est_secs}|total={bidir_total_target_mbps:?}|targets={target_lines:?}"
        ));
        lines.extend(legs.iter().map(leg_fingerprint));
    }
    lines.push("--- notices".into());
    lines.extend(notices.iter().cloned());
    lines.push(format!("--- next_port={port}"));
    let snapshot = lines.join("\n");

    let expected = include_str!("builder_plan_snapshot.txt").replace("\r\n", "\n");
    assert!(
        snapshot.trim_end() == expected.trim_end(),
        "\n计划展开发生了变化（命令参数 / 门限 / 预览行 / 提示之一）。\n\
         重构过程中出现：说明搬运没有保持等价，**不要更新快照**，去找搬错的那一处。\n\
         有意改了展开结果：逐行核对下面的差异，确认后再更新 builder_plan_snapshot.txt。\n\
         {}",
        first_difference(&expected, &snapshot)
    );
}

/// 两份多行文本的第一处差异，前后各带两行上下文。快照有上百行，
/// `assert_eq!` 把两整份字符串转义后并排打出来，人眼找不到差在哪。
fn first_difference(expected: &str, actual: &str) -> String {
    let expected: Vec<&str> = expected.trim_end().lines().collect();
    let actual: Vec<&str> = actual.trim_end().lines().collect();
    let at = expected
        .iter()
        .zip(&actual)
        .position(|(left, right)| left != right)
        .unwrap_or(expected.len().min(actual.len()));
    let from = at.saturating_sub(2);
    let show = |lines: &[&str]| {
        lines
            .iter()
            .enumerate()
            .skip(from)
            .take(5)
            .map(|(index, line)| format!("  {:>4} {line}", index + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // 快照一行能有几百个字符，再指出这一行里从哪个字符开始不同。
    let left: Vec<char> = expected.get(at).copied().unwrap_or("").chars().collect();
    let right: Vec<char> = actual.get(at).copied().unwrap_or("").chars().collect();
    let column = left.iter().zip(&right).take_while(|(a, b)| a == b).count();
    let around = |chars: &[char]| -> String {
        chars
            .iter()
            .skip(column.saturating_sub(20))
            .take(60)
            .collect()
    };
    format!(
        "第 {} 行第 {} 个字符起不同（期望 {} 行，实际 {} 行）\n\
         期望 …{}\n实际 …{}\n期望:\n{}\n实际:\n{}",
        at + 1,
        column + 1,
        expected.len(),
        actual.len(),
        around(&left),
        around(&right),
        show(&expected),
        show(&actual)
    )
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

/// 同一条现场：封顶只能把 1800/2000 压到线速的 95%（950），压不出「这条
/// 链路该验收多少」——16 个单元实测 934~984 就骑在 950 上。真正缺的是
/// 「这条腿是哪一对网口」这一层：SGMII1G 做发送端时收口那个 1800/2000
/// 对本条路径根本不成立，而按网口那张表一块网卡只能填一个数。
/// 双向早就有这一层（`rate_targets_bidir`），单向此前没有。
#[test]
fn a_single_direction_pair_target_outranks_the_per_nic_threshold() {
    let with_nic_policy = || {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "以太网 6", "SGMII1G", "192.168.0.101", 1000);
        spec.dst = ep(Side::Agent, "以太网 18", "SGMII2.5G", "192.168.0.105", 2500);
        spec.rate_mode = RateMode::Verify;
        spec.link_profiles = LinkProfiles {
            by_role: Vec::new(),
            by_nic: vec![NicProfile {
                host: "agent".into(),
                name: "以太网 18".into(),
                ipv4: "192.168.0.105".into(),
                rx_target_mbps: Some(2000.0),
                udp_bandwidth: None,
                ..Default::default()
            }],
        };
        spec
    };

    // 没填单向门限：还是按网口门限，并且被路径上限折算到 950。
    let mut port = PORT_BASE;
    let (units, _) = build_units(&[with_nic_policy()], true, &mut port);
    assert_eq!(iperf_single_task(&units[0]).rx_target_mbps, Some(950.0));

    // 填了就以它为准——按网口那个数对这条路径不成立，不该再参与判定。
    let mut spec = with_nic_policy();
    spec.rate_targets_single.ab = Some(850.0);
    let unit = build_single_iperf_unit(spec, PORT_BASE);
    let task = iperf_single_task(&unit);
    assert_eq!(task.rx_target_mbps, Some(850.0));
    assert_eq!(task.rate_mode, RateMode::Verify);
    // 预览必须说清这个数来自哪一层，否则和界面上那张网口表对不上。
    assert!(
        unit.target_lines
            .iter()
            .any(|line| line.contains("850") && line.contains("单向方向门限")),
        "{:?}",
        unit.target_lines
    );
}

/// 单向与双向各有一套配对门限，互不串台：双向同时灌包时两个方向互相抢，
/// 拿单向那个数去卡双向必然判 RATE_FAIL，反过来则是把双向的宽松值
/// 用到单向上，白放一批本该 FAIL 的链路。
#[test]
fn single_and_bidir_pair_targets_do_not_leak_into_each_other() {
    let mut spec = base_spec();
    spec.directions = vec!["ab".into(), "bidir".into()];
    spec.rate_mode = RateMode::Verify;
    spec.rate_targets_single.ab = Some(1800.0);
    spec.rate_targets_bidir.ab = Some(850.0);
    spec.rate_targets_bidir.ba = Some(850.0);

    let mut port = PORT_BASE;
    let (units, _) = build_units(&[spec], true, &mut port);
    let single = units.iter().find(|unit| !unit.bidir).expect("单向单元");
    assert_eq!(iperf_single_task(single).rx_target_mbps, Some(1800.0));

    let bidir = units.iter().find(|unit| unit.bidir).expect("双向单元");
    for leg in &bidir.legs {
        let LegKind::IperfSingle(task) = &leg.kind else {
            panic!("expected iperf legs");
        };
        assert_eq!(task.rx_target_mbps, Some(850.0), "{} 腿", leg.tag);
    }
}

/// 现场回归：Wi-Fi 协商速率一抖，控制台就再也开不了跑。
///
/// 预览走的是连接时缓存的拓扑，执行端开跑前重新扫描；而 5G 口的 PHY 速率
/// 在相邻两次扫描之间就会跳（本机实测 286 / 2401Mbps 交替）。那个数从
/// 端点、标题、`Unit.id` 三条路漏进 `plan_hash`，于是闸门把每一次带
/// Wi-Fi 口的运行都判成「计划已过期」——两次开跑的 `config_hash` 一模一样，
/// 变的只有单元指纹。
#[test]
fn a_wifi_rate_that_only_moved_the_displayed_number_keeps_the_plan_valid() {
    let wifi_at = |speed: u64| {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "en0", "SGMII1G", "192.168.8.100", 1000);
        spec.dst = ep(Side::Agent, "en1", "WIFI5G", "192.168.8.104", speed);
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        units
    };
    let fast = wifi_at(2401);
    let slow = wifi_at(286);

    // 跑的东西一模一样：Wi-Fi 的负载上限走的是固定档位，不跟协商速率
    // （`rate::nic_payload_ceiling_mbps`），所以 -b 和门限都没变。
    assert_eq!(
        crate::master::plan::units_fingerprint(&fast),
        crate::master::plan::units_fingerprint(&slow),
        "只有显示数字变了，闸门不该拦"
    );

    // 而 RESUME identity **仍然**跟着协商速率走：换了链路速率的 PASS
    // 不该被复用。这两件事分开，是这次归一有意保留的边界。
    assert_ne!(fast[0].id, slow[0].id, "resume identity 仍然记着协商速率");
}

/// 反过来：协商速率**真的**改变了执行内容时，指纹必须变。
///
/// RNDIS 是跟随协商速率裁剪的那一类（见 `rate::nic_payload_ceiling_mbps`
/// 里那条「RNDIS 报什么就按什么裁」），所以同一个 `-b 3G` 在 3750 和 1000
/// 两种协商下会被裁成不同的值——那是跑的东西变了，闸门必须拦。
#[test]
fn a_rate_that_actually_changes_the_offered_load_still_moves_the_fingerprint() {
    let rndis_at = |speed: u64| {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "usb0", "RNDIS", "192.168.9.2", speed);
        spec.dst = ep(Side::Agent, "eth0", "10GETH", "192.168.9.3", 10_000);
        spec.transports = vec!["udp".into()];
        spec.udp_profiles = vec![UdpProfile::bw("3G")];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        units
    };
    assert_ne!(
        crate::master::plan::units_fingerprint(&rndis_at(3750)),
        crate::master::plan::units_fingerprint(&rndis_at(1000)),
        "裁出来的 -b 不同 = 跑的东西不同"
    );
}

/// 反过来：门限在路径之内时一个字节都不能动，提示也不能冒出来。
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
    let legacy_v3_id =
        cts_resume_unit_id_with_schema("ctstraffic_v3", &cts, "V4", "ab", &legacy_units[0].legs);
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
    let legacy_v2_id =
        cts_resume_unit_id_with_schema("ctstraffic_v2", &base, "V4", "ab", &legacy_units[0].legs);
    assert_ne!(
        base_id, legacy_v2_id,
        "CTS P10/rolling coverage 判定上线后必须让 v2 PASS 无条件失效"
    );
    let legacy_v1_id =
        cts_resume_unit_id_with_schema("ctstraffic_v1", &base, "V4", "ab", &legacy_units[0].legs);
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
    // 16/15 而不是 4/3：EVB 的 ab 门限是 6400Mbps，每流 500Mbps 时至少要
    // 14 条并发流才够灌到它。4 条在计划期就会被判成「这一腿的有效判定窗口
    // 永远形不成」并收到提示，而 `build_single_udp_id` 要求一份**干净**的
    // 构建——这条测试问的是 resume identity，不该顺带背上一个不可行的负载。
    // 两个数仍然只差 1，「改了 UDP 流数身份就得变」的判据一个字没动。
    base.udp_streams = 16;
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
    udp_changed.udp_streams = 15;
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
/// **门限优先级的整层剥离**（回归方案 PLAN-08 / PLAN-09）。
///
/// 现有测试逐对验证「A 盖过 B」，但那种两两断言挡不住**重排**：把「按网口」
/// 挪到「单向方向门限」前面，逐对测试里只会红一条，而整条链的形状已经变了。
/// 这里把四层同时钉上去，再从高到低一层层拿掉，断言每一步的胜者和**预览里
/// 印的来源**同时正确——数字对而来源印错，用户在计划页上照样查不出为什么。
///
/// 单向链：单向方向门限 → 按网口 → 任务/全局 → （无）。
/// 双向链：双向合计（本腿只测量） → 双向方向门限 → 按网口 → 任务/全局。
#[test]
fn every_rx_target_layer_yields_to_the_one_above_it() {
    // 路径两端都是 2.5G，让路径上限（0.95 × 2500 = 2375）高于以下所有门限，
    // 免得封顶插进来把「哪一层赢了」搅浑——封顶本身另有测试。
    let stacked = || {
        let mut spec = base_spec();
        spec.src = ep(Side::Master, "以太网 6", "SGMII2.5G", "192.168.0.101", 2500);
        spec.dst = ep(Side::Agent, "以太网 18", "SGMII2.5G", "192.168.0.105", 2500);
        spec.rate_mode = RateMode::Verify;
        // 第 4 层：任务/全局门限。
        spec.rate_targets.ab = Some(400.0);
        spec.rate_targets.ba = Some(400.0);
        // 第 3 层：按网口。
        spec.link_profiles = LinkProfiles {
            by_role: Vec::new(),
            by_nic: vec![NicProfile {
                host: "agent".into(),
                name: "以太网 18".into(),
                ipv4: "192.168.0.105".into(),
                rx_target_mbps: Some(600.0),
                udp_bandwidth: None,
                ..Default::default()
            }],
        };
        // 第 2 层：方向门限（单向一套、双向一套，互不相干）。
        spec.rate_targets_single.ab = Some(800.0);
        spec.rate_targets_bidir.ab = Some(900.0);
        spec.rate_targets_bidir.ba = Some(900.0);
        spec
    };

    let target_of = |spec: SpecNorm, want_bidir: bool| -> (Option<f64>, Vec<String>) {
        let mut spec = spec;
        spec.directions = vec![if want_bidir { "bidir" } else { "ab" }.into()];
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        let unit = units
            .iter()
            .find(|u| u.bidir == want_bidir)
            .expect("应有对应方向的单元");
        let LegKind::IperfSingle(task) = &unit.legs[0].kind else {
            panic!("expect iperf leg");
        };
        (task.rx_target_mbps, unit.target_lines.clone())
    };
    let says = |lines: &[String], number: &str, source: &str| {
        assert!(
            lines
                .iter()
                .any(|l| l.contains(number) && l.contains(source)),
            "计划页要同时说清数字 {number} 和它来自「{source}」，实际：{lines:?}"
        );
    };

    // ---- 单向链 ----
    let (target, lines) = target_of(stacked(), false);
    assert_eq!(target, Some(800.0), "单向：方向门限在最上层");
    says(&lines, "800", "单向方向门限");

    let mut spec = stacked();
    spec.rate_targets_single = Default::default();
    let (target, lines) = target_of(spec, false);
    assert_eq!(target, Some(600.0), "拿掉方向门限，按网口接手");
    says(&lines, "600", "按网口门限");

    let mut spec = stacked();
    spec.rate_targets_single = Default::default();
    spec.link_profiles = LinkProfiles::default();
    let (target, lines) = target_of(spec, false);
    assert_eq!(target, Some(400.0), "再拿掉按网口，落到任务/全局");
    says(&lines, "400", "任务/频段/全局门限");

    let mut spec = stacked();
    spec.rate_targets_single = Default::default();
    spec.link_profiles = LinkProfiles::default();
    spec.rate_targets = Default::default();
    let (target, _) = target_of(spec, false);
    assert_eq!(target, None, "四层都没有就是没有门限，不许凭空造一个");

    // ---- 双向链：双向那一套不受单向门限影响 ----
    let (target, lines) = target_of(stacked(), true);
    assert_eq!(
        target,
        Some(900.0),
        "双向腿只认双向方向门限；单向的 800 不许渗进来"
    );
    says(&lines, "900", "双向方向门限");

    let mut spec = stacked();
    spec.rate_target_bidir_total = Some(1700.0);
    spec.directions = vec!["bidir".into()];
    let mut port = PORT_BASE;
    let (units, _) = build_units(&[spec], true, &mut port);
    let unit = &units[0];
    let LegKind::IperfSingle(task) = &unit.legs[0].kind else {
        panic!("expect iperf leg");
    };
    assert_eq!(
        task.rx_target_mbps, None,
        "配了两端 RX 合计，这一腿就没有自己的门限——否则会出现\
         「AB 判 RATE_FAIL、单元判 PASS」这种自相矛盾的两行"
    );
    // 分工：builder 这一层**只**说「本腿只测量」——逐腿门限本来就不存在，
    // 印一个数字反而是撒谎。合计那个数字是单元级的，由
    // `webui::plan::unit_target_lines` 在预览时补一行
    // 「AB 接收端 RX + BA 接收端 RX ≥ …」，那一层由
    // `wifi_band_thresholds_flow_into_targets_and_show_their_source` 守着。
    // 这里钉住的是分界：本层不许把单元级门限混进逐腿行里。
    says(&unit.target_lines, "本腿只测量", "双向 RX 合计门限");
    assert!(
        !unit.target_lines.iter().any(|line| line.contains("1700")),
        "逐腿行里冒出单元级的合计门限，会读成「这条腿按 1700 判」：{:?}",
        unit.target_lines
    );
    assert_eq!(unit.bidir_total_target_mbps, Some(1700.0));

    let mut spec = stacked();
    spec.rate_targets_bidir = Default::default();
    let (target, lines) = target_of(spec, true);
    assert_eq!(target, Some(600.0), "双向拿掉方向门限后，同样落到按网口");
    says(&lines, "600", "按网口门限");

    // ---- 两条链彼此隔离 ----
    let mut spec = stacked();
    spec.rate_targets_bidir = Default::default();
    spec.link_profiles = LinkProfiles::default();
    spec.rate_targets = Default::default();
    let (bidir_target, _) = target_of(spec.clone(), true);
    assert_eq!(
        bidir_target, None,
        "只填了单向门限时，双向腿必须是「没有门限」——\
         这正是 v6.2.8 分成两套的理由：850/850 的双向不该被 1800 的单向顶掉"
    );
    let (single_target, _) = target_of(spec, false);
    assert_eq!(single_target, Some(800.0), "同一份 spec，单向腿仍然拿得到");
}

/// **流数灌不到门限时，计划期就要说**（回归方案 CFG-07 / PLAN-12）。
///
/// 真机复现（run_20260906_175551，macOS ↔ Arch）：`udp_streams=2`、每流
/// `-b 300m`、门限 850Mbps。执行端要求「所有必需流并发活跃」才算有效判定
/// 窗口，而必需流数 = ceil(850 × 1.05 / 300) = 3 > 2——那个窗口**永远形不成**，
/// 整条腿稳定判 `NOT_EVALUATED / EFFECTIVE_WINDOW_SHORT`。
///
/// 三件事让它比「配错了」更值得挡：①完全确定，不是概率性的；②计划期
/// 已知全部输入，本可以提前算出来；③用户拿到的原因码指向采样窗口，
/// 而真因是流数不够——排查方向被带偏一整层。180s 的 Windows 预设下，
/// 这会让一轮里**每一个** UDP 单元都白跑。
#[test]
fn a_stream_count_that_can_never_reach_the_target_is_called_out_before_the_run() {
    let under_provisioned = |streams: u32, bandwidth: &str, target: f64| {
        let mut spec = base_spec();
        spec.transports = vec!["udp".into()];
        spec.udp_streams = streams;
        spec.udp_profiles = vec![UdpProfile::bw(bandwidth)];
        spec.udp_limit = false;
        spec.rate_mode = RateMode::Verify;
        spec.rate_targets_single.ab = Some(target);
        let mut port = PORT_BASE;
        build_units(&[spec], true, &mut port).1
    };

    // 2 × 300 = 600Mbps，够不到 850（含 5% 余量要 892.5）。
    let notices = under_provisioned(2, "300m", 850.0);
    let hit = notices
        .iter()
        .find(|line| line.contains("灌不到") && line.contains("850"))
        .unwrap_or_else(|| panic!("必须在计划期点名，实际提示：{notices:#?}"));
    assert!(hit.contains("至少要 3 条"), "要说清到底需要几条：{hit}");
    assert!(
        hit.contains("EFFECTIVE_WINDOW_SHORT"),
        "要把「跑完会得到哪个原因码」写出来，否则用户仍然会去查采样窗口：{hit}"
    );

    // 够了就不许聒噪：4 × 300 = 1200 > 892.5。
    let quiet = under_provisioned(4, "300m", 850.0);
    assert!(
        !quiet.iter().any(|line| line.contains("灌不到")),
        "灌得够却还在提示，下次真的不够时没人会看：{quiet:#?}"
    );

    // 没有门限就无从判断够不够，同样不许提示。
    let mut spec = base_spec();
    spec.transports = vec!["udp".into()];
    spec.udp_streams = 2;
    spec.udp_profiles = vec![UdpProfile::bw("300m")];
    spec.udp_limit = false;
    let mut port = PORT_BASE;
    let (_, notices) = build_units(&[spec], true, &mut port);
    assert!(
        !notices.iter().any(|line| line.contains("灌不到")),
        "没有门限时无从比较，不该提示：{notices:#?}"
    );
}

/// **凡是能改变「跑什么」或「按什么判」的输入，resume 身份都必须跟着变**
/// （回归方案 RES-02）。
///
/// 现有覆盖是**两两**的：合计门限一条、双向门限一条、2.4G 上限一条……
/// 那种写法挡不住「**忘了接**」——新加一层门限、或者给 `SpecNorm` 添一个
/// 影响下发的字段，逐对测试一条都不会红，而身份里少了它。少一个字段的后果
/// 不是「多跑一轮」，是**改完配置重跑，整批单元被 SKIP 掉，报告里沿用上一
/// 次的 PASS**——用户看到的是「我改的东西没生效」，而没有任何地方说为什么。
///
/// 所以这里按「改一格 → 身份必须变 / 必须不变」逐项扫，两侧都钉。
#[test]
fn every_input_that_changes_what_runs_or_how_it_is_judged_changes_the_resume_id() {
    // 单向 UDP 基线：这条路上四层门限、负载、判定模式全都走得到。
    let baseline = || {
        let mut spec = evb_udp_spec();
        spec.directions = vec!["ab".into()];
        spec.udp_streams = 16;
        spec.rate_mode = RateMode::Verify;
        spec.rate_targets_single.ab = Some(800.0);
        spec.rate_targets.ab = Some(400.0);
        spec
    };
    /// 「这一格改成什么样」——一句话说明 + 改法。
    type Tweak = (&'static str, Box<dyn Fn(&mut SpecNorm)>);

    // 不复用 `build_single_udp_id`：它要求一份不带任何提示的构建，而这里有
    // 几格（协商速率、余量）本来就会顺带触发计划期提示。提示与身份是两件事。
    let id = |spec: SpecNorm| {
        let mut port = PORT_BASE;
        let (units, _) = build_units(&[spec], true, &mut port);
        assert_eq!(units.len(), 1, "这一组夹具每次只该生成一个单元");
        units[0].id.clone()
    };
    let base_id = id(baseline());

    // ---- 改了必须失效 ----
    let must_change: Vec<Tweak> = vec![
        (
            "单向方向门限（本腿的实际验收线）",
            Box::new(|s: &mut SpecNorm| s.rate_targets_single.ab = Some(801.0)),
        ),
        (
            "通用门限（单向门限拿掉后就是它说了算）",
            Box::new(|s: &mut SpecNorm| {
                s.rate_targets_single = Default::default();
                s.rate_targets.ab = Some(401.0);
            }),
        ),
        (
            "按网口门限（同样能改出本腿的验收线）",
            Box::new(|s: &mut SpecNorm| {
                s.rate_targets_single = Default::default();
                s.rate_targets = Default::default();
                s.link_profiles = LinkProfiles {
                    by_role: Vec::new(),
                    by_nic: vec![NicProfile {
                        host: "agent".into(),
                        name: "10g".into(),
                        ipv4: "192.168.1.3".into(),
                        rx_target_mbps: Some(1234.0),
                        ..Default::default()
                    }],
                };
            }),
        ),
        (
            "判定模式（verify 与 observe 是两种结论）",
            Box::new(|s: &mut SpecNorm| s.rate_mode = RateMode::Observe),
        ),
        (
            "每流负载 -b",
            Box::new(|s: &mut SpecNorm| s.udp_profiles = vec![UdpProfile::bw("501m")]),
        ),
        (
            "报文长度 -l",
            Box::new(|s: &mut SpecNorm| {
                s.udp_profiles = vec![UdpProfile {
                    bandwidth: "500m".into(),
                    length: Some("1200".into()),
                    window: None,
                }]
            }),
        ),
        ("并发流数", Box::new(|s: &mut SpecNorm| s.udp_streams = 15)),
        ("时长", Box::new(|s: &mut SpecNorm| s.duration += 1)),
        (
            "方向",
            Box::new(|s: &mut SpecNorm| s.directions = vec!["ba".into()]),
        ),
        (
            "链路身份：发送口换了一块网卡",
            Box::new(|s: &mut SpecNorm| {
                s.src = ep(Side::Master, "usb2", "10GUSB", "192.168.1.4", 4200)
            }),
        ),
        (
            "链路身份：协商速率变了（裁流与封顶都吃它）",
            Box::new(|s: &mut SpecNorm| {
                s.dst = ep(Side::Agent, "10g", "10GETH", "192.168.1.3", 1000)
            }),
        ),
        (
            "判定口径本身：采样间隔",
            Box::new(|s: &mut SpecNorm| s.rate_check.sample_interval_ms += 1),
        ),
        (
            "判定口径本身：最低并发比例",
            Box::new(|s: &mut SpecNorm| s.rate_check.min_active_ratio = 0.5),
        ),
        (
            "判定口径本身：灌包余量",
            Box::new(|s: &mut SpecNorm| s.rate_check.offered_headroom_pct = 10.0),
        ),
    ];
    for (what, mutate) in must_change {
        let mut spec = baseline();
        mutate(&mut spec);
        assert_ne!(
            base_id,
            id(spec),
            "改了「{what}」，resume 身份却没变——改完配置重跑会整批 SKIP，\
             报告里沿用上一次的 PASS，而用户只会看到「我改的东西没生效」"
        );
    }

    // ---- 只改显示项，身份不许动 ----
    // 反面同样重要：身份平白失效意味着 resume 名存实亡，每次都全量重跑，
    // 而这个功能存在的理由就是别再跑一遍已经过了的。
    let must_not_change: Vec<Tweak> = vec![
        (
            "测试项名字",
            Box::new(|s: &mut SpecNorm| s.name = "换个名字".into()),
        ),
        (
            "报告里的链路分组标签",
            Box::new(|s: &mut SpecNorm| s.link_group = "有线 ↔ 有线".into()),
        ),
        (
            "双向门限（本腿是单向，它一次都不会被查）",
            Box::new(|s: &mut SpecNorm| {
                s.rate_targets_bidir.ab = Some(999.0);
                s.rate_targets_bidir.ba = Some(999.0);
            }),
        ),
        (
            "双向 RX 合计门限（同上，只对双向腿成立）",
            Box::new(|s: &mut SpecNorm| s.rate_target_bidir_total = Some(1700.0)),
        ),
    ];
    for (what, mutate) in must_not_change {
        let mut spec = baseline();
        mutate(&mut spec);
        assert_eq!(
            base_id,
            id(spec),
            "只改了「{what}」，resume 身份却失效了——每次都全量重跑，\
             resume 就名存实亡"
        );
    }

    // 双向腿是另一条链：那两层在这里必须**反过来**生效。
    let bidir = || {
        let mut spec = baseline();
        spec.directions = vec!["bidir".into()];
        spec.rate_targets_bidir.ab = Some(900.0);
        spec.rate_targets_bidir.ba = Some(900.0);
        spec
    };
    let bidir_id = id(bidir());
    let mut changed = bidir();
    changed.rate_targets_bidir.ab = Some(901.0);
    assert_ne!(bidir_id, id(changed), "双向腿上，双向方向门限必须进身份");
    let mut changed = bidir();
    changed.rate_target_bidir_total = Some(1700.0);
    assert_ne!(
        bidir_id,
        id(changed),
        "合计门限一配，这个单元就改成「按两端 RX 相加判一次」——\
         判定口径整个换了，绝不能复用逐方向那一次的 PASS"
    );
    // **两个不同的合计值之间**也必须分得开。配了合计，逐腿门限就统一变成
    // `None`（本腿只测量），于是 1700 和 1800 在逐腿身份上**一个字节都不差**
    // ——只有合计那一项自己能区分。少了它，把合计从 1700 调到 1800 重跑，
    // 整批单元会拿 1700 那次的 PASS 顶上来。
    let mut at_1700 = bidir();
    at_1700.rate_target_bidir_total = Some(1700.0);
    let mut at_1800 = bidir();
    at_1800.rate_target_bidir_total = Some(1800.0);
    let (a, b) = (id(at_1700), id(at_1800));
    assert_ne!(
        a, b,
        "合计门限 1700 与 1800 的 resume 身份相同。配了合计后逐腿门限都是 None，\
         逐腿身份分不出这两者，必须靠合计自己进身份"
    );

    let mut changed = bidir();
    changed.rate_targets_single.ab = Some(1.0);
    assert_eq!(
        bidir_id,
        id(changed),
        "双向腿不读单向门限，改它不该让身份失效"
    );
}

/// 灌包命令是**逐参数**下发的，不经过 shell（回归方案 SEC-05 后半）。
///
/// `extra` 里每一项都是一个独立的 argv 元素，最终交给
/// `Command::new(bin).args(&args)`——中间没有 shell。所以 `;` `&&` `$()`
/// 反引号这些只在 shell 里才有意义的字符，到 iperf3 手上就是普通字符，
/// 它会因为「这不是个合法的带宽写法」而拒绝，而不是执行什么。
///
/// 走的是**配置**这条路而不是控制台：`check_udp_bandwidth` 会把界面上填的
/// 怪写法挡在门外，但项目文件里的 `master_config.iperf.udp_profiles` 是整块
/// 原样搬运的，能把任意字符串送到这里。
///
/// 这条钉住的是**结构**而不是某几个字符：只要还有人把 extra 拼成一个字符串
/// 再交出去（为了打日志、为了展示命令行），注入面立刻就回来。所以断言的是
/// 「一个值 = 一个元素，原样，不拆不拼」。
#[test]
fn traffic_arguments_are_separate_argv_entries_never_a_shell_string() {
    let build = |bandwidth: &str, length: Option<&str>, window: Option<&str>| {
        let mut spec = base_spec();
        spec.transports = vec!["udp".into()];
        spec.udp_streams = 1;
        spec.udp_limit = false;
        spec.udp_profiles = vec![UdpProfile {
            bandwidth: bandwidth.into(),
            length: length.map(String::from),
            window: window.map(String::from),
        }];
        let mut port = PORT_BASE;
        build_units(&[spec], true, &mut port).0
    };
    let first_task = |units: &[Unit]| {
        units
            .iter()
            .flat_map(|u| u.legs.iter())
            .find_map(|leg| match &leg.kind {
                LegKind::IperfSingle(task) => Some(task.clone()),
                LegKind::IperfGroup { streams, .. } => streams.first().cloned(),
                _ => None,
            })
    };

    // `-b` 解析不出来时**整条单元都不产生**：那个字符串到不了命令行。
    assert!(
        first_task(&build("1m; rm -rf / #", None, None)).is_none(),
        "带宽解析不出来却仍排出了灌包腿——那个字符串会一路走到 argv 上"
    );

    // `-l` / `-w` 不参与数值解析，会原样下发；它们必须各自是**一个**元素。
    let units = build("1m", Some("1200 && whoami"), Some("256k; id"));
    let task = first_task(&units).expect("合法带宽下应有一条 iperf 腿");

    for needle in ["&& whoami", "; id"] {
        let hits: Vec<&String> = task
            .extra
            .iter()
            .filter(|arg| arg.contains(needle))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "{needle:?} 应当整体留在**单个** argv 元素里，实际 extra = {:?}",
            task.extra
        );
    }
    assert!(
        !task.extra.iter().any(|arg| arg.trim().is_empty()),
        "不许出现空参数——空元素是「按空白切过」的痕迹：{:?}",
        task.extra
    );
    // flag 与值必须成对且顺序正确：值跑到 flag 位置上是另一种注入。
    // `-b` 是**解析后重新生成**的（`1m` → `1000000`），原串一个字节都不留；
    // `-l` / `-w` 不做数值解析，原样透传，但各自只占**一个** argv 元素。
    // 后者意味着怪写法会走到 iperf3 手上并被它拒绝——那是执行期报错，不是
    // 注入；代价只是这一腿要跑起来才失败，而不是计划期就挡下。
    for (flag, value) in [
        ("-b", "1000000"),
        ("-l", "1200 && whoami"),
        ("-w", "256k; id"),
    ] {
        let idx = task
            .extra
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag} 应当下发：{:?}", task.extra));
        assert_eq!(
            task.extra[idx + 1],
            value,
            "{flag} 的值必须原样、单元素地紧跟其后：{:?}",
            task.extra
        );
    }
}

/// **裁流的两个边界：速率未知时不裁，一条腿灌不动时整个单元不排**
/// （回归方案 PLAN-05）。
///
/// 逐腿裁剪、Wi-Fi 固定档、RNDIS/NCM/10GUSB 的特例各自已有测试，这里补的是
/// 两端：
///
/// **① 速率未知不许裁，但也不许因此放过已知的那端。**
/// `path_payload_ceiling_mbps` 只在**两端都**问不出上限时才返回 `None`；
/// 一端已知就用已知那端——它仍然是实打实的物理约束。
/// 两端都未知时唯一正确的做法是照请求发：按一个猜出来的上限裁，等于用工具的
/// 猜测替换掉操作者的配置，失败方向还是「灌得比要求的少、却按原门限判」，
/// 直接制造假 FAIL。
///
/// **② 单流就灌不动时，整个单元不许排。** CTS 那条路上流数会算到 0
/// （iperf 那条路 v4.3.0 起改成压 `-b` 而不是跳过，见
/// `test_udp_over_path_ceiling_clips_bandwidth_instead_of_skipping`）。
/// 0 流必须让**整个单元消失**，而不是留下一个没有腿的空单元——空单元会
/// 进计数、进报告、占一行，却什么都没跑。
#[test]
fn an_unknown_link_speed_is_never_clipped_and_a_leg_that_cannot_run_drops_the_unit() {
    let cfg = RateCheckCfg::default();

    // ---- ① 未知速率 ----
    // 角色不在那张表里、协商速率又拿不到（macOS/Linux 扫不到 PHY 速率是常态）。
    let unknown = ep(Side::Master, "eth9", "MYSTERY", "192.168.1.9", 0);
    let known = ep(Side::Agent, "eth0", "SGMII1G", "192.168.1.3", 1000);
    let requested = UdpProfile::bw("2.6G").parsed_bandwidth().expect("合法带宽");

    // 两端都未知 → 整条路径无从裁起，照请求发。
    assert!(
        crate::rate::path_payload_ceiling_mbps(&unknown.nic, &unknown.nic, &cfg).is_none(),
        "两端都未知时，路径上限就不该被猜出来"
    );
    let load = udp_load_for_leg(&unknown, &unknown, requested, 4, true, false, &cfg);
    assert_eq!(
        (load.mbps, load.streams, load.clipped_from_mbps),
        (requested.mbps, 4, None),
        "两端速率都未知却裁了：按猜出来的上限灌、再按原门限判，就是配置出来的 FAIL"
    );

    // 一端已知 → **用已知那端**，不因为另一端未知就整条放行。
    // 这是「不误裁」的真实边界：不猜未知的那端，也不放过已知的那端。
    assert_eq!(
        crate::rate::path_payload_ceiling_mbps(&unknown.nic, &known.nic, &cfg),
        Some(1000.0),
        "一端未知不该让整条路径失去上限——已知那端仍然是物理约束"
    );
    let half_known = udp_load_for_leg(&unknown, &known, requested, 4, true, false, &cfg);
    assert!(
        half_known.clipped_from_mbps.is_some() || half_known.streams < 4,
        "已知那端是 1G，2.6G 却原样放行：{half_known:?}"
    );

    // 反面：两端都已知时必须裁，否则上面那条只是「从来不裁」。
    let clipped = udp_load_for_leg(&known, &known, requested, 4, true, false, &cfg);
    assert!(
        clipped.clipped_from_mbps.is_some() || clipped.streams < 4,
        "1G 路径上灌 2.6G 却原样放行：{clipped:?}"
    );

    // ---- ② 一条腿排不出来，整个单元不许留 ----
    // CTS UDP 那条路上单流超过路径上限时流数算到 0。
    let mut spec = base_spec();
    spec.src = ep(Side::Master, "eth0", "SGMII1G", "192.168.1.2", 1000);
    spec.dst = ep(Side::Agent, "eth0", "SGMII1G", "192.168.1.3", 1000);
    spec.kinds = vec!["ctstraffic".into()];
    spec.transports = vec!["udp".into()];
    spec.udp_streams = 4;
    spec.udp_limit = true;
    spec.udp_profiles = vec![UdpProfile {
        bandwidth: "2.6G".into(),
        length: Some("1200".into()),
        window: None,
    }];
    let mut port = PORT_BASE;
    let (units, notices) = build_units(&[spec], true, &mut port);
    // 注：`legs.clear()` 那一步在当前结构下够不到——路径上限是
    // `min(两端)`，对称，所以一个单元的两条腿永远同进同退，第一条排不出来时
    // `legs` 本来就是空的。它是防御性的，不是本条测试证到的东西。
    // 真正证到的是下面两条：0 流检查在，且跳过会说一声。
    assert!(
        units.iter().all(|unit| !unit.legs.is_empty()),
        "留下了一个没有腿的空单元：它会进计数、进报告、占一行，却什么都没跑"
    );
    assert!(
        units.is_empty(),
        "单流就超过路径上限时整个单元都该消失，实际排出了 {} 个",
        units.len()
    );
    assert!(
        notices.iter().any(|line| line.contains("跳过")),
        "跳过必须说一声，否则用户只会发现「少跑了几个」：{notices:#?}"
    );
}

/// **端口游标的回绕**（回归方案 PLAN-03）。
///
/// `alloc_port` 是 `wrapping_add(1).max(PORT_BASE)`。两个细节都要钉：
///
/// 1. `wrapping_add` 而不是 `+`：u16 加到 65535 再 +1 会 **panic**（debug）
///    或悄悄归零（release）。长计划真的会走到这里。
/// 2. `.max(PORT_BASE)` 而不是让它从 0 开始：0..56000 里全是别人的地盘
///    （系统服务、iperf3 默认的 5201、被测设备自己的管理口）。分到那段上
///    的表现不是「端口冲突」这么直白，而是 iperf3 绑不上、或者更糟——
///    绑上了一个**别人正在用**的端口，测出来的数里混着别人的流量。
#[test]
fn the_port_cursor_wraps_back_into_the_test_range_never_into_system_ports() {
    let step = |from: u16| {
        let mut next = from;
        let issued = alloc_port(&mut next);
        (issued, next)
    };

    assert_eq!(step(PORT_BASE), (PORT_BASE, PORT_BASE + 1));
    assert_eq!(step(65_534), (65_534, 65_535));
    // 回绕：下一个必须是 PORT_BASE，不是 0、也不是 1。
    assert_eq!(
        step(65_535),
        (65_535, PORT_BASE),
        "端口游标回绕到了测试区间之外——0..{PORT_BASE} 里全是别人的地盘，\
         绑上一个别人正在用的端口意味着测出来的数里混着别人的流量"
    );

    // 连续分配一整圈：每一个都必须落在 [PORT_BASE, 65535] 之内。
    let mut next = 65_530;
    for _ in 0..20 {
        let port = alloc_port(&mut next);
        assert!(
            port >= PORT_BASE,
            "分出了 {port}，低于测试区间下界 {PORT_BASE}"
        );
    }
}

/// 一个计划之内不许出现重复端口（回归方案 PLAN-03「同时活跃任务不碰撞」）。
///
/// 双向、多流、多档位叠加时端口是逐个游标分出去的；重复的后果是两个并发
/// 任务抢同一个监听口——先起的那个占住，后起的报「address in use」，而它
/// 在报表里会显示成一次**灌包失败**。
#[test]
fn one_plan_never_hands_out_the_same_port_twice() {
    let mut specs = Vec::new();
    for idx in 0..8 {
        let mut spec = base_spec();
        spec.name = format!("t{idx}");
        spec.directions = vec!["ab".into(), "ba".into(), "bidir".into()];
        spec.transports = vec!["tcp".into(), "udp".into()];
        spec.udp_streams = 4;
        spec.tcp_streams = 4;
        spec.udp_profiles = vec![UdpProfile::bw("100m"), UdpProfile::bw("200m")];
        spec.udp_limit = false;
        specs.push(spec);
    }
    let mut next = PORT_BASE;
    let (units, _) = build_units(&specs, true, &mut next);
    assert!(units.len() > 8, "夹具要足够大才测得到碰撞");

    let mut ports: Vec<u16> = Vec::new();
    for unit in &units {
        for leg in &unit.legs {
            match &leg.kind {
                LegKind::IperfSingle(task) => ports.push(task.port),
                LegKind::IperfGroup { streams, .. } => {
                    ports.extend(streams.iter().map(|task| task.port))
                }
                _ => {}
            }
        }
    }
    let unique: std::collections::HashSet<u16> = ports.iter().copied().collect();
    assert_eq!(
        unique.len(),
        ports.len(),
        "同一个计划里分出了重复端口（{} 个里只有 {} 个不同）——\
         两个并发任务抢同一个监听口，后起的那个会在报表里显示成一次灌包失败",
        ports.len(),
        unique.len()
    );
}

/// **同 /24 门禁只挡 iperf，绝不挡 ping**（回归方案 PLAN-04）。
///
/// 两端 IPv4 不在同一个 /24 时直连灌包起不来，跳过 iperf 是对的。但 ping
/// **必须照跑**——它正是那个能告诉你「到底通不通、通到什么程度」的东西。
/// 连 ping 一起挡掉的话，用户拿到的是一个空单元和一句「不同网段」，既不知道
/// 链路是死的还是活的，也没有 RTT 可以对比。跳过提示里那句「（ping 不受限）」
/// 就是这个承诺，而在此之前没有任何测试钉着它。
///
/// 另外两条边界一并钉住：门禁只对**跨机**成立（同机两块网卡不受限），
/// 以及 IPv6 不走这道门（v6 有自己的可用性判断）。
#[test]
fn the_same_subnet_gate_blocks_iperf_but_never_ping() {
    let build = |require_same_subnet: bool, cross: bool, v6: bool| {
        let mut spec = base_spec();
        spec.kinds = vec!["iperf".into(), "ping".into()];
        spec.transports = vec!["tcp".into()];
        spec.ipvers = vec![if v6 { "v6" } else { "v4" }.into()];
        spec.src = ep(Side::Master, "eth0", "SGMII1G", "192.168.1.2", 1000);
        // 不同 /24。
        spec.dst = ep(
            if cross { Side::Agent } else { Side::Master },
            "eth1",
            "SGMII1G",
            "10.9.9.3",
            1000,
        );
        let mut port = PORT_BASE;
        build_units(&[spec], require_same_subnet, &mut port)
    };
    let kinds = |units: &[Unit]| {
        let mut has_iperf = false;
        let mut has_ping = false;
        for unit in units {
            for leg in &unit.legs {
                match &leg.kind {
                    LegKind::Ping(_) => has_ping = true,
                    LegKind::IperfSingle(_) | LegKind::IperfGroup { .. } => has_iperf = true,
                    _ => {}
                }
            }
        }
        (has_iperf, has_ping)
    };

    // 跨机 + 不同 /24 + 门禁开：iperf 挡掉，ping 必须还在。
    let (units, notices) = build(true, true, false);
    assert_eq!(
        kinds(&units),
        (false, true),
        "同 /24 门禁把 ping 也挡掉了。那一行于是只剩一句「不同网段」——\
         链路是死是活、RTT 多少，全都无从知道"
    );
    assert!(
        notices.iter().any(|line| line.contains("ping 不受限")),
        "跳过 iperf 时要说清 ping 仍然会跑：{notices:#?}"
    );

    // 门禁关掉：iperf 照排（反面，否则上面那条只证明了「iperf 从来不排」）。
    assert_eq!(kinds(&build(false, true, false).0), (true, true));

    // 同机两块网卡：门禁只对跨机成立。
    assert_eq!(kinds(&build(true, false, false).0), (true, true));
}

/// **第 1 轮的稳定身份必须逐字节不变。**
///
/// 轮次是新功能，不该让任何历史 `task_results.json` 的 RESUME 命中失效。
/// 不加轮次（或写 `repeats: 1`）的老计划，跑出来的身份和以前一模一样。
#[test]
fn round_one_keeps_the_exact_identity_it_had_before_rounds_existed() {
    let mut port = PORT_BASE;
    let (plain, _) = build_units(&[base_spec()], true, &mut port);
    let mut port = PORT_BASE;
    let (once, _) = build_units_repeated(&[base_spec()], true, &mut port, 1);
    assert_eq!(plain.len(), once.len());
    for (a, b) in plain.iter().zip(once.iter()) {
        assert_eq!(a.id, b.id, "第 1 轮的稳定 ID 不许变");
        assert_eq!(a.title, b.title, "只跑一轮时标题也不许变");
    }
}

/// 第 2 轮起必须有**独立**身份。
///
/// 端口不进身份，所以同一份计划展开两遍拿到的是同一个 id。不区分的话，
/// 第 2 轮会直接命中第 1 轮刚写进去的 PASS 而整轮跳过——「连跑 N 遍看有没有
/// 偶发」这个功能本身就被取消掉了。
#[test]
fn later_rounds_get_their_own_identity_so_they_do_not_resume_onto_round_one() {
    let mut port = PORT_BASE;
    let (units, notices) = build_units_repeated(&[base_spec()], true, &mut port, 3);
    let mut port_once = PORT_BASE;
    let (single, _) = build_units(&[base_spec()], true, &mut port_once);
    let per_round = single.len();
    assert_eq!(units.len(), per_round * 3, "整份计划重复三遍");

    let ids: HashSet<&str> = units.iter().map(|u| u.id.as_str()).collect();
    assert_eq!(ids.len(), units.len(), "所有单元的身份必须两两不同");

    // 轮次在**最外层**：前 N 个是第 1 轮，接着 N 个是第 2 轮。
    assert!(units[0].title.ends_with("第 1 轮"));
    assert!(units[per_round].title.ends_with("第 2 轮"));
    assert!(units[per_round * 2].title.ends_with("第 3 轮"));
    // 同一条测试在各轮之间除了身份和标题应当完全一致。
    assert_eq!(units[0].link_group, units[per_round].link_group);
    assert_eq!(units[0].legs.len(), units[per_round].legs.len());
    assert!(notices.iter().any(|n| n.contains("稳定性轮次")));
}

/// 派生必须是**确定性**的：今天的第 3 轮要能命中昨天的第 3 轮。
#[test]
fn round_identities_are_stable_across_runs() {
    let ids = |port_start: u16| {
        let mut port = port_start;
        build_units_repeated(&[base_spec()], true, &mut port, 4)
            .0
            .into_iter()
            .map(|u| u.id)
            .collect::<Vec<_>>()
    };
    // 端口起点不同也不该改变身份——端口本来就不进身份。
    assert_eq!(ids(PORT_BASE), ids(PORT_BASE + 500));
}

/// 上限是防手滑：一次全量跑 11.5 小时，输错一位就是一个月。
#[test]
fn the_round_count_is_clamped_on_both_ends() {
    let mut port = PORT_BASE;
    let (zero, _) = build_units_repeated(&[base_spec()], true, &mut port, 0);
    let mut port_one = PORT_BASE;
    let (one, _) = build_units(&[base_spec()], true, &mut port_one);
    assert_eq!(zero.len(), one.len(), "0 当成 1 处理，不许展开成空计划");

    let mut port = PORT_BASE;
    let (many, _) = build_units_repeated(&[base_spec()], true, &mut port, MAX_ROUNDS + 50);
    assert_eq!(many.len(), one.len() * MAX_ROUNDS as usize);
}

/// `ip` 的别名展开成它说的那个版本，而不是第二份 IPv4。
///
/// 展开只问「是不是 `v6`」，以前 `"ipv6"` / `"6"` / 大写 `"V6"`（`pairs` 路径连小写
/// 都不做）都会被当成 IPv4：ID 相同的单元出现两份、两份都跑，报告里只有 V4，
/// 而计划里一句提示都没有。写法表与控制台共用 `canonical_ip_version`。
#[test]
fn ip_aliases_expand_to_the_version_they_name_instead_of_a_second_ipv4() {
    for aliases in [vec!["v4", "ipv6"], vec!["IPv4", "6"], vec!["4", "V6", "v6"]] {
        let mut spec = base_spec();
        spec.ipvers = aliases.iter().map(|value| value.to_string()).collect();
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        let titles: Vec<&str> = units.iter().map(|unit| unit.title.as_str()).collect();
        assert_eq!(units.len(), 2, "{aliases:?}: {titles:?}");
        assert!(titles[0].contains(" V4 "), "{aliases:?}: {titles:?}");
        assert!(titles[1].contains(" V6 "), "{aliases:?}: {titles:?}");
        assert_ne!(units[0].id, units[1].id, "{aliases:?}");
        assert!(notices.is_empty(), "{aliases:?}: {notices:?}");
    }
}

/// 认不出的 `ip` / `kinds` / `transports` 取值要说出来，而不是整类静默不生成。
///
/// 配置文件 `tests[]` 与 `pairs` 两条路都不校验这三个字段，`"kinds": ["iperf3"]`
/// 以前得到的是一个空计划和零条提示。
#[test]
fn unrecognised_axis_values_are_reported_instead_of_silently_dropped() {
    let mut spec = base_spec();
    spec.name = "typo".into();
    spec.kinds = vec!["iperf3".into(), "ping".into()];
    spec.transports = vec!["tcp".into(), "sctp".into()];
    spec.ipvers = vec!["v4".into(), "v5".into()];
    let mut port = PORT_BASE;
    let (units, notices) = build_units(&[spec.clone()], true, &mut port);
    // iperf3 不是别名：只剩 ping。
    assert!(
        units.iter().all(|unit| unit.title.starts_with("PING")),
        "{:?}",
        units.iter().map(|unit| &unit.title).collect::<Vec<_>>()
    );
    for expected in [
        "typo：kinds 取值 \"iperf3\" 无法识别，已忽略（可选 iperf / ctstraffic / ping）",
        "typo：ip 取值 \"v5\" 无法识别，已忽略（可选 v4 / v6）",
    ] {
        assert!(
            notices.iter().any(|notice| notice == expected),
            "{notices:?}"
        );
    }
    // 只剩 ping 时传输协议用不上，不为它提示。
    assert!(
        !notices.iter().any(|notice| notice.contains("transports")),
        "{notices:?}"
    );

    // 有灌包后端时，认不出的传输协议同样要说，认得出的照常展开。
    spec.kinds = vec!["iperf".into()];
    let mut port = PORT_BASE;
    let (units, notices) = build_units(&[spec], true, &mut port);
    assert!(!units.is_empty() && units.iter().all(|unit| unit.title.contains(" TCP ")));
    assert!(
        notices
            .iter()
            .any(|notice| notice
                == "typo：transports 取值 \"sctp\" 无法识别，已忽略（可选 tcp / udp）"),
        "{notices:?}"
    );
}

/// 门限按协商速率百分比换算时，四种吞吐后端都要把算式说出来。
///
/// 这段提示以前只在两条 iperf 路径上调用：同一个 `rx_target_percent`，iperf 单元
/// 的计划提示里写着「2500Mbps × 80% = 2000Mbps」，CTS 单元按同一个 2000 判定，却
/// 一句来历都没有——Wi-Fi 重新协商后门限变了，只配了 CTS 的人看不出为什么。
#[test]
fn a_percentage_derived_target_is_explained_for_every_traffic_backend() {
    for (kind, transport) in [
        ("iperf", "tcp"),
        ("iperf", "udp"),
        ("ctstraffic", "tcp"),
        ("ctstraffic", "udp"),
    ] {
        let mut spec = base_spec();
        spec.kinds = vec![kind.into()];
        spec.transports = vec![transport.into()];
        spec.link_profiles = LinkProfiles {
            by_role: Vec::new(),
            by_nic: vec![NicProfile {
                host: "agent".into(),
                name: "eth0".into(),
                ipv4: "192.168.1.3".into(),
                rx_target_percent: Some(80.0),
                ..Default::default()
            }],
        };
        let mut port = PORT_BASE;
        let (units, notices) = build_units(&[spec], true, &mut port);
        assert_eq!(units.len(), 1, "{kind}/{transport}");
        let target = match &units[0].legs[0].kind {
            LegKind::IperfSingle(task) => task.rx_target_mbps,
            LegKind::IperfGroup { streams, .. } => streams[0].rx_target_mbps,
            LegKind::CtsTraffic(task) => task.rx_target_mbps,
            LegKind::Ping(_) => None,
        };
        assert_eq!(target, Some(2000.0), "{kind}/{transport}");
        assert_eq!(
            notices,
            vec!["t：接收口 eth0 门限按协商速率换算：2500Mbps × 80% = 2000Mbps".to_string()],
            "{kind}/{transport}"
        );
    }
}

/// 同一句计划提示只说一遍，不管有多少个单元会把它再算出来。
///
/// 流数非法、`-w` 排空、路径裁剪这类提示按 方向 × IP 版本 × 档位 各算一遍，
/// 以前原样重复：命令行逐条打印，控制台逐条列出，一份普通计划里同一句话能出现
/// 十次。命令行（一次展开）和控制台（逐条规格展开再汇总）两条路都要去重。
#[test]
fn a_notice_is_said_once_however_many_units_repeat_it() {
    let mut spec = base_spec();
    spec.directions = vec!["ab".into(), "ba".into(), "bidir".into()];
    spec.ipvers = vec!["v4".into(), "v6".into()];
    spec.tcp_streams = 40;
    let expected =
        "t 的 iperf TCP 流数配置非法，将按兼容范围使用 32 流: TCP streams 必须在 1..=32，当前为 40（来源 tcp_streams）";

    let mut port = PORT_BASE;
    let (units, notices) = build_units(&[spec.clone()], true, &mut port);
    assert_eq!(units.len(), 6);
    assert_eq!(notices, vec![expected.to_string()]);

    let mut other = spec.clone();
    other.dst = ep(Side::Agent, "eth1", "SGMII2.5G", "192.168.1.4", 2500);
    let mut port = PORT_BASE;
    let plan = build_ui_units_repeated(&[spec, other], true, &mut port, 1);
    assert_eq!(plan.units.len(), 12);
    assert_eq!(plan.notices, vec![expected.to_string()]);
}
