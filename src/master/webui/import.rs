//! 把既有配置文件反解成界面上的勾选。**已封存（ADR-13）。**
//!
//! 方向和 [`super::plan`] 相反：那边是「界面 → 配置」，这边是「配置 → 界面」。
//!
//! # 封存的含义
//!
//! 端点、DTO 和 serde 形状**全部保留**（对外 JSON 字段即兼容面），但 v6.0 的
//! 控制台界面上**没有入口**。理由见 ADR-13：矩阵界面唯一独占的场景是「老
//! config 在浏览器里改改再跑」，而这条路对套件计划**从来就是有损且静默的**
//! ——`Config` 根本不承载套件，往返一次任务顺序、逐任务时长和验收目标全丢。
//! 重建矩阵 UI 的成本恰好落在旧页 bug 最密的那一块（参数组下标级联重排、
//! 整列开关、跨面板隐性耦合），而它救不了上面那个损失。
//!
//! 下面那句「两边必须互为逆运算」的老注释**对套件计划从来不成立**；
//! `api_import` 现在会对带 `origin`/`link_group` 的配置明确推一条 notice
//! 说明这件事，而不是继续静默降级。
//!
//! 需要改套件请走项目文件（`cpe-ui-project.json`）；需要跑老 config 请走
//! 命令行 `cpe_test master --config`——那条路 v6.0 零改动。

use super::*;

/// 一行矩阵勾选的回填值。`PairSelection` 只有 `Deserialize`——它是请求方向的
/// 类型，回填是相反方向，两者字段名必须一致但生命周期不同，分开写比给请求类型
/// 加一个只在这里用的 `Serialize` 更不容易在改动时互相带偏。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(super) struct PairImport {
    pub(super) src: String,
    pub(super) dst: String,
    pub(super) directions: Vec<String>,
    pub(super) rx_target_bidir_ab: String,
    pub(super) rx_target_bidir_ba: String,
    pub(super) udp_groups: Vec<usize>,
    pub(super) tcp_groups: Vec<usize>,
    pub(super) transports: Vec<String>,
    pub(super) ip: Vec<String>,
}

/// 导入时从 `tests[]` 里认出来的 UDP 参数组。字段和 `UdpGroup` 一致，
/// 方向相反（那个是请求，这个是回填）。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(super) struct UdpGroupOut {
    pub(super) name: String,
    pub(super) bandwidths: Vec<String>,
    pub(super) lengths: Vec<String>,
    pub(super) windows: Vec<String>,
    pub(super) streams: u32,
}

/// 导入时从 `tests[]` 里认出来的 TCP 参数组。字段和 `TcpGroup` 一致。
///
/// 一个 TCP 组会被 `config_from_request` 拆成好几条 TestSpec（每个 `-P` 一条，
/// 都带着这组的那份 `-w` 列表），所以回填时按「相同的 `-w` 列表」把它们并回一组，
/// 把各条的 `-P` 收成这一组的流数档位。两组恰好用同一份 `-w` 时会被并成一组，
/// 但跑出来的单元完全一样，不影响结果。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(super) struct TcpGroupOut {
    pub(super) name: String,
    pub(super) windows: Vec<String>,
    pub(super) streams: Vec<u32>,
}

#[derive(Debug, Serialize)]
pub(super) struct ImportOut {
    /// 顶部参数区，字段和 `/api/bootstrap` 完全一致——页面用同一段代码回填，
    /// 免得「导入」和「打开页面」两条路把同一个输入框填成两种样子。
    pub(super) settings: BootstrapOut,
    /// 这两项 `/api/bootstrap` 有意不回填（见 `RunRequest` 上的注释：不能让
    /// 同一个勾选框在不同机器上悄悄变成不同含义）。导入是人明确要求「按这份
    /// 文件来」，回填它们是对的，但要在 `notices` 里说一声。
    pub(super) limit_udp_by_link_speed: bool,
    pub(super) resume: bool,
    pub(super) pairs: Vec<PairImport>,
    /// 默认组之外的组；矩阵行上的 `udp_group` 按 1 起指向它们。
    pub(super) udp_groups: Vec<UdpGroupOut>,
    /// 默认组之外的 TCP 组；矩阵行上的 `tcp_group` 按 1 起指向它们。
    pub(super) tcp_groups: Vec<TcpGroupOut>,
    pub(super) nic_policies: Vec<NicPolicySelection>,
    /// 导入过程中丢掉或改写了什么。空列表 = 这份文件被完整表示了。
    pub(super) notices: Vec<String>,
}

