# PLAN v5.0 · 控制台前端重写 —— 执行方案

> 配套文档：`.ai/DESIGN-v5.0-webui.md` 记录**为什么**（为什么换框架、为什么全内联、
> 界面为什么这样重排）；本文件记录**做什么、怎么验**。两者冲突时以本文件为准，
> 并回头修 DESIGN。本文件取代 DESIGN 的 §4（构建链）、§6（测试策略）、§7（分期）。
>
> 仓库总规范见根目录 `AGENTS.md`。本文件不重复它，只补前端这一段。

---

## 0. 这份文档怎么用

按 §8 的分期顺序做。**每一期必须落在一个可运行状态上**——即 checkout 该期的
commit、`cargo build --release`、打开控制台，页面能用（功能少，但不是半截）。

每期结束跑 §8 里那一期的「验收」命令块，全绿才进下一期。分支 `feat/webui-vue`
在合并前不要求 main 那种全绿，但**每期的验收块是硬门槛**。

不确定的地方**不要自己发挥**：§10 列了唯一一个需要人拍板的取舍，其余都已定案。
定案之外出现新的取舍，停下来问，不要顺手选一个。

---

## 1. 对外部意见的逐条取舍

针对 GPT 给出的四条意见，逐条给结论。**采纳的部分已经写进下面各章，不再重复；
这一章只解释分歧。**

### 1.1 技术基线定案 —— 采纳

Vue 3 + TS + Vite + `vite-plugin-singlefile`，runtime-only，不用 Pinia、不用
vue-router、不用 Playwright，最终提交 `src/master/webui.html` 作为构建产物。
和 DESIGN §3 的结论一致，无分歧。

### 1.2 「产物字节比对不要做第一版 CI 硬门槛」 —— 采纳顾虑，不采纳方案

顾虑成立：跨环境字节一致依赖 Node/esbuild 的精确版本，第一版就拿它当门槛，
第一次 CI 红大概率是工具链差异而不是代码问题。

但**只剩三条结构断言是不够的**，因为它们防不住这个仓库最可能犯的错：

> 改了 `ui/src/**`，忘了 `npm run build`，提交上去。
> `src/master/webui.html` 还是旧的，测试全绿，页面行为对不上源码，几周都没人发现。

结构断言（无外链、不 eval、有 `#app`）在这种情况下**全部通过**——旧产物一样满足。

因此改成**溯源戳**（§6.3）：`emit.mjs` 把 `ui/` 源码的 MD5 写进产物注释，
一个普通的 Rust 测试重算并比对。这样：

- 门槛只要求「产物是从当前这份源码构建出来的」，**不要求两台机器构建出同样的字节**；
- 不需要在 CI 里装 Node 就能守住（`cargo test` 自己就守住了），和现有三条不变量
  同一种执行方式；
- 跨环境字节一致性作为**非阻塞**的信息性 job（§6.4），观察几周再谈要不要提级。

`md5` 已经是 `Cargo.toml` 里的依赖（`src/util.rs:767 md5_hex`），Node 侧
`crypto.createHash('md5')` 是内置的。**零新增依赖。**

### 1.3 「stores 不必机械拆成 store」 —— 采纳一半

**采纳的**：目录名 `stores/` 改成 `state/`。没有 Pinia 时它就是「模块级
`reactive` 单例 + 几个函数」，叫 store 会诱导出为了像 store 而写的样板。

**不采纳的**：不能因此就随手放。模块边界**按服务端资源切，不按屏幕切**，因为
决定「什么时候要重新拉」的是资源不是屏幕——连接一次，网卡表就该失效；跑完一轮，
报告路径就该更新；这些和用户当前在看哪一页无关。

按屏幕切会立刻撞上两个真问题：

- 进度轮询挂在「进度」视图的 `onMounted` 上，用户切去「监控」看一眼曲线，
  日志游标就断了。新的左栏导航让切换变得极其顺手，这个坑一定会踩到。
- 「快速工作台」和「计划复核」共享同一份 `UiPlan`，切成两个 store 就要同步两份。

定案：五个 state 模块，单向依赖，各带 `reset()`（§4.2）。

### 1.4 「Excel 工作项可以独立于 Vue 分支先做」 —— 采纳，但依赖关系要说准

结论对，理由要修正一下。GPT 把两类字段混为一谈了：

- `Row.src_side` / `dst_side` 是**报告内部**字段，前端一个字都不消费，
  和 Vue 分支零耦合，什么时候做都行。
