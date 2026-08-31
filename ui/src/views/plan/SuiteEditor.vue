<script setup lang="ts">
import { computed, ref } from 'vue';
import { formatNumberList, parseNumberList } from '../../domain/globals';
import {
  addSuite,
  addTask,
  directionLabel,
  duplicateSuite,
  moveTask,
  recipeSummary,
  removeSuite,
  removeTask,
  setTaskProtocol,
  taskUsesBidir,
  toggleTaskDirection,
  toggleTaskIp,
  toggleTaskRecipe,
  updateSuite,
  updateTask,
  type UiProtocol,
  type UiSuite,
  type UiTask,
} from '../../domain/plan-build';
import { plan } from '../../state/plan';

const emit = defineEmits<{ editRecipe: [recipeId: string] }>();

/**
 * 「套件」：左边一列套件名，右边只编辑选中的那一个。
 *
 * # 为什么是左右分栏而不是平铺
 *
 * 上一版把**所有**套件、所有任务、所有配置一次全展开。两个套件时还行，
 * 五个就是一堵墙：想改第四个套件的 UDP 方向，得先滚过前三个套件的全部任务，
 * 而屏幕上同时有二十几组复选框，没有一组是你正在看的。
 *
 * 分栏之后，纵向长度只跟**一个**套件的任务数有关，套件再多也只是左边那列变长。
 * 任务同样收成一行，点开才展开细节——一个套件常见 2~5 个任务，全展开同样会把
 * 「这个套件到底跑什么」这句话冲淡。
 */

const DIRECTIONS = ['ab', 'ba', 'bidir'];
const IPS = [
  { id: 'v4', label: 'IPv4' },
  { id: 'v6', label: 'IPv6' },
];
const PROTOCOLS: Array<{ id: UiProtocol; label: string }> = [
  { id: 'tcp', label: 'TCP' },
  { id: 'udp', label: 'UDP' },
  { id: 'ping', label: 'PING' },
];

/**
 * 选中的套件 id。
 *
 * 存 id 而不是下标：删掉一个套件之后下标会指向**另一个**套件，而那看起来像是
 * 「删错了」。读取一律走 `current`，它在 id 失效时回落到第一个。
 */
const selectedId = ref('');
const current = computed<UiSuite | undefined>(
  () => plan.ui.suites.find((suite) => suite.id === selectedId.value) ?? plan.ui.suites[0],
);

/** 展开了细节的任务 id。默认全收起——细节是「改的时候才看」的东西。 */
const openTasks = ref<string[]>([]);
function toggleTask(taskId: string): void {
  openTasks.value = openTasks.value.includes(taskId)
    ? openTasks.value.filter((id) => id !== taskId)
    : [...openTasks.value, taskId];
}

function recipesFor(protocol: UiProtocol) {
  return protocol === 'ping' ? [] : plan.ui.recipes[protocol];
}

function boundSets(suiteId: string): number {
  return plan.ui.bindings.filter((binding) => binding.suite_id === suiteId).length;
}

/** 左列那行小字：不点开也知道这个套件跑什么。 */
function suiteOutline(suite: UiSuite): string {
  return suite.tasks.map((task) => task.protocol.toUpperCase()).join(' → ') || '空';
}

/** 任务收起时的一行摘要：方向 · IP · 配置。 */
function taskSummary(task: UiTask): string {
  const parts = [
    (task.directions ?? []).map(directionLabel).join(' ') || '未选方向',
    (task.ip ?? []).join('/') || '未选 IP',
  ];
  if (task.protocol === 'ping') {
    parts.push(
      `${task.ping_count ?? '全局'} 次 / ${
        task.ping_payload_sizes?.length ? formatNumberList(task.ping_payload_sizes) : '全局'
      } 字节`,
    );
  } else {
    const picked = recipesFor(task.protocol).filter((recipe) =>
      task.recipe_ids.includes(recipe.id),
    );
    parts.push(picked.length ? picked.map((recipe) => recipe.name).join('、') : '全局默认档位');
  }
  if (task.duration) parts.push(`${task.duration}s`);
  return parts.join(' · ');
}

