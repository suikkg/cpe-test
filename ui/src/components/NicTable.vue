<script setup lang="ts">
import { nicKey, nicSpeedLabel, nicWifiContext } from '../domain/nics';
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
  /**
   * 当前选中行的 `nicKey`；不传 = 这张表不参与主从选择（辅测页现在就这样）。
   *
   * 选中是**可选**能力：加了它，接口名变成按钮；不加则和以前逐字节一样。
   */
  selectedKey?: string;
}>();
const emit = defineEmits<{ select: [nic: NicInfo] }>();

// 速率与选中标识都走 domain：表格和详情面板必须说同一句话。
const speed = nicSpeedLabel;
const keyOf = nicKey;
const wifi = nicWifiContext;
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
        <tr
          v-for="nic in nics"
          :key="keyOf(nic)"
          :class="{ picked: selectedKey !== undefined && selectedKey === keyOf(nic) }"
        >
          <td>
            <button
              v-if="selectedKey !== undefined"
              type="button"
              class="pick"
              :aria-pressed="selectedKey === keyOf(nic)"
              @click="emit('select', nic)"
            >
              {{ nic.name }}
            </button>
            <strong v-else>{{ nic.name }}</strong>
            <br />
            <small class="muted">{{ nic.description || '—' }}</small>
          </td>
          <td>
            <span class="role mono">{{ nic.role || 'UNKNOWN' }}</span>
            <small v-if="nic.wifi_band" class="muted"> · {{ nic.wifi_band }}</small>
            <!-- 无线上下文换行放在角色下面：它比角色长得多，挤在同一行会把
                 整张表撑宽，而这几项恰恰是 Wi-Fi 结果可复现的前提。 -->
            <small v-if="wifi(nic)" class="muted wifi-ctx">{{ wifi(nic) }}</small>
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
.wifi-ctx { display: block; margin-top: 3px; }
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
tbody tr.picked { background: var(--info-bg); }
/* 选中入口是接口名本身：不额外占一列，也不让整行变成一个巨大的点击区
   （整行可点时，用户复制 IP 的每一次划选都会顺手切换选中）。 */
.pick {
  padding: 0;
  font: inherit;
  font-weight: 700;
  color: var(--accent);
  background: none;
  border: 0;
  min-height: 0;
  text-align: left;
  text-decoration: underline;
  text-underline-offset: 3px;
}
.pick:hover:not(:disabled) { background: none; color: var(--accent-hover); }
.pick[aria-pressed='true'] { color: var(--ink); text-decoration: none; }
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