/// 导入一份 config.json，回填成界面状态。
///
/// 「下载 config.json」一直是单向的：改完一堆门限和档位，下次打开控制台又得
/// 从头点一遍，而那份文件里明明什么都有。这里做的是它的逆运算——把 config
/// 翻回界面选择，**不执行任何东西**。
///
/// 有意不要求先连上辅测机：全局参数和网口策略不依赖连接，配对选择留给页面在
/// 连上之后按端点名匹配（对不上的行会在 `notices` 里点名）。
pub(super) fn api_import(console: &Arc<Console>, body: &str) -> Result<serde_json::Value, String> {
    let incoming: Config = serde_json::from_str(body)
        .map_err(|error| format!("这不是一份能解析的 config.json：{error}"))?;
    let problems = incoming.validate();
    if !problems.is_empty() {
        return Err(format!("配置项异常，已拒绝导入：{}", problems.join("；")));
    }

    let mut state = lock_recover(&console.state);
    let mut notices = Vec::new();
    // 连接身份单独处理：token 空着时保留当前值。下载下来的 config 里带着
    // agent_token，但人手写的那份多半没有——用文件里的空串把已经连上的
    // 令牌冲掉，表现是导入之后「连接」突然 401。
    if incoming.agent_token.trim().is_empty() && !state.cfg.agent_token.trim().is_empty() {
        notices.push("文件里没有 agent_token，沿用当前已加载的令牌。".into());
    } else {
        state.cfg.agent_token = incoming.agent_token.clone();
    }
    let agent_token = state.cfg.agent_token.clone();
    let master = state.master.clone();
    let agent = state.agent.clone();

    state.cfg = Config {
        agent_token,
        ..incoming
    };
    if !state.cfg.agent_host.trim().is_empty() {
        state.agent_host = state.cfg.agent_host.trim().to_string();
    }

    // A14 的最低补救：**套件信息在这条路上会被静默丢掉**。
    //
    // `ImportOut` 只回填矩阵态，而 `Config` 根本不承载套件——所以「用工作台搭好
    // 套件 → 下载 config → 再导回来」的往返，会把任务顺序、逐任务时长、验收目标
    // 全部降级成一张扁平矩阵，而在此之前**六条 notice 里一条都没提**。用户毫无
    // 提示地跑出了另一份东西。
    //
    // 这个模块的头注释还写着「两边必须互为逆运算」——对套件计划来说，那句话
    // 从来就不成立。`/api/import` 按 ADR-13 已经决定不再重建界面入口，这里
    // 至少要把损失说出来。
    if state
        .cfg
        .tests
        .iter()
        .any(|test| test.origin.is_some() || test.link_group.is_some())
    {
        notices.push(
            "这份配置是从「快速工作台」的套件计划导出来的：套件的任务顺序、逐任务时长和\
             验收目标在扁平 config.json 里表示不了，导入后只剩一张矩阵。要改套件请回工作台\
             用项目文件（cpe-ui-project.json），不要走 config.json 往返。"
                .into(),
        );
    }
    if state.cfg.pairs.is_some() || state.cfg.universal_params.is_some() {
        notices.push(
            "文件用的是 pairs/universal_params 自动配对，界面矩阵是逐对勾选的，表示不了；\
             全局参数已导入，配对请在矩阵里自己勾。"
                .into(),
        );
    }
    let connected = !master.interfaces.is_empty() && !agent.interfaces.is_empty();
    if !connected && !state.cfg.tests.is_empty() {
        notices.push("还没连上辅测机，配对选择先存着；点「连接」扫到网口后会自动勾上。".into());
    }
    // settings 要先算：逐对的 UDP 覆盖是「和全局不一样的那部分」，
    // 没有全局值就判不出哪些该回填到行上。
    let settings = bootstrap_out(&state);
    // 默认组 = 执行区那几个框。文件里和它不一样的 UDP 参数会被认成附加组。
    let default_group = UdpGroupOut {
        name: "默认".into(),
        bandwidths: settings.udp_bandwidths.clone(),
        lengths: settings.udp_lengths.clone(),
        windows: settings.udp_windows.clone(),
        streams: settings.udp_streams,
    };
    // TCP 默认组 = 执行区的 `-w` / `-P`；文件里和它不一样的 TCP 参数认成附加组。
    let default_tcp_group = TcpGroupOut {
        name: "默认".into(),
        windows: settings.tcp_windows.clone(),
        streams: settings.tcp_streams.clone(),
    };
    let (pairs, udp_groups, tcp_groups, pair_notices) = pairs_from_tests(
        &state.cfg,
        &master,
        &agent,
        &default_group,
        &default_tcp_group,
    );
    notices.extend(pair_notices);
    if state.cfg.limit_udp_by_link_speed || state.cfg.resume {
        notices.push("「按链路上限裁剪」和「resume」按文件里的值勾上了，跑之前确认一眼。".into());
    }

    let nic_policies = configured_nic_policies(&state.cfg, &master, &agent);
    serde_json::to_value(ImportOut {
        settings,
        udp_groups,
        tcp_groups,
        limit_udp_by_link_speed: state.cfg.limit_udp_by_link_speed,
        resume: state.cfg.resume,
        pairs,
        nic_policies,
        notices,
    })
    .map_err(|error| format!("回填界面失败: {error}"))
}

