//! WebUI 的测试。
//!
//! 这里守的主要是**校验与编译**两件事：用户在界面上填出来的东西必须先被拦下
//! 不合法的组合，再原样编译成执行计划。界面能填出的组合远比 CLI 多，这些用例
//! 是唯一能穷举它们的地方。

use super::*;
use crate::protocol::NicInfo;
use serde_json::json;

fn state_with_pair() -> UiState {
    let nic = |name: &str, role: &str, ip: &str| NicInfo {
        name: name.into(),
        role: role.into(),
        ipv4: ip.into(),
        speed_mbps: 2500,
        ..Default::default()
    };
    UiState {
        cfg: Config::default(),
        agent_host: "10.0.0.2".into(),
        master: HostInfo {
            hostname: "m".into(),
            os: "test".into(),
            interfaces: vec![nic("以太网 6", "SGMII2.5G", "192.168.0.101")],
        },
        agent: HostInfo {
            hostname: "a".into(),
            os: "test".into(),
            interfaces: vec![nic("WLAN 3", "WIFI5G", "192.168.0.104")],
        },
    }
}

fn request() -> RunRequest {
    RunRequest {
        pairs: vec![PairSelection {
            rx_target_bidir_ab: String::new(),
            rx_target_bidir_ba: String::new(),
            udp_groups: Vec::new(),
            tcp_groups: Vec::new(),
            src: "master:NAME=以太网 6".into(),
            dst: "agent:NAME=WLAN 3".into(),
            directions: vec!["ab".into(), "bidir".into()],
            transports: vec!["tcp".into(), "udp".into()],
            ip: vec!["v4".into()],
        }],
        nic_policies: vec![
            NicPolicySelection {
                endpoint: "master:NAME=以太网 6".into(),
                rx_target: "1800".into(),
                udp_bandwidth: "2.6G".into(),
                udp_length: String::new(),
            },
            NicPolicySelection {
                endpoint: "agent:NAME=WLAN 3".into(),
                rx_target: "1600".into(),
                udp_bandwidth: "2.8G".into(),
                udp_length: String::new(),
            },
        ],
        duration: 60,
        tcp_windows: vec!["2m".into(), "4m".into(), "256m".into()],
        tcp_streams: vec![1, 5, 10],
        udp_bandwidths: vec!["1m".into(), "500m".into(), "1G".into()],
        udp_lengths: Vec::new(),
        udp_windows: Vec::new(),
        udp_streams: 1,
        udp_groups: Vec::new(),
        tcp_groups: Vec::new(),
        ping_count: 0,
        ping_payload_sizes: Vec::new(),
        limit_udp_by_link_speed: false,
        resume: false,
        screenshot: false,
        ui_plan: None,
        plan_hash: None,
    }
}

