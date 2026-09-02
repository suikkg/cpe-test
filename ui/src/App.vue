<script setup lang="ts">
import { computed, onMounted } from 'vue';
import { REGIONS, ui, goto, setTheme, applyTheme } from './state/ui';
import type { RegionId } from './state/ui';
import { agentNics, masterNics } from './state/inventory';
import { applyBootstrapDefaults, loadDraft, plan } from './state/plan';
import { run, view as runView } from './state/run';
import { load, session } from './state/session';
import LocalView from './views/local/LocalView.vue';
import AgentView from './views/agent/AgentView.vue';
import PlanView from './views/plan/PlanView.vue';
import RunView from './views/run/RunView.vue';
import ProgressView from './views/progress/ProgressView.vue';
import MonitorView from './views/monitor/MonitorView.vue';
import RunsView from './views/runs/RunsView.vue';

// 各区域的实时角标。旧页用「第几步」的编号来暗示进度，但那个编号是假的：
// 「本机」不编号却常驻，第 3 步内部又自带一套 1·2·3·4。改成状态角标之后，
// 导航栏说的是**现在是什么情况**，而不是**你应该走到第几步**。
const badges = computed<Partial<Record<RegionId, string>>>(() => ({
  local: masterNics.value.length ? `${masterNics.value.length} 网卡` : '',
  agent: agentNics.value.length ? `${agentNics.value.length} 网卡` : '',
  plan: plan.ui.bindings.length ? `${plan.ui.bindings.length} 项分配` : '',
  run: plan.preview ? `${plan.preview.units.length} 单元` : '',
  progress: run.running ? `${runView.value.done}/${runView.value.total}` : '',
}));

/** 口令失效是**全局终态**：没有口令时点什么都是 401，不该让人逐页去撞。 */
const unauthorized = computed(() => session.phase === 'unauthorized');
const connectionLabel = computed(() => {
  if (session.phase === 'connecting') return '连接中';
  if (session.phase === 'connected') return `已连 ${session.host || '辅测机'}`;
  if (session.phase === 'failed') return '辅测机未连接';
  return '待连接辅测机';
});
const runLabel = computed(() => {
  if (run.running) return `运行中 ${runView.value.done}/${runView.value.total}`;
  if (runView.value.finished) return '本轮已结束';
  return '空闲';
});

const flowRegions = computed(() => REGIONS.filter((r) => r.group === 'flow'));
const toolRegions = computed(() => REGIONS.filter((r) => r.group === 'tool'));

const themeLabel = computed(
  () => ({ system: '跟随系统', light: '亮色', dark: '暗色' })[ui.theme],
);

function cycleTheme(): void {
  setTheme(ui.theme === 'system' ? 'light' : ui.theme === 'light' ? 'dark' : 'system');
}

onMounted(() => {
  applyTheme();
  // **草稿在这里恢复，不在「测试计划」页。** 它以前挂在 PlanView 的 onMounted
  // 上，于是不路过那一页就永远不恢复：刷新之后直接点「执行」，看到的是一份
  // 出厂默认计划，而右边导航的角标还显示着上次的分配数。
  loadDraft();
  void load().then(() => {
    // 没有草稿时，执行区的标量默认取自控制台基线；有草稿则让路。
    if (session.bootstrap) applyBootstrapDefaults(session.bootstrap);
  });
});
</script>

