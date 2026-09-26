import { useEffect, useMemo, useState } from 'react';
import {
  Bar,
  Line,
  XAxis,
  YAxis,
  ComposedChart,
  ResponsiveContainer,
  Tooltip,
  CartesianGrid,
  Legend,
} from 'recharts';
import { Activity } from 'lucide-react';
import ChartFilterBar, { SOURCE_LABELS, type BoardSource } from '../../components/ChartFilterBar';
import ActivityHeatmap from '../../components/charts/ActivityHeatmap';
import ModelRanking from '../../components/charts/ModelRanking';
import { Badge, EmptyState, StatCard } from '../../components/ui';
import { useDateRange } from '../../hooks/useDateRange';
import { useIsDark } from '../../lib/useIsDark';
import type { UsageDayView, UsageHistoryResult, WbTokenStats } from '../../types';
import type { PlatformScope } from './KpiRow';

/**
 * Token 统计 Tab（credits-dashboard-plan.md §3.2/§5.2，批次 3 = issue #35 Bug2 用户可见修复）：
 * ① 总 Token 统计卡（总/输入/输出/缓存命中率）② Token 与调用趋势（双轴堆叠柱 + 调用虚线）
 * ③ 模型消耗排行 ④ 年度活动热力图。
 * 三源（口径不同，独立展示不合并）：
 * 本地 = workbuddy_token_stats（365 天，WB/CodeBuddy 客户端会话）；
 * API 网关 = api_usage_stats / api_wb_usage_stats（90 天，代理转发口径；custom 池仅 API 服务页展示）；
 * 官网 = Trae usage_history token 字段（365 天）；Buddy 官网无按日 token 明细（§8.2 诚实空态）。
 * 统一口径：总 Token = input + output + cache_write（官网/网关 cache_write 恒 0，即 input + output）；
 * 官网 input 按供应商语义假设已含缓存读取（与本地源一致），新增输入 = max(0, input − 缓存读取)。
 */

const STACK_COLORS: Record<string, string> = {
  cache_read: '#6366f1',
  new_input: '#38bdf8',
  output: '#22c55e',
  cache_write: '#f59e0b',
  input: '#38bdf8',
};

const SEG_LABELS: Record<string, string> = {
  cache_read: '缓存读取',
  new_input: '新增输入',
  output: '输出',
  cache_write: '缓存写入',
  input: '输入',
};

function fmtTokens(n: number): string {
  if (!Number.isFinite(n)) return '0';
  if (Math.abs(n) >= 1e8) return `${(n / 1e8).toFixed(2)} 亿`;
  if (Math.abs(n) >= 1e4) return `${(n / 1e4).toFixed(1)} 万`;
  return n.toLocaleString('zh-CN');
}

/** 网关源按平台维度选取（Trae 池 / Buddy 池；custom 池仅 API 服务页 UsageStatsPanel 展示） */
function gwPoolsFor(gw: GatewayDays | null, scope: PlatformScope): UsageDayView[] {
  if (!gw) return [];
  return scope === 'trae' ? gw.trae : gw.buddy;
}

export interface GatewayDays {
  trae: UsageDayView[];
  buddy: UsageDayView[];
}

type DayAgg = { input: number; output: number; cache_read: number; cache_write: number; calls: number };
const emptyAgg = (): DayAgg => ({ input: 0, output: 0, cache_read: 0, cache_write: 0, calls: 0 });

