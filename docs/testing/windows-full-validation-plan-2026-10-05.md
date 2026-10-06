# Windows 实机全面验证方案：Mac+Windows 双机 → Windows 单机（2026-10-05）

本文是测试设计，不是通过报告。所有用例初始状态 `NOT_RUN`。目标是在真实网卡、真实工具和真实被测 CPE 上检查工具的**功能、准确性、稳定性**三件事；不是 CPE 性能认证，测得的吞吐数字只用来检验工具，不作为设备指标。

执行顺序固定为：**阶段 0（冻结与门禁）→ 阶段 A（Mac+Windows 双机）→ 阶段 B（Windows 单机）**。双机在前，因为它能用 Mac 侧的 dummynet 造出「已知真值」，先把准确性的标尺立住；单机在后，覆盖只有 Windows 两端才能跑的 ctsTraffic、出厂参数和长稳。

与上午三份记录的关系：[双机有线冒烟](windows-real-machine-2026-10-05.md)、[统计与功能重点](windows-statistics-functions-2026-10-05.md)、[单机有线与 Wi-Fi](windows-same-host-wifi-2026-10-05.md)是本方案的前置探路，结论**不直接计入**本方案的 PASS；它们暴露的缺口在第 10 节逐条落到用例上。

---

## 1. 被测对象与冻结

- 分支 `feat/webui-vue`，HEAD `995b19d`，加上 41 个文件的未提交修改（+2840/−551）。当前工作区 265 个受跟踪源文件与上午最终验收用的 `final-source-manifest.json` 逐项 SHA-256 一致，对应 Windows exe `1b5ac090…dc154`。
- 开跑前重新生成源码清单与 exe 哈希（P0-01）。执行中途**任何**源码改动（包括修缺陷）都要重新冻结、重跑阶段 0；改动之前跑过的用例要标明它对应的是哪一版快照。
- Mac 辅测用同一份快照在 Mac 上 `cargo build --release` 构建，记录其哈希。

## 2. 环境事实（2026-10-05 实测探查）

| 项 | Mac（本机） | Windows（`ssh windows-cpe`） |
|---|---|---|
| 系统 | macOS，arm64 | Windows 11 Pro 22621，Ryzen 7 7735HS 16 线程，28.7 GB |
| 有线 | `en0`，1000baseT 全双工，192.168.8.102，`2408:843e:ca0:7531::3` | 「以太网」，Realtek 2.5GbE 协商 1 Gbps，192.168.8.103，`…::5`，扫描角色 `SGMII1G` |
| Wi-Fi | `en1`，802.11ax 5 GHz 信道 48 / 160 MHz，192.168.8.106，`…::2` | 「WLAN」，Intel AX200，802.11ax 5 GHz 信道 48，协商 2402 Mbps，192.168.8.108，`…::7` |
| 工具 | iperf3 3.18，rustc 1.96.0，node 24.12.0，adb（已连被测 CPE） | iperf3 3.22，ctsTraffic 2.0.4.0（`C:\CPE-Acceptance\app`），Rust 1.99 MSVC；**无 adb** |
| 其他 | — | 防火墙三个配置文件全开，两块网卡都归在 Public；电源计划「平衡」；当前无遗留测试进程、规则或计划任务 |

- 被测 CPE：OpenWrt 21.02.7，内核 5.4.238 aarch64，`br0` = 192.168.8.1，是两台电脑的网关，板上有 `/usr/bin/iperf3`，经 Mac 的 adb 可达。型号和接线拓扑仍未确认，P0-03 补记。
- 四块网卡同在 192.168.8.0/24，且都有全局 IPv6 与 link-local，所以 IPv6 真流量可以跑（上午三轮都没覆盖）。
- **控制面和数据面没有分开**：`ssh windows-cpe` 走 Windows「以太网」（192.168.8.103）。这带来两条执行约束：
  1. 测量窗口内不许经 SSH 传文件，只做必要的状态轮询，证据等用例结束后再拉，否则 SSH 流量会混进被测网卡的 RX；
  2. 要断开 Windows 有线的用例，必须改用 WLAN 地址（`ssh -o HostName=192.168.8.108 windows-cpe`）登录，而且断开动作必须由**到点自动恢复**的计划任务执行。
- 通过 OpenSSH 登录得到的是非交互会话，没有桌面。GDI 截图、本机浏览器这类用例要放进「仅在用户登录时运行」的交互式计划任务里跑，不能直接在 SSH 里起。

### 端口与身份（本方案专用，避开默认值）

