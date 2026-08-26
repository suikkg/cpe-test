# cpe_test v4.3 设计方案：判定口径修复、链路策略模型与本地控制台

> 起点版本 v4.2.7（commit 51e854c）。
> 全部缺陷结论锚到源码位置，并以实测运行 `run_20260825_215915_7684` 为证
> （120 单元 / 160 方向 / 6h47m，产物 199 CSV · 160 raw log · 262 截图）。

---

## §0 变更全景

缺陷编号 D1–D6 各自成节，功能编号 F1–F3 对应三项新需求。
「改动位置」是主要落点，不是全部触及文件。

| 编号 | 内容 | 级别 | 本轮受影响 | 改动位置 |
|---|---|---|---|---|
| D1 | 双向 UDP 共同窗口把跑通的方向一起判废 | P0 | 8 行 / 4 单元 | `executor.rs:6142` |
| D2 | iperf3 收尾握手失败，丢弃已测到的网卡数据 | P0 | 10 行 | `executor.rs:3679` |
| D3 | UDP 丢包率取错行且漏配 `1e+02%` | P0 | 6 行报成 0.000% | `iperf.rs:169–209` |
| D4 | `-b` 不按路径能力裁剪，1G 收端被灌 2.6G | P0 | 80 条 UDP 命令 | `builder.rs:1203` |
| D5 | `-w 256m` 让「工具自报发送」恒定虚高 119 Mbps | P1 | 65 行 TCP 全部 | `config.rs:152` |
| D6 | 计数器停滞报成 100% 覆盖率；链路失联不熔断 | P0 | 尾部 6 单元 / 21 分钟 | `monitor.rs` · `executor.rs` |
| F1 | 每单元前重新拉取辅测网口，替代启动时快照 | P1 | — | `ui.rs:198–212` |
| F2 | 角色兜底 + 单口覆盖的 RX 门限 / UDP 带宽模型 | P1 | — | `config.rs` · `rate.rs` |
| F3 | 本地 Web 控制台（勾选执行 + 实时进度） | P2 | — | 新增 `master/webui` |

### 贯穿全案的一条原则

「接收端 OS 网卡计数器是唯一正式口径，工具自报只作诊断」这条铁律本身没问题，
问题是它在四个地方被架空了：正式口径已经测到，却被诊断口径的失败（D2）、
对向腿的失败（D1）、缺失的裁剪（D4）和失真的可信度指标（D6）拖着一起作废。

下面每条修复都在把判定权**还给**网卡计数器，而不是削弱它。

---

## §1 P0 判定口径修复

### D1 · 双向 UDP：一条腿失败，另一条腿的有效数据被连坐作废

**现象**

任务 10 / 12 / 34 / 36 共 8 行，报表写 `接收端 RX 平均 = 未采集`、
`采样覆盖率 0.0%`、`NOT_EVALUATED`；而同一次运行落盘的 CSV 里，
这些方向的接收网卡跑了整整三分钟。

```
# nic_samples_unit-7684-33-34-…_agent_以太网.csv
# endpoint,辅测
# interface,以太网
# full_lifecycle_seconds,209.976588
# full_lifecycle_average_rx_mbps,923.080469   ← 208 个样本，205 个有流量，峰值 939.87
```

```
# master.log:10302
有效窗口: 0.0s / 180s（不足，不能正式判定）
[ab] 模式=Observe…流成功=1/1…RX均值=-，覆盖率=0.0%，结果=NOT_EVALUATED
[ba] 模式=Observe…流成功=0/1…RX均值=-，覆盖率=0.0%，结果=RATE_FAIL
```

**根因**

`select_udp_effective_window()`（`executor.rs:6142`）为整个单元求**一个**窗口，
四处跨腿收敛：

- `:6177–6178` — `lower = lower.max(…)` / `upper = upper.min(…)`，逐腿收窄同一区间
- `:6193` — `plans.iter().enumerate().all(…)`，要求每个时刻*每条腿*都有足够活跃流
- `:6159` — 任一腿目的端 monitor 缺失就 `return` 全零窗口。任务 10 的
  `(网卡监控停止失败: 监控 ID 不存在: mon11)` 正是走这条
- 结果在 `:4470` 赋给整单元，`:4597` 对**每条腿**判 `EFFECTIVE_WINDOW_SHORT`

于是 ba 腿单流 UDP 三次重试全挂 → 交集为空 → ab 腿那 923 Mbps 一起归零。
附带后果是 ab 腿早已结束、ba 腿还在重试，「双向并发」实际退化成两次串行
（任务 10 跑了 556.5 秒）。

