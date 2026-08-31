<script setup lang="ts">
import { computed } from 'vue';
import {
  formatNumberList,
  formatTokenList,
  globalsAreEmpty,
  parseNumberList,
  parseTokenList,
} from '../../domain/globals';
import { plan } from '../../state/plan';
import { session } from '../../state/session';

/**
 * 「全局默认档位」：套件里没写死参数时用哪一组。
 *
 * 对应 `RunRequest` 的顶层字段（`tcp_windows` / `udp_bandwidths` / …）。
 *
 * # 为什么常驻可见，且不折叠
 *
 * UDP 的 `-b` 是这套工具里被改得最多的一个数。它一度被放进一个默认收起的
 * 折叠块里，于是「全局带宽在哪」成了一个要先找到才能回答的问题——最常用的旋钮
 * 藏得最深，比旧页面还退了一步。折叠留给下面那张按网卡的大表，它才是偶尔才用的。
 *
 * # 为什么留空、却仍然看得见生效值
 *
 * 后端的口径是 `non_empty(请求, 配置)`：只有请求里非空才覆盖配置。所以这里
 * **一律不预填**——把配置里那份可能有意不成叉积的 `udp_profiles` 回填进三个框
 * 再发回去，就会被展成叉积，单元数悄悄变多。
 *
 * 但「留空」不该等于「不知道现在跑的是什么」。所以当前生效的那份值走
 * **placeholder**（灰字）显示：看得见、改得动、不会被误发回去。这一份来自
 * `/api/bootstrap`，即主控 `config.json` 里的值。
 *
 * 档位串一律逗号分隔，逐档各跑一轮（这是后端的展开语义，不是这里的约定）。
 */

/** 逗号串 ↔ 数组的双向绑定。写在一处，几个框共用。 */
function tokens(key: 'tcp_windows' | 'udp_bandwidths' | 'udp_lengths' | 'udp_windows') {
  return computed({
    get: () => formatTokenList(plan.globals[key]),
    set: (raw: string) => {
      plan.globals[key] = parseTokenList(raw);
    },
  });
}

function numbers(key: 'tcp_streams' | 'ping_payload_sizes') {
  return computed({
    get: () => formatNumberList(plan.globals[key]),
    set: (raw: string) => {
      plan.globals[key] = parseNumberList(raw);
    },
  });
}

/**
 * 标量档位（并发流 / ping 次数）的绑定。
 *
 * **不能用 `v-model.number`**：0 在模型里是「不覆盖」，而数字输入框会把它渲染成
 * 一个真真切切的 `0`，placeholder 永远不出现——于是「不覆盖」在屏幕上长得像
 * 「我把并发流设成了 0」。空串才是「不覆盖」该有的样子。
 */
function scalar(key: 'udp_streams' | 'ping_count') {
  return computed({
    get: () => (plan.globals[key] > 0 ? String(plan.globals[key]) : ''),
    set: (raw: string) => {
      const value = Number(raw.trim());
      plan.globals[key] = Number.isFinite(value) && value > 0 ? Math.trunc(value) : 0;
    },
  });
}

const udpStreams = scalar('udp_streams');
const pingCount = scalar('ping_count');
const tcpWindows = tokens('tcp_windows');
const tcpStreams = numbers('tcp_streams');
const udpBandwidths = tokens('udp_bandwidths');
const udpLengths = tokens('udp_lengths');
const udpWindows = tokens('udp_windows');
const pingSizes = numbers('ping_payload_sizes');

/**
 * 主控 `config.json` 里当前的值，用作 placeholder。
 *
 * 只用来**显示**，永远不会被当成用户的输入发回去。注意 `tcp_streams` /
 * `udp_streams` / `ping_*` 这几项在服务端是从 `cfg.tests` 反推的
 * （`webui/api.rs::bootstrap_out` 的「反推段」），所以它们是**指示性**的：
 * 配置里那几项本来就分散在各个 test 上，没有唯一答案。
 */
const configured = computed(() => session.bootstrap);

function hint(values: readonly string[] | readonly number[] | undefined, fallback: string): string {
  if (!values || values.length === 0) return fallback;
  return `沿用配置：${values.join(', ')}`;
}

