import { beforeEach, describe, expect, it, vi } from 'vitest';
import { defaultInnerConfig, innerLink, parseInnerProject, serializeInnerProject } from '../domain/inner';
import type { InnerConfig, InnerLink } from '../domain/inner';
import type { InnerCapability } from '../domain/inner';
import { innerNicChoices } from '../domain/inner-setup';
import {
  addInnerLink, addInnerScannedLinks, applyInnerBatch, importInner, inner, moveInnerLinkTo, probeInner, refreshInnerPlan,
  setInnerLinkEnabled, startInner, startSubnetThenInner, stopInner, syncInnerStatus, syncScenarioStatus,
  loadInnerRunConfig, scenarioBlocksActions, prepareAfterUnknownScenario,
} from './inner';
import { buildRunRequest, plan as subnetPlan } from './plan';
import { api } from '../api/client';
import { NetworkError, UnauthorizedError } from '../api/client';
import { session } from './session';
import type * as ApiModule from '../api/client';
vi.mock('../api/client', async (original) => ({ ...await original<typeof ApiModule>(), api: { get: vi.fn(), post: vi.fn() } }));

function link(patch: Partial<InnerLink> = {}): InnerLink {
  return { ...innerLink(), local_interface: 'WLAN', local_ip: '192.168.8.101', gateway: '192.168.8.1', ...patch };
}

