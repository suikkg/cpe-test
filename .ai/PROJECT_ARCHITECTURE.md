# CPE Test 项目架构索引（AI 专用）

> 生成基准：2026-08-11。本文服务于 AI/代码代理的源码定位，不是面向终端用户的操作手册。
> 所有 `file:line` 均以本文生成时工作树中的 `nl -ba` 为准。源码是唯一权威；当本文、README 或旧说明冲突时，先读源码并更新本文。

## 0. AI 阅读规则

1. 先从 `src/main.rs:24-199` 确认 CLI 模式，再沿调用链进入 `master/ui` 或 `agent/server`。
2. 双机 JSON 的字段、默认值和兼容性以 `src/protocol.rs:5-219`、`src/config.rs:6-278` 为准；改 HTTP 时必须同时检查 DTO、agent 路由和主控调用方。
3. 任务数量、顺序、端口和稳定 ID 是持久化兼容面。修改 `src/master/builder.rs:307-526` 前必须运行 builder 测试，并确认 RESUME 数据库影响。
4. 测试结果与报告字段由 `src/master/executor.rs:273-349`、`363-721`、`732-757` 共同定义；不要只改 `src/report.rs` 而遗漏 Row 构造。
5. 平台代码由 `cfg(windows)`、`cfg(target_os = "macos")` 和其他平台 stub 分隔。Windows 目标至少执行 `cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings` 与 MSVC 对应检查。
6. 当前 Serde 没有 `deny_unknown_fields`。配置中未声明的键会被忽略；不能根据未跟踪示例或 README 推断功能已实现。

## 1. 项目定位与构建

- 包名、版本、Rust edition 和描述在 `Cargo.toml:1-5`：crate `cpe_test`，版本 `3.0.0`，Rust 2021。
- 通用依赖在 `Cargo.toml:7-18`：Serde/JSON、tiny_http、wait-timeout、regex、GBK 解码、Base64、chrono、Ctrl-C、MD5 和 PNG。
- Windows API 依赖在 `Cargo.toml:20-28`：GetIfTable2、GDI 截图、控制台、DPI；release 的 LTO/strip 在 `Cargo.toml:30-32`。
- 顶层模块声明在 `src/main.rs:9-19`：`agent`、`cmd`、`config`、`http_client`、`master`、`nic`、`ping`、`protocol`、`report`、`screenshot`、`util`。

### 1.1 依赖方向

```text
main
├── config
├── agent::server ── cmd::iperf / nic / ping / protocol / screenshot / util
├── master::ui ── config / http_client / nic / builder / executor / report / util
│   ├── master::builder ── config / protocol / util
│   └── master::executor ── builder / cmd::iperf / http_client / monitor / ping /
│                           protocol / report / screenshot / util
├── nic ── classify / monitor / (scan_windows | scan_macos) / cmd parsers
└── protocol（双机 JSON DTO 边界）
```

`cmd` 只负责系统命令封装和文本解析；`util` 提供进程、编码、日志、时间、文件名、选择和网段工具；`report` 只消费 `Row`，不执行网络操作。

## 2. CLI 与主流程

### 2.1 入口和模式

- `main()` 在 `src/main.rs:24-36`：初始化 Windows 控制台/DPI，读取参数；无参数结束时暂停窗口。
- `real_main()` 在 `src/main.rs:38-199`，模式如下。

| 模式 | 源码调用流 | 行为 |
|---|---|---|
| 无参数 | `main.rs:167-191` | 交互选择 master、agent 或本机 scan；默认 master |
| `agent` | `main.rs:41-50 -> agent::run` | 读取配置，监听 agent HTTP，阻塞服务 |
| `master` | `main.rs:51-63 -> master::ui::run_master` | 连接 agent、扫描双端、构建/执行任务、写报告 |
| `scan` | `main.rs:64-76 -> nic::scan_host` | 只扫描并显示本机网卡 |
| `monitor` | `main.rs:77-162 -> monitor::run_continuous` | 独立 RX 采样、打印 Mbps、可写 CSV |

