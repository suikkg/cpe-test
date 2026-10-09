import { test, expect, type Page, type Route } from '@playwright/test';
import { readFileSync } from 'node:fs';
const fixture = (name: string) => JSON.parse(readFileSync(new URL(`../src/api/__fixtures__/${name}.json`, import.meta.url), 'utf8'));
const bootstrap = fixture('bootstrap_out');
const progress = fixture('progress_out');

const pageErrors = new WeakMap<Page, Error[]>();
test.beforeEach(({ page }) => {
  const errors: Error[] = [];
  pageErrors.set(page, errors);
  page.on('pageerror', (error) => errors.push(error));
});
test.afterEach(({ page }) => expect(pageErrors.get(page)).toEqual([]));

const token = 'browser-regression-secret';
const nic = (name: string, ipv4: string) => ({ name, ipv4, description: `${name} adapter`, role: 'SGMII2.5G', speed_mbps: 2500 });
const host = (name: string, suffix = '') => ({ hostname: name, os: 'Windows', interfaces: [
  nic(`${name}-LAN${suffix}`, '192.168.1.2'), nic(`${name}-USB${suffix}`, '10.1.1.2'),
] });
const local = (suffix = '') => ({ host: host('master', suffix), iperf3: 'iperf 3.18', version: 'test' });
const topology = (suffix = '') => ({ master: host('master', suffix), agent: host('agent', suffix) });
const success = (route: Route, data: unknown) => route.fulfill({ json: { ok: true, data } });
const nav = (page: Page, name: string) => page.getByRole('navigation', { name: '控制台区域' }).getByRole('button', { name: new RegExp(`^${name}`) });

async function consoleApi(page: Page, running: 'subnet' | 'inner' | 'scenario' | '' = '', entry = `/?token=${token}`) {
  const state = { localSuffix: '', connectSuffix: '', localFail: false, connectFail: false, connect401: false,
    localCalls: 0, connections: [] as Record<string, unknown>[], headers: [] as Record<string, string>[] };
  await page.route('**/api/**', async (route) => {
    const request = route.request();
    state.headers.push(request.headers());
    const path = new URL(request.url()).pathname;
    if (path === '/api/bootstrap') return success(route, bootstrap);
    if (path === '/api/progress') return success(route, { ...progress, from: 0, units_from: 0, lines: [], running: running === 'subnet' });
    if (path === '/api/inner/status') return success(route, {
      running: running === 'inner', current: '', error: null, completed: 0, total: 0, units: [], units_from: 0, has_report: false,
    });
    if (path === '/api/scenario/status') return success(route, { running: running === 'scenario', id: 'scenario-test', phase: 'subnet', error: null });
    if (path === '/api/local') {
      state.localCalls++;
      if (state.localFail) return route.abort('connectionfailed');
      return success(route, local(state.localSuffix));
    }
    if (path === '/api/connect') {
      state.connections.push(request.postDataJSON());
      if (state.connect401) return route.fulfill({ status: 401, json: { ok: false } });
      if (state.connectFail) return route.abort('connectionfailed');
      return success(route, topology(state.connectSuffix));
    }
    // 其它路由穿过真实服务端，避免宽泛空对象桩掩盖新增请求或 DTO 错误。
    return route.continue();
  });
  await page.goto(entry);
  await expect(page.getByRole('heading', { name: '连接', exact: true })).toBeVisible();
  await expect(page.getByText('master-LAN', { exact: true })).toBeVisible();
  return state;
}

async function connectAgent(page: Page) {
  await nav(page, '连接').click();
  await page.getByRole('textbox', { name: /^辅测机地址/ }).fill('192.168.1.3');
  await page.getByRole('button', { name: '连接', exact: true }).click();
  await expect(page.getByText('已连上', { exact: false }).first()).toBeVisible();
}