describe('内环独立状态与 API', () => {
  beforeEach(() => {
    vi.clearAllMocks(); inner.config = defaultInnerConfig(); inner.busy = false; inner.synced = true; inner.error = '';
    inner.preview = null; inner.previewStale = true; inner.previewError = '';
    inner.status = { running: false, current: '', error: null, completed: 0, total: 0, units: [], has_report: false };
    inner.scenarioStartPhase = 'idle'; inner.scenarioLastReadIdle = false;
    inner.scenario = { running: false, id: '', phase: '', error: null, runs: [] };
    subnetPlan.preview = null; subnetPlan.previewRequestFingerprint = '';
    vi.mocked(api.get).mockResolvedValue(inner.status);
  });
  it('扫描选口只添加明确勾选的项，重复添加不覆盖配置也不启动测试', () => {
    inner.capability = { local: { interfaces: [
      { name: 'ETH 1', ipv4: '192.168.0.100' },
      { name: 'ETH 2', ipv4: '192.168.0.101' },
    ] }, agents: [] } as unknown as InnerCapability;
    const selected = innerNicChoices(inner.capability)[1]!;
    const added = addInnerScannedLinks([selected.key]);
    expect(added).toHaveLength(1);
    expect(inner.config.links[0]).toMatchObject({ local_interface: 'ETH 2', local_ip: '192.168.0.101' });
    inner.config.links[0]!.enabled = false;
    inner.config.links[0]!.gateway = '192.168.0.9';
    expect(addInnerScannedLinks([selected.key])).toEqual([]);
    expect(inner.config.links[0]).toMatchObject({ enabled: false, gateway: '192.168.0.9' });
    expect(api.post).not.toHaveBeenCalled();
    inner.capability = null;
  });
  it('扫描、开跑、停止只访问内环命名空间', async () => {
    await probeInner();
    inner.config.links = [link()];
    // 状态响应独立于被 startInner 改动的对象。
    vi.mocked(api.get).mockResolvedValue({ ...inner.status });
    await startInner(); await stopInner();
    expect(vi.mocked(api.post).mock.calls.map(([path]) => path)).toEqual(['/api/inner/probe', '/api/inner/run', '/api/inner/stop']);
    // 带上了 units_from 游标，所以按前缀断言；要守的是「只碰内环命名空间」。
    expect(vi.mocked(api.get).mock.calls.every(([path]) => path.startsWith('/api/inner/status'))).toBe(true);
  });
  it.each<[string, Partial<InnerConfig>]>([
    ['UDP 速率未填写', { protocols: ['udp'], udp_mbps: null }],
    ['时长和数字参数正在清空编辑', { duration_secs: '' as never, parallel: 0, port: 0, repeats: 0 }],
    ['协议参数未填写完整', { tcp_streams: 0, tcp_window: '待填写', udp_length: '待填写', max_udp_loss_pct: -1 }],
    ['协议和方向暂未选择', { protocols: [], directions: [] }],
  ])('扫描不受%s影响，也不改写正在编辑的测试配置', async (_label, unfinished) => {
    inner.config = {
      ...defaultInnerConfig(), ...unfinished,
      adb_path: 'C:\\Program Files\\platform-tools\\adb.exe',
      serial: 'cpe-device-02', board_iperf: '/data/local/tmp/iperf3',
      agents: [
        { id: 'agent-a', address: ' 192.168.8.200 ', port: 28801, token: 'probe-token-a' },
        { id: 'agent-b', address: '192.168.8.201', port: 28802, token: 'probe-token-b' },
      ],
      // 新网口还没指定源 IP，甚至可以有未勾选的旧配置；扫描必须都不受影响。
      links: [innerLink(), { ...innerLink('agent-b'), enabled: false }],
    };
    const before = JSON.parse(JSON.stringify(inner.config));
    const capability = { serial: 'cpe-device-02' } as InnerCapability;
    vi.mocked(api.post).mockImplementationOnce(async (_path, body) => {
      // 扫描端点也会校验完整配置：mock 不能无条件成功掩盖无效请求。
      parseInnerProject(JSON.stringify(body));
      return capability;
    });

    await probeInner();

    expect(inner.error).toBe('');
    expect(inner.capability).toEqual(capability);
    expect(inner.busy).toBe(false);
    expect(inner.config).toEqual(before);
    expect(api.post).toHaveBeenCalledExactlyOnceWith('/api/inner/probe', {
      ...defaultInnerConfig(),
      adb_path: before.adb_path, serial: before.serial, board_iperf: before.board_iperf,
      agents: before.agents.map((agent: InnerConfig['agents'][number]) => ({
        ...agent, address: agent.address.trim(),
      })),
    });
  });
  it('扫描连接参数用独立快照，保留全部辅测机及内存令牌', async () => {
    inner.config.agents = [{ id: 'agent-a', address: '192.168.8.200', port: 28801, token: 'first-token' }];
    let finish!: (value: unknown) => void;
    vi.mocked(api.post).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    const pending = probeInner();
    const request = vi.mocked(api.post).mock.calls[0]?.[1] as InnerConfig;
    inner.config.agents[0].address = '192.168.8.201';
    inner.config.agents[0].token = 'edited-token';
    expect(request.agents).toEqual([
      { id: 'agent-a', address: '192.168.8.200', port: 28801, token: 'first-token' },
    ]);
    finish({ serial: 'device' });
    await pending;
    expect(inner.config.agents[0].token).toBe('edited-token');
    expect(inner.capability).toBeNull();
    expect(inner.busy).toBe(false);
  });
  it.each<[string, (config: InnerConfig) => void]>([
    ['ADB 路径', (cfg) => { cfg.adb_path = '/opt/adb'; }],
    ['ADB 序列号', (cfg) => { cfg.serial = 'another-device'; }],
    ['板侧工具', (cfg) => { cfg.board_iperf = '/data/iperf3'; }],
    ['辅测机标识', (cfg) => { cfg.agents[0].id = 'agent-b'; }],
    ['辅测机地址', (cfg) => { cfg.agents[0].address = '192.168.8.201'; }],
    ['辅测机端口', (cfg) => { cfg.agents[0].port = 28802; }],
    ['辅测机令牌', (cfg) => { cfg.agents[0].token = 'updated-token'; }],
    ['移除辅测机', (cfg) => { cfg.agents.splice(0, 1); }],
  ])('扫描成功后修改%s立即清除旧扫描结果', async (_label, change) => {
    inner.config.agents = [{ id: 'agent-a', address: '192.168.8.200', port: 28801, token: 'token' }];
    vi.mocked(api.post).mockResolvedValueOnce({ serial: 'device' });
    await probeInner();
    expect(inner.capability?.serial).toBe('device');
    change(inner.config);
    expect(inner.capability).toBeNull();
  });
  it('调整打流参数、网口设置和连接地址两端空格保留扫描结果', async () => {
    inner.config.agents = [{ id: 'agent-a', address: '192.168.8.200', port: 28801, token: 'token' }];
    vi.mocked(api.post).mockResolvedValueOnce({ serial: 'device' });
    await probeInner();
    inner.config.duration_secs = 30;
    inner.config.links = [link()];
    inner.config.links[0].gateway = '192.168.8.2';
    inner.config.agents[0].address = ' 192.168.8.200 ';
    expect(inner.capability?.serial).toBe('device');
  });
  it.each([false, true])('设备修改后即使改回原值，也不接收旧扫描的%s响应', async (fail) => {
    let finish!: (value: unknown) => void;
    let reject!: (reason: Error) => void;
    vi.mocked(api.post).mockImplementationOnce(() => new Promise((resolve, rejectPromise) => {
      finish = resolve; reject = rejectPromise;
    }));
    const pending = probeInner();
    inner.config.serial = 'another-device';
    inner.config.serial = '';
    if (fail) reject(new Error('旧设备连接失败')); else finish({ serial: 'old-device' });
    await pending;
    expect(inner.capability).toBeNull();
    expect(inner.error).toBe('');
    expect(inner.busy).toBe(false);
  });
  it('扫描仍暴露设备连接错误，不会偷偷替换辅测机或 ADB 设置', async () => {
    inner.config.agents = [{ id: 'agent-a', address: '', port: 28801, token: 'keep-token' }];
    vi.mocked(api.post).mockImplementationOnce(async (_path, body) => {
      parseInnerProject(JSON.stringify(body));
      return { serial: 'unexpected' };
    });
    await probeInner();
    expect(inner.error).toContain('辅测机');
    expect(inner.capability).toBeNull();
    expect(inner.busy).toBe(false);
    expect(inner.config.agents[0]).toEqual({ id: 'agent-a', address: '', port: 28801, token: 'keep-token' });
  });
  it('计划预览来自后端，配置改动后先失效再刷新', async () => {
    inner.config.links = [link()];
    vi.mocked(api.post).mockResolvedValue({ links: 1, units: 2, legs: 2, bidir_units: 0, estimated_secs: 80, uses_master: true, agents: [], skipped: [], rows: [] });
    await refreshInnerPlan();
    expect(vi.mocked(api.post).mock.calls[0]?.[0]).toBe('/api/inner/plan');
    expect(inner.preview?.units).toBe(2);
    expect(inner.previewStale).toBe(false);
    // 配置非法时不去打服务端，也不留下过期预览。
    vi.clearAllMocks();
    inner.config.links = [link({ gateway: '' })];
    await refreshInnerPlan();
    expect(api.post).not.toHaveBeenCalled();
    expect(inner.preview).toBeNull();
    expect(inner.previewStale).toBe(true);
    expect(inner.previewError).toBeTruthy();
  });
  it('一条都没勾选就不开跑，取消勾选不删配置', async () => {
    inner.config.links = [link({ enabled: false })];
    await startInner();
    expect(api.post).not.toHaveBeenCalled();
    expect(inner.error).toContain('勾选');
    expect(inner.config.links).toHaveLength(1);
  });
  it('乱序返回的旧预览成功或失败都不能覆盖新预览', async () => {
    for (const fail of [false, true]) {
      inner.config.links = [link()];
      let resolveOld!: (value: unknown) => void;
      let rejectOld!: (error: Error) => void;
      vi.mocked(api.post).mockImplementationOnce(() => new Promise((resolve, reject) => { resolveOld = resolve; rejectOld = reject; }));
      const old = refreshInnerPlan();
      inner.config.duration_secs = 30;
      vi.mocked(api.post).mockResolvedValueOnce({ units: 4 });
      await refreshInnerPlan();
      if (fail) rejectOld(new Error('旧请求失败')); else resolveOld({ units: 2 });
      await old;
      expect(inner.preview?.units).toBe(4);
      expect(inner.previewStale).toBe(false);
      expect(inner.previewError).toBe('');
    }
  });
  it('配置已修改但新预览尚未发出时，旧响应不能把预览标成有效', async () => {
    inner.config.links = [link()];
    let finish!: (value: unknown) => void;
    vi.mocked(api.post).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    const pending = refreshInnerPlan();
    inner.config.duration_secs = 31;
    finish({ units: 2 });
    await pending;
    expect(inner.preview).toBeNull();
    expect(inner.previewStale).toBe(true);
  });
  it('运行中或误导入子网文件都不替换当前配置', () => {
    inner.config.serial = 'keep';
    expect(() => importInner('{"project_version":1}')).toThrow('子网');
    expect(inner.config.serial).toBe('keep');
    inner.status.running = true;
    expect(() => importInner(serializeInnerProject(defaultInnerConfig()))).toThrow('等待');
    expect(inner.config.serial).toBe('keep');
  });
  it('未同步时不开跑，读取状态不重置配置', async () => {
    inner.synced = false; inner.config.serial = 'keep';
    await startInner(); expect(api.post).not.toHaveBeenCalled();
    await syncInnerStatus(); expect(inner.config.serial).toBe('keep');
  });
  it('停止后等待旧状态响应并重读，不让旧运行状态截断同步', async () => {
    let finish!: (value: unknown) => void;
    vi.mocked(api.get).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    const pending = syncInnerStatus();
    expect(syncInnerStatus()).toBe(pending);
    const stopping = stopInner();
    finish({ ...inner.status });
    await stopping;
    expect(api.get).toHaveBeenCalledTimes(2);
    expect(inner.busy).toBe(false);
  });
  it('组合场景启动响应丢失时只回读状态，不重复启动', async () => {
    inner.config.links = [link()];
    inner.preview = { units: 2 } as never;
    inner.previewStale = false;
    subnetPlan.preview = { plan_hash: 'plan-hash' } as never;
    subnetPlan.previewRequestFingerprint = JSON.stringify(buildRunRequest());
    vi.mocked(api.post).mockRejectedValueOnce(new NetworkError(new Error('connection lost')));
    vi.mocked(api.get).mockImplementation(async (path) => {
      if (path === '/api/scenario/status') return { running: true, id: 'scenario-1', phase: 'subnet', error: null };
      return inner.status;
    });

    await startSubnetThenInner();

    expect(inner.scenario).toMatchObject({ running: true, id: 'scenario-1', phase: 'subnet' });
    expect(api.post).toHaveBeenCalledTimes(1);
    expect(api.post).toHaveBeenCalledWith('/api/scenario/run', expect.anything());
    expect(api.get).toHaveBeenCalledWith('/api/scenario/status');
  });
  it('启动响应和连续三次状态回读丢失后仍恢复，不重复启动', async () => {
    inner.config.links = [link()];
    inner.preview = { units: 2 } as never;
    inner.previewStale = false;
    subnetPlan.preview = { plan_hash: 'plan-hash' } as never;
    subnetPlan.previewRequestFingerprint = JSON.stringify(buildRunRequest());
    vi.useFakeTimers();
    try {
      let statusCalls = 0;
      vi.mocked(api.post).mockRejectedValueOnce(new NetworkError(new Error('connection lost')));
      vi.mocked(api.get).mockImplementation(async (path) => {
        if (path === '/api/scenario/status') {
          statusCalls += 1;
          if (statusCalls <= 3) throw new NetworkError(new Error('status connection lost'));
          return { running: true, id: 'scenario-2', phase: 'subnet', error: null };
        }
        return inner.status;
      });

      await startSubnetThenInner();
      expect(inner.scenario.running).toBe(false);
      expect(inner.scenarioStartPhase).toBe('unknown');
      await vi.advanceTimersByTimeAsync(3000);
      expect(inner.scenario).toMatchObject({ running: true, id: 'scenario-2', phase: 'subnet' });
      expect(api.post).toHaveBeenCalledTimes(1);
      expect(statusCalls).toBe(4);
    } finally {
      vi.useRealTimers();
    }
  });
  it('未知启动收到空闲快照仍锁定，明确重新准备才解除且不重发', async () => {
    vi.useFakeTimers();
    try {
      inner.scenarioStartPhase = 'unknown';
      vi.mocked(api.get).mockImplementation(async (path) => path === '/api/scenario/status'
        ? { running: false, id: '', phase: '', error: null } : inner.status);
      await syncScenarioStatus();
      expect(inner.scenarioStartPhase).toBe('unknown');
      expect(scenarioBlocksActions()).toBe(true);
      await startSubnetThenInner();
      expect(api.post).not.toHaveBeenCalled();
      prepareAfterUnknownScenario();
      expect(scenarioBlocksActions()).toBe(false);
      expect(inner.previewStale).toBe(true);
      const reads = vi.mocked(api.get).mock.calls.length;
      await vi.advanceTimersByTimeAsync(3000);
      expect(api.get).toHaveBeenCalledTimes(reads);
    } finally { vi.clearAllTimers(); vi.useRealTimers(); }
  });

  it('未知场景查询遇到 401 停止轮询并进入全局会话失效', async () => {
    vi.useFakeTimers();
    try {
      inner.scenarioStartPhase = 'unknown';
      vi.mocked(api.get).mockRejectedValue(new UnauthorizedError());
      await syncScenarioStatus();
      expect(session.phase).toBe('unauthorized');
      expect(inner.scenarioLastReadIdle).toBe(false);
      const reads = vi.mocked(api.get).mock.calls.length;
      await vi.advanceTimersByTimeAsync(5000);
      expect(api.get).toHaveBeenCalledTimes(reads);
    } finally { vi.clearAllTimers(); vi.useRealTimers(); }
  });

  it('乱序场景状态响应不能覆盖较新的启动状态', async () => {
    vi.useFakeTimers();
    try {
      let resolveOld!: (value: unknown) => void;
      let resolveNew!: (value: unknown) => void;
      let statusCalls = 0;
      vi.mocked(api.get).mockImplementation((path) => {
        if (path === '/api/scenario/status') {
          statusCalls += 1;
          return new Promise((resolve) => {
            if (statusCalls === 1) resolveOld = resolve;
            else resolveNew = resolve;
          });
        }
        return Promise.resolve(inner.status);
      });

      const old = syncScenarioStatus();
      const latest = syncScenarioStatus();
      resolveNew({ running: true, id: 'scenario-new', phase: 'subnet', error: null });
      await latest;
      expect(inner.scenario).toMatchObject({ running: true, id: 'scenario-new' });
      resolveOld({ running: false, id: '', phase: '', error: null });
      await old;
      expect(inner.scenario).toMatchObject({ running: true, id: 'scenario-new' });
    } finally {
      vi.clearAllTimers();
      vi.useRealTimers();
    }
  });
  it('载入历史配置的请求期间锁住开始操作，完成后才解锁', async () => {
    let finish!: (value: unknown) => void;
    vi.mocked(api.post).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    const pending = loadInnerRunConfig('run-old');
    await Promise.resolve();
    expect(inner.busy).toBe(true);
    await startInner();
    expect(api.post).toHaveBeenCalledTimes(1);
    finish(defaultInnerConfig());
    await pending;
    expect(inner.busy).toBe(false);
  });
});

