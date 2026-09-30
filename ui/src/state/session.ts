import { reactive } from 'vue';
import { api, UnauthorizedError, errorMessage, hasToken } from '../api/client';
import { clockStamp } from '../domain/freshness';
import type { BootstrapOut, ConnectOut, ConnectReq, LocalOut } from '../api/dto';

/**
 * 会话资源：口令状态、辅测机连接、本机与对端的拓扑入口。
 *
 * 按**服务端资源**切分，不按屏幕切：本机网卡表同时被「本机」「测试计划」两个
 * 视图读，按屏幕切会让同一份数据有两个所有者。
 */

export type SessionPhase =
  /** 还没试过连 */
  | 'idle'
  /** 正在连 */
  | 'connecting'
  /** 连上了 */
  | 'connected'
  /** 连失败（错误在 `error` 里） */
  | 'failed'
  /**
   * 口令失效——**独立终态**，不和普通错误混。
   *
   * 旧页把 401 混进通用 toast，看到的人只会以为是网络抖动然后一直刷新；
   * 它真正需要的动作是「用带 ?token= 的完整地址重新打开」。
   */
  | 'unauthorized';

/** 表单里端口那一格的出厂值；和 `reset()` 用同一个常量，别写两遍。 */
const DEFAULT_AGENT_PORT = 28801;
let generation = 0;
let connecting: Promise<void> | undefined;

export const session = reactive({
  phase: 'idle' as SessionPhase,
  error: '',
  /** `/api/bootstrap` 的回填值；未加载时为 null */
  bootstrap: null as BootstrapOut | null,
  /** 本机信息。**不需要连上辅测机**就能拿到 */
  local: null as LocalOut | null,
  /**
   * 连上之后的双端拓扑。
   *
   * **失败不清它。** 清掉的话，改错一个地址、点一次连接，两张网卡表就空了，
   * 屏幕上「没扫到网卡」和「刚才那次请求失败了」长得一模一样。留着上一份并
   * 标成旧快照，人才知道自己看的是什么时候的数据。
   */
  connection: null as ConnectOut | null,
  /**
   * 手上这份拓扑是不是**上一次成功**留下的旧快照。
   *
   * 它和 `phase === 'failed'` 不是一回事：phase 说的是最近一次请求怎么了，
   * 这一位说的是屏幕上那两张表是什么时候的。
   */
  topologyStale: false,
  /**
   * 拓扑真正来自哪一台机器、什么时候来的。
   *
   * 和下面的表单字段**分开存**：地址栏是草稿，改一个字符就变；而顶栏那句
   * 「已连 …」说的是事实。合成一个字段时，光在输入框里敲一个新地址，顶栏
   * 就立刻宣称已经连上了那台新机器。
   */
  connectedHost: '',
  connectedPort: 0,
  connectedAt: null as number | null,
  /** 表单字段：辅测机地址 / 端口 / 共享令牌 / 网卡前缀过滤 */
  host: '',
  port: DEFAULT_AGENT_PORT,
  token: '',
  prefixes: [] as string[],
  /**
   * 「重新扫描」的可见反馈。
   *
   * 这一栏不是装饰：Windows 上 `scan_host()` 要拉起 ipconfig / netsh，一两秒里
   * 页面纹丝不动，而**扫完通常和扫之前长得一模一样**——没有反馈的话，
   * 「成功」和「按钮没反应」在屏幕上是同一个样子。agent 状态页的
   * 「重新扫描」早就是这么做的，这里照搬同一套。
   */
  scanning: false,
  scanMessage: '',
  scanKind: '' as '' | 'ok' | 'bad',
  /**
   * `/api/bootstrap` 与 `/api/local` 各自的失败，**不共用一格**。
   *
   * 它们是两个独立请求：本机网卡不依赖辅测机。共用一格时，后落地的那个成功
   * 会把先落地的那个错误擦掉——屏幕上一切正常，而其中一半的数据根本没拿到。
   */
  bootstrapError: '',
  localError: '',
  /** 本机信息最近一次成功读到的时刻；没读到过就是 null（显示「尚未同步」）。 */
  localAt: null as number | null,
  /**
   * 上一次 bootstrap **回填进表单的那份值**。
   *
   * 存在的理由是「晚到回填不能盖掉用户已经敲进去的东西」。`/api/bootstrap` 在
   * Windows 上要拉起 ipconfig / netsh，一两秒才回来；这段时间里用户完全可能
   * 已经把地址改成了另一台机器。直接赋值的话，他敲的字会在某个说不清的时刻
   * 被悄悄换掉——而且**只在慢的机器上出现**，快的机器上永远复现不了。
   *
   * 判据是「这一格还是不是我上次写进去的那个值」：是 → 用户没动过，可以回填；
   * 不是 → 那是用户的输入，不碰。比另设一个 `touched` 标志可靠，因为不需要
   * 每个输入框都记得去置位。
   */
  filled: { host: '', port: 0 },
});

