import type { NicInfo } from '../api/dto';
import { buildCandidates } from './pairs';
import type { UiPlan, UiProtocol } from './plan-build';
/** 明确的一对网口、一个任务；不带其它集合或整组绑定。 */
export function quickTestPlan(nics: NicInfo[], first: string, second: string, protocol: UiProtocol, duration: number, ip: 'v4' | 'v6'): UiPlan {
  if (first === second) throw new Error('请选择两个不同网口');
  const pair = buildCandidates(nics, []).find(p => p.srcNic.name === first && p.dstNic.name === second || p.srcNic.name === second && p.dstNic.name === first);
  if (!pair) throw new Error('所选网口已不在本机扫描结果中，请重新扫描');
  if (!Number.isInteger(duration) || duration < 6 || duration > 86400) throw new Error('测试时长为 6–86400 秒');
  const src = pair.srcNic.name === first ? pair.src : pair.dst;
  const dst = pair.srcNic.name === first ? pair.dst : pair.src;
  const pairId = `pair-${src}->${dst}`;
  return { ui_plan_version: 1,
    link_sets: [{ id: 'quick-local', name: '本机快捷测试', pair_refs: [{ id: pairId, src, dst }] }],
    recipes: { tcp: [{ id: 'quick-tcp', name: '快捷 TCP', profiles: [], tcp_windows: [], tcp_streams: [1] }],
      udp: [{ id: 'quick-udp', name: '快捷 UDP', profiles: [], bandwidths: ['2500m'], lengths: ['1400'], windows: [], udp_streams: [1] }], ping: [] },
    suites: [{ id: 'quick-suite', name: '快捷测试', note: '', execution: 'sequential', order: ['quick-task'], tasks: [{ id: 'quick-task', name: protocol.toUpperCase(), protocol, directions: ['ab', 'ba'], ip: [ip], recipe_ids: protocol === 'ping' ? [] : [`quick-${protocol}`], duration, ping_count: duration, ping_payload_sizes: [32] }] }],
    bindings: [{ id: 'quick-binding', link_set_id: 'quick-local', suite_id: 'quick-suite', pair_ids: [pairId], mode: 'replace' }] };
}
