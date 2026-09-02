<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { toggleBinding, toggleSuiteColumn, isBound } from '../../domain/plan-build';
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
import { goto } from '../../state/ui';

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

function editRecipe(recipeId: string): void {
  focusedRecipeId.value = recipeId;
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

/** 一个套件是不是已经分配给了全部集合（整列开关的三态显示）。 */
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
      <h2>测试计划</h2>
      <p class="muted">
        链路集合 × 流量套件。数量与耗时以「预览」的服务端回包为准，这一页只管你想跑什么。
      </p>
    </header>

    <p v-if="!topologyReady" class="warn" role="alert">
      还没连上辅测机，没有可信的拓扑可对账。已保留你存下的集合原样——
      连上之后会自动按当前网卡补齐。
    </p>

    <div class="bar">
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
      <span class="muted">项目文件保存完整可复现配置，不含口令</span>
    </div>

    <p v-if="projectNotices.error" class="bad" role="alert">{{ projectNotices.error }}</p>
    <p v-for="(n, i) in projectNotices.items" :key="i" class="warn">{{ n }}</p>

    <div class="summary" aria-label="当前计划概况">
      <div><strong>{{ sets.length }}</strong><span>链路集合</span></div>
      <div><strong>{{ pairCount }}</strong><span>网口对</span></div>
      <div><strong>{{ suites.length }}</strong><span>套件</span></div>
      <div><strong>{{ plan.ui.bindings.length }}</strong><span>已分配</span></div>
    </div>

    <nav class="workbench-tabs" aria-label="计划编辑区域">
      <button type="button" :class="{ on: section === 'assign' }" @click="section = 'assign'">
        1. 分配链路与套件
      </button>
      <button type="button" :class="{ on: section === 'suites' }" @click="section = 'suites'">
        2. 编辑套件 <small>{{ taskCount }} 个任务</small>
      </button>
      <button type="button" :class="{ on: section === 'recipes' }" @click="section = 'recipes'">
        3. 编辑流量配置
      </button>
    </nav>

    <div v-if="section === 'assign'" class="workbench-panel">
    <div class="bar">
      <span class="bar-label">候选链路筛选</span>
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

    <p v-if="plan.stale.length" class="warn" role="alert">
      有 {{ plan.stale.length }} 条网口对在当前拓扑里找不到了（已标出，未删除）。
      它们只要没被绑定就不会挡下预览。
    </p>

    <h3>分配表</h3>
    <div v-if="sets.length === 0" class="empty">
      还没有链路集合。连上辅测机后会按网卡角色自动生成一批。
    </div>
    <div v-else class="scroll">
      <table>
        <thead>
          <tr>
            <th>链路集合</th>
            <th v-for="suite in suites" :key="suite.id" class="suite-col">
              <div class="suite-head">
                <span>{{ suite.name }}</span>
                <button
                  type="button"
                  class="colall"
                  :class="columnState(suite.id)"
                  :title="columnState(suite.id) === 'all' ? '整列取消' : '整列勾上'"
                  @click="onToggleColumn(suite.id)"
                >
                  整列
                </button>
              </div>
            </th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="set in sets" :key="set.id">
            <td>
              <strong>{{ set.name }}</strong>
              <small class="muted"> · {{ set.pair_refs.length }} 对</small>
              <small v-if="set.auto" class="tag">自动</small>
            </td>
            <td v-for="suite in suites" :key="suite.id" class="cell">
              <label class="check">
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
      <button type="button" class="ghost" @click="section = 'suites'">继续编辑套件 →</button>
    </div>
    </div>

    <div v-else-if="section === 'suites'" class="workbench-panel">
    <h3>套件</h3>
    <p class="muted hint">一个套件 = 一串按顺序执行的任务；数量与耗时以「执行」页预览为准。</p>
    <SuiteEditor @edit-recipe="editRecipe" />
    <div class="panel-next">
      <button type="button" class="ghost" @click="section = 'assign'">← 返回分配</button>
      <button type="button" class="ghost" @click="section = 'recipes'">继续编辑流量配置 →</button>
    </div>
    </div>

    <div v-else class="workbench-panel">
    <h3>流量配置</h3>
    <p class="muted hint">任务一条配置都不选时，走「执行」页的全局默认档位。</p>
    <RecipeEditor :focus-recipe-id="focusedRecipeId" />
    <div class="panel-next finish">
      <button type="button" class="ghost" @click="section = 'suites'">← 返回套件</button>
      <div>
        <strong>计划配置完成？</strong>
        <span class="muted">到执行页让服务端计算准确的单元数和耗时。</span>
      </div>
      <button type="button" @click="goto('run')">去预览与执行 →</button>
    </div>
    </div>
  </section>
</template>

<style scoped>
.hidden-file { display: none; }
.summary {
  display: grid; grid-template-columns: repeat(4, minmax(100px, 1fr));
  gap: 8px; margin: 0 0 14px;
}
.summary > div {
  display: flex; align-items: baseline; gap: 7px; padding: 9px 11px;
  border: 1px solid var(--line); border-radius: 5px; background: var(--panel-2);
}
.summary strong { font: 700 18px/1 var(--fm); color: var(--accent); }
.summary span { font-size: 11.5px; color: var(--muted); }
.workbench-tabs {
  position: sticky; top: -18px; z-index: 2;
  display: grid; grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 0; margin: 0 0 14px; padding-top: 8px; background: var(--surface);
  border-bottom: 1px solid var(--line);
}
.workbench-tabs button {
  padding: 10px 12px; border: 0; border-bottom: 3px solid transparent;
  border-radius: 0; background: transparent; color: var(--muted);
}
.workbench-tabs button:hover { background: var(--head); }
.workbench-tabs button.on { border-bottom-color: var(--accent); color: var(--ink); font-weight: 700; }
.workbench-tabs small { font-weight: 400; }
.workbench-panel > h3:first-child { margin-top: 0; }
.panel-next {
  display: flex; align-items: center; justify-content: flex-end; gap: 12px;
  flex-wrap: wrap; margin: 16px 0 0; padding: 12px;
  border-top: 1px solid var(--line); background: var(--panel-2);
}
.panel-next.finish > div { display: flex; flex-direction: column; margin-right: auto; }
@media (max-width: 700px) {
  .summary { grid-template-columns: repeat(2, minmax(100px, 1fr)); }
  .workbench-tabs { grid-template-columns: 1fr; position: static; border: 1px solid var(--line); }
  .workbench-tabs button { border-bottom-width: 1px; }
}
.ghost {
  padding: 6px 14px;
  border: 1px solid var(--line);
  border-radius: 4px;
  background: var(--surface);
  color: var(--ink);
  font: inherit;
  font-size: 13px;
  cursor: pointer;
}
.ghost.danger { border-color: var(--bad); color: var(--bad); }
.bad {
  margin: 0 0 12px;
  padding: 9px 12px;
  border-left: 3px solid var(--bad);
  background: var(--bad-bg);
}
.bar {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
  margin: 0 0 14px;
}
.bar-label {
  font-size: 12px;
  color: var(--muted);
}
.segmented {
  display: inline-flex;
  border: 1px solid var(--line);
  border-radius: 4px;
  overflow: hidden;
}
.segmented button {
  padding: 6px 14px;
  border: 0;
  border-right: 1px solid var(--line);
  background: var(--surface);
  color: var(--ink);
  font: inherit;
  font-size: 13px;
  cursor: pointer;
}
.segmented button:last-child { border-right: 0; }
.segmented button.on { background: var(--accent); color: var(--on-accent); font-weight: 600; }
.scroll {
  max-width: 100%;
  overflow-x: auto;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--surface);
}
table { width: 100%; border-collapse: separate; border-spacing: 0; font-size: 13px; }
th, td { padding: 8px 11px; text-align: left; border-bottom: 1px solid var(--line); }
thead th { background: var(--head); font-size: 11.5px; color: var(--muted); white-space: nowrap; }
tbody tr:last-child td { border-bottom: 0; }
.suite-col { min-width: 130px; }
.suite-head { display: flex; align-items: center; gap: 8px; justify-content: space-between; }
.colall {
  padding: 2px 7px;
  border: 1px solid var(--line);
  border-radius: 3px;
  background: var(--canvas);
  color: var(--muted);
  font: inherit;
  font-size: 11px;
  cursor: pointer;
}
.colall.all { border-color: var(--accent); background: var(--accent); color: var(--on-accent); }
.colall.some { border-color: var(--focus); color: var(--focus); }
.cell { text-align: center; }
.check input { width: 16px; height: 16px; cursor: pointer; }
.sr {
  position: absolute;
  width: 1px; height: 1px;
  overflow: hidden; clip-path: inset(50%);
}
.tag {
  margin-left: 6px;
  padding: 1px 5px;
  border-radius: 3px;
  background: var(--info-bg);
  color: var(--muted);
  font-size: 10.5px;
}
.hint { margin: -4px 0 10px; font-size: 12.5px; }
.muted { color: var(--muted); }
code { font-family: var(--fm); }
.empty {
  padding: 14px 16px;
  border: 1px dashed var(--line);
  border-radius: 6px;
  color: var(--muted);
  background: var(--panel-2);
}
.warn {
  margin: 0 0 14px;
  padding: 9px 12px;
  border-left: 3px solid var(--focus);
  background: var(--info-bg);
}
.mono { font-family: var(--fm); }
</style>
