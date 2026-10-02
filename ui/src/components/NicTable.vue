<script setup lang="ts">
import { nicKey, nicLinkLocal, nicSpeedLabel, nicWifiContext } from '../domain/nics';
import type { NicInfo } from '../api/dto';

/**
 * 网卡表。**无状态展示件**：props in / emits out，不读 state、不发请求
 * （由 `lint-arch.mjs` 的分层规则挡着）。
 *
 * 扫描结果的全部字段都在表里（含 IPv6 全局地址），所以不再需要单独的详情面板。
 *
 * 网卡名、驱动描述、角色这些都来自辅测机——是网络来的字符串，一律当不可信。
 * Vue 的插值默认转义，所以这里不需要也**不许**用 `v-html`。
 */
defineProps<{
  nics: NicInfo[];
  /** 空表时显示的提示；不同来源（本机 / 未连接的辅测机）说法不一样 */
  emptyHint: string;
  /** 读屏用的表名，例如「主控网卡」。 */
  label: string;
}>();

const speed = nicSpeedLabel;
const keyOf = nicKey;
const wifi = nicWifiContext;
const linkLocal = nicLinkLocal;
</script>

<template>
  <p v-if="nics.length === 0" class="empty-state">{{ emptyHint }}</p>
  <div v-else class="table-wrap" tabindex="0" role="region" :aria-label="`${label}，可横向滚动`">
    <table class="data">
      <thead>
        <tr>
          <th scope="col">接口名</th>
          <th scope="col">角色</th>
          <th scope="col">IPv4</th>
          <th scope="col">网关</th>
          <th scope="col" class="num">协商速率</th>
          <th scope="col">IPv6</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="nic in nics" :key="keyOf(nic)">
          <td class="name">
            <strong>{{ nic.name }}</strong>
            <small>{{ nic.description || '—' }}</small>
          </td>
          <td>
            <span class="mono role">{{ nic.role || 'UNKNOWN' }}</span>
            <small v-if="nic.wifi_band">{{ nic.wifi_band }}</small>
            <!-- 无线上下文单独一行：它比角色长得多，而这几项恰恰是 Wi-Fi 结果可复现的前提。 -->
            <small v-if="wifi(nic)">{{ wifi(nic) }}</small>
          </td>
          <td class="mono">{{ nic.ipv4 || '—' }}</td>
          <td class="mono">{{ nic.gateway_v4 || '—' }}</td>
          <td class="num mono">{{ speed(nic) }}</td>
          <td class="mono">
            {{ linkLocal(nic) || '—' }}
            <small v-if="nic.ipv6_global">{{ nic.ipv6_global }}</small>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
table { min-width: 720px; }
tbody tr:hover { background: var(--panel-2); }
td { white-space: nowrap; }
td.name { min-width: 170px; max-width: 270px; white-space: normal; overflow-wrap: anywhere; }
.role { font-size: 12px; }
</style>