- 帮助文本在 `src/main.rs:201-233`。
- 长短参数解析在 `src/main.rs:235-272`；当前短参数映射 `-i/-n/-c/-d` 分别为 interval/iface/csv/duration。
- CSV 前缀拆分在 `src/main.rs:274-279`。
- Windows UTF-8 控制台和 DPI 感知在 `src/main.rs:281-290`。

### 2.2 Master 完整调用流

`src/master/ui.rs:35-297` 是主控编排器：

1. `35-54` 加载配置并应用 CLI 覆盖：agent host/port、前缀、resume、截图、是否打开报告。
2. `56-69` 开启 `master_*.log` 并记录配置来源。
3. `71-89` 读取/询问 agent 地址并保存 `.cpe_last_agent`。
4. `91-118` 调用 `/health`（客户端 `301-309`）。
5. `120-141` 扫描本机和调用 `/info`（客户端 `311-326`），无任一网卡则退出。
6. `143-155` 预检两端 iperf3；缺失时 ping 仍可运行，iperf 会失败。
7. `157-191`：有 `tests[]` 时按配置或交互二选一；`--auto` 没有 tests 会退出。
8. `193-203` 调用 `builder::build_units`，打印跳过提示。
9. `205-236` 非 auto 模式按 1-based 序号选择任务并确认。
10. `238-255` 创建 `Ctx`、结果库和输出目录，调用 `Ctx::run_all`，最后停止本地 server。
11. `256-297` 生成 `report_*.html`、打印汇总、按配置打开报告，并以 FAIL 数量决定退出码。

### 2.3 交互构建

- `interactive_build_specs`：`src/master/ui.rs:330-415`。
- 配对顺序：`enumerate_pairs` `417-434` 先跨机全组合，再主控同机两两，再辅测同机两两；跨机双方均 UNKNOWN 的组合跳过。
- Endpoint 公共构造和同机配对：`436-460`。
- 统一参数 DTO `UniversalParams`：`462-473`；询问方向、类型、传输、IP 版本、UDP 限流、流数、时长、ping 计数和 payload：`475-577`。
- 参数转 `SpecNorm`：`579-602`；单个接口选择和菜单工具：`604-692`。

## 3. 配置契约

### 3.1 顶层字段和默认值

`Config` 定义于 `src/config.rs:6-27`，默认值于 `29-45`：

| 字段 | 默认值 | 用途 |
|---|---|---|
| `agent_host` | `""` | 空值时交互询问 |
| `agent_port` | `28801` | agent HTTP 端口 |
| `ipv4_prefixes` | `["192.168."]` | NIC IPv4 前缀过滤 |
| `require_same_subnet_for_iperf` | `true` | 跨机 IPv4 iperf 要求同 /24，ping 不受限 |
| `limit_udp_by_link_speed` | `true` | 按发送网卡速率裁剪 UDP 流数，WiFi 不裁剪 |
| `screenshot` | `true` | 每个 iperf 单流/组执行后尝试双方截图 |
| `resume` | `false` | 跳过 24 小时内已 PASS 的 Unit |
| `open_report` | `true` | 完成后调用系统默认程序打开 HTML |
| `iperf` / `ping` | 各自 Default | 全局测试参数 |
| `tests` | `[]` | 配置驱动的测试规格 |

### 3.2 iperf、UDP、ping

- `IperfCfg` `src/config.rs:47-56`，默认 `duration=120`、TCP windows `64k/1m/4m`、UDP profiles `1m/100m/500m/1000m(-l 64)/2500m`：默认实现 `58-75`。
- `UdpProfile` `77-123`：`bandwidth` 是 iperf 字符串，`length` 可选；带宽换算为 Mbps `92-108`，稳定 profile name `110-115`，显示 label `117-122`。
- `PingCfg` `125-139`：默认 `count=100`、`payload_sizes=[32]`。
- `TestSpec` `141-173`：`src/dst` 必填；可选覆盖 iperf duration、ping count/payload、TCP windows、UDP profiles。
- 默认生成器 `175-189`：方向 A->B，类型 iperf，传输 TCP，IP v4，streams=1。
- `OneOrMany` `191-226` 支持字符串/数组：A->B/AB/A>B -> `ab`；B->A/BA/B>A -> `ba`；bidir/A<->B/双向 -> `bidir`；旧 `both` -> `ab,ba`；去重保序，无有效值回退 `ab`。

