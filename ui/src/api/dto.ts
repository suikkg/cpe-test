/**
 * 服务端 DTO 的 TypeScript 对应物。
 *
 * **权威在 Rust**（`src/master/webui/model.rs` 与 `src/protocol.rs`），这里是
 * 手写的镜像。每个类型上方注明来源符号名——不写行号：行号会烂，定位用
 * `grep -n "struct <名字>" src/master/webui/model.rs`。
 */

export interface NicInfo {
  name: string;
  description: string;
  role: string;
  ipv4: string;
  gateway_v4: string;
  ipv6_ll: string;
  ipv6_global: string;
  zone: string;
  speed_mbps: number;
  is_wifi: boolean;
  wifi_band: string;
  /**
   * 无线上下文。旧版 agent 不上报这几项，所以全部可缺省——
   * 界面上一律按「没读到」处理，不拿 0 当占位。
   */
  wifi_ssid?: string;
  wifi_signal_pct?: number | null;
  wifi_channel?: number | null;
  wifi_radio?: string;
  ifindex: number;
}

export interface HostInfo {
  hostname: string;
  os: string;
  interfaces: NicInfo[];
}

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
  ping_max_rtt_ms: number;
  ping_small_max_bytes: number,
  ping_medium_max_bytes: number,
  ping_wired_small_avg_rtt_ms: number,
  ping_wired_small_max_rtt_ms: number,
  ping_wired_medium_avg_rtt_ms: number,
  ping_wired_medium_max_rtt_ms: number,
  ping_wired_large_avg_rtt_ms: number,
  ping_wired_large_max_rtt_ms: number,
  ping_wifi_small_avg_rtt_ms: number,
  ping_wifi_small_max_rtt_ms: number,
  ping_wifi_medium_avg_rtt_ms: number,
  ping_wifi_medium_max_rtt_ms: number,
  ping_wifi_large_avg_rtt_ms: number,
  ping_wifi_large_max_rtt_ms: number,
  /**
   * 主控当前生效的**解析后配置**：判定与灌包参数的完整基线。
   *
   * 只含 `link_profiles` / `iperf` / `ctstraffic` / `ping` 四块，不含任何连接
   * 身份。导出项目时原样固化——界面上没有输入框的那些参数（`rate_check` 的
   * 负载上限与余量、角色配对门限、ctsTraffic 的帧率）只能靠它跨机复现。
   *
   * 前端不解释它的内容，只负责**原样搬运**：字段语义在 Rust 的 `Config` 里，
   * 在这边再写一份类型定义就是两份会漂的实现。
   */
  master_config: Record<string, unknown>;
  screenshot: boolean;
  /** 灌包期间并发探负载下时延；默认关。 */
  probe_during_traffic?: boolean;
  /** 每个 ping 单元额外做一次路径 MTU 探测；只进诊断。 */
  probe_path_mtu?: boolean;
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
  ipv4_prefixes: string[];
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

export interface PlannedUnit {
  seq: number;
  title: string;
  est_secs: number;
  resumed: boolean;
  load: string[];
  /** 每条腿**最终**按什么门限判、门限来自哪一层。 */
  targets: string[];
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
  est_total_secs: number;
  est_full_secs: number;
  notices: string[];
  blocking_errors?: string[];
  sections?: PlanSection[];
  trace?: PlanTrace[];
  plan_hash?: string;
  topology_fingerprint?: string;
  ui_plan_supported: boolean;
}

export interface UnitStatus {
  seq: number;
  title: string;
  verdict: string;
  reason_code: string;
  reason_detail: string;
  skipped: boolean;
  secs: number;
  link_group: string;
  /**
   * 本单元判定用的接收端 RX 平均（Mbps）；没起过流的单元为 null。
   *
   * 与报告汇总行同源。进度页要靠它回答「这一轮的数在不在往下滑」——
   * 只看 PASS/FAIL 的话，热衰减和 Wi-Fi 退避要等报告出来才发现。
   */
  rx_avg?: number | null;
  /** 本单元判定用的门限（Mbps）；null = 这一轮没有门限（Observe/Discover）。 */
  target_mbps?: number | null;
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
  done: UnitStatus[];
  counts: RunCounts;
  eta_secs: number | null;
  aborted_at_unit: number | null;
  report: string;
  finished: boolean;
}

export interface ProgressOut {
  running: boolean;
  from: number;
  lines: string[];
  report: string;
  run: RunStatus;
  units_from: number;
}

export interface MonitorPoint {
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

/** 一轮运行的单元级判定分布，取自 `meta.json`，与报告顶部那八个格子同源。 */
export interface VerdictTotals {
  total: number;
  pass: number;
  rate_fail: number;
  measured: number;
  not_evaluated: number;
  setup_error: number;
  skipped: number;
}

export interface RunEntry {
  id: string;
  modified: string;
  has_report: boolean;
  has_rows: boolean;
  has_xlsx: boolean;
  has_request: boolean;
  bytes: number;
  /**
   * 本轮的开始时刻，取自 `meta.json`。
   *
   * **和 `modified` 不是一回事**：后者是目录的修改时刻，重放一次报告它就会变，
   * 于是隔夜回来看到的「时间」是自己上午点重放的那一下。
   */
  started?: string;
  elapsed?: string;
  totals?: VerdictTotals;
  total_units?: number;
  /** 读到 `meta.json` 了吗；没有就是命令行旧目录或崩在写它之前。 */
  has_meta?: boolean;
}

export interface ReplayOut {
  id: string;
  report: string;
  xlsx: string | null;
  rows: number;
  skipped: number;
  warnings: string[];
}

/** `/api/runs/compare` 的回包。 */
export interface CompareOut {
  baseline: string;
  current: string;
  report: string;
  /** 两轮的 plan_hash 是否一致；不一致时「新增/缺失」说的是计划差异。 */
  same_plan: boolean;
  /** 有没有值得拦下来的变化（判定变坏或明显掉速）。 */
  has_regression: boolean;
  regressed: number;
  slower: number;
  fixed: number;
  added: number;
  disappeared: number;
  unchanged: number;
}

export interface RunRequestOut {
  id: string;
  request: unknown;
}
