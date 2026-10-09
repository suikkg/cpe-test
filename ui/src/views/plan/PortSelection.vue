<script setup lang="ts">
import { computed, ref } from 'vue';
import { parseEndpoint } from '../../domain/pairs';
import { assignedPairIds, selectedPortPairs, setPairAssigned, setPairsAssigned } from '../../domain/plan-ports';
import { bindingSelectionState, taskDirectionsLabel, toggleBinding, toggleTaskIp } from '../../domain/plan-build';
import { filterByQuery } from '../../domain/search';
import { masterNics, agentNics } from '../../state/inventory';
import { plan } from '../../state/plan';
import { session } from '../../state/session';
import { goto, ui } from '../../state/ui';

/**
 * 「网口」：按链路集合分组的网口对，给当前选中的测试内容（套件）勾选。
 *
 * 两种分配方式共用这一张表，旧版是两套编辑器：
 * - **组标题的复选框** = 整组分配（`pair_ids: []`），以后扫到的同类网口自动加入。
 *   即旧「分配矩阵」里的一格，走 `toggleBinding` / `bindingSelectionState`。
 * - **行复选框** = 只分配勾过的网口（`setPairAssigned`），扫描新增的不会悄悄加入。
 * 组标题上的「整组 / 3/5」把两者区分开。
 *
 * 「全部 / 跨机 / 同机」只控制显示，不碰集合与分配。旧版切换它会重建集合，
 * 被隐藏集合上的分配跟着被清掉。
 */
const emit = defineEmits<{ (e: 'review'): void; (e: 'edit-suite', id: string): void }>();
type Scope = 'all' | 'cross' | 'same';
const scope = ref<Scope>('all');
const scopes: Array<{ id: Scope; label: string }> = [
  { id: 'all', label: '全部' },
  { id: 'cross', label: '跨机' },
  { id: 'same', label: '同机' },
];

const suiteId = computed({
  get: () => (plan.ui.suites.some((item) => item.id === ui.suites.selected) ? ui.suites.selected : plan.ui.suites[0]?.id ?? ''),
  set: (value: string) => { ui.suites.selected = value; },
});
const suite = computed(() => plan.ui.suites.find((item) => item.id === suiteId.value));
const selected = computed(() => selectedPortPairs(plan.ui));

function endpoint(value: string) {
  const parsed = parseEndpoint(value);
  const nic = (parsed?.side === 'master' ? masterNics.value : agentNics.value)
    .find((item) => item.name === parsed?.name);
  return {
    side: parsed?.side ?? '',
    name: parsed?.name ?? value,
    host: parsed?.side === 'master' ? '主控' : parsed?.side === 'agent' ? '辅测' : '未知',
    address: nic?.ipv4 || nic?.ipv6_global || nic?.ipv6_ll || '无 IP',
    found: !!nic,
  };
}

const groups = computed(() => plan.ui.link_sets.map((set) => {
  const rows = set.pair_refs.map((pair) => {
    const assigned = plan.ui.suites.filter((item) => assignedPairIds(plan.ui, set.id, item.id).has(pair.id));
    const a = endpoint(pair.src);
    const b = endpoint(pair.dst);
    return {
      key: `${set.id}/${pair.id}`, setId: set.id, setName: set.name, pair, a, b,
      cross: a.side !== b.side,
      checked: assigned.some((item) => item.id === suiteId.value),
      otherSuites: assigned.filter((item) => item.id !== suiteId.value).map((item) => item.name),
    };
  });
  const state = suiteId.value ? bindingSelectionState(plan.ui, set.id, suiteId.value) : 'none';
  const whole = plan.ui.bindings.some(
    (binding) => binding.link_set_id === set.id && binding.suite_id === suiteId.value && binding.pair_ids.length === 0,
  );
  const picked = rows.filter((row) => row.checked).length;
  return { set, rows, state, tag: whole ? '整组' : `${picked}/${rows.length}` };
}));

