<script setup lang="ts">
import { computed, ref } from 'vue';
import RateChart from '../../components/RateChart.vue';
import {
  isMonitored,
  MONITOR_INTERVALS,
  MONITOR_MAX_SESSIONS,
  pendingStarts,
  type MonitorSide,
} from '../../domain/monitor-plan';
import { agentNics, masterNics } from '../../state/inventory';
import { monitor, startAll, startSession, stopAll, stopSession } from '../../state/monitor';

/**
 * 「监控」：独立于一轮测试的网卡速率观测。
 *
 * 和测试执行**正交**——开跑之前肉眼确认「这块网卡现在有没有流量、跑在什么
 * 量级」，跑起来之后盯着看也完全合理。所以它不受 running 约束。
 */
const side = ref<MonitorSide>('master');
const iface = ref('');
/**
 * 采样间隔。以前写死 1000ms，而这两件事都需要它可调：看毫秒级突发要 200ms，
 * 盯一整轮 11.5 小时的趋势要 5s（点数上限 7200，1s 只能存两小时）。
 */
const intervalMs = ref(1000);

const nics = computed(() => (side.value === 'master' ? masterNics.value : agentNics.value));

/** 已经在监控的那些。同一块网卡开两路读的是同一个内核计数器，纯粹浪费名额。 */
const running = computed(() => monitor.sessions.map((s) => ({ side: s.side, iface: s.iface })));

function taken(name: string): boolean {
  return isMonitored(running.value, side.value, name);
}

/** 「全部开始」这一下实际会开几块——按钮上直接写出来，免得点了才知道被上限截断。 */
const pending = computed(() =>
  pendingStarts(
    running.value,
    side.value,
    nics.value.map((nic) => nic.name),
  ),
);

const full = computed(() => monitor.sessions.length >= MONITOR_MAX_SESSIONS);
const selectable = computed(() => nics.value.filter((nic) => !taken(nic.name)).length);

async function add(): Promise<void> {
  if (!iface.value) return;
  if (await startSession(side.value, iface.value, intervalMs.value)) {
    // 开成了就把选择清掉：那一项马上会变成「已在监控」，留着只会让人再点一次。
    iface.value = '';
  }
}

async function addAll(): Promise<void> {
  await startAll(
    side.value,
    nics.value.map((nic) => nic.name),
    intervalMs.value,
  );
  iface.value = '';
}
</script>

<template>
  <section class="view">
    <header class="view-head">
      <h2>监控</h2>
      <p class="muted">
        独立的网卡速率观测，和一轮测试正交——开跑前确认量级，跑起来盯着看也行。
      </p>
    </header>

    <div class="bar">
      <label>
        <span>端</span>
        <select v-model="side">
          <option value="master">主控</option>
          <option value="agent">辅测</option>
        </select>
      </label>
      <label class="grow">
        <span>网卡</span>
        <select v-model="iface">
          <option value="">选一块网卡</option>
          <option
            v-for="nic in nics"
            :key="nic.name"
            :value="nic.name"
            :disabled="taken(nic.name)"
          >
            {{ nic.name }}（{{ nic.role || 'UNKNOWN' }}）{{ taken(nic.name) ? ' · 已在监控' : '' }}
          </option>
        </select>
      </label>
      <label>
        <span>采样间隔</span>
        <select v-model.number="intervalMs">
          <option v-for="option in MONITOR_INTERVALS" :key="option.ms" :value="option.ms">
            {{ option.label }}
          </option>
        </select>
      </label>
      <button
        type="button"
        class="primary"
        :disabled="!iface || monitor.starting || full"
        @click="add"
      >
        {{ monitor.starting ? '启动中…' : '开始监控' }}
      </button>
      <button
        type="button"
        class="ghost"
        :disabled="pending.length === 0 || monitor.starting"
        :title="`把${side === 'master' ? '主控' : '辅测'}这一端还没开的网卡一次开起来`"
        @click="addAll"
      >
        全部开始<template v-if="pending.length">（{{ pending.length }} 块）</template>
      </button>
      <button v-if="monitor.sessions.length" type="button" class="ghost" @click="stopAll">
        全部停止
      </button>
    </div>

    <p class="muted hint">
      同时最多 {{ MONITOR_MAX_SESSIONS }} 路（当前 {{ monitor.sessions.length }} 路）。
      同一端的同一块网卡不重复开——两条曲线读的是同一个内核计数器，必然一模一样。
      <template v-if="full"><strong>已达上限，先停掉一路再开。</strong></template>
      <template v-else-if="selectable === 0 && nics.length">这一端的网卡都已经在监控了。</template>
    </p>

    <p v-if="monitor.error" class="bad" role="alert">{{ monitor.error }}</p>

    <div v-if="monitor.sessions.length === 0" class="empty">
      还没有在跑的监控。选一块网卡开始。
    </div>

    <div v-for="s in monitor.sessions" :key="s.session" class="panel">
      <div class="panel-head">
        <strong>{{ s.side === 'master' ? '主控' : '辅测' }} · {{ s.iface }}</strong>
        <span v-if="s.error" class="err">{{ s.error }}</span>
        <span v-else-if="!s.running" class="muted">已停止</span>
        <button type="button" class="ghost small" @click="stopSession(s.session)">停止</button>
      </div>
      <RateChart :points="s.points" :interval-ms="s.intervalMs" />
    </div>
  </section>
</template>

<style scoped>
.bar {
  display: flex; flex-wrap: wrap; align-items: flex-end; gap: 10px;
  margin: 0 0 10px; padding: 12px;
  border: 1px solid var(--line); border-radius: 6px; background: var(--surface);
}
label { display: flex; flex-direction: column; gap: 4px; }
label.grow { flex: 1 1 220px; }
label span { font-size: 11.5px; color: var(--muted); }
select {
  padding: 7px 9px; border: 1px solid var(--line); border-radius: 4px;
  background: var(--canvas); color: var(--ink); font: inherit; font-size: 13px;
}
select option:disabled { color: var(--muted); }
.primary { padding: 8px 16px; border: 1px solid var(--accent); border-radius: 4px;
  background: var(--accent); color: var(--on-accent); font: inherit; font-weight: 600; cursor: pointer; }
.ghost { padding: 7px 14px; border: 1px solid var(--line); border-radius: 4px;
  background: var(--surface); color: var(--ink); font: inherit; cursor: pointer; }
.ghost.small { padding: 3px 10px; font-size: 12px; }
.primary:disabled, .ghost:disabled { opacity: .55; cursor: default; }
.hint { margin: 0 0 14px; font-size: 12px; }
.panel { margin: 0 0 14px; }
.panel-head { display: flex; align-items: center; gap: 10px; margin: 0 0 6px; }
.panel-head .err { color: var(--bad); font-size: 12px; }
.panel-head button { margin-left: auto; }
.empty { padding: 14px 16px; border: 1px dashed var(--line); border-radius: 6px; color: var(--muted); background: var(--panel-2); }
.bad { margin: 0 0 12px; padding: 9px 12px; border-left: 3px solid var(--bad); background: var(--bad-bg); }
.muted { color: var(--muted); }
</style>
