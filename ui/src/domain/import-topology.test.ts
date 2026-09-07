import { describe, expect, it } from 'vitest';
import type { NicInfo } from '../api/dto';
import { emptyPlan, ensureDefaults } from './plan-build';
import { reconcileImportedTopology } from './import-topology';
import { reconcileLinkSets } from './grouping';
import { buildCandidates } from './pairs';

const master = [{name:'eth0', role:'SGMII1G'}] as NicInfo[];
const agent = [{name:'wifi0', role:'WIFI5G'}] as NicInfo[];
function fixture() {
  const plan = ensureDefaults(emptyPlan());
  plan.link_sets = [{id:'saved',name:'保存的链路',pair_refs:[
    {id:'valid',src:'agent:NAME=wifi0',dst:'master:NAME=eth0'},
    {id:'missing',src:'master:NAME=gone',dst:'agent:NAME=wifi0'},
  ]}];
  plan.bindings = [{id:'binding',link_set_id:'saved',suite_id:plan.suites[0].id,pair_ids:['missing'],mode:''}];
  return plan;
}

describe('导入后的拓扑校验', () => {
  it('已知缺失的网口对移除并提示，显式绑定子集不能扩大成全选', () => {
    const out = reconcileImportedTopology(fixture(),master,agent);
    expect(out.plan.link_sets[0].pair_refs.map(p=>p.id)).toEqual(['valid']);
    expect(out.plan.bindings).toEqual([]);
    expect(out.notices.join(' ')).toContain('master:NAME=gone');
    expect(out.pending).toBe(0);
  });
  it('两端均未知时原样保留并等待扫描', () => {
    const source = fixture();
    const out = reconcileImportedTopology(source,null,null);
    expect(out.plan).toEqual(source);
    expect(out.pending).toBe(2);
    expect(out.notices).toEqual([]);
  });
  it('成功扫描但零网卡时照常移除空集合及绑定', () => {
    const out = reconcileImportedTopology(fixture(),[],agent);
    expect(out.plan.link_sets).toEqual([]);
    expect(out.plan.bindings).toEqual([]);
    expect(out.notices.join(' ')).toContain('链路集合');
    expect(out.plan.suites).toEqual(fixture().suites);
  });
  it('有效的角色与大小写别名解析成当前端点，保持方向与 pair id', () => {
    const source = fixture();
    source.link_sets[0].pair_refs = [{id:'valid',src:'remote:WIFI5G',dst:'local:name=ETH0'}];
    source.bindings[0].pair_ids = ['valid'];
    const out = reconcileImportedTopology(source,master,agent);
    const reconciled = reconcileLinkSets(out.plan.link_sets.map(s=>({...s,auto:false})),buildCandidates(master,agent),'all',new Set(['saved']));
    expect(reconciled.linkSets[0].pair_refs).toEqual([{id:'valid',src:'agent:NAME=wifi0',dst:'master:NAME=eth0'}]);
    expect(out.plan.bindings[0].pair_ids).toEqual(['valid']);
    expect(reconciled.stale).toEqual([]);
  });
});