- `TestSpec.link_group` 不一样：它是**计划编译器的输入**，得由前端在组装
  `UiPlan → RunRequest` 时填。所以它必须在 **P3（计划复核 + 执行）之前**
  在 Rust 侧存在并定好语义，否则 P3 写完还要回头改一遍。

见 §9。

### 1.5 「不要先做视觉重构」 —— 半采纳，理由要说清

同意**不设独立的视觉期**。不同意由此推出「先照搬旧布局，以后再美化」。

用户的原话是「后续执行都是 ui 上用快速工作台，所以前端也要重新设计更好看更符合
用户使用体验」。照搬旧布局的 Vue 版本**不满足这个要求**，而「以后再美化」这一
期在任何项目里都不会真的发生——它没有功能压力推着它走。

定案：**每一期交付的就是重新设计后的那一块**。布局和功能是同一份工作，不拆。
DESIGN §5 已经逐区写了新布局，照着做，不要先写一版旧布局的。

---

## 2. 技术基线（定案，不再讨论）

| 项 | 定案 | 备注 |
|---|---|---|
| 框架 | Vue 3 SFC，runtime-only | 模板构建期编译，产物里没有 `new Function` |
| 语言 | TypeScript strict | `noUnusedLocals` / `noUnusedParameters` 一起开 |
| 构建 | Vite 6 + `@vitejs/plugin-vue` + `vite-plugin-singlefile` | |
| 状态 | `reactive` / `computed` 模块单例 | 无 Pinia |
| 路由 | 无。左栏 = `state/ui.ts` 里的一个字段 | 无 vue-router |
| 单测 | Vitest，`environment: 'node'` | **不装 jsdom、不装 @vue/test-utils**，理由见 §7.2 |
| E2E | 无 | 无 Playwright |
| 产物 | `src/master/webui.html`，随源码一起提交 | `include_str!` 进二进制 |
| Rust 侧 | 执行/判定/报告语义**不动** | 例外见 §9 |

新增运行时依赖需要单独说明理由。图表**自己画 SVG**，不引图表库——监控曲线只有
两条折线，一个库的体积和 CSP 风险都不值。

---

## 3. 不可协商的运行时约束

这一章的每一条都有对应的机器检查（§6.2）。**不要试图放宽它们来换取方便。**

### 3.1 鉴权先于路由 ⇒ 产物必须完全自包含

`src/master/webui/http.rs:264` 起，口令校验在路由**之前**。任何路径没有
`X-CPE-Token` 一律 401，页面自身也不例外。

浏览器**不会**给 `<script src>` / `<link href>` 带自定义请求头。所以：

> 产物里出现任何外部子资源引用 = 那个资源 401 = 页面白屏。

这不是 CSP 的问题，改 CSP 解决不了。这就是必须全内联的全部理由。

### 3.2 生产 CSP（`src/master/webui/http.rs:132`，逐字）

```
default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline';
connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none';
frame-ancestors 'none'
```

由此推出的硬约束：

| 想做的事 | 能不能 | 为什么 |
|---|---|---|
| 内联 `<script type="module">` | ✅ | `'unsafe-inline'` 覆盖内联模块脚本 |
| 外链脚本 / 样式 | ❌ | `script-src` 没有 `'self'`；且见 §3.1 |
| 动态 `import()` | ❌ | 它是一次脚本 fetch，被 `script-src` 挡 |
| `eval` / `new Function` | ❌ | 没有 `'unsafe-eval'`。**永远不要加它** |
| `<style>` / `:style="{...}"` | ✅ | `style-src 'unsafe-inline'` |
| `@font-face`（含 data: URI） | ❌ | 没有 `font-src`，回落到 `default-src 'none'` |
| data: URI 图片 / 内联 `<svg>` | ✅ | `img-src 'self' data:` |
| Web Worker | ❌ | 无 `worker-src`，回落到 none |
| 同源 `fetch` | ✅ | `connect-src 'self'` |

字体只能用系统字体栈（现有皮肤已经是）。

### 3.3 跑测试时机器正在灌线速

`AGENTS.md` §0：界面**零持续动画**、零 `backdrop-filter`、轮询频率不许加码。

具体到实现：

