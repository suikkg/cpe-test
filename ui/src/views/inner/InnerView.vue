<script setup lang="ts">
import { scenarioBlocksActions } from '../../state/inner';
import { computed, nextTick, onMounted, ref } from 'vue';
import {
  inner, importInner, innerReport, loadInnerDraft,
  probeInner, refreshInnerPlan, startInner, startSubnetThenInner, stopInner, stopScenario, syncInnerStatus,
  syncScenarioStatus, prepareAfterUnknownScenario,
} from '../../state/inner';
import {
  DIRECTION_LABEL, FLOW_LABEL, INNER_DIRECTIONS, INNER_IP_VERSIONS, INNER_PROTOCOLS, MEASUREMENT_LABEL, PROTOCOL_LABEL,
  innerDuration, normalizeInnerDraft, serializeInnerProject,
} from '../../domain/inner';
import type { InnerLink } from '../../domain/inner';
import { innerBoardInterfaces, innerSetupIssues, type InnerSetupIssue } from '../../domain/inner-setup';
import { errorMessage } from '../../api/client';
import InnerLinkTable from './InnerLinkTable.vue';
import InnerLinkDetail from './InnerLinkDetail.vue';
import InnerResults from './InnerResults.vue';
import { saveFile } from '../download';

const props = defineProps<{ subnetRunning?: boolean }>();
const emit = defineEmits<{ (e: 'show-subnet-progress'): void }>();
const fileInput = ref<HTMLInputElement>();
const editing = ref<InnerLink | null>(null);
const locked = computed(() => props.subnetRunning || inner.busy || inner.status.running || scenarioBlocksActions() || !inner.synced);
const hosts = computed(() => [{ id: 'master', label: '主控本机' }, ...inner.config.agents.map((a) => ({ id: a.id, label: `${a.id} · ${a.address}` }))]);
const tcp = computed(() => inner.config.protocols.includes('tcp'));
const udp = computed(() => inner.config.protocols.includes('udp'));
const enabledLinks = computed(() => inner.config.links.filter((link) => link.enabled).length);
const setupIssues = computed(() => innerSetupIssues(inner.config));
const boardInterfaces = computed(() => innerBoardInterfaces(inner.capability, inner.config.links));
const devicePanel = ref<HTMLElement>();
const linksPanel = ref<HTMLElement>();
const paramsPanel = ref<HTMLElement>();
const detailPanel = ref<HTMLElement>();
const deviceSettings = ref<HTMLDetailsElement>();
const advancedParams = ref<HTMLDetailsElement>();
const startBlocked = computed(() => {
  if (props.subnetRunning) return '子网测试正在运行，结束后才能开始。';
  if (inner.scenarioStartPhase === 'unknown') return '启动结果未确认，正在查询，请勿重复启动。';
  if (inner.status.running || inner.scenario.running) return '测试正在运行。';
  if (!inner.synced) return '尚未读到运行状态。';
  if (inner.busy) return '处理中…';
  if (setupIssues.value.length) return '先补齐上面的缺项。';
  if (!inner.preview || inner.previewStale) return '等待预览；可点「刷新预览」。';
  if (!inner.preview.units) return '没有可执行单元，检查参与的网口。';
  return '';
});

async function editLink(link: InnerLink | null): Promise<void> {
  editing.value = link;
  if (!link) return;
  await nextTick();
  detailPanel.value?.scrollIntoView({ block: 'nearest' });
  detailPanel.value?.focus({ preventScroll: true });
}
async function fixIssue(issue: InnerSetupIssue): Promise<void> {
  if (issue.linkIndex !== undefined) { await editLink(inner.config.links[issue.linkIndex]); return; }
  if (issue.target === 'device' && deviceSettings.value) deviceSettings.value.open = true;
  if (issue.target === 'params' && advancedParams.value) advancedParams.value.open = true;
  const panel = ({ device: devicePanel.value, links: linksPanel.value, params: paramsPanel.value })[issue.target];
  panel?.scrollIntoView({ block: 'start' });
  panel?.focus({ preventScroll: true });
}

