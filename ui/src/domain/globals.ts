/** 执行请求里跨套件生效的全局默认档位与策略。 */

import type { BootstrapOut, NicInfo, RateTargets, UdpProfile } from '../api/dto';

export function parseTokenList(raw: string): string[] {
  return String(raw ?? '')
    .split(/[,，、\s]+/)
    .map((token) => token.trim())
    .filter((token) => token !== '');
}

export function formatTokenList(values: readonly string[]): string {
  return values.join(', ');
}

export function parseNumberList(raw: string): number[] {
  return parseTokenList(raw)
    .map((token) => Number(token))
    .filter((value) => Number.isFinite(value) && value > 0)
    .map((value) => Math.trunc(value));
}

export function formatNumberList(values: readonly number[]): string {
  return values.join(', ');
}

/** 空覆盖项在输入框里直接说明主控当前会采用的数值。 */
export function defaultNumberPlaceholder(value: unknown): string {
  return typeof value === 'number' && Number.isFinite(value) && value > 0
    ? `默认 ${value}`
    : '默认值加载中';
}

/** 一组主控/辅测 Wi-Fi 频段组合门限。空值用 0 表示不覆盖。 */
export interface UiWifiBandThreshold {
  master_band: string;
  agent_band: string;
  rx_target_master_to_agent_mbps: number;
  rx_target_agent_to_master_mbps: number;
  /**
   * 双向并发下**两端 RX 合计**的门限。
   *
   * 取代了曾经的两个「每方向双向门限」：Wi-Fi 之间抢同一段空口时间，两个方向
   * 怎么分完全取决于调度，要求各自达到一半没有物理依据。判定口径是
   * `AB 接收端 RX + BA 接收端 RX >= 合计`。
   */
  bidir_total_rx_target_mbps: number;
}

/** 兼容旧项目的具体 Wi-Fi 网口覆盖；新界面不再创建。 */
export interface UiWifiPairThreshold {
  src_endpoint: string;
  dst_endpoint: string;
  rx_target_ab_mbps: number;
  rx_target_ba_mbps: number;
  bidir_rx_target_ab_mbps: number;
  bidir_rx_target_ba_mbps: number;
}

export interface UiGlobals {
  tcp_windows: string[];
  tcp_streams: number[];
  udp_bandwidths: string[];
  udp_lengths: string[];
  udp_windows: string[];
  /** 0 表示不覆盖。 */
  udp_streams: number;
  /** 0 表示不覆盖当前测试配置。 */
  ping_count: number;
  /** 空数组表示不覆盖当前测试配置。 */
  ping_payload_sizes: number[];
  ping_small_max_bytes: number;
  ping_medium_max_bytes: number;
  ping_wired_small_avg_rtt_ms: number;
  ping_wired_small_max_rtt_ms: number;
  ping_wired_medium_avg_rtt_ms: number;
  ping_wired_medium_max_rtt_ms: number;
  ping_wired_large_avg_rtt_ms: number;
  ping_wired_large_max_rtt_ms: number;
  ping_wifi_small_avg_rtt_ms: number;
  ping_wifi_small_max_rtt_ms: number;
  ping_wifi_medium_avg_rtt_ms: number;
  ping_wifi_medium_max_rtt_ms: number;
  ping_wifi_large_avg_rtt_ms: number;
  ping_wifi_large_max_rtt_ms: number;
  /** 兼容旧项目的统一单向门限。 */
  wifi_pair_rx_target_mbps: number;
  /** 兼容旧项目的统一「每方向」双向门限；读入时按 ×2 迁移成合计。 */
  wifi_pair_bidir_rx_target_mbps: number;
  /** 全局 Wi-Fi 双向 RX 合计门限；频段表没填时的最后一层兜底。 */
  wifi_pair_bidir_total_rx_target_mbps: number;
  /** 按主控频段 × 辅测频段的 Wi-Fi 门限（两个单向 + 一个双向合计）。 */
  wifi_band_thresholds: UiWifiBandThreshold[];
  /** 兼容旧项目的具体网口覆盖；新界面不再创建。 */
  wifi_pair_thresholds: UiWifiPairThreshold[];
  /**
   * 钉死的 UDP 档位原样列表；空 = 不钉，走主控 config.json。
   *
   * 三条轴（带宽/长度/窗口）是**叉乘**语义，而主控档位表是逐条列出来的：
   * 内置基线只有 `1000m` 带 `-l 64`。把它拆回三条轴再叉乘会变成五档全带
   * `-l 64`，灌包条件当场就变了。项目要能跨机复现，就只能钉原样列表。
   */
  udp_profiles: UdpProfile[];
  /**
   * 钉死的**全局** RX 门限；`null` = 不钉，走目标主控自己的 config.json。
   *
   * 界面上没有输入框，但它实实在在参与判定。项目快照必须带走它，否则同一份
   * 项目在另一台主控上会改用那台机器的门限，「怎么判定」静默变了。
   * 三个字段全是 `null` 的对象也是有意义的：那是「本项目明确没有全局门限」。
   */
  global_rate_targets: RateTargets | null;
  /**
   * 钉死的判定模式；空串 = 不钉，走目标主控自己的 `config.json`。
   *
   * 和全局门限是一对：`observe` 只记录实测能力、不判 PASS/FAIL，`verify`
   * 没有门限时直接 `TARGET_MISSING`。界面上没有这个开关，但它决定这一轮
   * 出不出 PASS——不带走它，换台机器同一份项目可能整轮都是 MEASURED。
   */
  global_rate_mode: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringList(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === 'string')
    : [];
}

