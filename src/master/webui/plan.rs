//! 把一份 UI 请求编译成**可执行计划**。
//!
//! 这是 WebUI 最要紧的一层：界面上的勾选最终要变成一条条具体的 iperf/ctsTraffic
//! 命令，而用户看到的确认页必须和真正跑的东西是同一个东西。`plan_hash` 就是
//! 为这件事存在的——编译一次、展示这一次的结果、开跑时再核对一次哈希；
//! 中间任何一步让「界面状态」重新参与推导，确认页就失去意义了。

use super::*;
use crate::master::plan::ExecutionPlan;

#[allow(dead_code)]
pub(super) fn validated_config_from_request(
    state: &UiState,
    req: &RunRequest,
) -> Result<Config, String> {
    validate_request(state, req)?;
    let cfg = config_from_request(state, req);
    let problems = cfg.validate();
    if problems.is_empty() {
        Ok(cfg)
    } else {
        Err(format!("配置项异常：{}", problems.join("；")))
    }
}

/// 一轮里所有配对共用的档位。
///
/// 逐对覆盖只在这几项上做减法（某一行自己的 `-b`、自己的流数），所以把它们收成
/// 一个东西传给 `specs_for_pair`，而不是把七八个列表一路传参——那样每加一档
/// 扫描维度就要改三处签名。
pub(super) struct Sweeps {
    /// 第 0 项是默认组（执行区的 `-w` / `-P` 两个框），其余是附加组。
    pub(super) tcp_groups: Vec<ResolvedTcpGroup>,
    /// 第 0 项是默认组（执行区那几个框），其余是附加组。
    pub(super) udp_groups: Vec<ResolvedUdpGroup>,
    pub(super) ping_sizes: Vec<u32>,
    pub(super) duration: u64,
    /// 在「网口与策略」里单独指定了 UDP `-b` 的网口。
    pub(super) pinned_senders: HashSet<String>,
}

/// 一组 UDP 参数展开成「跑什么」。
#[derive(Debug, Clone, Default)]
pub(super) struct ResolvedUdpGroup {
    pub(super) bandwidths: Vec<String>,
    pub(super) lengths: Vec<String>,
    pub(super) windows: Vec<String>,
    pub(super) streams: u32,
    /// 只有默认组会用到：执行区的 `-b` 留空时，沿用配置文件里那份 profile
    /// **原样**。那份不一定是整齐的叉积（可以是 `1m/64` + `500m/1400`），
    /// 拆成三个轴再乘回去会把它变成另一组档位。
    pub(super) verbatim: Option<Vec<UdpProfile>>,
}

impl ResolvedUdpGroup {
    pub(super) fn profiles(&self) -> Vec<UdpProfile> {
        if let Some(profiles) = &self.verbatim {
            return profiles.clone();
        }
        self.bandwidths
            .iter()
            .flat_map(|bandwidth| udp_profiles_for(bandwidth, &self.lengths, &self.windows))
            .collect()
    }
}

/// 一组 TCP 参数展开成「跑什么」：`-w × -P` 两个轴。第 0 组是默认组。
#[derive(Debug, Clone, Default)]
pub(super) struct ResolvedTcpGroup {
    /// socket buffer 档位。默认组经过 `non_empty` 兜底不会为空；附加组留空
    /// 表示这一维不下发 `-w`（builder 见到空列表跑一条不带 `-w` 的）。
    pub(super) windows: Vec<String>,
    /// 并发流数档位；空按 `[1]`（builder 那边 -P 恒发，和 UDP 流数同理）。
    pub(super) stream_steps: Vec<u32>,
}

impl Sweeps {
    /// 选中的那一组。越界回落到默认组——校验已经挡过一次，这里不该再 panic。
    pub(super) fn udp_group(&self, index: usize) -> &ResolvedUdpGroup {
        self.udp_groups.get(index).unwrap_or(&self.udp_groups[0])
    }
    pub(super) fn tcp_group(&self, index: usize) -> &ResolvedTcpGroup {
        self.tcp_groups.get(index).unwrap_or(&self.tcp_groups[0])
    }
}

/// 把界面状态翻译成一份 config。规划和执行都走这一个函数，
/// 保证「预计耗时」和真正跑的是同一份东西。
pub(super) fn config_from_request(state: &UiState, req: &RunRequest) -> Config {
    if let Some(plan) = req.ui_plan.as_ref() {
        return config_from_ui_plan(state, req, plan);
    }
    let mut cfg = state.cfg.clone();
    cfg.agent_host = state.agent_host.clone();
    cfg.screenshot = req.screenshot;
    cfg.limit_udp_by_link_speed = req.limit_udp_by_link_speed;
    cfg.resume = req.resume;
    cfg.iperf.duration = req.duration.clamp(1, 86_400);
    cfg.pairs = None;
    cfg.universal_params = None;
    cfg.link_profiles.by_nic.clear();

    let windows = non_empty(&req.tcp_windows, &cfg.iperf.tcp_windows);
    let stream_steps: Vec<u32> = {
        let picked: Vec<u32> = req.tcp_streams.iter().copied().filter(|n| *n > 0).collect();
        if picked.is_empty() {
            vec![1]
        } else {
            picked
        }
    };
    let lengths = cleaned_list(&req.udp_lengths);
    let udp_windows = cleaned_list(&req.udp_windows);
    // 保序去重。`dedup()` 只合并相邻项，「32 1600 32」会留下两个 32——两个单元
    // 标题和 resume id 完全一样，在 task_results.json 里互相覆盖（后写的那条
    // 赢），resume 于是可能跳过一个其实 FAIL 了的单元，还白跑一遍全程。
    // 也不能先排序：档位顺序是用户自己排的，跑的顺序就该照他写的来。
    let mut seen_sizes = HashSet::new();
    let ping_sizes: Vec<u32> = req
        .ping_payload_sizes
        .iter()
        .copied()
        .filter(|size| *size > 0 && seen_sizes.insert(*size))
        .collect();
    // 默认组的档位同时写回 `iperf.udp_profiles`：下载出来的 config 交给
    // `master --auto` 跑时，没有「组」这个概念，读的就是这一份。
    let global_udp: Vec<UdpProfile> = req
        .udp_bandwidths
        .iter()
        .filter(|b| !b.trim().is_empty())
        .flat_map(|b| udp_profiles_for(b.trim(), &lengths, &udp_windows))
        .collect();
    if !global_udp.is_empty() {
        cfg.iperf.udp_profiles = global_udp;
    }
    cfg.iperf.tcp_windows = windows.clone();

    for policy in &req.nic_policies {
        if let Some(profile) = nic_profile(policy) {
            cfg.link_profiles.by_nic.push(profile);
        }
    }

    // 默认组 = 执行区那几个框；`-b` 留空时沿用配置文件里那份 profile 原样。
    let bandwidths = cleaned_list(&req.udp_bandwidths);
    let mut udp_groups = vec![ResolvedUdpGroup {
        verbatim: bandwidths
            .is_empty()
            .then(|| cfg.iperf.udp_profiles.clone()),
        bandwidths,
        lengths,
        windows: udp_windows,
        streams: req.udp_streams.max(1),
    }];
    udp_groups.extend(req.udp_groups.iter().map(|group| ResolvedUdpGroup {
        bandwidths: cleaned_list(&group.bandwidths),
        lengths: cleaned_list(&group.lengths),
        windows: cleaned_list(&group.windows),
        streams: group.streams.max(1),
        verbatim: None,
    }));

    // 默认 TCP 组 = 执行区的 `-w` / `-P`。`windows` 已经过 `non_empty` 兜底
    // （空则回落到配置里的 tcp_windows），`stream_steps` 空则是 `[1]`。
    let mut tcp_groups = vec![ResolvedTcpGroup {
        windows: windows.clone(),
        stream_steps: stream_steps.clone(),
    }];
    // 附加组不兜底：`-w` 留空就是那一维不下发 `-w`；`-P` 留空按 `[1]`。
    tcp_groups.extend(req.tcp_groups.iter().map(|group| {
        let steps: Vec<u32> = group.streams.iter().copied().filter(|n| *n > 0).collect();
        ResolvedTcpGroup {
            windows: cleaned_list(&group.windows),
            stream_steps: if steps.is_empty() { vec![1] } else { steps },
        }
    }));

    let sweeps = Sweeps {
        tcp_groups,
        udp_groups,
        ping_sizes,
        duration: req.duration.clamp(1, 86_400),
        pinned_senders: udp_pinned_senders(req),
    };
    cfg.tests = req
        .pairs
        .iter()
        .enumerate()
        .flat_map(|(idx, pair)| specs_for_pair(idx, pair, req, &sweeps))
        .collect();
    cfg
}