fn suite_request() -> RunRequest {
    let mut req = request();
    req.pairs.clear();
    req.nic_policies.clear();
    req.tcp_windows.clear();
    req.tcp_streams.clear();
    req.udp_bandwidths.clear();
    req.udp_lengths.clear();
    req.udp_windows.clear();
    req.udp_streams = 1;
    req.ui_plan = Some(UiPlan {
        ui_plan_version: 1,
        link_sets: vec![UiLinkSet {
            id: "set-a".into(),
            name: "A".into(),
            pair_refs: vec![UiPairRef {
                id: "pair-a".into(),
                src: "master:NAME=以太网 6".into(),
                dst: "agent:NAME=WLAN 3".into(),
            }],
        }],
        recipes: UiRecipes {
            tcp: vec![UiRecipe {
                id: "tcp-r".into(),
                name: "TCP".into(),
                profiles: vec![UiRecipeProfile {
                    window: Some("4m".into()),
                    streams: UiU32Values::One(10),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            udp: vec![UiRecipe {
                id: "udp-r".into(),
                name: "UDP".into(),
                profiles: vec![UiRecipeProfile {
                    bandwidth: Some("100m".into()),
                    length: Some("1200".into()),
                    streams: UiU32Values::One(1),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ping: Vec::new(),
        },
        suites: vec![UiSuite {
            id: "suite-a".into(),
            name: "TCP UDP".into(),
            note: String::new(),
            execution: "sequential".into(),
            order: vec!["task-tcp".into(), "task-udp".into()],
            tasks: vec![
                UiTask {
                    id: "task-tcp".into(),
                    name: "TCP".into(),
                    protocol: "tcp".into(),
                    directions: vec!["ab".into()],
                    ip: vec!["v4".into()],
                    recipe_ids: vec!["tcp-r".into()],
                    ..Default::default()
                },
                UiTask {
                    id: "task-udp".into(),
                    name: "UDP".into(),
                    protocol: "udp".into(),
                    directions: vec!["ba".into()],
                    ip: vec!["v4".into()],
                    recipe_ids: vec!["udp-r".into()],
                    ..Default::default()
                },
            ],
        }],
        bindings: vec![UiBinding {
            id: "bind-a".into(),
            link_set_id: "set-a".into(),
            suite_id: "suite-a".into(),
            mode: "replace".into(),
            order: 1,
            pair_ids: Vec::new(),
        }],
        plan_hash: None,
    });
    req.plan_hash = None;
    req
}

#[test]
fn suite_plan_keeps_tcp_and_udp_as_independent_specs_in_suite_order() {
    let state = state_with_pair();
    let req = suite_request();
    let cfg = validated_config_from_request(&state, &req).expect("suite request should validate");
    assert_eq!(
        cfg.tests.len(),
        2,
        "one TCP and one UDP spec, no protocol cross product"
    );
    assert_eq!(cfg.tests[0].transports, vec!["tcp"]);
    assert_eq!(cfg.tests[1].transports, vec!["udp"]);
    assert_eq!(cfg.tests[0].direction.directions(), vec!["ab"]);
    assert_eq!(cfg.tests[1].direction.directions(), vec!["ba"]);

    let compiled = compile_request(&state, &req).expect("compile suite plan");
    assert_eq!(
        compiled.units.len(),
        2,
        "TCP and UDP each produce one independent unit"
    );
    assert_eq!(compiled.trace.len(), compiled.units.len());
    assert_eq!(compiled.trace[0].protocol.as_deref(), Some("tcp"));
    assert_eq!(compiled.trace[1].protocol.as_deref(), Some("udp"));
    assert_eq!(compiled.trace[0].direction.as_deref(), Some("ab"));
    assert_eq!(compiled.trace[1].direction.as_deref(), Some("ba"));
    assert!(!compiled.plan_hash.is_empty());
    assert!(!compiled.topology_fingerprint.is_empty());
}

#[test]
fn suite_trace_distinguishes_both_from_a_bidirectional_unit() {
    let state = state_with_pair();
    let mut req = suite_request();
    let plan = req.ui_plan.as_mut().unwrap();
    // `both` is the legacy spelling for two independent one-way legs.  It
    // must not be collapsed into the single concurrent `bidir` unit: the
    // trace is consumed by the review UI and needs to identify each leg.
    plan.suites[0].tasks.retain(|task| task.id == "task-tcp");
    plan.suites[0].order = vec!["task-tcp".into()];
    plan.suites[0].tasks[0].directions = vec!["both".into()];

    let compiled = compile_request(&state, &req).expect("both should compile");
    assert_eq!(compiled.units.len(), 2);
    assert!(compiled.units.iter().all(|unit| !unit.bidir));
    assert_eq!(
        compiled
            .trace
            .iter()
            .map(|trace| trace.direction.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("ab"), Some("ba")]
    );

    // The concurrent spelling is the opposite contract: one unit with
    // two tagged legs, represented by a single `bidir` trace direction.
    let mut req = suite_request();
    let plan = req.ui_plan.as_mut().unwrap();
    plan.suites[0].tasks.retain(|task| task.id == "task-tcp");
    plan.suites[0].order = vec!["task-tcp".into()];
    plan.suites[0].tasks[0].directions = vec!["bidir".into()];
    let compiled = compile_request(&state, &req).expect("bidir should compile");
    assert_eq!(compiled.units.len(), 1);
    assert!(compiled.units[0].bidir);
    assert_eq!(compiled.trace[0].direction.as_deref(), Some("bidir"));
}

#[test]
fn suite_plan_rejects_legacy_pairs_and_parallel_execution() {
    let state = state_with_pair();
    let mut req = suite_request();
    req.pairs = request().pairs;
    let error = validate_request(&state, &req).expect_err("mixed request formats must fail");
    assert!(error.contains("不能同时"), "{error}");

    let mut req = suite_request();
    req.ui_plan.as_mut().unwrap().suites[0].execution = "parallel".into();
    let error = validate_request(&state, &req).expect_err("parallel suites are not supported");
    assert!(error.contains("sequential"), "{error}");
}

#[test]
fn quick_plan_applies_request_level_ping_defaults() {
    let state = state_with_pair();
    let mut req = suite_request();
    req.ping_count = 5;
    req.ping_payload_sizes = vec![64, 1400];
    let plan = req.ui_plan.as_mut().unwrap();
    plan.recipes.ping.clear();
    plan.suites[0].order.push("task-ping".into());
    plan.suites[0].tasks.push(UiTask {
        id: "task-ping".into(),
        name: "Ping".into(),
        protocol: "ping".into(),
        directions: vec!["ab".into()],
        ip: vec!["v4".into()],
        ..Default::default()
    });

    let compiled = compile_request(&state, &req).expect("ping suite should validate");
    assert_eq!(compiled.cfg.ping.count, 5);
    assert_eq!(compiled.cfg.ping.payload_sizes, vec![64, 1400]);
    let ping = compiled
        .cfg
        .tests
        .iter()
        .find(|test| test.kinds.iter().any(|kind| kind == "ping"))
        .expect("ping task should compile");
    assert_eq!(
        ping.ping_count, None,
        "task should inherit request defaults"
    );
    assert_eq!(ping.ping_payload_sizes, None);
}

#[test]
fn quick_plan_rejects_ping_recipe_references_until_recipe_fields_exist() {
    let state = state_with_pair();
    let mut req = suite_request();
    let plan = req.ui_plan.as_mut().expect("suite plan");
    plan.suites[0].tasks.retain(|task| task.id == "task-tcp");
    plan.suites[0].order = vec!["task-tcp".into()];
    plan.recipes.ping.push(UiRecipe {
        id: "ping-r".into(),
        name: "PING recipe".into(),
        ..Default::default()
    });
    plan.suites[0].tasks.push(UiTask {
        id: "task-ping".into(),
        name: "PING".into(),
        protocol: "ping".into(),
        directions: vec!["ab".into()],
        ip: vec!["v4".into()],
        recipe_ids: vec!["ping-r".into()],
        ..Default::default()
    });
    plan.suites[0].order.push("task-ping".into());

    let error = validate_request(&state, &req)
        .expect_err("PING recipe references must not be silently ignored");
    assert!(error.contains("暂不支持 PING 配置"), "{error}");
}

/// `UiRecipe.mode` 是死字段，必须被拒绝而不是被静默忽略。
///
/// 校验器过去只准 `fixed`/`scan` 两个取值，而 `webui/plan.rs` 从头到尾不读它——
/// 两个取值产出的是**同一份计划**。用户以为 `fixed` 把档位钉死成一档，实际三条轴
/// 全展开、耗时三倍。档位本来就由轴的取值个数表达（单值=钉死、多值=扫描），
/// mode 是冗余开关，所以照 PING 配方的先例拒绝它。详见 ADR-16。
/// 监控会话 id 在同一毫秒内也必须互不相同。
///
/// 旧写法是 `mon-<pid>-<毫秒>`。控制台有 4 个工作线程，两次
/// `/api/monitor/start` 完全可能落在同一毫秒上；撞了之后会话表的
/// `HashMap::insert` 会**静默**顶掉前一条——被顶掉的那条再没人能 stop 它
/// （表里查不到 id），采样线程要一直跑到 90 秒空闲超时，辅测机侧的 monitor
/// 资源也跟着多占一截。这条把「同毫秒」这个前提直接摆进断言。
#[test]
fn monitor_session_ids_stay_unique_within_the_same_millisecond() {
    let ids: Vec<String> = (0..500).map(|_| next_monitor_session_id()).collect();
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "会话 id 撞了: {ids:?}");
    // 500 次连发几乎必然跨不过 1 毫秒，前缀相同正是这条测试要覆盖的情况。
    assert!(
        ids[0].starts_with(&format!("mon-{}-", std::process::id())),
        "id 形状变了: {}",
        ids[0]
    );
}

/// 界面溯源走 `TestSpec.origin`，不再走 `name` 的 URL 编码侧信道。
///
/// 之前每条 spec 的 `name` 是 `ui-plan/<链路集合>/<绑定>/<对>/<套件>/<任务>/<配方>/<协议>`
/// 七段 URL 编码，`compile_request` 再把它拆回来重建 trace。那是整条计划链路上
/// 唯一的 stringly 侧信道：靠约定不靠类型，改一处分隔符就悄悄断掉，而且把一个
/// 本该给人看的字段占成了机器协议——报错里于是全是 `%E5%9F%BA%E7%BA%BF`。
///
/// 这条同时钉住：`name` 现在是人能读的，`link_group` 取链路集合的名字。
#[test]
fn the_ui_plan_traces_units_through_origin_not_through_the_test_name() {
    let state = state_with_pair();
    let req = suite_request();
    let compiled = compile_request(&state, &req).expect("compile");

    assert!(!compiled.cfg.tests.is_empty());
    for test in &compiled.cfg.tests {
        let origin = test
            .origin
            .as_ref()
            .unwrap_or_else(|| panic!("{} 缺少 origin", test.name));
        assert_eq!(origin.link_set_id, "set-a");
        assert_eq!(origin.link_set_name, "A");
        assert_eq!(origin.pair_id, "pair-a");
        assert_eq!(origin.suite_id, "suite-a");
        assert!(!origin.binding_id.is_empty(), "binding_id 要填");
        assert!(!origin.task_id.is_empty(), "task_id 要填");

        // 分组键取链路集合的名字（用户资产），不是主机名。
        assert_eq!(test.link_group.as_deref(), Some("A"));

        // name 回归纯展示名：不许再有编码痕迹。
        assert!(
            !test.name.starts_with("ui-plan/") && !test.name.contains('%'),
            "name 应该是给人看的，实得 {:?}",
            test.name
        );
        assert!(
            test.name.contains("TCP UDP"),
            "展示名里要有套件名，实得 {:?}",
            test.name
        );
    }

    // trace 必须真的从 origin 重建出来，而不是碰巧为空。
    let seen: Vec<_> = compiled
        .trace
        .iter()
        .map(|t| (t.link_set_id.clone(), t.suite_id.clone(), t.task_id.clone()))
        .collect();
    assert!(!seen.is_empty(), "trace 不该为空");
    for (link_set_id, suite_id, task_id) in &seen {
        assert_eq!(link_set_id.as_deref(), Some("set-a"));
        assert_eq!(suite_id.as_deref(), Some("suite-a"));
        assert!(task_id.is_some(), "task_id 要能溯源");
    }
    // 协议不进 origin（transports/kinds 已经说清楚了），但 trace 上仍要有。
    assert!(
        compiled.trace.iter().all(|t| matches!(
            t.protocol.as_deref(),
            Some("tcp") | Some("udp") | Some("ping")
        )),
        "trace 的协议标签丢了: {:?}",
        compiled
            .trace
            .iter()
            .map(|t| t.protocol.clone())
            .collect::<Vec<_>>()
    );
}

/// 旧版本导出的 `config.json` 仍然能被读懂。
///
/// 那些文件的溯源信息只在 `name` 里（`ui-plan/<七段>`），没有 `origin` 字段。
/// 回落解析必须留着，直到确定没人再拿旧导出来跑——否则用户把去年导出的配置
/// 拖回界面，复核树会整个塌成一层。
#[test]
fn a_config_exported_by_the_old_encoder_still_traces_back() {
    let mut test = TestSpec {
        name: "ui-plan/set-x/bind-x/pair-x/suite-x/task-x/recipe-x/udp".into(),
        src: "master:NAME=以太网 6".into(),
        dst: "agent:NAME=WLAN 3".into(),
        direction: OneOrMany::One("A->B".into()),
        kinds: vec!["iperf".into()],
        transports: vec!["udp".into()],
        ip: vec!["v4".into()],
        streams: 1,
        tcp_streams: None,
        udp_streams: None,
        iperf_duration: None,
        ping_count: None,
        ping_payload_sizes: None,
        tcp_windows: None,
        udp_profiles: None,
        rate_mode: None,
        rate_targets_mbps: None,
        rate_targets_bidir_mbps: None,
        link_group: None,
        origin: None,
    };

    let source = ui_source_from_spec(&test).expect("旧名字必须还能解析");
    assert_eq!(source.link_set_id, "set-x");
    assert_eq!(source.pair_id, "pair-x");
    assert_eq!(source.suite_id, "suite-x");
    assert_eq!(source.task_id, "task-x");
    assert_eq!(source.recipe_id, "recipe-x");
    assert_eq!(source.protocol, "udp");

    // 有 origin 时以 origin 为准，name 里的旧编码不再参与。
    test.origin = Some(UiOrigin {
        pair_id: "pair-new".into(),
        link_set_id: "set-new".into(),
        link_set_name: "新集合".into(),
        binding_id: "bind-new".into(),
        suite_id: "suite-new".into(),
        task_id: "task-new".into(),
        recipe_id: "recipe-new".into(),
    });
    let source = ui_source_from_spec(&test).expect("origin 必须被认");
    assert_eq!(source.link_set_id, "set-new");
    assert_eq!(source.pair_id, "pair-new");
    assert_eq!(
        source.protocol, "udp",
        "协议从 transports 推，不从 origin 存"
    );

    // 全空的 origin 等于没有 origin：老配置反序列化出来就是这个形状。
    test.origin = Some(UiOrigin::default());
    let source = ui_source_from_spec(&test).expect("空 origin 要回落到 name");
    assert_eq!(source.link_set_id, "set-x");
}

#[test]
fn quick_plan_rejects_the_dead_recipe_mode_field_instead_of_ignoring_it() {
    let state = state_with_pair();
    for mode in ["fixed", "scan", "Fixed"] {
        let mut req = suite_request();
        req.ui_plan.as_mut().expect("suite plan").recipes.udp[0].mode = mode.into();
        let error = validate_request(&state, &req)
            .err()
            .unwrap_or_else(|| panic!("mode={mode} 必须被拒绝，不能静默忽略"));
        assert!(error.contains("mode 字段已废弃"), "{error}");
        assert!(
            error.contains("删掉"),
            "报错要告诉用户怎么改，而不是只说不行: {error}"
        );
    }

    // 空 mode 是唯一合法取值：老项目文件删掉这一行之后必须照常能跑。
    let req = suite_request();
    assert!(
        req.ui_plan.as_ref().expect("suite plan").recipes.udp[0]
            .mode
            .is_empty(),
        "测试基线自己不该带 mode"
    );
    assert!(validate_request(&state, &req).is_ok(), "空 mode 必须放行");
}

#[test]
fn quick_plan_rejects_append_binding_mode_without_silent_replace() {
    let state = state_with_pair();
    let mut req = suite_request();
    req.ui_plan.as_mut().expect("suite plan").bindings[0].mode = "append".into();

    let error = validate_request(&state, &req)
        .expect_err("unsupported append mode must fail at validation");
    assert!(error.contains("append 尚未支持"), "{error}");
}

#[test]
fn quick_plan_ignores_unbound_empty_link_set_but_rejects_bound_empty_set() {
    let state = state_with_pair();
    let mut req = suite_request();
    {
        let plan = req.ui_plan.as_mut().expect("suite plan");
        // The UI permits creating a draft collection before selecting pairs.
        // An unrelated empty collection must not prevent another valid binding
        // from being previewed.
        plan.link_sets.push(UiLinkSet {
            id: "empty-draft".into(),
            name: "待填写".into(),
            pair_refs: Vec::new(),
        });
    }
    let compiled = compile_request(&state, &req).expect("unbound draft is harmless");
    assert_eq!(compiled.cfg.tests.len(), 2);

    // Once a suite is assigned to that collection, silently producing no
    // units would be much worse than an actionable validation error.
    req.ui_plan.as_mut().expect("suite plan").bindings[0].link_set_id = "empty-draft".into();
    let error = validate_request(&state, &req).expect_err("bound empty set must fail");
    assert!(error.contains("没有可执行的 pair_ref"), "{error}");

    // A non-empty set with an explicit subset remains valid when the
    // selected reference exists; the effective-pair check must not confuse
    // `pair_ids` with an instruction to run the whole set.
    let mut req = suite_request();
    req.ui_plan.as_mut().expect("suite plan").bindings[0].pair_ids = vec!["pair-a".into()];
    assert!(
        validate_request(&state, &req).is_ok(),
        "an existing pair_ids subset should remain executable"
    );
}

/// 未绑定的草稿集合里躺着**失效的网口对**，不该挡下另一套可执行分配。
///
/// 校验器的注释和 `使用说明.md` 都承诺「未绑定集合里的失效对只是提示」，但端点
/// 解析过去对所有 link_set 的每个 pair_ref 硬失败——一个没人引用的草稿就能顶掉
/// 整份请求，报错还指向用户根本没打算跑的集合。原来的测试只摆了个**空**草稿，
/// 空集合恰好绕过了那个循环，所以这条承诺破了也没人知道。
#[test]
fn quick_plan_ignores_stale_pairs_inside_an_unbound_draft_set() {
    let state = state_with_pair();
    let mut req = suite_request();
    {
        let plan = req.ui_plan.as_mut().expect("suite plan");
        plan.link_sets.push(UiLinkSet {
            id: "stale-draft".into(),
            name: "网卡换过之后剩下的草稿".into(),
            pair_refs: vec![UiPairRef {
                id: "pair-gone".into(),
                // 这两块网口在当前拓扑里都不存在了。
                src: "master:NAME=已经拔掉的网口".into(),
                dst: "agent:NAME=也不在了".into(),
            }],
        });
    }
    let compiled =
        compile_request(&state, &req).expect("未绑定草稿里的失效对不该挡下可执行的 binding");
    assert_eq!(compiled.cfg.tests.len(), 2);

    // 但一旦这个集合被绑定，失效对就必须当场报错——静默跑出零单元或者跑到
    // builder 里才炸，都比在预览阶段说清楚更糟。
    req.ui_plan.as_mut().expect("suite plan").bindings[0].link_set_id = "stale-draft".into();
    let error = validate_request(&state, &req).expect_err("被绑定的失效对必须拒绝");
    assert!(error.contains("已失效"), "{error}");

    // 明确用 pair_ids 选中同一条失效对，走的是另一条分支，也必须拒绝。
    let mut req = suite_request();
    {
        let plan = req.ui_plan.as_mut().expect("suite plan");
        plan.link_sets[0].pair_refs.push(UiPairRef {
            id: "pair-gone".into(),
            src: "master:NAME=已经拔掉的网口".into(),
            dst: "agent:NAME=也不在了".into(),
        });
        plan.bindings[0].pair_ids = vec!["pair-gone".into()];
    }
    let error = validate_request(&state, &req).expect_err("被 pair_ids 选中的失效对必须拒绝");
    assert!(error.contains("已失效"), "{error}");

    // 同一个集合里，没被 pair_ids 选中的那条失效对则应当被放过。
    let mut req = suite_request();
    {
        let plan = req.ui_plan.as_mut().expect("suite plan");
        let good = plan.link_sets[0].pair_refs[0].id.clone();
        plan.link_sets[0].pair_refs.push(UiPairRef {
            id: "pair-gone".into(),
            src: "master:NAME=已经拔掉的网口".into(),
            dst: "agent:NAME=也不在了".into(),
        });
        plan.bindings[0].pair_ids = vec![good];
    }
    assert!(
        validate_request(&state, &req).is_ok(),
        "没被选中的失效对不该挡下同集合里被选中的那条"
    );
}

#[test]
fn quick_plan_rejects_empty_udp_recipe_that_would_emit_no_units() {
    let state = state_with_pair();
    let mut req = suite_request();
    let recipe = req.ui_plan.as_mut().unwrap().recipes.udp[0].clone();
    let empty = UiRecipe {
        id: recipe.id,
        name: recipe.name,
        mode: recipe.mode,
        profiles: vec![UiRecipeProfile::default()],
        ..Default::default()
    };
    req.ui_plan.as_mut().unwrap().recipes.udp[0] = empty;
    let error = validate_request(&state, &req).expect_err("empty UDP recipe must fail");
    assert!(error.contains("-b") || error.contains("有效"), "{error}");
}

#[test]
fn quick_plan_validates_task_duration_and_profile_dimensions() {
    let state = state_with_pair();
    let mut req = suite_request();
    req.ui_plan.as_mut().unwrap().suites[0].tasks[0].duration = Some(0);
    let error = validate_request(&state, &req).expect_err("zero task duration must fail");
    assert!(error.contains("时长"), "{error}");

    let mut req = suite_request();
    req.ui_plan.as_mut().unwrap().recipes.udp[0].profiles[0].length = Some("65508".into());
    let error = validate_request(&state, &req).expect_err("oversized UDP profile must fail");
    assert!(error.contains("65507"), "{error}");

    let mut req = suite_request();
    req.ui_plan.as_mut().unwrap().recipes.udp[0].profiles[0].window = Some("not-size".into());
    let error = validate_request(&state, &req).expect_err("invalid UDP profile window must fail");
    assert!(error.contains("profile -w"), "{error}");
}

#[test]
fn quick_plan_preserves_slashes_in_trace_ids() {
    let state = state_with_pair();
    let mut req = suite_request();
    let plan = req.ui_plan.as_mut().unwrap();
    plan.link_sets[0].id = "set/a".into();
    plan.link_sets[0].pair_refs[0].id = "pair/a".into();
    plan.recipes.tcp[0].id = "tcp/recipe".into();
    plan.recipes.udp[0].id = "udp/recipe".into();
    plan.suites[0].id = "suite/a".into();
    plan.suites[0].tasks[0].id = "task/tcp".into();
    plan.suites[0].tasks[1].id = "task/udp".into();
    plan.suites[0].order = vec!["task/tcp".into(), "task/udp".into()];
    plan.suites[0].tasks[0].recipe_ids = vec!["tcp/recipe".into()];
    plan.suites[0].tasks[1].recipe_ids = vec!["udp/recipe".into()];
    plan.bindings[0].id = "binding/a".into();
    plan.bindings[0].link_set_id = "set/a".into();
    plan.bindings[0].suite_id = "suite/a".into();

    let compiled = compile_request(&state, &req).expect("slash IDs should be valid");
    assert_eq!(compiled.trace[0].link_set_id.as_deref(), Some("set/a"));
    assert_eq!(compiled.trace[0].pair_id.as_deref(), Some("pair/a"));
    assert_eq!(compiled.trace[0].suite_id.as_deref(), Some("suite/a"));
    assert_eq!(compiled.trace[0].task_id.as_deref(), Some("task/tcp"));
    assert_eq!(compiled.trace[0].recipe_id.as_deref(), Some("tcp/recipe"));
    assert_eq!(compiled.trace[1].task_id.as_deref(), Some("task/udp"));
    assert_eq!(compiled.trace[1].recipe_id.as_deref(), Some("udp/recipe"));
}

#[test]
fn quick_plan_rejects_duplicate_pair_ids_in_a_binding() {
    let state = state_with_pair();
    let mut req = suite_request();
    req.ui_plan.as_mut().unwrap().bindings[0].pair_ids = vec!["pair-a".into(), "pair-a".into()];
    let error = validate_request(&state, &req).expect_err("duplicate pair refs must fail");
    assert!(error.contains("重复引用"), "{error}");
}

#[test]
fn quick_plan_allows_link_set_and_recipe_ids_to_share_a_namespace_name() {
    let state = state_with_pair();
    let mut req = suite_request();
    let plan = req.ui_plan.as_mut().unwrap();
    // IDs are scoped by the field that owns them.  A human-authored
    // project commonly calls both its first link set and its first recipe
    // "default"; that must not be mistaken for a duplicate reference.
    plan.link_sets[0].id = "default".into();
    plan.recipes.tcp[0].id = "default".into();
    plan.bindings[0].link_set_id = "default".into();
    plan.suites[0].tasks[0].recipe_ids = vec!["default".into()];

    let compiled =
        compile_request(&state, &req).expect("link-set and recipe IDs may match across namespaces");
    assert_eq!(compiled.cfg.tests.len(), 2);
}

#[test]
fn quick_plan_honors_stream_axes_on_legacy_udp_profiles() {
    let state = state_with_pair();
    let mut req = suite_request();
    let recipe = &mut req.ui_plan.as_mut().unwrap().recipes.udp[0];
    recipe.profiles.clear();
    recipe.bandwidths.clear();
    recipe.lengths.clear();
    recipe.windows.clear();
    recipe.udp_profiles = vec![UdpProfile::bw("100m")];
    recipe.udp_streams = vec![2, 3];
    let cfg = validated_config_from_request(&state, &req).expect("legacy UDP recipe valid");
    let streams: Vec<u32> = cfg
        .tests
        .iter()
        .filter(|test| test.transports.iter().any(|transport| transport == "udp"))
        .filter_map(|test| test.udp_streams)
        .collect();
    assert_eq!(streams, vec![2, 3]);
}

/// 界面上填的门限/带宽必须真的变成 link_profiles，否则勾了等于没勾。
#[test]
fn ui_selection_becomes_a_real_config() {
    let cfg = config_from_request(&state_with_pair(), &request());
    assert_eq!(cfg.iperf.tcp_windows, vec!["2m", "4m", "256m"]);

    // 发送端网卡带的是它作为发送端时的带宽；接收端网卡带的是对向门限。
    let master_nic = cfg
        .link_profiles
        .by_nic
        .iter()
        .find(|p| p.name == "以太网 6")
        .expect("主控网卡应有覆盖项");
    assert_eq!(master_nic.host, "master");
    assert_eq!(master_nic.udp_bandwidth.as_deref(), Some("2.6G"));
    assert_eq!(master_nic.rx_target_mbps, Some(1800.0));

    let agent_nic = cfg
        .link_profiles
        .by_nic
        .iter()
        .find(|p| p.name == "WLAN 3")
        .expect("辅测网卡应有覆盖项");
    assert_eq!(agent_nic.udp_bandwidth.as_deref(), Some("2.8G"));
    assert_eq!(agent_nic.rx_target_mbps, Some(1600.0));
}

/// `-P` 在配置模型里是标量，多档位只能在界面层展开成多份 TestSpec；
/// TCP / UDP 必须拆开，否则「3 个 -P 档位」会把与 -P 无关的 UDP 单元复制三遍。
#[test]
fn stream_steps_expand_into_separate_specs_without_duplicating_udp() {
    let cfg = config_from_request(&state_with_pair(), &request());
    let tcp: Vec<&TestSpec> = cfg
        .tests
        .iter()
        .filter(|t| t.transports.contains(&"tcp".to_string()))
        .collect();
    let udp: Vec<&TestSpec> = cfg
        .tests
        .iter()
        .filter(|t| t.transports.contains(&"udp".to_string()))
        .collect();

    assert_eq!(tcp.len(), 3, "三个 -P 档位各一份");
    let mut steps: Vec<u32> = tcp.iter().filter_map(|t| t.tcp_streams).collect();
    steps.sort_unstable();
    assert_eq!(steps, vec![1, 5, 10]);
    for spec in &tcp {
        // -w 本来就是数组，交给 builder 展开，不在这里乘一遍。
        assert_eq!(
            spec.tcp_windows.as_deref(),
            Some(["2m".to_string(), "4m".to_string(), "256m".to_string()].as_slice())
        );
    }
    assert_eq!(udp.len(), 1, "UDP 不该被 -P 档位复制");
}

/// 某对填了 -b 覆盖，就只按它跑一档，不再参与全局档位扫描——
/// 否则「档位 1m/500m/1G」×「覆盖 1G」会跑出三个一模一样的单元。
#[test]
fn explicit_bandwidth_on_every_sending_nic_opts_out_of_the_global_sweep() {
    let with_override = config_from_request(&state_with_pair(), &request());
    let udp = with_override
        .tests
        .iter()
        .find(|t| t.transports.contains(&"udp".to_string()))
        .expect("应有 UDP spec");
    assert_eq!(udp.udp_profiles.as_ref().map(|v| v.len()), Some(1));

    let mut req = request();
    for policy in &mut req.nic_policies {
        policy.udp_bandwidth.clear();
    }
    let swept = config_from_request(&state_with_pair(), &req);
    let udp = swept
        .tests
        .iter()
        .find(|t| t.transports.contains(&"udp".to_string()))
        .expect("应有 UDP spec");
    assert_eq!(
        udp.udp_profiles.as_ref().map(|v| v.len()),
        Some(3),
        "没有覆盖时按全局三个档位扫描"
    );
}

/// 一边按网口固定、另一边留空时，留空腿仍需扫描全部全局档位；
/// 而被固定的那个方向不能跟着扫。
///
/// 这两件事必须**逐方向**判断。按整对判断时，只要有一条腿没被覆盖就整对
/// 去扫档位，于是「ab 被发送端钉死」的那个方向会被复制成 N 个一模一样的
/// 单元——3 档 × 180s 就是 6 分钟白跑，报告里还多出两行看着像 bug 的重复项。
#[test]
fn a_one_sided_bandwidth_override_sweeps_only_the_unpinned_direction() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].directions = vec!["ab".into(), "ba".into()];
    req.pairs[0].transports = vec!["udp".into()];
    // 发送端 master 钉死在 2.6G，反向发送端 agent 留空。
    req.nic_policies[1].udp_bandwidth.clear();

    let cfg = config_from_request(&state, &req);
    let pinned = cfg
        .tests
        .iter()
        .find(|test| test.direction.directions() == ["ab"])
        .expect("被钉死的 ab 方向应单独成一份 spec");
    assert_eq!(
        pinned.udp_profiles.as_ref().map(Vec::len),
        Some(1),
        "ab 的发送腿已被覆盖，扫档位只会生成重复单元"
    );
    let swept = cfg
        .tests
        .iter()
        .find(|test| test.direction.directions() == ["ba"])
        .expect("未覆盖的 ba 方向应保留档位扫描");
    assert_eq!(
        swept.udp_profiles.as_ref().map(Vec::len),
        Some(3),
        "未覆盖的反向发送腿仍要跑 1m/500m/1G 三档"
    );

    // 真正要防的是队列里出现重复单元，所以一路建到 unit 再查。
    let specs: Vec<_> = cfg
        .tests
        .iter()
        .map(|test| {
            builder::spec_from_config(test, &cfg, &state.master, &state.agent).expect("建 spec")
        })
        .collect();
    let mut port = builder::PORT_BASE;
    let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
    let titles: Vec<&str> = units.iter().map(|unit| unit.title.as_str()).collect();
    let unique: HashSet<&str> = titles.iter().copied().collect();
    assert_eq!(
        unique.len(),
        titles.len(),
        "同一条命令不该排进队列两次: {titles:?}"
    );
    assert_eq!(titles.len(), 4, "ab 一个 + ba 三档: {titles:?}");
}

/// 没填就不生成覆盖项，避免用一堆空条目盖掉配置文件里原有的策略。
#[test]
fn blank_inputs_produce_no_overrides() {
    let mut req = request();
    for policy in &mut req.nic_policies {
        policy.rx_target.clear();
        policy.udp_bandwidth.clear();
        policy.udp_length.clear();
    }
    let cfg = config_from_request(&state_with_pair(), &req);
    assert!(cfg.link_profiles.by_nic.is_empty());
}

#[test]
fn an_empty_checkbox_group_is_rejected_instead_of_silently_defaulting() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].directions.clear();
    assert!(validate_request(&state, &req)
        .unwrap_err()
        .contains("至少勾一个有效方向"));

    let mut req = request();
    req.pairs[0].transports.clear();
    assert!(validate_request(&state, &req)
        .unwrap_err()
        .contains("至少勾 TCP / UDP / PING"));
}

#[test]
fn invalid_sweep_values_are_rejected_before_starting_a_run() {
    let state = state_with_pair();
    let mut req = request();
    req.tcp_streams = vec![0, 33];
    assert!(validate_request(&state, &req)
        .unwrap_err()
        .contains("TCP -P"));

    let mut req = request();
    req.udp_bandwidths = vec!["500m-junk".into()];
    assert!(validate_request(&state, &req)
        .unwrap_err()
        .contains("UDP -b"));
}

#[test]
fn bootstrap_reports_token_presence_without_exposing_the_secret() {
    let mut state = state_with_pair();
    state.cfg.agent_token = "do-not-send-to-the-page".into();
    let console = Arc::new(Console {
        state: Mutex::new(state),
        running: AtomicBool::new(false),
        report: Mutex::new(String::new()),
        ui_token: String::new(),
        monitors: Mutex::new(HashMap::new()),
        run_status: Arc::new(RunStatusRecorder::new()),
    });
    let value = api_bootstrap(&console).expect("bootstrap");
    assert_eq!(value["token_configured"], true);
    assert!(value.get("agent_token").is_none());
    assert!(!value.to_string().contains("do-not-send-to-the-page"));
}

/// 界面留空不能把配置文件里的既有档位清成空列表。
#[test]
fn empty_lists_fall_back_to_the_configured_values() {
    let mut req = request();
    req.tcp_windows.clear();
    req.tcp_streams.clear();
    req.udp_bandwidths.clear();
    let state = state_with_pair();
    let cfg = config_from_request(&state, &req);
    assert_eq!(cfg.iperf.tcp_windows, state.cfg.iperf.tcp_windows);
    let tcp: Vec<&TestSpec> = cfg
        .tests
        .iter()
        .filter(|t| t.transports.contains(&"tcp".to_string()))
        .collect();
    assert_eq!(tcp.len(), 1, "没填 -P 时按单档跑");
    assert_eq!(tcp[0].tcp_streams, Some(1));
}

/// 界面产出的 config 必须能被 builder 直接消化——控制台不是第二条
/// 执行路径，它只是 config 的图形编辑器。
#[test]
fn the_generated_config_builds_real_units() {
    let state = state_with_pair();
    let cfg = config_from_request(&state, &request());
    let spec = builder::spec_from_config(&cfg.tests[0], &cfg, &state.master, &state.agent)
        .expect("界面生成的 TestSpec 必须可解析");
    let mut port = builder::PORT_BASE;
    let (units, _) = build_units(&[spec], cfg.require_same_subnet_for_iperf, &mut port);
    assert!(!units.is_empty(), "应生成任务");
    assert!(units.iter().any(|u| u.bidir), "勾了双向就该有双向单元");
}

#[test]
fn a_selection_that_builds_zero_units_is_rejected_before_run() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].ip = vec!["v6".into()];
    let cfg = validated_config_from_request(&state, &req).expect("请求字段本身有效");
    let error = ensure_config_builds_units(&cfg, &state).unwrap_err();
    assert!(error.contains("没有生成任何测试单元"));
    assert!(error.contains("缺少可用的 IPv6 地址"));
}

/// 网段前缀必须能在界面上改。默认只放行 `192.168.`，在 10.x / 172.x 的
/// 实验网里会把整张网卡表过滤成空——而控制台存在的意义正是让人不必回去
/// 手改 config.json。清空 = 列出全部网口，也必须是一个能表达的选择。
#[test]
fn the_console_can_change_which_subnets_show_up() {
    let parse = |body: &str| serde_json::from_str::<ConnectReq>(body).expect("解析连接参数");

    let req = parse(r#"{"host":"10.0.0.2","ipv4_prefixes":[" 10.228. ","172.16.",""]}"#);
    assert_eq!(
        cleaned_list(req.ipv4_prefixes.as_deref().expect("提交了前缀")),
        vec!["10.228.", "172.16."],
        "手抄进来的空白和空项要清掉"
    );

    // 提交空列表（用户把框清空）和根本没提交这个字段，是两件事。
    let emptied = parse(r#"{"host":"10.0.0.2","ipv4_prefixes":[]}"#).ipv4_prefixes;
    assert_eq!(
        emptied.as_deref().map(cleaned_list),
        Some(Vec::new()),
        "清空 = 显式要求列出全部网口"
    );
    assert_eq!(
        parse(r#"{"host":"10.0.0.2"}"#).ipv4_prefixes,
        None,
        "没提交就沿用已加载的配置，不能被当成清空"
    );
}

/// 界面上填的网段前缀要一路带进真正下发的 config，否则改了等于没改。
#[test]
fn the_chosen_subnets_reach_the_config_that_actually_runs() {
    let mut state = state_with_pair();
    state.cfg.ipv4_prefixes = vec!["10.228.".into()];
    let cfg = config_from_request(&state, &request());
    assert_eq!(cfg.ipv4_prefixes, vec!["10.228."]);
}

/// `-l` 档位要和 `-b` 取组合，并且真的变成命令行上的 `-l`。
#[test]
fn udp_datagram_size_steps_cross_with_bandwidth_steps() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].transports = vec!["udp".into()];
    req.pairs[0].directions = vec!["ab".into()];
    req.nic_policies
        .iter_mut()
        .for_each(|p| p.udp_bandwidth.clear());
    req.udp_bandwidths = vec!["100m".into(), "500m".into()];
    req.udp_lengths = vec!["64".into(), "1400".into()];

    let cfg = config_from_request(&state, &req);
    let udp = cfg
        .tests
        .iter()
        .find(|t| t.transports.contains(&"udp".to_string()))
        .expect("应有 UDP spec");
    let profiles = udp.udp_profiles.as_ref().expect("应有档位");
    let mut combos: Vec<(String, Option<String>)> = profiles
        .iter()
        .map(|p| (p.bandwidth.clone(), p.length.clone()))
        .collect();
    combos.sort();
    assert_eq!(
        combos,
        vec![
            ("100m".to_string(), Some("64".to_string())),
            ("100m".to_string(), Some("1400".to_string())),
            ("500m".to_string(), Some("64".to_string())),
            ("500m".to_string(), Some("1400".to_string())),
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>(),
        "2 个 -b × 2 个 -l = 4 档"
    );

    // 一路建到真实命令，确认 -l 没有在中途被丢掉。
    let specs: Vec<_> = cfg
        .tests
        .iter()
        .map(|t| builder::spec_from_config(t, &cfg, &state.master, &state.agent).expect("建 spec"))
        .collect();
    let mut port = builder::PORT_BASE;
    let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
    let mut sent: Vec<Vec<String>> = Vec::new();
    for unit in &units {
        for leg in &unit.legs {
            match &leg.kind {
                builder::LegKind::IperfSingle(task) => sent.push(task.extra.clone()),
                builder::LegKind::IperfGroup { streams, .. } => {
                    sent.extend(streams.iter().map(|task| task.extra.clone()))
                }
                _ => {}
            }
        }
    }
    assert!(!sent.is_empty(), "应当建出 iperf 任务");
    for extra in &sent {
        let at = extra
            .iter()
            .position(|arg| arg == "-l")
            .unwrap_or_else(|| panic!("每条 UDP 命令都要带 -l: {extra:?}"));
        assert!(
            matches!(
                extra.get(at + 1).map(String::as_str),
                Some("64") | Some("1400")
            ),
            "{extra:?}"
        );
    }
}

/// `-l` 留空时不能凭空写一个值进去：「没指定」和「指定成某个数」
/// 在报告里是两回事。
#[test]
fn a_blank_datagram_size_sends_no_l_flag_at_all() {
    let mut req = request();
    req.nic_policies
        .iter_mut()
        .for_each(|p| p.udp_bandwidth.clear());
    req.udp_lengths = vec!["  ".into(), String::new()];
    let cfg = config_from_request(&state_with_pair(), &req);
    let udp = cfg
        .tests
        .iter()
        .find(|t| t.transports.contains(&"udp".to_string()))
        .expect("应有 UDP spec");
    let profiles = udp.udp_profiles.as_ref().expect("应有档位");
    assert_eq!(profiles.len(), 3, "只有三个 -b 档位");
    assert!(profiles.iter().all(|p| p.length.is_none()));
}

/// 按网口钉死 -b 的方向，-l 档位仍要逐档跑：钉住的是带宽不是报文长度。
#[test]
fn pinning_the_bandwidth_does_not_pin_the_datagram_size() {
    let mut req = request();
    req.pairs[0].transports = vec!["udp".into()];
    req.pairs[0].directions = vec!["ab".into()];
    req.udp_lengths = vec!["64".into(), "1400".into()];
    let cfg = config_from_request(&state_with_pair(), &req);
    let pinned = cfg
        .tests
        .iter()
        .find(|t| t.direction.directions() == ["ab"])
        .expect("ab 被钉死");
    let profiles = pinned.udp_profiles.as_ref().expect("应有档位");
    assert_eq!(profiles.len(), 2, "两个 -l 档位各一份");
    assert!(profiles.iter().all(|p| p.length.is_some()));
}

/// 控制台默认不裁剪 -b，勾上才裁剪；配置文件里的值不参与。
#[test]
fn the_console_decides_clipping_regardless_of_the_config_file() {
    let mut state = state_with_pair();
    state.cfg.limit_udp_by_link_speed = true;

    let req = request();
    assert!(
        !config_from_request(&state, &req).limit_udp_by_link_speed,
        "界面没勾就不裁剪，配置文件里的 true 不能悄悄生效"
    );

    let mut on = request();
    on.limit_udp_by_link_speed = true;
    assert!(config_from_request(&state, &on).limit_udp_by_link_speed);
}

fn console_with(state: UiState) -> Arc<Console> {
    Arc::new(Console {
        state: Mutex::new(state),
        running: AtomicBool::new(false),
        report: Mutex::new(String::new()),
        ui_token: String::new(),
        monitors: Mutex::new(HashMap::new()),
        run_status: Arc::new(RunStatusRecorder::new()),
    })
}

fn console_for_monitor_tests() -> Arc<Console> {
    console_with(state_with_pair())
}

/// 环形缓冲挤掉旧点之后，游标必须还指得对。
///
/// 游标是**绝对**序号（和 /api/progress 的 from 一个语义）。前端拿着一个
/// 早于缓冲起点的 from 回来时，正确做法是从现存最早的点接着给，
/// 而不是把 from 当成数组下标去切——那会静默错位，曲线看着还挺像样。
#[test]
fn monitor_cursor_survives_the_ring_buffer_dropping_old_points() {
    let console = console_for_monitor_tests();
    let data = Arc::new(Mutex::new(MonitorData {
        running: true,
        ..Default::default()
    }));
    {
        let mut d = lock_recover(&data);
        for i in 0..(MONITOR_MAX_POINTS + 120) {
            d.push(MonitorPoint {
                t: i as f64,
                rx_mbps: i as f64,
                tx_mbps: 0.0,
            });
        }
        assert_eq!(d.dropped, 120, "超出上限的点必须被挤掉并记数");
        assert_eq!(d.points.len(), MONITOR_MAX_POINTS);
    }
    lock_recover(&console.monitors).insert(
        "s1".into(),
        MonitorSession {
            side: "master".into(),
            iface: "eth0".into(),
            stop: Arc::new(AtomicBool::new(false)),
            data,
            started: std::time::Instant::now(),
        },
    );

    // from=0 的落后游标：从现存最早的点开始给，而不是从数组第 0 个。
    let out = api_monitor_samples(&console, r#"{"cursors":[{"session":"s1","from":0}]}"#).unwrap();
    let first = &out["series"][0];
    assert_eq!(
        first["points"][0]["rx_mbps"], 120.0,
        "第一个点应是未被挤掉的最早点"
    );
    assert_eq!(first["from"], (MONITOR_MAX_POINTS + 120) as u64);
    assert_eq!(
        first["points"].as_array().unwrap().len(),
        MONITOR_MAX_POINTS,
        "落后游标应拿到缓冲里现有的全部"
    );

    // 追平之后再问，应该一个点都没有。
    let out = api_monitor_samples(
        &console,
        &format!(
            r#"{{"cursors":[{{"session":"s1","from":{}}}]}}"#,
            MONITOR_MAX_POINTS + 120
        ),
    )
    .unwrap();
    assert!(
        out["series"][0]["points"].as_array().unwrap().is_empty(),
        "追平后不该重发"
    );

    api_monitor_stop(&console, r#"{"session":"s1"}"#).unwrap();
    // 停掉的那一路只报自己那一条，不能让整次批量取样失败——同一次请求里
    // 还有别的曲线好好地在跑。
    let out = api_monitor_samples(&console, r#"{"cursors":[{"session":"s1","from":0}]}"#).unwrap();
    assert_eq!(out["series"][0]["running"], false);
    assert_eq!(out["series"][0]["error"], "监控会话已结束");
    // 再停一次不能 panic：页面上快速点两下停止是常事。
    let again = api_monitor_stop(&console, r#"{"session":"s1"}"#).unwrap();
    assert_eq!(again["stopped"], false);
}

/// 采样间隔的上限跟着监控端走。
///
/// agent 自己会把间隔夹到 200–5000ms，这边不跟着夹的话，选 10 秒会变成
/// 「对面按 5 秒采、这边按 10 秒只取最后一个样本」——一半样本无声丢掉，
/// 而同样选 10 秒监控本机却是对的。同一个输入框不能有两种语义。
#[test]
fn the_sampling_interval_ceiling_follows_which_side_is_being_watched() {
    assert_eq!(monitor_interval_ms("master", 0), 1_000, "0 = 用默认值");
    assert_eq!(monitor_interval_ms("agent", 0), 1_000);

    assert_eq!(
        monitor_interval_ms("master", 10_000),
        10_000,
        "本机自己 sleep，给多少是多少"
    );
    assert_eq!(
        monitor_interval_ms("agent", 10_000),
        5_000,
        "辅测机侧不能超过 agent 自己的夹紧上限"
    );
    assert_eq!(
        monitor_interval_ms("agent", 5_000),
        5_000,
        "正好在上限上要放行"
    );

    assert_eq!(monitor_interval_ms("master", 10), 200, "下限两端一致");
    assert_eq!(monitor_interval_ms("agent", 10), 200);
    assert_eq!(monitor_interval_ms("master", 999_999), 60_000);
}

/// 监控不受「有没有测试在跑」约束——边跑边看正是它最有用的场景。
/// 同时确认网卡名不存在时是一条错误信息，不是 panic、也不是假装在测。
#[test]
fn monitoring_starts_while_a_run_is_in_flight_and_reports_a_bad_interface() {
    let console = console_for_monitor_tests();
    console.running.store(true, Ordering::SeqCst);

    let started = api_monitor_start(
        &console,
        r#"{"side":"master","iface":"cpe-no-such-iface","interval_ms":200}"#,
    )
    .expect("测试在跑也必须能起监控");
    let session = started["session"].as_str().unwrap().to_string();

    // 采样线程读不到计数器会立刻收摊并写下错误。
    let mut error = String::new();
    for _ in 0..50 {
        let out = api_monitor_samples(
            &console,
            &format!(r#"{{"cursors":[{{"session":"{session}","from":0}}]}}"#),
        )
        .unwrap();
        let series = &out["series"][0];
        error = series["error"].as_str().unwrap_or_default().to_string();
        if !error.is_empty() && series["running"] == false {
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(!error.is_empty(), "网卡名不存在必须给出可读的错误");

    api_monitor_stop(&console, &format!("{{\"session\":\"{session}\"}}")).unwrap();
}

/// 用真实 tiny_http + handle() 把口令闸门跑一遍。
///
/// 重点是**页面本身也要挡**：`/` 返回的 HTML 里没有口令，但放行未认证的
/// 页面请求就等于把控制台的整个界面（以及它能做什么）展示给任何来问的人，
/// 而 API 401 之后界面只会是一屏报错——不如在门口就说清楚。
#[test]
fn the_console_token_gate_covers_both_the_page_and_the_api() {
    let console = Arc::new(Console {
        state: Mutex::new(state_with_pair()),
        running: AtomicBool::new(false),
        report: Mutex::new(String::new()),
        ui_token: "unit-secret".into(),
        monitors: Mutex::new(HashMap::new()),
        run_status: Arc::new(RunStatusRecorder::new()),
    });
    // Server 要留在外面：incoming_requests() 会一直阻塞，只有 unblock()
    // 能让它收场。整个 move 进线程就再也够不着它了，端口和线程会挂到
    // 测试进程结束。
    let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
    let port = server.server_addr().to_ip().unwrap().port();
    let worker = Arc::clone(&console);
    let worker_server = Arc::clone(&server);
    let thread = std::thread::spawn(move || {
        for request in worker_server.incoming_requests() {
            handle(request, &worker);
        }
    });
    // 预算必须大于**被测端点自己允许的耗时**，否则这条测试迟早会因为
    // 环境慢而不是闸门坏而红。`/api/local` 带对口令那一发会真的去扫本机：
    // Windows 上 `ipconfig /all` 允许 20s、每块 Wi-Fi 卡的 `netsh` 10s、
    // `iperf3 --version` 8s，加起来远超原来写的 5s——之前一直绿只是因为
    // CI 机器上没有 iperf3、扫描又快，5s 侥幸够用。Windows runner 上并行
    // 跑测试把扫描拖过 5s 时，它就报成「读头失败 (os error 10060)」，
    // 看起来像闸门坏了。这里给的是超时上限，正常路径仍然是毫秒级返回。
    let wait = Duration::from_secs(60);

    let (status, _) = crate::http_client::get("127.0.0.1", port, "/", wait).unwrap();
    assert_eq!(status, 401, "页面本身也必须要口令");

    let (status, _) = crate::http_client::get("127.0.0.1", port, "/api/local", wait).unwrap();
    assert_eq!(status, 401, "不带口令的 API 必须 401");

    let (status, _) =
        crate::http_client::get_auth("127.0.0.1", port, "/api/local", "wrong", wait).unwrap();
    assert_eq!(status, 401, "口令错必须 401");

    let (status, body) =
        crate::http_client::get("127.0.0.1", port, "/api/local?token=unit-secret", wait).unwrap();
    assert_eq!(status, 200, "查询串带对口令必须放行：这是浏览器唯一的入口");
    assert!(body.contains("\"ok\":true"), "{body}");

    let (status, _) =
        crate::http_client::get_auth("127.0.0.1", port, "/api/local", "unit-secret", wait).unwrap();
    assert_eq!(status, 200, "Bearer 带对口令必须放行");

    server.unblock();
    thread.join().expect("请求线程正常收场");
}

/// Ctrl+C 之后要不要退，取决于「这一刻有没有测试在跑」。
///
/// 跑着的时候退掉就是把报告扔了：那次 Ctrl+C 的语义是「优雅结束当前单元
/// 并出报告」，控制台得活到 run_master 写完。等它收完尾再退。
#[test]
fn the_console_only_quits_once_the_run_it_was_hosting_has_wound_down() {
    assert!(!should_shut_down(false, false), "没按 Ctrl+C 就不该退");
    assert!(!should_shut_down(false, true), "没按 Ctrl+C 更不该退");
    assert!(
        !should_shut_down(true, true),
        "测试还在跑：先让它把报告写完，这一拍不能退"
    );
    assert!(should_shut_down(true, false), "空闲时按 Ctrl+C 必须退出");
}

/// 取请求循环必须有出口。
///
/// 原来是 `while let Ok(request) = server.recv()`——没有超时、不查任何标志，
/// 于是 `run_master()` 一旦把 ctrlc handler 装上（它只置标志、不退进程），
/// SIGINT 就被永久吃掉，控制台再也关不掉。这条用真 server 跑一遍，
/// 确认置位之后循环真的会返回，而不是靠「应该会吧」。
#[test]
fn the_request_loop_returns_once_the_shutdown_flag_is_set() {
    let console = console_for_monitor_tests();
    let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_server = Arc::clone(&server);
    let worker_console = Arc::clone(&console);
    let worker_shutdown = Arc::clone(&shutdown);
    let thread = std::thread::spawn(move || {
        serve(&worker_server, &worker_console, &worker_shutdown);
    });

    // 没有任何请求进来，循环应当停在 recv_timeout 上空转而不是退出。
    std::thread::sleep(SHUTDOWN_POLL * 3);
    assert!(!thread.is_finished(), "没置位就不该自己退出");

    shutdown.store(true, Ordering::SeqCst);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !thread.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(thread.is_finished(), "置位后必须在一个轮询周期内返回");
    thread.join().expect("请求线程正常收场");
}

/// 进程退出前要把监控会话收干净，尤其是辅测机侧那路。
#[test]
fn shutting_down_stops_every_monitor_session() {
    let console = console_for_monitor_tests();
    for name in ["a", "b"] {
        lock_recover(&console.monitors).insert(
            name.into(),
            MonitorSession {
                side: "master".into(),
                iface: "eth0".into(),
                stop: Arc::new(AtomicBool::new(false)),
                data: Arc::new(Mutex::new(MonitorData {
                    running: true,
                    ..Default::default()
                })),
                started: std::time::Instant::now(),
            },
        );
    }

    stop_all_monitors(&console);

    assert!(
        lock_recover(&console.monitors).is_empty(),
        "退出时不能留下任何会话"
    );
}

/// 只有明确写成回环的地址才算回环——判错方向要往「多要一个口令」偏，
/// 反过来把一个真的可路由的地址当成回环，就是无声开洞。
#[test]
fn only_explicit_loopback_addresses_count_as_local() {
    for local in [
        "127.0.0.1",
        "127.1.2.3",
        "localhost",
        "LOCALHOST",
        "::1",
        " 127.0.0.1 ",
    ] {
        assert!(bind_is_loopback(local), "{local} 应判为回环");
    }
    for exposed in ["0.0.0.0", "192.168.8.101", "::", "10.0.0.1", ""] {
        assert!(!bind_is_loopback(exposed), "{exposed} 不该判为回环");
    }
}

/// 三种带口令的方式都要认；没配口令时一律放行（回环下的默认形态）。
#[test]
fn the_console_accepts_its_token_from_query_header_or_bearer() {
    assert!(
        request_is_authorized("", "", None, None),
        "没设口令就不该拦任何人"
    );

    let ok = |query: &str, header: Option<&str>, bearer: Option<&str>| {
        request_is_authorized("s3cr3t", query, header, bearer)
    };
    assert!(ok("token=s3cr3t", None, None), "地址栏里只能靠查询串");
    assert!(ok("from=3&token=s3cr3t", None, None), "查询串里位置不固定");
    assert!(ok("", Some("s3cr3t"), None), "页面之后走请求头");
    assert!(ok("", None, Some("s3cr3t")), "curl 复现问题时走 Bearer");

    assert!(!ok("", None, None), "什么都不带必须拒绝");
    assert!(!ok("token=wrong", None, None), "口令错必须拒绝");
    assert!(!ok("", Some("wrong"), None));
    assert!(!ok("mytoken=s3cr3t", None, None), "后缀撞名不算带对口令");
}

/// 畸形的百分号转义不能让控制台崩掉——这段输入来自网络。
///
/// `%` 后面跟多字节字符时，按 `&str` 下标切那两位会切在字符中间直接 panic；
/// 这里逐条钉住几种畸形写法都只是「原样留下那个 %」。
#[test]
fn a_malformed_percent_escape_never_panics_the_query_parser() {
    for raw in ["%", "%4", "%中文", "%zz", "abc%", "%%41", "中%文字"] {
        let decoded = urldecode(raw);
        assert!(!decoded.is_empty(), "{raw} 不该解出空串");
    }
    assert_eq!(urldecode("%41%42"), "AB");
    assert_eq!(urldecode("a+b"), "a b");
    assert_eq!(urldecode("%zz"), "%zz", "解不动的转义原样保留");
}

/// 口令里的特殊字符经过 URL 编码往返后必须还是同一个串，
/// 否则「照着打印的地址打开」会打不开。
#[test]
fn a_token_with_awkward_characters_survives_the_printed_url() {
    let token = "a b&c=d%e/f中文";
    let encoded = urlencode(token);
    assert!(!encoded.contains(' ') && !encoded.contains('&'));
    assert!(request_is_authorized(
        token,
        &format!("token={encoded}"),
        None,
        None
    ));
}

/// 矩阵里勾 PING 必须真的产出 ping 单元，而且次数/包长走界面填的值。
///
/// 界面把 PING 和 TCP/UDP 并排放在「协议」列，但配置模型里它是 `kinds`
/// 不是 `transports`；这条用例同时钉住这层映射和「只勾 PING 时不冒出
/// iperf 单元」。
#[test]
fn checking_ping_in_the_matrix_produces_ping_units_with_the_typed_budget() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].transports = vec!["ping".into()];
    req.ping_count = 5;
    req.ping_payload_sizes = vec![64, 1400];

    // 走完整链路而不是直接调 config_from_request：这条测试曾经是绿的，
    // 而 PING 在 validate_request 那一关整个被挡住——绕过校验的测试
    // 保不住「勾了 PING 真的能跑」这件事。
    let cfg = validated_config_from_request(&state, &req).expect("勾 PING 必须能过校验");
    let ping: Vec<_> = cfg
        .tests
        .iter()
        .filter(|t| t.kinds.iter().any(|k| k == "ping"))
        .collect();
    assert_eq!(ping.len(), 1, "勾了 PING 就该有一个 ping 测试项");
    assert_eq!(ping[0].ping_count, Some(5), "次数必须用界面填的");
    assert_eq!(
        ping[0].ping_payload_sizes.as_deref(),
        Some(&[64u32, 1400][..]),
        "包长档位必须用界面填的，回落到默认的三档会平白多跑几分钟"
    );
    assert!(ping[0].transports.is_empty(), "ping 单元不带 transport");
    assert!(
        cfg.tests
            .iter()
            .all(|t| !t.kinds.iter().any(|k| k == "iperf")),
        "只勾 PING 时不该冒出 iperf 单元"
    );
    ensure_config_builds_units(&cfg, &state).expect("ping 选择必须能构建出单元");
}

/// 双向门限按配对填，只在勾了「双向」时落进 config。
///
/// 按网卡填是不够的：同一块 RNDIS 口，和 Wi-Fi 组双向、和 SGMII 组双向，
/// 能收到的速率完全不是一个量级——一个数没法同时对两组成立。
#[test]
fn the_bidirectional_threshold_is_per_pair_and_only_applies_to_bidirectional_units() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].directions = vec!["ab".into(), "bidir".into()];
    req.pairs[0].rx_target_bidir_ab = "1000".into();
    req.pairs[0].rx_target_bidir_ba = "800".into();

    let cfg = validated_config_from_request(&state, &req).expect("应能过校验");
    let targets = cfg.tests[0]
        .rate_targets_bidir_mbps
        .as_ref()
        .expect("勾了双向且填了值就该落进 config");
    assert_eq!(targets.ab, Some(1000.0));
    assert_eq!(targets.ba, Some(800.0));
    assert_eq!(targets.forward, None, "双向门限没有 forward 这个概念");

    // 没勾双向时不写进 config——否则它会出现在下载的 config.json 里，
    // 让人以为在生效。
    let mut one_way = request();
    one_way.pairs[0].directions = vec!["ab".into()];
    assert!(config_from_request(&state, &one_way).tests[0]
        .rate_targets_bidir_mbps
        .is_none());
}

/// 填了双向门限却没勾双向，要当场报错而不是静默忽略。
///
/// 静默忽略的后果是：人以为门限已经放低，看到 FAIL 就去查链路，
/// 而真正的原因是那个数从头到尾没生效过。
#[test]
fn a_bidirectional_threshold_without_the_bidirectional_box_is_rejected() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].directions = vec!["ab".into()];
    req.pairs[0].rx_target_bidir_ab = "1000".into();

    let error = validate_request(&state, &req).expect_err("必须报错");
    assert!(error.contains("双向"), "{error}");
}

/// 双向门限只收绝对 Mbps：百分比按单块网卡的协商速率换算，
/// 而它说的是两块口并发时的能力，两者不成比例。
#[test]
fn a_percentage_bidirectional_threshold_is_rejected_with_the_reason() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].directions = vec!["bidir".into()];
    req.pairs[0].rx_target_bidir_ab = "50%".into();

    let error = validate_request(&state, &req).expect_err("百分比必须被拒");
    assert!(error.contains("绝对 Mbps"), "错误要说清为什么：{error}");

    req.pairs[0].rx_target_bidir_ab = "1000".into();
    assert!(validate_request(&state, &req).is_ok(), "绝对值要放行");
}

fn state_with_two_pairs() -> UiState {
    let nic = |name: &str, role: &str, ip: &str| NicInfo {
        name: name.into(),
        role: role.into(),
        ipv4: ip.into(),
        speed_mbps: 2500,
        ..Default::default()
    };
    UiState {
        cfg: Config::default(),
        agent_host: "10.0.0.2".into(),
        master: HostInfo {
            hostname: "m".into(),
            os: "test".into(),
            interfaces: vec![
                nic("以太网 6", "SGMII2.5G", "192.168.0.101"),
                nic("以太网 7", "SGMII1G", "192.168.0.102"),
            ],
        },
        agent: HostInfo {
            hostname: "a".into(),
            os: "test".into(),
            interfaces: vec![
                nic("WLAN 3", "WIFI5G", "192.168.0.104"),
                nic("USB 4", "RNDIS", "192.168.0.105"),
            ],
        },
    }
}

/// `-b` 在预览里按 Mbps 显示，别的参数照抄。
///
/// 下发的是精确 bit/s 整数，原样打印是 `-b 1000000000`——十个零要一个个数，
/// 而且抄回输入框会变成 10^9 Mbps（那里的裸数字按 Mbps 算）。
#[test]
fn the_plan_renders_bandwidth_in_mbps() {
    let args =
        |items: &[&str]| -> Vec<String> { items.iter().map(|item| item.to_string()).collect() };
    assert_eq!(
        readable_args(&args(&["-b", "1000000000", "-l", "1200"])),
        "-b 1000 Mbps -l 1200"
    );
    // 裁剪之后常常不是整 Mbps，别把它抹成整数。
    assert_eq!(
        readable_args(&args(&["-b", "2600500000"])),
        "-b 2600.5 Mbps"
    );
    // TCP 那条没有 -b，原样照抄。
    assert_eq!(
        readable_args(&args(&["-w", "4m", "-P", "10"])),
        "-w 4m -P 10"
    );
}

/// 把 bit/s 填进按 Mbps 解释的输入框，要当场说清楚，而不是拿着
/// 10^9 Mbps 去灌包。
#[test]
fn a_bandwidth_that_is_off_by_a_million_is_rejected() {
    let state = state_with_pair();
    let mut req = request();
    req.udp_bandwidths = vec!["1000000000".into()];
    let error = validate_request(&state, &req).expect_err("必须报错");
    assert!(
        error.contains("按 Mbps 算"),
        "错误要说清是单位填错了：{error}"
    );

    // 加了后缀就是正常的 1000Mbps，必须放行。
    for ok in ["1000m", "1G", "1000mbps"] {
        let mut req = request();
        req.udp_bandwidths = vec![ok.into()];
        assert!(validate_request(&state, &req).is_ok(), "{ok} 应当合法");
    }
}

/// 「预览任务」要把每条腿最终下发的参数摆出来。
///
/// 优先级（网口固定值 > 参数组 > 默认组）和链路裁剪都会改写这几个数字，
/// 而这两件事都发生在人看不见的地方。摆出来之后，填错了在跑之前就能发现。
#[test]
fn the_plan_shows_the_parameters_each_leg_will_actually_use() {
    let state = state_with_pair();
    let mut req = request();
    req.nic_policies.clear();
    req.udp_bandwidths = vec!["500m".into()];
    req.udp_lengths = vec!["1200".into()];
    req.udp_streams = 3;
    req.pairs[0].directions = vec!["ab".into()];
    req.pairs[0].transports = vec!["udp".into()];
    let cfg = validated_config_from_request(&state, &req).unwrap();

    let specs: Vec<_> = cfg
        .tests
        .iter()
        .map(|test| builder::spec_from_config(test, &cfg, &state.master, &state.agent).unwrap())
        .collect();
    let mut port = builder::PORT_BASE;
    let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
    let lines = unit_load_lines(&units[0]);

    assert_eq!(lines.len(), 1, "单向单元只有一条腿");
    assert!(lines[0].contains("-b 500 Mbps"), "{lines:?}");
    assert!(lines[0].contains("-l 1200"), "{lines:?}");
    assert!(lines[0].contains("×3 流"), "{lines:?}");
    // 单向单元也要带方向。这条腿的 tag 是空串（`dir_pairs` 对 ab/ba 就给空），
    // 所以方向只能来自单元自己的 `direction`；不兜的话预览里双向单元每行带
    // 方向、单向单元不带，同一份清单两种样子。
    assert!(
        lines[0].starts_with("A→B "),
        "单向单元的参数行要带方向：{lines:?}"
    );
}

/// 预览里**每一种**方向都要看得见，不能只有双向单元带方向标。
#[test]
fn the_preview_labels_the_direction_of_every_unit() {
    let state = state_with_pair();
    let mut req = request();
    req.udp_bandwidths = vec!["500m".into()];
    req.udp_streams = 1;
    req.pairs[0].transports = vec!["udp".into()];

    for (direction, expected) in [("ab", "A→B "), ("ba", "B→A "), ("bidir", "")] {
        req.pairs[0].directions = vec![direction.into()];
        let cfg = validated_config_from_request(&state, &req).unwrap();
        let specs: Vec<_> = cfg
            .tests
            .iter()
            .map(|test| builder::spec_from_config(test, &cfg, &state.master, &state.agent).unwrap())
            .collect();
        let mut port = builder::PORT_BASE;
        let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
        let lines = unit_load_lines(&units[0]);
        assert!(!lines.is_empty(), "{direction} 应当有参数行");
        if direction == "bidir" {
            // 双向单元两条腿各自带 ab/ba，靠 Leg.tag 就够。
            assert!(
                lines.iter().any(|line| line.starts_with("A→B "))
                    && lines.iter().any(|line| line.starts_with("B→A ")),
                "双向单元两条腿要分别标出方向：{lines:?}"
            );
        } else {
            assert!(
                lines.iter().all(|line| line.starts_with(expected)),
                "{direction} 的参数行要以 {expected:?} 开头：{lines:?}"
            );
        }
    }
}

/// 每一行选哪一组 UDP 参数，就跑那一组里写着的东西。
///
/// 「这几对 2500m 单流、那几对 1000m/500m 四流、还有几对带 -l」是一轮里
/// 最常见的安排，而执行区那份档位是所有勾中的配对共用的，表达不了。
#[test]
fn each_row_runs_the_udp_group_it_points_at() {
    let state = state_with_two_pairs();
    let mut req = request();
    req.nic_policies.clear();
    // 默认组：-b 1m、不带 -l、2 流。
    req.udp_bandwidths = vec!["1m".into()];
    req.udp_lengths = Vec::new();
    req.udp_streams = 2;
    req.udp_groups = vec![
        UdpGroup {
            name: "单流打满".into(),
            bandwidths: vec!["2500m".into()],
            lengths: vec!["64".into()],
            windows: Vec::new(),
            streams: 1,
        },
        UdpGroup {
            name: "多流".into(),
            bandwidths: vec!["1000m".into(), "500m".into()],
            lengths: Vec::new(),
            windows: Vec::new(),
            streams: 4,
        },
    ];
    req.pairs[0].directions = vec!["ab".into()];
    req.pairs[0].udp_groups = vec![1];
    let mut second = req.pairs[0].clone();
    second.src = "master:NAME=以太网 7".into();
    second.dst = "agent:NAME=USB 4".into();
    second.udp_groups = vec![2];
    let mut third = req.pairs[0].clone();
    third.src = "master:NAME=以太网 6".into();
    third.dst = "agent:NAME=USB 4".into();
    third.udp_groups = vec![0];
    req.pairs.push(second);
    req.pairs.push(third);

    let cfg = validated_config_from_request(&state, &req).expect("三行都该合法");
    // 单元名带着组号：同一对选两组时两批单元必须区分得开，否则 resume id
    // 撞车、互相覆盖。默认组沿用原名，改名会让历史 PASS 全部失效。
    let spec = |name: &str| {
        cfg.tests
            .iter()
            .find(|test| test.name == name)
            .unwrap_or_else(|| panic!("找不到单元 {name}"))
    };
    let profiles = |name: &str| -> Vec<(String, Option<String>)> {
        spec(name)
            .udp_profiles
            .as_ref()
            .unwrap()
            .iter()
            .map(|profile| (profile.bandwidth.clone(), profile.length.clone()))
            .collect()
    };

    assert_eq!(
        profiles("ui-1-udp-g2"),
        vec![("2500m".into(), Some("64".into()))]
    );
    assert_eq!(spec("ui-1-udp-g2").udp_streams, Some(1));
    assert_eq!(
        profiles("ui-2-udp-g3"),
        vec![("1000m".into(), None), ("500m".into(), None)],
        "组里没填 -l 就是不下发，不继承默认组"
    );
    assert_eq!(spec("ui-2-udp-g3").udp_streams, Some(4));
    assert_eq!(
        profiles("ui-3-udp"),
        vec![("1m".into(), None)],
        "没选组的行跑默认组，单元名不带后缀"
    );
    assert_eq!(spec("ui-3-udp").udp_streams, Some(2));
    // 默认组的档位仍然写回 iperf.udp_profiles：下载出来的 config 交给
    // master --auto 时读的是这一份。
    assert_eq!(
        cfg.iperf
            .udp_profiles
            .iter()
            .map(|profile| profile.bandwidth.clone())
            .collect::<Vec<_>>(),
        vec!["1m"]
    );
}

/// 同一行选两组 = 这一对跑两批，参数各按各的组来。
///
/// 矩阵里一对网口只有一行，不能多选的话「既按常规档位跑一遍、又用 1m 单流
/// 跑一遍」只能分两轮、出两份报告。
#[test]
fn one_row_can_run_several_groups() {
    let state = state_with_pair();
    let mut req = request();
    req.nic_policies.clear();
    req.udp_bandwidths = vec!["1000m".into()];
    req.udp_streams = 4;
    req.udp_groups = vec![UdpGroup {
        name: "慢速单流".into(),
        bandwidths: vec!["1m".into()],
        lengths: Vec::new(),
        windows: Vec::new(),
        streams: 1,
    }];
    req.pairs[0].directions = vec!["ab".into()];
    req.pairs[0].transports = vec!["udp".into()];
    req.pairs[0].udp_groups = vec![0, 1];

    let cfg = validated_config_from_request(&state, &req).unwrap();
    let udp: Vec<&TestSpec> = cfg
        .tests
        .iter()
        .filter(|test| test.transports.iter().any(|t| t == "udp"))
        .collect();
    assert_eq!(udp.len(), 2, "两组 = 两批单元");
    assert_eq!(udp[0].name, "ui-1-udp");
    assert_eq!(udp[0].udp_streams, Some(4));
    assert_eq!(
        udp[1].name, "ui-1-udp-g2",
        "组号进单元名，resume id 才不撞车"
    );
    assert_eq!(udp[1].udp_streams, Some(1));

    // 同一组选两次不该跑两遍：两批同名单元在 resume 里会互相覆盖。
    req.pairs[0].udp_groups = vec![1, 1, 0];
    let cfg = validated_config_from_request(&state, &req).unwrap();
    assert_eq!(
        cfg.tests
            .iter()
            .filter(|test| test.transports.iter().any(|t| t == "udp"))
            .count(),
        2
    );

    // 不带这个字段（老页面、手写请求）= 只跑默认组。
    req.pairs[0].udp_groups = Vec::new();
    let cfg = validated_config_from_request(&state, &req).unwrap();
    let udp: Vec<&TestSpec> = cfg
        .tests
        .iter()
        .filter(|test| test.transports.iter().any(|t| t == "udp"))
        .collect();
    assert_eq!(udp.len(), 1);
    assert_eq!(udp[0].name, "ui-1-udp");
}

/// 组是完整定义，不继承默认组——空的 `-b` 生成不出任何单元，要当场挡住。
/// 选了组却没勾 UDP 同理：那一组一个单元都不会跑。
#[test]
fn a_group_that_would_run_nothing_is_rejected() {
    let state = state_with_pair();

    let mut req = request();
    req.udp_groups = vec![UdpGroup {
        name: String::new(),
        bandwidths: Vec::new(),
        lengths: vec!["64".into()],
        windows: Vec::new(),
        streams: 1,
    }];
    let error = validate_request(&state, &req).expect_err("没填 -b 必须报错");
    assert!(error.contains("没填 -b"), "{error}");

    let mut req = request();
    req.udp_groups = vec![UdpGroup {
        name: String::new(),
        bandwidths: vec!["2500m".into()],
        lengths: Vec::new(),
        windows: Vec::new(),
        streams: 1,
    }];
    req.pairs[0].udp_groups = vec![1];
    req.pairs[0].transports = vec!["tcp".into()];
    let error = validate_request(&state, &req).expect_err("没勾 UDP 必须报错");
    assert!(error.contains("没有勾 UDP"), "{error}");

    // 指向一个不存在的组：页面删组时没同步过来才会出现，静默按默认组跑
    // 等于跑了另一件事。
    let mut req = request();
    req.pairs[0].udp_groups = vec![3];
    let error = validate_request(&state, &req).expect_err("越界必须报错");
    assert!(error.contains("不存在"), "{error}");

    // 组里的档位写错要指名是哪一组。
    let mut req = request();
    req.udp_groups = vec![UdpGroup {
        name: "很快组".into(),
        bandwidths: vec!["很快".into()],
        lengths: Vec::new(),
        windows: Vec::new(),
        streams: 1,
    }];
    let error = validate_request(&state, &req).expect_err("必须报错");
    assert!(error.contains("很快组"), "{error}");
}

/// 清空 `-l` / `-w` 就是真的不下发它们，而不是替人填一个 iperf3 默认值。
///
/// 「不指定」和「指定成某个具体值」在报告里读起来完全不同，不能混。
#[test]
fn clearing_udp_length_and_window_emits_no_such_flags() {
    let state = state_with_pair();
    let mut req = request();
    req.udp_lengths = Vec::new();
    req.udp_windows = Vec::new();
    req.udp_bandwidths = vec!["1000m".into()];
    req.nic_policies.clear();

    let cfg = validated_config_from_request(&state, &req).unwrap();
    assert!(
        cfg.iperf
            .udp_profiles
            .iter()
            .all(|profile| profile.length.is_none() && profile.window.is_none()),
        "全局档位不该凭空长出 -l / -w"
    );
    let pair_profiles = cfg
        .tests
        .iter()
        .filter_map(|test| test.udp_profiles.as_ref())
        .flatten();
    for profile in pair_profiles {
        assert!(
            profile.length.is_none() && profile.window.is_none(),
            "逐对档位也不该凭空长出 -l / -w：{profile:?}"
        );
    }
}

/// 「下载 config.json」再导入回来，界面上的勾选必须原样回到原处。
///
/// 导入是下载的逆运算，这条测试是它唯一的判据：两边任何一处口径不一样，
/// 表现都是「导进来看着差不多、跑出来不是那份配置」——比报错难查得多。
#[test]
fn downloading_then_importing_restores_the_same_selection() {
    let state = state_with_pair();
    let req = request();
    let cfg = config_from_request(&state, &req);
    let file = serde_json::to_string(&cfg).unwrap();

    let console = console_with(state_with_pair());
    let out = api_import(&console, &file).expect("自己下载的配置必须能导回来");

    let pair = &out["pairs"][0];
    assert_eq!(pair["src"], "master:NAME=以太网 6");
    assert_eq!(pair["dst"], "agent:NAME=WLAN 3");
    assert_eq!(
        pair["directions"].as_array().unwrap(),
        &vec![json!("ab"), json!("bidir")],
        "方向要按原样回来，不能被 TCP/UDP 那几条 TestSpec 拆散"
    );
    assert_eq!(
        pair["transports"].as_array().unwrap(),
        &vec![json!("tcp"), json!("udp")]
    );
    assert_eq!(pair["ip"].as_array().unwrap(), &vec![json!("v4")]);
    // 网口上钉了 -b 的那种配置，文件里的 profile 带宽是占位值，不能被
    // 当成「这一行自己选的组」读回来。
    assert_eq!(
        pair["udp_groups"].as_array().unwrap(),
        &vec![json!(0)],
        "网口钉死时不认成附加组"
    );
    assert!(
        out["udp_groups"].as_array().unwrap().is_empty(),
        "不该凭空多出一个由占位值拼成的组"
    );

    let settings = &out["settings"];
    assert_eq!(settings["duration"], 60);
    assert_eq!(
        settings["tcp_windows"].as_array().unwrap(),
        &vec![json!("2m"), json!("4m"), json!("256m")]
    );
    assert_eq!(
        settings["tcp_streams"].as_array().unwrap(),
        &vec![json!(1), json!(5), json!(10)],
        "每个 -P 档位是一条 TestSpec，回填时要合回一个列表"
    );
    assert_eq!(
        settings["udp_bandwidths"].as_array().unwrap(),
        &vec![json!("1m"), json!("500m"), json!("1G")]
    );

    // 网口策略是另一半：门限和按口 -b 都在 link_profiles 里，漏了它
    // 「导入成功」就是一句空话。
    let policies = out["nic_policies"].as_array().unwrap();
    let master = policies
        .iter()
        .find(|policy| policy["endpoint"] == "master:NAME=以太网 6")
        .expect("主控网口策略");
    assert_eq!(master["rx_target"], "1800");
    assert_eq!(master["udp_bandwidth"], "2.6G");
}

/// 文件里不同的 UDP 参数要被认成组，并按行选回去。
#[test]
fn importing_rebuilds_the_udp_groups_from_the_tests() {
    let state = state_with_two_pairs();
    let mut req = request();
    req.nic_policies.clear();
    req.udp_bandwidths = vec!["1m".into()];
    req.udp_streams = 2;
    req.udp_groups = vec![UdpGroup {
        name: "多流".into(),
        bandwidths: vec!["1000m".into(), "500m".into()],
        lengths: Vec::new(),
        windows: Vec::new(),
        streams: 4,
    }];
    req.pairs[0].udp_groups = vec![1];
    let mut second = req.pairs[0].clone();
    second.src = "master:NAME=以太网 7".into();
    second.dst = "agent:NAME=USB 4".into();
    second.udp_groups = vec![0];
    req.pairs.push(second);
    let cfg = config_from_request(&state, &req);

    let console = console_with(state_with_two_pairs());
    let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
    let groups = out["udp_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "只有一种和默认组不同的打法");
    assert_eq!(
        groups[0]["bandwidths"].as_array().unwrap(),
        &vec![json!("1000m"), json!("500m")]
    );
    assert_eq!(groups[0]["streams"], 4);
    assert_eq!(
        out["pairs"][0]["udp_groups"].as_array().unwrap(),
        &vec![json!(1)],
        "第一行选那一组"
    );
    assert_eq!(
        out["pairs"][1]["udp_groups"].as_array().unwrap(),
        &vec![json!(0)],
        "第二行留在默认组"
    );
    // 默认组还是执行区那份，不该被某一行的组顶掉。
    assert_eq!(
        out["settings"]["udp_bandwidths"].as_array().unwrap(),
        &vec![json!("1m")]
    );
    assert_eq!(out["settings"]["udp_streams"], 2);
}

/// 当一端按网口固定 `-b`、另一端仍扫 UDP 档位时，导入不能把未固定方向
/// 的附加组误判成占位值而丢掉。矩阵编译会把这种组合拆成两个 test：
/// pinned 的一条只用于固定方向，swept 的一条仍带用户选择的 profiles。
#[test]
fn importing_keeps_udp_group_for_unpinned_direction() {
    let state = state_with_pair();
    let mut req = request();
    req.nic_policies = vec![NicPolicySelection {
        endpoint: "master:NAME=以太网 6".into(),
        rx_target: String::new(),
        udp_bandwidth: "3m".into(),
        udp_length: String::new(),
    }];
    req.udp_bandwidths = vec!["1m".into()];
    req.udp_lengths = vec!["1200".into()];
    req.udp_streams = 1;
    req.udp_groups = vec![UdpGroup {
        name: "高带宽".into(),
        bandwidths: vec!["500m".into()],
        lengths: vec!["1200".into()],
        windows: Vec::new(),
        streams: 1,
    }];
    req.pairs[0].transports = vec!["udp".into()];
    req.pairs[0].directions = vec!["ab".into(), "ba".into()];
    req.pairs[0].udp_groups = vec![1];

    let cfg = validated_config_from_request(&state, &req).expect("原始配置必须合法");
    assert!(cfg.tests.iter().any(|test| {
        test.direction.directions() == ["ab"]
            && test
                .udp_profiles
                .as_ref()
                .is_some_and(|profiles| profiles[0].bandwidth == "3m")
    }));
    assert!(cfg.tests.iter().any(|test| {
        test.direction.directions() == ["ba"]
            && test
                .udp_profiles
                .as_ref()
                .is_some_and(|profiles| profiles[0].bandwidth == "500m")
    }));

    let console = console_with(state_with_pair());
    let out = api_import(&console, &serde_json::to_string(&cfg).unwrap())
        .expect("混合固定/扫描方向必须能导入");
    assert_eq!(
        out["udp_groups"][0]["bandwidths"].as_array().unwrap(),
        &vec![json!("500m")],
        "未固定的 B→A 方向仍应恢复附加 UDP 组"
    );
    assert_eq!(out["pairs"][0]["udp_groups"], json!([1]));
}

/// 下载 -> 导入 -> 再下载，两份配置**跑出来的单元必须一模一样**。
///
/// 这是导入功能真正要保证的东西，比逐个字段对更硬：任何一处回填走样，
/// 单元列表就变了。而「走样」在实际使用里不报错——它安静地按另一份配置
/// 跑完一整轮。
///
/// 比单元而不是比 config 的字节，是因为同一件事在 config 里可以有两种写法：
/// 界面没填 ping 次数时写的是 `null`（执行时回落到 `ping.count`），
/// 回填之后那一格会是回落出来的 100，两份 JSON 因此不同、跑的却是同一件事。
/// 单元列表是这两种写法的公共下游，也正是「跑什么」的定义。
#[test]
fn download_import_download_runs_the_same_units() {
    let state = state_with_two_pairs();
    let mut req = request();
    req.nic_policies.clear();
    req.udp_bandwidths = vec!["1m".into()];
    req.udp_lengths = vec!["1200".into()];
    req.udp_streams = 2;
    req.udp_groups = vec![UdpGroup {
        name: "单流".into(),
        bandwidths: vec!["2500m".into()],
        lengths: vec!["1200".into()],
        windows: Vec::new(),
        streams: 1,
    }];
    req.pairs[0].udp_groups = vec![1];
    req.pairs[0].rx_target_bidir_ab = "1000".into();
    let mut second = req.pairs[0].clone();
    second.src = "master:NAME=以太网 7".into();
    second.dst = "agent:NAME=USB 4".into();
    req.udp_groups.push(UdpGroup {
        name: "多流".into(),
        bandwidths: vec!["1000m".into(), "500m".into()],
        lengths: vec!["1200".into()],
        windows: Vec::new(),
        streams: 4,
    });
    second.udp_groups = vec![2];
    second.rx_target_bidir_ab = String::new();
    second.transports = vec!["udp".into(), "ping".into()];
    req.pairs.push(second);

    let first = validated_config_from_request(&state, &req).expect("原始配置必须合法");
    let file = serde_json::to_string(&first).unwrap();

    let console = console_with(state_with_two_pairs());
    let out = api_import(&console, &file).expect("必须能导回来");
    let replayed = request_from_import(&out);
    let second_pass = {
        let state = lock_recover(&console.state);
        validated_config_from_request(&state, &replayed).expect("回填出来的必须仍然合法")
    };

    assert_eq!(
        units_debug(&first, &state),
        units_debug(&second_pass, &state),
        "导入一轮之后跑的必须还是同一批单元"
    );
    // 顺带钉住这一轮里真正在意的那几个值，免得两边一起错还对得上。
    let dump = units_debug(&first, &state);
    assert!(dump.contains("2500m"), "第一行的逐对档位");
    assert!(
        dump.contains("1000m") && dump.contains("500m"),
        "第二行的两档"
    );
}

/// TCP 参数组和 UDP 一样要能下载再导回、跑出同一批单元；顺带盖住「附加组把
/// `-w` 留空 = 跑一条不带 `-w` 的 TCP」这条新路径，和「一行选多组 TCP」。
#[test]
fn tcp_groups_download_import_runs_the_same_units() {
    let state = state_with_two_pairs();
    let mut req = request();
    req.nic_policies.clear();
    // 默认 TCP 组：-w 两档 × -P 两档。
    req.tcp_windows = vec!["4m".into(), "256m".into()];
    req.tcp_streams = vec![1, 10];
    // 组1：单独的 -w、单流。组2：-w 留空（不下发 -w）、-P 扫两档——走 builder
    // 的 no-window 分支。
    req.tcp_groups = vec![
        TcpGroup {
            name: "大窗".into(),
            windows: vec!["512m".into()],
            streams: vec![1],
        },
        TcpGroup {
            name: "裸窗".into(),
            windows: Vec::new(),
            streams: vec![1, 4],
        },
    ];
    // 第一行只跑 TCP，选默认组 + 组1；第二行只跑 TCP，选组2（裸窗）。
    req.pairs[0].transports = vec!["tcp".into()];
    req.pairs[0].directions = vec!["ab".into()];
    req.pairs[0].rx_target_bidir_ab = String::new();
    req.pairs[0].udp_groups = Vec::new();
    req.pairs[0].tcp_groups = vec![0, 1];
    let mut second = req.pairs[0].clone();
    second.src = "master:NAME=以太网 7".into();
    second.dst = "agent:NAME=USB 4".into();
    second.tcp_groups = vec![2];
    req.pairs.push(second);

    let first = validated_config_from_request(&state, &req).expect("原始配置必须合法");
    let file = serde_json::to_string(&first).unwrap();

    let console = console_with(state_with_two_pairs());
    let out = api_import(&console, &file).expect("必须能导回来");
    let replayed = request_from_import(&out);
    let second_pass = {
        let state = lock_recover(&console.state);
        validated_config_from_request(&state, &replayed).expect("回填出来的必须仍然合法")
    };

    assert_eq!(
        units_debug(&first, &state),
        units_debug(&second_pass, &state),
        "TCP 组导入一轮之后跑的必须还是同一批单元"
    );
    let dump = units_debug(&first, &state);
    assert!(dump.contains("512m"), "组1 的 -w 档位应出现在单元里");
    // 裸窗组：应有不带 -w 的 TCP 单元（标签是 `TCP -P n` 而不是 `TCP -w .. -P n`）。
    assert!(
        dump.contains("TCP -P 4"),
        "裸窗组应生成一条不带 -w 的 TCP 单元"
    );
}

/// 一份 config 会生成哪些单元。Debug 里带着方向、协议、档位、流数和端口，
/// 「跑什么」的每一个可见维度都在。
fn units_debug(cfg: &Config, state: &UiState) -> String {
    let specs: Vec<_> = cfg
        .tests
        .iter()
        .map(|test| {
            builder::spec_from_config(test, cfg, &state.master, &state.agent)
                .unwrap_or_else(|error| panic!("{} 生成任务失败：{error}", test.name))
        })
        .collect();
    let mut port = builder::PORT_BASE;
    let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
    assert!(!units.is_empty(), "这份配置一个单元都没生成");
    format!("{units:#?}")
}

/// 把 `/api/import` 的回包重新组装成一次「开始测试」的请求，
/// 也就是页面拿到它之后会做的事。
fn request_from_import(out: &serde_json::Value) -> RunRequest {
    let settings = &out["settings"];
    let list = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let numbers = |value: &serde_json::Value| -> Vec<u32> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_u64())
                    .map(|v| v as u32)
                    .collect()
            })
            .unwrap_or_default()
    };
    let pairs = out["pairs"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|pair| PairSelection {
            src: pair["src"].as_str().unwrap_or_default().to_string(),
            dst: pair["dst"].as_str().unwrap_or_default().to_string(),
            directions: list(&pair["directions"]),
            rx_target_bidir_ab: pair["rx_target_bidir_ab"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            rx_target_bidir_ba: pair["rx_target_bidir_ba"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            udp_groups: pair["udp_groups"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_u64())
                        .map(|value| value as usize)
                        .collect()
                })
                .unwrap_or_default(),
            tcp_groups: pair["tcp_groups"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_u64())
                        .map(|value| value as usize)
                        .collect()
                })
                .unwrap_or_default(),
            transports: list(&pair["transports"]),
            ip: list(&pair["ip"]),
        })
        .collect();
    RunRequest {
        pairs,
        nic_policies: serde_json::from_value(out["nic_policies"].clone()).unwrap_or_default(),
        duration: settings["duration"].as_u64().unwrap_or(180),
        tcp_windows: list(&settings["tcp_windows"]),
        tcp_streams: numbers(&settings["tcp_streams"]),
        udp_bandwidths: list(&settings["udp_bandwidths"]),
        udp_lengths: list(&settings["udp_lengths"]),
        udp_windows: list(&settings["udp_windows"]),
        udp_streams: settings["udp_streams"].as_u64().unwrap_or(1) as u32,
        udp_groups: out["udp_groups"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|group| UdpGroup {
                name: group["name"].as_str().unwrap_or_default().to_string(),
                bandwidths: list(&group["bandwidths"]),
                lengths: list(&group["lengths"]),
                windows: list(&group["windows"]),
                streams: group["streams"].as_u64().unwrap_or(1) as u32,
            })
            .collect(),
        tcp_groups: out["tcp_groups"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|group| TcpGroup {
                name: group["name"].as_str().unwrap_or_default().to_string(),
                windows: list(&group["windows"]),
                streams: numbers(&group["streams"]),
            })
            .collect(),
        ping_count: settings["ping_count"].as_u64().unwrap_or(0) as u32,
        ping_payload_sizes: numbers(&settings["ping_payload_sizes"]),
        limit_udp_by_link_speed: out["limit_udp_by_link_speed"].as_bool().unwrap_or(false),
        resume: out["resume"].as_bool().unwrap_or(false),
        screenshot: settings["screenshot"].as_bool().unwrap_or(false),
        ui_plan: None,
        plan_hash: None,
    }
}

