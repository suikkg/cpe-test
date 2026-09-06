import { reactive } from 'vue';
import type { VerdictFilter } from '../domain/progress';

/** 左侧导航的区域标识。旧页用 1–5 的向导编号，但流程本来就不是严格线性的
 *  （「本机」不编号却常驻，第 3 步内部又自带 1·2·3·4），所以这里改用具名区域。 */
export type RegionId = 'local' | 'agent' | 'plan' | 'run' | 'progress' | 'monitor' | 'runs';

export interface RegionDef {
  id: RegionId;
  label: string;
  /** 分组：测试流程 / 独立工具。监控和「一轮测试」正交，不属于流程。 */
  group: 'flow' | 'tool';
}

export const REGIONS: readonly RegionDef[] = [
  { id: 'local', label: '本机', group: 'flow' },
  { id: 'agent', label: '辅测机', group: 'flow' },
  { id: 'plan', label: '测试计划', group: 'flow' },
  { id: 'run', label: '执行', group: 'flow' },
  { id: 'progress', label: '进度', group: 'flow' },
  { id: 'monitor', label: '监控', group: 'tool' },
  { id: 'runs', label: '历史运行', group: 'tool' },
];

/** 主题：跟随系统 / 强制亮 / 强制暗。旧页把这个写在 documentElement 的
 *  data-theme 上，CSS 变量按 :root[data-theme] 覆盖，这里保持同一套契约。 */
export type ThemePref = 'system' | 'light' | 'dark';

const THEME_KEY = 'cpe_ui_theme';

function storedTheme(): ThemePref {
  try {
    const raw = localStorage.getItem(THEME_KEY);
    if (raw === 'light' || raw === 'dark' || raw === 'system') return raw;
  } catch {
    // 隐私模式下 localStorage 会抛，主题偏好不值得为此中断加载。
  }
  return 'system';
}

/**
 * 纯界面上下文：查询、选中、返回来源（方案 §11.2）。
 *
 * 放在这里而不是各视图的 `ref` 里，是因为**视图会被 `v-if` 卸载**——切到别的
 * 区域再回来，组件是新挂载的一份，本地 `ref` 已经没了。用户会把搜索和选中
 * 当成「我刚才做过的事」，回来发现被清空只会以为界面出了错。
 *
 * 这里**不许**放业务数据（第二份网卡表、第二份计划），只放「我正在看哪一个、
 * 我筛掉了什么」。
 */
export interface RegionContext {
  /** 页内搜索的输入框内容，原样保留（含空格），匹配规则见 `domain/search`。 */
  query: string;
  /** 当前选中对象的稳定标识；空串 = 没选。 */
  selected: string;
}

function emptyContext(): RegionContext {
  return { query: '', selected: '' };
}

export const ui = reactive({
  region: 'local' as RegionId,
  theme: storedTheme(),
  /** 「本机」页的网卡列表上下文。 */
  local: emptyContext(),
  /** 「测试计划」页分配表的上下文（这一版只用到 query）。 */
  plan: emptyContext(),
  /** 套件工作区：query = 列表搜索，selected = 当前套件 id。 */
  suites: emptyContext(),
  /** 配置工作区：query = 列表搜索，selected = 当前配置 id。 */
  recipes: emptyContext(),
  /** 「执行」页计划复核清单的上下文（这一版只用到 query）。 */
  run: emptyContext(),
  /** 「进度」页已完成单元的上下文：query = 搜索，selected = 选中单元的 seq。 */
  progress: emptyContext(),
  /** 「监控」页：selected = 当前展示哪一路会话的曲线（**只换展示，不启停**）。 */
  monitor: emptyContext(),
  /** 「历史运行」页：query = 搜索，selected = 选中的运行目录 id。 */
  runs: emptyContext(),
  /** 进度页那一行可点筛选当前选的是哪一桶。 */
  progressFilter: 'all' as VerdictFilter,
  /**
   * 上面这组上下文属于哪一轮运行。
   *
   * 换了 `run_id` 必须整组清掉（§11.6）：上一轮的「只看未达标 + 搜 en1 + 选中 #37」
   * 套在新一轮上，看到的是一份**筛过的新数据**，而屏幕上没有任何地方说筛条件是
   * 上一轮留下的。
   */
  progressRunId: '',
  /**
   * 从某个任务点「编辑参数」跳进配置编辑器时记下的**来源**（方案 §11.2）。
   *
   * 不记的话，「返回套件」只能回到那个工作区，而选中的套件是组件本地 `ref`——
   * 重新挂载后回落到第一个。用户改完参数回来，看到的是别的套件，而他以为
   * 自己只是点了「返回」。
   */
  recipeReturn: null as { suiteId: string; taskId: string } | null,
});

export function goto(region: RegionId): void {
  ui.region = region;
}

/**
 * 换了一轮就把进度页的筛选、搜索和选中整组清掉（§11.6）。
 *
 * 同一轮之内反复调用是幂等的——它只在 `run_id` 真的变了时才动手，所以可以
 * 放在每一拍渲染里安心调用。
 */
export function syncProgressRun(runId: string): void {
  if (ui.progressRunId === runId) return;
  ui.progressRunId = runId;
  ui.progress = emptyContext();
  ui.progressFilter = 'all';
}

export function setTheme(theme: ThemePref): void {
  ui.theme = theme;
  applyTheme();
  try {
    localStorage.setItem(THEME_KEY, theme);
  } catch {
    // 同上：存不下就只在本次会话生效。
  }
}

export function applyTheme(): void {
  const root = document.documentElement;
  if (ui.theme === 'system') root.removeAttribute('data-theme');
  else root.setAttribute('data-theme', ui.theme);
}

/** 全部状态复位。每个 state 模块都要导出一个，供「断开连接 / 换辅测机」这类
 *  操作把整块资源清干净——旧页靠逐个变量手动赋值，漏一个就是幽灵状态。 */
export function reset(): void {
  ui.region = 'local';
  ui.local = emptyContext();
  ui.plan = emptyContext();
  ui.suites = emptyContext();
  ui.recipes = emptyContext();
  ui.run = emptyContext();
  ui.progress = emptyContext();
  ui.monitor = emptyContext();
  ui.runs = emptyContext();
  ui.progressFilter = 'all';
  ui.progressRunId = '';
  ui.recipeReturn = null;
}