/// Apply the request-wide settings shared by legacy and suite requests.  The
/// suite compiler calls this directly so it does not have to manufacture a
/// `PairSelection` (which would re-introduce the old shared TCP/UDP fields).
pub(super) fn ui_request_base_config(state: &UiState, req: &RunRequest) -> Config {
    let mut cfg = state.cfg.clone();
    cfg.agent_host = state.agent_host.clone();
    cfg.screenshot = req.screenshot;
    cfg.limit_udp_by_link_speed = req.limit_udp_by_link_speed;
    cfg.resume = req.resume;
    cfg.iperf.duration = req.duration.clamp(1, 86_400);
    cfg.pairs = None;
    cfg.universal_params = None;
    cfg.link_profiles.by_nic.clear();

    // Quick-plan tasks intentionally keep protocol-specific knobs on the
    // task, but PING's convenient default controls still live at the request
    // level (the same controls used by the legacy matrix).  Carry them into
    // the compiled config before a task falls back to cfg.ping; otherwise a
    // user changing "5 次 / 64 字节" in the quick workbench would silently
    // execute the values from the loaded config instead.
    if req.ping_count > 0 {
        cfg.ping.count = req.ping_count;
    }
    if !req.ping_payload_sizes.is_empty() {
        let mut seen = HashSet::new();
        cfg.ping.payload_sizes = req
            .ping_payload_sizes
            .iter()
            .copied()
            .filter(|size| *size > 0 && seen.insert(*size))
            .collect();
    }

    let tcp_windows = non_empty(&req.tcp_windows, &cfg.iperf.tcp_windows);
    cfg.iperf.tcp_windows = tcp_windows;
    let udp_bandwidths = cleaned_list(&req.udp_bandwidths);
    if !udp_bandwidths.is_empty() {
        let lengths = cleaned_list(&req.udp_lengths);
        let windows = cleaned_list(&req.udp_windows);
        cfg.iperf.udp_profiles = udp_bandwidths
            .iter()
            .flat_map(|b| udp_profiles_for(b, &lengths, &windows))
            .collect();
    }
    for policy in &req.nic_policies {
        if let Some(profile) = nic_profile(policy) {
            cfg.link_profiles.by_nic.push(profile);
        }
    }
    cfg
}

#[derive(Debug, Clone)]
pub(super) struct UiTcpProfile {
    pub(super) recipe_id: String,
    pub(super) window: Option<String>,
    pub(super) streams: u32,
}

#[derive(Debug, Clone)]
pub(super) struct UiUdpProfile {
    pub(super) recipe_id: String,
    pub(super) profile: UdpProfile,
    pub(super) streams: u32,
}

pub(super) fn first_or_one(values: Vec<u32>, fallback: u32) -> Vec<u32> {
    let values: Vec<u32> = values.into_iter().filter(|v| *v > 0).collect();
    if values.is_empty() {
        vec![fallback.max(1)]
    } else {
        values
    }
}

pub(super) fn recipe_tcp_profiles(
    recipe: &UiRecipe,
    fallback_streams: &[u32],
) -> Vec<UiTcpProfile> {
    let mut out = Vec::new();
    if !recipe.profiles.is_empty() {
        for profile in &recipe.profiles {
            let streams = profile
                .tcp_streams
                .as_ref()
                .unwrap_or(&profile.streams)
                .values();
            let streams = first_or_one(streams, fallback_streams.first().copied().unwrap_or(1));
            let windows = profile
                .window
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| vec![Some(value.to_string())])
                .unwrap_or_else(|| vec![None]);
            for window in windows {
                for stream in &streams {
                    out.push(UiTcpProfile {
                        recipe_id: recipe.id.clone(),
                        window: window.clone(),
                        streams: *stream,
                    });
                }
            }
        }
        return out;
    }

    let windows = cleaned_list(if !recipe.tcp_windows.is_empty() {
        &recipe.tcp_windows
    } else {
        &recipe.windows
    });
    let windows: Vec<Option<String>> = if windows.is_empty() {
        vec![None]
    } else {
        windows.into_iter().map(Some).collect()
    };
    let streams = first_or_one(
        recipe.tcp_streams.clone(),
        fallback_streams.first().copied().unwrap_or(1),
    );
    for window in windows {
        for stream in &streams {
            out.push(UiTcpProfile {
                recipe_id: recipe.id.clone(),
                window: window.clone(),
                streams: *stream,
            });
        }
    }
    // An entirely empty recipe is a valid fixed recipe: one TCP stream and no
    // explicit socket window.
    if out.is_empty() {
        out.push(UiTcpProfile {
            recipe_id: recipe.id.clone(),
            window: None,
            streams: 1,
        });
    }
    out
}

