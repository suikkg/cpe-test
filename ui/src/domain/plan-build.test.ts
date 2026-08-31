import { describe, expect, it } from 'vitest';
import {
  baselineSuite,
  canonicalDirection,
  defaultTcpRecipe,
  defaultUdpRecipe,
  deleteRecipe,
  directionLabel,
  emptyPlan,
  ensureDefaults,
  isBound,
  toggleBinding,
  toggleSuiteColumn,
  type UiPlan,
} from './plan-build';

function planWithSets(count: number): UiPlan {
  const plan = ensureDefaults(emptyPlan());
  plan.link_sets = Array.from({ length: count }, (_, i) => ({
    id: `set-${i}`,
    name: `集合 ${i}`,
    pair_refs: [{ id: `pair-${i}`, src: 'master:NAME=a', dst: 'agent:NAME=b' }],
  }));
  return plan;
}

/**
 * 承接 `the_recipe_card_can_be_deleted_and_edited_in_place`（PLAN §7.3）。
 */
describe('参数配置的删除', () => {
  it('删配置时清掉所有任务上的引用', () => {
    // 原始理由（逐字搬运）：配置卡片过去只有「编辑」，点击委派里根本没有删除
    // 分支——加错一条配置就再也去不掉，只能重建整个项目。
    const plan = ensureDefaults(emptyPlan());
    expect(plan.suites[0].tasks[0].recipe_ids).toContain('recipe-tcp-default');

    const after = deleteRecipe(plan, 'tcp', 'recipe-tcp-default');
    expect(after.recipes.tcp).toHaveLength(0);
    // 只删卡片不清引用会更糟：任务指着一个不存在的 recipe_id，预览时才报错，
    // 而报错指向的是任务不是配置。
    for (const suite of after.suites) {
      for (const task of suite.tasks) {
        expect(task.recipe_ids).not.toContain('recipe-tcp-default');
      }
    }
    // 不该误伤别的协议。
    expect(after.recipes.udp.map((r) => r.id)).toContain('recipe-udp-default');
    expect(after.suites[0].tasks[1].recipe_ids).toContain('recipe-udp-default');
  });

  it('删不存在的配置是空操作，不抛', () => {
    const plan = ensureDefaults(emptyPlan());
    expect(deleteRecipe(plan, 'tcp', '不存在').recipes.tcp).toHaveLength(1);
  });
});

/**
 * 承接 `the_assignment_table_can_toggle_a_whole_suite_column`（PLAN §7.3）。
 */
describe('分配表整列开关', () => {
  it('一次把套件分配给全部链路集合，再点一次全撤', () => {
    // 原始理由（逐字搬运）：套件多起来之后，「所有链路都跑这个套件」原本
    // 得逐格点。
    let plan = planWithSets(3);
    const suiteId = plan.suites[0].id;

    plan = toggleSuiteColumn(plan, suiteId);
    expect(plan.bindings).toHaveLength(3);
    expect(plan.link_sets.every((s) => isBound(plan, s.id, suiteId))).toBe(true);

    plan = toggleSuiteColumn(plan, suiteId);
    expect(plan.bindings).toHaveLength(0);
  });

  it('半勾状态下整列开关是「全勾上」而不是「全撤掉」', () => {
    // 三态归一：只要还有没绑的就全绑上。这样连点两下的结果可预测——
    // 不会出现「点一下勾了一半」这种让人再点一下试试的状态。
    let plan = planWithSets(3);
    const suiteId = plan.suites[0].id;
    plan = toggleBinding(plan, 'set-1', suiteId);
    expect(plan.bindings).toHaveLength(1);

    plan = toggleSuiteColumn(plan, suiteId);
    expect(plan.bindings).toHaveLength(3);
    // 已经绑上的那个不该被重复添加。
    expect(new Set(plan.bindings.map((b) => b.link_set_id)).size).toBe(3);
  });

  it('单格开关与整列开关操作同一份 bindings', () => {
    let plan = planWithSets(2);
    const suiteId = plan.suites[0].id;
    plan = toggleSuiteColumn(plan, suiteId);
    plan = toggleBinding(plan, 'set-0', suiteId);
    expect(isBound(plan, 'set-0', suiteId)).toBe(false);
    expect(isBound(plan, 'set-1', suiteId)).toBe(true);
  });

  it('绑定只用 replace 模式', () => {
    // `append` 没有定义好的合并语义，服务端会在预览前明确拒绝，
    // 不会静默当成替换。前端就不该造出这种绑定。
    const plan = toggleSuiteColumn(planWithSets(2), 'suite-baseline');
    expect(plan.bindings.every((b) => b.mode === 'replace')).toBe(true);
  });
});