/// 一档 `-b` 会因为每个 `-l` 档位各生成一份 profile；回填时不去重的话，
/// 「下载 → 导入」每走一轮档位就翻一倍。
#[test]
fn importing_does_not_multiply_the_udp_bandwidth_steps() {
    let state = state_with_pair();
    let mut req = request();
    req.udp_lengths = vec!["1200".into(), "1400".into()];
    let cfg = config_from_request(&state, &req);
    assert_eq!(cfg.iperf.udp_profiles.len(), 6, "3 档 -b × 2 档 -l");

    let console = console_with(state_with_pair());
    let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        out["settings"]["udp_bandwidths"].as_array().unwrap(),
        &vec![json!("1m"), json!("500m"), json!("1G")]
    );
    assert_eq!(
        out["settings"]["udp_lengths"].as_array().unwrap(),
        &vec![json!("1200"), json!("1400")]
    );
}

/// ping 的次数和包长只落在 tests[] 上，回填要从那里读。
///
/// 只读 cfg.ping 的话，一份「50 次 × 64 字节」的配置会回填成默认的
/// 「100 次 × 三档包长」：单元数变三倍，而框里的数字看着像是文件里的。
#[test]
fn importing_reads_the_ping_settings_off_the_tests() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].transports = vec!["ping".into()];
    req.ping_count = 50;
    req.ping_payload_sizes = vec![64];
    let cfg = config_from_request(&state, &req);

    let console = console_with(state_with_pair());
    let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(out["settings"]["ping_count"], 50);
    assert_eq!(
        out["settings"]["ping_payload_sizes"].as_array().unwrap(),
        &vec![json!(64)]
    );
    assert_eq!(
        out["pairs"][0]["transports"].as_array().unwrap(),
        &vec![json!("ping")],
        "ping 在配置里挂 kinds、在界面上挂协议列，回填要走相反那一步"
    );
}

