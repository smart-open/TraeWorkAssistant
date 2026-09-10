import { useCallback, useEffect, useMemo, useState } from 'react';
import { RefreshCw, Activity, Server } from 'lucide-react';
import {
  Bar,
  Line,
  XAxis,
  YAxis,
  BarChart,
  ComposedChart,
  ResponsiveContainer,
  Tooltip,
  CartesianGrid,
  Legend,
} from 'recharts';
import { StatCard, Badge, Progress, EmptyState, Spinner } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import { useIsDark } from '../../lib/useIsDark';
import type { WbTokenStats, WbUsageOfficial, WbUsageModelPoint, WbTokenAgg, WbUsageFallback } from '../../types';

/**
 * Token 统计面板（F-57/F-58/F-25，批次3）：
 * 本地 JSONL 统计（四指标卡 + 构成堆叠条 + 双轴趋势 + 年度热力图 + 模型排行）
 * + 官方请求用量（今日/近7天/本月 KPI + 按模型堆叠柱 + 官方模型排行）。
 * 数据源：workbuddy_token_stats（合并 ~/.workbuddy/projects 与 ~/.codebuddy/projects）
 * 与 workbuddy_usage_official（get-user-request-usage，口径标注为官方数据）。
 */

type RangeKey = 'today' | '7d' | '30d' | 'month' | 'year';

const RANGES: { key: RangeKey; label: string }[] = [
  { key: 'today', label: '今天' },
  { key: '7d', label: '近7天' },
  { key: '30d', label: '近30天' },
  { key: 'month', label: '本月' },
  { key: 'year', label: '近1年' },
];

const STACK_COLORS = {
  cache_read: '#6366f1', // 缓存读取
  new_input: '#38bdf8', // 新增输入
  output: '#22c55e', // 输出
  cache_write: '#f59e0b', // 缓存写入
};