**改法**

1. **窗口降级为腿级。** `select_udp_effective_window` 返回 `Vec<EffectiveWindow>`，
   每腿按自己的 `min_concurrent_streams` 求活跃区间，互不相干。`:6159` 的
   monitor 缺失只让该腿归零。
2. **新增单元级 `concurrency_window`**（各腿交集），*只用于标注，不参与任何一条腿的
   verdict*。
3. **把语义显式化而不是把数据丢掉。** 报表该行 reason 追加：
   `并发重叠 0.0s（对向 ba 未跑通，本行为单向条件下的实测）`。
   923 Mbps 是真的，但它不是在双向压力下测的——这两件事都要说，而现在两件都没说。
4. 「双向并发」单元新增执行前置检查：两腿 server 全部就绪后才同时放流；任一腿在
   `startup_timeout_secs` 内没起来，整单元直接判 `SETUP_ERROR` 并跳过，
   不再让另一腿空跑三分钟。

**验证**

新增单测 `bidirectional_udp_keeps_good_leg_when_peer_leg_fails`：ab 腿灌入本轮
unit-33-34 的真实 208 条样本、ba 腿零流，断言 ab 腿 `Verdict::Measured` 且
`avg_mbps ≈ 923.08`，同时 `concurrency_secs == 0.0`。

---

### D2 · iperf3 收尾握手失败，把跑完的 180 秒一起丢掉

**现象**

10 行判 `SETUP_ERROR`，其中 9 行网卡实测 125–1067 Mbps 已经算出来了。
错误全部发生在 180 秒灌包**结束之后**的结果交换阶段。

| 任务 | 链路 | 网卡实测 | 报表接收 | 判定 |
|---|---|---:|---:|---|
| 103 | 主控 WLAN → 主控 以太网 5 | 1067.902 | 0.000 | SETUP_ERROR |
| 107 ba | 主控 WLAN → 主控 以太网 5 | 970.397 | 0.000 | SETUP_ERROR |
| 105 ba | 主控 WLAN → 主控 以太网 5 | 949.447 | 0.000 | SETUP_ERROR |
| 99 | 主控 以太网 5 → 主控 WLAN | 692.793 | 0.000 | SETUP_ERROR |
| 37 | 主控 以太网 5 → 辅测 WLAN 3 | 497.608 | 0.000 | SETUP_ERROR |

**根因**

`executor.rs:3679` 把 `raw_ok == false` 放在**第一个**分支，优先于一切：

```rust
// rx_stats / nic_samples 就在上面几行算完了……
let (verdict, reason_code, reason_detail) = if !raw_ok {
    (Verdict::SetupError, "IPERF_EXEC_FAILED".to_string(), …)
} else if !measurement { … }
```

典型错误文本（发生在 180s 之后）：

```
iperf3: error - unable to send control message - port may not be
available, the other side may have stopped running, etc.:
Connection reset by peer
```

它把「环境根本没搭起来」和「跑完了但最后一次控制面握手失败」混成同一类。
前者性能结论无意义，后者*正式口径已经拿到了*。

**改法**

按「是否已经产生过有效吞吐窗口」拆分：

```
raw_ok == false
├─ effective_window.complete == false  →  SETUP_ERROR / IPERF_EXEC_FAILED （真没起来）
└─ effective_window.complete == true   →  判定完全交回 evaluate_nic_rx
                                          · RX 达标 → MEASURED / PASS
                                          · RX 低于目标 → 仍然 RATE_FAIL
                                          · RX 为 0 / 采样不可信 → 仍然 NOT_EVALUATED
                                          · reason_detail 前缀 IPERF_SUMMARY_LOST
                                          · ExecutionStatus 保持 Error
```

判据用现成的 `effective_window.complete`（「有效窗口 ≥ 要求时长」），
不引入新的阈值常量，也不匹配 iperf3 的错误文本——措辞会变，窗口不会。

关键约束：**降级路径不得升级任何一个失败判定**。verdict 和 reason_code
仍然完全由 `evaluate_nic_rx` 产出，`IPERF_SUMMARY_LOST` 只进 detail，
这样 `RX_BELOW_TARGET`、`NIC_RATE_MISSING` 这类信息不会被覆盖掉。
概览列显示成「MEASURED · ERROR」，一眼能看出工具侧出过问题。

**落地**（已完成）

