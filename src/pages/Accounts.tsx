import { useEffect, useMemo, useState } from 'react';
import {
  Camera,
  Download,
  Eye,
  Globe,
  HelpCircle,
  KeyRound,
  Loader2,
  LogIn,
  Pencil,
  Plus,
  RefreshCw,
  RotateCcw,
  Save,
  ScanSearch,
  ShieldAlert,
  Snowflake,
  Tag,
  Tags,
  Trash2,
  Upload,
  Zap,
} from 'lucide-react';
import PageHeader from '../components/PageHeader';
import SwitchProgressPanel from '../components/SwitchProgressPanel';
import { Badge, EmptyState, Modal } from '../components/ui';
import { open, save } from '@tauri-apps/plugin-dialog';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import { withMinDelay } from '../lib/delay';
import { copyText } from '../lib/clipboard';
import type { AccountView, DiscoveredAccount, ImportPreview, TagView } from '../types';
import { AddAccountModal } from './accounts/AddAccountModal';
import { CooldownBadge } from './accounts/CooldownBadge';
import { CreditCell } from './accounts/CreditCell';
import { CreditsExpireBadge } from './accounts/CreditsExpireBadge';
import { DeleteAccountConfirmModal, DeleteSlotConfirmModal } from './accounts/DeleteConfirmModals';
import { DiscoverModal } from './accounts/DiscoverModal';
import { EditAccountModal } from './accounts/EditAccountModal';
import { GroupSelect } from './accounts/GroupSelect';
import { GroupsModal, computePoolBadges } from './accounts/GroupsModal';
import { TagChips } from './accounts/TagSelect';
import { AccountTagsEditModal, TagsModal } from './accounts/TagsModal';
import { HelpModal } from './accounts/HelpModal';
import { ImportPreviewModal } from './accounts/ImportPreviewModal';
import { JwtStatusBadge } from './accounts/JwtStatusBadge';
import { JwtViewModal } from './accounts/JwtViewModal';
import { OAuthLoginModal } from './accounts/OAuthLoginModal';
import { PayIdentityBadge } from './accounts/PayIdentityBadge';
import { ProfileModal } from './accounts/SnapshotModal';
import { RefreshTokenBadge } from './accounts/RefreshTokenBadge';

