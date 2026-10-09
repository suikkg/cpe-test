import { describe, expect, it } from 'vitest';
import {
  canonicalInnerIpv6, innerIpv4, innerIpv6, innerIpv6LinkLocal, innerHistoryStatus,
  mergeInnerUnits,
  INNER_VERSION, defaultInnerConfig, innerDuration, innerIfaceWord, innerLink, innerSizeToken,
  normalizeInnerDraft, parseInnerProject, serializeInnerProject,
} from './inner';
import { parseProject } from './project';

/** 升级前那一代的文件：单个 protocol、board_interface、没有 enabled / measurement。 */
const V1_PROJECT = JSON.stringify({
  kind: 'cpe-inner-project',
  version: 1,
  config: {
    adb_path: 'adb', board_iperf: 'iperf3', duration_secs: 20, parallel: 1,
    protocol: 'tcp', directions: ['upload', 'download'], port: 56190,
    links: [{
      name: 'ETH', host: 'master', local_interface: '以太网', local_ip: '192.168.8.100',
      gateway: '192.168.8.1', board_interface: 'br0', upload_min_mbps: 800, download_min_mbps: 700,
    }],
    agents: [],
  },
});

function link(patch: Partial<ReturnType<typeof innerLink>> = {}) {
  return { ...innerLink(), local_interface: 'WLAN', local_ip: '192.168.8.101', gateway: '192.168.8.1', ...patch };
}

