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

export interface RunEntry {
  id: string;
  modified: string;
  has_report: boolean;
  has_rows: boolean;
  has_xlsx: boolean;
  has_request: boolean;
  bytes: number;
}

export interface ReplayOut {
  id: string;
  report: string;
  xlsx: string | null;
  rows: number;
  skipped: number;
  warnings: string[];
}

export interface RunRequestOut {
  id: string;
  request: unknown;
}
