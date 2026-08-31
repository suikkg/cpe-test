import { describe, expect, it } from 'vitest';
import {
  addRecipe,
  addSuite,
  addTask,
  axisExpansionIsExact,
  duplicateSuite,
  emptyPlan,
  ensureDefaults,
  moveTask,
  profilesToAxes,
  recipeIsAxisEditable,
  removeSuite,
  removeTask,
  setTaskProtocol,
  taskUsesBidir,
  toggleBinding,
  toggleTaskDirection,
  toggleTaskIp,
  toggleTaskRecipe,
  uniqueId,
  updateRecipe,
  updateSuite,
  updateTask,
  type UiPlan,
} from './plan-build';

/**
 * 套件 / 任务 / 配置的增删改。
 *
 * 这一层此前完全不存在：「测试计划」页只能看，改计划的唯一办法是在别处编好项目
 * 文件再导进来。这些操作里真正容易错的部分——删套件要清绑定、删配置要清引用、
 * 换协议要作废旧引用、id 不能重复、套件不能空——全是纯逻辑，钉在这里。
 */

function planWithSet(): UiPlan {
  const plan = ensureDefaults(emptyPlan());
  plan.link_sets = [
    { id: 'set-1', name: '集合', pair_refs: [{ id: 'p', src: 'master:NAME=a', dst: 'agent:NAME=b' }] },
  ];
  return plan;
}

describe('id 分配', () => {
  it('按序号找第一个空位，不用随机数也不用时间戳', () => {
    expect(uniqueId('task', [])).toBe('task-1');
    expect(uniqueId('task', ['task-1', 'task-3'])).toBe('task-2');
  });

  it('配置 id 在三个协议桶之间是同一个命名空间——服务端就是这么校验的', () => {
    let plan = ensureDefaults(emptyPlan());
    plan = addRecipe(plan, 'tcp');
    plan = addRecipe(plan, 'udp');
    const ids = [...plan.recipes.tcp, ...plan.recipes.udp].map((r) => r.id);
    expect(new Set(ids).size).toBe(ids.length);
  });
});

describe('套件', () => {
  it('新建的套件自带一条任务——服务端会把空套件整份拒掉', () => {
    const plan = addSuite(ensureDefaults(emptyPlan()));
    const added = plan.suites[plan.suites.length - 1];
    expect(added.tasks).toHaveLength(1);
    expect(added.order).toEqual(added.tasks.map((t) => t.id));
    expect(added.execution).toBe('sequential');
  });

  it('删套件同时清掉指向它的绑定，否则分配表会留下点不掉的勾', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    plan = toggleBinding(plan, 'set-1', suiteId);
    expect(plan.bindings).toHaveLength(1);
    plan = removeSuite(plan, suiteId);
    expect(plan.suites.find((s) => s.id === suiteId)).toBeUndefined();
    expect(plan.bindings).toHaveLength(0);
  });

  it('复制套件：任务另给 id，名字加副本，不带分配', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    plan = toggleBinding(plan, 'set-1', suiteId);
    plan = duplicateSuite(plan, suiteId);

    expect(plan.suites).toHaveLength(2);
    const [source, copy] = plan.suites;
    expect(copy.name).toBe(`${source.name} 副本`);
    expect(copy.id).not.toBe(source.id);
    const ids = plan.suites.flatMap((s) => s.tasks.map((t) => t.id));
    expect(new Set(ids).size).toBe(ids.length);
    // 分配不跟着复制：复制出来的这份正是要单独分配给某条链路的。
    expect(plan.bindings.filter((b) => b.suite_id === copy.id)).toHaveLength(0);
  });

  it('改名不动 id——id 进了绑定和溯源串', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    plan = updateSuite(plan, suiteId, { name: '改过的名字' });
    expect(plan.suites[0].id).toBe(suiteId);
    expect(plan.suites[0].name).toBe('改过的名字');
  });
});

