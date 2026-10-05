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
import { dateStrToEndTs } from '../../lib/format';
import { useAppStore } from '../../store';
import { useIsDark } from '../../lib/useIsDark';
import type {
  ViewKey,
  QoderAccountView,
  QoderCheckinRecord,
  QoderCreditsResult,
} from '../../types';

/**
 * qoder-overview 概述（F-80 §5.8，对照 BuddyOverview 结构逐块对齐；docker 版裁剪）：
 * 顶部统计卡 + 近 30 天签到结果（堆叠柱状）+ 积分榜 Top 榜 + 配置导航。
 * 差异：无成长中心（Qoder 无对应玩法）；桌面版的 CLI 状态桥 / 环境检测 / 打开客户端
 * 为 OS 专属能力，docker 版不含——登录账号卡与本机套餐卡随数据源一并移除。
 */

/** 概述页近 30 天签到趋势数据点（由 QoderCheckinRecord 按日聚合） */
interface TrendPoint {
  date: string;
  ok: number;
  already: number;
  failed: number;
}

/** 配置导航步骤（对齐 Buddy 概述 SetupGuide 形态；optional 步骤不计入完成度） */
interface Step {
  key: string;
  title: string;
  desc: string;
  done: boolean;
  actionLabel: string;
  view: ViewKey;
  optional?: boolean;
}

function aggregateTrends(records: QoderCheckinRecord[]): TrendPoint[] {
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
    else if (r.status === 'fail') p.failed += 1;
    // 其余状态（skip 等）不计入任何桶——未知状态误计为失败会污染趋势图
    map.set(r.date, p);
  }
  return [...map.values()].sort((a, b) => a.date.localeCompare(b.date));
}

