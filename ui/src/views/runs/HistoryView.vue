<script setup lang="ts">
import { computed } from 'vue';
import UiTabs from '../../components/UiTabs.vue';
import { ui } from '../../state/ui';
import type { HistoryTab } from '../../state/ui';
import InnerRuns from './InnerRuns.vue';
import ScenarioRuns from './ScenarioRuns.vue';
import SubnetRuns from './SubnetRuns.vue';

/**
 * 「历史」：子网、内环、组合场景三类记录。旧版内环和组合记录挂在内环页底部，
 * 和子网的「历史运行」是两处入口。
 *
 * 这一页和 `bundle.zip` 是**一个功能的两半**（ADR-15）：不做列表页，远程用户
 * 拿不到 run id，下载链接就形同虚设。
 */
const tabs = [
  { id: 'subnet', label: '子网' },
  { id: 'inner', label: '内环' },
  { id: 'scenario', label: '组合场景' },
];
const tab = computed({
  get: () => ui.historyTab,
  set: (value: string) => { ui.historyTab = value as HistoryTab; },
});
</script>

<template>
  <section class="view">
    <header class="page-head">
      <h2>历史</h2>
    </header>
    <UiTabs v-model="tab" label="历史记录类型" panel-prefix="history" :tabs="tabs" />
    <div v-if="tab === 'subnet'" id="history-subnet" role="tabpanel" aria-labelledby="history-tab-subnet">
      <SubnetRuns />
    </div>
    <div v-else-if="tab === 'inner'" id="history-inner" role="tabpanel" aria-labelledby="history-tab-inner">
      <InnerRuns />
    </div>
    <div v-else id="history-scenario" role="tabpanel" aria-labelledby="history-tab-scenario">
      <ScenarioRuns />
    </div>
  </section>
</template>
