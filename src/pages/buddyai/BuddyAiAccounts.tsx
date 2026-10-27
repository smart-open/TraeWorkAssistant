import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import {
  RefreshCw,
  Globe,
  KeyRound,
  Trash2,
  Loader2,
  Pencil,
  Download,
  HelpCircle,
  FolderCog,
  Tags,
  Copy,
  Upload,
  ScanSearch,
  ShieldAlert,
  LogIn,
  Save,
  AppWindow,
  TerminalSquare,
} from 'lucide-react';
import { listen } from '@tauri-apps/api/event';
import { save } from '@tauri-apps/plugin-dialog';
import { Badge, EmptyState, Modal, Spinner } from '../../components/ui';
import PageHeader from '../../components/PageHeader';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import { GroupsModal } from '../accounts/GroupsModal';
import { TagChips } from '../accounts/TagSelect';
import { AccountTagsEditModal, TagsModal } from '../accounts/TagsModal';
import { BuddyAiHelpModal } from './HelpModal';
import type { BuddyAiAccountView, GroupView, TagView, WbCheckinRecord, WbOauthDone, WbOauthProgress } from '../../types';

/**
 * BuddyAI（buddy-ai 国际版）账号管理页（P3 对齐国内 Buddy 账号管理）。
 *
 * 列对齐：账号（昵称+uid）/ 分组（共享分组下拉）/ 备注 / 积分 / TOKEN 到期 /
 * 凭证状态 / 操作（切换·保存·续期·编辑·删除）+ 批量勾选移除。
 * 国际不适用项（不展示）：每日目标 / 休假锁单 / 签到策略（intl 打卡为 ACP 链，
 * 无 UI 签到通道与签到点概念）。
 *
 * §3.4 隔离清单：本页所有命令走 buddy_ai_* 独立池；OAuth 事件前缀 buddy-ai-oauth-*。
 */

type ConfirmTarget =
  | { kind: 'remove-account'; account: BuddyAiAccountView }
  | { kind: 'remove-batch'; ids: string[] }
  | null;

