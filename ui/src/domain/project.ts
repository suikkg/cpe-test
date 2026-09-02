import { ensureDefaults, type UiPlan, type UiRecipe, type UiRecipeProfile } from './plan-build';
import type { UiBinding, UiLinkSet, UiPairRef, UiSuite, UiTask } from './plan-build';
import {
  emptyGlobals,
  migrateBidirPairToTotal,
  normalizeGlobals,
  type UiGlobals,
  type UiNicPolicy,
  type UiWifiBandThreshold,
} from './globals';

/**
 * 项目文件（`cpe-ui-project.json`）的读写。纯函数。
 *
 * # v3：导出的是**有效值快照**，不是输入框状态
 *
 * v2 直接序列化编辑态。界面上留空的格子在导出时是 `0` / `[]`，而屏幕上显示的
 * 灰字「默认 30」只存在于 bootstrap 回填里。换一台主控导入，后端会改用那台
 * 机器自己的 Ping 次数、payload 分类和 RTT 门限——**判定口径静默改变，而项目
 * 文件上看不出来**。v3 把值在导出前换算成主控当前真正会用的那一份
 * （`resolveEffectiveGlobals`），并把界面上根本没有输入框、却参与判定的两样
 * 东西也带走：全局 RX 门限、UDP 档位原样列表。
 *
 * # 导入是原子的，并且逐元素校验
 *
 * v2 只检查顶层容器是不是数组，元素一律强转。`suites: [null]` 会在页面
 * computed 读 `suite.tasks` 时抛异常，`recipes.tcp: [null]` 会在 `stripDeadFields`
 * 读 `recipe.mode` 时抛异常——**导入用户挑的一个 JSON 文件不该能让页面崩掉**。
 * 现在每个数组元素、每个数值都过一遍清洗；任何一步失败都返回错误，
 * 调用方保持原项目不变。
 *
 * # 仍然不做语义校验（ADR-11）
 *
 * 引用完整性、端点存在性、参数范围交给 `/api/plan` 的报错：端点是否存在没有
 * 拓扑根本判不了，而项目**允许在未连接时导入**。Rust 侧的 `validate_ui_plan`
 * 已经把这件事做完并打磨过文案，在 JS 里再写一份等于把重复固化成制度。
 */

export const PROJECT_VERSION = 3;

/** 可复现测试项目要带走的设置；这里不包含本地运行态、agent 或控制台口令。 */
export interface ProjectSettings {
  duration?: number;
  limit_udp_by_link_speed?: boolean;
  globals?: UiGlobals;
  /**
   * 主控的**解析后配置**：判定与灌包参数的完整基线。
   *
   * 界面上有输入框的东西由 `globals` / `nicPolicies` 带走；界面上**没有**输入框
   * 的（`rate_check` 的负载上限与余量、角色配对门限、ctsTraffic 的帧率与缓冲
   * 深度）只能靠这一整块跨机复现。逐字段加通道永远追不完，漏一个就是一次静默
   * 的口径漂移。
   *
   * 前端不解释它的内容，只**原样搬运**：字段语义在 Rust 的 `Config` 里，
   * 在这边再写一份类型定义就是两份会漂的实现。
   */
  masterConfig?: Record<string, unknown>;
}

/** v3 的执行默认值块：主控当前**真正会用**的档位。 */
export interface ProjectExecutionDefaults {
  duration: number;
  limit_udp_by_link_speed: boolean;
  tcp: { windows: string[]; streams: number[] };
  udp: {
    bandwidths: string[];
    lengths: string[];
    windows: string[];
    streams: number;
    /** 三条轴留空时钉住的原样档位表；空数组 = 由上面三条轴决定。 */
    profiles: unknown[];
  };
  ping: { count: number; payload_sizes: number[] };
}

