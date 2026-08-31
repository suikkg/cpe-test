import { computed, reactive } from 'vue';
import { api } from '../api/client';
import type { ProgressOut, RunStatus, UnitStatus } from '../api/dto';
import { mergeUnits, progressView } from '../domain/progress';
import { buildRunRequest, plan, previewIsCurrent } from './plan';

/**
 * 运行资源：起跑/停止 + 进度轮询。
 *
 * **轮询归 state 模块所有，不归视图。** 视图挂载/卸载不启停它——一轮测试跑
 * 11.5 小时，用户当然会在这期间切到别的页去看网卡或监控，切走就断轮询等于
 * 回来时进度是空的。
 */

const LOG_MAX_LINES = 4000;

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

export const run = reactive({
  running: false,
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
  polling: false,
});

export const view = computed(() =>
  progressView(run.status, run.running, run.units.length, new Date()),
);

export function reset(): void {
  stopPolling();
  run.running = false;
  run.logCursor = 0;
  run.unitCursor = 0;
  run.lines = [];
  run.status = emptyRun();
  run.units = [];
  run.report = '';
  run.starting = false;
  run.startError = '';
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
  timer = setTimeout(() => void tick(), 1000);
}

async function tick(): Promise<void> {
  if (!run.polling) return;
  try {
    // 带上手上这份 `run_id`：单元游标只在一轮之内有意义。服务端对不上就
    // 从 0 重发，否则「新一轮已经跑过陈旧游标」时这个标签页会永久缺开头
    // 那一段单元——而计数格走的是全量 counts，两块显示会对不上。
    const out = await api.get<ProgressOut>(
      `/api/progress?from=${run.logCursor}&units_from=${run.unitCursor}` +
        `&run_id=${encodeURIComponent(run.status.run_id)}`,
    );
    applyProgress(out);
  } catch {
    // 轮询失败不弹错：下一拍自己会重试，断线自愈是这套轮询天然的性质。
    // 真出问题时用户按「开始」会拿到明确报错。
  }
  if (run.polling) schedule();
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
  void tick();
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
  run.starting = true;
  run.startError = '';
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
    startPolling();
  } catch (error) {
    run.startError = error instanceof Error ? error.message : String(error);
  } finally {
    run.starting = false;
  }
}

export async function stop(): Promise<void> {
  try {
    await api.post('/api/stop', {});
  } catch (error) {
    run.startError = error instanceof Error ? error.message : String(error);
  }
}

export async function openReport(): Promise<void> {
  try {
    await api.post('/api/open-report', {});
  } catch (error) {
    run.startError = error instanceof Error ? error.message : String(error);
  }
}
