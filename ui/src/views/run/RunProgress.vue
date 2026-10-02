<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch, watchEffect } from 'vue';
import { freshnessLabel } from '../../domain/freshness';
import {
  filterByVerdict,
  humanDuration,
  unitRateLabel,
  unitRateTone,
  unitSearchFields,
  verdictTone,
} from '../../domain/progress';
import type { VerdictFilter } from '../../domain/progress';
import { filterByQuery, visibleCountLabel } from '../../domain/search';
import { LOG_MAX_LINES, openReport, run, skipUnit, startPolling, stop, syncStatus, view } from '../../state/run';
import { syncProgressRun, ui } from '../../state/ui';

/**
 * 「执行」页的进度面板：消费 `RunStatus`，**不解析任何日志行**。
 *
 * 一次 11.5 小时、210 单元的测试有三万行日志，刷新一次页面就要全量重放才能
 * 重建进度。v6.0 让 Rust 直接吐结构化状态（ADR-2），于是这里只做展示。
 */
const emit = defineEmits<{ prepare: [] }>();

/** 这一屏的数据是什么时候的。断线期间数字不动，只有这句话在动。 */
const freshness = computed(() => freshnessLabel(run.lastSyncAt));
// 换轮就把筛选/搜索/选中整组清掉；同一轮之内这是空操作。
watchEffect(() => syncProgressRun(run.status.run_id));

/** 一行可点筛选。数字全部取自**服务端 counts**，不用列表长度重算。 */
const filterChips = computed<Array<{ id: VerdictFilter; label: string; n: number }>>(() => {
  const c = run.status.counts;
  return [
    { id: 'all', label: '全部', n: view.value.done },
    { id: 'attention', label: '需处置', n: c.fail + c.not_evaluated + c.setup_error },
    { id: 'pass', label: '通过', n: c.pass },
    { id: 'fail', label: '未达标', n: c.fail },
    { id: 'not_evaluated', label: '未评估', n: c.not_evaluated },
    { id: 'setup_error', label: '准备失败', n: c.setup_error },
    { id: 'measured', label: '仅测量', n: c.measured },
    { id: 'skip', label: '已跳过', n: c.skip },
  ];
});

/** 筛选与搜索取交集；两者都保持 seq 顺序。 */
const shownUnits = computed(() =>
  filterByQuery(filterByVerdict(run.units, ui.progressFilter), ui.progress.query, unitSearchFields),
);
const unitCountLabel = computed(() => visibleCountLabel(shownUnits.value.length, run.units.length));
/**
 * 服务端汇总与本地已载入的单元数**分别说**。
 *
 * 增量还没追平时两者会差一截。用列表长度去重算服务端的通过数，屏幕上会出现
 * 一个既不是服务端说的、也不是本地看到的第三个数字。
 */
const loadedNote = computed(() =>
  run.units.length !== view.value.done ? `本页已载入 ${run.units.length} / ${view.value.done}` : '',
);

function toggleDetail(seq: number): void {
  ui.progress.selected = ui.progress.selected === String(seq) ? '' : String(seq);
}

const stateLabel = computed(() => {
  if (view.value.aborted) return '已中止';
  if (run.running) return '正在执行';
  if (view.value.finished) return '本轮结束';
  if (!run.status.run_id) return '未产生测试单元';
  return '等待状态更新';
});

// ---- 日志跟随 ----
//
// 默认跟到最新；**用户一往上翻就停下**。抢滚动位置是这一页最烦人的一件事。
const logBox = ref<HTMLElement | null>(null);
const following = ref(true);
const hasNewLines = ref(false);
function atBottom(el: HTMLElement): boolean {
  // 留 8px 余量：不同缩放比例下 scrollTop 会有零点几像素的误差。
  return el.scrollTop + el.clientHeight >= el.scrollHeight - 8;
}
function onLogScroll(): void {
  const el = logBox.value;
  if (!el) return;
  following.value = atBottom(el);
  if (following.value) hasNewLines.value = false;
}
function jumpToLatest(): void {
  const el = logBox.value;
  if (!el) return;
  el.scrollTop = el.scrollHeight;
  following.value = true;
  hasNewLines.value = false;
}
watch(
  () => run.lines,
  async () => {
    if (!following.value) {
      hasNewLines.value = true;
      return;
    }
    await nextTick();
    const el = logBox.value;
    if (el) el.scrollTop = el.scrollHeight;
  },
);

