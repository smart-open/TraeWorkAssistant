import { useCallback, useEffect, useRef, useState } from 'react';
import {
  RefreshCw,
  Download,
  Upload,
  UserPlus,
  ScanLine,
  ShieldAlert,
  LogIn,
  Save,
  KeyRound,
  TerminalSquare,
  DatabaseBackup,
  ArchiveRestore,
  Copy,
  Pencil,
  Trash2,
  Loader2,
  Coins,
  Users,
} from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState, Modal, Spinner } from '../../components/ui';
import { listen } from '@tauri-apps/api/event';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type {
  WorkBuddyAccountView,
  WbCreditPackage,
  WbCreditsResult,
  WbOauthDone,
  WbOauthProgress,
  WbResetItem,
  WbResetResult,
} from '../../types';

/**
 * buddy-accounts 账号管理（§3.7.2，F-54/F-56/F-60/F-50/F-14）：
 * 列表式账号池（对齐 Trae 账号管理）+ 聚合迁移入口 + 积分包明细弹窗 + 环境重置。
 */

function maskUid(uid: string): string {
  if (uid.length <= 14) return uid || '—';
  return `${uid.slice(0, 8)}…${uid.slice(-6)}`;
}

function fmtExpire(ts: number | null): string {
  if (!ts) return '—';
  const d = new Date(ts * 1000);
  return `${d.getFullYear()}/${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')}`;
}