test('未连接只扫本机；连接多前缀；一个重扫入口刷新同一份双端表', async ({ page }) => {
  const state = await consoleApi(page);
  state.localSuffix = '-local';
  await page.getByRole('button', { name: '重新扫描', exact: true }).click();
  await expect(page.getByText('master-LAN-local', { exact: true })).toBeVisible();
  expect(state.connections).toHaveLength(0);
  await page.getByRole('textbox', { name: /^辅测机地址/ }).fill('192.168.1.3');
  await page.getByLabel('IPv4 前缀过滤').fill('192.168., 10., ,172.16.');
  await page.getByLabel('IPv4 前缀过滤').press('Enter');
  await expect(page.getByText('agent-USB', { exact: true })).toBeVisible();
  expect(state.connections[0]).toMatchObject({ host: '192.168.1.3', ipv4_prefixes: ['192.168.', '10.', '172.16.'] });

  state.connectSuffix = '-agent-scan';
  await page.getByLabel('IPv4 前缀过滤').fill('');
  await page.getByRole('button', { name: '重新扫描', exact: true }).click();
  await expect(page.getByText('agent-LAN-agent-scan', { exact: true })).toBeVisible();
  expect(state.connections[1].ipv4_prefixes).toEqual([]);
  await expect(page.getByText('master-LAN-agent-scan', { exact: true })).toBeVisible();
  state.connectSuffix = '-local-scan';
  await page.getByRole('button', { name: '重新扫描', exact: true }).click();
  await expect(page.getByText('master-LAN-local-scan', { exact: true })).toBeVisible();
  await expect(page.getByText('agent-LAN-local-scan', { exact: true })).toBeVisible();
  expect(state.connections).toHaveLength(3);
  expect(state.localCalls).toBe(4);
  expect(state.headers.every((header) => header['x-cpe-token'] === token)).toBe(true);
});

for (const failure of ['local', 'connect'] as const) {
  test(`${failure} 断开保留双端旧快照、原身份和时间，重试成功才移除提示`, async ({ page }) => {
    const state = await consoleApi(page);
    await connectAgent(page);
    await page.getByRole('textbox', { name: /^辅测机地址/ }).fill('192.168.9.9');
    state[`${failure}Fail`] = true;
    await page.getByRole('button', { name: '重新扫描', exact: true }).click();
    await expect(page.getByText('与 192.168.1.3 的连接已断开', { exact: true })).toBeVisible();
    await expect(page.getByRole('alert')).toContainText('上次成功');
    await expect(page.getByRole('alert')).toContainText('192.168.1.3');
    await expect(page.getByRole('alert')).toContainText(/192\.168\.1\.3.*\d{2}:\d{2}:\d{2}/);
    await expect(page.getByText('agent-LAN', { exact: true })).toBeVisible();
    await expect(page.getByText('master-LAN', { exact: true })).toBeVisible();
    state[`${failure}Fail`] = false;
    state.connectSuffix = '-recovered';
    await page.getByRole('button', { name: '重新扫描', exact: true }).click();
    await expect(page.getByText('master-LAN-recovered', { exact: true })).toBeVisible();
    await expect(page.getByText(/上次成功连接/)).toHaveCount(0);
    await expect(page.getByText('已连 192.168.9.9', { exact: true })).toBeVisible();
  });
}

for (const running of ['subnet', 'inner', 'scenario'] as const) {
  test(`${running} 首页同步后锁定扫描、连接及表单提交`, async ({ page }) => {
    const state = await consoleApi(page, running);
    await expect(page.getByRole('button', { name: '重新扫描', exact: true })).toBeDisabled();
    await expect(page.getByRole('button', { name: '连接', exact: true })).toBeDisabled();
    await page.getByRole('textbox', { name: /^辅测机地址/ }).press('Enter');
    // 同时守住处理函数：requestSubmit 不受 disabled submit 按钮保护。
    await page.locator('form').evaluate((form: HTMLFormElement) => form.requestSubmit());
    expect(state.connections).toHaveLength(0);
    expect(state.localCalls).toBe(1);
  });
}

