<script setup lang="ts">
import { computed } from 'vue';
import type { PlannedUnit } from '../../api/dto';
import { humanDuration } from '../../domain/progress';
import { plan, preview, previewIsCurrent } from '../../state/plan';
import { run, start, stop } from '../../state/run';
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
      <p class="muted">先预览，确认要跑的东西，再开跑。数量与耗时由服务端算。</p>
    </header>

    <div class="controls">
      <label>
        <span>每单元时长（秒）</span>
        <input v-model.number="plan.duration" type="number" min="1" max="86400" />
      </label>
      <label class="switch">
        <input v-model="plan.resume" type="checkbox" />
        <span>RESUME：跳过 24 小时内已 PASS 的单元</span>
      </label>
      <label class="switch">
        <input v-model="plan.screenshot" type="checkbox" />
        <span>每个吞吐单元后截图</span>
      </label>
      <label class="switch">
        <input v-model="plan.limitUdpByLinkSpeed" type="checkbox" />
        <span
          title="勾上后 UDP 的 -b 会被整条路径的可信上限压下来；预览里「最终下发参数」显示的就是裁剪后的值。"
        >
          按链路上限裁剪 UDP 发送速率
        </span>
      </label>
      <button type="button" class="ghost" :disabled="plan.previewing" @click="preview">
        {{ plan.previewing ? '预览中…' : stale ? '重新预览' : '预览' }}
      </button>
      <button type="button" class="primary" :disabled="!canStart" @click="start">
        {{ run.starting ? '启动中…' : '开始测试' }}
      </button>
      <button v-if="run.running" type="button" class="ghost" @click="stop">停止</button>
    </div>

    <GlobalDefaults />
    <NicPolicyTable />

    <p v-if="plan.previewError" class="bad" role="alert">{{ plan.previewError }}</p>
    <p v-if="run.startError" class="bad" role="alert">{{ run.startError }}</p>
    <p v-if="stale" class="warn" role="status">
      计划或运行参数已在上次预览后改变。请重新预览，确认新的单元数和耗时后再开始。
    </p>

    <div v-if="!out" class="empty">还没预览。点「预览」让服务端算一遍要跑什么。</div>
    <template v-else>
      <div class="cards">
        <div class="card">
          <span class="card-label">测试单元</span>
          <strong class="card-value">{{ units.length }}</strong>
        </div>
        <div class="card">
          <span class="card-label">预计耗时</span>
          <strong class="card-value">{{ estimate }}</strong>
        </div>
        <div class="card" v-if="plan.resume">
          <span class="card-label">预计跳过</span>
          <strong class="card-value">{{ resumedCount }}</strong>
        </div>
      </div>

      <p v-for="(notice, i) in visiblePreviewNotices" :key="i" class="warn">{{ notice }}</p>

      <h3>计划复核</h3>
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
              <!-- 「字段还在、实际却被另一条规则盖掉」光看请求体看不出来，
                   所以这里直接印最终门限和它来自哪一层。 -->
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
.controls {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-end;
  gap: 12px;
  margin: 0 0 14px;
  padding: 12px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--surface);
}
.controls label { display: flex; flex-direction: column; gap: 4px; }
.controls label span { font-size: 11.5px; color: var(--muted); }
.controls .switch { flex-direction: row; align-items: center; gap: 6px; }
.controls .switch span { font-size: 13px; color: var(--ink); }
input[type='number'] {
  width: 110px;
  padding: 7px 9px;
  border: 1px solid var(--line);
  border-radius: 4px;
  background: var(--canvas);
  color: var(--ink);
  font: inherit;
}
.primary, .ghost {
  padding: 8px 16px;
  border-radius: 4px;
  font: inherit;
  font-weight: 600;
  cursor: pointer;
}
.primary { border: 1px solid var(--accent); background: var(--accent); color: var(--on-accent); }
.ghost { border: 1px solid var(--line); background: var(--surface); color: var(--ink); }
.primary:disabled, .ghost:disabled { opacity: .55; cursor: default; }
.cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 10px; margin: 0 0 14px; }
.card { padding: 10px 12px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.card-label { display: block; font-size: 11.5px; color: var(--muted); }
.card-value { display: block; margin-top: 3px; font-size: 18px; }
.section { margin: 0 0 10px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
.section > summary { padding: 9px 12px; cursor: pointer; display: flex; gap: 10px; align-items: baseline; }
.units { margin: 0; padding: 0 12px 10px 12px; list-style: none; }
.units li { display: flex; gap: 10px; align-items: baseline; padding: 6px 0; border-top: 1px solid var(--line); }
.units li.resumed { opacity: .6; }
.seq { flex: 0 0 46px; color: var(--muted); font-size: 12px; }
.unit-body { flex: 1 1 auto; min-width: 0; }
.unit-title { overflow-wrap: anywhere; }
.load { font-size: 12px; color: var(--muted); overflow-wrap: anywhere; }
.targets { font-size: 12px; color: #145a94; overflow-wrap: anywhere; }
.trace { font-size: 11.5px; }
.est { flex: 0 0 auto; font-size: 12px; color: var(--muted); }
.tag { margin-left: 6px; padding: 1px 5px; border-radius: 3px; background: var(--info-bg); font-size: 10.5px; }
.empty { padding: 14px 16px; border: 1px dashed var(--line); border-radius: 6px; color: var(--muted); background: var(--panel-2); }
.warn { margin: 0 0 8px; padding: 8px 11px; border-left: 3px solid var(--focus); background: var(--info-bg); }
.bad { margin: 0 0 12px; padding: 9px 12px; border-left: 3px solid var(--bad); background: var(--bad-bg); }
.mono { font-family: var(--fm); }
.hint { margin: -6px 0 14px; font-size: 12px; }
.muted { color: var(--muted); }
</style>
