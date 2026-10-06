//! 配置文件（config.json）加载。所有字段都真正生效。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 出厂默认口令，agent 认证与控制台访问共用。
///
/// 默认从「完全不认证」改成「一个固定口令」，挡的是**误连**不是攻击：
/// 同一段测试网里两套设备互相连错、扫描器顺手把 agent 点起来跑一轮，
/// 这类事故靠一个口令就能挡掉。
///
/// 它挡不住任何知道本工具的人——这个值写在源码、文档和发布包里，是公开的。
/// 本工具面向隔离测试网自用，按此取舍；要暴露到别的网段就用 `--token` /
/// `--ui-token` 换掉。
///
/// 和发布包里 `start_ui.bat` / `start_agent.bat` 的 `UI_TOKEN` / `AGENT_TOKEN`
/// 是同一个值：此前只有 .bat 带口令，直接跑 exe 完全不认证，同一个发布包里
/// 两条启动路径行为不一致。
pub const DEFAULT_TOKEN: &str = "cpetest";

/// agent 令牌最终会进入 HTTP `Authorization` 头；上限防止误填超长值把
/// 每次控制请求膨胀成无意义的大头部，控制字符则不能穿过 HTTP 头边界。
pub(crate) const MAX_AGENT_TOKEN_BYTES: usize = 4096;
pub(crate) const MAX_AGENT_ADDRESS_BYTES: usize = 256;

pub(crate) fn validate_agent_address_for_http(address: &str) -> Result<(), String> {
    if address.len() > MAX_AGENT_ADDRESS_BYTES {
        return Err(format!(
            "agent_host 超过 HTTP 主机地址上限（{} 字节）",
            MAX_AGENT_ADDRESS_BYTES
        ));
    }
    if address.chars().any(char::is_control) {
        return Err("agent_host 不能包含控制字符（含换行、回车或 NUL）".into());
    }
    Ok(())
}