/** v3 的验收块：所有影响 PASS/FAIL 的门限。 */
export interface ProjectAcceptance {
  ping_thresholds: Record<string, number>;
  nic_policies: UiNicPolicy[];
  wifi_band_thresholds: UiWifiBandThreshold[];
  wifi_pair_bidir_total_rx_target_mbps: number;
  /** 旧项目里的具体网口覆盖与统一门限；新界面不再创建，只做无损往返。 */
  legacy_wifi_overrides: {
    rx_target_mbps: number;
    bidir_rx_target_mbps: number;
    pair_thresholds: UiGlobals['wifi_pair_thresholds'];
  };
}

export interface ProjectFile {
  project_version: number;
  plan: UiPlan;
  execution_defaults: ProjectExecutionDefaults;
  acceptance: ProjectAcceptance;
  /**
   * 主控的解析后配置。**这一块才是「所有实际测试参数和判定参数」的所在地**；
   * 上面两块是界面态，运行时叠在它上面（和后端 `ui_baseline_config` +
   * `RunRequest` 的关系完全一致，不是新发明的优先级）。
   */
  master_config: Record<string, unknown>;
}

export interface ParseResult {
  ok: boolean;
  /** 解析成功时的计划 */
  plan?: UiPlan;
  /** 项目文件包含执行设置时带回的非敏感部分 */
  settings?: ProjectSettings;
  /** 按网口门限与负载策略 */
  nicPolicies?: UiNicPolicy[];
  /** 形状问题；语义问题不在这里，等预览 */
  error?: string;
  /** 读入时被丢掉/修正的东西，要让用户看见 */
  notices: string[];
}

const PING_THRESHOLD_KEYS = [
  'ping_small_max_bytes',
  'ping_medium_max_bytes',
  'ping_wired_small_avg_rtt_ms',
  'ping_wired_small_max_rtt_ms',
  'ping_wired_medium_avg_rtt_ms',
  'ping_wired_medium_max_rtt_ms',
  'ping_wired_large_avg_rtt_ms',
  'ping_wired_large_max_rtt_ms',
  'ping_wifi_small_avg_rtt_ms',
  'ping_wifi_small_max_rtt_ms',
  'ping_wifi_medium_avg_rtt_ms',
  'ping_wifi_medium_max_rtt_ms',
  'ping_wifi_large_avg_rtt_ms',
  'ping_wifi_large_max_rtt_ms',
] as const;

/** v6.0 随包项目把全局执行字段直接摊在 `settings` 下；认出来才好提示用户。 */
const LEGACY_FLAT_KEYS = [
  'tcp_windows',
  'tcp_streams',
  'udp_bandwidths',
  'udp_lengths',
  'udp_windows',
  'udp_streams',
  'ping_count',
  'ping_payload_sizes',
  'ping_max_rtt_ms',
  'wifi_pair_rx_target_mbps',
  'wifi_pair_bidir_rx_target_mbps',
  'wifi_band_thresholds',
  'wifi_pair_thresholds',
] as const;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function text(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function stringList(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === 'string')
    : [];
}

function positiveIntegerList(value: unknown): number[] {
  return Array.isArray(value)
    ? value
        .filter(
          (item): item is number =>
            typeof item === 'number' && Number.isFinite(item) && item > 0,
        )
        .map((item) => Math.trunc(item))
    : [];
}

function positiveInteger(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) && value > 0
    ? Math.trunc(value)
    : undefined;
}

// ---------------------------------------------------------------------------
// 逐元素清洗
// ---------------------------------------------------------------------------
//
// 每个 `clean*` 都接受 `unknown` 并返回一个**完整**的对象，或者 `null` 表示
// 「这一项没救了，丢掉」。丢掉多少条会记进 notices，让用户看得见。

function cleanPairRef(value: unknown): UiPairRef | null {
  if (!isRecord(value)) return null;
  const src = text(value.src).trim();
  const dst = text(value.dst).trim();
  if (!src || !dst) return null;
  return { id: text(value.id) || `${src}->${dst}`, src, dst };
}

