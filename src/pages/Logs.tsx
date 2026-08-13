import { useEffect, useState } from 'react';
import { RefreshCw, Search, Trash2, Download, Copy } from 'lucide-react';
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
  const toast = useAppStore((s) => s.pushToast);

  const [type, setType] = useState('all');
  const [kw, setKw] = useState('');
  const [date, setDate] = useState('');
  const [autoRefresh, setAutoRefresh] = useState(true);

  useEffect(() => {
    void refreshLogs({ logType: type, date: date || undefined });
  }, [type, date, refreshLogs]);

  useEffect(() => {
    if (!autoRefresh) return;
    const id = setInterval(() => {
      void refreshLogs({ logType: type, date: date || undefined, keyword: kw || undefined });
    }, 2000);
    return () => clearInterval(id);
  }, [autoRefresh, type, date, kw, refreshLogs]);

  const onSearch = () => {
    void refreshLogs({ logType: type, date: date || undefined, keyword: kw || undefined });
  };

  const copyProxyLog = async () => {
    if (proxyLog.length === 0) return;
    try {
      await navigator.clipboard.writeText(proxyLog.join('\n'));
      toast('success', '代理日志已复制');
    } catch {
      toast('error', '复制失败');
    }
  };

  const copyLogs = async () => {
    if (logs.length === 0) return;
    const text = logs.map((l) => `[${l.time}] [${l.log_type}] ${l.message}`).join('\n');
    try {
      await navigator.clipboard.writeText(text);
      toast('success', '日志已复制');
    } catch {
      toast('error', '复制失败');
    }
  };

  const exportLogs = () => {
    if (logs.length === 0) return;
    const header = '时间\t类型\t内容\n';
    const body = logs
      .map((l) => `${l.time}\t${l.log_type}\t${l.message}`)
      .join('\n');
    const blob = new Blob(['\ufeff' + header + body], { type: 'text/csv;charset=utf-8' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `trae-work-logs-${new Date().toISOString().slice(0, 10)}.csv`;
    a.click();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="flex h-full animate-fade-in flex-col">
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
                void refreshLogs({ logType: type, date: date || undefined, keyword: kw || undefined });
                void refreshAccounts();
              }}
              className="btn-outline"
            >
              <RefreshCw size={15} /> 刷新
            </button>
            <button onClick={exportLogs} disabled={logs.length === 0} className="btn-outline">
              <Download size={15} /> 导出
            </button>
          </>
        }
      />

      <div className="grid min-h-0 flex-1 gap-3 md:grid-cols-[1fr_2fr]">
        {/* 实时代理输出 - 最新置顶 */}
        <div className="card flex min-h-0 flex-col p-3">
          <div className="mb-2 flex items-center justify-between">
            <h3 className="text-sm font-medium">实时代理输出</h3>
            <div className="flex items-center gap-2">
              <span className="text-xs text-slate-400">{proxyLog.length} 行</span>
              <button
                onClick={copyProxyLog}
                disabled={proxyLog.length === 0}
                className="btn-ghost !p-1"
                title="复制代理日志"
              >
                <Copy size={13} />
              </button>
            </div>
          </div>
          <pre className="flex-1 min-h-0 overflow-auto whitespace-pre-wrap break-all rounded-lg bg-slate-950 p-3 text-xs leading-5 text-emerald-200">
            {proxyLog.length === 0 ? '（暂无输出，启动代理后这里会滚动日志）' : proxyLog.join('\n')}
          </pre>
        </div>

        {/* 查询日志 */}
        <div className="card flex min-h-0 flex-col p-3">
          <div className="mb-2 flex items-center gap-2">
            <select value={type} onChange={(e) => setType(e.target.value)} className="input !py-1.5 !text-xs w-28">
              {TYPES.map((t) => (
                <option key={t.v} value={t.v}>{t.label}</option>
              ))}
            </select>
            <input
              type="date"
              value={date}
              onChange={(e) => setDate(e.target.value)}
              className="input !py-1.5 !text-xs w-36"
            />
            <input
              value={kw}
              onChange={(e) => setKw(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && onSearch()}
              placeholder="搜索关键字…"
              className="input !py-1.5 !text-xs flex-1"
            />
            <button
              onClick={copyLogs}
              disabled={logs.length === 0}
              className="btn-ghost !p-1.5"
              title="复制日志"
            >
              <Copy size={14} />
            </button>
          </div>
          <div className="flex-1 min-h-0 overflow-auto rounded-lg bg-slate-50 p-2 text-xs dark:bg-slate-950">
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
        <div className="text-xs text-slate-400">实时模式已开启：代理输出最新置顶显示。</div>
      )}
    </div>
  );
}
