<script setup lang="ts">
import { computed, ref } from 'vue';
import NicTable from '../../components/NicTable.vue';
import { freshnessLabel } from '../../domain/freshness';
import { nicSearchFields } from '../../domain/nics';
import { filterByQuery } from '../../domain/search';
import { inner, scenarioBlocksActions } from '../../state/inner';
import { agentHostname, agentNics, masterHostname, masterNics } from '../../state/inventory';
import { run } from '../../state/run';
import { connect, load, rescan, session } from '../../state/session';
import { ui } from '../../state/ui';

/**
 * 「连接」：辅测机地址 + 两端网卡表。
 *
 * 旧版分成「本机」「辅测机」两页，两页各有一个「重新扫描」，而两张表本来就来自
 * 同一次扫描（连上之后本机那张读的是 `/api/connect` 回包里的 master）。合成一页
 * 之后只剩一个入口，行为仍由 `state/session::rescan` 决定：没连上只扫本机，
 * 连上了两端一起扫。
 */

// 未编辑时始终读取会话值，bootstrap 晚到也能回填；编辑中保留逗号和空格。
const prefixDraft = ref<string | null>(null);
const prefixText = computed({
  get: () => prefixDraft.value ?? session.prefixes.join(','),
  set: (value: string) => { prefixDraft.value = value; },
});
const showToken = ref(false);
const connecting = computed(() => session.phase === 'connecting');
const connected = computed(() => session.phase === 'connected');

/**
 * 正在跑测时锁住「连接」「重新扫描」和表单提交。
 *
 * 两者都发 `/api/connect`，会换掉辅测机身份和两端网卡表。后端有意不拦（执行线程
 * 起跑前已把 cfg 快照下来，正在跑的那一轮不会被改坏），所以这是 UX 门：保证
 * 「屏幕上写的就是正在测的」。子网、内环、组合场景任一在跑都算。
 */
const testInFlight = computed(() => run.running || inner.status.running || scenarioBlocksActions());
const locked = computed(() => connecting.value || session.scanning || testInFlight.value);
const lockHint = computed(() => (testInFlight.value ? '测试进行中，结束后才能修改连接或重新扫描' : undefined));

const iperf = computed(() => session.local?.iperf3 ?? null);
const localLoaded = computed(() => session.local !== null);
// 连接回包也包含主控网卡，工具信息未取到不应挡住这份有效拓扑。
const inventoryReady = computed(() => localLoaded.value || session.connection !== null);
const snapshotStamp = computed(() => freshnessLabel(session.connectedAt));
const prefixHint = computed(() => (session.prefixes.length ? session.prefixes.join('、') : '（无）'));

const masterShown = computed(() => filterByQuery(masterNics.value, ui.connect.query, nicSearchFields));
const agentShown = computed(() => filterByQuery(agentNics.value, ui.connect.query, nicSearchFields));

/** 空表要指向对的那一端：连上了而对端为空，是前缀过滤的问题，不是本机的问题。 */
const masterEmptyHint = computed(() => {
  if (ui.connect.query.trim() && masterNics.value.length) return `没有网卡匹配「${ui.connect.query.trim()}」。`;
  return connected.value ? `本机没有网卡的 IPv4 落在前缀 ${prefixHint.value} 内。` : '没有扫到网卡，检查网线或 Wi-Fi 后重新扫描。';
});
const agentEmptyHint = computed(() => {
  if (!connected.value && !session.connection) return '未连接辅测机。';
  if (ui.connect.query.trim() && agentNics.value.length) return `没有网卡匹配「${ui.connect.query.trim()}」。`;
  return `辅测机没有网卡的 IPv4 落在前缀 ${prefixHint.value} 内，修改前缀后重新连接。`;
});

function syncPrefixes(): void {
  session.prefixes = prefixText.value
    .split(',')
    .map((p) => p.trim())
    .filter((p) => p !== '');
}

async function onConnect(): Promise<void> {
  if (locked.value) return;
  syncPrefixes();
  await connect();
}

async function onRescan(): Promise<void> {
  if (locked.value) return;
  syncPrefixes();
  await rescan();
}
</script>

