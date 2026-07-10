# CPE 子网测试工具

> 两台电脑间自动化 ping + iperf3 灌包测试，零 Python/零 PowerShell，单二进制分发

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
- **20/32 流真实并发** — agent 的 HTTP worker 只创建/查询后台 iperf 作业，不再被 180 秒 client 占住
- **UDP 连续采样** — 从起流前持续记录双端 RX/TX、流连接/重试/结束事件，结束后重建共同有效窗口
- **已知/未知目标分离** — EVB 已知目标正式验收；CPE、SGMII、RNDIS、WiFi 未知能力默认只测量，不伪造 PASS
- **独立网卡监控** — 不依赖子网测试流程，单独对某网口做逐秒速率采样，输出 CSV
- **跨平台** — 最终两台 Windows，开发期间 macOS 可做全流程模拟测试

---

## 快速开始

### 第 1 步：准备文件

把以下 4 个文件放到两台电脑的同一目录：

```
cpe_test.exe          ← 本工具（单文件）
iperf3.exe            ← 从 iperf.fr 下载（只测 ping 可不放）
start_agent.bat       ← 辅测机双击
start_master.bat      ← 主控机双击
```

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

双击 `start_master.bat`，输入辅测机 IP，一路回车：

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
    --screenshot            每个 iperf 任务后截图
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
    "kinds": ["iperf", "ping"],
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
      { "bandwidth": "1000m", "length": "64" },
      { "bandwidth": "2500m" }
    ]
  },

  "ping": {
    "count": 100,
    "payload_sizes": [32, 1400]
  },

  "tests": [
    {
      "name": "2.5G口灌包",
      "src": "master:SGMII2.5G",
      "dst": "agent:SGMII2.5G",
      "direction": ["A->B", "bidir"],
      "kinds": ["iperf", "ping"],
      "transports": ["tcp", "udp"],
      "ip": ["v4"],
      "streams": 5,
      "iperf_duration": 180,
      "rate_mode": "observe"
    }
  ]
}
```

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
| `kinds` | array | `["iperf"] / ["ping"] / ["iperf","ping"]` | ["iperf"] |
| `transports` | array | `["tcp"] / ["udp"] / ["tcp","udp"]` | ["tcp"] |
| `ip` | array | `["v4"] / ["v6"]` | ["v4"] |
| `streams` | int | 并发流数（TCP = -P，UDP = 独立进程） | 1 |
| `iperf_duration` | int | 覆盖全局 UDP **有效测量窗口**时长（秒）；实际进程会自动增加起流/稳定/重试缓冲 | — |
| `rate_mode` | string | `auto` / `verify` / `observe` / `discover` | 全局值 |
| `rate_targets_mbps` | object | `forward` 或双向 `ab`/`ba` 的明确验收目标；未知时保持 null | — |
| `ping_count` | int | 覆盖全局 ping 包数 | — |
| `ping_payload_sizes` | array | 覆盖全局负载字节 | — |
| `tcp_windows` | array | 覆盖全局 TCP window 档位 | — |
| `udp_profiles` | array | 覆盖全局 UDP 带宽档位 | — |

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

### UDP 并发速率判定

UDP 单流、并发组和双向测试现在走同一套调度器：先把所有方向的 server 全部准备好，再按方向交错启动 client。网卡监控从起流前开始连续采样，直到最后一条流结束；最终根据流事件重建“哪些流在同一时刻真正有流量”的时间线。

`iperf.duration`（默认 180 秒）表示报告中用于判定的**有效稳态窗口**，不是 `iperf3 -t` 的固定进程墙钟时长。程序会自动加入背景采样、错峰起流、连接超时、稳定等待和一次失败流重试缓冲，所以一次 180 秒测试通常会运行约 200 秒；报告会分别列出“有效秒数/要求秒数”。

失败流不会造成无限等待：

- iperf3 的瞬态连接错误先按原机制重试，组调度器还会在 `startup_timeout_secs` 内重启对应 server/client，默认再重试 1 次。
- 2 条流的方向最低要求仍是 2 条。若最终只有 1 条成功，结果是 `NOT_EVALUATED / ACTIVE_STREAMS_LOW`，表示负载搭建不足，不会用单流速率误判 CPE 为 `RATE_FAIL`。
- 默认 `min_active_ratio=0.9`：5 条流要求至少 4 条，20 条要求至少 18 条；已知目标还会叠加 `ceil(目标 × 1.05 / 单流带宽)` 的 offered-load 要求。
- 运行中不阻塞等待并提前锁死窗口；结束后统一取满足活跃流条件的最长连续区间，迟到或提前结束只会如实改变该区间边界。
- 双向结果分别判断 AB/BA 目标，但使用“两边都达到各自最低活跃流数”的共同窗口，避免一边已满载、另一边仍在爬升时提前计分。

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
| `flow_retries` | 1 | startup 阶段组级 server/client 重启次数 |
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
│   │   └── server.rs        # REST server（16 worker）+ 非阻塞 iperf client job API
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
│   │   └── iperf.rs         # iperf3 解析、流事件、server 和异步 client job 管理
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
  │ iperf    │                 │ executor   │
  │ monitor  │                 │ report     │
  │ screenshot│                └────────────┘
  └──────────┘
```

