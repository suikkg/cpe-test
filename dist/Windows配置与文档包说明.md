# cpe_test v4.2.2 Windows 配置与文档包

仓库中的 `cpe_test-v4.2.2-windows-config-docs.zip` 是便于从 Git 直接下载的
Windows 配置、说明文档和启动脚本资料包。包内文件由仓库当前版本生成，并由 CI
逐文件与源码副本比对，避免配置或文档过期。

这个资料包**不包含可执行程序或吞吐工具**：

- 不包含 `cpe_test.exe`；请从 GitHub Release 下载正式
  `cpe_test-v4.2.2-windows-x86_64.zip`，或自行编译。
- 不包含 `ctsTraffic.exe`；正式 Windows Release ZIP 会捆绑固定并校验过的
  Microsoft ctsTraffic 2.0.4.0 x64。
- 不包含 `iperf3.exe` 及其 DLL；需要 iperf3 测试时，请放入完整的 Windows
  iperf3 发行包。

## 包内内容

- Windows 快速开始、完整 README、使用说明、NIC 说明和 UDP 验收场景。
- `config.example.json` 与 `configs/` 下四份可直接选择的具名配置。
- `start_agent.bat`、`start_master.bat`、`start_master_select_config.bat`。
- iperf3/ctsTraffic 放置说明、MIT 许可证和第三方声明。

## v4.2.2 行为要点

- TCP 与 UDP 并发流数可分别通过 `tcp_streams`、`udp_streams` 控制；缺省或为 `0` 时分别回退旧字段 `streams`，交互菜单按已选协议分别询问。
- `2.8G` 与 `2.8Gbps` 均严格规范化为 `2800000000 bit/s`；非法尾随内容不再交给 iperf3/CTS 宽松解释。`14k` 按 1024 进制等于 14336 字节，超过常见 MTU 时会产生 IP 分片。
- CTS 接收端 NIC 速率只统计真实事件证明的数据窗口，不再把启动、握手、轮询和清理空窗计入平均值，也不再用全生命周期均值回退。有效窗口证据不足时返回 `NOT_EVALUATED / CTSTRAFFIC_EFFECTIVE_WINDOW_SHORT`，避免误报低速或 `RATE_FAIL`。
- monitor 启动、停止、无样本和窗口内采样错误均有明确 `CTSTRAFFIC_MONITOR_*` 原因码；CTS resume 统计语义升级后不会复用旧版缓存结果。
- CI 和本地测试不能代替真实网卡、驱动、防火墙与 CPE 环境；发布后仍建议使用两台真实 Windows 10+ 设备完成 CTS TCP/UDP 双机数据流验收。

使用时，优先下载正式 Windows Release ZIP；如果只需要更新配置、文档或启动脚本，
可以单独下载本资料包并按需覆盖对应文件。主控和辅测机的 `cpe_test.exe` 必须来自同一
Release。
