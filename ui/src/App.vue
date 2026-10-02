<script setup lang="ts">
import { computed, onMounted } from 'vue';
import { REGIONS, ui, goto, setTheme, applyTheme } from './state/ui';
import type { RegionId } from './state/ui';
import { agentNics, masterNics } from './state/inventory';
import { applyBootstrapDefaults, loadDraft, plan } from './state/plan';
import { selectedPortPairs } from './domain/plan-ports';
import { run, view as runView, syncStatus } from './state/run';
import { load, session } from './state/session';
import ConnectView from './views/connect/ConnectView.vue';
import PlanView from './views/plan/PlanView.vue';
import RunView from './views/run/RunView.vue';
import MonitorView from './views/monitor/MonitorView.vue';
import InnerView from './views/inner/InnerView.vue';
import HistoryView from './views/runs/HistoryView.vue';
import { inner, loadInnerDraft, syncInnerStatus, syncScenarioStatus } from './state/inner';

// 各区域的实时角标：导航栏说的是**现在是什么情况**，不是「你应该走到第几步」。
const badges = computed<Partial<Record<RegionId, string>>>(() => {
  const pairs = selectedPortPairs(plan.ui);
  const nics = masterNics.value.length + agentNics.value.length;
  return {
    connect: nics ? `${masterNics.value.length}+${agentNics.value.length}` : '',
    plan: pairs ? `${pairs} 对` : '',
    run: run.running ? `${runView.value.done}/${runView.value.total}` : plan.preview ? `${plan.preview.units.length} 单元` : '',
    inner: inner.status.running ? `${inner.status.completed}/${inner.status.total}` : '',
  };
});

/** 口令失效是**全局终态**：没有口令时点什么都是 401，不该让人逐页去撞。 */
const unauthorized = computed(() => session.phase === 'unauthorized');
// 顶栏说的是**已经连上的那台**，不是地址栏里正在敲的那个。
const connectionLabel = computed(() => {
  if (session.phase === 'connecting') return '连接中';
  if (session.phase === 'connected') return `已连 ${session.connectedHost || '辅测机'}`;
  if (session.connectedHost) return `与 ${session.connectedHost} 的连接已断开`;
  if (session.phase === 'failed') return '辅测机未连接';
  return '辅测机未连接';
});

/**
 * 顶栏的运行状态，以及点它去哪一页。
 *
 * 组合场景的子网阶段要指向「执行」：跳过和停止在那里，进度数字也在那里。
 * 只写「组合场景运行中」并跳去内环页，操作员就得自己去找子网进度。
 * 「还没读到」和「读到了，是空闲」必须分开：真空闲可以开跑，没读到时开跑
 * 就是往一轮已经在跑的测试上再叠一轮。
 */
const runStatus = computed<{ label: string; target: RegionId; live: boolean }>(() => {
  const subnet = `${runView.value.done}/${runView.value.total}`;
  if (inner.scenarioStartPhase === 'unknown') return { label: '启动结果未确认', target: 'inner', live: false };
  if (inner.scenarioStartPhase === 'sending') return { label: '正在启动组合场景', target: 'inner', live: false };
  if (inner.scenario.running) {
    return run.running
      ? { label: `组合场景 · 子网 ${subnet}`, target: 'run', live: true }
      : { label: `组合场景 · 内环 ${inner.status.completed}/${inner.status.total}`, target: 'inner', live: true };
  }
  if (inner.status.running) return { label: `内环 ${inner.status.completed}/${inner.status.total}`, target: 'inner', live: true };
  if (!run.synced) return { label: '运行状态待同步', target: 'run', live: false };
  if (run.running) return { label: `运行中 ${subnet}`, target: 'run', live: true };
  if (runView.value.finished) return { label: '本轮已结束', target: 'run', live: false };
  return { label: '空闲', target: 'run', live: false };
});

