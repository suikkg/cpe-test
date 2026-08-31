# CPE Test 项目架构索引（AI 专用）

> 生成基准：2026-08-11。本文服务于 AI/代码代理的源码定位，不是面向终端用户的操作手册。
> 所有 `file:line` 均以本文生成时工作树中的 `nl -ba` 为准。源码是唯一权威；当本文、README 或旧说明冲突时，先读源码并更新本文。

> **引用约定**：本文只引用**模块路径与符号名**，不写行号。
>
> 本文档上一版把行号写进了正文（如 `load_config` 标注 `config.rs:228-271`），
> 到 v4.2.6 时全部失效——实测偏差 2.3×～21×（`Row` 标注 `report.rs:5-40`，实际在
> `:104`；`DbEnt` 标注 `executor.rs:786-797`，实际在 `:6628`）。这种"看似精确的
> 错误指引"比没有指引更危险：读者会直接跳到错误位置并据此判断。
> 需要定位时请用 `grep -n "fn <符号名>" <文件>`。

## 0.5 v6.0 的新模块与新不变量（本次架构变更索引）

> 下面这些是 v6.0 引入的，改动它们之前先读 `.ai/DESIGN-v6.0-architecture.md`
> 对应的 ADR，以及 `.ai/CHANGES-v6.0-verdict.md`（判定行为变更逐条说明）。

| 新模块 | 干什么 | 为什么存在（ADR） |
|---|---|---|
| `master/run_status.rs` | `RunStatus` / `UnitStatus` / `RunObserver` | 进度从「日志文本」变成结构化数据。executor 依赖 trait 不依赖 webui；CLI 传 `None` 行为零变化（ADR-2） |
| `master/executor/row.rs` | `RowIdentity` / `base_row` / `unit_row` | 报告行的**唯一**构造入口。加身份字段会让 10 个构造点全部编译失败——从「运行期空列」变成编译期错误（ADR-7） |
| `master/builder/{identity,policy,diagnostics}.rs` | resume identity、速率目标/链路策略、诊断单元 | 从 4359 行的 `builder.rs` 里按**改动的理由**分出来。`identity` 那份是 RESUME 承重面，不许「顺手清理」（R4） |
| `report/store.rs` | `rows.jsonl` + `meta.json` + `request.json` 读写 | 每单元增量落盘，报告可重放。崩溃损失从「整轮」变成「未完成的单元」（ADR-3）。`request.json` 是控制台发起这一轮时的 `RunRequest` 原文，「重新执行这一轮」唯一的输入（`meta.json` 里只有 `plan_hash`，那是摘要、反推不出计划）；命令行路径不写它 |
| `report/xlsx.rs` | `summary.xlsx` 四张表 | 第二个结果出口。**只吃类型化字段**，不许解析展示串（有结构断言）（ADR-7） |
| `master/webui/runs.rs` | `/api/runs`、`/api/runs/{id}/bundle.zip`、`/api/runs/report`、`/api/runs/request` | 远程访问者取回报告的唯一通道；报告的相对路径子资源撞「鉴权先于路由」，不能当静态站点服务（ADR-15、§13.3）。`report` 从 `rows.jsonl` 重放（**不要求先没有报告**——崩溃留下的那份可能是半截的；只挡正在跑的那一轮），`request` 回这一轮的计划原文供「重新执行」装载回控制台 |
| `ui/` | Vue 3 单文件产物 | 见 AGENTS.md §4 |

**v6.0 新增的结构断言**（都在 CI 的 `cargo test` 里）：

| 断言 | 守什么 |
|---|---|
| `every_production_row_is_built_through_the_shared_constructor` | 生产代码造 `Row` 只能走 `base_row`/`unit_row`，不许 `..Default::default()` |
| `the_leg_assembly_contracts_have_exactly_one_definition_in_the_tree` | 腿级判定装配的四个契约只能定义在 `rate.rs` / `rate_window.rs`（ADR-12） |
| `the_full_unit_expansion_is_byte_stable` | 稳定 ID / 端口顺序 / 单元展开的全量快照——**它红了先问「我是不是改了不该改的」** |
| `the_embedded_page_was_built_from_the_current_ui_sources` | 溯源戳：产物是不是从当前 `ui/` 源码构建的 |
| `every_verdict_label_round_trips` | `Verdict::label()` 与 `from_label()` 一一对应 |
| `the_summary_grid_has_one_cell_for_every_verdict` | 报告概览的统计格覆盖全部六个 verdict |