### 3.3 配置加载与边界

- `load_config` `src/config.rs:228-271`：显式 `--config` > 当前目录 `config.json` > 可执行文件目录 `config.json` > 默认；只有找不到文件时读取兼容环境变量 `AUTOTEST_IPV4_PREFIXES`、`AUTOTEST_AGENT_HOST`。
- 文件读取和 UTF-8 BOM 容忍：`273-278`。
- `#[serde(default)]` 允许缺省字段；没有 `deny_unknown_fields`，未知字段静默忽略。
- 根目录 `config.example.json:1-51` 是当前有效配置示例。README/未跟踪 smoke 配置里的 `pairs`、`universal_params`、`agent_token`、`rate_check`、`ctstraffic`、`rate_mode` 等不在 `Config` 中，不能写成已实现功能。

## 4. HTTP 协议

### 4.1 通用规则

- DTO 和统一包装 `Resp<T>{ok,error,data}` 在 `src/protocol.rs:60-82`；成功用 `ok_json` `68-75`，业务错误用 `err_json` `77-82`。
- 所有 agent 响应的 HTTP 状态为 200，业务失败放在 `ok=false`；panic 在 `src/agent/server.rs:127-131` 捕获并只包装一次。
- agent 请求体读取上限 100 MiB：`server.rs:22-27,119-124`。
- 空请求体解析为 DTO `Default`，错误文本来自 `server.rs:223-228`。

### 4.2 端点表

路由实现集中在 `src/agent/server.rs:142-220`；DTO 定义在 `src/protocol.rs`。

| HTTP | 请求 -> 响应 | 路由/DTO 行号 |
|---|---|---|
| `GET /health`、`POST /health` | 无请求 -> `Resp<HealthOut>` | 路由 `145-150`；`HealthOut` `211-219` |
| `POST /info` | `InfoReq` -> `Resp<HostInfo>` | 路由 `151-159`；`InfoReq` `84-90`、`HostInfo` `52-58` |
| `POST /ping` | `PingReq` -> `Resp<PingOut>` | 路由 `160-167`；`PingReq/PingOut` `92-118` |
| `POST /iperf/server/start` | `IperfServerStartReq` -> `Resp<IperfServerStartOut>` | 路由 `168-176`；DTO `120-133` |
| `POST /iperf/server/stop` | `IperfServerStopReq` -> `Resp<IperfServerStopOut>` | 路由 `177-183`；DTO `135-147` |
| `POST /iperf/client/run` | `IperfClientReq` -> `Resp<IperfClientOut>` | 路由 `184-199`；DTO `149-173` |
| `POST /monitor/start` | `MonitorStartReq` -> `Resp<MonitorStartOut>` | 路由 `200-204`；DTO `175-184` |
| `POST /monitor/stop` | `MonitorStopReq` -> `Resp<MonitorStopOut>` | 路由 `205-208`；DTO `186-196` |
| `POST /screenshot` | `ScreenshotReq` -> `Resp<ScreenshotOut>`（PNG Base64） | 路由 `209-218`；DTO `198-209` |

### 4.3 主控 HTTP 客户端

`src/http_client.rs:11-98` 实现零额外 HTTP 依赖的 HTTP/1.1 客户端：5 秒连接超时、调用方读超时、30 秒写超时；GET 发送空 body/Content-Length 0；POST 以 UTF-8 字节长度设置 Content-Length。响应支持 Content-Length、读到 EOF 和 chunked；chunk 解码 `101-128`，Content-Length/状态解析 `130-144`。

