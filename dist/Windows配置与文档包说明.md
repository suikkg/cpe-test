# cpe_test v6.7.0 Windows 配置与文档包

仓库中的 `cpe_test-v6.7.0-windows-config-docs.zip` 是便于从 Git 直接下载的
Windows 配置、说明文档和启动脚本资料包。包内文件由仓库当前版本生成，并由 CI
逐文件与源码副本比对，避免配置或文档过期。

这个资料包**不包含可执行程序或吞吐工具**：

- 不包含 `cpe_test.exe`；请从 GitHub Release 下载正式
  `cpe_test-v6.7.0-windows-x86_64.zip`，或自行编译。
- 不包含 `ctsTraffic.exe`；正式 Windows Release ZIP 会捆绑固定并校验过的
  Microsoft ctsTraffic 2.0.4.0 x64。
- 不包含 `iperf3.exe` 及其 DLL；需要 iperf3 测试时，请放入完整的 Windows
  iperf3 发行包。

## 包内内容

- Windows 快速开始、完整 README、使用说明、NIC 说明和 UDP 验收场景。
- `config.minimal.json`（最小可用：首次跑通只需改 `agent_host`、`agent_token`、
  `iperf.duration` 三项，其余走内置默认值）。
- `config.example.json`（完整字段面）与 `configs/` 下五份可直接选择的具名配置，
  其中 `config-full-tcp-udp-ping.json` 是 TCP+UDP+PING 全量预设（约 210 个单元）。
- `projects/cpe-ui-project-full.json`：图形控制台的「导入测试项目」文件（`project_version: 3`），
  内含全网口配对的链路集合、TCP/UDP/PING 参数、全局判定设置、Wi-Fi 互测门限、按网口策略和
  TCP→UDP→PING 套件；不包含 agent 地址、口令、RESUME、截图开关等本地运行态。
  v3 存的是**有效值快照**：界面上留空、由主控默认值兜底的格子在导出时已经换算成具体数字，
  换一台主控导入不会改判定口径。Wi-Fi 门限按实际频段组合保存两个单向门限和一个双向 RX 合计门限。
- `start_ui.bat`（图形控制台）、`start_agent.bat`、`start_master.bat`、`start_master_select_config.bat`。
- iperf3/ctsTraffic 放置说明、MIT 许可证和第三方声明。

## v6.7.0 行为要点

内环支持多档参数和每网口 TCP / UDP 门限，结果按网口展示接收速率与门限。默认采集参与电脑的测试结束截图，HTML 报告内嵌图片；截图失败不影响判定。内环项目 version 4 兼容旧项目，旧单值参数保持 RESUME 身份。子网、内环和报告文案已精简。Windows 包继续捆绑 ctsTraffic 2.0.4.0，iperf3 与内环所需 adb 由用户准备。
