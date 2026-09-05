<script setup lang="ts">
import { computed, onMounted } from 'vue';
import { failuresByLinkGroup, humanDuration, verdictTone } from '../../domain/progress';
import { openReport, run, startPolling, view } from '../../state/run';
import { goto } from '../../state/ui';

/**
 * 「进度」：消费 `RunStatus`，**不解析任何日志行**。
 *
 * v5.0 原本计划让这一页去解析 `[i/total]` 和「==> 单元结果:」两种日志行，
 * 并用 Rust 测试把日志格式钉死当协议。一次 11.5 小时、210 单元的测试有三万行
 * 日志，刷新一次页面就要全量重放才能重建进度。v6.0 让 Rust 直接吐结构化状态
 * （ADR-2），于是这里只做展示，日志文案彻底自由。
 */

const failures = computed(() => failuresByLinkGroup(run.units));
const counts = computed(() => run.status.counts);

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

    <div v-if="!run.status.run_id && !run.running" class="empty">
      <strong>等待开始第一轮测试</strong>
      <p>在「执行」页预览并开始测试后，这里会显示实时进度与结果。</p>
      <button type="button" @click="goto('run')">前往执行</button>
    </div>

    <template v-else>
      <section class="run-summary" aria-label="本轮运行进度">
        <div class="run-heading">
          <div>
            <span class="run-state" :class="{ running: run.running, aborted: view.aborted }">
              {{ view.aborted ? '已中止' : run.running ? '正在执行' : view.finished ? '本轮结束' : '等待状态更新' }}
            </span>
            <div v-if="run.status.run_id" class="run-id mono">{{ run.status.run_id }}</div>
          </div>
          <button v-if="view.finished && run.report" type="button" @click="openReport">打开报告</button>
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

      <p v-if="run.startError" class="bad" role="alert">{{ run.startError }}</p>
      <p v-if="view.aborted" class="bad" role="alert">
        连续多个灌包单元没有产生任何测量，已在第 {{ run.status.aborted_at_unit }} 个单元中止剩余队列。
        先确认被测设备是否掉线或重启，再重跑剩余项；已完成的部分会照常出报告。
      </p>

      <div class="cards" aria-label="判定统计">
        <div class="card pass"><span>PASS</span><strong>{{ counts.pass }}</strong><small>达标</small></div>
        <div class="card fail"><span>RATE_FAIL</span><strong>{{ counts.fail }}</strong><small>速率未达标</small></div>
        <div class="card"><span>MEASURED</span><strong>{{ counts.measured }}</strong><small>仅测量</small></div>
        <div class="card"><span>NOT_EVALUATED</span><strong>{{ counts.not_evaluated }}</strong><small>未能判定</small></div>
        <div class="card"><span>SETUP_ERROR</span><strong>{{ counts.setup_error }}</strong><small>准备错误</small></div>
        <div class="card"><span>SKIP</span><strong>{{ counts.skip }}</strong><small>已跳过</small></div>
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
              <small v-if="unit.reason_code" class="reason muted mono">{{ unit.reason_code }}</small>
            </li>
          </ul>
        </div>
      </template>

      <div class="section-heading"><h3>已完成</h3><span class="muted">{{ run.units.length }} 个单元</span></div>
      <div v-if="run.units.length === 0" class="empty compact">单元完成后，判定和耗时会显示在这里。</div>
      <div v-else class="scroll" tabindex="0" role="region" aria-label="已完成单元列表，可横向滚动">
        <table>
          <thead>
            <tr>
              <th scope="col">#</th><th scope="col">判定</th><th scope="col">标题</th><th scope="col">链路集合</th><th scope="col" class="num">耗时</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="unit in run.units" :key="unit.seq">
              <td class="mono">{{ unit.seq }}</td>
              <td><span class="verdict" :class="verdictTone(unit.verdict)">{{ unit.verdict }}</span></td>
              <td class="result-title">{{ unit.title }}</td>
              <td class="muted">{{ unit.link_group || '—' }}</td>
              <td class="num mono">{{ humanDuration(unit.secs) }}</td>
            </tr>
          </tbody>
        </table>
      </div>

      <details class="log-section">
        <summary><strong>运行日志</strong><span class="muted">排查问题时展开主控输出</span></summary>
        <div class="screen log" data-label="主控输出" tabindex="0" role="region" aria-label="运行日志，可滚动">
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
.verdict.inconclusive { color: var(--focus); background: var(--info-bg); }
.verdict.measured, .verdict.skip { color: var(--muted); }
.fail-group { margin: 0 0 10px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); overflow: hidden; }
.fail-head { display: flex; justify-content: space-between; flex-wrap: wrap; gap: 8px; padding: 10px 14px; background: var(--head); font-size: 12px; }
.fail-head span { color: var(--muted); }
.fail-group ul { margin: 0; padding: 0 14px; list-style: none; }
.fail-group li { display: flex; gap: 10px; align-items: baseline; padding: 10px 0; }
.fail-group li + li { border-top: 1px solid var(--line); }
.fail-title { flex: 1 1 auto; min-width: 0; overflow-wrap: anywhere; }
.reason { overflow-wrap: anywhere; }
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
.bad { margin: 0 0 12px; padding: 10px 13px; border-left: 3px solid var(--bad); background: var(--bad-bg); overflow-wrap: anywhere; }
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