describe('内环项目隔离', () => {
  it('新建默认使用板侧 LAN 和桥统计，机型名称可改且往返保留', () => {
    expect(innerLink()).toMatchObject({ gateway: '192.168.0.1', board_rx_interface: 'br0' });
    for (const board of ['eth0', 'eth1', 'usblan0', 'wlan0', 'wlan4', 'wlan1', 'br-lan', '']) {
      const cfg = defaultInnerConfig();
      cfg.links = [link({ gateway: '192.168.9.1', board_rx_interface: board })];
      const saved = serializeInnerProject(normalizeInnerDraft(cfg));
      expect(parseInnerProject(saved).links[0]).toMatchObject({ gateway: '192.168.9.1', board_rx_interface: board });
    }
  });
  it('新建默认值不改变旧文件的自动接口语义，也不填补遗漏的目标地址', () => {
    const cfg = defaultInnerConfig();
    cfg.links = [link()];
    const old = JSON.parse(serializeInnerProject(cfg));
    delete old.config.links[0].board_rx_interface;
    expect(parseInnerProject(JSON.stringify(old)).links[0].board_rx_interface).toBe('');
    delete old.config.links[0].gateway;
    expect(() => parseInnerProject(JSON.stringify(old))).toThrow();
  });
  it('省略 measurement 的链路与后端 serde 默认保持严格网卡口径', () => {
    const cfg = defaultInnerConfig();
    cfg.links = [link()];
    const saved = JSON.parse(serializeInnerProject(cfg));
    delete saved.config.links[0].measurement;
    expect(parseInnerProject(JSON.stringify(saved)).links[0]?.measurement).toBe('nic_strict');
  });
  it('多电脑链路往返保留绑定，令牌不写入导出或草稿', () => {
    const cfg = defaultInnerConfig();
    cfg.agents = [{ id: 'wifi-pc', address: '192.168.8.101', port: 28801, token: 'private-token' }];
    cfg.links = [link({ host: 'wifi-pc' })];
    const text = serializeInnerProject(cfg);
    expect(text).not.toContain('private-token');
    expect(parseInnerProject(text).links).toEqual(cfg.links);
    expect(cfg.agents[0]?.token).toBe('private-token');
    const subnet = parseProject(text);
    expect(subnet.ok).toBe(false);
    if (!subnet.ok) expect(subnet.error).toContain('内环');
  });
  it('辅测机地址和令牌不接受控制字符', () => {
    const cfg = defaultInnerConfig();
    for (const patch of [
      { address: 'agent.example\nbackup' },
      { address: 'agent.example\u0000' },
      { address: 'agent.example\u0085' },
      { address: 'agent.example', token: 'secret\u0007' },
      { address: 'agent.example', token: 'secret\u009f' },
    ]) {
      cfg.agents = [{ id: 'agent1', address: patch.address, port: 28801, token: patch.token }];
      expect(() => parseInnerProject(JSON.stringify(cfg)), JSON.stringify(patch)).toThrow('辅测机');
    }
    cfg.agents = [{ id: 'a'.repeat(257), address: 'agent.example', port: 28801 }];
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow('辅测机');
  });
  it('辅测机地址和令牌有与 HTTP 头相称的字节上限', () => {
    const cfg = defaultInnerConfig();
    cfg.agents = [{ id: 'agent1', address: 'a'.repeat(257), port: 28801, token: '' }];
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow('辅测机');
    cfg.agents[0].address = 'agent.example';
    cfg.agents[0].token = 't'.repeat(4097);
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow('辅测机');
  });
  it('不接受子网文件、未知字段、重复主机或未定义电脑', () => {
    expect(() => parseInnerProject('{"project_version":1}')).toThrow('子网');
    // 子网的裸配置没有 project_version：只认那一个键的话，报的是一串陌生
    // 字段名，而不是「这份文件该去子网导」。与后端 SUBNET_ONLY_KEYS 同一清单。
    for (const body of [
      '{"agent_host":"192.168.1.3","agent_token":"cpetest"}',
      '{"iperf":{"duration":180}}',
      '{"tests":[],"pairs":"all"}',
      '{"ctstraffic":{},"link_profiles":{}}',
    ]) {
      expect(() => parseInnerProject(body), body).toThrow('子网');
    }
    // 与子网无关的未知字段仍走原来的兜底提示（agent_host 现在会被上面那条
    // 更准的子网护栏先接住，不再适合当这条的样本）。
    expect(() => parseInnerProject('{"adb_path":"adb","board_iperf":"iperf3","没这个字段":1}')).toThrow('未知字段');
    // adb_path 是唯一被当程序执行的字段，前后端同一条规矩。
    const withAdb = (path: string) => {
      const cfg = defaultInnerConfig();
      cfg.adb_path = path;
      return JSON.stringify(cfg);
    };
    for (const good of ['adb', './adb', '/usr/local/bin/adb', 'adb.exe', 'C:\\Program Files\\platform-tools\\adb.exe']) {
      expect(() => parseInnerProject(withAdb(good)), good).not.toThrow('ADB 路径');
    }
    for (const bad of ['/usr/bin/curl', '/bin/sh', '-adb', '/usr/bin/adb-wrapper', '\\\\server\\share\\adb.exe']) {
      expect(() => parseInnerProject(withAdb(bad)), bad).toThrow('ADB 路径');
    }
    expect(() => parseInnerProject(withAdb(`/${'界'.repeat(255)}/adb`))).toThrow('ADB 路径');
    for (const bad of ['board serial', '-board', 'board;serial']) {
      const cfg = defaultInnerConfig(); cfg.serial = bad;
      expect(() => parseInnerProject(JSON.stringify(cfg)), bad).toThrow('序列号');
    }
    for (const bad of ['iperf 3', '-iperf3', 'iperf;3']) {
      const cfg = defaultInnerConfig(); cfg.board_iperf = bad;
      expect(() => parseInnerProject(JSON.stringify(cfg)), bad).toThrow('序列号');
    }
    expect(() => parseInnerProject(`{"kind":"cpe-inner-project","version":${INNER_VERSION + 1},"config":{}}`)).toThrow('高于本程序支持');
    const cfg = defaultInnerConfig();
    cfg.agents = [{ id: 'master', address: 'pc', port: 28801 }];
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow('唯一');
    cfg.agents = [];
    cfg.links = [link({ host: 'missing' })];
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow('所属电脑');
  });
  it('v1 文件升级后仍是两个单向单元，不会凭空变出双向并发', () => {
    const migrated = parseInnerProject(V1_PROJECT);
    expect(migrated.protocols).toEqual(['tcp']);
    expect(migrated.directions).toEqual(['upload', 'download']);
    expect(migrated.directions).not.toContain('bidir');
    expect(migrated.links[0]).toMatchObject({
      board_rx_interface: 'br0',
      enabled: true,
      // 升级不能顺手把旧配置换成另一套验收规则。
      measurement: 'nic_strict',
      upload_min_mbps: 800,
    });
    expect(migrated.repeats).toBe(1);
    // 升级后的对象再导出就是 v2，第二次导入不再触发迁移。
    expect(JSON.parse(serializeInnerProject(migrated)).version).toBe(INNER_VERSION);
    expect(parseInnerProject(serializeInnerProject(migrated))).toEqual(migrated);
    // 同时含两种协议字段时不猜。
    expect(() => parseInnerProject(V1_PROJECT.replace('"protocol":"tcp"', '"protocol":"tcp","protocols":["udp"]'))).toThrow('无法判断');
  });
  it('清空门限只测量，切回 TCP 清除 UDP 负载，切回严格模式清除工具门限', () => {
    const cfg = defaultInnerConfig(); cfg.udp_mbps = 100;
    cfg.links = [link({ upload_min_mbps: '' as unknown as number, measurement: 'nic_strict', tool_upload_min_mbps: 500 })];
    const normalized = normalizeInnerDraft(cfg);
    expect(normalized.udp_mbps).toBeNull();
    expect(normalized.links[0]?.upload_min_mbps).toBeNull();
    // 严格模式用不到工具口径，留着这个数只会让后端整份拒绝。
    expect(normalized.links[0]?.tool_upload_min_mbps).toBeNull();
    // 没勾双向就没有合计门限。
    cfg.links = [link({ measurement: 'tool', tool_upload_min_mbps: 500, bidir_total_min_mbps: 900, tool_bidir_total_min_mbps: 900 })];
    const single = normalizeInnerDraft(cfg);
    expect(single.links[0]).toMatchObject({ tool_upload_min_mbps: 500, bidir_total_min_mbps: null, tool_bidir_total_min_mbps: null });
    cfg.directions = ['bidir'];
    expect(normalizeInnerDraft(cfg).links[0]?.tool_bidir_total_min_mbps).toBe(900);
  });
  it('工具口径门限和双向合计门限都不能是配了却永不生效的数', () => {
    const cfg = defaultInnerConfig();
    cfg.links = [link({ measurement: 'nic_strict', tool_upload_min_mbps: 500 })];
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow('工具口径');
    cfg.links = [link({ measurement: 'nic_preferred', bidir_total_min_mbps: 900 })];
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow('双向');
    cfg.directions = ['bidir'];
    expect(parseInnerProject(JSON.stringify(cfg)).links[0]?.bidir_total_min_mbps).toBe(900);
  });
  it('辅测机地址会在导入和发起请求前去掉首尾空格', () => {
    const cfg = defaultInnerConfig();
    cfg.agents = [{ id: 'agent1', address: ' 192.168.8.101 ', port: 28801, token: '' }];
    const parsed = parseInnerProject(JSON.stringify(cfg));
    expect(parsed.agents[0]?.address).toBe('192.168.8.101');
    cfg.agents[0]!.address = ' 192.168.8.102 ';
    expect(normalizeInnerDraft(cfg).agents[0]?.address).toBe('192.168.8.102');
  });
  it('同一台电脑的同一网口和源 IP 不能被两条参与本轮的链路抢', () => {
    const cfg = defaultInnerConfig();
    cfg.links = [link({ name: 'A' }), link({ name: 'B' })];
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow('同一网口');
    // 取消勾选就不再冲突，配置也不会被删。
    cfg.links[1].enabled = false;
    const parsed = parseInnerProject(JSON.stringify(cfg));
    expect(parsed.links).toHaveLength(2);
    expect(parsed.links[1]?.enabled).toBe(false);
  });
  it('一轮可以同时跑 TCP 和 UDP，两种协议的档位互不串台', () => {
    const cfg = defaultInnerConfig();
    cfg.protocols = ['tcp', 'udp'];
    cfg.udp_mbps = 500; cfg.udp_length = '1400'; cfg.udp_streams = 8;
    cfg.tcp_window = '4m'; cfg.tcp_streams = 2; cfg.max_udp_loss_pct = 1;
    expect(parseInnerProject(serializeInnerProject(cfg))).toEqual(cfg);

    // 取消勾选后残留在表单里的档位不能跟着请求走，否则后端按「不测该协议
    // 却配了它的参数」直接拒绝，用户在界面上根本看不出哪一项还留着。
    cfg.protocols = ['tcp'];
    const onlyTcp = normalizeInnerDraft(cfg);
    expect(onlyTcp).toMatchObject({ udp_mbps: null, udp_length: null, udp_streams: null, max_udp_loss_pct: null });
    expect(onlyTcp).toMatchObject({ tcp_window: '4m', tcp_streams: 2 });
    parseInnerProject(JSON.stringify(onlyTcp));

    cfg.protocols = ['udp'];
    cfg.udp_mbps = 500;
    const onlyUdp = normalizeInnerDraft(cfg);
    expect(onlyUdp).toMatchObject({ tcp_window: null, tcp_streams: null, udp_mbps: 500 });
    parseInnerProject(JSON.stringify(onlyUdp));
  });
  it('协议和方向至少各选一项，且不接受重复或未知取值', () => {
    for (const patch of [
      { protocols: [] }, { directions: [] },
      { protocols: ['tcp', 'tcp'] }, { directions: ['upload', 'upload'] },
      { protocols: ['sctp'] }, { directions: ['both'] },
      { protocols: 'tcp' },
    ]) {
      const cfg = { ...defaultInnerConfig(), udp_mbps: null, ...patch };
      expect(() => parseInnerProject(JSON.stringify(cfg)), JSON.stringify(patch)).toThrow();
    }
    // 三个方向全选是合法的：单向和并发各出各的结果。
    expect(parseInnerProject(JSON.stringify({ ...defaultInnerConfig(), directions: ['upload', 'download', 'bidir'] })).directions)
      .toEqual(['upload', 'download', 'bidir']);
  });
  it('端口给双向的第二个方向留了一格，重复轮次有上限', () => {
    for (const patch of [{ port: 65535 }, { port: 1023 }, { repeats: 0 }, { repeats: 11 }, { repeats: 1.5 }]) {
      expect(() => parseInnerProject(JSON.stringify({ ...defaultInnerConfig(), ...patch })), JSON.stringify(patch)).toThrow();
    }
    expect(parseInnerProject(JSON.stringify({ ...defaultInnerConfig(), port: 65534, repeats: 10 })).repeats).toBe(10);
  });
  it('-w / -l 只放行数字加可选 k/m/g，板侧接口名不能带路径', () => {
    for (const good of ['64k', '4m', '1G', '1400', '128']) expect(innerSizeToken(good), good).toBe(true);
    for (const bad of ['4mb', '-4m', '4 m', '', '0', '4m;ls', '1e3', '99999999999999']) expect(innerSizeToken(bad), bad).toBe(false);
    const cfg = { ...defaultInnerConfig(), tcp_window: '4m;ls' };
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow('tcp_window');
    for (const good of ['br0', 'eth1', 'wlan0.2', 'rai0']) expect(innerIfaceWord(good), good).toBe(true);
    for (const bad of ['../../etc', 'br0/x', '', '-br0', '.', '..']) expect(innerIfaceWord(bad), bad).toBe(false);
    const bad = { ...defaultInnerConfig(), links: [link({ board_rx_interface: '../../etc' })] };
    expect(() => parseInnerProject(JSON.stringify(bad))).toThrow('统计接口');
  });
  it('预估时长说人话', () => {
    expect(innerDuration(40)).toBe('40 秒');
    expect(innerDuration(600)).toBe('10 分钟');
    expect(innerDuration(4500)).toBe('1 小时 15 分');
  });
});

