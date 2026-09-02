import { describe, expect, it } from 'vitest';
import bootstrapFixture from './__fixtures__/bootstrap_out.json';
import planFixture from './__fixtures__/plan_out.json';
import progressFixture from './__fixtures__/progress_out.json';
import runStatusFixture from './__fixtures__/run_status.json';
import type { BootstrapOut, PlanOut, ProgressOut, RunStatus } from './dto';

/**
 * DTO 契约测试的**前端半边**。
 *
 * 固定样例由 Rust 侧的 `dto_fixtures_are_regenerated_for_the_frontend_contract_test`
 * 用**真实的 serde 序列化**产出，所以它们就是服务端真会发出来的形状。
 *
 * 这里挡的是两个方向的漂移：
 *  - Rust 改了字段名 → 重跑 `cargo test` 后固定样例变了 → 这里的键断言红；
 *  - `dto.ts` 里凭空多出/少掉字段 → 下面的「键集合完全相等」断言红。
 *
 * 手写 DTO 的代价就是这条测试。换成代码生成要给 13 个端点养一条构建链，
 * 还要求每个贡献者装那套工具——这笔账不划算（PLAN §5）。
 */

/** 把 fixture 的键集合和一份「期望键」逐字比对，多一个少一个都报出来。 */
function expectExactKeys(actual: object, expected: string[], what: string): void {
  const got = Object.keys(actual).sort();
  const want = [...expected].sort();
  expect(got, `${what} 的字段集合与 dto.ts 不一致`).toEqual(want);
}

describe('RunStatus 契约', () => {
  const run = runStatusFixture as unknown as RunStatus;

  it('字段集合与 dto.ts 一致', () => {
    expectExactKeys(
      run,
      [
        'run_id',
        'plan_hash',
        'started_at',
        'total_units',
        'current',
        'done',
        'counts',
        'eta_secs',
        'aborted_at_unit',
        'report',
        'finished',
      ],
      'RunStatus',
    );
  });

  it('UnitStatus 与 CurrentUnit 的字段集合一致', () => {
    expect(run.done.length).toBeGreaterThan(0);
    expectExactKeys(
      run.done[0],
      ['seq', 'title', 'verdict', 'reason_code', 'reason_detail', 'skipped', 'secs', 'link_group'],
      'UnitStatus',
    );
    expect(run.current).not.toBeNull();
    expectExactKeys(
      run.current!,
      ['seq', 'title', 'est_secs', 'started_at', 'link_group'],
      'CurrentUnit',
    );
  });

  it('计数器覆盖 Verdict 的全部六个取值', () => {
    // 进度页说 PASS 的那个单元，和报告里说 PASS 的必须是同一件事——
    // 所以这里不发明第二套状态词汇表，计数器就按 Verdict 的六值来。
    expectExactKeys(
      run.counts,
      ['pass', 'fail', 'measured', 'not_evaluated', 'setup_error', 'skip'],
      'RunCounts',
    );
  });

  it('判定用大写下划线的 label，不是驼峰变体名', () => {
    // `RATE_FAIL` 这个拼法已经在报告 HTML、task_results.json 和 rows.jsonl 里了。
    // 前端如果按 `RateFail` 匹配，整个失败清单会静默地一条都筛不出来。
    expect(run.done[0].verdict).toMatch(/^[A-Z_]+$/);
    expect(run.done[0].reason_code).toMatch(/^[A-Z_]*$/);
  });
});

describe('ProgressOut 契约', () => {
  const progress = progressFixture as unknown as ProgressOut;

  it('日志与结构化状态并列存在，各带各的游标', () => {
    expectExactKeys(
      progress,
      ['running', 'from', 'lines', 'report', 'run', 'units_from'],
      'ProgressOut',
    );
    // lines 给人看（文案可以随便改），run 给机器读。
    expect(Array.isArray(progress.lines)).toBe(true);
    expect(typeof progress.run.total_units).toBe('number');
    // 两个游标必须是分开的：日志按行走、单元按单元走，速度差三个数量级。
    expect(typeof progress.from).toBe('number');
    expect(typeof progress.units_from).toBe('number');
  });
});

describe('PlanOut 契约', () => {
  const plan = planFixture as unknown as PlanOut;

  it('包含复核树要用的 sections 与 trace', () => {
    // 旧页把后端算好的这两份层级/溯源数据 100% 丢弃，只读平铺的 units 再自己
    // 重拼分组。新前端必须直接渲染它们（DESIGN §7 第 3 条）。
    expect(plan.sections, 'sections 丢了，复核树就只能自己重拼分组').toBeDefined();
    expect(plan.trace, 'trace 丢了，就没法把单元溯源回套件任务').toBeDefined();
    expect(plan.units.length).toBeGreaterThan(0);
  });

  it('plan_hash 在场——它是复核页与实跑之间唯一的握手', () => {
    expect(plan.plan_hash).toBeTruthy();
  });

  it('每个单元都带最终下发参数', () => {
    // 「网口固定值 > 参数组 > 默认组」这条优先级与其让人背，不如把每条腿的
    // 最终数字摆出来：填错了当场看得见。
    for (const unit of plan.units) {
      expectExactKeys(
        unit,
        ['seq', 'title', 'est_secs', 'resumed', 'load', 'targets'],
        'PlannedUnit',
      );
    }
  });

  it('trace 的每一项都能溯源回套件任务', () => {
    for (const trace of plan.trace ?? []) {
      expect(trace.seq).toBeGreaterThan(0);
      expect(trace.link_set_id).not.toBeNull();
      expect(trace.suite_id).not.toBeNull();
      expect(trace.task_id).not.toBeNull();
    }
  });
});

describe('BootstrapOut 契约', () => {
  it('字段集合与 dto.ts 一致', () => {
    expectExactKeys(
      bootstrapFixture as unknown as BootstrapOut,
      [
        'agent_host',
        'agent_port',
        'token_configured',
        'ipv4_prefixes',
        'duration',
        'tcp_windows',
        'tcp_streams',
        'udp_bandwidths',
        'udp_lengths',
        'udp_windows',
        'udp_streams',
        'ping_count',
        'ping_payload_sizes',
        'ping_max_rtt_ms',
        'ping_small_max_bytes',
        'ping_medium_max_bytes',
        'ping_wired_small_avg_rtt_ms',
        'ping_wired_small_max_rtt_ms',
        'ping_wired_medium_avg_rtt_ms',
        'ping_wired_medium_max_rtt_ms',
        'ping_wired_large_avg_rtt_ms',
        'ping_wired_large_max_rtt_ms',
        'ping_wifi_small_avg_rtt_ms',
        'ping_wifi_small_max_rtt_ms',
        'ping_wifi_medium_avg_rtt_ms',
        'ping_wifi_medium_max_rtt_ms',
        'ping_wifi_large_avg_rtt_ms',
        'ping_wifi_large_max_rtt_ms',
        'rate_targets_mbps',
        'rate_mode',
        'udp_profiles',
        'screenshot',
        'ui_plan_supported',
      ],
      'BootstrapOut',
    );
  });
});
