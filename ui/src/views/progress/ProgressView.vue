<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch, watchEffect } from 'vue';
import { freshnessLabel } from '../../domain/freshness';
import {
  failuresByLinkGroup,
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
import { goto, syncProgressRun, ui } from '../../state/ui';

/**
 * 「进度」：消费 `RunStatus`，**不解析任何日志行**。
 *
 * v5.0 原本计划让这一页去解析 `[i/total]` 和「==> 单元结果:」两种日志行，
 * 并用 Rust 测试把日志格式钉死当协议。一次 11.5 小时、210 单元的测试有三万行
 * 日志，刷新一次页面就要全量重放才能重建进度。v6.0 让 Rust 直接吐结构化状态
 * （ADR-2），于是这里只做展示，日志文案彻底自由。
 */

const failures = computed(() => failuresByLinkGroup(run.units));
/** 这一屏的数据是什么时候的。断线期间数字不动，只有这句话在动。 */
const freshness = computed(() => freshnessLabel(run.lastSyncAt));
/**
 * 「停止」按钮的说法按**确知程度**给，不按点击给。
 *
 * HTTP 200 只说明请求被受理，收尾还要等当前单元的工具退出、报告落盘。
 * 写成「已停止」会先说结束、随后被轮询翻回运行中。
 */
// 换轮就把筛选/搜索/选中整组清掉；同一轮之内这是空操作。
watchEffect(() => syncProgressRun(run.status.run_id));

/** 那一行可点筛选。数字全部取自**服务端 counts**，不用列表长度重算。 */
const filterChips = computed<Array<{ id: VerdictFilter; label: string; n: number }>>(() => {
  const c = run.status.counts;
  return [
    { id: 'all', label: '全部', n: view.value.done },
    { id: 'fail', label: '未达标', n: c.fail },
    { id: 'not_evaluated', label: '未评估', n: c.not_evaluated },
    { id: 'setup_error', label: '准备失败', n: c.setup_error },
    { id: 'pass', label: '通过', n: c.pass },
    { id: 'measured', label: '仅测量', n: c.measured },
    { id: 'skip', label: '已跳过', n: c.skip },
  ];
});

/** 筛选与搜索取交集；两者都保持 seq 顺序。 */
const shownUnits = computed(() =>
  filterByQuery(
    filterByVerdict(run.units, ui.progressFilter),
    ui.progress.query,
    unitSearchFields,
  ),
);
const unitCountLabel = computed(() => visibleCountLabel(shownUnits.value.length, run.units.length));
/**
 * 服务端汇总与本地已载入的单元数**分别说**（§11.6）。
 *
 * 增量还没追平时两者会差一截。用列表长度去重算服务端的通过数，屏幕上会出现
 * 一个既不是服务端说的、也不是本地看到的第三个数字。
 */
const loadedNote = computed(() =>
  run.units.length !== view.value.done
    ? `服务端汇总 ${view.value.done} 个，本页已载入 ${run.units.length} 个`
    : '',
);
const selectedUnit = computed(
  () => run.units.find((unit) => String(unit.seq) === ui.progress.selected) ?? null,
);
const selectedHidden = computed(
  () =>
    !!selectedUnit.value &&
    !shownUnits.value.some((unit) => String(unit.seq) === ui.progress.selected),
);
// ---- 日志跟随（§11.6）----
//
// 默认跟到最新；**用户一往上翻就停下**。抢滚动位置是这一页最烦人的一件事：
// 正在读一条报错，下一拍新日志到了，屏幕直接跳走。停下之后只用一个按钮说
// 「有新日志」，去不去由用户决定。
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

function pickUnit(seq: number): void {
  ui.progress.selected = ui.progress.selected === String(seq) ? '' : String(seq);
}

/**
 * 「跳过当前单元」按钮的说法，同样按**确知程度**给，不按点击给。
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
// 用户会在 11.5 小时里切去看网卡和监控，切走就断轮询等于回来时进度是空的。
onMounted(startPolling);
</script>

<template>
  <section class="view">
    <header class="view-head">
      <h2>进度</h2>
      <p class="muted">查看当前测试、已完成结果和需要处置的单元。切换页面后进度会继续更新。</p>
    </header>

    <div v-if="!run.synced" class="empty" role="status">
      <strong>运行状态待同步</strong>
      <p>
        还没读到主控上的运行状态。在读到之前这里不显示「空闲」——那两件事在屏幕上长得一样，
        而它们的下一步相反。
      </p>
      <button type="button" class="ghost" @click="syncStatus">同步运行状态</button>
    </div>

    <div v-else-if="!run.status.run_id && !run.running && !run.startError && !run.lines.length" class="empty">
      <strong>等待开始第一轮测试</strong>
      <p>在「执行」页预览并开始测试后，这里会显示实时进度与结果。</p>
      <button type="button" @click="goto('run')">前往执行</button>
    </div>

    <template v-else-if="run.synced">
      <section class="run-summary" aria-label="本轮运行进度">
        <div class="run-heading">
          <div>
            <span class="run-state" :class="{ running: run.running, aborted: view.aborted }">
              {{ view.aborted ? '已中止' : run.running ? '正在执行' : view.finished ? '本轮结束' : run.startError ? '启动未完成，请查看运行日志' : '等待状态更新' }}
            </span>
            <div v-if="run.status.run_id" class="run-id mono">{{ run.status.run_id }}</div>
          </div>
          <div class="run-actions">
            <span class="freshness muted mono" :title="'最近一次成功读到运行状态的时刻'">{{ freshness }}</span>
            <button
              v-if="run.running && run.stopPhase === 'idle'"
              type="button"
              class="ghost"
              :disabled="!run.status.run_id || !run.status.current || run.skipPhase !== 'idle'"
              title="掐断当前这一个单元，队列继续跑下一个。已请求停止整轮时这个按钮不出现。"
              @click="skipUnit"
            >{{ skipLabel }}</button>
            <button
              v-if="run.running || run.stopPhase !== 'idle'"
              type="button"
              class="ghost"
              :disabled="run.stopPhase !== 'idle'"
              @click="stop"
            >{{ stopLabel }}</button>
            <button v-if="view.finished && run.report" type="button" @click="openReport">打开报告</button>
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
          <span class="current-label">当前单元 <span class="mono">#{{ view.currentSeq }}</span></span>
          <strong>{{ view.currentTitle }}</strong>
        </div>
      </section>

      <p v-if="run.refreshError" class="warn" role="status">
        连接暂时中断，正在按原节奏重试。下面显示的是 <strong>{{ freshness }}</strong> 的数据，
        不是刚刚的；已完成的单元和日志都没有丢。（{{ run.refreshError }}）
      </p>
      <p v-if="run.stopPhase === 'accepted'" class="warn" role="status">
        已请求停止，等待当前单元收尾。<strong>收到请求不等于已经停下</strong>——以运行状态为准，
        已完成的部分照常出报告。
      </p>
      <div v-if="run.stopPhase === 'unknown'" class="warn" role="alert">
        <p>
          停止请求没有拿到应答，<strong>无法确认它有没有到达主控</strong>。不会自动重发——
          重发有可能停掉下一轮。请先同步一次运行状态再决定。
        </p>
        <button type="button" class="ghost" @click="syncStatus">同步运行状态</button>
      </div>
      <p v-if="run.skipPhase === 'accepted'" class="warn" role="status">
        已请求跳过当前单元，等待收尾。诊断会标记「被操作员手动跳过」，
        是否取得有效测量、能否判定，以最终结果为准。
      </p>
      <p v-if="run.skipPhase === 'unknown'" class="warn" role="alert">
        跳过请求没有拿到应答，无法确认它有没有到达主控。<strong>不会自动重发</strong>——
        重发有可能把下一个单元也跳掉。请先同步一次运行状态再决定。
      </p>
      <p v-if="run.skipError" class="bad" role="alert">跳过失败：{{ run.skipError }}</p>
      <p v-if="run.stopError" class="bad" role="alert">停止失败：{{ run.stopError }}</p>
      <p v-if="run.reportError" class="bad" role="alert">打开报告失败：{{ run.reportError }}</p>
      <p v-if="run.startError" class="bad" role="alert">{{ run.startError }}</p>
      <p v-if="view.aborted" class="bad" role="alert">
        连续多个灌包单元没有产生任何测量，已在第 {{ run.status.aborted_at_unit }} 个单元中止剩余队列。
        先确认被测设备是否掉线或重启，再重跑剩余项；已完成的部分会照常出报告。
      </p>

      <!-- 统计不再是六张大数字卡：它们只能看，而人看完统计的下一个动作永远是
           「把这一类找出来」。改成一行可点筛选，数字仍全部来自服务端 counts。 -->
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

      <template v-if="failures.length">
        <h3>需要处置</h3>
        <p class="muted hint">
          RATE_FAIL 表示未达标；NOT_EVALUATED 与 SETUP_ERROR 表示本轮无法判定，请先排查环境或执行问题。
        </p>
        <div v-for="group in failures" :key="group.group" class="fail-group">
          <div class="fail-head"><strong>{{ group.group }}</strong><span>{{ group.units.length }} 个待处置</span></div>
          <ul>
            <li v-for="unit in group.units" :key="unit.seq">
              <span class="seq mono">#{{ unit.seq }}</span>
              <span class="verdict" :class="verdictTone(unit.verdict)">{{ unit.verdict }}</span>
              <span class="fail-title">{{ unit.title }}</span>
              <!-- 「差多少」是看到 RATE_FAIL 之后的第一个问题：差 2% 和差一个
                   数量级对应完全不同的处置。 -->
              <small v-if="unitRateLabel(unit)" class="rate mono under">{{ unitRateLabel(unit) }}</small>
              <small v-if="unit.reason_code" class="reason muted mono">{{ unit.reason_code }}</small>
            </li>
          </ul>
        </div>
      </template>

      <div class="section-heading">
        <h3>已完成</h3>
        <span class="muted">{{ unitCountLabel }}</span>
        <span v-if="loadedNote" class="muted">· {{ loadedNote }}</span>
        <label v-if="run.units.length > 4" class="unit-search">
          <span class="sr-only">搜索已完成单元</span>
          <input
            type="search"
            :value="ui.progress.query"
            placeholder="搜序号、标题、链路或原因码"
            @input="ui.progress.query = ($event.target as HTMLInputElement).value"
          />
        </label>
        <button
          v-if="ui.progress.query || ui.progressFilter !== 'all'"
          type="button"
          class="ghost small"
          @click="ui.progress.query = ''; ui.progressFilter = 'all'"
        >
          清空筛选
        </button>
      </div>

      <p v-if="selectedHidden" class="hint" role="status">
        选中的单元不在当前筛选结果里，下面的详情仍是它。
      </p>

      <div v-if="selectedUnit" class="unit-detail" aria-label="单元详情">
        <div class="unit-detail-head">
          <span class="seq mono">#{{ selectedUnit.seq }}</span>
          <span class="verdict" :class="verdictTone(selectedUnit.verdict)">{{ selectedUnit.verdict }}</span>
          <button type="button" class="ghost small" @click="ui.progress.selected = ''">收起详情</button>
        </div>
        <p class="unit-detail-title">{{ selectedUnit.title }}</p>
        <dl>
          <dt>链路集合</dt><dd>{{ selectedUnit.link_group || '—' }}</dd>
          <dt>实测 / 门限</dt>
          <dd class="mono" :class="unitRateTone(selectedUnit) ?? ''">
            {{ unitRateLabel(selectedUnit) || '—' }}
          </dd>
          <dt>耗时</dt><dd class="mono">{{ humanDuration(selectedUnit.secs) }}</dd>
          <dt>原因码</dt><dd class="mono">{{ selectedUnit.reason_code || '—' }}</dd>
          <dt>原因</dt><dd>{{ selectedUnit.reason_detail || '—' }}</dd>
        </dl>
        <p class="muted small">
          <!-- 单元级的 RX 平均与门限现在在运行状态里（与报告汇总行同源）。
               P10、采样覆盖率、逐样本 CSV 仍然只在报告里——缺的照旧说缺。 -->
          这里的实测值就是判定用的那个数。要看 P10、采样覆盖率和逐样本曲线，
          等本轮结束后打开报告。
        </p>
      </div>

      <div v-if="run.units.length === 0" class="empty compact">单元完成后，判定和耗时会显示在这里。</div>
      <p v-else-if="shownUnits.length === 0" class="empty compact" role="status">
        没有单元匹配当前筛选。已完成 {{ run.units.length }} 个。
      </p>
      <div v-else class="scroll" tabindex="0" role="region" aria-label="已完成单元列表，可横向滚动">
        <table>
          <thead>
            <tr>
              <th scope="col">#</th><th scope="col">判定</th><th scope="col">标题</th><th scope="col">链路集合</th><th scope="col" class="num">实测 / 门限</th><th scope="col" class="num">耗时</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="unit in shownUnits"
              :key="unit.seq"
              :class="{ picked: String(unit.seq) === ui.progress.selected }"
            >
              <td class="mono">
                <button type="button" class="pick" :aria-pressed="String(unit.seq) === ui.progress.selected" @click="pickUnit(unit.seq)">
                  {{ unit.seq }}
                </button>
              </td>
              <td><span class="verdict" :class="verdictTone(unit.verdict)">{{ unit.verdict }}</span></td>
              <td class="result-title">{{ unit.title }}</td>
              <td class="muted">{{ unit.link_group || '—' }}</td>
              <td class="num mono rate" :class="unitRateTone(unit) ?? ''">{{ unitRateLabel(unit) || '—' }}</td>
              <td class="num mono">{{ humanDuration(unit.secs) }}</td>
            </tr>
          </tbody>
        </table>
      </div>

      <details class="log-section">
        <summary><strong>运行日志</strong><span class="muted">排查问题时展开主控输出</span></summary>
        <div class="log-bar">
          <span class="muted small">
            最多保留最近 {{ LOG_MAX_LINES }} 行；更早的已经不在这一页上。
          </span>
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
    </template>
  </section>
</template>

<style scoped>
.run-summary { margin: 0 0 16px; padding: 20px; border: 1px solid var(--line); border-radius: 8px; background: var(--panel-2); }
.run-heading { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
.run-state { display: inline-flex; align-items: center; gap: 8px; font-size: 14px; font-weight: 700; }
.run-state::before { content: ''; width: 8px; height: 8px; border-radius: 50%; background: var(--muted); }
.run-state.running::before { background: var(--accent); }
.run-state.aborted { color: var(--bad); }
.run-state.aborted::before { background: var(--bad); }
.run-id { margin-top: 5px; font-size: 11.5px; color: var(--muted); overflow-wrap: anywhere; }
.progress-labels { display: flex; flex-wrap: wrap; align-items: baseline; justify-content: space-between; gap: 8px; margin: 22px 0 9px; font-size: 12px; }
.progress-count { font-size: 25px; font-weight: 650; font-variant-numeric: tabular-nums; }
.timing { display: flex; gap: 12px; flex-wrap: wrap; }
.bar-wrap { height: 8px; border-radius: 4px; background: var(--line); overflow: hidden; }
/* 纯宽度变化，无 transition：这台机器此刻正在灌线速。 */
.bar { height: 100%; background: var(--accent); }
.current-unit { margin-top: 18px; padding-top: 14px; border-top: 1px solid var(--line); overflow-wrap: anywhere; }
.current-label { display: block; margin-bottom: 5px; color: var(--muted); font-size: 12px; }
.current-label .mono { margin-left: 6px; color: var(--accent); }
.current-unit > strong { font-size: 14px; font-weight: 600; }
.cards { display: grid; grid-template-columns: repeat(6, minmax(0, 1fr)); margin: 16px 0 24px; border: 1px solid var(--line); border-radius: 7px; background: var(--surface); overflow: hidden; }
.card { padding: 14px 12px; border-left: 1px solid var(--line); }
.card:first-child { border-left: 0; }
.card span { display: block; font-size: 10.5px; color: var(--muted); overflow-wrap: anywhere; }
.card strong { display: block; margin: 5px 0 2px; font-size: 24px; font-weight: 650; font-variant-numeric: tabular-nums; }
.card small { font-size: 11px; color: var(--muted); }
.card.pass strong { color: var(--ok); }
.card.fail strong { color: var(--bad); }
.verdict { display: inline-block; padding: 2px 6px; border-radius: 3px; font-weight: 600; font-size: 11px; font-family: var(--fm); white-space: nowrap; background: var(--panel-2); }
.verdict.pass { color: var(--ok); background: var(--ok-bg); }
.verdict.fail { color: var(--bad); background: var(--bad-bg); }
.verdict.inconclusive { color: var(--warn); background: var(--info-bg); }
.verdict.measured, .verdict.skip { color: var(--muted); }
.fail-group { margin: 0 0 10px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); overflow: hidden; }
.fail-head { display: flex; justify-content: space-between; flex-wrap: wrap; gap: 8px; padding: 10px 14px; background: var(--head); font-size: 12px; }
.fail-head span { color: var(--muted); }
.fail-group ul { margin: 0; padding: 0 14px; list-style: none; }
.fail-group li { display: flex; gap: 10px; align-items: baseline; padding: 10px 0; }
.fail-group li + li { border-top: 1px solid var(--line); }
.fail-title { flex: 1 1 auto; min-width: 0; overflow-wrap: anywhere; }
.reason { overflow-wrap: anywhere; }
/* 实测值的着色只在**有门限可比**时出现：Observe 模式没有门限，
   满屏绿色会被读成「全部达标」。不上色就是「这一格没有可比对象」。 */
