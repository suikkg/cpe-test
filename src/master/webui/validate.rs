//! 请求校验：界面能填出来的东西，未必是能跑的东西。
//!
//! 这一层是给「不写脚本的人」兜底的地方。它的存在意义不是防御恶意输入，而是
//! **在开跑之前把话说清楚**：哪个字段填错了、合法范围是什么。一旦放行，后面
//! 编译出的执行计划就不再有第二次说不的机会。

use super::*;

/// RX 门限输入框的两种写法。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum RxTarget {
    Mbps(f64),
    /// 协商速率的百分比，`90.0` = 90%。
    Percent(f64),
}

/// 解析 `1800` / `1800.5` / `90%`。空串返回 `Ok(None)`。
pub(super) fn parse_rx_target(raw: &str) -> Result<Option<RxTarget>, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let (number, is_percent) = match text.strip_suffix('%') {
        Some(rest) => (rest.trim(), true),
        None => (text, false),
    };
    let value: f64 = number
        .parse()
        .map_err(|_| format!("看不懂的门限写法 {raw:?}，请填 1800 或 90%"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("门限必须是大于 0 的有限值，当前 {raw:?}"));
    }
    if is_percent {
        // 上限放到 200%：聚合口、多流叠加确实可能超过单口协商速率，
        // 但一个三位数以上的百分比几乎一定是手滑。
        if value > 200.0 {
            return Err(format!("百分比门限 {raw:?} 超过 200%，请确认是不是写错了"));
        }
        Ok(Some(RxTarget::Percent(value)))
    } else {
        Ok(Some(RxTarget::Mbps(value)))
    }
}

pub(super) fn endpoint_exists(state: &UiState, endpoint: &str) -> bool {
    let Some((host, selector)) = endpoint.split_once(':') else {
        return false;
    };
    let Some(name) = selector.strip_prefix("NAME=") else {
        return false;
    };
    let interfaces = match host {
        "master" => &state.master.interfaces,
        "agent" => &state.agent.interfaces,
        _ => return false,
    };
    interfaces.iter().any(|nic| nic.name == name)
}

pub(super) fn ui_endpoint_exists(state: &UiState, endpoint: &str) -> bool {
    endpoint_exists(state, endpoint)
        || builder::resolve_endpoint(endpoint, &state.master, &state.agent).is_ok()
}

pub(super) fn values_are_allowed(values: &[String], allowed: &[&str]) -> bool {
    !values.is_empty()
        && values
            .iter()
            .all(|value| allowed.iter().any(|candidate| value == candidate))
}

/// 浏览器控件不是信任边界：即使页面会过滤，后端仍需拒绝空选择、越界数值和
/// 无效档位。尤其不能把“用户把整列取消勾选”静默解释成默认 AB/TCP/IPv4。
///
/// 拆成三段是因为这三段的判据来源不同：全局档位只看 `req` 自己，逐对检查还要
/// 看网口覆盖，网口策略要看当前扫到的网口表。混在一个函数里时，读到一半分不清
/// 手上这个 `pair` 到底受哪些外部状态影响。
pub(super) fn validate_request(state: &UiState, req: &RunRequest) -> Result<(), String> {
    if let Some(plan) = req.ui_plan.as_ref() {
        if !req.pairs.is_empty() {
            return Err("ui_plan 与 legacy pairs 不能同时提交".into());
        }
        validate_global_values(req)?;
        validate_ui_plan(state, plan)?;
    } else {
        validate_global_sweeps(req)?;
    }
    for (index, group) in req.udp_groups.iter().enumerate() {
        validate_udp_group(index + 1, group)?;
    }
    for (index, group) in req.tcp_groups.iter().enumerate() {
        validate_tcp_group(index + 1, group)?;
    }
    if req.ui_plan.is_none() {
        for pair in &req.pairs {
            validate_pair(state, pair, req.udp_groups.len(), req.tcp_groups.len())?;
        }
    }
    validate_nic_policies(state, req)
}

/// 一个附加的 UDP 参数组。默认组的那几格由 `validate_global_sweeps` 管。
pub(super) fn validate_udp_group(index: usize, group: &UdpGroup) -> Result<(), String> {
    let label = if group.name.trim().is_empty() {
        format!("UDP 参数组 {index}")
    } else {
        format!("UDP 参数组「{}」", group.name.trim())
    };
    // 组不继承默认组，所以 `-b` 空着不是「跟着全局」而是「一个档位都没有」，
    // 那一组生成不出任何单元。这里挡住，比让人在「预览任务」里数不到强。
    if cleaned_list(&group.bandwidths).is_empty() {
        return Err(format!("{label} 没填 -b：组是完整定义，不继承默认组的档位"));
    }
    for bandwidth in cleaned_list(&group.bandwidths) {
        check_udp_bandwidth(&bandwidth, &label)?;
    }
    for length in cleaned_list(&group.lengths) {
        let bytes = crate::cmd::ctstraffic::parse_size_bytes(&length)
            .map_err(|error| format!("{label} 的 -l {length:?} 无效：{error}"))?;
        if bytes > 65_507 {
            return Err(format!(
                "{label} 的 -l {length:?} 超过单个 UDP 报文上限 65507 字节"
            ));
        }
    }
    for window in cleaned_list(&group.windows) {
        crate::cmd::ctstraffic::parse_size_bytes(&window)
            .map_err(|error| format!("{label} 的 -w {window:?} 无效：{error}"))?;
    }
    if group.streams > MAX_UDP_STREAMS {
        return Err(format!(
            "{label} 的流数 {} 超过上限 {MAX_UDP_STREAMS}",
            group.streams
        ));
    }
    Ok(())
}

/// 一个附加的 TCP 参数组。默认组的 `-w` / `-P` 那两个框由 `validate_global_sweeps`
/// 管。TCP 组没有 UDP 那样的必填项（`-b`）：`-w`、`-P` 都可留空。
pub(super) fn validate_tcp_group(index: usize, group: &TcpGroup) -> Result<(), String> {
    let label = if group.name.trim().is_empty() {
        format!("TCP 参数组 {index}")
    } else {
        format!("TCP 参数组「{}」", group.name.trim())
    };
    for window in cleaned_list(&group.windows) {
        crate::cmd::ctstraffic::parse_size_bytes(&window)
            .map_err(|error| format!("{label} 的 -w {window:?} 无效：{error}"))?;
    }
    if group.streams.iter().any(|value| !(1..=32).contains(value)) {
        return Err(format!("{label} 的 -P 每一档都必须在 1..=32 之间"));
    }
    Ok(())
}

