# 提示词：cpe_test 设计方案重构评估（给 Claude 的审查任务）

> 用法：把本文件全文粘贴给 Claude（或让它读取本仓库后执行）。本文件是"审查任务书"，不是分析结果。
> 目标仓库：https://github.com/suikkg/cpe-test ，当前版本 v4.2.6（commit 1fd3c2e），Rust 2021 单 crate。

## 角色

同时以三个 15 年实务经验的立场审查，立场冲突时按下列顺序裁决，并在报告里注明分歧点：

1. **高级测试开发 / 自动化工程师**：关注可测试性、可观测性、故障注入、回归防线、CI 质量门禁的真实效力、测试产物（HTML 报告）作为验收证据的完整性。
2. **产品工程师**：关注"两台 Windows 10+ 电脑、零 Python/零 PowerShell、小白用户"这条真实用户旅程，配置复杂度、错误信息是否可行动、升级与兼容（resume）、发布链路成本。
3. **网络工程师**：关注测量口径的物理正确性——接收端 OS 网卡计数器 vs 工具自报速率、有效时间窗口切分、TCP/UDP 语义、丢包/抖动/RTT、iperf3 与 ctsTraffic 参数映射、跨平台网卡 API 差异、双机时钟/时间对齐。

你的经验应体现为**判断框架和校准**（比如：什么量级的测量误差可接受、什么测试才是真回归防线、什么配置复杂度会劝退非专业用户），而不是堆砌形容词。

## 背景事实（请自行到源码验证，不要盲信）

- 单 crate，无异步框架：tiny_http + std::thread + 手写状态机；CI 在 Windows/macOS/Linux 三平台构建。
- 单测 309 个，质量门禁 = fmt + test + clippy(-D warnings) + JSON/资料包校验 + 发布资产断言。
- 规模参考（当前工作树）：src/master/executor.rs ≈ 10.9k 行，src/master/builder.rs ≈ 3.1k，src/cmd/iperf.rs ≈ 2.8k，src/report.rs ≈ 2.4k，src/master/ui.rs ≈ 1.6k，src/util.rs ≈ 1.3k，src/http_client.rs ≈ 1.2k，src/nic/monitor.rs ≈ 1.1k。
- 架构索引见 .ai/PROJECT_ARCHITECTURE.md（生成于 2026-08-11，行号可能已过期；**发现它与源码不一致本身就是一项有价值的输出**）。

## 阅读顺序与资料

1. .ai/PROJECT_ARCHITECTURE.md（索引，行号过期时以源码为准）
2. src/main.rs（CLI 模式与模块边界）、src/protocol.rs（双机 DTO 边界）、src/config.rs（配置面）
3. src/master/ui.rs（编排）→ src/master/builder.rs（任务规划）→ src/master/executor.rs（执行主循环，重点）
4. src/agent/server.rs（agent 控制面）→ src/http_client.rs（RPC 层）
5. src/report.rs（报告/判定）、src/nic/*（网卡扫描与采样）、src/cmd/iperf.rs、src/cmd/ctstraffic.rs、src/ping.rs
6. 用户面：README.md、使用说明.md、config.example.json、UDP并发灌包验收场景.md
7. CI：.github/workflows/build.yml

## 必答问题

### 1. 是否需要重构（一句话结论 + 置信度，不绕弯）
给出"不重构的代价"与"重构引入的风险"两列对照。区分三种情况：必须重构（正在导致缺陷/不可维护）、应重构（成本收益明确）、不值得（当前形态是合理取舍）。

### 2. 三个视角各给出 Top 3 问题
每条必须有 文件:行号 证据与引文，并按格式：问题 → 证据 → 影响（谁会受伤）→ 建议方向。

- **测试开发自动化视角**：哪些逻辑没有可测试性（executor 的 10.9k 行里状态分支是否被单测真正覆盖）；http_client 的脚本化故障注入能力到什么程度；缺少什么会让回归防线失效；报告作为验收证据缺什么（错误路径、工具输出、采样覆盖）。
- **产品视角**：首次使用到拿到结论的路径有几处可能卡住；配置字段是否过度/不足；错误与原因码（如 `SINGLE_UDP_STREAM_FAILED`、`NOT_EVALUATED`）对小白用户和高级用户是否各得其位；resume/升级兼容策略是否合理；发布链路（tag→CI→Release→资料包）哪里最脆弱。
- **网络工程师视角**：接收端 RX 平均/P10 的统计口径是否有系统性偏差来源；有效窗口切分（iperf 行内区间优先、放弃 process-lifetime 回退）是否正确；UDP 双向并发、流数与 `-P`/`-b` 使用是否站得住；CTS 与 iperf3 参数映射（DatagramByteSize/14k 等）有无隐患；跨平台网卡计数器（GetIfTable2/netstat -ibn/sysfs）的采样一致性风险；双机时间不同步对结果的影响面。

### 3. 重构判断矩阵
对每个候选点给一行矩阵：**触发证据 / 影响 / 改动量 / 回归风险 / 结论（做 or 不做 or 只做局部）**。必须明确评估：

- executor.rs 的 10.9k 行：拆不拆、拆到哪一档（模块/子模块/独立 crate）、以什么边界拆、拆完如何验证行为不变；
- builder / executor / report / ui 的边界在现实中是否成立（谁偷偷越界了）；
- verdict + 原因码体系：是否可扩展、是否已经失控；
- 线程 + 手写状态机模型：可维护性的真实上限在哪；引入 async/新框架是否有收益，还是当前模型是被低估的合理选择（给出明确立场，不许骑墙）；
- util.rs 是否成了杂物袋（什么该收进哪里）。

### 4. 三阶段重构路线图
按 P0（止血/正确性类）/ P1（结构类）/ P2（体验类）给出：每阶段做什么、**怎么验证**（具体测试/CI/手工验收步骤）、明确**不做什么**。最后单列一项"最低成本、最高收益的第一刀"。

## 硬性要求

1. 中文输出；结论先行；分级（P0/P1/P2/P3）清晰。
2. 每条结论附证据（文件:行号 + 引文）；拿不准的明确写"未验证，需要……"。
3. 禁止泛泛建议——"提高可维护性"必须落到具体模块/函数/数据流。
4. 禁止主张删除用户可见功能；只评估"是否值得重构、怎么重构"。
5. **只输出分析报告，不写代码、不提交任何改动。**
6. 报告末尾给出：你审查时读过的文件清单 + 你认为最该补的 3 个测试。
