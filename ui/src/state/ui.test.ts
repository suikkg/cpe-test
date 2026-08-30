import { beforeEach, describe, expect, it } from 'vitest';
import { REGIONS, goto, reset, ui } from './ui';
import type { RegionId } from './ui';

describe('导航区域表', () => {
  beforeEach(reset);

  it('每个 RegionId 恰好有一条 REGIONS 记录', () => {
    // 这两处是分开写的：类型是给编译器看的，表是给渲染用的。少一条的后果不是
    // 报错，而是**导航栏里那个区域根本画不出来**——用户会以为功能没做。
    // 下面这个字面量数组是 RegionId 的穷举，加了新区域忘了进表，这里就红。
    const all: RegionId[] = ['local', 'agent', 'plan', 'run', 'progress', 'monitor', 'runs'];
    expect(REGIONS.map((r) => r.id).sort()).toEqual([...all].sort());
  });

  it('id 不重复', () => {
    // 重复 id 会让 v-for 的 :key 撞上，Vue 复用错节点，点 A 高亮 B。
    expect(new Set(REGIONS.map((r) => r.id)).size).toBe(REGIONS.length);
  });

  it('每个区域都有非空标签和合法分组', () => {
    for (const region of REGIONS) {
      expect(region.label.trim()).not.toBe('');
      expect(['flow', 'tool']).toContain(region.group);
    }
  });

  it('监控是独立工具，不在测试流程里', () => {
    // 监控和「一轮测试」正交：它在测试跑着的时候也能开，不该被排进流程序列。
    expect(REGIONS.find((r) => r.id === 'monitor')?.group).toBe('tool');
    expect(REGIONS.filter((r) => r.group === 'flow').map((r) => r.id)).toEqual([
      'local',
      'agent',
      'plan',
      'run',
      'progress',
    ]);
  });

  it('goto 切换区域，reset 回到起点', () => {
    // 模块级 reactive 是单例，用例之间会串味——每个 state 模块都得导出 reset()。
    goto('monitor');
    expect(ui.region).toBe('monitor');
    reset();
    expect(ui.region).toBe('local');
  });
});