export default function TokensTab({
  scope,
  source,
  usage,
  tokenStats,
  gateway,
}: {
  scope: PlatformScope;
  /** 激活数据源（源切换控件在页面第二层分类，Dashboard 承载；默认源按平台选择） */
  source: BoardSource;
  /** Trae 官网消耗明细（官网源 token 字段数据源） */
  usage: UsageHistoryResult | null;
  /** 本地 Token 统计（workbuddy_token_stats，365 天） */
  tokenStats: WbTokenStats | null;
  /** API 网关 Trae/Buddy 池用量（90 天） */
  gateway: GatewayDays | null;
}) {
  const isDark = useIsDark();
  const { range, setRange, startStr, todayStr, dateList } = useDateRange('7d');
  const [modelFilter, setModelFilter] = useState('');
  // 切源时清空模型筛选：旧源的模型在新源列表中不存在，残留会让趋势静默归零（审查修复）
  useEffect(() => setModelFilter(''), [source]);

  const traeActive = scope !== 'buddy';
  const buddyActive = scope !== 'trae';
  const inRange = (date: string) => date >= startStr && date <= todayStr;

  // ---- 源可用性（§8 诚实空态兜底；源禁用说明在页面第二层分类的切换控件上）----
  const localUnavailable = source === 'local' && !buddyActive;
  const officialUnavailable = source === 'official' && !traeActive;

  // ---- 范围 × 模型 × 平台 聚合 ----
  const agg = useMemo(() => {
    const byDate = new Map<string, DayAgg>();
    const byModel = new Map<string, { tokens: number; calls: number }>();
    const addDay = (date: string, p: DayAgg) => {
      const cur = byDate.get(date) ?? emptyAgg();
      cur.input += p.input;
      cur.output += p.output;
      cur.cache_read += p.cache_read;
      cur.cache_write += p.cache_write;
      cur.calls += p.calls;
      byDate.set(date, cur);
    };

    if (source === 'local' && buddyActive && tokenStats) {
      for (const [model, points] of Object.entries(tokenStats.daily_by_model)) {
        if (modelFilter && model !== modelFilter) continue;
        for (const p of points) {
          if (!p.date || !inRange(p.date)) continue;
          addDay(p.date, {
            input: p.input ?? 0,
            output: p.output ?? 0,
            cache_read: p.cache_read ?? 0,
            cache_write: p.cache_write ?? 0,
            calls: p.calls ?? 0,
          });
          const m = byModel.get(model) ?? { tokens: 0, calls: 0 };
          m.tokens += (p.input ?? 0) + (p.output ?? 0) + (p.cache_write ?? 0);
          m.calls += p.calls ?? 0;
          byModel.set(model, m);
        }
      }
    }
    if (source === 'gateway') {
      for (const day of gwPoolsFor(gateway, scope)) {
        if (!inRange(day.date)) continue;
        addDay(day.date, {
          input: day.prompt_tokens,
          output: day.completion_tokens,
          cache_read: 0,
          cache_write: 0,
          calls: day.total_requests,
        });
        for (const m of day.models) {
          if (modelFilter && m.name !== modelFilter) continue;
          const e = byModel.get(m.name) ?? { tokens: 0, calls: 0 };
          // 网关源无按模型 token 明细，排行口径 = 请求数（§3.1 同款诚实声明）
          e.calls += m.requests;
          byModel.set(m.name, e);
        }
      }
    }
    if (source === 'official' && traeActive && usage) {
      for (const a of usage.accounts) {
        for (const d of a.daily) {
          if (!inRange(d.date)) continue;
          addDay(d.date, {
            input: d.input_tokens,
            output: d.output_tokens,
            cache_read: d.cache_read_tokens,
            cache_write: 0,
            calls: d.sessions,
          });
        }
      }
    }
    let input = 0, output = 0, cache_read = 0, cache_write = 0, calls = 0;
    for (const v of byDate.values()) {
      input += v.input;
      output += v.output;
      cache_read += v.cache_read;
      cache_write += v.cache_write;
      calls += v.calls;
    }
    return { byDate, byModel, input, output, cache_read, cache_write, calls };
  }, [source, buddyActive, traeActive, tokenStats, gateway, scope, usage, modelFilter, startStr, todayStr]);

  // ---- 口径派生：总 Token / 缓存命中率 / 堆叠段 ----
  // 统一口径：总 Token = input + output + cache_write（官网/网关 cache_write 恒 0）；
  // 官网 input 假设已含缓存读取（与本地源供应商语义一致），不重复计。
  const totalTokens = agg.input + agg.output + agg.cache_write;
  const hitRate =
    source !== 'gateway' && agg.input > 0 ? agg.cache_read / agg.input : null;
  const segments =
    source === 'local'
      ? (['cache_read', 'new_input', 'output', 'cache_write'] as const)
      : source === 'gateway'
        ? (['input', 'output'] as const)
        : (['cache_read', 'new_input', 'output'] as const);

  // ---- 趋势数据（堆叠柱 + 调用虚线）----
  const trend = useMemo(
    () =>
      dateList.map((date) => {
        const a = agg.byDate.get(date) ?? emptyAgg();
        const row: Record<string, number | string> = {
          label:
            range === 'year'
              ? `${+date.slice(0, 4)}/${+date.slice(5, 7)}/${+date.slice(8, 10)}`
              : `${+date.slice(5, 7)}/${+date.slice(8, 10)}`,
          calls: a.calls,
        };
        // local/official：input 均按供应商语义含缓存读取，拆分为 缓存读取 + 新增输入（柱总高 = input + output，与总卡一致）
        if (source === 'local' || source === 'official') {
          row.cache_read = a.cache_read;
          row.new_input = Math.max(0, a.input - a.cache_read);
          row.output = a.output;
          if (source === 'local') row.cache_write = a.cache_write;
        } else {
          row.input = a.input;
          row.output = a.output;
        }
        return row;
      }),
    [dateList, range, agg, source],
  );
  const chartEmpty = trend.every((d) => (d.calls as number) === 0 && segments.every((k) => !(d[k] as number)));

  // ---- 模型排行（local=tokens；gateway=请求数口径；official 无按模型明细）----
  const ranking = useMemo(
    () =>
      [...agg.byModel.entries()].map(([model, m]) => ({
        model,
        value: source === 'local' ? m.tokens : m.calls,
        calls: source === 'local' ? m.calls : undefined,
      })),
    [agg, source],
  );

  // ---- 会话总数（§3.3：local=summary.calls；gateway=范围内请求合计；official=范围内 sessions）----
  const sessions = useMemo(() => {
    if (source === 'local') return tokenStats?.summary.calls ?? null;
    if (source === 'gateway') return gwPoolsFor(gateway, scope).reduce((s, d) => s + d.total_requests, 0);
    if (!traeActive || !usage) return null;
    return usage.accounts.reduce(
      (s, a) => s + a.daily.reduce((x, d) => (inRange(d.date) ? x + d.sessions : x), 0),
      0,
    );
  }, [source, tokenStats, gateway, scope, usage, traeActive, startStr, todayStr]);

  // ---- 年度热力图（全量历史，不受区间筛选影响；local 受模型筛选，与原 TokenStatsPanel 一致）----
  const heatValues = useMemo(() => {
    const m = new Map<string, number>();
    if (source === 'local' && buddyActive && tokenStats) {
      const fill = (pts: { date?: string; total?: number }[]) =>
        pts.forEach((p) => p.date && m.set(p.date, (m.get(p.date) ?? 0) + (p.total ?? 0)));
      if (modelFilter) fill(tokenStats.daily_by_model[modelFilter] ?? []);
      else fill(tokenStats.daily);
    } else if (source === 'gateway') {
      for (const d of gwPoolsFor(gateway, scope)) {
        m.set(d.date, (m.get(d.date) ?? 0) + d.prompt_tokens + d.completion_tokens);
      }
    } else if (source === 'official' && traeActive && usage) {
      for (const a of usage.accounts) {
        for (const d of a.daily) {
          m.set(d.date, (m.get(d.date) ?? 0) + d.input_tokens + d.output_tokens);
        }
      }
    }
    return m;
  }, [source, buddyActive, traeActive, tokenStats, gateway, scope, usage, modelFilter]);

  // ---- 覆盖窗口标注（§8.1）----
  const coverage =
    source === 'local'
      ? `本地 ${tokenStats?.window_days ?? 365} 天（~/.workbuddy + ~/.codebuddy 会话）`
      : source === 'gateway'
        ? '网关 90 天'
        : 'Trae 365 天 · Buddy 无接口';

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

  const unavailableHint =
    localUnavailable
      ? { title: '本地源无 Trae 数据', hint: '本地源 = WB/CodeBuddy 客户端会话统计；Trae 无本地源，切「官网」查看 Trae token 明细。' }
      : officialUnavailable
        ? { title: 'Buddy 官网未提供按日明细接口', hint: 'Token 的 input/output 日明细官网仅 Trae 提供；Buddy 请切「本地」或「API网关」源。' }
        : null;

  return (
    <div className="card p-5">
      <ChartFilterBar
        title="Token 统计"
        icon={<Activity size={16} className="text-indigo-500" />}
        badges={
          <>
            <Badge tone="slate">{SOURCE_LABELS[source]}源</Badge>
            {source === 'local' && tokenStats && (
              <>
                <Badge tone="slate">{tokenStats.files_scanned} 个会话文件</Badge>
                {tokenStats.parse_errors > 0 && <Badge tone="amber">{tokenStats.parse_errors} 行解析失败</Badge>}
              </>
            )}
            {source === 'gateway' && (
              <Badge tone="slate">
                {scope === 'trae' ? 'Trae 池' : 'Buddy 池'} · {gwPoolsFor(gateway, scope).length} 个日桶
              </Badge>
            )}
            {source === 'official' && usage?.cached && <Badge tone="slate">纯缓存读取</Badge>}
            {source === 'official' && usage?.truncated && <Badge tone="amber">部分时段会话超分页上限</Badge>}
          </>
        }
        sessions={sessions}
        models={
          source === 'local'
            ? (tokenStats?.models ?? []).map((m) => m.key ?? '').filter(Boolean)
            : source === 'gateway'
              ? [...new Set(gwPoolsFor(gateway, scope).flatMap((d) => d.models.map((m) => m.name)))].sort()
              : []
        }
        modelFilter={modelFilter}
        onModelFilter={setModelFilter}
        range={range}
        onRange={setRange}
        extra={coverage ? <span className="hidden text-xs text-slate-400 lg:inline">{coverage}</span> : undefined}
      />

      {unavailableHint ? (
        <EmptyState icon={<Activity size={22} />} title={unavailableHint.title} hint={unavailableHint.hint} />
      ) : (
        <>
          {/* ① 总 Token 统计卡（4 卡） */}
          <div className="grid grid-cols-2 gap-3 xl:grid-cols-4">
            <StatCard
              label="总 Token"
              value={fmtTokens(totalTokens)}
              hint={`${agg.calls.toLocaleString()} 次${source === 'gateway' ? '请求' : '调用'}`}
              tone="brand"
            />
            <StatCard
              label="输入 Token"
              value={fmtTokens(agg.input)}
              hint={source !== 'gateway' ? `其中缓存读取 ${fmtTokens(agg.cache_read)}` : undefined}
              tone="violet"
            />
            <StatCard
              label="输出 Token"
              value={fmtTokens(agg.output)}
              hint={source === 'local' ? `缓存写入 ${fmtTokens(agg.cache_write)}` : undefined}
              tone="green"
            />
            <StatCard
              label="缓存命中率"
              value={hitRate != null ? `${(hitRate * 100).toFixed(1)}%` : '—'}
              hint={
                source === 'gateway'
                  ? '网关源无缓存字段'
                  : source === 'official'
                    ? '口径：缓存读取 / 输入总量（假设官网 input 已含缓存读取）'
                    : '口径：缓存读取 / 输入总量'
              }
              tone="amber"
            />
          </div>

          {/* ② Token 与调用趋势（双轴：堆叠柱 + 调用虚线） */}
          <div className="mt-5">
            <div className="mb-2 text-sm font-medium">Token 与调用趋势</div>
            {chartEmpty ? (
              <EmptyState
                icon={<Activity size={22} />}
                title="暂无趋势数据"
                hint={
                  source === 'local'
                    ? '本地会话产生用量后这里会展示逐日 Token 构成与调用次数。'
                    : source === 'gateway'
                      ? '网关代理转发产生用量后展示（保留 90 天）；可在 API 服务页查看实时用量。'
                      : '点击右上角「刷新数据」增量拉取 Trae 官网 token 明细。'
                }
              />
            ) : (
              <div className="h-64">
                <ResponsiveContainer>
                  <ComposedChart data={trend} margin={{ top: 12, right: 8, left: 0, bottom: 4 }}>
                    <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                    <XAxis dataKey="label" {...axisProps} minTickGap={24} />
                    <YAxis yAxisId="tokens" {...axisProps} axisLine={false} width={56} tickFormatter={(v: number) => fmtTokens(v)} />
                    <YAxis yAxisId="calls" orientation="right" {...axisProps} axisLine={false} width={40} />
                    <Tooltip
                      contentStyle={tooltipStyle}
                      formatter={(v: number, name: string) => [
                        name === 'calls' ? String(v) : fmtTokens(v),
                        SEG_LABELS[name] ?? (name === 'calls' ? '调用次数' : name),
                      ]}
                    />
                    <Legend wrapperStyle={{ fontSize: 12 }} />
                    {segments.map((k) => (
                      <Bar
                        key={k}
                        yAxisId="tokens"
                        dataKey={k}
                        stackId="t"
                        name={SEG_LABELS[k]}
                        fill={STACK_COLORS[k]}
                        maxBarSize={22}
                      />
                    ))}
                    <Line yAxisId="calls" type="monotone" dataKey="calls" name="调用次数" stroke="#f43f5e" strokeWidth={1.5} strokeDasharray="5 3" dot={false} />
                  </ComposedChart>
                </ResponsiveContainer>
              </div>
            )}
          </div>

          {/* ③ 模型消耗排行（local=tokens；gateway=请求数口径，诚实声明） */}
          <div className="mt-5 border-t border-slate-100 pt-4 dark:border-zinc-800">
            <div className="mb-2 flex items-center gap-2">
              <h4 className="text-sm font-medium">模型消耗排行</h4>
              <span className="text-xs text-slate-400">所选区间 · Top8 + 其余合计</span>
              {source === 'gateway' && <Badge tone="amber">网关源无按模型 token 明细，口径 = 请求数</Badge>}
            </div>
            <ModelRanking
              items={ranking}
              fmtValue={source === 'local' ? fmtTokens : (n: number) => n.toLocaleString('zh-CN')}
              unit={source === 'local' ? 'tokens' : '次请求'}
              emptyHint={
                source === 'official'
                  ? '官网源无按模型 token 明细（日粒度仅 input/output/cache 汇总）。'
                  : source === 'gateway'
                    ? '该区间暂无网关请求记录。'
                    : '该范围内暂无本地 Token 记录。'
              }
            />
          </div>

          {/* ④ 年度活动热力图 */}
          <div className="mt-5 border-t border-slate-100 pt-4 dark:border-zinc-800">
            <div className="mb-2">
              <h4 className="text-sm font-medium">年度活动热力图</h4>
            </div>
            <ActivityHeatmap
              values={heatValues}
              unit="tokens"
              fmtValue={fmtTokens}
              emptyHint={
                source === 'local'
                  ? '最近一年暂无本地会话用量。'
                  : source === 'gateway'
                    ? '网关用量仅保留 90 天，热力图仅覆盖该窗口。'
                    : '暂无 Trae 官网 token 记录：点击「刷新数据」拉取后展示。'
              }
            />
          </div>
        </>
      )}
    </div>
  );
}