function nonNegativeNumber(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : 0;
}

function nonNegativeInteger(value: unknown): number {
  return Math.trunc(nonNegativeNumber(value));
}

function positiveIntegerList(value: unknown): number[] {
  return Array.isArray(value)
    ? value
        .filter(
          (item): item is number =>
            typeof item === 'number' && Number.isFinite(item) && item > 0,
        )
        .map((item) => Math.trunc(item))
        .filter((item) => item > 0)
    : [];
}

type LegacyWifiBandThreshold = {
  src_band: string;
  dst_band: string;
  rx_target_mbps: number;
  bidir_rx_target_mbps: number;
};

function wifiBandRuleHasValue(rule: UiWifiBandThreshold): boolean {
  return (
    rule.rx_target_master_to_agent_mbps > 0 ||
    rule.rx_target_agent_to_master_mbps > 0 ||
    rule.bidir_total_rx_target_mbps > 0
  );
}

/**
 * 旧项目里按方向填的两个双向门限，迁移成一个合计门限。
 *
 * 两个方向都填过：合计就是两者之和——这正是老口径下「双向判定通过」所要求的
 * 总量，换算不改变严格程度。只填了一个方向时**不擅自推导**：当成合计会凭空
 * 放宽一倍，翻倍又凭空收紧，两种都是替用户做决定。
 */
export function migrateBidirPairToTotal(ab: number, ba: number): number {
  return ab > 0 && ba > 0 ? ab + ba : 0;
}

