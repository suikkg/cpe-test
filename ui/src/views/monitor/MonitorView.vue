<script setup lang="ts">
import { computed, ref } from 'vue';
import RateChart from '../../components/RateChart.vue';
import { agentNics, masterNics } from '../../state/inventory';
import { monitor, startSession, stopAll, stopSession } from '../../state/monitor';

/**
 * 「监控」：独立于一轮测试的网卡速率观测。
 *
 * 和测试执行**正交**——开跑之前肉眼确认「这块网卡现在有没有流量、跑在什么
 * 量级」，跑起来之后盯着看也完全合理。所以它不受 running 约束。
 */
const side = ref<'master' | 'agent'>('master');
const iface = ref('');

const nics = computed(() => (side.value === 'master' ? masterNics.value : agentNics.value));

async function add(): Promise<void> {
  if (!iface.value) return;
  await startSession(side.value, iface.value);
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
          <option v-for="nic in nics" :key="nic.name" :value="nic.name">
            {{ nic.name }}（{{ nic.role || 'UNKNOWN' }}）
          </option>
        </select>
      </label>
      <button type="button" class="primary" :disabled="!iface || monitor.starting" @click="add">
        {{ monitor.starting ? '启动中…' : '开始监控' }}
      </button>
      <button v-if="monitor.sessions.length" type="button" class="ghost" @click="stopAll">
        全部停止
      </button>
    </div>

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
      <RateChart :points="s.points" />
    </div>
  </section>
</template>

<style scoped>
.bar {
  display: flex; flex-wrap: wrap; align-items: flex-end; gap: 10px;
  margin: 0 0 14px; padding: 12px;
  border: 1px solid var(--line); border-radius: 6px; background: var(--surface);
}
label { display: flex; flex-direction: column; gap: 4px; }
label.grow { flex: 1 1 220px; }
label span { font-size: 11.5px; color: var(--muted); }
select {
  padding: 7px 9px; border: 1px solid var(--line); border-radius: 4px;
  background: var(--canvas); color: var(--ink); font: inherit; font-size: 13px;
}
.primary { padding: 8px 16px; border: 1px solid var(--accent); border-radius: 4px;
  background: var(--accent); color: var(--on-accent); font: inherit; font-weight: 600; cursor: pointer; }
.ghost { padding: 7px 14px; border: 1px solid var(--line); border-radius: 4px;
  background: var(--surface); color: var(--ink); font: inherit; cursor: pointer; }
.ghost.small { padding: 3px 10px; font-size: 12px; }
.primary:disabled { opacity: .55; cursor: default; }
.panel { margin: 0 0 14px; }
.panel-head { display: flex; align-items: center; gap: 10px; margin: 0 0 6px; }
.panel-head .err { color: var(--bad); font-size: 12px; }
.panel-head button { margin-left: auto; }
.empty { padding: 14px 16px; border: 1px dashed var(--line); border-radius: 6px; color: var(--muted); background: var(--panel-2); }
.bad { margin: 0 0 12px; padding: 9px 12px; border-left: 3px solid var(--bad); background: var(--bad-bg); }
</style>
