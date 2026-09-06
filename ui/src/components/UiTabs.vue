<script setup lang="ts">
import { ref } from 'vue';

/**
 * 真正的 WAI-ARIA tablist：方向键在标签间移动焦点，Enter / 空格 / 点击才切换面板。
 *
 * # 为什么不是一排 `aria-pressed` 按钮
 *
 * 这一版之前是三个 `<button :aria-pressed>`。屏幕上看着一样，读屏和键盘上不是：
 * 按钮组没有「这是一组、共 3 个、当前第 1 个」这层关系，方向键也不动——用户得
 * 一路 Tab 过去。方案 §7 那条「方向键操作真正的 Tabs，普通按钮组不伪装 listbox」
 * 说的就是这件事。
 *
 * # 为什么方向键**不**顺手切换面板（手动激活）
 *
 * 这三块面板一块比一块重（分配矩阵、套件编辑器、流量配置）。自动激活时，用户
 * 从第一个按方向键找到第三个，中间那块会被完整挂载再卸载一次——在一台正在灌
 * 线速的机器上这不是"顺手"。所以焦点与选中分离：方向键只移动焦点，Enter / 空格
 * 才切。这也是 WAI-ARIA 对"面板昂贵"场景给的建议。
 *
 * # 为什么自己写而不是接组件库
 *
 * 实测过：接 Reka UI 的 Tabs 只为这一个控件，单文件产物从 254,791 涨到 268,527
 * 字节（+13.7KB / +5.4%），而这份产物是要 `include_str!` 进单个 exe 的。行为上它
 * 是对的（CSP 下能跑、role 与方向键都正确），但那个体积换来的只是这五十行。
 * 等 Combobox / Dialog / Menu 这类真正难写的（焦点陷阱、portal、typeahead）
 * 攒够了再谈——那时候库才划算。判断依据记在 `.ai/DESIGN-frontend-workbench.md`
 * 的 P2 记录里。
 *
 * `components/` 不读 state、不发请求（`lint-arch.mjs` 挡着）：props 进、emits 出。
 */

export interface UiTab {
  id: string;
  label: string;
  /** 标签下面那行小字，例如「2 个任务」。 */
  hint?: string;
}

const props = defineProps<{
  tabs: UiTab[];
  modelValue: string;
  /** 给读屏用的组名，例如「计划编辑区域」。 */
  label: string;
  /** 面板 id 前缀：`aria-controls` 指向 `${panelPrefix}-${tab.id}`。 */
  panelPrefix: string;
}>();
const emit = defineEmits<{ 'update:modelValue': [value: string] }>();

const buttons = ref<HTMLButtonElement[]>([]);

/** 焦点落在哪个标签上——和"选中哪个"是两件事，见文件头。 */
function focusAt(index: number): void {
  const count = props.tabs.length;
  if (!count) return;
  // 环形：最后一个按右键回到第一个。到头就停会让人以为键盘坏了。
  const next = ((index % count) + count) % count;
  buttons.value[next]?.focus();
}

function onKeydown(event: KeyboardEvent, index: number): void {
  const keys: Record<string, () => void> = {
    ArrowRight: () => focusAt(index + 1),
    ArrowLeft: () => focusAt(index - 1),
    Home: () => focusAt(0),
    End: () => focusAt(props.tabs.length - 1),
  };
  const handler = keys[event.key];
  if (!handler) return;
  event.preventDefault();
  handler();
}
</script>

<template>
  <div class="tabs" role="tablist" :aria-label="label">
    <button
      v-for="(tab, index) in tabs"
      :key="tab.id"
      ref="buttons"
      type="button"
      role="tab"
      :id="`${panelPrefix}-tab-${tab.id}`"
      :aria-selected="tab.id === modelValue"
      :aria-controls="`${panelPrefix}-${tab.id}`"
      :class="{ on: tab.id === modelValue }"
      :tabindex="tab.id === modelValue ? 0 : -1"
      @click="emit('update:modelValue', tab.id)"
      @keydown="onKeydown($event, index)"
    >
      <span>{{ tab.label }}</span>
      <small v-if="tab.hint">{{ tab.hint }}</small>
    </button>
  </div>
</template>