function addAgent(): void {
  let i = 1;
  while (inner.config.agents.some((a) => a.id === `agent${i}`)) i++;
  inner.config.agents.push({ id: `agent${i}`, address: '', port: 28801, token: '' });
}
function exportConfig(): void {
  saveFile('cpe-inner-project.json', serializeInnerProject(normalizeInnerDraft(inner.config)), 'application/json');
}
async function importFile(event: Event): Promise<void> {
  const input = event.target as HTMLInputElement;
  try {
    const file = input.files?.[0];
    if (file) { importInner(await file.text()); editing.value = null; void refreshInnerPlan(); }
  } catch (e) { inner.error = errorMessage(e); }
  finally { input.value = ''; }
}
async function report(): Promise<void> {
  try { const result = await innerReport(); saveFile(result.name, result.html, 'text/html;charset=utf-8'); }
  catch (e) { inner.error = errorMessage(e); }
}
async function startScenario(): Promise<void> {
  try { await startSubnetThenInner(); }
  catch (e) { inner.error = errorMessage(e); }
}
/**
 * 停止：组合场景在跑就停整个场景，否则停内环。
 *
 * 两个分支都必须**调用**。旧写法 `running ? stopScenario() : stopInner` 只引用
 * 了 `stopInner` 而没有调用它，单独的内环测试点「停止」什么都不发生。
 */
function stopCurrent(): void {
  if (inner.scenario.running) void stopScenario();
  else void stopInner();
}
/**
 * 勾选框直接改数组；最后一项不许取消，否则一个单元都跑不出来。
 *
 * 拒绝时返回的是**副本**，不是 `list` 本身：赋回同一个引用时 Vue 认作没变
 * （`toRaw` 相等），`:checked` 不会重算，于是 DOM 上的框已经空了而状态还留着
 * 那一项——下一次点击走 `on=true` 分支，拼出 `['tcp','tcp']`，前后端校验都拒，
 * 草稿从此存不下、计划也起不来，界面上没有恢复路径。
 */
function toggle<T>(list: T[], value: T, on: boolean): T[] {
  const next = on ? [...list, value] : list.filter((v) => v !== value);
  return next.length ? next : [...list];
}
const agentTone = (status: string) => (status === 'failed' ? 'bad' : status === 'ready' ? 'ok' : 'muted');
const agentText = (status: string) => ({ ready: '就绪', failed: '连接失败', not_participating: '本轮未参与' }[status] ?? status);

