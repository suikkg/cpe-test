# Windows 统计与功能重点验收（2026-10-05）

验收对象为 v6.5.2、`995b19d` 的未提交工作区快照，包含此前 NIC CSV 覆盖修复及本轮 CTS 事件解析修复；不能只用 HEAD 表示本次二进制。未提交或推送。最终源码清单和 exe SHA256 见证据目录。

## 范围与环境

Windows 11 Pro x64、192.168.8.103、以太网协商 1 Gbps；真实辅测为 Mac 192.168.8.102/en0。Windows Rust 1.99.0/MSVC、iperf3 3.22、ctsTraffic 2.0.4.0，浏览器为 Windows Playwright Chromium。

有线吞吐测试使用独立跨平台配置（1m socket window），未修改 Windows 内置预设。CTS 在同一台 Windows 上以 127.0.0.1 客户端和服务端运行，能验证真实工具、异步 API 和统计事件；不能据此证明 CPE 转发性能、两台 Windows 的默认预设或线路接收端 NIC 统计。

## 统计核对

独立 Python 审计不调用产品计算函数：从 NIC CSV 原始序列、正式窗口、背景边界和事件记录复算；直接读取 XLSX XML 数字单元格对账。统计脚本及全部逐项断言保存于证据目录。

|统计项|核对方式与结果|
|---|---|
|RX/TX 均值|按正式窗口裁剪、去重、背景扣除、有效时长加权；与 JSON 数值一致|
|RX-P10/TX-P10|从完整五秒滚动窗口复算分位数，与保存结果一致|
|RX 中位数/P95/最小/最大|从有效采样序列复算，全部一致|
|有效窗口与覆盖率|正式窗口、有效秒、要求秒、采样覆盖率、滚动覆盖率均对账|
|原始字节增量|相邻累计计数的差与保存的 delta 一致；监控生命周期 RX/TX 字节总量守恒|
|Windows 计数器交叉验证|`Get-NetAdapterStatistics` 起止快照包围监控原始计数；实际累计计数超过 4 GiB，未发生 32 位截断|
|监控 Mbps 单位|生命周期字节数 ×8/秒/1,000,000 与 API 数值一致|
|双向合计|Excel 的 AB 接收端 RX + BA 接收端 RX 与两腿实测一致|
|Ping|Windows 原始 Sent/Received/Lost、RTT min/avg/max 与 JSON 一致；DF 命令实际包含 `-f`|
|CTS TCP/UDP 单位|TCP 原始 bytes/s ×8/1,000,000；UDP 原始 bits/s /1,000,000，与事件数值一致|
|CTS UDP 帧与字节|10 Mbps ×5 秒 =6,250,000 字节；100 fps ×5 秒 =500 帧；丢失/重复/错误帧均为 0|
|Excel/HTML/进度页|Excel 数字精度与 JSON 一致、HTML 三位小数一致、进度页 RX 与判定和报告共源一致|
|缺失值与诊断|有线吞吐无目标为 MEASURED；500 Mbps 达标为 PASS；此前 1100 Mbps 不达标为 RATE_FAIL；负载 Ping 100% 超时仍只作诊断|

首轮重点审计包含 451 项采样/Excel 对账和 39 项功能统计断言；最终复测计数见 `statistics-audit.json` 与 `functional-statistics-audit.json`。这些是具体断言数，不代表已展开整个稳定回归方案的全部矩阵。

新增 Windows 接收端实测：run_20261005_120154_19740 为 MEASURED，RX 971.334 Mbps、RX-P10 982.963 Mbps、采样覆盖率 99.955%、有效窗口 20 秒。500 Mbps 验证运行 run_20261005_120918_16300 为 PASS；之后 run_20261005_120946_19532 RESUME 跳过 1 单元，未再次起流。

## 实际功能检查

真实服务端和 Windows 浏览器使用产品 exe，不替换网络 API 响应。鉴权、扫描、连接、监控和运行请求经过真实 HTTP 层；运行控制请求使用兼容的配对 DTO，并非对现代套件界面全部编辑流程的替代。