| 用途 | 地址:端口 |
|---|---|
| A1 Mac 辅测 agent | 192.168.8.102:29881 |
| A1 Windows 控制台 `ui` | 192.168.8.103:29880（`--ui-bind 192.168.8.103`，供 Mac 浏览器访问） |
| A2 Windows 辅测 agent | 192.168.8.103:29881 |
| A2 Mac 控制台 | 127.0.0.1:29880 |
| B 本机 agent | 127.0.0.1:29885 |
| 数据端口 | 56000–56400 TCP/UDP |

令牌一律用本轮专用值，只出现在证据目录里，不写进仓库。

## 3. 通用验收规则

### 3.1 状态与结论

- 用例状态：`NOT_RUN / RUNNING / PASS / FAIL / BLOCKED / NA`。缺设备、缺权限、缺时间记 `BLOCKED`，不能记 `NA`。父项要等全部必需子项都 PASS 才算 PASS。
- **用例 FAIL 和产品 RATE_FAIL 是两回事**。注入低速后产品给出 RATE_FAIL，这条用例是 PASS；健康链路上大量出现 NOT_EVALUATED / SETUP_ERROR，必须解释清楚，不能当作「吞吐已验收」。
- Observe 模式只出 MEASURED，不等于达标；门限分支在 AC-06 里专门测。
- 跨平台配置用 1m socket window，结果只证明执行链和统计链，**不计入 Windows 出厂参数验收**。出厂参数只在 B2 验，预设参数一律不改。

### 3.2 准确性容差（判定之前先定好，不许跑完再放宽）

| 对账项 | 基准 | 容差 |
|---|---|---|
| 报告 RX/TX 平均、P10、中位/P95/最小/最大、覆盖率 | 从保存的 NIC CSV 独立复算（不调产品函数） | 绝对 1e-5 Mbps；覆盖率 1e-10 |
| CSV 原始累计计数 | OS 计数：Windows `Get-NetAdapterStatistics`，Mac `netstat -ibn` 前后快照 | 首样本 ≥ 前快照，末样本 ≤ 后快照 |
| NIC RX / iperf3 接收端 goodput | 理论封装比：TCP ≈ 1514/1460；UDP `-l 1400` v4 ≈ 1442/1400、v6 ≈ 1462/1400 | 理论值 ±2%；超出时先查背景流量和计数口径（Wi-Fi 驱动是否计 L2 头）再定性 |
| dummynet 限速真值 R | 工具外先用裸 iperf3 确认限速生效 | 工具 RX ∈ [0.97R, 1.05R] |
| 有线重复性 | 同一单元连跑 5 次 | 有线 TCP 变异系数 ≤ 3%；Wi-Fi 只记录不判 |
| 判定窗口 | 配置时长 | 有效窗口 = 要求时长 ± `WINDOW_COMPLETE_TOLERANCE_MS`（100 ms）；覆盖率 ≥ 0.95 |
| 双向合计 | AB 接收端 RX + BA 接收端 RX，按两腿交集重算 | 1e-5 Mbps |
| Ping | 原始输出的发/收/丢、RTT 最小/平均/最大 | 完全一致 |

### 3.3 稳定性预算（沿用 [stable-regression-plan](stable-regression-plan.md) §6）

- 控制台 API 正常负载下 p95 ≤ 2 s，不触发客户端超时。
- 预热后同负载下，工作集、句柄、线程不持续单调增长；收尾后工作集相对起始空闲增长 ≤ max(50 MiB, 10%)。
- 资源回收时限按源码常量推出，不凭感觉定：
  - agent 收到 Ctrl+C 后资源归零 ≤ 5 s（`SWEEP_INTERVAL` 200 ms）；
  - 主控死亡后，agent 侧租约 = 时长 + 300 s（`RESOURCE_LEASE_GRACE_SECS`），再加一个 30 s 清扫周期；
  - 主控收尾等待 10 s（`RESOURCE_CLEANUP_WAIT_SECS`）；
  - CTS 进程宽限 30 s（`ctstraffic::PROCESS_GRACE_SECS`）；
  - 控制台监控：页面 90 s 不轮询即停采样并通知 agent 停止（`MONITOR_IDLE_TIMEOUT`）；180 s（`UI_MONITOR_LEASE_SECS`）是交给 agent 的兜底租约，只在控制台自身挂掉时起作用。
- 任何崩溃、死锁、持续泄漏、数据丢失、**错误 PASS**、重复起流，都立即登记为阻断缺陷；保留现场（日志、dump、进程与端口快照、owner 信息）后才能清理、才能继续。

