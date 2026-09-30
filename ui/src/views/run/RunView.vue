<script setup lang="ts">
import { scenarioBlocksActions } from '../../state/inner';
import { computed, ref, watch } from 'vue';
import type { PlannedUnit } from '../../api/dto';
import { humanDuration } from '../../domain/progress';
import { selectedPortPairs } from '../../domain/plan-ports';
import { directionLabel } from '../../domain/plan-build';
import { matchesTerms, queryTerms, visibleCountLabel } from '../../domain/search';
import { clampRounds, MAX_ROUNDS, plan, preview, previewIsCurrent } from '../../state/plan';
import { prepareAfterUnknownStart, run, start, stop, syncStatus } from '../../state/run';
import { inner } from '../../state/inner';
import { goto, ui } from '../../state/ui';
import GlobalDefaults from './GlobalDefaults.vue';
import NicPolicyTable from './NicPolicyTable.vue';

/**
 * 「执行」：计划复核树 + 执行区。
 *
 * 复核树**直接渲染 `PlanOut.sections` + `trace`**。旧页把后端算好的这两份
 * 层级/溯源数据 100% 丢弃，只读平铺的 `units` 再自己重拼分组——于是「界面上
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
  if (run.startPhase === 'accepted') goto('progress');
}
// socket 缓冲诊断属于底层排查信息，暂不在 WebUI 预览区展开。
// 保留其它预览提示，避免把真正影响执行的错误一并隐藏。
const visiblePreviewNotices = computed(
  () => out.value?.notices.filter((notice) => !notice.includes('socket 缓冲')) ?? [],
);

/**
 * 预览单元的页内搜索（§11.1）：序号、标题、最终参数与门限文字。
 *
 * 搜的是**服务端算完的那份文字**，不是界面上的配置值——用户在这一页要回答的
 * 问题是「我要找的那条腿最后按什么跑」，而那句话只有服务端说了算。
 * 顺序沿用 `seq`，过滤不重排。
 */
const unitMatches = computed(() => {
  const terms = queryTerms(ui.run.query);
  if (terms.length === 0) return null;
  return new Set(
    units.value
      .filter((unit) =>
        matchesTerms(terms, [
          unit.seq,
          `#${unit.seq}`,
          unit.title,
          ...(unit.load ?? []),
          ...(unit.targets ?? []),
        ]),
      )
      .map((unit) => unit.seq),
  );
});
const shownUnitCount = computed(() => unitMatches.value?.size ?? units.value.length);
const unitCountLabel = computed(() => visibleCountLabel(shownUnitCount.value, units.value.length));

/** 按 sections 分组的单元；sections 空时退化成一个「全部」组。 */
/** 本轮强制档位的一句话说明；两个都留空时为空串。 */
const forceNote = computed(() => {
  const parts: string[] = [];
  if (plan.forceTcpWindow.trim()) parts.push(`TCP -w ${plan.forceTcpWindow.trim()}`);
  if (plan.forceUdpBandwidth.trim()) parts.push(`UDP -b ${plan.forceUdpBandwidth.trim()}`);
  return parts.join(' · ');
});

const grouped = computed(() => {
  const value = out.value;
  if (!value) return [];
  const bySeq = new Map(value.units.map((u) => [u.seq, u]));
  const sections = value.sections ?? [];
  const keep = (list: PlannedUnit[]) =>
    unitMatches.value ? list.filter((unit) => unitMatches.value?.has(unit.seq)) : list;
  if (reviewOrder.value === 'execution' || sections.length === 0) {
    return [{ title: '实际执行顺序', units: keep(value.units).slice(0, visibleLimit.value) }];
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
 * 每个分组的耗时与占比。
 *
 * 「210 个单元、11.5 小时」这个数字出现的时候，操作员的下一个问题一定是
 * **「砍哪一段最划算」**——而在此之前这一页答不上来，只能按单元数估。
 *
 * 分母用**全部单元**的 `est_secs` 之和，不是筛选后的：占比要回答的是
 * 「这一组占整轮多少」，跟着筛选变的话每搜一次数字都不一样。
 * `est_secs` 的唯一实现在 Rust 的 builder，这里只做加法，不复算时长。
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
    record('实际执行顺序', all.map((unit) => unit.seq));
  } else {
    for (const section of sections) record(section.title, section.unit_seqs);
  }
  return costs;
});

