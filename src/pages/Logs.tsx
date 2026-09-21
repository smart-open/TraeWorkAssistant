import { useEffect, useState, useCallback, useRef } from 'react';
import { RefreshCw, Search, Trash2, Copy, Bug, FileText, X } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { EmptyState, Modal } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import { withMinDelay } from '../lib/delay';
import { copyText } from '../lib/clipboard';

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

/** 整日日志内容渲染软上限：超过时截断展示，避免超大 <pre> 卡死主线程 */
const MAX_API_LOG_CHARS = 200_000;

function ApiLogsTab() {
  const toast = useAppStore((s) => s.pushToast);

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
      /* ignore */
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

        {dates.length === 0 ? (
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
          title="Trae · 系统日志"
          desc="查看运行日志与 API 请求日志"
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
