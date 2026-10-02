<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import type { PlannedUnit } from '../../api/dto';
import { humanDuration } from '../../domain/progress';
import { selectedPortPairs } from '../../domain/plan-ports';
import { directionLabel } from '../../domain/plan-build';
import { matchesTerms, queryTerms, visibleCountLabel } from '../../domain/search';
import { inner, scenarioBlocksActions } from '../../state/inner';
import { clampRounds, MAX_ROUNDS, plan, preview, previewIsCurrent } from '../../state/plan';
import { run, start } from '../../state/run';
import { goto, ui } from '../../state/ui';

/**
 * 「执行」页的准备面板：本轮选项 + 预览 + 开始。
 *
 * 预览**直接渲染 `PlanOut.sections` + `trace`**。旧页把后端算好的这两份
 * 层级/溯源数据丢弃，只读平铺的 `units` 再自己重拼分组——于是「界面上
 * 这一组是怎么来的」在前端又有了一份规则。新前端不许再犯。
 */

const out = computed(() => plan.preview);
const units = computed<PlannedUnit[]>(() => out.value?.units ?? []);
const selectedPairs = computed(() => selectedPortPairs(plan.ui));
const otherRunning = computed(() => inner.status.running || scenarioBlocksActions());
const blockingErrors = computed(() => out.value?.blocking_errors ?? []);
const reviewOrder = ref<'execution' | 'source'>('execution');
const visibleLimit = ref(50);
watch(() => [plan.preview, ui.run.query, reviewOrder.value], () => { visibleLimit.value = 50; });

async function startAndView(): Promise<void> {
  await start();
  // 受理之后由运行状态切到进度面板；清掉「准备下一轮」，这一轮结束后先看结果。
  if (run.startPhase === 'accepted') ui.preparing = false;
}

// socket 缓冲诊断属于底层排查信息，不在预览区展开；其余提示照常显示。
const visiblePreviewNotices = computed(
  () => out.value?.notices.filter((notice) => !notice.includes('socket 缓冲')) ?? [],
);

/**
 * 预览单元的页内搜索：序号、标题、最终参数与门限文字。
 *
 * 搜的是**服务端算完的那份文字**，不是界面上的配置值。顺序沿用 `seq`，过滤不重排。
 */
const unitMatches = computed(() => {
  const terms = queryTerms(ui.run.query);
  if (terms.length === 0) return null;
  return new Set(
    units.value
      .filter((unit) =>
        matchesTerms(terms, [unit.seq, `#${unit.seq}`, unit.title, ...(unit.load ?? []), ...(unit.targets ?? [])]),
      )
      .map((unit) => unit.seq),
  );
});
const shownUnitCount = computed(() => unitMatches.value?.size ?? units.value.length);
const unitCountLabel = computed(() => visibleCountLabel(shownUnitCount.value, units.value.length));

const grouped = computed(() => {
  const value = out.value;
  if (!value) return [];
  const bySeq = new Map(value.units.map((u) => [u.seq, u]));
  const sections = value.sections ?? [];
  const keep = (list: PlannedUnit[]) =>
    unitMatches.value ? list.filter((unit) => unitMatches.value?.has(unit.seq)) : list;
  if (reviewOrder.value === 'execution' || sections.length === 0) {
    return [{ title: '执行顺序', units: keep(value.units).slice(0, visibleLimit.value) }];
  }
  let remaining = visibleLimit.value;
  return sections
    .map((section) => {
      const visible = keep(
        section.unit_seqs.map((seq) => bySeq.get(seq)).filter((u): u is PlannedUnit => !!u),
      ).slice(0, remaining);
      remaining -= visible.length;
      return { title: section.title, units: visible };
    })
    // 一个都没剩的分组直接不显示：留一个空标题只会让人以为那一组被排除了。
    .filter((group) => group.units.length > 0);
});

/**
 * 每个分组的耗时与占比：「砍哪一段最划算」。
 *
 * 分母用**全部单元**的 `est_secs` 之和，不跟着筛选变。`est_secs` 的唯一实现
 * 在 Rust 的 builder，这里只做加法，不复算时长。
 */
