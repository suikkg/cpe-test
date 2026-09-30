<script setup lang="ts">
import { scenarioBlocksActions } from '../../state/inner';
import { computed, nextTick, ref } from 'vue';
import NicTable from '../../components/NicTable.vue';
import type { NicInfo } from '../../api/dto';
import { nicKey, nicLinkLocal, nicSearchFields, nicSpeedLabel } from '../../domain/nics';
import { filterByQuery, visibleCountLabel } from '../../domain/search';
import { agentHostname, agentNics, masterHostname, masterNics } from '../../state/inventory';
import { load, rescan, session } from '../../state/session';
import { inner } from '../../state/inner';
import { run } from '../../state/run';
import { freshnessLabel } from '../../domain/freshness';
import { goto, ui } from '../../state/ui';

/** 本机信息独立于辅测机连接；尚未返回时不能把未知工具状态当成缺失。 */
const iperf = computed(() => session.local?.iperf3 ?? null);
const loaded = computed(() => session.local !== null);
const connected = computed(() => session.phase === 'connected');
const testInFlight = computed(() => run.running || inner.status.running || scenarioBlocksActions());
const scanLocked = computed(() => session.scanning || session.phase === 'connecting' || testInFlight.value);
const scanLockHint = computed(() => testInFlight.value
  ? '测试进行中，请等待结束后再重新扫描网卡。' : undefined);
async function onRescan(): Promise<void> {
  if (!scanLocked.value) await rescan();
}
// 连接回包也包含主控网卡，工具信息未取到不应挡住这份有效拓扑。
const inventoryReady = computed(() => loaded.value || session.connection !== null);

// ---- 搜索与选中（上下文在 state/ui，切走再回来还在） ----
const shown = computed(() => filterByQuery(masterNics.value, ui.local.query, nicSearchFields));
const countLabel = computed(() => visibleCountLabel(shown.value.length, masterNics.value.length));
const selected = computed(() =>
  masterNics.value.find((nic) => nicKey(nic) === ui.local.selected) ?? null,
);
/**
 * 选中的那块卡被查询挡在外面时**不**改选中（§11.2）。
 *
 * 悄悄切到第一条是最坏的做法：详情区的内容变了，而用户以为自己只是在打字。
 */
const selectedHidden = computed(
  () => !!selected.value && !shown.value.some((nic) => nicKey(nic) === ui.local.selected),
);

const detailBack = ref<HTMLButtonElement>();
const searchInput = ref<HTMLInputElement>();
let selectionTrigger: HTMLElement | null = null;
async function closeDetail(): Promise<void> {
  ui.local.selected = '';
  await nextTick();
  if (selectionTrigger?.isConnected) selectionTrigger.focus();
  else searchInput.value?.focus();
}
async function pick(nic: NicInfo): Promise<void> {
  // 再点一次同一块 = 收起详情。窄屏上这就是「返回列表」。
  if (ui.local.selected === nicKey(nic)) {
    await closeDetail();
    return;
  }
  selectionTrigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  ui.local.selected = nicKey(nic);
  await nextTick();
  detailBack.value?.focus();
}
function clearQuery(): void {
  ui.local.query = '';
}
const detailRows = computed(() => {
  const nic = selected.value;
  if (!nic) return [];
  return [
    { k: '接口名', v: nic.name, mono: true },
    { k: '驱动描述', v: nic.description || '未提供', mono: false },
    { k: '角色', v: nic.role || 'UNKNOWN', mono: true },
    { k: 'Wi-Fi 频段', v: nic.wifi_band || '不适用', mono: false },
    { k: 'IPv4', v: nic.ipv4 || '未获取', mono: true },
    { k: '默认网关', v: nic.gateway_v4 || '无默认路由', mono: true },
    { k: '协商速率', v: nicSpeedLabel(nic), mono: true },
    { k: 'IPv6 link-local', v: nicLinkLocal(nic) || '未获取', mono: true },
    { k: 'IPv6 全局', v: nic.ipv6_global || '未获取', mono: true },
  ];
});
</script>

