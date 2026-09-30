import { beforeEach, describe, expect, it } from 'vitest';
import { createSSRApp } from 'vue';
import { renderToString } from 'vue/server-renderer';
import LocalView from './LocalView.vue';
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
  const html = await renderToString(createSSRApp(LocalView));
  return html.match(/<button\b[^>]*>\s*重新扫描\s*<\/button>/)?.[0] ?? '';
}

describe('本机扫描入口', () => {
  it('空闲时可以重新扫描', async () => {
    expect(await rescanButton()).not.toContain('disabled');
    expect(await rescanButton()).not.toBe('');
  });

  it.each(['subnet', 'inner', 'scenario'] as const)('%s 运行时不能从本机页更换双端拓扑', async (kind) => {
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

  it('失败后保留的网卡列表必须标明是旧快照', async () => {
    session.topologyStale = true;
    session.connectedHost = '192.168.1.3';
    const html = await renderToString(createSSRApp(LocalView));
    expect(html).toContain('上次成功连接 192.168.1.3 时的快照');
  });
});
