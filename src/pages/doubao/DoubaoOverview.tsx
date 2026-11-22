import { useEffect, useMemo, useState } from 'react';
import { RefreshCw, CheckCircle2, Circle, ChevronRight, TriangleAlert } from 'lucide-react';
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
import CertManualGuide, { caCerExists } from '../../components/CertManualGuide';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { useIsDark } from '../../lib/useIsDark';
import type { AppLocate, DoubaoAccountView, DoubaoCheckinTrendPoint, DoubaoDashboardStats } from '../../types';

/** 豆包配置导航步骤定义（完成判定全部来自实时状态） */
interface GuideStep {
  key: string;
  title: string;
  desc: string;
  done: boolean;
  actionLabel: string;
  run: () => Promise<void> | void;
}

/** 豆包配置导航卡：安装 → 保存登录态 → 证书 → 代理（自动抓凭证）→ 完成首次签到 */
function DoubaoSetupGuide({ installed, accounts }: { installed: boolean; accounts: DoubaoAccountView[] }) {
  const certInstalled = useAppStore((s) => s.certInstalled);
  const proxy = useAppStore((s) => s.proxy);
  const startProxy = useAppStore((s) => s.startProxy);
  const setView = useAppStore((s) => s.setView);
  const pushToast = useAppStore((s) => s.pushToast);
  const refreshCert = useAppStore((s) => s.refreshCert);

  const [busy, setBusy] = useState<string | null>(null);
  // cert 步骤失败后展示手动导入指引（issue #12/#79）
  const [certGuideOpen, setCertGuideOpen] = useState(false);
  const snapshotCount = accounts.filter((a) => a.has_snapshot).length;
  const checkinDone = accounts.some((a) => a.checkin_last_date);

  const steps: GuideStep[] = [
    {
      key: 'install',
      title: '安装豆包客户端',
      desc: '多账号快照切换的目标客户端，需先安装并登录豆包账号。',
      done: installed,
      actionLabel: '打开豆包',
      run: async () => {
        try {
          await api.doubao.launch(proxy.running ? proxy.port : undefined);
        } catch (err) {
          pushToast('error', `打开豆包失败：${String(err)}`);
        }
      },
    },
    {
      key: 'snapshot',
      title: '保存当前登录态',
      desc: '在豆包中登录账号后到「账号管理」保存登录态，生成可切换的快照。',
      done: snapshotCount > 0,
      actionLabel: '去保存',
      run: () => setView('doubao-accounts'),
    },
    {
      key: 'cert',
      title: '信任 CA 证书',
      desc: '抓包代理需系统信任其证书，用于自动抓取会话凭证与会员额度数据。',
      done: certInstalled,
      actionLabel: '安装证书',
      run: async () => {
        await api.cert.install();
        await refreshCert();
      },
    },
    {
      key: 'proxy',
      title: '启动代理',
      desc: '代理运行后豆包流量经过代理：自动抓取当前账号的会话凭证（sessionid / sid_guard）并回写账号池，无需手动录入。',
      done: proxy.running,
      actionLabel: '启动代理',
      run: async () => {
        await startProxy();
      },
    },
    {
      key: 'checkin',
      title: '完成首次签到',
      desc: '有会话凭证的账号可在「一键签到」完成每日签到，成功后自动回写总额度与连续签到天数。',
      done: checkinDone,
      actionLabel: '去签到',
      run: () => setView('doubao-checkin'),
    },
  ];

  const completed = steps.filter((s) => s.done).length;
  const allDone = completed === steps.length;

  const handleRun = async (step: GuideStep) => {
    if (step.done || busy) return;
    setBusy(step.key);
    try {
      await step.run();
    } catch (e) {
      // 兜底弹出真实错误（issue #6：静默吞错导致"点了没反应"）
      pushToast('error', String(e));
      // cert 失败时卡片内给出手动导入三步指引（issue #12/#79）；ca.cer 未生成
      // （ensure_ca 失败）时指引无意义，仅保留报错 toast
      if (step.key === 'cert' && (await caCerExists())) setCertGuideOpen(true);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="mt-5 card overflow-hidden">
      <div className="flex items-center justify-between border-b border-slate-100 px-4 py-3 dark:border-zinc-800">
        <div>
          <h3 className="font-medium">配置导航</h3>
          <p className="text-xs text-slate-500">按步骤完成豆包管理初始化，已完成的步骤无需重复处理。</p>
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
              <button onClick={() => void handleRun(step)} disabled={busy === step.key} className="btn-outline shrink-0">
                {busy === step.key ? '处理中…' : step.actionLabel}
                <ChevronRight size={14} />
              </button>
            )}
          </li>
        ))}
      </ol>

      {certGuideOpen && !certInstalled && (
        <CertManualGuide onClose={() => setCertGuideOpen(false)} />
      )}

      {allDone && (
        <div className="border-t border-slate-100 bg-emerald-50/60 px-4 py-3 text-sm text-emerald-700 dark:border-zinc-800 dark:bg-emerald-500/10 dark:text-emerald-300">
          全部配置已完成，可在「账号管理」切换账号，在「额度看板」查看额度趋势。
        </div>
      )}
    </div>
  );
}