---

## 4. 阶段 0：冻结、门禁与环境准备（约 1.5 h）

| ID | 内容 | 通过标准 |
|---|---|---|
| P0-01 | 生成源码清单（全部受跟踪文件 + 未跟踪源文件的 SHA-256）、`git diff` 摘要、exe 与 iperf3/ctsTraffic 哈希；同步到 Windows 编译目录后逐项比对 | 两端清单零差异 |
| P0-02 | Mac 跑 AGENTS.md §1 四条门禁；`ui/` 跑 `npm ci && npm run test && npm run build && npm run verify`，**先 verify 已提交产物再重建**；Windows 原生跑 `cargo test --locked`、两条 clippy、`cargo build --release` | 全部退出码 0；Windows 原生测试数与 Mac 的差异逐项归因到 cfg 平台 |
| P0-03 | 环境快照：两端 OS、驱动版本、网卡速率/双工/频段/信道/信号、IP 与路由表、MTU、时钟偏差（`w32tm /stripchart`、`sntp`）、CPE 板子 `uname`/`/proc/net/dev`/桥成员；WLAN 配置文件名只进证据目录 | `environment.json` 两端各一份 |
| P0-04 | Windows：建防火墙规则组 `CPE-Full`，按程序（cpe_test/iperf3/ctsTraffic）放行 56000–56400、29880–29885、ICMPv4/v6 回显，远端只限 192.168.8.0/24 与 `2408:843e:ca0:7531::/64`；记录并临时关闭睡眠（`powercfg /change standby-timeout-ac 0`）。Mac：记录 pf 状态与应用防火墙状态，执行期间开 `caffeinate -dimsu` | 规则可整组删除；原始电源/pf 设置已存档 |
| P0-05 | 裸工具基线：工具之外直接用 iperf3 跑四条链路各方向 TCP/UDP 20 s、ping 100 次 | 得到每条链路的能力参考值，后面门限都从它推 |
| P0-06 | `cpe_test scan` 两端各跑一次，`cpe_test --help` | 角色：Windows 以太网→`SGMII1G`、WLAN→Wi-Fi 5G；Mac en0/en1 同理；IPv4/IPv6/速率与 P0-03 一致 |

---

## 5. 阶段 A：Mac + Windows 双机

链路编号（主控端 ↔ 辅测端）：

| 链路 | 两端 | 意义 |
|---|---|---|
| L1 | Win 以太网 ↔ Mac en0 | 有线↔有线，最干净的标尺链路 |
| L2 | Win 以太网 ↔ Mac en1 | 有线↔Wi-Fi |
| L3 | Win WLAN ↔ Mac en0 | Wi-Fi↔有线 |
| L4 | Win WLAN ↔ Mac en1 | Wi-Fi↔Wi-Fi，触发 ADR-18 双向合计 |

跨平台测试配置（`config-a.json`，证据目录内，不进仓库）：用 `tests[]` 显式列举，`iperf.duration` 30，`rate_check` 取 `background_secs` 3、`settle_secs` 3、`sample_interval_ms` 1000，TCP 窗口 `1m`，UDP 档位 `300m / -l 1400 / window 1m`（**必须显式写 window**，省略会走 Windows 的 256m，在 Mac 上必然报错）。

### A1 主轮：Windows 主控 + Mac 辅测（约 8 h）

Windows 是生产上的主控形态，这一轮覆盖 Windows 侧的主控代码路径：GetIfTable2 采样、中英文 ping 解析、GDI 截图、控制台、子进程看门狗。

#### A1-F 功能矩阵