/// 执行区那些「所有配对共用」的档位与数值。
pub(super) fn validate_global_sweeps(req: &RunRequest) -> Result<(), String> {
    if req.pairs.is_empty() {
        return Err("一个测试项都没勾".into());
    }
    validate_global_values(req)
}

/// 全局时长、参数档位和 ping 边界检查，共用于 legacy matrix 与 suite plan。
pub(super) fn validate_global_values(req: &RunRequest) -> Result<(), String> {
    if !(1..=86_400).contains(&req.duration) {
        return Err("时长必须在 1..=86400 秒之间".into());
    }
    if req
        .tcp_streams
        .iter()
        .any(|value| !(1..=32).contains(value))
    {
        return Err("TCP -P 每一档都必须在 1..=32 之间".into());
    }
    if !(1..=32).contains(&req.udp_streams) {
        return Err("UDP 流数必须在 1..=32 之间".into());
    }
    for (label, value) in [
        ("Wi-Fi 互测单向 RX 门限", req.wifi_pair_rx_target_mbps),
        (
            "Wi-Fi 互测双向每方向 RX 门限",
            req.wifi_pair_bidir_rx_target_mbps,
        ),
        (
            "Wi-Fi 互测双向 RX 合计门限",
            req.wifi_pair_bidir_total_rx_target_mbps,
        ),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(format!("{label}必须是非负有限 Mbps，0 表示不覆盖"));
        }
    }
    validate_ping_thresholds(req)?;
    validate_wifi_thresholds(req)?;
    for window in req
        .tcp_windows
        .iter()
        .filter(|value| !value.trim().is_empty())
    {
        crate::cmd::ctstraffic::parse_size_bytes(window.trim())
            .map_err(|error| format!("TCP -w 档位 {window:?} 无效：{error}"))?;
    }
    for bandwidth in req
        .udp_bandwidths
        .iter()
        .filter(|value| !value.trim().is_empty())
    {
        check_udp_bandwidth(bandwidth.trim(), "默认组")?;
    }
    for window in req.udp_windows.iter().filter(|v| !v.trim().is_empty()) {
        crate::cmd::ctstraffic::parse_size_bytes(window.trim())
            .map_err(|error| format!("UDP -w 档位 {window:?} 无效：{error}"))?;
    }
    for length in req.udp_lengths.iter().filter(|v| !v.trim().is_empty()) {
        // iperf3 的 -l 收字节数，也收 k/m 后缀；和下发命令用同一个解析器，
        // 免得界面放行的写法到了命令行上才炸。
        let bytes = crate::cmd::ctstraffic::parse_size_bytes(length.trim())
            .map_err(|error| format!("UDP -l 档位 {length:?} 无效：{error}"))?;
        if bytes > 65_507 {
            return Err(format!(
                "UDP -l 档位 {length:?} 超过单个 UDP 报文上限 65507 字节"
            ));
        }
    }
    // ping 的包长和次数同样要在这里挡住，理由和上面那条 UDP -l 一样：下游只会
    // **静默夹紧**（`ping::build` 把包长压到 MAX_PAYLOAD，`spec_from_config` 把
    // 次数压到 100000），而夹紧发生在分单元之后——两个越界档位各自成一个单元、
    // 各自算一个 resume id，跑出来却是同一次测试，报告上还写着两个不同的 -l。
    for size in &req.ping_payload_sizes {
        if *size > crate::ping::MAX_PAYLOAD {
            return Err(format!(
                "ping 包长档位 {size} 超过单包上限 {} 字节",
                crate::ping::MAX_PAYLOAD
            ));
        }
    }
    if req.ping_count > 100_000 {
        return Err(format!("ping 次数 {} 超过上限 100000", req.ping_count));
    }
    Ok(())
}

fn validate_nonnegative_finite(label: &str, value: f64) -> Result<(), String> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(format!("{label}必须是非负有限值，0 表示不覆盖"))
    }
}

fn validate_ping_thresholds(req: &RunRequest) -> Result<(), String> {
    for (label, value) in [
        ("有线 small Avg RTT", req.ping_wired_small_avg_rtt_ms),
        ("有线 small Max RTT", req.ping_wired_small_max_rtt_ms),
        ("有线 medium Avg RTT", req.ping_wired_medium_avg_rtt_ms),
        ("有线 medium Max RTT", req.ping_wired_medium_max_rtt_ms),
        ("有线 large Avg RTT", req.ping_wired_large_avg_rtt_ms),
        ("有线 large Max RTT", req.ping_wired_large_max_rtt_ms),
        ("Wi-Fi small Avg RTT", req.ping_wifi_small_avg_rtt_ms),
        ("Wi-Fi small Max RTT", req.ping_wifi_small_max_rtt_ms),
        ("Wi-Fi medium Avg RTT", req.ping_wifi_medium_avg_rtt_ms),
        ("Wi-Fi medium Max RTT", req.ping_wifi_medium_max_rtt_ms),
        ("Wi-Fi large Avg RTT", req.ping_wifi_large_avg_rtt_ms),
        ("Wi-Fi large Max RTT", req.ping_wifi_large_max_rtt_ms),
    ] {
        validate_nonnegative_finite(label, value)?;
    }
    validate_nonnegative_finite("兼容旧版有线 small Max RTT", req.ping_max_rtt_ms)
}

