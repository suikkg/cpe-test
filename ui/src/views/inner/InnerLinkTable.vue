<script setup lang="ts">
import { computed, ref } from 'vue';
import { inner, addInnerLink, addInnerScannedLinks, applyInnerBatch, moveInnerLinkTo, setInnerLinkEnabled } from '../../state/inner';
import type { InnerBatchPatch } from '../../state/inner';
import { INNER_MEASUREMENTS, MEASUREMENT_LABEL, canonicalInnerIpv6, innerNicIpv6 } from '../../domain/inner';
import type { NicInfo } from '../../api/dto';
import type { InnerLink, InnerMeasurement } from '../../domain/inner';
import { innerEndpointKey, innerNicChoices, innerNicOtherReason, innerSetupIssues } from '../../domain/inner-setup';

const props = defineProps<{ hosts: { id: string; label: string }[]; editing: InnerLink | null; disabled: boolean }>();
const emit = defineEmits<{ (e: 'edit', link: InnerLink | null): void }>();

const search = ref('');
const hostFilter = ref('');
const batchOpen = ref(false);
const selectedNics = ref<string[]>([]);
const showOther = ref(false);
const choices = computed(() => innerNicChoices(inner.capability, showOther.value));
const otherCount = computed(() => innerNicChoices(inner.capability, true).length - innerNicChoices(inner.capability).length);
const existingKeys = computed(() => new Set(inner.config.links.map((link) => innerEndpointKey(link.host, link.local_interface, link.local_ip, link.local_ipv6))));
const availableChoices = computed(() => choices.value.filter((choice) => !existingKeys.value.has(choice.key)));
const selectedCount = computed(() => availableChoices.value.filter((choice) => selectedNics.value.includes(choice.key)).length);
const issues = computed(() => innerSetupIssues(inner.config));
function addSelected(): void {
  const added = addInnerScannedLinks(availableChoices.value.filter((choice) => selectedNics.value.includes(choice.key)).map((choice) => choice.key));
  selectedNics.value = [];
  search.value = '';
  hostFilter.value = '';
  if (added.length) emit('edit', added[0]);
}
function addManual(): void {
  search.value = '';
  const link = addInnerLink(hostFilter.value || 'master');
  emit('edit', link);
}
const batch = ref({ gateway: '', gateway_ipv6: '', board_rx_interface: '', measurement: '' as '' | InnerMeasurement, upload_min_mbps: '', download_min_mbps: '' });

/** 表格里当前可见的行，连同它在配置里的真实下标——排序动的是配置，不是视图。 */
const rows = computed(() => inner.config.links
  .map((link, index) => ({ link, index }))
  .filter(({ link }) => (!hostFilter.value || link.host === hostFilter.value)
    && (!search.value.trim() || [link.name, link.local_interface, link.local_ip, link.gateway, link.local_ipv6 ?? '', link.gateway_ipv6 ?? '', link.board_rx_interface]
      .some((field) => field.toLowerCase().includes(search.value.trim().toLowerCase())))));
/**
 * 给每一行补上**可见的**上/下邻居在完整列表里的下标。
 *
 * 排序按钮要按人看到的顺序工作：过滤之后，「上移」应当换到上面那一行，而不是
 * 换到一行被筛掉、屏幕上根本不存在的链路。禁用条件同理——最后一行可见时，
 * 下移就该是灰的。
 */
const ordered = computed(() => rows.value.map(({ link, index }, position, all) => ({
  link,
  index,
  prev: position > 0 ? all[position - 1].index : undefined,
  next: position < all.length - 1 ? all[position + 1].index : undefined,
})));
const visibleEnabled = computed(() => rows.value.filter(({ link }) => link.enabled).map(({ link }) => link));
const enabledCount = computed(() => inner.config.links.filter((link) => link.enabled).length);

/**
 * 这一行现在能不能跑。
 *
 * 「没扫描」「电脑连不上」「电脑在但没有这个网口」是三件不同的事，混成一句
 * 「不可用」的话，拔掉辅测机和网卡改名在界面上就长得一样了。
 */