const totalRows = computed(() => groups.value.reduce((sum, group) => sum + group.rows.length, 0));
const shownGroups = computed(() => groups.value
  .map((group) => ({
    ...group,
    shown: filterByQuery(
      group.rows.filter((row) => scope.value === 'all' || row.cross === (scope.value === 'cross')),
      ui.plan.query,
      (row) => [row.setName, row.a.name, row.a.host, row.a.address, row.b.name, row.b.host, row.b.address],
    ),
  }))
  .filter((group) => group.shown.length > 0));
const shownRows = computed(() => shownGroups.value.flatMap((group) => group.shown));

type Row = (typeof groups.value)[number]['rows'][number];
function selectRow(row: Row, checked: boolean): void {
  if (!suiteId.value) return;
  plan.ui = setPairAssigned(plan.ui, row.setId, row.pair.id, suiteId.value, checked);
}
/** 批量只动显示行；扫描中缺失端点的网口不能新勾上。 */
function selectShown(checked: boolean): void {
  if (!suiteId.value) return;
  const rows = shownRows.value
    .filter((row) => !checked || (row.a.found && row.b.found))
    .map((row) => ({ setId: row.setId, pairId: row.pair.id }));
  plan.ui = setPairsAssigned(plan.ui, rows, suiteId.value, checked);
}
function toggleGroup(setId: string): void {
  if (suiteId.value) plan.ui = toggleBinding(plan.ui, setId, suiteId.value);
}
function changeTaskIp(taskId: string, ip: string): void {
  if (suite.value) plan.ui = toggleTaskIp(plan.ui, suite.value.id, taskId, ip);
}
</script>

<template>
  <section class="ports" aria-label="选择要互测的网口">
    <div class="suite-bar">
      <label class="suite-pick">
        <span>测试内容</span>
        <select v-model="suiteId">
          <option v-for="item in plan.ui.suites" :key="item.id" :value="item.id">{{ item.name }}</option>
        </select>
      </label>
      <button v-if="suite" type="button" class="ghost small" @click="emit('edit-suite', suite.id)">编辑</button>
      <div v-if="suite" class="tasks">
        <span v-for="task in suite.tasks" :key="task.id" class="task">
          <strong>{{ task.protocol.toUpperCase() }}</strong>
          <span class="muted">{{ taskDirectionsLabel(task) }}</span>
          <label v-for="ip in ['v4', 'v6']" :key="ip" class="check">
            <input
              type="checkbox"
              :checked="task.ip.includes(ip)"
              :disabled="task.ip.length === 1 && task.ip.includes(ip)"
              :aria-label="`${task.name || task.protocol} 使用 ${ip === 'v6' ? 'IPv6' : 'IPv4'}`"
              @change="changeTaskIp(task.id, ip)"
            >{{ ip === 'v6' ? 'IPv6' : 'IPv4' }}
          </label>
        </span>
      </div>
    </div>

    <p v-if="session.topologyStale" class="msg warn" role="status">
      网卡信息来自上次扫描，<button type="button" class="linklike" @click="goto('connect')">重新扫描</button>确认网口没有变化。
    </p>
    <p v-if="plan.stale.length" class="msg warn" role="status">
      {{ plan.stale.length }} 对网口在当前扫描中找不到，已保留并标出。
    </p>

    <div v-if="totalRows" class="toolbar">
      <div class="seg" role="group" aria-label="显示范围">
        <button
          v-for="item in scopes"
          :key="item.id"
          type="button"
          :aria-pressed="scope === item.id"
          @click="scope = item.id"
        >{{ item.label }}</button>
      </div>
      <label class="grow">
        <span class="sr-only">搜索要测试的网口</span>
        <input
          type="search"
          :value="ui.plan.query"
          placeholder="搜索网口名称、IP 或电脑"
          @input="ui.plan.query = ($event.target as HTMLInputElement).value"
        >
      </label>
      <button type="button" class="ghost small" @click="selectShown(true)">全选显示</button>
      <button type="button" class="ghost small" :disabled="!shownRows.some((row) => row.checked)" @click="selectShown(false)">取消显示</button>
      <span class="count">显示 {{ shownRows.length }} / {{ totalRows }} 对</span>
    </div>

    <div v-if="!totalRows" class="empty-state">
      <p>请扫描网卡。可配对两台电脑的网口，或本机的两个网口。</p>
      <button type="button" @click="goto('connect')">去连接</button>
    </div>
    <p v-else-if="!shownRows.length" class="empty-state">
      没有匹配的网口。<button type="button" class="linklike" @click="ui.plan.query = ''; scope = 'all'">清空筛选</button>
    </p>
    <div v-else class="table-wrap" tabindex="0" role="region" aria-label="网口选择表，可横向滚动">
      <table class="data">
        <thead>
          <tr><th scope="col" class="pick-col">选择</th><th scope="col">网口 A</th><th scope="col">网口 B</th><th scope="col">另测</th></tr>
        </thead>
        <tbody v-for="group in shownGroups" :key="group.set.id">
          <tr class="group-row">
            <th scope="rowgroup" colspan="4">
              <label class="check">
                <input
                  type="checkbox"
                  :checked="group.state === 'all'"
                  :indeterminate="group.state === 'some'"
                  :aria-checked="group.state === 'some' ? 'mixed' : group.state === 'all'"
                  :aria-label="`${group.set.name} 整组：${suite?.name}`"
                  @change="toggleGroup(group.set.id)"
                >
                <strong>{{ group.set.name }}</strong>
              </label>
              <span class="tag" :class="{ whole: group.tag === '整组' }">{{ group.tag }}</span>
            </th>
          </tr>
          <tr v-for="row in group.shown" :key="row.key" :class="{ selected: row.checked }">
            <td class="pick-col">
              <input
                type="checkbox"
                :checked="row.checked"
                :disabled="!row.checked && (!row.a.found || !row.b.found)"
                :aria-label="`${row.a.host} ${row.a.name} 与 ${row.b.host} ${row.b.name}：${suite?.name}`"
                @change="selectRow(row, ($event.target as HTMLInputElement).checked)"
              >
            </td>
            <td>
              <strong>{{ row.a.name }}</strong>
              <small>{{ row.a.host }} · {{ row.a.address }}</small>
              <small v-if="!row.a.found" class="missing">扫描中未发现</small>
            </td>
            <td>
              <strong>{{ row.b.name }}</strong>
              <small>{{ row.b.host }} · {{ row.b.address }}</small>
              <small v-if="!row.b.found" class="missing">扫描中未发现</small>
            </td>
            <td class="muted">{{ row.otherSuites.join('、') || '—' }}</td>
          </tr>
        </tbody>
      </table>
    </div>

    <div class="footer">
      <span class="count">{{ selected ? `已选 ${selected} 对网口` : '至少选择一对网口' }}</span>
      <button type="button" :disabled="!selected || plan.previewing" @click="emit('review')">
        {{ plan.previewing ? '正在生成预览…' : '预览与执行' }}
      </button>
    </div>
  </section>
