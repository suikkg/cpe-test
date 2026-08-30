import type { Candidate, LinkFilter } from './pairs';
import { groupByRole, matchesLinkFilter } from './pairs';
import type { UiLinkSet, UiPairRef, UiPlan } from './plan-build';

/**
 * 链路集合与**当前拓扑**的对账。纯函数。
 *
 * 这是旧页 `syncQuickSets` 的领域函数化。那份实现和渲染、全局可变状态、
 * 筛选按钮的 DOM 状态缠在一起，改一处就要整读；而它管的事情——「网卡换了之后
 * 用户存下来的计划还算不算数」——是这个工具里最容易悄悄出错的地方之一。
 */

/** 自动生成的集合会被拓扑重建；用户手工编辑过的不会。 */
export interface ManagedLinkSet extends UiLinkSet {
  /** true = 由角色自动推导，可以被拓扑变化重建 */
  auto: boolean;
}

/** 失效引用：端点在当前拓扑里找不到了。 */
export interface StaleMark {
  setId: string;
  pairId: string;
  src: string;
  dst: string;
}

export interface ReconcileResult {
  linkSets: ManagedLinkSet[];
  /** 手工集合里失效的对——**只提示，不删**，见下面的理由 */
  stale: StaleMark[];
  /** 因为拓扑不可信而整体跳过了对账 */
  skipped: boolean;
}

function pairKey(src: string, dst: string): string {
  // 方向无关：A→B 与 B→A 是同一条物理链路。
  return [src, dst].sort().join('|');
}

/**
 * 按当前拓扑对账链路集合。
 *
 * 三条规则，每一条都对应一个真实踩过的坑：
 *
 * 1. **拓扑不可信时整体跳过。** 项目可以在**没连上辅测机**的时候导入，那时
 *    端点表是空的；连接成功的响应里也可能是空网卡表（对端正在启动，或者
 *    IPv4 前缀过滤把网卡全滤掉了）。这两种情况下"对账"会把每个自动集合都
 *    清空、把每条手工引用都标成失效——**而实际上什么都没观测到**。
 *
 * 2. **自动集合跟着拓扑走，手工集合只标记不删。** 自动集合完全由角色推导，
 *    网口短暂消失时丢掉旧引用、下次扫描补回即可；用户手工编辑过的集合是他的
 *    资产，失效了要提示他，但不能替他删——删掉之后他连"原来这里有什么"都
 *    看不到了。
 *
 * 3. **有绑定的空自动集合要保留。** 角色/名称可能只是短暂变化。这里直接删掉
 *    集合的话，指向它的 binding 会跟着被清掉，等同角色链路恢复时虽然会按相同
 *    id 重建集合，用户仍得重新分配套件——离线项目就被悄悄改写了。留一个 0 对
 *    的卡片也比丢绑定安全。
 */
export function reconcileLinkSets(
  linkSets: ManagedLinkSet[],
  candidates: Candidate[],
  filter: LinkFilter,
  boundSetIds: ReadonlySet<string>,
): ReconcileResult {
  // 规则 1：没有可信拓扑就什么都不动。
  if (candidates.length === 0) {
    return { linkSets, stale: [], skipped: true };
  }

  const byKey = new Map<string, Candidate>();
  for (const pair of candidates) {
    byKey.set(pairKey(pair.src, pair.dst), pair);
  }

  // 一个集合都没有：按角色自动建一批。
  if (linkSets.length === 0) {
    return { linkSets: autoLinkSets(candidates, filter), stale: [], skipped: false };
  }

  const stale: StaleMark[] = [];
  const seen = new Set<string>();

  const reconciled: ManagedLinkSet[] = linkSets.map((set) => {
    const refs: UiPairRef[] = [];
    for (const ref of set.pair_refs ?? []) {
      const found = byKey.get(pairKey(ref.src, ref.dst));
      if (!found) {
        // 规则 2：自动集合丢掉旧引用；手工集合标失效但保留。
        if (!set.auto) {
          refs.push(ref);
          stale.push({ setId: set.id, pairId: ref.id, src: ref.src, dst: ref.dst });
        }
        continue;
      }
      // 自动集合跟着筛选走；手工集合不受筛选影响。
      if (set.auto && !matchesLinkFilter(found, filter)) continue;
      refs.push({ id: found.id, src: found.src, dst: found.dst });
      seen.add(pairKey(found.src, found.dst));
    }
    return { ...set, pair_refs: refs };
  });

  // 新扫到的链路并进同角色的自动集合；自定义集合不被悄悄改名或塞东西。
  for (const group of groupByRole(candidates, filter)) {
    for (const pair of group.pairs) {
      if (seen.has(pairKey(pair.src, pair.dst))) continue;
      let set = reconciled.find((item) => item.auto && item.name === group.label);
      if (!set) {
        set = { id: `linkset-${group.key}`, name: group.label, pair_refs: [], auto: true };
        reconciled.push(set);
      }
      set.pair_refs.push({ id: pair.id, src: pair.src, dst: pair.dst });
      seen.add(pairKey(pair.src, pair.dst));
    }
  }

  // 规则 3：空的自动集合只在**没人绑定**时才清掉。
  const kept = reconciled.filter(
    (set) => !set.auto || set.pair_refs.length > 0 || boundSetIds.has(set.id),
  );
  return { linkSets: kept, stale, skipped: false };
}

/** 按角色键自动建一批集合。 */
export function autoLinkSets(candidates: Candidate[], filter: LinkFilter): ManagedLinkSet[] {
  return groupByRole(candidates, filter).map((group) => ({
    id: `linkset-${group.key}`,
    name: group.label,
    pair_refs: group.pairs.map((pair) => ({ id: pair.id, src: pair.src, dst: pair.dst })),
    auto: true,
  }));
}

/** 清掉指向已不存在集合的绑定。 */
export function pruneBindings(plan: UiPlan, linkSets: { id: string }[]): UiPlan {
  const valid = new Set(linkSets.map((set) => set.id));
  const suiteIds = new Set(plan.suites.map((suite) => suite.id));
  return {
    ...plan,
    bindings: plan.bindings.filter(
      (binding) => valid.has(binding.link_set_id) && suiteIds.has(binding.suite_id),
    ),
  };
}
