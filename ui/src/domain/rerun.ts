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
  /**
   * 这一轮实际用的**解析后主控配置**；`null` = 当时就没带项目，用本机基线。
   *
   * 必须跟着重跑走。不还原它，重跑用的既不是归档里那份、也不是本机基线，
   * 而是内存里当前碰巧加载着的那份——导入项目 A 之后去重跑归档 B，B 会按
   * A 的门限判，而界面上完全看不出来。
   */
  masterConfig: Record<string, unknown> | null;
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
    masterConfig:
      request.master_config && typeof request.master_config === 'object'
        && !Array.isArray(request.master_config)
        ? (request.master_config as Record<string, unknown>)
        : null,
  };
}
