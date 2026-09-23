<script setup lang="ts">
import { computed, ref } from 'vue';
import { parseEndpoint } from '../../domain/pairs';
import { assignedPairIds, selectedPortPairs, setPairAssigned } from '../../domain/plan-ports';
import { taskDirectionsLabel, toggleTaskIp } from '../../domain/plan-build';
import { filterByQuery } from '../../domain/search';
import { masterNics, agentNics } from '../../state/inventory';
import { plan } from '../../state/plan';
import { session } from '../../state/session';
import { goto } from '../../state/ui';

const emit = defineEmits<{ (e: 'review'): void; (e: 'edit-suite', id: string): void }>();
const chosenSuite = ref('');
const search = ref('');
const suite = computed(() => plan.ui.suites.find((item) => item.id === chosenSuite.value) ?? plan.ui.suites[0]);
const selected = computed(() => selectedPortPairs(plan.ui));
function endpoint(value: string) {
  const parsed = parseEndpoint(value);
  const nic = (parsed?.side === 'master' ? masterNics.value : agentNics.value)
    .find((item) => item.name === parsed?.name);
  return {
    name: parsed?.name ?? value,
    host: parsed?.side === 'master' ? '主控' : parsed?.side === 'agent' ? '辅测机' : '未知电脑',
    address: nic?.ipv4 || nic?.ipv6_global || nic?.ipv6_ll || '未取得 IP',
    found: !!nic,
  };
}
const allRows = computed(() => plan.ui.link_sets.flatMap((set) => set.pair_refs.map((pair) => {
  const assigned = plan.ui.suites.filter((item) => assignedPairIds(plan.ui, set.id, item.id).has(pair.id));
  return {
    key: `${set.id}/${pair.id}`, setId: set.id, setName: set.name, pair,
    a: endpoint(pair.src), b: endpoint(pair.dst),
    checked: assigned.some((item) => item.id === suite.value?.id),
    otherSuites: assigned.filter((item) => item.id !== suite.value?.id).map((item) => item.name),
  };
})));
const rows = computed(() => filterByQuery(allRows.value, search.value, (row) => [
  row.setName, row.a.name, row.a.host, row.a.address, row.b.name, row.b.host, row.b.address,
]));
const selectedShown = computed(() => rows.value.filter((row) => row.checked).length);
function selectRow(row: typeof allRows.value[number], checked: boolean): void {
  if (!suite.value) return;
  plan.ui = setPairAssigned(plan.ui, row.setId, row.pair.id, suite.value.id, checked);
}
function selectShown(checked: boolean): void {
  for (const row of rows.value) {
    if (!checked || (row.a.found && row.b.found)) selectRow(row, checked);
  }
}
function changeTaskIp(taskId: string, ip: string): void {
  if (suite.value) plan.ui = toggleTaskIp(plan.ui, suite.value.id, taskId, ip);
}
</script>

<template>
  <section class="port-picker" aria-labelledby="port-picker-title">
    <div class="picker-heading">
      <div>
        <h3 id="port-picker-title">选择要互测的网口</h3>
        <p>每行是两个网口。勾选后，两端按所选测试内容依次测试；A、B 对应下方的实际网口。</p>
      </div>
      <span class="selection-total" role="status">已选 {{ selected }} 对网口</span>
    </div>
    <div class="suite-choice">
      <label>测试内容
        <select :value="suite?.id ?? ''" @change="chosenSuite = ($event.target as HTMLSelectElement).value">
          <option v-for="item in plan.ui.suites" :key="item.id" :value="item.id">{{ item.name }}</option>
        </select>
      </label>
      <button v-if="suite" type="button" class="ghost" @click="emit('edit-suite', suite.id)">调整测试内容</button>
    </div>
    <div v-if="suite" class="suite-detail">
      <div v-for="task in suite.tasks" :key="task.id" class="task-summary">
        <span><strong>{{ task.protocol.toUpperCase() }}</strong>：{{ taskDirectionsLabel(task) }}</span>
        <label v-for="ip in ['v4', 'v6']" :key="ip">
          <input type="checkbox" :checked="task.ip.includes(ip)" :disabled="task.ip.length === 1 && task.ip.includes(ip)"
            :aria-label="`${task.name || task.protocol} 使用 ${ip === 'v6' ? 'IPv6' : 'IPv4'}`"
            @change="changeTaskIp(task.id, ip)">{{ ip === 'v6' ? 'IPv6' : 'IPv4' }}
        </label>
      </div>
    </div>
    <p class="muted">两端都有对应地址才能测试该 IP 版本；仅需 IPv4 时可取消 IPv6。这里的修改适用于使用此测试内容的所有已选网口。</p>
    <p class="muted">TCP / UDP 测吞吐，PING 测连通性与时延。首次使用可保留默认参数；实际能跑的项目和耗时在下一步预览中确认。</p>
    <p v-if="session.topologyStale" class="notice" role="status">
      网卡信息来自上次扫描。请先<button type="button" class="text-button" @click="goto('agent')">重新连接并扫描</button>，确认网口没有变化。
    </p>
    <div v-if="allRows.length" class="picker-tools">
      <input v-model="search" type="search" placeholder="搜索网口名称、IP 或电脑" aria-label="搜索要测试的网口">
      <button type="button" class="ghost" @click="selectShown(true)">选择显示的网口</button>
      <button type="button" class="ghost" :disabled="!selectedShown" @click="selectShown(false)">取消显示的选择</button>
      <small>显示 {{ rows.length }} / {{ allRows.length }} 对；批量操作只影响显示行的「{{ suite?.name }}」</small>
    </div>
    <div v-if="!allRows.length" class="picker-empty">
      <strong>还没有可配对的网口</strong>
      <p>先确认网线或 Wi-Fi 已连接，在「本机」查看网卡，再连接辅测机。网口缺失时检查 IP 前缀过滤；同机测试也需要两个网口。</p>
      <button type="button" class="ghost" @click="goto('local')">检查本机网卡</button>
      <button type="button" @click="goto('agent')">连接辅测机</button>
    </div>
    <p v-else-if="!rows.length" class="picker-empty">没有匹配的网口。<button class="text-button" type="button" @click="search = ''">清空搜索</button></p>
    <div v-else class="port-scroll" tabindex="0" aria-label="网口选择表，可横向滚动">
      <table>
        <thead><tr><th scope="col">本轮测试</th><th scope="col">网口 A</th><th scope="col">网口 B</th><th scope="col">当前安排</th></tr></thead>
        <tbody>
          <tr v-for="row in rows" :key="row.key" :class="{ selected: row.checked }">
            <td><label class="port-check">
              <input type="checkbox" :checked="row.checked" :disabled="!row.checked && (!row.a.found || !row.b.found)"
                :aria-label="`${row.a.host} ${row.a.name} 与 ${row.b.host} ${row.b.name}：${suite?.name}`"
                @change="selectRow(row, ($event.target as HTMLInputElement).checked)">
              {{ row.checked ? '已选择' : '选择' }}
            </label></td>
            <td><strong>{{ row.a.name }}</strong><span>{{ row.a.host }} · {{ row.a.address }}</span><small v-if="!row.a.found" class="missing">扫描中未发现</small></td>
            <td><strong>{{ row.b.name }}</strong><span>{{ row.b.host }} · {{ row.b.address }}</span><small v-if="!row.b.found" class="missing">扫描中未发现</small></td>
            <td><strong>{{ row.checked ? suite?.name : '未选择当前测试内容' }}</strong>
              <span v-if="row.otherSuites.length">另测：{{ row.otherSuites.join('、') }}</span>
              <small>{{ row.setName }}</small>
              <button v-if="!row.a.found || !row.b.found" type="button" class="text-button" @click="goto('agent')">检查连接与网卡</button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <div class="picker-footer">
      <p>{{ selected ? `已安排 ${selected} 对网口。预览会列出每个方向的参数、门限和耗时。` : '至少选择一对网口，再复核并开始测试。' }}<br><small>搜索只隐藏列表行；已选网口仍会参与测试。</small></p>
      <button type="button" :disabled="!selected || plan.previewing" @click="emit('review')">{{ plan.previewing ? '正在生成预览…' : '下一步：预览与执行' }}</button>
    </div>
  </section>
