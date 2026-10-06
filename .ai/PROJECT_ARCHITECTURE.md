# CPE Test 项目架构索引（AI 专用）

> 生成基准：2026-08-11。本文服务于 AI/代码代理的源码定位，不是面向终端用户的操作手册。
> 源码是唯一权威；当本文、README 或旧说明冲突时，先读源码并更新本文。

> **引用约定**：本文只引用**模块路径与符号名**，不写行号。
>
> 本文档上一版把行号写进了正文（如 `load_config` 标注 `config.rs:228-271`），
> 到 v4.2.6 时全部失效——实测偏差 2.3×～21×（`Row` 标注 `report.rs:5-40`，实际在
> `:104`；`DbEnt` 标注 `executor.rs:786-797`，实际在 `:6628`）。这种"看似精确的
> 错误指引"比没有指引更危险：读者会直接跳到错误位置并据此判断。
> 需要定位时请用 `grep -n "fn <符号名>" <文件>`。

## 0.5 v6.0 的新模块与新不变量（本次架构变更索引）

> 下面这些是 v6.0 引入的，改动它们之前先读 `.ai/DESIGN-v6.0-architecture.md`
> 对应的 ADR，以及 `.ai/CHANGES-v6.0-verdict.md`（判定行为变更逐条说明）。

| 新模块 | 干什么 | 为什么存在（ADR） |
|---|---|---|
| `master/run_status.rs` | `RunStatus` / `UnitStatus` / `RunObserver` | 进度从「日志文本」变成结构化数据。executor 依赖 trait 不依赖 webui；CLI 传 `None` 行为零变化（ADR-2） |
| `master/executor/row.rs` | `RowIdentity` / `base_row` / `unit_row` | 报告行的**唯一**构造入口。加身份字段会让 10 个构造点全部编译失败——从「运行期空列」变成编译期错误（ADR-7） |
| `master/builder/{identity,policy,diagnostics}.rs` | resume identity、速率目标/链路策略（含 UDP 每腿负载 `udp_load_for_leg`）、诊断单元 | 从 4359 行的 `builder.rs` 里按**改动的理由**分出来。`identity` 那份是 RESUME 承重面，不许「顺手清理」（R4） |
| `master/builder/{iperf_tcp,iperf_udp,cts,ping}.rs` | 四种后端各自的单元展开（`expand_*`） | R4 的后半：`build_units` 只留外层循环、缺 IPv6 与同 /24 两道公共门禁和后端顺序。共享状态在 `builder::Expansion`（单元、计划提示、端口游标；提示经 `builder::Notices` 按首次出现去重，控制台逐规格展开后的汇总同样过它），每个「规格 × 方向 × IP 版本」组合是一个 `builder::Route`；四种后端共用的门限段只有 `Expansion::leg_rate` 一份 |
| `report/store.rs` | `rows.jsonl` + `meta.json` + `request.json` 读写 | 每单元增量落盘，报告可重放。崩溃损失从「整轮」变成「未完成的单元」（ADR-3）。`request.json` 是控制台发起这一轮时的 `RunRequest` 原文，「重新执行这一轮」唯一的输入（`meta.json` 里只有 `plan_hash`，那是摘要、反推不出计划）；命令行路径不写它 |
| `report/xlsx.rs` | `summary.xlsx` 四张表 | 第二个结果出口。**只吃类型化字段**，不许解析展示串（有结构断言）（ADR-7） |
| `master/webui/runs.rs` | `/api/runs`、`/api/runs/{id}/bundle.zip`、`/api/runs/report`、`/api/runs/request` | 远程访问者取回报告的唯一通道；报告的相对路径子资源撞「鉴权先于路由」，不能当静态站点服务（ADR-15、§13.3）。`report` 从 `rows.jsonl` 重放（**不要求先没有报告**——崩溃留下的那份可能是半截的；只挡正在跑的那一轮），`request` 回这一轮的计划原文供「重新执行」装载回控制台 |
| `ui/` | Vue 3 单文件产物 | 见 AGENTS.md §4 |

### 本轮（功能评审）新增的模块

| 新模块 | 干什么 | 为什么在这里 |
|---|---|---|
| `report/chart.rs` | 逐样本 CSV → 内联 SVG 速率曲线 | 报告一直只给 CSV 下载链接，「中途掉没掉速」得下载开 Excel 自己画。自绘 SVG 是**单文件离线**约束的直接推论（图表库违反它）。降采样按像素列压 min/max，**保峰值**——等距抽样会把掉速那一拍整个抽掉。不可信样本（`RateSample::valid`）是**断口**，曲线按 `Column::gap_before` 切成多条 `<polyline>`：画成一条贯通的会把采样中断的两端用直线连起来，一分钟没有数据的链路看上去是一段平直的健康曲线 |
| `report/compare.rs` | 两轮对比：`compare` + `render_html` | 版本回归唯一要回答的问题。**对齐键刻意不用 `Unit.id`**：那个身份含 `speed_mbps`（对 RESUME 是对的），Wi-Fi 一重协商同一条测试就成了两个 ID。这条是拿真实历史数据跑出来的。键里**含稳定性轮次**（`Row.round`；轮次 0 与 1 都归一成 1，不分轮的计划与分轮之前的历史对得上）——不含的话 N 轮在 `HashMap` 里互相覆盖只剩最后一轮，而报告不报任何错。**协商速率与 IP 也不进键**：iperf / CTS 腿取 `comparison_label`（计划里请求的档位），每条腿一项、不按流展开，不含路径裁剪、按网口策略改写的 `-b` / `-l`（`by_role` 跟着由协商速率推出的角色走，`by_nic` 按 IPv4 匹配）和 CTS 被裁剪的流数。身份带 `version`（`COMPARISON_IDENTITY_VERSION`）：版本 0（6.5.1 写下的、或从旧明细行还原的）由 `legacy_parameter` 去掉三种运行条件说明并相邻去重后再对齐，文案与归一规则由 `legacy_labels_normalize_to_the_comparison_label_the_builder_now_writes` 绑住；唯一还原不了的是 ≤6.5.1 里按网口改过 `-l` 的 UDP 单元（原档位没被记下）。报告每行显示 `identity_label`，不印序列化后的键 |
| `report/retention.rs` | `runs/` 保留策略 | `victims()` 是纯函数，删数据的逻辑不该只能靠「跑一遍看少了什么」验证。默认 `keep_runs: 0` 不删 |
| `master/executor/latency.rs` | 负载下时延探针 `LoadLatency` | 灌包腿旁并发一条 ICMP，复用 `ping::run_cancellable`。**只进诊断**——`evaluate_rx_acceptance` 在类型上就收不到它。起跑等 `wait_for_traffic`、收尾靠 `probe_stop` + 分段，见下面的不变量 |
| `master/executor.rs::DeadTrafficBreaker` | 两层熔断的纯状态机 | 分组那一层**行为上测不到**（要真实流量才显现），做成状态机才能穷举 |
| `master/executor/pmtu.rs` | 路径 MTU 探测（DF 位二分） | 现有包长档位测的是分片行为，看不出 1500 与 1492。`binary_search` 是纯函数——边界条件（全过 / 全不过 / 差一个字节）靠真链路验证不了 |
| `builder::repeat_units` / `round_scoped_id` | 稳定性轮次（最外层） | 命令行与控制台**共用这一个**：各写一份的话两边的轮次身份会漂，跨路径互相命中不了 RESUME |
| `webui::plan::apply_force_overrides` | 仅本轮强制档位 | 必须排在套件任务配置**之后**——配置本身就是覆盖项，排前面等于对配过参数的任务无效 |

**本轮新增/改动的不变量**：

- `report::verdict_totals` 是报告顶部八格与 `meta.json` 判定计数的**唯一**来源。
  它刻意**不写成 `match verdict => field += 1`**——那个形状是 `RunSummary::bump` /
  `RunCounts::bump` 的专利，`the_verdict_to_counter_mapping_has_no_third_copy` 禁止第三份。
- `cancel` 里「停止」与「跳过当前单元」是**两个独立标志**。跳过复用整轮取消那套收尾路径
  （所以也设 `RUN_CANCELLED`），单元边界靠 `resume_after_skip()` 清掉它继续；
  而 `STOP_REQUESTED` / `PROCESS_SHUTDOWN_REQUESTED` 优先，不许被那次清零抹掉。
  只有一个标志的话，「跳过之后紧接着点停止」必然被吃掉。
- `NicInfo` 的无线上下文（`wifi_ssid` / `wifi_signal_pct` / `wifi_channel` / `wifi_radio`）
  是**对外 JSON 兼容面**，全部 `#[serde(default)]`。它**刻意不并进 `NicInfo::brief()`**：
  `brief()` 参与 `Unit.title`，而标题进了
  `the_full_unit_expansion_is_byte_stable` 的全量快照，也进每一份历史报告的抬头。
- **两个导入判别器的键集必须零交集**。子网 `import::INNER_ONLY_KEYS` 与内环
  `config::SUBNET_ONLY_KEYS` 都靠「这些键对方一个都没有」来认形状；出现同名字段，
  两个导入口就开始互相拒收对方的文件，而报出来的是一句听上去很确定的错话。
  真撞上过：子网的稳定性轮次一度叫 `repeats`，而内环早就有一个。子网那个改名成
  `rounds`——语义上本来也不是一回事（内环在**最内层**重复单元，子网在**最外层**
  重复整套）。守在 `the_two_import_detectors_never_share_a_key`。
- **轮次的第 1 轮身份逐字节不变**（`round_scoped_id` 对 `round <= 1` 直接返回原值）。
  第 2 轮起才把轮次拌进哈希——端口不进身份，不拌的话同一份计划展开两遍拿到的是
  同一个 id，第 2 轮会直接命中第 1 轮刚写进去的 PASS 而整轮跳过。
- **路径 MTU 与负载下时延都只进诊断**。判定层只有一个权威（接收端 RX 对门限，
  ADR-17）；给它们开判定路径就是再造「说了算的地方」。PMTU 另有一枚
  `PING_DF_CAPABILITY`：旧 agent 会忽略 DF 位并报成功，据此得到的「大包能过」
  是个**听上去很确定的错答案**——宁可没有结果。
- **路径 MTU 探测只做 IPv4**（`pmtu::probe_path_mtu` 对 `v6` 直接返回 `Err`）。
  IPv6 协议层面没有 DF 位可设，分片只由源主机做：Windows 的 `ping -f` 文档标注
  IPv4-only、配 `-6` 时被忽略，macOS 的 `ping6` 不认 `-D`，只有 Linux 的 `-M do`
  真的生效。三条里两条会给出「大包能过」——**和旧 agent 静默忽略 DF 位是同一个
  错答案**，而 `PING_DF_CAPABILITY` 只认版本、认不出这一类，主战场又恰恰是
  Windows。按平台分叉的话，同一份报告在 Linux 主控上有 v6 结果、在 Windows 主控上
  没有，而两者都不报错。守在
  `path_mtu_probing_refuses_ipv6_instead_of_returning_a_number_it_cannot_trust`。
- **负载下时延探针必须和灌包同起同落，这是写代码保证的**。探针线程在
  `std::thread::scope` 一进去就 spawn，所以起跑要等 `latency::wait_for_traffic`
  （不等的话，server 启动 + 就绪探测 + 每流 200ms 错峰那几秒的**空载** RTT 会混进
  平均值，把探针要暴露的排队时延稀释掉）；收尾靠每条腿一个的 `probe_stop` 标志，
  整段探测按 `PROBE_CHUNK_SECS` 切段——`ping::run` 不可打断时，一条 2 秒就失败的
  腿要白等满整个计划时长，而点了「跳过当前单元」之后灌包作业被杀、探针还在 ping，
  那个按钮等于没按。agent 的 `/ping` 是同步 HTTP、没有取消通道，段边界就是它的上界。
