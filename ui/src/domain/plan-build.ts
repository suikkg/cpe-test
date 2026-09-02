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
  /**
   * 双向并发下**两端 RX 合计**的门限。
   *
   * 填了它，这个任务的双向单元只按 `AB 接收端 RX + BA 接收端 RX >= 门限`
   * 判一次，两条腿各自只测量。Wi-Fi↔Wi-Fi 抢的是同一段空口时间，要求两个
   * 方向各达到一半没有物理依据。
   */
  rx_target_bidir_total?: string;
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
    // 出厂配置也用可编辑轴。只有导入的历史固定组合才需要先“转成可编辑档位”。
    profiles: [],
    tcp_windows: ['4m'],
    tcp_streams: [10],
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
        ip: ['v4', 'v6'],
        recipe_ids: ['recipe-tcp-default'],
        rx_target_bidir_ab: '',
        rx_target_bidir_ba: '',
        rx_target_bidir_total: '',
      },
      {
        id: 'task-udp',
        name: 'UDP',
        protocol: 'udp',
        directions: ['ab', 'ba'],
        ip: ['v4', 'v6'],
        recipe_ids: ['recipe-udp-default'],
        rx_target_bidir_ab: '',
        rx_target_bidir_ba: '',
        rx_target_bidir_total: '',
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

// ---------------------------------------------------------------------------
// 套件 / 任务 / 配置的增删改
//
// 全是**纯函数**：吃一份 UiPlan、吐一份新的 UiPlan。这一层历史上根本不存在——
// 「测试计划」页只能看，套件、任务、TCP/UDP 配置一个都不能改，于是唯一能改计划
// 的办法是在别处编好一份项目文件导进来。而这些操作里真正容易错的部分（删了套件
// 要清绑定、删了配置要清任务上的引用、改了协议要作废旧配置引用、id 不能重复）
// 全是纯逻辑，正好该待在一个没有响应式、没有 DOM、没有网络的地方。
// ---------------------------------------------------------------------------

/**
 * 生成一个没被占用的 id。
 *
 * **不用随机数、不用时间戳**：id 会进项目文件、进 `origin` 溯源串、进报告的
 * 分组键，随机 id 会让「同一份计划导出两次 diff 全红」。按序号找第一个空位，
 * 同一份计划的同一串操作产出同一批 id，也让这些函数可以被直接断言。
 */
export function uniqueId(prefix: string, taken: Iterable<string>): string {
  const used = new Set(taken);
  for (let i = 1; ; i += 1) {
    const candidate = `${prefix}-${i}`;
    if (!used.has(candidate)) return candidate;
  }
}

function allRecipeIds(plan: UiPlan): string[] {
  // **配置 id 在三个协议桶之间是同一个命名空间**（服务端 `validate_ui_plan`
  // 用一个 HashSet 校验全部三桶）。分桶取 id 会造出一个 TCP 和 UDP 撞名的计划，
  // 而那份计划要到预览时才被服务端拒。
  return [...plan.recipes.tcp, ...plan.recipes.udp, ...plan.recipes.ping].map((r) => r.id);
}

function allTaskIds(plan: UiPlan): string[] {
  return plan.suites.flatMap((suite) => suite.tasks.map((task) => task.id));
}

/** 把 `order` 对齐到 `tasks` 的实际顺序。服务端允许缺项，但缺项等于顺序失去意义。 */
function syncOrder(suite: UiSuite): UiSuite {
  return { ...suite, order: suite.tasks.map((task) => task.id) };
}

function mapSuite(plan: UiPlan, suiteId: string, fn: (suite: UiSuite) => UiSuite): UiPlan {
  return {
    ...plan,
    suites: plan.suites.map((suite) => (suite.id === suiteId ? syncOrder(fn(suite)) : suite)),
  };
}

function mapTask(
  plan: UiPlan,
  suiteId: string,
  taskId: string,
  fn: (task: UiTask) => UiTask,
): UiPlan {
  return mapSuite(plan, suiteId, (suite) => ({
    ...suite,
    tasks: suite.tasks.map((task) => (task.id === taskId ? fn(task) : task)),
  }));
}

// ---- 套件 ----

export function addSuite(plan: UiPlan, name = '新套件'): UiPlan {
  const id = uniqueId('suite', plan.suites.map((suite) => suite.id));
  const suite: UiSuite = {
    id,
    name,
    note: '',
    execution: 'sequential',
    order: [],
    // 服务端要求每个套件至少有一个任务，空套件会在预览时被整份拒掉。
    // 与其让人先建一个必然报错的东西，不如开局就给一条 TCP。
    tasks: [newTask(plan, 'tcp')],
  };
  return { ...plan, suites: [...plan.suites, syncOrder(suite)] };
}

/**
 * 删套件，**同时清掉指向它的绑定**。
 *
 * 只删卡片不清绑定，分配表会留下一格指向不存在的套件——服务端 `pruneBindings`
 * 那一侧会忽略它，于是表面上"没事"，而用户看到的是一个再也点不掉的勾。
 */
export function removeSuite(plan: UiPlan, suiteId: string): UiPlan {
  return {
    ...plan,
    suites: plan.suites.filter((suite) => suite.id !== suiteId),
    bindings: plan.bindings.filter((binding) => binding.suite_id !== suiteId),
  };
}

/**
 * 复制一个套件（含全部任务），**不复制绑定**。
 *
 * 这是「按链路集合给不同门限」唯一可行的做法：`rx_target_bidir_*` 挂在
 * 任务上，一个套件绑给三个链路集合时三边共用同一组数字。要给某条链路单独的
 * 门限，就复制一份套件、改数字、只绑给那条链路。
 */
export function duplicateSuite(plan: UiPlan, suiteId: string): UiPlan {
  const source = plan.suites.find((suite) => suite.id === suiteId);
  if (!source) return plan;
  const id = uniqueId('suite', plan.suites.map((suite) => suite.id));
  const taken = new Set(allTaskIds(plan));
  const tasks = source.tasks.map((task) => {
    const taskId = uniqueId('task', taken);
    taken.add(taskId);
    return { ...task, id: taskId };
  });
  const copy: UiSuite = { ...source, id, name: `${source.name} 副本`, tasks };
  return { ...plan, suites: [...plan.suites, syncOrder(copy)] };
}

export function updateSuite(plan: UiPlan, suiteId: string, patch: Partial<UiSuite>): UiPlan {
  return mapSuite(plan, suiteId, (suite) => ({ ...suite, ...patch, id: suite.id }));
}

// ---- 任务 ----

function newTask(plan: UiPlan, protocol: UiProtocol): UiTask {
  const id = uniqueId('task', allTaskIds(plan));
  return {
    id,
    name: protocol.toUpperCase(),
    protocol,
    // 默认两个单向而不是 bidir：`both`/两条单向腿与「双向并发」是两件不同的事，
    // 半双工介质上双向并发时两个方向抢同一段介质时间，跑出来的数完全不是一回事。
    directions: ['ab', 'ba'],
    ip: ['v4', 'v6'],
    recipe_ids: [],
    rx_target_bidir_ab: '',
    rx_target_bidir_ba: '',
    rx_target_bidir_total: '',
  };
}

export function addTask(plan: UiPlan, suiteId: string, protocol: UiProtocol): UiPlan {
  const task = newTask(plan, protocol);
  return mapSuite(plan, suiteId, (suite) => ({ ...suite, tasks: [...suite.tasks, task] }));
}

/** 删任务。**套件里的最后一条不许删**：服务端会把没有任务的套件整份拒掉。 */
export function removeTask(plan: UiPlan, suiteId: string, taskId: string): UiPlan {
  const suite = plan.suites.find((item) => item.id === suiteId);
  if (!suite || suite.tasks.length <= 1) return plan;
  return mapSuite(plan, suiteId, (current) => ({
    ...current,
    tasks: current.tasks.filter((task) => task.id !== taskId),
  }));
}

export function updateTask(
  plan: UiPlan,
  suiteId: string,
  taskId: string,
  patch: Partial<UiTask>,
): UiPlan {
  return mapTask(plan, suiteId, taskId, (task) => ({ ...task, ...patch, id: task.id }));
}

/**
 * 换协议。**必须同时作废配置引用**：TCP 配置的 id 在 UDP 任务上是「引用了不存在
 * 的 udp 配置」，服务端会拒；而在界面上那一格看起来只是没勾中任何配置。
 *
 * PING 更硬：服务端明确拒绝带配置引用的 ping 任务（`UiRecipe` 没有 ping 语义，
 * 留着引用会让人以为它可配置，而参数其实被静默忽略）。
 */
export function setTaskProtocol(
  plan: UiPlan,
  suiteId: string,
  taskId: string,
  protocol: UiProtocol,
): UiPlan {
  return mapTask(plan, suiteId, taskId, (task) =>
    task.protocol === protocol
      ? task
      : {
          ...task,
          protocol,
          recipe_ids: [],
          ...(protocol === 'ping'
            ? {
                rx_target_bidir_ab: '',
                rx_target_bidir_ba: '',
                rx_target_bidir_total: '',
              }
            : {}),
          name: task.name === task.protocol.toUpperCase() ? protocol.toUpperCase() : task.name,
        },
  );
}

/** 任务在套件里的顺序就是执行顺序（`execution: sequential`）。 */
export function moveTask(plan: UiPlan, suiteId: string, taskId: string, delta: number): UiPlan {
  return mapSuite(plan, suiteId, (suite) => {
    const from = suite.tasks.findIndex((task) => task.id === taskId);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= suite.tasks.length) return suite;
    const tasks = [...suite.tasks];
    const [moved] = tasks.splice(from, 1);
    tasks.splice(to, 0, moved);
    return { ...suite, tasks };
  });
}

