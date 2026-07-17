# UDP 单流与并发灌包重构验收场景

本文用于验收 iperf3 与 ctsTraffic 的 UDP 单流、多流和双向灌包重构。验收重点不是“命令能结束”，而是确认工具流确实建立、单流硬门槛和失败重试符合规则、网卡 RX/TX 使用正确窗口统计，以及环境异常不会被误判成 CPE 性能失败。

## 一、验收前准备

两台测试电脑均准备：

- 当前版本 `cpe_test`。
- 兼容现网的 iperf3；Windows 旧版 3.1.x 也在支持范围内。
- 验收 CTS 时，两台电脑均为 Windows 10+，并准备发布包固定版本的 `ctsTraffic.exe`。
- 放通管理端口 28801 和测试端口 56000 起的端口段。
- 主控与 agent 的系统时间不要求严格同步；时间线使用各自单调时钟和主控 offset。
- 测试时保留 `master_日期时间.log`、HTML 报告和 `iperf_outputs/`。

建议基础配置：

```json
{
  "iperf": {
    "duration": 180,
    "udp_profiles": [{"bandwidth": "500m"}],
    "rate_check": {
      "mode": "auto",
      "sample_interval_ms": 1000,
      "background_secs": 3,
      "startup_timeout_secs": 15,
      "settle_secs": 5,
      "launch_interval_ms": 50,
      "min_concurrent_streams": 2,
      "min_active_ratio": 0.9,
      "offered_headroom_pct": 5,
      "flow_retries": 1,
      "discovery_step_secs": 10,
      "evb_usb_to_eth_target_mbps": 6400,
      "evb_eth_to_usb_target_mbps": 8400,
      "cpe_path_ceiling_mbps": 2500
    }
  }
}
```

`flow_retries` 虽位于历史兼容路径 `iperf.rate_check`，但本节所述 UDP 单流硬门槛同时适用于
iperf3 和 CTS；两种后端每方向总尝试数都按 `max(flow_retries + 1, 3)` 计算。

## 二、结果口径

| 结果 | 验收含义 |
|------|----------|
| `PASS` | 明确目标存在，发送负载、共同窗口、采样覆盖、RX 平均/P10和可选丢包均满足 |
| `RATE_FAIL` | 测试搭建有效，但 RX 低于目标、UDP 丢包/丢帧超限，或单流安全耗尽全部尝试后仍未灌通 |
| `UNSTABLE` | RX 平均达到目标，但 5 秒滚动 P10 低于目标 |
| `MEASURED` | 目标未知，完整测量实际能力，不作合格/不合格承诺 |
| `NOT_EVALUATED` | 流数、offered load、有效窗口或采样覆盖不足，不能评价 CPE 性能 |
| `SETUP_ERROR` | 平台、工具、参数、server/client 启停或状态查询、地址、显式取消、资源清理等环境搭建失败 |

验收时必须同时检查：

- HTML 的测试单元汇总行。
- 每个方向的组合计行。
- 流明细行及“流事件”原始输出。
- 主控日志中的 UDP 灌包进度、各方向 attempt/retry、有效窗口和截图日志。
- `iperf_outputs/iperf_raw_*.log`、`ctstraffic_raw_*.log` 的全部 client/server attempt 原文，以及 `nic_samples_*.csv` 的 OS 网卡逐样本记录。

报告通过率只按测试单元汇总行统计。20 条流明细加 1 个组合测试仍只能算 1 个测试单元，不能算 21 个。

还必须区分两层证据：工具自身的 rate、bytes、frame/datagram 等输出负责证明“本轮流确实
建立”，NIC 计数器负责验证“已建立流的接口实际吞吐是否达到目标”。背景 NIC 流量不能
把没有工具测量的单流尝试变成成功，工具自报速率也不能替代 NIC 作为正式目标口径。

## 三、并发与失败流容错

### U00A：iperf3 单向 1 流前两轮失败、第三轮灌通

