# 补充审查与回归验收（2026-09-10）

本记录针对 Claude 两轮修改后的当前工作区，未提交或推送。前两轮结论不替代本轮实测，当前发现与未覆盖事项分别记录。

## 本轮修复

| 问题 | 触发场景与影响 | 修复与验证 |
|---|---|---|
| 内环增量进度跨轮混行 | 旧页面带上一轮行数重连，新一轮已产生结果时，旧行可能留在新结果中 | `run_id` 与 `units_from` 配对；缺失/跨轮/越界回完整列表。后端、状态层及实机 API 验证 |
| 下载可能读到部分报告 | HTML/JSON 正在被截断重写时并发下载或列历史 | 同目录写完再替换；并发读写回归与实机报告逐字节核对 |
| 下行故障错误指向 br0 | PC RX 计数停滞而工具有接收流量 | 诊断使用实际 receiver_host/receiver；下行回归断言不包含板侧或 br0 |
| 导入校验前后端不一致 | 带空白的 IPv4、负数或小数项目版本通过前端却被 Rust 拒绝 | 前端按后端语法拒绝，补异常文件回归，原配置不被替换 |
| 未收尾记录显示“已完成” | 正在运行或崩溃留下中间摘要；旧目录无摘要 | 新摘要明确保存 finished；中间态显示未收尾，旧记录显示未知，错误优先。浏览器确认三种状态；历史枚举同时排除目录符号链接 |
| 组合场景启动与状态回读连续丢失 | 启动请求可能已经提交但响应丢失；紧接着状态回读也失败时，本地仍是空闲且不会继续轮询 | 启动命令只发一次；不确定时回读失败也保留延迟状态轮询，避免后台场景失去 UI 跟踪；前端回归覆盖 |
| 子网→内环阶段交界停止竞态 | 停止请求落在子网结束、内环启动前的窗口时，两边执行器都可能暂时为空，场景线程仍继续启动内环 | 内环启动后立即复核取消位，命中时主动停止刚启动的内环并以失败收尾；避免停止语义被阶段切换吞掉 |
| 前后端工具参数白名单漂移 | 前端可导入但后端拒绝的 `serial`/`board_iperf` 值会造成配置看似成功、启动才失败 | 前端同步后端 `safe_word` 规则，拒绝空白、控制符、分号和前导 `-`；前后端均有回归断言 |
| Linux agent 网卡名路径逃逸 | 手写配置把 `../` 或绝对路径放进远端 monitor 的网卡名时，可能逃出 `/sys/class/net` 目录 | Linux 计数器入口拒绝路径分隔符、控制符、`.`/`..` 和超长组件；跨 Linux target 编译检查及平台单测覆盖 |
| UDP 丢包计数不完整的误导诊断 | 只解析到丢包百分比、缺少 `lost/total` 时，报告可能把问题说成“没有 receiver 汇总行”，门槛是否可核验也不清楚 | 单独标记“比例存在但计数不完整”，并明确已配置门槛无法核验；不改变速率判定口径，补回归断言 |
| 场景历史/启动错误掉出界面 | 视图直接调用会抛错的异步状态函数时，网络错误或按钮竞态只进 rejected Promise，用户看不到错误栏 | 在视图事件边界捕获场景启动与历史载入异常，统一写入 `inner.error`；不改变状态层的可复用抛错语义 |
| 组合场景线程创建失败留下未收尾记录 | 场景目录已落盘但工作线程因资源不足无法创建时，API 返回失败而历史仍是 `finished:false` | spawn 失败分支同步写入 `phase=failed`、`finished=true` 和错误信息，再复位控制器状态 |
| 场景状态旧响应覆盖新状态 | 页面初次加载的状态 GET 与启动后的状态回读并发，旧响应最后到达时会把 `running=true` 覆盖回空闲，且停止续轮询 | 为场景状态请求增加单调序列号，只允许最后发出的请求落地；乱序响应回归覆盖 |
| 停止后的下一次场景被旧取消位立即取消 | 子网停止与场景线程共用全局当前测试取消位；上一轮的位不会自动清零，下一次场景在第一阶段前检查时误判为取消 | 场景通过互斥检查并受理新一轮时调用 `cancel::reset()`；保留进程退出位，新增跨轮语义说明 |
| 历史配置载入期间可并发编辑/启动 | 恢复接口等待网络响应时没有占用 busy，用户可以在旧响应回来前修改或启动，随后历史配置覆盖当前编辑 | 场景与内环历史载入都纳入 `inner.busy` 的 try/finally 生命周期，保持异常可见且恢复后解锁 |
| 损坏的组合历史先改子网再失败 | 组合历史恢复先写入子网计划，之后才解析内环配置；内环历史损坏时会留下半载入的子网状态 | 先解析内环历史，再提交子网计划，避免跨模块半成功 |
| 场景停止与启动未共用互斥 | 停止请求落在场景已通过检查但尚未标记 running 的窗口时，可能返回未停止而让新线程继续 | 停止端点与启动共用 `run_gate`，持锁时直接发取消信号，避免嵌套锁 |
| 历史根目录本身可被链接替换 | 只拒绝 `runs/<id>` 或 `scenarios/<id>` 的链接仍会让根目录链接把整个外部目录暴露给列表/读取路径；写入侧若不检查还会把报告落到外部目录 | `runs`、`inner_runs`、`scenarios` 根目录的读写路径统一要求为真实目录，场景创建后再确认未被替换 |
| 报告存储子文件跟随符号链接 | `request.json`/`rows.jsonl`/`meta.json` 或内环 `units.jsonl` 被替换成链接时，历史读取/追加会触碰外部文件 | 存储层读写前检查 `symlink_metadata` 的普通文件类型；Unix 回归同时覆盖请求、行、元数据及内环追加文件 |
| 场景记录收尾写入跟随符号链接 | 场景线程结束时直接重写 `request.json`，运行期间被替换的链接会把收尾状态写到外部路径 | 创建与收尾统一经过普通文件检查的记录写入 helper |