/// `tests[]` -> 矩阵行。
///
/// 一行矩阵会被 `config_from_request` 拆成好几条 TestSpec（TCP 的每个 `-P`
/// 档位一条、UDP 钉死/扫描各一条、ping 一条），所以这里按端点对合并回去，
/// 方向、协议、IP 版本取并集。
///
/// 反向的那条（`dst`/`src` 调过来写）合并进同一行并把方向对调：矩阵一行代表的
/// 是一对网口，A、B 谁在左边由界面的枚举顺序决定，不由文件决定。
pub(super) fn pairs_from_tests(
    cfg: &Config,
    master: &HostInfo,
    agent: &HostInfo,
    default_group: &UdpGroupOut,
    default_tcp_group: &TcpGroupOut,
) -> (
    Vec<PairImport>,
    Vec<UdpGroupOut>,
    Vec<TcpGroupOut>,
    Vec<String>,
) {
    let mut out: Vec<PairImport> = Vec::new();
    let mut groups: Vec<UdpGroupOut> = Vec::new();
    let mut tcp_groups: Vec<TcpGroupOut> = Vec::new();
    // 与 `out` 同序：每行按「相同 -w 列表」聚起它跑过的 TCP 档位（-w 列表 -> 各 -P）。
    let mut tcp_accum: Vec<std::collections::HashMap<Vec<String>, Vec<u32>>> = Vec::new();
    let mut notices = Vec::new();
    let mut ragged = false;
    let mut unresolved: Vec<String> = Vec::new();
    for test in &cfg.tests {
        let (Some(src), Some(dst)) = (
            canonical_endpoint(&test.src, master, agent),
            canonical_endpoint(&test.dst, master, agent),
        ) else {
            for raw in [&test.src, &test.dst] {
                if canonical_endpoint(raw, master, agent).is_none()
                    && !unresolved.iter().any(|seen| seen == raw)
                {
                    unresolved.push(raw.clone());
                }
            }
            continue;
        };
        let directions = test.direction.directions();
        let mut transports: Vec<String> = test
            .transports
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| t == "tcp" || t == "udp")
            .collect();
        let transports_have_udp = transports.iter().any(|t| t == "udp");
        let transports_have_tcp = transports.iter().any(|t| t == "tcp");
        // ping 在配置模型里挂在 kinds 上，界面把它和 TCP/UDP 并排放在「协议」
        // 列——回填时要走相反的那一步，否则纯 ping 的配置导进来是一行空协议。
        if test.kinds.iter().any(|kind| kind.trim() == "ping") {
            transports.push("ping".into());
        }
        let ip: Vec<String> = test
            .ip
            .iter()
            .map(|v| v.trim().to_lowercase())
            .filter(|v| v == "v4" || v == "v6")
            .collect();
        let bidir = test.rate_targets_bidir_mbps.clone().unwrap_or_default();

        let (idx, flip) =
            if let Some(idx) = out.iter().position(|row| row.src == src && row.dst == dst) {
                (idx, false)
            } else if let Some(idx) = out.iter().position(|row| row.src == dst && row.dst == src) {
                (idx, true)
            } else {
                out.push(PairImport {
                    src: src.clone(),
                    dst: dst.clone(),
                    ..Default::default()
                });
                (out.len() - 1, false)
            };
        // tcp_accum 与 out 对齐：新行出现就补一份空表（未解析的 test 在 idx
        // 之前就 continue 了，不会打乱对齐）。
        while tcp_accum.len() < out.len() {
            tcp_accum.push(std::collections::HashMap::new());
        }
        if transports_have_tcp {
            // 一条 TCP test 带着这一组的整份 -w 列表和它自己那一个 -P。手写配置
            // 可能没写 -w（None）——按默认组的窗口回填；-P 缺省按单流。
            let windows = test
                .tcp_windows
                .clone()
                .unwrap_or_else(|| default_tcp_group.windows.clone());
            let stream = test.tcp_streams.filter(|value| *value > 0).unwrap_or(1);
            let steps = tcp_accum[idx].entry(windows).or_default();
            if !steps.contains(&stream) {
                steps.push(stream);
            }
        }
        let row = &mut out[idx];
        for direction in directions {
            let direction = if flip {
                match direction.as_str() {
                    "ab" => "ba".to_string(),
                    "ba" => "ab".to_string(),
                    other => other.to_string(),
                }
            } else {
                direction
            };
            if !row.directions.contains(&direction) {
                row.directions.push(direction);
            }
        }
        for transport in transports {
            if !row.transports.contains(&transport) {
                row.transports.push(transport);
            }
        }
        for version in ip {
            if !row.ip.contains(&version) {
                row.ip.push(version);
            }
        }
        // 这条 test 的 UDP 参数和默认组一样吗？不一样就认成一个附加组，
        // 同样的参数只认一次（几十条 test 常常只有两三种打法）。
        //
        // 同一对可以有好几条 UDP test（一行选了多组），所以是**往这一行的组
        // 列表里加**，不是只认第一条。
        //
        // 发送端在 `by_nic` 里另有 `-b` 时跳过：那种情况下文件里的 profile 是
        // 占位值（见 `config_from_request` 的 pinned 分支），不是人填的选择。
        if transports_have_udp && !test_udp_all_directions_pinned(cfg, test, &src, &dst) {
            if let Some(profiles) = &test.udp_profiles {
                let (group, exact) = udp_group_from_profiles(
                    profiles,
                    test.udp_streams
                        .filter(|v| *v > 0)
                        .unwrap_or(default_group.streams),
                );
                ragged |= !exact;
                let selected = if group.bandwidths.is_empty() || group.same_run_as(default_group) {
                    0
                } else {
                    groups
                        .iter()
                        .position(|known| known.same_run_as(&group))
                        .unwrap_or_else(|| {
                            let mut named = group.clone();
                            named.name = format!("组 {}", groups.len() + 2);
                            groups.push(named);
                            groups.len() - 1
                        })
                        + 1
                };
                if !row.udp_groups.contains(&selected) {
                    row.udp_groups.push(selected);
                }
            }
        }

        let (ab, ba) = if flip {
            (bidir.ba, bidir.ab)
        } else {
            (bidir.ab, bidir.ba)
        };
        for (slot, value) in [
            (&mut row.rx_target_bidir_ab, ab),
            (&mut row.rx_target_bidir_ba, ba),
        ] {
            if let Some(value) = value.filter(|v| v.is_finite() && *v > 0.0) {
                if slot.is_empty() {
                    *slot = format_mbps(value);
                }
            }
        }
    }
    // 一条 UDP test 都没认出来的行（纯 TCP/ping，或者被网口值钉死的那种）
    // 明确写成「默认组」，别留一个空列表让页面去猜。
    for row in &mut out {
        if row.udp_groups.is_empty() {
            row.udp_groups.push(0);
        }
    }
    // TCP 组回填：按「相同 -w 列表」把同一行的 TCP test 并回一组，各条的 -P 收成
    // 这组的流数档位。两组恰好共用一份 -w 会被并成一组，但跑出来的单元一样。
    for (idx, accum) in tcp_accum.iter().enumerate() {
        // 稳定顺序：按 -w 列表排一下，免得每次导入组的编号乱跳。
        let mut entries: Vec<(&Vec<String>, &Vec<u32>)> = accum.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (windows, streams) in entries {
            let mut streams = streams.clone();
            streams.sort_unstable();
            let candidate = TcpGroupOut {
                name: String::new(),
                windows: windows.clone(),
                streams,
            };
            let selected = if candidate.same_run_as(default_tcp_group) {
                0
            } else {
                tcp_groups
                    .iter()
                    .position(|known| known.same_run_as(&candidate))
                    .unwrap_or_else(|| {
                        let mut named = candidate.clone();
                        named.name = format!("TCP 组 {}", tcp_groups.len() + 2);
                        tcp_groups.push(named);
                        tcp_groups.len() - 1
                    })
                    + 1
            };
            if !out[idx].tcp_groups.contains(&selected) {
                out[idx].tcp_groups.push(selected);
            }
        }
    }
    // 没认出任何 TCP test 的行（纯 UDP/ping）也写上默认组：矩阵行总有个选择。
    for row in &mut out {
        if row.tcp_groups.is_empty() {
            row.tcp_groups.push(0);
        }
    }
    if !unresolved.is_empty() {
        notices.push(format!(
            "这些端点在当前网口表里找不到，相关配对没有导入：{}",
            unresolved.join("、")
        ));
    }
    if ragged {
        notices.push(
            "文件里有 UDP 档位不是「每档 -b × 每档 -l × 每档 -w」的整齐组合（手写配置\
             常见）。参数组按三个轴各取一次去重来表示，导入后跑的档位会比文件里多；\
             要原样跑请直接 `master --auto --config 那个文件`。"
                .into(),
        );
    }
    if !tcp_groups.is_empty() {
        notices.push(format!(
            "文件里有 {} 组和默认组不同的 TCP 参数，已建成附加组并按行选好。",
            tcp_groups.len()
        ));
    }
    if !groups.is_empty() {
        notices.push(format!(
            "文件里有 {} 组和默认组不同的 UDP 参数，已建成附加组并按行选好。",
            groups.len()
        ));
    }
    (out, groups, tcp_groups, notices)
}