- `executor.rs` 抽出纯函数 `iperf_flow_verdict(IperfFlowVerdictIn)`，
  整条判定链可单测
- 单测 `client_tail_failure_after_full_window_keeps_nic_verdict`（任务 103 的
  1067.902Mbps）、`tail_failure_downgrade_never_upgrades_a_failing_rate`
  （RX 低于目标仍 RATE_FAIL；任务 115 那种全零仍 NOT_EVALUATED）、
  `client_failure_before_a_full_window_is_still_a_setup_error`

---

### D3 · UDP 丢包率：取的是最后一个残帧区间，且漏配科学计数法

**现象**

71 个可比对方向中，6 个把真实 99.68–99.98% 的丢包报成 `0.000%`。

```
# iperf_raw_unit-7684-33-34-…_udp_ab.log — server 段尾部
[  5] 203.01-204.01 sec   28.0 KBytes   37544/37546 (1e+02%)
[  5] 205.00-206.01 sec   14.0 KBytes   29235/29236 (1e+02%)
[  5] 206.01-206.56 sec   0.00 Bytes    0/0 (0%)          ← 报表取了这一行
- - - - - - - - - - - - - - - - - - - - - - - - -
[  5] 0.00-206.56 sec  14.4 MBytes  3035698/3036752 (1e+02%)  receiver
```

**根因**

`cmd/iperf.rs:169–170` 的两条正则都要求百分号前是 `\d+(\.\d+)?`，
**匹配不了 `1e+02%`**；`:203` 与 `:206` 又用 `captures_iter(text).last()`
取**整段文本的最后一次匹配**——那是 server 的最后一个区间行，
常常是 `0/0 (0%)` 的残帧。

两个 bug 叠加的结果是：真正的 `receiver` 汇总行因为格式不匹配被跳过，
报表拿到的是一个 0 字节区间的 0%。注意上方 `:173–195` 的逐行循环*本来就已经在
区分 sender / receiver*，只有丢包解析跳出循环去扫全文。

**改法**

1. **不再解析百分号，改用计数。** `loss_pct = lost / total × 100`。
   整数没有格式歧义，`1e+02` 问题自然消失。
2. **把丢包解析并入逐行循环，只认 `receiver` 汇总行。** 没有 receiver 行时返回
   `None`，而不是退回 sender 侧——sender 的 `0/3054905 (0%)` 是「我全发出去了」，
   物理上恒为 0，当丢包率用是错的。
3. `total == 0` 返回 `None`，不返回 `0.0`。
4. 报表在 `None` 时写「丢包未知（未取到 server 汇总）」。
   `aggregate_udp_loss`（`executor.rs:6270`）的按计数加总逻辑本身是对的，
   上游修好即可，但要把百分比取平均那条 fallback 删掉——多流场景对百分比取
   算术平均是错误加权。

**顺带修掉的一个更大的问题**

丢包率修对之后，「`接收端 RX 平均 938.646 Mbps` + `MEASURED`」这种行会露出真面目：
网卡确实收到了 938 Mbps 的 IP 分片，但应用层只拿到 9 Mbps。

建议在 UDP 行的判定里加一条：**丢包率 > `max_udp_loss_pct`（默认给 5%）时，
verdict 不得为 MEASURED，降为 `RATE_FAIL / UDP_LOSS_EXCEEDED`**。
`max_udp_loss_pct` 字段已经在 `config.rs:234` 存在，只是默认 `null` 从未生效。

---

### D4 · `-b` 不按路径能力裁剪：丢包是配置出来的，不是测出来的

**现象**

本轮 **80 条 UDP 命令全部是 `-b 2600000000`**，包括收端是 1 Gbps 以太网的那些。

```
# 报表「实际灌包命令」列（任务 34 ab）
iperf3 -c 192.168.0.102 -B 192.168.0.100 -p 56042 -t 206 -i 1 -f m
       -4 -u -b 2600000000 -l 14k -w 256m --forceflush
              └── 收端 192.168.0.102 协商 1000 Mbps
```

**根因**

`builder.rs:1203` `let iperf_bandwidth = parsed_bandwidth.iperf_arg();`
——直接取 profile 原值。开关 `limit_udp_by_link_speed` 只在
`builder.rs:544 allowed_udp_streams_for_mbps()` 里裁**流数**，从不碰带宽。
而 `rate.rs:28 path_payload_ceiling_mbps()` 早就实现了 `min(src, dst)`，
只是没有任何地方拿它裁 `-b`。

