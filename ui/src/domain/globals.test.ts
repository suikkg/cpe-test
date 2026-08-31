import { describe, expect, it } from 'vitest';
import {
  activeNicPolicies,
  emptyGlobals,
  formatNumberList,
  formatTokenList,
  globalsAreEmpty,
  parseNumberList,
  parseTokenList,
  policyFor,
  setNicPolicy,
  type UiNicPolicy,
} from './globals';

describe('档位串', () => {
  it('收逗号、中文逗号、顿号和空白', () => {
    expect(parseTokenList('4m, 64k、256m　1m')).toEqual(['4m', '64k', '256m', '1m']);
  });

  it('空串与全分隔符给空数组——空在后端有确定语义（沿用配置）', () => {
    expect(parseTokenList('')).toEqual([]);
    expect(parseTokenList('  , ,  ')).toEqual([]);
  });

  it('往返回来还是同一串', () => {
    expect(formatTokenList(parseTokenList('4m,64k'))).toBe('4m, 64k');
  });

  it('数字档位丢掉 0 与非数字：0 流 / 0 包长都不是合法档位', () => {
    expect(parseNumberList('1, 0, 10, abc, -3, 2.9')).toEqual([1, 10, 2]);
    expect(formatNumberList([1, 10])).toBe('1, 10');
  });
});

describe('全局默认档位', () => {
  it('新建的一份是全空的——「留空 = 沿用主控 config.json」', () => {
    expect(globalsAreEmpty(emptyGlobals())).toBe(true);
  });

  it('任意一项被填就不再算空', () => {
    const globals = emptyGlobals();
    globals.udp_streams = 4;
    expect(globalsAreEmpty(globals)).toBe(false);
  });
});

describe('按网卡策略', () => {
  const endpoint = 'master:NAME=以太网 6';

  it('读一个没设过的端点不产生副作用', () => {
    const list: UiNicPolicy[] = [];
    expect(policyFor(list, endpoint).rx_target).toBe('');
    expect(list).toHaveLength(0);
  });

  it('填进去再改一项，其余项保留', () => {
    let list = setNicPolicy([], endpoint, { rx_target: '90%' });
    list = setNicPolicy(list, endpoint, { udp_bandwidth: '1000m' });
    expect(policyFor(list, endpoint)).toEqual({
      endpoint,
      rx_target: '90%',
      udp_bandwidth: '1000m',
      udp_length: '',
    });
    expect(list).toHaveLength(1);
  });

  it('改空之后整条被丢掉，不留空壳', () => {
    let list = setNicPolicy([], endpoint, { rx_target: '1800' });
    expect(list).toHaveLength(1);
    list = setNicPolicy(list, endpoint, { rx_target: '   ' });
    expect(list).toHaveLength(0);
  });

  it('只发真的填了东西的条目', () => {
    const list: UiNicPolicy[] = [
      { endpoint, rx_target: '1800', udp_bandwidth: '', udp_length: '' },
      { endpoint: 'agent:NAME=eth0', rx_target: '', udp_bandwidth: ' ', udp_length: '' },
    ];
    expect(activeNicPolicies(list)).toHaveLength(1);
  });
});
