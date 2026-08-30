import type { RunStatus, UnitStatus } from '../api/dto';

/**
 * 进度的**展示模型组装**。纯函数。
 *
 * 注意这里没有任何解析器。v5.0 原本计划让前端去解析 `[i/total]` 和
 * 「==> 单元结果:」两种日志行，并用 Rust 测试把日志格式钉死当协议；
 * v6.0 改成 Rust 直接吐结构化 `RunStatus`（ADR-2），于是这一层从「解析器」
 * 变成了「把结构化数据摆成人能看的样子」——薄得多，而且日志文案彻底自由。
 */

/** 判定的展示分组：验收现场先看的是「哪些不行」。 */
export type VerdictTone = 'pass' | 'fail' | 'measured' | 'inconclusive' | 'skip';

export function verdictTone(verdict: string): VerdictTone {
  switch (verdict) {
    case 'PASS':
      return 'pass';
    case 'RATE_FAIL':
      return 'fail';
    case 'MEASURED':
      return 'measured';
    case 'SKIP':
      return 'skip';
    // NOT_EVALUATED 与 SETUP_ERROR 都是「这一轮下不了结论」，不是「设备不行」。
    // 把它们和 RATE_FAIL 混成一类，会让人拿着一份「环境有问题」的报告去
    // 找硬件的麻烦——这套判定一直在防的就是这个方向。
    default:
      return 'inconclusive';
  }
}

export interface ProgressView {
  /** 已完成 / 总数 */
  done: number;
  total: number;
  /** 0–1；总数未知时为 0 */
  ratio: number;
  /** 正在跑第几个（1-based），没有则 null */
  currentSeq: number | null;
  currentTitle: string;
  /** 剩余时间的人话；未知为空串 */
  eta: string;
  /** 已耗时占比之外，还要给一个「跑完大概什么时候」——小时级测试这个更有用 */
  finishHint: string;
  running: boolean;
  finished: boolean;
  aborted: boolean;
}

/** 秒数 → 「2 小时 13 分」这类人话。 */
export function humanDuration(secs: number | null | undefined): string {
  if (secs === null || secs === undefined || !Number.isFinite(secs) || secs < 0) return '';
  const s = Math.round(secs);
  if (s < 60) return `${s} 秒`;
  const minutes = Math.floor(s / 60);
  if (minutes < 60) return `${minutes} 分`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest === 0 ? `${hours} 小时` : `${hours} 小时 ${rest} 分`;
}

/** 「跑完大概几点」。11.5 小时的测试里，这个比「还剩 X 分钟」有用得多。 */
export function finishTimeHint(etaSecs: number | null | undefined, now = new Date()): string {
  if (etaSecs === null || etaSecs === undefined || !Number.isFinite(etaSecs) || etaSecs <= 0) {
    return '';
  }
  const at = new Date(now.getTime() + etaSecs * 1000);
  const hh = String(at.getHours()).padStart(2, '0');
  const mm = String(at.getMinutes()).padStart(2, '0');
  const sameDay = at.getDate() === now.getDate();
  return sameDay ? `预计 ${hh}:${mm} 跑完` : `预计次日 ${hh}:${mm} 跑完`;
}

export function progressView(
  run: RunStatus,
  running: boolean,
  doneCount: number,
  now = new Date(),
): ProgressView {
  const total = run.total_units;
  return {
    done: doneCount,
    total,
    ratio: total > 0 ? Math.min(1, doneCount / total) : 0,
    currentSeq: run.current?.seq ?? null,
    currentTitle: run.current?.title ?? '',
    eta: humanDuration(run.eta_secs),
    finishHint: finishTimeHint(run.eta_secs, now),
    running,
    finished: run.finished,
    aborted: run.aborted_at_unit !== null,
  };
}

/**
 * 失败清单：只留需要处置的单元。
 *
 * 一轮 210 单元的测试，验收现场要的是「哪几条不行、该找谁」。把 210 行全列
 * 出来等于没有这张清单。
 */
export function failureList(units: UnitStatus[]): UnitStatus[] {
  return units.filter((unit) => {
    const tone = verdictTone(unit.verdict);
    return tone === 'fail' || tone === 'inconclusive';
  });
}

/** 按链路组归拢失败——同一条链路上连着失败，指向的是链路而不是某个单元。 */
export function failuresByLinkGroup(
  units: UnitStatus[],
): Array<{ group: string; units: UnitStatus[] }> {
  const order: string[] = [];
  const buckets = new Map<string, UnitStatus[]>();
  for (const unit of failureList(units)) {
    const key = unit.link_group || '(未分组)';
    if (!buckets.has(key)) {
      buckets.set(key, []);
      order.push(key);
    }
    buckets.get(key)!.push(unit);
  }
  return order.map((group) => ({ group, units: buckets.get(group)! }));
}

/**
 * 合并增量：`units_from` 游标只回新完成的单元，前端自己攒完整列表。
 *
 * 幂等——同一批增量重复送达（比如请求重发）不会产生重复行。
 */
export function mergeUnits(existing: UnitStatus[], incoming: UnitStatus[]): UnitStatus[] {
  if (incoming.length === 0) return existing;
  const seen = new Set(existing.map((unit) => unit.seq));
  const merged = [...existing];
  for (const unit of incoming) {
    if (seen.has(unit.seq)) continue;
    seen.add(unit.seq);
    merged.push(unit);
  }
  merged.sort((a, b) => a.seq - b.seq);
  return merged;
}