/**
 * UDP 的 `-l` / `-w` 的提示，**取决于 `-b` 填没填**。
 *
 * 后端不是逐字段回落的：只要 `udp_bandwidths` 非空，它就用这三个框
 * **整体重建** `cfg.iperf.udp_profiles`（`ui_request_base_config`）——这时 `-l`
 * 留空就是真的不下发 `-l`，配置文件里那个值根本不参与。
 *
 * 所以 `-b` 有值时还显示「沿用配置：64」是**错的**：它会让人以为不填就会跑
 * `-l 64`，而实际一个 `-l` 都不下发。这正是「界面说的和实际跑的不一样」那一类，
 * 比没有提示更糟。
 */
function udpProfileHint(values: readonly string[] | undefined, flag: string): string {
  if (plan.globals.udp_bandwidths.length > 0) return `留空 = 不下发 ${flag}`;
  return hint(values, `留空 = 不下发 ${flag}`);
}

function scalarHint(value: number | undefined, fallback: string): string {
  return value && value > 0 ? `沿用配置：${value}` : fallback;
}

const untouched = computed(() => globalsAreEmpty(plan.globals));
</script>

<template>
  <section class="block">
    <div class="head">
      <strong>全局默认档位</strong>
      <small class="muted">
        {{
          untouched
            ? '全部沿用主控 config.json（灰字就是它当前的值）'
            : '填了值的格子按填的跑；空格子沿用主控 config.json（灰字）'
        }}
      </small>
    </div>

    <div class="grid">
      <label class="wide">
        <span>UDP 单流带宽 <code>-b</code></span>
        <input
          v-model="udpBandwidths"
          type="text"
          :placeholder="hint(configured?.udp_bandwidths, '如 2500m, 1000m')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>UDP 报文长度 <code>-l</code></span>
        <input
          v-model="udpLengths"
          type="text"
          :placeholder="udpProfileHint(configured?.udp_lengths, '-l')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>UDP socket buffer <code>-w</code></span>
        <input
          v-model="udpWindows"
          type="text"
          :placeholder="udpProfileHint(configured?.udp_windows, '-w')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>UDP 并发流</span>
        <input
          v-model="udpStreams"
          type="text"
          inputmode="numeric"
          :placeholder="scalarHint(configured?.udp_streams, '留空 = 不覆盖')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>TCP socket buffer <code>-w</code></span>
        <input
          v-model="tcpWindows"
          type="text"
          :placeholder="hint(configured?.tcp_windows, '如 4m, 64k')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>TCP 并发流 <code>-P</code></span>
        <input
          v-model="tcpStreams"
          type="text"
          :placeholder="hint(configured?.tcp_streams, '如 1, 10')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>Ping 次数</span>
        <input
          v-model="pingCount"
          type="text"
          inputmode="numeric"
          :placeholder="scalarHint(configured?.ping_count, '留空 = 不覆盖')"
          autocomplete="off"
        />
      </label>
      <label>
        <span>Ping 包长（字节）</span>
        <input
          v-model="pingSizes"
          type="text"
          :placeholder="hint(configured?.ping_payload_sizes, '如 32, 1400')"
          autocomplete="off"
        />
      </label>
    </div>

    <p class="muted hint">
      套件里选中的配置优先；这里填的是「套件没选配置时用哪一组」。填多个用逗号分隔，逐档各跑一轮。
      <br />
      <strong>UDP 那四格是一整组</strong>：只要 <code>-b</code> 填了，
      <code>-l</code> / <code>-w</code> 留空就是<strong>真的不下发</strong>这两个参数（用 iperf3 默认），
      配置文件里的值不再参与。TCP 两格则各自独立回落。
    </p>
  </section>
</template>

<style scoped>
.block {
  margin: 0 0 14px;
  padding: 11px 12px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--surface);
}
.head { display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; }
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(190px, 1fr));
  gap: 10px;
  margin: 10px 0 0;
}
/* `-b` 是被改得最多的一个数，给它两格宽，档位串写长了也不用横向滚。 */
.wide { grid-column: span 2; }
@media (max-width: 700px) { .wide { grid-column: auto; } }
label { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
label span { font-size: 11.5px; color: var(--muted); }
input {
  padding: 7px 9px;
  border: 1px solid var(--line);
  border-radius: 4px;
  background: var(--canvas);
  color: var(--ink);
  font: inherit;
  font-size: 13px;
  min-width: 0;
}
input::placeholder { color: var(--muted); opacity: 1; }
input:focus-visible { outline: 2px solid var(--focus); outline-offset: 1px; }
.hint { margin: 9px 0 0; font-size: 12px; }
code { font-family: var(--fm); }
.muted { color: var(--muted); }
</style>