onMounted(() => { loadInnerDraft(); void syncInnerStatus(); void syncScenarioStatus(); void refreshInnerPlan(); });
</script>
<template>
  <section class="view inner-view">
    <header class="page-head">
      <h2>内环测试</h2>
      <div class="actions">
        <button type="button" class="ghost small" :disabled="locked" @click="fileInput?.click()">导入配置</button>
        <button type="button" class="ghost small" @click="exportConfig">导出配置</button>
        <input ref="fileInput" type="file" accept=".json,application/json" hidden @change="importFile">
      </div>
    </header>

    <p v-if="!inner.draftSaved" class="msg warn" role="status">配置不完整（缺网卡或所选 IP 版本的地址），草稿未保存，刷新会丢失。</p>
    <p v-if="inner.error" class="msg bad" role="alert">{{ inner.error }}</p>
    <p v-if="props.subnetRunning" class="msg warn" role="status">
      子网测试正在占用网口，结束后才能开始内环测试。
      <button type="button" class="linklike" @click="emit('show-subnet-progress')">查看子网进度</button>
    </p>
    <div v-if="!inner.synced" class="msg warn" role="status">
      <p>尚未读到内环运行状态。</p>
      <button type="button" class="ghost small" @click="syncInnerStatus">重新同步</button>
    </div>

    <fieldset ref="devicePanel" :disabled="locked" class="inner-card" tabindex="-1">
      <legend>1 · 设备与网卡</legend>
      <div class="inner-actions">
        <button class="primary" @click="probeInner">{{ inner.busy ? '检查与扫描中…' : '检查 ADB / 扫描各电脑网卡' }}</button>
      </div>
      <details ref="deviceSettings" class="advanced-settings">
        <summary>ADB 与辅测机设置{{ inner.capability ? `（已识别 ${inner.capability.serial}）` : '' }}</summary>
        <div class="inner-grid">
          <label>主控 ADB 程序路径<input v-model="inner.config.adb_path" placeholder="adb"></label>
          <label>ADB 序列号（单设备可留空）<input v-model="inner.config.serial" placeholder="自动选择唯一设备"></label>
          <label>板侧 iperf3 路径<input v-model="inner.config.board_iperf" placeholder="iperf3"></label>
        </div>
        <p class="hint">辅测机运行同版本 <code>cpe_test agent</code>；没有辅测机时本机可独立测试。</p>
        <div v-for="(agent, index) in inner.config.agents" :key="index" class="agent-row inner-grid">
          <label>辅测机标识<input v-model="agent.id" :readonly="inner.config.links.some(l => l.host === agent.id)"></label>
          <label>地址<input v-model="agent.address" placeholder="192.168.8.101"></label>
          <label>端口<input v-model.number="agent.port" type="number" min="1" max="65535"></label>
          <label>令牌（可空，不导出）<input v-model="agent.token" type="password" autocomplete="off"></label>
          <button :disabled="inner.config.links.some(l => l.host === agent.id)" @click="inner.config.agents.splice(index, 1)">移除辅测机</button>
        </div>
        <div class="inner-actions">
          <button class="ghost" :disabled="inner.config.agents.length >= 8" @click="addAgent">添加辅测机</button>
        </div>
      </details>
      <div v-if="inner.capability" class="host-status">
        <span class="ok">ADB 已识别 CPE：{{ inner.capability.serial }}</span>
        <span v-for="agent in inner.capability.agents" :key="agent.id" :class="agentTone(agent.status)">
          {{ agent.id }}：{{ agentText(agent.status) }}<template v-if="agent.error"> — {{ agent.error }}</template>
        </span>
      </div>
      <p v-if="inner.capability?.agents.some(a => a.status === 'failed')" class="hint">辅测机连接失败时，可取消其网口的参与，只测本机。</p>
      <template v-if="inner.capability">
        <p v-if="inner.capability.board_inventory_error" class="warn" role="status">板侧接口信息不完整：{{ inner.capability.board_inventory_error }}。请核对统计接口后重新扫描。</p>
        <p v-if="boardInterfaces.length" class="hint">板侧 LAN / 统计接口：<span v-for="(iface, index) in boardInterfaces" :key="iface.name">{{ index ? '；' : '' }}{{ iface.name }} · {{ [...iface.addresses, ...(iface.ipv6_addresses ?? [])].join('、') || '无独立 IP' }}</span></p>
        <p v-else class="warn">未发现 192.168.* 的板侧 LAN 地址或统计接口，请展开完整清单核对。</p>
      </template>
      <details v-if="inner.capability"><summary>查看板侧全部系统接口（{{ inner.capability.board_interfaces.length }} 个）</summary>
        <p class="hint">这里是 CPE 的系统清单，实际参与测试的电脑网口以下方勾选为准。</p>
        <pre>{{ inner.capability.board_version }}</pre>
        <div class="board-inventory"><table class="data">
          <thead><tr><th scope="col">系统接口</th><th scope="col">IPv4 / IPv6</th><th scope="col">所属桥 / 成员</th></tr></thead>
          <tbody><tr v-for="iface in inner.capability.board_interfaces" :key="iface.name"><td>{{ iface.name }}</td><td>{{ [...iface.addresses, ...(iface.ipv6_addresses ?? [])].join('、') || '—' }}</td><td>{{ iface.master || '—' }}<template v-if="iface.members.length"> / {{ iface.members.join('、') }}</template></td></tr></tbody>
        </table></div>
      </details>
    </fieldset>

    <fieldset ref="linksPanel" :disabled="locked" class="inner-card" tabindex="-1">
      <legend>2 · 网口</legend>
      <InnerLinkTable :hosts="hosts" :editing="editing" :disabled="locked" @edit="editLink" />
      <div v-if="editing" ref="detailPanel" class="detail-panel" tabindex="-1"><InnerLinkDetail :link="editing" :hosts="hosts" :disabled="locked" @close="editing = null" /></div>
    </fieldset>

    <fieldset ref="paramsPanel" :disabled="locked" class="inner-card" tabindex="-1">
      <legend>3 · 参数</legend>
      <div class="inner-grid">
        <fieldset class="inner-choice">
          <legend>IP 版本（分别测试）</legend>
          <label v-for="version in INNER_IP_VERSIONS" :key="version" class="inner-check">
            <input type="checkbox" :checked="inner.config.ip_versions.includes(version)"
              :disabled="inner.config.ip_versions.length === 1 && inner.config.ip_versions.includes(version)"
              :title="inner.config.ip_versions.length === 1 && inner.config.ip_versions.includes(version) ? '至少保留一个 IP 版本' : undefined"
              @change="inner.config.ip_versions = toggle(inner.config.ip_versions, version, ($event.target as HTMLInputElement).checked)">
            IPv{{ version }}
          </label>
        </fieldset>
        <fieldset class="inner-choice">
          <legend>协议（依次测试）</legend>
          <label v-for="item in INNER_PROTOCOLS" :key="item" class="inner-check">
            <input type="checkbox" :checked="inner.config.protocols.includes(item)"
              :disabled="inner.config.protocols.length === 1 && inner.config.protocols.includes(item)"
              :title="inner.config.protocols.length === 1 && inner.config.protocols.includes(item) ? '至少要保留一个协议' : undefined"
              @change="inner.config.protocols = toggle(inner.config.protocols, item, ($event.target as HTMLInputElement).checked)">
            {{ PROTOCOL_LABEL[item] }}
          </label>
        </fieldset>
        <fieldset class="inner-choice">
          <legend>方向</legend>
          <label v-for="item in INNER_DIRECTIONS" :key="item" class="inner-check">
            <input type="checkbox" :checked="inner.config.directions.includes(item)"
              :disabled="inner.config.directions.length === 1 && inner.config.directions.includes(item)"
              :title="inner.config.directions.length === 1 && inner.config.directions.includes(item) ? '至少要保留一个方向' : undefined"
              @change="inner.config.directions = toggle(inner.config.directions, item, ($event.target as HTMLInputElement).checked)">
            {{ DIRECTION_LABEL[item] }}
          </label>
        </fieldset>
      </div>
      <p class="hint direction-guide">上行：电脑 → CPE，统计 CPE 板侧接收；下行：CPE → 电脑，统计所选电脑网卡接收；双向并发同时测上下行。</p>
      <div class="inner-grid">
        <label>每个方向测试时长（秒）<input v-model.number="inner.config.duration_secs" type="number" min="6" max="3600"></label>
        <label>每个单元重复轮次<input v-model.number="inner.config.repeats" type="number" min="1" max="10"></label>
        <label v-if="udp">UDP 每条流发送速率（Mbps，必填）<input v-model.number="inner.config.udp_mbps" type="number" min="0.01" step="any" placeholder="希望发送的速率"></label>
        <label class="inner-check"><input v-model="inner.config.resume" type="checkbox">跳过 24 小时内已通过的单元（RESUME）</label>
      </div>
      <details ref="advancedParams" class="advanced-settings">
        <summary>高级：并行流数、窗口、报文与端口</summary>
        <div class="inner-grid">
          <label>并行流数（默认）<input v-model.number="inner.config.parallel" type="number" min="1" max="16"></label>
          <label>板侧起始端口<input v-model.number="inner.config.port" type="number" min="1024" max="65534"></label>
          <label v-if="tcp">TCP 流数（可空，覆盖默认）<input v-model.number="inner.config.tcp_streams" type="number" min="1" max="16" placeholder="沿用并行流数"></label>
          <label v-if="tcp">TCP 窗口 -w（可空）<input v-model="inner.config.tcp_window" placeholder="如 4m、64k"></label>
          <label v-if="udp">UDP 流数（可空，覆盖默认）<input v-model.number="inner.config.udp_streams" type="number" min="1" max="16" placeholder="沿用并行流数"></label>
          <label v-if="udp">UDP 报文长度 -l（可空）<input v-model="inner.config.udp_length" placeholder="如 1400、64"></label>
          <label v-if="udp">UDP 丢包门槛 %（可空）<input v-model.number="inner.config.max_udp_loss_pct" type="number" min="0" max="100" step="any" placeholder="仅诊断，不推翻速率判定"></label>
        </div>
      </details>
    </fieldset>

    <section class="inner-card" aria-labelledby="inner-run-title">
      <h3 id="inner-run-title">4 · 预览与执行</h3>

      <section aria-label="计划预览">
        <div class="bar">
          <button class="ghost small" :disabled="locked || !!setupIssues.length" @click="refreshInnerPlan">刷新预览</button>
          <span v-if="inner.previewStale" class="warn" aria-live="polite">配置已改动，预览待刷新</span>
        </div>
        <div v-if="setupIssues.length" class="setup-check" role="status">
          <strong>还需完成 {{ setupIssues.length }} 项</strong>
          <ul><li v-for="(issue, index) in setupIssues" :key="index"><span>{{ issue.message }}</span><button class="ghost small" :disabled="locked" @click="fixIssue(issue)">{{ issue.action }}</button></li></ul>
        </div>
        <p v-if="inner.previewError && !setupIssues.length" class="msg bad" role="alert">{{ inner.previewError }}</p>
        <template v-if="inner.preview">
          <p class="summary">
            <strong>{{ inner.preview.units }}</strong> 个单元（双向 {{ inner.preview.bidir_units }}）·
            <strong>{{ inner.preview.legs }}</strong> 条数据腿 · {{ inner.preview.links }} 个网口 ·
            约 {{ innerDuration(inner.preview.estimated_secs) }}
            <template v-if="inner.preview.resumed"> · RESUME 跳过 {{ inner.preview.resumed }}</template>
            · 参与：{{ [inner.preview.uses_master ? '本机' : '', ...inner.preview.agents].filter(Boolean).join('、') || '无' }}
          </p>
          <p v-if="inner.preview.skipped.length" class="hint">未参与：{{ inner.preview.skipped.join('、') }}</p>
          <details>
            <summary>逐单元清单（{{ inner.preview.rows.length }} 行）</summary>
            <div class="preview-list">
              <p v-for="row in inner.preview.rows" :key="row.index">
                <strong>#{{ row.index }}</strong> {{ row.host }} / {{ row.link }} · IPv{{ row.ip_version ?? 4 }} · {{ PROTOCOL_LABEL[row.protocol] }} ·
                {{ DIRECTION_LABEL[row.direction] }} · 第 {{ row.repeat }} 轮 · {{ MEASUREMENT_LABEL[row.measurement] }}<span v-if="row.resumed"> · RESUME 跳过</span><br>
                <span v-for="leg in row.legs" :key="leg.flow" class="muted">
                  {{ FLOW_LABEL[leg.flow] }} 腿 → 接收端 {{ leg.receiver }}，端口 {{ leg.port }}；
                </span>
                <span class="muted">{{ row.verdict_basis }}</span>
              </p>
            </div>
          </details>
        </template>
      </section>

      <section aria-label="内环执行与结果">
        <p class="hint" role="status">{{ startBlocked || `将测试 ${enabledLinks} 个网口；未填门限的网口只记录速率。` }}</p>
        <div class="bar">
          <button class="primary" :disabled="!!startBlocked" @click="startInner">开始内环测试</button>
          <button class="ghost" :disabled="!!startBlocked" @click="startScenario">先跑子网，再跑内环（RESUME）</button>
          <button class="ghost danger" :disabled="inner.busy || (!inner.status.running && !inner.scenario.running && inner.synced)" @click="stopCurrent">停止测试</button>
          <button class="ghost" :disabled="!inner.status.has_report" @click="report">下载内环报告</button>
          <span class="status" aria-live="polite">
            {{ inner.scenarioStartPhase === 'unknown' ? '启动结果未确认' : inner.scenario.running ? '组合场景运行中' : inner.status.running ? '运行中' : inner.synced ? '空闲 / 已结束' : '待同步' }} ·
            {{ inner.status.completed }} / {{ inner.status.total }} 个单元
          </span>
        </div>
        <p v-if="inner.status.current" aria-live="polite">{{ inner.status.current }}</p>
        <div v-if="inner.scenarioStartPhase === 'unknown'" class="msg warn" role="status">
          <p>启动结果未确认，正在查询。请勿重复启动。</p>
          <button v-if="inner.scenarioLastReadIdle" class="ghost small" @click="prepareAfterUnknownScenario">已核实测试未运行，重新准备</button>
        </div>
        <p v-if="inner.scenario.running" class="msg" aria-live="polite">组合场景：{{ inner.scenario.phase === 'subnet' ? '正在执行子网测试' : inner.scenario.phase === 'inner' ? '子网完成，正在执行内环' : inner.scenario.phase }}</p>
        <p v-if="inner.status.error" class="msg bad" role="alert">{{ inner.status.error }}</p>
        <InnerResults />
      </section>
    </section>
  </section>
