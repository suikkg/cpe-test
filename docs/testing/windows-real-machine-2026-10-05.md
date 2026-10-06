# Windows 实机验收记录（2026-10-05）

当前版本 v6.5.2，来源为 995b19d 工作区快照及本次修复；未提交或推送。

## 环境与范围

Windows 11 Pro 192.168.8.103（以太网，1 Gbps）作为主控，macOS 192.168.8.102（en0，1000baseT 全双工）作为辅测，agent 端口 29881。实际 CPE 型号、固件和两端接线拓扑尚未确认，所以本轮证明双机有线实流量执行链，不作为完整 CPE 性能认证。

Windows Rust 1.99.0/MSVC，Mac Rust 1.96.0；iperf3 分别为 3.22/3.18。独立跨平台测试配置使用 1m socket window，持续 20 秒，背景与起流稳定各 3 秒；没有改动 Windows 内置 256m 预设。未覆盖双 Windows ctsTraffic、默认预设和长稳压力测试。

## 首轮实机结果

run_20261005_113358_14092：10 个单元全部执行完成，Ping 两项 PASS，吞吐八项 MEASURED，没有 SETUP_ERROR/NOT_EVALUATED 或最终清理错误。

|场景|Windows→Mac RX Mbps|Mac→Windows RX Mbps|
|---|---:|---:|
|TCP 单向|982.688|975.047|
|TCP 双向|768.117|771.403|
|UDP 两流单向（每流 300 Mbps）|618.070|617.947|
|UDP 两流双向（每流 300 Mbps）|618.174|617.650|
|UDP 单流（300 Mbps）|308.737|308.580|

数值为接收端网卡计数器口径，含链路开销，与工具应用负载口径不同。

另一次 run_20261005_114022_8064：门限 500 Mbps 实测 983.152 → PASS；门限 1100 Mbps 实测 983.906 → RATE_FAIL。1100 超过 1 Gbps 链路能力，是有意注入的不达标分支，退出码 1 符合预期。

## 发现与修复

双向 TCP 同网卡的两次独立监控使用同名 CSV，后保存者覆盖前者。首轮第三单元 Windows→Mac 的报告 RX 为 768.117319670 Mbps，保存的 CSV 只能复算出 762.886457444 Mbps，相差 5.230862226 Mbps。其他九条有效 RX 记录复算通过。

修复后的附件身份包含端点和完整 CSV 内容摘要（包括零点偏移），独立快照分别保存，相同快照复用。文件系统回归测试先在旧实现失败，再在修复后通过，同时检查不同内容、不同零点偏移及原文件保留。另修复 Rust 1.99 严格 Clippy 的冗余导入和像素循环提示。

## 检查与复测

本地四项仓库检查通过：fmt、完整测试（944 通过/1 忽略）、Clippy、Windows MSVC 目标 Clippy。Windows 原生完整测试 927 通过/1 忽略，fmt 和两项严格 Clippy 通过。

修复后 run_20261005_115248_3224：双向 TCP 和双向 UDP 两个单元均 MEASURED，退出码 0，无起流失败、NOT_EVALUATED 或最终清理错误。TCP 两腿 RX 为 720.681 / 821.823 Mbps；UDP 两腿为 613.304 / 618.593 Mbps。吞吐波动不改变本次验证目标：附件与测量结果可独立复核。

四条方向记录从保存 CSV 复算 RX 均值与有效覆盖率全部通过；原始字节差复算最大相对误差 0.058545%，低于 0.2% 容差。修复后 Windows release SHA256：`9a12dc69c9d539e0f550fbe29a0200d8a5e91b1a9dbac3e9b750ad9c529b9573`。Windows 五项检查（四项仓库检查和 release 构建）全部通过。

Mac 辅测服务已正常停止（servers/clients/monitors/errors 全为 0），Windows 本次临时防火墙规则与计划任务已移除，无遗留 iperf3 进程或测试端口监听。已启用并验证密钥登录。

## 证据

本目录保存原始 HTML、Excel、JSONL、工具日志和 NIC CSV；修复前失败日志 regression-before.log；修复文件哈希 fix-manifest.json，原始快照 source-manifest.json 位于父目录。independent-rx-check.json 保存独立复算明细。

复算按报告正式窗口裁剪 CSV 样本、去除重叠、扣除已保存背景值并按有效时长加权；CSV 的六位小数 Mbps 与报告绝对误差容差为 0.00001 Mbps，覆盖率容差 1e-10。原始字节差复算使用 CSV 毫秒整数周期，允许其舍入引入的 0.2% 相对误差。

完整实机证据目录：`<验收证据目录>/real-machine`；Windows 同步记录位于 `C:\CPE-Acceptance\real-machine-fixed`。


后续重点检查 Windows 统计和功能，发现并修复 CTS 事件说明行误报错误与 TCP server 速率列错误；详见 [Windows 统计与功能重点验收](windows-statistics-functions-2026-10-05.md)。