步骤：

1. 配置单向 iperf3 UDP、`streams=1`，分别用 `flow_retries=0`、`1` 和大于 2 的值运行测试桩。
2. 让前两轮完整 server/client 生命周期结束但没有 iperf3 rate/bytes/datagram 测量，第三轮返回有效工具测量。

预期：

- 每方向总尝试数严格为 `max(flow_retries + 1, 3)`；`0` 和 `1` 都至少执行 3 次，较大的配置值不得被压回 3 次。
- 每轮使用新的 request ID，完整启动 server/client；上一轮停止、进程 `wait`、输出 reader 和端口清理均确认后才开始下一轮。
- 前两轮原文不会被覆盖，第三轮灌通后停止继续重试；`retry_count=2`。
- 最终判定只使用成功轮的测量和真实错误，不能把前两轮的无测量或错误文本合并后污染第三轮。

### U00B：CTS UDP `Connections:1` 前两轮失败、第三轮灌通

步骤：在 Windows 10+ 双机或确定性测试桩中配置单向 CTS UDP、`streams=1`，让前两轮无
CTS rate/bytes/successful frames，第三轮产生有效 CTS 测量。

预期：

- 总尝试数同样是 `max(flow_retries + 1, 3)`，而不是只执行一次 CTS 进程。
- 每轮完整启动 CTS server/client，并在清理确认后复用端口；各轮使用不同 request ID。
- `ctstraffic_raw_*.log` 按 attempt 保留三轮命令、client/server 输出、事件和清理状态。
- 只解析并使用最终灌通轮做 verdict，`retry_count=2`；前两轮错误不能污染成功轮。

### U00C：单流三轮安全耗尽后硬失败

步骤：分别对 iperf3 UDP 单流和 CTS UDP `Connections:1`，让所有预算内尝试都完整执行、
清理确认，但始终没有所选工具自身的 rate、bytes、frame/datagram 证据。

预期：

- iperf3 返回 `RATE_FAIL / SINGLE_UDP_STREAM_FAILED`。
- CTS 返回 `RATE_FAIL / CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED`。
- 两者都不能降级为 `NOT_EVALUATED / ACTIVE_STREAMS_LOW`，也不能因为“0 流”笼统改成 `SETUP_ERROR`。
- 原因详情列出完整尝试数和最后一轮工具输出；该轮不写入 RESUME PASS。

### U00D：双向每方向 1 流各自独立并行

步骤：使用默认 `flow_retries=1` 配置 bidir，AB 与 BA 都只有 1 流；让 AB 第三轮成功，BA 三轮均无工具测量。

预期：

- AB、BA 各自拥有 `max(flow_retries + 1, 3)` 的完整预算，不能两方向合计只有三次。
- 两腿并行执行，可在时间线上看到 AB/BA attempt 重叠；不能先把 AB 三轮全跑完才开始 BA。
- AB 使用成功轮按真实性能判定；BA 为对应后端的单流硬失败。
- 测试单元汇总不得被另一方向的普通 `NOT_EVALUATED` 掩盖硬失败；每方向 raw log 和 retry count 独立。

### U00E：确定性环境或清理问题仍是 SetupError

分别制造：平台不支持、缺少工具、非法参数、server/client 启动或状态查询失败、显式取消，
以及 server/client stop、进程 `wait` 或输出 reader 清理无法确认。

预期：

- 均返回明确的 `SETUP_ERROR` 原因码，不得伪装为 `SINGLE_UDP_STREAM_FAILED` 或 `CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED`。
- 清理未确认时立即停止该方向后续尝试，不得猜测端口已释放，也不得在同端口启动新 request。
- 已安全完成的 attempt 原文仍保留，单元末尾 owner cleanup 可继续补偿，但不能把未确认状态改写成性能失败。

### U00F：背景 NIC 流量不能证明单流灌通