pub(crate) fn validate_agent_token_for_http(token: &str) -> Result<(), String> {
    if token.len() > MAX_AGENT_TOKEN_BYTES {
        return Err(format!(
            "agent_token 超过 HTTP 令牌上限（{} 字节）",
            MAX_AGENT_TOKEN_BYTES
        ));
    }
    if token.chars().any(char::is_control) {
        return Err("agent_token 不能包含控制字符（含换行、回车或 NUL）".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 辅测机管理口 IP（留空则交互询问）；发往 agent 的 Host 头最多 256 字节，不能含控制字符。
    pub agent_host: String,
    pub agent_port: u16,
    /// 与辅测 agent 之间的共享访问令牌。默认 [`DEFAULT_TOKEN`]。
    ///
    /// 非空时 agent 要求所有请求携带 `Authorization: Bearer <token>`，
    /// 未认证请求返回 401 且不会创建任何资源。
    ///
    /// 显式写成空串才会关闭认证（仅建议完全隔离的测试网）。注意 serde 的
    /// `default` 只在**字段缺失**时生效：配置文件里写了 `"agent_token": ""`
    /// 就是明确要求关闭认证，不会回落到默认口令。
    /// 令牌最多 4096 字节且不能含控制字符，否则不会发出 HTTP 请求。
    #[serde(default)]
    pub agent_token: String,
    /// agent 监听地址；默认 0.0.0.0。可设为 127.0.0.1 或测试网卡 IP 收紧暴露面。
    #[serde(default = "default_agent_bind")]
    pub agent_bind: String,
    /// 测试子网 IPv4 前缀过滤
    pub ipv4_prefixes: Vec<String>,
    /// 跨机 iperf3/ctsTraffic 要求两端同 /24（历史字段名保持兼容；ping 不受限）
    pub require_same_subnet_for_iperf: bool,
    /// UDP 按整条路径的可信负载上限裁剪档位/流数。
    pub limit_udp_by_link_speed: bool,
    /// 每个吞吐任务结束后在涉及端截图
    pub screenshot: bool,
    /// 24 小时内已 PASS 的任务跳过
    pub resume: bool,
    /// 测试完自动打开 HTML 报告
    pub open_report: bool,
    /// 连续这么多个灌包单元一条测量都没产生时熔断。0 表示只告警不中止。
    ///
    /// **两层共用这一个阈值**（`master::executor::DeadTrafficBreaker`）：
    ///
    /// - 按链路：某条链路连续这么多个空跑 → 放弃**这条链路**的剩余单元，
    ///   其余链路照跑；
    /// - 全局：连续这么多个空跑（不分链路）→ 中止整个剩余队列。
    ///
    /// 默认 0 保持不变，但它当初那条理由已经被分组这一层解决了：「连续零测量」
    /// 以前区分不了「被测设备掉线」和「其中一对网口本来就不通」，于是自动中止
    /// 会把别的配对一起砍掉。现在前者由全局那一层接住、后者由链路那一层接住，
    /// 无人值守跑长队列时设成 2~3 才真正可用。
    pub abort_after_dead_traffic_units: usize,
    /// `runs/` 里最多保留多少轮历史；`0` = 不删（默认）。
    ///
    /// 一次 210 单元的全量跑带截图和逐样本 CSV 就是几百 MB、上千个文件。
    /// 日更回归跑一年，`runs/` 会长到几十 GB，而历史列表每次都要对每个目录
    /// 递归算一次字节数。
    ///
    /// **默认 0 是刻意的**：删数据必须由人显式选择。没有什么比「升级一次，
    /// 历史记录少了一半」更符合本仓库戒律里那个「在没人注意的情况下改变行为」。
    ///
    /// 清理只动**本工具自己写出来的形状**（`run_<数字与下划线>`），
    /// 且只按目录名排序取最旧的删——人手放进 `runs/` 的归档、备注、别的目录
    /// 一个都不碰。见 `report::retention`。
    #[serde(default)]
    pub keep_runs: usize,
    /// 整份计划重复跑多少遍（稳定性 / 拷机）。`1` = 跑一遍（默认，行为不变）。
    ///
    /// **轮次在最外层**：整套跑完再跑一遍。稳定性要回答的是「同一套用例连跑
    /// 20 遍，有没有哪一遍开始掉」；轮次放在最内层（同一个单元连跑 N 次）
    /// 测的是热衰减，而那个用一个更长的 `duration` 就够了。
    ///
    /// 每一轮有**独立的稳定身份**（`builder::round_scoped_id`），所以同一次运行里
    /// 各轮不会互相命中 RESUME——不区分的话第 2 轮会直接命中第 1 轮刚写进去的
    /// PASS 而整轮跳过，正好把这个功能本身取消掉。
    ///
    /// **第 1 轮的身份逐字节不变**：不加轮次的老计划跑出来和以前一模一样，
    /// 历史 `task_results.json` 的 RESUME 不受影响。
    ///
    /// 上限 100（`builder::MAX_ROUNDS`），防手滑——一次全量跑 11.5 小时，
    /// 输错一位就是一个月。
    ///
    /// # 为什么不叫 `repeats`
    ///
    /// 内环配置里已经有一个 `repeats`（`inner::config`），而两边的导入判别器
    /// 靠**键集零交集**来认「你把文件导错地方了」：
    /// `import::INNER_ONLY_KEYS` 里就有 `repeats`。子网这边再叫同一个名字，
    /// 导出的子网 config 会被内环判别器认成内环配置，反之亦然——那不是措辞
    /// 问题，是两个导入口互相拒收对方的文件。
    ///
    /// 语义上两者本来也不是一回事：内环的 `repeats` 在展开顺序的**最内层**
    /// （同一个单元连跑 N 次），子网的 `rounds` 在**最外层**（整套跑完再跑一遍）。
    /// 不同的东西用不同的名字是对的。守在
    /// `the_two_import_detectors_never_share_a_key`。
    #[serde(default = "default_rounds")]
    pub rounds: u32,
    /// 按角色配对 / 按单块网卡给出的 RX 门限与 UDP 带宽。
    pub link_profiles: LinkProfiles,
    pub iperf: IperfCfg,
    /// Windows 专用 ctsTraffic 后端的简化默认参数。
    pub ctstraffic: CtsTrafficCfg,
    pub ping: PingCfg,
    /// 自动配对生成测试：字符串 "all" 或具体角色对列表
    #[serde(default)]
    pub pairs: Option<Pairs>,
    /// pairs 模式下的统一测试参数
    #[serde(default)]
    pub universal_params: Option<UniversalParams>,
    pub tests: Vec<TestSpec>,
}

/// pairs 字段：可以是 "all" 字符串，也可以是角色对数组
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Pairs {
    All(String),
    List(Vec<PairSpec>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairSpec {
    /// master 侧的角色 或 NAME=接口名
    pub master: String,
    /// agent 侧的角色 或 NAME=接口名
    pub agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniversalParams {
    #[serde(default = "default_direction")]
    pub directions: OneOrMany,
    #[serde(default = "default_kinds")]
    pub kinds: Vec<String>,
    #[serde(default = "default_transports")]
    pub transports: Vec<String>,
    #[serde(default = "default_ip")]
    pub ip: Vec<String>,
    #[serde(default = "default_streams")]
    pub streams: u32,
    /// 可选：覆盖 streams 的 TCP 并发流数（0/缺省时沿用 streams）。
    #[serde(default)]
    pub tcp_streams: Option<u32>,
    /// 可选：覆盖 streams 的 UDP 并发流数（0/缺省时沿用 streams）。
    #[serde(default)]
    pub udp_streams: Option<u32>,
    /// 历史字段名；当前供 iperf3 与 ctsTraffic 共用。
    #[serde(default)]
    pub iperf_duration: Option<u64>,
    #[serde(default)]
    pub ping_count: Option<u32>,
    #[serde(default)]
    pub ping_payload_sizes: Option<Vec<u32>>,
    #[serde(default)]
    pub tcp_windows: Option<Vec<String>>,
    #[serde(default)]
    pub udp_profiles: Option<Vec<UdpProfile>>,
    /// auto / verify / observe / discover
    #[serde(default)]
    pub rate_mode: Option<RateMode>,
    /// 双向可分别配置 ab/ba；单向可用 forward。
    #[serde(default)]
    pub rate_targets_mbps: Option<RateTargets>,
    /// **单向**单元专用的接收门限，按方向分别配置（`ab` / `ba`）。
    ///
    /// 和 `rate_targets_bidir_mbps` 对称，同样排在**单口覆盖之前**，理由也一样：
    /// 它是唯一知道「这条腿是哪一对网口」的门限来源。按网口那张表只能给一块网卡
    /// 填一个数，而同一块 SGMII2.5G 口，对端是 1G 口和对端是 10G 口时能收到的
    /// 完全不是一个量级——1G 口做发送端时，收口上挂的 1800/2000 在这条路径上
    /// 物理上就跑不到。`rate_check.rx_target_link_speed_ratio` 那道封顶只能把它
    /// 压到线速的 95%（1G 口上是 950），压不出「这条链路该验收多少」。
    ///
    /// 留空 = 单向照旧走既有的兜底链（单口覆盖 → `rate_targets_mbps` → 内置推导），
    /// 老配置行为不变。
    #[serde(default)]
    pub rate_targets_single_mbps: Option<RateTargets>,
    /// **双向并发**单元专用的接收门限，按方向分别配置（`ab` / `ba`）。
    ///
    /// 双向同时灌包时，两个方向的吞吐**不是相互独立的**：一个方向多占，
    /// 另一个就少拿，每个方向拿到的只有单向时的一部分——拿单向门限去卡双向
    /// 必然判 `RATE_FAIL`，而那是配置出来的失败，不是测出来的。
    ///
    /// **按配对而不是按网卡**：同一块 RNDIS 口，和 Wi-Fi 组双向、和 SGMII 组
    /// 双向，能拿到的接收速率完全不是一个量级；门限挂在网卡上只能填一个数，
    /// 必然有一组是错的。受限的是这条链路，不是某一端的网卡。
    ///
    /// 留空 = 双向也走既有的兜底链（单口覆盖 → `rate_targets_mbps` → 内置推导），
    /// 老配置行为不变。
    #[serde(default)]
    pub rate_targets_bidir_mbps: Option<RateTargets>,

    /// **双向并发**单元的「两端 RX 合计」门限。
    ///
    /// Wi-Fi↔Wi-Fi 上 AB 和 BA 的吞吐互相影响，不是相互独立的两个数：
    /// 两个方向能分到多少取决于调度，**没有理由要求各自达到一半**。
    /// 用户要验收的是这条链路在双向并发下总共还能过多少数据，所以口径是
    ///
    /// ```text
    /// 双向有效吞吐 = AB 方向接收端 RX 平均 + BA 方向接收端 RX 平均
    /// ```
    ///
    /// 用两端的 **RX** 相加而不是 TX+RX：同一个包在发送侧 TX 和接收侧 RX 各
    /// 记一次，相加就是重复计数；TX 还会混进背景流量和 socket 缓冲里从未上线
    /// 的字节。
    ///
    /// 配了它，这个双向单元就**只按合计判定**：两条腿各自只测量（`MEASURED`），
    /// 单元级比一次合计。留空则完全走既有的每方向门限链路，非 Wi-Fi 互测的场景
    /// 一个字节都没变。
    #[serde(default)]
    pub rate_target_bidir_total_mbps: Option<f64>,
}

fn default_rounds() -> u32 {
    1
}

fn default_agent_bind() -> String {
    "0.0.0.0".into()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            agent_host: String::new(),
            agent_port: 28801,
            agent_token: DEFAULT_TOKEN.into(),
            agent_bind: default_agent_bind(),
            ipv4_prefixes: vec!["192.168.".into()],
            require_same_subnet_for_iperf: true,
            limit_udp_by_link_speed: true,
            screenshot: true,
            resume: false,
            open_report: true,
            abort_after_dead_traffic_units: 0,
            keep_runs: 0,
            rounds: default_rounds(),
            link_profiles: LinkProfiles::default(),
            iperf: IperfCfg::default(),
            ctstraffic: CtsTrafficCfg::default(),
            ping: PingCfg::default(),
            pairs: None,
            universal_params: None,
            tests: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CtsTrafficCfg {
    /// ctsTraffic UDP MediaStream 每秒媒体帧数；每帧再拆成 datagram。
    pub udp_frame_rate: u32,
    /// UDP client 应用层缓冲深度（秒），不是 socket buffer。
    pub udp_buffer_depth_secs: u32,
    /// 控制台聚合状态输出周期（毫秒）。
    pub status_update_ms: u32,
}

impl Default for CtsTrafficCfg {
    fn default() -> Self {
        Self {
            udp_frame_rate: 100,
            udp_buffer_depth_secs: 1,
            status_update_ms: 1_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IperfCfg {
    /// 两种吞吐后端共用的全局默认灌包秒数（历史上位于 iperf 节点）
    pub duration: u64,
    /// TCP window 档位
    pub tcp_windows: Vec<String>,
    /// UDP 带宽档位
    pub udp_profiles: Vec<UdpProfile>,
    pub rate_check: RateCheckCfg,
}

impl Default for IperfCfg {
    fn default() -> Self {
        IperfCfg {
            duration: 180,
            tcp_windows: vec!["64k".into(), "1m".into(), "4m".into()],
            udp_profiles: vec![
                UdpProfile::bw("1m"),
                UdpProfile::bw("100m"),
                UdpProfile::bw("500m"),
                UdpProfile {
                    bandwidth: "1000m".into(),
                    length: Some("64".into()),
                    window: None,
                },
                UdpProfile::bw("2500m"),
            ],
            rate_check: RateCheckCfg::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateMode {
    #[default]
    Auto,
    Verify,
    Observe,
    Discover,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RateTargets {
    pub forward: Option<f64>,
    pub ab: Option<f64>,
    pub ba: Option<f64>,
}

impl RateTargets {
    pub fn for_direction(&self, direction: &str) -> Option<f64> {
        match direction {
            "ab" => self.ab.or(self.forward),
            "ba" => self.ba.or(self.forward),
            _ => self.forward,
        }
        .filter(|v| v.is_finite() && *v > 0.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateCheckCfg {
    pub mode: RateMode,
    pub targets_mbps: RateTargets,
    pub sample_interval_ms: u64,
    pub background_secs: u64,
    pub startup_timeout_secs: u64,
    pub settle_secs: u64,
    pub launch_interval_ms: u64,
    pub min_concurrent_streams: u32,
    pub min_active_ratio: f64,
    pub offered_headroom_pct: f64,
    /// UDP 完整 server/client 额外尝试预算；单流/单连接每方向总尝试数至少为 3。
    pub flow_retries: u32,
    pub discovery_step_secs: u64,
    /// EVB 10GUSB/NCM -> 10GETH 的已知接收目标。
    /// 兼容旧字段 evb_usb_tx_target_mbps（以 USB 发送方向命名）。
    #[serde(alias = "evb_usb_tx_target_mbps")]
    pub evb_usb_to_eth_target_mbps: f64,
    /// EVB 10GETH -> 10GUSB/NCM 的已知接收目标。
    /// 兼容旧字段 evb_usb_rx_target_mbps（以 USB 接收方向命名）。
    #[serde(alias = "evb_usb_rx_target_mbps")]
    pub evb_eth_to_usb_target_mbps: f64,
    /// SGMII2.5G（以及同量级的受限 CPE 子网口）的负载上限，不直接作为 PASS 目标。
    ///
    /// 默认 2600 而不是协商速率 2500：这类口的常规档位就是 `-b 2.6G`，上限压在
    /// 2500 会把每一轮常规灌包都裁一刀，而「裁剪」本意是拦住离谱值、不是修正
    /// 正常量级。和 Wi-Fi 那档 2882「取在 PHY 峰值上」是同一个用意。
    ///
    /// RNDIS 不再走这一档（它跟协商速率，见 `rate::nic_payload_ceiling_mbps`）。
    pub cpe_path_ceiling_mbps: f64,
    /// WiFi 网卡的负载上限，**不跟随协商速率**。
    ///
    /// WiFi 的「协商速率」是 PHY 速率，既不等于可用载荷，也会随信道条件在
    /// 一轮测试里反复跳（同一块 Wi-Fi 7 网卡会在 2402 / 2882 之间来回）。
    /// 拿它去裁 UDP 的 -b，等于让灌包强度跟着一个抖动的数字走，
    /// 前后两个单元的测试条件都不一样。
    ///
    /// 实践中 WiFi 一律按同一档灌（例如无论协商到 2.4G 还是 2.8G 都用
    /// -b 2.6G），所以这里给一个固定值。默认 2882 取的就是上面那个 PHY 峰值
    /// ——这条线的作用是拦住明显超出网卡物理能力的配置，取在峰值上，常规档位
    /// `-b 2.6G` 连带它上下的调整余地都不会被裁到。
    pub wifi_payload_ceiling_mbps: f64,
    /// 2.4GHz Wi-Fi 的负载上限，同样**不跟协商速率**。
    ///
    /// 必须和 5G/6G 分开：2.4GHz 只有 3 个不重叠信道、最多 40MHz 带宽，
    /// 和 5G 共用 2882 等于对 2.4G 口完全不裁剪，把 5G 档的 `-b 2.6G` 原样
    /// 丢给 2.4G 口，包必然大部分丢在空口上——那是配置出来的丢包，不是测出来的。
    ///
    /// 默认取 802.11ax 2SS 在 2.4GHz 的 PHY 峰值 574Mbps。**这是一条挡离谱值的线，
    /// 不是贴近可用载荷的线**：实际可用载荷明显低于 574，所以这个上限不会裁掉
    /// 正常量级的灌包，只拦住明显超出这个频段物理能力的配置。要按某条链路的
    /// 实际能力裁，在 `link_profiles` 里给那块网卡明确配 `-b`——明确配过的链路
    /// 不受本上限影响，那是操作者的判断，安全网不该推翻它。
    pub wifi_24g_payload_ceiling_mbps: f64,
    /// RX 门限相对**协商速率**的封顶系数；0 或负数 = 关掉这道封顶。
    ///
    /// 门限一直只看接收端（`rate::resolve_link_policy` 的「门限看接收端，
    /// 带宽看发送端」），于是发送口比接收口慢的组合会拿到一个物理上跑不到的
    /// 门限：run_20260905_125327_5940 里 `以太网 6`（协商 1000Mbps）做发送口、
    /// 门限取的是收口策略的 1800/2000，16 个单元实测 934~984（就是 1G 线速）
    /// 全判 RATE_FAIL——那是配置错误，不是设备缺陷。
    ///
    /// 封顶只能用**协商速率**，不能用 `nic_payload_ceiling_mbps`：后者对 Wi-Fi
    /// 返回的是固定档位（2882），拿它封顶会把 Wi-Fi 口上合理的高门限一起压低，
    /// 把真实不达标洗成 PASS。协商速率则永远是可用载荷的上界——它只可能删掉
    /// 本来就达不到的门限，不可能放过任何一条真的没跑到的链路。
    ///
    /// 默认 0.95：GbE 上 1518/1538 的成帧开销约 1.3%，再留一点余量。取值偏
    /// 保守是有意的——这条线的作用是拦住「门限比线速还高」，不是替操作者
    /// 决定「跑到线速的百分之几才算合格」。
    pub rx_target_link_speed_ratio: f64,
    pub max_udp_loss_pct: Option<f64>,
}

impl Default for RateCheckCfg {
    fn default() -> Self {
        Self {
            mode: RateMode::Auto,
            targets_mbps: RateTargets::default(),
            sample_interval_ms: 1000,
            background_secs: 3,
            startup_timeout_secs: 15,
            settle_secs: 5,
            launch_interval_ms: 50,
            min_concurrent_streams: 2,
            min_active_ratio: 0.90,
            offered_headroom_pct: 5.0,
            flow_retries: 1,
            discovery_step_secs: 10,
            evb_usb_to_eth_target_mbps: 6400.0,
            evb_eth_to_usb_target_mbps: 8400.0,
            cpe_path_ceiling_mbps: 2600.0,
            wifi_payload_ceiling_mbps: 2882.0,
            wifi_24g_payload_ceiling_mbps: 574.0,
            rx_target_link_speed_ratio: 0.95,
            max_udp_loss_pct: None,
        }
    }
}

/// 按方向给出的 UDP 单流带宽，形状与 `RateTargets` 一致。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DirectionalBandwidth {
    pub forward: Option<String>,
    pub ab: Option<String>,
    pub ba: Option<String>,
}

impl DirectionalBandwidth {
    pub fn for_direction(&self, direction: &str) -> Option<&str> {
        match direction {
            "ba" => self.ba.as_deref().or(self.forward.as_deref()),
            _ => self.ab.as_deref().or(self.forward.as_deref()),
        }
    }
}

/// 一条**角色配对**的策略，例如 `SGMII2.5G<->WIFI5G`。
///
/// 配对串左边是 A、右边是 B，`ab` / `ba` 相对这个顺序解释，与运行时某个
/// 单元自己的 A/B 无关——同一条物理链路在不同单元里可能正反着排。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RoleProfile {
    /// `角色A<->角色B`
    pub pair: String,
    pub rx_target_mbps: RateTargets,
    pub udp_bandwidth: DirectionalBandwidth,
}

/// 单块网卡的覆盖项。同一角色的两块网卡实测能力可以差很多
/// （Wi-Fi 7 BE200 和普通 5G 网卡都归 `WIFI5G`），角色层给默认值，
/// 这一层给例外。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NicProfile {
    /// `master` / `agent`
    pub host: String,
    /// 接口名，与网卡扫描里显示的一致
    pub name: String,
    /// 可选：同名接口有歧义时再用 IPv4 收窄
    pub ipv4: String,
    /// 作为**接收端**时的门限，绝对值（Mbps）。与 `rx_target_percent` 二选一，
    /// 两个都填时以绝对值为准。
    pub rx_target_mbps: Option<f64>,
    /// 作为**接收端**时的门限，按这块网卡**协商速率**的百分比（`90` = 90%）。
    ///
    /// 换算用的是每个单元开跑前重扫到的协商速率，所以 Wi-Fi 这类会重新协商的
    /// 口上，门限会跟着变。这是刻意的——按百分比要的就是「相对这条链路当前
    /// 能力」的判据；但换算结果必须在计划提示里说出来，否则同一份配置两次跑出
    /// 不同门限会没人看得懂。
    pub rx_target_percent: Option<f64>,
    /// 作为**发送端**时的 UDP 单流带宽
    pub udp_bandwidth: Option<String>,
    /// 作为**发送端**时的 UDP 报文长度（`-l`）。覆盖档位里的 `length`。
    pub udp_length: Option<String>,
}

/// 两层链路策略：角色兜底 + 单口覆盖。
///
/// 不配置时整个节点为空，全部走既有的内置推导，老配置行为不变。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LinkProfiles {
    pub by_role: Vec<RoleProfile>,
    pub by_nic: Vec<NicProfile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UdpProfile {
    pub bandwidth: String,
    #[serde(default)]
    pub length: Option<String>,
    /// iperf3 UDP socket buffer（`-w`）；省略时保持旧配置行为。
    #[serde(default)]
    pub window: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ParsedBandwidth {
    pub mbps: f64,
    pub bits_per_second: u64,
}

impl UdpProfile {
    pub fn bw(b: &str) -> Self {
        UdpProfile {
            bandwidth: b.into(),
            length: None,
            window: None,
        }
    }

    /// 严格解析完整带宽字符串。支持十进制数值加 `k/m/g` 或
    /// `kbps/mbps/gbps`（大小写不敏感），逗号也可作小数点；裸数为
    /// 兼容旧配置仍按 Mbps 解释。
    pub(crate) fn parsed_bandwidth(&self) -> Result<ParsedBandwidth, String> {
        let raw = self.bandwidth.trim();
        let lower = raw.to_ascii_lowercase();
        let (number, bps_multiplier) = [
            ("kbps", 1_000.0),
            ("mbps", 1_000_000.0),
            ("gbps", 1_000_000_000.0),
            ("k", 1_000.0),
            ("m", 1_000_000.0),
            ("g", 1_000_000_000.0),
        ]
        .into_iter()
        .find_map(|(suffix, multiplier)| {
            lower
                .strip_suffix(suffix)
                .map(|number| (number, multiplier))
        })
        .unwrap_or((lower.as_str(), 1_000_000.0));

        let mut separator_seen = false;
        let mut digits_before_separator = 0usize;
        let mut digits_after_separator = 0usize;
        for byte in number.bytes() {
            if byte.is_ascii_digit() {
                if separator_seen {
                    digits_after_separator += 1;
                } else {
                    digits_before_separator += 1;
                }
            } else if matches!(byte, b'.' | b',') && !separator_seen {
                separator_seen = true;
            } else {
                return Err(format!("无法解析 UDP 带宽 {}", self.bandwidth));
            }
        }
        if digits_before_separator == 0 || (separator_seen && digits_after_separator == 0) {
            return Err(format!("无法解析 UDP 带宽 {}", self.bandwidth));
        }

        let number = number.replace(',', ".");
        let value = number
            .parse::<f64>()
            .map_err(|_| format!("无法解析 UDP 带宽 {}", self.bandwidth))?;
        let bps = value * bps_multiplier;
        let rounded_bps = bps.round();
        // `u64::MAX as f64` 会舍入为 2^64；必须在转换前拒绝等于该
        // 边界的值，否则 `as u64` 会饱和成一个并非用户所写的速率。
        if !rounded_bps.is_finite() || rounded_bps < 1.0 || rounded_bps >= u64::MAX as f64 {
            return Err(format!("UDP 带宽超出有效范围: {}", self.bandwidth));
        }

        let bits_per_second = rounded_bps as u64;
        Ok(ParsedBandwidth {
            // 规划流数、报告 offered rate 与命令参数都基于同一个整数 bps，
            // 避免小数边界造成三者不一致。
            mbps: bits_per_second as f64 / 1_000_000.0,
            bits_per_second,
        })
    }

    pub fn name(&self) -> String {
        let mut name = format!("udp_b{}", self.bandwidth);
        if let Some(length) = &self.length {
            name.push_str(&format!("_l{length}"));
        }
        if let Some(window) = &self.window {
            name.push_str(&format!("_w{window}"));
        }
        name
    }

    pub fn label(&self) -> String {
        let mut label = format!("UDP -b {}", self.bandwidth);
        if let Some(length) = &self.length {
            label.push_str(&format!(" -l {length}"));
        }
        if let Some(window) = &self.window {
            label.push_str(&format!(" -w {window}"));
        }
        label
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PingCfg {
    pub count: u32,
    pub payload_sizes: Vec<u32>,
    /// 灌包**正在跑的时候**并发一条低速 ICMP 探针，测「负载下时延」。
    ///
    /// 在此之前 ping 与灌包是互斥的两种腿、单元之间顺序执行，于是 ping 测到的
    /// 永远是**空载** RTT。空载 0.4ms 的设备满载可能是 300ms——差两个数量级，
    /// 而用户感知到的「卡」几乎全部落在后者。
    ///
    /// **默认关**是刻意的：打开它会在每一轮吞吐测试期间多跑一个 ping 子进程，
    /// 也就是改变了测量条件。本仓库对「在没人注意的情况下改动基线」有明确戒律
    /// （见 AGENTS.md 里那条不许「修」内置预设的说明），所以这个新行为必须由人
    /// 显式打开，而不是升级一次就悄悄生效。
    ///
    /// 打开后的开销可以忽略：32 字节、约 1 秒一拍，整段合计不到 3 kbps。
    /// 结果**只进诊断**，绝不参与判定（ADR-17）。
    #[serde(default)]
    pub probe_during_traffic: bool,
    /// 每个 ping 单元额外做一次**路径 MTU 探测**（带「不分片」位二分逼近）。
    ///
    /// 现有的包长档位（32 / 1600 / 65500）测的是**分片行为**：不带 DF 位，
    /// 超长的包会被拆开发过去，照样通。而 1500 与 1492 的差别（PPPoE 封装）
    /// 是 CPE 桥接场景里最常见的一类现场故障，那需要 DF 位才看得见。
    ///
    /// **结果只进诊断，不做判定项**——这是刻意的：判定层现在只有一个权威
    /// （接收端 RX 平均对门限，ADR-17），给路径 MTU 开第二条判定路径就是再造
    /// 一个「说了算的地方」。要不要拿 1492 当不合格，是业务判断，由看报告的人定。
    ///
    /// 默认关，理由同 `probe_during_traffic`：它给每个 ping 单元多加十几轮探测，
    /// 改变了这一轮要跑多久。对端 agent 不支持时探测不跑，诊断里写明原因。
    #[serde(default)]
    pub probe_path_mtu: bool,
    /// payload <= 此值时归为 small。
    pub small_max_bytes: u32,
    /// payload <= 此值时归为 medium；再大归为 large。
    pub medium_max_bytes: u32,
    /// 兼容旧配置字段；现在表示“纯有线 + small”档最大 RTT。
    pub max_rtt_ms: f64,
    pub wired_small_avg_rtt_ms: f64,
    pub wired_medium_avg_rtt_ms: f64,
    pub wired_medium_max_rtt_ms: f64,
    pub wired_large_avg_rtt_ms: f64,
    pub wired_large_max_rtt_ms: f64,
    pub wifi_small_avg_rtt_ms: f64,
    pub wifi_small_max_rtt_ms: f64,
    pub wifi_medium_avg_rtt_ms: f64,
    pub wifi_medium_max_rtt_ms: f64,
    pub wifi_large_avg_rtt_ms: f64,
    pub wifi_large_max_rtt_ms: f64,
}

impl Default for PingCfg {
    fn default() -> Self {
        PingCfg {
            count: 180,
            payload_sizes: vec![32, 1600, 65500],
            probe_during_traffic: false,
            probe_path_mtu: false,
            small_max_bytes: 128,
            medium_max_bytes: 2000,
            max_rtt_ms: 30.0,
            wired_small_avg_rtt_ms: 10.0,
            wired_medium_avg_rtt_ms: 20.0,
            wired_medium_max_rtt_ms: 50.0,
            wired_large_avg_rtt_ms: 50.0,
            wired_large_max_rtt_ms: 100.0,
            wifi_small_avg_rtt_ms: 30.0,
            wifi_small_max_rtt_ms: 80.0,
            wifi_medium_avg_rtt_ms: 50.0,
            wifi_medium_max_rtt_ms: 100.0,
            wifi_large_avg_rtt_ms: 100.0,
            wifi_large_max_rtt_ms: 200.0,
        }
    }
}

/// 单个测试项（config.json 的 tests[]）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSpec {
    #[serde(default)]
    pub name: String,
    /// "master:SGMII2.5G" / "agent:WIFI5G" / "master:NAME=以太网 2"
    pub src: String,
    pub dst: String,
    /// "A->B" / "B->A" / "bidir" / "both"(旧值,展开为前两个)；可以是字符串或数组
    #[serde(default = "default_direction")]
    pub direction: OneOrMany,
    /// ["iperf","ctstraffic","ping"]，可任选或组合
    #[serde(default = "default_kinds")]
    pub kinds: Vec<String>,
    /// ["tcp","udp"]
    #[serde(default = "default_transports")]
    pub transports: Vec<String>,
    /// ["v4","v6"]
    #[serde(default = "default_ip")]
    pub ip: Vec<String>,
    #[serde(default = "default_streams")]
    pub streams: u32,
    /// 可选：覆盖 streams 的 TCP 并发流数（0/缺省时沿用 streams）。
    #[serde(default)]
    pub tcp_streams: Option<u32>,
    /// 可选：覆盖 streams 的 UDP 并发流数（0/缺省时沿用 streams）。
    #[serde(default)]
    pub udp_streams: Option<u32>,
    /// 历史字段名；当前供 iperf3 与 ctsTraffic 共用。
    #[serde(default)]
    pub iperf_duration: Option<u64>,
    #[serde(default)]
    pub ping_count: Option<u32>,
    #[serde(default)]
    pub ping_payload_sizes: Option<Vec<u32>>,
    #[serde(default)]
    pub tcp_windows: Option<Vec<String>>,
    #[serde(default)]
    pub udp_profiles: Option<Vec<UdpProfile>>,
    #[serde(default)]
    pub rate_mode: Option<RateMode>,
    #[serde(default)]
    pub rate_targets_mbps: Option<RateTargets>,
    /// **单向**单元专用的接收门限，按方向分别配置（`ab` / `ba`）。
    ///
    /// 和 `rate_targets_bidir_mbps` 对称，同样排在**单口覆盖之前**，理由也一样：
    /// 它是唯一知道「这条腿是哪一对网口」的门限来源。按网口那张表只能给一块网卡
    /// 填一个数，而同一块 SGMII2.5G 口，对端是 1G 口和对端是 10G 口时能收到的
    /// 完全不是一个量级——1G 口做发送端时，收口上挂的 1800/2000 在这条路径上
    /// 物理上就跑不到。`rate_check.rx_target_link_speed_ratio` 那道封顶只能把它
    /// 压到线速的 95%（1G 口上是 950），压不出「这条链路该验收多少」。
    ///
    /// 留空 = 单向照旧走既有的兜底链（单口覆盖 → `rate_targets_mbps` → 内置推导），
    /// 老配置行为不变。
    #[serde(default)]
    pub rate_targets_single_mbps: Option<RateTargets>,
    /// **双向并发**单元专用的接收门限，按方向分别配置（`ab` / `ba`）。
    ///
    /// 双向同时灌包时，两个方向的吞吐**不是相互独立的**：一个方向多占，
    /// 另一个就少拿，每个方向拿到的只有单向时的一部分——拿单向门限去卡双向
    /// 必然判 `RATE_FAIL`，而那是配置出来的失败，不是测出来的。
    ///
    /// **按配对而不是按网卡**：同一块 RNDIS 口，和 Wi-Fi 组双向、和 SGMII 组
    /// 双向，能拿到的接收速率完全不是一个量级；门限挂在网卡上只能填一个数，
    /// 必然有一组是错的。受限的是这条链路，不是某一端的网卡。
    ///
    /// 留空 = 双向也走既有的兜底链（单口覆盖 → `rate_targets_mbps` → 内置推导），
    /// 老配置行为不变。
    #[serde(default)]
    pub rate_targets_bidir_mbps: Option<RateTargets>,

    /// **双向并发**单元的「两端 RX 合计」门限。
    ///
    /// Wi-Fi↔Wi-Fi 上 AB 和 BA 的吞吐互相影响，不是相互独立的两个数：
    /// 两个方向能分到多少取决于调度，**没有理由要求各自达到一半**。
    /// 用户要验收的是这条链路在双向并发下总共还能过多少数据，所以口径是
    ///
    /// ```text
    /// 双向有效吞吐 = AB 方向接收端 RX 平均 + BA 方向接收端 RX 平均
    /// ```
    ///
    /// 用两端的 **RX** 相加而不是 TX+RX：同一个包在发送侧 TX 和接收侧 RX 各
    /// 记一次，相加就是重复计数；TX 还会混进背景流量和 socket 缓冲里从未上线
    /// 的字节。
    ///
    /// 配了它，这个双向单元就**只按合计判定**：两条腿各自只测量（`MEASURED`），
    /// 单元级比一次合计。留空则完全走既有的每方向门限链路，非 Wi-Fi 互测的场景
    /// 一个字节都没变。
    #[serde(default)]
    pub rate_target_bidir_total_mbps: Option<f64>,

    /// 报表分组键：这条测试属于哪一组链路。
    ///
    /// 取值优先级（界面填的那一份）：链路集合名 → 物理网口对 → `role_a ↔ role_b`。
    /// **永不用主机名**——Arch 机自报 `UNKNOWN-PC`，拿它当分组键会把一整批链路
    /// 并成一组。空表示没有分组信息，报表回落到按端点显示。
    #[serde(default)]
    pub link_group: Option<String>,

    /// 界面计划的溯源标注。
    ///
    /// 只进 trace 与报表分组，**不进 resume identity、不进判定**。
    /// 在此之前这些信息是 URL 编码进 `name` 的（`ui-plan/set/binding/...` 七段），
    /// 那是整条计划链路上唯一的 stringly 侧信道，靠约定不靠类型。
    #[serde(default)]
    pub origin: Option<UiOrigin>,
}

/// 界面计划的溯源标注。见 `TestSpec::origin`。
///
/// 字段全部是稳定 id：界面重排卡片、改名字都不影响它们，所以拿它做 trace
/// 重建和报表分组都不会在用户改个名字之后错位。`link_set_name` 是例外——
/// 它是给人看的（也是 `link_group` 的来源），会跟着用户改名走。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiOrigin {
    #[serde(default)]
    pub pair_id: String,
    #[serde(default)]
    pub link_set_id: String,
    #[serde(default)]
    pub link_set_name: String,
    #[serde(default)]
    pub binding_id: String,
    #[serde(default)]
    pub suite_id: String,
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub recipe_id: String,
}

impl UiOrigin {
    /// 七个字段全空 = 这条 spec 不是从界面来的（命令行 / pairs 预设）。
    pub fn is_empty(&self) -> bool {
        self.pair_id.is_empty()
            && self.link_set_id.is_empty()
            && self.link_set_name.is_empty()
            && self.binding_id.is_empty()
            && self.suite_id.is_empty()
            && self.task_id.is_empty()
            && self.recipe_id.is_empty()
    }
}

fn default_direction() -> OneOrMany {
    OneOrMany::One("A->B".into())
}
fn default_kinds() -> Vec<String> {
    vec!["iperf".into()]
}
fn default_transports() -> Vec<String> {
    vec!["tcp".into()]
}
fn default_ip() -> Vec<String> {
    vec!["v4".into()]
}
fn default_streams() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    /// 展开为规范方向列表：ab / ba / bidir（去重保序）
    pub fn directions(&self) -> Vec<String> {
        let raw: Vec<String> = match self {
            OneOrMany::One(s) => vec![s.clone()],
            OneOrMany::Many(v) => v.clone(),
        };
        let mut out: Vec<String> = Vec::new();
        for r in raw {
            let n = r.trim().to_uppercase();
            let mapped: Vec<&str> = match n.as_str() {
                "A->B" | "AB" | "A>B" => vec!["ab"],
                "B->A" | "BA" | "B>A" => vec!["ba"],
                "BIDIR" | "A<->B" | "双向" => vec!["bidir"],
                "BOTH" => vec!["ab", "ba"],
                _ => vec![],
            };
            for m in mapped {
                if !out.iter().any(|x| x == m) {
                    out.push(m.to_string());
                }
            }
        }
        if out.is_empty() {
            out.push("ab".into());
        }
        out
    }
}

/// 加载配置：--config 指定 > ./config.json > 程序同目录 config.json > 默认
impl Config {
    /// 加载后的取值校验。
    ///
    /// 这些字段大多在使用点各自 clamp 过，但有几个一旦写错只会让**每一个**
    /// 吞吐单元静默变成 NOT_EVALUATED，报告里只看得到「有效窗口不足」之类的
    /// 结果码，完全指不到是配置写错了。宁可在启动时直接报出来。
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let rc = &self.iperf.rate_check;
        let duration = self.iperf.duration;

        if let Err(problem) = validate_agent_address_for_http(&self.agent_host) {
            problems.push(problem);
        }
        if let Err(problem) = validate_agent_token_for_http(&self.agent_token) {
            problems.push(problem);
        }

        if self.agent_port == 0 {
            problems.push("agent_port 必须在 1..=65535 之间，不能为 0".into());
        }
        if duration == 0 {
            problems.push("iperf.duration 为 0：不会产生任何有效测量窗口".into());
        } else if rc.settle_secs >= duration {
            problems.push(format!(
                "iperf.rate_check.settle_secs={} 不小于 iperf.duration={}：每个灌包单元先丢掉的\
                 稳定等待比要计分的时长还长，进程会白跑这么久，多半是写错了",
                rc.settle_secs, duration
            ));
        }
        if rc.background_secs.saturating_add(rc.settle_secs) >= duration && duration > 0 {
            problems.push(format!(
                "iperf.rate_check.background_secs={} + settle_secs={} 不小于 duration={}：\
                 UDP 单元不计分的基线采样与稳定等待比计分时长还长，多半是写错了",
                rc.background_secs, rc.settle_secs, duration
            ));
        }
        if !(0.0..=1.0).contains(&rc.min_active_ratio) || !rc.min_active_ratio.is_finite() {
            problems.push(format!(
                "iperf.rate_check.min_active_ratio={} 超出 [0, 1]",
                rc.min_active_ratio
            ));
        }
        if !rc.offered_headroom_pct.is_finite() || rc.offered_headroom_pct < 0.0 {
            problems.push(format!(
                "iperf.rate_check.offered_headroom_pct={} 必须是非负有限值",
                rc.offered_headroom_pct
            ));
        }
        if rc.discovery_step_secs > 0 && rc.discovery_step_secs > duration {
            problems.push(format!(
                "iperf.rate_check.discovery_step_secs={} 大于 duration={}：discover 阶梯排到\
                 测试结束之后，最后几档流永远起不来",
                rc.discovery_step_secs, duration
            ));
        }
        if let Some(limit) = rc.max_udp_loss_pct {
            if !limit.is_finite() || !(0.0..=100.0).contains(&limit) {
                problems.push(format!(
                    "iperf.rate_check.max_udp_loss_pct={limit} 超出 [0, 100]"
                ));
            }
        }
        for (name, value) in [
            ("evb_usb_to_eth_target_mbps", rc.evb_usb_to_eth_target_mbps),
            ("evb_eth_to_usb_target_mbps", rc.evb_eth_to_usb_target_mbps),
            ("cpe_path_ceiling_mbps", rc.cpe_path_ceiling_mbps),
            ("wifi_payload_ceiling_mbps", rc.wifi_payload_ceiling_mbps),
            (
                "wifi_24g_payload_ceiling_mbps",
                rc.wifi_24g_payload_ceiling_mbps,
            ),
        ] {
            if !value.is_finite() || value <= 0.0 {
                problems.push(format!(
                    "iperf.rate_check.{name}={value} 必须是大于 0 的有限值"
                ));
            }
        }
        if self.ping.small_max_bytes == 0 {
            problems.push("ping.small_max_bytes 必须大于 0".into());
        }
        if self.ping.medium_max_bytes <= self.ping.small_max_bytes {
            problems.push(format!(
                "ping.medium_max_bytes={} 必须大于 ping.small_max_bytes={}",
                self.ping.medium_max_bytes, self.ping.small_max_bytes
            ));
        }
        for (name, avg, max) in [
            (
                "wired.small",
                self.ping.wired_small_avg_rtt_ms,
                self.ping.max_rtt_ms,
            ),
            (
                "wired.medium",
                self.ping.wired_medium_avg_rtt_ms,
                self.ping.wired_medium_max_rtt_ms,
            ),
            (
                "wired.large",
                self.ping.wired_large_avg_rtt_ms,
                self.ping.wired_large_max_rtt_ms,
            ),
            (
                "wifi.small",
                self.ping.wifi_small_avg_rtt_ms,
                self.ping.wifi_small_max_rtt_ms,
            ),
            (
                "wifi.medium",
                self.ping.wifi_medium_avg_rtt_ms,
                self.ping.wifi_medium_max_rtt_ms,
            ),
            (
                "wifi.large",
                self.ping.wifi_large_avg_rtt_ms,
                self.ping.wifi_large_max_rtt_ms,
            ),
        ] {
            if !avg.is_finite() || avg <= 0.0 {
                problems.push(format!(
                    "ping.{name}.avg_rtt_ms={avg} 必须是大于 0 的有限值"
                ));
            }
            if !max.is_finite() || max <= 0.0 {
                problems.push(format!(
                    "ping.{name}.max_rtt_ms={max} 必须是大于 0 的有限值"
                ));
            }
            if avg > max {
                problems.push(format!(
                    "ping.{name}.avg_rtt_ms={avg} 不能大于 max_rtt_ms={max}"
                ));
            }
        }
        problems
    }
}

/// 按优先级列出候选配置路径。抽出来是为了能测**顺序**本身——
/// 它依赖当前目录和 exe 位置，直接测 `load_config` 会互相踩。
pub(crate) fn config_candidates(explicit: Option<&str>, exe_dir: Option<&Path>) -> Vec<PathBuf> {
    match explicit {
        // 显式指定就**只认这一个**：找不到或读不了都不该悄悄换成别的文件。
        Some(path) => vec![PathBuf::from(path)],
        None => {
            let mut candidates = vec![PathBuf::from("config.json")];
            if let Some(dir) = exe_dir {
                let beside_exe = dir.join("config.json");
                // 从 exe 目录启动时两者是同一个文件，别读两遍。
                if beside_exe != candidates[0] {
                    candidates.push(beside_exe);
                }
            }
            candidates
        }
    }
}

/// [`load_config`] 的可测内核。**显式指定的配置读不出来是致命错误。**
///
/// 隐式那条路读不出来时退回默认是合理的：本来就是「碰运气看看旁边有没有」。
/// 但 `--config` 是人明确点名的那一份——解析失败却打一行 stderr 就接着用默认值
/// 跑，意味着整轮测试用的是**用户从没写过的门限**，而报告上不会有任何地方提到
/// 这件事。命令行刷过去的那一行警告，在 CI 日志和滚动的终端里等于不存在。
pub fn load_config_checked(
    explicit: Option<&str>,
    exe_dir: Option<&Path>,
) -> Result<(Config, Option<PathBuf>), String> {
    for p in config_candidates(explicit, exe_dir) {
        if p.exists() {
            match load_from(&p) {
                Ok(c) => {
                    for problem in c.validate() {
                        eprintln!("!! 配置项异常: {problem}");
                    }
                    return Ok((c, Some(p)));
                }
                Err(e) => {
                    if explicit.is_some() {
                        return Err(format!(
                            "配置文件 {} 解析失败: {e}\n                             这是 --config 明确指定的那一份，不会退回默认配置继续跑——\
                             用默认门限跑完一整轮，报告上不会有任何地方提到配置没生效。",
                            p.display()
                        ));
                    }
                    eprintln!("!! 配置文件 {} 解析失败: {e}", p.display());
                    eprintln!("!! 将使用默认配置继续");
                    return Ok((Config::default(), None));
                }
            }
        }
    }
    // 显式指定的文件不存在同样是致命的：静默用默认值跑等于换了一份配置。
    if let Some(path) = explicit {
        return Err(format!(
            "配置文件 {path} 不存在。--config 指定的路径不会退回默认配置。"
        ));
    }
    Ok((default_config_from_env(), None))
}

fn default_config_from_env() -> Config {
    let mut cfg = Config::default();
    // 兼容旧版环境变量
    if let Ok(v) = std::env::var("AUTOTEST_IPV4_PREFIXES") {
        let list: Vec<String> = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !list.is_empty() {
            cfg.ipv4_prefixes = list;
        }
    }
    if let Ok(v) = std::env::var("AUTOTEST_AGENT_HOST") {
        if !v.trim().is_empty() {
            cfg.agent_host = v.trim().to_string();
        }
    }
    cfg
}

/// 加载配置：--config 指定 > ./config.json > 程序同目录 config.json > 默认。
///
/// 显式指定的那一份读不出来时**直接退出**，理由见 [`load_config_checked`]。
pub fn load_config(explicit: Option<&str>) -> (Config, Option<PathBuf>) {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from));
    match load_config_checked(explicit, exe_dir.as_deref()) {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("!! {message}");
            std::process::exit(1);
        }
    }
}

fn load_from(p: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(p).map_err(|e| e.to_string())?;
    // 容忍 UTF-8 BOM
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str::<Config>(text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 发布包里的 `config.minimal.json` 必须真的能跑：只填三项、其余走默认，
    /// 且不能因为携带 `_说明` 之类的注释键而解析失败。
    #[test]
    fn shipped_minimal_config_parses_and_falls_back_to_defaults() {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.minimal.json"),
        )
        .expect("config.minimal.json 必须随仓库发布");
        let cfg: Config = serde_json::from_str(&text).expect("最小配置必须能解析");

        assert_eq!(cfg.agent_host, "192.168.1.3");
        assert_eq!(cfg.iperf.duration, 180);
        // 没填的字段全部落到默认值，且默认值本身通过校验。
        assert_eq!(cfg.agent_port, Config::default().agent_port);
        assert!(cfg.limit_udp_by_link_speed);
        assert_eq!(
            cfg.iperf.rate_check.min_active_ratio,
            RateCheckCfg::default().min_active_ratio
        );
        assert_eq!(cfg.ping.max_rtt_ms, PingCfg::default().max_rtt_ms);
        assert!(
            cfg.validate().is_empty(),
            "最小配置不应触发任何校验告警: {:?}",
            cfg.validate()
        );
    }

    /// `config.example.json` 是"完整字段面"的参考件，用户会整份抄走。
    ///
    /// 它必须能被真正的加载路径解析、通过校验，而且那几个**参考值**不能落后于
    /// 代码里的默认值——这份文件里写的数就是用户以为的默认值。抄一份把上限
    /// 钉死在旧数上，等于悄悄撤销一次校准（`cpe_path_ceiling_mbps` 2500 → 2600
    /// 那次就是这样漏掉的）。改默认值时这条测试会把这份文件一起拽上。
    #[test]
    fn shipped_example_config_parses_and_keeps_the_reference_values_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.json");
        let cfg = load_from(&path).expect("config.example.json 必须能被真正的加载路径解析");
        assert!(
            cfg.validate().is_empty(),
            "示例配置不应触发任何校验告警: {:?}",
            cfg.validate()
        );

        let defaults = RateCheckCfg::default();
        for (label, shipped, expected) in [
            (
                "cpe_path_ceiling_mbps",
                cfg.iperf.rate_check.cpe_path_ceiling_mbps,
                defaults.cpe_path_ceiling_mbps,
            ),
            (
                "wifi_payload_ceiling_mbps",
                cfg.iperf.rate_check.wifi_payload_ceiling_mbps,
                defaults.wifi_payload_ceiling_mbps,
            ),
            (
                "wifi_24g_payload_ceiling_mbps",
                cfg.iperf.rate_check.wifi_24g_payload_ceiling_mbps,
                defaults.wifi_24g_payload_ceiling_mbps,
            ),
            (
                "evb_usb_to_eth_target_mbps",
                cfg.iperf.rate_check.evb_usb_to_eth_target_mbps,
                defaults.evb_usb_to_eth_target_mbps,
            ),
            (
                "evb_eth_to_usb_target_mbps",
                cfg.iperf.rate_check.evb_eth_to_usb_target_mbps,
                defaults.evb_eth_to_usb_target_mbps,
            ),
        ] {
            assert_eq!(
                shipped, expected,
                "config.example.json 里的 {label} 落后于代码默认值，改默认值时要一起改这份参考件"
            );
        }

        // ping 次数和 RTT 门限同理：这份文件里写的数就是用户以为的默认值。
        assert_eq!(
            cfg.ping.count,
            PingCfg::default().count,
            "config.example.json 里的 ping.count 落后于代码默认值"
        );
        assert_eq!(
            cfg.ping.max_rtt_ms,
            PingCfg::default().max_rtt_ms,
            "config.example.json 里的 ping.max_rtt_ms 落后于代码默认值"
        );

        // agent_token 必须显式写成默认口令，不能留空串。
        //
        // serde 的 `default` 只在字段**缺失**时生效：这份文件里写着
        // `"agent_token": ""` 的话，照抄走的人拿到的是「显式关闭认证」，
        // 而不是默认口令——正好和他以为的相反，而且 agent 那边不会有任何提示。
        assert_eq!(
            cfg.agent_token, DEFAULT_TOKEN,
            "config.example.json 的 agent_token 必须写成默认口令，空串等于关掉认证"
        );
    }

    /// 随包发布的具名配置（`dist/configs/*.json`）也必须能被真正的加载路径解析
    /// 并通过校验。
    ///
    /// CI 只对它们做了 `json.load` —— 那只证明是合法 JSON，证明不了字段名没写错、
    /// `-l` 没超上限、`duration` 不是 0。用户是把这些文件当模板整份抄走的，
    /// 一个字段拼错要等到他真跑起来才发现。
    #[test]
    fn every_shipped_named_config_parses_and_validates() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/configs");
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .expect("dist/configs 必须存在")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        assert!(!paths.is_empty(), "dist/configs 下没有具名配置");

        for path in &paths {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let cfg = load_from(path).unwrap_or_else(|e| panic!("{name} 解析失败: {e}"));
            let warnings = cfg.validate();
            assert!(warnings.is_empty(), "{name} 触发校验告警: {warnings:?}");
        }

        // 全量预设是「照着 v4.4 那次实测拓扑抄的模板」，它的形状本身就是文档：
        // 5 块网口两两组合 = 10 对，TCP/UDP/PING 一次跑全，V4+V6 都在。
        // 少了任何一维，这份预设就不再是「全量」，而用户是照它改的。
        let full = load_from(&dir.join("config-full-tcp-udp-ping.json"))
            .expect("全量预设必须存在且可解析");
        assert_eq!(full.tests.len(), 10, "5 块网口两两组合应当是 10 对");
        for test in &full.tests {
            assert_eq!(test.direction.directions(), vec!["ab", "ba", "bidir"]);
            assert_eq!(test.transports, vec!["tcp", "udp"]);
            assert_eq!(test.ip, vec!["v4", "v6"]);
            assert!(
                test.kinds.iter().any(|k| k == "ping"),
                "{} 少了 ping",
                test.name
            );
            assert_eq!(test.ping_count, Some(full.ping.count));
            let profiles = test.udp_profiles.as_ref().expect("UDP 档位");
            assert!(
                profiles
                    .iter()
                    .all(|p| p.length.as_deref() == Some("14k")
                        && p.window.as_deref() == Some("256m")),
                "{} 的 UDP 档位应当是 -l 14k -w 256m",
                test.name
            );
        }
        // 同机组合（桥接/回环）也要在里面：v4.4 那次跑的就有 4 对同机。
        let same_host = full
            .tests
            .iter()
            .filter(|t| t.src.split(':').next() == t.dst.split(':').next())
            .count();
        assert_eq!(same_host, 4, "应当有 4 对同机组合（主控 1 对 + 辅测 3 对）");
    }

    /// 编译期读的文件必须在仓库里，不能是 `.gitignore` 排掉的本机配置。
    ///
    /// 守的是「本机四条门禁全绿、干净克隆连测试都编译不出来」这一类。
    /// `include_str!` 在编译期读盘：文件躺在开发机上，`cargo test` 就全绿，
    /// 而 CI 的 checkout 和任何新克隆里根本没有它。绿灯在这里完全没有分辨力。
    ///
    /// 历史实例：`src/inner/tests.rs` 曾在编译期直接读 `inner.local.example.json`，
    /// 而 `.gitignore` 的 `*.local.example` 加 `.json` 有意把这类本机配置挡在仓库外
    /// （它们可能带 agent_token）。两个决定各自都对，凑一起就是编译不过。
    ///
    /// 忽略规则从 `.gitignore` 现读，只认 `*.后缀` 这种简单通配——那正是
    /// 「本机配置」这条约定的写法。前缀通配（`report_*.html`）不在射程内，
    /// 因为编译期不会去 include 运行产物。
    #[test]
    fn no_compile_time_include_depends_on_a_gitignored_local_file() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let ignored_suffixes: Vec<String> = std::fs::read_to_string(root.join(".gitignore"))
            .expect("读 .gitignore")
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix("*."))
            .filter(|rest| !rest.is_empty() && !rest.contains('*') && !rest.contains('/'))
            .map(|rest| format!(".{rest}"))
            .collect();
        assert!(
            !ignored_suffixes.is_empty(),
            ".gitignore 里已经没有 `*.后缀` 形式的本机配置规则，这条守卫会永远为真——\
             要么把规则加回去，要么连这条测试一起删，不要留一个不会红的守卫"
        );

        let mut offenders = Vec::new();
        let mut stack = vec![root.join("src")];
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
                for macro_call in ["include_str!(", "include_bytes!("] {
                    for chunk in text.split(macro_call).skip(1) {
                        let Some(arg) = chunk.split('"').nth(1) else {
                            continue;
                        };
                        if let Some(hit) = ignored_suffixes.iter().find(|s| arg.ends_with(&***s)) {
                            offenders.push(format!(
                                "{}: {macro_call}{arg:?} 命中 .gitignore 的 *{hit}",
                                path.display()
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "编译期依赖了不在版本控制里的文件，干净克隆会编译失败: {offenders:#?}"
        );
    }

    #[test]
    fn validate_flags_settings_that_would_silently_kill_every_traffic_unit() {
        let ok = Config::default();
        assert!(ok.validate().is_empty(), "{:?}", ok.validate());

        // settle 不短于要计分的时长：进程会相应多跑，但这样的配置多半是写错了。
        let mut settle = Config::default();
        settle.iperf.duration = 10;
        settle.iperf.rate_check.settle_secs = 10;
        assert!(settle.validate().iter().any(|p| p.contains("settle_secs")));

        // 基线 + settle 合起来吃掉窗口。
        let mut baseline = Config::default();
        baseline.iperf.duration = 8;
        baseline.iperf.rate_check.settle_secs = 5;
        baseline.iperf.rate_check.background_secs = 3;
        assert!(baseline
            .validate()
            .iter()
            .any(|p| p.contains("background_secs")));

        let mut ratio = Config::default();
        ratio.iperf.rate_check.min_active_ratio = 1.5;
        assert!(ratio
            .validate()
            .iter()
            .any(|p| p.contains("min_active_ratio")));

        let mut loss = Config::default();
        loss.iperf.rate_check.max_udp_loss_pct = Some(-1.0);
        assert!(loss
            .validate()
            .iter()
            .any(|p| p.contains("max_udp_loss_pct")));

        let mut ping_rtt = Config::default();
        ping_rtt.ping.max_rtt_ms = 0.0;
        assert!(ping_rtt
            .validate()
            .iter()
            .any(|p| p.contains("wired.small") || p.contains("ping.max_rtt_ms")));

        // discover 阶梯排到测试结束之后，最后几档流永远起不来。
        let mut discover = Config::default();
        discover.iperf.duration = 30;
        discover.iperf.rate_check.discovery_step_secs = 60;
        assert!(discover
            .validate()
            .iter()
            .any(|p| p.contains("discovery_step_secs")));

        let mut ceiling = Config::default();
        ceiling.iperf.rate_check.cpe_path_ceiling_mbps = 0.0;
        assert!(ceiling
            .validate()
            .iter()
            .any(|p| p.contains("cpe_path_ceiling_mbps")));

        let bad_host = Config {
            agent_host: "bad\r\nHost: injected".into(),
            ..Config::default()
        };
        assert!(bad_host
            .validate()
            .iter()
            .any(|p| p.contains("agent_host") && p.contains("控制字符")));
        let bad_token = Config {
            agent_token: "bad\r\n".into(),
            ..Config::default()
        };
        assert!(bad_token.validate().iter().any(|p| p.contains("控制字符")));
        let too_long_token = Config {
            agent_token: "x".repeat(MAX_AGENT_TOKEN_BYTES + 1),
            ..Config::default()
        };
        assert!(too_long_token.validate().iter().any(|p| p.contains("上限")));
    }

    #[test]
    fn test_defaults() {
        let c = Config::default();
        assert_eq!(c.agent_port, 28801);
        assert_eq!(c.iperf.duration, 180);
        assert_eq!(c.iperf.tcp_windows, vec!["64k", "1m", "4m"]);
        assert_eq!(c.iperf.udp_profiles.len(), 5);
        assert!(c.iperf.udp_profiles.iter().all(|p| p.window.is_none()));
        assert_eq!(c.ping.count, 180);
        assert_eq!(c.ping.max_rtt_ms, 30.0);
        // 默认口令：从「不认证」改成一个固定值，挡的是误连不是攻击。
        // 钉住它是因为它同时是 agent 认证和控制台访问的出厂值，改动会同时
        // 影响两端能否互通。
        assert_eq!(c.agent_token, DEFAULT_TOKEN);
        assert_eq!(DEFAULT_TOKEN, "cpetest");
        assert_eq!(c.ping.payload_sizes, vec![32, 1600, 65500]);
        assert_eq!(c.iperf.rate_check.mode, RateMode::Auto);
        assert_eq!(c.iperf.rate_check.evb_usb_to_eth_target_mbps, 6400.0);
        assert_eq!(c.iperf.rate_check.evb_eth_to_usb_target_mbps, 8400.0);
    }

    #[test]
    fn test_parse_full() {
        let j = r#"{
            "agent_host": "10.228.46.50",
            "ipv4_prefixes": ["192.168.", "10.10."],
            "iperf": {"duration": 60},
            "ping": {"count": 10, "payload_sizes": [32, 1600, 65500], "max_rtt_ms": 12.5},
            "tests": [
                {"name":"t1","src":"master:SGMII2.5G","dst":"agent:SGMII2.5G",
                 "direction":"bidir","kinds":["iperf","ping"],"transports":["tcp","udp"],
                 "ip":["v4","v6"],"streams":5,"tcp_streams":7,"udp_streams":3,
                 "iperf_duration":300},
                {"name":"t2","src":"master:SGMII1G","dst":"agent:SGMII1G",
                 "direction":["A->B","B->A"]}
            ]
        }"#;
        let c: Config = serde_json::from_str(j).unwrap();
        assert_eq!(c.agent_host, "10.228.46.50");
        assert_eq!(c.iperf.duration, 60);
        assert_eq!(c.ping.max_rtt_ms, 12.5);
        // 未写的字段用默认
        assert_eq!(c.iperf.tcp_windows.len(), 3);
        assert_eq!(c.tests.len(), 2);
        assert_eq!(c.tests[0].direction.directions(), vec!["bidir"]);
        assert_eq!(c.tests[0].iperf_duration, Some(300));
        assert_eq!(c.tests[0].tcp_streams, Some(7));
        assert_eq!(c.tests[0].udp_streams, Some(3));
        assert_eq!(c.tests[1].direction.directions(), vec!["ab", "ba"]);
        assert_eq!(c.tests[1].kinds, vec!["iperf"]);
        assert_eq!(c.tests[1].tcp_streams, None);
        assert_eq!(c.tests[1].udp_streams, None);
    }

    #[test]
    fn test_direction_both() {
        let d = OneOrMany::One("both".into());
        assert_eq!(d.directions(), vec!["ab", "ba"]);
    }

    #[test]
    fn test_udp_profile() {
        let mbps = |bandwidth: &str| {
            UdpProfile::bw(bandwidth)
                .parsed_bandwidth()
                .ok()
                .map(|value| value.mbps)
        };
        assert_eq!(mbps("500m"), Some(500.0));
        assert_eq!(mbps("1g"), Some(1000.0));
        assert_eq!(mbps("2.8G"), Some(2800.0));
        assert_eq!(mbps("2.8Gbps"), Some(2800.0));
        assert_eq!(mbps("2,8gBpS"), Some(2800.0));
        let parsed = UdpProfile::bw("2.8Gbps").parsed_bandwidth().unwrap();
        // 下发给 iperf3 的是这个精确整数 bit/s（见 builder::UdpLoad::iperf_arg），
        // 不依赖它对 `Gbps` 等长后缀的非文档兼容行为。
        assert_eq!(parsed.bits_per_second, 2_800_000_000);
        for invalid in [
            "",
            "2.8oopsGbps",
            "2.8Gbps trailing",
            "2.8mbpsx",
            "1e3m",
            "1.2,3g",
            "1.",
            "+1m",
            "0m",
            "18446744073709.551616",
        ] {
            assert_eq!(mbps(invalid), None, "必须拒绝非完整带宽 value={invalid:?}");
        }
        assert_eq!(UdpProfile::bw("2500m").name(), "udp_b2500m");
        let p = UdpProfile {
            bandwidth: "1000m".into(),
            length: Some("64".into()),
            window: Some("4m".into()),
        };
        assert_eq!(p.name(), "udp_b1000m_l64_w4m");
        assert_eq!(p.label(), "UDP -b 1000m -l 64 -w 4m");
    }

    #[test]
    fn test_udp_profile_window_parse_is_backward_compatible() {
        let legacy: UdpProfile = serde_json::from_str(r#"{"bandwidth":"500m"}"#).unwrap();
        assert_eq!(legacy.bandwidth, "500m");
        assert_eq!(legacy.length, None);
        assert_eq!(legacy.window, None);

        let configured: UdpProfile =
            serde_json::from_str(r#"{"bandwidth":"1000m","length":"64","window":"4m"}"#).unwrap();
        assert_eq!(configured.length.as_deref(), Some("64"));
        assert_eq!(configured.window.as_deref(), Some("4m"));
        assert_eq!(configured.name(), "udp_b1000m_l64_w4m");
        assert_eq!(configured.label(), "UDP -b 1000m -l 64 -w 4m");
    }

    #[test]
    fn test_rate_check_parse() {
        let j = r#"{
            "iperf": {
                "rate_check": {
                    "mode": "verify",
                    "targets_mbps": {"ab": 6400, "ba": 8400},
                    "min_active_ratio": 0.8,
                    "flow_retries": 2
                }
            }
        }"#;
        let c: Config = serde_json::from_str(j).unwrap();
        assert_eq!(c.iperf.rate_check.mode, RateMode::Verify);
        assert_eq!(c.iperf.rate_check.targets_mbps.ab, Some(6400.0));
        assert_eq!(c.iperf.rate_check.targets_mbps.ba, Some(8400.0));
        assert_eq!(c.iperf.rate_check.min_active_ratio, 0.8);
        assert_eq!(c.iperf.rate_check.flow_retries, 2);
    }

    #[test]
    fn test_per_scenario_rate_mode_and_targets_parse() {
        let j = r#"{
            "universal_params": {
                "rate_mode": "discover",
                "rate_targets_mbps": {"forward": 2500}
            },
            "tests": [{
                "name": "evb",
                "src": "master:10GUSB",
                "dst": "agent:10GETH",
                "rate_mode": "verify",
                "rate_targets_mbps": {"ab": 6400, "ba": 8400}
            }]
        }"#;
        let c: Config = serde_json::from_str(j).unwrap();
        let universal = c.universal_params.unwrap();
        assert_eq!(universal.rate_mode, Some(RateMode::Discover));
        assert_eq!(universal.rate_targets_mbps.unwrap().forward, Some(2500.0));
        assert_eq!(c.tests[0].rate_mode, Some(RateMode::Verify));
        assert_eq!(
            c.tests[0].rate_targets_mbps.as_ref().unwrap().ab,
            Some(6400.0)
        );
        assert_eq!(
            c.tests[0].rate_targets_mbps.as_ref().unwrap().ba,
            Some(8400.0)
        );
    }

    #[test]
    fn test_evb_direction_target_names_and_legacy_aliases() {
        let current: Config = serde_json::from_str(
            r#"{
                "iperf": {"rate_check": {
                    "evb_usb_to_eth_target_mbps": 6100,
                    "evb_eth_to_usb_target_mbps": 8300
                }}
            }"#,
        )
        .unwrap();
        assert_eq!(current.iperf.rate_check.evb_usb_to_eth_target_mbps, 6100.0);
        assert_eq!(current.iperf.rate_check.evb_eth_to_usb_target_mbps, 8300.0);

        let legacy: Config = serde_json::from_str(
            r#"{
                "iperf": {"rate_check": {
                    "evb_usb_tx_target_mbps": 6200,
                    "evb_usb_rx_target_mbps": 8200
                }}
            }"#,
        )
        .unwrap();
        assert_eq!(legacy.iperf.rate_check.evb_usb_to_eth_target_mbps, 6200.0);
        assert_eq!(legacy.iperf.rate_check.evb_eth_to_usb_target_mbps, 8200.0);
    }

    /// **`--config` 指定的那一份读不出来，必须停下来**（回归方案 CFG-01 / 缺陷 D-11）。
    ///
    /// 原行为是打一行 stderr 警告然后**用默认配置把整轮跑完**。代价不是「跑失败」
    /// ——是跑成功，然后交出一份按**用户从没写过的门限**判出来的报告。命令行上
    /// 刷过去的那一行警告，在 CI 日志和滚动的终端里等于不存在。
    ///
    /// 隐式那条路（不带 `--config`，碰运气看看旁边有没有）保持原样：退回默认
    /// 本来就是它的语义。
    #[test]
    fn an_explicitly_named_config_never_silently_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("cpe_cfg01_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 坏 JSON：显式指定 → 报错，不返回配置。
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ \"iperf\": { \"duration\": ").unwrap();
        let error = load_config_checked(Some(bad.to_str().unwrap()), None)
            .expect_err("显式指定的坏配置必须是致命错误");
        assert!(
            error.contains("解析失败") && error.contains("不会退回默认配置"),
            "错误要说清为什么不继续：{error}"
        );

        // 文件根本不存在：同样不许静默用默认值跑。
        let missing = dir.join("nope.json");
        let error = load_config_checked(Some(missing.to_str().unwrap()), None)
            .expect_err("显式指定的文件不存在必须是致命错误");
        assert!(error.contains("不存在"), "{error}");

        // 好配置：正常读出来，并带回它的路径。
        let good = dir.join("good.json");
        std::fs::write(
            &good,
            "{\"agent_host\":\"10.0.0.9\",\"iperf\":{\"duration\":42}}",
        )
        .unwrap();
        let (cfg, path) =
            load_config_checked(Some(good.to_str().unwrap()), None).expect("合法配置必须读得出");
        assert_eq!(cfg.agent_host, "10.0.0.9");
        assert_eq!(cfg.iperf.duration, 42);
        assert_eq!(path.as_deref(), Some(good.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 候选路径的**顺序**：显式 > ./config.json > 程序同目录（回归方案 CFG-01）。
    ///
    /// 「有冲突 config」正是靠这个顺序解决的。显式指定时**只有一个候选**——
    /// 找不到就报错，不许悄悄滑到当前目录那一份去：那会让「我明明指定了 A」
    /// 变成「实际跑的是 B」，而两份文件的门限完全可以不一样。
    #[test]
    fn config_lookup_order_is_explicit_then_cwd_then_next_to_the_exe() {
        let exe_dir = Path::new("/opt/cpe");

        assert_eq!(
            config_candidates(Some("/tmp/mine.json"), Some(exe_dir)),
            vec![PathBuf::from("/tmp/mine.json")],
            "显式指定时只能有一个候选——滑到别的文件上意味着跑的不是我指定的那份"
        );

        assert_eq!(
            config_candidates(None, Some(exe_dir)),
            vec![PathBuf::from("config.json"), exe_dir.join("config.json")],
            "隐式顺序：先当前目录，再程序同目录"
        );

        assert_eq!(
            config_candidates(None, None),
            vec![PathBuf::from("config.json")],
            "拿不到 exe 位置时只剩当前目录"
        );

        // 从 exe 目录启动时两者是同一个文件，不该读两遍。
        assert_eq!(
            config_candidates(None, Some(Path::new(""))),
            vec![PathBuf::from("config.json")],
            "当前目录就是 exe 目录时候选不该重复"
        );
    }
}
