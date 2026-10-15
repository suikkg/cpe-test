import { emptyInnerParameterOptions, type InnerConfig, type InnerProtocol, type InnerParameterOptions, type InnerDirection, type InnerIpVersion, type InnerThresholds } from './inner';
export interface InnerGroup { id: string; name: string; links: string[]; all_links?: boolean }
export interface InnerRecipe { id: string; name: string; protocol: InnerProtocol; parameters: InnerParameterOptions }
export interface InnerTask { id: string; name: string; protocol: InnerProtocol; recipe: string | null; ip_versions: InnerIpVersion[]; directions: InnerDirection[]; duration_secs: number | null; repeats?: number | null; thresholds: InnerThresholds | null; ping_options: { count: number; payloads: number[] } | null; max_udp_loss_pct?: number | null }
export interface InnerSuite { id: string; name: string; tasks: InnerTask[] }
export interface InnerBinding { group: string; suite: string; links: string[] }
export interface InnerWorkspace { groups: InnerGroup[]; recipes: InnerRecipe[]; suites: InnerSuite[]; bindings: InnerBinding[] }
export interface InnerOrigin { group: string; suite: string; task: string }
export function nextInnerId(items: { id: string }[], prefix: string): string {
  let n = 1; while (items.some(i => i.id === `${prefix}${n}`)) n++; return `${prefix}${n}`;
}
export function initialInnerWorkspace(cfg: InnerConfig): InnerWorkspace {
  const recipes: InnerRecipe[] = cfg.protocols.filter(p => p !== 'ping').map(p => ({ id: p, name: p.toUpperCase(), protocol: p,
    parameters: p === 'tcp' ? { ...emptyInnerParameterOptions(), tcp_streams: cfg.parameter_options?.tcp_streams.length ? [...cfg.parameter_options.tcp_streams] : [cfg.tcp_streams ?? cfg.parallel], tcp_windows: cfg.parameter_options?.tcp_windows.length ? [...cfg.parameter_options.tcp_windows] : cfg.tcp_window ? [cfg.tcp_window] : [] }
      : { ...emptyInnerParameterOptions(), udp_streams: cfg.parameter_options?.udp_streams.length ? [...cfg.parameter_options.udp_streams] : [cfg.udp_streams ?? cfg.parallel], udp_rates_mbps: cfg.parameter_options?.udp_rates_mbps.length ? [...cfg.parameter_options.udp_rates_mbps] : cfg.udp_mbps != null ? [cfg.udp_mbps] : [], udp_windows: cfg.parameter_options?.udp_windows.length ? [...cfg.parameter_options.udp_windows] : cfg.udp_window ? [cfg.udp_window] : [], udp_lengths: cfg.parameter_options?.udp_lengths.length ? [...cfg.parameter_options.udp_lengths] : cfg.udp_length ? [cfg.udp_length] : [] } }));
  return { groups: [{ id: 'all', name: '全部网口', links: cfg.links.map(l => l.name), all_links: true }], recipes,
    suites: [{ id: 'baseline', name: '基线测试', tasks: cfg.protocols.map(p => ({ id: p, name: p.toUpperCase(), protocol: p, recipe: p === 'ping' ? null : p, ip_versions: [...cfg.ip_versions], directions: [...cfg.directions], duration_secs: null, repeats: cfg.repeats, thresholds: null, ping_options: null, max_udp_loss_pct: p === 'udp' ? cfg.max_udp_loss_pct : null })) }],
    bindings: [{ group: 'all', suite: 'baseline', links: [] }] };
}
export function innerAssignedLinks(cfg: InnerConfig): string[] {
  if (!cfg.workspace) return cfg.links.filter(l => l.enabled).map(l => l.name);
  const names = new Set<string>();
  for (const binding of cfg.workspace.bindings) {
    const group = cfg.workspace.groups.find(g => g.id === binding.group);
    const suite = cfg.workspace.suites.find(s => s.id === binding.suite);
    if (!group || !suite?.tasks.length) continue;
    for (const name of binding.links.length ? binding.links : innerGroupLinks(cfg, group)) {
      if (innerGroupLinks(cfg, group).includes(name) && cfg.links.some(l => l.name === name && l.enabled)) names.add(name);
    }
  }
  return [...names];
}
export function setInnerAssigned(w: InnerWorkspace, group: string, suite: string, name: string | null, selected: boolean): void {
  const index = w.bindings.findIndex(b => b.group === group && b.suite === suite);
  const existing = w.bindings[index];
  if (name === null) {
    if (index >= 0) w.bindings.splice(index, 1);
    if (selected) w.bindings.push({ group, suite, links: [] });
    return;
  }
  const members = w.groups.find(g => g.id === group)?.links ?? [];
  const current = existing ? existing.links.length ? [...existing.links] : [...members] : [];
  if (current.includes(name) === selected) return;
  const next = selected ? [...current, name] : current.filter(n => n !== name);
  if (index >= 0) w.bindings.splice(index, 1);
  if (next.length) w.bindings.push({ group, suite, links: next });
}

export function renameInnerLink(cfg: InnerConfig, oldName: string, name: string): void {
  const link = cfg.links.find(l => l.name === oldName); if (!link) return;
  link.name = name;
  if (!cfg.workspace) return;
  for (const group of cfg.workspace.groups) group.links = group.links.map(n => n === oldName ? name : n);
  for (const binding of cfg.workspace.bindings) binding.links = binding.links.map(n => n === oldName ? name : n);
}
export function removeInnerLink(cfg: InnerConfig, name: string): void {
  cfg.links = cfg.links.filter(l => l.name !== name);
  if (!cfg.workspace) return;
  for (const group of cfg.workspace.groups) group.links = group.links.filter(n => n !== name);
  cfg.workspace.bindings = cfg.workspace.bindings.filter(binding => {
    if (!binding.links.length) return true;
    binding.links = binding.links.filter(n => n !== name);
    return binding.links.length > 0;
  });
}

export function innerGroupLinks(cfg: InnerConfig, group: InnerGroup): string[] { return group.all_links ? cfg.links.map(l => l.name) : group.links; }