**判定层的三条新单源**（ADR-12，改之前读变更说明）：
`rate::effective_rate_target`（Observe/Discover 不比目标）、
`rate_window::offered_shortfall_explains_rx` + `offered_floor_mbps`（发送端没灌够的防误判）、
`WINDOW_COMPLETE_TOLERANCE_MS`（有效窗口容差，三条链共用）。

## 0. AI 阅读规则

1. 先从 `src/main.rs` 确认 CLI 模式，再沿调用链进入 `master/ui` 或 `agent/server`。
2. 双机 JSON 的字段、默认值和兼容性以 `src/protocol.rs`、`src/config.rs` 为准；改 HTTP 时必须同时检查 DTO、agent 路由和主控调用方。
3. 任务数量、顺序、端口和稳定 ID 是持久化兼容面。修改 `src/master/builder.rs` 前必须运行 builder 测试，并确认 RESUME 数据库影响。
4. 测试结果与报告字段由 `src/master/executor.rs` 共同定义；不要只改 `src/report.rs` 而遗漏 Row 构造。
5. 平台代码由 `cfg(windows)`、`cfg(target_os = "macos")` 和其他平台 stub 分隔。Windows 目标至少执行 `cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings` 与 MSVC 对应检查。
6. 当前 Serde 没有 `deny_unknown_fields`。配置中未声明的键会被忽略；不能根据未跟踪示例或 README 推断功能已实现。

## 1. 项目定位与构建

- 包名、版本、Rust edition 和描述在 `Cargo.toml`：crate `cpe_test`，Rust 2021。版本以 `Cargo.toml` 为准，不在本文重复。
- 通用依赖在 `Cargo.toml`：Serde/JSON、tiny_http、wait-timeout、regex、GBK 解码、Base64、chrono、Ctrl-C、MD5、PNG，以及 v6.0 新增的 `rust_xlsxwriter`（Excel 出口）。
- Windows API 依赖在 `Cargo.toml`：GetIfTable2、GDI 截图、控制台、DPI；release 开 LTO/strip。
- 顶层模块声明在 `src/main.rs`：`agent`、`cancel`、`clock`、`cmd`、`config`、`http_client`、`master`、`nic`、`parser_properties`、`ping`、`protocol`、`rate`、`report`、`resource`、`screenshot`、`util`、`verdict`、`console`。
- **`verdict` 是判定词汇表的唯一定义处**（v4.2.6 之后）：`Verdict`、`ExecutionStatus`、
  `HARD_SINGLE_UDP_FAILURE_CODES`、`aggregate_verdict`。`master::executor` 的
  `aggregate_unit_verdict` 与 `report` 的 `group_verdict` 都必须调用
  `verdict::aggregate_verdict`，**不得各自再写一份优先级**——这条约束由
  `verdict.rs` 里的结构断言 `verdict_priority_has_exactly_one_definition_in_the_tree`
  在 CI 中强制。历史上这两处分叉过，代价是两个静默错判。
- **`master::rate_window` 是正式速率判定口径的唯一实现处**（v4.2.6 之后）：
  `EffectiveWindow`、`RateStats`、`monitor_rate_stats`、`evaluate_nic_rx`
  及采样/滚动窗口覆盖率常量。这一层只依赖网卡计数器样本与目标速率，不接触
  进程、端口、HTTP 或线程，因此"采样不可信必须判 NOT_EVALUATED 而不是
  RATE_FAIL"这条铁律可以被单独审阅和测试。`executor` 的 UDP/TCP/CTS 三条
  路径都调用它。
- **`util` 只保留跨领域原语**（v4.2.6 之后）：子进程执行与解码、日志、时间、
  `lock_recover`、`sanitize`、`md5_hex`、`temp_file`、主机名/OS 名。领域性的
  东西已经各归各家——`cmd::tools`（iperf3/ctsTraffic 的定位与版本探测）、
  `console`（终端读行/提问/序号选择/打开文件）、`nic::same_slash24`
  （同 /24 判断是网络拓扑语义，不是字符串工具）。

### 1.1 依赖方向

```text
main
├── config
├── agent::server ── cmd::iperf / nic / ping / protocol / screenshot / util
├── master::ui ── config / http_client / nic / builder / executor / report / util
│   ├── master::builder ── config / protocol / util
│   │   └── builder::{identity, policy, diagnostics}
│   ├── master::run_status ── verdict（RunObserver trait；webui 提供实现）
│   └── master::executor ── builder / cmd::iperf / http_client / monitor / ping /
│                           protocol / report / report::store / run_status /
│                           screenshot / util
├── nic ── classify / monitor / (scan_windows | scan_macos) / cmd parsers
└── protocol（双机 JSON DTO 边界）
```

