import { describe, expect, it } from 'vitest';
import type { NicInfo } from '../api/dto';
import { autoLinkSets, pruneBindings, reconcileLinkSets, type ManagedLinkSet } from './grouping';
import { buildCandidates } from './pairs';
import { emptyPlan, ensureDefaults, toggleSuiteColumn } from './plan-build';

function nic(name: string, role: string): NicInfo {
  return {
    name,
    description: '',
    role,
    ipv4: '192.168.1.2',
    gateway_v4: '',
    ipv6_ll: '',
    ipv6_global: '',
    zone: '',
    speed_mbps: 2500,
    is_wifi: false,
    wifi_band: '',
    ifindex: 1,
  };
}

const master = [nic('以太网 6', 'SGMII2.5G')];
const agent = [nic('WLAN 3', 'WIFI5G'), nic('以太网 2', 'SGMII2.5G')];
const candidates = buildCandidates(master, agent);

describe('拓扑对账', () => {
  it('拓扑不可信时整体跳过，什么都不动', () => {
    // 项目可以在**没连上辅测机**的时候导入，那时端点表是空的；连接成功的
    // 响应里也可能是空网卡表（对端正在启动，或者 IPv4 前缀过滤把网卡全滤掉）。
    // 这两种情况下"对账"会把每个自动集合都清空、把每条手工引用都标成失效——
    // 而实际上什么都没观测到。
    const saved: ManagedLinkSet[] = [
      {
        id: 'set-mine',
        name: '我的集合',
        auto: false,
        pair_refs: [{ id: 'p1', src: 'master:NAME=不存在', dst: 'agent:NAME=也不存在' }],
      },
    ];
    const result = reconcileLinkSets(saved, [], 'all', new Set());
    expect(result.skipped).toBe(true);
    expect(result.linkSets).toBe(saved);
    expect(result.stale).toHaveLength(0);
  });

  it('一个集合都没有时按角色自动建一批', () => {
    const result = reconcileLinkSets([], candidates, 'all', new Set());
    expect(result.skipped).toBe(false);
    expect(result.linkSets.length).toBeGreaterThan(0);
    expect(result.linkSets.every((s) => s.auto)).toBe(true);
    // 每条候选链路都要落进某个集合，一条都不能漏。
    const covered = result.linkSets.flatMap((s) => s.pair_refs.map((r) => r.id));
    expect(new Set(covered).size).toBe(candidates.length);
  });

  it('手工集合里的失效对**只标记不删**', () => {
    // 用户手工编辑过的集合是他的资产。失效了要提示他，但不能替他删——
    // 删掉之后他连"原来这里有什么"都看不到了。
    const saved: ManagedLinkSet[] = [
      {
        id: 'set-mine',
        name: '我的集合',
        auto: false,
        pair_refs: [
          { id: 'gone', src: 'master:NAME=拔掉了', dst: 'agent:NAME=也拔掉了' },
          { id: candidates[0].id, src: candidates[0].src, dst: candidates[0].dst },
        ],
      },
    ];
    const result = reconcileLinkSets(saved, candidates, 'all', new Set());
    const mine = result.linkSets.find((s) => s.id === 'set-mine')!;
    expect(mine.pair_refs).toHaveLength(2);
    expect(result.stale).toHaveLength(1);
    expect(result.stale[0].pairId).toBe('gone');
  });

  it('自动集合里的失效对直接丢掉，不标失效', () => {
    // 自动集合完全由角色推导，网口短暂消失时丢掉旧引用、下次扫描补回即可。
    // 把自动集合也标成失效会阻塞预览，而用户根本没编辑过它。
    const saved: ManagedLinkSet[] = [
      {
        id: 'auto-1',
        name: '自动',
        auto: true,
        pair_refs: [{ id: 'gone', src: 'master:NAME=拔掉了', dst: 'agent:NAME=也拔掉了' }],
      },
    ];
    const result = reconcileLinkSets(saved, candidates, 'all', new Set());
    expect(result.stale).toHaveLength(0);
  });

  it('有绑定的空自动集合要保留', () => {
    // 角色/名称可能只是短暂变化。直接删掉集合的话，指向它的 binding 会跟着
    // 被清掉；等同角色链路恢复时虽然会按相同 id 重建集合，用户仍得重新分配
    // 套件——离线项目就被悄悄改写了。留一个 0 对的卡片也比丢绑定安全。
    const saved: ManagedLinkSet[] = [
      { id: 'auto-bound', name: '被绑定的自动集合', auto: true, pair_refs: [] },
      { id: 'auto-free', name: '没人要的自动集合', auto: true, pair_refs: [] },
    ];
    const result = reconcileLinkSets(saved, candidates, 'all', new Set(['auto-bound']));
    const ids = result.linkSets.map((s) => s.id);
    expect(ids).toContain('auto-bound');
    expect(ids).not.toContain('auto-free');
  });

  it('自动集合跟着筛选走，手工集合不受筛选影响', () => {
    const sameHost = buildCandidates(
      [nic('a', 'SGMII2.5G'), nic('b', 'SGMII2.5G')],
      [nic('c', 'WIFI5G')],
    );
    const same = sameHost.find((p) => !p.cross)!;
    const saved: ManagedLinkSet[] = [
      { id: 'auto-1', name: 'x', auto: true, pair_refs: [{ id: same.id, src: same.src, dst: same.dst }] },
      { id: 'mine', name: 'y', auto: false, pair_refs: [{ id: same.id, src: same.src, dst: same.dst }] },
    ];
    const result = reconcileLinkSets(saved, sameHost, 'cross', new Set(['auto-1', 'mine']));
    const auto = result.linkSets.find((s) => s.id === 'auto-1')!;
    const mine = result.linkSets.find((s) => s.id === 'mine')!;
    expect(auto.pair_refs.some((r) => r.id === same.id)).toBe(false);
    expect(mine.pair_refs.some((r) => r.id === same.id)).toBe(true);
  });

  it('新扫到的链路并进同角色的自动集合', () => {
    const partial = autoLinkSets(candidates.slice(0, 1), 'all');
    const result = reconcileLinkSets(partial, candidates, 'all', new Set());
    const covered = result.linkSets.flatMap((s) => s.pair_refs.map((r) => r.id));
    expect(new Set(covered).size).toBe(candidates.length);
  });

  it('对账是幂等的：连跑两次结果一样', () => {
    const once = reconcileLinkSets([], candidates, 'all', new Set());
    const twice = reconcileLinkSets(once.linkSets, candidates, 'all', new Set());
    expect(twice.linkSets.map((s) => ({ id: s.id, n: s.pair_refs.length }))).toEqual(
      once.linkSets.map((s) => ({ id: s.id, n: s.pair_refs.length })),
    );
  });
});

describe('绑定清理', () => {
  it('清掉指向已不存在集合的绑定', () => {
    let plan = ensureDefaults(emptyPlan());
    plan.link_sets = [{ id: 'set-a', name: 'A', pair_refs: [] }];
    plan = toggleSuiteColumn(plan, plan.suites[0].id);
    expect(plan.bindings).toHaveLength(1);

    const pruned = pruneBindings(plan, []);
    expect(pruned.bindings).toHaveLength(0);
  });

  it('集合还在就不动绑定', () => {
    let plan = ensureDefaults(emptyPlan());
    plan.link_sets = [{ id: 'set-a', name: 'A', pair_refs: [] }];
    plan = toggleSuiteColumn(plan, plan.suites[0].id);
    expect(pruneBindings(plan, [{ id: 'set-a' }]).bindings).toHaveLength(1);
  });
});
