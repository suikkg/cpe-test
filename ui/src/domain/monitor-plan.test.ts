import { describe, expect, it } from 'vitest';
import { isMonitored, MONITOR_MAX_SESSIONS, pendingStarts } from './monitor-plan';

const running = [
  { side: 'master' as const, iface: 'eth0' },
  { side: 'agent' as const, iface: 'eth1' },
];

describe('同一块网卡不许开两路', () => {
  it('键必须带上端：两端可能都有一块叫 eth0 的网卡', () => {
    expect(isMonitored(running, 'master', 'eth0')).toBe(true);
    expect(isMonitored(running, 'agent', 'eth0')).toBe(false);
  });

  it('没开过的返回 false', () => {
    expect(isMonitored(running, 'master', 'wlan0')).toBe(false);
  });
});

describe('全部开始', () => {
  it('只开还没开的那些', () => {
    expect(pendingStarts(running, 'master', ['eth0', 'eth1', 'wlan0'])).toEqual(['eth1', 'wlan0']);
  });

  it('去重，且跳过空名字', () => {
    expect(pendingStarts([], 'master', ['eth0', 'eth0', '', 'eth1'])).toEqual(['eth0', 'eth1']);
  });

  it('被总上限截断——服务端是逐个请求拒的，那样会先成功几路再连着报错', () => {
    const many = Array.from({ length: 20 }, (_, i) => `eth${i}`);
    expect(pendingStarts([], 'master', many)).toHaveLength(MONITOR_MAX_SESSIONS);
    // 已经开了 6 路时只剩 2 个名额。
    const busy = Array.from({ length: 6 }, (_, i) => ({
      side: 'agent' as const,
      iface: `a${i}`,
    }));
    expect(pendingStarts(busy, 'master', many)).toHaveLength(2);
  });

  it('满了就一个都不开', () => {
    const busy = Array.from({ length: MONITOR_MAX_SESSIONS }, (_, i) => ({
      side: 'agent' as const,
      iface: `a${i}`,
    }));
    expect(pendingStarts(busy, 'master', ['eth0'])).toEqual([]);
  });
});
