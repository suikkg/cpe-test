<script setup lang="ts">
import { computed, onMounted } from 'vue';
import { errorMessage } from '../../api/client';
import { innerHistoryStatus } from '../../domain/inner';
import { inner, innerRunReport, listInnerRuns, loadInnerRunConfig, scenarioBlocksActions } from '../../state/inner';
import { run } from '../../state/run';
import { goto } from '../../state/ui';
import { saveFile } from '../download';

/**
 * 「历史 › 内环」：`inner_runs/` 下的每一轮。
 *
 * 「恢复重跑」只装载配置并生成预览（默认打开 RESUME），回到内环页核对网口、
 * 补填辅测机令牌后再开始；新结果另存为一轮记录。
 */
const locked = computed(() => run.running || inner.busy || inner.status.running || scenarioBlocksActions() || !inner.synced);
const megabytes = (bytes: number) => `${(bytes / 1048576).toFixed(1)} MB`;

async function report(id: string): Promise<void> {
  try {
    const result = await innerRunReport(id);
    saveFile(result.name, result.html, 'text/html;charset=utf-8');
  } catch (e) {
    inner.error = errorMessage(e);
  }
}
async function reload(id: string): Promise<void> {
  try {
    await loadInnerRunConfig(id);
    goto('inner');
  } catch (e) {
    inner.error = errorMessage(e);
  }
}

onMounted(() => { void listInnerRuns(); });
</script>

<template>
  <div>
    <div class="toolbar">
      <span class="count">{{ inner.runs.length }} 条记录</span>
      <button type="button" class="ghost small push" @click="listInnerRuns">刷新</button>
    </div>
    <p v-if="inner.error" class="msg bad" role="alert">{{ inner.error }}</p>
    <p v-if="!inner.runs.length" class="empty-state">还没有内环运行记录。</p>
    <div v-else class="table-wrap" tabindex="0" role="region" aria-label="内环运行记录，可横向滚动">
      <table class="data">
        <thead><tr>
          <th scope="col">时间</th><th scope="col">网口</th><th scope="col" class="num">单元</th>
          <th scope="col" class="num">PASS</th><th scope="col" class="num">RATE_FAIL</th>
          <th scope="col" class="num">NOT_EVALUATED</th><th scope="col">状态</th>
          <th scope="col" class="num">大小</th><th scope="col">操作</th>
        </tr></thead>
        <tbody>
          <tr v-for="entry in inner.runs" :key="entry.id">
            <td class="nowrap">{{ entry.created_at || entry.id }}</td>
            <td>{{ entry.links.join('、') || '—' }}</td>
            <td class="num">{{ entry.units }}</td>
            <td class="num">{{ entry.passed }}</td>
            <td class="num">{{ entry.rate_failed }}</td>
            <td class="num">{{ entry.not_evaluated }}</td>
            <td>
              <span v-if="entry.error" class="bad">{{ entry.error }}</span>
              <span v-else>{{ innerHistoryStatus(entry) }}</span>
            </td>
            <td class="num">{{ megabytes(entry.bytes) }}</td>
            <td>
              <div class="actions">
                <button type="button" class="ghost small" :disabled="!entry.has_report" @click="report(entry.id)">下载报告</button>
                <button
                  type="button"
                  class="ghost small"
                  :disabled="locked || !entry.has_config"
                  title="载入当时的配置到内环页（默认 RESUME），核对后再开始"
                  @click="reload(entry.id)"
                >恢复重跑</button>
              </div>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </div>
</template>

<style scoped>
table { min-width: 860px; }
.nowrap { white-space: nowrap; }
.bad { color: var(--bad); }
.actions { display: flex; gap: 6px; flex-wrap: wrap; }
</style>
