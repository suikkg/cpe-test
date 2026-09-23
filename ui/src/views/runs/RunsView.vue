<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { api, downloadQuery, errorMessage } from '../../api/client';
import type { CompareOut, ReplayOut, RunEntry, RunRequestOut } from '../../api/dto';
import { adoptRunRequest, preview } from '../../state/plan';
import { filterByQuery, visibleCountLabel } from '../../domain/search';
import { goto, ui } from '../../state/ui';

/**
 * 「历史运行」：列出 `runs/` 下的每一轮，并给出取回、重放、重跑三个出口。
 *
 * 这一页和 `bundle.zip` 是**一个功能的两半**（ADR-15）：不做列表页，远程用户
 * 拿不到 run id，下载链接就形同虚设。而 11.5 小时的测试隔夜回来找报告是常态。
 */

const entries = ref<RunEntry[]>([]);

/**
 * 页内搜索。
 *
 * `meta.json` 落地之后，这一页终于有了**开始时间**和**判定分布**：
 * `started` 是这一轮真正开跑的时刻，和 `modified`（目录最后被写过的时刻，
 * 恢复一次报告就会变）是两回事，两个都留着可搜。
 *
 * 仍然**没有**被测设备型号——那需要一个运行身份字段，这一轮没做。
 */
const shownEntries = computed(() =>
  filterByQuery(entries.value, ui.runs.query, (entry) => [
    entry.id,
    entry.modified,
    entry.started,
    verdictSummary(entry),
  ]),
);

/**
 * 一轮的结论摘要，例如 `28/30 通过 · 2 未达标`。
 *
 * 读不到 `meta.json` 时返回空串——**不能把全 0 当成结论**：升级前写的旧目录
 * 和「真的一个单元都没跑」在数字上长得一模一样。
 *
 * 分母只有 `PASS + RATE_FAIL`，和报告顶部、和 `passTone` 同一个算式。
 * 一个都没判过时**不写「x/y 通过」**：拿 `total` 兜底的话，一轮全是 MEASURED
 * （Observe 模式 / 没配门限）的运行会显示「0/10 通过」，读起来是「全挂了」，
 * 而同一轮的报告写的是「没有单元设了验收门限」。两块屏幕，同一轮，相反的印象。
 */
function verdictSummary(entry: RunEntry): string {
  if (!entry.has_meta || !entry.totals) return '';
  const t = entry.totals;
  const judged = t.pass + t.rate_fail;
  if (judged === 0 && t.total === 0) return '';
  const parts = judged > 0 ? [`${t.pass}/${judged} 通过`] : [`${t.total} 个单元`];
  if (t.measured) parts.push(`${t.measured} 仅测量`);
  if (t.rate_fail) parts.push(`${t.rate_fail} 未达标`);
  if (t.not_evaluated) parts.push(`${t.not_evaluated} 未评估`);
  if (t.setup_error) parts.push(`${t.setup_error} 准备失败`);
  if (t.skipped) parts.push(`${t.skipped} 已跳过`);
  return parts.join(' · ');
}

/** 通过率的着色：与报告顶部同一个算式（分母只有 PASS + RATE_FAIL）。 */
function passTone(entry: RunEntry): 'ok' | 'bad' | '' {
  const t = entry.totals;
  if (!entry.has_meta || !t) return '';
  const judged = t.pass + t.rate_fail;
  if (judged === 0) return '';
  return t.rate_fail === 0 ? 'ok' : 'bad';
}
const entryCountLabel = computed(() =>
  visibleCountLabel(shownEntries.value.length, entries.value.length),
);
const selectedEntry = computed(
  () => entries.value.find((entry) => entry.id === ui.runs.selected) ?? null,
);
const selectedHidden = computed(
  () => !!selectedEntry.value && !shownEntries.value.some((entry) => entry.id === ui.runs.selected),
);
function pickEntry(id: string): void {
  ui.runs.selected = ui.runs.selected === id ? '' : id;
}
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
    error.value = errorMessage(e);
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
/**
 * 对比用的基线（旧的那一轮）。空 = 还没选。
 *
 * 选基线和选详情是两件事，所以用两个状态：详情跟着「我在看哪一条」走，
 * 基线跟着「我要拿哪一条当参照」走。合成一个的话，点开详情就会把基线换掉。
 */
