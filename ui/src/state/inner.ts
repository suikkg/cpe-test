import { reactive, watch } from 'vue';
import { api, errorMessage, NetworkError, UnauthorizedError } from '../api/client';
import { defaultInnerConfig, INNER_DRAFT_KEY, innerLink, mergeInnerUnits, normalizeInnerDraft, parseInnerProject, serializeInnerProject } from '../domain/inner';
import type { InnerCapability, InnerLink, InnerPreview, InnerRunEntry, InnerStatus, InnerStatusDelta } from '../domain/inner';
import { innerNicChoices, linksFromInnerChoices } from '../domain/inner-setup';
import { buildRunRequest, invalidatePreview, plan as subnetPlan, preview as previewSubnet, previewIsCurrent, adoptRunRequest } from './plan';
import { goto } from './ui';
import { session } from './session';

export const inner = reactive({
  config: { ...defaultInnerConfig(), ip_versions: [4, 6] as (4 | 6)[] }, capability: null as InnerCapability | null,
  status: { running: false, current: '', error: null, completed: 0, total: 0, units: [], has_report: false } as InnerStatus,
  /** 计划预览由后端产出；配置一改先标记失效，避免拿旧预览去开跑。 */
  preview: null as InnerPreview | null, previewStale: true, previewError: '',
  /** 内环历史，独立于子网的 runs/。 */
  runs: [] as InnerRunEntry[],
  synced: false, busy: false, error: '',
  scenarioStartPhase: 'idle' as 'idle' | 'sending' | 'accepted' | 'unknown',
  scenarioLastReadIdle: false,
  /** 当前配置是否已存进草稿。填到一半（还缺网卡/IP）时存不下，界面要说出来。 */
  draftSaved: true,
  scenario: {
    running: false, id: '', phase: '', error: null as string | null,
    runs: [] as Array<{ id: string; created_at: string; finished: boolean; phase: string; error: string | null; resume_subnet: boolean; resume_inner: boolean }>,
  },
});
let timer: ReturnType<typeof setTimeout> | undefined;
let planTimer: ReturnType<typeof setTimeout> | undefined;
let inFlight: Promise<void> | null = null;
let loaded = false;
let planRequest = 0;
let probeRequest = 0;
let scenarioTimer: ReturnType<typeof setTimeout> | undefined;
// 场景状态请求可能跨越一次启动命令；只允许最后发出的那一发改写本地状态。
// 否则页面初次加载的旧 GET 在启动响应之后返回，会把 running=true 覆盖回 false。
let scenarioRequest = 0;

/**
 * 尚未确认的那次组合场景启动所带的令牌；空串 = 没有待确认的启动。
 *
 * 启动请求没拿到应答时，只能靠回读状态判断它有没有起跑。以前拿「状态里的
 * 场景 ID ≠ 开始前记下的 ID」判断：页面初次读状态失败时记下的是空串，任何
 * 一个旧场景的 ID 都会被当成「这次起来了」，锁随之解开，而那条请求可能还在
 * 排队。现在只认状态里带回的、自己发出的这枚令牌。
 */
let pendingStartToken = '';

/**
 * 一次性的随机令牌，32 位十六进制。
 *
 * 不用 `crypto.randomUUID()`：用 `--ui-bind` 远程打开的明文 HTTP 页面不是安全
 * 上下文，没有这个函数；`getRandomValues` 两种上下文都有。
 */
function newStartToken(): string {
  const bytes = new Uint8Array(16);
  globalThis.crypto.getRandomValues(bytes);
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}

export function scenarioBlocksActions(): boolean {
  return inner.scenario.running || inner.scenarioStartPhase === 'sending' || inner.scenarioStartPhase === 'unknown';
}

/**
 * 操作员确认重新准备；只清本地准备态，绝不重发启动。
 *
 * 子网预览和内环预览**一起**作废：组合场景同时用两份计划，只作废内环那份的话，
 * 下一次启动会拿事故前的子网 `plan_hash` 直接重开——子网自己的
 * `prepareAfterUnknownStart` 早就要求重新预览了。
 */
