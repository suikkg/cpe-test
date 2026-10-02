<script setup lang="ts">
import { computed, onMounted } from 'vue';
import { errorMessage } from '../../api/client';
import { inner, listScenarioRuns, loadScenario, scenarioBlocksActions } from '../../state/inner';
import { run } from '../../state/run';

/**
 * 「历史 › 组合场景」：子网→内环组合运行。
 *
 * 载入会同时恢复子网计划和内环配置，两段都默认打开 RESUME，然后跳到内环页。
 */
const locked = computed(() => run.running || inner.busy || inner.status.running || scenarioBlocksActions() || !inner.synced);

async function reload(id: string): Promise<void> {
  try {
    await loadScenario(id);
  } catch (e) {
    inner.error = errorMessage(e);
  }
}

onMounted(() => { void listScenarioRuns(); });
</script>

<template>
  <div>
    <div class="toolbar">
      <span class="count">{{ inner.scenario.runs.length }} 条记录</span>
      <button type="button" class="ghost small push" @click="listScenarioRuns">刷新</button>
    </div>
    <p v-if="inner.error" class="msg bad" role="alert">{{ inner.error }}</p>
    <p v-if="!inner.scenario.runs.length" class="empty-state">还没有组合场景记录。</p>
    <div v-else class="table-wrap" tabindex="0" role="region" aria-label="组合场景记录，可横向滚动">
      <table class="data">
        <thead><tr><th scope="col">时间</th><th scope="col">状态</th><th scope="col">RESUME</th><th scope="col">操作</th></tr></thead>
        <tbody>
          <tr v-for="entry in inner.scenario.runs" :key="entry.id">
            <td>{{ entry.created_at || entry.id }}</td>
            <td :class="{ bad: !!entry.error }">{{ entry.error || entry.phase }}</td>
            <td>子网 {{ entry.resume_subnet ? '开' : '关' }} / 内环 {{ entry.resume_inner ? '开' : '关' }}</td>
            <td>
              <button
                type="button"
                class="ghost small"
                :disabled="locked"
                title="恢复子网计划和内环配置（两段默认 RESUME），到内环页核对后再开始"
                @click="reload(entry.id)"
              >载入并恢复重跑</button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </div>
</template>

<style scoped>
.bad { color: var(--bad); }
</style>
