import { describe, expect, it } from 'vitest';
import { bindingSelectionState, emptyPlan, ensureDefaults, toggleBinding, type UiPlan } from './plan-build';
import { assignedPairIds, selectedPortPairs, setPairAssigned, setPairsAssigned } from './plan-ports';

function fixture(): UiPlan {
  return {
    ...ensureDefaults(emptyPlan()),
    link_sets: [{ id: 'ports', name: '网口', pair_refs: [
      { id: 'p1', src: 'master:NAME=A', dst: 'agent:NAME=B' },
      { id: 'p2', src: 'agent:NAME=C', dst: 'master:NAME=A' },
    ] }],
    bindings: [{ id: 'original', link_set_id: 'ports', suite_id: 'suite-baseline', pair_ids: [], mode: 'replace' }],
  };
}

describe('逐网口分配', () => {
  it('整集合分配取消一对后只保留另一对，并保留端点方向与绑定身份', () => {
    const original = fixture();
    const changed = setPairAssigned(original, 'ports', 'p1', 'suite-baseline', false);
    expect(changed.bindings[0]).toEqual({ ...original.bindings[0], pair_ids: ['p2'] });
    expect(changed.link_sets).toEqual(original.link_sets);
    expect(original.bindings[0].pair_ids).toEqual([]);
  });
  it('取消最后一对删除绑定，不会意外变成全选', () => {
    let plan = setPairAssigned(fixture(), 'ports', 'p1', 'suite-baseline', false);
    plan = setPairAssigned(plan, 'ports', 'p2', 'suite-baseline', false);
    expect(plan.bindings).toEqual([]);
    expect(selectedPortPairs(plan)).toBe(0);
  });
  it('选择一对不会连带选择同集合其它网口或后来扫描到的网口', () => {
    const plan = fixture(); plan.bindings = [];
    const changed = setPairAssigned(plan, 'ports', 'p2', 'suite-baseline', true);
    changed.link_sets[0].pair_refs.push({ id: 'p3', src: 'master:NAME=D', dst: 'agent:NAME=E' });
    expect([...assignedPairIds(changed, 'ports', 'suite-baseline')]).toEqual(['p2']);
  });
  it('只改当前套件，保留其它分配及原执行顺序', () => {
    const plan = fixture();
    plan.suites.push({ ...plan.suites[0], id: 'other' });
    plan.bindings.unshift({ ...plan.bindings[0], id: 'other-binding', suite_id: 'other' });
    const changed = setPairAssigned(plan, 'ports', 'p1', 'suite-baseline', false);
    expect(changed.bindings.map((binding) => binding.id)).toEqual(['other-binding', 'original']);
    expect(changed.bindings[0]).toEqual(plan.bindings[0]);
    expect(selectedPortPairs(changed)).toBe(2);
  });
  it('忽略不存在的网口和套件，重复操作保持幂等', () => {
    const plan = fixture();
    expect(setPairAssigned(plan, 'ports', 'missing', 'suite-baseline', true)).toBe(plan);
    expect(setPairAssigned(plan, 'ports', 'p1', 'missing', true)).toBe(plan);
    const once = setPairAssigned(plan, 'ports', 'p1', 'suite-baseline', false);
    expect(setPairAssigned(once, 'ports', 'p1', 'suite-baseline', false)).toEqual(once);
  });
});

describe('分组网口表：整组与逐网口', () => {
  const rows = [{ setId: 'ports', pairId: 'p1' }, { setId: 'ports', pairId: 'p2' }];

  it('批量全选不会把整组分配改写成显式清单', () => {
    const plan = fixture();
    expect(setPairsAssigned(plan, rows, 'suite-baseline', true)).toBe(plan);
    expect(plan.bindings[0].pair_ids).toEqual([]);
  });

  it('组复选框：无 → 整组；取消一对 → 部分；再点组复选框 → 补回整组且保留绑定身份', () => {
    let plan = fixture();
    plan.bindings = [];
    plan = toggleBinding(plan, 'ports', 'suite-baseline');
    expect(plan.bindings).toHaveLength(1);
    expect(plan.bindings[0].pair_ids).toEqual([]);
    const id = plan.bindings[0].id;

    plan = setPairAssigned(plan, 'ports', 'p1', 'suite-baseline', false);
    expect(bindingSelectionState(plan, 'ports', 'suite-baseline')).toBe('some');

    plan = toggleBinding(plan, 'ports', 'suite-baseline');
    expect(plan.bindings).toEqual([expect.objectContaining({ id, pair_ids: [] })]);
    expect(bindingSelectionState(plan, 'ports', 'suite-baseline')).toBe('all');
  });

  it('批量取消只动显示的行，其余网口保持分配', () => {
    const plan = setPairsAssigned(fixture(), [rows[0]], 'suite-baseline', false);
    expect([...assignedPairIds(plan, 'ports', 'suite-baseline')]).toEqual(['p2']);
  });
});