- **禁止 `setInterval`。** 轮询一律用「响应落地后再排下一次」的 `setTimeout` 链。
  旧页面用的是 `setInterval(poll, 1000)`（`b3013e6:src/master/webui.html:3156`），
  机器一忙请求就会叠着发。这是重写要**修掉**的，不是要照搬的。
- 禁止 `@keyframes`。`transition` 用在 hover/展开这类一次性变化上可以。
- 轮询周期保持 1000ms，不要改小。

### 3.4 界面渲染的数据来自网络，一律当不可信

主机名、网卡名、错误串都来自辅测机。Vue 插值默认转义，所以规则很简单：
**禁止 `v-html`**。旧页面靠手写 `esc()` 兜，新架构不需要，也不许留后门。

### 3.5 禁止 `window.prompt` / `confirm` / `alert`

旧页面新建一条 UDP 配置要连点 5 个 prompt，中途取消会留半套数据
（这是 `the_recipe_card_can_be_deleted_and_edited_in_place` 记下来的真实缺口）。
新界面一律用就地编辑器 / 内嵌确认条。

---

## 4. 目录与分层

### 4.1 目录

```
ui/
  index.html                 # 只有 <div id="app"> + 一个 module script
  package.json / package-lock.json
  tsconfig.json / vite.config.ts / vitest.config.ts
  scripts/
    emit.mjs                 # 构建门禁 + 写 src/master/webui.html + 溯源戳
    lint-arch.mjs            # 分层与全局禁令的静态检查
  src/
    main.ts
    App.vue                  # 外壳：页头 + 左栏 + 主区
    api/
      client.ts              # 唯一的 fetch 出口
      dto.ts                 # 与 Rust model.rs 一一对应的类型
    state/                   # 模块级 reactive 单例
      ui.ts  session.ts  inventory.ts  plan.ts  run.ts  monitor.ts
    domain/                  # 纯函数。Vitest 的主要目标
      pairs.ts  grouping.ts  plan-build.ts  project.ts  progress.ts  format.ts
    components/              # 无状态展示件（props in / emits out）
    views/                   # 每个 region 一个
    styles/
      tokens.css  base.css
```

### 4.2 五个 state 模块

按服务端资源切分，**单向依赖**，箭头方向就是允许 import 的方向：

```
ui  ←（无依赖）
session  →  api
inventory →  api, session
plan      →  api, session, inventory, domain
run       →  api, plan
monitor   →  api, inventory
```

| 模块 | 持有 | 对应端点 |
|---|---|---|
| `ui.ts` | 当前 region、主题 | 无 |
| `session.ts` | 口令状态、agent host/port、`HealthOut`、连接错误 | `/api/bootstrap` `/api/connect` |
| `inventory.ts` | 本机 `HostInfo`、辅测机 `HostInfo`、`nic_policies`、派生的配对候选 | `/api/local` `/api/connect` |
| `plan.ts` | `UiPlan`（link_sets / recipes / suites / bindings）、执行区参数、`PlanOut`、`plan_hash` | `/api/plan` `/api/config` |
| `run.ts` | 运行中标志、日志游标 `from`、行缓冲、解析出的单元进度、报告路径 | `/api/run` `/api/stop` `/api/progress` `/api/open-report` |
| `monitor.ts` | 会话表 `session → {side, iface, points, from, running, error}` | `/api/monitor/*` |

每个模块导出：`state`（一个 `reactive`）、若干 `computed` 选择器、若干 action、
以及一个 **`reset()`**。`reset()` 不是可选的：模块级 `reactive` 是单例，Vitest
里没有 `reset()` 就会跨用例串味。

**轮询归 state 模块所有，不归视图。** `run.ts` 和 `monitor.ts` 各自持有自己的
`setTimeout` 句柄，视图挂载/卸载**不**启停它们。理由见 §1.3。

### 4.3 分层规则（`lint-arch.mjs` 静态检查）

| 目录 | 允许 import | 禁止 import |
|---|---|---|
| `domain/**` | `domain/**`、`api/dto`（仅类型） | `vue`、`api/client`、`state/**`、`components/**`、`views/**` |
| `api/**` | `api/**` | `state/**`、`components/**`、`views/**`、`vue` |
| `state/**` | `vue`、`api/**`、`domain/**`、按 §4.2 的箭头相互 import | `components/**`、`views/**` |
| `components/**` | `vue`、`domain/**`、`api/dto`（仅类型） | `api/client`、`state/**` |
| `views/**` | 全部 | — |
| `App.vue` | 全部 | — |