export function prepareAfterUnknownScenario(): void {
  if (inner.scenarioStartPhase !== 'unknown' || !inner.scenarioLastReadIdle) return;
  scenarioRequest++;
  if (scenarioTimer !== undefined) clearTimeout(scenarioTimer);
  scenarioTimer = undefined;
  pendingStartToken = '';
  inner.scenarioStartPhase = 'idle';
  inner.scenarioLastReadIdle = false;
  inner.previewStale = true;
  invalidatePreview();
  inner.error = '';
}

/** 扫描结果只描述当时的设备和电脑连接；测试参数不影响这份身份。 */
function probeConnection() {
  return {
    adb_path: inner.config.adb_path,
    serial: inner.config.serial,
    board_iperf: inner.config.board_iperf,
    agents: inner.config.agents.map((agent) => ({ ...agent, address: agent.address.trim() })),
  };
}
/** 能力快照与待添加选择共用连接身份；普通重扫不改变身份。 */
export function innerProbeIdentity(): string {
  return JSON.stringify(probeConnection());
}
watch(innerProbeIdentity, () => {
  // 同步失效也防住「改了又改回」：旧请求属于上一次连接，不能重新填回就绪状态。
  probeRequest++;
  inner.capability = null;
}, { flush: 'sync' });

export function loadInnerDraft(): void {
  if (loaded) return;
  loaded = true;
  try {
    const stored = localStorage.getItem(INNER_DRAFT_KEY);
    if (stored) inner.config = parseInnerProject(stored);
  } catch (e) { inner.error = `内环草稿未恢复：${errorMessage(e)}`; }
  watch(() => inner.config, () => {
    // 配置一动，上一份预览就不再描述「现在要跑什么」。
    inner.previewStale = true;
    if (planTimer !== undefined) clearTimeout(planTimer);
    planTimer = setTimeout(() => { void refreshInnerPlan(); }, 400);
    try {
      const text = serializeInnerProject(normalizeInnerDraft(inner.config));
      // 只存完整可跑的配置：半份配置存回去，下次开页面就会解析失败。
      parseInnerProject(text);
      localStorage.setItem(INNER_DRAFT_KEY, text);
      inner.draftSaved = true;
    }
    catch {
      // 两个原因以前落进同一个空 catch，标注却写着「存储不可用」：
      // `addInnerLink()` 造出来的新链路本来就缺网卡和 IP，校验必然抛错，于是
      // 从新增链路那一刻起**每一次编辑都存不下**，而 localStorage 里还压着
      // 上一份完整草稿。刷新一下，中间几十项修改凭空回退，界面全程没有任何提示。
      //
      // 现在把状态摆出来，并且**丢掉那份过期草稿**：存不下的时候，刷新应当回到
      // 默认配置，而不是无声无息地还原成另一份东西。
      inner.draftSaved = false;
      try {
        localStorage.removeItem(INNER_DRAFT_KEY);
      }
      catch { /* 存储真的不可用，这里也没别的可做。 */ }
    }
  }, { deep: true });
}

export function importInner(text: string): void {
  if (inner.busy || inner.status.running || scenarioBlocksActions() || !inner.synced) throw new Error('请等待状态同步或当前内环操作完成');
  inner.config = parseInnerProject(text);
  inner.capability = null;
  inner.preview = null;
  inner.previewStale = true;
  inner.error = '';
}

/**
 * 拉一份计划预览。
 *
 * 预览、执行器、进度和报告消费的是后端**同一个** plan 模块，页面不自己算
 * 笛卡尔积——否则「预览说 8 个单元、实际跑了 12 个」这种事迟早发生。
 */
