import { describe, expect, it } from 'vitest';
import type { HostInfo, NicInfo } from '../api/dto';
import { defaultInnerConfig, innerLink, type InnerCapability } from './inner';
import { innerBoardInterfaces, innerNicChoices, innerSetupIssues, linksFromInnerChoices, suggestedInnerBoardIpv6 } from './inner-setup';

const nic = (name: string, ipv4: string) => ({ name, ipv4 } as NicInfo);
const host = (interfaces: NicInfo[]) => ({ interfaces } as HostInfo);
function capability(): InnerCapability {
  return { serial: 'cpe', board_version: '', board_addresses: '', board_counters: '', board_interfaces: [],
    local: host([nic('ETH', '192.168.0.100'), nic('ETH', '192.168.0.101'), nic('ETH', '192.168.0.100'), nic('无地址', ''), nic('环回', '127.0.0.1')]),
    agents: [{ id: 'pc2', status: 'ready', error: null, info: host([nic('ETH', '192.168.0.100')]) },
      { id: 'offline', status: 'failed', error: '离线', info: host([nic('旧接口', '192.168.0.200')]) }] };
}

describe('内环扫描选口', () => {
  it('主控和辅测机共享展示过滤，展开 10/172 与虚拟口不混淆同网段多网卡', () => {
    const cap = capability();
    const interfaces = [nic('LAN-A', '192.168.0.2'), nic('LAN-B', '192.168.0.3'),
      nic('LAN-10', '10.0.0.2'), nic('LAN-172', '172.16.0.2'), nic('WireGuard', '192.168.9.2')];
    cap.local = host(interfaces);
    cap.agents = [{ id: 'pc2', status: 'ready', error: null, info: host([...interfaces].reverse()) }];
    expect(innerNicChoices(cap)).toHaveLength(4);
    const all = innerNicChoices(cap, true);
    expect(all).toHaveLength(10);
    expect(new Set(all.map((choice) => choice.key)).size).toBe(10);
    const selected = all.filter((choice) => choice.host === 'pc2' && choice.nic.name === 'LAN-172');
    expect(linksFromInnerChoices([], selected)[0]).toMatchObject({ host: 'pc2', local_interface: 'LAN-172', local_ip: '172.16.0.2' });
    expect(innerNicChoices(cap)).toHaveLength(4);
  });
  it('IPv6-only 物理网口可见，v6 隧道仍默认隐藏；双栈只添加一个网口', () => {
    const cap = capability();
    cap.agents = [];
    cap.local.interfaces = [
      { ...nic('ETH6', ''), ipv6_ll: 'fe80::100', ipv6_global: 'fd00::100' },
      { ...nic('utun6', ''), ipv6_ll: 'fe80::200' },
      { ...nic('ETH双栈', '192.168.0.2'), ipv6_ll: 'fe80::2' },
    ];
    const choices = innerNicChoices(cap);
    expect(choices.map((choice) => choice.nic.name)).toEqual(['ETH6', 'ETH双栈']);
    const added = linksFromInnerChoices([], choices);
    expect(added).toHaveLength(2);
    expect(added[0]).toMatchObject({ local_ip: '', local_ipv6: 'fe80::100', gateway_ipv6: null });
    expect(linksFromInnerChoices(added, choices)).toEqual([]);
  });
  it('只补齐已知板侧口唯一同类 IPv6，多个候选时要求用户选择', () => {
    const cap = capability();
    cap.board_interfaces = [{ name: 'br0', addresses: ['192.168.0.1/24'], ipv6_addresses: ['fe80::1/64', 'fd00::1/64'], master: '', members: ['eth0'], proc_counters: true, sysfs_counters: true }];
    const link = innerLink('master', { ...nic('ETH', '192.168.0.2'), ipv6_ll: 'fe80::2' });
    expect(suggestedInnerBoardIpv6(cap, link)).toBe('fe80::1');
    link.local_ipv6 = 'fd00::2';
    expect(suggestedInnerBoardIpv6(cap, link)).toBe('fd00::1');
    cap.board_interfaces[0].ipv6_addresses!.push('fd00::3/64');
    expect(suggestedInnerBoardIpv6(cap, link)).toBeNull();
    link.gateway = '10.0.0.1'; link.board_rx_interface = 'missing';
    expect(suggestedInnerBoardIpv6(cap, link)).toBeNull();
  });
  it('默认隐藏 utun 和非 192.168 地址，显式展开仍保留真实身份且不自动加入', () => {
    const cap = capability();
    cap.local.interfaces.push(nic('utun6', '198.18.0.1'), nic('utun7', '192.168.9.2'), nic('ETH 10', '10.0.0.2'), nic('RNDIS', '192.168.42.100'));
    const before = JSON.stringify(cap);
    expect(innerNicChoices(cap).map((choice) => choice.nic.name)).toEqual(['ETH', 'ETH', 'RNDIS', 'ETH']);
    expect(innerNicChoices(cap, true)).toHaveLength(7);
    expect(JSON.stringify(cap)).toBe(before);
  });
  it('板侧摘要按地址和本轮引用筛选，完整系统清单不冒充测试网口数量', () => {
    const cap = capability();
    const board = (name: string, addresses: string[]) => ({ name, addresses, master: '', members: [], proc_counters: true, sysfs_counters: true });
    cap.board_interfaces = [board('lo', ['127.0.0.1/8']), board('br0', ['192.168.0.1/24']), board('eth1', []), board('wan', ['10.0.0.1/24']), board('stats', [])];
    expect(innerBoardInterfaces(cap, []).map((iface) => iface.name)).toEqual(['br0']);
    const link = innerLink(); link.gateway = '10.0.0.1'; link.board_rx_interface = 'stats';
    expect(innerBoardInterfaces(cap, [link]).map((iface) => iface.name)).toEqual(['br0', 'wan', 'stats']);
    expect(cap.board_interfaces).toHaveLength(5);
  });
  it('按电脑、接口和 IP 标识扫描项，保留同名多地址并排除重复及无效地址', () => {
    expect(innerNicChoices(null)).toEqual([]);
    const choices = innerNicChoices(capability());
    expect(choices).toHaveLength(3);
    expect(new Set(choices.map((choice) => choice.key)).size).toBe(3);
    expect(choices.map((choice) => choice.host)).toEqual(['master', 'master', 'pc2']);
  });
  it('批量添加不覆盖已有参数、不重新启用已取消的口，且名字避重', () => {
    const existing = innerLink('master', nic('ETH', '192.168.0.100'), 'ETH');
    existing.enabled = false;
    existing.gateway = '192.168.0.9';
    existing.measurement = 'nic_strict';
    existing.upload_min_mbps = 800;
    const before = JSON.stringify(existing);
    const added = linksFromInnerChoices([existing], innerNicChoices(capability()));
    expect(added.map((link) => link.name)).toEqual(['ETH 2', 'ETH 3']);
    expect(added[0]).toMatchObject({ local_interface: 'ETH', local_ip: '192.168.0.101', gateway: '192.168.0.1', board_rx_interface: 'br0', measurement: 'nic_preferred' });
    expect(JSON.stringify(existing)).toBe(before);
    expect(linksFromInnerChoices([existing, ...added], innerNicChoices(capability()))).toEqual([]);
  });
  it('批量添加遵守 32 个网口上限', () => {
    const existing = Array.from({ length: 31 }, (_, index) => innerLink('master', nic(`已有 ${index}`, `10.0.0.${index + 1}`), `已有 ${index}`));
    expect(linksFromInnerChoices(existing, innerNicChoices(capability()))).toHaveLength(1);
  });
});

