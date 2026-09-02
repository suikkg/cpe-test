<script setup lang="ts">
import { computed } from 'vue';
import {
  defaultNumberPlaceholder,
  formatNumberList,
  formatTokenList,
  wifiBandLabel,
  wifiBandPairRows,
  parseNumberList,
  parseTokenList,
  setWifiBandThreshold,
  wifiBandThresholdFor,
} from '../../domain/globals';
import { agentNics, masterNics } from '../../state/inventory';
import { plan } from '../../state/plan';
import { session } from '../../state/session';

function tokens(key: 'tcp_windows' | 'udp_bandwidths' | 'udp_lengths' | 'udp_windows') {
  return computed({
    get: () => formatTokenList(plan.globals[key]),
    set: (raw: string) => {
      plan.globals[key] = parseTokenList(raw);
    },
  });
}

function numbers(key: 'tcp_streams' | 'ping_payload_sizes') {
  return computed({
    get: () => formatNumberList(plan.globals[key]),
    set: (raw: string) => {
      plan.globals[key] = parseNumberList(raw);
    },
  });
}

function scalar(key: 'udp_streams' | 'ping_count') {
  return computed({
    get: () => (plan.globals[key] > 0 ? String(plan.globals[key]) : ''),
    set: (raw: string) => {
      const value = Number(raw.trim());
      plan.globals[key] = Number.isFinite(value) && value > 0 ? Math.trunc(value) : 0;
    },
  });
}

/** RTT 允许小数毫秒，不能复用上面会 Math.trunc 的整数绑定。 */
type PositiveDecimalKey =
  | 'ping_small_max_bytes'
  | 'ping_medium_max_bytes'
  | 'ping_wired_small_avg_rtt_ms'
  | 'ping_wired_small_max_rtt_ms'
  | 'ping_wired_medium_avg_rtt_ms'
  | 'ping_wired_medium_max_rtt_ms'
  | 'ping_wired_large_avg_rtt_ms'
  | 'ping_wired_large_max_rtt_ms'
  | 'ping_wifi_small_avg_rtt_ms'
  | 'ping_wifi_small_max_rtt_ms'
  | 'ping_wifi_medium_avg_rtt_ms'
  | 'ping_wifi_medium_max_rtt_ms'
  | 'ping_wifi_large_avg_rtt_ms'
  | 'ping_wifi_large_max_rtt_ms';

type PingPolicyNumberKey = PositiveDecimalKey;

type WifiBandKey =
  | 'rx_target_master_to_agent_mbps'
  | 'rx_target_agent_to_master_mbps'
  | 'bidir_total_rx_target_mbps';

function positiveValue(key: PositiveDecimalKey): string {
  const value = plan.globals[key];
  return value > 0 ? String(value) : '';
}

function positiveNumber(event: Event): number {
  const value = Number((event.target as HTMLInputElement).value.trim());
  return Number.isFinite(value) && value > 0 ? value : 0;
}

const integerPositiveKeys = new Set<PositiveDecimalKey>([
  'ping_small_max_bytes',
  'ping_medium_max_bytes',
]);

function updatePositive(key: PositiveDecimalKey, event: Event): void {
  const value = positiveNumber(event);
  plan.globals[key] = integerPositiveKeys.has(key) ? Math.trunc(value) : value;
}

const udpStreams = scalar('udp_streams');
const pingCount = scalar('ping_count');

type PingPolicyRow = {
  bucket: string;
  range: string;
  wiredAvgKey: PingPolicyNumberKey;
  wiredMaxKey: PingPolicyNumberKey;
  wifiAvgKey: PingPolicyNumberKey;
  wifiMaxKey: PingPolicyNumberKey;
};

function configuredPingValue(key: PingPolicyNumberKey): number {
  const value = session.bootstrap?.[key];
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : 0;
}

function effectivePingValue(key: PingPolicyNumberKey): number {
  return plan.globals[key] > 0 ? plan.globals[key] : configuredPingValue(key);
}

function pingPlaceholder(key: PingPolicyNumberKey): string {
  return defaultNumberPlaceholder(configuredPingValue(key));
}

