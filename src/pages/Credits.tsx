import { useMemo, useState, useCallback, useEffect } from 'react';
import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  ResponsiveContainer,
  Tooltip,
  CartesianGrid,
} from 'recharts';
import { Coins, RefreshCw } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { StatCard, EmptyState } from '../components/ui';
import ExpiryCalendar, { type ExpiryItem } from '../components/ExpiryCalendar';
import { useAppStore } from '../store';
import { useIsDark } from '../lib/useIsDark';
import { fmtCredits, normZero } from '../lib/format';
import { localDate, type RangeKey, RANGES, rangeDates } from '../lib/trendRange';

export default function Credits() {
  const accounts = useAppStore((s) => s.accounts);
  const creditsDaily = useAppStore((s) => s.creditsDaily);
  const isDark = useIsDark();
  const refreshRemainingCredits = useAppStore((s) => s.refreshRemainingCredits);
  const refreshAccounts = useAppStore((s) => s.refreshAccounts);
  const refreshCreditsDaily = useAppStore((s) => s.refreshCreditsDaily);
  const pushToast = useAppStore((s) => s.pushToast);
  const [refreshing, setRefreshing] = useState(false);

  // 趋势区间（快照数据按日取值）
  const [range, setRange] = useState<RangeKey>('7d');

  const handleRefresh = useCallback(async () => {
    setRefreshing(true);
    try {
      // 1. 刷新所有账号剩余积分（后端会更新 credits_daily.json 快照）
      await refreshRemainingCredits();
      // 2. 重新加载账号列表（remaining_credits 字段）
      await refreshAccounts();
      // 3. 重新加载每日积分快照
      await refreshCreditsDaily();
      pushToast('success', '积分数据已刷新');
    } catch (err) {
      pushToast('error', `刷新失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
  }, [refreshRemainingCredits, refreshAccounts, refreshCreditsDaily, pushToast]);

  const rows = useMemo(
    () =>
      [...accounts].sort((a, b) => {
        const va = a.remaining_credits;
        const vb = b.remaining_credits;
        // null 排到最后
        if (va == null && vb == null) return 0;
        if (va == null) return 1;
        if (vb == null) return -1;
        return vb - va; // 降序
      }),
    [accounts],
  );
  const total = rows.reduce((s, a) => s + (a.remaining_credits ?? 0), 0);
  const avg = rows.length === 0 ? 0 : Math.round(total / rows.length);
  const generalTotal = rows.reduce((s, a) => s + (a.general_credits ?? 0), 0);
  const workTotal = rows.reduce((s, a) => s + (a.work_credits ?? 0), 0);
  const totalHint = accounts.some((a) => a.general_credits != null || a.work_credits != null)
    ? `通用 ${fmtCredits(generalTotal)} 积分 · Work ${fmtCredits(workTotal)} 积分`
    : '总剩余可用积分';

  const today = localDate(new Date());

  // 今日新增积分：每日快照 earned 字段（积分包 CycleStartTime 归日口径，含签到包与购买包）
  const todayNew = useMemo(() => {
    const snap = creditsDaily.find((s) => s.date === today);
    return snap && snap.earned > 0 ? Math.round(snap.earned) : 0;
  }, [creditsDaily, today]);

  // 今日消耗积分：余额快照口径
  const todayConsumed = useMemo(() => {
    const snap = creditsDaily.find((s) => s.date === today);
    return snap ? snap.consumed : 0;
  }, [creditsDaily, today]);

  // ---- 统计卡（积分总数 / 获得总积分 / 消耗总积分，快照口径） ----
  const allTimeConsumed = useMemo(
    () => creditsDaily.reduce((s, v) => s + v.consumed, 0),
    [creditsDaily],
  );
  // 区间聚合（快照数据）
  const rangeAgg = useMemo(() => {
    const dates = new Set(rangeDates(range));
    let consumed = 0;
    let earned = 0;
    for (const date of dates) {
      const snap = creditsDaily.find((s) => s.date === date);
      if (snap) {
        consumed += snap.consumed;
        earned += snap.earned;
      }
    }
    return { consumed, earned };
  }, [range, creditsDaily]);

  // 趋势数据（余额快照口径：快照缺失的日期为 null，recharts 跳点不画，避免误导性 0 值）
  const trend = useMemo(() => {
    const snapMap = new Map(creditsDaily.map((s) => [s.date, s]));
    return rangeDates(range).map((date) => {
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
  }, [range, creditsDaily]);

  const hasTrend = trend.some(
    (d) => d.total != null || d.earned != null || d.consumed != null,
  );
  const showDots = range === 'today' || range === '7d';

  // 到期日历（F-13 批次 2 补挂 Trae 侧）：积分包 + 会员两类，均 Unix 秒。
  // JWT token（登录凭证）到期不纳入日历——凭证到期与积分无关。
  // 「剩余 X / 总 Y」：X = 账号当前可用积分，Y = 本周期积分包 credits_limit 合计。
  const expiryItems = useMemo<ExpiryItem[]>(
    () =>
      accounts.flatMap((a) => {
        const items: ExpiryItem[] = [];
        if (a.credits_expire_at != null) {
          const totalTxt = a.total_credits != null ? ` / 总 ${fmtCredits(a.total_credits)}` : '';
          items.push({
            key: `${a.user_id}-credits`,
            label: a.name,
            kind: '积分包',
            expire_ts: a.credits_expire_at,
            note: `剩余 ${fmtCredits(a.remaining_credits ?? 0)}${totalTxt} 积分`,
          });
        }
        if (a.membership_expire != null) {
          items.push({
            key: `${a.user_id}-membership`,
            label: a.name,
            kind: '会员',
            expire_ts: a.membership_expire,
            note: a.pay_identity ? `套餐 ${a.pay_identity}` : null,
          });
        }
        return items;
      }),
    [accounts],
  );

  return (
    <div className="animate-fade-in">
      <div className="flex items-center justify-between">
        <PageHeader
          title="Trae · 积分看板"
          desc="查看每个账号的积分余额与趋势"
        />
        <button
          className="btn-ghost flex items-center gap-1.5 text-sm"
          onClick={handleRefresh}
          disabled={refreshing}
          title={refreshing ? '刷新中…' : '刷新数据'}
        >
          <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} />
          {refreshing ? '刷新中' : '刷新'}
        </button>
      </div>

      <div className="mb-5 grid grid-cols-2 gap-3 md:grid-cols-5">
        <StatCard label="可用总积分" value={fmtCredits(total)} hint={totalHint} tone="violet" />
        <StatCard label="账号数" value={rows.length} tone="brand" />
        <StatCard label="平均可用积分" value={normZero(avg).toLocaleString()} tone="blue" />
        <StatCard label="今日新增积分" value={normZero(todayNew).toLocaleString()} tone="green" hint={today} />
        <StatCard
          label="今日消耗积分"
          value={normZero(todayConsumed).toLocaleString('zh-CN', { maximumFractionDigits: 2 })}
          tone="amber"
          hint={today}
        />
      </div>

      <div className="card p-5">
        {/* 头部：标题 | 区间 */}
        <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="font-medium">积分统计</h3>
            <span className="text-xs text-slate-400">每日积分快照（23:30 / 23:40 自动落盘）</span>
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

        {/* 统计卡：积分总数（消耗+目前可用）/ 获得总积分 / 消耗总积分 */}
        <div className="mb-5 grid grid-cols-1 gap-3 md:grid-cols-3">
          <StatCard
            label="积分总数（消耗+目前可用）"
            value={normZero(Math.round(allTimeConsumed + total)).toLocaleString()}
            hint={`消耗 ${fmtCredits(allTimeConsumed)} + 目前可用 ${fmtCredits(total)}`}
            tone="brand"
          />
          <StatCard
            label="获得总积分"
            value={normZero(Math.round(rangeAgg.earned)).toLocaleString()}
            hint="所选区间内获得（积分包 CycleStartTime 口径）"
            tone="green"
          />
          <StatCard
            label="消耗总积分"
            value={normZero(rangeAgg.consumed).toLocaleString('zh-CN', { maximumFractionDigits: 2 })}
            hint="所选区间 · 余额快照口径"
            tone="amber"
          />
        </div>

        {/* 积分趋势图 */}
        <div className="mb-2 flex items-center gap-3">
          <h4 className="text-sm font-medium">积分趋势图</h4>
          <div className="flex items-center gap-3 text-xs text-slate-400">
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
          </div>
        </div>
        {accounts.length === 0 ? (
          <EmptyState icon={<Coins size={28} />} title="尚无账号数据" hint="添加账号后这里会展示积分趋势。" />
        ) : !hasTrend ? (
          <EmptyState
            icon={<Coins size={28} />}
            title="暂无趋势数据"
            hint="每日积分快照生成后（23:30 / 23:40）这里会展示余额趋势。"
          />
        ) : (
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
        )}
      </div>

      {/* 积分到期日历（F-13） */}
      <div className="mt-5 card p-4">
        <h3 className="mb-3 font-medium">积分到期日历</h3>
        <ExpiryCalendar items={expiryItems} emptyHint="暂无到期项：待账号完成签到/积分查询后展示积分包与会员到期时间。" />
      </div>
    </div>
  );
}
