import type { NicInfo } from '../api/dto';
import {
  INNER_DEFAULT_BOARD_RX, canonicalInnerIpv6, innerIpv4, innerIpv6, innerIpv6LinkLocal, innerNicIpv6, innerLink, normalizeInnerDraft, parseInnerProject,
  type InnerCapability, type InnerConfig, type InnerLink,
} from './inner';

export interface InnerNicChoice { key: string; host: string; nic: NicInfo }
export const innerEndpointKey = (host: string, name: string, ip: string, ipv6?: string | null): string => JSON.stringify([host, name, innerIpv4(ip) ? ip : canonicalInnerIpv6(ipv6)]);

/** 常规选口聚焦 CPE 的 192.168 网段，其他地址和常见隧道口须显式展开。 */
export function innerNicOtherReason(nic: NicInfo): string {
  if (/^(?:utun\d*|tun\d*|tap\d*|lo\d*|docker\d*|veth\w*|tailscale\d*|awdl\d*|llw\d*|anpi\d*|gif\d*|stf\d*)$/i.test(nic.name)
    || /\b(?:vpn|wireguard|wintun|tailscale|zerotier|loopback)\b/i.test(`${nic.name} ${nic.description ?? ''}`)) {
    return '隧道或虚拟接口';
  }
  return nic.ipv4.startsWith('192.168.') || (!innerIpv4(nic.ipv4) && innerNicIpv6(nic)) ? '' : '非 192.168 网段';
}

/** 扫描只提供电脑上的接口身份，不证明它能连到板侧；不修改完整扫描数据。 */
export function innerNicChoices(capability: InnerCapability | null, includeOther = false): InnerNicChoice[] {
  if (!capability) return [];
  const hosts = [{ host: 'master', info: capability.local }, ...capability.agents
    .filter((agent) => agent.status === 'ready' && agent.info)
    .map((agent) => ({ host: agent.id, info: agent.info! }))];
  const seen = new Set<string>();
  return hosts.flatMap(({ host, info }) => info.interfaces.flatMap((nic) => {
    const key = innerEndpointKey(host, nic.name, nic.ipv4, innerNicIpv6(nic));
    if (!nic.name || (!innerIpv4(nic.ipv4) && !innerNicIpv6(nic)) || seen.has(key) || (!includeOther && innerNicOtherReason(nic))) return [];
    seen.add(key);
    return [{ key, host, nic }];
  }));
}

/** 板侧只摘要展示常用 LAN 地址及当前配置引用的接口，完整系统清单保留在详情。 */
export function innerBoardInterfaces(capability: InnerCapability | null, links: InnerLink[]) {
  if (!capability) return [];
  const active = links.filter((link) => link.enabled);
  return capability.board_interfaces.filter((iface) => active.some((link) => link.board_rx_interface === iface.name)
    || ((iface.name === INNER_DEFAULT_BOARD_RX || iface.members.length > 0) && (iface.ipv6_addresses ?? []).some((address) => innerIpv6(address.split('/')[0])))
    || iface.addresses.some((address) => {
      const ip = address.split('/')[0];
      return innerIpv4(ip) && (ip.startsWith('192.168.') || active.some((link) => link.gateway === ip));
    }) || (iface.ipv6_addresses ?? []).some((address) => active.some((link) => canonicalInnerIpv6(link.gateway_ipv6)
      && canonicalInnerIpv6(link.gateway_ipv6) === canonicalInnerIpv6(address.split('/')[0]))));
}

/** 可选板侧 IPv6 始终带接口身份，链路本地地址不可跨接口混用。 */
export function innerBoardIpv6Choices(capability: InnerCapability | null) {
  return (capability?.board_interfaces ?? []).flatMap((iface) => (iface.ipv6_addresses ?? [])
    .map((address) => address.split('/')[0]).filter(innerIpv6)
    .map((address) => ({ name: iface.name, address })));
}

/** 只从已知 LAN 归属口或指定统计口补唯一的同类地址；不猜网关，也不覆盖手填值。 */
export function suggestedInnerBoardIpv6(capability: InnerCapability | null, link: InnerLink): string | null {
  if (!capability || !innerIpv6(link.local_ipv6)) return null;
  const board = capability.board_interfaces ?? [];
  const byIpv4 = board.filter((iface) => iface.addresses.some((address) => address.split('/')[0] === link.gateway));
  const interfaces = byIpv4.length ? byIpv4 : board.filter((iface) => iface.name === link.board_rx_interface);
  if (interfaces.length !== 1) return null;
  const candidates = [...new Set((interfaces[0].ipv6_addresses ?? []).map((address) => canonicalInnerIpv6(address.split('/')[0])))]
    .filter((ip) => ip && innerIpv6LinkLocal(ip) === innerIpv6LinkLocal(link.local_ipv6));
  return candidates.length === 1 ? candidates[0] : null;
}

