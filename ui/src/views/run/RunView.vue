<script setup lang="ts">
import { computed } from 'vue';
import type { PlannedUnit } from '../../api/dto';
import { humanDuration } from '../../domain/progress';
import { plan, preview, previewIsCurrent } from '../../state/plan';
import { run, start, stop } from '../../state/run';
import { goto } from '../../state/ui';
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
// socket 缓冲诊断属于底层排查信息，暂不在 WebUI 预览区展开。
// 保留其它预览提示，避免把真正影响执行的错误一并隐藏。
const visiblePreviewNotices = computed(
  () => out.value?.notices.filter((notice) => !notice.includes('socket 缓冲')) ?? [],
);

/** 按 sections 分组的单元；sections 空时退化成一个「全部」组。 */
const grouped = computed(() => {
  const value = out.value;
  if (!value) return [];
  const bySeq = new Map(value.units.map((u) => [u.seq, u]));
  const sections = value.sections ?? [];
  if (sections.length === 0) {
    return [{ title: '全部单元', units: value.units }];
  }
  return sections.map((section) => ({
    title: section.title,
    units: section.unit_seqs.map((seq) => bySeq.get(seq)).filter((u): u is PlannedUnit => !!u),
  }));
});

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
  () => !!out.value?.plan_hash && !stale.value && !run.running && !run.starting,
);
</script>

<template>
  <section class="view">
    <header class="view-head">
      <h2>执行</h2>
      <p class="muted">预览测试单元、预计耗时和最终门限，确认后开始测试。</p>
    </header>

    <section class="controls" aria-labelledby="run-options-title">
      <div class="controls-heading">
        <h3 id="run-options-title">运行参数</h3>
        <label class="duration-field">
          <span>每单元时长</span>
          <span class="number-with-unit">
            <input v-model.number="plan.duration" type="number" min="1" max="86400" />
            <span>秒</span>
          </span>
        </label>
      </div>
      <div class="run-options">
        <label class="switch">
          <input v-model="plan.resume" type="checkbox" />
          <span><strong>RESUME</strong><small>跳过 24 小时内已 PASS 的单元</small></span>
        </label>
        <label class="switch">
          <input v-model="plan.screenshot" type="checkbox" />
          <span><strong>保存截图</strong><small>每个吞吐单元后截图</small></span>
        </label>
        <label class="switch">
          <input v-model="plan.limitUdpByLinkSpeed" type="checkbox" />
          <span title="勾上后 UDP 的 -b 会被整条路径的可信上限压下来；预览里显示最终下发参数。">
            <strong>按链路上限裁剪 UDP 发送速率</strong><small>在预览中复核裁剪后的参数</small>
          </span>
        </label>
      </div>
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
        <button v-else type="button" class="primary" :disabled="!canStart" @click="start">
          {{ run.starting ? '启动中…' : '开始测试' }}
        </button>
        <button v-if="run.running" type="button" class="ghost stop" @click="stop">停止</button>
      </div>
    </div>

    <p v-if="plan.previewError" class="bad" role="alert">{{ plan.previewError }}</p>
    <p v-if="run.startError" class="bad" role="alert">{{ run.startError }}</p>
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
      </div>
      <div v-if="units.length === 0" class="empty">
        <strong>当前计划没有可执行单元</strong>
        <p>检查测试计划中的链路集合和任务，并根据上方预览提示调整。</p>
        <button type="button" class="ghost" @click="goto('plan')">检查测试计划</button>
      </div>
      <details v-for="(group, gi) in grouped" :key="gi" class="section" open>
        <summary>
          <strong>{{ group.title }}</strong>
          <small class="muted">{{ group.units.length }} 个单元</small>
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
                  · {{ traceBySeq.get(unit.seq)!.direction }}
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
    </template>
  </section>
</template>

<style scoped>
.controls { margin-bottom: 14px; border: 1px solid var(--line); border-radius: 8px; background: var(--surface); }
.controls-heading { display: flex; justify-content: space-between; align-items: center; gap: 14px; padding: 14px 18px; border-bottom: 1px solid var(--line); }
.controls-heading h3 { margin: 0; }
.duration-field, .number-with-unit { display: flex; align-items: center; gap: 10px; }
.duration-field { font-size: 13px; }
.number-with-unit { color: var(--muted); }
input[type='number'] { width: 94px; padding: 7px 9px; border: 1px solid var(--line); border-radius: 5px; background: var(--panel-2); color: var(--ink); font: inherit; }
.run-options { display: grid; grid-template-columns: 1fr 1fr 1.3fr; }
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
.review-heading { display: flex; align-items: center; gap: 12px; margin: 22px 0 10px; }
.review-heading h3 { margin: 0; }
.review-heading > span { font-size: 12px; }
.preview-state { color: var(--focus); }
.section { margin: 0 0 10px; border: 1px solid var(--line); border-radius: 7px; background: var(--surface); }
.section > summary { padding: 12px 16px; cursor: pointer; background: var(--head); border-radius: 6px; overflow-wrap: anywhere; }
.section > summary small { margin-left: 10px; white-space: nowrap; }
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
.warn { margin: 0 0 8px; padding: 10px 13px; border-left: 3px solid var(--focus); background: var(--info-bg); }
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