## 5. NIC 扫描、分类与监控

### 5.1 公共入口

- `src/nic/mod.rs:16-38` 的 `scan_host` 按平台扫描，随后按 `role_rank` 和接口名排序。
- IPv4 前缀判断 `40-46`：空列表全放行，非空任一非空前缀匹配即可。
- 展示表 `49-76` 同时输出角色、接口、IPv4、速率、WiFi 频段和 v6 link-local。

### 5.2 角色分类

- 排序常量 `src/nic/classify.rs:3-15`：10GETH、10GUSB、SGMII2.5G、SGMII1G、RNDIS、WiFi 系列、UNKNOWN。
- 分类 `17-60` 优先级：WiFi 频段；描述 10g+usb；RNDIS 关键字；4001-8999 的 USB 10G 兼容档；9000-12000 以太 10G；2.5G；1G；3400-4000 RNDIS 兜底；否则 UNKNOWN。
- `role_rank` `62-67`；Windows 名称 WiFi 兜底 `69-77`。

### 5.3 Windows

`src/nic/scan_windows.rs:15-24` 定义 GetIfTable2 行；`if_rows` `26-63` 采集别名、描述、接口索引、速率、WiFi 类型和 RX octets；UTF-16 处理 `65-68`；RX 查询 `70-77`；完整扫描/合并 ipconfig、GetIfTable2、netsh `79-136`。

Windows 文本适配器：`src/cmd/ipconfig.rs:9-119` 解析中英文 `ipconfig /all`；`src/cmd/netsh.rs:9-79` 解析 WiFi 名称、连接状态和频段。两者测试位于各自 `121-199`、`81-154`。

### 5.4 macOS 与其他平台

- macOS `src/nic/scan_macos.rs:14-63` 解析 ifconfig block；`65-83` 解析硬件端口；`85-107` 探测速率；`109-142` 获取 WiFi 频段/PHY；完整扫描 `144-203`。
- macOS 监控实现 `src/nic/monitor.rs:21-26` 使用 `netstat -ibn`；`parse_netstat_ib` `33-52` 取 Link 行 Ibytes。
- 非 Windows/macOS 的 NIC 扫描返回空列表（`nic/mod.rs:22-26`），RX 和截图分别返回“不支持”错误（`monitor.rs:28-31`、`screenshot.rs:95-98`）。

### 5.5 RX 监控

- 注册表 `MonitorMgr` `src/nic/monitor.rs:54-110`：start 保存累计字节和时间，stop 用差值计算平均 Mbps，ID 为 `monN`，sweep 清理过期条目。
- 独立连续监控选项和循环 `114-217`：Ctrl+C、间隔/时长、实时输出、可选 CSV。
- CSV 摘要重写 `219-241`：接口、间隔、时长、平均/峰值和采样明细。

## 6. 任务构建模型与不变量

### 6.1 中间模型

定义在 `src/master/builder.rs:10-120`：

- `PORT_BASE=56000` `10`；`Side` `12-25`。
- `Endpoint` `27-41` 保存 side、PC、`NicInfo`，`key` 用于禁止同一网口作为源/目标。
- `SpecNorm` `43-64` 是配置和交互的共同规范格式。
- `IperfTask` `66-77` 保存 v4/v6、TCP/UDP、profile label、源/目标、端口、时长、额外参数和流索引。
- `PingTask` `79-86` 保存 v4/v6、源/目标、计数和 payload。
- `LegKind` `88-96` 为单流 iperf、UDP 多流组或 ping；`Leg` `98-103` 带 `""/ab/ba` 标签；`Unit` `105-112` 是 RESUME 和执行的最小单元。
- IPv6 三元组 `V6Addrs` `114-120`；选择 link-local 优先、否则 global `122-137`。

### 6.2 配置解析与展开

