import { useCallback, useEffect, useState } from 'react';
import { RefreshCw, Coins } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { StatCard, Badge, Progress, EmptyState } from '../../components/ui';
import ExpiryCalendar from '../../components/ExpiryCalendar';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type { WbCreditsResult, WbCreditAccount, WbCreditPackage } from '../../types';

/**
 * buddy-credits 积分与统计（§3.7.4，F-20/F-22/F-56）：
 * 批次1 = 余额 KPI + 逐账号余额/包明细 + 到期日历；官方用量/Token 统计 Tab 随批次 3 开放。
 */
export default function BuddyCredits() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [result, setResult] = useState<WbCreditsResult | null>(null);
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
      // eslint-disable-next-line react-hooks/exhaustive-deps
    },
    [],
  );

  useEffect(() => {
    void refresh(false);
  }, [refresh]);

  const accounts: WbCreditAccount[] = result?.accounts ?? [];
  const total = accounts.reduce((s, a) => s + (a.balance ?? 0), 0);
  const allPackages: { acc: WbCreditAccount; pkg: WbCreditPackage }[] = accounts.flatMap((acc) =>
    (acc.packages ?? []).map((pkg) => ({ acc, pkg })),
  );
  const soonCount = allPackages.filter((x) => x.pkg.expire_soon).length;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="WorkBuddy · 积分与统计"
        desc="积分余额 · 积分包明细 · 到期日历"
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
        <div className="card p-8 text-center">
          <p className="text-sm font-medium">Token 统计（F-57）</p>
          <p className="mt-1 text-xs text-slate-400">
            本地 token 统计（~/.workbuddy/projects 与 ~/.codebuddy/projects JSONL 解析、缓存命中率、双轴趋势、年度热力图）随批次 3 开放。
          </p>
        </div>
      ) : (
        <>
          {/* KPI 卡 */}
          <div className="grid grid-cols-2 gap-4 xl:grid-cols-4">
            <StatCard label="总剩余积分" value={total.toFixed(2)} hint={result?.cached ? '缓存数据（≥5 分钟）' : '实时数据'} tone="amber" />
            <StatCard label="账号数" value={String(accounts.length)} hint={`${accounts.filter((a) => a.ok).length} 个查询成功`} tone="brand" />
            <StatCard label="积分包总数" value={String(allPackages.length)} hint="paid + free 包合计" tone="violet" />
            <StatCard label="7 天内到期" value={String(soonCount)} hint={soonCount > 0 ? '见下方到期日历' : '暂无临期包'} tone={soonCount > 0 ? 'red' : 'green'} />
          </div>

          {/* 逐账号余额 */}
          <div className="mt-5 card p-4">
            <div className="mb-3 flex items-center gap-2">
              <Coins size={16} className="text-amber-500" />
              <span className="text-sm font-medium">逐账号余额</span>
              {result?.cached && <Badge tone="slate">缓存</Badge>}
            </div>
            {accounts.length === 0 ? (
              <EmptyState
                icon={<Coins size={22} />}
                title="暂无积分数据"
                hint="请先在「账号管理」导入账号（需已录入凭证）；查询失败时请检查客户端登录态。"
              />
            ) : (
              <div className="space-y-2">
                {accounts.map((a) => (
                  <div key={a.user_id} className="flex items-center gap-3 rounded-lg border border-slate-100 px-3 py-2.5 text-sm dark:border-zinc-800">
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2 font-medium">
                        {a.name}
                        {a.source === 'legacy' && <Badge tone="slate">旧接口回退</Badge>}
                      </div>
                      {!a.ok && <div className="text-xs text-rose-500">{a.message}</div>}
                    </div>
                    <span className="tabular-nums text-lg font-semibold">{a.balance != null ? a.balance.toFixed(2) : '—'}</span>
                    <span className="w-20 text-right text-xs text-slate-400">{a.packages?.length ?? 0} 个包</span>
                  </div>
                ))}
              </div>
            )}
          </div>

          {/* 到期日历（F-13） */}
          <div className="mt-4 card p-4">
            <div className="mb-3 text-sm font-medium">积分包到期日历</div>
            <ExpiryCalendar
              items={allPackages.map(({ acc, pkg }) => ({
                key: `${acc.user_id}-${pkg.name}-${pkg.expire_ts ?? 0}`,
                label: `${acc.name} · ${pkg.name}`,
                kind: '积分包',
                expire_ts: pkg.expire_ts,
                note: `剩余 ${pkg.remaining.toFixed(2)} / ${(pkg.total ?? 0).toFixed(2)}`,
              }))}
            />
          </div>
        </>
      )}
    </div>
  );
}
