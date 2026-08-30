import type { NicInfo } from '../api/dto';

/**
 * 候选链路的枚举、筛选与分组。**纯函数**，不碰 vue / 网络 / DOM
 * （由 `scripts/lint-arch.mjs` 挡着）。
 *
 * 旧页出过的 UI bug 全部是这一层的纯逻辑 bug——角色键忽略 `pair.cross`、
 * 筛选和自动分组各写一份谓词——所以把它们赶进一个能用普通 Vitest 直测的
 * 目录，比挂载组件断言 DOM 结实得多。
 */

/** 端点串。三处共识的格式：`master:NAME=<接口名>` / `agent:NAME=<接口名>`。 */
export type EndpointRef = string;

export interface Candidate {
  /** 稳定 id：由两个端点串派生，重排界面不会改 */
  id: string;
  src: EndpointRef;
  dst: EndpointRef;
  srcNic: NicInfo;
  dstNic: NicInfo;
  /** 跨机（主控 ↔ 辅测）还是同机（桥接/回环） */
  cross: boolean;
}

export type LinkFilter = 'all' | 'cross' | 'same';

/**
 * 端点串的**唯一**格式化出口。
 *
 * 这个串是前端、编译器（`webui/validate.rs`）、报表三处的共识；拼错一个字
 * 的表现是「预览时说端点不存在」，而用户在界面上看到的名字完全正确。
 */
export function formatEndpoint(side: 'master' | 'agent', nic: NicInfo): EndpointRef {
  return `${side}:NAME=${nic.name}`;
}

/** 端点串的唯一解析出口，与 `formatEndpoint` 互为逆运算。 */
export function parseEndpoint(ref: EndpointRef): { side: string; name: string } | null {
  const at = ref.indexOf(':');
  if (at < 0) return null;
  const side = ref.slice(0, at);
  const rest = ref.slice(at + 1);
  if (!rest.startsWith('NAME=')) return null;
  return { side, name: rest.slice('NAME='.length) };
}

/**
 * 列出全部候选链路：**N(N−1)/2 的两两组合，含同机组合**。
 *
 * 同机两块网口之间也是一条真实链路（走桥接/回环）。把候选默认收窄成跨机，
 * 会让「我明明有这条链路，界面上找不到」变成用户的第一印象——这是旧页
 * `the_link_set_panel_lists_every_combination_and_stays_editable_in_place`
 * 记录下来的真实缺口，迁移过来的义务。
 */
export function buildCandidates(master: NicInfo[], agent: NicInfo[]): Candidate[] {
  const all: Array<{ side: 'master' | 'agent'; nic: NicInfo }> = [
    ...master.map((nic) => ({ side: 'master' as const, nic })),
    ...agent.map((nic) => ({ side: 'agent' as const, nic })),
  ];
  const out: Candidate[] = [];
  for (let i = 0; i < all.length; i += 1) {
    for (let j = i + 1; j < all.length; j += 1) {
      const a = all[i];
      const b = all[j];
      const src = formatEndpoint(a.side, a.nic);
      const dst = formatEndpoint(b.side, b.nic);
      out.push({
        id: `pair-${src}->${dst}`,
        src,
        dst,
        srcNic: a.nic,
        dstNic: b.nic,
        cross: a.side !== b.side,
      });
    }
  }
  return out;
}

/**
 * 筛选谓词。**导出一份，筛选和自动分组共用。**
 *
 * 旧页在渲染处过滤、在分组处写死 `pair.cross`，于是「点显示全部 → 再点按角色
 * 自动分组」生成的还是原来那批跨机集合，同机组合一条都不会被勾上——从用户
 * 角度就是这个按钮没反应。把它导出成唯一的一份，是把「两套谓词漂移」从
 * 测试问题升级成**结构上不可能**。
 */
export function matchesLinkFilter(pair: Candidate, filter: LinkFilter): boolean {
  if (filter === 'cross') return pair.cross;
  if (filter === 'same') return !pair.cross;
  return true;
}

/**
 * 自动分组用的角色键。
 *
 * **必须带上 `cross`**：同机与跨机可能落在同一对角色上（比如主控和辅测各有
 * 一块 SGMII2.5G，主控自己也有两块），不区分就会把两类链路并进同一个自动
 * 集合，而那两类的预期速率完全不是一个量级。
 */
export function roleKey(pair: Candidate): string {
  const roles = [pair.srcNic.role || 'UNKNOWN', pair.dstNic.role || 'UNKNOWN'].sort();
  return `${pair.cross ? 'cross' : 'same'}|${roles[0]}↔${roles[1]}`;
}

/** 按角色键自动分组，保序（第一次出现的顺序）。 */
export function groupByRole(
  pairs: Candidate[],
  filter: LinkFilter,
): Array<{ key: string; label: string; pairs: Candidate[] }> {
  const order: string[] = [];
  const buckets = new Map<string, Candidate[]>();
  // 分组必须跟着筛选走，不能自己再过滤一遍——那正是「按钮没反应」的根因。
  for (const pair of pairs.filter((p) => matchesLinkFilter(p, filter))) {
    const key = roleKey(pair);
    if (!buckets.has(key)) {
      buckets.set(key, []);
      order.push(key);
    }
    buckets.get(key)!.push(pair);
  }
  return order.map((key) => {
    const [scope, roles] = key.split('|');
    return {
      key,
      label: `${roles}${scope === 'same' ? '（同机）' : ''}`,
      pairs: buckets.get(key)!,
    };
  });
}
