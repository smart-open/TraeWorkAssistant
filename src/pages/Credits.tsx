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
} from 'recharts';
import { Coins } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { StatCard, Badge, EmptyState } from '../components/ui';
import { useAppStore } from '../store';

const COLORS = ['#6366f1', '#818cf8', '#22c55e', '#f59e0b', '#ef4444', '#0ea5e9', '#a855f7', '#14b8a6', '#f43f5e', '#10b981'];

export default function Credits() {
  const accounts = useAppStore((s) => s.accounts);
  const groups = useAppStore((s) => s.groups);

  const rows = useMemo(
    () => [...accounts].sort((a, b) => (b.credits ?? -1) - (a.credits ?? -1)),
    [accounts],
  );
  const total = rows.reduce((s, a) => s + (a.credits ?? 0), 0);
  const avg = rows.length === 0 ? 0 : Math.round(total / rows.length);

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="积分看板"
        desc="查看每个账号的积分余额与排行"
      />

      <div className="mb-5 grid grid-cols-3 gap-3">
        <StatCard label="积分总额" value={total.toLocaleString()} tone="amber" />
        <StatCard label="账号数" value={rows.length} tone="brand" />
        <StatCard label="账号平均积分" value={avg.toLocaleString()} tone="blue" />
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