- **「跳过当前单元」清取消位时要检查两遍**（`cancel::resume_after_skip`）。
  「先看有没有人要停，再清取消位」是 check-then-store：`request_cancel` 恰好落在
  这两步之间时，它设的 `RUN_CANCELLED` 会被那次 `store(false)` 抹掉，而
  `STOP_REQUESTED` 在执行循环里**没有第二个读者**（循环只看 `is_cancelled`）。
  拆成两个标志还不够，清完必须再看一眼。守在
  `a_stop_that_lands_inside_the_clear_window_still_stops_the_run`。
- **「要不要停」只认 `is_stop_requested`，不认 `is_cancelled`**（执行循环之外的读者）。
  跳过也会设取消位；组合场景的 `scenario_cancelled` 以前读取消位，子网阶段点一次
  「跳过」就调 `api_stop` 把整个场景停了。子网阶段结束后由 `discard_late_subnet_skip`
  丢掉落在最后一个单元收尾之后、没被消费的跳过请求。守在
  `skipping_a_subnet_unit_does_not_cancel_the_scenario_but_stopping_does`。
- **读写进程级取消位的测试必须持有 `cancel::test_guard()`**：它持有到测试线程结束、
  可重入；执行器夹具 `isolated_ctx` 自动调用。只让写标志的测试互斥不够——别人临时
  置位的取消位/退出位会让并发的执行器测试提前 break、让 `api_run` 测试拿到另一句错误。
  漏调由 `every_test_touching_cancel_flags_takes_the_guard` 按源码拦下。
- `keep_runs` / `rounds` 归在 `MASTER_CONFIG_LOCAL_KEYS`：「这台机器上留几份历史」是磁盘管理，
  不是判定参数。跟着项目走的话，别人导入这份项目会连带删掉自己的历史。

**v6.0 新增的结构断言**（都在 CI 的 `cargo test` 里）：

| 断言 | 守什么 |
|---|---|
| `every_production_row_is_built_through_the_shared_constructor` | 生产代码造 `Row` 只能走 `base_row`/`unit_row`，不许 `..Default::default()` |
| `the_leg_assembly_contracts_have_exactly_one_definition_in_the_tree` | 腿级判定装配的四个契约只能定义在 `rate.rs` / `rate_window.rs`（ADR-12） |
| `the_full_unit_expansion_is_byte_stable` | 稳定 ID / 端口顺序 / 单元展开的全量快照——**它红了先问「我是不是改了不该改的」** |
| `the_full_plan_expansion_including_commands_and_targets_is_stable` | 展开结果的全量快照：每条腿的 `-w/-P/-b/-l` 与 CTS 参数、判定模式与门限、`target_lines`、计划提示的内容与顺序。和上一条分开，因为红了的含义不同——这一条在有意改文案或参数时就该红 |
| `the_embedded_page_was_built_from_the_current_ui_sources` | 溯源戳：产物是不是从当前 `ui/` 源码构建的 |
| `every_verdict_label_round_trips` | `Verdict::label()` 与 `from_label()` 一一对应 |
| `the_summary_grid_has_one_cell_for_every_verdict` | 报告概览的统计格覆盖全部六个 verdict |
| `the_unit_summary_row_carries_every_quality_metric_not_just_two_of_them` | 「质量」列的 `udp_loss` / `tcp_retransmits` / `ping_loss` 一起上单元汇总行——`verdict_row` 优先返回汇总行，漏一个就是 HTML 概览和 Excel 上的一整列空白 |
| `traffic_units_are_counted_before_the_abandoned_link_shortcut` | 灌包单元计数排在「链路已放弃」早退分支之前，否则 `traffic_setup_errors` 会超过 `traffic_units`，收尾文案自相矛盾、诊断补跑也跟着少数 |
| `every_round_gets_its_own_row_instead_of_overwriting_the_previous_one` | 对比报告里每一轮各占一行 |
| `a_single_round_plan_keys_exactly_like_it_did_before_rounds_existed` | 不分轮的计划与历史数据算出的对齐键逐字不变 |
| `the_comparison_stylesheet_colours_every_verdict` | 对比报告的判定徽章六色齐全（只有基类时全渲染成同一个墨色，而那两列并排的意义就是看出翻转） |
| `a_sampling_outage_actually_breaks_the_line_instead_of_being_bridged` | 采样中断在曲线上是断口，不是一段被直线连过去的健康曲线 |

**判定层的单源**（ADR-12 / ADR-17，改之前读变更说明）：
`rate::effective_rate_target`（Observe/Discover 不比目标）、
`rate_window::evaluate_rx_acceptance`（**吞吐验收的唯一入口**）、
`rate_window::rx_acceptance_diagnostics`（哪些事实只作诊断）、
`rate_window::offered_floor_mbps`（目标 + 余量）、
`WINDOW_COMPLETE_TOLERANCE_MS`（有效窗口容差，三条链共用）。

### ADR-17：吞吐验收只有两层，其余全是诊断

用户确认的验收规则是**「接收端 RX 平均达到门限必定 PASS，不看其他指标」**。
在此之前它在三个地方被推翻：UDP 链把丢包门槛和 `rx_meets_target` 绑在一起、
`cts_apply_udp_loss` 会把已经 PASS 的 CTS 结果改写成 `RATE_FAIL`、
CTS/UDP 各自在 RX 比对**之前**插了流数与运行时错误分支。同一种故障因此在
TCP 和 UDP 两条路径上得到相反的结论。

现在的结构是：

1. **前提层**——这一轮能不能形成可信的接收端 RX 平均值（有效窗口、采样覆盖率、
   计数器零增长、有没有起过流）。形不成就是 `NOT_EVALUATED` / `SETUP_ERROR`。
   「有没有起过流」只问**有没有**：UDP 窗口只要求有流在跑，流数不足（配少了、
   中途掉了几条）只作诊断，和 TCP/CTS 同口径（2026-10 用户确认的口径 B）。
   计数器零增长（最长一段超过窗口 5%）由 `RateStats::stall_evidence` 分辨：
   工具侧同期也没数据在走 → 真断流，零是真实值，往下照常验收；工具侧仍在走 →
   计数器没记账；对不上时间 → 无法区分。后两种判 `COUNTER_STALLED`。
   旁证来自 `cmd::iperf_window::iperf_tool_trace`（server 逐秒行用 client 的时钟
   偏移投影；TCP 另有发送端逐秒行）与 `executor::window::cts_tool_trace`，
   **只决定这道门槛**。以前一律判 `COUNTER_STALLED`：第 10 秒就掉线的 CPE 永远
   拿不到 RATE_FAIL，报告还说「平均速率不可信」。守在
   `a_real_outage_is_judged_by_the_average_instead_of_blamed_on_the_counter`。
2. **验收层**——`evaluate_rx_acceptance(mode, target, rx_stats)`，四种结果封闭：
   无有效 RX → `NOT_EVALUATED`；无门限 → `MEASURED`；`RX >= 门限` → `PASS`；
   `RX < 门限` → `RATE_FAIL`。它**不接受**发送端参数，所以「TX 影响了判定」在类型上就不可能。

UDP 丢包、CTS 丢帧、TX 平均/P10/覆盖率、滚动窗口、中途掉速、工具退出状态、
起流数不足，全部经 `VerdictResult::diagnostics` → `Row::diagnostics` 走展示通道。

**「降级为诊断」不等于「不再告诉任何人」。** 第一版改错了方向：整套越界判据
（`rate_excursion` / `ExcursionKind` / `RateStats::series`）被 `#[allow(dead_code)]`
消音，生产代码里一个调用点都没有——一条全程平均 2200Mbps、中间整整断了一分钟
的链路报出干干净净的 PASS，报告上一个字都看不到。现在它由
`rx_acceptance_diagnostics` 消费，带原因码进诊断列。守在
`a_mid_run_outage_still_reaches_the_report_even_though_the_average_passed`。
`the_leg_assembly_contracts_have_exactly_one_definition_in_the_tree` 现在也守着
`fn evaluate_rx_acceptance` 与 `fn rx_acceptance_diagnostics` 各自只有一处定义。

### ADR-18：Wi-Fi 双向按两端 RX 合计判定

Wi-Fi↔Wi-Fi 上 AB 和 BA 的吞吐互相影响，不是相互独立的两个数，怎么分取决于
调度——要求各自达到单向门限的一半是凭空发明的约束。

**刻意不写双工模式**：工具从没探测过链路的双工能力，而这条规则也不需要那个前提。
「两个方向的吞吐不是相互独立的」是能观测到的事实，不管是传统单射频 Wi-Fi 还是
Wi-Fi 7 的 MLO 都成立；断言「半双工」会在 MLO/STR 的设备上站不住，断言有线
「全双工」更是纯推断。同理，界面与报告一律按「是不是 Wi-Fi 互测」分流，不按双工。新增
`TestSpec::rate_target_bidir_total_mbps` → `SpecNorm::rate_target_bidir_total` →
`Unit::bidir_total_target_mbps`，判定入口是
`executor::verdict_assembly::bidir_total`（同时给出判定和报告行填的合计值）：

```text
双向有效吞吐 = AB 方向接收端 RX 平均 + BA 方向接收端 RX 平均
              （两者都取两条腿同时在跑的同一段时间）
```

合计**不复用**腿级 `rx_avg`：腿级各用各的窗口（D1，一条腿失败不抹掉另一条腿），
而合计要在两条腿截断前的真实流量区间的交集上、从交集起点截出要求时长，用各自的
接收端样本（`LegOutcome::traffic`）重算。以前直接相加各自窗口的平均：一条腿起流
重试晚了十几秒，两边各混进一段单独跑的时间，Wi-Fi 上合计被抬高；重叠为零时就是
两次单独跑相加。为了让交集够长，配了合计门限的单元 TCP/CTS 两条腿各多跑
`cmd::iperf_window::BIDIR_OVERLAP_MARGIN_SECS`（`executor::traffic_process_secs`），
双向两条腿共用一个时间零点；交集不够要求时长 → `EFFECTIVE_WINDOW_SHORT`。

用两端 **RX** 相加而不是 TX+RX：同一个包在发送侧 TX 和接收侧 RX 各记一次，
相加是重复计数；TX 还会混进背景流量和 socket 缓冲里从未上线的字节。

配了合计门限时 `builder::policy::leg_rate_plan` 会把两条腿落到
`(RateMode::Observe, None)`——腿只测量，单元级比一次合计。只清门限不改模式的话，
显式配 `verify` 的用户会拿到一整轮 `TARGET_MISSING`。合计门限**必须**进 resume
identity（`push_bidir_total_identity`）：腿的 `rx_target_mbps` 是 `None`，
那条既有的「门限变了 identity 就变」的通路在这里断了。

每方向的 bidir 门限保留：非 Wi-Fi 互测按方向判定仍然正确，而且逐方向把关更严——合计只比一次总和，单方向掉速会被另一方向补上。

## 0. AI 阅读规则

