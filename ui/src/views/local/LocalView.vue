<script setup lang="ts">
import { computed } from 'vue';
import NicTable from '../../components/NicTable.vue';
import { masterHostname, masterNics } from '../../state/inventory';
import { rescan, session } from '../../state/session';

/**
 * 「本机」：控制台打开就能看的东西，**不需要连上辅测机**。
 *
 * 这一页存在的理由是把「工具链齐不齐、网卡认出来没有」这两个最常见的
 * 开场问题当场答掉——它们答不上来时，后面每一步都会失败得莫名其妙。
 */
const iperf = computed(() => session.local?.iperf3 ?? null);
const version = computed(() => session.local?.version ?? '');
</script>

<template>
  <section class="view">
    <header class="view-head">
      <h2>本机</h2>
      <p class="muted">控制台所在的这台机器。不需要连上辅测机就能看。</p>
    </header>

    <div class="cards">
      <div class="card">
        <span class="card-label">主机名</span>
        <strong class="card-value">{{ masterHostname || '—' }}</strong>
      </div>
      <div class="card">
        <span class="card-label">版本</span>
        <strong class="card-value mono">{{ version || '—' }}</strong>
      </div>
      <div class="card" :class="{ bad: !iperf }">
        <span class="card-label">iperf3</span>
        <strong class="card-value mono">{{ iperf ?? '未找到' }}</strong>
      </div>
    </div>

    <p v-if="!iperf" class="warn" role="alert">
      本机没找到 iperf3。把 <code>iperf3</code> 放到程序同目录，或装进 PATH——
      找不到它时所有灌包单元都会直接判 SETUP_ERROR。
    </p>

    <div class="bar">
      <h3>网卡</h3>
      <button type="button" class="ghost" :disabled="session.scanning" @click="rescan">
        {{ session.scanning ? '扫描中…' : '重新扫描' }}
      </button>
      <span v-if="session.scanMessage" class="scan" :class="session.scanKind">
        {{ session.scanMessage }}
      </span>
    </div>
    <p class="muted hint">
      插拔网线、开关 Wi-Fi、改完 IP 之后点它。连上辅测机时会「两端一起」重扫——
      连上之后这张表显示的就是那次连接扫到的（按 IPv4 前缀过滤过的）那一份。
    </p>
    <NicTable :nics="masterNics" empty-hint="没有扫到网卡。检查网线/Wi-Fi 是否连接，或到「辅测机」页调整 IPv4 前缀过滤。" />
  </section>
</template>

<style scoped>
.cards {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
  gap: 10px;
  margin: 0 0 18px;
}
.card {
  padding: 10px 12px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--surface);
}
.card.bad {
  border-color: var(--bad);
  background: var(--bad-bg);
}
.card-label {
  display: block;
  font-size: 11.5px;
  color: var(--muted);
}
.card-value {
  display: block;
  margin-top: 3px;
  font-size: 16px;
  overflow-wrap: anywhere;
}
.warn {
  margin: 0 0 18px;
  padding: 9px 12px;
  border-left: 3px solid var(--focus);
  background: var(--info-bg);
}
.bar { display: flex; align-items: baseline; gap: 12px; flex-wrap: wrap; margin: 0 0 4px; }
.bar h3 { margin: 0; }
.ghost {
  padding: 5px 13px;
  border: 1px solid var(--line);
  border-radius: 4px;
  background: var(--surface);
  color: var(--ink);
  font: inherit;
  font-size: 12.5px;
  cursor: pointer;
}
.ghost:disabled { opacity: .55; cursor: default; }
.scan { font-size: 12px; color: var(--muted); }
.scan.ok { color: var(--ok); }
.scan.bad { color: var(--bad); }
.hint { margin: 0 0 10px; font-size: 12px; }
.muted { color: var(--muted); }
code {
  font-family: var(--fm);
}
</style>