步骤：在被测接收网卡上制造明显背景 RX，同时让 iperf3/CTS 单流三轮都没有任何工具自身
rate、bytes、frame/datagram 测量。

预期：

- 即使 `nic-rx` 高于最低有效速率或目标，active stream 也不能凭 NIC 被补成 1。
- 安全耗尽后仍分别产生两个后端的单流硬失败原因码。
- NIC 样本完整保留用于排障，但原因详情应明确“背景网卡流量不能证明工具流建立”。

### U00G：已有工具测量后按真实结果判定

步骤：让第一轮已经产生有效工具测量，同时分别制造运行时错误、UDP 丢包/丢帧超限、RX
低于目标三种结果。

预期：

- 不再为了争取更好结果继续单流连通重试。
- 分别保留真实的 runtime error、loss high 或 RX below target verdict/reason。
- 不得把已有测量的真实性能问题改写为“未灌通”，也不得用后续重试掩盖失败。

### U01：双向 5 流/2 流全部成功

目的：确认不对称流数组合可以统一调度，并且小方向按 2 条要求计分。

步骤：

1. 构造一个双向 UDP 单元，AB 请求 5 条 500M，BA 请求 2 条 500M。
2. 确认所有测试端口均可访问。
3. 运行 180 秒有效窗口测试。

预期：

- server 日志先出现 7/7 准备完成，再开始 client。
- 两方向按流序号交错起流，而不是 AB 全起完后才启动 BA。
- 日志最终出现 AB active=5/5、BA active=2/2。
- BA 的“要求流数”为 2；AB 默认要求至少 4。
- 双向共同有效窗口达到 180 秒。
- 未知目标时两个方向均为 `MEASURED`；显式目标满足时为 `PASS`。

### U02：2 流方向首轮失败 1 条，重试成功

目的：回答“2 条流一条通、一条没通是否会重试”。

步骤：

1. 使用 U01 场景。
2. 在 BA 的其中一个测试端口上制造一次短暂连接失败，例如只在最初数秒拦截该端口，随后在 `startup_timeout_secs` 内放通。
3. 保持另一个 BA 端口正常。

预期：

- iperf 瞬态重试或组级重试被触发；组级重试时日志出现 `[UDP流重试]`。
- 只重启失败流对应的 server/client，不重启已经稳定运行的其他 6 条流。
- 报告该流 `retry_count >= 1`，事件中有 `Retry`，随后有有效 `Traffic`。
- 最终 BA 为 2/2，正常取得共同有效窗口并进入速率判定。
- 重试次数有限，不允许无限循环。

### U03：2 流方向永久失败 1 条

目的：确认不会死等，也不会用单流速率误判 CPE。

步骤：

1. 使用 U01 场景。
2. 持续拦截 BA 的一个端口，直到该流所有重试结束。

预期：

- 失败流按配置重试后结束；整个测试不会卡在“等待全部流连接”。
- BA 显示请求/活跃/要求 = `2/1/2`。
- BA 结果为 `NOT_EVALUATED`，原因码 `ACTIVE_STREAMS_LOW`。
- 测试单元汇总也不能是 `PASS` 或 `RATE_FAIL`。
- 原因详情明确写出“仅 1/2 条流成功，正式判定至少需要 2 条”。
- 该轮不写入 RESUME PASS。

### U04：5 流方向永久失败 1 条

目的：确认默认容错比例不是“全部流一个不能少”。

步骤：

1. 单向或双向请求 5 条流。
2. 持续拦截其中一个端口，让最终成功数为 4/5。

预期：

- 默认 `min_active_ratio=0.9` 时要求流数为 4。
- 未知目标且窗口完整时可产出 `MEASURED`。
- 已知目标时还必须满足 offered-load 要求；若 4 条流无法提供 `目标 × 1.05`，应为 `NOT_EVALUATED / CONFIGURED_LOAD_TOO_LOW` 或 `OFFERED_LOAD_LOW`，不能直接 PASS。