export async function refreshInnerPlan(): Promise<void> {
  if (inner.status.running || scenarioBlocksActions()) return;
  const request = ++planRequest;
  const snapshot = JSON.stringify(normalizeInnerDraft(inner.config));
  const current = () => request === planRequest && !inner.status.running && !scenarioBlocksActions()
    && snapshot === JSON.stringify(normalizeInnerDraft(inner.config));
  inner.previewStale = true;
  try {
    const cfg = JSON.parse(snapshot);
    // 本地先过一遍同一套校验，配置还没填完时不必去打服务端。
    parseInnerProject(JSON.stringify(cfg));
    const preview = await api.post<InnerPreview>('/api/inner/plan', cfg);
    if (!current()) return;
    inner.preview = preview;
    inner.previewStale = false;
    inner.previewError = '';
  } catch (e) {
    if (!current()) return;
    inner.preview = null;
    inner.previewStale = true;
    inner.previewError = errorMessage(e);
  }
}

/** 响应落地后再安排下一拍；同一控制器只有一条状态链。 */
export function syncInnerStatus(): Promise<void> {
  if (inFlight) return inFlight;
  if (timer !== undefined) clearTimeout(timer);
  inFlight = readStatus().finally(() => { inFlight = null; });
  return inFlight;
}
async function readStatus(): Promise<void> {
  let unauthorized = false;
  try {
    // 带游标问：单元只增不减，每秒把整份越来越大的数组重传一遍没有意义。
    const delta = await api.get<InnerStatusDelta>(
      `/api/inner/status?units_from=${inner.status.units.length}&run_id=${encodeURIComponent(inner.status.run_id ?? '')}`,
      { timeoutMs: 10000 },
    );
    const { units_from: _from, ...rest } = delta;
    inner.status = { ...rest, units: mergeInnerUnits(inner.status.units, delta) };
    inner.synced = true;
  } catch (e) {
    inner.synced = false;
    inner.error = errorMessage(e);
    unauthorized = e instanceof UnauthorizedError;
  } finally {
    if (!unauthorized && (inner.status.running || !inner.synced)) timer = setTimeout(() => { void syncInnerStatus(); }, 1000);
  }
}
async function freshStatusAfterCommand(): Promise<void> {
  // 命令之前发出的旧状态不能充当命令结果；先等旧请求，再读一次。
  if (inFlight) await inFlight;
  await syncInnerStatus();
}

export async function probeInner(): Promise<void> {
  if (inner.busy || inner.status.running || scenarioBlocksActions()) return;
  const request = ++probeRequest;
  inner.busy = true; inner.error = ''; inner.capability = null;
  try {
    // 扫描端点仍校验完整配置，但探测只消费设备连接信息。测试参数可能尚未
    // 填完（例如刚勾上 UDP 还没填速率），不能让它们挡住发现网口的第一步。
    const cfg = {
      ...defaultInnerConfig(),
      ...probeConnection(),
    };
    const capability = await api.post<InnerCapability>('/api/inner/probe', cfg);
    if (request === probeRequest) inner.capability = capability;
  } catch (e) { if (request === probeRequest) inner.error = errorMessage(e); }
  finally { inner.busy = false; }
}

export async function startInner(): Promise<void> {
  if (inner.busy || inner.status.running || scenarioBlocksActions() || !inner.synced) return;
  inner.busy = true; inner.error = '';
  try {
    const cfg = normalizeInnerDraft(inner.config);
    parseInnerProject(JSON.stringify(cfg));
    if (!cfg.links.some((link) => link.enabled)) throw new Error('本轮没有勾选任何网口，请至少勾选一条再开始');
    await api.post('/api/inner/run', cfg);
    inner.status.running = true;
  } catch (e) {
    inner.error = errorMessage(e);
    if (e instanceof NetworkError) inner.synced = false;
  } finally { await freshStatusAfterCommand(); inner.busy = false; }
}