<template>
  <div class="app">
    <header class="app-header">
      <div class="brand">
        <span class="eyebrow">Ping · iperf3</span>
        <h1>CPE 子网测试控制台</h1>
      </div>
      <div class="header-status">
        <span class="version"><i :class="{ live: session.phase === 'connected' }"></i>{{ connectionLabel }}</span>
        <span class="version"><i :class="{ live: run.running }"></i>{{ runLabel }}</span>
        <button type="button" class="ghost" @click="cycleTheme">主题：{{ themeLabel }}</button>
      </div>
    </header>

    <div class="app-body">
      <nav class="rail" aria-label="控制台区域">
        <button
          v-for="region in flowRegions"
          :key="region.id"
          type="button"
          class="rail-item"
          :class="{ active: ui.region === region.id }"
          :aria-current="ui.region === region.id ? 'page' : undefined"
          @click="goto(region.id)"
        >
          <span class="rail-label">{{ region.label }}</span>
          <span v-if="badges[region.id]" class="rail-badge mono">{{ badges[region.id] }}</span>
        </button>
        <hr class="rail-sep" />
        <button
          v-for="region in toolRegions"
          :key="region.id"
          type="button"
          class="rail-item"
          :class="{ active: ui.region === region.id }"
          :aria-current="ui.region === region.id ? 'page' : undefined"
          @click="goto(region.id)"
        >
          <span class="rail-label">{{ region.label }}</span>
        </button>
      </nav>

      <main class="app-main">
        <div v-if="unauthorized" class="screen" data-label="AUTH · 口令失效" role="alert">
          控制台口令无效或已失效。<br />
          请用带 <code>?token=&lt;口令&gt;</code> 的完整地址重新打开这个页面。<br />
          <span class="dim">口令由主控启动时的 --ui-token 决定。</span>
        </div>
        <LocalView v-else-if="ui.region === 'local'" />
        <AgentView v-else-if="ui.region === 'agent'" />
        <PlanView v-else-if="ui.region === 'plan'" />
        <RunView v-else-if="ui.region === 'run'" />
        <ProgressView v-else-if="ui.region === 'progress'" />
        <MonitorView v-else-if="ui.region === 'monitor'" />
        <RunsView v-else-if="ui.region === 'runs'" />
        <div v-else class="screen" data-label="BUILD · 施工中">
          当前区域：{{ ui.region }}<br />
          这一区还没接上（按 .ai/DESIGN-v6.0-architecture.md §18 的 P3–P6 分期推进）。
        </div>
      </main>
    </div>
  </div>
</template>

<style scoped>
.app-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px 24px;
  flex-wrap: wrap;
  padding: 16px 24px 14px;
  border-bottom: 1px solid var(--line);
  box-shadow: inset 0 -3px 0 var(--accent);
}
.brand { display: flex; flex-direction: column; gap: 4px; }
.eyebrow {
  font: 600 11px/1 var(--fm);
  letter-spacing: .26em;
  color: var(--accent);
  text-transform: uppercase;
}
h1 { margin: 0; font-size: 21px; font-weight: 700; line-height: 1.2; letter-spacing: -.01em; }

.header-status { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
/* 右上状态读数：像仪表前面板的一小块液晶。 */
.version {
  display: inline-flex; align-items: center; gap: 9px;
  padding: 7px 12px;
  background: var(--screen-bg);
  border: 1px solid var(--bezel);
  border-radius: 5px;
  color: var(--screen-ink);
  font: 12px/1.3 var(--fm);
  font-variant-numeric: tabular-nums;
  box-shadow: inset 0 0 0 1px var(--bezel-hi);
}
.version i {
  width: 7px; height: 7px; border-radius: 50%; background: var(--screen-dim);
}
.version i.live { background: var(--signal); box-shadow: 0 0 0 2px var(--bezel-hi); }

.rail {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 14px 10px;
  border-right: 1px solid var(--line);
  background: var(--panel-2);
  overflow-y: auto;
}
@media (max-width: 860px) {
  .rail {
    flex-direction: row;
    flex-wrap: wrap;
    border-right: 0;
    border-bottom: 1px solid var(--line);
  }
  .rail-sep { display: none; }
}
.rail-item {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  width: 100%;
  padding: 9px 11px;
  text-align: left;
  color: var(--ink);
  background: transparent;
  border: 1px solid transparent;
  border-radius: 5px;
}
@media (max-width: 860px) { .rail-item { width: auto; } }
.rail-item:hover:not(:disabled) { background: var(--head); }
.rail-item.active {
  background: var(--surface);
  border-color: var(--line);
  box-shadow: inset 3px 0 0 var(--accent);
  font-weight: 600;
}
.rail-badge {
  font-size: 11.5px;
  color: var(--muted);
}
.rail-sep { width: 100%; margin: 8px 0; border: 0; border-top: 1px solid var(--line); }
</style>
