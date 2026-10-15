import { describe, it, expect } from 'vitest';
import { defaultInnerPingPolicy, validateInnerPingPolicy } from './inner-options';
import { defaultInnerConfig, parseInnerProject, normalizeInnerDraft } from './inner';
describe('内环公共门限与临时覆盖', () => {
  it('公共门限使用子网相同的有线和 Wi-Fi 包长分档', () => {
    const p = defaultInnerPingPolicy();
    expect(p.wired_small_avg_rtt_ms).toBe(10);
    expect(p.wired_small_max_rtt_ms).toBe(30);
    expect(p.wifi_large_max_rtt_ms).toBe(200);
    expect(() => validateInnerPingPolicy({ ...p, wired_small_avg_rtt_ms: 31 })).toThrow();
    expect(() => validateInnerPingPolicy({ ...p, small_max_bytes: p.medium_max_bytes })).toThrow();
  });
  it('部分门限导入补公共默认值，未知字段拒绝', () => {
    expect(parseInnerProject(JSON.stringify({ ...defaultInnerConfig(), ping_policy: { wired_small_avg_rtt_ms: 0.5 } })).ping_policy?.wired_small_max_rtt_ms).toBe(30);
    expect(() => parseInnerProject(JSON.stringify({ ...defaultInnerConfig(), ping_policy: { unknown: 1 } }))).toThrow();
  });
  it('临时覆盖不接受 shell 文本、负速率或未知字段，空输入恢复不覆盖', () => {
    const cfg = defaultInnerConfig();
    for (const run_overrides of [{tcp_window:'4m; echo',udp_mbps:null},{tcp_window:null,udp_mbps:-1},{unknown:true}]) {
      expect(() => parseInnerProject(JSON.stringify({ ...cfg, run_overrides }))).toThrow();
    }
    cfg.run_overrides = { tcp_window: '', udp_mbps: '' as unknown as number };
    expect(normalizeInnerDraft(cfg).run_overrides).toEqual({tcp_window:null,udp_mbps:null,probe_during_traffic:false,probe_path_mtu:false,limit_udp_by_link_speed:false});
  });
});