export function reset(): void {
  generation += 1;
  connecting = undefined;
  session.phase = 'idle';
  session.error = '';
  session.bootstrap = null;
  session.bootstrapError = '';
  session.local = null;
  session.localError = '';
  session.localAt = null;
  session.connection = null;
  session.topologyStale = false;
  session.connectedHost = '';
  session.connectedPort = 0;
  session.connectedAt = null;
  session.filled = { host: '', port: 0 };
  session.host = '';
  session.port = DEFAULT_AGENT_PORT;
  session.token = '';
  session.prefixes = [];
  session.scanning = false;
  session.scanMessage = '';
  session.scanKind = '';
}

/** 控制台是否需要口令（只监听回环时服务端可以不设）。 */
export function tokenReady(): boolean {
  return hasToken() || session.bootstrap?.token_configured === false;
}

function fail(error: unknown): void {
  if (error instanceof UnauthorizedError) {
    session.phase = 'unauthorized';
    session.error = '';
    return;
  }
  session.phase = 'failed';
  session.error = errorMessage(error);
}

/**
 * 打开页面时的第一批请求。
 *
 * `bootstrap` 与 `local` 是**独立**的：本机网卡不依赖辅测机，所以即使还没连上
 * 对端，「本机」那一页也该是有内容的。两个请求并发发出，任一失败不拖垮另一个。
 */
export async function load(): Promise<void> {
  const epoch = generation;
  // 口令的落地**不在这里**：它以前是本函数的副作用，于是任何排在 `load()`
  // 之前的开场请求都会赶在口令之前出门（`App.vue` 的 `syncStatus()` 就这么撞过
  // 一次 401）。现在归 `api/client.ts::adoptToken()`，由 `main.ts` 和每次读
  // 口令时各兜一道，见那里的说明。
  const [bootstrap, local] = await Promise.allSettled([
    api.get<BootstrapOut>('/api/bootstrap'),
    api.get<LocalOut>('/api/local'),
  ]);
  if (epoch !== generation) return;
  if (bootstrap.status === 'fulfilled') {
    session.bootstrap = bootstrap.value;
    session.bootstrapError = '';
    // 只回填**用户没动过**的格子，判据见 `session.filled`。
    if (session.host === session.filled.host) session.host = bootstrap.value.agent_host;
    if (session.port === session.filled.port || session.port === DEFAULT_AGENT_PORT) {
      session.port = bootstrap.value.agent_port;
    }
    if (session.prefixes.length === 0) session.prefixes = [...bootstrap.value.ipv4_prefixes];
    session.filled = { host: session.host, port: session.port };
  } else if (bootstrap.reason instanceof UnauthorizedError) {
    session.phase = 'unauthorized';
  } else {
    session.bootstrapError = errorMessage(bootstrap.reason);
  }
  if (local.status === 'fulfilled') {
    session.local = local.value;
    session.localError = '';
    session.localAt = Date.now();
  } else if (local.reason instanceof UnauthorizedError) {
    session.phase = 'unauthorized';
  } else {
    session.localError = errorMessage(local.reason);
  }
}

