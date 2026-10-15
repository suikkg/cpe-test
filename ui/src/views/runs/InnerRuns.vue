<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { api, downloadQuery, errorMessage } from '../../api/client';
import { innerHistoryStatus } from '../../domain/inner';
import { inner, innerRunReport, listInnerRuns, loadInnerRunConfig, scenarioBlocksActions } from '../../state/inner';
import { run } from '../../state/run';
import { goto } from '../../state/ui';
import { saveBase64File, saveFile } from '../download';

/**
 * 「历史 › 内环」：`inner_runs/` 下的每一轮。
 *
 * 「恢复重跑」只装载配置并生成预览（默认打开 RESUME），回到内环页核对网口、
 * 补填辅测机令牌后再开始；新结果另存为一轮记录。
 */
interface CompareRow { title:string; status:string; baseline_index:number|null; current_index:number|null; baseline_verdict:string|null; current_verdict:string|null; baseline_mbps:number|null; current_mbps:number|null; rate_change_pct?:number|null }
interface Comparison { baseline_finished:boolean; current_finished:boolean; regressions:number; added:number; missing:number; unmatchable:number; resumed:number; rows:CompareRow[] }
const baseline = ref('');
const comparison = ref<Comparison|null>(null);
const comparing = ref(false);
const query = ref('');
const entries = computed(() => inner.runs.filter(r => `${r.id} ${r.created_at} ${r.links.join(' ')} ${r.error ?? ''}`.toLocaleLowerCase().includes(query.value.trim().toLocaleLowerCase())));
const statusLabels: Record<string,string> = { SAME:'判定相同', SLOWER:'速率下降 ≥5%', REGRESSION:'回归', RECOVERED:'恢复通过', CHANGED:'判定变化', ADDED:'新增', MISSING:'缺失', UNMATCHABLE:'无法唯一匹配', RESUMED:'复用历史', SKIPPED:'用户跳过' };
const speed = (v:number|null) => v == null ? '—' : v.toFixed(1);
async function compare(id:string): Promise<void> {
  if (!baseline.value || comparing.value) return;
  comparing.value = true; inner.error = '';
  try { comparison.value = await api.post<Comparison>('/api/inner/runs/compare', { baseline:baseline.value, current:id }); }
  catch(e) { comparison.value = null; inner.error = errorMessage(e); }
  finally { comparing.value = false; }
}
const locked = computed(() => run.running || inner.busy || inner.status.running || scenarioBlocksActions() || !inner.synced);
const bundleUrl = (id:string) => `/api/inner/runs/${encodeURIComponent(id)}/bundle.zip${downloadQuery()}`;
const megabytes = (bytes: number) => `${(bytes / 1048576).toFixed(1)} MB`;