| ID | 内容 | 预期 |
|---|---|---|
| A1-F01 | L1 全矩阵：ping（32/1472/1473 字节 × 100 次）、iperf TCP（`-P 1`、`-P 4`）、UDP（单流、3 流）× v4/v6 × ab/ba/bidir，共 42 单元 | 全部执行完成；有效窗口满 30 s；无 SETUP_ERROR；v6 走 link-local（Mac 端带 zone、Windows 端不带） |
| A1-F02 | L4 全矩阵同 A1-F01，另加 bidir 配 `rate_target_bidir_total_mbps` 的单元 | 合计门限下两条腿落成 Observe/None，单元只比一次合计；预览上能看到最终生效的门限 |
| A1-F03 | L2、L3 精简：TCP `-P 4`、UDP 单流、ping 32 × v4/v6 × 三方向，共 36 单元 | 同上；报告方向、端点、协议字段走类型化字段，HTML 与 Excel 一致 |
| A1-F04 | 跨机 ctsTraffic（Mac 端不是 Windows） | 在起流**之前**被平台门禁拒绝，给出明确原因；不挂起、不出假 PASS、不留进程 |
| A1-F05 | UDP 按网卡速率裁流：`limit_udp_by_link_speed: true`，L1 上 300m × 5 流 | 1000/300 → 裁到 3 流并出计划提示；Wi-Fi 链路不裁 |
| A1-F06 | 同 /24 门禁：Mac en0 临时加别名 `10.99.0.2/24`，用它配一条跨机对 | iperf 被跳过并给出提示，ping 照跑；收尾删除别名 |
| A1-F07 | 路径 MTU 探测：`ping.probe_path_mtu: true` | v4 二分出 1500 路径（1472 能过、1473 不能）；v6 明确拒绝，不给数字；结果只进诊断 |
| A1-F08 | 负载下时延探针（`probe_during_traffic`） | 探针与灌包同起同落：不混入起流前的空载 RTT；跳过单元后探针在一个分段（≤10 s）内停下 |
| A1-F09 | 截图：交互式计划任务里跑主控，`screenshot: true` | Windows GDI PNG 解码有效；Mac 侧没有屏幕录制权限时静默跳过，不影响判定 |
| A1-F10 | 稳定性轮次 `rounds: 2` | 第 2 轮 ID 与第 1 轮不同（不会命中刚写下的 PASS）；对比报告每轮各占一行 |
| A1-F11 | `cpe_test monitor -n 以太网 -i 1 -d 60 -c mon.csv`，与 iperf 并行 | CSV 平均值与 `Get-NetAdapterStatistics` 的差值折算一致；`-n` 填不存在的网卡时明确报错，不输出 0 速率 |
| A1-F12 | 报告重放与对比：`cpe_test report <run>`，再拿两轮跑 `cpe_test compare` | 重放结果与原报告逐行一致；无回归时退出码 0，有回归 1，不完整 2 |

#### A1-C 准确性（Mac 侧 dummynet 造真值，需要 sudo）

dummynet 规则装进独立锚点 `com.apple/cpe-test`，只匹配本轮两端地址和 56000–56400 端口；装规则前、撤规则后各记一次 `pfctl -s info`。每次改限速，先用裸 iperf3 确认生效，再跑工具。

| ID | 内容 | 预期 |
|---|---|---|
| A1-C01 | 本阶段所有吞吐行 100% 从 NIC CSV 独立复算（复用上午的 `recompute_rx.py` / `audit_stats.py`，先复核脚本本身） | 满足 3.2 第一行 |
| A1-C02 | OS 计数夹逼：每单元前后各取一次两端 OS 网卡快照 | 满足 3.2 第二行 |
| A1-C03 | NIC/goodput 封装比：L1 的 TCP、UDP v4/v6 | 满足 3.2 第三行；上午 Wi-Fi 接收端 UDP 100m 只测到 101.04 Mbps（理论约 103），这里要用 goodput 比值把原因查清 |
| A1-C04 | 限速真值：L1 Win→Mac 方向，`dnctl pipe` 依次限 300/500/700 Mbit/s，TCP 和 UDP（UDP 发送速率设在 R 之上） | 满足 3.2 第四行；UDP 超发部分只进诊断（丢包/TX），**不改速率判定** |
| A1-C05 | 重复性：L1 TCP `-P 4` 连跑 5 次；L4 同样跑 5 次 | 有线 CV ≤ 3%；Wi-Fi 只记录 |
| A1-C06 | 判定分支：门限分别取 0.5×基线（PASS）、1.2×基线（RATE_FAIL / RX_BELOW_TARGET）、Verify 模式不给门限（NOT_EVALUATED / TARGET_MISSING）、Observe（MEASURED）、Auto 不给门限（MEASURED）；限速 500 下门限取 480 / 520 做贴边 | `(verdict, reason_code)` 组合与源码契约一致；退出码与 FAIL 数一致 |
| A1-C07 | 中途断流：60 s TCP 单元在第 25 s 用 pf `block` 断流 10 s；门限设成「平均值仍能过」 | 判 PASS，同时带中途掉速/断流诊断；断流旁证判为「真断流」，**不判 COUNTER_STALLED**；曲线在断口处断开，不是一条直线连过去 |
| A1-C08 | 注入丢包与时延：pipe 加 `plr 0.01`、`delay 20` | UDP 丢包约 1%，只进诊断；ping 丢包率与 RTT 增量和注入值一致；负载下时延探针反映出排队时延 |
| A1-C09 | 双向合计复算：L4 bidir 合计单元 | 按两腿交集重算出的值与报告一致；交集不够要求时长 → `EFFECTIVE_WINDOW_SHORT` |
| A1-C10 | Ping 对照：同一组次数和包长，在工具之外手动 `ping` 一遍 | 工具解析出的数字与原始输出一致；丢包率与对照实验在统计上可比 |
| A1-C11 | 出口一致：HTML / XLSX / rows.jsonl / meta.json / 进度页 | 数值、判定、八格计数（`verdict_totals`）完全一致；组合计行不计入总数 |
| A1-C12 | 跨机时间轴：记录两端时钟偏差，核对 Mac 侧样本是否对齐到主控的单元零点 | 窗口边界只取决于单调时钟和 elapsed，与墙钟偏差无关 |

