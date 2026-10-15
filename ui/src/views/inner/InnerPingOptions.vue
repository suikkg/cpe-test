<script setup lang="ts">
defineProps<{ options: { count:number; payloads:number[] } }>();
</script>
<template>
  <div class="toolbar"><label class="field">Ping 次数<input v-model.number="options.count" type="number" min="1" max="1000"></label><label class="field">Ping 包长（字节，空格或逗号分隔）<input :value="options.payloads.join(', ')" @change="options.payloads = ($event.target as HTMLInputElement).value.split(/[,，\s]+/).filter(Boolean).map(Number)"></label></div>
</template>