function normalizeWifiBandThresholds(value: unknown): UiWifiBandThreshold[] {
  if (!Array.isArray(value)) return [];
  const canonical = new Map<string, UiWifiBandThreshold>();
  const legacy = new Map<string, LegacyWifiBandThreshold>();

  for (const item of value) {
    if (!isRecord(item)) continue;
    const rawMaster = typeof item.master_band === 'string' ? item.master_band.trim() : '';
    const rawAgent = typeof item.agent_band === 'string' ? item.agent_band.trim() : '';
    if (rawMaster && rawAgent) {
      const master_band = canonicalWifiBand(rawMaster);
      const agent_band = canonicalWifiBand(rawAgent);
      const rule: UiWifiBandThreshold = {
        master_band,
        agent_band,
        rx_target_master_to_agent_mbps: nonNegativeNumber(
          item.rx_target_master_to_agent_mbps,
        ),
        rx_target_agent_to_master_mbps: nonNegativeNumber(
          item.rx_target_agent_to_master_mbps,
        ),
        bidir_total_rx_target_mbps:
          nonNegativeNumber(item.bidir_total_rx_target_mbps) ||
          migrateBidirPairToTotal(
            nonNegativeNumber(item.bidir_rx_target_master_to_agent_mbps),
            nonNegativeNumber(item.bidir_rx_target_agent_to_master_mbps),
          ),
      };
      if (wifiBandRuleHasValue(rule)) canonical.set(`${master_band}\u0000${agent_band}`, rule);
      continue;
    }

    // v6.2.2 试验版按“发送频段 → 接收频段”存两列。迁移成频段组合时，
    // 正向规则填主控→辅测，反向规则填辅测→主控；同频规则自然得到两边相同值。
    const rawSrc = typeof item.src_band === 'string' ? item.src_band.trim() : '';
    const rawDst = typeof item.dst_band === 'string' ? item.dst_band.trim() : '';
    if (!rawSrc || !rawDst) continue;
    const src_band = canonicalWifiBand(rawSrc);
    const dst_band = canonicalWifiBand(rawDst);
    const rule: LegacyWifiBandThreshold = {
      src_band,
      dst_band,
      rx_target_mbps: nonNegativeNumber(item.rx_target_mbps),
      bidir_rx_target_mbps: nonNegativeNumber(item.bidir_rx_target_mbps),
    };
    if (rule.rx_target_mbps > 0 || rule.bidir_rx_target_mbps > 0) {
      legacy.set(`${src_band}\u0000${dst_band}`, rule);
    }
  }

  for (const rule of legacy.values()) {
    const key = `${rule.src_band}\u0000${rule.dst_band}`;
    if (canonical.has(key)) continue;
    const reverse = legacy.get(`${rule.dst_band}\u0000${rule.src_band}`);
    const migrated: UiWifiBandThreshold = {
      master_band: rule.src_band,
      agent_band: rule.dst_band,
      rx_target_master_to_agent_mbps: rule.rx_target_mbps,
      rx_target_agent_to_master_mbps: reverse?.rx_target_mbps ?? 0,
      bidir_total_rx_target_mbps: migrateBidirPairToTotal(
        rule.bidir_rx_target_mbps,
        reverse?.bidir_rx_target_mbps ?? 0,
      ),
    };
    if (wifiBandRuleHasValue(migrated)) canonical.set(key, migrated);
  }

  return [...canonical.values()];
}

function normalizeWifiPairThresholds(value: unknown): UiWifiPairThreshold[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((item): UiWifiPairThreshold[] => {
    if (!isRecord(item)) return [];
    const src_endpoint = typeof item.src_endpoint === 'string' ? item.src_endpoint.trim() : '';
    const dst_endpoint = typeof item.dst_endpoint === 'string' ? item.dst_endpoint.trim() : '';
    if (!src_endpoint || !dst_endpoint) return [];
    const rule = {
      src_endpoint,
      dst_endpoint,
      rx_target_ab_mbps: nonNegativeNumber(item.rx_target_ab_mbps),
      rx_target_ba_mbps: nonNegativeNumber(item.rx_target_ba_mbps),
      bidir_rx_target_ab_mbps: nonNegativeNumber(item.bidir_rx_target_ab_mbps),
      bidir_rx_target_ba_mbps: nonNegativeNumber(item.bidir_rx_target_ba_mbps),
    };
    return Object.values(rule).some((value) => typeof value === 'number' && value > 0)
      ? [rule]
      : [];
  });
}

