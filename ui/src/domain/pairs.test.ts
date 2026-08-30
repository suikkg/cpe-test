import { describe, expect, it } from 'vitest';
import type { NicInfo } from '../api/dto';
import {
  buildCandidates,
  formatEndpoint,
  groupByRole,
  matchesLinkFilter,
  parseEndpoint,
  roleKey,
} from './pairs';

function nic(name: string, role: string, ip = '192.168.1.2'): NicInfo {
  return {
    name,
    description: '',
    role,
    ipv4: ip,
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

/**
 * 这里承接 `the_link_set_panel_lists_every_combination_and_stays_editable_in_place`
 * 的义务（PLAN §7.3）。那条 Rust 测试靠 grep 手写 HTML 工作，产物换成 Vue
 * bundle 后失效——但它记录的**真实缺口**必须留下来，逐字搬在下面。
 */
describe('候选链路枚举', () => {
  const master = [nic('以太网 6', 'SGMII2.5G'), nic('以太网 7', 'SGMII1G')];
  const agent = [nic('WLAN 3', 'WIFI5G'), nic('以太网 2', 'SGMII2.5G')];

  it('默认列出全部组合，**含同机组合**', () => {
    // 原始理由（逐字搬运）：同机两块网口之间也是一条真实链路（走桥接/回环），
    // 把候选默认收窄成跨机，会让「我明明有这条链路，界面上找不到」变成第一
    // 印象。
    const pairs = buildCandidates(master, agent);
    expect(pairs).toHaveLength(6); // C(4,2)
    expect(pairs.filter((p) => !p.cross)).toHaveLength(2); // 主控 1 对 + 辅测 1 对
    expect(pairs.filter((p) => p.cross)).toHaveLength(4);
  });

  it('端点串的格式化与解析互为逆运算', () => {
    // 这个串是前端、编译器（validate.rs）、报表三处的共识。拼错一个字的表现是
    // 「预览时说端点不存在」，而界面上显示的名字完全正确。
    const ref = formatEndpoint('master', nic('以太网 6', 'SGMII2.5G'));
    expect(ref).toBe('master:NAME=以太网 6');
    expect(parseEndpoint(ref)).toEqual({ side: 'master', name: '以太网 6' });
    expect(parseEndpoint('乱写的')).toBeNull();
    expect(parseEndpoint('master:以太网')).toBeNull();
  });

  it('pair id 稳定：同一组网卡两次枚举得到同一批 id', () => {
    // id 进项目文件，界面重排或重连之后必须还认得出同一条链路。
    const a = buildCandidates(master, agent).map((p) => p.id);
    const b = buildCandidates(master, agent).map((p) => p.id);
    expect(a).toEqual(b);
  });
});

describe('筛选与自动分组用同一个谓词', () => {
  const master = [nic('以太网 6', 'SGMII2.5G'), nic('以太网 7', 'SGMII2.5G')];
  const agent = [nic('以太网 2', 'SGMII2.5G')];
  const pairs = buildCandidates(master, agent);

  it('自动分组跟着筛选走，不自己再过滤一遍', () => {
    // 原始理由（逐字搬运）：只在渲染处过滤、分组处写死 `pair.cross` 时，
    // 「点显示全部 → 再点按角色自动分组」生成的还是原来那批跨机集合，
    // 同机组合一条都不会被勾上——从用户角度就是这个按钮没反应。
    //
    // 这条现在从「靠测试发现不一致」升级成「结构上不可能」：
    // groupByRole 内部调的就是导出的 matchesLinkFilter。
    const all = groupByRole(pairs, 'all').flatMap((g) => g.pairs);
    const cross = groupByRole(pairs, 'cross').flatMap((g) => g.pairs);
    const same = groupByRole(pairs, 'same').flatMap((g) => g.pairs);

    expect(all).toHaveLength(pairs.length);
    expect(cross.every((p) => p.cross)).toBe(true);
    expect(same.every((p) => !p.cross)).toBe(true);
    expect(cross.length + same.length).toBe(all.length);

    // 分组结果必须与直接用谓词筛选完全一致。
    expect(new Set(all.map((p) => p.id))).toEqual(
      new Set(pairs.filter((p) => matchesLinkFilter(p, 'all')).map((p) => p.id)),
    );
  });

  it('角色键区分同机与跨机', () => {
    // 原始理由（逐字搬运）：同机与跨机可能落在同一对角色上，角色键必须能把
    // 两者分开，否则两类链路会被并进同一个自动集合。
    //
    // 这里三块网卡全是 SGMII2.5G：不带 cross 的话三对会挤进一个组，
    // 而同机那对的预期速率和跨机那两对完全不是一个量级。
    const sameHost = pairs.find((p) => !p.cross)!;
    const crossHost = pairs.find((p) => p.cross)!;
    expect(roleKey(sameHost)).not.toBe(roleKey(crossHost));
    expect(roleKey(sameHost)).toContain('same');
    expect(roleKey(crossHost)).toContain('cross');

    const groups = groupByRole(pairs, 'all');
    expect(groups.length).toBeGreaterThan(1);
  });

  it('角色键与两端顺序无关', () => {
    // A↔B 和 B↔A 是同一类链路，不该分成两组。
    const ab = buildCandidates([nic('a', 'SGMII2.5G')], [nic('b', 'WIFI5G')])[0];
    const ba = buildCandidates([nic('b', 'WIFI5G')], [nic('a', 'SGMII2.5G')])[0];
    expect(roleKey(ab)).toBe(roleKey(ba));
  });

  it('分组保序：按第一次出现的顺序排', () => {
    const groups = groupByRole(pairs, 'all');
    expect(groups.map((g) => g.key)).toEqual([...new Set(pairs.map(roleKey))]);
  });
});
