import { reactive } from 'vue';
import type { VerdictFilter } from '../domain/progress';

/** 左侧导航的区域标识。旧页用 1–5 的向导编号，但流程本来就不是严格线性的
 *  （「本机」不编号却常驻，第 3 步内部又自带 1·2·3·4），所以这里改用具名区域。
 *
 *  「连接」合并了旧的本机/辅测机两页（两页的重扫是同一个请求）；「执行」合并了
 *  旧的执行/进度两页（开始之后本来就自动跳过去）。 */
export type RegionId = 'connect' | 'plan' | 'run' | 'inner' | 'monitor' | 'history';

export interface RegionDef {
  id: RegionId;
  label: string;
  /** 分组：测试流程 / 独立工具。监控和「一轮测试」正交，不属于流程。 */
  group: 'flow' | 'tool' | 'inner';
}

export const REGIONS: readonly RegionDef[] = [
  { id: 'connect', label: '连接', group: 'flow' },
  { id: 'plan', label: '计划', group: 'flow' },
  { id: 'run', label: '执行', group: 'flow' },
  { id: 'inner', label: '内环测试', group: 'inner' },
  { id: 'monitor', label: '监控', group: 'tool' },
  { id: 'history', label: '历史', group: 'tool' },
];

/** 「计划」页的三个标签。 */
export type PlanTab = 'ports' | 'content' | 'limits';
/** 「历史」页的三个标签。 */
export type HistoryTab = 'subnet' | 'inner' | 'scenario';

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
  region: 'connect' as RegionId,
  theme: storedTheme(),
  /** 「连接」页的网卡列表上下文（只用到 query）。 */
  connect: emptyContext(),
  /** 「计划」页网口表的上下文（只用到 query）。 */
  plan: emptyContext(),
  /** 套件工作区：query = 列表搜索，selected = 当前套件 id。 */
  suites: emptyContext(),
  /** 「测试内容」里就地展开编辑的配置：selected = 配置 id（query 未用）。 */
  recipes: emptyContext(),
  /** 「执行」页准备面板的预览清单上下文（只用到 query）。 */
  run: emptyContext(),
  /** 「执行」页进度面板的已完成单元：query = 搜索，selected = 展开详情的单元 seq。 */
  progress: emptyContext(),
  /** 「监控」页：selected = 当前展示哪一路会话的曲线（**只换展示，不启停**）。 */
  monitor: emptyContext(),
  /** 「历史」页子网标签：query = 搜索。 */
  history: emptyContext(),
  planTab: 'ports' as PlanTab,
  historyTab: 'subnet' as HistoryTab,
  /**
   * 「执行」页上一轮已结束、用户点了「准备下一轮」。
   *
   * 只在「本轮已结束」时起作用：运行中、开始结果未确认时一律显示进度，不看它。
   * 开始被受理时清掉，这样新一轮结束后仍先看到结果。
   */
  preparing: false,
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
  ui.region = 'connect';
  ui.connect = emptyContext();
  ui.plan = emptyContext();
  ui.suites = emptyContext();
  ui.recipes = emptyContext();
  ui.run = emptyContext();
  ui.progress = emptyContext();
  ui.monitor = emptyContext();
  ui.history = emptyContext();
  ui.planTab = 'ports';
  ui.historyTab = 'subnet';
  ui.preparing = false;
  ui.progressFilter = 'all';
  ui.progressRunId = '';
}
