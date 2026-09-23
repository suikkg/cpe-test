import { uniqueId, type UiPlan } from './plan-build';

/** 空 pair_ids 表示整个集合；取消最后一个网口必须删除绑定，不能写回空数组。 */
export function assignedPairIds(plan: UiPlan, setId: string, suiteId: string): Set<string> {
  const ids = plan.link_sets.find((set) => set.id === setId)?.pair_refs.map((pair) => pair.id) ?? [];
  const valid = new Set(ids);
  return new Set(plan.bindings
    .filter((binding) => binding.link_set_id === setId && binding.suite_id === suiteId)
    .flatMap((binding) => binding.pair_ids.length ? binding.pair_ids : ids)
    .filter((id) => valid.has(id)));
}

/** 逐网口编辑既有分配，保留集合、端点方向、其它套件和绑定顺序。 */
export function setPairAssigned(
  plan: UiPlan, setId: string, pairId: string, suiteId: string, selected: boolean,
): UiPlan {
  const set = plan.link_sets.find((item) => item.id === setId);
  if (!set?.pair_refs.some((pair) => pair.id === pairId)
    || !plan.suites.some((suite) => suite.id === suiteId)) return plan;
  const ids = assignedPairIds(plan, setId, suiteId);
  if (selected) ids.add(pairId);
  else ids.delete(pairId);
  const matches = (binding: UiPlan['bindings'][number]) =>
    binding.link_set_id === setId && binding.suite_id === suiteId;
  const original = plan.bindings.find(matches);
  const replacement = ids.size ? {
    id: original?.id ?? uniqueId('binding-port', plan.bindings.map((binding) => binding.id)),
    link_set_id: setId, suite_id: suiteId,
    // 显式选择只包含眼前选过的网口，扫描新增网口不能悄悄加入测试。
    pair_ids: set.pair_refs.filter((pair) => ids.has(pair.id)).map((pair) => pair.id),
    mode: original?.mode ?? 'replace',
  } : null;
  let inserted = false;
  const bindings = plan.bindings.flatMap((binding) => {
    if (!matches(binding)) return [binding];
    if (inserted || !replacement) return [];
    inserted = true;
    return [replacement];
  });
  if (!original && replacement) bindings.push(replacement);
  return { ...plan, bindings };
}

export function selectedPortPairs(plan: UiPlan): number {
  return plan.link_sets.reduce((total, set) => {
    const selected = new Set(plan.suites.flatMap((suite) =>
      [...assignedPairIds(plan, set.id, suite.id)]));
    return total + selected.size;
  }, 0);
}
