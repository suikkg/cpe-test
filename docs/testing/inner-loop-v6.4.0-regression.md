# v6.4.0 发版前回归验收

基线：已发布 v6.3.2。本轮把 ADB 内环测速 v2 与组合场景纳入发布面，验收目标是
**「实际使用一开始就能跑起来」**，不是「四条门禁绿了」。

## 为什么不能只看门禁

本轮开场时四条门禁在开发机上全绿（fmt / 771 项测试 / 双目标严格 Clippy），
而仓库处于**新克隆连测试都编译不出来**的状态：`src/inner/tests.rs` 在编译期
`include_str!` 读 `inner.local.example.json`，同一轮里另一个决定又用
`.gitignore` 的 `*.local.example.json` 把这类本机配置挡在了仓库外
（它们可能带 `agent_token`）。两个决定各自都对，凑一起就是 CI 和任何新贡献者
都编不过。`git archive HEAD` 里该文件 0 个匹配。

绿灯对这一类**完全没有分辨力**——文件躺在开发机上，本地怎么跑都是绿的。

处置：去掉编译期依赖（该测试的覆盖由 `V1_PROJECT` 这份全本机、无 agents 的
JSON 字面量和多处 `agents.clear()` 承担，无独占损失），删掉 README 里指向该
文件的那句（公开 README 不该指着仓库里没有的文件），并加第四条结构守卫
`no_compile_time_include_depends_on_a_gitignored_local_file`。守卫做过注入
验证：把那行加回去立刻红，去掉即绿。

## 干净树验收

不是在开发机的工作树上跑，而是按 `git ls-files` 重建一份只含**版本控制里
实际有的文件**的树（232 个文件），从零全量编译：

- `cargo test --locked` → **772 passed / 0 failed**
- `cargo build --release --locked` → 6.79 MB 二进制

## 真机端到端（自研 CPE）

用干净树产出的 release 二进制，对真实设备跑通整轮内环：

- 板侧：OpenWrt aarch64，`br0` = 192.168.8.1，iperf3 3.10.1
- 主控：macOS，en0 SGMII1G 192.168.8.100（与板侧同网段）
- 计划：TCP + UDP × 上行 / 下行 / 双向并发 = **6 个单元，全部 MEASURED，无失败**，71 秒完成
- 产物齐全：`report.html`（56 KB 自包含、零外链）、`result.json`、`summary.json`
  （`finished: true`、`units: 6`）、`config.json`、`units.jsonl`（6 行）、
  双向单元各带两条腿的板侧 server 日志

**核实过一个可疑现象**：6 秒时长下网卡口径系统性低于工具口径（上行 867.91 vs
901.00，差 3.7%）。查 `rx` 明细为 `median 918.77 / min 640.54`——TCP 慢启动
那一秒把 6 秒窗口的均值拖了下去。改回文档默认的 20 秒复测：上行 881.31 vs
889.00（差 0.9%），下行网卡 945.47 **高于**工具 920.00（网卡计以太帧头，本该
略高）。系统性偏差不存在，测量层判定口径正常。

## 运行期冒烟

用同一个 release 二进制起真进程打真 HTTP：

- CLI：`--help` / `scan` / 未知模式退出码、`inner` 缺 `--config` 的用法提示
- 控制台鉴权：`GET /` 无凭据 401、带 token 200；页面有挂载点、带溯源戳、
  **零外链子资源**（铁律 3 的机器保证）
- 会话 cookie 的「只」字：只带 cookie 刷新 `GET /` → 200，只带 cookie 打
  `/api/bootstrap` → 401
- CSRF：POST 缺 `X-CPE-Console` 被拒
- 只读端点 8 条（含新增的 `/api/inner/runs`、`/api/inner/status`、
  `/api/scenario/status`、`/api/scenario/runs`）全部 200 且 `ok=true`
- `/api/inner/plan` 预览返回 4 单元 / 4 腿 / 2 链路，带 `verdict_basis` 与接收端说明
- agent：`Authorization: Bearer` 认证的三态（无 / 错 / 对）、`/info` 结构、
  非法 JSON 单次包装、状态页 200
- 控制台与 agent 日志中 **panic 计数 0**

冒烟脚本首轮报出的 8 条"失败"经逐条核实**全部是脚本自身前提错误**（agent 用
`Authorization: Bearer` 而非 `X-CPE-Token`；只读端点是 GET 不是 POST；未知接口
按 AGENTS §4.1 走 `200 + ok=false` 信封；这台机器上真接着 ADB 设备所以
`--probe` 退出 0），不是产品缺陷。

## 仍然缺席

真实 Windows 双机、ctsTraffic 全链路、Windows 默认预设与 24 小时长稳未在本轮
执行。Windows 原生测试与三平台打包由发布 CI 承担。
