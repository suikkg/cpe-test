import { beforeEach, describe, expect, it } from 'vitest';
import { createSSRApp } from 'vue';
import { renderToString } from 'vue/server-renderer';
import type { NicInfo } from '../../api/dto';
import {
  defaultInnerConfig, innerLink,
  type InnerBoardInterface, type InnerCapability, type InnerLeg, type InnerPreviewRow, type InnerUnit,
} from '../../domain/inner';
import { inner } from '../../state/inner';
import InnerView from './InnerView.vue';
import InnerLinkDetail from './InnerLinkDetail.vue';

function nic(name: string, ipv4: string, ipv6 = ''): NicInfo {
  return {
    name, description: '', role: 'LAN', ipv4, gateway_v4: '192.168.0.1',
    ipv6_ll: ipv6, ipv6_global: '', zone: name, speed_mbps: 1000,
    is_wifi: false, wifi_band: '', ifindex: 1,
  };
}

function board(name: string, addresses: string[] = [], ipv6: string[] = []): InnerBoardInterface {
  return { name, addresses, ipv6_addresses: ipv6, master: '', members: [], proc_counters: true, sysfs_counters: true };
}

function capability(interfaces: NicInfo[]): InnerCapability {
  return {
    serial: 'CPE-SSR', board_version: 'test-board', board_addresses: '', board_counters: '',
    board_interfaces: [board('br0', ['192.168.0.1/24'], ['fe80::1/64'])],
    local: { hostname: 'test-master', os: 'Windows', interfaces }, agents: [],
  };
}

function previewRow(index: number, ipVersion: 4 | 6): InnerPreviewRow {
  return {
    index, link: 'LAN', host: 'master', protocol: 'tcp', direction: 'upload', ip_version: ipVersion,
    repeat: 1, measurement: 'nic_preferred', verdict_basis: '接收端网卡 RX',
    legs: [{ flow: 'up', port: 56190, receiver: 'CPE br0', nic_target_mbps: null, tool_target_mbps: null }],
  };
}

function leg(): InnerLeg {
  return {
    flow: 'up', port: 56190, receiver: 'br0', receiver_host: 'CPE', counter_source: 'proc',
    source: 'nic', mbps: 900, target_mbps: null, fallback_reason: null,
    verdict: 'MEASURED', reason: 'NO_TARGET', detail: '', diagnostics: [],
    nic_rx_mbps: 900, nic_verdict: 'MEASURED', nic_reason: 'NO_TARGET', nic_target_mbps: null,
    coverage: 1, effective_secs: 20, required_secs: 20,
    tool_sender_mbps: 910, tool_receiver_mbps: 900, tool_receiver_note: '',
    udp_loss_pct: null, udp_lost_datagrams: null, udp_total_datagrams: null,
  };
}

function result(index: number, ipVersion: 4 | 6 | undefined, legs: InnerLeg[]): InnerUnit {
  return {
    index, link: `结果网口 ${index}`, host: 'master', protocol: 'tcp', direction: 'upload',
    ip_version: ipVersion, streams: 1, repeat: 1, measurement: 'nic_preferred',
    verdict: legs.length ? 'MEASURED' : 'NOT_EVALUATED', reason: '', detail: '', diagnostics: [],
    total_mbps: null, total_target_mbps: null, overlap_secs: null, legs,
  };
}

beforeEach(() => {
  inner.config = defaultInnerConfig();
  inner.capability = null;
  inner.status = { running: false, current: '', error: null, completed: 0, total: 0, units: [], has_report: false };
  inner.preview = null;
  inner.previewStale = true;
  inner.previewError = '';
  inner.runs = [];
  inner.synced = true;
  inner.busy = false;
  inner.error = '';
  inner.draftSaved = true;
  inner.scenarioStartPhase = 'idle'; inner.scenarioLastReadIdle = false;
  inner.scenario = { running: false, id: '', phase: '', error: null, runs: [] };
});