/** 相同电脑/接口/IP 只添加一次；已取消参与的配置也保留，不偷偷启用它。 */
export function linksFromInnerChoices(existing: InnerLink[], choices: InnerNicChoice[], capability: InnerCapability | null = null): InnerLink[] {
  const keys = new Set(existing.map((link) => innerEndpointKey(link.host, link.local_interface, link.local_ip, link.local_ipv6)));
  const names = new Set(existing.map((link) => link.name));
  const added: InnerLink[] = [];
  for (const choice of choices) {
    if (existing.length + added.length >= 32) break;
    if (keys.has(choice.key)) continue;
    let name = choice.nic.name;
    for (let suffix = 2; names.has(name); suffix++) name = `${choice.nic.name} ${suffix}`;
    const link = innerLink(choice.host, choice.nic, name);
    link.gateway_ipv6 = suggestedInnerBoardIpv6(capability, link);
    added.push(link);
    keys.add(choice.key);
    names.add(name);
  }
  return added;
}

export interface InnerSetupIssue {
  message: string;
  action: string;
  target: 'device' | 'links' | 'params';
  linkIndex?: number;
}

/** 将缺项定位到可编辑的网口；能力扫描和实际连通性不参与配置门禁。 */
export function innerSetupIssues(config: InnerConfig): InnerSetupIssue[] {
  config = normalizeInnerDraft(config);
  const issues: InnerSetupIssue[] = [];
  if (!config.links.some((link) => link.enabled)) {
    issues.push({ message: '本轮还没有勾选参与测试的网口。', action: config.links.length ? '勾选网口' : '添加网口', target: 'links' });
  }
  config.links.forEach((link, linkIndex) => {
    const missing = [];
    if (!link.name.trim()) missing.push('网口名称');
    if (!link.local_interface.trim()) missing.push('电脑网卡');
    if (config.ip_versions.includes(4) && link.enabled) {
      if (!innerIpv4(link.local_ip)) missing.push('有效的电脑 IPv4');
      if (!innerIpv4(link.gateway)) missing.push('有效的 CPE LAN IPv4');
    }
    if (config.ip_versions.includes(6) && link.enabled) {
      if (!innerIpv6(link.local_ipv6)) missing.push('有效的电脑 IPv6');
      if (!innerIpv6(link.gateway_ipv6)) missing.push('有效的 CPE LAN IPv6');
    }
    const prefix = link.name.trim() || `网口 ${linkIndex + 1}`;
    if (missing.length) issues.push({ message: `${prefix}：请补齐${missing.join('、')}。`, action: '编辑此网口', target: 'links', linkIndex });
    else if (config.ip_versions.includes(4) && link.enabled && link.local_ip === link.gateway) issues.push({ message: `${prefix}：电脑 IPv4 和 CPE LAN 地址不能相同。`, action: '修改地址', target: 'links', linkIndex });
    else if (config.ip_versions.includes(6) && link.enabled && canonicalInnerIpv6(link.local_ipv6) === canonicalInnerIpv6(link.gateway_ipv6)) issues.push({ message: `${prefix}：电脑 IPv6 和 CPE LAN IPv6 不能相同。`, action: '修改地址', target: 'links', linkIndex });
    else if (config.ip_versions.includes(6) && link.enabled && innerIpv6LinkLocal(link.local_ipv6) !== innerIpv6LinkLocal(link.gateway_ipv6)) issues.push({ message: `${prefix}：两端 IPv6 需同为链路本地地址或同为非链路本地地址。`, action: '修改地址', target: 'links', linkIndex });
  });
  if (config.protocols.includes('udp') && !(Number(config.udp_mbps) > 0) && !config.parameter_options?.udp_rates_mbps.length) {
    issues.push({ message: '已选择 UDP，请填写每条流的发送速率。', action: '填写 UDP 速率', target: 'params' });
  }
  if (!issues.length) {
    try { parseInnerProject(JSON.stringify(normalizeInnerDraft(config))); }
    catch (error) {
      const message = error instanceof Error ? error.message : '配置有误，请检查填写内容';
      const target = /ADB|序列号|板侧工具|辅测机/.test(message) ? 'device' : /链路|网口|地址|门限|统计接口/.test(message) ? 'links' : 'params';
      issues.push({ message, action: target === 'device' ? '检查设备设置' : target === 'links' ? '检查网口设置' : '检查打流参数', target });
    }
  }
  return issues;
}