fn validate_wifi_thresholds(req: &RunRequest) -> Result<(), String> {
    let mut band_pairs = HashSet::new();
    let mut legacy_bands = HashSet::new();
    for (index, rule) in req.wifi_band_thresholds.iter().enumerate() {
        // 去重按**稳定枚举**判：`5GHz` 和 `5g` 是同一个频段，两条规则同时存在
        // 时到底哪条生效取决于遍历顺序，那正是要挡住的形态。
        let master = super::plan::canonical_wifi_band(&rule.master_band);
        let agent = super::plan::canonical_wifi_band(&rule.agent_band);
        let src = super::plan::canonical_wifi_band(&rule.src_band);
        let dst = super::plan::canonical_wifi_band(&rule.dst_band);
        let canonical = !rule.master_band.trim().is_empty() || !rule.agent_band.trim().is_empty();
        let legacy = !rule.src_band.trim().is_empty() || !rule.dst_band.trim().is_empty();
        if canonical && legacy {
            return Err(format!("Wi-Fi 频段门限第 {} 行混用了新旧字段", index + 1));
        }
        if canonical {
            if rule.master_band.trim().is_empty() || rule.agent_band.trim().is_empty() {
                return Err(format!(
                    "Wi-Fi 频段门限第 {} 行缺少主控或辅测频段",
                    index + 1
                ));
            }
            if !band_pairs.insert((master, agent)) {
                return Err(format!("Wi-Fi 频段门限第 {} 行与已有规则重复", index + 1));
            }
            for (label, value) in [
                (
                    "Wi-Fi 单向主控→辅测 RX 门限",
                    rule.rx_target_master_to_agent_mbps,
                ),
                (
                    "Wi-Fi 单向辅测→主控 RX 门限",
                    rule.rx_target_agent_to_master_mbps,
                ),
                ("Wi-Fi 双向 RX 合计门限", rule.bidir_total_rx_target_mbps),
                (
                    "Wi-Fi 双向主控→辅测 RX 门限",
                    rule.bidir_rx_target_master_to_agent_mbps,
                ),
                (
                    "Wi-Fi 双向辅测→主控 RX 门限",
                    rule.bidir_rx_target_agent_to_master_mbps,
                ),
            ] {
                validate_nonnegative_finite(label, value)?;
            }
            continue;
        }

        if rule.src_band.trim().is_empty() || rule.dst_band.trim().is_empty() {
            return Err(format!(
                "Wi-Fi 频段门限第 {} 行缺少发送或接收频段",
                index + 1
            ));
        }
        if !legacy_bands.insert((src, dst)) {
            return Err(format!("Wi-Fi 频段门限第 {} 行与已有规则重复", index + 1));
        }
        validate_nonnegative_finite("Wi-Fi 频段单向 RX 门限", rule.rx_target_mbps)?;
        validate_nonnegative_finite("Wi-Fi 频段双向每方向 RX 门限", rule.bidir_rx_target_mbps)?;
    }

    let mut pairs = HashSet::new();
    for (index, rule) in req.wifi_pair_thresholds.iter().enumerate() {
        let src = rule.src_endpoint.trim();
        let dst = rule.dst_endpoint.trim();
        if src.is_empty() || dst.is_empty() {
            return Err(format!(
                "Wi-Fi 网口对门限第 {} 行缺少发送或接收网口",
                index + 1
            ));
        }
        if !pairs.insert((src, dst)) {
            return Err(format!("Wi-Fi 网口对门限第 {} 行与已有规则重复", index + 1));
        }
        for (label, value) in [
            ("Wi-Fi 网口对单向 A→B RX 门限", rule.rx_target_ab_mbps),
            ("Wi-Fi 网口对单向 B→A RX 门限", rule.rx_target_ba_mbps),
            ("Wi-Fi 网口对双向 A→B RX 门限", rule.bidir_rx_target_ab_mbps),
            ("Wi-Fi 网口对双向 B→A RX 门限", rule.bidir_rx_target_ba_mbps),
        ] {
            validate_nonnegative_finite(label, value)?;
        }
    }
    Ok(())
}

pub(super) fn canonical_ui_direction(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "ab" | "a->b" | "a>b" | "a_to_b" => Some("ab"),
        "ba" | "b->a" | "b>a" | "b_to_a" => Some("ba"),
        "bidir" | "both-way" | "a<->b" | "双向" => Some("bidir"),
        // `both` is the legacy spelling for two independent one-way legs.
        "both" => Some("both"),
        _ => None,
    }
}

pub(super) fn canonical_ui_ip(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "v4" | "ipv4" | "4" => Some("v4"),
        "v6" | "ipv6" | "6" => Some("v6"),
        _ => None,
    }
}

pub(super) fn ui_task_protocol(task: &UiTask) -> Option<String> {
    let raw = if !task.protocol.trim().is_empty() {
        task.protocol.trim().to_ascii_lowercase()
    } else if task.transports.len() == 1 {
        task.transports[0].trim().to_ascii_lowercase()
    } else {
        String::new()
    };
    match raw.as_str() {
        "tcp" | "udp" | "ping" => Some(raw),
        _ => None,
    }
}