1. 先从 `src/main.rs` 确认 CLI 模式，再沿调用链进入 `master/ui` 或 `agent/server`。
2. 双机 JSON 的字段、默认值和兼容性以 `src/protocol.rs`、`src/config.rs` 为准；改 HTTP 时必须同时检查 DTO、agent 路由和主控调用方。
3. 任务数量、顺序、端口和稳定 ID 是持久化兼容面。修改 `src/master/builder.rs` 前必须运行 builder 测试，并确认 RESUME 数据库影响。
4. 测试结果与报告字段由 `src/master/executor.rs` 共同定义；不要只改 `src/report.rs` 而遗漏 Row 构造。
5. 平台代码由 `cfg(windows)`、`cfg(target_os = "macos")` 和其他平台 stub 分隔。Windows 目标至少执行 `cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings` 与 MSVC 对应检查。
6. 当前 Serde 没有 `deny_unknown_fields`。配置中未声明的键会被忽略；不能根据未跟踪示例或 README 推断功能已实现。

## 1. 项目定位与构建

- 包名、版本、Rust edition 和描述在 `Cargo.toml`：crate `cpe_test`，Rust 2021。版本以 `Cargo.toml` 为准，不在本文重复。
- 通用依赖在 `Cargo.toml`：Serde/JSON、tiny_http、wait-timeout、regex、GBK 解码、Base64、chrono、Ctrl-C、MD5、PNG，以及 v6.0 新增的 `rust_xlsxwriter`（Excel 出口）。
- Windows API 依赖在 `Cargo.toml`：GetIfTable2、GDI 截图、控制台、DPI；release 开 LTO/strip。
- 顶层模块声明在 `src/main.rs`：`agent`、`cancel`、`clock`、`cmd`、`config`、`http_client`、`master`、`nic`、`parser_properties`、`ping`、`protocol`、`rate`、`report`、`resource`、`screenshot`、`util`、`verdict`、`console`。
- **`verdict` 是判定词汇表的唯一定义处**（v4.2.6 之后）：`Verdict`、`ExecutionStatus`、
  `HARD_SINGLE_UDP_FAILURE_CODES`、`aggregate_verdict`。`master::executor` 的
  `aggregate_unit_verdict` 与 `report` 的 `group_verdict` 都必须调用
  `verdict::aggregate_verdict`，**不得各自再写一份优先级**——这条约束由
  `verdict.rs` 里的结构断言 `verdict_priority_has_exactly_one_definition_in_the_tree`
  在 CI 中强制。历史上这两处分叉过，代价是两个静默错判。
- **`master::rate_window` 是正式速率判定口径的唯一实现处**（v4.2.6 之后）：
  `EffectiveWindow`、`RateStats`、`monitor_rate_stats`、`evaluate_nic_rx`
  及采样/滚动窗口覆盖率常量。这一层只依赖网卡计数器样本与目标速率，不接触
  进程、端口、HTTP 或线程，因此"采样不可信必须判 NOT_EVALUATED 而不是
  RATE_FAIL"这条铁律可以被单独审阅和测试。`executor` 的 UDP/TCP/CTS 三条
  路径都调用它。
- **`util` 只保留跨领域原语**（v4.2.6 之后）：子进程执行与解码、日志、时间、
  `lock_recover`、`sanitize`、`md5_hex`、`temp_file`、主机名/OS 名。领域性的
  东西已经各归各家——`cmd::tools`（iperf3/ctsTraffic 的定位与版本探测）、
  `console`（终端读行/提问/序号选择/打开文件）、`nic::same_slash24`
  （同 /24 判断是网络拓扑语义，不是字符串工具）。

### 1.1 依赖方向

```text
main
├── config
├── agent::server ── cmd::iperf / nic / ping / protocol / screenshot / util
├── master::ui ── config / http_client / nic / builder / executor / report / util
│   ├── master::builder ── config / protocol / util
│   │   └── builder::{identity, policy, diagnostics}
│   ├── master::run_status ── verdict（RunObserver trait；webui 提供实现）
│   └── master::executor ── builder / cmd::iperf / http_client / monitor / ping /
│                           protocol / report / report::store / run_status /
│                           screenshot / util
├── nic ── classify / monitor / (scan_windows | scan_macos) / cmd parsers
└── protocol（双机 JSON DTO 边界）
```

`cmd` 只负责系统命令封装和文本解析；`util` 提供进程、编码、日志、时间、文件名、选择和网段工具；`report` 只消费 `Row`，不执行网络操作。

**executor → run_status 是单向的**：executor 依赖 `RunObserver` 这个 trait，
webui 提供实现。加这条边**没有**让 executor 依赖 webui——这是 ADR-2 特意保住的
依赖方向。

**`report::store` 的位置**：它和 `report::html`/`report::xlsx` 平级，都是
`report::model` 的消费端。executor 只依赖 `model` + `store`。

## 2. CLI 与主流程

### 2.1 入口和模式

- `main()` 在 `src/main.rs`：初始化 Windows 控制台/DPI，读取参数；无参数结束时暂停窗口。
- `real_main()` 在 `src/main.rs`，模式如下。

| 模式 | 源码调用流 | 行为 |
|---|---|---|
| 无参数 | `main.rs` | 交互选择 master、agent 或本机 scan；默认 master |
| `agent` | `main.rs -> agent::run` | 读取配置，监听 agent HTTP，阻塞服务 |
| `master` | `main.rs -> master::ui::run_master` | 连接 agent、扫描双端、构建/执行任务、写报告 |
| `scan` | `main.rs -> nic::scan_host` | 只扫描并显示本机网卡 |
| `monitor` | `main.rs -> monitor::run_continuous` | 独立 RX 采样、打印 Mbps、可写 CSV |

- 帮助文本在 `src/main.rs`。
- 长短参数解析在 `src/main.rs`；当前短参数映射 `-i/-n/-c/-d` 分别为 interval/iface/csv/duration。
- CSV 前缀拆分在 `src/main.rs`。
- Windows UTF-8 控制台和 DPI 感知在 `src/main.rs`。

### 2.2 Master 完整调用流

`src/master/ui.rs` 是主控编排器：

1. 加载配置并应用 CLI 覆盖：agent host/port、前缀、resume、截图、是否打开报告。
2. 开启 `master_*.log` 并记录配置来源。
3. 读取/询问 agent 地址并保存 `.cpe_last_agent`。
4. 调用 `/health`（客户端）。
5. 扫描本机和调用 `/info`（客户端），无任一网卡则退出。
6. 预检两端 iperf3；缺失时 ping 仍可运行，iperf 会失败。
7.：有 `tests[]` 时按配置或交互二选一；`--auto` 没有 tests 会退出。
8. 调用 `builder::build_units`，打印跳过提示。
9. 非 auto 模式按 1-based 序号选择任务并确认。
10. 创建 `Ctx`、结果库和输出目录，调用 `Ctx::run_all`，最后停止本地 server。
11. 生成 `report_*.html`、打印汇总、按配置打开报告，并以 FAIL 数量决定退出码。

### 2.3 交互构建

- `interactive_build_specs`：`src/master/ui.rs`。
- 配对顺序：`enumerate_pairs` 先跨机全组合，再主控同机两两，再辅测同机两两；跨机双方均 UNKNOWN 的组合跳过。
- Endpoint 公共构造和同机配对：。
- 统一参数 DTO `UniversalParams`：；询问方向、类型、传输、IP 版本、UDP 限流、流数、时长、ping 计数和 payload：。
- 参数转 `SpecNorm`：；单个接口选择和菜单工具：。

## 3. 配置契约

### 3.1 顶层字段和默认值

`Config` 定义于 `src/config.rs`，默认值于：

| 字段 | 默认值 | 用途 |
|---|---|---|
| `agent_host` | `""` | 空值时交互询问 |
| `agent_port` | | agent HTTP 端口 |
| `ipv4_prefixes` | `["192.168."]` | NIC IPv4 前缀过滤 |
| `require_same_subnet_for_iperf` | `true` | 跨机 IPv4 iperf 要求同 /24，ping 不受限 |
| `limit_udp_by_link_speed` | `true` | 按发送网卡速率裁剪 UDP 流数，WiFi 不裁剪 |
| `screenshot` | `true` | 每个 iperf 单流/组执行后尝试双方截图 |
| `resume` | `false` | 跳过 24 小时内已 PASS 的 Unit |
| `open_report` | `true` | 完成后调用系统默认程序打开 HTML |
| `iperf` / `ping` | 各自 Default | 全局测试参数 |
| `tests` | `[]` | 配置驱动的测试规格 |

### 3.2 iperf、UDP、ping

- `IperfCfg` `src/config.rs`，默认 `duration=120`、TCP windows `64k/1m/4m`、UDP profiles `1m/100m/500m/1000m(-l 64)/2500m`：默认实现。
- `UdpProfile`：`bandwidth` 是 iperf 字符串，`length` 可选；带宽换算为 Mbps，稳定 profile name，显示 label。
- `PingCfg`：默认 `count=100`、`payload_sizes=[32]`。
- `TestSpec`：`src/dst` 必填；可选覆盖 iperf duration、ping count/payload、TCP windows、UDP profiles。
- 默认生成器：方向 A->B，类型 iperf，传输 TCP，IP v4，streams=1。
- `OneOrMany` 支持字符串/数组：A->B/AB/A>B -> `ab`；B->A/BA/B>A -> `ba`；bidir/A<->B/双向 -> `bidir`；旧 `both` -> `ab,ba`；去重保序，无有效值回退 `ab`。

### 3.3 配置加载与边界

- `load_config` `src/config.rs`：显式 `--config` > 当前目录 `config.json` > 可执行文件目录 `config.json` > 默认；只有找不到文件时读取兼容环境变量 `AUTOTEST_IPV4_PREFIXES`、`AUTOTEST_AGENT_HOST`。
- 文件读取和 UTF-8 BOM 容忍：。
- `#[serde(default)]` 允许缺省字段；没有 `deny_unknown_fields`，未知字段静默忽略。
- 根目录 `config.example.json` 是当前有效配置示例。

> **更正（v6.0 核查）**：上一版这里写着 `pairs`、`universal_params`、`agent_token`、
> `rate_check`、`ctstraffic`、`rate_mode`「不在 `Config` 中，不能写成已实现功能」。
> **这六个字段现在全都在 `Config` 里**（`grep -n "pub pairs" src/config.rs` 即可核实），
> 而且 `pairs` 是**全部 6 份出厂配置的主通路**。按旧描述去判断会得出完全相反的结论。

### 3.1.1 测试来源有三种，各服务一类用户

`Config.tests[]` 不是唯一入口。三条来源并存是**有意的**，不是历史包袱：

| 来源 | 长什么样 | 谁在用 | v6.0 的处置 |
|---|---|---|---|
| `tests[]` 显式列举 | 每条测试一个 `TestSpec` | 界面导出的 config、手写精调 | 不动 |
| `pairs` + `universal_params` 自动配对 | 给一批网口 + 一组通用参数，由 `generate_specs_from_pairs` 展开 | **全部 6 份出厂预设**（`config.example.json` + `dist/configs/*.json`）；无人值守/批量回归的主通路 | **零改动**（ADR-13 明确保护） |
| 交互式构建 | `master/ui.rs` 的菜单现场问出来 | 命令行手动跑一次 | 不动 |

界面（快速工作台 + 项目文件）产出的是第一种。**改配置解析时三条路都要过一遍**——
它们共用 `spec_from_config` 之后的全部管线，但入口的默认值填充各不相同。

## 4. HTTP 协议

### 4.1 通用规则

- DTO 和统一包装 `Resp<T>{ok,error,data}` 在 `src/protocol.rs`；成功用 `ok_json`，业务错误用 `err_json`。
- 所有 agent 响应的 HTTP 状态为 200，业务失败放在 `ok=false`；panic 在 `src/agent/server.rs` 捕获并只包装一次。
- agent 请求体读取上限 100 MiB：`server.rs`。
- 空请求体解析为 DTO `Default`，错误文本来自 `server.rs`。