function toggleInList(list: string[], value: string): string[] {
  return list.includes(value) ? list.filter((item) => item !== value) : [...list, value];
}

/**
 * 勾选方向。留空是允许的中间态——服务端会在预览时说「方向无效」，
 * 前端不再写第二份校验（ADR-11）。
 */
export function toggleTaskDirection(
  plan: UiPlan,
  suiteId: string,
  taskId: string,
  direction: string,
): UiPlan {
  return mapTask(plan, suiteId, taskId, (task) => {
    const directions = toggleInList(task.directions, direction);
    // 取消「双向并发」时把双向门限一起清掉：服务端会拒绝「填了双向门限却没选
    // 双向」的任务，而那两个输入框这时已经从界面上消失了，人根本看不见是哪里错。
    const stillBidir = directions.some((item) => canonicalDirection(item) === 'bidir');
    return stillBidir
      ? { ...task, directions }
      : {
          ...task,
          directions,
          rx_target_bidir_ab: '',
          rx_target_bidir_ba: '',
          rx_target_bidir_total: '',
        };
  });
}

export function toggleTaskIp(plan: UiPlan, suiteId: string, taskId: string, ip: string): UiPlan {
  return mapTask(plan, suiteId, taskId, (task) => ({ ...task, ip: toggleInList(task.ip, ip) }));
}

