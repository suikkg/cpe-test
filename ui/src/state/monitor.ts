import { reactive, watch } from 'vue';
import { api, errorMessage, UnauthorizedError } from '../api/client';
import { session } from './session';
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
  /** 启动时确认的主机身份；切换辅测机后旧曲线不能冒充新机器。 */
  host: string;
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
  notice: '',
  refreshError: '',
  polling: false,
});

export function reset(): void {
  generation += 1;
  stopPolling();
  monitor.sessions = [];
  monitor.starting = false;
  monitor.error = '';
  monitor.notice = '';
  monitor.refreshError = '';
}

let timer: ReturnType<typeof setTimeout> | undefined;
let generation = 0;
let pollEpoch = 0;
let agentGeneration = 0;

function agentIdentity(): string {
  return session.connectedHost ? `${session.connectedHost}:${session.connectedPort}` : '';
}

watch(agentIdentity, () => {
  agentGeneration += 1;
  const previous = monitor.sessions.filter((item) => item.side === 'agent');
  if (!previous.length) return;
  // 本机监控可以继续；旧辅测会话要按自己的 session ID 回收，不带新机器身份。
  monitor.sessions = monitor.sessions.filter((item) => item.side !== 'agent');
  monitor.notice = '辅测机连接已切换，已结束旧辅测机的监控显示；请为当前辅测机重新选择网卡。';
  if (!monitor.sessions.length) stopPolling();
  void Promise.allSettled(previous.map((item) => api.post('/api/monitor/stop', { session: item.session })));
}, { flush: 'sync' });

/** setTimeout 链，不是 setInterval——机器忙时请求不许堆叠。 */
function schedule(epoch: number): void {
  if (timer !== undefined) clearTimeout(timer);
  timer = setTimeout(() => { timer = undefined; void tick(epoch); }, 1000);
}

async function tick(epoch: number): Promise<void> {
  if (!monitor.polling || epoch !== pollEpoch) return;
  if (monitor.sessions.length > 0) {
    try {
      // **一次问完全部在跑的监控。** 每路各发一次也能 work，但浏览器对同一个源
      // 的并发连接就那么几条：8 路监控 + 进度轮询会把它占满，日志那一路开始
      // 一秒一顿。
      const out = await api.post<{ series: MonitorSeriesOut[] }>('/api/monitor/samples', {
        cursors: monitor.sessions.map((s) => ({ session: s.session, from: s.from })),
      });
      if (epoch !== pollEpoch) return;
      monitor.refreshError = '';
      for (const series of out.series ?? []) {
        const target = monitor.sessions.find((s) => s.session === series.session);
        if (!target) continue;
        target.points = appendPoints(target.points, series.points);
        target.from = series.from;
        target.running = series.running;
        target.error = series.error;
      }
    } catch (error) {
      if (epoch !== pollEpoch) return;
      if (error instanceof UnauthorizedError) {
        // 口令失效不是断线，**自愈不了**：继续按秒重试只会刷出一串 401，而屏幕
        // 上什么都不会变——曲线就那么静止着，没有一处说它已经不再更新了。而这台
        // 机器此刻多半正在灌线速，那串请求还得它自己扛。停掉这条链，交给全局
        // 终态说话，和 `run.ts` 的进度轮询同一套处理。
        session.phase = 'unauthorized';
        stopPolling();
        return;
      }
      // 其余的断线自愈：下一拍重试。
      monitor.refreshError = errorMessage(error);
    }
  }
  if (monitor.polling && epoch === pollEpoch) schedule(epoch);
}

export function startPolling(): void {
  if (monitor.polling) return;
  monitor.polling = true;
  void tick(++pollEpoch);
}

export function stopPolling(): void {
  pollEpoch += 1;
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
  if (!iface || monitor.starting) return false;
  if (side === 'agent' && (session.phase !== 'connected' || session.topologyStale)) {
    monitor.error = '请先成功连接辅测机，再开始监控。';
    return false;
  }
  if (isMonitored(monitor.sessions, side, iface)) {
    monitor.error = `${side === 'master' ? '主控' : '辅测'} ${iface} 已经在监控了`;
    return false;
  }
  monitor.starting = true;
  monitor.error = '';
  const epoch = generation;
  const agentEpoch = agentGeneration;
  const host = side === 'agent' ? agentIdentity()
    : session.connection?.master.hostname ?? session.local?.host.hostname ?? '主控本机';
  try {
    const out = await api.post<{ session: string }>('/api/monitor/start', {
      side,
      iface,
      interval_ms: intervalMs,
    });
    if (epoch !== generation || (side === 'agent' && agentEpoch !== agentGeneration)) {
      // 请求途中换机或停止全部时，迟到的启动应答不能重新挂回界面。
      await api.post('/api/monitor/stop', { session: out.session }).catch(() => undefined);
      return false;
    }
    monitor.sessions.push({
      session: out.session,
      side,
      iface,
      host,
      points: [],
      from: 0,
      running: true,
      error: '',
      intervalMs,
    });
    monitor.notice = '';
    startPolling();
    return true;
  } catch (error) {
    if (epoch !== generation || (side === 'agent' && agentEpoch !== agentGeneration)) return false;
    monitor.error = errorMessage(error);
    return false;
  } finally {
    if (epoch === generation) monitor.starting = false;
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
  generation += 1;
  monitor.starting = false;
  const ids = monitor.sessions.map((s) => s.session);
  monitor.sessions = [];
  stopPolling();
  await Promise.allSettled(ids.map((id) => api.post('/api/monitor/stop', { session: id })));
}