/**
 * 近 30 天签到结果卡：按日落库保证同账号同日仅一条记录（重试轮覆盖为最终态），
 * 展示成功（含已签，已签视为成功）/ 失败计数 + Recharts 三色堆叠柱状图（对齐 Buddy 概述样式）
 */
function CheckinResultsCard({ trend }: { trend: DoubaoCheckinTrendPoint[] }) {
  const sums = useMemo(
    () =>
      // P3-15 审查修复：徽标并入 skip（确定性跳过）——trend 点已带 skip 字段，
      // 此前徽标只计 ok/failed，与下方堆叠柱状图三色口径不一致
      trend.reduce(
        (acc, d) => ({
          ok: acc.ok + d.ok + d.already,
          skip: acc.skip + (d.skip ?? 0),
          failed: acc.failed + d.failed,
        }),
        { ok: 0, skip: 0, failed: 0 },
      ),
    [trend],
  );
  const isDark = useIsDark();

  return (
    <div className="card p-5">
      <div className="mb-4 flex items-center justify-between">
        <h3 className="font-medium">近 30 天签到结果</h3>
        <div className="flex items-center gap-1.5 text-xs">
          <Badge tone={sums.ok > 0 ? 'green' : 'slate'}>成功 {sums.ok}</Badge>
          {sums.skip > 0 && <Badge tone="slate">跳过 {sums.skip}</Badge>}
          <Badge tone={sums.failed > 0 ? 'red' : 'slate'}>失败 {sums.failed}</Badge>
        </div>
      </div>
      {trend.length === 0 ? (
        <div className="flex h-40 items-center justify-center text-sm text-slate-400">
          数据积累中：注册「每日签到」或到「一键签到」手动签到后，这里会展示近 30 天每日签到结果。
        </div>
      ) : (
        <div className="h-64">
          <ResponsiveContainer>
            <BarChart data={trend} margin={{ top: 8, right: 16, left: 0, bottom: 4 }} barCategoryGap="24%">
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
              <Bar dataKey="skip" name="跳过" stackId="trend" fill="#94a3b8" maxBarSize={28} />
              <Bar dataKey="failed" name="失败" stackId="trend" fill="#f43f5e" maxBarSize={28} radius={[4, 4, 0, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}
    </div>
  );
}

/** 额度 TOP 榜：Recharts 柱状图（对齐 Buddy 积分榜样式），从高到低 Top 8（quota_total 未知不参与） */
function QuotaTopCard({ accounts }: { accounts: DoubaoAccountView[] }) {
  const top = useMemo(
    () =>
      accounts
        .filter((a) => a.quota_total != null)
        .sort((a, b) => (b.quota_total ?? 0) - (a.quota_total ?? 0))
        .slice(0, 8)
        .map((a) => ({ name: a.name || a.user_id, quota: a.quota_total ?? 0 })),
    [accounts],
  );
  const isDark = useIsDark();
  // 柱顶数值紧凑格式：≥1万用「x.x万」，避免轴标签溢出
  const compact = (v: number) => (v >= 10000 ? `${(v / 10000).toFixed(1)}万` : v.toLocaleString());

  return (
    <div className="card p-5">
      <div className="mb-4 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <h3 className="font-medium">额度 TOP 榜</h3>
          {top.length > 0 && (
            <span className="rounded-full bg-zinc-100 px-2 py-0.5 text-xs font-medium text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
              Top {top.length}
            </span>
          )}
        </div>
        <span className="text-xs text-slate-400">按总额度排序</span>
      </div>
      {top.length === 0 ? (
        <div className="flex h-40 items-center justify-center text-sm text-slate-400">
          数据积累中：完成签到或查询额度后，账号总额度会自动回写并参与排行。
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
                formatter={(v: number) => [v.toLocaleString(), '总额度']}
              />
              <Bar dataKey="quota" radius={[8, 8, 0, 0]} maxBarSize={44}>
                {top.map((_, i) => {
                  const colors = isDark ? ['#fafafa', '#e4e4e7', '#d4d4d8'] : ['#27272a', '#3f3f46', '#52525b'];
                  const fill = i < 3 ? colors[i] : isDark
                    ? `rgba(212,212,216,${Math.max(0.35, 0.6 - (i - 3) * 0.05).toFixed(2)})`
                    : `rgba(82,82,91,${Math.max(0.35, 0.6 - (i - 3) * 0.05).toFixed(2)})`;
                  return <Cell key={i} fill={fill} />;
                })}
                <LabelList
                  dataKey="quota"
                  position="top"
                  formatter={compact}
                  style={{ fontSize: 10, fill: isDark ? '#a1a1aa' : '#94a3b8', fontWeight: 500 }}
                />
              </Bar>
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}
    </div>
  );
}

export default function DoubaoOverview() {
  const pushToast = useAppStore((s) => s.pushToast);
  const setView = useAppStore((s) => s.setView);
  const [locate, setLocate] = useState<AppLocate | null>(null);
  const [accounts, setAccounts] = useState<DoubaoAccountView[]>([]);
  const [stats, setStats] = useState<DoubaoDashboardStats | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = async () => {
    setRefreshing(true);
    try {
      // 环境检测 / 账号池 / 看板统计并行；后两者失败不阻断概述。
      // locate 单独 catch 为 null：否则 reject 会短路 Promise.all，丢弃已成功的账号池/看板统计结果
      let locateErr: unknown = null;
      const [loc, accs, st] = await Promise.all([
        api.env.locate('doubao').catch((e) => {
          locateErr = e;
          return null;
        }),
        api.doubao.accountsList().catch(() => [] as DoubaoAccountView[]),
        // 失败哨兵 undefined：保留上次成功数据，不把已有统计清成空态
        //（对齐 accountsList 失败给空数组的降级口径）
        api.doubao.dashboardStats().catch(() => undefined),
      ]);
      // locate 失败沿用原有失败路径（toast 提示环境检测问题），但不清空已有 locate、不丢其余数据
      if (loc == null) pushToast('error', `豆包环境检测失败：${String(locateErr)}`);
      else setLocate(loc);
      setAccounts(accs);
      if (st !== undefined) setStats(st);
    } catch (err) {
      pushToast('error', `豆包概述刷新失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
  };

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const installed = !!locate?.exe;
  const currentAccount = accounts.find((a) => a.is_current);

  /** 告警汇总：会话过期 + 7 天内到期 + 超 25 天未保活（口径与账号管理页到期提醒一致） */
  const alerts = useMemo(() => {
    const expired = accounts.filter((a) => a.session_state === 'expired');
    const soon = accounts.filter((a) => {
      if (!a.session_expire_at || a.session_state === 'expired') return false;
      const t = new Date(a.session_expire_at.replace(' ', 'T')).getTime();
      return !Number.isNaN(t) && t - Date.now() < 7 * 24 * 3600 * 1000;
    });
    // 保活是全局任务：取所有账号中最新一次保活时间（'YYYY-MM-DD HH:MM:SS' 字典序=时间序）
    const lastKa = accounts
      .map((a) => a.last_keepalive_at)
      .filter((v): v is string => !!v)
      .sort()
      .pop();
    const kaStale =
      !!lastKa && (() => {
        const t = new Date(lastKa.replace(' ', 'T')).getTime();
        return !Number.isNaN(t) && Date.now() - t > 25 * 24 * 3600 * 1000;
      })();
    return { expired: expired.length, soon: soon.length, kaStale, total: expired.length + soon.length + (kaStale ? 1 : 0) };
  }, [accounts]);

  const alertHint =
    alerts.total === 0
      ? accounts.length > 0
        ? '暂无告警：会话与保活状态正常'
        : '保存登录态后开始监控会话状态'
      : [
          alerts.expired > 0 ? `${alerts.expired} 个会话已过期` : '',
          alerts.soon > 0 ? `${alerts.soon} 个 7 天内到期` : '',
          alerts.kaStale ? '超 25 天未保活' : '',
        ]
          .filter(Boolean)
          .join(' · ');

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="豆包 · 概述"
        desc="豆包桌面版多账号管理 · 签到 / 额度 / 会话总览"
        actions={
          <button onClick={() => void refresh()} className="btn-outline" disabled={refreshing}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      {/* 顶部 KPI：账号总数 / 总额度 / 登录账号 / 告警提醒 */}
      <div className="mt-5 grid grid-cols-2 gap-4 xl:grid-cols-4">
        <button className="text-left" onClick={() => setView('doubao-accounts')}>
          <StatCard
            label="账号总数"
            value={String(accounts.length)}
            hint={accounts.length > 0 ? '点击进入账号管理' : '在账号管理页保存登录态后计入'}
            tone="brand"
          />
        </button>
        <StatCard
          label="总额度"
          value={(stats?.quota_total ?? 0).toLocaleString()}
          hint={
            stats && stats.quota_known_count > 0
              ? `${stats.quota_known_count}/${stats.account_count} 个账号已知额度`
              : '签到/巡检后自动回写'
          }
          tone="amber"
        />
        <button className="text-left" onClick={() => setView('doubao-accounts')}>
          <StatCard
            label="登录账号"
            value={
              currentAccount ? (
                <span className="flex items-center gap-2 text-xl">
                  <span className="max-w-40 truncate">{currentAccount.name}</span>
                  {currentAccount.quota_level ? (
                    <Badge tone="amber">{currentAccount.quota_level}</Badge>
                  ) : currentAccount.quota_checked_at ? (
                    <Badge tone="slate">免费</Badge>
                  ) : null}
                </span>
              ) : (
                '未检测到'
              )
            }
            hint={currentAccount ? `当前豆包登录账号 · ${currentAccount.user_id}` : '切换/保存登录态后展示'}
            tone="violet"
          />
        </button>
        <StatCard
          label="告警提醒"
          value={
            <span className="flex items-center gap-2">
              {alerts.total > 0 && <TriangleAlert size={20} className="text-rose-500" />}
              {alerts.total}
            </span>
          }
          hint={alertHint}
          tone={alerts.total > 0 ? 'red' : 'green'}
        />
      </div>

      {/* 近 30 天签到结果 + 额度 TOP 榜（各占一行） */}
      <div className="mt-4 space-y-4">
        <CheckinResultsCard trend={stats?.trend ?? []} />
        <QuotaTopCard accounts={accounts} />
      </div>

      <DoubaoSetupGuide installed={installed} accounts={accounts} />
    </div>
  );
}
