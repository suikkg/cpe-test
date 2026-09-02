import { normalizeGlobals, type UiGlobals, type UiNicPolicy } from './globals';
import { ensureDefaults, type UiPlan } from './plan-build';

/** 把 runs/<run>/request.json 读回成控制台编辑态。 */
export interface RerunSnapshot {
  duration: number;
  resume: boolean;
  screenshot: boolean;
  limitUdpByLinkSpeed: boolean;
  globals: UiGlobals;
  nicPolicies: UiNicPolicy[];
  plan: UiPlan | null;
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

  const globals: UiGlobals = normalizeGlobals(request);

  const rawPlan = request.ui_plan;
  let plan: UiPlan | null = null;
  if (rawPlan && typeof rawPlan === 'object') {
    const candidate = rawPlan as UiPlan & { plan_hash?: unknown };
    if (Array.isArray(candidate.suites) && Array.isArray(candidate.bindings)) {
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