/**
 * 「跳过」「停止」的说法按**确知程度**给，不按点击给。
 *
 * HTTP 200 只说明请求被受理，当前单元还要等工具退出、结果落盘。
 */
const skipLabel = computed(() => {
  if (run.skipPhase === 'sending') return '正在请求跳过…';
  if (run.skipPhase === 'accepted') return '已请求跳过';
  if (run.skipPhase === 'unknown') return '跳过结果未确认';
  return '跳过当前单元';
});
const stopLabel = computed(() => {
  if (run.stopPhase === 'sending') return '正在请求停止…';
  if (run.stopPhase === 'accepted') return '已请求停止';
  if (run.stopPhase === 'unknown') return '停止结果未确认';
  return '停止测试';
});

// 轮询归 state 模块所有：这里只保证它开着，**不在卸载时停**。
onMounted(startPolling);
</script>

<template>
  <div v-if="!run.synced" class="empty-state" role="status">
    <p>尚未读到主控运行状态。</p>
    <button type="button" class="ghost" @click="syncStatus">同步运行状态</button>
  </div>

  <div v-else class="progress">
    <section class="run-summary" aria-label="本轮运行进度">
      <div class="run-heading">
        <div>
          <span class="run-state" :class="{ running: run.running, aborted: view.aborted }">{{ stateLabel }}</span>
          <span v-if="run.status.run_id" class="run-id mono">{{ run.status.run_id }}</span>
        </div>
        <div class="run-actions">
          <span class="freshness muted mono" title="最近一次读到运行状态的时刻">{{ freshness }}</span>
          <button
            v-if="run.running && run.stopPhase === 'idle'"
            type="button"
            class="ghost"
            :disabled="!run.status.run_id || !run.status.current || run.skipPhase !== 'idle'"
            title="停止当前单元，继续下一个"
            @click="skipUnit"
          >{{ skipLabel }}</button>
          <button
            v-if="run.running || run.stopPhase !== 'idle'"
            type="button"
            class="ghost danger"
            :disabled="run.stopPhase !== 'idle'"
            @click="stop"
          >{{ stopLabel }}</button>
          <button v-if="view.finished && run.report" type="button" class="ghost" @click="openReport">打开报告</button>
          <button v-if="!run.running && run.startPhase !== 'unknown'" type="button" @click="emit('prepare')">准备下一轮</button>
        </div>
      </div>
      <div class="progress-labels">
        <span><strong class="progress-count">{{ view.done }}</strong><span class="muted"> / {{ view.total }} 个单元已完成</span></span>
        <span class="timing muted">
          <span v-if="view.eta">剩余 {{ view.eta }}</span>
          <span v-if="view.finishHint">{{ view.finishHint }}</span>
        </span>
      </div>
      <div
        class="bar-wrap"
        role="progressbar"
        aria-label="已完成的测试单元"
        :aria-valuenow="view.done"
        :aria-valuemin="0"
        :aria-valuemax="view.total || 1"
        :aria-valuetext="`${view.done} / ${view.total} 个单元已完成`"
      >
        <div class="bar" :style="{ width: `${Math.round(view.ratio * 100)}%` }"></div>
      </div>
      <div v-if="view.currentSeq" class="current-unit">
        <span class="current-label">当前 <span class="mono">#{{ view.currentSeq }}</span></span>
        <strong>{{ view.currentTitle }}</strong>
      </div>
    </section>

    <p v-if="run.refreshError" class="msg warn" role="status">
      连接中断，正在重试；下面是 {{ freshness }} 的数据。（{{ run.refreshError }}）
    </p>
    <p v-if="run.stopPhase === 'accepted'" class="msg warn" role="status">已请求停止，等待当前单元收尾；已完成的结果会进报告。</p>
    <div v-if="run.stopPhase === 'unknown'" class="msg warn" role="alert">
      <p>停止结果未确认，请同步运行状态确认本轮是否仍在运行。</p>
      <button type="button" class="ghost small" @click="syncStatus">同步运行状态</button>
    </div>
    <p v-if="run.skipPhase === 'accepted'" class="msg warn" role="status">已请求跳过当前单元，正在收尾。</p>
    <p v-if="run.skipPhase === 'unknown'" class="msg warn" role="alert">跳过结果未确认，请先同步运行状态再操作。</p>
    <p v-if="run.skipError" class="msg bad" role="alert">跳过失败：{{ run.skipError }}</p>
    <p v-if="run.stopError" class="msg bad" role="alert">停止失败：{{ run.stopError }}</p>
    <p v-if="run.reportError" class="msg bad" role="alert">打开报告失败：{{ run.reportError }}</p>
    <p v-if="view.aborted" class="msg bad" role="alert">
      连续多个灌包单元没有任何测量，已在第 {{ run.status.aborted_at_unit }} 个单元中止剩余队列。
      先确认被测设备是否掉线或重启，再重跑剩余项；已完成的部分照常出报告。
    </p>

    <div class="chips" role="group" aria-label="按判定筛选已完成单元">
      <button
        v-for="chip in filterChips"
        :key="chip.id"
        type="button"
        class="chip"
        :class="[chip.id, { on: ui.progressFilter === chip.id }]"
        :aria-pressed="ui.progressFilter === chip.id"
        @click="ui.progressFilter = chip.id"
      >
        {{ chip.label }} <strong>{{ chip.n }}</strong>
      </button>
    </div>

    <div class="toolbar">
      <label v-if="run.units.length > 4" class="grow">
        <span class="sr-only">搜索已完成单元</span>
        <input
          type="search"
          :value="ui.progress.query"
          placeholder="搜序号、标题、链路或原因码"
          @input="ui.progress.query = ($event.target as HTMLInputElement).value"
        />
      </label>
      <span class="count">{{ unitCountLabel }}<template v-if="loadedNote"> · {{ loadedNote }}</template></span>
      <button
        v-if="ui.progress.query || ui.progressFilter !== 'all'"
        type="button"
        class="ghost small"
        @click="ui.progress.query = ''; ui.progressFilter = 'all'"
      >清空筛选</button>
    </div>

    <p v-if="run.units.length === 0" class="empty-state">单元完成后在这里列出判定和耗时。</p>
    <p v-else-if="shownUnits.length === 0" class="empty-state" role="status">没有单元匹配当前筛选。</p>
    <div v-else class="table-wrap" tabindex="0" role="region" aria-label="已完成单元列表，可横向滚动">
      <table class="data">
        <thead>
          <tr>
            <th scope="col">#</th><th scope="col">判定</th><th scope="col">标题</th><th scope="col">链路集合</th>
            <th scope="col" class="num">实测 / 门限</th><th scope="col" class="num">耗时</th>
          </tr>
        </thead>
        <tbody>
          <template v-for="unit in shownUnits" :key="unit.seq">
            <tr :class="{ picked: String(unit.seq) === ui.progress.selected }">
              <td class="mono">
                <button
                  type="button"
                  class="pick"
                  :aria-expanded="String(unit.seq) === ui.progress.selected"
                  :aria-label="`#${unit.seq} 详情`"
                  @click="toggleDetail(unit.seq)"
                >{{ unit.seq }}</button>
              </td>
              <td><span class="verdict" :class="verdictTone(unit.verdict)">{{ unit.verdict }}</span></td>
              <td class="result-title">{{ unit.title }}</td>
              <td class="muted">{{ unit.link_group || '—' }}</td>
              <td class="num mono rate" :class="unitRateTone(unit) ?? ''">{{ unitRateLabel(unit) || '—' }}</td>
              <td class="num mono">{{ humanDuration(unit.secs) }}</td>
            </tr>
            <tr v-if="String(unit.seq) === ui.progress.selected" class="detail-row">
              <td colspan="6">
                <dl>
                  <dt>原因码</dt><dd class="mono">{{ unit.reason_code || '—' }}</dd>
                  <dt>原因</dt><dd>{{ unit.reason_detail || '—' }}</dd>
                </dl>
                <p class="hint">P10、采样覆盖率和完整曲线在报告里。</p>
              </td>
            </tr>
          </template>
        </tbody>
      </table>
    </div>

    <details class="log-section">
      <summary><strong>运行日志</strong></summary>
      <div class="log-bar">
        <span class="hint">只保留最近 {{ LOG_MAX_LINES }} 行</span>
        <button v-if="!following" type="button" class="ghost small" @click="jumpToLatest">
          {{ hasNewLines ? '有新日志 · 回到最新' : '回到最新' }}
        </button>
      </div>
      <div
        ref="logBox"
        class="screen log"
        data-label="主控输出"
        tabindex="0"
        role="region"
        aria-label="运行日志，可滚动"
        @scroll="onLogScroll"
      >
        <div v-if="run.lines.length === 0" class="dim">等待主控输出…</div>
        <div v-for="(line, i) in run.lines" :key="i">{{ line }}</div>
      </div>
    </details>
  </div>
