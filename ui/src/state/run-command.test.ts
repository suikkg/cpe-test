import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { PlanOut } from '../api/dto';
import { plan, buildRunRequest } from './plan';
import { reset, run, start, stop, stopPolling, syncStatus } from './run';
import { session, reset as resetSession } from './session';

/**
 * 有副作用的命令**在拿不到应答时怎么办**，以及断线时屏幕上还剩什么。
 *
 * 这一组守的是三件在真机上代价很高的事：
 * 1. 「开始」超时后又自动发一遍 → 两轮同时灌包，而界面上看不出来；
 * 2. 轮询一断就清空 → 11.5 小时跑到一半，网络抖一下，已完成的结果全没了；
 * 3. 口令失效后继续按秒轮询 → 一串 401，屏幕上什么都不变。
 */

type FakeResponse = { status: number; ok: boolean; json: () => Promise<unknown> };

function ok(data: unknown): FakeResponse {
  return { status: 200, ok: true, json: async () => ({ ok: true, data }) };
}
function serverFailure(message: string): FakeResponse {
  return { status: 200, ok: true, json: async () => ({ ok: false, error: message }) };
}
function unauthorized(): FakeResponse {
  return { status: 401, ok: false, json: async () => ({ ok: false }) };
}

const emptyRun = {
  run_id: '',
  plan_hash: '',
  started_at: '',
  total_units: 0,
  current: null,
  done: [],
  counts: { pass: 0, fail: 0, measured: 0, not_evaluated: 0, setup_error: 0, skip: 0 },
  eta_secs: null,
  aborted_at_unit: null,
  report: '',
  finished: false,
};

function progress(runId: string, running: boolean): FakeResponse {
  return ok({
    running,
    from: 7,
    lines: ['a'],
    report: '',
    units_from: 0,
    run: { ...emptyRun, run_id: runId },
  });
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  reset();
  resetSession();
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  stopPolling();
  vi.unstubAllGlobals();
});

describe('运行状态的新鲜度', () => {
  it('读到之前是「未同步」，读到之后才有时刻', async () => {
    expect(run.synced).toBe(false);
    expect(run.lastSyncAt).toBeNull();
    fetchMock.mockResolvedValueOnce(progress('run_a', false));
    await syncStatus();
    expect(run.synced).toBe(true);
    expect(typeof run.lastSyncAt).toBe('number');
    expect(run.refreshError).toBe('');
  });

  it('断线只标旧，不清已完成的数据和游标', async () => {
    fetchMock.mockResolvedValueOnce(progress('run_a', false));
    await syncStatus();
    const at = run.lastSyncAt;
    expect(run.logCursor).toBe(7);
    expect(run.lines).toEqual(['a']);

    fetchMock.mockRejectedValueOnce(new TypeError('Failed to fetch'));
    await syncStatus();
    expect(run.refreshError).not.toBe('');
    // 数据、游标、上次成功时刻一个都不许动——它们是断线期间屏幕上仅剩的东西。
    expect(run.logCursor).toBe(7);
    expect(run.lines).toEqual(['a']);
    expect(run.lastSyncAt).toBe(at);
    expect(run.synced).toBe(true);
  });

  it('口令失效进全局终态并停掉轮询，不再刷 401', async () => {
    // 轮询链正开着——这正是「一直刷 401 而屏幕上什么都不变」的那个前提。
    run.polling = true;
    fetchMock.mockResolvedValue(unauthorized());
    await syncStatus();
    expect(session.phase).toBe('unauthorized');
    expect(run.polling).toBe(false);
    // 这不是「断线」，不该在进度页上显示成正在重试。
    expect(run.refreshError).toBe('');
  });
});

