<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { api, downloadQuery } from '../../api/client';
import type { RunEntry } from '../../api/dto';

/**
 * 「历史运行」：列出 `runs/` 下的每一轮，并给出打包下载。
 *
 * 这一页和 `bundle.zip` 是**一个功能的两半**（ADR-15）：不做列表页，远程用户
 * 拿不到 run id，下载链接就形同虚设。而 11.5 小时的测试隔夜回来找报告是常态。
 */

const entries = ref<RunEntry[]>([]);
const loading = ref(false);
const error = ref('');

async function load(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    entries.value = await api.get<RunEntry[]>('/api/runs');
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

function size(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/**
 * 下载链接必须带 token——鉴权先于路由，不带就是 401。
 *
 * 这里是**唯一**允许把 token 放进 URL 的地方：浏览器发起的下载不会带自定义头，
 * 而 `<a download>` 的相对 URL 也不继承 `fetch` 那套。
 *
 * **代价要说清楚，不要假装没有**：`download` 免掉的只是**会话历史**那一条记录，
 * 浏览器的下载列表（`chrome://downloads`）会长期保留来源 URL，里面就带着口令。
 * 这和 `api/client.ts` 里 `adoptTokenFromUrl()` 特地把地址栏 `?token=` 抹掉的
 * 理由是同一个，所以这里不是「不进历史」，而是**换了个地方留痕**。
 *
 * 暂时接受，理由是这条链路上口令本来就不是秘密：控制台是明文 HTTP（没有 TLS），
 * 口令在同一个局域网上以明文 header 往返，启动时打印的地址也带着 `?token=`。
 * 下载列表里多一份，威胁模型上并没有引入新的攻击者。
 *
 * 真要修的话是走 `api` 拿 `blob` 再 `URL.createObjectURL`——那要给 `client.ts`
 * 开一个非 JSON 出口，并且整个包要先进浏览器内存。留作独立改动。
 *
 * 查询串由 `client.ts::downloadQuery()` 拼：口令怎么取只能有一处实现，这里
 * 以前自己读 `sessionStorage`，把 `TOKEN_KEY` 抄成了第二份。
 */
function bundleUrl(id: string): string {
  return `/api/runs/${encodeURIComponent(id)}/bundle.zip${downloadQuery()}`;
}

onMounted(load);
</script>

<template>
  <section class="view">
    <header class="view-head">
      <h2>历史运行</h2>
      <p class="muted">
        每一轮的报告、逐样本 CSV 和原始输出。远程访问时用「下载包」把整个目录取回本地。
      </p>
    </header>

    <div class="bar">
      <button type="button" class="ghost" :disabled="loading" @click="load">
        {{ loading ? '刷新中…' : '刷新' }}
      </button>
      <span class="muted">{{ entries.length }} 轮</span>
    </div>

    <p v-if="error" class="bad" role="alert">{{ error }}</p>

    <div v-if="entries.length === 0 && !loading" class="empty">
      还没有跑过测试。
    </div>
    <div v-else class="scroll">
      <table>
        <thead>
          <tr>
            <th>运行</th><th>时间</th><th>产物</th><th class="num">大小</th><th>取回</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="entry in entries" :key="entry.id">
            <td class="mono">{{ entry.id }}</td>
            <td class="muted">{{ entry.modified || '—' }}</td>
            <td>
              <span v-if="entry.has_report" class="chip ok">报告</span>
              <span v-if="entry.has_xlsx" class="chip">Excel</span>
              <span v-if="entry.has_rows" class="chip" title="有 rows.jsonl，崩溃后可重放报告">
                可重放
              </span>
              <span v-if="!entry.has_report && !entry.has_rows" class="chip warn">仅日志</span>
            </td>
            <td class="num mono">{{ size(entry.bytes) }}</td>
            <td>
              <a class="dl" :href="bundleUrl(entry.id)" :download="`${entry.id}.zip`">下载包</a>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <p class="muted hint">
      主控崩溃/断电后，只要包里有 <code>rows.jsonl</code>，
      <code>cpe_test report runs/&lt;目录&gt;</code> 就能把已完成部分重放成完整报告。
    </p>
  </section>
</template>

<style scoped>
.bar { display: flex; align-items: center; gap: 12px; margin: 0 0 14px; }
.scroll { max-width: 100%; overflow-x: auto; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
table { width: 100%; border-collapse: separate; border-spacing: 0; font-size: 13px; }
th, td { padding: 8px 11px; text-align: left; border-bottom: 1px solid var(--line); }
thead th { background: var(--head); font-size: 11.5px; color: var(--muted); white-space: nowrap; }
tbody tr:last-child td { border-bottom: 0; }
.num { text-align: right; font-variant-numeric: tabular-nums; }
.chip {
  display: inline-block; margin-right: 5px; padding: 1px 6px;
  border-radius: 3px; background: var(--panel-2); color: var(--muted); font-size: 11px;
}
.chip.ok { background: var(--ok-bg); color: var(--ok); }
.chip.warn { background: var(--info-bg); color: var(--focus); }
.dl { color: var(--accent); }
.ghost {
  padding: 6px 14px; border: 1px solid var(--line); border-radius: 4px;
  background: var(--surface); color: var(--ink); font: inherit; cursor: pointer;
}
.ghost:disabled { opacity: .55; cursor: default; }
.empty { padding: 14px 16px; border: 1px dashed var(--line); border-radius: 6px; color: var(--muted); background: var(--panel-2); }
.bad { margin: 0 0 12px; padding: 9px 12px; border-left: 3px solid var(--bad); background: var(--bad-bg); }
.hint { margin: 12px 0 0; font-size: 12.5px; }
.mono { font-family: var(--fm); }
code { font-family: var(--fm); }
</style>