export default function QoderOverview() {
  const pushToast = useAppStore((s) => s.pushToast);
  const setView = useAppStore((s) => s.setView);
  const isDark = useIsDark();
  const [accounts, setAccounts] = useState<QoderAccountView[]>([]);
  const [credits, setCredits] = useState<QoderCreditsResult | null>(null);
  const [records, setRecords] = useState<QoderCheckinRecord[]>([]);
  const [refreshing, setRefreshing] = useState(false);

  // fresh=false 初载走后端缓存（口径对齐 Dashboard/BuddyOverview）；仅顶部「刷新」按钮传 true 强制拉全账号
  const refresh = async (fresh: boolean) => {
    setRefreshing(true);
    try {
      // creditsFetch 失败不拖垮整页（原裸 await 使单路 reject 连带其余三路 setState
      // 全部跳过），同时保留 QoderCredits 页 I18 意图「不内联吞错」：ok/err 包裹后
      // 失败显式 toast 并保留旧数据；其余两路为本地读库，降级不阻塞
      const [accs, crRes, recs] = await Promise.all([
        api.qoder.accountsList().catch(() => [] as QoderAccountView[]),
        api.qoder.creditsFetch(undefined, fresh).then(
          (cr) => ({ ok: true as const, cr }),
          (err: unknown) => ({ ok: false as const, err }),
        ),
        api.qoder.checkinResults(30).catch(() => [] as QoderCheckinRecord[]),
      ]);
      setAccounts(accs);
      if (crRes.ok) {
        setCredits(crRes.cr);
      } else {
        pushToast('error', `积分查询失败（展示缓存数据）：${String(crRes.err)}`);
      }
      setRecords(recs);
    } catch (err) {
      pushToast('error', `Qoder 概述刷新失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
  };

  useEffect(() => {
    void refresh(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const total = accounts.length;
  // 空账号列表显示空态（—）而非误导性的 0.00：未导入账号 ≠ 零积分
  const totalBalance =
    credits && credits.accounts.length > 0
      ? credits.accounts.reduce((s, a) => s + (a.total ?? 0), 0)
      : null;
  const okAccounts = credits?.accounts.filter((a) => a.ok).length ?? 0;
  // 今日签到账号数（今日记录去重 user_id）
  const today = new Date().toLocaleDateString('sv-SE');
  const checkedToday = useMemo(
    () => new Set(records.filter((r) => r.date === today && r.status !== 'fail').map((r) => r.user_id)).size,
    [records, today],
  );

  const trends = useMemo(() => aggregateTrends(records), [records]);

  // 告警提醒：Token 24h 内将过期（含已过期）账号数 + 需重新登录账号数
  const nowSec = Math.floor(Date.now() / 1000);
  const tokenSoonSet = new Set(
    accounts
      .filter((a) => a.token_expires_at != null && a.token_expires_at <= nowSec + 86400)
      .map((a) => a.id),
  );
  const reloginSet = new Set(accounts.filter((a) => a.needs_relogin).map((a) => a.id));
  // 凭证类告警并集去重：同一账号可能既 token 临期又需重登，相加会重复计数
  const credAlertSet = new Set([...tokenSoonSet, ...reloginSet]);
  // 积分 7 天内到期账号数（口径对齐到期日历 ExpiryTab：剩余未知或 > 0 的包计入，
  // 无 plan 包明细回退 plan_expires_at 兜底；plan 包随订阅周期重置同样计入）
  const windowEnd = nowSec + 7 * 86400;
  const creditsSoonSet = new Set(
    (credits?.accounts ?? [])
      .filter((a) => {
        if (!a.ok) return false;
        const pkgs = (a.packages ?? []).filter((p) => p.amount == null || p.amount > 0);
        const pkgSoon = pkgs.some((p) => {
          const end = dateStrToEndTs(p.expire_at);
          return end != null && end <= windowEnd;
        });
        if (pkgSoon) return true;
        // 老缓存无包明细时回退聚合口径：plan 剩余 > 0 且订阅周期 7 天内到期
        const hasPlanPkg = pkgs.some((p) => p.source === 'plan');
        if (hasPlanPkg || !(a.plan_credits ?? 0)) return false;
        const planEnd = dateStrToEndTs(a.plan_expires_at);
        return planEnd != null && planEnd <= windowEnd;
      })
      .map((a) => a.user_id),
  );
  // 告警账号数 = 凭证类 ∪ 积分到期类并集去重（P3 审查修复：同账号可能跨类别
  // 双计；hint 分项计数保持独立展示，主数值口径为「去重后的告警账号数」）
  const alertCount = new Set([...credAlertSet, ...creditsSoonSet]).size;

  // 积分榜 Top（按余额降序，对齐 Buddy 概述）
  const top = useMemo(
    () =>
      [...(credits?.accounts ?? [])]
        .filter((a) => a.total != null && a.total > 0)
        .sort((a, b) => (b.total ?? 0) - (a.total ?? 0))
        .slice(0, 10)
        .map((a) => ({ name: a.name || a.user_id, credits: a.total as number })),
    [credits],
  );

  // 配置导航（步骤完成态实时判定）
  const steps: Step[] = [
    {
      key: 'account',
      title: '导入 PAT 凭证',
      desc: '在 qoder.com.cn/account/integrations 创建 PAT（pt- 前缀，仅关闭页面前可见一次）后到「账号管理」导入。',
      done: accounts.length > 0,
      actionLabel: '去导入',
      view: 'qoder-accounts',
    },
    {
      key: 'checkin',
      title: '完成首次签到',
      desc: '验证签到链路是否跑通（双活动一次覆盖，claim 幂等可放心重试）。',
      done: records.some((r) => r.date === today),
      actionLabel: '去签到',
      view: 'qoder-checkin',
    },
    {
      key: 'credits',
      title: '查询积分余额',
      desc: '录入凭证后查询各账号 Plan / Add-on 积分余额与积分包到期情况。',
      done: (credits?.accounts.length ?? 0) > 0,
      actionLabel: '查看积分',
      view: 'qoder-credits',
    },
  ];
  // 可选步骤不计入完成度
  const required = steps.filter((s) => !s.optional);
  const completed = required.filter((s) => s.done).length;
  const allDone = completed === required.length;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Qoder · 概述"
        desc="Qoder 账号运行总览 · 告警提醒 / 签到趋势与积分榜"
        actions={
          <button onClick={() => void refresh(true)} className="btn-outline" disabled={refreshing}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      {/* 顶部统计卡（对齐 Buddy 概述：数值一眼掌握） */}
      <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
        <StatCard
          label="账号总数"
          value={total}
          hint={`今日已领 ${checkedToday}`}
          tone="brand"
        />
        <StatCard
          label="可用总积分"
          value={totalBalance != null ? totalBalance.toFixed(2) : '—'}
          hint={credits ? `${okAccounts}/${credits.accounts.length} 个查询成功` : '导入 PAT 后自动查询'}
          tone="amber"
        />
        <StatCard
          label="告警提醒"
          value={alertCount}
          hint={`Token 24h 内过期 ${tokenSoonSet.size} · 需重新登录 ${reloginSet.size} · 积分 7 天内到期 ${creditsSoonSet.size}`}
          tone={alertCount > 0 ? 'red' : 'slate'}
        />
      </div>

      {/* 近 30 天签到结果（对齐 Buddy 概述：堆叠柱状按日汇总） */}
      <div className="mt-5 card p-5">
        <div className="mb-4 flex items-center justify-between">
          <h3 className="font-medium">近 30 天签到结果</h3>
          <span className="text-xs text-slate-400">按日汇总 · 成功 / 已领 / 失败</span>
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
                <Bar dataKey="already" name="已领" stackId="trend" fill="#0ea5e9" maxBarSize={28} />
                <Bar dataKey="failed" name="失败" stackId="trend" fill="#f43f5e" maxBarSize={28} radius={[4, 4, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>

      {/* 积分榜 Top 榜（对齐 Buddy 概述） */}
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
            暂无积分数据，导入 PAT 并查询积分后展示 Top 榜。
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

      {/* 配置导航（对齐 Buddy 概述 SetupGuide 形态） */}
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
                <div className="flex shrink-0 items-center gap-2">
                  <span className="shrink-0 rounded-full bg-emerald-50 px-2.5 py-1 text-xs font-medium text-emerald-600 dark:bg-emerald-500/15 dark:text-emerald-400">
                    已完成
                  </span>
                </div>
              ) : step.optional ? (
                <span className="shrink-0 rounded-full bg-zinc-100 px-2.5 py-1 text-xs font-medium text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                  可选
                </span>
              ) : (
                <button
                  onClick={() => setView(step.view)}
                  className="btn-outline shrink-0"
                >
                  {step.actionLabel}
                  <ChevronRight size={14} />
                </button>
              )}
            </li>
          ))}
        </ol>
        {/* Qoder 说明（签到收益账号级） */}
        <div className="mt-3 border-t border-slate-100 px-4 pt-3 text-xs leading-relaxed text-slate-400 dark:border-zinc-800">
          <b className="font-medium text-slate-500 dark:text-zinc-400">Qoder 说明：</b>
          Credits 按账号消耗（FEFO：最先到期优先）；签到收益为账号级 Add-on Credits。
          两条产品线不互通：原灵码订单（lingma_ 前缀）的 Credits 不在 Qoder 生态生效。
        </div>
        {allDone && (
          <div className="border-t border-slate-100 bg-emerald-50/60 px-4 py-3 text-sm text-emerald-700 dark:border-zinc-800 dark:bg-emerald-500/10 dark:text-emerald-300">
            全部配置已完成，双活动每日领取交给自动化即可！
          </div>
        )}
      </div>
    </div>
  );
}
