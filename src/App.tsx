import { useEffect, useState } from 'react';
import { Menu } from 'lucide-react';
import Sidebar from './components/Sidebar';
import Toaster from './components/Toaster';
import { useAppStore } from './store';
import { resolveTheme } from './lib/themes';
import Login from './pages/Login';
import Dashboard from './pages/Dashboard';
import Accounts from './pages/Accounts';
import Checkin from './pages/Checkin';
import Logs from './pages/Logs';
import Settings from './pages/Settings';
import ApiService from './pages/ApiService';
import BuddyOverview from './pages/buddy/BuddyOverview';
import BuddyAccounts from './pages/buddy/BuddyAccounts';
import BuddyCheckin from './pages/buddy/BuddyCheckin';
// 积分看板（credits-dashboard-plan.md；平台拆分）：同一组件按 platform 渲染
// Trae 页（credits 视图，替换原 pages/Credits.tsx）与 Buddy 页（buddy-credits 视图）
import CreditsDashboard from './pages/dashboard/Dashboard';
import BuddyApiService from './pages/buddy/BuddyApiService';
import BuddySettings from './pages/buddy/BuddySettings';
import QoderOverview from './pages/qoder/QoderOverview';
import QoderAccounts from './pages/qoder/QoderAccounts';
import QoderCheckin from './pages/qoder/QoderCheckin';
import QoderApiService from './pages/qoder/QoderApiService';
import QoderSettings from './pages/qoder/QoderSettings';
import ApiManagerModal from './components/api/ApiManagerModal';

function renderView(view: string) {
  switch (view) {
    case 'dashboard':
      return <Dashboard />;
    case 'accounts':
      return <Accounts />;
    case 'checkin':
      return <Checkin />;
    case 'credits':
      // key 强制按平台重挂载：两视图共用同一组件类型，缺 key 时 React 复用实例仅更新 props，
      // 挂载数据加载 useEffect 不重跑，切换平台后另一平台数据全空
      return <CreditsDashboard key="trae" platform="trae" />;
    case 'logs':
      return <Logs />;
    case 'api-service':
      return <ApiService />;
    case 'settings':
      return <Settings />;
    case 'buddy-overview':
      return <BuddyOverview />;
    case 'buddy-accounts':
      return <BuddyAccounts />;
    case 'buddy-checkin':
      return <BuddyCheckin />;
    case 'buddy-credits':
      return <CreditsDashboard key="buddy" platform="buddy" />;
    case 'buddy-api-service':
      return <BuddyApiService />;
    case 'buddy-settings':
      return <BuddySettings />;
    case 'qoder-overview':
      return <QoderOverview />;
    case 'qoder-accounts':
      return <QoderAccounts />;
    case 'qoder-checkin':
      return <QoderCheckin />;
    case 'qoder-credits':
      return <CreditsDashboard key="qoder" platform="qoder" />;
    case 'qoder-api-service':
      return <QoderApiService key="qoder-api-service" />;
    case 'qoder-settings':
      return <QoderSettings />;
    default:
      return <Dashboard />;
  }
}

export default function App() {
  const view = useAppStore((s) => s.view);
  const setView = useAppStore((s) => s.setView);
  const init = useAppStore((s) => s.init);
  const ready = useAppStore((s) => s.ready);
  const authed = useAppStore((s) => s.authed);
  const settings = useAppStore((s) => s.settings);

  // 移动端抽屉：窄屏侧栏默认收起，汉堡按钮展开（md 及以上恒为常驻侧栏）
  const [drawerOpen, setDrawerOpen] = useState(false);

  useEffect(() => {
    void init().catch((err) => {
      console.error('初始化失败:', err);
    });
  }, [init]);

  useEffect(() => {
    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    const apply = () => {
      const theme = useAppStore.getState().settings?.theme;
      const t = resolveTheme(theme, mq.matches);
      document.documentElement.classList.toggle('dark', t.dark);
      document.documentElement.dataset.theme = t.id;
    };
    apply();
    mq.addEventListener('change', apply);
    return () => mq.removeEventListener('change', apply);
  }, [settings?.theme]);

  if (!ready) {
    return (
      <div className="flex h-full items-center justify-center bg-slate-100 dark:bg-zinc-950">
        <div className="flex flex-col items-center gap-3">
          <div className="h-8 w-8 animate-spin rounded-full border-3 border-zinc-400 border-t-transparent" />
          <span className="text-sm text-slate-500">正在加载…</span>
        </div>
      </div>
    );
  }

  // 未登录（ADR-4）：整页登录页，登录成功后经 afterLogin 进入主界面
  if (!authed) {
    return (
      <>
        <Login />
        <Toaster />
      </>
    );
  }

  return (
    <div className="flex h-full flex-col bg-slate-100 text-slate-800 dark:bg-zinc-950 dark:text-zinc-100">
      <div className="flex min-h-0 flex-1">
        {/* 常驻侧栏（md 及以上） */}
        <div className="hidden md:block">
          <Sidebar view={view} onNav={setView} />
        </div>
        {/* 移动端顶栏：汉堡 + 标题 */}
        <div className="fixed inset-x-0 top-0 z-30 flex h-12 items-center gap-2 border-b border-slate-200 bg-white/95 px-3 backdrop-blur md:hidden dark:border-zinc-800 dark:bg-zinc-950/95">
          <button
            type="button"
            aria-label="打开菜单"
            onClick={() => setDrawerOpen(true)}
            className="rounded-lg p-2 text-slate-600 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800"
          >
            <Menu size={20} />
          </button>
          <span className="text-sm font-medium">AI Work 助手</span>
        </div>
        {/* 抽屉：遮罩 + 侧栏（窄屏） */}
        {drawerOpen && (
          <div className="fixed inset-0 z-40 md:hidden" role="dialog" aria-modal="true">
            <div className="absolute inset-0 bg-black/40" onClick={() => setDrawerOpen(false)} />
            <div className="absolute inset-y-0 left-0 shadow-xl">
              <Sidebar
                view={view}
                onNav={(v) => {
                  setView(v);
                  setDrawerOpen(false);
                }}
              />
            </div>
          </div>
        )}
        <main className="relative flex min-w-0 flex-1 flex-col">
          <div className="relative min-h-0 flex-1 overflow-auto p-5 pt-16 md:pt-5">{renderView(view)}</div>
        </main>
      </div>
      <Toaster />
      {/* 全局 API 管理弹窗（任意 activeApp 视图均可打开，unified-api-gateway-design §5.2） */}
      <ApiManagerModal />
    </div>
  );
}
