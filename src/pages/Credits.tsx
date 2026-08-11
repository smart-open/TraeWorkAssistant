import { useMemo } from 'react';
import {
  BarChart,
  Bar,
  LineChart,
  Line,
  XAxis,
  YAxis,
  ResponsiveContainer,
  Tooltip,
  CartesianGrid,
  Cell,
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

const COLORS = ['#6366f1', '#818cf8', '#22c55e', '#f59e0b', '#ef4444', '#0ea5e9', '#a855f7', '#14b8a6', '#f43f5e', '#10b981'];

export default function Credits() {
  const accounts = useAppStore((s) => s.accounts);
  const groups = useAppStore((s) => s.groups);
  const creditsHistory = useAppStore((s) => s.creditsHistory);

  const rows = useMemo(
    () => [...accounts].sort((a, b) => (b.credits ?? -1) - (a.credits ?? -1)),
    [accounts],
  );
  const total = rows.reduce((s, a) => s + (a.credits ?? 0), 0);
  const avg = rows.length === 0 ? 0 : Math.round(total / rows.length);

  // 今日新增积分：credits_history 中当天 delta 之和
  const today = localDate(new Date());
  const todayNew = useMemo(
    () => creditsHistory.filter((r) => r.date === today).reduce((s, r) => s + (r.delta || 0), 0),
    [creditsHistory, today],
  );
  // 近 7 日趋势：按本地日期聚合每日新增
  const trend = useMemo(() => {
    const map = new Map<string, number>();
    for (const r of creditsHistory) map.set(r.date, (map.get(r.date) || 0) + (r.delta || 0));
    const days: { label: string; delta: number }[] = [];
    for (let i = 6; i >= 0; i--) {
      const d = new Date(Date.now() - i * 86400000);
      const key = localDate(d);
      days.push({ label: `${d.getMonth() + 1}/${d.getDate()}`, delta: map.get(key) || 0 });
    }
    return days;
  }, [creditsHistory]);

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="积分看板"
        desc="查看每个账号的积分余额与排行"
      />

      <div className="mb-5 grid grid-cols-2 gap-3 md:grid-cols-4">
        <StatCard label="积分总额" value={total.toLocaleString()} tone="amber" />
        <StatCard label="账号数" value={rows.length} tone="brand" />
        <StatCard label="账号平均积分" value={avg.toLocaleString()} tone="blue" />
        <StatCard label="今日新增积分" value={todayNew.toLocaleString()} tone="green" hint={today} />
      </div>

      <div className="grid gap-3 md:grid-cols-1">
        <div className="card p-4">
          <h3 className="mb-3 font-medium">账号积分排行</h3>
          {rows.length === 0 ? (
            <EmptyState icon={<Coins size={28} />} title="尚无积分数据" hint="添加账号或运行一次签到即可看到。" />
          ) : (
            <div className="h-72">
              <ResponsiveContainer>
                <BarChart data={rows.slice(0, 12)} margin={{ top: 8, right: 12, left: 0, bottom: 4 }}>
                  <CartesianGrid strokeDasharray="3 3" stroke="#e2e8f0" opacity={0.4} />
                  <XAxis dataKey="name" tick={{ fontSize: 11 }} interval={0} angle={-18} textAnchor="end" height={48} />
                  <YAxis tick={{ fontSize: 11 }} />
                  <Tooltip
                    contentStyle={{ fontSize: 12, borderRadius: 8 }}
                    formatter={(v: number) => v.toLocaleString()}
                  />
                  <Bar dataKey="credits" radius={[6, 6, 0, 0]}>
                    {rows.slice(0, 12).map((_, i) => (
                      <Cell key={i} fill={COLORS[i % COLORS.length]} />
                    ))}
                  </Bar>
                </BarChart>
              </ResponsiveContainer>
            </div>
          )}
        </div>

        <div className="card p-4">
          <h3 className="mb-3 font-medium">近 7 日积分趋势</h3>
          {creditsHistory.length === 0 ? (
            <EmptyState icon={<Coins size={28} />} title="尚无趋势数据" hint="运行签到后这里会展示每日积分变化。" />
          ) : (
            <div className="h-48">
              <ResponsiveContainer>
                <LineChart data={trend} margin={{ top: 8, right: 12, left: 0, bottom: 4 }}>
                  <CartesianGrid strokeDasharray="3 3" stroke="#e2e8f0" opacity={0.4} />
                  <XAxis dataKey="label" tick={{ fontSize: 11 }} />
                  <YAxis tick={{ fontSize: 11 }} />
                  <Tooltip contentStyle={{ fontSize: 12, borderRadius: 8 }} />
                  <Line type="monotone" dataKey="delta" stroke="#6366f1" strokeWidth={2} dot={{ r: 3 }} />
                </LineChart>
              </ResponsiveContainer>
            </div>
          )}
        </div>
      </div>

      <div className="mt-5 card overflow-hidden">
        <table className="w-full text-sm">
          <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-slate-900">
            <tr>
              <th className="px-4 py-2 text-left">排名</th>
              <th className="px-4 py-2 text-left">账号</th>
              <th className="px-4 py-2 text-left">分组</th>
              <th className="px-4 py-2 text-right">积分</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((a, i) => {
              const g = groups.find((x) => x.id === a.group_id);
              return (
                <tr key={a.user_id} className="border-t border-slate-200 dark:border-slate-800">
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
                  <td className="px-4 py-2 text-right tabular-nums">{(a.credits ?? 0).toLocaleString()}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}