function costLabel(title: string): string {
  const cost = groupCost.value.get(title);
  if (!cost || cost.secs <= 0) return '';
  return `${humanDuration(cost.secs)} · 占 ${cost.pct.toFixed(0)}%`;
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
    // 状态还没读到时不放行：这台机器上可能已经有一轮在跑，再起一轮就是
    // 两轮同时灌包，而屏幕上看不出来。
    run.synced &&
    run.startPhase !== 'unknown',
);

/** 按钮旁边那句「为什么现在还不能开始」。空串 = 可以开始。 */
const blockedReason = computed(() => {
  if (run.running) return '已有一轮在运行。';
  if (otherRunning.value) return '内环或组合测试正在运行。请等它结束，再开始子网测试。';
  if (!run.synced) return '还没读到主控的运行状态，先同步一次再开始。';
  if (run.startPhase === 'unknown') return '上一次「开始」没有拿到应答，先确认它到底有没有起跑。';
  if (run.starting) return '正在提交，请等这一次的结果。';
  if (plan.previewing) return '正在核对网口和测试参数，预览完成后即可复核。';
  if (!selectedPairs.value) return '还没有选择要测试的网口。返回测试计划，至少勾选一对网口。';
  if (!out.value?.plan_hash) return '还没预览过这一轮计划。';
  if (stale.value) return '预览已过期，参数改过了。';
  if (blockingErrors.value.length) return '计划中有不可执行项目，请按下方提示调整网口、IP 版本或测试内容后重新预览。';
  if (!units.value.length) return '当前没有可执行单元，请检查网口选择和测试内容。';
  return '';
});
</script>

