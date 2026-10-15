import { describe, expect, it } from 'vitest';
import { defaultInnerConfig, innerLink, parseInnerProject, serializeInnerProject, normalizeInnerDraft } from './inner';
import { innerSetupIssues } from './inner-setup';
import { initialInnerWorkspace, innerAssignedLinks, setInnerAssigned, renameInnerLink, removeInnerLink } from './inner-workspace';
function config() {
  const cfg = defaultInnerConfig();
  cfg.links = [{ ...innerLink(), name: 'LAN-A', local_interface: 'LAN-A', local_ip: '192.168.0.2', gateway: '192.168.0.1' },
    { ...innerLink(), name: 'LAN-B', host: 'agent1', local_interface: 'LAN-B', local_ip: '192.168.0.3', gateway: '192.168.0.1' }];
  cfg.agents = [{ id: 'agent1', address: '192.168.0.200', port: 28801, token: 'secret' }];
  cfg.workspace = initialInnerWorkspace(cfg); cfg.workspace.bindings[0].links = ['LAN-A'];
  return cfg;
}
describe('内环套件计划与分配', () => {
  it('套件缺项定位到实际网口和标签，不把修复按钮留在参数页', () => {
    const cfg = config(); cfg.workspace!.suites[0].tasks[0].ip_versions = [6];
    const issue = innerSetupIssues(cfg)[0];
    expect(issue).toMatchObject({ target:'links', linkIndex:0, workspaceTab:'ports' });
  });
  it('旧统一参数转成套件时保留单项重复，不悄悄减少计划范围', () => {
    const cfg = defaultInnerConfig(); cfg.repeats = 3;
    const workspace = initialInnerWorkspace(cfg);
    expect(workspace.suites[0].tasks[0].repeats).toBe(3);
  });

  it('本机任务不要求未分配的辅测机补 IPv6；往返导出保持任务与轮次且不带口令', () => {
    const cfg = config(); cfg.ip_versions = [4,6]; cfg.rounds = 3;
    const text = serializeInnerProject(cfg);
    expect(text).not.toContain('secret');
    const restored = parseInnerProject(text);
    expect(restored.rounds).toBe(3); expect(restored.workspace?.bindings[0].links).toEqual(['LAN-A']);
    expect(innerAssignedLinks(restored)).toEqual(['LAN-A']);
  });
  it('取消最后一个显式网口删除分配，不能变成整组测试', () => {
    const cfg = config(); const w = cfg.workspace!;
    setInnerAssigned(w, 'all', 'baseline', 'LAN-A', false);
    expect(w.bindings).toEqual([]); expect(innerAssignedLinks(cfg)).toEqual([]);
    setInnerAssigned(w, 'all', 'baseline', null, true);
    expect(w.bindings[0].links).toEqual([]);
    setInnerAssigned(w, 'all', 'baseline', 'LAN-A', true);
    expect(w.bindings[0].links).toEqual([]);
  });
  it('网口改名同步集合与分配；删除最后一个选口不会扩大执行范围', () => {
    const cfg = config(); renameInnerLink(cfg, 'LAN-A', '测试 LAN');
    expect(innerAssignedLinks(cfg)).toEqual(['测试 LAN']);
    expect(cfg.workspace!.groups[0].links).toContain('测试 LAN');
    removeInnerLink(cfg, '测试 LAN');
    expect(innerAssignedLinks(cfg)).toEqual([]); expect(cfg.workspace!.bindings).toEqual([]);
  });
  it('错误任务引用及空 IP 版本阻止导入且不修改原配置', () => {
    const cfg = config(); const bad = structuredClone(cfg);
    bad.workspace!.suites[0].tasks[0].recipe = 'missing';
    expect(() => parseInnerProject(JSON.stringify(bad))).toThrow('协议不匹配');
    expect(cfg.workspace!.suites[0].tasks[0].recipe).toBe('tcp');
    bad.workspace!.suites[0].tasks[0].recipe = 'tcp'; bad.workspace!.suites[0].tasks[0].ip_versions = [];
    expect(() => parseInnerProject(JSON.stringify(bad))).toThrow('IP');
  });
  it('清空任务时长与门限正确恢复继承', () => {
    const cfg = config(); cfg.workspace!.suites[0].tasks[0].duration_secs = '' as unknown as number;
    expect(normalizeInnerDraft(cfg).workspace!.suites[0].tasks[0].duration_secs).toBeNull();
  });
});
