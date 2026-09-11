import { useEffect, useMemo, useState } from 'react';
import { RefreshCw, ShieldCheck, Users, CheckCircle2, Circle, ChevronRight, TrendingUp, HeartPulse } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { StatCard, Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { AppLocate, DoubaoAccountView, DoubaoHistoryEvent } from '../../types';

/** 豆包配置导航步骤定义（完成判定全部来自实时状态） */
interface GuideStep {
  key: string;
  title: string;
  desc: string;
  done: boolean;
  actionLabel: string;
  run: () => Promise<void> | void;
}

/** 豆包配置导航卡：安装 → 保存登录态 → 证书 → 代理（自动抓凭证）→ 会员额度 */
function DoubaoSetupGuide({ installed, accounts }: { installed: boolean; accounts: DoubaoAccountView[] }) {
  const certInstalled = useAppStore((s) => s.certInstalled);
  const proxy = useAppStore((s) => s.proxy);
  const startProxy = useAppStore((s) => s.startProxy);
  const setView = useAppStore((s) => s.setView);
  const pushToast = useAppStore((s) => s.pushToast);
  const refreshCert = useAppStore((s) => s.refreshCert);

  const [busy, setBusy] = useState<string | null>(null);
  const snapshotCount = accounts.filter((a) => a.has_snapshot).length;
  const quotaChecked = accounts.some((a) => a.quota_checked_at);

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
      key: 'quota',
      title: '查看会员额度',
      desc: '额度接口已内置默认值，无需配置。有会话凭证的账号会自动查询，并在账号名后显示会员等级或「免费」标识（悬停账号名看额度状态 / 到期时间）。',
      done: quotaChecked,
      actionLabel: '去账号管理',
      run: () => setView('doubao-accounts'),
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
              <button onClick={() => void handleRun(step)} disabled={busy === step.key} className="btn-primary shrink-0">
                {busy === step.key ? '处理中…' : step.actionLabel}
                <ChevronRight size={14} />
              </button>
            )}
          </li>
        ))}
      </ol>

      {allDone && (
        <div className="border-t border-slate-100 bg-emerald-50/60 px-4 py-3 text-sm text-emerald-700 dark:border-zinc-800 dark:bg-emerald-500/10 dark:text-emerald-300">
          全部配置已完成，可在「账号管理」切换账号并查询会员额度。
        </div>
      )}
    </div>
  );
}

