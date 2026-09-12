import { useCallback, useEffect, useRef, useState, type MouseEvent } from 'react';
import {
  RefreshCw,
  Download,
  Upload,
  ScanSearch,
  Globe,
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
  AppWindow,
  MonitorCheck,
  History,
} from 'lucide-react';
import { createPortal } from 'react-dom';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState, Modal, Spinner } from '../../components/ui';
import { listen } from '@tauri-apps/api/event';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type {
  ProfileInfo,
  WbCheckinRecord,
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

/** 快照大小本地格式化（避免逐行 invoke profile_format_size） */
function fmtSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(i === 0 || v >= 10 ? 0 : 1)} ${units[i]}`;
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

/** chatdata_info 状态条目（id → meta / null = 查询失败降级） */
type ChatMetaEntry = [string, { backed: boolean; files?: number; backed_at?: string } | null];

export default function BuddyAccounts() {
  const pushToast = useAppStore((s) => s.pushToast);
  const switchTo = useAppStore((s) => s.switchTo);
  const switchingTo = useAppStore((s) => s.switchingTo);
  const saveCurrentLogin = useAppStore((s) => s.saveCurrentLogin);
  const savingLogin = useAppStore((s) => s.savingLogin);
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [credits, setCredits] = useState<Map<string, WbCreditPackage[]>>(new Map());
  // 今日签到状态（user_id → 最新一条记录；success/already=已签，fail=失败，无=未签）
  const [checkinMap, setCheckinMap] = useState<Map<string, WbCheckinRecord>>(new Map());
  const [loading, setLoading] = useState(false);
  const [importing, setImporting] = useState(false);
  const [scanPreview, setScanPreview] = useState<{ nickname: string; uid: string; exists: boolean; already_in_pool?: boolean } | null>(null);
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
  // CodeBuddy 桌面端当前登录 uid（env check 失败静默，仅影响「CodeBuddy在线」徽标展示）
  const [cbUid, setCbUid] = useState<string | null>(null);
  // 双应用切换/保存菜单（复刻 Trae Accounts appMenu）：{ id, kind } —— kind='switch' 切换登录态 / 'save' 保存登录态
  const [appMenu, setAppMenu] = useState<{ id: string; kind: 'switch' | 'save'; x: number; y: number } | null>(null);
  // 行内异步操作互斥（凭证续期 / CLI 桥接 / 会话备份）：执行期间该行异步按钮禁用 + spinner
  const [rowOp, setRowOp] = useState<{ id: string; kind: 'refresh' | 'cli' | 'backup' } | null>(null);
  // 各弹框确认按钮 pending 态（防连点重复触发）
  const [confirmImportBusy, setConfirmImportBusy] = useState(false);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [editBusy, setEditBusy] = useState(false);
  const [exportBusy, setExportBusy] = useState(false);
  const [restoreBusy, setRestoreBusy] = useState(false);
  const [copyBusy, setCopyBusy] = useState(false);
  // 导入备份文件 pending（解析 + 入池期间顶部按钮禁用）
  const [importingBackup, setImportingBackup] = useState(false);
  // 环境重置清单读取 pending
  const [resetLoading, setResetLoading] = useState(false);
  // OAuth 流程 pending（发起 → done 事件/超时；期间顶部按钮禁用，弹框提前关闭仍监听结果）
  const [oauthBusy, setOauthBusy] = useState(false);
  // 切换/保存登录态 90s 看门狗：done 事件异常缺失时本地解除按钮互斥（store 只读，页面级兜底）
  const [lockTimedOut, setLockTimedOut] = useState(false);
  // 各账号会话备份状态（id → chatdata_info meta，null = 查询失败降级）：行内「已备份」徽标数据源
  const [chatMeta, setChatMeta] = useState<Map<string, { backed: boolean; files?: number; backed_at?: string } | null>>(new Map());
  // ---- 登录态快照管理（WorkBuddy / CodeBuddy）----
  const [snapOpen, setSnapOpen] = useState(false);
  const [snapTarget, setSnapTarget] = useState<'WorkBuddy' | 'CodeBuddy'>('WorkBuddy');
  const [snapList, setSnapList] = useState<ProfileInfo[]>([]);
  const [snapLoading, setSnapLoading] = useState(false);
  // 行内操作执行中（互斥所有快照操作按钮）：{ slot, kind }
  const [snapOp, setSnapOp] = useState<{ slot: string; kind: 'restore' | 'delete' } | null>(null);
  // 二次确认弹框内容（恢复并启动 / 删除）
  const [snapConfirm, setSnapConfirm] = useState<{ slot: string; kind: 'restore' | 'delete' } | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const accs = await api.workbuddy.accountsList();
      setAccounts(accs);
      // 会话备份状态徽标：逐账号轻量查询 chatdata_info；任一失败仅该账号不显示徽标（纯状态降级，不阻断列表）
      void Promise.all(
        accs.map(async (a): Promise<ChatMetaEntry> => {
          try {
            const m = await api.workbuddy.chatdataInfo(a.id);
            return [a.id, { backed: m.backed, files: m.files, backed_at: m.backed_at }];
          } catch {
            return [a.id, null];
          }
        }),
      ).then((entries) => setChatMeta(new Map(entries)));
      // CLI 当前号查询（失败仅置空徽标：纯状态复位，不阻断列表展示；切号/删号后 refresh 会自动重取）
      api.workbuddy
        .cliStatus()
        .then((s) => setCliActiveId(s.active_account_id))
        .catch(() => setCliActiveId(null));
      // CodeBuddy 桌面端当前登录 uid（失败静默为纯状态复位置空：仅影响「CodeBuddy在线」徽标展示，不阻断列表）
      api.env
        .codebuddyEnvCheck()
        .then((c) => setCbUid(c.uid))
        .catch(() => setCbUid(null));
      // 积分缓存查询（≥5min 缓存，失败不阻断列表展示）
      api.workbuddy
        .creditsFetch()
        .then((r: WbCreditsResult) => {
          const m = new Map<string, WbCreditPackage[]>();
          for (const a of r.accounts) m.set(a.user_id, a.packages ?? []);
          setCredits(m);
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
          pushToast('warn', 'OAuth 扫码超过 5 分钟未收到结果事件，已解除等待；结果请以账号列表为准');
        }, 310_000)
      : undefined;
    return () => {
      void un1.then((f) => f());
      void un2.then((f) => f());
      if (timer) clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [oauthOpen, oauthBusy]);

  // 切换/保存登录态看门狗：switch-done / save-login-done 事件异常缺失时，90s 后本地解除
  // 行内按钮互斥（store 状态只读，此处仅页面级兜底；事件迟到仍会正常提示结果）
  useEffect(() => {
    if (!switchingTo && !savingLogin) {
      setLockTimedOut(false);
      return;
    }
    setLockTimedOut(false);
    const timer = setTimeout(() => {
      setLockTimedOut(true);
      pushToast('warn', '切换/保存超过 90 秒未收到完成事件，已解除按钮锁定；结果请以日志与列表状态为准');
    }, 90_000);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [switchingTo, savingLogin]);

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
    try {
      await navigator.clipboard.writeText(oauthAuthUrl);
      pushToast('success', '登录链接已复制到剪贴板');
    } catch {
      pushToast('error', '复制登录链接失败：请手动选中链接复制');
    }
  };

  // 打开环境重置弹框：拉取 16 项清单（默认勾选所有存在项）
  const openEnvReset = async () => {
    setResetLoading(true);
    try {
      const items = await api.workbuddy.envResetItems();
      setResetItems(items);
      setResetChecked(new Set(items.filter((x) => x.exists).map((x) => x.id)));
      setResetResults(null);
      setResetConfirming(false);
      setResetOpen(true);
    } catch (err) {
      pushToast('error', `读取清理清单失败：${String(err)}`);
    } finally {
      setResetLoading(false);
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

  // 打开目标应用选择菜单（复刻 Trae Accounts 的 appMenu 定位/尺寸/关闭逻辑）；
  // 选中 WorkBuddy / CodeBuddy 后才真正发起 switchTo / saveCurrentLogin；切换保留需重登拦截
  const openAppMenu = (a: WorkBuddyAccountView, kind: 'switch' | 'save', e: MouseEvent<HTMLButtonElement>) => {
    if (kind === 'switch' && a.needs_relogin) {
      pushToast('warn', '该账号标记需重新登录，请先重新登录客户端并保存登录态');
      return;
    }
    const r = e.currentTarget.getBoundingClientRect();
    setAppMenu(appMenu?.id === a.id && appMenu.kind === kind ? null : { id: a.id, kind, x: r.right, y: r.bottom });
  };

  const handleRefreshToken = async (a: WorkBuddyAccountView) => {
    setRowOp({ id: a.id, kind: 'refresh' });
    try {
      await withMinDelay(api.workbuddy.refreshToken(a.id), 1000);
      pushToast('success', `「${a.nickname || a.id}」凭证已续期`);
      await refresh();
    } catch (err) {
      pushToast('error', `续期失败：${String(err)}`);
    } finally {
      setRowOp(null);
    }
  };

  // 设为 CLI 账号（F-06）：token store 凭证 → ~/.codebuddy/settings.json env
  const handleSetCli = async (a: WorkBuddyAccountView) => {
    setRowOp({ id: a.id, kind: 'cli' });
    try {
      await withMinDelay(api.workbuddy.cliBridgeSet(a.id), 800);
      pushToast('success', `「${a.nickname || a.id}」已设为 CodeBuddy CLI 账号（重启 CLI 后生效）`);
      void refresh(); // 更新列表内「CodeBuddy 当前号」徽标
    } catch (err) {
      pushToast('error', `CLI 桥接失败：${String(err)}`);
    } finally {
      setRowOp(null);
    }
  };

  // 会话三件套备份（F-44）：projects + 双 db 快照；执行前自动关闭 WorkBuddy
  const handleBackupChats = async (a: WorkBuddyAccountView) => {
    setRowOp({ id: a.id, kind: 'backup' });
    try {
      const r = await withMinDelay(api.workbuddy.chatdataBackup(a.id), 1200);
      pushToast('success', `「${a.nickname || a.id}」会话已备份（${r.files} 个文件）`);
    } catch (err) {
      pushToast('error', `会话备份失败：${String(err)}`);
    } finally {
      setRowOp(null);
    }
  };

  // 恢复会话（二次确认后执行；覆盖现有 ~/.workbuddy 会话数据）
  const confirmRestoreChats = async () => {
    if (!restoreFor) return;
    setRestoreBusy(true);
    try {
      const r = await withMinDelay(api.workbuddy.chatdataRestore(restoreFor.id), 1200);
      pushToast('success', `会话已恢复（${r.files} 个文件）；原有数据保留于 ~/.workbuddy/*.bak`);
    } catch (err) {
      pushToast('error', `会话恢复失败：${String(err)}`);
    } finally {
      setRestoreBusy(false);
      setRestoreFor(null);
    }
  };

  // 复制会话到目标账号（F-45）：新 id 复制 + 云端映射注册
  const confirmCopyChats = async () => {
    if (!copyFor || !copyTarget) return;
    setCopyBusy(true);
    try {
      const r = await withMinDelay(api.workbuddy.chatdataCopy(copyFor.id, copyTarget), 1500);
      pushToast(
        'success',
        `已复制 ${r.copied} 个会话（sessions 克隆 ${r.sessions_cloned}，云端映射 ${r.mappings_registered}）到目标账号`,
      );
      setCopyFor(null);
    } catch (err) {
      pushToast('error', `会话复制失败：${String(err)}`);
    } finally {
      setCopyBusy(false);
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

  const exportPool = () => {
    setExportOpen(true);
  };

  // 导出确认（F-46 扩展）：可选是否附带凭证副本（迁移场景用）
  const confirmExport = async () => {
    setExportBusy(true);
    try {
      const data = await withMinDelay(api.workbuddy.accountsExport(exportWithCreds), 1000);
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

  // ---- 登录态快照管理（WorkBuddy / CodeBuddy 双应用 profile 槽）----
  // 读取指定应用的快照列表（profile_list 按 targetApp 映射 profiles_workbuddy / profiles_codebuddy）
  const loadSnapshots = async (target: 'WorkBuddy' | 'CodeBuddy') => {
    setSnapLoading(true);
    try {
      setSnapList(await api.profiles.list(target));
    } catch (err) {
      pushToast('error', `读取快照列表失败：${String(err)}`);
      setSnapList([]);
    } finally {
      setSnapLoading(false);
    }
  };

  const openSnapshots = () => {
    setSnapConfirm(null);
    setSnapOpen(true);
    void loadSnapshots(snapTarget);
  };

  const switchSnapTarget = (t: 'WorkBuddy' | 'CodeBuddy') => {
    setSnapTarget(t);
    void loadSnapshots(t);
  };

  // 确认执行快照操作（恢复并启动 / 删除）；执行期间互斥全部快照操作按钮
  const confirmSnapOp = async () => {
    if (!snapConfirm) return;
    const { slot, kind } = snapConfirm;
    setSnapOp({ slot, kind });
    try {
      if (kind === 'restore') {
        await withMinDelay(api.profiles.restore(slot, snapTarget), 1000);
        pushToast('success', `快照「${slot}」已恢复，正在启动 ${snapTarget}…`);
        setSnapConfirm(null);
        await Promise.all([loadSnapshots(snapTarget), refresh()]);
      } else {
        await withMinDelay(api.profiles.delete(slot, snapTarget), 1000);
        pushToast('success', `快照「${slot}」已删除`);
        setSnapConfirm(null);
        await loadSnapshots(snapTarget);
      }
    } catch (err) {
      pushToast('error', `${kind === 'restore' ? '恢复快照' : '删除快照'}失败：${String(err)}`);
    } finally {
      setSnapOp(null);
    }
  };

  const busy = (!!switchingTo || !!savingLogin) && !lockTimedOut;

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
            <button className="btn-outline" onClick={openSnapshots}>
              <History size={15} /> 快照管理
            </button>
            <button className="btn-outline" onClick={() => void importFromAuth()} disabled={importing}>
              {importing ? <Spinner /> : <ScanSearch size={15} />} 导入本机账号
            </button>
            <button
              className="btn-outline"
              onClick={() => void startOauth()}
              disabled={oauthBusy}
              title={oauthBusy ? '扫码流程进行中…' : undefined}
            >
              <Globe size={15} /> OAuth 扫码
            </button>
            <button className="btn-outline" onClick={exportPool} disabled={accounts.length === 0}>
              <Download size={15} /> 导出
            </button>
            <button
              className="btn-outline"
              onClick={() => importFileRef.current?.click()}
              disabled={importingBackup}
              title={importingBackup ? '正在导入备份…' : undefined}
            >
              {importingBackup ? <Spinner /> : <Upload size={15} />} 导入备份
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
            <button
              className="btn-outline !text-rose-600 hover:!border-rose-300"
              onClick={() => void openEnvReset()}
              disabled={resetLoading}
              title={resetLoading ? '正在读取清理清单…' : undefined}
            >
              {resetLoading ? <Spinner /> : <ShieldAlert size={15} />} 环境重置
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
            hint="先在 WorkBuddy 客户端登录，然后点击上方「导入本机账号」自动扫描入池；切换/保存登录态会在账号管理中生成快照。注意：客户端退出登录后登录凭证已被清空，将无法导入，需先重新登录客户端。"
          />
        </div>
      ) : (
        <div className="mt-5 card overflow-x-auto">
          <table className="w-full min-w-[860px] text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-left">会员等级</th>
                <th className="px-4 py-2 text-right">可用积分</th>
                <th className="px-4 py-2 text-left">token 到期</th>
                <th className="px-4 py-2 text-left">积分包</th>
                <th className="px-4 py-2 text-left">签到状态</th>
                <th className="px-4 py-2 text-left">状态</th>
                <th className="px-4 py-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              {accounts.map((a) => {
                const pkgs = credits.get(a.id) ?? [];
                const switching = switchingTo === a.id || savingLogin === a.id;
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
                      {a.edition_type ? (
                        <Badge tone={a.edition_type.toLowerCase() === 'pro' ? 'blue' : 'slate'}>{a.edition_type}</Badge>
                      ) : (
                        <span className="text-xs text-slate-300 dark:text-zinc-600">—</span>
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
                      {(() => {
                        const rec = checkinMap.get(a.id);
                        if (!rec) return <Badge tone="slate">未签</Badge>;
                        if (rec.status === 'fail') return <Badge tone="red">签到失败</Badge>;
                        if (rec.status === 'already') return <Badge tone="blue">已签（本轮已签）</Badge>;
                        return <Badge tone="green">已签{rec.reward != null ? ` +${rec.reward}` : ''}</Badge>;
                      })()}
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
                        {cbUid != null && cbUid === a.uid && (
                          <Badge tone="violet" title="CodeBuddy 桌面端当前登录此账号（与 WorkBuddy 在线相互独立）">
                            <MonitorCheck size={12} /> CodeBuddy在线
                          </Badge>
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
                          title={switching ? '操作执行中…' : a.needs_relogin ? '需重新登录后才能切换' : '设为当前（选择目标应用）'}
                          onClick={(e) => openAppMenu(a, 'switch', e)}
                          disabled={busy || a.needs_relogin}
                          className={`btn-ghost !p-2 ${switching ? 'text-amber-500' : 'text-emerald-600 dark:text-emerald-400'} ${(busy || a.needs_relogin) && !switching ? 'opacity-40 cursor-not-allowed' : ''}`}
                        >
                          {switching ? <Loader2 size={14} className="animate-spin" /> : <LogIn size={14} />}
                        </button>
                        <button
                          title={savingLogin === a.id ? '正在保存登录态…' : '保存当前登录态（选择目标应用）'}
                          onClick={(e) => openAppMenu(a, 'save', e)}
                          disabled={busy}
                          className={`btn-ghost !p-2 ${savingLogin === a.id ? 'text-amber-500' : ''} ${busy && savingLogin !== a.id ? 'opacity-40 cursor-not-allowed' : ''}`}
                        >
                          {savingLogin === a.id ? <Loader2 size={14} className="animate-spin" /> : <Save size={14} />}
                        </button>
                        <button
                          title={rowOpKind === 'refresh' ? '凭证续期中…' : '凭证续期（refreshToken 换新 accessToken）'}
                          onClick={() => void handleRefreshToken(a)}
                          disabled={rowOpKind != null || !a.has_credential}
                          className={`btn-ghost !p-2 text-amber-500 hover:bg-amber-50 dark:hover:bg-amber-500/10 ${rowOpKind != null ? 'opacity-40 cursor-not-allowed' : ''}`}
                        >
                          {rowOpKind === 'refresh' ? <Loader2 size={14} className="animate-spin" /> : <KeyRound size={14} />}
                        </button>
                        <button
                          title={rowOpKind === 'cli' ? 'CLI 桥接中…' : a.has_credential ? '设为 CodeBuddy CLI 账号（写入 ~/.codebuddy/settings.json）' : '需先导入凭证副本'}
                          onClick={() => void handleSetCli(a)}
                          disabled={rowOpKind != null || !a.has_credential}
                          className={`btn-ghost !p-2 text-sky-500 hover:bg-sky-50 dark:hover:bg-sky-500/10 ${rowOpKind != null ? 'opacity-40 cursor-not-allowed' : ''}`}
                        >
                          {rowOpKind === 'cli' ? <Loader2 size={14} className="animate-spin" /> : <TerminalSquare size={14} />}
                        </button>
                        {(() => {
                          // 已备份徽标（chatdata_info 元数据；查询失败该账号无徽标，纯状态降级）
                          const meta = chatMeta.get(a.id);
                          if (!meta?.backed) return null;
                          const extra = [meta.backed_at, meta.files != null ? `${meta.files} 个文件` : '']
                            .filter(Boolean)
                            .join(' · ');
                          return <Badge tone="green" title={extra ? `已备份：${extra}` : '会话已备份'}>已备份</Badge>;
                        })()}
                        <button
                          title={rowOpKind === 'backup' ? '会话备份中…' : '备份会话（projects + 双 db 快照）'}
                          onClick={() => void handleBackupChats(a)}
                          disabled={rowOpKind != null}
                          className={`btn-ghost !p-2 ${rowOpKind != null ? 'opacity-40 cursor-not-allowed' : ''}`}
                        >
                          {rowOpKind === 'backup' ? <Loader2 size={14} className="animate-spin" /> : <DatabaseBackup size={14} />}
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

      {/* 应用选择菜单（复刻 Trae Accounts appMenu）：Portal + fixed 定位，避免被表格容器 overflow
          裁剪或被后续行遮盖；左右两块大按钮 hover amber 高亮；双应用目标 WorkBuddy / CodeBuddy。
          切换/保存执行期间隐藏菜单，防止重复发起 */}
      {appMenu &&
        !busy &&
        createPortal(
          <div
            className="fixed z-50 w-[420px] overflow-hidden rounded-lg border border-slate-200 bg-white text-slate-700 shadow-lg dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
            style={{
              left: Math.max(8, appMenu.x - 420),
              top: (() => {
                const MENU_H = 150;
                const below = appMenu.y + 4 + MENU_H;
                return below > window.innerHeight ? appMenu.y - MENU_H - 8 : appMenu.y + 4;
              })(),
            }}
            onMouseLeave={() => setAppMenu(null)}
          >
            <div className="px-4 pb-1 pt-3 text-[11px] font-semibold text-slate-400 dark:text-zinc-500">
              {appMenu.kind === 'switch' ? '切换此账号到…' : '保存当前登录态到…'}
            </div>
            <div className="grid grid-cols-2 gap-2 p-3 pt-1.5">
              {([
                { app: 'WorkBuddy', label: 'WorkBuddy', desc: 'WorkBuddy 桌面客户端', Icon: AppWindow },
                { app: 'CodeBuddy', label: 'CodeBuddy', desc: 'CodeBuddy 桌面客户端', Icon: TerminalSquare },
              ] as const).map((opt) => (
                <button
                  key={opt.app}
                  onClick={() => {
                    const { id, kind } = appMenu;
                    setAppMenu(null);
                    if (kind === 'switch') {
                      void switchTo(id, opt.app);
                    } else {
                      void saveCurrentLogin(id, opt.app);
                    }
                  }}
                  className="group flex flex-col items-start gap-1 rounded-lg border border-slate-200 bg-white px-4 py-3 text-left transition hover:border-amber-400 hover:bg-amber-50 hover:shadow-sm dark:border-zinc-700 dark:bg-zinc-800 dark:hover:border-amber-500 dark:hover:bg-amber-500/10"
                >
                  <span className="flex items-center gap-2 text-sm font-semibold">
                    <opt.Icon size={16} className="text-slate-500 transition group-hover:text-amber-500 dark:text-zinc-400" />
                    {opt.label}
                  </span>
                  <span className="text-[11px] text-slate-400 dark:text-zinc-500">{opt.desc}</span>
                </button>
              ))}
            </div>
          </div>,
          document.body,
        )}

      {/* 导入预览确认弹框 */}
      <Modal
        open={scanPreview != null}
        onClose={() => {
          if (!confirmImportBusy) setScanPreview(null);
        }}
        title="导入本机账号"
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
          <div className="mt-1 text-xs text-slate-400">仅移出账号池，不删除登录态快照与凭证文件。</div>
        </div>
      </Modal>

      {/* 会话恢复确认弹框（F-44，覆盖性操作二次确认） */}
      <Modal
        open={restoreFor != null}
        onClose={() => {
          if (!restoreBusy) setRestoreFor(null);
        }}
        title="恢复会话数据"
        footer={
          <>
            <button className="btn-outline" onClick={() => setRestoreFor(null)} disabled={restoreBusy}>取消</button>
            <button
              className="btn-primary !bg-amber-600 hover:!bg-amber-500"
              onClick={() => void confirmRestoreChats()}
              disabled={restoreBusy}
            >
              {restoreBusy ? <Spinner /> : null} 确认恢复
            </button>
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
        onClose={() => {
          if (!copyBusy) setCopyFor(null);
        }}
        title={`复制会话 · ${copyFor?.nickname || copyFor?.id || ''}`}
        footer={
          <>
            <button className="btn-outline" onClick={() => setCopyFor(null)} disabled={copyBusy}>取消</button>
            <button className="btn-primary" onClick={() => void confirmCopyChats()} disabled={copyBusy || !copyTarget}>
              {copyBusy ? <Spinner /> : null} 开始复制
            </button>
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

      {/* 导出选项弹框（F-46 扩展：凭证是否随导出） */}
      <Modal
        open={exportOpen}
        onClose={() => {
          if (!exportBusy) setExportOpen(false);
        }}
        title="导出账号池"
        footer={
          <>
            <button className="btn-outline" onClick={() => setExportOpen(false)} disabled={exportBusy}>取消</button>
            <button className="btn-primary" onClick={() => void confirmExport()} disabled={exportBusy}>
              {exportBusy ? <Spinner /> : null} 确认导出
            </button>
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
          {oauthStage !== 'success' &&
            (oauthAuthUrl ? (
              <div className="space-y-1.5 text-xs text-slate-400">
                <div>未自动打开浏览器？可复制完整链接手动打开：</div>
                <div className="flex items-start gap-2">
                  <span className="min-w-0 flex-1 font-mono break-all">{oauthAuthUrl}</span>
                  <button
                    className="btn-outline shrink-0 !px-2 !py-1 !text-xs"
                    onClick={() => void copyOauthUrl()}
                    title="复制完整登录链接"
                  >
                    <Copy size={12} /> 复制完整链接
                  </button>
                </div>
                <div>若浏览器仍提示登录链接不完整，请复制完整链接手动打开。</div>
              </div>
            ) : (
              <div className="text-xs text-slate-400">未获取到登录链接，请查看应用日志</div>
            ))}
          <div className="text-xs text-slate-400">
            登录成功后账号自动入池，凭证仅写入本地 token store（全程掩码，不上传）。
          </div>
        </div>
      </Modal>

      {/* 环境重置弹框（F-14：16 项勾选预览 + 二次确认 + Keycloak 注销开关） */}
      <Modal
        open={resetOpen}
        onClose={() => {
          if (!resetBusy) setResetOpen(false);
        }}
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

      {/* 登录态快照管理弹窗（WorkBuddy / CodeBuddy 双应用）：槽位列表 + 恢复并启动 / 删除（均二次确认） */}
      <Modal
        open={snapOpen}
        onClose={() => {
          if (!snapOp && !snapConfirm) setSnapOpen(false);
        }}
        size="lg"
        bodyClass="max-h-[70vh] overflow-y-auto"
        title="登录态快照管理"
        footer={
          <button
            className="btn-outline"
            onClick={() => void loadSnapshots(snapTarget)}
            disabled={snapLoading || snapOp != null || snapConfirm != null}
          >
            <RefreshCw size={14} className={snapLoading ? 'animate-spin' : ''} /> 刷新列表
          </button>
        }
      >
        <div className="space-y-3 text-sm">
          {/* 目标应用切换（本地 state，默认 WorkBuddy；切换即重新拉取对应 profile 档案） */}
          <div className="flex items-center gap-2">
            <span className="text-xs font-medium text-slate-500">目标应用</span>
            {(['WorkBuddy', 'CodeBuddy'] as const).map((t) => (
              <button
                key={t}
                className={`btn-outline !px-3 !py-1 text-xs ${snapTarget === t ? '!border-brand-500 !text-brand-600' : ''}`}
                disabled={snapLoading || snapOp != null || snapConfirm != null}
                onClick={() => switchSnapTarget(t)}
              >
                {t}
              </button>
            ))}
          </div>
          {snapLoading ? (
            <div className="flex items-center justify-center gap-2 py-8 text-xs text-slate-400">
              <Spinner /> 正在读取快照列表…
            </div>
          ) : snapList.length === 0 ? (
            <div className="py-8 text-center text-xs text-slate-400">暂无快照：点账号行「保存登录态到…」生成</div>
          ) : (
            <div className="space-y-1.5">
              {snapList.map((p) => (
                <div
                  key={p.slot}
                  className="flex items-center justify-between gap-3 rounded-lg border border-slate-100 px-3 py-2 dark:border-zinc-800"
                >
                  <div className="min-w-0">
                    <div className="truncate font-mono text-xs font-medium">{p.slot}</div>
                    <div className="mt-0.5 text-xs text-slate-400">
                      更新于 {p.last_modified || '—'} · {fmtSize(p.size_bytes)} · {p.file_count} 个文件
                    </div>
                  </div>
                  <div className="flex shrink-0 gap-1.5">
                    <button
                      className="btn-outline !px-2 !py-1 text-xs"
                      disabled={snapLoading || snapOp != null || snapConfirm != null}
                      title="恢复该快照并启动客户端"
                      onClick={() => setSnapConfirm({ slot: p.slot, kind: 'restore' })}
                    >
                      {snapOp?.slot === p.slot && snapOp.kind === 'restore' ? <Spinner /> : null} 恢复并启动
                    </button>
                    <button
                      className="btn-outline !px-2 !py-1 text-xs !text-rose-600 hover:!border-rose-300"
                      disabled={snapLoading || snapOp != null || snapConfirm != null}
                      onClick={() => setSnapConfirm({ slot: p.slot, kind: 'delete' })}
                    >
                      删除
                    </button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      </Modal>

      {/* 快照操作二次确认弹框（恢复并启动 / 删除；禁 window.confirm，红线） */}
      <Modal
        open={snapConfirm != null}
        onClose={() => {
          if (!snapOp) setSnapConfirm(null);
        }}
        title={snapConfirm?.kind === 'restore' ? '恢复并启动' : '删除快照'}
        footer={
          <>
            <button className="btn-outline" onClick={() => setSnapConfirm(null)} disabled={snapOp != null}>
              取消
            </button>
            <button
              className={`btn-primary ${snapConfirm?.kind === 'delete' ? '!bg-rose-600 hover:!bg-rose-500' : ''}`}
              onClick={() => void confirmSnapOp()}
              disabled={snapOp != null}
            >
              {snapOp ? <Spinner /> : null} 确认{snapConfirm?.kind === 'restore' ? '恢复' : '删除'}
            </button>
          </>
        }
      >
        <div className="space-y-1 text-sm">
          {snapConfirm?.kind === 'restore' ? (
            <>
              <div>恢复快照「{snapConfirm?.slot}」到 {snapTarget} 并启动客户端？</div>
              <div className="rounded-lg bg-amber-50 p-3 text-xs text-amber-700 dark:bg-amber-500/10 dark:text-amber-300">
                恢复会以该快照覆盖 {snapTarget} 当前的登录态文件；执行前请确认当前登录可被替换。
              </div>
            </>
          ) : (
            <>
              <div>确定删除快照「{snapConfirm?.slot}」？</div>
              <div className="mt-1 text-xs text-slate-400">删除后无法恢复；账号池中的账号与凭证副本不受影响。</div>
            </>
          )}
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
    </div>
  );
}
