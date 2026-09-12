import { useEffect, useMemo, useState } from 'react';
import { createPortal } from 'react-dom';
import {
  AppWindow,
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
  Snowflake,
  SquareTerminal,
  Tags,
  Trash2,
  Upload,
  Zap,
} from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Badge, EmptyState } from '../components/ui';
import { open, save } from '@tauri-apps/plugin-dialog';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import { withMinDelay } from '../lib/delay';
import type { AccountView, DiscoveredAccount, ImportPreview } from '../types';
import { AddAccountModal } from './accounts/AddAccountModal';
import { CooldownBadge } from './accounts/CooldownBadge';
import { CreditCell } from './accounts/CreditCell';
import { CreditsExpireBadge } from './accounts/CreditsExpireBadge';
import { DeleteAccountConfirmModal, DeleteSlotConfirmModal } from './accounts/DeleteConfirmModals';
import { DiscoverModal } from './accounts/DiscoverModal';
import { EditAccountModal } from './accounts/EditAccountModal';
import { GroupSelect } from './accounts/GroupSelect';
import { GroupsModal } from './accounts/GroupsModal';
import { HelpModal } from './accounts/HelpModal';
import { ImportPreviewModal } from './accounts/ImportPreviewModal';
import { JwtStatusBadge } from './accounts/JwtStatusBadge';
import { JwtViewModal } from './accounts/JwtViewModal';
import { OAuthLoginModal } from './accounts/OAuthLoginModal';
import { PayIdentityBadge } from './accounts/PayIdentityBadge';
import { ProfileModal } from './accounts/SnapshotModal';

