<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { inner } from '../../state/inner';
import { DIRECTION_LABEL, FLOW_LABEL, PROTOCOL_LABEL, SOURCE_LABEL, innerParameterLabel } from '../../domain/inner';
import { innerRecordedCounts, innerVerdictLabel, innerUnitExplanation, innerUsesTotal, innerRateText, innerTargetText, innerTotalTargetText, innerFlowPath } from '../../domain/inner-report';
import type { InnerLeg, InnerUnit } from '../../domain/inner';

const PAGE = 20;
const verdictFilter = ref('');
const linkFilter = ref('');
const page = ref(1);

const links = computed(() => [...new Set(inner.status.units.map((unit) => unit.link))]);
const filtered = computed(() => inner.status.units.filter((unit) =>
  (!verdictFilter.value || unit.verdict === verdictFilter.value)
  && (!linkFilter.value || unit.link === linkFilter.value)));
const pages = computed(() => Math.max(1, Math.ceil(filtered.value.length / PAGE)));
const visible = computed(() => filtered.value.slice((page.value - 1) * PAGE, page.value * PAGE));
// 结果一直在长；筛选变化或结果变少时把页码拉回有效范围，别停在空白页上。
watch([filtered, pages], () => { if (page.value > pages.value) page.value = pages.value; });

const rate = innerRateText;
const counts = computed(() => innerRecordedCounts(inner.status.units));
const usesTotal = (unit: InnerUnit) => innerUsesTotal(inner.config, unit);
/**
 * 丢包只有拿到 receiver 汇总行才印数字；拿不到印「未知」而不是 0%。
 *
 * 三个字段**要么都有要么都没有**，与后端 `report::loss` 同一条规则。以前这里
 * 只看百分比、计数缺了就省略括号，于是同一个方向在控制台显示 `0.412%`、下载的
 * 报告显示「未知」——两个界面对同一次测量各说各话，而「未知」这个措辞本来就是
 * 为了防止有人把「没测到」读成 0%。
 */
function loss(unit: InnerUnit, leg: InnerLeg): string {
  if (unit.protocol !== 'udp') return '—';
  if (leg.udp_loss_pct == null || leg.udp_lost_datagrams == null || leg.udp_total_datagrams == null) return '未知';
  return `${leg.udp_loss_pct.toFixed(3)}% (${leg.udp_lost_datagrams}/${leg.udp_total_datagrams})`;
}
</script>

