import { describe, expect, it } from 'vitest';
import type { NicInfo } from '../api/dto';
import { filterByQuery } from './search';
import { nicKey, nicLinkLocal, nicSearchFields, nicSpeedLabel, nicWifiContext } from './nics';

function nic(patch: Partial<NicInfo>): NicInfo {
  return {
    name: 'en0',
    description: 'Ethernet',
    role: 'SGMII1G',
    ipv4: '192.168.8.100',
    gateway_v4: '192.168.8.1',
    ipv6_ll: 'fe80::1813',
    ipv6_global: '',
    zone: 'en0',
    speed_mbps: 1000,
    ifindex: 0,
    is_wifi: false,
    wifi_band: '',
    ...patch,
  } as NicInfo;
}

describe('网卡展示规则', () => {
  it('协商速率 0 是「未获取」，不是 0 Mbps', () => {
    // 写成「0 Mbps」会让人以为链路挂了，而多半只是驱动没报。
    expect(nicSpeedLabel(nic({ speed_mbps: 0 }))).toBe('未获取');
    expect(nicSpeedLabel(nic({ speed_mbps: 1000 }))).toBe('1000 Mbps');
  });

  it('选中标识用接口名 + IPv4，不用下标', () => {
    // 下标会在重扫后指向另一块卡（拔掉一根线，后面全往前挪一位）。
    expect(nicKey(nic({ name: 'en1', ipv4: '10.0.0.2' }))).toBe('en1|10.0.0.2');
    expect(nicKey(nic({}))).not.toBe(nicKey(nic({ ipv4: '192.168.8.101' })));
  });

  it('link-local 带上 zone；没有地址就是空串', () => {
    expect(nicLinkLocal(nic({}))).toBe('fe80::1813%en0');
    expect(nicLinkLocal(nic({ zone: '' }))).toBe('fe80::1813');
    expect(nicLinkLocal(nic({ ipv6_ll: '' }))).toBe('');
  });

  it('搜得到接口名、描述、IPv4、IPv6 和角色', () => {
    const list = [nic({}), nic({ name: 'en1', role: 'WIFI5G', description: 'Wi-Fi', ipv4: '192.168.8.104', ipv6_ll: 'fe80::14a8', wifi_band: '5GHz' })];
    const hit = (q: string) => filterByQuery(list, q, nicSearchFields).map((n) => n.name);
    expect(hit('wifi5g')).toEqual(['en1']);
    expect(hit('ethernet')).toEqual(['en0']);
    expect(hit('8.104')).toEqual(['en1']);
    expect(hit('fe80::1813')).toEqual(['en0']);
    expect(hit('5ghz')).toEqual(['en1']);
  });
});

describe('nicWifiContext', () => {
  const base: NicInfo = {
    name: 'WLAN',
    description: 'Intel Wi-Fi 6',
    role: 'WIFI5G',
    ipv4: '192.168.1.2',
    gateway_v4: '',
    ipv6_ll: '',
    ipv6_global: '',
    zone: '',
    speed_mbps: 2402,
    is_wifi: true,
    wifi_band: '5GHz',
    ifindex: 7,
  };

  it('把 SSID、信道、信号与无线电类型串成一行', () => {
    expect(
      nicWifiContext({
        ...base,
        wifi_ssid: 'CPE_TEST_5G',
        wifi_channel: 149,
        wifi_signal_pct: 99,
        wifi_radio: '802.11ax',
      }),
    ).toBe('CPE_TEST_5G · 信道 149 · 信号 99% · 802.11ax');
  });

  it('有线口一个字都不出', () => {
    expect(nicWifiContext({ ...base, is_wifi: false, wifi_ssid: 'x' })).toBe('');
  });

  it('旧版 agent 没上报这几项时返回空串，而不是一串占位符', () => {
    expect(nicWifiContext(base)).toBe('');
  });

  it('信号 0% 是真实读数，必须照样显示', () => {
    // 断在边缘的链路信号就是 0%。把它当成「没读到」滤掉，屏幕上会显示成
    // 一块信号正常的网卡，而那正是要查的现象。
    expect(nicWifiContext({ ...base, wifi_signal_pct: 0 })).toBe('信号 0%');
  });
});
