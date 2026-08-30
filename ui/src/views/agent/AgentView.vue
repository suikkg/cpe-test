<script setup lang="ts">
import { computed, ref } from 'vue';
import NicTable from '../../components/NicTable.vue';
import { agentHostname, agentNics, topologyReady } from '../../state/inventory';
import { connect, session } from '../../state/session';

/**
 * 「辅测机」：连接对端，拿到它的网卡表。
 *
 * 前缀过滤必须能在界面上改：默认只放行 `192.168.`，在 10.x / 172.x 的实验网里
 * 会把整张网卡表过滤成空——而控制台存在的意义就是让人不必回去手改 config.json。
 */
const prefixText = ref('');
const busy = computed(() => session.phase === 'connecting');

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

// 首次进入时用 bootstrap 回填的前缀填输入框。
if (prefixText.value === '' && session.prefixes.length > 0) {
  prefixText.value = session.prefixes.join(',');
}
</script>

<template>
  <section class="view">
    <header class="view-head">
      <h2>辅测机</h2>
      <p class="muted">连上对端，扫出双方网卡——之后才谈得上配对和灌包。</p>
    </header>

    <form class="form" @submit.prevent="onConnect">
      <label>
        <span>地址</span>
        <input v-model="session.host" type="text" placeholder="192.168.1.3" autocomplete="off" />
      </label>
      <label class="narrow">
        <span>端口</span>
        <input v-model.number="session.port" type="number" min="1" max="65535" />
      </label>
      <label>
        <span>共享令牌</span>
        <input
          v-model="session.token"
          type="text"
          placeholder="与 agent --token 一致"
          autocomplete="off"
        />
      </label>
      <label>
        <span>IPv4 前缀过滤</span>
        <input v-model="prefixText" type="text" placeholder="192.168.,10." autocomplete="off" />
      </label>
      <button type="submit" class="primary" :disabled="busy">
        {{ busy ? '连接中…' : '连接' }}
      </button>
    </form>

    <p v-if="session.phase === 'unauthorized'" class="bad" role="alert">
      控制台口令无效或已失效。请用带 <code>?token=</code> 的完整地址重新打开这个页面。
    </p>
    <p v-else-if="session.phase === 'failed' && session.error" class="bad" role="alert">
      {{ session.error }}
    </p>
    <p v-else-if="session.phase === 'connected'" class="ok">
      已连上 <strong>{{ agentHostname || session.host }}</strong
      >，扫到 {{ agentNics.length }} 块网卡。
    </p>

    <h3>对端网卡</h3>
    <NicTable
      :nics="agentNics"
      empty-hint="还没连上辅测机。填好地址和令牌点「连接」。"
    />

    <p v-if="session.phase === 'connected' && !topologyReady" class="warn" role="alert">
      对端连上了，但本机这边一块网卡都没扫到——前缀过滤可能把它们全滤掉了。
    </p>
  </section>
</template>

<style scoped>
.form {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-end;
  gap: 10px;
  margin: 0 0 16px;
  padding: 12px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--surface);
}
label {
  display: flex;
  flex-direction: column;
  gap: 4px;
  flex: 1 1 180px;
}
label.narrow {
  flex: 0 0 110px;
}
label span {
  font-size: 11.5px;
  color: var(--muted);
}
input {
  padding: 7px 9px;
  border: 1px solid var(--line);
  border-radius: 4px;
  background: var(--canvas);
  color: var(--ink);
  font: inherit;
  font-size: 13px;
}
input:focus-visible {
  outline: 2px solid var(--focus);
  outline-offset: 1px;
}
.primary {
  padding: 8px 18px;
  border: 1px solid var(--accent);
  border-radius: 4px;
  background: var(--accent);
  color: var(--on-accent);
  font: inherit;
  font-weight: 600;
  cursor: pointer;
}
.primary:disabled {
  opacity: 0.6;
  cursor: default;
}
.ok,
.bad,
.warn {
  margin: 0 0 16px;
  padding: 9px 12px;
  border-radius: 4px;
}
.ok {
  border-left: 3px solid var(--ok);
  background: var(--ok-bg);
}
.bad {
  border-left: 3px solid var(--bad);
  background: var(--bad-bg);
}
.warn {
  border-left: 3px solid var(--focus);
  background: var(--info-bg);
}
code {
  font-family: var(--fm);
}
</style>