const groupCost = computed(() => {
  const all = out.value?.units ?? [];
  const total = all.reduce((sum, unit) => sum + (unit.est_secs || 0), 0);
  const bySeq = new Map(all.map((unit) => [unit.seq, unit]));
  const sections = out.value?.sections ?? [];
  const costs = new Map<string, { secs: number; pct: number }>();
  const record = (title: string, seqs: number[]) => {
    const secs = seqs.reduce((sum, seq) => sum + (bySeq.get(seq)?.est_secs || 0), 0);
    costs.set(title, { secs, pct: total > 0 ? (secs / total) * 100 : 0 });
  };
  if (reviewOrder.value === 'execution' || sections.length === 0) {
    record('执行顺序', all.map((unit) => unit.seq));
  } else {
    for (const section of sections) record(section.title, section.unit_seqs);
  }
  return costs;
});
function costLabel(title: string): string {
  const cost = groupCost.value.get(title);
  if (!cost || cost.secs <= 0) return '';
  return `${humanDuration(cost.secs)} · ${cost.pct.toFixed(0)}%`;
}
function costPct(title: string): number {
  return groupCost.value.get(title)?.pct ?? 0;
}

/** 溯源：单元序号 → 它来自哪个套件任务。 */
const traceBySeq = computed(() => new Map((out.value?.trace ?? []).map((t) => [t.seq, t])));

const estimate = computed(() => {
  const value = out.value;
  if (!value) return '';
  // 开着 resume 时按区间显示：跳过的都真跳过 vs 一个都不跳。
  if (plan.resume && value.est_full_secs !== value.est_total_secs) {
    return `${humanDuration(value.est_total_secs)} – ${humanDuration(value.est_full_secs)}`;
  }
  return humanDuration(value.est_full_secs);
});
const resumedCount = computed(() => units.value.filter((u) => u.resumed).length);
const stale = computed(() => !!out.value && !previewIsCurrent());
const forceNote = computed(() => {
  const parts: string[] = [];
  if (plan.forceTcpWindow.trim()) parts.push(`TCP -w ${plan.forceTcpWindow.trim()}`);
  if (plan.forceUdpBandwidth.trim()) parts.push(`UDP -b ${plan.forceUdpBandwidth.trim()}`);
  return parts.join(' · ');
});
const diagnosticsOn = computed(() => !!forceNote.value || plan.probeDuringTraffic || plan.probePathMtu || plan.limitUdpByLinkSpeed);

const canStart = computed(
  () =>
    !!out.value?.plan_hash &&
    units.value.length > 0 &&
    blockingErrors.value.length === 0 &&
    !plan.previewing &&
    !otherRunning.value &&
    !stale.value &&
    !run.running &&
    !run.starting &&
    // 状态还没读到时不放行：这台机器上可能已经有一轮在跑。
    run.synced &&
    run.startPhase !== 'unknown',
);

/** 摘要行下面那句「为什么现在还不能开始」。空串 = 可以开始。 */
const blockedReason = computed(() => {
  if (otherRunning.value) return '内环或组合测试正在运行，结束后才能开始子网测试。';
  if (!run.synced) return '尚未读到主控运行状态。';
  if (run.starting) return '正在提交…';
  if (plan.previewing) return '正在生成预览…';
  if (!selectedPairs.value) return '还没有选择网口。';
  if (!out.value?.plan_hash) return '先预览。';
  if (stale.value) return '参数改过了，重新预览后才能开始。';
  if (blockingErrors.value.length) return '计划中有不可执行的项目，见下方。';
  if (!units.value.length) return '没有可执行单元。';
  return '';
});

const summary = computed(() => {
  if (!out.value) return selectedPairs.value ? `已选 ${selectedPairs.value} 对网口` : '还没有选择网口';
  const parts = [`${selectedPairs.value} 对网口`, `${units.value.length} 个单元`];
  if (estimate.value) parts.push(`约 ${estimate.value}`);
  if (plan.resume && resumedCount.value) parts.push(`RESUME 跳过 ${resumedCount.value}`);
  return parts.join(' · ');
});

/** 进来时预览已过期就自动预览一次；只在空闲、选了网口、没有在预览时。 */
onMounted(() => {
  if (run.running || plan.previewing || !selectedPairs.value) return;
  if (!out.value || stale.value) void preview();
});
</script>

