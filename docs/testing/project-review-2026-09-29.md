# 全项目审查与界面文案检查（2026-09-29）

## 范围与基线

- 基线为本地 `6cd268a`（2026-09-23，v6.5.0）及当前工作区。保留开始检查前已有的未提交修改；本记录不把全部工作区差异都归为本次新增。
- 检索本地全部引用，共 136 条历史提交。2026-09-29 没有本地新提交；检查包括当天工作区修改。未获取远端，结论仅针对本地历史。
- 建立全项目文件与模块索引，检查 CLI 入口、生产模块调用关系、全部 Vue 模板及面向用户的提示字符串，重点阅读扫描、选择、执行、报告和鉴权路径及相关历史差异。并非逐行重新审查所有历史补丁，也不表示项目不存在其他缺陷。

## 模块覆盖

| 范围 | 检查内容与证据 |
| --- | --- |
| 入口与配置 | `src/main.rs` 模式分派、`config` 默认值与消费者、示例配置及 Windows 预设约束；未调整灌包参数 |
| 网卡扫描 | `nic/scan_windows`、`scan_linux`、`scan_macos` 的有效地址与前缀筛选；主控、辅测机 `/info` 的扫描语义 |
| 内环选择 | `domain/inner-setup`、`state/inner`、`InnerLinkTable`：utun/虚拟口、10/172 网段、重扫排序、隐藏项、连接身份变更、无辅测机 |
| 内环执行与报告 | `inner` 的计划、预检、地址绑定、远程请求和报告：同网段多网卡、IPv4/IPv6、链路本地作用域、电脑及网口身份 |
| 子网执行与判定 | `master/builder`、`master/executor`、`rate_window`、`verdict`、`executor/row`：计划与结果身份、RX 判定单一实现、诊断与结论分工 |
| 协议与生命周期 | `protocol`、`http_client`、`agent/server`、进程与监控资源管理：能力协商、旧字段语义、owner 清理、停止与失败处理 |
| 控制台与鉴权 | `master/webui/http`、`api`、前端会话与运行状态：鉴权先于路由、页面 cookie 与 API token 分离、扫描事务提交、失败保留快照 |
| 结果与历史 | `report`、运行存储、恢复配置、报告比较与结果页面：类型化身份、历史信息来源与用户提示 |
| 构建与交付 | CI、Vite 单文件产物、源码戳、前端分层/CSP、Windows Clippy、配置文档包 |

## 用户可见文案

未发现完整系统 prompt 或对话记录被直接显示在 UI。发现部分文字把开发解释、实现细节和防回归理由写成了产品提示，相关来源包括：

- `6cf2b3e`（9 月 6 日）：运行状态尚未同步时的长篇实现解释。
- `9d8dba6`（9 月 10 日）：内环中的“参数一个字节都不会丢”“把别人的源 IP 写到自己头上”等表述。
- `6cd268a`（9 月 23 日）：悬浮提示中的 `request.json`、空闲快照与请求受理的实现解释。

清理了内环、监控、计划编辑、开始测试、运行进度和历史详情中的此类正文与悬浮提示。保留输入规则、操作后果、测量来源和必要的异常操作步骤。实现理由留在代码注释和开发文档，未新增面向用户的审查说明。

`AGENTS.md` 新增用户可见文案约束，并把已过时的“Vue 尚未合入”说明改为当前构建链；架构文档移除空白统计占位和过时的行号指引。

## 已复现并修复的问题

### 1. 换辅测机后继承上台电脑的待添加选择

复现：扫描并勾选某辅测机的网卡，修改辅测机地址，再扫描另一台具有相同逻辑 ID、网卡名和 IP 的电脑。原有选择会在新电脑上重新出现。

修复：待添加选择与探测连接身份同步失效。修改地址、端口、令牌或 ADB 序列号立即清空待添加选择；同一连接的普通重扫继续按网卡身份恢复。已添加配置仍保留，换机后需要核对。

Chromium 用例在修复前复现地址切换仍被勾选，修复后覆盖四种身份修改；另一个用例覆盖无辅测机、本机重扫顺序变化、隐藏接口和双栈选择。

### 2. 全量扫描意图被辅测机默认前缀替换

复现：辅测机默认前缀为 `192.168.`，实际还有 10/172 网段或 IPv6-only 接口。内环全扫和子网控制台留空前缀都发送空 `ipv4_prefixes`；旧 `/info` 语义将空列表解释为使用辅测机默认前缀，导致主控和辅测机清单不一致。

修复：增加显式 `all_interfaces` 与 `unfiltered_info_v1` 能力。主控内环全扫及子网控制台留空前缀显式请求全接口；agent 集中解析前缀。缺少能力的旧 agent 会明确提示更新；子网页仍可填写显式前缀兼容旧 agent。旧客户端缺省或空列表继续保留原来的默认前缀语义。

子网页 HTTP 回归在修复前因 `all_interfaces` 为 false 失败，修复后通过；同时验证不支持全扫时保留原连接配置和双端快照、显式前缀兼容旧 agent。

## 针对性回归

- `protocol::tests::info_scan_distinguishes_explicit_all_from_legacy_default_and_prefixes`：旧缺省、显式前缀及全量扫描语义。
- `inner::remote::tests::inner_remote_scan_is_unfiltered_and_rejects_agents_that_would_ignore_the_request`：远程能力协商、10/172 前缀和 IPv6-only 响应。
- `master::webui::tests::console_scan_prefixes_match_the_ui_and_unsupported_full_scan_preserves_state`：真实 HTTP 请求和连接事务。
- `inner::tests::inner_multinic_binding_keeps_host_interface_addresses_and_report_identity_together`：两台电脑、同网段多口、重复 IPv6 链路本地地址及不同作用域、绑定与报告身份一致；缺失接口拒绝执行。
- `ui/src/domain/inner-setup.test.ts` 与 `ui/e2e/console.spec.ts`：两端筛选、虚拟口、换机清选择、无辅测机和重扫选择恢复。

## 验证

验证环境：macOS，本机 Rust 工具链、真实 Chromium，以及 Windows MSVC 目标的静态检查。浏览器扫描与运行响应使用测试桩，未执行 Windows 双机接线或 CPE 实机灌包验收。

| 检查 | 结果 |
| --- | --- |
| `npm ci` | 通过 |
| `npm run test` | 34 个文件，500 项通过 |
| `npm run build` | 类型检查、分层检查、单文件构建和产物闸门通过 |
| `npm run verify` | 通过；已更新 `src/master/webui.html` |
| `npm run test:e2e` | Chromium 18 项通过 |
| `cargo fmt --check` | 通过 |
| `cargo test --locked` | 881 项通过，0 失败，1 项忽略（浏览器服务由 Playwright 单独启动） |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过 |
| `cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc -- -D warnings` | 通过 |
| `python3 dist/build_config_docs_bundle.py 6.5.0` | 通过；23 项文档/配置重新打包 |
| 文档包 SHA-256 校验 | 通过；`e02aac33e1d795f5e201fd0cec5b6f4fe3b4e4ff6a06a17e53676bc98d5e6f24` |
| `git diff --check` | 通过 |

本次不提交、不推送，也不更改版本号。