### 4.2 端点表

路由实现集中在 `src/agent/server.rs`；DTO 定义在 `src/protocol.rs`。

| HTTP | 请求 -> 响应 | 路由/DTO 行号 |
|---|---|---|
| `GET /health`、`POST /health` | 无请求 -> `Resp<HealthOut>` | 路由；`HealthOut` |
| `POST /info` | `InfoReq` -> `Resp<HostInfo>` | 路由；`InfoReq` `HostInfo` |
| `POST /ping` | `PingReq` -> `Resp<PingOut>` | 路由；`PingReq/PingOut` |
| `POST /iperf/server/start` | `IperfServerStartReq` -> `Resp<IperfServerStartOut>` | 路由；DTO |
| `POST /iperf/server/stop` | `IperfServerStopReq` -> `Resp<IperfServerStopOut>` | 路由；DTO |
| `POST /iperf/client/run` | `IperfClientReq` -> `Resp<IperfClientOut>` | 路由；DTO |
| `POST /monitor/start` | `MonitorStartReq` -> `Resp<MonitorStartOut>` | 路由；DTO |
| `POST /monitor/stop` | `MonitorStopReq` -> `Resp<MonitorStopOut>` | 路由；DTO |
| `POST /screenshot` | `ScreenshotReq` -> `Resp<ScreenshotOut>`（PNG Base64） | 路由；DTO |

### 4.3 主控 HTTP 客户端

`src/http_client.rs` 实现零额外 HTTP 依赖的 HTTP/1.1 客户端：5 秒连接超时、调用方读超时、30 秒写超时；GET 发送空 body/Content-Length 0；POST 以 UTF-8 字节长度设置 Content-Length。响应支持 Content-Length、读到 EOF 和 chunked；chunk 解码，Content-Length/状态解析。

## 5. NIC 扫描、分类与监控

### 5.1 公共入口

- `src/nic/mod.rs` 的 `scan_host` 按平台扫描，随后按 `role_rank` 和接口名排序。
- IPv4 前缀判断：空列表全放行，非空任一非空前缀匹配即可。
- 展示表 同时输出角色、接口、IPv4、速率、WiFi 频段和 v6 link-local。

### 5.2 角色分类

- 排序常量 `src/nic/classify.rs`：10GETH、10GUSB、SGMII2.5G、SGMII1G、RNDIS、WiFi 系列、UNKNOWN。
- 分类 优先级：WiFi 频段；描述 10g+usb；RNDIS 关键字；4001-8999 的 USB 10G 兼容档；9000-12000 以太 10G；2.5G；1G；3400-4000 RNDIS 兜底；否则 UNKNOWN。
- `role_rank`；Windows 名称 WiFi 兜底。

### 5.3 Windows

`src/nic/scan_windows.rs` 定义 GetIfTable2 行；`if_rows` 采集别名、描述、接口索引、速率、WiFi 类型和 RX octets；UTF-16 处理；RX 查询；完整扫描/合并 ipconfig、GetIfTable2、netsh。

Windows 文本适配器：`src/cmd/ipconfig.rs` 解析中英文 `ipconfig /all`；`src/cmd/netsh.rs` 解析 WiFi 名称、连接状态和频段。两者测试位于各自。

### 5.4 macOS 与其他平台

- macOS `src/nic/scan_macos.rs` 解析 ifconfig block； 解析硬件端口； 探测速率； 获取 WiFi 频段/PHY；完整扫描。
- macOS 监控实现 `src/nic/monitor.rs` 使用 `netstat -ibn`；`parse_netstat_ib` 取 Link 行 Ibytes。
- 非 Windows/macOS 的 NIC 扫描返回空列表（`nic/mod.rs`），RX 和截图分别返回“不支持”错误（`monitor.rs`、`screenshot.rs`）。

### 5.5 RX 监控

- 注册表 `MonitorMgr` `src/nic/monitor.rs`：start 保存累计字节和时间，stop 用差值计算平均 Mbps，ID 为 `monN`，sweep 清理过期条目。
- 独立连续监控选项和循环：Ctrl+C、间隔/时长、实时输出、可选 CSV。
- CSV 摘要重写：接口、间隔、时长、平均/峰值和采样明细。

## 6. 任务构建模型与不变量

### 6.1 中间模型

定义在 `src/master/builder.rs`：

- `PORT_BASE=56000`；`Side`。
- `Endpoint` 保存 side、PC、`NicInfo`，`key` 用于禁止同一网口作为源/目标。
- `SpecNorm` 是配置和交互的共同规范格式。
- `IperfTask` 保存 v4/v6、TCP/UDP、profile label、源/目标、端口、时长、额外参数和流索引。
- `PingTask` 保存 v4/v6、源/目标、计数和 payload。
- `LegKind` 为单流 iperf、UDP 多流组或 ping；`Leg` 带 `""/ab/ba` 标签；`Unit` 是 RESUME 和执行的最小单元。
- IPv6 三元组 `V6Addrs`；选择 link-local 优先、否则 global。

### 6.2 配置解析与展开

- endpoint 角色/NAME 解析；配置 TestSpec -> SpecNorm，streams clamp 到 1..32，iperf duration 1..86400，ping count 1..100000。
- UDP 发送口限流：WiFi、WIFI 角色、未知速率或非法带宽不裁剪；否则 `floor(speed/bandwidth)` 与请求流数取最小。
- 方向腿 `dir_pairs`：ab 一腿、ba 一腿、bidir 按 `[ab,ba]` 两腿。
- 共享 materializer `map_legs` 和 `unit` 消除了 TCP/ping 重复初始化。
- Unit 生成主循环 `build_units`：先方向，再 IP 版本，再按 iperf（TCP、UDP）→ ctsTraffic → ping 的顺序交给 `builder/{iperf_tcp,iperf_udp,cts,ping}.rs` 展开；跨机 IPv4 iperf 与 ctsTraffic 可受同 /24 门禁，ping 不受门禁。端口按这个顺序全局递增分配。入口先经 `canonical_axes` 把 `ip` / `kinds` / `transports` 换成规范值（`ip` 的写法表 `canonical_ip_version` 与控制台共用），认不出的值进计划提示——展开只问「是不是 `v6`」，不归一的话 `"ipv6"` 会变成第二份 IPv4。
- 套件控制台共用 `builder::build_ui_units_repeated`：逐规格展开、整套派生轮次、按稳定 ID 保序去重，`UiPlanUnits::spec_indices` 保留最终单元的原始规格索引。`webui::plan::compile_request` 用这些索引生成来源信息，并直接对最终单元计算计划哈希；`ui::run_master` 仅在 `console_request` 含 `ui_plan` 时使用同一展开函数。普通 CLI 继续使用 `build_units_repeated`，显式重复任务保留。去重不重分配已留下单元的端口，不改首轮 ID 或轮次身份。

### 6.3 端口、流和稳定 ID

- 主流程在 `src/master/ui.rs` 将端口游标初始化为 `PORT_BASE=56000`；`alloc_port` `builder.rs` 返回当前端口并递增，达到 65535 后回绕到 56000，因此只在回绕前单调且不重复。
- TCP：每个 window 生成一个 Unit，腿内使用 `-w <window> -P <streams>`，端口按腿顺序分配。
- UDP：每个 profile 生成一个 Unit；每条腿根据发送口得到流数，多于 1 流时每流独立端口/进程；任一腿为 0 则整个 profile Unit 跳过。
- Ping：每个 payload 一个 Unit，构造。
- TCP ID 模板：`iperf_v1|V4/V6|tcp|profile|duration|src-id|dst-id|direction`。
- UDP ID 模板：另含 `streams`。
- Ping ID 模板：`ping_v1|count|payload|V4/V6|src-id|dst-id|direction`。
- 修改 ID 模板、字段顺序或 `v1` 会使旧 `task_results.json` 的 RESUME 命中失效；这是有意的兼容边界。

## 7. 执行器、判定、截图与 RESUME

### 7.1 Ctx 与远端统一调用

- `Ctx` `src/master/executor.rs` 持有 agent 地址、配置、输出目录、本地 server/monitor、线程安全 Row 和 ResultDb。
- `agent_post` 统一序列化、POST、HTTP 状态、`Resp<T>` 解析、业务错误和缺 data。
- 双端 ping/iperf server/client/monitor 适配；截图低层获取，截图开关和文件保存。
- Agent 截图日志保留状态/长度、HTTP 前缀、JSON 前缀、业务错误、缺 data 和 Base64 长度；`byte_prefix` 以 UTF-8 边界安全截断。主控截图失败仍静默跳过。

### 7.2 调度与双向

- `run_all` 顺序遍历 Unit；多腿 Unit 用 scoped threads 并行。
- `resume=true` 时在执行前查询 `ResultDb::fresh_pass`；跳过行是 `ok=None`，计入 skip。
- 双向两腿完成后 互填 `peer_rx`，保留三位精度和对向 tag。
- Unit PASS 必须所有腿 `LegOutcome.ok=true`；空腿视为 FAIL；每个 Unit 结果写回数据库并暂停 1 秒。

### 7.3 Ping

- `run_ping_leg` `src/master/executor.rs` 选择 v4/v6 地址、调用本地或 agent ping、生成 Row 和 raw 输出。
- `src/ping.rs` 构造 Windows `ping` 或 macOS/BSD `ping/ping6`；执行。
- 解析 `ping.rs`：中英文 Windows、BSD/macOS；只认真实 RTT 的 reply，修正目标不可达/ICMP 错误被统计为 received 的假成功。

### 7.4 iperf 单流和 UDP 组

- server/client/stop 核心 `exec_iperf_core` `src/master/executor.rs`；IPv6 zone 处理。
- 单流 `run_iperf_single`：接收端 monitor、client/server 输出解析、PASS、截图和 Row。
- UDP 并发组 `run_iperf_group`：错峰 200ms 起流、共享接收端 monitor、每流 Row、组合计 Row、组合 PASS。
- `iperf_row` 是单流/组内流/组合计的公共 Row 基底，字段必须与 Unit/Task 对齐。
- 单流 PASS：client 成功且未超时，并且 iperf 文本有正测量或接收网卡 RX > `MIN_VALID_RX_MBPS=0.01`；组内每流需文本测量，组合还需 RX > 阈值。

### 7.5 ResultDb / RESUME

- `DbEnt`、`ResultDb` 和 24 小时常量 `src/master/executor.rs`。
- 加载 JSON；fresh PASS 判断：要求 `ok=true`、`age.num_hours() <= 24` 且未来偏差不超过 60 秒；由于整小时截断，过去记录实际可命中到不足 25 小时。
- `set`；原子临时文件写入和 rename。

## 8. iperf、命令与公共基础设施

### 8.1 iperf 适配

`src/cmd/iperf.rs` 只放 server 与作业两套注册表共用的生命周期工具（ID 校验、租约截止、分段锁），
并 `pub use` 外部用到的符号；实现按职责分在 `src/cmd/iperf/`，外部路径仍是 `crate::cmd::iperf::*`：

