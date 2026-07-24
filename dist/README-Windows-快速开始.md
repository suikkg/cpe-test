# CPE 测试工具 Windows 包

> 如果你从 Git 仓库下载的是 `cpe_test-v4.2.2-windows-config-docs.zip`，那是一份只含
> 配置、文档和启动脚本的资料包，不含 `cpe_test.exe`、`ctsTraffic.exe` 或 iperf3。
> 开箱即用请下载 GitHub Release 的 `cpe_test-v4.2.2-windows-x86_64.zip`；也可以自行
> 编译程序后，把资料包内容与 exe 放到同一目录。

将这个文件夹完整复制到**主控机**和**辅测机**。两台电脑必须使用同一个
`cpe_test` Release；不要只替换其中一台的 exe。

## 包内文件与系统要求

- `cpe_test.exe`：主控、agent、网卡扫描和监控共用的程序。
- `ctsTraffic.exe`：Microsoft ctsTraffic 2.0.4.0 x64，随官方 v4.2.2 Windows 包固定捆绑并校验；仅支持 Windows 10 或更高版本。
- `start_*.bat`：双击启动脚本。
- `configs\`：SGMII、Wi-Fi、10GUSB 等具名配置。
- `THIRD_PARTY_NOTICES.md` 及 CTS/WIL 许可文件：第三方归属和许可说明。

iperf3 未内置：不同 Windows 发行版依赖的 DLL 不同。需要 iperf3 测试时，把和电脑
架构一致的完整 iperf3 发行包内容复制到本目录，包括 `iperf3.exe` 和同包 DLL
（常见为 `cygwin1.dll`）。只测 Ping 或 ctsTraffic 时不需要 iperf3；只测 Ping 时也不需要 CTS。

ctsTraffic 测试要求主控和辅测都是 Windows 10+，且两边都能在 `cpe_test.exe` 同目录
或 `PATH` 中找到 `ctsTraffic.exe`。程序会实际检查两端 Windows major 版本，只有不低于 10 才通过；
Windows 7/8/8.1、版本无法确认以及 macOS/Linux 都不支持 CTS，但不会阻断已选的 iperf3/Ping。

## 使用

1. 辅测机双击 `start_agent.bat`，首次防火墙提示请选择“允许访问”。保持窗口打开，记下显示的可达 IP。
2. 主控机双击 `start_master_select_config.bat`，选择对应网口配置，再输入辅测机 IP。
3. 交互菜单中选择 iperf3、ctsTraffic、两者对比、Ping 或全部；确认任务清单后开始。
4. 测试结束后，HTML 报告、主控日志和 `iperf_outputs` 附件目录保存在主控机当前目录。

也可直接运行 `start_master.bat configs\config-sgmii.json`，把文件名替换为
`config-wifi5g.json`、`config-10gusb.json` 或 `config-all-common.json`。

运行时 TCP/UDP 会打印 `[灌包进度]`：`nic-rx` 是 Windows `GetIfTable2` 网卡计数器
实测接收速率，iperf3/CTS 自报速率不替代正式 NIC 吞吐口径；但单流是否真正建立仍必须
有工具自身的 rate、bytes、frame/datagram 证据。`iperf_outputs` 是为兼容旧版保留的目录名：

- `iperf_raw_*.log`：iperf3 client/server/事件和全部重试原文。
- `ctstraffic_raw_*.log`：CTS 每轮 client/server 命令、输出、解析摘要和生命周期。
- `nic_samples_*.csv`：逐样本 RX/TX 网卡计数。

## 配置说明

JSON 标准不支持注释，不能在一个 JSON 内可靠地“取消注释”来选择网口。这里使用多份具名配置：

- `config-sgmii.json`：SGMII 1G/2.5G。
- `config-wifi5g.json`：Wi-Fi 5G。
- `config-10gusb.json`：主控 10GUSB、辅测 10GETH。
- `config-all-common.json`：以上常用网口组合。

预置配置为了保持升级前的默认测试量，仍使用 `"kinds": ["iperf", "ping"]`。只跑 CTS：

```json
"kinds": ["ctstraffic"]
```

两种后端与 Ping 都跑：

```json
"kinds": ["iperf", "ctstraffic", "ping"]
```

如需更改网段、时长、流数或速率，复制最接近的配置再修改；JSON 内不要加入
`//`、`#` 或 `/* ... */` 注释。

### CTS 参数语义

