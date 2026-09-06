import { describe, expect, it } from 'vitest';
import { filterByQuery, matchesTerms, queryTerms, visibleCountLabel } from './search';

const rows = [
  { name: 'en0', desc: 'Ethernet', ip: '192.168.8.100', role: 'SGMII1G' },
  { name: 'en1', desc: 'Wi-Fi', ip: '192.168.8.104', role: 'WIFI5G' },
  { name: 'utun6', desc: '', ip: '10.0.0.1', role: 'UNKNOWN' },
];
const fields = (r: (typeof rows)[number]) => [r.name, r.desc, r.ip, r.role];

describe('页内搜索', () => {
  it('大小写不敏感的字面量匹配', () => {
    expect(filterByQuery(rows, 'wifi5g', fields).map((r) => r.name)).toEqual(['en1']);
    expect(filterByQuery(rows, 'ETHERNET', fields).map((r) => r.name)).toEqual(['en0']);
  });

  it('多个词各自都要命中（AND），不是任一命中', () => {
    expect(filterByQuery(rows, 'en 8.104', fields).map((r) => r.name)).toEqual(['en1']);
    // OR 的话这一条会把三行全留下。
    expect(filterByQuery(rows, 'en0 wifi', fields)).toEqual([]);
  });

  it('点号和冒号按普通字符处理，不当正则', () => {
    // 当正则的话 `.` 匹配任意字符，`192x168x8x100` 也会命中。
    expect(filterByQuery(rows, '192.168.8.100', fields).map((r) => r.name)).toEqual(['en0']);
    expect(filterByQuery([{ v: '192x168x8x100' }], '192.168.8.100', (r) => [r.v])).toEqual([]);
    // 冒号连写在正则里是语法错误，这里必须只是两个普通字符。
    expect(filterByQuery([{ v: 'fe80::1813' }], 'fe80::', (r) => [r.v])).toHaveLength(1);
  });

  it('空查询返回全部，并且保持原顺序', () => {
    expect(filterByQuery(rows, '   ', fields).map((r) => r.name)).toEqual(['en0', 'en1', 'utun6']);
    expect(queryTerms('  ')).toEqual([]);
  });

  it('过滤不重排：顺序永远沿用传进来的那一份', () => {
    const reversed = [...rows].reverse();
    expect(filterByQuery(reversed, 'en', fields).map((r) => r.name)).toEqual(['en1', 'en0']);
  });

  it('数字字段也能搜，空字段不参与', () => {
    expect(matchesTerms(['1000'], ['en0', 1000, null, undefined, ''])).toBe(true);
    expect(matchesTerms(['x'], [null, ''])).toBe(false);
  });

  it('计数只在真的过滤掉东西时才写成「显示 a / b」', () => {
    expect(visibleCountLabel(3, 3)).toBe('3 项');
    expect(visibleCountLabel(1, 3)).toBe('显示 1 / 3 项');
  });
});
