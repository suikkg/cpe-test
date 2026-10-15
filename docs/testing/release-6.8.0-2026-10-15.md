# v6.8.0 发布验收（2026-10-15）

## 交付范围

核对 Claude 最后两项交付：未连接辅测机时本机网卡按 IPv4 前缀筛选，`/api/local` 不再清空服务端前缀；本机计划执行时全扫，避免前缀外已选网口在执行重扫时消失。窄屏导航保留子网测试、内环测试和工具分组名。对应 Rust、纯函数与浏览器回归均存在并通过。

本版本同时收录此前尚未提交的内环流程对齐：连接/计划/执行、链路集合与套件分配、Ping、整计划轮次、定向跳过、启动确认、临时覆盖及诊断、Excel/整轮包/两轮对比，以及本机两口快捷测试。保留旧配置与 IPv4 RESUME 身份。判定共用入口和离线内联页面约束由完整测试守护。

## 软件门禁

在本地 macOS 当前源码执行：

- `cargo fmt --check`。
- `cargo test --locked`：1027 通过，1 项浏览器服务器夹具按设计忽略；该夹具由浏览器回归另行启动。
- `cargo clippy --locked --all-targets -- -D warnings`。
- `cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc -- -D warnings`。
- `npm ci && npm run test && npm run build && npm run verify`：570 项前端测试通过，分层与内联产物检查通过。
- `npm run test:e2e`：29 项 Chromium 回归通过，包括前缀筛选、清空后重扫、本机快捷测试和宽/窄屏导航分组。
- Windows 配置文档包重建，核对 SHA-256、ZIP CRC、完整条目集合及每项源码字节一致。

版本更新后重新构建内联页面并重跑 Rust 四件事与前端门禁；发布流水线另外执行 Linux 质量门、Windows 原生测试/Clippy 和三平台构建。最终流水线及下载附件结果以 v6.8.0 tag 对应的 GitHub Actions 和 Release 为准。

## 实机证据复核

独立读取 `target/validation/windows-real-20261014/` 的原始状态及 ZIP：完整内环两轮共 6 单元（TCP/UDP 为 MEASURED，Ping 为 PASS），跳过场景第 1 项为 SKIP，停止场景仅保留已完成项并记录用户取消。对比跳过 1、缺失 3、回归 0、无法唯一匹配 0。子网包和内环包各 12 项，Excel 15 个 ZIP 项，CRC 均正常；清理记录中的主控进程、iperf3、临时防火墙规则及控制台监听端口均为空。

这批证据来自 10 月 14 日 Windows 实机运行，详见 [原始验收记录](windows-real-acceptance-2026-10-14.md)。本轮没有重跑实机矩阵；最新前缀与窄屏修复由当前源码的真实 HTTP 层 Chromium 回归覆盖。

实机未覆盖桌面截图、IPv6、RESUME、带门限 PASS/RATE_FAIL、辅测机、快捷 UDP、直接 Windows USB 及长稳矩阵。这些范围不以单元测试或浏览器请求桩冒充实流量验收。

## 已知行为

未连接辅测机时编辑前缀只改变当前页面筛选，刷新恢复服务端前缀（默认 `192.168.`）；连接辅测机后使用该次连接保存的前缀。本机完整扫描仍用于项目导入时检查网口是否存在，避免把仅被筛选隐藏的网口当成丢失。