叠加 `-l 14k` 在 1500 MTU 上要拆成 10 个 IP 分片（丢任一片则整个数据报作废）、
以及 UDP 只跑单流（TCP 是 `-P 10`），`SINGLE_UDP_STREAM_FAILED` 反复出现基本是必然。

**改法**

并入 §3 的链路策略模型：`-b` 最终值 =
`min(用户为该链路该方向填的值, path_payload_ceiling_mbps(发端, 收端))`，
并把裁剪动作写进任务标签与报表，例如 `UDP -b 1G（按 SGMII1G 收端从 2.6G 裁剪）`。

同时把 UDP 默认流数与 `-l` 一并提到界面上：单流 14 KB 数据报打 2.6 Gbps
是一个线程每秒 23 万个分片，这本身就是被测项之外的瓶颈。

---

### D5 · `-w 256m` 让「工具自报发送」恒定虚高 119 Mbps

**现象**

65 条 TCP 行，`发 − 收` 的分布是 **118.92 ± 1.90 Mbps**。
而 `10 × 256 MB ÷ 180 s = 119.3 Mbps`。
这不是测量噪声，是躺在 socket 发送缓冲里、从未上过线的 2.56 GB。

同一原因让每次开测第一秒都打出 `iperf=22271.0Mbps`；
也让任务 115 在链路已断的情况下仍「发送 2.86 GB / 136 Mbps」。

**改法**

1. 默认 `-w` 从 `256m` 降到 `4m`（`config.rs:152`）；界面上 `-w` 归入「高级」，
   当 `-w × 流数` 超过链路 BDP 四倍时给黄色提示。
2. 报表列名从「发送」改为「**工具自报发送（含未上线缓冲）**」，
   并在诊断详情补一行 `估算在途缓冲 = -w × 流数 = 2.56 GB`，让 119 这个数可核对。

判定口径不变——正式结论一直是网卡 RX，这两条只修呈现与默认值。

---

### D6 · 计数器停滞报成 100% 覆盖率；链路失联后不熔断

**现象**

04:32:40 起，辅测两块网卡的计数器**同时**停止推进，`valid=true`、`error` 为空，
一直到运行结束。

```
# nic_samples_unit-7684-114-115-…_agent_以太网.csv
elapsed_ms  rx_bytes       rx_delta  rx_mbps  valid
  6059      584989192928   69032059  547.16   true
  7078      521372261356   31257399  245.30   true
  8086      521372261356          0    0.00   true   ← 此后 193 秒不变
 …
200125      521372261356          0    0.00   true
```

报表对这一行写的是 `采样覆盖率 100.0%`。
同时工具毫无察觉地又跑了 6 个单元、21 分钟。

**改法**

1. **计数器停滞检测（已完成）。** `RateStats` 新增 `stalled_ratio`：判定窗口内
   **计数器连续零增长**的最长一段占已覆盖时长的比例。看的是原始
   `rx_delta_bytes == 0` 这个硬事实，而不是扣完背景之后的速率。
   取「最长连续一段」而非零样本总数，是为了把「起流前后各空几秒」（正常，
   分散短段）和「中途卡死」（异常，一整段）区分开。

   门槛与采样覆盖率共用 `MIN_RATE_SAMPLE_COVERAGE`：窗口里至少 95% 的时间
   要有真实推进的计数，否则判 `NOT_EVALUATED / COUNTER_STALLED`。

   这一条**排在 `NIC_RATE_MISSING` 之前**：停滞场景里 avg 通常也是 0，会被
   「没有可用速率」抢先吃掉，而「采到样本但计数器不动」比前者具体得多。
   `evaluate_nic_rx`（TCP/CTS）与 `run_udp_unit` 的内联链（UDP）两处都加，
   避免同一种故障在两条判定链上写出两种原因码。

   注意这不是「把 100% 覆盖率改成低覆盖率」——样本确实一条不缺，覆盖率本来
   就该是满的。停滞是与覆盖率**正交**的另一种不可信，需要单独的指标和原因码。
2. **接口身份校验。** `MonitorStartReq`（`protocol.rs:396`）新增可选
   `expect_ifindex`；agent 侧在 start 时校验别名对应的 ifindex 是否一致。
   现在 monitor 只按别名字符串找接口（`nic/monitor.rs:46`），
   换了网卡而别名相同就会静默采错一块。