test('真实页面 cookie 支持刷新和新标签；API 只带 cookie 仍为 401', async ({ page, context }) => {
  const state = await consoleApi(page);
  await expect(page).toHaveURL(/\/$/);
  const cookie = (await context.cookies()).find((c) => c.name === 'cpe_ui_session');
  expect(cookie).toMatchObject({ value: token, sameSite: 'Strict', httpOnly: false, expires: -1 });
  const refreshed = await page.reload();
  expect(refreshed?.status()).toBe(200);
  await expect(page.getByText('master-LAN', { exact: true })).toBeVisible();
  expect(state.headers.every((header) => header['x-cpe-token'] === token)).toBe(true);
  // 新标签的 sessionStorage 是空的：只靠服务端 cookie 打开，再由前端补请求头。
  const fresh = await context.newPage();
  const firstApi = fresh.waitForRequest((r) => r.url().includes('/api/'));
  await consoleApi(fresh, '', '/');
  expect((await firstApi).headers()['x-cpe-token']).toBe(token);
  await fresh.close();
  // APIRequestContext 不经过 page.route，验证的是真实 Rust 鉴权。
  expect((await context.request.get('/api/progress')).status()).toBe(401);
  expect((await context.request.post('/api/connect', { headers: { 'X-CPE-Console': '1' }, data: {} })).status()).toBe(401);
  expect((await context.request.get('/api/progress', { headers: { 'X-CPE-Token': token } })).status()).toBe(200);
});

test('API 401 进入全局口令失效提示，普通网络失败不冒充 401', async ({ page }) => {
  const state = await consoleApi(page);
  await connectAgent(page);
  state.connect401 = true;
  await page.getByRole('button', { name: '重新扫描', exact: true }).click();
  await expect(page.getByRole('alert')).toContainText('控制台口令无效或已失效');
  await expect(page.getByRole('alert')).toContainText('?token=');
  await nav(page, '连接').click();
  await expect(page.getByRole('heading', { name: '连接', exact: true })).toHaveCount(0);
  expect(state.connections).toHaveLength(2);
});

for (const width of [1280, 600]) {
  test(`键盘导航与网卡搜索（${width}px）`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    await consoleApi(page);
    await page.keyboard.press('Tab');
    await expect(page.getByRole('link', { name: '跳转到主要内容' })).toBeFocused();
    await page.keyboard.press('Enter');
    await expect(page.locator('#main-content')).toBeFocused();
    const search = page.getByRole('searchbox', { name: '搜索网卡' });
    await search.fill('USB');
    await expect(page.getByText('master-LAN', { exact: true })).toHaveCount(0);
    await expect(page.getByText('master-USB', { exact: true })).toBeVisible();
    await nav(page, '计划').focus();
    await page.keyboard.press('Space');
    await expect(page.getByRole('heading', { name: '计划', exact: true })).toBeVisible();
    await nav(page, '连接').focus();
    await page.keyboard.press('Enter');
    // 搜索属于界面上下文：切走再回来仍在。
    await expect(page.getByRole('searchbox', { name: '搜索网卡' })).toHaveValue('USB');
  });
}

test('计划 Tabs 的方向键环绕、Home/End 与 Enter/空格手动激活', async ({ page }) => {
  await consoleApi(page);
  await connectAgent(page);
  await nav(page, '计划').click();
  const tabs = page.getByRole('tablist', { name: '计划编辑区域' }).getByRole('tab');
  await tabs.nth(0).focus();
  await page.keyboard.press('ArrowLeft');
  await expect(tabs.nth(2)).toBeFocused();
  await expect(tabs.nth(0)).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(0)).toBeFocused();
  await page.keyboard.press('End');
  await expect(tabs.nth(2)).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(tabs.nth(2)).toHaveAttribute('aria-selected', 'true');
  await expect(page.getByRole('tabpanel')).toHaveAttribute('id', await tabs.nth(2).getAttribute('aria-controls') as string);
  await page.keyboard.press('Home');
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(1)).toBeFocused();
  await page.keyboard.press('Space');
  await expect(tabs.nth(1)).toHaveAttribute('aria-selected', 'true');
  await expect(tabs.nth(1)).toHaveAttribute('tabindex', '0');
  await expect(tabs.nth(2)).toHaveAttribute('tabindex', '-1');
});