`cmd` 只负责系统命令封装和文本解析；`util` 提供进程、编码、日志、时间、文件名、选择和网段工具；`report` 只消费 `Row`，不执行网络操作。

**executor → run_status 是单向的**：executor 依赖 `RunObserver` 这个 trait，
webui 提供实现。加这条边**没有**让 executor 依赖 webui——这是 ADR-2 特意保住的
依赖方向。

**`report::store` 的位置**：它和 `report::html`/`report::xlsx` 平级，都是
`report::model` 的消费端。executor 只依赖 `model` + `store`。

## 2. CLI 与主流程

### 2.1 入口和模式

- `main()` 在 `src/main.rs`：初始化 Windows 控制台/DPI，读取参数；无参数结束时暂停窗口。
- `real_main()` 在 `src/main.rs`，模式如下。

| 模式 | 源码调用流 | 行为 |
|---|---|---|
| 无参数 | `main.rs` | 交互选择 master、agent 或本机 scan；默认 master |
| `agent` | `main.rs -> agent::run` | 读取配置，监听 agent HTTP，阻塞服务 |
| `master` | `main.rs -> master::ui::run_master` | 连接 agent、扫描双端、构建/执行任务、写报告 |
| `scan` | `main.rs -> nic::scan_host` | 只扫描并显示本机网卡 |
| `monitor` | `main.rs -> monitor::run_continuous` | 独立 RX 采样、打印 Mbps、可写 CSV |

- 帮助文本在 `src/main.rs`。
- 长短参数解析在 `src/main.rs`；当前短参数映射 `-i/-n/-c/-d` 分别为 interval/iface/csv/duration。
- CSV 前缀拆分在 `src/main.rs`。
- Windows UTF-8 控制台和 DPI 感知在 `src/main.rs`。

### 2.2 Master 完整调用流

`src/master/ui.rs` 是主控编排器：

1. 加载配置并应用 CLI 覆盖：agent host/port、前缀、resume、截图、是否打开报告。
2. 开启 `master_*.log` 并记录配置来源。
3. 读取/询问 agent 地址并保存 `.cpe_last_agent`。
4. 调用 `/health`（客户端）。
5. 扫描本机和调用 `/info`（客户端），无任一网卡则退出。
6. 预检两端 iperf3；缺失时 ping 仍可运行，iperf 会失败。
7.：有 `tests[]` 时按配置或交互二选一；`--auto` 没有 tests 会退出。
8. 调用 `builder::build_units`，打印跳过提示。
9. 非 auto 模式按 1-based 序号选择任务并确认。
10. 创建 `Ctx`、结果库和输出目录，调用 `Ctx::run_all`，最后停止本地 server。
11. 生成 `report_*.html`、打印汇总、按配置打开报告，并以 FAIL 数量决定退出码。

### 2.3 交互构建

- `interactive_build_specs`：`src/master/ui.rs`。
- 配对顺序：`enumerate_pairs` 先跨机全组合，再主控同机两两，再辅测同机两两；跨机双方均 UNKNOWN 的组合跳过。
- Endpoint 公共构造和同机配对：。
- 统一参数 DTO `UniversalParams`：；询问方向、类型、传输、IP 版本、UDP 限流、流数、时长、ping 计数和 payload：。
- 参数转 `SpecNorm`：；单个接口选择和菜单工具：。

## 3. 配置契约

### 3.1 顶层字段和默认值

`Config` 定义于 `src/config.rs`，默认值于：

| 字段 | 默认值 | 用途 |
|---|---|---|
| `agent_host` | `""` | 空值时交互询问 |
| `agent_port` | | agent HTTP 端口 |
| `ipv4_prefixes` | `["192.168."]` | NIC IPv4 前缀过滤 |
| `require_same_subnet_for_iperf` | `true` | 跨机 IPv4 iperf 要求同 /24，ping 不受限 |
| `limit_udp_by_link_speed` | `true` | 按发送网卡速率裁剪 UDP 流数，WiFi 不裁剪 |
| `screenshot` | `true` | 每个 iperf 单流/组执行后尝试双方截图 |
| `resume` | `false` | 跳过 24 小时内已 PASS 的 Unit |
| `open_report` | `true` | 完成后调用系统默认程序打开 HTML |
| `iperf` / `ping` | 各自 Default | 全局测试参数 |
| `tests` | `[]` | 配置驱动的测试规格 |

### 3.2 iperf、UDP、ping

