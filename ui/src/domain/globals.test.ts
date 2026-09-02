import { describe, expect, it } from 'vitest';
import {
  activeNicPolicies,
  defaultNumberPlaceholder,
  emptyGlobals,
  formatNumberList,
  formatTokenList,
  globalsAreEmpty,
  canonicalWifiBand,
  normalizeGlobals,
  wifiBandLabel,
  WIFI_BAND_24G,
  WIFI_BAND_5G,
  WIFI_BAND_6G,
  WIFI_BAND_UNKNOWN,
  resolveEffectiveGlobals,
  parseNumberList,
  parseTokenList,
  policyFor,
  setNicPolicy,
  setWifiBandThreshold,
  setWifiPairThreshold,
  wifiBandPairRows,
  wifiBandThresholdFor,
  wifiPairThresholdFor,
  type UiNicPolicy,
} from './globals';

describe('档位串', () => {
  it('收逗号、中文逗号、顿号和空白', () => {
    expect(parseTokenList('4m, 64k、256m　1m')).toEqual(['4m', '64k', '256m', '1m']);
  });

  it('空串与全分隔符给空数组——由后端决定实际使用值', () => {
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
  it('把后端有效值显示成明确的默认值提示', () => {
    expect(defaultNumberPlaceholder(10)).toBe('默认 10');
    expect(defaultNumberPlaceholder(12.5)).toBe('默认 12.5');
    expect(defaultNumberPlaceholder(0)).toBe('默认值加载中');
    expect(defaultNumberPlaceholder(undefined)).toBe('默认值加载中');
  });

  it('新建的一份是全空的', () => {
    const globals = emptyGlobals();
    expect(globalsAreEmpty(globals)).toBe(true);
  });

  it('任意一项被填就不再算空', () => {
    const globals = emptyGlobals();
    globals.udp_streams = 4;
    expect(globalsAreEmpty(globals)).toBe(false);
  });

  it('Ping 高级 RTT 门限也属于有效覆盖项', () => {
    const globals = emptyGlobals();
    globals.ping_wired_small_max_rtt_ms = 12.5;
    expect(globalsAreEmpty(globals)).toBe(false);
  });

  it('Wi-Fi 互测门限属于全局覆盖项', () => {
    const globals = emptyGlobals();
    globals.wifi_band_thresholds = setWifiBandThreshold([], 'wifi_5g', 'wifi_5g', {
      bidir_total_rx_target_mbps: 900,
    });
    expect(globalsAreEmpty(globals)).toBe(false);
  });

  it('每个频段组合保存两个单向门限和一个双向合计门限', () => {
    const rules = setWifiBandThreshold([], 'wifi_5g', 'wifi_2_4g', {
      rx_target_master_to_agent_mbps: 650,
      rx_target_agent_to_master_mbps: 420,
      bidir_total_rx_target_mbps: 700,
    });
    expect(wifiBandThresholdFor(rules, 'wifi_5g', 'wifi_2_4g')).toMatchObject({
      rx_target_master_to_agent_mbps: 650,
      rx_target_agent_to_master_mbps: 420,
      bidir_total_rx_target_mbps: 700,
    });
  });

  it('旧的两个方向双向门限按两者之和迁移成合计', () => {
    const migrated = normalizeGlobals({
      wifi_band_thresholds: [
        {
          master_band: 'wifi_5g',
          agent_band: 'wifi_5g',
          bidir_rx_target_master_to_agent_mbps: 510,
          bidir_rx_target_agent_to_master_mbps: 390,
        },
      ],
    });
    expect(migrated.wifi_band_thresholds[0]?.bidir_total_rx_target_mbps).toBe(900);
  });

  it('只填了一个方向的旧双向门限不擅自推导合计', () => {
    const migrated = normalizeGlobals({
      wifi_band_thresholds: [
        {
          master_band: 'wifi_5g',
          agent_band: 'wifi_5g',
          bidir_rx_target_master_to_agent_mbps: 700,
        },
      ],
    });
    expect(migrated.wifi_band_thresholds).toEqual([]);
  });

  it('只生成两端实际识别到的频段组合，并按频段去重', () => {
    const nic = (name: string, band: string) => ({
      name,
      description: '',
      role: 'WIFI',
      ipv4: '',
      gateway_v4: '',
      ipv6_ll: '',
      ipv6_global: '',
      zone: '',
      speed_mbps: 0,
      is_wifi: true,
      wifi_band: band,
      ifindex: 0,
    });
    expect(
      wifiBandPairRows(
        [nic('master-1', 'wifi_5g'), nic('master-2', '5g')],
        [nic('agent-1', '5G'), nic('agent-2', 'wifi_5g')],
      ),
    ).toEqual([{ masterBand: 'wifi_5g', agentBand: 'wifi_5g' }]);
    expect(wifiBandPairRows([nic('master', 'wifi_2_4g')], [nic('agent', '2.4G')])).toEqual([
      { masterBand: 'wifi_2_4g', agentBand: 'wifi_2_4g' },
    ]);
  });

  it('旧版具体网口对读取兼容支持四个方向值', () => {
    const src = 'master:NAME=WLAN 5';
    const dst = 'agent:NAME=WLAN 2';
    let rules = setWifiPairThreshold([], src, dst, {
      rx_target_ab_mbps: 700,
      rx_target_ba_mbps: 530,
      bidir_rx_target_ab_mbps: 340,
      bidir_rx_target_ba_mbps: 260,
    });
    expect(wifiPairThresholdFor(rules, src, dst)).toMatchObject({
      rx_target_ab_mbps: 700,
      rx_target_ba_mbps: 530,
      bidir_rx_target_ab_mbps: 340,
      bidir_rx_target_ba_mbps: 260,
    });
    rules = setWifiPairThreshold(rules, src, dst, {
      rx_target_ab_mbps: 0,
      rx_target_ba_mbps: 0,
      bidir_rx_target_ab_mbps: 0,
      bidir_rx_target_ba_mbps: 0,
    });
    expect(rules).toHaveLength(0);
  });

  it('导入畸形全局设置时保持编辑态字段形状，并迁移旧 Ping 门限', () => {
    const globals = normalizeGlobals({
      tcp_windows: '4m',
      udp_lengths: ['14k', 1400],
      ping_small_max_bytes: 128.75,
      ping_max_rtt_ms: 12.5,
      wifi_band_thresholds: [
        { src_band: 'wifi_5g', dst_band: 'wifi_2_4g', rx_target_mbps: 700 },
        { src_band: '', dst_band: 'wifi_5g', rx_target_mbps: 900 },
      ],
      wifi_pair_thresholds: 'not-an-array',
    });
    expect(globals.tcp_windows).toEqual([]);
    expect(globals.udp_lengths).toEqual(['14k']);
    expect(globals.ping_small_max_bytes).toBe(128);
    expect(globals.ping_wired_small_max_rtt_ms).toBe(12.5);
    expect(globals.wifi_band_thresholds).toHaveLength(1);
    expect(globals.wifi_band_thresholds[0]).toMatchObject({
      master_band: 'wifi_5g',
      agent_band: 'wifi_2_4g',
      rx_target_master_to_agent_mbps: 700,
    });
    expect(globals.wifi_pair_thresholds).toEqual([]);
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
    list = setNicPolicy(list, endpoint, { udp_bandwidth: '1000m', udp_length: '1400' });
    expect(policyFor(list, endpoint)).toEqual({
      endpoint,
      rx_target: '90%',
      udp_bandwidth: '1000m',
      udp_length: '1400',
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

describe('导出前的有效值换算', () => {
  const bootstrap = {
    agent_host: '',
    agent_port: 28801,
    token_configured: true,
    ipv4_prefixes: [],
    duration: 180,
    tcp_windows: ['4m'],
    tcp_streams: [10],
    udp_bandwidths: ['1m', '1000m', '2500m'],
    udp_lengths: ['64'],
    udp_windows: [],
    udp_streams: 1,
    ping_count: 30,
    ping_payload_sizes: [32, 1600, 65500],
    ping_max_rtt_ms: 30,
    ping_small_max_bytes: 128,
    ping_medium_max_bytes: 2000,
    ping_wired_small_avg_rtt_ms: 10,
    ping_wired_small_max_rtt_ms: 30,
    ping_wired_medium_avg_rtt_ms: 20,
    ping_wired_medium_max_rtt_ms: 50,
    ping_wired_large_avg_rtt_ms: 50,
    ping_wired_large_max_rtt_ms: 100,
    ping_wifi_small_avg_rtt_ms: 30,
    ping_wifi_small_max_rtt_ms: 80,
    ping_wifi_medium_avg_rtt_ms: 50,
    ping_wifi_medium_max_rtt_ms: 100,
    ping_wifi_large_avg_rtt_ms: 100,
    ping_wifi_large_max_rtt_ms: 200,
    rate_targets_mbps: { forward: 1200, ab: null, ba: null },
    rate_mode: 'verify',
    udp_profiles: [{ bandwidth: '1m' }, { bandwidth: '1000m', length: '64' }],
    screenshot: false,
    ui_plan_supported: true,
  };

  it('留空的格子换算成主控当前生效值', () => {
    const resolved = resolveEffectiveGlobals(emptyGlobals(), bootstrap);
    expect(resolved.ping_count).toBe(30);
    expect(resolved.ping_payload_sizes).toEqual([32, 1600, 65500]);
    expect(resolved.ping_wifi_large_max_rtt_ms).toBe(200);
    expect(resolved.tcp_windows).toEqual(['4m']);
    expect(resolved.global_rate_targets).toEqual({ forward: 1200, ab: null, ba: null });
    expect(resolved.global_rate_mode).toBe('verify');
  });

  it('三条 UDP 轴全空时钉住原样档位表，而不是拆成会叉乘的三条轴', () => {
    // 主控内置基线只有 1000m 带 -l 64。拆成「带宽 × 长度」再叉乘，
    // 会变成两档全带 -l 64，灌包条件当场就变了。
    const resolved = resolveEffectiveGlobals(emptyGlobals(), bootstrap);
    expect(resolved.udp_profiles).toEqual([
      { bandwidth: '1m' },
      { bandwidth: '1000m', length: '64' },
    ]);
    expect(resolved.udp_bandwidths).toEqual([]);
    expect(resolved.udp_lengths).toEqual([]);
  });

  it('单独留空的 -l / -w 是「明确不下发」，不许回落', () => {
    const globals = { ...emptyGlobals(), udp_bandwidths: ['2500m'] };
    const resolved = resolveEffectiveGlobals(globals, bootstrap);
    expect(resolved.udp_lengths).toEqual([]);
    expect(resolved.udp_windows).toEqual([]);
    expect(resolved.udp_profiles).toEqual([]);
  });

  it('TCP 流数的兜底是 1，不是主控档位表', () => {
    // 后端的兜底是「取不到就 1」。写成 bootstrap.tcp_streams（这里是 10）
    // 会当场改变本机的测试内容——导出不该顺手改测试。
    expect(resolveEffectiveGlobals(emptyGlobals(), bootstrap).tcp_streams).toEqual([1]);
  });

  it('已经填过的值一个都不动', () => {
    const globals = {
      ...emptyGlobals(),
      ping_count: 7,
      tcp_windows: ['64k'],
      udp_streams: 4,
      global_rate_targets: { forward: null, ab: 900, ba: null },
    };
    const resolved = resolveEffectiveGlobals(globals, bootstrap);
    expect(resolved.ping_count).toBe(7);
    expect(resolved.tcp_windows).toEqual(['64k']);
    expect(resolved.udp_streams).toBe(4);
    expect(resolved.global_rate_targets).toEqual({ forward: null, ab: 900, ba: null });
  });

  it('还没拿到 bootstrap 时原样返回，不伪造默认值', () => {
    expect(resolveEffectiveGlobals(emptyGlobals(), null)).toEqual(emptyGlobals());
  });
});

describe('频段的稳定枚举', () => {
  it('把各种写法收敛成同一个词', () => {
    for (const raw of ['5GHz', '5g', 'WIFI5G', 'wifi_5g', '5 GHz']) {
      expect(canonicalWifiBand(raw), raw).toBe(WIFI_BAND_5G);
    }
    for (const raw of ['2.4GHz', '2.4G', 'WIFI2.4G', 'wifi_2_4g', '24g']) {
      expect(canonicalWifiBand(raw), raw).toBe(WIFI_BAND_24G);
    }
    for (const raw of ['6GHz', '6g', 'wifi_6g']) {
      expect(canonicalWifiBand(raw), raw).toBe(WIFI_BAND_6G);
    }
    expect(canonicalWifiBand('以太网')).toBe(WIFI_BAND_UNKNOWN);
    expect(canonicalWifiBand('')).toBe(WIFI_BAND_UNKNOWN);
  });

  it('旧项目里存的展示串在读入时被收敛，规则不会失效', () => {
    const migrated = normalizeGlobals({
      wifi_band_thresholds: [
        {
          master_band: '5GHz',
          agent_band: '2.4GHz',
          rx_target_master_to_agent_mbps: 700,
        },
      ],
    });
    expect(migrated.wifi_band_thresholds[0]).toMatchObject({
      master_band: WIFI_BAND_5G,
      agent_band: WIFI_BAND_24G,
    });
    // 用旧写法也照样查得到——查找同样走枚举。
    expect(
      wifiBandThresholdFor(migrated.wifi_band_thresholds, '5GHz', '2.4GHz')
        .rx_target_master_to_agent_mbps,
    ).toBe(700);
  });

  it('界面文案改了也不影响规则匹配', () => {
    // 这条守的正是「展示串当键」的老毛病：wifiBandLabel 只负责显示。
    expect(wifiBandLabel(WIFI_BAND_5G)).toBe('5G');
    expect(wifiBandLabel(WIFI_BAND_24G)).toBe('2.4G');
    expect(wifiBandLabel(WIFI_BAND_6G)).toBe('6G');
    expect(wifiBandLabel(WIFI_BAND_UNKNOWN)).toBe('未知频段');
  });
});