### U05：20 流 EVB 8.4G 目标

目的：确认目标反推负载与默认容错共同生效。

步骤：

1. `10GETH → 10GUSB/NCM`，20 条、每条 500M、auto 模式。
2. 依次验证 20/20、18/20、17/20 三种最终成功数。

预期：

- 自动目标为 8400 Mbps。
- `ceil(8400 × 1.05 / 500)=18`，默认 90% 活跃比例也要求 18，因此最终要求为 18。
- 20/20 和 18/20 可继续正式判定；17/20 必须 `NOT_EVALUATED / ACTIVE_STREAMS_LOW`。
- RX 平均和 P10均达到 8400、TX-P10达到 8820 且其他条件满足时才 PASS。

### U06：20 流 EVB 6.4G 目标

步骤：`10GUSB/NCM → 10GETH`，20 条、每条 500M、auto 模式。

预期：

- 自动目标为 6400 Mbps。
- 目标负载理论需要 14 条，但 90% 场景忠实度要求 18 条，因此最终仍要求 18。
- 不能为了“够 6.4G”把只连通 14～17 条的测试当成完整 20 流验收。

### U07：agent 20/32 条真实并发

目的：确认固定 16 worker 不再导致长 client 分批运行。

步骤：

1. 分别配置 20 条和 32 条远端 client 流，持续至少 30 秒。
2. 观察 agent 日志及主控 `[灌包进度][UDP]`。

预期：

- `/iperf/client/start` 请求快速返回 job id，HTTP worker 不等待 `-t` 结束。
- 第 17～32 条流无需等待前 16 条结束。
- 在同一运行区间能看到 active 达到 20 或 32（受配置 quorum 和实际环境限制时，报告应明确实际值）。
- `/iperf/client/status` 可按 cursor 增量返回事件；结束后 `/iperf/client/stop` 清理 job。
- 不出现固定在 16 条的平台期后再启动剩余流的批处理特征。

## 四、速率目标与路径识别

### R01：EVB NCM 4.2G 显示 bug

步骤：网卡描述含 NCM，协商显示 4200 Mbps，对端为 10GETH 10000 Mbps。

预期：

- 网卡识别为 `10GUSB`，不识别成 RNDIS 或 UNKNOWN。
- auto 模式下，NCM→10GETH 目标 6400，反向 10GETH→NCM 目标 8400。
- UDP 流数不因 4200 显示值被错误裁成 8 条 500M 流。

### R02：NCM 正常显示 10G

步骤：NCM 网卡协商 10000 Mbps，与 10GETH 构成 EVB 路径。

预期：行为与 R01 相同，NCM→10GETH 使用 6400，10GETH→NCM 使用 8400 自动目标。

### R03：RNDIS 3.7G

步骤：RNDIS 网卡协商约 3700 Mbps，配置 20 条 500M。

预期：

- 角色保持 `RNDIS`；即使描述同时出现 USB/10G 字样，RNDIS 关键字优先。
- 路径可信负载上限约 2500 Mbps，最多生成 5 条 500M 流。
- 默认 auto 转 observe，结果为 `MEASURED`，不能把 2500 当成自动 PASS 目标。

### R04：SGMII2.5G

步骤：路径任一端为 SGMII2.5G，配置 20 条 500M。

预期：整条路径最多生成 5 条；默认 observe。若产品有正式 2.35G 等目标，必须显式配置 verify 目标。

### R05：SGMII1G

步骤：路径任一端为 SGMII1G，配置 20 条 500M。

预期：整条路径最多生成 2 条；这 2 条要求全部成功。最终 1/2 时为 `NOT_EVALUATED`。

### R06：NCM/10GUSB 经过受限 CPE 子网

步骤：一端 NCM/10GUSB，另一端为 SGMII2.5G、RNDIS 或 WiFi。

预期：