另校正文档：30 秒是“单元结束时检查的重写间隔”，不是保证报告最多陈旧 30 秒的后台刷新。若下一单元很长，HTML/完整 JSON 可以落后更久，JSONL 已保留之前完成的单元。摘要生成仍遍历已有单元，重产物仍按时间节流整份写入，因此不宣称所有 O(N²) 工作已被消除。报告替换在 Unix 使用同目录 rename，在 Windows 使用带 `REPLACE_EXISTING` 的系统替换，避免第二次落盘因目标已存在而失败。

## 自动验证

- `cargo fmt --check` 通过。
- `cargo test --locked`：754 通过，0 失败。
- `cargo clippy --locked --all-targets -- -D warnings` 通过。
- `cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc -- -D warnings` 通过。
- 前端 npm ci、Vitest 312 条、build、verify 全部通过；内联产物戳 `3a35c0daec3ee1cfcaa6b210f2b7fd74`。
- `cargo check --locked --all-targets --target x86_64-unknown-linux-gnu` 通过；Linux 测试二进制因当前 macOS linker 无法链接，未冒充为已执行。
- dist 文档包重新生成并校验：`54213ae34793356c0ec65ba23098e05c104c10d4ac281cde053074df635a710c`。
- 第一次 Rust 检查与前端构建同时进行，溯源测试捕获旧产物；前端构建完成后按规定顺序完整重跑，全绿。没有忽略该失败。

## 实机内环

环境：OpenWrt 5.4.238 aarch64、板侧 iperf3 3.10.1、br0 192.168.8.1；macOS 主控 en0 192.168.8.100（1GbE）、en1 192.168.8.104（5GHz Wi-Fi）。这是当前连接设备，不是用户照片中的 192.168.0.1 机型。每腿 6 秒、TCP/UDP 各 2 流、UDP 每流 50 Mbps，无性能门限；MEASURED 表示形成测量，不能当作性能达标 PASS。

| 网口 | 协议 | 方向 | 上行 br0 RX Mbps | 下行 PC RX Mbps | 共同窗口秒 |
|---|---|---|---:|---:|---:|
| eth | tcp | upload | 884.50 | — | — |
| eth | tcp | download | — | 867.29 | — |
| eth | tcp | bidir | 824.54 | 851.83 | 5.973 |
| eth | udp | upload | 91.10 | — | — |
| eth | udp | download | — | 93.13 | — |
| eth | udp | bidir | 97.70 | 95.53 | 5.982 |
| wifi | tcp | upload | 634.65 | — | — |
| wifi | tcp | download | — | 1128.20 | — |
| wifi | tcp | bidir | 511.48 | 535.72 | 5.976 |
| wifi | udp | upload | 98.13 | — | — |
| wifi | udp | download | — | 88.92 | — |
| wifi | udp | bidir | 93.70 | 94.32 | 5.995 |

所有 12 单元、16 条腿均为网卡来源，覆盖率约 99.9%–100%。已核对：

- 所有实际 client 命令均无 `-R`；上下行接收端分别为板侧 br0 与所选 PC 网卡。
- 双向真实并发、独立端口，共同有效窗口约 5.97–6.00 秒。
- JSONL 逐条解析后与最终 result.json 的 units 完全相等；API 下载 HTML 与磁盘字节一致。
- 历史配置重新装载后计划逐字段一致；正确运行标识续传一条，旧运行标识回全部六条。
- 未配置门限保持 MEASURED；RX 低于门限、可信低速不能触发工具兜底、混合来源不能合计、无重叠不能验收、丢包只诊断等契约由完整测试覆盖。RX 核心判定和窗口算法未改。

eth 产物：`/Users/kk/uv/cpe_test/main/inner_runs/inner_20260910_072709_69901_267517000`。

wifi 产物：`/Users/kk/uv/cpe_test/main/inner_runs/inner_20260910_072820_69901_135731000`。

通过本机 agent 接收板侧下行时，先用板侧进程表确认普通 client 已运行，再发送 UI stop。取消结果保留，未执行后续双向单元；板侧资源目录、PC 接收端端口、agent server/monitor 均已回收。这里只证明 agent 协议路径，不能替代 Windows 独立辅测机。

## 子网功能

