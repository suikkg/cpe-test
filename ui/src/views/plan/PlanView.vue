<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import UiTabs from '../../components/UiTabs.vue';
import { toggleBinding, toggleSuiteColumn, isBound } from '../../domain/plan-build';
import { linkSetSearchFields } from '../../domain/grouping';
import { freshnessLabel } from '../../domain/freshness';
import { filterByQuery, visibleCountLabel } from '../../domain/search';
import type { LinkFilter } from '../../domain/pairs';
import { topologyReady } from '../../state/inventory';
import {
  candidates,
  exportProject,
  importProject,
  plan,
  projectNotices,
  reconcile,
  restoreDefaultProject,
} from '../../state/plan';
import RecipeEditor from './RecipeEditor.vue';
import SuiteEditor from './SuiteEditor.vue';
import { goto, ui } from '../../state/ui';

/**
 * 「测试计划」：链路集合 × 套件的分配表。
 *
 * 这一页只管**意图**。单元数量、耗时、resume 预判一律等 `/api/plan` 回包——
 * 前端不复算（旧页那份浏览器估算和 Rust 的展开规则是两份实现，界面说 40 个
 * 单元、实际跑出 52 个，而两边"各自都没错"）。
 */

const filters: Array<{ id: LinkFilter; label: string }> = [
  { id: 'all', label: '全部' },
  { id: 'cross', label: '跨机' },
  { id: 'same', label: '同机' },
];

const suites = computed(() => plan.ui.suites);
const sets = computed(() => plan.linkSets);
type WorkbenchSection = 'assign' | 'suites' | 'recipes';
const section = ref<WorkbenchSection>('assign');
const focusedRecipeId = ref('');
const resetArmed = ref(false);
const assignedSets = computed(
  () => new Set(plan.ui.bindings.map((binding) => binding.link_set_id)).size,
);
const pairCount = computed(() => sets.value.reduce((sum, set) => sum + set.pair_refs.length, 0));
const taskCount = computed(() => suites.value.reduce((sum, suite) => sum + suite.tasks.length, 0));

/** 三个工作区。小字是**当前实况**，不是说明文案——它回答「这一区现在有什么」。 */
const workbenchTabs = computed(() => [
  { id: 'assign', label: '分配链路与套件', hint: `${assignedSets.value}/${sets.value.length} 个集合已分配` },
  { id: 'suites', label: '编辑套件', hint: `${taskCount.value} 个任务` },
  { id: 'recipes', label: '编辑流量配置', hint: 'TCP / UDP 档位' },
]);

/**
 * 「从哪一条任务来的」——找不到就是 null（来源被删了）。
 *
 * §11.2：来源已删除时返回它的列表并说明对象已不存在，**不能**挑一个同名的
 * 冒充原对象。这里的做法是：找不到就不给「返回任务」，只留通用的「返回套件」。
 */
const returnTask = computed(() => {
  const from = ui.recipeReturn;
  if (!from) return null;
  const suite = plan.ui.suites.find((item) => item.id === from.suiteId);
  const task = suite?.tasks.find((item) => item.id === from.taskId);
  if (!suite || !task) return null;
  return { suiteName: suite.name || '(未命名)', taskName: task.name || task.protocol.toUpperCase() };
});

function backToTask(): void {
  const from = ui.recipeReturn;
  if (from) ui.suites.selected = from.suiteId;
  ui.recipeReturn = null;
  section.value = 'suites';
}

function editRecipe(payload: { recipeId: string; suiteId: string; taskId: string }): void {
  focusedRecipeId.value = payload.recipeId;
  // 记下从哪一条任务来的；配置编辑器据此给出「返回任务」。
  ui.recipeReturn = { suiteId: payload.suiteId, taskId: payload.taskId };
  ui.recipes.selected = payload.recipeId;
  section.value = 'recipes';
}

function onRestoreDefault(): void {
  if (!resetArmed.value) {
    resetArmed.value = true;
    return;
  }
  restoreDefaultProject();
  resetArmed.value = false;
  section.value = 'assign';
}