const navigationGroups = [
  { label: '子网测试', regions: REGIONS.filter((r) => r.group === 'flow') },
  { label: '内环测试', regions: REGIONS.filter((r) => r.group === 'inner') },
  { label: '工具', regions: REGIONS.filter((r) => r.group === 'tool') },
];
const regionIcons: Record<RegionId, string> = {
  connect: 'M5 3h14v6H5z M5 15h14v6H5z M12 9v6 M8 6h.01 M8 18h.01',
  plan: 'M9 5h12 M9 12h12 M9 19h12 M3 4h2v2H3z M3 11h2v2H3z M3 18h2v2H3z',
  run: 'M7 3l14 9-14 9z',
  inner: 'M5 7h14 M15 3l4 4-4 4 M19 17H5 M9 13l-4 4 4 4',
  monitor: 'M2 12h4l3-8 5 16 3-8h5',
  history: 'M4 5v5h5 M4 10a9 9 0 1 1 0 5 M12 7v5l3 2',
};

const themeLabel = computed(
  () => ({ system: '跟随系统', light: '亮色', dark: '暗色' })[ui.theme],
);

function cycleTheme(): void {
  setTheme(ui.theme === 'system' ? 'light' : ui.theme === 'light' ? 'dark' : 'system');
}

onMounted(() => {
  applyTheme();
  // **草稿在这里恢复，不在「计划」页。** 挂在某一页上的话，不路过那一页就
  // 永远不恢复：刷新之后直接点「执行」，看到的是一份出厂默认计划。
  loadDraft();
  loadInnerDraft();
  void syncInnerStatus();
  void syncScenarioStatus();
  // 先认一次「服务器上是不是已经有一轮在跑」。走的是轮询那同一个出口，
  // 不新开第二条链、不提高频率；读到在跑才把轮询接上。
  void syncStatus();
  void load().then(() => {
    // 没有草稿时，执行区的标量默认取自控制台基线；有草稿则让路。
    if (session.bootstrap) applyBootstrapDefaults(session.bootstrap);
  });
});
</script>

<template>
  <div class="app">
    <a class="skip-link" href="#main-content">跳转到主要内容</a>
    <header class="app-header">
      <div class="brand">
        <span class="brand-mark" aria-hidden="true">CPE</span>
        <h1>CPE 测试控制台</h1>
        <small v-if="session.local?.version" class="version mono">v{{ session.local.version }}</small>
      </div>
      <div class="header-status">
        <span class="status-readout" :class="{ connected: session.phase === 'connected' }">
          <i aria-hidden="true"></i>{{ connectionLabel }}
        </span>
        <button type="button" class="run-status" :class="{ live: runStatus.live }" @click="goto(runStatus.target)">
          <i aria-hidden="true"></i>{{ runStatus.label }}
        </button>
        <button type="button" class="ghost theme-toggle" :title="`主题：${themeLabel}`" :aria-label="`切换主题，当前${themeLabel}`" @click="cycleTheme">
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3a9 9 0 1 0 0 18z"/><circle cx="12" cy="12" r="9"/></svg>
        </button>
      </div>
    </header>

    <div class="app-body">
      <nav class="rail" aria-label="控制台区域">
        <div v-for="group in navigationGroups" :key="group.label" class="rail-group">
          <p v-if="group.regions.length > 1" class="rail-heading">{{ group.label }}</p>
          <button
            v-for="region in group.regions"
            :key="region.id"
            type="button"
            class="rail-item"
            :class="{ active: ui.region === region.id }"
            :aria-current="ui.region === region.id ? 'page' : undefined"
            @click="goto(region.id)"
          >
            <svg class="rail-icon" viewBox="0 0 24 24" aria-hidden="true"><path :d="regionIcons[region.id]"/></svg>
            <span class="rail-label">{{ region.label }}</span>
            <span v-if="badges[region.id]" class="rail-badge">{{ badges[region.id] }}</span>
          </button>
        </div>
      </nav>

      <main id="main-content" class="app-main" tabindex="-1">
        <div v-if="unauthorized" class="screen" data-label="口令失效" role="alert">
          控制台口令无效或已失效。<br />
          请用带 <code>?token=&lt;口令&gt;</code> 的完整地址重新打开这个页面。<br />
          <span class="dim">口令由主控启动时的 --ui-token 决定。</span>
        </div>
        <ConnectView v-else-if="ui.region === 'connect'" />
        <PlanView v-else-if="ui.region === 'plan'" />
        <RunView v-else-if="ui.region === 'run'" />
        <MonitorView v-else-if="ui.region === 'monitor'" />
        <HistoryView v-else-if="ui.region === 'history'" />
        <InnerView v-else-if="ui.region === 'inner'" :subnet-running="run.running" @show-subnet-progress="goto('run')" />
      </main>
    </div>
  </div>
