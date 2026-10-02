import { beforeEach, describe, expect, it } from 'vitest';
import { createSSRApp } from 'vue';
import { renderToString } from 'vue/server-renderer';
import ConnectView from './ConnectView.vue';
import { inner } from '../../state/inner';
import { reset as resetRun, run } from '../../state/run';
import { reset as resetSession, session } from '../../state/session';

beforeEach(() => {
  resetRun();
  resetSession();
  inner.status.running = false;
  inner.scenario.running = false;
  inner.scenarioStartPhase = 'idle';
});

async function rescanButton(): Promise<string> {
  const html = await renderToString(createSSRApp(ConnectView));
  return html.match(/<button\b[^>]*>\s*重新扫描\s*<\/button>/)?.[0] ?? '';
}

describe('连接页的扫描入口', () => {
  it('空闲时可以重新扫描', async () => {
    expect(await rescanButton()).not.toContain('disabled');
    expect(await rescanButton()).not.toBe('');
  });

  it.each(['subnet', 'inner', 'scenario'] as const)('%s 运行时不能更换双端拓扑', async (kind) => {
    if (kind === 'subnet') run.running = true;
    if (kind === 'inner') inner.status.running = true;
    if (kind === 'scenario') inner.scenario.running = true;
    expect(await rescanButton()).toContain('disabled');
  });

  it.each(['sending', 'unknown'] as const)('组合场景 %s 时不能重扫', async (phase) => {
    inner.scenarioStartPhase = phase;
    expect(await rescanButton()).toContain('disabled');
  });

  it('连接进行中不能再次扫描', async () => {
    session.phase = 'connecting';
    expect(await rescanButton()).toContain('disabled');
  });

  it('运行中连同表单提交按钮一起锁住', async () => {
    run.running = true;
    const html = await renderToString(createSSRApp(ConnectView));
    expect(html.match(/<button[^>]*type="submit"[^>]*>/)?.[0] ?? '').toContain('disabled');
  });

  it('失败后保留的网卡列表必须标明是旧快照', async () => {
    session.topologyStale = true;
    session.connectedHost = '192.168.1.3';
    const html = await renderToString(createSSRApp(ConnectView));
    expect(html).toContain('上次成功连接 192.168.1.3 时的快照');
  });

  it('连接失败时在同一条提示里说明下面是旧网卡', async () => {
    session.phase = 'failed';
    session.error = '连接超时';
    session.topologyStale = true;
    session.connectedHost = '192.168.1.3';
    const html = await renderToString(createSSRApp(ConnectView));
    const alert = html.match(/<p class="msg bad" role="alert"[^>]*>([\s\S]*?)<\/p>/)?.[1] ?? '';
    expect(alert).toContain('连接超时');
    expect(alert).toContain('下面仍是上次成功连接 192.168.1.3 时的网卡');
  });
});
