<script setup lang="ts">
import { computed, nextTick, onMounted, ref } from 'vue';
import {
  inner, importInner, innerReport, innerRunReport, listInnerRuns, loadInnerDraft, loadInnerRunConfig,
  probeInner, refreshInnerPlan, startInner, startSubnetThenInner, stopInner, stopScenario, syncInnerStatus,
  listScenarioRuns, loadScenario, syncScenarioStatus,
} from '../../state/inner';
import {
  DIRECTION_LABEL, FLOW_LABEL, INNER_DIRECTIONS, INNER_IP_VERSIONS, INNER_PROTOCOLS, MEASUREMENT_LABEL, PROTOCOL_LABEL,
  innerDuration, innerHistoryStatus, normalizeInnerDraft, serializeInnerProject,
} from '../../domain/inner';
import type { InnerLink } from '../../domain/inner';
import { innerBoardInterfaces, innerSetupIssues, type InnerSetupIssue } from '../../domain/inner-setup';
import { errorMessage } from '../../api/client';
import InnerLinkTable from './InnerLinkTable.vue';
import InnerLinkDetail from './InnerLinkDetail.vue';
import InnerResults from './InnerResults.vue';

const props = defineProps<{ subnetRunning?: boolean }>();
const emit = defineEmits<{ (e: 'show-subnet-progress'): void }>();
const fileInput = ref<HTMLInputElement>();
const editing = ref<InnerLink | null>(null);
const locked = computed(() => props.subnetRunning || inner.busy || inner.status.running || inner.scenario.running || !inner.synced);
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
  if (props.subnetRunning) return '子网测试正在运行，请先等待它结束或在进度页请求停止。';
  if (inner.status.running || inner.scenario.running) return '测试正在运行，请等待结束或停止当前测试。';
  if (!inner.synced) return '先同步运行状态，确认没有其他测试正在执行。';
  if (inner.busy) return '正在处理当前操作，请稍候。';
  if (setupIssues.value.length) return '先处理上方缺项，再复核计划。';
  if (!inner.preview || inner.previewStale) return '正在等待当前配置的预览；可点击「刷新预览」。';
  if (!inner.preview.units) return '当前计划没有可执行单元，请检查参与网口。';
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
function download(name: string, body: string, type: string): void {
  const url = URL.createObjectURL(new Blob([body], { type }));
  const a = document.createElement('a'); a.href = url; a.download = name; a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
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
  try { const result = await innerReport(); download(result.name, result.html, 'text/html;charset=utf-8'); }
  catch (e) { inner.error = errorMessage(e); }
}
async function historyReport(id: string): Promise<void> {
  try { const result = await innerRunReport(id); download(result.name, result.html, 'text/html;charset=utf-8'); }
  catch (e) { inner.error = errorMessage(e); }
}
async function startScenario(): Promise<void> {
  try { await startSubnetThenInner(); }
  catch (e) { inner.error = errorMessage(e); }
}
async function loadScenarioRun(id: string): Promise<void> {
  try { await loadScenario(id); }
  catch (e) { inner.error = errorMessage(e); }
}
async function reloadConfig(id: string): Promise<void> {
  try { await loadInnerRunConfig(id); editing.value = null; }
  catch (e) { inner.error = errorMessage(e); }
}
const megabytes = (bytes: number) => `${(bytes / 1048576).toFixed(1)} MB`;
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

onMounted(() => { loadInnerDraft(); void syncInnerStatus(); void syncScenarioStatus(); void refreshInnerPlan(); void listInnerRuns(); void listScenarioRuns(); });
</script>

