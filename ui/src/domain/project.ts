import { ensureDefaults, type UiPlan, type UiRecipe } from './plan-build';

/**
 * 项目文件（`cpe-ui-project.json`）的读写。纯函数。
 *
 * # 只做形状检查，不做语义校验（ADR-11）
 *
 * 引用完整性、端点存在性、参数范围一律交给 `/api/plan` 的报错。理由不是省事：
 *
 * - 语义校验里最重的一类——**端点是否存在**——没有拓扑根本做不了，而项目
 *   允许在**未连接时导入**（用户在飞机上改计划是真实场景）。所以语义错误
 *   本来就只能在首次预览时暴露。
 * - Rust 侧的 `validate_ui_plan` 已经有约 326 行做这件事，而且报错文案是面向
 *   用户打磨过的。旧页那约 200 行手写 JSON 校验和它逐字段重复、无共享 schema；
 *   把 JS 那份搬进 Vitest 等于**把重复固化成制度**。
 *
 * 这里只回答一个问题：这是不是一份能被解析的项目文件。
 */

export const PROJECT_VERSION = 1;

export interface ProjectFile {
  project_version: number;
  ui_plan: UiPlan;
  settings?: Record<string, unknown>;
  nic_policies?: unknown[];
  topology_fingerprint?: string;
}

export interface ParseResult {
  ok: boolean;
  /** 解析成功时的计划 */
  plan?: UiPlan;
  /** 形状问题；语义问题不在这里，等预览 */
  error?: string;
  /** 读入时被丢掉/修正的东西，要让用户看见 */
  notices: string[];
}

/**
 * 读入一份项目文件。
 *
 * 形状检查只有四条：能不能 JSON.parse、`project_version` 认不认、`ui_plan`
 * 在不在、几个顶层容器是不是数组。再往下就是语义了。
 */
export function parseProject(text: string): ParseResult {
  const notices: string[] = [];
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch (error) {
    return {
      ok: false,
      error: `这不是一份能解析的 JSON：${error instanceof Error ? error.message : error}`,
      notices,
    };
  }
  // 数组也是 `typeof === 'object'`——不显式排掉的话，一份 `[]` 会一路走到
  // 「缺少 project_version」，报错指向的位置就偏了。
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
    return { ok: false, error: '项目文件的顶层必须是一个对象', notices };
  }
  const file = raw as Partial<ProjectFile>;

  if (typeof file.project_version !== 'number') {
    return { ok: false, error: '缺少 project_version', notices };
  }
  if (file.project_version > PROJECT_VERSION) {
    return {
      ok: false,
      error: `项目文件版本 ${file.project_version} 比当前程序支持的 ${PROJECT_VERSION} 新，请升级 cpe_test`,
      notices,
    };
  }
  const plan = file.ui_plan;
  if (typeof plan !== 'object' || plan === null) {
    return { ok: false, error: '缺少 ui_plan', notices };
  }
  for (const key of ['link_sets', 'suites', 'bindings'] as const) {
    if (!Array.isArray(plan[key])) {
      return { ok: false, error: `ui_plan.${key} 必须是数组`, notices };
    }
  }
  if (typeof plan.recipes !== 'object' || plan.recipes === null) {
    return { ok: false, error: 'ui_plan.recipes 必须是对象', notices };
  }

  return { ok: true, plan: ensureDefaults(stripDeadFields(plan, notices)), notices };
}

/**
 * 丢掉已经废弃的字段。
 *
 * 目前只有一个：`UiRecipe.mode`。它是死字段——校验器过去只准 `fixed`/`scan`，
 * 而计划编译器从头到尾不读它，两个取值产出**同一份计划**。用户以为 `fixed`
 * 把档位钉死成一档，实际三条轴全展开、耗时三倍。
 *
 * 服务端现在会明确拒绝非空 `mode`（ADR-16）。而那个字段是**旧版界面自动写进去
 * 的**，用户一个字都没打过——让他为工具自己填的东西去手改 JSON 不合理。
 * 所以在导入时替他抹掉，并说一声。
 */
function stripDeadFields(plan: UiPlan, notices: string[]): UiPlan {
  let stripped = 0;
  const clean = (recipes: UiRecipe[] | undefined): UiRecipe[] =>
    (recipes ?? []).map((recipe) => {
      if (recipe.mode === undefined || recipe.mode === '') return recipe;
      stripped += 1;
      const { mode: _mode, ...rest } = recipe;
      return rest;
    });

  const next: UiPlan = {
    ...plan,
    recipes: {
      tcp: clean(plan.recipes?.tcp),
      udp: clean(plan.recipes?.udp),
      ping: clean(plan.recipes?.ping),
    },
  };
  if (stripped > 0) {
    notices.push(
      `已移除 ${stripped} 处废弃的 mode 字段：档位由轴的取值个数决定（单值=钉死、多值=扫描），` +
        `mode 从来没有被计划编译器读过。这个字段是旧版界面自动写进去的，不影响你的计划。`,
    );
  }
  return next;
}

/** 导出一份项目文件。**不含任何口令**——它是要传阅的。 */
export function buildProject(plan: UiPlan, topologyFingerprint?: string): ProjectFile {
  return {
    project_version: PROJECT_VERSION,
    ui_plan: plan,
    ...(topologyFingerprint ? { topology_fingerprint: topologyFingerprint } : {}),
  };
}

export function serializeProject(plan: UiPlan, topologyFingerprint?: string): string {
  return `${JSON.stringify(buildProject(plan, topologyFingerprint), null, 2)}\n`;
}
