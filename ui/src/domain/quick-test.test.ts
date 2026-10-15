import { describe, expect, it } from 'vitest';
import { quickTestPlan } from './quick-test';
import type { NicInfo } from '../api/dto';
const nics = ['A','B','C'].map((name, i) => ({ name, ipv4: `192.168.0.${i + 2}` } as NicInfo));
describe('本机两口快捷测试', () => {
  it('只绑定所选一对，第三个口与辅测机均不参与', () => {
    const plan = quickTestPlan(nics, 'A', 'B', 'tcp', 20, 'v4');
    expect(plan.link_sets).toHaveLength(1); expect(plan.link_sets[0].pair_refs).toHaveLength(1);
    expect(plan.link_sets[0].pair_refs[0]).toMatchObject({ src: 'master:NAME=A', dst: 'master:NAME=B' });
    expect(plan.bindings[0].pair_ids).toEqual([plan.link_sets[0].pair_refs[0].id]);
    expect(plan.suites[0].tasks).toHaveLength(1);
    expect(plan.suites[0].tasks[0].directions).toEqual(['ab','ba']);
  });
  it('A/B 顺序跟随用户选择，不被扫描顺序翻转', () => {
    expect(quickTestPlan(nics, 'B', 'A', 'tcp', 20, 'v4').link_sets[0].pair_refs[0]).toMatchObject({ src: 'master:NAME=B', dst: 'master:NAME=A' });
  });
  it('相同网口、扫描失效与无效时长不能生成计划', () => {
    expect(() => quickTestPlan(nics, 'A', 'A', 'ping', 20, 'v4')).toThrow('不同');
    expect(() => quickTestPlan(nics, 'A', 'gone', 'tcp', 20, 'v4')).toThrow('重新扫描');
    expect(() => quickTestPlan(nics, 'A', 'B', 'udp', 0, 'v4')).toThrow('时长');
  });
});