- `IperfCfg` `src/config.rs`，默认 `duration=120`、TCP windows `64k/1m/4m`、UDP profiles `1m/100m/500m/1000m(-l 64)/2500m`：默认实现。
- `UdpProfile`：`bandwidth` 是 iperf 字符串，`length` 可选；带宽换算为 Mbps，稳定 profile name，显示 label。
- `PingCfg`：默认 `count=100`、`payload_sizes=[32]`。
- `TestSpec`：`src/dst` 必填；可选覆盖 iperf duration、ping count/payload、TCP windows、UDP profiles。
- 默认生成器：方向 A->B，类型 iperf，传输 TCP，IP v4，streams=1。
- `OneOrMany` 支持字符串/数组：A->B/AB/A>B -> `ab`；B->A/BA/B>A -> `ba`；bidir/A<->B/双向 -> `bidir`；旧 `both` -> `ab,ba`；去重保序，无有效值回退 `ab`。

### 3.3 配置加载与边界

- `load_config` `src/config.rs`：显式 `--config` > 当前目录 `config.json` > 可执行文件目录 `config.json` > 默认；只有找不到文件时读取兼容环境变量 `AUTOTEST_IPV4_PREFIXES`、`AUTOTEST_AGENT_HOST`。
- 文件读取和 UTF-8 BOM 容忍：。
- `#[serde(default)]` 允许缺省字段；没有 `deny_unknown_fields`，未知字段静默忽略。
- 根目录 `config.example.json` 是当前有效配置示例。

> **更正（v6.0 核查）**：上一版这里写着 `pairs`、`universal_params`、`agent_token`、
> `rate_check`、`ctstraffic`、`rate_mode`「不在 `Config` 中，不能写成已实现功能」。
> **这六个字段现在全都在 `Config` 里**（`grep -n "pub pairs" src/config.rs` 即可核实），
> 而且 `pairs` 是**全部 6 份出厂配置的主通路**。按旧描述去判断会得出完全相反的结论。

### 3.1.1 测试来源有三种，各服务一类用户

`Config.tests[]` 不是唯一入口。三条来源并存是**有意的**，不是历史包袱：

| 来源 | 长什么样 | 谁在用 | v6.0 的处置 |
|---|---|---|---|
| `tests[]` 显式列举 | 每条测试一个 `TestSpec` | 界面导出的 config、手写精调 | 不动 |
| `pairs` + `universal_params` 自动配对 | 给一批网口 + 一组通用参数，由 `generate_specs_from_pairs` 展开 | **全部 6 份出厂预设**（`config.example.json` + `dist/configs/*.json`）；无人值守/批量回归的主通路 | **零改动**（ADR-13 明确保护） |
| 交互式构建 | `master/ui.rs` 的菜单现场问出来 | 命令行手动跑一次 | 不动 |

界面（快速工作台 + 项目文件）产出的是第一种。**改配置解析时三条路都要过一遍**——
它们共用 `spec_from_config` 之后的全部管线，但入口的默认值填充各不相同。

## 4. HTTP 协议

### 4.1 通用规则

- DTO 和统一包装 `Resp<T>{ok,error,data}` 在 `src/protocol.rs`；成功用 `ok_json`，业务错误用 `err_json`。
- 所有 agent 响应的 HTTP 状态为 200，业务失败放在 `ok=false`；panic 在 `src/agent/server.rs` 捕获并只包装一次。
- agent 请求体读取上限 100 MiB：`server.rs`。
- 空请求体解析为 DTO `Default`，错误文本来自 `server.rs`。

### 4.2 端点表

路由实现集中在 `src/agent/server.rs`；DTO 定义在 `src/protocol.rs`。

| HTTP | 请求 -> 响应 | 路由/DTO 行号 |
|---|---|---|
| `GET /health`、`POST /health` | 无请求 -> `Resp<HealthOut>` | 路由；`HealthOut` |
| `POST /info` | `InfoReq` -> `Resp<HostInfo>` | 路由；`InfoReq` `HostInfo` |
| `POST /ping` | `PingReq` -> `Resp<PingOut>` | 路由；`PingReq/PingOut` |
| `POST /iperf/server/start` | `IperfServerStartReq` -> `Resp<IperfServerStartOut>` | 路由；DTO |
| `POST /iperf/server/stop` | `IperfServerStopReq` -> `Resp<IperfServerStopOut>` | 路由；DTO |
| `POST /iperf/client/run` | `IperfClientReq` -> `Resp<IperfClientOut>` | 路由；DTO |
| `POST /monitor/start` | `MonitorStartReq` -> `Resp<MonitorStartOut>` | 路由；DTO |
| `POST /monitor/stop` | `MonitorStopReq` -> `Resp<MonitorStopOut>` | 路由；DTO |
| `POST /screenshot` | `ScreenshotReq` -> `Resp<ScreenshotOut>`（PNG Base64） | 路由；DTO |

