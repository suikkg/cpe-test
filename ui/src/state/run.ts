import { computed, reactive } from 'vue';
import { api, errorMessage, NetworkError, UnauthorizedError } from '../api/client';
import type { ProgressOut, RunStatus, UnitStatus } from '../api/dto';
import { mergeUnits, progressView } from '../domain/progress';
import { buildRunRequest, plan, previewIsCurrent } from './plan';
import { session } from './session';

/**
 * 运行资源：起跑/停止 + 进度轮询。
 *
 * **轮询归 state 模块所有，不归视图。** 视图挂载/卸载不启停它——一轮测试跑
 * 11.5 小时，用户当然会在这期间切到别的页去看网卡或监控，切走就断轮询等于
 * 回来时进度是空的。
 */

/**
 * 日志的定长上限。**导出**是为了让界面照实说「只保留最近这么多行」——
 * 被淘汰的行不能继续被当成"可检索"（§11.6）。
 */
export const LOG_MAX_LINES = 4000;

function emptyRun(): RunStatus {
  return {
    run_id: '',
    plan_hash: '',
    started_at: '',
    total_units: 0,
    current: null,
    done: [],
    counts: { pass: 0, fail: 0, measured: 0, not_evaluated: 0, setup_error: 0, skip: 0 },
    eta_secs: null,
    aborted_at_unit: null,
    report: '',
    finished: false,
  };
}

/** 有副作用的命令发出去之后，我们**确知**了什么。 */
export type CommandPhase =
  /** 没在发 */
  | 'idle'
  /** 已发出，还没拿到应答 */
  | 'sending'
  /** 服务端受理了 */
  | 'accepted'
  /** 连应答都没拿到：它可能已经在对面执行了，只能去读状态，不能再发一遍 */
  | 'unknown';

export const run = reactive({
  running: false,
  /**
   * 这个标签页**读到过**运行状态没有。
   *
   * 没读到之前一律显示「待同步」，不能显示「空闲」：那两件事在屏幕上长得一样，
   * 而它们的下一步相反——真空闲可以开跑，没读到时开跑就是往一轮已经在跑的
   * 测试上再叠一轮。
   */
  synced: false,
  /** 最近一次成功读到运行状态的时刻（epoch ms）；没读到过就是 null。 */
  lastSyncAt: null as number | null,
  /** 最近一拍轮询失败的原因；成功一次就清空。断线期间**不清数据**，只标旧。 */
  refreshError: '',
  /** 日志游标：服务端回的 `from` 就是下一拍该用的值 */
  logCursor: 0,
  /** 单元游标：与日志游标**分开**，两者推进速度差三个数量级 */
  unitCursor: 0,
  lines: [] as string[],
  status: emptyRun() as RunStatus,
  /** 攒起来的完整单元列表（服务端只回增量） */
  units: [] as UnitStatus[],
  report: '',
  starting: false,
  startError: '',
  /** 「开始」这条命令的确知程度，见 `CommandPhase`。 */
  startPhase: 'idle' as CommandPhase,
  /** 「停止」这条命令的确知程度。受理 ≠ 已经停下来，收尾还要跑一会儿。 */
  stopPhase: 'idle' as CommandPhase,
  stopError: '',
  reportError: '',
  polling: false,
});

export const view = computed(() =>
  progressView(run.status, run.running, run.units.length, new Date()),
);

export function reset(): void {
  stopPolling();
  run.running = false;
  run.synced = false;
  run.lastSyncAt = null;
  run.refreshError = '';
  run.logCursor = 0;
  run.unitCursor = 0;
  run.lines = [];
  run.status = emptyRun();
  run.units = [];
  run.report = '';
  run.starting = false;
  run.startError = '';
  run.startPhase = 'idle';
  run.stopPhase = 'idle';
  run.stopError = '';
  run.reportError = '';
}

let timer: ReturnType<typeof setTimeout> | undefined;

/**
 * **setTimeout 链，不是 setInterval。**
 *
 * 旧页用的是 `setInterval(poll, 1000)`：机器一忙请求就会叠着发，而这台机器
 * 此刻正在灌线速。「响应落地后再排下一次」保证任何时刻最多一个在飞的请求。
 * （`lint-arch.mjs` 全局禁 setInterval，就是为了不让这条退回去。）
 */