/// 文件把一对网口写反了（`src`/`dst` 调过来），要合进同一行并把方向对调。
///
/// 矩阵一行代表的是**一对**网口。同一对口在文件里正着写一条、反着写一条是
/// 完全合法的；不合并的话它会占两行，而界面只画得出一行——另一行的勾选
/// 就此消失，人看不出少了什么。
#[test]
fn importing_folds_a_reversed_pair_into_one_row() {
    let state = state_with_pair();
    let mut cfg = config_from_request(&state, &request());
    // 只把 UDP 那条掉个头，TCP 三条保持原样：合并要发生在两种写法之间。
    let udp = cfg
        .tests
        .iter_mut()
        .find(|test| test.transports.iter().any(|t| t == "udp"))
        .expect("UDP 那条");
    std::mem::swap(&mut udp.src, &mut udp.dst);
    udp.direction = OneOrMany::Many(vec!["A->B".into()]);
    udp.rate_targets_bidir_mbps = Some(crate::config::RateTargets {
        forward: None,
        ab: Some(900.0),
        ba: None,
    });

    let console = console_with(state_with_pair());
    let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        out["pairs"].as_array().unwrap().len(),
        1,
        "同一对口只占一行"
    );
    let pair = &out["pairs"][0];
    assert_eq!(
        pair["src"], "master:NAME=以太网 6",
        "行的朝向按先出现的那条"
    );
    assert_eq!(
        pair["directions"].as_array().unwrap(),
        &vec![json!("ab"), json!("bidir"), json!("ba")],
        "反着写的那条里的 A→B，在这一行是 B→A"
    );
    assert_eq!(pair["rx_target_bidir_ba"], "900", "双向门限跟着方向一起翻");
    assert_eq!(pair["rx_target_bidir_ab"], "");
}

