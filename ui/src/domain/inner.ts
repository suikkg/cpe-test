import type { HostInfo, NicInfo } from '../api/dto';

export const INNER_KIND = 'cpe-inner-project';
/** v3 增加 IP 版本；v1/v2 缺少版本选择时保持 IPv4。 */
export const INNER_VERSION = 3;
export const INNER_DRAFT_KEY = 'cpe_inner_project_v1';
export const INNER_DEFAULT_GATEWAY = '192.168.0.1';
export const INNER_DEFAULT_BOARD_RX = 'br0';
export const MAX_AGENT_ADDRESS_BYTES = 256;
export const MAX_AGENT_TOKEN_BYTES = 4096;
export interface InnerAgent { id: string; address: string; port: number; token?: string }
export type InnerProtocol = 'tcp' | 'udp';
export type InnerIpVersion = 4 | 6;
export const INNER_IP_VERSIONS: InnerIpVersion[] = [4, 6];
/** 页面上的方向。`bidir` 是一个含上下行两条腿的单元，不是两次顺序单向。 */
export type InnerDirection = 'upload' | 'download' | 'bidir';
/** 一条腿的数据走向。 */
export type InnerFlow = 'up' | 'down';
/** 接收速率取自哪一层。 */
export type InnerMeasurement = 'nic_strict' | 'nic_preferred' | 'tool';
export const INNER_PROTOCOLS: InnerProtocol[] = ['tcp', 'udp'];
export const INNER_DIRECTIONS: InnerDirection[] = ['upload', 'download', 'bidir'];
export const INNER_MEASUREMENTS: InnerMeasurement[] = ['nic_strict', 'nic_preferred', 'tool'];
export const PROTOCOL_LABEL: Record<InnerProtocol, string> = { tcp: 'TCP', udp: 'UDP' };
export const DIRECTION_LABEL: Record<InnerDirection, string> = { upload: '上行', download: '下行', bidir: '双向并发' };
export const FLOW_LABEL: Record<InnerFlow, string> = { up: '上行', down: '下行' };
export const MEASUREMENT_LABEL: Record<InnerMeasurement, string> = {
  nic_strict: '网卡计数严格',
  nic_preferred: '网卡优先，工具兜底',
  tool: '工具接收速率',
};
export const MEASUREMENT_HINT: Record<InnerMeasurement, string> = {
  nic_strict: '使用接收网卡速率判定；无法取得可信采样时标记为 NOT_EVALUATED。',
  nic_preferred: '优先使用接收网卡速率；无法取得可信采样时改用工具接收速率，并应用工具门限。',
  tool: '使用工具接收速率判定，需单独填写工具门限；网卡速率作为参考。',
};
export const SOURCE_LABEL: Record<string, string> = {
  nic: '网卡字节计数',
  tool: '工具接收汇总',
  none: '无可信来源',
};

export interface InnerLink {
  name: string;
  host: string;
  /** 本轮是否参与。取消勾选保留全部参数，只是不执行、不预检、不拉辅测机进门禁。 */
  enabled: boolean;
  local_interface: string;
  local_ip: string;
  local_ipv6: string | null;
  /** 板侧 LAN 地址：server bind 与 client 目标。 */
  gateway: string;
  gateway_ipv6: string | null;
  /** 板侧统计接口，与 LAN 地址归属解耦；留空按地址归属自动识别。 */
  board_rx_interface: string;
  measurement: InnerMeasurement;
  upload_min_mbps: number | null;
  download_min_mbps: number | null;
  /** 双向并发的网卡口径合计门限；留空则按逐方向门限判定，不折半推算。 */
  bidir_total_min_mbps: number | null;
  /** 工具口径门限，与网卡口径完全独立。 */
  tool_upload_min_mbps: number | null;
  tool_download_min_mbps: number | null;
  tool_bidir_total_min_mbps: number | null;
}

