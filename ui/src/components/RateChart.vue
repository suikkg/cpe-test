<script setup lang="ts">
import { computed } from 'vue';
import type { MonitorPoint } from '../api/dto';
import {
  axisMax,
  polyline,
  readings,
  reducePoints,
  timeSpan,
  type SeriesKey,
} from '../domain/monitor-chart';

/**
 * 速率曲线。**自绘 SVG，不引图表库**——一个图表库的体积会顶掉整个产物预算，
 * 而这里要画的只是两条折线。
 *
 * 无状态展示件：所有计算走 `domain/monitor-chart`（纯函数、有单测），
 * 这里只负责把算好的坐标摆进 SVG。
 */
const props = defineProps<{ points: MonitorPoint[] }>();

const W = 720;
const H = 160;
/** 每像素列压一次 min/max：尖峰不许被抽掉。 */
const COLS = W;

const span = computed(() => timeSpan(props.points));

// **曲线、均值、峰值同一批点**——三者取不同窗口时读数和图对不上。
const rx = computed(() => readings(props.points, 'rx_mbps'));
const tx = computed(() => readings(props.points, 'tx_mbps'));
const max = computed(() => axisMax(Math.max(rx.value.peak, tx.value.peak)));

function line(key: SeriesKey): string {
  const { t0, span: s } = span.value;
  return polyline(reducePoints(props.points, key, t0, s, COLS), t0, s, max.value, W, H);
}

const rxLine = computed(() => line('rx_mbps'));
const txLine = computed(() => line('tx_mbps'));

function fmt(v: number): string {
  return v >= 1000 ? `${(v / 1000).toFixed(2)} Gbps` : `${v.toFixed(1)} Mbps`;
}
</script>

<template>
  <div class="chart screen" data-label="SCOPE · 网卡速率">
    <svg :viewBox="`0 0 ${W} ${H}`" preserveAspectRatio="none" role="img" aria-label="网卡速率曲线">
      <line v-for="i in 3" :key="i" x1="0" :y1="(H / 4) * i" :x2="W" :y2="(H / 4) * i" class="grid" />
      <polyline v-if="txLine" :points="txLine" class="tx" />
      <polyline v-if="rxLine" :points="rxLine" class="rx" />
    </svg>
    <div class="readout mono">
      <span class="rx-k">RX</span> 当前 {{ fmt(rx.last) }} · 均 {{ fmt(rx.avg) }} · 峰 {{ fmt(rx.peak) }}
      <span class="sep">|</span>
      <span class="tx-k">TX</span> 当前 {{ fmt(tx.last) }} · 均 {{ fmt(tx.avg) }} · 峰 {{ fmt(tx.peak) }}
      <span class="sep">|</span>
      <span class="dim">{{ rx.samples }} 样本 · 上界 {{ fmt(max) }}</span>
    </div>
    <p class="dim window">读数与曲线取同一段样本（当前缓冲全部）。</p>
  </div>
</template>

<style scoped>
.chart { padding: 12px 14px 10px; }
svg { display: block; width: 100%; height: 160px; }
.grid { stroke: var(--scope-grid); stroke-width: 1; }
.rx { fill: none; stroke: var(--signal); stroke-width: 1.5; vector-effect: non-scaling-stroke; }
.tx { fill: none; stroke: var(--scope-axis); stroke-width: 1.2; vector-effect: non-scaling-stroke; }
.readout { margin-top: 8px; font-size: 12px; color: var(--screen-ink); }
.rx-k { color: var(--signal); font-weight: 700; }
.tx-k { color: var(--scope-axis); font-weight: 700; }
.sep { margin: 0 8px; color: var(--screen-dim); }
.dim { color: var(--screen-dim); }
.window { margin: 4px 0 0; font-size: 11px; }
</style>
