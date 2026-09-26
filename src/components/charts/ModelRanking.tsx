import { Progress } from '../ui';

/**
 * 公共模型消耗排行（credits-dashboard-plan.md §4.2，合并两处内联实现）：
 * Top8 + 其余合计行，Progress 条按占比展示。
 */

export interface RankItem {
  model: string;
  value: number;
  /** 可选调用/请求次数（展示在数值旁） */
  calls?: number;
}

export default function ModelRanking({
  items,
  fmtValue,
  unit,
  emptyHint,
}: {
  items: RankItem[];
  /** 数值格式化 */
  fmtValue: (n: number) => string;
  /** 单位（如「积分」「tokens」） */
  unit: string;
  emptyHint?: string;
}) {
  const sorted = [...items].sort((a, b) => b.value - a.value);
  const top = sorted.slice(0, 8);
  const rest = sorted.slice(8);
  const grand = sorted.reduce((s, x) => s + x.value, 0);
  const restTotal = rest.reduce((s, x) => s + x.value, 0);

  if (top.length === 0) {
    return <div className="text-xs text-slate-400">{emptyHint ?? '暂无排行数据'}</div>;
  }

  return (
    <div className="space-y-2">
      {top.map((m) => (
        <div key={m.model} className="rounded-lg border border-slate-100 px-3 py-2 dark:border-zinc-800">
          <div className="flex items-center justify-between gap-2 text-sm">
            <span className="truncate font-medium">{m.model}</span>
            <span className="shrink-0 tabular-nums text-xs text-slate-400">
              {m.calls != null ? `${m.calls} 次 · ` : ''}
              {fmtValue(m.value)} {unit}
            </span>
          </div>
          <Progress value={m.value} max={grand} />
        </div>
      ))}
      {(rest.length > 0 || sorted.length > 8) && (
        <div className="px-3 text-xs text-slate-400">
          其余 {rest.length} 个模型合计 {fmtValue(restTotal)} {unit} · 全部模型合计 {fmtValue(grand)} {unit}
        </div>
      )}
    </div>
  );
}