- offered load 按整条路径最低瓶颈裁剪，而不是只看 NCM 的 10G 能力。
- 不自动套用 EVB 8400/6400 目标。
- 默认结果为 `MEASURED`；显式 verify 才进行产品门槛判断。

### R07：WiFi 路径

步骤：分别使用 866M、2402M、4804M 协商速率的 WiFi，配置高于能力的流数。

预期：负载上限分别不超过 866、2402、2500 Mbps；协商 4804M 也不会默认按 4.8G 灌包或当成 PASS 目标。

### R08：未知目标 discover

步骤：20 条 500M，`rate_mode: "discover"`，`discovery_step_secs: 10`。

预期：

- 流约按 5/10/15/20 四档逐级加入。
- 报告原始输出含 `active_streams,samples,avg_rx_mbps,p10_rx_mbps`。
- 最终结果为 `MEASURED`，不因某一档达到协商速率自动 PASS。

## 五、连续采样与有效窗口

### W01：完整 180 秒共同窗口

步骤：正常运行 180 秒 UDP 双向测试。

预期：

- 起流前有默认 3 秒背景样本。
- 达到各方向 quorum 后丢弃默认 5 秒 settle。
- 报告有效/要求秒为 180/180，采样覆盖率不低于 95%。
- iperf3 实际进程时长比 180 秒长，属于设计预期。

### W02：流启动较慢但最终窗口完整

步骤：让一条必需流延迟数秒后重试成功。

预期：共同窗口从最后一个方向达到 quorum 后开始，早启动流自动多跑，最终仍取得 180 秒；启动爬升不计入 RX 平均。

### W03：一方向提前跌破 quorum

步骤：在共同窗口结束前让 2 流方向的一条流提前结束。

预期：共同有效窗口在跌破 2 条时结束；不足 180 秒则 `NOT_EVALUATED / EFFECTIVE_WINDOW_SHORT`，不能拿较短数据硬判 PASS。

### W04：背景流量扣除

步骤：起流前制造稳定且可控的背景 RX/TX，开始 UDP 后保持背景流量不变；另做一次只有
背景 NIC 流量、工具没有测量的单流对照。

预期：已由工具证明起流时，报告统计扣除背景中位数；原始网卡总流量与报告业务流量差值
接近背景值。只有背景流量时不得证明单流灌通。背景变化剧烈时应通过 P10/覆盖率和原始样本排查。

### W05：计数器回退或采样失败

步骤：测试中重启网卡或制造计数器读取失败。

预期：异常样本 `valid=false`，错误写入 monitor 输出；覆盖率低于 95% 时为 `NOT_EVALUATED / SAMPLE_COVERAGE_LOW`，不能产生 `RATE_FAIL`。

### W06：双向统计口径

预期：AB 和 BA 使用同一个共同时间区间，但分别读取各自接收端 RX、发送端 TX并分别对目标判定；不能把两个方向速率相加后只给一个 PASS/FAIL。

### W07：iperf stdout 缓冲后集中输出

步骤：使用会块缓冲 stdout 的旧版 iperf3，或在测试桩中让全部 interval 行在进程结束时集中到达；输出同时包含非零起点区间（例如 `5.00-180.00 sec`）。

预期：

- 活跃时长优先取 iperf 行内 `结束秒 - 开始秒`，示例按 175 秒而不是 180 秒，更不能按日志集中到达的几毫秒计算。
- 该时间只用于裁剪网卡样本；正式 RX 平均/P10 仍来自接收网卡计数器。iperf3 的 rate/bytes/datagram 输出可证明流已建立并保留为诊断列，但不替代 NIC 正式口径。
- 无法解析可信 interval 时才回退到 Traffic/Connected/Started 事件；短测量不能被扩成完整 180 秒。

### W08：恢复样本不能伪造稳定窗口

步骤：让网卡计数器读取失败一个或多个周期，随后恢复；恢复样本的 `interval_ms` 跨越失败周期，并保持总字节差正确。