describe('开始：结果未知不等于失败', () => {
  beforeEach(() => {
    plan.preview = {
      plan_hash: 'h',
      units: [],
      trace: [],
      notices: [],
      est_full_secs: 1,
      est_total_secs: 1,
      sections: [],
    } as unknown as PlanOut;
    plan.previewRequestFingerprint = JSON.stringify(buildRunRequest());
  });

  it('服务端答了失败：确定没起跑，就地重试', async () => {
    fetchMock.mockResolvedValueOnce(serverFailure('计划已过期'));
    await start();
    expect(run.startPhase).toBe('idle');
    expect(run.startError).toContain('计划已过期');
    expect(run.running).toBe(false);
  });

  it('没拿到应答：进「未确认」，并且**不再发一次** /api/run', async () => {
    fetchMock.mockImplementation(async (path: string) => {
      if (String(path).startsWith('/api/run')) throw new TypeError('Failed to fetch');
      return progress('run_a', false);
    });
    await start();
    expect(run.startPhase).toBe('unknown');
    expect(run.startError).toBe('');
    const runPosts = fetchMock.mock.calls.filter((c) => String(c[0]).startsWith('/api/run'));
    expect(runPosts).toHaveLength(1);
    // 它改去读状态了——那是唯一允许的后续动作。
    expect(fetchMock.mock.calls.some((c) => String(c[0]).startsWith('/api/progress'))).toBe(true);
  });

  it('连点两次只发一条命令', async () => {
    let resolveRun: (value: FakeResponse) => void = () => {};
    fetchMock.mockImplementation(
      (path: string) =>
        String(path).startsWith('/api/run')
          ? new Promise<FakeResponse>((resolve) => { resolveRun = resolve; })
          : Promise.resolve(progress('run_a', true)),
    );
    const first = start();
    const second = start();
    resolveRun(ok(null));
    await Promise.all([first, second]);
    const runPosts = fetchMock.mock.calls.filter((c) => String(c[0]).startsWith('/api/run'));
    expect(runPosts).toHaveLength(1);
  });
});

describe('受理了，却没有起跑', () => {
  it('POST 成功但这一轮在产生 run_id 之前就结束了，必须说出来', async () => {
    plan.preview = {
      plan_hash: 'h',
      units: [],
      trace: [],
      notices: [],
      est_full_secs: 1,
      est_total_secs: 1,
      sections: [],
    } as unknown as PlanOut;
    plan.previewRequestFingerprint = JSON.stringify(buildRunRequest());
    fetchMock.mockImplementation(async (path: string) =>
      String(path).startsWith('/api/run') ? ok(null) : progress('', false),
    );
    await start();
    expect(run.startPhase).toBe('accepted');
    // 执行端复核计划哈希不通过就会这样退出：POST 成功过，界面这边一切正常，
    // 屏幕却从「启动中」悄悄回到「空闲」，没有一处提到刚才那次点击。
    await syncStatus();
    expect(run.startPhase).toBe('idle');
    expect(run.startError).toContain('在产生任何测试单元之前就结束');
    expect(run.running).toBe(false);
  });
});

describe('停止：受理不是结束', () => {
  it('HTTP 成功只把它记成「已受理」，不动 running', async () => {
    run.running = true;
    fetchMock.mockResolvedValueOnce(ok(null));
    await stop();
    expect(run.stopPhase).toBe('accepted');
    // 收尾还要跑一会儿；这里若直接置成结束，屏幕会先说结束再被轮询翻回来。
    expect(run.running).toBe(true);
  });

  it('真的停下来之后，那句「已请求停止」自己收回去', async () => {
    run.running = true;
    fetchMock.mockResolvedValueOnce(ok(null));
    await stop();
    expect(run.stopPhase).toBe('accepted');
    // 结束由运行状态说了算；留着这句话，结束之后界面上还挂着一个悬而未决的动作。
    fetchMock.mockResolvedValueOnce(progress('run_a', false));
    await syncStatus();
    expect(run.stopPhase).toBe('idle');
    expect(run.running).toBe(false);
  });

  it('没拿到应答时不认定已停，也不重发', async () => {
    run.running = true;
    fetchMock.mockImplementation(async (path: string) => {
      if (String(path).startsWith('/api/stop')) throw new TypeError('Failed to fetch');
      return progress('run_a', true);
    });
    await stop();
    expect(run.stopPhase).toBe('unknown');
    expect(run.running).toBe(true);
    const stopPosts = fetchMock.mock.calls.filter((c) => String(c[0]).startsWith('/api/stop'));
    expect(stopPosts).toHaveLength(1);
  });
});
