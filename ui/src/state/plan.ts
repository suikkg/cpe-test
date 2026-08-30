import { computed, reactive, watch } from 'vue';
import { api } from '../api/client';
import type { PlanOut } from '../api/dto';
import { pruneBindings, reconcileLinkSets, type ManagedLinkSet } from '../domain/grouping';
import { buildCandidates, type Candidate, type LinkFilter } from '../domain/pairs';
import { emptyPlan, ensureDefaults, type UiPlan } from '../domain/plan-build';
import { parseProject, serializeProject } from '../domain/project';
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

  /** `/api/plan` 的回包；null = 还没预览过 */
  preview: null as PlanOut | null,
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
      JSON.stringify({ ui: plan.ui, linkSets: plan.linkSets, filter: plan.filter }),
    );
  } catch {
    // 隐私模式 / 配额满：草稿存不下只是少一层保险，不该打断编辑。
  }
}

export function loadDraft(): boolean {
  try {
    const raw = localStorage.getItem(DRAFT_KEY);
    if (!raw) return false;
    const parsed = JSON.parse(raw) as {
      ui?: UiPlan;
      linkSets?: ManagedLinkSet[];
      filter?: LinkFilter;
    };
    if (!parsed.ui) return false;
    // **只做形状检查**，语义校验（引用完整性、端点存在性、参数范围）交给
    // `/api/plan` 的报错（ADR-11）：端点是否存在没有拓扑根本做不了，而草稿
    // 允许在未连接时恢复。
    if (!Array.isArray(parsed.ui.suites) || !Array.isArray(parsed.ui.bindings)) return false;
    plan.ui = ensureDefaults(parsed.ui);
    plan.linkSets = Array.isArray(parsed.linkSets) ? parsed.linkSets : [];
    plan.filter = parsed.filter ?? 'all';
    return true;
  } catch {
    return false;
  }
}

/** 草稿自动保存。debounce 到 500ms：编辑期每敲一个字都写一次没有意义。 */
let saveTimer: ReturnType<typeof setTimeout> | undefined;
watch(
  () => [plan.ui, plan.linkSets, plan.filter],
  () => {
    if (saveTimer !== undefined) clearTimeout(saveTimer);
    saveTimer = setTimeout(saveDraft, 500);
  },
  { deep: true },
);

export function reset(): void {
  plan.ui = ensureDefaults(emptyPlan());
  plan.linkSets = [];
  plan.filter = 'all';
  plan.stale = [];
  plan.preview = null;
  plan.previewing = false;
  plan.previewError = '';
}

/** 组装 `/api/plan` 与 `/api/run` 共用的请求体。 */
export function buildRunRequest(): Record<string, unknown> {
  return {
    duration: plan.duration,
    resume: plan.resume,
    screenshot: plan.screenshot,
    // 矩阵路径已按 ADR-13 退役，这里只发套件计划。
    pairs: [],
    nic_policies: [],
    ui_plan: plan.ui,
  };
}

/**
 * 预览：**唯一**的单元数量/耗时/resume 预判来源。
 *
 * 前端不复算这些——旧页那份浏览器里的估算与 Rust 的展开规则是两份实现。
 */
export async function preview(): Promise<void> {
  plan.previewing = true;
  plan.previewError = '';
  try {
    plan.preview = await api.post<PlanOut>('/api/plan', buildRunRequest());
  } catch (error) {
    plan.preview = null;
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

/** 导出当前计划。**不含任何口令**——项目文件是要传阅的。 */
export function exportProject(): string {
  return serializeProject(plan.ui);
}