export default function Accounts() {
  const accounts = useAppStore((s) => s.accounts);
  const groups = useAppStore((s) => s.groups);
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
  const profileApp = useAppStore((s) => s.profileApp);
  const setProfileApp = useAppStore((s) => s.setProfileApp);
  const profileBackup = useAppStore((s) => s.profileBackup);
  const profileRestore = useAppStore((s) => s.profileRestore);
  const profileDelete = useAppStore((s) => s.profileDelete);
  const oauthLogin = useAppStore((s) => s.oauthLogin);
  const refreshProfiles = useAppStore((s) => s.refreshProfiles);

  const [filter, setFilter] = useState<string>('all');
  const [addOpen, setAddOpen] = useState(false);
  const [groupOpen, setGroupOpen] = useState(false);
  const [editTarget, setEditTarget] = useState<AccountView | null>(null);
  // 双应用切换/保存菜单：{ userId, kind } —— kind='switch' 切换登录态 / 'save' 保存登录态
  const [appMenu, setAppMenu] = useState<{ userId: string; kind: 'switch' | 'save'; x: number; y: number } | null>(null);
  const [jwtTarget, setJwtTarget] = useState<AccountView | null>(null);
  const [profileOpen, setProfileOpen] = useState(false);
  const [oauthOpen, setOAuthOpen] = useState(false);
  const [helpOpen, setHelpOpen] = useState(false);
  // F-08 自动发现：扫描本机两个 Trae 应用 storage.json 的登录账号
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

  /** 刷新账号 + 套餐身份（先拉套餐缓存，再重建账号视图） */
  const refreshAccountsAndPay = async () => {
    try {
      await api.accounts.refreshPayStatus();
    } catch {
      /* 套餐刷新失败不阻断账号刷新 */
    }
    void refreshAccounts();
    void refreshGroups();
    void refreshRemainingCredits();
  };

  const filtered = useMemo(() => {
    if (filter === 'all') return accounts;
    if (filter === 'ungrouped') return accounts.filter((a) => !a.group_id);
    return accounts.filter((a) => a.group_id === filter);
  }, [accounts, filter]);

  useEffect(() => {
    void refreshAccounts();
    void refreshGroups();
  }, [refreshAccounts, refreshGroups]);

  const onDelete = async (a: AccountView) => {
    setDeleteTarget(a);
  };

  const confirmDelete = async () => {
    if (!deleteTarget) return;
    try {
      await withMinDelay(deleteAccount(deleteTarget.user_id, true));
    } finally {
      setDeleteTarget(null);
    }
  };

  const confirmDeleteSlot = async () => {
    if (!deleteSlot) return;
    try {
      await withMinDelay(profileDelete(deleteSlot));
    } finally {
      setDeleteSlot(null);
    }
  };

  const copyJwt = async (jwt: string) => {
    try {
      await navigator.clipboard.writeText(jwt);
      toast('success', 'JWT 已复制到剪贴板');
    } catch {
      toast('error', '复制失败，请手动选择文本复制');
    }
  };

  const exportAccounts = async () => {
    if (accounts.length === 0) {
      toast('warn', '没有账号可导出');
      return;
    }
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
        `导入完成：新增 ${report.added} 个账号，跳过 ${report.skipped} 个重复${report.groups_added ? `，新增分组 ${report.groups_added} 个` : ''}`,
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
        desc="维护账号、调整分组、重置设备 ID 与登录态切换"
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
            <button onClick={() => void refreshAccountsAndPay()} className="btn-outline" title="刷新账号列表、套餐与积分数据">
              <RefreshCw size={15} /> 刷新
            </button>
            <button onClick={() => void runDiscover()} className="btn-outline" title="扫描本机 Trae Work / Trae 已登录账号，一键加入账号池">
              <ScanSearch size={15} /> 扫描本机
            </button>
            <button onClick={() => setOAuthOpen(true)} className="btn-outline" title="通过 OAuth 授权登录添加账号">
              <Globe size={15} /> OAuth 登录
            </button>
            <button onClick={() => setAddOpen(true)} className="btn-outline" title="手动粘贴 JWT 添加账号">
              <Plus size={15} /> 添加账号
            </button>
            <button onClick={() => void exportAccounts()} className="btn-outline" title="导出所有账号为 JSON 文件">
              <Download size={15} /> 导出账户
            </button>
            <button
              onClick={() => void importAccounts()}
              disabled={importing}
              className={importing ? 'btn-outline cursor-not-allowed opacity-60' : 'btn-outline'}
              title="从导出的 JSON 文件导入账号（自动去重）"
            >
              {importing ? <Loader2 size={15} className="animate-spin" /> : <Upload size={15} />} 导入账号
            </button>
            <button onClick={() => setGroupOpen(true)} className="btn-outline" title="管理账号分组">
              <Tags size={15} /> 分组管理
            </button>
            <button onClick={() => { void refreshProfiles(); setProfileOpen(true); }} className="btn-outline" title="查看/备份/恢复登录态快照">
              <Camera size={15} /> 快照管理
            </button>
          </>
        }
      />

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
              hint="点击右上角「添加账号」粘贴 JWT，或先启动代理，在 Trae Work / Trae 中登录后自动捕获。"
            />
          </div>
        ) : (
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-left">分组</th>
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
                      <div className="flex items-center gap-1">
                        <JwtStatusBadge hours={a.jwt_exp_hours} />
                        {a.has_refresh_token && (
                          <span title="支持自动刷新" className="text-sky-500">
                            <Zap size={12} />
                          </span>
                        )}
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
                        <button title="编辑账号" onClick={() => setEditTarget(a)} className="btn-ghost !p-2">
                          <Pencil size={14} />
                        </button>
                        {a.cooldown_type && (
                          <button
                            title="解除冷却"
                            onClick={() => void cooldownClear(a.user_id)}
                            className="btn-ghost !p-2 text-sky-500 hover:bg-sky-50 dark:hover:bg-sky-500/10"
                          >
                            <Snowflake size={14} />
                          </button>
                        )}
                        {/* SessionDead（JWT 被服务端吊销）时 exp 往往未到，必须常显续期入口，
                            否则与签到/切换失败的「点续期 JWT」指引断链（issue #9 审查项） */}
                        {(a.jwt_exp_hours === null || a.jwt_exp_hours <= 24 || a.cooldown_type === 'SessionDead') && (
                          <button
                            title={switchingTo || savingLogin ? '切换/保存进行中，暂不能续期' : '续期 JWT（启动代理并切换账号）'}
                            onClick={() => void renewJwt(a.user_id)}
                            disabled={!!switchingTo || !!savingLogin}
                            className="btn-ghost !p-2 text-amber-500 hover:bg-amber-50 disabled:cursor-not-allowed disabled:opacity-40 dark:hover:bg-amber-500/10"
                          >
                            <KeyRound size={14} />
                          </button>
                        )}
                        {a.has_refresh_token && (
                          <button
                            title="刷新 JWT"
                            onClick={() => void refreshJwt(a.user_id)}
                            className="btn-ghost !p-2 text-sky-500 hover:bg-sky-50 dark:hover:bg-sky-500/10"
                          >
                            <Zap size={14} />
                          </button>
                        )}
                        <div className="relative flex items-center">
                          <button
                            title={switchingTo ? (switchingTo === a.user_id ? '切换中…' : '正在切换其他账号') : '切换此账号（选择目标应用）'}
                            onClick={(e) => {
                              const r = e.currentTarget.getBoundingClientRect();
                              setAppMenu(appMenu?.userId === a.user_id && appMenu.kind === 'switch' ? null : { userId: a.user_id, kind: 'switch', x: r.right, y: r.bottom });
                            }}
                            disabled={!!switchingTo || !!savingLogin}
                            className={`btn-ghost !p-2 ${switchingTo === a.user_id ? 'text-amber-500' : ''} ${(switchingTo && switchingTo !== a.user_id) || savingLogin ? 'opacity-40 cursor-not-allowed' : ''}`}
                          >
                            {switchingTo === a.user_id ? <Loader2 size={14} className="animate-spin" /> : <LogIn size={14} />}
                          </button>
                          <button
                            title={savingLogin ? (savingLogin === a.user_id ? '保存中…' : '正在保存其他账号') : '保存当前登录态（选择目标应用）'}
                            onClick={(e) => {
                              const r = e.currentTarget.getBoundingClientRect();
                              setAppMenu(appMenu?.userId === a.user_id && appMenu.kind === 'save' ? null : { userId: a.user_id, kind: 'save', x: r.right, y: r.bottom });
                            }}
                            disabled={!!switchingTo || !!savingLogin}
                            className={`btn-ghost !p-2 ${savingLogin === a.user_id ? 'text-amber-500' : ''} ${(savingLogin && savingLogin !== a.user_id) || switchingTo ? 'opacity-40 cursor-not-allowed' : ''}`}
                          >
                            {savingLogin === a.user_id ? <Loader2 size={14} className="animate-spin" /> : <Save size={14} />}
                          </button>
                        </div>
                        <button
                          title={switchingTo || savingLogin ? '切换/保存进行中，暂不能重置' : '重置设备 ID'}
                          onClick={() => void resetDevice(a.user_id)}
                          disabled={!!switchingTo || !!savingLogin}
                          className="btn-ghost !p-2 disabled:cursor-not-allowed disabled:opacity-40"
                        >
                          <RotateCcw size={14} />
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

      {/* 应用选择菜单：Portal + fixed 定位，避免被表格容器 overflow 裁剪或被后续行遮盖。
          issue #9 反馈：原来的窄下拉两项太小易点错，改为左右两块大按钮（带图标+描述），
          hover 用 amber 高亮让目标区域醒目不易误触 */}
      {appMenu &&
        !switchingTo &&
        !savingLogin &&
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
                { app: 'Trae', label: 'Trae CN', desc: 'Trae CN IDE 客户端', Icon: SquareTerminal },
                { app: 'TraeWork', label: 'TRAE SOLO CN', desc: 'Trae Work 桌面端', Icon: AppWindow },
              ] as const).map((opt) => (
                <button
                  key={opt.app}
                  onClick={() => {
                    const { userId, kind } = appMenu;
                    setAppMenu(null);
                    if (kind === 'switch') {
                      void switchTo(userId, opt.app);
                    } else {
                      void saveCurrentLogin(userId, opt.app);
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
      <ProfileModal
        open={profileOpen}
        onClose={() => setProfileOpen(false)}
        profiles={profiles}
        profileActive={profileActive}
        profileProgress={profileProgress}
        profileApp={profileApp}
        onSwitchApp={(app) => void setProfileApp(app)}
        onBackup={(slot) => void profileBackup(slot)}
        onRestore={(slot) => void profileRestore(slot)}
        onDelete={async (slot) => {
          setDeleteSlot(slot);
        }}
      />
      <DeleteAccountConfirmModal
        target={deleteTarget}
        onClose={() => setDeleteTarget(null)}
        onConfirm={() => void confirmDelete()}
      />
      <DeleteSlotConfirmModal
        slot={deleteSlot}
        onClose={() => setDeleteSlot(null)}
        onConfirm={() => void confirmDeleteSlot()}
      />
      <OAuthLoginModal
        open={oauthOpen}
        onClose={() => setOAuthOpen(false)}
        groups={groups}
        onLogin={async (callbackUrl, accountName, groupId) => {
          try {
            await oauthLogin(callbackUrl, accountName, groupId);
            setOAuthOpen(false);
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