function status(link: InnerLink): { text: string; tone: string } {
  const index = inner.config.links.indexOf(link);
  if (issues.value.some((issue) => issue.linkIndex === index)) return { text: '待补齐信息', tone: 'warn' };
  if (!link.enabled) return { text: '本轮不测试', tone: '' };
  if (!inner.capability) return { text: '尚未扫描', tone: '' };
  if (link.host === 'master') return match(inner.capability.local.interfaces, link);
  const agent = inner.capability.agents.find((a) => a.id === link.host);
  if (!agent) return { text: '未配置电脑', tone: 'bad' };
  if (agent.status === 'failed') return { text: '电脑未就绪', tone: 'bad' };
  if (!agent.info) return { text: '未扫描', tone: '' };
  return match(agent.info.interfaces, link);
}
function match(interfaces: NicInfo[], link: InnerLink): { text: string; tone: string } {
  const found = interfaces.filter((nic) => nic.name === link.local_interface
    && (!inner.config.ip_versions.includes(4) || nic.ipv4 === link.local_ip)
    && (!inner.config.ip_versions.includes(6) || [nic.ipv6_ll, nic.ipv6_global].some((ip) => canonicalInnerIpv6(ip) && canonicalInnerIpv6(ip) === canonicalInnerIpv6(link.local_ipv6))));
  if (found.length === 1) return { text: '已发现网卡', tone: 'ok' };
  return { text: found.length ? '重复匹配' : '未发现该网口', tone: 'warn' };
}

function applyBatch(): void {
  const patch: InnerBatchPatch = {};
  if (inner.config.ip_versions.includes(4) && batch.value.gateway.trim()) patch.gateway = batch.value.gateway.trim();
  if (inner.config.ip_versions.includes(6) && batch.value.gateway_ipv6.trim()) patch.gateway_ipv6 = batch.value.gateway_ipv6.trim();
  if (batch.value.board_rx_interface.trim()) patch.board_rx_interface = batch.value.board_rx_interface.trim();
  if (batch.value.measurement) patch.measurement = batch.value.measurement;
  for (const key of ['upload_min_mbps', 'download_min_mbps'] as const) {
    const raw = batch.value[key].trim();
    if (raw) patch[key] = Number(raw);
  }
  applyInnerBatch(visibleEnabled.value, patch);
  batchOpen.value = false;
}
function remove(index: number, link: InnerLink): void {
  if (props.editing === link) emit('edit', null);
  inner.config.links.splice(index, 1);
}
</script>

