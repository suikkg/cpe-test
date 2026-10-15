<script setup lang="ts">
import { scenarioBlocksActions } from '../../state/inner';
import { computed, nextTick, onMounted, ref, watch } from 'vue';
import {
  inner, importInner, innerReport, loadInnerDraft, restoreInnerDefaults,
  probeInner, prepareAfterUnknownInner, skipInnerUnit, refreshInnerPlan, startInner, startSubnetThenInner, stopInner, stopScenario, syncInnerStatus,
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
import InnerParameterInput from './InnerParameterInput.vue';
import InnerWorkspace from './InnerWorkspace.vue';
import InnerPingOptions from './InnerPingOptions.vue';
import UiTabs from '../../components/UiTabs.vue';
import NicTable from '../../components/NicTable.vue';
import type { NicInfo } from '../../api/dto';
import { filterByQuery } from '../../domain/search';
import { innerAssignedLinks } from '../../domain/inner-workspace';
import { api } from '../../api/client';
import { saveBase64File } from '../download';
import { saveFile } from '../download';

const props = defineProps<{ subnetRunning?: boolean; staged?: boolean; page?: 'connect' | 'plan' | 'run' }>();
const emit = defineEmits<{ (e: 'show-subnet-progress'): void; (e: 'navigate', page: 'connect' | 'plan' | 'run'): void }>();
const resetArmed = ref(false);
function restoreDefault(): void { if (!resetArmed.value) { resetArmed.value = true; return; } restoreInnerDefaults(); editing.value = null; resetArmed.value = false; }
const fileInput = ref<HTMLInputElement>();
const editing = ref<InnerLink | null>(null);
const locked = computed(() => props.subnetRunning || inner.busy || inner.status.running || scenarioBlocksActions() || !inner.synced);
const hosts = computed(() => [{ id: 'master', label: '主控本机' }, ...inner.config.agents.map((a) => ({ id: a.id, label: `${a.id} · ${a.address}` }))]);
const tcp = computed(() => inner.config.protocols.includes('tcp'));
const udp = computed(() => inner.config.protocols.includes('udp'));
const enabledLinks = computed(() => innerAssignedLinks(inner.config).length);
watch(() => [inner.status.running, inner.status.has_report], () => { if (props.staged && (inner.status.running || (inner.status.has_report && inner.stage === 'connect' && !inner.preparing))) inner.stage = 'run'; });
const overrides = computed(() => inner.config.run_overrides!);
const stage = computed({ get: () => props.page ?? inner.stage, set: (value: string) => { inner.stage = value as typeof inner.stage; if (props.page) emit('navigate', inner.stage); } });
const visible = (value: string) => !props.staged || stage.value === value;
const showProgress = computed(() => !inner.synced || inner.status.running || scenarioBlocksActions() || ((inner.status.has_report || inner.status.units.length > 0) && !inner.preparing));
function review(): void { stage.value = 'run'; inner.preparing = true; void refreshInnerPlan(); }
async function begin(): Promise<void> { await startInner(); if (inner.status.running) inner.preparing = false; }
async function excel(): Promise<void> { try { const out = await api.get<{name:string; base64:string}>('/api/inner/excel'); saveBase64File(out.name, out.base64); } catch(e) { inner.error = errorMessage(e); } }
const setupIssues = computed(() => innerSetupIssues(inner.config));
const boardInterfaces = computed(() => innerBoardInterfaces(inner.capability, inner.config.links));
const connectionQuery = ref('');
const filteredNics = (nics: NicInfo[]) => filterByQuery(nics, connectionQuery.value, n => [n.name, n.description, n.role, n.ipv4, n.ipv6_global, n.ipv6_ll]);
const devicePanel = ref<HTMLElement>();
const linksPanel = ref<HTMLElement>();
const paramsPanel = ref<HTMLElement>();
const detailPanel = ref<HTMLElement>();
const deviceSettings = ref<HTMLDetailsElement>();
const advancedParams = ref<HTMLDetailsElement>();
const startBlocked = computed(() => {
  if (inner.startPhase === 'unknown') return '启动结果未确认，请勿重复启动。';
  if (props.subnetRunning) return '子网测试正在运行，结束后才能开始。';
  if (inner.scenarioStartPhase === 'unknown') return '启动结果未确认，正在查询，请勿重复启动。';
  if (inner.status.running || inner.scenario.running) return '测试正在运行。';
  if (!inner.synced) return '尚未读到运行状态。';
  if (inner.busy) return '处理中…';
  if (setupIssues.value.length) return '配置有缺项。';
  if (!inner.preview?.plan_hash || inner.previewStale) return '预览未更新。';
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
  if (issue.workspaceTab) inner.workspaceTab = issue.workspaceTab;
  if (props.staged) stage.value = issue.target === 'device' ? 'connect' : 'plan';
  await nextTick();
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

onMounted(() => { if (props.staged && (inner.status.running || scenarioBlocksActions())) inner.stage = 'run'; loadInnerDraft(); void syncInnerStatus(); void syncScenarioStatus(); void refreshInnerPlan(); });
</script>
<template>
  <section class="view inner-view">
    <header class="page-head">
      <h2>{{ page ? {connect:'连接',plan:'计划',run:'执行'}[page] : '内环测试' }}</h2>
      <div v-if="!staged || page === 'plan'" class="actions">
        <button type="button" class="ghost small" :disabled="locked" @click="fileInput?.click()">导入项目</button>
        <button type="button" class="ghost small" @click="exportConfig">导出项目</button>
        <button type="button" class="ghost small" :disabled="locked" :class="{danger: resetArmed}" @click="restoreDefault">{{ resetArmed ? '确认恢复默认' : '恢复默认' }}</button><button v-if="resetArmed" type="button" class="ghost small" @click="resetArmed = false">取消</button>
        <input ref="fileInput" type="file" accept=".json,application/json" hidden @change="importFile">
      </div>
    </header>

    <UiTabs v-if="staged && !page" v-model="stage" label="内环测试流程" panel-prefix="inner-flow" :tabs="[{id:'connect',label:'连接'},{id:'plan',label:'计划'},{id:'run',label:'执行'}]" />
    <p v-if="!inner.draftSaved" class="msg warn" role="status">配置不完整，草稿未保存，刷新会丢失。</p>
    <p v-if="inner.error" class="msg bad" role="alert">{{ inner.error }}</p>
    <p v-if="props.subnetRunning" class="msg warn" role="status">
      子网测试正在占用网口，结束后才能开始内环测试。
      <button type="button" class="linklike" @click="emit('show-subnet-progress')">查看子网进度</button>
    </p>
    <div v-if="!inner.synced" class="msg warn" role="status">
      <p>尚未读到内环运行状态。</p>
      <button type="button" class="ghost small" @click="syncInnerStatus">重新同步</button>
    </div>

    <fieldset v-if="visible('connect')" id="inner-flow-connect" ref="devicePanel" :disabled="locked" class="inner-card" tabindex="-1">
      <legend>设备与网卡</legend>
      <p class="hint">主控通过 ADB 连接 CPE 即可测试；辅测机只在其网口参与时需要。</p>
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
        <p class="hint">辅测机（可选）</p>
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

      <template v-if="inner.capability">
        <p v-if="inner.capability.board_iperf_error" class="warn" role="status">板侧 iperf3 未就绪：{{ inner.capability.board_iperf_error }}</p>
        <p v-if="inner.capability.board_inventory_error" class="warn" role="status">板侧接口信息不完整：{{ inner.capability.board_inventory_error }}。</p>
        <p v-if="boardInterfaces.length" class="hint">板侧 LAN / 统计接口：<span v-for="(iface, index) in boardInterfaces" :key="iface.name">{{ index ? '；' : '' }}{{ iface.name }} · {{ [...iface.addresses, ...(iface.ipv6_addresses ?? [])].join('、') || '无独立 IP' }}</span></p>
        <p v-else class="warn">未发现板侧 LAN 地址或统计接口。</p>
      </template>
      <div v-if="inner.capability" class="toolbar"><label class="grow"><span class="sr-only">搜索内环电脑网卡</span><input v-model="connectionQuery" type="search" aria-label="搜索内环电脑网卡" placeholder="接口名、描述、IP、角色"></label></div>
      <template v-if="inner.capability"><h3>主控 · {{ inner.capability.local.hostname }}</h3><NicTable label="内环主控网卡" :nics="filteredNics(inner.capability.local.interfaces)" empty-hint="无匹配网卡" /><template v-for="agent in inner.capability.agents" :key="agent.id"><h3>{{ agent.id }} · {{ agent.info?.hostname ?? '未连接' }}</h3><NicTable :label="`${agent.id} 内环网卡`" :nics="filteredNics(agent.info?.interfaces ?? [])" empty-hint="无可用网卡" /></template></template>
      <details v-if="inner.capability"><summary>查看板侧全部系统接口（{{ inner.capability.board_interfaces.length }} 个）</summary>

        <pre>{{ inner.capability.board_version }}</pre>
        <div class="board-inventory"><table class="data">
          <thead><tr><th scope="col">系统接口</th><th scope="col">IPv4 / IPv6</th><th scope="col">所属桥 / 成员</th></tr></thead>
          <tbody><tr v-for="iface in inner.capability.board_interfaces" :key="iface.name"><td>{{ iface.name }}</td><td>{{ [...iface.addresses, ...(iface.ipv6_addresses ?? [])].join('、') || '—' }}</td><td>{{ iface.master || '—' }}<template v-if="iface.members.length"> / {{ iface.members.join('、') }}</template></td></tr></tbody>
        </table></div>
      </details>
    </fieldset>

    <fieldset v-if="visible('plan')" id="inner-flow-plan" ref="linksPanel" :disabled="locked" class="inner-card" tabindex="-1">
      <legend>计划</legend>
      <InnerWorkspace :disabled="locked" @review="review" />
      <InnerLinkTable v-if="!inner.config.workspace || inner.workspaceTab === 'ports'" :hosts="hosts" :editing="editing" :disabled="locked" @edit="editLink" />
      <div v-if="editing && (!inner.config.workspace || inner.workspaceTab === 'ports')" ref="detailPanel" class="detail-panel" tabindex="-1"><InnerLinkDetail :link="editing" :hosts="hosts" :disabled="locked" @close="editing = null" /></div>
    </fieldset>

    <fieldset v-if="visible('plan') && !inner.config.workspace" ref="paramsPanel" :disabled="locked" class="inner-card" tabindex="-1">
      <legend>测试内容与默认值</legend>
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
      <InnerPingOptions v-if="inner.config.protocols.includes('ping') && inner.config.ping_options" :options="inner.config.ping_options" />
      <p class="hint direction-guide">上行：电脑 → CPE；下行：CPE → 电脑；双向：上下行同时运行。</p>
      <div class="inner-grid">
        <label>测试时长（秒）<input v-model.number="inner.config.duration_secs" type="number" min="1" max="86400"></label>
        <label>重复轮次<input v-model.number="inner.config.repeats" type="number" min="1" max="10"></label>
        <p class="muted">多档参数用空格或逗号分隔，按组合测试。每项最多 16 档，整轮最多 4096 个单元。</p>
        <label v-if="udp">UDP 每流速率（Mbps）<InnerParameterInput :config="inner.config" field="udp_mbps" axis="udp_rates_mbps" :numeric="true" /></label>
        <label class="inner-check"><input v-model="inner.config.screenshot" type="checkbox">测试结束后截图（参与电脑，每单元一次）</label>
        <label class="inner-check"><input v-model="inner.config.resume" type="checkbox">跳过 24 小时内已通过的单元（RESUME）</label>
      </div>
      <details ref="advancedParams" class="advanced-settings">
        <summary>高级：并行流数、窗口、报文与端口</summary>
        <div class="inner-grid">
          <label>默认流数<input v-model.number="inner.config.parallel" type="number" min="1" max="32"></label>
          <label>板侧起始端口<input v-model.number="inner.config.port" type="number" min="1024" max="65534"></label>
          <label v-if="tcp">TCP 流数<InnerParameterInput :config="inner.config" field="tcp_streams" axis="tcp_streams" :numeric="true" /></label>
          <label v-if="tcp">TCP 窗口 -w<InnerParameterInput :config="inner.config" field="tcp_window" axis="tcp_windows" :numeric="false" /></label>
          <label v-if="udp">UDP 流数<InnerParameterInput :config="inner.config" field="udp_streams" axis="udp_streams" :numeric="true" /></label>
          <label v-if="udp">UDP 窗口 -w<InnerParameterInput :config="inner.config" field="udp_window" axis="udp_windows" :numeric="false" /></label><label v-if="udp">UDP 报文长度 -l<InnerParameterInput :config="inner.config" field="udp_length" axis="udp_lengths" :numeric="false" /></label>
          <label v-if="udp">UDP 丢包门槛 %<input v-model.number="inner.config.max_udp_loss_pct" type="number" min="0" max="100" step="any" placeholder="仅诊断，不推翻速率判定"></label>
        </div>
      </details>
    </fieldset>

    <section v-if="visible('run')" id="inner-flow-run" class="inner-card" aria-labelledby="inner-run-title">
      <h3 id="inner-run-title">执行</h3>

      <section v-if="!staged || !showProgress" aria-label="计划预览">
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
            <strong>{{ inner.preview.legs }}</strong> 个传输方向 · {{ inner.preview.links }} 个网口 ·
            约 {{ innerDuration(inner.preview.estimated_secs) }}
            <template v-if="inner.preview.resumed"> · RESUME 跳过 {{ inner.preview.resumed }}</template>
            · 参与：{{ [inner.preview.uses_master ? '本机' : '', ...inner.preview.agents].filter(Boolean).join('、') || '无' }}
          </p>
          <p v-if="inner.preview.skipped.length" class="hint">未参与：{{ inner.preview.skipped.join('、') }}</p>
          <details>
            <summary>逐单元清单（{{ inner.preview.rows.length }} 行）</summary>
            <div class="preview-list">
              <p v-for="row in inner.preview.rows" :key="row.index">
                <strong>#{{ row.index }}</strong><span v-if="row.origin"> {{ row.origin.group }} / {{ row.origin.suite }} / {{ row.origin.task }} · 第 {{ row.round }} 遍 · </span> {{ row.host }} / {{ row.link }} · IPv{{ row.ip_version ?? 4 }} · {{ PROTOCOL_LABEL[row.protocol] }} ·
                {{ DIRECTION_LABEL[row.direction] }} · 第 {{ row.repeat }} 轮 · {{ MEASUREMENT_LABEL[row.measurement] }}<span v-if="row.resumed"> · RESUME 跳过</span><br>
                <span v-for="leg in row.legs" :key="leg.flow" class="muted">
                  {{ FLOW_LABEL[leg.flow] }} → 接收端 {{ leg.receiver }}，端口 {{ leg.port }}；
                </span>
                <span class="muted">{{ row.parameter_label }} · {{ row.verdict_basis }}</span>
              </p>
            </div>
          </details>
        </template>
      </section>

      <section aria-label="内环执行与结果">
        <div v-if="staged && !showProgress" class="toolbar"><label class="field">整计划轮次<input v-model.number="inner.config.rounds" type="number" min="1" max="100" :disabled="locked"></label><label class="check"><input v-model="inner.config.resume" type="checkbox" :disabled="locked">跳过 24 小时内已通过的单元（RESUME）</label><label class="check"><input v-model="inner.config.screenshot" type="checkbox" :disabled="locked">每个吞吐单元后截图</label></div>
        <details v-if="(!staged || !showProgress) && overrides"><summary>临时覆盖与诊断</summary><fieldset :disabled="locked" class="inner-grid"><label>本轮强制 TCP -w<input v-model="overrides.tcp_window" placeholder="留空不覆盖"></label><label>本轮强制 UDP 每流 Mbps<input v-model.number="overrides.udp_mbps" type="number" min="0" step="any" placeholder="留空不覆盖"></label><label class="inner-check"><input v-model="overrides.probe_during_traffic" type="checkbox">测负载下时延</label><label class="inner-check"><input v-model="overrides.probe_path_mtu" type="checkbox">探路径 MTU</label><label class="inner-check"><input v-model="overrides.limit_udp_by_link_speed" type="checkbox">按链路上限裁剪 UDP 发送速率</label></fieldset></details>
        <p class="hint" role="status">{{ startBlocked || `将测试 ${enabledLinks} 个网口；未填门限的网口只记录速率。` }}</p>
        <div class="bar">
          <button v-if="!staged || !showProgress" class="primary" :disabled="!!startBlocked" @click="begin">开始内环测试</button>
          <button v-if="!staged || !showProgress" class="ghost" :disabled="!!startBlocked" @click="startScenario">先跑子网，再跑内环（RESUME）</button>
          <button v-if="inner.status.running" class="ghost" :disabled="inner.status.current_unit == null || inner.status.skip_requested" @click="skipInnerUnit">{{ inner.status.skip_requested ? '正在跳过…' : '跳过当前单元' }}</button>
          <button v-if="!staged || showProgress" class="ghost danger" :disabled="inner.busy || (!inner.status.running && !inner.scenario.running && inner.synced)" @click="stopCurrent">停止测试</button>
          <button v-if="!staged || showProgress" class="ghost" :disabled="!inner.status.has_report" @click="report">下载内环报告</button>
          <button v-if="!staged || showProgress" class="ghost" :disabled="!inner.status.has_report" @click="excel">下载 Excel</button>
          <button v-if="staged && showProgress && !inner.status.running && !scenarioBlocksActions() && inner.synced" class="ghost" @click="review">准备下一轮</button>
          <span v-if="!staged || showProgress" class="status" aria-live="polite">
            {{ inner.startPhase === 'unknown' ? '启动结果未确认' : inner.startPhase === 'sending' ? '启动中' : inner.scenarioStartPhase === 'unknown' ? '启动结果未确认' : inner.scenario.running ? '组合场景运行中' : inner.status.running ? '运行中' : inner.synced ? '空闲 / 已结束' : '待同步' }} ·
            {{ inner.status.completed }} / {{ inner.status.total }} 个单元
          </span>
        </div>
        <div v-if="inner.startPhase === 'unknown'" class="msg warn" role="alert"><p>启动结果未确认，请勿重复启动。核实未运行后重新准备。</p><button class="ghost small" @click="syncInnerStatus">再同步一次运行状态</button><button v-if="inner.synced && !inner.status.running" class="ghost small" @click="prepareAfterUnknownInner">已在主控确认没有运行，重新准备</button></div>
        <p v-if="(!staged || showProgress) && inner.status.current" aria-live="polite">{{ inner.status.current }}</p>
        <div v-if="inner.scenarioStartPhase === 'unknown'" class="msg warn" role="status">
          <p>启动结果未确认，正在查询。请勿重复启动。</p>
          <button v-if="inner.scenarioLastReadIdle" class="ghost small" @click="prepareAfterUnknownScenario">已核实测试未运行，重新准备</button>
        </div>
        <p v-if="inner.scenario.running" class="msg" aria-live="polite">组合场景：{{ inner.scenario.phase === 'subnet' ? '正在执行子网测试' : inner.scenario.phase === 'inner' ? '子网完成，正在执行内环' : inner.scenario.phase }}</p>
        <p v-if="(!staged || showProgress) && inner.status.error" class="msg bad" role="alert">{{ inner.status.error }}</p>
        <InnerResults v-if="!staged || showProgress" />
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
