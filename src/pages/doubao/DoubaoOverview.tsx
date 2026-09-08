import { useEffect, useState } from 'react';
import { RefreshCw, ShieldCheck, Users, ExternalLink } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { StatCard } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { AppLocate, DoubaoAccountView } from '../../types';

export default function DoubaoOverview() {
  const pushToast = useAppStore((s) => s.pushToast);
  const setView = useAppStore((s) => s.setView);
  const [locate, setLocate] = useState<AppLocate | null>(null);
  const [accounts, setAccounts] = useState<DoubaoAccountView[]>([]);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = async () => {
    setRefreshing(true);
    try {
      // 环境检测与账号池并行；账号池失败不阻断概述
      const [loc, accs] = await Promise.all([
        api.env.locate('doubao'),
        api.doubao.accountsList().catch(() => [] as DoubaoAccountView[]),
      ]);
      setLocate(loc);
      setAccounts(accs);
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

  const launch = async () => {
    try {
      await api.doubao.launch();
    } catch (err) {
      pushToast('error', `打开豆包失败：${String(err)}`);
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="豆包 · 概述"
        desc="豆包桌面版多账号管理 · 快照切换 / 会话保活 / 会员额度"
        actions={
          <>
            <button onClick={() => void launch()} className="btn-outline" disabled={!installed}>
              <ExternalLink size={15} /> 打开豆包
            </button>
            <button onClick={() => void refresh()} className="btn-outline" disabled={refreshing}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
            </button>
          </>
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
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2 text-sm font-medium">
                    {a.name}
                    {a.is_current && (
                      <span className="rounded bg-emerald-100 px-1.5 py-0.5 text-[11px] font-medium text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300">
                        当前账号
                      </span>
                    )}
                  </div>
                  <div className="font-mono text-xs text-slate-400">{a.user_id}</div>
                </div>
                <div className="text-xs text-slate-400">
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

      <div className="mt-4 flex items-start gap-2 rounded-lg border border-slate-100 p-3 text-xs text-slate-500 dark:border-zinc-800">
        <ShieldCheck size={14} className="mt-0.5 shrink-0" />
        <span>
          合规说明：本工具仅管理本人合法持有的豆包账号，不破解、不绕过付费；会员额度仅做展示。会话凭证等同密码，仅本地存储并全程掩码展示。
        </span>
      </div>
    </div>
  );
}
