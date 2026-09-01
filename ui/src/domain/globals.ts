/** 执行请求里跨套件生效的全局默认档位与策略。 */

export function parseTokenList(raw: string): string[] {
  return String(raw ?? '')
    .split(/[,，、\s]+/)
    .map((token) => token.trim())
    .filter((token) => token !== '');
}

export function formatTokenList(values: readonly string[]): string {
  return values.join(', ');
}

export function parseNumberList(raw: string): number[] {
  return parseTokenList(raw)
    .map((token) => Number(token))
    .filter((value) => Number.isFinite(value) && value > 0)
    .map((value) => Math.trunc(value));
}

export function formatNumberList(values: readonly number[]): string {
  return values.join(', ');
}

export interface UiGlobals {
  tcp_windows: string[];
  tcp_streams: number[];
  udp_bandwidths: string[];
  udp_lengths: string[];
  udp_windows: string[];
  /** 0 表示不覆盖。 */
  udp_streams: number;
  /** 0 = 沿用配置里的 `ping.count`。 */
  ping_count: number;
  /** 空 = 沿用配置里的 `ping.payload_sizes`。 */
  ping_payload_sizes: number[];
  /** 0 = 沿用配置里的 `ping.max_rtt_ms`。 */
  ping_max_rtt_ms: number;
}

export function emptyGlobals(): UiGlobals {
  return {
    tcp_windows: [],
    tcp_streams: [],
    udp_bandwidths: [],
    udp_lengths: [],
    udp_windows: [],
    udp_streams: 0,
    ping_count: 0,
    ping_payload_sizes: [],
    ping_max_rtt_ms: 0,
  };
}

export function defaultGlobals(): UiGlobals {
  return {
    ...emptyGlobals(),
    udp_bandwidths: ['2500m'],
    tcp_windows: ['4m'],
  };
}

export function globalsAreEmpty(globals: UiGlobals): boolean {
  return (
    globals.tcp_windows.length === 0 &&
    globals.tcp_streams.length === 0 &&
    globals.udp_bandwidths.length === 0 &&
    globals.udp_lengths.length === 0 &&
    globals.udp_windows.length === 0 &&
    globals.udp_streams === 0 &&
    globals.ping_count === 0 &&
    globals.ping_payload_sizes.length === 0 &&
    globals.ping_max_rtt_ms === 0
  );
}

export interface UiNicPolicy {
  endpoint: string;
  rx_target: string;
  udp_bandwidth: string;
  udp_length: string;
}

export function emptyNicPolicy(endpoint: string): UiNicPolicy {
  return { endpoint, rx_target: '', udp_bandwidth: '', udp_length: '' };
}

export function nicPolicyIsEmpty(policy: UiNicPolicy): boolean {
  return (
    policy.rx_target.trim() === '' &&
    policy.udp_bandwidth.trim() === '' &&
    policy.udp_length.trim() === ''
  );
}

export function policyFor(policies: readonly UiNicPolicy[], endpoint: string): UiNicPolicy {
  return policies.find((policy) => policy.endpoint === endpoint) ?? emptyNicPolicy(endpoint);
}

export function setNicPolicy(
  policies: readonly UiNicPolicy[],
  endpoint: string,
  patch: Partial<Omit<UiNicPolicy, 'endpoint'>>,
): UiNicPolicy[] {
  const next = { ...policyFor(policies, endpoint), ...patch, endpoint };
  const rest = policies.filter((policy) => policy.endpoint !== endpoint);
  return nicPolicyIsEmpty(next) ? rest : [...rest, next];
}

export function activeNicPolicies(policies: readonly UiNicPolicy[]): UiNicPolicy[] {
  return policies.filter((policy) => !nicPolicyIsEmpty(policy));
}
