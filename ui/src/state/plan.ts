import { computed, reactive, watch } from 'vue';
import { api, errorMessage } from '../api/client';
import type { BootstrapOut, PlanOut } from '../api/dto';
import {
  activeNicPolicies,
  defaultGlobals,
  normalizeGlobals,
  resolveEffectiveGlobals,
  type UiGlobals,
  type UiNicPolicy,
} from '../domain/globals';
import { pruneBindings, reconcileLinkSets, type ManagedLinkSet } from '../domain/grouping';
import { buildCandidates, type Candidate, type LinkFilter } from '../domain/pairs';
import { emptyPlan, ensureDefaults, type UiPlan } from '../domain/plan-build';
import { parseProject, serializeProject, type ProjectSettings } from '../domain/project';
import { parseRunRequest } from '../domain/rerun';
import { agentNics, masterNics } from './inventory';
import { session } from './session';

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

const DRAFT_KEY = 'cpe_ui_plan_draft';

export const plan = reactive({
  ui: ensureDefaults(emptyPlan()) as UiPlan,
  linkSets: [] as ManagedLinkSet[],
  filter: 'all' as LinkFilter,
  stale: [] as Array<{ setId: string; pairId: string; src: string; dst: string }>,
  duration: 180,
  resume: false,
  screenshot: false,
  limitUdpByLinkSpeed: false,
  globals: defaultGlobals() as UiGlobals,
  nicPolicies: [] as UiNicPolicy[],
  /**
   * 项目带来的**解析后主控配置**；`null` = 没有项目，用本机基线。
   *
   * 前端不解释它的内容，只原样搬运——字段语义在 Rust 的 `Config` 里。
   */
  masterConfig: null as Record<string, unknown> | null,
  preview: null as PlanOut | null,
  previewRequestFingerprint: '',
  previewing: false,
  previewError: '',
});

export const candidates = computed<Candidate[]>(() =>
  buildCandidates(masterNics.value, agentNics.value),
);

const boundSetIds = computed(() => new Set(plan.ui.bindings.map((b) => b.link_set_id)));

export function reconcile(): void {
  const result = reconcileLinkSets(
    plan.linkSets,
    candidates.value,
    plan.filter,
    boundSetIds.value,
  );
  if (result.skipped) return;
  plan.linkSets = result.linkSets;
  plan.stale = result.stale;
  plan.ui = pruneBindings(
    { ...plan.ui, link_sets: result.linkSets.map(({ auto: _auto, ...rest }) => rest) },
    result.linkSets,
  );
}

function saveDraft(): void {
  try {
    localStorage.setItem(
      DRAFT_KEY,
      JSON.stringify({
        ui: plan.ui,
        linkSets: plan.linkSets,
        filter: plan.filter,
        duration: plan.duration,
        resume: plan.resume,
        screenshot: plan.screenshot,
        limitUdpByLinkSpeed: plan.limitUdpByLinkSpeed,
        globals: plan.globals,
        nicPolicies: plan.nicPolicies,
        masterConfig: plan.masterConfig,
      }),
    );
  } catch {
    // localStorage 不可用时只失去草稿恢复，不阻断编辑。
  }
}

let draftRestored = false;

export function loadDraft(): boolean {
  try {
    const raw = localStorage.getItem(DRAFT_KEY);
    if (!raw) return false;
    const parsed = JSON.parse(raw) as {
      ui?: UiPlan;
      linkSets?: ManagedLinkSet[];
      filter?: LinkFilter;
      duration?: number;
      resume?: boolean;
      screenshot?: boolean;
      limitUdpByLinkSpeed?: boolean;
      globals?: UiGlobals;
      nicPolicies?: UiNicPolicy[];
      masterConfig?: Record<string, unknown> | null;
    };
    if (!parsed.ui) return false;
    if (!Array.isArray(parsed.ui.suites) || !Array.isArray(parsed.ui.bindings)) return false;
    const rawUi = parsed.ui as unknown as Record<string, unknown>;
    const rawRecipes = rawUi.recipes;
    if (
      rawRecipes !== undefined &&
      (typeof rawRecipes !== 'object' || rawRecipes === null || Array.isArray(rawRecipes))
    ) {
      return false;
    }
    if (rawRecipes && typeof rawRecipes === 'object') {
      for (const key of ['tcp', 'udp', 'ping']) {
        const value = (rawRecipes as Record<string, unknown>)[key];
        if (value !== undefined && !Array.isArray(value)) return false;
      }
    }
    plan.ui = ensureDefaults(parsed.ui);
    plan.linkSets = Array.isArray(parsed.linkSets) ? parsed.linkSets : [];
    plan.filter = parsed.filter ?? 'all';
    if (typeof parsed.duration === 'number' && parsed.duration > 0) {
      plan.duration = parsed.duration;
    }
    plan.resume = parsed.resume === true;
    plan.screenshot = parsed.screenshot === true;
    plan.limitUdpByLinkSpeed = parsed.limitUdpByLinkSpeed === true;
    plan.globals = parsed.globals ? normalizeGlobals(parsed.globals) : defaultGlobals();
    plan.nicPolicies = Array.isArray(parsed.nicPolicies) ? parsed.nicPolicies : [];
    plan.masterConfig = isPlainObject(parsed.masterConfig) ? parsed.masterConfig : null;
    draftRestored = true;
    return true;
  } catch {
    return false;
  }
}

