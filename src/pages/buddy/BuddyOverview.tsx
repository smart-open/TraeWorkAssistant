import { useEffect, useMemo, useState } from 'react';
import { RefreshCw, ExternalLink, CheckCircle2, Circle, ChevronRight, Coins } from 'lucide-react';
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
import { open } from '@tauri-apps/plugin-shell';
import PageHeader from '../../components/PageHeader';
import { StatCard, Badge, EmptyState } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { useIsDark } from '../../lib/useIsDark';
import type {
  ViewKey,
  WorkBuddyEnvCheck,
  WorkBuddyAccountView,
  WbCreditsResult,
  WbActivityInfo,
  WbCheckinRecord,
} from '../../types';

/** 概述页近 30 天签到趋势数据点（由 WbCheckinRecord 按日聚合） */
interface TrendPoint {
  date: string;
  ok: number;
  already: number;
  failed: number;
}

/** 配置导航步骤（对齐 Trae 概述 SetupGuide 的形态） */
interface Step {
  key: string;
  title: string;
  desc: string;
  done: boolean;
  actionLabel: string;
  view: ViewKey;
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
  const [env, setEnv] = useState<WorkBuddyEnvCheck | null>(null);
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [credits, setCredits] = useState<WbCreditsResult | null>(null);
  const [activity, setActivity] = useState<WbActivityInfo | null>(null);
  const [records, setRecords] = useState<WbCheckinRecord[]>([]);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = async () => {
    setRefreshing(true);
    try {
      const [e, accs, cr, recs] = await Promise.all([
        api.workbuddy.envCheck().catch(() => null),
        api.workbuddy.accountsList().catch(() => [] as WorkBuddyAccountView[]),
        api.workbuddy.creditsFetch().catch(() => null),
        api.workbuddy.checkinResults(30).catch(() => [] as WbCheckinRecord[]),
      ]);
      setEnv(e);
      setAccounts(accs);
      setCredits(cr);
      setRecords(recs);
      // 活动信息（低频附加展示，失败静默）
      api.workbuddy
        .activityInfo()
        .then(setActivity)
        .catch(() => setActivity(null));
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
  const pkgCount = credits?.accounts.reduce((s, a) => s + (a.packages?.length ?? 0), 0) ?? 0;
  // 今日签到账号数（今日记录去重 user_id）
  const today = new Date().toLocaleDateString('sv-SE');
  const checkedToday = useMemo(
    () => new Set(records.filter((r) => r.date === today && r.status !== 'fail').map((r) => r.user_id)).size,
    [records, today],
  );

  const trends = useMemo(() => aggregateTrends(records), [records]);

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
      key: 'client',
      title: '安装 WorkBuddy 客户端',
      desc: '签到/积分目标客户端，需先安装并登录至少一个账号。',
      done: !!env?.installed,
      actionLabel: env?.installed ? '打开客户端' : '前往下载',
      view: 'buddy-settings',
    },
    {
      key: 'account',
      title: '导入本机账号',
      desc: '在客户端登录后到「账号管理」导入本机账号，或用 OAuth 扫码入池。',
      done: accounts.length > 0,
      actionLabel: '去导入',
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
  const completed = steps.filter((s) => s.done).length;
  const allDone = completed === steps.length;

  const openClient = () => {
    const exe = env?.exe;
    if (exe) {
      void open(`file:///${exe}`).catch((e) => pushToast('error', `打开客户端失败：${String(e)}`));
    } else {
      pushToast('warn', '未检测到 WorkBuddy 客户端，请先安装');
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 概述"
        desc="WorkBuddy / CodeBuddy 多账号管理 · 切换 / 续期 / 签到 / 积分"
        actions={
          <>
            <button className="btn-outline" onClick={openClient}>
              <ExternalLink size={15} /> 打开客户端
            </button>
            <button onClick={() => void refresh()} className="btn-outline" disabled={refreshing}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
            </button>
          </>
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
          label="积分包"
          value={String(pkgCount)}
          hint={pkgCount > 0 ? '明细见积分看板' : '暂无积分包数据'}
          tone="violet"
        />
        <StatCard
          label="客户端"
          value={env?.running ? '运行中' : env?.installed ? '已停止' : '未检测到'}
          hint={env?.version ? `v${env.version}` : '环境详情见环境配置'}
          tone={env?.running ? 'green' : env?.installed ? 'slate' : 'amber'}
        />
        <StatCard
          label="今日签到"
          value={checkedToday > 0 ? '已完成' : '未完成'}
          hint={checkedToday > 0 ? `今日 ${checkedToday} 个账号已签` : '可到签到页立即执行'}
          tone={checkedToday > 0 ? 'green' : 'slate'}
        />
      </div>

      {/* 活动信息卡（低频附加展示） */}
      {activity && (activity.banners.length > 0 || activity.payment_type || activity.dosage_notify) && (
        <div className="mt-4 card p-4">
          <div className="mb-2 flex items-center justify-between">
            <span className="text-sm font-medium">活动信息</span>
            <div className="flex items-center gap-2">
              {activity.payment_type && <Badge tone="violet">{activity.payment_type}</Badge>}
              {activity.errors.length > 0 && (
                <span className="text-xs text-slate-400">部分数据源不可用</span>
              )}
            </div>
          </div>
          {activity.banners.length > 0 && (
            <div className="flex gap-2 overflow-x-auto pb-1">
              {activity.banners.map((b, i) => (
                <div
                  key={`${b.title}-${i}`}
                  className="min-w-56 max-w-80 shrink-0 rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800"
                >
                  <div className="font-medium text-slate-700 dark:text-zinc-200">{b.title || '活动'}</div>
                  {b.content && <div className="mt-1 line-clamp-2 text-slate-500 dark:text-zinc-400">{b.content}</div>}
                  {b.url && (
                    <button
                      className="mt-1 flex items-center gap-1 text-brand-600 hover:underline dark:text-brand-400"
                      onClick={() => void open(b.url).catch(() => pushToast('warn', '链接无法打开'))}
                    >
                      查看详情 <ExternalLink size={11} />
                    </button>
                  )}
                </div>
              ))}
            </div>
          )}
          {activity.dosage_notify != null && Object.keys(activity.dosage_notify).length > 0 && (
            <div className="mt-2 rounded-lg bg-amber-50 px-3 py-2 text-xs text-amber-700 dark:bg-amber-500/10 dark:text-amber-400">
              用量提醒：{Object.entries(activity.dosage_notify)
                .filter(([, v]) => v != null && v !== '')
                .slice(0, 4)
                .map(([k, v]) => `${k}=${String(v)}`)
                .join(' · ')}
            </div>
          )}
        </div>
      )}

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

      {/* 积分榜 Top 榜（对齐 Trae 概述） */}
      {top.length > 0 && (
        <div className="mt-5 card p-5">
          <div className="mb-4 flex items-center justify-between">
            <div className="flex items-center gap-2">
              <h3 className="font-medium">积分榜 Top 榜</h3>
              <span className="rounded-full bg-zinc-100 px-2 py-0.5 text-xs font-medium text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                Top {top.length}
              </span>
            </div>
            <span className="text-xs text-slate-400">按可用积分排序</span>
          </div>
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
        </div>
      )}

      {/* 组件配置导航（对齐 Trae 概述 SetupGuide 形态） */}
      <div className="mt-5 card overflow-hidden">
        <div className="flex items-center justify-between border-b border-slate-100 px-4 py-3 dark:border-zinc-800">
          <div>
            <h3 className="font-medium">配置导航</h3>
            <p className="text-xs text-slate-500">按步骤完成初始化，已完成的步骤无需重复处理。</p>
          </div>
          <Badge tone={allDone ? 'green' : 'amber'}>
            {completed}/{steps.length} 已完成
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
              ) : (
                <button
                  onClick={() => (step.key === 'client' ? openClient() : setView(step.view))}
                  className="btn-primary shrink-0"
                >
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

      {accounts.length === 0 && (
        <div className="mt-5">
          <EmptyState
            icon={<Coins size={26} />}
            title="暂无 WorkBuddy 账号"
            hint="先在 WorkBuddy 客户端登录，然后到「账号管理」导入本机账号或 OAuth 扫码入池。"
          />
        </div>
      )}
    </div>
  );
}