- 9 单元 TCP/UDP/Ping、ab/ba/bidir 计划；导出→导入→导出逐字段一致，plan_hash 一致。
- 内环误导入子网、子网误导入内环、无可识别字段文件均拒绝。
- 历史 `runs/run_20260909_212449_33244` 的 29 行在临时副本重放，不改原报告。HTML/Excel 生成成功；825 个有效 Excel 单元格的文字/判定一致，6 个浮点数仅有 JSON 往返末位误差（约 10^-14，以 10^-12 容差验证）。副本：`/private/tmp/cpe-extra-subnet-replay`。
- 子网实机执行链完成：9 单元，PASS 2 / RATE_FAIL 1 / MEASURED 4 / NOT_EVALUATED 2 / SETUP_ERROR 0；失败原因均可解释（Wi-Fi RX 计数器停滞、Wi-Fi 小包 Ping RTT 超门限），无最终清理错误。报告和 `summary.xlsx` 均生成，目录为 `runs/run_20260910_073330_75043`。这次运行是在同一台 Mac 上运行本机 agent，验证编排、报告、Excel 和判定链路，不能替代独立双机线速结果。

## 未覆盖与功能缺口

- 内环当前没有 Excel 出口，只有 HTML/JSON。子网提供 summary.xlsx。不能用“子网 Excel 验证通过”代替内环 Excel 支持。
- Windows MSVC 目标静态检查通过，但本轮没有 Windows 原生运行、ctsTraffic 实机或独立双 PC 线速验收。
- 未测试照片设备及其 USB/RNDIS、2.4GHz、2.5GbE；已测试当前设备的 ETH 与 5GHz Wi-Fi。
- 短时无门限矩阵验证功能和统计链路，不代替长时间稳定性、目标速率性能验收。

## 本轮最终门禁（2026-09-10）

- `cargo fmt --check`：通过。
- `cargo test --locked`：755 passed，0 failed。
- `cargo clippy --locked --all-targets -- -D warnings`：通过。
- `cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc -- -D warnings`：通过。
- `cargo check --locked --all-targets --target x86_64-unknown-linux-gnu`：通过；当前 macOS 链接器不能执行 Linux 测试二进制，未将该环境限制误报为 Linux 实机测试。
- `git diff --check`：通过。
- 前端最近一次完整门禁：313 tests、架构 lint、单文件构建、verify 全部通过；在补齐辅测机 ID、ADB 路径及 C1 控制字符的前后端边界校验后，内嵌产物为 344840 字节，源码戳为 `05546ac0dfd0a822ada331f148e0a24e`。
- Windows 配置文档包 23 项逐字生成并校验，SHA-256 为 `54213ae34793356c0ec65ba23098e05c104c10d4ac281cde053074df635a710c`。
- 最后一轮契约复审未发现需继续修改的字段漂移、互斥竞态或历史恢复半成功路径；场景状态中的运行 ID 字段当前为预留兼容字段，前端不消费，未做无收益的 API 变更。
- 追加修复：组合场景切入内环时立即把 `request.json.phase` 持久化为 `inner`，避免长运行期间历史页显示陈旧阶段；新增定向回归，随后 Rust 测试为 755 passed、0 failed，双 target clippy 通过。
- 修复后再次运行前端 `npm run test`（28 files / 312 tests）、`npm run build`、`npm run verify`，全部通过，内联产物仍为 344789 字节、源码戳 `3a35c0daec3ee1cfcaa6b210f2b7fd74`；`git diff --check`、`cargo fmt --check`、发布包校验仍通过。
- 追加修复：前端辅测机 ID 改用与后端相同的 `safe_word` 白名单，补测 256 字节边界；前端 312 tests、单文件构建/verify、Rust 755 tests、双 target clippy 与 Linux target check 全绿。
- 再追加修复：ADB 路径前端按 UTF-8 字节数同步后端 512 字节上限，补充多字节路径边界测试；前端 312 tests、Rust 755 tests、fmt、双 target clippy、Linux target check、diff 审计和发布包校验全绿。
- 再追加修复：前端地址、令牌和 ADB 路径同步 Rust 的 C1 控制字符拒绝规则；前端 312 tests、Rust 755 tests、双 target clippy、Linux target check、内联产物 verify、diff 审计和发布包校验全绿。
- 再追加修复：导入缺少 `measurement` 的旧/手写链路时，前端显式采用后端 serde 的 `nic_strict` 默认，避免与新建表单的推荐默认 `nic_preferred` 漂移；新增回归后前端 313 tests、Rust 755 tests、内联产物 verify、双 target clippy、Linux target check、diff 审计和发布包校验全绿。
- 继续复审打包边界：run 历史包只递归真实目录、只收普通文件，FIFO/设备节点不再被 `File::open` 读入下载线程；WebUI 历史相关 150 项定向测试全绿，随后继续执行全量门禁。
- 继续复审内环报告落盘：固定的 `.pending` 临时名若被预置为符号链接会被 `std::fs::write` 跟随；现在原子写入先拒绝非普通临时文件，并新增 Unix 回归验证不会写到目录外；内环 50 项定向测试通过，随后重跑全量 Rust 门禁。
- 上述修复后的全量门禁：`cargo fmt --check`、`cargo test --locked`（756 passed，0 failed）、本机与 `x86_64-pc-windows-msvc` 严格 Clippy、Linux target check、`git diff --check`、发布包 SHA-256 均通过。
- 再复审历史 zip 的递归边界：目录项现在携带不跟随链接的类型信息，递归入口也再次确认是真实目录；WebUI 定向 150 项与最终全量门禁再次通过，最终 Rust 计数保持 756。
- 继续复审远端采样生命周期：异常 agent 返回空 monitor ID 时现在立即拒绝并交给 owner cleanup，避免进入无法 stop 的正常路径；新增内环断言，最终 `cargo test --locked` 为 757 passed、0 failed，双 target Clippy、Linux target check、差异检查与发布包校验均通过。
- 再复审历史目录边界：runs、scenarios、inner_runs 不再在不跟随链接的类型检查后调用会跟随链接的 `is_dir()` 二次判断；WebUI 150 项、内环 51 项定向回归及最终 757 项全量门禁均通过。
- 补齐历史大小统计边界：`dir_size` 的递归入口也要求是真实目录，目录被替换成链接时不会把外部目录计入 run 大小；最终 757 项全量门禁再次通过。

