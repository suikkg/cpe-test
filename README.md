# CPE 子网测试工具

> 两台电脑间自动化 ping + iperf3 / Microsoft ctsTraffic 灌包测试，零 Python/零 PowerShell

## v4.2.0

- 新增 Windows 10+ 专用 Microsoft ctsTraffic 后端，支持 TCP/UDP、双向测试、多连接和 socket buffer 参数。
- iperf3 UDP profile 新增可选 `window`，会生成 `-w <size>`；旧配置不写该字段时行为不变。
- UDP 单流硬连通门槛统一覆盖 iperf3 与 ctsTraffic：单向 1 流或双向每方向 1 流都会按方向独立执行至少 3 次完整尝试，安全耗尽后直接记录硬失败，不再降级为“活跃流不足/未评估”。
- ctsTraffic 与 iperf3 分别做平台、能力和二进制前置检查；一个后端不可用不会阻断另一个后端或 Ping，也不会静默回退。
- Windows Release 包固定捆绑并校验 ctsTraffic 2.0.4.0；三平台产物使用互不冲突的归档名和 SHA-256 清单。

[![Rust](https://img.shields.io/badge/Rust-1.82%2B-orange)](https://rustup.rs)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-lightgrey)
[![CI](https://github.com/suikkg/cpe-test/actions/workflows/build.yml/badge.svg)](https://github.com/suikkg/cpe-test/actions/workflows/build.yml)

---

## 目录

- [概览](#概览)
- [快速开始](#快速开始)
- [命令行用法](#命令行用法)
- [网卡速率监控](#网卡速率监控)
- [配置文件](#配置文件)
- [模块架构](#模块架构)
- [角色分类体系](#角色分类体系)
- [截图与报告](#截图与报告)
- [RESUME 断点续跑](#resume-断点续跑)
- [跨平台策略](#跨平台策略)
- [编译与部署](#编译与部署)
- [常见问题](#常见问题)

---

## 概览

CPE（Customer Premises Equipment）子网测试工具用于在**两台电脑之间**自动化执行网络连通性和吞吐量测试，生成结构化的 HTML 报告。

**核心特性：**

- **Zero PowerShell** — 网卡扫描走 ipconfig + GetIfTable2 API + netsh wlan，不依赖 PowerShell 或 wmic
- **Zero 线程安全隐患** — 无 COM 多线程问题；agent 是固定线程池 + Arc\<Mutex\>，panic 不崩服务
- **单二进制分发** — 一个 exe 文件完成主控/辅测/监控三模式，不需要 pip install
- **REST + JSON** — 主控 ↔ 辅测走标准 HTTP，带超时/重试/错误码
- **双吞吐后端** — iperf3 跨平台通用测试；ctsTraffic 提供 Windows 10+ 专用 TCP/UDP 流量模型
- **20/32 流真实并发** — agent 的 HTTP worker 只创建/查询后台吞吐作业，不再被长时间 client 占住
- **UDP 连续采样** — 从起流前持续记录双端 RX/TX、流连接/重试/结束事件，结束后重建共同有效窗口
- **TCP/UDP 实时网卡速率** — 运行日志按秒打印 OS 网卡计数器的 `nic-rx`；工具输出用于确认起流和诊断，不替代 NIC 正式吞吐口径
- **原始记录独立落盘** — 每条流保存 client/server/事件和所有重试原文，并保存 NIC 逐样本 CSV
- **已知/未知目标分离** — EVB 已知目标正式验收；CPE、SGMII、RNDIS、WiFi 未知能力默认只测量，不伪造 PASS
- **独立网卡监控** — 不依赖子网测试流程，单独对某网口做逐秒速率采样，输出 CSV
- **按后端跨平台** — ping/iperf3 可在 Windows、macOS 等平台运行；ctsTraffic 仅支持 Windows 10+

---

## 快速开始

### 第 1 步：准备文件

把以下文件和目录放到两台电脑的同一目录：

```
cpe_test.exe          ← 本工具（单文件）
iperf3.exe            ← 从 iperf.fr 下载（只测 Ping/ctsTraffic 可不放）
ctsTraffic.exe        ← v4.2.0 Windows Release 已捆绑（仅 Windows 10+）
start_agent.bat       ← 辅测机双击
start_master.bat      ← 主控机双击
start_master_select_config.bat ← 主控机选择网口配置后双击
configs\             ← SGMII / Wi-Fi / 10GUSB 等具名配置
```

主控机和辅测机必须使用同一个 `cpe_test` 版本。选择 ctsTraffic 时，两台电脑都必须是
Windows 10 或更高版本，且都能在 `cpe_test.exe` 同目录或 `PATH` 中找到 `ctsTraffic.exe`。

### 第 2 步：辅测机启动 agent

双击 `start_agent.bat`，窗口里会显示本机 IP：

```
本机 IP 列表（把主控能 ping 通的那个告诉主控机）:
    以太网 = 192.168.8.101
    WLAN  = 10.228.46.50
agent 已启动，监听 0.0.0.0:28801
```

- **记录 IP**（选主控能连上的那个，一般是管理口）
- 防火墙提示时**全部放行**
- **不关窗口**

### 第 3 步：主控机测试

双击 `start_master_select_config.bat`，选择本次网口配置后输入辅测机 IP。也可以双击
`start_master.bat`（默认 SGMII 配置），或以 `start_master.bat configs\\config-wifi5g.json`
指定配置：

```
请输入辅测机 IP: 192.168.8.101
```

程序自动扫描双端网卡 → 生成任务 → 执行 → 弹出 HTML 报告。

---

## 命令行用法

```
cpe_test                    交互选择模式（双击运行就是这个）
cpe_test agent              辅测机启动常驻服务
    --port N                指定监听端口（默认 28801）

cpe_test master             主控发起测试
    --agent-host IP         辅测机 IP
    --agent-port N          辅测机端口（默认 28801）
    --config FILE           指定配置文件（默认找 ./config.json）
    --auto                  免交互：按配置文件 tests 全部执行
    --resume                24 小时内已 PASS 的任务跳过
    --no-open               结束后不自动打开报告
    --screenshot            每个吞吐任务后截图
    --prefix A.,B.          临时指定 IPv4 前缀过滤

cpe_test scan               查看本机网卡识别结果
    --prefix A.,B.

cpe_test monitor            独立网卡速率监控 (按 Ctrl+C 停止)
    --iface NAME / -n NAME  网卡名称 (不指定则用上次选择或交互选)
    --interval N / -i N     采样间隔秒数 (默认 1)
    --duration N / -d N     监控时长秒数 (0=不限，默认 0)
    --csv FILE / -c FILE    输出 CSV 文件路径 (可选)
```

---

## 网卡速率监控

`cpe_test monitor` 是独立于子网测试的网卡速率采样工具，适合**单台电脑**单独对某个网口做实时速率观测。

### 核心特性

- **DU Meter 同源精度** — 走 `GetIfTable2.InOctets` (Windows) / `netstat -ibn` (macOS)，64位累计字节差值法，不丢包
- **逐秒采样** — 可配采样间隔（1/2/5/10s），实时打印当前 Mbps
- **自动 CSV** — 测试期间实时追加写入，Ctrl+C 结束时自动在文件顶部注入平均值/峰值等统计摘要
- **记住上次网卡** — 第一次选完网卡后保存到 `.cpe_monitor_iface`，下次直接回车即可

### 用法示例

```bash
# 交互模式：列出网卡，选择后开始监控
cpe_test monitor

# 直接指定网卡，每秒采样，输出到 CSV
cpe_test monitor -n "以太网" -c speed_log.csv

# 每 5 秒采样一次，跑 120 秒自动停止
cpe_test monitor -n "以太网" -i 5 -d 120 -c r.csv
```

### 运行时输出

```
网卡: [以太网]  间隔: 1s  按 Ctrl+C 停止

时间          速率(Mbps)
--------------------------
12:00:01        1690.10
12:00:02        1688.50
^C
==================================================
网卡: 以太网
时长: 2s (2 次采样)
平均: 1689.30 Mbps
峰值: 1690.10 Mbps
最低: 1688.50 Mbps
CSV : speed_log.csv
```

### CSV 文件格式

文件顶部自动写入统计摘要（`#` 号行可被 pandas/Excel 自动跳过）：

```csv
# === CPE NIC Monitor Report ===
# Interface,以太网
# Interval,1s
# Duration,120s
# Average (Mbps),1685.30
# Peak (Mbps),1750.45
# ================================
Time,Speed(Mbps)
12:00:01,1690.10
12:00:02,1688.50
```

---

## 配置文件

配置文件 `config.json` 放到 exe 同目录，所有测试参数通过 JSON 控制，**不需要改代码**。

JSON 标准没有注释，不能可靠地在同一个配置内用 `//`、`#` 或 `/* ... */` 来切换网口。
发布包的 `configs` 目录提供 `config-sgmii.json`、`config-wifi5g.json`、
`config-10gusb.json` 和 `config-all-common.json`；用 `--config` 或
`start_master_select_config.bat` 选择对应文件即可。需要自定义时复制最接近的一份再改，
不要往 JSON 添加注释。

完整字段：

```json
{
  "agent_host": "192.168.8.101",
  "agent_port": 28801,
  "ipv4_prefixes": ["192.168."],
  "require_same_subnet_for_iperf": true,
  "limit_udp_by_link_speed": true,
  "screenshot": false,
  "resume": false,
  "open_report": true,

  "pairs": "all",
  "universal_params": {
    "directions": ["A->B", "bidir"],
    "kinds": ["iperf", "ctstraffic", "ping"],
    "transports": ["tcp", "udp"],
    "ip": ["v4"],
    "streams": 5,
    "iperf_duration": 180,
    "rate_mode": "auto",
    "rate_targets_mbps": {"forward": null, "ab": null, "ba": null}
  },
  "iperf": {
    "duration": 180,
    "tcp_windows": ["64k", "1m", "4m"],
    "udp_profiles": [
      { "bandwidth": "1m" },
      { "bandwidth": "500m" },
      { "bandwidth": "1000m", "length": "64", "window": "1m" },
      { "bandwidth": "2500m" }
    ]
  },

  "ctstraffic": {
    "udp_frame_rate": 100,
    "udp_buffer_depth_secs": 1,
    "status_update_ms": 1000
  },

  "ping": {
    "count": 100,
    "payload_sizes": [32, 1600, 65500]
  },

  "tests": [
    {
      "name": "2.5G口灌包",
      "src": "master:SGMII2.5G",
      "dst": "agent:SGMII2.5G",
      "direction": ["A->B", "bidir"],
      "kinds": ["ctstraffic", "ping"],
      "transports": ["tcp", "udp"],
      "ip": ["v4"],
      "streams": 5,
      "iperf_duration": 180,
      "rate_mode": "observe"
    }
  ]
}
```

`require_same_subnet_for_iperf` 是为兼容旧配置保留的字段名，当前会同时约束跨机
iperf3 和 ctsTraffic 的 IPv4 直连任务。发布包预置配置默认仍使用
`["iperf", "ping"]`；要启用 CTS，将 `kinds` 改成 `["ctstraffic"]` 或上述组合即可。

### IP 自适应

配置写 `"master:SGMII2.5G"` 这种**角色引用**，运行时自动解析成当前机器的实际 IP。
换电脑不用改配置：角色识别对了，IP 自动跟着变。

兜底方案：`"master:NAME=以太网 2"` 按接口名精确匹配。

### tests[] 字段说明

| 字段 | 类型 | 说明 | 默认 |
|------|------|------|------|
| `name` | string | 测试名称 | — |
| `src` / `dst` | string | `"side:ROLE"` 或 `"side:NAME=接口名"` | — |
| `direction` | string/array | `"A->B" / "B->A" / "bidir" / "both"` | A->B |
| `kinds` | array | `iperf`、`ctstraffic`、`ping` 可任选或组合；`cts` 是 ctsTraffic 别名 | ["iperf"] |
| `transports` | array | `["tcp"] / ["udp"] / ["tcp","udp"]` | ["tcp"] |
| `ip` | array | `["v4"] / ["v6"]` | ["v4"] |
| `streams` | int | 并发流数；iperf3 TCP=`-P`、UDP=独立进程，ctsTraffic=`Connections` | 1 |
| `iperf_duration` | int | 历史字段名，现供两种吞吐后端共用；映射规则见下文，必须大于 0 | — |
| `rate_mode` | string | `auto` / `verify` / `observe` / `discover` | 全局值 |
| `rate_targets_mbps` | object | `forward` 或双向 `ab`/`ba` 的明确验收目标；未知时保持 null | — |
| `ping_count` | int | 覆盖全局 ping 包数 | — |
| `ping_payload_sizes` | array | 覆盖全局负载字节；默认覆盖 32、1600、65500 | — |
| `tcp_windows` | array | TCP socket buffer 档位；ctsTraffic 映射为方向正确的 Send/Recv buffer | — |
| `udp_profiles` | array | UDP profile；支持 `bandwidth`、可选 `length` 和可选 `window` | — |

### pairs 自动配对（比 tests[] 更省事的写法）

如果你的测试场景是"主控上 N 个网口 × 辅测上 M 个网口，全部互相测一遍"，
不用逐个写 tests[]，用 `pairs` 一行搞定：

```json
{
  "pairs": "all",
  "universal_params": {
    "directions": ["A->B", "bidir"],
    "kinds": ["iperf", "ping"],
    "transports": ["tcp", "udp"],
    "ip": ["v4"],
    "streams": 1,
    "iperf_duration": 180,
    "rate_mode": "auto"
  }
}
```

`pairs` 支持两种值：

| 值 | 含义 |
|---|---|
| `"all"` | 自动枚举主控 × 辅测所有网口两两组合（同 UNKNOWN 跳过） |
| `[{...}, ...]` | 手动列出角色对，每项 `{"master":"SGMII2.5G", "agent":"SGMII2.5G"}` |

`universal_params` 是统一应用给所有配对的测试参数，字段与 `tests[]` 中的对应字段完全一致，不写则用全局默认值（`iperf.duration` / `ping.count` 等）。

**优先级**：`tests[]` 非空时优先使用 `tests[]`。只有 `tests[]` 为空且 `pairs` 有值时，才走自动配对。

### ctsTraffic 后端（仅 Windows 10+）

在 `tests[].kinds` 或 `universal_params.kinds` 中使用 `"ctstraffic"`：

```json
"kinds": ["ctstraffic"]
```

也可以同一场景同时跑两种后端和 Ping，便于保留各自原始结果：

```json
"kinds": ["iperf", "ctstraffic", "ping"]
```

ctsTraffic 是 Windows 专用高级吞吐/可靠性工具，不是 iperf3 的替代实现。使用它时请注意：

- 主控和 agent 必须都是 Windows 10 或更高版本，并使用同一个 `cpe_test` 版本；两端都要能找到 `ctsTraffic.exe`。
- 程序会检查 `ctstraffic_v1` 能力、操作系统和两端二进制。任何一项不满足时，仅对应 CTS 单元记录 `SETUP_ERROR / CTSTRAFFIC_PREFLIGHT_FAILED`；不会静默改跑 iperf3，已选的 iperf3/Ping 仍继续。
- 配置中的 `src → dst` 始终表示数据方向。TCP Push 在 `src` 启动 ctsTraffic client 发送、在 `dst` 启动 server 接收；UDP MediaStream 则必须在 `src` 启动 server 发送、在 `dst` 启动 client 接收。程序已处理这层角色反转，报告仍按 `src → dst` 展示。
- `streams` 映射为 client 的 `Connections:N`；一个 CTS 进程承载 N 条连接，不会像 iperf3 UDP 那样展开为 N 个进程。
- 历史字段 `iperf_duration` 供两种吞吐后端共用。CTS TCP 映射为 client 的 `TimeLimit:<毫秒>`，这是硬截止；CTS UDP 映射为双方的 `StreamLength:<秒>`。自动化测试必须使用大于 0 的时长；若要使用 ctsTraffic 原生无限连接语义，请脱离本工具手工运行并自行停止。
- `tcp_windows` 对 CTS 表示 Winsock socket buffer：TCP 发送端用 `SendBufValue`、接收端用 `RecvBufValue`。`udp_profiles[].window` 对 iperf3 生成 `-w`，对 CTS UDP 则在 server 发送端使用 `SendBufValue`、client 接收端使用 `RecvBufValue`。这些都是 socket buffer，不是 TCP 拥塞窗口。
- `udp_profiles[].bandwidth` 对 CTS 是**每条连接**的 `BitsPerSecond`，总提供速率约为“每流带宽 × streams”；`length` 映射为 `DatagramByteSize`。默认 `FrameRate=100`、`BufferDepth=1`，属于 ctsTraffic MediaStream 模型，不等同于 iperf3 UDP flood。
- CTS UDP 的 `Connections:1` 与 iperf3 UDP 单流一样是每方向独立的硬连通门槛。总尝试数为 `max(flow_retries + 1, 3)`；双向 AB、BA 各自拥有完整预算并并行执行。每轮都必须完整启动 server/client，并在确认两端停止和资源清理完成后才可复用端口。
- 是否真正灌通只认工具自身的 rate、bytes、successful frames/datagrams 等证据；NIC RX 即使有背景流量也不能证明 CTS 连接已经建立。NIC 仅在工具已证明起流后用于验证正式目标吞吐、采样覆盖和稳定性。
- 单连接在全部尝试均安全完成后仍没有 CTS 自身测量时，记录 `RATE_FAIL / CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED`。平台、工具、参数、server/client 生命周期或清理异常仍记录 `SETUP_ERROR`；一旦某轮已有工具测量，就按该轮真实运行错误、UDP 丢帧和目标速率判定，不用后续重试掩盖真实性能结果。
- CTS 不套用 iperf3 的“每个连接展开成独立进程”、settle、5 秒滚动 P10 和分阶段 `discover` 实现；CTS 使用 `discover` 时按固定 `Connections` 的能力测量记录为 `MEASURED`。两者只共享上述 UDP 单流硬连通与安全生命周期原则。
- iperf3 与 ctsTraffic 的连接模型、截止方式和 UDP 帧模型不同。报告可以并列保留两者，但不应把数值当成同一实现语义下的直接替换结果。

CTS 专用默认项：

| 字段 | 默认 | 含义 |
|------|------|------|
| `ctstraffic.udp_frame_rate` | 100 | UDP MediaStream 每秒帧数；每帧可再拆为 datagram |
| `ctstraffic.udp_buffer_depth_secs` | 1 | UDP client 应用层缓冲深度（秒），不是 socket buffer |
| `ctstraffic.status_update_ms` | 1000 | ctsTraffic 聚合状态输出周期（毫秒） |

`window`/`tcp_windows` 支持字节数以及 `k`、`m`、`g` 后缀，例如 `"64k"`、`"1m"`、
`"1.5m"`。UDP profile 省略 `window` 时维持旧行为，不会额外传入 socket buffer 参数。

### UDP 单流硬门槛与 iperf3 并发速率判定

UDP 单流、并发组和双向测试现在走同一套调度器：先把所有方向的 server 全部准备好，再按方向交错启动 client。网卡监控从起流前开始连续采样，直到最后一条流结束；最终根据流事件重建“哪些流在同一时刻真正有流量”的时间线。

`iperf.duration`（默认 180 秒）表示报告中用于判定的**有效稳态窗口**，不是 `iperf3 -t` 的固定进程墙钟时长。程序会自动加入背景采样、错峰起流、连接超时、稳定等待和失败流重试缓冲，所以一次 180 秒多流测试通常会运行约 200 秒；单流若需要完整执行多轮，墙钟时间还会按实际尝试数增加。报告会分别列出“有效秒数/要求秒数”。

失败流不会造成无限等待：

- iperf3 的瞬态连接错误先按原机制重试，组调度器还会重启对应 server/client。单向 1 流或双向每方向 1 流属于硬连通门槛，每个方向的总尝试数为 `max(flow_retries + 1, 3)`；双向 AB、BA 各自独立且并行。全部尝试安全完成后仍没有 iperf3 自身 rate/bytes/datagrams 证据时，记录 `RATE_FAIL / SINGLE_UDP_STREAM_FAILED`，不会降级为 `NOT_EVALUATED / ACTIVE_STREAMS_LOW`。
- 每次业务尝试都完整启动 server/client，结束后同步停止并等待子进程退出、输出线程回收；只有精确 `request_id` 的 server stop 和本轮清理均已确认，才会在同一端口启动下一轮。平台、工具、非法参数、server/client 启动或状态查询失败、显式取消以及停止未确认都属于 `SETUP_ERROR`，不会伪装成性能失败。
- 是否灌通只认 iperf3 自身 rate、bytes 或 datagrams 等输出证据，背景 NIC 流量不能把失败尝试变成成功。一旦某轮已有工具测量，就按该轮真实运行错误、UDP 丢包和目标速率继续判定，不靠额外重试掩盖真实结果。
- HTTP 传输重试复用同一个 `request_id`，因此 start 响应丢失不会重复创建进程；测试单元结束、报错或 panic 时还会按唯一 `owner_id` 批量清理 server/client/monitor。动态 lease 是断联后的最后兜底，不会用固定短 TTL 误杀合法长测。
- 2 条流的方向最低要求仍是 2 条。若最终只有 1 条成功，结果是 `NOT_EVALUATED / ACTIVE_STREAMS_LOW`，表示负载搭建不足，不会用单流速率误判 CPE 为 `RATE_FAIL`。
- 默认 `min_active_ratio=0.9`：5 条流要求至少 4 条，20 条要求至少 18 条；已知目标还会叠加 `ceil(目标 × 1.05 / 单流带宽)` 的 offered-load 要求。
- 运行中不阻塞等待并提前锁死窗口；结束后统一取满足活跃流条件的最长连续区间，迟到或提前结束只会如实改变该区间边界。
- 双向结果分别判断 AB/BA 目标，但使用“两边都达到各自最低活跃流数”的共同窗口，避免一边已满载、另一边仍在爬升时提前计分。

#### 网卡实测速率与 iperf 时间区间

报告里的“接收网卡平均/P10/中位/P95/最低/最高”和用于目标吞吐 PASS/FAIL 的 RX/TX 数据始终来自操作系统网卡累计字节计数器（Windows `GetIfTable2`、macOS `netstat -ibn`、Linux sysfs），不是 iperf3 自报速率。工具输出仍有一个不可替代的用途：必须由 iperf3/CTS 自身的 rate、bytes、frame/datagram 证据确认流确实建立；NIC 只验证已建立流的正式目标，不能用背景流量证明灌通。

TCP 和 UDP 运行期间现在使用同一套日志口径，例如：

```text
[灌包进度][TCP][ab] active=1/1 connected=1 ended=0 nic-rx=2368.4Mbps iperf=2379.0Mbps err=0
[灌包进度][UDP][ba] active=20/20 connected=20 ended=0 nic-rx=8420.3Mbps iperf=10000.0Mbps err=0
```

`nic-rx` 是接收端操作系统网卡累计字节差值得到的当期实际速率；`iperf` 只是 iperf3 interval 行的诊断值。旧版 iperf3 若块缓冲 stdout，运行中 `iperf` 可能暂时显示 `-`，但 `nic-rx` 仍会按网卡采样周期更新。实时 `nic-rx` 是未扣背景流量的网卡总速率，正式报告仍按共同有效窗口、背景基线和覆盖率规则计算。

旧版 iperf3 的 stdout 接到 pipe 后可能块缓冲，所有 interval 行会在结束时一瞬间刷出。如果直接用“第一行日志到最后一行日志”的墙钟差，180 秒测试会被误算成几毫秒。当前实现优先解析 iperf 行内的 `a-b sec`，按 `b-a` 反推真实活跃区间；解析不到时才回退到 Traffic/Connected/Started 事件。这个区间只负责裁剪网卡样本，不会把 iperf3 内部速率替换成正式网卡速率。

网卡平均值按每个样本与有效窗口的实际重叠毫秒数加权，覆盖率按有效时间并集计算。P10 来自完整覆盖的 5 秒滚动窗口；不足 5 秒不会用瞬时样本冒充 P10。计数器读取失败后跨多个周期的恢复样本可以补回总平均值，但不能伪造 5 秒稳定窗口。有明确目标时，RX/TX 总采样覆盖率和滚动窗口覆盖率都必须至少 95%，否则返回 `NOT_EVALUATED / SAMPLE_COVERAGE_LOW` 或 `RATE_WINDOW_COVERAGE_LOW`。

#### Ping 子网通信与自动故障诊断

常规 Ping 是**子网通信测试**，默认对每个所选方向分别覆盖 32、1600、65500 字节负载。`PASS` 的含义是“至少收到一个来自目标的 Echo Reply，子网可通信”；完整丢包率仍会写入日志和报告，但这里不是额外的 Ping 质量门槛。

Ping 解析只承认带 RTT 时间的真实 Echo Reply。Windows 的“目标不可达/一般故障”、macOS 的 ICMP Redirect 等额外 ICMP 行不会被算成目标回复，也不会再把与 Redirect 同时出现的真实回复误减掉。命令无法启动、超时、远端 HTTP/JSON 失败、源地址绑定失败等执行问题会报告为 `SETUP_ERROR / PING_EXEC_ERROR` 或 `PING_TIMEOUT`；只有命令正常完成但 0 个 Echo Reply 才报告为 `RATE_FAIL / PING_UNREACHABLE`。

如果本轮选择了吞吐测试，但所有 iperf3/ctsTraffic 单元都没有产生任何有效速率测量（包括缺少二进制、旧 agent 缺能力、所有 server 创建失败或所有 client 都未起流），主控会自动追加短时故障诊断：

- 按失败任务的每个唯一 IPv4 方向固定补跑 32 字节短 Ping，每个方向 3 包；自动诊断不再发送 1600/65500，避免故障时增加等待和无关变量。
- 对失败任务涉及的每块网卡，绑定该网卡 IPv4 源地址 Ping 该接口自己的 IPv4 默认网关，用于区分网卡/载体异常和吞吐后端搭建异常。
- 网卡没有 IPv4 默认网关时报告 `NOT_EVALUATED / GATEWAY_NOT_FOUND`，不会伪装成 100% 网络丢包。
- 已经选中的同方向 32 字节常规 Ping 不会重复执行；网关诊断仍会追加。

Windows 网关来自 `GetIpForwardTable2` 默认路由表并按接口索引关联；macOS 使用带 `-ifscope` 的默认路由查询。网卡扫描输出会显示 `gw:`，便于核对诊断目标。当前自动诊断不抓 PCAP；抓包留给后续 RF 自动化按异常条件启用，避免正常吞吐测试受到额外采集负担。

#### 主控/agent 版本与后端门禁

可靠 iperf3 生命周期由 agent `/health` 的 `reliable_lifecycle_v1` 能力声明，ctsTraffic 生命周期由 `ctstraffic_v1` 声明；运行中网卡快照由 `live_nic_progress_v1` 声明。主控在启动 server/client 前分别检查所选后端：iperf3 检查生命周期能力和两端 iperf3；ctsTraffic 检查两端是否为 Windows、CTS 能力和两端 ctsTraffic，实际系统仍必须满足上游的 Windows 10+ 要求。建议始终把同一个 Release 包完整复制到两台电脑。

检查失败只会阻断对应后端，并分别记录 `IPERF_PREFLIGHT_FAILED` 或 `CTSTRAFFIC_PREFLIGHT_FAILED`；不会静默回退，也不会阻断已选的另一吞吐后端或 Ping。只选 Ping 时仍兼容旧 agent，也不要求安装任何吞吐工具。

#### 速率模式

| 模式 | 适用场景 | 报告行为 |
|------|----------|----------|
| `auto` | 推荐默认 | 有显式目标或明确 EVB 10GUSB↔10GETH 路径时转 `verify`，否则转 `observe` |
| `verify` | 已知验收线 | 检查 TX offered load、RX 平均/P10、有效窗口和可选 UDP 丢包率；未提供显式或 EVB 自动目标时返回 `NOT_EVALUATED / TARGET_MISSING` |
| `observe` | CPE 理论/实际值未知 | 完整测量并输出速率统计，结果为 `MEASURED`，不伪造 PASS |
| `discover` | 首次摸底容量 | 约按 25%/50%/75%/100% 流数分阶段加压，报告附 `active_streams → avg/P10 RX` 表 |

自动目标只用于明确的 EVB 载体路径：

- `10GUSB/NCM → 10GETH`：默认 RX 目标 6400 Mbps。
- `10GETH → 10GUSB/NCM`：默认 RX 目标 8400 Mbps。
- NCM 显示 4.2G 是已知协商显示问题，按 10GUSB 能力处理；显示 10G 时同样处理。
- RNDIS 即使显示约 3.7G，默认可信负载上限仍为约 2500 Mbps。
- SGMII2.5G 上限约 2500 Mbps，SGMII1G 上限约 1000 Mbps；WiFi 取协商速率与 2500 Mbps 的较小值。
- NCM/10GUSB 经过 CPE 子网中的 SGMII、RNDIS 或 WiFi 时，offered load 按整条路径最低瓶颈裁剪，且默认仍是 `observe`，不会把 2.5G 上限直接当成产品 PASS 线。

EVB 自动目标可以在全局配置中调整：

```json
{
  "iperf": {
    "rate_check": {
      "evb_usb_to_eth_target_mbps": 6400,
      "evb_eth_to_usb_target_mbps": 8400
    }
  }
}
```

也可以按单个场景覆盖。下面配置中 `src=10GUSB`、`dst=10GETH`，所以 `ab` 是 USB→ETH，`ba` 是 ETH→USB：

```json
{
  "name": "EVB 双向自定义目标",
  "src": "master:10GUSB",
  "dst": "agent:10GETH",
  "direction": "bidir",
  "transports": ["udp"],
  "rate_mode": "verify",
  "rate_targets_mbps": {"ab": 6400, "ba": 8400}
}
```

目标优先级为：`tests[].rate_targets_mbps`（或 pairs 的 `universal_params.rate_targets_mbps`）→ 全局 `rate_check.targets_mbps` → 上述 EVB 角色自动目标。`ab` 始终表示配置中的 `src → dst`，`ba` 表示 `dst → src`；单向场景也可以使用 `forward`，因此交换 src/dst 后不要机械照抄 ab/ba 数值。

如果 CPE 已有经过评审的验收目标，应显式写入测试项：

```json
{
  "name": "CPE 双向回归",
  "src": "master:10GUSB",
  "dst": "agent:SGMII2.5G",
  "direction": "bidir",
  "transports": ["udp"],
  "streams": 5,
  "rate_mode": "verify",
  "rate_targets_mbps": {"ab": 2350, "ba": 2200}
}
```

#### `rate_check` 参数

| 字段 | 默认 | 含义 |
|------|------|------|
| `sample_interval_ms` | 1000 | RX/TX 连续采样周期，限制为 200～5000ms |
| `background_secs` | 3 | 起流前背景基线采样；统计会扣除中位背景流量 |
| `startup_timeout_secs` | 15 | 允许失败流快速重试及建立共同窗口的启动阶段 |
| `settle_secs` | 5 | 达到最低活跃流数后丢弃的稳定等待时间 |
| `launch_interval_ms` | 50 | 流之间错峰启动间隔；双向按流序号交错 |
| `min_concurrent_streams` | 2 | 多流测试允许正式计分的绝对最低流数 |
| `min_active_ratio` | 0.9 | 请求流数的最低活跃比例 |
| `offered_headroom_pct` | 5 | 验证目标所需的发送负载余量 |
| `flow_retries` | 1 | UDP 额外重试预算；一般为“初次尝试 + `flow_retries`”，iperf3 单流和 CTS `Connections:1` 每方向总尝试数强制为 `max(flow_retries + 1, 3)` |
| `discovery_step_secs` | 10 | discover 每个负载阶梯的保持时间 |
| `evb_usb_to_eth_target_mbps` | 6400 | EVB USB/NCM → 10G 以太方向的目标 |
| `evb_eth_to_usb_target_mbps` | 8400 | EVB 10G 以太 → USB/NCM 方向的目标 |
| `cpe_path_ceiling_mbps` | 2500 | RNDIS/SGMII2.5G/受限 CPE 路径的默认负载上限，不是 PASS 目标 |
| `max_udp_loss_pct` | null | 可选 UDP 丢包率上限；null 表示不作为门槛 |

完整的人工与自动验收矩阵见 [UDP并发灌包验收场景.md](UDP并发灌包验收场景.md)。

---

## 模块架构

```
cpe_test/
├── Cargo.toml
├── Cargo.lock
├── config.example.json      # 配置文件示例
├── src/
│   ├── main.rs              # CLI 入口 + 模式选择（master/agent/scan/monitor）
│   │
│   ├── agent/
│   │   ├── mod.rs
│   │   └── server.rs        # REST server（16 worker）+ 非阻塞吞吐作业 API
│   │
│   ├── master/
│   │   ├── mod.rs
│   │   ├── ui.rs            # 交互式菜单（复刻旧版交互逻辑）
│   │   ├── executor.rs      # 任务调度/执行/截图/结果库
│   │   └── builder.rs       # spec → 任务单元生成 + 端口分配
│   │
│   ├── nic/
│   │   ├── mod.rs           # scan_host() + 格式输出
│   │   ├── classify.rs      # 角色分类（纯逻辑，不分平台）
│   │   ├── scan_windows.rs  # ipconfig + GetIfTable2 + netsh wlan
│   │   ├── scan_macos.rs    # ifconfig + system_profiler + networksetup
│   │   └── monitor.rs       # NIC RX/TX 连续采样（Windows/macOS/Linux）
│   │
│   ├── cmd/
│   │   ├── mod.rs
│   │   ├── ipconfig.rs      # 解析 ipconfig /all（中英文）
│   │   ├── netsh.rs         # 解析 netsh wlan show interfaces
│   │   ├── iperf.rs         # iperf3 解析、流事件、server 和异步 client job 管理
│   │   └── ctstraffic.rs    # CTS 参数构造、输出解析与受控作业执行
│   │
│   ├── ping.rs              # ping 命令构造 + 执行 + 输出解析
│   │                        # （中/英/BSD 三格式，白名单策略排除 ICMP 错误应答）
│   ├── protocol.rs          # HTTP JSON 请求/响应类型定义
│   ├── config.rs            # JSON 配置文件加载
│   ├── rate.rs              # EVB/CPE 路径上限、已知/未知速率目标策略
│   ├── util.rs              # 子进程(超时/GBK)、日志、时间、辅助函数
│   ├── http_client.rs       # 极简 HTTP/1.1 客户端（零第三方依赖）
│   ├── screenshot.rs        # 截图（Windows GDI / macOS screencapture）
│   └── report.rs            # HTML 报告生成（单文件，内嵌样式）
│
├── .github/
│   └── workflows/
│       └── build.yml        # CI：Windows / macOS / Linux 自动编译 + Release
│
├── dist/                     # 部署用启动脚本（随 Release 分发）
│   ├── start_agent.bat
│   └── start_master.bat
│   ├── start_master_select_config.bat
│   └── configs/              # SGMII / Wi-Fi / 10GUSB 具名配置
│
├── NIC_README.md             # 网卡扫描技术详解
├── UDP并发灌包验收场景.md     # UDP 重构人工/自动验收矩阵
├── README.md
└── 使用说明.md               # 小白版图文教程
```

### 核心交互流程

```
辅测机                         主控机
  ┌──────────┐                ┌──────────────┐
  │  agent   │  HTTP POST     │   master     │
  │  :28801  │◄──────────────►│ 交互菜单/cfg │
  └────┬─────┘                └──────┬───────┘
       │                             │
  ┌────┴─────┐                 ┌─────┴──────┐
  │ nic/scan │                 │ master/    │
  │ ping     │                 │ builder    │
  │ iperf/CTS│                 │ executor   │
  │ monitor  │                 │ report     │
  │ screenshot│                └────────────┘
  └──────────┘
```

1. 主控启动 → 扫描本机 + POST `/info` 获取辅测机网卡
2. 配置文件或交互菜单 → 生成 `SpecNorm`（src/dst/方向/协议/流数）
3. builder → 解析角色名称为具体 IP，分配端口，生成 `Unit` 列表
4. executor → 逐个执行单元；按 iperf3 或 ctsTraffic 语义启动两端角色并持续采集 NIC
   - iperf3 UDP 会先准备所有方向的 server，失败时按流快速重试
   - 每个唯一端点只启动一个 RX/TX 连续采样器并采背景基线
   - 双向流按序号交错启动；远端 client 通过 `/iperf/client/start|status|stop` 后台作业执行
   - 优先用 iperf 行内 `a-b sec`（按 `b-a`）校准缓冲输出的活跃区间，再结合 Traffic/Retry/Ended 重建时间线并取双向共同有效窗口
   - ctsTraffic 通过 `/ctstraffic/start|status|stop` 管理一个承载 N 个 Connections 的作业，并按 TCP/UDP 的真实 client/server 方向启动
   - 分开计算 AB/BA 的 TX/RX 平均、P10、覆盖率、丢包率与原因码
5. 全部完成后生成 HTML 报告 + 自动打开

---

## 角色分类体系

```
角色             判定规则（按优先级从高到低）
───────────────  ───────────────────────────────────────────────
WIFI5G           是 WiFi 且频段 = 5GHz
WIFI2.4G         是 WiFi 且频段 = 2.4GHz
WIFI6G           是 WiFi 且频段 = 6GHz
WIFI             是 WiFi 但频段未知
10GUSB           高速 USB/NCM，速率 4001-12000 Mbps（兼容 NCM 4.2G bug）
RNDIS            描述含 "rndis" / "remote ndis"，优先于 USB/10G 字样
10GETH           速率 9000-12000 Mbps（描述不含 USB）
SGMII2.5G        速率 2400-2600 Mbps
SGMII1G          速率 900-1100 Mbps
RNDIS(兜底)      速率 3400-4000 Mbps
UNKNOWN          以上都不匹配
```

角色排序权重：`10GETH > 10GUSB > SGMII2.5G > SGMII1G > RNDIS > WIFI5G > WIFI6G > WIFI2.4G > WIFI > UNKNOWN`

---

## 原始记录、截图与报告

每个灌包单元完成并回收进程后，会在历史兼容目录 `iperf_outputs/` 保存：

- `iperf_raw_*.log`：client 命令、client stdout/stderr、server stdout/stderr、结构化流事件及所有重试 attempt。
- `ctstraffic_raw_*.log`：CTS 每轮 client/server 命令、stdout/stderr、解析摘要、生命周期和全部重试 attempt。
- `nic_samples_*.csv`：OS 网卡累计 RX/TX 字节、周期增量、当期 RX/TX Mbps、有效性和读取错误。

HTML 报告底部的“原始输出”仍内嵌便于查看的内容，并提供“独立原始记录”和“网卡逐样本 CSV”链接。`master_*.log` 只保存运行进度、摘要、错误和这些文件的路径，不重复塞入大体积的工具原文。复制报告给别人时，应连同整个 `iperf_outputs/` 一起复制。

### 截图流程

```
主控 → 截图请求（label=测试名_方向）
       │
       ├── 若接收端是本机 → capture_png() → PNG 字节
       └── 若接收端是辅测 → POST /screenshot → base64 PNG
               │
               ├── 两端各自尝试截图
               └── 任一成功 → 保存到 iperf_outputs/
                                命名: screenshot_{label}_{主控/辅测}_{时间戳}.png
                                报告链接: ./iperf_outputs/screenshot_xxx.png
```

- 两端都尝试截图，全部成功则报告中出现多个 `查看截图` 链接
- 辅测机不存盘，传完即丢（无磁盘残留）
- 请求失败、HTTP 状态异常、JSON/Base64 解析失败、截图 API 报错和本地写文件失败，都会把具体原因写入 `master_日期时间.log`

### 报告列

| 列 | 说明 |
|------|------|
| 时间 / Task ID / Parent ID | 任务标识 |
| 任务 / IP / 传输 / 参数 | 测试描述 |
| 源/目标 PC / 接口 / IP | 网络端点 |
| 结果 | PASS / RATE_FAIL / UNSTABLE / MEASURED / NOT_EVALUATED / SETUP_ERROR / SKIP |
| 执行状态 / 原因码 / 原因详情 | 区分性能不达标、负载不足、窗口不足、连接或采样环境异常 |
| 请求/活跃/要求流、重试 | 展示真实并发建立情况；2 流只通 1 流时可直接定位 |
| 目标、TX 均值/P10 | 验证是否向 CPE 提供了足够 offered load |
| 接收网卡平均/P10/中位/P95/最低/最高 Mbps | **共同有效窗口内的网卡口径吞吐** |
| 有效/要求秒、采样覆盖率 | 验证是否取得完整有效窗口；滚动窗口不足时原因详情会列出 RX/TX 覆盖百分比 |
| 对向接收 Mbps | 双向时对端实测吞吐 |
| 后端发送/接收 Mbps | iperf3 或 ctsTraffic 的工具证据，用于确认起流和诊断；不代替 NIC 正式吞吐口径 |
| UDP/Ping 丢包率 | 丢包百分比 |
| Ping 平均 ms | 平均时延 |
| 截图 / 执行命令 | 用于复现 |

---

## RESUME 断点续跑

24 小时内已 PASS 的任务自动跳过：

```bash
cpe_test master --auto --resume
```

跳过依据：`task_results.json` 中 task_id（MD5 稳定哈希）的 ok=true 且时间 < 24h。只有正式 `PASS` 会写入可跳过状态；`MEASURED`、`NOT_EVALUATED` 等不会被当成已通过。

---

## 跨平台策略

| 平台 | 角色 | 说明 |
|------|------|------|
| macOS | 主控/辅测 | ping/iperf3/报告可用；ctsTraffic 单元明确不支持 |
| Windows 10+ | 主控/辅测 | ping、iperf3、ctsTraffic 全部支持 |
| Linux | 编译/CI | ping/iperf3 部分功能可用；ctsTraffic 单元明确不支持 |

### 网卡扫描差异

| 功能 | Windows | macOS |
|------|---------|-------|
| 接口枚举 | `ipconfig /all` | `ifconfig -a` |
| 速率 | `GetIfTable2` API | `ifconfig -m` 解析 media 行 |
| WiFi | `netsh wlan show interfaces` | `networksetup` + `system_profiler` |
| IPv4 网关 | `GetIpForwardTable2` 按 InterfaceIndex | `route -n get -inet -ifscope <iface> default` |
| RX/TX 监控 | `GetIfTable2.InOctets/OutOctets` | `netstat -ibn` 的 Ibytes/Obytes |

### ping 输出解析

`ping.rs` 同时兼容 **Windows 中文 / Windows 英文 / macOS(BSD)** 三种 ping 输出格式：

```
Windows(中文):  来自 192.168.1.3 的回复: 字节=32 时间<1ms
Windows(英文):  Reply from 192.168.1.3: bytes=32 time<1ms
macOS:          64 bytes from 192.168.1.3: icmp_seq=0 ttl=64 time=1.605 ms
```

**白名单策略**：regression 发现 Windows 的 ping 统计行（"已发送=X，已接收=Y"）会把
ICMP 错误应答（"无法访问目标主机"、"TTL 传输中过期"、"一般故障"等）也计为"已接收"，
因为本机确实收到了一个回复包，只是不是来自目标主机的 echo reply。

修复：不维护错误消息黑名单，而是反向只认**有 RTT 时间**的目标回复行。逐行匹配
Windows 的 `的回复:` / `Reply from`，或 BSD/macOS 的 `bytes from + icmp_seq`；若该行没有
`时间[<=]\d` 或 `time[<=]\d`，就不是目标 Echo Reply。统计行只提供实际发送数和接收上限，
最终 `received` 必须有逐包 Echo Reply 证据，因此 Redirect 与不可达都不会被算成功。

```
统计行显示:           已发送=4，已接收=4，丢失=0 (0%丢失)
逐行核查后修正:       已接收=0，丢失=4，丢包率=100%  ← ok=false
```

---

## 编译与部署

### macOS 本地调试

```bash
cargo test --locked
cargo build --release --locked
./target/release/cpe_test scan
```

### macOS → Windows 交叉编译

```bash
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64
cargo build --release --locked --target x86_64-pc-windows-gnu
# 产物: target/x86_64-pc-windows-gnu/release/cpe_test.exe
```

### Windows 本地编译（推荐，更稳定）

```bash
# 装一次 rustup: https://rustup.rs （5 分钟）
cargo build --release --locked
# 产物: target\release\cpe_test.exe
```

自行编译后，把 `cpe_test.exe`、启动脚本和所需吞吐工具放到两台 Windows 电脑同一目录：
iperf3 测试需要完整的 iperf3 Windows 发行包；ctsTraffic 测试需要 `ctsTraffic.exe`。
官方 v4.2.0 Windows Release ZIP 已捆绑固定且校验过的 ctsTraffic 2.0.4.0，但由于发行包差异不内置 iperf3。

### GitHub Actions CI

每次推送 tag（`v*`）会先执行 `cargo fmt --check`、`cargo test --locked` 和 `cargo clippy --locked --all-targets -- -D warnings`，全部通过后才锁定依赖编译 Windows / macOS / Linux 三平台并发布到 Release 页面。
手动触发：Actions → Build Release → Run workflow。

配置文件 `.github/workflows/build.yml`：
- `windows-latest` → `x86_64-pc-windows-msvc` 
- `macos-latest` → `aarch64-apple-darwin`
- `ubuntu-latest` → `x86_64-unknown-linux-gnu`

Release 资产使用平台唯一名称，避免不同系统的 `cpe_test` 相互覆盖：

- `cpe_test-<tag>-windows-x86_64.zip`
- `cpe_test-<tag>-macos-aarch64.tar.gz`
- `cpe_test-<tag>-linux-x86_64.tar.gz`
- `SHA256SUMS`

Windows ZIP 包含启动脚本、四份配置、固定 CTS 二进制和第三方许可；Unix 使用
`tar.gz` 保留 `cpe_test` 可执行位。发布作业会再次核对资产名称、数量、内部结构和哈希。

仓库同时跟踪一份不含可执行程序的
[`cpe_test-v4.2.0-windows-config-docs.zip`](dist/cpe_test-v4.2.0-windows-config-docs.zip)，
便于直接从 Git 下载 Windows 配置、文档和启动脚本。其 SHA-256 位于同目录的
`.zip.sha256` 文件；CI 会逐文件确认压缩包内容与仓库源文件一致。需要开箱即用的程序、
固定版 ctsTraffic 和许可证全集时，仍应下载上面的正式 Windows Release ZIP。

---

## 常见问题

### agent 连不上

1. 辅测机的 agent 窗口开着吗？
2. IP 输对了吗？（用辅测机窗口里显示的）
3. 防火墙放行了吗？最快验证：`ping 辅测机IP`
4. 还不行关掉防火墙试一下

### 未找到 iperf3

把 `iperf3.exe`（含同目录的 cygwin1.dll 等）放到 `cpe_test.exe` 同目录。
只测 Ping 或 ctsTraffic 可以不装 iperf3。

### ctsTraffic 不可用或被前置检查阻断

- ctsTraffic 只支持 Windows 10 或更高版本，macOS/Linux 不会尝试启动，也不会自动改跑 iperf3。
- 把 `ctsTraffic.exe` 放到两台电脑各自的 `cpe_test.exe` 同目录，或加入 `PATH`；官方 Windows Release ZIP 已包含它。
- 主控和 agent 必须使用同一个 `cpe_test` Release。旧 agent 没有 `ctstraffic_v1` 能力时会记录 `CTSTRAFFIC_PREFLIGHT_FAILED`。
- 只想继续其他测试时，可把 `kinds` 改为 `["iperf", "ping"]`；只有 CTS 单元会被阻断。

### 网卡列表空白/不全

- 运行 `cpe_test scan --prefix 你本机的网段` 验证
- 默认只认 `192.168.` 开头的 IP，改 `config.json` 的 `ipv4_prefixes`
- 断开的网卡不显示

### 灌包全 FAIL 但 ping 通

- 两端都准备了所选后端吗（iperf3 或 ctsTraffic）？
- 防火墙拦了 56000+ 端口？
- 两端 IP 不同网段？关掉 `require_same_subnet_for_iperf`

### UDP 大档位没生成任务

默认按整条路径的可信负载上限裁剪：RNDIS/SGMII2.5G 约 2.5G、SGMII1G 约 1G，WiFi 取协商速率与 2.5G 的较小值。关掉 `limit_udp_by_link_speed` 可以强制生成，但可能只是在制造发送端拥塞，不建议作为正式验收口径。

### UDP 单向 1 流或双向每方向 1 流始终不通，怎么判

iperf3 UDP 单流和 CTS UDP `Connections:1` 都是每方向独立的硬连通门槛。每个方向总尝试数为 `max(flow_retries + 1, 3)`，双向 AB、BA 各自独立并行；每轮都要完整启动并确认 server/client 清理完成。全部安全尝试仍没有工具自身 rate/bytes/frame/datagram 证据时，iperf3 记录 `RATE_FAIL / SINGLE_UDP_STREAM_FAILED`，CTS 记录 `RATE_FAIL / CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED`。缺工具、平台不支持、非法参数、生命周期或清理失败仍是 `SETUP_ERROR`。一旦已有工具测量，则按真实运行错误、丢包和目标速率判定，不通过继续重试掩盖结果。

### 2 条 UDP 流只通了 1 条，会不会直接判性能 FAIL

不会。失败流会先在启动阶段重试；若最终仍是 1/2，报告为 `NOT_EVALUATED / ACTIVE_STREAMS_LOW`，表示 offered load 搭建不足。该轮不会拿单流速率判断 CPE 吞吐，也不会写入 RESUME PASS。

### 20/32 条流是否仍会被 agent 的 16 worker 分两批

不会。`/iperf/client/start` 只创建后台作业并立即返回，长时间运行的 iperf3 不占 HTTP worker；主控通过 status 轮询事件。日志中的 `[灌包进度][UDP] active=...` 和报告“请求/活跃/要求流”可验证实际并发数。

### 灌包原始记录在哪里

HTML 报告底部仍有“原始输出”。此外，iperf3 会在 `iperf_outputs/` 生成 `iperf_raw_*.log`，ctsTraffic 会生成 `ctstraffic_raw_*.log`，网卡监控会生成 `nic_samples_*.csv`；重试不会覆盖前一次原文。目录名为兼容旧版本仍保留 `iperf_outputs`。`master_*.log` 不重复写全部原始行，只记录进度和文件路径。

### 报告的 RX/P10 是 iperf3 内部速率还是网卡实际速率

是接收端操作系统网卡计数器的实际速率。iperf3 行内的 `0.00-180.00 sec` 只用来识别真实活跃时间，解决 stdout 缓冲导致日志时间差只有几毫秒的问题；它不会参与正式 RX 平均或 P10 数值计算。工具自报值不替代网卡吞吐口径，但 rate/bytes/frame/datagram 等工具自身证据仍是确认单流确实灌通的必要条件，背景 NIC 流量不能替代它。

### 灌包失败重试前会不会先释放端口

会。两种 UDP 后端每轮都先停止 client，再按本轮 `request_id` 精确停止 server，并确认进程已经 `wait` 回收、输出 reader 已结束；确认成功后才允许同端口的下一轮 start。若 kill/wait 或远端确认失败，该方向立即停止重试并报告 `SETUP_ERROR`，单元末尾还会按 `owner_id` 再做批量补偿清理。

### 截图空白/无

- Windows agent 需要 GDI 授权（通常首次跑会自动弹窗）
- macOS 终端需要系统设置 → 隐私 → 屏幕录制 授权
- 截图失败不改变吞吐判定；具体 API/HTTP/解析/写盘原因会记录在 `master_日期时间.log`
