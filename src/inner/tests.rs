use super::*;
use crate::verdict::Verdict;

fn example() -> InnerConfig {
    config::parse_config(include_str!("../../inner.example.json")).unwrap()
}

#[test]
fn board_inventory_falls_back_to_proc_and_address_interfaces_without_sysfs() {
    let interfaces = adb::board_interfaces(
        "",
        "34: br0 inet 192.168.8.1/24 scope global",
        "br0: 100 1 2 3 4 5 6 7 200 9 10 11 12 13 14 15",
    );
    let (name, source) = adb::resolve_rx_interface("", "br0", &interfaces).unwrap();
    assert_eq!(name, "br0");
    assert_eq!(source, CounterSource::ProcNetDev);
    assert_eq!(interfaces[0].addresses, vec!["192.168.8.1/24"]);
}

#[test]
fn tool_capable_strategies_do_not_require_readable_nic_counters() {
    let mut link = example().links[1].clone();
    let mut cap = capability(&link, "agent1");
    for iface in &mut cap.board_interfaces {
        iface.proc_counters = false;
        iface.sysfs_counters = false;
    }
    link.measurement = Measurement::NicStrict;
    assert!(preflight_link(&link, &cap).is_err());
    for strategy in [Measurement::Tool, Measurement::NicPreferred] {
        link.measurement = strategy;
        assert!(preflight_link(&link, &cap)
            .unwrap()
            .counter_source
            .is_none());
        let samples = Sampler::Unavailable("counter unavailable".into())
            .stop()
            .unwrap();
        assert!(samples.samples.is_empty());
        assert_eq!(samples.errors, vec!["counter unavailable"]);
    }
}

