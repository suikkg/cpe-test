import { reactive } from 'vue';
import { api, UnauthorizedError, adoptTokenFromUrl, hasToken } from '../api/client';
import type { BootstrapOut, ConnectOut, LocalOut } from '../api/dto';

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

export async function connect(): Promise<void> {
  session.phase = 'connecting';
  session.error = '';
  try {
    session.connection = await api.post<ConnectOut>('/api/connect', {
      host: session.host.trim(),
      port: session.port,
      token: session.token,
      prefixes: session.prefixes,
    });
    session.phase = 'connected';
  } catch (error) {
    session.connection = null;
    fail(error);
  }
}