function cleanLinkSet(value: unknown): UiLinkSet | null {
  if (!isRecord(value)) return null;
  const id = text(value.id).trim();
  if (!id) return null;
  const pair_refs = Array.isArray(value.pair_refs)
    ? value.pair_refs.map(cleanPairRef).filter((item): item is UiPairRef => item !== null)
    : [];
  return { id, name: text(value.name), pair_refs };
}

function cleanRecipeProfile(value: unknown): UiRecipeProfile | null {
  if (!isRecord(value)) return null;
  const profile: UiRecipeProfile = {};
  const window = text(value.window).trim();
  const bandwidth = text(value.bandwidth).trim();
  const length = text(value.length).trim();
  const streams = positiveInteger(value.streams);
  if (window) profile.window = window;
  if (bandwidth) profile.bandwidth = bandwidth;
  if (length) profile.length = length;
  if (streams !== undefined) profile.streams = streams;
  return Object.keys(profile).length > 0 ? profile : null;
}

/**
 * 清洗一条配置卡片。
 *
 * `mode` 在这里被**丢掉**：它是死字段——校验器过去只准 `fixed`/`scan`，而计划
 * 编译器从头到尾不读它，两个取值产出同一份计划。服务端现在会明确拒绝非空
 * `mode`（ADR-16），而那个字段是旧版界面自动写进去的，用户一个字都没打过。
 */
function cleanRecipe(value: unknown, stripped: { mode: number }): UiRecipe | null {
  if (!isRecord(value)) return null;
  const id = text(value.id).trim();
  if (!id) return null;
  if (text(value.mode) !== '') stripped.mode += 1;
  const recipe: UiRecipe = {
    id,
    name: text(value.name),
    profiles: Array.isArray(value.profiles)
      ? value.profiles
          .map(cleanRecipeProfile)
          .filter((item): item is UiRecipeProfile => item !== null)
      : [],
  };
  const tcp_windows = stringList(value.tcp_windows);
  const tcp_streams = positiveIntegerList(value.tcp_streams);
  const bandwidths = stringList(value.bandwidths);
  const lengths = stringList(value.lengths);
  const windows = stringList(value.windows);
  const udp_streams = positiveIntegerList(value.udp_streams);
  if (tcp_windows.length) recipe.tcp_windows = tcp_windows;
  if (tcp_streams.length) recipe.tcp_streams = tcp_streams;
  if (bandwidths.length) recipe.bandwidths = bandwidths;
  if (lengths.length) recipe.lengths = lengths;
  if (windows.length) recipe.windows = windows;
  if (udp_streams.length) recipe.udp_streams = udp_streams;
  return recipe;
}

function cleanTask(value: unknown): UiTask | null {
  if (!isRecord(value)) return null;
  const id = text(value.id).trim();
  if (!id) return null;
  const protocol = text(value.protocol).trim().toLowerCase();
  const task: UiTask = {
    id,
    name: text(value.name),
    protocol: protocol === 'udp' || protocol === 'ping' ? protocol : 'tcp',
    directions: stringList(value.directions),
    ip: stringList(value.ip),
    recipe_ids: stringList(value.recipe_ids),
    rx_target_bidir_ab: text(value.rx_target_bidir_ab),
    rx_target_bidir_ba: text(value.rx_target_bidir_ba),
    rx_target_bidir_total: text(value.rx_target_bidir_total),
  };
  const duration = positiveInteger(value.duration);
  const ping_count = positiveInteger(value.ping_count);
  if (duration !== undefined) task.duration = duration;
  if (ping_count !== undefined) task.ping_count = ping_count;
  if (Array.isArray(value.ping_payload_sizes)) {
    task.ping_payload_sizes = positiveIntegerList(value.ping_payload_sizes);
  }
  return task;
}