</template>

<style scoped>
.inner-view { max-width: 1400px; }
.advanced-settings { margin-top: 16px; padding-top: 12px; border-top: 1px solid var(--line); }
.advanced-settings > summary { margin-bottom: 12px; font-size: 13px; }
.direction-guide { margin: 10px 0 14px; }
.setup-check { margin-top: 12px; padding: 12px 14px; background: var(--panel-2); border-left: 3px solid var(--warn); }
.setup-check ul { list-style: none; padding: 0; margin: 6px 0 0; }
.setup-check li { display: flex; align-items: center; justify-content: space-between; gap: 16px; padding: 6px 0; font-size: 13px; }
.setup-check button { flex-shrink: 0; }
.inner-card { min-width: 0; margin: 16px 0; padding: 18px 20px; border: 1px solid var(--line); border-radius: 8px; background: var(--surface); }
.inner-card > legend, .inner-card > h3 { font-weight: 700; padding: 0 8px; margin: 0; font-size: 15px; }
.inner-card > h3 { padding: 0; margin-bottom: 12px; }
.inner-card .hint { margin: 8px 0; }
.inner-card .warn { color: var(--warn); }
.inner-card section + section { margin-top: 16px; padding-top: 14px; border-top: 1px solid var(--line); }
.inner-grid { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 14px; }
.inner-grid label { display: flex; flex-direction: column; gap: 6px; min-width: 0; font-size: 13px; }
.inner-grid input, .inner-grid select { width: 100%; min-width: 0; }
.agent-row { padding: 14px; margin: 12px 0; border: 1px solid var(--line); border-radius: 6px; background: var(--panel-2); }
.agent-row button { align-self: end; justify-self: start; }
.inner-actions { margin-top: 12px; }
.bar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.status { color: var(--muted); font-size: 12.5px; }
.summary { margin: 10px 0 4px; font-size: 13px; }
.detail-panel { margin-top: 16px; }
.host-status { display: flex; flex-wrap: wrap; gap: 8px 20px; margin-top: 12px; font-size: 13px; }
.host-status .ok { color: var(--ok); }
.host-status .bad { color: var(--bad); }
.host-status .muted { color: var(--muted); }
.inner-choice { min-width: 0; padding: 8px 12px 12px; border: 1px solid var(--line); border-radius: 6px; }
.inner-choice legend { font-size: 13px; padding: 0 6px; }
.inner-check { display: inline-flex; flex-direction: row !important; align-items: center; gap: 6px; margin-right: 16px; font-size: 13px; }
.inner-check input { width: auto !important; }
.preview-list { max-height: 20rem; overflow-y: auto; font-size: 13px; }
.board-inventory { max-height: 18rem; overflow: auto; border: 1px solid var(--line); border-radius: 6px; }
.board-inventory thead th { position: sticky; top: 0; }
.muted { color: var(--muted); font-size: 12px; }
pre { white-space: pre-wrap; overflow-wrap: anywhere; }
@media (max-width: 1000px) { .inner-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
@media (max-width: 650px) {
  .inner-grid { grid-template-columns: 1fr; }
  .inner-card { padding: 14px; }
  .setup-check li { align-items: flex-start; flex-direction: column; gap: 8px; }
}
</style>
