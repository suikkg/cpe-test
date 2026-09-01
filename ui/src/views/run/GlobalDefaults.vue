<script setup lang="ts">
import { computed } from 'vue';
import {
  formatNumberList,
  formatTokenList,
  globalsAreEmpty,
  parseNumberList,
  parseTokenList,
} from '../../domain/globals';
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
function positiveDecimal(key: 'ping_max_rtt_ms') {
  return computed({
    get: () => (plan.globals[key] > 0 ? String(plan.globals[key]) : ''),
    set: (raw: string) => {
      const value = Number(raw.trim());
      plan.globals[key] = Number.isFinite(value) && value > 0 ? value : 0;
    },
  });
}

const udpStreams = scalar('udp_streams');
const pingCount = scalar('ping_count');
const pingMaxRtt = positiveDecimal('ping_max_rtt_ms');
const tcpWindows = tokens('tcp_windows');
const tcpStreams = numbers('tcp_streams');
const udpBandwidths = tokens('udp_bandwidths');
const udpLengths = tokens('udp_lengths');
const udpWindows = tokens('udp_windows');
const pingSizes = numbers('ping_payload_sizes');

const configured = computed(() => session.bootstrap);

function hint(values: readonly string[] | readonly number[] | undefined, fallback: string): string {
  if (!values || values.length === 0) return fallback;
  return `沿用配置：${values.join(', ')}`;
}

function udpProfileHint(values: readonly string[] | undefined, flag: string): string {
  if (plan.globals.udp_bandwidths.length > 0) return `留空 = 不下发 ${flag}`;
  return hint(values, `留空 = 不下发 ${flag}`);
}

function scalarHint(value: number | undefined, fallback: string): string {
  return value && value > 0 ? `沿用配置：${value}` : fallback;
}

const untouched = computed(() => globalsAreEmpty(plan.globals));
</script>

<template>
  <section class="block">
    <div class="head">
      <strong>全局默认档位</strong>
      <small class="muted">
        {{
          untouched
            ? '全部沿用主控 config.json（灰字就是它当前的值）'
            : '填了值的格子按填的跑；空格子沿用主控 config.json（灰字）'
        }}
      </small>
    </div>

    <div class="grid">
      <label class="wide">
        <span>UDP 单流带宽 <code>-b</code></span>
        <input
          v-model="udpBandwidths"
          type="text"
          :placeholder="hint(configured?.udp_bandwidths, '如 2500m, 1000m')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>UDP 报文长度 <code>-l</code></span>
        <input
          v-model="udpLengths"
          type="text"
          :placeholder="udpProfileHint(configured?.udp_lengths, '-l')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>UDP socket buffer <code>-w</code></span>
        <input
          v-model="udpWindows"
          type="text"
          :placeholder="udpProfileHint(configured?.udp_windows, '-w')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>UDP 并发流</span>
        <input
          v-model="udpStreams"
          type="text"
          inputmode="numeric"
          :placeholder="scalarHint(configured?.udp_streams, '留空 = 不覆盖')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>TCP socket buffer <code>-w</code></span>
        <input
          v-model="tcpWindows"
          type="text"
          :placeholder="hint(configured?.tcp_windows, '如 4m, 64k')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>TCP 并发流 <code>-P</code></span>
        <input
          v-model="tcpStreams"
          type="text"
          :placeholder="hint(configured?.tcp_streams, '如 1, 10')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>Ping 次数</span>
        <input
          v-model="pingCount"
          type="text"
          inputmode="numeric"
          :placeholder="scalarHint(configured?.ping_count, '留空 = 不覆盖')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>Ping 包长（字节）</span>
        <input
          v-model="pingSizes"
          type="text"
          :placeholder="hint(configured?.ping_payload_sizes, '如 32, 1400')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>Ping 最大 RTT（ms）</span>
        <input
          v-model="pingMaxRtt"
          type="text"
          inputmode="decimal"
          :placeholder="scalarHint(configured?.ping_max_rtt_ms, '留空 = 不覆盖')"
          autocomplete="off"
        />
      </label>
    </div>

    <p class="muted hint">
      套件里选中的配置优先；这里填的是「套件没选配置时用哪一组」。填多个用逗号分隔，逐档各跑一轮。
      <br />
      <strong>Ping PASS = 0% 丢包 + 最大 RTT 不超过门限</strong>；RTT 数据缺失同样不通过。
      <br />
      <strong>UDP 那四格是一整组</strong>：只要 <code>-b</code> 填了，
      <code>-l</code> / <code>-w</code> 留空就是<strong>真的不下发</strong>这两个参数（用 iperf3 默认），
      配置文件里的值不再参与。TCP 两格则各自独立回落。
    </p>
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
code { font-family: var(--fm); }
.muted { color: var(--muted); }
</style>
