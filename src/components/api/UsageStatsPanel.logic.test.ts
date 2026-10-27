import { describe, expect, it } from 'vitest';

import {
  fmtClock,
  localDateStr,
  maxOpt,
  maxOptList,
  mergeBuckets,
  mergeCounters,
  mergeDay,
  mergeKeyTokens,
  mergeModelLatency,
} from './UsageStatsPanel';
import type {
  UsageCounterView,
  UsageDayView,
  UsageKeyTokenView,
  UsageModelLatencyView,
} from '../../types';

/** 最小计数器（模型/账号/Key 维度通用） */
const ctr = (name: string, requests: number, ok = requests, errors = 0, credits = 0) =>
  ({ name, requests, ok, errors, credits }) as UsageCounterView;

/** 最小 Key token 用量 */
const kt = (name: string, prompt_tokens: number, completion_tokens: number) =>
  ({ name, prompt_tokens, completion_tokens }) as UsageKeyTokenView;

/** 最小模型延迟分位（未传字段缺省 undefined） */
const lat = (
  model: string,
  samples: number,
  extra: Partial<UsageModelLatencyView> = {},
) => ({ model, samples, ...extra }) as UsageModelLatencyView;

/** 最小日记录（必填字段取默认零值，按需覆盖可选字段） */
const day = (date: string, over: Partial<UsageDayView> = {}) =>
  ({
    date,
    total_requests: 0,
    ok: 0,
    errors: 0,
    stream_requests: 0,
    prompt_tokens: 0,
    completion_tokens: 0,
    credits: 0,
    original_credits: 0,
    avg_duration_ms: 0,
    models: [],
    accounts: [],
    keys: [],
    key_tokens: [],
    ...over,
  }) as UsageDayView;

describe('mergeCounters（同名计数器合并）', () => {
  it('同名累加各维计数，不同名保留，保持首次出现顺序', () => {
    const r = mergeCounters(
      [ctr('gpt', 10, 8, 2, 3), ctr('k2', 5)],
      [ctr('gpt', 4, 3, 1, 1), ctr('k1', 7)],
    );
    expect(r).toEqual([
      { name: 'gpt', requests: 14, ok: 11, errors: 3, credits: 4 },
      { name: 'k2', requests: 5, ok: 5, errors: 0, credits: 0 },
      { name: 'k1', requests: 7, ok: 7, errors: 0, credits: 0 },
    ]);
  });

  it('不修改入参（返回新数组新元素）', () => {
    const a = [ctr('gpt', 10)];
    const b = [ctr('gpt', 4)];
    const r = mergeCounters(a, b);
    expect(a[0]).toEqual({ name: 'gpt', requests: 10, ok: 10, errors: 0, credits: 0 });
    expect(b[0]).toEqual({ name: 'gpt', requests: 4, ok: 4, errors: 0, credits: 0 });
    expect(r[0]).not.toBe(a[0]);
  });
});

describe('mergeKeyTokens（按 Key 的 token 合并）', () => {
  it('同名累加输入/输出 token，不同名保留', () => {
    const r = mergeKeyTokens([kt('k1', 100, 50)], [kt('k1', 10, 5), kt('k2', 7, 3)]);
    expect(r).toEqual([
      { name: 'k1', prompt_tokens: 110, completion_tokens: 55 },
      { name: 'k2', prompt_tokens: 7, completion_tokens: 3 },
    ]);
  });
});

describe('mergeDay（两条日记录合并）', () => {
  it('基础字段相加；avg_duration_ms 按请求数加权并四舍五入', () => {
    const a = day('2026-01-01', {
      total_requests: 3,
      ok: 2,
      errors: 1,
      avg_duration_ms: 1000,
      prompt_tokens: 30,
      completion_tokens: 20,
      credits: 4,
      original_credits: 5,
      stream_requests: 2,
    });
    const b = day('2026-01-01', {
      total_requests: 1,
      ok: 1,
      avg_duration_ms: 500,
    });
    const r = mergeDay(a, b);
    expect(r.total_requests).toBe(4);
    expect(r.ok).toBe(3);
    expect(r.errors).toBe(1);
    expect(r.stream_requests).toBe(2);
    expect(r.prompt_tokens).toBe(30);
    expect(r.completion_tokens).toBe(20);
    expect(r.credits).toBe(4);
    expect(r.original_credits).toBe(5);
    // (1000*3 + 500*1) / 4 = 875
    expect(r.avg_duration_ms).toBe(875);
    expect(r.date).toBe('2026-01-01');
  });

  it('avg_ttfb_ms 按样本数加权；ttfb_samples 缺省按 0 计', () => {
    const a = day('2026-01-01', { avg_ttfb_ms: 300, ttfb_samples: 2 });
    const b = day('2026-01-01', { avg_ttfb_ms: 900, ttfb_samples: 2 });
    expect(mergeDay(a, b).avg_ttfb_ms).toBe(600); // (300*2+900*2)/4
    expect(mergeDay(a, b).ttfb_samples).toBe(4);

    const c = day('2026-01-01', { avg_ttfb_ms: 300, ttfb_samples: 2 });
    const d = day('2026-01-01'); // 无样本
    expect(mergeDay(c, d).avg_ttfb_ms).toBe(300);
    expect(mergeDay(c, d).ttfb_samples).toBe(2);
  });

  it('两侧均无 TTFT 样本时 avg_ttfb_ms 为 undefined', () => {
    const r = mergeDay(day('2026-01-01'), day('2026-01-01'));
    expect(r.avg_ttfb_ms).toBeUndefined();
    expect(r.ttfb_samples).toBe(0);
  });

  it('P95/最大值取保守最大值（跨 undefined）', () => {
    const a = day('2026-01-01', { p95_duration_ms: 1200, max_duration_ms: 2000, p95_ttfb_ms: 400 });
    const b = day('2026-01-01', { p95_duration_ms: 800, p95_ttfb_ms: 700 });
    const r = mergeDay(a, b);
    expect(r.p95_duration_ms).toBe(1200);
    expect(r.max_duration_ms).toBe(2000);
    expect(r.p95_ttfb_ms).toBe(700);
  });

  it('嵌套计数器/token 按同名合并', () => {
    const a = day('2026-01-01', {
      models: [ctr('gpt', 10)],
      accounts: [ctr('acc', 6)],
      keys: [ctr('key', 8)],
      key_tokens: [kt('key', 100, 50)],
    });
    const b = day('2026-01-01', {
      models: [ctr('gpt', 4), ctr('k2', 1)],
      keys: [ctr('key', 2)],
      key_tokens: [kt('key', 10, 5)],
    });
    const r = mergeDay(a, b);
    expect(r.models).toEqual([
      { name: 'gpt', requests: 14, ok: 14, errors: 0, credits: 0 },
      { name: 'k2', requests: 1, ok: 1, errors: 0, credits: 0 },
    ]);
    expect(r.accounts).toEqual([{ name: 'acc', requests: 6, ok: 6, errors: 0, credits: 0 }]);
    expect(r.keys).toEqual([{ name: 'key', requests: 10, ok: 10, errors: 0, credits: 0 }]);
    expect(r.key_tokens).toEqual([{ name: 'key', prompt_tokens: 110, completion_tokens: 55 }]);
  });
});

