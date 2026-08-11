import { useEffect, useState } from 'react';
import { RefreshCw, Search, Trash2 } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { EmptyState } from '../components/ui';
import { useAppStore } from '../store';

const TYPES = [
  { v: 'all', label: '全部' },
  { v: 'proxy', label: '代理' },
  { v: 'checkin', label: '签到' },
  { v: 'switch', label: '切换' },
];

const typeColor: Record<string, string> = {
  proxy: 'text-sky-600 dark:text-sky-300',
  checkin: 'text-emerald-600 dark:text-emerald-300',
  switch: 'text-violet-600 dark:text-violet-300',
  system: 'text-slate-500',
  error: 'text-rose-600 dark:text-rose-300',
};

export default function Logs() {
  const logs = useAppStore((s) => s.logs);
  const proxyLog = useAppStore((s) => s.proxyLog);
  const refreshLogs = useAppStore((s) => s.refreshLogs);
  const refreshAccounts = useAppStore((s) => s.refreshAccounts);

  const [type, setType] = useState('all');
  const [kw, setKw] = useState('');
  const [autoRefresh, setAutoRefresh] = useState(true);

  useEffect(() => {
    void refreshLogs({ logType: type, keyword: kw || undefined });
  }, [type, refreshLogs]);

  const onSearch = () => {
    void refreshLogs({ logType: type, keyword: kw || undefined });
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="运行日志"
        desc="查看代理、签到、账号切换的运行日志与实时输出"
        actions={
          <>
            <label className="flex items-center gap-2 text-xs text-slate-500">
              <input type="checkbox" checked={autoRefresh} onChange={(e) => setAutoRefresh(e.target.checked)} />
              实时滚动
            </label>
            <button onClick={onSearch} className="btn-outline">
              <Search size={15} /> 查询
            </button>
            <button
              onClick={() => {
                void refreshLogs({ logType: type, keyword: kw || undefined });
                void refreshAccounts();
              }}
              className="btn-outline"
            >
              <RefreshCw size={15} /> 刷新
            </button>
          </>
        }
      />

      <div className="mb-4 grid gap-3 md:grid-cols-[1fr_2fr]">
        <div className="card p-3">
          <div className="mb-2 flex items-center justify-between">
            <h3 className="text-sm font-medium">实时代理输出</h3>
            <span className="text-xs text-slate-400">{proxyLog.length} 行</span>
          </div>
          <pre className="max-h-[480px] overflow-auto whitespace-pre-wrap break-all rounded-lg bg-slate-950 p-3 text-xs leading-5 text-emerald-200">
            {proxyLog.length === 0 ? '（暂无输出，启动代理后这里会滚动日志）' : proxyLog.join('\n')}
          </pre>
        </div>

        <div className="card p-3">
          <div className="mb-2 flex items-center gap-2">
            <select value={type} onChange={(e) => setType(e.target.value)} className="input !py-1.5 !text-xs w-28">
              {TYPES.map((t) => (
                <option key={t.v} value={t.v}>{t.label}</option>
              ))}
            </select>
            <input
              value={kw}
              onChange={(e) => setKw(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && onSearch()}
              placeholder="搜索关键字…"
              className="input !py-1.5 !text-xs flex-1"
            />
          </div>
          <div className="max-h-[480px] overflow-auto rounded-lg bg-slate-50 p-2 text-xs dark:bg-slate-950">
            {logs.length === 0 ? (
              <EmptyState icon={<Trash2 size={28} />} title="暂无日志" hint="尝试调整类型与关键字后查询。" />
            ) : (
              logs.map((l, i) => (
                <div key={i} className="flex gap-2 border-b border-slate-200 py-1 dark:border-slate-800">
                  <span className="shrink-0 font-mono text-slate-400">{l.time}</span>
                  <span className={`shrink-0 ${typeColor[l.log_type] ?? 'text-slate-500'}`}>
                    [{l.log_type}]
                  </span>
                  <span className="break-all">{l.message}</span>
                </div>
              ))
            )}
          </div>
        </div>
      </div>

      {autoRefresh && proxyLog.length > 0 && (
        <div className="text-xs text-slate-400">实时模式已开启：代理输出会自动滚动显示。</div>
      )}
    </div>
  );
}