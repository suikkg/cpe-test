<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { inner } from '../../state/inner';
import { DIRECTION_LABEL, FLOW_LABEL, PROTOCOL_LABEL, SOURCE_LABEL } from '../../domain/inner';
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

const rate = (value: number | null | undefined) => (value == null ? '—' : value.toFixed(2));
/**
 * 丢包只有拿到 receiver 汇总行才印数字；拿不到印「未知」而不是 0%。
 *
 * 三个字段**要么都有要么都没有**，与后端 `report::loss` 同一条规则。以前这里
 * 只看百分比、计数缺了就省略括号，于是同一条腿在控制台显示 `0.412%`、下载的
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
    <div v-if="inner.status.units.length" class="bar filters">
      <label>判定<select v-model="verdictFilter">
        <option value="">全部</option>
        <option v-for="v in ['PASS', 'RATE_FAIL', 'MEASURED', 'NOT_EVALUATED', 'SETUP_ERROR']" :key="v" :value="v">{{ v }}</option>
      </select></label>
      <label>网口<select v-model="linkFilter">
        <option value="">全部</option>
        <option v-for="name in links" :key="name" :value="name">{{ name }}</option>
      </select></label>
      <span class="muted">{{ filtered.length }} / {{ inner.status.units.length }} 个单元</span>
      <template v-if="pages > 1">
        <button :disabled="page <= 1" @click="page--">上一页</button>
        <span aria-live="polite">第 {{ page }} / {{ pages }} 页</span>
        <button :disabled="page >= pages" @click="page++">下一页</button>
      </template>
    </div>

    <div v-if="visible.length" class="result-table">
      <table>
        <thead><tr>
          <th scope="col">#</th><th scope="col">电脑 / 网口</th><th scope="col">协议</th><th scope="col">方向</th>
          <th scope="col">轮次</th><th scope="col">腿 / 接收端</th><th scope="col" class="num">速率 Mbps</th>
          <th scope="col">来源</th><th scope="col" class="num">门限</th><th scope="col" class="num">网卡 RX</th>
          <th scope="col" class="num">工具接收</th><th scope="col" class="num">UDP 丢包</th><th scope="col">判定</th>
        </tr></thead>
        <tbody>
          <template v-for="unit in visible" :key="unit.index">
            <!--
              一条腿都没有的单元也要有一行：后端为这种情况专门留了
              UnitDirectionResultMissing 判定，而它同时计入 completed 与
              not_evaluated。只按 legs 渲染的话，计数和看得见的行数对不上，
              页面上没有一处解释差在哪。与 report.rs 的渲染保持一致。
            -->
            <tr v-if="!unit.legs.length" :key="`${unit.index}-empty`">
              <td class="num">{{ unit.index }}</td>
              <td>{{ unit.host }}<br><strong>{{ unit.link }}</strong></td>
              <td>{{ PROTOCOL_LABEL[unit.protocol] }}</td>
              <td>{{ DIRECTION_LABEL[unit.direction] }}</td>
              <td class="num">{{ unit.repeat }}</td>
              <td colspan="7" class="muted">本单元没有产生任何一条腿的结果</td>
              <td>{{ unit.verdict }}</td>
            </tr>
            <tr v-for="(leg, i) in unit.legs" :key="`${unit.index}-${leg.flow}`">
              <td v-if="i === 0" :rowspan="unit.legs.length" class="num">{{ unit.index }}</td>
              <td v-if="i === 0" :rowspan="unit.legs.length">{{ unit.host }}<br><strong>{{ unit.link }}</strong></td>
              <td v-if="i === 0" :rowspan="unit.legs.length">{{ PROTOCOL_LABEL[unit.protocol] }}</td>
              <td v-if="i === 0" :rowspan="unit.legs.length">{{ DIRECTION_LABEL[unit.direction] }}</td>
              <td v-if="i === 0" :rowspan="unit.legs.length" class="num">{{ unit.repeat }}</td>
              <td>{{ FLOW_LABEL[leg.flow] }}<br><span class="muted">{{ leg.receiver_host }} {{ leg.receiver }}</span></td>
              <td class="num"><strong>{{ rate(leg.mbps) }}</strong></td>
              <td>{{ SOURCE_LABEL[leg.source] ?? leg.source }}</td>
              <td class="num">{{ rate(leg.target_mbps) }}</td>
              <td class="num">{{ rate(leg.nic_rx_mbps) }}</td>
              <td class="num">{{ rate(leg.tool_receiver_mbps) }}</td>
              <td class="num">{{ loss(unit, leg) }}</td>
              <td v-if="i === 0" :rowspan="unit.legs.length">
                <strong>{{ unit.verdict }}</strong>
                <details>
                  <summary>详情</summary>
                  <p>{{ unit.reason }}: {{ unit.detail }}</p>
                  <p v-if="unit.total_mbps != null">
                    双向合计 {{ rate(unit.total_mbps) }} Mbps，门限 {{ rate(unit.total_target_mbps) }}；
                    共同有效重叠 {{ unit.overlap_secs != null ? unit.overlap_secs.toFixed(2) + 's' : '无' }}。
                  </p>
                  <p v-for="(item, k) in unit.diagnostics" :key="`u${k}`">{{ item }}</p>
                  <template v-for="one in unit.legs" :key="`d-${one.flow}`">
                    <p><strong>{{ FLOW_LABEL[one.flow] }} 腿 · 端口 {{ one.port }} · {{ one.verdict }}</strong></p>
                    <p>{{ one.reason }}: {{ one.detail }}</p>
                    <p v-if="one.fallback_reason">已改用{{ SOURCE_LABEL[one.source] }}，原因：{{ one.fallback_reason }}</p>
                    <p>
                      网卡口径独立留存：RX {{ rate(one.nic_rx_mbps) }} Mbps，验收 {{ one.nic_verdict }}（{{ one.nic_reason }}），
                      门限 {{ rate(one.nic_target_mbps) }}。工具口径：接收 {{ rate(one.tool_receiver_mbps) }} Mbps（{{ one.tool_receiver_note }}），
                      发送 {{ rate(one.tool_sender_mbps) }} Mbps 仅诊断。
                    </p>
                    <p>采样覆盖 {{ (one.coverage * 100).toFixed(1) }}%；有效时长 {{ one.effective_secs.toFixed(2) }}s / 配置 {{ one.required_secs }}s。</p>
                    <p v-for="(item, k) in one.diagnostics" :key="`l${k}`">{{ item }}</p>
                  </template>
                </details>
              </td>
            </tr>
          </template>
        </tbody>
      </table>
    </div>
    <p v-else-if="inner.status.units.length" class="muted">当前筛选没有匹配的单元；已完成的结果都还在，清空筛选即可看到。</p>
  </div>
</template>

<style scoped>
.filters { flex-wrap: wrap; gap: 10px; align-items: center; }
.filters label { display: inline-flex; align-items: center; gap: 6px; font-size: 13px; }
.result-table { overflow-x: auto; max-height: 32rem; overflow-y: auto; }
.result-table table { width: 100%; border-collapse: collapse; }
.result-table th, .result-table td { text-align: left; vertical-align: top; padding: 10px; border-bottom: 1px solid var(--line); }
.result-table thead th { position: sticky; top: 0; z-index: 1; background: var(--head); white-space: nowrap; }
.result-table td.num, .result-table th.num { text-align: right; white-space: nowrap; }
.result-table details { max-width: 420px; }
.muted { color: var(--muted); font-size: 12px; }
</style>
