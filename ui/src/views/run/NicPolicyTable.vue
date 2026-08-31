<script setup lang="ts">
import { computed } from 'vue';
import { activeNicPolicies, policyFor, setNicPolicy } from '../../domain/globals';
import { formatEndpoint } from '../../domain/pairs';
import { agentNics, masterNics } from '../../state/inventory';
import { plan } from '../../state/plan';

/**
 * 「按网口门限与负载」：一块网口在**所有**配对里共用的策略。
 *
 * 对应 `RunRequest.nic_policies`（`webui/model.rs::NicPolicySelection`）。
 * 三项都是可选的，三项全空的条目不会发出去。
 *
 * 门限为什么挂在网口上而不是配对上：一块口作为接收端能收多少，主要由它自己
 * 决定，而配对有 N² 条、逐条填不现实。**双向并发**是例外——那时受限的是这条
 * 链路而不是某一端，所以双向门限按任务分方向填，在「测试计划」页的任务上。
 *
 * **这里没有 UDP 包长 `-l`**（`NicPolicySelection.udp_length` 仍在协议里，只是
 * 界面不给入口）：`-l` 属于流量配置，在「测试计划」页每条 TCP/UDP 配置里设置，
 * 全表只留那一处来源。同一个参数开两个入口，最后一定是两处对不上。
 *
 * 写法两种共用一个框：`1800` = 绝对 1800Mbps，`90%` = 协商速率的 90%。
 * **不在前端校验**：语义报错一律交给 `/api/plan`（ADR-11），前端再写一份
 * 「看得懂 90% 吗」就是第二份口径。
 */

interface Row {
  endpoint: string;
  side: '主控' | '辅测';
  name: string;
  role: string;
  ipv4: string;
  speed: string;
}

const rows = computed<Row[]>(() => [
  ...masterNics.value.map((nic) => ({
    endpoint: formatEndpoint('master', nic),
    side: '主控' as const,
    name: nic.name,
    role: nic.role || 'UNKNOWN',
    ipv4: nic.ipv4 || '—',
    // 0 表示「拿不到协商速率」，不是「0 Mbps」；百分比门限换算不出来时要能看见。
    speed: nic.speed_mbps > 0 ? `${nic.speed_mbps} Mbps` : '—',
  })),
  ...agentNics.value.map((nic) => ({
    endpoint: formatEndpoint('agent', nic),
    side: '辅测' as const,
    name: nic.name,
    role: nic.role || 'UNKNOWN',
    ipv4: nic.ipv4 || '—',
    speed: nic.speed_mbps > 0 ? `${nic.speed_mbps} Mbps` : '—',
  })),
]);

const activeCount = computed(() => activeNicPolicies(plan.nicPolicies).length);

function value(endpoint: string, field: 'rx_target' | 'udp_bandwidth'): string {
  return policyFor(plan.nicPolicies, endpoint)[field];
}

function update(endpoint: string, field: 'rx_target' | 'udp_bandwidth', event: Event): void {
  const next = (event.target as HTMLInputElement).value;
  plan.nicPolicies = setNicPolicy(plan.nicPolicies, endpoint, { [field]: next });
}

function clearAll(): void {
  plan.nicPolicies = [];
}
</script>

<template>
  <details class="block">
    <summary>
      <strong>按网口门限与负载</strong>
      <small class="muted">
        {{ activeCount ? `${activeCount} 个网口已设` : '未设，全部走兜底判定' }}
      </small>
    </summary>

    <p class="muted hint">
      「RX 通过门限」是这块网口<strong>作为接收端</strong>时判 PASS 的线：填
      <code>1800</code> 是绝对 1800Mbps，填 <code>90%</code> 是协商速率的 90%。
    </p>

    <div v-if="rows.length === 0" class="empty">
      还没有网口可设。先到「辅测机」页连上对端。
    </div>
    <template v-else>
      <div class="scroll">
        <table>
          <thead>
            <tr>
              <th>端</th>
              <th>网口</th>
              <th>IPv4</th>
              <th class="num">协商速率</th>
              <th>RX 通过门限</th>
              <th>UDP 带宽 <code>-b</code></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in rows" :key="row.endpoint">
              <td class="muted">{{ row.side }}</td>
              <td>
                <strong>{{ row.name }}</strong>
                <br />
                <small class="muted mono">{{ row.role }}</small>
              </td>
              <td class="mono">{{ row.ipv4 }}</td>
              <td class="num mono">{{ row.speed }}</td>
              <td>
                <input
                  type="text"
                  placeholder="1800 或 90%"
                  autocomplete="off"
                  :value="value(row.endpoint, 'rx_target')"
                  :aria-label="`${row.name} 的 RX 通过门限`"
                  @input="update(row.endpoint, 'rx_target', $event)"
                />
              </td>
              <td>
                <input
                  type="text"
                  placeholder="如 1000m"
                  autocomplete="off"
                  :value="value(row.endpoint, 'udp_bandwidth')"
                  :aria-label="`${row.name} 作为发送端的 UDP 带宽`"
                  @input="update(row.endpoint, 'udp_bandwidth', $event)"
                />
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <div class="bar">
        <button type="button" class="ghost" :disabled="activeCount === 0" @click="clearAll">
          清空全部网口策略
        </button>
        <span class="muted">钉死了发送带宽的方向不会再逐档扫描，单元数会随之减少。</span>
      </div>
    </template>
  </details>
</template>

<style scoped>
.block {
  margin: 0 0 14px;
  padding: 10px 12px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--surface);
}
.block > summary { cursor: pointer; display: flex; align-items: baseline; gap: 10px; }
.hint { margin: 8px 0 10px; font-size: 12px; }
.scroll { max-width: 100%; overflow-x: auto; border: 1px solid var(--line); border-radius: 6px; }
table { width: 100%; border-collapse: separate; border-spacing: 0; font-size: 13px; }
th, td { padding: 7px 9px; text-align: left; border-bottom: 1px solid var(--line); vertical-align: top; }
thead th { background: var(--head); font-size: 11.5px; color: var(--muted); white-space: nowrap; }
tbody tr:last-child td { border-bottom: 0; }
.num { text-align: right; font-variant-numeric: tabular-nums; }
input {
  width: 108px;
  padding: 5px 7px;
  border: 1px solid var(--line);
  border-radius: 4px;
  background: var(--canvas);
  color: var(--ink);
  font: inherit;
  font-size: 12.5px;
}
input:focus-visible { outline: 2px solid var(--focus); outline-offset: 1px; }
.bar { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; margin: 10px 0 0; }
.ghost {
  padding: 5px 12px;
  border: 1px solid var(--line);
  border-radius: 4px;
  background: var(--surface);
  color: var(--ink);
  font: inherit;
  font-size: 12.5px;
  cursor: pointer;
}
.ghost:disabled { opacity: .55; cursor: default; }
.empty {
  padding: 12px 14px;
  border: 1px dashed var(--line);
  border-radius: 6px;
  color: var(--muted);
  background: var(--panel-2);
}
.muted { color: var(--muted); }
.mono { font-family: var(--fm); }
code { font-family: var(--fm); }
</style>