- `args`：server/client 参数构造；TCP 用 `-P`，UDP 用 `-u` 加额外 `-b/-l`；`extra` 不许覆盖的受控参数（`RESERVED_CLIENT_FLAGS`）。该黑名单有两类：改**测量口径**的（`-c/-B/-p/-t/-i/-f/-u/-4/-6`）和碰**文件系统**的（`-F/--file`、`-I/--pidfile`、`--logfile`）。后一类是安全边界而非口径——`extra` 原样进 agent 执行的命令行，`-F` 会把 agent 本机文件原样灌给对端 server（持令牌者整份取走，已本地复现），`-I/--logfile` 让 agent 往任意路径写；即使没有本地调用方也必须挡，它是协议边界对任何调用方的约束。守在 `cmd::iperf::tests::file_system_flags_are_blocked_in_every_spelling` 与 `a_file_exfiltration_attempt_is_refused_before_it_runs`。
- `parse`：`IperfParsed` 和最佳 sender/receiver/measurement 判定；文本速率、Bytes/bits、单位换算和 UDP 丢包解析；运行中逐行的实时事件（`classify_live_line`）。汇总与实时两处共用 `rate_mbps` 一份单位换算（bit 按 1000、Byte 按 1024 进位）。
- `server`：`IperfServerMgr`。不带 request_id 的旧协议同端口先停旧 server，带 request_id 的按 request/owner 幂等；后台收集 stdout/stderr，TCP connect 探测 ready，主动 stop/kill，sweep/stop_all。
- 就绪探测（`server`）：IPv6 zone 解析成 scope id（数字索引，或 unix 上按接口名查），每轮重新解析地址并逐个尝试全部解析结果（任一连上即就绪），connect 超时 1 秒、都失败后 200ms 再试，总超时 15 秒；Windows 不带 zone 的 link-local 只等 300ms 并确认进程存活。
- `client`：瞬态错误和重试，最多 3 次，单次总超时为 duration+120 秒，保留实时输出和 stderr；事件时间轴对齐（`align_event_to_epoch`）。
- `jobs`：`IperfClientJobMgr`，异步 client 作业（`/iperf/client/start` 立即返回 job id）、租约、tombstone、owner 清理；CTS 经 `start_external_request` 复用同一套。
- 文本输出上限 `iperf::OUTPUT_LIMIT`：client（经 `ProcessSpec::stdout_limit`，内环 ADB 执行器同样转发）与 server 各自只留开头与结尾、中间按行省略（`util::BoundedOutput`）。判定只读末尾汇总行；不封顶时 `-P 32` 约 9 小时就超过主控读响应的 `http_client::MAX_RESPONSE_BYTES`，server 停止 / client 结果读不回来。「三次尝试之和小于响应上限的一半」是编译期断言。ctsTraffic 的输出解析要扫全部状态行，不封顶。
- 测试在 `src/cmd/iperf/tests.rs`，属白盒测试：`SrvEntry`、两个注册表的内部字段对它开放为 `pub(super)`。

### 8.2 公共 util

- 编码和命令结果 `src/util.rs`；GBK fallback 适配中文 Windows。
- piped 子进程 helper；阻塞执行/超时 kill；实时流式执行、无尾换行回调和 stderr。
- 日志；时间；文件名安全化；主机/OS。
- iperf3 定位/版本：程序同目录优先，再查 PATH。
- 交互输入；打开报告；MD5；临时文件。
- 选择解析：空输入全选、逗号/范围、去重保序和边界错误；同 /24。

## 9. 报告与截图

### 9.1 Row 和 HTML

- `Row` 字段定义在 `src/report.rs`：排序键、任务/父 ID、源/目标、状态、接收/对向/发送/接收速率、UDP/ping 指标、截图、命令、raw、组合计标记。
- `ReportMeta`：主控、辅测、agent host、开始/结束/耗时。
- HTML 辅助函数：截图链接、HTML 转义、数值格式、 PASS/FAIL/SKIP 映射。
- `write_report`：按 sort_key 排序；组合计不进入总数；输出元数据、统计、25 列表格和原始输出 details；字段均转义。
- Task/Parent ID 在表格显示前用 `short8` 截 8 个 Unicode 字符：。
- `Row.raws`：原始记录文件落盘成功后，行里只留 `report::embedded_raw` 的首尾版本（`executor::row_raws`），全文只在原始记录里；落盘失败时行里保留全文。`embedded_raw` 把省略说明算进上限、是幂等的——执行器裁一次、渲染再过一遍，第二遍原样返回。

### 9.2 截图实现

- macOS `src/screenshot.rs` 调用系统 `screencapture` 临时 PNG。
- Windows GDI 主屏抓取：GetDC/CreateCompatibleBitmap/BitBlt/GetDIBits，BGRA 转 RGBA，再编码 PNG。
- 其他平台固定错误。
- PNG 编码；测试在 解码回读 2x2 RGBA，而不只检查魔数。

## 10. 测试覆盖索引（当前 Rust 全量 773 项；下表列出按模块维护的覆盖面）

| 区域 | 测试位置 | 覆盖 |
|---|---|---|
| 配置 | `config.rs` | 默认值抽样、代表性 JSON 反序列化及部分缺省字段、bidir/方向数组/both 展开、UDP profile 带宽换算/名称/标签 |
| CLI | `main.rs` | 长短 flags、值/开关规则、CSV 拆分 |
| builder | `builder.rs` | TCP/UDP 稳定 ID、端口、双向组、UDP 限流/WiFi 豁免、同 /24、ping、IPv6 |
| executor | `executor.rs` | ResultDb 保存/加载、刚写入 PASS 命中、未知 ID、失败覆盖 |
| UI | `ui.rs` | 跨机优先、同机配对顺序、UNKNOWN 过滤 |
| iperf | `cmd/iperf/tests.rs` | TCP/UDP 结果解析、Gbits/sec 与 MBytes/sec 换算、无测量数据的错误输出、瞬态错误判定、client/server 参数构造 |
| ping | `ping.rs` | 中文/英文/BSD、全丢、不可达假成功、部分成功 |
| Windows parser | `cmd/ipconfig.rs`、`cmd/netsh.rs` | 中英文适配器、WiFi 状态/频段 |
| NIC 分类 | `nic/classify.rs` | 角色、排序、WiFi 名称；USB 4000/4001/8999/12000 与以太网 8999/9000/12001 分类样例 |
| macOS NIC/监控 | `scan_macos.rs`、`monitor.rs` | ifconfig、netstat Ibytes |
| ADB 内环 | `inner/tests.rs`、`inner/adb_client.rs`、`inner/receiver_server.rs`、`inner/remote.rs`、`inner/webui.rs` | 配置迁移、计划与稳定 ID、ADB/辅测机生命周期、双向并发、RX 判定来源、RESUME、报告原子写入与历史边界 |
| HTTP/agent | `http_client.rs`、`agent/server.rs` | Content-Length/状态行解析、chunked 正常及非法大小、tiny_http POST 回环、空 body 默认请求、非法 JSON 错误单次包装 |
| util | `util.rs` | 选择解析、同 /24、sanitize、run_cmd 成功/启动错误、非 Windows streaming 无尾换行 stdout 回调/收集及 stderr |
| report/screenshot | `report.rs`、`screenshot.rs` | PASS/SKIP、转义、排序/组合计、截图链接；PNG 实际解码 |

## 11. 不可破坏的不变量与修改入口

### 11.1 必须保持

- 两轮对比使用 `Row.comparison_identity`（`ComparisonIdentity` / `ComparisonLeg`），由 `executor::row::unit_row` 从计划保存单元方向、轮次、各腿端点及参数/时长；不改变 `Leg.tag`、行级方向或 RESUME 身份。历史缺字段时只从完整类型化明细还原，重复或不完整的键全部保留并标记 `DeltaKind::Ambiguous`，禁止覆盖或任意配对；`Ambiguous` 排在 `Regressed` / `SlowerButStillSameVerdict` 之后。任一轮判定为 SKIP（只来自 RESUME 复用）的对齐项归为 `DeltaKind::Resumed`，不算回归也不算转好。CLI 退出码由 `master::ui::compare_exit_code` 决定：有回归 1，无回归但不完整 2，否则 0（回归优先）；API 附加 `ambiguous` 与 `resumed` 计数。
- 子网扫描统一调用 `InfoReq::for_scan`：空前缀显式全扫，有前缀照常过滤。预览和执行启动都验证完整扫描能力，`LiveTopology` 复用相同请求构造，不添加额外轮询。旧 `/info` 缺省请求仍回落到 agent 默认前缀。
- 组合场景的启动结果未知由 `state/inner::scenarioStartPhase` 持续保存，`scenarioBlocksActions` 统一拦截冲突操作；断线只续接状态查询，不重发启动。**确认起跑只认状态里带回的本次启动令牌**（`scenario::Request::start_token`，`valid_start_token` 校验，`/api/scenario/status` 原样带回）：不再拿「场景 ID 与开始前不同」判断——初次状态读取失败时基准是空串，任何旧场景都会被误认。别的页面起的场景在跑时保持未确认。单次空闲不解除未知，明确重新准备（`prepareAfterUnknownScenario`）只清本地状态，并**同时作废内环与子网预览**；401 进入统一会话失效流程。监控每条轮询链绑定 `pollEpoch`，停止即作废，旧请求成功/失败都不得改状态或续接。
- `/api/skip-unit` 必须提供 `run_id` / `unit_seq`；`RunStatusRecorder::request_skip` 与 `unit_started` / `unit_finished` 共用状态锁，目标核验和取消信号写入不可分离，同一目标只写一次。执行器在发布下一单元之前消耗前一单元的 skip，停止/退出始终优先。


- 内环 `ip_versions` 只允许 4/6，每个版本独立单元、预检、结果及 RESUME；旧配置缺字段默认 IPv4，新建页面默认双栈。`Link::local_ipv6` / `gateway_ipv6` 是不带 zone 的 IPv6 单播，参与链路两端须同为链路本地或同为非链路本地；`TrafficAddresses` 按执行端真实网口构造绑定地址和目标，板侧地址经 ADB 验证唯一 LAN 归属，不能由 IPv4 推算；`adb::lan_interface_v6` 仅在扫描证明共享 link-local 地址的全部匹配口属于同一座也持有该地址的桥时选桥作用域，跨桥或缺少归属证据仍拒绝。取消参与的 IPv6-only 链路不要求补 IPv4，反之亦然。`plan::legacy_link_identity` 固定旧字段的 Debug 格式以保留原 IPv4 身份。`BoardInterface::ipv6_addresses` 与既有 IPv4 `addresses` 分列，空前缀电脑扫描保留 IPv6-only 接口，显式 IPv4 前缀的旧过滤语义保持不变。

- 内环主控扫描不设前缀，辅测机以 `InfoReq::all_interfaces` 显式请求全接口扫描（含 IPv6-only），先核实 `UNFILTERED_INFO_CAPABILITY`（`inner::remote::Remote::info` 复用 `InfoReq::for_scan(&[])` 与 `missing_capability`，与子网入口同一条规则，提示文案为内环自有）；旧 `/info` 请求缺少该字段时仍保留空列表回落到 agent 默认前缀的语义，不支持显式全扫的旧 agent 明确报错。

- 内环扫描绑定 ADB 与辅测机连接身份；身份编辑立即清能力快照和待添加勾选；两者共用 `state/inner::innerProbeIdentity`。同一连接重扫按电脑、接口与地址键恢复待添加选择，不按扫描顺序恢复；请求代次拒绝迟到成功或失败。子网本机页重扫在任一种测试运行期间禁用，失败保留旧快照并明确标注，再次重扫刷新双端。监控会话绑定已连接辅测机的 host/port，换机回收旧辅测会话并拒绝迟到启动应答；本机监控继续。