export default function Accounts() {
  const accounts = useAppStore((s) => s.accounts);
  const groups = useAppStore((s) => s.groups);
  const localEntitlement = useAppStore((s) => s.localEntitlement);
  const refreshLocalEntitlement = useAppStore((s) => s.refreshLocalEntitlement);
  const refreshAccounts = useAppStore((s) => s.refreshAccounts);
  const refreshGroups = useAppStore((s) => s.refreshGroups);
  const addAccount = useAppStore((s) => s.addAccount);
  const deleteAccount = useAppStore((s) => s.deleteAccount);
  const updateAccount = useAppStore((s) => s.updateAccount);
  const createGroup = useAppStore((s) => s.createGroup);
  const updateGroup = useAppStore((s) => s.updateGroup);
  const removeGroup = useAppStore((s) => s.removeGroup);
  const moveAccount = useAppStore((s) => s.moveAccount);
  const resetDevice = useAppStore((s) => s.resetDevice);
  const switchTo = useAppStore((s) => s.switchTo);
  const switchingTo = useAppStore((s) => s.switchingTo);
  const saveCurrentLogin = useAppStore((s) => s.saveCurrentLogin);
  const savingLogin = useAppStore((s) => s.savingLogin);
  const renewJwt = useAppStore((s) => s.renewJwt);
  const refreshRemainingCredits = useAppStore((s) => s.refreshRemainingCredits);
  const cooldownClear = useAppStore((s) => s.cooldownClear);
  const refreshJwt = useAppStore((s) => s.refreshJwt);
  const toast = useAppStore((s) => s.pushToast);
  const profiles = useAppStore((s) => s.profiles);
  const profileProgress = useAppStore((s) => s.profileProgress);
  const profileActive = useAppStore((s) => s.profileActive);
  const profileBackup = useAppStore((s) => s.profileBackup);
  const profileRestore = useAppStore((s) => s.profileRestore);
  const profileDelete = useAppStore((s) => s.profileDelete);
  const oauthLogin = useAppStore((s) => s.oauthLogin);
  const refreshProfiles = useAppStore((s) => s.refreshProfiles);

  const [filter, setFilter] = useState<string>('all');
  const [addOpen, setAddOpen] = useState(false);
  const [groupOpen, setGroupOpen] = useState(false);
  // 共享标签体系（P1 需求③）：定义列表 + 全量 uid→tag_ids 映射 + 编辑弹框目标
  const [tagDefs, setTagDefs] = useState<TagView[]>([]);
  const [tagMap, setTagMap] = useState<Record<string, string[]>>({});
  const [tagsOpen, setTagsOpen] = useState(false);
  const [tagEditTarget, setTagEditTarget] = useState<AccountView | null>(null);
  // 池来源徽标（需求①可选增强）：groupId → 在哪些池有成员
  const [poolBadges, setPoolBadges] = useState<Record<string, string[]>>({});
  const [editTarget, setEditTarget] = useState<AccountView | null>(null);
  const [jwtTarget, setJwtTarget] = useState<AccountView | null>(null);
  const [profileOpen, setProfileOpen] = useState(false);
  const [oauthOpen, setOAuthOpen] = useState(false);
  // 行内「重新登录」目标账号（无/失效 refresh_token 的账号定向 OAuth）
  const [reloginTarget, setReloginTarget] = useState<AccountView | null>(null);
  const [helpOpen, setHelpOpen] = useState(false);
  // F-08 自动发现：扫描本机 Trae 应用 storage.json 的登录账号
  const [scanOpen, setScanOpen] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [discovered, setDiscovered] = useState<DiscoveredAccount[] | null>(null);
  // 正在入池的账号（per-item loading，防重复点击）
  const [addingUid, setAddingUid] = useState<string | null>(null);
  // 导入账号进行中（按钮 loading，防重复点击）
  const [importing, setImporting] = useState(false);
  // F-46 导入预览：文件内容 + 预览数据 + 勾选的账号下标
  const [importContent, setImportContent] = useState('');
  const [importPreviewData, setImportPreviewData] = useState<ImportPreview | null>(null);
  const [importSelected, setImportSelected] = useState<Set<number>>(new Set());
  // 删除确认（禁 window.confirm，红线）：删除账号 / 删除快照
  const [deleteTarget, setDeleteTarget] = useState<AccountView | null>(null);
  const [deleteSlot, setDeleteSlot] = useState<string | null>(null);
  // F9：三层防重入 + loading 态（对齐 Buddy 版）——顶部刷新 / 删除确认 / 行内操作
  //（刷新 JWT、解除冷却）：连点会逐账号重复打上游积分查询或重复触发确认
  const [refreshBusy, setRefreshBusy] = useState(false);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [rowOpUid, setRowOpUid] = useState<string | null>(null);
  // 导出凭证确认（审查 P0-2；禁 window.confirm，红线）：Trae 导出恒含明文 JWT / refreshToken
  const [exportConfirming, setExportConfirming] = useState(false);
  // 切换/保存 90s 看门狗（对齐 BuddyAccounts）：switch-done / save-login-done 事件异常缺失
  // （桥挂死/事件丢失/后台线程 panic）时 switchingTo/savingLogin 会永久非空——全部切换/保存/
  // 续期/重置按钮被禁用，用户感知为「点击切换账号无反应」。90s 后解除
  // 按钮互斥并清空 store 进行中状态（issue #44：仅页面级解锁会让 spinner/「切换中…」
  // 永久显示；事件迟到仍会正常提示结果）。
  const [lockTimedOut, setLockTimedOut] = useState(false);
  const clearSwitchLocks = useAppStore((s) => s.clearSwitchLocks);
  const busy = (!!switchingTo || !!savingLogin) && !lockTimedOut;
  useEffect(() => {
    if (!switchingTo && !savingLogin) {
      setLockTimedOut(false);
      return;
    }
    setLockTimedOut(false);
    const timer = setTimeout(() => {
      setLockTimedOut(true);
      clearSwitchLocks();
      toast('warn', '切换/保存超过 90 秒未收到完成事件，已解除按钮锁定；结果请以日志与列表状态为准');
    }, 90_000);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [switchingTo, savingLogin]);

  const runDiscover = async () => {
    setScanOpen(true);
    setScanning(true);
    try {
      const list = await api.accounts.discover();
      setDiscovered(list);
    } catch (err) {
      setDiscovered([]);
      toast('error', `扫描本机账号失败：${String(err)}`);
    } finally {
      setScanning(false);
    }
  };

  const addDiscovered = async (d: DiscoveredAccount) => {
    if (addingUid) return;
    setAddingUid(d.user_id);
    try {
      await api.accounts.addDiscovered(d.user_id, '', d.app, d.dc_uid, d.uid_confident);
      toast('success', `账号 ${d.user_id} 已加入账号池`);
      const list = await api.accounts.discover();
      setDiscovered(list);
      void refreshAccounts();
    } catch (err) {
      toast('error', `加入失败：${String(err)}`);
    } finally {
      setAddingUid(null);
    }
  };

  /** 刷新账号 + 套餐身份（先拉套餐缓存，再重建账号视图）。
   *  F9：防重入——连点会逐账号重复打上游积分查询，易频控 */
  const refreshAccountsAndPay = async () => {
    if (refreshBusy) return;
    setRefreshBusy(true);
    try {
      try {
        await api.accounts.refreshPayStatus();
      } catch {
        /* 套餐刷新失败不阻断账号刷新 */
      }
      void refreshAccounts();
      void refreshGroups();
      await refreshRemainingCredits();
    } finally {
      setRefreshBusy(false);
    }
  };

  const filtered = useMemo(() => {
    if (filter === 'all') return accounts;
    if (filter === 'ungrouped') return accounts.filter((a) => !a.group_id);
    return accounts.filter((a) => a.group_id === filter);
  }, [accounts, filter]);

  // 共享标签加载/维护（P1 需求③）
  const refreshTags = async () => {
    try {
      const [defs, map] = await Promise.all([api.tags.list(), api.tags.map()]);
      setTagDefs(defs);
      setTagMap(map);
    } catch (err) {
      toast('error', `标签加载失败：${String(err)}`);
    }
  };
  const saveAccountTags = async (uid: string, ids: string[]) => {
    try {
      await api.tags.setAccountTags(uid, ids);
      await refreshTags();
      await refreshAccounts();
      toast('success', '标签已更新');
    } catch (err) {
      toast('error', `标签保存失败：${String(err)}`);
      throw err;
    }
  };

  useEffect(() => {
    void refreshAccounts();
    void refreshGroups();
    void refreshLocalEntitlement();
    void refreshTags();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refreshAccounts, refreshGroups, refreshLocalEntitlement]);

  const onDelete = async (a: AccountView) => {
    setDeleteTarget(a);
  };

  const confirmDelete = async () => {
    if (!deleteTarget || deleteBusy) return;
    // F9：删除确认 1s withMinDelay 窗口可连点二次触发——busy 防重入
    setDeleteBusy(true);
    try {
      await withMinDelay(deleteAccount(deleteTarget.user_id, true));
    } finally {
      setDeleteBusy(false);
      setDeleteTarget(null);
    }
  };

  const confirmDeleteSlot = async () => {
    if (!deleteSlot || deleteBusy) return;
    setDeleteBusy(true);
    try {
      await withMinDelay(profileDelete(deleteSlot));
    } finally {
      setDeleteBusy(false);
      setDeleteSlot(null);
    }
  };

  /** F9：行内操作互斥（刷新 JWT / 解除冷却）——执行期间全表行内按钮禁用 + 当前行 spinner */
  const runRowOp = async (uid: string, op: () => Promise<void>) => {
    if (rowOpUid) return;
    setRowOpUid(uid);
    try {
      await op();
    } finally {
      setRowOpUid(null);
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
    setExportConfirming(false);
    try {
      const payload = await api.accounts.exportRaw();
      const content = JSON.stringify(payload, null, 2);
      const fileStamp = new Date().toISOString().slice(0, 19).replace(/[T:]/g, '-');
      const filePath = await save({
        defaultPath: `trae-accounts-${fileStamp}.json`,
        filters: [{ name: 'JSON', extensions: ['json'] }],
      });
      if (!filePath) return;
      await api.misc.writeTextFile(filePath, content);
      toast('success', `已导出 ${accounts.length} 个账号到 ${filePath}`);
    } catch (err) {
      toast('error', `导出失败：${String(err)}`);
    }
  };

  const importAccounts = async () => {
    if (importing) return;
    setImporting(true);
    try {
      const filePath = await open({
        multiple: false,
        filters: [{ name: 'JSON', extensions: ['json'] }],
      });
      if (!filePath || typeof filePath !== 'string') return;
      const content = await api.misc.readTextFile(filePath);
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
        `导入完成：新增 ${report.added} 个账号，跳过 ${report.skipped} 个重复${report.groups_added ? `，新增分组 ${report.groups_added} 个` : ''}${
          report.tags_restored || report.tags_ignored
            ? `，标签绑定恢复 ${report.tags_restored ?? 0} 条${report.tags_ignored ? `（未知标签忽略 ${report.tags_ignored}）` : ''}`
            : ''
        }`,
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
        desc="多账号入池 · 切换登录 · JWT 续期"
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
            <button
              onClick={() => void refreshAccountsAndPay()}
              disabled={refreshBusy}
              className={refreshBusy ? 'btn-outline cursor-not-allowed opacity-60' : 'btn-outline'}
              title="刷新账号列表、套餐与积分数据"
            >
              {refreshBusy ? <Loader2 size={15} className="animate-spin" /> : <RefreshCw size={15} />} 刷新
            </button>
            <button onClick={() => setOAuthOpen(true)} className="btn-outline" title="通过 OAuth 授权登录添加账号">
              <Globe size={15} /> OAuth 登录
            </button>
            <button onClick={() => void runDiscover()} className="btn-outline" title="扫描本机 Trae 已登录账号，一键加入账号池">
              <ScanSearch size={15} /> 扫描本机账号
            </button>
            <button onClick={() => setAddOpen(true)} className="btn-outline" title="手动粘贴 JWT 添加账号">
              <Plus size={15} /> 添加账号
            </button>
            <button onClick={() => exportAccounts()} className="btn-outline" title="导出所有账号为 JSON 文件">
              <Download size={15} /> 导出账号
            </button>
            <button
              onClick={() => void importAccounts()}
              disabled={importing}
              className={importing ? 'btn-outline cursor-not-allowed opacity-60' : 'btn-outline'}
              title="从导出的 JSON 文件导入账号（自动去重）"
            >
              {importing ? <Loader2 size={15} className="animate-spin" /> : <Upload size={15} />} 导入账号
            </button>
            <button
              onClick={() => {
                setGroupOpen(true);
                // 池来源徽标：共享分组三池成员并集按前缀统计（可选增强，失败静默）
                api.groups
                  .sharedList()
                  .then((defs) => setPoolBadges(computePoolBadges(defs)))
                  .catch(() => {});
              }}
              className="btn-outline"
              title="管理账号分组"
            >
              <Tags size={15} /> 分组管理
            </button>
            <button onClick={() => void refreshTags().then(() => setTagsOpen(true))} className="btn-outline" title="管理共享标签（三池通用）">
              <Tag size={15} /> 标签管理
            </button>
            <button onClick={() => { void refreshProfiles(); setProfileOpen(true); }} className="btn-outline" title="查看/备份/恢复登录态快照">
              <Camera size={15} /> 快照管理
            </button>
          </>
        }
      />

      {/* 切换/保存进度面板（issue #44 遗留项：含「续期 JWT」内的切换流程） */}
      <SwitchProgressPanel />

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
              hint="点击右上角「添加账号」粘贴 JWT，或先启动代理，在 Trae 中登录后自动捕获。"
            />
          </div>
        ) : (
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-left">分组</th>
                <th className="px-4 py-2 text-left">标签</th>
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
                        {/* 客户端当前登录标记：本机 storage.json/vscdb 推导的登录 uid 与账号池匹配 */}
                        {a.user_id && a.user_id === localEntitlement?.uid && (
                          <Badge tone="brand" title="当前 Trae 客户端登录的账号">
                            Trae 登录中
                          </Badge>
                        )}
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
                      <TagChips
                        tagIds={tagMap[a.user_id] ?? a.tags}
                        allTags={tagDefs}
                        onEdit={() => setTagEditTarget(a)}
                      />
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex items-center gap-1">
                        <JwtStatusBadge hours={a.jwt_exp_hours} />
                        {a.jwt_uid_mismatch && (
                          <Badge
                            tone="amber"
                            title={`该账号的 JWT 实际属于账号 ${a.jwt_uid ?? '?'}，与记录的账号 id ${a.user_id} 不一致（可能粘贴错 JWT 或被污染）。快照槽位按账号 id 命名，请编辑账号重新粘贴正确的 JWT，或删除该账号重新收录`}
                          >
                            JWT/id 不符
                          </Badge>
                        )}
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
                        <button
                          title={switchingTo ? (switchingTo === a.user_id ? '切换中…' : '正在切换其他账号') : '切换到此账号登录 Trae'}
                          onClick={() => void switchTo(a.user_id, 'Trae')}
                          disabled={busy}
                          className={`btn-ghost !p-2 ${switchingTo === a.user_id ? 'text-amber-500' : 'text-emerald-600 dark:text-emerald-400'} ${(switchingTo && switchingTo !== a.user_id) || savingLogin ? 'opacity-40 cursor-not-allowed' : ''}`}
                        >
                          {switchingTo === a.user_id ? <Loader2 size={14} className="animate-spin" /> : <LogIn size={14} />}
                        </button>
                        <button
                          title={savingLogin ? (savingLogin === a.user_id ? '保存中…' : '正在保存其他账号') : '保存当前登录态到该账号'}
                          onClick={() => void saveCurrentLogin(a.user_id, 'Trae')}
                          disabled={busy}
                          className={`btn-ghost !p-2 ${savingLogin === a.user_id ? 'text-amber-500' : ''} ${(savingLogin && savingLogin !== a.user_id) || switchingTo ? 'opacity-40 cursor-not-allowed' : ''}`}
                        >
                          {savingLogin === a.user_id ? <Loader2 size={14} className="animate-spin" /> : <Save size={14} />}
                        </button>
                        {/* SessionDead（JWT 被服务端吊销）时 exp 往往未到，必须常显续期入口，
                            否则与签到/切换失败的「点续期 JWT」指引断链（issue #9 审查项） */}
                        {(a.jwt_exp_hours === null || a.jwt_exp_hours <= 24 || a.cooldown_type === 'SessionDead') && (
                          <button
                            /* issue #126 语义澄清：本按钮是「被动捕获」，依赖 TRAE 重新鉴权才有新 JWT */
                            title={
                              switchingTo || savingLogin
                                ? '切换/保存进行中，暂不能续期'
                                : a.has_refresh_token
                                  ? '续期 JWT（启动代理并切换账号捕获；JWT 仍有效时抓不到新凭证，请改用「刷新 JWT」）'
                                  : '重新捕获 JWT（仅当 JWT 已失效、TRAE 重新鉴权时才能捕获新凭证；凭证仍有效请保持代理开启等待过期自动续期，或重新 OAuth 登录）'
                            }
                            onClick={() => void renewJwt(a.user_id)}
                            disabled={busy}
                            className="btn-ghost !p-2 text-amber-500 hover:bg-amber-50 disabled:cursor-not-allowed disabled:opacity-40 dark:hover:bg-amber-500/10"
                          >
                            <KeyRound size={14} />
                          </button>
                        )}
                        {a.has_refresh_token && (
                          <button
                            title={rowOpUid ? '行内操作进行中' : '刷新 JWT'}
                            onClick={() => void runRowOp(a.user_id, () => refreshJwt(a.user_id))}
                            disabled={rowOpUid != null}
                            className="btn-ghost !p-2 text-sky-500 hover:bg-sky-50 dark:hover:bg-sky-500/10 disabled:cursor-not-allowed disabled:opacity-40"
                          >
                            {rowOpUid === a.user_id ? <Loader2 size={14} className="animate-spin" /> : <Zap size={14} />}
                          </button>
                        )}
                        {/* 重新 OAuth 登录：无 refresh_token / RT 失效 / RT 已过期
                            的账号无法自动续期，定向重新授权更新凭证（后端按 uid 校验防登错）。
                            地球图标语义=打开浏览器授权；sky 配色在全部 6 个主题（含暗夜紫/
                            琥珀暖夜）上均有足够对比 */}
                        {(!a.has_refresh_token ||
                          a.refresh_token_invalid ||
                          (a.refresh_token_expires_at != null &&
                            a.refresh_token_expires_at > 0 &&
                            a.refresh_token_expires_at <= Math.floor(Date.now() / 1000))) && (
                          <button
                            title="重新 OAuth 登录（浏览器授权，为该账号重新获取 JWT / refresh_token）"
                            onClick={() => setReloginTarget(a)}
                            disabled={busy || rowOpUid != null}
                            className="btn-ghost !p-2 text-sky-600 hover:bg-sky-50 disabled:cursor-not-allowed disabled:opacity-40 dark:text-sky-400 dark:hover:bg-sky-500/10"
                          >
                            <Globe size={14} />
                          </button>
                        )}
                        {a.cooldown_type && (
                          <button
                            title={rowOpUid ? '行内操作进行中' : '解除冷却'}
                            onClick={() => void runRowOp(a.user_id, () => cooldownClear(a.user_id))}
                            disabled={rowOpUid != null}
                            className="btn-ghost !p-2 text-sky-500 hover:bg-sky-50 dark:hover:bg-sky-500/10 disabled:cursor-not-allowed disabled:opacity-40"
                          >
                            {rowOpUid === a.user_id ? <Loader2 size={14} className="animate-spin" /> : <Snowflake size={14} />}
                          </button>
                        )}
                        <button
                          title={switchingTo || savingLogin ? '切换/保存进行中，暂不能重置' : '重置设备 ID'}
                          onClick={() => void resetDevice(a.user_id)}
                          disabled={busy}
                          className="btn-ghost !p-2 disabled:cursor-not-allowed disabled:opacity-40"
                        >
                          <RotateCcw size={14} />
                        </button>
                        <button title="编辑账号" onClick={() => setEditTarget(a)} className="btn-ghost !p-2">
                          <Pencil size={14} />
                        </button>
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

      {/* 应用选择菜单已退役（TraeWork 并入 Trae 单客户端）：切换/保存按钮直接执行 */}

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
        poolBadges={poolBadges}
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
      <TagsModal
        open={tagsOpen}
        onClose={() => setTagsOpen(false)}
        tags={tagDefs}
        onCreate={async (name, color) => {
          try {
            await api.tags.create(name, color);
            await refreshTags();
          } catch (err) {
            // 审查 F5：异常兜底 toast（对齐 Buddy/Qoder 账号页），重抛由弹框保留输入便于重试
            toast('error', `创建标签失败：${String(err)}`);
            throw err;
          }
        }}
        onRename={async (id, name) => {
          try {
            await api.tags.update(id, { name });
            await refreshTags();
            toast('success', `标签已重命名为「${name}」`);
          } catch (err) {
            toast('error', `重命名标签失败：${String(err)}`);
            throw err;
          }
        }}
        onRecolor={async (id, color) => {
          try {
            await api.tags.update(id, { color });
            await refreshTags();
            toast('success', '标签颜色已更新');
          } catch (err) {
            toast('error', `修改标签颜色失败：${String(err)}`);
            throw err;
          }
        }}
        onDelete={async (id) => {
          try {
            await api.tags.remove(id);
            await refreshTags();
            toast('success', '标签已删除，引用已清理');
          } catch (err) {
            toast('error', `删除标签失败：${String(err)}`);
            throw err;
          }
        }}
      />
      <AccountTagsEditModal
        open={tagEditTarget != null}
        onClose={() => setTagEditTarget(null)}
        accountName={tagEditTarget?.name ?? ''}
        tags={tagDefs}
        selected={tagEditTarget ? (tagMap[tagEditTarget.user_id] ?? tagEditTarget.tags ?? []) : []}
        onSave={async (ids) => {
          if (!tagEditTarget) return;
          await saveAccountTags(tagEditTarget.user_id, ids);
        }}
      />
      <ProfileModal
        open={profileOpen}
        onClose={() => setProfileOpen(false)}
        profiles={profiles}
        profileActive={profileActive}
        profileProgress={profileProgress}
        onBackup={(slot) => void profileBackup(slot)}
        onRestore={(slot) => void profileRestore(slot)}
        onDelete={async (slot) => {
          setDeleteSlot(slot);
        }}
      />
      <DeleteAccountConfirmModal
        target={deleteTarget}
        busy={deleteBusy}
        onClose={() => setDeleteTarget(null)}
        onConfirm={() => void confirmDelete()}
      />
      <DeleteSlotConfirmModal
        slot={deleteSlot}
        busy={deleteBusy}
        onClose={() => setDeleteSlot(null)}
        onConfirm={() => void confirmDeleteSlot()}
      />
      {/* 导出凭证确认弹框（审查 P0-2；禁 window.confirm，红线） */}
      <Modal
        open={exportConfirming}
        onClose={() => setExportConfirming(false)}
        title="确认导出明文凭证"
        footer={
          <>
            <button className="btn-outline" onClick={() => setExportConfirming(false)}>取消</button>
            <button
              className="btn-primary !bg-rose-600 hover:!bg-rose-500"
              onClick={() => void doExportAccounts()}
            >
              我已知晓风险，继续导出
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
        open={oauthOpen || reloginTarget !== null}
        relogin={
          reloginTarget
            ? { userId: reloginTarget.user_id, accountName: reloginTarget.name }
            : null
        }
        onClose={() => {
          setOAuthOpen(false);
          setReloginTarget(null);
        }}
        groups={groups}
        onLogin={async (callbackUrl, accountName, groupId, expectUid) => {
          try {
            await oauthLogin(callbackUrl, accountName, groupId, expectUid);
            setOAuthOpen(false);
            setReloginTarget(null);
          } catch {
            /* toast 已发出 */
          }
        }}
      />
      <HelpModal open={helpOpen} onClose={() => setHelpOpen(false)} />
      <DiscoverModal
        open={scanOpen}
        scanning={scanning}
        discovered={discovered}
        addingUid={addingUid}
        onClose={() => setScanOpen(false)}
        onAdd={(d) => void addDiscovered(d)}
        onRescan={() => void runDiscover()}
      />
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
