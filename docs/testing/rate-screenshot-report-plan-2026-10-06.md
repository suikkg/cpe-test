# 速率统计、截图完整性、判定与报告/Excel 专项测试方案（v6.6.0，2026-10-06）

本文是测试设计，不是通过报告。所有用例初始状态 `NOT_RUN`。本轮只回答三个问题：

1. **速率统计准不准**：报告上的每个速率数字，能不能从原始样本独立算回来；算法背后的口径假设（背景扣除、窗口定位、采样间隔、双向合计）在真实链路上是否成立。
2. **截图有没有遗漏**：该截的每一条腿、每一端是否都有一张能打开、有内容的图；截不到的时候，读报告的人能不能看出来。
3. **判定和 report / Excel 对不对**：每一行的结论是否符合 ADR-17 / ADR-18 的契约；HTML、Excel、`rows.jsonl`、`meta.json`、进度页是否说的是同一件事。

不是 CPE 性能认证：测到的吞吐只用来检验工具。

## 0. 与 10-05 全面验证的关系

[10-05 方案](windows-full-validation-plan-2026-10-05.md) / [报告](windows-full-validation-report-2026-10-05.md)已经证明：网卡口径的**算术**与保存的 CSV 一致（复算约 7.9 万项）、主要判定分支在真实故障下符合契约、长稳无泄漏。本方案**不重跑**同样的浸泡和起停，只补三块它没覆盖、或只点到为止的地方：

| 方面 | 10-05 的覆盖 | 本轮补什么 |
|---|---|---|
| 速率 | 复算「报告 = CSV」；OS 计数夹逼；封装比；QoS 限速 | 口径假设本身：背景扣除在背景流量变化时的偏差、采样间隔扫描、窗口起点与工具事件的对齐、双向合计的**展示值与判定值**是否同源 |
| 截图 | 仅 A1-F09 一条：「PNG 能解码、尺寸对」 | 按「期望集合」逐腿逐端核对；会话形态（SSH 启动、锁屏、熄屏、多显示器、DPI）；失败时报告里看不看得出；同机端点的重复截图；对测量的干扰 |
| 判定 | 各分支「出现过且合理」 | 每一行都过一遍**独立判定复核器**，不再只看分支出现没有 |
| 报告/Excel | A1-C11 出口一致（抽样） | 字段级映射表全量对账；Excel 能否独立支撑结论；用合成 `rows.jsonl` 重放造真实运行造不出来的边界 |

---

## 1. 被测版本与冻结

- 分支 `feat/webui-vue`，HEAD `a3be5e0`（`release: v6.6.0` 之后只多一条测试修正），`Cargo.toml` 6.6.0，工作区干净。
- Windows 上 `C:\CPE-Acceptance\app\cpe_test.exe` 目前是 **v6.5.2**（10-05 中午的构建），不能直接用。P0-02 从同一份源码在 Windows 原生重新构建。
- 冻结规则沿用上一轮：执行中途任何源码改动，都要重新冻结、重跑阶段 0；改动之前跑过的用例标明对应哪一版。

## 2. 环境（2026-10-06 经 SSH 复核）

| 项 | Mac（本机） | Windows（`ssh windows-cpe`） |
|---|---|---|
| 网卡 | `en0` 有线 1G，192.168.8.102；`en1` Wi-Fi 5G，192.168.8.106 | 「以太网」Realtek 2.5GbE 协商 1 Gbps，192.168.8.103；「WLAN」AX200 协商 2.4 Gbps，192.168.8.108 |
| 显示 | 记录主屏分辨率与缩放 | AMD Radeon 1920×1080；另装有 **Parsec 虚拟显示器驱动**（S-07 要用到） |
| 会话 | — | 交互用户的控制台会话处于活动状态（`query user`），截图依赖它 |
| 表格软件 | Numbers | **无 Excel / WPS / LibreOffice** |
| Python | 3.14（审计脚本在这里跑） | 只有应用商店占位的 `python.exe`，不能用 |
| 遗留 | — | 无 `CPE-*` 计划任务、无 `CPE-*` 防火墙规则、无 iperf3 / ctsTraffic / cpe_test 进程 |

被测 CPE：自研 CPE，OpenWrt 21.02.7，`br0` 192.168.8.1，是两台电脑的网关。四块网卡同在 192.168.8.0/24，都有 link-local 与全局 IPv6。

### 2.1 拓扑与链路

| 拓扑 | 主控 | 辅测 | 这一拓扑独有的覆盖 |
|---|---|---|---|
| T1 | Windows | Mac agent（192.168.8.102:29881） | Windows 主控侧 GetIfTable2 采样、主控端 GDI 截图、Mac 端 `screencapture` |
| T2 | Mac | Windows agent（192.168.8.103:29881） | Windows agent 的 `/screenshot`（base64 回传）与远端采样 |
| T3 | Windows 单机 | — | 两端同在主控（同侧双截图）、ctsTraffic、出厂参数 |

链路：L1 = Win 以太网 ↔ Mac en0（有线标尺）；L4 = Win WLAN ↔ Mac en1（Wi-Fi，触发 ADR-18）；L5 = Win 以太网 ↔ Win WLAN（T3，经 CPE）。数据端口 56000–56400，控制台 29880。

### 2.2 经 SSH 操作的约束

