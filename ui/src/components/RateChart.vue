<script setup lang="ts">
import { computed } from 'vue';
import type { MonitorPoint } from '../api/dto';
import {
  axisMax,
  formatElapsed,
  formatRate,
  polyline,
  readings,
  reducePoints,
  timeSpan,
  timeTicks,
  valueTicks,
  type SeriesKey,
} from '../domain/monitor-chart';

/**
 * 速率曲线。**自绘 SVG，不引图表库**——一个图表库的体积会顶掉整个产物预算，
 * 而这里要画的只是两条折线。
 *
 * 无状态展示件：所有计算走 `domain/monitor-chart`（纯函数、有单测），
 * 这里只负责把算好的坐标摆进 SVG。
 *
 * # 刻度为什么用 HTML 而不是 SVG `<text>`
 *
 * 绘图区是 `preserveAspectRatio="none"` 拉伸的——曲线要占满容器宽度，而时间跨度
 * 和容器宽度没有固定比例。同一个 SVG 里的文字会跟着横向拉伸变形。所以刻度用
 * 定位在绘图区外的普通 HTML 元素：位置按百分比给，和网格线用**同一套分割**
 * （`valueTicks` / `timeTicks` 的 divisions 与下面 `DIVISIONS` 同源），
 * 不会出现「线在这里、数字标在那里」。
 */
const props = defineProps<{
  points: MonitorPoint[];
  /** 这一路实际下发的采样间隔（毫秒），标在图脚 */
  intervalMs?: number;
}>();

const W = 720;
const H = 160;
/** 每像素列压一次 min/max：尖峰不许被抽掉。 */
const COLS = W;
/** 网格与刻度的分割数。改这里，两条轴一起变。 */
const DIVISIONS = 4;

const span = computed(() => timeSpan(props.points));
const hasData = computed(() => props.points.length > 0);

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

/** 纵轴：上界 → 0，均分成 DIVISIONS 段。 */
const yTicks = computed(() =>
  valueTicks(max.value, DIVISIONS).map((value, index) => ({
    label: formatRate(value),
    // 第 0 个在顶（= 上界），最后一个在底（= 0）。
    top: `${(index / DIVISIONS) * 100}%`,
  })),
);

/** 横轴：会话开始后的秒数。相对时间——两端系统时钟不保证同步。 */
const xTicks = computed(() => {
  const { t0, t1 } = span.value;
  return timeTicks(t0, hasData.value ? t1 : t0, DIVISIONS).map((value, index) => ({
    label: formatElapsed(value),
    left: `${(index / DIVISIONS) * 100}%`,
    index,
  }));
});

const interval = computed(() =>
  props.intervalMs && props.intervalMs > 0 ? `${props.intervalMs} ms/样本` : '',
);

function fmt(v: number): string {
  return v >= 1000 ? `${(v / 1000).toFixed(2)} Gbps` : `${v.toFixed(1)} Mbps`;
}
</script>

<template>
  <div class="chart screen" data-label="SCOPE · 网卡速率">
    <div class="plot-row">
      <div class="y-axis" aria-hidden="true">
        <span v-for="tick in yTicks" :key="tick.top" class="y-tick" :style="{ top: tick.top }">
          {{ tick.label }}
        </span>
        <span class="y-unit">Mbps</span>
      </div>
      <div class="plot">
        <svg
          :viewBox="`0 0 ${W} ${H}`"
          preserveAspectRatio="none"
          role="img"
          :aria-label="`网卡速率曲线：RX 当前 ${fmt(rx.last)}，TX 当前 ${fmt(tx.last)}，纵轴上界 ${fmt(max)}`"
        >
          <line
            v-for="i in DIVISIONS - 1"
            :key="`h${i}`"
            x1="0"
            :y1="(H / DIVISIONS) * i"
            :x2="W"
            :y2="(H / DIVISIONS) * i"
            class="grid"
          />
          <line
            v-for="i in DIVISIONS - 1"
            :key="`v${i}`"
            :x1="(W / DIVISIONS) * i"
            y1="0"
            :x2="(W / DIVISIONS) * i"
            :y2="H"
            class="grid"
          />
          <polyline v-if="txLine" :points="txLine" class="tx" />
          <polyline v-if="rxLine" :points="rxLine" class="rx" />
        </svg>
        <p v-if="!hasData" class="waiting dim">等待第一批采样…</p>
        <div class="x-axis" aria-hidden="true">
          <span
            v-for="tick in xTicks"
            :key="tick.left"
            class="x-tick"
            :class="{ first: tick.index === 0, last: tick.index === DIVISIONS }"
            :style="{ left: tick.left }"
          >
            {{ tick.label }}
          </span>
        </div>
      </div>
    </div>

    <div class="readout mono">
      <span class="rx-k">RX</span> 当前 {{ fmt(rx.last) }} · 均 {{ fmt(rx.avg) }} · 峰 {{ fmt(rx.peak) }}
      <span class="sep">|</span>
      <span class="tx-k">TX</span> 当前 {{ fmt(tx.last) }} · 均 {{ fmt(tx.avg) }} · 峰 {{ fmt(tx.peak) }}
      <span class="sep">|</span>
      <span class="dim">
        {{ rx.samples }} 样本<template v-if="interval"> · {{ interval }}</template> · 上界
        {{ fmt(max) }}
      </span>
    </div>
    <p class="dim window">
      读数与曲线取同一段样本（当前缓冲全部）。横轴是「会话开始后」的相对时间——
      两端的系统时钟不保证同步。
    </p>
  </div>
</template>

<style scoped>
.chart { padding: 12px 14px 10px; }
.plot-row { display: flex; align-items: stretch; gap: 6px; }
/* 纵轴刻度：绝对定位在绘图区左侧，位置和网格线用同一套百分比。 */
.y-axis {
  position: relative;
  flex: 0 0 auto;
  width: 46px;
  height: 160px;
}
.y-tick {
  position: absolute;
  right: 4px;
  transform: translateY(-50%);
  font: 10px/1 var(--fm);
  font-variant-numeric: tabular-nums;
  color: var(--screen-dim);
  white-space: nowrap;
}
.y-unit {
  position: absolute;
  right: 4px;
  bottom: -14px;
  font: 10px/1 var(--fm);
  color: var(--screen-dim);
}
.plot { position: relative; flex: 1 1 auto; min-width: 0; padding-bottom: 16px; }
svg { display: block; width: 100%; height: 160px; }
.grid { stroke: var(--scope-grid); stroke-width: 1; }
.rx { fill: none; stroke: var(--signal); stroke-width: 1.5; vector-effect: non-scaling-stroke; }
.tx { fill: none; stroke: var(--scope-axis); stroke-width: 1.2; vector-effect: non-scaling-stroke; }
.waiting {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  margin: 0;
  font-size: 12px;
}
.x-axis { position: relative; height: 14px; }
.x-tick {
  position: absolute;
  top: 2px;
  transform: translateX(-50%);
  font: 10px/1 var(--fm);
  font-variant-numeric: tabular-nums;
  color: var(--screen-dim);
  white-space: nowrap;
}
/* 两端的刻度贴边，免得被裁掉半个字。 */
.x-tick.first { transform: none; left: 0 !important; }
.x-tick.last { transform: translateX(-100%); }
.readout { margin-top: 10px; font-size: 12px; color: var(--screen-ink); }
.rx-k { color: var(--signal); font-weight: 700; }
.tx-k { color: var(--scope-axis); font-weight: 700; }
.sep { margin: 0 8px; color: var(--screen-dim); }
.dim { color: var(--screen-dim); }
.window { margin: 4px 0 0; font-size: 11px; }
</style>