#### A1-S 稳定性与故障注入

| ID | 内容 | 预期 |
|---|---|---|
| A1-S01 | 2 h 混合浸泡：L1+L4 的 TCP/UDP/ping 子集，`rounds` 循环；两端每 60 s 采进程资源（Windows：工作集/句柄/线程；Mac：RSS/文件描述符/线程） | 满足 3.3；无未解释的 SETUP_ERROR |
| A1-S02 | 30 次起停：经 API 在单元开始后第 5–40 s 之间随机点停止 | 每次都在时限内回收（两端无本轮 iperf3、无监听端口），紧接着的下一轮能被受理；部分结果与报告都保留 |
| A1-S03 | 10 次跳过当前单元，含「跳过后立即停止」 | 跳过只影响当前单元；停止不会被跳过吃掉 |
| A1-S04 | 运行中 `kill -9` Mac agent | 剩余单元给出明确的 SETUP_ERROR / 熔断结论，不挂起；Windows 本机 iperf3 被回收；agent 重启后下一轮正常 |
| A1-S05 | 控制面中断：pf 挡住 29881 端口 20 s，数据面不动 | 要么正常完成，要么明确报错；不重复起流（`request_id` 幂等），agent 侧 server 数量不翻倍 |
| A1-S06 | `taskkill /F` 主控 | Windows 子进程由看门狗 ≤ 5 s 回收；Mac 侧资源在租约 + 30 s 内回收；`cpe_test report` 能重放半截的 run；带 `--resume` 重跑时 24 h 内的 PASS 被跳过 |
| A1-S07 | L4 单元进行中，Mac Wi-Fi 断电 15 s 后恢复 | 该单元给出带断流诊断的 RATE_FAIL 或 NOT_EVALUATED；恢复后的后续单元正常；拓扑变化有记录 |
| A1-S08 | L1 TCP 单元连续 30 min | 采样不断档、覆盖率 ≥ 0.95；累计计数越过 4 GiB 后不截断 |
| A1-S09 | 熔断：全程用 pf 挡住数据端口 | 连续 `abort_after_dead_traffic_units` 个无流量单元后整轮停下，补跑诊断 ping；`traffic_setup_errors` 不超过 `traffic_units` |

#### A1-U 控制台（Windows `ui`，Mac 上用 Chrome 经局域网访问）

| ID | 内容 | 预期 |
|---|---|---|
| A1-U01 | 带 `?token=` 打开；地址栏抹掉 token；F5 刷新 | 页面正常（靠会话 cookie）；无运行期 JS 错误 |
| A1-U02 | 只带 cookie 调 `/api/*`；不带 token；错 token | 一律 401 |
| A1-U03 | `curl -H "Host: evil.example"`；`Host: 192.168.8.103:29880` | 域名被拒，IP 放行 |
| A1-U04 | 连接页：连 Mac agent、重扫 | 网卡表的全部字段与 P0-03 一致（Wi-Fi 频段/信道/信号）；运行中「连接」「重新扫描」禁用 |
| A1-U05 | 计划页：按链路集合勾网口、编辑测试内容、门限与默认值；预览 | 门限优先级与预览 `targets` 一致；空计划、被跳过项会阻断开始 |
| A1-U06 | 执行页：开始、进度、跳过、停止；连点开始 | 只受理一次；未知应答保持 unknown，不当作已启动 |
| A1-U07 | 历史页：列表、下载 bundle.zip、重放报告、重新执行（RESUME 装载）、两轮对比 | 文件完整；离线打开报告时没有外部请求 |
| A1-U08 | 监控页：本机和辅测机各开一个会话，关掉页面 | 90 s 无轮询后两端会话都被回收（agent 侧另有 180 s 兜底租约） |
| A1-U09 | 项目导出再导入 | `master_config` 四个块齐全，不含 `by_nic`；导入后预览与导出前一致 |
| A1-U10 | 界面文案与动画：运行中执行 `document.getAnimations().length` | 恒为 0；界面不出现 ctsTraffic / CTS 字样 |
| A1-U11 | 灌包下的界面开销：L1 TCP 同一单元，控制台关/开各跑 3 次 | 中位数下降 ≤ 5% |

