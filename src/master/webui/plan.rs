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

pub(super) struct Sweeps {
    pub(super) tcp_groups: Vec<ResolvedTcpGroup>,
    pub(super) udp_groups: Vec<ResolvedUdpGroup>,
    pub(super) ping_sizes: Vec<u32>,
    pub(super) duration: u64,
    pub(super) pinned_senders: HashSet<String>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ResolvedUdpGroup {
    pub(super) bandwidths: Vec<String>,
    pub(super) lengths: Vec<String>,
    pub(super) windows: Vec<String>,
    pub(super) streams: u32,
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

#[derive(Debug, Clone, Default)]
pub(super) struct ResolvedTcpGroup {
    pub(super) windows: Vec<String>,
    pub(super) stream_steps: Vec<u32>,
}

impl Sweeps {
    pub(super) fn udp_group(&self, index: usize) -> &ResolvedUdpGroup {
        self.udp_groups.get(index).unwrap_or(&self.udp_groups[0])
    }
    pub(super) fn tcp_group(&self, index: usize) -> &ResolvedTcpGroup {
        self.tcp_groups.get(index).unwrap_or(&self.tcp_groups[0])
    }
}

fn apply_ping_policy_overrides(cfg: &mut crate::config::PingCfg, req: &RunRequest) {
    if req.ping_small_max_bytes > 0 {
        cfg.small_max_bytes = req.ping_small_max_bytes;
    }
    if req.ping_medium_max_bytes > 0 {
        cfg.medium_max_bytes = req.ping_medium_max_bytes;
    }
    if req.ping_wired_small_avg_rtt_ms > 0.0 {
        cfg.wired_small_avg_rtt_ms = req.ping_wired_small_avg_rtt_ms;
    }
    let wired_small_max = if req.ping_wired_small_max_rtt_ms > 0.0 {
        req.ping_wired_small_max_rtt_ms
    } else {
        req.ping_max_rtt_ms
    };
    if wired_small_max > 0.0 {
        cfg.max_rtt_ms = wired_small_max;
    }
    if req.ping_wired_medium_avg_rtt_ms > 0.0 {
        cfg.wired_medium_avg_rtt_ms = req.ping_wired_medium_avg_rtt_ms;
    }
    if req.ping_wired_medium_max_rtt_ms > 0.0 {
        cfg.wired_medium_max_rtt_ms = req.ping_wired_medium_max_rtt_ms;
    }
    if req.ping_wired_large_avg_rtt_ms > 0.0 {
        cfg.wired_large_avg_rtt_ms = req.ping_wired_large_avg_rtt_ms;
    }
    if req.ping_wired_large_max_rtt_ms > 0.0 {
        cfg.wired_large_max_rtt_ms = req.ping_wired_large_max_rtt_ms;
    }
    if req.ping_wifi_small_avg_rtt_ms > 0.0 {
        cfg.wifi_small_avg_rtt_ms = req.ping_wifi_small_avg_rtt_ms;
    }
    if req.ping_wifi_small_max_rtt_ms > 0.0 {
        cfg.wifi_small_max_rtt_ms = req.ping_wifi_small_max_rtt_ms;
    }
    if req.ping_wifi_medium_avg_rtt_ms > 0.0 {
        cfg.wifi_medium_avg_rtt_ms = req.ping_wifi_medium_avg_rtt_ms;
    }
    if req.ping_wifi_medium_max_rtt_ms > 0.0 {
        cfg.wifi_medium_max_rtt_ms = req.ping_wifi_medium_max_rtt_ms;
    }
    if req.ping_wifi_large_avg_rtt_ms > 0.0 {
        cfg.wifi_large_avg_rtt_ms = req.ping_wifi_large_avg_rtt_ms;
    }
    if req.ping_wifi_large_max_rtt_ms > 0.0 {
        cfg.wifi_large_max_rtt_ms = req.ping_wifi_large_max_rtt_ms;
    }
}

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
    apply_ping_policy_overrides(&mut cfg.ping, req);

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
    let mut seen_sizes = HashSet::new();
    let ping_sizes: Vec<u32> = req
        .ping_payload_sizes
        .iter()
        .copied()
        .filter(|size| *size > 0 && seen_sizes.insert(*size))
        .collect();
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

    let mut tcp_groups = vec![ResolvedTcpGroup {
        windows: windows.clone(),
        stream_steps: stream_steps.clone(),
    }];
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
    apply_ping_policy_overrides(&mut cfg.ping, req);

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

pub(super) fn ui_link_group(link_set: &UiLinkSet) -> Option<String> {
    let name = link_set.name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

pub(super) fn ui_origin_for(
    link_set: &UiLinkSet,
    binding_id: &str,
    pair: &UiPairRef,
    suite: &UiSuite,
    task: &UiTask,
    recipe_id: &str,
) -> UiOrigin {
    UiOrigin {
        pair_id: pair.id.clone(),
        link_set_id: link_set.id.clone(),
        link_set_name: link_set.name.clone(),
        binding_id: binding_id.to_string(),
        suite_id: suite.id.clone(),
        task_id: task.id.clone(),
        recipe_id: recipe_id.to_string(),
    }
}

pub(super) fn ui_display_name(suite: &UiSuite, task: &UiTask, label: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !suite.name.trim().is_empty() {
        parts.push(suite.name.trim());
    }
    if !task.name.trim().is_empty() {
        parts.push(task.name.trim());
    }
    if !label.is_empty() {
        parts.push(label);
    }
    if parts.is_empty() {
        return "ui-plan".into();
    }
    parts.join(" · ")
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
        link_group: None,
        origin: None,
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
    link_set: &UiLinkSet,
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
                    let mut spec = ui_task_base_spec(
                        ui_display_name(suite, task, &format!("TCP -P {}", profile.streams)),
                        pair,
                        task,
                        "tcp",
                        &directions,
                        &ips,
                        req.duration,
                    );
                    spec.origin = Some(ui_origin_for(
                        link_set,
                        binding_id,
                        pair,
                        suite,
                        task,
                        &profile.recipe_id,
                    ));
                    spec.link_group = ui_link_group(link_set);
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
                    let origin =
                        ui_origin_for(link_set, binding_id, pair, suite, task, &profile.recipe_id);
                    if !pinned.is_empty() {
                        let pinned_key = format!(
                            "{:?}|{:?}|{}",
                            profile.profile.length, profile.profile.window, profile.streams
                        );
                        if pinned_profiles_seen.insert(pinned_key) {
                            let mut spec = ui_task_base_spec(
                                ui_display_name(suite, task, "UDP（按网口策略钉死）"),
                                pair,
                                task,
                                "udp",
                                &pinned,
                                &ips,
                                req.duration,
                            );
                            spec.origin = Some(origin.clone());
                            spec.link_group = ui_link_group(link_set);
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
                            ui_display_name(suite, task, "UDP"),
                            pair,
                            task,
                            "udp",
                            &swept,
                            &ips,
                            req.duration,
                        );
                        spec.origin = Some(origin.clone());
                        spec.link_group = ui_link_group(link_set);
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
                let mut spec = ui_task_base_spec(
                    ui_display_name(suite, task, "PING"),
                    pair,
                    task,
                    "ping",
                    &directions,
                    &ips,
                    req.duration,
                );
                spec.origin = Some(ui_origin_for(
                    link_set, binding_id, pair, suite, task, &recipe_id,
                ));
                spec.link_group = ui_link_group(link_set);
                out.push(spec);
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
                    set,
                ));
            }
        }
    }
    cfg.tests = tests;
    cfg
}

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
    let want_ping = wants("ping");

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
        udp_streams: None,
        iperf_duration: Some(sweeps.duration),
        ping_count: None,
        ping_payload_sizes: None,
        tcp_windows: None,
        udp_profiles: None,
        rate_mode: None,
        rate_targets_mbps: None,
        link_group: None,
        origin: None,
    };

    if want_tcp {
        for group_index in selected_tcp_groups(pair) {
            let tcp = sweeps.tcp_group(group_index);
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
                spec.tcp_windows = Some(tcp.windows.clone());
                tests.push(spec);
            }
        }
    }
    for group_index in selected_udp_groups(pair) {
        if !want_udp {
            break;
        }
        let udp = sweeps.udp_group(group_index);
        let udp_streams = udp.streams;
        let suffix = if group_index == 0 {
            String::new()
        } else {
            format!("-g{}", group_index + 1)
        };
        let src_pinned = sweeps.pinned_senders.contains(&pair.src);
        let dst_pinned = sweeps.pinned_senders.contains(&pair.dst);
        let pinned_direction = |d: &String| match d.as_str() {
            "ab" => src_pinned,
            "ba" => dst_pinned,
            "bidir" => src_pinned && dst_pinned,
            _ => false,
        };
        let (pinned, swept): (Vec<String>, Vec<String>) =
            directions.iter().cloned().partition(pinned_direction);

        if !pinned.is_empty() {
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
            spec.udp_profiles = Some(udp_profiles_for(placeholder, &udp.lengths, &udp.windows));
            tests.push(spec);
        }
        if !swept.is_empty() {
            let mut spec = base(format!("ui-{}-udp{suffix}", idx + 1), vec!["udp".into()]);
            spec.direction = OneOrMany::Many(swept);
            spec.udp_streams = Some(udp_streams);
            spec.udp_profiles = Some(udp.profiles());
            tests.push(spec);
        }
    }
    if want_ping {
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

pub(super) fn cleaned_list(raw: &[String]) -> Vec<String> {
    raw.iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

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

pub(super) fn distinct(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values.filter(|value| seen.insert(value.clone())).collect()
}

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

pub(super) fn rx_target_mbps(target: RxTarget) -> Option<f64> {
    match target {
        RxTarget::Mbps(value) => Some(value),
        RxTarget::Percent(_) => None,
    }
}

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

pub(super) fn unit_load_lines(unit: &builder::Unit) -> Vec<String> {
    unit.legs
        .iter()
        .filter_map(|leg| {
            let (task, streams) = match &leg.kind {
                builder::LegKind::IperfSingle(task) => (task, 1),
                builder::LegKind::IperfGroup { streams, .. } => (streams.first()?, streams.len()),
                _ => return None,
            };
            let mut text = String::new();
            let direction = if leg.tag.is_empty() {
                unit.direction.as_str()
            } else {
                leg.tag.as_str()
            };
            match direction {
                "ab" => text.push_str("A→B "),
                "ba" => text.push_str("B→A "),
                "bidir" => text.push_str("双向 "),
                "" => {}
                other => {
                    text.push_str(other);
                    text.push(' ');
                }
            }
            text.push_str(&readable_args(&task.extra));
            if task.udp && streams > 1 {
                text.push_str(&format!(" ×{streams} 流"));
            }
            Some(text)
        })
        .collect()
}

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

pub(super) fn ui_name_segment_decode(raw: &str) -> String {
    urldecode(raw)
}

pub(super) fn topology_fingerprint(state: &UiState) -> String {
    crate::master::plan::topology_fingerprint(&state.master, &state.agent)
}

pub(super) fn ui_source_from_spec(test: &TestSpec) -> Option<UiSource> {
    if let Some(origin) = test.origin.as_ref().filter(|origin| !origin.is_empty()) {
        return Some(UiSource {
            pair_id: origin.pair_id.clone(),
            link_set_id: origin.link_set_id.clone(),
            suite_id: origin.suite_id.clone(),
            task_id: origin.task_id.clone(),
            recipe_id: origin.recipe_id.clone(),
            protocol: spec_protocol(test).unwrap_or_default(),
        });
    }
    ui_source_from_test_name(&test.name)
}

pub(super) fn spec_protocol(test: &TestSpec) -> Option<String> {
    if test
        .kinds
        .iter()
        .any(|kind| kind.eq_ignore_ascii_case("ping"))
    {
        return Some("ping".into());
    }
    test.transports
        .first()
        .map(|transport| transport.to_ascii_lowercase())
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
        for test in &cfg.tests {
            match builder::spec_from_config(test, &cfg, &state.master, &state.agent) {
                Ok(spec) => {
                    let (mut built, build_notices) = build_units(
                        std::slice::from_ref(&spec),
                        cfg.require_same_subnet_for_iperf,
                        &mut port,
                    );
                    notices.extend(build_notices);
                    let source = ui_source_from_spec(test);
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
            .filter(|protocol| !protocol.is_empty())
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
