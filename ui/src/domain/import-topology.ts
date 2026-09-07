import type { NicInfo } from '../api/dto';
import type { UiPlan } from './plan-build';

/** null 表示尚未取得可信快照；空数组表示成功扫描但没有网卡。 */
export function reconcileImportedTopology(plan: UiPlan, master: NicInfo[] | null, agent: NicInfo[] | null) {
  function resolve(endpoint: string): string | false | null {
    const at = endpoint.indexOf(':');
    if (at < 0) return null; // 格式错误交给原有后端校验，不能猜成网口缺失。
    const side = endpoint.slice(0, at).trim().toLowerCase();
    const nics = ['master', 'local', '主控'].includes(side) ? master
      : ['agent', 'remote', '辅测'].includes(side) ? agent : null;
    if (nics === null) return null;
    const selector = endpoint.slice(at + 1).trim();
    const canonicalSide = ['master', 'local', '主控'].includes(side) ? 'master' : 'agent';
    if (selector.startsWith('NAME=') || selector.startsWith('name=')) {
      const name = selector.slice(5).trim().toLowerCase();
      const nic = nics.find((nic) => nic.name === selector.slice(5).trim())
        ?? nics.find((nic) => nic.name.toLowerCase() === name);
      return nic ? `${canonicalSide}:NAME=${nic.name}` : false;
    }
    const nic = nics.find((nic) => nic.role.toUpperCase() === selector.toUpperCase());
    return nic ? `${canonicalSide}:NAME=${nic.name}` : false;
  }

  let pending = 0;
  const notices: string[] = [];
  const removedSets = new Set<string>();
  const link_sets = plan.link_sets.map((set) => {
    const pair_refs = set.pair_refs.flatMap((pair) => {
      const src = resolve(pair.src);
      const dst = resolve(pair.dst);
      if (src === false || dst === false) {
        notices.push(`已移除「${set.name}」的网口对 ${pair.src} → ${pair.dst}：当前扫描中未找到${src === false ? pair.src : pair.dst}。`);
        return [];
      }
      if (src === null || dst === null) pending += 1;
      return [{ ...pair, src: src ?? pair.src, dst: dst ?? pair.dst }];
    });
    if (set.pair_refs.length > 0 && pair_refs.length === 0) removedSets.add(set.id);
    return { ...set, pair_refs };
  }).filter((set) => !removedSets.has(set.id));
  const byId = new Map(link_sets.map((set) => [set.id, new Set(set.pair_refs.map((pair) => pair.id))]));
  const bindings = plan.bindings.flatMap((binding) => {
    const valid = byId.get(binding.link_set_id);
    if (!valid) return [];
    if (binding.pair_ids.length === 0) return [binding];
    const pair_ids = binding.pair_ids.filter((id) => valid.has(id));
    // 空 pair_ids 表示整集合，不能把失效的显式子集误扩大成全部。
    return pair_ids.length ? [{ ...binding, pair_ids }] : [];
  });
  if (removedSets.size) notices.push(`已移除 ${removedSets.size} 个没有有效网口对的链路集合。`);
  const removedBindings = plan.bindings.length - bindings.length;
  if (removedBindings) notices.push(`已移除 ${removedBindings} 条失效的套件分配，套件和流量配置仍保留。`);
  return { plan: { ...plan, link_sets, bindings }, pending, notices };
}
