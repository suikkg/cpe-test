import { describe, expect, it } from 'vitest';
// 随包发布的示例项目就是 v3 形状的活样本：它一旦和解析器对不上，
// 用户在现场点「导入测试项目」才会发现。
import shipped from '../../../dist/projects/cpe-ui-project-full.json';
import { parseProject, PROJECT_VERSION, serializeProject } from './project';

const text = JSON.stringify(shipped);

describe('随包示例项目', () => {
  it('是当前版本，且能被解析器接受', () => {
    expect(shipped.project_version).toBe(PROJECT_VERSION);
    const result = parseProject(text);
    expect(result.error ?? '').toBe('');
    expect(result.ok).toBe(true);
    expect(result.plan!.suites.length).toBeGreaterThan(0);
    expect(result.plan!.link_sets[0].pair_refs).toHaveLength(10);
    expect(result.settings?.duration).toBe(180);
    expect(result.settings?.limit_udp_by_link_speed).toBe(true);
    expect(result.settings?.globals?.wifi_band_thresholds[0]).toMatchObject({
      master_band: 'wifi_5g',
      bidir_total_rx_target_mbps: 1000,
    });
    // v2 里这些格子是 0（「走主控默认值」）；v3 存的是有效值快照。
    expect(result.settings?.globals?.ping_wifi_large_max_rtt_ms).toBe(200);
    expect(result.settings?.globals?.ping_small_max_bytes).toBe(128);
  });

  it('再导出一次形状不变（幂等）', () => {
    const first = parseProject(text);
    const second = parseProject(
      serializeProject(first.plan!, first.settings, first.nicPolicies),
    );
    expect(second.ok).toBe(true);
    expect(second.settings?.globals).toEqual(first.settings?.globals);
    expect(second.plan).toEqual(first.plan);
  });
});