describe('网口表格的编辑动作', () => {
  beforeEach(() => { inner.config = defaultInnerConfig(); });
  it('排序换的是位置不是对象，正在编辑的那一条身份不变', () => {
    inner.config.links = [link({ name: 'A' }), link({ name: 'B', local_ip: '192.168.8.102' }), link({ name: 'C', local_ip: '192.168.8.103' })];
    const editing = inner.config.links[1];
    moveInnerLinkTo(1, 0);
    expect(inner.config.links.map((l) => l.name)).toEqual(['B', 'A', 'C']);
    expect(inner.config.links[0]).toBe(editing);
    moveInnerLinkTo(0, -1); // 越界不动
    expect(inner.config.links.map((l) => l.name)).toEqual(['B', 'A', 'C']);
    moveInnerLinkTo(2, 3);
    expect(inner.config.links.map((l) => l.name)).toEqual(['B', 'A', 'C']);
    moveInnerLinkTo(1, 1); // 原地不动
    expect(inner.config.links.map((l) => l.name)).toEqual(['B', 'A', 'C']);
  });
  it('筛选之后上移换的是看得见的那一行，不是被筛掉的邻居', () => {
    // 表格上过滤成只看 agent1 时，可见的是 B 和 D；点 D 的「上移」应当把 D 排到
    // B 前面。按 ±1 走的话换到的是被筛掉的 C，屏幕上两行的先后一点没变。
    inner.config.links = [
      link({ name: 'A', host: 'master' }),
      link({ name: 'B', host: 'agent1', local_ip: '192.168.8.102' }),
      link({ name: 'C', host: 'master', local_ip: '192.168.8.103' }),
      link({ name: 'D', host: 'agent1', local_ip: '192.168.8.104' }),
    ];
    const visible = () => inner.config.links
      .map((l, index) => ({ l, index }))
      .filter(({ l }) => l.host === 'agent1');
    const rows = visible();
    const target = rows[1];
    const prev = rows[0].index;
    moveInnerLinkTo(target.index, prev);
    expect(visible().map(({ l }) => l.name)).toEqual(['D', 'B']);
  });
  it('批量只改参数，绝不复制别人的电脑、网卡和源 IP', () => {
    const a = link({ name: 'A', host: 'master', local_interface: 'en0', local_ip: '192.168.8.100' });
    const b = link({ name: 'B', host: 'agent1', local_interface: 'wlan0', local_ip: '192.168.8.101' });
    inner.config.links = [a, b];
    applyInnerBatch([a, b], { gateway: '192.168.9.1', measurement: 'nic_preferred', upload_min_mbps: 800 });
    for (const item of [a, b]) {
      expect(item.gateway).toBe('192.168.9.1');
      expect(item.measurement).toBe('nic_preferred');
      expect(item.upload_min_mbps).toBe(800);
    }
    expect(a.local_ip).toBe('192.168.8.100');
    expect(b.local_ip).toBe('192.168.8.101');
    expect(b.host).toBe('agent1');
    // 切回严格模式当场清掉工具门限：留着就是个永不生效、还会让后端整份拒绝的数。
    a.tool_upload_min_mbps = 700;
    applyInnerBatch([a], { measurement: 'nic_strict' });
    expect(a.tool_upload_min_mbps).toBeNull();
  });
  it('勾选和删除是两回事', () => {
    inner.config.links = [link({ name: 'A' }), link({ name: 'B', local_ip: '192.168.8.102' })];
    setInnerLinkEnabled(inner.config.links, false);
    expect(inner.config.links.every((l) => !l.enabled)).toBe(true);
    expect(inner.config.links).toHaveLength(2);
    setInnerLinkEnabled([inner.config.links[0]], true);
    expect(inner.config.links.map((l) => l.enabled)).toEqual([true, false]);
  });
  it('新建网口自动避重名并追加到末尾', () => {
    const first = addInnerLink();
    const second = addInnerLink('agent1');
    expect(first.name).not.toBe(second.name);
    expect(second.host).toBe('agent1');
    expect(inner.config.links.map((l) => l.name)).toEqual([first.name, second.name]);
  });
});

it('状态游标携带运行标识，跨轮响应替换旧行', async () => {
  inner.status = { running: false, current: '', error: null, completed: 1, total: 1,
    run_id: 'old-run', units: [{ index: 99 } as never], has_report: true };
  vi.mocked(api.get).mockResolvedValueOnce({ ...inner.status, run_id: 'new-run',
    units_from: 0, units: [{ index: 1 }] });
  await syncInnerStatus();
  expect(api.get).toHaveBeenLastCalledWith('/api/inner/status?units_from=1&run_id=old-run', { timeoutMs: 10000 });
  expect(inner.status.units.map((u) => u.index)).toEqual([1]);
  expect(inner.status.run_id).toBe('new-run');
});
