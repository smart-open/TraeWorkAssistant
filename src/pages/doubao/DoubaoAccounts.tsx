import { useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import {
  Users,
  RefreshCw,
  Repeat,
  Rocket,
  Timer,
  Coins,
  Save,
  Pencil,
  Trash2,
  ShieldCheck,
  HelpCircle,
} from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Modal } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { DoubaoAccountView, DoubaoQuotaResult, DoubaoRenewSummary, DoubaoSnapshotMeta } from '../../types';

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
  const openDoubaoAs = useAppStore((s) => s.openDoubaoAs);
  const saveCurrentLogin = useAppStore((s) => s.saveCurrentLogin);
  const switchingTo = useAppStore((s) => s.switchingTo);
  const switchProgress = useAppStore((s) => s.switchProgress);
  const savingLogin = useAppStore((s) => s.savingLogin);
  const saveLoginProgress = useAppStore((s) => s.saveLoginProgress);
  const proxy = useAppStore((s) => s.proxy);

  const [accounts, setAccounts] = useState<DoubaoAccountView[]>([]);
  const [snapshotMetas, setSnapshotMetas] = useState<Record<string, DoubaoSnapshotMeta | null>>({});
  const [loading, setLoading] = useState(false);
  const [dialog, setDialog] = useState<DialogState>(null);
  const [renewRunning, setRenewRunning] = useState(false);
  const [renewSummary, setRenewSummary] = useState<DoubaoRenewSummary | null>(null);
  const [keepaliveRunning, setKeepaliveRunning] = useState(false);
  const [keepaliveProgress, setKeepaliveProgress] = useState<string[]>([]);
  const [quotaRunningFor, setQuotaRunningFor] = useState<string | null>(null);
  const [quotaResult, setQuotaResult] = useState<{ userId: string; result: DoubaoQuotaResult } | null>(null);
  // 保存当前登录态表单
  const [uidInput, setUidInput] = useState('');
  const [nameInput, setNameInput] = useState('');
  // 编辑别名/备注表单
  const [editName, setEditName] = useState('');
  const [editNote, setEditNote] = useState('');
  const [editSessionId, setEditSessionId] = useState('');
  const [editSidGuard, setEditSidGuard] = useState('');
  const [helpOpen, setHelpOpen] = useState(false);

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
      // C3：加载快照版本元数据（有快照的账号并行读取本地 JSON，供快照列悬停展示）
      const withSnapshot = list.filter((a) => a.has_snapshot);
      if (withSnapshot.length > 0) {
        const entries = await Promise.all(
          withSnapshot.map(async (a) => {
            try {
              return [a.user_id, await api.doubao.snapshotMeta(a.user_id)] as const;
            } catch {
              return [a.user_id, null] as const;
            }
          }),
        );
        setSnapshotMetas(Object.fromEntries(entries));
      } else {
        setSnapshotMetas({});
      }
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
    // 保活进度
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

  /** 代理抓包凭证自动回写：豆包客户端/网页版流量经过代理时，device_proxy.py 会抓到
   *  当前登录账号的 sessionid / sid_guard 并落盘；这里挂载时执行一次 + 每 20s 轮询，
   *  后端幂等（凭证内容未变化直接跳过），有写入才提示并刷新列表。 */
  useEffect(() => {
    let stopped = false;
    const apply = async () => {
      if (stopped) return;
      try {
        const applied = await api.doubao.credentialAutoApply();
        if (applied) {
          pushToast('success', `已自动写入代理抓到的会话凭证：${applied}`);
          void reload();
        }
      } catch {
        /* 静默：抓包文件不存在 / 无当前账号等属正常情况 */
      }
    };
    void apply();
    const timer = setInterval(() => void apply(), 20000);
    return () => {
      stopped = true;
      clearInterval(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** 会员额度自动查询：有会话凭证且从未查过额度的账号，进入本页后串行查询一次，
   *  结果缓存进账号池（后端回写），账号名后即显示会员/免费标识。
   *  查到额度窗口已用完时弹一次提醒（含重置时间）。 */
  const quotaAutoFetched = useRef(false);
  useEffect(() => {
    if (quotaAutoFetched.current || loading || accounts.length === 0) return;
    const pending = accounts.filter((a) => a.session_state !== 'none' && !a.quota_checked_at);
    quotaAutoFetched.current = true;
    if (pending.length === 0) return;
    (async () => {
      for (const a of pending) {
        try {
          const r = await api.doubao.fetchQuota(a.user_id);
          const exhaustedWin = (r.parsed?.items ?? []).find(
            (it) => 'used_percent' in it && (it.exhausted || it.used_percent >= 100),
          );
          if (exhaustedWin && 'reset_at' in exhaustedWin) {
            pushToast(
              'warn',
              `账号 ${a.name} 的「${exhaustedWin.name}」额度已用完${exhaustedWin.reset_at ? `，${exhaustedWin.reset_at} 重置` : ''}`,
            );
          }
        } catch {
          /* 查询失败静默：不留缓存，下次进入页面会重试 */
        }
      }
      void reload();
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [accounts, loading]);

  /** 立即保活：启动豆包 8s 联网滑动续期后优雅关闭（运行中则跳过） */
  const doKeepalive = async () => {
    setKeepaliveRunning(true);
    setKeepaliveProgress([]);
    try {
      await api.doubao.keepaliveRun();
      pushToast('info', '保活已启动，请稍候（约 12 秒）…');
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
    // 无论是否填别名都入池（快照已生成，池里有元数据后可直接编辑别名/凭证）
    api.doubao.accountSave(uid, nameInput.trim() || undefined).catch(() => {});
  };

  /** 从代理抓包文件读取最新抓到的豆包会话凭证，预填编辑弹框（免手动抄写） */
  const doFillCaptured = async () => {
    if (!dialog || dialog.mode !== 'edit') return;
    try {
      const c = await api.doubao.capturedCredential();
      if (!c) {
        pushToast(
          'warn',
          '暂未抓到凭证：请先在顶栏「启动代理」，再用浏览器（走系统代理）登录网页版 doubao.com，然后回到这里点此填充',
        );
        return;
      }
      setEditSessionId(c.session_id);
      if (c.sid_guard) setEditSidGuard(c.sid_guard);
      pushToast('success', `已填入抓包凭证（抓到于 ${c.captured_at}）。请确认网页版登录的就是账号 ${dialog.userId}`);
    } catch (err) {
      pushToast('error', `读取抓包凭证失败：${String(err)}`);
    }
  };

  const submitEdit = async () => {
    if (!dialog || dialog.mode !== 'edit') return;
    try {
      await api.doubao.accountSave(dialog.userId, editName.trim() || dialog.userId, editNote.trim());
      // 凭证字段已回填现有值：修改即覆盖，清空后保存即删除凭证
      await api.doubao.accountSetCredential(dialog.userId, editSessionId.trim() || undefined, editSidGuard.trim() || undefined);
      pushToast('success', '账号信息已更新');
      setDialog(null);
      void reload();
    } catch (err) {
      pushToast('error', `更新失败：${String(err)}`);
    }
  };

  /** 删除账号：移出账号池 + 删除该账号快照文件（双重确认，不可恢复） */
  const doRemove = async (uid: string) => {
    if (
      !confirm(
        `删除账号 ${uid}？\n\n将同时执行：\n· 移出账号池（清除别名 / 备注 / 凭证）\n· 删除快照文件（登录态不可恢复，再使用需重新登录）`,
      )
    )
      return;
    if (!confirm(`⚠ 二次确认：账号 ${uid} 的登录态快照将被永久删除、无法恢复。\n\n确定删除吗？`)) return;
    try {
      await api.doubao.accountRemove(uid);
      if (accounts.find((a) => a.user_id === uid)?.has_snapshot) {
        await api.profiles.delete(uid, 'Doubao');
      }
      pushToast('info', `账号 ${uid} 已删除（含快照）`);
      await reload();
    } catch (err) {
      pushToast('error', `删除失败：${String(err)}`);
    }
  };

  /** 运行续期巡检：有手动凭证的账号在线探活；若全部账号都未录入凭证，
   *  自动回退为 KeepAlive 保活（让豆包客户端自己滑动续期），避免"点了没效果"。 */
  const doRenew = async () => {
    setRenewRunning(true);
    setRenewSummary(null);
    try {
      const s = await api.doubao.renewRun(false);
      setRenewSummary(s);
      if (s.renew) {
        const { ok, expired, error, skipped } = s.renew;
        // 全部因无凭证跳过 → 探活路径不可用，自动改走保活
        if (ok === 0 && expired === 0 && error === 0 && skipped > 0) {
          pushToast(
            'info',
            `${skipped} 个账号未录入会话凭证（sessionid），无法在线探活——已自动改用保活方式续期会话`,
          );
          setRenewRunning(false);
          void doKeepalive();
          return;
        }
        pushToast(expired > 0 ? 'warn' : 'success', `巡检完成：有效 ${ok}，过期 ${expired}，异常 ${error}`);
      }
      await reload();
    } catch (err) {
      pushToast('error', `巡检失败：${String(err)}`);
    } finally {
      setRenewRunning(false);
    }
  };

  const anyBusy = renewRunning || keepaliveRunning || !!switchingTo || !!savingLogin;

  /** 查询会员额度（需该账号已有会话凭证；成功后结果会缓存进账号池，徽标/悬停提示随之更新） */
  const doFetchQuota = async (a: DoubaoAccountView) => {
    if (a.session_state === 'none') {
      pushToast(
        'warn',
        `账号 ${a.name} 尚无会话凭证：开启代理后豆包流量经过代理即可自动写入凭证，或在该账号的编辑弹框中手动录入`,
      );
      return;
    }
    setQuotaRunningFor(a.user_id);
    setQuotaResult(null);
    try {
      const r = await api.doubao.fetchQuota(a.user_id);
      setQuotaResult({ userId: a.user_id, result: r });
      await reload();
    } catch (err) {
      pushToast('error', `额度查询失败：${String(err)}`);
    } finally {
      setQuotaRunningFor(null);
    }
  };

  /** 会员/免费标识（quota_checked_at 非空 = 已查询过） */
  const quotaBadge = (a: DoubaoAccountView) => {
    if (!a.quota_checked_at) return null;
    if (a.quota_level) return <Badge tone="amber">{a.quota_level}</Badge>;
    return <Badge tone="slate">免费</Badge>;
  };

  /** 悬停账号名时的额度提示（额度状态 / 会员 / 到期时间） */
  const quotaTip = (a: DoubaoAccountView) => {
    if (!a.quota_checked_at) {
      return a.session_state === 'none'
        ? '额度未查询：该账号尚无会话凭证（开启代理自动写入，或编辑弹框手动录入）'
        : '额度未查询：有会话凭证的账号会自动查询，也可点击「额度」图标手动查询';
    }
    return [
      `会员：${a.quota_level || '免费'}`,
      `到期时间：${a.quota_expire_at || '—（免费 / 长期有效）'}`,
      `额度状态：${a.quota_summary || '未识别到额度条目'}`,
      `查询时间：${a.quota_checked_at}`,
    ].join('\n');
  };

  /** 解析 sid_guard（格式 sid|创建毫秒|有效期秒|…）→ 到期时间；无法解析返回 null */
  const sidGuardExpiry = (sidGuard: string | null): Date | null => {
    if (!sidGuard) return null;
    const parts = sidGuard.split('|');
    if (parts.length < 3) return null;
    const createMs = Number(parts[1]);
    const ttlSec = Number(parts[2]);
    if (!Number.isFinite(createMs) || !Number.isFinite(ttlSec) || createMs <= 0 || ttlSec <= 0) return null;
    return new Date(createMs + ttlSec * 1000);
  };

  /** 会话状态徽标（B3 到期分层：有效 → ≤7 天内到期黄色提醒 → 已过期红色） */
  const sessionBadge = (a: DoubaoAccountView) => {
    if (a.session_state === 'expired') return <Badge tone="red">已过期</Badge>;
    if (a.session_state === 'ok' || a.session_state === 'unknown') {
      const expiry = sidGuardExpiry(a.sid_guard);
      if (expiry) {
        const daysLeft = Math.floor((expiry.getTime() - Date.now()) / 86400000);
        if (daysLeft < 0) return <Badge tone="red">凭证已到期</Badge>;
        if (daysLeft <= 7) {
          return (
            <Badge tone="amber" title={`会话凭证 ${daysLeft === 0 ? '今天' : `${daysLeft} 天后`}（${expiry.toLocaleDateString()}）到期，请及时保活或重新登录`}>
              {daysLeft === 0 ? '今天到期' : `${daysLeft} 天后到期`}
            </Badge>
          );
        }
      }
      if (a.session_state === 'ok') {
        return <Badge tone="green">有效{expiry ? ` · ${expiry.toLocaleDateString()} 到期` : ''}</Badge>;
      }
      return <Badge tone="amber">未探活</Badge>;
    }
    return <span className="text-xs text-slate-400">—</span>;
  };

  /** C3：快照列悬停提示（版本元数据：schema / Chromium 版本 / 生成时间 / IndexedDB） */
  const snapshotTip = (a: DoubaoAccountView): string => {
    if (!a.has_snapshot) return '无快照';
    const m = snapshotMetas[a.user_id];
    if (!m) return '旧版快照（无版本元数据）：恢复时跳过版本校验，建议重新保存一次登录态';
    if (m.schema_version === 0) return `旧版快照 · Chromium ${m.chromium_version}：无元数据，恢复时跳过 schemaVersion 校验`;
    return [
      `快照版本：schema v${m.schema_version}`,
      m.chromium_version ? `生成时豆包内核：Chromium ${m.chromium_version}` : '',
      m.created_at ? `生成时间：${m.created_at}` : '',
      m.include_idb ? '已纳入 IndexedDB（对话历史随账号迁移）' : '未纳入 IndexedDB（可在环境配置开启）',
    ]
      .filter(Boolean)
      .join('\n');
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="豆包 · 账号管理"
        desc="多账号快照保存与一键切换 · 快照存 data/profiles_doubao/<uid>/"
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

      {/* 续期巡检摘要 */}
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
              {renewSummary.renew.skipped > 0 ? '（未录入 sessionid 凭证，可在编辑弹框中录入后探活）' : ''}
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
                      <div className="flex items-center gap-2" title={quotaTip(a)}>
                        <span className="font-medium">{a.name}</span>
                        {a.is_current && <Badge tone="green">当前</Badge>}
                        {quotaBadge(a)}
                        {quotaRunningFor === a.user_id && <Badge tone="violet">查询中…</Badge>}
                      </div>
                      <div className="font-mono text-xs text-slate-400">{a.user_id}</div>
                    </td>
                    <td className="max-w-40 truncate px-3 py-2 text-xs text-slate-500" title={a.note}>
                      {a.note || '—'}
                    </td>
                    <td className="px-3 py-2 text-xs">
                      {a.has_snapshot ? (
                        <div title={snapshotTip(a)}>
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
                          title="一键以该账号打开豆包：恢复快照后直接启动（代理运行中自动注入代理）"
                          onClick={() => void openDoubaoAs(a.user_id, proxy.running ? proxy.port : undefined)}
                          disabled={anyBusy || !a.has_snapshot}
                          className="btn-ghost !p-2 text-violet-500 hover:bg-violet-50 disabled:opacity-30 dark:hover:bg-violet-500/10"
                        >
                          <Rocket size={14} />
                        </button>
                        <button
                          title="切换到此账号（自动备份当前登录态到 last 槽 + 原账号槽）"
                          onClick={() => void switchTo(a.user_id, 'Doubao')}
                          disabled={anyBusy || !a.has_snapshot}
                          className="btn-ghost !p-2 text-sky-500 hover:bg-sky-50 disabled:opacity-30 dark:hover:bg-sky-500/10"
                        >
                          <Repeat size={14} />
                        </button>
                        <button
                          title="查询/刷新会员额度（结果缓存后悬停账号名可查看额度状态 / 会员 / 到期时间）"
                          onClick={() => void doFetchQuota(a)}
                          disabled={anyBusy || quotaRunningFor === a.user_id}
                          className="btn-ghost !p-2 text-amber-500 hover:bg-amber-50 disabled:opacity-30 dark:hover:bg-amber-500/10"
                        >
                          <Coins size={14} className={quotaRunningFor === a.user_id ? 'animate-pulse' : ''} />
                        </button>
                        <button
                          title="编辑别名 / 备注 / 会话凭证"
                          onClick={() => {
                            setEditName(a.name === a.user_id ? '' : a.name);
                            setEditNote(a.note);
                            // 回填已存凭证（本地应用明文展示；清空后保存即删除）
                            setEditSessionId(a.session_id ?? '');
                            setEditSidGuard(a.sid_guard ?? '');
                            setDialog({ mode: 'edit', userId: a.user_id, name: a.name, note: a.note, sessionId: a.session_id ?? '', sidGuard: a.sid_guard ?? '' });
                          }}
                          disabled={anyBusy}
                          className="btn-ghost !p-2"
                        >
                          <Pencil size={14} />
                        </button>
                        <button
                          title="删除账号（含快照，需二次确认）"
                          onClick={() => void doRemove(a.user_id)}
                          disabled={anyBusy}
                          className="btn-ghost !p-2 text-rose-500 hover:bg-rose-50 disabled:opacity-30 dark:hover:bg-rose-500/10"
                        >
                          <Trash2 size={14} />
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
          「以账号打开」= 恢复该账号快照后直接启动豆包（代理运行中自动注入代理）；「切换」= 关闭豆包 →
          当前登录态自动备份（last 槽 + 原账号槽）→ 恢复目标快照 → 重启豆包。
          会话保活与每日定时任务在「环境配置」页设置；有会话凭证的账号会自动查询会员额度并在账号名后显示会员/免费标识，悬停账号名可查看详情。
        </div>
      </div>

      {/* 会员额度结果弹框 */}
      <Modal
        open={!!quotaResult}
        onClose={() => setQuotaResult(null)}
        title={`会员额度 · ${accounts.find((a) => a.user_id === quotaResult?.userId)?.name ?? quotaResult?.userId ?? ''}`}
        size="lg"
      >
        {quotaResult && (
          <div className="space-y-3">
            <div className="flex flex-wrap items-center gap-2 text-sm">
              {quotaResult.result.parsed.level != null ? (
                <Badge tone="violet">会员等级：{quotaResult.result.parsed.level}</Badge>
              ) : (
                <Badge tone="slate">未识别到会员等级字段</Badge>
              )}
              {quotaResult.result.parsed.expire_at && (
                <Badge tone="green">到期：{quotaResult.result.parsed.expire_at}</Badge>
              )}
              {quotaResult.result.parsed.is_gift && <Badge tone="amber">活动赠送</Badge>}
              <Badge tone="slate">HTTP {quotaResult.result.http_status}</Badge>
            </div>
            {quotaResult.result.parsed.items.length > 0 ? (
              <div className="space-y-2">
                {quotaResult.result.parsed.items.map((it, i) => {
                  if ('used_percent' in it) {
                    // quota/summary 窗口结构：当前时段 / 近 7 天，used_percent + reset_at
                    const pct = Math.max(0, Math.min(100, Number(it.used_percent) || 0));
                    return (
                      <div key={i} className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
                        <div className="flex items-center justify-between text-sm">
                          <span className="font-medium">{it.name}</span>
                          <span className={`text-xs ${it.exhausted ? 'text-rose-500' : 'text-slate-500'}`}>
                            {it.exhausted ? '已用完' : `已用 ${it.used_percent}%`}
                            {it.reset_at ? ` · ${it.reset_at} 重置` : ''}
                          </span>
                        </div>
                        <div className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-slate-100 dark:bg-zinc-800">
                          <div
                            className={`h-full rounded-full ${it.exhausted ? 'bg-rose-400' : 'bg-gradient-to-r from-emerald-500 to-teal-400'}`}
                            style={{ width: `${pct}%` }}
                          />
                        </div>
                      </div>
                    );
                  }
                  const total = Number(it.total);
                  const left = it.left != null ? Number(it.left) : null;
                  const pct = Number.isFinite(total) && total > 0 && left != null && Number.isFinite(left) ? Math.max(0, Math.min(100, (left / total) * 100)) : null;
                  return (
                    <div key={i} className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
                      <div className="flex items-center justify-between text-sm">
                        <span className="font-medium">{it.name}</span>
                        <span className="text-xs text-slate-500">
                          {left != null ? `剩余 ${it.left} / ${it.total}` : `总量 ${it.total}`}
                          {it.used != null ? ` · 已用 ${it.used}` : ''}
                        </span>
                      </div>
                      {pct != null && (
                        <div className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-slate-100 dark:bg-zinc-800">
                          <div
                            className="h-full rounded-full bg-gradient-to-r from-emerald-500 to-teal-400"
                            style={{ width: `${pct}%` }}
                          />
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            ) : (
              <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-900 dark:text-zinc-400">
                未从响应中识别出额度条目。若接口返回结构与预期不同，请根据下方键路径调整接口地址后重试。
              </div>
            )}
            {quotaResult.result.parsed.subscription?.active ? (
              <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
                <div className="mb-1.5 text-xs font-medium text-slate-500 dark:text-zinc-400">订阅记录</div>
                <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm">
                  <span className="font-medium">{quotaResult.result.parsed.subscription.name}</span>
                  {quotaResult.result.parsed.subscription.period_days != null && (
                    <span className="text-slate-500">{quotaResult.result.parsed.subscription.period_days} 天</span>
                  )}
                  {quotaResult.result.parsed.subscription.is_gift && <Badge tone="amber">活动赠送</Badge>}
                  <Badge tone="green">生效中</Badge>
                </div>
                <div className="mt-1 text-xs text-slate-400">
                  {quotaResult.result.parsed.subscription.start_at ?? '—'} 起 ·{' '}
                  {quotaResult.result.parsed.subscription.expire_at ?? '—'} 到期
                </div>
              </div>
            ) : (
              <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-900 dark:text-zinc-400">
                当前无生效订阅（免费账号：基础对话不限次，额度窗口见上方）。
              </div>
            )}
          </div>
        )}
      </Modal>

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
                会话凭证 sessionid（代理自动回写，可手动修改）
              </label>
              <input
                value={editSessionId}
                onChange={(e) => setEditSessionId(e.target.value)}
                placeholder="启动代理登录豆包后自动写入；也可手动粘贴"
                className="input font-mono text-xs"
              />
              <input
                value={editSidGuard}
                onChange={(e) => setEditSidGuard(e.target.value)}
                placeholder="sid_guard（格式 sid|创建时间|有效期|…，用于到期提醒）"
                className="input mt-2 font-mono text-xs"
              />
              <div className="mt-2 rounded-lg border border-slate-200 p-2.5 dark:border-zinc-700">
                <button onClick={() => void doFillCaptured()} className="btn-outline !py-1 !text-xs">
                  从代理抓包自动填充
                </button>
                <p className="mt-1.5 text-xs leading-relaxed text-slate-500 dark:text-zinc-400">
                  通常无需手动操作：代理开启且豆包流量经过代理时，凭证会<b>自动回写到当前登录账号</b>。
                  也可手动：① 顶栏「启动代理」→ ② 浏览器开启系统代理，登录网页版
                  <code className="mx-1 rounded bg-slate-100 px-1 dark:bg-zinc-800">doubao.com</code>
                  → ③ 回到这里点上方按钮填入。
                </p>
              </div>
              <p className="mt-1 text-xs text-slate-400">
                凭证等同密码，仅存本地。清空两个输入框后保存即删除该账号凭证。
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

      {/* 使用帮助弹框 */}
      <DoubaoHelpModal open={helpOpen} onClose={() => setHelpOpen(false)} />
    </div>
  );
}

/** 豆包账号管理使用帮助（快照机制 / 保存 / 切换 / 保活 / 额度） */
function DoubaoHelpModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal open={open} onClose={onClose} title="豆包账号管理使用帮助" size="lg">
      <div className="space-y-4 text-sm">
        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Save size={15} className="text-amber-500" /> 保存当前登录态
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            在豆包中登录某个账号后，点击右上角「保存当前登录态」。系统会自动识别当前账号 user_id
            （读取 <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">%APPDATA%\Doubao\public_config.json</code>
            ，识别失败时可手动输入），然后关闭豆包 → 将登录核心文件白名单快照到
            <code className="mx-1 rounded bg-slate-100 px-1 dark:bg-zinc-800">data/profiles_doubao/&lt;uid&gt;/</code>
            → 重新启动豆包。每个账号的登录态独立存储，互不干扰。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 首次使用前，请先在豆包中登录目标账号，再点「保存当前登录态」创建快照。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Repeat size={15} className="text-amber-500" /> 切换账号流程
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li>点击目标账号行的「切换」图标</li>
            <li>系统自动关闭豆包，并将当前登录态备份（原账号槽 + <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">last</code> 槽作为安全回退）</li>
            <li>恢复目标账号的快照（Cookies、Local State、Local Storage、saman 账号体系等）</li>
            <li>重新启动豆包，自动以目标账号登录</li>
          </ol>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 目标账号从未保存过登录态时，切换会被中止并提示「无快照」，请先保存该账号的登录态。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Rocket size={15} className="text-amber-500" /> 一键以账号打开
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击账号行的 <b>火箭图标</b>，一步完成「恢复该账号快照 → 启动豆包」，无需先切换再手动打开。
            代理运行中时会自动为豆包注入代理（流量走抓包，凭证自动回写）。切换前登录态仍会自动备份到
            <code className="mx-1 rounded bg-slate-100 px-1 dark:bg-zinc-800">last</code>槽与原账号槽，可放心使用。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Timer size={15} className="text-amber-500" /> 会话保活
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            豆包会话约 30 天滑动续期：只要定期登录/联网使用就会自动延期。「立即保活」会启动豆包约 8
            秒完成会话刷新后关闭；也可在「环境配置」注册每日定时保活任务，无需人工干预。超过 25 天未保活时进入页面会提醒。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <ShieldCheck size={15} className="text-amber-500" /> 会话凭证（自动获取）
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            通常无需任何手动操作：顶栏「启动代理」后，豆包客户端 / 网页版的流量经过代理，工具会自动从
            Cookie 提取 <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">sessionid / sid_guard</code>
            并回写到<b>当前登录账号</b>（本页每 20 秒检查一次，有变化才写入）。编辑弹框中可查看、修改或清空凭证（清空后保存即删除）；凭证等同密码，仅存本地。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Coins size={15} className="text-amber-500" /> 会员额度与标识
          </h3>
          <ul className="ml-4 list-disc space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li>有会话凭证的账号进入本页后会<b>自动查询</b>一次会员额度，账号名后显示会员等级或「免费」标识。</li>
            <li><b>鼠标悬停账号名</b>可查看：额度状态（各额度条目余量）、会员等级、到期时间（免费账号无到期时间）。</li>
            <li>点击「额度」图标可手动刷新；额度接口已内置默认值（代理抓包实测固化），一般无需在「环境配置」修改。</li>
          </ul>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Trash2 size={15} className="text-amber-500" /> 快照规则与删除说明
          </h3>
          <ul className="ml-4 list-disc space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li>每个账号只有<b>一个</b>快照槽（<code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">data/profiles_doubao/&lt;uid&gt;/</code>），重复保存即覆盖更新。</li>
            <li>另有全局唯一的 <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">last</code> 槽：每次切换时自动存放"切换前正在使用的登录态"，下次切换会覆盖——只用于回退一步，防误覆盖。</li>
            <li><b>删除账号</b>会同时移出账号池记录（别名 / 备注 / 凭证）并删除该账号的快照文件，需经过两次确认，删除后该账号需重新登录才能再次使用。</li>
            <li><b>快照版本校验</b>：新快照自带版本元数据（schemaVersion + 豆包内核版本），恢复前自动校验完整性（leveldb 结构），豆包大版本升级后若快照不兼容会中止并提示重新保存；悬停「快照」列可查看版本信息。</li>
            <li><b>IndexedDB（对话历史等）</b>默认不纳入快照以控制体积；可在「环境配置 → 快照设置」开启后重新保存登录态，对话历史等完整状态将随账号迁移。</li>
          </ul>
        </section>
      </div>
    </Modal>
  );
}