- endpoint 角色/NAME 解析 `139-204`；配置 TestSpec -> SpecNorm `206-250`，streams clamp 到 1..32，iperf duration 1..86400，ping count 1..100000。
- UDP 发送口限流 `252-269`：WiFi、WIFI 角色、未知速率或非法带宽不裁剪；否则 `floor(speed/bandwidth)` 与请求流数取最小。
- 方向腿 `dir_pairs` `271-277`：ab 一腿、ba 一腿、bidir 按 `[ab,ba]` 两腿。
- 共享 materializer `map_legs` `280-291` 和 `unit` `293-301` 消除了 TCP/ping 重复初始化。
- Unit 生成主循环 `307-526`：先方向，再 IP 版本，再 iperf/ping；跨机 IPv4 iperf 可受同 /24 门禁 `339-347`，ping 不受门禁。

### 6.3 端口、流和稳定 ID

- 主流程在 `src/master/ui.rs:194` 将端口游标初始化为 `PORT_BASE=56000`；`alloc_port` `builder.rs:528-532` 返回当前端口并递增，达到 65535 后回绕到 56000，因此只在回绕前单调且不重复。
- TCP：每个 window 生成一个 Unit，腿内使用 `-w <window> -P <streams>`，端口按腿顺序分配 `350-371`。
- UDP：每个 profile 生成一个 Unit；每条腿根据发送口得到流数 `395-423`，多于 1 流时每流独立端口/进程 `429-459`；任一腿为 0 则整个 profile Unit 跳过。
- Ping：每个 payload 一个 Unit，构造 `493-519`。
- TCP ID 模板 `379-387`：`iperf_v1|V4/V6|tcp|profile|duration|src-id|dst-id|direction`。
- UDP ID 模板 `473-482`：另含 `streams`。
- Ping ID 模板 `510-518`：`ping_v1|count|payload|V4/V6|src-id|dst-id|direction`。
- 修改 ID 模板、字段顺序或 `v1` 会使旧 `task_results.json` 的 RESUME 命中失效；这是有意的兼容边界。

## 7. 执行器、判定、截图与 RESUME

### 7.1 Ctx 与远端统一调用

- `Ctx` `src/master/executor.rs:20-29` 持有 agent 地址、配置、输出目录、本地 server/monitor、线程安全 Row 和 ResultDb。
- `agent_post` `49-71` 统一序列化、POST、HTTP 状态、`Resp<T>` 解析、业务错误和缺 data。
- 双端 ping/iperf server/client/monitor 适配 `75-171`；截图低层获取 `180-225`，截图开关和文件保存 `228-263`。
- Agent 截图日志保留状态/长度、HTTP 前缀、JSON 前缀、业务错误、缺 data 和 Base64 长度；`byte_prefix` `724-730` 以 UTF-8 边界安全截断。主控截图失败仍静默跳过。

### 7.2 调度与双向

- `run_all` `273-349` 顺序遍历 Unit；多腿 Unit 用 scoped threads 并行。
- `resume=true` 时在执行前查询 `ResultDb::fresh_pass`；跳过行是 `ok=None`，计入 skip。
- 双向两腿完成后 `317-329` 互填 `peer_rx`，保留三位精度和对向 tag。
- Unit PASS 必须所有腿 `LegOutcome.ok=true`；空腿视为 FAIL；每个 Unit 结果写回数据库并暂停 1 秒。

### 7.3 Ping

- `run_ping_leg` `src/master/executor.rs:363-458` 选择 v4/v6 地址、调用本地或 agent ping、生成 Row 和 raw 输出。
- `src/ping.rs:10-40` 构造 Windows `ping` 或 macOS/BSD `ping/ping6`；执行 `42-58`。
- 解析 `ping.rs:60-181`：中英文 Windows、BSD/macOS；只认真实 RTT 的 reply，修正目标不可达/ICMP 错误被统计为 received 的假成功。

### 7.4 iperf 单流和 UDP 组

