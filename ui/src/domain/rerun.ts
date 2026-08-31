import { emptyGlobals, type UiGlobals, type UiNicPolicy } from './globals';
import { ensureDefaults, type UiPlan } from './plan-build';

/**
 * 把 `runs/<run>/request.json` 读回成控制台的编辑态。纯函数。
 *
 * 这是「重新执行这一轮」的第一半。第二半**必须**是人再走一次预览：
 * `plan_hash` 是「界面上确认的东西 == 实际跑的东西」唯一的强制点，隔了一夜的
 * 网口拓扑可能已经变了，老计划里的端点未必还存在。所以这里**丢掉原来的
 * `plan_hash`**——带着它去开跑，要么被服务端拒（拓扑变了），要么在没人复核的
 * 情况下放行，两种都不是想要的。
 *
 * 形状检查只做到「能不能用」，语义（引用完整性、端点存在性、参数范围）照旧交给
 * `/api/plan` 的报错（ADR-11）。
 */

export interface RerunSnapshot {
  duration: number;
  resume: boolean;
  screenshot: boolean;
  limitUdpByLinkSpeed: boolean;
  globals: UiGlobals;
  nicPolicies: UiNicPolicy[];
  /** 套件计划；矩阵路径跑出来的旧目录没有它 */
  plan: UiPlan | null;
}

function stringList(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string') : [];
}

function numberList(value: unknown): number[] {
  return Array.isArray(value)
    ? value.filter((item): item is number => typeof item === 'number' && Number.isFinite(item))
    : [];
}

function count(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : fallback;
}

function nicPolicies(value: unknown): UiNicPolicy[] {
  if (!Array.isArray(value)) return [];
  const out: UiNicPolicy[] = [];
  for (const item of value) {
    if (!item || typeof item !== 'object') continue;
    const record = item as Record<string, unknown>;
    const endpoint = typeof record.endpoint === 'string' ? record.endpoint : '';
    if (!endpoint) continue;
    out.push({
      endpoint,
      rx_target: typeof record.rx_target === 'string' ? record.rx_target : '',
      udp_bandwidth: typeof record.udp_bandwidth === 'string' ? record.udp_bandwidth : '',
      udp_length: typeof record.udp_length === 'string' ? record.udp_length : '',
    });
  }
  return out;
}

export function parseRunRequest(raw: unknown): RerunSnapshot | null {
  if (!raw || typeof raw !== 'object') return null;
  const request = raw as Record<string, unknown>;

  const globals: UiGlobals = {
    ...emptyGlobals(),
    tcp_windows: stringList(request.tcp_windows),
    tcp_streams: numberList(request.tcp_streams),
    udp_bandwidths: stringList(request.udp_bandwidths),
    udp_lengths: stringList(request.udp_lengths),
    udp_windows: stringList(request.udp_windows),
    udp_streams: count(request.udp_streams, 0),
    ping_count: count(request.ping_count, 0),
    ping_payload_sizes: numberList(request.ping_payload_sizes),
  };

  const rawPlan = request.ui_plan;
  let plan: UiPlan | null = null;
  if (rawPlan && typeof rawPlan === 'object') {
    const candidate = rawPlan as UiPlan & { plan_hash?: unknown };
    if (Array.isArray(candidate.suites) && Array.isArray(candidate.bindings)) {
      // `plan_hash` 是**上一轮**的握手值，不能跟着回到编辑态：留着它，下一次
      // 提交就可能带着一个没有人复核过的哈希去开跑。
      const { plan_hash: _discarded, ...rest } = candidate;
      plan = ensureDefaults(rest as UiPlan);
    }
  }

  return {
    duration: count(request.duration, 180) || 180,
    resume: request.resume === true,
    screenshot: request.screenshot === true,
    limitUdpByLinkSpeed: request.limit_udp_by_link_speed === true,
    globals,
    nicPolicies: nicPolicies(request.nic_policies),
    plan,
  };
}