### 4.3 主控 HTTP 客户端

`src/http_client.rs` 实现零额外 HTTP 依赖的 HTTP/1.1 客户端：5 秒连接超时、调用方读超时、30 秒写超时；GET 发送空 body/Content-Length 0；POST 以 UTF-8 字节长度设置 Content-Length。响应支持 Content-Length、读到 EOF 和 chunked；chunk 解码，Content-Length/状态解析。

## 5. NIC 扫描、分类与监控

### 5.1 公共入口

- `src/nic/mod.rs` 的 `scan_host` 按平台扫描，随后按 `role_rank` 和接口名排序。
- IPv4 前缀判断：空列表全放行，非空任一非空前缀匹配即可。
- 展示表 同时输出角色、接口、IPv4、速率、WiFi 频段和 v6 link-local。

### 5.2 角色分类

- 排序常量 `src/nic/classify.rs`：10GETH、10GUSB、SGMII2.5G、SGMII1G、RNDIS、WiFi 系列、UNKNOWN。
- 分类 优先级：WiFi 频段；描述 10g+usb；RNDIS 关键字；4001-8999 的 USB 10G 兼容档；9000-12000 以太 10G；2.5G；1G；3400-4000 RNDIS 兜底；否则 UNKNOWN。
- `role_rank`；Windows 名称 WiFi 兜底。

### 5.3 Windows

`src/nic/scan_windows.rs` 定义 GetIfTable2 行；`if_rows` 采集别名、描述、接口索引、速率、WiFi 类型和 RX octets；UTF-16 处理；RX 查询；完整扫描/合并 ipconfig、GetIfTable2、netsh。

Windows 文本适配器：`src/cmd/ipconfig.rs` 解析中英文 `ipconfig /all`；`src/cmd/netsh.rs` 解析 WiFi 名称、连接状态和频段。两者测试位于各自。

### 5.4 macOS 与其他平台

- macOS `src/nic/scan_macos.rs` 解析 ifconfig block； 解析硬件端口； 探测速率； 获取 WiFi 频段/PHY；完整扫描。
- macOS 监控实现 `src/nic/monitor.rs` 使用 `netstat -ibn`；`parse_netstat_ib` 取 Link 行 Ibytes。
- 非 Windows/macOS 的 NIC 扫描返回空列表（`nic/mod.rs`），RX 和截图分别返回“不支持”错误（`monitor.rs`、`screenshot.rs`）。

### 5.5 RX 监控

- 注册表 `MonitorMgr` `src/nic/monitor.rs`：start 保存累计字节和时间，stop 用差值计算平均 Mbps，ID 为 `monN`，sweep 清理过期条目。
- 独立连续监控选项和循环：Ctrl+C、间隔/时长、实时输出、可选 CSV。
- CSV 摘要重写：接口、间隔、时长、平均/峰值和采样明细。

## 6. 任务构建模型与不变量

### 6.1 中间模型

定义在 `src/master/builder.rs`：

- `PORT_BASE=56000`；`Side`。
- `Endpoint` 保存 side、PC、`NicInfo`，`key` 用于禁止同一网口作为源/目标。
- `SpecNorm` 是配置和交互的共同规范格式。
- `IperfTask` 保存 v4/v6、TCP/UDP、profile label、源/目标、端口、时长、额外参数和流索引。
- `PingTask` 保存 v4/v6、源/目标、计数和 payload。
- `LegKind` 为单流 iperf、UDP 多流组或 ping；`Leg` 带 `""/ab/ba` 标签；`Unit` 是 RESUME 和执行的最小单元。
- IPv6 三元组 `V6Addrs`；选择 link-local 优先、否则 global。

### 6.2 配置解析与展开

- endpoint 角色/NAME 解析；配置 TestSpec -> SpecNorm，streams clamp 到 1..32，iperf duration 1..86400，ping count 1..100000。
- UDP 发送口限流：WiFi、WIFI 角色、未知速率或非法带宽不裁剪；否则 `floor(speed/bandwidth)` 与请求流数取最小。
- 方向腿 `dir_pairs`：ab 一腿、ba 一腿、bidir 按 `[ab,ba]` 两腿。
- 共享 materializer `map_legs` 和 `unit` 消除了 TCP/ping 重复初始化。
- Unit 生成主循环：先方向，再 IP 版本，再 iperf/ping；跨机 IPv4 iperf 可受同 /24 门禁，ping 不受门禁。

