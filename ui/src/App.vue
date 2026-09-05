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

const navigationGroups = [
  { label: '测试流程', regions: REGIONS.filter((r) => r.group === 'flow') },
  { label: '工具与记录', regions: REGIONS.filter((r) => r.group === 'tool') },
];
const regionDescriptions: Record<RegionId, string> = {
  local: '网卡与工具检查', agent: '连接与双端扫描', plan: '链路、配置与套件',
  run: '预览并开始测试', progress: '当前任务与日志', monitor: '实时网卡吞吐', runs: '报告与重新执行',
};
const regionIcons: Record<RegionId, string> = {
  local: 'M3 4h18v12H3z M8 21h8 M12 16v5',
  agent: 'M5 3h14v6H5z M5 15h14v6H5z M12 9v6 M8 6h.01 M8 18h.01',
  plan: 'M9 5h12 M9 12h12 M9 19h12 M3 4h2v2H3z M3 11h2v2H3z M3 18h2v2H3z',
  run: 'M7 3l14 9-14 9z',
  progress: 'M3 3v18h18 M7 16v-5 M12 16V6 M17 16v-8',
  monitor: 'M2 12h4l3-8 5 16 3-8h5',
  runs: 'M4 5v5h5 M4 10a9 9 0 1 1 0 5 M12 7v5l3 2',
};

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
    <a class="skip-link" href="#main-content">跳转到主要内容</a>
    <header class="app-header">
      <div class="brand">
        <span class="brand-mark" aria-hidden="true">CPE</span>
        <div>
          <h1>CPE 子网测试控制台</h1>
          <p>双机链路测试 <span>Ping / iperf3 / ctsTraffic</span></p>
        </div>
      </div>
      <div class="header-status">
        <span class="status-readout" :class="{ connected: session.phase === 'connected' }">
          <i aria-hidden="true"></i>{{ connectionLabel }}
        </span>
        <button type="button" class="run-status" :class="{ live: run.running }" @click="goto('progress')">
          <i aria-hidden="true"></i>{{ runLabel }}
        </button>
        <button type="button" class="ghost theme-toggle" :aria-label="`切换主题，当前${themeLabel}`" @click="cycleTheme">
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3a9 9 0 1 0 0 18z"/><circle cx="12" cy="12" r="9"/></svg>
          <span>{{ themeLabel }}</span>
        </button>
      </div>
    </header>

    <div class="app-body">
      <nav class="rail" aria-label="控制台区域">
        <div v-for="group in navigationGroups" :key="group.label" class="rail-group">
          <p class="rail-heading">{{ group.label }}</p>
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
            <span class="rail-copy">
              <span class="rail-label">{{ region.label }}</span>
              <small>{{ regionDescriptions[region.id] }}</small>
            </span>
            <span v-if="badges[region.id]" class="rail-badge">{{ badges[region.id] }}</span>
          </button>
        </div>
        <div class="rail-footer">
          <span>双机测试工作台</span>
          <small v-if="session.local?.version">v{{ session.local.version }}</small>
        </div>
      </nav>

      <main id="main-content" class="app-main" tabindex="-1">
        <div v-if="unauthorized" class="screen" data-label="口令失效" role="alert">
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
      </main>
    </div>
  </div>
</template>

