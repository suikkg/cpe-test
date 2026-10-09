<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import UiTabs from '../../components/UiTabs.vue';
import { freshnessLabel } from '../../domain/freshness';
import { selectedPortPairs } from '../../domain/plan-ports';
import { topologyReady } from '../../state/inventory';
import {
  exportProject,
  importProject,
  plan,
  preview,
  projectNotices,
  reconcile,
  restoreDefaultProject,
} from '../../state/plan';
import { goto, ui } from '../../state/ui';
import type { PlanTab } from '../../state/ui';
import GlobalDefaults from './GlobalDefaults.vue';
import NicPolicyTable from './NicPolicyTable.vue';
import PortSelection from './PortSelection.vue';
import SuiteEditor from './SuiteEditor.vue';

/**
 * 「计划」：测什么、按什么标准判。三个标签都写进项目文件。
 *
 * 这一页只管**意图**。单元数量、耗时、resume 预判一律等 `/api/plan` 回包——
 * 前端不复算（旧页那份浏览器估算和 Rust 的展开规则是两份实现，界面说 40 个
 * 单元、实际跑出 52 个，而两边"各自都没错"）。
 */

const resetArmed = ref(false);
const selectedPairs = computed(() => selectedPortPairs(plan.ui));
const taskCount = computed(() => plan.ui.suites.reduce((sum, suite) => sum + suite.tasks.length, 0));

/** 标签下的小字是**当前实况**，不是说明文案。 */
const tabs = computed(() => [
  { id: 'ports', label: '网口', hint: selectedPairs.value ? `已选 ${selectedPairs.value} 对` : '' },
  { id: 'content', label: '测试内容', hint: `${plan.ui.suites.length} 套件 · ${taskCount.value} 任务` },
  { id: 'limits', label: '门限与默认值', hint: '' },
]);
const tab = computed({
  get: () => ui.planTab,
  set: (value: string) => { ui.planTab = value as PlanTab; },
});

/** 去执行页准备这一轮：上一轮的结果让位给预览。 */
function reviewPorts(): void {
  ui.preparing = true;
  goto('run');
  void preview();
}

function editSuite(id: string): void {
  ui.suites.selected = id;
  ui.planTab = 'content';
}

/**
 * 草稿的可见状态。
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
      return '草稿未保存（浏览器禁止本地存储），请导出项目备份';
    default:
      return '';
  }
});

function onRestoreDefault(): void {
  if (!resetArmed.value) {
    resetArmed.value = true;
    return;
  }
  restoreDefaultProject();
  resetArmed.value = false;
  ui.planTab = 'ports';
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
    <header class="page-head">
      <h2>计划</h2>
      <div class="actions">
        <span class="draft" :class="plan.draftState" role="status">{{ draftLabel }}</span>
        <button type="button" class="ghost small" title="项目文件保存完整配置，不含口令" @click="fileInput?.click()">导入项目</button>
        <button type="button" class="ghost small" @click="onExport">导出项目</button>
        <button type="button" class="ghost small" :class="{ danger: resetArmed }" @click="onRestoreDefault">
          {{ resetArmed ? '确认恢复默认' : '恢复默认' }}
        </button>
        <button v-if="resetArmed" type="button" class="ghost small" @click="resetArmed = false">取消</button>
        <input ref="fileInput" type="file" accept="application/json,.json" hidden @change="onImport" />
      </div>
    </header>

    <p v-if="!topologyReady" class="msg warn" role="status">
      尚未取得两端网卡。<button type="button" class="linklike" @click="goto('connect')">去连接</button>
      <template v-if="plan.pendingImportTopology">；导入项目里的网口会保留，扫描成功后自动核对。</template>
    </p>
    <p v-else-if="plan.pendingImportTopology" class="msg warn" role="status">
      导入的部分网口尚未核对，连接或重新扫描成功后会移除确认缺失的网口并提示。
    </p>
    <p v-if="projectNotices.error" class="msg bad" role="alert">{{ projectNotices.error }}</p>
    <p v-for="(n, i) in projectNotices.items" :key="i" class="msg warn">{{ n }}</p>

    <UiTabs v-model="tab" label="计划编辑区域" panel-prefix="plan" :tabs="tabs" />

    <div v-if="tab === 'ports'" id="plan-ports" role="tabpanel" aria-labelledby="plan-tab-ports">
      <PortSelection @review="reviewPorts" @edit-suite="editSuite" />
    </div>
    <div v-else-if="tab === 'content'" id="plan-content" role="tabpanel" aria-labelledby="plan-tab-content">
      <SuiteEditor />
    </div>
    <div v-else id="plan-limits" role="tabpanel" aria-labelledby="plan-tab-limits">
      <p class="hint precedence">门限优先级：任务上填的门限 › 按网口门限 › Wi-Fi 频段门限 › 默认。生效门限见执行页预览。</p>
      <NicPolicyTable />
      <GlobalDefaults />
    </div>
  </section>
</template>

<style scoped>
.draft { font-size: 12px; color: var(--muted); }
.draft.saved { color: var(--ok); }
.draft.failed { color: var(--warn); font-weight: 600; }
.precedence { margin: 0 0 4px; }
</style>
