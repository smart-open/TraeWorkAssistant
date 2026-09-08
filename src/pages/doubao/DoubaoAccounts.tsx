import { useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import {
  Users,
  RefreshCw,
  Repeat,
  Timer,
  Coins,
  Cookie,
  Save,
  Pencil,
  Trash2,
  UserMinus,
  ShieldCheck,
  History,
} from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Modal } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { DoubaoAccountView, DoubaoRenewSummary } from '../../types';

/** P4-P5 待接入功能（P2 快照/切换、P3 会话续期已落地，不再展示） */
const PENDING_FEATURES: { phase: string; title: string; icon: typeof Users; desc: string }[] = [
  {
    phase: 'P4',
    title: '会员额度',
    icon: Coins,
    desc: 'MITM 抓包固化订阅额度接口（专业版 / 生图 / 视频日额度），额度条展示，仅展示不代刷。',
  },
  {
    phase: 'P5',
    title: 'Cookie 级热切换',
    icon: Cookie,
    desc: '实测 cookie 加密为 v10（AES-256-GCM + DPAPI，当前用户可解密）；sessionid 池化后进程内重写 Cookies 表，免重启切换（二期增强项）。',
  },
];

function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

type DialogState =
  | { mode: 'save-login' }
  | { mode: 'edit'; userId: string; name: string; note: string; sessionId: string; sidGuard: string }
  | null;

