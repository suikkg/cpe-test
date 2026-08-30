# v6.0 判定行为变更说明（R6 / ADR-12）

> 腿级判定装配层此前有三份实现（`udp_leg_verdict` / `iperf_flow_verdict` /
> CTS 的内联 if-else），并且**已经对同一事实给出过不同结论**。收敛之后，
> 下列判定会与 v4.6.0 不同。每一条都是**向正确方向**的改动，但都会改变
> 历史 run 重跑后的 verdict 分布，所以逐条列在这里。
>
> resume 不受影响：identity 不含 verdict，缓存不失效。
> 建议上线后首轮与旧版并跑一次比对（ADR-12 的风险缓解项）。

## 变更一：Observe / Discover 下不再拿目标判 FAIL

| | v4.6.0 | v6.0 |
|---|---|---|
| UDP 腿，`rate_mode=observe`/`discover`，且有可解析目标 | `RATE_FAIL` / `RX_BELOW_TARGET` | `MEASURED` / `TARGET_UNKNOWN` |
| TCP / CTS 腿，同样条件 | `MEASURED` / `TARGET_UNKNOWN` | 不变 |

**为什么是错的**：`evaluate_nic_rx`（TCP/CTS 走的那条）开头就把 Observe/Discover
的 target 清空，而 UDP 腿是自己内联的一条等价链，全函数不出现 Observe/Discover，
直接拿 target 比。于是同一台设备的 UDP 腿判 FAIL、TCP/CTS 腿判 MEASURED。

`Discover` 尤其严重：它的语义就是**故意分阶梯灌不满**去找拐点，拿目标判它的
FAIL 是结构性误判。

**可达性比原判断更广**：`builder::leg_rx_target` 有三个来源，其中
`rate_targets_bidir` 与网口策略 `policy.rx_target_mbps` 是**直接返回**的，
不经过 `rate::resolve_target_mbps` 的模式清空。所以不需要「显式配 observe
且能解析出目标」这种巧合，双向门限或单口策略配了目标就会命中。

**收敛处**：`rate::effective_rate_target` —— 全仓唯一定义，三条链都经过它。
`rate::resolve_target_mbps` 也改为调用它，不再自带一份 `match`。

回归测试：`observe_and_discover_never_fail_a_leg_against_a_target`、
`clearing_the_target_also_clears_the_offered_floor`。

## 变更二：「工具没产生吞吐测量」统一为 SETUP_ERROR

| 链 | v4.6.0 | v6.0 |
|---|---|---|
| iperf 单腿 | `RATE_FAIL` / `NO_VALID_MEASUREMENT` | **`SETUP_ERROR`** / `NO_VALID_MEASUREMENT` |
| UDP 组（0 条流成功） | `SETUP_ERROR` / `NO_STREAM_STARTED` | 不变 |
| CTS | `SETUP_ERROR` / `CTS_NO_MEASUREMENT` | 不变 |

**为什么要统一**：`SETUP_ERROR` 与 `RATE_FAIL` 在聚合优先级、处置建议、
`RunSummary` 的计数器归属上都不同——同一件事被记成两类结论，不是措辞差异。

**为什么统一到 SETUP_ERROR**：iperf3 干净退出却没有 rate/bytes，分不清
「链路真的一个字节没过」和「结果交换失败没读到」。在分不清的时候声称是 CPE
的错，正是这套判定一直在防的误判方向；速率的权威口径本来也是网卡计数器，
不是 iperf3 的自报值。

**副作用（有意）**：这类单元现在计入 `traffic_setup_errors` 而不是 `fail`，
因此更容易触发「连续零测量」熔断与自动诊断 ping —— 一轮产不出测量的运行
本来就该被拦住。

**不在统一范围内**：单流 UDP 用尽重试仍不通仍判 `RATE_FAIL`
（`SINGLE_UDP_STREAM_FAILED`）。那是「该方向必须灌通」的契约被破坏，是有意的。

