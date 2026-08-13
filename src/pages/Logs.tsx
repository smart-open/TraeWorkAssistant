import { useEffect, useState, useCallback } from 'react';
import { RefreshCw, Search, Trash2, Download, Copy, ChevronLeft, ChevronRight, Eye } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { EmptyState, Modal } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import type { ProxyLogEntry } from '../types';

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

// ======================== 运行日志 Tab ========================

function SystemLogsTab() {
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
    const body = logs.map((l) => `${l.time}\t${l.log_type}\t${l.message}`).join('\n');
    const blob = new Blob(['\ufeff' + header + body], { type: 'text/csv;charset=utf-8' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `trae-work-logs-${new Date().toISOString().slice(0, 10)}.csv`;
    a.click();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="grid min-h-0 flex-1 gap-3 md:grid-cols-[1fr_2fr]">
      {/* 实时代理输出 */}
      <div className="card flex min-h-0 flex-col p-3">
        <div className="mb-2 flex items-center justify-between">
          <h3 className="text-sm font-medium">实时代理输出</h3>
          <div className="flex items-center gap-2">
            <span className="text-xs text-slate-400">{proxyLog.length} 行</span>
            <button onClick={copyProxyLog} disabled={proxyLog.length === 0} className="btn-ghost !p-1" title="复制代理日志">
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
          <input type="date" value={date} onChange={(e) => setDate(e.target.value)} className="input !py-1.5 !text-xs w-36" />
          <input
            value={kw}
            onChange={(e) => setKw(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && onSearch()}
            placeholder="搜索关键字…"
            className="input !py-1.5 !text-xs flex-1"
          />
          <button onClick={copyLogs} disabled={logs.length === 0} className="btn-ghost !p-1.5" title="复制日志">
            <Copy size={14} />
          </button>
        </div>
        <div className="flex-1 min-h-0 overflow-auto rounded-lg bg-slate-50 p-2 text-xs dark:bg-zinc-950">
          {logs.length === 0 ? (
            <EmptyState icon={<Trash2 size={28} />} title="暂无日志" hint="尝试调整类型与关键字后查询。" />
          ) : (
            logs.map((l, i) => (
              <div key={i} className="flex gap-2 border-b border-slate-200 py-1 dark:border-zinc-800">
                <span className="shrink-0 font-mono text-slate-400">{l.time}</span>
                <span className={`shrink-0 ${typeColor[l.log_type] ?? 'text-slate-500'}`}>[{l.log_type}]</span>
                <span className="break-all">{l.message}</span>
              </div>
            ))
          )}
        </div>
      </div>

      {autoRefresh && proxyLog.length > 0 && (
        <div className="text-xs text-slate-400">实时模式已开启：代理输出最新置顶显示。</div>
      )}
    </div>
  );
}

// ======================== 代理日志 Tab ========================

const PAGE_SIZE = 30;

function ProxyLogsTab() {
  const toast = useAppStore((s) => s.pushToast);
  const [entries, setEntries] = useState<ProxyLogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(0);
  const [kw, setKw] = useState('');
  const [startDate, setStartDate] = useState('');
  const [endDate, setEndDate] = useState('');
  const [loading, setLoading] = useState(false);
  const [detail, setDetail] = useState<string | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);

  const fetchList = useCallback(async () => {
    setLoading(true);
    try {
      const r = await api.misc.proxyLogsList({
        keyword: kw || undefined,
        startTime: startDate ? `${startDate} 00:00:00` : undefined,
        endTime: endDate ? `${endDate} 23:59:59` : undefined,
        offset: page * PAGE_SIZE,
        limit: PAGE_SIZE,
      });
      setEntries(r.entries);
      setTotal(r.total);
    } catch (e) {
      toast('error', `查询代理日志失败：${String(e)}`);
    } finally {
      setLoading(false);
    }
  }, [kw, startDate, endDate, page, toast]);

  useEffect(() => {
    void fetchList();
  }, [fetchList]);

  const onSearch = () => {
    setPage(0);
    void fetchList();
  };

  const showDetail = async (id: string) => {
    setDetailLoading(true);
    setDetail('');
    try {
      const raw = await api.misc.proxyLogDetail(id);
      setDetail(raw);
    } catch (e) {
      toast('error', `获取详情失败：${String(e)}`);
    } finally {
      setDetailLoading(false);
    }
  };

  const copyDetail = async () => {
    if (!detail) return;
    try {
      await navigator.clipboard.writeText(detail);
      toast('success', '已复制到剪贴板');
    } catch {
      toast('error', '复制失败');
    }
  };

  const totalPages = Math.ceil(total / PAGE_SIZE);

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      {/* 过滤栏 */}
      <div className="card flex flex-wrap items-center gap-2 p-3">
        <input
          type="date"
          value={startDate}
          onChange={(e) => setStartDate(e.target.value)}
          className="input !py-1.5 !text-xs w-36"
          title="开始日期"
        />
        <span className="text-xs text-slate-400">至</span>
        <input
          type="date"
          value={endDate}
          onChange={(e) => setEndDate(e.target.value)}
          className="input !py-1.5 !text-xs w-36"
          title="结束日期"
        />
        <input
          value={kw}
          onChange={(e) => setKw(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && onSearch()}
          placeholder="搜索关键字（URL/Header/Body）…"
          className="input !py-1.5 !text-xs flex-1"
        />
        <button onClick={onSearch} className="btn-outline !py-1.5">
          <Search size={14} /> 查询
        </button>
        <button onClick={() => { setKw(''); setStartDate(''); setEndDate(''); setPage(0); }} className="btn-ghost !py-1.5">
          <RefreshCw size={14} /> 重置
        </button>
      </div>

      {/* 列表 */}
      <div className="card flex min-h-0 flex-1 flex-col overflow-hidden">
        <div className="flex items-center justify-between border-b border-slate-100 px-3 py-2 dark:border-zinc-800">
          <span className="text-sm font-medium">
            代理请求日志 {loading ? '(加载中…)' : `(${total} 条)`}
          </span>
          {total > 0 && (
            <span className="text-xs text-slate-400">
              第 {page * PAGE_SIZE + 1}-{Math.min((page + 1) * PAGE_SIZE, total)} 条 / 共 {totalPages} 页
            </span>
          )}
        </div>
        <div className="flex-1 min-h-0 overflow-auto">
          {entries.length === 0 ? (
            <div className="p-6">
              <EmptyState icon={<Trash2 size={28} />} title="暂无代理日志" hint="启动代理后，拦截到的请求会记录在这里。" />
            </div>
          ) : (
            <table className="w-full text-sm">
              <thead className="sticky top-0 bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                <tr>
                  <th className="px-3 py-2 text-left">时间</th>
                  <th className="px-3 py-2 text-left">方法</th>
                  <th className="px-3 py-2 text-left">主机</th>
                  <th className="px-3 py-2 text-left">路径</th>
                  <th className="px-3 py-2 text-left">状态</th>
                  <th className="px-3 py-2 text-right">大小</th>
                  <th className="px-3 py-2 text-left">SSE摘要</th>
                  <th className="px-3 py-2 text-right">操作</th>
                </tr>
              </thead>
              <tbody>
                {entries.map((e) => (
                  <tr key={e.id} className="border-t border-slate-100 hover:bg-slate-50 dark:border-zinc-800 dark:hover:bg-zinc-800/50">
                    <td className="px-3 py-2 whitespace-nowrap font-mono text-xs text-slate-500">{e.timestamp}</td>
                    <td className="px-3 py-2">
                      <span className={`chip whitespace-nowrap ${
                        e.method === 'WebSocket' ? 'bg-violet-100 text-violet-700 dark:bg-violet-500/15 dark:text-violet-300' :
                        e.method === 'HTTP GET' ? 'bg-sky-100 text-sky-700 dark:bg-sky-500/15 dark:text-sky-300' :
                        e.method === 'HTTP POST' ? 'bg-emerald-100 text-emerald-700 dark:bg-emerald-500/15 dark:text-emerald-300' :
                        e.method === 'HTTP PUT' || e.method === 'HTTP PATCH' ? 'bg-amber-100 text-amber-700 dark:bg-amber-500/15 dark:text-amber-300' :
                        e.method === 'HTTP DELETE' ? 'bg-rose-100 text-rose-700 dark:bg-rose-500/15 dark:text-rose-300' :
                        'bg-slate-100 text-slate-600 dark:bg-zinc-800 dark:text-zinc-300'
                      }`}>
                        {e.method}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-xs font-mono text-slate-600 dark:text-zinc-400">{e.host}</td>
                    <td className="px-3 py-2 text-xs text-slate-500 max-w-xs truncate" title={e.path}>{e.path}</td>
                    <td className="px-3 py-2">
                      <span className={`text-xs font-mono ${
                        e.status.startsWith('2') ? 'text-emerald-500' :
                        e.status.startsWith('4') || e.status.startsWith('5') ? 'text-rose-500' :
                        'text-slate-500'
                      }`}>
                        {e.status}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-right text-xs text-slate-400 tabular-nums">
                      {e.size > 1024 ? `${(e.size / 1024).toFixed(1)}KB` : `${e.size}B`}
                    </td>
                    <td className="px-3 py-2 text-xs">
                      {e.sse_model || e.sse_tokens ? (
                        <div className="flex flex-col gap-0.5">
                          {e.sse_model && (
                            <span className="text-sky-500 font-medium">{e.sse_model}</span>
                          )}
                          {e.sse_tokens && (
                            <span className="text-slate-400 font-mono">{e.sse_tokens}</span>
                          )}
                        </div>
                      ) : (
                        <span className="text-slate-300">-</span>
                      )}
                    </td>
                    <td className="px-3 py-2 text-right">
                      <button onClick={() => void showDetail(e.id)} className="btn-ghost !p-1.5" title="查看详情">
                        <Eye size={14} />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
        {/* 分页 */}
        {totalPages > 1 && (
          <div className="flex items-center justify-between border-t border-slate-100 px-3 py-2 dark:border-zinc-800">
            <button
              onClick={() => setPage((p) => Math.max(0, p - 1))}
              disabled={page === 0}
              className="btn-outline !py-1 disabled:opacity-40"
            >
              <ChevronLeft size={14} /> 上一页
            </button>
            <span className="text-xs text-slate-400">{page + 1} / {totalPages}</span>
            <button
              onClick={() => setPage((p) => Math.min(totalPages - 1, p + 1))}
              disabled={page >= totalPages - 1}
              className="btn-outline !py-1 disabled:opacity-40"
            >
              下一页 <ChevronRight size={14} />
            </button>
          </div>
        )}
      </div>

      {/* 详情弹窗 */}
      <Modal
        open={detail !== null || detailLoading}
        onClose={() => { setDetail(null); setDetailLoading(false); }}
        title="请求详情"
        footer={
          <>
            <button onClick={() => { setDetail(null); setDetailLoading(false); }} className="btn-ghost">关闭</button>
            <button onClick={copyDetail} disabled={!detail} className="btn-primary">
              <Copy size={14} /> 复制
            </button>
          </>
        }
      >
        {detailLoading ? (
          <div className="py-8 text-center text-sm text-slate-400">加载中…</div>
        ) : (
          <pre className="max-h-[60vh] overflow-auto whitespace-pre-wrap break-all rounded-lg bg-slate-50 p-3 text-xs dark:bg-zinc-950">
            {detail}
          </pre>
        )}
      </Modal>
    </div>
  );
}

// ======================== 主页面 ========================

export default function Logs() {
  const [tab, setTab] = useState<'system' | 'proxy'>('system');

  return (
    <div className="flex h-full animate-fade-in flex-col">
      <PageHeader
        title="系统日志"
        desc="查看运行日志与代理请求日志"
      />

      {/* Tab 切换 */}
      <div className="mb-3 flex gap-2">
        <button
          onClick={() => setTab('system')}
          className={`chip border ${tab === 'system' ? 'border-brand-500 bg-brand-50 text-brand-700 dark:bg-brand-500/15 dark:text-brand-300' : 'border-slate-300 text-slate-500'}`}
        >
          运行日志
        </button>
        <button
          onClick={() => setTab('proxy')}
          className={`chip border ${tab === 'proxy' ? 'border-brand-500 bg-brand-50 text-brand-700 dark:bg-brand-500/15 dark:text-brand-300' : 'border-slate-300 text-slate-500'}`}
        >
          代理日志
        </button>
      </div>

      {tab === 'system' ? <SystemLogsTab /> : <ProxyLogsTab />}
    </div>
  );
}