/// 一组 profile + 流数 -> 界面上的参数组。
///
/// 第二个返回值表示这份 profile 是不是一个整齐的叉积。界面上的组只能表达
/// 「每档 -b × 每档 -l × 每档 -w」，手写的配置可以不是那样（`1m/64` 加
/// `500m/1400`），那时按三个轴去重会**多**出组合，必须说出来。
pub(super) fn udp_group_from_profiles(
    profiles: &[UdpProfile],
    streams: u32,
) -> (UdpGroupOut, bool) {
    let bandwidths = distinct(profiles.iter().map(|profile| profile.bandwidth.clone()));
    let lengths = distinct(profiles.iter().filter_map(|profile| profile.length.clone()));
    let windows = distinct(profiles.iter().filter_map(|profile| profile.window.clone()));
    let combinations = bandwidths.len().max(1) * lengths.len().max(1) * windows.len().max(1);
    let exact = combinations == profiles.len();
    (
        UdpGroupOut {
            name: String::new(),
            bandwidths,
            lengths,
            windows,
            streams,
        },
        exact,
    )
}

impl UdpGroupOut {
    /// 两组会不会跑出同一批单元。名字不算——它只是给人看的。
    pub(super) fn same_run_as(&self, other: &UdpGroupOut) -> bool {
        self.bandwidths == other.bandwidths
            && self.lengths == other.lengths
            && self.windows == other.windows
            && self.streams == other.streams
    }
}

