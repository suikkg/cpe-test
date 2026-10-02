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
import { reconcileImportedTopology } from '../domain/import-topology';
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
  pendingImportTopology: false,
  /** 草稿写入的可见状态，见 `DraftState`。 */
  draftState: 'idle' as DraftState,
  /** 最近一次真的写进去的时刻；没写成过就是 null。 */
  draftAt: null as number | null,
  duration: 180,
  resume: false,
  screenshot: false,
  /** 灌包期间并发探负载下时延。默认关：打开它就改变了测量条件。 */
  probeDuringTraffic: false,
  /** 每个 ping 单元额外探一次路径 MTU（带 DF 位二分）。只进诊断。 */
  probePathMtu: false,
  /**
   * 「仅本轮」强制档位：空 = 不覆盖。
   *
   * 盖在套件任务配置**之后**，所以配过参数的任务也照样被覆盖——那正是
   * 「就这一轮压一档试试」要覆盖的对象。值进 request.json，重跑跑的是同一件事。
   */
  forceTcpWindow: '',
  forceUdpBandwidth: '',
  /** 整份计划重复跑多少遍（稳定性 / 拷机）。1 = 跑一遍。 */
  rounds: 1,
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
  if (plan.pendingImportTopology) {
    const connected = !!session.connection && !session.topologyStale && !session.scanning;
    const master = connected ? session.connection!.master.interfaces
      : !session.connection && session.local && !session.localError ? session.local.host.interfaces : null;
    const agent = connected ? session.connection!.agent.interfaces : null;
    const checked = reconcileImportedTopology(plan.ui, master, agent);
    plan.ui = checked.plan;
    plan.linkSets = checked.plan.link_sets.map((set) => ({ ...set, auto: false }));
    plan.pendingImportTopology = checked.pending > 0;
    projectNotices.items.push(...checked.notices);
    if (plan.pendingImportTopology) {
      plan.stale = [];
      return;
    }
  }
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

watch(
  () => [session.connection, session.local, session.topologyStale, session.scanning, session.localError],
  () => { if (plan.pendingImportTopology) reconcile(); },
  { deep: true },
);

/**
 * 草稿的三种可见状态（方案 §11.3）。
 *
 * 「已保存」**只能在 `localStorage.setItem` 真的成功之后**说。隐私模式、
 * 配额满、被策略禁掉的浏览器上它会抛——那时候还显示"已保存"，用户关掉标签页
 * 才发现二十分钟的分配没了，而界面从头到尾都在说没事。
 */
export type DraftState =
  /** 还没有过改动 */
  | 'idle'
  /** 改了，等 500ms 的节流窗口 */
  | 'pending'
  /** 真的写进去了 */
  | 'saved'
  /** 写不进去：不挡编辑，但必须让人知道该导出备份 */
  | 'failed';

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
        probeDuringTraffic: plan.probeDuringTraffic,
        probePathMtu: plan.probePathMtu,
        forceTcpWindow: plan.forceTcpWindow,
        forceUdpBandwidth: plan.forceUdpBandwidth,
        rounds: plan.rounds,
        limitUdpByLinkSpeed: plan.limitUdpByLinkSpeed,
        globals: plan.globals,
        nicPolicies: plan.nicPolicies,
        masterConfig: plan.masterConfig,
        pendingImportTopology: plan.pendingImportTopology,
      }),
    );
    plan.draftState = 'saved';
    plan.draftAt = Date.now();
  } catch {
    // 写不进去不阻断编辑（那会让人连改都改不了），但**必须说出来**：
    // 这一刻起，关掉标签页就真的没了。
    plan.draftState = 'failed';
  }
}

let draftRestored = false;
/** 刚从 localStorage 恢复完：紧接着那一次 watcher 触发不算"用户改了东西"。 */
let restoredThisTick = false;

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
      probeDuringTraffic?: boolean;
      probePathMtu?: boolean;
      forceTcpWindow?: string;
      forceUdpBandwidth?: string;
      rounds?: number;
      limitUdpByLinkSpeed?: boolean;
      globals?: UiGlobals;
      nicPolicies?: UiNicPolicy[];
      masterConfig?: Record<string, unknown> | null;
      pendingImportTopology?: boolean;
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
    plan.pendingImportTopology = parsed.pendingImportTopology === true;
    plan.linkSets = Array.isArray(parsed.linkSets) ? parsed.linkSets : [];
    // 「全部/跨机/同机」已改成网口表上只影响显示的筛选；集合始终按全部候选生成。
    // 旧草稿里存的 cross/same 不再生效：roleKey 区分跨机与同机，两类网口从不进
    // 同一个集合，所以 'all' 只会多出没有分配的集合，实际执行的单元不变。
    plan.filter = 'all';
    if (typeof parsed.duration === 'number' && parsed.duration > 0) {
      plan.duration = parsed.duration;
    }
    plan.resume = parsed.resume === true;
    plan.screenshot = parsed.screenshot === true;
    plan.probeDuringTraffic = parsed.probeDuringTraffic === true;
    plan.probePathMtu = parsed.probePathMtu === true;
    plan.forceTcpWindow = typeof parsed.forceTcpWindow === 'string' ? parsed.forceTcpWindow : '';
    plan.forceUdpBandwidth =
      typeof parsed.forceUdpBandwidth === 'string' ? parsed.forceUdpBandwidth : '';
    plan.rounds = clampRounds(parsed.rounds);
    plan.limitUdpByLinkSpeed = parsed.limitUdpByLinkSpeed === true;
    plan.globals = parsed.globals ? normalizeGlobals(parsed.globals) : defaultGlobals();
    plan.nicPolicies = Array.isArray(parsed.nicPolicies) ? parsed.nicPolicies : [];
    plan.masterConfig = isPlainObject(parsed.masterConfig) ? parsed.masterConfig : null;
    draftRestored = true;
    // 恢复出来的这一份**本来就在盘上**，不是"待保存的改动"。不标一下的话，
    // 页面一打开就会闪一句「修改待保存」，而用户什么都没动过。
    restoredThisTick = true;
    plan.draftState = 'saved';
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
    plan.probeDuringTraffic,
    plan.probePathMtu,
    plan.forceTcpWindow,
    plan.forceUdpBandwidth,
    plan.rounds,
    plan.limitUdpByLinkSpeed,
    plan.globals,
    plan.nicPolicies,
    plan.masterConfig,
    plan.pendingImportTopology,
  ],
  () => {
    if (restoredThisTick) {
      restoredThisTick = false;
      return;
    }
    // 先说「待保存」：这是用户按下键到真正落盘之间那 500ms 的真实状态。
    plan.draftState = 'pending';
    if (saveTimer !== undefined) clearTimeout(saveTimer);
    saveTimer = setTimeout(saveDraft, 500);
  },
  { deep: true },
);