</template>

<style scoped>
.port-picker { margin: 18px 0 24px; padding: 20px; border: 1px solid var(--line); border-top: 3px solid var(--accent); border-radius: 7px; background: var(--surface); }
.picker-heading, .suite-choice, .picker-tools, .picker-footer { display: flex; align-items: center; justify-content: space-between; flex-wrap: wrap; gap: 12px; }
h3, p { margin: 0; }
.picker-heading p, .muted, .picker-footer p { margin-top: 7px; color: var(--muted); font-size: 13px; line-height: 1.65; }
.selection-total { color: var(--accent); font-weight: 700; white-space: nowrap; }
.suite-choice { justify-content: flex-start; margin: 18px 0 8px; }
.suite-choice label { display: flex; align-items: center; gap: 12px; font-weight: 600; }
select, input[type='search'] { min-height: 40px; padding: 8px 10px; border: 1px solid var(--line); border-radius: 5px; background: var(--surface); color: var(--ink); font: inherit; max-width: 100%; }
.suite-detail { display: flex; flex-wrap: wrap; gap: 8px 22px; font-size: 13px; line-height: 1.6; }
.task-summary { display: flex; align-items: center; flex-wrap: wrap; gap: 8px 14px; padding: 8px 0; }
.task-summary label { display: inline-flex; align-items: center; gap: 6px; }
.task-summary input { accent-color: var(--accent); }
.picker-tools { justify-content: flex-start; margin: 18px 0 10px; }
.picker-tools input { flex: 1 1 220px; }
.picker-tools small { flex-basis: 100%; color: var(--muted); }
.port-scroll { overflow-x: auto; border: 1px solid var(--line); border-radius: 5px; }
table { width: 100%; border-collapse: collapse; font-size: 13px; }
th { background: var(--head); text-align: left; color: var(--muted); font-size: 12px; }
th, td { padding: 11px 14px; border-bottom: 1px solid var(--line); }
tbody tr:last-child td { border-bottom: 0; }
tr.selected { background: var(--edited); }
td { min-width: 160px; vertical-align: top; overflow-wrap: anywhere; }
td:first-child { min-width: 95px; }
td strong, td span, td small { display: block; }
td span, td small { margin-top: 5px; color: var(--muted); }
.port-check { display: flex; align-items: center; gap: 8px; min-height: 36px; cursor: pointer; white-space: nowrap; }
.port-check input { width: 18px; height: 18px; accent-color: var(--accent); }
.missing { color: var(--warn); }
.notice { margin-top: 14px; padding: 10px; background: var(--info-bg); color: var(--warn); }
.picker-empty { padding: 22px 0; line-height: 1.8; }
.picker-empty button { margin: 10px 10px 0 0; }
.picker-footer { margin-top: 16px; }
.ghost { background: var(--surface); color: var(--ink); border: 1px solid var(--line); }
.text-button { display: inline; padding: 0 3px; min-height: 0; background: none; color: var(--accent); border: 0; font: inherit; text-decoration: underline; }
@media (max-width: 700px) { .port-picker { padding: 14px; } .suite-choice label { flex-wrap: wrap; } .picker-footer > button { width: 100%; } }
</style>
