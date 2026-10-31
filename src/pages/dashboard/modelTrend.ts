/**
 * 各模型消耗趋势数据构建（issue #160）——零依赖纯函数，供 CreditsTab / TokensTab 共用、
 * modelTrend.test.ts 直接测试。从 日×模型 聚合（模型 → 日期 → 数值）构建折线图数据：
 * 按区间累计降序取 Top8，其余聚合为「其他」系列；行内使用内部数据键规避 recharts dataKey 断链。
 */

/** 「其他」聚合系列的内部数据键 */
export const REST_SERIES_KEY = 'rest';

export interface TrendSeries {
  /** 行内数据键（内部键 m0/m1/rest，规避 recharts dataKey 的 lodash path 解析对含 `.` 模型名断链） */
  key: string;
  /** 展示名（Legend/Tooltip） */
  name: string;
}

export interface ModelTrendData {
  /** 折线系列（按区间累计降序，「其他」在末位） */
  series: TrendSeries[];
  /** 逐日行：label + 各系列数值（缺失日补 0） */
  rows: Record<string, number | string>[];
}

export function buildModelTrend(
  /** 模型 → 日期 → 数值（日×模型聚合，即 CreditsTab/TokensTab 的 agg.modelByDate） */
  modelByDate: Map<string, Map<string, number>>,
  /** 升序自然日序列（useDateRange.dateList） */
  dateList: string[],
  /** 区间键：'year' 时 label 为三段去前导零，否则 M/D */
  range: string,
): ModelTrendData {
  // 区间累计求和 → 降序取 Top8，其余聚合为「其他」
  const totals = new Map<string, number>();
  for (const [model, days] of modelByDate) {
    let sum = 0;
    for (const v of days.values()) sum += v;
    totals.set(model, sum);
  }
  const sorted = [...totals.entries()].sort((a, b) => b[1] - a[1]);
  const top8 = sorted.slice(0, 8).map(([m]) => m);
  const rest = sorted.slice(8).map(([m]) => m);
  // 行内用内部键 m0/m1/rest：recharts dataKey 走 lodash path 解析，含 `.` 的模型名（如 gpt-4.1）会断链
  const series: TrendSeries[] = [
    ...top8.map((name, i) => ({ key: `m${i}`, name })),
    ...(rest.length > 0 ? [{ key: REST_SERIES_KEY, name: '其他' }] : []),
  ];
  const rows = dateList.map((date) => {
    const row: Record<string, number | string> = {
      label:
        range === 'year'
          ? `${+date.slice(0, 4)}/${+date.slice(5, 7)}/${+date.slice(8, 10)}`
          : `${+date.slice(5, 7)}/${+date.slice(8, 10)}`,
    };
    top8.forEach((m, i) => {
      row[`m${i}`] = modelByDate.get(m)?.get(date) ?? 0;
    });
    if (rest.length > 0) {
      let restSum = 0;
      for (const m of rest) restSum += modelByDate.get(m)?.get(date) ?? 0;
      row[REST_SERIES_KEY] = restSum;
    }
    return row;
  });
  return { series, rows };
}