export function applyBootstrapDefaults(bootstrap: BootstrapOut): void {
  if (draftRestored) return;
  if (bootstrap.duration > 0) plan.duration = bootstrap.duration;
  plan.screenshot = bootstrap.screenshot;
  plan.probeDuringTraffic = bootstrap.probe_during_traffic === true;
  plan.probePathMtu = bootstrap.probe_path_mtu === true;
}

/**
 * 轮次的取值收敛。**和 Rust 侧的 `builder::MAX_ROUNDS` 是同一个上限**——
 * 界面放行 500、后端夹到 100 的话，预览说 500 轮、实际跑 100 轮。
 *
 * 上限存在的理由是防手滑：一次全量跑 11.5 小时，输错一位就是一个月。
 */
export const MAX_ROUNDS = 100;

export function clampRounds(value: unknown): number {
  const n = typeof value === 'number' && Number.isFinite(value) ? Math.floor(value) : 1;
  return Math.min(MAX_ROUNDS, Math.max(1, n));
}

let previewRequest = 0;

export function invalidatePreview(): void {
  ++previewRequest;
  plan.preview = null;
  plan.previewRequestFingerprint = '';
  plan.previewing = false;
  plan.previewError = '';
}

/** 项目不保存的本轮选项不能跟着切换项目；历史重跑则从归档显式恢复。 */
function resetRunOptions(): void {
  plan.resume = false;
  plan.screenshot = false;
  plan.probeDuringTraffic = false;
  plan.probePathMtu = false;
  plan.forceTcpWindow = '';
  plan.forceUdpBandwidth = '';
  plan.rounds = 1;
}

export function reset(): void {
  draftRestored = false;
  plan.ui = ensureDefaults(emptyPlan());
  plan.linkSets = [];
  plan.filter = 'all';
  plan.stale = [];
  plan.pendingImportTopology = false;
  invalidatePreview();
  plan.duration = 180;
  resetRunOptions();
  plan.limitUdpByLinkSpeed = false;
  plan.globals = defaultGlobals();
  plan.nicPolicies = [];
  plan.masterConfig = null;
}

export function restoreDefaultProject(): void {
  plan.pendingImportTopology = false;
  plan.ui = ensureDefaults(emptyPlan());
  plan.linkSets = [];
  plan.filter = 'all';
  plan.stale = [];
  invalidatePreview();
  resetRunOptions();
  reconcile();
}

export function buildRunRequest(): Record<string, unknown> {
  const globals = plan.globals;
  return {
    duration: plan.duration,
    resume: plan.resume,
    screenshot: plan.screenshot,
    probe_during_traffic: plan.probeDuringTraffic,
    probe_path_mtu: plan.probePathMtu,
    force_tcp_window: plan.forceTcpWindow,
    force_udp_bandwidth: plan.forceUdpBandwidth,
    rounds: plan.rounds,
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
  const requestId = ++previewRequest;
  const fingerprint = JSON.stringify(buildRunRequest());
  const current = () => requestId === previewRequest
    && fingerprint === JSON.stringify(buildRunRequest());
  plan.previewing = true;
  plan.previewError = '';
  try {
    const result = await api.post<PlanOut>('/api/plan', JSON.parse(fingerprint));
    if (!current()) return;
    plan.preview = result;
    plan.previewRequestFingerprint = fingerprint;
  } catch (error) {
    if (!current()) return;
    plan.preview = null;
    plan.previewRequestFingerprint = '';
    plan.previewError = errorMessage(error);
  } finally {
    // 配置已改但还没发新预览时也要结束忙碌态；旧请求不能结束新请求的忙碌态。
    if (requestId === previewRequest) plan.previewing = false;
  }
}

export const projectNotices = reactive({ items: [] as string[], error: '' });

export function importProject(text: string): boolean {
  const result = parseProject(text);
  projectNotices.items = result.notices;
  projectNotices.error = result.error ?? '';
  if (!result.ok || !result.plan) return false;
  invalidatePreview();
  resetRunOptions();
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
  plan.pendingImportTopology = true;
  plan.stale = [];
  reconcile();
  return true;
}

export function adoptRunRequest(raw: unknown, skipPassed: boolean): boolean {
  const snapshot = parseRunRequest(raw);
  if (!snapshot) return false;
  invalidatePreview();
  plan.duration = snapshot.duration;
  plan.screenshot = snapshot.screenshot;
  plan.probeDuringTraffic = snapshot.probeDuringTraffic;
  plan.probePathMtu = snapshot.probePathMtu;
  plan.forceTcpWindow = snapshot.forceTcpWindow;
  plan.forceUdpBandwidth = snapshot.forceUdpBandwidth;
  plan.rounds = snapshot.rounds;
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
