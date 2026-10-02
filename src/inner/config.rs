use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 内环项目当前 schema 版本。版本 1 是「固定上下行、板侧统计接口绑死在
/// 网关归属口、只有网卡口径」那一代，导入时按 [`migrate_v1`] 升级。
pub const PROJECT_VERSION: u32 = 3;
pub const PROJECT_KIND: &str = "cpe-inner-project";
/// 地址最终进入 HTTP Host 头；DNS 名称的协议上限是 253 字节，给 IP/端口解析
/// 和实现留一点余量，但不接受把整份请求体塞进一个主机名字段。
pub(crate) const MAX_AGENT_ADDRESS_BYTES: usize = 256;
/// 令牌最终进入 HTTP Authorization 头。正常共享口令远小于此值；上限用于
/// 防止配置文件把单个头字段膨胀到 MiB 级。
pub(crate) const MAX_AGENT_TOKEN_BYTES: usize = 4096;

/// 内环配置独立于双机 Config；拼错键必须报错，不能静默沿用双机预设。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InnerConfig {
    pub adb_path: String,
    pub serial: String,
    pub board_iperf: String,
    pub duration_secs: u64,
    /// 未按协议覆盖时两种协议共用的并发流数。
    pub parallel: u32,
    /// 本轮要跑的协议；两种都填就对每条链路先跑完 TCP 再跑 UDP。
    ///
    /// 之前这里是单个 `protocol`，一轮只能测一种——想同时要 TCP 和 UDP 的结论
    /// 必须跑两轮、拿到两份互不相干的报告，链路和板侧状态也不再是同一时刻的。
    pub protocols: Vec<Protocol>,
    /// 每个版本独立展开测试单元；旧配置缺省只测 IPv4。
    pub ip_versions: Vec<u8>,
    /// 本轮要跑的方向。上行、下行是两个独立的单向单元；双向并发是一个
    /// 含两条腿的单元。三者可同时勾选，各自出结果，两次顺序单向不算双向。
    pub directions: Vec<Direction>,
    /// 覆盖 `parallel` 的 TCP 并发流数；留空沿用 `parallel`。
    pub tcp_streams: Option<u32>,
    /// 覆盖 `parallel` 的 UDP 并发流数；留空沿用 `parallel`。
    pub udp_streams: Option<u32>,
    /// TCP socket 缓冲（iperf3 `-w`），如 `4m`；留空用系统默认。
    pub tcp_window: Option<String>,
    /// UDP 每条流的发送速率 Mbps（iperf3 `-b`）。协议含 UDP 时必填。
    pub udp_mbps: Option<f64>,
    /// UDP 数据报载荷字节数（iperf3 `-l`），如 `1400`；留空用 iperf3 默认。
    pub udp_length: Option<String>,
    /// UDP 丢包诊断门槛（%）。与子网口径一致：超限只写诊断，
    /// 达标与否仍然只看本腿选中的接收速率来源。
    pub max_udp_loss_pct: Option<f64>,
    /// 接收端 server 起始端口。双向单元的两条腿各占一个端口，用 `port` 与
    /// `port + 1`，因此这里的上限比单向低一格。
    pub port: u16,
    /// 每个测试单元重复几轮。每一轮都是独立的一条结果，**不会**因为前一轮
    /// 没达标就自动重跑到 PASS；预览里也按重复后的单元数报数。
    pub repeats: u32,
    /// 重跑时跳过 24 小时内已经 PASS 的同一内环单元。
    #[serde(default)]
    pub resume: bool,
    pub agents: Vec<AgentConfig>,
    pub links: Vec<Link>,
}

impl Default for InnerConfig {
    fn default() -> Self {
        Self {
            adb_path: "adb".into(),
            serial: String::new(),
            board_iperf: "iperf3".into(),
            duration_secs: 20,
            parallel: 1,
            protocols: vec![Protocol::Tcp],
            ip_versions: vec![4],
            directions: vec![Direction::Upload, Direction::Download],
            tcp_streams: None,
            udp_streams: None,
            tcp_window: None,
            udp_mbps: None,
            udp_length: None,
            max_udp_loss_pct: None,
            port: 56190,
            repeats: 1,
            resume: false,
            agents: Vec::new(),
            links: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tcp => "TCP",
            Self::Udp => "UDP",
        }
    }
    pub fn is_udp(self) -> bool {
        self == Self::Udp
    }
}

