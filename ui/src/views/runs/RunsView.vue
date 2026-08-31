<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { api, downloadQuery } from '../../api/client';
import type { ReplayOut, RunEntry, RunRequestOut } from '../../api/dto';
import { adoptRunRequest, preview } from '../../state/plan';
import { goto } from '../../state/ui';

/**
 * 「历史运行」：列出 `runs/` 下的每一轮，并给出取回、重放、重跑三个出口。
 *
 * 这一页和 `bundle.zip` 是**一个功能的两半**（ADR-15）：不做列表页，远程用户
 * 拿不到 run id，下载链接就形同虚设。而 11.5 小时的测试隔夜回来找报告是常态。
 */

const entries = ref<RunEntry[]>([]);
const loading = ref(false);
const error = ref('');
const notice = ref('');
/** 正在重放/装载的那一行，用来禁掉按钮并给出「在做了」的反馈。 */
const busy = ref('');
/**
 * 重跑时是否跳过 24 小时内已 PASS 的单元（就是 RESUME）。
 *
 * 复测最常用的选项，所以放在这一页的入口上，而不是让人跑去执行页再找一遍。
 */
const skipPassed = ref(true);

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

/**
 * 重放报告。**不要求「必须没有报告才能点」**。
 *
 * 崩溃留下的 `report.html` 可能是写到一半的，也可能是补跑之前的旧版本；
 * 「已经有报告」恰恰是最需要用新数据盖掉它的情形之一。重放本身是幂等的——
 * 同一批 `rows.jsonl` 放几次都是同一份报告。服务端只挡一种情况：正在跑的那一轮。
 */
async function replay(entry: RunEntry): Promise<void> {
  busy.value = entry.id;
  error.value = '';
  notice.value = '';
  try {
    const out = await api.post<ReplayOut>('/api/runs/report', { id: entry.id });
    const parts = [`已从 ${out.rows} 行结果重放：${out.report}`];
    if (out.skipped > 0) {
      parts.push(`跳过 ${out.skipped} 行无法解析的记录（通常是崩溃时写了一半的最后一行）`);
    }
    parts.push(...out.warnings);
    notice.value = parts.join('；');
    await load();
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    busy.value = '';
  }
}

/**
 * 重新执行：把这一轮的计划**装载回控制台**，然后跳到「执行」页并预览一次。
 *
 * 有意不直接开跑。`plan_hash` 是「界面上确认的东西 == 实际跑的东西」唯一的
 * 强制点，而隔了一夜网口拓扑可能已经变了，老计划里的端点未必还在。该看到的是
 * 复核页上的差异，而不是一轮悄悄少跑了几条链路的测试。
 */