export interface InnerConfig {
  adb_path: string; serial: string; board_iperf: string; duration_secs: number;
  parallel: number; ip_versions: InnerIpVersion[]; protocols: InnerProtocol[]; directions: InnerDirection[];
  tcp_streams: number | null; udp_streams: number | null; tcp_window: string | null;
  udp_mbps: number | null; udp_length: string | null; max_udp_loss_pct: number | null;
  port: number; repeats: number; resume: boolean; agents: InnerAgent[]; links: InnerLink[];
}

export interface InnerBoardInterface {
  name: string; addresses: string[]; master: string; members: string[];
  ipv6_addresses?: string[];
  proc_counters: boolean; sysfs_counters: boolean;
}
export type InnerHostStatus = 'ready' | 'failed' | 'not_participating';
export interface InnerAgentCapability {
  id: string; status: InnerHostStatus; error: string | null; info: HostInfo | null;
}
export interface InnerCapability {
  serial: string; board_version: string; board_addresses: string; board_counters: string;
  board_interfaces: InnerBoardInterface[]; local: HostInfo; agents: InnerAgentCapability[];
  board_inventory_error?: string | null;
}

/** 一条腿的结果。网卡口径始终单独留一份，兜底用了工具也不会改写它。 */
export interface InnerLeg {
  flow: InnerFlow; port: number; receiver: string; receiver_host: string;
  counter_source: string | null;
  source: 'nic' | 'tool' | 'none';
  mbps: number | null; target_mbps: number | null; fallback_reason: string | null;
  verdict: string; reason: string; detail: string; diagnostics: string[];
  nic_rx_mbps: number | null; nic_verdict: string; nic_reason: string; nic_target_mbps: number | null;
  coverage: number; effective_secs: number; required_secs: number;
  tool_sender_mbps: number | null; tool_receiver_mbps: number | null; tool_receiver_note: string;
  udp_loss_pct: number | null; udp_lost_datagrams: number | null; udp_total_datagrams: number | null;
}

/** 一行 = 一个测试单元。双向的两条腿挂在同一行下面。 */
export interface InnerUnit {
  id?: string; index: number; link: string; host: string; protocol: InnerProtocol; direction: InnerDirection;
  ip_version?: InnerIpVersion;
  streams: number; repeat: number; measurement: InnerMeasurement;
  verdict: string; resumed?: boolean; reason: string; detail: string; diagnostics: string[];
  total_mbps: number | null; total_target_mbps: number | null; overlap_secs: number | null;
  legs: InnerLeg[];
}

export interface InnerStatus {
  run_id?: string;
  running: boolean; current: string; error: string | null; completed: number; total: number;
  units: InnerUnit[]; has_report: boolean;
}

/** `/api/inner/status` 的原始回包：`units` 是**增量**，从 `units_from` 开始。 */
export interface InnerStatusDelta extends Omit<InnerStatus, 'units'> {
  units: InnerUnit[];
  units_from: number;
}

/**
 * 把一份增量并进现有单元列表。
 *
 * `units_from === 0` 表示这是整份（新一轮，或页面首次拉取），直接替换；大于 0
 * 时截到该位置再接上——重连或漏了一拍时不会把中间的单元拼错位。
 */
export function mergeInnerUnits(current: InnerUnit[], delta: InnerStatusDelta): InnerUnit[] {
  if (delta.units_from === 0) return delta.units;
  return [...current.slice(0, delta.units_from), ...delta.units];
}

export interface InnerPreviewLeg {
  flow: InnerFlow; port: number; receiver: string;
  nic_target_mbps: number | null; tool_target_mbps: number | null;
}
export interface InnerPreviewRow {
  id?: string; index: number; link: string; host: string; protocol: InnerProtocol; direction: InnerDirection;
  ip_version?: InnerIpVersion;
  repeat: number; measurement: InnerMeasurement; legs: InnerPreviewLeg[]; verdict_basis: string; resumed?: boolean;
}
/** 计划预览由后端的 plan 模块产出——页面不再自己算一遍笛卡尔积。 */
export interface InnerPreview {
  links: number; units: number; legs: number; bidir_units: number; estimated_secs: number;
  uses_master: boolean; agents: string[]; skipped: string[]; rows: InnerPreviewRow[]; resumed?: number;
}