预期：恢复样本按完整时间参与加权平均和总覆盖率，但不能单独生成多个完整 5 秒窗口。有明确目标时，RX/TX 任一侧完整滚动窗口覆盖率低于 95% 均为 `NOT_EVALUATED / RATE_WINDOW_COVERAGE_LOW`；不足 5 秒时 P10 必须为空，不能退化成瞬时样本 P10。

### W09：失败重试前同步释放端口

步骤：让某单流首轮完整执行但没有工具测量且能够安全清理，观察下一轮；再制造一次 server/client stop、kill/wait 或 reader 回收无法确认的测试桩结果。

预期：

- iperf3 与 CTS 的正常无测量路径都先停止 client，再按本轮 request ID 精确停止 server；确认进程退出、wait 回收和 reader 结束后，才使用新 request ID 在同端口重试。
- stop 未确认时禁止同端口继续重试，并以 `SETUP_ERROR` 报告资源清理错误；不能靠等待固定毫秒数猜测端口已经释放，也不能计入“安全耗尽”。
- 迟到的旧 request stop 不会杀死同端口的新 request。

### W10：响应丢失、owner 清理与旧 agent 门禁

步骤：分别丢弃一次 server/client start 响应、重复发送同一请求；让测试单元 panic；再分别
使用不声明 `reliable_lifecycle_v1` 或 `ctstraffic_v1` 的旧 agent 运行对应后端任务。

预期：

- 同一 request ID、相同参数只复用已有进程/作业并续租，不创建第二份；stop-before-start tombstone 阻止迟到 start 复活。
- 单元正常结束、错误和 panic 后均按唯一 owner ID 清理 client → server → monitor；重复 cleanup 幂等，owner A 不影响 owner B。
- 清理未确认产生 `RESOURCE_CLEANUP_FAILED`；单元 panic 产生 `UNIT_PANIC`，随后单元继续执行。
- 旧 agent 在任何对应工具进程启动前被主控明确拦截并提示升级；仅阻断缺能力的后端，另一吞吐后端和 ping-only 任务仍可执行。

## 六、正式判定边界

### V01：发送负载不足

步骤：配置明确 RX 目标，但故意降低每流 `-b` 或减少流数。

预期：

- 配置阶段已能确定流数不足时：`NOT_EVALUATED / CONFIGURED_LOAD_TOO_LOW`。
- 运行后 TX-P10 未达到 `目标 × (1 + offered_headroom_pct)` 时：`NOT_EVALUATED / OFFERED_LOAD_LOW`。
- 两者均不能记为 CPE `RATE_FAIL`。

### V02：RX 平均低于目标

前提：流数、TX-P10、窗口和覆盖率均满足。

预期：`RATE_FAIL / RX_BELOW_TARGET`，详情列出实际 RX 平均和目标。

### V03：平均达标但持续掉速

前提：RX 平均达到目标，5 秒滚动 P10低于目标。

预期：`UNSTABLE / RX_UNSTABLE`，不能被平均值掩盖为 PASS。

### V04：UDP 丢包率

步骤：配置 `max_udp_loss_pct`，制造部分流不同的 lost/total。

预期：优先按所有流的 `lost datagrams / total datagrams` 加权计算组合丢包率，而不是简单平均各流百分比；超限时 `RATE_FAIL / UDP_LOSS_HIGH`。若已配置丢包门槛但旧版/异常输出完全缺少丢包数据，应为 `NOT_EVALUATED / UDP_LOSS_DATA_MISSING`，不能用 0% 代替缺失值。

### V05：未知目标与 RESUME

步骤：observe 场景完整跑出 `MEASURED`，随后使用 `--resume` 再跑。

预期：该任务不会被跳过。只有正式 `PASS` 才进入 24 小时 RESUME 成功库。

### V06：verify 忘记配置目标