const policyRows = computed<PingPolicyRow[]>(() => {
  const smallMax = effectivePingValue('ping_small_max_bytes');
  const mediumMax = effectivePingValue('ping_medium_max_bytes');
  return [
    {
      bucket: 'small',
      range: smallMax > 0 ? `≤ ${smallMax} B` : '—',
      wiredAvgKey: 'ping_wired_small_avg_rtt_ms',
      wiredMaxKey: 'ping_wired_small_max_rtt_ms',
      wifiAvgKey: 'ping_wifi_small_avg_rtt_ms',
      wifiMaxKey: 'ping_wifi_small_max_rtt_ms',
    },
    {
      bucket: 'medium',
      range: smallMax > 0 && mediumMax > 0 ? `${smallMax + 1}–${mediumMax} B` : '—',
      wiredAvgKey: 'ping_wired_medium_avg_rtt_ms',
      wiredMaxKey: 'ping_wired_medium_max_rtt_ms',
      wifiAvgKey: 'ping_wifi_medium_avg_rtt_ms',
      wifiMaxKey: 'ping_wifi_medium_max_rtt_ms',
    },
    {
      bucket: 'large',
      range: mediumMax > 0 ? `> ${mediumMax} B` : '—',
      wiredAvgKey: 'ping_wired_large_avg_rtt_ms',
      wiredMaxKey: 'ping_wired_large_max_rtt_ms',
      wifiAvgKey: 'ping_wifi_large_avg_rtt_ms',
      wifiMaxKey: 'ping_wifi_large_max_rtt_ms',
    },
  ];
});

function pingValue(key: PingPolicyNumberKey): string {
  return positiveValue(key);
}

const wifiBandRows = computed(() => wifiBandPairRows(masterNics.value, agentNics.value));

function bandRuleValue(
  row: { masterBand: string; agentBand: string },
  key: WifiBandKey,
): string {
  const value = wifiBandThresholdFor(
    plan.globals.wifi_band_thresholds,
    row.masterBand,
    row.agentBand,
  )[key];
  return value > 0 ? String(value) : '';
}

function updateBandRule(
  row: { masterBand: string; agentBand: string },
  key: WifiBandKey,
  event: Event,
): void {
  plan.globals.wifi_band_thresholds = setWifiBandThreshold(
    plan.globals.wifi_band_thresholds,
    row.masterBand,
    row.agentBand,
    { [key]: positiveNumber(event) },
  );
}

const hasLegacyWifiOverrides = computed(
  () =>
    plan.globals.wifi_pair_rx_target_mbps > 0 ||
    plan.globals.wifi_pair_bidir_rx_target_mbps > 0 ||
    plan.globals.wifi_pair_bidir_total_rx_target_mbps > 0 ||
    plan.globals.wifi_pair_thresholds.length > 0,
);

function clearLegacyWifiOverrides(): void {
  plan.globals.wifi_pair_rx_target_mbps = 0;
  plan.globals.wifi_pair_bidir_rx_target_mbps = 0;
  plan.globals.wifi_pair_bidir_total_rx_target_mbps = 0;
  plan.globals.wifi_pair_thresholds = [];
}

const tcpWindows = tokens('tcp_windows');
const tcpStreams = numbers('tcp_streams');
const udpBandwidths = tokens('udp_bandwidths');
const udpLengths = tokens('udp_lengths');
const udpWindows = tokens('udp_windows');
const pingSizes = numbers('ping_payload_sizes');

</script>

