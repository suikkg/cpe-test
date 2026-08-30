<script setup lang="ts">
import { computed, onMounted } from 'vue';
import { failuresByLinkGroup, humanDuration, verdictTone } from '../../domain/progress';
import { openReport, run, startPolling, view } from '../../state/run';

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
      <p class="muted">
        单元级状态直接来自服务端，不靠解析日志——刷新页面不会丢进度。
      </p>
    </header>

    <div v-if="!run.status.run_id && !run.running" class="empty">
      还没有正在跑或跑完的测试。去「执行」页预览并开跑。
    </div>

    <template v-else>
      <div class="bar-wrap">
        <div class="bar" :style="{ width: `${Math.round(view.ratio * 100)}%` }"></div>
        <span class="bar-text mono">
          {{ view.done }} / {{ view.total }}
          <template v-if="view.eta"> · 剩余 {{ view.eta }}</template>
          <template v-if="view.finishHint"> · {{ view.finishHint }}</template>
        </span>
      </div>

      <p v-if="view.aborted" class="bad" role="alert">
        连续多个灌包单元没有产生任何测量，已在第 {{ run.status.aborted_at_unit }} 个单元中止剩余队列。
        先确认被测设备是否掉线或重启，再重跑剩余项；已完成的部分会照常出报告。
      </p>

      <div v-if="view.currentSeq" class="screen" data-label="RUNNING · 正在执行">
        #{{ view.currentSeq }} {{ view.currentTitle }}
      </div>
      <p v-else-if="view.finished" class="ok">
        本轮结束。
        <button v-if="run.report" type="button" class="link" @click="openReport">打开报告</button>
      </p>

      <div class="cards">
        <div class="card pass"><span>PASS</span><strong>{{ counts.pass }}</strong></div>
        <div class="card fail"><span>RATE_FAIL</span><strong>{{ counts.fail }}</strong></div>
        <div class="card"><span>MEASURED</span><strong>{{ counts.measured }}</strong></div>
        <div class="card"><span>NOT_EVALUATED</span><strong>{{ counts.not_evaluated }}</strong></div>
        <div class="card"><span>SETUP_ERROR</span><strong>{{ counts.setup_error }}</strong></div>
        <div class="card"><span>SKIP</span><strong>{{ counts.skip }}</strong></div>
      </div>

      <template v-if="failures.length">
        <h3>需要处置</h3>
        <p class="muted hint">
          RATE_FAIL 是「设备没达标」；NOT_EVALUATED 与 SETUP_ERROR 是「这一轮下不了结论」——
          先解决后者，它们说明的是环境或执行问题。
        </p>
        <div v-for="group in failures" :key="group.group" class="fail-group">
          <div class="fail-head">{{ group.group }}</div>
          <ul>
            <li v-for="unit in group.units" :key="unit.seq">
              <span class="seq mono">#{{ unit.seq }}</span>
              <span class="verdict" :class="verdictTone(unit.verdict)">{{ unit.verdict }}</span>
              <span class="fail-title">{{ unit.title }}</span>
              <small v-if="unit.reason_code" class="muted mono">{{ unit.reason_code }}</small>
            </li>
          </ul>
        </div>
      </template>

      <h3>已完成</h3>
      <div v-if="run.units.length === 0" class="empty">还没有跑完的单元。</div>
      <div v-else class="scroll">
        <table>
          <thead>
            <tr>
              <th>#</th><th>判定</th><th>标题</th><th>链路组</th><th class="num">耗时</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="unit in run.units" :key="unit.seq">
              <td class="mono">{{ unit.seq }}</td>
              <td><span class="verdict" :class="verdictTone(unit.verdict)">{{ unit.verdict }}</span></td>
              <td>{{ unit.title }}</td>
              <td class="muted">{{ unit.link_group || '—' }}</td>
              <td class="num mono">{{ humanDuration(unit.secs) }}</td>
            </tr>
          </tbody>
        </table>
      </div>

      <h3>日志</h3>
      <div class="screen log" data-label="LOG · 主控输出">
        <div v-for="(line, i) in run.lines" :key="i">{{ line }}</div>
      </div>
    </template>
  </section>
</template>

<style scoped>
.bar-wrap {
  position: relative;
  height: 26px;
  margin: 0 0 14px;
  border: 1px solid var(--line);
  border-radius: 4px;
  background: var(--panel-2);
  overflow: hidden;
}
/* 纯宽度变化，无 transition：这台机器此刻正在灌线速。 */
.bar { height: 100%; background: var(--accent); }
.bar-text {
  position: absolute; inset: 0;
  display: flex; align-items: center; justify-content: center;
  font-size: 12px; color: var(--ink);
}
.cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(120px, 1fr)); gap: 8px; margin: 14px 0; }
.card { padding: 8px 10px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.card span { display: block; font-size: 11px; color: var(--muted); }
.card strong { display: block; margin-top: 2px; font-size: 18px; }
.card.pass strong { color: var(--ok); }
.card.fail strong { color: var(--bad); }
.verdict { font-weight: 700; font-size: 12px; font-family: var(--fm); }
.verdict.pass { color: var(--ok); }
.verdict.fail { color: var(--bad); }
.verdict.inconclusive { color: var(--focus); }
.verdict.measured, .verdict.skip { color: var(--muted); }
.fail-group { margin: 0 0 10px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.fail-head { padding: 7px 11px; background: var(--head); font-size: 12px; font-weight: 600; }
.fail-group ul { margin: 0; padding: 6px 11px 9px; list-style: none; }
.fail-group li { display: flex; gap: 9px; align-items: baseline; padding: 3px 0; }
.fail-title { flex: 1 1 auto; min-width: 0; overflow-wrap: anywhere; }
.seq { flex: 0 0 42px; color: var(--muted); font-size: 12px; }
.hint { margin: 0 0 8px; font-size: 12.5px; }
.scroll { max-width: 100%; overflow-x: auto; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
table { width: 100%; border-collapse: separate; border-spacing: 0; font-size: 13px; }
th, td { padding: 7px 11px; text-align: left; border-bottom: 1px solid var(--line); }
thead th { background: var(--head); font-size: 11.5px; color: var(--muted); white-space: nowrap; }
tbody tr:last-child td { border-bottom: 0; }
.num { text-align: right; font-variant-numeric: tabular-nums; }
.log { max-height: 320px; overflow-y: auto; white-space: pre-wrap; }
.ok { margin: 0 0 12px; padding: 9px 12px; border-left: 3px solid var(--ok); background: var(--ok-bg); }
.bad { margin: 0 0 12px; padding: 9px 12px; border-left: 3px solid var(--bad); background: var(--bad-bg); }
.link { border: 0; background: none; color: var(--accent); font: inherit; text-decoration: underline; cursor: pointer; padding: 0; }
.empty { padding: 14px 16px; border: 1px dashed var(--line); border-radius: 6px; color: var(--muted); background: var(--panel-2); }
.mono { font-family: var(--fm); }
</style>
