import { reactive } from 'vue';
import { api } from '../api/client';
import type { MonitorPoint, MonitorSeriesOut } from '../api/dto';
import { appendPoints } from '../domain/monitor-chart';
import { isMonitored, pendingStarts, type MonitorSide } from '../domain/monitor-plan';

/**
 * 监控资源：网卡速率曲线的会话表。
 *
 * **和一轮测试正交**——边跑边看正是它最有用的场景，所以不受 `running` 约束。
 * 轮询同样归本模块所有：切到别的页不停采样，否则回来时曲线是断的。
 */

export interface MonitorSession {
  session: string;
  side: MonitorSide;
  iface: string;
  points: MonitorPoint[];
  /** 服务端游标：下一拍从这里取 */
  from: number;
  running: boolean;
  error: string;
  /** 这一路实际下发的采样间隔（毫秒），用来在图上标出粒度 */
  intervalMs: number;
}

export const monitor = reactive({
  sessions: [] as MonitorSession[],
  starting: false,
  error: '',
  polling: false,
});

export function reset(): void {
  stopPolling();
  monitor.sessions = [];
  monitor.starting = false;
  monitor.error = '';
}

let timer: ReturnType<typeof setTimeout> | undefined;

/** setTimeout 链，不是 setInterval——机器忙时请求不许堆叠。 */
function schedule(): void {
  timer = setTimeout(() => void tick(), 1000);
}

async function tick(): Promise<void> {
  if (!monitor.polling) return;
  if (monitor.sessions.length > 0) {
    try {
      // **一次问完全部在跑的监控。** 每路各发一次也能 work，但浏览器对同一个源
      // 的并发连接就那么几条：8 路监控 + 进度轮询会把它占满，日志那一路开始
      // 一秒一顿。
      const out = await api.post<{ series: MonitorSeriesOut[] }>('/api/monitor/samples', {
        cursors: monitor.sessions.map((s) => ({ session: s.session, from: s.from })),
      });
      for (const series of out.series ?? []) {
        const target = monitor.sessions.find((s) => s.session === series.session);
        if (!target) continue;
        target.points = appendPoints(target.points, series.points);
        target.from = series.from;
        target.running = series.running;
        target.error = series.error;
      }
    } catch {
      // 断线自愈：下一拍重试。
    }
  }
  if (monitor.polling) schedule();
}

export function startPolling(): void {
  if (monitor.polling) return;
  monitor.polling = true;
  void tick();
}

export function stopPolling(): void {
  monitor.polling = false;
  if (timer !== undefined) {
    clearTimeout(timer);
    timer = undefined;
  }
}

/**
 * 起一路监控。
 *
 * **同一端的同一块网卡不许开两路**：两条曲线读的是同一个内核计数器，必然一模
 * 一样，却各占一个会话名额（总共 8 个），还让人以为自己在对比两件事。界面上
 * 那一项也会被禁掉，这里是不走界面时的那道。
 */
export async function startSession(
  side: MonitorSide,
  iface: string,
  intervalMs: number,
): Promise<boolean> {
  if (!iface) return false;
  if (isMonitored(monitor.sessions, side, iface)) {
    monitor.error = `${side === 'master' ? '主控' : '辅测'} ${iface} 已经在监控了`;
    return false;
  }
  monitor.starting = true;
  monitor.error = '';
  try {
    const out = await api.post<{ session: string }>('/api/monitor/start', {
      side,
      iface,
      interval_ms: intervalMs,
    });
    monitor.sessions.push({
      session: out.session,
      side,
      iface,
      points: [],
      from: 0,
      running: true,
      error: '',
      intervalMs,
    });
    startPolling();
    return true;
  } catch (error) {
    monitor.error = error instanceof Error ? error.message : String(error);
    return false;
  } finally {
    monitor.starting = false;
  }
}

/**
 * 把某一端还没开的网卡一次全开起来。
 *
 * 上限在这里先算好（`pendingStarts`），不是不信任服务端——服务端当然会拒，
 * 但那是**逐个请求**拒的：一次点下去会先成功几路、再连着报几次错，人看到的是
 * 一串红字加一堆已经开起来的曲线，说不清到底哪几路开成了。
 *
 * 串行起：并发发 8 个 `/api/monitor/start` 会同时抢那个 `MONITOR_MAX_SESSIONS`
 * 的名额判断，而每一路都要真的起一条采样线程。
 */
export async function startAll(
  side: MonitorSide,
  ifaces: readonly string[],
  intervalMs: number,
): Promise<number> {
  const pending = pendingStarts(monitor.sessions, side, ifaces);
  let started = 0;
  for (const iface of pending) {
    if (await startSession(side, iface, intervalMs)) started += 1;
    else break; // 一路失败通常意味着后面都会失败（撞上限、辅测机没连上）
  }
  return started;
}

export async function stopSession(session: string): Promise<void> {
  try {
    await api.post('/api/monitor/stop', { session });
  } catch {
    // 停不掉也要把本地那一路摘掉：服务端有空闲超时兜底。
  }
  monitor.sessions = monitor.sessions.filter((s) => s.session !== session);
  if (monitor.sessions.length === 0) stopPolling();
}

/** 全停。切换辅测机或退出时用——旧页是串行发的，8 路就是 8 个 RTT。 */
export async function stopAll(): Promise<void> {
  const ids = monitor.sessions.map((s) => s.session);
  await Promise.allSettled(ids.map((id) => api.post('/api/monitor/stop', { session: id })));
  monitor.sessions = [];
  stopPolling();
}