function cleanSuite(value: unknown): UiSuite | null {
  if (!isRecord(value)) return null;
  const id = text(value.id).trim();
  if (!id) return null;
  const tasks = Array.isArray(value.tasks)
    ? value.tasks.map(cleanTask).filter((item): item is UiTask => item !== null)
    : [];
  const taskIds = new Set(tasks.map((task) => task.id));
  return {
    id,
    name: text(value.name),
    note: text(value.note),
    execution: text(value.execution) || 'sequential',
    // 顺序里指着不存在任务的条目会让服务端整份拒掉；这里直接滤掉，
    // 比让用户对着「order 引用了无效 task」去手改 JSON 强。
    order: stringList(value.order).filter((taskId) => taskIds.has(taskId)),
    tasks,
  };
}

function cleanBinding(value: unknown): UiBinding | null {
  if (!isRecord(value)) return null;
  const id = text(value.id).trim();
  const link_set_id = text(value.link_set_id).trim();
  const suite_id = text(value.suite_id).trim();
  if (!id || !link_set_id || !suite_id) return null;
  return {
    id,
    link_set_id,
    suite_id,
    pair_ids: stringList(value.pair_ids),
    mode: text(value.mode) || 'all',
  };
}

/** ID 去重：重复 ID 会让「按 id 找」在两条记录之间摇摆，只保留第一条。 */
function dedupeById<T extends { id: string }>(items: T[], dropped: { count: number }): T[] {
  const seen = new Set<string>();
  return items.filter((item) => {
    if (seen.has(item.id)) {
      dropped.count += 1;
      return false;
    }
    seen.add(item.id);
    return true;
  });
}

function cleanPlan(value: unknown, notices: string[]): UiPlan | { error: string } {
  if (!isRecord(value)) return { error: '缺少 ui_plan' };
  // 容器**必须在场且是数组**。少一个容器不是「这一项留空」，是文件被截断或
  // 根本不是项目文件；这种情况下继续往下清洗只会给出一份看似合理、实则残缺
  // 的计划。
  for (const key of ['link_sets', 'suites', 'bindings'] as const) {
    if (!Array.isArray(value[key])) {
      return { error: `ui_plan.${key} 必须是数组` };
    }
  }
  const rawRecipes = value.recipes;
  if (!isRecord(rawRecipes)) {
    return { error: 'ui_plan.recipes 必须是对象' };
  }
  for (const key of ['tcp', 'udp', 'ping'] as const) {
    const list = rawRecipes[key];
    if (list !== undefined && !Array.isArray(list)) {
      return { error: `ui_plan.recipes.${key} 必须是数组` };
    }
  }

  const stripped = { mode: 0 };
  const dropped = { count: 0 };
  const cleanList = <T>(raw: unknown, clean: (item: unknown) => T | null): T[] => {
    if (!Array.isArray(raw)) return [];
    const out: T[] = [];
    for (const item of raw) {
      const cleaned = clean(item);
      if (cleaned === null) dropped.count += 1;
      else out.push(cleaned);
    }
    return out;
  };
  const recipeList = (key: 'tcp' | 'udp' | 'ping'): UiRecipe[] =>
    dedupeById(
      cleanList(rawRecipes[key], (item) => cleanRecipe(item, stripped)),
      dropped,
    );

  const plan: UiPlan = {
    ui_plan_version: positiveInteger(value.ui_plan_version) ?? 1,
    link_sets: dedupeById(cleanList(value.link_sets, cleanLinkSet), dropped),
    recipes: { tcp: recipeList('tcp'), udp: recipeList('udp'), ping: recipeList('ping') },
    suites: dedupeById(cleanList(value.suites, cleanSuite), dropped),
    bindings: dedupeById(cleanList(value.bindings, cleanBinding), dropped),
  };
  if (dropped.count > 0) {
    notices.push(
      `项目文件里有 ${dropped.count} 项内容缺少必需字段或是重复 ID，已忽略。` +
        `导入没有中断——余下的内容是完整的，但请在预览里核对一遍。`,
    );
  }
  if (stripped.mode > 0) {
    notices.push(
      `已移除 ${stripped.mode} 处废弃的 mode 字段：档位由轴的取值个数决定（单值=钉死、多值=扫描），` +
        `mode 从来没有被计划编译器读过。这个字段是旧版界面自动写进去的，不影响你的计划。`,
    );
  }
  return plan;
}