pub(super) fn validate_ui_recipe(
    protocol: &str,
    recipe: &UiRecipe,
    index: usize,
) -> Result<(), String> {
    if recipe.id.trim().is_empty() {
        return Err(format!("{protocol} 配置 {} 缺少稳定 id", index + 1));
    }
    // `mode` 是个**死字段**：校验器过去只准 fixed/scan 两个取值，而编译器
    // （`webui/plan.rs`）从头到尾不读它——`fixed` 和 `scan` 产出的是**同一份计划**。
    // 用户以为 `fixed` 把档位钉死成一档，实际三条轴全展开、耗时三倍。
    //
    // 不实现它、而是拒绝它：fixed/scan 的语义已经由轴的取值个数天然表达
    // （单值 = 钉死，多值 = 扫描），mode 只是个冗余开关。同一个校验器为 PING
    // 配方写下过完全一样的判断（「让字段看起来可配置而被静默忽略」正是要拒绝的
    // 形状），这里跟上那个先例。详见 .ai/DESIGN-v6.0-architecture.md ADR-16。
    //
    // serde 字段保留，所以老项目文件仍能被解析——只是会在这里被明确挡下并告诉
    // 用户怎么改，而不是继续静默地跑出另一份东西。
    if !recipe.mode.trim().is_empty() {
        return Err(format!(
            "{protocol} 配置 {} 的 mode 字段已废弃（当前值 {:?}）：档位由轴的取值个数决定，\
             单值就是钉死、多值就是扫描，mode 从来没有被计划编译器读过。\
             把这一行从项目文件里删掉即可。",
            recipe.id,
            recipe.mode.trim()
        ));
    }
    if protocol == "tcp" {
        for window in recipe
            .tcp_windows
            .iter()
            .chain(recipe.windows.iter())
            .filter(|v| !v.trim().is_empty())
        {
            crate::cmd::ctstraffic::parse_size_bytes(window.trim())
                .map_err(|e| format!("TCP 配置 {} 的 -w {:?} 无效：{e}", recipe.id, window))?;
        }
        for profile in &recipe.profiles {
            if let Some(window) = profile.window.as_deref().filter(|v| !v.trim().is_empty()) {
                crate::cmd::ctstraffic::parse_size_bytes(window.trim()).map_err(|e| {
                    format!(
                        "TCP 配置 {} 的 profile -w {:?} 无效：{e}",
                        recipe.id, window
                    )
                })?;
            }
            for streams in profile
                .tcp_streams
                .as_ref()
                .unwrap_or(&profile.streams)
                .values()
            {
                if !(1..=32).contains(&streams) {
                    return Err(format!("TCP 配置 {} 的 -P 必须在 1..=32 之间", recipe.id));
                }
            }
        }
        if recipe.tcp_streams.iter().any(|v| !(1..=32).contains(v)) {
            return Err(format!("TCP 配置 {} 的 -P 必须在 1..=32 之间", recipe.id));
        }
    } else if protocol == "udp" {
        for bandwidth in recipe.bandwidths.iter().filter(|v| !v.trim().is_empty()) {
            check_udp_bandwidth(bandwidth.trim(), &format!("UDP 配置 {}", recipe.id))?;
        }
        for length in recipe.lengths.iter().filter(|v| !v.trim().is_empty()) {
            let bytes = crate::cmd::ctstraffic::parse_size_bytes(length.trim())
                .map_err(|e| format!("UDP 配置 {} 的 -l {:?} 无效：{e}", recipe.id, length))?;
            if bytes > 65_507 {
                return Err(format!("UDP 配置 {} 的 -l 超过 65507 字节", recipe.id));
            }
        }
        for window in recipe.windows.iter().filter(|v| !v.trim().is_empty()) {
            crate::cmd::ctstraffic::parse_size_bytes(window.trim())
                .map_err(|e| format!("UDP 配置 {} 的 -w {:?} 无效：{e}", recipe.id, window))?;
        }
        for profile in &recipe.profiles {
            if let Some(bandwidth) = profile
                .bandwidth
                .as_deref()
                .filter(|v| !v.trim().is_empty())
            {
                check_udp_bandwidth(bandwidth.trim(), &format!("UDP 配置 {}", recipe.id))?;
            }
            if let Some(length) = profile.length.as_deref().filter(|v| !v.trim().is_empty()) {
                let bytes =
                    crate::cmd::ctstraffic::parse_size_bytes(length.trim()).map_err(|e| {
                        format!(
                            "UDP 配置 {} 的 profile -l {:?} 无效：{e}",
                            recipe.id, length
                        )
                    })?;
                if bytes > 65_507 {
                    return Err(format!(
                        "UDP 配置 {} 的 profile -l 超过 65507 字节",
                        recipe.id
                    ));
                }
            }
            if let Some(window) = profile.window.as_deref().filter(|v| !v.trim().is_empty()) {
                crate::cmd::ctstraffic::parse_size_bytes(window.trim()).map_err(|e| {
                    format!(
                        "UDP 配置 {} 的 profile -w {:?} 无效：{e}",
                        recipe.id, window
                    )
                })?;
            }
            let streams = profile
                .udp_streams
                .as_ref()
                .unwrap_or(&profile.streams)
                .values();
            if streams.iter().any(|v| !(1..=32).contains(v)) {
                return Err(format!("UDP 配置 {} 的流数必须在 1..=32 之间", recipe.id));
            }
        }
        if recipe.udp_streams.iter().any(|v| !(1..=32).contains(v)) {
            return Err(format!("UDP 配置 {} 的流数必须在 1..=32 之间", recipe.id));
        }
        for profile in &recipe.udp_profiles {
            check_udp_bandwidth(profile.bandwidth.trim(), &format!("UDP 配置 {}", recipe.id))?;
            if let Some(length) = profile.length.as_deref().filter(|v| !v.trim().is_empty()) {
                let bytes =
                    crate::cmd::ctstraffic::parse_size_bytes(length.trim()).map_err(|e| {
                        format!(
                            "UDP 配置 {} 的 profile -l {:?} 无效：{e}",
                            recipe.id, length
                        )
                    })?;
                if bytes > 65_507 {
                    return Err(format!(
                        "UDP 配置 {} 的 profile -l 超过 65507 字节",
                        recipe.id
                    ));
                }
            }
            if let Some(window) = profile.window.as_deref().filter(|v| !v.trim().is_empty()) {
                crate::cmd::ctstraffic::parse_size_bytes(window.trim()).map_err(|e| {
                    format!(
                        "UDP 配置 {} 的 profile -w {:?} 无效：{e}",
                        recipe.id, window
                    )
                })?;
            }
        }
        // An explicitly defined UDP recipe must expand to at least one
        // profile. Without this guard a card containing only empty fields is
        // accepted and silently contributes no test units.
        let has_bandwidth = if !recipe.udp_profiles.is_empty() {
            recipe
                .udp_profiles
                .iter()
                .any(|profile| !profile.bandwidth.trim().is_empty())
        } else if !recipe.profiles.is_empty() {
            let recipe_fallback = recipe
                .bandwidths
                .iter()
                .any(|value| !value.trim().is_empty());
            recipe.profiles.iter().any(|profile| {
                profile
                    .bandwidth
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                    || recipe_fallback
            })
        } else {
            recipe
                .bandwidths
                .iter()
                .any(|value| !value.trim().is_empty())
        };
        // A completely empty recipe means "use the request/config default";
        // that is useful when a suite intentionally wants the shared default
        // without duplicating its axes.  Reject only an explicitly populated
        // recipe whose fields all resolve to empty values, because that shape
        // otherwise looks configured while producing zero units.
        let explicitly_configured = !recipe.udp_profiles.is_empty()
            || !recipe.profiles.is_empty()
            || !recipe.bandwidths.is_empty()
            || !recipe.lengths.is_empty()
            || !recipe.windows.is_empty()
            || !recipe.udp_streams.is_empty();
        if explicitly_configured && !has_bandwidth {
            return Err(format!(
                "UDP 配置 {} 没有有效的 -b 档位，无法生成测试单元",
                recipe.id
            ));
        }
    }
    Ok(())
}