`domain/` 不许 import `vue` 是这套规则的重心：**历史上出过的 UI bug 全部是纯逻辑
bug**（角色键忽略 `pair.cross`、整列勾选、`-l` 被全局档位反向覆写），把它们赶进
一个没有响应式、没有 DOM、没有网络的目录，才能用最便宜的方式钉住。

---

## 5. 服务端契约

**Rust 侧这一版不改端点。** `src/master/webui/model.rs` 是唯一权威，
`ui/src/api/dto.ts` 手写对应类型，每个类型上方注明来源行号。
不要引入代码生成——13 个端点不值得一条构建链。

### 5.1 端点表

| 方法 | 路径 | 请求体 | 响应 `data` | Rust |
|---|---|---|---|---|
| GET | `/api/bootstrap` | — | `BootstrapOut` | `model.rs:344` |
| GET | `/api/local` | — | `LocalOut` | `model.rs:365` |
| POST | `/api/connect` | `ConnectReq` | `ConnectOut` | `api.rs:9` / `model.rs:336` |
| POST | `/api/plan` | `RunRequest` | `PlanOut` | `model.rs:388` |
| POST | `/api/config` | `RunRequest` | `Config`（JSON） | `api.rs:308` |
| POST | `/api/import` | `Config`（JSON） | 矩阵态回填 | `import.rs:77` |
| POST | `/api/run` | `RunRequest` | `{}` | `api.rs:318` |
| POST | `/api/stop` | `{}` | `{stopping:true}` | `api.rs:459` |
| POST | `/api/open-report` | `{}` | `{opened:true}` | `api.rs:467` |
| GET | `/api/progress?from=N` | — | `ProgressOut` | `model.rs:464` |
| POST | `/api/monitor/start` | `{side,iface,interval_ms}` | 会话 id | `monitor.rs:56` |
| POST | `/api/monitor/samples` | `{cursors:[{session,from}]}` | `{series:[MonitorSeriesOut]}` | `monitor.rs:143` |
| POST | `/api/monitor/stop` | `{session}` | `{stopped:bool}` | `monitor.rs:190` |

所有响应统一包在 `{ok, error, data}`（`protocol.rs:71`）里。

### 5.2 `api/client.ts` 必须逐条满足

1. **口令**：`?token=` → `sessionStorage` → `history.replaceState` 把 query 抹掉。
   照搬旧页面 `b3013e6:src/master/webui.html:726-731` 的行为，这是安全行为，不要"优化"。
2. 每个请求带 `X-CPE-Token`。
3. **POST 额外带 `X-CPE-Console: 1`**，否则被当跨站请求拒掉（`http.rs:253`）。
   GET 不需要。
4. `ok:false` 抛出 `new Error(error)`。
5. **HTTP 401 单独处理**：不要混进普通错误提示。走一个专门的「口令失效」终态，
   提示用带 `?token=` 的完整地址重新打开。旧页面把它混在通用 toast 里，
   看到的人只会以为是网络抖动。
6. 不做自动重试。这是内网工具，重试只会掩盖问题。

### 5.3 进度轮询

- 游标语义：请求 `?from=N`，响应里的 `from` 是**下一次**该用的值。
- **切换 region 不重置游标**，也不停轮询（§4.2）。
- `report` 字段由服务端从日志里捞（`api.rs:497`），非空即可点「打开报告」。
- 全量重载走 `?from=0`。

### 5.4 单元级进度靠解析两行日志

`ProgressOut.lines` 是原始日志。要做单元级进度 / ETA / PASS·FAIL 计数
（DESIGN §5.6），需要这两行——它们的格式在 executor 里是稳定的：

```
src/master/executor.rs:441   "\n[{i}/{total}] {title}"
src/master/executor.rs:683   "  ==> 单元结果: {LABEL}"
```

`LABEL ∈ {PASS, RATE_FAIL, MEASURED, NOT_EVALUATED, SETUP_ERROR, SKIP}`
（`src/verdict.rs:32`）。另有 resume 跳过行 `"  已PASS，上次时间: {t}，跳过 (RESUME)"`
（`executor.rs:505`）。

**双向钉住**，缺一不可：

- 前端：`domain/progress.ts` 出一个纯解析函数，Vitest 覆盖这三种行 + 乱序 + 半行。
- Rust：`src/master/executor.rs` 加测试
  `the_progress_lines_the_console_parses_keep_their_shape`，把这三个格式串钉死。
  这样改日志文案会红在 Rust 侧，而不是悄悄弄坏界面。