<template>
  <section class="view">
    <header class="view-head">
      <h2>子网测试 · 预览与执行</h2>
      <p class="muted">确认选中的网口、流量方向和预计耗时。点击开始后进入进度页，可随时请求停止。</p>
    </header>

    <div class="port-review">
      <div><strong>{{ selectedPairs ? `本轮已选择 ${selectedPairs} 对网口` : '先选择本轮要测的网口' }}</strong>
        <p>每个测试单元是一项独立测试。A→B 表示 A 发、B 收；双向并发表示两端同时收发。</p></div>
      <button type="button" class="ghost" @click="goto('plan')">{{ selectedPairs ? '更改网口与测试内容' : '去选择网口' }}</button>
    </div>

    <section class="controls" aria-labelledby="run-options-title">
      <div class="controls-heading">
        <h3 id="run-options-title">运行参数</h3>
        <label class="duration-field">
          <span>默认吞吐时长</span>
          <span class="number-with-unit">
            <input v-model.number="plan.duration" type="number" min="1" max="86400" />
            <span>秒</span>
          </span>
        </label>
        <label class="field">
          <span>稳定性轮次</span>
          <span class="number-with-unit">
            <input
              :value="plan.rounds"
              type="number"
              min="1"
              :max="MAX_ROUNDS"
              title="整份计划跑完后再重复。默认 1 遍；增加轮次会相应延长测试时间。"
              @input="plan.rounds = clampRounds(Number(($event.target as HTMLInputElement).value))"
            />
            <span>遍</span>
          </span>
        </label>
      </div>
      <p class="control-help">首次测试可保留默认值。任务中单独设置的时长优先；PING 使用次数。下方预览显示实际生效参数。</p>
      <p v-if="plan.rounds > 1" class="control-help" role="status">整份计划将重复 {{ plan.rounds }} 遍，测试时间随轮次增加。预览按实际顺序逐轮列出。</p>
      <div class="run-options basic-options">
        <label class="switch">
          <input v-model="plan.resume" type="checkbox" />
          <span><strong>跳过近期已通过项目</strong><small>RESUME：复用 24 小时内同一单元的 PASS</small></span>
        </label>
        <label class="switch">
          <input v-model="plan.screenshot" type="checkbox" />
          <span><strong>保存截图</strong><small>每个吞吐单元后截图</small></span>
        </label>
      </div>
      <details class="temporary-options" :open="!!forceNote || plan.probeDuringTraffic || plan.probePathMtu || plan.limitUdpByLinkSpeed">
      <summary>临时参数与诊断选项<span>仅在需要覆盖套件参数或排查问题时调整</span></summary>
      <div class="fields force-row">
        <label class="field">
          <span>仅本轮强制 TCP <code>-w</code></span>
          <input
            v-model="plan.forceTcpWindow"
            type="text"
            placeholder="留空 = 不覆盖"
            title="覆盖本轮所有 TCP 任务的窗口大小，保留套件原参数；历史重跑会沿用此值。"
          />
        </label>
        <label class="field">
          <span>仅本轮强制 UDP <code>-b</code></span>
          <input
            v-model="plan.forceUdpBandwidth"
            type="text"
            placeholder="留空 = 不覆盖"
            title="覆盖本轮 UDP 发送带宽，保留原报文长度和缓冲区设置。"
          />
        </label>
      </div>
      <p v-if="forceNote" class="hint" role="status">
        本轮强制：{{ forceNote }}。<strong>盖过套件里配好的参数</strong>，只对这一轮生效、不写回套件；
        预览里逐单元显示的就是覆盖后的参数。
      </p>
      <div class="run-options">
        <label class="switch">
          <input v-model="plan.probeDuringTraffic" type="checkbox" />
          <span title="打流期间每秒测量一次 32 字节 Ping 时延，结果仅供诊断。">
            <strong>测负载下时延</strong><small>打流时额外测量网络响应时间</small>
          </span>
        </label>
        <label class="switch">
          <input v-model="plan.probePathMtu" type="checkbox" />
          <span title="在 IPv4 Ping 测试中探测不分片的最大包长，结果仅供诊断。">
            <strong>探路径 MTU</strong><small>检查不分片时可传输的最大包长</small>
          </span>
        </label>
        <label class="switch">
          <input v-model="plan.limitUdpByLinkSpeed" type="checkbox" />
          <span title="勾上后 UDP 的 -b 会被整条路径的可信上限压下来；预览里显示最终下发参数。">
            <strong>按链路上限裁剪 UDP 发送速率</strong><small>在预览中复核裁剪后的参数</small>
          </span>
        </label>
      </div>
      </details>
    </section>

    <details class="advanced">
      <summary>
        <strong>全局默认档位与判定门限</strong>
        <span class="muted">按需调整，预览显示最终生效值</span>
      </summary>
      <div class="advanced-body"><GlobalDefaults /></div>
    </details>
    <NicPolicyTable />

    <div class="review-actions">
      <div class="review-status" role="status">
        <strong>{{ run.running ? '测试正在运行' : stale ? '预览需要更新' : out ? '请复核本轮计划' : '准备预览' }}</strong>
        <span class="muted">{{ run.running ? '可前往进度页查看当前单元与结果。' : stale ? '参数已修改，重新预览后才能开始。' : out ? '检查下方单元、负载和门限后开始测试。' : '预览后显示本轮单元数与预计耗时。' }}</span>
      </div>
      <div class="buttons">
        <button type="button" class="ghost" :disabled="plan.previewing" @click="preview">
          {{ plan.previewing ? '预览中…' : stale ? '重新预览' : '预览' }}
        </button>
        <button v-if="run.running" type="button" class="primary" @click="goto('progress')">查看进度</button>
        <button v-else type="button" class="primary" :disabled="!canStart" @click="startAndView">
          {{ run.starting ? '启动中…' : '开始测试' }}
        </button>
        <button
          v-if="run.running || run.stopPhase !== 'idle'"
          type="button"
          class="ghost stop"
          :disabled="run.stopPhase !== 'idle'"
          @click="stop"
        >
          {{ run.stopPhase === 'sending' ? '正在请求停止…' : run.stopPhase === 'accepted' ? '已请求停止' : run.stopPhase === 'unknown' ? '停止结果未确认' : '停止' }}
        </button>
      </div>
    </div>

    <p v-if="plan.previewError" class="bad" role="alert">{{ plan.previewError }}</p>
    <div v-if="blockingErrors.length" class="bad" role="alert">
      <strong>开始前需要处理</strong>
      <ul><li v-for="error in blockingErrors" :key="error">{{ error }}</li></ul>
      <button type="button" class="ghost" @click="goto('plan')">调整网口与测试内容</button>
      <button type="button" class="ghost" @click="goto('agent')">检查两端网卡</button>
    </div>
    <p v-if="run.startError" class="bad" role="alert">{{ run.startError }}</p>
    <div v-if="run.startPhase === 'unknown'" class="warn" role="alert">
      <p>
        「开始」这条请求<strong>没有拿到应答</strong>，无法确认这一轮有没有起跑。
        正在同步运行状态，请勿重复开始。
        请先在主控确认没有运行，再重新准备并预览。
      </p>
      <button type="button" class="ghost" @click="syncStatus">再同步一次运行状态</button>
      <button v-if="run.synced && !run.running && !run.refreshError" type="button" class="ghost"
        @click="prepareAfterUnknownStart">已在主控确认没有运行，重新准备</button>
    </div>
    <p v-else-if="blockedReason && !stale" class="hint start-blocked" role="status">
      现在还不能开始：{{ blockedReason }}
    </p>
    <p v-if="stale" class="warn" role="status">
      计划或运行参数已在上次预览后改变。请重新预览，确认新的单元数和耗时后再开始。
    </p>

    <div v-if="!out" class="empty">
      <strong>先确认这一轮要跑什么</strong>
      <p>点击「预览」，检查测试单元、最终下发参数和门限。预览不会启动测试。</p>
      <button type="button" class="ghost" @click="goto('plan')">返回测试计划</button>
    </div>
    <template v-else>
      <div class="cards" :class="{ stale }">
        <div class="card">
          <span class="card-label">测试单元</span>
          <strong class="card-value">{{ units.length }}<small> 个</small></strong>
        </div>
        <div class="card">
          <span class="card-label">预计耗时</span>
          <strong class="card-value">{{ estimate }}</strong>
        </div>
        <div v-if="plan.resume" class="card">
          <span class="card-label">RESUME 预计跳过</span>
          <strong class="card-value">{{ resumedCount }}<small> 个</small></strong>
        </div>
      </div>

      <p v-for="(notice, i) in visiblePreviewNotices" :key="i" class="warn">{{ notice }}</p>

      <div class="review-heading">
        <h3>计划复核</h3>
        <span v-if="stale" class="preview-state">上次预览</span>
        <span v-else class="muted">最终下发参数与生效门限</span>
        <label v-if="units.length > 4" class="unit-search">
          <span class="sr-only">搜索测试单元</span>
          <input
            type="search"
            :value="ui.run.query"
            placeholder="搜序号、标题、参数或门限"
            @input="ui.run.query = ($event.target as HTMLInputElement).value"
          />
        </label>
        <span v-if="ui.run.query" class="muted">{{ unitCountLabel }}</span>
        <button v-if="ui.run.query" type="button" class="ghost small" @click="ui.run.query = ''">
          清空搜索
        </button>
      </div>
      <div v-if="units.length" class="review-order" role="group" aria-label="复核清单排列方式">
        <button type="button" class="ghost small" :aria-pressed="reviewOrder === 'execution'" @click="reviewOrder = 'execution'">按执行顺序</button>
        <button type="button" class="ghost small" :aria-pressed="reviewOrder === 'source'" @click="reviewOrder = 'source'">按测试内容分组</button>
        <span>{{ reviewOrder === 'execution' ? '按序号依次执行；双向并发在同一单元内同时进行。' : '分组仅方便查阅，实际仍按序号执行。' }}</span>
      </div>
      <p v-if="unitMatches && shownUnitCount === 0" class="empty compact-empty" role="status">
        没有单元匹配「{{ ui.run.query.trim() }}」。搜索只影响这份清单的显示，
        <strong>开始测试仍然跑全部 {{ units.length }} 个单元</strong>。
      </p>
      <div v-if="units.length === 0" class="empty">
        <strong>当前计划没有可执行单元</strong>
        <p>检查测试计划中的链路集合和任务，并根据上方预览提示调整。</p>
        <button type="button" class="ghost" @click="goto('plan')">检查测试计划</button>
      </div>
      <details v-for="(group, gi) in grouped" :key="gi" class="section" open>
        <summary>
          <strong>{{ group.title }}</strong>
          <small class="muted">{{ group.units.length }} 个单元</small>
          <!-- 「砍哪一段最划算」——占比条让这一组的代价当场可见。
               分母是全部单元，不跟着筛选变。 -->
          <small v-if="costLabel(group.title)" class="muted cost">{{ costLabel(group.title) }}</small>
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
              <div v-if="unit.targets?.length" class="targets">
                {{ unit.targets.join(' · ') }}
              </div>
              <div v-if="traceBySeq.get(unit.seq)" class="trace muted">
                {{ traceBySeq.get(unit.seq)!.protocol ?? '' }}
                <template v-if="traceBySeq.get(unit.seq)!.direction">
                  · {{ directionLabel(traceBySeq.get(unit.seq)!.direction!) }}
                </template>
                <template v-if="traceBySeq.get(unit.seq)!.ip">
                  · {{ traceBySeq.get(unit.seq)!.ip }}
                </template>
              </div>
            </div>
            <span class="est mono">{{ humanDuration(unit.est_secs) }}</span>
          </li>
        </ol>
      </details>
      <div v-if="shownUnitCount > visibleLimit" class="more-units">
        <span>已显示前 {{ visibleLimit }} / {{ shownUnitCount }} 个匹配单元，完整计划仍会全部执行。</span>
        <button type="button" class="ghost" @click="visibleLimit += 50">再显示 50 个</button>
      </div>
    </template>
  </section>