export async function stopInner(): Promise<void> {
  if (inner.busy || scenarioBlocksActions()) return;
  inner.busy = true; inner.error = '';
  try { await api.post('/api/inner/stop'); }
  catch (e) { inner.error = errorMessage(e); }
  finally { await freshStatusAfterCommand(); inner.busy = false; }
}

/** 按「子网→内环」顺序运行两段测试；两段各自使用自己的 RESUME 历史。 */
export async function startSubnetThenInner(): Promise<void> {
  if (inner.busy || inner.status.running || scenarioBlocksActions() || !inner.synced) return;
  const hash = subnetPlan.preview?.plan_hash;
  if (!hash || !previewIsCurrent()) throw new Error('请先在子网「执行」页预览当前计划，再启动子网→内环场景');
  if (!inner.preview || inner.previewStale) throw new Error('请先刷新内环预览，再启动子网→内环场景');
  const subnet = { ...buildRunRequest(), plan_hash: hash, resume: true };
  const innerCfg = normalizeInnerDraft(inner.config);
  parseInnerProject(JSON.stringify(innerCfg));
  if (!innerCfg.links.some((link) => link.enabled)) throw new Error('内环场景没有勾选任何网口');
  inner.busy = true; inner.error = '';
  const startToken = newStartToken();
  pendingStartToken = startToken;
  inner.scenarioStartPhase = 'sending';
  inner.scenarioLastReadIdle = false;
  scenarioRequest++;
  try {
    const out = await api.post<{ started: boolean; id: string }>('/api/scenario/run', {
      subnet, inner: innerCfg, resume_subnet: true, resume_inner: true, start_token: startToken,
    });
    pendingStartToken = '';
    inner.scenarioStartPhase = 'accepted';
    inner.scenario.running = true;
    inner.scenario.id = out.id;
    inner.scenario.phase = 'subnet';
    await syncScenarioStatus(true);
  } catch (e) {
    inner.error = errorMessage(e);
    // 场景启动是有副作用的：没有拿到响应不等于没有创建场景。只回读状态，
    // 不重发启动请求，避免网络抖动下跑出两条场景或把后端的互斥错误误当成新失败。
    // 如果这次回读也断在半路，本地仍是 running=false，普通状态链不会自行续
    // 轮询；保留一条延迟回读，避免后台场景继续跑而页面永远失去跟踪。
    inner.scenarioStartPhase = e instanceof NetworkError ? 'unknown' : 'idle';
    // 服务端明确拒绝时这次启动确定没发生，令牌作废；断线时留着等状态里带回来。
    if (!(e instanceof NetworkError)) pendingStartToken = '';
    await syncScenarioStatus(e instanceof NetworkError);
  } finally { inner.busy = false; }
}

export async function stopScenario(): Promise<void> {
  if (!inner.scenario.running) return;
  try { await api.post('/api/scenario/stop', {}); }
  catch (e) { inner.error = errorMessage(e); }
}

