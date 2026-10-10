import { useEffect, useMemo, useRef, useState } from 'react';
import {
  Download,
  Eye,
  Globe,
  HelpCircle,
  Loader2,
  Pencil,
  Plus,
  RefreshCw,
  ShieldAlert,
  Snowflake,
  Tags,
  Trash2,
  Upload,
  Zap,
} from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Badge, EmptyState, Modal } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import { withMinDelay } from '../lib/delay';
import { copyText } from '../lib/clipboard';
import type { AccountView, ImportPreview } from '../types';
import { AddAccountModal } from './accounts/AddAccountModal';
import { CooldownBadge } from './accounts/CooldownBadge';
import { CreditCell } from './accounts/CreditCell';
import { CreditsExpireBadge } from './accounts/CreditsExpireBadge';
import { DeleteAccountConfirmModal } from './accounts/DeleteConfirmModals';
import { EditAccountModal } from './accounts/EditAccountModal';
import { GroupSelect } from './accounts/GroupSelect';
import { GroupsModal } from './accounts/GroupsModal';
import { HelpModal } from './accounts/HelpModal';
import { ImportPreviewModal } from './accounts/ImportPreviewModal';
import { JwtStatusBadge } from './accounts/JwtStatusBadge';
import { JwtViewModal } from './accounts/JwtViewModal';
import { OAuthLoginModal } from './accounts/OAuthLoginModal';
import { PayIdentityBadge } from './accounts/PayIdentityBadge';
import { RefreshTokenBadge } from './accounts/RefreshTokenBadge';