const render = () => renderToString(createSSRApp(InnerView));
function scanPicker(html: string): string {
  const picker = html.match(/<section\b[^>]*aria-label="从扫描结果添加网口"[^>]*>[\s\S]*?<\/section>/)?.[0];
  expect(picker).toBeDefined();
  return picker!;
}
function ipSelector(html: string): string {
  const selector = html.match(/<legend[^>]*>IP 版本（分别测试）<\/legend>([\s\S]*?)<\/fieldset>/)?.[1];
  expect(selector).toBeDefined();
  return selector!;
}

describe('内环扫描清单的真实口径', () => {
  it('46 个板侧系统接口完整保留在默认折叠的清单中，不能冒充待测电脑网口数', async () => {
    inner.capability = capability([nic('LAN', '192.168.0.100')]);
    inner.capability.board_interfaces.push(...Array.from({ length: 45 }, (_, i) => board(`system${i}`)));
    const html = await render();
    const inventory = html.match(/<details([^>]*)><summary[^>]*>查看板侧全部系统接口（46 个）<\/summary>([\s\S]*?)<\/details>/);
    expect(inventory).not.toBeNull();
    expect(inventory![1]).not.toMatch(/\bopen(?:[\s=>]|$)/);
    expect(inventory![2]).toContain('这里是 CPE 的系统清单');
    expect(inventory![2]).toContain('实际参与测试的电脑网口以下方勾选为准');
    expect(inventory![2].match(/<tbody[^>]*>([\s\S]*?)<\/tbody>/)?.[1].match(/<tr\b/g)).toHaveLength(46);
    expect(scanPicker(html).match(/class="scan-option"/g)).toHaveLength(1);
    expect(html).toContain('共 0 条，本轮参与 0 条');
    expect(inner.capability.board_interfaces).toHaveLength(46);
  });

  it('默认不显示 utun 和其他 IPv4 网段，保留显式展开入口与原始扫描数据', async () => {
    inner.capability = capability([
      nic('LAN', '192.168.0.100'), nic('utun6', '198.18.0.1'),
      nic('utun7', '192.168.9.2'), nic('实验网', '10.0.0.2'),
    ]);
    const before = JSON.stringify(inner.capability);
    const picker = scanPicker(await render());
    expect(picker).toContain('主控本机 · LAN');
    expect(picker).not.toContain('utun6');
    expect(picker).not.toContain('utun7');
    expect(picker).not.toContain('198.18.0.1');
    expect(picker).not.toContain('10.0.0.2');
    expect(picker).toContain('显示其他网段 / 隧道接口（3 项）');
    expect(picker.match(/<label[^>]*class="other-toggle"[^>]*>([\s\S]*?)<\/label>/)?.[1]).not.toContain('checked');
    expect(JSON.stringify(inner.capability)).toBe(before);
    expect(inner.config.links).toEqual([]);
  });

  it.each(['', '0.0.0.0'])('仅有 IPv6 的真实电脑网口仍可选择（IPv4=%s）', async (ipv4) => {
    inner.capability = capability([nic('IPv6-LAN', ipv4, 'fe80::100'), nic('utun6', '', 'fe80::200')]);
    const picker = scanPicker(await render());
    expect(picker).toContain('主控本机 · IPv6-LAN');
    expect(picker).toContain('IPv6 fe80::100');
    expect(picker).not.toContain('utun6');
    expect(picker).not.toContain('fe80::200');
    expect(picker.match(/class="scan-option"/g)).toHaveLength(1);
  });
});