1. **控制面和数据面共用 Windows「以太网」**（SSH 走 192.168.8.103）。测量窗口内不经 SSH 传文件；轮询只用一条短命令、间隔 ≥ 60 s；证据整批跑完再 `scp` 回来。L1 上的测量不受 ≤1 kbps 的空闲 SSH 影响，但一次几 MB 的拷贝会直接进接收端 RX。
2. **截图只能在交互式会话里产生。** Windows 上的主控、agent、OS 计数采样器一律用「仅在用户登录时运行」的计划任务在活动会话里启动，不在 SSH 会话里直接起（SSH 里起的进程不在交互式桌面上，S-04 专门测它会怎样）。模板：

   ```powershell
   $a = New-ScheduledTaskAction -Execute 'C:\CPE-Acceptance\rsr\app\cpe_test.exe' `
        -Argument 'agent --port 29881 --token <本轮令牌> --no-ui' -WorkingDirectory 'C:\CPE-Acceptance\rsr\agent'
   $p = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
   Register-ScheduledTask -TaskName 'CPE-RSR-Agent' -Action $a -Principal $p -Force | Out-Null
   Start-ScheduledTask -TaskName 'CPE-RSR-Agent'
   ```

3. **经 SSH 下发 PowerShell 一律用 `-EncodedCommand`**（UTF-16LE 再 base64）。默认 shell 是 cmd，管道符和引号会把命令拆坏——本次探查环境时已经踩到。中文网卡名在 PowerShell 5.1 脚本里有编码问题，脚本里按 `ifIndex` 取网卡。
4. 测试期间关闭锁屏、屏保、显示器熄灭与睡眠（P0-05 记录原值，收尾还原）；S-05 再单独打开它们测。
5. 令牌只用本轮专用值，只进证据目录。

---

## 3. 代码核对得到的口径

复核器（第 4 节）照这一节**独立实现**，不调用产品代码。下面只写模块路径与符号名；定位用 `grep -n "fn <符号>"`。

### 3.1 速率统计（`master::rate_window::monitor_rate_stats`）

- **样本**：CSV 每行代表 `[elapsed_ms − interval_ms, elapsed_ms)` 一段时间。有效 = `valid` 为真、`interval_ms > 0`、值有限。采样周期被夹在 200–5000 ms（`nic::monitor`）。
- **背景**：基线截止点之前（iperf 为第一条 `Started` 事件，`cmd::iperf_window::iperf_baseline_cutoff_ms`；CTS / UDP 组各有自己的截止函数）的有效样本取中位数。每个样本扣掉背景后下限为 0。
- **窗口**：行上的 `window_start_ms` / `window_end_ms`。样本先裁进窗口、按起点排序，重叠部分只计一次。
- **RX 平均** = Σ(速率 × 窗口内覆盖毫秒) / Σ覆盖毫秒；**采样覆盖率** = Σ覆盖毫秒 / 窗口毫秒（上限 1）。
- **中位 / P95 / 最小 / 最大**：对裁进窗口的样本速率**不加权**取值。
- **分位数**（`percentile`）是最近秩：下标 = `round((n−1)·q)`。Rust 的 `round` 是「远离零」，Python 的 `round` 是银行家舍入——复核脚本必须写成 `floor(x + 0.5)`，否则 n 为偶数时中位数会差一个样本。
- **RX-P10**：5 s 滚动窗口均值序列（`rolling_time_window_series`）的 P10。只用「周期 ≤ 2×标称 + 50 ms、且前一条样本不是无效样本」的样本；一个 5 s 窗口覆盖不足 5000 − 50 ms 不算数。滚动覆盖率 = 实得窗口数 / 期望窗口数。标称周期 = 窗口内部样本周期的下中位数，上限 5000 ms（`nominal_monitor_interval_ms`）。
- **停滞**：窗口内 `rx_delta_bytes == 0` 的最长连续段 / 覆盖毫秒。超过 5% **且**长于 1.5 个采样间隔才算停滞；工具旁证（`RateStats::stall_evidence`）决定它挡不挡判定。
- CSV 速率保留 6 位小数，所以复算容差取 1e-5 Mbps。

### 3.2 判定

| 层 | 规则（顺序即优先级） | 源 |
|---|---|---|
| iperf 腿前提 | client 失败且不属于「全程跑完只丢汇总 / 跑过流量后中途退出」→ SETUP_ERROR `IPERF_EXEC_FAILED`；无吞吐测量 → SETUP_ERROR `NO_VALID_MEASUREMENT`；窗口不完整 → NOT_EVALUATED `IPERF_EFFECTIVE_WINDOW_SHORT` | `executor::verdict_assembly::iperf_flow_verdict` |
| UDP 腿前提 | 0 条流成功：单流重试耗尽 → RATE_FAIL `SINGLE_UDP_STREAM_FAILED`，否则 `NO_STREAM_STARTED`；窗口不完整 → NOT_EVALUATED `EFFECTIVE_WINDOW_SHORT`；流数不足只作诊断 | `udp_leg_verdict` |
| 验收（三条链共用） | 停滞挡判定 → NE `COUNTER_STALLED`；平均缺失或 ≤ 0.01（旁证确认真断流时除外）→ NE `NIC_RATE_MISSING`；覆盖率 < 0.95 → NE `SAMPLE_COVERAGE_LOW`；Observe/Discover 清目标；无目标：Verify → NE `TARGET_MISSING`，其余 → MEASURED `TARGET_UNKNOWN`；RX ≥ 目标 → PASS；否则 RATE_FAIL `RX_BELOW_TARGET` | `rate_window::evaluate_rx_acceptance` |
| Ping | 无网关 → NE `GATEWAY_NOT_FOUND`；执行错误 → SETUP_ERROR `PING_TIMEOUT` / `PING_EXEC_ERROR`；0% 丢包且平均 RTT ≤ A 且最大 RTT ≤ M → PASS `PING_OK`；否则 RATE_FAIL，原因按 丢包 → RTT 缺失 → 平均超限 → 峰值超限 的顺序取第一个 | `executor::ping_leg` |
| 单元聚合 | 空 → SETUP_ERROR；任一 SETUP_ERROR；单流 UDP 硬失败 → RATE_FAIL；遮蔽其他腿的 NE；RATE_FAIL；NE；MEASURED；SKIP；全 PASS | `verdict::aggregate_verdict` |
| 双向合计 | 配了 `rate_target_bidir_total_mbps` 时两腿只测量；合计在两腿真实流量区间的**交集**上、从交集起点截出要求时长后重算；交集不够长 → NE `EFFECTIVE_WINDOW_SHORT` | `verdict_assembly::bidir_total` |

UDP 丢包、CTS 丢帧、TX、滚动窗口、中途掉速、工具退出状态、起流数不足一律只进 `diagnostics`，不改判定。

### 3.3 截图（`master::executor::artifact::Ctx::take_screenshots`）

- **时机**：每条腿判定完、原始记录写盘**之后**才截。截到的是这条腿流量结束后的屏幕，不是流量进行中的屏幕。
- **哪一端**：对任务的 `[dst.side, src.side]` 各截一次。主控端在本进程里截（Windows GDI 只抓主显示器 `SM_CXSCREEN × SM_CYSCREEN`；macOS `screencapture -x`），辅测端 `POST /screenshot`（超时 180 s，PNG 以 base64 走响应）。
- **挂在哪一行**：iperf TCP 每腿一行；UDP 只挂组合计行（流明细行没有）；CTS 每腿一行；Ping 与各类诊断 Ping 没有。
- **失败**：只在 `master.log` 写一行「[截图] …」，判定不受影响。报告里该格显示「未采集」；**某一节（Ping / UDP / TCP）一张图都没有时，截图列整列不出现**。`meta.json` 不记录本轮有没有开截图（控制台路径的 `request.json` 里有）。
- **文件**：`iperf_outputs/screenshot_<标题净化后前 80 字节>_<master|agent>_<时间>_<进程内序号>.png`，`create_new` 写入；行里存相对报告的 `./iperf_outputs/…`。
- **开关的默认值**：命令行看配置（出厂 6 份配置全是 `false`，`--screenshot` 强制打开）；控制台首次打开取内置默认（`Config::default()` 为 `true`），导入项目或恢复默认后被 `ui/src/state/plan.ts` 的 `resetRunOptions` 置为 `false`，恢复草稿和历史重跑按保存值。

### 3.4 出口字段映射

一个单元在 `rows.jsonl` 里是若干明细行加一条单元汇总行（`is_unit_summary`）。Excel「概览」和失败清单取 `report::model::verdict_row`，有汇总行时就是汇总行。

| 字段 | HTML 概览（每方向一行） | HTML 明细 | Excel 概览（每单元一行） | Excel 逐行明细 | 其他 |
|---|---|---|---|---|---|
| 判定 / 原因码 | ✓ | ✓ | ✓ | ✓ | `meta.verdict_totals`、失败清单、进度页、退出码 |
| RX 平均 | ✓ | ✓ | 汇总行的值：单向 = 该方向；双向配合计门限 = **交集合计**；双向未配 = 空 | ✓ | 进度页单元 RX |
| 双向 RX 平均合计 | 双向汇总标题（**两腿各自窗口平均之和**） | — | ✓（同左，两腿之和） | — | 判定用的是汇总行 RX 平均（交集）——见 Q-1 |
| RX-P10 / TX 平均 | ✓ | ✓ | TX ✓ | ✓（另有 TX-P10） | |
| RX 中位 / P95 / 最小 / 最大、窗口起止、背景 | — | ✓ | — | — | 只在 HTML 明细与 `rows.jsonl` |
| 采样覆盖率 / 滚动覆盖率 / 有效秒 / 要求秒 | 覆盖率 ✓ | ✓ | 覆盖率 ✓ | ✓ | |
| 目标 | ✓ | ✓ | ✓ | ✓ | 预览 `PlannedUnit::targets` |
| UDP 丢包 / TCP 重传 | 质量列 | ✓ | ✓ | ✓ | |
| Ping 丢包 | 质量列 | ✓ | ✓ | **无** | |
| Ping RTT 最小 / 平均 / 最大 | 质量列 | ✓ | **无** | **无** | 见 Q-5 |
| 工具自报发送 / 接收 | — | ✓ | — | ✓ | |
| 截图 | 截图列 | 链接与缩略图 | — | — | |
| 诊断 | — | ✓ | ✓ | ✓ | |
| 计划提示 / 运行健康 | 页顶 | — | 表下方抬头区 | — | `meta.json` |

---

## 4. 独立复核工具

四个脚本放证据目录，不进仓库；只用 Python 标准库读 CSV / JSON / HTML / xlsx（`zipfile` + XML）。PNG 像素统计用证据目录里单独建的虚拟环境装 Pillow，不动系统 Python。

| 脚本 | 输入 | 输出 |
|---|---|---|
| `audit_rate.py` | run 目录（`rows.jsonl`、`iperf_outputs/*.csv`、原始记录）、外部 OS 采样 | 每行的复算值与差值；窗口与工具事件的对齐；外部采样夹逼 |
| `audit_verdict.py` | 同上 + 本轮配置 | 每行、每单元按 3.2 推出的期望判定，与报告逐条比 |
| `audit_outputs.py` | run 目录 + 进度快照 | 按 3.4 逐字段比对 HTML / xlsx / `meta.json` / 进度页 |
| `audit_shots.py` | run 目录 + 计划 + `master.log` | 期望截图集合 vs 行里引用 vs 磁盘文件；PNG 解码、尺寸、亮度标准差、文件时间 |

**脚本先自检再用**：每个脚本先喂一份故意改坏的副本——CSV 改一个数、删一张截图、xlsx 改一格、把一行 PASS 改成 RATE_FAIL、把背景值改大——必须报 FAIL。做不到就说明脚本没在检查它声称检查的东西，此时它的 PASS 不算数（P0-07）。

### 4.1 容差（判定之前定好，跑完不许放宽）

| 对账项 | 基准 | 容差 |
|---|---|---|
| RX/TX 平均、中位、P95、最小、最大、P10、背景 | CSV 按 3.1 独立复算 | 绝对 1e-5 Mbps |
| 采样覆盖率、滚动覆盖率、停滞比例 | 同上 | 1e-9 |
| 样本速率与字节差 | `rx_delta_bytes × 8 / interval_ms / 1000` | 相对 0.2%（毫秒取整） |
| CSV 累计计数 | 外部 OS 采样器（P0-06） | 首样本 ≥ 单元开始前读数，末样本 ≤ 单元结束后读数 |
| 判定窗口 | 原始记录 `FLOW EVENTS` 的首个流量事件 + `settle_secs` | 起点 ±100 ms；长度 = 要求时长 ±100 ms（`WINDOW_COMPLETE_TOLERANCE_MS`） |
| 双向合计（判定值） | 交集窗口重算 | 1e-5 Mbps |
| 有线 NIC RX / iperf3 接收端 payload | 按抓包确认的 MSS：带时间戳选项 1514/1448 ≈ 1.046，不带 1514/1460 ≈ 1.037；UDP `-l 1400` v4 1442/1400，v6 1462/1400 | ±2%；双向 TCP 混有对向 ACK，只记录不判 |
| 背景扣除 | 3.1 的公式代入实测样本 | ±2% |
| Ping | 原始逐包行与汇总行 | 发/收/丢完全一致；RTT 与 Windows 汇总一致（整数毫秒） |
| Excel 数值 | `rows.jsonl` | 逐位相等 |
| HTML 数值 | `rows.jsonl` 四舍五入到 3 位 | ≤ 0.0005 |
| 截图 | 期望集合（S-00） | 0 缺失；尺寸 = 该机物理分辨率；亮度标准差 > 2（排除纯黑 / 纯色） |

---

## 5. 阶段 0：冻结、部署、采样器与脚本自检（约 2 h）

| ID | 内容 | 通过标准 |
|---|---|---|
| P0-01 | `git archive a3be5e0` 生成源码包与 SHA-256 清单；Mac `cargo build --release --locked`，记录 exe 哈希 | 清单与工作区一致 |
| P0-02 | 源码包 `scp` 到 `C:\CPE-Acceptance\rsr\src`，Windows 原生 `cargo test --locked`、两条 clippy、`cargo build --release --locked`；exe 与 `app\` 下的 iperf3 / ctsTraffic 一起放进 `C:\CPE-Acceptance\rsr\app`，记录哈希 | 全部退出码 0；两端清单零差异；`cpe_test --help` 显示 v6.6.0 |
| P0-03 | Mac 跑 AGENTS.md §1 四条门禁 | 全绿（绿灯只证明没退回去） |
| P0-04 | 环境快照：两端网卡速率 / 频段 / 信道 / 信号、IP、MTU、主显示器分辨率与缩放、会话状态、电源与锁屏设置 | `environment.json` 两端各一份 |
| P0-05 | Windows：防火墙规则组 `CPE-RSR` 按程序放行 56000–56400 与 29880–29881，远端只限 192.168.8.0/24 与本网段 IPv6；关闭锁屏、屏保、显示器熄灭、睡眠（存原值）。Mac：确认启动 agent 的终端有「屏幕录制」授权，执行期间 `caffeinate -dimsu` | 规则可整组删除；原值已存档 |
| P0-06 | 外部 OS 计数采样器：Windows 用计划任务起一个 PowerShell 循环，每秒按 `ifIndex` 读 `Get-NetAdapterStatistics`，带毫秒 UTC 时间写本机 CSV（不走网络）；Mac 同样每秒读 `netstat -ibn` | 两端采样器连续运行、无断档 |
| P0-07 | 四个复核脚本按第 4 节自检；用 10-05 证据目录里一个已知 run 跑一遍，结果应与当时的审计一致 | 每个故意改坏的副本都被抓出来 |
| P0-08 | 裸工具基线：工具之外直接用 iperf3 跑 L1、L4 各方向 TCP / UDP 20 s ×2 | 得到每条链路的参考值，后面的门限都从它推 |

---

## 6. 运行批次

用例共享运行：先把批次跑出来，再对每个批次的 run 目录跑全部四个复核脚本。批次之间不改源码。

公共配置（证据目录内，不进仓库）：`tests[]` 显式列举；`iperf.duration` 30；`rate_check` 取 `mode: auto`、`sample_interval_ms` 1000、`background_secs` 3、`settle_secs` 3；TCP 窗口 `1m`、`-P 4`；UDP 档位 `300m / -l 1400 / window 1m`（跨平台必须显式写 window，省略会走 Windows 的 256m，在 Mac 上必然报错）。

| 批次 | 拓扑 | 内容 | 入口 | 截图 | 约时长 |
|---|---|---|---|---|---|
| RUN-A 主矩阵 | T1 | L1、L4：TCP、UDP 单流、UDP 3 流、ping 32/1472 ×100，三方向，v4；L1 另加 TCP 与 ping 的 v6。约 39 单元 | 命令行 `master --auto --screenshot` | 开 | 45 min |
| RUN-B 门限分支 | T1 | 按 P0-08 基线设门限：0.5×（PASS）、1.2×（RATE_FAIL）、Verify 不给门限、Observe 给门限、门限 1180 撞 1G 链路上限；L4 双向配合计门限（取 RUN-A 两腿合计 ±3%）×5 轮；ping 门限收紧到 1 ms | 命令行 | 开 | 50 min |
| RUN-C 故障注入 | T1 | 防火墙中途断流 10 s（门限一高一低各一次）；Mac Wi-Fi 断 15 s；单元中途杀 Windows 上的 iperf3；整单元挡数据端口；ping 同网段未用地址；完成后带 `--resume` 再跑一遍 | 命令行 | 开 | 40 min |
| RUN-D 角色互换 | T2 | L1、L4 的 TCP 双向、UDP 3 流双向。Windows agent 依次在：交互式会话、SSH 会话、锁屏状态下启动 / 运行（S-04 / S-05） | 命令行（Mac 上） | 开 | 45 min |
| RUN-E 单机 | T3 | L5：TCP、UDP 3 流、ping、命令行 ctsTraffic TCP/UDP，三方向 | 命令行（交互式任务） | 开 | 40 min |
| RUN-F 采样扫描 | T1 | L1 TCP 单向：`sample_interval_ms` 200 / 500 / 1000 / 2000 / 5000 × `duration` 10 / 30 / 180（10 s 与 180 s 只跑 1000 ms） | 命令行 | 关 | 50 min |
| RUN-G 背景流量 | T1 | L1 Mac→Win TCP 30 s，另起一条不受工具控制的 50 Mbps UDP（Mac→Win 以太网，端口 57000），三种形态见 R-06 | 命令行 | 关 | 20 min |
| RUN-H 控制台 | T1 | Windows `ui --ui-bind 192.168.8.103`，Mac 浏览器访问：截图开关的各种入口、进度页抓取、跳过 / 停止、历史下载包、重放 | 控制台 | 按用例 | 60 min |
| SYN 合成重放 | — | RUN-A 目录的副本，改 `rows.jsonl` 后 `cpe_test report` 重放（X-07） | 命令行 | — | 40 min |

---

## 7. 用例

### 7.1 速率统计（R）

| ID | 内容 | 批次 | 通过标准 |
|---|---|---|---|
| R-01 | 全量复算：每一行的 RX/TX 平均、中位、P95、最小、最大、P10、背景、覆盖率、滚动覆盖率、停滞比例 | 全部 | 4.1 第一、二行 |
| R-02 | 样本自洽：相邻累计计数差 = 保存的 delta；速率 = 字节差折算；delta 为负只允许出现在有记录的复位 / 回绕处 | 全部 | 4.1 第三行 |
| R-03 | OS 计数夹逼与时间对齐：每个单元的 CSV 首末累计计数落在外部采样器的前后读数之间；用累计计数插值把单元 `elapsed_ms` 换算成绝对时刻，供 S-09、S-10 使用（两边计数器不同源时只做夹逼） | 全部 | 4.1 第四行 |
| R-04 | 窗口定位：窗口起点 = 原始记录里首个真实流量事件 + `settle_secs`；长度 = 要求时长；终点不晚于最后一个流量证据。UDP 组按 `executor::window::leg_active_span`，CTS 按状态行的 TimeSlice | A、E、F | 4.1 第五行 |
| R-05 | 网卡口径与工具口径：L1 有线 TCP、UDP v4/v6 的 NIC RX / iperf3 接收端 payload 比值（先抓包确认 MSS） | A | 4.1 第七行；Wi-Fi 行不判（10-05 的 O-01：Wi-Fi 驱动计数口径不一） |
| R-06 | 背景扣除。G1：背景 UDP 全程开着 → 报告 RX ≈ 总 RX − 50，`baseline_mbps` ≈ 50；G2：背景只在基线段开，起流前停 → 报告 RX 预计**偏低约 50**；G3：背景只在判定窗口中间开 10 s → 报告 RX 预计**偏高约 50×10/30** | G | 三种形态都与 3.1 公式的预测一致（±2%）。G2、G3 的偏差是「基线 = 起流前中位数」这条口径的固有性质，结果写进报告的「口径风险」，要不要改由用户决定，不登记为缺陷；与公式不一致才是缺陷 |
| R-07 | 采样参数扫描：同一链路不同采样周期的 RX 平均；期望的滚动窗口数；P10 只在窗口 ≥ 5 s 时出现；10 s 窗口单个零样本不判停滞（Q-03 回归）；配置 100 ms 时实际被夹到 200 ms 且 CSV 能看出来 | F | 各采样周期的平均相对 1000 ms 的结果偏差 ≤ 1%；其余与 3.1 一致 |
| R-08 | 双向合计：按两腿原始事件求交集窗口，用两腿接收端 CSV 重算合计，与三个数比：①汇总行 RX 平均（判定值）；②HTML 双向汇总标题的「双向 RX 平均合计」；③Excel 概览「双向 RX 平均合计」。另统计 ①与②的差值分布，以及有没有哪一轮 ①、② 落在门限两侧 | A、B | ① 与重算值差 ≤ 1e-5。② ③ 与 ① 不同源时按 Q-1 处理 |
| R-09 | 中途断流：防火墙挡 10 s。诊断里断流的起止秒数与 CSV 零增长段一致（±1 个样本）；断流计入平均；旁证判为真断流，不出现 `COUNTER_STALLED`；曲线在断口处断开 | C | 全部满足 |
| R-10 | Ping 统计：逐包行与汇总行独立复算（发 / 收 / 丢 / 最小 / 平均 / 最大）；`时间<1ms` 的处理；同网段未用地址的「无法访问目标主机」不算收到 | A、C、E | 4.1 Ping 行 |
| R-11 | 进度页与最终结果：每个单元结束时进度页给出的单元 RX / 目标 = 汇总行 | H | 完全一致 |
| R-12 | 显示舍入：HTML 全部数值 = `rows.jsonl` 四舍五入 3 位；xlsx 数值逐位等于 `rows.jsonl` | 全部 | 4.1 最后两行（数值） |
| R-13 | 限速真值抽检：Windows QoS 在 L1 Win→Mac 限 300 / 500 Mbit/s，先用裸 iperf3 确认生效 | 单独 | 工具 RX ∈ [0.95R, 1.08R]（与 10-05 B3-C03 同容差） |

### 7.2 截图（S）

#### S-00 期望集合

| 单元 | 截图挂在哪一行 | 跨机（T1 / T2） | 同机（T3） |
|---|---|---|---|
| iperf TCP 单向 | 该腿行 | 主控 1 + 辅测 1 | 同侧截 2 次，行里只存后 1 张（Q-3） |
| iperf TCP 双向 | AB、BA 两行各自 | 每腿各 2，共 4 | 每腿写 2 引用 1 |
| iperf UDP（单流 / 多流，单向 / 双向） | 组合计行；流明细行没有 | 同 TCP | 同 TCP |
| ctsTraffic | 每腿一行 | 同 TCP（跨机 CTS 被平台门禁拒，实际只有 T3） | 同 TCP |
| 子网 Ping、诊断 Ping、路径 MTU | 无 | 0 | 0 |
| RESUME 跳过、开跑前重扫判 `NIC_DISAPPEARED`、计划阶段被跳过的项目 | 无 | 0（预期，实测确认） | 0 |
| 被「跳过当前单元」或「停止」掐断的腿 | 照常截（腿收尾之后） | 记录实际 | 记录实际 |

`audit_shots.py` 按上表从计划推出期望集合：一行一端一张。

#### 用例

| ID | 内容 | 批次 | 通过标准 |
|---|---|---|---|
| S-01 | 开关入口：命令行用出厂配置（`false`）、加 `--screenshot`、配置写 `true`；控制台首次打开、导入项目后、恢复默认后、恢复草稿后、从历史重跑；同时看 `cpe_test --help` 里有没有 `--screenshot` | A、H | 每个入口的实际行为（看 `request.json` 与 `iperf_outputs/` 里有没有 PNG）与 3.3 一致。首次打开为开、导入后为关这一不一致按 Q-2 处理；帮助文本缺项按 Q-7 处理 |
| S-02 | 跨机全覆盖（Windows 主控 + Mac 辅测） | A、B、C | 期望集合 0 缺失；每张都存在、能解码；Windows 图 1920×1080，Mac 图等于其主屏分辨率；亮度标准差 > 2；磁盘上的 PNG 数 = 被引用数 |
| S-03 | 跨机全覆盖（Mac 主控 + Windows 辅测，交互式会话） | D | 同 S-02；另记录最大一张 PNG 的 base64 长度，与 `http_client::MAX_RESPONSE_BYTES` 的余量 |
| S-04 | Windows 进程不在交互式会话里：agent 从 SSH 会话直接启动（T2）；主控从 SSH 会话直接启动（T1） | D、单独 | 截图失败或全黑都可以接受，但①判定与 RX 不受影响；②记录读报告的人能不能看出来：截图列是「未采集」还是整列消失、`meta.json` 有没有痕迹、`master.log` 写了什么（Q-4） |
| S-05 | 屏幕状态：锁屏（在交互式会话里调 `LockWorkStation`）、显示器熄灭、屏保运行 | D | 不挂起；单个截图耗时 ≤ 180 s 超时；结果按 S-04 的口径记录 |
| S-06 | macOS 辅测没有「屏幕录制」授权（换一个未授权的终端启动 agent） | 单独（T1） | 失败或只截到桌面背景都可接受；`audit_shots.py` 能标出「有图但没内容」；可见性同 S-04 |
| S-07 | 多显示器与缩放：启用 Parsec 虚拟显示器做扩展屏；系统缩放 125% / 150% | 单独（T1） | 多屏时只截主屏（按设计，记录）；缩放后图片尺寸仍是物理 1920×1080，不是 1536×864（DPI 感知在 `main` 里设）。改缩放需要注销时记 BLOCKED |
| S-08 | 同机端点的重复截图 | E | 每腿写盘 2 张、引用 1 张。确认后按 Q-3 处理；记录本轮孤儿文件数与总大小 |
| S-09 | 截图时效：PNG 修改时间晚于同一腿原始记录的修改时间、早于下一单元第一个产物；借 R-03 的时间对齐标出截图时刻落在 RX 曲线的哪里 | A、D、E | 每张都在本腿流量结束之后、下一单元开始之前；记录「腿结束 → 截图落盘」的耗时分布（主控端、辅测端分开） |
| S-10 | 截图对测量的干扰：L1 TCP 双向，截图开 / 关各 3 次。辅测端截图的 base64 走 `agent_host`（Mac en0，正是 L1 的数据网卡），先结束的那条腿截图时，另一条腿可能还在窗口里 | 单独 | 用 R-03 对齐检查有没有截图传输落进任何一条腿的判定窗口；开 / 关两组每腿 RX 中位数差 ≤ 1% |
| S-11 | 路径长度：运行目录放在约 200 字符深的路径下，链路集合名用 > 80 字节的中文 | 单独（T1） | 文件名按 80 字节截断后照常写入；超过 260 字符写不成时只记日志、该格「未采集」、判定不变、不崩溃 |
| S-12 | 链接可达：报告里引用的每个路径相对 `report.html` 都存在；控制台历史下载的 `bundle.zip` 里都有；解包后在 Mac 上断网用 Chrome 打开，没有破图；`cpe_test report` 重放、整个 run 目录挪位置之后链接仍然有效 | A、H | 全部满足 |
| S-13 | 截图失败不影响判定：S-04 / S-05 / S-06 各批的判定与 RX，和截图正常的同参数批次相比只有正常的轮间波动 | D、单独 | 不出现由截图引起的 SETUP_ERROR；同单元 RX 差在 R-07 的波动范围内 |

### 7.3 判定（V）

| ID | 内容 | 批次 | 通过标准 |
|---|---|---|---|
| V-01 | 判定复核器全量：每一行按 3.2 推出期望 `(判定, 原因码)`。速率行的比较用 R-01 **复算出来的** RX 平均，不用行里自己的数，这样算术错和比较错都抓得到；复算值与目标相差 < 1e-5 的「贴边行」单独列出人工复核。另外断言：判定为 PASS / RATE_FAIL / MEASURED 的行，覆盖率 ≥ 0.95、窗口完整、没有停滞挡判定 | 全部 | 0 不一致 |
| V-02 | 分支覆盖（每个分支至少一行真实数据）：PASS、RATE_FAIL `RX_BELOW_TARGET`、MEASURED `TARGET_UNKNOWN`、NE `TARGET_MISSING`、NE `IPERF_EFFECTIVE_WINDOW_SHORT`（中途杀 iperf3）、SETUP_ERROR（整单元挡端口）、SETUP_ERROR `NIC_DISAPPEARED`（开跑前关 Mac Wi-Fi）、SKIP `RESUME_FRESH_PASS`、带断流诊断的 PASS 与 RATE_FAIL、`PING_OK`、`PING_PACKET_LOSS_HIGH`、`PING_RTT_MAX_EXCEEDED`、CTS 的 MEASURED 与窗口不足 | B、C、E | 每个分支都出现，且 V-01 通过。`COUNTER_STALLED`、`SAMPLE_COVERAGE_LOW` 实机难以稳定触发，由单元测试覆盖，记 NA 并写明理由 |
| V-03 | 门限来源：每行 `target_mbps` 等于计划预览给出的生效门限，来源标签正确；门限 1180 在 1G 链路上被折算，并出现在日志、`meta.json`、HTML 顶部、Excel 抬头 | B、H | 全部一致 |
| V-04 | 诊断不改判：带 `UDP_LOSS_HIGH`、发送端负载不足、`RX_DROPOUT` / `RX_OUTAGE`、流数不足等诊断的行，判定仍只由 RX 与目标决定 | A、C | V-01 对这些行无一例外 |
| V-05 | 单元聚合：双向两方向各配门限，一边高一边低 → 单元 RATE_FAIL；UDP 3 流中途杀掉其中一条流的 iperf3 → 只进诊断（口径 B）；单元汇总行 = 复核器按 `aggregate_verdict` 重算的结果 | B、C | 全部一致 |
| V-06 | 计数与退出码：HTML 顶部八格 = `meta.json` 的 `verdict_totals` = 复核器按单元计数；SKIP 不进总数；命令行退出码：RATE_FAIL + NOT_EVALUATED + SETUP_ERROR 单元数 > 0 或收尾清理有错 → 1，否则 0（`RunSummary::any_not_passed`；MEASURED 与 SKIP 不算） | 全部 | 完全一致 |
| V-07 | 人为干预：控制台「跳过当前单元」→ 该单元有说明；「停止」→ 只有确实被掐断的单元有说明，已跑完的单元没有 | H | 与 `executor::operator_interruption_note` 的约定一致 |

### 7.4 报告与 Excel（X）

| ID | 内容 | 批次 | 通过标准 |
|---|---|---|---|
| X-01 | 多出口逐字段对账：按 3.4 把 `rows.jsonl`、HTML（标准库 `html.parser` 解析）、xlsx 四张表、`meta.json`、进度页逐项比 | 全部 | 数值按 4.1；文本完全一致；单元序号 = `group_seq` = Excel 序号 = 日志里的 `[i/N]` |
| X-02 | HTML 结构：每个单元只出现一次；双向单元 AB / BA 各一行；按 Ping / UDP / TCP 分节；计划提示与运行健康横幅的出现条件；有 CSV 时才画曲线；断网用 Chrome 打开 `file://`，零外部请求 | 全部 | 全部满足 |
| X-03 | Excel 结构：四张表名正确；首行冻结；自动筛选覆盖全部数据行；数值是数字单元格；缺失值是空格子而不是 0；覆盖率按 0.0% 显示；「概览」行数 = 单元数；「逐行明细」行数 = 非汇总行数；「按链路分组」的「方向执行数」之和 = 各单元方向数之和，通过率 = PASS / (PASS + RATE_FAIL)；「失败清单」恰好是 RATE_FAIL / NOT_EVALUATED / SETUP_ERROR 的单元，处置建议不为空；链路集合名用 `=1+1`、`@SUM(A1)` 时仍是文本 | 全部、H | 全部满足；用 Numbers 打开无报错。**用真 Excel 打开**两台机器都没有 Office，记 BLOCKED，除非操作员另给一台 |
| X-04 | Excel 能否独立支撑结论：对每一类判定，看 Excel 里能不能看到「拿什么比、和谁比」 | A、B | TCP / UDP：RX 平均与目标都在。双向合计见 Q-1。Ping 的 RTT 超限见 Q-5 |
| X-05 | 重放一致：对 run 目录的副本跑 `cpe_test report`，HTML 与原报告逐字节比（有差异逐条说明来源），xlsx 逐格相等，`meta.json` 不被改动 | A、E | 除可解释的差异外完全一致 |
| X-06 | 崩溃重放：RUN-A 子集跑到一半 `taskkill /F` 主控，然后重放 | 单独 | 报告恰好包含已完成的单元；计数自洽；xlsx 照常生成；`rows.jsonl` 末行不完整时计入 `skipped` 而不是整份读失败 |
| X-07 | 合成边界（SYN，改副本的 `rows.jsonl` 后重放）：①RX 恰好等于目标的 PASS 行；②没有类型化字段的旧行（走兜底推断）；③名称含 `<script>`、`&`、引号、emoji、500 字的标题；④某个文本字段 > 32767 字符；⑤数值为 null；⑥最后一行被截断；⑦双向行：两腿之和 ≥ 门限、汇总行 RX 平均 < 门限、判定 RATE_FAIL；⑧全部 SKIP / 0 个单元 | SYN | ③ 全部转义、不执行；⑤ 显示「—」或空格子，不出现 NaN；⑥ 计入 `skipped`；⑦ 看 HTML 与 Excel 怎么呈现（Q-1）；④ 看是否只得到一条警告、旧的 `summary.xlsx` 是否残留（Q-6） |
| X-08 | 编码：中文网卡名在 CSV 文件名里被净化；CSV 用 Numbers 打开是否乱码（CSV 不带 BOM，中文 Windows 上的 Excel 可能乱码）；HTML 声明 UTF-8 | A、E | 记录现状；文件名必须全 ASCII |
| X-09 | 历史下载包：`bundle.zip` 的条目与 run 目录文件一一对应、大小一致；解包后离线打开报告 | H | 完全一致 |

---

## 8. 预先登记的疑点

下面几条是读代码时看到的，**都还没在实机上证实**。按仓库约定，每条先独立证伪一次再谈修：先用实机或合成数据确认现象存在，再判断它是不是问题、问题有多大。

| ID | 疑点 | 依据 | 怎么证实 / 证伪 | 若成立 |
|---|---|---|---|---|
| Q-1 | 双向合计的**展示值与判定值不同源**。配了合计门限时，判定和汇总行的「RX 平均」用两腿交集窗口重算的合计；HTML 双向汇总标题（紧挨着门限显示）和 Excel 的「双向 RX 平均合计」列却是两腿**各自窗口**平均之和（`report::model::bidirectional_rx_average_sum`）。两腿起流有先后时，Wi-Fi 上单独跑的那一段会抬高后者——ADR-18 正是为了排除这一段才改成取交集的。于是 Excel 同一行的两列、HTML 标题与原因明细，可能各说一个数，甚至落在门限两侧 | `executor` 汇总行的 `unit_rx_avg`；`verdict_assembly::bidir_total`；`report.rs` 双向汇总标题 | R-08 统计实机差值分布；X-07 ⑦ 用合成数据看呈现 | 报告自相矛盾，建议 P2：门限旁边只能放判定用的那个数 |
| Q-2 | 控制台截图开关默认值前后不一致：首次打开为开（内置默认 `true`），导入项目或恢复默认后为关（`resetRunOptions`）；出厂 6 份配置全是 `false`。用户导入项目后跑完才发现一张图都没有 | `ui/src/state/plan.ts`；`Config::default` | S-01 | 截图「遗漏」最可能的来源，建议统一默认值（选哪个由用户决定） |
| Q-3 | 同机端点（T3 单机）每条腿对同一侧截两次：第二张覆盖了行里的路径，第一张成为没有被引用的孤儿文件 | `take_screenshots` 对 `[dst.side, src.side]` 逐个截、后写覆盖前写 | S-08 数文件 | 浪费时间和磁盘，不影响结论；建议 P3 |
| Q-4 | 截图开着但全部失败时，报告与 `meta.json` 没有任何痕迹：该节截图列整列不出现，和「没开截图」看起来一样；原因只在 `master.log` | `report.rs` 的 `has_shots`；`RunMeta` 不含截图开关 | S-04 / S-05 / S-06 | 「遗漏不可见」，建议 P2/P3：报告抬头说明本轮开了截图、成功几张 |
| Q-5 | Excel 里没有 Ping RTT：「概览」只有 Ping 丢包，「逐行明细」连丢包都没有。RTT 超限导致的 RATE_FAIL 在 Excel 里只能从「原因明细」文字里读 | `report::xlsx` 的表头 | X-04 | 建议 P3：补 RTT 列 |
| Q-6 | 重放时 Excel 生成失败（例如某个文本字段超过 Excel 单元格 32767 字符上限，`rust_xlsxwriter` 会报错），新的 `report.html` 照常写出，旧的 `summary.xlsx` 原样留在目录里，两个出口不同源；提示只在重放的 `warnings` 里 | `master::ui::replay_report_into` | X-07 ④ | 实际触发概率低，建议 P3：失败时删掉旧文件或改名 |
| Q-7 | `cpe_test --help` 的 master 段没有列 `--screenshot`（README 有，解析也认） | `main.rs::print_help` | S-01 顺带看 | 文档同步义务（AGENTS.md §5），P3 |

顺带（不需要实机）：`report::model::Row::nic_samples_tx` 的字段注释还写着「TX 采样是否决性门槛」，与 ADR-17 相反，代码行为已经改了，只是注释没跟上。

---

## 9. 执行顺序与时间预算

| 顺序 | 内容 | 预计 | 依赖 |
|---|---|---|---|
| 1 | 阶段 0（P0-01～08） | 2 h | — |
| 2 | RUN-A → 四个复核脚本 | 1.5 h | P0-07 脚本自检通过 |
| 3 | RUN-B、RUN-C → 复核 | 2.5 h | P0-08 基线 |
| 4 | RUN-F、RUN-G、R-13 | 1.5 h | — |
| 5 | RUN-D（含 S-04 / S-05）、S-06、S-07、S-10、S-11 | 3 h | S-02 通过 |
| 6 | RUN-E（单机） | 1 h | — |
| 7 | RUN-H（控制台）、X-06 | 1.5 h | — |
| 8 | SYN、疑点逐条定性、写报告 | 2 h | 以上全部 |

合计约 2 个工作日，不需要夜间浸泡。疑点 Q-1～Q-7 定性之后要不要修、怎么修，先拿结论问用户，不在执行中途改源码（改了就回到阶段 0）。

## 10. 环境改动与清理

每个阶段结束执行一次，写 `cleanup.json`。只清理本轮自己的资源。

| 端 | 改动 | 恢复 |
|---|---|---|
| Windows | 计划任务 `CPE-RSR-*`（agent、主控、控制台、OS 采样器、定时注入） | 逐个停止并注销，确认列表为空 |
| Windows | 防火墙规则组 `CPE-RSR`、`CPE-RSR-inject` | `Remove-NetFirewallRule -Group …`，确认 0 条 |
| Windows | QoS 策略 `CPE-RSR-*` | `Remove-NetQosPolicy -PolicyStore ActiveStore` |
| Windows | 锁屏、屏保、显示器熄灭、睡眠；缩放比例；Parsec 虚拟显示器 | 按 P0-05 / P0-04 存档值还原 |
| Windows | `C:\CPE-Acceptance\rsr\` | 保留作证据；构建产物备份在里面 |
| Mac | agent、背景 iperf3、OS 采样器、`caffeinate` | 全部退出；29881 无监听 |
| Mac | Wi-Fi 开关、临时启动 agent 用的未授权终端 | Wi-Fi 打开并重新关联；不改屏幕录制授权本身 |
| 两端 | 进程与端口 | 无 cpe_test / iperf3 / ctsTraffic 残留；29880–29881、56000–56400、57000 无监听 |

## 11. 证据与记录

- 证据根目录：Mac `<验收证据目录>/rsr-2026-10-06/<批次或用例>/`，Windows `C:\CPE-Acceptance\rsr\evidence\`。每个批次保存：配置、计划预览、run 目录全文、外部 OS 采样 CSV、`master.log`、进度快照（RUN-H）、四个脚本的 JSON 输出、命令与退出码。
- 每个用例记录：ID、状态、版本哈希、run_id、预期、实际、偏差说明、证据路径、缺陷 ID。
- 缺陷登记 `D-20261006-nn`：复现步骤、最小配置、原始证据，并分清「用例 FAIL」与「产品判定」各是什么。疑点 Q-x 证实后转成缺陷编号；证伪了就在报告里写明证伪过程。
- 写进仓库的记录只写「自研 CPE」「被测 CPE」，不写厂商、SSID、主机名、本机路径和令牌。不提交、不推送，除非另行要求。

## 12. 不在本轮范围

内环（ADB）与组合场景、两轮对比报告的对齐规则、长稳浸泡与反复起停、控制台布局与交互细节、出厂参数的全量矩阵——这些在 10-05 已覆盖，本轮不改动相关代码，不重测。仍然缺设备的：第二台 Windows（双 Windows 跨机 CTS）、Windows 10、RNDIS / 10GUSB / 2.5G 实速网口、Wi-Fi 6 GHz、4K 主屏（高分屏截图的响应体上限只能由单元测试覆盖）。