/// 校验任务级的显式验收目标。
///
/// `RateTargets::for_direction` 会把非法值当成“未配置”并继续走自动推导；
/// 对来自浏览器的计划来说这会把一个明显的输入错误静默吞掉，最终报告看起来
/// 像是用户根本没有填写目标。因此 UI 计划要在边界处拒绝非有限值和非正值。
pub(super) fn validate_ui_rate_targets(
    label: &str,
    targets: &crate::config::RateTargets,
) -> Result<(), String> {
    for (direction, value) in [
        ("forward", targets.forward),
        ("ab", targets.ab),
        ("ba", targets.ba),
    ] {
        if let Some(value) = value {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!(
                    "{label} 的 {direction} 目标必须是大于 0 的有限 Mbps"
                ));
            }
        }
    }
    Ok(())
}

/// Validate a suite plan without touching the legacy matrix checks.
pub(super) fn validate_ui_plan(state: &UiState, plan: &UiPlan) -> Result<(), String> {
    if plan.ui_plan_version > 1 {
        return Err(format!(
            "不支持的 ui_plan_version={}（当前支持 1）",
            plan.ui_plan_version
        ));
    }
    if plan.link_sets.is_empty() {
        return Err("ui_plan 至少需要一个 link_set".into());
    }
    if plan.suites.is_empty() {
        return Err("ui_plan 至少需要一个 suite".into());
    }
    if plan.bindings.is_empty() {
        return Err("ui_plan 至少需要一个 binding".into());
    }

    // 哪些网口对**真的会被跑到**：拓扑相关的检查只对它们硬失败。
    //
    // 这一段修的是一处不对称：下面那个循环过去对**所有** link_set 的每个
    // pair_ref 都做端点存在性与解析，一个没人引用的草稿集合里躺着一条失效的
    // 网口对，就能把整份请求顶掉，报错还指向用户根本没打算跑的集合。而同一个
    // 函数的注释、以及 `使用说明.md`，承诺的都是「未绑定集合里的失效对只是提示，
    // 不会阻止另一套可执行分配」。测试当时只覆盖了「空草稿」，没覆盖「非空但含
    // 失效对的草稿」，所以这条承诺一直是破的。
    //
    // 口径与文档逐字对齐：整组绑定 ⇒ 该集合的全部对；`pair_ids` 明确选中 ⇒ 只有
    // 被选中的那几条。形状类检查（id 缺失/重复、端点串为空、源目标同一个串）不在
    // 此列——它们不看拓扑，任何集合里出现都是数据错误，照旧全量检查。
    let mut referenced_all: HashSet<&str> = HashSet::new();
    let mut referenced_pairs: HashSet<(&str, &str)> = HashSet::new();
    for binding in &plan.bindings {
        if binding.pair_ids.is_empty() {
            referenced_all.insert(binding.link_set_id.as_str());
        } else {
            for pair_id in &binding.pair_ids {
                referenced_pairs.insert((binding.link_set_id.as_str(), pair_id.as_str()));
            }
        }
    }
    let is_referenced = |set_id: &str, pair_id: &str| {
        referenced_all.contains(set_id) || referenced_pairs.contains(&(set_id, pair_id))
    };

    let mut ids = HashSet::new();
    for (index, set) in plan.link_sets.iter().enumerate() {
        if set.id.trim().is_empty() || !ids.insert(set.id.clone()) {
            return Err(format!("link_set {} 的 id 缺失或重复", index + 1));
        }
        let mut pair_ids = HashSet::new();
        let mut pair_endpoints = HashSet::new();
        for (pair_index, pair) in set.pair_refs.iter().enumerate() {
            if pair.id.trim().is_empty() || !pair_ids.insert(pair.id.clone()) {
                return Err(format!("link_set {} 的 pair_ref id 缺失或重复", set.id));
            }
            if pair.src.trim().is_empty() || pair.dst.trim().is_empty() || pair.src == pair.dst {
                return Err(format!(
                    "link_set {} 的 pair_ref {} 端点为空或源目标相同：{} -> {}",
                    set.id,
                    pair_index + 1,
                    pair.src,
                    pair.dst
                ));
            }
            if !is_referenced(&set.id, &pair.id) {
                // 未被任何 binding 选中：拓扑对不上只是草稿的事，界面会把它标成
                // 「端点已消失」，不该挡下别人的可执行分配。
                continue;
            }
            if !ui_endpoint_exists(state, &pair.src) || !ui_endpoint_exists(state, &pair.dst) {
                return Err(format!(
                    "link_set {} 的 pair_ref {} 已失效：{} -> {}",
                    set.id,
                    pair_index + 1,
                    pair.src,
                    pair.dst
                ));
            }
            // NAME= and role selectors can spell the same physical interface in
            // different ways. Resolve both before comparing; a raw-string check
            // alone would let a self-link through and only fail much later in the
            // builder, after the preview had already been shown.
            let src_endpoint = builder::resolve_endpoint(&pair.src, &state.master, &state.agent)
                .map_err(|error| {
                    format!(
                        "link_set {} 的 pair_ref {} 源端点无效：{error}",
                        set.id,
                        pair_index + 1
                    )
                })?;
            let dst_endpoint = builder::resolve_endpoint(&pair.dst, &state.master, &state.agent)
                .map_err(|error| {
                    format!(
                        "link_set {} 的 pair_ref {} 目标端点无效：{error}",
                        set.id,
                        pair_index + 1
                    )
                })?;
            if src_endpoint.key() == dst_endpoint.key() {
                return Err(format!(
                    "link_set {} 的 pair_ref {} 源和目标不能是同一块网口",
                    set.id,
                    pair_index + 1
                ));
            }
            let mut endpoint_key = [src_endpoint.key(), dst_endpoint.key()];
            endpoint_key.sort();
            if !pair_endpoints.insert(endpoint_key) {
                return Err(format!(
                    "link_set {} 包含重复的网口对：{} -> {}",
                    set.id, pair.src, pair.dst
                ));
            }
        }
        // An empty set is allowed as an unbound draft.  The quick workspace
        // lets users create a collection before selecting concrete NIC pairs,
        // and execution requests can also contain a stale-only collection
        // after the browser filters invalid endpoints.  A set that is actually
        // referenced by a binding is checked below and must still contain at
        // least one pair; keeping the distinction here avoids rejecting an
        // otherwise runnable plan merely because an unused draft is present.
        // 同理，非空但**未被引用**的草稿里那些失效的网口对，上面已经跳过了。
    }

    // Recipe IDs are global across protocol buckets so a binding remains
    // stable even if the UI reorders TCP and UDP cards.  They are a separate
    // namespace from link-set IDs: a project is perfectly entitled to call a
    // set and a recipe both "default" because references always carry the
    // owning field (link_set_id vs recipe_ids).  Reusing the top-level `ids`
    // set here would reject that harmless, and common, naming pattern.
    let mut recipe_ids = HashSet::new();
    for (protocol, recipes) in [
        ("tcp", &plan.recipes.tcp),
        ("udp", &plan.recipes.udp),
        ("ping", &plan.recipes.ping),
    ] {
        for (index, recipe) in recipes.iter().enumerate() {
            if recipe.id.trim().is_empty() || !recipe_ids.insert(recipe.id.clone()) {
                return Err(format!("{protocol} 配置 id 缺失或重复：{}", recipe.id));
            }
            validate_ui_recipe(protocol, recipe, index)?;
        }
    }

    let mut suite_ids = HashSet::new();
    for suite in &plan.suites {
        if suite.id.trim().is_empty() || !suite_ids.insert(suite.id.clone()) {
            return Err(format!("suite id 缺失或重复：{}", suite.id));
        }
        if !suite.execution.trim().is_empty() && !suite.execution.eq_ignore_ascii_case("sequential")
        {
            return Err(format!("suite {} 只支持 execution=sequential", suite.id));
        }
        if suite.tasks.is_empty() {
            return Err(format!("suite {} 没有任务", suite.id));
        }
        let mut task_ids = HashSet::new();
        for task in &suite.tasks {
            if task.id.trim().is_empty() || !task_ids.insert(task.id.clone()) {
                return Err(format!("suite {} 的 task id 缺失或重复", suite.id));
            }
            let protocol = ui_task_protocol(task)
                .ok_or_else(|| format!("suite {} 的 task {} 协议无效", suite.id, task.id))?;
            if task.transports.iter().any(|transport| {
                let transport = transport.trim().to_ascii_lowercase();
                !transport.is_empty() && transport != protocol
            }) {
                return Err(format!(
                    "suite {} 的 task {} protocol 与 transports 不一致",
                    suite.id, task.id
                ));
            }
            if task.directions.is_empty()
                || task
                    .directions
                    .iter()
                    .any(|direction| canonical_ui_direction(direction).is_none())
            {
                return Err(format!("suite {} 的 task {} 方向无效", suite.id, task.id));
            }
            if task.ip.is_empty() || task.ip.iter().any(|ip| canonical_ui_ip(ip).is_none()) {
                return Err(format!(
                    "suite {} 的 task {} IP 版本无效",
                    suite.id, task.id
                ));
            }
            let recipe_ids = &task.recipe_ids;
            // PING currently takes its count and payload sizes from the task
            // (or the request-wide controls).  `UiRecipe` has no ping-specific
            // fields, and the compiler only used a referenced id for naming,
            // which made a non-empty PING recipe look configurable while its
            // parameters were silently ignored.  Reject that ambiguous shape
            // until a recipe schema with explicit ping semantics is added.
            if protocol == "ping" && !recipe_ids.is_empty() {
                return Err(format!(
                    "suite {} 的 task {} 暂不支持 PING 配置引用，请直接填写 ping 次数和包长",
                    suite.id, task.id
                ));
            }
            let recipes = match protocol.as_str() {
                "tcp" => &plan.recipes.tcp,
                "udp" => &plan.recipes.udp,
                _ => &plan.recipes.ping,
            };
            let mut seen_recipe_ids = HashSet::new();
            for recipe_id in recipe_ids {
                if !seen_recipe_ids.insert(recipe_id) {
                    return Err(format!(
                        "suite {} 的 task {} 重复引用 {} 配置 {}",
                        suite.id, task.id, protocol, recipe_id
                    ));
                }
                if !recipes.iter().any(|recipe| recipe.id == *recipe_id) {
                    return Err(format!(
                        "suite {} 的 task {} 引用了不存在的 {} 配置 {}",
                        suite.id, task.id, protocol, recipe_id
                    ));
                }
            }
            if let Some(duration) = task.duration {
                if !(1..=86_400).contains(&duration) {
                    return Err(format!(
                        "suite {} 的 task {} 时长必须在 1..=86400 秒之间",
                        suite.id, task.id
                    ));
                }
            }
            if let Some(targets) = &task.rate_targets_mbps {
                validate_ui_rate_targets(
                    &format!("suite {} 的 task {} rate_targets_mbps", suite.id, task.id),
                    targets,
                )?;
            }
            if protocol == "ping" && task.ping_count.is_some_and(|v| v > 100_000) {
                return Err(format!("suite {} 的 ping 次数超过 100000", suite.id));
            }
            if task
                .ping_payload_sizes
                .as_ref()
                .is_some_and(|sizes| sizes.iter().any(|v| *v > crate::ping::MAX_PAYLOAD))
            {
                return Err(format!("suite {} 的 ping 包长超过上限", suite.id));
            }
            if protocol == "ping" && task.ping_payload_sizes.as_ref().is_some_and(Vec::is_empty) {
                return Err(format!(
                    "suite {} 的 task {} 至少需要一个 ping 包长",
                    suite.id, task.id
                ));
            }
            for (label, raw) in [
                ("A→B", &task.rx_target_bidir_ab),
                ("B→A", &task.rx_target_bidir_ba),
                ("双向 RX 合计", &task.rx_target_bidir_total),
            ] {
                if raw.trim().is_empty() {
                    continue;
                }
                if !task
                    .directions
                    .iter()
                    .filter_map(|d| canonical_ui_direction(d))
                    .any(|d| d == "bidir")
                {
                    return Err(format!(
                        "suite {} 的 task {} 填了 {label} 双向门限但未选择双向",
                        suite.id, task.id
                    ));
                }
                if let Some(RxTarget::Percent(_)) = parse_rx_target(raw)? {
                    return Err(format!(
                        "suite {} 的 task {} 双向门限只能填绝对 Mbps",
                        suite.id, task.id
                    ));
                }
            }
        }
        if !suite.order.is_empty() {
            let mut seen_order = HashSet::new();
            for task_id in &suite.order {
                if !task_ids.contains(task_id) || !seen_order.insert(task_id) {
                    return Err(format!(
                        "suite {} 的 order 引用了无效或重复 task {}",
                        suite.id, task_id
                    ));
                }
            }
        }
    }

    let set_ids: HashSet<&str> = plan.link_sets.iter().map(|s| s.id.as_str()).collect();
    let mut binding_ids = HashSet::new();
    for binding in &plan.bindings {
        if binding.id.trim().is_empty() || !binding_ids.insert(binding.id.clone()) {
            return Err(format!("binding id 缺失或重复：{}", binding.id));
        }
        if !set_ids.contains(binding.link_set_id.as_str()) {
            return Err(format!(
                "binding {} 引用了不存在的 link_set {}",
                binding.id, binding.link_set_id
            ));
        }
        if !suite_ids.contains(binding.suite_id.as_str()) {
            return Err(format!(
                "binding {} 引用了不存在的 suite {}",
                binding.id, binding.suite_id
            ));
        }
        // `append` has no defined merge semantics in the current planner:
        // bindings already select an explicit set of pair refs and a suite,
        // while the compiler treats every binding as a complete replacement
        // assignment.  Accepting it here would therefore make a hand-written
        // project appear to request append while silently executing replace.
        // Keep the omitted/replace spellings (both mean the current behavior)
        // and reject the unsupported mode at the API boundary.
        if !binding.mode.trim().is_empty() && !binding.mode.trim().eq_ignore_ascii_case("replace") {
            return Err(format!(
                "binding {} 的 mode 只支持 replace（append 尚未支持）",
                binding.id
            ));
        }
        let set = plan
            .link_sets
            .iter()
            .find(|s| s.id == binding.link_set_id)
            .expect("set id checked above");
        if !binding.pair_ids.is_empty() {
            let mut seen_pair_ids = HashSet::new();
            for pair_id in &binding.pair_ids {
                if !seen_pair_ids.insert(pair_id) {
                    return Err(format!(
                        "binding {} 重复引用了 pair_ref {}",
                        binding.id, pair_id
                    ));
                }
                if !set.pair_refs.iter().any(|pair| pair.id == *pair_id) {
                    return Err(format!(
                        "binding {} 引用了不存在的 pair_ref {}",
                        binding.id, pair_id
                    ));
                }
            }
        }
        // A binding must always resolve to at least one concrete pair.  This
        // check deliberately happens after validating `pair_ids`, so an
        // unknown ID still gets the more useful "不存在" diagnostic above.
        let has_effective_pairs = if binding.pair_ids.is_empty() {
            !set.pair_refs.is_empty()
        } else {
            binding
                .pair_ids
                .iter()
                .any(|pair_id| set.pair_refs.iter().any(|pair| pair.id == *pair_id))
        };
        if !has_effective_pairs {
            return Err(format!(
                "binding {} 没有可执行的 pair_ref；请先为链路集合添加网口对",
                binding.id
            ));
        }
    }
    Ok(())
}