|功能|结果|
|---|---|
|本机扫描、中文网卡名、1 Gbps 与 IPv4|通过，与实机身份一致|
|无 token、错误 agent token、cookie 单独请求 API|均正确拒绝|
|浏览器首次打开、抹去 token、F5 刷新|页面正常、无运行期 JS 错误|
|双端连接、配置导出导入、计划生成|通过，保存真实配置与计划 JSON|
|实时监控 start/samples/stop|Windows 500ms 会话正常采样、停止|
|重复 run|运行中请求第二轮被拒绝|
|过期 skip、当前 skip|旧 run_id 被拒绝；当前单元可跳过，确实进入下一单元|
|stop|下一单元停止，保留部分结果及报告；实际资源完成清理|
|手动跳过的判定口径|保持测量判定并附操作员跳过诊断；中断起流的 SETUP_ERROR 不改写为 SKIP，这是当前 ADR-17 契约|
|Windows 截图|真实 GDI PNG 保存成功，签名及尺寸有效|
|owner 清理|监控被回收，status 不可再访问；已关闭 owner 拒绝迟到的资源启动|
|不存在的网卡|明确拒绝，不静默返回假零速率|
|DF Ping|真实 Windows→Mac 5 发5 收，0 丢包，RTT min/avg/max 均为1ms|
|CTS TCP/UDP 作业|真实启动、幂等请求 ID、产生流量事件、同步停止确认、owner 清理通过|
|RESUME|仅已有正式 PASS 命中，本轮0秒完成、跳过1项、无灌包进度|
|报告重放与对比|对隔离副本重放成功；同数据对比返回0，无回归|
|离线报告|Windows Chromium offline 模式从文件打开成功，无 JS 错误，附件相对链接均存在|

脚本初期的请求头、方向枚举、可选空字段和相对路径错误已修正；未把这些测试脚本错误登记为产品缺陷。最终通过记录与中间记录分开保留。

## 本轮发现和修复

1. CTS TCP 说明行 `* Network Errors ... failed IO patterns ...` / `* Data Errors ...` 被误记成 Error 事件。只过滤这两类明确说明行，真实故障（含星号开头的故障）仍生成 Error。
2. CTS TCP server 的 Traffic 事件误读 SendBps；真实 Push 模式服务端承载 RecvBps，导致没有正接收速率事件。现按 client/server 分别取发送/接收列，并统一换算 Mbps。

修复位于 `src/cmd/ctstraffic.rs`，未改速率判定优先级或门限算法。回归测试 `real_cts_legends_and_receiver_status_preserve_event_meaning` 先在旧实现失败，再验证说明行、TCP 两端列和真实故障均正确。原始 Windows CTS JSON 保存在 `cts-before-fix/`。

## 最终检查与证据

最终统计审计：451 项采样/Excel 对账与105 项功能统计断言，共556 项全部通过；真实功能检查按名称去重为43 项，全部通过。

最终代码本地四项检查全部通过（完整测试945 通过/1 忽略）；Windows 原生 fmt、完整测试（928 通过/1 忽略）、两项严格 Clippy 和 release 构建全部通过。

修复后 CTS TCP 客户端说明行没有 Error 事件，服务端19 条正接收速率事件均按原始 RecvBps 独立复算通过；UDP 字节数与500 帧结果再次确认。TCP 回环复测使用5 秒、64 KiB 缓冲和100ms 状态输出，UDP 使用5 秒、1 MiB 缓冲和1000ms 状态输出。

本机短 CTS 测试观察到管道输出缓冲：2 秒/1000ms 状态频率时服务端只有 Started 事件；提高状态输出频率后能观察到接收事件。短测脚本的20 秒等待也短于产品预留的30 秒清理余量，最终按产品有界期限等待。不能据此声称默认 CTS 时序/跨机正式窗口已经验收；原始短测记录保留在 `cts-short-buffered-server.json` 与 `cts-fixed-harness-timeout.json`。

最终 Windows exe SHA256：`1b5ac090f16803ee492e0259831b8c731cd0230db25f419c014756212dddc154`。265 个原有受跟踪源文件在本地与 Windows 编译目录逐项 SHA256 比对，无差异；新验收文档独立保存。

本次 Mac 辅测正常退出，Windows 测试服务、临时防火墙规则、计划任务均已清理，无遗留 iperf3/ctsTraffic 进程或测试端口监听。

完整证据目录：`<验收证据目录>/windows-focused`；Windows 侧：`C:\CPE-Acceptance\windows-focused`。原始配置、计划、progress、NIC CSV、JSONL、HTML/XLSX、PNG、API/CTS 输出、源码清单、完整检查日志和审计脚本均保留。测试 token 仅用于本轮隔离验收。

尚未覆盖：Windows 10、双 Windows 穿 CPE 的 CTS 流量与默认预设、IPv6 真流量、Wi-Fi/RNDIS/10GUSB 流量矩阵、驱动计数器真实复位、物理拔线恢复、长稳压力。CPE 型号/固件与接线拓扑仍未确认；这些不能标为通过。

## 后续单机双网口实测

Windows接入Wi-Fi后的真实有线↔Wi-Fi矩阵、物理网卡抓包证据及CTS有效窗口缺口，见 [Windows单机有线与Wi-Fi实机验收](windows-same-host-wifi-2026-10-05.md)。该轮并非全部通过，不能将此前回环工具检查扩展成正式CTS速率验收通过。