for (const endpoint of ['local', 'connect'] as const) {
  test(`${endpoint} 重扫请求未返回时，不能重复发请求`, async ({ page }) => {
    const state = await consoleApi(page);
    await connectAgent(page);
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    await page.route(`**/api/${endpoint}`, async (route) => {
      await gate;
      await route.fallback();
    });
    const request = page.waitForRequest((r) => new URL(r.url()).pathname === `/api/${endpoint}`);
    await page.getByRole('button', { name: '重新扫描', exact: true }).click();
    await request;
    await expect(page.getByRole('button', { name: '扫描中…', exact: true })).toBeDisabled();
    await expect(page.locator('button[type="submit"]')).toBeDisabled();
    await page.locator('form').evaluate((form: HTMLFormElement) => form.requestSubmit());
    await expect(page.getByRole('button', { name: '扫描中…', exact: true })).toBeDisabled();
    release();
    await expect(page.getByText(/已重新扫描 · 本机 2 块/)).toBeVisible();
    expect(state.connections).toHaveLength(2);
    expect(state.localCalls).toBe(2);
  });
}

async function innerScan(page: Page) {
  await consoleApi(page);
  const state = { reverse: false, probes: [] as Record<string, unknown>[] };
  await page.route('**/api/inner/probe', (route) => {
    const config = route.request().postDataJSON();
    state.probes.push(config);
    const interfaces = [
      { ...nic('LAN-A', '192.168.8.2'), ipv6_ll: 'fe80::2' },
      { ...nic('LAN-B', '192.168.8.3'), ipv6_ll: 'fe80::3' },
      nic('utun7', '192.168.9.2'), nic('LAN-10', '10.1.1.2'), nic('LAN-172', '172.16.1.2'),
    ];
    if (state.reverse) interfaces.reverse();
    return success(route, {
      serial: 'cpe', board_version: '', board_addresses: '', board_counters: '', board_interfaces: [],
      local: { ...host('master'), interfaces },
      agents: config.agents.map((agent: { id: string }) => ({
        id: agent.id, status: 'ready', error: null, info: { ...host('remote'), interfaces },
      })),
    });
  });
  await nav(page, '内环测试').click();
  return state;
}
const scanInner = (page: Page) => page.getByRole('button', { name: '检查 ADB / 扫描各电脑网卡', exact: true }).click();
const innerPicker = (page: Page) => page.getByRole('region', { name: '从扫描结果添加网口' });

for (const field of ['地址', '端口', '令牌（可空，不导出）', 'ADB 序列号（单设备可留空）']) {
  test(`内环待添加选择不能跨越连接身份修改：${field}`, async ({ page }) => {
    await innerScan(page);
    await page.getByText('ADB 与辅测机设置', { exact: true }).click();
    await page.getByRole('button', { name: '添加辅测机', exact: true }).click();
    await page.getByLabel('地址', { exact: true }).fill('192.168.8.200');
    await scanInner(page);
    const pick = () => innerPicker(page).getByRole('checkbox', { name: /agent1.*LAN-B/ });
    await pick().check();
    await expect(pick()).toBeChecked();
    await page.getByLabel(field, { exact: true }).fill(field === '端口' ? '28802' : field === '地址' ? '192.168.8.201' : 'new-identity');
    await expect(innerPicker(page)).toHaveCount(0);
    await scanInner(page);
    // 新机器可以具有完全相同的网卡名和源地址，也必须重新明确选择。
    await expect(pick()).not.toBeChecked();
    await expect(page.getByRole('button', { name: '添加选中的 0 个网口' })).toBeDisabled();
  });
}