### A2 角色互换：Mac 主控 + Windows 辅测（约 2 h）

生产环境两端都是 Windows。这一轮补上 Windows 作为 **agent** 的代码路径（agent HTTP 服务、作业管理器、Windows 侧经 HTTP 的采样与回收）；另外 adb 只在 Mac 上，内环只能在这一轮测。

| ID | 内容 | 预期 |
|---|---|---|
| A2-F01 | L1、L4：TCP/UDP/ping × v4/v6 × 三方向冒烟（36 单元） | 与 A1 同链路同参数的结果可比（中位数差 ≤ 5%，Wi-Fi 只记录） |
| A2-F02 | Windows agent 收到 Ctrl+C；agent 端 owner 清理；迟到的启动请求 | 5 s 内资源归零；已关闭的 owner 拒收迟到的资源启动 |
| A2-F03 | 内环（ADB）：链路包括 Mac en0/en1（host=master）与 Windows 以太网/WLAN（host=agent）；TCP/UDP × upload/download/bidir × v4/v6；三种测量策略 `nic_strict / nic_preferred / tool` | 上行取板侧 `br0` RX，下行取电脑网口 RX；网卡口径与板侧 `/proc/net/dev` 前后差值对账；可信低速不触发兜底；bidir 按两腿交集计算 |
| A2-F04 | 组合场景：控制台发 `/api/scenario/run`（先子网后内环），中途刷新页面 | 阶段持久化（`inner` → `finished`）；刷新后用启动令牌续上，不重复发起 |
| A2-F05 | 内环里 `enabled: false` 的 Windows 链路，同时 Windows agent 离线 | 不阻断 Mac 本机链路的测试 |

---

## 6. 阶段 B：Windows 单机（以太网 ↔ WLAN 经 CPE）

主控和本机 agent（127.0.0.1:29885）都在 Windows 上，`tests[]` 写 `master:NAME=以太网` ↔ `master:NAME=WLAN`。上午单机那轮的用法已经证明这条路可行。

### B0 前提：流量确实出了物理网口（约 0.5 h）

同一台主机的两个本地地址之间，协议栈有可能直接在内部投递，流量不经过 CPE。

| ID | 内容 | 通过标准 |
|---|---|---|
| B0-01 | 路由表、源地址绑定；10 s iperf 期间开 `pktmon`，只抓这两个 IP | 物理 NIC 层能看到两张网卡 MAC 之间的帧 |
| B0-02 | 同一段 iperf 前后取两块网卡的 `Get-NetAdapterStatistics` | 两块网卡都有和流量同量级的收发增量 |
| B0-03 | 反证：故意绑到只走回环的地址对 | 工具判 NOT_EVALUATED（计数器零增长），**不能**给出 PASS；用来验证计数器可信度这道门是活的 |

B0-01 或 B0-02 不通过，阶段 B 的吞吐用例全部 BLOCKED。

### B1 功能矩阵（约 1.5 h）

| ID | 内容 | 预期 |
|---|---|---|
| B1-F01 | ping 32/1600/65500 × 100 次 × v4/v6 × 三方向 | 判定与原始输出、RTT 门限分档（有线/Wi-Fi × 小/中/大包）一致；上午只发 3 包看到的丢包，这里用 100 包重新定性 |
| B1-F02 | iperf TCP `-P 1/4`、UDP 单流/3 流 × v4/v6 × 三方向 | 有效窗口满；IPv6 真流量通过 |
| B1-F03 | ctsTraffic TCP/UDP × v4/v6 × 三方向，状态间隔取默认值和 100 ms 两档 | 有效窗口满足要求时长。上午 6 项因窗口不足（7.4 s / 8.7 s，要求 10 s）判 NOT_EVALUATED，当时的二进制已包含现在这份 settle 延长逻辑，所以这一项**预期仍可能复现**：先把原始事件时间线（TimeSlice 与接收时刻）拉出来定位根因，再决定是否登记缺陷 |
| B1-F04 | CTS 的 RX 分布字段 | 中位/P95/最小/最大要么有值且复算一致，要么明确登记成展示缺口；空值不能算通过 |
| B1-F05 | CTS 作业：幂等 request_id、同步停止确认、owner 清理；起流中途杀掉 ctsTraffic | 流量跑过的判 `CTSTRAFFIC_EFFECTIVE_WINDOW_SHORT`，从没起来的判 SETUP_ERROR；30 s 宽限内进程消失 |
| B1-F06 | RESUME：带门限跑出 PASS 后，用 `--resume` 再跑 | 只有 24 h 内的 PASS 被跳过，0 灌包；改门限后不再命中 |