let saveTimer: ReturnType<typeof setTimeout> | undefined;
watch(
  () => [
    plan.ui,
    plan.linkSets,
    plan.filter,
    plan.duration,
    plan.resume,
    plan.screenshot,
    plan.limitUdpByLinkSpeed,
    plan.globals,
    plan.nicPolicies,
    plan.masterConfig,
  ],
  () => {
    if (saveTimer !== undefined) clearTimeout(saveTimer);
    saveTimer = setTimeout(saveDraft, 500);
  },
  { deep: true },
);

export function applyBootstrapDefaults(bootstrap: BootstrapOut): void {
  if (draftRestored) return;
  if (bootstrap.duration > 0) plan.duration = bootstrap.duration;
  plan.screenshot = bootstrap.screenshot;
}

export function reset(): void {
  draftRestored = false;
  plan.ui = ensureDefaults(emptyPlan());
  plan.linkSets = [];
  plan.filter = 'all';
  plan.stale = [];
  plan.preview = null;
  plan.previewRequestFingerprint = '';
  plan.previewing = false;
  plan.previewError = '';
  plan.duration = 180;
  plan.resume = false;
  plan.screenshot = false;
  plan.limitUdpByLinkSpeed = false;
  plan.globals = defaultGlobals();
  plan.nicPolicies = [];
  plan.masterConfig = null;
}

export function restoreDefaultProject(): void {
  plan.ui = ensureDefaults(emptyPlan());
  plan.linkSets = [];
  plan.filter = 'all';
  plan.stale = [];
  plan.preview = null;
  plan.previewRequestFingerprint = '';
  plan.previewError = '';
  reconcile();
}

export function buildRunRequest(): Record<string, unknown> {
  const globals = plan.globals;
  return {
    duration: plan.duration,
    resume: plan.resume,
    screenshot: plan.screenshot,
    limit_udp_by_link_speed: plan.limitUdpByLinkSpeed,
    tcp_windows: globals.tcp_windows,
    tcp_streams: globals.tcp_streams,
    udp_bandwidths: globals.udp_bandwidths,
    udp_lengths: globals.udp_lengths,
    udp_windows: globals.udp_windows,
    ping_count: globals.ping_count,
    ping_payload_sizes: globals.ping_payload_sizes,
    ping_small_max_bytes: globals.ping_small_max_bytes,
    ping_medium_max_bytes: globals.ping_medium_max_bytes,
    ping_wired_small_avg_rtt_ms: globals.ping_wired_small_avg_rtt_ms,
    ping_wired_small_max_rtt_ms: globals.ping_wired_small_max_rtt_ms,
    ping_wired_medium_avg_rtt_ms: globals.ping_wired_medium_avg_rtt_ms,
    ping_wired_medium_max_rtt_ms: globals.ping_wired_medium_max_rtt_ms,
    ping_wired_large_avg_rtt_ms: globals.ping_wired_large_avg_rtt_ms,
    ping_wired_large_max_rtt_ms: globals.ping_wired_large_max_rtt_ms,
    ping_wifi_small_avg_rtt_ms: globals.ping_wifi_small_avg_rtt_ms,
    ping_wifi_small_max_rtt_ms: globals.ping_wifi_small_max_rtt_ms,
    ping_wifi_medium_avg_rtt_ms: globals.ping_wifi_medium_avg_rtt_ms,
    ping_wifi_medium_max_rtt_ms: globals.ping_wifi_medium_max_rtt_ms,
    ping_wifi_large_avg_rtt_ms: globals.ping_wifi_large_avg_rtt_ms,
    ping_wifi_large_max_rtt_ms: globals.ping_wifi_large_max_rtt_ms,
    wifi_pair_rx_target_mbps: globals.wifi_pair_rx_target_mbps,
    wifi_pair_bidir_rx_target_mbps: globals.wifi_pair_bidir_rx_target_mbps,
    wifi_pair_bidir_total_rx_target_mbps: globals.wifi_pair_bidir_total_rx_target_mbps,
    wifi_band_thresholds: globals.wifi_band_thresholds,
    wifi_pair_thresholds: globals.wifi_pair_thresholds,
    // 项目带来的**解析后主控配置**：界面上没有输入框、却决定「怎么跑、怎么判」
    // 的那一整块（rate_check 的负载上限与余量、角色配对门限、ctsTraffic 参数）。
    // 没有项目时不发，后端用自己的基线。
    ...(plan.masterConfig ? { master_config: plan.masterConfig } : {}),
    ...(globals.udp_streams > 0 ? { udp_streams: globals.udp_streams } : {}),
    pairs: [],
    nic_policies: activeNicPolicies(plan.nicPolicies),
    ui_plan: plan.ui,
  };
}

