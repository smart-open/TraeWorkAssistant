import { useMemo, useState } from 'react';

/**
 * 公共日期范围 hook（credits-dashboard-plan.md §4.2，自 TokenStatsPanel/Credits 内联实现上提）。
 * 边界均为本地自然日 ISO 字符串（YYYY-MM-DD，可直接字符串比较）；dateList 为升序自然日序列。
 */

export type RangeKey = 'today' | '7d' | '30d' | 'month' | 'year';

export const RANGES: { key: RangeKey; label: string }[] = [
  { key: 'today', label: '今日' },
  { key: '7d', label: '近7天' },
  { key: '30d', label: '近30天' },
  { key: 'month', label: '本月' },
  { key: 'year', label: '近一年' },
];

export function fmtDate(d: Date): string {
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

export function addDays(d: Date, n: number): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
}

export interface DateRange {
  /** 所选范围键 */
  range: RangeKey;
  setRange: (r: RangeKey) => void;
  /** 范围起点（含）YYYY-MM-DD */
  startStr: string;
  /** 今天（含）YYYY-MM-DD */
  todayStr: string;
  /** 范围内自然日升序序列（year 也返回 365 天，供趋势图逐日铺底） */
  dateList: string[];
}

export function useDateRange(initial: RangeKey = '7d'): DateRange {
  const [range, setRange] = useState<RangeKey>(initial);
  return useMemo(() => {
    const today = new Date();
    const t = fmtDate(today);
    let s = t;
    switch (range) {
      case 'today':
        s = t;
        break;
      case '7d':
        s = fmtDate(addDays(today, -6));
        break;
      case '30d':
        s = fmtDate(addDays(today, -29));
        break;
      case 'month':
        s = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-01`;
        break;
      case 'year':
        s = fmtDate(addDays(today, -364));
        break;
    }
    const list: string[] = [];
    let cur = new Date(`${s}T00:00:00`);
    while (fmtDate(cur) <= t) {
      list.push(fmtDate(cur));
      cur = addDays(cur, 1);
    }
    return { range, setRange, startStr: s, todayStr: t, dateList: list };
  }, [range]);
}