test('内环未配置辅测机时可选本机；重扫按网卡身份恢复且隐藏项不串选', async ({ page }) => {
  const state = await innerScan(page);
  await scanInner(page);
  const picker = innerPicker(page);
  await expect(picker.locator('.scan-option')).toHaveCount(2);
  await picker.getByRole('checkbox', { name: /主控本机.*LAN-B/ }).check();
  await picker.getByRole('checkbox', { name: /显示其他网段/ }).check();
  await expect(picker.locator('.scan-option')).toHaveCount(5);
  await expect(picker.getByRole('checkbox', { name: /utun7/ })).not.toBeChecked();
  await expect(picker.getByRole('checkbox', { name: /LAN-10/ })).not.toBeChecked();
  await expect(picker.getByRole('checkbox', { name: /LAN-172/ })).not.toBeChecked();
  state.reverse = true;
  await scanInner(page);
  await expect(picker.getByRole('checkbox', { name: /主控本机.*LAN-B/ })).toBeChecked();
  await expect(picker.getByRole('checkbox', { name: /主控本机.*LAN-A/ })).not.toBeChecked();
  await page.getByRole('button', { name: '添加选中的 1 个网口' }).click();
  await expect(page.getByRole('combobox', { name: '接到 CPE 的电脑网卡', exact: true })).toHaveValue(JSON.stringify(['master', 'LAN-B', '192.168.8.3']));
  await expect(page.getByRole('combobox', { name: '电脑网卡 IPv6', exact: true })).toHaveValue('fe80::3');
  expect(state.probes).toHaveLength(2);
  expect(state.probes.every((probe) => (probe.agents as unknown[]).length === 0)).toBe(true);
});


test('跳过请求绑定显示的单元；目标变化被拒绝后刷新进度', async ({ page }) => {
  await consoleApi(page, 'subnet');
  let target: unknown;
  let advanced = false;
  await page.route('**/api/skip-unit', async (route) => {
    target = route.request().postDataJSON();
    advanced = true;
    return route.fulfill({ json: { ok: false, error: '目标单元已结束或当前运行已变化，请刷新进度后重试' } });
  });
  await page.route('**/api/progress*', async (route) => success(route, {
    ...progress, from: 0, lines: [], units_from: 0, running: true,
    run: { ...progress.run, current: { ...progress.run.current, seq: advanced ? 3 : 2,
      title: advanced ? '下一个单元' : progress.run.current.title } },
  }));
  await nav(page, '执行').click();
  await page.getByRole('button', { name: '跳过当前单元', exact: true }).click();
  expect(target).toEqual({ run_id: progress.run.run_id, unit_seq: 2 });
  await expect(page.getByText('下一个单元', { exact: true })).toBeVisible();
  await expect(page.getByRole('alert')).toContainText('目标单元已结束');
});

test('单独运行的内环测试点「停止测试」真的发出停止请求', async ({ page }) => {
  await consoleApi(page, 'inner');
  let stops = 0;
  await page.route('**/api/inner/stop', (route) => { stops++; return success(route, {}); });
  await nav(page, '内环测试').click();
  await page.getByRole('button', { name: '停止测试', exact: true }).click();
  await expect.poll(() => stops).toBe(1);
});

test('内环多档位输入保留分隔符，真实计划按网口协议门限展开并能保存恢复', async ({ page }) => {
  await innerScan(page);
  const screenshot = page.getByRole('checkbox', { name: '测试结束后截图（参与电脑，每单元一次）', exact: true });
  await expect(screenshot).toBeChecked();
  await screenshot.uncheck();
  await scanInner(page);
  await innerPicker(page).getByRole('checkbox', { name: /主控本机.*LAN-A/ }).check();
  await page.getByRole('button', { name: '添加选中的 1 个网口' }).click();
  await page.getByRole('checkbox', { name: 'IPv6', exact: true }).uncheck();
  await page.getByRole('checkbox', { name: 'UDP', exact: true }).check();
  await page.getByText('高级：统计接口、测量策略与验收门限', { exact: true }).click();
  await page.getByRole('button', { name: '设置 TCP 门限', exact: true }).click();
  await page.getByRole('button', { name: '设置 UDP 门限', exact: true }).click();
  await page.getByLabel('TCP 上行 · 网卡口径 Mbps', { exact: true }).fill('800');
  await page.getByLabel('UDP 上行 · 网卡口径 Mbps', { exact: true }).fill('90');
  await page.getByLabel('UDP 上行 · 工具口径 Mbps', { exact: true }).fill('80');
  await page.getByLabel('UDP 每流速率（Mbps）').fill('100，200');
  await page.getByText('高级：并行流数、窗口、报文与端口', { exact: true }).click();
  const streams = page.getByLabel('TCP 流数');
  await streams.pressSequentially('1 4');
  await expect(streams).toHaveValue('1 4');
  await page.getByLabel('TCP 窗口 -w').fill('64k,4m');
  await page.getByLabel('UDP 报文长度 -l').fill('64 1400');
  await page.getByRole('button', { name: '刷新预览', exact: true }).click();
  await page.getByText('逐单元清单（16 行）', { exact: true }).click();
  const preview = page.getByRole('region', { name: '计划预览', exact: true });
  await expect(preview).toContainText('-P 4 / -w 4m');
  await expect(preview).toContainText('-b 200 Mbps / -l 1400');
  await expect(preview).toContainText('800.000 Mbps');
  await expect(preview).toContainText('90.000 Mbps');
  await expect(preview).toContainText('80.000 Mbps');
  await streams.fill('1 oops');
  await expect(page.getByRole('button', { name: '刷新预览', exact: true })).toBeDisabled();
  await streams.fill('1 4');
  await page.waitForTimeout(300); // 草稿的既有去抖写入
  await page.reload();
  await nav(page, '内环测试').click();
  await page.getByText('高级：并行流数、窗口、报文与端口', { exact: true }).click();
  await expect(streams).toHaveValue('1, 4');
  await expect(screenshot).not.toBeChecked();
  await expect(page.getByLabel('UDP 每流速率（Mbps）')).toHaveValue('100, 200');
});