fn assembled_bidir(strategy: Measurement, down_start: u64, total: bool) -> UnitRow {
    use crate::protocol::{IperfEventKind, MonitorSample};
    let mut cfg = example();
    cfg.links.truncate(1);
    cfg.protocols = vec![Protocol::Tcp];
    cfg.tcp_streams = Some(1);
    cfg.duration_secs = 20;
    cfg.directions = vec![Direction::Bidir];
    cfg.links[0].measurement = strategy;
    cfg.links[0].upload_min_mbps = Some(800.0);
    cfg.links[0].download_min_mbps = Some(800.0);
    cfg.links[0].bidir_total_min_mbps = total.then_some(1600.0);
    if strategy != Measurement::NicStrict {
        cfg.links[0].tool_upload_min_mbps = Some(800.0);
        cfg.links[0].tool_download_min_mbps = Some(800.0);
        cfg.links[0].tool_bidir_total_min_mbps = total.then_some(1600.0);
    }
    let plan = plan::build(&cfg).unwrap();
    let unit = &plan.units[0];
    let raw = unit
        .legs
        .iter()
        .enumerate()
        .map(|(index, leg)| {
            let start = if index == 0 { 3_000 } else { down_start };
            LegRaw {
                plan: leg.clone(),
                receiver: "test".into(),
                receiver_host: "test".into(),
                counter_source: None,
                client: IperfClientOut {
                    ok: true,
                    output: SINGLE.into(),
                    ..Default::default()
                },
                events: vec![
                    IperfFlowEvent {
                        kind: IperfEventKind::Started,
                        elapsed_ms: start,
                        ..Default::default()
                    },
                    IperfFlowEvent {
                        kind: IperfEventKind::Ended,
                        elapsed_ms: start + 20_000,
                        ..Default::default()
                    },
                ],
                samples: MonitorStopOut {
                    samples: (1..=20)
                        .map(|second| MonitorSample {
                            elapsed_ms: start + second * 1000,
                            interval_ms: 1000,
                            rx_mbps: 900.0,
                            rx_delta_bytes: 112_500_000,
                            valid: true,
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                },
                server_log: String::new(),
            }
        })
        .collect();
    assemble_unit(
        &cfg,
        &cfg.links[0],
        unit,
        &LinkPreflight {
            board_iface: "br0".into(),
            counter_source: Some(CounterSource::ProcNetDev),
            addresses: TrafficAddresses::ipv4(&cfg.links[0]),
        },
        raw,
    )
}

#[test]
fn disjoint_bidirectional_legs_never_produce_an_acceptance_or_total() {
    for strategy in [
        Measurement::NicStrict,
        Measurement::NicPreferred,
        Measurement::Tool,
    ] {
        for total in [false, true] {
            let row = assembled_bidir(strategy, 24_000, total);
            assert_eq!(row.verdict, "NOT_EVALUATED", "{strategy:?}, {total}");
            assert_eq!(row.total_mbps, None);
            assert!(row
                .legs
                .iter()
                .all(|leg| leg.verdict == "NOT_EVALUATED" && leg.mbps.is_none()));
            let aligned = assembled_bidir(strategy, 3_000, total);
            assert_eq!(aligned.verdict, "PASS", "{strategy:?}, {total}");
        }
    }
}

#[test]
fn tool_whole_run_summaries_cannot_be_added_as_partial_overlap_rates() {
    let row = assembled_bidir(Measurement::Tool, 13_000, true);
    assert_eq!(row.overlap_secs, Some(10.0));
    assert_eq!(row.verdict, "NOT_EVALUATED");
    assert_eq!(row.total_mbps, None);
    assert!(row.legs.iter().all(|leg| leg.tool.receiver_mbps.is_some()));
    let nic = assembled_bidir(Measurement::NicStrict, 13_000, true);
    assert_eq!(nic.verdict, "PASS", "可信重叠 RX 达标仍然通过");
}

#[test]
fn directional_aggregate_keeps_the_reason_of_the_selected_verdict() {
    let mut failed = sample_leg(Flow::Up, Source::Nic, 100.0);
    failed.verdict = "RATE_FAIL".into();
    failed.reason = "RX_BELOW_TARGET".into();
    let result = aggregate(
        &[failed, sample_leg(Flow::Down, Source::Nic, 900.0)],
        &crate::verdict::VerdictResult::measured(
            crate::reason::ReasonCode::TargetUnknown,
            "no total",
        ),
    );
    assert_eq!(result.verdict, Verdict::RateFail);
    assert_eq!(result.code, crate::reason::ReasonCode::RxBelowTarget);
}

/// v1 的内环项目文件，用来钉住迁移。字段就是升级前那一代的样子：
/// 单个 `protocol`、`board_interface`、没有 enabled / measurement / repeats。
const V1_PROJECT: &str = r#"{
  "kind": "cpe-inner-project",
  "version": 1,
  "config": {
    "adb_path": "adb",
    "board_iperf": "iperf3",
    "duration_secs": 20,
    "parallel": 1,
    "protocol": "tcp",
    "directions": ["upload", "download"],
    "port": 56190,
    "links": [
      {
        "name": "ETH",
        "host": "master",
        "local_interface": "以太网",
        "local_ip": "192.168.8.100",
        "gateway": "192.168.8.1",
        "board_interface": "br0",
        "upload_min_mbps": 800.0,
        "download_min_mbps": 700.0
      }
    ],
    "agents": []
  }
}"#;

fn capability(link: &Link, host: &str) -> Capability {
    let nic = crate::protocol::NicInfo {
        name: link.local_interface.clone(),
        ipv4: link.local_ip.to_string(),
        ..Default::default()
    };
    let info = crate::protocol::HostInfo {
        interfaces: vec![nic],
        ..Default::default()
    };
    let counters =
        "br0: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16\neth1: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16";
    Capability {
        serial: "test".into(),
        board_version: String::new(),
        board_addresses: "34: br0 inet 192.168.8.1/24 scope global".into(),
        board_counters: counters.into(),
        board_interfaces: adb::board_interfaces(
            "br0||10|20\neth1|br0|1|2\nlo||0|0",
            "34: br0 inet 192.168.8.1/24 scope global",
            counters,
        ),
        board_inventory_error: None,
        local: if host == "master" {
            info.clone()
        } else {
            crate::protocol::HostInfo::default()
        },
        agents: vec![AgentCapability {
            id: "agent1".into(),
            status: HostStatus::Ready,
            error: None,
            info: (host != "master").then_some(info),
        }],
    }
}

// ---------------- 配置、迁移与兼容 ----------------

#[test]
fn inner_projects_reject_subnet_configs_and_keep_remote_hosts_without_exporting_tokens() {
    assert!(config::parse_config(r#"{"project_version":1}"#)
        .unwrap_err()
        .contains("子网"));
    // 子网的裸配置（控制台下载的 config.json、手写的那份）没有
    // `project_version`，只认那一个键的话，报出来的是 `deny_unknown_fields`
    // 的一串字段名，半个字不提「导错地方了」。子网侧认内环认三种特征，
    // 内环这边也得认得出子网，两边才对称。
    let minimal = config::parse_config(include_str!("../../config.minimal.json")).unwrap_err();
    assert!(minimal.contains("子网"), "{minimal}");
    for body in [
        r#"{"agent_host":"192.168.1.3"}"#,
        r#"{"iperf":{"duration":180}}"#,
        r#"{"tests":[],"pairs":"all"}"#,
        r#"{"abort_after_dead_traffic_units":3,"ctstraffic":{}}"#,
    ] {
        let error = config::parse_config(body).unwrap_err();
        assert!(error.contains("子网"), "{body} → {error}");
    }
    // 反过来不能误伤：内环自己的配置一个都不许被当成子网的。
    config::parse_config(&serde_json::to_string(&example()).unwrap())
        .expect("内环配置不能被子网护栏误伤");
    let mut cfg = example();
    cfg.agents[0].token = "private-agent-token".into();
    let encoded = serde_json::to_string(&cfg).unwrap();
    assert!(!encoded.contains("private-agent-token"));
    let roundtrip = config::parse_config(&encoded).unwrap();
    assert_eq!(roundtrip.links[1].host, "agent1");
    cfg.links[0].host = "missing".into();
    assert!(cfg.validate().is_err());
    cfg.links[0].host = "master".into();
    cfg.agents[0].id = "master".into();
    assert!(cfg.validate().is_err());
}

#[test]
fn inner_agent_addresses_reject_control_characters() {
    let mut cfg = example();
    cfg.agents[0].address = "agent.example\u{0000}".into();
    assert!(config::parse_config(&serde_json::to_string(&cfg).unwrap())
        .unwrap_err()
        .contains("地址和端口"));
    cfg.agents[0].address = "agent.example\nbackup".into();
    assert!(config::parse_config(&serde_json::to_string(&cfg).unwrap())
        .unwrap_err()
        .contains("地址和端口"));
}

#[test]
fn inner_agent_http_fields_have_reasonable_wire_size_limits() {
    let mut cfg = example();
    cfg.agents[0].address = "a".repeat(257);
    assert!(config::parse_config(&serde_json::to_string(&cfg).unwrap())
        .unwrap_err()
        .contains("地址和端口"));
    cfg.agents[0].address = "agent.example".into();
    cfg.agents[0].token = "t".repeat(4097);
    assert!(cfg.validate().unwrap_err().contains("地址和端口"));
}

#[test]
fn inner_agent_addresses_are_trimmed_before_http_use() {
    let mut cfg = example();
    cfg.agents[0].address = "  agent.example  ".into();
    let parsed = config::parse_config(&serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(parsed.agents[0].address, "agent.example");
}

/// 候选清单去重要认不相邻的重复。
#[test]
fn rx_candidates_never_lists_the_same_interface_twice() {
    // 两级桥：br0 既是网关口，又出现在自己的成员链里（成员 eth1 的 master
    // 指回 br0）。拼出来是 [br0, eth1, br0]，两个 br0 不相邻，dedup() 放过。
    let interfaces = adb::board_interfaces(
        "br0|br0|10|20\neth1|br0|1|2",
        "34: br0 inet 192.168.8.1/24 scope global",
        "",
    );
    let candidates = adb::rx_candidates("br0", &interfaces);
    let mut sorted = candidates.clone();
    sorted.sort();
    let before = sorted.len();
    sorted.dedup();
    assert_eq!(before, sorted.len(), "候选清单里有重复项: {candidates:?}");
}

/// `adb_path` 是唯一会被当程序执行的配置字段，得管住它指向什么。
#[test]
fn adb_path_must_point_at_adb_so_a_config_cannot_pick_the_program_to_run() {
    let with_path = |path: &str| {
        let mut cfg = example();
        cfg.adb_path = path.into();
        cfg.validate()
    };
    // 位置随便放，Windows 的空格和反斜杠都要能用。
    for good in [
        "adb",
        "./adb",
        "/usr/local/bin/adb",
        "adb.exe",
        "C:\\Program Files\\platform-tools\\adb.exe",
        "platform-tools/adb-1.0.41",
    ] {
        assert!(with_path(good).is_ok(), "应放行: {good}");
    }
    // 校验放行的值，必须就是最后拿去执行的值：带首尾空格的路径（从表格里
    // 粘出来的常见样子）不能校验时 trim、执行时不 trim。
    let mut padded = example();
    padded.adb_path = "  /nonexistent/adb  ".into();
    assert!(padded.validate().is_ok(), "带空格的路径应能通过校验");
    // 不依赖机器上真有 adb：只看**报错里出现的是哪个字符串**。带空格说明
    // 拿去执行的还是未 trim 的原串，校验过的值和执行的值就是两回事。
    let error = Adb::connect(&padded).err().expect("路径不存在，必然失败");
    assert!(
        error.contains("/nonexistent/adb") && !error.contains("  /nonexistent/adb  "),
        "执行的必须是 trim 之后的那个值，实际: {error}"
    );
    // 换成别的程序就不行——这正是带着 UI 口令 POST /api/inner/probe 时
    // 能让主控执行任意可执行文件的那条路。
    for bad in [
        "/usr/bin/curl",
        "/bin/sh",
        "C:\\Windows\\System32\\cmd.exe",
        "",
        "   ",
        "-adb",
        "adb\nrm -rf /",
    ] {
        assert!(with_path(bad).is_err(), "应拒绝: {bad:?}");
    }
}

#[test]
fn v1_projects_migrate_without_inventing_a_bidirectional_test_or_a_new_verdict_rule() {
    let migrated = config::parse_config(V1_PROJECT).unwrap();
    assert_eq!(
        migrated.protocols,
        vec![Protocol::Tcp],
        "protocol → protocols"
    );
    assert_eq!(
        migrated.directions,
        vec![Direction::Upload, Direction::Download],
        "v1 的先上行后下行是两个独立单向单元，绝不能折成 bidir"
    );
    assert!(!migrated.directions.contains(&Direction::Bidir));
    let link = &migrated.links[0];
    assert_eq!(link.board_rx_interface, "br0", "board_interface → 采样接口");
    assert!(link.enabled, "v1 没有勾选概念，迁移后全部参与");
    assert_eq!(
        link.measurement,
        Measurement::NicStrict,
        "升级不能顺手把旧配置换成另一套验收规则"
    );
    assert_eq!(migrated.repeats, 1);
    // 展开后仍是两个单向单元，各一条腿。
    let plan = plan::build(&migrated).unwrap();
    assert_eq!(plan.unit_count(), 2);
    assert_eq!(plan.leg_count(), 2);
    assert_eq!(plan.bidir_units(), 0);

    // 未来版本拒绝导入，且不改当前配置——parse 只返回 Err，从不就地修补。
    let future = V1_PROJECT.replace("\"version\": 1", "\"version\": 99");
    assert!(config::parse_config(&future).unwrap_err().contains("99"));
    // protocol 与 protocols 同时出现时不猜用哪个。
    let both = V1_PROJECT.replace(
        "\"protocol\": \"tcp\",",
        "\"protocol\": \"tcp\", \"protocols\": [\"udp\"],",
    );
    assert!(config::parse_config(&both).is_err());
}

#[test]
fn config_rejects_unknown_keys_and_unsafe_remote_words() {
    example().validate().unwrap();
    assert!(serde_json::from_str::<InnerConfig>(r#"{"duraton_secs":20}"#).is_err());
    for bad in ["iperf3; reboot", "$(reboot)", "'", "-s", "\niperf3", ""] {
        let mut cfg = example();
        cfg.board_iperf = bad.into();
        assert!(cfg.validate().is_err(), "{bad:?}");
    }
    for bad in ["board serial", "-board", "board;serial"] {
        let mut cfg = example();
        cfg.serial = bad.into();
        assert!(cfg.validate().is_err(), "{bad:?}");
    }
    for good in ["iperf3", "/usr/bin/iperf3", "/data/local/tmp/iperf3"] {
        assert!(config::safe_word(good));
    }
    // 板侧统计接口要拼进 /sys/class/net/<接口>/…，比 safe_word 更窄。
    for bad in ["../../etc", "br0/x", "", "-br0", "."] {
        assert!(!config::iface_word(bad), "{bad:?}");
        let mut cfg = example();
        cfg.links[0].board_rx_interface = bad.into();
        assert!(cfg.validate().is_err() || bad.is_empty(), "{bad:?}");
    }
    for good in ["br0", "eth1", "wlan0.2", "rai0"] {
        assert!(config::iface_word(good), "{good}");
    }
}

#[test]
fn config_does_not_silently_accept_invalid_thresholds_or_udp_load() {
    for value in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let mut cfg = example();
        cfg.links[0].upload_min_mbps = Some(value);
        assert!(cfg.validate().is_err());
    }
    let mut cfg = example();
    cfg.protocols = vec![Protocol::Udp];
    assert!(cfg.validate().is_err(), "UDP 必须给出每流速率");
    cfg.udp_mbps = Some(100.0);
    cfg.validate().unwrap();
    cfg.links.push(cfg.links[0].clone());
    assert!(cfg.validate().is_err());
}

#[test]
fn tool_thresholds_and_bidir_thresholds_are_separate_knobs_that_cannot_be_silently_dead() {
    let mut cfg = example();
    // 严格模式永远走不到工具口径，配了工具门限就是配了个永不生效的数。
    cfg.links[0].tool_upload_min_mbps = Some(500.0);
    assert!(cfg.validate().unwrap_err().contains("工具口径"));
    cfg.links[0].measurement = Measurement::NicPreferred;
    cfg.validate().unwrap();
    // 没勾双向就不接受双向合计门限。
    cfg.links[0].bidir_total_min_mbps = Some(900.0);
    assert!(cfg.validate().unwrap_err().contains("双向"));
    cfg.directions.push(Direction::Bidir);
    cfg.validate().unwrap();
    // 网卡门限和工具门限各走各的，互不推导。
    let link = &cfg.links[0];
    assert_eq!(link.leg_target(Flow::Up, true), Some(500.0));
    assert_eq!(link.leg_target(Flow::Up, false), None);
    assert_eq!(link.total_target(false), Some(900.0));
    assert_eq!(link.total_target(true), None);
}

#[test]
fn one_run_covers_both_protocols_and_keeps_per_protocol_knobs_with_their_protocol() {
    let mut cfg = example();
    cfg.protocols = vec![Protocol::Tcp, Protocol::Udp];
    cfg.udp_mbps = Some(500.0);
    cfg.validate().unwrap();
    assert_eq!(
        plan::build(&cfg).unwrap().unit_count(),
        cfg.links.len() * 4,
        "两协议 × 两方向"
    );

    // 选了两种协议就必须两种都跑；不能悄悄只跑第一种。
    let link = &cfg.links[0];
    for protocol in [Protocol::Tcp, Protocol::Udp] {
        let req = client_request(&cfg, link, protocol, Flow::Up, cfg.port);
        assert_eq!(req.udp, protocol == Protocol::Udp);
        iperf::check_client_extra(&req).unwrap();
        assert_eq!(
            req.extra.contains(&"-b".into()),
            protocol == Protocol::Udp,
            "-b 只属于 UDP"
        );
    }

    // 重复选择是配置错误，不是「去重后继续」——跑两遍同一档只会让报告多一行假数据。
    cfg.protocols = vec![Protocol::Tcp, Protocol::Tcp];
    assert!(cfg.validate().is_err());
    cfg.protocols = vec![Protocol::Tcp];
    cfg.directions = vec![Direction::Upload, Direction::Upload];
    assert!(cfg.validate().is_err());
    cfg.directions = vec![Direction::Upload];
    cfg.udp_mbps = None;
    cfg.validate().unwrap();
    assert_eq!(plan::build(&cfg).unwrap().unit_count(), cfg.links.len());
    cfg.directions.clear();
    assert!(cfg.validate().is_err(), "一个方向都不选跑不出任何结果");
}

#[test]
fn per_protocol_knobs_are_validated_and_never_leak_into_the_other_protocol() {
    let mut cfg = example();
    cfg.tcp_streams = Some(4);
    cfg.tcp_window = Some("4m".into());
    cfg.validate().unwrap();
    assert_eq!(cfg.streams(Protocol::Tcp), 4);
    assert_eq!(
        cfg.streams(Protocol::Udp),
        cfg.parallel,
        "留空沿用 parallel"
    );
    let req = client_request(&cfg, &cfg.links[0], Protocol::Tcp, Flow::Up, cfg.port);
    iperf::check_client_extra(&req).unwrap();
    assert_eq!(req.extra, vec!["-P", "4", "-w", "4m"], "TCP 档位只带 -P/-w");

    // UDP 专属的档位不能配在只测 TCP 的一轮里，否则填了也不生效却没有任何提示。
    for mutate in [
        (|c: &mut InnerConfig| c.udp_length = Some("1400".into())) as fn(&mut InnerConfig),
        |c: &mut InnerConfig| c.max_udp_loss_pct = Some(1.0),
    ] {
        let mut only_tcp = example();
        mutate(&mut only_tcp);
        assert!(only_tcp.validate().is_err());
    }
    let mut only_udp = example();
    only_udp.protocols = vec![Protocol::Udp];
    only_udp.udp_mbps = Some(100.0);
    only_udp.udp_length = Some("1400".into());
    only_udp.udp_streams = Some(8);
    only_udp.max_udp_loss_pct = Some(1.0);
    only_udp.validate().unwrap();
    let req = client_request(
        &only_udp,
        &only_udp.links[0],
        Protocol::Udp,
        Flow::Down,
        only_udp.port,
    );
    iperf::check_client_extra(&req).unwrap();
    assert_eq!(req.extra, vec!["-P", "8", "-b", "100M", "-l", "1400"]);
    only_udp.tcp_window = Some("4m".into());
    assert!(
        only_udp.validate().is_err(),
        "不测 TCP 时 tcp_window 无处生效"
    );

    // -w / -l 原样进命令行，只放行数字加可选 k/m/g。
    for bad in [
        "4mb",
        "-4m",
        "4 m",
        "",
        "0",
        "4m;ls",
        "1e3",
        "99999999999999",
    ] {
        let mut cfg = example();
        cfg.tcp_window = Some(bad.into());
        assert!(cfg.validate().is_err(), "{bad}");
    }
    for good in ["64k", "4m", "1G", "1400", "128"] {
        let mut cfg = example();
        cfg.tcp_window = Some(good.into());
        cfg.validate().unwrap();
    }
    for bad in [Some(0), Some(17)] {
        let mut cfg = example();
        cfg.tcp_streams = bad;
        assert!(cfg.validate().is_err());
    }
    let mut loss = example();
    loss.protocols = vec![Protocol::Udp];
    loss.udp_mbps = Some(100.0);
    for bad in [-0.1, 100.1, f64::NAN] {
        loss.max_udp_loss_pct = Some(bad);
        assert!(loss.validate().is_err(), "{bad}");
    }
}

// ---------------- 计划 ----------------

#[test]
fn an_unchecked_link_keeps_its_config_but_stops_gating_the_round() {
    let mut cfg = example();
    // agent1 那条链路不参与本轮：辅测机就算离线也不该拦住本机测试。
    cfg.links[1].enabled = false;
    let plan = plan::build(&cfg).unwrap();
    assert_eq!(plan.links, vec![0], "只跑勾选的网口");
    assert!(plan.agents.is_empty(), "没被引用的辅测机不进连接门禁");
    assert!(plan.uses_master);
    assert_eq!(cfg.links.len(), 2, "取消勾选不删配置");
    assert!(cfg.referenced_agents().is_empty());

    // 一条 agent 都没配也照样能跑本机。
    let mut alone = example();
    alone.links.remove(1);
    alone.agents.clear();
    let plan = plan::build(&alone).unwrap();
    assert_eq!(plan.unit_count(), 2);
    assert!(plan.agents.is_empty());

    // 重新勾上，辅测机就回到门禁里。
    cfg.links[1].enabled = true;
    assert_eq!(
        plan::build(&cfg).unwrap().agents,
        vec!["agent1".to_string()]
    );
}

#[test]
fn the_plan_is_the_only_cartesian_product_and_keeps_the_user_order() {
    let mut cfg = example();
    cfg.protocols = vec![Protocol::Tcp, Protocol::Udp];
    cfg.udp_mbps = Some(500.0);
    cfg.directions = vec![Direction::Upload, Direction::Download, Direction::Bidir];
    cfg.repeats = 2;
    let plan = plan::build(&cfg).unwrap();
    // 网口 → 协议 → 方向 → 轮次，外层永远是网口。
    assert_eq!(plan.unit_count(), 2 * 2 * 3 * 2);
    assert_eq!(plan.leg_count(), 2 * 2 * 4 * 2, "双向单元是两条腿");
    assert_eq!(plan.bidir_units(), 2 * 2 * 2);
    let order: Vec<(&str, &str)> = plan
        .units
        .iter()
        .map(|unit| (unit.link_name.as_str(), unit.protocol.label()))
        .collect();
    assert_eq!(order[0], ("ETH", "TCP"));
    assert_eq!(order[order.len() - 1], ("Wi-Fi", "UDP"));
    assert!(
        order[..12].iter().all(|(link, _)| *link == "ETH"),
        "第一条网口的全部单元跑完才轮到下一条：{order:?}"
    );
    // 预览、执行和报告消费的是同一份，条数必须一致。
    let preview = plan::preview(&cfg).unwrap();
    assert_eq!(preview.units, plan.unit_count());
    assert_eq!(preview.legs, plan.leg_count());
    assert_eq!(preview.bidir_units, plan.bidir_units());
    assert_eq!(preview.rows.len(), preview.units);
    assert!(preview.estimated_secs >= cfg.duration_secs * preview.units as u64);
}

#[test]
fn inner_resume_identity_ignores_order_and_port_but_tracks_measurement_inputs() {
    let mut cfg = example();
    cfg.links.truncate(1);
    cfg.agents.clear();
    cfg.directions = vec![Direction::Upload];
    let first = plan::build(&cfg).unwrap().units[0].id.clone();
    cfg.port += 10;
    let moved_port = plan::build(&cfg).unwrap().units[0].id.clone();
    assert_eq!(first, moved_port, "临时端口变化不应清空内环 RESUME");
    cfg.duration_secs += 1;
    let changed_duration = plan::build(&cfg).unwrap().units[0].id.clone();
    assert_ne!(first, changed_duration, "实际测量时长变化必须失效旧 PASS");
    cfg.duration_secs -= 1;
    cfg.links[0].upload_min_mbps = Some(123.0);
    let changed_target = plan::build(&cfg).unwrap().units[0].id.clone();
    assert_ne!(first, changed_target, "验收门限变化必须失效旧 PASS");
    cfg.links[0].upload_min_mbps = None;
    cfg.serial = "another-board".into();
    let changed_board = plan::build(&cfg).unwrap().units[0].id.clone();
    assert_ne!(first, changed_board, "切换 ADB 设备必须失效旧 PASS");
    cfg.serial.clear();
    cfg.agents.push(config::AgentConfig {
        id: "agent1".into(),
        address: "192.168.8.201".into(),
        port: 28801,
        token: String::new(),
    });
    cfg.links[0].host = "agent1".into();
    let changed_host = plan::build(&cfg).unwrap().units[0].id.clone();
    cfg.agents[0].address = "192.168.8.202".into();
    let changed_agent = plan::build(&cfg).unwrap().units[0].id.clone();
    assert_ne!(changed_host, changed_agent, "切换辅测机地址必须失效旧 PASS");
    cfg.links.reverse();
    let reordered = plan::build(&cfg).unwrap().units[0].id.clone();
    assert_eq!(changed_agent, reordered, "仅改变列表位置不应改变身份");
}

#[test]
fn resumed_tool_unit_keeps_the_tool_total_threshold_in_its_report_row() {
    let mut cfg = example();
    cfg.links.truncate(1);
    cfg.agents.clear();
    cfg.directions = vec![Direction::Bidir];
    cfg.links[0].measurement = Measurement::Tool;
    cfg.links[0].bidir_total_min_mbps = Some(900.0);
    cfg.links[0].tool_bidir_total_min_mbps = Some(700.0);

    let unit = plan::build(&cfg).unwrap().units.remove(0);
    let row = resumed_row(&unit);
    assert_eq!(row.verdict, "PASS");
    assert_eq!(row.total_target_mbps, Some(700.0));
}

#[test]
fn resume_can_finish_without_touching_the_test_device_when_every_unit_is_fresh() {
    let mut cfg = example();
    cfg.links.truncate(1);
    cfg.agents.clear();
    cfg.directions = vec![Direction::Upload];
    let plan = plan::build(&cfg).unwrap();
    let mut resumed = std::collections::HashSet::new();
    assert!(!all_units_resumed(&plan.units, &resumed));
    resumed.insert(plan.units[0].id.clone());
    assert!(all_units_resumed(&plan.units, &resumed));
    assert!(cfg.serial.is_empty(), "测试覆盖空 serial 的自动选择路径");
    assert!(!can_skip_before_device_identification(
        &cfg,
        &plan.units,
        &resumed
    ));
    cfg.serial = "board-serial".into();
    assert!(can_skip_before_device_identification(
        &cfg,
        &plan.units,
        &resumed
    ));
    assert!(
        !all_units_resumed(&[], &resumed),
        "空计划不是一次完整 RESUME"
    );
}

#[cfg(unix)]
#[test]
fn inner_history_rejects_symlinked_report_and_config_files() {
    let root = std::path::Path::new(super::RUNS_ROOT);
    std::fs::create_dir_all(root).unwrap();
    let dir = root.join(format!("inner_symlink_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", dir.join("report.html")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", dir.join("config.json")).unwrap();
    let body = serde_json::json!({"id": dir.file_name().unwrap()}).to_string();
    assert!(history::report(&body).is_err());
    assert!(history::config(&body).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bidirectional_unit_is_one_unit_with_two_concurrent_legs_on_two_ports() {
    let mut cfg = example();
    cfg.links.truncate(1);
    cfg.agents.clear();
    cfg.directions = vec![Direction::Bidir];
    let plan = plan::build(&cfg).unwrap();
    assert_eq!(plan.unit_count(), 1, "双向是一个单元，不是两个");
    let unit = &plan.units[0];
    assert_eq!(unit.legs.len(), 2);
    assert_eq!(unit.legs[0].flow, Flow::Up);
    assert_eq!(unit.legs[1].flow, Flow::Down);
    assert_ne!(
        unit.legs[0].port, unit.legs[1].port,
        "两条腿同时在跑，共用端口会把两股流灌进同一个 server"
    );
    assert_eq!(unit.legs[1].port, cfg.port + 1);

    // 两次顺序单向和一次双向并发是不同的计划，条数就不一样。
    let mut sequential = cfg.clone();
    sequential.directions = vec![Direction::Upload, Direction::Download];
    let sequential = plan::build(&sequential).unwrap();
    assert_eq!(sequential.unit_count(), 2);
    assert_eq!(sequential.bidir_units(), 0);
    assert!(sequential.units.iter().all(|unit| unit.legs.len() == 1));

    // 端口加一越界必须提前挡下，而不是在板侧撞端口。
    let mut edge = cfg.clone();
    edge.port = u16::MAX;
    assert!(edge.validate().is_err());
}

#[test]
fn the_preview_says_which_side_receives_and_which_threshold_applies() {
    let mut cfg = example();
    cfg.links.truncate(1);
    cfg.agents.clear();
    cfg.links[0].measurement = Measurement::NicPreferred;
    cfg.links[0].upload_min_mbps = Some(800.0);
    cfg.links[0].tool_upload_min_mbps = Some(750.0);
    cfg.directions = vec![Direction::Upload];
    let preview = plan::preview(&cfg).unwrap();
    let row = &preview.rows[0];
    assert!(row.legs[0].receiver.contains("板侧"), "上行的接收端在板侧");
    assert_eq!(row.legs[0].nic_target_mbps, Some(800.0));
    assert_eq!(row.legs[0].tool_target_mbps, Some(750.0));
    assert!(row.verdict_basis.contains("750.000"));

    cfg.directions = vec![Direction::Download];
    let preview = plan::preview(&cfg).unwrap();
    assert!(
        preview.rows[0].legs[0].receiver.contains("以太网"),
        "下行的接收端是网口所在电脑"
    );

    // 双向没配合计门限时，明说按逐方向判定、不折半。
    cfg.directions = vec![Direction::Bidir];
    let preview = plan::preview(&cfg).unwrap();
    assert!(preview.rows[0].verdict_basis.contains("不由单向门限折半"));
    cfg.links[0].bidir_total_min_mbps = Some(1500.0);
    let preview = plan::preview(&cfg).unwrap();
    assert!(preview.rows[0].verdict_basis.contains("1500.000"));

    // 没勾选的网口要写在预览里，让人知道它只是没参与、配置还在。
    let mut skipped = example();
    skipped.links[1].enabled = false;
    let preview = plan::preview(&skipped).unwrap();
    assert_eq!(preview.skipped, vec!["Wi-Fi".to_string()]);
}

// ---------------- 板侧适配与采样 ----------------

#[test]
fn distributed_links_validate_against_their_own_pc_not_the_master_inventory() {
    let cfg = example();
    let link = &cfg.links[1];
    let capability = capability(link, "agent1");
    let resolved = preflight_link(link, &capability).unwrap();
    assert_eq!(resolved.board_iface, "br0");
    let mut wrong_pc = link.clone();
    wrong_pc.host = "master".into();
    assert!(preflight_link(&wrong_pc, &capability).is_err());
}

#[test]
fn the_board_statistics_interface_is_decoupled_from_the_lan_address_owner() {
    let cfg = example();
    let link = &cfg.links[1];
    let capability = capability(link, "agent1");
    // 留空 → 按 LAN 地址归属识别成 br0。
    assert_eq!(
        preflight_link(link, &capability).unwrap().board_iface,
        "br0"
    );
    // 明确指定桥成员 eth1：它不持有 192.168.8.1，但计数可读，就该被接受。
    let mut member = link.clone();
    member.board_rx_interface = "eth1".into();
    let resolved = preflight_link(&member, &capability).unwrap();
    assert_eq!(resolved.board_iface, "eth1");
    assert_eq!(resolved.counter_source, Some(CounterSource::ProcNetDev));
    // 板侧根本没有的接口要提前报错，而不是跑完一轮拿回一堆 NOT_EVALUATED。
    let mut missing = link.clone();
    missing.board_rx_interface = "eth9".into();
    assert!(preflight_link(&missing, &capability)
        .unwrap_err()
        .contains("eth9"));
    // LAN 地址本身仍必须唯一归属某个板侧接口——它是 server 的 bind 目标。
    let mut wrong_gateway = link.clone();
    wrong_gateway.gateway = "10.0.0.1".parse().unwrap();
    assert!(preflight_link(&wrong_gateway, &capability).is_err());
}

#[test]
fn the_inventory_lists_bridge_members_and_never_adds_two_interfaces_together() {
    let inventory =
        "br0||100|200\neth0|br0|1|2\neth1|br0|3|4\nra0|br0|5|6\nusb0||7|8\nbad name||1|2";
    let addresses =
        "34: br0 inet 192.168.8.1/24 scope global\n35: usb0 inet 192.168.9.1/24 scope global";
    let proc = "br0: 100 1 2 3 4 5 6 7 200 9 10 11 12 13 14 15";
    let ifaces = adb::board_interfaces(inventory, addresses, proc);
    let by_name = |name: &str| ifaces.iter().find(|i| i.name == name).unwrap();
    assert_eq!(by_name("br0").members, vec!["eth0", "eth1", "ra0"]);
    assert_eq!(by_name("br0").addresses, vec!["192.168.8.1/24"]);
    assert_eq!(by_name("eth1").master, "br0");
    assert!(by_name("eth1").addresses.is_empty(), "成员口不必持有地址");
    assert!(
        !ifaces.iter().any(|i| i.name.contains(' ')),
        "非法接口名丢弃"
    );
    // 计数能力分开记：br0 两条路径都行，eth1 只有 sysfs。
    assert!(by_name("br0").proc_counters && by_name("br0").sysfs_counters);
    assert!(!by_name("eth1").proc_counters && by_name("eth1").sysfs_counters);
    assert_eq!(
        by_name("eth1").counter_source(),
        Some(CounterSource::Sysfs),
        "标准路径不可用时才回落 sysfs"
    );
    // 候选只列出真能读出计数的接口，且从不把桥和成员加起来。
    let candidates = adb::rx_candidates("br0", &ifaces);
    assert_eq!(candidates, vec!["br0", "eth0", "eth1", "ra0"]);

    // 一个能读的都没有 → 提前报错，不静默测出一堆 0。
    let dead = adb::board_interfaces("dead||x|y", addresses, "");
    assert!(adb::resolve_rx_interface("dead", "dead", &dead).is_err());
}

#[test]
fn board_counters_use_exact_interface_and_rx_tx_byte_columns() {
    let text = "br00: 999 1 2 3 4 5 6 7 888 9 10 11 12 13 14 15\r\n br0: 12345678901 1 2 3 4 5 6 7 98765432109 9 10 11 12 13 14 15\r\n";
    assert_eq!(
        adb::parse_counters(text, "br0").unwrap(),
        (12345678901, 98765432109)
    );
    assert!(adb::parse_counters(text, "br").is_err());
    assert!(adb::parse_counters("br0: 1 2 3", "br0").is_err());
    assert!(adb::parse_counters(&text.replace("12345678901", "-1"), "br0").is_err());

    // sysfs 是同一套驱动统计的另一个入口，不是另一份「更准」的数据。
    assert_eq!(
        adb::parse_sysfs_counters("12345678901\n98765432109\n", "br0").unwrap(),
        (12345678901, 98765432109)
    );
    for bad in ["", "1", "1 2 3", "abc\n2", "-1\n2"] {
        assert!(adb::parse_sysfs_counters(bad, "br0").is_err(), "{bad:?}");
    }
}

#[test]
fn legacy_adbd_requires_remote_status_even_when_transport_exit_is_zero() {
    assert_eq!(
        adb::parse_shell_output("result\r\n\r\n__CPE_INNER_STATUS__:0\r\n").unwrap(),
        "result\n"
    );
    assert!(
        adb::parse_shell_output("not found\n__CPE_INNER_STATUS__:127\n")
            .unwrap_err()
            .contains("127")
    );
    assert!(adb::parse_shell_output("test failed\n__CPE_INNER_STATUS__:1\n").is_err());
    assert!(adb::parse_shell_output("truncated response").is_err());
    assert!(adb::parse_shell_output("\n__CPE_INNER_STATUS__:0\nextra").is_err());
}

#[test]
fn gateway_is_a_board_local_address_not_the_board_wan_default_route() {
    let text =
        "4: ccmni2 inet 10.0.0.2/24 scope global\n34: br0 inet 192.168.8.1/24 scope global\n";
    assert_eq!(
        adb::address_interface(text, "192.168.8.1".parse().unwrap()).unwrap(),
        "br0"
    );
    assert!(adb::address_interface(text, "10.0.0.1".parse().unwrap()).is_err());
    let duplicate = format!("{text}35: br1 inet 192.168.8.1/24 scope global\n");
    assert!(adb::address_interface(&duplicate, "192.168.8.1".parse().unwrap()).is_err());
}

#[test]
fn ordinary_clients_follow_the_sender_without_swapping_rx_thresholds() {
    let mut cfg = example();
    cfg.links[0].upload_min_mbps = Some(800.0);
    cfg.links[0].download_min_mbps = Some(700.0);
    let link = &cfg.links[0];
    for flow in [Flow::Up, Flow::Down] {
        let req = client_request(&cfg, link, Protocol::Tcp, flow, cfg.port);
        assert_eq!(
            req.bind_ip,
            if flow == Flow::Up {
                link.local_ip
            } else {
                link.gateway
            }
            .to_string()
        );
        assert_eq!(
            req.dst,
            if flow == Flow::Up {
                link.gateway
            } else {
                link.local_ip
            }
            .to_string()
        );
        iperf::check_client_extra(&req).unwrap();
        assert!(!req.extra.contains(&"-R".into()));
        assert!(
            !req.extra.contains(&"-w".into()),
            "内环不可沿用双机 Windows 大窗口"
        );
    }
    // 普通 client 位于发送端，接收端仍按上下行绑定各自的 RX 与门限。
    assert!(Flow::Up.receiver_is_board());
    assert!(!Flow::Down.receiver_is_board());
    assert_eq!(link.leg_target(Flow::Up, false), Some(800.0));
    assert_eq!(link.leg_target(Flow::Down, false), Some(700.0));
}

#[test]
fn cli_requires_an_explicit_config_and_rejects_typos_and_duplicates() {
    let parse =
        |args: &[&str]| cli_options(&args.iter().map(|v| v.to_string()).collect::<Vec<_>>());
    assert_eq!(
        parse(&["--config", "inner.json", "--probe"]).unwrap(),
        (PathBuf::from("inner.json"), true, false)
    );
    for args in [
        vec![],
        vec!["--config"],
        vec!["--config", "--probe"],
        vec!["--probe"],
        vec!["--config", "x", "--prob"],
        vec!["--config", "x", "--probe", "--probe"],
    ] {
        assert!(parse(&args).is_err(), "{args:?}");
    }
}

// ---------------- 来源选择与判定 ----------------

fn nic(avg: Option<f64>, coverage: f64, target: Option<f64>) -> NicView {
    let stats = RateStats {
        avg_mbps: avg,
        coverage,
        ..Default::default()
    };
    NicView {
        avg_mbps: avg,
        acceptance: evaluate_rx_acceptance(crate::config::RateMode::Auto, target, &stats),
    }
}

fn tool(text: &str, streams: u32) -> ToolView {
    ToolView {
        rate: measure::parse_receiver_summary(text, streams, ToolOrigin::ClientSummary),
    }
}

const SINGLE: &str =
    "[  5] 0.00-20.00 sec 2.19 GBytes 940 Mbits/sec sender\n[  5] 0.00-20.00 sec 2.19 GBytes 938 Mbits/sec receiver\n";

#[test]
fn the_tool_rate_only_counts_a_real_receiver_summary_never_a_sender_or_a_last_interval() {
    let rate = measure::parse_receiver_summary(SINGLE, 1, ToolOrigin::ClientSummary).unwrap();
    assert!(
        (rate.mbps - 938.0).abs() < 0.5,
        "取 receiver 行而不是 sender"
    );
    assert_eq!(rate.aggregate, measure::Aggregate::Single);

    // 只有 sender 行 → 拿不到接收端结果，不能拿发送端顶替。
    let sender_only = "[  5] 0.00-20.00 sec 2.19 GBytes 940 Mbits/sec sender\n";
    assert!(measure::parse_receiver_summary(sender_only, 1, ToolOrigin::ClientSummary).is_err());
    // 只有逐秒 interval 行 → 最后一行不是汇总。
    let intervals = "[  5] 19.00-20.00 sec 100 MBytes 838 Mbits/sec\n";
    assert!(measure::parse_receiver_summary(intervals, 1, ToolOrigin::ClientSummary).is_err());
    assert!(measure::parse_receiver_summary("", 1, ToolOrigin::ClientSummary).is_err());

    // 多流没有 [SUM] 时不能取某一条流：那是总速率的 N 分之一。
    let per_stream = "[  5] 0.00-20.00 sec 1.09 GBytes 469 Mbits/sec receiver\n[  7] 0.00-20.00 sec 1.09 GBytes 469 Mbits/sec receiver\n";
    let error =
        measure::parse_receiver_summary(per_stream, 2, ToolOrigin::ClientSummary).unwrap_err();
    assert!(error.contains("SUM"), "{error}");
    // 有 [SUM] 就用 SUM。
    let with_sum = format!("{per_stream}[SUM] 0.00-20.00 sec 2.18 GBytes 938 Mbits/sec receiver\n");
    let rate = measure::parse_receiver_summary(&with_sum, 2, ToolOrigin::ClientSummary).unwrap();
    assert!((rate.mbps - 938.0).abs() < 0.5);
    assert_eq!(rate.aggregate, measure::Aggregate::Sum);
}

#[test]
fn strict_mode_never_swaps_in_the_tool_number_and_a_trusted_slow_link_stays_rate_fail() {
    // 计数器可信、就是慢：工具那边显示 938 也不能把 RATE_FAIL 救成 PASS。
    let slow = nic(Some(12.0), 1.0, Some(800.0));
    for strategy in [Measurement::NicStrict, Measurement::NicPreferred] {
        let picked = measure::select_leg(
            strategy,
            &slow,
            &tool(SINGLE, 1),
            Some(800.0),
            Some(800.0),
            false,
        );
        assert_eq!(picked.verdict.verdict, Verdict::RateFail, "{strategy:?}");
        assert_eq!(picked.source, Source::Nic);
        assert_eq!(picked.mbps, Some(12.0));
        assert!(picked.fallback_reason.is_none(), "可信就没有兜底这回事");
    }
    // 计数器不可信时，严格模式仍然 NOT_EVALUATED，绝不改口径。
    let broken = nic(None, 0.0, Some(800.0));
    let strict = measure::select_leg(
        Measurement::NicStrict,
        &broken,
        &tool(SINGLE, 1),
        Some(800.0),
        Some(800.0),
        false,
    );
    assert_eq!(strict.verdict.verdict, Verdict::NotEvaluated);
    assert_eq!(strict.source, Source::None);
    assert!(strict.mbps.is_none());
}

#[test]
fn the_tool_fallback_records_its_source_and_never_inherits_the_nic_threshold() {
    let broken = nic(None, 0.0, Some(800.0));
    // 有工具口径门限才形成工具口径的验收结论。
    let judged = measure::select_leg(
        Measurement::NicPreferred,
        &broken,
        &tool(SINGLE, 1),
        Some(800.0),
        Some(900.0),
        false,
    );
    assert_eq!(judged.source, Source::Tool);
    assert_eq!(judged.target_mbps, Some(900.0));
    assert_eq!(judged.verdict.verdict, Verdict::Pass, "938 ≥ 900");
    assert!(judged.fallback_reason.is_some());
    // 工具口径的门限自己说了算：调高就该 RATE_FAIL，不去看网卡那个 800。
    assert_eq!(
        measure::select_leg(
            Measurement::NicPreferred,
            &broken,
            &tool(SINGLE, 1),
            Some(800.0),
            Some(1200.0),
            false,
        )
        .verdict
        .verdict,
        Verdict::RateFail
    );

    // 没有工具口径门限 → 只 MEASURED，并明说网卡验收没有形成。
    let measured = measure::select_leg(
        Measurement::NicPreferred,
        &broken,
        &tool(SINGLE, 1),
        Some(800.0),
        None,
        false,
    );
    assert_eq!(measured.verdict.verdict, Verdict::Measured);
    assert_eq!(measured.target_mbps, None, "绝不继承网卡门限");
    assert!(measured.verdict.detail.contains("网卡口径验收未形成"));

    // 两路都没有可信结果 → NOT_EVALUATED，不拿发送端顶替。
    let nothing = measure::select_leg(
        Measurement::NicPreferred,
        &broken,
        &tool("[  5] 0.00-20.00 sec 2 GBytes 940 Mbits/sec sender\n", 1),
        Some(800.0),
        Some(900.0),
        false,
    );
    assert_eq!(nothing.verdict.verdict, Verdict::NotEvaluated);
    assert_eq!(nothing.source, Source::None);

    // 明确的工具策略：拿不到 receiver 汇总时也不偷偷改用网卡计数。
    let tool_only = measure::select_leg(
        Measurement::Tool,
        &nic(Some(500.0), 1.0, Some(400.0)),
        &tool("", 1),
        Some(400.0),
        Some(400.0),
        false,
    );
    assert_eq!(tool_only.verdict.verdict, Verdict::NotEvaluated);
    assert_eq!(tool_only.source, Source::None);
}

#[test]
fn a_frozen_counter_with_real_tool_traffic_is_evidence_but_a_slow_link_is_not() {
    let frozen = NicView {
        avg_mbps: Some(0.0),
        acceptance: evaluate_rx_acceptance(
            crate::config::RateMode::Auto,
            Some(800.0),
            &RateStats {
                avg_mbps: Some(0.0),
                coverage: 1.0,
                stalled_ratio: 1.0,
                ..Default::default()
            },
        ),
    };
    assert!(
        measure::counter_mismatch_hint(&frozen, &tool(SINGLE, 1), "板侧", "br0")
            .is_some_and(|hint| hint.starts_with("COUNTER_SOURCE_SUSPECT:"))
    );
    let hint = measure::counter_mismatch_hint(&frozen, &tool(SINGLE, 1), "master", "en0").unwrap();
    assert!(hint.contains("master en0"));
    assert!(!hint.contains("板侧") && !hint.contains("br0"));
    // 单纯「网卡数字比工具低」不是证据——链路可能本来就慢。
    let slow = nic(Some(12.0), 1.0, Some(800.0));
    assert!(measure::counter_mismatch_hint(&slow, &tool(SINGLE, 1), "板侧", "br0").is_none());
    // 工具侧也没数就更谈不上证据。
    assert!(measure::counter_mismatch_hint(&frozen, &tool("", 1), "板侧", "br0").is_none());
}

#[test]
fn a_bidirectional_total_only_adds_two_receivers_of_the_same_source_layer() {
    let leg = |source: Source, mbps: f64| LegMeasurement {
        source,
        mbps: Some(mbps),
        target_mbps: None,
        verdict: crate::verdict::VerdictResult::default(),
        fallback_reason: None,
    };
    let up = leg(Source::Nic, 600.0);
    let down = leg(Source::Nic, 400.0);
    let judged = measure::total_verdict(&[&up, &down], Some(900.0), None);
    assert_eq!(judged.verdict, Verdict::Pass);
    assert!(judged.diagnostics[0].contains("1000.000"));
    assert_eq!(
        measure::total_verdict(&[&up, &down], Some(1200.0), None).verdict,
        Verdict::RateFail
    );
    // 没有该口径的合计门限 → 只测量，不 PASS。
    assert_eq!(
        measure::total_verdict(&[&up, &down], None, Some(900.0)).verdict,
        Verdict::Measured,
        "工具口径的门限救不了网卡口径的合计"
    );
    // 混合来源不能相加。
    let mixed = measure::total_verdict(&[&up, &leg(Source::Tool, 400.0)], Some(900.0), Some(900.0));
    assert_eq!(mixed.verdict, Verdict::NotEvaluated);
    assert!(mixed.detail.contains("来源层次不同"));
    // 缺一条腿 / 一条腿没结果都不给合计。
    assert_eq!(
        measure::total_verdict(&[&up], Some(900.0), None).verdict,
        Verdict::NotEvaluated
    );
    let dead = LegMeasurement {
        source: Source::None,
        mbps: None,
        target_mbps: None,
        verdict: crate::verdict::VerdictResult::default(),
        fallback_reason: None,
    };
    assert_eq!(
        measure::total_verdict(&[&up, &dead], Some(900.0), None).verdict,
        Verdict::NotEvaluated
    );
    // 两条腿都是工具口径、也确实有数，但没配工具口径合计门限 → 只 MEASURED。
    let tool_pair = [leg(Source::Tool, 600.0), leg(Source::Tool, 400.0)];
    assert_eq!(
        measure::total_verdict(&[&tool_pair[0], &tool_pair[1]], Some(900.0), None).verdict,
        Verdict::Measured
    );
    let overflowing = [leg(Source::Nic, f64::MAX), leg(Source::Nic, f64::MAX)];
    let overflow_judgement =
        measure::total_verdict(&[&overflowing[0], &overflowing[1]], Some(1.0), None);
    assert_eq!(overflow_judgement.verdict, Verdict::NotEvaluated);
    assert_eq!(
        overflow_judgement.code,
        crate::reason::ReasonCode::NoValidMeasurement
    );
}

#[test]
fn bidirectional_legs_are_measured_against_the_common_overlap_window_or_nothing() {
    let window = |start: u64, end: u64| EffectiveWindow {
        start_ms: start,
        end_ms: end,
        available_secs: (end - start) as f64 / 1000.0,
        required_secs: 20,
        complete: true,
    };
    let shared = overlap_window(&[window(1_000, 21_000), window(1_500, 21_500)]).unwrap();
    assert_eq!((shared.start_ms, shared.end_ms), (1_500, 21_000));
    assert!((shared.available_secs - 19.5).abs() < 1e-9);
    assert!(!shared.complete, "重叠不足配置时长就不算完整");
    // 没有交集 = 这两条腿根本不是同时在跑，不给合计窗口。
    assert!(overlap_window(&[window(0, 10_000), window(11_000, 21_000)]).is_none());
    assert!(overlap_window(&[]).is_none());
}

#[test]
fn optional_targets_measure_and_directional_targets_use_the_shared_rx_contract() {
    // 网卡口径的验收入口仍然是子网那一个，语义一字不改。
    let stats = RateStats {
        avg_mbps: Some(100.0),
        coverage: 1.0,
        ..Default::default()
    };
    let judge = |target| evaluate_rx_acceptance(crate::config::RateMode::Auto, target, &stats);
    assert_eq!(judge(None).verdict, Verdict::Measured);
    assert_eq!(judge(Some(90.0)).verdict, Verdict::Pass);
    assert_eq!(judge(Some(200.0)).verdict, Verdict::RateFail);
    assert_eq!(
        evaluate_rx_acceptance(
            crate::config::RateMode::Auto,
            Some(90.0),
            &RateStats::default()
        )
        .verdict,
        Verdict::NotEvaluated
    );
    // 覆盖率低、计数器冻结分别有各自的原因码，不混成一句「没数」。
    let low = RateStats {
        avg_mbps: Some(100.0),
        coverage: 0.5,
        ..Default::default()
    };
    assert_eq!(
        evaluate_rx_acceptance(crate::config::RateMode::Auto, Some(90.0), &low).code,
        crate::reason::ReasonCode::SampleCoverageLow
    );
    let frozen = RateStats {
        avg_mbps: Some(0.0),
        coverage: 1.0,
        stalled_ratio: 1.0,
        ..Default::default()
    };
    assert_eq!(
        evaluate_rx_acceptance(crate::config::RateMode::Auto, Some(90.0), &frozen).code,
        crate::reason::ReasonCode::CounterStalled
    );
}

#[test]
fn udp_loss_is_reported_from_the_receiver_summary_and_never_defaults_to_zero() {
    let mut cfg = example();
    cfg.protocols = vec![Protocol::Udp];
    cfg.udp_mbps = Some(100.0);
    cfg.max_udp_loss_pct = Some(1.0);
    let diagnostics = |loss: Option<f64>, protocol| {
        udp_loss_diagnostics(
            &cfg,
            protocol,
            loss,
            loss.map(|_| 100),
            loss.map(|_| 10_000),
            "",
        )
    };

    let over = diagnostics(Some(2.5), Protocol::Udp);
    assert!(over.iter().any(|d| d.contains("100/10000")));
    assert!(
        over.iter().any(|d| d.starts_with("UDP_LOSS_HIGH:")),
        "超门槛必须留下可检索的诊断码：{over:?}"
    );
    assert!(
        over.iter().any(|d| d.contains("只看接收端速率")),
        "丢包不推翻速率判定，口径要写在诊断里：{over:?}"
    );
    assert!(!diagnostics(Some(0.5), Protocol::Udp)
        .iter()
        .any(|d| d.starts_with("UDP_LOSS_HIGH:")));

    // 拿不到 receiver 汇总行时报「未知」，绝不能算成 0%。
    let unknown = diagnostics(None, Protocol::Udp);
    assert!(unknown.iter().any(|d| d.contains("未知")), "{unknown:?}");
    assert!(!unknown.iter().any(|d| d.contains("0.000%")));
    assert!(
        unknown.iter().any(|d| d.contains("门槛")),
        "配了门槛却没数据要说明：{unknown:?}"
    );
    let incomplete = udp_loss_diagnostics(&cfg, Protocol::Udp, Some(2.5), None, None, "");
    assert!(
        incomplete.iter().any(|d| d.contains("计数不完整")),
        "只有百分比而没有 lost/total 时要明确说明：{incomplete:?}"
    );
    assert!(
        incomplete.iter().any(|d| d.contains("门槛无法核验")),
        "不完整计数不能假装完成门槛核验：{incomplete:?}"
    );
    assert!(
        diagnostics(Some(2.5), Protocol::Tcp).is_empty(),
        "TCP 没有丢包统计"
    );
}

// ---------------- 隔离与报告 ----------------

#[test]
fn inner_orchestration_and_reports_do_not_enter_the_subnet_executor_or_history() {
    assert_ne!(RUNS_ROOT, "runs");
    let source = include_str!("mod.rs");
    assert!(!source.contains("master::executor"));
    assert!(!source.contains("cancel::reset"));
    assert!(
        !include_str!("adb.rs").contains("TcpStream::connect"),
        "主控仅通过 ADB 管理板侧，不要求主控能走被测 LAN"
    );
    // 判定和来源选择只能有一个出处：执行器里不许再出现一次比较逻辑。
    assert!(
        !source.contains("evaluate_rx_acceptance(crate::config::RateMode::Verify"),
        "内环不进 Verify 的 TARGET_MISSING 分支"
    );
}

fn sample_leg(flow: Flow, source: Source, mbps: f64) -> LegRow {
    LegRow {
        flow,
        port: 56190,
        receiver: if flow.receiver_is_board() {
            "br0".into()
        } else {
            "以太网".into()
        },
        receiver_host: if flow.receiver_is_board() {
            "板侧".into()
        } else {
            "master".into()
        },
        counter_source: flow
            .receiver_is_board()
            .then_some(CounterSource::ProcNetDev),
        source,
        mbps: Some(mbps),
        target_mbps: Some(400.0),
        fallback_reason: (source == Source::Tool)
            .then(|| "COUNTER_STALLED: 计数器整窗零增长".to_string()),
        verdict: "PASS".into(),
        reason: "PASS".into(),
        detail: "详情".into(),
        diagnostics: vec!["诊断一".into()],
        nic_rx_mbps: Some(487.5),
        nic_verdict: "PASS".into(),
        nic_reason: "PASS".into(),
        nic_target_mbps: Some(400.0),
        background_mbps: 0.25,
        coverage: 0.999,
        effective_secs: 19.4,
        required_secs: 20,
        rx: RxDistribution {
            p10_mbps: Some(470.0),
            median_mbps: Some(489.0),
            p95_mbps: Some(496.0),
            min_mbps: Some(455.0),
            max_mbps: Some(498.0),
            rolling_coverage: 1.0,
            stalled_ratio: 0.0,
        },
        tool: ToolReport {
            sender_mbps: Some(500.0),
            receiver_mbps: Some(488.1),
            receiver_note: "取自 PC 侧 client 的 receiver 汇总行".into(),
            udp_loss_pct: Some(2.375),
            udp_lost_datagrams: Some(1000),
            udp_total_datagrams: Some(42105),
        },
        client: crate::protocol::IperfClientOut {
            ok: true,
            timed_out: false,
            cancelled: false,
            process_started: Some(true),
            cleanup_confirmed: Some(true),
            cmd: "iperf3 -c 192.168.8.1".into(),
            output: "客户端输出".into(),
        },
        server_log: "板侧 <server> 输出".into(),
        rx_samples: None,
    }
}

#[test]
fn report_escapes_device_output_and_explains_probe_only() {
    let report = RunReport {
        schema_version: 2,
        current: String::new(),
        created_at: "now".into(),
        config: example(),
        plan: None,
        probe_only: true,
        capability: None,
        units: Vec::new(),
        error: Some("<script>alert('x')</script>".into()),
    };
    let html = report::render(&report);
    assert!(html.contains("仅能力探测，未灌包"));
    assert!(!html.contains("<script>"));
    assert!(html.contains("&lt;script&gt;"));
    for forbidden in [
        "<script src",
        "<link",
        "<img",
        "http://",
        "https://",
        "@keyframes",
    ] {
        assert!(!html.contains(forbidden));
    }
}

#[test]
fn report_keeps_both_calibres_side_by_side_and_names_the_one_it_used() {
    // 控制台此前只印网卡 RX 与判定：iperf3 自报的收发速率、UDP 丢包和板侧
    // server 原始输出全都看不到，「工具一个字节没收到」和「收到了但网卡没采上」
    // 在界面上长得一模一样。两处口径必须同时出现，且要说清用了哪一个。
    let mut cfg = example();
    cfg.protocols = vec![Protocol::Udp];
    cfg.udp_mbps = Some(500.0);
    cfg.directions = vec![Direction::Bidir];
    cfg.links[0].measurement = Measurement::NicPreferred;
    let unit = UnitRow {
        id: "test-unit".into(),
        index: 1,
        link: "ETH".into(),
        host: "master".into(),
        ip_version: 4,
        protocol: Protocol::Udp,
        direction: Direction::Bidir,
        streams: 4,
        repeat: 1,
        measurement: Measurement::NicPreferred,
        verdict: "PASS".into(),
        resumed: false,
        reason: "PASS".into(),
        detail: "双向接收速率合计 1000.000Mbps".into(),
        diagnostics: vec!["双向两条腿的共同有效重叠窗口 19.40s".into()],
        total_mbps: Some(1000.0),
        total_target_mbps: Some(900.0),
        overlap_secs: Some(19.4),
        legs: vec![
            sample_leg(Flow::Up, Source::Nic, 600.0),
            sample_leg(Flow::Down, Source::Tool, 400.0),
        ],
    };
    let html = report::render(&RunReport {
        schema_version: 2,
        current: String::new(),
        created_at: "now".into(),
        config: cfg,
        plan: None,
        probe_only: false,
        capability: None,
        units: vec![unit],
        error: None,
    });
    for needle in [
        "UDP",           // 协议列：一份报告里可能 TCP/UDP 混排
        "双向并发",      // 方向语义不能退化成两条单向
        "500.00",        // 工具发送
        "488.10",        // 工具接收
        "2.375%",        // 丢包率
        "1000/42105",    // 丢包计数，百分比之外的原始证据
        "487.50",        // 网卡 RX，验收口径
        "网卡字节计数",  // 上行腿采用的来源
        "工具接收汇总",  // 下行腿采用的来源
        "1000.000 Mbps", // 双向合计
        "19.40",         // 共同有效重叠
        "客户端输出",
        "板侧 &lt;server&gt; 输出", // server 日志入报告且已转义
        "470.00",                   // RX 分布 P10
    ] {
        assert!(html.contains(needle), "报告缺少 {needle}");
    }
    assert!(!html.contains("<server>"));
}

#[test]
fn board_server_logs_keep_both_ends_so_long_runs_do_not_bloat_the_report() {
    let short = (0..80)
        .map(|i| format!("行{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(clip_log(&short), short, "短日志原样保留");

    // 长日志两头都要留：开头的 accept/connect 证明 client 打到了板子，
    // 结尾的汇总行带 UDP 的 lost/total。
    let mut lines = vec![
        "Server listening on 5201".into(),
        "accept: 192.168.8.100".into(),
    ];
    lines.extend(
        (0..4000).map(|i| format!("[  5] {i}.00-{}.00 sec 100 MBytes 838 Mbits/sec", i + 1)),
    );
    lines.push(
        "[SUM] 0.00-3600.00 sec 900 GBytes 2000 Mbits/sec 1234/5678901 (0.022%) receiver".into(),
    );
    let clipped = clip_log(&lines.join("\n"));
    assert!(clipped.contains("accept: 192.168.8.100"));
    assert!(clipped.contains("1234/5678901"));
    assert!(clipped.contains("省略中间"), "砍掉了就要说明砍了多少");
    assert!(clipped.len() < lines.join("\n").len() / 10);
}

#[test]
fn the_run_summary_counts_units_not_legs_and_keeps_the_failure_visible() {
    let mut report = RunReport {
        schema_version: 2,
        current: String::new(),
        created_at: "2026-09-09 10:00:00".into(),
        config: example(),
        plan: None,
        probe_only: false,
        capability: None,
        units: vec![
            UnitRow {
                id: "test-pass".into(),
                index: 1,
                link: "ETH".into(),
                host: "master".into(),
                ip_version: 4,
                protocol: Protocol::Tcp,
                direction: Direction::Bidir,
                streams: 1,
                repeat: 1,
                measurement: Measurement::NicStrict,
                verdict: "PASS".into(),
                resumed: false,
                reason: "PASS".into(),
                detail: String::new(),
                diagnostics: Vec::new(),
                total_mbps: Some(1000.0),
                total_target_mbps: Some(900.0),
                overlap_secs: Some(19.4),
                legs: vec![
                    sample_leg(Flow::Up, Source::Nic, 600.0),
                    sample_leg(Flow::Down, Source::Nic, 400.0),
                ],
            },
            UnitRow {
                id: "test-not-evaluated".into(),
                index: 2,
                link: "Wi-Fi".into(),
                host: "agent1".into(),
                ip_version: 4,
                protocol: Protocol::Tcp,
                direction: Direction::Upload,
                streams: 1,
                repeat: 1,
                measurement: Measurement::NicStrict,
                verdict: "NOT_EVALUATED".into(),
                resumed: false,
                reason: "NIC_RATE_MISSING".into(),
                detail: String::new(),
                diagnostics: Vec::new(),
                total_mbps: None,
                total_target_mbps: None,
                overlap_secs: None,
                legs: vec![sample_leg(Flow::Up, Source::None, 0.0)],
            },
        ],
        error: None,
    };
    let summary = history::summarize(&report);
    // 一个双向单元算一个单元，不是两条腿两行。
    assert_eq!(summary.units, 2);
    assert_eq!(summary.passed, 1);
    assert_eq!(summary.not_evaluated, 1);
    assert_eq!(summary.rate_failed, 0);
    assert_eq!(summary.links, vec!["ETH".to_string(), "Wi-Fi".to_string()]);
    assert!(summary.error.is_none());
    assert_eq!(summary.finished, Some(true));
    report.current = "下一单元".into();
    assert_eq!(history::summarize(&report).finished, Some(false));
    report.current.clear();
    // 执行中断也要在历史列表上看得见，不能只剩一个成功计数。
    report.error = Some("用户已取消内环测试".into());
    assert_eq!(
        history::summarize(&report).error.as_deref(),
        Some("用户已取消内环测试")
    );
    assert_eq!(history::summarize(&report).finished, Some(false));
    let old = history::Summary {
        finished: None,
        ..Default::default()
    };
    assert!(
        !history::reusable_summary(&old),
        "收尾状态未知不能拿来 RESUME"
    );
    let mut errored = history::Summary {
        finished: Some(true),
        ..Default::default()
    };
    errored.error = Some("旧记录错误".into());
    assert!(
        !history::reusable_summary(&errored),
        "带错误的旧摘要不能拿来 RESUME"
    );
}

#[test]
fn bidirectional_aggregate_never_treats_a_missing_leg_as_pass() {
    let result = aggregate(
        &[sample_leg(Flow::Up, Source::Nic, 900.0)],
        &crate::verdict::VerdictResult::not_evaluated(
            crate::reason::ReasonCode::UnitDirectionResultMissing,
            "双向合计需要上行和下行两条腿的结果，本单元缺少其中一条",
        ),
    );
    assert_eq!(result.verdict, Verdict::NotEvaluated);
    assert_eq!(
        result.code,
        crate::reason::ReasonCode::UnitDirectionResultMissing
    );
}

#[test]
fn history_files_round_trip_the_config_without_carrying_the_agent_token() {
    let dir = std::env::temp_dir().join(format!("cpe-inner-history-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = example();
    cfg.agents[0].token = "private-agent-token".into();
    let report = RunReport {
        schema_version: 2,
        current: String::new(),
        created_at: "2026-09-09 10:00:00".into(),
        config: cfg,
        plan: None,
        probe_only: true,
        capability: None,
        units: Vec::new(),
        error: None,
    };
    report::save(&dir, &report).unwrap();
    let text = std::fs::read_to_string(dir.join("config.json")).unwrap();
    assert!(!text.contains("private-agent-token"), "令牌不进历史文件");
    // 装载回来仍走同一条解析/迁移链，拿到的是可直接开跑的配置。
    let restored = config::parse_config(&text).unwrap();
    assert_eq!(restored.links.len(), 2);
    assert_eq!(restored.agents[0].token, "");
    let summary: history::Summary =
        serde_json::from_str(&std::fs::read_to_string(dir.join("summary.json")).unwrap()).unwrap();
    assert!(summary.probe_only && summary.units == 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn server_lease_reaps_only_its_child_and_removes_its_resource_directory() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use wait_timeout::ChildExt;
    let name = format!("cpe-inner-lease-test-{}", std::process::id());
    let base = std::env::temp_dir().join(name);
    std::fs::create_dir(&base).unwrap();
    let bin = base.join("fake-iperf");
    // exec 保持 PID，避免测试替身自己制造一个额外的 sleep 子孙。
    std::fs::write(&bin, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    let owned = base.join("owned");
    let script = adb::server_script(
        bin.to_str().unwrap(),
        "192.168.8.1".parse().unwrap(),
        56190,
        owned.to_str().unwrap(),
        1,
    );
    let mut child = Command::new("sh")
        .args(["-c", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let ended = child.wait_timeout(Duration::from_secs(5)).unwrap();
    if ended.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(ended.is_some(), "租约必须终止后台 server 并回收控制进程");
    assert!(!owned.exists());
    std::fs::remove_file(bin).unwrap();
    std::fs::remove_dir(base).unwrap();
}

fn unit_row(index: usize) -> UnitRow {
    UnitRow {
        id: format!("unit-{index}"),
        index,
        link: "ETH".into(),
        host: "master".into(),
        ip_version: 4,
        protocol: Protocol::Tcp,
        direction: Direction::Upload,
        streams: 1,
        repeat: 1,
        measurement: Measurement::NicPreferred,
        verdict: "MEASURED".into(),
        resumed: false,
        reason: "TARGET_UNKNOWN".into(),
        detail: "接收端网卡 RX 已测得".into(),
        diagnostics: Vec::new(),
        total_mbps: None,
        total_target_mbps: None,
        overlap_secs: None,
        legs: vec![sample_leg(Flow::Up, Source::Nic, 900.0)],
    }
}

/// 单元之间的落盘：轻的每次都写，重的按时间节流，收尾必写。
#[test]
fn progress_saves_append_units_and_throttle_the_full_rewrites() {
    let dir = std::env::temp_dir().join(format!("cpe-inner-save-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut report = RunReport {
        schema_version: 2,
        current: String::new(),
        created_at: "now".into(),
        config: example(),
        plan: None,
        probe_only: false,
        capability: None,
        units: Vec::new(),
        error: None,
    };
    let mut appended = 0;
    let mut last_heavy = None;

    // 第一次：重产物立刻写出来，页面上的「下载内环报告」从第一个单元起就能用。
    report.units.push(unit_row(1));
    report::save_progress(&dir, &report, &mut appended, &mut last_heavy).unwrap();
    assert!(dir.join("report.html").is_file(), "首个单元后就该有报告");
    assert!(dir.join("summary.json").is_file());
    assert_eq!(appended, 1);
    let first_heavy = std::fs::metadata(dir.join("result.json")).unwrap().len();

    // 紧接着的几个单元：units.jsonl 一条条追加，重产物不再重写。
    for index in 2..=4 {
        report.units.push(unit_row(index));
        report::save_progress(&dir, &report, &mut appended, &mut last_heavy).unwrap();
    }
    assert_eq!(appended, 4);
    let lines = std::fs::read_to_string(dir.join("units.jsonl")).unwrap();
    assert_eq!(
        lines.lines().count(),
        4,
        "每个单元恰好追加一行，不重写已经写过的"
    );
    assert_eq!(
        std::fs::metadata(dir.join("result.json")).unwrap().len(),
        first_heavy,
        "30 秒内不该再整份重写一次"
    );

    // 收尾必写：节流只作用于跑动过程中的中间态。
    report::save(&dir, &report).unwrap();
    assert!(
        std::fs::metadata(dir.join("result.json")).unwrap().len() > first_heavy,
        "收尾要把 4 个单元全部写进去"
    );
    assert!(dir.join("config.json").is_file(), "config.json 由收尾负责");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_report_downloads_never_read_a_truncated_generation() {
    let dir = std::env::temp_dir().join(format!("cpe-inner-atomic-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("report.html");
    let old = vec![b'A'; 256 * 1024];
    let new = vec![b'B'; 512 * 1024];
    report::write_atomic(&path, &old).unwrap();
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            for _ in 0..100 {
                let content = std::fs::read(&path).unwrap();
                assert!(content == old || content == new, "下载到了部分文件");
            }
        });
        for i in 0..40 {
            report::write_atomic(&path, if i % 2 == 0 { &new } else { &old }).unwrap();
        }
        reader.join().unwrap();
    });
    std::fs::remove_dir_all(dir).unwrap();
}

/// 生产代码不许手写 `MoveFileExW`——覆盖式重命名一律走 `std::fs::rename`。
///
/// 这条前提在本仓库出现过两次，两次都写着「Windows 对已有目标返回 AlreadyExists，
/// 所以要自己调 `MoveFileExW`」，两次都是假的：std 的 `rename` 第一步就是同一个
/// `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING)`。手写版唯一的实际区别是**丢掉了
/// std 在 `ERROR_ACCESS_DENIED` 上的兜底**（改用 `FileRenameInfoEx` +
/// `REPLACE_IF_EXISTS | POSIX_SEMANTICS` 重试，目标被别人打开着也能替换）。
///
/// 第一次在 `master::executor::db::save`，靠人读出来；第二次在
/// `inner::report::replace_file`，靠 Windows CI 上 `os error 5` 咬出来——
/// 本机 macOS/Linux 全绿，因为那段代码在 `#[cfg(windows)]` 里，压根没编译。
/// 「平台分支里的代码」正是本地门禁最没有分辨力的地方，所以这里用源码扫描。
///
/// 只看代码，不看注释：上面那两处的说明文字里就带着这个符号名。
#[test]
fn no_hand_rolled_move_file_ex_in_the_tree() {
    // 拼出来，免得这条测试自己的源码把自己扫红。
    let needle = concat!("MoveFile", "ExW");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            for (index, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                if code.contains(needle) {
                    offenders.push(format!("{}:{}", path.display(), index + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "覆盖式重命名一律用 std::fs::rename；手写 {needle} 会丢掉 std 在 \
         ERROR_ACCESS_DENIED 上的 FileRenameInfoEx 兜底: {offenders:#?}"
    );
}

#[cfg(unix)]
#[test]
fn atomic_report_write_never_follows_a_symlinked_pending_file() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "cpe-inner-atomic-symlink-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let outside = root.join("outside.txt");
    let path = root.join("report.html");
    let pending = path.with_extension("pending");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(&outside, b"keep me").unwrap();
    symlink(&outside, &pending).unwrap();

    let result = report::write_atomic(&path, b"must not escape");
    assert!(result.is_ok());
    assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");
    assert_eq!(std::fs::read(&path).unwrap(), b"must not escape");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn remote_monitor_start_must_return_a_stop_capable_id() {
    assert!(validate_remote_monitor_id("mon42").is_ok());
    assert!(validate_remote_monitor_id("").is_err());
    assert!(validate_remote_monitor_id("  \t").is_err());
}

/// 前后端校验规则的**共同**验收表，见 `src/inner/validation_corpus.json`。
///
/// 这几条白名单在 Rust 和 TypeScript 里各有一份手写实现，历史上漂移过五次，
/// 每次都表现为「界面保存成功、后端才拒绝」。普通单测挡不住它：两侧各自的
/// 用例都能过，只是内容不一样。所以把用例本身抽成一份两边共读的语料——
/// 改规则必须先改语料，然后 `cargo test` 和 `npm run test` 一起变红。
///
/// 对应的前端断言在 `ui/src/domain/inner-corpus.test.ts`。
#[test]
fn the_shared_validation_corpus_matches_the_rust_side() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/inner/validation_corpus.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{} 读不到：{e}", path.display()));
    let corpus: serde_json::Value = serde_json::from_str(&text).expect("语料不是合法 JSON");

    // 没有独立谓词的两个上限按常量比对；前端那份由 inner-corpus.test.ts 比对。
    let limits = &corpus["limits"];
    assert_eq!(
        limits["agent_address_bytes"].as_u64(),
        Some(config::MAX_AGENT_ADDRESS_BYTES as u64),
        "辅测机地址上限与语料不一致"
    );
    assert_eq!(
        limits["agent_token_bytes"].as_u64(),
        Some(config::MAX_AGENT_TOKEN_BYTES as u64),
        "辅测机令牌上限与语料不一致"
    );

    let cases = corpus["cases"].as_array().expect("cases 必须是数组");
    assert!(
        cases.len() >= 50,
        "语料被削减了？当前只有 {} 条",
        cases.len()
    );
    let mut seen = std::collections::HashSet::new();
    for case in cases {
        let field = case["field"].as_str().expect("field 必须是字符串");
        let why = case["why"].as_str().unwrap_or("");
        let expected = case["valid"].as_bool().expect("valid 必须是布尔");

        let mut value = case["value"].as_str().unwrap_or("").to_string();
        for key in ["repeat", "repeat_suffix"] {
            if let Some(spec) = case[key].as_array() {
                let unit = spec[0].as_str().expect("repeat 第一项是字符串");
                let times = spec[1].as_u64().expect("repeat 第二项是次数") as usize;
                value.push_str(&unit.repeat(times));
            }
        }

        let actual = match field {
            "size_token" => config::size_token(&value),
            "safe_word" => config::safe_word(&value),
            "iface_word" => config::iface_word(&value),
            "adb_program" => config::adb_program(&value),
            other => panic!("语料里出现了 Rust 侧没有接线的字段 {other:?}"),
        };
        assert_eq!(
            actual, expected,
            "{field}({value:?}) 期望 {expected}，实际 {actual} —— {why}"
        );
        seen.insert(field);
    }
    let mut seen: Vec<_> = seen.into_iter().collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        ["adb_program", "iface_word", "safe_word", "size_token"],
        "语料必须覆盖全部四条白名单"
    );
}

// IPv4/v6 共享执行链，只在真实端点与作用域上分开；不能把两种流混成一次验收。
fn dual_stack_example() -> InnerConfig {
    let mut cfg = example();
    cfg.ip_versions = vec![4, 6];
    for (index, link) in cfg.links.iter_mut().enumerate() {
        link.local_ipv6 = Some(format!("fe80::{}", index + 100).parse().unwrap());
        link.gateway_ipv6 = Some("fe80::1".parse().unwrap());
    }
    cfg
}

#[test]
fn ipv6_config_is_explicit_and_old_v2_projects_remain_ipv4_only() {
    let legacy = example();
    assert_eq!(legacy.ip_versions, vec![4]);
    let mut value = serde_json::to_value(&legacy).unwrap();
    value.as_object_mut().unwrap().remove("ip_versions");
    let envelope = serde_json::json!({"kind":config::PROJECT_KIND,"version":2,"config":value});
    assert_eq!(
        config::parse_config(&envelope.to_string())
            .unwrap()
            .ip_versions,
        vec![4]
    );
    assert_eq!(config::PROJECT_VERSION, 3);

    let mut cfg = dual_stack_example();
    cfg.ip_versions = vec![6];
    let mut value = serde_json::to_value(&cfg).unwrap();
    for link in value["links"].as_array_mut().unwrap() {
        link.as_object_mut().unwrap().remove("local_ip");
        link.as_object_mut().unwrap().remove("gateway");
    }
    let parsed = config::parse_config(&value.to_string()).unwrap();
    assert_eq!(parsed.links[0].local_ip, std::net::Ipv4Addr::UNSPECIFIED);
    assert_eq!(plan::build(&parsed).unwrap().unit_count(), 4);

    for versions in [vec![], vec![4, 4], vec![6, 6], vec![5]] {
        cfg.ip_versions = versions;
        assert!(cfg.validate().is_err());
    }
}

#[test]
fn ipv6_config_rejects_unusable_or_mismatched_addresses_before_execution() {
    for bad in [
        "::",
        "::1",
        "ff02::1",
        "::ffff:192.168.8.100",
        "fe80::100%eth0",
        "192.168.8.100",
        "fe80:::1",
    ] {
        let mut value = serde_json::to_value(dual_stack_example()).unwrap();
        value["links"][0]["local_ipv6"] = bad.into();
        assert!(config::parse_config(&value.to_string()).is_err(), "{bad}");
    }
    let mut cfg = dual_stack_example();
    cfg.links[0].local_ipv6 = None;
    assert!(cfg.validate().unwrap_err().contains("电脑 IPv6"));
    cfg.links[0].enabled = false;
    cfg.validate().unwrap();
    cfg.links[0].enabled = true;
    cfg.links[0].local_ipv6 = cfg.links[0].gateway_ipv6;
    assert!(cfg.validate().unwrap_err().contains("不能等于"));
    cfg.links[0].local_ipv6 = Some("fd00::100".parse().unwrap());
    assert!(cfg.validate().unwrap_err().contains("同为"));
    cfg.links[0].gateway_ipv6 = Some("fd00::1".parse().unwrap());
    cfg.validate().unwrap();
}

#[test]
fn ipv6_plan_expands_versions_independently_and_preserves_the_ipv4_resume_identity() {
    let mut cfg = config::parse_config(r#"{"serial":"board","links":[{"name":"ETH","local_interface":"Ethernet","local_ip":"192.168.0.100","gateway":"192.168.0.1"}]}"#).unwrap();
    let legacy = plan::build(&cfg).unwrap();
    // 按 schema v2 的原始字段序列计算并固定，新增字段不能让历史 PASS 失效。
    assert_eq!(legacy.units[0].id, "664b1070b9bfc57c7d94d8fb41e51737");
    cfg.ip_versions = vec![4, 6];
    cfg.links[0].local_ipv6 = Some("fe80::100".parse().unwrap());
    cfg.links[0].gateway_ipv6 = Some("fe80::1".parse().unwrap());
    cfg.protocols = vec![Protocol::Tcp, Protocol::Udp];
    cfg.udp_mbps = Some(100.0);
    cfg.directions.push(Direction::Bidir);
    cfg.repeats = 2;
    let built = plan::build(&cfg).unwrap();
    assert_eq!(built.unit_count(), 24);
    assert_eq!(built.leg_count(), 32);
    assert_eq!(built.bidir_units(), 8);
    assert!(built.units[..12].iter().all(|unit| unit.ip_version == 4));
    assert!(built.units[12..].iter().all(|unit| unit.ip_version == 6));
    assert_eq!(
        built
            .units
            .iter()
            .map(|unit| &unit.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        24
    );
    cfg.protocols = vec![Protocol::Tcp];
    cfg.udp_mbps = None;
    cfg.directions = vec![Direction::Upload, Direction::Download];
    cfg.repeats = 1;
    let dual = plan::build(&cfg).unwrap();
    assert_eq!(dual.units[0].id, legacy.units[0].id);
    assert_eq!(dual.units[1].id, legacy.units[1].id);
    assert_ne!(dual.units[0].id, dual.units[2].id);
    cfg.links[0].gateway_ipv6 = Some("fe80::2".parse().unwrap());
    let changed = plan::build(&cfg).unwrap();
    assert_eq!(dual.units[0].id, changed.units[0].id);
    assert_ne!(dual.units[2].id, changed.units[2].id);
    let resumed = std::collections::HashSet::from([legacy.units[0].id.clone()]);
    let preview = plan::preview_with_resumed(&cfg, &resumed).unwrap();
    assert_eq!(preview.resumed, 1);
    assert_eq!(preview.rows[2].ip_version, 6);
    assert!(preview.rows[2].legs[0].receiver.contains("fe80::2"));
}

#[test]
fn ipv6_preflight_matches_canonical_addresses_and_scopes_each_sender_to_its_own_interface() {
    let mut cfg = dual_stack_example();
    cfg.links[1].measurement = Measurement::Tool;
    let link = &cfg.links[1];
    let mut cap = capability(link, "agent1");
    cap.board_addresses
        .push_str("\n34: br0 inet6 fe80:0:0:0:0:0:0:1/64 scope link");
    let host = cap.agents[0].info.as_mut().unwrap();
    host.os = "linux".into();
    host.interfaces[0].ipv6_ll = "fe80:0:0:0:0:0:0:101".into();
    host.interfaces[0].zone = "eth7".into();
    let flight = preflight_version(link, &cap, 6).unwrap();
    assert_eq!(flight.addresses.pc_bind, "fe80::101%eth7");
    assert_eq!(flight.addresses.up_target, "fe80::1%eth7");
    assert_eq!(flight.addresses.board_bind, "fe80::1%br0");
    assert_eq!(flight.addresses.down_target, "fe80::101%br0");
    for flow in [Flow::Up, Flow::Down] {
        let request =
            client_request_for_addresses(&cfg, &flight.addresses, Protocol::Tcp, flow, cfg.port);
        assert!(request.v6);
        assert!(!request.extra.iter().any(|arg| arg == "-R"));
        assert_eq!(
            request.bind_ip,
            if flow == Flow::Up {
                "fe80::101%eth7"
            } else {
                "fe80::1%br0"
            }
        );
        assert_eq!(
            request.dst,
            if flow == Flow::Up {
                "fe80::1%eth7"
            } else {
                "fe80::101%br0"
            }
        );
    }
    cap.agents[0].info.as_mut().unwrap().os = "windows".into();
    let windows = preflight_version(link, &cap, 6).unwrap();
    assert_eq!(windows.addresses.pc_bind, "fe80::101");
    assert_eq!(windows.addresses.up_target, "fe80::1");
    assert_eq!(windows.addresses.down_target, "fe80::101%br0");
    cap.agents[0].info.as_mut().unwrap().interfaces[0].ipv6_ll = "fe80::999".into();
    assert!(preflight_version(link, &cap, 6)
        .unwrap_err()
        .contains("未唯一匹配"));
}

#[test]
fn ipv6_only_hosts_and_global_addresses_do_not_depend_on_ipv4_or_add_zones() {
    let mut cfg = dual_stack_example();
    cfg.ip_versions = vec![6];
    cfg.links[1].measurement = Measurement::Tool;
    cfg.links[1].local_ipv6 = Some("fd00::101".parse().unwrap());
    cfg.links[1].gateway_ipv6 = Some("fd00::1".parse().unwrap());
    let link = &cfg.links[1];
    let mut cap = capability(link, "agent1");
    cap.board_addresses = "34: br0 inet6 fd00::1/64 scope global".into();
    let host = cap.agents[0].info.as_mut().unwrap();
    host.os = "linux".into();
    host.interfaces[0].ipv4.clear();
    host.interfaces[0].ipv6_global = "fd00::101".into();
    let flight = preflight_version(link, &cap, 6).unwrap();
    assert_eq!(flight.addresses.pc_bind, "fd00::101");
    assert_eq!(flight.addresses.board_bind, "fd00::1");
    assert_eq!(flight.addresses.up_target, "fd00::1");
    assert_eq!(flight.addresses.down_target, "fd00::101");
    cap.board_addresses.clear();
    assert!(
        preflight_version(link, &cap, 6).is_err(),
        "不能把 IPv4 地址或配置的统计接口当成板侧 IPv6 归属证据"
    );
}

#[test]
fn ipv6_results_keep_version_in_reports_and_resume_rows() {
    let cfg = dual_stack_example();
    let built = plan::build(&cfg).unwrap();
    let row = resumed_row(&built.units[2]);
    assert_eq!(row.ip_version, 6);
    assert_eq!(serde_json::to_value(&row).unwrap()["ip_version"], 6);
    let report = RunReport {
        schema_version: 3,
        current: String::new(),
        created_at: String::new(),
        config: cfg,
        plan: None,
        probe_only: false,
        capability: None,
        units: vec![row],
        error: None,
    };
    assert!(report::render(&report).contains("IPv6 / TCP"));
}

#[test]
fn an_unchecked_ipv6_only_link_does_not_require_ipv4_for_another_links_round() {
    let mut cfg = dual_stack_example();
    cfg.ip_versions = vec![4];
    cfg.links[1].enabled = false;
    cfg.links[1].local_ip = std::net::Ipv4Addr::UNSPECIFIED;
    cfg.links[1].gateway = std::net::Ipv4Addr::UNSPECIFIED;
    cfg.validate().unwrap();
    assert!(plan::build(&cfg)
        .unwrap()
        .units
        .iter()
        .all(|unit| unit.link == 0));
    cfg.links[1].enabled = true;
    assert!(cfg.validate().unwrap_err().contains("IPv4"));
}