3. **链路健康熔断。** 单元级维护「连续零流量单元」计数（默认 K = 2）。触发后暂停队列并询问：
   `连续 2 个单元接收端零流量，可能是被测设备已失联。[继续] [跳过剩余] [结束并出报告]`
   `--auto` 下默认「跳过剩余」，并在报告顶部打横幅。
4. 报告新增「**运行健康**」区块：熔断点、被跳过的单元、以及
   「自 04:32:40 起辅测两端计数器同时停滞」这类事实。

**关于这一轮尾部的六个单元**

115–120 的全零已确认由被测载体导致，不计入工具缺陷：`valid=true`、
tx 侧仍有零星控制面字节、iperf 同步变成 `Connection timed out`，采样器本身正常。

上面第 1、3、4 条要解决的不是「为什么会零」，而是
**「零的时候工具应该说什么、应该停下来做什么」**——
现在它说的是 100% 覆盖率，做的是继续跑 21 分钟。

---

## §2 F1 · 辅测网口按单元重扫

「辅测机只启动一次，但每轮执行都重新获取网口」——agent 侧本来就是对的，
问题全在主控的缓存。

```rust
// src/agent/server.rs:367 — agent 每次调用都实扫，无缓存
(Method::Post, "/info") => {
    …
    ok_json(scan_host(&prefixes))     // ← 每次都重新扫，这里没问题
}
```

```rust
// src/master/ui.rs:198–212 — 主控只扫一次，之后一路按引用传下去
logln("正在扫描本机网卡...");
let master_info = scan_host(&cfg.ipv4_prefixes);
logln("正在获取辅测机网卡...");
let agent_info = match agent_info(&agent_host, cfg.agent_port, …) { … };
// 此后 6 小时 47 分钟内不再更新，builder 和 executor 全程用这份快照
```

**改法**

1. **新增 `refresh_topology()`**，在三个时机调用：GUI 点「刷新网口」、
   **每个测试单元开始前**（一次 RPC，约 10 ms，相对 180 秒的单元可忽略）、
   agent 断线重连之后。
2. **网卡指纹 `NicIdentity { name, ifindex, mac, ipv4 }`**。每单元开始前比对上一轮快照：
   - **IP / ifindex 变了** → 用新值重建该单元的 iperf 目标地址与 monitor iface，
     日志记 `拓扑变更`
   - **网卡消失** → 该单元判 `SETUP_ERROR / NIC_DISAPPEARED`，
     不再对着不存在的接口起 monitor
   - **协商速率变了** → 重新推导该单元的 `-b` 上限与 RX 门限，报表该行注明
     `协商速率 2882 → 1441 Mbps，门限已按新值重算`
3. 快照进 resume 状态。断点续跑时先重扫再比对，避免拿旧 ifindex 接着跑。

**为什么这条对本轮尤其重要**

辅测机的 WLAN 3 是 Wi-Fi 7 BE200 320MHz，协商速率在一轮 6 小时的测试里
会随信道条件浮动。用启动那一刻的 2882 Mbps 去推导整轮的 `-b` 与门限，
后面几十个单元的基准就是错的——而这个错误在报告里完全不可见，
因为报告打印的也是那份快照。

---

## §3 F2 · 两层链路策略：角色兜底 + 单口覆盖

角色层配一次可跨机器复用、条目少（10 个配对 vs 120 个任务）；
但同一角色的两块网卡实测能力可能差很多——本轮辅测 WLAN 3 是 Wi-Fi 7 BE200，
主控 WLAN 是另一颗，都归 `WIFI5G` 却不是一回事。
所以角色层给默认值，单口层留覆盖口。

### 配置 schema

新增 `link_profiles` 节点，完全向后兼容——不写就走现有内置推导。

```json
"link_profiles": {
  "by_role": [
    { "pair": "SGMII2.5G<->SGMII1G",
      "rx_target_mbps": { "ab": 900,    "ba": 900    },
      "udp_bandwidth":  { "ab": "1G",   "ba": "1G"   } },
    { "pair": "SGMII2.5G<->WIFI5G",
      "rx_target_mbps": { "ab": 1600,   "ba": 1600   },
      "udp_bandwidth":  { "ab": "2.6G", "ba": "2.6G" } }
  ],
  "by_nic": [
    { "host": "agent", "name": "WLAN 3", "ipv4": "192.168.0.104",
      "as_receiver": { "rx_target_mbps": 1800 },
      "as_sender":   { "udp_bandwidth":  "2.8G" } }
  ]
}
```

### 解析优先级

一条链路、一个方向，最终值这样定：