async function report(id: string): Promise<void> {
  try {
    const result = await innerRunReport(id);
    saveFile(result.name, result.html, 'text/html;charset=utf-8');
  } catch (e) {
    inner.error = errorMessage(e);
  }
}
async function excel(id: string): Promise<void> {
  try { const out = await api.post<{ name:string; base64:string }>('/api/inner/runs/excel', { id }); saveBase64File(out.name, out.base64); }
  catch(e) { inner.error = errorMessage(e); }
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
      <label class="grow"><span class="sr-only">搜索内环历史</span><input v-model="query" type="search" aria-label="搜索内环历史" placeholder="时间、网口、状态"></label><span class="count">{{ entries.length }} / {{ inner.runs.length }} 条记录</span>
      <button type="button" class="ghost small push" @click="listInnerRuns">刷新</button>
    </div>
    <p v-if="inner.error" class="msg bad" role="alert">{{ inner.error }}</p>
    <p v-if="!inner.runs.length" class="empty-state">还没有内环运行记录。</p><p v-else-if="!entries.length" class="empty-state">无匹配记录。</p>
    <p v-if="baseline" class="hint">对比基线：{{ baseline }}<button class="ghost small" @click="baseline = ''; comparison = null">清除基线</button></p>
    <div v-if="entries.length" class="table-wrap" tabindex="0" role="region" aria-label="内环运行记录，可横向滚动">
      <table class="data">
        <thead><tr>
          <th scope="col">时间</th><th scope="col">网口</th><th scope="col" class="num">单元</th>
          <th scope="col" class="num">PASS</th><th scope="col" class="num">历史复用</th><th scope="col" class="num">跳过</th><th scope="col" class="num">RATE_FAIL</th>
          <th scope="col" class="num">NOT_EVALUATED</th><th scope="col">状态</th>
          <th scope="col" class="num">大小</th><th scope="col">操作</th>
        </tr></thead>
        <tbody>
          <tr v-for="entry in entries" :key="entry.id">
            <td class="nowrap">{{ entry.created_at || entry.id }}</td>
            <td>{{ entry.links.join('、') || '—' }}</td>
            <td class="num">{{ entry.units }}</td>
            <td class="num">{{ entry.passed }}</td><td class="num">{{ entry.resumed ?? 0 }}</td><td class="num">{{ entry.skipped ?? 0 }}</td>
            <td class="num">{{ entry.rate_failed }}</td>
            <td class="num">{{ entry.not_evaluated }}</td>
            <td>
              <span v-if="entry.error" class="bad">{{ entry.error }}</span>
              <span v-else>{{ innerHistoryStatus(entry) }}</span>
            </td>
            <td class="num">{{ megabytes(entry.bytes) }}</td>
            <td>
              <div class="actions"><a class="ghost small dl" :href="bundleUrl(entry.id)" :download="`${entry.id}.zip`">下载包</a>
                <button type="button" class="ghost small" :disabled="!entry.has_report" @click="report(entry.id)">下载报告</button><button class="ghost small" :disabled="comparing" @click="baseline = entry.id; comparison = null">设为基线</button><button v-if="baseline && baseline !== entry.id" class="ghost small" :disabled="comparing" @click="compare(entry.id)">与基线对比</button>
                <button type="button" class="ghost small" :disabled="!entry.has_excel" @click="excel(entry.id)">下载 Excel</button>
                <button
                  type="button"
                  class="ghost small"
                  :disabled="locked || !entry.has_config"
                  title="载入历史配置，默认启用 RESUME"
                  @click="reload(entry.id)"
                >恢复重跑</button>
              </div>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <section v-if="comparison" aria-label="内环两轮对比" class="panel">
      <div class="toolbar"><h3>两轮对比</h3><button class="ghost small" @click="saveFile('cpe-inner-comparison.json', JSON.stringify(comparison, null, 2), 'application/json')">下载对比</button></div>
      <p v-if="!comparison.baseline_finished || !comparison.current_finished" class="warn">{{ !comparison.baseline_finished ? '基线未收尾；' : '' }}{{ !comparison.current_finished ? '本轮未收尾。' : '' }}</p>
      <p>{{ comparison.regressions }} 项回归 · {{ comparison.added }} 项新增 · {{ comparison.missing }} 项缺失 · {{ comparison.unmatchable }} 项无法唯一匹配 · {{ comparison.resumed }} 项复用历史</p>
      <div class="table-wrap"><table class="data"><thead><tr><th scope="col">单元</th><th scope="col">变化</th><th scope="col">基线判定</th><th scope="col">本轮判定</th><th scope="col">基线 RX Mbps</th><th scope="col">本轮 RX Mbps</th><th scope="col">速率变化</th></tr></thead><tbody><tr v-for="(row,index) in comparison.rows" :key="index"><td>{{ row.title }}</td><td>{{ statusLabels[row.status] ?? row.status }}</td><td>{{ row.baseline_verdict ?? '—' }}</td><td>{{ row.current_verdict ?? '—' }}</td><td>{{ speed(row.baseline_mbps) }}</td><td>{{ speed(row.current_mbps) }}</td><td>{{ row.rate_change_pct == null ? '—' : `${row.rate_change_pct.toFixed(1)}%` }}</td></tr></tbody></table></div>
    </section>
  </div>
</template>

<style scoped>
table { min-width: 860px; }
.nowrap { white-space: nowrap; }
.bad { color: var(--bad); }
.actions { display: flex; gap: 6px; flex-wrap: wrap; }
</style>