/// 矩阵里的一行。`pinned` 是在「网口与策略」里单独指定了 UDP `-b` 的网口，
/// 逐对档位能不能生效要看它。
pub(super) fn validate_pair(
    state: &UiState,
    pair: &PairSelection,
    udp_group_count: usize,
    tcp_group_count: usize,
) -> Result<(), String> {
    if pair.src == pair.dst
        || !endpoint_exists(state, &pair.src)
        || !endpoint_exists(state, &pair.dst)
    {
        return Err(format!(
            "测试配对已失效：{} -> {}。请刷新网口后重新选择",
            pair.src, pair.dst
        ));
    }
    if !values_are_allowed(&pair.directions, &["ab", "ba", "bidir"]) {
        return Err(format!(
            "配对 {} / {} 至少勾一个有效方向",
            pair.src, pair.dst
        ));
    }
    // PING 和 TCP/UDP 并排放在界面的「协议」列，白名单里也必须一起放行。
    // 漏掉它不只是「PING 跑不了」：整条请求会被判非法，连同一配对里本来
    // 能跑的 TCP/UDP 一起废掉，而错误文案还在让人去勾 TCP 或 UDP。
    // 双向门限只收绝对值，且只有勾了「双向」才有意义。填了却没勾双向要报错
    // 而不是静默忽略——静默忽略的话，人会以为门限放低了、看到 FAIL 去查链路。
    let bidir_selected = pair.directions.iter().any(|d| d == "bidir");
    for (label, raw) in [
        ("A→B", &pair.rx_target_bidir_ab),
        ("B→A", &pair.rx_target_bidir_ba),
        ("双向 RX 合计", &pair.rx_target_bidir_total),
    ] {
        if raw.trim().is_empty() {
            continue;
        }
        if !bidir_selected {
            return Err(format!(
                "配对 {} / {} 填了 {label} 双向门限，却没有勾「双向」。\
                 双向门限只作用于双向并发单元；单向的门限在「网口与策略」里改",
                pair.src, pair.dst
            ));
        }
        match parse_rx_target(raw).map_err(|error| {
            format!(
                "配对 {} / {} 的 {label} 双向门限：{error}",
                pair.src, pair.dst
            )
        })? {
            Some(RxTarget::Mbps(_)) => {}
            Some(RxTarget::Percent(_)) => {
                return Err(format!(
                    "配对 {} / {} 的 {label} 双向门限只能填绝对 Mbps。\
                     百分比要按单块网卡的协商速率换算，而双向门限说的是这两块口\
                     并发时的能力，两者不成比例",
                    pair.src, pair.dst
                ))
            }
            None => {}
        }
    }
    // 选了默认组之外的组，却没勾 UDP：那几组一个单元都不会跑。和双向门限
    // 同一条规矩——选了却不生效要当场说，静默忽略的话人会以为跑的是那组。
    let udp_selected = pair.transports.iter().any(|t| t == "udp");
    if !udp_selected && pair.udp_groups.iter().any(|index| *index > 0) {
        return Err(format!(
            "配对 {} / {} 选了 UDP 参数组，却没有勾 UDP",
            pair.src, pair.dst
        ));
    }
    for index in &pair.udp_groups {
        if *index > udp_group_count {
            return Err(format!(
                "配对 {} / {} 选的 UDP 参数组不存在（共 {udp_group_count} 个附加组）",
                pair.src, pair.dst
            ));
        }
    }
    // TCP 参数组同 UDP：选了默认组之外的组却没勾 TCP，那几组一个单元都不跑，
    // 当场说清楚，别静默忽略。
    let tcp_selected = pair.transports.iter().any(|t| t == "tcp");
    if !tcp_selected && pair.tcp_groups.iter().any(|index| *index > 0) {
        return Err(format!(
            "配对 {} / {} 选了 TCP 参数组，却没有勾 TCP",
            pair.src, pair.dst
        ));
    }
    for index in &pair.tcp_groups {
        if *index > tcp_group_count {
            return Err(format!(
                "配对 {} / {} 选的 TCP 参数组不存在（共 {tcp_group_count} 个附加组）",
                pair.src, pair.dst
            ));
        }
    }
    if !values_are_allowed(&pair.transports, &["tcp", "udp", "ping"]) {
        return Err(format!(
            "配对 {} / {} 至少勾 TCP / UDP / PING 之一",
            pair.src, pair.dst
        ));
    }
    if !values_are_allowed(&pair.ip, &["v4", "v6"]) {
        return Err(format!(
            "配对 {} / {} 至少勾 IPv4 或 IPv6",
            pair.src, pair.dst
        ));
    }
    Ok(())
}