<template>
  <section class="view inner-view">
    <div class="view-head">
      <div>
        <h2>内环测试</h2>
        <p>逐个测电脑网口与 CPE 之间的收发速率。本机可以独立完成，辅测机按需添加。</p>
      </div>
      <div class="bar">
        <button :disabled="locked" @click="fileInput?.click()">导入内环配置</button>
        <button @click="download('cpe-inner-project.json', serializeInnerProject(normalizeInnerDraft(inner.config)), 'application/json')">导出内环配置</button>
        <input ref="fileInput" type="file" accept=".json,application/json" hidden @change="importFile">
      </div>
    </div>
    <section class="wiring" aria-labelledby="inner-wiring-title">
      <h3 id="inner-wiring-title">先接好线，再选择要测的网口</h3>
      <div class="wiring-path"><strong>主控电脑</strong><span>USB / ADB 控制连接</span><strong>CPE</strong></div>
      <div class="wiring-path"><strong>电脑上要测的网口</strong><span>网线接 LAN；Wi-Fi 连 CPE；RNDIS 选对应接口</span><strong>CPE LAN 地址</strong></div>
      <p>ADB 接在主控电脑，用于控制 CPE；测试数据走下面选中的网卡。每个电脑网卡都要有本轮所选 IPv4 / IPv6 地址，并能访问 CPE 对应 LAN 地址。辅测机只在它的网口参与本轮时使用。</p>
    </section>
    <p class="inner-note">操作顺序：检查 ADB 和扫描网卡 → 勾选实际接到 CPE 的网卡 → 核对 CPE LAN 地址 → 复核计划并开始。内环配置与子网配置分别保存，运行中的测试结束后才能切换。</p>
    <p v-if="!inner.draftSaved" class="muted" role="status">
      当前配置尚未保存到草稿。请补齐网卡和所选 IP 版本的两端地址；若配置完整，请检查浏览器存储是否可用。现在刷新会回到默认配置。
    </p>
    <p v-if="inner.error" class="bad" role="alert">{{ inner.error }}</p>
    <p v-if="props.subnetRunning" class="inner-note" role="status">子网测试正在占用网口，结束后才能开始内环测试。<button @click="emit('show-subnet-progress')">查看子网进度</button></p>
    <div v-if="!inner.synced" class="bar"><span class="warn">运行状态尚未确认</span><button @click="syncInnerStatus">重新同步</button></div>

    <fieldset ref="devicePanel" :disabled="locked" class="inner-card" tabindex="-1">
      <legend>1 · 检查设备，扫描网卡</legend>
      <p>确认 CPE 已通过 ADB 接到主控，点击扫描。单台设备可自动识别，程序路径与辅测机在下方按需设置。</p>
      <div class="bar inner-actions"><button class="primary" @click="probeInner">{{ inner.busy ? '检查与扫描中…' : '检查 ADB / 扫描各电脑网卡' }}</button><span class="muted">扫描不启动测试，也不修改电脑网络设置。</span></div>
      <details ref="deviceSettings" class="advanced-settings">
        <summary>ADB 与辅测机设置{{ inner.capability ? `（已识别 ${inner.capability.serial}）` : '' }}</summary>
        <p>ADB 设备接在主控电脑；其他电脑运行同版本 <code>cpe_test agent</code>，并填写可从主控访问的地址。没有辅测机时本机也能独立测试。</p>
        <div class="inner-grid">
          <label>主控 ADB 程序路径<input v-model="inner.config.adb_path" placeholder="adb"></label>
          <label>ADB 序列号（单设备可留空）<input v-model="inner.config.serial" placeholder="自动选择唯一设备"></label>
          <label>板侧 iperf3 路径<input v-model="inner.config.board_iperf" placeholder="iperf3"></label>
        </div>
        <div v-for="(agent, index) in inner.config.agents" :key="index" class="agent-row inner-grid">
          <label>辅测机标识<input v-model="agent.id" :readonly="inner.config.links.some(l => l.host === agent.id)"></label>
          <label>地址<input v-model="agent.address" placeholder="192.168.8.101"></label>
          <label>端口<input v-model.number="agent.port" type="number" min="1" max="65535"></label>
          <label>令牌（可空，不导出）<input v-model="agent.token" type="password" autocomplete="off"></label>
          <button :disabled="inner.config.links.some(l => l.host === agent.id)" @click="inner.config.agents.splice(index, 1)">移除辅测机</button>
        </div>
        <div class="bar inner-actions">
          <button :disabled="inner.config.agents.length >= 8" @click="addAgent">添加辅测机</button>
        </div>
      </details>
      <p v-if="inner.busy">正在处理，请稍候……</p>
      <div v-if="inner.capability" class="host-status">
        <span class="ok">ADB 已识别 CPE：{{ inner.capability.serial }}</span>
        <span v-for="agent in inner.capability.agents" :key="agent.id" :class="agentTone(agent.status)">
          {{ agent.id }}：{{ agentText(agent.status) }}<template v-if="agent.error"> — {{ agent.error }}</template>
        </span>
      </div>
      <p v-if="inner.capability?.agents.some(a => a.status === 'failed')" class="muted">
        连接失败的辅测机不会删除配置。只要没有网口勾选它，本轮照样可以只跑本机。
      </p>
      <template v-if="inner.capability">
        <p v-if="inner.capability.board_inventory_error" class="warn" role="status">板侧接口信息不完整：{{ inner.capability.board_inventory_error }}。请核对统计接口后重新扫描。</p>
        <p v-if="boardInterfaces.length" class="muted">板侧 LAN / 本轮统计接口：<span v-for="(iface, index) in boardInterfaces" :key="iface.name">{{ index ? '；' : '' }}{{ iface.name }} · {{ [...iface.addresses, ...(iface.ipv6_addresses ?? [])].join('、') || '无独立 IP' }}</span></p>
        <p v-else class="warn">未发现 192.168.* 的板侧 LAN 地址或本轮统计接口，请展开完整清单核对设备地址。</p>
      </template>
      <details v-if="inner.capability"><summary>查看板侧全部系统接口（{{ inner.capability.board_interfaces.length }} 个）</summary>
        <p class="muted">包含桥、桥成员、WAN、回环和虚拟接口。这里是 CPE 的系统清单，实际参与测试的电脑网口以下方勾选为准。</p>
        <pre>{{ inner.capability.board_version }}</pre>
        <div class="board-inventory"><table>
          <thead><tr><th scope="col">系统接口</th><th scope="col">IPv4 / IPv6</th><th scope="col">所属桥 / 成员</th></tr></thead>
          <tbody><tr v-for="iface in inner.capability.board_interfaces" :key="iface.name"><td>{{ iface.name }}</td><td>{{ [...iface.addresses, ...(iface.ipv6_addresses ?? [])].join('、') || '—' }}</td><td>{{ iface.master || '—' }}<template v-if="iface.members.length"> / {{ iface.members.join('、') }}</template></td></tr></tbody>
        </table></div>
      </details>
    </fieldset>

    <fieldset ref="linksPanel" :disabled="locked" class="inner-card" tabindex="-1">
      <legend>2 · 选择本轮网口，核对地址</legend>
      <p class="muted">
        板侧 LAN 地址填这条链路访问的板侧地址，不是板侧 WAN 默认网关。取消勾选只是这一轮不跑它，参数一个字节都不会丢。
      </p>
      <InnerLinkTable :hosts="hosts" :editing="editing" :disabled="locked" @edit="editLink" />
      <div v-if="editing" ref="detailPanel" class="detail-panel" tabindex="-1"><InnerLinkDetail :link="editing" :hosts="hosts" :disabled="locked" @close="editing = null" /></div>
    </fieldset>

    <fieldset ref="paramsPanel" :disabled="locked" class="inner-card" tabindex="-1">
      <legend>3 · 打流参数</legend>
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
          <legend>协议（可多选，同一轮依次跑）</legend>
          <label v-for="item in INNER_PROTOCOLS" :key="item" class="inner-check">
            <input type="checkbox" :checked="inner.config.protocols.includes(item)"
              :disabled="inner.config.protocols.length === 1 && inner.config.protocols.includes(item)"
              :title="inner.config.protocols.length === 1 && inner.config.protocols.includes(item) ? '至少要保留一个协议' : undefined"
              @change="inner.config.protocols = toggle(inner.config.protocols, item, ($event.target as HTMLInputElement).checked)">
            {{ PROTOCOL_LABEL[item] }}
          </label>
        </fieldset>
        <fieldset class="inner-choice">
          <legend>方向（可多选）</legend>
          <label v-for="item in INNER_DIRECTIONS" :key="item" class="inner-check">
            <input type="checkbox" :checked="inner.config.directions.includes(item)"
              :disabled="inner.config.directions.length === 1 && inner.config.directions.includes(item)"
              :title="inner.config.directions.length === 1 && inner.config.directions.includes(item) ? '至少要保留一个方向' : undefined"
              @change="inner.config.directions = toggle(inner.config.directions, item, ($event.target as HTMLInputElement).checked)">
            {{ DIRECTION_LABEL[item] }}
          </label>
        </fieldset>
        <label>每个方向测试时长（秒）<input v-model.number="inner.config.duration_secs" type="number" min="6" max="3600"></label>
        <label>每个单元重复轮次<input v-model.number="inner.config.repeats" type="number" min="1" max="10"></label>
        <label v-if="udp">UDP 每条流发送速率（Mbps，必填）<input v-model.number="inner.config.udp_mbps" type="number" min="0.01" step="any" placeholder="填写希望发送的速率"></label>
        <label class="inner-check"><input v-model="inner.config.resume" type="checkbox">恢复重跑时启用 RESUME<small class="muted">跳过 24 小时内已 PASS 的同一单元</small></label>
      </div>
      <p class="direction-guide"><strong>上行：</strong>电脑 → CPE，看 CPE 接收速率。<strong>下行：</strong>CPE → 电脑，看所选网卡接收速率。</p>
      <details ref="advancedParams" class="advanced-settings">
        <summary>高级打流参数：并行流数、窗口、报文与端口</summary>
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
      <p class="muted">
        「上行 + 下行」是两个独立的单向单元；「双向并发」是一个同时跑上下行两条腿的单元。两次顺序单向不等于双向并发，
        三项可以同时勾选，各自出结果。重复轮次只是多跑几遍并各留一条记录，不会因为没达标就自动重跑。
      </p>
    </fieldset>

    <section class="inner-card" aria-label="计划预览">
      <div class="bar">
        <h3>4 · 检查本轮计划</h3>
        <button :disabled="locked || !!setupIssues.length" @click="refreshInnerPlan">刷新预览</button>
        <span v-if="inner.previewStale" class="warn" aria-live="polite">配置已改动，预览待刷新</span>
      </div>
      <div v-if="setupIssues.length" class="setup-check" role="status">
        <strong>还需完成 {{ setupIssues.length }} 项</strong>
        <ul><li v-for="(issue, index) in setupIssues" :key="index"><span>{{ issue.message }}</span><button :disabled="locked" @click="fixIssue(issue)">{{ issue.action }}</button></li></ul>
      </div>
      <p v-else class="muted">配置已填写完整。启动时会检查设备和链路；扫描结果与计划预览不代表测试已通过。</p>
      <p v-if="inner.previewError && !setupIssues.length" class="bad" role="alert">{{ inner.previewError }}</p>
      <template v-if="inner.preview">
        <p>
          本轮 <strong>{{ inner.preview.units }}</strong> 个测试单元（其中双向并发 {{ inner.preview.bidir_units }} 个）、
          共 <strong>{{ inner.preview.legs }}</strong> 条数据腿，覆盖 {{ inner.preview.links }} 条网口，
          预估 {{ innerDuration(inner.preview.estimated_secs) }}。
          <span v-if="inner.preview.resumed">其中 {{ inner.preview.resumed }} 个单元将按 RESUME 跳过。</span>
          参与的电脑：{{ [inner.preview.uses_master ? '本机' : '', ...inner.preview.agents].filter(Boolean).join('、') || '无' }}。
        </p>
        <p v-if="inner.preview.skipped.length" class="muted">
          未参与本轮：{{ inner.preview.skipped.join('、') }}（配置已保留，勾上即可加入）。
        </p>
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
      <p v-else-if="!inner.previewError" class="muted">填完设备和网口后这里会显示本轮到底要跑什么。</p>
    </section>

    <section class="inner-card" aria-label="内环执行与结果">
      <h3>5 · 开始测试，查看结果</h3>
      <p v-if="startBlocked" class="muted" role="status">{{ startBlocked }}</p>
      <p v-else class="muted">将按上方清单测试 {{ enabledLinks }} 个网口。未填写验收门限时只记录速率，可在网口的高级设置中填写门限。</p>
      <div class="bar">
        <button class="primary" :disabled="!!startBlocked" @click="startInner">开始内环测试</button>
        <button :disabled="!!startBlocked" @click="startScenario">先跑子网，再跑内环（RESUME）</button>
        <button :disabled="inner.busy || (!inner.status.running && !inner.scenario.running && inner.synced)" @click="inner.scenario.running ? stopScenario() : stopInner">停止测试</button>
        <button :disabled="!inner.status.has_report" @click="report">下载内环报告</button>
        <span aria-live="polite">
          {{ inner.status.running ? '运行中' : inner.synced ? '空闲 / 已结束' : '待同步' }} ·
          {{ inner.status.completed }} / {{ inner.status.total }} 个单元
        </span>
      </div>
      <p v-if="inner.status.current" aria-live="polite">{{ inner.status.current }}</p>
      <p v-if="inner.scenario.running" class="notice" aria-live="polite">组合场景进行中：{{ inner.scenario.phase === 'subnet' ? '先执行子网测试' : inner.scenario.phase === 'inner' ? '子网完成，正在执行内环' : inner.scenario.phase }}</p>
      <p v-if="inner.status.error" class="bad" role="alert">{{ inner.status.error }}</p>
      <p class="muted">
        上行的接收端在板侧，下行的接收端是网口所在电脑；接收端按数据走向定，不按谁跑 client。
        速率优先取可信的接收接口字节计数；只有该链路选了兜底策略、且计数不可用或已判不可信时才改用工具 receiver 汇总，
        并在「来源」列标出来。工具口径的门限单独配置，工具数字不会套用网卡门限，可信的低速也不会被兜底救成达标。
      </p>
      <InnerResults />
    </section>

    <section class="inner-card" aria-label="子网内环组合场景历史">
      <div class="bar"><h3>子网→内环场景历史</h3><button @click="listScenarioRuns">刷新列表</button></div>
      <p class="muted">组合记录同时保存子网计划和内环配置；载入后会把两份配置恢复并默认打开两段 RESUME。</p>
      <div v-if="inner.scenario.runs.length" class="history">
        <table><thead><tr><th>时间</th><th>状态</th><th>RESUME</th><th>操作</th></tr></thead>
          <tbody><tr v-for="run in inner.scenario.runs" :key="run.id"><td>{{ run.created_at || run.id }}</td><td>{{ run.error || run.phase }}</td><td>子网 {{ run.resume_subnet ? '开' : '关' }} / 内环 {{ run.resume_inner ? '开' : '关' }}</td><td><button :disabled="locked" @click="loadScenarioRun(run.id)">载入并恢复重跑</button></td></tr></tbody>
        </table>
      </div>
      <p v-else class="muted">还没有组合场景记录。</p>
    </section>

    <section class="inner-card" aria-label="内环历史">
      <div class="bar">
        <h3>内环历史</h3>
        <button @click="listInnerRuns">刷新列表</button>
        <span class="muted">保存在 inner_runs/，与子网历史各走各的。</span>
      </div>
      <p v-if="!inner.runs.length" class="muted">还没有历史记录。跑完一轮后这里会列出报告，可下载或把当时的配置装载回来。</p>
      <div v-else class="history">
        <table>
          <thead><tr>
            <th scope="col">时间</th><th scope="col">网口</th><th scope="col" class="num">单元</th>
            <th scope="col" class="num">PASS</th><th scope="col" class="num">RATE_FAIL</th>
            <th scope="col" class="num">NOT_EVALUATED</th><th scope="col">备注</th>
            <th scope="col" class="num">大小</th><th scope="col">操作</th>
          </tr></thead>
          <tbody>
            <tr v-for="run in inner.runs" :key="run.id">
              <td>{{ run.created_at || run.id }}</td>
              <td>{{ run.links.join('、') || '—' }}</td>
              <td class="num">{{ run.units }}</td>
              <td class="num">{{ run.passed }}</td>
              <td class="num">{{ run.rate_failed }}</td>
              <td class="num">{{ run.not_evaluated }}</td>
              <td>
                <span v-if="run.error" class="bad">{{ run.error }}</span>
                <span v-else>{{ innerHistoryStatus(run) }}</span>
              </td>
              <td class="num">{{ megabytes(run.bytes) }}</td>
              <td class="bar">
                <button :disabled="!run.has_report" @click="historyReport(run.id)">下载报告</button>
                <button :disabled="locked || !run.has_config" @click="reloadConfig(run.id)">恢复重跑（RESUME）</button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <p class="muted">
        「装载配置」只把当时的配置放回控制台并重新生成预览，不会直接开跑：隔了一夜的网口拓扑可能已经变了，
        该看到的是预览里的差异。辅测机令牌不写进历史文件，装载后需要重填。重跑会生成新的一条记录。
      </p>
    </section>
  </section>