/// 还没连上辅测机也要能导入：全局参数当场生效，配对留给页面在连上之后按
/// 端点名匹配。按角色写的端点（`master:SGMII2.5G`）这时解析不了，得点名。
#[test]
fn importing_before_connecting_keeps_the_named_pairs() {
    let mut cfg = config_from_request(&state_with_pair(), &request());
    cfg.tests.push(TestSpec {
        name: "by-role".into(),
        src: "master:SGMII2.5G".into(),
        dst: "agent:WIFI5G".into(),
        ..cfg.tests[0].clone()
    });

    let console = console_with(UiState {
        cfg: Config::default(),
        agent_host: String::new(),
        master: HostInfo::default(),
        agent: HostInfo::default(),
    });
    let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        out["pairs"].as_array().unwrap().len(),
        1,
        "NAME= 写法不需要实扫就能认"
    );
    let notices = out["notices"].as_array().unwrap();
    assert!(
        notices
            .iter()
            .any(|n| n.as_str().unwrap().contains("SGMII2.5G")),
        "认不出来的端点必须点名，不能默默少一行：{notices:?}"
    );
}

/// 文件里没有 agent_token 时不能把已经加载的令牌冲掉。
///
/// 手写的 config 多半不带令牌；用空串覆盖的表现是导入之后点「连接」突然
/// 401，而人刚做的事看起来和连接毫无关系。
#[test]
fn importing_a_file_without_a_token_keeps_the_loaded_one() {
    let mut state = state_with_pair();
    state.cfg.agent_token = "loaded-secret".into();
    let console = console_with(state);

    let mut cfg = config_from_request(&state_with_pair(), &request());
    // 显式清空：Config::default() 现在带着出厂默认口令，不清掉的话这里测的
    // 就变成「文件里有令牌」，跟本用例要守的「文件里没有令牌」正好相反。
    cfg.agent_token = String::new();
    let out = api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(out["settings"]["token_configured"], true);
    assert_eq!(
        lock_recover(&console.state).cfg.agent_token,
        "loaded-secret"
    );
    assert!(
        out["notices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("agent_token")),
        "沿用旧令牌要说一声"
    );

    // 文件里带着令牌时以文件为准：那才是这份配置连得上的那台。
    cfg.agent_token = "from-file".into();
    api_import(&console, &serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(lock_recover(&console.state).cfg.agent_token, "from-file");
}

/// 导入的是**配置**，不是「一份差不多的 JSON」。看不懂要当场说清。
#[test]
fn importing_rubbish_says_so_instead_of_half_applying_it() {
    let console = console_with(state_with_pair());
    let error = api_import(&console, "{ 这不是 json }").expect_err("必须报错");
    assert!(error.contains("config.json"), "{error}");

    let mut cfg = config_from_request(&state_with_pair(), &request());
    cfg.iperf.duration = 0;
    let error = api_import(&console, &serde_json::to_string(&cfg).unwrap())
        .expect_err("过不了 validate 的配置不能导进来");
    assert!(error.contains("duration"), "{error}");
    assert_eq!(
        lock_recover(&console.state).cfg.iperf.duration,
        Config::default().iperf.duration,
        "被拒的导入不能改动任何现有状态"
    );
}

/// 监听地址不是访问地址：`0.0.0.0` 弹给浏览器打不开。
#[test]
fn the_printed_address_is_one_a_browser_can_actually_open() {
    assert_eq!(display_addr("0.0.0.0", 28800), "127.0.0.1:28800");
    assert_eq!(display_addr("::", 28800), "[::1]:28800");
    assert_eq!(display_addr("[::]", 28800), "[::1]:28800");
    assert_eq!(display_addr(" 0.0.0.0 ", 28800), "127.0.0.1:28800");
    // 绑到具体地址时那个地址本来就是该用的访问地址，不能改写。
    assert_eq!(display_addr("127.0.0.1", 28800), "127.0.0.1:28800");
    assert_eq!(display_addr("192.168.8.101", 28800), "192.168.8.101:28800");
    assert_eq!(display_addr("::1", 28800), "[::1]:28800");
    assert!(bind_is_wildcard("0.0.0.0") && bind_is_wildcard("::"));
    assert!(!bind_is_wildcard("127.0.0.1") && !bind_is_wildcard("192.168.8.101"));
}

/// PING 必须能过 `validate_request`——单独勾、和 TCP/UDP 一起勾都算。
///
/// 白名单曾经只写了 tcp/udp，而 `values_are_allowed` 要求**每一项**都在集合里：
/// 勾上 PING 不是「PING 跑不了」，是整份请求作废，连同一配对里本来能跑的
/// TCP/UDP 一起废掉，页面上还提示人去勾 TCP 或 UDP。
#[test]
fn checking_ping_passes_validation_alone_and_alongside_tcp_udp() {
    let state = state_with_pair();
    for transports in [
        vec!["ping".to_string()],
        vec!["tcp".to_string(), "udp".to_string(), "ping".to_string()],
    ] {
        let mut req = request();
        req.pairs[0].transports = transports.clone();
        if let Err(error) = validate_request(&state, &req) {
            panic!("{transports:?} 必须通过校验，却被拒：{error}");
        }
    }

    let mut bogus = request();
    bogus.pairs[0].transports = vec!["icmp".into()];
    assert!(
        validate_request(&state, &bogus).is_err(),
        "白名单之外的写法仍要挡住"
    );
}

/// 包长档位要保序去重。`dedup()` 只合并相邻项：「32 1600 32」会漏过去，
/// 两个 32 的单元标题和 resume id 一模一样，在结果库里互相覆盖，
/// 还白跑一遍全程。
#[test]
fn repeated_ping_payload_sizes_collapse_even_when_not_adjacent() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].transports = vec!["ping".into()];
    req.ping_payload_sizes = vec![1600, 32, 1600, 0, 32];

    let cfg = config_from_request(&state, &req);
    let ping = cfg
        .tests
        .iter()
        .find(|t| t.kinds.iter().any(|k| k == "ping"))
        .expect("应有 ping 测试项");
    assert_eq!(
        ping.ping_payload_sizes.as_deref(),
        Some(&[1600u32, 32][..]),
        "重复档位只留一份，且保持用户填的顺序"
    );
}

