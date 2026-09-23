<script setup lang="ts">
import { computed, ref } from 'vue';
import { inner } from '../../state/inner';
import { INNER_MEASUREMENTS, MEASUREMENT_HINT, MEASUREMENT_LABEL, innerNicIpv6 } from '../../domain/inner';
import type { InnerLink } from '../../domain/inner';
import { innerBoardIpv6Choices, innerEndpointKey, innerNicChoices, innerNicOtherReason, innerSetupIssues, suggestedInnerBoardIpv6 } from '../../domain/inner-setup';

const props = defineProps<{ link: InnerLink; hosts: { id: string; label: string }[]; disabled: boolean }>();
const emit = defineEmits<{ (e: 'close'): void }>();

const showOther = ref(false);
/** 已配置的其他网段保持可见，不能把旧配置悄悄换成第一块网卡。 */
const nics = computed(() => {
  return innerNicChoices(inner.capability, true).filter((choice) => choice.host === props.link.host
    && (showOther.value || !innerNicOtherReason(choice.nic) || choice.key === selectedKey.value));
});
const selectedKey = computed(() => innerEndpointKey(props.link.host, props.link.local_interface, props.link.local_ip, props.link.local_ipv6));
const v4 = computed(() => inner.config.ip_versions.includes(4));
const v6 = computed(() => inner.config.ip_versions.includes(6));
const boardIpv6 = computed(() => innerBoardIpv6Choices(inner.capability));
const nicIpv6 = computed(() => [...new Set(innerNicChoices(inner.capability, true)
  .filter((choice) => choice.host === props.link.host && choice.nic.name === props.link.local_interface)
  .flatMap((choice) => [choice.nic.ipv6_ll, choice.nic.ipv6_global]).filter(Boolean))]);
const linkIssues = computed(() => innerSetupIssues(inner.config).filter((issue) => issue.linkIndex === inner.config.links.indexOf(props.link)));
const usesTool = computed(() => props.link.measurement !== 'nic_strict');
const bidir = computed(() => inner.config.directions.includes('bidir'));

function selectNic(event: Event): void {
  const choice = nics.value.find((item) => item.key === (event.target as HTMLSelectElement).value);
  if (choice) {
    Object.assign(props.link, { local_interface: choice.nic.name, local_ip: choice.nic.ipv4, local_ipv6: innerNicIpv6(choice.nic) || null });
    if (!props.link.gateway_ipv6) props.link.gateway_ipv6 = suggestedInnerBoardIpv6(inner.capability, props.link);
  }
}
function changeHost(): void {
  // 换电脑就清掉网卡和源 IP：它们是上一台机器的身份，留着只会看起来像填好了。
  Object.assign(props.link, { local_interface: '', local_ip: '', local_ipv6: null });
}
</script>