const baselineId = ref('');

function toggleBaseline(id: string): void {
  baselineId.value = baselineId.value === id ? '' : id;
}

/**
 * 两轮对比。基线必须比本轮**旧**——目录名带时间戳，直接按字符串比就够。
 *
 * 反着选不拦（有时就是想看「新的比旧的好在哪」），但要在提示语里说清楚
 * 哪一份当了基线，否则「+15%」的方向会被读反。
 */
async function compare(entry: RunEntry): Promise<void> {
  const baseline = baselineId.value;
  if (!baseline || baseline === entry.id) return;
  busy.value = entry.id;
  error.value = '';
  notice.value = '';
  try {
    const out = await api.post<CompareOut>('/api/runs/compare', {
      baseline,
      current: entry.id,
    });
    const parts = [
      `已对比（基线 ${out.baseline} → 本轮 ${out.current}）：${out.report}`,
      `判定变坏 ${out.regressed} · 速率下降 ${out.slower} · 判定转好 ${out.fixed} · 新增 ${out.added} · 缺失 ${out.disappeared} · 无实质变化 ${out.unchanged}`,
    ];
    if (!out.same_plan) {
      parts.push('两轮不是同一套计划，「新增/缺失」说的是计划差异，不是设备表现');
    }
    notice.value = parts.join('；');
    await load();
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    busy.value = '';
  }
}

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
    error.value = errorMessage(e);
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
    error.value = errorMessage(e);
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
      <p class="muted">取回每一轮的报告与原始数据，也可以恢复报告或载入计划重新执行。</p>
    </header>

    <div class="bar">
      <div class="list-heading">
        <strong>运行记录 <span class="count">{{ entries.length }}</span></strong>
        <button type="button" class="ghost" :disabled="loading || !!busy" @click="load">
          {{ loading ? '刷新中…' : '刷新' }}
        </button>
      </div>
      <label class="switch">
        <input v-model="skipPassed" type="checkbox" />
        <span><strong>重新执行时启用 RESUME</strong><small>跳过 24 小时内已 PASS 的单元</small></span>
      </label>
    </div>

    <p v-if="error" class="bad" role="alert">{{ error }}</p>
    <p v-if="notice" class="notice" role="status">{{ notice }}</p>

    <div v-if="loading && entries.length === 0" class="empty" role="status">正在读取运行记录…</div>
    <div v-else-if="entries.length === 0 && !error" class="empty">
      <strong>还没有运行记录</strong>
      <p>完成一轮测试后，可在这里下载报告、样本 CSV 和原始输出。</p>
      <button type="button" @click="goto('run')">前往执行</button>
    </div>
    <div v-else-if="entries.length" class="runs-bar">
      <label class="search">
        <span class="sr-only">搜索运行记录</span>
        <input
          type="search"
          :value="ui.runs.query"
          placeholder="搜运行目录 ID、开始时间或结论"
          @input="ui.runs.query = ($event.target as HTMLInputElement).value"
        />
      </label>
      <button v-if="ui.runs.query" type="button" class="ghost small" @click="ui.runs.query = ''">
        清空搜索
      </button>
      <span class="muted">{{ entryCountLabel }}</span>
    </div>

    <p v-if="baselineId" class="hint" role="status">
      对比基线：<strong class="mono">{{ baselineId }}</strong>。在另一轮上点「与基线对比」出差异报告。
      <button type="button" class="linklike" @click="baselineId = ''">取消基线</button>
    </p>

    <p v-if="selectedHidden" class="hint" role="status">
      选中的运行不在搜索结果里，下面的详情仍是它。
    </p>

    <div v-if="selectedEntry" class="run-detail" aria-label="运行详情">
      <div class="run-detail-head">
        <strong class="mono">{{ selectedEntry.id }}</strong>
        <button type="button" class="ghost small" @click="ui.runs.selected = ''">收起详情</button>
      </div>
      <dl>
        <dt>开始时间</dt>
        <dd>{{ selectedEntry.started || '未知（升级前的旧目录）' }}</dd>
        <dt>耗时</dt>
        <dd class="mono">{{ selectedEntry.elapsed || '—' }}</dd>
        <dt>结论</dt>
        <dd :class="passTone(selectedEntry)">
          {{ verdictSummary(selectedEntry) || '未知（升级前的旧目录，打开报告可看）' }}
        </dd>
        <dt>修改时间</dt>
        <dd>{{ selectedEntry.modified || '时间未知' }}</dd>
        <dt>目录大小</dt>
        <dd class="mono">{{ size(selectedEntry.bytes) }}</dd>
        <dt>HTML 报告</dt>
        <dd>{{ selectedEntry.has_report ? '有' : '没有' }}</dd>
        <dt>Excel</dt>
        <dd>{{ selectedEntry.has_xlsx ? '有' : '没有' }}</dd>
        <dt>结果明细</dt>
        <dd>{{ selectedEntry.has_rows ? '有（可重新生成报告）' : '没有（不能恢复报告）' }}</dd>
        <dt>原计划</dt>
        <dd>{{ selectedEntry.has_request ? '有（可装载重跑）' : '没有（不能重跑）' }}</dd>
      </dl>
      <p class="muted small">
        「开始时间」和「结论」来自这一轮的 <code>meta.json</code>，与报告顶部那八个格子同源。
        「修改时间」则是目录最后被写过的时刻，<strong>不是测试开始时间</strong>——恢复一次报告它就会变。
        被测设备型号这一页还没有，要看得打开报告。
      </p>
    </div>

    <div v-if="entries.length && shownEntries.length === 0" class="empty" role="status">
      没有运行记录匹配「{{ ui.runs.query.trim() }}」。
      <button type="button" class="linklike" @click="ui.runs.query = ''">清空搜索</button>
      可以看到全部 {{ entries.length }} 条。
    </div>

    <div v-else-if="entries.length" class="scroll" tabindex="0" role="region" aria-label="历史运行记录，可横向滚动">
      <table :aria-busy="loading || !!busy">
        <thead>
          <tr>
            <th scope="col">运行 / 时间</th><th scope="col">结论</th><th scope="col">产物</th><th scope="col" class="num">大小</th><th scope="col">操作</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="entry in shownEntries"
            :key="entry.id"
            :class="{ working: busy === entry.id, picked: entry.id === ui.runs.selected }"
          >
            <td class="run-identity">
              <button
                type="button"
                class="pick mono"
                :aria-pressed="entry.id === ui.runs.selected"
                @click="pickEntry(entry.id)"
              >
                {{ entry.id }}
              </button>
              <!-- 优先显示真正的开始时间；只有旧目录才退回目录修改时刻，
                   并明说那是修改时间，免得被当成开跑时间读。 -->
              <span class="muted">{{ entry.started || `${entry.modified || '时间未知'}（修改时间）` }}</span>
              <span v-if="busy === entry.id" class="working-label" role="status">正在处理…</span>
            </td>
            <td class="verdicts">
              <span v-if="verdictSummary(entry)" :class="passTone(entry)">{{ verdictSummary(entry) }}</span>
              <span v-else class="muted">—</span>
            </td>
            <td>
              <div class="artifacts">
                <span v-if="entry.has_report" class="chip ready">报告</span>
                <span v-if="entry.has_xlsx" class="chip">Excel</span>
                <span
                  v-if="entry.has_rows"
                  class="chip"
                  title="已有逐单元结果明细，可用来重新生成报告"
                >结果明细</span>
                <span v-if="!entry.has_report && !entry.has_rows" class="chip warn">仅日志</span>
              </div>
            </td>
            <td class="num mono">{{ size(entry.bytes) }}</td>
            <td>
              <div class="actions">
                <a class="dl" :href="bundleUrl(entry.id)" :download="`${entry.id}.zip`">下载包</a>
                <button
                  v-if="entry.has_rows"
                  type="button"
                  class="ghost small"
                  :disabled="!!busy"
                  :title="entry.has_report ? '用已保存的结果明细重新生成报告，覆盖现有报告与 Excel' : '用已保存的结果明细恢复这一轮报告'"
                  @click="replay(entry)"
                >{{ entry.has_report ? '重新生成报告' : '恢复报告' }}</button>
                <button
                  v-if="entry.has_request"
                  type="button"
                  class="ghost small"
                  :disabled="!!busy"
                  title="载入这一轮的计划并跳到「执行」页预览，确认后再开始测试"
                  @click="rerun(entry)"
                >重新执行</button>
                <button
                  v-if="entry.has_rows"
                  type="button"
                  class="ghost small"
                  :class="{ 'is-baseline': baselineId === entry.id }"
                  :aria-pressed="baselineId === entry.id"
                  :disabled="!!busy"
                  title="把这一轮设为对比基线，然后在另一轮上点「与基线对比」"
                  @click="toggleBaseline(entry.id)"
                >{{ baselineId === entry.id ? '基线 ✓' : '设为基线' }}</button>
                <button
                  v-if="entry.has_rows && baselineId && baselineId !== entry.id"
                  type="button"
                  class="ghost small"
                  :disabled="!!busy"
                  title="按单元对齐，列出判定变坏 / 速率下降 / 转好 / 新增 / 缺失"
                  @click="compare(entry)"
                >与基线对比</button>
              </div>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <p v-if="entries.length" class="download-hint muted">「下载包」包含这一轮的全部产物，远程访问时也可以取回本地。</p>

    <details class="help">
      <summary><strong>报告恢复与重新执行说明</strong></summary>
      <dl class="legend">
        <dt>结果明细与报告恢复</dt>
        <dd>
          每个单元完成后都会保存结果明细。即使主控崩溃或断电，已有明细仍可通过「恢复报告」生成报告。
          已有报告时，可用「重新生成报告」更新报告与 Excel。
        </dd>
        <dt>重新执行</dt>
        <dd>
          有完整计划的记录会显示「重新执行」。点击后载入该轮计划，并前往「执行」页预览；
          检查当前网口与单元列表后，再点击「开始测试」。
        </dd>
        <dt>测试被中断后</dt>
        <dd>
          先恢复报告，查看已完成的结果，再启用 RESUME 并「重新执行」。
          24 小时内已 PASS 的单元会被跳过，被中断的单元会从头重跑。
        </dd>
      </dl>
    </details>
  </section>
