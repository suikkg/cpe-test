# v6.2.9 稳定版回归测试报告

执行人：Claude (Opus 5)　执行日期：2026-09-06　
**验证提交：`ef497afd79433680404acb346653b5ec614bac7a` + 未提交工作区改动**（本轮八项修复）　
基线目标分支：`origin/main` = `485ba34`（v4.6.0），HEAD 领先 79 提交、落后 0 → 合并为 fast-forward

> 本报告只陈述**已执行**的部分。未执行项一律标 `NOT_RUN`，环境不具备项一律标 `BLOCKED`，不以 mock、不以 macOS/Linux 代替 Windows 实测。

---

## 1. 结论

**可以发版；Windows 那一半仍然缺证据，本报告不替它背书。**

判定内核在穷举复算下**完全符合契约**，92 项用例现在**没有一项是 NOT_RUN**——要么执行过，要么写明了为什么执行不了。修掉的 11 个缺陷里只有 D-04、D-01 的一层是本版引入的回潮/回归，其余全是更老的既存问题，其中 D-08（符号链接逃逸）、D-09（超限请求体被截断后照样执行）、D-11（`--config` 读不出来就用默认值跑完）三条各自都能让一次运行悄悄变成另一件事。

**没有满足的那一条**是方案 §8 的合入门槛：

> 「所有 P0 子项 PASS；必测范围没有 BLOCKED/NOT_RUN，**Windows 双机、CTS 和默认 Windows 预设有真实证据**。」

本轮唯一可用的第二台机器是 `192.168.8.101`（Arch Linux x86_64）。Windows 原生测试、发布包干净启动、ctsTraffic 全链路、默认 Windows 预设（`-w 256m`）、24 小时长稳与 100 次启停**全部缺席**。这些不是「没来得及」，是没有环境——20 项 BLOCKED 里绝大多数是同一个原因。

所以这一版的可信范围是：**判定与配置这条链上的逻辑经过了穷举与变异验证，Windows 上的运行时行为没有。** 拿去做 Windows 现场验收之前，§7 那张表里的项要先补。

同时本轮发现并修复了 **11 个缺陷**（D-05、D-06 出自对 D-01 那次改动的代码复查；D-06 起全部是更老的既存问题）。D-04 是 v6.2.9 引入的回潮；D-01 是**分层的**——单向门限那一层是 v6.2.8 的回归，双向合计那一层自 v6.2.4 起就一直在丢，通用门限与 `rate_mode` 则是矩阵路径从未接过的老缺口（详见 §4）。

## 2. 数量

| 状态 | 父项数 | 说明 |
|---|---|---|
| PASS | 72 | 已执行且满足验收标准 |
| BLOCKED | 20 | 缺 Windows 双机 / CTS / 第二块无线网卡 / 24h 窗口 / 桌面会话 |
| NOT_RUN | **0** | —— |
| **合计** | **92** | |

**「PASS」在这份报告里的含义**：该项的每一条验收标准都有对应的断言或真机证据，且新写的断言**逐条做过变异验证**（把被测的那道闸门改坏，测试必须立刻红）。变异证不了的地方（例如结构性成立、或缺少时钟注入口）都在对应行里写明了，没有当成证明。

自动化命令退出码：macOS 上 `cargo fmt --check` / `cargo test --locked` / `cargo clippy` / `cargo clippy --target x86_64-pc-windows-msvc` **四项全 0**；Arch Linux 上前三项全 0，**第四项无法在该机执行**——101 用发行版 rustc、没有 rustup，`x86_64-pc-windows-msvc` 的 `core`/`std` 未安装（`error[E0463]: can't find crate for \`core\``）。交叉目标的 clippy 证据只有 macOS 一份。前端 `npm ci/verify/test/build/verify` **五项全 0**；`git diff --check` = 0；dist 包 SHA-256 与 `.sha256` 一致。

测试计数（**修复后**）：macOS **682** / Arch Linux **686**，`#[ignore]` = **0**。两平台之差 4 已逐项归因（`scan_linux` 4 + `util` linux-only 2 − `scan_macos` 2），无隐藏跳过。对应基线（`ef497af` 未改动时）为 macOS 646 / Arch Linux 650，两边各 +36 与新增 Rust 断言数一致（其中 2 条是 `#[cfg(unix)]` 的符号链接测试，两平台都跑）。前端 Vitest 246 → **254**。

