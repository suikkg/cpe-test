<script setup lang="ts">
import { computed, ref } from 'vue';
import NicTable from '../../components/NicTable.vue';
import { agentHostname, agentNics, masterNics } from '../../state/inventory';
import { freshnessLabel } from '../../domain/freshness';
import { connect, rescan, session } from '../../state/session';
import { goto } from '../../state/ui';

/**
 * 「辅测机」：连接对端，拿到它的网卡表。
 *
 * 前缀过滤必须能在界面上改：默认只放行 `192.168.`，在 10.x / 172.x 的实验网里
 * 会把整张网卡表过滤成空——而控制台存在的意义就是让人不必回去手改 config.json。
 */
// 未编辑时始终读取会话值，bootstrap 晚到也能回填；编辑中保留逗号和空格。
const prefixDraft = ref<string | null>(null);
const prefixText = computed({
  get: () => prefixDraft.value ?? session.prefixes.join(','),
  set: (value: string) => { prefixDraft.value = value; },
});
const showToken = ref(false);
const busy = computed(() => session.phase === 'connecting');
/** 旧快照标注用的时刻；从没连上过时是「尚未同步」，不填页面打开时间。 */
const connectedStamp = computed(() => freshnessLabel(session.connectedAt));
const connected = computed(() => session.phase === 'connected');

/**
 * 「连上了但对端一块网卡都没有」必须说成**辅测机**的问题。
 *
 * 这一页以前只有一句 `!topologyReady` 的提示，而 `topologyReady` 是「两端都
 * 有网卡」——对端为空时它同样为假，于是页面显示的是「本机这边一块网卡都没扫
 * 到」。人照着去查本机，本机好好的。空表的提示也一直停在「还没连上辅测机」，
 * 明明已经连上了。两处都在把责任指向错误的一端。
 */
const agentEmpty = computed(() => connected.value && agentNics.value.length === 0);
const masterEmpty = computed(() => connected.value && masterNics.value.length === 0);

const prefixHint = computed(() =>
  session.prefixes.length ? session.prefixes.join('、') : '（当前没有设前缀）',
);

const agentEmptyHint = computed(() =>
  connected.value
    ? '辅测机无对应网卡：对端已连上，但它没有一块网卡的 IPv4 落在当前前缀过滤里。'
    : '还没连上辅测机。填好地址和令牌点「连接」。',
);

function syncPrefixes(): void {
  session.prefixes = prefixText.value
    .split(',')
    .map((p) => p.trim())
    .filter((p) => p !== '');
}

async function onConnect(): Promise<void> {
  syncPrefixes();
  await connect();
}

async function onRescan(): Promise<void> {
  syncPrefixes();
  await rescan();
}
</script>