</template>

<style scoped>
.bar { margin: 0 0 16px; padding: 16px 18px; border: 1px solid var(--line); border-radius: 8px; background: var(--panel-2); }
.list-heading { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
.list-heading > strong { display: flex; align-items: center; gap: 10px; font-size: 14px; }
.count { padding: 1px 7px; border: 1px solid var(--line); border-radius: 4px; background: var(--surface); color: var(--muted); font-size: 12px; font-variant-numeric: tabular-nums; }
.switch { display: flex; align-items: flex-start; gap: 9px; margin-top: 14px; padding-top: 14px; border-top: 1px solid var(--line); font-size: 13px; cursor: pointer; }
.switch input { margin: 4px 0 0; accent-color: var(--accent); }
.switch strong { font-weight: 500; }
.switch small { margin-left: 12px; font-size: 12px; color: var(--muted); }
.scroll { max-width: 100%; overflow-x: auto; border: 1px solid var(--line); border-radius: 7px; background: var(--surface); }
table { width: 100%; min-width: 700px; border-collapse: separate; border-spacing: 0; font-size: 13px; }
th, td { padding: 14px; text-align: left; border-bottom: 1px solid var(--line); }
thead th { padding-block: 10px; background: var(--head); font-size: 11.5px; color: var(--muted); white-space: nowrap; }
tbody tr:last-child td { border-bottom: 0; }
tbody tr:hover, tbody tr.working { background: var(--panel-2); }
.run-identity { min-width: 240px; }
.run-identity > strong { display: block; font-size: 12px; font-weight: 600; overflow-wrap: anywhere; }
.run-identity > span { display: block; margin-top: 4px; font-size: 11.5px; }
.working-label { color: var(--accent); }
.num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
.artifacts { display: flex; flex-wrap: wrap; gap: 5px; min-width: 120px; }
/* 结论那一列：全过是绿的、有未达标是红的，读不到 meta 不上色。
   「读不到」和「零失败」在这里必须分得开。 */
.verdicts { white-space: nowrap; font-size: 12px; }
.is-baseline { border-color: var(--accent); color: var(--accent); font-weight: 600; }
.ok { color: var(--ok); }
.bad { color: var(--bad); }
.actions { display: flex; align-items: center; gap: 7px; flex-wrap: wrap; min-width: 180px; }
.runs-bar { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; margin: 0 0 12px; }
.search { flex: 1 1 240px; max-width: 360px; }
.search input { width: 100%; padding: 8px 11px; border: 1px solid var(--line); border-radius: 6px; background: var(--surface); color: var(--ink); font: inherit; }
button.small { min-height: 32px; padding: 6px 11px; font-size: 12.5px; }
.linklike { padding: 0; min-height: 0; font: inherit; color: var(--accent); background: none; border: 0; text-decoration: underline; text-underline-offset: 3px; }
.linklike:hover:not(:disabled) { background: none; color: var(--accent-hover); }
.run-detail { margin: 0 0 14px; padding: 14px 16px; border: 1px solid var(--line); border-radius: 7px; background: var(--panel-2); }
.run-detail-head { display: flex; align-items: center; justify-content: space-between; gap: 10px; margin-bottom: 10px; }
.run-detail dl { display: grid; grid-template-columns: auto minmax(0, 1fr); gap: 6px 14px; margin: 0 0 10px; font-size: 12.5px; }
.run-detail dt { color: var(--muted); white-space: nowrap; }
.run-detail dd { margin: 0; overflow-wrap: anywhere; }
.run-detail .small { margin: 0; font-size: 12px; }
tbody tr.picked { background: var(--info-bg); }
.pick { padding: 0; min-height: 0; font: inherit; font-weight: 700; color: var(--accent); background: none; border: 0; text-align: left; text-decoration: underline; text-underline-offset: 3px; }
.pick:hover:not(:disabled) { background: none; color: var(--accent-hover); }
.pick[aria-pressed='true'] { color: var(--ink); text-decoration: none; }
.chip { display: inline-block; padding: 2px 6px; border: 1px solid var(--line); border-radius: 3px; background: var(--panel-2); color: var(--muted); font-size: 11px; white-space: nowrap; }
.chip.ready { border-color: transparent; background: var(--ok-bg); color: var(--ok); }
.chip.warn { border-color: transparent; background: var(--info-bg); color: var(--warn); }
.dl { display: inline-flex; align-items: center; justify-content: center; min-height: 33px; padding: 5px 10px; border: 1px solid var(--accent); border-radius: 5px; color: var(--accent); font-size: 12px; font-weight: 600; text-decoration: none; white-space: nowrap; }
.dl:hover { background: var(--info-bg); }
.ghost { padding: 7px 14px; border: 1px solid var(--line); border-radius: 5px; background: var(--surface); color: var(--ink); font: inherit; cursor: pointer; }
.ghost.small { min-height: 33px; padding: 5px 10px; font-size: 12px; white-space: nowrap; }
.ghost:disabled { opacity: .55; cursor: not-allowed; }
.empty { padding: 30px 24px; border: 1px dashed var(--line); border-radius: 7px; color: var(--muted); background: var(--panel-2); }
.empty strong { display: block; font-size: 15px; color: var(--ink); }
.empty p { margin: 6px 0 16px; max-width: 60ch; font-size: 13px; }
.bad { margin: 0 0 12px; padding: 10px 13px; border-left: 3px solid var(--bad); background: var(--bad-bg); overflow-wrap: anywhere; }
.notice { margin: 0 0 12px; padding: 10px 13px; border-left: 3px solid var(--ok); background: var(--ok-bg); overflow-wrap: anywhere; }
.download-hint { margin: 10px 0 0; font-size: 12px; }
.help { margin-top: 24px; border-top: 1px solid var(--line); }
.help > summary { padding: 15px 0; font-size: 13px; cursor: pointer; }
.legend { margin: 0; padding: 0 0 8px 18px; font-size: 12.5px; max-width: 80ch; }
.legend dt { margin: 12px 0 4px; font-weight: 700; }
.legend dt:first-child { margin-top: 0; }
.legend dd { margin: 0; color: var(--muted); line-height: 1.7; }
.mono { font-family: var(--fm); }
@media (max-width: 600px) {
  .bar { padding: 14px; }
  .switch small { display: block; margin: 4px 0 0; }
  .actions { gap: 6px; }
}
</style>