impl TcpGroupOut {
    /// 两组会不会跑出同一批单元。流数比之前先排序去重：回填时是从各条 -P
    /// 收集起来的，顺序和重复都可能和默认组那份不一样；-w 档位保持原序比较。
    pub(super) fn same_run_as(&self, other: &TcpGroupOut) -> bool {
        let norm = |values: &[u32]| {
            let mut out = values.to_vec();
            out.sort_unstable();
            out.dedup();
            out
        };
        self.windows == other.windows && norm(&self.streams) == norm(&other.streams)
    }
}

/// 把 config 里的端点写法统一成矩阵用的 `master:NAME=以太网 6`。
///
/// 没连上辅测机时解析不了 `master:SGMII2.5G` 这种按角色写的端点（角色到网卡
/// 名的映射来自实扫），但已经是 `NAME=` 写法的可以原样用——先连接再导入和
/// 先导入再连接都得能走通。
pub(super) fn canonical_endpoint(raw: &str, master: &HostInfo, agent: &HostInfo) -> Option<String> {
    if let Ok(endpoint) = builder::resolve_endpoint(raw, master, agent) {
        let side = match endpoint.side {
            builder::Side::Master => "master",
            builder::Side::Agent => "agent",
        };
        return Some(format!("{side}:NAME={}", endpoint.nic.name));
    }
    let (side, rest) = raw.split_once(':')?;
    let side = match side.trim().to_lowercase().as_str() {
        "master" | "local" | "主控" => "master",
        "agent" | "remote" | "辅测" => "agent",
        _ => return None,
    };
    let name = rest
        .trim()
        .strip_prefix("NAME=")
        .or_else(|| rest.trim().strip_prefix("name="))?
        .trim();
    (!name.is_empty()).then(|| format!("{side}:NAME={name}"))
}