export function toggleTaskRecipe(
  plan: UiPlan,
  suiteId: string,
  taskId: string,
  recipeId: string,
): UiPlan {
  return mapTask(plan, suiteId, taskId, (task) => ({
    ...task,
    recipe_ids: toggleInList(task.recipe_ids, recipeId),
  }));
}

export function taskUsesBidir(task: UiTask): boolean {
  return (
    task.protocol !== 'ping' &&
    (task.directions ?? []).some((raw) => canonicalDirection(raw) === 'bidir')
  );
}

// ---- 配置（recipe） ----

/**
 * 新建一条配置。
 *
 * **只用轴字段，不用 `profiles`**：轴字段（`tcp_windows` × `tcp_streams`、
 * `bandwidths` × `lengths` × `windows` × `udp_streams`）在界面上就是几个能直接
 * 编辑的格子，而 `profiles` 是一份已经展开的组合列表，编辑器改不动它。
 * 两种服务端都收，读旧项目时 `profiles` 原样保留。
 *
 * `mode` **不写**：它是死字段，校验器只准 `fixed`/`scan` 而编译器从头到尾不读，
 * 两个取值产出同一份计划；服务端现在会明确拒绝非空 `mode`（ADR-16）。
 */
export function addRecipe(plan: UiPlan, protocol: 'tcp' | 'udp'): UiPlan {
  const id = uniqueId(`recipe-${protocol}`, allRecipeIds(plan));
  const recipe: UiRecipe =
    protocol === 'tcp'
      ? { id, name: '新 TCP 配置', profiles: [], tcp_windows: [], tcp_streams: [1] }
      : {
          id,
          name: '新 UDP 配置',
          profiles: [],
          bandwidths: ['1000m'],
          lengths: [],
          windows: [],
          udp_streams: [1],
        };
  return { ...plan, recipes: { ...plan.recipes, [protocol]: [...plan.recipes[protocol], recipe] } };
}