<template>
  <section class="view">
    <header class="view-head local-head">
      <div>
        <h2>本机</h2>
        <p class="muted">检查主控机的网卡和测试工具，准备双机链路测试。</p>
      </div>
      <button type="button" @click="goto(connected ? 'plan' : 'agent')">
        {{ connected ? '配置测试计划' : '连接辅测机' }}
      </button>
    </header>

    <section class="test-modes" aria-label="选择测试方式">
      <div>
        <h3>两个电脑网口互测</h3>
        <p>选择主控、辅测机或同一电脑上的两个网口，测试它们经过 CPE 网络的连通性与吞吐。</p>
        <button type="button" @click="goto(connected ? 'plan' : 'agent')">{{ connected ? '选择子网测试网口' : '连接辅测机，准备子网测试' }}</button>
      </div>
      <div>
        <h3>电脑网口与 CPE 板侧互测</h3>
        <p>通过 ADB 控制 CPE，逐个选择 LAN、Wi-Fi 或 RNDIS 网口测上行、下行；本机即可独立测试。</p>
        <button type="button" class="ghost" @click="goto('inner')">进入内环测试，扫描网口</button>
      </div>
    </section>

    <div class="connection-overview" aria-label="双端连接状态">
      <div class="endpoint">
        <span class="endpoint-label">主控机 <small>本机</small></span>
        <strong>{{ masterHostname || (inventoryReady ? '主机名未知' : '正在读取…') }}</strong>
        <span class="hint">{{ inventoryReady ? `${masterNics.length} 块网卡` : '读取网卡与工具信息' }}</span>
      </div>
      <div class="connection-path" :class="{ connected }">
        <span class="path-line" aria-hidden="true"></span>
        <span>{{ connected ? '双端已连接' : '等待连接辅测机' }}</span>
      </div>
      <div class="endpoint">
        <span class="endpoint-label">辅测机 <small>对端</small></span>
        <strong>{{ connected ? agentHostname || session.connectedHost : '尚未连接' }}</strong>
        <span class="hint">{{ connected ? `${agentNics.length} 块网卡` : '连接后获取对端网卡' }}</span>
      </div>
    </div>

    <div class="tool-strip" aria-label="本机工具信息">
      <div><span class="muted">控制台版本</span><strong class="mono">{{ session.local?.version || '—' }}</strong></div>
      <div>
        <span class="muted">iperf3</span>
        <strong :class="{ missing: loaded && !iperf }">{{ loaded ? iperf || '未找到' : connected ? '未获取，请重新扫描' : '正在检查…' }}</strong>
      </div>
    </div>
    <p v-if="loaded && !iperf" class="warn" role="alert">
      本机未找到 iperf3。请将它放到程序同目录，或加入 PATH，然后重新扫描。
      使用 iperf3 的测试需要先准备好这个工具。
    </p>
    <div v-if="!loaded && session.localError" class="load-error" role="alert">
      <p>本机信息加载失败：{{ session.localError }}</p>
      <button type="button" class="ghost" @click="load">重新加载</button>
    </div>

    <div class="inventory-head">
      <div>
        <h3>
          本机网卡
          <!-- 还没读到时不写数字：`0` 和「真的一块都没有」在屏幕上一样，
               而它们的下一步完全不同（等 vs 去查网线）。 -->
          <span v-if="inventoryReady">{{ countLabel }}</span>
          <span v-else>读取中</span>
        </h3>
        <p class="hint">插拔网线、开关 Wi-Fi 或修改 IP 后，重新扫描更新列表。</p>
      </div>
      <button type="button" class="ghost" :disabled="scanLocked" :title="scanLockHint" @click="onRescan">
        {{ session.scanning ? '扫描中…' : '重新扫描' }}
      </button>
    </div>
    <p v-if="session.scanMessage" class="scan" :class="session.scanKind" role="status">{{ session.scanMessage }}</p>
    <p v-if="testInFlight" class="hint filter-note" role="status">测试进行中，网卡列表保持开跑时的连接信息；结束后可重新扫描。</p>
    <p v-if="session.topologyStale" class="warn" role="status">
      当前网卡列表仍是上次成功连接 {{ session.connectedHost }} 时的快照（{{ freshnessLabel(session.connectedAt) }}）。
      重新扫描成功后才会更新，当前不能据此确认网口状态。
    </p>
    <p v-if="connected" class="hint filter-note">当前显示连接时按 IPv4 前缀过滤的网卡；重新扫描会同时更新两端。</p>
    <p v-if="!inventoryReady && !session.localError" class="loading" role="status">正在读取本机网卡…</p>

    <template v-else-if="inventoryReady">
      <div v-if="masterNics.length" class="search-bar">
        <label class="search">
          <span class="sr-only">搜索网卡</span>
          <input
            ref="searchInput"
            type="search"
            :value="ui.local.query"
            placeholder="搜接口名、描述、IP、角色"
            @input="ui.local.query = ($event.target as HTMLInputElement).value"
          />
        </label>
        <button v-if="ui.local.query" type="button" class="ghost small" @click="clearQuery">清空搜索</button>
      </div>

      <p v-if="selectedHidden" class="hint picked-hidden" role="status">
        当前选中的网卡不在搜索结果里，下面的详情仍是它。
        <button type="button" class="linklike" @click="clearQuery">清空搜索</button>
      </p>

      <div class="inventory" :class="{ 'has-detail': !!selected }">
        <div class="inventory-list" :class="{ 'hide-narrow': !!selected }">
          <NicTable
            :nics="shown"
            :selected-key="ui.local.selected"
            :empty-hint="
              ui.local.query
                ? `没有网卡匹配「${ui.local.query.trim()}」。清空搜索可以看到全部 ${masterNics.length} 块。`
                : '没有扫到网卡。检查网线/Wi-Fi 是否连接，或到「辅测机」页调整 IPv4 前缀过滤。'
            "
            @select="pick"
          />
        </div>

        <aside v-if="selected" class="detail" aria-label="网卡详情">
          <div class="detail-head">
            <strong>{{ selected.name }}</strong>
            <button ref="detailBack" type="button" class="ghost small" @click="closeDetail">
              返回网卡列表
            </button>
          </div>
          <dl>
            <template v-for="row in detailRows" :key="row.k">
              <dt>{{ row.k }}</dt>
              <dd :class="{ mono: row.mono }">{{ row.v }}</dd>
            </template>
          </dl>
          <p class="hint">这些值来自最近一次扫描；改了 IP 或换了网线，用上面的「重新扫描」。</p>
        </aside>
      </div>
    </template>
  </section>
