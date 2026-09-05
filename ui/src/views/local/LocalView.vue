<script setup lang="ts">
import { computed } from 'vue';
import NicTable from '../../components/NicTable.vue';
import { agentHostname, agentNics, masterHostname, masterNics } from '../../state/inventory';
import { load, rescan, session } from '../../state/session';
import { goto } from '../../state/ui';

/** 本机信息独立于辅测机连接；尚未返回时不能把未知工具状态当成缺失。 */
const iperf = computed(() => session.local?.iperf3 ?? null);
const loaded = computed(() => session.local !== null);
const connected = computed(() => session.phase === 'connected');
// 连接回包也包含主控网卡，工具信息未取到不应挡住这份有效拓扑。
const inventoryReady = computed(() => loaded.value || session.connection !== null);
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
        <strong>{{ connected ? agentHostname || session.host : '尚未连接' }}</strong>
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
    <div v-if="!loaded && session.phase === 'failed'" class="load-error" role="alert">
      <p>本机信息加载失败：{{ session.error }}</p>
      <button type="button" class="ghost" @click="load">重新加载</button>
    </div>

    <div class="inventory-head">
      <div><h3>本机网卡 <span>{{ masterNics.length }}</span></h3><p class="hint">插拔网线、开关 Wi-Fi 或修改 IP 后，重新扫描更新列表。</p></div>
      <button type="button" class="ghost" :disabled="session.scanning" @click="rescan">
        {{ session.scanning ? '扫描中…' : '重新扫描' }}
      </button>
    </div>
    <p v-if="session.scanMessage" class="scan" :class="session.scanKind" role="status">{{ session.scanMessage }}</p>
    <p v-if="connected" class="hint filter-note">当前显示连接时按 IPv4 前缀过滤的网卡；重新扫描会同时更新两端。</p>
    <p v-if="!inventoryReady && session.phase !== 'failed'" class="loading" role="status">正在读取本机网卡…</p>
    <NicTable v-else-if="inventoryReady" :nics="masterNics" empty-hint="没有扫到网卡。检查网线/Wi-Fi 是否连接，或到「辅测机」页调整 IPv4 前缀过滤。" />
  </section>
</template>

<style scoped>
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
.warn, .load-error { margin: 0 0 22px; padding: 12px 16px; border-left: 3px solid var(--focus); background: var(--info-bg); font-size: 13px; }
.load-error { border-color: var(--bad); background: var(--bad-bg); }
.load-error p { margin-top: 0; }
.scan { margin: -6px 0 14px; font-size: 12px; color: var(--muted); }
.scan.ok { color: var(--ok); }
.scan.bad { color: var(--bad); }
.filter-note { margin-top: -6px; }
.loading { padding: 24px; border: 1px dashed var(--line); border-radius: 6px; color: var(--muted); }
@media (max-width: 600px) {
  .connection-overview { grid-template-columns: minmax(0, 1fr); padding: 20px; gap: 20px; }
  .connection-path { text-align: left; flex-direction: row; align-items: center; justify-content: flex-start; }
  .path-line { width: 40px; margin: 0; }
  .endpoint strong { font-size: 19px; }
  .tool-strip > div { flex-wrap: wrap; gap: 4px 12px; }
}
</style>
