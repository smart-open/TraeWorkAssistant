import { useCallback, useEffect, useRef, useState } from 'react';
import {
  Download,
  ExternalLink,
  Fingerprint,
  FolderCog,
  Globe,
  HelpCircle,
  KeyRound,
  Loader2,
  Pencil,
  RefreshCw,
  ShieldAlert,
  Trash2,
  Upload,
  UserPlus,
  Users,
} from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState, Modal } from '../../components/ui';
// 审查 #16：listen 走 web 桥（lib/tauri），浏览器环境无 @tauri-apps/api 运行时
import { api, listen } from '../../lib/tauri';
import { GroupSelect } from '../accounts/GroupSelect';
import { GroupsModal } from '../accounts/GroupsModal';
import { QoderHelpModal } from './HelpModal';
import { useAppStore } from '../../store';
import type {
  GroupView,
  QoderAccountView,
  QoderOauthDone,
  QoderOauthProgress,
} from '../../types';

/**
 * qoder-accounts 账号管理（F-80 §5.8，对照 BuddyAccounts 裁剪复刻）：
 * 账号池列表 + 凭证来源徽标 + PAT 导入 / OAuth 设备流双通道，分组管理、
 * 凭证续期、积分余额、账号池导出/导入。
 * docker 版裁剪：桌面专属的账号切换/快照管理/CLI 状态桥/环境重置/IDE 扫描链路已移除。
 */

const PAT_URL = 'https://qoder.com.cn/account/integrations';

/** 破坏性操作确认弹框目标（禁 window.confirm，红线）：移除账号 */
type QoderConfirm = { kind: 'remove-account'; account: QoderAccountView } | null;

/** 到期时间格式化 + 剩余天数色阶（对齐 BuddyAccounts tokenTone/fmtExpire；秒级时间戳） */
function fmtExpire(ts: number | null | undefined): string {
  if (!ts) return '—';
  const d = new Date(ts * 1000);
  return `${d.getFullYear()}/${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')}`;
}

function tokenExpireTone(ts: number | null | undefined): string {
  if (ts == null) return 'text-slate-400';
  const days = (ts * 1000 - Date.now()) / 86400000;
  if (days < 0) return 'text-rose-500';
  if (days < 1) return 'text-amber-500';
  return 'text-emerald-600 dark:text-emerald-400';
}

/** Token 状态：状态徽标 + 到期时间（PAT 长期凭证无过期信息时标注「长期有效」） */
function TokenBadge({ a }: { a: QoderAccountView }) {
  if (!a.has_credential) return <Badge tone="red">无凭证</Badge>;
  if (a.needs_relogin) return <Badge tone="red" title={a.relogin_reason}>需重新登录</Badge>;
  return (
    <div className="flex flex-col items-start gap-0.5">
      <Badge tone="green">{a.token_kind === 'pat' ? 'PAT 有效' : 'Token 有效'}</Badge>
      <span className={`text-[11px] tabular-nums ${tokenExpireTone(a.token_expires_at)}`}>
        {a.token_expires_at != null ? fmtExpire(a.token_expires_at) : a.token_kind === 'pat' ? '长期有效' : '—'}
      </span>
    </div>
  );
}

/** 设备指纹徽标（§5.10）：machine_id 前 8 位，点击查看完整指纹 */
function FingerprintBadge({ a, onOpen }: { a: QoderAccountView; onOpen: (a: QoderAccountView) => void }) {
  if (!a.fingerprint) return <Badge tone="slate">未绑定</Badge>;
  return (
    <button
      className="inline-flex cursor-pointer items-center"
      title="查看完整设备指纹"
      onClick={() => onOpen(a)}
    >
      <Badge tone="violet">
        <Fingerprint size={10} className="mr-1" />
        {a.fingerprint}…
      </Badge>
    </button>
  );
}