/// 这个端点在「网口与策略」里单独指定了 UDP `-b` 吗。
///
/// 它决定文件里那条 test 的 profile 带宽是「人填的档位」还是「占位值」。
pub(super) fn endpoint_pins_udp_bandwidth(cfg: &Config, endpoint: &str) -> bool {
    let Some((host, rest)) = endpoint.split_once(':') else {
        return false;
    };
    let Some(name) = rest.strip_prefix("NAME=") else {
        return false;
    };
    cfg.link_profiles.by_nic.iter().any(|profile| {
        profile
            .udp_bandwidth
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
            && profile.host.eq_ignore_ascii_case(host)
            && profile.name.eq_ignore_ascii_case(name)
    })
}

/// Whether every UDP sending leg represented by a test is pinned to a
/// per-NIC bandwidth override.
///
/// A config generated from the matrix may split one logical pair into two UDP
/// tests when only one endpoint is pinned: the pinned direction carries a
/// placeholder profile, while the unpinned direction still carries the
/// user's sweep.  Treating the pair as pinned merely because *either*
/// endpoint has an override would make `api_import` discard that sweep and
/// silently turn the row back into the default UDP group.  Decide per test,
/// using its concrete directions, so only a test whose every sending leg is
/// pinned is ignored during group reconstruction.
pub(super) fn test_udp_all_directions_pinned(
    cfg: &Config,
    test: &TestSpec,
    src: &str,
    dst: &str,
) -> bool {
    let src_pinned = endpoint_pins_udp_bandwidth(cfg, src);
    let dst_pinned = endpoint_pins_udp_bandwidth(cfg, dst);
    let directions = test.direction.directions();
    !directions.is_empty()
        && directions.iter().all(|direction| match direction.as_str() {
            "ab" => src_pinned,
            "ba" => dst_pinned,
            "bidir" => src_pinned && dst_pinned,
            _ => false,
        })
}

/// 门限回填成人写得出来的样子：整数不带小数点，其余保留一位。
pub(super) fn format_mbps(value: f64) -> String {
    if (value.fract()).abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}
