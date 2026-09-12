/**
 * 全局 API 管理 · 用量统计（unified-api-gateway-design §5.2/§5.3）
 * days（Trae 桶）/ wb_days（Buddy 桶）/ custom_days（自定义模型桶）三桶聚合，
 * 参数化资源池筛选（全部/Trae/Buddy/自定义）；含模型分布 Top5（按所选池聚合）。
 * 数据源：api_usage_stats / api_wb_usage_stats / api_custom_usage_stats（落盘数据，
 * 服务未运行也可查看，口径与键名不变 §9.6）。
 */
import { useEffect, useMemo, useState } from 'react';
import { BarChart3, RefreshCw } from 'lucide-react';
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
import type { UsageCounterView, UsageDayView, UsageKeyTokenView } from '../../types';

/** 资源池筛选（§5.2：全部/Trae/Buddy/自定义模型） */
export type PoolFilter = 'all' | 'trae' | 'buddy' | 'custom';

const FILTER_LABELS: Record<PoolFilter, string> = {
  all: '全部',
  trae: 'Trae',
  buddy: 'Buddy',
  custom: '自定义',
};

/** 同名计数器合并（模型/账号/Key 维度通用） */
function mergeCounters(a: UsageCounterView[], b: UsageCounterView[]): UsageCounterView[] {
  const map = new Map<string, UsageCounterView>();
  for (const c of [...a, ...b]) {
    const e = map.get(c.name);
    if (e) {
      e.requests += c.requests;
      e.ok += c.ok;
      e.errors += c.errors;
    } else {
      map.set(c.name, { ...c });
    }
  }
  return [...map.values()];
}

/** 按 Key 的 token 用量合并 */
function mergeKeyTokens(a: UsageKeyTokenView[], b: UsageKeyTokenView[]): UsageKeyTokenView[] {
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

/** 两条日记录合并（avg_duration 按请求数加权） */
function mergeDay(a: UsageDayView, b: UsageDayView): UsageDayView {
  const total = a.total_requests + b.total_requests;
  return {
    date: a.date,
    total_requests: total,
    ok: a.ok + b.ok,
    errors: a.errors + b.errors,
    stream_requests: a.stream_requests + b.stream_requests,
    prompt_tokens: a.prompt_tokens + b.prompt_tokens,
    completion_tokens: a.completion_tokens + b.completion_tokens,
    avg_duration_ms:
      total > 0
        ? Math.round(
            (a.avg_duration_ms * a.total_requests + b.avg_duration_ms * b.total_requests) / total,
          )
        : 0,
    models: mergeCounters(a.models, b.models),
    accounts: mergeCounters(a.accounts, b.accounts),
    keys: mergeCounters(a.keys, b.keys),
    key_tokens: mergeKeyTokens(a.key_tokens, b.key_tokens),
  };
}

/** 多桶按日合并（同日相加），按日期升序 */
function mergeBuckets(...buckets: UsageDayView[][]): UsageDayView[] {
  const map = new Map<string, UsageDayView>();
  for (const bucket of buckets) {
    for (const d of bucket) {
      const cur = map.get(d.date);
      map.set(d.date, cur ? mergeDay(cur, d) : { ...d });
    }
  }
  return [...map.values()].sort((x, y) => x.date.localeCompare(y.date));
}

export default function UsageStatsPanel() {
  const [days, setDays] = useState(14);
  const [filter, setFilter] = useState<PoolFilter>('all');
  const [traeUsage, setTraeUsage] = useState<UsageDayView[]>([]);
  const [buddyUsage, setBuddyUsage] = useState<UsageDayView[]>([]);
  const [customUsage, setCustomUsage] = useState<UsageDayView[]>([]);
  const [loading, setLoading] = useState(false);

  const load = async (d: number) => {
    setLoading(true);
    try {
      const [trae, buddy, custom] = await Promise.all([
        api.apiServer.usageStats(d).catch(() => [] as UsageDayView[]),
        api.apiServer.wbUsageStats(d).catch(() => [] as UsageDayView[]),
        api.apiServer.customUsageStats(d).catch(() => [] as UsageDayView[]),
      ]);
      setTraeUsage(trae);
      setBuddyUsage(buddy);
      setCustomUsage(custom);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void load(days);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [days]);

  // 按筛选聚合多桶（"全部"时相加，§8.2）
  const usage = useMemo(() => {
    if (filter === 'trae') return traeUsage;
    if (filter === 'buddy') return buddyUsage;
    if (filter === 'custom') return customUsage;
    return mergeBuckets(traeUsage, buddyUsage, customUsage);
  }, [filter, traeUsage, buddyUsage, customUsage]);

  // 汇总（跨天聚合）+ 图表数据（与原页面统计逻辑一致，纯搬移）
  const summary = useMemo(() => {
    const models = new Map<string, { requests: number; ok: number; errors: number }>();
    const t = usage.reduce(
      (acc, d) => {
        acc.requests += d.total_requests;
        acc.ok += d.ok;
        acc.errors += d.errors;
        acc.prompt += d.prompt_tokens;
        acc.completion += d.completion_tokens;
        acc.weightedDuration += d.avg_duration_ms * d.total_requests;
        for (const m of d.models) {
          const e = models.get(m.name) ?? { requests: 0, ok: 0, errors: 0 };
          e.requests += m.requests;
          e.ok += m.ok;
          e.errors += m.errors;
          models.set(m.name, e);
        }
        return acc;
      },
      { requests: 0, ok: 0, errors: 0, prompt: 0, completion: 0, weightedDuration: 0 },
    );
    const topModels = [...models.entries()]
      .sort((a, b) => b[1].requests - a[1].requests)
      .slice(0, 5)
      .map(([name, v]) => ({ name, ...v }));
    return {
      ...t,
      topModels,
      successRate: t.requests > 0 ? ((t.ok / t.requests) * 100).toFixed(1) : '—',
      avgDuration: t.requests > 0 ? Math.round(t.weightedDuration / t.requests) : 0,
    };
  }, [usage]);

  const chartData = useMemo(
    () => usage.map((d) => ({ date: d.date.slice(5), 成功: d.ok, 失败: d.errors })),
    [usage],
  );

  const filterHint =
    filter === 'all'
      ? 'Trae + Buddy + 自定义 三桶聚合'
      : filter === 'custom'
        ? '仅自定义模型桶'
        : `仅 ${FILTER_LABELS[filter]} 桶`;

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
          {(Object.keys(FILTER_LABELS) as PoolFilter[]).map((f) => (
            <button
              key={f}
              className={
                'rounded-md px-2 py-1 text-xs transition ' +
                (filter === f
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

      <div className="mb-4 grid grid-cols-2 gap-3 sm:grid-cols-4">
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
          hint="按请求加权"
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
            模型分布（近 {days} 天 Top 5 · {FILTER_LABELS[filter]}）
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
    </div>
  );
}