- server/client/stop 核心 `exec_iperf_core` `src/master/executor.rs:463-521`；IPv6 zone 处理 `760-766`。
- 单流 `run_iperf_single` `523-600`：接收端 monitor、client/server 输出解析、PASS、截图和 Row。
- UDP 并发组 `run_iperf_group` `604-721`：错峰 200ms 起流、共享接收端 monitor、每流 Row、组合计 Row、组合 PASS。
- `iperf_row` `732-757` 是单流/组内流/组合计的公共 Row 基底，字段必须与 Unit/Task 对齐。
- 单流 PASS：client 成功且未超时，并且 iperf 文本有正测量或接收网卡 RX > `MIN_VALID_RX_MBPS=0.01`；组内每流需文本测量，组合还需 RX > 阈值。

### 7.5 ResultDb / RESUME

- `DbEnt`、`ResultDb` 和 24 小时常量 `src/master/executor.rs:786-797`。
- 加载 JSON `800-806`；fresh PASS 判断 `809-822`：要求 `ok=true`、`age.num_hours() <= 24` 且未来偏差不超过 60 秒；由于整小时截断，过去记录实际可命中到不足 25 小时。
- `set` `824-833`；原子临时文件写入和 rename `836-844`。

## 8. iperf、命令与公共基础设施

### 8.1 iperf 适配

`src/cmd/iperf.rs`：

- 常量和 server/client 参数构造 `18-67`；TCP 用 `-P`，UDP 用 `-u` 加额外 `-b/-l`。
- `IperfParsed` 和最佳 sender/receiver/measurement 判定 `72-92`；文本速率、Bytes/bits、单位换算和 UDP 丢包解析 `95-134`。
- `IperfServerMgr` `148-262`：同端口先停旧 server，后台收集 stdout，TCP connect 探测 ready，主动 stop/kill，sweep/stop_all。
- 就绪探测 `265-293`：去掉 IPv6 zone，解析地址一次，200ms 轮询，超时 15 秒。
- client 瞬态错误和重试 `297-344`：最多 3 次，单次总超时为 duration+120 秒，保留实时输出和 stderr。

### 8.2 公共 util

- 编码和命令结果 `src/util.rs:12-38`；GBK fallback 适配中文 Windows。
- piped 子进程 helper `40-59`；阻塞执行/超时 kill `61-86`；实时流式执行、无尾换行回调和 stderr `88-159`。
- 日志 `163-180`；时间 `182-192`；文件名安全化 `194-206`；主机/OS `208-236`。
- iperf3 定位/版本 `238-273`：程序同目录优先，再查 PATH。
- 交互输入 `277-285`；打开报告 `289-299`；MD5 `301-303`；临时文件 `305-309`。
- 选择解析 `311-345`：空输入全选、逗号/范围、去重保序和边界错误；同 /24 `347-352`。

## 9. 报告与截图

### 9.1 Row 和 HTML

- `Row` 字段定义在 `src/report.rs:5-40`：排序键、任务/父 ID、源/目标、状态、接收/对向/发送/接收速率、UDP/ping 指标、截图、命令、raw、组合计标记。
- `ReportMeta` `42-50`：主控、辅测、agent host、开始/结束/耗时。
- HTML 辅助函数 `52-77`：截图链接、HTML 转义、数值格式、 PASS/FAIL/SKIP 映射。
- `write_report` `79-238`：按 sort_key 排序；组合计不进入总数；输出元数据、统计、25 列表格和原始输出 details；字段均转义。
- Task/Parent ID 在表格显示前用 `short8` 截 8 个 Unicode 字符：`240-242`。

### 9.2 截图实现

- macOS `src/screenshot.rs:4-28` 调用系统 `screencapture` 临时 PNG。
- Windows GDI 主屏抓取 `30-93`：GetDC/CreateCompatibleBitmap/BitBlt/GetDIBits，BGRA 转 RGBA，再编码 PNG。
- 其他平台固定错误 `95-98`。
- PNG 编码 `100-115`；测试在 `117-131` 解码回读 2x2 RGBA，而不只检查魔数。