/** 一次历史运行的摘要，来自运行目录里的 summary.json。 */
export interface InnerRunEntry {
  id: string; created_at: string; probe_only: boolean; finished?: boolean | null;
  units: number; passed: number; rate_failed: number; not_evaluated: number;
  links: string[]; error: string | null;
  has_report: boolean; has_config: boolean; bytes: number;
}

export function defaultInnerConfig(): InnerConfig {
  return { adb_path: 'adb', serial: '', board_iperf: 'iperf3', duration_secs: 20,
    parallel: 1, ip_versions: [4], protocols: ['tcp'], directions: ['upload', 'download'],
    tcp_streams: null, udp_streams: null, tcp_window: null,
    udp_mbps: null, udp_length: null, max_udp_loss_pct: null,
    port: 56190, repeats: 1, resume: false, agents: [], links: [] };
}

/** 与后端 `size_token` 同一条白名单：`-w` / `-l` 原样进 iperf3 命令行。 */
export function innerSizeToken(value: string): boolean {
  return /^[0-9]{1,12}[kKmMgG]?$/.test(value) && Number.parseInt(value, 10) > 0;
}
/** 与后端 `config::safe_word` 同步：会进入 iperf3/ADB 参数，但不是可执行路径。 */
export function innerSafeWord(value: string): boolean {
  return /^[A-Za-z0-9_./:-]{1,256}$/.test(value) && !value.startsWith('-');
}
/** 与后端 `iface_word` 同一条白名单：板侧接口名要拼进 /sys/class/net/<接口>/…。 */
export function innerIfaceWord(value: string): boolean {
  return /^[A-Za-z0-9_.:-]{1,32}$/.test(value) && !value.startsWith('-') && value !== '.' && value !== '..';
}

/** 新建链路默认用「网卡优先，工具兜底」：新配置推荐这一档，迁移来的旧配置保持严格。 */
export function innerLink(host = 'master', nic?: NicInfo, name = 'ETH'): InnerLink {
  return { name, host, enabled: true, local_interface: nic?.name ?? '', local_ip: nic?.ipv4 ?? '',
    local_ipv6: nic ? innerNicIpv6(nic) || null : null, gateway_ipv6: null,
    gateway: INNER_DEFAULT_GATEWAY, board_rx_interface: INNER_DEFAULT_BOARD_RX, measurement: 'nic_preferred',
    upload_min_mbps: null, download_min_mbps: null, bidir_total_min_mbps: null,
    tool_upload_min_mbps: null, tool_download_min_mbps: null, tool_bidir_total_min_mbps: null };
}

function record(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === 'object' && !Array.isArray(value);
}
function onlyKeys(value: Record<string, unknown>, allowed: string[]): void {
  const extra = Object.keys(value).filter((k) => !allowed.includes(k));
  if (extra.length) throw new Error(`内环配置含未知字段：${extra.join('、')}`);
}
function number(value: unknown, min: number, max: number, integer = false): boolean {
  return typeof value === 'number' && Number.isFinite(value) && value >= min && value <= max && (!integer || Number.isInteger(value));
}

/**
 * v1 到 v2。只做重命名和补默认值，**不改变任何测试语义**。
 *
 * v1 的 `["upload","download"]` 是「先上行再下行」两个独立单向单元，迁移后
 * 仍然是两个单向单元。把它折成 `bidir` 会凭空造出一份从没测过的并发结论。
 */