describe('内环开始前的可定位缺项', () => {
  it('未参与的 IPv6-only 网口不会阻断其他网口的 IPv4 测试，重新勾选后才要求 IPv4', () => {
    const config = defaultInnerConfig();
    config.links = [innerLink('master', nic('ETH4', '192.168.0.2'), 'ETH4'),
      { ...innerLink('master', { ...nic('ETH6', ''), ipv6_ll: 'fe80::2' }, 'ETH6'), enabled: false }];
    expect(innerSetupIssues(config)).toEqual([]);
    config.links[1].enabled = true;
    expect(innerSetupIssues(config)[0]).toMatchObject({ linkIndex: 1, target: 'links' });
  });
  it('IPv6 缺项定位到网口，v6-only 不要求 IPv4，未参与链路不要求 v6', () => {
    const config = defaultInnerConfig(); config.ip_versions = [6];
    config.links = [innerLink('master', { ...nic('ETH6', ''), ipv6_ll: 'fe80::2' })];
    expect(innerSetupIssues(config)[0]).toMatchObject({ linkIndex: 0, target: 'links' });
    expect(innerSetupIssues(config)[0].message).toContain('CPE LAN IPv6');
    config.links[0].gateway_ipv6 = 'fe80::1';
    expect(innerSetupIssues(config)).toEqual([]);
    config.links.push({ ...innerLink('master', nic('旧网口', ''), '旧网口'), enabled: false });
    expect(innerSetupIssues(config)).toEqual([]);
  });
  it('空计划引导添加网口，缺失地址定位到具体一行', () => {
    const config = defaultInnerConfig();
    expect(innerSetupIssues(config)).toEqual([{ message: '本轮还没有勾选参与测试的网口。', action: '添加网口', target: 'links' }]);
    config.links = [innerLink()];
    expect(innerSetupIssues(config)[0]).toMatchObject({ linkIndex: 0, action: '编辑此网口', target: 'links' });
    expect(innerSetupIssues(config)[0]?.message).toContain('电脑网卡');
    config.links[0] = innerLink('master', nic('ETH', '192.168.0.1'));
    expect(innerSetupIssues(config)[0]?.message).toContain('不能相同');
  });
  it('UDP 缺发送速率时给出参数修复入口，补好后消失', () => {
    const config = defaultInnerConfig();
    config.links = [innerLink('master', nic('ETH', '192.168.0.100'))];
    config.protocols.push('udp');
    expect(innerSetupIssues(config)[0]).toMatchObject({ target: 'params', action: '填写 UDP 速率' });
    config.udp_mbps = 500;
    expect(innerSetupIssues(config)).toEqual([]);
  });
  it('未参与的辅测机无需在线；配置完整不代表链路已经可达', () => {
    const config = defaultInnerConfig();
    config.agents = [{ id: 'offline', address: '192.168.2.5', port: 28801 }];
    config.links = [innerLink('master', nic('ETH', '192.168.0.100')), { ...innerLink('offline', nic('ETH', '192.168.2.100'), '未参与'), enabled: false }];
    expect(innerSetupIssues(config)).toEqual([]);
  });
  it('高级参数仍使用现有解析器校验，不另建验收规则', () => {
    const config = defaultInnerConfig();
    config.links = [innerLink('master', nic('ETH', '192.168.0.100'))];
    config.tcp_window = 'wrong';
    expect(innerSetupIssues(config)[0]).toMatchObject({ target: 'params' });
    config.tcp_window = null;
    config.adb_path = 'wrong';
    expect(innerSetupIssues(config)[0]).toMatchObject({ target: 'device' });
  });
});
