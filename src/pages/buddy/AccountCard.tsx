import { MoreHorizontal, Pencil, Trash2, CheckCircle2, RefreshCw, TerminalSquare, DatabaseBackup, ArchiveRestore, Copy } from 'lucide-react';
import { Badge, Progress } from '../../components/ui';
import { cn } from '../../lib/cn';
import type { WorkBuddyAccountView, WbCreditPackage } from '../../types';

/**
 * F-54 账号卡片（双态）：当前账号高亮 + 对勾徽标；备用账号一键「设为当前」。
 * 版式对齐 workbuddy-product-design.md §3.7.2 与豆包/既有组件体系（零新增依赖）。
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

function tokenTone(ts: number | null): 'green' | 'amber' | 'red' | 'slate' {
  if (ts == null) return 'slate';
  const days = (ts * 1000 - Date.now()) / 86400000;
  if (days < 0) return 'red';
  if (days < 1) return 'amber';
  return 'green';
}

export default function AccountCard({
  account,
  packages,
  switching,
  onSwitch,
  onSaveLogin,
  onRefreshToken,
  onSetCli,
  onBackupChats,
  onRestoreChats,
  onCopyChats,
  onEdit,
  onDelete,
  onViewPackages,
}: {
  account: WorkBuddyAccountView;
  packages: WbCreditPackage[];
  switching: boolean;
  onSwitch: () => void;
  onSaveLogin: () => void;
  onRefreshToken: () => void;
  onSetCli: () => void;
  onBackupChats: () => void;
  onRestoreChats: () => void;
  onCopyChats: () => void;
  onEdit: () => void;
  onDelete: () => void;
  onViewPackages: () => void;
}) {
  const top2 = packages.slice(0, 2);
  return (
    <div
      className={cn(
        'card relative overflow-hidden p-4 transition',
        account.is_current && 'ring-2 ring-emerald-500/60 dark:ring-emerald-400/50',
        account.needs_relogin && 'ring-2 ring-rose-500/60 dark:ring-rose-400/50',
      )}
    >
      {/* 头部：昵称 + 徽标 + 菜单 */}
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="truncate text-sm font-semibold text-slate-800 dark:text-zinc-100">
              {account.nickname || '(未命名)'}
            </span>
            {account.is_current && (
              <span className="flex items-center gap-0.5 rounded-full bg-emerald-100 px-1.5 py-0.5 text-[11px] font-medium text-emerald-700 dark:bg-emerald-500/15 dark:text-emerald-300">
                <CheckCircle2 size={11} /> 当前
              </span>
            )}
            {account.edition_type && (
              <Badge tone={account.edition_type.toLowerCase() === 'pro' ? 'blue' : 'slate'}>
                {account.edition_type}
              </Badge>
            )}
          </div>
          <div className="mt-0.5 font-mono text-xs text-slate-400">{maskUid(account.uid)}</div>
        </div>
        <div className="group relative shrink-0">
          <button className="btn-ghost h-7 w-7 !p-0" aria-label="更多操作">
            <MoreHorizontal size={15} />
          </button>
          <div className="invisible absolute right-0 z-20 mt-1 w-32 rounded-lg border border-slate-200 bg-white py-1 opacity-0 shadow-lg transition group-hover:visible group-hover:opacity-100 dark:border-zinc-700 dark:bg-zinc-900">
            <button
              onClick={onEdit}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs hover:bg-slate-50 dark:hover:bg-zinc-800"
            >
              <Pencil size={13} /> 编辑信息
            </button>
            <button
              onClick={onRefreshToken}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs hover:bg-slate-50 dark:hover:bg-zinc-800"
            >
              <RefreshCw size={13} /> 续期凭证
            </button>
            <button
              onClick={onSetCli}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs hover:bg-slate-50 dark:hover:bg-zinc-800"
              title="写入 ~/.codebuddy/settings.json 供 CodeBuddy CLI 使用"
            >
              <TerminalSquare size={13} /> 设为 CLI 账号
            </button>
            <button
              onClick={onBackupChats}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs hover:bg-slate-50 dark:hover:bg-zinc-800"
              title="会话三件套：正文 jsonl + workbuddy.db + 云端映射 db"
            >
              <DatabaseBackup size={13} /> 备份会话
            </button>
            <button
              onClick={onRestoreChats}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs hover:bg-slate-50 dark:hover:bg-zinc-800"
              title="恢复会话三件套到 ~/.workbuddy（覆盖现有数据）"
            >
              <ArchiveRestore size={13} /> 恢复会话
            </button>
            <button
              onClick={onCopyChats}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs hover:bg-slate-50 dark:hover:bg-zinc-800"
              title="以新会话 id 复制到目标账号（含云端映射注册）"
            >
              <Copy size={13} /> 复制会话到…
            </button>
            <button
              onClick={onDelete}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-rose-600 hover:bg-rose-50 dark:text-rose-400 dark:hover:bg-rose-500/10"
            >
              <Trash2 size={13} /> 删除账号
            </button>
          </div>
        </div>
      </div>

      {/* 状态徽标行 */}
      <div className="mt-2 flex flex-wrap gap-1.5">
        {account.is_current ? (
          <Badge tone="green">在线</Badge>
        ) : (
          <Badge tone="slate">离线</Badge>
        )}
        {account.needs_relogin && <Badge tone="red">需重新登录</Badge>}
        <Badge tone={tokenTone(account.access_token_expires_at)}>
          {account.access_token_expires_at
            ? `token ${fmtExpire(account.access_token_expires_at)}`
            : 'token 期限未知'}
        </Badge>
        {!account.has_credential && <Badge tone="amber">无凭证副本</Badge>}
        {!account.has_snapshot && <Badge tone="amber">无快照</Badge>}
      </div>

      {/* 余额大字 */}
      <div className="mt-3 flex items-end gap-2">
        <span className="text-2xl font-semibold tabular-nums text-slate-800 dark:text-zinc-100">
          {account.credits_balance != null ? account.credits_balance.toFixed(2) : '—'}
        </span>
        <span className="pb-0.5 text-xs text-slate-400">
          {packages.length > 0 ? `${packages.length} 个积分包` : '暂无积分包明细'}
          {account.credits_fetched_at ? ` · ${account.credits_fetched_at.slice(11)} 更新` : ''}
        </span>
      </div>

      {/* 活跃明细：按到期升序取前 2 个包 + 查看全部入口 */}
      {packages.length > 0 && (
        <div className="mt-3 border-t border-slate-100 pt-3 dark:border-zinc-800">
          <div className="mb-1.5 text-xs font-medium text-slate-500">活跃明细</div>
          <div className="space-y-2">
            {top2.map((p, i) => (
              <div key={i}>
                <div className="flex items-center justify-between text-xs">
                  <span className="truncate text-slate-600 dark:text-zinc-300">
                    {p.remaining.toFixed(2)} 积分 · {p.name}
                  </span>
                  <span className={p.expire_soon ? 'shrink-0 pl-2 text-rose-500' : 'shrink-0 pl-2 text-slate-400'}>
                    {p.end_time ? `${p.end_time.slice(5, 10)} 到期` : ''}
                  </span>
                </div>
                <Progress value={p.remaining} max={p.total || 1} className="mt-1 h-1.5" />
              </div>
            ))}
          </div>
          {packages.length > 2 && (
            <button
              onClick={onViewPackages}
              className="mt-2 text-xs text-brand-600 hover:underline dark:text-brand-400"
            >
              查看全部积分包 →
            </button>
          )}
        </div>
      )}

      {/* 操作区：双态 */}
      <div className="mt-4 flex gap-2">
        {account.is_current ? (
          <button className="btn-outline flex-1 justify-center" onClick={onSaveLogin} disabled={switching}>
            {switching ? '处理中…' : '保存当前登录态'}
          </button>
        ) : (
          <>
            <button className="btn-primary flex-1 justify-center" onClick={onSwitch} disabled={switching}>
              {switching ? '切换中…' : '设为当前'}
            </button>
            <button className="btn-outline flex-1 justify-center" onClick={onSaveLogin} disabled={switching}>
              保存登录态
            </button>
          </>
        )}
      </div>
    </div>
  );
}