原始脚本和本轮证据：`/private/tmp/cpe-extra-acceptance/`、`/private/tmp/cpe-extra-regression.py`、`/private/tmp/cpe-extra-subnet.py`；实际报告保留在项目的 inner_runs/ 与 runs/。

## 前两轮 CSV 完成记录（归档）

以下是接手时已完成的事项，属于前两轮记录，不全部算作本轮重新实测。

| 步骤 | 状态 | 备注 |
|---|---|---|
| 确认设备与测试矩阵 | DONE | 自研 CPE 实机 + 本机 en0/en1 |
| 内环实机验收（ETH 链路 12 单元） | DONE | TCP/UDP × 上下行/双向 全 MEASURED |
| 内环实机验收（Wi-Fi 链路 6 单元） | DONE | TCP 上726.78/下1175.18 Mbps；UDP 丢包 0% |
| 自动回归（cargo test 735 + vitest 297） | DONE | 全绿 |
| iperf 窗口逻辑抽取比对 | DONE | 6 个函数逐字节一致，无行为漂移 |
| 子网实机跑通（9 单元） | DONE | PASS2/FAIL1/MEASURED4/NOT_EVAL2，判定均可解释 |
| 导入导出往返一致性 | DONE | 配置逐字段 + plan_hash + 12 单元标题全一致 |
| 报告与 Excel 重放比对 | DONE | 688 单元格仅 1 处差异（v6.3.2 提示文案改进） |
| 内外环互斥与停止回收 | DONE | 4 个护栏生效；无残留进程/端口 |
| 历史目录隔离 | DONE | runs/ 与 inner_runs/ 无交叉 |
| 缺陷修复：导入保留 agent_host/agent_port | DONE | 含 notice；实机复现场景已验证 |
| 缺陷修复：拒绝无可识别字段的 JSON | DONE | 原来返回成功并刷成默认值 |
| 缺陷修复：内环侧识别子网裸配置 | DONE | 前后端同一份 SUBNET_ONLY_KEYS（11 键） |
| 修复后回归 | DONE | cargo 737 + vitest 297 全绿；fmt/clippy 干净；导入导出往返仍一致 |
| code-review 15 条：逐条核实 | DONE | 全部属实，无虚报 |
| 修复 1 勾选框取消最后一项卡死 | DONE | toggle 返回副本 + 最后一项显式禁用 |
| 修复 2 内环轮询无重试 | DONE | 只读轮询 3 次/250ms，与子网同口径 |
| 修复 3 回收失败被 Drop 二次执行 | DONE | 进入即标记已关；两个错误都上报 |
| 修复 4 容差守卫扫不到 iperf 链 | DONE | 注入违规验证过能咬住 |
| 修复 5 双向门限与合计值判据不一致 | DONE | 同一 same_source 判据 |
| 修复 6 adb_path 未校验形状 | DONE | 前后端同规则；文件名须以 adb 开头 |
| 修复 7 ADB 超时被丢弃 + 探测未缓存 | DONE | 超时透传；按(程序 |
| 修复 8 接口清单失败无诊断 | DONE | 记入 capability 并打印 |
| 修复 9 每单元重写不变的 config.json | DONE | 保留跑中可下载报告的能力 |
| 修复 10 每腿三次解析 + 重算窗口 | DONE | 降为一次解析、复用已算窗口 |
| 修复 11 草稿静默不保存 | DONE | 区分原因并丢弃过期草稿，界面提示 |
| 修复 12 过滤后排序按钮错位 | DONE | 按可见邻居移动，含回归用例 |
| 修复 13 project.ts 写死 kind | DONE | 改引用 INNER_KIND |
| 修复 14 帮助目录名少一段 | DONE | 补 <纳秒> |
| 修复后回归 | DONE | cargo 738 + vitest 298 全绿；实机内环 6/6 MEASURED |
| 第二轮 code-review 15 条：逐条核实 | DONE | 14 条属实，1 条（裸内环配置绕过）断言有误 |
| 修复 15 守卫仍漏第四处容差 | DONE | 加 inner/mod.rs 并认 required_secs 写法；注入验证 |
| 修复 16 Lease::close 双重回收 | DONE | 与 receiver_server 同规矩 |
| 修复 17 Sampler 先置标记再停 | DONE | 改为先停成功再记；方向与上条相反且注明原因 |
| 修复 18 adb_path 校验与执行不同值 | DONE | 第一条命令处即 trim |
| 修复 19 子网侧内环识别键补齐 | DONE | 3 → 9 个 |
| 修复 20 is_page 漏 HEAD | DONE | GET \| HEAD |
| 修复 21 inner status 无游标 | DONE | units_from；observer 改为只追加 |
| 修复 22 report() 持锁读大文件 | DONE | 取路径后放锁 |
| 修复 23 前端缺 IPv4 校验 | DONE | 与后端四类排除同规则，含前导零 |
| 修复 24 丢包可打印条件两处不一致 | DONE | 统一到报告的严格规则 |
| 修复 25 spawn 失败残留 current/total | DONE | State 整体复位 |
| 修复 26 rx_candidates dedup 漏非相邻重复 | DONE | 保序去重；注入验证 |
| 修复 27 零腿单元两处都不渲染 | DONE | 两层各补一行携带单元判定 |
| 修复 28 落盘 O(N^2) | DONE | units.jsonl 追加 + 重产物 30s 节流，收尾必写 |
| 修复 29 inner.local.example.json 未纳管 | DONE | 按本地文件 gitignore |
| 第二轮修复后回归 | DONE | cargo 741 + vitest 304 全绿；clippy 干净；实机内环 6/6 MEASURED |

### 2026-09-10 20:21 — 边界字段长度复审

- 辅测机 `address` 与 `token` 会分别进入 HTTP `Host` / `Authorization` 头；原先只拒绝控制字符，允许在 1 MiB 请求体内构造异常巨大的字段。后端新增 256 字节地址、4096 字节令牌上限，前端用 `TextEncoder` 按字节同步校验；新增 Rust/前端回归各 1 条。
- 本轮门禁：Rust 758 项通过；前端 314 项通过；Vite 单文件构建/溯源戳与 `verify` 通过；主机 Clippy、`x86_64-pc-windows-msvc` Clippy、Linux target check、`git diff --check`、发布包 SHA-256 全部通过。

### 2026-09-10 20:35 — 内环停止/启动互斥复审

- 发现普通 `/api/inner/stop` 直通内环控制器而未持有 `run_gate`；它可能与已通过检查、尚未清除旧取消位的 `/api/inner/run` 交错，导致停止信号被新一轮启动清掉。现在普通内环停止与启动共用 `run_gate`；组合场景分支仍由 `scenario.stop()` 自己加锁，避免二次加锁。
- WebUI 定向 150 项与 `cargo fmt --check` 通过；继续执行全量门禁。
- 同轮复审还发现 `/api/inner/probe` 的设备探测没有和内环启动共享 admission gate；现在探测也持有 `run_gate`，避免长探测与新一轮同时占用 ADB/板侧资源。既有场景/子网边界语义未扩大。
- shutdown/Ctrl+C 收尾路径也改为先完成场景停止、再持 `run_gate` 停普通内环，避免退出信号与 `inner/start` 交错时吞掉停止位。
- 修复后复跑：Rust `cargo test --locked` 758 项通过；主机与 `x86_64-pc-windows-msvc` Clippy、Linux target check、`git diff --check`、发布包 SHA-256 均通过；WebUI 定向 150 项保持通过。

### 2026-09-10 — 主控 agent 令牌 HTTP 边界复审

- 发现主控 `Config.agent_token`、命令行和 WebUI 连接参数最终都会进入自研 HTTP 客户端的 `Authorization: Bearer` 头；此前内环令牌有控制字符/长度校验，主控令牌却能把换行或超长值直接拼入请求。
- 新增统一 HTTP 边界：地址最长 256 字节、令牌最长 4096 字节且两者都不能含控制字符；配置校验、WebUI 连接入口和真实/注入 transport 发出前均复用同一规则，拒绝发生在状态修改/网络发送前；补 Rust 配置、HTTP transport、WebUI 回归各 1 条。
- 当前新增地址/令牌回归与格式检查通过；随后完整门禁为 Rust `cargo test --locked` 760 项通过，主机与 Windows target Clippy、Linux target check、`git diff --check`、发布包 SHA-256 均通过。

### 2026-09-10 — executor 增量结果复审

- 发现拓扑消失与 RESUME 跳过两条提前 `continue` 路径只更新了内存行和 UI observer，没有走 `rows.jsonl` 增量落盘；进程若在下一单元前退出，重放报告会漏掉已处理单元。
- 两条路径现在都调用同一份 `persist_new_rows()`，不改判定、DB PASS 语义或执行顺序；在既有 RESUME executor 回归中新增“跳过行必须落盘”的断言，定向 executor 120 项通过。

### 2026-09-10 — 执行器修复后的完整门禁

- 执行器增量落盘修复后重新跑完整门禁：`cargo test --locked` 760 项通过；主机与 `x86_64-pc-windows-msvc` Clippy、Linux target check、格式检查、`git diff --check` 与发布包 SHA-256 全部通过。
- UDP/CTS 生命周期复审未发现新的可证实缺陷：每轮 server/client stop、wait/reap、monitor 收尾均由 owner 级兜底清理覆盖；测量后运行异常仍只进入 diagnostics，不改写 RX 判定。

### 2026-09-10 — 主控地址记忆写入时机复审

- 发现 `.cpe_last_agent` 在辅测机健康检查成功前就写入用户输入，失败/输错地址会污染下一次默认值；写入已移到健康检查成功之后，保持“上一次实际连通地址”的语义。
- `master::ui` 定向 16 项及修复后完整 760 项 Rust 门禁通过；Windows target clippy、Linux target check、格式/差异检查和发布包 SHA-256 继续全绿。

### 2026-09-10 — 前端产物复核

- `ui/` 重新执行 `npm ci && npm run test && npm run build && npm run verify`：28 个测试文件、314 项测试通过；架构检查 77 个文件通过；单文件产物 344947 字节、源码戳一致，`verify` 通过。

### 2026-09-10 — 内环辅测机地址规范化复审

- 发现内环导入会接受地址首尾空格，但后端内环 HTTP 路径不会自动去掉它；主控 `/api/connect` 已经 trim，造成同一类地址在两个入口语义不一致（导入成功后探测失败）。现在 Rust `parse_config`、前端 `parseInnerProject` 和 `normalizeInnerDraft` 都只规范化地址，不触碰不透明的令牌；新增 Rust/前端回归各 1 条。
- 定向验证：Rust `inner_agent` 3 项、前端 `inner.test.ts` 25 项通过。并行启动全量门禁时 Rust 测试先于前端产物重建，曾按预期捕获旧源码戳；前端重建后已确认产物戳更新，待串行重跑 Rust 全量门禁。

### 2026-09-10 — 主控产物临时文件复审

- 发现主控 executor 的逐条原始产物与截图写入、以及 RESUME 结果库保存仍有可预测临时名/文件名；普通 `write` 会跟随本地预置的符号链接。现在临时文件和截图均用 `create_new`（临时产物再原子 rename），结果库同样改为安全临时写入；失败只影响旁路证据，不改变测试判定。
- 新增两条 Unix 回归，分别验证产物临时文件和结果库临时文件不会覆盖链接目标；定向 `symlinked_temp_file` 2 项通过。
- 地址规范化修复后串行 Rust 全量门禁已通过：`cargo test` 761 项，主机/Windows target Clippy、Linux target check、格式检查、`git diff --check` 与发布包 SHA-256 全部通过；前端 28 个测试文件、315 项测试及单文件构建/verify 通过。主控产物安全修复后再次通过 Rust `cargo test` 763 项及两套 Clippy、Linux target check、格式/差异检查和发布包 SHA-256；架构文档计数已同步为 763。

### 2026-09-10 — 截图证据文件名复审

- 产物写入改用 `create_new` 后复查到原文件名只有秒级时间戳；重复单元在同一秒会因安全写入改造而跳过截图。现在文件名加入进程内原子序号，保留 `create_new` 的符号链接防护并保证同秒截图各自可引用；新增唯一性回归。
- 定向截图/产物/DB 4 项回归通过；架构文档测试计数同步为 764。截图修复后的最终 Rust/前端门禁将在四小时窗口收尾前再串行确认。

### 2026-09-10 — 内外环原子落盘策略统一

- 复查内环 `report::write_atomic` 时发现它只拒绝已有的非普通 `.pending`，仍可能覆盖预置普通文件/硬链接；现与主控产物、RESUME 库统一为删除旧临时项后 `create_new` 写入，再原子替换目标。既有符号链接回归改为同时验证“不会越界”和“正常生成新报告”。
- `inner::tests::atomic_report_write_never_follows_a_symlinked_pending_file` 通过；随后最终 Rust 门禁再次通过：764 项测试、主机/Windows target Clippy、Linux target check、格式/差异检查与发布包 SHA-256 全绿。

### 2026-09-10 — Windows RESUME 库替换复审

- 复查跨平台落盘时确认 `ResultDb::save` 的目标文件会在第一个单元后已存在；普通 `std::fs::rename` 在 Windows 上不能可靠覆盖已有目标，原实现又静默丢弃错误，可能让 RESUME 库停在第一条记录。现在使用 `MoveFileExW(REPLACE_EXISTING)`，Unix 继续使用原子 rename；临时文件仍由 `create_new` 建立。
- `result_db_save_does_not_follow_a_symlinked_temp_file` 通过，Windows target Clippy 通过；随后最终全量 Rust 门禁再次通过：764 项测试、主机/Windows target Clippy、Linux target check、格式/差异检查与发布包 SHA-256 全绿。

---

## 2026-09-10 续轮：把横扫固化成机制（Claude）

上一轮 4 小时循环修了 35 条，但绝大多数只留下了针对**那一处**的用例。这一轮不再
继续找同类第 36 条，改为回答「同一类的下一处怎么办」，并补上被漏掉的残留。

### 地基（三件，缺一后面全白跑）

- 建 `CLAUDE.md`：这个仓库的契约写在 `AGENTS.md`，而 Claude Code 默认只读
  `CLAUDE.md`——之前几轮 Claude 是在**看不到四条门禁、三条铁律和联检表**的情况下
  审的代码。新文件只做转发（`@AGENTS.md` + `@.ai/PROJECT_ARCHITECTURE.md`），
  外加四条本仓库特有的审查注意事项。
- `git add -N` 近万行未纳管的新代码（`src/inner/` 7553 行、`ui/src/views/inner/`、
  `scenario.rs`、`iperf_window.rs` 等）。之前 `git diff` 只有 1153 行插入，
  锚定 diff 的审查工具**看不到九成的改动面**；纳管后是 12163 行。
- 回退 `src/master/executor/db.rs` 手写的 `MoveFileExW`。它的理由是「Windows 的
  `std::fs::rename` 不保证覆盖目标」，但 Rust std 的 Windows 实现本来就是
  `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING)`，而且在 `ERROR_ACCESS_DENIED` 时
  还会用 `FileRenameInfoEx` 兜住只读属性——手写版把那条兜底丢了，净效果是在
  主战场上比改之前更脆。证据：`$(rustc --print sysroot)/.../std/src/sys/fs/windows.rs:1311`。
  `create_new` 那半截（拒绝预置的符号链接）保留，并在函数注释里写明为什么不要再改回去。

### 新发现并修复

- **采样会话在起线程失败时永久泄漏**（`master/webui/monitor.rs`）。
  `MonitorData { running: true }` 在 `Builder::spawn` 之前就填好，而两个 spawn helper
  都写成 `let _ = ...` 把失败吞掉，照样注册会话并回 `started`。
  `reap_dead_monitors` 的规则是「`running == true` 就留着」——没有线程去把它翻成
  false，于是这条会话永久占着 8 个槽位里的一个，界面上还显示「运行中、没有错误」。
  线程耗尽在这个工具上不是假设：AGENTS.md 写着「跑测试时机器正在灌线速」。
  两个 helper 改为返回 `io::Result`，起不来就在**注册之前**报错。
  守在 `a_monitor_that_cannot_start_leaves_no_unreapable_session`。
- **符号链接横扫的残留**：`master::ui::create_run_paths` 里 `symlink_metadata` 判过类型
  之后又调了一次会跟随链接的 `runs_dir.is_dir()`——正是上一轮说「已经全部去掉」的
  那种二次复判。去掉。
- **产物文件名没有长度上限**（`master/executor/artifact.rs`）。`label` 是 `unit.title`，
  由用户配置里的链路名拼出来，不设限；其余拼进文件名的部分都有界。Windows 没开
  长路径支持时整条路径卡在 260 字符，`write_output_artifact` 只记一行日志就跳过——
  现场表现是「Windows 上截图莫名少了几张」，而开发机是 macOS（`PATH_MAX` 1024），
  永远复现不出来。标签截到 80 字符；文件名里的 `seq` 保证截断不会撞名。
  守在 `artifact_filenames_stay_short_enough_for_windows_max_path`。

### 补上没有测试的既有修复

- **网卡在开跑前消失**这条路径此前**完全没有测试**：`isolated_ctx` 里 `topology: None`，
  整条拓扑刷新分支从来没被执行过。补了 `EverythingGone` 假拓扑源，断言判死、
  增量 JSONL 落盘、进度收尾三件事都发生。（RESUME 跳过那条上一轮已有 JSONL 断言。）
- **受理新场景时清取消位的顺序**。`reset()` 若跑到互斥检查前面，一次被拒绝的启动
  会抹掉正在进行中的停止信号。用源码顺序断言而非行为测试：取消位是进程级全局量，
  在并行跑的 771 个测试里置位会让无关的 executor 用例随机提前收尾。

### 固化下来的三条机制

见 `.ai/PROJECT_ARCHITECTURE.md` §11.1 末尾。每条都做过**注入验证**——故意写回旧写法，
确认测试变红，再还原：

| 机制 | 守卫 | 注入验证 |
|---|---|---|
| 历史目录不跟随符号链接 | `history_modules_never_use_link_following_path_checks` | 加一处 `p.is_dir()` → 红（精确到行号） |
| 前后端 51 条共享校验语料 | `the_shared_validation_corpus_matches_the_rust_side` + `inner-corpus.test.ts` | 前端正则 `{1,256}` 改成 `{1,255}` → 红 |
| 每条路由声明并发类别 | `every_console_route_declares_its_concurrency_class` | 加一条 `/api/danger/wipe` → 红 |

### 本轮门禁

- `cargo fmt --check`、`cargo test --locked`（**771 passed，0 failed**）、本机与
  `x86_64-pc-windows-msvc` 严格 Clippy、`x86_64-unknown-linux-gnu` target check、
  `git diff --check`、发布包 SHA-256：全部通过。
- 前端：368 tests、`vue-tsc`、78 文件架构 lint、单文件构建、`verify` 全绿；
  内联产物 345012 字节，源码戳 `e5878c550acf6d9e495f09d175903669`。

### 记 backlog、本轮不改

- `/api/connect`、`/api/config`、`/api/import` 在跑测期间能改控制台状态。执行线程用的是
  `api_run_impl` 起线程前的 `cfg` 快照，所以**不影响正在跑的那一轮**，只是界面显示与
  实际被测对象可能不一致。已在路由清单里记成 `stateful` 并写明这个前提。

  **后续补正**：先前记录说这只有直接调 API 才碰得到，不对。前端确实没拦——
  `AgentView` 的连接按钮此前只按 `session.phase === 'connecting'` 和 `session.scanning`
  禁用，没读任何「进行中」状态。（`/api/config`、`/api/import` 则前端根本不调，
  实际界面可达的只有 `/api/connect`，连接与重新扫描两个按钮都走它。）
  已在 `AgentView.vue` 上加门：`run.running || inner.status.running || inner.scenario.running`
  任一为真就禁用这两个按钮，并给出可见理由（不是只挂 `title`）。
  这是 **UX 门不是安全门**——判定口径仍由后端 run_gate 保证，前端只保证
  「屏幕上写的就是正在测的」。后端保持不拦，因为拦不得：快照已经取走，
  拦了只会让人在跑测时改不了辅测机地址。
- 本地 `runs/`、`inner_runs/` 目录的符号链接 TOCTOU：攻击者要能往这些目录写东西，
  前提是已有本机写权限，那时换掉 exe 更省事。上一轮已按这条线扫过一遍，
  不再继续扩大改动面。

---

## 2026-09-10 发版验收轮：v6.4.0（Claude）

这一轮的目标是发版前验收，不是继续横扫。**开场时四条门禁在开发机上全绿，
而仓库处于「新克隆连测试都编译不出来」的状态**——这正是 CLAUDE.md 第 3 条
说的那种绿灯：它只证明没退回去。

### 发版阻断项（1 个）

`src/inner/tests.rs` 编译期 `include_str!` 读 `inner.local.example.json`，
而同一轮里 `.gitignore` 的 `*.local.example.json` 有意把这类本机配置挡在仓库外
（它们可能带 `agent_token`）。`git archive HEAD` 里该文件 0 个匹配，CI 的
checkout 和任何新贡献者都编不过。两个决定各自都对，凑一起就是编译失败——
这是上一轮修复 29「按本地文件 gitignore」只做了一半留下的。

处置：去掉编译期依赖而不是把本机配置塞进公开仓库（`.gitignore` 那条约定是对的）。
该测试的覆盖没有损失——`V1_PROJECT` 就是一份全本机、无 `agents` 的 JSON 字面量，
另有多处 `agents.clear()`。README 里指向该文件的那句一并删掉：公开 README 不该
指着仓库里没有的文件。

同时把这一整类静态扫了一遍：所有 `include_str!` / `include_bytes!` 的数据文件、
测试里走 `CARGO_MANIFEST_DIR` 读的路径、前端测试跨目录 import 的两个 JSON——
只有这一处。

### 第四条防复发机制

`no_compile_time_include_depends_on_a_gitignored_local_file`（`src/config.rs`）：
`include_str!` / `include_bytes!` 的实参不许命中 `.gitignore` 里 `*.后缀` 那类
本机配置规则。忽略规则从 `.gitignore` 现读，改规则不用回来改测试。
注入验证：加回那行立刻红，去掉即绿。

（写这条守卫时它先红了一次，抓的是它自己文档注释里引用的那行原文——
说明匹配确实生效，也说明这类自指要避开。）

### 干净树 + 真机验收

- 按 `git ls-files` 重建只含版本控制里实际有的文件的树（232 个），从零全量编译：
  `cargo test --locked` **772 passed**，`cargo build --release` 出 6.79 MB 二进制。
- 用该二进制对真实自研 CPE（OpenWrt aarch64，`br0` 192.168.8.1，iperf3 3.10.1）
  跑通整轮内环：TCP + UDP × 上行/下行/双向并发 **6 个单元全部 MEASURED**，71 秒，
  产物齐全（`finished: true`、6 行 JSONL、双向单元两条腿日志各一份、报告零外链）。
- 运行期冒烟：控制台鉴权三态、会话 cookie 的「只」字（只带 cookie 刷新页面 200 /
  打 API 401）、CSRF 头、8 条只读端点、`/api/inner/plan` 预览、agent 的
  `Authorization: Bearer` 三态与非法 JSON 单次包装，两侧日志 panic 计数 0。

### 核实过、判定不是缺陷的现象

6 秒时长下网卡口径系统性低于工具口径（上行 867.91 vs 901.00，差 3.7%）。
查 `rx` 明细为 `median 918.77 / min 640.54`——TCP 慢启动那一秒把 6 秒窗口的
均值拖了下去。改回文档默认的 20 秒复测：上行差 0.9%，下行网卡 945.47 **高于**
工具 920.00（网卡计以太帧头，本该略高）。测量层没有系统性偏差。

冒烟脚本首轮报的 8 条"失败"逐条核实**全部是脚本自身前提错误**，不是产品缺陷。

### 未覆盖

真实 Windows 双机、ctsTraffic 全链路、Windows 默认预设与 24 小时长稳。
Windows 原生测试与三平台打包由发布 CI 承担。