步骤：在非 EVB 自动目标路径上设置 `rate_mode: "verify"`，但不填写 `rate_targets_mbps`。

预期：结果为 `NOT_EVALUATED / TARGET_MISSING`，原因详情提示补充有效目标；不能静默退回 `MEASURED`，更不能 PASS。

## 七、报告、日志与截图

### L01：报告只按单元统计

步骤：运行 20 流单向或 20+20 双向任务。

预期：顶部“测试单元”只增加 1，不因流明细和方向组合行增加为 20、21、40 或 42。

### L02：失败流可回溯

预期：每条 iperf3 流和每个 CTS attempt 都包含命令、client 输出、server 输出、结构化事件、
重试次数和最终错误；组合行包含请求/活跃/要求流及原因码。单流三轮不能覆盖前两轮原文。

### L03：截图成功

预期：主控和辅测任一端成功即可在报告中出现链接；成功保存路径写入主控日志。

### L04：截图失败原因

分别制造：

- 主控截图 API 失败。
- agent HTTP 请求失败或返回非 200。
- agent 返回错误 JSON、缺少 data 或非法 Base64。
- 输出目录不可写。

预期：`master_日期时间.log` 写出对应的具体失败环节和错误信息；吞吐 verdict 不因截图失败而改变。

## 八、自动化回归

开发机执行：

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
git diff --check
```

自动化单元测试至少覆盖：

- `flow_retries=0/1` 时两种后端单流仍有 3 次总尝试，较大配置按 `flow_retries+1` 扩展。
- iperf3 单流和 CTS `Connections:1` 前两轮无测量、第三轮成功；parser/verdict 只使用成功轮。
- 两种后端三轮安全无测量分别产生 `SINGLE_UDP_STREAM_FAILED` 和 `CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED`。
- 双向每方向 1 流各自拥有独立三轮预算并行执行，硬失败优先于另一腿普通 `NOT_EVALUATED`。
- 平台、工具、非法参数、start/status、显式取消和清理未确认均保持 `SETUP_ERROR`，cleanup 不确定时禁止复用端口。
- 背景 NIC 流量不能充当工具 rate/bytes/frame/datagram 证据；已有工具测量后按真实错误、丢包和目标判定，不继续重试掩盖结果。
- CTS raw log 保留全部 attempt、最终 `retry_count=完整尝试数-1`，单流三轮的 estimate/lease 覆盖完整墙钟预算。
- 2 条要求 2 条、5 条要求 4 条、20 条 EVB 目标要求 18 条。
- 2 流失败的组级重试边界。
- 5 流/2 流双向中，小方向只有 1 条时不能形成共同窗口。
- 完整和不足 180 秒的共同窗口。
- 背景扣除、P10 和采样覆盖率。
- 非零起始 iperf 区间按 `end-start` 计算，缓冲集中输出不缩短活跃窗口。
- 网卡平均按时间加权，恢复长样本不能伪造 5 秒滚动窗口，RX/TX 滚动覆盖不足不能正式判定。
- discover 的 25%/50%/75%/100% 阶梯。
- 32 个异步 client job 同时处于运行态，不受 16 worker 限制。
- server 真实端口在 stop 成功返回后可立即重新绑定；旧 stop 不杀新 request。
- client stop 等待 worker/子进程/reader 完整退出，重复 start/stop 和 stop-before-start 均幂等安全。
- owner 清理隔离、动态 lease、单元 panic 后清理和旧 agent capability gate。
- UDP lost/total 解析与加权丢包基础数据。
- 报告只统计测试单元汇总行。
- RNDIS 3.7G、NCM 4.2G/10G、EVB 8.4G/6.4G 和 CPE 路径裁剪。

在受限沙箱中，HTTP roundtrip 测试可能因为禁止绑定本地 socket 报 `Operation not permitted`；应在允许本地监听的环境重跑，不能将其误判为业务代码失败。
