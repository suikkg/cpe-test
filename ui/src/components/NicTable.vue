<script setup lang="ts">
import type { NicInfo } from '../api/dto';

/**
 * 网卡表。**无状态展示件**：props in / emits out，不读 state、不发请求
 * （由 `lint-arch.mjs` 的分层规则挡着）。
 *
 * 网卡名、驱动描述、角色这些都来自辅测机——是网络来的字符串，一律当不可信。
 * Vue 的插值默认转义，所以这里不需要也**不许**用 `v-html`。
 */
defineProps<{
  nics: NicInfo[];
  /** 空表时显示的提示；不同来源（本机 / 未连接的辅测机）说法不一样 */
  emptyHint: string;
}>();

function speed(nic: NicInfo): string {
  // 0 表示「拿不到协商速率」，不是「0 Mbps」。填 0 会让人以为链路挂了。
  return nic.speed_mbps > 0 ? `${nic.speed_mbps} Mbps` : '—';
}
</script>

<template>
  <div v-if="nics.length === 0" class="empty">{{ emptyHint }}</div>
  <div v-else class="scroll" tabindex="0" role="region" aria-label="网卡列表，可横向滚动">
    <table>
      <thead>
        <tr>
          <th scope="col">接口名</th>
          <th scope="col">角色</th>
          <th scope="col">IPv4</th>
          <th scope="col">网关</th>
          <th scope="col" class="num">协商速率</th>
          <th scope="col">IPv6 link-local</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="nic in nics" :key="nic.ifindex || nic.name">
          <td>
            <strong>{{ nic.name }}</strong>
            <br />
            <small class="muted">{{ nic.description || '—' }}</small>
          </td>
          <td>
            <span class="role mono">{{ nic.role || 'UNKNOWN' }}</span>
            <small v-if="nic.wifi_band" class="muted"> · {{ nic.wifi_band }}</small>
          </td>
          <td class="mono">{{ nic.ipv4 || '—' }}</td>
          <td class="mono">{{ nic.gateway_v4 || '—' }}</td>
          <td class="num mono">{{ speed(nic) }}</td>
          <td class="mono">
            {{ nic.ipv6_ll || '—' }}<template v-if="nic.zone">%{{ nic.zone }}</template>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
/* 宽表在自己的容器里横向滚动，页面本身永不横向滚。 */
.scroll {
  max-width: 100%;
  overflow-x: auto;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--surface);
}
table {
  width: 100%;
  min-width: 760px;
  border-collapse: separate;
  border-spacing: 0;
  font-size: 13px;
}
th,
td {
  padding: 13px 14px;
  text-align: left;
  border-bottom: 1px solid var(--line);
  vertical-align: top;
}
thead th {
  position: sticky;
  top: 0;
  background: var(--head);
  font-size: 11.5px;
  font-weight: 600;
  color: var(--muted);
  white-space: nowrap;
}
tbody tr:last-child td {
  border-bottom: 0;
}
tbody tr:hover { background: var(--panel-2); }
td:first-child { min-width: 180px; max-width: 270px; overflow-wrap: anywhere; }
td:first-child small { display: inline-block; margin-top: 3px; line-height: 1.5; }
td:not(:first-child) { white-space: nowrap; }
.num {
  text-align: right;
  font-variant-numeric: tabular-nums;
}
.mono {
  font-family: var(--fm);
}
.muted {
  color: var(--muted);
}
.role {
  font-size: 12px;
}
.empty {
  padding: 14px 16px;
  border: 1px dashed var(--line);
  border-radius: 6px;
  color: var(--muted);
  background: var(--panel-2);
}
</style>
