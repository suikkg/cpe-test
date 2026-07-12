# CPE 测试工具 Windows 包

将整个文件夹完整复制到**主控机**和**辅测机**。两台电脑都应具备：

- `cpe_test.exe`
- 本目录的 `start_*.bat` 和 `configs` 文件夹

需要灌包时，两台电脑还必须具备 `iperf3.exe`，以及该 iperf3 Windows 发行包中
与它同目录的所有 DLL（常见为 `cygwin1.dll`）。纯 Ping 不需要 iperf3。

本包未内置 iperf3：它是独立第三方二进制，且不同 Windows 发行版依赖的 DLL 不同。请将和测试电脑架构一致的完整 iperf3 发行包内容复制到本目录；只测 ping 时可不放 iperf3。

## 使用

1. 辅测机双击 `start_agent.bat`，首次防火墙提示请选择“允许访问”。保持窗口打开，记下显示的可达 IP。
2. 主控机双击 `start_master_select_config.bat`，选择对应网口配置，再输入辅测机 IP。
3. 测试结束后，HTML 报告和日志保存在主控机当前目录。

运行时 TCP 和 UDP 都会打印 `[灌包进度]`：`nic-rx` 是 Windows `GetIfTable2`
网卡计数器实测接收速率，`iperf` 是 iperf3 自报诊断值。测试完成后，
`iperf_outputs` 中的 `iperf_raw_*.log` 保存 client/server/事件和全部重试原文，
`nic_samples_*.csv` 保存逐样本 RX/TX 计数；HTML 报告底部可直接打开这些附件。

常规子网 Ping 默认覆盖 32、1600、65500 字节负载。若本轮所有 iperf 都没有
产生有效速率测量（包括缺 iperf3、旧 agent 被安全拦截、server 全部创建失败或
client 全部未起流），主控会继续/追加 Ping 诊断：每个唯一子网方向只发 3 个
32 字节短 Ping，并绑定每块涉及网卡的源 IP Ping 该接口自己的 IPv4 网关。
扫描输出会显示 `gw:`。
无网关时报告 `GATEWAY_NOT_FOUND`，不会伪装成 100% 丢包；Ping 命令或 HTTP 执行
错误归为 `SETUP_ERROR`。当前版本不抓 PCAP。

也可直接运行 `start_master.bat configs\config-sgmii.json`，把文件名替换为 `config-wifi5g.json`、`config-10gusb.json` 或 `config-all-common.json`。

## 配置说明

JSON 标准不支持注释，不能在一个 JSON 内可靠地“取消注释”来选择网口。这里使用多份具名配置：

- `config-sgmii.json`：SGMII 1G/2.5G
- `config-wifi5g.json`：Wi-Fi 5G
- `config-10gusb.json`：主控 10GUSB、辅测 10GETH
- `config-all-common.json`：以上常用网口组合

如需更改网段、时长、流数或速率，复制一份最接近的文件并修改；JSON 内不要加入 `//`、`#` 或 `/* ... */` 注释。

## 常见问题

- 提示未找到 iperf3：两台机器都补齐 `iperf3.exe` 和它同发行包的 DLL。
- 缺 iperf3 时：iperf 单元会明确失败，但已选 Ping 和自动子网/网关诊断仍会执行。
- 灌包端口：Windows 防火墙需允许 Agent 的 28801 和 iperf 动态测试端口（通常从 56000 起）。
- 网卡找错：先在命令行运行 `cpe_test.exe scan`，确认角色识别；必要时以 `tests` 的 `master:NAME=接口名` / `agent:NAME=接口名` 精确指定。