ETA 用 `PlanOut.units[].est_secs`（开跑前就有）配当前单元序号算，不要自己另算一套。

如果解析被证明脆（连续两次因为日志改动而红），退路是给 `ProgressOut` 加结构化
字段。**先不要做**——那要把一条状态通道穿过同时服务 CLI 的 executor，成本高得多。

---

## 6. 构建链与门禁

### 6.1 命令

```
npm ci            # ui/ 下
npm run dev       # 开发。注意：dev server 下没有真后端，见下
npm run test      # vitest run
npm run build     # vue-tsc --noEmit && lint-arch && vite build && emit
npm run verify    # lint-arch && emit --check   （不构建，供 CI / 提交前自查）
```

`npm run dev` 需要真后端：在 `vite.config.ts` 里配 `server.proxy`，把 `/api`
转到本机跑着的控制台（默认 `http://127.0.0.1:28802`，端口做成 env 可覆盖），
并让 proxy 注入 `X-CPE-Token`。**这只影响 dev，不影响产物。**

### 6.2 `lint-arch.mjs` 检查项

对 `ui/src/**` 做静态检查，命中任何一条即失败：

1. §4.3 的分层 import 规则。
2. `v-html`（§3.4）。
3. `eval(`、`new Function(`、动态 `import(`（§3.2）。
4. `window.prompt(` / `window.confirm(` / `alert(` / 裸 `prompt(` / `confirm(`（§3.5）。
5. `setInterval(`、`@keyframes`、`backdrop-filter`（§3.3）。
6. `@font-face`、`url(http`、`//fonts.`（§3.2）。
7. 文件名必须全 ASCII（溯源戳要求跨语言排序一致，§6.3）。

### 6.3 溯源戳

`emit.mjs` 在写产物前算一个戳，插进产物尾部：

```html
<!-- cpe-ui-stamp: <32 位小写 md5> -->
```

**算法（两端必须逐字一致）：**

1. 收集文件：`ui/index.html`、`ui/package.json`、`ui/package-lock.json`、
   `ui/tsconfig.json`、`ui/vite.config.ts`，以及 `ui/src/` 下的全部文件（递归）。
2. 路径取相对 `ui/` 的 POSIX 形式（分隔符 `/`），按 **UTF-8 字节升序**排序。
   （全 ASCII，由 §6.2 第 7 条保证，所以 Node 的默认 sort 和 Rust 的 `String` 序一致。）
3. 每个文件的内容做 **CRLF → LF** 归一。（Windows 检出必然带 CRLF。）
4. 拼接 `path + "\n" + content + "\n"`，整体取 MD5，小写十六进制。

Rust 侧对应测试（新增，放 `src/master/webui/tests.rs`）：

```
the_embedded_page_was_built_from_the_current_ui_sources
```

用 `env!("CARGO_MANIFEST_DIR")` 定位 `ui/`，重算并与 `include_str!("../webui.html")`
里的戳比对。失败信息必须直说「`ui/` 改了但没重新构建，去 `ui/` 跑 `npm run build`」。

MD5 用现成的 `crate::util::md5_hex`，Node 用内置 `crypto`。**零新增依赖。**

> 戳只回答一个问题：产物是不是从当前这份源码来的。它不要求两台机器构建出相同
> 字节，所以它不受 esbuild 版本影响。

### 6.4 CI

**阻塞门槛（`cargo test` 就够，不需要 Node）：**

| 测试 | 守什么 |
|---|---|
| `the_embedded_page_has_no_external_subresources` | §3.1。这是鉴权/CSP 那条不变量唯一的机器保证 |
| `the_embedded_page_never_evals` | §3.2 |
| `the_embedded_page_mounts_into_the_expected_root` | 产物里有 `id="app"` |
| `the_embedded_page_was_built_from_the_current_ui_sources` | §6.3 |
| `the_progress_lines_the_console_parses_keep_their_shape` | §5.4 |

**新增 job `ui`（阻塞）**：pinned Node 22 → `npm ci` → `npm run test` →
`npm run build` → `npm run verify`。守的是「源码本身能过类型检查、单测和分层规则」。

**新增 job `ui-repro`（`continue-on-error: true`，不阻塞）**：另一台
runner 重建，比对 `src/master/webui.html` 字节差异，只报告不拦。跑满一个发布周期
后再决定要不要提级成阻塞。这是 §1.2 里对 GPT 那条意见的落地方式。