## 3. 环境矩阵完成度

| 环境 | 状态 |
|---|---|
| 开发机 macOS 26.6.2 arm64 + Arch Linux x86_64 | ✅ 完整 Rust/前端/契约 |
| Windows 10 x64 双端 | ❌ 无机器 |
| Windows 11 x64 双端 | ❌ 无机器 |
| Windows 10↔11 | ❌ 无机器 |
| 两台 Windows + CPE | ❌ 无机器 |
| 干净离线 Windows | ❌ 无机器 |
| 旁路双机（macOS ↔ Arch Linux，同 /24） | ✅ 已用于 FLOW/LIFE/OUT/PLAN 的真机层 |

旁路拓扑：主控 macOS `en0`(SGMII1G,1000M) 与 `en1`(WIFI5G,2401M)；辅测 Arch `enp2s0`(SGMII1G,1000M)。
**旁路配置显式标记为"不计入 Windows 默认预设验收"**：`-w` 按 macOS `kern.ipc.maxsockbuf`=8MiB / Linux `wmem_max`=4MiB 调小，内置 `256m` 预设在这两个平台上必然失败（AGENTS.md 明确这是预期行为，未去"修"它）。

**IPv6 真机层缺席，原因是环境不是程序**：两端裸 `ping6` 双向 100% 丢包，而同网段其它 IPv6 主机对 `ff02::1` 有 5 个回应；两台机器都跑着 `sing0` TUN 代理。工具在该路径上的表现是**正确的**——iperf 判 `SETUP_ERROR` 并完整保留 3 次尝试的原始错误，ping 判 `RATE_FAIL/PING_PACKET_LOSS_HIGH`。

## 4. 缺陷清单（11 个，均已修复并闭环）

### D-01 · 导出导入丢门限 —— P1，一半是 v6.2.8 回归、一半是更老的缺口

**按层分年份**（`git log -S` 核过）：`rate_targets_single_mbps` 由 7d36700 在 **v6.2.8** 引入导出侧、从未接导入侧，属回归；`rate_target_bidir_total_mbps` 自 **v6.2.4**（1492d4e）起就一直丢；`rate_targets_mbps` 与 `rate_mode` 在矩阵路径上**从来没有接过**。把整条 D-01 记成「v6.2.8 引入的回归」会低估它的年龄，也会让人以为回滚一版就能躲开。
`POST /api/import` 只回填 `rate_targets_bidir_mbps` 的 ab/ba 两格，**其余门限层全部丢弃**且 `notices` 为空数组：

| 配置字段 | 修复前 | 修复后 |
|---|---|---|
| `rate_targets_bidir_mbps`（双向每边） | 保留 | 保留 |
| `rate_target_bidir_total_mbps`（双向合计） | **丢** | 保留 |
| `rate_targets_single_mbps`（单向，v6.2.8 新增） | **丢** | 保留 |
| `rate_targets_mbps`（通用） | **丢** | 保留 |
| `rate_mode`（判定模式） | **丢** | 保留 |

后果：一份"单向 1800 / 双向每边 850"的配置往返一趟就变成"双向 850、单向没门限"，单向腿回落到网口门限——**正是 v6.2.8 修掉的那个现场问题被原样装回去**，而用户毫无提示。合计门限丢失会让判定口径从"两端 RX 合计比一次"退回"逐方向各比一次"；`rate_mode` 丢失会把 verify 悄悄降级成全局默认。

修法（按用户裁定：**必须真保留，不是提示了事**）：`PairSelection`（请求）与 `PairImport`（回填）对称补齐这五层，`specs_for_pair` 写出、`pairs_from_tests` 读回，并沿用既有的 `flip` 归一（同一对网口反向出现时方向敏感的门限跟着交换）。`forward` 简写在回填时摊进空着的 ab/ba——**判定等价**，因为这条链上 `for_direction` 的实参只会是 `ab`/`ba`。
回归断言：`every_rate_target_layer_survives_a_download_import_round_trip`（逐字段比对，**撤掉任一侧都会红**，两个方向均已验证）+ `a_forward_shorthand_is_spread_into_both_directions_on_import`。