/// 「网口与策略」那张表。
pub(super) fn validate_nic_policies(state: &UiState, req: &RunRequest) -> Result<(), String> {
    let mut seen = HashSet::new();
    for policy in &req.nic_policies {
        if !endpoint_exists(state, &policy.endpoint) {
            return Err(format!(
                "网口策略已失效：{}。请刷新网口后重新填写",
                policy.endpoint
            ));
        }
        if !seen.insert(policy.endpoint.clone()) {
            return Err(format!("网口策略重复：{}", policy.endpoint));
        }
        parse_rx_target(&policy.rx_target)
            .map_err(|error| format!("{} 的 RX 门限：{error}", policy.endpoint))?;
        if !policy.udp_length.trim().is_empty() {
            let bytes = crate::cmd::ctstraffic::parse_size_bytes(policy.udp_length.trim())
                .map_err(|error| format!("{} 的 UDP -l 无效：{error}", policy.endpoint))?;
            if bytes > 65_507 {
                return Err(format!(
                    "{} 的 UDP -l 超过单个 UDP 报文上限 65507 字节",
                    policy.endpoint
                ));
            }
        }
        if !policy.udp_bandwidth.trim().is_empty() {
            check_udp_bandwidth(policy.udp_bandwidth.trim(), &policy.endpoint)?;
        }
    }
    Ok(())
}

