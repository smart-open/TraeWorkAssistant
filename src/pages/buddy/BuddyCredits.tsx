import { useCallback, useEffect, useState } from 'react';
import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  ResponsiveContainer,
  Tooltip,
  CartesianGrid,
} from 'recharts';
import { RefreshCw, Coins } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { StatCard, Badge, EmptyState } from '../../components/ui';
import ExpiryCalendar from '../../components/ExpiryCalendar';
import TokenStatsPanel from './TokenStatsPanel';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { useIsDark } from '../../lib/useIsDark';
import { withMinDelay } from '../../lib/delay';
import type { WbCreditsResult, WbCreditAccount, WbCreditPackage, WbUsageFallback } from '../../types';

/**
 * buddy-credits 积分看板（§3.7.4，F-20/F-22/F-56/F-57/F-58/F-25，对齐 Trae 积分看板）：
 * KPI 统计 + 近 7 日用量趋势 + 账号积分明细排名 + 积分包到期日历；Token 统计 Tab。
 */
export default function BuddyCredits() {
  const pushToast = useAppStore((s) => s.pushToast);
  const isDark = useIsDark();
  const [result, setResult] = useState<WbCreditsResult | null>(null);
  const [usage, setUsage] = useState<WbUsageFallback | null>(null);
  const [loading, setLoading] = useState(false);
  const [tab, setTab] = useState<'credits' | 'stats'>('credits');

  const refresh = useCallback(
    async (fresh = false) => {
      setLoading(true);
      try {
        const r = await withMinDelay(api.workbuddy.creditsFetch(undefined, fresh), 1000);
        setResult(r);
        if (fresh) pushToast('success', '积分已刷新');
      } catch (err) {
        pushToast('error', `积分查询失败：${String(err)}`);
      } finally {
        setLoading(false);
      }
      // 用量快照（本地推导，失败静默隐藏趋势卡）
      api.workbuddy
        .usageFallback()
        .then(setUsage)
        .catch(() => setUsage(null));
      // eslint-disable-next-line react-hooks/exhaustive-deps
    },
    [],
  );

  useEffect(() => {
    void refresh(false);
  }, [refresh]);

  const accounts: WbCreditAccount[] = [...(result?.accounts ?? [])].sort(
    (a, b) => (b.balance ?? -1) - (a.balance ?? -1),
  );
  const total = accounts.reduce((s, a) => s + (a.balance ?? 0), 0);
  const okCount = accounts.filter((a) => a.ok).length;
  const allPackages: { acc: WbCreditAccount; pkg: WbCreditPackage }[] = accounts.flatMap((acc) =>
    (acc.packages ?? []).map((pkg) => ({ acc, pkg })),
  );
  const soonCount = allPackages.filter((x) => x.pkg.expire_soon).length;
  const avg = accounts.length === 0 ? 0 : total / accounts.length;

  // 近 7 日用量趋势（usageFallback 本地推导快照）
  const trend = (usage?.daily ?? []).slice(-7).map((d) => ({
    label: d.date.slice(5),
    usage: d.usage,
  }));
  const hasTrend = trend.some((d) => d.usage > 0);

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 积分看板"
        desc="积分余额 · 积分明细 · 到期日历"
        actions={
          <>
            <div className="flex rounded-lg bg-slate-100 p-1 dark:bg-zinc-900">
              <button
                className={`rounded-md px-3 py-1 text-xs font-medium ${tab === 'credits' ? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-800 dark:text-zinc-100' : 'text-slate-500 dark:text-zinc-400'}`}
                onClick={() => setTab('credits')}
              >
                积分统计
              </button>
              <button
                className={`rounded-md px-3 py-1 text-xs font-medium ${tab === 'stats' ? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-800 dark:text-zinc-100' : 'text-slate-500 dark:text-zinc-400'}`}
                onClick={() => setTab('stats')}
              >
                Token 统计
              </button>
            </div>
            <button className="btn-outline" onClick={() => void refresh(true)} disabled={loading}>
              <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 强制刷新
            </button>
          </>
        }
      />

      {tab === 'stats' ? (
        <TokenStatsPanel remainingCredits={result ? total : null} />
      ) : (
        <>
          {/* KPI 卡（对齐 Trae 积分看板：5 卡） */}
          <div className="grid grid-cols-2 gap-3 md:grid-cols-5">
            <StatCard
              label="总剩余积分"
              value={total.toFixed(2)}
              hint={result?.cached ? '缓存数据（≥5 分钟）' : '实时数据'}
              tone="violet"
            />
            <StatCard label="账号数" value={accounts.length} hint={`${okCount} 个查询成功`} tone="brand" />
            <StatCard label="平均可用积分" value={accounts.length > 0 ? avg.toFixed(0) : '0'} tone="blue" />
            <StatCard label="积分包总数" value={String(allPackages.length)} hint="paid + free 包合计" tone="amber" />
            <StatCard
              label="7 天内到期"
              value={String(soonCount)}
              hint={soonCount > 0 ? '见下方到期日历' : '暂无临期包'}
              tone={soonCount > 0 ? 'red' : 'green'}
            />
          </div>

          {/* 近 7 日用量趋势（本地快照推导，对齐 Trae 积分看板趋势卡形态） */}
          {usage && hasTrend && (
            <div className="mt-5 card p-5">
              <div className="mb-4 flex items-center justify-between">
                <div className="flex items-center gap-2">
                  <h3 className="font-medium">近 7 日积分消耗</h3>
                  <span className="rounded-full bg-zinc-100 px-2 py-0.5 text-xs font-medium text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                    7 Days
                  </span>
                </div>
                <div className="flex items-center gap-3 text-xs text-slate-400">
                  <span className="flex items-center gap-1">
                    <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#f59e0b' }} />
                    消耗积分
                  </span>
                  <span>本地用量快照推导</span>
                </div>
              </div>
              <div className="h-56">
                <ResponsiveContainer>
                  <LineChart data={trend} margin={{ top: 24, right: 16, left: 0, bottom: 4 }}>
                    <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                    <XAxis
                      dataKey="label"
                      tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }}
                      axisLine={{ stroke: isDark ? '#3f3f46' : '#e2e8f0' }}
                      tickLine={false}
                    />
                    <YAxis tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }} axisLine={false} tickLine={false} width={48} />
                    <Tooltip
                      cursor={{ stroke: isDark ? '#52525b' : '#cbd5e1', strokeWidth: 1, strokeDasharray: '3 3' }}
                      contentStyle={{
                        fontSize: 12,
                        borderRadius: 10,
                        border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
                        background: isDark ? '#18181b' : '#fff',
                        color: isDark ? '#e4e4e7' : '#1e293b',
                        boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
                        padding: '8px 12px',
                      }}
                      formatter={(v: number) => [Number(v).toFixed(2), '消耗积分']}
                    />
                    <Line type="monotone" dataKey="usage" stroke="#f59e0b" strokeWidth={2.5} dot={{ r: 3, fill: '#f59e0b', strokeWidth: 0 }} activeDot={{ r: 5 }} />
                  </LineChart>
                </ResponsiveContainer>
              </div>
            </div>
          )}

          {/* 账号积分明细（对齐 Trae：排名表格） */}
          {accounts.length === 0 ? (
            <div className="mt-5">
              <EmptyState
                icon={<Coins size={22} />}
                title="暂无积分数据"
                hint="请先在「账号管理」导入账号（需已录入凭证）；查询失败时请检查客户端登录态。"
              />
            </div>
          ) : (
            <div className="mt-5 card overflow-hidden">
              <div className="border-b border-slate-100 px-5 py-3 dark:border-zinc-800">
                <h3 className="font-medium">账号积分明细</h3>
              </div>
              <table className="w-full text-sm">
                <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                  <tr>
                    <th className="px-4 py-2 text-left">排名</th>
                    <th className="px-4 py-2 text-left">账号</th>
                    <th className="px-4 py-2 text-left">积分包</th>
                    <th className="px-4 py-2 text-right">可用积分</th>
                  </tr>
                </thead>
                <tbody>
                  {accounts.map((a, i) => (
                    <tr key={a.user_id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                      <td className="px-4 py-2 text-slate-400">#{i + 1}</td>
                      <td className="px-4 py-2">
                        <div className="flex items-center gap-1.5 font-medium">
                          {a.name}
                          {a.source === 'legacy' && <Badge tone="slate">旧接口回退</Badge>}
                        </div>
                        {!a.ok && <div className="text-xs text-rose-500">{a.message}</div>}
                      </td>
                      <td className="px-4 py-2 text-xs text-slate-400">{a.packages?.length ?? 0} 个包</td>
                      <td className="px-4 py-2 text-right tabular-nums">
                        {a.balance != null ? a.balance.toFixed(2) : '—'}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}

          {/* 积分包到期日历（F-13；环境配置页的到期日历已收敛到这里） */}
          <div className="mt-5 card p-4">
            <h3 className="mb-3 font-medium">积分包到期日历</h3>
            <ExpiryCalendar
              items={allPackages.map(({ acc, pkg }) => ({
                key: `${acc.user_id}-${pkg.name}-${pkg.expire_ts ?? 0}`,
                label: `${acc.name} · ${pkg.name}`,
                kind: '积分包',
                expire_ts: pkg.expire_ts,
                note: `剩余 ${pkg.remaining.toFixed(2)} / ${(pkg.total ?? 0).toFixed(2)}`,
              }))}
              emptyHint="暂无积分包：待账号录入凭证并完成积分查询后展示到期时间。"
            />
          </div>
        </>
      )}
    </div>
  );
}
