import { describe, expect, it } from 'vitest';
import type { InnerUnit } from './inner';
import { defaultInnerConfig, innerLink } from './inner';
import { innerRecordedCounts, innerTargetText, innerRateText, innerUsesTotal, innerTotalTargetText, innerUnitExplanation } from './inner-report';
const unit = (verdict: string, resumed = false): InnerUnit => ({ index: 1, link: 'ETH', host: 'master', protocol: 'tcp', direction: 'bidir', streams: 1, repeat: 1, measurement: 'nic_preferred', verdict, resumed, reason: '', detail: '', diagnostics: [], total_mbps: null, total_target_mbps: null, overlap_secs: null, legs: [] });
describe('内环报告只解释已有结果', () => {
  it('按单元统计，复用历史不冒充本轮达标', () => {
    const units = [unit('PASS'), unit('PASS', true), unit('RATE_FAIL'), unit('MEASURED'), unit('NOT_EVALUATED'), unit('SETUP_ERROR')];
    expect(innerRecordedCounts(units)).toEqual({ total: 6, pass: 1, fail: 1, measured: 1, unknown: 1, error: 1, resumed: 1 });
  });
  it('区分未设门限、未取得测量和真实零值', () => {
    expect(innerTargetText(null)).toBe('未设门限'); expect(innerRateText(null)).toBe('未获取'); expect(innerRateText(0)).toBe('0.00');
  });
  it('双向验收方式及配置门限读本次结果快照，不随编辑变化', () => {
    const cfg = defaultInnerConfig(); const link = innerLink('master', undefined, 'ETH'); cfg.links = [link];
    const row = unit('NOT_EVALUATED'); row.bidir_targets = { nic_mbps: 1800, tool_mbps: 1700 };
    expect(innerUsesTotal(cfg, row)).toBe(true); expect(innerTotalTargetText(cfg, row)).toContain('1800.00 Mbps');
    link.bidir_total_min_mbps = 5; expect(innerTotalTargetText(cfg, row)).toContain('1800.00 Mbps');
    row.bidir_targets = null; expect(innerUsesTotal(cfg, row)).toBe(false);
  });
  it('兼容缺少快照的旧记录；实际合计门限优先', () => {
    const cfg = defaultInnerConfig(); const row = unit('PASS'); row.total_target_mbps = 900;
    expect(innerUsesTotal(cfg, row)).toBe(true); expect(innerTotalTargetText(cfg, row)).toBe('900.00 Mbps');
  });
  it('解释沿用已有判定，历史复用不表示本轮实测', () => {
    const row = unit('RATE_FAIL'); row.total_mbps = 9999; expect(innerUnitExplanation(row)).toContain('低于');
    row.resumed = true; expect(innerUnitExplanation(row)).toContain('本轮未重新测试');
  });
});
