import { useEffect } from 'react';
import TitleBar from './components/TitleBar';
import Sidebar from './components/Sidebar';
import TopBar from './components/TopBar';
import Toaster from './components/Toaster';
import { useAppStore } from './store';
import Dashboard from './pages/Dashboard';
import Accounts from './pages/Accounts';
import Checkin from './pages/Checkin';
import Credits from './pages/Credits';
import Logs from './pages/Logs';
import Settings from './pages/Settings';

function renderView(view: string) {
  switch (view) {
    case 'dashboard':
      return <Dashboard />;
    case 'accounts':
      return <Accounts />;
    case 'checkin':
      return <Checkin />;
    case 'credits':
      return <Credits />;
    case 'logs':
      return <Logs />;
    case 'settings':
      return <Settings />;
  }
}

export default function App() {
  const view = useAppStore((s) => s.view);
  const setView = useAppStore((s) => s.setView);
  const init = useAppStore((s) => s.init);
  const settings = useAppStore((s) => s.settings);

  useEffect(() => {
    void init();
  }, [init]);

  useEffect(() => {
    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    const apply = () => {
      const theme = useAppStore.getState().settings?.theme ?? 'system';
      const dark = theme === 'dark' || (theme === 'system' && mq.matches);
      document.documentElement.classList.toggle('dark', dark);
    };
    apply();
    mq.addEventListener('change', apply);
    return () => mq.removeEventListener('change', apply);
  }, []);

  return (
    <div className="flex h-full flex-col bg-slate-100 text-slate-800 dark:bg-slate-950 dark:text-slate-100">
      <TitleBar />
      <div className="flex min-h-0 flex-1">
        <Sidebar view={view} onNav={setView} />
        <main className="relative flex min-w-0 flex-1 flex-col">
          <div className="pointer-events-none absolute inset-0 bg-[radial-gradient(60%_50%_at_0%_0%,rgba(99,102,241,0.07),transparent)]" />
          <TopBar />
          <div className="relative min-h-0 flex-1 overflow-auto p-5">{renderView(view)}</div>
        </main>
      </div>
      <Toaster />
    </div>
  );
}