### D-02 · 同一个词在两个出口是两个意思 —— P1，OUT-01 类
`RunSummary::fail`（命令行汇总）把 `NOT_EVALUATED` 与 `SETUP_ERROR` 也累加，`RunCounts::fail`（控制台进度）只数 `RATE_FAIL`——而 `RunCounts` 的文档注释白纸黑字写着"字段与 `RunSummary` **同名同义**，不另起炉灶"。
真机撞上：一轮 6 单元里 `RATE_FAIL` **一条都没有**，命令行却打印 `FAIL: 2`（那 2 个是 SETUP_ERROR，在同一行被数了两遍）。做验收的人会据此报"两个吞吐不达标"，而控制台上同一轮显示 0 失败。
修法：判定→计数收敛为唯一映射 `RunSummary::bump`；退出码另立 `any_not_passed()`，**逐字节保持原有语义**（非 PASS 一律非 0 退出）。
回归断言：`counters_mean_the_same_thing_on_both_exits` + 结构断言 `the_verdict_to_counter_mapping_has_no_third_copy`（后者用还原内联 match 证明会红）。
真机复验：17 单元一轮，逐单元 PASS 10 / RATE_FAIL 3 / NOT_EVALUATED 2 / SETUP_ERROR 2，与汇总行**逐格相等且合计 = 17**。

### D-03 · 流数灌不到门限时，跑完才知道 —— P1，CFG-07 输入边界
`udp_streams=2`、每流 `-b 300m`、门限 850Mbps 时，必需并发流数 = `ceil(850×1.05/300)` = **3 > 2**，"所有必需流并发活跃"的判定窗口**按构造永不成立**，整条腿稳定判 `NOT_EVALUATED / EFFECTIVE_WINDOW_SHORT`。
三件事让它值得挡：①完全确定，不是概率性的；②计划期已知全部输入；③用户拿到的原因码指向**采样窗口**，真因却是流数不够，排查方向被带偏一整层。180s 的 Windows 预设下，一轮里**每一个** UDP 单元都会这样白跑。
修法：计划期点名，说清需要几条、跑完会得到哪个原因码、以及三条出路。公式复用执行端那一份（`required_udp_streams` 提为 `pub(crate)`），**不在 builder 里重写**——那正是刚在计数层修掉的同一类分裂。
回归断言：`a_stream_count_that_can_never_reach_the_target_is_called_out_before_the_run`。
连带修正：`udp_resume_id_is_independent_of_tcp_stream_configuration` 的夹具流数由 4/3 调到 16/15（EVB 门限 6400Mbps、每流 500Mbps 需 14 条）。**一条断言都没放松**，只是让夹具的负载变得可行。

### D-04 · 界面又出现 ctsTraffic —— P1，明确用户要求的回潮
`ui/src/App.vue` 的 eyebrow 写成 `双机链路测试 Ping / iperf3 / ctsTraffic`。用户明确要求过不止一次，v5.0 时就在**同一个位置**被指出并改过一次。
已改回 `Ping / iperf3`，并把约束钉进 `ui/scripts/lint-arch.mjs` 的全局禁令（**仅作用于 `.vue`**；Rust 后端、报告列名 `CTS/TCP`、日志不受限）。用还原变异证明闸门会红。口头约定挡不住第三次。

### D-05 · D-01 补进来的那几格绕过了校验 —— P1，**本轮代码复查发现**
D-01 把单向门限、通用门限和 `rate_mode` 接进矩阵路径时，只接了「读得懂就用」那一半：
`specs_for_pair` 里 `text_target` 是 `parse_rx_target(..).ok().flatten()`，`rate_mode` 是 `_ => None`
——**看不懂就当没填**。而 `validate_pair` 只校验双向那三格。于是同一个错误写法，落在不同格子里待遇完全不同：