</template>

<style scoped>
.test-modes { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 24px; margin-bottom: 24px; padding: 20px; border: 1px solid var(--line); border-left: 3px solid var(--accent); border-radius: 6px; }
.test-modes h3 { margin: 0 0 8px; font-size: 15px; }
.test-modes p { margin: 0 0 14px; font-size: 13px; line-height: 1.7; color: var(--muted); }
.test-modes button { max-width: 100%; }
@media (max-width: 700px) { .test-modes { grid-template-columns: 1fr; gap: 20px; padding: 16px; } }
.local-head { display: flex; align-items: center; justify-content: space-between; gap: 16px; flex-wrap: wrap; }
.local-head button { flex-shrink: 0; }
.connection-overview { display: grid; grid-template-columns: minmax(0, 1fr) minmax(130px, .7fr) minmax(0, 1fr); gap: 24px; padding: 26px; background: var(--panel-2); border: 1px solid var(--line); border-radius: 9px; }
.endpoint { display: grid; gap: 7px; min-width: 0; }
.endpoint-label { color: var(--muted); font-size: 12px; }
.endpoint-label small { margin-left: 8px; padding: 2px 6px; border: 1px solid var(--line); border-radius: 3px; font-size: 10px; }
.endpoint strong { font-size: 21px; font-weight: 650; overflow-wrap: anywhere; }
.connection-path { display: flex; flex-direction: column; justify-content: center; gap: 9px; text-align: center; color: var(--muted); font-size: 11px; }
.path-line { display: block; position: relative; border-top: 1px dashed var(--line); margin: 10px 0 0; }
.path-line::before, .path-line::after { content: ''; position: absolute; width: 7px; height: 7px; top: -4px; border: 1px solid var(--muted); border-radius: 50%; background: var(--panel-2); }
.path-line::before { left: 0; }
.path-line::after { right: 0; }
.connected { color: var(--ok); }
.connected .path-line { border-color: var(--ok); border-top-style: solid; }
.connected .path-line::before, .connected .path-line::after { border-color: var(--ok); background: var(--ok); }
.tool-strip { display: flex; flex-wrap: wrap; gap: 12px 30px; padding: 15px 2px; margin-bottom: 24px; border-bottom: 1px solid var(--line); font-size: 12px; }
.tool-strip > div { display: flex; align-items: baseline; gap: 12px; min-width: 0; }
.tool-strip strong { font-weight: 500; overflow-wrap: anywhere; }
.tool-strip .missing { color: var(--bad); }
.inventory-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; margin-bottom: 16px; }
.inventory-head h3 { margin: 0; }
.inventory-head h3 span { font-size: 12px; font-weight: 500; margin-left: 6px; color: var(--muted); }
.inventory-head p { margin: 5px 0 0; }
.warn, .load-error { margin: 0 0 22px; padding: 12px 16px; border-left: 3px solid var(--warn); background: var(--info-bg); font-size: 13px; }
.load-error { border-color: var(--bad); background: var(--bad-bg); }
.load-error p { margin-top: 0; }
.scan { margin: -6px 0 14px; font-size: 12px; color: var(--muted); }
.scan.ok { color: var(--ok); }
.scan.bad { color: var(--bad); }
.filter-note { margin-top: -6px; }
.loading { padding: 24px; border: 1px dashed var(--line); border-radius: 6px; color: var(--muted); }
.search-bar { display: flex; align-items: center; gap: 10px; margin-bottom: 12px; }
.search { flex: 1 1 260px; max-width: 380px; }
.search input { width: 100%; padding: 8px 11px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); color: var(--ink); font: inherit; }
button.small { min-height: 32px; padding: 6px 11px; font-size: 12.5px; }
.linklike { padding: 0; min-height: 0; font: inherit; color: var(--accent); background: none; border: 0; text-decoration: underline; text-underline-offset: 3px; }
.linklike:hover:not(:disabled) { background: none; color: var(--accent-hover); }
.picked-hidden { margin: -4px 0 12px; }
/* 桌面同页主从；窄屏改成「列表 ⇄ 详情」切换（方案 §5.2）。 */
.inventory { display: grid; gap: 16px; }
.inventory.has-detail { grid-template-columns: minmax(0, 1fr) minmax(240px, 320px); }
.inventory-list { min-width: 0; }
.detail { padding: 16px 18px; border: 1px solid var(--line); border-radius: 7px; background: var(--panel-2); align-self: start; }
.detail-head { display: flex; align-items: center; justify-content: space-between; gap: 10px; margin-bottom: 12px; }
.detail-head strong { font-size: 15px; overflow-wrap: anywhere; }
.detail dl { display: grid; grid-template-columns: auto minmax(0, 1fr); gap: 7px 14px; margin: 0 0 12px; font-size: 12.5px; }
.detail dt { color: var(--muted); white-space: nowrap; }
.detail dd { margin: 0; overflow-wrap: anywhere; }
.detail .mono { font-family: var(--fm); }
@media (max-width: 860px) {
  /* 六列的表挤不进手机；进详情就把列表让出去，详情自带「返回网卡列表」。 */
  .inventory.has-detail { grid-template-columns: minmax(0, 1fr); }
  .inventory-list.hide-narrow { display: none; }
}
@media (max-width: 600px) {
  .connection-overview { grid-template-columns: minmax(0, 1fr); padding: 20px; gap: 20px; }
  .connection-path { text-align: left; flex-direction: row; align-items: center; justify-content: flex-start; }
  .path-line { width: 40px; margin: 0; }
  .endpoint strong { font-size: 19px; }
  .tool-strip > div { flex-wrap: wrap; gap: 4px 12px; }
}
</style>
