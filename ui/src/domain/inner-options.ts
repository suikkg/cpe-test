export interface InnerPingPolicy {
  small_max_bytes: number;
  medium_max_bytes: number;
  wired_small_avg_rtt_ms: number;
  wired_small_max_rtt_ms: number;
  wired_medium_avg_rtt_ms: number;
  wired_medium_max_rtt_ms: number;
  wired_large_avg_rtt_ms: number;
  wired_large_max_rtt_ms: number;
  wifi_small_avg_rtt_ms: number;
  wifi_small_max_rtt_ms: number;
  wifi_medium_avg_rtt_ms: number;
  wifi_medium_max_rtt_ms: number;
  wifi_large_avg_rtt_ms: number;
  wifi_large_max_rtt_ms: number;
}
export function defaultInnerPingPolicy(): InnerPingPolicy { return {
  small_max_bytes: 128,
  medium_max_bytes: 2000,
  wired_small_avg_rtt_ms: 10,
  wired_small_max_rtt_ms: 30,
  wired_medium_avg_rtt_ms: 20,
  wired_medium_max_rtt_ms: 50,
  wired_large_avg_rtt_ms: 50,
  wired_large_max_rtt_ms: 100,
  wifi_small_avg_rtt_ms: 30,
  wifi_small_max_rtt_ms: 80,
  wifi_medium_avg_rtt_ms: 50,
  wifi_medium_max_rtt_ms: 100,
  wifi_large_avg_rtt_ms: 100,
  wifi_large_max_rtt_ms: 200,
}; }
export function validateInnerPingPolicy(policy: InnerPingPolicy): void {
  const expected = Object.keys(defaultInnerPingPolicy());
  if (!policy || typeof policy !== 'object' || Object.keys(policy).some(k => !expected.includes(k))) throw new Error('Ping 门限字段无效');
  if (!Number.isInteger(policy.small_max_bytes) || !Number.isInteger(policy.medium_max_bytes) || policy.small_max_bytes <= 0 || policy.medium_max_bytes <= policy.small_max_bytes || policy.medium_max_bytes > 65500) throw new Error('Ping 包长分档须为 0 < small < medium <= 65500');
  if (!Number.isFinite(policy.wired_small_avg_rtt_ms) || !Number.isFinite(policy.wired_small_max_rtt_ms) || policy.wired_small_avg_rtt_ms <= 0 || policy.wired_small_avg_rtt_ms > policy.wired_small_max_rtt_ms) throw new Error('Ping RTT 门限须为正数，平均门限不能大于最大门限');
  if (!Number.isFinite(policy.wired_medium_avg_rtt_ms) || !Number.isFinite(policy.wired_medium_max_rtt_ms) || policy.wired_medium_avg_rtt_ms <= 0 || policy.wired_medium_avg_rtt_ms > policy.wired_medium_max_rtt_ms) throw new Error('Ping RTT 门限须为正数，平均门限不能大于最大门限');
  if (!Number.isFinite(policy.wired_large_avg_rtt_ms) || !Number.isFinite(policy.wired_large_max_rtt_ms) || policy.wired_large_avg_rtt_ms <= 0 || policy.wired_large_avg_rtt_ms > policy.wired_large_max_rtt_ms) throw new Error('Ping RTT 门限须为正数，平均门限不能大于最大门限');
  if (!Number.isFinite(policy.wifi_small_avg_rtt_ms) || !Number.isFinite(policy.wifi_small_max_rtt_ms) || policy.wifi_small_avg_rtt_ms <= 0 || policy.wifi_small_avg_rtt_ms > policy.wifi_small_max_rtt_ms) throw new Error('Ping RTT 门限须为正数，平均门限不能大于最大门限');
  if (!Number.isFinite(policy.wifi_medium_avg_rtt_ms) || !Number.isFinite(policy.wifi_medium_max_rtt_ms) || policy.wifi_medium_avg_rtt_ms <= 0 || policy.wifi_medium_avg_rtt_ms > policy.wifi_medium_max_rtt_ms) throw new Error('Ping RTT 门限须为正数，平均门限不能大于最大门限');
  if (!Number.isFinite(policy.wifi_large_avg_rtt_ms) || !Number.isFinite(policy.wifi_large_max_rtt_ms) || policy.wifi_large_avg_rtt_ms <= 0 || policy.wifi_large_avg_rtt_ms > policy.wifi_large_max_rtt_ms) throw new Error('Ping RTT 门限须为正数，平均门限不能大于最大门限');
}