<style scoped>
.app-header {
  display: flex; align-items: center; justify-content: space-between;
  gap: 16px 24px; flex-wrap: wrap;
  padding: 18px 26px; border-bottom: 1px solid var(--line);
  background: var(--surface);
}
.brand { display: flex; align-items: center; gap: 13px; min-width: 0; }
.brand-mark {
  display: grid; place-items: center; flex: 0 0 46px; height: 46px;
  background: var(--accent); color: var(--on-accent); border-radius: 9px;
  font-size: 15px; font-weight: 750; letter-spacing: -.04em;
}
h1 { margin: 0; font-size: 18px; font-weight: 700; line-height: 1.4; }
.brand p { margin: 3px 0 0; font-size: 11px; color: var(--muted); }
.brand p span { margin-left: 8px; }
.header-status { display: flex; align-items: center; gap: 16px; flex-wrap: wrap; min-width: 0; }
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
.run-status { padding: 7px 10px; background: var(--panel-2); border-color: var(--line); }
.run-status:hover:not(:disabled) { background: var(--head); }
.run-status.live { background: var(--screen-bg); color: var(--signal); border-color: var(--bezel); }
.run-status.live i { background: var(--signal); }
.theme-toggle { display: inline-flex; align-items: center; gap: 7px; font-size: 12px; font-weight: 500; }
.theme-toggle svg { width: 16px; height: 16px; fill: none; stroke: currentColor; stroke-width: 1.5; }
.theme-toggle path { fill: currentColor; stroke: none; }
.rail {
  display: flex; flex-direction: column; gap: 24px;
  padding: 24px 12px 16px; border-right: 1px solid var(--line);
  background: var(--panel-2); overflow-y: auto;
}
.rail-group { display: grid; gap: 4px; }
.rail-heading { margin: 0 12px 8px; color: var(--muted); font-size: 11px; }
.rail-item {
  display: flex; align-items: center; gap: 10px; width: 100%;
  padding: 12px 10px; text-align: left; color: var(--muted);
  background: transparent; border: 1px solid transparent; border-radius: 7px;
  position: relative;
}
.rail-item:hover:not(:disabled) { background: var(--head); color: var(--ink); }
.rail-item.active { background: var(--surface); border-color: var(--line); color: var(--accent); }
.rail-item.active::before {
  content: ''; position: absolute; left: -1px; top: 14px; bottom: 14px;
  width: 3px; border-radius: 2px; background: var(--accent);
}
.rail-icon { width: 18px; height: 18px; flex: 0 0 18px; fill: none; stroke: currentColor; stroke-width: 1.6; stroke-linecap: round; stroke-linejoin: round; }
.rail-copy { display: grid; gap: 3px; min-width: 0; }
.rail-label { font-size: 13px; font-weight: 600; color: var(--ink); white-space: nowrap; }
.active .rail-label { color: var(--accent); }
.rail-copy small { color: var(--muted); font-size: 10px; font-weight: 400; white-space: nowrap; }
.rail-badge {
  margin-left: auto; align-self: flex-start; margin-top: 1px;
  font-size: 10px; font-weight: 500; color: var(--muted); white-space: nowrap;
  font-variant-numeric: tabular-nums;
}
.rail-footer { margin-top: auto; padding: 12px 10px 0; border-top: 1px solid var(--line); display: grid; gap: 4px; color: var(--muted); font-size: 11px; }
.rail-footer small { font-variant-numeric: tabular-nums; }
@media (max-width: 1100px) {
  .header-status { gap: 8px; }
  .rail-badge { display: none; }
}
@media (max-width: 760px) {
  .app-header { padding: 14px 16px 12px; gap: 12px; }
  .brand-mark { flex-basis: 38px; height: 38px; font-size: 13px; }
  h1 { font-size: 16px; }
  .brand p span { display: none; }
  .header-status { width: 100%; gap: 10px; }
  .status-readout { flex: 1; }
  .theme-toggle { padding-inline: 9px; }
  .rail { flex-direction: row; gap: 8px; padding: 8px 12px; overflow-x: auto; border-right: 0; border-bottom: 1px solid var(--line); }
  .rail-group { display: flex; gap: 4px; flex: 0 0 auto; }
  .rail-group + .rail-group { padding-left: 8px; border-left: 1px solid var(--line); }
  .rail-heading, .rail-copy small, .rail-footer { display: none; }
  .rail-item { width: auto; gap: 7px; padding: 9px 12px; }
  .rail-item.active::before { top: auto; bottom: -1px; left: 12px; right: 12px; height: 2px; width: auto; }
  .rail-icon { width: 16px; height: 16px; flex-basis: 16px; }
}
</style>