</template>

<style scoped>
.run-summary { margin: 0 0 12px; padding: 18px 20px; border: 1px solid var(--line); border-radius: 8px; background: var(--panel-2); }
.run-heading { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
.run-state { display: inline-flex; align-items: center; gap: 8px; font-size: 14px; font-weight: 700; }
.run-state::before { content: ''; width: 8px; height: 8px; border-radius: 50%; background: var(--muted); }
.run-state.running::before { background: var(--accent); }
.run-state.aborted { color: var(--bad); }
.run-state.aborted::before { background: var(--bad); }
.run-id { margin-left: 10px; font-size: 11.5px; color: var(--muted); overflow-wrap: anywhere; }
.run-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.freshness { font-size: 12px; white-space: nowrap; }
.progress-labels { display: flex; flex-wrap: wrap; align-items: baseline; justify-content: space-between; gap: 8px; margin: 18px 0 8px; font-size: 12px; }
.progress-count { font-size: 24px; font-weight: 650; font-variant-numeric: tabular-nums; }
.timing { display: flex; gap: 12px; flex-wrap: wrap; }
.bar-wrap { height: 8px; border-radius: 4px; background: var(--line); overflow: hidden; }
/* 纯宽度变化，无 transition：这台机器此刻正在灌线速。 */
.bar { height: 100%; background: var(--accent); }
.current-unit { margin-top: 14px; padding-top: 12px; border-top: 1px solid var(--line); overflow-wrap: anywhere; }
.current-label { margin-right: 8px; color: var(--muted); font-size: 12px; }
.current-label .mono { color: var(--accent); }
.current-unit > strong { font-size: 14px; font-weight: 600; }
/* 实测值的着色只在**有门限可比**时出现：Observe 模式没有门限，满屏绿色会被读成「全部达标」。 */
.rate.over { color: var(--ok); }
.rate.under { color: var(--bad); }
.result-title { min-width: 220px; overflow-wrap: anywhere; }
table { min-width: 640px; }
tbody tr:hover { background: var(--panel-2); }
tr.picked { background: var(--info-bg); }
.detail-row td { background: var(--panel-2); }
.detail-row dl { display: grid; grid-template-columns: auto minmax(0, 1fr); gap: 4px 14px; margin: 0; font-size: 12.5px; }
.detail-row dt { color: var(--muted); white-space: nowrap; }
.detail-row dd { margin: 0; overflow-wrap: anywhere; }
.detail-row .hint { margin: 8px 0 0; }
.pick { padding: 0; min-height: 0; font: inherit; font-family: var(--fm); color: var(--accent); background: none; border: 0; text-decoration: underline; text-underline-offset: 3px; }
.pick:hover:not(:disabled) { background: none; color: var(--accent-hover); }
.pick[aria-expanded='true'] { color: var(--ink); text-decoration: none; font-weight: 700; }
.chips { display: flex; flex-wrap: wrap; gap: 8px; margin: 16px 0 4px; }
.chip {
  display: inline-flex; align-items: center; gap: 7px;
  min-height: 32px; padding: 5px 12px; font-size: 12.5px; font-weight: 500;
  color: var(--ink); background: var(--surface);
  border: 1px solid var(--line); border-radius: 999px;
}
.chip strong { font-variant-numeric: tabular-nums; }
.chip:hover:not(:disabled) { background: var(--head); }
.chip.on { color: var(--on-accent); background: var(--accent); border-color: var(--accent); }
.chip.on:hover:not(:disabled) { background: var(--accent-hover); }
.chip.fail strong, .chip.attention strong { color: var(--bad); }
.chip.pass strong { color: var(--ok); }
.chip.on strong { color: inherit; }
.log-section { margin-top: 20px; border-top: 1px solid var(--line); }
.log-section > summary { padding: 13px 0; }
.log { max-height: 320px; margin-top: 8px; overflow: auto; white-space: pre-wrap; overflow-wrap: anywhere; }
.log-bar { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
@media (max-width: 600px) {
  .run-summary { padding: 14px; }
  .progress-labels { align-items: flex-start; flex-direction: column; }
}
</style>