/// 越界包长要当场拒绝，不能留给 `ping::build` 悄悄夹紧。
///
/// 夹紧发生在分单元之后：65500 和 100000 会变成两个 resume id 不同、
/// 跑起来完全一样的单元，报告上却各自写着自己那个 `-l`。
#[test]
fn an_oversized_ping_budget_is_rejected_before_starting_a_run() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].transports = vec!["ping".into()];

    req.ping_payload_sizes = vec![32, crate::ping::MAX_PAYLOAD + 1];
    let error = validate_request(&state, &req).expect_err("越界包长必须被拒");
    assert!(error.contains("65500"), "错误里要写清上限：{error}");

    req.ping_payload_sizes = vec![crate::ping::MAX_PAYLOAD];
    assert!(validate_request(&state, &req).is_ok(), "正好在上限上要放行");

    req.ping_count = 100_001;
    assert!(
        validate_request(&state, &req).is_err(),
        "次数同样会被 builder 静默夹紧，也要在这里挡住"
    );
}

/// 裸 IPv6 要补方括号才拼得出监听地址。
///
/// `bind_is_loopback` 是认 `"::1"` 的，不补的话「判定放行 → 监听失败」
/// 这条路走得通，人只会看到一句莫名其妙的启动错误。
#[test]
fn ipv6_binds_get_bracketed_before_they_reach_the_listener() {
    assert_eq!(listen_addr("127.0.0.1", 28800), "127.0.0.1:28800");
    assert_eq!(listen_addr("0.0.0.0", 28800), "0.0.0.0:28800");
    assert_eq!(listen_addr("::1", 28800), "[::1]:28800");
    assert_eq!(
        listen_addr("[::1]", 28800),
        "[::1]:28800",
        "已经带括号的不重复加"
    );
    assert_eq!(listen_addr("::", 28800), "[::]:28800");
    assert_eq!(listen_addr(" ::1 ", 28800), "[::1]:28800");
    for bind in ["127.0.0.1", "0.0.0.0", "::1", "[::1]", "::"] {
        let addr = listen_addr(bind, 28800);
        addr.parse::<std::net::SocketAddr>()
            .unwrap_or_else(|error| panic!("{bind} 拼出的 {addr} 必须能解析：{error}"));
    }
}

/// 定长时间比较不能顺手把「相等」判错。
#[test]
fn the_constant_time_compare_still_agrees_with_equality() {
    assert!(secret_eq("s3cret", "s3cret"));
    assert!(!secret_eq("s3cret", "s3creT"));
    assert!(!secret_eq("s3cret", "s3cre"), "短一截也不算对");
    assert!(!secret_eq("s3cret", ""));
    assert!(secret_eq("", ""));
    assert!(secret_eq("口令", "口令"), "多字节口令按字节比也要相等");
}

/// 关掉浏览器标签页不会通知服务端，所以「显式 stop」不能是会话表唯一的出口。
///
/// 采样线程自己会收摊，但它只结束线程；会话连同那个 7200 点的缓冲会一直
/// 留在表里，刷新一次页面就多一条。
#[test]
fn monitor_sessions_whose_page_went_away_get_reaped() {
    let stale = std::time::Instant::now() - (MONITOR_IDLE_TIMEOUT + Duration::from_secs(1));
    let now = std::time::Instant::now();
    let session = |running: bool, last_poll: Option<std::time::Instant>, started| MonitorSession {
        side: "master".into(),
        iface: "eth0".into(),
        stop: Arc::new(AtomicBool::new(false)),
        data: Arc::new(Mutex::new(MonitorData {
            running,
            last_poll,
            ..Default::default()
        })),
        started,
    };
    let mut monitors: HashMap<String, MonitorSession> = HashMap::new();
    // 线程还在跑：哪怕页面很久没来取，也由采样线程自己决定什么时候停。
    monitors.insert("live".into(), session(true, Some(stale), stale));
    // 线程刚停，页面还在轮询：留着让它把「已停止」读走并正常收尾。
    monitors.insert("just-stopped".into(), session(false, Some(now), now));
    // 线程停了、页面也早就不来了：这条只剩内存占用。
    monitors.insert("abandoned".into(), session(false, Some(stale), stale));
    // 一次都没被取过样本，且开出来已经很久：页面开完就被关掉了。
    monitors.insert("never-polled".into(), session(false, None, stale));
    // 刚刚开出来还没轮到第一次轮询：不能误伤。
    monitors.insert("starting".into(), session(false, None, now));

    reap_dead_monitors(&mut monitors);

    let mut left: Vec<&str> = monitors.keys().map(|k| k.as_str()).collect();
    left.sort_unstable();
    assert_eq!(left, ["just-stopped", "live", "starting"]);
}

/// 会话数有上限，而且要在**起线程之前**判。
///
/// 控制台一旦 `--ui-bind` 出去，一个拿到口令的客户端循环调
/// /api/monitor/start 就能一路撑起线程和辅测机侧的 monitor 资源。
#[test]
fn the_console_refuses_to_pile_up_monitor_sessions() {
    let console = console_for_monitor_tests();
    {
        let mut monitors = lock_recover(&console.monitors);
        for idx in 0..MONITOR_MAX_SESSIONS {
            monitors.insert(
                format!("s{idx}"),
                MonitorSession {
                    side: "master".into(),
                    iface: "eth0".into(),
                    stop: Arc::new(AtomicBool::new(false)),
                    data: Arc::new(Mutex::new(MonitorData {
                        running: true,
                        ..Default::default()
                    })),
                    started: std::time::Instant::now(),
                },
            );
        }
    }
    let error = api_monitor_start(&console, r#"{"side":"master","iface":"eth0"}"#)
        .expect_err("撞上限必须直接拒绝，而不是再起一条线程");
    assert!(error.contains("最多"), "{error}");
    assert_eq!(
        lock_recover(&console.monitors).len(),
        MONITOR_MAX_SESSIONS,
        "被拒的那次不能留下任何痕迹"
    );
}

/// 临时 config 里带着 agent_token，而 /tmp 是全局可读的。
#[cfg(unix)]
#[test]
fn the_temp_run_config_is_not_world_readable() {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!(
        "cpe_ui_private_{}_{}.json",
        std::process::id(),
        now_millis()
    ));
    // 先摆一个 0644 的残file：`mode()` 只在创建时生效，沿用旧权限就等于没修。
    std::fs::write(&path, "{}").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    write_private_config(&path, r#"{"agent_token":"s3cret"}"#).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    let body = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(mode, 0o600, "同机的别人不能读到 agent_token");
    assert_eq!(
        body, r#"{"agent_token":"s3cret"}"#,
        "权限之外内容要原样写进去"
    );
}

/// resume 的跳过只是预判，页面要同时拿得到「全跳过」和「全实跑」两个数。
#[test]
fn the_plan_reports_both_ends_of_the_resume_estimate() {
    let console = console_for_monitor_tests();
    let body = serde_json::json!({
        "pairs": [{
            "src": "master:NAME=以太网 6",
            "dst": "agent:NAME=WLAN 3",
            "directions": ["ab"],
            "transports": ["tcp"],
            "ip": ["v4"],
        }],
        "duration": 60,
        "tcp_windows": ["2m"],
        "tcp_streams": [1],
        "udp_streams": 1,
        "resume": false,
    })
    .to_string();

    let out = api_plan(&console, &body).expect("计划必须生成");
    let total = out["est_total_secs"].as_u64().expect("est_total_secs");
    let full = out["est_full_secs"].as_u64().expect("est_full_secs");
    assert!(total > 0, "至少要有一个单元");
    assert_eq!(total, full, "没开 resume 时两个数必须一致");
}

/// resume 和裁剪开关同理：界面上的勾选是唯一来源，配置文件里的值不参与。
///
/// 这一条以前是控制台唯一没暴露、却又会悄悄生效的配置项——config.json 里
/// 写了 `resume: true`，界面上既看不见也关不掉。
#[test]
fn the_console_decides_resume_regardless_of_the_config_file() {
    let mut state = state_with_pair();
    state.cfg.resume = true;

    let req = request();
    assert!(
        !config_from_request(&state, &req).resume,
        "界面没勾就不跳过，配置文件里的 true 不能悄悄生效"
    );

    let mut on = request();
    on.resume = true;
    assert!(config_from_request(&state, &on).resume);
}

/// -l 必须能塞进一个 UDP 报文。
#[test]
fn an_impossible_datagram_size_is_rejected_before_starting_a_run() {
    let state = state_with_pair();
    let mut req = request();
    req.udp_lengths = vec!["70000".into()];
    let error = validated_config_from_request(&state, &req).unwrap_err();
    assert!(error.contains("65507"), "{error}");

    req.udp_lengths = vec!["1400x".into()];
    let error = validated_config_from_request(&state, &req).unwrap_err();
    assert!(error.contains("UDP -l"), "{error}");
}

/// `-b` × `-l` × `-w` 三维取组合，每一项留空就在那一维退化成「不下发」。
#[test]
fn udp_socket_buffer_steps_join_the_same_cross_product() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].transports = vec!["udp".into()];
    req.pairs[0].directions = vec!["ab".into()];
    req.nic_policies
        .iter_mut()
        .for_each(|p| p.udp_bandwidth.clear());
    req.udp_bandwidths = vec!["500m".into()];
    req.udp_lengths = vec!["64".into(), "1400".into()];
    req.udp_windows = vec!["2m".into(), "8m".into()];

    let cfg = config_from_request(&state, &req);
    let udp = cfg
        .tests
        .iter()
        .find(|t| t.transports.contains(&"udp".to_string()))
        .expect("应有 UDP spec");
    let mut labels: Vec<String> = udp
        .udp_profiles
        .as_ref()
        .expect("应有档位")
        .iter()
        .map(|p| p.label())
        .collect();
    labels.sort();
    assert_eq!(
        labels,
        vec![
            "UDP -b 500m -l 1400 -w 2m",
            "UDP -b 500m -l 1400 -w 8m",
            "UDP -b 500m -l 64 -w 2m",
            "UDP -b 500m -l 64 -w 8m",
        ],
        "1 档 -b × 2 档 -l × 2 档 -w = 4 档"
    );

    // UDP 的 -w 不能顺手改写 TCP 的 -w：两者是两个独立输入。
    assert_eq!(cfg.iperf.tcp_windows, vec!["2m", "4m", "256m"]);

    // 一路建到真实命令，确认 -w 跟着下发。
    let specs: Vec<_> = cfg
        .tests
        .iter()
        .map(|t| builder::spec_from_config(t, &cfg, &state.master, &state.agent).expect("建 spec"))
        .collect();
    let mut port = builder::PORT_BASE;
    let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
    let mut seen_windows: Vec<String> = Vec::new();
    for unit in &units {
        for leg in &unit.legs {
            let tasks: Vec<&builder::IperfTask> = match &leg.kind {
                builder::LegKind::IperfSingle(task) => vec![task],
                builder::LegKind::IperfGroup { streams, .. } => streams.iter().collect(),
                _ => Vec::new(),
            };
            for task in tasks {
                let at = task
                    .extra
                    .iter()
                    .position(|arg| arg == "-w")
                    .unwrap_or_else(|| panic!("每条 UDP 命令都要带 -w: {:?}", task.extra));
                seen_windows.push(task.extra[at + 1].clone());
            }
        }
    }
    seen_windows.sort();
    seen_windows.dedup();
    assert_eq!(seen_windows, vec!["2m", "8m"]);
}

/// 三项都留空时，UDP 命令上一个 `-l` / `-w` 都不该出现。
#[test]
fn blank_udp_extras_add_no_flags_to_the_command() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].transports = vec!["udp".into()];
    req.pairs[0].directions = vec!["ab".into()];
    req.nic_policies
        .iter_mut()
        .for_each(|p| p.udp_bandwidth.clear());
    req.udp_bandwidths = vec!["500m".into()];
    req.udp_lengths = Vec::new();
    req.udp_windows = Vec::new();

    let cfg = config_from_request(&state, &req);
    let profiles = cfg
        .tests
        .iter()
        .find(|t| t.transports.contains(&"udp".to_string()))
        .and_then(|t| t.udp_profiles.clone())
        .expect("应有档位");
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].label(), "UDP -b 500m");
    assert!(profiles[0].length.is_none() && profiles[0].window.is_none());
}

/// 配置文件里重复出现的 `-l` / `-w` 回填到界面时要压成一份，
/// 否则打开页面档位就自己翻倍。
#[test]
fn repeated_profile_extras_collapse_when_filling_the_form() {
    assert_eq!(
        distinct(["2m", "8m", "2m", "8m"].iter().map(|v| v.to_string())),
        vec!["2m", "8m"]
    );
}

/// UDP 的 -w 和 TCP 一样按尺寸解析，写错要在开跑前拦下。
#[test]
fn an_invalid_udp_socket_buffer_is_rejected_before_starting_a_run() {
    let state = state_with_pair();
    let mut req = request();
    req.udp_windows = vec!["8毫米".into()];
    let error = validated_config_from_request(&state, &req).unwrap_err();
    assert!(error.contains("UDP -w"), "{error}");
}

/// 门限输入框要同时收下绝对值和百分比两种写法。
#[test]
fn the_threshold_field_takes_both_mbps_and_percent() {
    assert_eq!(parse_rx_target("1800"), Ok(Some(RxTarget::Mbps(1800.0))));
    assert_eq!(
        parse_rx_target(" 1800.5 "),
        Ok(Some(RxTarget::Mbps(1800.5)))
    );
    assert_eq!(parse_rx_target("90%"), Ok(Some(RxTarget::Percent(90.0))));
    assert_eq!(parse_rx_target("90 %"), Ok(Some(RxTarget::Percent(90.0))));
    assert_eq!(parse_rx_target(""), Ok(None));
    assert_eq!(parse_rx_target("   "), Ok(None));

    assert!(parse_rx_target("0").is_err(), "0 不是门限");
    assert!(parse_rx_target("-5").is_err());
    assert!(parse_rx_target("很快").is_err());
    assert!(
        parse_rx_target("900%").is_err(),
        "三位数百分比几乎一定是把 Mbps 写成了百分号"
    );
}

/// 百分比要落到 by_nic.rx_target_percent，绝对值落到 rx_target_mbps，
/// 两者不能互相串。
#[test]
fn percent_and_absolute_thresholds_land_in_different_fields() {
    let mut req = request();
    req.nic_policies[0].rx_target = "90%".into();
    req.nic_policies[1].rx_target = "1600".into();
    let cfg = config_from_request(&state_with_pair(), &req);

    let by_percent = cfg
        .link_profiles
        .by_nic
        .iter()
        .find(|p| p.name == "以太网 6")
        .expect("主控网卡");
    assert_eq!(by_percent.rx_target_percent, Some(90.0));
    assert_eq!(by_percent.rx_target_mbps, None);

    let absolute = cfg
        .link_profiles
        .by_nic
        .iter()
        .find(|p| p.name == "WLAN 3")
        .expect("辅测网卡");
    assert_eq!(absolute.rx_target_mbps, Some(1600.0));
    assert_eq!(absolute.rx_target_percent, None);
}

/// 按网口填的 `-l` 要覆盖全局档位，且只作用于这块网卡作发送端的那条腿。
#[test]
fn a_per_nic_datagram_size_overrides_the_global_step() {
    let state = state_with_pair();
    let mut req = request();
    req.pairs[0].transports = vec!["udp".into()];
    req.pairs[0].directions = vec!["ab".into(), "ba".into()];
    req.udp_bandwidths = vec!["100m".into()];
    req.udp_lengths = vec!["1400".into()];
    // 只有主控口指定 -l 64；辅测口留空，走全局的 1400。
    req.nic_policies[0].udp_length = "64".into();
    req.nic_policies[1].udp_length.clear();

    let cfg = config_from_request(&state, &req);
    let specs: Vec<_> = cfg
        .tests
        .iter()
        .map(|t| builder::spec_from_config(t, &cfg, &state.master, &state.agent).expect("建 spec"))
        .collect();
    let mut port = builder::PORT_BASE;
    let (units, _) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);

    let mut by_sender: Vec<(String, String)> = Vec::new();
    for unit in &units {
        for leg in &unit.legs {
            let tasks: Vec<&builder::IperfTask> = match &leg.kind {
                builder::LegKind::IperfSingle(task) => vec![task],
                builder::LegKind::IperfGroup { streams, .. } => streams.iter().collect(),
                _ => Vec::new(),
            };
            for task in tasks {
                let at = task
                    .extra
                    .iter()
                    .position(|arg| arg == "-l")
                    .unwrap_or_else(|| panic!("应带 -l: {:?}", task.extra));
                by_sender.push((task.src.nic.name.clone(), task.extra[at + 1].clone()));
            }
        }
    }
    by_sender.sort();
    by_sender.dedup();
    assert_eq!(
        by_sender,
        vec![
            ("WLAN 3".to_string(), "1400".to_string()),
            ("以太网 6".to_string(), "64".to_string()),
        ],
        "发送口填了 -l 就用它的，没填的那条腿仍走全局档位"
    );

    // 标签必须跟着实际下发值走，不然报表里印的 -l 和命令行对不上。
    assert!(
        units.iter().any(|u| u.title.contains("-l 64")),
        "{:?}",
        units.iter().map(|u| &u.title).collect::<Vec<_>>()
    );
}

