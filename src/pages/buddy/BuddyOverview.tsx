import { useEffect, useState } from 'react';
import { RefreshCw, ExternalLink, ArrowRight } from 'lucide-react';
import { open } from '@tauri-apps/plugin-shell';
import PageHeader from '../../components/PageHeader';
import { StatCard, Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type { WorkBuddyEnvCheck, WorkBuddyAccountView, WbCreditsResult } from '../../types';
import type { ExpiryItem as ExpiryEntry } from '../../components/ExpiryCalendar';

/** WorkBuddy 概述页（§3.7.1）：四指标卡 + 环境卡 + 到期提醒条 + 快捷入口 */
export default function BuddyOverview() {
  const pushToast = useAppStore((s) => s.pushToast);
  const setView = useAppStore((s) => s.setView);
  const [env, setEnv] = useState<WorkBuddyEnvCheck | null>(null);
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [credits, setCredits] = useState<WbCreditsResult | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = async () => {
    setRefreshing(true);
    try {
      const [e, accs] = await Promise.all([
        api.workbuddy.envCheck(),
        api.workbuddy.accountsList().catch(() => [] as WorkBuddyAccountView[]),
      ]);
      setEnv(e);
      setAccounts(accs);
      // 积分失败不阻断概述（未录入凭证时正常）
      api.workbuddy
        .creditsFetch()
        .then((r) => setCredits(r))
        .catch(() => setCredits(null));
    } catch (err) {
      pushToast('error', `WorkBuddy 环境检测失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
  };

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const current = accounts.find((a) => a.is_current);
  const backupCount = accounts.length - (current ? 1 : 0);
  const totalBalance = credits?.accounts.reduce((s, a) => s + (a.balance ?? 0), 0);

  // 到期提醒条：7 天内到期的 token / 积分包
  const expiring: ExpiryEntry[] = [];
  for (const a of accounts) {
    if (a.access_token_expires_at) {
      const days = (a.access_token_expires_at * 1000 - Date.now()) / 86400000;
      if (days < 7) {
        expiring.push({
          key: `token-${a.id}`,
          label: `${a.nickname || a.id} · accessToken`,
          kind: 'token',
          expire_ts: a.access_token_expires_at,
          note: null,
        });
      }
    }
  }
  for (const acc of credits?.accounts ?? []) {
    for (const p of acc.packages) {
      if (p.expire_soon && p.expire_ts) {
        expiring.push({
          key: `pkg-${acc.user_id}-${p.name}-${p.expire_ts}`,
          label: `${acc.name} · ${p.name}`,
          kind: '积分包',
          expire_ts: p.expire_ts,
          note: `剩余 ${p.remaining.toFixed(2)}`,
        });
      }
    }
  }

  const sourceText =
    env?.exe == null ? '未检测到' : '已定位';

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="WorkBuddy · 概述"
        desc="WorkBuddy / CodeBuddy 多账号管理 · 切换 / 续期 / 签到 / 积分"
        actions={
          <>
            <button
              className="btn-outline"
              onClick={() => {
                const exe = env?.exe;
                if (exe) {
                  void open(`file:///${exe}`).catch((e) => pushToast('error', `打开客户端失败：${String(e)}`));
                } else {
                  pushToast('warn', '未检测到 WorkBuddy 客户端，请先安装');
                }
              }}
            >
              <ExternalLink size={15} /> 打开客户端
            </button>
            <button onClick={() => void refresh()} className="btn-outline" disabled={refreshing}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
            </button>
          </>
        }
      />

      {/* 四指标卡 */}
      <div className="grid grid-cols-2 gap-4 xl:grid-cols-4">
        <button className="text-left" onClick={() => setView('buddy-accounts')}>
          <StatCard
            label="当前账号"
            value={current ? current.nickname || current.id : '未切换'}
            hint={current ? current.edition_type || '—' : '可在账号管理页设为当前'}
            tone="brand"
          />
        </button>
        <button className="text-left" onClick={() => setView('buddy-accounts')}>
          <StatCard
            label="池内账号"
            value={String(accounts.length)}
            hint={backupCount > 0 ? `备用 ${backupCount} 个` : '在账号管理页导入本机账号'}
            tone="violet"
          />
        </button>
        <StatCard
          label="总剩余积分"
          value={totalBalance != null ? totalBalance.toFixed(2) : '—'}
          hint={credits?.cached ? '缓存数据 · 可强制刷新' : credits ? '实时数据' : '录入凭证后自动查询'}
          tone="amber"
        />
        <StatCard
          label="客户端"
          value={env?.running ? '运行中' : env?.installed ? '已停止' : '未检测到'}
          hint={env?.version ? `v${env.version} · ${sourceText}` : sourceText}
          tone={env?.running ? 'green' : env?.installed ? 'slate' : 'amber'}
        />
      </div>

      {/* 环境卡 */}
      <div className="mt-5 card p-4">
        <div className="mb-3 flex items-center justify-between">
          <span className="text-sm font-medium">环境检测</span>
          <div className="flex items-center gap-2">
            <Badge tone={env?.installed ? 'green' : 'red'}>{env?.installed ? '已安装' : '未安装'}</Badge>
            <Badge tone={env?.running ? 'green' : 'slate'}>{env?.running ? '运行中' : '已停止'}</Badge>
            <Badge tone={env?.auth_file_exists ? 'green' : 'amber'}>
              {env?.auth_file_exists ? 'auth 文件 ✓' : 'auth 文件缺失'}
            </Badge>
          </div>
        </div>
        <div className="grid gap-3 lg:grid-cols-2">
          <div className="rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
            <div className="mb-1 font-medium text-slate-500">客户端路径</div>
            <div className="break-all font-mono text-slate-600 dark:text-zinc-300">{env?.exe ?? '未检测到'}</div>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
            <div className="mb-1 font-medium text-slate-500">auth 文件（登录态）</div>
            <div className="break-all font-mono text-slate-600 dark:text-zinc-300">
              {env?.auth_file_exists ? '%LOCALAPPDATA%\\CodeBuddyExtension\\Data\\Public\\auth\\workbuddy-desktop.info' : '不存在（未登录）'}
            </div>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
            <div className="mb-1 font-medium text-slate-500">当前登录快照</div>
            <div className="text-slate-600 dark:text-zinc-300">
              {env?.snapshot_uid
                ? `${env.snapshot_nickname || env.snapshot_uid}（${env.snapshot_edition || '未知版本'}）`
                : env?.data_dir_exists
                  ? '~/.workbuddy 快照不可读（未登录或未启动过）'
                  : '~/.workbuddy 数据目录不存在'}
            </div>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
            <div className="mb-1 font-medium text-slate-500">数据目录</div>
            <div className="break-all font-mono text-slate-600 dark:text-zinc-300">
              {env?.data_dir_exists ? '~/.workbuddy ✓' : '~/.workbuddy（不存在）'}
            </div>
          </div>
        </div>
      </div>

      {/* 到期提醒条 */}
      {expiring.length > 0 && (
        <div className="mt-4 card p-4">
          <div className="mb-2 flex items-center justify-between">
            <span className="text-sm font-medium">7 天内到期提醒</span>
            <button onClick={() => setView('buddy-settings')} className="flex items-center gap-1 text-xs text-brand-600 hover:underline dark:text-brand-400">
              到期日历 <ArrowRight size={12} />
            </button>
          </div>
          <div className="flex gap-2 overflow-x-auto pb-1">
            {expiring.map((it) => (
              <span key={it.key} className="flex shrink-0 items-center gap-2 rounded-lg border border-rose-200 bg-rose-50 px-3 py-2 text-xs dark:border-rose-500/30 dark:bg-rose-500/10">
                <Badge tone="red">即将到期</Badge>
                <span className="font-medium text-slate-700 dark:text-zinc-200">{it.label}</span>
                <span className="text-slate-400">{it.note}</span>
              </span>
            ))}
          </div>
        </div>
      )}

      {/* 快捷入口 */}
      <div className="mt-4 flex flex-wrap gap-2">
        <button className="btn-outline" onClick={() => setView('buddy-accounts')}>去切换账号</button>
        <button className="btn-outline" onClick={() => setView('buddy-checkin')}>立即签到</button>
        <button className="btn-outline" onClick={() => setView('buddy-credits')}>查看积分</button>
      </div>
    </div>
  );
}
