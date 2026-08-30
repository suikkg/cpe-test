import { describe, expect, it } from 'vitest';
import type { RunStatus, UnitStatus } from '../api/dto';
import {
  failureList,
  failuresByLinkGroup,
  finishTimeHint,
  humanDuration,
  mergeUnits,
  progressView,
  verdictTone,
} from './progress';

function unit(seq: number, verdict: string, group = 'SGMII ↔ WLAN'): UnitStatus {
  return {
    seq,
    title: `unit ${seq}`,
    verdict,
    reason_code: verdict === 'PASS' ? 'RX_TARGET_MET' : 'RX_BELOW_TARGET',
    reason_detail: '',
    skipped: verdict === 'SKIP',
    secs: 180,
    link_group: group,
  };
}

function run(partial: Partial<RunStatus> = {}): RunStatus {
  return {
    run_id: 'run_x',
    plan_hash: 'hash',
    started_at: '2026-08-30 10:00:00',
    total_units: 10,
    current: null,
    done: [],
    counts: { pass: 0, fail: 0, measured: 0, not_evaluated: 0, setup_error: 0, skip: 0 },
    eta_secs: null,
    aborted_at_unit: null,
    report: '',
    finished: false,
    ...partial,
  };
}

describe('判定分组', () => {
  it('NOT_EVALUATED 与 SETUP_ERROR 不和 RATE_FAIL 混成一类', () => {
    // 前两者是「这一轮下不了结论」，后者是「设备没达标」。混成一类会让人
    // 拿着一份「环境有问题」的报告去找硬件的麻烦——这套判定一直在防的
    // 就是这个方向。
    expect(verdictTone('RATE_FAIL')).toBe('fail');
    expect(verdictTone('NOT_EVALUATED')).toBe('inconclusive');
    expect(verdictTone('SETUP_ERROR')).toBe('inconclusive');
    expect(verdictTone('PASS')).toBe('pass');
    expect(verdictTone('MEASURED')).toBe('measured');
    expect(verdictTone('SKIP')).toBe('skip');
  });

  it('不认识的判定当作「下不了结论」，不当成通过', () => {
    // 服务端将来加了新 verdict 而前端还没跟上时，宁可显示成「要看一眼」，
    // 也不要显示成绿的。
    expect(verdictTone('SOMETHING_NEW')).toBe('inconclusive');
    expect(verdictTone('')).toBe('inconclusive');
  });
});

describe('失败清单', () => {
  const units = [
    unit(1, 'PASS'),
    unit(2, 'RATE_FAIL'),
    unit(3, 'MEASURED'),
    unit(4, 'NOT_EVALUATED'),
    unit(5, 'SKIP'),
    unit(6, 'SETUP_ERROR'),
  ];

  it('只留需要处置的：一轮 210 单元全列出来等于没有这张清单', () => {
    expect(failureList(units).map((u) => u.seq)).toEqual([2, 4, 6]);
  });

  it('按链路组归拢——同一条链路连着失败指向的是链路，不是某个单元', () => {
    const mixed = [
      unit(1, 'RATE_FAIL', 'A ↔ B'),
      unit(2, 'RATE_FAIL', 'C ↔ D'),
      unit(3, 'NOT_EVALUATED', 'A ↔ B'),
      unit(4, 'PASS', 'A ↔ B'),
    ];
    const grouped = failuresByLinkGroup(mixed);
    expect(grouped).toHaveLength(2);
    expect(grouped[0].group).toBe('A ↔ B');
    expect(grouped[0].units.map((u) => u.seq)).toEqual([1, 3]);
    expect(grouped[1].units.map((u) => u.seq)).toEqual([2]);
  });

  it('没有链路组时落进「未分组」而不是空字符串', () => {
    expect(failuresByLinkGroup([unit(1, 'RATE_FAIL', '')])[0].group).toBe('(未分组)');
  });
});

describe('增量合并', () => {
  it('游标增量攒成完整列表', () => {
    let list: UnitStatus[] = [];
    list = mergeUnits(list, [unit(1, 'PASS'), unit(2, 'PASS')]);
    list = mergeUnits(list, [unit(3, 'RATE_FAIL')]);
    expect(list.map((u) => u.seq)).toEqual([1, 2, 3]);
  });

  it('幂等：同一批增量重复送达不产生重复行', () => {
    // 请求重发、或者前端游标推进失败重试时会发生。
    let list = mergeUnits([], [unit(1, 'PASS'), unit(2, 'PASS')]);
    list = mergeUnits(list, [unit(2, 'PASS'), unit(3, 'PASS')]);
    expect(list.map((u) => u.seq)).toEqual([1, 2, 3]);
  });

  it('乱序到达也按 seq 排好', () => {
    const list = mergeUnits([unit(3, 'PASS')], [unit(1, 'PASS')]);
    expect(list.map((u) => u.seq)).toEqual([1, 3]);
  });

  it('空增量不改动原列表引用', () => {
    const before = [unit(1, 'PASS')];
    expect(mergeUnits(before, [])).toBe(before);
  });
});

describe('时长与完成时刻', () => {
  it('说人话', () => {
    expect(humanDuration(45)).toBe('45 秒');
    expect(humanDuration(120)).toBe('2 分');
    expect(humanDuration(3600)).toBe('1 小时');
    expect(humanDuration(7980)).toBe('2 小时 13 分');
  });

  it('未知时长回空串而不是 0', () => {
    expect(humanDuration(null)).toBe('');
    expect(humanDuration(undefined)).toBe('');
    expect(humanDuration(Number.NaN)).toBe('');
    expect(humanDuration(-1)).toBe('');
  });

  it('给出「跑完大概几点」——11.5 小时的测试里这个比「还剩多少」有用', () => {
    const now = new Date('2026-08-30T10:00:00');
    expect(finishTimeHint(3600, now)).toBe('预计 11:00 跑完');
    // 跨天要说清楚，否则「预计 02:30」会被读成今天凌晨。
    expect(finishTimeHint(20 * 3600, now)).toBe('预计次日 06:00 跑完');
    expect(finishTimeHint(0, now)).toBe('');
    expect(finishTimeHint(null, now)).toBe('');
  });
});

describe('进度展示模型', () => {
  it('从 RunStatus 直接组装，不解析任何日志', () => {
    const view = progressView(
      run({
        total_units: 10,
        current: {
          seq: 4,
          title: 'IPERF V4 UDP',
          est_secs: 180,
          started_at: '',
          link_group: 'A ↔ B',
        },
        eta_secs: 1260,
      }),
      true,
      3,
      new Date('2026-08-30T10:00:00'),
    );
    expect(view.done).toBe(3);
    expect(view.total).toBe(10);
    expect(view.ratio).toBeCloseTo(0.3);
    expect(view.currentSeq).toBe(4);
    expect(view.currentTitle).toBe('IPERF V4 UDP');
    expect(view.eta).toBe('21 分');
    expect(view.finishHint).toBe('预计 10:21 跑完');
    expect(view.running).toBe(true);
    expect(view.finished).toBe(false);
    expect(view.aborted).toBe(false);
  });

  it('总数未知时比例不炸', () => {
    expect(progressView(run({ total_units: 0 }), false, 0).ratio).toBe(0);
  });

  it('完成数超过总数时比例封顶到 1', () => {
    // 追加的诊断单元会让已完成数超过计划总数——那时进度条不该溢出。
    expect(progressView(run({ total_units: 3 }), false, 5).ratio).toBe(1);
  });

  it('熔断中止能被看出来', () => {
    const view = progressView(run({ aborted_at_unit: 7 }), false, 7);
    expect(view.aborted).toBe(true);
  });
});