function migrateV1(raw: Record<string, unknown>): Record<string, unknown> {
  const config = { ...raw };
  if ('protocol' in config) {
    if ('protocols' in config) throw new Error('配置同时含 protocol 与 protocols，无法判断该用哪一个');
    config.protocols = [config.protocol];
    delete config.protocol;
  }
  if (Array.isArray(config.links)) {
    config.links = config.links.map((item) => {
      if (!record(item)) return item;
      const link = { ...item };
      if ('board_interface' in link) {
        link.board_rx_interface ??= link.board_interface;
        delete link.board_interface;
      }
      // v1 没有勾选和策略：全部参与，并保持严格网卡口径——升级不能顺手
      // 把旧配置换成另一套验收规则。
      link.enabled ??= true;
      link.measurement ??= 'nic_strict';
      return link;
    });
  }
  return config;
}

/**
 * IPv4 单播地址，与后端 `Ipv4Addr` 解析加上那四条排除同步。
 *
 * 后端要的是能解析成 `Ipv4Addr` 且非 unspecified/loopback/multicast/broadcast
 * 的地址。前端不挡的话，`192.168.8.1OO`（字母 O）这种笔误照样存进草稿、链路显示
 * 得好好的，直到 `/api/inner/plan` 回一句 serde 的 `invalid IP address syntax`
 * ——32 条链路里是哪一条，界面上一个字都不说。
 */
function ipv4Octets(value: string): number[] | null {
  const parts = value.split('.');
  if (parts.length !== 4) return null;
  // 前导零：Rust 的 `Ipv4Addr` 解析器会拒（`01.2.3.4` 不合法），这里跟着拒，
  // 免得前端放行、后端再驳一次。
  if (parts.some((part) => !/^\d{1,3}$/.test(part) || (part.length > 1 && part.startsWith('0')))) return null;
  const octets = parts.map(Number);
  return octets.some((n) => n > 255) ? null : octets;
}
export function innerIpv4(value: string): boolean {
  const octets = ipv4Octets(value);
  if (!octets) return false;
  const [first] = octets;
  if (octets.every((n) => n === 0)) return false;      // 0.0.0.0
  if (first === 127) return false;                     // 环回
  if (first >= 224 && first <= 239) return false;      // 组播
  if (octets.every((n) => n === 255)) return false;    // 广播
  return true;
}

/** 只收裸 IPv6 单播地址；zone 由实际执行端按接口添加，不能把 PC 的 zone 发给板侧。 */
export function innerIpv6(value: string | null | undefined): boolean {
  return !!canonicalInnerIpv6(value);
}
export function canonicalInnerIpv6(value: string | null | undefined): string {
  if (!value || !/^[0-9a-fA-F:.]+$/.test(value) || !value.includes(':')) return '';
  try {
    const ip = new URL(`http://[${value}]/`).hostname.slice(1, -1);
    return ip === '::' || ip === '::1' || ip.startsWith('ff') || ip.startsWith('::ffff:') ? '' : ip;
  } catch { return ''; }
}
export function innerNicIpv6(nic: NicInfo): string {
  return [nic.ipv6_ll, nic.ipv6_global].find((ip) => innerIpv6(ip)) ?? '';
}
export function innerIpv6LinkLocal(value: string | null | undefined): boolean {
  const ip = canonicalInnerIpv6(value);
  return !!ip && (Number.parseInt(ip.split(':')[0], 16) & 0xffc0) === 0xfe80;
}

/** 与后端 `config::adb_program` 同步。导出是为了让共享语料能直接断言它。 */
export function isAdbProgram(value: string): boolean {
  const path = value.trim();
  if (!path || new TextEncoder().encode(path).length > 512 || path.startsWith('-')) return false;
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f-\u009f]/.test(path)) return false;
  return (path.split(/[/\\]/).pop() ?? path).toLowerCase().startsWith('adb');
}

function looksLikeV1(raw: Record<string, unknown>): boolean {
  return 'protocol' in raw
    || (Array.isArray(raw.links) && raw.links.some((link) => record(link) && 'board_interface' in link));
}

