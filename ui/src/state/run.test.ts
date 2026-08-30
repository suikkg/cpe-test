import { beforeEach, describe, expect, it } from 'vitest';
import type { ProgressOut, RunStatus } from '../api/dto';
import { applyProgress, reset, run } from './run';

/**
 * 进度合并的**换轮语义**。
 *
 * 这里守的是一条真出过的 bug：同一个控制台跑第二轮时，进度页显示的是**上一轮**
 * 的单元列表和失败清单，而计数格显示的是新一轮的。两块都"看起来正常"，只是说的
 * 不是同一轮——比一眼能看出来的错更难发现。
 *
 * 成因是两边凑的：服务端 `/api/run` 被接受后要读配置、扫拓扑、建计划才轮到
 * `run_started`，那几拍回的还是上一轮的全套单元；前端又只按 `seq` 去重，上一轮的
 * 1..N 号会把新一轮同号的挤掉。服务端那一半由
 * `a_second_run_never_serves_the_previous_runs_units` 守着，这里守前端这一半。
 */

function status(runId: string, seqs: number[], total = 3): RunStatus {
  return {
    run_id: runId,
    plan_hash: 'h',
    started_at: '',
    total_units: total,
    current: null,
    done: seqs.map((seq) => ({
      seq,
      title: `${runId}#${seq}`,
      verdict: 'PASS',
      reason_code: '',
      reason_detail: '',
      skipped: false,
      secs: 1,
      link_group: 'SGMII ↔ WLAN',
    })),
    counts: { pass: seqs.length, fail: 0, measured: 0, not_evaluated: 0, setup_error: 0, skip: 0 },
    eta_secs: 0,
    aborted_at_unit: null,
    report: '',
    finished: false,
  };
}

function tick(s: RunStatus, unitsFrom: number): ProgressOut {
  return { running: true, from: 0, lines: [], report: '', units_from: unitsFrom, run: s };
}

describe('applyProgress', () => {
  beforeEach(() => reset());

  it('同一轮之内按游标累加，重复送达不产生重复行', () => {
    applyProgress(tick(status('run_a', [1, 2]), 2));
    applyProgress(tick(status('run_a', [3]), 3));
    // 重发同一批（请求重试）
    applyProgress(tick(status('run_a', [3]), 3));
    expect(run.units.map((u) => u.seq)).toEqual([1, 2, 3]);
    expect(run.unitCursor).toBe(3);
  });

  it('换了一轮就把上一轮攒的单元丢掉，而不是按 seq 去重挤掉新的', () => {
    applyProgress(tick(status('run_old', [1, 2, 3]), 3));
    expect(run.units.map((u) => u.title)).toEqual(['run_old#1', 'run_old#2', 'run_old#3']);

    // 服务端换轮：run_id 变了，越界游标已在服务端自愈成 0 并全量重传。
    applyProgress(tick(status('run_new', [1, 2]), 2));
    expect(run.units.map((u) => u.title)).toEqual(['run_new#1', 'run_new#2']);
    expect(run.status.run_id).toBe('run_new');
  });

  it('「开始」被接受、run_started 还没到的那几拍也算换轮', () => {
    applyProgress(tick(status('run_old', [1, 2, 3]), 3));
    // 服务端 reset() 之后、run_started 之前：run_id 是空串、done 是空的。
    applyProgress(tick(status('', []), 0));
    expect(run.units).toEqual([]);
    expect(run.unitCursor).toBe(0);
    // 新一轮真的开始。
    applyProgress(tick(status('run_new', [1]), 1));
    expect(run.units.map((u) => u.title)).toEqual(['run_new#1']);
  });
});