/// 只填 `-l`、不填 `-b` 的网口不算「带宽被钉死」，仍要扫全局 -b 档位。
#[test]
fn a_per_nic_datagram_size_alone_does_not_pin_the_bandwidth() {
    let mut req = request();
    req.pairs[0].transports = vec!["udp".into()];
    req.pairs[0].directions = vec!["ab".into()];
    req.nic_policies
        .iter_mut()
        .for_each(|p| p.udp_bandwidth.clear());
    req.nic_policies[0].udp_length = "64".into();
    req.udp_bandwidths = vec!["1m".into(), "500m".into(), "1G".into()];

    let cfg = config_from_request(&state_with_pair(), &req);
    let udp = cfg
        .tests
        .iter()
        .find(|t| t.transports.contains(&"udp".to_string()))
        .expect("应有 UDP spec");
    assert_eq!(
        udp.udp_profiles.as_ref().map(Vec::len),
        Some(3),
        "-b 没被覆盖，三个档位都要跑"
    );
}

/// 三项全空才不生成覆盖项；只填 `-l` 也要生成。
#[test]
fn a_lone_datagram_size_still_produces_an_override() {
    let mut req = request();
    for policy in &mut req.nic_policies {
        policy.rx_target.clear();
        policy.udp_bandwidth.clear();
        policy.udp_length.clear();
    }
    req.nic_policies[0].udp_length = "64".into();
    let cfg = config_from_request(&state_with_pair(), &req);
    assert_eq!(cfg.link_profiles.by_nic.len(), 1);
    assert_eq!(
        cfg.link_profiles.by_nic[0].udp_length.as_deref(),
        Some("64")
    );
}

/// 按网口的 -l 同样不能超过一个 UDP 报文装得下的大小。
#[test]
fn a_per_nic_datagram_size_is_bounded_too() {
    let state = state_with_pair();
    let mut req = request();
    req.nic_policies[0].udp_length = "70000".into();
    let error = validated_config_from_request(&state, &req).unwrap_err();
    assert!(error.contains("65507"), "{error}");
}

/// **计划闸门的前提**：预览路径和执行路径必须推导出同一批单元。
///
/// 预览为了把每个单元追溯回它的套件任务，是逐 spec 单独 `build_units` 的；
/// 执行端是把所有 spec 一次性交进去。两者本该等价（端口计数器是共享的），
/// 但这份等价从来没有人守过——一旦哪天有了跨 spec 的去重或排序，预览页
/// 就会开始展示和实际执行不同的东西，而且完全没有痕迹。
///
/// 计划哈希就建立在这份等价上：算哈希用的是执行端的推导方式，展示用的是
/// 逐 spec 的结果。这条断言若红，说明复核页在撒谎，必须先修那个分叉，
/// 而不是把哈希改成两边各算各的。
#[test]
fn the_preview_and_execution_paths_build_the_same_units() {
    let state = state_with_pair();
    let req = suite_request();
    let compiled = compile_request(&state, &req).expect("compile suite plan");
    let canonical = canonical_plan_units(&compiled.cfg, &state);

    assert_eq!(
        canonical.len(),
        compiled.units.len(),
        "两条路径的单元个数不一致：执行端 {} / 预览 {}",
        canonical.len(),
        compiled.units.len()
    );
    for (preview, execution) in compiled.units.iter().zip(canonical.iter()) {
        assert_eq!(
            format!("{preview:?}"),
            format!("{execution:?}"),
            "同一个位置上的单元不一致"
        );
    }
}

/// 计划哈希必须钉住**执行内容**，不是请求报文。
#[test]
fn the_plan_hash_tracks_what_will_actually_run() {
    let state = state_with_pair();
    let req = suite_request();
    let a = compile_request(&state, &req).expect("compile");
    let b = compile_request(&state, &req).expect("compile again");
    assert_eq!(a.plan_hash, b.plan_hash, "同样的输入必须得到同样的哈希");

    // 换一份会改变执行内容的请求，哈希必须跟着变。
    let mut other = req.clone();
    if let Some(plan) = other.ui_plan.as_mut() {
        for suite in &mut plan.suites {
            for task in &mut suite.tasks {
                task.duration = Some(task.duration.unwrap_or(180) + 7);
            }
        }
    }
    let c = compile_request(&state, &other).expect("compile changed");
    assert_ne!(
        a.plan_hash, c.plan_hash,
        "执行内容变了，哈希却没变——闸门就形同虚设"
    );
}

/// 拓扑变了，之前确认过的计划就该失效。
#[test]
fn the_plan_hash_changes_when_the_topology_changes() {
    let req = suite_request();
    let before = compile_request(&state_with_pair(), &req).expect("compile");

    let mut moved = state_with_pair();
    moved.agent.interfaces[0].ipv4 = "192.168.0.200".into();
    let after = compile_request(&moved, &req).expect("compile after move");
    assert_ne!(
        before.plan_hash, after.plan_hash,
        "对端 IP 变了，计划必须重新确认"
    );
}

/// 基线套件的出厂 UDP 档位（`-b 2500m` · `-l 14k` · `-w 256m` · 单流）必须真的能过
/// 服务端校验并编译成一条 UDP 单元。
///
/// 这条测试原本还有前半段：从 `include_str!` 进来的手写页面里 grep 掉网口表的
/// 「作为发送端：UDP -l」列、`cell('udp_length'` 与 `udpDefault.lengths`，用来钉住
/// 「`-l` 只有一个来源 = 套件里的参数配置，全局档位不许反向覆写」。产物换成 Vue
/// bundle 之后，grep 手写 HTML 这种做法不再成立，那半段的义务按
/// `.ai/PLAN-v5.0-frontend.md` §7.3 转给 Vitest `domain/plan-build.test.ts`：
/// **由 `UiPlan` 组装出的 `RunRequest` 里，全局 `udp_lengths` 不会被套件里的 `-l`
/// 反向写回**。留在这里的后半段是纯 DTO 逻辑，与前端形态无关，原样保留。
#[test]
fn the_baseline_udp_profile_compiles_through_the_real_validator() {
    // 出厂套件里写死的那三个档位，必须真的能过服务端校验——否则默认套件一打开
    // 就是红的。这种错只有把同一组值送进真实校验路径才会露出来（`-l` 超 65507、
    // `-w` 写法不认，都是在这一步才拦下）。
    let state = state_with_pair();
    let mut req = suite_request();
    let plan = req.ui_plan.as_mut().expect("suite plan");
    plan.recipes.udp[0].profiles.clear();
    plan.recipes.udp[0].bandwidths = vec!["2500m".into()];
    plan.recipes.udp[0].lengths = vec!["14k".into()];
    plan.recipes.udp[0].windows = vec!["256m".into()];
    plan.recipes.udp[0].udp_streams = vec![1];
    let cfg =
        validated_config_from_request(&state, &req).expect("基线 TCP+UDP 的出厂档位必须能过校验");
    let udp = cfg
        .tests
        .iter()
        .find(|test| test.transports.iter().any(|t| t == "udp"))
        .expect("应当编译出一条 UDP test");
    let profiles = udp.udp_profiles.as_ref().expect("UDP 档位");
    assert_eq!(profiles.len(), 1, "三条轴各一个值，只该展开成一档");
    assert_eq!(profiles[0].bandwidth, "2500m");
    assert_eq!(profiles[0].length.as_deref(), Some("14k"));
    assert_eq!(profiles[0].window.as_deref(), Some("256m"));
}

/// 随包发布的测试项目（`dist/projects/cpe-ui-project-full.json`）必须在它**自己
/// 声明的那套拓扑**上真的能编译成执行计划。
///
/// 这份文件是手写生成的，不是从界面导出来的——端点名拼错一个字、任务里给 PING
/// 挂了 `recipe_ids`、绑定指向不存在的套件，任何一处都要等用户在现场导入、点
/// 预览才会红。这里把 v4.4 那次实测的 5 块网口摆出来，让后端校验器自己说话。
#[test]
fn the_shipped_full_project_compiles_against_the_topology_it_declares() {
    let project: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("dist/projects/cpe-ui-project-full.json"),
        )
        .expect("全量测试项目必须存在"),
    )
    .expect("测试项目必须是合法 JSON");

    let plan: UiPlan =
        serde_json::from_value(project["ui_plan"].clone()).expect("ui_plan 必须能被后端 DTO 接受");
    assert_eq!(plan.link_sets.len(), 1);
    assert_eq!(
        plan.link_sets[0].pair_refs.len(),
        10,
        "5 块网口两两组合 = 10 对"
    );

    // run_20260828_162822_17788 里的那 5 个端点。
    let nic = |name: &str, speed: u64| NicInfo {
        name: name.into(),
        role: String::new(),
        ipv4: String::new(),
        speed_mbps: speed,
        ..Default::default()
    };
    let mut state = state_with_pair();
    state.master.interfaces = vec![nic("以太网 5", 3750), nic("WLAN", 2402)];
    state.agent.interfaces = vec![
        nic("以太网 3", 2500),
        nic("以太网", 1000),
        nic("WLAN 3", 2882),
    ];
    for (index, iface) in state.master.interfaces.iter_mut().enumerate() {
        iface.ipv4 = format!("192.168.0.{}", 100 + index * 4);
    }
    for (index, iface) in state.agent.interfaces.iter_mut().enumerate() {
        iface.ipv4 = format!("192.168.0.{}", 101 + index);
    }

    let settings = &project["settings"];
    let mut req = suite_request();
    req.ui_plan = Some(plan);
    req.pairs.clear();
    req.nic_policies.clear();
    req.duration = settings["duration"].as_u64().unwrap();
    req.ping_count = settings["ping_count"].as_u64().unwrap() as u32;
    req.ping_payload_sizes = settings["ping_payload_sizes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();

    let cfg = validated_config_from_request(&state, &req)
        .expect("全量测试项目必须能在它声明的拓扑上编译成计划");

    // TCP / UDP / PING 三种任务各自独立成 spec，一条都不能被吞掉。
    let count = |kind: &str, transport: &str| {
        cfg.tests
            .iter()
            .filter(|t| {
                t.kinds.iter().any(|k| k == kind)
                    && (transport.is_empty() || t.transports.iter().any(|x| x == transport))
            })
            .count()
    };
    assert!(count("iperf", "tcp") > 0, "少了 TCP");
    assert!(count("iperf", "udp") > 0, "少了 UDP");
    assert!(count("ping", "") > 0, "少了 PING");

    // 参数照搬 v4.4：TCP -w 64k/4m，UDP -b 1000m/2500m -l 14k -w 256m。
    // 编译器把每一档 `-w` 展开成独立的 spec（一档一个单元），所以这里比的是
    // 所有 TCP spec 合起来覆盖的档位集合，而不是某一条 spec 里有几档。
    let tcp_windows: HashSet<String> = cfg
        .tests
        .iter()
        .filter(|t| t.transports.iter().any(|x| x == "tcp"))
        .flat_map(|t| t.tcp_windows.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        tcp_windows,
        HashSet::from(["64k".to_string(), "4m".to_string()]),
        "TCP 档位应当覆盖 -w 64k 和 -w 4m"
    );
    let udp_profiles: Vec<_> = cfg
        .tests
        .iter()
        .filter(|t| t.transports.iter().any(|x| x == "udp"))
        .flat_map(|t| t.udp_profiles.clone().unwrap_or_default())
        .collect();
    assert!(!udp_profiles.is_empty(), "UDP 档位");
    let bandwidths: HashSet<String> = udp_profiles.iter().map(|p| p.bandwidth.clone()).collect();
    assert_eq!(
        bandwidths,
        HashSet::from(["1000m".to_string(), "2500m".to_string()]),
        "UDP 档位应当覆盖 -b 1000m 和 -b 2500m"
    );
    for profile in &udp_profiles {
        assert_eq!(profile.length.as_deref(), Some("14k"));
        assert_eq!(profile.window.as_deref(), Some("256m"));
    }
    assert_eq!(cfg.ping.count, 180);

    // 同机组合（桥接/回环）必须活着走到 cfg：这正是「默认列出全部组合」要覆盖的
    // 那一类链路，被静默丢掉的话这份项目就名不副实。
    let same_host = cfg
        .tests
        .iter()
        .filter(|t| t.src.split(':').next() == t.dst.split(':').next())
        .count();
    assert!(same_host > 0, "同机组合被丢掉了");

    // 这份预设有多大，是用户点「开始测试」之前最该知道的一个数：
    // 10 对 × 3 方向 × 2 IP × (TCP 2 档 + UDP 2 档 + PING 3 档包长) = 210 个单元，
    // 预计 11 小时 26 分。钉住它既给文档一个可信来源，也让「改了某个参数导致
    // 单元数翻倍」当场可见——这份预设本来就贴着「跑一整夜」的量级，多一维就过夜跑不完。
    let units = canonical_plan_units(&cfg, &state);
    let est: u64 = units.iter().map(|u| u.est_secs).sum();
    assert_eq!(
        units.len(),
        210,
        "全量预设的单元数变了（预计耗时 {} 小时 {} 分）——文档里的数字要一起改",
        est / 3600,
        (est % 3600) / 60
    );
    assert!(
        (11 * 3600..13 * 3600).contains(&est),
        "全量预设预计耗时 {est} 秒，偏离「跑一整夜」的量级"
    );
}

/// 端到端走一遍控制台真实的执行路径，确认计划闸门放行。
///
/// 控制台的路是：编译计划 → 把 cfg 写成临时 JSON → `run_master` 读回来 →
/// 按 `MasterOpts` 覆盖几个开关 → 重新推导单元 → 与确认过的哈希核对。
///
/// 中间任何一环有损，闸门就会把**每一次**控制台运行判成「计划已过期」——
/// 功能全好，全被自己的闸门挡死。这条用例就是把那一整条路走完。
#[test]
fn a_console_run_survives_the_temp_config_round_trip_and_passes_the_plan_gate() {
    use crate::master::plan::ExecutionPlan;

    let state = state_with_pair();
    let req = suite_request();
    let compiled = compile_request(&state, &req).expect("compile");
    let confirmed = compiled.plan_hash.clone();
    assert!(!confirmed.is_empty(), "预览必须给出计划哈希");

    // 控制台把 cfg 落成临时 JSON 交给 run_master。
    let json = serde_json::to_string_pretty(&compiled.cfg).expect("写临时配置");
    let mut reloaded: Config = serde_json::from_str(&json).expect("读回临时配置");

    // run_master 依 MasterOpts { auto: true, no_open: true, .. } 施加的覆盖。
    reloaded.open_report = false;

    // run_master 侧的推导。
    let executed = canonical_plan_units(&reloaded, &state);
    let plan = ExecutionPlan::new(
        &reloaded,
        crate::master::plan::topology_fingerprint(&state.master, &state.agent),
        executed,
        Vec::new(),
    );

    assert!(
        plan.matches(Some(&confirmed)),
        "控制台确认的计划在执行端被判成过期了：确认 {confirmed} / 执行 {}",
        plan.plan_hash
    );
    assert!(!plan.is_empty(), "这一轮应当有单元可跑");
}

/// 闸门要真的能拦住「确认之后计划变了」。
#[test]
fn the_plan_gate_rejects_a_plan_that_changed_after_confirmation() {
    use crate::master::plan::ExecutionPlan;

    let state = state_with_pair();
    let confirmed = compile_request(&state, &suite_request())
        .expect("compile")
        .plan_hash;

    // 确认之后对端换了 IP：推导出的单元跟着变，闸门必须拦下。
    let mut moved = state_with_pair();
    moved.agent.interfaces[0].ipv4 = "192.168.0.200".into();
    let after = compile_request(&moved, &suite_request()).expect("compile after move");
    let plan = ExecutionPlan::new(
        &after.cfg,
        crate::master::plan::topology_fingerprint(&moved.master, &moved.agent),
        canonical_plan_units(&after.cfg, &moved),
        Vec::new(),
    );
    assert!(
        !plan.matches(Some(&confirmed)),
        "对端 IP 变了却仍然放行，闸门形同虚设"
    );

    // 没走确认流程（命令行直跑）时不设闸。
    assert!(plan.matches(None), "命令行直跑不该被闸门拦");
    assert!(plan.matches(Some("  ")), "空哈希等同于没确认过");
}

// ---------------------------------------------------------------------------
// 构建产物的不变量
//
// 控制台页面是 `include_str!` 进二进制的 Vite 产物（源码在 `ui/`）。下面四条是
// 这条构建链**唯一**不需要 Node 就能跑的机器保证：`cargo test` 在本地和 CI 上
// 都会跑，贡献者不装 Node 也拦得住。
//
// 前三条守的是「产物长得对不对」，第四条守的是「产物是不是从当前源码来的」——
// 缺了第四条，前三条对一份陈旧产物同样全绿。
// ---------------------------------------------------------------------------

