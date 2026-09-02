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

  // 这一条是冲着一个真实缺陷来的：示例项目发布时 `master_config` 是 `{}`，
  // 而空对象在后端等价于「没带」——这份号称「完整可复现快照」的旗舰示例，
  // 判定参数一个都没带，导入后全落到目标机器的基线。上面两条断言都是绿的，
  // 因为它们只看解析和往返。
  it('带着完整的判定基线，而不是一个空壳', () => {
    const result = parseProject(text);
    const master = result.settings?.masterConfig;
    expect(master, 'master_config 缺失就等于没带判定参数').toBeTruthy();
    expect(Object.keys(master!).sort()).toEqual(['ctstraffic', 'iperf', 'link_profiles', 'ping']);
    // 界面上没有输入框、却直接决定 PASS/FAIL 的那些参数必须在里面。
    const iperf = master!.iperf as Record<string, unknown>;
    expect(Object.keys(iperf.rate_check as object).length).toBeGreaterThan(10);
    expect((master!.link_profiles as Record<string, unknown>).by_role).toBeDefined();
    // 反过来：按网口的门限覆盖是本机身份，不许跟着项目走。
    expect((master!.link_profiles as Record<string, unknown>).by_nic).toBeUndefined();
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
