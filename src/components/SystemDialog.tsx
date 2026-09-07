import { useEffect, useState } from 'react';
import { Settings2, ScrollText } from 'lucide-react';
import { Modal } from './ui';
import GeneralSettingsPanel from './GeneralSettingsPanel';
import Logs from '../pages/Logs';
import { cn } from '../lib/cn';

/**
 * 系统设置与系统日志弹框（侧边栏左下角系统图标入口）。
 * Tab 1 系统设置：外观 / 语言 / 通用与通知 / 代理配置。
 * Tab 2 系统日志：复用系统日志页面（运行日志 / 代理日志 / API 请求日志）。
 */
export default function SystemDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [tab, setTab] = useState<'settings' | 'logs'>('settings');

  // 每次打开重置到第一个 tab
  useEffect(() => {
    if (open) setTab('settings');
  }, [open]);

  const TABS = [
    { key: 'settings' as const, label: '系统设置', icon: Settings2 },
    { key: 'logs' as const, label: '系统日志', icon: ScrollText },
  ];

  return (
    <Modal open={open} onClose={onClose} title="系统设置与系统日志" size="xl">
      <div className="flex h-[72vh] flex-col">
        {/* Tab 切换 — 分段控件风格 */}
        <div className="mb-3 inline-flex shrink-0 items-center gap-1 self-start rounded-xl border border-slate-200/80 bg-slate-50/80 p-1 dark:border-zinc-700/60 dark:bg-zinc-800/40">
          {TABS.map(({ key, label, icon: Icon }) => (
            <button
              key={key}
              onClick={() => setTab(key)}
              className={cn(
                'flex items-center gap-1.5 rounded-lg px-3.5 py-1.5 text-sm font-medium transition-all duration-200',
                tab === key
                  ? 'bg-white text-zinc-800 shadow-soft dark:bg-zinc-700 dark:text-zinc-50'
                  : 'text-slate-500 hover:text-slate-700 dark:text-zinc-400 dark:hover:text-zinc-200',
              )}
            >
              <Icon size={15} />
              {label}
            </button>
          ))}
        </div>

        <div className="flex min-h-0 flex-1 flex-col">
          {tab === 'settings' ? (
            <div className="min-h-0 flex-1 overflow-auto pr-1">
              <GeneralSettingsPanel />
            </div>
          ) : (
            <Logs embedded />
          )}
        </div>
      </div>
    </Modal>
  );
}
