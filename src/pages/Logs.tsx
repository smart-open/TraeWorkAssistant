import { useEffect, useState, useCallback, useRef } from 'react';
import { RefreshCw, Search, Trash2, Copy, Bug, FileText, X, ChevronLeft, ChevronRight, List, ScrollText } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { EmptyState, Modal } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import { withMinDelay } from '../lib/delay';
import { copyText } from '../lib/clipboard';
import type { ApiLogEntry } from '../types';

const TYPES = [
  { v: 'all', label: '全部' },
  { v: 'checkin', label: '签到' },
  { v: 'app', label: '应用' },
];

const typeColor: Record<string, string> = {
  checkin: 'text-emerald-600 dark:text-emerald-300',
  app: 'text-amber-600 dark:text-amber-300',
  system: 'text-slate-500',
  error: 'text-rose-600 dark:text-rose-300',
};

// ======================== 运行日志 Tab ========================

function SystemLogsTab() {
  const logs = useAppStore((s) => s.logs);
  const refreshLogs = useAppStore((s) => s.refreshLogs);
  const toast = useAppStore((s) => s.pushToast);

  const [type, setType] = useState('all');
  const [kw, setKw] = useState('');
  const [date, setDate] = useState('');
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [clearConfirm, setClearConfirm] = useState(false);
  const [clearing, setClearing] = useState(false);
  // 自动刷新轮询读取的关键字快照：输入经 onSearch/回车确认后才生效，避免每键重建 interval
  const kwRef = useRef('');

  useEffect(() => {
    void refreshLogs({ logType: type, date: date || undefined }, true);
  }, [type, date, refreshLogs]);

  useEffect(() => {
    if (!autoRefresh) return;
    const id = setInterval(() => {
      void refreshLogs({ logType: type, date: date || undefined, keyword: kwRef.current || undefined });
    }, 2000);
    return () => clearInterval(id);
  }, [autoRefresh, type, date, refreshLogs]);

  const onSearch = () => {
    kwRef.current = kw;
    void refreshLogs({ logType: type, date: date || undefined, keyword: kw || undefined }, true);
  };

  const copyLogs = async () => {
    if (logs.length === 0) {
      toast('error', '暂无日志可复制');
      return;
    }
    const text = logs.map((l) => `[${l.time}] [${l.log_type}] ${l.message}`).join('\n');
    if (await copyText(text)) {
      toast('success', '日志已复制');
    } else {
      toast('error', '复制失败');
    }
  };

  // T6：按类型清理日志文件（删除后写入方自动重建）
  const doClearLogs = async () => {
    setClearing(true);
    try {
      const removed = await api.misc.logsClear(type);
      toast('success', `日志已清理（删除 ${removed} 个文件）`);
      setClearConfirm(false);
      kwRef.current = kw;
      void refreshLogs({ logType: type, date: date || undefined, keyword: kw || undefined });
    } catch (e) {
      toast('error', `清理日志失败：${String(e)}`);
    } finally {
      setClearing(false);
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      {/* 查询日志 */}
      <div className="card flex min-h-0 flex-1 flex-col p-3">
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
          <button
            onClick={() => setClearConfirm(true)}
            disabled={clearing}
            className="btn-ghost !p-1.5 text-rose-500 hover:text-rose-600"
            title="清理当前类型日志"
          >
            <Trash2 size={14} />
          </button>
          <label className="ml-1 flex shrink-0 items-center gap-1.5 text-xs text-slate-500">
            <input type="checkbox" checked={autoRefresh} onChange={(e) => setAutoRefresh(e.target.checked)} />
            自动刷新
          </label>
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

      {/* 清理日志确认弹窗（T6） */}
      <Modal
        open={clearConfirm}
        onClose={() => !clearing && setClearConfirm(false)}
        title="清理日志"
        footer={
          <>
            <button onClick={() => setClearConfirm(false)} disabled={clearing} className="btn-ghost">
              取消
            </button>
            <button onClick={() => void doClearLogs()} disabled={clearing} className="btn-primary">
              <Trash2 size={14} /> {clearing ? '清理中…' : '确认清理'}
            </button>
          </>
        }
      >
        <p className="text-sm">
          将删除类型为「{TYPES.find((t) => t.v === type)?.label ?? type}」的日志文件，删除后不可恢复。
        </p>
        <p className="mt-1 text-xs text-slate-400">日志文件会在后续写入时自动重建，不影响应用运行。</p>
      </Modal>
    </div>
  );
}

// ======================== API 请求日志 Tab ========================

// ======================== F-84 结构化「最近请求」视图 ========================

const RECENT_PAGE_SIZE = 50;
const RECENT_POOLS = [
  { v: 'all', label: '全部' },
  { v: 'trae', label: 'Trae' },
  { v: 'buddy', label: 'Buddy' },
  { v: 'buddy-ai', label: 'Buddy AI' },
  { v: 'qoder', label: 'Qoder' },
  { v: 'custom', label: '自定义' },
];
const RECENT_RESULTS = [
  { v: 'all', label: '全部' },
  { v: 'ok', label: '成功' },
  { v: 'fail', label: '失败' },
];

/** 状态着色：2xx 成功 / 429 与 4xx 降级琥珀 / 5xx 失败红 */
function statusChipCls(status: number): string {
  if (status >= 200 && status < 300) {
    return 'bg-emerald-100 text-emerald-700 dark:bg-emerald-500/15 dark:text-emerald-300';
  }
  if (status >= 400 && status < 500) {
    return 'bg-amber-100 text-amber-700 dark:bg-amber-500/15 dark:text-amber-300';
  }
  return 'bg-rose-100 text-rose-700 dark:bg-rose-500/15 dark:text-rose-300';
}

function ApiRecentRequests() {
  // D-14：可选时间窗口（后端 api_logs_recent 默认 3 天），供「天数」按钮组切换
  const RECENT_DAYS = [1, 3, 7, 14, 30] as const;
  const [rows, setRows] = useState<ApiLogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [truncated, setTruncated] = useState(false);
  const [page, setPage] = useState(1);
  const [pool, setPool] = useState('all');
  const [result, setResult] = useState('all');
  const [loading, setLoading] = useState(false);
  // D-14：当前窗口天数；daysRef 双轨记录，防 load 闭包拿到旧值
  const [days, setDays] = useState(3);
  const daysRef = useRef(3);
  const reqIdRef = useRef(0);

  const load = useCallback(async (p: number, pf: string, rf: string, d?: number) => {
    const reqId = ++reqIdRef.current;
    setLoading(true);
    try {
      const r = await api.apiServer.logsRecent({
        // D-14：显式天数优先，未传时回落 daysRef（避免回调闭包旧值）
        days: d ?? daysRef.current,
        page: p,
        pageSize: RECENT_PAGE_SIZE,
        pool: pf === 'all' ? undefined : pf,
        result: rf === 'all' ? undefined : (rf as 'ok' | 'fail'),
      });
      if (reqIdRef.current !== reqId) return;
      setRows(r.rows);
      setTotal(r.total);
      setTruncated(r.truncated);
      setPage(p);
    } catch {
      // 静默：查询失败保留上一页数据（日志面板非关键路径）
    } finally {
      if (reqIdRef.current === reqId) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load(1, pool, result);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const totalPages = Math.max(1, Math.ceil(total / RECENT_PAGE_SIZE));

  return (
    <div className="p-4">
      {/* 筛选条 */}
      <div className="mb-3 flex flex-wrap items-center gap-2 rounded-lg border border-slate-200/70 bg-slate-50/60 p-2.5 dark:border-zinc-700/50 dark:bg-zinc-800/30">
        <span className="text-xs text-slate-400">池</span>
        <div className="flex flex-wrap gap-1">
          {RECENT_POOLS.map((p) => (
            <button
              key={p.v}
              className={`rounded-md px-2 py-0.5 text-xs font-medium transition ${
                pool === p.v
                  ? 'bg-zinc-800 text-white dark:bg-zinc-200 dark:text-zinc-900'
                  : 'bg-white text-slate-500 hover:bg-slate-100 dark:bg-zinc-800 dark:text-zinc-400 dark:hover:bg-zinc-700'
              }`}
              disabled={loading}
              onClick={() => { setPool(p.v); void load(1, p.v, result); }}
            >
              {p.label}
            </button>
          ))}
        </div>
        <span className="ml-2 text-xs text-slate-400">结果</span>
        <div className="flex gap-1">
          {RECENT_RESULTS.map((r) => (
            <button
              key={r.v}
              className={`rounded-md px-2 py-0.5 text-xs font-medium transition ${
                result === r.v
                  ? 'bg-zinc-800 text-white dark:bg-zinc-200 dark:text-zinc-900'
                  : 'bg-white text-slate-500 hover:bg-slate-100 dark:bg-zinc-800 dark:text-zinc-400 dark:hover:bg-zinc-700'
              }`}
              disabled={loading}
              onClick={() => { setResult(r.v); void load(1, pool, r.v); }}
            >
              {r.label}
            </button>
          ))}
        </div>
        <span className="ml-2 text-xs text-slate-400">天数</span>
        <div className="flex gap-1">
          {RECENT_DAYS.map((d) => (
            <button
              key={d}
              className={`rounded-md px-2 py-0.5 text-xs font-medium transition ${
                days === d
                  ? 'bg-zinc-800 text-white dark:bg-zinc-200 dark:text-zinc-900'
                  : 'bg-white text-slate-500 hover:bg-slate-100 dark:bg-zinc-800 dark:text-zinc-400 dark:hover:bg-zinc-700'
              }`}
              disabled={loading}
              onClick={() => { setDays(d); daysRef.current = d; void load(1, pool, result, d); }}
            >
              {d}
            </button>
          ))}
        </div>
        <span className="ml-auto flex items-center gap-2 text-xs text-slate-400">
          {truncated && (
            <span className="rounded-md bg-amber-100 px-2 py-0.5 font-medium text-amber-700 dark:bg-amber-500/15 dark:text-amber-300">
              已达解析上限
            </span>
          )}
          共 {total.toLocaleString()} 条
          <button
            className="btn-ghost flex items-center gap-1 !p-1 text-xs"
            onClick={() => void load(page, pool, result)}
            disabled={loading}
            title="刷新"
          >
            <RefreshCw size={12} className={loading ? 'animate-spin' : ''} />
          </button>
        </span>
      </div>

      {/* 请求表 */}
      <div className="max-h-[480px] overflow-auto rounded-lg border border-slate-200/70 dark:border-zinc-700/50">
        <table className="w-full table-fixed text-left text-xs">
          <thead className="sticky top-0 bg-slate-50 text-[11px] text-slate-400 dark:bg-zinc-800/80 dark:text-zinc-500">
            <tr className="border-b border-slate-200 dark:border-zinc-700">
              <th className="w-[13%] px-2 py-2 font-medium">时间</th>
              <th className="w-[19%] px-2 py-2 font-medium">端点</th>
              <th className="w-[14%] px-2 py-2 font-medium">模型</th>
              <th className="w-[9%] px-2 py-2 font-medium">池</th>
              <th className="w-[12%] px-2 py-2 font-medium">账号</th>
              <th className="w-[7%] px-2 py-2 text-center font-medium">状态</th>
              <th className="w-[8%] px-2 py-2 text-right font-medium">耗时</th>
              <th className="w-[8%] px-2 py-2 text-right font-medium">TTFT</th>
              <th className="w-[10%] px-2 py-2 font-medium">失败原因</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r, i) => (
              <tr
                key={`${r.date}-${r.time}-${i}`}
                className="border-b border-slate-100 hover:bg-slate-50/70 dark:border-zinc-800 dark:hover:bg-zinc-800/40"
              >
                <td className="truncate px-2 py-1.5 font-mono text-slate-500 dark:text-zinc-400" title={`${r.date} ${r.time}`}>
                  {r.date.slice(5)} {r.time}
                </td>
                <td className="truncate px-2 py-1.5 font-mono text-slate-600 dark:text-zinc-300" title={`${r.method} ${r.path}${r.stream ? '（流式）' : ''}`}>
                  {r.path}
                  {r.stream && <span className="ml-1 text-[10px] text-sky-500">SSE</span>}
                </td>
                <td className="truncate px-2 py-1.5 text-slate-600 dark:text-zinc-300" title={r.model}>{r.model}</td>
                <td className="truncate px-2 py-1.5 text-slate-500 dark:text-zinc-400">{r.pool}</td>
                <td className="truncate px-2 py-1.5 text-slate-600 dark:text-zinc-300" title={`${r.acct}（${r.keyName}）`}>{r.acct}</td>
                <td className="px-2 py-1.5 text-center">
                  <span className={`inline-block rounded px-1.5 py-0.5 text-[10px] font-semibold ${statusChipCls(r.status)}`}>
                    {r.status}
                  </span>
                </td>
                <td className="px-2 py-1.5 text-right font-mono text-slate-500 dark:text-zinc-400">
                  {r.durationMs >= 1000 ? `${(r.durationMs / 1000).toFixed(1)}s` : `${r.durationMs}ms`}
                </td>
                <td className="px-2 py-1.5 text-right font-mono text-slate-400 dark:text-zinc-500">
                  {r.ttfbMs != null ? `${r.ttfbMs}ms` : '-'}
                </td>
                <td className="truncate px-2 py-1.5 text-rose-600 dark:text-rose-300" title={r.error ?? undefined}>
                  {r.error ?? ''}
                </td>
              </tr>
            ))}
            {loading && (
              <tr>
                <td colSpan={9} className="px-2 py-6 text-center text-slate-400">
                  加载中…
                </td>
              </tr>
            )}
            {rows.length === 0 && !loading && (
              <tr>
                <td colSpan={9} className="px-2 py-6 text-center text-slate-400">
                  {/* M-7：total 为筛选后计数，文案分场景——有筛选用筛选文案，无筛选才提天数 */}
                  {(pool !== 'all' || result !== 'all') ? '当前筛选无匹配请求' : `最近 ${days} 天无请求`}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {/* 分页条 */}
      {totalPages > 1 && (
        <div className="mt-2 flex items-center justify-end gap-2 text-xs text-slate-500 dark:text-zinc-400">
          <span>第 {page} / {totalPages} 页</span>
          <button
            className="btn-outline flex items-center gap-1 px-2 py-1 text-xs"
            disabled={page <= 1 || loading}
            onClick={() => void load(page - 1, pool, result)}
          >
            <ChevronLeft size={12} />
            上一页
          </button>
          <button
            className="btn-outline flex items-center gap-1 px-2 py-1 text-xs"
            disabled={page >= totalPages || loading}
            onClick={() => void load(page + 1, pool, result)}
          >
            下一页
            <ChevronRight size={12} />
          </button>
        </div>
      )}
    </div>
  );
}

/** 整日日志内容渲染软上限：超过时截断展示，避免超大 <pre> 卡死主线程 */
const MAX_API_LOG_CHARS = 200_000;

function ApiLogsTab() {
  const toast = useAppStore((s) => s.pushToast);

  // F-84：视图切换（结构化最近请求 / 原始整日文本日志）
  const [view, setView] = useState<'recent' | 'raw'>('recent');
  const [dates, setDates] = useState<string[]>([]);
  const [selected, setSelected] = useState('');
  const [content, setContent] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [debugEnabled, setDebugEnabled] = useState(false);
  const [togglingDebug, setTogglingDebug] = useState(false);
  const [kw, setKw] = useState('');
  const [startTime, setStartTime] = useState('');
  const [endTime, setEndTime] = useState('');
  const [searching, setSearching] = useState(false);
  const [filtered, setFiltered] = useState(false);

  // 当前选中日期的 ref：loadDates 不再依赖 selected 状态，避免 selected 变化导致 effect 二次拉取
  const selectedRef = useRef('');

  const loadDates = useCallback(async () => {
    setRefreshing(true);
    try {
      const d = await withMinDelay(api.apiServer.logsList());
      setDates(d);
      if (d.length > 0 && !selectedRef.current) {
        selectedRef.current = d[0];
        setSelected(d[0]);
        void loadDetail(d[0]);
      }
    } catch {
      // 失败可见化（审查 F1.3）：静默吞错让「故障」与「确无日志」不可区分
      toast('error', 'API 日志日期列表读取失败');
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const loadDetail = useCallback(async (date: string) => {
    setLoading(true);
    selectedRef.current = date;
    setSelected(date);
    setFiltered(false);
    setKw('');
    setStartTime('');
    setEndTime('');
    try {
      const c = await withMinDelay(api.apiServer.logsDetail(date));
      setContent(c);
    } catch {
      setContent(null);
      toast('error', `API 日志详情读取失败（${date}）`);
    } finally {
      setLoading(false);
    }
  }, []);

  const search = async () => {
    if (!selected) return;
    setSearching(true);
    setFiltered(true);
    try {
      const c = await withMinDelay(api.apiServer.logsSearch({
        date: selected,
        startTime: startTime.trim() || undefined,
        endTime: endTime.trim() || undefined,
        keyword: kw.trim() || undefined,
      }));
      setContent(c);
    } catch {
      setContent(null);
      toast('error', 'API 日志搜索失败');
    } finally {
      setSearching(false);
    }
  };

  const resetSearch = async () => {
    setKw('');
    setStartTime('');
    setEndTime('');
    setFiltered(false);
    await loadDetail(selected);
  };

  const toggleDebug = async () => {
    setTogglingDebug(true);
    try {
      const newVal = await withMinDelay(api.apiServer.debugToggle());
      setDebugEnabled(newVal);
      toast(newVal ? 'success' : 'info', `Debug 模式已${newVal ? '开启' : '关闭'}`);
    } catch (err) {
      toast('error', `切换 Debug 失败：${String(err)}`);
    } finally {
      setTogglingDebug(false);
    }
  };

  useEffect(() => {
    void loadDates();
    // Web 版网关常驻运行：Debug 状态直接读取（无需先探测服务启停）
    api.apiServer
      .debugStatus()
      .then(setDebugEnabled)
      .catch(() => {});
  }, [loadDates]);

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      {/* 工具栏 */}
      <div className="card overflow-hidden">
        <div className="flex items-center justify-between border-b border-slate-100 px-4 py-3 dark:border-zinc-800">
          <div className="flex items-center gap-2">
            <FileText size={16} className="text-slate-400 dark:text-zinc-500" />
            <h2 className="text-sm font-semibold text-slate-700 dark:text-zinc-200">API 请求日志</h2>
            {selected && (
              <span className="rounded-md bg-slate-100 px-2 py-0.5 text-xs font-medium text-slate-500 dark:bg-zinc-800 dark:text-zinc-400">
                {selected}
              </span>
            )}
            <span className="rounded-md bg-emerald-100 px-2 py-0.5 text-xs font-medium text-emerald-700 dark:bg-emerald-500/15 dark:text-emerald-300">
              网关运行中
            </span>
          </div>
          <div className="flex items-center gap-2">
            {/* F-84：视图切换（结构化 / 原始文本） */}
            <div className="flex overflow-hidden rounded-lg border border-slate-200 dark:border-zinc-700">
              <button
                className={`flex items-center gap-1 px-2 py-1 text-xs font-medium transition ${
                  view === 'recent'
                    ? 'bg-zinc-800 text-white dark:bg-zinc-200 dark:text-zinc-900'
                    : 'text-slate-500 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800'
                }`}
                onClick={() => setView('recent')}
                title="结构化最近请求（池/账号/结果/耗时/TTFT）"
              >
                <List size={12} />
                最近请求
              </button>
              <button
                className={`flex items-center gap-1 px-2 py-1 text-xs font-medium transition ${
                  view === 'raw'
                    ? 'bg-zinc-800 text-white dark:bg-zinc-200 dark:text-zinc-900'
                    : 'text-slate-500 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800'
                }`}
                onClick={() => setView('raw')}
                title="原始整日文本日志"
              >
                <ScrollText size={12} />
                原始日志
              </button>
            </div>
            <button
              className={`flex items-center gap-1 rounded-lg px-2.5 py-1 text-xs font-medium transition ${
                debugEnabled
                  ? 'bg-amber-100 text-amber-700 dark:bg-amber-500/15 dark:text-amber-300'
                  : 'text-slate-400 hover:bg-slate-100 hover:text-slate-600 dark:text-zinc-500 dark:hover:bg-zinc-800 dark:hover:text-zinc-300'
              } ${togglingDebug ? 'cursor-not-allowed opacity-50' : ''}`}
              onClick={() => void toggleDebug()}
              disabled={togglingDebug}
              title="开启后记录完整请求/响应信息"
            >
              <Bug size={13} className={togglingDebug ? 'animate-pulse' : ''} />
              {togglingDebug ? '切换中…' : `Debug ${debugEnabled ? 'ON' : 'OFF'}`}
            </button>
            <button
              className="btn-ghost flex items-center gap-1 text-xs"
              onClick={() => void loadDates()}
              disabled={refreshing}
            >
              <RefreshCw size={13} className={refreshing ? 'animate-spin' : ''} />
              {refreshing ? '刷新中…' : '刷新'}
            </button>
          </div>
        </div>

        {view === 'recent' ? (
          <ApiRecentRequests />
        ) : dates.length === 0 ? (
          <p className="py-12 text-center text-sm text-slate-400">暂无日志</p>
        ) : (
          <div className="p-4">
            {/* 日期选择 */}
            <div className="mb-3 flex flex-wrap gap-1.5">
              {dates.map((d) => (
                <button
                  key={d}
                  className={`rounded-lg px-2.5 py-1 text-xs font-medium transition-all ${
                    selected === d
                      ? 'bg-zinc-800 text-white shadow-soft dark:bg-zinc-200 dark:text-zinc-900'
                      : 'bg-slate-100 text-slate-500 hover:bg-slate-200 dark:bg-zinc-800 dark:text-zinc-400 dark:hover:bg-zinc-700'
                  }`}
                  onClick={() => void loadDetail(d)}
                >
                  {d}
                </button>
              ))}
            </div>

            {/* 搜索栏 */}
            <div className="mb-3 flex flex-wrap items-center gap-2 rounded-lg border border-slate-200/70 bg-slate-50/60 p-2.5 dark:border-zinc-700/50 dark:bg-zinc-800/30">
              <div className="flex items-center gap-1">
                <input
                  type="time"
                  value={startTime}
                  onChange={(e) => setStartTime(e.target.value)}
                  className="input h-8 w-28 py-0 text-xs"
                  placeholder="开始"
                />
                <span className="text-xs text-slate-400">→</span>
                <input
                  type="time"
                  value={endTime}
                  onChange={(e) => setEndTime(e.target.value)}
                  className="input h-8 w-28 py-0 text-xs"
                  placeholder="结束"
                />
              </div>
              <div className="relative flex-1 min-w-[140px]">
                <Search size={13} className="absolute left-2 top-1/2 -translate-y-1/2 text-slate-400" />
                <input
                  type="text"
                  value={kw}
                  onChange={(e) => setKw(e.target.value)}
                  onKeyDown={(e) => { if (e.key === 'Enter') void search(); }}
                  className="input h-8 w-full py-0 pl-7 pr-3 text-xs"
                  placeholder="关键字搜索（不区分大小写）"
                />
              </div>
              <button
                className="btn-primary h-8 px-3 py-0 text-xs"
                onClick={() => void search()}
                disabled={searching || !selected}
              >
                <Search size={12} />
                {searching ? '搜索中…' : '搜索'}
              </button>
              {filtered && (
                <button
                  className="btn-outline h-8 px-3 py-0 text-xs"
                  onClick={() => void resetSearch()}
                  disabled={searching}
                >
                  <X size={12} />
                  清除
                </button>
              )}
              {filtered && (
                <span className="rounded-md bg-amber-100 px-2 py-0.5 text-xs font-medium text-amber-700 dark:bg-amber-500/15 dark:text-amber-300">
                  已过滤
                </span>
              )}
            </div>

            {/* 日志内容 */}
            <div className="max-h-[480px] overflow-auto rounded-lg bg-slate-50 p-3 dark:bg-zinc-900/50">
              {loading || searching ? (
                <p className="py-4 text-center text-sm text-slate-400">{searching ? '搜索中…' : '加载中…'}</p>
              ) : content ? (
                <>
                  {content.length > MAX_API_LOG_CHARS && (
                    <p className="mb-2 rounded-lg border border-amber-200 bg-amber-50 px-2 py-1 text-xs text-amber-700 dark:border-amber-700 dark:bg-amber-900/20 dark:text-amber-300">
                      日志过长（共 {content.length.toLocaleString()} 字符），仅显示前 200,000 字符。请用关键字 / 时间段过滤缩小范围。
                    </p>
                  )}
                  <pre className="whitespace-pre-wrap break-all font-mono text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
                    {content.length > MAX_API_LOG_CHARS ? content.slice(0, MAX_API_LOG_CHARS) : content}
                  </pre>
                </>
              ) : (
                <p className="py-4 text-center text-sm text-slate-400">无内容</p>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

// ======================== 主页面 ========================

export default function Logs({ embedded = false }: { embedded?: boolean }) {
  const [tab, setTab] = useState<'system' | 'api'>('system');

  return (
    <div className="flex h-full animate-fade-in flex-col">
      {!embedded && (
        <PageHeader
          title="系统日志"
          desc="查看全部应用的运行日志与 API 请求日志"
        />
      )}

      {/* Tab 切换 — 分段控件风格 */}
      <div className="mb-3 inline-flex items-center gap-1 rounded-xl border border-slate-200/80 bg-slate-50/80 p-1 dark:border-zinc-700/60 dark:bg-zinc-800/40">
        <button
          onClick={() => setTab('system')}
          className={`flex items-center gap-1.5 rounded-lg px-3.5 py-1.5 text-sm font-medium transition-all duration-200 ${
            tab === 'system'
              ? 'bg-white text-zinc-800 shadow-soft dark:bg-zinc-700 dark:text-zinc-50'
              : 'text-slate-500 hover:text-slate-700 dark:text-zinc-400 dark:hover:text-zinc-200'
          }`}
        >
          运行日志
        </button>
        <button
          onClick={() => setTab('api')}
          className={`flex items-center gap-1.5 rounded-lg px-3.5 py-1.5 text-sm font-medium transition-all duration-200 ${
            tab === 'api'
              ? 'bg-white text-zinc-800 shadow-soft dark:bg-zinc-700 dark:text-zinc-50'
              : 'text-slate-500 hover:text-slate-700 dark:text-zinc-400 dark:hover:text-zinc-200'
          }`}
        >
          <FileText size={15} />
          API 请求日志
        </button>
      </div>

      {tab === 'system' && <SystemLogsTab />}
      {tab === 'api' && <ApiLogsTab />}
    </div>
  );
}