- `webui::api::api_connect` 的空前缀显式请求 `InfoReq::all_interfaces`，与本机无前缀扫描一致；旧 agent 无完整扫描能力时拒绝全扫并保留旧连接，显式前缀仍兼容。它先在候选配置中应用地址、端口、令牌和前缀，待 `/health`、`/info` 与本机扫描成功后，一次性提交连接身份和双端网卡清单。失败不得留下新地址配旧网卡，也不得部分覆盖上次成功连接的配置。

- `state/run` 的进度响应受请求代次约束，开始受理后不接收开始前的快照。开始超时保持 `unknown`，一次空闲快照不证明请求未执行；操作员确认后的 `prepareAfterUnknownStart` 只清准备态和预览，不发起运行。跳过受理绑定发出时的运行和单元，旧单元的迟到增量不得解除当前跳过状态。`state/session` 合并重复连接，重置后旧连接响应不得覆盖新会话。

- `rate_window::longest_zero_delta_run_ms` 与 RX 平均采用同一有效窗口：边界区间裁剪、重复/重叠时间只计一次、缺采断开连续段；`scan_excursion` 同样不把缺采前后两段短掉速拼成一次连续掉速。窗口外空闲不参与停滞比例。

- `executor::window::leg_effective_window` 按流量事件和采样边界计算 UDP 的毫秒区间，不按一秒网格补长尾部。完整性可使用既有时间容差，窗口终点仍不得超过实际证据。`executor::agent` 的显式用户取消在回收成功后仍保留 `cancelled`；取消事实和回收确认彼此独立，超时重试语义不因此改变。

- 报告的方向统一由 `report::model::direction_tag` 优先读取类型化字段；历史缺失才从文案兜底。`group_rows` 按 `sort_key` 还原顺序，HTML 与 Excel 单元序号共用 `group_seq`；历史缺失 `unit_seq` 时不得把不同单元并为一组。Excel 链路键包括源端/接收端 `RowSide`，同名网口的正反向单向单元分别统计。

- 子网预览和开始的阻断规则只有 `webui::plan::CompiledPlan::blocking_errors` 一份实现：规格编译失败、套件中有被跳过的项目、没有可执行单元。「被跳过的项目」按 builder 给的类别认（`builder::NoticeKind::Skipped`，经 `CompiledPlan::skipped_notices`），不看提示文字是不是以「跳过 」开头；底层诊断（`NoticeKind::Diagnostic`，如 `-w` 排空）进 `PlanOut::diagnostic_notices`，预览不展开，命令行与运行日志照常打印全部提示。`api_plan` 返回可选 `PlanOut::blocking_errors`，`api_run_impl` 使用同一结果拒绝启动；前端只读该字段，不另解析提示文本。旧矩阵仍允许执行有效部分，空计划仍拒绝。「执行」页 `views/run/RunView` 按运行状态在准备面板（`RunPrepare`）与进度面板（`RunProgress`）之间切换，切换条件由状态推出、不另存：运行中、开始应答 `sending`/`unknown`、运行状态未同步一律显示进度；本轮已结束（有 `run_id` 或留有日志）时先看结果，`ui.preparing`（「准备下一轮」、从计划或历史进入时置位）才回到准备面板。只有确认 `startPhase === 'accepted'` 才清 `ui.preparing`，未知应答不当作启动成功；进入准备面板且预览过期时自动预览一次。

- 「计划 › 网口」`views/plan/PortSelection` 是唯一的分配编辑器（旧的集合×套件矩阵已并入），按链路集合分组：组复选框走 `domain/plan-build::toggleBinding` / `bindingSelectionState`（整集合 `pair_ids: []`，后来扫描到的同类网口自动参与，组标题标「整组」）；行复选框走 `domain/plan-ports::setPairAssigned`，批量「全选显示 / 取消显示」走 `setPairsAssigned`——已处于目标状态的行不重写，整组分配不会被改写成显式清单。保留集合 ID、端点方向、其他套件和绑定顺序；取消最后一对必须删除绑定，不能写回空数组；逐网口新增绑定使用显式 ID。批量操作仅影响显示行的当前套件，搜索与「全部/跨机/同机」只控制显示，不改集合与分配。`plan.filter` 固定为 `'all'`，旧草稿里的 `cross`/`same` 读入时改写：`pairs::roleKey` 区分跨机与同机，两类网口从不进同一个集合，所以 `'all'` 只多出未分配的集合，执行单元不变；它不在项目文件里。
- 「计划 › 测试内容」`views/plan/SuiteEditor` 在任务里就地展开共享流量配置（`RecipeFields`），影响面由纯函数 `domain/plan-build::recipeReferences` 点名，未被引用的配置由 `unusedRecipes` 列出并可清理。「门限与默认值」标签承载 `NicPolicyTable` 与 `GlobalDefaults`，标注的优先级（任务门限 › 按网口 › Wi-Fi 频段 › 默认）与 `webui::plan::apply_wifi_pair_targets` 的 `fill_direction_target` / `nic_rx_override_resolves` 顺序一致。

- 控制台浏览器回归在 `ui/e2e/console.spec.ts`，使用 `webui::tests::browser_regression_server` 交付真实鉴权、CSP 和内联页面；扫描与运行响应可控，cookie-only API 请求直接验证 Rust 服务。`state/session::rescan` 的本机阶段失败也保留并标旧双端快照；从未连接过辅测机时，本机阶段失败只记 `localError`，不把会话标成连接失败。控制台导航为「连接 / 计划 / 执行 / 内环测试 / 监控 / 历史」六区：「连接」`views/connect/ConnectView` 合并旧本机与辅测机两页、只留一个重扫入口，`NicTable` 列出全部扫描字段（不再有网卡详情面板）；「历史」`views/runs/HistoryView` 以标签承载子网（`SubnetRuns`）、内环（`InnerRuns`）与组合场景（`ScenarioRuns`）记录。子网重新执行固定以 RESUME 装载，准备面板上可取消。内环「停止测试」在组合场景时停场景、否则停内环，两个分支都必须**调用**（`InnerView::stopCurrent`），守在 e2e「单独运行的内环测试点「停止测试」真的发出停止请求」。

- 子网 `state/plan::invalidatePreview` 增加请求代次并清空预览状态；`preview` 响应仅在请求代次和当前配置快照同时匹配时落地。成功导入项目或恢复默认时调用 `resetRunOptions`，清空项目不保存的 RESUME、截图、探测、强制窗口/带宽并将轮次恢复为 1；历史 `adoptRunRequest` 则从归档恢复这些选项。导入前发出的旧响应不得覆盖新项目，也不得结束新请求的忙碌态。

- 内环扫描选择由纯函数 `domain/inner-setup::innerNicChoices` / `linksFromInnerChoices` 生成，默认展示 192.168 网段或仅 IPv6 的有效电脑网口并隐藏常见隧道/虚拟口，其他候选须显式展开；完整扫描不被裁剪，板侧全部系统接口与测试网口数分开；用户明确选择后由 `state/inner::addInnerScannedLinks` 添加，按电脑、接口名和 IPv4 去重，不把扫描结果全量加入。`innerSetupIssues` 同源生成缺项与修复定位；`InnerView` 参数区用一句话说明上下行的统计口径。`InnerLinkTable` 批量设置只处理当前显示且 enabled 的行，扫描发现网卡不等于链路预检通过。

- ADB 内环入口 `inner::run_cli` 与 `inner::webui::Controller` 共用 `inner::perform`，独立严格配置 `inner::config::InnerConfig`、项目标识 `cpe-inner-project` 与 schema `version: 3`（兼容 v1/v2），不混入子网 Config/Unit；内环单元有独立稳定身份和 24 小时 PASS RESUME，绝不命中子网历史。分层固定：`config` 定义 schema 与 v1→v2 迁移（`protocol`→`protocols`、`board_interface`→`board_rx_interface`，补 `enabled`/`measurement`/`repeats`/`resume`；顺序单向**绝不**迁成 `bidir`，策略保持 `nic_strict`）；`plan` 是**全仓唯一**的笛卡尔积，页面预览、执行器、进度和报告都消费它，展开顺序为 网口 → IP 版本 → 协议 → 方向 → 轮次；`adb` 适配板侧设备（接口清单、桥成员、`/proc/net/dev` 与 sysfs 两条读取路径、按端口起 server）；`measure` 是纯策略层，来源选择与门限配对只在这里；`mod` 只负责按计划起流采样；`report`/`history` 输出与历史。`inner::remote` 根据链路 host 使用现有 agent client/monitor/owner cleanup 协议，令牌不序列化。`inner::adb` 仅经 ADB 确认自有 server 就绪，不要求主控能路由到被测 LAN；前台 shell 使用本次 PID、停止标记和有限租约回收，未确认回收时停止后续起流。发送端始终运行普通 client：上行 PC client → 板侧 server，下行板侧 client → PC server，不使用 `-R`；`inner::adb_client` 经公共 ProcessExecutor 复用 iperf 参数/重试/事件解析，`inner::receiver_server` 按方向管理本机/agent/板侧 server，server owner 与 client/monitor owner 隔离以便停止后先收日志；接收端按数据走向定（上行采板侧 RX，下行采网口所在电脑 RX），不按谁跑 client 推断。`enabled: false` 的网口保留配置但不执行、不预检，其引用的辅测机也不进连接门禁——无 agent 或 agent 离线都不阻断本机测试。`bidir` 是一个含两条腿的单元：两腿各占 `port` / `port+1`、独立 job 与日志、在作用域线程里同时起流，按两腿截断前真实流量区间的**交集**计算（两条腿先各自扣掉起流爬升 `plan::SETTLE_SECS`；两条腿各多跑 `BIDIR_OVERLAP_MARGIN_SECS`，交集从起点截出配置时长；交集不够长就不下结论；工具口径同样取共同窗口，见下），一腿失败置位单元取消位停止对向并定向回收。网卡口径与子网同规则：有效窗口不完整判 `IPERF_EFFECTIVE_WINDOW_SHORT`，不在没跑满的窗口上下结论；工具口径的 receiver 汇总覆盖不到配置时长（中途退出时 server 仍可能打出一条只覆盖前几秒的汇总）同样拒收。`-P`/`-w`/`-b`/`-l` 按协议分开配置，`tcp_window` / `udp_length` 走 `config::size_token` 白名单、`board_rx_interface` 走 `config::iface_word` 白名单后才拼进命令行或 sysfs 路径。测量策略 `nic_strict` / `nic_preferred` / `tool` 决定来源：网卡口径始终单独留存并仍由 `rate_window::evaluate_rx_acceptance` 产出，字段语义不变；兜底整条腿只选一次来源并记录原因，**可信低速不触发兜底**，工具口径**不继承**网卡门限（无工具门限只出 MEASURED）；工具速率首选接收端 server 逐秒记录在判定窗口上的时间加权平均（`cmd::iperf_window::receiver_rate_over`；多流只认 `[SUM]` 行，覆盖不足 95% 拒收），和网卡口径同一段时间，双向时就是共同窗口；拿不到才退回全程 receiver 汇总（含起流爬升，双向时只有两条腿汇总覆盖同一段才可用）。两者都只认接收端，不取 sender / interval 末行。以前只认全程汇总，`tool` 策略的双向单元只要两条腿起跑差 100ms 以上就无法评价；守在 `a_tool_strategy_bidir_unit_is_judged_on_the_receivers_per_second_rates`；双向合计只相加同来源层次的两端接收速率，配了合计门限才按合计判一次。`max_udp_loss_pct` 超限仅追加 `UDP_LOSS_HIGH`，不推翻速率判定。板侧 server 原文按首尾裁剪后随腿进报告；判定（逐秒接收记录、断流旁证）用裁剪前的完整日志。`inner` 与子网共用纯 `cmd::iperf_window` 和 `rate_window`，不调用子网执行器。`master::webui::http` 在既有鉴权后转发 `/api/inner/*`（含 `plan` 预览与 `runs*` 历史，历史目录名按白名单精确比对、不做路径拼接）；两类启动共用 run_gate 防止并发占线，但配置、运行状态和取消标志分开；组合入口 `/api/scenario/*` 按子网后内环顺序执行，并在 `scenarios/` 保存两份原始配置，历史恢复默认打开两段各自的 RESUME。内环输出 `inner_runs`（`report.html`/`result.json`/`summary.json`/`config.json`/`units.jsonl`），子网历史仍只读 `runs`；历史「装载配置」只回配置不直接开跑。前端 `state/inner`、`domain/inner`、`views/inner/*` 不读写子网计划/连接/运行状态；独立草稿键与导入导出，误导入不得改写另一模式的配置；计划预览来自后端，前端不自算笛卡尔积。