| 输入 | `rx_target_bidir_ab` | `rx_target_single_ab` / `rx_target_ab`（D-01 新增） |
|---|---|---|
| `"abc"` | 报错「看不懂的门限写法」 | **静默失效** |
| `"90%"` | 报错「只能填绝对 Mbps」 | **静默失效** |
| `rate_mode: "verfiy"` | — | **静默回落到全局模式** |

还有第四种：填了 `rx_target_single_ab` 却只勾了双向——`UiTask` 和双向那三格在同样形状上都是硬报错，这里静默丢弃。
四种情形已用实测确认（非法值全部被 `validated_config_from_request` 接受，产出 `single=None generic=None mode=None`）。
最后 `rate_mode` 那一行尤其要命：D-01 修的就是「verify 被悄悄降级成全局默认」，把校验漏在请求侧，等于把同一个 bug 从导入侧搬到了请求侧。

修法（**按用户裁定：硬报错，与既有口径一致**）：
- 四格门限补上和双向同一道校验（解析错误上抬、百分比拒绝）；
- 单向那两格补方向门禁；**通用层不加**——它是门限链最后一层，单向双向都吃，勾哪个方向都成立；
- `rate_mode` 改白名单，认不出就报错并列出认得的四个值，留空仍表示跟随全局。

用户当时问「这样会导致其它配置没能导入吗」——**不会**：`/api/import` 这条路上根本不跑 `validate_pair`，校验发生在之后的 `/api/plan` / `/api/run`。真正要配套的是回填侧，见 D-06。
又问「为什么不在导出时或 webui 设置时提醒」——控制台那条路（`UiTask`）**一直就是硬报错**（`validate.rs:861` 起）；矩阵这条路按 ADR-13 封存、界面上没有入口，没有「设置时」可提醒；而「导出时」（`config_from_request`）跑在校验**之后**，提醒来得太晚。硬报错让两条路对同一个错给同一个反应。

顺带按裁定消歧了字段名：`PairSelection` 的通用层由 `rx_target_ab/ba` 改名为 `rx_target_generic_ab/ba`（`UiTask.rx_target_ab` 指的是**单向层**、排在按网口门限**之上**，而这一层排在**之下**，两个 DTO 挂在同一个 `RunRequest` 上）。旧名保留为 `#[serde(alias)]`，老请求照样解析。

回归断言：`the_new_target_layers_reject_what_the_old_ones_reject`、`a_single_direction_target_needs_a_single_direction_selected`（含「通用层不许误伤」的反面）、`the_old_generic_target_field_name_still_parses`。三条均已用变异证明会红。

### D-06 · 导入回填塞进死值，让这一行再也跑不了 —— P1，**既存问题，非本轮引入**
`pairs_from_tests` 逐 test 回填方向门限，却不看这一行最终有没有那个方向；`validate_pair` 又要求「填了方向门限就得勾对应方向」。两者对不上时的表现极难查：**导入成功、`notices` 干净，点开始才报错**，而报错指着的那一格用户从没碰过——是导入自己塞进去的，还让他去取消勾选。

实测复现（本轮变异验证）：手写 config 只跑 `A→B`，却在 test 上留了一份 `rate_targets_bidir_mbps`（这在只跑单向时是**死值**，`leg_rx_target` 只在 `bidir` 时才读它），导进来即得
`ERR = 配对 … 填了 A→B 双向门限，却没有勾「双向」`。

这条**不是 D-01 引入的**：双向那三格从有回填起就这样。D-01 新接的单向两格会长出对称的一份，所以一并按同一条规矩处理。

修法：`pairs_from_tests` 末尾加一遍**最终方向集**裁剪——行内没有 `bidir` 就清掉双向三格，没有 `ab`/`ba` 就清掉单向两格。必须是最后一遍：`ab` 与 `bidir` 两条 test 并成一行时两层都要留下，按逐 test 判断会把该留的也裁掉（这一面单独立了断言）。
回归断言：`importing_never_backfills_a_target_the_row_cannot_run`。变异（拿掉裁剪）立即红。

