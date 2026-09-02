import { describe, expect, it } from 'vitest';
import { parseRunRequest } from './rerun';

/**
 * 「重新执行历史运行」读回的是 `runs/<run>/request.json`。
 *
 * 这一层最要紧的一条是**丢掉上一轮的 `plan_hash`**：带着它回到编辑态，下一次
 * 提交就可能拿一个没有人复核过的哈希去开跑，而 `plan_hash` 正是「界面上确认的
 * 东西 == 实际跑的东西」唯一的强制点。
 */
describe('parseRunRequest', () => {
  const full = {
    duration: 300,
    resume: true,
    screenshot: true,
    limit_udp_by_link_speed: true,
    tcp_windows: ['4m'],
    tcp_streams: [1, 10],
    udp_bandwidths: ['2500m'],
    udp_lengths: ['14k'],
    udp_windows: ['256m'],
    udp_streams: 2,
    ping_count: 5,
    ping_payload_sizes: [32],
    ping_max_rtt_ms: 12.5,
    nic_policies: [
      { endpoint: 'master:NAME=eth0', rx_target: '90%', udp_bandwidth: '', udp_length: '' },
      { rx_target: '1800' },
    ],
    plan_hash: 'stale-hash',
    ui_plan: {
      ui_plan_version: 1,
      link_sets: [{ id: 'ls', name: 'L', pair_refs: [] }],
      recipes: { tcp: [], udp: [], ping: [] },
      suites: [],
      bindings: [],
      plan_hash: 'stale-hash',
    },
  };

  it('把执行区那几项原样读回来', () => {
    const snapshot = parseRunRequest(full)!;
    expect(snapshot.duration).toBe(300);
    expect(snapshot.screenshot).toBe(true);
    expect(snapshot.limitUdpByLinkSpeed).toBe(true);
    expect(snapshot.globals.tcp_streams).toEqual([1, 10]);
    expect(snapshot.globals.udp_streams).toBe(2);
    expect(snapshot.globals.ping_payload_sizes).toEqual([32]);
    expect(snapshot.globals.ping_wired_small_max_rtt_ms).toBe(12.5);
  });

  it('丢掉计划里的 plan_hash：复核必须重新走一遍', () => {
    const snapshot = parseRunRequest(full)!;
    expect(snapshot.plan).not.toBeNull();
    expect(snapshot.plan as unknown as Record<string, unknown>).not.toHaveProperty('plan_hash');
  });

  it('没有 endpoint 的网卡策略直接丢掉——它谁也指不到', () => {
    expect(parseRunRequest(full)!.nicPolicies).toHaveLength(1);
  });

  it('空计划补出厂默认，缺字段用兜底值', () => {
    const snapshot = parseRunRequest({})!;
    expect(snapshot.duration).toBe(180);
    expect(snapshot.resume).toBe(false);
    expect(snapshot.plan).toBeNull();
    expect(snapshot.globals.tcp_windows).toEqual([]);
    expect(snapshot.globals.ping_wired_small_max_rtt_ms).toBe(0);
  });

  it('不是对象就读不出来', () => {
    expect(parseRunRequest(null)).toBeNull();
    expect(parseRunRequest('{}')).toBeNull();
  });

  it('矩阵路径跑出来的旧目录没有 ui_plan，照样能读出执行区参数', () => {
    const snapshot = parseRunRequest({ duration: 60, pairs: [] })!;
    expect(snapshot.plan).toBeNull();
    expect(snapshot.duration).toBe(60);
  });
});