export function emptyGlobals(): UiGlobals {
  return {
    tcp_windows: [],
    tcp_streams: [],
    udp_bandwidths: [],
    udp_lengths: [],
    udp_windows: [],
    udp_streams: 0,
    ping_count: 0,
    ping_payload_sizes: [],
    ping_small_max_bytes: 0,
    ping_medium_max_bytes: 0,
    ping_wired_small_avg_rtt_ms: 0,
    ping_wired_small_max_rtt_ms: 0,
    ping_wired_medium_avg_rtt_ms: 0,
    ping_wired_medium_max_rtt_ms: 0,
    ping_wired_large_avg_rtt_ms: 0,
    ping_wired_large_max_rtt_ms: 0,
    ping_wifi_small_avg_rtt_ms: 0,
    ping_wifi_small_max_rtt_ms: 0,
    ping_wifi_medium_avg_rtt_ms: 0,
    ping_wifi_medium_max_rtt_ms: 0,
    ping_wifi_large_avg_rtt_ms: 0,
    ping_wifi_large_max_rtt_ms: 0,
    wifi_pair_rx_target_mbps: 0,
    wifi_pair_bidir_rx_target_mbps: 0,
    wifi_pair_bidir_total_rx_target_mbps: 0,
    wifi_band_thresholds: [],
    wifi_pair_thresholds: [],
    udp_profiles: [],
    global_rate_targets: null,
    global_rate_mode: '',
  };
}

/**
 * 把项目文件、浏览器草稿或历史 request.json 的未知值收敛成安全的编辑态。
 *
 * 这些入口都允许读取旧文件，不能用类型断言代替运行时清洗：一个错误的数组
 * 如果原样写进 reactive state，界面会在 `.join()` 或 `.length` 处直接崩溃。
 * 旧版的单一 `ping_max_rtt_ms` 按原后端语义迁移到有线 small 的 Max RTT。
 */
export function normalizeGlobals(value: unknown): UiGlobals {
  const source = isRecord(value) ? value : {};
  const globals = emptyGlobals();
  globals.tcp_windows = stringList(source.tcp_windows);
  globals.tcp_streams = positiveIntegerList(source.tcp_streams);
  globals.udp_bandwidths = stringList(source.udp_bandwidths);
  globals.udp_lengths = stringList(source.udp_lengths);
  globals.udp_windows = stringList(source.udp_windows);
  globals.udp_streams = nonNegativeInteger(source.udp_streams);
  globals.ping_count = nonNegativeInteger(source.ping_count);
  globals.ping_payload_sizes = positiveIntegerList(source.ping_payload_sizes);
  globals.ping_small_max_bytes = nonNegativeInteger(source.ping_small_max_bytes);
  globals.ping_medium_max_bytes = nonNegativeInteger(source.ping_medium_max_bytes);

  const decimalKeys = [
    'ping_wired_small_avg_rtt_ms',
    'ping_wired_small_max_rtt_ms',
    'ping_wired_medium_avg_rtt_ms',
    'ping_wired_medium_max_rtt_ms',
    'ping_wired_large_avg_rtt_ms',
    'ping_wired_large_max_rtt_ms',
    'ping_wifi_small_avg_rtt_ms',
    'ping_wifi_small_max_rtt_ms',
    'ping_wifi_medium_avg_rtt_ms',
    'ping_wifi_medium_max_rtt_ms',
    'ping_wifi_large_avg_rtt_ms',
    'ping_wifi_large_max_rtt_ms',
    'wifi_pair_rx_target_mbps',
    'wifi_pair_bidir_rx_target_mbps',
    'wifi_pair_bidir_total_rx_target_mbps',
  ] as const;
  for (const key of decimalKeys) globals[key] = nonNegativeNumber(source[key]);
  if (globals.ping_wired_small_max_rtt_ms === 0) {
    globals.ping_wired_small_max_rtt_ms = nonNegativeNumber(source.ping_max_rtt_ms);
  }
  globals.wifi_band_thresholds = normalizeWifiBandThresholds(source.wifi_band_thresholds);
  globals.wifi_pair_thresholds = normalizeWifiPairThresholds(source.wifi_pair_thresholds);
  globals.udp_profiles = normalizeUdpProfiles(source.udp_profiles);
  globals.global_rate_targets = normalizeRateTargets(source.global_rate_targets);
  globals.global_rate_mode = normalizeRateMode(source.global_rate_mode);
  return globals;
}