### B2 Windows 出厂参数（约 2.5 h）

单机只有一块 SGMII1G 和一块 Wi-Fi 5G，出厂预设按「主控角色 ↔ 辅测角色」配对，原样跑会配成同一块网卡、无法执行。做法是：**把 `dist/configs/config-all-common.json` 的 `iperf`、`rate_check`、`ctstraffic`、`ping` 块逐字复制**，只把 `pairs` 换成以太网↔WLAN 的 `tests[]`。在证据里记录这份配置与原预设的 diff，确认 diff 只涉及拓扑。

| ID | 内容 | 预期 |
|---|---|---|
| B2-01 | 默认参数：180 s，5 流，TCP 64k/1m/4m，UDP 100m/500m/1000m(-l 64)/2500m，Auto 模式，ping 180 次 × 32/1600/65500，三方向，v4 | 全部单元执行完成；UDP `-w 256m` 在 Windows 上正常工作；`-w` 排空诊断（`SOCKET_BUFFER_DRAIN_WARN_SECS`）按条件出现 |
| B2-02 | 默认参数下跑 ctsTraffic TCP/UDP × 三方向 | 同 B1-F03 的窗口要求 |
| B2-03 | 门限来源：Auto 模式下的角色门限、Wi-Fi 频段门限、`rx_target_link_speed_ratio` | 预览中的门限与报告一致，能说清每个门限的来源 |

### B3 准确性（约 1 h）

| ID | 内容 | 预期 |
|---|---|---|
| B3-C01 | B1/B2 全部吞吐行做 CSV 独立复算 + OS 计数夹逼 | 满足 3.2 前两行 |
| B3-C02 | 用 pktmon 计数佐证物理层收发量级 | 环形抓包只用来证明路径，不拿来算全程丢包 |
| B3-C03 | Windows QoS 限速真值：`New-NetQosPolicy -PolicyStore ActiveStore` 按源地址限 300 Mbit/s，先用裸 iperf3 验证生效 | 工具 RX ∈ [0.95R, 1.08R]（QoS 限速比 dummynet 粗，容差放宽，开跑前定死）；如果限速不生效，记 BLOCKED，以 A1-C04 的结论为准 |
| B3-C04 | CTS 单位换算：TCP bytes/s ×8/1e6、UDP bits/s /1e6；UDP 帧数 = 帧率 × 时长 | 与事件数值完全一致 |
| B3-C05 | 判定分支在单机上重跑一遍（同 A1-C06 的取法） | 同 A1-C06 |

### B4 稳定性（约 8 h 浸泡过夜 + 2 h）

| ID | 内容 | 预期 |
|---|---|---|
| B4-S01 | 8 h 混合浸泡（发布候选则 24 h）：ping / iperf TCP/UDP / CTS TCP/UDP，单向与双向轮换，含至少一个 30 min 长单元 | 满足 3.3；每轮记录 run_id、完成数、预期数、错误分类 |
| B4-S02 | 100 次 API 起停（脚本化） | 同 A1-S02；第 100 次时资源曲线平稳 |
| B4-S03 | 运行中 `netsh wlan disconnect` 20 s，再由计划任务自动重连 | 同 A1-S07 |
| B4-S04 | 运行中禁用以太网 20 s：先切到 WLAN 地址登录 SSH，由计划任务执行「禁用 → 20 s 后启用」，另设一个 T+120 s 的兜底启用任务 | 单元结论明确；恢复后续跑正常；主机始终能连回来 |
| B4-S05 | 单元中途分别杀掉 iperf3 与 ctsTraffic | iperf3 跑过流量的判 `IPERF_EFFECTIVE_WINDOW_SHORT`，CTS 同 B1-F05；下一单元不受影响 |
| B4-S06 | `taskkill /F` 主控 | 看门狗 ≤ 5 s 回收子进程；本机 agent 侧在租约内回收；能重放、能 RESUME |
| B4-S07 | 控制台开着灌包（同 A1-U11） | 中位数下降 ≤ 5% |