<template>
  <section class="block">
    <div class="head">
      <strong>全局默认档位</strong>
    </div>

    <div class="grid">
      <label class="wide">
        <span>UDP 单流带宽 <code>-b</code></span>
        <input
          v-model="udpBandwidths"
          type="text"
          placeholder="如 2500m, 1000m"
          autocomplete="off"
        />
      </label>
      <label>
        <span>UDP 报文长度 <code>-l</code></span>
        <input
          v-model="udpLengths"
          type="text"
          placeholder="留空 = 不下发 -l"
          autocomplete="off"
        />
      </label>
      <label>
        <span>UDP socket buffer <code>-w</code></span>
        <input
          v-model="udpWindows"
          type="text"
          placeholder="留空 = 不下发 -w"
          autocomplete="off"
        />
      </label>
      <label>
        <span>UDP 并发流</span>
        <input
          v-model="udpStreams"
          type="text"
          inputmode="numeric"
          placeholder="如 1"
          autocomplete="off"
        />
      </label>
      <label>
        <span>TCP socket buffer <code>-w</code></span>
        <input
          v-model="tcpWindows"
          type="text"
          placeholder="如 4m, 64k"
          autocomplete="off"
        />
      </label>
      <label>
        <span>TCP 并发流 <code>-P</code></span>
        <input
          v-model="tcpStreams"
          type="text"
          placeholder="如 1, 10"
          autocomplete="off"
        />
      </label>
      <label>
        <span>Ping 次数</span>
        <input
          v-model="pingCount"
          type="text"
          inputmode="numeric"
          placeholder="如 180"
          autocomplete="off"
        />
      </label>
      <label>
        <span>Ping 包长（字节）</span>
        <input
          v-model="pingSizes"
          type="text"
          placeholder="如 32, 1400"
          autocomplete="off"
        />
      </label>
    </div>

    <details class="policy">
      <summary><strong>Ping 高级阈值</strong> <span class="muted">自动按链路类型 × payload 档位选择；需要时可临时收紧/放宽</span></summary>
      <div class="policy-grid">
        <label class="bucket-rule">
          <span>small 最大字节</span>
          <input :value="positiveValue('ping_small_max_bytes')" inputmode="numeric" :placeholder="pingPlaceholder('ping_small_max_bytes')" @input="updatePositive('ping_small_max_bytes', $event)" />
        </label>
        <label class="bucket-rule">
          <span>medium 最大字节</span>
          <input :value="positiveValue('ping_medium_max_bytes')" inputmode="numeric" :placeholder="pingPlaceholder('ping_medium_max_bytes')" @input="updatePositive('ping_medium_max_bytes', $event)" />
        </label>
      </div>
      <div class="table-scroll">
        <table class="ping-policy-table">
          <colgroup>
            <col class="payload-col" />
            <col class="range-col" />
            <col class="rtt-col" />
            <col class="rtt-col" />
            <col class="rtt-col" />
            <col class="rtt-col" />
          </colgroup>
          <thead>
            <tr><th>payload 档位</th><th>范围</th><th colspan="2">有线</th><th colspan="2">Wi-Fi</th></tr>
            <tr class="subhead"><th></th><th></th><th>Avg RTT（ms）</th><th>Max RTT（ms）</th><th>Avg RTT（ms）</th><th>Max RTT（ms）</th></tr>
          </thead>
          <tbody>
            <tr v-for="row in policyRows" :key="row.bucket">
              <th scope="row">{{ row.bucket }}</th>
              <td class="muted">{{ row.range }}</td>
              <td><input :value="pingValue(row.wiredAvgKey)" :placeholder="pingPlaceholder(row.wiredAvgKey)" :aria-label="`有线 ${row.bucket} Avg RTT`" @input="updatePositive(row.wiredAvgKey, $event)" /></td>
              <td><input :value="pingValue(row.wiredMaxKey)" :placeholder="pingPlaceholder(row.wiredMaxKey)" :aria-label="`有线 ${row.bucket} Max RTT`" @input="updatePositive(row.wiredMaxKey, $event)" /></td>
              <td><input :value="pingValue(row.wifiAvgKey)" :placeholder="pingPlaceholder(row.wifiAvgKey)" :aria-label="`Wi-Fi ${row.bucket} Avg RTT`" @input="updatePositive(row.wifiAvgKey, $event)" /></td>
              <td><input :value="pingValue(row.wifiMaxKey)" :placeholder="pingPlaceholder(row.wifiMaxKey)" :aria-label="`Wi-Fi ${row.bucket} Max RTT`" @input="updatePositive(row.wifiMaxKey, $event)" /></td>
            </tr>
          </tbody>
        </table>
      </div>
      <p class="muted hint">灰字“默认 xx”是主控当前生效值；输入数值后覆盖。所有档位仍要求 0% 丢包。</p>
    </details>

    <details class="policy">
      <summary><strong>Wi-Fi 互测门限</strong> <span class="muted">按当前识别到的频段组合显示</span></summary>
      <div v-if="wifiBandRows.length === 0" class="empty-inline">两端识别到 Wi-Fi 网口后显示门限表。</div>
      <div v-else class="table-scroll">
        <table class="ping-policy-table wifi-table wifi-matrix">
          <thead>
            <tr><th colspan="2">频段组合</th><th colspan="2">单向测试</th><th>双向并发</th></tr>
            <tr class="subhead"><th>主控</th><th>辅测</th><th>主控 → 辅测</th><th>辅测 → 主控</th><th>两端 RX 合计</th></tr>
          </thead>
          <tbody>
            <tr v-for="row in wifiBandRows" :key="`${row.masterBand}-${row.agentBand}`">
              <th scope="row">{{ wifiBandLabel(row.masterBand) }}</th>
              <td class="band-cell">{{ wifiBandLabel(row.agentBand) }}</td>
              <td><input :value="bandRuleValue(row, 'rx_target_master_to_agent_mbps')" inputmode="decimal" placeholder="Mbps" :aria-label="`${wifiBandLabel(row.masterBand)} 到 ${wifiBandLabel(row.agentBand)} 单向 RX 门限`" @input="updateBandRule(row, 'rx_target_master_to_agent_mbps', $event)" /></td>
              <td><input :value="bandRuleValue(row, 'rx_target_agent_to_master_mbps')" inputmode="decimal" placeholder="Mbps" :aria-label="`${wifiBandLabel(row.agentBand)} 到 ${wifiBandLabel(row.masterBand)} 单向 RX 门限`" @input="updateBandRule(row, 'rx_target_agent_to_master_mbps', $event)" /></td>
              <td><input :value="bandRuleValue(row, 'bidir_total_rx_target_mbps')" inputmode="decimal" placeholder="Mbps" :aria-label="`${wifiBandLabel(row.masterBand)} 与 ${wifiBandLabel(row.agentBand)} 双向并发两端 RX 合计门限`" @input="updateBandRule(row, 'bidir_total_rx_target_mbps', $event)" /></td>
            </tr>
          </tbody>
        </table>
      </div>
      <p class="muted hint">
        双向并发按<strong>两端 RX 合计</strong>判定：<code>主控端 RX 平均 + 辅测端 RX 平均 ≥ 合计门限</code>。
        Wi-Fi 之间抢的是同一段空口时间，两个方向怎么分完全看调度，不要求各达到一半——
        合计 900 时「720 + 230 = 950」就是 PASS。留空则双向单元只显示实测值，不判 PASS/FAIL。
      </p>
      <div v-if="hasLegacyWifiOverrides" class="legacy-warning">
        <span>当前项目含旧版具体网口覆盖，仍按兼容规则执行。</span>
        <button type="button" class="ghost small" @click="clearLegacyWifiOverrides">清除旧覆盖</button>
      </div>
    </details>

  </section>
