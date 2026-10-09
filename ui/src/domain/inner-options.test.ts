import { describe, expect, it } from 'vitest';
import { defaultInnerConfig, emptyInnerThresholds, innerLink, normalizeInnerDraft, parseInnerParameterTokens, parseInnerProject, serializeInnerProject } from './inner';
import { innerSetupIssues } from './inner-setup';
const config = () => {
  const cfg = defaultInnerConfig();
  const link = innerLink();
  Object.assign(link, { local_interface: 'ETH', local_ip: '192.168.0.2' });
  cfg.links = [link];
  return cfg;
};
describe('内环多档位与分协议门限', () => {
  it('截图默认开启，旧项目兼容，关闭可导入导出且非法类型拒绝', () => {
    const cfg = config(); expect(cfg.screenshot).toBe(true);
    delete cfg.screenshot; expect(parseInnerProject(JSON.stringify(cfg)).screenshot).toBe(true);
    cfg.screenshot = false; expect(parseInnerProject(serializeInnerProject(cfg)).screenshot).toBe(false);
    expect(() => parseInnerProject(JSON.stringify({ ...cfg, screenshot: 'false' }))).toThrow('截图');
  });
  it('接受空格、中英文逗号并保留非法项供校验', () => {
    expect(parseInnerParameterTokens('1 2,4，8、16', true)).toEqual([1, 2, 4, 8, 16]);
    expect(parseInnerParameterTokens('64k 4m，16m', false)).toEqual(['64k', '4m', '16m']);
    expect(parseInnerParameterTokens('100 12oops', true)[1]).toBeNaN();
  });
  it('导入导出保留档位及每网口独立协议门限', () => {
    const cfg = config(); cfg.protocols = ['tcp', 'udp'];
    cfg.parameter_options!.tcp_streams = [1, 4]; cfg.parameter_options!.udp_rates_mbps = [100, 200.5];
    cfg.links[0].tcp_thresholds = { ...emptyInnerThresholds(), upload_min_mbps: 800 };
    cfg.links[0].udp_thresholds = { ...emptyInnerThresholds(), upload_min_mbps: 90, tool_upload_min_mbps: 80 };
    expect(innerSetupIssues(cfg)).toEqual([]);
    expect(parseInnerProject(serializeInnerProject(cfg))).toEqual(cfg);
  });
  it.each([['tcp_streams', [1, 1]], ['tcp_streams', [1.5]], ['tcp_windows', ['bad']], ['udp_rates_mbps', [0]], ['udp_rates_mbps', [null]], ['udp_lengths', ['1400;reboot']]])('拒绝非法档位 %s', (key, values) => {
    const cfg = config(); cfg.protocols = ['tcp', 'udp']; cfg.udp_mbps = 100;
    Object.assign(cfg.parameter_options!, { [key]: values });
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow();
  });
  it('过大的叉乘计划在前端拒绝', () => {
    const cfg = config(); cfg.protocols = ['udp']; cfg.repeats = 2;
    const values = Array.from({ length: 16 }, (_, i) => i + 1);
    Object.assign(cfg.parameter_options!, { udp_streams: values, udp_rates_mbps: values, udp_lengths: values.map(String) });
    expect(() => parseInnerProject(JSON.stringify(cfg))).toThrow('4096');
  });
  it('切换协议、策略与方向时清除不适用参数', () => {
    const cfg = config(); cfg.protocols = ['tcp']; cfg.links[0].measurement = 'nic_strict';
    cfg.parameter_options!.udp_rates_mbps = [100];
    cfg.links[0].tcp_thresholds = { ...emptyInnerThresholds(), upload_min_mbps: 800, tool_upload_min_mbps: 700, bidir_total_min_mbps: 1500 };
    const normalized = normalizeInnerDraft(cfg);
    expect(normalized.parameter_options!.udp_rates_mbps).toEqual([]);
    expect(normalized.links[0].tcp_thresholds).toEqual({ ...emptyInnerThresholds(), upload_min_mbps: 800 });
    expect(() => parseInnerProject(JSON.stringify(normalized))).not.toThrow();
  });
});
