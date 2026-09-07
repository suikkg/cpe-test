import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { api, downloadQuery } from './client';

/**
 * 口令怎么落地，**与调用顺序无关**。
 *
 * 钉的是一个真实回归：`App.vue` 的 `onMounted` 在 `load()` 之前插了一句
 * `syncStatus()`，而口令的落地当时埋在 `load()` 里。于是页面刚打开时最先出门的
 * 那一发 `/api/progress` 不带口令，401，界面进「口令失效」全局终态；随后成功的
 * `load()` 并不会把它翻回来。用户看到的是「刚打开显示没口令，等几秒刷新就好」
 * ——刷新之所以好，只是因为 sessionStorage 里已经被上一次访问写进去了。
 *
 * 所以这里**不调用任何 state 模块**：直接发一个请求，断言它自己就带上了口令。
 */

class MemoryStorage {
  private map = new Map<string, string>();
  getItem(key: string): string | null {
    return this.map.get(key) ?? null;
  }
  setItem(key: string, value: string): void {
    this.map.set(key, value);
  }
  removeItem(key: string): void {
    this.map.delete(key);
  }
  clear(): void {
    this.map.clear();
  }
}

let fetchMock: ReturnType<typeof vi.fn>;
let replaced: string[];

/** 装一套够用的浏览器门面：地址栏、history、sessionStorage、cookie。 */
function browser(href: string, cookie = ''): void {
  replaced = [];
  vi.stubGlobal('sessionStorage', new MemoryStorage());
  vi.stubGlobal('document', { cookie });
  vi.stubGlobal('window', {
    location: { href },
    history: {
      replaceState: (_state: unknown, _title: string, url: string) => {
        replaced.push(url);
        const next = new URL(url, href);
        (globalThis as { window: { location: { href: string } } }).window.location.href =
          next.toString();
      },
    },
  });
}

beforeEach(() => {
  fetchMock = vi.fn(async () => ({ status: 200, ok: true, json: async () => ({ ok: true, data: {} }) }));
  vi.stubGlobal('fetch', fetchMock);
});
afterEach(() => vi.unstubAllGlobals());

/** 最近一次请求实际发出去的 `X-CPE-Token`。 */
function sentToken(): string {
  // 不用 `Array.prototype.at`：tsconfig 的 target 停在 ES2020（老 WebView 也要能跑）。
  const calls = fetchMock.mock.calls;
  const init = calls[calls.length - 1][1] as { headers: Record<string, string> };
  return init.headers['X-CPE-Token'];
}

describe('口令在第一发请求之前就落地', () => {
  it('地址栏带 ?token=：开场第一发请求就带着它，不用等 load()', async () => {
    browser('http://127.0.0.1:28800/?token=s3cr3t');
    // 这一句模拟 `App.vue` 里排在 `load()` **之前**的 `syncStatus()`。
    await api.get('/api/progress?from=0');
    expect(sentToken()).toBe('s3cr3t');
  });

  it('第一发请求就把地址栏里的 ?token= 抹掉', async () => {
    browser('http://127.0.0.1:28800/?token=s3cr3t');
    await api.get('/api/progress?from=0');
    expect(replaced).toEqual(['/']);
    expect(window.location.href).not.toContain('token=');
  });

  it('地址栏没带时退到服务端下发的会话 cookie（复制地址到新标签打开）', async () => {
    browser('http://127.0.0.1:28800/', 'other=1; cpe_ui_session=from%2Dcookie');
    await api.get('/api/progress?from=0');
    expect(sentToken()).toBe('from-cookie');
  });

  it('地址栏的口令比 cookie 新，两者都在时用地址栏那一份', async () => {
    browser('http://127.0.0.1:28800/?token=fresh', 'cpe_ui_session=stale');
    await api.get('/api/progress?from=0');
    expect(sentToken()).toBe('fresh');
  });

  it('服务端没设口令时，空口令是正常的，不是失败', async () => {
    browser('http://127.0.0.1:28800/');
    await api.get('/api/bootstrap');
    expect(sentToken()).toBe('');
  });

  it('浏览器自己发起的下载也拿得到口令（它不带自定义头）', () => {
    browser('http://127.0.0.1:28800/?token=a b');
    expect(downloadQuery()).toBe('?token=a%20b');
  });
});