</template>

<style scoped>
.block {
  margin: 0 0 14px;
  padding: 11px 12px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--surface);
}
.head { display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; }
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(190px, 1fr));
  gap: 10px;
  margin: 10px 0 0;
}
.wide { grid-column: span 2; }
@media (max-width: 700px) { .wide { grid-column: auto; } }
label { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
label span { font-size: 11.5px; color: var(--muted); }
input {
  padding: 7px 9px;
  border: 1px solid var(--line);
  border-radius: 4px;
  background: var(--canvas);
  color: var(--ink);
  font: inherit;
  font-size: 13px;
  min-width: 0;
}
input::placeholder { color: var(--muted); opacity: 1; }
input:focus-visible { outline: 2px solid var(--focus); outline-offset: 1px; }
.hint { margin: 9px 0 0; font-size: 12px; }
.policy { margin-top: 10px; border-top: 1px dashed var(--line); padding-top: 9px; }
.policy summary { cursor: pointer; font-size: 12px; }
.policy-grid { display: grid; grid-template-columns: repeat(2, minmax(190px, 1fr)); gap: 10px; margin-top: 9px; max-width: 520px; }
.table-scroll { max-width: 100%; overflow-x: auto; margin-top: 11px; border: 1px solid var(--line); border-radius: 5px; }
.ping-policy-table { width: 100%; min-width: 660px; table-layout: fixed; border-collapse: separate; border-spacing: 0; font-size: 12px; }
.ping-policy-table .payload-col { width: 92px; }
.ping-policy-table .range-col { width: 108px; }
.ping-policy-table .rtt-col { width: 176px; }
.ping-policy-table th, .ping-policy-table td { padding: 7px 8px; border-bottom: 1px solid var(--line); text-align: left; vertical-align: middle; }
.ping-policy-table thead th { background: var(--head); color: var(--muted); font-weight: 600; line-height: 1.35; white-space: normal; }
.ping-policy-table thead .subhead th { padding-top: 4px; padding-bottom: 5px; font-size: 11px; font-weight: 500; }
.ping-policy-table tbody th { font-weight: 600; }
.ping-policy-table tbody tr:last-child th, .ping-policy-table tbody tr:last-child td { border-bottom: 0; }
.ping-policy-table input { width: 100%; box-sizing: border-box; }
.wifi-table { min-width: 720px; }
.wifi-table input { min-width: 100px; }
.empty-inline { margin-top: 9px; padding: 10px; border: 1px dashed var(--line); color: var(--muted); font-size: 12px; }
.wifi-matrix th:nth-child(-n + 2), .wifi-matrix td:nth-child(-n + 2) { width: 13%; }
.wifi-matrix .band-cell { font-weight: 600; }
.legacy-warning { display: flex; align-items: center; justify-content: space-between; gap: 10px; margin-top: 9px; padding: 8px 10px; border: 1px solid var(--warn); border-radius: 5px; color: var(--muted); font-size: 12px; }
@media (max-width: 560px) { .policy-grid { grid-template-columns: 1fr; max-width: none; } }
code { font-family: var(--fm); }
.muted { color: var(--muted); }
</style>
