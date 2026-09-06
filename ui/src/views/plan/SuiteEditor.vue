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
  taskUsesSingleDirection,
  toggleTaskDirection,
  toggleTaskIp,
  toggleTaskRecipe,
  updateSuite,
  updateTask,
  type UiProtocol,
  type UiSuite,
  type UiTask,
} from '../../domain/plan-build';
import { filterByQuery, visibleCountLabel } from '../../domain/search';
import { plan } from '../../state/plan';
import { ui } from '../../state/ui';

// 带上**来源**：光有 recipeId，「返回任务」只能回到这个工作区，回不到那一条任务。
const emit = defineEmits<{ editRecipe: [payload: { recipeId: string; suiteId: string; taskId: string }] }>();

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
// 选中放 `state/ui`，不放组件本地 `ref`：这一页是 `v-if` 卸载的，切去
// 「配置」改完参数再回来，本地 ref 已经没了，用户会看到另一个套件被选中。
const selectedId = computed({
  get: () => ui.suites.selected,
  set: (value: string) => { ui.suites.selected = value; },
});
const current = computed<UiSuite | undefined>(
  () => plan.ui.suites.find((suite) => suite.id === selectedId.value) ?? plan.ui.suites[0],
);

/** 左列的搜索：套件名，外加它里面任务的名字与协议（§11.1）。 */
const visibleSuites = computed(() =>
  filterByQuery(plan.ui.suites, ui.suites.query, (suite) => [
    suite.name,
    ...suite.tasks.map((task) => task.name),
    ...suite.tasks.map((task) => task.protocol),
  ]),
);
const suiteCountLabel = computed(() =>
  visibleCountLabel(visibleSuites.value.length, plan.ui.suites.length),
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

/**
 * 删掉之后选**原位置的下一个**，末项则选上一个（§11.2）。
 *
 * 一律回到第一个的话，删掉第 7 个套件之后视线被甩回列表顶端——用户接下来
 * 多半还要删第 8 个，而他得先滚回去重新找。
 */
function onRemoveSuite(suiteId: string): void {
  const at = plan.ui.suites.findIndex((suite) => suite.id === suiteId);
  plan.ui = removeSuite(plan.ui, suiteId);
  const next = plan.ui.suites[at] ?? plan.ui.suites[at - 1] ?? plan.ui.suites[0];
  selectedId.value = next?.id ?? '';
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

function onRxTarget(
  suiteId: string,
  taskId: string,
  field:
    | 'rx_target_ab'
    | 'rx_target_ba'
    | 'rx_target_bidir_ab'
    | 'rx_target_bidir_ba'
    | 'rx_target_bidir_total',
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
    <div class="list-col">
      <label v-if="plan.ui.suites.length > 3" class="list-search">
        <span class="sr-only">搜索套件</span>
        <input
          type="search"
          :value="ui.suites.query"
          placeholder="搜套件名或任务"
          @input="ui.suites.query = ($event.target as HTMLInputElement).value"
        />
      </label>
      <p v-if="ui.suites.query" class="list-count muted">
        {{ suiteCountLabel }}
        <button type="button" class="linklike" @click="ui.suites.query = ''">清空</button>
      </p>
    <div class="list" role="group" aria-label="选择套件">
      <button
        v-for="suite in visibleSuites"
        :key="suite.id"
        type="button"
        class="list-item"
        :class="{ on: current?.id === suite.id }"
        :aria-pressed="current?.id === suite.id"
        @click="selectedId = suite.id"
      >
        <span class="list-name">{{ suite.name || '(未命名)' }}</span>
        <span class="list-meta">
          {{ suiteOutline(suite) }}
        </span>
        <span class="list-meta">
          {{ boundSets(suite.id) ? `已分配给 ${boundSets(suite.id)} 个链路集合` : '尚未分配链路集合' }}
        </span>
      </button>
      <p v-if="visibleSuites.length === 0" class="list-empty muted">
        没有套件匹配「{{ ui.suites.query.trim() }}」。
      </p>
      <button type="button" class="ghost add" @click="onAddSuite">+ 新增套件</button>
    </div>
    </div>

    <!-- 右：编辑选中的那一个 -->
    <div v-if="current" class="detail">
      <div class="detail-head">
        <label class="name-field">
          <span>套件名称</span>
          <input
            class="name"
            type="text"
            :value="current.name"
            @input="onSuiteField(current.id, 'name', $event)"
          />
        </label>
        <label class="note-field">
          <span>备注 <small>可选</small></span>
          <input
            class="note"
            type="text"
            placeholder="说明此套件的用途"
            :value="current.note"
            @input="onSuiteField(current.id, 'note', $event)"
          />
        </label>
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

      <div class="outline">
        <strong>{{ current.tasks.length }} 个任务</strong>
        <span class="muted">按顺序执行：{{ suiteOutline(current) }}</span>
        <span class="muted">展开任务可调整方向、IP 与参数</span>
      </div>

      <ol class="tasks">
        <li v-for="(task, ti) in current.tasks" :key="task.id" class="task">
          <div class="task-row">
            <span class="task-number" :aria-label="`第 ${ti + 1} 个任务`">{{ ti + 1 }}</span>
            <button
              type="button"
              class="disclose"
              :aria-expanded="openTasks.includes(task.id)"
              :aria-controls="`task-settings-${task.id}`"
              :aria-label="`${openTasks.includes(task.id) ? '收起' : '展开'} ${task.name || '任务'} 的配置`"
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
            <div class="task-actions">
              <button
                type="button"
                class="ghost tiny"
                :disabled="ti === 0"
                title="上移（套件里的任务按顺序执行）"
                :aria-label="`上移 ${task.name || '任务'}`"
                @click="plan.ui = moveTask(plan.ui, current.id, task.id, -1)"
              >
                ↑
              </button>
              <button
                type="button"
                class="ghost tiny"
                :disabled="ti === current.tasks.length - 1"
                title="下移"
                :aria-label="`下移 ${task.name || '任务'}`"
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
          </div>

          <div v-if="openTasks.includes(task.id)" :id="`task-settings-${task.id}`" class="task-body">
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

            <fieldset v-if="task.protocol !== 'ping'" class="wide recipe-fieldset">
              <legend>配置（多选 = 各跑一遍）</legend>
              <span v-if="recipesFor(task.protocol).length === 0" class="muted small-hint">
                还没有 {{ task.protocol.toUpperCase() }} 配置，请在「编辑流量配置」中添加。
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
                  @click="emit('editRecipe', { recipeId: recipe.id, suiteId: current.id, taskId: task.id })"
                >
                  编辑参数
                </button>
              </div>
              <span v-if="task.recipe_ids.length === 0" class="muted small-hint">
                一个都不选 = 走「执行」页的全局默认档位
              </span>
            </fieldset>

            <fieldset v-else class="wide ping-fieldset">
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

            <fieldset class="wide duration-fieldset">
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

            <fieldset
              v-if="taskUsesSingleDirection(task, 'ab') || taskUsesSingleDirection(task, 'ba')"
              class="wide"
            >
              <legend>单向的接收门限（Mbps）</legend>
              <label v-if="taskUsesSingleDirection(task, 'ab')" class="inline">
                <span>A→B 接收端</span>
                <input
                  type="text"
                  placeholder="留空 = 走按网口门限"
                  :value="task.rx_target_ab ?? ''"
                  @input="onRxTarget(current.id, task.id, 'rx_target_ab', $event)"
                />
              </label>
              <label v-if="taskUsesSingleDirection(task, 'ba')" class="inline">
                <span>B→A 接收端</span>
                <input
                  type="text"
                  placeholder="留空 = 走按网口门限"
                  :value="task.rx_target_ba ?? ''"
                  @input="onRxTarget(current.id, task.id, 'rx_target_ba', $event)"
                />
              </label>
              <p class="muted small-hint">
                填了这里就<strong>不再看「按网口门限」那张表</strong>：那张表一块网卡只能填一个数，
                而同一块网卡对不同对端能收到的完全不是一个量级——1G 口做发送端时，
                收口上挂的 1800/2000 在这条路径上物理上就跑不到。只能填绝对 Mbps。
              </p>
              <p class="muted small-hint">门限挂在任务上，作用于所有分配了本套件的链路集合。</p>
            </fieldset>

            <fieldset v-if="taskUsesBidir(task)" class="wide">
              <legend>双向并发的接收门限（Mbps）</legend>
              <label class="inline">
                <span>双向 RX 合计</span>
                <input
                  type="text"
                  placeholder="留空 = 只显示实测"
                  :value="task.rx_target_bidir_total ?? ''"
                  @input="onRxTarget(current.id, task.id, 'rx_target_bidir_total', $event)"
                />
              </label>
              <p class="muted small-hint">
                判一次：<strong>A→B 接收端 RX + B→A 接收端 RX ≥ 合计</strong>，单位 Mbps。
              </p>
              <details class="per-direction-bidir">
                <summary>按方向分别设门限（逐方向把关）</summary>
                <label class="inline">
                  <span>A→B 接收端</span>
                  <input
                    type="text"
                    placeholder="留空 = 走兜底判定"
                    :value="task.rx_target_bidir_ab ?? ''"
                    @input="onRxTarget(current.id, task.id, 'rx_target_bidir_ab', $event)"
                  />
                </label>
                <label class="inline">
                  <span>B→A 接收端</span>
                  <input
                    type="text"
                    placeholder="留空 = 走兜底判定"
                    :value="task.rx_target_bidir_ba ?? ''"
                    @input="onRxTarget(current.id, task.id, 'rx_target_bidir_ba', $event)"
                  />
                </label>
                <p class="muted small-hint">两个方向各判一次；填了合计门限时这两格不参与判定。</p>
              </details>
              <p class="muted small-hint">门限挂在任务上，作用于所有分配了本套件的链路集合。</p>
            </fieldset>
          </div>
        </li>
      </ol>

      <div class="task-add">
        <span class="muted">添加任务</span>
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
.split { display: grid; grid-template-columns: 236px minmax(0, 1fr); gap: 20px; align-items: start; }
.list-col { display: grid; gap: 8px; min-width: 0; }
.list-search input { width: 100%; padding: 7px 10px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); color: var(--ink); font: inherit; font-size: 13px; }
.list-count { margin: 0; font-size: 12px; }
.list-empty { margin: 4px 0; font-size: 12.5px; }
.linklike { padding: 0; min-height: 0; font: inherit; font-size: 12px; color: var(--accent); background: none; border: 0; text-decoration: underline; text-underline-offset: 3px; }
.linklike:hover:not(:disabled) { background: none; color: var(--accent-hover); }
.list { display: flex; flex-direction: column; gap: 6px; max-height: min(620px, calc(100vh - 240px)); overflow-y: auto; overscroll-behavior: contain; padding-right: 6px; scrollbar-gutter: stable; }
.list-item { display: flex; flex-direction: column; gap: 5px; padding: 12px; text-align: left; border: 1px solid transparent; border-radius: 7px; background: transparent; color: var(--ink); font: inherit; cursor: pointer; }
.list-item:hover { background: var(--head); }
.list-item.on { background: var(--info-bg); border-color: var(--line); box-shadow: inset 3px 0 0 var(--accent); }
.list-name { font-size: 13px; font-weight: 600; overflow-wrap: anywhere; }
.list-meta { font-size: 11px; line-height: 1.55; color: var(--muted); overflow-wrap: anywhere; }
.add { margin-top: 4px; }
.detail { min-width: 0; padding: 20px; border: 1px solid var(--line); border-radius: 9px; background: var(--surface); }
.detail-head { display: flex; align-items: flex-end; gap: 10px; flex-wrap: wrap; }
.detail-head > label { display: flex; flex-direction: column; gap: 6px; min-width: 0; }
.detail-head label > span { font-size: 12px; color: var(--muted); }
.detail-head label small { margin-left: 5px; font-size: 10px; }
.name-field { flex: 1 1 170px; }
.note-field { flex: 1 1 200px; }
.detail-head .name { font-weight: 600; font-size: 14px; }
.outline { display: flex; align-items: baseline; gap: 5px 12px; flex-wrap: wrap; margin: 20px 0 12px; font-size: 12px; }
.outline > strong { font-size: 13px; }
.outline > span:last-child { flex-basis: 100%; font-size: 11px; }
.tasks { margin: 0; padding: 0; list-style: none; }
.task { margin: 0 0 10px; border: 1px solid var(--line); border-radius: 7px; background: var(--panel-2); }
.task-row { display: grid; grid-template-columns: 22px 30px 80px minmax(90px, 1fr) auto; align-items: center; gap: 8px; padding: 12px; }
.task-number { color: var(--muted); font-size: 12px; font-variant-numeric: tabular-nums; text-align: center; }
.task-row .name { width: 100%; font-weight: 600; }
.task-summary { grid-row: 2; grid-column: 3 / -1; min-width: 0; font-size: 11px; line-height: 1.6; overflow-wrap: anywhere; }
.task-actions { display: flex; align-items: center; gap: 4px; }
.disclose { display: flex; justify-content: center; align-items: center; width: 30px; min-height: 34px; padding: 0; border: 1px solid var(--line); border-radius: 4px; background: var(--surface); color: var(--accent); font: inherit; font-size: 16px; cursor: pointer; }
.disclose:hover { background: var(--head); }
.task-body { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 16px; padding: 18px 14px 14px; border-top: 1px solid var(--line); background: var(--surface); border-radius: 0 0 7px 7px; }
fieldset { margin: 0; padding: 12px; border: 1px solid var(--line); border-radius: 6px; min-width: 0; }
fieldset.wide { grid-column: 1 / -1; }
legend { padding: 0 5px; font-size: 12px; font-weight: 600; color: var(--muted); }
.check { display: flex; align-items: flex-start; gap: 9px; font-size: 12px; min-height: 32px; padding: 5px 0; cursor: pointer; }
.check input { width: 17px; height: 17px; flex: 0 0 auto; margin: 1px 0 0; accent-color: var(--accent); cursor: pointer; }
.check > span { min-width: 0; overflow-wrap: anywhere; }
.check small { display: block; margin-top: 3px; font-size: 11px; line-height: 1.5; }
.recipe-choice { display: flex; align-items: center; gap: 12px; padding: 4px 0; border-bottom: 1px solid var(--line); }
.recipe-choice:last-of-type { border-bottom: 0; }
.recipe-choice .check { flex: 1 1 auto; min-width: 0; }
.recipe-edit { margin-left: auto; min-height: 32px; padding: 5px 8px; border: 0; background: transparent; color: var(--accent); font-size: 11px; white-space: nowrap; }
.recipe-edit:hover { background: var(--head); }
.inline { display: grid; grid-template-columns: minmax(120px, .75fr) minmax(0, 1fr); align-items: center; gap: 12px; margin: 7px 0; font-size: 12px; }
.inline span { color: var(--muted); font-size: 12px; }
.inline input { width: 100%; min-width: 0; }
.duration-fieldset .inline { max-width: 380px; }
.small-hint { display: block; margin: 8px 0 0; font-size: 11px; line-height: 1.7; }
input[type='text'], select { min-height: 36px; padding: 7px 9px; border: 1px solid var(--line); border-radius: 5px; background: var(--surface); color: var(--ink); font: inherit; font-size: 12px; min-width: 0; }
input[type='text'] { cursor: text; }
select { cursor: pointer; }
input[type='text']:hover, select:hover { border-color: var(--accent); }
input:focus-visible, select:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }
.task-add { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; margin: 18px 0 0; padding-top: 14px; border-top: 1px solid var(--line); }
.task-add > span { font-size: 12px; margin-right: 4px; }
.ghost { min-height: 36px; padding: 7px 12px; border: 1px solid var(--line); border-radius: 5px; background: var(--surface); color: var(--ink); font: inherit; font-size: 12px; cursor: pointer; }
.ghost.small { font-size: 12px; }
.ghost.tiny { min-width: 30px; min-height: 32px; padding: 5px 7px; font-size: 11px; }
.ghost.danger { color: var(--bad); }
.ghost:disabled { opacity: .45; cursor: not-allowed; }
.per-direction-bidir { margin: 14px 0 0; padding-top: 10px; border-top: 1px solid var(--line); }
.per-direction-bidir > summary { color: var(--accent); cursor: pointer; font-size: 12px; line-height: 1.7; }
.per-direction-bidir[open] > summary { margin-bottom: 12px; }
.muted { color: var(--muted); }
.mono { font-family: var(--fm); }
@media (max-width: 1100px) {
  .split { grid-template-columns: 190px minmax(0, 1fr); gap: 14px; }
  .detail { padding: 16px; }
  .task-row { grid-template-columns: 18px 30px 78px minmax(80px, 1fr); gap: 7px; }
  .task-actions { grid-column: 3 / -1; justify-content: flex-end; }
  .task-summary { grid-row: 2; }
}
@media (max-width: 760px) {
  .split { grid-template-columns: minmax(0, 1fr); }
  .list { flex-direction: row; max-height: none; overflow-x: auto; overflow-y: hidden; padding: 0 0 8px; scrollbar-gutter: auto; }
  .list-item { flex: 0 0 200px; border-color: var(--line); }
  .list > .add { flex: 0 0 auto; margin: 0; }
}
@media (max-width: 480px) {
  .detail { padding: 12px; }
  .task-row { padding: 10px; grid-template-columns: 16px 28px 76px minmax(65px, 1fr); gap: 6px; }
  .task-row .name { font-size: 11px; padding-inline: 6px; }
  .task-body { grid-template-columns: minmax(0, 1fr); gap: 12px; padding: 14px 10px 10px; }
  .inline { grid-template-columns: minmax(0, 1fr); gap: 5px; margin: 10px 0; }
  .recipe-choice { align-items: flex-start; gap: 4px; }
  .recipe-edit { padding-inline: 3px; }
}
</style>
