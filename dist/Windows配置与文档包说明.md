# cpe_test v4.2.0 Windows 配置与文档包

仓库中的 `cpe_test-v4.2.0-windows-config-docs.zip` 是便于从 Git 直接下载的
Windows 配置、说明文档和启动脚本资料包。包内文件由仓库当前版本生成，并由 CI
逐文件与源码副本比对，避免配置或文档过期。

这个资料包**不包含可执行程序或吞吐工具**：

- 不包含 `cpe_test.exe`；请从 GitHub Release 下载正式
  `cpe_test-v4.2.0-windows-x86_64.zip`，或自行编译。
- 不包含 `ctsTraffic.exe`；正式 Windows Release ZIP 会捆绑固定并校验过的
  Microsoft ctsTraffic 2.0.4.0 x64。
- 不包含 `iperf3.exe` 及其 DLL；需要 iperf3 测试时，请放入完整的 Windows
  iperf3 发行包。

## 包内内容

- Windows 快速开始、完整 README、使用说明、NIC 说明和 UDP 验收场景。
- `config.example.json` 与 `configs/` 下四份可直接选择的具名配置。
- `start_agent.bat`、`start_master.bat`、`start_master_select_config.bat`。
- iperf3/ctsTraffic 放置说明、MIT 许可证和第三方声明。

使用时，优先下载正式 Windows Release ZIP；如果只需要更新配置、文档或启动脚本，
可以单独下载本资料包并按需覆盖对应文件。主控和辅测机的 `cpe_test.exe` 必须来自同一
Release。