/// 页面上的方向。发送端为普通 client：上行 PC 发起，下行板侧通过 ADB 发起；
/// 接收端启动 server，双方均不使用反向参数。
///
/// `Bidir` 是**一个**同时含上下行两条腿的单元，不是「先上行再下行」。
/// 两次顺序单向永远不会被迁移或聚合成 `Bidir`：它们测不出同一时刻
/// 双向争抢时的表现，那恰恰是双向并发要回答的问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Upload,
    Download,
    Bidir,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Upload => "上行（PC → 板侧）",
            Self::Download => "下行（板侧 → PC）",
            Self::Bidir => "双向并发（同一链路同时上下行）",
        }
    }
    /// 本方向展开出的数据腿。双向是唯一一个两条腿的方向。
    pub fn flows(self) -> &'static [Flow] {
        match self {
            Self::Upload => &[Flow::Up],
            Self::Download => &[Flow::Down],
            Self::Bidir => &[Flow::Up, Flow::Down],
        }
    }
    pub fn is_bidir(self) -> bool {
        self == Self::Bidir
    }
}

/// 一条腿的数据走向。方向是页面语义，腿是执行语义：双向单元里两条腿
/// 同时在跑，各自有独立的端口、job 和接收端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Flow {
    Up,
    Down,
}

impl Flow {
    pub fn label(self) -> &'static str {
        match self {
            Self::Up => "上行",
            Self::Down => "下行",
        }
    }
    /// 接收端在板侧还是在网口所在电脑。**不按「谁跑 client」推断**：
    /// client 两条腿都在 PC 侧，接收端却是相反的两端。
    pub fn receiver_is_board(self) -> bool {
        self == Self::Up
    }
}

/// 一条腿的接收速率取自哪一层。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Measurement {
    /// 网卡计数严格模式：只认可信的接收接口字节计数，不可信就 NOT_EVALUATED。
    /// v1 配置迁移后保持这一档，正式验收口径与子网完全一致。
    #[default]
    NicStrict,
    /// 网卡优先、工具兜底：计数不可用或已判不可信时才改用工具 receiver 汇总。
    /// **可信的低速不触发兜底**——否则就是换口径把 RATE_FAIL 救成 PASS。
    NicPreferred,
    /// 明确使用工具 receiver 汇总；字节计数若可用则并列作诊断。
    Tool,
}

impl Measurement {
    pub fn label(self) -> &'static str {
        match self {
            Self::NicStrict => "网卡计数严格",
            Self::NicPreferred => "网卡优先，工具兜底",
            Self::Tool => "工具接收速率",
        }
    }
    /// 严格模式永远不会用到工具口径，因此也不接受工具口径门限：
    /// 配了却永不生效的门限比没有门限更容易让人误判。
    pub fn uses_tool(self) -> bool {
        self != Self::NicStrict
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub name: String,
    /// master 为主控；其他值为 agents 中的 id。
    #[serde(default = "master_host")]
    pub host: String,
    /// 本轮是否参与。取消勾选保留全部参数，只是不执行、不预检、
    /// 也不把它引用的辅测机拉进连接门禁。
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    pub local_interface: String,
    #[serde(default = "unspecified_v4")]
    pub local_ip: Ipv4Addr,
    /// 板侧 LAN 地址：上行 server 绑定/目标，下行 client 源地址，同时校验板侧归属。
    #[serde(default = "unspecified_v4")]
    pub gateway: Ipv4Addr,
    /// IPv6 地址不接受 zone；执行端根据已核实的接口身份附加作用域。
    #[serde(default)]
    pub local_ipv6: Option<Ipv6Addr>,
    #[serde(default)]
    pub gateway_ipv6: Option<Ipv6Addr>,
    /// 板侧 **RX 采样接口**，与 `gateway` 的地址归属解耦。
    ///
    /// 留空按地址归属自动识别；填写时可以是经过确认的桥、桥成员或别的
    /// 逻辑接口——统计接口不必自己持有这个 LAN IP。绝不默认把 br0 与 eth1
    /// 累加：那多半是把同一批包数了两遍。
    #[serde(default, alias = "board_interface")]
    pub board_rx_interface: String,
    /// 这条链路的接收速率来源策略。
    #[serde(default)]
    pub measurement: Measurement,
    #[serde(default)]
    pub upload_min_mbps: Option<f64>,
    #[serde(default)]
    pub download_min_mbps: Option<f64>,
    /// 双向并发单元的**网卡口径合计**门限。留空则双向单元按逐方向门限判定，
    /// 不从单向门限自动除以二。
    #[serde(default)]
    pub bidir_total_min_mbps: Option<f64>,
    /// 工具口径门限，与网卡口径完全独立。没有它时工具兜底只出 MEASURED，
    /// 不会继承网卡门限自动 PASS。
    #[serde(default)]
    pub tool_upload_min_mbps: Option<f64>,
    #[serde(default)]
    pub tool_download_min_mbps: Option<f64>,
    #[serde(default)]
    pub tool_bidir_total_min_mbps: Option<f64>,
}