describe('方向词汇表', () => {
  it('both 与 bidir 是两件不同的事', () => {
    // both = 两条独立的单向腿；bidir = 同一个双向并发单元。
    // 半双工介质上双向并发时两个方向抢同一段介质时间，跑出来的数完全不是
    // 一回事——把 both 显示成「双向」再保存回 bidir 会**改变执行语义**。
    expect(canonicalDirection('both')).toBe('both');
    expect(canonicalDirection('bidir')).toBe('bidir');
    expect(directionLabel('both')).toBe('A→B、B→A（分开执行）');
    expect(directionLabel('bidir')).toBe('双向并发');
  });

  it('认得旧项目文件里的各种拼法', () => {
    for (const raw of ['ab', 'A->B', 'a>b', 'A_TO_B']) {
      expect(canonicalDirection(raw), raw).toBe('ab');
    }
    for (const raw of ['bidir', 'A<->B', '双向', 'both-way']) {
      expect(canonicalDirection(raw), raw).toBe('bidir');
    }
    expect(canonicalDirection('斜着跑')).toBeNull();
  });
});

/**
 * 承接 `the_udp_datagram_size_is_configured_only_in_the_suite` 的**前半段**
 * （PLAN §7.3）。后半段是纯 DTO 逻辑，仍留在 Rust 侧。
 */
describe('出厂默认', () => {
  it('基线 UDP 档位逐字保持：-b 2500m · -l 14k · -w 256m · 单流', () => {
    // 这组值是按 Windows 调的。`-w 256m` 在 Linux/macOS 上必然报错
    // （被 kern.ipc.maxsockbuf / net.core.wmem_max 夹住），那是**预期行为**——
    // 调小它等于在没人注意的情况下削弱 Windows 上的基线。
    const udp = defaultUdpRecipe();
    expect(udp.bandwidths).toEqual(['2500m']);
    expect(udp.lengths).toEqual(['14k']);
    expect(udp.windows).toEqual(['256m']);
    expect(udp.udp_streams).toEqual([1]);
  });

  it('基线 TCP 档位逐字保持：-w 4m · 10 流', () => {
    expect(defaultTcpRecipe()).toMatchObject({
      profiles: [],
      tcp_windows: ['4m'],
      tcp_streams: [10],
    });
  });

  it('基线套件是 TCP + UDP 两个任务，双向分开执行', () => {
    const suite = baselineSuite();
    expect(suite.name).toBe('基线 TCP+UDP');
    expect(suite.execution).toBe('sequential');
    expect(suite.order).toEqual(['task-tcp', 'task-udp']);
    expect(suite.tasks.map((t) => t.protocol)).toEqual(['tcp', 'udp']);
    for (const task of suite.tasks) {
      expect(task.directions).toEqual(['ab', 'ba']);
      expect(task.ip).toEqual(['v4', 'v6']);
    }
  });

  it('`-l` 只有一个来源：套件里的参数配置', () => {
    // 承接原测试的核心断言。全局档位**不许**反向覆写套件的默认配置——否则
    // 「基线 TCP+UDP」会一直显示出厂 udp_profiles 里那条 `-l 64`，而界面上
    // 没有任何地方交代是谁改的。
    //
    // 结构上保证：`-l` 只出现在 recipe 的 lengths 里，UiTask 上根本没有这个
    // 字段可填，也就无从被别处写回。
    const udp = defaultUdpRecipe();
    expect(udp.lengths).toEqual(['14k']);
    const suite = baselineSuite();
    for (const task of suite.tasks) {
      expect(Object.keys(task)).not.toContain('udp_length');
      expect(Object.keys(task)).not.toContain('lengths');
    }
  });

  it('不写废弃的 mode 字段', () => {
    // mode 是死字段：校验器过去只准 fixed/scan，而计划编译器从头到尾不读它，
    // 两个取值产出同一份计划。服务端现在会明确拒绝非空 mode（ADR-16）。
    expect(defaultTcpRecipe().mode).toBeUndefined();
    expect(defaultUdpRecipe().mode).toBeUndefined();
  });
});

describe('ensureDefaults', () => {
  it('只在空的时候补，不覆盖已有的', () => {
    const plan = emptyPlan();
    plan.recipes.tcp.push({ id: 'mine', name: '我的', profiles: [{ window: '64k' }] });
    const after = ensureDefaults(plan);
    expect(after.recipes.tcp).toHaveLength(1);
    expect(after.recipes.tcp[0].id).toBe('mine');
    // UDP 那边是空的，仍然补默认。
    expect(after.recipes.udp[0].id).toBe('recipe-udp-default');
  });

  it('不改动传入的对象', () => {
    // 意图模型会被多个视图共享；就地改会让另一个视图在不知情的情况下看到变化。
    const plan = emptyPlan();
    ensureDefaults(plan);
    expect(plan.suites).toHaveLength(0);
  });
});
