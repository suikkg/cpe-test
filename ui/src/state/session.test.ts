import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { connect, load, rescan, reset, session } from './session';

/**
 * 连接这一页的**异常路径**。
 *
 * 三件事在真机上都出现过、而且都只在"慢"或"错"的时候才出现——也就是最难复现、
 * 最容易被当成偶发的那一类：
 *
 * 1. `/api/bootstrap` 在 Windows 上要拉起 ipconfig / netsh，一两秒才回来；
 *    这段时间里用户改掉的地址会被回填悄悄换回去。
 * 2. 连接失败时把网卡表清空，于是"没扫到网卡"和"刚才那次请求失败了"在屏幕上
 *    长得一模一样。
 * 3. bootstrap 与 local 共用一格错误，后落地的成功会把先落地的错误擦掉。
 */

type FakeResponse = { status: number; ok: boolean; json: () => Promise<unknown> };
const ok = (data: unknown): FakeResponse => ({ status: 200, ok: true, json: async () => ({ ok: true, data }) });
const boom = (): FakeResponse => ({ status: 500, ok: false, json: async () => ({ ok: false }) });
const unauthorized = (): FakeResponse => ({ status: 401, ok: false, json: async () => ({ ok: false }) });

const bootstrap = {
  agent_host: '192.168.1.3',
  agent_port: 28801,
  ipv4_prefixes: ['192.168.'],
  token_configured: true,
};
const host = (name: string, ifaces: number) => ({
  hostname: name,
  interfaces: Array.from({ length: ifaces }, (_, i) => ({ name: `en${i}`, ipv4: `10.0.0.${i}` })),
});
const localOut = { host: host('master', 2), iperf3: 'iperf 3.18', version: '6.2.8' };
const connectOut = { master: host('master', 2), agent: host('agent', 3) };

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  reset();
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});
afterEach(() => vi.unstubAllGlobals());

function route(map: Record<string, FakeResponse | (() => never)>) {
  fetchMock.mockImplementation(async (path: string) => {
    for (const [prefix, value] of Object.entries(map)) {
      if (String(path).startsWith(prefix)) {
        if (typeof value === 'function') value();
        return value;
      }
    }
    throw new Error(`未打桩的请求 ${path}`);
  });
}

describe('bootstrap 回填', () => {
  it('用户没动过就回填', async () => {
    route({ '/api/bootstrap': ok(bootstrap), '/api/local': ok(localOut) });
    await load();
    expect(session.host).toBe('192.168.1.3');
    expect(session.prefixes).toEqual(['192.168.']);
  });

  it('用户已经敲了地址时，晚到的回填不许盖掉', async () => {
    // 请求在飞的时候用户改了地址——慢机器上这是常态，快机器上永远复现不了。
    session.host = '10.9.9.9';
    route({ '/api/bootstrap': ok(bootstrap), '/api/local': ok(localOut) });
    await load();
    expect(session.host).toBe('10.9.9.9');
  });
});

describe('两个独立请求的失败不互相覆盖', () => {
  it('bootstrap 失败、local 成功：错误留在自己那一格', async () => {
    route({ '/api/bootstrap': boom(), '/api/local': ok(localOut) });
    await load();
    expect(session.bootstrapError).not.toBe('');
    expect(session.localError).toBe('');
    expect(session.local).not.toBeNull();
    // 这两个都不是「连接失败」，不该动连接状态。
    expect(session.phase).toBe('idle');
  });

  it('local 失败、bootstrap 成功：同上，反过来', async () => {
    route({ '/api/bootstrap': ok(bootstrap), '/api/local': boom() });
    await load();
    expect(session.localError).not.toBe('');
    expect(session.bootstrapError).toBe('');
    expect(session.bootstrap).not.toBeNull();
  });

  it('任一个 401 都进全局终态', async () => {
    route({ '/api/bootstrap': unauthorized(), '/api/local': ok(localOut) });
    await load();
    expect(session.phase).toBe('unauthorized');
  });
});

describe('连接身份与旧快照', () => {
  it('连接未完成时复用同一请求，输入草稿不冒充已连接身份', async () => {
    let release!: (value: FakeResponse) => void;
    fetchMock.mockReturnValueOnce(new Promise<FakeResponse>((resolve) => { release = resolve; }));
    session.host = '192.168.1.3';
    const first = connect();
    session.host = '192.168.1.9';
    const second = connect();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    release(ok(connectOut));
    await Promise.all([first, second]);
    expect(session.connectedHost).toBe('192.168.1.3');
    expect(session.connectedPort).toBe(28801);
    expect(session.host).toBe('192.168.1.9');
  });

  it('重置后的旧连接应答不能覆盖新会话', async () => {
    let release!: (value: FakeResponse) => void;
    fetchMock.mockReturnValueOnce(new Promise<FakeResponse>((resolve) => { release = resolve; }));
    session.host = '192.168.1.3';
    const old = connect();
    reset();
    session.host = '192.168.1.9';
    const newer = { master: host('new-master', 1), agent: host('new-agent', 1) };
    fetchMock.mockResolvedValueOnce(ok(newer));
    await connect();
    release(ok(connectOut));
    await old;
    expect(session.connectedHost).toBe('192.168.1.9');
    expect(session.connection?.agent.hostname).toBe('new-agent');
    expect(session.phase).toBe('connected');
  });

  it('成功之后才落地身份，失败保留上一份拓扑并标旧', async () => {
    session.host = '192.168.1.3';
    route({ '/api/connect': ok(connectOut) });
    await connect();
    expect(session.connectedHost).toBe('192.168.1.3');
    expect(session.connection).not.toBeNull();
    expect(session.topologyStale).toBe(false);

    // 换一台机器、连失败：顶栏那句「已连 …」说的还是上一台，网卡表也还在。
    session.host = '10.9.9.9';
    route({ '/api/connect': boom() });
    await connect();
    expect(session.phase).toBe('failed');
    expect(session.connectedHost).toBe('192.168.1.3');
    expect(session.connection).not.toBeNull();
    expect(session.topologyStale).toBe(true);
  });
});

