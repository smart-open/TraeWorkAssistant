import { useCallback, useEffect, useState } from 'react';
import { RefreshCw, Download, Upload, UserPlus, ScanLine, LayoutGrid, Rows3 } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState, Modal, Spinner } from '../../components/ui';
import { Users } from 'lucide-react';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import AccountCard from './AccountCard';
import type { WorkBuddyAccountView, WbCreditPackage, WbCreditsResult } from '../../types';

/**
 * buddy-accounts 账号管理（§3.7.2，F-54/F-56/F-60）：
 * 聚合迁移入口（导入本机账号 / 导出）+ 双态卡片区 + 积分包明细弹窗。
 */
export default function BuddyAccounts() {
  const pushToast = useAppStore((s) => s.pushToast);
  const switchTo = useAppStore((s) => s.switchTo);
  const switchingTo = useAppStore((s) => s.switchingTo);
  const saveCurrentLogin = useAppStore((s) => s.saveCurrentLogin);
  const savingLogin = useAppStore((s) => s.savingLogin);
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [credits, setCredits] = useState<Map<string, WbCreditPackage[]>>(new Map());
  const [loading, setLoading] = useState(false);
  const [importing, setImporting] = useState(false);
  const [scanPreview, setScanPreview] = useState<{ nickname: string; uid: string; exists: boolean } | null>(null);
  const [detailFor, setDetailFor] = useState<WorkBuddyAccountView | null>(null);
  const [deleteFor, setDeleteFor] = useState<WorkBuddyAccountView | null>(null);
  const [editFor, setEditFor] = useState<WorkBuddyAccountView | null>(null);
  const [editName, setEditName] = useState('');
  const [cardView, setCardView] = useState(true);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const accs = await api.workbuddy.accountsList();
      setAccounts(accs);
      // 积分缓存查询（≥5min 缓存，失败不阻断）
      api.workbuddy
        .creditsFetch()
        .then((r: WbCreditsResult) => {
          const m = new Map<string, WbCreditPackage[]>();
          for (const a of r.accounts) m.set(a.user_id, a.packages ?? []);
          setCredits(m);
        })
        .catch(() => {});
    } catch (err) {
      pushToast('error', `读取账号失败：${String(err)}`);
    } finally {
      setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 导入本机账号（F-04）：扫描 auth 文件 → 预览确认 → 入池
  const importFromAuth = async () => {
    setImporting(true);
    try {
      const scan = await withMinDelay(api.workbuddy.scanAuthFile(), 800);
      if (!scan) {
        pushToast('warn', '未找到 auth 文件：请先在 WorkBuddy 客户端登录一个账号');
        return;
      }
      if (scan.exists) {
        pushToast('info', `该账号已在池中（${scan.nickname || scan.id}）`);
        return;
      }
      setScanPreview({ nickname: scan.nickname, uid: scan.uid, exists: scan.exists });
    } catch (err) {
      pushToast('error', `扫描 auth 文件失败：${String(err)}`);
    } finally {
      setImporting(false);
    }
  };

  const confirmImport = async () => {
    try {
      const name = scanPreview?.nickname || undefined;
      const view = await withMinDelay(api.workbuddy.accountImportAuth(name), 1000);
      pushToast('success', `账号「${view.nickname || view.id}」已入池`);
      setScanPreview(null);
      await refresh();
    } catch (err) {
      pushToast('error', `导入失败：${String(err)}`);
    }
  };

  const handleSwitch = (a: WorkBuddyAccountView) => {
    if (a.needs_relogin) {
      pushToast('warn', '该账号标记需重新登录，请先重新登录客户端并保存登录态');
      return;
    }
    void switchTo(a.id, 'WorkBuddy');
  };

  const handleSave = (a: WorkBuddyAccountView) => {
    void saveCurrentLogin(a.id, 'WorkBuddy');
  };

  const handleRefreshToken = async (a: WorkBuddyAccountView) => {
    try {
      await withMinDelay(api.workbuddy.refreshToken(a.id), 1000);
      pushToast('success', `「${a.nickname || a.id}」凭证已续期`);
      await refresh();
    } catch (err) {
      pushToast('error', `续期失败：${String(err)}`);
    }
  };

  const handleDelete = (a: WorkBuddyAccountView) => {
    setDeleteFor(a);
  };

  const confirmDelete = async () => {
    if (!deleteFor) return;
    try {
      await api.workbuddy.accountRemove(deleteFor.id, false);
      pushToast('info', '账号已删除');
      setDeleteFor(null);
      await refresh();
    } catch (err) {
      pushToast('error', `删除失败：${String(err)}`);
    }
  };

  const handleEdit = (a: WorkBuddyAccountView) => {
    setEditFor(a);
    setEditName(a.nickname);
  };

  const confirmEdit = async () => {
    if (!editFor) return;
    try {
      await api.workbuddy.accountSave(editFor.id, editName || undefined, undefined);
      setEditFor(null);
      await refresh();
      pushToast('success', '账号已更新');
    } catch (err) {
      pushToast('error', `更新失败：${String(err)}`);
    }
  };

  const exportPool = () => {
    // 导出账号元数据（掩码凭证：不含 token，仅 uid/昵称/过期时间）
    const data = accounts.map((a) => ({
      id: a.id,
      uid: a.uid,
      nickname: a.nickname,
      edition_type: a.edition_type,
      access_token_expires_at: a.access_token_expires_at,
      refresh_token_expires_at: a.refresh_token_expires_at,
      note: a.note,
    }));
    const blob = new Blob([JSON.stringify({ accounts: data }, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `workbuddy_accounts_${new Date().toISOString().slice(0, 10)}.json`;
    a.click();
    URL.revokeObjectURL(url);
    pushToast('success', '账号元数据已导出（凭证不导出）');
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="WorkBuddy · 账号管理"
        desc="多账号入池 · 切换登录 · 凭证续期"
        actions={
          <>
            <button onClick={() => void refresh()} className="btn-outline" disabled={loading}>
              <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 刷新
            </button>
            <button className="btn-outline" onClick={() => setCardView((v) => !v)} title="切换视图">
              {cardView ? <Rows3 size={15} /> : <LayoutGrid size={15} />}
              {cardView ? '列表' : '卡片'}
            </button>
          </>
        }
      />

      {/* 顶部聚合区「添加与迁移账号」（F-60） */}
      <div className="card p-4">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <div className="text-sm font-medium">添加与迁移账号</div>
            <div className="text-xs text-slate-400">快速导入账号，或从已有环境恢复</div>
          </div>
          <div className="flex flex-wrap gap-2">
            <button className="btn-outline" onClick={() => void importFromAuth()} disabled={importing}>
              {importing ? <Spinner /> : <ScanLine size={15} />} 导入本机账号
            </button>
            <button className="btn-outline" onClick={exportPool} disabled={accounts.length === 0}>
              <Download size={15} /> 导出
            </button>
            <button className="btn-outline" onClick={() => pushToast('info', '导入备份 / OAuth 扫码将随后续批次开放')} disabled>
              <Upload size={15} /> 导入备份
            </button>
            <button className="btn-outline" onClick={() => pushToast('info', 'OAuth 扫码添加将随后续批次开放')} disabled>
              <UserPlus size={15} /> OAuth 扫码
            </button>
          </div>
        </div>
      </div>

      {/* 账号卡片区 */}
      {accounts.length === 0 ? (
        <div className="mt-5">
          <EmptyState
            icon={<Users size={26} />}
            title="暂无 WorkBuddy 账号"
            hint="先在 WorkBuddy 客户端登录，然后点击上方「导入本机账号」自动扫描入池；切换/保存登录态会在账号管理中生成快照。"
          />
        </div>
      ) : cardView ? (
        <div className="mt-5 grid gap-4 lg:grid-cols-2 xl:grid-cols-3">
          {accounts.map((a) => (
            <AccountCard
              key={a.id}
              account={a}
              packages={credits.get(a.id) ?? []}
              switching={switchingTo === a.id || savingLogin === a.id}
              onSwitch={() => handleSwitch(a)}
              onSaveLogin={() => handleSave(a)}
              onRefreshToken={() => void handleRefreshToken(a)}
              onEdit={() => handleEdit(a)}
              onDelete={() => handleDelete(a)}
              onViewPackages={() => setDetailFor(a)}
            />
          ))}
        </div>
      ) : (
        <div className="mt-5 card overflow-hidden">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-slate-100 text-left text-xs text-slate-400 dark:border-zinc-800">
                <th className="px-4 py-2.5 font-medium">账号</th>
                <th className="px-4 py-2.5 font-medium">版本</th>
                <th className="px-4 py-2.5 font-medium">余额</th>
                <th className="px-4 py-2.5 font-medium">token 到期</th>
                <th className="px-4 py-2.5 font-medium">状态</th>
                <th className="px-4 py-2.5 text-right font-medium">操作</th>
              </tr>
            </thead>
            <tbody>
              {accounts.map((a) => (
                <tr key={a.id} className="border-b border-slate-50 last:border-0 dark:border-zinc-800/60">
                  <td className="px-4 py-2.5">
                    <div className="font-medium">{a.nickname || a.id}</div>
                    <div className="font-mono text-xs text-slate-400">{a.uid.slice(0, 8)}…</div>
                  </td>
                  <td className="px-4 py-2.5">{a.edition_type || '—'}</td>
                  <td className="px-4 py-2.5 tabular-nums">{a.credits_balance?.toFixed(2) ?? '—'}</td>
                  <td className="px-4 py-2.5 text-xs">
                    {a.access_token_expires_at
                      ? new Date(a.access_token_expires_at * 1000).toLocaleDateString()
                      : '—'}
                  </td>
                  <td className="px-4 py-2.5">
                    {a.is_current ? <Badge tone="green">当前</Badge> : a.needs_relogin ? <Badge tone="red">需重登</Badge> : <Badge tone="slate">备用</Badge>}
                  </td>
                  <td className="px-4 py-2.5 text-right">
                    <button className="btn-outline !px-2 !py-1 text-xs" onClick={() => handleSwitch(a)} disabled={a.is_current}>
                      {a.is_current ? '当前' : '设为当前'}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* 导入预览确认弹框 */}
      <Modal
        open={scanPreview != null}
        onClose={() => setScanPreview(null)}
        title="导入本机账号"
        footer={
          <>
            <button className="btn-outline" onClick={() => setScanPreview(null)}>取消</button>
            <button className="btn-primary" onClick={() => void confirmImport()}>确认入池</button>
          </>
        }
      >
        <div className="space-y-1 text-sm">
          <div>检测到本机登录账号，确认导入账号池？</div>
          <div className="rounded-lg bg-slate-50 p-3 text-xs dark:bg-zinc-900">
            <div>昵称：{scanPreview?.nickname || '(未识别)'}</div>
            <div className="font-mono">uid：{scanPreview?.uid || '(未识别)'}</div>
            <div className="mt-1 text-slate-400">凭证将仅存本地（等同密码，全程掩码展示）</div>
          </div>
        </div>
      </Modal>

      {/* 删除确认弹框（禁 window.confirm，红线） */}
      <Modal
        open={deleteFor != null}
        onClose={() => setDeleteFor(null)}
        title="删除账号"
        footer={
          <>
            <button className="btn-outline" onClick={() => setDeleteFor(null)}>取消</button>
            <button className="btn-primary !bg-rose-600 hover:!bg-rose-500" onClick={() => void confirmDelete()}>确认删除</button>
          </>
        }
      >
        <div className="text-sm">
          确定删除账号「{deleteFor?.nickname || deleteFor?.id}」？
          <div className="mt-1 text-xs text-slate-400">仅移出账号池，不删除登录态快照与凭证文件。</div>
        </div>
      </Modal>

      {/* 编辑弹框（禁 window.prompt，红线） */}
      <Modal
        open={editFor != null}
        onClose={() => setEditFor(null)}
        title="编辑账号"
        footer={
          <>
            <button className="btn-outline" onClick={() => setEditFor(null)}>取消</button>
            <button className="btn-primary" onClick={() => void confirmEdit()}>保存</button>
          </>
        }
      >
        <label className="block text-sm">
          <span className="mb-1 block text-xs font-medium text-slate-500">显示名</span>
          <input
            className="input w-full"
            value={editName}
            onChange={(e) => setEditName(e.target.value)}
            placeholder="账号显示名"
          />
        </label>
      </Modal>

      {/* 积分包明细弹窗（F-56） */}
      <Modal
        open={detailFor != null}
        onClose={() => setDetailFor(null)}
        size="lg"
        bodyClass="max-h-[70vh] overflow-y-auto"
        title={`${detailFor?.nickname || detailFor?.id || ''} · 共 ${credits.get(detailFor?.id ?? '')?.length ?? 0} 个积分包`}
      >
        {(credits.get(detailFor?.id ?? '') ?? []).length === 0 ? (
          <p className="py-6 text-center text-xs text-slate-400">
            暂无积分包明细：请先在「积分与统计」页查询（需账号已录入凭证）
          </p>
        ) : (
          <div className="space-y-3">
            {(credits.get(detailFor?.id ?? '') ?? []).map((p, i) => (
              <div key={i} className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
                <div className="flex items-center justify-between gap-2">
                  <span className="truncate text-sm font-medium">{p.name}</span>
                  <span className="shrink-0 tabular-nums text-sm">
                    {(p.remaining ?? 0).toFixed(2)} / {(p.total ?? 0).toFixed(2)}
                    <span className="ml-2 text-xs text-slate-400">已用 {(p.used ?? 0).toFixed(2)}</span>
                  </span>
                </div>
                <div className="mt-1.5 flex items-center gap-2">
                  <span className={p.expire_soon ? 'text-xs text-rose-500' : 'text-xs text-slate-400'}>
                    {p.end_time ? `到期 ${p.end_time.slice(0, 10).replace(/-/g, '/')}` : '到期时间未知'}
                  </span>
                  {p.expire_soon && <Badge tone="red">即将到期</Badge>}
                </div>
                <div className="mt-2 h-2 w-full overflow-hidden rounded-full bg-slate-200 dark:bg-zinc-800">
                  <div
                    className="h-full rounded-full bg-emerald-500"
                    style={{ width: `${p.total > 0 ? Math.min(100, Math.round((p.remaining / p.total) * 100)) : 0}%` }}
                  />
                </div>
              </div>
            ))}
          </div>
        )}
      </Modal>
    </div>
  );
}
