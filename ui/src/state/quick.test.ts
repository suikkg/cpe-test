import { beforeEach, expect, it, vi } from 'vitest';
import { api } from '../api/client';
import type { NicInfo } from '../api/dto';
import { plan, reset } from './plan';
import { session } from './session';
import { prepareQuick, quick, restoreQuick } from './quick';
vi.mock('../api/client', () => ({ api: { get: vi.fn(), post: vi.fn() }, errorMessage: (e: unknown) => String(e) }));
beforeEach(() => {
  reset(); quick.previous = null; vi.clearAllMocks();
  session.connection = null; session.local = { host: { hostname: 'm', os: 'Windows', interfaces: ['A','B','C'].map((name,i) => ({ name, ipv4:`192.168.0.${i+2}` } as NicInfo)) }, iperf3:'test', version:'test' };
  vi.mocked(api.post).mockResolvedValue({ units: [], plan_hash:'quick' });
});
it('快捷计划不丢原计划及本轮选项，恢复后预览必须作废', async () => {
  plan.duration = 45; plan.rounds = 3; plan.resume = true; plan.forceTcpWindow = '64k';
  const original = JSON.stringify(plan.ui);
  await prepareQuick('A','B','tcp',20,'v4');
  expect(plan.ui.link_sets[0].pair_refs).toHaveLength(1); expect(plan.rounds).toBe(1);
  restoreQuick();
  expect(JSON.stringify(plan.ui)).toBe(original); expect(plan.duration).toBe(45); expect(plan.rounds).toBe(3);
  expect(plan.resume).toBe(true); expect(plan.forceTcpWindow).toBe('64k'); expect(plan.preview).toBeNull();
});
it('六秒快捷时长只覆盖任务，保留原计划默认时长和基线校验窗口', async () => {
  plan.duration = 45;
  await prepareQuick('A','B','tcp',6,'v4');
  expect(plan.duration).toBe(45);
  expect(plan.ui.suites[0].tasks[0].duration).toBe(6);
  const request = vi.mocked(api.post).mock.calls.find(([path]) => path === '/api/plan')?.[1] as { duration: number; ui_plan: typeof plan.ui };
  expect(request.duration).toBe(45);
  expect(request.ui_plan.suites[0].tasks[0].duration).toBe(6);
});