1. 主控启动 → 扫描本机 + POST `/info` 获取辅测机网卡
2. 配置文件或交互菜单 → 生成 `SpecNorm`（src/dst/方向/协议/流数）
3. builder → 解析角色名称为具体 IP，分配端口，生成 `Unit` 列表
4. executor → 逐个执行单元；UDP 单流/多流/双向统一调度
   - 所有方向的 server 全量预启动，失败时快速重试
   - 每个唯一端点只启动一个 RX/TX 连续采样器并采背景基线
   - 双向流按序号交错启动；远端 client 通过 `/iperf/client/start|status|stop` 后台作业执行
   - 根据 Traffic/Retry/Ended 事件重建活跃流时间线，取双向共同有效窗口
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

## 截图与报告

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
| 有效/要求秒、采样覆盖率 | 验证是否取得完整 180 秒有效窗口 |
| 对向接收 Mbps | 双向时对端实测吞吐 |
| iperf 发送/接收 Mbps | iperf3 自报速率 |
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
| macOS | 主控/辅测 | 开发测试用，ping/iperf/报告全流程可用 |
| Windows | 主控/辅测 | 最终生产环境 |
| Linux | 编译/CI | GitHub Actions 自动编译，部分功能可用 |

### 网卡扫描差异

| 功能 | Windows | macOS |
|------|---------|-------|
| 接口枚举 | `ipconfig /all` | `ifconfig -a` |
| 速率 | `GetIfTable2` API | `ifconfig -m` 解析 media 行 |
| WiFi | `netsh wlan show interfaces` | `networksetup` + `system_profiler` |
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

修复：不维护错误消息黑名单，而是反向只认**有 RTT 时间**的回复行。逐行匹配回复标记
（`的回复:` / `Reply from` / `bytes from`），若该行无 `时间[<=]\d` 或 `time[<=]\d`
则不计入真正成功。`saturating_sub` 安全扣减，避免跨平台计数差异导致下溢。

```
统计行显示:           已发送=4，已接收=4，丢失=0 (0%丢失)
逐行核查后修正:       已接收=0，丢失=4，丢包率=100%  ← ok=false
```

---

## 编译与部署

### macOS 本地调试

```bash
cargo build --release
cargo test
./target/release/cpe_test scan
```

### macOS → Windows 交叉编译

```bash
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64
cargo build --release --target x86_64-pc-windows-gnu
# 产物: target/x86_64-pc-windows-gnu/release/cpe_test.exe
```

### Windows 本地编译（推荐，更稳定）

```bash
# 装一次 rustup: https://rustup.rs （5 分钟）
cargo build --release
# 产物: target\release\cpe_test.exe
```

编译后把 `cpe_test.exe` + `iperf3.exe` + 启动脚本放到两台 Windows 电脑同一目录即可。

### GitHub Actions CI

每次推送 tag（`v*`）自动编译 Windows / macOS / Linux 三平台，发布到 Release 页面。
手动触发：Actions → Build Release → Run workflow。

配置文件 `.github/workflows/build.yml`：
- `windows-latest` → `x86_64-pc-windows-msvc` 
- `macos-latest` → `aarch64-apple-darwin`
- `ubuntu-latest` → `x86_64-unknown-linux-gnu`

---

## 常见问题

### agent 连不上

1. 辅测机的 agent 窗口开着吗？
2. IP 输对了吗？（用辅测机窗口里显示的）
3. 防火墙放行了吗？最快验证：`ping 辅测机IP`
4. 还不行关掉防火墙试一下

### 未找到 iperf3

把 `iperf3.exe`（含同目录的 cygwin1.dll 等）放到 `cpe_test.exe` 同目录。
只测 ping 可以不装 iperf3。

### 网卡列表空白/不全

- 运行 `cpe_test scan --prefix 你本机的网段` 验证
- 默认只认 `192.168.` 开头的 IP，改 `config.json` 的 `ipv4_prefixes`
- 断开的网卡不显示

### 灌包全 FAIL 但 ping 通

- 两端都放了 iperf3 吗？
- 防火墙拦了 56000+ 端口？
- 两端 IP 不同网段？关掉 `require_same_subnet_for_iperf`

### UDP 大档位没生成任务

默认按整条路径的可信负载上限裁剪：RNDIS/SGMII2.5G 约 2.5G、SGMII1G 约 1G，WiFi 取协商速率与 2.5G 的较小值。关掉 `limit_udp_by_link_speed` 可以强制生成，但可能只是在制造发送端拥塞，不建议作为正式验收口径。

### 2 条 UDP 流只通了 1 条，会不会直接判性能 FAIL

不会。失败流会先在启动阶段重试；若最终仍是 1/2，报告为 `NOT_EVALUATED / ACTIVE_STREAMS_LOW`，表示 offered load 搭建不足。该轮不会拿单流速率判断 CPE 吞吐，也不会写入 RESUME PASS。

### 20/32 条流是否仍会被 agent 的 16 worker 分两批

不会。`/iperf/client/start` 只创建后台作业并立即返回，长时间运行的 iperf3 不占 HTTP worker；主控通过 status 轮询事件。日志中的 `[UDP进度] active=...` 和报告“请求/活跃/要求流”可验证实际并发数。

### 截图空白/无

- Windows agent 需要 GDI 授权（通常首次跑会自动弹窗）
- macOS 终端需要系统设置 → 隐私 → 屏幕录制 授权
- 截图失败不改变吞吐判定；具体 API/HTTP/解析/写盘原因会记录在 `master_日期时间.log`