<template>
  <fieldset :disabled="props.disabled" class="detail">
    <legend>编辑「{{ props.link.name }}」</legend>
    <p v-for="issue in linkIssues" :key="issue.message" class="warn">{{ issue.message }}</p>
    <div class="grid">
      <label>网口名称<input v-model="props.link.name" placeholder="ETH / Wi-Fi / RNDIS"></label>
      <label>所在电脑<select v-model="props.link.host" @change="changeHost">
        <option v-for="host in props.hosts" :key="host.id" :value="host.id">{{ host.label }}</option>
      </select></label>
      <label>接到 CPE 的电脑网卡<select :value="nics.some(choice => choice.key === selectedKey) ? selectedKey : ''" @change="selectNic">
        <option value="">{{ nics.length ? '请选择实际接到 CPE 的网卡' : '尚无扫描结果，可重新扫描或手动填写' }}</option>
        <option v-for="choice in nics" :key="choice.key" :value="choice.key">{{ choice.nic.name }} · {{ choice.nic.ipv4 || innerNicIpv6(choice.nic) }}</option>
      </select></label>
      <label v-if="v4">CPE LAN IPv4<input v-model="props.link.gateway" placeholder="如 192.168.0.1，按实际设备填写"><small class="muted">填 CPE 的 LAN 地址，不是电脑 IP，也不是 WAN 默认网关。</small></label>
      <label v-if="v6">电脑网卡 IPv6<input v-model="props.link.local_ipv6" list="inner-pc-ipv6" placeholder="从扫描选择或填写，例如 fe80::1234"><datalist id="inner-pc-ipv6"><option v-for="ip in nicIpv6" :key="ip" :value="ip" /></datalist></label>
      <label v-if="v6">CPE LAN IPv6<input v-model="props.link.gateway_ipv6" list="inner-board-ipv6" placeholder="从板侧扫描选择或按实际地址填写"><datalist id="inner-board-ipv6"><option v-for="choice in boardIpv6" :key="`${choice.name}:${choice.address}`" :value="choice.address">{{ choice.name }}</option></datalist></label>
    </div>
    <p class="identity">电脑网卡：<strong>{{ props.link.local_interface || '未选择' }}</strong><template v-if="v4">　IPv4：<strong>{{ props.link.local_ip || '未填写' }}</strong></template><template v-if="v6">　IPv6：<strong>{{ props.link.local_ipv6 || '未填写' }}</strong></template></p>
    <p v-if="v6" class="muted">优先使用两端链路本地地址（fe80::/10），也可填写两端全局 / ULA 地址。不要填 %接口或 /前缀；程序按实际执行端绑定网口。板侧 IPv6 来自真实扫描，不按 IPv4 推算。</p>
    <label class="other-toggle"><input v-model="showOther" type="checkbox">显示此电脑的其他网段 / 隧道接口</label>
    <details :open="!nics.length" class="advanced">
      <summary>手动填写网卡{{ v4 ? '与 IPv4' : '' }}</summary>
      <p class="muted">只用于扫描未覆盖的接口。网卡名称与 IP 必须真实存在于所选电脑；这里不会修改电脑的网络设置。</p>
      <div class="grid">
        <label>网卡名称<input v-model="props.link.local_interface" placeholder="以太网"></label>
        <label v-if="v4">该电脑网卡 IPv4<input v-model="props.link.local_ip" placeholder="192.168.0.100"></label>
      </div>
    </details>

    <h4>这一口测什么</h4>
    <p class="muted">
      上行（PC → CPE）看板侧桥接口 RX，默认 br0；下行（CPE → PC）看上方所选电脑网卡的 RX。
      双向分别记录这两处接收速率。RNDIS、网线、Wi-Fi 都沿用这个口径，无需配置板侧成员口。
    </p>
    <details class="advanced">
      <summary>高级：统计接口、测量策略与验收门限</summary>
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
    </details>
    <div class="bar"><button @click="emit('close')">完成，返回网口清单</button></div>
  </fieldset>
</template>

<style scoped>
.detail { margin: 0; padding: 16px; border: 1px solid var(--line); border-radius: 6px; background: var(--panel-2); }
.detail legend { font-weight: 700; padding: 0 6px; }
.detail h4 { margin: 16px 0 4px; }
.advanced { margin: 14px 0; padding: 12px 0; border-block: 1px solid var(--line); }
.advanced > summary { cursor: pointer; margin-bottom: 12px; font-size: 13px; }
.identity { font-size: 13px; overflow-wrap: anywhere; }
.other-toggle { display: flex; align-items: center; gap: 8px; font-size: 13px; }
.warn { color: var(--warn); }
.grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px; }
.grid label { display: flex; flex-direction: column; gap: 6px; font-size: 13px; min-width: 0; }
.grid input, .grid select { width: 100%; min-width: 0; }
.muted { color: var(--muted); font-size: 12px; }
@media (max-width: 900px) { .grid { grid-template-columns: 1fr; } }
</style>