- 项目导入通过 `domain/import-topology::reconcileImportedTopology` 区分未知快照与成功扫描的空网卡表；只清理确认缺失的端点，并提示被删除的集合/绑定。绑定的显式 `pair_ids` 清空时必须删除该绑定，不能变成整集合分配。手工集合协调保留 pair ID 和端点方向。待校验状态随草稿保存，并在取得可信拓扑后自动校验。

- 组合场景的 `request.json.phase` 与内存状态同步：子网阶段完成、进入内环时立即持久化为 `inner`，收尾再写入最终 `finished`/`error`，历史列表不能在长运行期间显示陈旧阶段。

- 报告分类在存在类型化协议/后端时不得再被任务名称覆盖；历史字段缺失才使用字符串兜底。HTML 与 Excel 在单元汇总缺失时通过 `report::model::verdict_row` 选择匹配聚合判定的原因来源，方向代表行评分共用 `direction_row_score`。

- 主流程从 56000 开始递增分配端口，达到 65535 后回绕到 56000；TCP 使用一个 client 的 `-P`，UDP 多流使用独立进程/端口；bidir 始终是 `[ab,ba]` 两腿。
- 稳定 ID 模板和字段顺序见 `builder.rs`；其输入构造还包括 `ep_id` TCP profile 名，以及 `config.rs`/`builder.rs` 的 UDP profile 名。改变模板、字段顺序或任一输入规范会让历史 RESUME 不再命中。
- IPv4 同 /24 门禁只限制跨机 iperf；ping 不受限。IPv6 优先双端 link-local，其次 global；macOS 执行时加 zone，Windows 不加。
- UDP 限流按每条腿的发送 NIC；WiFi/未知速率不裁剪；任一腿不能承载 profile 就跳过整个 Unit。
- PASS 规则：ping 见 `ping.rs` 与 `executor.rs`；iperf core、单流、组内流、组汇总和 Unit 分别见 `executor.rs`；组合计行在 `executor.rs` 标记，并由 `report.rs` 排除在报告总数外。
- RESUME 是 Unit 级；当前由 `executor/db::resume_age_is_fresh` 按实际时长严格小于 24 小时判断，并容忍未来时间 60 秒。agent HTTP 线程池固定 16 worker，但 iperf3/CTS 的 client 作业各自跑在独立命名线程（`iperf-client-<id>`）上，`/iperf/client/start` 立即返回 job id，**并发流数不受 16 的限制**；每 30 秒 sweep，server/monitor 最大存活分别为 10/30 分钟（`agent/server.rs`）。
- 对外 JSON 字段即使当前生产代码没有本地消费者，也属于协议兼容面；删除/重命名要同步所有端点和版本策略。
- WebUI 的 Wi-Fi 门限以“主控频段 × 辅测频段”为一组，每组两个单向门限（主控→辅测、辅测→主控）加**一个双向 RX 合计门限**；界面只按当前两端实际频段组合去重显示。旧的两个「每方向双向门限」按两者之和迁移成合计，只填过一个方向的不推导。旧发送频段规则和具体网口覆盖只作 request.json 读取兼容，新项目不再创建。
- 频段在**存储与比较**上一律是稳定枚举 `wifi_2_4g` / `wifi_5g` / `wifi_6g` / `unknown`（Rust `plan::canonical_wifi_band`，TS `canonicalWifiBand`），界面再渲染成 `2.4G / 5G / 6G`。展示文案是最容易被改的东西，而改完之后频段规则会**静默失效**——找不到规则不报错，只是门限没了。
- 门限的最终生效值必须能在预览上直接看到（`PlannedUnit::targets`）。`RateTargets::for_direction("ab")` 是 `ab.or(forward)`，所以「`forward` 字段还在」不能证明它还在生效；补兜底门限一律走 `fill_direction_target`，它按 `for_direction` 的结果判断，不看某个字段填没填。
- **控制台访问口令默认随机，不回落公开值**（`util::generate_console_token`）。没给
  `--ui-token` / `CPE_UI_TOKEN` 时每次启动现生成一枚随机口令（熵取自标准库 `RandomState`
  读的 OS CSPRNG，不引入第三方依赖），随启动地址的 `?token=` 打印、浏览器自动带着打开。
  以前这里回落到公开默认 `cpetest`，而控制台能改配置、发起测试、下载 config，等于默认把
  钥匙交给同网段。`cpetest`（`config::DEFAULT_TOKEN`）**只**保留为 agent 共享令牌的默认值
  （两机零配置互连靠它，随机化会断掉这条通路）——两者口径分开，别再合并。守在
  `util::tests::generated_console_tokens_are_well_formed_and_not_constant`。
- **控制台只认按 IP 访问的 `Host`，带 DNS 域名一律拒**（`webui::http::host_header_is_safe`
  / 纯函数核 `host_value_is_safe`）。这是 DNS 重绑定的防线：攻击站点把域名解析到控制台
  地址、诱浏览器发「同源」请求时，`Host` 带的是攻击者域名；控制台从来只按 IP 访问（启动
  打印、`--ui-bind` 都是 IP），所以只放行 IP 字面量与 `localhost`，缺省/空 `Host` 放行
  （原生客户端可能不带，而浏览器必带且 JS 改不了）。这道门排在鉴权之前但**只拒不授**，
  不违反「鉴权先于路由」（方向相反）。守在
  `webui::tests::the_host_gate_allows_ips_and_localhost_but_rejects_dns_names`。
- **控制台的判定基线是内置默认值，不是 `config.json`**（`webui::console_baseline_config`）。隐式加载的那份只留下「这台机器接在哪个网络上」：`agent_host` / `agent_port` / `agent_token` / `ipv4_prefixes` / `require_same_subnet_for_iperf`。`rate_check` 的门限与负载上限、`link_profiles.by_role`、`ctstraffic` 参数以前是从这份文件原样带进每一轮控制台运行的——同一份项目在「exe 旁边放了 config.json」的机器和没放的机器上判定口径不同，而项目文件里看不出来。`--config` 是**显式**选择，整份生效；区别不在文件内容，在于人有没有做这个选择。守在 `an_implicitly_loaded_config_only_contributes_connection_identity`。
- 项目文件 `project_version: 3` 是**完整有效快照**，分两层：`execution_defaults` / `acceptance` 是界面态（导出前经 `resolveEffectiveGlobals` 把留空的格子换算成真正会用的值），`master_config` 是**解析后的主控配置**（`RunRequest::master_config`）。后者用白名单 `MASTER_CONFIG_KEYS = [link_profiles, iperf, ctstraffic, ping]` 裁出来，在后端经 **深合并** 覆盖基线。
  - 为什么整块而不是逐字段：界面上没有输入框却决定判定与灌包的参数有几十个（`rate_check` 的负载上限/余量/并发流下限、`link_profiles.by_role` 的角色配对门限、`ctstraffic` 的帧率与缓冲深度），逐字段加通道永远追不完，漏一个就是一次静默的口径漂移。
  - 为什么白名单而不是黑名单：这份文件要传阅，将来给 `Config` 加一个口令类字段，黑名单会让它悄悄进项目。代价由 `every_config_field_is_either_snapshotted_or_deliberately_local` 兜住——`Config` 顶层加任何字段都会让它红，逼人做一次「参数还是本机身份」的判断。
  - 必须**深**合并：浅合并时项目里只写了 `iperf.rate_check.targets_mbps` 就会把整个 `iperf` 块换成 serde 默认值，比不合并还糟。
  - UDP 档位按**原样列表**走 `master_config.iperf.udp_profiles`，不拆成三条轴——三条轴是叉乘语义，`1000m` 单独带 `-l 64` 的档位表还原不回来。
  - 白名单块**内部**仍按本机身份剔除：`MASTER_CONFIG_LOCAL_PATHS` 目前是 `link_profiles.by_nic`。它的键是 `host + 接口名 + ipv4`，是「这台机器上这块网卡」的身份，不是判定参数。剔除必须导出、导入两侧都做——只在导出侧做，历史项目文件里已经带着的那份照样会生效。它跟着项目走会造成一个极隐蔽的故障：`rate::link_policy` 查 `by_nic` 用 `.find()`（首个匹配胜出），而项目带来的条目在 `apply_master_config` 里先落位、界面「按网口策略」后 `push`——项目里的旧条目盖过操作员刚填的数，界面显示 900、实际按 1800 判。守在 `per_nic_overrides_never_travel_in_a_project_nor_outrank_the_console_table`。
  - **合并失败必须响亮**：`apply_master_config` 返回 `Result`，`config_from_request` / `ui_request_base_config` / `config_from_ui_plan` 一路上抛到 `/api/plan` 与 `/api/run`。它以前在两处 `Err` 上静默 `return`——项目里任何一处类型不符都会让整块 patch 悄悄消失，这一轮改用目标机器自己的基线跑完，而界面上看不出区别。守在 `a_master_config_that_cannot_be_merged_stops_the_run_instead_of_falling_back`。
  - 导出侧同样不许产出空壳：`master_config` 为空在后端等价于「没带」，所以 `exportProject()` 在拿不到基线时**返回 `null` 并留错误**，而不是导出一个结构完整、换台机器就静默改判定口径的文件。随包示例项目 `dist/projects/cpe-ui-project-full.json` 就这么发出去过一次，`shipped-project.test.ts` 现在断言它带着四个块且不含 `by_nic`。
  - 重跑（`parseRunRequest`）要还原 `master_config`，当时没带项目就显式置 `null`。不还原它，重跑用的既不是归档里那份也不是本机基线，而是内存里当前碰巧加载着的那份。

- 内环双向无共同窗口时两腿均无有效判定；工具全程汇总仅在本腿窗口与共同窗口边界相差不超过 `cmd::iperf_window::WINDOW_COMPLETE_TOLERANCE_MS` 时参与双向验收，否则仅留作诊断。`LinkPreflight.counter_source` 可为空：严格策略拒绝，工具及优先网卡策略通过 `Sampler::Unavailable` 留存采样失败原因。板侧清单合并 sysfs、proc 与地址表，各读取路径独立降级。前端计划响应须同时匹配请求序号和当前配置快照才可落地。

- 内环 UI 的统计口径：上行板侧桥（默认 `br0`）RX、下行所选 PC 网口 RX、双向两者各一腿。界面不提供板侧成员口映射或候选，桥名称保留可编辑以适配机型；历史配置字段保持兼容。

