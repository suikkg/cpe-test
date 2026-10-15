<script setup lang="ts">
import { computed, ref } from 'vue';
import { masterNics } from '../../state/inventory';
import { quick, prepareQuick, restoreQuick } from '../../state/quick';
import type { UiProtocol } from '../../domain/plan-build';
defineProps<{ disabled: boolean }>();
const first = ref(''); const second = ref(''); const protocol = ref<UiProtocol>('tcp'); const ip = ref<'v4'|'v6'>('v4'); const duration = ref(20); const error = ref('');
const nics = computed(() => masterNics.value);
async function prepare(): Promise<void> {
  try { await prepareQuick(first.value, second.value, protocol.value, duration.value, ip.value); error.value = ''; }
  catch(e) { error.value = e instanceof Error ? e.message : '快捷测试配置无效'; }
}
</script>
<template>
  <section class="panel" aria-labelledby="quick-test-title">
    <h3 id="quick-test-title">本机两口快捷测试</h3>
    <p class="hint">仅所选两个本机网口参与。</p>
    <fieldset :disabled="disabled" class="quick-fields">
      <label class="field">网口 A<select aria-label="网口 A" v-model="first"><option value="">选择网口</option><option v-for="nic in nics" :key="nic.name" :value="nic.name">{{ nic.name }} · {{ nic.ipv4 || nic.ipv6_ll || nic.ipv6_global }}</option></select></label>
      <label class="field">网口 B<select aria-label="网口 B" v-model="second"><option value="">选择网口</option><option v-for="nic in nics" :key="nic.name" :value="nic.name">{{ nic.name }} · {{ nic.ipv4 || nic.ipv6_ll || nic.ipv6_global }}</option></select></label>
      <label class="field">测试内容<select aria-label="测试内容" v-model="protocol"><option value="tcp">TCP</option><option value="udp">UDP</option><option value="ping">Ping</option></select></label>
      <label class="field">IP 版本<select aria-label="IP 版本" v-model="ip"><option value="v4">IPv4</option><option value="v6">IPv6</option></select></label>
      <label class="field">时长 / Ping 次数<input v-model.number="duration" type="number" min="6" max="86400"></label>
      <button type="button" :disabled="!first || !second || first === second" @click="prepare">预览快捷测试</button>
      <button v-if="quick.previous" type="button" class="ghost" @click="restoreQuick">恢复原计划</button>
    </fieldset>
    <p v-if="error" class="msg bad" role="alert">{{ error }}</p>
  </section>
</template>
<style scoped>
.quick-fields { display:flex; flex-wrap:wrap; align-items:end; gap:12px; border:0; padding:0; margin:0; }
</style>
