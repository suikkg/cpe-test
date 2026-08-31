<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import {
  formatNumberList,
  formatTokenList,
  parseNumberList,
  parseTokenList,
} from '../../domain/globals';
import {
  addRecipe,
  axisExpansionIsExact,
  deleteRecipe,
  profilesToAxes,
  recipeIsAxisEditable,
  recipeSummary,
  updateRecipe,
  type UiRecipe,
} from '../../domain/plan-build';
import { plan } from '../../state/plan';

const props = defineProps<{ focusRecipeId?: string }>();

/**
 * 「流量配置」：左边一列配置，右边只编辑选中的那一条。
 *
 * 档位是**轴**，逐档各跑一轮：`-w 4m,64k` × `-P 1,10` 是四个测试单元。
 * 所以每加一个档位，单元数和总耗时都会跟着涨——预览页的数字才是准的。
 *
 * # 一条配置是**共享**的
 *
 * 同一条配置可以被多个任务引用，改它会**同时改变所有引用它的任务**。这是有意的：
 * 「同一对网口既按常规档位跑一遍、又用 1m 单流跑一遍」靠的就是两条配置各被引用
 * 一次。但共享的代价是「我只想改这一个任务」会波及别处，所以右边把引用它的任务
 * **逐条列出来**——影响面得看得见，而不是改完预览时才发现单元数不对。
 *
 * PING 没有配置卡片：服务端明确拒绝带配置引用的 ping 任务（`UiRecipe` 上没有任何
 * ping 语义，留着引用会让人以为它可配置，而参数其实被静默忽略）。
 * ping 的次数和包长直接填在任务上。
 */

type Bucket = 'tcp' | 'udp';

interface Entry {
  protocol: Bucket;
  recipe: UiRecipe;
}

const entries = computed<Entry[]>(() => [
  ...plan.ui.recipes.tcp.map((recipe) => ({ protocol: 'tcp' as const, recipe })),
  ...plan.ui.recipes.udp.map((recipe) => ({ protocol: 'udp' as const, recipe })),
]);

/** 存 id 不存下标：删掉一条之后下标会指向另一条，看起来像是删错了。 */
const selectedId = ref('');
watch(
  () => props.focusRecipeId,
  (id) => {
    if (id) selectedId.value = id;
  },
  { immediate: true },
);
const current = computed<Entry | undefined>(
  () => entries.value.find((entry) => entry.recipe.id === selectedId.value) ?? entries.value[0],
);

/** 引用这条配置的任务，带上它所在的套件——影响面要能点名，不能只给个数字。 */
function referencedBy(recipeId: string): Array<{ suite: string; task: string }> {
  const out: Array<{ suite: string; task: string }> = [];
  for (const suite of plan.ui.suites) {
    for (const task of suite.tasks) {
      if (task.recipe_ids.includes(recipeId)) {
        out.push({ suite: suite.name || '(未命名套件)', task: task.name || '(未命名任务)' });
      }
    }
  }
  return out;
}

function onAdd(protocol: Bucket): void {
  plan.ui = addRecipe(plan.ui, protocol);
  const list = plan.ui.recipes[protocol];
  selectedId.value = list[list.length - 1].id;
}

function onDelete(protocol: Bucket, recipeId: string): void {
  plan.ui = deleteRecipe(plan.ui, protocol, recipeId);
  selectedId.value = entries.value[0]?.recipe.id ?? '';
}

function onName(protocol: Bucket, recipeId: string, event: Event): void {
  plan.ui = updateRecipe(plan.ui, protocol, recipeId, {
    name: (event.target as HTMLInputElement).value,
  });
}

function onTokens(
  protocol: Bucket,
  recipeId: string,
  field: 'tcp_windows' | 'bandwidths' | 'lengths' | 'windows',
  event: Event,
): void {
  plan.ui = updateRecipe(plan.ui, protocol, recipeId, {
    [field]: parseTokenList((event.target as HTMLInputElement).value),
  });
}

function onNumbers(
  protocol: Bucket,
  recipeId: string,
  field: 'tcp_streams' | 'udp_streams',
  event: Event,
): void {
  plan.ui = updateRecipe(plan.ui, protocol, recipeId, {
    [field]: parseNumberList((event.target as HTMLInputElement).value),
  });
}

/** 把固定组合摊成可编辑的轴。多于一条时会变成叉积——按钮上写清楚了。 */
function onExpand(protocol: Bucket, recipe: UiRecipe): void {
  plan.ui = updateRecipe(plan.ui, protocol, recipe.id, profilesToAxes(recipe, protocol));
}

