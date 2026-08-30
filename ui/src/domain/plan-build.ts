/**
 * 测试计划的**意图模型**（UiPlan）与它到 `RunRequest` 的组装。纯函数。
 *
 * 分层上这是「用户想跑什么」，不是「实际会跑出几个单元」——后者由 Rust 的
 * `/api/plan` 回包说了算，前端**不复算**。旧页在浏览器里另写了一份数量估算
 * （`qcandidatePairs` 一族），和 Rust 的 `enumerate_pairs`/`PlanOut` 构成两份
 * 实现、两套规则。这里只管交互态。
 */

// ---------------------------------------------------------------------------
// UiPlan（与 Rust `webui/model.rs` 的 UiPlan 系列一一对应）
// ---------------------------------------------------------------------------

export interface UiPairRef {
  id: string;
  src: string;
  dst: string;
}

export interface UiLinkSet {
  id: string;
  name: string;
  pair_refs: UiPairRef[];
}

export interface UiRecipeProfile {
  window?: string;
  bandwidth?: string;
  length?: string;
  streams?: number;
}

export interface UiRecipe {
  id: string;
  name: string;
  profiles: UiRecipeProfile[];
  tcp_windows?: string[];
  tcp_streams?: number[];
  bandwidths?: string[];
  lengths?: string[];
  windows?: string[];
  udp_streams?: number[];
  /**
   * **不再写这个字段。**
   *
   * `mode` 是死字段：校验器过去只准 `fixed`/`scan`，而计划编译器从头到尾不读
   * 它——两个取值产出的是同一份计划。用户以为 `fixed` 把档位钉死成一档，实际
   * 三条轴全展开、耗时三倍。服务端现在会明确拒绝非空 `mode`（ADR-16），
   * 类型上保留只是为了能读懂旧项目文件。
   */
  mode?: string;
}

export interface UiRecipes {
  tcp: UiRecipe[];
  udp: UiRecipe[];
  ping: UiRecipe[];
}

export type UiProtocol = 'tcp' | 'udp' | 'ping';

export interface UiTask {
  id: string;
  name: string;
  protocol: UiProtocol;
  /** 规范方向：ab / ba / bidir（both 会被展开成 ab + ba） */
  directions: string[];
  ip: string[];
  recipe_ids: string[];
  duration?: number;
  ping_count?: number;
  ping_payload_sizes?: number[];
  rx_target_bidir_ab?: string;
  rx_target_bidir_ba?: string;
}

export interface UiSuite {
  id: string;
  name: string;
  note: string;
  execution: string;
  order: string[];
  tasks: UiTask[];
}

export interface UiBinding {
  id: string;
  link_set_id: string;
  suite_id: string;
  pair_ids: string[];
  mode: string;
}

export interface UiPlan {
  ui_plan_version: number;
  link_sets: UiLinkSet[];
  recipes: UiRecipes;
  suites: UiSuite[];
  bindings: UiBinding[];
}

// ---------------------------------------------------------------------------
// 方向词汇表
// ---------------------------------------------------------------------------

/**
 * 方向的规范化。与后端 `canonical_ui_direction` 保持**同一套词义**。
 *
 * `both` 与 `bidir` 是两件不同的事：`both` 是两条独立的 A→B / B→A 单向腿，
 * `bidir` 是同一个双向并发单元。旧项目里两者都可能出现，把 `both` 显示成
 * 「双向」而在保存时悄悄改成 `bidir`，会**改变执行语义**——半双工介质上双向
 * 并发时两个方向抢同一段介质时间，跑出来的数完全不是一回事。
 */
export function canonicalDirection(raw: string): string | null {
  switch (String(raw ?? '').trim().toLowerCase()) {
    case 'ab':
    case 'a->b':
    case 'a>b':
    case 'a_to_b':
      return 'ab';
    case 'ba':
    case 'b->a':
    case 'b>a':
    case 'b_to_a':
      return 'ba';
    case 'bidir':
    case 'both-way':
    case 'a<->b':
    case '双向':
      return 'bidir';
    case 'both':
      return 'both';
    default:
      return null;
  }
}

export function directionLabel(raw: string): string {
  switch (canonicalDirection(raw)) {
    case 'both':
      return 'A→B、B→A（分开执行）';
    case 'ab':
      return 'A→B';
    case 'ba':
      return 'B→A';
    case 'bidir':
      return '双向并发';
    default:
      return String(raw ?? '').trim() || '未选方向';
  }
}

export function taskDirectionsLabel(task: UiTask): string {
  const labels: string[] = [];
  for (const raw of task.directions ?? []) {
    const label = directionLabel(raw);
    if (!labels.includes(label)) labels.push(label);
  }
  return labels.length ? labels.join('、') : '未选方向';
}

// ---------------------------------------------------------------------------
// 出厂默认
//
// 逐字搬自 v4.6.0 手写页的 `tcpRecipeDefault` / `udpRecipeDefault` /
// `baselineSuite` / `ensureQuickDefaults`。**值不许改**：`-b 2500m -l 14k
// -w 256m` 这组基线是按 Windows 调的，Rust 侧另有测试钉着它能过校验。
//
// 注意 `-w 256m` 在 Linux/macOS 上必然报错（被 kern.ipc.maxsockbuf /
// net.core.wmem_max 夹住），那是**预期行为**，不是要修的 bug——调小它等于在
// 没人注意的情况下削弱 Windows 上的基线。
// ---------------------------------------------------------------------------

export function defaultTcpRecipe(): UiRecipe {
  return {
    id: 'recipe-tcp-default',
    name: '默认 TCP',
    profiles: [{ window: '4m', streams: 10 }],
  };
}