### D-07 · 报告里印出一句不成立的不等式 —— P1，**RATE-06 发现，v6.2.7 起既存**
v6.2.7（`eb003e8`）把 RX-P10 诊断的触发线从「低于目标」放宽到「低于目标的 **90%**」——改对了，P10 只是诊断指标、不参与 PASS/FAIL，正常抖动不该次次报警。但**核对不通过时印的那句话没跟着改**：条件比 `target * 0.9`，文案却拿 `target` 去比。

实测（目标 800、P10 750）：

```text
判定原因与展示指标不一致: RX_P10_BELOW_TARGET；RX-P10 750.000 Mbps >= 目标 800.000 Mbps
```

**750 >= 800 不成立。** 而这一行出现的时机偏偏是工具**判对了**的时候（P10 在 720..800 的容差内、正确地不报警），却让读报告的人看到一个算错的不等式——报告正是拿去做验收证据的那份东西。

修法：两条分支共用同一个 `floor = target * 0.9`，不一致提示改成「>= 目标 90% 720.000」。
连带更正两条既有断言——它们把错误文案**逐字钉住**了（`rate_reason_validation_uses_the_metrics_shown_to_the_user` 与 `contradictory_direction_reason_is_flagged_in_overview_and_bidir_summary`）。断言强度没变，仍是全等比对，只是钉住的数对了。
回归断言：`the_p10_diagnostic_compares_against_ninety_percent_and_says_so`。**改动之前，把 `0.9` 改回 `1.0` 一条测试都不会红**——那条 90% 线此前零覆盖。

### D-08 · `runs/` 下的符号链接会被当成一次运行打包出去 —— P1，**SEC-05 发现，既存**
`/api/runs/<id>/bundle.zip` 的 id 解析是**白名单式**的——枚举 `runs/` 下的目录名精确比对，请求串不参与任何路径拼接。这挡住了全部「能表示上级目录的写法」（`..`、编码穿越、绝对路径、Windows 盘符），既有测试也把这些逐条钉住了。

但它比对的是**名字**，不是那个名字指向哪儿。实测（探针）：

```text
runs/sec05-link-probe -> /tmp/…            resolve => Some("runs/sec05-link-probe")
                                            bundle  => 打包成功，含链接目标里的文件
```

run 目录**内部**的链接同样被跟着走，而 zip 里的条目名仍是 `run_xxx/…`——解开的人看不出内容根本不来自那次运行：

```text
["sec05-inner/escape/secret.txt", "sec05-inner/report.html", "sec05-inner/secret-link.txt"]
```

**单看不是提权**：能在 `runs/` 里建链接的人本来就读得到那些文件。危险在于它**跨了信任边界**——`--ui-bind` 之后控制台在局域网上，于是「本地任何一个以当前用户身份跑的东西写下一个链接」被放大成「远程用户凭口令读任意文件」。

修法：`resolve_run_dir` 用 `file_type()`（不跟随链接）与 `is_dir()`（跟随）双重判定；`collect_files` 跳过所有符号链接条目。这个工具自己从不在 `runs/` 里建链接，挡它零代价，也和该模块既定的思路一致——不去猜有哪些危险写法，只认自己产出的东西。
回归断言：`a_symlink_in_the_runs_directory_is_not_a_run`、`a_symlink_inside_a_run_directory_is_not_bundled`，外加补上白名单的**正面**一半 `a_real_run_directory_still_resolves_to_exactly_itself`——既有的传统测试通篇断言 `is_none()`，把 `resolve_run_dir` 改成无条件返回 `None` 它照样全绿，而「下载报告包」这个功能已经死了。三条均已变异证红。

### D-09 · 超限的请求体被静默截断，服务端照样据此执行 —— P1，**SEC-04 发现，既存**
主控与 agent 的请求体都是 `take(MAX_BODY)` + 读到底：**超了就静默截断**，没有任何显式的大小检查。实测（主控 `/api/import`，上限 1 MiB）：

| 请求体 | 修复前 | 真正的问题 |
|---|---|---|
| 上限+1，尾部是空白 | **200，导入成功** | 截断后仍是合法 JSON，服务端**据此执行了导入**；用户手上的文件和实际生效的配置不是一份东西 |
| 上限+1，截到内容里 | 「这不是一份能解析的 config.json」 | 真因是请求体超限，人会去查自己的文件 |

