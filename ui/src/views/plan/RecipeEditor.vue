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
    <div class="list" role="group" aria-label="选择流量配置">
      <template v-for="bucket in (['tcp', 'udp'] as Bucket[])" :key="bucket">
        <div class="list-group"><span>{{ bucket.toUpperCase() }} 配置</span><span>{{ plan.ui.recipes[bucket].length }}</span></div>
        <button
          v-for="recipe in plan.ui.recipes[bucket]"
          :key="recipe.id"
          type="button"
          class="list-item"
          :class="{ on: current?.recipe.id === recipe.id }"
          :aria-pressed="current?.recipe.id === recipe.id"
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
        <label class="name-field">
          <span>配置名称</span>
          <input
            class="name"
            type="text"
            :value="current.recipe.name"
            @input="onName(current.protocol, current.recipe.id, $event)"
          />
        </label>
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
        <strong>共享配置：修改会应用到以下 {{ referencedBy(current.recipe.id).length }} 个任务</strong>
        <span v-for="(ref, i) in referencedBy(current.recipe.id)" :key="i" class="chip">
          {{ ref.suite }} / {{ ref.task }}
        </span>
      </p>
      <p v-else class="muted impact-none">
        尚未被任务引用。在「编辑套件」中勾选此配置后，才会用于测试。
      </p>

      <template v-if="recipeIsAxisEditable(current.recipe)">
        <div v-if="current.protocol === 'tcp'" class="fields">
          <label>
            <span>套接字缓冲区 <code>-w</code></span>
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
            <span>套接字缓冲区 <code>-w</code></span>
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
          多个档位用逗号分隔，每种组合各跑一轮。例如缓冲区 <code>4m, 64k</code>
          搭配并发流 <code>1, 10</code>，会展开为 2 × 2 = 4 个测试单元。
        </p>
      </template>

      <div v-else class="frozen">
        <p class="muted">
          此配置包含 {{ current.recipe.profiles.length }} 条固定组合，测试时按以下组合逐条执行。
          需要调整参数时，可先转成可编辑档位。
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
    <div v-else class="empty">
      <strong>添加第一条流量配置</strong>
      <p>选择添加 TCP 或 UDP 配置，设置带宽、缓冲区与并发流档位。</p>
    </div>
  </div>
</template>

<style scoped>
.split { display: grid; grid-template-columns: 236px minmax(0, 1fr); gap: 20px; align-items: start; }
.list {
  display: flex; flex-direction: column; gap: 6px; max-height: min(620px, calc(100vh - 240px));
  overflow-y: auto; overscroll-behavior: contain; padding-right: 6px; scrollbar-gutter: stable;
}
.list-group { display: flex; align-items: center; justify-content: space-between; margin: 10px 0 3px; padding: 0 8px; font-size: 12px; font-weight: 600; color: var(--muted); }
.list-group:first-child { margin-top: 0; }
.list-group > span:last-child { font-size: 11px; font-variant-numeric: tabular-nums; }
.list-item {
  display: flex; flex-direction: column; gap: 5px; padding: 12px; text-align: left;
  border: 1px solid transparent; border-radius: 7px; background: transparent; color: var(--ink); font: inherit; cursor: pointer;
}
.list-item:hover { background: var(--head); }
.list-item.on { background: var(--info-bg); border-color: var(--line); box-shadow: inset 3px 0 0 var(--accent); }
.list-name { font-size: 13px; font-weight: 600; overflow-wrap: anywhere; }
.list-meta { font-size: 11px; line-height: 1.55; color: var(--muted); overflow-wrap: anywhere; }
.add { margin: 1px 0 12px; }
.detail { min-width: 0; padding: 20px; border: 1px solid var(--line); border-radius: 9px; background: var(--surface); }
.detail-head { display: flex; align-items: flex-end; gap: 12px; flex-wrap: wrap; }
.name-field { flex: 1 1 170px; }
.name-field .name { font-weight: 600; font-size: 14px; }
.proto { flex: 0 0 auto; align-self: flex-start; margin-top: 25px; padding: 4px 8px; border-radius: 4px; background: var(--info-bg); color: var(--accent); font-size: 11px; font-weight: 700; }
.impact { margin: 18px 0 0; padding: 12px; font-size: 12px; border-left: 3px solid var(--focus); background: var(--info-bg); border-radius: 0 5px 5px 0; }
.impact strong { display: block; margin-bottom: 6px; font-weight: 600; }
.impact-none { margin: 18px 0 0; padding: 10px 12px; background: var(--panel-2); border-radius: 5px; font-size: 12px; }
.chip { display: inline-block; margin: 3px 5px 0 0; padding: 3px 7px; border-radius: 4px; background: var(--surface); font-size: 11px; overflow-wrap: anywhere; }
.fields { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 18px 16px; margin: 22px 0 0; }
label { display: flex; flex-direction: column; gap: 7px; min-width: 0; }
label > span { font-size: 12px; color: var(--muted); }
input { width: 100%; min-height: 38px; padding: 8px 10px; border: 1px solid var(--line); border-radius: 5px; background: var(--surface); color: var(--ink); font: inherit; font-size: 13px; min-width: 0; cursor: text; }
input:hover { border-color: var(--accent); }
input:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }
.hint { margin: 20px 0 0; padding-top: 14px; border-top: 1px solid var(--line); font-size: 12px; line-height: 1.8; }
.frozen { margin: 18px 0 0; font-size: 12.5px; }
.frozen > p { line-height: 1.7; }
.frozen ul { margin: 12px 0 16px; padding: 12px 12px 12px 30px; border-radius: 5px; background: var(--panel-2); line-height: 1.9; overflow-wrap: anywhere; }
.ghost { min-height: 36px; padding: 7px 12px; border: 1px solid var(--line); border-radius: 5px; background: var(--surface); color: var(--ink); font: inherit; font-size: 12px; cursor: pointer; }
.ghost.small { font-size: 12px; }
.ghost.danger { color: var(--bad); }
.muted { color: var(--muted); }
.mono, code { font-family: var(--fm); }
.empty { padding: 24px; border: 1px dashed var(--line); border-radius: 8px; }
.empty strong { font-size: 14px; }
.empty p { font-size: 13px; color: var(--muted); }
@media (max-width: 1000px) {
  .split { grid-template-columns: 190px minmax(0, 1fr); gap: 14px; }
  .detail { padding: 16px; }
}
@media (max-width: 760px) {
  .split { grid-template-columns: minmax(0, 1fr); }
  .list { max-height: 240px; padding: 8px; border: 1px solid var(--line); border-radius: 7px; background: var(--panel-2); }
  .list-item { background: var(--surface); }
}
@media (max-width: 480px) {
  .fields { grid-template-columns: minmax(0, 1fr); }
  .detail { padding: 14px; }
  .detail-head { gap: 8px; }
  .proto { padding: 4px 6px; }
  .name-field { flex-basis: 130px; }
}
</style>
