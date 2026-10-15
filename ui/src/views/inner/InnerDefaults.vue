<script setup lang="ts">
import { computed } from 'vue';
import InnerPingOptions from './InnerPingOptions.vue';
import { inner } from '../../state/inner';
import { defaultInnerPingPolicy, type InnerPingPolicy } from '../../domain/inner-options';
const policy = computed(() => inner.config.ping_policy!);
const rows: {label:string; avg:keyof InnerPingPolicy; max:keyof InnerPingPolicy}[] = [
  {label:'有线 · small',avg:'wired_small_avg_rtt_ms',max:'wired_small_max_rtt_ms'},
  {label:'有线 · medium',avg:'wired_medium_avg_rtt_ms',max:'wired_medium_max_rtt_ms'},
  {label:'有线 · large',avg:'wired_large_avg_rtt_ms',max:'wired_large_max_rtt_ms'},
  {label:'Wi-Fi · small',avg:'wifi_small_avg_rtt_ms',max:'wifi_small_max_rtt_ms'},
  {label:'Wi-Fi · medium',avg:'wifi_medium_avg_rtt_ms',max:'wifi_medium_max_rtt_ms'},
  {label:'Wi-Fi · large',avg:'wifi_large_avg_rtt_ms',max:'wifi_large_max_rtt_ms'},
];
</script>
<template>
  <div>
    <div class="toolbar">
      <label class="field">默认时长（秒）<input v-model.number="inner.config.duration_secs" type="number" min="1" max="86400"></label>
      <label class="field">默认并行流数<input v-model.number="inner.config.parallel" type="number" min="1" max="32"></label>
      <label class="field">接收端起始端口<input v-model.number="inner.config.port" type="number" min="1024" max="65534"></label>
    </div>
    <p class="hint">门限优先级：任务 › 网口协议 › 网口公共。工具门限与网卡门限独立。</p>
    <InnerPingOptions v-if="inner.config.ping_options" :options="inner.config.ping_options" />
    <details>
      <summary>Ping 门限</summary>
      <div class="toolbar"><button v-if="!inner.config.ping_policy" class="ghost small" @click="inner.config.ping_policy = defaultInnerPingPolicy()">自定义 Ping 门限</button><button v-else class="ghost small" @click="inner.config.ping_policy = null">恢复默认 Ping 门限</button></div>
      <template v-if="inner.config.ping_policy">
        <div class="toolbar"><label class="field">small 包长上限（字节）<input v-model.number="policy.small_max_bytes" type="number" min="1" max="65499"></label><label class="field">medium 包长上限（字节）<input v-model.number="policy.medium_max_bytes" type="number" min="2" max="65500"></label></div>
        <div class="table-wrap"><table class="data"><thead><tr><th scope="col">网络 / 包长</th><th scope="col">平均 RTT 门限 ms</th><th scope="col">最大 RTT 门限 ms</th></tr></thead><tbody><tr v-for="row in rows" :key="row.label"><th scope="row">{{ row.label }}</th><td><input v-model.number="policy[row.avg]" :aria-label="`${row.label} 平均 RTT 门限 ms`" type="number" min="0" step="any"></td><td><input v-model.number="policy[row.max]" :aria-label="`${row.label} 最大 RTT 门限 ms`" type="number" min="0" step="any"></td></tr></tbody></table></div>
      </template>
    </details>
  </div>
</template>
