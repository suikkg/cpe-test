<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { taskDirectionsLabel, toggleBinding, toggleSuiteColumn, isBound } from '../../domain/plan-build';
import type { LinkFilter } from '../../domain/pairs';
import { topologyReady } from '../../state/inventory';
import {
  candidates,
  exportProject,
  importProject,
  loadDraft,
  plan,
  projectNotices,
  reconcile,
} from '../../state/plan';

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
 * 不走服务端：项目文件是纯前端的意图文档，服务端根本没有它。
 */
function onExport(): void {
  const blob = new Blob([exportProject()], { type: 'application/json' });
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
  // 草稿优先：误刷新不该让一份配了半天的计划归零。
  loadDraft();
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
      <input
        ref="fileInput"
        type="file"
        accept="application/json,.json"
        class="hidden-file"
        @change="onImport"
      />
      <span class="muted">项目文件只存计划意图，不含口令</span>
    </div>

    <p v-if="projectNotices.error" class="bad" role="alert">{{ projectNotices.error }}</p>
    <p v-for="(n, i) in projectNotices.items" :key="i" class="warn">{{ n }}</p>

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

    <h3>套件</h3>
    <div v-for="suite in suites" :key="suite.id" class="suite-card">
      <div class="suite-title">
        <strong>{{ suite.name }}</strong>
        <small class="muted">按顺序执行</small>
      </div>
      <ol class="tasks">
        <li v-for="task in suite.tasks" :key="task.id">
          <span class="proto mono">{{ task.protocol.toUpperCase() }}</span>
          <span>{{ task.name }}</span>
          <small class="muted">{{ taskDirectionsLabel(task) }} · {{ task.ip.join('/') }}</small>
        </li>
      </ol>
    </div>
  </section>
</template>

<style scoped>
.hidden-file { display: none; }
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
.suite-card {
  margin: 0 0 10px;
  padding: 11px 13px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--surface);
}
.suite-title { display: flex; align-items: baseline; gap: 10px; }
.tasks { margin: 8px 0 0; padding-left: 20px; }
.tasks li { display: flex; align-items: baseline; gap: 8px; margin: 3px 0; }
.proto {
  padding: 1px 6px;
  border-radius: 3px;
  background: var(--panel-2);
  font-size: 11px;
}
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