</template>

<style scoped>
.port-review { display: flex; justify-content: space-between; align-items: center; gap: 16px; flex-wrap: wrap; padding: 16px 0; margin-bottom: 16px; border-bottom: 1px solid var(--line); }
.port-review p, .control-help { color: var(--muted); font-size: 13px; line-height: 1.6; margin: 7px 0 0; }
.control-help { padding: 0 18px 14px; }
.temporary-options { border-top: 1px solid var(--line); }
.temporary-options > summary { cursor: pointer; padding: 14px 18px; font-size: 13px; font-weight: 600; }
.temporary-options > summary span { margin-left: 12px; color: var(--muted); font-weight: 400; }
.force-row { padding: 0 18px; }
.review-order, .more-units { display: flex; align-items: center; flex-wrap: wrap; gap: 10px; margin: 12px 0; font-size: 13px; color: var(--muted); }
.review-order button[aria-pressed='true'] { border-color: var(--accent); color: var(--accent); background: var(--info-bg); }
.controls { margin-bottom: 14px; border: 1px solid var(--line); border-radius: 8px; background: var(--surface); }
.controls-heading { display: flex; flex-wrap: wrap; justify-content: space-between; align-items: center; gap: 14px; padding: 14px 18px; border-bottom: 1px solid var(--line); }
.controls-heading h3 { margin: 0; }
.duration-field, .number-with-unit { display: flex; align-items: center; gap: 10px; }
.duration-field { font-size: 13px; }
.number-with-unit { color: var(--muted); }
input[type='number'] { width: 94px; padding: 7px 9px; border: 1px solid var(--line); border-radius: 5px; background: var(--panel-2); color: var(--ink); font: inherit; }
.run-options { display: grid; grid-template-columns: 1fr 1fr 1.3fr; }
.basic-options { grid-template-columns: 1fr 1fr; }
.switch { display: flex; align-items: flex-start; gap: 9px; padding: 16px 18px; cursor: pointer; }
.switch + .switch { border-left: 1px solid var(--line); }
.switch input { margin: 4px 0 0; accent-color: var(--accent); }
.switch strong, .switch small { display: block; }
.switch strong { font-size: 13px; font-weight: 600; }
.switch small { margin-top: 3px; font-size: 12px; color: var(--muted); }
.advanced { margin-bottom: 10px; border: 1px solid var(--line); border-radius: 7px; background: var(--surface); }
.advanced > summary { padding: 13px 16px; cursor: pointer; font-size: 13px; }
.advanced > summary span { margin-left: 10px; font-size: 12px; }
.advanced-body { padding: 0 12px 2px; }
.review-actions { display: flex; align-items: center; justify-content: space-between; gap: 16px; padding: 16px 18px; margin: 18px 0 14px; border: 1px solid var(--line); border-left: 4px solid var(--accent); border-radius: 6px; background: var(--panel-2); }
.review-status { display: flex; flex-direction: column; gap: 3px; }
.review-status strong { font-size: 14px; }
.review-status span { font-size: 12px; }
.buttons { display: flex; flex-wrap: wrap; gap: 8px; flex-shrink: 0; }
.primary, .ghost { padding: 8px 16px; border-radius: 5px; font: inherit; font-weight: 600; cursor: pointer; }
.primary { border: 1px solid var(--accent); background: var(--accent); color: var(--on-accent); }
.ghost { border: 1px solid var(--line); background: var(--surface); color: var(--ink); }
.stop { color: var(--bad); }
.primary:disabled, .ghost:disabled { opacity: .55; cursor: not-allowed; }
.cards { display: flex; flex-wrap: wrap; margin-bottom: 18px; padding: 16px 0; border-block: 1px solid var(--line); }
.card { flex: 1 1 180px; padding: 0 20px; }
.card + .card { border-left: 1px solid var(--line); }
.card-label { display: block; font-size: 12px; color: var(--muted); }
.card-value { display: block; margin-top: 5px; font-size: 22px; font-weight: 650; font-variant-numeric: tabular-nums; }
.card-value small { font-size: 12px; color: var(--muted); font-weight: 400; }
.cards.stale .card-value { color: var(--muted); }
.review-heading { display: flex; align-items: center; gap: 12px; margin: 22px 0 10px; flex-wrap: wrap; }
.unit-search { flex: 1 1 220px; max-width: 320px; }
.unit-search input { width: 100%; padding: 7px 10px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); color: var(--ink); font: inherit; font-size: 13px; }
button.small { min-height: 32px; padding: 6px 11px; font-size: 12.5px; }
.compact-empty { padding: 14px 16px; font-size: 13px; }
.review-heading h3 { margin: 0; }
.review-heading > span { font-size: 12px; }
.preview-state { color: var(--warn); }
.section { margin: 0 0 10px; border: 1px solid var(--line); border-radius: 7px; background: var(--surface); }
.section > summary { padding: 12px 16px; cursor: pointer; background: var(--head); border-radius: 6px; overflow-wrap: anywhere; }
.section > summary small { margin-left: 10px; white-space: nowrap; }
.cost { font-variant-numeric: tabular-nums; white-space: nowrap; }
.force-row code { font-size: .9em; }
/* 占比条是**静态**的：这个仓库对跑测期间的持续动画有明确禁令
   （机器正在灌线速），所以只画一条不动的填充。 */