function tokenTone(ts: number | null): string {
  if (ts == null) return 'text-slate-400';
  const days = (ts * 1000 - Date.now()) / 86400000;
  if (days < 0) return 'text-rose-500';
  if (days < 1) return 'text-amber-500';
  return 'text-emerald-600 dark:text-emerald-400';
}

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
  const [restoreFor, setRestoreFor] = useState<WorkBuddyAccountView | null>(null);
  const [copyFor, setCopyFor] = useState<WorkBuddyAccountView | null>(null);
  const [copyTarget, setCopyTarget] = useState('');
  const [exportOpen, setExportOpen] = useState(false);
  const [exportWithCreds, setExportWithCreds] = useState(false);
  const importFileRef = useRef<HTMLInputElement>(null);
  const [editFor, setEditFor] = useState<WorkBuddyAccountView | null>(null);
  const [editName, setEditName] = useState('');
  // OAuth 扫码（F-50）：事件驱动弹框（后端全流程，进度经 wb-oauth-progress / 结果 wb-oauth-done）
  const [oauthOpen, setOauthOpen] = useState(false);
  const [oauthStage, setOauthStage] = useState('');
  const [oauthMessage, setOauthMessage] = useState('');
  const [oauthAuthUrl, setOauthAuthUrl] = useState<string | null>(null);
  // 环境重置（F-14）：16 项勾选预览 → 二次确认 → 执行结果
  const [resetOpen, setResetOpen] = useState(false);
  const [resetItems, setResetItems] = useState<WbResetItem[]>([]);
  const [resetChecked, setResetChecked] = useState<Set<string>>(new Set());
  const [resetKeycloak, setResetKeycloak] = useState(true);
  const [resetConfirming, setResetConfirming] = useState(false);
  const [resetBusy, setResetBusy] = useState(false);
  const [resetResults, setResetResults] = useState<WbResetResult[] | null>(null);
  // CodeBuddy CLI 当前号（settings.json token → 池 id），列表内徽标展示
  const [cliActiveId, setCliActiveId] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const accs = await api.workbuddy.accountsList();
      setAccounts(accs);
      // CLI 当前号查询（失败不阻断列表展示；切号/删号后 refresh 会自动重取）
      api.workbuddy
        .cliStatus()
        .then((s) => setCliActiveId(s.active_account_id))
        .catch(() => setCliActiveId(null));
      // 积分缓存查询（≥5min 缓存，失败不阻断列表展示）
      api.workbuddy
        .creditsFetch()
        .then((r: WbCreditsResult) => {
          const m = new Map<string, WbCreditPackage[]>();
          for (const a of r.accounts) m.set(a.user_id, a.packages ?? []);
          setCredits(m);
        })
        .catch(() => pushToast('warn', '积分缓存查询失败，积分包列为空'));
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

  // OAuth 事件监听（F-50）：仅弹框打开期间订阅，关闭即解绑
  useEffect(() => {
    if (!oauthOpen) return;
    const un1 = listen<WbOauthProgress>('wb-oauth-progress', (e) => {
      setOauthStage(e.payload.stage);
      setOauthMessage(e.payload.message);
      if (e.payload.auth_url) setOauthAuthUrl(e.payload.auth_url);
    });
    const un2 = listen<WbOauthDone>('wb-oauth-done', (e) => {
      if (e.payload.ok) {
        setOauthStage('success');
        setOauthMessage(e.payload.message);
        pushToast('success', e.payload.message);
        void refresh();
        setTimeout(() => setOauthOpen(false), 1200);
      } else {
        setOauthStage('error');
        setOauthMessage(e.payload.message);
      }
    });
    return () => {
      void un1.then((f) => f());
      void un2.then((f) => f());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [oauthOpen]);

  // 发起 OAuth 扫码（后端打开浏览器 + 轮询 + 自动入池）
  const startOauth = async () => {
    setOauthStage('init');
    setOauthMessage('正在发起扫码登录…');
    setOauthAuthUrl(null);
    setOauthOpen(true);
    try {
      await api.workbuddy.oauthLogin();
    } catch (err) {
      setOauthStage('error');
      setOauthMessage(String(err));
    }
  };

  // 打开环境重置弹框：拉取 16 项清单（默认勾选所有存在项）
  const openEnvReset = async () => {
    try {
      const items = await api.workbuddy.envResetItems();
      setResetItems(items);
      setResetChecked(new Set(items.filter((x) => x.exists).map((x) => x.id)));
      setResetResults(null);
      setResetConfirming(false);
      setResetOpen(true);
    } catch (err) {
      pushToast('error', `读取清理清单失败：${String(err)}`);
    }
  };

  const toggleResetItem = (id: string) => {
    setResetChecked((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  // 执行环境重置（二次确认后；单项失败不中断）
  const confirmEnvReset = async () => {
    setResetBusy(true);
    try {
      const results = await withMinDelay(
        api.workbuddy.envReset([...resetChecked], resetKeycloak),
        1500,
      );
      setResetResults(results);
      setResetConfirming(false);
      const fail = results.filter((r) => !r.ok).length;
      if (fail === 0) pushToast('success', `环境重置完成（${results.length} 项全部成功）`);
      else pushToast('warn', `环境重置完成，${fail} 项失败，请查看详情`);
      await refresh();
    } catch (err) {
      pushToast('error', `环境重置失败：${String(err)}`);
    } finally {
      setResetBusy(false);
    }
  };

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

  // 设为 CLI 账号（F-06）：token store 凭证 → ~/.codebuddy/settings.json env
  const handleSetCli = async (a: WorkBuddyAccountView) => {
    try {
      await withMinDelay(api.workbuddy.cliBridgeSet(a.id), 800);
      pushToast('success', `「${a.nickname || a.id}」已设为 CodeBuddy CLI 账号（重启 CLI 后生效）`);
      void refresh(); // 更新列表内「CodeBuddy 当前号」徽标
    } catch (err) {
      pushToast('error', `CLI 桥接失败：${String(err)}`);
    }
  };

  // 会话三件套备份（F-44）：projects + 双 db 快照；执行前自动关闭 WorkBuddy
  const handleBackupChats = async (a: WorkBuddyAccountView) => {
    try {
      const r = await withMinDelay(api.workbuddy.chatdataBackup(a.id), 1200);
      pushToast('success', `「${a.nickname || a.id}」会话已备份（${r.files} 个文件）`);
    } catch (err) {
      pushToast('error', `会话备份失败：${String(err)}`);
    }
  };

  // 恢复会话（二次确认后执行；覆盖现有 ~/.workbuddy 会话数据）
  const confirmRestoreChats = async () => {
    if (!restoreFor) return;
    try {
      const r = await withMinDelay(api.workbuddy.chatdataRestore(restoreFor.id), 1200);
      pushToast('success', `会话已恢复（${r.files} 个文件）；原有数据保留于 ~/.workbuddy/*.bak`);
    } catch (err) {
      pushToast('error', `会话恢复失败：${String(err)}`);
    } finally {
      setRestoreFor(null);
    }
  };

  // 复制会话到目标账号（F-45）：新 id 复制 + 云端映射注册
  const confirmCopyChats = async () => {
    if (!copyFor || !copyTarget) return;
    try {
      const r = await withMinDelay(api.workbuddy.chatdataCopy(copyFor.id, copyTarget), 1500);
      pushToast(
        'success',
        `已复制 ${r.copied} 个会话（sessions 克隆 ${r.sessions_cloned}，云端映射 ${r.mappings_registered}）到目标账号`,
      );
      setCopyFor(null);
    } catch (err) {
      pushToast('error', `会话复制失败：${String(err)}`);
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
    setExportOpen(true);
  };

  // 导出确认（F-46 扩展）：可选是否附带凭证副本（迁移场景用）
  const confirmExport = async () => {
    try {
      const data = await api.workbuddy.accountsExport(exportWithCreds);
      const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `workbuddy_accounts_${new Date().toISOString().slice(0, 10)}.json`;
      a.click();
      URL.revokeObjectURL(url);
      pushToast(
        'success',
        exportWithCreds ? '账号池已导出（含凭证，文件等同密码请妥善保管）' : '账号元数据已导出（凭证不导出）',
      );
      setExportOpen(false);
    } catch (err) {
      pushToast('error', `导出失败：${String(err)}`);
    }
  };

  // 导入备份（F-46 扩展）：选择导出文件 → 解析入池（含凭证回写）
  const importBackupFile = async (file: File) => {
    try {
      const text = await file.text();
      const payload = JSON.parse(text) as Record<string, unknown>;
      const r = await withMinDelay(api.workbuddy.accountsImport(payload), 1000);
      pushToast('success', `导入完成：新增 ${r.added}，跳过 ${r.skipped}（已在池中），带凭证 ${r.with_credentials}`);
      await refresh();
    } catch (err) {
      pushToast('error', `导入失败：${String(err)}`);
    }
  };

  const busy = !!switchingTo || !!savingLogin;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 账号管理"
        desc="多账号入池 · 切换登录 · 凭证续期"
        actions={
          <>
            <button onClick={() => void refresh()} className="btn-outline" disabled={loading}>
              <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 刷新
            </button>
            <button className="btn-outline" onClick={() => void importFromAuth()} disabled={importing}>
              {importing ? <Spinner /> : <ScanLine size={15} />} 导入本机账号
            </button>
            <button className="btn-outline" onClick={() => void startOauth()}>
              <UserPlus size={15} /> OAuth 扫码
            </button>
            <button className="btn-outline" onClick={exportPool} disabled={accounts.length === 0}>
              <Download size={15} /> 导出
            </button>
            <button className="btn-outline" onClick={() => importFileRef.current?.click()}>
              <Upload size={15} /> 导入备份
            </button>
            <input
              ref={importFileRef}
              type="file"
              accept=".json,application/json"
              className="hidden"
              onChange={(e) => {
                const f = e.target.files?.[0];
                if (f) void importBackupFile(f);
                e.target.value = '';
              }}
            />
            <button className="btn-outline !text-rose-600 hover:!border-rose-300" onClick={() => void openEnvReset()}>
              <ShieldAlert size={15} /> 环境重置
            </button>
          </>
        }
      />

      {/* 账号列表（对齐 Trae 账号管理表格） */}
      {accounts.length === 0 ? (
        <div className="mt-5">
          <EmptyState
            icon={<Users size={26} />}
            title="暂无 WorkBuddy 账号"
            hint="先在 WorkBuddy 客户端登录，然后点击上方「导入本机账号」自动扫描入池；切换/保存登录态会在账号管理中生成快照。"
          />
        </div>
      ) : (
        <div className="mt-5 card overflow-x-auto">
          <table className="w-full min-w-[860px] text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-left">版本</th>
                <th className="px-4 py-2 text-right">可用积分</th>
                <th className="px-4 py-2 text-left">token 到期</th>
                <th className="px-4 py-2 text-left">积分包</th>
                <th className="px-4 py-2 text-left">状态</th>
                <th className="px-4 py-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              {accounts.map((a) => {
                const pkgs = credits.get(a.id) ?? [];
                const switching = switchingTo === a.id || savingLogin === a.id;
                return (
                  <tr key={a.id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                    <td className="px-4 py-3">
                      <div className="flex items-center gap-1.5">
                        <span className="font-medium">{a.nickname || a.id}</span>
                        {a.is_current && <Badge tone="green">当前</Badge>}
                        {a.needs_relogin && (
                          <span title={a.relogin_reason} className="text-rose-500">
                            <ShieldAlert size={12} />
                          </span>
                        )}
                      </div>
                      <div className="font-mono text-xs text-slate-400">{maskUid(a.uid)}</div>
                    </td>
                    <td className="px-4 py-3">
                      {a.edition_type ? (
                        <Badge tone={a.edition_type.toLowerCase() === 'pro' ? 'blue' : 'slate'}>{a.edition_type}</Badge>
                      ) : (
                        <span className="text-xs text-slate-300">-</span>
                      )}
                    </td>
                    <td className="px-4 py-3 text-right tabular-nums">
                      {a.credits_balance != null ? a.credits_balance.toFixed(2) : '-'}
                    </td>
                    <td className="px-4 py-3 text-xs">
                      <div className={tokenTone(a.access_token_expires_at)}>{fmtExpire(a.access_token_expires_at)}</div>
                      {a.refresh_token_expires_at && (
                        <div className="text-slate-400">RT {fmtExpire(a.refresh_token_expires_at)}</div>
                      )}
                    </td>
                    <td className="px-4 py-3">
                      {pkgs.length > 0 ? (
                        <button
                          className="flex items-center gap-1 text-xs text-brand-600 hover:underline dark:text-brand-400"
                          onClick={() => setDetailFor(a)}
                        >
                          <Coins size={13} /> {pkgs.length} 个包
                        </button>
                      ) : (
                        <span className="text-xs text-slate-300">-</span>
                      )}
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex items-center gap-1">
                        {a.is_current ? (
                          <Badge tone="green">在线</Badge>
                        ) : a.needs_relogin ? (
                          <Badge tone="red">需重登</Badge>
                        ) : (
                          <Badge tone="slate">备用</Badge>
                        )}
                        {cliActiveId === a.id && (
                          <Badge tone="blue" title="CodeBuddy CLI 当前账号（~/.codebuddy/settings.json）">
                            <TerminalSquare size={12} /> CodeBuddy
                          </Badge>
                        )}
                      </div>
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex justify-end gap-1">
                        <button
                          title={a.is_current ? '当前在线账号' : a.needs_relogin ? '需重新登录后才能切换' : '设为当前（切换客户端登录）'}
                          onClick={() => handleSwitch(a)}
                          disabled={a.is_current || busy || a.needs_relogin}
                          className={`btn-ghost !p-2 ${switching ? 'text-amber-500' : 'text-emerald-600 dark:text-emerald-400'} ${(a.is_current || busy || a.needs_relogin) && !switching ? 'opacity-40 cursor-not-allowed' : ''}`}
                        >
                          {switching ? <Loader2 size={14} className="animate-spin" /> : <LogIn size={14} />}
                        </button>
                        <button
                          title="保存当前登录态（快照客户端当前登录到该账号）"
                          onClick={() => handleSave(a)}
                          disabled={busy}
                          className="btn-ghost !p-2"
                        >
                          <Save size={14} />
                        </button>
                        <button
                          title="凭证续期（refreshToken 换新 accessToken）"
                          onClick={() => void handleRefreshToken(a)}
                          disabled={!a.has_credential}
                          className="btn-ghost !p-2 text-amber-500 hover:bg-amber-50 dark:hover:bg-amber-500/10"
                        >
                          <KeyRound size={14} />
                        </button>
                        <button
                          title={a.has_credential ? '设为 CodeBuddy CLI 账号（写入 ~/.codebuddy/settings.json）' : '需先导入凭证副本'}
                          onClick={() => void handleSetCli(a)}
                          disabled={!a.has_credential}
                          className="btn-ghost !p-2 text-sky-500 hover:bg-sky-50 dark:hover:bg-sky-500/10"
                        >
                          <TerminalSquare size={14} />
                        </button>
                        <button
                          title="备份会话（projects + 双 db 快照）"
                          onClick={() => void handleBackupChats(a)}
                          className="btn-ghost !p-2"
                        >
                          <DatabaseBackup size={14} />
                        </button>
                        <button
                          title="从备份恢复会话"
                          onClick={() => setRestoreFor(a)}
                          className="btn-ghost !p-2"
                        >
                          <ArchiveRestore size={14} />
                        </button>
                        <button
                          title="复制会话到其他账号"
                          onClick={() => {
                            setCopyFor(a);
                            setCopyTarget('');
                          }}
                          className="btn-ghost !p-2"
                        >
                          <Copy size={14} />
                        </button>
                        <button title="编辑账号" onClick={() => handleEdit(a)} className="btn-ghost !p-2">
                          <Pencil size={14} />
                        </button>
                        <button
                          title="删除账号"
                          onClick={() => handleDelete(a)}
                          className="btn-ghost !p-2 text-rose-500 hover:bg-rose-50 dark:hover:bg-rose-500/10"
                        >
                          <Trash2 size={14} />
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
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

      {/* 会话恢复确认弹框（F-44，覆盖性操作二次确认） */}
      <Modal
        open={restoreFor != null}
        onClose={() => setRestoreFor(null)}
        title="恢复会话数据"
        footer={
          <>
            <button className="btn-outline" onClick={() => setRestoreFor(null)}>取消</button>
            <button className="btn-primary !bg-amber-600 hover:!bg-amber-500" onClick={() => void confirmRestoreChats()}>确认恢复</button>
          </>
        }
      >
        <div className="space-y-1 text-sm">
          <div>把「{restoreFor?.nickname || restoreFor?.id}」的会话备份恢复到 ~/.workbuddy？</div>
          <div className="rounded-lg bg-amber-50 p-3 text-xs text-amber-700 dark:bg-amber-500/10 dark:text-amber-300">
            将覆盖现有会话数据（原数据自动保留为 .bak）；执行时会先自动关闭 WorkBuddy 客户端。
          </div>
        </div>
      </Modal>

      {/* 会话复制弹框（F-45：选择目标账号） */}
      <Modal
        open={copyFor != null}
        onClose={() => setCopyFor(null)}
        title={`复制会话 · ${copyFor?.nickname || copyFor?.id || ''}`}
        footer={
          <>
            <button className="btn-outline" onClick={() => setCopyFor(null)}>取消</button>
            <button className="btn-primary" onClick={() => void confirmCopyChats()} disabled={!copyTarget}>开始复制</button>
          </>
        }
      >
        <div className="space-y-2 text-sm">
          <div>选择目标账号：源会话将以全新会话 id 复制过去，并注册到目标账号的云端映射（复制前自动快照数据库）。</div>
          <select className="input w-full" value={copyTarget} onChange={(e) => setCopyTarget(e.target.value)}>
            <option value="">— 选择目标账号 —</option>
            {accounts
              .filter((x) => x.id !== copyFor?.id)
              .map((x) => (
                <option key={x.id} value={x.id}>
                  {x.nickname || x.id}
                </option>
              ))}
          </select>
          <div className="text-xs text-slate-400">
            数据来源：该账号的会话备份（如有）或当前 ~/.workbuddy/projects；执行时会先自动关闭 WorkBuddy 客户端。
          </div>
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

      {/* 导出选项弹框（F-46 扩展：凭证是否随导出） */}
      <Modal
        open={exportOpen}
        onClose={() => setExportOpen(false)}
        title="导出账号池"
        footer={
          <>
            <button className="btn-outline" onClick={() => setExportOpen(false)}>取消</button>
            <button className="btn-primary" onClick={() => void confirmExport()}>确认导出</button>
          </>
        }
      >
        <div className="space-y-3 text-sm">
          <div>导出账号池为 JSON 文件，可用于备份或迁移到其他设备。</div>
          <label className="flex items-start gap-2">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={exportWithCreds}
              onChange={(e) => setExportWithCreds(e.target.checked)}
            />
            <span>
              包含凭证副本（refreshToken / accessToken）
              <span className="mt-0.5 block text-xs text-amber-600 dark:text-amber-400">
                含凭证的导出文件等同密码：仅用于本机迁移，请勿分享；不含凭证的导出仅恢复元数据（需重新续期登录）。
              </span>
            </span>
          </label>
        </div>
      </Modal>

      {/* OAuth 扫码弹框（F-50：后端全流程，事件驱动进度展示） */}
      <Modal
        open={oauthOpen}
        onClose={() => setOauthOpen(false)}
        title="OAuth 扫码登录"
      >
        <div className="space-y-3 text-sm">
          <div className="flex items-center gap-2">
            {oauthStage === 'success' ? (
              <Badge tone="green">完成</Badge>
            ) : oauthStage === 'error' ? (
              <Badge tone="red">失败</Badge>
            ) : (
              <>
                <Spinner />
                <span className="text-xs text-slate-400">流程进行中（最长 300 秒）</span>
              </>
            )}
          </div>
          <div className="rounded-lg bg-slate-50 p-3 text-xs dark:bg-zinc-900">{oauthMessage || '等待开始…'}</div>
          {oauthAuthUrl && oauthStage !== 'success' && (
            <div className="text-xs text-slate-400">
              未自动打开浏览器？<span className="font-mono break-all">{oauthAuthUrl.slice(0, 80)}…</span>
            </div>
          )}
          <div className="text-xs text-slate-400">
            登录成功后账号自动入池，凭证仅写入本地 token store（全程掩码，不上传）。
          </div>
        </div>
      </Modal>

      {/* 环境重置弹框（F-14：16 项勾选预览 + 二次确认 + Keycloak 注销开关） */}
      <Modal
        open={resetOpen}
        onClose={() => setResetOpen(false)}
        size="lg"
        bodyClass="max-h-[75vh] overflow-y-auto"
        title="环境重置 / 彻底登出"
      >
        {resetResults ? (
          <div className="space-y-2 text-sm">
            <div className="font-medium">执行结果</div>
            {resetResults.map((r) => (
              <div key={r.id} className="flex items-start gap-2 rounded-lg bg-slate-50 p-2.5 text-xs dark:bg-zinc-900">
                {r.ok ? <Badge tone="green">成功</Badge> : <Badge tone="red">失败</Badge>}
                <div>
                  <div className="font-medium">{resetItems.find((x) => x.id === r.id)?.label ?? r.id}</div>
                  <div className="mt-0.5 text-slate-500">{r.detail}</div>
                </div>
              </div>
            ))}
          </div>
        ) : (
          <div className="space-y-3 text-sm">
            <div className="rounded-lg bg-amber-50 p-3 text-xs text-amber-700 dark:bg-amber-500/10 dark:text-amber-300">
              将清除本机 WorkBuddy 的全部认证残留（客户端回到未登录态）。执行时会自动关闭 WorkBuddy；
              账号池与已备份的会话数据不受影响。请逐项确认：
            </div>
            <div className="space-y-1.5">
              {resetItems.map((item, i) => (
                <label key={item.id} className="flex items-start gap-2 rounded-lg border border-slate-100 p-2.5 dark:border-zinc-800">
                  <input
                    type="checkbox"
                    className="mt-0.5"
                    checked={resetChecked.has(item.id)}
                    onChange={() => toggleResetItem(item.id)}
                  />
                  <span className="min-w-0">
                    <span className="text-xs font-medium">
                      {String(i + 1).padStart(2, '0')}. {item.label}
                    </span>
                    {!item.exists && <Badge tone="slate">未检测到</Badge>}
                    <span className="mt-0.5 block text-xs text-slate-400">{item.detail}</span>
                  </span>
                </label>
              ))}
            </div>
            <label className="flex items-start gap-2">
              <input type="checkbox" className="mt-0.5" checked={resetKeycloak} onChange={(e) => setResetKeycloak(e.target.checked)} />
              <span>
                同时注销 Keycloak SSO 会话（浏览器打开注销页，需当前 accessToken）
                <span className="mt-0.5 block text-xs text-slate-400">先于清理执行；找不到可用凭证时自动跳过。</span>
              </span>
            </label>
            {!resetConfirming ? (
              <button
                className="btn-outline !border-rose-300 !text-rose-600"
                disabled={resetChecked.size === 0 && !resetKeycloak}
                onClick={() => setResetConfirming(true)}
              >
                执行清理（已选 {resetChecked.size + (resetKeycloak ? 1 : 0)} 项）
              </button>
            ) : (
              <div className="flex items-center gap-2">
                <span className="text-xs font-medium text-rose-600">⚠️ 不可逆操作：所选残留将被永久清除，确认执行？</span>
                <button className="btn-outline" onClick={() => setResetConfirming(false)} disabled={resetBusy}>
                  再想想
                </button>
                <button
                  className="btn-primary !bg-rose-600 hover:!bg-rose-500"
                  onClick={() => void confirmEnvReset()}
                  disabled={resetBusy}
                >
                  {resetBusy ? <Spinner /> : null} 确认执行
                </button>
              </div>
            )}
          </div>
        )}
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
            暂无积分包明细：请先在「积分看板」页查询（需账号已录入凭证）
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
