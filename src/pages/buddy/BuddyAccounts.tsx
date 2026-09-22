import { useCallback, useEffect, useRef, useState } from 'react';
import {
  RefreshCw,
  Download,
  Upload,
  ScanSearch,
  Globe,
  ShieldAlert,
  KeyRound,
  Copy,
  Pencil,
  Trash2,
  Loader2,
  Coins,
  Users,
  HelpCircle,
  FolderCog,
} from 'lucide-react';
import { useMemo } from 'react';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState, Modal, Spinner } from '../../components/ui';
import { BuddyHelpModal } from './HelpModal';
import { GroupSelect } from '../accounts/GroupSelect';
import { GroupsModal } from '../accounts/GroupsModal';
import { listen, api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import { copyText } from '../../lib/clipboard';
import type {
  GroupView,
  WbCheckinRecord,
  WorkBuddyAccountView,
  WbCreditPackage,
  WbCreditsResult,
  WbOauthDone,
  WbOauthProgress,
} from '../../types';

/**
 * buddy-accounts 账号管理（§3.7.2，F-54/F-56/F-60/F-50/F-14）：
 * 列表式账号池（对齐 Trae 账号管理）+ 聚合迁移入口 + 积分包明细弹窗。
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
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [helpOpen, setHelpOpen] = useState(false);
  const [credits, setCredits] = useState<Map<string, WbCreditPackage[]>>(new Map());
  // 今日签到状态（user_id → 最新一条记录；success/already=已签，fail=失败，无=未签）
  const [checkinMap, setCheckinMap] = useState<Map<string, WbCheckinRecord>>(new Map());
  const [loading, setLoading] = useState(false);
  const [importing, setImporting] = useState(false);
  const [scanPreview, setScanPreview] = useState<{ nickname: string; uid: string; exists: boolean; already_in_pool?: boolean } | null>(null);
  const [detailFor, setDetailFor] = useState<WorkBuddyAccountView | null>(null);
  const [deleteFor, setDeleteFor] = useState<WorkBuddyAccountView | null>(null);
  const importFileRef = useRef<HTMLInputElement>(null);
  const [editFor, setEditFor] = useState<WorkBuddyAccountView | null>(null);
  const [editName, setEditName] = useState('');
  // OAuth 扫码（F-50）：事件驱动弹框（后端全流程，进度经 wb-oauth-progress / 结果 wb-oauth-done）
  const [oauthOpen, setOauthOpen] = useState(false);
  const [oauthStage, setOauthStage] = useState('');
  const [oauthMessage, setOauthMessage] = useState('');
  const [oauthAuthUrl, setOauthAuthUrl] = useState<string | null>(null);
  // 行内异步操作互斥（凭证续期）：执行期间该行异步按钮禁用 + spinner
  const [rowOp, setRowOp] = useState<{ id: string; kind: 'refresh' } | null>(null);
  // 各弹框确认按钮 pending 态（防连点重复触发）
  const [confirmImportBusy, setConfirmImportBusy] = useState(false);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [editBusy, setEditBusy] = useState(false);
  const [exportBusy, setExportBusy] = useState(false);
  // 导入备份文件 pending（解析 + 入池期间顶部按钮禁用）
  const [importingBackup, setImportingBackup] = useState(false);
  // OAuth 流程 pending（发起 → done 事件/超时；期间顶部按钮禁用，弹框提前关闭仍监听结果）
  const [oauthBusy, setOauthBusy] = useState(false);
  // ---- 账号分组（对齐 Trae 账号管理）----
  const [wbGroups, setWbGroups] = useState<GroupView[]>([]);
  const [groupOpen, setGroupOpen] = useState(false);
  const [filter, setFilter] = useState<string>('all');

  /** 分组列表重取（count/uids 由后端按账号 group_id 实时推导） */
  const reloadGroups = useCallback(() => {
    api.workbuddy.groups
      .list()
      .then(setWbGroups)
      .catch(() => {});
  }, []);

  // 分组过滤（对齐 Trae 账号管理）：all / ungrouped / 指定分组 id
  const filtered = useMemo(() => {
    if (filter === 'all') return accounts;
    if (filter === 'ungrouped') return accounts.filter((a) => !a.group_id);
    return accounts.filter((a) => a.group_id === filter);
  }, [accounts, filter]);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const accs = await api.workbuddy.accountsList();
      setAccounts(accs);
      // 分组列表（失败静默：过滤 chips 缺失不阻断列表）
      reloadGroups();
      // 积分缓存查询（≥5min 缓存，失败不阻断列表展示）
      api.workbuddy
        .creditsFetch()
        .then((r: WbCreditsResult) => {
          const m = new Map<string, WbCreditPackage[]>();
          for (const a of r.accounts) m.set(a.user_id, a.packages ?? []);
          setCredits(m);
          // 套餐回填发生在积分拉取链路（payment-type → edition_type，仅补空）：
          // 静默重取列表刷新会员套餐列（有新回填时才与原列表不同，无副作用）
          if (accs.some((a) => !a.edition_type)) {
            api.workbuddy
              .accountsList()
              .then((fresh) => setAccounts(fresh))
              .catch(() => {});
          }
        })
        .catch(() => pushToast('warn', '积分缓存查询失败，积分包列为空'));
      // 今日签到状态（失败静默，不阻断列表）
      api.workbuddy
        .checkinResults(1)
        .then((recs: WbCheckinRecord[]) => {
          const today = new Date();
          const todayStr = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-${String(today.getDate()).padStart(2, '0')}`;
          const m = new Map<string, WbCheckinRecord>();
          for (const r of recs) {
            if (r.date !== todayStr) continue; // days=1 含昨日，仅保留今天
            if (!m.has(r.user_id)) m.set(r.user_id, r); // 接口返回新→旧序，首见 = 最新一条
          }
          setCheckinMap(m);
        })
        .catch(() => {
          setCheckinMap(new Map());
          pushToast('warn', '今日签到状态查询失败');
        });
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

  // OAuth 事件监听（F-50）：弹框打开或流程 pending 期间订阅；弹框提前关闭仍能收到结果提示
  useEffect(() => {
    if (!oauthOpen && !oauthBusy) return;
    const un1 = listen<WbOauthProgress>('wb-oauth-progress', (e) => {
      setOauthStage(e.payload.stage);
      setOauthMessage(e.payload.message);
      if (e.payload.auth_url) setOauthAuthUrl(e.payload.auth_url);
    });
    const un2 = listen<WbOauthDone>('wb-oauth-done', (e) => {
      setOauthBusy(false);
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
    // 超时兜底：后端流程最长 300s；done 事件异常缺失时解除 pending，避免顶部按钮永久禁用
    const timer = oauthBusy
      ? setTimeout(() => {
          setOauthBusy(false);
          pushToast('warn', 'OAuth 登录超过 5 分钟未收到结果事件，已解除等待；结果请以账号列表为准');
        }, 310_000)
      : undefined;
    return () => {
      void un1.then((f) => f());
      void un2.then((f) => f());
      if (timer) clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [oauthOpen, oauthBusy]);

  // 发起 OAuth 扫码（后端打开浏览器 + 轮询 + 自动入池）
  const startOauth = async () => {
    setOauthBusy(true);
    setOauthStage('init');
    setOauthMessage('正在发起扫码登录…');
    setOauthAuthUrl(null);
    setOauthOpen(true);
    try {
      await api.workbuddy.oauthLogin();
    } catch (err) {
      setOauthStage('error');
      setOauthMessage(String(err));
      setOauthBusy(false);
    }
  };

  // 复制 OAuth 登录链接：浏览器未自动打开 / 提示链接不完整时的手动兜底
  const copyOauthUrl = async () => {
    if (!oauthAuthUrl) return;
    if (await copyText(oauthAuthUrl)) {
      pushToast('success', '登录链接已复制到剪贴板');
    } else {
      pushToast('error', '复制登录链接失败：请手动选中链接复制');
    }
  };

  // 导入本机账号（F-04）：扫描 auth 文件 → 预览确认 → 入池
  const importFromAuth = async () => {
    setImporting(true);
    try {
      const scan = await withMinDelay(api.workbuddy.scanAuthFile(), 800);
      // 扫描成功但零条可导入（未登录 / 客户端退出登录后 auth 文件被清空，uid 为空或后端返回 null）：
      // 明确 warn 反馈并终止，不打开空预览弹框
      if (!scan || !scan.uid) {
        pushToast('warn', '未在本机发现有效登录凭证：请先在 WorkBuddy / CodeBuddy 客户端登录，再点击导入');
        return;
      }
      // 旧后端仅回 exists=true（已在池中）→ 维持拦截；新后端另回 already_in_pool=true 时放行进入预览（保存将更新凭证）
      if (scan.exists && scan.already_in_pool !== true) {
        pushToast('info', `该账号已在池中（${scan.nickname || scan.id}）`);
        return;
      }
      setScanPreview({ nickname: scan.nickname, uid: scan.uid, exists: scan.exists, already_in_pool: scan.already_in_pool });
    } catch (err) {
      pushToast('error', `扫描 auth 文件失败：${String(err)}`);
    } finally {
      setImporting(false);
    }
  };

  const confirmImport = async () => {
    setConfirmImportBusy(true);
    try {
      const name = scanPreview?.nickname || undefined;
      const view = await withMinDelay(api.workbuddy.accountImportAuth(name), 1000);
      pushToast('success', `账号「${view.nickname || view.id}」已入池`);
      setScanPreview(null);
      await refresh();
    } catch (err) {
      pushToast('error', `导入失败：${String(err)}`);
    } finally {
      setConfirmImportBusy(false);
    }
  };

  const handleRefreshToken = async (a: WorkBuddyAccountView) => {
    setRowOp({ id: a.id, kind: 'refresh' });
    try {
      await withMinDelay(api.workbuddy.refreshToken(a.id, true), 1000);
      pushToast('success', `「${a.nickname || a.id}」凭证已续期`);
      await refresh();
    } catch (err) {
      pushToast('error', `续期失败：${String(err)}`);
    } finally {
      setRowOp(null);
    }
  };

  const handleDelete = (a: WorkBuddyAccountView) => {
    setDeleteFor(a);
  };

  const confirmDelete = async () => {
    if (!deleteFor) return;
    setDeleteBusy(true);
    try {
      await withMinDelay(api.workbuddy.accountRemove(deleteFor.id, false), 1000);
      pushToast('info', '账号已删除');
      setDeleteFor(null);
      await refresh();
    } catch (err) {
      pushToast('error', `删除失败：${String(err)}`);
    } finally {
      setDeleteBusy(false);
    }
  };

  const handleEdit = (a: WorkBuddyAccountView) => {
    setEditFor(a);
    setEditName(a.nickname);
  };

  const confirmEdit = async () => {
    if (!editFor) return;
    setEditBusy(true);
    try {
      await withMinDelay(api.workbuddy.accountSave(editFor.id, editName || undefined, undefined), 1000);
      setEditFor(null);
      await refresh();
      pushToast('success', '账号已更新');
    } catch (err) {
      pushToast('error', `更新失败：${String(err)}`);
    } finally {
      setEditBusy(false);
    }
  };

  // 导出（Web 简版一步导出）：固定含凭证副本（迁移必需），文件等同密码
  const exportPool = () => {
    void confirmExport();
  };

  // 导出确认（F-46 扩展）：默认含凭证副本（迁移场景必需）
  const confirmExport = async () => {
    setExportBusy(true);
    try {
      const data = await withMinDelay(api.workbuddy.accountsExport(true), 1000);
      const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `workbuddy_accounts_${new Date().toISOString().slice(0, 10)}.json`;
      a.click();
      URL.revokeObjectURL(url);
      pushToast('success', '账号池已导出（含凭证，文件等同密码请妥善保管）');
    } catch (err) {
      pushToast('error', `导出失败：${String(err)}`);
    } finally {
      setExportBusy(false);
    }
  };

  // 导入备份（F-46 扩展）：选择导出文件 → 解析入池（含凭证回写；已在池中的账号更新凭证）
  const importBackupFile = async (file: File) => {
    setImportingBackup(true);
    try {
      const text = await file.text();
      const payload = JSON.parse(text) as Record<string, unknown>;
      const r = await withMinDelay(api.workbuddy.accountsImport(payload), 1000);
      // 主结果：新增 N 个账号（+ 更新 M 个）；skipped 仅在后端确有跳过时提示（旧后端恒 0 保持兼容）
      const parts = [`新增 ${r.added} 个账号`];
      if (r.updated && r.updated > 0) parts.push(`更新 ${r.updated} 个`);
      if (r.skipped > 0) parts.push(`跳过 ${r.skipped}（已在池中）`);
      pushToast('success', `导入完成：${parts.join('、')}，带凭证 ${r.with_credentials}`);
      // 被拒绝条目（id + 原因）单独 warn 提示，最多列出前 3 条
      if (r.rejected && r.rejected.length > 0) {
        const head = r.rejected
          .slice(0, 3)
          .map((x) => `「${x.id}」${x.reason}`)
          .join('；');
        pushToast('warn', `${r.rejected.length} 条被拒绝导入：${head}${r.rejected.length > 3 ? '…' : ''}`);
      }
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
        title="Buddy · 账号管理"
        desc="多账号入池 · 切换登录 · 凭证续期"
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
              onClick={() => void startOauth()}
              disabled={oauthBusy}
              title={oauthBusy ? '扫码流程进行中…' : undefined}
            >
              <Globe size={15} /> OAuth登录
            </button>
            <button className="btn-outline" onClick={() => void importFromAuth()} disabled={importing}>
              {importing ? <Spinner /> : <ScanSearch size={15} />} 扫描本机账号
            </button>
            <button className="btn-outline" onClick={exportPool} disabled={accounts.length === 0}>
              <Download size={15} /> 导出账号
            </button>
            <button
              className="btn-outline"
              onClick={() => importFileRef.current?.click()}
              disabled={importingBackup}
              title={importingBackup ? '正在导入备份…' : undefined}
            >
              {importingBackup ? <Spinner /> : <Upload size={15} />} 导入账号
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
            <button onClick={() => setGroupOpen(true)} className="btn-outline" title="管理账号分组">
              <FolderCog size={15} /> 分组管理
            </button>
          </>
        }
      />

      {/* 分组过滤 chips（对齐 Trae 账号管理） */}
      {accounts.length > 0 && (
        <div className="mb-3 mt-5 flex flex-wrap items-center gap-2 text-sm">
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
          {wbGroups.map((g) => (
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
      )}

      {/* 账号列表（对齐 Trae 账号管理表格） */}
      {accounts.length === 0 ? (
        <div className="mt-5">
          <EmptyState
            icon={<Users size={26} />}
            title="暂无 WorkBuddy 账号"
            hint="点击上方「OAuth登录」或「扫描本机账号」将账号入池。注意：客户端退出登录后登录凭证已被清空，将无法导入，需先重新登录客户端。"
          />
        </div>
      ) : (
        <div className="mt-5 card overflow-x-auto">
          <table className="w-full min-w-[980px] text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-left">分组</th>
                <th className="px-4 py-2 text-left">会员套餐</th>
                <th className="px-4 py-2 text-left">积分包</th>
                <th className="px-4 py-2 text-right">可用积分</th>
                <th className="px-4 py-2 text-left">token 到期</th>
                <th className="px-4 py-2 text-left">签到状态</th>
                <th className="px-4 py-2 text-left">状态</th>
                <th className="px-4 py-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              {filtered.map((a) => {
                const pkgs = credits.get(a.id) ?? [];
                // 7 日内将过期（仍有剩余）的积分包 → 列表标记提示
                const nowSec = Math.floor(Date.now() / 1000);
                const soonestExpire = pkgs.reduce<number | null>((min, p) => {
                  if (p.expire_ts == null || p.remaining <= 0 || p.expire_ts <= nowSec) return min;
                  if (p.expire_ts > nowSec + 7 * 86400) return min;
                  return min == null ? p.expire_ts : Math.min(min, p.expire_ts);
                }, null);
                // 本行有异步操作执行中时，该行其余异步小按钮一并禁用（防连点/并发）
                const rowOpKind = rowOp && rowOp.id === a.id ? rowOp.kind : null;
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
                      <GroupSelect
                        value={a.group_id || null}
                        groups={wbGroups}
                        onChange={(gid) => {
                          void api.workbuddy.accountMove(a.id, gid).then(() => {
                            // 本地同步账号分组 + 重取分组计数（不整表刷新）
                            setAccounts((prev) =>
                              prev.map((x) => (x.id === a.id ? { ...x, group_id: gid ?? '' } : x)),
                            );
                            reloadGroups();
                          }).catch((err) => pushToast('error', `分组调整失败：${String(err)}`));
                        }}
                      />
                    </td>
                    <td className="px-4 py-3">
                      {a.edition_type ? (
                        <Badge
                          tone={a.edition_type.toLowerCase() === 'pro' ? 'blue' : a.is_current ? 'violet' : 'slate'}
                          title={a.is_current ? `当前套餐：${a.edition_type}` : a.edition_type}
                        >
                          {a.edition_type}
                        </Badge>
                      ) : (
                        <span className="text-xs text-slate-300 dark:text-zinc-600">—</span>
                      )}
                    </td>
                    <td className="px-4 py-3">
                      {pkgs.length > 0 ? (
                        <div className="flex flex-col items-start gap-1">
                          <button
                            className="flex items-center gap-1 text-xs text-brand-600 hover:underline dark:text-brand-400"
                            onClick={() => setDetailFor(a)}
                          >
                            <Coins size={13} /> {pkgs.length} 个包
                          </button>
                          {soonestExpire != null && (
                            <Badge
                              tone="amber"
                              title={`有积分包将于 ${new Date(soonestExpire * 1000).toLocaleDateString('zh-CN')} 过期，明细见积分包弹窗`}
                            >
                              7 日内到期
                            </Badge>
                          )}
                        </div>
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
                      {(() => {
                        const rec = checkinMap.get(a.id);
                        if (!rec) return <Badge tone="slate">未签</Badge>;
                        if (rec.status === 'fail') return <Badge tone="red">签到失败</Badge>;
                        if (rec.status === 'already') return <Badge tone="blue">已签（本轮已签）</Badge>;
                        return <Badge tone="green">已签{rec.reward != null ? ` +${rec.reward}` : ''}</Badge>;
                      })()}
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex flex-col gap-1">
                        <div className="flex items-center gap-1">
                          {a.is_current ? (
                            <Badge tone="green">在线</Badge>
                          ) : a.needs_relogin ? (
                            <Badge tone="red">需重登</Badge>
                          ) : (
                            <Badge tone="slate">备用</Badge>
                          )}
                          {a.is_current_workbuddy && (
                            <Badge tone="green" title="WorkBuddy 端当前账号（切换桥标记，与 CodeBuddy 端相互独立）">
                              WB当前
                            </Badge>
                          )}
                          {a.is_current_codebuddy && (
                            <Badge tone="violet" title="CodeBuddy 端当前登录账号（客户端 genie.userId 实测，与 WorkBuddy 端相互独立）">
                              CB当前
                            </Badge>
                          )}
                        </div>
                      </div>
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex justify-end gap-1">
                        <button
                          title={rowOpKind === 'refresh' ? '凭证续期中…' : '凭证续期（refreshToken 换新 accessToken）'}
                          onClick={() => void handleRefreshToken(a)}
                          disabled={rowOpKind != null || !a.has_credential}
                          className={`btn-ghost !p-2 text-amber-500 hover:bg-amber-50 dark:hover:bg-amber-500/10 ${rowOpKind != null ? 'opacity-40 cursor-not-allowed' : ''}`}
                        >
                          {rowOpKind === 'refresh' ? <Loader2 size={14} className="animate-spin" /> : <KeyRound size={14} />}
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
        onClose={() => {
          if (!confirmImportBusy) setScanPreview(null);
        }}
        title="扫描本机账号"
        footer={
          <>
            <button className="btn-outline" onClick={() => setScanPreview(null)} disabled={confirmImportBusy}>取消</button>
            <button className="btn-primary" onClick={() => void confirmImport()} disabled={confirmImportBusy}>
              {confirmImportBusy ? <Spinner /> : null} 确认入池
            </button>
          </>
        }
      >
        <div className="space-y-1 text-sm">
          <div className="flex items-center gap-2">
            <span>检测到本机登录账号，确认导入账号池？</span>
            {scanPreview?.already_in_pool === true && (
              <Badge tone="amber">已在池中（保存将更新凭证）</Badge>
            )}
          </div>
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
        onClose={() => {
          if (!deleteBusy) setDeleteFor(null);
        }}
        title="删除账号"
        footer={
          <>
            <button className="btn-outline" onClick={() => setDeleteFor(null)} disabled={deleteBusy}>取消</button>
            <button
              className="btn-primary !bg-rose-600 hover:!bg-rose-500"
              onClick={() => void confirmDelete()}
              disabled={deleteBusy}
            >
              {deleteBusy ? <Spinner /> : null} 确认删除
            </button>
          </>
        }
      >
        <div className="text-sm">
          确定删除账号「{deleteFor?.nickname || deleteFor?.id}」？
          <div className="mt-1 text-xs text-slate-400">仅移出账号池，不影响凭证文件。</div>
        </div>
      </Modal>

      {/* 编辑弹框（禁 window.prompt，红线） */}
      <Modal
        open={editFor != null}
        onClose={() => {
          if (!editBusy) setEditFor(null);
        }}
        title="编辑账号"
        footer={
          <>
            <button className="btn-outline" onClick={() => setEditFor(null)} disabled={editBusy}>取消</button>
            <button className="btn-primary" onClick={() => void confirmEdit()} disabled={editBusy}>
              {editBusy ? <Spinner /> : null} 保存
            </button>
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

      {/* OAuth 扫码弹框（F-50：后端全流程，事件驱动进度展示） */}
      <Modal
        open={oauthOpen}
        onClose={() => setOauthOpen(false)}
        title="OAuth 登录"
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
          {oauthStage !== 'success' &&
            (oauthAuthUrl ? (
              <div className="space-y-1.5 text-xs text-slate-400">
                <div>点击链接在新标签页完成登录；也可复制链接到其他设备浏览器打开：</div>
                <div className="flex items-start gap-2">
                  <a
                    href={oauthAuthUrl}
                    target="_blank"
                    rel="noreferrer"
                    className="min-w-0 flex-1 font-mono break-all text-indigo-500 underline dark:text-indigo-400"
                    title="打开登录页"
                  >
                    {oauthAuthUrl}
                  </a>
                  <button
                    className="btn-outline shrink-0 !px-2 !py-1 !text-xs"
                    onClick={() => void copyOauthUrl()}
                    title="复制完整登录链接"
                  >
                    <Copy size={12} /> 复制完整链接
                  </button>
                </div>
                <div>若页面提示登录链接不完整，请复制完整链接到新标签页打开。</div>
              </div>
            ) : (
              <div className="text-xs text-slate-400">未获取到登录链接，请查看应用日志</div>
            ))}
          <div className="text-xs text-slate-400">
            登录成功后账号自动入池，凭证仅写入本地 token store（全程掩码，不上传）。
          </div>
        </div>
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

      {/* 分组管理弹框（复用 Trae GroupsModal，WB 分组走 workbuddy_groups_* 命令） */}
      <GroupsModal
        open={groupOpen}
        onClose={() => setGroupOpen(false)}
        groups={wbGroups}
        onCreate={async (name, color) => {
          await api.workbuddy.groups.create(name, color);
          reloadGroups();
        }}
        onRename={async (id, name) => {
          await api.workbuddy.groups.update(id, { name });
          reloadGroups();
        }}
        onRecolor={async (id, color) => {
          await api.workbuddy.groups.update(id, { color });
          reloadGroups();
        }}
        onDelete={async (id) => {
          await api.workbuddy.groups.remove(id);
          reloadGroups();
          await refresh(); // 组内账号回落「未分组」，列表同步
        }}
      />

      {/* 使用帮助弹框（与 Trae 账号管理同款入口样式，内容为 Buddy 特性口径） */}
      <BuddyHelpModal open={helpOpen} onClose={() => setHelpOpen(false)} />
    </div>
  );
}