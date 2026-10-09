<script setup lang="ts">
import { computed } from 'vue';
import { prepareAfterUnknownStart, run, syncStatus } from '../../state/run';
import { ui } from '../../state/ui';
import RunPrepare from './RunPrepare.vue';
import RunProgress from './RunProgress.vue';

/**
 * 「执行」：准备（选项 + 预览 + 开始）与进度合在一页，按运行状态切换。
 *
 * 旧版是两页，点开始后自动跳过去；合成一页后切换由状态推出来，不另存：
 * 运行中、开始结果未确认、状态还没读到——一律看进度；本轮已结束（或留下了
 * 日志）时先看结果，点「准备下一轮」（`ui.preparing`）才回到准备面板。
 */
const showProgress = computed(() => {
  if (!run.synced || run.running) return true;
  if (run.startPhase === 'sending' || run.startPhase === 'unknown') return true;
  // 有日志也算「这一轮有东西可看」：受理之后、一个单元都没产生就结束的那一轮
  // 没有 run_id，原因只在日志里。
  return (!!run.status.run_id || run.lines.length > 0) && !ui.preparing;
});

/** 操作员在主控确认过没有运行之后才退出未知态；回到准备面板，必须重新预览。 */
function confirmNotRunning(): void {
  prepareAfterUnknownStart();
  if (run.startPhase === 'idle') ui.preparing = true;
}
</script>

<template>
  <section class="view">
    <header class="page-head">
      <h2>执行</h2>
    </header>

    <p v-if="run.startError" class="msg bad" role="alert">{{ run.startError }}</p>
    <div v-if="run.startPhase === 'unknown'" class="msg warn" role="alert">
      <p>启动结果未确认，请勿重复启动。核实未运行后重新准备。</p>
      <button type="button" class="ghost small" @click="syncStatus">再同步一次运行状态</button>
      <button
        v-if="run.synced && !run.running && !run.refreshError"
        type="button"
        class="ghost small"
        @click="confirmNotRunning"
      >已在主控确认没有运行，重新准备</button>
    </div>

    <RunProgress v-if="showProgress" @prepare="ui.preparing = true" />
    <RunPrepare v-else />
  </section>
</template>