---

## 7. 测试策略

### 7.1 四层，各管各的

| 层 | 工具 | 管什么 |
|---|---|---|
| 纯逻辑 | Vitest（`environment: 'node'`） | `domain/**` 全部导出函数 |
| 契约 | Vitest | `dto.ts` 对着真实响应样本反序列化；项目文件校验器吃畸形输入 |
| 产物 | Rust `#[test]` | §6.4 那五条 |
| 端到端 | 手动 | 双机真跑一轮。没有 Playwright |

### 7.2 为什么不装 jsdom / @vue/test-utils

回顾 `src/master/webui/tests.rs` 里那四个页面测试记录的真实缺口，**没有一个是
渲染问题**：全是纯逻辑——分组函数忽略了 `pair.cross`、整列开关缺分支、`-l` 被
全局档位反向覆写、编辑器缺删除分支。新架构把这些全放进 `domain/`，用普通
Vitest 就能覆盖到，而且比挂载组件断言 DOM 结实得多。

组件测试等到出现**第一个真的只在渲染层出现的 bug** 再加。在那之前，jsdom +
@vue/test-utils 只是让 `npm ci` 更慢、依赖面更大。

### 7.3 四项遗留义务

`src/master/webui/tests.rs` 里这四个测试靠 grep 手写 HTML 源码工作，产物变成
Vue bundle 后必然失效。**P0 删掉它们**（`#[ignore]` 会烂在那里），
义务转移如下。每一项的**原始理由**（它们各自记录的真实缺口）必须逐字搬进
新测试的注释里——那才是这些测试真正的价值。

| 原测试 | 转移到 | 落在哪一期 |
|---|---|---|
| `the_recipe_card_can_be_deleted_and_edited_in_place` | Vitest `domain/plan-build.test.ts`：删除一条 recipe 会清掉所有 task 上的 `recipe_ids` 引用 + §6.2 第 4 条全局禁 prompt | P2 |
| `the_link_set_panel_lists_every_combination_and_stays_editable_in_place` | Vitest `domain/pairs.test.ts`：候选默认含同机组合；`domain/grouping.test.ts`：自动分组与筛选**调用同一个导出的谓词** `matchesLinkFilter()`（这条从"靠测试发现不一致"升级成"结构上不可能不一致"） | P2 |
| `the_assignment_table_can_toggle_a_whole_suite_column` | Vitest `domain/plan-build.test.ts::toggleSuiteColumn` | P2 |
| `the_udp_datagram_size_is_configured_only_in_the_suite` | Vitest `domain/plan-build.test.ts`：由 `UiPlan` 组装出的 `RunRequest` 里，全局 `udp_lengths` 不会被套件里的 `-l` 反向写回 | P3 |

P7 的验收包含「这四项各有一个命名的、通过的替代测试」。

---

## 8. 分期

### P0 · 脚手架（进行中）

**范围**：`ui/` 骨架、Vite/TS 配置、CSS 变量搬迁、`emit.mjs`、`lint-arch.mjs`、
溯源戳两端、删掉四个失效测试、加上 §6.4 的五条 Rust 测试、CI 两个 job。

**已完成**：`ui/` 目录、`vite.config.ts`、`tsconfig.json`、
`styles/tokens.css`（逐字搬自旧页面 6–52 行）、`styles/base.css`、`App.vue` 外壳、
`emit.mjs` 四条不变量、产物已能写进 `src/master/webui.html`（70243 字节，检查全过）。

**还差**：
- `stores/` 改名 `state/`；`types/ui.ts` 里的 `REGIONS` 并进 `state/ui.ts`。
- `lint-arch.mjs` + `vitest.config.ts`。
- 溯源戳（`emit.mjs` 写入 + Rust 测试重算）。
- 删四个失效测试，加五条新 Rust 测试。
- CI 两个 job。
- **真浏览器验一次**：起 release 控制台，带 `?token=` 打开，确认内联
  `<script type="module">` 在 `script-src 'unsafe-inline'` 下被接受、Vue 挂载成功、
  控制台零 CSP 告警。

**验收**：
```bash
cd ui && npm ci && npm run test && npm run build && npm run verify && cd ..
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo test --locked
```

### P1 · 外壳 + 会话 + 本机/辅测机

