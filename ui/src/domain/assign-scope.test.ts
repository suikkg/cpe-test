import { describe, expect, it } from 'vitest';
import { linkSetSearchFields } from './grouping';
import type { Candidate } from './pairs';
import { isBound, toggleBinding, toggleSuiteColumn } from './plan-build';
import type { UiLinkSet, UiPlan } from './plan-build';
import { filterByQuery } from './search';

/**
 * 分配表加了名称查询之后，**批量操作的作用域**必须原样不动。
 *
 * 这一组守的是一件很容易在"顺手"里做错的事：搜出两行、点表头「全选此列」，
 * 如果那个按钮改成只作用于可见行，用户下次清空搜索会发现另外十几个集合没被
 * 分配——而他记得自己点过全选。方案 §11.1 因此把口径钉死：**整列作用于全部**，
 * 界面负责把这件事写出来，而不是把语义改掉。
 */

function nic(name: string, ipv4: string, role: string) {
  return { name, ipv4, role, description: '', gateway_v4: '', ipv6_ll: '', ipv6_global: '', zone: '', speed_mbps: 1000, ifindex: 0, is_wifi: false, wifi_band: '' };
}
function candidate(id: string, a: ReturnType<typeof nic>, b: ReturnType<typeof nic>): Candidate {
  return { id, src: `master:NAME=${a.name}`, dst: `agent:NAME=${b.name}`, srcNic: a, dstNic: b, cross: true } as Candidate;
}

const pairs = [
  candidate('p1', nic('en0', '192.168.8.100', 'SGMII1G'), nic('en0', '192.168.8.200', 'SGMII1G')),
  candidate('p2', nic('en1', '192.168.8.104', 'WIFI5G'), nic('en1', '192.168.8.204', 'WIFI5G')),
];
const sets: UiLinkSet[] = [
  { id: 's1', name: '有线 ↔ 有线', pair_refs: [{ id: 'p1', src: pairs[0].src, dst: pairs[0].dst }] },
  { id: 's2', name: '无线 ↔ 无线', pair_refs: [{ id: 'p2', src: pairs[1].src, dst: pairs[1].dst }] },
  { id: 's3', name: '导入进来的老集合', pair_refs: [{ id: 'gone', src: 'master:NAME=旧口', dst: 'agent:NAME=旧口' }] },
];
const index = new Map(pairs.map((p) => [p.id, p]));
const plan = (): UiPlan =>
  ({ ui_plan_version: 1, link_sets: sets, recipes: { tcp: [], udp: [], ping: [] }, suites: [{ id: 'suite-a', name: 'A', note: '', execution: 'sequential', order: [], tasks: [] }], bindings: [] }) as unknown as UiPlan;

const search = (q: string) => filterByQuery(sets, q, (set) => linkSetSearchFields(set, index));

describe('链路集合搜索', () => {
  it('搜集合名、成员网口名、IP、角色都命中', () => {
    expect(search('无线').map((s) => s.id)).toEqual(['s2']);
    expect(search('en0').map((s) => s.id)).toEqual(['s1']);
    expect(search('192.168.8.204').map((s) => s.id)).toEqual(['s2']);
    expect(search('wifi5g').map((s) => s.id)).toEqual(['s2']);
  });

  it('成员在候选表里查不到时，退回端点串本身，而不是把这一行抹掉', () => {
    // 导入的老项目、拓扑变过的集合都会走到这一条。搜不到可以，凭空消失不行。
    expect(search('旧口').map((s) => s.id)).toEqual(['s3']);
    expect(search('').map((s) => s.id)).toEqual(['s1', 's2', 's3']);
  });
});

describe('整列操作的作用域', () => {
  it('作用于**全部**集合，不受当前查询影响', () => {
    // 界面上此刻只显示 s2（搜了「无线」），但整列全选必须把三个都绑上。
    expect(search('无线')).toHaveLength(1);
    const after = toggleSuiteColumn(plan(), 'suite-a');
    expect(after.bindings.map((b) => b.link_set_id).sort()).toEqual(['s1', 's2', 's3']);
  });

  it('三态按全部集合算，不按可见行算', () => {
    // 只绑了 s2 时，即使查询让 s2 成为唯一可见行，整列也只能是「部分」。
    const some = toggleBinding(plan(), 's2', 'suite-a');
    const boundCount = sets.filter((set) => isBound(some, set.id, 'suite-a')).length;
    expect(boundCount).toBe(1);
    expect(boundCount).not.toBe(sets.length);
  });

  it('单行勾选只动那一行；清空查询不会撤销已勾的项', () => {
    const one = toggleBinding(plan(), 's2', 'suite-a');
    expect(one.bindings.map((b) => b.link_set_id)).toEqual(['s2']);
    // 查询只是显示层，绑定一个字节都不动。
    expect(search('').map((s) => s.id)).toEqual(['s1', 's2', 's3']);
    expect(one.bindings).toHaveLength(1);
  });
});
