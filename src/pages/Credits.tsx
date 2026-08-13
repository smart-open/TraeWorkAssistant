import { useMemo } from 'react';
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  ResponsiveContainer,
  Tooltip,
  CartesianGrid,
  Cell,
  LabelList,
} from 'recharts';
import { Coins } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { StatCard, Badge, EmptyState } from '../components/ui';
import { useAppStore } from '../store';

function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = `${d.getMonth() + 1}`.padStart(2, '0');
  const day = `${d.getDate()}`.padStart(2, '0');
  return `${y}-${m}-${day}`;
}

export default function Credits() {
  const accounts = useAppStore((s) => s.accounts);
  const groups = useAppStore((s) => s.groups);
  const creditsHistory = useAppStore((s) => s.creditsHistory);

  const rows = useMemo(
    () => [...accounts].sort((a, b) => (b.remaining_credits ?? -1) - (a.remaining_credits ?? -1)),
    [accounts],
  );
  const total = rows.reduce((s, a) => s + (a.remaining_credits ?? 0), 0);
  const avg = rows.length === 0 ? 0 : Math.round(total / rows.length);

  // 今日新增积分：
  // - 有 credits_history 记录时，取当天 delta 之和
  // - 没有历史记录但有可用积分时，用当前总可用积分作为今日数据
  const today = localDate(new Date());
  const todayNew = useMemo(() => {
    const histVal = creditsHistory
      .filter((r) => r.date === today)
      .reduce((s, r) => s + (r.delta || 0), 0);
    if (histVal > 0) return histVal;
    if (creditsHistory.length === 0 && total > 0) return Math.round(total);
    return histVal;
  }, [creditsHistory, today, total]);

  // 近 7 日趋势：
  // - 有 credits_history 记录时，按日期聚合 delta
  // - 没有历史记录时，用当前 total 作为今日数据点，其余天为 0
  const trend = useMemo(() => {
    const map = new Map<string, number>();
    for (const r of creditsHistory) map.set(r.date, (map.get(r.date) || 0) + (r.delta || 0));
    const days: { label: string; delta: number }[] = [];
    for (let i = 6; i >= 0; i--) {
      const d = new Date(Date.now() - i * 86400000);
      const key = localDate(d);
      let val = map.get(key) || 0;
      if (i === 0 && val === 0 && total > 0) {
        val = Math.round(total);
      }
      days.push({ label: `${d.getMonth() + 1}/${d.getDate()}`, delta: val });
    }
    return days;
  }, [creditsHistory, total]);

  const hasTrend = trend.some((d) => d.delta > 0);

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="积分看板"
        desc="查看每个账号的积分余额与趋势"
      />

      <div className="mb-5 grid grid-cols-2 gap-3 md:grid-cols-4">
        <StatCard label="可用积分总额" value={total.toLocaleString('zh-CN', { minimumFractionDigits: 0, maximumFractionDigits: 2 })} hint="总剩余可用积分" tone="amber" />
        <StatCard label="账号数" value={rows.length} tone="brand" />
        <StatCard label="平均可用积分" value={avg.toLocaleString()} tone="blue" />
        <StatCard label="今日新增积分" value={todayNew.toLocaleString()} tone="green" hint={today} />
      </div>

      <div className="card p-5">
        <div className="mb-4 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <h3 className="font-medium">近 7 日积分趋势</h3>
            <span className="rounded-full bg-zinc-100 px-2 py-0.5 text-xs font-medium text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
              7 Days
            </span>
          </div>
          <span className="text-xs text-slate-400">
            {creditsHistory.length > 0 ? '每日新增积分' : '当前可用积分'}
          </span>
        </div>
        {accounts.length === 0 ? (
          <EmptyState icon={<Coins size={28} />} title="尚无账号数据" hint="添加账号后这里会展示积分趋势。" />
        ) : !hasTrend ? (
          <EmptyState
            icon={<Coins size={28} />}
            title="暂无趋势数据"
            hint="执行签到或刷新积分后，这里会展示每日积分变化趋势。"
          />
        ) : (
          <div className="h-56">
            <ResponsiveContainer>
              <BarChart data={trend} margin={{ top: 24, right: 16, left: 0, bottom: 4 }} barCategoryGap="40%">
                <defs>
                  <linearGradient id="barGradient" x1="0" y1="0" x2="0" y2="1">
                    <stop offset="0%" stopColor="#3f3f46" />
                    <stop offset="100%" stopColor="#71717a" />
                  </linearGradient>
                </defs>
                <CartesianGrid strokeDasharray="3 3" stroke="#e2e8f0" opacity={0.25} vertical={false} />
                <XAxis
                  dataKey="label"
                  tick={{ fontSize: 11, fill: '#94a3b8' }}
                  axisLine={{ stroke: '#e2e8f0' }}
                  tickLine={false}
                />
                <YAxis tick={{ fontSize: 11, fill: '#94a3b8' }} axisLine={false} tickLine={false} width={48} />
                <Tooltip
                  cursor={{ fill: 'rgba(0,0,0,0.03)' }}
                  contentStyle={{
                    fontSize: 12,
                    borderRadius: 10,
                    border: '1px solid #e2e8f0',
                    boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
                    padding: '8px 12px',
                  }}
                  formatter={(v: number) => [v.toLocaleString(), '积分']}
                />
                <Bar dataKey="delta" radius={[6, 6, 0, 0]} maxBarSize={36}>
                  {trend.map((d, i) => {
                    const isToday = i === trend.length - 1;
                    return (
                      <Cell
                        key={i}
                        fill={isToday && creditsHistory.length === 0 ? '#71717a' : 'url(#barGradient)'}
                      />
                    );
                  })}
                  <LabelList
                    dataKey="delta"
                    position="top"
                    formatter={(v: number) => v > 0 ? (v >= 1000 ? `${(v / 1000).toFixed(1)}k` : v.toFixed(0)) : ''}
                    style={{ fontSize: 10, fill: '#94a3b8', fontWeight: 500 }}
                  />
                </Bar>
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>

      <div className="mt-5 card overflow-hidden">
        <div className="border-b border-slate-100 px-5 py-3 dark:border-zinc-800">
          <h3 className="font-medium">账号积分明细</h3>
        </div>
        <table className="w-full text-sm">
          <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
            <tr>
              <th className="px-4 py-2 text-left">排名</th>
              <th className="px-4 py-2 text-left">账号</th>
              <th className="px-4 py-2 text-left">分组</th>
              <th className="px-4 py-2 text-right">剩余可用积分</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((a, i) => {
              const g = groups.find((x) => x.id === a.group_id);
              return (
                <tr key={a.user_id} className="border-t border-slate-200 dark:border-zinc-800">
                  <td className="px-4 py-2">#{i + 1}</td>
                  <td className="px-4 py-2">
                    <div className="font-medium">{a.name}</div>
                    <div className="text-xs text-slate-400">{a.user_id}</div>
                  </td>
                  <td className="px-4 py-2">
                    {g ? (
                      <Badge tone="slate">
                        <span className="inline-block h-2 w-2 rounded-full" style={{ background: g.color }} />
                        {g.name}
                      </Badge>
                    ) : (
                      <span className="text-xs text-slate-400">未分组</span>
                    )}
                  </td>
                  <td className="px-4 py-2 text-right tabular-nums">{(a.remaining_credits ?? 0).toLocaleString('zh-CN', { minimumFractionDigits: 0, maximumFractionDigits: 2 })}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
