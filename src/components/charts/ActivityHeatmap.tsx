import { useMemo } from 'react';
import { useIsDark } from '../../lib/useIsDark';
import { addDays, fmtDate } from '../../hooks/useDateRange';

/**
 * 公共年度活动热力图（credits-dashboard-plan.md §4.2）：
 * 合并两处内联实现。GitHub 贡献图风格：周一为首列，53 周 × 7 行，值取 [0, max] 四档梯度。
 * 弹性布局：53 周列 flex-1 均分容器宽度（与面板同宽），格子 aspect-square 派生高度；
 * 窄容器回退横向滚动（min-w 640px）。
 */

/** 绿色梯度：GitHub 明/暗两套色板 */
const HEAT_COLORS = {
  light: { empty: '#ebedf0', levels: ['#9be9a8', '#40c463', '#30a14e', '#216e39'] },
  dark: { empty: '#27272a', levels: ['#0e4429', '#006d32', '#26a641', '#39d353'] },
};

export default function ActivityHeatmap({
  values,
  unit,
  fmtValue,
  emptyHint,
}: {
  /** 日期 → 数值（键为 YYYY-MM-DD；全量历史，不受区间筛选影响，由调用方决定） */
  values: Map<string, number>;
  /** 数值单位（tooltip 与空态文案） */
  unit: string;
  /** 数值格式化（缺省千分位） */
  fmtValue?: (n: number) => string;
  emptyHint?: string;
}) {
  const isDark = useIsDark();
  const fmt = fmtValue ?? ((n: number) => n.toLocaleString('zh-CN'));

  const { weeks, monthLabels, max } = useMemo(() => {
    const end = new Date();
    // 起点对齐到 end 所在周的周一（行 0=周一 … 6=周日），回退 52 周
    const weekday = (end.getDay() + 6) % 7;
    const gridEnd = addDays(end, 6 - weekday);
    const gridStart = addDays(gridEnd, -7 * 53 + 1);
    const ws: { date: string; value: number }[][] = [];
    const ml: (string | null)[] = [];
    let prevMonth = -1;
    let m = 0;
    for (let w = 0; w < 53; w++) {
      const col: { date: string; value: number }[] = [];
      for (let d = 0; d < 7; d++) {
        const key = fmtDate(addDays(gridStart, w * 7 + d));
        const v = values.get(key) ?? 0;
        col.push({ date: key, value: v });
        m = Math.max(m, v);
      }
      ws.push(col);
      const first = col[0]!.date;
      const mo = new Date(`${first}T00:00:00`).getMonth();
      ml.push(mo !== prevMonth ? `${mo + 1}月` : null);
      prevMonth = mo;
    }
    return { weeks: ws, monthLabels: ml, max: m };
  }, [values]);

  const colors = isDark ? HEAT_COLORS.dark : HEAT_COLORS.light;
  const heatColor = (v: number): string => {
    if (v <= 0 || max <= 0) return colors.empty;
    const r = v / max;
    if (r <= 0.25) return colors.levels[0];
    if (r <= 0.5) return colors.levels[1];
    if (r <= 0.75) return colors.levels[2];
    return colors.levels[3];
  };

  if (max === 0) {
    return (
      <div className="py-4 text-xs text-slate-400">
        {emptyHint ?? `暂无${unit}记录。`}
      </div>
    );
  }

  return (
    <div className="overflow-x-auto pb-1">
      {/* 弹性网格：53 周列 flex-1 均分容器宽度（与「模型消耗排行」同宽），格子 aspect-square 派生高度 */}
      <div className="min-w-[640px]">
        {/* 月份标签行 */}
        <div className="mb-[3px] flex gap-[3px] pl-6">
          {weeks.map((_, i) => (
            <span
              key={i}
              className="flex-1 truncate text-[10px] leading-none text-slate-400 dark:text-zinc-500"
            >
              {monthLabels[i] ?? ''}
            </span>
          ))}
        </div>
        <div className="flex gap-[3px]">
          {/* 周一/三/五 标注列 */}
          <div className="flex w-5 shrink-0 flex-col justify-between text-[10px] leading-none text-slate-400 dark:text-zinc-500">
            <span>一</span>
            <span>三</span>
            <span>五</span>
          </div>
          {/* 7 行格（按周列组织） */}
          <div className="flex flex-1 gap-[3px]">
            {weeks.map((week, wi) => (
              <div key={wi} className="flex flex-1 flex-col gap-[3px]">
                {week.map((cell) => (
                  <div
                    key={cell.date}
                    className="aspect-square w-full rounded-[2px]"
                    style={{ background: heatColor(cell.value) }}
                    title={`${cell.date} · ${fmt(cell.value)} ${unit}`}
                  />
                ))}
              </div>
            ))}
          </div>
        </div>
        {/* 少 → 多 图例 */}
        <div className="mt-1.5 flex items-center justify-end gap-1 text-[10px] text-slate-400 dark:text-zinc-500">
          少
          {[colors.empty, ...colors.levels].map((c) => (
            <span key={c} className="h-[10px] w-[10px] rounded-[2px]" style={{ background: c }} />
          ))}
          多
        </div>
      </div>
    </div>
  );
}