impl Link {
    pub fn local_address(&self, ip_version: u8) -> IpAddr {
        if ip_version == 6 {
            self.local_ipv6.unwrap_or(Ipv6Addr::UNSPECIFIED).into()
        } else {
            self.local_ip.into()
        }
    }
    pub fn board_address(&self, ip_version: u8) -> IpAddr {
        if ip_version == 6 {
            self.gateway_ipv6.unwrap_or(Ipv6Addr::UNSPECIFIED).into()
        } else {
            self.gateway.into()
        }
    }

    /// 本腿在指定口径下的单向门限。
    pub fn leg_target(&self, flow: Flow, tool: bool) -> Option<f64> {
        match (flow, tool) {
            (Flow::Up, false) => self.upload_min_mbps,
            (Flow::Down, false) => self.download_min_mbps,
            (Flow::Up, true) => self.tool_upload_min_mbps,
            (Flow::Down, true) => self.tool_download_min_mbps,
        }
    }
    /// 双向合计门限。
    pub fn total_target(&self, tool: bool) -> Option<f64> {
        if tool {
            self.tool_bidir_total_min_mbps
        } else {
            self.bidir_total_min_mbps
        }
    }
}

fn master_host() -> String {
    "master".into()
}
fn enabled_default() -> bool {
    true
}
fn unspecified_v4() -> Ipv4Addr {
    Ipv4Addr::UNSPECIFIED
}