/** 额度趋势 + 运维健康卡（A2/B2）：数据来自 doubao_health_history.json 运维历史 */
function DoubaoInsights({ events }: { events: DoubaoHistoryEvent[] }) {
  const setView = useAppStore((s) => s.setView);

  // 趋势：取「当前时段」窗口的 used_percent，按天取最后一次，近 14 天
  const trend = useMemo(() => {
    const byDay = new Map<string, number>();
    for (const e of events) {
      if (e.kind !== 'quota' || !e.ok) continue;
      const w = (e.windows ?? []).find((x) => x.name?.includes('当前时段'));
      if (!w || typeof w.used_percent !== 'number') continue;
      byDay.set(e.ts.slice(0, 10), Math.max(0, Math.min(100, w.used_percent)));
    }
    return [...byDay.entries()].sort(([a], [b]) => a.localeCompare(b)).slice(-14);
  }, [events]);

  // 健康：近 7 天各 kind 计数
  const health = useMemo(() => {
    const cutoff = Date.now() - 7 * 86400000;
    const recent = events.filter((e) => new Date(e.ts.replace(' ', 'T')).getTime() >= cutoff);
    const count = (k: DoubaoHistoryEvent['kind']) => recent.filter((e) => e.kind === k).length;
    const lastKeepalive = [...events].reverse().find((e) => e.kind === 'keepalive');
    return {
      keepalive: count('keepalive'),
      renew: count('renew'),
      quota: count('quota'),
      lastKeepaliveAt: lastKeepalive?.ts ?? null,
    };
  }, [events]);

  const W = 280;
  const H = 56;
  const pts = trend.map(([, pct], i) => {
    const x = trend.length === 1 ? W / 2 : (i / (trend.length - 1)) * (W - 8) + 4;
    const y = 4 + (1 - pct / 100) * (H - 12);
    return { x, y, pct };
  });
  const line = pts.map((p) => `${p.x.toFixed(1)},${p.y.toFixed(1)}`).join(' ');
  const area = pts.length > 1 ? `${line} ${pts[pts.length - 1].x.toFixed(1)},${H - 2} ${pts[0].x.toFixed(1)},${H - 2}` : '';
  const last = pts[pts.length - 1];

  return (
    <div className="mt-5 grid gap-4 lg:grid-cols-2">
      {/* 额度趋势（近 14 天） */}
      <div className="card p-4">
        <div className="mb-2 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <TrendingUp size={16} className="text-amber-500" />
            <span className="text-sm font-medium">额度趋势 · 当前时段</span>
          </div>
          {last && (
            <span className={`text-xs font-medium ${last.pct >= 100 ? 'text-rose-500' : 'text-slate-500'}`}>
              {trend[trend.length - 1][0]} 已用 {Math.round(last.pct)}%
            </span>
          )}
        </div>
        {pts.length >= 2 ? (
          <svg viewBox={`0 0 ${W} ${H}`} className="w-full" role="img" aria-label="额度使用趋势图">
            <line x1="4" y1={H - 2} x2={W - 4} y2={H - 2} className="stroke-slate-200 dark:stroke-zinc-700" strokeWidth="1" />
            <line x1="4" y1="4" x2={W - 4} y2="4" className="stroke-slate-100 dark:stroke-zinc-800" strokeWidth="1" strokeDasharray="3 3" />
            {area && <polygon points={area} className="fill-amber-500/10" />}
            <polyline points={line} className="fill-none stroke-amber-500" strokeWidth="2" strokeLinejoin="round" strokeLinecap="round" />
            {last && <circle cx={last.x} cy={last.y} r="3" className="fill-amber-500" />}
          </svg>
        ) : (
          <p className="py-6 text-center text-xs text-slate-400">
            数据积累中：查询会员额度或注册「每日额度巡检」后，这里会展示近 14 天额度用量走势。
          </p>
        )}
        <div className="mt-1 flex justify-between text-[11px] text-slate-400">
          <span>{trend[0]?.[0] ?? ''}</span>
          <span>{trend[trend.length - 1]?.[0] ?? ''}</span>
        </div>
      </div>

      {/* 运维健康（近 7 天） */}
      <div className="card p-4">
        <div className="mb-2 flex items-center gap-2">
          <HeartPulse size={16} className="text-emerald-500" />
          <span className="text-sm font-medium">运维健康 · 近 7 天</span>
        </div>
        <div className="flex flex-wrap items-center gap-2 text-xs">
          <Badge tone={health.keepalive > 0 ? 'green' : 'slate'}>保活 {health.keepalive} 次</Badge>
          <Badge tone={health.renew > 0 ? 'green' : 'slate'}>探活巡检 {health.renew} 次</Badge>
          <Badge tone={health.quota > 0 ? 'green' : 'slate'}>额度查询 {health.quota} 次</Badge>
        </div>
        <p className="mt-2 text-xs text-slate-400">
          {health.lastKeepaliveAt
            ? `最近保活：${health.lastKeepaliveAt}（每日任务自动执行，超过 25 天未保活会提醒）`
            : '尚未执行过保活：可在「账号管理」立即保活，或在「环境配置」注册每日保活任务。'}
        </p>
        <button onClick={() => setView('doubao-settings')} className="mt-2 text-xs text-brand-600 hover:underline dark:text-brand-400">
          前往环境配置管理保活 / 巡检任务 →
        </button>
      </div>
    </div>
  );
}

/** 会员状态块（到期日历并入账号概览行）：过期红 / ≤7 天临期琥珀 / 其余灰 */
function MemberExpiry({ a }: { a: DoubaoAccountView }) {
  if (!a.quota_checked_at) {
    return <div className="shrink-0 text-[11px] text-slate-300 dark:text-zinc-600">未查额度</div>;
  }
  if (!a.quota_expire_at) {
    return <div className="shrink-0 text-xs text-slate-300 dark:text-zinc-600">免费账号</div>;
  }
  const ts = new Date(a.quota_expire_at.replace(' ', 'T')).getTime();
  if (Number.isNaN(ts)) return null;
  const days = Math.ceil((ts - Date.now()) / 86400000);
  const date = a.quota_expire_at.slice(0, 10);
  if (days <= 0) {
    return (
      <div className="shrink-0 text-right text-xs font-medium text-rose-500" title={`会员已于 ${date} 过期`}>
        会员已过期
        <div className="text-[11px] font-normal text-rose-400">{date}</div>
      </div>
    );
  }
  if (days <= 7) {
    return (
      <div className="shrink-0 text-right text-xs font-medium text-amber-500" title={`会员 ${date} 到期，剩余 ${days} 天`}>
        {days} 天后到期
        <div className="text-[11px] font-normal text-slate-400">{date}</div>
      </div>
    );
  }
  return (
    <div className="shrink-0 text-right text-xs text-slate-500 dark:text-zinc-400" title={`会员 ${date} 到期`}>
      会员到期
      <div className="text-[11px] text-slate-400">{date}</div>
    </div>
  );
}

