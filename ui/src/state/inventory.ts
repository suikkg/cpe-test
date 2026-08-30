import { computed } from 'vue';
import type { NicInfo } from '../api/dto';
import { session } from './session';

/**
 * 拓扑资源：双端网卡表，以及从它派生的候选链路。
 *
 * 这里**只做派生**，不自己发请求：网卡表的所有者是 `session`（它来自
 * `/api/local` 与 `/api/connect` 两个端点）。同一份数据两个所有者，
 * 就是「界面上两处显示的网卡数不一样」那类问题的来源。
 */

export const masterNics = computed<NicInfo[]>(
  () => session.connection?.master.interfaces ?? session.local?.host.interfaces ?? [],
);

export const agentNics = computed<NicInfo[]>(() => session.connection?.agent.interfaces ?? []);

export const masterHostname = computed(
  () => session.connection?.master.hostname ?? session.local?.host.hostname ?? '',
);

export const agentHostname = computed(() => session.connection?.agent.hostname ?? '');

/** 双端都扫到网卡了才谈得上配对。 */
export const topologyReady = computed(
  () => masterNics.value.length > 0 && agentNics.value.length > 0,
);