function onAddSuite(): void {
  plan.ui = addSuite(plan.ui);
  selectedId.value = plan.ui.suites[plan.ui.suites.length - 1].id;
}

function onRemoveSuite(suiteId: string): void {
  plan.ui = removeSuite(plan.ui, suiteId);
  selectedId.value = plan.ui.suites[0]?.id ?? '';
}

function onDuplicateSuite(suiteId: string): void {
  plan.ui = duplicateSuite(plan.ui, suiteId);
  selectedId.value = plan.ui.suites[plan.ui.suites.length - 1].id;
}

function onSuiteField(suiteId: string, field: 'name' | 'note', event: Event): void {
  plan.ui = updateSuite(plan.ui, suiteId, { [field]: (event.target as HTMLInputElement).value });
}

function onAddTask(suiteId: string, protocol: UiProtocol): void {
  plan.ui = addTask(plan.ui, suiteId, protocol);
  const suite = plan.ui.suites.find((item) => item.id === suiteId);
  const added = suite?.tasks[suite.tasks.length - 1];
  // 刚加的任务直接展开：加它就是为了配它。
  if (added) openTasks.value = [...openTasks.value, added.id];
}

function onTaskName(suiteId: string, taskId: string, event: Event): void {
  plan.ui = updateTask(plan.ui, suiteId, taskId, {
    name: (event.target as HTMLInputElement).value,
  });
}

function onProtocol(suiteId: string, taskId: string, event: Event): void {
  plan.ui = setTaskProtocol(
    plan.ui,
    suiteId,
    taskId,
    (event.target as HTMLSelectElement).value as UiProtocol,
  );
}

function onDuration(suiteId: string, taskId: string, event: Event): void {
  const raw = (event.target as HTMLInputElement).value.trim();
  const value = Number(raw);
  plan.ui = updateTask(plan.ui, suiteId, taskId, {
    // 空 = 跟着「执行」页的每单元时长走；后端对 `duration` 是 Option。
    duration: raw === '' || !Number.isFinite(value) || value <= 0 ? undefined : Math.trunc(value),
  });
}

function onPingCount(suiteId: string, taskId: string, event: Event): void {
  const raw = (event.target as HTMLInputElement).value.trim();
  const value = Number(raw);
  plan.ui = updateTask(plan.ui, suiteId, taskId, {
    ping_count: raw === '' || !Number.isFinite(value) || value <= 0 ? undefined : Math.trunc(value),
  });
}

function onPingSizes(suiteId: string, taskId: string, event: Event): void {
  const sizes = parseNumberList((event.target as HTMLInputElement).value);
  plan.ui = updateTask(plan.ui, suiteId, taskId, {
    // 空数组会被服务端拒（「至少需要一个 ping 包长」），所以空就是「不覆盖」。
    ping_payload_sizes: sizes.length ? sizes : undefined,
  });
}

function onBidirTarget(
  suiteId: string,
  taskId: string,
  field: 'rx_target_bidir_ab' | 'rx_target_bidir_ba',
  event: Event,
): void {
  plan.ui = updateTask(plan.ui, suiteId, taskId, {
    [field]: (event.target as HTMLInputElement).value,
  });
}

function has(list: string[] | undefined, value: string): boolean {
  return (list ?? []).includes(value);
}
</script>