.rate { white-space: nowrap; font-variant-numeric: tabular-nums; }
.rate.over { color: var(--ok); }
.rate.under { color: var(--bad); }
.seq { flex: 0 0 38px; color: var(--muted); font-size: 12px; }
.hint { margin: 0 0 12px; font-size: 12.5px; max-width: 80ch; }
.section-heading { display: flex; align-items: baseline; justify-content: space-between; gap: 12px; margin: 22px 0 10px; }
.section-heading h3 { margin: 0; }
.section-heading > span { font-size: 12px; }
.scroll { max-width: 100%; overflow-x: auto; border: 1px solid var(--line); border-radius: 7px; background: var(--surface); }
table { width: 100%; min-width: 620px; border-collapse: separate; border-spacing: 0; font-size: 13px; }
th, td { padding: 10px 12px; text-align: left; border-bottom: 1px solid var(--line); }
thead th { background: var(--head); font-size: 11.5px; color: var(--muted); white-space: nowrap; }
tbody tr:last-child td { border-bottom: 0; }
tbody tr:hover { background: var(--panel-2); }
.result-title { min-width: 200px; overflow-wrap: anywhere; }
.num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
.log-section { margin-top: 22px; border-top: 1px solid var(--line); }
.log-section > summary { padding: 15px 0; cursor: pointer; }
.log-section > summary > span { margin-left: 12px; font-size: 12px; }
.log { max-height: 320px; margin-top: 8px; overflow: auto; white-space: pre-wrap; overflow-wrap: anywhere; }
.log-bar { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
.log-bar .small { font-size: 12px; }
.bad { margin: 0 0 12px; padding: 10px 13px; border-left: 3px solid var(--bad); background: var(--bad-bg); overflow-wrap: anywhere; }
/* 断联/已受理这类「还没定论」的话用告警色，不用失败色：它们不是失败。 */
.warn { margin: 0 0 12px; padding: 10px 13px; border-left: 3px solid var(--warn); background: var(--info-bg); overflow-wrap: anywhere; }
.warn p { margin: 0 0 8px; }
.run-actions { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
/* 一行可点筛选，替掉六张只能看的大数字卡。窄屏换行，不做局部横滚——
   七个短标签换行后仍然一眼看得全。 */
.chips { display: flex; flex-wrap: wrap; gap: 8px; margin: 18px 0 6px; }
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
.chip.fail strong { color: var(--bad); }
.chip.pass strong { color: var(--ok); }
.chip.on strong { color: inherit; }
.unit-search { flex: 1 1 220px; max-width: 320px; }
.unit-search input { width: 100%; padding: 7px 10px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); color: var(--ink); font: inherit; font-size: 13px; }
button.small { min-height: 30px; padding: 5px 10px; font-size: 12px; }
.unit-detail { margin: 10px 0 14px; padding: 14px 16px; border: 1px solid var(--line); border-radius: 7px; background: var(--panel-2); }
.unit-detail-head { display: flex; align-items: center; gap: 10px; margin-bottom: 8px; }
.unit-detail-head .ghost { margin-left: auto; }
.unit-detail-title { margin: 0 0 10px; font-size: 13px; overflow-wrap: anywhere; }
.unit-detail dl { display: grid; grid-template-columns: auto minmax(0, 1fr); gap: 6px 14px; margin: 0 0 10px; font-size: 12.5px; }
.unit-detail dt { color: var(--muted); white-space: nowrap; }
.unit-detail dd { margin: 0; overflow-wrap: anywhere; }
.unit-detail .small { margin: 0; font-size: 12px; }
tbody tr.picked { background: var(--info-bg); }
.pick { padding: 0; min-height: 0; font: inherit; font-family: var(--fm); color: var(--accent); background: none; border: 0; text-decoration: underline; text-underline-offset: 3px; }
.pick:hover:not(:disabled) { background: none; color: var(--accent-hover); }
.pick[aria-pressed='true'] { color: var(--ink); text-decoration: none; font-weight: 700; }
.freshness { font-size: 12px; white-space: nowrap; }
.empty { padding: 30px 24px; border: 1px dashed var(--line); border-radius: 7px; color: var(--muted); background: var(--panel-2); }
.empty strong { display: block; font-size: 15px; color: var(--ink); }
.empty p { margin: 6px 0 16px; font-size: 13px; max-width: 60ch; }
.empty.compact { padding: 18px; font-size: 13px; }
.mono { font-family: var(--fm); }
@media (max-width: 850px) {
  .cards { grid-template-columns: repeat(3, minmax(0, 1fr)); }
  .card:nth-child(4) { border-left: 0; }
  .card:nth-child(n + 4) { border-top: 1px solid var(--line); }
}
@media (max-width: 600px) {
  .run-summary { padding: 16px; }
  .run-heading { align-items: flex-start; flex-wrap: wrap; }
  .progress-labels { align-items: flex-start; flex-direction: column; }
  .fail-group li { display: grid; grid-template-columns: 36px minmax(0, 1fr); gap: 5px 8px; }
  .fail-group .verdict { justify-self: start; }
  .fail-title, .reason { grid-column: 2; }
  .log-section > summary > span { display: block; margin: 4px 0 0 18px; }
}
</style>