export function updateRecipe(
  plan: UiPlan,
  protocol: UiProtocol,
  recipeId: string,
  patch: Partial<UiRecipe>,
): UiPlan {
  return {
    ...plan,
    recipes: {
      ...plan.recipes,
      [protocol]: plan.recipes[protocol].map((recipe) =>
        recipe.id === recipeId ? { ...recipe, ...patch, id: recipe.id } : recipe,
      ),
    },
  };
}

/**
 * 这条配置的档位能不能直接编辑。
 *
 * 服务端 `recipe_tcp_profiles` / `recipe_udp_profiles` 在 `profiles` 非空时**直接
 * 返回**，压根不看轴字段。所以对一条带 `profiles` 的配置提供轴输入框，是在给
 * 用户一个改了不生效的界面——最坏的一种：他会以为自己把带宽调到了 1000m，
 * 而实际跑的仍然是 profiles 里那份。
 */
export function recipeIsAxisEditable(recipe: UiRecipe): boolean {
  return (recipe.profiles?.length ?? 0) === 0;
}

/**
 * 摊成轴字段是否**无损**。
 *
 * `profiles` 是一份显式的组合列表，可以有意不成叉积（比如只有 `2500m/14k` 和
 * `1000m/1k` 两条，而不是 2×2 四条）。摊成轴之后必然变成叉积，单元数会变多。
 * 只有一条时两者等价，可以静默转换；多条时必须先把这句话说给人听。
 */
export function axisExpansionIsExact(recipe: UiRecipe): boolean {
  return (recipe.profiles?.length ?? 0) <= 1;
}

function distinct<T>(values: Array<T | undefined | null>): T[] {
  const out: T[] = [];
  for (const value of values) {
    if (value === undefined || value === null || value === '') continue;
    if (!out.includes(value)) out.push(value);
  }
  return out;
}

/** 把 `profiles` 摊成轴字段并清空它，让这条配置变得可编辑。 */
export function profilesToAxes(recipe: UiRecipe, protocol: 'tcp' | 'udp'): UiRecipe {
  const profiles = recipe.profiles ?? [];
  if (protocol === 'tcp') {
    return {
      ...recipe,
      profiles: [],
      tcp_windows: distinct([...(recipe.tcp_windows ?? []), ...profiles.map((p) => p.window)]),
      tcp_streams: distinct([...(recipe.tcp_streams ?? []), ...profiles.map((p) => p.streams)]),
    };
  }
  return {
    ...recipe,
    profiles: [],
    bandwidths: distinct([...(recipe.bandwidths ?? []), ...profiles.map((p) => p.bandwidth)]),
    lengths: distinct([...(recipe.lengths ?? []), ...profiles.map((p) => p.length)]),
    windows: distinct([...(recipe.windows ?? []), ...profiles.map((p) => p.window)]),
    udp_streams: distinct([...(recipe.udp_streams ?? []), ...profiles.map((p) => p.streams)]),
  };
}

/** 一条配置在界面上的一句话摘要（收起时也要看得出它是什么）。 */
export function recipeSummary(recipe: UiRecipe, protocol: UiProtocol): string {
  const parts: string[] = [];
  const profiles = recipe.profiles ?? [];
  if (profiles.length > 0) {
    parts.push(`${profiles.length} 条固定组合`);
  } else if (protocol === 'tcp') {
    if (recipe.tcp_windows?.length) parts.push(`-w ${recipe.tcp_windows.join('/')}`);
    if (recipe.tcp_streams?.length) parts.push(`-P ${recipe.tcp_streams.join('/')}`);
  } else if (protocol === 'udp') {
    if (recipe.bandwidths?.length) parts.push(`-b ${recipe.bandwidths.join('/')}`);
    if (recipe.lengths?.length) parts.push(`-l ${recipe.lengths.join('/')}`);
    if (recipe.windows?.length) parts.push(`-w ${recipe.windows.join('/')}`);
    if (recipe.udp_streams?.length) parts.push(`${recipe.udp_streams.join('/')} 流`);
  }
  return parts.length ? parts.join(' · ') : '未填档位（等价于最朴素的一条）';
}