describe('任务', () => {
  it('新增任务后 order 跟着走：套件里的顺序就是执行顺序', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    plan = addTask(plan, suiteId, 'ping');
    const suite = plan.suites[0];
    expect(suite.tasks).toHaveLength(3);
    expect(suite.order).toEqual(suite.tasks.map((t) => t.id));
  });

  it('最后一条任务删不掉——服务端会拒绝没有任务的套件', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    plan = removeTask(plan, suiteId, plan.suites[0].tasks[0].id);
    expect(plan.suites[0].tasks).toHaveLength(1);
    const before = plan.suites[0].tasks[0].id;
    plan = removeTask(plan, suiteId, before);
    expect(plan.suites[0].tasks).toHaveLength(1);
  });

  it('上下移动同时改 order', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    const [first, second] = plan.suites[0].tasks.map((t) => t.id);
    plan = moveTask(plan, suiteId, first, 1);
    expect(plan.suites[0].tasks.map((t) => t.id)).toEqual([second, first]);
    expect(plan.suites[0].order).toEqual([second, first]);
    // 越界不动
    plan = moveTask(plan, suiteId, second, -1);
    expect(plan.suites[0].tasks.map((t) => t.id)).toEqual([second, first]);
  });

  it('换协议必须作废旧配置引用：TCP 的 id 在 UDP 任务上是「引用了不存在的配置」', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    const taskId = plan.suites[0].tasks[0].id;
    expect(plan.suites[0].tasks[0].recipe_ids).toContain('recipe-tcp-default');
    plan = setTaskProtocol(plan, suiteId, taskId, 'udp');
    expect(plan.suites[0].tasks[0].protocol).toBe('udp');
    expect(plan.suites[0].tasks[0].recipe_ids).toEqual([]);
  });

  it('新任务默认同时覆盖 IPv4 和 IPv6', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    plan = addTask(plan, suiteId, 'udp');
    expect(plan.suites[0].tasks[plan.suites[0].tasks.length - 1].ip).toEqual(['v4', 'v6']);
  });

  it('PING 不使用吞吐双向门限，切成 PING 时清掉旧值', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    const taskId = plan.suites[0].tasks[0].id;
    plan = toggleTaskDirection(plan, suiteId, taskId, 'bidir');
    plan = updateTask(plan, suiteId, taskId, {
      rx_target_bidir_ab: '1800',
      rx_target_bidir_ba: '1700',
    });
    plan = setTaskProtocol(plan, suiteId, taskId, 'ping');
    expect(taskUsesBidir(plan.suites[0].tasks[0])).toBe(false);
    expect(plan.suites[0].tasks[0].rx_target_bidir_ab).toBe('');
    expect(plan.suites[0].tasks[0].rx_target_bidir_ba).toBe('');
  });

  it('取消双向并发时把双向门限一起清掉', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    const taskId = plan.suites[0].tasks[0].id;
    plan = toggleTaskDirection(plan, suiteId, taskId, 'bidir');
    expect(taskUsesBidir(plan.suites[0].tasks[0])).toBe(true);
    plan = updateTask(plan, suiteId, taskId, { rx_target_bidir_ab: '1800' });

    plan = toggleTaskDirection(plan, suiteId, taskId, 'bidir');
    // 服务端会拒绝「填了双向门限却没选双向」，而那两个输入框这时已经不在界面上了。
    expect(plan.suites[0].tasks[0].rx_target_bidir_ab).toBe('');
    expect(plan.suites[0].tasks[0].rx_target_bidir_ba).toBe('');
  });

  it('方向、IP、配置都是开关语义', () => {
    let plan = planWithSet();
    const suiteId = plan.suites[0].id;
    const taskId = plan.suites[0].tasks[0].id;
    plan = toggleTaskIp(plan, suiteId, taskId, 'v6');
    expect(plan.suites[0].tasks[0].ip).toEqual(['v4']);
    plan = toggleTaskIp(plan, suiteId, taskId, 'v4');
    expect(plan.suites[0].tasks[0].ip).toEqual([]);
    plan = toggleTaskIp(plan, suiteId, taskId, 'v6');
    expect(plan.suites[0].tasks[0].ip).toEqual(['v6']);

    plan = toggleTaskRecipe(plan, suiteId, taskId, 'recipe-tcp-default');
    expect(plan.suites[0].tasks[0].recipe_ids).toEqual([]);
    plan = toggleTaskRecipe(plan, suiteId, taskId, 'recipe-tcp-default');
    expect(plan.suites[0].tasks[0].recipe_ids).toEqual(['recipe-tcp-default']);
  });
});

describe('配置的档位编辑', () => {
  it('带固定组合的配置不给轴输入框——服务端有 profiles 时压根不看轴字段', () => {
    const plan = ensureDefaults(emptyPlan());
    // 出厂 TCP/UDP 默认打开即可编辑；固定组合只来自历史项目。
    expect(recipeIsAxisEditable(plan.recipes.tcp[0])).toBe(true);
    expect(recipeIsAxisEditable(plan.recipes.udp[0])).toBe(true);
  });

  it('一条固定组合摊成轴是无损的', () => {
    const plan = ensureDefaults(emptyPlan());
    const recipe = { ...plan.recipes.tcp[0], profiles: [{ window: '4m', streams: 10 }] };
    expect(axisExpansionIsExact(recipe)).toBe(true);
    const axes = profilesToAxes(recipe, 'tcp');
    expect(axes.profiles).toEqual([]);
    expect(axes.tcp_windows).toEqual(['4m']);
    expect(axes.tcp_streams).toEqual([10]);
    expect(recipeIsAxisEditable(axes)).toBe(true);
  });

  it('多条固定组合摊成轴会变叉积——必须先说清楚', () => {
    const recipe = {
      id: 'r',
      name: 'r',
      profiles: [
        { bandwidth: '2500m', length: '14k' },
        { bandwidth: '1000m', length: '1k' },
      ],
    };
    expect(axisExpansionIsExact(recipe)).toBe(false);
    const axes = profilesToAxes(recipe, 'udp');
    // 2 条显式组合 → 2×2 的叉积轴。数量变了，所以这一步不能静默做。
    expect(axes.bandwidths).toEqual(['2500m', '1000m']);
    expect(axes.lengths).toEqual(['14k', '1k']);
  });

  it('改配置不动 id', () => {
    let plan = ensureDefaults(emptyPlan());
    plan = updateRecipe(plan, 'udp', 'recipe-udp-default', {
      name: '改名',
      bandwidths: ['1000m'],
    });
    expect(plan.recipes.udp[0].id).toBe('recipe-udp-default');
    expect(plan.recipes.udp[0].bandwidths).toEqual(['1000m']);
  });

  it('新建的配置只用轴字段，且不写死字段 mode', () => {
    const plan = addRecipe(ensureDefaults(emptyPlan()), 'udp');
    const added = plan.recipes.udp[plan.recipes.udp.length - 1];
    expect(added.profiles).toEqual([]);
    expect(added.mode).toBeUndefined();
    expect(recipeIsAxisEditable(added)).toBe(true);
  });
});