<template>
  <div class="split">
    <!-- 左：套件列表 -->
    <div class="list" role="listbox" aria-label="套件">
      <button
        v-for="suite in plan.ui.suites"
        :key="suite.id"
        type="button"
        role="option"
        class="list-item"
        :class="{ on: current?.id === suite.id }"
        :aria-selected="current?.id === suite.id"
        @click="selectedId = suite.id"
      >
        <span class="list-name">{{ suite.name || '(未命名)' }}</span>
        <span class="list-meta">
          {{ suiteOutline(suite) }} ·
          {{ boundSets(suite.id) ? `已分配 ${boundSets(suite.id)}` : '未分配' }}
        </span>
      </button>
      <button type="button" class="ghost add" @click="onAddSuite">+ 新增套件</button>
    </div>

    <!-- 右：编辑选中的那一个 -->
    <div v-if="current" class="detail">
      <div class="detail-head">
        <input
          class="name"
          type="text"
          :value="current.name"
          aria-label="套件名称"
          @input="onSuiteField(current.id, 'name', $event)"
        />
        <input
          class="note"
          type="text"
          placeholder="备注（可留空）"
          :value="current.note"
          aria-label="套件备注"
          @input="onSuiteField(current.id, 'note', $event)"
        />
        <button
          type="button"
          class="ghost small"
          title="复制一份（含全部任务，不含分配）。给某条链路单独的双向门限就靠它。"
          @click="onDuplicateSuite(current.id)"
        >
          复制
        </button>
        <button
          type="button"
          class="ghost small danger"
          :disabled="plan.ui.suites.length <= 1"
          :title="
            plan.ui.suites.length <= 1
              ? '至少要留一个套件'
              : '删除这个套件，并清掉分配表里指向它的那一列'
          "
          @click="onRemoveSuite(current.id)"
        >
          删除
        </button>
      </div>

      <p class="muted outline">按顺序执行：{{ suiteOutline(current) }}</p>

      <ol class="tasks">
        <li v-for="(task, ti) in current.tasks" :key="task.id" class="task">
          <div class="task-row">
            <button
              type="button"
              class="disclose"
              :aria-expanded="openTasks.includes(task.id)"
              :title="openTasks.includes(task.id) ? '收起' : '展开配置'"
              @click="toggleTask(task.id)"
            >
              {{ openTasks.includes(task.id) ? '▾' : '▸' }}
            </button>
            <select
              :value="task.protocol"
              aria-label="协议"
              @change="onProtocol(current.id, task.id, $event)"
            >
              <option v-for="p in PROTOCOLS" :key="p.id" :value="p.id">{{ p.label }}</option>
            </select>
            <input
              class="name"
              type="text"
              :value="task.name"
              aria-label="任务名称"
              @input="onTaskName(current.id, task.id, $event)"
            />
            <span class="task-summary muted">{{ taskSummary(task) }}</span>
            <button
              type="button"
              class="ghost tiny"
              :disabled="ti === 0"
              title="上移（套件里的任务按顺序执行）"
              @click="plan.ui = moveTask(plan.ui, current.id, task.id, -1)"
            >
              ↑
            </button>
            <button
              type="button"
              class="ghost tiny"
              :disabled="ti === current.tasks.length - 1"
              title="下移"
              @click="plan.ui = moveTask(plan.ui, current.id, task.id, 1)"
            >
              ↓
            </button>
            <button
              type="button"
              class="ghost tiny danger"
              :disabled="current.tasks.length <= 1"
              :title="current.tasks.length <= 1 ? '套件至少要留一个任务' : '删除这个任务'"
              @click="plan.ui = removeTask(plan.ui, current.id, task.id)"
            >
              删除
            </button>
          </div>

          <div v-if="openTasks.includes(task.id)" class="task-body">
            <fieldset>
              <legend>方向</legend>
              <label v-for="direction in DIRECTIONS" :key="direction" class="check">
                <input
                  type="checkbox"
                  :checked="has(task.directions, direction)"
                  @change="plan.ui = toggleTaskDirection(plan.ui, current.id, task.id, direction)"
                />
                <span>{{ directionLabel(direction) }}</span>
              </label>
            </fieldset>

            <fieldset>
              <legend>IP 版本</legend>
              <label v-for="ip in IPS" :key="ip.id" class="check">
                <input
                  type="checkbox"
                  :checked="has(task.ip, ip.id)"
                  @change="plan.ui = toggleTaskIp(plan.ui, current.id, task.id, ip.id)"
                />
                <span>{{ ip.label }}</span>
              </label>
            </fieldset>

            <fieldset v-if="task.protocol !== 'ping'">
              <legend>配置（多选 = 各跑一遍）</legend>
              <span v-if="recipesFor(task.protocol).length === 0" class="muted small-hint">
                还没有 {{ task.protocol.toUpperCase() }} 配置，去下面「流量配置」加一条
              </span>
              <div
                v-for="recipe in recipesFor(task.protocol)"
                :key="recipe.id"
                class="recipe-choice"
              >
                <label class="check">
                  <input
                    type="checkbox"
                    :checked="has(task.recipe_ids, recipe.id)"
                    @change="plan.ui = toggleTaskRecipe(plan.ui, current.id, task.id, recipe.id)"
                  />
                  <span>
                    {{ recipe.name }}
                    <small class="muted mono">{{ recipeSummary(recipe, task.protocol) }}</small>
                  </span>
                </label>
                <button
                  type="button"
                  class="recipe-edit"
                  :aria-label="`编辑 ${recipe.name} 的参数`"
                  @click="emit('editRecipe', recipe.id)"
                >
                  编辑参数 →
                </button>
              </div>
              <span v-if="task.recipe_ids.length === 0" class="muted small-hint">
                一个都不选 = 走「执行」页的全局默认档位
              </span>
            </fieldset>

            <fieldset v-else>
              <legend>PING 参数</legend>
              <label class="inline">
                <span>次数</span>
                <input
                  type="text"
                  inputmode="numeric"
                  placeholder="沿用全局"
                  :value="task.ping_count ?? ''"
                  @input="onPingCount(current.id, task.id, $event)"
                />
              </label>
              <label class="inline">
                <span>包长（逗号分隔，各成一个单元）</span>
                <input
                  type="text"
                  placeholder="沿用全局"
                  :value="formatNumberList(task.ping_payload_sizes ?? [])"
                  @input="onPingSizes(current.id, task.id, $event)"
                />
              </label>
            </fieldset>

            <fieldset>
              <legend>本任务时长</legend>
              <label class="inline">
                <span>秒</span>
                <input
                  type="text"
                  inputmode="numeric"
                  placeholder="沿用执行页"
                  :value="task.duration ?? ''"
                  @input="onDuration(current.id, task.id, $event)"
                />
              </label>
            </fieldset>

            <fieldset v-if="taskUsesBidir(task)" class="wide">
              <legend>双向并发时各方向的接收门限（Mbps）</legend>
              <label class="inline">
                <span>A→B 接收端</span>
                <input
                  type="text"
                  placeholder="留空 = 走兜底判定"
                  :value="task.rx_target_bidir_ab ?? ''"
                  @input="onBidirTarget(current.id, task.id, 'rx_target_bidir_ab', $event)"
                />
              </label>
              <label class="inline">
                <span>B→A 接收端</span>
                <input
                  type="text"
                  placeholder="留空 = 走兜底判定"
                  :value="task.rx_target_bidir_ba ?? ''"
                  @input="onBidirTarget(current.id, task.id, 'rx_target_bidir_ba', $event)"
                />
              </label>
              <p class="muted small-hint">
                只收<strong>绝对 Mbps</strong>：双向并发时受限的是整条链路，而百分比要拿单块网口的
                协商速率去换算，两者不成比例。两个方向分开填——半双工链路的两个方向能差一个数量级。
                这组数字挂在<strong>任务</strong>上，会作用于所有分配了本套件的链路集合；
                想给某条链路单独的门限，用上面的「复制」再单独分配。
              </p>
            </fieldset>
          </div>
        </li>
      </ol>

      <div class="task-add">
        <span class="muted">加一条任务：</span>
        <button
          v-for="p in PROTOCOLS"
          :key="p.id"
          type="button"
          class="ghost small"
          @click="onAddTask(current.id, p.id)"
        >
          + {{ p.label }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.split { display: grid; grid-template-columns: 216px minmax(0, 1fr); gap: 12px; align-items: start; }
.list {
  display: flex; flex-direction: column; gap: 4px;
  max-height: min(480px, calc(100vh - 300px));
  overflow-y: auto; overscroll-behavior: contain; padding-right: 4px;
  scrollbar-gutter: stable;
}
.list-item {
  display: flex; flex-direction: column; gap: 2px;
  padding: 7px 9px; text-align: left;
  border: 1px solid transparent; border-radius: 5px;
  background: transparent; color: var(--ink); font: inherit; cursor: pointer;
}
.list-item:hover { background: var(--head); }
.list-item.on {
  background: var(--surface); border-color: var(--line);
  box-shadow: inset 3px 0 0 var(--accent); font-weight: 600;
}
.list-name { font-size: 13px; overflow-wrap: anywhere; }
.list-meta { font-size: 11px; color: var(--muted); font-weight: 400; overflow-wrap: anywhere; }
.add { margin-top: 4px; }

.detail {
  min-width: 0; padding: 11px 12px;
  border: 1px solid var(--line); border-radius: 6px; background: var(--surface);
}
.detail-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.detail-head .name { flex: 0 1 190px; font-weight: 700; }
.detail-head .note { flex: 1 1 160px; }
.outline { margin: 8px 0 10px; font-size: 12px; }

.tasks { margin: 0; padding: 0; list-style: none; }
.task { margin: 0 0 6px; border: 1px solid var(--line); border-radius: 5px; background: var(--canvas); }
.task-row { display: flex; align-items: center; gap: 7px; padding: 6px 8px; flex-wrap: wrap; }
.task-row .name { flex: 0 1 130px; font-weight: 600; }
.task-summary { flex: 1 1 180px; min-width: 0; font-size: 11.5px; overflow-wrap: anywhere; }
.disclose {
  flex: 0 0 auto; width: 20px; padding: 0;
  border: 0; background: none; color: var(--muted); font: inherit; cursor: pointer;
}
.task-body {
  display: grid; grid-template-columns: repeat(auto-fit, minmax(190px, 1fr));
  gap: 9px; padding: 0 8px 8px; border-top: 1px solid var(--line); padding-top: 9px;
}
fieldset { margin: 0; padding: 6px 8px; border: 1px solid var(--line); border-radius: 5px; min-width: 0; }
fieldset.wide { grid-column: 1 / -1; }
legend { padding: 0 4px; font-size: 11px; color: var(--muted); }
.check { display: flex; align-items: flex-start; gap: 6px; font-size: 12.5px; margin: 3px 0; }
.check input { width: 15px; height: 15px; flex: 0 0 auto; margin-top: 1px; }
.check > span { min-width: 0; overflow-wrap: anywhere; }
.check small { display: block; font-size: 11px; line-height: 1.4; }
.recipe-choice { display: flex; align-items: center; gap: 6px; }
.recipe-choice .check { flex: 1 1 auto; min-width: 0; }
.recipe-edit {
  margin-left: auto; padding: 1px 6px; border: 0; background: transparent;
  color: var(--accent); font-size: 11.5px; white-space: nowrap;
}
.recipe-edit:hover { background: var(--head); }
.inline { display: flex; align-items: center; gap: 6px; margin: 3px 0; font-size: 12.5px; }
.inline span { color: var(--muted); font-size: 11.5px; }
.inline input { flex: 1 1 80px; min-width: 0; }
.small-hint { display: block; margin: 4px 0 0; font-size: 11.5px; line-height: 1.5; }
input[type='text'], select {
  padding: 5px 7px; border: 1px solid var(--line); border-radius: 4px;
  background: var(--surface); color: var(--ink); font: inherit; font-size: 12.5px; min-width: 0;
  cursor: text; box-shadow: inset 0 0 0 1px var(--bezel-hi);
}
.detail-head input { background: var(--surface); }
input[type='text']:hover { border-color: var(--accent); }
input:focus-visible, select:focus-visible { outline: 2px solid var(--focus); outline-offset: 1px; }
.task-add { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; margin: 8px 0 0; }
.ghost {
  padding: 5px 12px; border: 1px solid var(--line); border-radius: 4px;
  background: var(--surface); color: var(--ink); font: inherit; font-size: 12.5px; cursor: pointer;
}
.ghost.small { padding: 3px 10px; font-size: 12px; }
.ghost.tiny { padding: 2px 7px; font-size: 12px; }
.ghost.danger { border-color: var(--bad); color: var(--bad); }
.ghost:disabled { opacity: .5; cursor: default; }
.muted { color: var(--muted); }
.mono { font-family: var(--fm); }
/* 窄屏退化成上下两段：左列变成一条横向可滚的清单。 */
@media (max-width: 860px) {
  .split { grid-template-columns: minmax(0, 1fr); }
  .list {
    flex-direction: row; max-height: none; overflow-x: auto; overflow-y: hidden;
    padding: 0 0 4px; scrollbar-gutter: auto;
  }
  .list-item { flex: 0 0 auto; min-width: 150px; }
}
</style>