export default function DoubaoAccounts() {
  const pushToast = useAppStore((s) => s.pushToast);
  const switchTo = useAppStore((s) => s.switchTo);
  const saveCurrentLogin = useAppStore((s) => s.saveCurrentLogin);
  const switchingTo = useAppStore((s) => s.switchingTo);
  const switchProgress = useAppStore((s) => s.switchProgress);
  const savingLogin = useAppStore((s) => s.savingLogin);
  const saveLoginProgress = useAppStore((s) => s.saveLoginProgress);

  const [accounts, setAccounts] = useState<DoubaoAccountView[]>([]);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false); // 恢复/删除等本地操作进行中
  const [profileProgress, setProfileProgress] = useState<string[]>([]);
  const [dialog, setDialog] = useState<DialogState>(null);
  const [renewRunning, setRenewRunning] = useState(false);
  const [renewSummary, setRenewSummary] = useState<DoubaoRenewSummary | null>(null);
  const [keepaliveRunning, setKeepaliveRunning] = useState(false);
  const [keepaliveProgress, setKeepaliveProgress] = useState<string[]>([]);
  // 保存当前登录态表单
  const [uidInput, setUidInput] = useState('');
  const [nameInput, setNameInput] = useState('');
  // 编辑别名/备注表单
  const [editName, setEditName] = useState('');
  const [editNote, setEditNote] = useState('');
  const [editSessionId, setEditSessionId] = useState('');
  const [editSidGuard, setEditSidGuard] = useState('');

  /** 到期提醒：
   *  ① 池级保活：距 last_keepalive_at 超过 25 天（sid_guard 30 天滑动窗口临近耗尽）；
   *  ② 账号级：session_state=expired 或 session_expire_at 7 天内到期（手动录入凭证的账号）。 */
  const checkExpiry = (list: DoubaoAccountView[]) => {
    const expired = list.filter((a) => a.session_state === 'expired');
    if (expired.length > 0) {
      pushToast('error', `${expired.length} 个豆包账号会话已判定过期：${expired.map((a) => a.name).join('、')}，请重新登录`);
    }
    const soon = list.filter((a) => {
      if (!a.session_expire_at || a.session_state === 'expired') return false;
      const t = new Date(a.session_expire_at.replace(' ', 'T')).getTime();
      return !Number.isNaN(t) && t - Date.now() < 7 * 24 * 3600 * 1000;
    });
    if (soon.length > 0) {
      pushToast('warn', `${soon.length} 个豆包账号会话 7 天内到期：${soon.map((a) => a.name).join('、')}`);
    }
    const lastKa = list.find((a) => a.last_keepalive_at)?.last_keepalive_at;
    if (!lastKa && list.length > 0) {
      // 无记录不提醒（老数据/未注册任务）
      return;
    }
    if (lastKa) {
      const t = new Date(lastKa.replace(' ', 'T')).getTime();
      if (!Number.isNaN(t) && Date.now() - t > 25 * 24 * 3600 * 1000) {
        pushToast('warn', `豆包已超过 25 天未保活（上次 ${lastKa}），会话可能临近失效，请运行保活或登录豆包刷新`);
      }
    }
  };

  const reload = async (notifyExpiry = false) => {
    setLoading(true);
    try {
      const list = await api.doubao.accountsList();
      setAccounts(list);
      if (notifyExpiry) checkExpiry(list);
    } catch (err) {
      pushToast('error', `读取豆包账号失败：${String(err)}`);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void reload(true);
    // 切换/保存/快照操作完成（store 负责 toast），这里负责刷新列表
    const cleanups: Array<() => void> = [];
    void listen('switch-done', () => void reload()).then((u) => cleanups.push(u));
    void listen('save-login-done', () => void reload()).then((u) => cleanups.push(u));
    void listen('profile-progress', (e) =>
      setProfileProgress((prev) => [...prev.slice(-49), e.payload as string]),
    ).then((u) => cleanups.push(u));
    void listen('profile-done', () => {
      setBusy(false);
      void reload();
    }).then((u) => cleanups.push(u));
    // 保活进度（P3）
    void listen('keepalive-progress', (e) =>
      setKeepaliveProgress((prev) => [...prev.slice(-49), e.payload as string]),
    ).then((u) => cleanups.push(u));
    void listen('keepalive-done', (e) => {
      const ok = (e.payload as { success: boolean }).success;
      setKeepaliveRunning(false);
      pushToast(
        ok ? 'success' : 'error',
        ok ? '保活完成：豆包会话已滑动续期' : '保活未完成，请查看日志',
      );
      void reload();
    }).then((u) => cleanups.push(u));
    return () => cleanups.forEach((u) => u());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** 立即保活：启动豆包 25s 联网滑动续期后优雅关闭（运行中则跳过） */
  const doKeepalive = async () => {
    setKeepaliveRunning(true);
    setKeepaliveProgress([]);
    try {
      await api.doubao.keepaliveRun();
      pushToast('info', '保活已启动，请稍候（约 30 秒）…');
    } catch (err) {
      setKeepaliveRunning(false);
      pushToast('error', `保活失败：${String(err)}`);
    }
  };

  /** 打开「保存当前登录态」弹框：自动探测当前登录 uid（%APPDATA%\Doubao\public_config.json）预填 */
  const openSaveLoginDialog = async () => {
    setUidInput('');
    setNameInput('');
    setDialog({ mode: 'save-login' });
    try {
      const uid = await api.doubao.detectUid();
      if (uid) {
        setUidInput(uid);
      } else {
        pushToast('info', '未能自动识别当前豆包账号，请手动输入 user_id');
      }
    } catch {
      /* 探测失败静默，保持手动输入 */
    }
  };

  const submitSaveLogin = async () => {
    const uid = uidInput.trim();
    if (!uid) {
      pushToast('warn', '请输入账号 user_id');
      return;
    }
    setDialog(null);
    // 沿用现有管线：关豆包 → 快照到 profiles_doubao/<uid> → 重启（NDJSON 进度）
    await saveCurrentLogin(uid, 'Doubao');
    if (nameInput.trim()) {
      // 别名异步入池，失败不影响保存
      api.doubao.accountSave(uid, nameInput.trim()).catch(() => {});
    }
  };

  const submitEdit = async () => {
    if (!dialog || dialog.mode !== 'edit') return;
    try {
      await api.doubao.accountSave(dialog.userId, editName.trim() || dialog.userId, editNote.trim());
      // 凭证字段留空 = 不修改；填了则覆盖（手动录入来源，参与探活巡检）
      if (editSessionId.trim() || editSidGuard.trim()) {
        await api.doubao.accountSetCredential(dialog.userId, editSessionId.trim() || undefined, editSidGuard.trim() || undefined);
      }
      pushToast('success', '账号信息已更新');
      setDialog(null);
      void reload();
    } catch (err) {
      pushToast('error', `更新失败：${String(err)}`);
    }
  };

  const doRestore = async (uid: string) => {
    if (!confirm(`恢复账号 ${uid} 的登录态？豆包会先关闭，恢复后自动重启（当前登录态会先备份到 last 槽）。`)) return;
    setBusy(true);
    setProfileProgress([]);
    try {
      await api.profiles.restore(uid, 'Doubao');
      pushToast('info', '正在恢复登录态，请稍候…');
    } catch (err) {
      setBusy(false);
      pushToast('error', `恢复失败：${String(err)}`);
    }
  };

  const doDeleteSnapshot = async (uid: string) => {
    if (!confirm(`删除账号 ${uid} 的登录态快照？该账号将需要重新登录才能再切换。`)) return;
    try {
      await api.profiles.delete(uid, 'Doubao');
      await reload();
      pushToast('info', '快照已删除');
    } catch (err) {
      pushToast('error', `删除失败：${String(err)}`);
    }
  };

  const doRemove = async (uid: string) => {
    if (!confirm(`将账号 ${uid} 移出账号池？（不影响其快照文件）`)) return;
    try {
      await api.doubao.accountRemove(uid);
      await reload();
      pushToast('info', '已移出账号池');
    } catch (err) {
      pushToast('error', `移除失败：${String(err)}`);
    }
  };

  /** 运行续期巡检（解密 cookie + 探活续期），完成后刷新列表会话状态 */
  const doRenew = async () => {
    setRenewRunning(true);
    setRenewSummary(null);
    try {
      const s = await api.doubao.renewRun(false);
      setRenewSummary(s);
      if (s.renew) {
        const { ok, expired, error } = s.renew;
        pushToast(expired > 0 ? 'warn' : 'success', `巡检完成：有效 ${ok}，过期 ${expired}，异常 ${error}`);
      }
      await reload();
    } catch (err) {
      pushToast('error', `巡检失败：${String(err)}`);
    } finally {
      setRenewRunning(false);
    }
  };

  const anyBusy = busy || renewRunning || keepaliveRunning || !!switchingTo || !!savingLogin;

  /** 会话状态徽标（P3） */
  const sessionBadge = (a: DoubaoAccountView) => {
    if (a.session_state === 'ok') {
      return <Badge tone="green">有效{a.session_expire_at ? ` · ${a.session_expire_at.slice(0, 10)}` : ''}</Badge>;
    }
    if (a.session_state === 'expired') return <Badge tone="red">已过期</Badge>;
    if (a.session_state === 'unknown') return <Badge tone="amber">未探活</Badge>;
    return <span className="text-xs text-slate-400">—</span>;
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="豆包 · 账号管理"
        desc="快照切换（P2）+ 会话续期（P3）· 快照存 data/profiles_doubao/<uid>/"
        actions={
          <>
            <button onClick={() => void reload()} className="btn-outline" disabled={loading}>
              <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 刷新
            </button>
            <button onClick={() => void doRenew()} className="btn-outline" disabled={anyBusy}>
              <ShieldCheck size={15} /> {renewRunning ? '巡检中…' : '续期巡检'}
            </button>
            <button onClick={() => void doKeepalive()} className="btn-outline" disabled={anyBusy}>
              <Timer size={15} /> {keepaliveRunning ? '保活中…' : '立即保活'}
            </button>
            <button onClick={() => void openSaveLoginDialog()} className="btn-primary" disabled={anyBusy}>
              <Save size={15} /> 保存当前登录态
            </button>
          </>
        }
      />

      {/* 切换 / 保存进度（复用全局 NDJSON 事件管线） */}
      {(switchingTo || savingLogin) && (
        <div className="mt-5 rounded-lg border border-brand-300 bg-brand-50 p-3 dark:border-brand-700 dark:bg-brand-900/20">
          <div className="mb-1 text-xs font-medium text-brand-700 dark:text-brand-300">
            {switchingTo ? `正在切换至 ${switchingTo}…` : `正在保存 ${savingLogin} 的登录态…`}
          </div>
          <div className="max-h-40 space-y-0.5 overflow-auto font-mono text-xs text-brand-600 dark:text-brand-400">
            {(switchingTo ? switchProgress : saveLoginProgress).length === 0 ? (
              <div>等待中...</div>
            ) : (
              (switchingTo ? switchProgress : saveLoginProgress).map((line, i) => <div key={i}>{line}</div>)
            )}
          </div>
        </div>
      )}
      {busy && profileProgress.length > 0 && (
        <div className="mt-5 rounded-lg border border-brand-300 bg-brand-50 p-3 dark:border-brand-700 dark:bg-brand-900/20">
          <div className="mb-1 text-xs font-medium text-brand-700 dark:text-brand-300">正在处理…</div>
          <div className="max-h-40 space-y-0.5 overflow-auto font-mono text-xs text-brand-600 dark:text-brand-400">
            {profileProgress.map((line, i) => (
              <div key={i}>{line}</div>
            ))}
          </div>
        </div>
      )}

      {/* 保活进度（P3，NDJSON 事件） */}
      {keepaliveRunning && (
        <div className="mt-5 rounded-lg border border-emerald-300 bg-emerald-50 p-3 dark:border-emerald-700 dark:bg-emerald-900/20">
          <div className="mb-1 text-xs font-medium text-emerald-700 dark:text-emerald-300">
            正在保活（启动豆包 → 等待会话联网刷新 → 关闭）…
          </div>
          <div className="max-h-32 space-y-0.5 overflow-auto font-mono text-xs text-emerald-600 dark:text-emerald-400">
            {keepaliveProgress.length === 0 ? (
              <div>等待中...</div>
            ) : (
              keepaliveProgress.map((line, i) => <div key={i}>{line}</div>)
            )}
          </div>
        </div>
      )}

      {/* 续期巡检摘要（P3） */}
      {renewSummary && (
        <div className="mt-5 card p-4 text-xs text-slate-500 dark:text-zinc-400">
          <div className="mb-1 flex items-center gap-2 text-sm font-medium text-slate-700 dark:text-zinc-200">
            <ShieldCheck size={15} className="text-emerald-500" /> 续期巡检结果
            <span className="text-xs font-normal text-slate-400">{renewSummary.finished_at}</span>
          </div>
          {renewSummary.renew && (
            <div className="mb-2">
              有效 {renewSummary.renew.ok} · 过期 {renewSummary.renew.expired} · 异常{' '}
              {renewSummary.renew.error} · 跳过 {renewSummary.renew.skipped}
              {renewSummary.sync ? ` · Cookie 同步 ${renewSummary.sync.synced} 个` : ''}
            </div>
          )}
          {renewSummary.accounts && renewSummary.accounts.length > 0 && (
            <div className="max-h-40 space-y-0.5 overflow-auto font-mono">
              {renewSummary.accounts.map((a) => (
                <div key={a.user_id}>
                  {a.user_id}: {a.status}
                  {a.detail ? `（${a.detail}）` : ''}
                  {a.renewed ? ' · 已续期' : ''}
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      {/* 账号列表 */}
      <div className="mt-5 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Users size={16} className="text-violet-500" />
          <span className="text-sm font-medium">账号池</span>
          <Badge tone="green">P2 已接入</Badge>
          <span className="text-xs text-slate-400">{accounts.length} 个账号</span>
        </div>
        {accounts.length === 0 ? (
          <div className="py-8 text-center text-xs text-slate-400">
            暂无账号。登录豆包后点击右上角「保存当前登录态」建立首个快照。
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                <tr>
                  <th className="whitespace-nowrap px-3 py-2 text-left">账号</th>
                  <th className="whitespace-nowrap px-3 py-2 text-left">备注</th>
                  <th className="whitespace-nowrap px-3 py-2 text-left">快照</th>
                  <th className="whitespace-nowrap px-3 py-2 text-left">会话</th>
                  <th className="whitespace-nowrap px-3 py-2 text-left">最后修改</th>
                  <th className="whitespace-nowrap px-3 py-2 text-right">操作</th>
                </tr>
              </thead>
              <tbody>
                {accounts.map((a) => (
                  <tr key={a.user_id} className="border-t border-slate-200 dark:border-zinc-800">
                    <td className="px-3 py-2">
                      <div className="flex items-center gap-2">
                        <span className="font-medium">{a.name}</span>
                        {a.is_current && <Badge tone="green">当前</Badge>}
                      </div>
                      <div className="font-mono text-xs text-slate-400">{a.user_id}</div>
                    </td>
                    <td className="max-w-40 truncate px-3 py-2 text-xs text-slate-500" title={a.note}>
                      {a.note || '—'}
                    </td>
                    <td className="px-3 py-2 text-xs">
                      {a.has_snapshot ? (
                        <div>
                          <div className="text-emerald-600 dark:text-emerald-400">已保存</div>
                          <div className="text-slate-400">
                            {fmtSize(a.size_bytes)} · {a.file_count} 文件
                          </div>
                        </div>
                      ) : (
                        <span className="text-slate-400">无快照</span>
                      )}
                    </td>
                    <td className="whitespace-nowrap px-3 py-2">{sessionBadge(a)}</td>
                    <td className="whitespace-nowrap px-3 py-2 text-xs text-slate-500">
                      {a.last_modified || a.added_at || '—'}
                    </td>
                    <td className="px-3 py-2">
                      <div className="flex justify-end gap-1">
                        <button
                          title="切换到此账号"
                          onClick={() => void switchTo(a.user_id, 'Doubao')}
                          disabled={anyBusy || !a.has_snapshot}
                          className="btn-ghost !p-2 text-sky-500 hover:bg-sky-50 disabled:opacity-30 dark:hover:bg-sky-500/10"
                        >
                          <Repeat size={14} />
                        </button>
                        <button
                          title="恢复快照（不备份当前）"
                          onClick={() => void doRestore(a.user_id)}
                          disabled={anyBusy || !a.has_snapshot}
                          className="btn-ghost !p-2 text-violet-500 hover:bg-violet-50 disabled:opacity-30 dark:hover:bg-violet-500/10"
                        >
                          <History size={14} />
                        </button>
                        <button
                          title="编辑别名 / 备注 / 会话凭证"
                          onClick={() => {
                            setEditName(a.name === a.user_id ? '' : a.name);
                            setEditNote(a.note);
                            setEditSessionId('');
                            setEditSidGuard('');
                            setDialog({ mode: 'edit', userId: a.user_id, name: a.name, note: a.note, sessionId: '', sidGuard: '' });
                          }}
                          disabled={anyBusy}
                          className="btn-ghost !p-2"
                        >
                          <Pencil size={14} />
                        </button>
                        <button
                          title="删除快照"
                          onClick={() => void doDeleteSnapshot(a.user_id)}
                          disabled={anyBusy || !a.has_snapshot}
                          className="btn-ghost !p-2 text-rose-500 hover:bg-rose-50 disabled:opacity-30 dark:hover:bg-rose-500/10"
                        >
                          <Trash2 size={14} />
                        </button>
                        <button
                          title="移出账号池"
                          onClick={() => void doRemove(a.user_id)}
                          disabled={anyBusy}
                          className="btn-ghost !p-2 text-slate-400 hover:bg-slate-50 disabled:opacity-30 dark:hover:bg-zinc-800"
                        >
                          <UserMinus size={14} />
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <div className="mt-3 text-xs text-slate-400">
          切换流程：关闭豆包 → 当前登录态自动备份（last 槽 + 原账号槽）→ 恢复目标快照 → 重启豆包。会话状态由续期巡检判定
          （sid_guard 滑动续期，到期前 7 天提醒）；保活端点与每日定时任务在「环境配置」页设置。快照与 Trae 相互独立。
        </div>
      </div>

      {/* P3-P5 待接入功能 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Users size={16} className="text-slate-400" />
          <span className="text-sm font-medium">后续路线</span>
          <Badge tone="amber">待接入</Badge>
        </div>
        <div className="space-y-2">
          {PENDING_FEATURES.map((f) => {
            const Icon = f.icon;
            return (
              <div
                key={f.title}
                className="flex items-start gap-3 rounded-lg border border-slate-100 p-3 dark:border-zinc-800"
              >
                <div className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-slate-100 text-slate-500 dark:bg-zinc-800 dark:text-zinc-400">
                  <Icon size={15} />
                </div>
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="text-sm font-medium">{f.title}</span>
                    <Badge tone="slate">{f.phase}</Badge>
                  </div>
                  <div className="mt-0.5 text-xs text-slate-500">{f.desc}</div>
                </div>
              </div>
            );
          })}
        </div>
      </div>

      {/* 保存当前登录态弹框 */}
      <Modal open={dialog?.mode === 'save-login'} onClose={() => setDialog(null)} title="保存当前豆包登录态" size="lg">
        <div className="space-y-3">
          <div>
            <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">账号 user_id</label>
            <input
              value={uidInput}
              onChange={(e) => setUidInput(e.target.value)}
              placeholder="自动探测失败时请手动输入"
              className="input font-mono"
            />
          </div>
          <div>
            <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">别名（可选）</label>
            <input
              value={nameInput}
              onChange={(e) => setNameInput(e.target.value)}
              placeholder="如：工作号"
              className="input"
            />
          </div>
          <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-900 dark:text-zinc-400">
            保存时会先关闭豆包，将当前登录态白名单快照到该账号槽位后重启豆包。user_id 可在
            <span className="mx-1 font-mono">%APPDATA%\Doubao\public_config.json</span>
            中查看。
          </div>
          <div className="flex justify-end gap-2">
            <button onClick={() => setDialog(null)} className="btn-outline">
              取消
            </button>
            <button onClick={() => void submitSaveLogin()} className="btn-primary">
              <Save size={15} /> 保存
            </button>
          </div>
        </div>
      </Modal>

      {/* 编辑别名 / 备注弹框 */}
      <Modal open={dialog?.mode === 'edit'} onClose={() => setDialog(null)} title="编辑账号信息" size="lg">
        {dialog?.mode === 'edit' && (
          <div className="space-y-3">
            <div className="font-mono text-xs text-slate-400">{dialog.userId}</div>
            <div>
              <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">别名</label>
              <input
                value={editName}
                onChange={(e) => setEditName(e.target.value)}
                placeholder={dialog.userId}
                className="input"
              />
            </div>
            <div>
              <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">备注</label>
              <textarea
                value={editNote}
                onChange={(e) => setEditNote(e.target.value)}
                rows={2}
                className="input resize-none"
              />
            </div>
            <div>
              <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">
                会话凭证 sessionid（可选，高级）
              </label>
              <input
                value={editSessionId}
                onChange={(e) => setEditSessionId(e.target.value)}
                placeholder="留空不修改；录入后可参与续期巡检探活"
                className="input font-mono text-xs"
              />
              <input
                value={editSidGuard}
                onChange={(e) => setEditSidGuard(e.target.value)}
                placeholder="sid_guard（可选，格式 sid|创建时间|有效期|…，用于到期提醒）"
                className="input mt-2 font-mono text-xs"
              />
              <p className="mt-1 text-xs text-slate-400">
                凭证等同密码，仅存本地。豆包桌面客户端 cookie 为客户端级加密，无法自动提取，需从网页版或抓包工具手动获取。
              </p>
            </div>
            <div className="flex justify-end gap-2">
              <button onClick={() => setDialog(null)} className="btn-outline">
                取消
              </button>
              <button onClick={() => void submitEdit()} className="btn-primary">
                保存
              </button>
            </div>
          </div>
        )}
      </Modal>
    </div>
  );
}
