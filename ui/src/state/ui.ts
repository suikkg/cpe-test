import { reactive } from 'vue';

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

export const ui = reactive({
  region: 'local' as RegionId,
  theme: storedTheme(),
});

export function goto(region: RegionId): void {
  ui.region = region;
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
}
