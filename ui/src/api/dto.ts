/**
 * 服务端 DTO 的 TypeScript 对应物。
 *
 * **权威在 Rust**（`src/master/webui/model.rs` 与 `src/protocol.rs`），这里是
 * 手写的镜像。每个类型上方注明来源符号名——不写行号：行号会烂，定位用
 * `grep -n "struct <名字>" src/master/webui/model.rs`。
 *
 * 为什么不做代码生成：13 个端点不值得一条构建链，而那条链会把「改 Rust 必须
 * 重跑生成器」加进每个贡献者的工作流。守护改成**契约测试**：Rust 侧把每个
 * `*Out` 的样例序列化成 JSON，Vitest 反序列化断言字段——字段漂移两边都红。
 */

// ---------------------------------------------------------------------------
// 拓扑（Rust: protocol.rs::NicInfo / HostInfo）
// ---------------------------------------------------------------------------

export interface NicInfo {
  name: string;
  description: string;
  /** SGMII1G / SGMII2.5G / RNDIS / WIFI5G / WIFI2.4G / WIFI / UNKNOWN */
  role: string;
  ipv4: string;
  gateway_v4: string;
  ipv6_ll: string;
  ipv6_global: string;
  /** fe80 的 zone：Windows 是接口索引数字，macOS 是接口名 */
  zone: string;
  /** 协商速率 Mbps；未知为 0 */
  speed_mbps: number;
  is_wifi: boolean;
  /** "2.4GHz" / "5GHz" / "6GHz" / "" */
  wifi_band: string;
  ifindex: number;
}

export interface HostInfo {
  hostname: string;
  os: string;
  interfaces: NicInfo[];
}

// ---------------------------------------------------------------------------
// 会话（Rust: webui/model.rs::BootstrapOut / ConnectOut, webui/api.rs::ConnectReq）
// ---------------------------------------------------------------------------

export interface BootstrapOut {
  agent_host: string;
  agent_port: number;
  token_configured: boolean;
  ipv4_prefixes: string[];
  duration: number;
  tcp_windows: string[];
  tcp_streams: number[];
  udp_bandwidths: string[];
  udp_lengths: string[];
  udp_windows: string[];
  udp_streams: number;
  ping_count: number;
  ping_payload_sizes: number[];
  screenshot: boolean;
  ui_plan_supported: boolean;
}

export interface LocalOut {
  host: HostInfo;
  iperf3: string | null;
  version: string;
}

export interface ConnectReq {
  host: string;
  port: number;
  token: string;
  prefixes: string[];
}

export interface HealthOut {
  ok?: boolean;
  version?: string;
  capabilities?: string[];
  [key: string]: unknown;
}

export interface ConnectOut {
  health: HealthOut;
  master: HostInfo;
  agent: HostInfo;
  nic_policies: unknown[];
}

// ---------------------------------------------------------------------------
// 计划（Rust: webui/model.rs::PlanOut / PlannedUnit / PlanSection / PlanTrace）
// ---------------------------------------------------------------------------

export interface PlannedUnit {
  seq: number;
  title: string;
  est_secs: number;
  /** 开了 resume 且 24 小时内已 PASS——会被跳过 */
  resumed: boolean;
  /** 这个单元每条腿**最终**下发的参数（已含链路裁剪） */
  load: string[];
}

export interface PlanSection {
  link_set_id: string | null;
  suite_id: string | null;
  task_id: string | null;
  title: string;
  unit_seqs: number[];
}

export interface PlanTrace {
  seq: number;
  pair_id: string | null;
  link_set_id: string | null;
  suite_id: string | null;
  task_id: string | null;
  lane_id: string | null;
  recipe_id: string | null;
  protocol: string | null;
  direction: string | null;
  ip: string | null;
  requested_args: string[];
  effective_args: string[];
  value_sources: string[];
  skipped_reason: string | null;
  resumed: boolean;
}

export interface PlanOut {
  units: PlannedUnit[];
  /** 预计跳过的都真跳过时的耗时 */
  est_total_secs: number;
  /** 一个都不跳时的耗时；开着 resume 时按区间显示 */
  est_full_secs: number;
  notices: string[];
  /** 层级信息。**必须直接渲染它**，不要拿平铺的 units 自己重拼分组 */
  sections?: PlanSection[];
  trace?: PlanTrace[];
  /** 复核页与实跑之间唯一的握手 */
  plan_hash?: string;
  topology_fingerprint?: string;
  ui_plan_supported: boolean;
}

// ---------------------------------------------------------------------------
// 运行状态（Rust: master/run_status.rs — ADR-2）
//
// 这一组是 v6.0 新增的**结构化进度**。在它之前，单元级进度要靠前端解析
// `[i/total]` 和「==> 单元结果:」两种日志行；一次 11.5 小时的测试有三万行
// 日志，刷新一次页面就得全量重放。现在日志只给人看，文案可以随便改。
// ---------------------------------------------------------------------------

export interface UnitStatus {
  /** 1-based，与日志的 [i/total] 和报告的 #N 同一个数 */
  seq: number;
  title: string;
  /** Verdict::label()：PASS / RATE_FAIL / MEASURED / NOT_EVALUATED / SETUP_ERROR / SKIP */
  verdict: string;
  reason_code: string;
  reason_detail: string;
  skipped: boolean;
  secs: number;
  link_group: string;
}

export interface CurrentUnit {
  seq: number;
  title: string;
  est_secs: number;
  started_at: string;
  link_group: string;
}

export interface RunCounts {
  pass: number;
  fail: number;
  measured: number;
  not_evaluated: number;
  setup_error: number;
  skip: number;
}

export interface RunStatus {
  run_id: string;
  plan_hash: string;
  started_at: string;
  total_units: number;
  current: CurrentUnit | null;
  /** 游标增量：`units_from=N` 只回新完成的 */
  done: UnitStatus[];
  counts: RunCounts;
  /** 剩余估算秒数。**Rust 算的，前端不复算** */
  eta_secs: number | null;
  aborted_at_unit: number | null;
  /** 由 executor 回调直接写入，不再从日志里搜「报告已生成: 」 */
  report: string;
  finished: boolean;
}

export interface ProgressOut {
  running: boolean;
  /** 日志游标：下一拍该用的 from */
  from: number;
  /** 给人看的日志文本 */
  lines: string[];
  report: string;
  /** 给机器读的结构化状态 */
  run: RunStatus;
  /** 单元游标：下一拍该用的 units_from */
  units_from: number;
}

// ---------------------------------------------------------------------------
// 监控（Rust: webui/monitor.rs::MonitorSeriesOut / MonitorPoint）
// ---------------------------------------------------------------------------

export interface MonitorPoint {
  /** 会话开始后的秒数。用相对时间：两端系统时钟不保证同步 */
  t: number;
  rx_mbps: number;
  tx_mbps: number;
}

export interface MonitorSeriesOut {
  session: string;
  side: string;
  iface: string;
  from: number;
  points: MonitorPoint[];
  running: boolean;
  error: string;
}

// ---------------------------------------------------------------------------
// 历史运行（Rust: webui/runs.rs::RunEntry — ADR-15）
// ---------------------------------------------------------------------------

export interface RunEntry {
  /** 目录名。同时是 bundle.zip 的入参和 `cpe_test report` 的入参 */
  id: string;
  modified: string;
  has_report: boolean;
  /** 有 rows.jsonl 就能重放报告，即使 report.html 没写出来（崩溃场景） */
  has_rows: boolean;
  has_xlsx: boolean;
  bytes: number;
}