### 6.3 端口、流和稳定 ID

- 主流程在 `src/master/ui.rs` 将端口游标初始化为 `PORT_BASE=56000`；`alloc_port` `builder.rs` 返回当前端口并递增，达到 65535 后回绕到 56000，因此只在回绕前单调且不重复。
- TCP：每个 window 生成一个 Unit，腿内使用 `-w <window> -P <streams>`，端口按腿顺序分配。
- UDP：每个 profile 生成一个 Unit；每条腿根据发送口得到流数，多于 1 流时每流独立端口/进程；任一腿为 0 则整个 profile Unit 跳过。
- Ping：每个 payload 一个 Unit，构造。
- TCP ID 模板：`iperf_v1|V4/V6|tcp|profile|duration|src-id|dst-id|direction`。
- UDP ID 模板：另含 `streams`。
- Ping ID 模板：`ping_v1|count|payload|V4/V6|src-id|dst-id|direction`。
- 修改 ID 模板、字段顺序或 `v1` 会使旧 `task_results.json` 的 RESUME 命中失效；这是有意的兼容边界。

## 7. 执行器、判定、截图与 RESUME

### 7.1 Ctx 与远端统一调用

- `Ctx` `src/master/executor.rs` 持有 agent 地址、配置、输出目录、本地 server/monitor、线程安全 Row 和 ResultDb。
- `agent_post` 统一序列化、POST、HTTP 状态、`Resp<T>` 解析、业务错误和缺 data。
- 双端 ping/iperf server/client/monitor 适配；截图低层获取，截图开关和文件保存。
- Agent 截图日志保留状态/长度、HTTP 前缀、JSON 前缀、业务错误、缺 data 和 Base64 长度；`byte_prefix` 以 UTF-8 边界安全截断。主控截图失败仍静默跳过。

### 7.2 调度与双向

- `run_all` 顺序遍历 Unit；多腿 Unit 用 scoped threads 并行。
- `resume=true` 时在执行前查询 `ResultDb::fresh_pass`；跳过行是 `ok=None`，计入 skip。
- 双向两腿完成后 互填 `peer_rx`，保留三位精度和对向 tag。
- Unit PASS 必须所有腿 `LegOutcome.ok=true`；空腿视为 FAIL；每个 Unit 结果写回数据库并暂停 1 秒。

### 7.3 Ping

- `run_ping_leg` `src/master/executor.rs` 选择 v4/v6 地址、调用本地或 agent ping、生成 Row 和 raw 输出。
- `src/ping.rs` 构造 Windows `ping` 或 macOS/BSD `ping/ping6`；执行。
- 解析 `ping.rs`：中英文 Windows、BSD/macOS；只认真实 RTT 的 reply，修正目标不可达/ICMP 错误被统计为 received 的假成功。

### 7.4 iperf 单流和 UDP 组

- server/client/stop 核心 `exec_iperf_core` `src/master/executor.rs`；IPv6 zone 处理。
- 单流 `run_iperf_single`：接收端 monitor、client/server 输出解析、PASS、截图和 Row。
- UDP 并发组 `run_iperf_group`：错峰 200ms 起流、共享接收端 monitor、每流 Row、组合计 Row、组合 PASS。
- `iperf_row` 是单流/组内流/组合计的公共 Row 基底，字段必须与 Unit/Task 对齐。
- 单流 PASS：client 成功且未超时，并且 iperf 文本有正测量或接收网卡 RX > `MIN_VALID_RX_MBPS=0.01`；组内每流需文本测量，组合还需 RX > 阈值。

### 7.5 ResultDb / RESUME

- `DbEnt`、`ResultDb` 和 24 小时常量 `src/master/executor.rs`。
- 加载 JSON；fresh PASS 判断：要求 `ok=true`、`age.num_hours() <= 24` 且未来偏差不超过 60 秒；由于整小时截断，过去记录实际可命中到不足 25 小时。
- `set`；原子临时文件写入和 rename。

## 8. iperf、命令与公共基础设施

### 8.1 iperf 适配

`src/cmd/iperf.rs`：