// ---------------------------------------------------------------------------
// 读入
// ---------------------------------------------------------------------------

function nonNegative(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : 0;
}

function legacyBandNotice(value: unknown): boolean {
  return (
    Array.isArray(value) &&
    value.some((item) => isRecord(item) && ('src_band' in item || 'dst_band' in item))
  );
}

function legacyDirectionalBidirNotice(value: unknown): boolean {
  return (
    Array.isArray(value) &&
    value.some(
      (item) =>
        isRecord(item) &&
        item.bidir_total_rx_target_mbps === undefined &&
        (nonNegative(item.bidir_rx_target_master_to_agent_mbps) > 0 ||
          nonNegative(item.bidir_rx_target_agent_to_master_mbps) > 0),
    )
  );
}

/** 把 v1/v2 的扁平/嵌套设置和 v3 的分块设置，统一收敛成一份 `UiGlobals`。 */
function globalsFromFile(
  file: Record<string, unknown>,
  notices: string[],
): {
  globals?: UiGlobals;
  duration?: number;
  limitUdp?: boolean;
  masterConfig?: Record<string, unknown>;
} {
  const execution = isRecord(file.execution_defaults) ? file.execution_defaults : null;
  const acceptance = isRecord(file.acceptance) ? file.acceptance : null;
  if (execution || acceptance) {
    const tcp = isRecord(execution?.tcp) ? execution.tcp : {};
    const udp = isRecord(execution?.udp) ? execution.udp : {};
    const ping = isRecord(execution?.ping) ? execution.ping : {};
    const pingThresholds = isRecord(acceptance?.ping_thresholds)
      ? acceptance.ping_thresholds
      : {};
    const legacy = isRecord(acceptance?.legacy_wifi_overrides)
      ? acceptance.legacy_wifi_overrides
      : {};
    const flat: Record<string, unknown> = {
      tcp_windows: tcp.windows,
      tcp_streams: tcp.streams,
      udp_bandwidths: udp.bandwidths,
      udp_lengths: udp.lengths,
      udp_windows: udp.windows,
      udp_streams: udp.streams,
      udp_profiles: udp.profiles,
      ping_count: ping.count,
      ping_payload_sizes: ping.payload_sizes,
      ...pingThresholds,
      wifi_band_thresholds: acceptance?.wifi_band_thresholds,
      wifi_pair_bidir_total_rx_target_mbps:
        acceptance?.wifi_pair_bidir_total_rx_target_mbps,
      wifi_pair_rx_target_mbps: legacy.rx_target_mbps,
      wifi_pair_bidir_rx_target_mbps: legacy.bidir_rx_target_mbps,
      wifi_pair_thresholds: legacy.pair_thresholds,
    };
    noticeWifiMigrations(flat, notices);
    return {
      globals: normalizeGlobals(flat),
      duration: positiveInteger(execution?.duration),
      limitUdp:
        typeof execution?.limit_udp_by_link_speed === 'boolean'
          ? execution.limit_udp_by_link_speed
          : undefined,
      masterConfig: isRecord(file.master_config) ? file.master_config : undefined,
    };
  }

  // v1 / v2：settings.globals 或 settings 下的扁平字段。
  const settings = isRecord(file.settings) ? file.settings : {};
  if (!isRecord(file.settings)) {
    // 完全没有执行设置的旧项目：**不要**造一份空 globals 回去。空 globals 和
    // 「这份项目没说」是两件事——前者会把上一份项目的默认档位一起抹掉，
    // 界面上变成一张全空的表，而用户什么都没改。
    return {
      limitUdp:
        typeof file.limit_udp_by_link_speed === 'boolean'
          ? file.limit_udp_by_link_speed
          : undefined,
    };
  }
  const flatLegacy = !isRecord(settings.globals);
  const source = flatLegacy ? settings : (settings.globals as Record<string, unknown>);
  noticeWifiMigrations(source, notices);
  const globals = normalizeGlobals(source);
  if (flatLegacy && LEGACY_FLAT_KEYS.some((key) => key in settings)) {
    notices.push('已兼容旧版项目里的扁平执行设置，并迁移到当前项目格式。');
  }
  const legacyTopLevelLimit =
    typeof file.limit_udp_by_link_speed === 'boolean'
      ? file.limit_udp_by_link_speed
      : undefined;
  return {
    globals,
    duration: positiveInteger(settings.duration),
    limitUdp:
      typeof settings.limit_udp_by_link_speed === 'boolean'
        ? settings.limit_udp_by_link_speed
        : legacyTopLevelLimit,
  };
}