function schedule(): void {
  timer = setTimeout(() => void pollTick(), 1000);
}

/**
 * 状态读取**只有这一个出口**，轮询和一次性快照共用它。
 *
 * 两个出口的代价是两条各自计时的请求链：初始快照那条不会因为轮询在跑就让路，
 * 于是「一秒一拍」在页面刚打开的几秒里变成两拍——而这台机器此刻正在灌线速。
 * `inFlight` 保证任何时刻最多一个在飞的请求。
 */
let inFlight = false;

async function tick(): Promise<boolean> {
  if (inFlight) return false;
  inFlight = true;
  let ok = false;
  try {
    // 带上手上这份 `run_id`：单元游标只在一轮之内有意义。服务端对不上就
    // 从 0 重发，否则「新一轮已经跑过陈旧游标」时这个标签页会永久缺开头
    // 那一段单元——而计数格走的是全量 counts，两块显示会对不上。
    const out = await api.get<ProgressOut>(
      `/api/progress?from=${run.logCursor}&units_from=${run.unitCursor}` +
        `&run_id=${encodeURIComponent(run.status.run_id)}`,
    );
    applyProgress(out);
    run.synced = true;
    run.lastSyncAt = Date.now();
    run.refreshError = '';
    ok = true;
  } catch (error) {
    if (error instanceof UnauthorizedError) {
      // 口令失效是**全局终态**：继续按秒轮询只会刷出一串 401，而屏幕上
      // 什么都不会变。停掉这条链，让全局提示接手。
      session.phase = 'unauthorized';
      stopPolling();
      run.refreshError = '';
    } else {
      // 断线**不清数据**：已完成的单元、日志和游标全部原样留着，只把
      // 「这份是旧的」说出来，下一拍按原间隔自己重试。
      run.refreshError = errorMessage(error);
    }
  } finally {
    inFlight = false;
  }
  return ok;
}

async function pollTick(): Promise<void> {
  if (!run.polling) return;
  await tick();
  if (run.polling) schedule();
}

/**
 * 读一次运行状态，**不**开轮询链。
 *
 * 两个用处：页面打开时先认一次「服务器上是不是已经有一轮在跑」（在此之前
 * 界面显示「待同步」而不是「空闲」），以及开始/停止拿不到应答时去核对结果。
 * 读到正在跑就顺手把轮询接上。
 */
export async function syncStatus(): Promise<void> {
  const ok = await tick();
  if (ok && run.running) startPolling();
}

/** 把一拍回包并进本地状态。导出是为了能被单测直接喂数据。 */
export function applyProgress(out: ProgressOut): void {
  // **换了一轮就把攒的单元丢掉。** 服务端一侧已经会把越界游标自愈成 0
  // 并全量重传，但那还不够：`mergeUnits` 按 `seq` 去重，上一轮的 1..N 号
  // 单元会把新一轮同号的挤掉——列表里显示的是上一轮的判定，而计数格显示的
  // 是新一轮的，两块都"看起来正常"，只是说的不是同一轮。
  //
  // 用 `run_id` 判而不是用 `running`：一轮结束到下一轮开始之间 `running`
  // 会翻两次，而 `run_id` 只在真的换了一轮时变。
  const runChanged = out.run.run_id !== run.status.run_id;
  // 「已受理」之后第一次读到「没在跑、也没有 run_id」——这一轮在产生任何计划
  // 之前就自己结束了（例如执行端复核计划哈希不通过而退出）。POST 成功过，所以
  // 界面这边一切正常；不说出来的话，屏幕就是从「启动中」悄悄回到「空闲」，
  // 没有任何一处提到刚才那次点击。不去解析日志内容下结论，只把人指过去。
  if (run.startPhase === 'accepted' && !out.running && !out.run.run_id) {
    run.startPhase = 'idle';
    run.startError =
      '主控受理了「开始」，但这一轮在产生任何测试单元之前就结束了。' +
      '到「进度」页展开运行日志看最后几行——那里写着执行端为什么退出。';
  }
  // 停下来这件事由**运行状态**说了算，不由那次 HTTP 200 说了算：一旦真的
  // 不在跑了，「已请求停止 / 停止结果未确认」这两句话就没有意义了，留着只会
  // 让结束之后的界面上还挂着一个悬而未决的动作。
  if (!out.running && run.stopPhase !== 'sending') run.stopPhase = 'idle';
  run.running = out.running;
  run.logCursor = out.from;
  run.unitCursor = out.units_from;
  run.status = out.run;
  run.units = mergeUnits(runChanged ? [] : run.units, out.run.done);
  if (out.report) run.report = out.report;
  if (out.lines.length) {
    // 定长数组：旧页用 `textContent +=`，长测试后期是二次方开销。
    const merged = run.lines.concat(out.lines);
    run.lines = merged.length > LOG_MAX_LINES ? merged.slice(-LOG_MAX_LINES) : merged;
  }
}