/**
 * 子网文件的两种形态：带 `project_version` 的项目文件，和控制台「下载
 * config.json」／手写的裸配置。只认前者的话，后者会掉进下面的 `onlyKeys`，
 * 报出来的是一串陌生字段名，而不是「这份文件该去子网导」。
 *
 * 这些键内环一个都没有，与后端 `SUBNET_ONLY_KEYS` 同一份清单。
 */
const SUBNET_ONLY_KEYS = [
  'agent_host', 'agent_port', 'require_same_subnet_for_iperf', 'ipv4_prefixes',
  'link_profiles', 'universal_params', 'ctstraffic', 'tests', 'pairs', 'iperf', 'ping',
] as const;

/** 独立格式；先验证再替换草稿，错误文件不产生部分导入。 */
export function parseInnerProject(text: string): InnerConfig {
  const file: unknown = JSON.parse(text);
  if (!record(file)) throw new Error('内环配置必须是 JSON 对象');
  if ('project_version' in file) throw new Error('这是子网项目，请在子网测试计划中导入');
  const subnetKey = SUBNET_ONLY_KEYS.find((key) => key in file);
  if (subnetKey) throw new Error(`这是子网配置（含 ${subnetKey}），请在子网测试计划中导入`);
  let raw = file;
  if ('kind' in file) {
    onlyKeys(file, ['kind', 'version', 'config']);
    if (file.kind !== INNER_KIND || !number(file.version, 0, Number.MAX_SAFE_INTEGER, true) || !record(file.config)) throw new Error(`仅支持 ${INNER_KIND} 项目`);
    if ((file.version as number) > INNER_VERSION) throw new Error(`内环项目版本 ${file.version} 高于本程序支持的 ${INNER_VERSION}，请升级程序后再导入`);
    raw = (file.version as number) < 2 ? migrateV1(file.config) : file.config;
  } else if (looksLikeV1(raw)) {
    raw = migrateV1(raw);
  }
  onlyKeys(raw, Object.keys(defaultInnerConfig()));
  const cfg = { ...defaultInnerConfig(), ...raw } as InnerConfig;
  if (typeof cfg.adb_path !== 'string' || !cfg.adb_path.trim() || typeof cfg.serial !== 'string' || typeof cfg.board_iperf !== 'string' || !cfg.board_iperf.trim()) throw new Error('ADB 路径、序列号和板侧工具必须是文本');
  // 与后端 `adb_program` 同一条规矩：这是唯一会被主控当程序执行的字段，
  // 管的是「指向什么」而不是「长什么样」——位置随意（Windows 的空格和反斜杠
  // 都要能用），但文件名必须以 adb 开头。前端不挡的话，界面存得下、后端一跑
  // 就报错，人得在两处之间来回猜。
  if (!isAdbProgram(cfg.adb_path)) throw new Error('ADB 路径的文件名必须以 adb 开头（位置随意，可含空格/反斜杠）');
  if (!innerSafeWord(cfg.board_iperf) || (cfg.serial !== '' && !innerSafeWord(cfg.serial))) throw new Error('序列号和板侧工具只能含字母、数字及 _./:-，且不能以 - 开头');
  // 双向单元的两条腿各占一个端口，所以端口上限留一格给 port + 1。
  if (!number(cfg.duration_secs, 6, 3600, true) || !number(cfg.parallel, 1, 16, true) || !number(cfg.port, 1024, 65534, true) || !number(cfg.repeats, 1, 10, true)) throw new Error('时长应为 6–3600 秒，并行流 1–16，端口 1024–65534（双向占用 port 与 port+1），重复轮次 1–10');
  const list = <T,>(values: unknown, allowed: readonly T[]): T[] => {
    if (!Array.isArray(values) || !values.length || values.length > allowed.length) throw new Error('IP 版本、协议和方向都至少选一项');
    if (new Set(values).size !== values.length || values.some((v) => !allowed.includes(v as T))) throw new Error('IP 版本、协议和方向不能重复或取未知值');
    return values as T[];
  };
  cfg.protocols = list(cfg.protocols, INNER_PROTOCOLS);
  cfg.ip_versions = list(cfg.ip_versions, INNER_IP_VERSIONS);
  cfg.directions = list(cfg.directions, INNER_DIRECTIONS);
  for (const streams of [cfg.tcp_streams, cfg.udp_streams]) {
    if (streams !== null && !number(streams, 1, 16, true)) throw new Error('按协议覆盖的并发流数应留空或为 1–16');
  }
  const udp = cfg.protocols.includes('udp');
  if (udp ? !number(cfg.udp_mbps, Number.MIN_VALUE, 1e6) : cfg.udp_mbps !== null) throw new Error('UDP 必须填写每条流的速率；不测 UDP 时不接受该项');
  for (const [label, value] of [['tcp_window', cfg.tcp_window], ['udp_length', cfg.udp_length]] as const) {
    if (value !== null && (typeof value !== 'string' || !innerSizeToken(value))) throw new Error(`${label} 只能是数字加可选的 k/m/g，例如 4m、1400`);
  }
  if (cfg.tcp_window !== null && !cfg.protocols.includes('tcp')) throw new Error('不测 TCP 时不接受 TCP 窗口');
  if ((cfg.udp_length !== null || cfg.max_udp_loss_pct !== null) && !udp) throw new Error('不测 UDP 时不接受 UDP 报文长度和丢包门槛');
  if (cfg.max_udp_loss_pct !== null && !number(cfg.max_udp_loss_pct, 0, 100)) throw new Error('UDP 丢包门槛应留空或为 0–100');
  if (!Array.isArray(cfg.agents) || cfg.agents.length > 8 || !Array.isArray(cfg.links) || cfg.links.length > 32) throw new Error('最多 8 台辅测机、32 条链路');
  const hosts = new Set(['master']);
  cfg.agents = cfg.agents.map((agent) => {
    if (!record(agent)) throw new Error('辅测机格式无效');
    onlyKeys(agent, ['id', 'address', 'port', 'token']);
    if (typeof agent.id !== 'string' || !innerSafeWord(agent.id) || hosts.has(agent.id)
      || typeof agent.address !== 'string' || !agent.address.trim()
      || new TextEncoder().encode(agent.address).length > MAX_AGENT_ADDRESS_BYTES
      || /[\u0000-\u001f\u007f-\u009f]/.test(agent.address)
      || !number(agent.port, 1, 65535, true)
      || (agent.token !== undefined && (typeof agent.token !== 'string'
        || new TextEncoder().encode(agent.token).length > MAX_AGENT_TOKEN_BYTES
        || /[\u0000-\u001f\u007f-\u009f]/.test(agent.token)))) {
      throw new Error('辅测机标识应唯一，且地址、端口、令牌格式必须有效');
    }
    hosts.add(agent.id);
    return { id: agent.id, address: agent.address.trim(), port: agent.port, token: agent.token ?? '' } as InnerAgent;
  });
  const bidir = cfg.directions.includes('bidir');
  const names = new Set<string>();
  const endpoints = new Set<string>();
  cfg.links = cfg.links.map((item) => {
    if (!record(item)) throw new Error('链路格式无效');
    onlyKeys(item, Object.keys(innerLink()));
    // 新建默认值不能改变旧文件的自动识别语义，也不能掩盖遗漏的目标地址。
    const legacyDefaults = innerLink();
    legacyDefaults.local_ip = '0.0.0.0';
    legacyDefaults.gateway = '0.0.0.0';
    legacyDefaults.board_rx_interface = '';
    // 后端 serde 对缺少 measurement 的 Link 默认为 nic_strict；新建表单的
    // 推荐值是 nic_preferred，但不能让导入一个省略该字段的旧/手写文件时
    // 前后端各自补出不同的验收口径。
    legacyDefaults.measurement = 'nic_strict';
    const link = { ...legacyDefaults, ...item } as InnerLink;
    if (![link.name, link.local_interface].every((v) => typeof v === 'string' && v.trim()) || typeof link.local_ip !== 'string' || typeof link.gateway !== 'string' || typeof link.board_rx_interface !== 'string' || !hosts.has(link.host) || names.has(link.name)) throw new Error('链路须有唯一名称、所属电脑、网卡、本地 IP 和板侧 LAN 地址');
    // 地址的形状也得在这一层挡住，理由和 adb_path 那条一样：界面存得下、后端
    // 一跑就报错，人得在两处之间来回猜是哪条链路写错了。
    if (!ipv4Octets(link.local_ip) || !ipv4Octets(link.gateway)) throw new Error(`${link.name}: IPv4 格式无效；不参与或仅测 IPv6 时可省略 IPv4 字段`);
    if (cfg.ip_versions.includes(4) && link.enabled) {
      if (!innerIpv4(link.local_ip) || !innerIpv4(link.gateway)) throw new Error(`${link.name}: 本机 IP 和板侧 LAN 地址都要是实际的 IPv4 单播地址`);
      if (link.local_ip === link.gateway) throw new Error(`${link.name}: 本机 IP 不能等于板侧地址`);
    }
    if ([link.local_ipv6, link.gateway_ipv6].some((value) => value !== null && (typeof value !== 'string' || !innerIpv6(value)))) throw new Error(`${link.name}: IPv6 应留空或填写单播地址，不带 %接口或 /前缀`);
    if (cfg.ip_versions.includes(6) && link.enabled) {
      if (!innerIpv6(link.local_ipv6) || !innerIpv6(link.gateway_ipv6)) throw new Error(`${link.name}: 请填写电脑和 CPE LAN 的 IPv6 单播地址`);
      if (canonicalInnerIpv6(link.local_ipv6) === canonicalInnerIpv6(link.gateway_ipv6)) throw new Error(`${link.name}: 电脑 IPv6 不能等于板侧地址`);
      if (innerIpv6LinkLocal(link.local_ipv6) !== innerIpv6LinkLocal(link.gateway_ipv6)) throw new Error(`${link.name}: 两端 IPv6 需同为链路本地地址或同为非链路本地地址`);
    }
    if (typeof link.enabled !== 'boolean' || !INNER_MEASUREMENTS.includes(link.measurement)) throw new Error('链路的参与状态和测量策略取值无效');
    if (link.board_rx_interface && !innerIfaceWord(link.board_rx_interface)) throw new Error(`${link.name}: 板侧统计接口只能含字母、数字及 _.:-`);
    for (const value of [link.upload_min_mbps, link.download_min_mbps, link.bidir_total_min_mbps,
      link.tool_upload_min_mbps, link.tool_download_min_mbps, link.tool_bidir_total_min_mbps]) {
      if (value !== null && !number(value, Number.MIN_VALUE, 1e6)) throw new Error('验收门限应留空或为正数 Mbps');
    }
    // 严格模式永远走不到工具口径，配了工具门限就是配了个永不生效的数。
    if (link.measurement === 'nic_strict' && [link.tool_upload_min_mbps, link.tool_download_min_mbps, link.tool_bidir_total_min_mbps].some((v) => v !== null)) throw new Error(`${link.name}: 网卡计数严格模式不会用到工具口径，因此不接受工具口径门限`);
    if (!bidir && [link.bidir_total_min_mbps, link.tool_bidir_total_min_mbps].some((v) => v !== null)) throw new Error(`${link.name}: 未勾选双向并发时不接受双向合计门限`);
    for (const version of cfg.ip_versions) {
      const endpoint = JSON.stringify([link.host, link.local_interface, version, version === 4 ? link.local_ip : canonicalInnerIpv6(link.local_ipv6)]);
      if (link.enabled && endpoints.has(endpoint)) throw new Error(`${link.name}: 与另一条参与本轮的链路使用了同一台电脑的同一网口和源 IP`);
      if (link.enabled) endpoints.add(endpoint);
    }
    names.add(link.name);
    return link;
  });
  return cfg;
}

