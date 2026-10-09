<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { emptyInnerParameterOptions, parseInnerParameterTokens } from '../../domain/inner';
import type { InnerConfig, InnerParameterOptions } from '../../domain/inner';
const props = defineProps<{ config: InnerConfig; field: 'tcp_streams' | 'udp_streams' | 'tcp_window' | 'udp_mbps' | 'udp_length'; axis: keyof InnerParameterOptions; numeric: boolean }>();
const formatted = computed(() => {
  const values = props.config.parameter_options?.[props.axis];
  return values?.length ? values.join(', ') : String(props.config[props.field] ?? '');
});
const raw = ref(formatted.value);
const focused = ref(false);
watch(formatted, value => { if (!focused.value) raw.value = value; });
watch(() => props.config, () => { raw.value = formatted.value; });
function input(event: Event): void {
  raw.value = (event.target as HTMLInputElement).value;
  const options = props.config.parameter_options ??= emptyInnerParameterOptions();
  Object.assign(options, { [props.axis]: parseInnerParameterTokens(raw.value, props.numeric) });
  Object.assign(props.config, { [props.field]: null });
}
</script>
<template>
  <input :value="raw" placeholder="多个档位用空格或逗号分隔；留空沿用默认" @focus="focused = true" @blur="focused = false" @input="input">
</template>