## 10. 测试覆盖索引（源码声明 52 项；macOS 52、Linux 51、Windows 50）

| 区域 | 测试位置 | 覆盖 |
|---|---|---|
| 配置 | `config.rs:280-338` | 默认值抽样、代表性 JSON 反序列化及部分缺省字段、bidir/方向数组/both 展开、UDP profile 带宽换算/名称/标签 |
| CLI | `main.rs:292-309` | 长短 flags、值/开关规则、CSV 拆分 |
| builder | `builder.rs:534-692` | TCP/UDP 稳定 ID、端口、双向组、UDP 限流/WiFi 豁免、同 /24、ping、IPv6 |
| executor | `executor.rs:846-869` | ResultDb 保存/加载、刚写入 PASS 命中、未知 ID、失败覆盖 |
| UI | `ui.rs:694-734` | 跨机优先、同机配对顺序、UNKNOWN 过滤 |
| iperf | `cmd/iperf.rs:346-437` | TCP/UDP 结果解析、Gbits/sec 与 MBytes/sec 换算、无测量数据的错误输出、瞬态错误判定、client/server 参数构造 |
| ping | `ping.rs:183-325` | 中文/英文/BSD、全丢、不可达假成功、部分成功 |
| Windows parser | `cmd/ipconfig.rs:121-199`、`cmd/netsh.rs:81-154` | 中英文适配器、WiFi 状态/频段 |
| NIC 分类 | `nic/classify.rs:79-137` | 角色、排序、WiFi 名称；USB 4000/4001/8999/12000 与以太网 8999/9000/12001 分类样例 |
| macOS NIC/监控 | `scan_macos.rs:205-233`、`monitor.rs:243-260` | ifconfig、netstat Ibytes |
| HTTP/agent | `http_client.rs:146-188`、`agent/server.rs:230-244` | Content-Length/状态行解析、chunked 正常及非法大小、tiny_http POST 回环、空 body 默认请求、非法 JSON 错误单次包装 |
| util | `util.rs:354-423` | 选择解析、同 /24、sanitize、run_cmd 成功/启动错误、非 Windows streaming 无尾换行 stdout 回调/收集及 stderr |
| report/screenshot | `report.rs:244-283`、`screenshot.rs:117-131` | PASS/SKIP、转义、排序/组合计、截图链接；PNG 实际解码 |

## 11. 不可破坏的不变量与修改入口

### 11.1 必须保持

- 主流程从 56000 开始递增分配端口，达到 65535 后回绕到 56000；TCP 使用一个 client 的 `-P`，UDP 多流使用独立进程/端口；bidir 始终是 `[ab,ba]` 两腿。
- 稳定 ID 模板和字段顺序见 `builder.rs:379-387,473-482,510-518`；其输入构造还包括 `ep_id` `303-305`、TCP profile 名 `352-353`，以及 `config.rs:110-115`/`builder.rs:392-393` 的 UDP profile 名。改变模板、字段顺序或任一输入规范会让历史 RESUME 不再命中。
- IPv4 同 /24 门禁只限制跨机 iperf；ping 不受限。IPv6 优先双端 link-local，其次 global；macOS 执行时加 zone，Windows 不加。
- UDP 限流按每条腿的发送 NIC；WiFi/未知速率不裁剪；任一腿不能承载 profile 就跳过整个 Unit。
- PASS 规则：ping 见 `ping.rs:153-170` 与 `executor.rs:399-457`；iperf core、单流、组内流、组汇总和 Unit 分别见 `executor.rs:519-520,546-548,634-635,677-681,331-345`；组合计行在 `executor.rs:695-714` 标记，并由 `report.rs:81-89` 排除在报告总数外。
- RESUME 是 Unit 级；当前按 `age.num_hours() <= 24` 判断，过去记录实际可命中到不足 25 小时，并容忍未来时间 60 秒（`executor.rs:797-822`）。agent 固定 16 worker；每 30 秒 sweep，server/monitor 最大存活分别为 10/30 分钟（`agent/server.rs:22-27,89-108`）。
- 对外 JSON 字段即使当前生产代码没有本地消费者，也属于协议兼容面；删除/重命名要同步所有端点和版本策略。