</template>

<style scoped>
.suite-bar { display: flex; align-items: center; flex-wrap: wrap; gap: 10px 14px; }
.suite-pick { display: flex; align-items: center; gap: 10px; font-weight: 600; }
.suite-pick select { min-width: 160px; }
.tasks { display: flex; flex-wrap: wrap; gap: 6px 18px; font-size: 13px; }
.task { display: inline-flex; align-items: center; gap: 8px; }
.task .check { font-size: 12.5px; }
.pick-col { width: 64px; }
.pick-col input { width: 17px; height: 17px; }
td { min-width: 150px; overflow-wrap: anywhere; }
td.pick-col { min-width: 0; }
.group-row th { background: var(--panel-2); color: var(--ink); font-size: 13px; }
.group-row .check { gap: 8px; }
.group-row input { width: 17px; height: 17px; }
.tag {
  margin-left: 10px; padding: 1px 8px; border: 1px solid var(--line); border-radius: 10px;
  color: var(--muted); font: 500 11px/1.6 var(--fm);
}
.tag.whole { border-color: var(--accent); color: var(--accent); }
tr.selected { background: var(--edited); }
.missing { color: var(--warn) !important; }
.footer { display: flex; align-items: center; justify-content: flex-end; flex-wrap: wrap; gap: 8px 14px; margin-top: 16px; }
.count { color: var(--muted); font-size: 12.5px; }
@media (max-width: 700px) { .footer > .count { flex-basis: 100%; text-align: right; } .footer > button { width: 100%; } }
</style>
