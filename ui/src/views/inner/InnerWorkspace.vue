<script setup lang="ts">
import { computed } from 'vue';
import { inner } from '../../state/inner';
import { emptyInnerParameterOptions, emptyInnerThresholds, INNER_DIRECTIONS, INNER_IP_VERSIONS, DIRECTION_LABEL, type InnerProtocol, type InnerConfig, defaultInnerConfig } from '../../domain/inner';
import { innerGroupLinks, initialInnerWorkspace, nextInnerId, setInnerAssigned, type InnerTask, type InnerRecipe, type InnerGroup } from '../../domain/inner-workspace';
import UiTabs from '../../components/UiTabs.vue';
import InnerParameterInput from './InnerParameterInput.vue';
import InnerDefaults from './InnerDefaults.vue';
import InnerPingOptions from './InnerPingOptions.vue';
defineProps<{ disabled: boolean }>();
const emit = defineEmits<{ review: [] }>();
const tab = computed({ get: () => inner.workspaceTab, set: (v: string) => { inner.workspaceTab = v as typeof inner.workspaceTab; } });
const selectedSuiteId = computed({ get: () => inner.selectedSuiteId, set: (v:string) => { inner.selectedSuiteId = v; } });
const suiteId = computed({ get: () => w.value?.suites.some(s => s.id === selectedSuiteId.value) ? selectedSuiteId.value : w.value?.suites[0]?.id ?? '', set: (id: string) => { selectedSuiteId.value = id; } });
const w = computed(() => inner.config.workspace!);
const currentSuite = computed(() => w.value?.suites.find(s => s.id === suiteId.value) ?? w.value?.suites[0]);
function activate(): void { inner.config.workspace = initialInnerWorkspace(inner.config); }
function addGroup(): void { w.value.groups.push({ id: nextInnerId(w.value.groups, 'group'), name: '新链路集合', links: [] }); }
function addSuite(): void { const id = nextInnerId(w.value.suites, 'suite'); w.value.suites.push({ id, name: '新套件', tasks: [] }); suiteId.value = id; }
function removeGroup(id: string): void { w.value.groups = w.value.groups.filter(g => g.id !== id); w.value.bindings = w.value.bindings.filter(b => b.group !== id); }
function removeSuite(id: string): void { w.value.suites = w.value.suites.filter(g => g.id !== id); w.value.bindings = w.value.bindings.filter(b => b.suite !== id); }
function addTask(protocol: InnerProtocol): void {
  const suite = currentSuite.value; if (!suite) return;
  let recipe = w.value.recipes.find(r => r.protocol === protocol);
  if (protocol !== 'ping' && !recipe) {
    recipe = { id: nextInnerId(w.value.recipes, 'recipe'), name: protocol.toUpperCase(), protocol, parameters: { ...emptyInnerParameterOptions(), ...(protocol === 'udp' ? { udp_rates_mbps: [2500], udp_lengths: ['1400'], udp_streams: [1] } : { tcp_streams: [1] }) } };
    w.value.recipes.push(recipe);
  }
  suite.tasks.push({ id: nextInnerId(suite.tasks, 'task'), name: protocol.toUpperCase(), protocol, recipe: recipe?.id ?? null,
    ip_versions: [...inner.config.ip_versions], directions: ['upload', 'download'], duration_secs: null, repeats: 1, thresholds: null, ping_options: protocol === 'ping' ? JSON.parse(JSON.stringify(inner.config.ping_options)) : null });
}
function newRecipe(task: InnerTask): void {
  const previous = w.value.recipes.find(r => r.id === task.recipe);
  const recipe: InnerRecipe = { id: nextInnerId(w.value.recipes, 'recipe'), name: '新流量配置', protocol: task.protocol, parameters: previous ? JSON.parse(JSON.stringify(previous.parameters)) : emptyInnerParameterOptions() };
  w.value.recipes.push(recipe); task.recipe = recipe.id;
}
const assigned = (group: string, name?: string) => {
  const binding = w.value.bindings.find(b => b.group === group && b.suite === currentSuite.value?.id);
  return !!binding && (!name ? !binding.links.length : !binding.links.length || binding.links.includes(name));
};
function assign(group: string, name: string | null, event: Event): void {
  if (!currentSuite.value) return;
  const target = w.value.groups.find(g => g.id === group);
  if (target?.all_links) target.links = members(target);
  setInnerAssigned(w.value, group, currentSuite.value.id, name, (event.target as HTMLInputElement).checked);
}
const members = (group: InnerGroup) => innerGroupLinks(inner.config, group);
function member(group: InnerGroup, name: string, event: Event): void {
  if (group.all_links) { group.links = members(group); group.all_links = false; }
  if ((event.target as HTMLInputElement).checked) group.links.push(name);
  else { group.links = group.links.filter(n => n !== name); w.value.bindings = w.value.bindings.filter(b => { if (b.group !== (group as { id?: string }).id || !b.links.length) return true; b.links = b.links.filter(n => n !== name); return b.links.length > 0; }); }
}
function toggle<T>(values: T[], value: T, event: Event): T[] { return (event.target as HTMLInputElement).checked ? [...values, value] : values.filter(v => v !== value); }
const recipeConfigs = new WeakMap<InnerRecipe, InnerConfig>();
function options(recipe: InnerRecipe): InnerConfig {
  let config = recipeConfigs.get(recipe);
  if (!config) { config = { ...defaultInnerConfig(), protocols: [recipe.protocol], parameter_options: recipe.parameters }; recipeConfigs.set(recipe, config); }
  config.parameter_options = recipe.parameters;
  return config;
}
</script>
<template>
  <section aria-label="内环计划工作台">
    <div v-if="!w" class="panel"><p>当前模式：全网口统一参数。</p><button :disabled="disabled" @click="activate">启用套件计划</button></div>
    <template v-else>
      <UiTabs v-model="tab" label="内环计划编辑区域" panel-prefix="inner-workspace" :tabs="[{id:'ports',label:'网口'},{id:'content',label:'测试内容'},{id:'limits',label:'门限与默认值'}]" />
      <fieldset :disabled="disabled" class="workspace-fields">
        <div v-if="tab === 'ports'" id="inner-workspace-ports" role="tabpanel" aria-labelledby="inner-workspace-tab-ports">
          <div class="toolbar"><label class="field">分配到套件<select v-model="suiteId"><option v-for="s in w.suites" :key="s.id" :value="s.id">{{ s.name }}</option></select></label><button @click="addGroup">新增链路集合</button><button @click="emit('review')">复核并执行</button></div>
          <p class="hint">执行范围：已分配且参与的网口。整组包含集合新增网口。</p>
          <section v-for="group in w.groups" :key="group.id" class="panel">
            <div class="toolbar"><input v-model="group.name" aria-label="链路集合名称"><label class="check"><input v-model="group.all_links" type="checkbox">全部网口（含新增）</label><label class="check"><input type="checkbox" :checked="assigned(group.id)" :disabled="!currentSuite" @change="assign(group.id, null, $event)">整组分配</label><button class="ghost small danger" @click="removeGroup(group.id)">删除集合</button></div>
            <div class="table-wrap"><table class="data"><thead><tr><th>加入集合</th><th>本轮分配</th><th>电脑 / 网口</th><th>电脑 → CPE</th></tr></thead><tbody><tr v-for="link in inner.config.links" :key="link.name"><td><input type="checkbox" :aria-label="`${link.name} 加入 ${group.name}`" :checked="members(group).includes(link.name)" @change="member(group, link.name, $event)"></td><td><input type="checkbox" :aria-label="`${link.name} 分配 ${currentSuite?.name ?? ''}`" :disabled="!members(group).includes(link.name) || !currentSuite" :checked="members(group).includes(link.name) && assigned(group.id, link.name)" @change="assign(group.id, link.name, $event)"></td><td>{{ link.host === 'master' ? '主控本机' : link.host }} / {{ link.name }}</td><td>{{ link.local_ip }} → {{ link.gateway }}</td></tr></tbody></table></div>
          </section>
        </div>
        <div v-else-if="tab === 'content'" id="inner-workspace-content" role="tabpanel" aria-labelledby="inner-workspace-tab-content">
          <div class="toolbar"><label class="field">套件<select v-model="suiteId"><option v-for="s in w.suites" :key="s.id" :value="s.id">{{ s.name }}</option></select></label><button @click="addSuite">新增套件</button></div>
          <template v-if="currentSuite"><div class="toolbar"><input v-model="currentSuite.name" aria-label="套件名称"><button class="ghost danger small" @click="removeSuite(currentSuite.id)">删除套件</button><button v-for="protocol in (['tcp','udp','ping'] as const)" :key="protocol" @click="addTask(protocol)">添加 {{ protocol.toUpperCase() }} 任务</button></div>
          <section v-for="(task, index) in currentSuite.tasks" :key="task.id" class="panel">
            <div class="toolbar"><strong>{{ index + 1 }} · {{ task.protocol.toUpperCase() }}</strong><input v-model="task.name" aria-label="任务名称"><button class="ghost small" :disabled="index === 0" @click="currentSuite.tasks.splice(index - 1, 0, currentSuite.tasks.splice(index, 1)[0])">上移</button><button class="ghost danger small" @click="currentSuite.tasks.splice(index, 1)">删除任务</button></div>
            <div class="toolbar"><label v-for="v in INNER_IP_VERSIONS" :key="v" class="check"><input type="checkbox" :checked="task.ip_versions.includes(v)" @change="task.ip_versions = toggle(task.ip_versions, v, $event)">IPv{{ v }}</label><label v-for="d in INNER_DIRECTIONS" :key="d" class="check"><input type="checkbox" :checked="task.directions.includes(d)" @change="task.directions = toggle(task.directions, d, $event)">{{ DIRECTION_LABEL[d] }}</label><label v-if="task.protocol !== 'ping'" class="field">任务时长（秒，留空继承）<input v-model.number="task.duration_secs" type="number" min="1" max="86400"></label><label class="field">单项重复次数<input v-model.number="task.repeats" type="number" min="1" max="10" placeholder="1"></label></div>
            <template v-if="task.protocol !== 'ping'"><div class="toolbar"><label class="field">共享流量配置<select v-model="task.recipe"><option :value="null">未选择</option><option v-for="r in w.recipes.filter(r => r.protocol === task.protocol)" :key="r.id" :value="r.id">{{ r.name }}</option></select></label><button class="ghost small" @click="newRecipe(task)">复制为新配置</button></div>
              <div v-for="recipe in w.recipes.filter(r => r.id === task.recipe)" :key="recipe.id" class="recipe-grid"><label class="field">配置名称<input v-model="recipe.name"></label><p class="hint">共享：{{ w.suites.flatMap(s => s.tasks.filter(t => t.recipe === recipe.id).map(t => `${s.name} / ${t.name}`)).join('、') }}</p>
                <template v-if="recipe.protocol === 'tcp'"><label class="field">TCP 流数<InnerParameterInput :config="options(recipe)" field="tcp_streams" axis="tcp_streams" numeric /></label><label class="field">TCP 窗口 -w<InnerParameterInput :config="options(recipe)" field="tcp_window" axis="tcp_windows" :numeric="false" /></label></template>
                <template v-else><label class="field">UDP 流数<InnerParameterInput :config="options(recipe)" field="udp_streams" axis="udp_streams" numeric /></label><label class="field">UDP 每流速率 Mbps<InnerParameterInput :config="options(recipe)" field="udp_mbps" axis="udp_rates_mbps" numeric /></label><label class="field">UDP 窗口 -w<InnerParameterInput :config="options(recipe)" field="udp_window" axis="udp_windows" :numeric="false" /></label><label class="field">UDP 报文长度<InnerParameterInput :config="options(recipe)" field="udp_length" axis="udp_lengths" :numeric="false" /></label></template>
              </div>
              <label v-if="task.protocol === 'udp'" class="field">UDP 丢包诊断门槛 %<input v-model.number="task.max_udp_loss_pct" type="number" min="0" max="100" step="any" placeholder="留空继承"></label>
              <details><summary>任务门限（优先于网口门限）</summary><button v-if="!task.thresholds" class="ghost small" @click="task.thresholds = emptyInnerThresholds()">设置任务门限</button><div v-else class="recipe-grid"><label v-for="(label,key) in {upload_min_mbps:'上行 RX Mbps',download_min_mbps:'下行 RX Mbps',bidir_total_min_mbps:'双向 RX 合计 Mbps',tool_upload_min_mbps:'工具上行 Mbps',tool_download_min_mbps:'工具下行 Mbps',tool_bidir_total_min_mbps:'工具双向合计 Mbps'}" :key="key" class="field">{{ label }}<input v-model.number="task.thresholds[key]" type="number" min="0" placeholder="留空继承"></label><button class="ghost small" @click="task.thresholds = null">继承网口门限</button></div></details>
            </template>
            <template v-else><button v-if="!task.ping_options" class="ghost small" @click="task.ping_options = JSON.parse(JSON.stringify(inner.config.ping_options))">设置任务 Ping 参数</button><template v-else><InnerPingOptions :options="task.ping_options" /><button class="ghost small" @click="task.ping_options = null">继承默认 Ping 参数</button></template></template>
          </section></template>
          <details><summary>未引用的流量配置</summary><div v-for="r in w.recipes.filter(r => !w.suites.some(s => s.tasks.some(t => t.recipe === r.id)))" :key="r.id" class="toolbar">{{ r.name }}<button class="ghost small" @click="w.recipes = w.recipes.filter(i => i.id !== r.id)">删除配置</button></div></details>
        </div>
        <div v-else id="inner-workspace-limits" role="tabpanel" aria-labelledby="inner-workspace-tab-limits"><InnerDefaults /></div>
      </fieldset>
    </template>
  </section>
</template>
<style scoped>
.workspace-fields { border:0; padding:0; margin:0; min-width:0; }
.recipe-grid { display:grid; grid-template-columns:repeat(2,minmax(0,1fr)); gap:12px; }
@media(max-width:650px) { .recipe-grid { grid-template-columns:1fr; } }
</style>
