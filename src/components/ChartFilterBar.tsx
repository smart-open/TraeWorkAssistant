import { type ReactNode } from 'react';
import { RANGES, type RangeKey } from '../hooks/useDateRange';

/** 数据源口径（credits-dashboard-plan.md §3.4）：本地 / API 网关 / 官网。
 *  源切换控件由页面级第二层分类承载（不在业务面板内），此处仅提供类型与标签。 */
export type BoardSource = 'local' | 'gateway' | 'official';

export const SOURCE_LABELS: Record<BoardSource, string> = {
  local: '本地',
  gateway: 'API网关',
  official: '官网',
};

export const SOURCES: BoardSource[] = ['local', 'gateway', 'official'];

/**
 * 公共图表工具行（业务面板内）：标题 + 徽标区 + 会话总数 + 模型下拉 + 日期范围。
 * 各统计 Tab（积分/Token）共用；数据源切换与全局刷新在页面工具行（Dashboard）承载。
 */
export default function ChartFilterBar({
  title,
  icon,
  badges,
  sessions,
  models,
  modelFilter,
  onModelFilter,
  range,
  onRange,
  extra,
}: {
  title: string;
  icon?: ReactNode;
  /** 标题后徽标区（数据源说明等） */
  badges?: ReactNode;
  /** 会话总数；null/undefined 显示「—」（§3.3：官网源=Trae sessions，本地源=summary.calls，网关=requests 合计） */
  sessions?: number | null;
  models: string[];
  modelFilter: string;
  onModelFilter: (m: string) => void;
  range: RangeKey;
  onRange: (r: RangeKey) => void;
  /** 右侧附加元素（如明细更新时间） */
  extra?: ReactNode;
}) {
  return (
    <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
      <div className="flex flex-wrap items-center gap-2">
        {icon}
        <h3 className="font-medium">{title}</h3>
        <span className="text-xs text-slate-400">
          会话总数：{sessions != null ? sessions.toLocaleString() : '—'}
        </span>
        {badges}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <select
          className="input !w-auto !py-1 text-xs"
          value={modelFilter}
          onChange={(e) => onModelFilter(e.target.value)}
          title="筛选趋势与消耗合计的模型"
        >
          <option value="">全部模型</option>
          {models.map((m) => (
            <option key={m} value={m}>
              {m}
            </option>
          ))}
        </select>
        <div className="flex items-center gap-1">
          {RANGES.map((r) => (
            <button
              key={r.key}
              onClick={() => onRange(r.key)}
              className={`chip border ${
                range === r.key
                  ? 'border-brand-500 text-brand-600 dark:text-brand-400'
                  : 'border-slate-200 text-slate-500 dark:border-zinc-700 dark:text-zinc-400'
              }`}
            >
              {r.label}
            </button>
          ))}
        </div>
        {extra}
      </div>
    </div>
  );
}