/** 一档 UDP 参数的运行时清洗；`bandwidth` 是必需的，其余两项可缺省。 */
export function normalizeUdpProfiles(value: unknown): UdpProfile[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((item): UdpProfile[] => {
    if (!isRecord(item)) return [];
    const bandwidth = typeof item.bandwidth === 'string' ? item.bandwidth.trim() : '';
    if (!bandwidth) return [];
    const optional = (raw: unknown): string | undefined => {
      const text = typeof raw === 'string' ? raw.trim() : '';
      return text === '' ? undefined : text;
    };
    return [
      {
        bandwidth,
        ...(optional(item.length) ? { length: optional(item.length) } : {}),
        ...(optional(item.window) ? { window: optional(item.window) } : {}),
      },
    ];
  });
}

/** 判定模式只认这四个词；别的一律当作「没钉」。 */
export function normalizeRateMode(value: unknown): string {
  const text = typeof value === 'string' ? value.trim().toLowerCase() : '';
  return ['auto', 'verify', 'observe', 'discover'].includes(text) ? text : '';
}

/**
 * 全局门限的运行时清洗。
 *
 * `null`/非对象 → `null`（不钉）。是对象就逐字段清洗，**保留全 null 的对象**：
 * 那是「明确没有全局门限」，和「没意见」是两件事。
 */
export function normalizeRateTargets(value: unknown): RateTargets | null {
  if (!isRecord(value)) return null;
  const positive = (raw: unknown): number | null =>
    typeof raw === 'number' && Number.isFinite(raw) && raw > 0 ? raw : null;
  return {
    forward: positive(value.forward),
    ab: positive(value.ab),
    ba: positive(value.ba),
  };
}

export function defaultGlobals(): UiGlobals {
  return {
    ...emptyGlobals(),
    udp_bandwidths: ['2500m'],
    tcp_windows: ['4m'],
  };
}

export function globalsAreEmpty(globals: UiGlobals): boolean {
  return (
    globals.tcp_windows.length === 0 &&
    globals.tcp_streams.length === 0 &&
    globals.udp_bandwidths.length === 0 &&
    globals.udp_lengths.length === 0 &&
    globals.udp_windows.length === 0 &&
    globals.udp_streams === 0 &&
    globals.ping_count === 0 &&
    globals.ping_payload_sizes.length === 0 &&
    globals.ping_small_max_bytes === 0 &&
    globals.ping_medium_max_bytes === 0 &&
    globals.ping_wired_small_avg_rtt_ms === 0 &&
    globals.ping_wired_small_max_rtt_ms === 0 &&
    globals.ping_wired_medium_avg_rtt_ms === 0 &&
    globals.ping_wired_medium_max_rtt_ms === 0 &&
    globals.ping_wired_large_avg_rtt_ms === 0 &&
    globals.ping_wired_large_max_rtt_ms === 0 &&
    globals.ping_wifi_small_avg_rtt_ms === 0 &&
    globals.ping_wifi_small_max_rtt_ms === 0 &&
    globals.ping_wifi_medium_avg_rtt_ms === 0 &&
    globals.ping_wifi_medium_max_rtt_ms === 0 &&
    globals.ping_wifi_large_avg_rtt_ms === 0 &&
    globals.ping_wifi_large_max_rtt_ms === 0 &&
    globals.wifi_pair_rx_target_mbps === 0 &&
    globals.wifi_pair_bidir_rx_target_mbps === 0 &&
    globals.wifi_pair_bidir_total_rx_target_mbps === 0 &&
    globals.wifi_band_thresholds.length === 0 &&
    globals.wifi_pair_thresholds.length === 0 &&
    globals.udp_profiles.length === 0 &&
    globals.global_rate_targets === null &&
    globals.global_rate_mode === ''
  );
}

export function emptyWifiBandThreshold(
  master_band: string,
  agent_band: string,
): UiWifiBandThreshold {
  return {
    master_band: canonicalWifiBand(master_band),
    agent_band: canonicalWifiBand(agent_band),
    rx_target_master_to_agent_mbps: 0,
    rx_target_agent_to_master_mbps: 0,
    bidir_total_rx_target_mbps: 0,
  };
}

