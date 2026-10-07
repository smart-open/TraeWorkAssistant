import { useEffect, useState } from 'react';
import {
  LayoutDashboard,
  Users,
  PlayCircle,
  Coins,
  Settings,
  Server,
  KeyRound,
  SlidersHorizontal,
  Palette,
  Info,
  Sparkles,
  Bot,
  Boxes,
} from 'lucide-react';
import { useAppStore } from '../store';
import { cn } from '../lib/cn';
import { nextTheme } from '../lib/themes';
import { resolveAppIcon } from '../lib/appIcons';
import type { ViewKey, AppKey } from '../types';
import AboutDialog from './AboutDialog';
import SystemDialog from './SystemDialog';

export type { ViewKey };

const NAV: { key: ViewKey; label: string; icon: typeof Users }[] = [
  { key: 'dashboard', label: '概览', icon: LayoutDashboard },
  { key: 'accounts', label: '账号管理', icon: Users },
  { key: 'checkin', label: '一键签到', icon: PlayCircle },
  { key: 'credits', label: '积分看板', icon: Coins },
  { key: 'api-service', label: '资源调度', icon: Server },
  { key: 'settings', label: '环境配置', icon: Settings },
];

/** Buddy 应用菜单（workbuddy-product-design.md §3.7.0：五页应用级子导航，批次1） */
const BUDDY_NAV: { key: ViewKey; label: string; icon: typeof Users }[] = [
  { key: 'buddy-overview', label: '概述', icon: LayoutDashboard },
  { key: 'buddy-accounts', label: '账号管理', icon: Users },
  { key: 'buddy-checkin', label: '签到与成长', icon: PlayCircle },
  { key: 'buddy-credits', label: '积分看板', icon: Coins },
  { key: 'buddy-api-service', label: '资源调度', icon: Server },
  { key: 'buddy-settings', label: '环境配置', icon: Settings },
];

/** Qoder 应用菜单（F-80：六页子导航，对齐 Buddy；docker 版同构复用） */
const QODER_NAV: { key: ViewKey; label: string; icon: typeof Users }[] = [
  { key: 'qoder-overview', label: '概述', icon: LayoutDashboard },
  { key: 'qoder-accounts', label: '账号管理', icon: Users },
  { key: 'qoder-checkin', label: '每日签到', icon: PlayCircle },
  { key: 'qoder-credits', label: '积分看板', icon: Coins },
  { key: 'qoder-api-service', label: '资源调度', icon: Server },
  { key: 'qoder-settings', label: '环境配置', icon: Settings },
];

/** 应用切换 Tab：trae = Trae 菜单；buddy = WorkBuddy 菜单；qoder = Qoder CN 菜单 */
export const APP_TABS: { key: AppKey; label: string; icon: typeof Users; disabled?: boolean; title?: string }[] = [
  { key: 'trae', label: 'Trae', icon: Sparkles },
  { key: 'buddy', label: 'Buddy', icon: Bot, title: 'WorkBuddy / CodeBuddy' },
  { key: 'qoder', label: 'Qoder', icon: Boxes, title: 'Qoder CN' },
];

/** 全部合法应用 key（settings 脏值兜底用） */
export const APP_KEYS = APP_TABS.map((t) => t.key);

/** 栅格列数必须使用字面量，Tailwind JIT 才能扫描生成 */
const GRID_COLS = ['grid-cols-1', 'grid-cols-2', 'grid-cols-3', 'grid-cols-4'] as const;

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
  const setShowApiManager = useAppStore((s) => s.setShowApiManager);
  const settings = useAppStore((s) => s.settings);
  const [showAbout, setShowAbout] = useState(false);
  const [showSystem, setShowSystem] = useState(false);

  // 侧边栏应用显示（系统设置「应用显示」维护）：固定应用始终可见，其余按隐藏列表过滤。
  // pinned 非法/缺失回退 trae（后端 state.rs 已归一，此处对前端旧缓存双保险）
  const pinnedApp = (APP_KEYS.includes(settings?.pinned_app as AppKey) ? settings?.pinned_app : 'trae') as AppKey;
  const hiddenApps = settings?.hidden_apps ?? [];
  const visibleTabs = APP_TABS.filter((t) => t.key === pinnedApp || !hiddenApps.includes(t.key));

  // 当前激活应用被隐藏（或固定项变更）时自动回落固定应用主页，避免停留在无 Tab 的应用
  useEffect(() => {
    if (!visibleTabs.some((t) => t.key === activeApp)) {
      setActiveApp(pinnedApp);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeApp, pinnedApp, settings?.hidden_apps]);

  // 按当前应用切换菜单：Trae → 6 页；Buddy → 6 页；Qoder → 6 页（F-80）
  const nav = activeApp === 'buddy' ? BUDDY_NAV : activeApp === 'qoder' ? QODER_NAV : NAV;

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
    <aside className="flex h-full w-52 shrink-0 flex-col border-r border-slate-200 bg-white dark:border-zinc-800 dark:bg-zinc-950">
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
      {/* 应用切换 Tab（位于左下角工具图标行上方；显示项由系统设置「应用显示」控制） */}
      <div className="border-t border-slate-200 p-3 dark:border-zinc-800">
        <div
          className={cn(
            'grid gap-1 rounded-lg bg-slate-100 p-1 dark:bg-zinc-900',
            GRID_COLS[visibleTabs.length - 1] ?? 'grid-cols-4',
          )}
        >
          {visibleTabs.map((tab) => {
            // 自定义图标优先（系统设置「应用图标」），非法/缺失回退 APP_TABS 内置图标
            const TabIcon = resolveAppIcon(settings?.app_icons?.[tab.key]) ?? tab.icon;
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
        {/* API 管理入口（统一网关 §5.1，原 Github 图标位置） */}
        <button
          onClick={() => setShowApiManager(true)}
          className="flex h-8 w-8 items-center justify-center rounded-lg text-slate-500 transition hover:bg-slate-100 hover:text-slate-700 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-200"
          aria-label="API 管理"
          title="API 管理（统一网关：启停 / 接口配置 / API Keys / 生态接入 / 用量统计）"
        >
          <KeyRound size={17} />
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