const fmtDate = (d: Date) =>
  `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
const addDays = (d: Date, n: number) => new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);

function fmtTokens(n: number): string {
  if (!Number.isFinite(n)) return '0';
  if (Math.abs(n) >= 1e8) return `${(n / 1e8).toFixed(2)} 亿`;
  if (Math.abs(n) >= 1e4) return `${(n / 1e4).toFixed(1)} 万`;
  return n.toLocaleString('zh-CN');
}

const zero = () => ({ input: 0, output: 0, cache_read: 0, cache_write: 0, calls: 0 });
type Agg = ReturnType<typeof zero>;

type HeatCell = { date: string; total: number };
type HeatMonth = { idx: number; label: string };

function addPoint(acc: Agg, p: WbTokenAgg) {
  acc.input += p.input ?? 0;
  acc.output += p.output ?? 0;
  acc.cache_read += p.cache_read ?? 0;
  acc.cache_write += p.cache_write ?? 0;
  acc.calls += p.calls ?? 0;
}

export default function TokenStatsPanel({ remainingCredits }: { remainingCredits: number | null }) {
  const pushToast = useAppStore((s) => s.pushToast);
  const isDark = useIsDark();
  const [stats, setStats] = useState<WbTokenStats | null>(null);
  const [statsLoading, setStatsLoading] = useState(false);
  const [usage, setUsage] = useState<WbUsageOfficial | null>(null);
  const [fallback, setFallback] = useState<WbUsageFallback | null>(null);
  const [usageLoading, setUsageLoading] = useState(false);
  const [usageError, setUsageError] = useState('');
  const [range, setRange] = useState<RangeKey>('7d');
  const [modelFilter, setModelFilter] = useState('');

  const loadStats = useCallback(async () => {
    setStatsLoading(true);
    try {
      const r = await withMinDelay(api.workbuddy.tokenStats(), 800);
      setStats(r);
    } catch (err) {
      pushToast('error', `本地 Token 统计失败：${String(err)}`);
    } finally {
      setStatsLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const loadUsage = useCallback(
    async (fresh = false) => {
      setUsageLoading(true);
      setUsageError('');
      try {
        const r = await api.workbuddy.usageOfficial(undefined, fresh);
        setUsage(r);
        setFallback(null);
      } catch (err) {
        // 官方用量不可用 → 自动切换快照回退数据源（F-27），仍失败才报错
        setUsage(null);
        try {
          const fb = await api.workbuddy.usageFallback();
          setFallback(fb);
          setUsageError('');
        } catch {
          setUsageError(String(err));
          setFallback(null);
        }
      } finally {
        setUsageLoading(false);
      }
    },
    [],
  );

  useEffect(() => {
    void loadStats();
    void loadUsage(false);
  }, [loadStats, loadUsage]);

  // 范围窗口（本地日期字符串比较，ISO 格式可直接比较）
  const { startStr, todayStr, dateList } = useMemo(() => {
    const today = new Date();
    const t = fmtDate(today);
    let s = t;
    switch (range) {
      case 'today':
        s = t;
        break;
      case '7d':
        s = fmtDate(addDays(today, -6));
        break;
      case '30d':
        s = fmtDate(addDays(today, -29));
        break;
      case 'month':
        s = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-01`;
        break;
      case 'year':
        s = fmtDate(addDays(today, -364));
        break;
    }
    const list: string[] = [];
    if (range !== 'year') {
      let cur = new Date(s.replace(/-/g, '/'));
      while (fmtDate(cur) <= t) {
        list.push(fmtDate(cur));
        cur = addDays(cur, 1);
      }
    }
    return { startStr: s, todayStr: t, dateList: list };
  }, [range]);

  const inRange = useCallback((date?: string) => !!date && date >= startStr && date <= todayStr, [startStr, todayStr]);

  // 范围 × 模型筛选 聚合（数据源 = 天 × 模型粒度）
  const modelAgg = useMemo(() => {
    const map = new Map<string, Agg>();
    if (!stats) return map;
    for (const [model, points] of Object.entries(stats.daily_by_model)) {
      if (modelFilter && model !== modelFilter) continue;
      for (const p of points) {
        if (!inRange(p.date)) continue;
        if (!map.has(model)) map.set(model, zero());
        addPoint(map.get(model)!, p);
      }
    }
    return map;
  }, [stats, range, modelFilter, inRange]);

  const totals = useMemo(() => {
    const t = zero();
    for (const v of modelAgg.values()) {
      t.input += v.input;
      t.output += v.output;
      t.cache_read += v.cache_read;
      t.cache_write += v.cache_write;
      t.calls += v.calls;
    }
    return t;
  }, [modelAgg]);

  const totalTokens = totals.input + totals.output + totals.cache_write;
  const hitRate = totals.input > 0 ? totals.cache_read / totals.input : null;

  // 双轴趋势数据（堆叠柱四类构成 + 调用次数虚线）
  const chartData = useMemo(() => {
    if (!stats) return [];
    const byDate = new Map<string, Agg>();
    for (const [model, points] of Object.entries(stats.daily_by_model)) {
      if (modelFilter && model !== modelFilter) continue;
      for (const p of points) {
        const d0 = p.date;
        if (!d0 || !inRange(d0)) continue;
        if (!byDate.has(d0)) byDate.set(d0, zero());
        addPoint(byDate.get(d0)!, p);
      }
    }
    const src = range === 'year' ? [...byDate.keys()].sort() : dateList;
    return src.map((d) => {
      const a = byDate.get(d) ?? zero();
      return {
        date: d.slice(5).replace('-', '/'),
        cache_read: a.cache_read,
        new_input: Math.max(0, a.input - a.cache_read),
        output: a.output,
        cache_write: a.cache_write,
        calls: a.calls,
      };
    });
  }, [stats, range, modelFilter, inRange, dateList]);

  // 年度热力图（GitHub 风格：周列 × 7 行）
  const heatmap = useMemo(() => {
    if (!stats) {
      return { weeks: [] as HeatCell[][], max: 0, months: [] as HeatMonth[] };
    }
    const dailyMap = new Map<string, number>();
    const fill = (pts: WbTokenAgg[]) => pts.forEach((p) => dailyMap.set(p.date ?? '', (dailyMap.get(p.date ?? '') ?? 0) + (p.total ?? 0)));
    if (modelFilter) fill(stats.daily_by_model[modelFilter] ?? []);
    else fill(stats.daily);
    const today = new Date();
    const end = today;
    // 起点对齐到 end 所在周的周一（行 0=周一 … 6=周日），回退 52 周
    const weekday = (end.getDay() + 6) % 7;
    const gridEnd = addDays(end, 6 - weekday);
    const gridStart = addDays(gridEnd, -7 * 53 + 1);
    const weeks: { date: string; total: number }[][] = [];
    const months: { idx: number; label: string }[] = [];
    let lastMonth = -1;
    for (let w = 0; w < 53; w++) {
      const col: { date: string; total: number }[] = [];
      for (let d = 0; d < 7; d++) {
        const cur = addDays(gridStart, w * 7 + d);
        const key = fmtDate(cur);
        col.push({ date: key, total: dailyMap.get(key) ?? 0 });
        if (d === 0 && cur.getMonth() !== lastMonth) {
          lastMonth = cur.getMonth();
          months.push({ idx: w, label: `${lastMonth + 1}月` });
        }
      }
      weeks.push(col);
    }
    const max = Math.max(0, ...[...dailyMap.values()]);
    return { weeks, max, months };
  }, [stats, modelFilter]);

  const heatColor = (v: number) => {
    if (v <= 0) return isDark ? '#27272a' : '#e2e8f0';
    const ratio = heatmap.max > 0 ? v / heatmap.max : 0;
    if (ratio > 0.75) return '#15803d';
    if (ratio > 0.5) return '#22c55e';
    if (ratio > 0.25) return '#4ade80';
    return '#bbf7d0';
  };

  // 模型排行（范围内跨模型，Top8 + 其余合计）
  const ranking = useMemo(() => {
    const arr = [...modelAgg.entries()]
      .map(([model, a]) => ({ model, total: a.input + a.output + a.cache_write, calls: a.calls }))
      .sort((a, b) => b.total - a.total);
    const grand = arr.reduce((s, x) => s + x.total, 0);
    return { top: arr.slice(0, 8), rest: arr.slice(8), grand };
  }, [modelAgg]);

  // 官方用量：按模型堆叠柱（Top6 + 其他）
  const officialChart = useMemo(() => {
    if (!usage?.daily?.length) return [];
    const modelTotals = new Map<string, number>();
    usage.models.forEach((m) => modelTotals.set(m.model, m.credit));
    const top = [...modelTotals.entries()].sort((a, b) => b[1] - a[1]).slice(0, 6).map(([m]) => m);
    return usage.daily.map((d) => {
      const row: Record<string, number | string> = { date: d.date.slice(5).replace('-', '/') };
      let other = 0;
      for (const m of d.models ?? []) {
        if (top.includes(m.model)) row[m.model] = (row[m.model] as number ?? 0) + m.credit;
        else other += m.credit;
      }
      if (other > 0) row['其他'] = other;
      return row;
    });
  }, [usage]);

  const officialModels = usage?.models ?? [];
  const officialGrand = officialModels.reduce((s, m) => s + m.credit, 0);
  const officialTop = [...officialModels].sort((a, b) => b.credit - a.credit).slice(0, 8);
  const officialRest = officialModels.slice(8).reduce((s, m) => s + m.credit, 0);

  const tooltipStyle = {
    fontSize: 12,
    borderRadius: 10,
    border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
    background: isDark ? '#18181b' : '#fff',
    color: isDark ? '#e4e4e7' : '#1e293b',
    boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
    padding: '8px 12px',
  } as const;

  const axisProps = {
    tick: { fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' },
    axisLine: { stroke: isDark ? '#3f3f46' : '#e2e8f0' },
    tickLine: false,
  } as const;

  const chartEmpty = chartData.every((d) => d.calls === 0);

  return (
    <div className="space-y-4">
      {/* 工具行：范围 + 模型筛选 + 刷新 */}
      <div className="card p-4">
        <div className="flex flex-wrap items-center gap-2">
          <Activity size={16} className="text-indigo-500" />
          <span className="text-sm font-medium">本地 Token 统计</span>
          <Badge tone="slate">{stats?.files_scanned ?? '—'} 个会话文件</Badge>
          {(stats?.parse_errors ?? 0) > 0 && <Badge tone="amber">{stats?.parse_errors} 行解析失败</Badge>}
          <div className="ml-auto flex flex-wrap items-center gap-2">
            <select
              className="input !w-auto !py-1 text-xs"
              value={modelFilter}
              onChange={(e) => setModelFilter(e.target.value)}
            >
              <option value="">全部模型</option>
              {(stats?.models ?? []).map((m) => (
                <option key={m.key} value={m.key}>
                  {m.key}
                </option>
              ))}
            </select>
            <div className="flex rounded-lg bg-slate-100 p-1 dark:bg-zinc-900">
              {RANGES.map((r) => (
                <button
                  key={r.key}
                  className={`rounded-md px-2.5 py-1 text-xs font-medium ${range === r.key ? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-800 dark:text-zinc-100' : 'text-slate-500 dark:text-zinc-400'}`}
                  onClick={() => setRange(r.key)}
                >
                  {r.label}
                </button>
              ))}
            </div>
            <button className="btn-outline !px-2 !py-1 text-xs" onClick={() => void loadStats()} disabled={statsLoading}>
              <RefreshCw size={13} className={statsLoading ? 'animate-spin' : ''} /> 重扫
            </button>
          </div>
        </div>

        {/* 四指标卡（F-57 ①） */}
        <div className="mt-4 grid grid-cols-2 gap-4 xl:grid-cols-4">
          <StatCard label="总 Token" value={fmtTokens(totalTokens)} hint={`${totals.calls} 次调用`} tone="brand" />
          <StatCard label="输入 Token" value={fmtTokens(totals.input)} hint={`其中缓存读取 ${fmtTokens(totals.cache_read)}`} tone="violet" />
          <StatCard label="输出 Token" value={fmtTokens(totals.output)} hint={`缓存写入 ${fmtTokens(totals.cache_write)}`} tone="green" />
          <StatCard
            label="缓存命中率"
            value={hitRate != null ? `${(hitRate * 100).toFixed(1)}%` : '—'}
            hint="口径：缓存读取 / 输入总量"
            tone="amber"
          />
        </div>

        {/* Token 构成堆叠条（F-57 ②） */}
        <div className="mt-4">
          <div className="mb-1.5 text-xs font-medium text-slate-500">Token 构成（{RANGES.find((r) => r.key === range)?.label}）</div>
          {totalTokens === 0 ? (
            <div className="rounded-lg bg-slate-50 py-6 text-center text-xs text-slate-400 dark:bg-zinc-900">该范围内暂无本地 Token 记录</div>
          ) : (
            <>
              <div className="flex h-3 w-full overflow-hidden rounded-full bg-slate-200 dark:bg-zinc-800">
                {[
                  { k: 'cache_read' as const, v: totals.cache_read, label: '缓存读取' },
                  { k: 'new_input' as const, v: totals.input - totals.cache_read, label: '新增输入' },
                  { k: 'output' as const, v: totals.output, label: '输出' },
                  { k: 'cache_write' as const, v: totals.cache_write, label: '缓存写入' },
                ].map((seg) => (
                  <div
                    key={seg.k}
                    style={{ width: `${(seg.v / totalTokens) * 100}%`, background: STACK_COLORS[seg.k] }}
                    title={`${seg.label}：${fmtTokens(seg.v)}（${((seg.v / totalTokens) * 100).toFixed(1)}%）`}
                  />
                ))}
              </div>
              <div className="mt-1.5 flex flex-wrap gap-3 text-xs text-slate-500">
                {[
                  { v: totals.cache_read, label: '缓存读取', color: STACK_COLORS.cache_read },
                  { v: totals.input - totals.cache_read, label: '新增输入', color: STACK_COLORS.new_input },
                  { v: totals.output, label: '输出', color: STACK_COLORS.output },
                  { v: totals.cache_write, label: '缓存写入', color: STACK_COLORS.cache_write },
                ].map((seg) => (
                  <span key={seg.label} className="flex items-center gap-1">
                    <span className="inline-block h-2 w-2 rounded-full" style={{ background: seg.color }} />
                    {seg.label} {fmtTokens(seg.v)}
                  </span>
                ))}
              </div>
            </>
          )}
        </div>
      </div>

      {/* 双轴趋势图（F-57 ③） */}
      <div className="card p-4">
        <div className="mb-2 text-sm font-medium">Token 与调用趋势</div>
        {chartEmpty ? (
          <EmptyState icon={<Activity size={22} />} title="暂无趋势数据" hint="本地会话产生用量后这里会展示逐日 Token 构成与调用次数。" />
        ) : (
          <div className="h-64">
            <ResponsiveContainer>
              <ComposedChart data={chartData} margin={{ top: 12, right: 8, left: 0, bottom: 4 }}>
                <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                <XAxis dataKey="date" {...axisProps} minTickGap={24} />
                <YAxis yAxisId="tokens" {...axisProps} axisLine={false} width={56} tickFormatter={(v: number) => fmtTokens(v)} />
                <YAxis yAxisId="calls" orientation="right" {...axisProps} axisLine={false} width={40} />
                <Tooltip contentStyle={tooltipStyle} formatter={(v: number, name: string) => [name === 'calls' ? String(v) : fmtTokens(v), ({ cache_read: '缓存读取', new_input: '新增输入', output: '输出', cache_write: '缓存写入', calls: '调用次数' } as Record<string, string>)[name] ?? name]} />
                <Legend wrapperStyle={{ fontSize: 12 }} />
                <Bar yAxisId="tokens" dataKey="cache_read" stackId="t" fill={STACK_COLORS.cache_read} maxBarSize={22} />
                <Bar yAxisId="tokens" dataKey="new_input" stackId="t" fill={STACK_COLORS.new_input} maxBarSize={22} />
                <Bar yAxisId="tokens" dataKey="output" stackId="t" fill={STACK_COLORS.output} maxBarSize={22} />
                <Bar yAxisId="tokens" dataKey="cache_write" stackId="t" fill={STACK_COLORS.cache_write} maxBarSize={22} />
                <Line yAxisId="calls" type="monotone" dataKey="calls" stroke="#f43f5e" strokeWidth={1.5} strokeDasharray="5 3" dot={false} />
              </ComposedChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>

      {/* 年度活动热力图（F-57 ④） */}
      <div className="card p-4">
        <div className="mb-2 flex items-center justify-between">
          <span className="text-sm font-medium">年度活动热力图</span>
          <span className="flex items-center gap-1 text-xs text-slate-400">
            少
            {['#e2e8f0', '#bbf7d0', '#4ade80', '#22c55e', '#15803d'].map((c) => (
              <span key={c} className="inline-block h-2.5 w-2.5 rounded-sm" style={{ background: isDark && c === '#e2e8f0' ? '#27272a' : c }} />
            ))}
            多
          </span>
        </div>
        {heatmap.max === 0 ? (
          <div className="rounded-lg bg-slate-50 py-6 text-center text-xs text-slate-400 dark:bg-zinc-900">最近一年暂无本地会话用量</div>
        ) : (
          <div className="overflow-x-auto pb-1">
            <div className="min-w-max">
              <div className="relative mb-1 h-4" style={{ marginLeft: 28 }}>
                {heatmap.months.map((m) => (
                  <span key={`${m.idx}-${m.label}`} className="absolute text-[10px] text-slate-400" style={{ left: m.idx * 12 }}>
                    {m.label}
                  </span>
                ))}
              </div>
              <div className="flex gap-0.5">
                <div className="mr-1 flex w-6 flex-col justify-between py-0 text-[9px] text-slate-400">
                  <span>一</span>
                  <span>三</span>
                  <span>五</span>
                </div>
                {heatmap.weeks.map((week, wi) => (
                  <div key={wi} className="flex flex-col gap-0.5">
                    {week.map((cell) => (
                      <span
                        key={cell.date}
                        title={`${cell.date}：${fmtTokens(cell.total)} tokens`}
                        className="h-[10px] w-[10px] rounded-[2px]"
                        style={{ background: heatColor(cell.total) }}
                      />
                    ))}
                  </div>
                ))}
              </div>
            </div>
          </div>
        )}
      </div>

      {/* 官方请求用量（F-25/F-58）：口径=官方接口 */}
      <div className="card p-4">
        <div className="mb-3 flex flex-wrap items-center gap-2">
          <Server size={16} className="text-emerald-500" />
          <span className="text-sm font-medium">官方请求用量</span>
          <Badge tone="slate">来自 WorkBuddy 官方请求用量</Badge>
          {usage && (
            <span className="text-xs text-slate-400">
              {usage.range_start.slice(5).replace('-', '/')} ~ {usage.range_end.slice(5).replace('-', '/')} · {usage.request_count_total} 次请求
            </span>
          )}
          <button className="btn-outline ml-auto !px-2 !py-1 text-xs" onClick={() => void loadUsage(true)} disabled={usageLoading}>
            {usageLoading ? <Spinner /> : <RefreshCw size={13} className={usageLoading ? 'animate-spin' : ''} />} 刷新
          </button>
        </div>

        {usageError ? (
          <div className="rounded-lg bg-rose-50 p-3 text-xs text-rose-600 dark:bg-rose-500/10 dark:text-rose-300">
            官方用量查询失败：{usageError}
            <div className="mt-1 text-slate-400">请确认账号已录入凭证且在有效期内；本地 Token 统计不受影响。</div>
          </div>
        ) : !usage && fallback ? (
          <>
            {/* 快照回退数据源（F-27）：官方不可用时自动切换，口径明示 */}
            <div className="mb-3 flex flex-wrap items-center gap-2">
              <Badge tone="amber">快照回退数据源</Badge>
              <span className="text-xs text-slate-400">{fallback.note} · 本地时序 {fallback.snapshot_days} 天</span>
            </div>
            <div className="grid grid-cols-2 gap-4 xl:grid-cols-4">
              <StatCard label="剩余积分" value={remainingCredits != null ? remainingCredits.toFixed(2) : '—'} hint="来自积分三件套缓存" tone="amber" />
              <StatCard label="今日消耗" value={fallback.summary.usage_today.toFixed(2)} hint="快照推导" tone="red" />
              <StatCard label="近 7 天" value={fallback.summary.usage_7days.toFixed(2)} hint="快照推导" tone="violet" />
              <StatCard label="本月" value={fallback.summary.usage_this_month.toFixed(2)} hint="快照推导" tone="brand" />
            </div>
            <div className="mt-4 h-48">
              <ResponsiveContainer>
                <BarChart data={fallback.daily.map((d) => ({ date: d.date.slice(5), usage: d.usage }))} margin={{ top: 8, right: 8, left: 0, bottom: 4 }}>
                  <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                  <XAxis dataKey="date" {...axisProps} minTickGap={24} />
                  <YAxis {...axisProps} axisLine={false} width={48} />
                  <Tooltip contentStyle={tooltipStyle} formatter={(v: number) => [v.toFixed(2), '积分']} />
                  <Bar dataKey="usage" name="每日消耗" fill="#f59e0b" maxBarSize={22} />
                </BarChart>
              </ResponsiveContainer>
            </div>
            <div className="mt-2 text-xs text-slate-400">
              推导口径：当日消耗 = 前一日总余额 − 当日总余额 + 当日签到奖励；充值包到账日显示为 0，精确请求数请以官方口径为准。
            </div>
          </>
        ) : !usage ? (
          <div className="py-6 text-center text-xs text-slate-400">{usageLoading ? '加载中…' : '暂无官方用量数据'}</div>
        ) : (
          <>
            {/* 四 KPI 卡（F-58：剩余/今日消耗/近7天/本月） */}
            <div className="grid grid-cols-2 gap-4 xl:grid-cols-4">
              <StatCard label="剩余积分" value={remainingCredits != null ? remainingCredits.toFixed(2) : '—'} hint="来自积分三件套缓存" tone="amber" />
              <StatCard label="今日消耗" value={usage.summary.usage_today.toFixed(2)} hint="官方口径" tone="red" />
              <StatCard label="近 7 天" value={usage.summary.usage_7days.toFixed(2)} hint="官方口径" tone="violet" />
              <StatCard label="本月" value={usage.summary.usage_this_month.toFixed(2)} hint="官方口径" tone="brand" />
            </div>

            {/* 官方消耗堆叠柱（按模型，近 31 天） */}
            <div className="mt-4 h-56">
              <ResponsiveContainer>
                <BarChart data={officialChart} margin={{ top: 8, right: 8, left: 0, bottom: 4 }}>
                  <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                  <XAxis dataKey="date" {...axisProps} minTickGap={24} />
                  <YAxis {...axisProps} axisLine={false} width={48} />
                  <Tooltip contentStyle={tooltipStyle} formatter={(v: number, name: string) => [v.toFixed(2), name]} />
                  <Legend wrapperStyle={{ fontSize: 12 }} />
                  {Object.keys(
                    officialChart.reduce<Record<string, boolean>>((acc, row) => {
                      Object.keys(row).forEach((k) => {
                        if (k !== 'date') acc[k] = true;
                      });
                      return acc;
                    }, {}),
                  ).map((k, i) => (
                    <Bar key={k} dataKey={k} stackId="u" fill={['#6366f1', '#22c55e', '#f59e0b', '#38bdf8', '#a855f7', '#f43f5e', '#94a3b8'][i % 7]} maxBarSize={22} />
                  ))}
                </BarChart>
              </ResponsiveContainer>
            </div>

            {/* 官方模型排行 Top8（请求数/合计积分/占比） */}
            <div className="mt-3 space-y-2">
              {officialTop.length === 0 ? (
                <div className="text-xs text-slate-400">近 31 天暂无官方请求记录</div>
              ) : (
                officialTop.map((m: WbUsageModelPoint) => (
                  <div key={m.model} className="rounded-lg border border-slate-100 px-3 py-2 dark:border-zinc-800">
                    <div className="flex items-center justify-between gap-2 text-sm">
                      <span className="truncate font-medium">{m.model}</span>
                      <span className="shrink-0 tabular-nums text-xs text-slate-400">
                        {m.request_count} 次 · {m.credit.toFixed(2)} 积分
                      </span>
                    </div>
                    <Progress value={m.credit} max={officialGrand} />
                  </div>
                ))
              )}
              {(officialTop.length > 0 && (officialRest > 0 || officialModels.length > 8)) && (
                <div className="px-3 text-xs text-slate-400">
                  其余 {officialModels.length - 8} 个模型合计 {officialRest.toFixed(2)} 积分 · 全部模型合计 {officialGrand.toFixed(2)}
                </div>
              )}
            </div>
          </>
        )}
      </div>

      {/* 本地模型排行（范围内 Top8 + 其余合计，F-57/58 补充口径） */}
      <div className="card p-4">
        <div className="mb-2 flex items-center gap-2">
          <span className="text-sm font-medium">本地模型排行</span>
          <Badge tone="slate">{RANGES.find((r) => r.key === range)?.label}</Badge>
          {modelFilter && <Badge tone="amber">已筛选：{modelFilter}</Badge>}
        </div>
        {ranking.top.length === 0 ? (
          <div className="text-xs text-slate-400">该范围内暂无本地 Token 记录</div>
        ) : (
          <div className="space-y-2">
            {ranking.top.map((m) => (
              <div key={m.model} className="rounded-lg border border-slate-100 px-3 py-2 dark:border-zinc-800">
                <div className="flex items-center justify-between gap-2 text-sm">
                  <span className="truncate font-medium">{m.model}</span>
                  <span className="shrink-0 tabular-nums text-xs text-slate-400">
                    {m.calls} 次 · {fmtTokens(m.total)} tokens
                  </span>
                </div>
                <Progress value={m.total} max={ranking.grand} />
              </div>
            ))}
            {ranking.rest.length > 0 && (
              <div className="px-3 text-xs text-slate-400">
                其余 {ranking.rest.length} 个模型合计 {fmtTokens(ranking.rest.reduce((s, x) => s + x.total, 0))} tokens · 全部合计 {fmtTokens(ranking.grand)}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