describe('状态增量合并', () => {
  const unit = (n: number) => ({ index: n } as never);
  it('units_from=0 是整份，直接替换；大于 0 时按位置接上', () => {
    expect(mergeInnerUnits([unit(9)], { units_from: 0, units: [unit(1), unit(2)] } as never))
      .toEqual([unit(1), unit(2)]);
    expect(mergeInnerUnits([unit(1), unit(2)], { units_from: 2, units: [unit(3)] } as never))
      .toEqual([unit(1), unit(2), unit(3)]);
  });
  it('漏了一拍或重连时截到游标处再接，不把中间的单元拼错位', () => {
    // 本地已有 4 个，但服务端说这批是从第 2 个开始的：第 3、4 个要被这批覆盖。
    expect(mergeInnerUnits([unit(1), unit(2), unit(3), unit(4)], { units_from: 2, units: [unit(30)] } as never))
      .toEqual([unit(1), unit(2), unit(30)]);
  });
  it('新一轮开跑（服务端清零）不会把上一轮的单元留在表里', () => {
    expect(mergeInnerUnits([unit(1), unit(2), unit(3)], { units_from: 0, units: [] } as never))
      .toEqual([]);
  });
});

describe('链路地址校验与后端同规则', () => {
  it('放行实际单播地址，拒掉笔误与后端会驳的四类地址', () => {
    for (const good of ['192.168.8.100', '10.0.0.1', '172.16.5.4', '169.254.1.1']) {
      expect(innerIpv4(good), good).toBe(true);
    }
    for (const bad of [
      '192.168.8.1OO',     // 字母 O
      '192.168.8',         // 段数不对
      '192.168.8.256',     // 越界
      '01.2.3.4',          // 前导零，Rust 解析器也拒
      '0.0.0.0',           // unspecified
      '127.0.0.1',         // 环回
      '224.0.0.1',         // 组播
      '255.255.255.255',   // 广播
      '',
    ]) {
      expect(innerIpv4(bad), bad).toBe(false);
    }
  });
  it('本机 IP 等于板侧地址时当场报出是哪条链路', () => {
    const cfg = defaultInnerConfig();
    cfg.links = [link({ name: '同址', local_ip: '192.168.8.1', gateway: '192.168.8.1' })];
    expect(() => parseInnerProject(serializeInnerProject(cfg))).toThrow('同址');
  });
  it('地址笔误在前端就被挡住，不留到 plan 才报', () => {
    const cfg = defaultInnerConfig();
    cfg.links = [link({ name: 'L1', local_ip: '192.168.8.1OO' })];
    expect(() => parseInnerProject(serializeInnerProject(cfg))).toThrow('L1');
  });
});