<template>
  <div class="link-table">
    <section v-if="inner.capability" class="scan-picker" aria-label="从扫描结果添加网口">
      <strong>选择实际接到 CPE 的电脑网卡</strong>
      <p class="muted">勾选后自动填入电脑、网卡和 IPv4 / IPv6；添加后核对每个网口的 CPE LAN 地址。已发现网卡不代表 CPE 已可达，启动时还会检查链路。</p>
      <p class="muted">默认显示 192.168.* 和仅有 IPv6 的电脑网卡，隐藏常见隧道和虚拟接口。其他网段可按需展开，已添加的配置不受影响。</p>
      <label v-if="otherCount" class="other-toggle"><input v-model="showOther" type="checkbox">显示其他网段 / 隧道接口（{{ otherCount }} 项）</label>
      <div v-if="choices.length" class="scan-options">
        <label v-for="choice in choices" :key="choice.key" class="scan-option">
          <input v-model="selectedNics" type="checkbox" :value="choice.key" :disabled="props.disabled || existingKeys.has(choice.key)">
          <span><strong>{{ props.hosts.find(host => host.id === choice.host)?.label ?? choice.host }} · {{ choice.nic.name }}</strong><small>IPv4 {{ choice.nic.ipv4 || '未获取' }}</small><small>IPv6 {{ innerNicIpv6(choice.nic) || '未获取' }}</small><small v-if="existingKeys.has(choice.key)">已添加</small><small v-if="innerNicOtherReason(choice.nic)" class="warn">{{ innerNicOtherReason(choice.nic) }} · 请核对是否接到 CPE</small></span>
        </label>
      </div>
      <p v-else class="muted">{{ showOther ? '未发现带有效 IP 的网卡。' : '未发现符合默认筛选的 192.168.* 或仅 IPv6 网卡。' }}请检查网线或 Wi-Fi 连接及电脑地址{{ otherCount && !showOther ? '，也可展开其他接口' : '' }}后重新选择。</p>
      <button :disabled="props.disabled || !selectedCount || inner.config.links.length >= 32" @click="addSelected">添加选中的 {{ selectedCount }} 个网口</button>
      <small v-if="selectedCount > 32 - inner.config.links.length" class="muted">本次最多还能添加 {{ 32 - inner.config.links.length }} 个。</small>
    </section>
    <div class="bar table-tools">
      <input v-model="search" type="search" placeholder="搜索网口名 / 网卡 / IP" aria-label="搜索网口">
      <select v-model="hostFilter" aria-label="按电脑筛选">
        <option value="">全部电脑</option>
        <option v-for="host in props.hosts" :key="host.id" :value="host.id">{{ host.label }}</option>
      </select>
      <button :disabled="props.disabled" @click="setInnerLinkEnabled(rows.map((r) => r.link), true)">勾选当前筛选</button>
      <button :disabled="props.disabled" @click="setInnerLinkEnabled(rows.map((r) => r.link), false)">取消勾选</button>
      <button :disabled="props.disabled || !visibleEnabled.length" :aria-expanded="batchOpen" @click="batchOpen = !batchOpen">
        批量设置（{{ visibleEnabled.length }} 条）
      </button>
      <button :disabled="props.disabled || inner.config.links.length >= 32" @click="addManual">手动添加网口</button>
      <span class="muted">共 {{ inner.config.links.length }} 条，本轮参与 {{ enabledCount }} 条</span>
    </div>

    <fieldset v-if="batchOpen" :disabled="props.disabled" class="batch">
      <legend>批量修改已勾选的 {{ visibleEnabled.length }} 条（留空的项不改）</legend>
      <div class="batch-grid">
        <label v-if="inner.config.ip_versions.includes(4)">CPE LAN IPv4<input v-model="batch.gateway" placeholder="如 192.168.0.1；留空不修改"></label>
        <label v-if="inner.config.ip_versions.includes(6)">CPE LAN IPv6<input v-model="batch.gateway_ipv6" placeholder="按实际扫描地址填写；留空不修改"></label>
        <label>板侧桥接口<input v-model="batch.board_rx_interface" placeholder="如 br0；留空不修改"></label>
        <label>测量策略<select v-model="batch.measurement">
          <option value="">不修改</option>
          <option v-for="item in INNER_MEASUREMENTS" :key="item" :value="item">{{ MEASUREMENT_LABEL[item] }}</option>
        </select></label>
        <label>上行网卡门限 Mbps<input v-model="batch.upload_min_mbps" type="number" min="0.01" step="any"></label>
        <label>下行网卡门限 Mbps<input v-model="batch.download_min_mbps" type="number" min="0.01" step="any"></label>
      </div>
      <p class="muted">电脑、网卡名和源 IP 不参与批量：那是「哪台机器的哪个口」的身份，复制过去会把别人的源 IP 写到自己头上。</p>
      <div class="bar"><button class="primary" @click="applyBatch">应用到已勾选</button><button @click="batchOpen = false">取消</button></div>
    </fieldset>

    <p v-if="!inner.config.links.length" class="muted">还没有网口。先点上方「检查 ADB / 扫描各电脑网卡」，再从扫描结果勾选实际接到 CPE 的网卡。</p>
    <div v-else class="scroll">
      <table>
        <thead><tr>
          <th scope="col">参与</th><th scope="col">顺序</th><th scope="col">电脑</th><th scope="col">网口</th>
          <th scope="col">电脑 IP</th><th scope="col">CPE LAN 地址</th><th scope="col">统计接口 / 策略</th>
          <th scope="col">状态</th><th scope="col">操作</th>
        </tr></thead>
        <tbody>
          <tr v-for="{ link, index, prev, next } in ordered" :key="index" :class="{ current: props.editing === link, off: !link.enabled }">
            <td><input v-model="link.enabled" type="checkbox" :disabled="props.disabled" :aria-label="`${link.name} 参与本轮`"></td>
            <td class="order">
              <span class="num">{{ index + 1 }}</span>
              <button :disabled="props.disabled || prev === undefined" :aria-label="`${link.name} 上移`" @click="moveInnerLinkTo(index, prev!)">↑</button>
              <button :disabled="props.disabled || next === undefined" :aria-label="`${link.name} 下移`" @click="moveInnerLinkTo(index, next!)">↓</button>
            </td>
            <td>{{ link.host === 'master' ? '本机' : link.host }}</td>
            <td><strong>{{ link.name }}</strong><br><span class="muted">{{ link.local_interface || '未选网卡' }}</span></td>
            <td><div v-if="inner.config.ip_versions.includes(4)">v4 {{ link.local_ip || '未填写' }}</div><div v-if="inner.config.ip_versions.includes(6)">v6 {{ link.local_ipv6 || '未填写' }}</div></td>
            <td><div v-if="inner.config.ip_versions.includes(4)">v4 {{ link.gateway || '未填写' }}</div><div v-if="inner.config.ip_versions.includes(6)">v6 {{ link.gateway_ipv6 || '未填写' }}</div></td>
            <td>{{ link.board_rx_interface || '自动' }}<br><span class="muted">{{ MEASUREMENT_LABEL[link.measurement] }}</span></td>
            <td :class="status(link).tone">{{ status(link).text }}</td>
            <td class="bar">
              <button @click="emit('edit', props.editing === link ? null : link)">{{ props.editing === link ? '收起' : status(link).tone === 'warn' || status(link).tone === 'bad' ? '检查 / 修改' : '编辑' }}</button>
              <button :disabled="props.disabled" @click="remove(index, link)">删除</button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <p v-if="inner.config.links.length && !rows.length" class="muted">没有网口匹配当前筛选。<button @click="search = ''; hostFilter = ''">清空筛选</button></p>
  </div>
