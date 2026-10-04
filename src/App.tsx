import { useEffect } from 'react';
import TitleBar from './components/TitleBar';
import Sidebar from './components/Sidebar';
import TopBar from './components/TopBar';
import Toaster from './components/Toaster';
import { useAppStore } from './store';
import { resolveTheme } from './lib/themes';
import Dashboard from './pages/Dashboard';
import Accounts from './pages/Accounts';
import Checkin from './pages/Checkin';
import Logs from './pages/Logs';
import Settings from './pages/Settings';
import ApiService from './pages/ApiService';
import DoubaoOverview from './pages/doubao/DoubaoOverview';
import DoubaoAccounts from './pages/doubao/DoubaoAccounts';
import DoubaoSettings from './pages/doubao/DoubaoSettings';
import BuddyOverview from './pages/buddy/BuddyOverview';
import BuddyAccounts from './pages/buddy/BuddyAccounts';
import BuddyCheckin from './pages/buddy/BuddyCheckin';
// 积分看板（credits-dashboard-plan.md；平台拆分）：同一组件按 platform 渲染
// Trae 页（credits 视图）、Buddy 页（buddy-credits 视图）、Qoder 页（qoder-credits 视图，替换旧 QoderCredits.tsx）
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
    case 'doubao-overview':
      return <DoubaoOverview />;
    case 'doubao-accounts':
      return <DoubaoAccounts />;
    case 'doubao-settings':
      return <DoubaoSettings />;
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
  const settings = useAppStore((s) => s.settings);

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

  return (
    <div className="flex h-full flex-col bg-slate-100 text-slate-800 dark:bg-zinc-950 dark:text-zinc-100">
      <TitleBar />
      <div className="flex min-h-0 flex-1">
        <Sidebar view={view} onNav={setView} />
        <main className="relative flex min-w-0 flex-1 flex-col">
          <TopBar />
          <div className="relative min-h-0 flex-1 overflow-auto p-5">{renderView(view)}</div>
        </main>
      </div>
      <Toaster />
      {/* 全局 API 管理弹窗（任意 activeApp 视图均可打开，unified-api-gateway-design §5.2） */}
      <ApiManagerModal />
    </div>
  );
}