</template>

<style scoped>
.app-header {
  display: flex; align-items: center; justify-content: space-between;
  gap: 12px 24px; flex-wrap: wrap;
  padding: 12px 24px; border-bottom: 1px solid var(--line);
  background: var(--surface);
}
.brand { display: flex; align-items: center; gap: 10px; min-width: 0; }
.brand-mark {
  display: grid; place-items: center; flex: 0 0 34px; height: 34px;
  background: var(--accent); color: var(--on-accent); border-radius: 7px;
  font-size: 12px; font-weight: 750; letter-spacing: -.04em;
}
h1 { margin: 0; font-size: 16px; font-weight: 700; line-height: 1.4; white-space: nowrap; }
.version { color: var(--muted); font-size: 11px; }
.header-status { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; min-width: 0; }
.status-readout, .run-status {
  display: inline-flex; align-items: center; gap: 7px;
  font-size: 12px; font-weight: 500; color: var(--muted);
}
.status-readout { overflow-wrap: anywhere; }
.status-readout i, .run-status i {
  width: 6px; height: 6px; border-radius: 50%; background: var(--muted); flex: 0 0 6px;
}
.status-readout.connected { color: var(--ok); }
.status-readout.connected i { background: var(--ok); }
.run-status { min-height: 32px; padding: 5px 10px; background: var(--panel-2); border-color: var(--line); }
.run-status:hover:not(:disabled) { background: var(--head); }
.run-status.live { background: var(--screen-bg); color: var(--signal); border-color: var(--bezel); }
.run-status.live i { background: var(--signal); }
.theme-toggle { display: inline-grid; place-items: center; min-height: 32px; padding: 5px 8px; }
.theme-toggle svg { width: 16px; height: 16px; fill: none; stroke: currentColor; stroke-width: 1.5; }
.theme-toggle path { fill: currentColor; stroke: none; }
.rail {
  display: flex; flex-direction: column; gap: 18px;
  padding: 20px 10px 16px; border-right: 1px solid var(--line);
  background: var(--panel-2); overflow-y: auto;
}
.rail-group { display: grid; gap: 2px; }
.rail-heading { margin: 0 10px 6px; color: var(--muted); font-size: 11px; }
.rail-item {
  display: flex; align-items: center; gap: 10px; width: 100%;
  padding: 9px 10px; text-align: left; color: var(--muted);
  background: transparent; border: 1px solid transparent; border-radius: 7px;
  position: relative;
}
.rail-item:hover:not(:disabled) { background: var(--head); color: var(--ink); }
.rail-item.active { background: var(--surface); border-color: var(--line); color: var(--accent); }
.rail-item.active::before {
  content: ''; position: absolute; left: -1px; top: 10px; bottom: 10px;
  width: 3px; border-radius: 2px; background: var(--accent);
}
.rail-icon { width: 17px; height: 17px; flex: 0 0 17px; fill: none; stroke: currentColor; stroke-width: 1.6; stroke-linecap: round; stroke-linejoin: round; }
.rail-label { font-size: 13px; font-weight: 600; color: var(--ink); white-space: nowrap; }
.active .rail-label { color: var(--accent); }
.rail-badge {
  margin-left: auto; font-size: 10.5px; font-weight: 500; color: var(--muted); white-space: nowrap;
  font-variant-numeric: tabular-nums;
}
@media (max-width: 1100px) {
  .rail-badge { display: none; }
}
@media (max-width: 760px) {
  .app-header { padding: 10px 16px; gap: 8px; }
  .header-status { width: 100%; gap: 8px; }
  .status-readout { flex: 1; }
  .rail { flex-direction: row; gap: 8px; padding: 8px 12px; overflow-x: auto; border-right: 0; border-bottom: 1px solid var(--line); }
  .rail-group { display: flex; gap: 4px; flex: 0 0 auto; }
  .rail-group + .rail-group { padding-left: 8px; border-left: 1px solid var(--line); }
  .rail-heading { display: none; }
  .rail-item { width: auto; gap: 7px; padding: 8px 12px; }
  .rail-item.active::before { top: auto; bottom: -1px; left: 12px; right: 12px; height: 2px; width: auto; }
  .rail-icon { width: 16px; height: 16px; flex-basis: 16px; }
}
</style>
