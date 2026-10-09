import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { PlanOut } from '../api/dto';
import { plan, buildRunRequest, reset as resetPlan } from './plan';
import { applyProgress, prepareAfterUnknownStart, reset, run, skipUnit, start, stop, stopPolling, syncStatus } from './run';
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
  it('并发同步等待同一个请求，不会提前返回或重复请求', async () => {
    let resolveResponse!: (value: FakeResponse) => void;
    fetchMock.mockReturnValue(new Promise<FakeResponse>((resolve) => { resolveResponse = resolve; }));
    const first = syncStatus();
    const second = syncStatus();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    resolveResponse(progress('shared', false));
    await Promise.all([first, second]);
    expect(run.status.run_id).toBe('shared');
    expect(run.synced).toBe(true);
  });

  it('重置之后迟到的旧响应不能覆盖新状态', async () => {
    let resolveOld!: (value: FakeResponse) => void;
    fetchMock.mockReturnValueOnce(new Promise<FakeResponse>((resolve) => { resolveOld = resolve; }));
    const old = syncStatus();
    reset();
    fetchMock.mockResolvedValueOnce(progress('new', false));
    await syncStatus();
    resolveOld(progress('old', false));
    await old;
    expect(run.status.run_id).toBe('new');
  });

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

  it('开始成功后，开始前发出的旧快照不能把上一轮结果写回来', async () => {
    let releaseOld!: (value: FakeResponse) => void;
    fetchMock.mockReturnValueOnce(new Promise<FakeResponse>((resolve) => { releaseOld = resolve; }));
    const previous = syncStatus();
    fetchMock.mockResolvedValueOnce(ok(null));
    await start();
    expect(run.startPhase).toBe('accepted');
    releaseOld(progress('previous-run', false));
    await previous;
    expect(run.running).toBe(true);
    expect(run.status.run_id).toBe('');
    expect(run.lines).toEqual([]);
    expect(run.startError).toBe('');
  });

  it('重置后的旧开始应答不能启动轮询或改变新会话', async () => {
    let release!: (value: FakeResponse) => void;
    fetchMock.mockReturnValueOnce(new Promise<FakeResponse>((resolve) => { release = resolve; }));
    const previous = start();
    reset();
    release(ok(null));
    await previous;
    expect(run.running).toBe(false);
    expect(run.startPhase).toBe('idle');
    expect(run.polling).toBe(false);
  });

  it('未知应答后空闲快照不自动解锁；显式恢复只清预览、不重发开始', async () => {
    run.startPhase = 'unknown';
    fetchMock.mockResolvedValue(progress('', false));
    await syncStatus();
    expect(run.startPhase).toBe('unknown');
    await start();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    prepareAfterUnknownStart();
    expect(run.startPhase).toBe('idle');
    expect(plan.preview).toBeNull();
    expect(plan.previewRequestFingerprint).toBe('');
    expect(fetchMock).toHaveBeenCalledTimes(1);
    await start();
    expect(run.startError).toContain('先点「预览」');
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('开始超时后必须收到新的状态，旧空闲快照不能提前允许人工恢复', async () => {
    fetchMock.mockResolvedValueOnce(progress('', false));
    await syncStatus();
    expect(run.synced).toBe(true);
    expect(run.lastSyncAt).not.toBeNull();

    let releaseOld!: (value: FakeResponse) => void;
    fetchMock.mockReturnValueOnce(new Promise<FakeResponse>((resolve) => { releaseOld = resolve; }));
    const oldRead = syncStatus();
    fetchMock.mockRejectedValueOnce(new TypeError('Failed to fetch'));
    let releaseNew!: (value: FakeResponse) => void;
    fetchMock.mockReturnValueOnce(new Promise<FakeResponse>((resolve) => { releaseNew = resolve; }));
    await start();
    expect(run.startPhase).toBe('unknown');
    expect(run.synced).toBe(false);
    expect(run.lastSyncAt).toBeNull();
    prepareAfterUnknownStart();
    expect(run.startPhase).toBe('unknown');
    expect(plan.preview).not.toBeNull();

    releaseOld(progress('', false));
    await oldRead;
    expect(run.synced).toBe(false);
    prepareAfterUnknownStart();
    expect(run.startPhase).toBe('unknown');

    const newRead = syncStatus();
    releaseNew(progress('', false));
    await newRead;
    expect(run.synced).toBe(true);
    expect(run.startPhase).toBe('unknown');
    prepareAfterUnknownStart();
    expect(run.startPhase).toBe('idle');
    expect(plan.preview).toBeNull();
    expect(fetchMock.mock.calls.filter((call) => String(call[0]) === '/api/run')).toHaveLength(1);
  });

  it('未同步、运行中或同步失败时不能解除未知开始', () => {
    run.startPhase = 'unknown';
    prepareAfterUnknownStart();
    expect(run.startPhase).toBe('unknown');
    run.synced = true;
    run.running = true;
    prepareAfterUnknownStart();
    expect(run.startPhase).toBe('unknown');
    run.running = false;
    run.refreshError = '断线';
    prepareAfterUnknownStart();
    expect(run.startPhase).toBe('unknown');
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

/**
 * **一秒一拍，而且任何时刻最多一个在飞的请求**（回归方案 UI-05）。
 *
 * 这条守的是 `run.ts` 里那个 `inFlight` 闸门。它有历史：旧页用的是
 * `setInterval(poll, 1000)`，机器一忙请求就叠着发——而这台机器此刻**正在灌
 * 线速**，多出来的请求抢的是被测链路自己的带宽，测出来的数会因为「打开了
 * 控制台」而变低。`lint-arch.mjs` 全局禁 `setInterval` 挡的是同一件事，但
 * 挡不住「快照那条链和轮询那条链各发各的」。
 *
 * 光有 `setTimeout` 链不够：`syncStatus()` 是第二个出口，页面刚打开时它和
 * 轮询链会同时在跑。所以断言的是**出口合并后的效果**——慢响应期间无论怎么
 * 催，都只有一个请求在飞。
 */
describe('轮询不叠加', () => {
  it('响应比轮询周期还慢时，请求也不许叠着发', async () => {
    let inFlight = 0;
    let maxInFlight = 0;
    let release: (() => void) | undefined;
    fetchMock.mockImplementation(async () => {
      inFlight += 1;
      maxInFlight = Math.max(maxInFlight, inFlight);
      await new Promise<void>((resolve) => {
        release = resolve;
      });
      inFlight -= 1;
      return progress('run-slow', true);
    });

    // 第一拍挂住不返回，模拟一个比轮询周期还慢的响应。
    const first = syncStatus();
    await Promise.resolve();
    expect(fetchMock).toHaveBeenCalledTimes(1);

    // 在它还没落地时反复催——真实里这是「用户切回页面 + 轮询到点」同时发生。
    const extra = [syncStatus(), syncStatus(), syncStatus()];
    await Promise.resolve();
    expect(maxInFlight).toBe(1);
    expect(fetchMock).toHaveBeenCalledTimes(1);

    release?.();
    await Promise.all([first, ...extra]);
    expect(maxInFlight).toBe(1);
    // `syncStatus` 读到 running=true 会顺手接上轮询链，那条链会再发一拍并
    // 挂在同一个 mock 上。收尾时放掉它，免得把 `inFlight` 漏给下一条用例。
    stopPolling();
    release?.();
    await Promise.resolve();
  });

  it('前一拍落地之后才排下一拍，且落地的数据照常生效', async () => {
    // running=false：这一条只看闸门的放开，不把轮询链也牵进来
    // （读到「正在跑」会顺手接上轮询，那会多发一拍，掩盖掉这里要验的东西）。
    fetchMock.mockResolvedValue(progress('run-a', false));
    await syncStatus();
    expect(run.synced).toBe(true);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    // 落地之后再催一次，这一次必须真的发出去——否则闸门就成了「只发一次」。
    await syncStatus();
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });
});

/**
 * **一个挂死的请求不许把轮询链永久卡住**（回归方案 UI-05 / 缺陷 D-10）。
 *
 * `inFlight` 保证同时最多一个在飞的请求——但它是模块级的，而 `fetch` 自己
 * **不带超时**。连接断在半路（被测链路正被灌到线速、辅测机掉线）时 `fetch`
 * 可能几分钟既不 reject 也不 resolve，于是：
 *
 * - 后续每一拍都被 `inFlight` 挡掉，`tick()` 在进 try 之前就返回；
 * - `run.refreshError` 因此**永远不会被赋值**——屏幕上没有任何错误，只是数据
 *   一直是上一拍的，「上次同步」停在几分钟前；
 * - 「断开连接 / 换辅测机」也救不回来：`reset()` 不清 `inFlight`。
 *
 * 11.5 小时的长测试里网络抖一次就够了。两处都要修：请求要有上限，
 * `reset()` 要把闸门放开。
 */
describe('挂死的请求不会永久卡住轮询', () => {
  it('reset 之后闸门必须放开，新连上的机器不该被上一台的挂死请求挡住', async () => {
    let release: (() => void) | undefined;
    fetchMock.mockImplementation(
      () =>
        new Promise<FakeResponse>((resolve) => {
          release = () => resolve(progress('run-x', false));
        }),
    );
    const hung = syncStatus();
    await Promise.resolve();
    expect(fetchMock).toHaveBeenCalledTimes(1);

    // 请求还挂着，这时候「断开连接」。
    reset();

    // 换了一台机器，新的读取必须真的发得出去。
    fetchMock.mockResolvedValue(progress('run-y', false));
    await syncStatus();
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(run.synced).toBe(true);

    release?.();
    await hung;
  });

  it('超过上限的请求会被中止，并落到「断线」那条路上显示出来', async () => {
    vi.useFakeTimers();
    try {
      // 真实的 fetch 会在 signal abort 时 reject；这里照做。
      fetchMock.mockImplementation(
        (_url: string, init: { signal?: AbortSignal }) =>
          new Promise((_resolve, reject) => {
            init.signal?.addEventListener('abort', () =>
              reject(new Error('The operation was aborted')),
            );
          }),
      );
      const pending = syncStatus();
      await vi.advanceTimersByTimeAsync(31_000);
      await pending;
      expect(run.refreshError).not.toBe('');
      expect(run.synced).toBe(false);
    } finally {
      vi.useRealTimers();
    }

    // 关键的一半：中止之后闸门放开了，下一拍能正常发。
    fetchMock.mockResolvedValue(progress('run-z', false));
    await syncStatus();
    expect(run.synced).toBe(true);
    expect(run.refreshError).toBe('');
  });
});

describe('负载下时延开关', () => {
  it('默认关，且请求体里如实带着 false', () => {
    // 打开它会在每个方向旁边多跑一个 ping 子进程，也就是改变了测量条件。
    // 升级一次就悄悄生效，等于在没人注意的情况下改了基线。
    resetPlan();
    expect(plan.probeDuringTraffic).toBe(false);
    expect(buildRunRequest().probe_during_traffic).toBe(false);
  });

  it('勾上之后进请求体', () => {
    resetPlan();
    plan.probeDuringTraffic = true;
    expect(buildRunRequest().probe_during_traffic).toBe(true);
  });
});

describe('跳过当前单元', () => {
  it('没有明确的运行或当前单元时不发送跳过，首个单元到达后仍只受理一次', async () => {
    const current = { seq: 1, title: '首个单元', est_secs: 180, started_at: '', link_group: '' };
    run.running = true;
    await skipUnit();
    run.status = { ...emptyRun, run_id: 'active', current: null };
    await skipUnit();
    run.status = { ...emptyRun, run_id: '', current };
    await skipUnit();
    expect(fetchMock).not.toHaveBeenCalled();
    expect(run.skipPhase).toBe('idle');

    const active = { running: true, from: 0, lines: [], report: '', units_from: 0,
      run: { ...emptyRun, run_id: 'active', current } };
    applyProgress(active);
    fetchMock.mockImplementation(async (path: string) =>
      path === '/api/skip-unit' ? ok(null) : ok(active));
    await skipUnit();
    await syncStatus();
    expect(run.skipPhase).toBe('accepted');
    await skipUnit();
    const requests = fetchMock.mock.calls.filter((call) => String(call[0]) === '/api/skip-unit');
    expect(requests).toHaveLength(1);
    expect(JSON.parse(requests[0][1].body)).toEqual({ run_id: 'active', unit_seq: 1 });
  });

  it('旧单元增量不解除跳过，目标完成后未知应答才解除', async () => {
    const current = { seq: 2, title: '当前单元', est_secs: 180, started_at: '', link_group: '' };
    run.status = { ...emptyRun, run_id: 'active', current };
    run.running = true;
    fetchMock.mockRejectedValue(new TypeError('Failed to fetch'));
    await skipUnit();
    expect(run.skipPhase).toBe('unknown');
    const completed = (seq: number) => ({
      seq, title: `unit ${seq}`, verdict: 'PASS', reason_code: '', reason_detail: '',
      skipped: false, secs: 1, link_group: '',
    });
    applyProgress({ running: true, from: 0, lines: [], report: '', units_from: 1,
      run: { ...emptyRun, run_id: 'active', current, done: [completed(1)] } });
    expect(run.skipPhase).toBe('unknown');
    applyProgress({ running: true, from: 0, lines: [], report: '', units_from: 2,
      run: { ...emptyRun, run_id: 'active', current: null, done: [completed(2)] } });
    expect(run.skipPhase).toBe('idle');
  });

  it('已受理的跳过和停止不能再次发送', async () => {
    run.running = true;
    run.skipPhase = 'accepted';
    run.stopPhase = 'accepted';
    await skipUnit();
    await stop();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('拿不到应答时不自动重发——重发会把下一个单元也跳掉', async () => {
    // 和「开始」「停止」同一条纪律：有副作用的命令超时后一律去读状态，
    // 不凭超时重来。这里的代价尤其具体：多跳一个单元，而且看不出来。
    let calls = 0;
    vi.spyOn(globalThis, 'fetch').mockImplementation(() => {
      calls += 1;
      return Promise.reject(new TypeError('Failed to fetch'));
    });
    reset();
    run.running = true;
    run.status = { ...emptyRun, run_id: 'active',
      current: { seq: 1, title: '当前单元', est_secs: 180, started_at: '', link_group: '' } };
    await skipUnit();
    expect(run.skipPhase).toBe('unknown');
    expect(calls).toBeLessThanOrEqual(2); // 一次 skip + 一次状态同步，没有重发
  });

  it('跳过与停止是两个状态位，互不覆盖', () => {
    // 共用一个状态位的话，跳过一次之后界面会一直显示「已请求停止」。
    reset();
    expect(run.skipPhase).toBe('idle');
    expect(run.stopPhase).toBe('idle');
    run.skipPhase = 'accepted';
    expect(run.stopPhase).toBe('idle');
  });
});
