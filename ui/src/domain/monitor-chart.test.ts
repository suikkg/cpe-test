import { describe, expect, it } from 'vitest';
import type { MonitorPoint } from '../api/dto';
import {
  MAX_POINTS,
  appendPoints,
  axisMax,
  polyline,
  readings,
  reducePoints,
  timeSpan,
} from './monitor-chart';

function pts(values: Array<[number, number]>): MonitorPoint[] {
  return values.map(([t, rx]) => ({ t, rx_mbps: rx, tx_mbps: rx / 2 }));
}

/**
 * 这四条是 `b3013e6` 修对过的东西，DESIGN §4.2 把它们从「待办」改成了
 * **「不许回归」**。Vue 版退回去的话没有任何别的测试会红。
 */
describe('监控曲线的四条不许回归项', () => {
  it('1. X 轴用 point.t，不是数组下标', () => {
    // 采样有丢有补。下标当时间会让曲线在丢样的地方被悄悄压扁——看上去是速率
    // 平稳，实际是那段根本没采到。
    const sparse = pts([
      [0, 100],
      [1, 100],
      // 这里丢了 8 秒的样本
      [10, 900],
    ]);
    const { t0, t1, span } = timeSpan(sparse);
    expect(t0).toBe(0);
    expect(t1).toBe(10);
    expect(span).toBe(10);

    const line = polyline(
      sparse.map((p) => ({ t: p.t, v: p.rx_mbps })),
      t0,
      span,
      1000,
      100,
      50,
    );
    const xs = line.split(' ').map((pair) => Number(pair.split(',')[0]));
    // 用 t：第二个点在 10% 处；用下标的话会在 50% 处。
    expect(xs[1]).toBeCloseTo(10, 1);
    expect(xs[2]).toBeCloseTo(100, 1);
  });

  it('2. 点数上限是单一值 7200', () => {
    // 曾经是 7200 / 3600 / 600 三处不一致。
    expect(MAX_POINTS).toBe(7200);
    const many = pts(Array.from({ length: 8000 }, (_, i) => [i, i]));
    expect(appendPoints([], many)).toHaveLength(MAX_POINTS);
    // 丢的是最旧的那批。
    expect(appendPoints([], many)[0].t).toBe(8000 - MAX_POINTS);
  });

  it('3. 读数与曲线用同一批点', () => {
    // 三者取不同窗口时，读数和图对不上，而两边都「看起来没错」。
    const points = pts([
      [0, 100],
      [1, 300],
      [2, 200],
    ]);
    const r = readings(points, 'rx_mbps');
    expect(r.avg).toBeCloseTo(200);
    expect(r.peak).toBe(300);
    expect(r.last).toBe(200);
    expect(r.samples).toBe(points.length);
  });

  it('4. 按每像素列压 min/max，不抽稀——尖峰不许被漏掉', () => {
    // 抽稀（每 N 个取一个）会把尖峰整个漏掉，而尖峰恰恰是灌包测试要看的东西：
    // 「平均达标但中间掉过底」正是 RX_DROPOUT 在抓的事。
    const points = pts(Array.from({ length: 1000 }, (_, i) => [i, 500]));
    points[137].rx_mbps = 2400; // 一个尖峰
    points[642].rx_mbps = 5; // 一个深谷

    const reduced = reducePoints(points, 'rx_mbps', 0, 999, 100);
    const values = reduced.map((p) => p.v);
    expect(Math.max(...values), '尖峰被抽没了').toBe(2400);
    expect(Math.min(...values), '深谷被抽没了').toBe(5);
    // 压完之后点数应当远少于原始点数，否则等于没压。
    expect(reduced.length).toBeLessThan(points.length / 2);
  });
});

describe('reducePoints 的边界', () => {
  it('点数少时原样返回，不做无谓压缩', () => {
    const points = pts([
      [0, 1],
      [1, 2],
    ]);
    expect(reducePoints(points, 'rx_mbps', 0, 1, 100)).toEqual([
      { t: 0, v: 1 },
      { t: 1, v: 2 },
    ]);
  });

  it('span 为 0 或 cols 为 0 时不炸', () => {
    const points = pts([[0, 1]]);
    expect(() => reducePoints(points, 'rx_mbps', 0, 0, 0)).not.toThrow();
  });

  it('同一列内先出现的极值先画，折线不自己回头', () => {
    const points = pts(Array.from({ length: 100 }, (_, i) => [i, i % 2 === 0 ? 10 : 90]));
    const reduced = reducePoints(points, 'rx_mbps', 0, 99, 5);
    for (let i = 1; i < reduced.length; i += 1) {
      expect(reduced[i].t).toBeGreaterThanOrEqual(reduced[i - 1].t);
    }
  });
});

describe('坐标轴', () => {
  it('留 10% 余量并取到两位有效数字', () => {
    expect(axisMax(0)).toBe(100);
    // 90 * 1.1 在浮点下是 99.00000000000001，于是进位到 100——
    // 对坐标轴来说这反而是更好的那个数。
    expect(axisMax(90)).toBe(100);
    expect(axisMax(930)).toBe(1100);
    expect(axisMax(2400)).toBe(2700);
  });

  it('不会把曲线压扁到半个高度', () => {
    // 取一位有效数字会把 1023 抬到 2000——曲线只占半个高度，而这张图正是
    // 用来看波形起伏的。要求上界不超过峰值的 1.5 倍。
    for (const peak of [90, 137, 930, 1023, 2400, 9800]) {
      expect(axisMax(peak), `峰值 ${peak}`).toBeLessThanOrEqual(peak * 1.5);
      expect(axisMax(peak), `峰值 ${peak}`).toBeGreaterThanOrEqual(peak);
    }
  });

  it('空数据不产生折线', () => {
    expect(polyline([], 0, 1, 100, 100, 50)).toBe('');
    expect(polyline([{ t: 0, v: 1 }], 0, 1, 0, 100, 50)).toBe('');
  });
});

describe('环形缓冲', () => {
  it('空增量不改动原数组引用', () => {
    const before = pts([[0, 1]]);
    expect(appendPoints(before, [])).toBe(before);
  });

  it('未超上限时全部保留', () => {
    const merged = appendPoints(pts([[0, 1]]), pts([[1, 2]]));
    expect(merged).toHaveLength(2);
  });
});
