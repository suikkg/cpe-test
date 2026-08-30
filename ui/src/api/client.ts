/**
 * 控制台**唯一**的 fetch 出口。
 *
 * 全仓不许有第二处 `fetch(`——由 `scripts/lint-arch.mjs` 的分层规则挡着
 * （`components/**` 与 `domain/**` 都不许 import 本文件）。理由不是洁癖：
 * 口令怎么带、CSRF 头怎么加、401 怎么处理，这三件事每多一份实现就多一处
 * 会漏的地方，而漏掉的表现分别是「请求 401」「请求被当跨站拒掉」「口令过期
 * 却显示成网络抖动」——三种都会让人以为是网络问题去查网络。
 */

/** 服务端统一响应包装（Rust: `protocol.rs::Resp`）。 */
interface Resp<T> {
  ok: boolean;
  error?: string;
  data?: T;
}

/** 口令失效。调用方要走专门的终态，不要混进通用错误提示。 */
export class UnauthorizedError extends Error {
  constructor() {
    super('口令无效或已失效');
    this.name = 'UnauthorizedError';
  }
}

const TOKEN_KEY = 'cpe_ui_token';

/**
 * 从 URL 取出口令、存进 sessionStorage，然后**把 query 从地址栏抹掉**。
 *
 * 这是安全行为，不是整洁：地址栏里的 `?token=` 会进浏览器历史、会被截图带走、
 * 会在用户复制链接发给同事时一起发出去。旧页面就是这么做的
 * （`b3013e6:src/master/webui.html` 的 726-731 行），照搬，不要「优化」。
 *
 * 用 sessionStorage 而不是 localStorage：关掉标签页就没了，符合「一次会话」的
 * 预期；控制台口令不该在这台机器上长期留存。
 */
export function adoptTokenFromUrl(): void {
  try {
    const url = new URL(window.location.href);
    const token = url.searchParams.get('token');
    if (token) {
      sessionStorage.setItem(TOKEN_KEY, token);
      url.searchParams.delete('token');
      window.history.replaceState(null, '', url.pathname + url.search + url.hash);
    }
  } catch {
    // 隐私模式下 sessionStorage 会抛。抹地址栏这一步已经尽力了，
    // 请求仍会带上内存里的空口令并得到 401——那是正确的可见失败。
  }
}

function token(): string {
  try {
    return sessionStorage.getItem(TOKEN_KEY) ?? '';
  } catch {
    return '';
  }
}

/** 只监听回环时服务端可以不设口令，这时 token 为空是正常的。 */
export function hasToken(): boolean {
  return token() !== '';
}

/**
 * 给**浏览器自己发起的下载**拼查询串（`?token=…`，没有口令时为空串）。
 *
 * 浏览器下载不带自定义头，`<a download>` 的相对 URL 也不继承 `fetch` 那套，
 * 所以这是唯一允许把口令放进 URL 的通道；代价见 `views/runs/RunsView.vue`。
 *
 * 存在的理由是「口令怎么取只有一处实现」：调用方以前自己 `sessionStorage
 * .getItem('cpe_ui_token')`，于是 `TOKEN_KEY` 有了第二份硬编码——改了这里那份
 * 而漏掉调用方，表现是下载链接静默 401，看起来完全像是服务端的问题。
 */
export function downloadQuery(): string {
  const value = token();
  return value ? `?token=${encodeURIComponent(value)}` : '';
}

async function request<T>(method: 'GET' | 'POST', path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { 'X-CPE-Token': token() };
  if (method === 'POST') {
    headers['Content-Type'] = 'application/json';
    // 自定义头是这套鉴权的 CSRF 门：浏览器不会给跨站表单请求带它，
    // 所以服务端只要求 POST 带（`webui/http.rs`）。漏了它的表现是请求被拒，
    // 而错误信息看起来完全像是口令不对。
    headers['X-CPE-Console'] = '1';
  }

  const response = await fetch(path, {
    method,
    headers,
    body: method === 'POST' ? JSON.stringify(body ?? {}) : undefined,
    // 内网工具，不需要也不该带 cookie。
    credentials: 'omit',
    cache: 'no-store',
  });

  // 401 单独成一类：旧页面把它混进通用 toast，看到的人只会以为是网络抖动，
  // 然后一直刷新。它需要的是「用带 ?token= 的完整地址重新打开」。
  if (response.status === 401) {
    throw new UnauthorizedError();
  }
  if (!response.ok) {
    throw new Error(`HTTP ${response.status}`);
  }

  const payload = (await response.json()) as Resp<T>;
  if (!payload.ok) {
    throw new Error(payload.error || '服务端返回了失败但没有说明原因');
  }
  return payload.data as T;
}

/**
 * **不做自动重试。**
 *
 * 这是内网工具，一次请求失败要么是口令不对、要么是主控没起来、要么是被测
 * 机器真的忙不过来——三种都需要人看见。自动重试只会把它们变成「界面偶尔卡
 * 一下」，然后在真出问题的时候多花十分钟才被发现。
 */
export const api = {
  get: <T>(path: string) => request<T>('GET', path),
  post: <T>(path: string, body?: unknown) => request<T>('POST', path, body),
};
