# AGENTS.md — cpe_test 协作规范

> 给在这个仓库里改代码的人和 AI 代理看。**架构地图在 `.ai/PROJECT_ARCHITECTURE.md`，本文不重复它**；
> 本文只讲「怎么改、改完怎么证明没坏、哪些线不能碰」。
> 冲突时以源码为准，然后回来更新这两份文档。

---

## 0. 这是什么

一个 CPE 子网灌包测试工具：双机（主控 + 辅测机）自动跑 ping / iperf3 / Windows ctsTraffic，
出 HTML 报告。**单个 exe，运行期零 Python、零 PowerShell、零第三方运行时**。

由此推出四条不能商量的产品约束：

| 约束 | 后果 |
|---|---|
| 单文件分发 | 控制台页面必须 `include_str!` 进二进制，不能有外部 JS/CSS/字体/图片 |
| 运行期离线 | 报告必须自包含；页面不许连 CDN；CSP 里没有任何外部源 |
| 主战场是 Windows | 任何改动都要过 `x86_64-pc-windows-msvc` 的 clippy 与测试 |
| 跑测试时机器正在灌线速 | 界面**零持续动画**、零 `backdrop-filter`；轮询频率不许加码 |

---

## 1. 改完必须跑的四件事

按顺序，全绿才算改完。CI 跑的就是这几条（`.github/workflows/build.yml`）。

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc -- -D warnings
```

动了 `dist/` 下的配置或文档，再加一条（CI 会逐条目字节比对）：

```bash
python3 dist/build_config_docs_bundle.py <版本号>   # 例：4.6.0
shasum -a 256 dist/cpe_test-v<版本号>-windows-config-docs.zip   # 必须等于同名 .sha256 里的值
```

动了前端，见 §4。

**不要用「跑了一部分」代替全跑。** 这个仓库里 `master/builder.rs`、`master/executor.rs`、
`report.rs` 三者的字段是耦合的，只跑其中一个模块的测试会漏掉另外两个。

---

## 2. 三条铁律

这三条历史上都被破坏过，代价分别是两次静默错判和一次协议不兼容。每条都有 CI 里的结构断言看着。

1. **判定优先级只有一份实现** —— `verdict::aggregate_verdict`。
   `master::executor::aggregate_unit_verdict` 和 `report::group_verdict` 都必须调它，
   不许各写一份。由 `verdict_priority_has_exactly_one_definition_in_the_tree` 强制。

2. **速率判定口径只有一份实现** —— `master::rate_window`。
   这一层只吃网卡计数器样本和目标速率，不碰进程、端口、HTTP、线程。
   所以「采样不可信必须判 NOT_EVALUATED 而不是 RATE_FAIL」这条能被单独审。
   UDP / TCP / CTS 三条路径都调它。

3. **鉴权先于路由** —— `master/webui/http.rs::handle()` 里，token 校验在**任何**分支之前，
   页面自己也不例外。页面里带着给 API 用的口令，放行未认证的 `GET /`
   等于把口令发给任何来问的人。
   **推论：控制台不能有需要浏览器自动加载的子资源**（`<script src>`、`<link href>`、
   `<img src>` 指向本站路径都会 401，因为浏览器不会给它们带自定义头）。
   这条是 §4 「前端必须打成单文件」的根因，不是风格偏好。

其余不变量（端口分配、稳定 ID 模板、RESUME 命中窗口、同 /24 门禁、bidir 两腿……）
见 `.ai/PROJECT_ARCHITECTURE.md` §11.1。**改之前先读那一节**。

---

## 3. 改哪里，联检哪里

完整表在 `.ai/PROJECT_ARCHITECTURE.md` §11.2。最常踩的四条重抄在这里：

| 需求 | 首先改 | 一定会漏掉的联检点 |
|---|---|---|
| 配置字段 / 默认值 | `config.rs` | `config.example.json`、`config.minimal.json`、`dist/configs/*.json`（4+1 份）、README、`使用说明.md`，以及钉住默认值的那几个测试 |
| HTTP DTO / 端点 | `protocol.rs` 或 `agent/server.rs` | `http_client.rs`、`master/executor.rs`、agent 侧解析与错误包装测试 |
| 任务数量 / 顺序 / ID / 端口 | `master/builder.rs` | executor 的 `sort_key` 构造、`report.rs` 的排序与组合计、**历史 RESUME 会不再命中** |
| 报告列 / HTML | `report.rs` | `executor.rs` 里**全部** `Row` 构造点（漏一个就是空列） |

补两条本仓库特有的：

- **对外 JSON 字段即使当前没有本地消费者，也是兼容面。** 删名字/改名字要同步所有端点。
- **`Leg.tag` 对单向单元是空串**（`dir_pairs()` 对 `ab`/`ba` 就给空，执行侧靠这个空串表示
  「单向」）。要展示方向请用 `Unit.direction`（只读展示字段），**不要去填 `Leg.tag`**。

---

## 4. 前端构建链

> 状态：`feat/webui-vue` 分支上正在把 `src/master/webui.html`（手写 3519 行）
> 换成 Vue 3 + Vite 构建产物。方案见 `.ai/DESIGN-v5.0-webui.md`。
> **在该分支合入 main 之前，本节描述的是目标形态；main 上仍是手写单文件。**

### 目标形态

```
ui/                      # Vite 项目，只有开发期需要 Node
  src/**/*.vue|ts        # 手写源码 —— 审阅和测试都盯这里
  package-lock.json      # 锁死，CI 用 npm ci
