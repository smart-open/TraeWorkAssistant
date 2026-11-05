import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
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
} from 'lucide-react';
import { listen } from '../../lib/tauri';
import { Badge, EmptyState, Modal, Spinner } from '../../components/ui';
import PageHeader from '../../components/PageHeader';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import { GroupsModal } from '../accounts/GroupsModal';
import { TagChips } from '../accounts/TagSelect';
import { AccountTagsEditModal, TagsModal } from '../accounts/TagsModal';
import { PoolStatusBadge } from '../accounts/PoolStatusBadge';
import { usePoolStatus } from '../../hooks/usePoolStatus';
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
 *
 * Docker 适配（移植自 main）：桌面专属能力已裁剪——
 * 切换/保存登录态（api.switchAccount/saveCurrentLogin，客户端快照语义）、
 * 扫描本机账号（buddy_ai_local_scan）、环境重置（buddy_ai_env_reset）、
 * is_current_* 当前端徽标（依赖客户端 auth/live 文件 uid 探测）。
 */

type ConfirmTarget =
  | { kind: 'remove-account'; account: BuddyAiAccountView }
  | { kind: 'remove-batch'; ids: string[] }
  | null;

export default function BuddyAiAccounts() {
  const pushToast = useAppStore((s) => s.pushToast);
  // 运行期限流/冷却徽标（issue #178）：429 软冷却/积分耗尽硬冷却只在网关内存池，联表注入
  const poolStatus = usePoolStatus('buddyai');
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
  // 导出密码（含凭证导出必填，两次输入一致；关闭/成功后清空，不留在内存与 DOM）
  const [exportPwd, setExportPwd] = useState('');
  const [exportPwd2, setExportPwd2] = useState('');
  // 加密导出文件的导入口令（检测到 AIWQENC1 信封时弹框索取）
  const [importPwdOpen, setImportPwdOpen] = useState(false);
  const [importPwd, setImportPwd] = useState('');
  const [importPwdBusy, setImportPwdBusy] = useState(false);
  const [pendingImport, setPendingImport] = useState<Record<string, unknown> | null>(null);
  // 帮助 / 分组管理 / 标签管理弹框（对齐国内 BuddyAccounts）
  const [helpOpen, setHelpOpen] = useState(false);
  const [groupOpen, setGroupOpen] = useState(false);
  const [tagsOpen, setTagsOpen] = useState(false);
  // 导入账号（docker：浏览器 file input，无桌面扫描/环境重置）
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
  // Docker 裁剪：api.switchAccount/saveCurrentLogin 为桌面客户端快照语义
  //（关闭客户端 → 快照 → 恢复），Web 服务端无此通道；事件监听与看门狗一并移除。

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

  // 导出账号池（P8 对齐国内 F-46：默认脱敏）。
  // 审查修复（第二轮）：含凭证导出改为**口令加密信封**——原实现导出明文
  // access_token/refresh_token（文件等同密码），与 Qoder 侧「含凭证必须设密码」口径不一致；
  // 不含凭证的元数据导出保持明文、无需密码。
  // Docker 适配：无桌面 save 对话框，改为 Blob + a.download 浏览器下载（对齐国内 BuddyAccounts）
  const exportPwdReady = exportPwd.trim().length > 0 && exportPwd === exportPwd2;
  const doExport = async () => {
    if (exportWithCreds && !exportPwdReady) {
      pushToast('warn', '请先设置导出密码（两次输入需一致）');
      return;
    }
    setExportBusy(true);
    try {
      const payload = await api.buddyai.accountsExport(
        exportWithCreds,
        exportWithCreds ? exportPwd || undefined : undefined,
      );
      // FL-5：导出计数取自导出载荷（页面快照可能已过期；加密信封无 accounts 字段时回落 0）
      const exportedCount = Array.isArray(payload.accounts) ? payload.accounts.length : 0;
      const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `buddy-ai-accounts-${new Date().toISOString().slice(0, 10)}.json`;
      a.click();
      URL.revokeObjectURL(url);
      pushToast(
        'success',
        exportWithCreds
          ? `已导出 ${exportedCount} 个账号（含凭证，已用导出密码加密；导入时需输入同一密码）`
          : `已导出 ${exportedCount} 个账号（不含凭证）`,
      );
      setExportOpen(false);
      setExportPwd('');
      setExportPwd2('');
    } catch (err) {
      pushToast('error', `导出失败：${String(err).slice(0, 160)}`);
    } finally {
      setExportBusy(false);
    }
  };

  /** 扫描本机账号（buddy_ai_local_scan）：桌面专属，Docker 无客户端不移植 */

  /** 导入账号 JSON（accounts_export 回导；合并语义：同 id 跳过不覆盖） */
  const runImport = async (parsed: Record<string, unknown>, password?: string) => {
    try {
      const r = await api.buddyai.accountsImport(parsed, password);
      pushToast(
        'success',
        `导入完成：新增 ${r.added} · 跳过 ${r.skipped}（已存在）· 无效 ${r.invalid} · 凭证回灌 ${r.tokens_added}`,
      );
      await refresh();
      return true;
    } catch (err) {
      pushToast('error', `导入失败：${String(err).slice(0, 160)}`);
      return false;
    }
  };

  const doImport = async (file: File) => {
    let parsed: Record<string, unknown>;
    try {
      parsed = JSON.parse(await file.text()) as Record<string, unknown>;
    } catch {
      pushToast('error', '导入失败：文件不是合法 JSON');
      return;
    }
    // 加密信封（AIWQENC1，encrypted=true 且载荷在 data 字段）→ 先索取导出密码
    // （审查修复第二轮：含凭证导出已加密，导入侧需提供口令；明文旧文件不受影响）
    if (parsed.encrypted === true && typeof parsed.data === 'string') {
      setPendingImport(parsed);
      setImportPwd('');
      setImportPwdOpen(true);
      return;
    }
    await runImport(parsed);
  };

  const confirmImportPwd = async () => {
    if (!pendingImport) return;
    if (!importPwd.trim()) {
      pushToast('warn', '请输入导出时设置的密码');
      return;
    }
    setImportPwdBusy(true);
    const ok = await runImport(pendingImport, importPwd);
    setImportPwdBusy(false);
    if (ok) {
      setImportPwdOpen(false);
      setPendingImport(null);
      setImportPwd('');
    }
  };

  /** 环境重置（buddy_ai_env_reset）：桌面专属（清理本机 ~/.workbuddy-ai），Docker 不移植 */

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
            <button
              className="btn-outline"
              onClick={() => setExportOpen(true)}
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

      {/* 切换/保存进度面板 + 目标端菜单：桌面专属（客户端快照语义），Docker 裁剪 */}

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
            /* Docker 适配：桌面版「扫描本机账号」入口已裁剪 */
            hint="点击「OAuth登录」完成 workbuddy.ai 授权自动入池；也可「导入账号」导入账号池 JSON"
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
                      {/* 运行期限流/冷却（issue #178）：网关内存池状态，无则不渲染。
                          池条目以内部账号 id（wb-<hash>）为键——buddy_ai_upstream_accounts
                          的 uid=a.id，而非真实用户 uuid（View.uid），勿用 a.uid 联表 */}
                      <PoolStatusBadge status={poolStatus.get(a.id)} />
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
                      {/* 操作列：续期 → 编辑 → 删除（切换/保存登录态为桌面客户端快照语义，Docker 裁剪） */}
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
            <div className="space-y-2">
              {/* 审查修复（第二轮）：凭证以口令加密信封导出，不再是明文副本 */}
              <div className="rounded-md bg-amber-50 px-3 py-2 text-[11px] leading-4 text-amber-700 dark:bg-amber-500/10 dark:text-amber-300">
                勾选后凭证以 AES-256-GCM 加密写入文件（导入需同一密码）。请设置导出密码并妥善保管——密码丢失无法导入。
              </div>
              <input
                type="password"
                className="input w-full"
                placeholder="导出密码"
                value={exportPwd}
                onChange={(e) => setExportPwd(e.target.value)}
                autoComplete="new-password"
              />
              <input
                type="password"
                className="input w-full"
                placeholder="再次输入导出密码"
                value={exportPwd2}
                onChange={(e) => setExportPwd2(e.target.value)}
                autoComplete="new-password"
              />
              {exportPwd && exportPwd2 && !exportPwdReady ? (
                <div className="text-[11px] text-red-600 dark:text-red-400">两次输入的密码不一致</div>
              ) : null}
            </div>
          ) : null}
          <div className="flex justify-end gap-2 pt-1">
            <button className="btn-secondary" disabled={exportBusy} onClick={() => setExportOpen(false)}>
              取消
            </button>
            <button
              className={`btn-primary inline-flex items-center gap-1.5 ${exportWithCreds ? 'bg-red-600 hover:bg-red-700' : ''}`}
              disabled={exportBusy || (exportWithCreds && !exportPwdReady)}
              onClick={() => void doExport()}
            >
              {exportBusy ? <Loader2 size={14} className="animate-spin" /> : <Download size={14} />}
              {exportWithCreds ? '加密并导出' : '确认导出'}
            </button>
          </div>
        </div>
      </Modal>

      {/* 加密导出文件的导入口令（审查修复第二轮：含凭证导出已加密，导入需同一密码） */}
      <Modal
        open={importPwdOpen}
        onClose={() => !importPwdBusy && setImportPwdOpen(false)}
        title="该导出文件已加密"
        widthClass="max-w-sm"
      >
        <div className="space-y-3 text-sm">
          <div className="text-xs text-slate-500 dark:text-zinc-400">
            请输入导出时设置的密码（凭证以 AES-256-GCM 加密，密码错误将无法导入）。
          </div>
          <input
            type="password"
            className="input w-full"
            placeholder="导出密码"
            value={importPwd}
            onChange={(e) => setImportPwd(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !importPwdBusy) void confirmImportPwd();
            }}
            autoComplete="current-password"
          />
          <div className="flex justify-end gap-2 pt-1">
            <button className="btn-secondary" disabled={importPwdBusy} onClick={() => setImportPwdOpen(false)}>
              取消
            </button>
            <button
              className="btn-primary inline-flex items-center gap-1.5"
              disabled={importPwdBusy || !importPwd.trim()}
              onClick={() => void confirmImportPwd()}
            >
              {importPwdBusy ? <Loader2 size={14} className="animate-spin" /> : <Upload size={14} />}
              解密并导入
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
    </div>
  );
}