<template>
  <div class="prepare">
    <div class="summary-bar" :class="{ stale }">
      <div class="summary-text" role="status">
        <strong>{{ summary }}</strong>
        <span v-if="blockedReason" class="muted">{{ blockedReason }}</span>
      </div>
      <div class="buttons">
        <button type="button" class="ghost" :disabled="plan.previewing || !selectedPairs" @click="preview">
          {{ plan.previewing ? '预览中…' : out ? '重新预览' : '预览' }}
        </button>
        <button type="button" :disabled="!canStart" @click="startAndView">
          {{ run.starting ? '启动中…' : '开始测试' }}
        </button>
      </div>
    </div>

    <div class="options">
      <label class="field">
        <span>默认时长</span>
        <span class="unit-input"><input v-model.number="plan.duration" type="number" min="1" max="86400" /> 秒</span>
      </label>
      <label class="field">
        <span>轮次</span>
        <span class="unit-input">
          <input
            :value="plan.rounds"
            type="number"
            min="1"
            :max="MAX_ROUNDS"
            title="整份计划跑完后再重复"
            @input="plan.rounds = clampRounds(Number(($event.target as HTMLInputElement).value))"
          /> 遍
        </span>
      </label>
      <label class="check option">
        <input v-model="plan.resume" type="checkbox" />
        <span>跳过 24 小时内已通过的单元（RESUME）</span>
      </label>
      <label class="check option">
        <input v-model="plan.screenshot" type="checkbox" />
        <span>每个吞吐单元后截图</span>
      </label>
    </div>

    <details class="diag" :open="diagnosticsOn">
      <summary>临时覆盖与诊断<span v-if="diagnosticsOn" class="on-mark">已启用</span></summary>
      <div class="diag-body">
        <label class="field">
          <span>本轮强制 TCP <code>-w</code></span>
          <input v-model="plan.forceTcpWindow" type="text" placeholder="留空 = 不覆盖" />
        </label>
        <label class="field">
          <span>本轮强制 UDP <code>-b</code></span>
          <input v-model="plan.forceUdpBandwidth" type="text" placeholder="留空 = 不覆盖" />
        </label>
        <label class="check" title="打流期间每秒测一次 32 字节 Ping，结果只进诊断">
          <input v-model="plan.probeDuringTraffic" type="checkbox" />测负载下时延
        </label>
        <label class="check" title="IPv4 Ping 单元额外探测不分片的最大包长，结果只进诊断">
          <input v-model="plan.probePathMtu" type="checkbox" />探路径 MTU
        </label>
        <label class="check">
          <input v-model="plan.limitUdpByLinkSpeed" type="checkbox" />按链路上限裁剪 UDP 发送速率
        </label>
      </div>
      <p v-if="forceNote" class="hint">本轮强制 {{ forceNote }}：覆盖套件参数，只对这一轮生效，预览里显示的就是覆盖后的参数。</p>
    </details>

    <p v-if="plan.previewError" class="msg bad" role="alert">{{ plan.previewError }}</p>
    <div v-if="blockingErrors.length" class="msg bad" role="alert">
      <p><strong>开始前需要处理</strong></p>
      <ul><li v-for="error in blockingErrors" :key="error">{{ error }}</li></ul>
      <button type="button" class="ghost small" @click="goto('plan')">调整计划</button>
    </div>
    <p v-for="(notice, i) in visiblePreviewNotices" :key="i" class="msg warn">{{ notice }}</p>

    <div v-if="!selectedPairs" class="empty-state">
      <p>先在计划里选择要测的网口。</p>
      <button type="button" @click="goto('plan')">去计划</button>
    </div>
    <template v-else-if="out">
      <div v-if="units.length" class="toolbar">
        <label v-if="units.length > 4" class="grow">
          <span class="sr-only">搜索测试单元</span>
          <input
            type="search"
            :value="ui.run.query"
            placeholder="搜序号、标题、参数或门限"
            @input="ui.run.query = ($event.target as HTMLInputElement).value"
          />
        </label>
        <span v-if="ui.run.query" class="count">{{ unitCountLabel }}</span>
        <div class="seg push" role="group" aria-label="预览排列方式">
          <button type="button" :aria-pressed="reviewOrder === 'execution'" @click="reviewOrder = 'execution'">按执行顺序</button>
          <button type="button" :aria-pressed="reviewOrder === 'source'" @click="reviewOrder = 'source'">按测试内容</button>
        </div>
      </div>
      <p v-if="unitMatches && shownUnitCount === 0" class="empty-state" role="status">
        没有单元匹配「{{ ui.run.query.trim() }}」；开始测试仍会跑全部 {{ units.length }} 个。
      </p>
      <details v-for="(group, gi) in grouped" :key="gi" class="section" :class="{ stale }" open>
        <summary>
          <strong>{{ group.title }}</strong>
          <small class="muted">{{ group.units.length }} 个单元</small>
          <small v-if="costLabel(group.title)" class="muted cost">{{ costLabel(group.title) }}</small>
          <!-- 占比条是静态的：机器正在灌线速，界面不放持续动画。 -->
          <span v-if="costPct(group.title) > 0" class="cost-bar" aria-hidden="true">
            <i :style="{ width: `${Math.min(100, costPct(group.title))}%` }"></i>
          </span>
        </summary>
        <ol class="units">
          <li v-for="unit in group.units" :key="unit.seq" :class="{ resumed: unit.resumed }">
            <span class="seq mono">#{{ unit.seq }}</span>
            <div class="unit-body">
              <div class="unit-title">
                {{ unit.title }}
                <small v-if="unit.resumed" class="tag">将跳过</small>
              </div>
              <div class="load mono">{{ unit.load.join(' · ') || '—' }}</div>
              <!-- 直接展示服务端计算的最终门限，避免被覆盖的配置值造成误导。 -->
              <div v-if="unit.targets?.length" class="targets">{{ unit.targets.join(' · ') }}</div>
              <div v-if="traceBySeq.get(unit.seq)" class="trace muted">
                {{ traceBySeq.get(unit.seq)!.protocol ?? '' }}
                <template v-if="traceBySeq.get(unit.seq)!.direction">
                  · {{ directionLabel(traceBySeq.get(unit.seq)!.direction!) }}
                </template>
                <template v-if="traceBySeq.get(unit.seq)!.ip"> · {{ traceBySeq.get(unit.seq)!.ip }}</template>
              </div>
            </div>
            <span class="est mono">{{ humanDuration(unit.est_secs) }}</span>
          </li>
        </ol>
      </details>
      <div v-if="shownUnitCount > visibleLimit" class="toolbar">
        <span class="count">已显示前 {{ visibleLimit }} / {{ shownUnitCount }} 个，全部单元都会执行。</span>
        <button type="button" class="ghost small" @click="visibleLimit += 50">再显示 50 个</button>
      </div>
    </template>
  </div>