export function wifiBandThresholdFor(
  rules: readonly UiWifiBandThreshold[],
  master_band: string,
  agent_band: string,
): UiWifiBandThreshold {
  const master = canonicalWifiBand(master_band);
  const agent = canonicalWifiBand(agent_band);
  return (
    rules.find(
      (rule) =>
        canonicalWifiBand(rule.master_band) === master &&
        canonicalWifiBand(rule.agent_band) === agent,
    ) ?? emptyWifiBandThreshold(master, agent)
  );
}

export function setWifiBandThreshold(
  rules: readonly UiWifiBandThreshold[],
  master_band: string,
  agent_band: string,
  patch: Partial<Omit<UiWifiBandThreshold, 'master_band' | 'agent_band'>>,
): UiWifiBandThreshold[] {
  const master = canonicalWifiBand(master_band);
  const agent = canonicalWifiBand(agent_band);
  const current = wifiBandThresholdFor(rules, master, agent);
  const next = { ...current, ...patch, master_band: master, agent_band: agent };
  const rest = rules.filter(
    (rule) =>
      !(
        canonicalWifiBand(rule.master_band) === master &&
        canonicalWifiBand(rule.agent_band) === agent
      ),
  );
  return wifiBandRuleHasValue(next) ? [...rest, next] : rest;
}

export interface WifiBandPairRow {
  masterBand: string;
  agentBand: string;
}

export function nicIsWifi(nic: NicInfo): boolean {
  return nic.is_wifi || nic.wifi_band.trim() !== '' || nic.role.toUpperCase().includes('WIFI');
}

/**
 * 频段的**稳定枚举**。存进请求和项目文件的一律是这四个词。
 *
 * 与 Rust 的 `canonical_wifi_band` 是同一套词。以前两端各自产出 `'5GHz'` 这样
 * 的展示串再按字符串比较——规则一模一样所以一直没出事，但那是靠两份实现恰好
 * 同步维持的。展示文案是最容易被改的东西，而改完之后频段规则会**静默失效**：
 * 找不到规则不会报错，只是门限没了。
 */
export const WIFI_BAND_24G = 'wifi_2_4g';
export const WIFI_BAND_5G = 'wifi_5g';
export const WIFI_BAND_6G = 'wifi_6g';
export const WIFI_BAND_UNKNOWN = 'unknown';

/** 吃网卡自报值、旧项目里的展示串（`5GHz`）、以及枚举值本身。 */
export function canonicalWifiBand(raw: string): string {
  const text = String(raw ?? '').toLowerCase();
  if (text.includes('2.4') || text.includes('2_4') || text.includes('24g')) return WIFI_BAND_24G;
  if (text.includes('6')) return WIFI_BAND_6G;
  if (text.includes('5')) return WIFI_BAND_5G;
  return WIFI_BAND_UNKNOWN;
}

export function normalizedWifiBand(nic: NicInfo): string {
  return canonicalWifiBand(`${nic.wifi_band} ${nic.role}`);
}

/** 只按两端实际识别到的 Wi-Fi 组合出行，同频/同类网卡不会重复铺表。 */
export function wifiBandPairRows(
  masterNics: readonly NicInfo[],
  agentNics: readonly NicInfo[],
): WifiBandPairRow[] {
  const rows = new Map<string, WifiBandPairRow>();
  for (const master of masterNics.filter(nicIsWifi)) {
    for (const agent of agentNics.filter(nicIsWifi)) {
      const row = {
        masterBand: normalizedWifiBand(master),
        agentBand: normalizedWifiBand(agent),
      };
      rows.set(`${row.masterBand}\u0000${row.agentBand}`, row);
    }
  }
  const rank: Record<string, number> = {
    [WIFI_BAND_5G]: 0,
    [WIFI_BAND_24G]: 1,
    [WIFI_BAND_6G]: 2,
    [WIFI_BAND_UNKNOWN]: 3,
  };
  return [...rows.values()].sort(
    (left, right) =>
      (rank[left.masterBand] ?? 10) - (rank[right.masterBand] ?? 10) ||
      (rank[left.agentBand] ?? 10) - (rank[right.agentBand] ?? 10),
  );
}