.cost-bar { flex: 0 0 72px; height: 4px; border-radius: 2px; background: var(--line); overflow: hidden; }
.cost-bar i { display: block; height: 100%; background: var(--accent); }
.units { margin: 0; padding: 0 16px 4px; list-style: none; }
.units li { display: flex; gap: 12px; align-items: baseline; padding: 12px 0; border-top: 1px solid var(--line); }
.units li.resumed .unit-title { color: var(--muted); }
.seq { flex: 0 0 42px; color: var(--muted); font-size: 12px; }
.unit-body { flex: 1 1 auto; min-width: 0; }
.unit-title { font-weight: 600; overflow-wrap: anywhere; }
.load { margin-top: 4px; font-size: 12px; color: var(--muted); overflow-wrap: anywhere; }
.targets { margin-top: 3px; font-size: 12px; color: var(--accent); overflow-wrap: anywhere; }
.trace { margin-top: 3px; font-size: 11.5px; }
.est { flex: 0 0 auto; font-size: 12px; color: var(--muted); }
.tag { margin-left: 6px; padding: 2px 6px; border-radius: 3px; background: var(--info-bg); font-size: 10.5px; font-weight: 500; }
.empty { padding: 30px 24px; border: 1px dashed var(--line); border-radius: 7px; color: var(--muted); background: var(--panel-2); }
.empty strong { display: block; color: var(--ink); font-size: 15px; }
.empty p { margin: 6px 0 16px; font-size: 13px; max-width: 60ch; }
.warn { margin: 0 0 8px; padding: 10px 13px; border-left: 3px solid var(--warn); background: var(--info-bg); }
.warn p { margin: 0 0 8px; }
.start-blocked { margin: 0 0 8px; }
.bad { margin: 0 0 12px; padding: 10px 13px; border-left: 3px solid var(--bad); background: var(--bad-bg); overflow-wrap: anywhere; }
.mono { font-family: var(--fm); }
.muted { color: var(--muted); }
@media (max-width: 760px) {
  .run-options { grid-template-columns: 1fr; }
  .switch + .switch { border-left: 0; border-top: 1px solid var(--line); }
  .switch { padding-block: 12px; }
  .review-actions { align-items: stretch; flex-direction: column; }
  .buttons { flex-shrink: 1; }
  .buttons button { flex: 1; }
  .advanced > summary span { display: block; margin: 5px 0 0 18px; }
  .card { flex-basis: 130px; padding: 8px 14px; }
}
@media (max-width: 480px) {
  .controls-heading { align-items: flex-start; flex-direction: column; }
  .units li { display: grid; grid-template-columns: 36px minmax(0, 1fr); gap: 4px 8px; }
  .est { grid-column: 2; }
  .cards { display: grid; grid-template-columns: 1fr 1fr; }
  .card-value { font-size: 19px; }
  .card:nth-child(3) { border-left: 0; }
  .review-heading { align-items: baseline; flex-wrap: wrap; }
}
</style>
