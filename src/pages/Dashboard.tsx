import { useMemo } from 'react';
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  ResponsiveContainer,
  Tooltip,
  CartesianGrid,
  Cell,
} from 'recharts';
import {
  ShieldAlert,
  ExternalLink,
  Gift,
  RefreshCw,
} from 'lucide-react';
import PageHeader from '../components/PageHeader';
import SetupGuide from '../components/SetupGuide';
import { StatCard, Badge, EmptyState } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';

export default function Dashboard() {
  const accounts = useAppStore((s) => s.accounts);
  const env = useAppStore((s) => s.env);
  const proxy = useAppStore((s) => s.proxy);
  const certInstalled = useAppStore((s) => s.certInstalled);
  const toast = useAppStore((s) => s.pushToast);

  const total = accounts.length;
  const checkedToday = accounts.filter((a) => a.checked_today).length;
  const totalCredits = useMemo(
    () => accounts.reduce((s, a) => s + (a.credits ?? 0), 0),
    [accounts],
  );
  const warned = accounts.filter(
    (a) => a.jwt_exp_hours !== null && a.jwt_exp_hours <= 24,
  ).length;

  const top = useMemo(
    () =>
      [...accounts]
        .filter((a) => a.credits != null)
        .sort((a, b) => (b.credits ?? 0) - (a.credits ?? 0))
        .slice(0, 8)
        .map((a) => ({ name: a.name, credits: a.credits as number })),
    [accounts],
  );

  const refresh = async () => {
    toast('info', '刷新中…');
    // 简单做法：依次触发 store 刷新动作
    const s = useAppStore.getState();
    await Promise.all([
      s.refreshEnv(),
      s.refreshCert(),
      s.refreshProxy(),
      s.refreshAccounts(),
      s.refreshGroups(),
    ]);
    toast('success', '已刷新');
  };

  const openTrae = async () => {
    try {
      if (env?.installed) {
        await api.env.openApp(proxy.running ? proxy.port : undefined);
      } else {
        await api.env.openSite();
      }
    } catch (e) {
      toast('error', `打开 Trae Work 失败：${String(e)}`);
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="概览"
        desc="Trae 多账号签到工作台 · 一眼掌握状态与快捷入口"
        actions={
          <button onClick={refresh} className="btn-outline">
            <RefreshCw size={15} /> 刷新
          </button>
        }
      />

      <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
        <StatCard label="账号总数" value={total} hint={`今日已签 ${checkedToday}`} tone="brand" />
        <StatCard label="积分合计" value={totalCredits.toLocaleString()} tone="amber" />
        <StatCard
          label="代理状态"
          value={proxy.running ? `运行 :${proxy.port}` : '未启动'}
          hint={proxy.started_at != null ? `已捕获 ${proxy.captured} 个 · 启动于 ${new Date(proxy.started_at * 1000).toLocaleTimeString()}` : undefined}
          tone={proxy.running ? 'green' : 'slate'}
        />
        <StatCard
          label="JWT 告警"
          value={warned}
          hint="24h 内将过期"
          tone={warned > 0 ? 'red' : 'slate'}
        />
      </div>

      {!env?.installed && (
        <div className="mt-5 card flex items-center justify-between gap-4 p-4">
          <div className="flex items-center gap-3">
            <ShieldAlert className="text-amber-500" />
            <div>
              <div className="font-medium">未检测到 Trae 安装</div>
              <div className="text-xs text-slate-500">请先安装 Trae，再启动代理进行账号登录态捕获。</div>
            </div>
          </div>
          <button
            onClick={() => void openTrae()}
            className="btn-outline"
          >
            <ExternalLink size={15} /> {env?.installed ? '打开 Trae Work' : '前往下载'}
          </button>
        </div>
      )}
      {!certInstalled && env?.installed && (
        <div className="mt-3 card flex items-center justify-between gap-4 p-4">
          <div className="flex items-center gap-3">
            <ShieldAlert className="text-amber-500" />
            <div>
              <div className="font-medium">CA 证书尚未安装</div>
              <div className="text-xs text-slate-500">代理已启动但 TRAE 不信任代理证书将无法拦截签到接口。</div>
            </div>
          </div>
          <button onClick={() => api.cert.install()} className="btn-primary">
            一键安装证书
          </button>
        </div>
      )}

      <div className="mt-5">
        <SetupGuide />
      </div>

      <div className="mt-5 grid gap-3 md:grid-cols-3">
        <div className="card p-4 md:col-span-3">
          <div className="mb-3 flex items-center justify-between">
            <h3 className="font-medium">积分榜 Top 榜</h3>
            <Badge tone="brand">实时</Badge>
          </div>
          {top.length === 0 ? (
            <EmptyState icon={<Gift size={28} />} title="暂无积分数据" hint="运行签到后这里会显示积分排行。" />
          ) : (
            <div className="h-72">
              <ResponsiveContainer>
                <BarChart data={top} margin={{ top: 8, right: 16, left: 0, bottom: 4 }}>
                  <CartesianGrid strokeDasharray="3 3" stroke="#e2e8f0" opacity={0.4} />
                  <XAxis dataKey="name" tick={{ fontSize: 11 }} interval={0} angle={-18} textAnchor="end" height={48} />
                  <YAxis tick={{ fontSize: 11 }} />
                  <Tooltip
                    contentStyle={{ fontSize: 12, borderRadius: 8 }}
                    formatter={(v: number) => v.toLocaleString()}
                  />
                  <Bar dataKey="credits" radius={[6, 6, 0, 0]}>
                    {top.map((_, i) => (
                      <Cell key={i} fill={i === 0 ? '#6366f1' : '#818cf8'} />
                    ))}
                  </Bar>
                </BarChart>
              </ResponsiveContainer>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}