/** 枚举 → 界面文案。改这里只影响显示，不会让任何一条频段规则失效。 */
export function wifiBandLabel(band: string): string {
  switch (canonicalWifiBand(band)) {
    case WIFI_BAND_5G:
      return '5G';
    case WIFI_BAND_24G:
      return '2.4G';
    case WIFI_BAND_6G:
      return '6G';
    default:
      return '未知频段';
  }
}

export function emptyWifiPairThreshold(
  src_endpoint: string,
  dst_endpoint: string,
): UiWifiPairThreshold {
  return {
    src_endpoint,
    dst_endpoint,
    rx_target_ab_mbps: 0,
    rx_target_ba_mbps: 0,
    bidir_rx_target_ab_mbps: 0,
    bidir_rx_target_ba_mbps: 0,
  };
}

export function wifiPairThresholdFor(
  rules: readonly UiWifiPairThreshold[],
  src_endpoint: string,
  dst_endpoint: string,
): UiWifiPairThreshold {
  return (
    rules.find(
      (rule) => rule.src_endpoint === src_endpoint && rule.dst_endpoint === dst_endpoint,
    ) ?? emptyWifiPairThreshold(src_endpoint, dst_endpoint)
  );
}

export function setWifiPairThreshold(
  rules: readonly UiWifiPairThreshold[],
  src_endpoint: string,
  dst_endpoint: string,
  patch: Partial<Omit<UiWifiPairThreshold, 'src_endpoint' | 'dst_endpoint'>>,
): UiWifiPairThreshold[] {
  const current = wifiPairThresholdFor(rules, src_endpoint, dst_endpoint);
  const next = { ...current, ...patch, src_endpoint, dst_endpoint };
  const rest = rules.filter(
    (rule) => !(rule.src_endpoint === src_endpoint && rule.dst_endpoint === dst_endpoint),
  );
  const hasValue =
    next.rx_target_ab_mbps > 0 ||
    next.rx_target_ba_mbps > 0 ||
    next.bidir_rx_target_ab_mbps > 0 ||
    next.bidir_rx_target_ba_mbps > 0;
  return hasValue ? [...rest, next] : rest;
}

export interface UiNicPolicy {
  endpoint: string;
  rx_target: string;
  udp_bandwidth: string;
  udp_length: string;
}

export function emptyNicPolicy(endpoint: string): UiNicPolicy {
  return { endpoint, rx_target: '', udp_bandwidth: '', udp_length: '' };
}

export function nicPolicyIsEmpty(policy: UiNicPolicy): boolean {
  return (
    policy.rx_target.trim() === '' &&
    policy.udp_bandwidth.trim() === '' &&
    policy.udp_length.trim() === ''
  );
}

export function policyFor(policies: readonly UiNicPolicy[], endpoint: string): UiNicPolicy {
  return policies.find((policy) => policy.endpoint === endpoint) ?? emptyNicPolicy(endpoint);
}

export function setNicPolicy(
  policies: readonly UiNicPolicy[],
  endpoint: string,
  patch: Partial<Omit<UiNicPolicy, 'endpoint'>>,
): UiNicPolicy[] {
  const next = { ...policyFor(policies, endpoint), ...patch, endpoint };
  const rest = policies.filter((policy) => policy.endpoint !== endpoint);
  return nicPolicyIsEmpty(next) ? rest : [...rest, next];
}

export function activeNicPolicies(policies: readonly UiNicPolicy[]): UiNicPolicy[] {
  return policies.filter((policy) => !nicPolicyIsEmpty(policy));
}

// ---------------------------------------------------------------------------
// 有效值快照
// ---------------------------------------------------------------------------