function tokens(values: string[] | undefined): string {
  return formatTokenList(values ?? []);
}

function numbers(values: number[] | undefined): string {
  return formatNumberList(values ?? []);
}
</script>

<template>
  <div class="split">
    <!-- 左：配置列表，按协议分段 -->
    <div class="list" role="listbox" aria-label="流量配置">
      <template v-for="bucket in (['tcp', 'udp'] as Bucket[])" :key="bucket">
        <div class="list-group">{{ bucket.toUpperCase() }}</div>
        <button
          v-for="recipe in plan.ui.recipes[bucket]"
          :key="recipe.id"
          type="button"
          role="option"
          class="list-item"
          :class="{ on: current?.recipe.id === recipe.id }"
          :aria-selected="current?.recipe.id === recipe.id"
          @click="selectedId = recipe.id"
        >
          <span class="list-name">{{ recipe.name || '(未命名)' }}</span>
          <span class="list-meta mono">{{ recipeSummary(recipe, bucket) }}</span>
          <span class="list-meta">被 {{ referencedBy(recipe.id).length }} 个任务引用</span>
        </button>
        <button type="button" class="ghost add" @click="onAdd(bucket)">
          + {{ bucket.toUpperCase() }} 配置
        </button>
      </template>
    </div>

    <!-- 右：编辑选中的那一条 -->
    <div v-if="current" class="detail">
      <div class="detail-head">
        <span class="proto mono">{{ current.protocol.toUpperCase() }}</span>
        <input
          class="name"
          type="text"
          :value="current.recipe.name"
          aria-label="配置名称"
          @input="onName(current.protocol, current.recipe.id, $event)"
        />
        <button
          type="button"
          class="ghost small danger"
          :title="
            referencedBy(current.recipe.id).length > 0
              ? '删除后，引用它的任务会自动去掉这条引用'
              : '删除这条配置'
          "
          @click="onDelete(current.protocol, current.recipe.id)"
        >
          删除
        </button>
      </div>

      <!-- 影响面：共享是有意的，但得看得见 -->
      <p v-if="referencedBy(current.recipe.id).length" class="impact">
        <strong>改它会同时影响这 {{ referencedBy(current.recipe.id).length }} 个任务：</strong>
        <span v-for="(ref, i) in referencedBy(current.recipe.id)" :key="i" class="chip">
          {{ ref.suite }} / {{ ref.task }}
        </span>
      </p>
      <p v-else class="muted impact-none">
        还没有任务引用它。到上面的套件里勾上，或者它不会产生任何单元。
      </p>

      <template v-if="recipeIsAxisEditable(current.recipe)">
        <div v-if="current.protocol === 'tcp'" class="fields">
          <label>
            <span>socket buffer <code>-w</code></span>
            <input
              type="text"
              placeholder="留空 = 用 iperf3 默认窗口"
              :value="tokens(current.recipe.tcp_windows)"
              @input="onTokens('tcp', current.recipe.id, 'tcp_windows', $event)"
            />
          </label>
          <label>
            <span>并发流 <code>-P</code></span>
            <input
              type="text"
              placeholder="留空 = 单流"
              :value="numbers(current.recipe.tcp_streams)"
              @input="onNumbers('tcp', current.recipe.id, 'tcp_streams', $event)"
            />
          </label>
        </div>
        <div v-else class="fields">
          <label>
            <span>单流带宽 <code>-b</code></span>
            <input
              type="text"
              placeholder="必填，如 2500m"
              :value="tokens(current.recipe.bandwidths)"
              @input="onTokens('udp', current.recipe.id, 'bandwidths', $event)"
            />
          </label>
          <label>
            <span>报文长度 <code>-l</code></span>
            <input
              type="text"
              placeholder="留空 = 不下发 -l"
              :value="tokens(current.recipe.lengths)"
              @input="onTokens('udp', current.recipe.id, 'lengths', $event)"
            />
          </label>
          <label>
            <span>socket buffer <code>-w</code></span>
            <input
              type="text"
              placeholder="留空 = 不下发 -w"
              :value="tokens(current.recipe.windows)"
              @input="onTokens('udp', current.recipe.id, 'windows', $event)"
            />
          </label>
          <label>
            <span>并发流</span>
            <input
              type="text"
              placeholder="留空 = 单流"
              :value="numbers(current.recipe.udp_streams)"
              @input="onNumbers('udp', current.recipe.id, 'udp_streams', $event)"
            />
          </label>
        </div>
        <p class="muted hint">
          档位是<strong>轴</strong>，逐档各跑一轮：填 <code>4m, 64k</code> 就是两档，
          再配 <code>-P 1, 10</code> 就是 2×2 = 四个测试单元。
        </p>
      </template>

      <div v-else class="frozen">
        <p class="muted">
          这条配置用的是「固定组合」（{{ current.recipe.profiles.length }} 条），
          服务端在有固定组合时不看下面的档位——所以这里不给输入框，免得改了不生效。
        </p>
        <ul class="mono">
          <li v-for="(profile, i) in current.recipe.profiles" :key="i">
            <template v-if="profile.bandwidth">-b {{ profile.bandwidth }} </template>
            <template v-if="profile.length">-l {{ profile.length }} </template>
            <template v-if="profile.window">-w {{ profile.window }} </template>
            <template v-if="profile.streams">×{{ profile.streams }} 流</template>
          </li>
        </ul>
        <button type="button" class="ghost small" @click="onExpand(current.protocol, current.recipe)">
          转成可编辑档位{{ axisExpansionIsExact(current.recipe) ? '' : '（会摊成叉积，单元数变多）' }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.split { display: grid; grid-template-columns: 216px minmax(0, 1fr); gap: 12px; align-items: start; }
.list {
  display: flex; flex-direction: column; gap: 4px;
  max-height: min(480px, calc(100vh - 300px));
  overflow-y: auto; overscroll-behavior: contain; padding-right: 4px;
  scrollbar-gutter: stable;
}
.list-group {
  margin: 6px 0 2px; padding-left: 4px;
  font: 600 10.5px/1 var(--fm); letter-spacing: .18em; color: var(--muted);
}
.list-item {
  display: flex; flex-direction: column; gap: 2px;
  padding: 7px 9px; text-align: left;
  border: 1px solid transparent; border-radius: 5px;
  background: transparent; color: var(--ink); font: inherit; cursor: pointer;
}
.list-item:hover { background: var(--head); }
.list-item.on {
  background: var(--surface); border-color: var(--line);
  box-shadow: inset 3px 0 0 var(--accent); font-weight: 600;
}
.list-name { font-size: 13px; overflow-wrap: anywhere; }
.list-meta { font-size: 11px; color: var(--muted); font-weight: 400; overflow-wrap: anywhere; }
.add { margin-bottom: 2px; }

.detail {
  min-width: 0; padding: 11px 12px;
  border: 1px solid var(--line); border-radius: 6px; background: var(--surface);
}
.detail-head { display: flex; align-items: center; gap: 9px; flex-wrap: wrap; }
.detail-head .name { flex: 1 1 170px; font-weight: 700; }
.proto {
  flex: 0 0 auto; padding: 2px 7px; border-radius: 3px;
  background: var(--panel-2); color: var(--muted); font-size: 11px;
}
.impact {
  margin: 9px 0 0; padding: 7px 10px; font-size: 12px;
  border-left: 3px solid var(--focus); background: var(--info-bg);
}
.impact-none { margin: 9px 0 0; font-size: 12px; }
.chip {
  display: inline-block; margin: 2px 4px 0 0; padding: 1px 6px;
  border-radius: 3px; background: var(--surface); font-size: 11.5px;
}
.fields {
  display: grid; grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
  gap: 10px; margin: 11px 0 0;
}
label { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
label span { font-size: 11.5px; color: var(--muted); }
input {
  padding: 6px 8px; border: 1px solid var(--line); border-radius: 4px;
  background: var(--surface); color: var(--ink); font: inherit; font-size: 13px; min-width: 0;
  cursor: text; box-shadow: inset 0 0 0 1px var(--bezel-hi);
}
input:hover { border-color: var(--accent); }
input:focus-visible { outline: 2px solid var(--focus); outline-offset: 1px; }
.hint { margin: 9px 0 0; font-size: 12px; }
.frozen { margin: 11px 0 0; font-size: 12.5px; }
.frozen ul { margin: 6px 0 8px; padding-left: 20px; }
.ghost {
  padding: 5px 12px; border: 1px solid var(--line); border-radius: 4px;
  background: var(--surface); color: var(--ink); font: inherit; font-size: 12.5px; cursor: pointer;
}
.ghost.small { padding: 3px 10px; font-size: 12px; }
.ghost.danger { border-color: var(--bad); color: var(--bad); }
.muted { color: var(--muted); }
.mono { font-family: var(--fm); }
code { font-family: var(--fm); }
@media (max-width: 860px) {
  .split { grid-template-columns: minmax(0, 1fr); }
  .list { max-height: 220px; }
}
</style>