### 11.2 常见修改入口

| 需求 | 首先改 | 必须联检 |
|---|---|---|
| CLI/新参数 | `main.rs:38-279`；master 参数另看 `master/ui.rs:21-54` | `main.rs:201-233` 的帮助、`292-309` 的解析测试、README |
| 配置字段/默认 | `config.rs:6-278` | `config.rs:280-338`、`config.example.json`、README，以及 `main.rs`、`master/ui.rs`、`master/builder.rs`、`master/executor.rs`、`agent/server.rs` 中对应消费者 |
| HTTP DTO/端点 | `protocol.rs:5-219` 或 `agent/server.rs:111-228` | `http_client.rs:11-188`、`master/ui.rs:301-326`、`master/executor.rs:49-223`、对应实现及 `agent/server.rs:230-244` 的解析/错误包装测试 |
| 任务数量/顺序/ID/端口 | `builder.rs:10,122-532` | `builder.rs:534-692`、`master/ui.rs:193-218`、`executor.rs:785-844`、executor 的 `sort_key` 构造与 `report.rs:79-80` |
| PASS/并发/监控/截图/RESUME | `executor.rs:20-844` | `executor.rs:846-869`、builder Unit/legs、`cmd/iperf.rs`、`ping.rs`、`nic/monitor.rs`、`screenshot.rs`、agent 对应端点及 `report.rs` |
| iperf 命令/解析/进程 | `cmd/iperf.rs:18-344` | `cmd/iperf.rs:346-437`、`protocol.rs:120-173`、`agent/server.rs:168-199`、`master/executor.rs:84-146,463-520`、`util::run_streaming` |
| ping 命令/解析 | `ping.rs:8-181` | `ping.rs:183-325`、`protocol.rs:92-118`、`agent/server.rs:160-167`、`executor.rs:363-458` |
| NIC 角色 | `nic/classify.rs:3-77` | `nic/classify.rs:79-137`、`nic/mod.rs:17-38`、Windows/macOS 扫描、`builder.rs:140-204,253-269` 及 UI 角色选择 |
| 平台采集/监控 | `nic/mod.rs:16-76`、`nic/scan_windows.rs:15-136`、`nic/scan_macos.rs:14-203`、`nic/monitor.rs:14-241`、`cmd/ipconfig.rs:9-119`、`cmd/netsh.rs:9-79` | Windows GNU/MSVC；`ipconfig.rs:121-199`、`netsh.rs:81-154`、`scan_macos.rs:205-233`、`monitor.rs:243-260`，以及 main/agent/executor 调用方 |
| 报告列/HTML | `report.rs:5-242` | `executor.rs:283-292,426-451,568-593,641-714,732-757` 的全部 Row 构造；`report.rs:244-283` 的转义、排序/组合计、PASS/SKIP、截图链接测试（当前无 FAIL/golden） |

## 12. 变更与验证记录

- 本次重构前 Rust 物理行数：`6,430`；生产区（每文件首个 `#[cfg(test)]` 前）`5,598`。
- 最终 Rust 物理行数：`6,402`；生产物理行数：`5,424`；生产区净减少 `174` 行，全部 Rust 净减少 `28` 行。
- 测试从基线 `46` 增加到 `52`，没有通过删除测试获得减量。
- 已验证：`cargo fmt --all -- --check`、`cargo test --all-targets --locked`（52/52）、本机严格 Clippy、Windows GNU/MSVC 严格 Clippy、`git diff --check`。
- 旧说明 `使用说明.md:276-277` 关于 iperf server `-1`/netstat LISTEN 探测已过时；当前实现是无 `-1`、主动 stop、TCP connect ready 探测（`cmd/iperf.rs:148-293`）。维护 AI 文档时以当前实现为准。