npm run build            # → 全内联的单个 HTML
src/master/webui.html    # 构建产物，**提交进仓库**
```

三条规矩：

1. **`cargo build` 永远不跑 Node。** 产物是提交进仓库的，克隆下来就能编。
   贡献者不装 Node 也能改 Rust。
2. **改前端 = 改 `ui/src/` 然后重新构建并把产物一起提交。**
   直接手改 `src/master/webui.html` 会在下次构建时被覆盖，且 CI 会因为
   产物与源码对不上而失败。
3. **产物必须全内联。** 不是审美偏好，是 §2 铁律 3 的推论：任何
   `<script src>` / `<link href>` 都会被鉴权挡成 401。CI 有一条测试直接
   在产物里搜外部引用，搜到就红。

### CSP

控制台的 CSP 是：

```
default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline';
connect-src 'self'; img-src 'self' data:; base-uri 'none';
form-action 'none'; frame-ancestors 'none'
```

- **不许加 `'unsafe-eval'`。** 只有 Vue 的「完整版（含运行期模板编译器）」需要它；
  走 Vite 预编译后不需要。加了就等于把模板字符串变成可执行面。
- **加 `'self'` 到 `script-src` 没有用**，理由见 §2 铁律 3 的推论。

---

## 5. 文档同步义务

改了下面左边的东西，右边的文件必须在**同一个提交里**跟着改：

| 改了 | 同步 |
|---|---|
| CLI 参数 / 模式 | `README.md`、`main.rs` 的帮助文本及其解析测试 |
| 配置字段 | §3 表格第一行列的那一整排 |
| 界面流程 / 术语 | `使用说明.md`、`dist/README-Windows-快速开始.md` |
| 架构（新模块、依赖方向、不变量） | `.ai/PROJECT_ARCHITECTURE.md` |
| 前端构建链 | 本文 §4 |

**术语一致性**：界面上叫「配置」的东西，文档里不许叫「配方」；界面叫「链路集合」，
文档不许叫「链路组」。历史上这两组词各自漂过一次，用户按文档找不到按钮。

---

## 6. 提交与发布

- 提交信息用中文，首行 `<type>: <做了什么>`，正文按模块分段说**为什么**，不是罗列 diff。
- 版本号只在 `release:` 提交里动，同时改 `Cargo.toml` 和 `dist/` 里带版本号的文件名。
- CI 会校验 tag 与 `Cargo.toml` 版本一致、且 tag 指向 main。
- **不要在没被要求时提交或推送。**

---

## 7. 给 AI 代理的补充

1. 先从 `src/main.rs` 确认 CLI 模式，再沿调用链进 `master/ui` 或 `agent/server`。
2. `.ai/PROJECT_ARCHITECTURE.md` 只写模块路径和符号名，**不写行号**——
   上一版把行号写进正文，到 v4.2.6 时全部失效，偏差 2.3×～21×。
   「看似精确的错误指引」比没有指引更危险。定位请用 `grep -n "fn <符号名>" <文件>`。
   本文同理：本文出现的所有行数（如「3519 行」）是规模量级，不是定位坐标。
3. Serde 当前没有 `deny_unknown_fields`。配置里没声明的键会被静默忽略，
   **不能根据示例配置或 README 推断某功能已经实现**——去代码里找消费者。
4. 判断「这个改动要不要加测试」的标准：如果它是一条能被下一次重构悄悄破坏的约束，
   就加一条结构断言。本仓库已有的先例：判定优先级唯一性、请求体上限余量、
   示例配置与代码默认值一致、报告序号与 `group_seq` 同源。
