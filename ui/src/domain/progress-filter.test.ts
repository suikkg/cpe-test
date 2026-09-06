import { describe, expect, it } from 'vitest';
import type { UnitStatus } from '../api/dto';
import { filterByVerdict, unitSearchFields, verdictBucket } from './progress';
import { filterByQuery } from './search';

function unit(seq: number, verdict: string, patch: Partial<UnitStatus> = {}): UnitStatus {
  return {
    seq,
    title: `IPERF V4 TCP | 主控 en0 -> 辅测 en1`,
    verdict,
    reason_code: '',
    reason_detail: '',
    skipped: false,
    secs: 10,
    link_group: 'SGMII ↔ WLAN',
    ...patch,
  } as UnitStatus;
}

describe('判定分桶', () => {
  it('桶名与服务端 counts 的字段一一对应', () => {
    expect(verdictBucket('PASS')).toBe('pass');
    expect(verdictBucket('RATE_FAIL')).toBe('fail');
    expect(verdictBucket('MEASURED')).toBe('measured');
    expect(verdictBucket('NOT_EVALUATED')).toBe('not_evaluated');
    expect(verdictBucket('SETUP_ERROR')).toBe('setup_error');
    expect(verdictBucket('SKIP')).toBe('skip');
  });

  it('**未知判定归到「未评估」，绝不归成功**', () => {
    // §3.1：未知值不能自动归入成功。归错方向的代价是把没判过的当成过了。
    expect(verdictBucket('WHAT_IS_THIS')).toBe('not_evaluated');
    expect(verdictBucket('')).toBe('not_evaluated');
    expect(verdictBucket('pass ')).toBe('pass');
  });
});

describe('筛选与搜索', () => {
  const units = [unit(1, 'PASS'), unit(2, 'RATE_FAIL', { reason_code: 'RX_BELOW_TARGET' }), unit(3, 'PASS'), unit(4, 'SKIP')];

  it('按桶筛选，保持 seq 顺序', () => {
    expect(filterByVerdict(units, 'pass').map((u) => u.seq)).toEqual([1, 3]);
    expect(filterByVerdict(units, 'all').map((u) => u.seq)).toEqual([1, 2, 3, 4]);
    expect(filterByVerdict(units, 'measured')).toEqual([]);
  });

  it('搜得到序号、标题、链路、判定和原因码', () => {
    const hit = (q: string) => filterByQuery(units, q, unitSearchFields).map((u) => u.seq);
    expect(hit('#2')).toEqual([2]);
    expect(hit('rx_below_target')).toEqual([2]);
    expect(hit('wlan')).toEqual([1, 2, 3, 4]);
    expect(hit('rate_fail')).toEqual([2]);
  });

  it('筛选与搜索取交集，且都不改动原数组', () => {
    const filtered = filterByVerdict(units, 'pass');
    expect(filterByQuery(filtered, '#3', unitSearchFields).map((u) => u.seq)).toEqual([3]);
    expect(units.map((u) => u.seq)).toEqual([1, 2, 3, 4]);
  });
});