**范围**：`api/client.ts`、`api/dto.ts`、`state/session.ts`、`state/inventory.ts`；
左栏导航按 DESIGN §5.2 落地（去掉假的 1–5 步骤编号，改成带实时徽标的区域栏）；
「本机」「辅测机」两个视图：连接、口令、IPv4 前缀过滤、两侧网卡表、
`HealthOut` 的工具链状态。项目导入导出按钮位置挪到页头（DESIGN §5.2）。

**可运行状态**：能连上辅测机，能看到两侧网卡表。

**验收**：同 P0 的命令块，外加手工：连接成功后网卡表非空；填错 IP 时错误提示
指向具体原因（`api.rs:220` 那句已经写好了，照原样显示）；改前缀清空 =
列出全部网卡（`ConnectReq.ipv4_prefixes` 是 `Option`，区分"没提交"和"提交了空"，
前端必须提交空数组而不是省略字段）。

### P2 · 快速工作台

**范围**：`domain/pairs.ts`、`domain/grouping.ts`、`domain/plan-build.ts`、
`state/plan.ts`；三栏 master-detail + 常驻复核栏（DESIGN §5.3）；链路集合、
流量套件、参数配置就地编辑器、分配表（含整列开关）；筛选改三态单选
（全部 / 跨机 / 同机）；自动分组**先预览再应用**。

**可运行状态**：能拼出完整 `UiPlan` 并在复核栏看到结构（还不调 `/api/plan`）。

**验收**：P0 命令块 + §7.3 表里 P2 那三项替代测试存在且通过。

### P3 · 计划复核 + 执行

**范围**：`/api/plan` 预览树（用 `PlanOut.sections` + `trace`，不要自己在前端
重排单元）；每个单元展示 `load`（最终下发参数）；`notices` 展示；执行区参数
（duration / TCP -w -P / UDP -b -l -w / ping / resume / 按链路上限裁剪 / 截图）；
`/api/run` 并携带 `plan_hash`；`/api/config` 导出 config.json。

**依赖**：§9.1 的 `TestSpec.link_group` 必须先在 Rust 侧落地。

**可运行状态**：能真跑一轮完整测试。

**验收**：P0 命令块 + §7.3 表里 P3 那一项 + 手工：双机真跑一轮小规模计划，
`plan_hash` 闸门在改动计划后确实拦下来。

### P4 · 进度

**范围**：`domain/progress.ts` 解析器 + Rust 侧格式钉子（§5.4）；`state/run.ts`
的自调度轮询；单元级进度条 + ETA + PASS/FAIL 计数 + 失败清单；原始日志作为
可折叠的次要区；停止；打开报告。

**验收**：P0 命令块 + `the_progress_lines_the_console_parses_keep_their_shape`
通过 + 手工：跑测试期间在各 region 之间来回切，日志不断、不重、不丢。

### P5 · 监控

**范围**：`state/monitor.ts`；多路会话；**一次批量轮询问全部会话**
（`/api/monitor/samples` 就是为这个设计的，见 `monitor.rs:31` 的注释——
每路各发一次会把浏览器同源连接数占满，把日志那一路拖顿）；自绘 SVG 双折线；
单路结束不影响其余各路（`running:false` + `error` 就地摘掉那一条）。

**验收**：P0 命令块 + 手工：同时开 4 路以上监控 + 一轮测试在跑，日志轮询不卡顿。

### P6 · 项目导入导出

**范围**：`domain/project.ts`——项目文件（`project_version: 1`，含 `ui_plan`、
`settings`、`nic_policies`、`topology_fingerprint`）的**严格校验器**，
返回 `Result<Project, string[]>`。

这一段**必须重视**：项目文件是用户从磁盘选的、完全不可信的 JSON，旧页面为它写了
约 150 行校验（`b3013e6:src/master/webui.html:1670-1735`），每一条都是踩出来的。
把那些检查逐条搬过来，配畸形输入的 Vitest 用例。导出走 Blob + `<a download>`，
和旧页面一致——**这条路今天是通的（CSP 不拦），不要"改进"它。**

**验收**：P0 命令块 + 畸形项目文件用例全绿 + 手工：导出→重新打开控制台→导入→
预览出同一批单元。

### P7 · 收尾

**范围**：文档同步（`使用说明.md`、`README.md`、`dist/README-Windows-快速开始.md`
里凡是描述界面的段落）；`dist/build_config_docs_bundle.py` 重出包；
§7.3 四项义务逐条核对；`.ai/DESIGN-v5.0-webui.md` 与本文件对齐实际做法；
版本号 → 5.0.0。