/// 解析内环项目或裸配置。
///
/// 迁移失败不改调用方的现有配置：这里只返回 `Result`，从不就地修补。
pub fn parse_config(text: &str) -> Result<InnerConfig, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("内环配置 JSON 无效: {e}"))?;
    if value.get("project_version").is_some() {
        return Err("这是子网项目，请在子网测试中导入".into());
    }
    // 子网还有第二种形态：控制台「下载 config.json」和手写的裸配置都没有
    // `project_version`，只认那一个键的话，这类文件会一路掉进下面的
    // `deny_unknown_fields`，报出来的是「unknown field
    // `abort_after_dead_traffic_units`, expected one of ...」——一串字段名，
    // 半个字没说清「你把子网的文件导到内环来了」。
    //
    // 这些键内环一个都没有（内环是 `agents`/`links`/`board_iperf`），拿来认
    // 形状不会误伤；子网侧认内环也是同一条路子（`kind`/`adb_path`/
    // `board_iperf`），两边这才对称。
    const SUBNET_ONLY_KEYS: [&str; 11] = [
        "agent_host",
        "agent_port",
        "require_same_subnet_for_iperf",
        "ipv4_prefixes",
        "link_profiles",
        "universal_params",
        "ctstraffic",
        "tests",
        "pairs",
        "iperf",
        "ping",
    ];
    if let Some(key) = SUBNET_ONLY_KEYS
        .iter()
        .find(|key| value.get(*key).is_some())
    {
        return Err(format!("这是子网配置（含 {key}），请在子网测试中导入"));
    }
    let raw = if value.get("kind").is_some() {
        let kind = value
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or("内环项目缺少 version")?;
        if kind != PROJECT_KIND {
            return Err(format!("仅支持 {PROJECT_KIND} 项目"));
        }
        if version > PROJECT_VERSION as u64 {
            return Err(format!(
                "内环项目版本 {version} 高于本程序支持的 {PROJECT_VERSION}，请升级程序后再导入"
            ));
        }
        let extra: Vec<_> = value
            .as_object()
            .map(|map| {
                map.keys()
                    .filter(|k| !["kind", "version", "config"].contains(&k.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if !extra.is_empty() {
            return Err(format!("内环项目含未知字段：{}", extra.join("、")));
        }
        let mut config = value
            .get("config")
            .cloned()
            .filter(serde_json::Value::is_object)
            .ok_or("内环项目缺少 config 对象")?;
        if version < 2 {
            migrate_v1(&mut config)?;
        }
        config
    } else {
        // 裸配置没有版本号，只能看形状：出现 v1 独有的键就按 v1 迁移。
        let mut config = value;
        if looks_like_v1(&config) {
            migrate_v1(&mut config)?;
        }
        config
    };
    let mut cfg: InnerConfig =
        serde_json::from_value(raw).map_err(|e| format!("内环配置无效: {e}"))?;
    // 地址最终会进入 HTTP 客户端；和主控的 /api/connect 一样，去掉表单或
    // 手写 JSON 里常见的首尾空格。令牌不能做同样的处理，它是不透明的秘密。
    for agent in &mut cfg.agents {
        agent.address = agent.address.trim().to_string();
    }
    cfg.validate()?;
    Ok(cfg)
}

fn looks_like_v1(value: &serde_json::Value) -> bool {
    value.get("protocol").is_some()
        || value
            .get("links")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|links| links.iter().any(|l| l.get("board_interface").is_some()))
}

/// v1 → v2。
///
/// 只做重命名和补默认值，**不改变任何测试语义**：v1 的 `["upload","download"]`
/// 是「先跑上行再跑下行」两个独立单向单元，迁移后仍然是两个单向单元。把它
/// 折成 `bidir` 会凭空造出一份从没测过的并发结论。
fn migrate_v1(config: &mut serde_json::Value) -> Result<(), String> {
    let map = config.as_object_mut().ok_or("内环配置必须是 JSON 对象")?;
    if let Some(protocol) = map.remove("protocol") {
        if map.contains_key("protocols") {
            return Err("配置同时含 protocol 与 protocols，无法判断该用哪一个".into());
        }
        map.insert("protocols".into(), serde_json::Value::Array(vec![protocol]));
    }
    if let Some(links) = map.get_mut("links").and_then(|v| v.as_array_mut()) {
        for link in links {
            let Some(link) = link.as_object_mut() else {
                continue;
            };
            if let Some(iface) = link.remove("board_interface") {
                // v1 的 board_interface 必须等于网关归属口，语义上就是采样接口。
                link.entry("board_rx_interface").or_insert(iface);
            }
            // v1 没有 enabled / measurement：全部参与，并保持严格网卡口径，
            // 升级不会让既有配置换成另一套验收规则。
            link.entry("enabled")
                .or_insert(serde_json::Value::Bool(true));
            link.entry("measurement")
                .or_insert(serde_json::Value::String("nic_strict".into()));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub id: String,
    pub address: String,
    pub port: u16,
    #[serde(default, skip_serializing)]
    pub token: String,
}

/// iperf3 的尺寸参数（`-w` / `-l`）：十进制数值加可选的 k/m/g 后缀。
///
/// 白名单而不是黑名单：这两个值原样拼进命令行，放行任何别的形状就等于
/// 让配置文件往 iperf3 里塞参数，`RESERVED_CLIENT_FLAGS` 那道闸也就白设了。
pub fn size_token(value: &str) -> bool {
    let digits = value
        .strip_suffix(['k', 'K', 'm', 'M', 'g', 'G'])
        .unwrap_or(value);
    digits.len() <= 12
        && digits.bytes().all(|b| b.is_ascii_digit())
        && digits.parse::<u64>().is_ok_and(|v| v > 0)
}

fn unique<T: Eq + std::hash::Hash>(values: &[T]) -> bool {
    values
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        == values.len()
}

pub fn safe_word(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
}

/// ADB 可执行文件路径。
///
/// 这是整份配置里**唯一一个会被当程序执行**的字段（`Command::new(&adb.program)`），
/// 而它此前只查了非空——`board_iperf` 和 `serial` 都走 [`safe_word`]，偏偏最危险
/// 的这个没有。带着 UI 口令 POST 一次 `/api/inner/probe`，就能让主控运行任意
/// 可执行文件。子网侧从来没有这个面：那边的 iperf3 只在固定列表里找，路径不由
/// 配置提供。
///
/// 不能直接套 `safe_word`：Windows 路径要用反斜杠和空格
/// （`C:\Program Files\platform-tools\adb.exe`），那个白名单一个都不放行。
/// 所以这里管的是**指向什么**而不是长什么样：
///
/// - 文件名（去掉可选的 `.exe`、大小写不敏感）必须是 `adb`，或 `adb-` 加版本号
///   （`adb-1.0.41`，多版本并存时常见）。以前只要求「以 adb 开头」，任何名字以
///   adb 开头的程序都能借这个字段执行，等于没挡住上面那件事；
/// - 只接受本机路径：开头两个字符都是分隔符的一律拒绝——网络共享（`\\server\share`、
///   `//server/share`、混用分隔符的写法）和设备命名空间（`\\?\`、`\\.\`）都长这样。
///   这个字段会被当程序执行，指到别的机器上就是让主控运行一份不在本机的程序。
///
/// 只写 `adb` / `adb.exe` 时走 PATH，本机任意目录下的 adb 照常可用。
pub fn adb_program(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.len() > 512 || value.starts_with('-') {
        return false;
    }
    // 控制字符（含 NUL/换行）不出现在真实路径里，出现就是在拼别的东西。
    if value.chars().any(char::is_control) {
        return false;
    }
    let mut leading = value.chars();
    if matches!(
        (leading.next(), leading.next()),
        (Some('/' | '\\'), Some('/' | '\\'))
    ) {
        return false;
    }
    let name = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    stem == "adb"
        || stem.strip_prefix("adb-").is_some_and(|version| {
            version.starts_with(|c: char| c.is_ascii_digit())
                && version.chars().all(|c| c.is_ascii_digit() || c == '.')
        })
}

/// 板侧接口名。比 [`safe_word`] 更窄：它要拼进 `/sys/class/net/<iface>/...`，
/// 放行 `/` 或 `..` 就等于让配置文件挑选读哪个文件。Linux 接口名本来也不含
/// 这两样东西。
pub fn iface_word(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && !value.starts_with('-')
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

impl InnerConfig {
    /// 该协议实际使用的并发流数：按协议覆盖优先，留空沿用 `parallel`。
    pub fn streams(&self, protocol: Protocol) -> u32 {
        match protocol {
            Protocol::Tcp => self.tcp_streams,
            Protocol::Udp => self.udp_streams,
        }
        .unwrap_or(self.parallel)
    }

    /// 本轮参与执行的链路下标，保持用户排定的顺序。
    pub fn active_links(&self) -> Vec<usize> {
        (0..self.links.len())
            .filter(|index| self.links[*index].enabled)
            .collect()
    }

    /// 本轮实际被引用的辅测机 id。未启用链路引用的辅测机不在其中，
    /// 因此不扫描、不连接，也不能阻断本轮。
    pub fn referenced_agents(&self) -> Vec<&AgentConfig> {
        let used: std::collections::HashSet<&str> = self
            .active_links()
            .into_iter()
            .map(|index| self.links[index].host.as_str())
            .collect();
        self.agents
            .iter()
            .filter(|agent| used.contains(agent.id.as_str()))
            .collect()
    }

    pub fn validate(&self) -> Result<(), String> {
        if !adb_program(&self.adb_path)
            || !safe_word(&self.board_iperf)
            || (!self.serial.is_empty() && !safe_word(&self.serial))
        {
            return Err(
                "ADB 路径必须是本机上名为 adb 或 adb.exe 的文件（可带版本号如 adb-1.0.41，可含空格/反斜杠，不接受网络路径）；serial/board_iperf 只能含字母、数字及 _./:-"
                    .into(),
            );
        }
        // 双向单元的两条腿各占一个端口，所以端口上限留一格给 port + 1。
        if !(6..=3600).contains(&self.duration_secs)
            || !(1..=16).contains(&self.parallel)
            || self.port < 1024
            || self.port == u16::MAX
            || !(1..=10).contains(&self.repeats)
            || self.links.len() > 32
        {
            return Err(
                "duration_secs 需为 6..3600，parallel 为 1..16，port 为 1024..65534（双向占用 port 与 port+1），repeats 为 1..10，最多 32 条链路"
                    .into(),
            );
        }
        if self.protocols.is_empty() || self.directions.is_empty() {
            return Err("至少选择一种协议和一个方向".into());
        }
        if !unique(&self.protocols) || !unique(&self.directions) {
            return Err("协议和方向不能重复选择".into());
        }
        if self.ip_versions.is_empty()
            || !unique(&self.ip_versions)
            || self
                .ip_versions
                .iter()
                .any(|version| ![4, 6].contains(version))
        {
            return Err("IP 版本至少选择 IPv4 或 IPv6，且不能重复".into());
        }
        for streams in [self.tcp_streams, self.udp_streams].into_iter().flatten() {
            if !(1..=16).contains(&streams) {
                return Err("按协议覆盖的并发流数需为 1..16，留空则沿用 parallel".into());
            }
        }
        let udp = self.protocols.contains(&Protocol::Udp);
        if self
            .udp_mbps
            .is_some_and(|v| !v.is_finite() || v <= 0.0 || v > 1e6)
            || (udp && self.udp_mbps.is_none())
            || (!udp && self.udp_mbps.is_some())
        {
            return Err(
                "UDP 必须设置正数 udp_mbps（每条流 Mbps，上限 1000000）；不测 UDP 时不接受此项"
                    .into(),
            );
        }
        // -w / -l 原样进 iperf3 命令行，只放行「数字 + 可选 k/m/g」这一种形状。
        for (label, value) in [
            ("tcp_window", &self.tcp_window),
            ("udp_length", &self.udp_length),
        ] {
            if value.as_deref().is_some_and(|v| !size_token(v)) {
                return Err(format!("{label} 只能是数字加可选的 k/m/g，例如 4m、1400"));
            }
        }
        if self.tcp_window.is_some() && !self.protocols.contains(&Protocol::Tcp) {
            return Err("不测 TCP 时不接受 tcp_window".into());
        }
        if (self.udp_length.is_some() || self.max_udp_loss_pct.is_some()) && !udp {
            return Err("不测 UDP 时不接受 udp_length / max_udp_loss_pct".into());
        }
        if self
            .max_udp_loss_pct
            .is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v))
        {
            return Err("max_udp_loss_pct 需为 0..100".into());
        }
        let mut names = std::collections::HashSet::new();
        let mut hosts = std::collections::HashSet::from(["master"]);
        if self.agents.len() > 8 {
            return Err("最多配置 8 台辅测机".into());
        }
        for agent in &self.agents {
            if !safe_word(&agent.id)
                || !hosts.insert(&agent.id)
                || agent.port == 0
                || agent.address.trim().is_empty()
                || agent.address.len() > MAX_AGENT_ADDRESS_BYTES
                || agent.address.chars().any(char::is_control)
                || agent.token.len() > MAX_AGENT_TOKEN_BYTES
                || agent.token.chars().any(char::is_control)
            {
                return Err("辅测机标识须唯一且不能为 master，地址和端口必须有效".into());
            }
        }
        let bidir = self.directions.contains(&Direction::Bidir);
        let mut endpoints = std::collections::HashSet::new();
        for link in &self.links {
            if !hosts.contains(link.host.as_str()) {
                return Err(format!("{}: 未配置所属电脑 {}", link.name, link.host));
            }
            if link.name.trim().is_empty()
                || !names.insert(&link.name)
                || link.local_interface.trim().is_empty()
            {
                return Err("链路名称须非空且唯一，local_interface 不能为空".into());
            }
            if !link.board_rx_interface.is_empty() && !iface_word(&link.board_rx_interface) {
                return Err(format!(
                    "{}: 板侧统计接口只能含字母、数字及 _.:-",
                    link.name
                ));
            }
            if link.enabled && self.ip_versions.contains(&4) {
                for ip in [link.local_ip, link.gateway] {
                    if ip.is_unspecified()
                        || ip.is_loopback()
                        || ip.is_multicast()
                        || ip.is_broadcast()
                    {
                        return Err(format!("{}: 需要实际 LAN IPv4 单播地址", link.name));
                    }
                }
                if link.local_ip == link.gateway {
                    return Err(format!("{}: 本机 IP 不能等于板侧地址", link.name));
                }
            }
            for ip in [link.local_ipv6, link.gateway_ipv6].into_iter().flatten() {
                if ip.is_unspecified()
                    || ip.is_loopback()
                    || ip.is_multicast()
                    || ip.to_ipv4_mapped().is_some()
                {
                    return Err(format!(
                        "{}: 需要实际 LAN IPv6 单播地址，不能使用 IPv4 映射地址",
                        link.name
                    ));
                }
            }
            if link.enabled && self.ip_versions.contains(&6) {
                let (Some(local), Some(board)) = (link.local_ipv6, link.gateway_ipv6) else {
                    return Err(format!(
                        "{}: 已选择 IPv6，请填写电脑 IPv6 和 CPE LAN IPv6 地址",
                        link.name
                    ));
                };
                if local == board {
                    return Err(format!("{}: 电脑 IPv6 不能等于板侧 IPv6 地址", link.name));
                }
                if local.is_unicast_link_local() != board.is_unicast_link_local() {
                    return Err(format!(
                        "{}: 两端 IPv6 须同为链路本地地址或同为非链路本地地址",
                        link.name
                    ));
                }
            }
            // 同一台电脑上的同一个「网口 + 源 IP」重复出现，两条链路会抢同一
            // 块网卡的计数器，谁也说不清结果算谁的。
            if link.enabled {
                for &version in &self.ip_versions {
                    if !endpoints.insert((
                        link.host.as_str(),
                        link.local_interface.as_str(),
                        link.local_address(version),
                    )) {
                        return Err(format!(
                            "{}: 与另一条参与本轮的链路使用了同一台电脑的同一网口和源 IP",
                            link.name
                        ));
                    }
                }
            }
            for target in [
                link.upload_min_mbps,
                link.download_min_mbps,
                link.bidir_total_min_mbps,
                link.tool_upload_min_mbps,
                link.tool_download_min_mbps,
                link.tool_bidir_total_min_mbps,
            ]
            .into_iter()
            .flatten()
            {
                if !target.is_finite() || target <= 0.0 || target > 1e6 {
                    return Err(format!("{}: 门限需为正数 Mbps（上限 1000000）", link.name));
                }
            }
            if !link.measurement.uses_tool()
                && [
                    link.tool_upload_min_mbps,
                    link.tool_download_min_mbps,
                    link.tool_bidir_total_min_mbps,
                ]
                .iter()
                .any(Option::is_some)
            {
                return Err(format!(
                    "{}: 网卡计数严格模式不会用到工具口径，因此不接受工具口径门限",
                    link.name
                ));
            }
            if !bidir
                && [link.bidir_total_min_mbps, link.tool_bidir_total_min_mbps]
                    .iter()
                    .any(Option::is_some)
            {
                return Err(format!("{}: 未勾选双向并发时不接受双向合计门限", link.name));
            }
        }
        Ok(())
    }
}