export default function DoubaoOverview() {
  const pushToast = useAppStore((s) => s.pushToast);
  const setView = useAppStore((s) => s.setView);
  const [locate, setLocate] = useState<AppLocate | null>(null);
  const [accounts, setAccounts] = useState<DoubaoAccountView[]>([]);
  const [history, setHistory] = useState<DoubaoHistoryEvent[]>([]);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = async () => {
    setRefreshing(true);
    try {
      // 环境检测 / 账号池 / 运维历史并行；后两者失败不阻断概述
      const [loc, accs, hist] = await Promise.all([
        api.env.locate('doubao'),
        api.doubao.accountsList().catch(() => [] as DoubaoAccountView[]),
        api.doubao.history().catch(() => [] as DoubaoHistoryEvent[]),
      ]);
      setLocate(loc);
      setAccounts(accs);
      setHistory(hist);
    } catch (err) {
      pushToast('error', `豆包环境检测失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
  };

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const installed = !!locate?.exe;
  const sourceText =
    locate?.source === 'settings'
      ? '手动指定'
      : locate?.source === 'registry'
        ? '注册表'
        : locate?.source === 'default'
          ? '默认路径'
          : locate?.source === 'process'
            ? '进程反查'
            : '未检测到';
  const snapshotCount = accounts.filter((a) => a.has_snapshot).length;
  const currentAccount = accounts.find((a) => a.is_current);

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="豆包 · 概述"
        desc="豆包桌面版多账号管理 · 快照切换 / 会话保活 / 会员额度"
        actions={
          <button onClick={() => void refresh()} className="btn-outline" disabled={refreshing}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      <div className="mt-5 grid grid-cols-2 gap-4 xl:grid-cols-4">
        <button className="text-left" onClick={() => setView('doubao-accounts')}>
          <StatCard
            label="账号总数"
            value={String(accounts.length)}
            hint={
              currentAccount
                ? `当前：${currentAccount.name}`
                : accounts.length > 0
                  ? '尚无切换记录'
                  : '在账号管理页保存登录态后计入'
            }
            tone="brand"
          />
        </button>
        <StatCard
          label="安装情况"
          value={installed ? '已安装' : '未检测到'}
          hint={locate?.version ? `${sourceText} · v${locate.version}` : sourceText}
          tone={installed ? 'green' : 'amber'}
        />
        <button className="text-left" onClick={() => setView('doubao-accounts')}>
          <StatCard
            label="登录态快照"
            value={String(snapshotCount)}
            hint={snapshotCount > 0 ? '可随时切换/恢复' : '随首次保存登录态生成'}
            tone="violet"
          />
        </button>
        <StatCard
          label="数据目录"
          value={locate?.user_data_dir ? '已定位' : '—'}
          hint={locate?.user_data_dir}
          tone="amber"
        />
      </div>

      {/* 洞察卡（额度趋势 + 运维健康）上移至账号概览之前 */}
      {(history.length > 0 || accounts.length > 0) && <DoubaoInsights events={history} />}

      {/* 账号概览：会员等级/到期状态并入行内（原到期日历信息） */}
      {accounts.length > 0 && (
        <div className="mt-5 card p-4">
          <div className="mb-3 flex items-center gap-2">
            <Users size={16} className="text-violet-500" />
            <span className="text-sm font-medium">账号概览</span>
          </div>
          <div className="space-y-2">
            {accounts.slice(0, 5).map((a) => (
              <button
                key={a.user_id}
                onClick={() => setView('doubao-accounts')}
                className="flex w-full items-center gap-3 rounded-lg border border-slate-100 p-3 text-left transition hover:bg-slate-50 dark:border-zinc-800 dark:hover:bg-zinc-900"
                title={a.quota_summary ? `额度：${a.quota_summary}${a.quota_checked_at ? `（${a.quota_checked_at} 查询）` : ''}` : undefined}
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2 text-sm font-medium">
                    {a.name}
                    {a.is_current && (
                      <span className="rounded bg-emerald-100 px-1.5 py-0.5 text-[11px] font-medium text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300">
                        当前账号
                      </span>
                    )}
                    {a.quota_level ? (
                      <Badge tone="violet">{a.quota_level}</Badge>
                    ) : a.quota_checked_at ? (
                      <Badge tone="slate">免费</Badge>
                    ) : null}
                  </div>
                  <div className="font-mono text-xs text-slate-400">{a.user_id}</div>
                </div>
                <MemberExpiry a={a} />
                <div className="shrink-0 text-xs text-slate-400">
                  {a.has_snapshot ? `快照 ${a.last_modified || '—'}` : '无快照'}
                </div>
              </button>
            ))}
            {accounts.length > 5 && (
              <button onClick={() => setView('doubao-accounts')} className="text-xs text-brand-600 hover:underline dark:text-brand-400">
                查看全部 {accounts.length} 个账号 →
              </button>
            )}
          </div>
        </div>
      )}

      <DoubaoSetupGuide installed={installed} accounts={accounts} />

      <div className="mt-4 flex items-start gap-2 rounded-lg border border-slate-100 p-3 text-xs text-slate-500 dark:border-zinc-800">
        <ShieldCheck size={14} className="mt-0.5 shrink-0" />
        <span>
          合规说明：本工具仅管理本人合法持有的豆包账号，不破解、不绕过付费；会员额度仅做展示。会话凭证等同密码，仅本地存储并全程掩码展示。
        </span>
      </div>
    </div>
  );
}