- 配置中的 `src → dst` 始终表示数据方向。TCP 是 `src` client 发送、`dst` server 接收；UDP MediaStream 是 `src` server 发送、`dst` client 接收。程序自动处理 UDP 角色反转，报告方向不变。
- TCP 使用 `tcp_streams`，UDP 使用 `udp_streams`，缺省或为 `0` 时分别回退旧字段 `streams`。有效流数映射为 `Connections:N`；一个 CTS 进程承载 N 条连接。CTS 自动化要求所选协议的有效流数在 `1..=32`；配置文件超出范围会记录 `SETUP_ERROR / CTSTRAFFIC_ARGS_INVALID`，交互式输入则要求重输，不会静默修正。
- 历史字段 `iperf_duration` 供两个吞吐后端共用。CTS TCP 映射为 `TimeLimit:<毫秒>` 硬截止，CTS UDP 映射为 `StreamLength:<秒>`。CTS 自动化只接受 `1..=86400` 秒；配置文件中的 `duration=0` 不是无限，也不会被静默夹为 1 秒，而是记录 `SETUP_ERROR / CTSTRAFFIC_ARGS_INVALID`；交互式输入会要求重输。无限测试只能手工运行原生命令并手工停止。
- `tcp_windows` 对 CTS 是 Winsock socket buffer；`udp_profiles[].window` 对 iperf3 生成 `-w`，对 CTS 映射为实际发送/接收方向的 `SendBufValue`/`RecvBufValue`。它们不是 TCP 拥塞窗口；非法尺寸会记录 `SETUP_ERROR / CTSTRAFFIC_ARGS_INVALID`。
- UDP `bandwidth` 必须完整匹配十进制数值加可选 `k/m/g` 或 `kbps/mbps/gbps`；`2.8G` 与 `2.8Gbps` 都规范化为 `2800000000 bit/s`。尾随垃圾、科学计数和混合后缀不会再透传后端。CTS UDP 的速率按每条连接生效，总提供速率约为“每流带宽 × `udp_streams`”。
- `length` 的 `k/m/g` 使用 1024 进制；`14k` 等于 14336 字节。iperf3 保留原生 `-l 14k`，CTS 映射为 `DatagramByteSize:14336` 且不得大于 65507 字节。14 KiB 超过常见 1500 字节 MTU，会产生 IP 分片。
- 默认 `ctstraffic.udp_frame_rate=100`、`udp_buffer_depth_secs=1`。这是 MediaStream 模型，不等同于 iperf3 UDP flood；两种后端的数值不应当作完全相同语义的互换结果。
- CTS 接收端 NIC 统计只计算 `Connected`、有效 `Traffic` 与 `Ended` 事件可以证明的数据窗口，不计 monitor RPC、进程启动、握手、状态轮询和清理。不会用 `Total Time`、正常退出或疑似缓冲输出补窗口，也不会在无样本时回退全生命周期平均值。证据不足时返回 `NOT_EVALUATED / CTSTRAFFIC_EFFECTIVE_WINDOW_SHORT`；monitor 启动、停止、无样本和窗口内采样错误分别使用 `CTSTRAFFIC_MONITOR_*` 原因码。
- CTS 不使用 iperf3 的逐进程多流、5 秒 P10 与分阶段 `discover` 实现，但两者共享下面的 UDP 单流硬门槛和安全清理原则。

UDP socket buffer 示例：

```json
{"bandwidth": "500m", "length": "1400", "window": "1m"}
```

`window` 可使用 `64k`、`1m`、`1.5m` 等；省略时保持旧配置行为。

iperf3 UDP 单流和 CTS UDP `Connections:1` 都是每方向独立的硬连通门槛。单向 1 流，或
双向 AB、BA 每方向各 1 流时，每个方向总尝试数为 `max(flow_retries + 1, 3)`；双向两腿
各自拥有完整预算并行执行。每轮都必须完整启动 server/client，并确认停止和清理成功后
才能复用端口。

判断灌通只认所选工具自身的 rate、bytes、frame/datagram 等输出；NIC 背景流量不能补成
成功，NIC 只验证已建立流的目标吞吐。全部安全尝试仍无工具测量时，iperf3 记录
`RATE_FAIL / SINGLE_UDP_STREAM_FAILED`，CTS 记录
`RATE_FAIL / CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED`，不会降级为“活跃流不足/未评估”。
平台不支持、缺工具、非法参数、尚无测量时的 server/client 生命周期、显式取消或清理未确认仍报告
`SETUP_ERROR`。一旦已有 CTS 工具测量，server 在显式停止前异常退出或超时会记录
`RATE_FAIL / CTSTRAFFIC_RUNTIME_ERRORS`，并按真实运行错误、UDP 丢包/丢帧和目标速率判定，不会靠继续重试掩盖结果。

## 前置检查和故障诊断

主控会分别检查两个吞吐后端。CTS 会确认主控真实 Windows major 版本不低于 10，且 agent 只在自身同样通过真实版本检查时才声明 `ctstraffic_v1`
能力，然后检查两端二进制；版本无法确认时 fail-closed。任一项不满足时，仅 CTS 单元记录
`SETUP_ERROR / CTSTRAFFIC_PREFLIGHT_FAILED`。程序不会静默改跑 iperf3，已选的 iperf3/Ping 仍继续。

本地单元测试和 CI 可以验证参数、生命周期、报告与发布包，但不能代替真实网卡、驱动、防火墙和 CPE 环境。发布后仍建议在两台真实 Windows 10+ 设备上完成 CTS TCP/UDP 双机数据流验收。

常规子网 Ping 默认覆盖 32、1600、65500 字节负载。若本轮所有 iperf3/CTS 单元都没有
有效速率测量，主控会继续或追加短 Ping 和本机网关诊断。无网关时报告
`GATEWAY_NOT_FOUND`，不会伪装成 100% 丢包；当前版本不抓 PCAP。

## 常见问题

- 未找到 iperf3：两台机器都补齐 `iperf3.exe` 和它同发行包的 DLL。
- CTS 被阻断：确认两台电脑都是 Windows 10+、两边使用同一个 `cpe_test` Release，且同目录都有 `ctsTraffic.exe`。
- 防火墙：允许 Agent 的 28801 端口和动态测试端口（通常从 56000 起），并允许所选吞吐工具联网。
- 网卡找错：运行 `cpe_test.exe scan`；必要时用 `master:NAME=接口名` / `agent:NAME=接口名` 精确指定。