describe('重新扫描', () => {
  it('重置后的旧扫描不能覆盖新本机快照或扫描状态', async () => {
    let release!: (value: FakeResponse) => void;
    fetchMock.mockReturnValueOnce(new Promise<FakeResponse>((resolve) => { release = resolve; }));
    const old = rescan();
    reset();
    session.scanning = true;
    release(ok(localOut));
    await old;
    expect(session.local).toBeNull();
    expect(session.scanning).toBe(true);
    expect(session.scanMessage).toBe('');
  });

  it('扫描失败保留上次成功的两张表，并说明下面是旧的', async () => {
    session.host = '192.168.1.3';
    route({ '/api/connect': ok(connectOut) });
    await connect();
    const before = session.connection;

    route({ '/api/local': ok(localOut), '/api/connect': boom() });
    await rescan();
    expect(session.scanKind).toBe('bad');
    expect(session.scanMessage).toContain('仍是上次成功的网卡');
    // 抹成空拓扑会让「分配链路」那一页 reconcile 掉用户的分配意图。
    expect(session.connection).toBe(before);
  });

  it('扫描成功时报出两端块数和完成时刻', async () => {
    session.host = '192.168.1.3';
    route({ '/api/connect': ok(connectOut), '/api/local': ok(localOut) });
    await connect();
    await rescan();
    expect(session.scanKind).toBe('ok');
    expect(session.scanMessage).toContain('本机 2 块 / 辅测 3 块');
    expect(session.scanMessage).toMatch(/\d{2}:\d{2}:\d{2}/);
  });

  it('连接扫描失败后的再次扫描必须刷新实际展示的双端快照', async () => {
    session.host = '192.168.1.3';
    route({ '/api/connect': ok(connectOut), '/api/local': ok(localOut) });
    await connect();
    route({ '/api/local': ok(localOut), '/api/connect': boom() });
    await rescan();
    expect(session.phase).toBe('failed');

    const refreshed = { master: host('master', 1), agent: host('agent', 4) };
    route({ '/api/local': ok(localOut), '/api/connect': ok(refreshed) });
    await rescan();
    expect(session.phase).toBe('connected');
    expect(session.topologyStale).toBe(false);
    expect(session.connection).toEqual(refreshed);
    expect(session.scanMessage).toContain('本机 1 块 / 辅测 4 块');
  });

  it('再次连接仍失败时不能把只刷新的本机信息报成双端扫描成功', async () => {
    session.host = '192.168.1.3';
    route({ '/api/connect': ok(connectOut), '/api/local': ok(localOut) });
    await connect();
    route({ '/api/local': ok(localOut), '/api/connect': boom() });
    await rescan();
    await rescan();
    expect(session.scanKind).toBe('bad');
    expect(session.scanMessage).toContain('仍是上次成功的网卡');
    expect(session.connection).toEqual(connectOut);
  });
});

describe('重扫在本机请求阶段失败', () => {
  it('双端快照保留并标旧，不继续宣称连接有效', async () => {
    session.host = '192.168.1.3';
    route({ '/api/connect': ok(connectOut) });
    await connect();
    const connectedAt = session.connectedAt;
    route({ '/api/local': () => { throw new TypeError('offline'); } });
    await rescan();
    expect(session.phase).toBe('failed');
    expect(session.topologyStale).toBe(true);
    expect(session.connection).toEqual(connectOut);
    expect(session.connectedAt).toBe(connectedAt);
    expect(session.scanMessage).toContain('仍是上次成功的网卡');
    expect(session.scanning).toBe(false);
  });

  it('本机扫描 401 保留快照但进入鉴权终态', async () => {
    route({ '/api/connect': ok(connectOut) });
    await connect();
    route({ '/api/local': unauthorized() });
    await rescan();
    expect(session.phase).toBe('unauthorized');
    expect(session.topologyStale).toBe(true);
    expect(session.connection).toEqual(connectOut);
    expect(session.scanKind).toBe('');
    expect(session.scanning).toBe(false);
  });
});
