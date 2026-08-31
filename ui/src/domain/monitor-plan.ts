/**
 * 「哪几路监控该开」的纯逻辑。
 *
 * 拆出来是因为它有两条容易漏的边界，而两条都只在多路监控时才暴露：
 * **同一块网卡不许开两路**，以及**总路数有上限**。
 */

/** 与服务端 `webui/state.rs::MONITOR_MAX_SESSIONS` 同值——一处改两处都要改。 */
export const MONITOR_MAX_SESSIONS = 8;

export type MonitorSide = 'master' | 'agent';

export interface MonitorTarget {
  side: MonitorSide;
  iface: string;
}

/**
 * 这块网卡是不是已经在监控了。
 *
 * 同一块网卡开两路是**纯粹的浪费加误导**：两条曲线读的是同一个内核计数器，
 * 必然一模一样，却各占一个会话名额（总共 8 个），还让人以为自己在对比两件事。
 * 主控和辅测各自的网卡名可能相同（都叫 `eth0`），所以键必须带上端。
 */
export function isMonitored(
  running: readonly MonitorTarget[],
  side: MonitorSide,
  iface: string,
): boolean {
  return running.some((target) => target.side === side && target.iface === iface);
}

/**
 * 「全部开始」实际会开哪几块：还没开的，且不超过总上限。
 *
 * 上限在前端也拦一道，不是不信任服务端——服务端当然会拒（`MONITOR_MAX_SESSIONS`），
 * 但那是**逐个请求**拒的：一次点「全部开始」会先成功几路、再连着报几次错，
 * 而人看到的是一串红字加一堆已经开起来的曲线，说不清到底哪几路开成了。
 */
export function pendingStarts(
  running: readonly MonitorTarget[],
  side: MonitorSide,
  ifaces: readonly string[],
  limit = MONITOR_MAX_SESSIONS,
): string[] {
  const room = Math.max(limit - running.length, 0);
  const out: string[] = [];
  for (const iface of ifaces) {
    if (out.length >= room) break;
    if (!iface) continue;
    if (isMonitored(running, side, iface)) continue;
    if (out.includes(iface)) continue;
    out.push(iface);
  }
  return out;
}

/**
 * 可选的采样间隔（毫秒）。
 *
 * 上限跟着**辅测机**走：agent 侧在 `MonitorMgr::start_owned` 里被夹到
 * 200–5000ms，界面给出更大的值只会变成「agent 按 5 秒采、这边按 10 秒取最后
 * 一个样本」——一半样本无声丢掉。所以两端共用同一份档位，不按端分叉。
 */
export const MONITOR_INTERVALS: ReadonlyArray<{ ms: number; label: string }> = [
  { ms: 200, label: '200 ms（细，采样开销最大）' },
  { ms: 500, label: '500 ms' },
  { ms: 1000, label: '1 秒（默认）' },
  { ms: 2000, label: '2 秒' },
  { ms: 5000, label: '5 秒（粗，看长时间趋势）' },
];
