import type { MonitorPoint } from '../api/dto';

/**
 * 速率曲线的抽稀与读数统计。纯函数。
 *
 * # 四条「不许回归」项
 *
 * `b3013e6` 修对过下面四件事，Vue 版**不许退回去**（DESIGN §4.2 的更正框把它们
 * 从「待办」改成了「不许回归」）：
 *
 * 1. **X 轴用 `point.t`，不是数组下标。** 采样有丢有补，下标当时间会让曲线在
 *    丢样的地方被悄悄压扁——看上去是速率平稳，实际是那段没采到。
 * 2. **点数上限单值 7200**，不再是 7200/3600/600 三处不一致。
 * 3. **曲线、均值、峰值同一个窗口。** 三者取不同窗口时，读数和图对不上，
 *    而两边都「看起来没错」。
 * 4. **`reducePoints` 按每像素列压 min/max，不是抽稀。** 直接抽稀会把尖峰漏掉，
 *    而尖峰恰恰是灌包测试要看的东西。
 */

/** 环形缓冲上限。与服务端 `MONITOR_MAX_POINTS` 同值——一处改两处都要改。 */
export const MAX_POINTS = 7200;

export type SeriesKey = 'rx_mbps' | 'tx_mbps';

export interface ReducedPoint {
  t: number;
  v: number;
}

/**
 * 按**每像素列**压成 min/max 两点，而不是抽稀。
 *
 * 抽稀（每 N 个取一个）会把尖峰整个漏掉。灌包测试里那些尖峰和凹陷恰恰是要看
 * 的东西——「平均达标但中间掉过底」正是判定里 `RX_DROPOUT` 在抓的事。
 *
 * 同一列内先出现的极值先画，保证折线不会自己回头。
 */
export function reducePoints(
  points: MonitorPoint[],
  key: SeriesKey,
  t0: number,
  span: number,
  cols: number,
): ReducedPoint[] {
  if (points.length <= cols * 2 || span <= 0 || cols <= 0) {
    return points.map((p) => ({ t: p.t, v: p[key] }));
  }
  const out: ReducedPoint[] = [];
  let col = -1;
  let lo: ReducedPoint | null = null;
  let hi: ReducedPoint | null = null;
  const flush = (): void => {
    if (!lo || !hi) return;
    if (lo.t <= hi.t) {
      out.push(lo);
      if (hi.t !== lo.t) out.push(hi);
    } else {
      out.push(hi);
      out.push(lo);
    }
  };
  for (const p of points) {
    const c = Math.floor(((p.t - t0) / span) * cols);
    const v = p[key];
    if (c !== col) {
      flush();
      col = c;
      lo = { t: p.t, v };
      hi = { t: p.t, v };
    } else {
      if (lo && v < lo.v) lo = { t: p.t, v };
      if (hi && v > hi.v) hi = { t: p.t, v };
    }
  }
  flush();
  return out;
}

export interface Readings {
  /** 窗口内的平均值 */
  avg: number;
  /** 窗口内的峰值 */
  peak: number;
  /** 最后一个采样点的值 */
  last: number;
  /** 参与统计的样本数——让人知道这些读数有多少数据支撑 */
  samples: number;
}

/**
 * 读数统计。
 *
 * **必须和曲线用同一批点**：三者取不同窗口时，读数和图对不上，而两边都
 * 「看起来没错」。所以这个函数吃的就是画图用的那个数组。
 */
export function readings(points: MonitorPoint[], key: SeriesKey): Readings {
  if (points.length === 0) {
    return { avg: 0, peak: 0, last: 0, samples: 0 };
  }
  let sum = 0;
  let peak = 0;
  for (const p of points) {
    const v = p[key];
    sum += v;
    if (v > peak) peak = v;
  }
  return {
    avg: sum / points.length,
    peak,
    last: points[points.length - 1][key],
    samples: points.length,
  };
}

/** 把新样本并进环形缓冲，超出上限时丢最旧的。 */
export function appendPoints(existing: MonitorPoint[], incoming: MonitorPoint[]): MonitorPoint[] {
  if (incoming.length === 0) return existing;
  const merged = existing.concat(incoming);
  return merged.length > MAX_POINTS ? merged.slice(-MAX_POINTS) : merged;
}