</template>

<style scoped>
.summary-bar {
  display: flex; align-items: center; justify-content: space-between; gap: 12px 16px; flex-wrap: wrap;
  padding: 14px 16px; border: 1px solid var(--line); border-left: 4px solid var(--accent); border-radius: 6px;
  background: var(--panel-2);
}
.summary-bar.stale { border-left-color: var(--warn); }
.summary-text { display: flex; flex-direction: column; gap: 3px; }
.summary-text strong { font-size: 15px; font-variant-numeric: tabular-nums; }
.summary-text span { font-size: 12.5px; }
.buttons { display: flex; gap: 8px; flex-wrap: wrap; }
.options { display: flex; align-items: end; flex-wrap: wrap; gap: 12px 24px; margin: 16px 0 8px; }
.option { min-height: 36px; font-size: 13px; }
.diag { margin: 8px 0 12px; border-top: 1px solid var(--line); }
.diag > summary { padding: 10px 0; font-size: 13px; font-weight: 600; }
.on-mark { margin-left: 8px; color: var(--warn); font-size: 12px; font-weight: 500; }
.diag-body { display: flex; align-items: end; flex-wrap: wrap; gap: 12px 24px; }
.diag-body .check { min-height: 36px; font-size: 13px; }
.diag .hint { margin: 8px 0 0; }
.section { margin: 0 0 10px; border: 1px solid var(--line); border-radius: 7px; background: var(--surface); }
.section.stale { opacity: .6; }
.section > summary { padding: 11px 16px; background: var(--head); border-radius: 6px; overflow-wrap: anywhere; }
.section > summary small { margin-left: 10px; white-space: nowrap; }
.cost { font-variant-numeric: tabular-nums; }
.cost-bar { display: inline-block; vertical-align: middle; width: 72px; height: 4px; margin-left: 10px; border-radius: 2px; background: var(--line); overflow: hidden; }
.cost-bar i { display: block; height: 100%; background: var(--accent); }
.units { margin: 0; padding: 0 16px 4px; list-style: none; }
.units li { display: flex; gap: 12px; align-items: baseline; padding: 11px 0; border-top: 1px solid var(--line); }
.units li.resumed .unit-title { color: var(--muted); }
.seq { flex: 0 0 42px; color: var(--muted); font-size: 12px; }
.unit-body { flex: 1 1 auto; min-width: 0; }
.unit-title { font-weight: 600; overflow-wrap: anywhere; }
.load { margin-top: 4px; font-size: 12px; color: var(--muted); overflow-wrap: anywhere; }
.targets { margin-top: 3px; font-size: 12px; color: var(--accent); overflow-wrap: anywhere; }
.trace { margin-top: 3px; font-size: 11.5px; }
.est { flex: 0 0 auto; font-size: 12px; color: var(--muted); }
.tag { margin-left: 6px; padding: 2px 6px; border-radius: 3px; background: var(--info-bg); font-size: 10.5px; font-weight: 500; }
@media (max-width: 760px) {
  .buttons { width: 100%; }
  .buttons button { flex: 1; }
}
@media (max-width: 480px) {
  .units li { display: grid; grid-template-columns: 36px minmax(0, 1fr); gap: 4px 8px; }
  .est { grid-column: 2; }
}
</style>
