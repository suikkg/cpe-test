<script setup lang="ts">
import { computed } from 'vue';
import { inner } from '../../state/inner';
import { INNER_MEASUREMENTS, MEASUREMENT_HINT, MEASUREMENT_LABEL } from '../../domain/inner';
import type { InnerLink } from '../../domain/inner';

const props = defineProps<{ link: InnerLink; hosts: { id: string; label: string }[]; disabled: boolean }>();
const emit = defineEmits<{ (e: 'close'): void }>();

/** 只列出该电脑扫描到的、带 IPv4 的网卡；手填仍然允许。 */
const nics = computed(() => {
  const info = props.link.host === 'master'
    ? inner.capability?.local
    : inner.capability?.agents.find((a) => a.id === props.link.host)?.info;
  return info?.interfaces.filter((nic) => nic.ipv4) ?? [];
});
const usesTool = computed(() => props.link.measurement !== 'nic_strict');
const bidir = computed(() => inner.config.directions.includes('bidir'));

function selectNic(event: Event): void {
  const nic = nics.value.find((n) => n.name === (event.target as HTMLSelectElement).value);
  if (nic) Object.assign(props.link, { local_interface: nic.name, local_ip: nic.ipv4 });
}
function changeHost(): void {
  // 换电脑就清掉网卡和源 IP：它们是上一台机器的身份，留着只会看起来像填好了。
  Object.assign(props.link, { local_interface: '', local_ip: '' });
}
</script>

<template>
  <fieldset :disabled="props.disabled" class="detail">
    <legend>编辑「{{ props.link.name }}」</legend>
    <div class="grid">
      <label>网口名称<input v-model="props.link.name" placeholder="ETH / Wi-Fi / RNDIS"></label>
      <label>所在电脑<select v-model="props.link.host" @change="changeHost">
        <option v-for="host in props.hosts" :key="host.id" :value="host.id">{{ host.label }}</option>
      </select></label>
      <label>扫描到的网卡<select :value="props.link.local_interface" @change="selectNic">
        <option value="">选择网卡或在下方手填</option>
        <option v-for="nic in nics" :key="nic.name" :value="nic.name">{{ nic.name }} · {{ nic.ipv4 }}</option>
      </select></label>
      <label>网卡名称<input v-model="props.link.local_interface" placeholder="以太网"></label>
      <label>该电脑网卡 IPv4<input v-model="props.link.local_ip" placeholder="192.168.0.100"></label>
      <label>板侧 LAN 地址<input v-model="props.link.gateway" placeholder="默认 192.168.0.1，可修改"></label>
    </div>

    <h4>接收端统计</h4>
    <p class="muted">
      上行（PC → CPE）看板侧桥接口 RX，默认 br0；下行（CPE → PC）看上方所选电脑网卡的 RX。
      双向分别记录这两处接收速率。RNDIS、网线、Wi-Fi 都沿用这个口径，无需配置板侧成员口。
    </p>
    <div class="grid">
      <label>板侧桥接口<input v-model="props.link.board_rx_interface" placeholder="默认 br0；可手填，留空=自动"></label>
      <label>测量策略<select v-model="props.link.measurement">
        <option v-for="item in INNER_MEASUREMENTS" :key="item" :value="item">{{ MEASUREMENT_LABEL[item] }}</option>
      </select></label>
    </div>
    <p class="muted">桥接口名称变化时可手动修改，例如 br-lan；留空按板侧 LAN 地址归属自动识别。修改随草稿和导出配置保存。</p>
    <p class="muted">{{ MEASUREMENT_HINT[props.link.measurement] }}</p>

    <h4>验收门限</h4>
    <p class="muted">留空只测量。网卡口径和工具口径的门限互相独立，工具速率不会套用网卡门限。</p>
    <div class="grid">
      <label>上行 · 网卡口径 Mbps<input v-model.number="props.link.upload_min_mbps" type="number" min="0.01" step="any" placeholder="留空仅测量"></label>
      <label>下行 · 网卡口径 Mbps<input v-model.number="props.link.download_min_mbps" type="number" min="0.01" step="any" placeholder="留空仅测量"></label>
      <label v-if="bidir">双向合计 · 网卡口径 Mbps<input v-model.number="props.link.bidir_total_min_mbps" type="number" min="0.01" step="any" placeholder="留空=按逐方向门限判定"></label>
      <label v-if="usesTool">上行 · 工具口径 Mbps<input v-model.number="props.link.tool_upload_min_mbps" type="number" min="0.01" step="any" placeholder="留空只标 MEASURED"></label>
      <label v-if="usesTool">下行 · 工具口径 Mbps<input v-model.number="props.link.tool_download_min_mbps" type="number" min="0.01" step="any" placeholder="留空只标 MEASURED"></label>
      <label v-if="usesTool && bidir">双向合计 · 工具口径 Mbps<input v-model.number="props.link.tool_bidir_total_min_mbps" type="number" min="0.01" step="any" placeholder="留空只标 MEASURED"></label>
    </div>
    <p v-if="bidir" class="muted">双向合计门限留空时，两条腿各自按方向门限判定；系统不会拿单向门限除以二当合计门限。</p>
    <div class="bar"><button @click="emit('close')">收起详情</button></div>
  </fieldset>
</template>

<style scoped>
.detail { margin: 0; padding: 16px; border: 1px solid var(--line); border-radius: 6px; background: var(--panel-2); }
.detail legend { font-weight: 700; padding: 0 6px; }
.detail h4 { margin: 16px 0 4px; }
.grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px; }
.grid label { display: flex; flex-direction: column; gap: 6px; font-size: 13px; min-width: 0; }
.grid input, .grid select { width: 100%; min-width: 0; }
.muted { color: var(--muted); font-size: 12px; }
@media (max-width: 900px) { .grid { grid-template-columns: 1fr; } }
</style>