function noticeWifiMigrations(source: Record<string, unknown>, notices: string[]): void {
  if (legacyBandNotice(source.wifi_band_thresholds)) {
    notices.push('已把旧版 Wi-Fi 发送频段→接收频段门限迁移为当前频段组合。');
  }
  if (legacyDirectionalBidirNotice(source.wifi_band_thresholds)) {
    notices.push(
      '旧版按方向填的两个双向门限已迁移为「两端 RX 合计 = 两者之和」；' +
        '只填过一个方向的规则没有推导合计，需要你自己确认一次。',
    );
  }
  if (nonNegative(source.wifi_pair_bidir_rx_target_mbps) > 0) {
    notices.push(
      '旧版「统一双向每方向门限」按 ×2 迁移为双向 RX 合计门限（两个方向各一份）。',
    );
  }
  if (
    nonNegative(source.wifi_pair_rx_target_mbps) > 0 ||
    (Array.isArray(source.wifi_pair_thresholds) && source.wifi_pair_thresholds.length > 0)
  ) {
    notices.push('项目含旧版具体 Wi-Fi 网口覆盖，仍按兼容规则执行；可在执行页清除。');
  }
}

function parseNicPolicies(value: unknown, notices: string[]): UiNicPolicy[] | undefined {
  if (value === undefined) return undefined;
  if (!Array.isArray(value)) {
    notices.push('项目文件里的 nic_policies 不是数组，已忽略按网口策略。');
    return undefined;
  }
  let dropped = 0;
  const policies = value.flatMap((item): UiNicPolicy[] => {
    if (!isRecord(item) || typeof item.endpoint !== 'string' || !item.endpoint.trim()) {
      dropped += 1;
      return [];
    }
    return [
      {
        endpoint: item.endpoint,
        rx_target: text(item.rx_target),
        udp_bandwidth: text(item.udp_bandwidth),
        udp_length: text(item.udp_length),
      },
    ];
  });
  if (dropped > 0) notices.push(`项目文件里有 ${dropped} 条按网口策略无法识别，已忽略。`);
  return policies;
}

/**
 * 读入一份项目文件。
 *
 * **原子**：任何一步失败都返回 `ok: false`，调用方保持当前项目不变；成功时
 * 返回的每一个字段都已经清洗过，可以直接放进 reactive state 而不会在
 * `.join()` / `.length` / `.tasks` 上炸掉。
 */
