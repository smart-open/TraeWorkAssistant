import { useState } from 'react';
import {
  LayoutDashboard,
  Users,
  PlayCircle,
  Coins,
  Settings,
  Server,
  Github,
  Globe,
  SlidersHorizontal,
  Palette,
  Info,
  Sparkles,
  Bot,
  LayoutGrid,
} from 'lucide-react';
import { open } from '@tauri-apps/plugin-shell';
import { useAppStore } from '../store';
import { cn } from '../lib/cn';
import { LINK_REPO, LINK_BLOG } from '../lib/about';
import { nextTheme } from '../lib/themes';
import type { ViewKey, AppKey } from '../types';
import AboutDialog from './AboutDialog';
import SystemDialog from './SystemDialog';

export type { ViewKey };

const NAV: { key: ViewKey; label: string; icon: typeof Users }[] = [
  { key: 'dashboard', label: '概览', icon: LayoutDashboard },
  { key: 'accounts', label: '账号管理', icon: Users },
  { key: 'checkin', label: '一键签到', icon: PlayCircle },
  { key: 'credits', label: '积分看板', icon: Coins },
  { key: 'api-service', label: 'API 服务', icon: Server },
  { key: 'settings', label: '环境配置', icon: Settings },
];

/** 豆包应用菜单（doubao-trae-switch-plan.md：概述 / 账号管理 / 环境配置） */
const DOUBAO_NAV: { key: ViewKey; label: string; icon: typeof Users }[] = [
  { key: 'doubao-overview', label: '概述', icon: LayoutDashboard },
  { key: 'doubao-accounts', label: '账号管理', icon: Users },
  { key: 'doubao-settings', label: '环境配置', icon: Settings },
];

/** Buddy 应用菜单（workbuddy-product-design.md §3.7.0：五页应用级子导航，批次1） */
const BUDDY_NAV: { key: ViewKey; label: string; icon: typeof Users }[] = [
  { key: 'buddy-overview', label: '概述', icon: LayoutDashboard },
  { key: 'buddy-accounts', label: '账号管理', icon: Users },
  { key: 'buddy-checkin', label: '签到与成长', icon: PlayCircle },
  { key: 'buddy-credits', label: '积分与统计', icon: Coins },
  { key: 'buddy-settings', label: '环境配置', icon: Settings },
];

/** 应用切换 Tab：trae = 当前菜单；buddy = 批次1接入；doubao = 接入中 */
const APP_TABS: { key: AppKey; label: string; icon: typeof Users; disabled?: boolean; title?: string }[] = [
  { key: 'trae', label: 'Trae', icon: Sparkles },
  { key: 'buddy', label: 'Buddy', icon: Bot, title: 'WorkBuddy / CodeBuddy' },
  { key: 'doubao', label: '豆包', icon: LayoutGrid },
];

export default function Sidebar({
  view,
  onNav,
}: {
  view: ViewKey;
  onNav: (v: ViewKey) => void;
}) {
  const pushToast = useAppStore((s) => s.pushToast);
  const activeApp = useAppStore((s) => s.activeApp);
  const setActiveApp = useAppStore((s) => s.setActiveApp);
  const [showAbout, setShowAbout] = useState(false);
  const [showSystem, setShowSystem] = useState(false);

  // 按当前应用切换菜单：Trae → 现有 6 页；豆包 → 3 页；Buddy → 5 页（批次1）
  const nav = activeApp === 'doubao' ? DOUBAO_NAV : activeApp === 'buddy' ? BUDDY_NAV : NAV;

  const openExternal = async (url: string, label: string) => {
    try {
      await open(url);
    } catch (e) {
      pushToast('error', `打开${label}失败：${String(e)}`);
    }
  };

  // 主题轮询：每次点击切换到下一个主题并持久化
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

  return (
    <aside className="flex w-52 shrink-0 flex-col border-r border-slate-200 bg-white dark:border-zinc-800 dark:bg-zinc-950">
      <nav className="flex-1 space-y-1 p-3">
        {nav.map((item) => {
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
      {/* 应用切换 Tab（位于左下角工具图标行上方） */}
      <div className="border-t border-slate-200 p-3 dark:border-zinc-800">
        <div className="grid grid-cols-3 gap-1 rounded-lg bg-slate-100 p-1 dark:bg-zinc-900">
          {APP_TABS.map((tab) => {
            const TabIcon = tab.icon;
            const active = activeApp === tab.key;
            return (
              <button
                key={tab.key}
                disabled={tab.disabled}
                onClick={() => setActiveApp(tab.key)}
                title={tab.title ?? tab.label}
                className={cn(
                  'flex flex-col items-center gap-0.5 rounded-md px-1 py-1.5 text-[11px] font-medium transition active:scale-[0.97]',
                  tab.disabled && 'cursor-not-allowed opacity-40',
                  active
                    ? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-800 dark:text-zinc-100'
                    : 'text-slate-500 hover:text-slate-700 dark:text-zinc-500 dark:hover:text-zinc-300',
                )}
              >
                <TabIcon size={15} />
                {tab.label}
              </button>
            );
          })}
        </div>
      </div>
      <div className="flex items-center justify-center gap-1 border-t border-slate-200 p-3 dark:border-zinc-800">
        <button
          onClick={() => void openExternal(LINK_REPO, '软件 Github 地址')}
          className="flex h-8 w-8 items-center justify-center rounded-lg text-slate-500 transition hover:bg-slate-100 hover:text-slate-700 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-200"
          aria-label="软件 Github 地址"
          title="软件 Github 地址"
        >
          <Github size={17} />
        </button>
        <button
          onClick={() => void openExternal(LINK_BLOG, '作者博客主页')}
          className="flex h-8 w-8 items-center justify-center rounded-lg text-slate-500 transition hover:bg-slate-100 hover:text-slate-700 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-200"
          aria-label="作者博客主页"
          title="作者博客主页"
        >
          <Globe size={17} />
        </button>
        <button
          onClick={() => setShowSystem(true)}
          className="flex h-8 w-8 items-center justify-center rounded-lg text-slate-500 transition hover:bg-slate-100 hover:text-slate-700 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-200"
          aria-label="系统设置与系统日志查看"
          title="系统设置与系统日志查看"
        >
          <SlidersHorizontal size={17} />
        </button>
        <button
          onClick={() => void cycleTheme()}
          className="flex h-8 w-8 items-center justify-center rounded-lg text-slate-500 transition hover:bg-slate-100 hover:text-slate-700 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-200"
          aria-label="切换主题"
          title="切换主题（在所有主题间轮询）"
        >
          <Palette size={17} />
        </button>
        <button
          onClick={() => setShowAbout(true)}
          className="flex h-8 w-8 items-center justify-center rounded-lg text-slate-500 transition hover:bg-slate-100 hover:text-slate-700 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-200"
          aria-label="软件说明"
          title="软件说明（版本 / 概述 / 作者 / 版权）"
        >
          <Info size={17} />
        </button>
      </div>
      <AboutDialog open={showAbout} onClose={() => setShowAbout(false)} />
      <SystemDialog open={showSystem} onClose={() => setShowSystem(false)} />
    </aside>
  );
}
