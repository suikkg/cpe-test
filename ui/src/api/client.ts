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
  error?: unknown;
  data?: T;
}

/** 把接口/运行时错误转成用户能读懂的文本，避免直接显示 `[object Object]`。 */
export function errorMessage(value: unknown): string {
  if (value instanceof Error) return value.message;
  if (typeof value === 'string') return value;
  if (value == null) return '';
  try {
    const json = JSON.stringify(value);
    if (json && json !== '{}') return json;
  } catch {
    // 某些异常对象不可序列化，退到下面的安全文本。
  }
  return String(value) === '[object Object]' ? '服务端返回了无法识别的错误' : String(value);
}

/** 口令失效。调用方要走专门的终态，不要混进通用错误提示。 */
export class UnauthorizedError extends Error {
  constructor() {
    super('口令无效或已失效');
    this.name = 'UnauthorizedError';
  }
}

/**
 * 请求**没能拿到应答**：网络断了、主控进程没了、连接被中途掐断。
 *
 * 和「服务端答了但说失败」必须分开，因为这两种的下一步完全不同：服务端答了
 * `ok:false`，那这条命令**确定没执行**；而连应答都没有时，它可能已经在对面
 * 跑起来了。开始/停止这类有副作用的命令只能按后者处理——去读一次运行状态，
 * 而不是再发一遍（这套 client 一贯不自动重试，理由见文件末尾）。
 */
export class NetworkError extends Error {
  /** 原始的 fetch 拒因，只进诊断，不直接显示——它常常是一句 `Failed to fetch`。 */
  readonly reason: unknown;
  constructor(reason: unknown) {
    super('请求没有得到应答：网络中断，或主控没有响应');
    this.name = 'NetworkError';
    this.reason = reason;
  }
}