/// 产物里不许有任何需要浏览器另发一次请求去取的子资源。
///
/// 这是铁律 3（鉴权先于路由）的直接推论，也是它**唯一**的机器保证：
/// `webui/http.rs::handle` 的 token 校验在任何分支之前，而浏览器不会给
/// `<script src>` / `<link href>` / 外链 `<img>` 带自定义头，所以任何外链子资源
/// 都会被挡成 401——页面会白屏，而且是只在「真的带 token 打开控制台」时才白，
/// 开发期 vite dev server 上一切正常。
///
/// 这条不是风格偏好，是 `ui/vite.config.ts` 里那个 `viteSingleFile` 存在的理由。
#[test]
fn the_embedded_page_has_no_external_subresources() {
    const PAGE: &str = include_str!("../webui.html");
    for (pattern, why) in [
        (
            r#"(?i)<script\b[^>]*\bsrc\s*="#,
            "页面里有 <script src=...>，会被鉴权挡成 401",
        ),
        (
            r#"(?i)<link\b[^>]*\brel\s*=\s*["']?(stylesheet|modulepreload|preload)"#,
            "页面里有 <link rel=stylesheet/preload>，同上",
        ),
        (
            r#"(?i)\b(?:src|href)\s*=\s*["'](?:https?:)?//"#,
            "页面里有指向外部主机的 src/href；运行期是离线内网",
        ),
        (
            r#"(?i)@import\s+(?:url\()?["']?(?:https?:)?//"#,
            "CSS 里有外部 @import",
        ),
        (r#"@font-face"#, "页面里有 @font-face，字体必须是系统字体"),
    ] {
        let re = regex::Regex::new(pattern).expect("pattern");
        assert!(
            !re.is_match(PAGE),
            "{why}（命中：{:?}）",
            re.find(PAGE).map(|m| m.as_str())
        );
    }
}

/// 产物里不许出现 `eval` / `new Function`。
///
/// 控制台的 CSP 里没有 `'unsafe-eval'`，而且**不许加**——加了等于把模板字符串
/// 变成可执行面。只有 Vue 的「完整版（含运行期模板编译器）」需要它，走 SFC
/// 预编译则不需要。这条挡的是有人不小心把 alias 指回完整版：那种情况下页面在
/// dev server 上照常工作，只有装进 exe 打开才在控制台里报 CSP 违规、整页不动。
#[test]
fn the_embedded_page_never_evals() {
    const PAGE: &str = include_str!("../webui.html");
    for (pattern, why) in [
        (r#"(?:^|[^.\w$])eval\s*\("#, "页面里有 eval("),
        (r#"new\s+Function\s*\("#, "页面里有 new Function("),
    ] {
        let re = regex::Regex::new(pattern).expect("pattern");
        assert!(
            !re.is_match(PAGE),
            "{why}，而 CSP 里没有 unsafe-eval（也不许加）"
        );
    }
}

/// 产物里必须有 Vue 要挂上去的那个根节点。
///
/// `main.ts` 是 `createApp(App).mount('#app')`。挂载点没了不会有任何报错，
/// 页面就是一片空白——这种失败最容易在「改 index.html 顺手清理」时发生。
#[test]
fn the_embedded_page_mounts_into_the_expected_root() {
    const PAGE: &str = include_str!("../webui.html");
    let re = regex::Regex::new(r#"id\s*=\s*["']app["']"#).expect("pattern");
    assert!(re.is_match(PAGE), "产物里找不到 id=\"app\" 挂载点");
}

/// 产物必须是从**当前这份** `ui/` 源码构建出来的。
///
/// 上面三条防不住这个仓库最可能犯的日常错误：改了 `ui/src/` 却忘了重新构建。
/// 陈旧的产物一样没有外链、没有 eval、一样有挂载点，三条全绿，而用户拿到的
/// 是上一个版本的界面。
///
/// 所以 `ui/scripts/emit.mjs` 在写产物时算一枚源码树的 MD5 戳写进产物尾部，
/// 这里按**逐字相同**的算法重算一遍比对。它只比源码、不比产物字节，因此不受
/// esbuild 版本或构建机差异影响，也不要求 CI 装 Node——`cargo test` 就够。
///
/// 算法（改动要同步改 `emit.mjs` 与 `.ai/PLAN-v5.0-frontend.md` §6.3）：
///   1. 文件 = `STAMP_ROOTS` 那几份 + `ui/src/` 下全部文件（递归）。
///   2. 路径取相对 `ui/` 的 POSIX 形式，按字节升序排序（全 ASCII 由
///      `lint-arch.mjs` 第 7 条保证，所以 Node 的 sort 与 Rust 的 `String` 序一致）。
///   3. 内容做 CRLF -> LF 归一（Windows 检出必然带 CRLF）。
///   4. 拼 `path + "\n" + content + "\n"`，整体取 MD5，小写十六进制。
#[test]
fn the_embedded_page_was_built_from_the_current_ui_sources() {
    const PAGE: &str = include_str!("../webui.html");
    const STAMP_ROOTS: &[&str] = &[
        "index.html",
        "package.json",
        "package-lock.json",
        "tsconfig.json",
        "vite.config.ts",
    ];

    let ui_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");

    /// 测试专用文件不进戳：它们进不了产物。
    ///
    /// 算法两端必须**逐字一致**，对应 `emit.mjs` 里的 `isTestOnly`，那边是
    /// `/\.(test|spec)\.[cm]?[jt]sx?$/`。这里以前只硬写了 `.test.ts` /
    /// `.spec.ts` / `.test.js` / `.spec.js` 四个后缀，比正则窄——加一个
    /// `Foo.test.tsx` 或 `.test.mjs`，JS 侧排除、Rust 侧收进戳，两边算出不同的
    /// 哈希，于是一份刚刚构建好的产物会被这条测试指着说「陈旧」。
    ///
    /// 戳本身就是为了防「两份实现漂开」而存在的，它自己更不能有两份实现。
    /// 所以下面按同一个文法展开，而不是再抄一串后缀。
    fn is_test_only(rel: &str) -> bool {
        if rel.contains("__fixtures__/") {
            return true;
        }
        // 尾部：`.` + 可选的 `c`/`m` + `j`/`t` + `s` + 可选的 `x`
        let rest = rel.strip_suffix('x').unwrap_or(rel);
        let Some(rest) = rest.strip_suffix('s') else {
            return false;
        };
        let Some(rest) = rest.strip_suffix(['j', 't']) else {
            return false;
        };
        let rest = rest.strip_suffix(['c', 'm']).unwrap_or(rest);
        // 剩下的必须以 `.test.` 或 `.spec.` 结尾。
        rest.ends_with(".test.") || rest.ends_with(".spec.")
    }

    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("读不了 {}: {e}", dir.display()))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .expect("在 ui/ 下")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                if !is_test_only(&rel) {
                    out.push(rel);
                }
            }
        }
    }

    let mut files: Vec<String> = STAMP_ROOTS.iter().map(|s| (*s).to_string()).collect();
    walk(&ui_root.join("src"), &ui_root, &mut files);
    files.sort();

    let mut blob = String::new();
    for rel in &files {
        let content = std::fs::read_to_string(ui_root.join(rel))
            .unwrap_or_else(|e| panic!("读不了 ui/{rel}: {e}"));
        blob.push_str(rel);
        blob.push('\n');
        blob.push_str(&content.replace("\r\n", "\n"));
        blob.push('\n');
    }
    let expected = crate::util::md5_hex(&blob);

    let marker = "<!-- cpe-ui-stamp: ";
    let at = PAGE.find(marker).unwrap_or_else(|| {
        panic!(
            "产物里没有溯源戳。它是 emit.mjs 写的：在 ui/ 下跑 npm ci && npm run build，\
             并把 src/master/webui.html 一起提交。"
        )
    });
    let rest = &PAGE[at + marker.len()..];
    let actual = &rest[..rest.find(' ').unwrap_or(rest.len())];

    assert_eq!(
        actual, expected,
        "\n\nui/ 的源码改了，但 src/master/webui.html 没有重新构建。\n\
         去 ui/ 下跑：npm ci && npm run build\n\
         然后把产物 src/master/webui.html 和源码一起提交。\n\
         （产物里的戳 {actual}，当前源码算出来是 {expected}）\n"
    );
}

/// `/api/progress` 必须同时给出日志文本和结构化状态，且各走各的游标。
///
/// 这是 ADR-2 落到端点上的样子：`lines` 给日志屏（**给人看的**，文案随便改），
/// `run` 给进度页（**给机器读的**）。在此之前只有 `lines`，前端得去解析
/// `[i/total]` 和「==> 单元结果:」两种日志行才能画出单元级进度——一次 11.5
/// 小时的测试有三万行日志，刷新一次页面就要全量重放一遍。
#[test]
fn the_progress_endpoint_serves_structured_status_next_to_the_log_text() {
    use crate::master::run_status::{RunObserver, UnitStatus};

    let console = console_with(state_with_pair());
    console
        .run_status
        .run_started("run_demo", "plan-hash", 3, 300);
    console.run_status.unit_finished(
        UnitStatus {
            seq: 1,
            title: "IPERF V4 TCP".into(),
            verdict: "PASS".into(),
            reason_code: "RX_TARGET_MET".into(),
            reason_detail: "达标".into(),
            skipped: false,
            secs: 12,
            link_group: "SGMII ↔ WLAN".into(),
        },
        200,
    );

    let out = api_progress(&console, "from=0&units_from=0");
    assert_eq!(out["run"]["run_id"], "run_demo");
    assert_eq!(out["run"]["plan_hash"], "plan-hash");
    assert_eq!(out["run"]["total_units"], 3);
    assert_eq!(out["run"]["counts"]["pass"], 1);
    assert_eq!(out["run"]["done"][0]["seq"], 1);
    assert_eq!(out["run"]["done"][0]["verdict"], "PASS");
    assert_eq!(out["run"]["eta_secs"], 200);
    // 端点回一个可以直接拿去当下一拍入参的游标。
    assert_eq!(out["units_from"], 1);
    // 日志那一路原样保留。
    assert!(out.get("lines").is_some(), "日志屏还要靠 lines");

    // 带上游标之后不再重传已完成单元。
    let next = api_progress(&console, "from=0&units_from=1");
    assert!(
        next["run"]["done"].as_array().expect("done").is_empty(),
        "稳态每拍不该重传已完成单元"
    );
    assert_eq!(next["run"]["counts"]["pass"], 1, "计数仍是全量");
    assert_eq!(next["units_from"], 1);
}

/// 报告路径来自回调，不再从日志里搜「报告已生成: 」。
///
/// 那个做法把一句给人看的提示语变成了协议：改个措辞，界面上的「打开报告」
/// 按钮就永远是灰的，而且没有任何测试会红。
#[test]
fn the_report_path_reaches_the_console_without_scraping_the_log() {
    use crate::master::run_status::RunObserver;

    let console = console_with(state_with_pair());
    console.run_status.run_started("run_demo", "hash", 1, 10);
    assert_eq!(api_progress(&console, "from=0")["report"], "");

    console
        .run_status
        .report_written("runs/run_demo/report.html");
    let out = api_progress(&console, "from=0");
    assert_eq!(out["run"]["report"], "runs/run_demo/report.html");
    // `/api/open-report` 读的是 console.report，两边必须同步。
    assert_eq!(out["report"], "runs/run_demo/report.html");
    assert_eq!(
        lock_recover(&console.report).clone(),
        "runs/run_demo/report.html"
    );
}

/// 套件计划经由 `config.json` 往返会**丢掉套件**，导入时必须说出来。
///
/// A14：`ImportOut` 只回填矩阵态，而 `Config` 根本不承载套件——「工作台搭好
/// 套件 → 下载 config → 再导回来」会把任务顺序、逐任务时长、验收目标全部降级
/// 成一张扁平矩阵。此前六条 notice 里一条都没提这件事，用户毫无提示地跑出了
/// 另一份东西。模块头注释还写着「两边必须互为逆运算」——对套件计划那句话
/// 从来就不成立。
#[test]
fn importing_a_suite_derived_config_warns_that_the_suite_is_lost() {
    let state = state_with_pair();
    let req = suite_request();
    // 先走一遍「工作台 → config.json」这半程。
    let compiled = compile_request(&state, &req).expect("compile");
    let file = serde_json::to_string(&compiled.cfg).expect("导出 config");

    let console = console_with(state_with_pair());
    let out = api_import(&console, &file).expect("导入本身仍然要成功");
    let notices = out["notices"].as_array().expect("notices");
    let text = notices
        .iter()
        .filter_map(|n| n.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        text.contains("套件"),
        "套件被降级成矩阵这件事必须出现在 notices 里，实得：{text}"
    );
    assert!(
        text.contains("项目文件") || text.contains("cpe-ui-project"),
        "要告诉用户正路在哪：{text}"
    );

    // 反面：不是从工作台来的扁平配置，不该出现这条噪声。
    let plain = Config {
        tests: vec![TestSpec {
            name: "手写的".into(),
            src: "master:NAME=以太网 6".into(),
            dst: "agent:NAME=WLAN 3".into(),
            direction: OneOrMany::One("A->B".into()),
            kinds: vec!["iperf".into()],
            transports: vec!["tcp".into()],
            ip: vec!["v4".into()],
            streams: 1,
            tcp_streams: None,
            udp_streams: None,
            iperf_duration: None,
            ping_count: None,
            ping_payload_sizes: None,
            tcp_windows: None,
            udp_profiles: None,
            rate_mode: None,
            rate_targets_mbps: None,
            rate_targets_bidir_mbps: None,
            link_group: None,
            origin: None,
        }],
        ..Config::default()
    };
    let console = console_with(state_with_pair());
    let out = api_import(&console, &serde_json::to_string(&plain).unwrap()).expect("导入");
    let text = out["notices"]
        .as_array()
        .expect("notices")
        .iter()
        .filter_map(|n| n.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        !text.contains("套件的任务顺序"),
        "扁平配置不该报套件丢失：{text}"
    );
}

/// 把每个对外 DTO 的样例序列化成 JSON 写进 `ui/src/api/__fixtures__/`。
///
/// 这是 `ui/src/api/dto.ts` 的**契约测试的 Rust 半边**（DESIGN §7 第 6 条）：
/// 手写的 TS 类型和 Rust 的 `*Out` 之间没有代码生成，靠这批固定样例对齐。
/// Rust 侧改了字段名却没改 dto.ts，Vitest 那半边会红；反过来 dto.ts 里凭空
/// 多出一个字段，也会在 Vitest 里被抓到。
///
/// 之所以**写文件**而不是内联比对：固定样例同时也是前端写单测时的输入，
/// 让两边看同一份 JSON 比让两边各自造假数据可靠。
///
/// 这个测试只**产出**，不断言——它不该在 CI 上因为「文件和上次不一样」而红
/// （那是 `emit --check` 和溯源戳的职责）。它的价值在于：改了 DTO 跑一次
/// `cargo test`，前端那半边的固定样例就自动跟上了。
#[test]
fn dto_fixtures_are_regenerated_for_the_frontend_contract_test() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/src/api/__fixtures__");
    if std::fs::create_dir_all(&dir).is_err() {
        // 源码树只读（比如从 tarball 解出来跑测试）时静默跳过：
        // 这个测试是开发期工具，不是产品不变量。
        return;
    }

    let write = |name: &str, value: serde_json::Value| {
        let text = serde_json::to_string_pretty(&value).expect("样例必须能序列化");
        let _ = std::fs::write(dir.join(format!("{name}.json")), format!("{text}\n"));
    };

    // 运行状态：v6.0 新增的结构化进度，前端进度页完全建在它上面。
    let recorder = crate::master::run_status::RunStatusRecorder::new();
    {
        use crate::master::run_status::{CurrentUnit, RunObserver, UnitStatus};
        recorder.run_started("run_20260830_101112_1234", "plan-hash-abc", 3, 300);
        recorder.unit_finished(
            UnitStatus {
                seq: 1,
                title: "IPERF V4 TCP -w 4m -P 10 | 主控 eth0 -> 辅测 eth0".into(),
                verdict: "PASS".into(),
                reason_code: "RX_TARGET_MET".into(),
                reason_detail: "网卡 RX 平均 2310.500Mbps 达到目标 2000.000Mbps".into(),
                skipped: false,
                secs: 182,
                link_group: "SGMII ↔ WLAN".into(),
            },
            120,
        );
        recorder.unit_started(CurrentUnit {
            seq: 2,
            title: "IPERF V4 UDP -b 2500m | 主控 eth0 -> 辅测 eth0".into(),
            est_secs: 180,
            started_at: "2026-08-30 10:14:20".into(),
            link_group: "SGMII ↔ WLAN".into(),
        });
    }
    let (_, mut run) = recorder.snapshot(0, None);
    // 固定时间戳：`run_started` 填的是 `now_full()`，直接写出去会让每次
    // `cargo test` 都改动这批文件——`git status` 常年是脏的，而且它们进
    // 溯源戳的输入集时会让产物每跑一次测试就"过期"一次。
    run.started_at = "2026-08-30 10:11:12".into();
    write(
        "run_status",
        serde_json::to_value(&run).expect("RunStatus 必须能序列化"),
    );
    write(
        "progress_out",
        serde_json::to_value(ProgressOut {
            running: true,
            from: 1420,
            lines: vec!["[2/3] IPERF V4 UDP -b 2500m".into()],
            report: String::new(),
            units_from: run.done.len(),
            run,
        })
        .expect("ProgressOut 必须能序列化"),
    );

    // 计划：复核树直接渲染 sections + trace，不许在前端重排。
    let console = console_with(state_with_pair());
    if let Ok(plan) = api_plan(&console, &serde_json::to_string(&suite_request()).unwrap()) {
        write("plan_out", plan);
    }
    {
        let state = lock_recover(&console.state);
        if let Ok(value) = serde_json::to_value(bootstrap_out(&state)) {
            write("bootstrap_out", value);
        }
    }
}

/// `runs/<id>/bundle.zip` 的 id 解析是**白名单式**的：只认已经存在的目录名。
///
/// 不是「过滤掉危险字符」，而是「只认我自己列出来的那些名字」——前者要穷举
/// 所有能表示上级目录的写法（`..`、`%2e%2e`、绝对路径、Windows 的 `\`、
/// 盘符…），后者不存在这个问题面。
#[test]
fn the_bundle_id_only_resolves_to_directories_that_actually_exist() {
    use super::runs::resolve_run_dir;

    for evil in [
        "..",
        ".",
        "../etc",
        "../../etc/passwd",
        "runs/../..",
        "/etc/passwd",
        "a/b",
        "a\\b",
        ".hidden",
        "",
        // 盘符：上面那段注释一直把它列为「黑名单要穷举的写法之一」，而实现
        // 当时恰好漏了它。Windows 上 `Path::new("runs").join("C:")` 会被盘符
        // 前缀整个替换成 `C:`（= 该盘的当前目录），于是打包的是进程 CWD。
        // 现在解析按 `runs/` 下的真实目录名精确比对，这些都拿不到目录。
        "C:",
        "c:",
        "C:Windows",
        r"\\?\C:\Windows",
        "\\\\server\\share",
    ] {
        assert!(resolve_run_dir(evil).is_none(), "{evil:?} 不该解析出目录");
    }
    // 不存在的名字也拿不到目录——白名单的另一半。
    assert!(resolve_run_dir("run_does_not_exist_12345").is_none());
}

/// 报告包必须是**真的能解开的 zip**，而且解开就是 `run_<id>/…`。
///
/// 打包这件事本身现在交给 `zip` crate（此前是手写的，条目数 `u16`、大小与
/// 偏移 `u32`、没有 zip64，越界时静默产出坏包）。这条测试守的不是 crate 的
/// 正确性，而是**我们对它的用法**：store 模式、顶层套一层 run 目录、子目录
/// 用 `/` 分隔。这三条写错时产物看起来仍是个正常文件。
#[test]
fn the_report_bundle_is_a_zip_that_really_unpacks() {
    use super::runs::build_bundle;

    let dir = std::env::temp_dir().join(format!(
        "cpe_bundle_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(dir.join("iperf_outputs")).expect("temp dir");
    std::fs::write(dir.join("report.html"), "<html>报告正文</html>").expect("report");
    std::fs::write(dir.join("rows.jsonl"), "{\"a\":1}\n{\"a\":2}\n").expect("rows");
    std::fs::write(dir.join("iperf_outputs/raw.log"), "iperf3 原始输出").expect("raw");

    // 产物是临时 zip 文件（不再是内存里的 `Vec`）：守卫在，文件就在。
    let bundle = build_bundle(&dir, "run_demo").expect("打包");
    assert!(bundle.path.is_file(), "打包产物应当落在临时文件里");
    let bytes = std::fs::read(&bundle.path).expect("读回打包产物");
    assert_eq!(&bytes[..2], b"PK", "不是 zip");
    // 中央目录结束记录的魔数必须在（很多解压器只认它来定位条目表）。
    assert!(
        bytes.windows(4).any(|w| w == 0x0605_4b50u32.to_le_bytes()),
        "缺少 end-of-central-directory"
    );

    // 顶层套一层 run 目录：解开是 run_demo/report.html，而不是把文件散进
    // 用户的下载目录。
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("run_demo/report.html"), "顶层目录名不对");
    assert!(
        text.contains("run_demo/iperf_outputs/raw.log"),
        "子目录没进包"
    );
    // store 模式：原文应当能在包里直接看到。
    assert!(text.contains("报告正文"), "store 模式下原文应当原样在包里");

    // 用系统 unzip 真校验一遍——结构写错时产物看起来仍是个文件，只有真解压
    // 才知道。`unzip` 不支持从 stdin 读，所以先落盘。
    let zip_path = dir.join("bundle.zip");
    std::fs::write(&zip_path, &bytes).expect("写 zip");
    // 机器上没有 unzip（比如精简的 Windows CI 镜像）时跳过这一步，
    // 上面那几条结构断言仍然生效。
    if let Ok(result) = std::process::Command::new("unzip")
        .arg("-t")
        .arg(&zip_path)
        .output()
    {
        assert!(
            result.status.success(),
            "unzip -t 判定这个包坏了：\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    // 守卫析构就要把临时 zip 删掉：它和 run 目录一样大，漏一个就在用户的
    // 临时目录里堆一份。
    let temp_zip = bundle.path.clone();
    drop(bundle);
    assert!(!temp_zip.exists(), "Bundle 析构后临时 zip 必须消失");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `/api/runs` 列出历史运行，新的在前。
#[test]
fn the_run_list_puts_the_newest_first() {
    let value = super::runs::api_runs().expect("列目录不该失败");
    let entries = value.as_array().expect("数组");
    // 每一项的形状是前端契约。
    for entry in entries {
        for key in [
            "id",
            "modified",
            "has_report",
            "has_rows",
            "has_xlsx",
            "bytes",
        ] {
            assert!(entry.get(key).is_some(), "RunEntry 少了字段 {key}");
        }
    }
    // 目录名带时间戳，倒序即最新在前。
    let ids: Vec<&str> = entries.iter().filter_map(|e| e["id"].as_str()).collect();
    let mut sorted = ids.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(ids, sorted, "历史运行应当新的在前");
}
