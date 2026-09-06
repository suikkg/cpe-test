import { describe, expect, it } from 'vitest';
import { clockStamp, freshnessLabel } from './freshness';

describe('数据新鲜度', () => {
  it('时刻补零到 HH:MM:SS', () => {
    expect(clockStamp(new Date(2026, 8, 6, 9, 5, 3))).toBe('09:05:03');
    expect(clockStamp(new Date(2026, 8, 6, 23, 59, 59))).toBe('23:59:59');
  });

  it('从来没成功过时说「尚未同步」，不回落成当前时间', () => {
    // 这条是这个模块存在的全部理由：回落成 now() 会把「从没同步过」
    // 显示成「刚同步过」，正好藏起用户最需要察觉的那一种。
    expect(freshnessLabel(null)).toBe('尚未同步');
    expect(freshnessLabel(null)).not.toMatch(/\d{2}:\d{2}:\d{2}/);
  });

  it('有时刻就说那个时刻', () => {
    expect(freshnessLabel(new Date(2026, 8, 6, 1, 2, 3).getTime())).toBe('01:02:03 更新');
  });
});