export default function BuddyAiAccounts() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [accounts, setAccounts] = useState<BuddyAiAccountView[] | null>(null);
  const [groups, setGroups] = useState<GroupView[]>([]);
  const [loading, setLoading] = useState(true);
  const [rowOp, setRowOp] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirmTarget, setConfirmTarget] = useState<ConfirmTarget>(null);
  const [confirmBusy, setConfirmBusy] = useState(false);
  // OAuth 弹框
  const [oauthOpen, setOauthOpen] = useState(false);
  const [oauthStage, setOauthStage] = useState('');
  const [oauthMessage, setOauthMessage] = useState('');
  const [oauthAuthUrl, setOauthAuthUrl] = useState<string | null>(null);
  const [oauthBusy, setOauthBusy] = useState(false);
  // 编辑弹框（昵称/备注）
  const [editTarget, setEditTarget] = useState<BuddyAiAccountView | null>(null);
  const [editName, setEditName] = useState('');
  const [editNote, setEditNote] = useState('');
  const [editBusy, setEditBusy] = useState(false);
  // 导出弹框（P8：默认脱敏；附带凭证副本需强确认——导出文件等同密码）
  const [exportOpen, setExportOpen] = useState(false);
  const [exportWithCreds, setExportWithCreds] = useState(false);
  const [exportBusy, setExportBusy] = useState(false);
  // 审查修复（第5项）：含凭证导出必须设密码（AIWQENC1 加密信封），双输入防手误
  const [exportPwd, setExportPwd] = useState('');
  const [exportPwd2, setExportPwd2] = useState('');
  // 审查修复（第5项）：加密导出文件导入时需密码解密（AIWQENC1 信封），暂存解析后的信封载荷
  const [importEnvelope, setImportEnvelope] = useState<Record<string, unknown> | null>(null);
  const [importPwd, setImportPwd] = useState('');
  const [importBusy, setImportBusy] = useState(false);
  // 帮助 / 分组管理 / 标签管理弹框（对齐国内 BuddyAccounts）
  const [helpOpen, setHelpOpen] = useState(false);
  const [groupOpen, setGroupOpen] = useState(false);
  const [tagsOpen, setTagsOpen] = useState(false);
  // 扫描本机账号 / 导入账号 / 环境重置（对齐国内 BuddyAccounts 功能集）
  const [scanOpen, setScanOpen] = useState(false);
  const [scanBusy, setScanBusy] = useState(false);
  const [scanResult, setScanResult] = useState<Record<string, unknown> | null>(null);
  const [resetOpen, setResetOpen] = useState(false);
  const [resetItems, setResetItems] = useState<Set<string>>(new Set());
  const [resetConfirm, setResetConfirm] = useState(false);
  const [resetBusy, setResetBusy] = useState(false);
  const importFileRef = useRef<HTMLInputElement>(null);
  const [tagDefs, setTagDefs] = useState<TagView[]>([]);
  const [tagMap, setTagMap] = useState<Record<string, string[]>>({});
  const [tagEditTarget, setTagEditTarget] = useState<BuddyAiAccountView | null>(null);
  // 积分包明细弹窗（对齐国内「积分包」列内弹窗；数据源 credits_packages 池快照）
  const [pkgTarget, setPkgTarget] = useState<BuddyAiAccountView | null>(null);
  // 今日打卡状态（打卡状态列；对齐国内「今日已打」徽标列）
  const [todayStatus, setTodayStatus] = useState<Map<string, WbCheckinRecord>>(new Map());
  // 分组过滤 chips + 客户端分页（对齐国内 BuddyAccounts：chips 单选筛选，表尾 grid-cols-2 分页）
  const [chipFilter, setChipFilter] = useState('');
  const [page, setPage] = useState(1);
  // 切换/保存登录态菜单（复刻国内 appMenu）：{ id, kind } —— kind='switch' 切换登录态 / 'save' 保存登录态
  const [appMenu, setAppMenu] = useState<{ id: string; kind: 'switch' | 'save'; x: number; y: number } | null>(null);
  // 切换/保存进度（页面级：进度行 + 忙态互斥 + 90s 看门狗；事件契约与国内同源）
  const [swLines, setSwLines] = useState<string[]>([]);
  const [swBusy, setSwBusy] = useState(false);
  const swBusyRef = useRef(false);

  const reloadTags = useCallback(async () => {
    try {
      const [defs, map] = await Promise.all([api.tags.list(), api.tags.map()]);
      setTagDefs(defs);
      setTagMap(map);
    } catch {
      // 标签为增强面：失败不阻塞账号列表（静默保留旧值）
    }
  }, []);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const [accs, gs, recs] = await Promise.all([
        api.buddyai.accountsList(),
        api.groups.sharedList().catch(() => [] as GroupView[]),
        api.buddyai.checkinResults(7).catch(() => [] as WbCheckinRecord[]),
      ]);
      setAccounts(accs);
      setGroups(gs);
      // 打卡状态列（对齐国内「今日已打」）：records.user_id = 池账号 id
      const today = new Date();
      const todayStr = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-${String(today.getDate()).padStart(2, '0')}`;
      const m = new Map<string, WbCheckinRecord>();
      for (const r of recs) if (r.date === todayStr) m.set(r.user_id, r);
      setTodayStatus(m);
      // FH-4 配套：selected 裁剪到现存账号（他端增删后全选框判定不失真）
      setSelected((prev) => {
        const ids = new Set(accs.map((a) => a.id));
        const next = new Set([...prev].filter((id) => ids.has(id)));
        return next.size === prev.size ? prev : next;
      });
    } catch (err) {
      pushToast('error', `账号列表读取失败：${String(err).slice(0, 120)}`);
    } finally {
      setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void reloadTags();
  }, [reloadTags]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const groupName = useMemo(() => {
    const m = new Map<string, string>();
    for (const g of groups) m.set(g.id, g.name);
    return m;
  }, [groups]);

  // OAuth 弹框事件契约：事件前缀 buddy-ai-oauth-*（310s 看门狗兜底事件丢失）。
  // FL-6 审查修复：监听只在弹框打开时注册一次（原依赖含 oauthBusy，done 置 false
  // 会触发 cleanup+重挂监听/重启 watchdog——无泄漏但属无谓重建）
  useEffect(() => {
    if (!oauthOpen) return;
    let watchdog: ReturnType<typeof setTimeout> | undefined;
    // done 成功后 1.2s 自动关闭弹框（审查残留项修复：定时器入 cleanup 清理，
    // 用户提前手动关闭/切走页面时不再触发卸载后 setState）
    let closeTimer: ReturnType<typeof setTimeout> | undefined;
    const unProgress = listen<WbOauthProgress>('buddy-ai-oauth-progress', (e) => {
      setOauthStage(e.payload.stage ?? '');
      setOauthMessage(e.payload.message ?? '');
      if (e.payload.auth_url) setOauthAuthUrl(e.payload.auth_url);
    });
    const unDone = listen<WbOauthDone>('buddy-ai-oauth-done', (e) => {
      // 审查修复（L26 / Low 修复）：done 即定局（成功或失败均已展示），撤销看门狗——
      // 原 310s 定时器在结果信息已展示后仍会触发，把结果覆盖成「登录超时」误导排查
      if (watchdog) clearTimeout(watchdog);
      watchdog = undefined;
      setOauthBusy(false);
      if (e.payload.ok) {
        pushToast('success', e.payload.message || '国际账号已扫码登录并自动入池');
        void refresh();
        closeTimer = setTimeout(() => setOauthOpen(false), 1200);
      } else {
        setOauthMessage(e.payload.message || '登录失败');
        pushToast('error', e.payload.message || '登录失败');
      }
    });
    watchdog = setTimeout(() => {
      setOauthBusy(false);
      setOauthMessage('登录超时（超过 310 秒无响应），请重试');
    }, 310_000);
    return () => {
      void unProgress.then((f) => f());
      void unDone.then((f) => f());
      if (watchdog) clearTimeout(watchdog);
      if (closeTimer) clearTimeout(closeTimer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [oauthOpen]);

  const startOauth = async () => {
    if (oauthBusy) return;
    setOauthOpen(true);
    setOauthStage('init');
    setOauthMessage('正在请求登录 state…');
    setOauthAuthUrl(null);
    setOauthBusy(true);
    try {
      await api.buddyai.oauthLogin();
    } catch (err) {
      setOauthBusy(false);
      setOauthMessage(String(err));
      pushToast('error', `发起登录失败：${String(err).slice(0, 120)}`);
    }
  };

  const doRenew = async (a: BuddyAiAccountView) => {
    setRowOp(`renew:${a.id}`);
    try {
      const msg = await withMinDelay(api.buddyai.refreshToken(a.id), 800);
      pushToast('success', `${a.nickname || a.id}：${msg}`);
      await refresh();
    } catch (err) {
      pushToast('error', `续期失败：${String(err).slice(0, 160)}`);
    } finally {
      setRowOp(null);
    }
  };

  // ── 切换/保存登录态（对齐国内 BuddyAccounts appMenu + 进度事件契约）──
  // 目标端：WorkBuddyAI（国际客户端独立档案）/ CodeBuddyAI（与国内 CodeBuddy 同客户端，
  // 后端按 buddy-ai 池反查区分）；进度经 switch-progress / save-login-progress NDJSON。
  useEffect(() => {
    const unsubs: Array<() => void> = [];
    let disposed = false;
    const track = (p: Promise<() => void>) => void p.then((u) => (disposed ? u() : unsubs.push(u)));
    const onLine = (ev: { payload: unknown }) => {
      if (!swBusyRef.current) return;
      try {
        const p = JSON.parse(String(ev.payload)) as Record<string, unknown>;
        const msg = String(p.message ?? '');
        const status = String(p.status ?? '');
        if (msg) setSwLines((prev) => [...prev.slice(-60), `[${status}] ${msg}`]);
      } catch {
        /* 非 NDJSON 行忽略 */
      }
    };
    track(listen<string>('switch-progress', onLine));
    track(listen<string>('save-login-progress', onLine));
    track(
      listen<{ success: boolean; raw?: string }>('switch-done', (e) => {
        if (!swBusyRef.current) return;
        setSwBusy(false);
        swBusyRef.current = false;
        // M9 修复（审查 L25）：成功/失败完成 toast 由 store 全局监听统一推送
        // （onSwitchDone），页面级只负责解锁与刷新，避免双 toast；与国内
        // BuddyAccounts 口径一致
        void refresh();
      }),
    );
    track(
      listen<{ success: boolean; raw?: string }>('save-login-done', (e) => {
        if (!swBusyRef.current) return;
        setSwBusy(false);
        swBusyRef.current = false;
        // M9 修复（审查 L25）：同上，完成 toast 由 store 全局监听统一发（onSaveLoginDone）
        void refresh();
      }),
    );
    return () => {
      disposed = true;
      for (const u of unsubs) u();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 看门狗：done 事件异常缺失时 90s 解锁（对齐国内 issue #44 兜底）
  useEffect(() => {
    if (!swBusy) return;
    const timer = setTimeout(() => {
      setSwBusy(false);
      swBusyRef.current = false;
      pushToast('warn', '切换/保存超过 90 秒未收到完成事件，已解除按钮锁定；结果以日志与列表状态为准');
    }, 90_000);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [swBusy]);

  /** 打开目标应用选择菜单（复刻国内 appMenu 定位）；切换对需重登账号拦截 */
  const openAppMenu = (a: BuddyAiAccountView, kind: 'switch' | 'save', e: { clientX: number; clientY: number }) => {
    if (swBusy) return;
    if (kind === 'switch' && a.needs_relogin) {
      pushToast('warn', '该账号标记需重新登录，请先重新登录客户端并保存登录态');
      return;
    }
    setAppMenu({ id: a.id, kind, x: e.clientX, y: e.clientY });
  };

  /** 菜单选中目标端后真正发起；busy 在此刻置位（菜单未选中即关闭则不置忙，
   *  避免误锁 90s——审查修复：原在 openAppMenu 提前置忙，用户移开鼠标未选择
   *  也会锁定全部行按钮直到看门狗超时） */
  const runAppAction = async (id: string, kind: 'switch' | 'save', app: 'WorkBuddyAI' | 'CodeBuddyAI') => {
    setAppMenu(null);
    setSwLines([]);
    setSwBusy(true);
    swBusyRef.current = true;
    try {
      if (kind === 'switch') await api.switchAccount(id, app);
      else await api.saveCurrentLogin(id, app);
    } catch (err) {
      setSwBusy(false);
      swBusyRef.current = false;
      pushToast('error', `${kind === 'switch' ? '切换' : '保存'}发起失败：${String(err).slice(0, 140)}`);
    }
  };

  const openEdit = (a: BuddyAiAccountView) => {
    setEditTarget(a);
    setEditName(a.nickname);
    setEditNote(a.note);
  };

  const confirmEdit = async () => {
    if (!editTarget) return;
    setEditBusy(true);
    try {
      await api.buddyai.accountSave(editTarget.id, { name: editName.trim(), note: editNote.trim() });
      pushToast('success', '账号信息已保存');
      setEditTarget(null);
      await refresh();
    } catch (err) {
      pushToast('error', `保存失败：${String(err).slice(0, 120)}`);
    } finally {
      setEditBusy(false);
    }
  };

  const setGroup = async (a: BuddyAiAccountView, group_id: string) => {
    try {
      await api.buddyai.accountSave(a.id, { group_id });
      pushToast('success', '分组已更新');
      await refresh();
    } catch (err) {
      pushToast('error', `分组更新失败：${String(err).slice(0, 120)}`);
    }
  };

  // 导出账号池（P8 对齐国内 F-46：默认脱敏；凭证副本强确认——含凭证必须设密码加密导出）
  const doExport = async () => {
    // 审查修复（第5项）：含凭证导出必须提供非空且一致的密码，否则拒绝发起导出
    if (exportWithCreds && (!exportPwd.trim() || exportPwd !== exportPwd2)) {
      pushToast('error', '两次输入的导出密码为空或不一致，无法导出含凭证的文件');
      return;
    }
    setExportBusy(true);
    try {
      const payload = await api.buddyai.accountsExport(exportWithCreds, exportWithCreds ? exportPwd : undefined);
      // FL-5：导出计数取自导出载荷（页面快照可能已过期）；加密信封顶层无 accounts，回退页面计数
      const exportedCount = Array.isArray(payload.accounts) ? payload.accounts.length : (accounts?.length ?? 0);
      const filePath = await save({
        defaultPath: `buddy-ai-accounts-${new Date().toISOString().slice(0, 10)}.json`,
        filters: [{ name: 'JSON', extensions: ['json'] }],
      });
      if (!filePath) {
        setExportBusy(false);
        return;
      }
      await api.misc.writeTextFile(filePath, JSON.stringify(payload, null, 2));
      // 加密提示按后端实际返回形态判定：池内无任何凭证时后端原样明文导出（无 encrypted 字段），
      // 勾选「含凭证」但实际无凭证时不应误导用户「已加密」
      const sealed = payload.encrypted === true;
      pushToast('success', `已导出 ${exportedCount} 个账号到 ${filePath}${sealed ? '（凭证已 AES-256-GCM 加密，请牢记导出密码）' : ''}`);
      setExportOpen(false);
    } catch (err) {
      pushToast('error', `导出失败：${String(err).slice(0, 160)}`);
    } finally {
      setExportBusy(false);
    }
  };

  /** 扫描本机 WorkBuddy AI 登录账号（account-snapshot.json 识别 + 池内匹配） */
  const doLocalScan = async () => {
    setScanBusy(true);
    try {
      const r = await api.buddyai.localScan();
      setScanResult(r);
      setScanOpen(true);
    } catch (err) {
      pushToast('error', `扫描失败：${String(err).slice(0, 140)}`);
    } finally {
      setScanBusy(false);
    }
  };

  /** 导入账号 JSON（accounts_export 回导；合并语义：同 id 跳过不覆盖；加密信封文件先弹密码框） */
  const doImport = async (file: File) => {
    try {
      const parsed = JSON.parse(await file.text()) as Record<string, unknown>;
      // 审查修复（第5项）：AIWQENC1 加密信封（顶层 data 为 base64 字符串，与后端 envelope_data_of 判据一致）需密码解密
      if (typeof parsed?.data === 'string' && parsed.data.length > 0) {
        setImportPwd('');
        setImportEnvelope(parsed);
        return;
      }
      const r = await api.buddyai.accountsImport(parsed);
      pushToast(
        'success',
        `导入完成：新增 ${r.added} · 跳过 ${r.skipped}（已存在）· 无效 ${r.invalid} · 凭证回灌 ${r.tokens_added}`,
      );
      await refresh();
    } catch (err) {
      pushToast('error', `导入失败：${String(err).slice(0, 160)}`);
    }
  };

  /** 审查修复（第5项）：导入加密信封文件（AIWQENC1）——带密码交后端解密后按原合并语义入池 */
  const doImportEncrypted = async () => {
    if (!importEnvelope || !importPwd.trim()) return;
    setImportBusy(true);
    try {
      const r = await api.buddyai.accountsImport(importEnvelope, importPwd);
      pushToast(
        'success',
        `导入完成：新增 ${r.added} · 跳过 ${r.skipped}（已存在）· 无效 ${r.invalid} · 凭证回灌 ${r.tokens_added}`,
      );
      setImportEnvelope(null);
      setImportPwd('');
      await refresh();
    } catch (err) {
      pushToast('error', `导入失败：${String(err).slice(0, 160)}`);
    } finally {
      setImportBusy(false);
    }
  };

  /** 环境重置（白名单目录清理；login = 登录态清除，需勾选强确认） */
  const RESET_ITEMS: [string, string][] = [
    ['cache', '缓存目录（cache）'],
    ['logs', '客户端日志（logs）'],
    ['pending-telemetry', '待上报遥测（pending-telemetry）'],
    ['traces', '链路追踪（traces）'],
    ['shell-snapshots', 'Shell 快照（shell-snapshots）'],
    ['editor-sdk-sandbox-ws', '沙箱工作区（editor-sdk-sandbox-ws）'],
    ['file-tree-manifests', '文件树清单（file-tree-manifests）'],
    ['memory', '客户端记忆（memory）'],
    ['workspace-sessions', '工作区会话（workspace/sessions）'],
    ['login', '本机登录态（security 加密凭证库 = 客户端登出，需重新登录）'],
  ];
  const doEnvReset = async () => {
    setResetBusy(true);
    try {
      const r = await api.buddyai.envReset([...resetItems]);
      if (r.failed > 0) pushToast('warn', `环境重置完成，${r.failed} 项失败（见结果明细）`);
      else pushToast('success', `环境重置完成（${r.results.length} 项）`);
      setResetOpen(false);
      setResetItems(new Set());
      setResetConfirm(false);
      await refresh();
    } catch (err) {
      pushToast('error', `环境重置失败：${String(err).slice(0, 160)}`);
    } finally {
      setResetBusy(false);
    }
  };

  const doRemove = async () => {
    if (!confirmTarget) return;
    setConfirmBusy(true);
    try {
      if (confirmTarget.kind === 'remove-account') {
        await withMinDelay(api.buddyai.accountRemove(confirmTarget.account.id), 800);
        pushToast('info', `已移除 ${confirmTarget.account.nickname || confirmTarget.account.id}`);
        setConfirmTarget(null);
      } else {
        // FH-4 审查修复：批量移除逐项容错——失败项收集继续，成功项即时从 selected
        // 剔除（原实现中途失败即中断且 selected 残留已删 id，重试必撞「账号不存在」，
        // 排在失败项之后的账号永远删不掉）
        const failed: string[] = [];
        let removed = 0;
        for (const id of confirmTarget.ids) {
          try {
            await api.buddyai.accountRemove(id);
            removed += 1;
            setSelected((prev) => {
              const next = new Set(prev);
              next.delete(id);
              return next;
            });
          } catch {
            failed.push(id);
          }
        }
        if (failed.length === 0) {
          pushToast('info', `已批量移除 ${removed} 个账号`);
          setConfirmTarget(null);
        } else {
          pushToast('error', `已移除 ${removed} 个，${failed.length} 个失败（${failed.slice(0, 3).join(', ')}${failed.length > 3 ? '…' : ''}），弹框保留可重试`);
        }
      }
      await refresh();
    } catch (err) {
      pushToast('error', `移除失败：${String(err).slice(0, 120)}`);
    } finally {
      setConfirmBusy(false);
    }
  };

  const toggleSelect = (id: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const expText = (ts: number | null) => (ts ? new Date(ts * 1000).toLocaleString() : '-');

  // 分组过滤 + 分页（对齐国内：chips 单选 → 表格 → 表尾分页）
  const PAGE_SIZE = 20;
  const filteredAccounts = useMemo(() => {
    if (!accounts) return [];
    if (chipFilter === '') return accounts;
    if (chipFilter === '__ungrouped__') return accounts.filter((a) => !a.group_id);
    return accounts.filter((a) => a.group_id === chipFilter);
  }, [accounts, chipFilter]);
  const totalPages = Math.max(Math.ceil(filteredAccounts.length / PAGE_SIZE), 1);
  const pagedAccounts = useMemo(
    () => filteredAccounts.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE),
    [filteredAccounts, page],
  );
  useEffect(() => setPage(1), [chipFilter]);
  useEffect(() => {
    if (page > totalPages) setPage(totalPages);
  }, [page, totalPages]);
  const allSelected =
    filteredAccounts.length > 0 && filteredAccounts.every((a) => selected.has(a.id));

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="BuddyAI · 账号管理"
        desc="多账号入池 · OAuth 登录 · 凭证续期"
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
            <button className="btn-outline" onClick={() => void startOauth()} disabled={oauthBusy} title={oauthBusy ? 'OAuth 流程进行中…' : undefined}>
              <Globe size={15} /> OAuth登录
            </button>
            <button className="btn-outline" onClick={() => void doLocalScan()} disabled={scanBusy} title="扫描本机 WorkBuddy AI 客户端登录账号（识别 + 池内匹配）">
              {scanBusy ? <Spinner /> : <ScanSearch size={15} />} 扫描本机账号
            </button>
            <button
              className="btn-outline"
              onClick={() => {
                // 审查修复（第5项）：每次打开导出弹框重置密码输入，避免残留上次的密码
                setExportPwd('');
                setExportPwd2('');
                setExportOpen(true);
              }}
              disabled={!accounts || accounts.length === 0}
              title="导出账号池为 JSON（可选是否附带凭证副本）"
            >
              <Download size={15} /> 导出账号
            </button>
            <button
              className="btn-outline"
              onClick={() => importFileRef.current?.click()}
              title="导入「导出账号」生成的 JSON（合并语义：同 id 跳过不覆盖）"
            >
              <Upload size={15} /> 导入账号
            </button>
            <input
              ref={importFileRef}
              type="file"
              accept=".json,application/json"
              className="hidden"
              onChange={(e) => {
                const f = e.target.files?.[0];
                if (f) void doImport(f);
                e.target.value = '';
              }}
            />
            <button
              className="btn-outline !text-rose-600 hover:!border-rose-300"
              onClick={() => {
                setResetItems(new Set());
                setResetConfirm(false);
                setResetOpen(true);
              }}
              title="清理国际客户端环境目录（白名单项，强确认）"
            >
              <ShieldAlert size={15} /> 环境重置
            </button>
            <button className="btn-outline" onClick={() => setGroupOpen(true)} title="新建 / 重命名 / 换色 / 删除分组">
              <FolderCog size={15} /> 分组管理
            </button>
            <button className="btn-outline" onClick={() => setTagsOpen(true)} title="共享标签管理（三池通用）">
              <Tags size={15} /> 标签管理
            </button>
            {selected.size > 0 && (
              <button
                className="btn-outline inline-flex items-center gap-1.5 text-red-500"
                onClick={() => setConfirmTarget({ kind: 'remove-batch', ids: [...selected] })}
              >
                <Trash2 size={15} /> 批量移除（{selected.size}）
              </button>
            )}
          </>
        }
      />

      {/* 切换/保存进度面板（对齐国内 SwitchProgressPanel：PageHeader 之后，忙态才渲染） */}
      {swBusy && (
        <div className="card mt-4 max-h-52 overflow-y-auto p-3">
          <div className="mb-1.5 flex items-center gap-2 text-xs font-medium text-slate-500">
            <Loader2 size={13} className="animate-spin text-brand-500" />
            登录态操作进行中（关闭客户端 → 快照 → 恢复/备份 → 重启，约 10-45 秒）
          </div>
          {swLines.map((l, i) => (
            <div key={i} className="font-mono text-[11px] leading-4 text-slate-500 dark:text-zinc-400">
              {l}
            </div>
          ))}
        </div>
      )}

      {/* 切换/保存目标端选择菜单（复刻国内 appMenu：portal 定位，鼠标移出关闭） */}
      {appMenu &&
        createPortal(
          <div
            className="fixed z-50 w-[420px] overflow-hidden rounded-lg border border-slate-200 bg-white text-slate-700 shadow-lg dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
            style={{
              // 右缘钳制（审查 P3）：窗口过窄时 420px 菜单可能溢出右边界
              left: Math.min(Math.max(8, appMenu.x - 420), Math.max(8, window.innerWidth - 428)),
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
              {(
                [
                  { app: 'WorkBuddyAI' as const, label: 'WorkBuddy', desc: 'WorkBuddy AI 桌面客户端（国际版）', Icon: AppWindow },
                  { app: 'CodeBuddyAI' as const, label: 'CodeBuddy', desc: 'CodeBuddy AI 桌面客户端（国际域）', Icon: TerminalSquare },
                ]
              ).map((opt) => (
                <button
                  key={opt.app}
                  onClick={() => {
                    const { id, kind } = appMenu;
                    void runAppAction(id, kind, opt.app);
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

      {/* 分组过滤 chips（对齐国内：全部/未分组/各分组 带色点+计数，单选筛选） */}
      <div className="mb-3 mt-5 flex flex-wrap items-center gap-2 text-sm">
        <button
          className={`rounded-full border px-3 py-1 text-xs ${
            chipFilter === ''
              ? 'border-brand-400 bg-brand-50 font-medium text-brand-600 dark:border-brand-500/40 dark:bg-brand-500/10 dark:text-brand-400'
              : 'border-slate-200 text-slate-500 hover:border-slate-300 dark:border-zinc-700 dark:text-zinc-400'
          }`}
          onClick={() => setChipFilter('')}
        >
          全部 <span className="tabular-nums">({accounts?.length ?? 0})</span>
        </button>
        <button
          className={`rounded-full border px-3 py-1 text-xs ${
            chipFilter === '__ungrouped__'
              ? 'border-brand-400 bg-brand-50 font-medium text-brand-600 dark:border-brand-500/40 dark:bg-brand-500/10 dark:text-brand-400'
              : 'border-slate-200 text-slate-500 hover:border-slate-300 dark:border-zinc-700 dark:text-zinc-400'
          }`}
          onClick={() => setChipFilter('__ungrouped__')}
        >
          未分组 <span className="tabular-nums">({(accounts ?? []).filter((a) => !a.group_id).length})</span>
        </button>
        {groups.map((g) => (
          <button
            key={g.id}
            className={`inline-flex items-center gap-1.5 rounded-full border px-3 py-1 text-xs ${
              chipFilter === g.id
                ? 'border-brand-400 bg-brand-50 font-medium text-brand-600 dark:border-brand-500/40 dark:bg-brand-500/10 dark:text-brand-400'
                : 'border-slate-200 text-slate-500 hover:border-slate-300 dark:border-zinc-700 dark:text-zinc-400'
            }`}
            onClick={() => setChipFilter(chipFilter === g.id ? '' : g.id)}
          >
            <span className="h-2 w-2 rounded-full" style={{ backgroundColor: g.color }} />
            {g.name} <span className="tabular-nums">({(accounts ?? []).filter((a) => a.group_id === g.id).length})</span>
          </button>
        ))}
      </div>

      {/* 账号列表（对齐国内：card 直排表格，内边距由单元格承担） */}
      <div className="card overflow-x-auto">
        {loading && accounts === null ? (
          <div className="flex items-center justify-center gap-2 py-10 text-sm text-slate-400">
            <Loader2 size={16} className="animate-spin" /> 加载中…
          </div>
        ) : !accounts || accounts.length === 0 ? (
          <EmptyState
            icon={<Globe size={28} />}
            title="还没有国际版账号"
            /* 审查修复（M9）：引导文案与页面按钮实名对齐（原「扫码登录/导入凭证」
               均非本页按钮名，用户照提示找不到入口），并补充「扫描本机账号」入口 */
            hint="点击「OAuth登录」完成 workbuddy.ai 授权自动入池；也可「扫描本机账号」识别本机客户端登录账号，或「导入账号」导入账号池 JSON"
          />
        ) : filteredAccounts.length === 0 ? (
          <div className="flex h-32 items-center justify-center text-sm text-slate-400">当前分组下无账号。</div>
        ) : (
          <>
          <table className="w-full min-w-[1080px] text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-4 py-2 text-left">
                  <input
                    type="checkbox"
                    checked={allSelected}
                    onChange={(e) =>
                      setSelected(
                        e.target.checked
                          ? new Set([...selected, ...filteredAccounts.map((a) => a.id)])
                          : new Set([...selected].filter((id) => !filteredAccounts.some((a) => a.id === id))),
                      )
                    }
                  />
                </th>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-left">分组</th>
                <th className="px-4 py-2 text-left">标签</th>
                <th className="px-4 py-2 text-left">积分包</th>
                <th className="px-4 py-2 text-right">可用积分</th>
                <th className="px-4 py-2 text-left">token 到期</th>
                <th className="px-4 py-2 text-left">签到状态</th>
                <th className="px-4 py-2 text-left">状态</th>
                <th className="px-4 py-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              {pagedAccounts.map((a) => {
                // 7 日内将过期（仍有剩余）的积分包 → 列表标记提示（对齐国内口径）
                const nowSecRow = Math.floor(Date.now() / 1000);
                const soonestExpire = (a.credits_packages ?? []).reduce<number | null>((min, p) => {
                  if (p.expire_secs == null || p.expire_secs <= nowSecRow) return min;
                  if (p.expire_secs > nowSecRow + 7 * 86400) return min;
                  return min == null ? p.expire_secs : Math.min(min, p.expire_secs);
                }, null);
                return (
                <tr key={a.id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                  <td className="px-4 py-3">
                    <input type="checkbox" checked={selected.has(a.id)} onChange={() => toggleSelect(a.id)} />
                  </td>
                  <td className="px-4 py-3">
                    <div className="flex items-center gap-1">
                      <span className="font-medium">{a.nickname || a.id}</span>
                      {a.is_current_workbuddy && (
                        <Badge tone="green" title="WorkBuddy AI 端当前账号（国际 auth 文件 uid 匹配）">WB当前</Badge>
                      )}
                      {a.is_current_codebuddy && (
                        <Badge tone="blue" title="CodeBuddy AI 端当前账号（live storage.json genie.uid 匹配）">CB当前</Badge>
                      )}
                      {/* 快照健康度（双端）：切换后登录身份未确认 → 红色警示；健康 → 灰色快照态（含保存时间） */}
                      {(() => {
                        const wbFail = a.snapshot_verify_failed_workbuddy_ms != null;
                        const cbFail = a.snapshot_verify_failed_codebuddy_ms != null;
                        const fmt = (ms: number | null | undefined) =>
                          ms != null ? new Date(ms).toLocaleString('zh-CN') : null;
                        if (wbFail || cbFail) {
                          const parts = [
                            wbFail && `WorkBuddy AI 快照最近一次切换未确认登录身份（${fmt(a.snapshot_verify_failed_workbuddy_ms)}）`,
                            cbFail && `CodeBuddy AI 快照最近一次切换未确认登录身份（${fmt(a.snapshot_verify_failed_codebuddy_ms)}）`,
                          ].filter(Boolean);
                          return (
                            <Badge
                              tone="red"
                              title={`${parts.join('；')}：登录态可能已失效，请打开客户端核实，必要时重新登录后「保存当前登录态」`}
                            >
                              快照验证失败
                            </Badge>
                          );
                        }
                        const saved = [
                          a.snapshot_saved_at_workbuddy_ms != null && `WorkBuddy AI 快照保存于 ${fmt(a.snapshot_saved_at_workbuddy_ms)}`,
                          a.snapshot_saved_at_codebuddy_ms != null && `CodeBuddy AI 快照保存于 ${fmt(a.snapshot_saved_at_codebuddy_ms)}`,
                        ].filter(Boolean);
                        return saved.length > 0 ? (
                          <Badge tone="slate" title={saved.join('；')}>快照✓</Badge>
                        ) : null;
                      })()}
                    </div>
                    <div className="font-mono text-xs text-slate-400">
                      {a.uid || a.id}
                      {a.note ? <span className="ml-1.5 font-sans text-[11px] font-normal text-slate-400">（{a.note}）</span> : null}
                    </div>
                  </td>
                  <td className="px-4 py-3">
                    <select
                      className="rounded-md border border-slate-200 bg-transparent px-1.5 py-1 text-xs focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900"
                      value={a.group_id}
                      onChange={(e) => void setGroup(a, e.target.value)}
                    >
                      <option value="">未分组</option>
                      {groups.map((g) => (
                        <option key={g.id} value={g.id}>
                          {g.name}
                        </option>
                      ))}
                    </select>
                  </td>
                  <td className="px-4 py-3">
                    <TagChips tagIds={tagMap[a.id] ?? []} allTags={tagDefs} onEdit={() => setTagEditTarget(a)} />
                  </td>
                  <td className="px-4 py-3">
                    {(a.credits_packages?.length ?? 0) > 0 ? (
                      <div className="flex flex-col items-start gap-1">
                        <button
                          className="flex items-center gap-1 text-xs text-brand-600 hover:underline dark:text-brand-400"
                          onClick={() => setPkgTarget(a)}
                          title="查看积分包明细"
                        >
                          {a.credits_packages!.length} 个包
                        </button>
                        {soonestExpire != null && (
                          <Badge tone="amber" title={`有积分包将于 ${new Date(soonestExpire * 1000).toLocaleDateString('zh-CN')} 过期，明细见积分包弹窗`}>
                            7 日内到期
                          </Badge>
                        )}
                      </div>
                    ) : (
                      <span className="text-xs text-slate-300 dark:text-zinc-600">-</span>
                    )}
                  </td>
                  <td className="px-4 py-3 text-right tabular-nums">
                    {a.credits_balance != null ? a.credits_balance.toFixed(2) : '-'}
                  </td>
                  <td className="px-4 py-3 text-xs">
                    <div className={a.access_token_expires_at && a.access_token_expires_at * 1000 < Date.now() ? 'text-rose-500' : 'text-slate-500 dark:text-zinc-400'}>
                      {expText(a.access_token_expires_at)}
                    </div>
                  </td>
                  <td className="px-4 py-3">
                    {(() => {
                      const rec = todayStatus.get(a.id);
                      if (rec?.status === 'success')
                        return <Badge tone="green" title={rec.message}>已打{rec.reward != null ? ` +${rec.reward}` : ''}</Badge>;
                      if (rec?.status === 'already') return <Badge tone="blue" title={rec.message}>已打（本轮已打）</Badge>;
                      if (rec?.status === 'fail') return <Badge tone="red" title={rec.message}>打卡失败</Badge>;
                      return <Badge tone="slate">未打</Badge>;
                    })()}
                  </td>
                  <td className="px-4 py-3">
                    {/* 状态列（凭证健康度，非启停语义——池启停在「资源调度」页账号池勾选） */}
                    <div className="flex flex-col gap-1">
                      <div className="flex items-center gap-1">
                        {a.needs_relogin ? (
                          <Badge tone="red">需重登</Badge>
                        ) : !a.has_credential ? (
                          <Badge tone="amber">无凭证</Badge>
                        ) : (
                          <Badge tone="green">可用</Badge>
                        )}
                      </div>
                      {a.needs_relogin && a.relogin_reason ? (
                        <div className="text-[11px] text-slate-400">{a.relogin_reason}</div>
                      ) : null}
                    </div>
                  </td>
                  <td className="px-4 py-3">
                    <div className="flex justify-end gap-1">
                      {/* 操作列顺序对齐国内 BuddyAccounts：切换 → 保存 → 续期 → 编辑 → 删除 */}
                      <button
                        className={`btn-ghost !p-2 text-emerald-600 hover:bg-emerald-50 dark:text-emerald-400 dark:hover:bg-emerald-500/10 ${rowOp !== null || swBusy ? 'opacity-40 cursor-not-allowed' : ''}`}
                        title="设为当前（选择目标应用）"
                        disabled={rowOp !== null || swBusy}
                        onClick={(e) => openAppMenu(a, 'switch', e)}
                      >
                        <LogIn size={14} />
                      </button>
                      <button
                        className={`btn-ghost !p-2 ${rowOp !== null || swBusy ? 'opacity-40 cursor-not-allowed' : ''}`}
                        title="保存当前登录态（选择目标应用）"
                        disabled={rowOp !== null || swBusy}
                        onClick={(e) => openAppMenu(a, 'save', e)}
                      >
                        <Save size={14} />
                      </button>
                      <button
                        className={`btn-ghost !p-2 text-amber-500 hover:bg-amber-50 dark:hover:bg-amber-500/10 ${rowOp !== null ? 'opacity-40 cursor-not-allowed' : ''}`}
                        title="凭证续期（refreshToken 换新 accessToken）"
                        disabled={rowOp !== null || !a.has_credential}
                        onClick={() => void doRenew(a)}
                      >
                        {rowOp === `renew:${a.id}` ? <Loader2 size={14} className="animate-spin" /> : <KeyRound size={14} />}
                      </button>
                      <button
                        className="btn-ghost !p-2 text-slate-500 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800"
                        title="编辑昵称/备注"
                        disabled={rowOp !== null}
                        onClick={() => openEdit(a)}
                      >
                        <Pencil size={14} />
                      </button>
                      <button
                        className={`btn-ghost !p-2 text-rose-500 hover:bg-rose-50 dark:hover:bg-rose-500/10 ${rowOp !== null ? 'opacity-40 cursor-not-allowed' : ''}`}
                        title="移除账号（池条目 + 凭证 + vault 条目三清）"
                        disabled={rowOp !== null}
                        onClick={() => setConfirmTarget({ kind: 'remove-account', account: a })}
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
          {/* 表尾分页（对齐国内 grid-cols-2：左侧计数、右侧翻页） */}
          <div className="mt-3 grid grid-cols-2 items-center px-4 pb-3 text-xs text-slate-400">
            <span>
              共 {filteredAccounts.length} 个账号 · 每页 {PAGE_SIZE} 条 · 第 {page}/{totalPages} 页
            </span>
            <div className="flex items-center justify-end gap-2">
              <button className="btn-outline px-2 py-1" disabled={page <= 1} onClick={() => setPage((p) => Math.max(p - 1, 1))}>
                上一页
              </button>
              <span className="tabular-nums">{page} / {totalPages}</span>
              <button className="btn-outline px-2 py-1" disabled={page >= totalPages} onClick={() => setPage((p) => Math.min(p + 1, totalPages))}>
                下一页
              </button>
            </div>
          </div>
          </>
        )}
      </div>

      {/* OAuth 进度弹框 */}
      <Modal
        open={oauthOpen}
        onClose={() => {
          if (!oauthBusy) setOauthOpen(false);
        }}
        title="Buddy 国际版扫码登录"
        widthClass="max-w-md"
      >
        <div className="space-y-3 text-sm">
          <div className="flex items-center gap-2">
            {oauthBusy ? <Loader2 size={15} className="animate-spin text-brand-500" /> : <Globe size={15} className="text-brand-500" />}
            <span className="font-medium">{oauthStage === 'done' ? '完成' : '进行中'}</span>
          </div>
          <p className="text-slate-500 dark:text-zinc-400">{oauthMessage}</p>
          {oauthAuthUrl ? (
            <>
              <a href={oauthAuthUrl} target="_blank" rel="noreferrer" className="btn-primary inline-flex w-full items-center justify-center gap-2">
                打开登录页（workbuddy.ai）
              </a>
              {/* 浏览器未自动打开时的兜底（对齐国内「复制完整链接」） */}
              <button
                className="btn-outline inline-flex w-full items-center justify-center gap-2"
                onClick={() => {
                  void navigator.clipboard
                    .writeText(oauthAuthUrl)
                    .then(() => pushToast('success', '登录链接已复制，请勿泄露'))
                    .catch(() => pushToast('error', '复制失败，请手动选中地址栏链接复制'));
                }}
              >
                <Copy size={14} /> 复制完整链接
              </button>
            </>
          ) : null}
          <p className="text-[11px] leading-4 text-slate-400">
            登录流程：浏览器完成 workbuddy.ai 授权后，网关自动轮询拿票并国际账号入池（最长 300 秒）。
          </p>
          <div className="flex justify-end">
            <button className="btn-secondary" disabled={oauthBusy} onClick={() => setOauthOpen(false)}>
              {oauthBusy ? '运行中…' : '关闭'}
            </button>
          </div>
        </div>
      </Modal>

      {/* 编辑弹框 */}
      <Modal open={editTarget !== null} onClose={() => !editBusy && setEditTarget(null)} title="编辑账号" widthClass="max-w-sm">
        {editTarget ? (
          <div className="space-y-3 text-sm">
            <div>
              <label className="label">昵称</label>
              <input className="input" value={editName} onChange={(e) => setEditName(e.target.value)} />
            </div>
            <div>
              <label className="label">备注</label>
              <input className="input" value={editNote} onChange={(e) => setEditNote(e.target.value)} placeholder="如：主力打卡号" />
            </div>
            <div className="flex justify-end gap-2 pt-1">
              <button className="btn-secondary" disabled={editBusy} onClick={() => setEditTarget(null)}>
                取消
              </button>
              <button className="btn-primary inline-flex items-center gap-1.5" disabled={editBusy} onClick={() => void confirmEdit()}>
                {editBusy ? <Loader2 size={14} className="animate-spin" /> : null} 保存
              </button>
            </div>
          </div>
        ) : null}
      </Modal>

      {/* 导出弹框（P8：默认脱敏；凭证副本强确认） */}
      <Modal open={exportOpen} onClose={() => !exportBusy && setExportOpen(false)} title="导出 BuddyAI 账号池" widthClass="max-w-sm">
        <div className="space-y-3 text-sm">
          <div>导出账号池为 JSON 文件，可用于备份或迁移到其他设备的 BuddyAI 应用。</div>
          <label className="flex items-start gap-2">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={exportWithCreds}
              onChange={(e) => setExportWithCreds(e.target.checked)}
            />
            <span>
              附带凭证副本
              <span className="ml-1 text-xs text-slate-400">（不勾选 = 仅账号清单，导入后需重新 OAuth 登录或导入带凭证的账号 JSON）</span>
            </span>
          </label>
          {exportWithCreds ? (
            <div className="rounded-md bg-red-50 px-3 py-2 text-[11px] leading-4 text-red-600 dark:bg-red-500/10 dark:text-red-300">
              ⚠ 勾选后 access_token / refresh_token 将以 AES-256-GCM 加密写入导出文件（等同账号密码）——请妥善保管文件并牢记导出密码，导入时需输入。
            </div>
          ) : null}
          {exportWithCreds ? (
            <>
              <div>
                <label className="label">导出密码（用于加密凭证）</label>
                <input
                  type="password"
                  className="input"
                  value={exportPwd}
                  onChange={(e) => setExportPwd(e.target.value)}
                  placeholder="导入加密文件时需输入此密码"
                />
              </div>
              <div>
                <label className="label">确认密码</label>
                <input
                  type="password"
                  className="input"
                  value={exportPwd2}
                  onChange={(e) => setExportPwd2(e.target.value)}
                  onKeyDown={(e) => {
                    // 审查修复（第5项）：回车快捷提交，仅当密码非空且两次一致时允许
                    if (e.key === 'Enter' && !exportBusy && exportPwd.trim() && exportPwd === exportPwd2) void doExport();
                  }}
                  placeholder="再次输入相同密码"
                />
                {exportPwd2.length > 0 && exportPwd !== exportPwd2 ? (
                  <p className="mt-1 text-[11px] text-red-500">两次输入的密码不一致</p>
                ) : null}
              </div>
            </>
          ) : null}
          <div className="flex justify-end gap-2 pt-1">
            <button className="btn-secondary" disabled={exportBusy} onClick={() => setExportOpen(false)}>
              取消
            </button>
            <button
              className={`btn-primary inline-flex items-center gap-1.5 ${exportWithCreds ? 'bg-red-600 hover:bg-red-700' : ''}`}
              disabled={exportBusy || (exportWithCreds && (!exportPwd.trim() || exportPwd !== exportPwd2))}
              onClick={() => void doExport()}
            >
              {exportBusy ? <Loader2 size={14} className="animate-spin" /> : <Download size={14} />}
              {exportWithCreds ? '我已知晓风险，加密导出' : '确认导出'}
            </button>
          </div>
        </div>
      </Modal>

      {/* 审查修复（第5项）：AIWQENC1 加密导出文件需密码解密——导入密码弹框 */}
      <Modal open={importEnvelope !== null} onClose={() => !importBusy && setImportEnvelope(null)} title="导入加密账号文件" widthClass="max-w-sm">
        <div className="space-y-3 text-sm">
          <div>该文件为加密导出（凭证已加密），请输入导出时设置的密码进行解密导入。</div>
          <div>
            <label className="label">导出密码</label>
            <input
              type="password"
              className="input"
              autoFocus
              value={importPwd}
              onChange={(e) => setImportPwd(e.target.value)}
              onKeyDown={(e) => {
                // 审查修复（第5项）：回车快捷提交，仅当密码非空且不处于忙态时允许
                if (e.key === 'Enter' && !importBusy && importPwd.trim()) void doImportEncrypted();
              }}
              placeholder="导出时设置的密码"
            />
          </div>
          <div className="flex justify-end gap-2 pt-1">
            <button className="btn-secondary" disabled={importBusy} onClick={() => setImportEnvelope(null)}>
              取消
            </button>
            <button
              className="btn-primary inline-flex items-center gap-1.5"
              disabled={importBusy || !importPwd.trim()}
              onClick={() => void doImportEncrypted()}
            >
              {importBusy ? <Loader2 size={14} className="animate-spin" /> : <Upload size={14} />} 解密并导入
            </button>
          </div>
        </div>
      </Modal>

      {/* 移除确认弹框（删除类操作二次确认） */}
      <Modal open={confirmTarget !== null} onClose={() => !confirmBusy && setConfirmTarget(null)} title="确认移除账号" widthClass="max-w-sm">
        {confirmTarget ? (
          <div className="space-y-3 text-sm">
            <p>
              {confirmTarget.kind === 'remove-account' ? (
                <>
                  确定移除国际账号 <b>{confirmTarget.account.nickname || confirmTarget.account.id}</b>？
                </>
              ) : (
                <>确定批量移除 <b>{confirmTarget.ids.length}</b> 个国际账号？</>
              )}
              将同时清除其凭证记录与 buddy-ai vault 条目，不可恢复。
            </p>
            <div className="flex justify-end gap-2">
              <button className="btn-secondary" disabled={confirmBusy} onClick={() => setConfirmTarget(null)}>
                取消
              </button>
              <button
                className="btn-primary inline-flex items-center gap-1.5 bg-red-600 hover:bg-red-700"
                disabled={confirmBusy}
                onClick={() => void doRemove()}
              >
                {confirmBusy ? <Loader2 size={14} className="animate-spin" /> : <Trash2 size={14} />} 确认移除
              </button>
            </div>
          </div>
        ) : null}
      </Modal>

      {/* 积分包明细弹窗（对齐国内积分包列内弹窗；数据源 get-user-resource 分包快照） */}
      <Modal open={pkgTarget !== null} onClose={() => setPkgTarget(null)} title={`积分包明细 · ${pkgTarget?.nickname || pkgTarget?.id || ''}`} widthClass="max-w-lg">
        {pkgTarget ? (
          <div className="space-y-3 text-sm">
            {(pkgTarget.credits_packages?.length ?? 0) === 0 ? (
              <p className="py-4 text-center text-xs text-slate-400">无积分包数据，请先到「积分看板」同步。</p>
            ) : (
              <table className="w-full text-xs">
                <thead>
                  <tr className="border-b border-slate-100 text-left text-slate-400 dark:border-zinc-800">
                    <th className="py-1.5 pr-2 font-medium">包名</th>
                    <th className="py-1.5 pr-2 text-right font-medium">剩余 / 总额</th>
                    <th className="py-1.5 text-right font-medium">到期时间</th>
                  </tr>
                </thead>
                <tbody>
                  {(pkgTarget.credits_packages ?? []).map((p, i) => {
                    /* 审查修复（U8）：补「未过期」判断——原实现已过期包（expire_secs
                       <= now）也命中 soon 被标「7 日内到期」，语义失真；对齐列表行
                       soonestExpire（L727 附近）口径：仅「未过期且 7 日内到期」标记 */
                    const nowSecs = Math.floor(Date.now() / 1000);
                    const soon =
                      p.expire_secs != null && p.expire_secs > nowSecs && p.expire_secs <= nowSecs + 7 * 86400;
                    return (
                      <tr key={i} className="border-b border-slate-50 last:border-0 dark:border-zinc-800/50">
                        <td className="py-1.5 pr-2">
                          {p.name || p.code}
                          {soon && p.remain > 0 ? <Badge tone="amber">7 日内到期</Badge> : null}
                        </td>
                        <td className="py-1.5 pr-2 text-right tabular-nums">
                          {p.remain.toFixed(2)} / {p.size.toFixed(2)}
                        </td>
                        <td className="py-1.5 text-right tabular-nums text-slate-500">
                          {p.expire_secs ? new Date(p.expire_secs * 1000).toLocaleDateString() : '—'}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            )}
            <div className="flex justify-end">
              <button className="btn-secondary" onClick={() => setPkgTarget(null)}>关闭</button>
            </div>
          </div>
        ) : null}
      </Modal>

      {/* 分组管理弹框（复用共享 GroupsModal；buddy-ai 分组走共享分组体系） */}
      <GroupsModal
        open={groupOpen}
        onClose={() => setGroupOpen(false)}
        groups={groups}
        onCreate={async (name, color) => {
          try {
            await api.groups.create(name, color);
            await refresh();
          } catch (err) {
            pushToast('error', `新建分组失败：${String(err)}`);
            throw err;
          }
        }}
        onRename={async (id, name) => {
          try {
            await api.groups.update(id, { name });
            await refresh();
          } catch (err) {
            pushToast('error', `重命名分组失败：${String(err)}`);
            throw err;
          }
        }}
        onRecolor={async (id, color) => {
          try {
            await api.groups.update(id, { color });
            await refresh();
          } catch (err) {
            pushToast('error', `修改分组颜色失败：${String(err)}`);
            throw err;
          }
        }}
        onDelete={async (id) => {
          try {
            await api.groups.remove(id);
            await refresh();
            pushToast('success', '分组已删除，组内账号回落未分组');
          } catch (err) {
            pushToast('error', `删除分组失败：${String(err)}`);
            throw err;
          }
        }}
      />

      {/* 共享标签管理 + 账号标签编辑（三池通用组件复用） */}
      <TagsModal
        open={tagsOpen}
        onClose={() => setTagsOpen(false)}
        tags={tagDefs}
        onCreate={async (name, color) => {
          try {
            await api.tags.create(name, color);
            await reloadTags();
          } catch (err) {
            pushToast('error', `创建标签失败：${String(err)}`);
            throw err;
          }
        }}
        onRename={async (id, name) => {
          try {
            await api.tags.update(id, { name });
            pushToast('success', `标签已重命名为「${name}」`);
            await reloadTags();
          } catch (err) {
            pushToast('error', `重命名标签失败：${String(err)}`);
            throw err;
          }
        }}
        onRecolor={async (id, color) => {
          try {
            await api.tags.update(id, { color });
            pushToast('success', '标签颜色已更新');
            await reloadTags();
          } catch (err) {
            pushToast('error', `修改标签颜色失败：${String(err)}`);
            throw err;
          }
        }}
        onDelete={async (id) => {
          try {
            await api.tags.remove(id);
            await reloadTags();
            await refresh();
            pushToast('success', '标签已删除，引用已清理');
          } catch (err) {
            pushToast('error', `删除标签失败：${String(err)}`);
            throw err;
          }
        }}
      />
      <AccountTagsEditModal
        open={tagEditTarget != null}
        onClose={() => setTagEditTarget(null)}
        accountName={tagEditTarget?.nickname || tagEditTarget?.id || ''}
        tags={tagDefs}
        selected={tagEditTarget ? (tagMap[tagEditTarget.id] ?? []) : []}
        onSave={async (ids) => {
          if (!tagEditTarget) return;
          try {
            await api.tags.setAccountTags(tagEditTarget.id, ids);
            await reloadTags();
            pushToast('success', '标签已更新');
          } catch (err) {
            pushToast('error', `标签保存失败：${String(err)}`);
            throw err;
          }
        }}
      />

      {/* 使用帮助弹框（国际版口径） */}
      <BuddyAiHelpModal open={helpOpen} onClose={() => setHelpOpen(false)} />

      {/* 扫描本机账号结果弹框（account-snapshot.json 识别 + 池内匹配；凭证在加密 vault 不可提取） */}
      <Modal open={scanOpen} onClose={() => setScanOpen(false)} title="扫描本机账号" widthClass="max-w-md">
        <div className="space-y-3 text-sm">
          {!scanResult?.found ? (
            <div className="text-slate-500">{String(scanResult?.reason ?? '未找到本机登录账号。')}</div>
          ) : (
            <>
              <div className="rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
                <div className="grid grid-cols-[84px_1fr] gap-y-1.5">
                  <span className="text-slate-400">昵称</span>
                  <span className="font-medium">{String(scanResult.nickname || '—')}</span>
                  <span className="text-slate-400">UID</span>
                  <span className="font-mono">{String(scanResult.uid || '—')}</span>
                  <span className="text-slate-400">本机套餐</span>
                  <span>
                    {String(scanResult.edition_type || '—')}
                    {scanResult.is_pro ? '（Pro）' : ''}
                  </span>
                </div>
              </div>
              {scanResult.in_pool ? (
                <div className="rounded-md bg-emerald-50 px-3 py-2 text-xs text-emerald-700 dark:bg-emerald-500/10 dark:text-emerald-300">
                  该账号已在 buddy-ai 池中（{String(scanResult.pool_account_id)}），无需重复导入。
                </div>
              ) : (
                <div className="rounded-md bg-amber-50 px-3 py-2 text-xs text-amber-700 dark:bg-amber-500/10 dark:text-amber-300">
                  本机登录账号尚未入池。国际版凭证存于客户端加密 vault（不可提取明文），
                  请在本机客户端保持该账号登录，然后在账号管理页点「OAuth登录」完成授权入池。
                </div>
              )}
            </>
          )}
          <div className="flex justify-end pt-1">
            <button className="btn-secondary" onClick={() => setScanOpen(false)}>
              关闭
            </button>
          </div>
        </div>
      </Modal>

      {/* 环境重置弹框（白名单目录清理；勾选「我已确认」才可执行——删除类操作二次确认） */}
      <Modal open={resetOpen} onClose={() => !resetBusy && setResetOpen(false)} title="BuddyAI 环境重置" widthClass="max-w-lg">
        <div className="space-y-3 text-sm">
          <div className="rounded-md bg-amber-50 px-3 py-2 text-xs text-amber-700 dark:bg-amber-500/10 dark:text-amber-300">
            将删除 WorkBuddy AI 国际客户端数据目录（~/.workbuddy-ai）下所选内容，
            操作不可恢复；建议先关闭客户端再执行。
          </div>
          <div className="max-h-64 space-y-1.5 overflow-y-auto rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            {RESET_ITEMS.map(([key, label]) => (
              <label key={key} className="flex cursor-pointer items-center gap-2 text-xs">
                <input
                  type="checkbox"
                  checked={resetItems.has(key)}
                  onChange={(e) =>
                    setResetItems((prev) => {
                      const next = new Set(prev);
                      if (e.target.checked) next.add(key);
                      else next.delete(key);
                      return next;
                    })
                  }
                />
                <span className={key === 'login' ? 'font-medium text-red-500' : ''}>{label}</span>
              </label>
            ))}
          </div>
          <label className="flex items-start gap-2 text-xs text-slate-500">
            <input type="checkbox" className="mt-0.5" checked={resetConfirm} onChange={(e) => setResetConfirm(e.target.checked)} />
            我已确认所选内容的删除风险（登录态清除后需重新登录客户端）。
          </label>
          <div className="flex justify-end gap-2 pt-1">
            <button className="btn-secondary" disabled={resetBusy} onClick={() => setResetOpen(false)}>
              取消
            </button>
            <button
              className="btn-primary inline-flex items-center gap-1.5 bg-red-600 hover:bg-red-700"
              disabled={resetBusy || resetItems.size === 0 || !resetConfirm}
              onClick={() => void doEnvReset()}
            >
              {resetBusy ? <Spinner /> : <ShieldAlert size={14} />} 执行重置（{resetItems.size} 项）
            </button>
          </div>
        </div>
      </Modal>
    </div>
  );
}