export function parseProject(text_: string): ParseResult {
  const notices: string[] = [];
  let raw: unknown;
  try {
    raw = JSON.parse(text_);
  } catch (error) {
    return {
      ok: false,
      error: `这不是一份能解析的 JSON：${error instanceof Error ? error.message : error}`,
      notices,
    };
  }
  // 数组也是 `typeof === 'object'`——不显式排掉的话，一份 `[]` 会一路走到
  // 「缺少 project_version」，报错指向的位置就偏了。
  if (!isRecord(raw)) {
    return { ok: false, error: '项目文件的顶层必须是一个对象', notices };
  }
  const file = raw as Record<string, unknown>;

  if (typeof file.project_version !== 'number' || !Number.isFinite(file.project_version)) {
    return { ok: false, error: '缺少 project_version', notices };
  }
  if (file.project_version > PROJECT_VERSION) {
    return {
      ok: false,
      error: `项目文件版本 ${file.project_version} 比当前程序支持的 ${PROJECT_VERSION} 新，请升级 cpe_test`,
      notices,
    };
  }

  // v3 用 `plan`，v1/v2 用 `ui_plan`。
  const rawPlan = file.plan !== undefined ? file.plan : file.ui_plan;
  const cleaned = cleanPlan(rawPlan, notices);
  if ('error' in cleaned) return { ok: false, error: cleaned.error, notices };

  const { globals, duration, limitUdp, masterConfig } = globalsFromFile(file, notices);
  const nicPolicies = parseNicPolicies(
    isRecord(file.acceptance) ? file.acceptance.nic_policies : file.nic_policies,
    notices,
  );

  const settings: ProjectSettings = {};
  if (globals) settings.globals = globals;
  if (masterConfig) settings.masterConfig = masterConfig;
  if (duration !== undefined) settings.duration = duration;
  if (limitUdp !== undefined) settings.limit_udp_by_link_speed = limitUdp;

  return {
    ok: true,
    plan: ensureDefaults(cleaned),
    settings,
    nicPolicies,
    notices,
  };
}

// ---------------------------------------------------------------------------
// 导出
// ---------------------------------------------------------------------------

/**
 * 导出一份项目文件。
 *
 * 传进来的 `settings.globals` 必须已经是 `resolveEffectiveGlobals` 的结果——
 * 这个函数不认识 bootstrap，也不该认识：它只负责把一份已经确定的值排成 v3
 * 的形状。这样「换算」只有一处实现，而且能被单独测。
 */
export function buildProject(
  plan: UiPlan,
  settings?: ProjectSettings,
  nicPolicies?: UiNicPolicy[],
): ProjectFile {
  const globals = settings?.globals ?? emptyGlobals();
  const pingThresholds: Record<string, number> = {};
  for (const key of PING_THRESHOLD_KEYS) pingThresholds[key] = globals[key];
  return {
    project_version: PROJECT_VERSION,
    plan,
    execution_defaults: {
      duration: settings?.duration ?? 180,
      limit_udp_by_link_speed: settings?.limit_udp_by_link_speed === true,
      tcp: { windows: [...globals.tcp_windows], streams: [...globals.tcp_streams] },
      udp: {
        bandwidths: [...globals.udp_bandwidths],
        // `-l` / `-w` 留空是「明确不下发」，导出时**保持空**，不许回落。
        lengths: [...globals.udp_lengths],
        windows: [...globals.udp_windows],
        streams: globals.udp_streams,
        // 三条轴全空时走主控档位表，那张表在 `master_config.iperf.udp_profiles`
        // 里整块带走——拆成会叉乘的三条轴还原不回来。
        profiles: [],
      },
      ping: {
        count: globals.ping_count,
        payload_sizes: [...globals.ping_payload_sizes],
      },
    },
    acceptance: {
      ping_thresholds: pingThresholds,
      nic_policies: nicPolicies ?? [],
      wifi_band_thresholds: [...globals.wifi_band_thresholds],
      wifi_pair_bidir_total_rx_target_mbps: globals.wifi_pair_bidir_total_rx_target_mbps,
      legacy_wifi_overrides: {
        rx_target_mbps: globals.wifi_pair_rx_target_mbps,
        bidir_rx_target_mbps: globals.wifi_pair_bidir_rx_target_mbps,
        pair_thresholds: [...globals.wifi_pair_thresholds],
      },
    },
    master_config: settings?.masterConfig ?? {},
  };
}

export function serializeProject(
  plan: UiPlan,
  settings?: ProjectSettings,
  nicPolicies?: UiNicPolicy[],
): string {
  return `${JSON.stringify(buildProject(plan, settings, nicPolicies), null, 2)}\n`;
}

// `migrateBidirPairToTotal` 是 globals 层的迁移入口，本模块通过 normalizeGlobals
// 间接使用；再导出一次是为了让它的存在被显式表达出来。
export { migrateBidirPairToTotal };