export async function syncScenarioStatus(retryWhenUnknown = false): Promise<void> {
  if (retryWhenUnknown && !inner.scenario.running && inner.scenarioStartPhase !== 'accepted') {
    inner.scenarioStartPhase = 'unknown';
  }
  const request = ++scenarioRequest;
  if (scenarioTimer !== undefined) {
    clearTimeout(scenarioTimer);
    scenarioTimer = undefined;
  }
  try {
    const status = await api.get<{
      running: boolean; id: string; phase: string; error: string | null; start_token?: string;
    }>('/api/scenario/status');
    if (request !== scenarioRequest) return;
    inner.scenarioLastReadIdle = !status.running;
    // 只有状态里带回的是**这一次**启动的令牌，才算确认起跑（不论此刻是否已跑完）。
    // 别的标签页起的场景在跑时保持未确认：`scenarioBlocksActions()` 照样锁着操作，
    // 它结束、读到空闲之后，仍由操作员核实后点「重新准备」。
    if (pendingStartToken && status.start_token === pendingStartToken) {
      pendingStartToken = '';
      inner.scenarioStartPhase = 'accepted';
    }
    inner.scenario.running = status.running;
    inner.scenario.id = status.id;
    inner.scenario.phase = status.phase;
    inner.scenario.error = status.error;
    if (status.error) inner.error = status.error;
    // 场景第二阶段仍由独立的内环控制器产出进度；场景轮询不能把内环轮询
    // 链掐掉，否则页面只会显示「组合场景进行中」，单元和当前腿永远不更新。
    void syncInnerStatus();
    if (status.running || inner.scenarioStartPhase === 'unknown') {
      scheduleScenarioStatus();
    }
  } catch (e) {
    if (request !== scenarioRequest) return;
    inner.error = errorMessage(e);
    // 场景状态也是响应落地后再排下一拍；一次临时网络错误不能让一个仍在
    // 后台执行的五小时场景永久失去 UI 进度。401 则交给统一的会话失效流程，
    // 不继续用错误口令轮询。
    inner.scenarioLastReadIdle = false;
    if (e instanceof UnauthorizedError) session.phase = 'unauthorized';
    else if (scenarioBlocksActions()) scheduleScenarioStatus();
  }
}

function scheduleScenarioStatus(): void {
  if (scenarioTimer !== undefined) clearTimeout(scenarioTimer);
  scenarioTimer = setTimeout(() => { void syncScenarioStatus(); }, 1000);
}

export async function listScenarioRuns(): Promise<void> {
  try { inner.scenario.runs = await api.get<typeof inner.scenario.runs>('/api/scenario/runs'); }
  catch (e) { inner.error = errorMessage(e); }
}

/** 载入组合场景的两份配置，仍需在子网页重新预览后点击开始。 */
export async function loadScenario(id: string): Promise<void> {
  if (inner.busy || inner.status.running || scenarioBlocksActions() || !inner.synced) throw new Error('请等待状态同步或当前测试完成');
  inner.busy = true;
  try {
    const request = await api.post<{ subnet: unknown; inner: Record<string, unknown> }>('/api/scenario/runs/request', { id });
    // 先验证两份历史输入，再改任何一边的当前配置；否则损坏的内环历史会
    // 先把子网计划换掉，随后 parseInnerProject 失败，页面就停在半载入状态。
    const cfg = parseInnerProject(JSON.stringify(request.inner));
    cfg.resume = true;
    if (!adoptRunRequest(request.subnet, true)) throw new Error('场景中的子网计划无法恢复');
    inner.config = cfg;
    inner.capability = null;
    inner.preview = null;
    inner.previewStale = true;
    await refreshInnerPlan();
    await previewSubnet();
    goto('inner');
  } finally {
    inner.busy = false;
  }
}

export async function innerReport(): Promise<{ name: string; html: string }> {
  return api.get('/api/inner/report');
}

// ---------------- 历史 ----------------

export async function listInnerRuns(): Promise<void> {
  try { inner.runs = await api.get<InnerRunEntry[]>('/api/inner/runs'); }
  catch (e) { inner.error = errorMessage(e); }
}
export async function innerRunReport(id: string): Promise<{ name: string; html: string }> {
  return api.post('/api/inner/runs/report', { id });
}
/**
 * 把某一轮的配置装载回控制台。
 *
 * **只装载，不开跑**：隔夜的网口拓扑可能已经变了，老配置里的网卡未必还在。
 * 装载后预览会重新生成，该看到的是差异，而不是一轮悄悄少跑了几条网口的测试。
 * 令牌不在历史文件里，装载后仍要手工重填。
 */