---

## 7. 执行顺序与时间预算

| 顺序 | 阶段 | 预计 | 依赖 |
|---|---|---|---|
| 1 | 阶段 0 | 1.5 h | — |
| 2 | A1-F → A1-C → A1-U | 5 h | P0-05 的基线，用来推门限 |
| 3 | A1-S（含 2 h 浸泡） | 3 h | A1-F 通过 |
| 4 | A2 | 2 h | A1 通过 |
| 5 | B0 → B1 → B3 | 3 h | B0 是阶段 B 的硬前提 |
| 6 | B2 | 2.5 h | B1 通过 |
| 7 | B4（8 h 浸泡放夜间） | 10 h | B1、B2 通过 |

合计约 2.5–3 个工作日，含两次夜间浸泡。阶段之间如有源码改动，回到阶段 0 重新冻结。

## 8. 环境改动与清理清单

每个阶段结束都执行一次清理，并写 `cleanup.json`。只清理本轮 owner 的资源，不动无关进程。

| 端 | 改动 | 恢复方式 |
|---|---|---|
| Windows | 防火墙规则组 `CPE-Full` | `Remove-NetFirewallRule -Group CPE-Full` |
| Windows | 计划任务 `CPE-Full-*` | 逐个注销，确认列表为空 |
| Windows | QoS 策略 `CPE-AC-*` | `Remove-NetQosPolicy -PolicyStore ActiveStore` |
| Windows | 电源与睡眠设置 | 按 P0-04 存档值还原 |
| Windows | pktmon | `pktmon stop`，删除筛选器 |
| Windows | WLAN / 以太网 | 确认都已连接、地址与 P0-03 一致 |
| Mac | pf 锚点、dummynet 管道 | `sudo pfctl -a com.apple/cpe-test -F all`；`sudo dnctl -q flush`；pf 启用状态还原到 P0-04 记录值 |
| Mac | en0 别名、Wi-Fi 电源 | 删除别名；Wi-Fi 打开并重新关联 |
| 两端 | agent、主控、caffeinate | 全部退出；29880–29885、56000–56400 无监听；无 iperf3/ctsTraffic/cpe_test 残留 |

## 9. 证据与记录

- 证据根目录：Mac 侧 `<验收证据目录>/full-plan/<用例 ID>/`，Windows 侧 `C:\CPE-Acceptance\full-plan\<用例 ID>\`。每个用例目录保存：配置、计划、run 目录（rows.jsonl / meta.json / HTML / XLSX / NIC CSV / 工具原文）、OS 计数快照、复算结果 JSON、命令及退出码。
- 每个用例记录：ID、状态、快照哈希、run_id、预期、实际、偏差说明、证据路径、缺陷 ID。
- 缺陷登记为 `D-yyyymmdd-nn`，内容包括复现步骤、最小配置、原始证据，以及「用例 FAIL」与「产品判定」各是什么。修复走 AGENTS.md 的四条门禁，回归测试要先在旧实现上红、再在修复后绿；修完重新冻结。
- 不提交、不推送，除非另行要求。验证记录写进仓库时只写「被测 CPE」，SSID 和厂商相关字样都不写。

## 10. 上午三轮遗留的缺口与本方案中对应的用例

| 缺口 | 对应用例 |
|---|---|
| IPv6 真流量从未跑过 | A1-F01/F02/F03、A2-F01、B1-F01～F03 |
| CTS 有效窗口不足（6 项 NOT_EVALUATED） | B1-F03（先定位根因）、B2-02 |
| CTS 行缺 RX 中位/P95/最小/最大 | B1-F04 |
| Wi-Fi 大包 ping 只发 3 包就下结论 | B1-F01（100 包）、A1-C10 对照实验 |
| Wi-Fi 接收端 UDP RX 与理论封装开销对不上 | A1-C03 |
| Windows 出厂参数（256m 等）没跑过 | B2-01/02 |
| 长稳、反复起停、物理断链恢复 | A1-S01/S02/S07、B4-S01～S04 |
| 内环与组合场景没有实机覆盖 | A2-F03/F04/F05 |
| 现代套件页面的编辑流程没有走全 | A1-U04～U09 |
| Windows 作为 agent 没测过 | A2 整轮 |

本方案仍覆盖不到、需要另外的硬件：第二台 Windows（双 Windows 跨机 CTS）、Windows 10、RNDIS / 10GUSB / 2.5G 实速网口、Wi-Fi 6 GHz。执行完成后这些在报告里记为 `BLOCKED`，并写明缺的是什么设备。
