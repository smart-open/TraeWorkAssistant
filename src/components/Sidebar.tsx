import { useState } from 'react';
import {
  LayoutDashboard,
  Users,
  PlayCircle,
  Coins,
  ScrollText,
  Settings,
  Server,
  Github,
  Globe,
  Palette,
  Info,
} from 'lucide-react';
import { open } from '@tauri-apps/plugin-shell';
import { useAppStore } from '../store';
import { cn } from '../lib/cn';
import { THEMES, nextTheme } from '../lib/themes';
import { LINK_REPO, LINK_BLOG } from '../lib/about';
import type { ViewKey } from '../types';
import AboutDialog from './AboutDialog';

export type { ViewKey };

const NAV: { key: ViewKey; label: string; icon: typeof Users }[] = [
  { key: 'dashboard', label: '概览', icon: LayoutDashboard },
  { key: 'accounts', label: '账号管理', icon: Users },
  { key: 'checkin', label: '一键签到', icon: PlayCircle },
  { key: 'credits', label: '积分看板', icon: Coins },
  { key: 'api-service', label: 'API 服务', icon: Server },
  { key: 'logs', label: '系统日志', icon: ScrollText },
  { key: 'settings', label: '系统设置', icon: Settings },
];

export default function Sidebar({
  view,
  onNav,
}: {
  view: ViewKey;
  onNav: (v: ViewKey) => void;
}) {
  const pushToast = useAppStore((s) => s.pushToast);
  const [showAbout, setShowAbout] = useState(false);

  const openExternal = async (url: string, label: string) => {
    try {
      await open(url);
    } catch (e) {
      pushToast('error', `打开${label}失败：${String(e)}`);
    }
  };

  // 主题轮询：按 THEMES 顺序循环切换并持久化（旧值自动归一化）
  const cycleTheme = async () => {
    const { settings, saveSettings } = useAppStore.getState();
    if (!settings) return;
    const next = nextTheme(settings.theme);
    try {
      await saveSettings({ ...settings, theme: next.id });
      pushToast('success', `主题已切换：${next.name}`);
    } catch {
      /* toast 已发出 */
    }
  };

  const toolBtn =
    'flex h-8 w-8 items-center justify-center rounded-lg text-slate-500 transition hover:bg-slate-100 hover:text-slate-700 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-200';

  return (
    <aside className="flex w-52 shrink-0 flex-col border-r border-slate-200 bg-white dark:border-zinc-800 dark:bg-zinc-950">
      <nav className="flex-1 space-y-1 p-3">
        {NAV.map((item) => {
          const Icon = item.icon;
          const active = view === item.key;
          return (
            <button
              key={item.key}
              onClick={() => onNav(item.key)}
              className={cn(
                'flex w-full items-center gap-3 rounded-lg px-3 py-2 text-sm font-medium transition active:scale-[0.98]',
                active
                  ? 'bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900'
                  : 'text-slate-600 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800',
              )}
            >
              <Icon size={17} />
              {item.label}
            </button>
          );
        })}
      </nav>
      <div className="flex items-center justify-center gap-1 border-t border-slate-200 p-3 dark:border-zinc-800">
        <button
          onClick={() => void openExternal(LINK_REPO, '软件 Github 地址')}
          className={toolBtn}
          aria-label="软件 Github 地址"
          title="软件 Github 地址"
        >
          <Github size={17} />
        </button>
        <button
          onClick={() => void openExternal(LINK_BLOG, '作者博客主页')}
          className={toolBtn}
          aria-label="作者博客主页"
          title="作者博客主页"
        >
          <Globe size={17} />
        </button>
        <button
          onClick={() => void cycleTheme()}
          className={toolBtn}
          aria-label="切换主题"
          title={`切换主题（${THEMES.map((t) => t.name).join(' → ')} 轮询）`}
        >
          <Palette size={17} />
        </button>
        <button
          onClick={() => setShowAbout(true)}
          className={toolBtn}
          aria-label="软件说明"
          title="软件说明（版本 / 概述 / 作者 / 版权）"
        >
          <Info size={17} />
        </button>
      </div>
      <AboutDialog open={showAbout} onClose={() => setShowAbout(false)} />
    </aside>
  );
}
