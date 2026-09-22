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
import { RefreshCw, Coins, TrendingUp } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { StatCard, Badge, EmptyState } from '../../components/ui';
import ExpiryCalendar from '../../components/ExpiryCalendar';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { useIsDark } from '../../lib/useIsDark';
import { withMinDelay } from '../../lib/delay';
import { normZero } from '../../lib/format';
import { type RangeKey, RANGES, rangeDates } from '../../lib/trendRange';
import type { WbCreditsResult, WbCreditAccount, WbCreditPackage, WbCreditsTrendSnap } from '../../types';

/**
 * buddy-credits 积分看板（§3.7.4，F-20/F-22/F-56/F-57/F-58/F-25，对齐 Trae 积分看板）：
 * KPI 统计 + 积分趋势图（总/获得/消耗三线，快照差分落库）+ 账号积分明细排名 + 积分包到期日历。
 */
export default function BuddyCredits() {
  const pushToast = useAppStore((s) => s.pushToast);
  const isDark = useIsDark();
  const [result, setResult] = useState<WbCreditsResult | null>(null);
  // 积分趋势数据源（三线图）：wb_credits_history 每日快照（23:30 自动落盘）差分推导
  const [snapshots, setSnapshots] = useState<WbCreditsTrendSnap[]>([]);
  // 趋势空态原因（快照未建立 / 查询失败时展示引导；有数据即清空）
  const [trendEmptyReason, setTrendEmptyReason] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  // 趋势区间（对齐 Trae 积分看板：今日/近7天/近30天/本月/近一年）
  const [range, setRange] = useState<RangeKey>('7d');

  const refresh = useCallback(
    async (fresh = false) => {
      setLoading(true);
      try {
        const r = await withMinDelay(api.workbuddy.creditsFetch(undefined, fresh), 1000);
        setResult(r);
        if (fresh && r.stale) {
          pushToast('warn', '积分刷新失败，已回退展示历史缓存数据');
        } else if (fresh) {
          pushToast('success', '积分已刷新');
        }
      } catch (err) {
        pushToast('error', `积分查询失败：${String(err)}`);
      } finally {
        setLoading(false);
      }
      // 积分趋势数据源：wb_credits_history 每日快照（23:30 落库）差分推导，
      // 持久化于 SQLite；空态（status=empty）展示引导提示，失败展示原因
      api.workbuddy
        .creditsTrend()
        .then((r) => {
          if (r.status === 'empty') {
            setSnapshots([]);
            setTrendEmptyReason(r.reason);
            return;
          }
          setSnapshots(r.snapshots);
          setTrendEmptyReason(null);
        })
        .catch((e) => {
          setSnapshots([]);
          setTrendEmptyReason(String(e));
        });
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

  // 积分趋势（三线图，对齐 Trae 积分看板）：快照缺失的日期为 null，recharts 跳点不画，避免误导性 0 值
  const snapMap = new Map(snapshots.map((s) => [s.date, s]));
  const trend = rangeDates(range).map((date) => {
    const snap = snapMap.get(date);
    return {
      label:
        range === 'year'
          ? `${date.slice(0, 4)}/${+date.slice(5, 7)}/${+date.slice(8, 10)}`
          : `${+date.slice(5, 7)}/${+date.slice(8, 10)}`,
      total: snap?.total ?? null,
      earned: snap?.earned ?? null,
      consumed: snap?.consumed ?? null,
    };
  });
  const hasTrend = trend.some((d) => d.total != null || d.earned != null || d.consumed != null);
  const showDots = range === 'today' || range === '7d';

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 积分看板"
        desc="积分余额 · 积分明细 · 到期日历"
        actions={
          <button className="btn-outline" onClick={() => void refresh(true)} disabled={loading}>
            <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      {/* KPI 卡（对齐 Trae 积分看板：5 卡） */}
          <div className="grid grid-cols-2 gap-3 md:grid-cols-5">
            <StatCard
              label="可用积分总数"
              value={total.toFixed(2)}
              hint={result?.stale ? '历史缓存回退（本次查询失败）' : result?.cached ? '缓存数据（≥10 分钟）' : '实时数据'}
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

          {/* 积分趋势图（对齐 Trae 积分看板：总/获得/消耗三线；wb_credits_history 每日快照差分） */}
          <div className="mt-5 card p-5">
            <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
              <div className="flex flex-wrap items-center gap-3 text-xs text-slate-400">
                <h3 className="text-sm font-medium text-slate-900 dark:text-zinc-100">积分趋势图</h3>
                <span className="flex items-center gap-1">
                  <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#6366f1' }} />
                  积分总数
                </span>
                <span className="flex items-center gap-1">
                  <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#22c55e' }} />
                  获得积分
                </span>
                <span className="flex items-center gap-1">
                  <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#f59e0b' }} />
                  消耗积分
                </span>
                <span className="hidden sm:inline">每日积分快照（23:30 自动落盘）</span>
              </div>
              <div className="flex items-center gap-1">
                {RANGES.map((r) => (
                  <button
                    key={r.key}
                    onClick={() => setRange(r.key)}
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
            </div>
            {hasTrend ? (
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
                    <YAxis tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }} axisLine={false} tickLine={false} width={56} />
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
                      formatter={(v: number, name: string) => {
                        const labels: Record<string, string> = { total: '积分总数', earned: '获得积分', consumed: '消耗积分' };
                        return [normZero(v).toLocaleString('zh-CN', { maximumFractionDigits: 2 }), labels[name] ?? name];
                      }}
                    />
                    <Line type="monotone" dataKey="total" stroke="#6366f1" strokeWidth={2.5} dot={showDots ? { r: 3, fill: '#6366f1', strokeWidth: 0 } : false} activeDot={{ r: 5 }} connectNulls />
                    <Line type="monotone" dataKey="earned" stroke="#22c55e" strokeWidth={2} dot={showDots ? { r: 3, fill: '#22c55e', strokeWidth: 0 } : false} activeDot={{ r: 5 }} connectNulls />
                    <Line type="monotone" dataKey="consumed" stroke="#f59e0b" strokeWidth={3} dot={showDots ? { r: 3, fill: '#f59e0b', strokeWidth: 0 } : false} activeDot={{ r: 5 }} connectNulls />
                  </LineChart>
                </ResponsiveContainer>
              </div>
            ) : (
              <EmptyState
                icon={<TrendingUp size={28} />}
                title="暂无趋势数据"
                hint={trendEmptyReason ?? '每日积分快照生成后（23:30 自动落盘）这里会展示积分趋势。'}
              />
            )}
          </div>

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
                          {a.source === 'legacy' && (
                            <Badge
                              tone="slate"
                              title="新版计费三接口（summary/paid/free）本次未返回数据，已自动改用旧版聚合接口（v2 get-user-resource）兜底取数：余额 = 积分包剩余求和，数据可信。偶发多为网络抖动或接口短暂异常；若该账号持续出现，建议在日志页核查计费接口返回码。"
                            >
                              旧接口回退
                            </Badge>
                          )}
                          {a.source === 'local_quota' && (
                            <Badge
                              tone="slate"
                              title="云端计费接口全部失败，已探测本机桌面服务端口兜底取得余额（无积分包明细）。"
                            >
                              本地兜底
                            </Badge>
                          )}
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
              items={allPackages
                // 剩余积分为 0 的包（已用完）无到期提醒价值，过滤不展示
                .filter(({ pkg }) => pkg.remaining > 0)
                .map(({ acc, pkg }) => ({
                  key: `${acc.user_id}-${pkg.name}-${pkg.expire_ts ?? 0}`,
                  label: `${acc.name} · ${pkg.name}`,
                  kind: '积分包',
                  expire_ts: pkg.expire_ts,
                  note: `剩余 ${pkg.remaining.toFixed(2)} / ${(pkg.total ?? 0).toFixed(2)}`,
                }))}
              emptyHint="暂无剩余积分的积分包：待账号录入凭证并完成积分查询后展示到期时间。"
            />
          </div>
    </div>
  );
}