- 常量和 server/client 参数构造；TCP 用 `-P`，UDP 用 `-u` 加额外 `-b/-l`。
- `IperfParsed` 和最佳 sender/receiver/measurement 判定；文本速率、Bytes/bits、单位换算和 UDP 丢包解析。
- `IperfServerMgr`：同端口先停旧 server，后台收集 stdout，TCP connect 探测 ready，主动 stop/kill，sweep/stop_all。
- 就绪探测：去掉 IPv6 zone，解析地址一次，200ms 轮询，超时 15 秒。
- client 瞬态错误和重试：最多 3 次，单次总超时为 duration+120 秒，保留实时输出和 stderr。

### 8.2 公共 util

- 编码和命令结果 `src/util.rs`；GBK fallback 适配中文 Windows。
- piped 子进程 helper；阻塞执行/超时 kill；实时流式执行、无尾换行回调和 stderr。
- 日志；时间；文件名安全化；主机/OS。
- iperf3 定位/版本：程序同目录优先，再查 PATH。
- 交互输入；打开报告；MD5；临时文件。
- 选择解析：空输入全选、逗号/范围、去重保序和边界错误；同 /24。

## 9. 报告与截图

### 9.1 Row 和 HTML

- `Row` 字段定义在 `src/report.rs`：排序键、任务/父 ID、源/目标、状态、接收/对向/发送/接收速率、UDP/ping 指标、截图、命令、raw、组合计标记。
- `ReportMeta`：主控、辅测、agent host、开始/结束/耗时。
- HTML 辅助函数：截图链接、HTML 转义、数值格式、 PASS/FAIL/SKIP 映射。
- `write_report`：按 sort_key 排序；组合计不进入总数；输出元数据、统计、25 列表格和原始输出 details；字段均转义。
- Task/Parent ID 在表格显示前用 `short8` 截 8 个 Unicode 字符：。

### 9.2 截图实现

- macOS `src/screenshot.rs` 调用系统 `screencapture` 临时 PNG。
- Windows GDI 主屏抓取：GetDC/CreateCompatibleBitmap/BitBlt/GetDIBits，BGRA 转 RGBA，再编码 PNG。
- 其他平台固定错误。
- PNG 编码；测试在 解码回读 2x2 RGBA，而不只检查魔数。

## 10. 测试覆盖索引（源码声明 52 项；macOS 52、Linux 51、Windows 50）

| 区域 | 测试位置 | 覆盖 |
|---|---|---|
| 配置 | `config.rs` | 默认值抽样、代表性 JSON 反序列化及部分缺省字段、bidir/方向数组/both 展开、UDP profile 带宽换算/名称/标签 |
| CLI | `main.rs` | 长短 flags、值/开关规则、CSV 拆分 |
| builder | `builder.rs` | TCP/UDP 稳定 ID、端口、双向组、UDP 限流/WiFi 豁免、同 /24、ping、IPv6 |
| executor | `executor.rs` | ResultDb 保存/加载、刚写入 PASS 命中、未知 ID、失败覆盖 |
| UI | `ui.rs` | 跨机优先、同机配对顺序、UNKNOWN 过滤 |
| iperf | `cmd/iperf.rs` | TCP/UDP 结果解析、Gbits/sec 与 MBytes/sec 换算、无测量数据的错误输出、瞬态错误判定、client/server 参数构造 |
| ping | `ping.rs` | 中文/英文/BSD、全丢、不可达假成功、部分成功 |
| Windows parser | `cmd/ipconfig.rs`、`cmd/netsh.rs` | 中英文适配器、WiFi 状态/频段 |
| NIC 分类 | `nic/classify.rs` | 角色、排序、WiFi 名称；USB 4000/4001/8999/12000 与以太网 8999/9000/12001 分类样例 |
| macOS NIC/监控 | `scan_macos.rs`、`monitor.rs` | ifconfig、netstat Ibytes |
| HTTP/agent | `http_client.rs`、`agent/server.rs` | Content-Length/状态行解析、chunked 正常及非法大小、tiny_http POST 回环、空 body 默认请求、非法 JSON 错误单次包装 |
| util | `util.rs` | 选择解析、同 /24、sanitize、run_cmd 成功/启动错误、非 Windows streaming 无尾换行 stdout 回调/收集及 stderr |
| report/screenshot | `report.rs`、`screenshot.rs` | PASS/SKIP、转义、排序/组合计、截图链接；PNG 实际解码 |

## 11. 不可破坏的不变量与修改入口

### 11.1 必须保持