describe('导入格式与后端一致', () => {
  it('拒绝后端无法解析的地址空白和非整数版本', () => {
    const cfg = defaultInnerConfig();
    cfg.links = [link()];
    const project = JSON.parse(serializeInnerProject(cfg));
    for (const version of [-1, 1.5]) {
      expect(() => parseInnerProject(JSON.stringify({ ...project, version }))).toThrow();
    }
    for (const field of ['local_ip', 'gateway'] as const) {
      const bad = structuredClone(project);
      bad.config.links[0][field] += ' ';
      expect(() => parseInnerProject(JSON.stringify(bad))).toThrow('IPv4');
    }
  });
});

describe('内环 IPv6 配置与兼容', () => {
  it('旧 v2 省略 IP 版本时保持 IPv4，不自动增加测试单元', () => {
    const cfg = defaultInnerConfig(); cfg.links = [link()];
    const saved = JSON.parse(serializeInnerProject(cfg)); saved.version = 2;
    delete saved.config.ip_versions;
    delete saved.config.links[0].local_ipv6; delete saved.config.links[0].gateway_ipv6;
    expect(parseInnerProject(JSON.stringify(saved)).ip_versions).toEqual([4]);
    expect(parseInnerProject(JSON.stringify(saved)).links[0].local_ipv6).toBeNull();
  });
  it('明确标成 v2 的旧字段错误不能被 v1 迁移悄悄吞掉', () => {
    const saved = JSON.parse(V1_PROJECT); saved.version = 2;
    expect(() => parseInnerProject(JSON.stringify(saved))).toThrow('未知字段');
  });
  it('双栈导出导入保留真实两端地址，缺 IPv6 必须明确报错', () => {
    const cfg = defaultInnerConfig(); cfg.ip_versions = [4, 6];
    cfg.links = [link({ local_ipv6: 'fe80::100', gateway_ipv6: 'fe80::1' })];
    expect(parseInnerProject(serializeInnerProject(cfg))).toEqual(cfg);
    cfg.links[0].gateway_ipv6 = null;
    expect(() => parseInnerProject(serializeInnerProject(cfg))).toThrow('IPv6');
    cfg.links[0].enabled = false;
    expect(parseInnerProject(serializeInnerProject(cfg)).links[0].enabled).toBe(false);
  });
  it('仅 IPv6 可不填 IPv4，清空 v6 不会被猜成板侧地址', () => {
    const cfg = defaultInnerConfig(); cfg.ip_versions = [6];
    cfg.links = [link({ local_ip: '', gateway: '', local_ipv6: 'fd12::100', gateway_ipv6: 'fd12::1' })];
    const normalized = normalizeInnerDraft(cfg);
    expect(normalized.links[0]).toMatchObject({ local_ip: '0.0.0.0', gateway: '0.0.0.0' });
    expect(parseInnerProject(serializeInnerProject(normalized)).ip_versions).toEqual([6]);
    cfg.links[0].gateway_ipv6 = '';
    expect(normalizeInnerDraft(cfg).links[0].gateway_ipv6).toBeNull();
    expect(() => parseInnerProject(serializeInnerProject(normalizeInnerDraft(cfg)))).toThrow('IPv6');
  });
  it.each(['::', '::1', 'ff02::1', '::ffff:192.168.0.1', 'fe80::1%en0', 'fe80::1/64', '[fe80::1]', 'fe80:::1', 'fe80::1 '])('拒绝不可测试或带执行端作用域的地址 %s', (ip) => {
    expect(innerIpv6(ip)).toBe(false);
  });
  it.each(['fe80::1', 'febf::2', 'fd12:3456::1', '2001:db8::1'])('接受 IPv6 单播 %s', (ip) => {
    expect(innerIpv6(ip)).toBe(true);
  });
  it('等价地址与 /10 链路本地作用域校验一致', () => {
    expect(canonicalInnerIpv6('FE80:0:0:0:0:0:0:1')).toBe('fe80::1');
    expect(innerIpv6LinkLocal('febf::1')).toBe(true);
    expect(innerIpv6LinkLocal('fec0::1')).toBe(false);
    const cfg = defaultInnerConfig(); cfg.ip_versions = [6];
    cfg.links = [link({ local_ipv6: 'fe80::1', gateway_ipv6: 'FE80:0:0:0:0:0:0:1' })];
    expect(() => parseInnerProject(serializeInnerProject(cfg))).toThrow('不能等于');
    cfg.links[0].gateway_ipv6 = 'fd12::1';
    expect(() => parseInnerProject(serializeInnerProject(cfg))).toThrow('同为');
    cfg.links[0].gateway_ipv6 = 'febf::2';
    expect(parseInnerProject(serializeInnerProject(cfg)).ip_versions).toEqual([6]);
  });
  it('同接口等价 IPv6 不能作为两条参与链路重复加入', () => {
    const cfg = defaultInnerConfig(); cfg.ip_versions = [6];
    cfg.links = [link({ name: 'a', local_ipv6: 'fe80::100', gateway_ipv6: 'fe80::1' }), link({ name: 'b', local_ipv6: 'FE80:0:0:0:0:0:0:100', gateway_ipv6: 'fe80::2' })];
    expect(() => parseInnerProject(serializeInnerProject(cfg))).toThrow('同一网口');
  });
  it.each([{ versions: [] }, { versions: [4, 4] }, { versions: [5] }, { versions: ['6'] }])('拒绝无效 IP 版本选择 $versions', ({ versions }) => {
    expect(() => parseInnerProject(JSON.stringify({ ...defaultInnerConfig(), ip_versions: versions }))).toThrow('IP 版本');
  });
});

it('历史中间态和旧记录不冒充完成', () => {
  expect(innerHistoryStatus({ finished: false, probe_only: false })).toBe('未收尾');
  expect(innerHistoryStatus({ probe_only: false })).toContain('未知');
  expect(innerHistoryStatus({ finished: true, probe_only: false })).toBe('已完成');
  expect(innerHistoryStatus({ finished: true, probe_only: true })).toBe('仅探测');
});
