import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { MonitorPoint } from '../api/dto';
import { monitor, reset, startPolling, startSession, stopPolling } from './monitor';

/**
 * 监控会话表：**哪一批样本属于哪一路曲线**。
 *
 * 这一层此前没有任何测试（只有 `domain/monitor-chart` 的画法测试）。它管的是
 * 会话身份，而搞混会话的表现在屏幕上完全看不出来：两条曲线都在动、都有数，
 * 只是其中一条画的是另一块网卡——而人正是拿这两条曲线在对比两端。
 */

type FakeResponse = { status: number; ok: boolean; json: () => Promise<unknown> };
function ok(data: unknown): FakeResponse {
  return { status: 200, ok: true, json: async () => ({ ok: true, data }) };
}

function point(t: number, rx: number): MonitorPoint {
  return { t, rx_mbps: rx, tx_mbps: 0 } as MonitorPoint;
}

function series(session: string, points: MonitorPoint[], from: number, running = true) {
  return { session, side: 'master', iface: 'eth0', from, points, running, error: '' };
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  reset();
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  stopPolling();
  reset();
  vi.unstubAllGlobals();
});

describe('监控会话', () => {
  it('同一端的同一块网卡不许开两路', async () => {
    fetchMock.mockResolvedValue(ok({ session: 's1' }));
    expect(await startSession('master', 'eth0', 1000)).toBe(true);
    expect(monitor.sessions).toHaveLength(1);

    // 第二次必须被本模块挡下来，一个请求都不该发。
    fetchMock.mockClear();
    expect(await startSession('master', 'eth0', 1000)).toBe(false);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(monitor.sessions).toHaveLength(1);

    // 换一块网卡、或者换一端，都是另一路。
    fetchMock.mockResolvedValue(ok({ session: 's2' }));
    expect(await startSession('master', 'eth1', 1000)).toBe(true);
    fetchMock.mockResolvedValue(ok({ session: 's3' }));
    expect(await startSession('agent', 'eth0', 1000)).toBe(true);
    expect(monitor.sessions.map((s) => s.session)).toEqual(['s1', 's2', 's3']);
  });

  it('样本按 session 归位，不按顺序也不按网卡名', async () => {
    fetchMock.mockResolvedValue(ok({ session: 'a' }));
    await startSession('master', 'eth0', 1000);
    fetchMock.mockResolvedValue(ok({ session: 'b' }));
    await startSession('agent', 'eth0', 1000); // 同名网卡，不同端

    // 服务端把两路的顺序反过来送。
    // `startSession` 内部已经把轮询接上了；先停掉，再换 mock、重新起一拍，
    // 否则这里设的 mock 会和已经排在 1 秒后的那一拍抢。
    stopPolling();
    fetchMock.mockResolvedValue(
      ok({
        series: [series('b', [point(1, 200)], 1), series('a', [point(1, 100)], 1)],
      }),
    );
    startPolling();
    await vi.waitFor(() => expect(monitor.sessions[0].points).toHaveLength(1));
    stopPolling();

    const a = monitor.sessions.find((s) => s.session === 'a')!;
    const b = monitor.sessions.find((s) => s.session === 'b')!;
    expect(a.points[0].rx_mbps).toBe(100);
    expect(b.points[0].rx_mbps).toBe(200);
  });

  it('认不出的 session 直接丢掉，不许挂到别人身上', async () => {
    fetchMock.mockResolvedValue(ok({ session: 'a' }));
    await startSession('master', 'eth0', 1000);

    // 上一轮遗留的会话 id：这一路已经不在表里了。
    stopPolling();
    fetchMock.mockResolvedValue(
      ok({ series: [series('gone', [point(1, 999)], 1), series('a', [point(1, 10)], 1)] }),
    );
    startPolling();
    await vi.waitFor(() => expect(monitor.sessions[0].points).toHaveLength(1));
    stopPolling();

    expect(monitor.sessions).toHaveLength(1);
    expect(monitor.sessions[0].points.map((p) => p.rx_mbps)).toEqual([10]);
  });

  it('一拍失败不清掉已经画出来的点', async () => {
    fetchMock.mockResolvedValue(ok({ session: 'a' }));
    await startSession('master', 'eth0', 1000);
    stopPolling();
    fetchMock.mockResolvedValue(ok({ series: [series('a', [point(1, 10), point(2, 20)], 2)] }));
    startPolling();
    await vi.waitFor(() => expect(monitor.sessions[0].points).toHaveLength(2));

    // 下一拍断线：曲线必须原样留着——这条链路正在被灌到线速，
    // 抖一下就把曲线清空，人会以为是设备掉了。
    const before = monitor.sessions[0].points.length;
    const callsBeforeFailure = fetchMock.mock.calls.length;
    fetchMock.mockRejectedValue(new Error('boom'));
    // 必须等到**真的走过一拍失败**再断言，否则断言的是失败发生之前的状态。
    await vi.waitFor(
      () => expect(fetchMock.mock.calls.length).toBeGreaterThan(callsBeforeFailure),
      { timeout: 3000 },
    );
    stopPolling();
    expect(monitor.sessions[0].points).toHaveLength(before);
    expect(monitor.sessions[0].from).toBe(2);
  });
});