/** 导出和持久化不携带辅测机令牌。发起 API 时使用内存中的完整配置。 */
export function serializeInnerProject(cfg: InnerConfig): string {
  const config = { ...cfg, agents: cfg.agents.map(({ id, address, port }) => ({ id, address, port })) };
  return JSON.stringify({ kind: INNER_KIND, version: INNER_VERSION, config }, null, 2);
}

/** 表单会留下已不适用的档位（取消勾选 UDP 后的速率、清空后的 '' 字符串、
 *  从工具兜底切回严格模式后的工具门限）。草稿保存和发起 API 都走这里，
 *  界面上删掉的档位不能偷偷跟着请求走。 */
export function normalizeInnerDraft(cfg: InnerConfig): InnerConfig {
  const blank = (value: unknown): boolean => value === null || value === undefined || String(value).trim() === '';
  const num = (value: number | null): number | null => (blank(value) ? null : value);
  const text = (value: string | null): string | null => (blank(value) ? null : String(value).trim());
  const tcp = cfg.protocols.includes('tcp');
  const udp = cfg.protocols.includes('udp');
  const bidir = cfg.directions.includes('bidir');
  return { ...cfg,
    tcp_streams: tcp ? num(cfg.tcp_streams) : null,
    udp_streams: udp ? num(cfg.udp_streams) : null,
    tcp_window: tcp ? text(cfg.tcp_window) : null,
    udp_length: udp ? text(cfg.udp_length) : null,
    max_udp_loss_pct: udp ? num(cfg.max_udp_loss_pct) : null,
    udp_mbps: udp ? num(cfg.udp_mbps) : null,
    agents: cfg.agents.map((agent) => ({ ...agent, address: agent.address.trim() })),
    links: cfg.links.map((link) => {
      const tool = link.measurement !== 'nic_strict';
      return { ...link,
        local_ip: link.local_ip.trim() || (cfg.ip_versions.includes(4) && link.enabled ? '' : '0.0.0.0'),
        gateway: link.gateway.trim() || (cfg.ip_versions.includes(4) && link.enabled ? '' : '0.0.0.0'),
        local_ipv6: text(link.local_ipv6), gateway_ipv6: text(link.gateway_ipv6),
        board_rx_interface: (link.board_rx_interface ?? '').trim(),
        upload_min_mbps: num(link.upload_min_mbps),
        download_min_mbps: num(link.download_min_mbps),
        bidir_total_min_mbps: bidir ? num(link.bidir_total_min_mbps) : null,
        tool_upload_min_mbps: tool ? num(link.tool_upload_min_mbps) : null,
        tool_download_min_mbps: tool ? num(link.tool_download_min_mbps) : null,
        tool_bidir_total_min_mbps: tool && bidir ? num(link.tool_bidir_total_min_mbps) : null };
    }) };
}

/** 把秒数说成人话，预览和执行区共用。 */
export function innerDuration(secs: number): string {
  if (secs < 60) return `${secs} 秒`;
  const minutes = Math.round(secs / 60);
  return minutes < 60 ? `${minutes} 分钟` : `${Math.floor(minutes / 60)} 小时 ${minutes % 60} 分`;
}

/** 缺少错误不代表执行完成；崩溃留下的中间摘要仍未收尾。 */
export function innerHistoryStatus(run: Pick<InnerRunEntry, 'finished' | 'probe_only'>): string {
  if (run.finished === false) return '未收尾';
  if (run.finished !== true) return '旧记录，收尾状态未知';
  return run.probe_only ? '仅探测' : '已完成';
}