</template>

<style scoped>
.scan-picker { margin-bottom: 18px; padding: 16px; background: var(--panel-2); border-left: 3px solid var(--accent); }
.scan-options { display: grid; grid-template-columns: repeat(auto-fit, minmax(230px, 1fr)); gap: 8px; margin: 12px 0; }
.other-toggle { display: flex; align-items: center; gap: 8px; font-size: 13px; }
.scan-option { display: flex; align-items: flex-start; gap: 9px; padding: 9px; border: 1px solid var(--line); background: var(--surface); font-size: 13px; }
.scan-option input { margin-top: 3px; flex-shrink: 0; }
.scan-option span { overflow-wrap: anywhere; }
.scan-option small { display: block; margin-top: 3px; color: var(--muted); }
.table-tools { flex-wrap: wrap; gap: 8px; align-items: center; }
.table-tools input[type="search"] { min-width: 200px; }
/* 表格限高 + 固定表头：几十条链路也不会把页面撑成一条长卷。 */
.scroll { max-height: 24rem; overflow: auto; border: 1px solid var(--line); border-radius: 6px; }
table { width: 100%; border-collapse: collapse; font-size: 13px; }
th, td { padding: 8px 10px; text-align: left; border-bottom: 1px solid var(--line); vertical-align: top; }
thead th { position: sticky; top: 0; z-index: 1; background: var(--head); white-space: nowrap; }
tbody tr.current { outline: 2px solid var(--accent); outline-offset: -2px; }
tbody tr.off td { opacity: 0.55; }
td.order { white-space: nowrap; }
td.order button { padding: 0 6px; margin-left: 2px; }
td.bar { display: table-cell; white-space: nowrap; }
td.bar button { margin-right: 4px; }
.ok { color: var(--ok, #1a7f37); }
.warn { color: var(--warn, #9a6700); }
.bad { color: var(--bad); }
.muted { color: var(--muted); font-size: 12px; }
.batch { margin: 12px 0; padding: 12px 16px; border: 1px solid var(--line); border-radius: 6px; background: var(--panel-2); }
.batch-grid { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 12px; }
.batch-grid label { display: flex; flex-direction: column; gap: 6px; font-size: 13px; min-width: 0; }
.batch-grid input, .batch-grid select { width: 100%; min-width: 0; }
@media (max-width: 900px) { .batch-grid { grid-template-columns: 1fr; } .scroll { max-height: none; } }
</style>