test('内环结果主行说明双向合计与无有效验收，历史复用不冒充本轮通过', async ({ page }) => {
  await consoleApi(page);
  const leg = (flow: 'up' | 'down', source: string) => ({ flow, port: flow === 'up' ? 56190 : 56191, receiver: flow === 'up' ? 'br0' : 'ETH', receiver_host: flow === 'up' ? '板侧' : 'master', counter_source: null, source, mbps: 900, target_mbps: null, fallback_reason: source === 'tool' ? '计数器不可用' : null, verdict: 'MEASURED', reason: 'TARGET_UNKNOWN', detail: '只记录接收速率', diagnostics: [], nic_rx_mbps: 900, nic_verdict: 'MEASURED', nic_reason: '', nic_target_mbps: null, coverage: 1, effective_secs: 20, required_secs: 20, tool_sender_mbps: 920, tool_receiver_mbps: 900, tool_receiver_note: 'receiver 记录', udp_loss_pct: null, udp_lost_datagrams: null, udp_total_datagrams: null });
  const row = { index: 1, link: 'ETH', host: 'master', ip_version: 4, protocol: 'tcp', direction: 'bidir', streams: 1, parameters: { streams: 1, tcp_window: '4m', udp_mbps: null, udp_length: null }, bidir_targets: { nic_mbps: 1800, tool_mbps: 1700 }, repeat: 1, measurement: 'nic_preferred', verdict: 'NOT_EVALUATED', reason: 'BIDIR_SOURCE_MISMATCH', detail: '两端来源不同，不能合计验收', diagnostics: [], total_mbps: null, total_target_mbps: null, overlap_secs: 20, legs: [leg('up', 'nic'), leg('down', 'tool')] };
  await page.route('**/api/inner/status*', route => success(route, { running: false, current: '', error: null, completed: 2, total: 2, units_from: 0, has_report: true, units: [row, { ...row, index: 2, resumed: true, verdict: 'PASS', legs: [] }] }));
  await nav(page, '内环测试').click();
  const results = page.getByRole('region', { name: '内环执行与结果' });
  await expect(results).toContainText('双向合计 未获取 Mbps'); await expect(results).toContainText('配置网卡合计 1800.00 Mbps');
  await expect(results).toContainText('无法评价（NOT_EVALUATED）'); await expect(results).toContainText('复用历史 PASS');
  await expect(results.getByLabel('本轮结果概览')).toContainText('本轮达标0'); await expect(results).toContainText('-P 1 / -w 4m');
  await results.getByText('测量依据与诊断', { exact: true }).first().click();
  await expect(results).toContainText('已改用工具接收汇总：计数器不可用');
});