/// 界面上 UDP 并发流数的上限，和输入框的 `max` 对齐。
pub(super) const MAX_UDP_STREAMS: u32 = 32;

/// `-b` 的量纲护栏。
///
/// 输入框里的裸数字按 **Mbps** 算（`UdpProfile::parsed_bandwidth` 里无后缀时
/// 乘 10^6），而「预览任务」以前打印的是 bit/s 整数——把 `1000000000` 抄回输入框
/// 就变成 10^9 Mbps，解析得过、校验得过，然后拿着一个天文数字去灌包。
///
/// 400Gbps 远高于这套工具面对的任何链路（最快 10GETH），又远低于那种手滑，
/// 挡在这里能把「填错单位」变成一句能读懂的话。
pub(super) const MAX_UDP_BANDWIDTH_MBPS: f64 = 400_000.0;

/// 解析并检查一个 `-b` 档位。`label` 用来说清是哪一格填错了。
pub(super) fn check_udp_bandwidth(raw: &str, label: &str) -> Result<(), String> {
    let parsed = UdpProfile::bw(raw)
        .parsed_bandwidth()
        .map_err(|error| format!("{label} 的 UDP -b {raw:?} 无效：{error}"))?;
    if parsed.mbps > MAX_UDP_BANDWIDTH_MBPS {
        return Err(format!(
            "{label} 的 UDP -b {raw:?} 折合 {:.0} Mbps，超出这套工具面对的任何链路。\
             输入框里的裸数字按 Mbps 算（`1000` = 1000Mbps），要写 bit/s 请加后缀：\
             `1000m` 或 `1G` 都是 1000Mbps",
            parsed.mbps
        ));
    }
    Ok(())
}

/// 在「网口与策略」里单独指定了 UDP `-b` 的那些网口。
///
/// 这个覆盖按**发送腿**生效（见 builder 里的 `link_policy(...).udp_bandwidth`），
/// 所以它同时决定了「全局/逐对档位对这条腿还有没有意义」。
pub(super) fn udp_pinned_senders(req: &RunRequest) -> HashSet<String> {
    req.nic_policies
        .iter()
        .filter(|policy| !policy.udp_bandwidth.trim().is_empty())
        .map(|policy| policy.endpoint.clone())
        .collect()
}