export default function QoderAccounts() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [accounts, setAccounts] = useState<QoderAccountView[]>([]);
  const [loading, setLoading] = useState(true);
  // 帮助弹框（对齐 BuddyAccounts leftExtra 帮助入口）
  const [helpOpen, setHelpOpen] = useState(false);
  // 账号分组（强加 Buddy 分组体系：chips 过滤 + GroupSelect 列 + 分组管理弹窗）
  const [qoderGroups, setQoderGroups] = useState<GroupView[]>([]);
  const [groupOpen, setGroupOpen] = useState(false);
  const [filter, setFilter] = useState('all');
  // PAT 导入弹框
  const [showImport, setShowImport] = useState(false);
  const [patName, setPatName] = useState('');
  const [patValue, setPatValue] = useState('');
  const [importing, setImporting] = useState(false);
  // 行内凭证续期（对照 BuddyAccounts rowOp refresh）：执行中该行按钮 spinner + 禁用
  const [renewingId, setRenewingId] = useState<string | null>(null);
  // 编辑弹框（改名/备注）
  const [editing, setEditing] = useState<QoderAccountView | null>(null);
  const [editName, setEditName] = useState('');
  const [editNote, setEditNote] = useState('');
  const [editBusy, setEditBusy] = useState(false);
  // 指纹查看弹框（§5.10）
  const [fpViewing, setFpViewing] = useState<QoderAccountView | null>(null);
  // OAuth 设备流登录（进度弹窗；事件契约对齐 BuddyAccounts wb-oauth 模式）
  const [oauthRunning, setOauthRunning] = useState(false);
  // OAuth 兼容模式开关（2026-10-02 审查预案）：上次授权超时后置 true，下一次点击
  // 改用不带 client_id 的授权 URL；成功后复位。ref 而非 state：不触发重渲染
  const oauthCompatRef = useRef(false);
  const [oauthCanceling, setOauthCanceling] = useState(false);
  const [oauthMsg, setOauthMsg] = useState('');
  const [oauthUrl, setOauthUrl] = useState<string | null>(null);
  const [showOauth, setShowOauth] = useState(false);
  // 破坏性操作确认弹框（禁 window.confirm，红线）：移除账号
  const [confirmTarget, setConfirmTarget] = useState<QoderConfirm>(null);
  const [confirmBusy, setConfirmBusy] = useState(false);
  // 导出/导入账号池（M4，对照 BuddyAccounts F-46 扩展）
  const [exportOpen, setExportOpen] = useState(false);
  const [exportWithCreds, setExportWithCreds] = useState(false);
  // 含凭证导出的二次确认弹框（审查 P0-2；禁 window.confirm，红线）
  const [credExportConfirm, setCredExportConfirm] = useState(false);
  // 含凭证导出密码（审查 P1-1：凭证 AES-256-GCM 加密导出，非空 + 两次一致才放行）
  const [exportPwd, setExportPwd] = useState('');
  const [exportPwd2, setExportPwd2] = useState('');
  const [exportBusy, setExportBusy] = useState(false);
  // 导入账号池弹框：文件 + 可选解密密码（仅加密导出文件需要；旧明文文件免密兼容）
  const [importPoolOpen, setImportPoolOpen] = useState(false);
  const [importPoolFile, setImportPoolFile] = useState<File | null>(null);
  const [importPoolPwd, setImportPoolPwd] = useState('');
  const [importingBackup, setImportingBackup] = useState(false);
  const unlisten = useRef<(() => void)[]>([]);
  // OAuth 完成延迟关弹框的定时器（卸载时清理，防卸载后 setState）
  const oauthTimers = useRef<number[]>([]);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setAccounts(await api.qoder.accountsList());
    } catch (err) {
      pushToast('error', `读取账号失败：${String(err)}`);
    } finally {
      setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 分组列表局部刷新（对齐 BuddyAccounts reloadGroups：不整表刷新）
  const reloadGroups = useCallback(() => {
    api.qoder.groups.list().then(setQoderGroups).catch(() => setQoderGroups([]));
  }, []);

  // 分组过滤（对齐 BuddyAccounts：全部 / 未分组 / 指定分组）
  const filtered =
    filter === 'all'
      ? accounts
      : filter === 'ungrouped'
        ? accounts.filter((a) => !a.group_id)
        : accounts.filter((a) => a.group_id === filter);

  useEffect(() => {
    void refresh();
    reloadGroups();
    let disposed = false;
    void listen<QoderOauthProgress>('qoder-oauth-progress', (ev) => {
      const p = ev.payload;
      setOauthMsg(p.message);
      if (p.auth_url) setOauthUrl(p.auth_url);
    }).then((u) => {
      if (disposed) u();
      else unlisten.current.push(u);
    });
    void listen<QoderOauthDone>('qoder-oauth-done', (ev) => {
      const d = ev.payload;
      setOauthRunning(false);
      setOauthCanceling(false);
      setOauthMsg(d.message);
      // 兼容模式切换（2026-10-02 审查预案）：授权超时且可能因官方 client_id 被轮换
      // 导致授权页「参数无效」→ 下一次点击自动改用兼容模式（不带 client_id）；
      // 成功即复位回常规模式
      if (!d.ok && d.message.includes('超时')) {
        oauthCompatRef.current = true;
      } else if (d.ok) {
        oauthCompatRef.current = false;
      }
      if (d.ok) {
        pushToast('success', d.message);
        void refresh();
        oauthTimers.current.push(window.setTimeout(() => setShowOauth(false), 1200));
      } else {
        pushToast('error', d.message);
      }
    }).then((u) => {
      if (disposed) u();
      else unlisten.current.push(u);
    });
    return () => {
      disposed = true;
      unlisten.current.forEach((u) => u());
      unlisten.current = [];
      oauthTimers.current.forEach((t) => clearTimeout(t));
      oauthTimers.current = [];
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refresh]);

  // OAuth 完成看门狗（对齐 BuddyAccounts wb-oauth 同款兜底）：后端轮询最长 180s，
  // 看门狗 310s 留足事件送达余量；done 事件异常缺失（桥挂死/事件丢失/后台线程 panic）
  // 时 oauthRunning 永挂，OAuth/IDE 扫描按钮永久禁用。310s 后解除等待并提示；迟到的 done 事件仍会正常提示。
  useEffect(() => {
    if (!oauthRunning) return;
    const timer = setTimeout(() => {
      setOauthRunning(false);
      setOauthCanceling(false);
      pushToast('warn', 'OAuth 登录超过 5 分钟未收到结果事件，已解除等待；结果请以账号列表为准');
    }, 310_000);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [oauthRunning]);

  const startOauth = async () => {
    setOauthRunning(true);
    setOauthCanceling(false);
    // 兼容模式（上次授权超时后自动切换）：授权 URL 不带 client_id（预案见 qoder_oauth.rs）
    const compat = oauthCompatRef.current;
    setOauthMsg(compat ? '正在打开 Qoder 授权页（兼容模式）…' : '正在打开 Qoder 授权页…');
    if (compat) pushToast('info', '本次 OAuth 使用兼容模式（不带 client_id），用于绕过授权页「参数无效」');
    setOauthUrl(null);
    setShowOauth(true);
    try {
      await api.qoder.oauthLogin(compat);
    } catch (err) {
      setOauthRunning(false);
      setOauthCanceling(false);
      setShowOauth(false);
      pushToast('error', `发起 OAuth 登录失败：${String(err)}`);
    }
  };

  // 取消授权（弹框「取消授权」/运行中关闭弹框时自动触发）：后端轮询线程自行发失败终态
  const cancelOauth = async () => {
    setOauthCanceling(true);
    try {
      await api.qoder.oauthCancel();
    } catch {
      // 后端取消失败不阻塞 UI：终态仍由 done 事件或 180s 超时兜底
    } finally {
      setOauthCanceling(false);
    }
  };

  // 关闭 OAuth 弹框：运行中先发取消（避免后台轮询空转至超时），其余直接关
  const closeOauthModal = () => {
    if (oauthRunning) void cancelOauth();
    setShowOauth(false);
  };

  const importPat = async () => {
    const token = patValue.trim();
    if (!token) {
      pushToast('warn', '请粘贴 PAT（pt- 前缀，qoder.com.cn/account/integrations 创建）');
      return;
    }
    // 前缀本地预检（对齐后端 accounts.rs 格式门禁）：pt- = 官方 PAT，jt- = 客户端
    // job token；其余前缀多为误贴其他平台凭证，提前提示避免无谓请求
    if (!token.startsWith('pt-') && !token.startsWith('jt-')) {
      pushToast('warn', '凭证前缀不识别（应为 pt- PAT 或 jt- job token），请检查是否误贴其他平台凭证');
      return;
    }
    setImporting(true);
    try {
      const v = await api.qoder.accountImportPat(patName.trim() || undefined, token);
      pushToast('success', `账号已导入：${v.nickname || v.id}`);
      closeImport();
      void refresh();
    } catch (err) {
      pushToast('error', `导入失败：${String(err)}`);
    } finally {
      setImporting(false);
    }
  };

  // 关闭 PAT 导入弹框：PAT 等同密码，关闭时一并清空输入框残留（备注名同步重置）
  const closeImport = () => {
    setShowImport(false);
    setPatName('');
    setPatValue('');
  };

  const saveEdit = async () => {
    if (!editing || editBusy) return;
    setEditBusy(true);
    try {
      await api.qoder.accountSave(editing.id, editName.trim() || undefined, editNote);
      pushToast('success', '已保存');
      setEditing(null);
      void refresh();
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setEditBusy(false);
    }
  };

  // 移除账号：先弹确认弹框（禁 window.confirm，红线），确认后由 confirmDestructive 执行
  const removeAccount = (a: QoderAccountView) => {
    setConfirmTarget({ kind: 'remove-account', account: a });
  };

  // 凭证续期（对照 BuddyAccounts handleRefreshToken）：force 恒刷，成功用返回的最新视图原位更新行
  const handleRenewToken = async (a: QoderAccountView) => {
    setRenewingId(a.id);
    try {
      const view = await api.qoder.accountRefreshToken(a.id);
      setAccounts((prev) => prev.map((x) => (x.id === a.id ? view : x)));
      pushToast('success', `「${a.nickname || a.id}」凭证已续期`);
    } catch (err) {
      pushToast('error', `续期失败：${String(err)}`);
      // 失败也可能已回写 needs_relogin 等状态：静默重取对齐
      //（不走 refresh()，避免整表 loading 行闪烁）
      api.qoder.accountsList().then(setAccounts).catch(() => {});
    } finally {
      setRenewingId(null);
    }
  };

  // 确认弹框执行器（移除账号）
  const confirmDestructive = async () => {
    const t = confirmTarget;
    if (!t) return;
    setConfirmBusy(true);
    try {
      await api.qoder.accountRemove(t.account.id);
      pushToast('success', '已移除');
      void refresh();
      setConfirmTarget(null);
    } catch (err) {
      pushToast('error', `移除失败：${String(err)}`);
    } finally {
      setConfirmBusy(false);
    }
  };

  const copyText = async (text: string, label: string) => {
    if (!text) {
      pushToast('warn', `${label}为空，暂无可复制内容`);
      return;
    }
    try {
      await navigator.clipboard.writeText(text);
      pushToast('success', `${label} 已复制`);
    } catch {
      pushToast('error', '复制失败');
    }
  };

  // 导出确认（M4，对照 BuddyAccounts F-46 扩展）：可选是否附带凭证副本。
  // 含凭证时先弹独立确认弹框（审查 P0-2；禁 window.confirm，红线），
  // 弹框内强制设置导出密码（审查 P1-1：凭证加密导出，明文凭证不再落盘）
  const exportPwdReady = exportPwd.trim().length > 0 && exportPwd === exportPwd2;
  const confirmExport = () => {
    if (exportWithCreds) {
      setCredExportConfirm(true);
      return;
    }
    void doExport();
  };

  const doExport = async () => {
    // 双保险：含凭证导出必须有合法密码（正常路径由弹框校验保证）
    if (exportWithCreds && !exportPwdReady) {
      pushToast('warn', '请先设置导出密码（两次输入需一致）');
      return;
    }
    setCredExportConfirm(false);
    setExportBusy(true);
    try {
      const data = await api.qoder.accountsExport(exportWithCreds, exportWithCreds ? exportPwd || undefined : undefined);
      const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      // 本地日期命名（对齐 QoderCheckin/Dashboard：toISOString 为 UTC，跨日会错一天）
      const now = new Date();
      const localDate = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}-${String(now.getDate()).padStart(2, '0')}`;
      a.href = url;
      a.download = `qoder_accounts_${localDate}.json`;
      a.click();
      // 延迟回收 blob URL：click() 后立即 revoke 可能中断部分浏览器对 blob 的异步读取
      setTimeout(() => URL.revokeObjectURL(url), 1_000);
      pushToast(
        'success',
        exportWithCreds
          ? '账号池已导出（含凭证，已用导出密码加密；导入时需输入同一密码）'
          : '账号元数据已导出（凭证不导出）',
      );
      setExportOpen(false);
      setExportPwd('');
      setExportPwd2('');
    } catch (err) {
      pushToast('error', `导出失败：${String(err)}`);
    } finally {
      setExportBusy(false);
    }
  };

  // 关闭导出密码弹框（取消/点遮罩关闭，导出成功路径亦清空）：密码等同敏感凭证，
  // 关闭即清空输入残留，避免下次导出被预填（审查 F2）
  const closeCredExport = () => {
    if (exportBusy) return;
    setCredExportConfirm(false);
    setExportPwd('');
    setExportPwd2('');
  };

  // 导入账号池（M4）：选择导出文件 → kind 校验入池（uid 幂等原位更新；含凭证回写）。
  // 密码可选：仅加密导出文件（AIWQENC1 信封）需要；旧明文导出文件免密向后兼容
  const importBackupFile = async (file: File, password: string) => {
    setImportingBackup(true);
    try {
      const payload = JSON.parse(await file.text()) as Record<string, unknown>;
      const r = await api.qoder.accountsImport(payload, password || undefined);
      const parts = [`新增 ${r.added} 个账号`];
      if (r.updated > 0) parts.push(`更新 ${r.updated} 个`);
      pushToast('success', `导入完成：${parts.join('、')}，带凭证 ${r.with_credentials}`);
      // F-80-余 v2 迁移提示：历史明文含凭证文件 → 建议重新以加密格式导出归档
      //（不自动改动用户文件，仅提示）
      if (r.plaintext_credentials) {
        pushToast(
          'warn',
          '该导出文件为历史明文格式，凭证以明文保存存在泄露风险：建议重新导出并设置导出密码（AES-256-GCM 加密），替换归档文件',
        );
      }
      if (r.rejected && r.rejected.length > 0) {
        const head = r.rejected
          .slice(0, 3)
          .map((x) => `「${x.id}」${x.reason}`)
          .join('；');
        pushToast('warn', `${r.rejected.length} 条被拒绝导入：${head}${r.rejected.length > 3 ? '…' : ''}`);
      }
      setImportPoolOpen(false);
      setImportPoolFile(null);
      setImportPoolPwd('');
      await refresh();
    } catch (err) {
      pushToast('error', `导入失败：${String(err)}`);
    } finally {
      setImportingBackup(false);
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Qoder · 账号管理"
        desc="全家桶账号池 · PAT / OAuth 双通道 · 分组管理 · 导出导入"
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
            <button onClick={() => void refresh()} className="btn-outline" disabled={loading}>
              <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 刷新
            </button>
            <button
              className="btn-outline"
              disabled={oauthRunning}
              onClick={() => void startOauth()}
              title="模拟客户端设备流：浏览器授权后自动获取 dt- 凭证入池"
            >
              {oauthRunning ? <Loader2 size={15} className="animate-spin" /> : <Globe size={15} />} OAuth登录
            </button>
            <button className="btn-outline" disabled={importing} onClick={() => setShowImport(true)}>
              <UserPlus size={15} /> 导入 PAT
            </button>
            <button
              className="btn-outline"
              disabled={accounts.length === 0 || exportBusy}
              onClick={() => setExportOpen(true)}
              title="导出账号池为 JSON（可选是否附带凭证副本）"
            >
              <Download size={15} /> 导出账号
            </button>
            <button
              className="btn-outline"
              disabled={importingBackup}
              onClick={() => setImportPoolOpen(true)}
              title="导入账号池 JSON（uid 幂等合并，设备指纹仅在本地为空时补入；加密导出文件需解密密码）"
            >
              {importingBackup ? <Loader2 size={15} className="animate-spin" /> : <Upload size={15} />} 导入账号
            </button>
            <button onClick={() => setGroupOpen(true)} className="btn-outline" title="管理账号分组">
              <FolderCog size={15} /> 分组管理
            </button>
          </>
        }
      />

      {/* 分组过滤 chips（对齐 BuddyAccounts：全部/未分组/各分组带 count 与色点） */}
      {accounts.length > 0 && (
        <div className="mb-3 mt-5 flex flex-wrap items-center gap-2 text-sm">
          <button
            onClick={() => setFilter('all')}
            className={`chip border ${filter === 'all' ? 'border-brand-500 text-brand-600 dark:text-brand-400' : 'border-slate-300 text-slate-500 dark:border-zinc-700 dark:text-zinc-400'}`}
          >
            全部 ({accounts.length})
          </button>
          <button
            onClick={() => setFilter('ungrouped')}
            className={`chip border ${filter === 'ungrouped' ? 'border-brand-500 text-brand-600 dark:text-brand-400' : 'border-slate-300 text-slate-500 dark:border-zinc-700 dark:text-zinc-400'}`}
          >
            未分组 ({accounts.filter((a) => !a.group_id).length})
          </button>
          {qoderGroups.map((g) => (
            <button
              key={g.id}
              onClick={() => setFilter(g.id)}
              className={`chip border ${filter === g.id ? 'border-brand-500 text-brand-600 dark:text-brand-400' : 'border-slate-300 text-slate-500 dark:border-zinc-700 dark:text-zinc-400'}`}
              style={{ borderColor: filter === g.id ? g.color : undefined }}
            >
              <span className="inline-block h-2 w-2 rounded-full" style={{ background: g.color }} />
              {g.name} ({g.count})
            </button>
          ))}
        </div>
      )}

      {/* 账号池列表（对齐 BuddyAccounts：无账号 EmptyState，有账号全宽卡片表格） */}
      {accounts.length === 0 ? (
        <div className="mt-5">
          {loading ? (
            <div className="flex items-center justify-center gap-2 py-12 text-sm text-slate-400">
              <Loader2 size={16} className="animate-spin" /> 加载中…
            </div>
          ) : (
            <EmptyState
              icon={<Users size={26} />}
              title="暂无 Qoder 账号"
              hint="三种方式入池：导入 PAT（qoder.com.cn → Integrations 创建）/ OAuth 设备流登录 / 扫描本地账号（本机已登录 Qoder CN IDE 时一键导入）。"
            />
          )}
        </div>
      ) : (
      <div className="mt-5 card overflow-x-auto">
        <table className="w-full min-w-[980px] text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                <tr>
                  <th className="px-4 py-2 text-left">账号</th>
                  <th className="px-4 py-2 text-left">分组</th>
                  <th className="px-4 py-2 text-left">套餐</th>
                  <th className="px-4 py-2 text-left">凭证来源</th>
                  <th className="px-4 py-2 text-left">Token 状态</th>
                  <th className="px-4 py-2 text-left">设备指纹</th>
                  <th className="px-4 py-2 text-right">积分余额</th>
                  <th className="px-4 py-2 text-right">操作</th>
                </tr>
              </thead>
            <tbody>
              {loading ? (
                <tr>
                  <td colSpan={8} className="px-3 py-4 text-center text-xs text-slate-400">加载中…</td>
                </tr>
              ) : (
                filtered.map((a) => (
                  <tr key={a.id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                    <td className="px-4 py-3">
                      <div className="flex flex-wrap items-center gap-1.5">
                        <span className="font-medium">{a.nickname || a.id}</span>
                      </div>
                      <div className="text-xs text-slate-400">{[a.uid, a.note].filter(Boolean).join(' · ') || a.id}</div>
                    </td>
                    <td className="px-4 py-3">
                      <GroupSelect
                        value={a.group_id || null}
                        groups={qoderGroups}
                        onChange={(gid) => {
                          void api.qoder.accountMove(a.id, gid).then(() => {
                            setAccounts((prev) => prev.map((x) => (x.id === a.id ? { ...x, group_id: gid ?? '' } : x)));
                            reloadGroups();
                          }).catch((err) => pushToast('error', `分组调整失败：${String(err)}`));
                        }}
                      />
                    </td>
                    <td className="px-4 py-3 text-xs text-slate-500">
                      {a.plan ? <span className="font-medium text-sky-600 dark:text-sky-400">{a.plan}</span> : <span className="text-slate-300 dark:text-zinc-600">—</span>}
                    </td>
                    <td className="px-4 py-3">
                      <Badge tone={a.credential_source === 'pat' ? 'green' : 'slate'}>
                        {a.credential_source || '—'}
                      </Badge>
                    </td>
                    <td className="px-4 py-3"><TokenBadge a={a} /></td>
                    <td className="px-4 py-3"><FingerprintBadge a={a} onOpen={setFpViewing} /></td>
                    <td className="px-4 py-3 text-right tabular-nums text-xs">
                      {a.credits_balance != null ? a.credits_balance.toLocaleString() : '-'}
                    </td>
                    <td className="px-4 py-3 text-right">
                      <div className="flex justify-end gap-1">
                        <button
                          className="btn-ghost !p-2 text-amber-500 hover:bg-amber-50 dark:hover:bg-amber-500/10"
                          title={renewingId === a.id ? '凭证续期中…' : '凭证续期（PAT 重换作业令牌 / refresh_token 换新 access_token）'}
                          disabled={renewingId != null || !a.has_credential}
                          onClick={() => void handleRenewToken(a)}
                        >
                          {renewingId === a.id ? <Loader2 size={14} className="animate-spin" /> : <KeyRound size={14} />}
                        </button>
                        <button
                          className="btn-ghost !p-2"
                          title="编辑名称/备注"
                          onClick={() => {
                            setEditing(a);
                            setEditName(a.nickname);
                            setEditNote(a.note);
                          }}
                        >
                          <Pencil size={14} />
                        </button>
                        <button
                          className="btn-ghost !p-2 text-rose-500 hover:bg-rose-50 dark:hover:bg-rose-500/10"
                          title="移除账号"
                          onClick={() => removeAccount(a)}
                        >
                          <Trash2 size={14} />
                        </button>
                      </div>
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      )}

      <p className="mt-3 text-xs text-slate-400">
        凭证双通道：导入 PAT（官方认可，pt- 前缀）/ OAuth 设备流（dt-，约 30 天自动续期）。
        同一账号的 PAT 与客户端凭证按 token 派生 id，分属两条池记录。
      </p>

      {/* OAuth 进度弹框（随时可关；运行中关闭自动取消后台轮询） */}
      <Modal open={showOauth} onClose={closeOauthModal} title="Qoder OAuth 登录"
        footer={
          oauthRunning ? (
            <button className="btn-outline" onClick={closeOauthModal}>
              {oauthCanceling ? <Loader2 size={14} className="animate-spin" /> : null}
              取消授权
            </button>
          ) : (
            <button className="btn-outline" onClick={() => setShowOauth(false)}>关闭</button>
          )
        }
      >
        <div className="space-y-3">
          <div className="flex items-center gap-2 text-sm">
            {oauthRunning ? (
              <Loader2 size={16} className="animate-spin text-violet-500" />
            ) : (
              <KeyRound size={16} className="text-emerald-500" />
            )}
            <span>{oauthMsg || '等待授权…'}</span>
          </div>
          {oauthUrl && (
            <div className="rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
              <p className="mb-1 text-slate-400">
                若浏览器未自动打开，请手动访问授权页（勿泄露该链接）：
              </p>
              <p className="break-all font-mono text-[11px] text-slate-500 dark:text-zinc-400">{oauthUrl}</p>
            </div>
          )}
          <p className="text-xs text-slate-400">
            授权完成后本工具自动获取设备凭证（约 30 天有效，自动续期）入池，无需手工创建 PAT。
          </p>
        </div>
      </Modal>

      {/* 破坏性操作确认弹框（禁 window.confirm，红线）：移除账号 */}
      <Modal
        open={confirmTarget != null}
        onClose={() => {
          if (!confirmBusy) setConfirmTarget(null);
        }}
        title="移除账号"
        footer={
          <>
            <button className="btn-outline" disabled={confirmBusy} onClick={() => setConfirmTarget(null)}>
              取消
            </button>
            <button
              className="btn-primary !bg-rose-600 hover:!bg-rose-500"
              disabled={confirmBusy}
              onClick={() => void confirmDestructive()}
            >
              {confirmBusy ? <Loader2 size={14} className="animate-spin" /> : null}
              确认移除
            </button>
          </>
        }
      >
        <div className="text-sm">
          {confirmTarget?.kind === 'remove-account' && (
            <>
              确认移除账号「{confirmTarget.account.nickname || confirmTarget.account.id}」？
              <div className="mt-1 text-xs text-rose-500">将同时清除其凭证记录与设备指纹。</div>
            </>
          )}
        </div>
      </Modal>

      {/* PAT 导入弹框 */}
      <Modal
        open={showImport}
        onClose={closeImport}
        title="导入 Qoder PAT"
        footer={
          <>
            <button className="btn-outline" onClick={closeImport}>取消</button>
            <button className="btn-primary" disabled={importing} onClick={() => void importPat()}>
              {importing ? '导入中…' : '导入'}
            </button>
          </>
        }
      >
        <div className="space-y-3">
          <div className="flex items-start gap-2 rounded-lg border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
            <KeyRound size={14} className="mt-0.5 shrink-0" />
            <span>
              PAT 仅在创建页关闭前可见一次，请先在
              <a href={PAT_URL} target="_blank" rel="noreferrer" className="mx-1 inline-flex items-center gap-0.5 underline">
                qoder.com.cn/account/integrations <ExternalLink size={10} />
              </a>
              创建后立即粘贴到下方。PAT 等同密码，仅存储在本机。
            </span>
          </div>
          <label className="block text-sm">
            <span className="mb-1 block text-xs text-slate-500">备注名（可选）</span>
            <input
              className="input w-full"
              value={patName}
              onChange={(e) => setPatName(e.target.value)}
              placeholder="如：主号 / 工作号"
            />
          </label>
          <label className="block text-sm">
            <span className="mb-1 block text-xs text-slate-500">Personal Access Token（pt- 前缀）</span>
            <input
              className="input w-full font-mono"
              value={patValue}
              onChange={(e) => setPatValue(e.target.value)}
              placeholder="pt-..."
              type="password"
            />
          </label>
        </div>
      </Modal>

      {/* 指纹查看弹框（§5.10 每账号稳定绑定） */}
      <Modal
        open={fpViewing != null}
        onClose={() => setFpViewing(null)}
        title={`设备指纹 · ${fpViewing?.nickname || fpViewing?.id || ''}`}
        footer={
          <button className="btn-outline" onClick={() => setFpViewing(null)}>关闭</button>
        }
      >
        {fpViewing && (
          <div className="space-y-3">
            <div className="flex items-start gap-2 rounded-lg border border-violet-200 bg-violet-50 p-3 text-xs text-violet-700 dark:border-violet-500/30 dark:bg-violet-500/10 dark:text-violet-300">
              <Fingerprint size={14} className="mt-0.5 shrink-0" />
              <span>
                每账号稳定绑定指纹（多账号并发）：入池时生成一次并持久保存，永不轮换。
                签到/积分请求缺少真实捕获设备头时，以 machine_id 注入 Cosy-MachineId，
                machine_token 每次现场随机（服务端无强绑定校验）。移除账号将同步删除其指纹。
              </span>
            </div>
            {(
              [
                ['Cosy-MachineId（machine_id）', fpViewing.device_profile?.machine_id],
                ['Device ID（device_id）', fpViewing.device_profile?.device_id],
                ['UMID（umid）', fpViewing.device_profile?.umid],
              ] as const
            ).map(([label, val]) => (
              <div key={label} className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
                <div className="mb-1 flex items-center justify-between">
                  <span className="text-xs text-slate-400">{label}</span>
                  <button
                    className="btn-ghost h-6 !px-2 text-[11px]"
                    onClick={() => void copyText(val || '', label)}
                  >
                    复制
                  </button>
                </div>
                <p className="break-all font-mono text-xs text-slate-600 dark:text-zinc-300">
                  {val || '—'}
                </p>
              </div>
            ))}
          </div>
        )}
      </Modal>

      {/* 编辑弹框 */}
      <Modal
        open={editing != null}
        onClose={() => setEditing(null)}
        title="编辑账号"
        footer={
          <>
            <button className="btn-outline" onClick={() => setEditing(null)}>取消</button>
            <button className="btn-primary" disabled={editBusy} onClick={() => void saveEdit()}>
              {editBusy ? '保存中…' : '保存'}
            </button>
          </>
        }
      >
        <div className="space-y-3">
          <label className="block text-sm">
            <span className="mb-1 block text-xs text-slate-500">显示名</span>
            <input className="input w-full" value={editName} onChange={(e) => setEditName(e.target.value)} />
          </label>
          <label className="block text-sm">
            <span className="mb-1 block text-xs text-slate-500">备注</span>
            <input className="input w-full" value={editNote} onChange={(e) => setEditNote(e.target.value)} />
          </label>
        </div>
      </Modal>

      {/* 导出账号池弹框（M4，对照 BuddyAccounts F-46 扩展）：可选是否附带凭证副本 */}
      <Modal
        open={exportOpen}
        onClose={() => {
          if (!exportBusy) setExportOpen(false);
        }}
        title="导出 Qoder 账号池"
        footer={
          <>
            <button className="btn-outline" disabled={exportBusy} onClick={() => setExportOpen(false)}>
              取消
            </button>
            <button className="btn-primary" disabled={exportBusy} onClick={() => confirmExport()}>
              {exportBusy ? '导出中…' : '确认导出'}
            </button>
          </>
        }
      >
        <div className="space-y-3">
          <label className="flex cursor-pointer items-start gap-2 text-sm">
            <input
              type="checkbox"
              className="mt-1"
              checked={exportWithCreds}
              onChange={(e) => setExportWithCreds(e.target.checked)}
            />
            <span>
              附带凭证副本（access_token / PAT）
              <span className="ml-1 text-xs text-slate-400">不勾选时仅导出元数据，导入后需重新获取凭证</span>
            </span>
          </label>
          <div className="flex items-start gap-2 rounded-lg border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
            <ShieldAlert size={14} className="mt-0.5 shrink-0" />
            <span>
              勾选后须设置导出密码，凭证以 AES-256-GCM 加密写入导出文件；请牢记密码，
              导入时需输入同一密码。密码与文件均请妥善保管，切勿通过不可信渠道传输。
            </span>
          </div>
          <p className="text-xs text-slate-400">
            导出格式 kind=aiwork-qoder-pool；导入端按 uid 幂等合并——已有账号仅补全空缺字段，设备指纹仅在本地为空时补入，绝不覆盖。
          </p>
        </div>
      </Modal>

      {/* 含凭证导出二次确认弹框（审查 P0-2；禁 window.confirm，红线）：
          红色警示框 + 「我已知晓风险」按钮（对齐 Accounts/BuddyAccounts 凭证导出标准）；
          强制设置导出密码（审查 P1-1），凭证以 AES-256-GCM 加密后才写入导出文件 */}
      <Modal
        open={credExportConfirm}
        onClose={() => closeCredExport()}
        title="设置导出密码（加密凭证）"
        footer={
          <>
            <button className="btn-outline" disabled={exportBusy} onClick={() => closeCredExport()}>
              取消
            </button>
            <button
              className="btn-primary !bg-rose-600 hover:!bg-rose-500"
              disabled={exportBusy || !exportPwdReady}
              title={!exportPwdReady ? '需设置非空密码且两次输入一致' : '加密后导出凭证'}
              onClick={() => void doExport()}
            >
              {exportBusy ? '导出中…' : '我已知晓风险，加密并导出'}
            </button>
          </>
        }
      >
        <div className="space-y-3 text-sm">
          <div className="flex items-start gap-2 rounded-lg border border-rose-200 bg-rose-50 p-3 text-xs text-rose-700 dark:border-rose-500/30 dark:bg-rose-500/10 dark:text-rose-300">
            <ShieldAlert size={14} className="mt-0.5 shrink-0" />
            <span>
              导出文件将包含账号凭证（accessToken / refreshToken / PAT，等同密码），将以
              AES-256-GCM 加密后写入：不设置密码无法导出。请牢记密码并妥善保管文件，
              密码丢失将无法导入，且切勿通过不可信渠道传输。
            </span>
          </div>
          <label className="block text-sm">
            <span className="mb-1 block text-xs text-slate-500">导出密码（非空）</span>
            <input
              className="input w-full font-mono"
              value={exportPwd}
              onChange={(e) => setExportPwd(e.target.value)}
              type="password"
              placeholder="至少 1 个字符，建议 8 位以上强密码"
              autoComplete="new-password"
            />
          </label>
          <label className="block text-sm">
            <span className="mb-1 block text-xs text-slate-500">确认密码（两次输入需一致）</span>
            <input
              className="input w-full font-mono"
              value={exportPwd2}
              onChange={(e) => setExportPwd2(e.target.value)}
              type="password"
              autoComplete="new-password"
            />
          </label>
          {exportPwd2.length > 0 && !exportPwdReady && (
            <p className="text-xs text-rose-600 dark:text-rose-400">两次输入不一致，请检查后重试</p>
          )}
        </div>
      </Modal>

      {/* 导入账号池弹框：文件选择 + 可选解密密码（审查 P1-1；旧明文导出文件免密兼容） */}
      <Modal
        open={importPoolOpen}
        onClose={() => {
          // 密码等同凭证：X/遮罩关闭同样清理，避免残留且重开预填（对齐取消按钮）
          if (!importingBackup) {
            setImportPoolOpen(false);
            setImportPoolFile(null);
            setImportPoolPwd('');
          }
        }}
        title="导入 Qoder 账号池"
        footer={
          <>
            <button
              className="btn-outline"
              disabled={importingBackup}
              onClick={() => {
                setImportPoolOpen(false);
                setImportPoolFile(null);
                setImportPoolPwd('');
              }}
            >
              取消
            </button>
            <button
              className="btn-primary"
              disabled={importingBackup || !importPoolFile}
              title={!importPoolFile ? '请先选择导出文件' : '按 uid 幂等合并入池'}
              onClick={() => importPoolFile && void importBackupFile(importPoolFile, importPoolPwd)}
            >
              {importingBackup ? '导入中…' : '确认导入'}
            </button>
          </>
        }
      >
        <div className="space-y-3">
          <label className="block text-sm">
            <span className="mb-1 block text-xs text-slate-500">选择导出文件（JSON）</span>
            <input
              type="file"
              accept=".json,application/json"
              className="input w-full"
              onChange={(e) => {
                setImportPoolFile(e.target.files?.[0] ?? null);
                e.target.value = '';
              }}
            />
          </label>
          <label className="block text-sm">
            <span className="mb-1 block text-xs text-slate-500">解密密码（可选，仅加密导出文件需要）</span>
            <input
              className="input w-full font-mono"
              value={importPoolPwd}
              onChange={(e) => setImportPoolPwd(e.target.value)}
              type="password"
              placeholder="加密导出时设置的密码；旧明文导出文件无需填写"
              autoComplete="off"
            />
          </label>
          <p className="text-xs text-slate-400">
            按 uid 幂等合并：已有账号仅补全空缺字段，设备指纹仅在本地为空时补入，绝不覆盖。
          </p>
        </div>
      </Modal>

      {/* 分组管理弹窗（对齐 BuddyAccounts：复用 GroupsModal，强加 Buddy 分组体系） */}
      <GroupsModal
        open={groupOpen}
        onClose={() => setGroupOpen(false)}
        groups={qoderGroups}
        onCreate={async (name, color) => {
          try {
            await api.qoder.groups.create(name, color);
          } catch (err) {
            pushToast('error', `新建分组失败：${String(err)}`);
            return;
          }
          reloadGroups();
        }}
        onRename={async (id, name) => {
          try {
            await api.qoder.groups.update(id, { name });
          } catch (err) {
            pushToast('error', `重命名失败：${String(err)}`);
            return;
          }
          reloadGroups();
        }}
        onRecolor={async (id, color) => {
          try {
            await api.qoder.groups.update(id, { color });
          } catch (err) {
            pushToast('error', `修改颜色失败：${String(err)}`);
            return;
          }
          reloadGroups();
        }}
        onDelete={async (id) => {
          try {
            await api.qoder.groups.remove(id);
          } catch (err) {
            pushToast('error', `删除分组失败：${String(err)}`);
            return;
          }
          // 组内账号本地同步回落「未分组」（后端 with_pool_mut 已置空，前端对齐）
          setAccounts((prev) => prev.map((x) => (x.group_id === id ? { ...x, group_id: '' } : x)));
          if (filter === id) setFilter('all');
          reloadGroups();
        }}
      />

      {/* 帮助弹窗（对齐 BuddyAccounts 页头帮助入口） */}
      <QoderHelpModal open={helpOpen} onClose={() => setHelpOpen(false)} />
    </div>
  );
}