- 主流程从 56000 开始递增分配端口，达到 65535 后回绕到 56000；TCP 使用一个 client 的 `-P`，UDP 多流使用独立进程/端口；bidir 始终是 `[ab,ba]` 两腿。
- 稳定 ID 模板和字段顺序见 `builder.rs`；其输入构造还包括 `ep_id` TCP profile 名，以及 `config.rs`/`builder.rs` 的 UDP profile 名。改变模板、字段顺序或任一输入规范会让历史 RESUME 不再命中。
- IPv4 同 /24 门禁只限制跨机 iperf；ping 不受限。IPv6 优先双端 link-local，其次 global；macOS 执行时加 zone，Windows 不加。
- UDP 限流按每条腿的发送 NIC；WiFi/未知速率不裁剪；任一腿不能承载 profile 就跳过整个 Unit。
- PASS 规则：ping 见 `ping.rs` 与 `executor.rs`；iperf core、单流、组内流、组汇总和 Unit 分别见 `executor.rs`；组合计行在 `executor.rs` 标记，并由 `report.rs` 排除在报告总数外。
- RESUME 是 Unit 级；当前按 `age.num_hours() <= 24` 判断，过去记录实际可命中到不足 25 小时，并容忍未来时间 60 秒（`executor.rs`）。agent HTTP 线程池固定 16 worker，但 iperf3/CTS 的 client 作业各自跑在独立命名线程（`iperf-client-<id>`）上，`/iperf/client/start` 立即返回 job id，**并发流数不受 16 的限制**；每 30 秒 sweep，server/monitor 最大存活分别为 10/30 分钟（`agent/server.rs`）。
- 对外 JSON 字段即使当前生产代码没有本地消费者，也属于协议兼容面；删除/重命名要同步所有端点和版本策略。

### 11.2 常见修改入口

| 需求 | 首先改 | 必须联检 |
|---|---|---|
| CLI/新参数 | `main.rs`；master 参数另看 `master/ui.rs` | `main.rs` 的帮助 的解析测试、README |
| 配置字段/默认 | `config.rs` | `config.rs`、`config.example.json`、README，以及 `main.rs`、`master/ui.rs`、`master/builder.rs`、`master/executor.rs`、`agent/server.rs` 中对应消费者 |
| HTTP DTO/端点 | `protocol.rs` 或 `agent/server.rs` | `http_client.rs`、`master/ui.rs`、`master/executor.rs`、对应实现及 `agent/server.rs` 的解析/错误包装测试 |
| 任务数量/顺序/ID/端口 | `builder.rs` | `builder.rs`、`master/ui.rs`、`executor.rs`、executor 的 `sort_key` 构造与 `report.rs` |
| PASS/并发/监控/截图/RESUME | `executor.rs` | `executor.rs`、builder Unit/legs、`cmd/iperf.rs`、`ping.rs`、`nic/monitor.rs`、`screenshot.rs`、agent 对应端点及 `report.rs` |
| iperf 命令/解析/进程 | `cmd/iperf.rs` | `cmd/iperf.rs`、`protocol.rs`、`agent/server.rs`、`master/executor.rs`、`util::run_streaming` |
| ping 命令/解析 | `ping.rs` | `ping.rs`、`protocol.rs`、`agent/server.rs`、`executor.rs` |
| NIC 角色 | `nic/classify.rs` | `nic/classify.rs`、`nic/mod.rs`、Windows/macOS 扫描、`builder.rs` 及 UI 角色选择 |
| 平台采集/监控 | `nic/mod.rs`、`nic/scan_windows.rs`、`nic/scan_macos.rs`、`nic/monitor.rs`、`cmd/ipconfig.rs`、`cmd/netsh.rs` | Windows GNU/MSVC；`ipconfig.rs`、`netsh.rs`、`scan_macos.rs`、`monitor.rs`，以及 main/agent/executor 调用方 |
| 报告列/HTML | `report.rs` | `executor.rs` 的全部 Row 构造；`report.rs` 的转义、排序/组合计、PASS/SKIP、截图链接测试（当前无 FAIL/golden） |

## 12. 变更与验证记录

- 本次重构前 Rust 物理行数：；生产区（每文件首个 `#[cfg(test)]` 前）。
- 最终 Rust 物理行数：；生产物理行数：；生产区净减少 行，全部 Rust 净减少 行。
- 测试从基线 增加到，没有通过删除测试获得减量。
- 已验证：`cargo fmt --all -- --check`、`cargo test --all-targets --locked`（52/52）、本机严格 Clippy、Windows GNU/MSVC 严格 Clippy、`git diff --check`。
- 旧说明 `使用说明.md:276-277` 关于 iperf server `-1`/netstat LISTEN 探测已过时；当前实现是无 `-1`、主动 stop、TCP connect ready 探测（`cmd/iperf.rs`）。维护 AI 文档时以当前实现为准。
