import { FLOW_LABEL, emptyInnerThresholds } from './inner';
import type { InnerConfig, InnerThresholds, InnerUnit } from './inner';

export const INNER_VERDICT_LABELS: Record<string, string> = {
  PASS: '达标', RATE_FAIL: '未达标', MEASURED: '仅测量', NOT_EVALUATED: '无法评价', SETUP_ERROR: '执行失败', SKIP: '跳过',
};
export const innerVerdictLabel = (verdict: string): string => INNER_VERDICT_LABELS[verdict] ?? '未知状态';
export function innerRecordedCounts(units: InnerUnit[]) {
  const count = (verdict: string) => units.filter(u => !u.resumed && u.verdict === verdict).length;
  return { total: units.length, pass: count('PASS'), fail: count('RATE_FAIL'), measured: count('MEASURED'), unknown: count('NOT_EVALUATED'), error: count('SETUP_ERROR'), resumed: units.filter(u => u.resumed).length };
}
/** 只读已有结果；配置仅用于解释无有效合计时实际要求的门限。 */
export function innerEffectiveThresholds(config: InnerConfig, unit: InnerUnit): InnerThresholds {
  const link = config.links.find(l => l.name === unit.link && l.host === unit.host);
  const result = emptyInnerThresholds();
  if (!link) return result;
  const overrides = unit.protocol === 'tcp' ? link.tcp_thresholds : link.udp_thresholds;
  for (const key of Object.keys(result) as (keyof InnerThresholds)[]) result[key] = overrides?.[key] ?? link[key];
  return result;
}
export function innerUsesTotal(config: InnerConfig, unit: InnerUnit): boolean {
  if (unit.direction !== 'bidir') return false;
  if (unit.bidir_targets !== undefined) return unit.bidir_targets !== null;
  const t = innerEffectiveThresholds(config, unit);
  return unit.total_target_mbps != null || t.bidir_total_min_mbps != null || (unit.measurement !== 'nic_strict' && t.tool_bidir_total_min_mbps != null);
}
export function innerUnitExplanation(unit: InnerUnit): string {
  if (unit.resumed) return '复用 24 小时内的 PASS，本轮未重新测试。';
  const explanation: Record<string, string> = {
    PASS: '接收速率达到门限。', RATE_FAIL: '接收速率低于门限。',
    MEASURED: '未设门限，仅记录速率。',
    NOT_EVALUATED: '无有效验收结果。', SETUP_ERROR: '起流或执行环境失败。',
  };
  return `${explanation[unit.verdict] ?? '请查看单元判定原因。'} ${unit.detail}`.trim();
}
export function innerTargetText(target: number | null | undefined): string {
  return target == null ? '未设门限' : `${target.toFixed(2)} Mbps`;
}
export const innerRateText = (value: number | null | undefined): string => value == null ? '未获取' : value.toFixed(2);
export function innerTotalTargetText(config: InnerConfig, unit: InnerUnit): string {
  if (unit.total_target_mbps != null) return innerTargetText(unit.total_target_mbps);
  const t = unit.bidir_targets ? { bidir_total_min_mbps: unit.bidir_targets.nic_mbps, tool_bidir_total_min_mbps: unit.bidir_targets.tool_mbps } : innerEffectiveThresholds(config, unit);
  return `未能应用（配置网卡合计 ${innerTargetText(t.bidir_total_min_mbps)}；工具合计 ${innerTargetText(t.tool_bidir_total_min_mbps)}）`;
}
export const innerFlowPath = (flow: keyof typeof FLOW_LABEL): string => flow === 'up' ? '电脑 → CPE' : 'CPE → 电脑';