// ---- 分配表的名称查询 ----
//
// **只控制看得见哪些行**，不碰配对、绑定或链路范围（§11.1）。它绝不调用
// `reconcile()`：那是「全部/跨机/同机」那一组的事，它会重建链路集合并连带
// 剪掉指向不存在集合的绑定——用搜索触发它，等于打一个字就悄悄删掉用户的分配。
const pairIndex = computed(() => new Map(candidates.value.map((pair) => [pair.id, pair])));
const visibleSets = computed(() =>
  filterByQuery(sets.value, ui.plan.query, (set) => linkSetSearchFields(set, pairIndex.value)),
);
const setCountLabel = computed(() => visibleCountLabel(visibleSets.value.length, sets.value.length));

/**
 * 草稿的可见状态（§11.3 的三句话）。
 *
 * 「已保存」只在真的写进去之后说；写不进去时给的是**可执行的下一步**
 * （导出项目备份），不是一句"保存失败"。
 */
const draftLabel = computed(() => {
  switch (plan.draftState) {
    case 'pending':
      return '修改待保存…';
    case 'saved':
      return `草稿已保存 · ${freshnessLabel(plan.draftAt)}`;
    case 'failed':
      return '草稿未保存（浏览器不让写本地存储），请用「导出项目」留个备份';
    default:
      return '';
  }
});
/** 有查询时必须把「整列操作到底影响谁」说出来（§11.1）。 */
const columnScopeNote = computed(() =>
  ui.plan.query.trim() && visibleSets.value.length !== sets.value.length
    ? `整列操作影响全部 ${sets.value.length} 个链路集合，当前显示 ${visibleSets.value.length} 个`
    : '',
);

/**
 * 一个套件是不是已经分配给了全部集合（整列开关的三态显示）。
 *
 * 按**全部**集合算，不按当前显示的那几行——整列按钮作用于全部，三态就必须
 * 跟它说同一件事。否则搜出两行、两行都勾上，按钮会显示「取消全选」，
 * 而点下去取消的是全部集合的绑定。
 */
function columnState(suiteId: string): 'none' | 'some' | 'all' {
  if (sets.value.length === 0) return 'none';
  const bound = sets.value.filter((set) => isBound(plan.ui, set.id, suiteId)).length;
  if (bound === 0) return 'none';
  return bound === sets.value.length ? 'all' : 'some';
}

function onToggleColumn(suiteId: string): void {
  plan.ui = toggleSuiteColumn(plan.ui, suiteId);
}

function onToggleCell(linkSetId: string, suiteId: string): void {
  plan.ui = toggleBinding(plan.ui, linkSetId, suiteId);
}

function onFilter(next: LinkFilter): void {
  plan.filter = next;
  reconcile();
}

const fileInput = ref<HTMLInputElement | null>(null);

/**
 * 导出项目：用 Blob + 一次性 <a download>。
 *
 * 不走服务端：项目文件是纯前端的计划与执行设置文档，服务端根本没有它。
 */
