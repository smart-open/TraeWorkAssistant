/**
 * 趋势图时间区间工具（Trae / Buddy 积分看板共用）：
 * 区间 → 本地自然日序列（升序），快照缺失日期由调用方映射为 null 跳点。
 */

export function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = `${d.getMonth() + 1}`.padStart(2, '0');
  const day = `${d.getDate()}`.padStart(2, '0');
  return `${y}-${m}-${day}`;
}

export type RangeKey = 'today' | '7d' | '30d' | 'month' | 'year';

export const RANGES: { key: RangeKey; label: string }[] = [
  { key: 'today', label: '今日' },
  { key: '7d', label: '近7天' },
  { key: '30d', label: '近30天' },
  { key: 'month', label: '本月' },
  { key: 'year', label: '近一年' },
];

/** 区间 → 本地自然日序列（升序） */
export function rangeDates(range: RangeKey): string[] {
  const today = new Date();
  const dates: string[] = [];
  const push = (d: Date) => dates.push(localDate(d));
  switch (range) {
    case 'today':
      push(today);
      break;
    case '7d':
      for (let i = 6; i >= 0; i--) push(new Date(Date.now() - i * 86400000));
      break;
    case '30d':
      for (let i = 29; i >= 0; i--) push(new Date(Date.now() - i * 86400000));
      break;
    case 'month': {
      for (
        let d = new Date(today.getFullYear(), today.getMonth(), 1);
        d <= today;
        d = new Date(d.getTime() + 86400000)
      ) {
        push(new Date(d));
      }
      break;
    }
    case 'year':
      for (let i = 364; i >= 0; i--) push(new Date(Date.now() - i * 86400000));
      break;
  }
  return dates;
}
