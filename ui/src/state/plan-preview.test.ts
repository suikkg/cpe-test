import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api } from '../api/client';
import type * as ApiModule from '../api/client';
import type { PlanOut } from '../api/dto';
import { defaultGlobals } from '../domain/globals';
import { emptyPlan, ensureDefaults } from '../domain/plan-build';
import { serializeProject } from '../domain/project';
import {
  adoptRunRequest, buildRunRequest, importProject, plan, preview, previewIsCurrent,
  reset, restoreDefaultProject,
} from './plan';
import { reset as resetSession } from './session';

vi.mock('../api/client', async (original) => ({
  ...await original<typeof ApiModule>(),
  api: { get: vi.fn(), post: vi.fn() },
}));

function deferredPreview() {
  let resolve!: (value: PlanOut) => void;
  let reject!: (reason: Error) => void;
  const response = new Promise<PlanOut>((yes, no) => { resolve = yes; reject = no; });
  vi.mocked(api.post).mockReturnValueOnce(response);
  return {
    pending: preview(),
    finish: (fail = false, hash = 'old') => {
      if (fail) reject(new Error('旧请求失败'));
      else resolve({ plan_hash: hash } as PlanOut);
    },
  };
}

function changeRunOptions(): void {
  plan.resume = true;
  plan.screenshot = true;
  plan.probeDuringTraffic = true;
  plan.probePathMtu = true;
  plan.forceTcpWindow = '64k';
  plan.forceUdpBandwidth = '500m';
  plan.rounds = 100;
}

function expectDefaultRunOptions(): void {
  expect(buildRunRequest()).toMatchObject({
    resume: false,
    screenshot: false,
    probe_during_traffic: false,
    probe_path_mtu: false,
    force_tcp_window: '',
    force_udp_bandwidth: '',
    rounds: 1,
  });
}

beforeEach(() => {
  resetSession();
  reset();
  vi.clearAllMocks();
});

describe('子网预览只接受当前请求', () => {
  it.each([false, true])('旧请求先返回（失败=%s）不能结束新请求的忙碌态', async (fail) => {
    const old = deferredPreview();
    plan.duration = 60;
    const latest = deferredPreview();
    old.finish(fail);
    await old.pending;
    expect(plan.previewing).toBe(true);
    expect(plan.preview).toBeNull();
    expect(plan.previewError).toBe('');

    latest.finish(false, 'latest');
    await latest.pending;
    expect(plan.previewing).toBe(false);
    expect(plan.preview?.plan_hash).toBe('latest');
    expect(previewIsCurrent()).toBe(true);
  });

  it.each([false, true])('旧请求迟到（失败=%s）不能覆盖已完成的新预览', async (fail) => {
    const old = deferredPreview();
    plan.rounds = 2;
    const latest = deferredPreview();
    latest.finish(false, 'latest');
    await latest.pending;
    old.finish(fail);
    await old.pending;
    expect(plan.preview?.plan_hash).toBe('latest');
    expect(plan.previewError).toBe('');
    expect(previewIsCurrent()).toBe(true);
  });

  it.each([false, true])('配置已编辑、尚未发新预览时也丢弃旧响应（失败=%s）', async (fail) => {
    const old = deferredPreview();
    plan.globals.udp_bandwidths.push('500m');
    // 网络层拿到独立快照；在途请求不会随着编辑态继续变化。
    expect(vi.mocked(api.post).mock.calls[0]?.[1]).toMatchObject({ udp_bandwidths: ['2500m'] });
    old.finish(fail);
    await old.pending;
    expect(plan.preview).toBeNull();
    expect(plan.previewRequestFingerprint).toBe('');
    expect(plan.previewError).toBe('');
    expect(plan.previewing).toBe(false);
    expect(previewIsCurrent()).toBe(false);
  });

  const replacements = [
    ['重新开始', () => reset()],
    ['恢复默认项目', () => restoreDefaultProject()],
    ['导入项目', () => {
      expect(importProject(serializeProject(ensureDefaults(emptyPlan()), {
        duration: 180, globals: defaultGlobals(),
      }))).toBe(true);
    }],
    ['装载历史', () => {
      expect(adoptRunRequest(buildRunRequest(), false)).toBe(true);
    }],
  ] as const;

  it.each(replacements)('%s 即使配置相同也不能复活旧预览或旧错误', async (_name, replace) => {
    for (const fail of [false, true]) {
      const old = deferredPreview();
      replace();
      expect(plan.previewing).toBe(false);
      old.finish(fail);
      await old.pending;
      expect(plan.preview).toBeNull();
      expect(plan.previewRequestFingerprint).toBe('');
      expect(plan.previewError).toBe('');
      expect(previewIsCurrent()).toBe(false);
    }
  });

  it('导入失败不取消当前配置的预览，也不清掉本轮设置', async () => {
    changeRunOptions();
    const before = JSON.stringify(buildRunRequest());
    const current = deferredPreview();
    expect(importProject('{错误 JSON')).toBe(false);
    expect(JSON.stringify(buildRunRequest())).toBe(before);
    expect(plan.previewing).toBe(true);
    current.finish(false, 'current');
    await current.pending;
    expect(plan.preview?.plan_hash).toBe('current');
    expect(previewIsCurrent()).toBe(true);
  });
});

describe('项目切换隔离本轮设置，历史重跑保留归档设置', () => {
  it('导入项目清除 100 轮和强制覆盖，保留项目自己的时长及档位', () => {
    changeRunOptions();
    plan.preview = { plan_hash: 'previous-project' } as PlanOut;
    plan.previewRequestFingerprint = JSON.stringify(buildRunRequest());
    const globals = defaultGlobals();
    globals.udp_bandwidths = ['1000m'];
    const project = serializeProject(ensureDefaults(emptyPlan()), {
      duration: 60,
      limit_udp_by_link_speed: true,
      globals,
      masterConfig: { ping: { count: 8 } },
    });
    expect(importProject(project)).toBe(true);
    expectDefaultRunOptions();
    expect(plan.duration).toBe(60);
    expect(plan.limitUdpByLinkSpeed).toBe(true);
    expect(plan.globals.udp_bandwidths).toEqual(['1000m']);
    expect(plan.masterConfig).toEqual({ ping: { count: 8 } });
    expect(plan.preview).toBeNull();
    expect(previewIsCurrent()).toBe(false);
  });

  it('恢复默认项目清除本轮开关，保留现有的全局档位与时长', () => {
    changeRunOptions();
    plan.duration = 60;
    plan.globals.udp_bandwidths = ['1000m'];
    restoreDefaultProject();
    expectDefaultRunOptions();
    expect(plan.duration).toBe(60);
    expect(plan.globals.udp_bandwidths).toEqual(['1000m']);
  });

  it.each([false, true])('历史重跑按归档恢复临时参数，并应用本次跳过选择=%s', (skipPassed) => {
    changeRunOptions();
    const archive = JSON.parse(JSON.stringify(buildRunRequest()));
    reset();
    expect(adoptRunRequest(archive, skipPassed)).toBe(true);
    expect(buildRunRequest()).toMatchObject({
      resume: skipPassed,
      screenshot: true,
      probe_during_traffic: true,
      probe_path_mtu: true,
      force_tcp_window: '64k',
      force_udp_bandwidth: '500m',
      rounds: 100,
    });
  });
});