function onExport(): void {
  const text = exportProject();
  // 拿不到判定基线时不下载——理由已经写进 projectNotices.error，就显示在下面。
  if (text === null) return;
  const blob = new Blob([text], { type: 'application/json' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = 'cpe-ui-project.json';
  a.click();
  URL.revokeObjectURL(url);
}

async function onImport(event: Event): Promise<void> {
  const input = event.target as HTMLInputElement;
  const file = input.files?.[0];
  if (!file) return;
  importProject(await file.text());
  // 清掉 value，否则同一个文件选第二次不会触发 change。
  input.value = '';
}

onMounted(() => {
  // 草稿由 `App.vue` 在启动时恢复——挂在这里的话，不路过这一页就永远不恢复。
  // 这里只按当前拓扑对一次账；它是幂等的，多调几次无害。
  reconcile();
});
</script>

<template>
  <section class="view">
    <header class="view-head">
      <div>
        <h2>测试计划</h2>
        <p class="muted">为链路集合分配套件，再按需调整任务与流量配置。</p>
      </div>
      <button type="button" class="preview-action" @click="goto('run')">预览与执行</button>
    </header>

    <p v-if="!topologyReady" class="warn" role="alert">
      双端网卡尚未全部就绪。导入时按已取得的扫描结果核对对应端点；未知端点暂时保留，
      连接或重新扫描成功后继续核对。
    </p>

    <div class="bar project-tools">
      <button type="button" class="ghost" @click="fileInput?.click()">导入项目</button>
      <button type="button" class="ghost" @click="onExport">导出项目</button>
      <button
        type="button"
        class="ghost"
        :class="{ danger: resetArmed }"
        @click="onRestoreDefault"
      >
        {{ resetArmed ? '确认清空并恢复' : '恢复默认计划' }}
      </button>
      <button v-if="resetArmed" type="button" class="ghost" @click="resetArmed = false">取消</button>
      <input
        ref="fileInput"
        type="file"
        accept="application/json,.json"
        class="hidden-file"
        @change="onImport"
      />
      <span class="muted project-note">项目文件保存完整配置，不含口令</span>
      <span class="draft" :class="plan.draftState" role="status">{{ draftLabel }}</span>
    </div>

    <p v-if="projectNotices.error" class="bad" role="alert">{{ projectNotices.error }}</p>
    <p v-if="plan.pendingImportTopology" class="warn" role="status">
      导入网口待校验：部分端点尚未取得可信扫描结果，暂时保留。连接或重新扫描成功后会自动移除确认缺失的网口对并提示。
    </p>
    <p v-for="(n, i) in projectNotices.items" :key="i" class="warn">{{ n }}</p>

    <div class="summary" aria-label="当前计划概况">
      <div><strong>{{ sets.length }}</strong><span>链路集合</span></div>
      <div><strong>{{ pairCount }}</strong><span>网口对</span></div>
      <div><strong>{{ suites.length }}</strong><span>套件</span></div>
      <div><strong>{{ plan.ui.bindings.length }}</strong><span>套件分配</span></div>
    </div>

    <UiTabs
      v-model="section"
      class="workbench-tabs"
      label="计划编辑区域"
      panel-prefix="workbench"
      :tabs="workbenchTabs"
    />

    <div
      v-if="section === 'assign'"
      id="workbench-assign"
      role="tabpanel"
      aria-labelledby="workbench-tab-assign"
      class="workbench-panel"
    >
    <div class="bar filter-bar">
      <span class="bar-label">候选链路</span>
      <div class="segmented" role="group" aria-label="候选链路筛选">
        <button
          v-for="f in filters"
          :key="f.id"
          type="button"
          :class="{ on: plan.filter === f.id }"
          :aria-pressed="plan.filter === f.id"
          @click="onFilter(f.id)"
        >
          {{ f.label }}
        </button>
      </div>
      <span class="muted">共 {{ candidates.length }} 条候选</span>
    </div>

    <div v-if="sets.length" class="bar search-bar">
      <label class="search">
        <span class="sr-only">搜索链路集合</span>
        <input
          type="search"
          :value="ui.plan.query"
          placeholder="搜集合名、网口名、IP、角色"
          @input="ui.plan.query = ($event.target as HTMLInputElement).value"
        />
      </label>
      <button v-if="ui.plan.query" type="button" class="ghost small" @click="ui.plan.query = ''">
        清空搜索
      </button>
      <span class="muted">{{ setCountLabel }}</span>
      <span v-if="columnScopeNote" class="muted scope-note">{{ columnScopeNote }}</span>
    </div>

    <p v-if="plan.stale.length" class="warn" role="alert">
      有 {{ plan.stale.length }} 条网口对在当前拓扑里找不到了（已标出，未删除）。
      它们只要没被绑定就不会挡下预览。
    </p>

    <div class="section-head">
      <div>
        <h3>分配表</h3>
        <p class="muted hint">勾选交叉格，为这一行的链路集合分配套件。可用列头按钮批量选择。</p>
      </div>
    </div>
    <div v-if="sets.length === 0" class="empty">
      <strong>还没有可分配的链路集合</strong>
      <p>连接辅测机并确认两端网卡信息后，会按网卡角色自动生成链路集合。</p>
      <button type="button" class="ghost" @click="goto('agent')">前往连接辅测机</button>
    </div>
    <p v-else-if="visibleSets.length === 0" class="empty compact-empty" role="status">
      没有链路集合匹配「{{ ui.plan.query.trim() }}」。
      <button type="button" class="linklike" @click="ui.plan.query = ''">清空搜索</button>
      可以看到全部 {{ sets.length }} 个。
    </p>
    <div v-else class="scroll">
      <table aria-label="链路集合与套件分配表">
        <thead>
          <tr>
            <th scope="col" class="set-col">链路集合</th>
            <th v-for="suite in suites" :key="suite.id" scope="col" class="suite-col">
              <div class="suite-head">
                <span>{{ suite.name }}</span>
                <button
                  type="button"
                  class="colall"
                  :class="columnState(suite.id)"
                  :title="columnState(suite.id) === 'all' ? '取消分配给所有链路集合' : '分配给所有链路集合'"
                  :aria-label="`${suite.name}：${columnState(suite.id) === 'all' ? '取消全选' : '全选此列'}`"
                  :aria-pressed="columnState(suite.id) === 'some' ? 'mixed' : columnState(suite.id) === 'all'"
                  @click="onToggleColumn(suite.id)"
                >
                  {{ columnState(suite.id) === 'all' ? '取消全选' : '全选此列' }}
                </button>
              </div>
            </th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="set in visibleSets" :key="set.id">
            <th scope="row" class="set-col">
              <strong>{{ set.name }}</strong>
              <span class="set-meta">
                <small class="muted">{{ set.pair_refs.length }} 对网口</small>
                <small v-if="set.auto" class="tag">自动生成</small>
              </span>
            </th>
            <td v-for="suite in suites" :key="suite.id" class="cell" :class="{ bound: isBound(plan.ui, set.id, suite.id) }">
              <label class="check" :title="`${set.name} / ${suite.name}`">
                <input
                  type="checkbox"
                  :checked="isBound(plan.ui, set.id, suite.id)"
                  @change="onToggleCell(set.id, suite.id)"
                />
                <span class="sr">{{ set.name }} 跑 {{ suite.name }}</span>
              </label>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <div class="panel-next">
      <span class="muted">已为 {{ assignedSets }}/{{ sets.length }} 个集合分配套件</span>
      <button type="button" class="ghost" @click="section = 'suites'">编辑套件</button>
    </div>
    </div>

    <div
      v-else-if="section === 'suites'"
      id="workbench-suites"
      role="tabpanel"
      aria-labelledby="workbench-tab-suites"
      class="workbench-panel"
    >
    <h3>套件</h3>
    <p class="muted hint">一个套件包含一组按顺序执行的任务。选中套件，展开任务调整参数。</p>
    <SuiteEditor @edit-recipe="editRecipe" />
    <div class="panel-next">
      <button type="button" class="ghost" @click="section = 'assign'">返回分配</button>
      <button type="button" class="ghost" @click="section = 'recipes'">编辑流量配置</button>
    </div>
    </div>

    <div
      v-else
      id="workbench-recipes"
      role="tabpanel"
      aria-labelledby="workbench-tab-recipes"
      class="workbench-panel"
    >
    <h3>流量配置</h3>
    <p class="muted hint">任务一条配置都不选时，走「执行」页的全局默认档位。</p>
    <p v-if="returnTask" class="return-note" role="status">
      正在改的是<strong>共享配置</strong>，改动对所有引用它的任务立即生效。
      <button type="button" class="linklike" @click="backToTask">
        返回「{{ returnTask.suiteName }} · {{ returnTask.taskName }}」
      </button>
    </p>
    <RecipeEditor :focus-recipe-id="focusedRecipeId" />
    <div class="panel-next finish">
      <button v-if="returnTask" type="button" class="ghost" @click="backToTask">
        返回「{{ returnTask.taskName }}」
      </button>
      <button v-else type="button" class="ghost" @click="section = 'suites'">返回套件</button>
      <div>
        <strong>计划配置完成？</strong>
        <span class="muted">在执行页预览实际测试单元、门限与预计耗时。</span>
      </div>
      <button type="button" @click="goto('run')">预览与执行</button>
    </div>
    </div>
  </section>
</template>

<style scoped>
.hidden-file { display: none; }
.view-head { display: flex; align-items: center; justify-content: space-between; gap: 20px; }
.preview-action { flex: 0 0 auto; }
.bar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.project-tools { padding: 12px 0 16px; margin-bottom: 0; }
.project-note { margin-left: auto; font-size: 12px; }
.summary {
  display: flex; flex-wrap: wrap; gap: 12px 26px; padding: 14px 0;
  border-top: 1px solid var(--line); border-bottom: 1px solid var(--line);
}
.summary > div { display: flex; align-items: baseline; gap: 8px; }
.summary strong { font-size: 20px; line-height: 1; font-variant-numeric: tabular-nums; }
.summary span { font-size: 12px; color: var(--muted); }
.workbench-tabs {
  position: sticky; top: 0; z-index: 3;
  display: grid; grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 0; margin: 0 0 22px; padding-top: 12px; background: var(--surface);
  border-bottom: 1px solid var(--line);
}
.workbench-tabs button {
  display: flex; flex-wrap: wrap; align-items: baseline; justify-content: center; gap: 4px 10px;
  padding: 13px 10px; border: 0; border-bottom: 3px solid transparent;
  border-radius: 0; background: transparent; color: var(--muted);
}
.workbench-tabs button:hover { background: var(--head); }
.workbench-tabs button.on { border-bottom-color: var(--accent); color: var(--accent); background: var(--info-bg); font-weight: 700; }
.workbench-tabs small { font-size: 11px; font-weight: 400; }
.workbench-panel > h3:first-child { margin-top: 0; }
.filter-bar { margin-bottom: 20px; }
.bar-label { margin-right: 4px; font-size: 13px; font-weight: 600; }
.search-bar { margin-bottom: 14px; }
.search { flex: 1 1 240px; max-width: 360px; }
.search input { width: 100%; padding: 8px 11px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); color: var(--ink); font: inherit; }
button.small { min-height: 32px; padding: 6px 11px; font-size: 12.5px; }
/* 整列操作的作用域说明：有查询时它必须在场，所以给它一个不会被挤没的落点。 */
.scope-note { flex-basis: 100%; font-size: 12px; }
.linklike { padding: 0; min-height: 0; font: inherit; color: var(--accent); background: none; border: 0; text-decoration: underline; text-underline-offset: 3px; }
.linklike:hover:not(:disabled) { background: none; color: var(--accent-hover); }
.compact-empty { padding: 16px 18px; font-size: 13px; }
.return-note { margin: 0 0 12px; padding: 9px 12px; border-left: 3px solid var(--accent); background: var(--info-bg); font-size: 12.5px; }
.draft { font-size: 12px; color: var(--muted); }
.draft.saved { color: var(--ok); }
.draft.failed { color: var(--warn); font-weight: 600; }
.filter-bar > .muted { margin-left: 4px; font-size: 12px; }
.section-head h3 { margin: 0 0 6px; }
.panel-next {
  display: flex; align-items: center; justify-content: flex-end; gap: 12px;
  flex-wrap: wrap; margin: 20px 0 0; padding: 16px 0 0; border-top: 1px solid var(--line);
}
.panel-next > .muted { margin-right: auto; font-size: 12px; }
.panel-next.finish > div { display: flex; flex-direction: column; gap: 3px; margin-right: auto; font-size: 12px; }
.ghost {
  min-height: 36px; padding: 7px 12px; border: 1px solid var(--line); border-radius: 6px;
  background: var(--surface); color: var(--ink); font: inherit; font-size: 12px; cursor: pointer;
}
.ghost.danger { border-color: var(--bad); color: var(--bad); }
.bad, .warn { margin: 0 0 16px; padding: 12px 14px; border-left: 3px solid var(--bad); background: var(--bad-bg); border-radius: 0 6px 6px 0; }
.warn { border-left-color: var(--warn); background: var(--info-bg); }
.segmented { display: inline-flex; padding: 3px; gap: 3px; border: 1px solid var(--line); border-radius: 7px; background: var(--panel-2); }
.segmented button {
  min-height: 30px; padding: 5px 16px; border: 0; border-radius: 4px;
  background: transparent; color: var(--muted); font: inherit; font-size: 12px; cursor: pointer;
}
.segmented button:hover { background: var(--head); }
.segmented button.on { background: var(--accent); color: var(--on-accent); font-weight: 600; }
.scroll { max-width: 100%; overflow-x: auto; border: 1px solid var(--line); border-radius: 8px; background: var(--surface); }
table { width: 100%; border-collapse: separate; border-spacing: 0; font-size: 13px; }
th, td { padding: 13px 16px; text-align: left; border-bottom: 1px solid var(--line); }
thead th { background: var(--head); font-size: 12px; color: var(--muted); }
tbody tr:last-child > * { border-bottom: 0; }
.set-col { position: sticky; left: 0; z-index: 1; min-width: 188px; border-right: 1px solid var(--line); }
tbody .set-col { background: var(--surface); font-weight: 400; }
.set-col strong { display: block; font-size: 13px; overflow-wrap: anywhere; }
.set-meta { display: flex; align-items: center; gap: 8px; margin-top: 5px; }
.suite-col { min-width: 166px; }
.suite-head { display: flex; flex-direction: column; align-items: center; gap: 8px; text-align: center; }
.suite-head > span { color: var(--ink); overflow-wrap: anywhere; }
.colall {
  min-height: 28px; padding: 4px 8px; border: 1px solid var(--line); border-radius: 4px;
  background: var(--surface); color: var(--muted); font: inherit; font-size: 11px; cursor: pointer;
}
.colall:hover { background: var(--panel-2); }
.colall.all { border-color: var(--accent); color: var(--accent); background: var(--info-bg); }
.colall.some { border-color: var(--accent); color: var(--accent); }
.cell { padding: 0; text-align: center; }
.cell.bound { background: var(--edited); }
.check { display: flex; justify-content: center; align-items: center; min-height: 66px; padding: 16px; cursor: pointer; }
.check:hover { box-shadow: inset 0 0 0 1px var(--accent); }
.check input { width: 18px; height: 18px; margin: 0; cursor: pointer; accent-color: var(--accent); }
.sr { position: absolute; width: 1px; height: 1px; overflow: hidden; clip-path: inset(50%); }
.tag { padding: 1px 5px; border-radius: 3px; background: var(--panel-2); color: var(--muted); font-size: 10px; }
.hint { margin: 0 0 16px; font-size: 12.5px; }
.muted { color: var(--muted); }
.empty { padding: 28px; border: 1px dashed var(--line); border-radius: 8px; background: var(--panel-2); }
.empty strong { display: block; font-size: 14px; }
.empty p { margin: 6px 0 16px; color: var(--muted); font-size: 13px; }
@media (max-width: 700px) {
  .view-head { align-items: flex-start; flex-direction: column; gap: 12px; }
  .project-note { flex-basis: 100%; margin: 2px 0 0; }
  .summary { gap: 14px 24px; }
  .summary > div { flex: 1 1 calc(50% - 24px); }
  .workbench-tabs { position: static; margin-bottom: 18px; }
  .workbench-tabs button { align-content: start; gap: 5px; padding: 10px 6px; font-size: 12px; }
  .workbench-tabs small { flex-basis: 100%; font-size: 10px; }
  .set-col { min-width: 132px; max-width: 150px; }
  th, td { padding: 12px; }
  .suite-col { min-width: 135px; }
  .panel-next { align-items: flex-start; }
  .panel-next.finish > div { flex-basis: 100%; order: -1; }
}
</style>