<template>
  <section class="view">
    <header class="page-head">
      <h2>连接</h2>
      <div class="actions">
        <button type="button" class="ghost" :disabled="locked" :title="lockHint" @click="onRescan">
          {{ session.scanning ? '扫描中…' : '重新扫描' }}
        </button>
      </div>
    </header>

    <form class="connect-form" @submit.prevent="onConnect">
      <label class="field host">
        <span>辅测机地址</span>
        <input v-model="session.host" type="text" placeholder="如 192.168.1.3" autocomplete="off" required />
      </label>
      <label class="field port">
        <span>端口</span>
        <input v-model.number="session.port" type="number" min="1" max="65535" required />
      </label>
      <div class="field token">
        <label for="agent-token">共享令牌</label>
        <div class="token-input">
          <input
            id="agent-token"
            v-model="session.token"
            :type="showToken ? 'text' : 'password'"
            placeholder="默认 cpetest；留空沿用当前"
            autocomplete="off"
          /><button
            type="button"
            class="ghost"
            :aria-pressed="showToken"
            aria-controls="agent-token"
            :aria-label="showToken ? '隐藏共享令牌' : '显示共享令牌'"
            @click="showToken = !showToken"
          >{{ showToken ? '隐藏' : '显示' }}</button>
        </div>
      </div>
      <label class="field prefix">
        <span>IPv4 前缀过滤</span>
        <input v-model="prefixText" type="text" placeholder="留空 = 全部；逗号分隔，如 192.168.,10." autocomplete="off" />
      </label>
      <button type="submit" class="primary" :disabled="locked" :title="lockHint">
        {{ connecting ? '连接中…' : '连接' }}
      </button>
    </form>
    <p class="hint">辅测机上运行 <code>cpe_test.exe agent</code>（或双击后选 3），填它能被本机访问到的 IP。</p>

    <p v-if="testInFlight" class="hint" role="status">测试进行中，连接与扫描已锁定。</p>
    <p v-if="session.scanMessage" class="msg" :class="session.scanKind" role="status">{{ session.scanMessage }}</p>
    <p v-if="session.phase === 'failed' && session.error" class="msg bad" role="alert">
      {{ session.error }}
      <template v-if="session.topologyStale">
        下面仍是上次成功连接 {{ session.connectedHost }} 时的网卡（{{ snapshotStamp }}）。
      </template>
    </p>
    <p v-else-if="session.topologyStale" class="msg warn" role="status">
      网卡列表仍是上次成功连接 {{ session.connectedHost }} 时的快照（{{ snapshotStamp }}），重新扫描成功后才会更新。
    </p>
    <p v-else-if="connected" class="msg ok" role="status">
      已连上 <strong>{{ agentHostname || session.connectedHost }}</strong>，本机 {{ masterNics.length }} 块 / 辅测 {{ agentNics.length }} 块网卡。
    </p>
    <div v-if="!localLoaded && session.localError" class="msg bad" role="alert">
      <p>本机信息加载失败：{{ session.localError }}</p>
      <button type="button" class="ghost small" @click="load">重新加载</button>
    </div>
    <p v-if="localLoaded && !iperf" class="msg warn" role="alert">
      本机未找到 iperf3：放到程序同目录或加入 PATH，再重新扫描。
    </p>

    <div class="toolbar">
      <label class="grow">
        <span class="sr-only">搜索网卡</span>
        <input
          type="search"
          :value="ui.connect.query"
          placeholder="搜接口名、描述、IP、角色"
          @input="ui.connect.query = ($event.target as HTMLInputElement).value"
        />
      </label>
      <span v-if="localLoaded" class="count">iperf3：{{ iperf || '未找到' }}</span>
    </div>

    <h3>主控 · {{ masterHostname || '本机' }} <span class="count">{{ inventoryReady ? `${masterNics.length} 块` : '读取中' }}</span></h3>
    <p v-if="!inventoryReady && !session.localError" class="hint" role="status">正在读取本机网卡…</p>
    <NicTable v-else-if="inventoryReady" label="主控网卡" :nics="masterShown" :empty-hint="masterEmptyHint" />

    <h3>辅测 · {{ agentHostname || session.connectedHost || '未连接' }} <span v-if="session.connection" class="count">{{ agentNics.length }} 块</span></h3>
    <NicTable label="辅测网卡" :nics="agentShown" :empty-hint="agentEmptyHint" />
  </section>
</template>

<style scoped>
.connect-form {
  display: grid;
  grid-template-columns: minmax(180px, 1.4fr) 100px minmax(200px, 1.2fr) minmax(180px, 1.4fr) auto;
  gap: 12px;
  align-items: end;
}
.connect-form input { width: 100%; }
.token-input { display: flex; }
.token-input input { flex: 1; border-top-right-radius: 0; border-bottom-right-radius: 0; }
.token-input button { border-top-left-radius: 0; border-bottom-left-radius: 0; margin-left: -1px; }
.connect-form + .hint { margin: 8px 0 0; }
.count { margin-left: 6px; color: var(--muted); font-size: 12.5px; font-weight: 400; }
h3 .count { font-size: 12px; }
@media (max-width: 1000px) {
  .connect-form { grid-template-columns: 1fr 100px; }
  .token, .prefix { grid-column: 1 / -1; }
  .connect-form > button { grid-column: 1 / -1; justify-self: start; }
}
</style>