export default function Accounts() {
  const accounts = useAppStore((s) => s.accounts);
  const groups = useAppStore((s) => s.groups);
  const refreshAccounts = useAppStore((s) => s.refreshAccounts);
  const refreshGroups = useAppStore((s) => s.refreshGroups);
  const addAccount = useAppStore((s) => s.addAccount);
  const deleteAccount = useAppStore((s) => s.deleteAccount);
  const updateAccount = useAppStore((s) => s.updateAccount);
  const createGroup = useAppStore((s) => s.createGroup);
  const updateGroup = useAppStore((s) => s.updateGroup);
  const removeGroup = useAppStore((s) => s.removeGroup);
  const moveAccount = useAppStore((s) => s.moveAccount);
  const refreshRemainingCredits = useAppStore((s) => s.refreshRemainingCredits);
  const cooldownClear = useAppStore((s) => s.cooldownClear);
  const refreshJwt = useAppStore((s) => s.refreshJwt);
  const toast = useAppStore((s) => s.pushToast);
  const oauthLogin = useAppStore((s) => s.oauthLogin);

  const [filter, setFilter] = useState<string>('all');
  const [addOpen, setAddOpen] = useState(false);
  const [groupOpen, setGroupOpen] = useState(false);
  const [editTarget, setEditTarget] = useState<AccountView | null>(null);
  const [jwtTarget, setJwtTarget] = useState<AccountView | null>(null);
  const [oauthOpen, setOAuthOpen] = useState(false);
  const [helpOpen, setHelpOpen] = useState(false);
  // 导入账号进行中（按钮 loading，防重复点击）
  const [importing, setImporting] = useState(false);
  // F-46 导入预览：文件内容 + 预览数据 + 勾选的账号下标
  const [importContent, setImportContent] = useState('');
  const [importPreviewData, setImportPreviewData] = useState<ImportPreview | null>(null);
  const [importSelected, setImportSelected] = useState<Set<number>>(new Set());
  // 删除确认（禁 window.confirm，红线）：删除账号
  const [deleteTarget, setDeleteTarget] = useState<AccountView | null>(null);
  // 导出凭证确认（审查 P0-2；禁 window.confirm，红线）：Trae 导出恒含明文 JWT / refreshToken
  const [exportConfirming, setExportConfirming] = useState(false);
  // 导出进行中（审查 #18 防双击）：Modal 不响应关闭，确认按钮禁用 + 旋转图标
  const [exportBusy, setExportBusy] = useState(false);
  // Web 版文件选择：隐藏 input 触发系统文件选择框
  const importInputRef = useRef<HTMLInputElement>(null);
  // 刷新防重入（审查 F3.1）：连点会对上游逐账号重复发积分查询（易频控）
  const [refreshBusy, setRefreshBusy] = useState(false);
  // 删除确认防连点（审查 F3.4）：1s withMinDelay 窗口内可二次触发（Buddy 版已修，Trae 版对齐）
  const [deleteBusy, setDeleteBusy] = useState(false);
  // 行内操作互斥（审查 F3.5，对齐 Buddy 版 rowOp）：解除冷却/刷新 JWT 期间禁重复触发
  const [rowOpUid, setRowOpUid] = useState<string | null>(null);

  const filtered = useMemo(() => {
    if (filter === 'all') return accounts;
    if (filter === 'ungrouped') return accounts.filter((a) => !a.group_id);
    return accounts.filter((a) => a.group_id === filter);
  }, [accounts, filter]);

  useEffect(() => {
    void refreshAccounts();
    void refreshGroups();
  }, [refreshAccounts, refreshGroups]);

  const onDelete = async (a: AccountView) => {
    setDeleteTarget(a);
  };

  const confirmDelete = async () => {
    if (!deleteTarget || deleteBusy) return;
    setDeleteBusy(true);
    try {
      await withMinDelay(deleteAccount(deleteTarget.user_id, true));
    } finally {
      setDeleteBusy(false);
      setDeleteTarget(null);
    }
  };

  /** 行内操作互斥执行（审查 F3.5）：同账号操作进行中再次点击直接忽略 */
  const runRowOp = async (uid: string, op: () => Promise<void>) => {
    if (rowOpUid) return;
    setRowOpUid(uid);
    try {
      await op();
    } finally {
      setRowOpUid(null);
    }
  };

  /** 刷新防重入（审查 F3.1） */
  const onRefresh = async () => {
    if (refreshBusy) return;
    setRefreshBusy(true);
    try {
      await Promise.all([refreshAccounts(), refreshRemainingCredits()]);
    } finally {
      setRefreshBusy(false);
    }
  };

  const copyJwt = async (jwt: string) => {
    if (await copyText(jwt)) {
      toast('success', 'JWT 已复制到剪贴板');
    } else {
      toast('error', '复制失败，请手动选择文本复制');
    }
  };

  const exportAccounts = () => {
    if (accounts.length === 0) {
      toast('warn', '没有账号可导出');
      return;
    }
    // 凭证导出强确认（审查 P0-2；禁 window.confirm，红线）：导出文件恒含明文 JWT / refreshToken
    setExportConfirming(true);
  };

  const doExportAccounts = async () => {
    if (exportBusy) return; // 审查 #18：防双击重复导出
    setExportBusy(true);
    try {
      const payload = await api.accounts.exportRaw();
      const content = JSON.stringify(payload, null, 2);
      const fileStamp = new Date().toISOString().slice(0, 19).replace(/[T:]/g, '-');
      // Web 版导出：Blob 下载（替代桌面 save 对话框）
      const blob = new Blob([content], { type: 'application/json' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `trae-accounts-${fileStamp}.json`;
      a.click();
      URL.revokeObjectURL(url);
      toast('success', `已导出 ${accounts.length} 个账号`);
    } catch (err) {
      toast('error', `导出失败：${String(err)}`);
    } finally {
      setExportBusy(false);
      setExportConfirming(false);
    }
  };

  // Web 版导入：选择文件读文本后走既有预览 + 导入流程
  const handleImportFile = async (file: File) => {
    if (importing) return;
    setImporting(true);
    try {
      const content = await file.text();
      // F-46：先预览解析，再由用户勾选确认后按索引导入
      const preview = await api.accounts.importPreview(content);
      if (preview.total === 0) {
        toast('warn', '导入文件中没有账号，请核对文件内容');
        return;
      }
      setImportContent(content);
      setImportPreviewData(preview);
      // 默认勾选：账号池中不存在且带 JWT 的账号；已存在的默认不勾
      setImportSelected(
        new Set(preview.accounts.filter((a) => !a.exists && a.has_jwt).map((a) => a.index)),
      );
    } catch (err) {
      toast('error', `读取导入文件失败：${String(err)}`);
    } finally {
      setImporting(false);
    }
  };

  const toggleImportItem = (index: number) => {
    setImportSelected((prev) => {
      const next = new Set(prev);
      if (next.has(index)) {
        next.delete(index);
      } else {
        next.add(index);
      }
      return next;
    });
  };

  const confirmImport = async () => {
    if (!importPreviewData || importing) return;
    const only = [...importSelected];
    if (only.length === 0) {
      toast('warn', '请至少勾选一个要导入的账号');
      return;
    }
    setImporting(true);
    try {
      const report = await api.accounts.importAccounts(importContent, only);
      toast(
        'success',
        `导入完成：新增 ${report.added} 个账号，跳过 ${report.skipped} 个重复${report.groups_added ? `，新增分组 ${report.groups_added} 个` : ''}`,
      );
      setImportPreviewData(null);
      setImportContent('');
      void refreshAccounts();
      void refreshGroups();
    } catch (err) {
      toast('error', `导入失败：${String(err)}`);
    } finally {
      setImporting(false);
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Trae · 账号管理"
        desc="多账号入池 · JWT 续期"
        leftExtra={
          <button
            onClick={() => setHelpOpen(true)}
            title="使用帮助"
            className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-sky-100 text-sky-600 shadow-sm transition hover:bg-sky-200 hover:shadow dark:bg-sky-500/15 dark:text-sky-300 dark:hover:bg-sky-500/25"
          >
            <HelpCircle size={17} />
          </button>
        }
        actions={
          <>
            <button onClick={() => void onRefresh()} disabled={refreshBusy} className="btn-outline" title="刷新账号列表与积分数据">
              {refreshBusy ? <Loader2 size={15} className="animate-spin" /> : <RefreshCw size={15} />} 刷新
            </button>
            <button onClick={() => setOAuthOpen(true)} className="btn-outline" title="通过 OAuth 授权登录添加账号">
              <Globe size={15} /> OAuth 登录
            </button>
            <button onClick={() => setAddOpen(true)} className="btn-outline" title="手动粘贴 JWT 添加账号">
              <Plus size={15} /> 添加账号
            </button>
            <button onClick={() => exportAccounts()} className="btn-outline" title="导出所有账号为 JSON 文件">
              <Download size={15} /> 导出账号
            </button>
            <button
              onClick={() => importInputRef.current?.click()}
              disabled={importing}
              className={importing ? 'btn-outline cursor-not-allowed opacity-60' : 'btn-outline'}
              title="从导出的 JSON 文件导入账号（自动去重）"
            >
              {importing ? <Loader2 size={15} className="animate-spin" /> : <Upload size={15} />} 导入账号
            </button>
            <button onClick={() => setGroupOpen(true)} className="btn-outline" title="管理账号分组">
              <Tags size={15} /> 分组管理
            </button>
          </>
        }
      />

      {/* 隐藏文件选择框：导入账号（Web 版，替代桌面 open 对话框） */}
      <input
        ref={importInputRef}
        type="file"
        accept=".json,application/json"
        className="hidden"
        onChange={(e) => {
          const f = e.target.files?.[0];
          e.target.value = '';
          if (f) void handleImportFile(f);
        }}
      />

      <div className="mb-3 flex flex-wrap items-center gap-2 text-sm">
        <button
          onClick={() => setFilter('all')}
          className={`chip border ${filter === 'all' ? 'border-brand-500 text-brand-600' : 'border-slate-300 text-slate-500'}`}
        >
          全部 ({accounts.length})
        </button>
        <button
          onClick={() => setFilter('ungrouped')}
          className={`chip border ${filter === 'ungrouped' ? 'border-brand-500 text-brand-600' : 'border-slate-300 text-slate-500'}`}
        >
          未分组 ({accounts.filter((a) => !a.group_id).length})
        </button>
        {groups.map((g) => (
          <button
            key={g.id}
            onClick={() => setFilter(g.id)}
            className={`chip border ${filter === g.id ? 'border-brand-500 text-brand-600' : 'border-slate-300 text-slate-500'}`}
            style={{ borderColor: filter === g.id ? g.color : undefined }}
          >
            <span className="inline-block h-2 w-2 rounded-full" style={{ background: g.color }} />
            {g.name} ({g.count})
          </button>
        ))}
      </div>

      <div className="card overflow-hidden">
        {filtered.length === 0 ? (
          <div className="p-6">
            <EmptyState
              icon={<Plus size={28} />}
              title={filter === 'all' ? '还没有账号' : '此分组下没有账号'}
              hint="点击右上角「添加账号」粘贴 JWT，或使用 OAuth 登录授权添加。"
            />
          </div>
        ) : (
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-left">分组</th>
                <th className="px-4 py-2 text-left">JWT</th>
                <th className="px-4 py-2 text-left">设备 ID</th>
                <th className="px-4 py-2 text-left">今日</th>
                <th className="px-4 py-2 text-left">冷却</th>
                <th className="px-4 py-2 text-right">可用积分</th>
                <th className="px-4 py-2 text-left">积分过期</th>
                <th className="px-4 py-2 text-right">今日新增积分</th>
                <th className="px-4 py-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              {filtered.map((a) => {
                return (
                  <tr key={a.user_id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                    <td className="px-4 py-3">
                      <div className="flex items-center gap-1.5">
                        <span className="font-medium">{a.name}</span>
                        <PayIdentityBadge
                          identity={a.pay_identity}
                          expire={a.membership_expire}
                          nextBilling={a.membership_next_billing}
                        />
                      </div>
                      <div className="text-xs text-slate-400">{a.user_id}</div>
                    </td>
                    <td className="px-4 py-3">
                      <GroupSelect
                        value={a.group_id}
                        groups={groups}
                        onChange={(gid) => void moveAccount(a.user_id, gid)}
                      />
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex items-center gap-1">
                        <JwtStatusBadge hours={a.jwt_exp_hours} />
                        {a.has_refresh_token && (
                          <span title="支持自动刷新" className="text-sky-500">
                            <Zap size={12} />
                          </span>
                        )}
                        {/* F-78 批次 3：refresh_token 生命周期（失效/连续失败/即将过期）+ 凭证保存时间；missing=历史导入无凭证（issue #76 痛点②） */}
                        <RefreshTokenBadge
                          invalid={a.refresh_token_invalid}
                          fails={a.refresh_token_fails}
                          expiresAt={a.refresh_token_expires_at}
                          savedAt={a.auth_saved_at}
                          missing={!a.has_refresh_token}
                        />
                        <button
                          title="查看 JWT"
                          onClick={() => setJwtTarget(a)}
                          className="btn-ghost !p-1"
                        >
                          <Eye size={13} />
                        </button>
                      </div>
                    </td>
                    <td className="px-4 py-3 font-mono text-xs text-slate-500">{a.device_id_masked ?? '-'}</td>
                    <td className="px-4 py-3">
                      {a.checked_today ? (
                        <Badge tone="green">已签</Badge>
                      ) : (
                        <Badge tone="slate">未签</Badge>
                      )}
                    </td>
                    <td className="px-4 py-3">
                      {a.cooldown_type ? (
                        <CooldownBadge type={a.cooldown_type} until={a.cooldown_until} />
                      ) : (
                        <span className="text-xs text-slate-300">-</span>
                      )}
                    </td>
                    <td className="px-4 py-3 text-right">
                      <CreditCell account={a} />
                    </td>
                    <td className="px-4 py-3">
                      <CreditsExpireBadge expireAt={a.credits_expire_at} />
                    </td>
                    <td className="px-4 py-3 text-right tabular-nums">
                      {a.credits != null ? a.credits.toLocaleString() : '-'}
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex justify-end gap-1">
                        <button title="编辑账号" onClick={() => setEditTarget(a)} className="btn-ghost !p-2">
                          <Pencil size={14} />
                        </button>
                        {a.cooldown_type && (
                          <button
                            title="解除冷却"
                            onClick={() => void runRowOp(a.user_id, () => cooldownClear(a.user_id))}
                            disabled={rowOpUid !== null}
                            className="btn-ghost !p-2 text-sky-500 hover:bg-sky-50 disabled:cursor-not-allowed disabled:opacity-50 dark:hover:bg-sky-500/10"
                          >
                            <Snowflake size={14} />
                          </button>
                        )}
                        {a.has_refresh_token && (
                          <button
                            title="刷新 JWT"
                            onClick={() => void runRowOp(a.user_id, () => refreshJwt(a.user_id))}
                            disabled={rowOpUid !== null}
                            className="btn-ghost !p-2 text-sky-500 hover:bg-sky-50 disabled:cursor-not-allowed disabled:opacity-50 dark:hover:bg-sky-500/10"
                          >
                            <Zap size={14} />
                          </button>
                        )}
                        <button title="删除" onClick={() => void onDelete(a)} className="btn-ghost !p-2 text-rose-500 hover:bg-rose-50 dark:hover:bg-rose-500/10">
                          <Trash2 size={14} />
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>

      <AddAccountModal
        open={addOpen}
        onClose={() => setAddOpen(false)}
        groups={groups}
        onSubmit={async (name, jwt, gid) => {
          try {
            await addAccount(name, jwt, gid);
            setAddOpen(false);
          } catch {
            /* toast 已发出 */
          }
        }}
      />
      <EditAccountModal
        account={editTarget}
        onClose={() => setEditTarget(null)}
        onSubmit={async (name, jwt) => {
          if (!editTarget) return;
          try {
            await updateAccount(editTarget.user_id, name, jwt);
            setEditTarget(null);
          } catch {
            /* toast 已发出 */
          }
        }}
      />
      <JwtViewModal
        account={jwtTarget}
        onClose={() => setJwtTarget(null)}
        onCopy={copyJwt}
      />
      <GroupsModal
        open={groupOpen}
        onClose={() => setGroupOpen(false)}
        groups={groups}
        onCreate={async (name, color) => {
          await createGroup(name, color);
        }}
        onRename={async (id, name) => {
          await updateGroup(id, { name });
        }}
        onRecolor={async (id, color) => {
          await updateGroup(id, { color });
        }}
        onDelete={async (id) => {
          await removeGroup(id);
        }}
      />
      <DeleteAccountConfirmModal
        target={deleteTarget}
        onClose={() => setDeleteTarget(null)}
        onConfirm={() => void confirmDelete()}
      />
      {/* 导出凭证确认弹框（审查 P0-2；禁 window.confirm，红线） */}
      <Modal
        open={exportConfirming}
        onClose={() => {
          if (!exportBusy) setExportConfirming(false); // 审查 #18：导出中禁止关闭
        }}
        title="确认导出明文凭证"
        footer={
          <>
            <button className="btn-outline" disabled={exportBusy} onClick={() => setExportConfirming(false)}>
              取消
            </button>
            <button
              className="btn-primary !bg-rose-600 hover:!bg-rose-500"
              disabled={exportBusy}
              onClick={() => void doExportAccounts()}
            >
              {exportBusy ? <Loader2 size={14} className="animate-spin" /> : null} 我已知晓风险，继续导出
            </button>
          </>
        }
      >
        <div className="space-y-3 text-sm">
          <div>即将导出 {accounts.length} 个账号，文件包含明文 JWT / refreshToken 凭证（等同密码）。</div>
          <div className="flex items-start gap-2 rounded-lg border border-rose-200 bg-rose-50 p-3 text-xs text-rose-700 dark:border-rose-500/30 dark:bg-rose-500/10 dark:text-rose-300">
            <ShieldAlert size={14} className="mt-0.5 shrink-0" />
            <span>仅应在可信环境用于账号迁移，导出后请妥善保管，切勿通过不可信渠道传输。</span>
          </div>
        </div>
      </Modal>
      <OAuthLoginModal
        open={oauthOpen}
        onClose={() => setOAuthOpen(false)}
        groups={groups}
        onLogin={async (callbackUrl, accountName, groupId) => {
          try {
            await oauthLogin(callbackUrl, accountName, groupId);
            setOAuthOpen(false);
          } catch {
            /* toast 已发出 */
          }
        }}
      />
      <HelpModal open={helpOpen} onClose={() => setHelpOpen(false)} />
      <ImportPreviewModal
        preview={importPreviewData}
        selected={importSelected}
        importing={importing}
        onClose={() => setImportPreviewData(null)}
        onToggle={toggleImportItem}
        onSelectOnly={setImportSelected}
        onConfirm={() => void confirmImport()}
      />
    </div>
  );
}