export function previewIsCurrent(): boolean {
  return (
    !!plan.preview?.plan_hash &&
    plan.previewRequestFingerprint === JSON.stringify(buildRunRequest())
  );
}

export async function preview(): Promise<void> {
  plan.previewing = true;
  plan.previewError = '';
  const request = buildRunRequest();
  const fingerprint = JSON.stringify(request);
  try {
    plan.preview = await api.post<PlanOut>('/api/plan', request);
    plan.previewRequestFingerprint = fingerprint;
  } catch (error) {
    plan.preview = null;
    plan.previewRequestFingerprint = '';
    plan.previewError = errorMessage(error);
  } finally {
    plan.previewing = false;
  }
}

export const projectNotices = reactive({ items: [] as string[], error: '' });

export function importProject(text: string): boolean {
  const result = parseProject(text);
  projectNotices.items = result.notices;
  projectNotices.error = result.error ?? '';
  if (!result.ok || !result.plan) return false;
  plan.ui = result.plan;
  plan.linkSets = result.plan.link_sets.map((set) => ({ ...set, auto: false }));
  const settings: ProjectSettings = result.settings ?? {};
  plan.duration =
    typeof settings.duration === 'number' && settings.duration > 0 ? settings.duration : 180;
  plan.limitUdpByLinkSpeed = settings.limit_udp_by_link_speed === true;
  plan.globals = settings.globals ? normalizeGlobals(settings.globals) : defaultGlobals();
  plan.nicPolicies = result.nicPolicies ?? [];
  // 老项目（v1/v2）没有这一块：保持 null，用目标主控自己的基线，行为与从前一致。
  plan.masterConfig = settings.masterConfig ?? null;
  reconcile();
  return true;
}

export function adoptRunRequest(raw: unknown, skipPassed: boolean): boolean {
  const snapshot = parseRunRequest(raw);
  if (!snapshot) return false;
  plan.duration = snapshot.duration;
  plan.screenshot = snapshot.screenshot;
  plan.limitUdpByLinkSpeed = snapshot.limitUdpByLinkSpeed;
  plan.globals = snapshot.globals;
  plan.nicPolicies = snapshot.nicPolicies;
  // 归档里那一份判定基线要跟着回来；当时没带项目就显式清空，不能把当前
  // 内存里另一个项目的那份留着接着用。
  plan.masterConfig = snapshot.masterConfig;
  plan.resume = skipPassed;
  if (snapshot.plan) {
    plan.ui = snapshot.plan;
    plan.linkSets = snapshot.plan.link_sets.map((set) => ({ ...set, auto: false }));
    reconcile();
  }
  plan.preview = null;
  plan.previewRequestFingerprint = '';
  plan.previewError = '';
  return true;
}

/**
 * 导出项目。
 *
 * 关键一步是 `resolveEffectiveGlobals`：导出的必须是**主控当前真正会用的值**，
 * 不是输入框状态。留空的格子在编辑态里是 `0` / `[]`，而屏幕上显示的灰字
 * 「默认 30」只存在于 bootstrap 回填里——直接序列化编辑态，换一台主控导入就会
 * 改用那台机器自己的默认值，判定口径静默改变。
 */
/**
 * 导出项目。**拿不到判定基线时宁可不导**，返回 `null` 并留下一条错误。
 *
 * 空的 `master_config` 在后端等价于「没带」——导出一个结构完整、`master_config`
 * 是 `{}` 的文件，看不出任何异常，换台机器导入却会静默回落到那台机器的基线。
 * 这正是「项目自带全部判定参数」要根除的故障，导出这一侧不能自己制造。
 */
export function exportProject(): string | null {
  // 判定与灌包参数的完整基线。已经导入过项目就原样带走它自己的那一份，
  // 否则固化主控当前生效的这一份——两种情况下导出的都是**这一轮真正会用
  // 的参数**，而不是「用户改过的那几个」。
  const masterConfig = plan.masterConfig ?? session.bootstrap?.master_config;
  if (!masterConfig || Object.keys(masterConfig).length === 0) {
    projectNotices.error =
      '还没拿到主控的判定基线（档位、门限、角色配对门限等），现在导出的项目在别的机器上会回落到那台机器的配置。' +
      '请等页面加载完成或刷新一次再导出。';
    return null;
  }
  projectNotices.error = '';
  return serializeProject(
    plan.ui,
    {
      duration: plan.duration,
      limit_udp_by_link_speed: plan.limitUdpByLinkSpeed,
      globals: resolveEffectiveGlobals(plan.globals, session.bootstrap),
      masterConfig,
    },
    activeNicPolicies(plan.nicPolicies),
  );
}