<template>
  <div>
    <dl v-if="inner.status.units.length" class="result-counts" aria-label="本轮结果概览">
      <div v-for="(value, label) in { '已记录单元': counts.total, '本轮达标': counts.pass, '未达标': counts.fail, '仅测量': counts.measured, '无法评价': counts.unknown, '执行失败': counts.error, '复用历史': counts.resumed }" :key="label"><dt>{{ label }}</dt><dd>{{ value }}</dd></div>
    </dl>
    <p v-if="inner.status.units.length" class="muted">每行一个单元。历史复用不计入本轮达标。</p>
    <div v-if="inner.status.units.length" class="bar filters">
      <label>判定<select v-model="verdictFilter">
        <option value="">全部</option>
        <option v-for="v in ['PASS', 'RATE_FAIL', 'MEASURED', 'NOT_EVALUATED', 'SETUP_ERROR']" :key="v" :value="v">{{ innerVerdictLabel(v) }}（{{ v }}）</option>
      </select></label>
      <label>网口<select v-model="linkFilter"><option value="">全部</option><option v-for="name in links" :key="name" :value="name">{{ name }}</option></select></label>
      <span class="muted">{{ filtered.length }} / {{ inner.status.units.length }} 个单元</span>
      <template v-if="pages > 1"><button :disabled="page <= 1" @click="page--">上一页</button><span aria-live="polite">第 {{ page }} / {{ pages }} 页</span><button :disabled="page >= pages" @click="page++">下一页</button></template>
    </div>
    <div v-if="visible.length" class="result-table">
      <table>
        <thead><tr><th scope="col">#</th><th scope="col">电脑 / 网口</th><th scope="col">协议 / 参数</th><th scope="col">方向 / 轮次</th><th scope="col">接收速率 / 门限</th><th scope="col">判定</th></tr></thead>
        <tbody>
          <tr v-for="unit in visible" :key="unit.index">
            <td class="num">{{ unit.index }}</td>
            <td>{{ unit.host === 'master' ? '主控本机' : unit.host }}<br><strong>{{ unit.link }}</strong></td>
            <td>{{ PROTOCOL_LABEL[unit.protocol] }} / IPv{{ unit.ip_version ?? 4 }}<small v-if="unit.parameters" class="muted">{{ innerParameterLabel(unit.parameters, unit.protocol) }}</small></td>
            <td>{{ DIRECTION_LABEL[unit.direction] }}<small>第 {{ unit.repeat }} 轮</small></td>
            <td class="acceptance-cell">
              <p v-if="unit.resumed" class="muted">本轮未重新测试</p>
              <template v-else>
                <template v-if="usesTotal(unit)"><strong>双向合计 {{ rate(unit.total_mbps) }} Mbps</strong><p>合计门限 {{ innerTotalTargetText(inner.config, unit) }}</p><small>按两端 RX 合计判定一次</small></template>
                <strong v-else-if="unit.direction === 'bidir'">上下行分别判定</strong>
                <p v-for="leg in unit.legs" :key="leg.flow"><strong>{{ FLOW_LABEL[leg.flow] }} {{ rate(leg.mbps) }} Mbps</strong> / {{ usesTotal(unit) ? '仅测量' : leg.source === 'none' ? '未形成有效验收' : '门限 ' + innerTargetText(leg.target_mbps) }}<small>{{ SOURCE_LABEL[leg.source] }} · {{ innerFlowPath(leg.flow) }} · 接收端 {{ leg.receiver_host }} {{ leg.receiver }}</small></p>
                <p v-if="!unit.legs.length" class="muted">无测量结果</p>
              </template>
            </td>
            <td class="verdict-cell">
              <strong class="verdict-badge" :data-verdict="unit.resumed ? 'RESUME' : unit.verdict">{{ unit.resumed ? '复用历史 PASS' : innerVerdictLabel(unit.verdict) + '（' + unit.verdict + '）' }}</strong>
              <p>{{ innerUnitExplanation(unit) }}</p>
              <details>
                <summary>测量依据与诊断</summary>
                <p v-if="unit.direction === 'bidir'">共同有效重叠 {{ unit.overlap_secs != null ? unit.overlap_secs.toFixed(2) + 's' : '未取得' }}。</p>
                <p v-for="(item, k) in unit.diagnostics" :key="`u${k}`">{{ item }}</p>
                <div v-for="one in unit.legs" :key="one.flow" class="leg-evidence">
                  <h4>{{ FLOW_LABEL[one.flow] }} · {{ innerFlowPath(one.flow) }} · 端口 {{ one.port }}</h4>
                  <p>{{ one.detail }}</p>
                  <p v-if="one.fallback_reason" class="fallback">已改用{{ SOURCE_LABEL[one.source] }}：{{ one.fallback_reason }}</p>
                  <p>网卡 RX {{ rate(one.nic_rx_mbps) }} Mbps，门限 {{ innerTargetText(one.nic_target_mbps) }}；{{ innerVerdictLabel(one.nic_verdict) }}（{{ one.nic_verdict }}）。</p>
                  <p>工具口径：接收 {{ rate(one.tool_receiver_mbps) }} Mbps；{{ one.tool_receiver_note }}。工具发送 {{ rate(one.tool_sender_mbps) }} Mbps（仅诊断）。</p>
                  <p>UDP 丢包 {{ loss(unit, one) }}（仅诊断）。采样覆盖 {{ (one.coverage * 100).toFixed(1) }}%；有效时长 {{ one.effective_secs.toFixed(2) }}s / 配置 {{ one.required_secs }}s。</p>
                  <p v-for="(item, k) in one.diagnostics" :key="k">{{ item }}</p>
                  <small>原因代码：{{ one.reason }}；网卡独立判定：{{ one.nic_reason }}</small>
                </div>
                <p v-if="unit.screenshot">{{ unit.screenshot.error ? `截图失败：${unit.screenshot.error}` : `已保存 ${unit.screenshot.host} 桌面截图，打开报告查看` }}</p>
                <small>单元原因代码：{{ unit.reason }}</small>
              </details>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <p v-else-if="inner.status.units.length" class="muted">无匹配结果，请清空筛选。</p>
  </div>
</template>

<style scoped>
.filters { flex-wrap: wrap; gap: 10px; align-items: center; }
.filters label { display: inline-flex; align-items: center; gap: 6px; font-size: 13px; }
.result-table small { display: block; margin-top: 4px; }
.result-table { overflow-x: auto; max-height: 40rem; overflow-y: auto; }
.result-table table { width: 100%; border-collapse: collapse; }
.result-table th, .result-table td { text-align: left; vertical-align: top; padding: 10px; border-bottom: 1px solid var(--line); }
.result-table thead th { position: sticky; top: 0; z-index: 1; background: var(--head); white-space: nowrap; }
.result-table td.num, .result-table th.num { text-align: right; white-space: nowrap; }
 .acceptance-cell { min-width: 270px; }
.verdict-cell { min-width: 240px; max-width: 420px; }
.result-table p { margin: 6px 0; font-size: 13px; }
.result-table small { color: var(--muted); font-size: 12px; }
.result-counts { display: flex; flex-wrap: wrap; gap: 12px 26px; margin: 12px 0; padding: 12px; background: var(--panel-2); }
.result-counts dt { font-size: 12px; color: var(--muted); }
.result-counts dd { margin: 0; font-size: 22px; font-weight: 650; font-variant-numeric: tabular-nums; }
.verdict-badge { display: inline-block; padding: 3px 8px; border: 1px solid var(--line); border-radius: 4px; font-size: 12px; }
.verdict-badge[data-verdict="PASS"] { color: var(--ok); }
.verdict-badge[data-verdict="RATE_FAIL"], .verdict-badge[data-verdict="SETUP_ERROR"] { color: var(--bad); }
.verdict-badge[data-verdict="NOT_EVALUATED"] { color: var(--warn); }
.verdict-badge[data-verdict="RESUME"] { color: var(--muted); }
.leg-evidence { border-top: 1px solid var(--line); margin-top: 12px; padding-top: 8px; }
.leg-evidence h4 { font-size: 13px; margin: 4px 0; }
.fallback { color: var(--warn); }
.muted { color: var(--muted); font-size: 12px; }
</style>