回归测试：`no_measurement_is_a_setup_error_on_every_chain`。

## 变更三：TCP / CTS 链获得「发送端没灌够」的防误判

| | v4.6.0 | v6.0 |
|---|---|---|
| CTS/iperf 腿：TX-P10 < 目标+余量，且 RX ≈ TX | `RATE_FAIL` / `RX_BELOW_TARGET` | `NOT_EVALUATED` / `OFFERED_LOAD_LOW` |
| UDP 腿，同样条件 | `NOT_EVALUATED` / `OFFERED_LOAD_LOW` | 不变 |

**为什么是错的**：`offered_floor` / `offered_shortfall_explains_rx` /
`OFFERED_LOAD_LOW` 此前**只存在于 UDP 链**；`evaluate_nic_rx` 只查 TX
**覆盖率**、不查 TX **水平**。于是 CTS UDP 单流灌不满时 `RX < target` 直接判
`RX_BELOW_TARGET`——正是 UDP 链两个单测拼命要防的「把发送端瓶颈写成 CPE
性能失败」，在 CTS 路径上零防护。

**收敛处**：`rate_window::offered_shortfall_explains_rx` /
`offered_floor_mbps` / `RX_TRACKS_TX_RATIO`，三条链共用同一个算法和同一个余量。

**这个闸只在 RX 没达标时开火。** 它的全部作用是「解释缺口」，没有缺口时它无话
可说。收敛的第一版把它架在 `rx_avg < target` **外面**（照抄了 UDP 链原本的
if-else 次序），于是它在 RX 已经达标时也开火：TCP 不限速，链路上限贴着目标时
TX-P10 落在「目标 ~ 目标+余量」之间是常态，一条达标的腿就被判成
`NOT_EVALUATED` / `OFFERED_LOAD_LOW`。

拿本文档反复引用的那次 run 当例子：主控 WLAN 全场上限 2102、目标 2000、
余量 5%（floor 2100），TX-P10 2005、RX 平均 2014 —— v4.6.0 判 PASS，收敛的
第一版判 NOT_EVALUATED。**这是纯粹的回归，不在本次有意的行为变更之列**，
已修正（UDP 链同改，那条 if-else 从一开始就是这个次序）。

回归测试：`an_underfilled_sender_never_becomes_a_cpe_failure_on_any_chain`、
两处同名的 `a_leg_that_met_its_target_is_never_downgraded_by_the_offered_gate`
（后者还顺带断言两条链对同一组输入给出同一个结论）。

## 变更四：UDP 有效窗口获得与 iperf/CTS 相同的 100ms 容差

| | v4.6.0 | v6.0 |
|---|---|---|
| UDP 腿跑满 179.95s / 要求 180s | `NOT_EVALUATED` / `EFFECTIVE_WINDOW_SHORT` | 正常判定 |
| 同样的 TCP / CTS 腿 | 正常判定（100ms 容差） | 不变 |

50 毫秒的收尾差异不是测量事实的差异，是三条链各自决定容差的结果。
常量改名 `CTS_TIMELINE_TOLERANCE_MS` → `WINDOW_COMPLETE_TOLERANCE_MS`：
名字里不该有后端（iperf 路径一直在用那个叫 CTS 的常量，这本身就是分叉的症状）。

## 防再分叉

新增结构断言 `the_leg_assembly_contracts_have_exactly_one_definition_in_the_tree`
（`rate_window.rs`），照 `verdict_priority_has_exactly_one_definition_in_the_tree`
的样子扫源码：`RateMode::Observe | RateMode::Discover`、`RX_TRACKS_TX_RATIO`、
`offered_shortfall_explains_rx`、`effective_rate_target` 四个标记只允许出现在
`rate.rs` 与 `rate_window.rs`。

语义重复靠普通测试发现不了——两份实现各自都能通过自己的用例。这正是历史上
两次静默错判的形状，所以在源码层面把门关上。