**验收**：完整的 `AGENTS.md` §1 四件事 + `npm run test/build/verify` +
双机真跑一轮完整预设（`dist/projects/cpe-ui-project-full.json`）。

---

## 9. 相邻的 Rust 侧工作项

**这两项可以和 Vue 分支并行，在 main 上单独提交。** 但 §9.1 有时序约束。

### 9.1 `TestSpec.link_group`（P3 之前必须落地）

Excel 汇总报告要按链路分组。分组键的优先级（DESIGN §8.2 已定）：

1. 界面上的链路集合名
2. 物理网口对
3. 角色对

**永远不要用主机名**——Arch 那台自报 `UNKNOWN-PC`，拿它当键会把不同机器混成一组。

`link_group` 是**计划编译器的输入**，由前端在 `UiPlan → RunRequest` 时填，
所以字段和语义必须先定下来，否则 P3 写完要返工。

`Row.src_side` / `dst_side` 是报告内部字段，前端零消费，什么时候做都行。

### 9.2 Excel 汇总报告

`summary.xlsx`，`rust_xlsxwriter`，实测二进制 +0.95 MB。与前端无耦合。

### 9.3 P10 / 中位数 / P95 的展示口径（已确认）

P10 **继续计算，只是不展示**。它是承重的：

- `src/master/rate_window.rs:442-443` 用 `rx_stats.p10_mbps.is_some() &&
  tx_stats.p10_mbps.is_some()` 当「5 秒滚动窗口已完整」的信号；
- `src/master/verdict_assembly.rs:499` 在 `offered_shortfall_explains_rx` 里用
  `tx_stats.p10_mbps`。

删字段会静默改变判定。中位数和 P95 从展示里去掉。

---

## 10. 唯一需要拍板的取舍：高级矩阵和 config.json 导入

用户说过「后续执行都是 ui 上用快速工作台」。据此我的**建议**是：

- **v5.0 不重建高级矩阵界面**，不重建 `/api/import`（config.json 导入）的界面入口。
- **保留 `/api/config`（导出 config.json）**——一个按钮一次 fetch，
  「界面里拼计划 → 导出给命令行/批处理跑」是真需求，成本近乎零。
- **Rust 侧的 `pairs` / `nic_policies` / `udp_groups` / `tcp_groups` DTO 和
  `/api/import` 端点一律不动**，命令行路径和老 config 完全不受影响。

理由：矩阵界面是旧页面里最大的一块，而控制台的预设分发路径已经是
`dist/projects/*.json`（项目文件），命令行走 `dist/configs/*.json`（config 文件），
两条路都不经过矩阵界面。

**代价说清楚**：`/api/import` 把 config.json 翻回的是**矩阵形态**的界面状态
（`import.rs:77` 起），没有矩阵界面就没有地方显示它。也就是说，
「手里有一份老 config.json，想在控制台里改改再跑」这条路会断，
只能改 JSON 后走命令行。

如果这条路要留，就是 +1 期（P6b，重建矩阵 + 导入回填），放在 P6 之后。

**在拿到答复之前按"不重建"推进**，因为它不产生返工——矩阵是独立的一块，
后加不影响前面任何一期。

---

## 11. 合并前我会逐条对的清单

1. `cargo fmt --check` / `cargo test --locked` / 两个 target 的 clippy 全绿。
2. `ui/`：`npm run test` `npm run build` `npm run verify` 全绿，
   且 `git status` 在 `npm run build` 之后是干净的（产物已提交）。
3. §6.4 的五条 Rust 测试存在且通过。
4. §6.2 的七条静态检查真的会拦——**每一条我会故意写一行违规代码验证它变红**。
5. §7.3 四项义务各有一个命名的替代测试，且原始理由注释被搬过来了。
6. `domain/**` 里没有 `vue` 的 import。
7. 轮询：全局搜不到 `setInterval`；`run.ts` / `monitor.ts` 的定时器不随视图挂载启停。
8. 真机：Windows 双机跑一轮 `dist/projects/cpe-ui-project-full.json`，
   期间在各 region 间反复切换，日志不断、不重、不丢。
9. 浏览器控制台零 CSP 告警、零报错。
10. 文档同步义务（`AGENTS.md` §5）已履行，`dist/` 的包已重出且 SHA-256 对得上。
