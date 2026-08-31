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

/**
 * 计划资源：用户的**意图**（UiPlan）+ 服务端算出的**可执行计划**（PlanOut）。
 *
 * 两者的分工是这一层最要紧的事：
 * - 意图归前端，它是用户的文档，可以随便改、可以离线编辑；
 * - **单元数量、耗时估算、resume 预判、plan_hash 一律以 `/api/plan` 回包为准。**
 *   旧页在浏览器里另算了一份数量估算，和 Rust 的展开规则构成两份实现——
 *   界面说 40 个单元、实际跑出 52 个，而两边"各自都没错"。
 */

const DRAFT_KEY = 'cpe_ui_plan_draft';

export const plan = reactive({
  /** 意图模型 */
  ui: ensureDefaults(emptyPlan()) as UiPlan,
  /** 链路集合（带 auto 标记；auto 的会被拓扑重建） */
  linkSets: [] as ManagedLinkSet[],
  /** 候选链路的筛选三态 */
  filter: 'all' as LinkFilter,
  /** 拓扑对账标出的失效引用 */
  stale: [] as Array<{ setId: string; pairId: string; src: string; dst: string }>,

  /** 执行区参数——**显式建模在这里**，见下面的注释 */
  duration: 180,
  resume: false,
  screenshot: false,
  /**
   * 按整条路径的可信上限裁剪 UDP `-b`。
   *
   * 界面默认关：控制台上填多少就发多少，超额灌包本来就是要看的场景之一。
   * 配置文件里的 `limit_udp_by_link_speed` **不回填到这里**，否则同一个勾选框
   * 在不同机器上含义不同（后端 `ui_request_base_config` 直接用请求里的值覆盖）。
   */
  limitUdpByLinkSpeed: false,

  /** 全局默认档位。内置默认见 `domain/globals.ts::defaultGlobals`；空格子 = 沿用主控 config.json。 */
  globals: defaultGlobals() as UiGlobals,
  /** 按网卡的门限 / UDP 带宽 / UDP 包长。只存真的填了东西的条目。 */
  nicPolicies: [] as UiNicPolicy[],

  /** `/api/plan` 的回包；null = 还没预览过 */
  preview: null as PlanOut | null,
  /** 预览成功时那一份请求的指纹；用来阻止修改后拿旧 plan_hash 开跑。 */
  previewRequestFingerprint: '',
  previewing: false,
  previewError: '',
});

/**
 * 执行区的默认参数组**属于 plan 状态**，不从别处偷读。
 *
 * 旧页的快速模式顶层档位是悄悄读高级矩阵的 `TCP_GROUPS[0]` / `UDP_GROUPS[0]`
 * 的——代码注释自己都写着「控件在 Advanced 标签页却影响 quick 提交」。
 * 结果是：用户在快速工作台**看不见的一张表**能改变它提交的内容。
 * 新结构里默认组就是执行区的表单字段，在这里显式建模。
 */

export const candidates = computed<Candidate[]>(() =>
  buildCandidates(masterNics.value, agentNics.value),
);

/** 计划里当前被引用的集合 id（用于「有绑定的空自动集合要保留」）。 */
const boundSetIds = computed(() => new Set(plan.ui.bindings.map((b) => b.link_set_id)));

/**
 * 按当前拓扑对账链路集合。
 *
 * 在「连上辅测机」「切换筛选」「导入项目」之后调。它是幂等的，多调几次无害。
 */
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

/**
 * 把意图草稿存进 localStorage。
 *
 * 旧页**零持久化**：F5 之后 MASTER/AGENT/PAIRS/LINK_SETS/SUITES/BINDINGS 全部
 * 归零，唯一补救是事先手工导出项目。而配一份 210 单元的计划要真实工时——
 * 误刷新一次就全没了。
 *
 * 只存 UiPlan：它**不含任何口令**，可以安全落地。连接信息（agent 地址/令牌）
 * 不进这里。
 */
function saveDraft(): void {
  try {
    localStorage.setItem(
      DRAFT_KEY,
      JSON.stringify({
        ui: plan.ui,
        linkSets: plan.linkSets,
        filter: plan.filter,
        // 执行区那几项也要存：一张 210 单元的计划配了半天，门限和档位是其中
        // 最费时的部分，误刷新一次全没了和计划本身丢了没有区别。
        duration: plan.duration,
        resume: plan.resume,
        screenshot: plan.screenshot,
        limitUdpByLinkSpeed: plan.limitUdpByLinkSpeed,
        globals: plan.globals,
        nicPolicies: plan.nicPolicies,
      }),
    );
  } catch {
    // 隐私模式 / 配额满：草稿存不下只是少一层保险，不该打断编辑。
  }
}

/**
 * 草稿是不是已经恢复过。
 *
 * `applyBootstrapDefaults` 要靠它决定让不让路：草稿是**用户自己的选择**，
 * 配置文件里的默认值不能盖掉它。
 */
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
    // **只做形状检查**，语义校验（引用完整性、端点存在性、参数范围）交给
    // `/api/plan` 的报错（ADR-11）：端点是否存在没有拓扑根本做不了，而草稿
    // 允许在未连接时恢复。
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
    // 草稿里存的是用户当时的选择，**整份采用**；只有 v6.1 之前的老草稿没有这一项，
    // 那时才落回内置默认。逐字段 merge 会把用户特意清空的格子又填回去。
    plan.globals = parsed.globals ? { ...emptyGlobals(), ...parsed.globals } : defaultGlobals();
    plan.nicPolicies = Array.isArray(parsed.nicPolicies) ? parsed.nicPolicies : [];
    draftRestored = true;
    return true;
  } catch {
    return false;
  }
}

