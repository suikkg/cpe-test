<script setup lang="ts">
import { computed, ref } from 'vue';
import { inner, addInnerLink, applyInnerBatch, moveInnerLinkTo, setInnerLinkEnabled } from '../../state/inner';
import type { InnerBatchPatch } from '../../state/inner';
import { INNER_MEASUREMENTS, MEASUREMENT_LABEL } from '../../domain/inner';
import type { InnerLink, InnerMeasurement } from '../../domain/inner';

const props = defineProps<{ hosts: { id: string; label: string }[]; editing: InnerLink | null; disabled: boolean }>();
const emit = defineEmits<{ (e: 'edit', link: InnerLink | null): void }>();

const search = ref('');
const hostFilter = ref('');
const batchOpen = ref(false);
const batch = ref({ gateway: '', board_rx_interface: '', measurement: '' as '' | InnerMeasurement, upload_min_mbps: '', download_min_mbps: '' });

/** 表格里当前可见的行，连同它在配置里的真实下标——排序动的是配置，不是视图。 */
const rows = computed(() => inner.config.links
  .map((link, index) => ({ link, index }))
  .filter(({ link }) => (!hostFilter.value || link.host === hostFilter.value)
    && (!search.value.trim() || [link.name, link.local_interface, link.local_ip, link.gateway, link.board_rx_interface]
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
  if (!inner.capability) return { text: '待检查', tone: '' };
  if (link.host === 'master') return match(inner.capability.local.interfaces, link);
  const agent = inner.capability.agents.find((a) => a.id === link.host);
  if (!agent) return { text: '未配置电脑', tone: 'bad' };
  if (agent.status === 'failed') return { text: '电脑未就绪', tone: 'bad' };
  if (!agent.info) return { text: '未扫描', tone: '' };
  return match(agent.info.interfaces, link);
}
function match(interfaces: { name: string; ipv4: string }[], link: InnerLink): { text: string; tone: string } {
  const found = interfaces.filter((nic) => nic.name === link.local_interface && nic.ipv4 === link.local_ip);
  if (found.length === 1) return { text: '就绪', tone: 'ok' };
  return { text: found.length ? '重复匹配' : '未发现该网口', tone: 'warn' };
}

function applyBatch(): void {
  const patch: InnerBatchPatch = {};
  if (batch.value.gateway.trim()) patch.gateway = batch.value.gateway.trim();
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
      <button :disabled="props.disabled || inner.config.links.length >= 32" @click="emit('edit', addInnerLink())">添加网口</button>
      <span class="muted">共 {{ inner.config.links.length }} 条，本轮参与 {{ enabledCount }} 条</span>
    </div>

    <fieldset v-if="batchOpen" :disabled="props.disabled" class="batch">
      <legend>批量修改已勾选的 {{ visibleEnabled.length }} 条（留空的项不改）</legend>
      <div class="batch-grid">
        <label>板侧 LAN 地址<input v-model="batch.gateway" placeholder="如 192.168.0.1；留空不修改"></label>
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

    <p v-if="!inner.config.links.length" class="muted">还没有网口。点「添加网口」，或先扫描各电脑网卡再逐条填写。</p>
    <div v-else class="scroll">
      <table>
        <thead><tr>
          <th scope="col">参与</th><th scope="col">顺序</th><th scope="col">电脑</th><th scope="col">网口</th>
          <th scope="col">源 IP</th><th scope="col">板侧 LAN</th><th scope="col">统计接口 / 策略</th>
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
            <td>{{ link.local_ip || '—' }}</td>
            <td>{{ link.gateway || '—' }}</td>
            <td>{{ link.board_rx_interface || '自动' }}<br><span class="muted">{{ MEASUREMENT_LABEL[link.measurement] }}</span></td>
            <td :class="status(link).tone">{{ status(link).text }}</td>
            <td class="bar">
              <button @click="emit('edit', props.editing === link ? null : link)">{{ props.editing === link ? '收起' : '编辑' }}</button>
              <button :disabled="props.disabled" @click="remove(index, link)">删除</button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <p v-if="inner.config.links.length && !rows.length" class="muted">当前筛选没有匹配的网口；清空搜索即可看到全部 {{ inner.config.links.length }} 条。</p>
  </div>
</template>

<style scoped>
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
