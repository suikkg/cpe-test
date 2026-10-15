import { reactive } from 'vue';
import { invalidatePreview, plan, preview } from './plan';
import { masterNics } from './inventory';
import { goto, ui } from './ui';
import { quickTestPlan } from '../domain/quick-test';
import type { UiProtocol } from '../domain/plan-build';
export const quick = reactive({ previous: null as string | null });
export async function prepareQuick(first: string, second: string, protocol: UiProtocol, duration: number, ip: 'v4'|'v6'): Promise<void> {
  const next = quickTestPlan(masterNics.value, first, second, protocol, duration, ip);
  quick.previous ??= JSON.stringify(plan);
  invalidatePreview();
  plan.pendingImportTopology = false;
  plan.ui = next; plan.linkSets = next.link_sets.map(s => ({ ...s, auto: false }));
  const endpoints = new Set(next.link_sets.flatMap(s => s.pair_refs.flatMap(p => [p.src, p.dst])));
  plan.nicPolicies = plan.nicPolicies.filter(p => endpoints.has(p.endpoint));
  plan.resume = false; plan.rounds = 1; plan.forceTcpWindow = ''; plan.forceUdpBandwidth = '';
  plan.probeDuringTraffic = false; plan.probePathMtu = false; plan.limitUdpByLinkSpeed = false;
  // 快捷时长已写入任务；默认时长仍属于原计划，避免短测误改整份基线的采样/阶梯校验。
  ui.preparing = true; goto('run'); await preview();
}
export function restoreQuick(): void { if (quick.previous) { invalidatePreview(); Object.assign(plan, JSON.parse(quick.previous), { preview: null, previewError: '', previewing: false, previewRequestFingerprint: '' }); } quick.previous = null; }