<template>
  <section class="view">
    <header class="view-head">
      <h2>辅测机</h2>
      <p class="muted">输入辅测机地址，连接后获取两端网卡，再配置测试计划。</p>
    </header>

    <form class="form" @submit.prevent="onConnect">
      <div class="form-heading"><h3>连接设置</h3><span class="hint">请先在对端启动辅测机服务</span></div>
      <label>
        <span>地址</span>
        <input v-model="session.host" type="text" placeholder="192.168.1.3" autocomplete="off" required />
      </label>
      <label class="narrow">
        <span>端口</span>
        <input v-model.number="session.port" type="number" min="1" max="65535" required />
      </label>
      <div class="token-field">
        <label for="agent-token">共享令牌</label>
        <div class="token-input"><input
          id="agent-token"
          v-model="session.token"
          :type="showToken ? 'text' : 'password'"
          placeholder="与 agent --token 一致"
          autocomplete="off"
        /><button type="button" class="ghost token-toggle" :aria-pressed="showToken" aria-controls="agent-token" :aria-label="showToken ? '隐藏共享令牌' : '显示共享令牌'" @click="showToken = !showToken">{{ showToken ? '隐藏' : '显示' }}</button></div>
      </div>
      <label class="prefix-field">
        <span>IPv4 前缀过滤</span>
        <input v-model="prefixText" type="text" placeholder="192.168.,10." autocomplete="off" aria-describedby="prefix-help" />
      </label>
      <div class="form-actions">
      <p id="prefix-help" class="hint">多个前缀用英文逗号分隔，对两端同时生效。留空显示全部网卡。</p>
      <button type="submit" class="primary" :disabled="busy || session.scanning">
        {{ busy ? '连接中…' : '连接' }}
      </button>
      </div>
    </form>

    <p v-if="session.phase === 'unauthorized'" class="bad" role="alert">
      控制台口令无效或已失效。请用带 <code>?token=</code> 的完整地址重新打开这个页面。
    </p>
    <p v-else-if="session.phase === 'failed' && session.error" class="bad" role="alert">
      {{ session.error }}
      <template v-if="session.topologyStale">
        <br />
        下面两张网卡表仍是<strong>上次成功</strong>的那一份（{{ session.connectedHost }}
        · {{ connectedStamp }}），不是这次请求的结果。
      </template>
    </p>
    <div v-else-if="connected && !agentEmpty" class="ok connection-success" role="status">
      <span>已连上 <strong>{{ agentHostname || session.connectedHost }}</strong>，扫到 {{ agentNics.length }} 块网卡。</span>
      <button type="button" class="ghost" @click="goto('plan')">配置测试计划</button>
    </div>

    <p v-if="agentEmpty" class="warn" role="alert">
      已连上 <strong>{{ agentHostname || session.connectedHost }}</strong>，但<strong>辅测机无对应网卡</strong>：
      它没有一块网卡的 IPv4 落在当前前缀过滤 <code>{{ prefixHint }}</code> 里。
      对端在 10.x / 172.x 这类网段时，把上面的「IPv4 前缀过滤」改掉再连一次。
    </p>

    <div class="bar">
      <h3>对端网卡</h3>
      <button
        type="button"
        class="ghost"
        :disabled="session.scanning || busy"
        title="沿用上面的地址、令牌和前缀，把两端的网卡重扫一遍"
        @click="onRescan"
      >
        {{ session.scanning ? '扫描中…' : '重新扫描' }}
      </button>
      <span v-if="session.scanMessage" class="scan" :class="session.scanKind" role="status">
        {{ session.scanMessage }}
      </span>
    </div>
    <NicTable :nics="agentNics" :empty-hint="agentEmptyHint" />

    <p v-if="masterEmpty" class="warn" role="alert">
      对端连上了，但<strong>本机</strong>这边一块网卡都没扫到——同一份前缀过滤
      <code>{{ prefixHint }}</code> 也作用在本机上。
    </p>
  </section>
</template>

<style scoped>
.form { display: grid; grid-template-columns: minmax(0, 2fr) 110px minmax(0, 2fr); align-items: end; gap: 20px 16px; margin: 0 0 20px; padding: 22px; border: 1px solid var(--line); border-radius: 9px; background: var(--panel-2); }
.form-heading { grid-column: 1 / -1; display: flex; align-items: baseline; gap: 14px; flex-wrap: wrap; }
.form-heading h3 { margin: 0; }
.form > label { display: flex; flex-direction: column; gap: 7px; min-width: 0; }
.form > label span, .token-field > label { font-size: 12px; font-weight: 600; color: var(--ink); }
input { width: 100%; padding: 9px 11px; border: 1px solid var(--line); border-radius: 5px; background: var(--surface); color: var(--ink); font: inherit; font-size: 13px; }
.token-field { display: grid; gap: 7px; min-width: 0; }
.token-input { display: flex; gap: 6px; }
.token-toggle { font-size: 12px; padding: 7px 10px; flex-shrink: 0; }
.prefix-field { grid-column: 1 / -1; }
.form-actions { grid-column: 1 / -1; display: flex; align-items: center; justify-content: space-between; gap: 16px; border-top: 1px solid var(--line); padding-top: 16px; }
.form-actions p { margin: 0; }
.form-actions button { min-width: 110px; flex-shrink: 0; }
.ok, .bad, .warn { margin: 0 0 20px; padding: 12px 16px; border-radius: 5px; }
.ok { border-left: 3px solid var(--ok); background: var(--ok-bg); }
.bad { border-left: 3px solid var(--bad); background: var(--bad-bg); }
.warn { border-left: 3px solid var(--warn); background: var(--info-bg); }
.connection-success { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
.connection-success button { font-size: 12px; }
code { font-family: var(--fm); overflow-wrap: anywhere; }
.bar { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; margin: 26px 0 14px; }
.bar h3 { margin: 0; margin-right: auto; }
.scan { font-size: 12px; color: var(--muted); }
.scan.ok, .scan.bad { margin: 0; padding: 0; border: 0; background: transparent; }
.scan.ok { color: var(--ok); }
.scan.bad { color: var(--bad); }
@media (max-width: 900px) {
  .form { grid-template-columns: minmax(0, 1fr) 110px; }
  .token-field { grid-column: 1 / -1; }
}
@media (max-width: 480px) {
  .form { padding: 16px; grid-template-columns: minmax(0, 1fr) 85px; gap: 16px 12px; }
  .form-actions { flex-direction: column; align-items: stretch; gap: 12px; }
}
</style>