/** 草稿自动保存。debounce 到 500ms：编辑期每敲一个字都写一次没有意义。 */
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

/**
 * 用主控 `config.json` 的值填执行区的**标量**默认（每单元时长、截图开关）。
 *
 * 只填标量，不填档位列表：档位一律留空、以 placeholder 显示当前生效值
 * （见 `views/run/GlobalDefaults.vue`）。回填档位会把配置里那份可能有意不成
 * 叉积的 `udp_profiles` 展成叉积，单元数悄悄变多。
 *
 * **草稿优先**：恢复过草稿就不动——那是用户上次自己填的，不该被配置文件盖掉。
 */
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

/** 只恢复计划编辑区，保留执行页的时长、截图和门限设置。 */
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

/**
 * 组装 `/api/plan` 与 `/api/run` 共用的请求体。
 *
 * **两个端点必须发同一份东西**，否则复核页看到的计划和实际跑的不是一回事——
 * `plan_hash` 那道闸拦的正是这个，而闸门只在两边的输入相同的时候才有意义。
 *
 * 这里以前只发 duration/resume/screenshot + ui_plan，`nic_policies` 恒为空数组，
 * 全局档位与 `limit_udp_by_link_speed` 一个都不发。后端这几条通路一直是通的
 * （`ui_request_base_config` 全都消费），所以表现是「界面上没有这些开关」，
 * 而不是「设了不生效」。
 */
export function buildRunRequest(): Record<string, unknown> {
  const globals = plan.globals;
  return {
    duration: plan.duration,
    resume: plan.resume,
    screenshot: plan.screenshot,
    limit_udp_by_link_speed: plan.limitUdpByLinkSpeed,
    // 空数组在后端就是「沿用配置」（`non_empty(请求, 配置)`），无条件发。
    tcp_windows: globals.tcp_windows,
    tcp_streams: globals.tcp_streams,
    udp_bandwidths: globals.udp_bandwidths,
    udp_lengths: globals.udp_lengths,
    udp_windows: globals.udp_windows,
    // `ping_count` 的 0 有定义：「沿用配置里的 ping.count」。
    ping_count: globals.ping_count,
    ping_payload_sizes: globals.ping_payload_sizes,
    // **`udp_streams` 没有「不填」这个取值。** 它在服务端是
    // `#[serde(default = "default_streams")]`（默认 1），而校验器要求 1..=32——
    // 发一个显式的 0 会被整份请求顶回来（「UDP 流数必须在 1..=32 之间」），
    // 于是界面上一个留空的输入框让预览彻底点不动。留空就**不发这个键**，
    // 让服务端用它自己的默认值。
    ...(globals.udp_streams > 0 ? { udp_streams: globals.udp_streams } : {}),
    // 矩阵路径已按 ADR-13 退役，这里只发套件计划。
    pairs: [],
    nic_policies: activeNicPolicies(plan.nicPolicies),
    ui_plan: plan.ui,
  };
}

/** 当前表单是否仍与最后一次服务端预览完全一致。 */
export function previewIsCurrent(): boolean {
  return (
    !!plan.preview?.plan_hash &&
    plan.previewRequestFingerprint === JSON.stringify(buildRunRequest())
  );
}

/**
 * 预览：**唯一**的单元数量/耗时/resume 预判来源。
 *
 * 前端不复算这些——旧页那份浏览器里的估算与 Rust 的展开规则是两份实现。
 */
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


// ---------------------------------------------------------------------------
// 项目文件的导入导出
// ---------------------------------------------------------------------------

/** 导入结果里要给用户看的提示（比如「抹掉了废弃的 mode 字段」）。 */
export const projectNotices = reactive({ items: [] as string[], error: '' });

/**
 * 从一段文本导入项目。
 *
 * 只做形状检查——语义（引用完整性、端点存在性、参数范围）交给 `/api/plan`
 * 的报错（ADR-11）。导入成功后立刻对账一次拓扑：连着的话集合会按当前网卡
 * 补齐，没连的话原样保留。
 */
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

/**
 * 把某一轮历史运行的请求装载回编辑态（「重新执行」的前半）。
 *
 * **只装载，不开跑。** 后半必须是人再走一次「预览」：`plan_hash` 是「界面上
 * 确认的东西 == 实际跑的东西」唯一的强制点，而隔了一夜网口拓扑可能已经变了，
 * 老计划里的端点未必还在。直接开跑要么被服务端拒，要么在没人复核的情况下
 * 少跑几条链路——后者才是真正危险的那种。
 *
 * `skipPassed` 就是 RESUME：跳过 24 小时内已有正式 PASS 的单元。它是复测最常
 * 用的选项，所以在「重新执行」入口上直接给，而不是让人跑去执行页再找一遍。
 */
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
  // 上一轮的预览结果与这一份计划无关，留着会让「执行」页看起来已经复核过。
  plan.preview = null;
  plan.previewRequestFingerprint = '';
  plan.previewError = '';
  return true;
}

/** 导出当前计划。**不含任何口令**——项目文件是要传阅的。 */
export function exportProject(): string {
  return serializeProject(plan.ui);
}
