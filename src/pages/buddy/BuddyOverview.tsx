import { useEffect, useMemo, useState } from 'react';
import { RefreshCw, CheckCircle2, Circle, ChevronRight } from 'lucide-react';
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
  Legend,
} from 'recharts';
import PageHeader from '../../components/PageHeader';
import { StatCard, Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { useIsDark } from '../../lib/useIsDark';
import type {
  ViewKey,
  WorkBuddyAccountView,
  WbCreditsResult,
  WbCheckinRecord,
} from '../../types';

/** 概述页近 30 天签到趋势数据点（由 WbCheckinRecord 按日聚合） */
interface TrendPoint {
  date: string;
  ok: number;
  already: number;
  failed: number;
}

/** 配置导航步骤（optional 步骤不计入完成度） */
interface Step {
  key: string;
  title: string;
  desc: string;
  done: boolean;
  actionLabel: string;
  view: ViewKey;
  /** 可选步骤：不计入 x/y 完成度 */
  optional?: boolean;
}

const statusText: Record<string, string> = {
  success: '成功',
  already: '已签',
  fail: '失败',
};

function aggregateTrends(records: WbCheckinRecord[]): TrendPoint[] {
  const map = new Map<string, TrendPoint>();
  const seen = new Set<string>(); // 同日同账号去重（手动+定时多轮签到不重复计数）
  for (const r of records) {
    if (!r.date) continue;
    const key = `${r.date}|${r.user_id || r.name}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const p = map.get(r.date) ?? { date: r.date, ok: 0, already: 0, failed: 0 };
    if (r.status === 'success') p.ok += 1;
    else if (r.status === 'already') p.already += 1;
    else p.failed += 1;
    map.set(r.date, p);
  }
  return [...map.values()].sort((a, b) => a.date.localeCompare(b.date));
}

/** WorkBuddy 概述页：顶部统计卡 + 近 30 天签到结果 + 积分榜 Top + 配置导航（对齐 Trae 概述结构） */
export default function BuddyOverview() {
  const pushToast = useAppStore((s) => s.pushToast);
  const setView = useAppStore((s) => s.setView);
  const isDark = useIsDark();
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [credits, setCredits] = useState<WbCreditsResult | null>(null);
  const [records, setRecords] = useState<WbCheckinRecord[]>([]);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = async () => {
    setRefreshing(true);
    try {
      const [accs, cr, recs] = await Promise.all([
        api.workbuddy.accountsList().catch(() => [] as WorkBuddyAccountView[]),
        api.workbuddy.creditsFetch().catch(() => null),
        api.workbuddy.checkinResults(30).catch(() => [] as WbCheckinRecord[]),
      ]);
      setAccounts(accs);
      setCredits(cr);
      setRecords(recs);
    } catch (err) {
      pushToast('error', `WorkBuddy 概述刷新失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
  };

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const total = accounts.length;
  const totalBalance = credits?.accounts.reduce((s, a) => s + (a.balance ?? 0), 0) ?? null;
  const okAccounts = credits?.accounts.filter((a) => a.ok).length ?? 0;
  // 今日签到账号数（今日记录去重 user_id）
  const today = new Date().toLocaleDateString('sv-SE');
  const checkedToday = useMemo(
    () => new Set(records.filter((r) => r.date === today && r.status !== 'fail').map((r) => r.user_id)).size,
    [records, today],
  );

  const trends = useMemo(() => aggregateTrends(records), [records]);

  // 登录账号 / 本机套餐：账号列表中 is_current 标记的当前生效登录（服务端 auth 文件在线判定）
  const wbAccount = accounts.find((a) => a.is_current) ?? null;
  const wbLoginName = wbAccount?.nickname ?? null;
  const wbPlan = wbAccount?.edition_type ?? null;
  const cbAccount = accounts.find((a) => a.is_current_codebuddy) ?? null;
  const cbLoginName = cbAccount?.nickname ?? null;
  const cbPlan = cbAccount?.edition_type ?? null;
  // 告警提醒：Token 24h 内将过期（含已过期）账号数 + 积分包 7 日内将过期包数（仍有剩余）
  const nowSec = Math.floor(Date.now() / 1000);
  const tokenSoon = accounts.filter(
    (a) => a.access_token_expires_at != null && a.access_token_expires_at <= nowSec + 86400,
  ).length;
  const pkgSoon = (credits?.accounts ?? []).reduce(
    (n, acc) =>
      n +
      (acc.packages ?? []).filter(
        (p) => p.expire_ts != null && p.expire_ts <= nowSec + 7 * 86400 && p.remaining > 0,
      ).length,
    0,
  );
  const alertCount = tokenSoon + pkgSoon;

  // 积分榜 Top（按余额降序，参考 Trae 概述）
  const top = useMemo(
    () =>
      [...(credits?.accounts ?? [])]
        .filter((a) => a.balance != null && a.balance > 0)
        .sort((a, b) => (b.balance ?? 0) - (a.balance ?? 0))
        .slice(0, 10)
        .map((a) => ({ name: a.name || a.user_id, credits: a.balance as number })),
    [credits],
  );

  // 配置导航（步骤完成态实时判定）
  const steps: Step[] = [
    {
      key: 'account',
      title: '录入账号',
      desc: '通过 OAuth 登录或导入账号库的方式将账号入池。',
      done: accounts.length > 0,
      actionLabel: '去录入',
      view: 'buddy-accounts',
    },
    {
      key: 'checkin',
      title: '完成首次签到',
      desc: '验证签到链路是否跑通（凭证有效、接口可达）。',
      done: records.some((r) => r.date === today),
      actionLabel: '去签到',
      view: 'buddy-checkin',
    },
    {
      key: 'credits',
      title: '查询积分余额',
      desc: '录入凭证后查询各账号积分包余额与到期情况。',
      done: (credits?.accounts.length ?? 0) > 0,
      actionLabel: '查看积分',
      view: 'buddy-credits',
    },
  ];
  // 可选步骤不计入完成度
  const required = steps.filter((s) => !s.optional);
  const completed = required.filter((s) => s.done).length;
  const allDone = completed === required.length;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 概述"
        desc="WorkBuddy / CodeBuddy 运行总览 · 登录账号 / 套餐 / 告警提醒 · 签到趋势与积分榜"
        actions={
          <button onClick={() => void refresh()} className="btn-outline" disabled={refreshing}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      {/* 顶部统计卡（对齐 Trae 概述：数值一眼掌握） */}
      <div className="grid grid-cols-2 gap-3 md:grid-cols-5">
        <StatCard
          label="账号总数"
          value={total}
          hint={`今日已签 ${checkedToday}`}
          tone="brand"
        />
        <StatCard
          label="可用总积分"
          value={totalBalance != null ? totalBalance.toFixed(2) : '—'}
          hint={credits ? `${okAccounts}/${credits.accounts.length} 个查询成功` : '录入凭证后自动查询'}
          tone="amber"
        />
        <StatCard
          label="登录账号"
          value={wbLoginName ?? cbLoginName ?? '未登录'}
          hint={
            [`WorkBuddy：${wbLoginName ?? '未登录'}`, `CodeBuddy：${cbLoginName ?? '未登录'}`].join(' · ')
          }
          tone="violet"
        />
        <StatCard
          label="本机套餐"
          value={wbPlan ?? cbPlan ?? '—'}
          hint={[`WorkBuddy：${wbPlan ?? '—'}`, `CodeBuddy：${cbPlan ?? '—'}`].join(' · ')}
          tone="violet"
        />
        <StatCard
          label="告警提醒"
          value={alertCount}
          hint={`Token 24h 内过期 ${tokenSoon} · 积分包 7 日内过期 ${pkgSoon}`}
          tone={alertCount > 0 ? 'red' : 'slate'}
        />
      </div>

      {/* 近 30 天签到结果（从签到页移入，无数据显示空态） */}
      <div className="mt-5 card p-5">
        <div className="mb-4 flex items-center justify-between">
          <h3 className="font-medium">近 30 天签到结果</h3>
          <span className="text-xs text-slate-400">按日汇总 · 成功 / 已签 / 失败</span>
        </div>
        {trends.length === 0 ? (
          <div className="flex h-40 items-center justify-center text-sm text-slate-400">
            暂无签到记录，完成一次签到后这里会显示趋势。
          </div>
        ) : (
          <div className="h-64">
            <ResponsiveContainer>
              <BarChart data={trends} margin={{ top: 8, right: 16, left: 0, bottom: 4 }} barCategoryGap="24%">
                <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                <XAxis
                  dataKey="date"
                  tickFormatter={(v: string) => v.slice(5)}
                  tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }}
                  axisLine={{ stroke: isDark ? '#3f3f46' : '#e2e8f0' }}
                  tickLine={false}
                />
                <YAxis allowDecimals={false} tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }} axisLine={false} tickLine={false} width={36} />
                <Tooltip
                  cursor={{ fill: isDark ? 'rgba(255,255,255,0.05)' : 'rgba(0,0,0,0.03)' }}
                  contentStyle={{
                    fontSize: 12,
                    borderRadius: 10,
                    border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
                    background: isDark ? '#18181b' : '#fff',
                    color: isDark ? '#e4e4e7' : '#1e293b',
                    boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
                    padding: '8px 12px',
                  }}
                />
                <Legend wrapperStyle={{ fontSize: 12 }} />
                <Bar dataKey="ok" name="成功" stackId="trend" fill="#10b981" maxBarSize={28} />
                <Bar dataKey="already" name="已签" stackId="trend" fill="#0ea5e9" maxBarSize={28} />
                <Bar dataKey="failed" name="失败" stackId="trend" fill="#f43f5e" maxBarSize={28} radius={[4, 4, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>

      {/* 积分榜 Top 榜（对齐 Trae 概述；无数据显示空态引导，不再整卡隐藏） */}
      <div className="mt-5 card p-5">
        <div className="mb-4 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <h3 className="font-medium">积分榜 Top 榜</h3>
            {top.length > 0 && (
              <span className="rounded-full bg-zinc-100 px-2 py-0.5 text-xs font-medium text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                Top {top.length}
              </span>
            )}
          </div>
          <span className="text-xs text-slate-400">按可用积分排序</span>
        </div>
        {top.length === 0 ? (
          <div className="flex h-40 items-center justify-center text-sm text-slate-400">
            暂无积分数据，导入账号并查询积分后展示 Top 榜。
          </div>
        ) : (
          <div className="h-72">
            <ResponsiveContainer>
              <BarChart data={top} margin={{ top: 24, right: 16, left: 0, bottom: 4 }} barCategoryGap="36%">
                <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                <XAxis
                  dataKey="name"
                  tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }}
                  interval={0}
                  angle={-20}
                  textAnchor="end"
                  height={52}
                  axisLine={{ stroke: isDark ? '#3f3f46' : '#e2e8f0' }}
                  tickLine={false}
                />
                <YAxis tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }} axisLine={false} tickLine={false} width={48} />
                <Tooltip
                  cursor={{ fill: isDark ? 'rgba(255,255,255,0.05)' : 'rgba(0,0,0,0.03)' }}
                  contentStyle={{
                    fontSize: 12,
                    borderRadius: 10,
                    border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
                    background: isDark ? '#18181b' : '#fff',
                    color: isDark ? '#e4e4e7' : '#1e293b',
                    boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
                    padding: '8px 12px',
                  }}
                  formatter={(v: number) => [v.toFixed(2), '可用积分']}
                />
                <Bar dataKey="credits" radius={[8, 8, 0, 0]} maxBarSize={44}>
                  {top.map((_, i) => {
                    const colors = isDark
                      ? ['#fafafa', '#e4e4e7', '#d4d4d8']
                      : ['#27272a', '#3f3f46', '#52525b'];
                    const fill = i < 3 ? colors[i] : isDark
                      ? `rgba(212,212,216,${Math.max(0.35, 0.6 - (i - 3) * 0.05).toFixed(2)})`
                      : `rgba(82,82,91,${Math.max(0.35, 0.6 - (i - 3) * 0.05).toFixed(2)})`;
                    return <Cell key={i} fill={fill} />;
                  })}
                  <LabelList
                    dataKey="credits"
                    position="top"
                    formatter={(v: number) => (v >= 1000 ? `${(v / 1000).toFixed(1)}k` : v.toFixed(0))}
                    style={{ fontSize: 10, fill: isDark ? '#a1a1aa' : '#94a3b8', fontWeight: 500 }}
                  />
                </Bar>
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>

      {/* 组件配置导航（对齐 Trae 概述 SetupGuide 形态） */}
      <div className="mt-5 card overflow-hidden">
        <div className="flex items-center justify-between border-b border-slate-100 px-4 py-3 dark:border-zinc-800">
          <div>
            <h3 className="font-medium">配置导航</h3>
            <p className="text-xs text-slate-500">按步骤完成初始化，已完成的步骤无需重复处理。</p>
          </div>
          <Badge tone={allDone ? 'green' : 'amber'}>
            {completed}/{required.length} 已完成
          </Badge>
        </div>
        <ol className="divide-y divide-slate-100 dark:divide-slate-800">
          {steps.map((step, i) => (
            <li key={step.key} className="flex items-center gap-3 px-4 py-3">
              <div className={step.done ? 'text-emerald-500' : 'text-slate-300 dark:text-zinc-600'}>
                {step.done ? <CheckCircle2 size={20} /> : <Circle size={20} />}
              </div>
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium text-slate-800 dark:text-zinc-100">
                  {i + 1}. {step.title}
                </div>
                <div className="text-xs text-slate-500">{step.desc}</div>
              </div>
              {step.done ? (
                <span className="shrink-0 rounded-full bg-emerald-50 px-2.5 py-1 text-xs font-medium text-emerald-600 dark:bg-emerald-500/15 dark:text-emerald-400">
                  已完成
                </span>
              ) : step.optional ? (
                <span className="shrink-0 rounded-full bg-zinc-100 px-2.5 py-1 text-xs font-medium text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                  可选
                </span>
              ) : (
                <button onClick={() => setView(step.view)} className="btn-outline shrink-0">
                  {step.actionLabel}
                  <ChevronRight size={14} />
                </button>
              )}
            </li>
          ))}
        </ol>
        {allDone && (
          <div className="border-t border-slate-100 bg-emerald-50/60 px-4 py-3 text-sm text-emerald-700 dark:border-zinc-800 dark:bg-emerald-500/10 dark:text-emerald-300">
            🎉 全部配置已完成，定时签到/积分轮换交给自动化即可！
          </div>
        )}
      </div>
    </div>
  );
}
