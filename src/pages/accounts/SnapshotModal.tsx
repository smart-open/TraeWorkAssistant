import { useEffect, useState } from 'react';
import { ArchiveRestore, DatabaseBackup, Trash2 } from 'lucide-react';
import { Modal } from '../../components/ui';
import { api } from '../../lib/tauri';
import { cn } from '../../lib/cn';
import type { ProfileInfo } from '../../types';

function ProfileSize({ bytes }: { bytes: number }) {
  const [text, setText] = useState('');
  useEffect(() => {
    let cancel = false;
    api.profiles
      .formatSize(bytes)
      .then((t) => {
        if (!cancel) setText(t);
      })
      .catch(() => {
        if (!cancel) setText(`${bytes} B`);
      });
    return () => {
      cancel = true;
    };
  }, [bytes]);
  return <span>{text || '...'}</span>;
}

/** 登录态快照管理（原 ProfileModal）：查看/备份/恢复快照，删除确认由页面 DeleteSlotConfirmModal 承担 */
export function ProfileModal({
  open,
  onClose,
  profiles,
  profileActive,
  profileProgress,
  profileApp,
  onSwitchApp,
  onBackup,
  onRestore,
  onDelete,
}: {
  open: boolean;
  onClose: () => void;
  profiles: ProfileInfo[];
  profileActive: boolean;
  profileProgress: string[];
  profileApp: 'TraeWork' | 'Trae';
  onSwitchApp: (app: 'TraeWork' | 'Trae') => void;
  onBackup: (slot: string) => void;
  onRestore: (slot: string) => void;
  onDelete: (slot: string) => Promise<void>;
}) {
  return (
    <Modal open={open} onClose={onClose} title="登录态快照管理" size="xl">
      <div className="space-y-3">
        {/* 目标应用切换：快照按应用隔离存储（profiles / profiles_trae），F-03 参数化 */}
        <div className="flex items-center gap-2">
          <span className="text-xs text-slate-500 dark:text-zinc-400">目标应用</span>
          <div className="flex overflow-hidden rounded-lg border border-slate-200 dark:border-zinc-700">
            {([
              { app: 'TraeWork', label: 'Trae Work' },
              { app: 'Trae', label: 'Trae CN' },
            ] as const).map((opt) => (
              <button
                key={opt.app}
                disabled={profileActive}
                onClick={() => {
                  if (opt.app !== profileApp) onSwitchApp(opt.app);
                }}
                className={cn(
                  'px-3 py-1 text-xs transition',
                  profileApp === opt.app
                    ? 'bg-brand-600 text-white'
                    : 'bg-white text-slate-600 hover:bg-slate-50 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700',
                )}
              >
                {opt.label}
              </button>
            ))}
          </div>
        </div>
        {profileActive && (
          <div className="rounded-lg border border-brand-300 bg-brand-50 p-3 dark:border-brand-700 dark:bg-brand-900/20">
            <div className="mb-1 text-xs font-medium text-brand-700 dark:text-brand-300">
              正在处理...
            </div>
            <div className="max-h-32 space-y-0.5 overflow-auto font-mono text-xs text-brand-600 dark:text-brand-400">
              {profileProgress.length === 0 ? (
                <div>等待中...</div>
              ) : (
                profileProgress.map((line, i) => <div key={i}>{line}</div>)
              )}
            </div>
          </div>
        )}
        {profiles.length === 0 ? (
          <div className="py-6 text-center text-xs text-slate-400">暂无快照</div>
        ) : (
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="whitespace-nowrap px-3 py-2 text-left">账号 (user_id)</th>
                <th className="whitespace-nowrap px-3 py-2 text-right">文件数</th>
                <th className="whitespace-nowrap px-3 py-2 text-right">大小</th>
                <th className="whitespace-nowrap px-3 py-2 text-left">最后修改</th>
                <th className="whitespace-nowrap px-3 py-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              {profiles.map((p) => (
                <tr
                  key={p.slot}
                  className="row-hover border-t border-slate-200 dark:border-zinc-800"
                >
                  <td className="whitespace-nowrap px-3 py-2 font-mono text-xs">{p.slot}</td>
                  <td className="whitespace-nowrap px-3 py-2 text-right tabular-nums">{p.file_count}</td>
                  <td className="whitespace-nowrap px-3 py-2 text-right tabular-nums">
                    <ProfileSize bytes={p.size_bytes} />
                  </td>
                  <td className="whitespace-nowrap px-3 py-2 text-xs text-slate-500">
                    {p.last_modified || '-'}
                  </td>
                  <td className="whitespace-nowrap px-3 py-2">
                    <div className="flex justify-end gap-1">
                      <button
                        title="备份（将当前登录态备份到该槽位）"
                        onClick={() => onBackup(p.slot)}
                        disabled={profileActive}
                        className="btn-ghost !p-2"
                      >
                        <DatabaseBackup size={14} />
                      </button>
                      <button
                        title="恢复（将该槽位快照恢复到客户端）"
                        onClick={() => onRestore(p.slot)}
                        disabled={profileActive}
                        className="btn-ghost !p-2 text-sky-500 hover:bg-sky-50 dark:hover:bg-sky-500/10"
                      >
                        <ArchiveRestore size={14} />
                      </button>
                      <button
                        title="删除"
                        onClick={() => void onDelete(p.slot)}
                        disabled={profileActive}
                        className="btn-ghost !p-2 text-rose-500 hover:bg-rose-50 dark:hover:bg-rose-500/10"
                      >
                        <Trash2 size={14} />
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <div className="text-xs text-slate-400">
          快照在切换账号时自动备份，也可手动管理
        </div>
      </div>
    </Modal>
  );
}