export async function loadInnerRunConfig(id: string): Promise<void> {
  if (inner.busy || inner.status.running || scenarioBlocksActions() || !inner.synced) throw new Error('请等待状态同步或当前内环操作完成');
  inner.busy = true;
  try {
    const cfg = await api.post<Record<string, unknown>>('/api/inner/runs/config', { id });
    const loaded = parseInnerProject(JSON.stringify(cfg));
    // 历史入口语义是「恢复重跑」：沿用当时的计划，但默认只重跑未通过/未完成
    // 的单元；用户仍可在打流参数区取消勾选再做一次全量复测。
    loaded.resume = true;
    inner.config = loaded;
    inner.capability = null;
    inner.preview = null;
    inner.previewStale = true;
    await refreshInnerPlan();
  } finally {
    inner.busy = false;
  }
}

// ---------------- 网口表格的编辑动作 ----------------

/**
 * 调整执行顺序。
 *
 * 换的是数组里的位置，**不是**对象本身——正在编辑的那一条、勾选状态和
 * 参数都跟着对象走，排序不会让详情面板指到别人身上。
 */
/**
 * 把 `from` 处的链路移到 `to` 处（都是**完整列表**里的下标）。
 *
 * 之前是 `moveInnerLink(index, ±1)`，只会和紧邻的那一行换位。表格上方有搜索和
 * 电脑筛选，可见的往往只是一个子集：点「上移」换到的是一行**看不见的**邻居，
 * 屏幕上两行的先后一点没变，只有「顺序」那一列的数字跳了一下。人会以为没生效
 * 而反复点，实际执行顺序已经被改了好几次。
 */
export function moveInnerLinkTo(from: number, to: number): void {
  const links = inner.config.links;
  if (from === to || from < 0 || from >= links.length || to < 0 || to >= links.length) return;
  const [moved] = links.splice(from, 1);
  links.splice(to, 0, moved);
}

/** 勾选/取消参与本轮。配置一个字节都不动，只是这一轮跑不跑它。 */
export function setInnerLinkEnabled(links: InnerLink[], enabled: boolean): void {
  for (const link of links) link.enabled = enabled;
}

/**
 * 批量改参数。
 *
 * 白名单里**没有** host / local_interface / local_ip：那三样是「这条链路是
 * 哪台电脑的哪个网口」的身份，批量复制过去只会把别人的源 IP 写到自己头上。
 */
export type InnerBatchPatch = Partial<Pick<InnerLink,
  'gateway' | 'gateway_ipv6' | 'board_rx_interface' | 'measurement'
  | 'upload_min_mbps' | 'download_min_mbps' | 'bidir_total_min_mbps'
  | 'tool_upload_min_mbps' | 'tool_download_min_mbps' | 'tool_bidir_total_min_mbps'>>;

export function applyInnerBatch(links: InnerLink[], patch: InnerBatchPatch): void {
  for (const link of links) {
    for (const [key, value] of Object.entries(patch) as [keyof InnerLink, never][]) {
      if (value !== undefined && String(value).trim() !== '') link[key] = value;
    }
    // 切回严格模式时，工具口径门限当场清掉——留着就是个永不生效的数，
    // 而且会让后端直接拒绝整份配置。
    if (link.measurement === 'nic_strict') {
      link.tool_upload_min_mbps = null;
      link.tool_download_min_mbps = null;
      link.tool_bidir_total_min_mbps = null;
    }
  }
}

/** 新建一条链路并追加到末尾，名字自动避重。 */
export function addInnerLink(host = 'master'): InnerLink {
  let i = inner.config.links.length + 1;
  while (inner.config.links.some((l) => l.name === `网口 ${i}`)) i++;
  const link = innerLink(host, undefined, `网口 ${i}`);
  inner.config.links.push(link);
  return link;
}

/** 只添加操作员明确选中的扫描项，保留原有网口及其参数。 */
export function addInnerScannedLinks(keys: string[]): InnerLink[] {
  const selected = new Set(keys);
  const added = linksFromInnerChoices(inner.config.links, innerNicChoices(inner.capability, true).filter((choice) => selected.has(choice.key)), inner.capability);
  inner.config.links.push(...added);
  return added;
}
