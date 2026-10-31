import { describe, expect, it } from 'vitest';
import { buildModelTrend, REST_SERIES_KEY } from './modelTrend';

/** 便捷构造：模型 →（日期 → 数值）字面量 → Map 结构（与 agg.modelByDate 同形） */
function toModelByDate(spec: Record<string, Record<string, number>>): Map<string, Map<string, number>> {
  const out = new Map<string, Map<string, number>>();
  for (const [model, days] of Object.entries(spec)) {
    out.set(model, new Map(Object.entries(days)));
  }
  return out;
}

const DATES = ['2026-10-05', '2026-10-06', '2026-10-07'];

describe('buildModelTrend（issue #160 各模型消耗趋势）', () => {
  it('模型超过 8 个：区间累计降序取 Top8，其余聚合为「其他」', () => {
    const spec: Record<string, Record<string, number>> = {};
    // 9 个模型，累计值 = 编号（model-9 最大、model-1 最小）
    for (let i = 1; i <= 9; i++) spec[`model-${i}`] = { '2026-10-05': i, '2026-10-06': i };
    const { series, rows } = buildModelTrend(toModelByDate(spec), DATES.slice(0, 2), '7d');

    expect(series).toHaveLength(9);
    expect(series.map((s) => s.name)).toEqual([
      'model-9', 'model-8', 'model-7', 'model-6', 'model-5', 'model-4', 'model-3', 'model-2', '其他',
    ]);
    expect(series[8]).toEqual({ key: REST_SERIES_KEY, name: '其他' });
    // Top8 行值 = 各自当日值；「其他」行值 = 未入 Top8 的 model-1
    expect(rows[0]).toEqual({ label: '10/5', m0: 9, m1: 8, m2: 7, m3: 6, m4: 5, m5: 4, m6: 3, m7: 2, rest: 1 });
    expect(rows[1]).toEqual({ label: '10/6', m0: 9, m1: 8, m2: 7, m3: 6, m4: 5, m5: 4, m6: 3, m7: 2, rest: 1 });
  });

  it('模型不超过 8 个：不生成「其他」系列', () => {
    const { series, rows } = buildModelTrend(
      toModelByDate({ a: { '2026-10-05': 3 }, b: { '2026-10-05': 1 } }),
      DATES.slice(0, 1),
      '7d',
    );
    expect(series.map((s) => s.key)).toEqual(['m0', 'm1']);
    expect(rows[0]).toEqual({ label: '10/5', m0: 3, m1: 1 });
    expect(rows[0]).not.toHaveProperty(REST_SERIES_KEY);
  });

  it('缺失日补 0', () => {
    const { rows } = buildModelTrend(toModelByDate({ a: { '2026-10-05': 7 } }), DATES, '7d');
    expect(rows).toEqual([
      { label: '10/5', m0: 7 },
      { label: '10/6', m0: 0 },
      { label: '10/7', m0: 0 },
    ]);
  });

  it('空数据：series 为空，rows 仍按 dateList 生成 label 行', () => {
    const { series, rows } = buildModelTrend(new Map(), DATES.slice(0, 2), '7d');
    expect(series).toEqual([]);
    expect(rows).toEqual([{ label: '10/5' }, { label: '10/6' }]);
  });

  it('label 格式：year 三段去前导零，短区间 M/D', () => {
    expect(buildModelTrend(new Map(), ['2026-01-09'], 'year').rows[0].label).toBe('2026/1/9');
    expect(buildModelTrend(new Map(), ['2026-01-09'], '7d').rows[0].label).toBe('1/9');
  });

  it('回归防护：含 `.` 的模型名不出现在行键（recharts dataKey 的 lodash path 断链）', () => {
    const { series, rows } = buildModelTrend(
      toModelByDate({
        'gpt-4.1': { '2026-10-05': 5 },
        'claude-3.5-sonnet': { '2026-10-05': 4 },
        'gemini-2.5-pro': { '2026-10-05': 1 },
      }),
      DATES.slice(0, 1),
      '7d',
    );
    // 行键只允许 label / 内部键 m0..m7 / rest——含 `.` 的原始模型名绝不能作键
    for (const key of Object.keys(rows[0])) {
      expect(key === 'label' || /^m\d+$/.test(key) || key === REST_SERIES_KEY).toBe(true);
    }
    // 数值仍按模型名正确映射
    expect(series.map((s) => s.name)).toEqual(['gpt-4.1', 'claude-3.5-sonnet', 'gemini-2.5-pro']);
    expect(rows[0].m0).toBe(5);
    expect(rows[0].m1).toBe(4);
  });

  it('跨日累计参与排序；平局稳定、构建确定性', () => {
    const spec = toModelByDate({
      a: { '2026-10-05': 2, '2026-10-06': 3 }, // 累计 5，跨日求和
      b: { '2026-10-05': 5 }, // 累计 5，与 a 平局
      c: { '2026-10-05': 1 },
    });
    const r1 = buildModelTrend(spec, DATES, '7d');
    const r2 = buildModelTrend(spec, DATES, '7d');
    // 同输入两次构建结果一致（排序稳定，无随机性）
    expect(r1).toEqual(r2);
    expect(r1.series.map((s) => s.name)).toEqual(['a', 'b', 'c']);
    // 缺失日（10-07）补 0
    expect(r1.rows[2]).toEqual({ label: '10/7', m0: 0, m1: 0, m2: 0 });
  });
});