```
RX 门限   by_nic[收端].rx_target_mbps
      ↓ 缺省   by_role[配对].rx_target_mbps[方向]
      ↓ 缺省   rate_check.targets_mbps[方向]        （现有全局）
      ↓ 缺省   auto_evb_target_mbps()               （现有内置，rate.rs:39）
      ↓ 缺省   None → Observe 模式，只记录不判合格

UDP -b   min(
           by_nic[发端].udp_bandwidth
             ?? by_role[配对].udp_bandwidth[方向]
             ?? udp_profiles[].bandwidth          （现有全局档位）,
           path_payload_ceiling_mbps(发端, 收端)   ← rate.rs:28，修 D4
         )
```

**两者都必须按方向独立。** 本轮 `以太网 6 → WLAN 3 = 1821 Mbps`，
反向 `WLAN 3 → 以太网 6 = 17 Mbps`——同一条链路两个方向差 100 倍。
用一个门限卡两个方向没有意义，所以 `rx_target_mbps` 和 `udp_bandwidth`
都是 `{ ab, ba }` 结构而不是标量。现有的 `RateTargets`（`config.rs:191`）
已经是这个形状，沿用即可。

### 落在代码上

- `config.rs` 新增 `LinkProfiles / RoleProfile / NicProfile`。
  `RateTargets` 保持不动，继续做全局兜底。
- `rate.rs` 新增纯函数
  `resolve_link_policy(src, dst, dir, profiles, cfg) -> LinkPolicy`，
  返回 `{ rx_target_mbps: Option<f64>, udp_bandwidth_bps: u64, clipped_from: Option<String> }`。
  上面两张优先级表就是这一个函数，可以单独测。
- `builder.rs:1203` 改用 `LinkPolicy.udp_bandwidth_bps`；
  `builder.rs:534–535` 的 `rate_targets` 改用 `LinkPolicy.rx_target_mbps`。
- `clipped_from` 进任务标签与报表——「你填了 2.8G 但被 1G 收端裁到 1G」
  这件事必须在报告里看得见，否则又是一个静默改写。

### 验证

`resolve_link_policy` 的优先级矩阵单测：单口覆盖 > 角色 > 全局 > 内置 > None
五级各一例，加裁剪生效/不生效两支。这是纯函数，不需要跑网络。

---

## §4 F3 · 本地 Web 控制台

双击 `cpe_test.exe` → 起一个 `tiny_http` 服务在 `127.0.0.1:28800`
（`cpe_test ui --port N` 可改）→ 吐出内嵌单页 → 调系统默认浏览器打开。
零新依赖，单 exe 不变，三平台 CI 不受影响。

**为什么不是原生窗口**

这个界面的主体是三样东西：配对 × 方向的勾选矩阵、一张可编辑的门限/带宽表格、
一条实时进度流。在 HTML 里都是原生控件；在 egui 或裸 Win32 里都要手搓表格与
可编辑单元格。而项目已经依赖 `tiny_http`（agent 一直在用），
HTML 报告那套样式也能直接复用。原生窗口方案会把依赖树从 12 个 crate 涨到 150+、
exe 从 3 MB 涨到 25 MB+，换来的只是少一个浏览器窗口。

### 页面结构

一页五段，从上到下就是操作顺序；第五段在开始执行后显示。

```
┌─ CPE 子网测试 ─────────────────────────────────────────────────────┐
│                                                                    │
│  ① 连接                                                            │
│     辅测机 [10.228.46.50    ] : [28801] token [        ]           │
│     [连接]  [刷新网口]                                             │
│     ✓ ADZC9100022 (windows) · agent v4.3.0 · iperf 3.20 · CTS 可用  │
│                                                                    │
│  ② 网口与策略                         RX门限       UDP -b          │
│     主控  以太网 6  SGMII2.5G  2500M  [1800]Mbps    [2.6G]         │
│     主控  以太网 5  RNDIS      3750M  [    ]        [    ]         │
│     辅测  以太网    SGMII1G    1000M  [ 900]Mbps    [  1G]         │
│     辅测  WLAN 3    WIFI5G     2882M  [1600]Mbps    [2.6G]         │
│                                                                    │
│  ③ 测试矩阵            [全选] [全不选] [只选跨机]                  │
│     ┌──────────────────────────┬──────────────┬─────────┬──────┐  │
│     │ 配对                     │ 方向         │ 协议    │ IP   │  │
│     ├──────────────────────────┼──────────────┼─────────┼──────┤  │
│     │☑ SGMII2.5G↔SGMII1G       │☑A→B ☑B→A ☐双向│☑TCP ☑UDP│☑v4   │  │
│     │☑ SGMII2.5G↔WIFI5G        │☑A→B ☑B→A ☐双向│☑TCP ☑UDP│☑v4   │  │
│     │☐ 主控同机 2.5G↔RNDIS     │              │         │      │  │
│     └──────────────────────────┴──────────────┴─────────┴──────┘  │
│                                                                    │
│  ④ 执行                                                            │
│     TCP -w [2m 4m 256m]  TCP -P [1 5 10]  UDP -b [1m 500m 1G]     │
│     时长[180]s  UDP流[1]  ☑截图                                    │
│     [下载 config.json] [预览任务]                    [开始测试]     │
└────────────────────────────────────────────────────────────────────┘
```

