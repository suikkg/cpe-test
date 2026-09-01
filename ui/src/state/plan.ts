import { computed, reactive, watch } from 'vue';
import { api } from '../api/client';
import type { BootstrapOut, PlanOut } from '../api/dto';
import {
  activeNicPolicies,
  defaultGlobals,
  emptyGlobals,
  type UiGlobals,
  type UiNicPolicy,
} from '../domain/globals';
import { pruneBindings, reconcileLinkSets, type ManagedLinkSet } from '../domain/grouping';
import { buildCandidates, type Candidate, type LinkFilter } from '../domain/pairs';
import { emptyPlan, ensureDefaults, type UiPlan } from '../domain/plan-build';
import { parseProject, serializeProject } from '../domain/project';
import { parseRunRequest } from '../domain/rerun';
import { agentNics, masterNics } from './inventory';

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
    };
    if (!parsed.ui) return false;
    if (!Array.isArray(parsed.ui.suites) || !Array.isArray(parsed.ui.bindings)) return false;
    plan.ui = ensureDefaults(parsed.ui);
    plan.linkSets = Array.isArray(parsed.linkSets) ? parsed.linkSets : [];
    plan.filter = parsed.filter ?? 'all';
    if (typeof parsed.duration === 'number' && parsed.duration > 0) {
      plan.duration = parsed.duration;
    }
    plan.resume = parsed.resume === true;
    plan.screenshot = parsed.screenshot === true;
    plan.limitUdpByLinkSpeed = parsed.limitUdpByLinkSpeed === true;
    plan.globals = parsed.globals ? { ...emptyGlobals(), ...parsed.globals } : defaultGlobals();
    plan.nicPolicies = Array.isArray(parsed.nicPolicies) ? parsed.nicPolicies : [];
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
  plan.limitUdpByLinkSpeed = false;
  plan.globals = defaultGlobals();
  plan.nicPolicies = [];
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
    ping_max_rtt_ms: globals.ping_max_rtt_ms,
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
    plan.previewError = error instanceof Error ? error.message : String(error);
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

export function exportProject(): string {
  return serializeProject(plan.ui);
}