const TOKEN_KEY = 'cpe_ui_token';
/** 服务端交付页面时下发的会话 cookie；名字与 `webui/http.rs::SESSION_COOKIE` 同源。 */
const SESSION_COOKIE = 'cpe_ui_session';

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
function adoptTokenFromUrl(): void {
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

/**
 * 从服务端下发的会话 cookie 里补一份口令。
 *
 * sessionStorage 是**按标签页**的：把控制台地址复制到新标签打开时那里是空的，
 * 而页面本身能打开（服务端认那枚 cookie）。没有这一步，新标签的表现是
 * 「界面出来了、每一个 API 都 401」——比刷新报错更难懂。
 *
 * 只在 sessionStorage 里没有时才用它：地址栏刚给过的口令是**更新**的那一份，
 * 换口令重启主控之后，旧 cookie 还可能挂在浏览器上。
 */
function adoptTokenFromCookie(): void {
  try {
    if (sessionStorage.getItem(TOKEN_KEY)) return;
    const found = document.cookie
      .split(';')
      .map((pair) => pair.trim())
      .find((pair) => pair.startsWith(`${SESSION_COOKIE}=`));
    if (!found) return;
    const value = decodeURIComponent(found.slice(SESSION_COOKIE.length + 1));
    if (value) sessionStorage.setItem(TOKEN_KEY, value);
  } catch {
    // cookie 被禁 / 隐私模式：退回「地址栏必须带 ?token=」那条路，是可见的失败。
  }
}

/**
 * 把口令从「这次导航带来的东西」搬进 sessionStorage，**在发出任何请求之前**。
 *
 * 地址栏优先于 cookie：地址栏刚给过的那一份是更新的，换口令重启主控之后
 * 浏览器上还可能挂着旧 cookie。
 *
 * # 为什么它必须是幂等的、而且由 `token()` 兜底
 *
 * 这一步以前埋在 `state/session.ts::load()` 里，也就是「开场那批请求」之一的
 * **副作用**。于是它对调用顺序敏感：`App.vue` 的 `onMounted` 后来在 `load()`
 * 之前插了一句 `syncStatus()`（先认一次「服务器上是不是已经有一轮在跑」），
 * 那一发 `/api/progress` 就赶在口令落地之前出门，必然 401，界面直接进
 * 「口令失效」这个全局终态——而随后成功的 `load()` 并不会把它翻回来。
 * 表现正是「刚打开是无口令，过几秒刷新（sessionStorage 里已经有了）就好」。
 *
 * 靠「记得排在第一个」防不住这类错误：任何人再加一个开场请求就会重犯。所以
 * 落点改成两处——`main.ts` 在挂载前显式调一次（地址栏该**立刻**抹掉，不能等到
 * 第一个请求才抹），`token()` 每次读之前再兜一次，于是**没有任何一条请求路径
 * 能跑在口令前面**。
 *
 * 不设「已经认过」的门闩：这两步本来就自带幂等（`?token=` 认完就被抹掉，
 * cookie 那步在 sessionStorage 已有值时直接返回），一个门闩换不来什么，
 * 却要为单测再开一个只在测试里用的复位出口。
 */
export function adoptToken(): void {
  adoptTokenFromUrl();
  adoptTokenFromCookie();
}

function token(): string {
  adoptToken();
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

/**
 * 单次请求的上限。**没有它，一个挂死的请求会让轮询链永久停摆。**
 *
 * `fetch` 自己不带超时：连接断在半路（被测链路正被灌到线速、辅测机掉线）时，
 * 它可能几分钟不 reject 也不 resolve。而 `run.ts` 的 `inFlight` 闸门在此期间
 * 会把后续每一拍都挡掉——屏幕上不报错，只是数据一直是上一拍的，「上次同步」
 * 停在几分钟前。11.5 小时的长测试里网络抖一次就够了。
 *
 * 默认放到 120 秒是照着**最慢的合法端点**定的：`/api/local` 在 Windows 上要
 * 真去扫网卡（`ipconfig /all` 20s + 每块 Wi-Fi 卡 `netsh` 10s + `iperf3
 * --version` 8s），这条路正常就能跑到几十秒。轮询那条链自己传更短的值。
 */
const DEFAULT_TIMEOUT_MS = 120_000;

export interface RequestOptions {
  /** 覆盖默认超时；轮询用更短的值，慢扫描端点用默认值。 */
  timeoutMs?: number;
}

async function request<T>(
  method: 'GET' | 'POST',
  path: string,
  body?: unknown,
  options?: RequestOptions,
): Promise<T> {
  const headers: Record<string, string> = { 'X-CPE-Token': token() };
  if (method === 'POST') {
    headers['Content-Type'] = 'application/json';
    // 自定义头是这套鉴权的 CSRF 门：浏览器不会给跨站表单请求带它，
    // 所以服务端只要求 POST 带（`webui/http.rs`）。漏了它的表现是请求被拒，
    // 而错误信息看起来完全像是口令不对。
    headers['X-CPE-Console'] = '1';
  }

  // `AbortSignal.timeout` 在测试环境和老 WebView 里未必有，退回手搓的
  // controller；两条路的可观察行为一样——超时即 reject，落到 NetworkError。
  const timeoutMs = options?.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const controller = typeof AbortController === 'function' ? new AbortController() : undefined;
  const timer = controller
    ? setTimeout(() => controller.abort(new Error(`请求超过 ${timeoutMs}ms 未返回`)), timeoutMs)
    : undefined;
  try {
    const response = await fetch(path, {
      method,
      headers,
      body: method === 'POST' ? JSON.stringify(body ?? {}) : undefined,
      // 内网工具，不需要也不该带 cookie。
      credentials: 'omit',
      cache: 'no-store',
      signal: controller?.signal,
    }).catch((reason: unknown) => { throw new NetworkError(reason); });

    if (response.status === 401) throw new UnauthorizedError();
    if (!response.ok) throw new Error(`HTTP ${response.status}`);

    // 收到响应头不代表拿到了命令结果；正文中断、超时或无法解码都应按结果未知处理。
    const payload = await response.json().catch((reason: unknown) => {
      throw new NetworkError(reason);
    }) as Resp<T>;
    if (!payload || typeof payload.ok !== 'boolean') {
      throw new NetworkError(new Error('服务端响应格式无效'));
    }
    if (!payload.ok) {
      throw new Error(errorMessage(payload.error) || '服务端返回了失败但没有说明原因');
    }
    return payload.data as T;
  } finally {
    // 包括读取正文在内的整个请求结束后才撤销超时。
    if (timer !== undefined) clearTimeout(timer);
  }
}

/**
 * **不做自动重试。**
 *
 * 这是内网工具，一次请求失败要么是口令不对、要么是主控没起来、要么是被测
 * 机器真的忙不过来——三种都需要人看见。自动重试只会把它们变成「界面偶尔卡
 * 一下」，然后在真出问题的时候多花十分钟才被发现。
 */
export const api = {
  get: <T>(path: string, options?: RequestOptions) => request<T>('GET', path, undefined, options),
  post: <T>(path: string, body?: unknown, options?: RequestOptions) =>
    request<T>('POST', path, body, options),
};