</template>

<style scoped>
.inner-view { max-width: 1400px; }
.wiring { padding: 18px 20px; border: 1px solid var(--line); border-left: 4px solid var(--accent); background: var(--surface); }
.wiring h3 { margin: 0 0 14px; font-size: 16px; }
.wiring-path { display: grid; grid-template-columns: minmax(140px, 1fr) minmax(240px, 2fr) minmax(130px, 1fr); align-items: center; gap: 16px; margin: 12px 0; font-size: 13px; }
.wiring-path > span { text-align: center; padding: 6px; border-bottom: 2px solid var(--line); color: var(--muted); }
.wiring-path > strong:last-child { text-align: right; }
.wiring p { margin-bottom: 0; max-width: 78ch; color: var(--muted); font-size: 13px; line-height: 1.7; }
.advanced-settings { margin-top: 18px; padding-top: 14px; border-top: 1px solid var(--line); }
.advanced-settings > summary { cursor: pointer; margin-bottom: 14px; font-size: 13px; }
.direction-guide { padding: 12px 0; line-height: 1.8; }
.setup-check { margin-top: 16px; padding: 14px 16px; background: var(--panel-2); border-left: 3px solid var(--warn); }
.setup-check ul { list-style: none; padding: 0; margin: 8px 0 0; }
.setup-check li { display: flex; align-items: center; justify-content: space-between; gap: 16px; padding: 8px 0; font-size: 13px; }
.setup-check button { flex-shrink: 0; }
.inner-note { border-left: 3px solid var(--accent); padding: 10px 14px; background: var(--panel-2); }
.inner-card { min-width: 0; margin: 18px 0; padding: 20px; border: 1px solid var(--line); border-radius: 8px; background: var(--surface); }
.inner-card legend, .inner-card h3 { font-weight: 700; padding: 0 8px; margin: 0; }
.inner-card p { color: var(--muted); }
.inner-card p.bad { color: var(--bad); }
.inner-grid { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 14px; }
.inner-grid label { display: flex; flex-direction: column; gap: 6px; min-width: 0; font-size: 13px; }
.inner-grid input, .inner-grid select { width: 100%; min-width: 0; }
.agent-row { padding: 16px; margin: 16px 0; border: 1px solid var(--line); border-radius: 6px; background: var(--panel-2); }
.agent-row button { align-self: end; justify-self: start; }
.inner-actions { margin-top: 16px; }
.detail-panel { margin-top: 16px; }
.host-status { display: flex; flex-wrap: wrap; gap: 8px 20px; margin-top: 12px; font-size: 13px; }
.host-status .ok { color: var(--ok, #1a7f37); }
.host-status .bad { color: var(--bad); }
.host-status .muted { color: var(--muted); }
.inner-choice { min-width: 0; padding: 8px 12px 12px; border: 1px solid var(--line); border-radius: 6px; }
.inner-choice legend { font-size: 13px; padding: 0 6px; }
.inner-check { display: inline-flex; flex-direction: row; align-items: center; gap: 6px; margin-right: 16px; font-size: 13px; }
.inner-check input { width: auto; }
.preview-list { max-height: 20rem; overflow-y: auto; font-size: 13px; }
.history { overflow-x: auto; max-height: 22rem; overflow-y: auto; }
.board-inventory { max-height: 18rem; overflow: auto; }
.board-inventory table { width: 100%; border-collapse: collapse; font-size: 13px; }
.board-inventory th, .board-inventory td { padding: 7px 10px; text-align: left; border-bottom: 1px solid var(--line); }
.board-inventory thead th { position: sticky; top: 0; background: var(--head); }
.history table { width: 100%; border-collapse: collapse; font-size: 13px; }
.history th, .history td { text-align: left; padding: 8px 10px; border-bottom: 1px solid var(--line); white-space: nowrap; }
.history thead th { position: sticky; top: 0; z-index: 1; background: var(--head); }
.history td.num, .history th.num { text-align: right; }
.history td.bar { display: table-cell; }
.history td.bar button { margin-right: 4px; }
.history .bad { color: var(--bad); }
.muted { color: var(--muted); font-size: 12px; }
pre { white-space: pre-wrap; overflow-wrap: anywhere; }
@media (max-width: 1000px) { .inner-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
@media (max-width: 650px) {
  .inner-grid { grid-template-columns: 1fr; }
  .inner-card { padding: 14px; }
  .wiring-path { grid-template-columns: 1fr; gap: 3px; }
  .wiring-path > span { text-align: left; }
  .wiring-path > strong:last-child { text-align: left; }
  .setup-check li { align-items: flex-start; flex-direction: column; gap: 8px; }
}
@media (max-width: 650px) { .inner-grid { grid-template-columns: 1fr; } .inner-card { padding: 14px; } }
</style>