第一行和 D-01 是同一类——配置静默地变成了另一份，区别是这次截掉的是任意内容。agent 侧同一写法的后果是**启停指令本身**被改写。

修法：多读一个字节，好让「刚好到上限」和「超了」分得开；超限一律拒绝并说明真因，**放在鉴权之后**（铁律三：先认人再谈请求内容）。两侧都改。
回归断言：`a_body_at_the_limit_works_and_one_byte_over_is_refused_not_truncated`——覆盖 limit-1 / limit / limit+1 各两种补白位置、以及未认证超限请求仍先 401 且不泄露上限值。两处变异（退回静默截断、把检查挪到鉴权之前）立即红。

### D-10 · 一个挂死的请求把控制台的轮询永久卡住 —— P1，**UI-05 发现，既存**
`run.ts` 用 `inFlight` 保证同时最多一个在飞的请求（旧页 `setInterval` 叠着发，抢的是被测链路自己的带宽）。但 `fetch` **不带任何超时**，而 `inFlight` 是模块级的、`reset()` 也不清它。于是连接断在半路时：

- 后续每一拍都被闸门挡掉，`tick()` 在进 `try` 之前就返回；
- `run.refreshError` 因此**永远不会被赋值**——屏幕上没有任何错误，只是数据一直停在上一拍；
- 「断开连接 / 换辅测机」也救不回来。

11.5 小时的长测试里网络抖一次就够了，而这台机器此刻正在灌线速——半死的连接恰恰是最可能出现的。

修法：`client.ts` 加 `AbortController` 超时（默认 120s，照着最慢的合法端点 `/api/local` 的 Windows 网卡扫描定；进度轮询用 30s），超时落到既有的「断线」分支，把「这份是旧的」显示出来；`reset()` 清 `inFlight`。
回归断言：`挂死的请求不会永久卡住轮询` 两条 + `轮询不叠加` 两条。两处变异（reset 不清闸门、请求不带 signal）立即红。

### D-11 · `--config` 指定的文件读不出来时，用默认配置把整轮跑完 —— P1，**CFG-01 发现，既存**
`load_config` 此前没有任何测试。实测 `--config bad.json`：

```text
!! 配置文件 …/bad.json 解析失败: EOF while parsing a value at line 1 column 25
!! 将使用默认配置继续
→ duration=180  agent_host=""   （用户写的那份里的门限一个都没生效）
```

代价不是「跑失败」——是**跑成功**，然后交出一份按用户从没写过的门限判出来的报告。命令行上刷过去的那一行 stderr 警告，在 CI 日志和滚动的终端里等于不存在。

修法：显式指定的那一份解析失败**或文件不存在**都改为致命错误（`load_config_checked` 返回 `Err`，`load_config` 打印后退出 1）。**隐式那条路保持原样**——不带 `--config` 时「碰运气看看旁边有没有」，退回默认本来就是它的语义。
同时把候选路径抽成可测的纯函数 `config_candidates` 并钉住顺序：显式指定时**只有一个候选**（滑到当前目录那一份上意味着跑的不是我指定的那份，而两份文件的门限完全可以不一样）。
回归断言：`an_explicitly_named_config_never_silently_falls_back_to_defaults`、`config_lookup_order_is_explicit_then_cwd_then_next_to_the_exe`。两处变异立即红。

## 5. 判定内核的独立验证（本轮最有分量的正面结论）