### 运行时视图

点「开始测试」后同页显示实时 `nic-rx / iperf / err` 日志（直接复用现有
`[灌包进度]` 输出）和 `[结束并出报告]`。停止请求复用进程级取消信号，执行器会清理
当前资源并生成部分报告；跑完给一个真正调用系统默认浏览器的「打开报告」按钮。

`暂停` / `跳过当前` 没有在 v4.3.0 放一个只能改外观的假按钮：现有执行器只有整轮取消
协议，要安全支持这两项必须给当前单元和远端作业增加独立控制状态，留待后续版本。

### 接口

主控自己的 UI 面，和 agent 的 `/info` 等不是同一套，也不监听外网地址。

| 方法 / 路径 | 入参 | 返回 |
|---|---|---|
| `GET /` | — | 内嵌单页（HTML + CSS + JS 全内联） |
| `GET /api/bootstrap` | — | 不含 token 明文的启动配置 |
| `POST /api/connect` | `{ host, port, token }` | HealthOut + 两端 HostInfo |
| `POST /api/plan` | 勾选状态 | 生成的单元列表 + 预计耗时（**不执行**） |
| `POST /api/config` | 勾选状态 | 可下载的完整 config JSON |
| `POST /api/run` | 勾选状态 | `{ started }` |
| `GET /api/progress` | `?from=绝对日志游标` | 新日志、运行状态和报告路径 |
| `POST /api/stop` | — | 请求优雅结束并生成部分报告 |
| `POST /api/open-report` | — | 用系统默认浏览器打开报告 |

### GUI 不是第二条执行路径

「开始测试」做的事就是：**把界面状态序列化成一份 config，然后调用现有的
`run_master()`**。GUI 只是 config 的图形编辑器加进度视图。

这是刻意的——CI 的 `--auto` 回归防线、现有的 `configs.json` 用法、
以及 resume 断点续跑，全都不动。`main.rs:48` 的 mode 分发新增 `ui` 分支，
空参数从 `""` 的交互式 CLI 改为默认走 `ui`；`cpe_test master` 仍进现有交互式流程。

### 给别人用的那一公里

- 界面上每个数字旁标出它是**你填的**还是**自动推导的**，改过的格子高亮
- `[下载 config.json]` 让老手仍然能拿去跑 `--auto`；`ui --config` 会载入 agent、
  全局档位和已有 `by_nic` 策略，token 只保留在后端，不以明文回显
- 错误直接写人话：「辅测机 10.228.46.50:28801 连不上。请确认对方已双击
  `start_agent.bat`，且 28801 端口在防火墙放行」，而不是抛一个 IO error
- 「预览任务」在执行前给出单元清单和预计耗时——本轮 120 个单元 6 小时 47 分，
  这个数字必须在点下去之前能看到

---

## §5 落地顺序与验证

| 阶段 | 内容 | 说明 |
|---|---|---|
| **Phase 1** ✅ 已完成 | D3 丢包解析 · D2 收尾降级 · D4 `-b` 裁剪 · D6-1 停滞检测 | 全是局部函数改动 + 单测，348 tests / fmt / clippy 全绿 |
| **Phase 2** ✅ 已完成 | D1 腿级窗口 · D6-2 链路健康告警 · F1 按单元重扫 | 触及 executor 判定主循环 |
| **Phase 3** ✅ 已完成 | F2 两层链路策略 · D5 缓冲提示与在途估算 | 新 schema 向后兼容，老 config 行为不变 |
| **Phase 4** ✅ 已完成 | F3 Web 控制台 | 复用 tiny_http，`cpe_test ui`；双击默认进这里 |