/**
 * 曲线的时间跨度。**用 `point.t`，不是数组下标。**
 *
 * 采样有丢有补：下标当时间会让曲线在丢样的地方被悄悄压扁，看上去是速率平稳，
 * 实际是那段根本没采到。
 */
export function timeSpan(points: MonitorPoint[]): { t0: number; t1: number; span: number } {
  if (points.length === 0) return { t0: 0, t1: 0, span: 0 };
  const t0 = points[0].t;
  const t1 = points[points.length - 1].t;
  return { t0, t1, span: Math.max(t1 - t0, 0.001) };
}

/** 生成 SVG 折线的 `points` 属性。 */
export function polyline(
  reduced: ReducedPoint[],
  t0: number,
  span: number,
  maxValue: number,
  width: number,
  height: number,
): string {
  if (reduced.length === 0 || maxValue <= 0) return '';
  return reduced
    .map((p) => {
      const x = ((p.t - t0) / span) * width;
      const y = height - (p.v / maxValue) * height;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(' ');
}

/**
 * Y 轴上界：留 10% 余量，再向上取到**两位有效数字**。
 *
 * 取两位而不是一位：一位有效数字会把 1023 抬到 2000，曲线只占半个高度，
 * 而这张图是用来看波形起伏的——压扁一半等于把要看的东西丢了。
 */
export function axisMax(peak: number): number {
  if (peak <= 0) return 100;
  const withMargin = peak * 1.1;
  const magnitude = 10 ** Math.floor(Math.log10(withMargin));
  const step = magnitude / 10;
  return Math.ceil(withMargin / step) * step;
}

/**
 * Y 轴刻度值，从上界到 0，共 `divisions + 1` 个。
 *
 * 和 `RateChart` 里那几条水平网格线是**同一套分割**：网格线画在 1/4、2/4、3/4，
 * 刻度就必须落在 0、1/4、2/4、3/4、4/4。两边各写一份的话，改了分割数就会出现
 * 「线在这里、数字标在那里」——而那种图比没有刻度更糟，它会让人读错量级。
 */
export function valueTicks(max: number, divisions = 4): number[] {
  if (!(max > 0) || divisions < 1) return [];
  const out: number[] = [];
  for (let i = divisions; i >= 0; i -= 1) out.push((max * i) / divisions);
  return out;
}

/**
 * X 轴刻度：会话开始后的秒数，从 `t0` 到 `t1`。
 *
 * 用 `point.t` 而不是数组下标——采样有丢有补，下标当时间会让曲线在丢样的地方
 * 被悄悄压扁（本模块开头第 1 条）。刻度当然要和曲线用同一个坐标。
 */
export function timeTicks(t0: number, t1: number, divisions = 4): number[] {
  if (divisions < 1) return [];
  const span = t1 - t0;
  const out: number[] = [];
  for (let i = 0; i <= divisions; i += 1) out.push(t0 + (span * i) / divisions);
  return out;
}

/** 速率读数：上千就换 Gbps，免得轴上全是五位数。 */
export function formatRate(mbps: number): string {
  if (!Number.isFinite(mbps)) return '—';
  if (mbps >= 1000) return `${(mbps / 1000).toFixed(mbps >= 10000 ? 0 : 2)}G`;
  if (mbps >= 100) return mbps.toFixed(0);
  if (mbps >= 10) return mbps.toFixed(1);
  return mbps.toFixed(2);
}

/** 相对时间读数。**相对而不是绝对**：两端的系统时钟不保证同步。 */
export function formatElapsed(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) return '0s';
  const total = Math.round(secs);
  if (total < 60) return `${total}s`;
  const minutes = Math.floor(total / 60);
  if (minutes < 60) return `${minutes}m${String(total % 60).padStart(2, '0')}s`;
  return `${Math.floor(minutes / 60)}h${String(minutes % 60).padStart(2, '0')}m`;
}
