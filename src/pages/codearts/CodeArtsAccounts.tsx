import { useCallback, useEffect, useState } from 'react';
import {
  KeyRound,
  Loader2,
  Pencil,
  PlayCircle,
  RefreshCw,
  Trash2,
  UserPlus,
  Users,
} from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState, Modal } from '../../components/ui';
import { api } from '../../lib/tauri';
import { GroupSelect } from '../accounts/GroupSelect';
import { useAppStore } from '../../store';
import type { CodeartsAccountView, GroupView } from '../../types';
import { listen } from '@tauri-apps/api/event';

/**
 * codearts-accounts 账号管理：账号列表 + AK/SK 手工录入 + 华为云 OAuth 登录
 * （PKCE+DPoP，系统浏览器授权不抢占窗口）+ 改名/备注/分组 + 单账号重试签到。
 */
type ConfirmTarget = { kind: 'remove'; account: CodeartsAccountView } | null;

export default function CodeArtsAccounts() {
  const pushToast = useAppStore((s) => s.pushToast);
  const startCodeartsCheckin = useAppStore((s) => s.startCodeartsCheckin);
  const checkinRunning = useAppStore((s) => s.codeartsCheckin.running);
  const [accounts, setAccounts] = useState<CodeartsAccountView[]>([]);
  const [groups, setGroups] = useState<GroupView[]>([]);
  const [loading, setLoading] = useState(true);
  // AK/SK 录入弹框（SK 等同密码，关闭即清空残留）
  const [showImport, setShowImport] = useState(false);
  const [impName, setImpName] = useState('');
  const [impAk, setImpAk] = useState('');
  const [impSk, setImpSk] = useState('');
  const [impGroup, setImpGroup] = useState<string | null>(null);
  const [importing, setImporting] = useState(false);
  // 华为云 OAuth 登录（进度弹框 + 手动访问授权页兜底）
  const [oauthRunning, setOauthRunning] = useState(false);
  const [oauthMsg, setOauthMsg] = useState('');
  const [oauthUrl, setOauthUrl] = useState('');
  // 编辑弹框（改名/备注/分组）
  const [editing, setEditing] = useState<CodeartsAccountView | null>(null);
  const [editName, setEditName] = useState('');
  const [editNote, setEditNote] = useState('');
  const [editGroup, setEditGroup] = useState<string | null>(null);
  const [editBusy, setEditBusy] = useState(false);
  // 破坏性操作确认（禁 window.confirm）
  const [confirmTarget, setConfirmTarget] = useState<ConfirmTarget>(null);
  const [confirmBusy, setConfirmBusy] = useState(false);
  const [retryingId, setRetryingId] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const [accs, gs] = await Promise.all([
        api.codearts.accountsList(),
        api.groups.sharedList().catch(() => [] as GroupView[]),
      ]);
      setAccounts(accs);
      setGroups(gs);
    } catch (err) {
      pushToast('error', `读取账号失败：${String(err)}`);
    } finally {
      setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // OAuth 进度/终态事件（系统浏览器授权，回调经本地回环写回凭据）
  useEffect(() => {
    let disposed = false;
    let unprog: (() => void) | undefined;
    let undone: (() => void) | undefined;
    void Promise.all([
      listen<{ stage: string; message: string; auth_url?: string | null }>(
        'codearts-oauth-progress',
        (e) => {
          if (disposed) return;
          setOauthMsg(e.payload.message || '正在处理…');
          if (e.payload.auth_url) setOauthUrl(e.payload.auth_url);
        },
      ),
      listen<{ ok: boolean; id: string; name: string; message: string }>(
        'codearts-oauth-done',
        (e) => {
          if (disposed) return;
          const p = e.payload;
          setOauthRunning(false);
          if (p.ok) {
            pushToast('success', `CodeArts 登录成功：${p.name || p.id}`);
            void refresh();
          } else {
            pushToast('error', `CodeArts 登录失败：${p.message}`);
          }
        },
      ),
    ]).then(([a, b]) => {
      if (disposed) { a(); b(); return; }
      unprog = a; undone = b;
    });
    // 5 分钟看门狗：回填卡住时解除运行态
    const timer = window.setTimeout(() => setOauthRunning(false), 300_000);
    return () => {
      disposed = true;
      window.clearTimeout(timer);
      unprog?.(); undone?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const startOAuth = async () => {
    setOauthRunning(true);
    setOauthMsg('正在启动华为云授权…');
    setOauthUrl('');
    try {
      const url = await api.codearts.oauthLogin();
      setOauthUrl(url);
    } catch (err) {
      setOauthRunning(false);
      pushToast('error', `启动 OAuth 失败：${String(err)}`);
    }
  };

  const cancelOAuth = async () => {
    try { await api.codearts.oauthCancel(); } catch { /* 忽略 */ }
    setOauthRunning(false);
  };

  const importAccount = async () => {
    const ak = impAk.trim();
    const sk = impSk.trim();
    if (!ak || !sk) {
      pushToast('warn', 'AK 与 SK 均不能为空');
      return;
    }
    setImporting(true);
    try {
      const v = await api.codearts.accountImportAkSk(impName.trim() || undefined, ak, sk, impGroup);
      pushToast('success', `账号已导入：${v.name || v.id}`);
      closeImport();
      void refresh();
    } catch (err) {
      pushToast('error', `导入失败：${String(err)}`);
    } finally {
      setImporting(false);
    }
  };

  // 关闭录入弹框：SK/AK 等同密码，一并清空输入残留
  const closeImport = () => {
    setShowImport(false);
    setImpName('');
    setImpAk('');
    setImpSk('');
    setImpGroup(null);
  };

  const openEdit = (a: CodeartsAccountView) => {
    setEditing(a);
    setEditName(a.name || '');
    setEditNote(a.note || '');
    setEditGroup(a.group_id);
  };

  const saveEdit = async () => {
    if (!editing || editBusy) return;
    setEditBusy(true);
    try {
      await api.codearts.accountSave(editing.id, editName.trim() || undefined, editNote, editGroup);
      pushToast('success', '已保存');
      setEditing(null);
      void refresh();
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setEditBusy(false);
    }
  };

  const confirmRemove = async () => {
    if (!confirmTarget) return;
    setConfirmBusy(true);
    try {
      await api.codearts.accountRemove(confirmTarget.account.id);
      pushToast('success', '已移除账号');
      setConfirmTarget(null);
      void refresh();
    } catch (err) {
      pushToast('error', `移除失败：${String(err)}`);
    } finally {
      setConfirmBusy(false);
    }
  };

  // 单账号重试签到：仅对该账号发起一轮，跳过今日已签
  const retryCheckin = async (a: CodeartsAccountView) => {
    if (checkinRunning || retryingId) return;
    setRetryingId(a.id);
    try {
      await startCodeartsCheckin({ user_ids: [a.id], skip_checked_in: true });
      pushToast('info', `已发起「${a.name || a.id}」签到重试，结果请以签到页进度为准`);
    } catch (err) {
      pushToast('error', `发起签到重试失败：${String(err)}`);
    } finally {
      setRetryingId(null);
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="CodeArts · 账号管理"
        desc="华为云 OAuth 登录 / AK/SK 手工录入 · 改名/备注/分组 · 单账号签到重试"
        actions={
          <>
            <button onClick={() => void refresh()} className="btn-outline" disabled={loading}>
              <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 刷新
            </button>
            <button className="btn-outline" onClick={() => void startOAuth()} disabled={oauthRunning}>
              <KeyRound size={15} /> 华为云登录
            </button>
            <button className="btn-primary" onClick={() => setShowImport(true)}>
              <UserPlus size={15} /> 录入 AK/SK
            </button>
          </>
        }
      />

      {accounts.length === 0 ? (
        loading ? (
          <div className="flex items-center justify-center gap-2 py-12 text-sm text-slate-400">
            <Loader2 size={16} className="animate-spin" /> 加载中…
          </div>
        ) : (
          <div className="mt-5">
            <EmptyState
              icon={<Users size={26} />}
              title="暂无 CodeArts 账号"
              hint="点击右上角「华为云登录」在浏览器授权（推荐，凭据自动续期），或「录入 AK/SK」手工入池。"
            />
          </div>
        )
      ) : (
        <div className="mt-5 card overflow-x-auto">
          <table className="w-full min-w-[760px] text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-left">AK</th>
                <th className="px-4 py-2 text-left">分组</th>
                <th className="px-4 py-2 text-left">凭证</th>
                <th className="px-4 py-2 text-right">积分余额</th>
                <th className="px-4 py-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              {accounts.map((a) => (
                <tr key={a.id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                  <td className="px-4 py-3">
                    <div className="font-medium">{a.name || a.id}</div>
                    <div className="text-xs text-slate-400">{[a.id, a.note].filter(Boolean).join(' · ')}</div>
                  </td>
                  <td className="px-4 py-3 font-mono text-xs text-slate-500">{a.ak_masked || '—'}</td>
                  <td className="px-4 py-3">
                    <GroupSelect
                      value={a.group_id}
                      groups={groups}
                      onChange={(gid) => {
                        void api.codearts
                          .accountSave(a.id, undefined, undefined, gid)
                          .then(() => {
                            setAccounts((prev) =>
                              prev.map((x) => (x.id === a.id ? { ...x, group_id: gid } : x)),
                            );
                            pushToast('success', '分组已调整');
                          })
                          .catch((err) => pushToast('error', `分组调整失败：${String(err)}`));
                      }}
                    />
                  </td>
                  <td className="px-4 py-3">
                    <Badge tone={a.has_credential ? 'green' : 'red'}>
                      {a.has_credential ? '已录入' : '无凭证'}
                    </Badge>
                  </td>
                  <td className="px-4 py-3 text-right tabular-nums text-xs">
                    {a.credits_balance != null ? a.credits_balance.toLocaleString() : '-'}
                  </td>
                  <td className="px-4 py-3 text-right">
                    <div className="flex justify-end gap-1">
                      <button
                        className="btn-ghost !px-2 !py-1 text-xs"
                        title="改名/备注"
                        onClick={() => openEdit(a)}
                      >
                        <Pencil size={14} /> 编辑
                      </button>
                      <button
                        className="btn-ghost !px-2 !py-1 text-xs"
                        title="对该账号重试签到"
                        disabled={checkinRunning || retryingId === a.id}
                        onClick={() => void retryCheckin(a)}
                      >
                        {retryingId === a.id ? (
                          <Loader2 size={14} className="animate-spin" />
                        ) : (
                          <PlayCircle size={14} />
                        )}{' '}
                        重试签到
                      </button>
                      <button
                        className="btn-ghost !px-2 !py-1 text-xs text-rose-500"
                        title="移除账号"
                        onClick={() => setConfirmTarget({ kind: 'remove', account: a })}
                      >
                        <Trash2 size={14} /> 移除
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* AK/SK 录入弹框 */}
      <Modal
        open={showImport}
        onClose={closeImport}
        title="录入 CodeArts AK/SK"
        footer={
          <>
            <button className="btn-outline" onClick={closeImport} disabled={importing}>
              取消
            </button>
            <button className="btn-primary" onClick={() => void importAccount()} disabled={importing}>
              {importing ? <Loader2 size={15} className="animate-spin" /> : <KeyRound size={15} />} 导入
            </button>
          </>
        }
      >
        <div className="space-y-3">
          <label className="block">
            <span className="text-xs text-slate-500">名称（可选）</span>
            <input
              className="input mt-1 w-full"
              value={impName}
              onChange={(e) => setImpName(e.target.value)}
              placeholder="如：主账号"
            />
          </label>
          <label className="block">
            <span className="text-xs text-slate-500">Access Key（AK）</span>
            <input
              className="input mt-1 w-full font-mono text-xs"
              value={impAk}
              onChange={(e) => setImpAk(e.target.value)}
              placeholder="华为云 Access Key"
              autoComplete="off"
            />
          </label>
          <label className="block">
            <span className="text-xs text-slate-500">Secret Key（SK，仅提交一次，加密落库）</span>
            <input
              type="password"
              className="input mt-1 w-full font-mono text-xs"
              value={impSk}
              onChange={(e) => setImpSk(e.target.value)}
              placeholder="华为云 Secret Key"
              autoComplete="off"
            />
          </label>
          <label className="block">
            <span className="text-xs text-slate-500">分组（可选）</span>
            <div className="mt-1">
              <GroupSelect value={impGroup} groups={groups} onChange={setImpGroup} />
            </div>
          </label>
        </div>
      </Modal>

      {/* 编辑弹框（改名/备注/分组） */}
      <Modal
        open={!!editing}
        onClose={() => setEditing(null)}
        title="编辑账号"
        footer={
          <>
            <button className="btn-outline" onClick={() => setEditing(null)} disabled={editBusy}>
              取消
            </button>
            <button className="btn-primary" onClick={() => void saveEdit()} disabled={editBusy}>
              {editBusy ? <Loader2 size={15} className="animate-spin" /> : null} 保存
            </button>
          </>
        }
      >
        <div className="space-y-3">
          <label className="block">
            <span className="text-xs text-slate-500">名称</span>
            <input
              className="input mt-1 w-full"
              value={editName}
              onChange={(e) => setEditName(e.target.value)}
            />
          </label>
          <label className="block">
            <span className="text-xs text-slate-500">备注</span>
            <input
              className="input mt-1 w-full"
              value={editNote}
              onChange={(e) => setEditNote(e.target.value)}
            />
          </label>
          <label className="block">
            <span className="text-xs text-slate-500">分组</span>
            <div className="mt-1">
              <GroupSelect value={editGroup} groups={groups} onChange={setEditGroup} />
            </div>
          </label>
        </div>
      </Modal>

      {/* 移除确认弹框（禁 window.confirm） */}
      <Modal
        open={!!confirmTarget}
        onClose={() => setConfirmTarget(null)}
        title="确认移除账号"
        footer={
          <>
            <button className="btn-outline" onClick={() => setConfirmTarget(null)} disabled={confirmBusy}>
              取消
            </button>
            <button className="btn-danger" onClick={() => void confirmRemove()} disabled={confirmBusy}>
              {confirmBusy ? <Loader2 size={15} className="animate-spin" /> : null} 确认移除
            </button>
          </>
        }
      >
        <p className="text-sm">
          确认移除「{confirmTarget?.account.name || confirmTarget?.account.id}」？移除后需重新登录或录入 AK/SK 才能使用。
        </p>
      </Modal>

      {/* 华为云 OAuth 授权进度（系统浏览器已打开，本弹框只展示状态，不抢占焦点） */}
      {oauthRunning && (
        <Modal
          open={oauthRunning}
          onClose={() => void cancelOAuth()}
          title="华为云授权登录"
        >
          <div className="space-y-3 text-sm">
            <div className="flex items-center gap-2 text-slate-600 dark:text-zinc-300">
              <Loader2 size={16} className="animate-spin" /> {oauthMsg || '正在等待浏览器授权…'}
            </div>
            <p className="text-xs text-slate-500 dark:text-zinc-400">
              已用系统默认浏览器打开华为云授权页。完成登录后凭据会自动写回，无需手动复制。
              若浏览器未自动打开，请手动访问下方链接：
            </p>
            {oauthUrl && (
              <a
                href={oauthUrl}
                target="_blank"
                rel="noreferrer"
                className="block break-all rounded border border-slate-200 p-2 text-xs text-brand-600 hover:underline dark:border-zinc-700 dark:text-brand-400"
              >
                {oauthUrl}
              </a>
            )}
            <div className="flex justify-end pt-2">
              <button className="btn-outline" onClick={() => void cancelOAuth()}>取消</button>
            </div>
          </div>
        </Modal>
      )}
    </div>
  );
}