describe('内环双栈的表单、预览和结果', () => {
  it('默认仅选 IPv4，最后一个 IP 版本不能取消，IPv6 保持可选', async () => {
    const selector = ipSelector(await render());
    const labels = [...selector.matchAll(/<label[^>]*>([\s\S]*?)<\/label>/g)].map((m) => m[1]);
    const v4 = labels.find((label) => label.includes('IPv4'))!;
    const v6 = labels.find((label) => label.includes('IPv6'))!;
    expect(v4).toContain('checked');
    expect(v4).toContain('disabled');
    expect(v6).not.toContain('checked');
    expect(v6).not.toContain('disabled');
  });

  it('双栈分别显示两个选项和两端 IPv4 / IPv6 地址，不把 IPv6 收进隐藏字段', async () => {
    const computer = nic('LAN', '192.168.0.100', 'fe80::100');
    inner.capability = capability([computer]);
    inner.config.ip_versions = [4, 6];
    inner.config.links = [{ ...innerLink('master', computer, 'LAN'), gateway_ipv6: 'fe80::1' }];
    const html = await render();
    const selector = ipSelector(html);
    expect(selector.match(/\bchecked/g)).toHaveLength(2);
    expect(selector).not.toContain('disabled');
    expect(html).toContain('v4 192.168.0.100');
    expect(html).toContain('v4 192.168.0.1');
    expect(html).toContain('v6 fe80::100');
    expect(html).toContain('v6 fe80::1');
    expect(html).toContain('已发现网卡');
  });

  it('IPv6-only 的详情提供两端 IPv6 输入与实际板侧候选，不显示 IPv4 必填项', async () => {
    const computer = nic('IPv6-LAN', '', 'fe80::100');
    inner.capability = capability([computer]);
    inner.config.ip_versions = [6];
    inner.config.links = [{ ...innerLink('master', computer, 'LAN'), gateway_ipv6: 'fe80::1' }];
    const html = await renderToString(createSSRApp(InnerLinkDetail, {
      link: inner.config.links[0], hosts: [{ id: 'master', label: '主控本机' }], disabled: false,
    }));
    expect(html).toMatch(/电脑网卡 IPv6<input[^>]*value="fe80::100"/);
    expect(html).toMatch(/CPE LAN IPv6<input[^>]*value="fe80::1"/);
    expect(html).toMatch(/<datalist[^>]*id="inner-board-ipv6"[^>]*>[\s\S]*value="fe80::1"[\s\S]*br0/);
    expect(html).not.toContain('CPE LAN IPv4');
    expect(html).not.toContain('该电脑网卡 IPv4');
  });

  it('逐单元预览保留服务端返回的 IP 版本，结果有腿和无腿两条渲染路径都标明版本', async () => {
    inner.preview = {
      links: 1, units: 2, legs: 2, bidir_units: 0, estimated_secs: 40,
      uses_master: true, agents: [], skipped: [], rows: [previewRow(1, 4), previewRow(2, 6)],
    };
    inner.status.units = [result(1, 4, [leg()]), result(2, 6, [leg()]), result(3, 6, [])];
    inner.status.completed = 3;
    inner.status.total = 3;
    const html = await render();
    const preview = html.match(/aria-label="计划预览"[^>]*>([\s\S]*?)<\/section>/)?.[1] ?? '';
    const results = html.match(/aria-label="内环执行与结果"[^>]*>([\s\S]*?)<\/section>/)?.[1] ?? '';
    expect(preview).toContain('LAN · IPv4 · TCP');
    expect(preview).toContain('LAN · IPv6 · TCP');
    expect(results.match(/TCP \/ IPv4/g)).toHaveLength(1);
    expect(results.match(/TCP \/ IPv6/g)).toHaveLength(2);
    expect(results).toContain('本单元没有产生任何一条腿的结果');
  });

  it('升级前缺少 IP 版本的结果明确显示 IPv4', async () => {
    inner.status.units = [result(1, undefined, [])];
    expect(await render()).toContain('TCP / IPv4');
  });
});


it('未知启动显示查询状态，禁用开始，并仅在已读到空闲后提供人工恢复', async () => {
  inner.scenarioStartPhase = 'unknown';
  let html = await render();
  expect(html).toContain('启动结果未确认');
  expect(html).not.toContain('空闲 / 已结束');
  expect(html.match(/<button[^>]*disabled[^>]*>开始内环测试<\/button>/)).not.toBeNull();
  expect(html).not.toContain('已核实测试未运行，重新准备');
  inner.scenarioLastReadIdle = true;
  html = await render();
  expect(html).toContain('已核实测试未运行，重新准备');
});
