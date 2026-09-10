import { describe, expect, it } from 'vitest';
// 后端是权威，这份语料放在后端目录下；前端只是提前给出可读错误。
// 对应的 Rust 断言是 `inner::tests::the_shared_validation_corpus_matches_the_rust_side`。
import corpus from '../../../src/inner/validation_corpus.json';
import {
  innerIfaceWord,
  innerSafeWord,
  innerSizeToken,
  isAdbProgram,
  MAX_AGENT_ADDRESS_BYTES,
  MAX_AGENT_TOKEN_BYTES,
} from './inner';

type Case = {
  field: string;
  value?: string;
  repeat?: [string, number];
  repeat_suffix?: [string, number];
  valid: boolean;
  why: string;
};

const validators: Record<string, (value: string) => boolean> = {
  size_token: innerSizeToken,
  safe_word: innerSafeWord,
  iface_word: innerIfaceWord,
  adb_program: isAdbProgram,
};

function build(one: Case): string {
  let value = one.value ?? '';
  for (const spec of [one.repeat, one.repeat_suffix]) {
    if (spec) value += spec[0].repeat(spec[1]);
  }
  return value;
}

// 这几条白名单在 Rust 和 TypeScript 里各有一份手写实现，历史上漂移过五次，每次都
// 表现为「界面保存成功、后端才拒绝」。两侧各自的单测都挡不住它——各自都能过，只是
// 内容不一样。所以用例本身由两边共读：改规则要先改语料，然后两边一起变红。
describe('前后端共享的校验语料', () => {
  const cases = corpus.cases as Case[];

  it('覆盖全部四条白名单，且没有前端未接线的字段', () => {
    const fields = [...new Set(cases.map((one) => one.field))].sort();
    expect(fields).toEqual(['adb_program', 'iface_word', 'safe_word', 'size_token']);
    expect(cases.length).toBeGreaterThanOrEqual(50);
  });

  it('两个没有独立谓词的上限与后端常量一致', () => {
    expect(MAX_AGENT_ADDRESS_BYTES).toBe(corpus.limits.agent_address_bytes);
    expect(MAX_AGENT_TOKEN_BYTES).toBe(corpus.limits.agent_token_bytes);
  });

  it.each(cases.map((one) => [one.field, build(one), one.valid, one.why] as const))(
    '%s(%j) → %s：%s',
    (field, value, valid) => {
      expect(validators[field](value)).toBe(valid);
    },
  );
});