describe('maxOpt / maxOptList（可选数值取保守最大）', () => {
  it('maxOpt：双 undefined 为 undefined；单侧缺省取另一侧；两侧取大', () => {
    expect(maxOpt(undefined, undefined)).toBeUndefined();
    expect(maxOpt(3, undefined)).toBe(3);
    expect(maxOpt(undefined, 5)).toBe(5);
    expect(maxOpt(2, 9)).toBe(9);
  });

  it('maxOptList：过滤 undefined 后取最大；全缺省或空表为 undefined', () => {
    expect(maxOptList([undefined, 7, undefined, 4])).toBe(7);
    expect(maxOptList([undefined, undefined])).toBeUndefined();
    expect(maxOptList([])).toBeUndefined();
  });
});

describe('mergeModelLatency（按模型的延迟分位合并）', () => {
  it('同名累加样本数、各分位取保守最大；异名保留', () => {
    const a = [lat('gpt', 10, { p95_duration_ms: 1000, avg_ttfb_ms: 200 })];
    const b = [lat('gpt', 5, { p95_duration_ms: 1500, avg_ttfb_ms: 150 }), lat('k2', 3)];
    const r = mergeModelLatency(a, b);
    expect(r).toEqual([
      { model: 'gpt', samples: 15, p95_duration_ms: 1500, avg_ttfb_ms: 200 },
      { model: 'k2', samples: 3 },
    ]);
  });

  it('按样本数降序排列', () => {
    const r = mergeModelLatency(
      [lat('a', 1)],
      [lat('c', 9), lat('b', 5)],
    );
    expect(r.map((m) => m.model)).toEqual(['c', 'b', 'a']);
  });

  it('任一侧缺省按空数组处理', () => {
    expect(mergeModelLatency([lat('gpt', 1)], undefined)).toEqual([
      { model: 'gpt', samples: 1 },
    ]);
    expect(mergeModelLatency(undefined, [lat('gpt', 2)])).toEqual([
      { model: 'gpt', samples: 2 },
    ]);
  });
});

describe('mergeBuckets（多桶按日合并）', () => {
  it('同日跨桶相加，异日保留，按日期升序', () => {
    const trae = [day('2026-01-02', { total_requests: 3, ok: 3 })];
    const buddy = [day('2026-01-01', { total_requests: 5, errors: 1, ok: 4 })];
    const qoder = [day('2026-01-02', { total_requests: 2, ok: 2 })];
    const r = mergeBuckets(trae, buddy, qoder);
    expect(r.map((d) => d.date)).toEqual(['2026-01-01', '2026-01-02']);
    expect(r[0].total_requests).toBe(5);
    expect(r[1].total_requests).toBe(5); // 3 + 2
    expect(r[1].errors).toBe(0);
  });

  it('单桶透传且不改写入参元素（浅拷贝）', () => {
    const src = [day('2026-01-01', { total_requests: 1 })];
    const r = mergeBuckets(src);
    expect(r).toHaveLength(1);
    expect(r[0]).not.toBe(src[0]);
    expect(r[0].total_requests).toBe(1);
  });
});

describe('localDateStr / fmtClock（custom 会话明细时间格式化）', () => {
  it('localDateStr 按本地时区输出 YYYY-MM-DD，单数位补零', () => {
    expect(localDateStr(new Date(2026, 0, 5))).toBe('2026-01-05');
    expect(localDateStr(new Date(2026, 10, 23))).toBe('2026-11-23');
  });

  it('fmtClock 将秒级时间戳格式化为 HH:MM:SS，单数位补零', () => {
    const ts = new Date(2026, 0, 5, 9, 8, 7).getTime() / 1000;
    expect(fmtClock(ts)).toBe('09:08:07');
    const ts2 = new Date(2026, 0, 5, 23, 59, 59).getTime() / 1000;
    expect(fmtClock(ts2)).toBe('23:59:59');
  });
});