- **速率统计的几条窗口规则**（2026-10 速率统计审查）：
  - 没有 iperf3 汇总行时（结果交换前就失败，发 TEST_END 时连接被重置），有效区间取
    逐秒行**合起来**覆盖的整段（`cmd::iperf_window::iperf_active_interval`）。以前取
    「最长的一行」，跑满 180 秒的测量只剩最后 1 秒，`IPERF_SUMMARY_LOST` 保住网卡
    口径的那条路径在最常见的形态下走不到。守在
    `a_full_run_without_a_summary_line_keeps_its_whole_window`。
  - 「区间 → 判定窗口」只有 `cmd::iperf_window::window_from_span` 一份（iperf 单腿、
    UDP 腿、双向合计、内环重叠共用）；UDP 截断前的区间是
    `executor::window::leg_active_span`。
  - CTS 窗口内的个别监控读数失败不再单独否决判定：缺口由 RX 采样覆盖率把关，和
    iperf/UDP 同口径；异常照样进诊断列。
  - 起流头 `settle_secs` 秒不进平均，所有后端、所有协议同一规则：iperf3 TCP、CTS
    TCP/UDP 的判定窗口从真实流量起点扣掉它（内环 TCP/UDP 用 `inner::plan::SETTLE_SECS`），
    进程相应多跑这一段。以前只有子网 iperf UDP 扣，同一条链路的数字在后端之间不可比。
    「进程实际跑多久」只有 `cmd::iperf_window::traffic_process_secs` 一份：执行端用它
    下发 `-t` / `TimeLimit`，builder 与内环计划用它估时（`est_secs` 进两份全量快照）。
    子网 iperf UDP 组由组调度器（`executor::window::leg_active_span`）按同一个参数扣。
  - iperf3 TCP 跑出过流量、执行环境也没问题、只是中途退出（被测设备重启、链路断开）
    判 `IPERF_EFFECTIVE_WINDOW_SHORT`，不再判 `SETUP_ERROR`——和 UDP、CTS 对同一件事的
    结论一致，熔断计数也不再把它记成「一个测量都没产生」。真正的环境问题由
    `IperfFlowVerdictIn::setup_error`（`iperf_client_setup_error` + server 停止未确认）
    单独给出，仍判 `SETUP_ERROR`。
  - 板侧计数器可能是 32 位（32 位内核 + 老驱动），满 4 GiB 回绕：
    `NicCounterReader::may_wrap_at_32_bits`（只有 `inner::adb::BoardCounters` 为真）打开后，
    `nic::monitor::counter_delta` 把「前后都在 32 位范围内、补一圈折算不超过 25 Gbit/s」的
    倒退按回绕补算，其余仍按复位丢掉那一拍。不认回绕时 2.5G 线速约 14 秒丢一拍，覆盖率
    掉到 95% 以下整条腿无法评价。
  - 工具自报速率只作展示，但口径要对：`IperfParsed::best_receiver` 只认 receiver 汇总行，
    拿不到就留空（以前退回 client 最后一行，即发送端某一秒的速率）；ctsTraffic 的发送 /
    接收速率取有流量状态行的平均（以前取峰值那一秒），没有状态行才退回摘要。
  - server 逐秒记录按区间长度认汇总行（`MAX_INTERVAL_LINE_MS`）：iperf3 3.1.x 的 UDP
    汇总行不带 `sender` / `receiver` 字样，混进逐秒记录会让断流旁证判反、窗口覆盖率被凑满，
    还会触发「新测试从 0 计时」把前面的逐秒行清空。
  - 已知、未修正的口径偏差：TCP 双向时接收端网卡 RX 里混有对向那条腿的 ACK（约为
    对向吞吐的百分之一二，网卡计数器分不开）。

- **防复发的五条机制**（2026-09-10 那轮横扫的产物；它们守的是「下一处」，不是已修的那几处）：
  - 历史目录的类型判断只有一种形状——不跟随符号链接的那种。枚举/打包/落盘历史的
    五个模块（`report/store`、`master/webui/runs`、`master/webui/scenario`、
    `inner/history`、`inner/webui`）的生产代码里不许出现 `.is_dir()` / `.is_file()` /
    `fs::metadata()`。守在 `history_modules_never_use_link_following_path_checks`。
    这张表**之外**的地方仍可以用跟随版本，那是有意的：`master::ui::replay_report_into`
    收的是人在命令行上敲的目录，把 run 目录做成软链再重放是合法用法。
  - 前后端的四条白名单（`size_token` / `safe_word` / `iface_word` / `adb_program`）
    与两个 HTTP 头长度上限，用例本身抽成 `src/inner/validation_corpus.json`，
    Rust 与 TypeScript 各读一遍。改规则必须先改语料，两边一起变红。守在
    `the_shared_validation_corpus_matches_the_rust_side` 与 `inner-corpus.test.ts`。
  - 控制台的每条 HTTP 路由都要声明并发类别（gated / readonly / stateful）。
    新增任何一条都会让 `every_console_route_declares_its_concurrency_class` 变红，
    作者必须回答「它会不会起测/停测/占用被测资源」。`stateful` 那几条不加门的
    **前提**是执行线程不回读 `console.state`（`api_run_impl` 起线程前已把 `cfg`
    快照下来）；哪天执行线程开始回读，它们就必须搬进 `gated`。
    界面侧另有一道 **UX 门**：`ConnectView` 的「连接」「重新扫描」（两者都发
    `/api/connect`）在 `run.running || inner.status.running || inner.scenario.running`
    任一为真时禁用，表单提交处理也检查同一道门，理由是页面显示的辅测机会和实际被测的那台对不上。
    `App` 首次挂载同时同步组合场景状态，刷新后无需先进入内环页才能锁定。
    它不替代后端门禁，后端也有意不拦这三个端点。
  - 编译期读的文件必须在版本控制里。`include_str!` / `include_bytes!` 的实参
    不许命中 `.gitignore` 里 `*.后缀` 那类「本机配置」规则。守在
    `no_compile_time_include_depends_on_a_gitignored_local_file`（`src/config.rs`）。
    它防的是一种**四条门禁完全没有分辨力**的故障：文件躺在开发机上，
    本地 fmt/test/clippy 全绿，而 CI 的 checkout 和任何新克隆里根本没有它，
    `cargo test` 连编译都过不去。历史实例——`src/inner/tests.rs` 编译期读
    `inner.local.example.json`，同一轮里另一个决定又把这类文件 gitignore 掉了，
    两个决定各自都对，凑一起就是新克隆编不过。规则从 `.gitignore` 现读，
    改忽略规则不用回来改测试。
  - 覆盖式重命名一律 `std::fs::rename`，生产代码不许手写 `MoveFileExW`。守在
    `no_hand_rolled_move_file_ex_in_the_tree`（`src/inner/tests.rs`）。
    「Windows 对已有目标返回 AlreadyExists」是假前提——std 第一步就是同一个
    `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING)`；手写版唯一的实际区别是丢掉
    std 在 `ERROR_ACCESS_DENIED` 上的 `FileRenameInfoEx` 兜底。这条前提在本仓库
    出现过两次：`master::executor::db::save` 靠人读出来，
    `inner::report::replace_file` 靠 Windows CI 的 `os error 5` 咬出来——
    **它在 `#[cfg(windows)]` 里，本机 macOS/Linux 的四条门禁压根没编译到它**。

### 11.2 常见修改入口

| 需求 | 首先改 | 必须联检 |
|---|---|---|
| CLI/新参数 | `main.rs`；master 参数另看 `master/ui.rs` | `main.rs` 的帮助 的解析测试、README |
| 配置字段/默认 | `config.rs` | `config.rs`、`config.example.json`、README，以及 `main.rs`、`master/ui.rs`、`master/builder.rs`、`master/executor.rs`、`agent/server.rs` 中对应消费者 |
| HTTP DTO/端点 | `protocol.rs` 或 `agent/server.rs` | `http_client.rs`、`master/ui.rs`、`master/executor.rs`、对应实现及 `agent/server.rs` 的解析/错误包装测试 |
| 任务数量/顺序/ID/端口 | `builder.rs`（外层循环）与 `builder/` 下对应后端的 `expand_*` | `builder/tests.rs` 的两份全量快照、`master/ui.rs`、`executor.rs`、executor 的 `sort_key` 构造与 `report.rs` |
| PASS/并发/监控/截图/RESUME | `executor.rs` | `executor.rs`、builder Unit/legs、`cmd/iperf/`、`ping.rs`、`nic/monitor.rs`、`screenshot.rs`、agent 对应端点及 `report.rs` |
| iperf 命令/解析/进程 | `cmd/iperf/` 下对应职责的文件（args / parse / server / client / jobs） | `cmd/iperf/tests.rs`、`protocol.rs`、`agent/server.rs`、`master/executor.rs`、`inner/adb_client.rs`（复用 client 执行与重试）、`cmd/ctstraffic.rs`（复用作业管理）、`util::run_streaming` |
| ping 命令/解析 | `ping.rs` | `ping.rs`、`protocol.rs`、`agent/server.rs`、`executor.rs` |
| NIC 角色 | `nic/classify.rs` | `nic/classify.rs`、`nic/mod.rs`、Windows/macOS 扫描、`builder.rs` 及 UI 角色选择 |
| 平台采集/监控 | `nic/mod.rs`、`nic/scan_windows.rs`、`nic/scan_macos.rs`、`nic/monitor.rs`、`cmd/ipconfig.rs`、`cmd/netsh.rs` | Windows GNU/MSVC；`ipconfig.rs`、`netsh.rs`、`scan_macos.rs`、`monitor.rs`，以及 main/agent/executor 调用方 |
| 报告列/HTML | `report.rs` | `executor.rs` 的全部 Row 构造；`report.rs` 的转义、排序/组合计、PASS/SKIP、截图链接测试（当前无 FAIL/golden） |

## 12. 变更与验证记录

历史回归记录在 `docs/testing/` 与 `.ai/REGRESSION-REVIEW-20260910.md`。
当前检查结果见 `docs/testing/project-review-2026-09-29.md`；测试数量随代码变化，不在架构索引重复维护。

### 内环进度与产物一致性补充

- `inner::webui::Controller::status` 以 `run_id` 和 `units_from` 配对续传；缺失或跨轮标识、越界游标均回完整列表，不能将上一轮的行数当作新一轮的游标。
- `inner::report::write_atomic` 同目录写完再替换，保证并发下载只看到完整旧版或新版。`save_progress` 每单元追加 JSONL、刷新摘要；重产物在单元边界按 30 秒间隔节流，`save` 收尾必写。这不是每 30 秒的后台定时刷新，最长陈旧时间受下一单元耗时影响。
- `history::Summary::finished` 为可选字段；新记录明确区分收尾与中间态，旧记录未知，不能从 `error == None` 推断完成。内环目前只提供 HTML/JSON，无 Excel 出口；子网 Excel 继续由 `report::xlsx` 输出。

### 网卡原始样本快照身份

`master::executor::artifact::Ctx::save_monitor_samples` 的附件身份包含端点和完整 CSV 内容摘要（含时间零点偏移）。双向 TCP 在同一网卡上的独立监控快照必须保存为不同附件，禁止后保存的快照覆盖前一条报告行的原始证据；相同快照可复用同一附件。由 `independent_monitor_snapshots_do_not_overwrite_saved_samples` 验证。

### CTS 过程事件口径

`cmd::ctstraffic::classify_line` 忽略工具的 Network Errors/Data Errors 星号说明行，仍保留以星号开头的真实故障；TCP client 的过程速率来自 SendBps，TCP server 来自 RecvBps，均转换为 Mbps。接收端事件不可用发送列代替。由 `real_cts_legends_and_receiver_status_preserve_event_meaning` 保证，真实故障行仍保留错误事件。