export function startPolling(): void {
  if (run.polling) return;
  run.polling = true;
  void pollTick();
}

export function stopPolling(): void {
  run.polling = false;
  if (timer !== undefined) {
    clearTimeout(timer);
    timer = undefined;
  }
}

/**
 * 开跑。**必须带上复核页拿到的 `plan_hash`**。
 *
 * 那是「界面上确认的东西 == 实际跑的东西」唯一的强制点：执行端会自己再推导
 * 一次计划，对不上这个哈希就拒绝开跑。不带它等于把这道闸拆了。
 */
export async function start(): Promise<void> {
  // 重复提交在这台机器上不是「多发一个请求」：每一轮都会真的去灌包。
  // 按钮的 disabled 挡不住连点与回车重复触发，闸门放在这里。
  if (run.starting) return;
  run.starting = true;
  run.startError = '';
  run.startPhase = 'sending';
  try {
    const hash = plan.preview?.plan_hash;
    if (!hash) {
      throw new Error('先点「预览」——没有复核过的计划哈希，执行端会拒绝开跑');
    }
    if (!previewIsCurrent()) {
      throw new Error('计划或运行参数在预览后有改动，请重新预览再开始');
    }
    await api.post('/api/run', { ...buildRunRequest(), plan_hash: hash });
    // 起跑成功就把上一轮的残留清掉，但**保留轮询**。
    run.units = [];
    run.lines = [];
    run.logCursor = 0;
    run.unitCursor = 0;
    run.report = '';
    run.running = true;
    run.startPhase = 'accepted';
    startPolling();
  } catch (error) {
    if (error instanceof NetworkError) {
      // **不能再发一遍。** 没拿到应答不等于没执行；这一轮可能已经在对面
      // 起跑了，再发一次就是两轮同时灌包。只能去读状态，由服务器说了算。
      run.startPhase = 'unknown';
      run.startError = '';
      void syncStatus();
    } else {
      // 服务端答了「失败」：这一轮确定没起来，计划和表单原样留着，就地重试。
      run.startPhase = 'idle';
      run.startError = errorMessage(error);
    }
  } finally {
    run.starting = false;
  }
}

/**
 * 请求停止。**HTTP 200 只说明「已受理」，不说明这一轮已经结束。**
 *
 * 收尾要等当前单元的工具退出、报告落盘，可能还有几十秒。把受理直接当成
 * 结束，屏幕会先说「本轮已结束」，随后轮询又把它翻回「运行中」。
 */
export async function stop(): Promise<void> {
  if (run.stopPhase === 'sending') return;
  run.stopPhase = 'sending';
  run.stopError = '';
  try {
    await api.post('/api/stop', {});
    run.stopPhase = 'accepted';
  } catch (error) {
    if (error instanceof NetworkError) {
      // 同样不重发：停止请求可能已经到了。去读状态，别凭超时认定已停。
      run.stopPhase = 'unknown';
      void syncStatus();
    } else {
      run.stopPhase = 'idle';
      run.stopError = errorMessage(error);
    }
  }
}

export async function openReport(): Promise<void> {
  run.reportError = '';
  try {
    await api.post('/api/open-report', {});
  } catch (error) {
    // 单独一格：报告打不开和「这一轮没起来」是两件事，挤在
    // `startError` 里会让开始按钮旁边冒出一句和它无关的话。
    run.reportError = errorMessage(error);
  }
}