| 项 | 做法 | 结果 |
|---|---|---|
| RX 判定真值表 | 按 AGENTS.md 铁律 2 与方案 §3 的**契约文本**独立写期望，非照抄实现。穷举 4 模式 × 6 种目标 × 7 种采样状态 | **一次通过**，含 `Verify` 缺目标 → `TARGET_MISSING`（不是 MEASURED）、`Observe/Discover` 强清目标、等号归 PASS、NaN/inf |
| 可信度边界 | 三道门槛全部用**常量表达式的 f64 上下邻值**（ULP），不用十进制字面量 | 通过；反面"三道都恰好在通过侧"必须给出速率结论亦成立 |
| 聚合优先级 | 按文档条号建独立全序模型，穷举 13 个代表项的全部单项 + **169 有序对（双向）** + **2197 三元组** | 全部一致，且与传入顺序无关 |
| 门限四层优先级 | 四层同时钉上再逐层剥离，数字与**预览里印的来源**同时断言 | 通过，防重排 |
| RX 独立复算 | 从落盘的**原始字节计数器 + 单调时间**自己算，不读工具算好的 `rx_mbps` 列 | 8 行全部对上，最大相对偏差 **0.76%**（多数 <0.1%），**每行复算判定与工具判定一致** |
| 计划哈希闸门 | 变异测试：把 `canonical_unit_for_fingerprint` 改成 no-op | 立即红——闸门是活的，不是摆设 |

## 6. 真机层已确证的行为

- **LIFE-01 停止全生命周期**：运行中第 6/48 单元 SIGINT → 主控 3 秒退出；主控 iperf3 归零；**辅测机上监听 56004/56005 的两个 iperf3 server 亦被回收（0 残留、0 监听）**，agent 进程 PID 不变；生成部分报告 `report.html` + `summary.xlsx`；退出码 1。
- **FLOW-07 Ping 判定**：三种结果在真机上都出现且判定正确——v4 正常 PASS；v6 收/发 0/6 判 `PING_PACKET_LOSS_HIGH`；Wi-Fi 平均 RTT 50.632ms > 30ms、最大 106.477ms > 80ms 判 `PING_RTT_AVG_EXCEEDED`。
- **FLOW-08 健康熔断**：连续 2 个零测量灌包单元后按设计告警，且**不擅自中止**（配置未开）。
- **PLAN-09 单向门限**：用户原始报告的场景在真机上闭合——有线↔有线单向门限 850 生效并判 PASS（独立复算 973.2Mbps），双向另设 400 未被单向值污染。
- **SEC-01/02/03**：10 格鉴权矩阵、会话 cookie 的"只对 `GET /` 生效"、CSP 与自包含（外链/`@font-face`/`eval`/`setInterval`/`@keyframes`/`backdrop-filter` **全为 0**）均在活体 HTTP 与浏览器上验证。

## 7. 未测范围与具体原因

| 范围 | 原因 |
|---|---|
| WIN-01…04、STAB-01…03、FLOW-06(CTS)、DEV-01 的 Windows 原生半边 | **无 Windows 机器**。唯一第二台是 Arch Linux |
| RATE-08 Wi-Fi 双向合计的真机层 | 101 的 `wlp3s0` 处于 DOWN，无第二块无线网卡，构不成 Wi-Fi↔Wi-Fi |
| FLOW-01 的 IPv6 半边 | 环境：两端 `sing0` TUN 代理导致 ICMPv6 双向不通（已排除程序原因） |
| PLAN-06/07 的真机层 | 未构造出"预览缓存拓扑 vs 执行前重扫"两次 PHY 读数不同的真机时序 |
| UI-11 的响应式与对比度实测 | 只做了可访问性的键盘半边 |
| 其余 48 项 | 本轮未执行 |

## 8. 交给用户的决定

1. 十一项修复已随 **v6.3.0** 一并提交（见 `release: v6.3.0`）：版本号、dist 配置文档包重建、README 与 Windows 说明、工作流发布说明同步更新。
2. 合入到主分支前仍需补齐 Windows 双机证据。本报告对 v6.3.0 的背书**只覆盖判定与配置这条链上的逻辑**，不覆盖 Windows 上的运行时行为。

### 已裁定（2026-09-06）
| 议题 | 裁定 | 落实 |
|---|---|---|
| 填了单向门限却只勾双向 | **硬报错**，与 `UiTask`／双向三格同口径 | D-05；配套的回填裁剪见 D-06 |
| `rx_target_ab` 在两个 DTO 里同名不同层 | **改名 + serde alias** | 通用层改 `rx_target_generic_ab/ba`，旧名保留 alias |
| D-03 是提示还是拒绝 | **保持提示** | 不改；理由是 Observe/Discover 下 `leg_rate_plan` 仍可能算出 target，拒绝会误伤合法探测跑法 |