pub(super) fn recipe_udp_profiles(
    recipe: &UiRecipe,
    fallback_bandwidths: &[String],
    fallback_streams: u32,
) -> Vec<UiUdpProfile> {
    let mut out = Vec::new();
    if !recipe.udp_profiles.is_empty() {
        let streams = first_or_one(recipe.udp_streams.clone(), fallback_streams);
        for profile in &recipe.udp_profiles {
            for stream in &streams {
                out.push(UiUdpProfile {
                    recipe_id: recipe.id.clone(),
                    profile: UdpProfile {
                        bandwidth: profile.bandwidth.trim().to_string(),
                        length: profile
                            .length
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string),
                        window: profile
                            .window
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string),
                    },
                    streams: *stream,
                });
            }
        }
        return out;
    }
    if !recipe.profiles.is_empty() {
        for profile in &recipe.profiles {
            let bandwidths: Vec<String> = profile
                .bandwidth
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(|value| vec![value.trim().to_string()])
                .unwrap_or_else(|| cleaned_list(&recipe.bandwidths));
            if bandwidths.is_empty() {
                continue;
            }
            let streams = profile
                .udp_streams
                .as_ref()
                .unwrap_or(&profile.streams)
                .values();
            let streams = first_or_one(streams, fallback_streams);
            for bandwidth in bandwidths {
                for stream in &streams {
                    out.push(UiUdpProfile {
                        recipe_id: recipe.id.clone(),
                        profile: UdpProfile {
                            bandwidth: bandwidth.clone(),
                            length: profile
                                .length
                                .as_deref()
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(str::to_string),
                            window: profile
                                .window
                                .as_deref()
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(str::to_string),
                        },
                        streams: *stream,
                    });
                }
            }
        }
        return out;
    }

    let bandwidths = cleaned_list(&recipe.bandwidths);
    let bandwidths = if bandwidths.is_empty() {
        cleaned_list(fallback_bandwidths)
    } else {
        bandwidths
    };
    let lengths = cleaned_list(&recipe.lengths);
    let windows = cleaned_list(&recipe.windows);
    let lengths: Vec<Option<String>> = if lengths.is_empty() {
        vec![None]
    } else {
        lengths.into_iter().map(Some).collect()
    };
    let windows: Vec<Option<String>> = if windows.is_empty() {
        vec![None]
    } else {
        windows.into_iter().map(Some).collect()
    };
    let streams = first_or_one(recipe.udp_streams.clone(), fallback_streams);
    for bandwidth in bandwidths {
        for length in &lengths {
            for window in &windows {
                for stream in &streams {
                    out.push(UiUdpProfile {
                        recipe_id: recipe.id.clone(),
                        profile: UdpProfile {
                            bandwidth: bandwidth.clone(),
                            length: length.clone(),
                            window: window.clone(),
                        },
                        streams: *stream,
                    });
                }
            }
        }
    }
    out
}

pub(super) fn normalized_ui_directions(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for value in raw {
        match canonical_ui_direction(value) {
            Some("both") => {
                for direction in ["ab", "ba"] {
                    if !out.iter().any(|v| v == direction) {
                        out.push(direction.to_string());
                    }
                }
            }
            Some(direction) if !out.iter().any(|v| v == direction) => {
                out.push(direction.to_string())
            }
            _ => {}
        }
    }
    out
}

pub(super) fn normalized_ui_ips(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for value in raw {
        if let Some(ip) = canonical_ui_ip(value) {
            if !out.iter().any(|v| v == ip) {
                out.push(ip.to_string());
            }
        }
    }
    out
}

pub(super) fn ui_task_targets(task: &UiTask) -> Option<crate::config::RateTargets> {
    let ab = parse_rx_target(&task.rx_target_bidir_ab)
        .ok()
        .flatten()
        .and_then(rx_target_mbps);
    let ba = parse_rx_target(&task.rx_target_bidir_ba)
        .ok()
        .flatten()
        .and_then(rx_target_mbps);
    (ab.is_some() || ba.is_some()).then_some(crate::config::RateTargets {
        forward: None,
        ab,
        ba,
    })
}