async function rerun(entry: RunEntry): Promise<void> {
  busy.value = entry.id;
  error.value = '';
  notice.value = '';
  try {
    const out = await api.post<RunRequestOut>('/api/runs/request', { id: entry.id });
    if (!adoptRunRequest(out.request, skipPassed.value)) {
      error.value = '这一轮的计划读不出来（多半是升级前的旧格式）';
      return;
    }
    goto('run');
    // 装载完立刻预览一次：拓扑变了要当场看见，而不是等人点了「开始」才被拒。
    await preview();
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    busy.value = '';
  }
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
      <label class="switch">
        <input v-model="skipPassed" type="checkbox" />
        <span>重新执行时跳过 24 小时内已 PASS 的单元（RESUME）</span>
      </label>
    </div>

    <p v-if="error" class="bad" role="alert">{{ error }}</p>
    <p v-if="notice" class="ok" role="status">{{ notice }}</p>

    <div v-if="entries.length === 0 && !loading" class="empty">
      还没有跑过测试。
    </div>
    <div v-else class="scroll">
      <table>
        <thead>
          <tr>
            <th>运行</th><th>时间</th><th>产物</th><th class="num">大小</th><th>操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="entry in entries" :key="entry.id">
            <td class="mono">{{ entry.id }}</td>
            <td class="muted">{{ entry.modified || '—' }}</td>
            <td>
              <span v-if="entry.has_report" class="chip ok">报告</span>
              <span v-if="entry.has_xlsx" class="chip">Excel</span>
              <span
                v-if="entry.has_rows"
                class="chip"
                title="目录里有 rows.jsonl（每个单元跑完即落盘的结果明细），可以据此重新渲染报告"
              >
                结果明细
              </span>
              <span v-if="!entry.has_report && !entry.has_rows" class="chip warn">仅日志</span>
            </td>
            <td class="num mono">{{ size(entry.bytes) }}</td>
            <td class="actions">
              <a class="dl" :href="bundleUrl(entry.id)" :download="`${entry.id}.zip`">下载包</a>
              <button
                v-if="entry.has_rows"
                type="button"
                class="ghost small"
                :disabled="busy === entry.id"
                :title="
                  entry.has_report
                    ? '用 rows.jsonl 重新渲染报告，覆盖现有的 report.html 与 summary.xlsx'
                    : '这一轮没写出报告，用 rows.jsonl 把它放出来'
                "
                @click="replay(entry)"
              >
                {{ entry.has_report ? '重新生成报告' : '恢复报告' }}
              </button>
              <button
                v-if="entry.has_request"
                type="button"
                class="ghost small"
                :disabled="busy === entry.id"
                title="把这一轮的计划装载回控制台并跳到「执行」页预览；确认无误后再点「开始测试」"
                @click="rerun(entry)"
              >
                重新执行
              </button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <dl class="legend">
      <dt>结果明细</dt>
      <dd>
        目录里有 <code>rows.jsonl</code>——每个单元跑完就追加落盘的结果行。
        主控崩溃/断电时报告可能还没写出来，但这份数据在，所以随时可以按上面的按钮
        把报告重新渲染出来（命令行等价物是
        <code>cpe_test report runs/&lt;目录&gt;</code>）。<strong>不要求先没有报告</strong>：
        重放是幂等的，同一批行放几次都是同一份报告。
      </dd>
      <dt>重新执行</dt>
      <dd>
        目录里有 <code>request.json</code>，即这一轮的完整计划原文（控制台发起的运行才有）。
        点它会把计划装载回控制台并跳到「执行」页预览一次——<strong>不会直接开跑</strong>：
        隔了一夜网口拓扑可能变了，该看到的是复核页上的差异，而不是一轮悄悄少跑几条链路的测试。
      </dd>
      <dt>崩溃之后怎么办</dt>
      <dd>
        先看这一行有没有「结果明细」。有就点「恢复报告」，已完成的部分会变成一份完整报告；
        再点「重新执行」并勾上上面的 RESUME，只有失败和没跑到的单元会重来。
        <strong>崩溃恢复的语义是「结果不丢，但运行本身不续跑」</strong>——被打断的那个单元
        会整个重跑，而不是接着半份测量往下算。
      </dd>
    </dl>
  </section>
</template>

<style scoped>
.bar { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; margin: 0 0 14px; }
.switch { display: flex; align-items: center; gap: 6px; font-size: 13px; }
.scroll { max-width: 100%; overflow-x: auto; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); }
table { width: 100%; border-collapse: separate; border-spacing: 0; font-size: 13px; }
th, td { padding: 8px 11px; text-align: left; border-bottom: 1px solid var(--line); }
thead th { background: var(--head); font-size: 11.5px; color: var(--muted); white-space: nowrap; }
tbody tr:last-child td { border-bottom: 0; }
.num { text-align: right; font-variant-numeric: tabular-nums; }
.actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
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
.ghost.small { padding: 3px 10px; font-size: 12px; }
.ghost:disabled { opacity: .55; cursor: default; }
.empty { padding: 14px 16px; border: 1px dashed var(--line); border-radius: 6px; color: var(--muted); background: var(--panel-2); }
.bad { margin: 0 0 12px; padding: 9px 12px; border-left: 3px solid var(--bad); background: var(--bad-bg); }
.ok { margin: 0 0 12px; padding: 9px 12px; border-left: 3px solid var(--ok); background: var(--ok-bg); overflow-wrap: anywhere; }
.legend { margin: 16px 0 0; font-size: 12.5px; }
.legend dt { margin: 10px 0 3px; font-weight: 700; }
.legend dd { margin: 0; color: var(--muted); }
.mono { font-family: var(--fm); }
code { font-family: var(--fm); }
</style>
