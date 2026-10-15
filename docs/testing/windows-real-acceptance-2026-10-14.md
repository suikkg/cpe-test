# Windows 真机验收脚本复核与修复（2026-10-14）

验收对象为当前未提交工作区，未提交、推送或发布。Windows 原生 release SHA-256：`6092fe9c738a89553b633cb9f193fdd03a82c19c6f1ac2061a10f3a6472a3075`。
编译时 Windows 源码树与本地 306 个文件中 305 个一致；唯一差异是打包后在 `src/master/rate_window.rs` 里改的一段文档注释（只有 `///` 行），不影响二进制。之后这份文件也同步到了 Windows，门禁是在两边完全一致的源码树上跑的。

## 起因：上一轮验收脚本放过了什么

`windows-inner-subnet-parity-2026-10-14.md` 那一轮的脚本（`cpe-native-real.cjs`，当时放在 `/tmp`、没进仓库）：

- **不断言任何判定。** 修复前那一轮 Ping 被 BusyBox 解析误判为 `RATE_FAIL`、上行统计口 eth1 网卡 0.030 Mbps / 板侧 server 653.7 Mbps 判成 `MEASURED`，脚本照样通过（它只在全部断言通过后才写出 `actual-evidence.json`，而修复前那份证据存在）。两处问题都是人工翻证据时才发现的。
- **内环没有走界面。** 配置由脚本手写后直接调 `/api/inner/plan` 和 `/api/inner/run`，`pc_capacity` 也是脚本注入的。
- 两轮对比断言（`RECOVERED === 2`）是照当天历史写死的；历史条目靠「6 个单元」去猜。
- 收尾只查本机进程、TCP 监听和防火墙，没查板侧 iperf3 和 ADB 转发。记录里「CPE 无活动 iperf3」「ADB 转发已清理」没有留下对应产物。
- 速率模式强制成 `observe`，`background_secs=2 / settle_secs=1`，都不是默认值。

## 本轮修复

1. **统计口不在流量路径上，不再判成有效的低速**（`RateStats::counter_off_path`，原因码 `COUNTER_OFF_PATH`）。计数器还在走、平均却不到同一窗口内接收端 iperf3 逐秒记录的一半（工具 ≥ 1 Mbps）时判 `NOT_EVALUATED`。工具值只决定前提、不和门限比较（ADR-17）；两边一致地低（链路本来就慢）照常 `RATE_FAIL`；CTS 不参与。内环「网卡优先，工具兜底」因此自动兜底到工具口径。
2. **内环报告删掉每条腿都挂的两句说明**（`src/inner/mod.rs`：「请保持其他业务空闲，多网卡同网段时核实路由」「本框架逐网口串行执行」）。工具口径 PASS 的判定依据从诊断改进 `detail`。加结构断言：正常完成的腿不产生诊断行。
3. **内环 Ping 接上分辨率说明**（与子网 `ping_leg` 同一份 `ping::resolution_caveat`）。说明文案去掉操作建议和 `**` 标记，子网同步受益。
4. **内环两轮对比的行标题印出板侧统计口**（`compare::counter_label`），换了统计口的两轮不再出现两行标题相同的「新增」「缺失」。
5. **报告「有效测量时长」显示计分时长**（`LegRow::scored_secs` / `innerScoredSecs`），不再把双向重叠 10.96 s 印成「10.96s / 6s」。JSON 的 `effective_secs` 含义不变。
6. **真机验收收进仓库**：`ui/e2e/real/`（`npm run test:real`，Windows 用 `run-windows.ps1`）。`/api/inner/status` 新增 `history_id`，用来把本轮绑定到历史条目。

## 软件检查

- 本地：fmt、Rust 完整测试 1025 通过 / 1 忽略、原生严格 Clippy、Windows MSVC 目标严格 Clippy 全部通过；前端 567 项测试、构建、产物校验通过；Chromium 回归 27 项通过；配置文档包重建，SHA-256 与 `.sha256` 一致。
- Windows（源码树与本地完全一致）：fmt、原生完整测试 1009 通过 / 1 忽略、原生严格 Clippy 全部通过。和本地相差的 16 项与上一轮一样，是各平台专属测试。

## 实机结果（Windows，默认判定口径，不带 `--config`）

环境同上一轮：Windows 11，以太网 192.168.8.103（1000 Mbps）与 WLAN 192.168.8.108；CPE 192.168.8.1，LAN 归属 br0，板侧 `/usr/bin/iperf3`。CPE 的 USB 接在 macOS 上，Windows adb 经 SSH 反向隧道访问 macOS 的 ADB server（两端协议版本均为 1.0.41）。控制台由 `run-windows.ps1` 经 SSH 启动，吞吐任务每个 10 s。四个场景全部通过（3.8 分钟）。

|场景|结果|
|---|---|
|子网：本机两口快捷测试（以太网 ↔ WLAN，界面操作）|`run_20261014_230846_1372`，两方向 MEASURED，719.8 / 855.2 Mbps；历史条目与下载包一致|
|内环：完整两轮（界面选口、整组分配、TCP 双向 / UDP 下行 / Ping 双向）|6 个单元。TCP 四条腿 MEASURED：br0 904.3 / 858.7、以太网 933.6 / 909.6 Mbps，网卡计数均不低于工具值。UDP 请求 4 × 2500，按界面带入的 SGMII1G / 1000 裁成 1 × 1000，实测 561.1 / 561.0 Mbps。Ping 两方向 5/5 PASS，上行 `time<1ms` 带分辨率说明。报告 6 张曲线；Excel 与下载包逐项（含 CRC）和状态一致|
|内环：跳过当前单元|第 1 单元 `SKIP / USER_SKIP`，UDP、Ping 照跑。与完整一轮对比：SKIPPED 1、SAME 2、MISSING 3（第 2 遍），无新增、无法匹配为 0；行标题带「统计口 br0」|
|内环：停止测试|在第 2 单元中停止：只留下第 1 单元的结果，错误为「用户已取消；已完成的结果保留」，没有起 Ping|

收尾：板侧没有 iperf3；Windows 上没有 cpe_test / iperf3 进程，没有临时防火墙规则；隧道已关闭（15037 端口没有 LISTEN）；临时脚本与源码包已删除。

同一块板在 macOS 上作为脚本自检也跑过一遍：内环三个场景通过。子网快捷测试（en0 ↔ en1）按预期失败：macOS 上同机两个地址之间走回环，一个方向命中 `COUNTER_STALLED`，另一个方向命中新的 `COUNTER_OFF_PATH`（网卡 0.195 Mbps，工具 161754.7 Mbps）。改动前，这个方向会判成「MEASURED 0.195 Mbps」。

## 未覆盖

- **截图**：控制台经 SSH 启动，跑在非交互会话里，`BitBlt 失败：句柄无效`。截图失败不影响判定。要覆盖截图路径，需要在桌面会话里运行 `run-windows.ps1`。
- IPv6、RESUME、配置门限后的 PASS / RATE_FAIL、辅测机路径、快捷 UDP、快捷测试后恢复原计划、Windows 直接 USB 接线。
- 默认完整预设与长稳矩阵。

## 证据

本地副本 `target/validation/windows-real-20261014/`；Windows `C:\CPE-Acceptance\real-20261014\ui\test-results\real-evidence\20261014_230824`。包含请求体、逐轮状态、HTML 报告、Excel、下载包、两轮对比、网卡计数前后快照、控制台日志与收尾核对。
