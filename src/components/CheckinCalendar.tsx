import { useMemo, useState } from 'react';
import { CalendarDays, ChevronLeft, ChevronRight } from 'lucide-react';
import { Badge } from './ui';
import {
  canShiftMonth,
  dayDotToneClass,
  monthBounds,
  monthCells,
  shiftMonth,
  type CheckinCalendarDay,
} from './checkinCalendarModel';

// 类型由调用方（页面）从本文件导入；内核纯逻辑在 checkinCalendarModel.ts（含单测）
export type { CheckinCalendarDay, CheckinCalendarEntry } from './checkinCalendarModel';

/**
 * 签到档期日历（通用卡，F-80-余 v2 自 Qoder「每日签到」页抽平复用）：
 * 近 90 天签到结果按日可视化（月切换 + 选中日明细），辅助校验排期决策。
 * 数据源由调用方聚合为逐日结构后传入（聚合逻辑见 checkinCalendarModel.ts）——
 * - Qoder/Buddy：每账号每日一条明细（status: success/already/fail）
 * - Trae：暂无逐账号历史命令，传一条「全账号汇总」行（status: partial = 部分失败）
 * 日格状态点：绿 = 全部已领 / 琥珀 = 部分失败 / 红 = 全部失败 / 灰 = 无数据。
 */

const STATUS_BADGE: Record<
  import('./checkinCalendarModel').CheckinCalendarEntry['status'],
  { text: string; tone: 'green' | 'blue' | 'red' | 'amber' }
> = {
  success: { text: '已领', tone: 'green' },
  already: { text: '此前已领', tone: 'blue' },
  fail: { text: '失败', tone: 'red' },
  partial: { text: '部分失败', tone: 'amber' },
};

export default function CheckinCalendarCard({
  title,
  days,
  footnote,
}: {
  title: string;
  days: CheckinCalendarDay[];
  footnote?: string;
}) {
  const byDay = useMemo(() => {
    const m = new Map<string, CheckinCalendarDay>();
    for (const d of days) m.set(d.date, d);
    return m;
  }, [days]);

  const [calMonth, setCalMonth] = useState(() => {
    const d = new Date();
    return { y: d.getFullYear(), m: d.getMonth() };
  });
  const [selectedDay, setSelectedDay] = useState<string | null>(null);

  const cal = useMemo(() => monthCells(calMonth.y, calMonth.m), [calMonth]);
  const bounds = useMemo(() => monthBounds(new Date()), []);
  const canPrev = canShiftMonth(calMonth, bounds, -1);
  const canNext = canShiftMonth(calMonth, bounds, 1);
  const goMonth = (delta: number) => setCalMonth((cur) => shiftMonth(cur, delta));

  const sel = selectedDay ? byDay.get(selectedDay) : undefined;

  return (
    <div className="mt-4 card p-4">
      <div className="mb-3 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <CalendarDays size={16} className="text-violet-500" />
          <span className="text-sm font-medium">{title}</span>
        </div>
        <div className="flex items-center gap-1.5">
          <button className="btn-outline px-2 py-1" onClick={() => goMonth(-1)} disabled={!canPrev}>
            <ChevronLeft size={14} />
          </button>
          <span className="min-w-[88px] text-center text-sm tabular-nums">
            {calMonth.y} 年 {calMonth.m + 1} 月
          </span>
          <button className="btn-outline px-2 py-1" onClick={() => goMonth(1)} disabled={!canNext}>
            <ChevronRight size={14} />
          </button>
        </div>
      </div>
      <div className="grid grid-cols-7 gap-1">
        {['日', '一', '二', '三', '四', '五', '六'].map((w) => (
          <div key={w} className="pb-1 text-center text-[11px] text-slate-400">
            {w}
          </div>
        ))}
        {Array.from({ length: cal.lead }).map((_, i) => (
          <div key={`lead-${i}`} />
        ))}
        {cal.cells.map(({ day, date }) => {
          const st = byDay.get(date);
          const isToday = date === new Date().toLocaleDateString('sv-SE');
          return (
            <button
              key={date}
              onClick={() => setSelectedDay(selectedDay === date ? null : date)}
              title={st ? `${date}：${st.ok} 已领 / ${st.fail} 失败` : `${date}：无签到记录`}
              className={`flex min-h-[52px] flex-col items-center justify-between rounded-lg border px-1 py-1 transition ${
                selectedDay === date
                  ? 'border-brand-400 bg-brand-50/60 dark:border-brand-500/60 dark:bg-brand-500/10'
                  : 'border-slate-100 hover:border-slate-300 dark:border-zinc-800 dark:hover:border-zinc-600'
              }`}
            >
              <span
                className={`text-[11px] tabular-nums ${
                  isToday ? 'font-bold text-brand-600 dark:text-brand-400' : 'text-slate-500 dark:text-zinc-400'
                }`}
              >
                {day}
              </span>
              <span className="flex items-center gap-0.5">
                {st &&
                  (st.dots && st.dots.length > 0
                    ? // 逐活动状态点（Qoder 双活动；最多展示 2 个，完整信息见选中日明细）
                      st.dots.slice(0, 2).map((tone, i) => (
                        <span key={i} className={`h-1.5 w-1.5 rounded-full ${tone}`} />
                      ))
                    : // 回退：按 ok/fail 计数的单点
                      <span className={`h-1.5 w-1.5 rounded-full ${dayDotToneClass(st.ok, st.fail)}`} />)}
              </span>
              <span className="text-[10px] tabular-nums text-emerald-600 dark:text-emerald-400">
                {st && st.reward != null && st.reward > 0
                  ? Number.isInteger(st.reward)
                    ? `+${st.reward}`
                    : `+${st.reward.toFixed(2)}`
                  : st && st.ok > 0
                    ? st.ok
                    : ''}
              </span>
            </button>
          );
        })}
      </div>
      <div className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 text-[11px] text-slate-400">
        <span className="flex items-center gap-1">
          <span className="h-1.5 w-1.5 rounded-full bg-emerald-500" />
          全部账号已领
        </span>
        <span className="flex items-center gap-1">
          <span className="h-1.5 w-1.5 rounded-full bg-amber-500" />
          部分失败
        </span>
        <span className="flex items-center gap-1">
          <span className="h-1.5 w-1.5 rounded-full bg-rose-500" />
          全部失败
        </span>
        {footnote && <span>{footnote}</span>}
      </div>
      {selectedDay && (
        <div className="mt-3 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
          <div className="mb-2 text-xs font-medium text-slate-500 dark:text-zinc-300">
            {selectedDay} 明细
          </div>
          {!sel || sel.entries.length === 0 ? (
            <div className="text-xs text-slate-400">当日无签到记录</div>
          ) : (
            <div className="space-y-1">
              {sel.entries.map((e, i) => (
                <div key={`${e.name}-${i}`} className="flex flex-wrap items-center gap-2 text-xs">
                  <span className="min-w-0 max-w-[180px] truncate font-medium text-slate-600 dark:text-zinc-300">
                    {e.name}
                  </span>
                  <Badge tone={STATUS_BADGE[e.status].tone}>{STATUS_BADGE[e.status].text}</Badge>
                  {e.note && <span className="min-w-0 flex-1 truncate text-slate-400">{e.note}</span>}
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