### 实施过程中相对原方案的修正

原方案里三处判断在落地时被证明是错的，已按实际改法更新对应章节：

1. **D2 不覆盖 verdict/reason_code。** 原写「判成 MEASURED / IPERF_SUMMARY_LOST」，
   那会把 `RX_BELOW_TARGET` 这类信息盖掉。改为判定完全交回 `evaluate_nic_rx`，
   `IPERF_SUMMARY_LOST` 只进 detail，执行状态保持 `Error`。
2. **D6-1 不是「把样本标 invalid 让覆盖率掉下来」。** 样本一条不缺，覆盖率本来
   就该是 100%；停滞是与覆盖率**正交**的另一种不可信，需要自己的指标
   （`stalled_ratio`）和原因码（`COUNTER_STALLED`）。
3. **D6-2 默认不中止。** 「连续零测量」区分不了「设备掉线」和「其中一对网口
   本来就不通」，后者在多配对批量测试里很常见，自动中止会把别的配对一起砍掉。
   改为默认只告警 + 报告顶部留痕，`abort_after_dead_traffic_units` 可选开启。

### 实施过程中新增的三项

| 项 | 起因 | 落点 |
|---|---|---|
| WiFi 上限不跟协商速率 | WiFi 的协商值是 PHY 速率，同一块网卡在 2402/2882 之间来回跳；跟着它裁 `-b`，相邻两个单元的灌包强度都不一样 | 新增 `wifi_payload_ceiling_mbps`（默认 2800），WIFI* 角色不再读协商值 |
| 显式配置豁免裁剪 | `link_profiles` 里专门为一条链路写下的带宽是明确判断，自动裁剪不该推翻它；裁剪是给没配过的链路兜底的安全网 | `udp_load_for_leg` 增加 `explicit` 参数 |
| 窗口不足也要给出速率 | 「这一行不作数」和「这一行什么都没测到」是两回事；任务 97 的网卡 202/202 个样本有流量、全程 487.1Mbps，报表却只有「未采集」 | `lifecycle_rx_hint()`，明确标注「仅供定位，不作判定依据」 |
| 报告加执行序号 | 120 个单元里同名标题会重复十几次，拿控制台记录去报告里对不上 | 概览新增 `#` 列、明细区 `#N`，与控制台 `[N/总数]` 一致 |

### 最低成本、最高收益的第一刀

**D3。** 把丢包解析从「全文最后一个百分号」改成「`receiver` 汇总行的 `lost/total`」
——大约 15 行代码，直接把 6 行 `0.000%` 变回 99.7–99.98% 的真值，
并让「99% 丢包却判 MEASURED」这类行第一次变得可见。
它不改任何结构，不需要夹具，改完当天就能重跑验证。

### 回归夹具

把本轮几个关键单元的产物收进 `tests/fixtures/` 作为判定口径的黄金样本。
这一轮数据的价值在于它**同时包含了五种形态**，是现成的、真实的、覆盖面完整的回归防线：

| 形态 | 夹具来源 | 护住的缺陷 |
|---|---|---|
| 正常满速 | `unit-7684-0-1`（TCP 938 Mbps） | 基线，防回归 |
| 满丢包 + 科学计数法 | `unit-7684-33-34 udp_ab` | D3 |
| 计数器停滞 | `unit-7684-114-115 agent CSV` | D6-1 |
| 收尾握手失败 | `unit-7684-102-103 raw log + CSV` | D2 |
| 单腿失败 + 对向有效 | `unit-7684-33-34 两个 CSV` | D1 |

### 手工验收：改完之后同一份配置应该看到什么

- UDP 行的「丢包」列不再出现「收 0.59 Mbps + 丢包 0.000%」这种自相矛盾的组合
- 任务 34 那类单元，ab 方向报出 `923 Mbps / MEASURED`，并带上「对向未跑通」的标注；
  ba 方向仍是 `RATE_FAIL`
- 任务 103 那类单元，报出 `1067.902 Mbps / MEASURED` + `IPERF_SUMMARY_LOST`，
  而不是 `SETUP_ERROR`
- 对 1G 收端的 UDP 命令是 `-b 1000000000`，任务标签写明从 2.6G 裁剪而来
- TCP 行的「发 − 收」差值随 `-w` 下调而同步收缩，不再恒定在 119
- 链路断掉后第 2 个零流量单元触发熔断，报告顶部有横幅，不再有 21 分钟的空跑