/**
 * 把「输入框状态」换算成「主控当前**真正会用**的值」。
 *
 * 导出项目时必须走这一层。界面上留空的格子显示的是灰字 `默认 30`，那个 30 只
 * 存在于 bootstrap 回填里，从来没有进过 `plan.globals`——直接序列化编辑态，
 * 导出的 Ping 次数就是 `0`、TCP 窗口就是 `[]`。换一台主控导入，后端会改用那台
 * 机器自己的默认值，「怎么判定」静默变了，而项目文件上看不出来。
 *
 * 每一条兜底都必须和**后端真正的兜底**一致，不能照抄界面 placeholder：
 *
 * - `tcp_windows` 空 → 主控 `iperf.tcp_windows`（后端 `non_empty`）。
 * - `tcp_streams` 空 → `[1]`。后端的兜底是「取不到就 1」，不是主控档位表；
 *   写成主控档位表会当场改变本机的测试内容。
 * - `udp_*` 三条轴全空 → 钉 `udp_profiles` 原样列表，三条轴保持空。
 *   三条轴是叉乘语义，还原不回逐条档位（见 `UiGlobals.udp_profiles`）。
 * - `udp_lengths` / `udp_windows` 单独留空 → **保持空**：那是「明确不下发
 *   `-l`/`-w`」，回落回去等于替用户加参数。
 * - `udp_streams` 0 → `1`（后端 `max(1)`）。
 * - Ping 的次数、包长、RTT 门限空/0 → 主控当前生效值。
 */
export function resolveEffectiveGlobals(
  globals: UiGlobals,
  bootstrap: BootstrapOut | null,
): UiGlobals {
  const next: UiGlobals = { ...globals };
  if (!bootstrap) return next;

  const orNumber = (value: number, fallback: number): number =>
    value > 0 ? value : Number.isFinite(fallback) && fallback > 0 ? fallback : value;

  if (next.tcp_windows.length === 0) next.tcp_windows = [...bootstrap.tcp_windows];
  if (next.tcp_streams.length === 0) next.tcp_streams = [1];

  const udpAxesAllEmpty =
    next.udp_bandwidths.length === 0 &&
    next.udp_lengths.length === 0 &&
    next.udp_windows.length === 0;
  if (udpAxesAllEmpty) {
    next.udp_profiles = normalizeUdpProfiles(bootstrap.udp_profiles);
  } else if (next.udp_bandwidths.length === 0) {
    // 轴填了一半：带宽回落到主控档位表的带宽集合，与后端 `fallback_bandwidths`
    // 同一口径；`-l`/`-w` 保持用户填的那份。
    next.udp_bandwidths = [...bootstrap.udp_bandwidths];
  }
  if (next.udp_streams <= 0) next.udp_streams = 1;

  next.ping_count = orNumber(next.ping_count, bootstrap.ping_count);
  if (next.ping_payload_sizes.length === 0) {
    next.ping_payload_sizes = [...bootstrap.ping_payload_sizes];
  }
  next.ping_small_max_bytes = orNumber(next.ping_small_max_bytes, bootstrap.ping_small_max_bytes);
  next.ping_medium_max_bytes = orNumber(
    next.ping_medium_max_bytes,
    bootstrap.ping_medium_max_bytes,
  );
  const rttKeys = [
    'ping_wired_small_avg_rtt_ms',
    'ping_wired_small_max_rtt_ms',
    'ping_wired_medium_avg_rtt_ms',
    'ping_wired_medium_max_rtt_ms',
    'ping_wired_large_avg_rtt_ms',
    'ping_wired_large_max_rtt_ms',
    'ping_wifi_small_avg_rtt_ms',
    'ping_wifi_small_max_rtt_ms',
    'ping_wifi_medium_avg_rtt_ms',
    'ping_wifi_medium_max_rtt_ms',
    'ping_wifi_large_avg_rtt_ms',
    'ping_wifi_large_max_rtt_ms',
  ] as const;
  for (const key of rttKeys) next[key] = orNumber(next[key], bootstrap[key]);

  // 全局门限没被项目钉过就固化主控当前那一份。导入方因此不会回落到自己的
  // config.json——这正是「换机导入判定不变」的关键一条。
  next.global_rate_targets = next.global_rate_targets ?? {
    forward: bootstrap.rate_targets_mbps?.forward ?? null,
    ab: bootstrap.rate_targets_mbps?.ab ?? null,
    ba: bootstrap.rate_targets_mbps?.ba ?? null,
  };
  next.global_rate_mode = next.global_rate_mode || normalizeRateMode(bootstrap.rate_mode);
  return next;
}
