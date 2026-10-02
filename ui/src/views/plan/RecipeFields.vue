<script setup lang="ts">
import { computed } from 'vue';
import { formatNumberList, formatTokenList, parseNumberList, parseTokenList } from '../../domain/globals';
import {
  axisExpansionIsExact,
  deleteRecipe,
  profilesToAxes,
  recipeIsAxisEditable,
  recipeReferences,
  updateRecipe,
  type UiRecipe,
} from '../../domain/plan-build';
import { plan } from '../../state/plan';

/**
 * 一条 TCP/UDP 流量配置的参数，在任务里**就地**展开编辑。
 *
 * 档位是**轴**，逐档各跑一轮：`-w 4m,64k` × `-P 1,10` 是四个测试单元。
 *
 * 配置是**共享**的：同一条可以被多个任务引用，改它会同时改变所有引用它的任务。
 * 所以顶部把引用它的任务逐条列出来——影响面得看得见。
 *
 * PING 没有配置：服务端明确拒绝带配置引用的 ping 任务，次数和包长直接填在任务上。
 */
const props = defineProps<{ protocol: 'tcp' | 'udp'; recipeId: string }>();
const emit = defineEmits<{ close: [] }>();

const recipe = computed<UiRecipe | undefined>(() =>
  plan.ui.recipes[props.protocol].find((item) => item.id === props.recipeId),
);
const references = computed(() => recipeReferences(plan.ui, props.recipeId));

function patch(value: Partial<UiRecipe>): void {
  plan.ui = updateRecipe(plan.ui, props.protocol, props.recipeId, value);
}
function onTokens(field: 'tcp_windows' | 'bandwidths' | 'lengths' | 'windows', event: Event): void {
  patch({ [field]: parseTokenList((event.target as HTMLInputElement).value) });
}
function onNumbers(field: 'tcp_streams' | 'udp_streams', event: Event): void {
  patch({ [field]: parseNumberList((event.target as HTMLInputElement).value) });
}
function onDelete(): void {
  plan.ui = deleteRecipe(plan.ui, props.protocol, props.recipeId);
  emit('close');
}
/** 把固定组合摊成可编辑的轴。多于一条时会变成叉积——按钮上写清楚了。 */
function onExpand(): void {
  if (recipe.value) patch(profilesToAxes(recipe.value, props.protocol));
}
const tokens = (values: string[] | undefined) => formatTokenList(values ?? []);
const numbers = (values: number[] | undefined) => formatNumberList(values ?? []);
</script>

<template>
  <div v-if="recipe" class="recipe-fields">
    <div class="head">
      <label class="field name">
        <span>配置名称</span>
        <input type="text" :value="recipe.name" @input="patch({ name: ($event.target as HTMLInputElement).value })" />
      </label>
      <button type="button" class="ghost small danger" @click="onDelete">删除配置</button>
    </div>
    <p v-if="references.length > 1" class="shared">
      共享：改动同时作用于
      <span v-for="ref in references" :key="`${ref.suiteId}/${ref.taskId}`" class="chip">{{ ref.suite }} / {{ ref.task }}</span>
    </p>

    <template v-if="recipeIsAxisEditable(recipe)">
      <div v-if="protocol === 'tcp'" class="grid">
        <label class="field">
          <span>套接字缓冲区 <code>-w</code></span>
          <input type="text" placeholder="留空 = iperf3 默认" :value="tokens(recipe.tcp_windows)" @input="onTokens('tcp_windows', $event)" />
        </label>
        <label class="field">
          <span>并发流 <code>-P</code></span>
          <input type="text" placeholder="留空 = 单流" :value="numbers(recipe.tcp_streams)" @input="onNumbers('tcp_streams', $event)" />
        </label>
      </div>
      <div v-else class="grid">
        <label class="field">
          <span>单流带宽 <code>-b</code></span>
          <input type="text" placeholder="必填，如 2500m" :value="tokens(recipe.bandwidths)" @input="onTokens('bandwidths', $event)" />
        </label>
        <label class="field">
          <span>报文长度 <code>-l</code></span>
          <input type="text" placeholder="留空 = 不下发" :value="tokens(recipe.lengths)" @input="onTokens('lengths', $event)" />
        </label>
        <label class="field">
          <span>套接字缓冲区 <code>-w</code></span>
          <input type="text" placeholder="留空 = 不下发" :value="tokens(recipe.windows)" @input="onTokens('windows', $event)" />
        </label>
        <label class="field">
          <span>并发流</span>
          <input type="text" placeholder="留空 = 单流" :value="numbers(recipe.udp_streams)" @input="onNumbers('udp_streams', $event)" />
        </label>
      </div>
      <p class="hint">多个档位用逗号分隔，每种组合各跑一个单元（如 <code>4m, 64k</code> × <code>1, 10</code> = 4 个）。</p>
    </template>

    <div v-else class="frozen">
      <ul class="mono">
        <li v-for="(profile, i) in recipe.profiles" :key="i">
          <template v-if="profile.bandwidth">-b {{ profile.bandwidth }} </template>
          <template v-if="profile.length">-l {{ profile.length }} </template>
          <template v-if="profile.window">-w {{ profile.window }} </template>
          <template v-if="profile.streams">×{{ profile.streams }} 流</template>
        </li>
      </ul>
      <button type="button" class="ghost small" @click="onExpand">
        转成可编辑档位{{ axisExpansionIsExact(recipe) ? '' : '（会展开为叉积，单元数变多）' }}
      </button>
    </div>
  </div>
</template>

<style scoped>
.recipe-fields {
  margin: 8px 0 4px 26px; padding: 12px 14px;
  border: 1px solid var(--line); border-left: 3px solid var(--accent); border-radius: 6px;
  background: var(--panel-2);
}
.head { display: flex; align-items: end; gap: 8px; flex-wrap: wrap; }
.head .name { flex: 1 1 200px; }
.head input { width: 100%; }
.shared { margin: 10px 0 0; font-size: 12.5px; color: var(--warn); }
.chip {
  display: inline-block; margin: 2px 4px 0 0; padding: 1px 8px;
  border: 1px solid var(--line); border-radius: 10px; background: var(--surface); color: var(--ink);
}
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(170px, 1fr)); gap: 10px; margin-top: 12px; }
.grid input { width: 100%; }
.hint { margin: 8px 0 0; }
.frozen ul { margin: 10px 0; padding-left: 18px; font-size: 12.5px; }
</style>
