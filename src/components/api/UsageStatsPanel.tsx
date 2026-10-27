/**
 * 全局 API 管理 · 用量统计（unified-api-gateway-design §5.2/§5.3）
 * days（Trae 桶）/ wb_days（Buddy 桶）/ buddy_ai_days（BuddyAI 桶）/
 * custom_days（自定义模型桶）/ qoder_days（Qoder 桶）
 * 五桶聚合，参数化资源池筛选（全部/Trae/Buddy/BuddyAI/Qoder/自定义）；
 * 含模型分布 Top5（按所选池聚合）。
 * 数据源：api_usage_stats / api_wb_usage_stats / api_buddy_ai_usage_stats /
 * api_custom_usage_stats / api_qoder_usage_stats
 * （落盘数据，服务未运行也可查看，口径与键名不变 §9.6）。
 */
import { Fragment, useEffect, useMemo, useRef, useState } from 'react';
import { BarChart3, ChevronDown, ChevronRight, RefreshCw } from 'lucide-react';
import {
  Bar,
  BarChart,
  CartesianGrid,
  Legend,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';
import { StatCard } from '../ui';
import { api } from '../../lib/tauri';
import { fmtTokens } from '../../lib/format';
import { cn } from '../../lib/cn';
import { useAppStore } from '../../store';
import { isPoolVisible } from '../../lib/appVisibility';
import type {
  UsageCounterView,
  UsageDayView,
  UsageKeyTokenView,
  UsageModelLatencyView,
  UsageSessionView,
  UsageSessionsResult,
} from '../../types';

/** 资源池筛选（§5.2：全部/Trae/Buddy/BuddyAI 国际/Qoder/自定义模型） */
export type PoolFilter = 'all' | 'trae' | 'buddy' | 'buddy-ai' | 'qoder' | 'custom';

const FILTER_LABELS: Record<PoolFilter, string> = {
  all: '全部',
  trae: 'Trae',
  buddy: 'Buddy',
  'buddy-ai': 'BuddyAI',
  qoder: 'Qoder',
  custom: '自定义',
};

/** 同名计数器合并（模型/账号/Key 维度通用） */
export function mergeCounters(a: UsageCounterView[], b: UsageCounterView[]): UsageCounterView[] {
  const map = new Map<string, UsageCounterView>();
  for (const c of [...a, ...b]) {
    const e = map.get(c.name);
    if (e) {
      e.requests += c.requests;
      e.ok += c.ok;
      e.errors += c.errors;
      e.credits += c.credits;
    } else {
      map.set(c.name, { ...c });
    }
  }
  return [...map.values()];
}

/** 按 Key 的 token 用量合并 */
export function mergeKeyTokens(a: UsageKeyTokenView[], b: UsageKeyTokenView[]): UsageKeyTokenView[] {
  const map = new Map<string, UsageKeyTokenView>();
  for (const t of [...a, ...b]) {
    const e = map.get(t.name);
    if (e) {
      e.prompt_tokens += t.prompt_tokens;
      e.completion_tokens += t.completion_tokens;
    } else {
      map.set(t.name, { ...t });
    }
  }
  return [...map.values()];
}

/** 两条日记录合并（avg_duration 按请求数加权；TTFT 按样本数加权） */
export function mergeDay(a: UsageDayView, b: UsageDayView): UsageDayView {
  const total = a.total_requests + b.total_requests;
  const ttfbSamples = (a.ttfb_samples ?? 0) + (b.ttfb_samples ?? 0);
  const avgTtfb =
    ttfbSamples > 0
      ? Math.round(
          ((a.avg_ttfb_ms ?? 0) * (a.ttfb_samples ?? 0) +
            (b.avg_ttfb_ms ?? 0) * (b.ttfb_samples ?? 0)) /
            ttfbSamples,
        )
      : undefined;
  return {
    date: a.date,
    total_requests: total,
    ok: a.ok + b.ok,
    errors: a.errors + b.errors,
    stream_requests: a.stream_requests + b.stream_requests,
    prompt_tokens: a.prompt_tokens + b.prompt_tokens,
    completion_tokens: a.completion_tokens + b.completion_tokens,
    credits: a.credits + b.credits,
    original_credits: a.original_credits + b.original_credits,
    avg_duration_ms:
      total > 0
        ? Math.round(
            (a.avg_duration_ms * a.total_requests + b.avg_duration_ms * b.total_requests) / total,
          )
        : 0,
    p95_duration_ms: maxOpt(a.p95_duration_ms, b.p95_duration_ms),
    max_duration_ms: maxOpt(a.max_duration_ms, b.max_duration_ms),
    avg_ttfb_ms: avgTtfb,
    p95_ttfb_ms: maxOpt(a.p95_ttfb_ms, b.p95_ttfb_ms),
    ttfb_samples: ttfbSamples,
    models: mergeCounters(a.models, b.models),
    accounts: mergeCounters(a.accounts, b.accounts),
    keys: mergeCounters(a.keys, b.keys),
    key_tokens: mergeKeyTokens(a.key_tokens, b.key_tokens),
    model_latency: mergeModelLatency(a.model_latency, b.model_latency),
  };
}

/** 可选数值取较大者（P95/最大值跨桶合并取保守值） */
export function maxOpt(a?: number, b?: number): number | undefined {
  if (a == null && b == null) return undefined;
  return Math.max(a ?? 0, b ?? 0);
}

/** 多日可选 P95 取最大值（跨天聚合取保守值） */
export function maxOptList(values: (number | undefined)[]): number | undefined {
  const vs = values.filter((v): v is number => v != null);
  return vs.length > 0 ? Math.max(...vs) : undefined;
}

/** 按模型的延迟分位合并（F-76①） */
export function mergeModelLatency(
  a: UsageModelLatencyView[] = [],
  b: UsageModelLatencyView[] = [],
): UsageModelLatencyView[] {
  const map = new Map<string, UsageModelLatencyView>();
  for (const m of [...a, ...b]) {
    const e = map.get(m.model);
    if (e) {
      e.samples += m.samples;
      e.p50_duration_ms = maxOpt(e.p50_duration_ms, m.p50_duration_ms);
      e.p95_duration_ms = maxOpt(e.p95_duration_ms, m.p95_duration_ms);
      e.max_duration_ms = maxOpt(e.max_duration_ms, m.max_duration_ms);
      e.avg_ttfb_ms = maxOpt(e.avg_ttfb_ms, m.avg_ttfb_ms);
      e.p95_ttfb_ms = maxOpt(e.p95_ttfb_ms, m.p95_ttfb_ms);
    } else {
      map.set(m.model, { ...m });
    }
  }
  return [...map.values()].sort((x, y) => y.samples - x.samples);
}

/** 多桶按日合并（同日相加），按日期升序 */
export function mergeBuckets(...buckets: UsageDayView[][]): UsageDayView[] {
  const map = new Map<string, UsageDayView>();
  for (const bucket of buckets) {
    for (const d of bucket) {
      const cur = map.get(d.date);
      map.set(d.date, cur ? mergeDay(cur, d) : { ...d });
    }
  }
  return [...map.values()].sort((x, y) => x.date.localeCompare(y.date));
}

/** 本地日 YYYY-MM-DD（custom 会话明细区间端点） */
export function localDateStr(dt: Date): string {
  const p = (n: number) => String(n).padStart(2, '0');
  return `${dt.getFullYear()}-${p(dt.getMonth() + 1)}-${p(dt.getDate())}`;
}

/** 当日时刻 HH:MM:SS（custom 会话明细行时间列，自然日已在会话行体现） */
export function fmtClock(ts: number): string {
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

export default function UsageStatsPanel() {
  const [days, setDays] = useState(14);
  const [filter, setFilter] = useState<PoolFilter>('all');
  const [traeUsage, setTraeUsage] = useState<UsageDayView[]>([]);
  const [buddyUsage, setBuddyUsage] = useState<UsageDayView[]>([]);
  const [buddyAiUsage, setBuddyAiUsage] = useState<UsageDayView[]>([]);
  const [customUsage, setCustomUsage] = useState<UsageDayView[]>([]);
  const [qoderUsage, setQoderUsage] = useState<UsageDayView[]>([]);
  const [loading, setLoading] = useState(false);
  // M9 修复（审查 L30）：加载序号门控——快速切换 7/14/30 天时并发 load 的旧响应
  // 可能晚于新响应到达，用序号门控：响应到达时仅当仍是最新一次发起才 setState，
  // 防止旧数据覆盖新结果
  const loadSeqRef = useRef(0);

  const load = async (d: number) => {
    const seq = ++loadSeqRef.current;
    setLoading(true);
    try {
      const [trae, buddy, buddyAi, custom, qoder] = await Promise.all([
        api.apiServer.usageStats(d).catch(() => [] as UsageDayView[]),
        api.apiServer.wbUsageStats(d).catch(() => [] as UsageDayView[]),
        api.apiServer.buddyAiUsageStats(d).catch(() => [] as UsageDayView[]),
        api.apiServer.customUsageStats(d).catch(() => [] as UsageDayView[]),
        api.apiServer.qoderUsageStats(d).catch(() => [] as UsageDayView[]),
      ]);
      if (seq !== loadSeqRef.current) return;
      setTraeUsage(trae);
      setBuddyUsage(buddy);
      setBuddyAiUsage(buddyAi);
      setCustomUsage(custom);
      setQoderUsage(qoder);
    } finally {
      if (seq === loadSeqRef.current) setLoading(false);
    }
  };

  useEffect(() => {
    void load(days);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [days]);

  // 按筛选聚合多桶（"全部"时相加，§8.2）。
  // 应用显隐联动：当前筛选对应的应用被隐藏时回落"全部"（派生值，无额外 effect）；
  // 落盘用量照常统计，隐藏池数据仍计入"全部"聚合（账务数据不因 UI 隐藏消失）
  const settings = useAppStore((s) => s.settings);
  const effFilter: PoolFilter =
    filter !== 'all' && filter !== 'custom' && !isPoolVisible(settings, filter) ? 'all' : filter;
  const usage = useMemo(() => {
    if (effFilter === 'trae') return traeUsage;
    if (effFilter === 'buddy') return buddyUsage;
    if (effFilter === 'buddy-ai') return buddyAiUsage;
    if (effFilter === 'qoder') return qoderUsage;
    if (effFilter === 'custom') return customUsage;
    return mergeBuckets(traeUsage, buddyUsage, buddyAiUsage, qoderUsage, customUsage);
  }, [effFilter, traeUsage, buddyUsage, buddyAiUsage, qoderUsage, customUsage]);

  // ---- Custom 池会话明细（issue #149 P2）：仅 effFilter==='custom' 时加载 ----
  const [sessData, setSessData] = useState<UsageSessionsResult | null>(null);
  const [sessPage, setSessPage] = useState(1);
  const [sessLoading, setSessLoading] = useState(false);
  // 审查对齐 SessionsTab No.11 口径：查询失败持久错误态（失败静默归零会被误读为「暂无流水」）
  const [sessError, setSessError] = useState<string | null>(null);
  const [expandedKey, setExpandedKey] = useState<string | null>(null);
  const [detail, setDetail] = useState<UsageSessionView[] | null>(null);
  const sessSeqRef = useRef(0);
  // 明细请求序号守卫（对齐 SessionsTab）：快速切换展开行时旧响应可能后到，
  // 仅最新一次请求可写明细态
  const detailSeqRef = useRef(0);

  const loadSess = async (d: number, p: number) => {
    const seq = ++sessSeqRef.current;
    setSessLoading(true);
    try {
      const end = localDateStr(new Date());
      const start = localDateStr(new Date(Date.now() - (d - 1) * 86_400_000));
      const r = await api.apiServer.customSessionsQuery({ start, end, page: p, pageSize: 20 });
      if (seq === sessSeqRef.current) {
        setSessData(r);
        setSessError(null);
      }
    } catch (err) {
      if (seq === sessSeqRef.current) {
        setSessData(null);
        setSessError(String(err));
      }
    } finally {
      if (seq === sessSeqRef.current) setSessLoading(false);
    }
  };

  // 展开/明细随主查询重置（对齐 SessionsTab resetExpanded）：翻页或改天数后
  // 行键可能恰好重合，陈旧明细不得残留
  const resetExpanded = () => {
    detailSeqRef.current++;
    setExpandedKey(null);
    setDetail(null);
  };

  // 展开会话看各轮：session_id = conversation_key 精确匹配（后端 LIMIT 500）
  const toggleSessDetail = (r: UsageSessionView) => {
    const key = `${r.date}|${r.user_id}|${r.session_id}|${r.preview}`;
    if (expandedKey === key) {
      resetExpanded();
      return;
    }
    const seq = ++detailSeqRef.current;
    setExpandedKey(key);
    setDetail(null);
    void api.apiServer
      .customSessionDetail({
        date: r.date,
        uid: r.user_id,
        sessionId: r.session_id,
        preview: r.preview,
      })
      .then((rows) => {
        if (seq !== detailSeqRef.current) return;
        setDetail(rows);
      })
      .catch(() => {
        if (seq !== detailSeqRef.current) return;
        setDetail([]);
      });
  };

  useEffect(() => {
    resetExpanded();
    if (effFilter === 'custom') void loadSess(days, sessPage);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [effFilter, days, sessPage]);

  // 汇总（跨天聚合）+ 图表数据（与原页面统计逻辑一致，纯搬移）
  const summary = useMemo(() => {
    const models = new Map<string, { requests: number; ok: number; errors: number }>();
    const modelLatency = new Map<string, UsageModelLatencyView>();
    const t = usage.reduce(
      (acc, d) => {
        acc.requests += d.total_requests;
        acc.ok += d.ok;
        acc.errors += d.errors;
        acc.prompt += d.prompt_tokens;
        acc.completion += d.completion_tokens;
        acc.weightedDuration += d.avg_duration_ms * d.total_requests;
        acc.ttfbSamples += d.ttfb_samples ?? 0;
        acc.weightedTtfb += (d.avg_ttfb_ms ?? 0) * (d.ttfb_samples ?? 0);
        for (const m of d.models) {
          const e = models.get(m.name) ?? { requests: 0, ok: 0, errors: 0 };
          e.requests += m.requests;
          e.ok += m.ok;
          e.errors += m.errors;
          models.set(m.name, e);
        }
        for (const m of d.model_latency ?? []) {
          const e = modelLatency.get(m.model);
          if (e) {
            e.samples += m.samples;
            e.p95_duration_ms = maxOpt(e.p95_duration_ms, m.p95_duration_ms);
            e.max_duration_ms = maxOpt(e.max_duration_ms, m.max_duration_ms);
            e.avg_ttfb_ms = maxOpt(e.avg_ttfb_ms, m.avg_ttfb_ms);
            e.p95_ttfb_ms = maxOpt(e.p95_ttfb_ms, m.p95_ttfb_ms);
          } else {
            modelLatency.set(m.model, { ...m });
          }
        }
        return acc;
      },
      {
        requests: 0,
        ok: 0,
        errors: 0,
        prompt: 0,
        completion: 0,
        weightedDuration: 0,
        ttfbSamples: 0,
        weightedTtfb: 0,
      },
    );
    const topModels = [...models.entries()]
      .sort((a, b) => b[1].requests - a[1].requests)
      .slice(0, 5)
      .map(([name, v]) => ({ name, ...v }));
    const latencyModels = [...modelLatency.values()]
      .sort((a, b) => b.samples - a.samples)
      .slice(0, 5);
    return {
      ...t,
      topModels,
      latencyModels,
      successRate: t.requests > 0 ? ((t.ok / t.requests) * 100).toFixed(1) : '—',
      avgDuration: t.requests > 0 ? Math.round(t.weightedDuration / t.requests) : 0,
      avgTtfb: t.ttfbSamples > 0 ? Math.round(t.weightedTtfb / t.ttfbSamples) : undefined,
      p95Ttfb: maxOptList(usage.map((d) => d.p95_ttfb_ms)),
      p95Duration: maxOptList(usage.map((d) => d.p95_duration_ms)),
    };
  }, [usage]);

  const chartData = useMemo(
    () => usage.map((d) => ({ date: d.date.slice(5), 成功: d.ok, 失败: d.errors })),
    [usage],
  );

  // 可见池筛选按钮（custom 恒可见；隐藏应用的池筛选项隐藏）与聚合提示文案
  const visibleFilters = (Object.keys(FILTER_LABELS) as PoolFilter[]).filter(
    (f) => f === 'all' || f === 'custom' || isPoolVisible(settings, f),
  );
  const poolHintLabels = (['trae', 'buddy', 'buddy-ai', 'qoder'] as const)
    .filter((f) => isPoolVisible(settings, f))
    .map((f) => FILTER_LABELS[f]);
  const filterHint =
    effFilter === 'all'
      ? `${[...poolHintLabels, '自定义'].join(' + ')} 聚合`
      : effFilter === 'custom'
        ? '仅自定义模型桶'
        : `仅 ${FILTER_LABELS[effFilter]} 桶`;

  return (
    <div className="card p-4">
      <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <BarChart3 size={16} className="text-brand-500" />
          <h3 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">用量统计</h3>
          <span className="hidden text-xs text-slate-400 sm:inline">按日落盘 · 独立于服务运行状态</span>
        </div>
        <div className="flex flex-wrap items-center gap-1">
          {([7, 14, 30] as const).map((d) => (
            <button
              key={d}
              className={
                'rounded-md px-2 py-1 text-xs transition ' +
                (days === d
                  ? 'bg-brand-500/10 font-medium text-brand-600 dark:text-brand-400'
                  : 'text-slate-500 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800')
              }
              onClick={() => setDays(d)}
            >
              {d}天
            </button>
          ))}
          <span className="mx-1 text-slate-300 dark:text-zinc-700">|</span>
          {visibleFilters.map((f) => (
            <button
              key={f}
              className={
                'rounded-md px-2 py-1 text-xs transition ' +
                (effFilter === f
                  ? 'bg-brand-500/10 font-medium text-brand-600 dark:text-brand-400'
                  : 'text-slate-500 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800')
              }
              onClick={() => setFilter(f)}
            >
              {FILTER_LABELS[f]}
            </button>
          ))}
          <button
            className="btn-ghost ml-1 flex items-center gap-1 text-xs"
            onClick={() => void load(days)}
            disabled={loading}
          >
            <RefreshCw size={13} className={loading ? 'animate-spin' : ''} />
            刷新
          </button>
        </div>
      </div>

      {/* 5 个统计块单行铺开：sm 起 5 列，收紧间距防换行 */}
      <div className="mb-4 grid grid-cols-2 gap-2 sm:grid-cols-5">
        <StatCard
          label="总请求数"
          value={summary.requests}
          tone="brand"
          hint={`近 ${days} 天 · ${filterHint}`}
        />
        <StatCard
          label="成功率"
          value={summary.successRate === '—' ? '—' : `${summary.successRate}%`}
          tone="green"
          hint={`失败 ${summary.errors} 次`}
        />
        <StatCard
          label="Token 消耗"
          value={fmtTokens(summary.prompt + summary.completion)}
          tone="amber"
          hint={`输入 ${fmtTokens(summary.prompt)} / 输出 ${fmtTokens(summary.completion)}`}
        />
        <StatCard
          label="平均耗时"
          value={summary.requests > 0 ? `${summary.avgDuration}ms` : '—'}
          tone="blue"
          hint={summary.p95Duration != null ? `按请求加权 · P95 ${summary.p95Duration}ms` : '按请求加权'}
        />
        <StatCard
          label="首字延迟 TTFT"
          value={summary.avgTtfb != null ? `${summary.avgTtfb}ms` : '—'}
          tone="violet"
          hint={
            summary.avgTtfb != null
              ? `P95 ${summary.p95Ttfb ?? '—'}ms · ${summary.ttfbSamples} 样本（流式成功）`
              : '暂无流式样本'
          }
        />
      </div>

      {summary.requests > 0 ? (
        <div className="h-52 text-slate-500 dark:text-zinc-400">
          <ResponsiveContainer width="100%" height="100%">
            <BarChart data={chartData} margin={{ top: 4, right: 8, bottom: 0, left: -16 }}>
              <CartesianGrid strokeDasharray="3 3" stroke="currentColor" opacity={0.15} vertical={false} />
              <XAxis dataKey="date" tick={{ fill: 'currentColor', fontSize: 11 }} tickLine={false} />
              <YAxis allowDecimals={false} tick={{ fill: 'currentColor', fontSize: 11 }} tickLine={false} />
              <Tooltip
                contentStyle={{
                  borderRadius: 8,
                  border: '1px solid rgba(120,120,120,0.25)',
                  fontSize: 12,
                }}
              />
              <Legend wrapperStyle={{ fontSize: 12 }} />
              <Bar dataKey="成功" stackId="s" fill="#10b981" />
              <Bar dataKey="失败" stackId="s" fill="#f43f5e" radius={[3, 3, 0, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </div>
      ) : (
        <p className="py-6 text-center text-sm text-slate-400">
          暂无请求数据 — 发起一次 API 调用后这里会展示按日趋势（{filterHint}）
        </p>
      )}

      {summary.topModels.length > 0 && (
        <div className="mt-4">
          <p className="mb-2 text-xs font-medium text-slate-500 dark:text-zinc-400">
            模型分布（近 {days} 天 Top 5 · {FILTER_LABELS[effFilter]}）
          </p>
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400">
                  <th className="pb-2 pr-4 font-medium">模型</th>
                  <th className="pb-2 pr-4 font-medium">请求数</th>
                  <th className="pb-2 pr-4 font-medium">成功</th>
                  <th className="pb-2 pr-4 font-medium">失败</th>
                  <th className="pb-2 font-medium">占比</th>
                </tr>
              </thead>
              <tbody>
                {summary.topModels.map((m) => {
                  const pct = summary.requests > 0 ? (m.requests / summary.requests) * 100 : 0;
                  return (
                    <tr
                      key={m.name}
                      className="border-b border-slate-100 last:border-0 dark:border-zinc-800"
                    >
                      <td className="py-2 pr-4 font-mono text-xs font-medium text-slate-700 dark:text-zinc-200">
                        {m.name}
                      </td>
                      <td className="py-2 pr-4 tabular-nums text-slate-600 dark:text-zinc-300">{m.requests}</td>
                      <td className="py-2 pr-4 tabular-nums text-emerald-600 dark:text-emerald-400">{m.ok}</td>
                      <td className="py-2 pr-4 tabular-nums text-rose-600 dark:text-rose-400">{m.errors}</td>
                      <td className="w-40 py-2">
                        <div className="flex items-center gap-2">
                          <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-slate-100 dark:bg-zinc-800">
                            <div
                              className={cn('h-full rounded-full bg-brand-500')}
                              style={{ width: `${Math.min(100, pct)}%` }}
                            />
                          </div>
                          <span className="w-12 shrink-0 text-right text-xs tabular-nums text-slate-400">
                            {pct.toFixed(1)}%
                          </span>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {summary.latencyModels.length > 0 && (
        <div className="mt-4">
          <p className="mb-2 text-xs font-medium text-slate-500 dark:text-zinc-400">
            模型延迟（近 {days} 天 Top 5 · P50/P95 总耗时 + 首字延迟）
          </p>
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400">
                  <th className="pb-2 pr-4 font-medium">模型</th>
                  <th className="pb-2 pr-4 font-medium">样本数</th>
                  <th className="pb-2 pr-4 font-medium">P50 耗时</th>
                  <th className="pb-2 pr-4 font-medium">P95 耗时</th>
                  <th className="pb-2 pr-4 font-medium">平均 TTFT</th>
                  <th className="pb-2 font-medium">P95 TTFT</th>
                </tr>
              </thead>
              <tbody>
                {summary.latencyModels.map((m) => (
                  <tr
                    key={m.model}
                    className="border-b border-slate-100 last:border-0 dark:border-zinc-800"
                  >
                    <td className="py-2 pr-4 font-mono text-xs font-medium text-slate-700 dark:text-zinc-200">
                      {m.model}
                    </td>
                    <td className="py-2 pr-4 tabular-nums text-slate-600 dark:text-zinc-300">{m.samples}</td>
                    <td className="py-2 pr-4 tabular-nums text-slate-600 dark:text-zinc-300">
                      {m.p50_duration_ms != null ? `${m.p50_duration_ms}ms` : '—'}
                    </td>
                    <td className="py-2 pr-4 tabular-nums text-slate-600 dark:text-zinc-300">
                      {m.p95_duration_ms != null ? `${m.p95_duration_ms}ms` : '—'}
                    </td>
                    <td className="py-2 pr-4 tabular-nums text-violet-600 dark:text-violet-400">
                      {m.avg_ttfb_ms != null ? `${m.avg_ttfb_ms}ms` : '—'}
                    </td>
                    <td className="py-2 tabular-nums text-violet-600 dark:text-violet-400">
                      {m.p95_ttfb_ms != null ? `${m.p95_ttfb_ms}ms` : '—'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* Custom 池会话明细（issue #149 P2）：请求级流水按会话两遍归并；无积分语义 */}
      {effFilter === 'custom' && (
        <div className="mt-4">
          <div className="mb-2 flex items-center justify-between">
            <p className="text-xs font-medium text-slate-500 dark:text-zinc-400">
              会话明细（近 {days} 天 · 点击行展开看各轮请求阶段）
            </p>
            {sessData && sessData.total > 0 && (
              <div className="flex items-center gap-2 text-xs text-slate-400">
                <button
                  className="rounded px-1.5 py-0.5 hover:bg-slate-100 disabled:opacity-40 dark:hover:bg-zinc-800"
                  disabled={sessData.page <= 1 || sessLoading}
                  onClick={() => setSessPage(sessData.page - 1)}
                >
                  上一页
                </button>
                <span className="tabular-nums">
                  {sessData.page}/{Math.max(1, Math.ceil(sessData.total / sessData.page_size))}
                </span>
                <button
                  className="rounded px-1.5 py-0.5 hover:bg-slate-100 disabled:opacity-40 dark:hover:bg-zinc-800"
                  disabled={
                    sessData.page >= Math.ceil(sessData.total / sessData.page_size) || sessLoading
                  }
                  onClick={() => setSessPage(sessData.page + 1)}
                >
                  下一页
                </button>
              </div>
            )}
          </div>
          {!sessData || sessData.rows.length === 0 ? (
            <p className="py-4 text-center text-xs text-slate-400">
              {sessLoading
                ? '加载中…'
                : sessError
                  ? `会话流水加载失败：${sessError}`
                  : '暂无会话流水 — 自定义上游发起一轮对话后这里会展示按会话聚合的请求明细'}
            </p>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400">
                    <th className="pb-2 pr-4 font-medium">时间</th>
                    <th className="pb-2 pr-4 font-medium">Key</th>
                    <th className="pb-2 pr-4 font-medium">会话说明</th>
                    <th className="pb-2 pr-4 font-medium">模型</th>
                    <th className="pb-2 pr-4 font-medium">请求</th>
                    <th className="pb-2 font-medium">Token</th>
                  </tr>
                </thead>
                <tbody>
                  {sessData.rows.map((r) => {
                    const key = `${r.date}|${r.user_id}|${r.session_id}|${r.preview}`;
                    const open = expandedKey === key;
                    return (
                      <Fragment key={key}>
                        <tr
                          className={cn(
                            'cursor-pointer border-b border-slate-100 dark:border-zinc-800',
                            open && 'bg-slate-50 dark:bg-zinc-800/40',
                          )}
                          onClick={() => toggleSessDetail(r)}
                        >
                          <td className="py-2 pr-4 tabular-nums text-slate-600 dark:text-zinc-300">
                            <span className="inline-flex items-center gap-1">
                              {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
                              {fmtClock(r.ts)}
                            </span>
                          </td>
                          <td className="py-2 pr-4 font-mono text-xs text-slate-700 dark:text-zinc-200">
                            {r.account_name || r.user_id}
                          </td>
                          <td className="py-2 pr-4 text-slate-600 dark:text-zinc-300">
                            <span className="block max-w-[16rem] truncate" title={r.preview}>
                              {r.preview || '（空）'}
                            </span>
                          </td>
                          <td className="py-2 pr-4 font-mono text-xs text-slate-700 dark:text-zinc-200">
                            {r.model}
                          </td>
                          <td className="py-2 pr-4 text-xs text-slate-500 dark:text-zinc-400">
                            {r.mode || '1 次请求'}
                          </td>
                          <td className="py-2 tabular-nums text-xs text-slate-500 dark:text-zinc-400">
                            {fmtTokens(r.input_tokens)} / {fmtTokens(r.output_tokens)}
                          </td>
                        </tr>
                        {open && (
                          <tr className="border-b border-slate-100 bg-slate-50/60 dark:border-zinc-800 dark:bg-zinc-800/20">
                            <td colSpan={6} className="px-2 py-2">
                              {detail === null ? (
                                <p className="py-1 text-center text-xs text-slate-400">加载明细中…</p>
                              ) : detail.length === 0 ? (
                                <p className="py-1 text-center text-xs text-slate-400">无请求级明细</p>
                              ) : (
                                <div className="space-y-1">
                                  {detail.map((d, i) => (
                                    <div key={i} className="flex items-center gap-2 text-xs">
                                      <span className="w-16 shrink-0 tabular-nums text-slate-400">
                                        {fmtClock(d.ts)}
                                      </span>
                                      <span
                                        className={cn(
                                          'shrink-0 rounded px-1.5 py-0.5 text-[11px] font-medium',
                                          d.mode.includes('失败')
                                            ? 'bg-rose-500/10 text-rose-600 dark:text-rose-400'
                                            : d.mode === '工具轮'
                                              ? 'bg-violet-500/10 text-violet-600 dark:text-violet-400'
                                              : 'bg-brand-500/10 text-brand-600 dark:text-brand-400',
                                        )}
                                      >
                                        {d.mode || '未知阶段'}
                                      </span>
                                      <span className="shrink-0 font-mono text-slate-600 dark:text-zinc-300">
                                        {d.model}
                                      </span>
                                      <span
                                        className="truncate text-slate-500 dark:text-zinc-400"
                                        title={d.preview}
                                      >
                                        {d.preview || '（空）'}
                                      </span>
                                      <span className="ml-auto shrink-0 tabular-nums text-slate-400">
                                        {fmtTokens(d.input_tokens)} / {fmtTokens(d.output_tokens)}
                                      </span>
                                    </div>
                                  ))}
                                </div>
                              )}
                            </td>
                          </tr>
                        )}
                      </Fragment>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
