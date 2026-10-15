import { computed } from 'vue';
import type { NicInfo } from '../api/dto';
import { filterByIpv4Prefixes } from '../domain/nics';
import { session } from './session';

/**
 * 拓扑资源：双端网卡表，以及从它派生的候选链路。
 *
 * 这里**只做派生**，不自己发请求：网卡表的所有者是 `session`（它来自
 * `/api/local` 与 `/api/connect` 两个端点）。同一份数据两个所有者，
 * 就是「界面上两处显示的网卡数不一样」那类问题的来源。
 */

/**
 * 连上辅测机后读 `/api/connect` 回包（后端已按前缀扫描）；没连时读
 * `/api/local` 的全量扫描，在这里按同一个前缀筛。
 */
export const masterNics = computed<NicInfo[]>(() => {
  if (session.connection) return session.connection.master.interfaces;
  return filterByIpv4Prefixes(session.local?.host.interfaces ?? [], session.prefixes);
});

/** 没连辅测机时被前缀筛掉的本机网卡数；连上后由后端过滤，恒为 0。 */
export const masterNicsHiddenByPrefix = computed(() =>
  session.connection ? 0 : (session.local?.host.interfaces.length ?? 0) - masterNics.value.length,
);

export const agentNics = computed<NicInfo[]>(() => session.connection?.agent.interfaces ?? []);

export const masterHostname = computed(
  () => session.connection?.master.hostname ?? session.local?.host.hostname ?? '',
);

export const agentHostname = computed(() => session.connection?.agent.hostname ?? '');

/** 本机两口即可配对；跨机计划仍在编译时核验辅测机。 */
export const topologyReady = computed(
  () => masterNics.value.length + agentNics.value.length >= 2,
);