/**
 * 重新扫描网卡。**「本机」和「辅测机」两页共用同一个实现**。
 *
 * 网卡是会变的：插拔网线、开关 Wi-Fi、装驱动、改 IP——控制台却没有重扫入口，
 * 只能整页刷新（而刷新还要重新走一遍连接）。agent 的状态页一直有「重新扫描」，
 * 主控这边反而没有。
 *
 * 两页共用一个实现，是因为两张表本来就来自**同一次扫描**：连上之后
 * `masterNics` 读的是 `/api/connect` 回包里的 `master`（按 IPv4 前缀过滤过的
 * 那一份），不是 `/api/local`。所以「本机页只重扫本机」做不到——那样按下去
 * 表格不会变，看起来又是按钮没反应。
 *
 * - 还没连上：只能扫本机（`/api/local`，有意不按前缀过滤）。
 * - 已连上：两端一起重扫（`/api/connect`），沿用当前的地址、令牌和前缀。
 */
export async function rescan(): Promise<void> {
  if (session.scanning || connecting) return;
  const epoch = generation;
  session.scanning = true;
  session.scanMessage = '正在重新扫描网卡…';
  session.scanKind = '';
  try {
    // 失败时保留的 connection 仍是表格来源；重试必须同时刷新它，不能只更新
    // 被旧 connection 遮住的 local，再把未变化的表格报成「扫描成功」。
    const connected = session.connection !== null;
    // 本机那一份总要刷：它是「还没连上」时唯一的来源，也是 iperf3 与版本号的来源。
    const local = await api.get<LocalOut>('/api/local');
    if (epoch !== generation) return;
    session.local = local;
    session.localError = '';
    session.localAt = Date.now();
    if (connected) {
      await connect();
      if (epoch !== generation) return;
      if (session.phase !== 'connected') {
        // 重扫失败保留上一次成功的两张表（`connect()` 不清 connection），
        // 只把它标成旧的。**不能**顺手把拓扑抹成空——那会让「分配链路」
        // 那一页对着一份空拓扑去 reconcile，把用户的分配意图一起清掉。
        session.scanMessage = `重新扫描失败：${session.error || '连接对端失败'}；下面仍是上次成功的网卡`;
        session.scanKind = 'bad';
        return;
      }
      const master = session.connection?.master.interfaces.length ?? 0;
      const agent = session.connection?.agent.interfaces.length ?? 0;
      session.scanMessage = `已重新扫描 · 本机 ${master} 块 / 辅测 ${agent} 块 · ${stamp()}`;
    } else {
      session.scanMessage = `已重新扫描本机 · ${local.host.interfaces.length} 块网卡 · ${stamp()}`;
    }
    session.scanKind = 'ok';
  } catch (error) {
    if (epoch !== generation) return;
    session.topologyStale = session.connection !== null;
    fail(error);
    if (error instanceof UnauthorizedError) {
      session.scanMessage = '';
      session.scanKind = '';
      return;
    }
    session.scanMessage = `重新扫描失败：${errorMessage(error)}${session.local || session.connection ? '；下面仍是上次成功的网卡' : ''}`;
    session.scanKind = 'bad';
  } finally {
    if (epoch === generation) session.scanning = false;
  }
}

/** 扫描完成的时刻，与其它三处「这份是什么时候的」共用 `domain/freshness`。 */
function stamp(): string {
  return clockStamp(Date.now());
}

export function connect(): Promise<void> {
  if (connecting) return connecting;
  const pending = connectOnce(generation).finally(() => {
    if (connecting === pending) connecting = undefined;
  });
  connecting = pending;
  return pending;
}

async function connectOnce(epoch: number): Promise<void> {
  session.phase = 'connecting';
  session.error = '';
  try {
    // 字段名以 Rust 侧的 `ConnectReq` 为准（`webui/api.rs`）：serde 没开
    // `deny_unknown_fields`，名字对不上不会报错，只会被静默丢掉。
    const request: ConnectReq = {
      host: session.host.trim(),
      port: session.port,
      token: session.token,
      ipv4_prefixes: [...session.prefixes],
    };
    const connection = await api.post<ConnectOut>('/api/connect', request);
    if (epoch !== generation) return;
    session.connection = connection;
    // 身份只在**成功之后**落地：在这之前顶栏说的还是上一台机器，那是事实。
    session.connectedHost = request.host;
    session.connectedPort = request.port;
    session.connectedAt = Date.now();
    session.topologyStale = false;
    session.phase = 'connected';
  } catch (error) {
    if (epoch !== generation) return;
    // 保留上一份拓扑并标旧，不清空（理由见 `session.connection` 的注释）。
    session.topologyStale = session.connection !== null;
    fail(error);
  }
}