pub(super) fn ui_task_base_spec(
    name: String,
    pair: &UiPairRef,
    task: &UiTask,
    protocol: &str,
    directions: &[String],
    ips: &[String],
    duration: u64,
) -> TestSpec {
    TestSpec {
        name,
        src: pair.src.clone(),
        dst: pair.dst.clone(),
        direction: OneOrMany::Many(directions.to_vec()),
        kinds: if protocol == "ping" {
            vec!["ping".into()]
        } else {
            vec!["iperf".into()]
        },
        transports: if protocol == "ping" {
            Vec::new()
        } else {
            vec![protocol.to_string()]
        },
        ip: ips.to_vec(),
        streams: 1,
        tcp_streams: None,
        udp_streams: None,
        iperf_duration: Some(task.duration.unwrap_or(duration).clamp(1, 86_400)),
        ping_count: task.ping_count.filter(|value| *value > 0),
        ping_payload_sizes: task.ping_payload_sizes.clone(),
        tcp_windows: None,
        udp_profiles: None,
        rate_mode: task.rate_mode,
        rate_targets_mbps: task.rate_targets_mbps.clone(),
        rate_targets_bidir_mbps: ui_task_targets(task),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn ui_specs_for_task(
    pair: &UiPairRef,
    suite: &UiSuite,
    task: &UiTask,
    recipes: &UiRecipes,
    req: &RunRequest,
    cfg: &Config,
    binding_id: &str,
    link_set_id: &str,
) -> Vec<TestSpec> {
    let Some(protocol) = ui_task_protocol(task) else {
        return Vec::new();
    };
    let directions = normalized_ui_directions(&task.directions);
    let ips = normalized_ui_ips(&task.ip);
    let mut out = Vec::new();
    match protocol.as_str() {
        "tcp" => {
            let selected: Vec<&UiRecipe> = if task.recipe_ids.is_empty() {
                Vec::new()
            } else {
                task.recipe_ids
                    .iter()
                    .filter_map(|id| recipes.tcp.iter().find(|recipe| recipe.id == *id))
                    .collect()
            };
            let fallback_streams: Vec<u32> = req
                .tcp_streams
                .iter()
                .copied()
                .filter(|value| *value > 0)
                .collect();
            let fallback_windows = non_empty(&req.tcp_windows, &cfg.iperf.tcp_windows);
            let fallback = UiRecipe {
                id: "default".into(),
                name: "默认 TCP".into(),
                tcp_windows: fallback_windows.clone(),
                tcp_streams: fallback_streams.clone(),
                ..Default::default()
            };
            let recipes: Vec<&UiRecipe> = if selected.is_empty() {
                vec![&fallback]
            } else {
                selected
            };
            for recipe in recipes {
                for profile in recipe_tcp_profiles(recipe, &fallback_streams) {
                    let suffix = format!(
                        "{}/{}/{}/{}/{}/{}",
                        ui_name_segment(link_set_id),
                        ui_name_segment(binding_id),
                        ui_name_segment(&pair.id),
                        ui_name_segment(&suite.id),
                        ui_name_segment(&task.id),
                        ui_name_segment(&profile.recipe_id)
                    );
                    let mut spec = ui_task_base_spec(
                        format!("ui-plan/{suffix}/tcp-P{}", profile.streams),
                        pair,
                        task,
                        "tcp",
                        &directions,
                        &ips,
                        req.duration,
                    );
                    spec.tcp_streams = Some(profile.streams);
                    spec.tcp_windows = Some(profile.window.into_iter().collect());
                    out.push(spec);
                }
            }
        }
        "udp" => {
            let selected: Vec<&UiRecipe> = if task.recipe_ids.is_empty() {
                Vec::new()
            } else {
                task.recipe_ids
                    .iter()
                    .filter_map(|id| recipes.udp.iter().find(|recipe| recipe.id == *id))
                    .collect()
            };
            let fallback_bandwidths = if req.udp_bandwidths.is_empty() {
                cfg.iperf
                    .udp_profiles
                    .iter()
                    .map(|profile| profile.bandwidth.clone())
                    .collect::<Vec<_>>()
            } else {
                req.udp_bandwidths.clone()
            };
            let mut fallback = UiRecipe {
                id: "default".into(),
                name: "默认 UDP".into(),
                bandwidths: fallback_bandwidths.clone(),
                lengths: req.udp_lengths.clone(),
                windows: req.udp_windows.clone(),
                udp_streams: vec![req.udp_streams.max(1)],
                ..Default::default()
            };
            // With no suite recipe and no request-wide UDP axes, preserve the
            // configured profile list verbatim (it may be intentionally
            // non-Cartesian) instead of reconstructing it from bandwidths.
            if req.udp_bandwidths.is_empty()
                && req.udp_lengths.is_empty()
                && req.udp_windows.is_empty()
            {
                fallback.udp_profiles = cfg.iperf.udp_profiles.clone();
            }
            let recipes: Vec<&UiRecipe> = if selected.is_empty() {
                vec![&fallback]
            } else {
                selected
            };
            let src_pinned = req.nic_policies.iter().any(|policy| {
                policy.endpoint == pair.src && !policy.udp_bandwidth.trim().is_empty()
            });
            let dst_pinned = req.nic_policies.iter().any(|policy| {
                policy.endpoint == pair.dst && !policy.udp_bandwidth.trim().is_empty()
            });
            // A pinned sending leg does not depend on the recipe bandwidth.
            // Collapse such profiles by their remaining dimensions so a scan
            // over 1G/2G/3G does not run the exact same pinned command three
            // times.  Keep stream count in the key because it is an actual
            // execution dimension even when `-b` is overridden.
            let mut pinned_profiles_seen: HashSet<String> = HashSet::new();
            for recipe in recipes {
                for profile in recipe_udp_profiles(recipe, &fallback_bandwidths, req.udp_streams) {
                    let pinned_direction = |direction: &String| match direction.as_str() {
                        "ab" => src_pinned,
                        "ba" => dst_pinned,
                        "bidir" => src_pinned && dst_pinned,
                        _ => false,
                    };
                    let (pinned, swept): (Vec<String>, Vec<String>) =
                        directions.iter().cloned().partition(pinned_direction);
                    let suffix = format!(
                        "{}/{}/{}/{}/{}/{}",
                        ui_name_segment(link_set_id),
                        ui_name_segment(binding_id),
                        ui_name_segment(&pair.id),
                        ui_name_segment(&suite.id),
                        ui_name_segment(&task.id),
                        ui_name_segment(&profile.recipe_id)
                    );
                    if !pinned.is_empty() {
                        let pinned_key = format!(
                            "{:?}|{:?}|{}",
                            profile.profile.length, profile.profile.window, profile.streams
                        );
                        if !pinned_profiles_seen.insert(pinned_key) {
                            // The same pinned profile was already emitted for
                            // this task/recipe.  Swept directions still need
                            // every profile and are handled below.
                        } else {
                            let mut spec = ui_task_base_spec(
                                format!("ui-plan/{suffix}/udp-pinned"),
                                pair,
                                task,
                                "udp",
                                &pinned,
                                &ips,
                                req.duration,
                            );
                            let placeholder = req
                                .nic_policies
                                .iter()
                                .find(|policy| {
                                    (policy.endpoint == pair.src || policy.endpoint == pair.dst)
                                        && !policy.udp_bandwidth.trim().is_empty()
                                })
                                .map(|policy| policy.udp_bandwidth.trim().to_string())
                                .unwrap_or_else(|| profile.profile.bandwidth.clone());
                            let mut pinned_profile = profile.profile.clone();
                            pinned_profile.bandwidth = placeholder;
                            spec.udp_streams = Some(profile.streams);
                            spec.udp_profiles = Some(vec![pinned_profile]);
                            out.push(spec);
                        }
                    }
                    if !swept.is_empty() {
                        let mut spec = ui_task_base_spec(
                            format!("ui-plan/{suffix}/udp"),
                            pair,
                            task,
                            "udp",
                            &swept,
                            &ips,
                            req.duration,
                        );
                        spec.udp_streams = Some(profile.streams);
                        spec.udp_profiles = Some(vec![profile.profile.clone()]);
                        out.push(spec);
                    }
                }
            }
        }
        "ping" => {
            let selected: Vec<String> = if task.recipe_ids.is_empty() {
                vec!["default".into()]
            } else {
                task.recipe_ids.clone()
            };
            for recipe_id in selected {
                let suffix = format!(
                    "{}/{}/{}/{}/{}/{}",
                    ui_name_segment(link_set_id),
                    ui_name_segment(binding_id),
                    ui_name_segment(&pair.id),
                    ui_name_segment(&suite.id),
                    ui_name_segment(&task.id),
                    ui_name_segment(&recipe_id)
                );
                out.push(ui_task_base_spec(
                    format!("ui-plan/{suffix}/ping"),
                    pair,
                    task,
                    "ping",
                    &directions,
                    &ips,
                    req.duration,
                ));
            }
        }
        _ => {}
    }
    out
}

pub(super) fn config_from_ui_plan(state: &UiState, req: &RunRequest, plan: &UiPlan) -> Config {
    let mut cfg = ui_request_base_config(state, req);
    let mut bindings: Vec<(usize, &UiBinding)> = plan.bindings.iter().enumerate().collect();
    bindings.sort_by_key(|(index, binding)| (binding.order, *index));
    let mut tests = Vec::new();
    for (_, binding) in bindings {
        let Some(set) = plan
            .link_sets
            .iter()
            .find(|set| set.id == binding.link_set_id)
        else {
            continue;
        };
        let Some(suite) = plan
            .suites
            .iter()
            .find(|suite| suite.id == binding.suite_id)
        else {
            continue;
        };
        let pairs: Vec<&UiPairRef> = if binding.pair_ids.is_empty() {
            set.pair_refs.iter().collect()
        } else {
            binding
                .pair_ids
                .iter()
                .filter_map(|id| set.pair_refs.iter().find(|pair| pair.id == *id))
                .collect()
        };
        let mut tasks: Vec<&UiTask> = Vec::new();
        if suite.order.is_empty() {
            tasks.extend(suite.tasks.iter());
        } else {
            for task_id in &suite.order {
                if let Some(task) = suite.tasks.iter().find(|task| task.id == *task_id) {
                    tasks.push(task);
                }
            }
            // Validation permits a partial order for forward compatibility;
            // append unmentioned tasks in declaration order.
            for task in &suite.tasks {
                if !suite.order.iter().any(|id| id == &task.id) {
                    tasks.push(task);
                }
            }
        }
        for pair in pairs {
            for task in &tasks {
                tests.extend(ui_specs_for_task(
                    pair,
                    suite,
                    task,
                    &plan.recipes,
                    req,
                    &cfg,
                    &binding.id,
                    &set.id,
                ));
            }
        }
    }
    cfg.tests = tests;
    cfg
}

/// 这一行要跑哪几组：去重保序，空列表按「只跑默认组」解读。
///
/// 去重是必须的：同一组选两次会生成两批同名单元，resume 里互相覆盖，
/// 后写的那条赢——于是可能跳过一个其实 FAIL 了的单元。
pub(super) fn selected_udp_groups(pair: &PairSelection) -> Vec<usize> {
    if pair.udp_groups.is_empty() {
        return vec![0];
    }
    let mut seen = HashSet::new();
    pair.udp_groups
        .iter()
        .copied()
        .filter(|index| seen.insert(*index))
        .collect()
}

/// TCP 版的同一件事：去重保序，空列表按「只跑默认组」解读。
pub(super) fn selected_tcp_groups(pair: &PairSelection) -> Vec<usize> {
    if pair.tcp_groups.is_empty() {
        return vec![0];
    }
    let mut seen = HashSet::new();
    pair.tcp_groups
        .iter()
        .copied()
        .filter(|index| seen.insert(*index))
        .collect()
}

/// 矩阵里的一行 -> 若干条 TestSpec。
///
/// 一行会被拆开是因为配置模型里 `tcp_streams` 是标量、ping 挂在 `kinds` 上、
/// 而 UDP 的「被网口钉死的方向」和「还要扫档位的方向」用的是两份不同的档位。
pub(super) fn specs_for_pair(
    idx: usize,
    pair: &PairSelection,
    req: &RunRequest,
    sweeps: &Sweeps,
) -> Vec<TestSpec> {
    let mut tests: Vec<TestSpec> = Vec::new();
    let directions = pair.directions.clone();
    let ip = pair.ip.clone();
    let wants = |t: &str| pair.transports.iter().any(|x| x == t);
    let (want_tcp, want_udp) = (wants("tcp"), wants("udp"));
    // ping 在配置模型里是 `kinds` 而不是 `transports`——界面把它和 TCP/UDP
    // 并排放在「协议」列只是给人看的，落到 config 上必须分开：ping 单元
    // 不带 transport，走 builder 里那条独立分支。
    let want_ping = wants("ping");

    // 双向门限只有勾了「双向」才有意义；没勾时不写进 config，
    // 免得它出现在下载下来的 config.json 里让人以为在生效。
    let bidir_targets = directions
        .iter()
        .any(|d| d == "bidir")
        .then(|| crate::config::RateTargets {
            forward: None,
            ab: parse_rx_target(&pair.rx_target_bidir_ab)
                .ok()
                .flatten()
                .and_then(rx_target_mbps),
            ba: parse_rx_target(&pair.rx_target_bidir_ba)
                .ok()
                .flatten()
                .and_then(rx_target_mbps),
        })
        .filter(|targets| targets.ab.is_some() || targets.ba.is_some());

    let base = |name: String, transports: Vec<String>| TestSpec {
        name,
        rate_targets_bidir_mbps: bidir_targets.clone(),
        src: pair.src.clone(),
        dst: pair.dst.clone(),
        direction: OneOrMany::Many(directions.clone()),
        kinds: vec!["iperf".into()],
        transports,
        ip: ip.clone(),
        streams: 1,
        tcp_streams: None,
        // UDP 流数只写在 UDP 单元上。写在 TCP/ping 单元上既没有意义，又会让
        // 回填时分不清「默认组的流数」是哪一个（那边是按 tests[] 反推的）。
        udp_streams: None,
        iperf_duration: Some(sweeps.duration),
        ping_count: None,
        ping_payload_sizes: None,
        tcp_windows: None,
        udp_profiles: None,
        rate_mode: None,
        rate_targets_mbps: None,
    };

    // TCP 每个 -P 档位独立成一份 TestSpec：`tcp_streams` 在配置模型里是标量，
    // 而 -w 本来就是数组，由 builder 自己展开。TCP/UDP 也必须拆开，否则
    // 「3 个 -P 档位」会把与 -P 无关的 UDP 单元复制三遍。
    // 选中的每一组各生成一批 TCP 单元（`-w × -P`）。同一行选两组 = 这一对
    // 跑两遍，参数各按各的组来——和 UDP 的多组展开一模一样。
    if want_tcp {
        for group_index in selected_tcp_groups(pair) {
            let tcp = sweeps.tcp_group(group_index);
            // 默认组沿用原来的单元名（`ui-N-tcp-P{P}`），改名会改掉 resume id
            // ——虽然 TCP 的 resume id 只认 profile（-w/-P），不认 spec.name，
            // 这里保持一致仍是对的。别的组各带一个后缀。
            let suffix = if group_index == 0 {
                String::new()
            } else {
                format!("-g{}", group_index + 1)
            };
            for streams in &tcp.stream_steps {
                let mut spec = base(
                    format!("ui-{}-tcp{suffix}-P{streams}", idx + 1),
                    vec!["tcp".into()],
                );
                spec.tcp_streams = Some(*streams);
                // 空列表原样传给 builder：它把「没有 -w 档位」跑成一条不带 -w
                // 的 TCP。默认组经过 non_empty 兜底不会走到这一支。
                spec.tcp_windows = Some(tcp.windows.clone());
                tests.push(spec);
            }
        }
    }
    // 选中的每一组各生成一批 UDP 单元。同一行选两组 = 这一对跑两遍，
    // 参数各按各的组来。
    for group_index in selected_udp_groups(pair) {
        if !want_udp {
            break;
        }
        let udp = sweeps.udp_group(group_index);
        let udp_streams = udp.streams;
        // 第 0 组沿用原来的单元名：改名会改掉 resume id，让历史 PASS 全部失效。
        // 别的组各带一个后缀，否则同一对的两批单元同名、resume 里互相覆盖。
        let suffix = if group_index == 0 {
            String::new()
        } else {
            format!("-g{}", group_index + 1)
        };
        let src_pinned = sweeps.pinned_senders.contains(&pair.src);
        let dst_pinned = sweeps.pinned_senders.contains(&pair.dst);
        // 一个方向的每条发送腿都有按网口覆盖时，全局 -b 档位对它不起作用：
        // builder 会把每一档都替换回那个覆盖值，扫 N 档就得到 N 个完全相同
        // 的单元。必须**逐方向**判断而不是整对判断——「ab 被发送端钉死、
        // 反向 ba 仍要扫档位」是最常见的组合，按整对判断时那三个 ab 单元
        // 会一模一样地各跑一遍全程。
        let pinned_direction = |d: &String| match d.as_str() {
            "ab" => src_pinned,
            "ba" => dst_pinned,
            "bidir" => src_pinned && dst_pinned,
            _ => false,
        };
        let (pinned, swept): (Vec<String>, Vec<String>) =
            directions.iter().cloned().partition(pinned_direction);

        if !pinned.is_empty() {
            // 占位值：builder 会按腿替换成各自的精确覆盖值，这里填什么都行，
            // 取一个真实值只是为了万一覆盖项被后续校验剔除时不至于离谱。
            let placeholder = req
                .nic_policies
                .iter()
                .find(|policy| {
                    (policy.endpoint == pair.src || policy.endpoint == pair.dst)
                        && !policy.udp_bandwidth.trim().is_empty()
                })
                .map(|policy| policy.udp_bandwidth.trim())
                .unwrap_or("1m");
            let mut spec = base(
                format!("ui-{}-udp{suffix}-pinned", idx + 1),
                vec!["udp".into()],
            );
            spec.direction = OneOrMany::Many(pinned);
            spec.udp_streams = Some(udp_streams);
            // -b 被网口钉死，但 -l 档位仍要逐档跑：钉住的是带宽，不是报文长度。
            spec.udp_profiles = Some(udp_profiles_for(placeholder, &udp.lengths, &udp.windows));
            tests.push(spec);
        }
        if !swept.is_empty() {
            // 还有腿没被覆盖的方向照常逐档扫描；已覆盖的那条腿在每个单元里
            // 保持固定值（双向单元里一钉一扫就是这种情况）。
            let mut spec = base(format!("ui-{}-udp{suffix}", idx + 1), vec!["udp".into()]);
            spec.direction = OneOrMany::Many(swept);
            spec.udp_streams = Some(udp_streams);
            spec.udp_profiles = Some(udp.profiles());
            tests.push(spec);
        }
    }
    if want_ping {
        // 每个包长档位在 builder 里各成一个单元，所以这里必须让界面把
        // 次数和包长填全：不填就回落到 ping.count=100 × 三档包长，
        // 每个配对每个方向平白多出三个各一百多秒的单元，而这件事要到
        // 「预览任务」才看得见，太晚了。
        let mut spec = base(format!("ui-{}-ping", idx + 1), Vec::new());
        spec.kinds = vec!["ping".into()];
        spec.ping_count = (req.ping_count > 0).then_some(req.ping_count);
        if !sweeps.ping_sizes.is_empty() {
            spec.ping_payload_sizes = Some(sweeps.ping_sizes.clone());
        }
        tests.push(spec);
    }
    tests
}

/// 去空白、丢空项。手抄进来的参数列表和网段前缀共用这一份清洗。
///
/// 只清洗，不替换成默认值：清洗后剩下空列表在两处都是有意义的选择
/// （前缀清空 = 列出全部网口，`-l` 清空 = 不下发 `-l`）。
pub(super) fn cleaned_list(raw: &[String]) -> Vec<String> {
    raw.iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// 一个 `-b` 档位 × 全部 `-l` 档位 × 全部 `-w` 档位。
///
/// 某一项留空就在那一维退化成一档、且**完全不下发该参数**——不能拿 iperf3 的
/// 默认值写死进命令，那会把「没指定」变成「指定了某个具体值」，两者在报告里
/// 读起来完全不同。
pub(super) fn udp_profiles_for(
    bandwidth: &str,
    lengths: &[String],
    windows: &[String],
) -> Vec<UdpProfile> {
    let one_none = [None];
    let lengths: Vec<Option<String>> = if lengths.is_empty() {
        one_none.to_vec()
    } else {
        lengths.iter().cloned().map(Some).collect()
    };
    let windows: Vec<Option<String>> = if windows.is_empty() {
        one_none.to_vec()
    } else {
        windows.iter().cloned().map(Some).collect()
    };
    let mut out = Vec::with_capacity(lengths.len() * windows.len());
    for length in &lengths {
        for window in &windows {
            out.push(UdpProfile {
                bandwidth: bandwidth.to_string(),
                length: length.clone(),
                window: window.clone(),
            });
        }
    }
    out
}

/// 保序去重。配置文件里同一个 `-l` / `-w` 常在多个档位上重复出现，
/// 回填到界面时得压成一份，否则一打开页面档位就自己翻倍。
pub(super) fn distinct(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values.filter(|value| seen.insert(value.clone())).collect()
}

/// 界面没填就退回配置文件里的既有值，不要用空列表把它清掉。
pub(super) fn non_empty(picked: &[String], fallback: &[String]) -> Vec<String> {
    let cleaned: Vec<String> = picked
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    if cleaned.is_empty() {
        fallback.to_vec()
    } else {
        cleaned
    }
}

/// 配对门限只收绝对 Mbps。
///
/// 百分比要拿接收端网卡的协商速率来换算，而这个值每个单元开跑前才重扫；
/// 配对门限是「这两块口凑在一起、并发时的能力」，跟单独一块口的协商速率
/// 不成比例——`WIFI5G 2882Mbps × 50%` 和「和 RNDIS 组双向时能收到多少」
/// 没有关系。收百分比只会给出一个看着有依据、其实是瞎算的数。
pub(super) fn rx_target_mbps(target: RxTarget) -> Option<f64> {
    match target {
        RxTarget::Mbps(value) => Some(value),
        RxTarget::Percent(_) => None,
    }
}

/// `master:NAME=以太网 6` -> 一条 by_nic 覆盖。三项全空就不生成覆盖项。
pub(super) fn nic_profile(policy: &NicPolicySelection) -> Option<crate::config::NicProfile> {
    let target = parse_rx_target(&policy.rx_target).ok().flatten();
    let bandwidth = policy.udp_bandwidth.trim();
    let length = policy.udp_length.trim();
    if target.is_none() && bandwidth.is_empty() && length.is_empty() {
        return None;
    }
    let (host, rest) = policy.endpoint.split_once(':')?;
    let name = rest.strip_prefix("NAME=")?;
    let mbps = |value: Option<RxTarget>| match value {
        Some(RxTarget::Mbps(value)) => Some(value),
        _ => None,
    };
    let percent = |value: Option<RxTarget>| match value {
        Some(RxTarget::Percent(value)) => Some(value),
        _ => None,
    };
    Some(crate::config::NicProfile {
        host: host.to_string(),
        name: name.to_string(),
        ipv4: String::new(),
        rx_target_mbps: mbps(target),
        rx_target_percent: percent(target),
        udp_bandwidth: (!bandwidth.is_empty()).then(|| bandwidth.to_string()),
        udp_length: (!length.is_empty()).then(|| length.to_string()),
    })
}

/// 一个单元里每条腿最终下发的参数，一行一条腿。
///
/// 直接读 `IperfTask.extra`——那就是要交给 iperf3 的东西，不是这里再算一遍。
/// 再算一遍就会有第二份口径，两份迟早对不上，而这行字存在的意义正是「所见即
/// 所跑」。
pub(super) fn unit_load_lines(unit: &builder::Unit) -> Vec<String> {
    unit.legs
        .iter()
        .filter_map(|leg| {
            let (task, streams) = match &leg.kind {
                builder::LegKind::IperfSingle(task) => (task, 1),
                builder::LegKind::IperfGroup { streams, .. } => (streams.first()?, streams.len()),
                // ctsTraffic 和 ping 的参数不在这套 -b/-l/-w 里，标题已经说清了。
                _ => return None,
            };
            let mut text = String::new();
            if !leg.tag.is_empty() {
                text.push_str(match leg.tag.as_str() {
                    "ab" => "A→B ",
                    "ba" => "B→A ",
                    other => other,
                });
            }
            text.push_str(&readable_args(&task.extra));
            // iperf3 的 `-P` 由它自己开流，UDP 这边是我们逐流起进程，
            // 两种「流数」在命令里长得不一样，所以只给后者补一句。
            if task.udp && streams > 1 {
                text.push_str(&format!(" ×{streams} 流"));
            }
            Some(text)
        })
        .collect()
}

/// 命令参数照抄，只把 `-b` 那个数换成 Mbps 写法。
///
/// 下发的 `-b` 是精确的 bit/s 整数（`UdpLoad::iperf_arg`，为的是不依赖 iperf3
/// 对 `Gbps` 这类长后缀的非文档行为）。原样打印出来是 `-b 1000000000`——十个零
/// 要一个个数，而这一行存在的意义是"跟你填的那个数对得上"。换算成 Mbps 是同一个
/// 数字换个写法，不是重算，所以"所见即所跑"没有被破坏。
///
/// 顺带避免一个真实的坑：把 `1000000000` 抄回 `-b` 输入框，那里的裸数字按 **Mbps**
/// 算（见 `UdpProfile::parsed_bandwidth`），于是变成 10^9 Mbps。
pub(super) fn readable_args(extra: &[String]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(extra.len());
    let mut iter = extra.iter().peekable();
    while let Some(arg) = iter.next() {
        out.push(arg.clone());
        if arg != "-b" {
            continue;
        }
        let Some(value) = iter.peek() else { continue };
        let Ok(bits) = value.parse::<u64>() else {
            continue;
        };
        iter.next();
        let mbps = bits as f64 / 1_000_000.0;
        out.push(if (mbps.fract()).abs() < f64::EPSILON {
            format!("{mbps:.0} Mbps")
        } else {
            format!("{mbps:.1} Mbps")
        });
    }
    out.join(" ")
}

/// Encode an arbitrary user/project ID before embedding it in the internal
/// slash-delimited TestSpec name.  UI IDs are normally generated as hex, but
/// the HTTP API and imported project files are allowed to carry human IDs such
/// as `wifi/a`; letting those raw slashes through shifts every following trace
/// field and makes the preview point at the wrong suite/task.  Percent-escape
/// every byte outside the URI unreserved set so the transform is reversible
/// for UTF-8 as well as punctuation.
pub(super) fn ui_name_segment(raw: &str) -> String {
    urlencode(raw)
}

pub(super) fn ui_name_segment_decode(raw: &str) -> String {
    urldecode(raw)
}

pub(super) fn topology_fingerprint(state: &UiState) -> String {
    crate::master::plan::topology_fingerprint(&state.master, &state.agent)
}

pub(super) fn ui_source_from_test_name(name: &str) -> Option<UiSource> {
    let mut parts = name.split('/');
    if parts.next()? != "ui-plan" {
        return None;
    }
    let link_set_id = ui_name_segment_decode(parts.next()?);
    let _binding_id = ui_name_segment_decode(parts.next()?);
    Some(UiSource {
        pair_id: ui_name_segment_decode(parts.next()?),
        link_set_id,
        suite_id: ui_name_segment_decode(parts.next()?),
        task_id: ui_name_segment_decode(parts.next()?),
        recipe_id: ui_name_segment_decode(parts.next()?),
        protocol: parts.next()?.split('-').next()?.to_string(),
    })
}

pub(super) fn unit_protocol(unit: &builder::Unit) -> Option<String> {
    unit.legs.first().map(|leg| match &leg.kind {
        builder::LegKind::IperfSingle(task) => {
            if task.udp {
                "udp".to_string()
            } else {
                "tcp".to_string()
            }
        }
        builder::LegKind::IperfGroup { streams, .. } => {
            if streams.first().is_some_and(|task| task.udp) {
                "udp".to_string()
            } else {
                "tcp".to_string()
            }
        }
        builder::LegKind::CtsTraffic(task) => {
            if task.udp {
                "udp".to_string()
            } else {
                "tcp".to_string()
            }
        }
        builder::LegKind::Ping(_) => "ping".to_string(),
    })
}

pub(super) fn unit_effective_args(unit: &builder::Unit) -> Vec<String> {
    unit.legs
        .iter()
        .flat_map(|leg| match &leg.kind {
            builder::LegKind::IperfSingle(task) => task.extra.clone(),
            builder::LegKind::IperfGroup { streams, .. } => streams
                .first()
                .map(|task| task.extra.clone())
                .unwrap_or_default(),
            _ => Vec::new(),
        })
        .collect()
}

/// Return the concrete endpoints carried by a unit's first leg.
///
/// `builder::Leg::tag` is intentionally empty for a one-way leg (the tag is
/// reserved for the two legs inside a bidirectional unit), so it cannot be
/// used as the direction source for the quick-plan trace.  Looking at the
/// resolved endpoints keeps the trace correct for both A→B and B→A without
/// changing the executor/reporting semantics of `Leg::tag`.
pub(super) fn leg_endpoints(
    leg: &builder::Leg,
) -> Option<(&builder::Endpoint, &builder::Endpoint)> {
    match &leg.kind {
        builder::LegKind::IperfSingle(task) => Some((&task.src, &task.dst)),
        builder::LegKind::IperfGroup { streams, .. } => {
            streams.first().map(|task| (&task.src, &task.dst))
        }
        builder::LegKind::CtsTraffic(task) => Some((&task.src, &task.dst)),
        builder::LegKind::Ping(task) => Some((&task.src, &task.dst)),
    }
}

/// Resolve the direction represented by a built unit relative to its source
/// `TestSpec`.  Bidirectional units are one concurrent unit with two legs;
/// one-way units have an empty leg tag, so compare endpoint keys instead.
pub(super) fn unit_direction_for_spec(
    unit: &builder::Unit,
    spec: &builder::SpecNorm,
) -> Option<String> {
    if unit.bidir {
        return Some("bidir".into());
    }
    let (src, dst) = leg_endpoints(unit.legs.first()?)?;
    if src.key() == spec.src.key() && dst.key() == spec.dst.key() {
        Some("ab".into())
    } else if src.key() == spec.dst.key() && dst.key() == spec.src.key() {
        Some("ba".into())
    } else {
        None
    }
}

pub(super) fn compile_request(state: &UiState, req: &RunRequest) -> Result<CompiledPlan, String> {
    validate_request(state, req)?;
    let cfg = config_from_request(state, req);
    let problems = cfg.validate();
    if !problems.is_empty() {
        return Err(format!("配置项异常：{}", problems.join("；")));
    }
    let mut notices = Vec::new();
    let mut spec_errors = Vec::new();
    let mut units = Vec::new();
    let mut sources: Vec<Option<UiSource>> = Vec::new();
    let mut source_directions: Vec<Option<String>> = Vec::new();
    let mut port = builder::PORT_BASE;

    if req.ui_plan.is_some() {
        // Build each spec separately so every generated unit can be traced back
        // to its suite task.  Port allocation remains global and deterministic.
        for test in &cfg.tests {
            match builder::spec_from_config(test, &cfg, &state.master, &state.agent) {
                Ok(spec) => {
                    let (mut built, build_notices) = build_units(
                        std::slice::from_ref(&spec),
                        cfg.require_same_subnet_for_iperf,
                        &mut port,
                    );
                    notices.extend(build_notices);
                    let source = ui_source_from_test_name(&test.name);
                    // `Leg::tag` is intentionally empty for one-way units, so
                    // retain the concrete A→B/B→A direction while the named
                    // source spec is still available.  Bidirectional units
                    // are represented by a single unit and remain `bidir`.
                    for unit in &built {
                        sources.push(source.clone());
                        source_directions.push(unit_direction_for_spec(unit, &spec));
                    }
                    units.append(&mut built);
                }
                Err(error) => {
                    spec_errors.push(format!("{} 无法生成任务：{error}", test.name));
                    notices.push(format!("跳过 {}: {error}", test.name));
                }
            }
        }
    } else {
        let mut specs = Vec::new();
        for test in &cfg.tests {
            match builder::spec_from_config(test, &cfg, &state.master, &state.agent) {
                Ok(spec) => specs.push(spec),
                Err(error) => {
                    spec_errors.push(format!("{} 无法生成任务：{error}", test.name));
                    notices.push(format!("跳过 {}: {error}", test.name));
                }
            }
        }
        let (built, build_notices) =
            build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
        notices.extend(build_notices);
        units = built;
        sources.resize(units.len(), None);
        source_directions.resize(units.len(), None);
    }

    if req.ui_plan.is_some() {
        // Stable builder IDs include the effective protocol/profile/endpoint
        // shape.  If two bindings accidentally describe that same shape, keep
        // one execution unit and make the reduction visible to the caller.
        let mut seen_ids = HashSet::new();
        let mut unique_units = Vec::with_capacity(units.len());
        let mut unique_sources = Vec::with_capacity(sources.len());
        let mut unique_directions = Vec::with_capacity(source_directions.len());
        for (index, unit) in units.into_iter().enumerate() {
            if seen_ids.insert(unit.id.clone()) {
                unique_units.push(unit);
                unique_sources.push(sources.get(index).cloned().flatten());
                unique_directions.push(source_directions.get(index).cloned().flatten());
            }
        }
        let removed_count = sources.len().saturating_sub(unique_units.len());
        if removed_count > 0 {
            notices.push(format!(
                "计划去重：移除了 {removed_count} 个最终参数完全相同的重复单元"
            ));
        }
        units = unique_units;
        sources = unique_sources;
        source_directions = unique_directions;
    }

    let resumed = if cfg.resume {
        let db = ResultDb::load(std::path::PathBuf::from("task_results.json"));
        units
            .iter()
            .map(|unit| db.fresh_pass(&unit.id).is_some())
            .collect()
    } else {
        vec![false; units.len()]
    };
    // 哈希必须算在**执行端真正会推导出来的那批单元**上。
    //
    // 这里展示用的 `units` 是逐 spec 单独构建的（为了把每个单元追溯回它的
    // 套件任务），而 `run_master` 是把所有 spec 一次性交给 `build_units`。
    // 两条路径本该等价——`canonical_plan_units` 就是按执行端的方式再走一遍，
    // 拿它算哈希，闸门两边比的才是同一件东西。等价性由
    // `the_preview_and_execution_paths_build_the_same_units` 守着。
    let topology_fingerprint = topology_fingerprint(state);
    let execution_plan = ExecutionPlan::new(
        &cfg,
        topology_fingerprint.clone(),
        canonical_plan_units(&cfg, state),
        Vec::new(),
    );
    let plan_hash = execution_plan.plan_hash.clone();
    let mut trace = Vec::with_capacity(units.len());
    let mut sections = Vec::new();
    for (index, unit) in units.iter().enumerate() {
        let source = sources.get(index).and_then(|source| source.clone());
        let (pair_id, link_set_id, suite_id, task_id, recipe_id) = source
            .as_ref()
            .map(|source| {
                (
                    Some(source.pair_id.clone()),
                    (!source.link_set_id.is_empty()).then(|| source.link_set_id.clone()),
                    Some(source.suite_id.clone()),
                    Some(source.task_id.clone()),
                    Some(source.recipe_id.clone()),
                )
            })
            .unwrap_or((None, None, None, None, None));
        let protocol = source
            .as_ref()
            .map(|source| source.protocol.clone())
            .or_else(|| unit_protocol(unit));
        let direction = source_directions.get(index).cloned().flatten().or_else(|| {
            (!unit.legs.is_empty()).then(|| {
                unit.legs
                    .iter()
                    .map(|leg| {
                        if leg.tag.is_empty() {
                            "ab"
                        } else {
                            leg.tag.as_str()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
        });
        let ip = if unit.title.contains(" V6 ") {
            Some("v6".into())
        } else if unit.title.contains(" V4 ") {
            Some("v4".into())
        } else {
            None
        };
        let effective_args = unit_effective_args(unit);
        trace.push(PlanTrace {
            seq: index + 1,
            pair_id: pair_id.clone(),
            link_set_id: link_set_id.clone(),
            suite_id: suite_id.clone(),
            task_id: task_id.clone(),
            lane_id: task_id.clone(),
            recipe_id: recipe_id.clone(),
            protocol: protocol.clone(),
            direction,
            ip,
            requested_args: effective_args.clone(),
            effective_args,
            value_sources: if req.ui_plan.is_some() {
                vec!["suite recipe（网口策略/链路裁剪由 builder 最终决定）".into()]
            } else {
                vec!["legacy matrix".into()]
            },
            skipped_reason: None,
            resumed: resumed[index],
        });
        let key = (link_set_id.clone(), suite_id.clone(), task_id.clone());
        if let Some(section) = sections.iter_mut().find(|section: &&mut PlanSection| {
            (
                section.link_set_id.clone(),
                section.suite_id.clone(),
                section.task_id.clone(),
            ) == key
        }) {
            section.unit_seqs.push(index + 1);
        } else {
            sections.push(PlanSection {
                link_set_id,
                suite_id,
                task_id,
                title: unit.title.clone(),
                unit_seqs: vec![index + 1],
            });
        }
    }
    if req.ui_plan.is_none() {
        // Keep the legacy response compact and backwards-compatible; hierarchy
        // is only meaningful for the suite planner.
        trace.clear();
        sections.clear();
    }
    Ok(CompiledPlan {
        cfg,
        units,
        notices,
        resumed,
        trace,
        sections,
        plan_hash,
        topology_fingerprint,
        spec_errors,
    })
}

#[allow(dead_code)]
pub(super) fn ensure_config_builds_units(cfg: &Config, state: &UiState) -> Result<(), String> {
    let mut specs = Vec::new();
    for test in &cfg.tests {
        let spec = builder::spec_from_config(test, cfg, &state.master, &state.agent)
            .map_err(|error| format!("{} 无法生成任务：{error}", test.name))?;
        specs.push(spec);
    }

    let mut port = builder::PORT_BASE;
    let (units, notices) = build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port);
    if units.is_empty() {
        let detail = if notices.is_empty() {
            String::new()
        } else {
            format!("：{}", notices.join("；"))
        };
        return Err(format!("所选配置最终没有生成任何测试单元{detail}"));
    }
    Ok(())
}

/// 按**执行端的方式**推导单元：所有 spec 一次性交给 `build_units`。
///
/// 与 `run_master` 里那段保持一字不差的等价，是计划闸门能成立的前提。
pub(super) fn canonical_plan_units(cfg: &Config, state: &UiState) -> Vec<builder::Unit> {
    let mut specs = Vec::new();
    for test in &cfg.tests {
        if let Ok(spec) = builder::spec_from_config(test, cfg, &state.master, &state.agent) {
            specs.push(spec);
        }
    }
    let mut port = builder::PORT_BASE;
    build_units(&specs, cfg.require_same_subnet_for_iperf, &mut port).0
}
