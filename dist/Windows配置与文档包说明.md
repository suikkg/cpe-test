# cpe_test v4.2.1 Windows 配置与文档包

仓库中的 `cpe_test-v4.2.1-windows-config-docs.zip` 是便于从 Git 直接下载的
Windows 配置、说明文档和启动脚本资料包。包内文件由仓库当前版本生成，并由 CI
逐文件与源码副本比对，避免配置或文档过期。

这个资料包**不包含可执行程序或吞吐工具**：

- 不包含 `cpe_test.exe`；请从 GitHub Release 下载正式
  `cpe_test-v4.2.1-windows-x86_64.zip`，或自行编译。
- 不包含 `ctsTraffic.exe`；正式 Windows Release ZIP 会捆绑固定并校验过的
  Microsoft ctsTraffic 2.0.4.0 x64。
- 不包含 `iperf3.exe` 及其 DLL；需要 iperf3 测试时，请放入完整的 Windows
  iperf3 发行包。

## 包内内容

- Windows 快速开始、完整 README、使用说明、NIC 说明和 UDP 验收场景。
- `config.example.json` 与 `configs/` 下四份可直接选择的具名配置。
- `start_agent.bat`、`start_master.bat`、`start_master_select_config.bat`。
- iperf3/ctsTraffic 放置说明、MIT 许可证和第三方声明。

## v4.2.1 CTS 行为要点

- CTS 门禁会检查主控和 agent 的真实 Windows 版本，仅 Windows major 版本不低于 10 时启用；Windows 7/8/8.1、版本无法确认以及 macOS/Linux 会 fail-closed，不阻断 iperf3/Ping。
- 配置文件中的非法 CTS TCP/UDP `window`、UDP `bandwidth`/`length`、`streams`、`duration` 都会记录 `SETUP_ERROR / CTSTRAFFIC_ARGS_INVALID`，不启动 CTS 进程；交互式越界输入会要求重输。`duration=0` 不代表无限，也不会被静默修正；无限测试需手工运行 ctsTraffic 原生命令并手工停止。
- 已有 CTS 工具测量后，server 在显式停止前异常退出或超时会记录 `RATE_FAIL / CTSTRAFFIC_RUNTIME_ERRORS`。UDP 单向 1 流或双向每方向 1 流仍按方向独立执行 `max(flow_retries + 1, 3)` 次尝试，安全耗尽仍无工具证据时直接硬失败。
- CI 和本地测试不能代替现场网卡、驱动、防火墙与 CPE 环境；发布后仍建议使用两台真实 Windows 10+ 设备完成 CTS TCP/UDP 双机数据流验收。

使用时，优先下载正式 Windows Release ZIP；如果只需要更新配置、文档或启动脚本，
可以单独下载本资料包并按需覆盖对应文件。主控和辅测机的 `cpe_test.exe` 必须来自同一
Release。
