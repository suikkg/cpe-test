import { reactive } from 'vue';
import {
  api,
  UnauthorizedError,
  adoptTokenFromCookie,
  adoptTokenFromUrl,
  hasToken,
} from '../api/client';
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

export const session = reactive({
  phase: 'idle' as SessionPhase,
  error: '',
  /** `/api/bootstrap` 的回填值；未加载时为 null */
  bootstrap: null as BootstrapOut | null,
  /** 本机信息。**不需要连上辅测机**就能拿到 */
  local: null as LocalOut | null,
  /** 连上之后的双端拓扑 */
  connection: null as ConnectOut | null,
  /** 表单字段：辅测机地址 / 端口 / 共享令牌 / 网卡前缀过滤 */
  host: '',
  port: 28801,
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
});

export function reset(): void {
  session.phase = 'idle';
  session.error = '';
  session.bootstrap = null;
  session.local = null;
  session.connection = null;
  session.host = '';
  session.port = 28801;
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
  session.error = error instanceof Error ? error.message : String(error);
}

/**
 * 打开页面时的第一批请求。
 *
 * `bootstrap` 与 `local` 是**独立**的：本机网卡不依赖辅测机，所以即使还没连上
 * 对端，「本机」那一页也该是有内容的。两个请求并发发出，任一失败不拖垮另一个。
 */
export async function load(): Promise<void> {
  adoptTokenFromUrl();
  // 地址栏没带口令时，退到服务端交付页面时下发的会话 cookie。刷新与「复制地址
  // 到新标签打开」都走这一条——两者的 `GET /` 都是靠 cookie 认过的。
  adoptTokenFromCookie();
  const [bootstrap, local] = await Promise.allSettled([
    api.get<BootstrapOut>('/api/bootstrap'),
    api.get<LocalOut>('/api/local'),
  ]);
  if (bootstrap.status === 'fulfilled') {
    session.bootstrap = bootstrap.value;
    session.host = bootstrap.value.agent_host;
    session.port = bootstrap.value.agent_port;
    session.prefixes = [...bootstrap.value.ipv4_prefixes];
  } else {
    fail(bootstrap.reason);
  }
  if (local.status === 'fulfilled') {
    session.local = local.value;
  } else if (session.phase !== 'unauthorized') {
    fail(local.reason);
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
  if (session.scanning) return;
  session.scanning = true;
  session.scanMessage = '正在重新扫描网卡…';
  session.scanKind = '';
  try {
    const connected = session.phase === 'connected';
    // 本机那一份总要刷：它是「还没连上」时唯一的来源，也是 iperf3 与版本号的来源。
    const local = await api.get<LocalOut>('/api/local');
    session.local = local;
    if (connected) {
      await connect();
      if (session.phase !== 'connected') {
        session.scanMessage = `重新扫描失败：${session.error || '连接对端失败'}`;
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
    if (error instanceof UnauthorizedError) {
      session.phase = 'unauthorized';
      session.scanMessage = '';
      session.scanKind = '';
      return;
    }
    session.scanMessage = `重新扫描失败：${error instanceof Error ? error.message : String(error)}`;
    session.scanKind = 'bad';
  } finally {
    session.scanning = false;
  }
}

/** 扫描完成的时刻。给的是**本地时钟**的时分秒，只用来回答「这份表是刚才的吗」。 */
function stamp(): string {
  const now = new Date();
  return [now.getHours(), now.getMinutes(), now.getSeconds()]
    .map((part) => String(part).padStart(2, '0'))
    .join(':');
}

export async function connect(): Promise<void> {
  session.phase = 'connecting';
  session.error = '';
  try {
    // 字段名以 Rust 侧的 `ConnectReq` 为准（`webui/api.rs`）：serde 没开
    // `deny_unknown_fields`，名字对不上不会报错，只会被静默丢掉。
    const request: ConnectReq = {
      host: session.host.trim(),
      port: session.port,
      token: session.token,
      ipv4_prefixes: session.prefixes,
    };
    session.connection = await api.post<ConnectOut>('/api/connect', request);
    session.phase = 'connected';
  } catch (error) {
    session.connection = null;
    fail(error);
  }
}