export function defaultUdpRecipe(): UiRecipe {
  // 用轴字段而不是 profiles：「编辑」打开就是这四格填好的样子。
  return {
    id: 'recipe-udp-default',
    name: '默认 UDP',
    profiles: [],
    bandwidths: ['2500m'],
    lengths: ['14k'],
    windows: ['256m'],
    udp_streams: [1],
  };
}

export function baselineSuite(): UiSuite {
  return {
    id: 'suite-baseline',
    name: '基线 TCP+UDP',
    note: '',
    execution: 'sequential',
    order: ['task-tcp', 'task-udp'],
    tasks: [
      {
        id: 'task-tcp',
        name: 'TCP',
        protocol: 'tcp',
        directions: ['ab', 'ba'],
        ip: ['v4'],
        recipe_ids: ['recipe-tcp-default'],
        rx_target_bidir_ab: '',
        rx_target_bidir_ba: '',
      },
      {
        id: 'task-udp',
        name: 'UDP',
        protocol: 'udp',
        directions: ['ab', 'ba'],
        ip: ['v4'],
        recipe_ids: ['recipe-udp-default'],
        rx_target_bidir_ab: '',
        rx_target_bidir_ba: '',
      },
    ],
  };
}

export function emptyPlan(): UiPlan {
  return {
    ui_plan_version: 1,
    link_sets: [],
    recipes: { tcp: [], udp: [], ping: [] },
    suites: [],
    bindings: [],
  };
}

/** 补齐出厂默认：只在**空**的时候补，不覆盖用户已有的东西。 */
export function ensureDefaults(plan: UiPlan): UiPlan {
  const next: UiPlan = {
    ...plan,
    recipes: {
      tcp: [...(plan.recipes?.tcp ?? [])],
      udp: [...(plan.recipes?.udp ?? [])],
      ping: [...(plan.recipes?.ping ?? [])],
    },
    suites: [...(plan.suites ?? [])],
  };
  if (next.recipes.tcp.length === 0) next.recipes.tcp.push(defaultTcpRecipe());
  if (next.recipes.udp.length === 0) next.recipes.udp.push(defaultUdpRecipe());
  if (next.suites.length === 0) next.suites.push(baselineSuite());
  return next;
}

// ---------------------------------------------------------------------------
// 编辑操作
// ---------------------------------------------------------------------------

/**
 * 删掉一条配置，**并清掉所有任务上对它的引用**。
 *
 * 承接 `the_recipe_card_can_be_deleted_and_edited_in_place` 的义务（PLAN §7.3）。
 * 原始理由（逐字搬运）：配置卡片过去只有「编辑」，点击委派里根本没有删除
 * 分支——加错一条配置就再也去不掉，只能重建整个项目。
 *
 * 只删卡片不清引用会更糟：任务指着一个不存在的 recipe_id，预览时才报错，
 * 而报错指向的是任务不是配置。
 */
export function deleteRecipe(plan: UiPlan, protocol: UiProtocol, recipeId: string): UiPlan {
  return {
    ...plan,
    recipes: {
      ...plan.recipes,
      [protocol]: plan.recipes[protocol].filter((recipe) => recipe.id !== recipeId),
    },
    suites: plan.suites.map((suite) => ({
      ...suite,
      tasks: suite.tasks.map((task) => ({
        ...task,
        recipe_ids: task.recipe_ids.filter((id) => id !== recipeId),
      })),
    })),
  };
}

/**
 * 分配表的**整列开关**：把一个套件一次性分配给全部链路集合，或一次性撤掉。
 *
 * 承接 `the_assignment_table_can_toggle_a_whole_suite_column` 的义务。
 * 原始理由（逐字搬运）：套件多起来之后，「所有链路都跑这个套件」原本得逐格点。
 *
 * 语义是**三态归一**：只要有任何一个集合还没绑上，就全绑上；全绑上了才是全撤。
 * 这样连点两下的结果可预测——不会出现「点一下勾了一半」。
 */
export function toggleSuiteColumn(plan: UiPlan, suiteId: string): UiPlan {
  const setIds = plan.link_sets.map((set) => set.id);
  const bound = new Set(
    plan.bindings.filter((b) => b.suite_id === suiteId).map((b) => b.link_set_id),
  );
  const allBound = setIds.length > 0 && setIds.every((id) => bound.has(id));

  if (allBound) {
    return {
      ...plan,
      bindings: plan.bindings.filter((b) => b.suite_id !== suiteId),
    };
  }
  const missing = setIds.filter((id) => !bound.has(id));
  return {
    ...plan,
    bindings: [
      ...plan.bindings,
      ...missing.map((linkSetId) => ({
        id: `binding-${linkSetId}-${suiteId}`,
        link_set_id: linkSetId,
        suite_id: suiteId,
        pair_ids: [],
        // `append` 没有定义好的合并语义，服务端会拒绝；只用 replace。
        mode: 'replace',
      })),
    ],
  };
}

export function toggleBinding(plan: UiPlan, linkSetId: string, suiteId: string): UiPlan {
  const existing = plan.bindings.find(
    (b) => b.link_set_id === linkSetId && b.suite_id === suiteId,
  );
  if (existing) {
    return { ...plan, bindings: plan.bindings.filter((b) => b !== existing) };
  }
  return {
    ...plan,
    bindings: [
      ...plan.bindings,
      {
        id: `binding-${linkSetId}-${suiteId}`,
        link_set_id: linkSetId,
        suite_id: suiteId,
        pair_ids: [],
        mode: 'replace',
      },
    ],
  };
}

export function isBound(plan: UiPlan, linkSetId: string, suiteId: string): boolean {
  return plan.bindings.some((b) => b.link_set_id === linkSetId && b.suite_id === suiteId);
}
