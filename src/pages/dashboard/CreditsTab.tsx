import { useEffect, useMemo, useState } from 'react';
import { Bar, Line, XAxis, YAxis, ComposedChart, ResponsiveContainer, Tooltip, CartesianGrid } from 'recharts';
import { Coins } from 'lucide-react';
import ChartFilterBar, { SOURCE_LABELS, type BoardSource } from '../../components/ChartFilterBar';
import ActivityHeatmap from '../../components/charts/ActivityHeatmap';
import ModelRanking from '../../components/charts/ModelRanking';
import { Badge, EmptyState, StatCard } from '../../components/ui';
import { useDateRange } from '../../hooks/useDateRange';
import { useIsDark } from '../../lib/useIsDark';
import { fmtCredits } from '../../lib/format';
import type {
  CreditsDailySnapshot,
  UsageHistoryResult,
  WbCheckinRecord,
  WbCreditsSnapshot,
  WbUsageFallback,
  WbUsageOfficialAll,
} from '../../types';
import type { PlatformScope } from './KpiRow';
import type { GatewayDays } from './TokensTab';
import {
  mergeEarned,
  traeEarnedByDate,
  traeUsageToPoints,
  wbCheckinEarnedByDate,
  wbFallbackToPoints,
  wbOfficialAllToPoints,
  wbSnapshotEarnedByDate,
} from './adapters';

/**
 * 积分统计 Tab（credits-dashboard-plan.md §3.1/§5.1）：
 * ① 总积分统计卡（区间总获得/区间总消耗/净获得）② 积分趋势图 ③ 模型消耗排行 ④ 年度活动热力图。
 * 三源口径（§8 页面内诚实标注，不做静默合并）：
 * 官网 = Trae usage_history（365 天，日×模型）+ Buddy usage_official（31 天，日合计；模型排行另含 Buddy 31 天全窗口汇总）；
 * 本地 = WB 快照差分（365 天，日合计）；API 网关 = Trae/Buddy 池 usage_stats（90 天）——
 * api_usage 仅记 tokens 不记积分扣减，**决策落地：以请求数为主要口径、积分数不估算**（§3.1 推荐项）。
 */

const tooltipStyle = (isDark: boolean) => ({
  fontSize: 12,
  borderRadius: 10,
  border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
  background: isDark ? '#18181b' : '#fff',
  color: isDark ? '#e4e4e7' : '#1e293b',
  boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
  padding: '8px 12px',
} as const);

export default function CreditsTab({
  scope,
  source,
  usage,
  creditsDaily,
  wbOfficial,
  wbFallback,
  wbSnapshots,
  wbCheckin,
  gateway,
}: {
  scope: PlatformScope;
  /** 激活数据源（源切换控件在页面第二层分类，Dashboard 承载） */
  source: BoardSource;
  /** Trae 官网消耗明细（usage_history_fetch） */
  usage: UsageHistoryResult | null;
  /** Trae 每日积分快照（获得积分口径） */
  creditsDaily: CreditsDailySnapshot[];
  /** Buddy 官网用量聚合（31 天） */
  wbOfficial: WbUsageOfficialAll | null;
  /** Buddy 快照差分回退（365 天） */
  wbFallback: WbUsageFallback | null;
  /** Buddy 积分快照时序（方案 B：earned 优先口径） */
  wbSnapshots: WbCreditsSnapshot[];
  /** Buddy 签到日志（90 天，方案 A 回退口径） */
  wbCheckin: WbCheckinRecord[];
  /** API 网关 Trae/Buddy 池用量（90 天；网关源数据源） */
  gateway: GatewayDays | null;
}) {
  const isDark = useIsDark();
  const { range, setRange, startStr, todayStr, dateList } = useDateRange('7d');
  const [modelFilter, setModelFilter] = useState('');
  // 切源时清空模型筛选：旧源的模型在新源列表中不存在，残留会让趋势静默归零（审查修复）
  useEffect(() => setModelFilter(''), [source]);

  const traeActive = scope !== 'buddy';
  const buddyActive = scope !== 'trae';
  // 本地自然日 ISO 字符串可直接比较；作为 memo 依赖以 startStr/todayStr 表达
  const inRange = (date: string) => date >= startStr && date <= todayStr;
  const isGateway = source === 'gateway';

  // ---- 各源 → BoardPoint ----
  const traePoints = useMemo(() => (source === 'official' ? traeUsageToPoints(usage) : []), [source, usage]);
  const buddyPoints = useMemo(
    () => (source === 'official' ? wbOfficialAllToPoints(wbOfficial) : wbFallbackToPoints(wbFallback)),
    [source, wbOfficial, wbFallback],
  );

  // ---- 模型清单（官网源 Trae 粒度 / 网关源模型计数；按累计降序）----
  const allModels = useMemo(() => {
    if (source === 'official' && traeActive) {
      const totals = new Map<string, number>();
      for (const p of traePoints) {
        for (const [model, m] of Object.entries(p.models)) {
          totals.set(model, (totals.get(model) ?? 0) + (m.credits ?? 0));
        }
      }
      return [...totals.entries()].sort((x, y) => y[1] - x[1]).map(([m]) => m);
    }
    if (source === 'gateway') {
      const pools = scope === 'trae' ? (gateway?.trae ?? []) : (gateway?.buddy ?? []);
      return [...new Set(pools.flatMap((d) => d.models.map((m) => m.name)))].sort();
    }
    return [];
  }, [source, traeActive, traePoints, gateway, scope]);

  // ---- 范围 × 模型 × 平台 聚合 ----
  const agg = useMemo(() => {
    const consumeByDate = new Map<string, number>();
    const earnedByDate = new Map<string, number>();
    const modelTotals = new Map<string, number>();
    let sessions = 0;
    let requests = 0;
    let okRequests = 0;
    let errRequests = 0;
    const addConsume = (date: string, v: number) => {
      consumeByDate.set(date, (consumeByDate.get(date) ?? 0) + v);
    };
    if (isGateway) {
      // 网关源：请求数口径（api_usage 仅记 tokens，不估算积分，§3.1 决策落地）
      const pools = scope === 'trae' ? (gateway?.trae ?? []) : (gateway?.buddy ?? []);
      for (const day of pools) {
        if (!inRange(day.date)) continue;
        sessions += day.total_requests;
        // 模型筛选时趋势/总量/成功/失败均按匹配模型过滤（网关按模型计数含 ok/errors，口径一致）
        const matched = modelFilter ? day.models.filter((m) => m.name === modelFilter) : null;
        const dayConsume = matched
          ? matched.reduce((s, m) => s + m.requests, 0)
          : day.total_requests;
        okRequests += matched
          ? matched.reduce((s, m) => s + m.ok, 0)
          : day.ok;
        errRequests += matched
          ? matched.reduce((s, m) => s + m.errors, 0)
          : day.errors;
        addConsume(day.date, dayConsume);
        requests += dayConsume;
        for (const m of day.models) {
          if (modelFilter && m.name !== modelFilter) continue;
          modelTotals.set(m.name, (modelTotals.get(m.name) ?? 0) + m.requests);
        }
      }
    } else {
      // Trae 官网源：日 × 模型粒度（模型筛选仅作用于消耗合计线，排行恒为全模型）
      if (traeActive && source === 'official') {
        for (const p of traePoints) {
          if (!inRange(p.date)) continue;
          sessions += p.calls ?? 0;
          addConsume(p.date, modelFilter ? (p.models[modelFilter]?.credits ?? 0) : (p.credits ?? 0));
          for (const [model, m] of Object.entries(p.models)) {
            modelTotals.set(model, (modelTotals.get(model) ?? 0) + (m.credits ?? 0));
          }
        }
      }
      // Buddy：仅日合计（官网 31 天零填充 / 本地快照差分）；模型筛选时不计入消耗合计，保持口径诚实
      if (buddyActive && !modelFilter) {
        for (const p of buddyPoints) {
          if (!inRange(p.date)) continue;
          addConsume(p.date, p.credits ?? 0);
        }
      }
      // Buddy 模型排行（官网源：wbOfficial.models = 官网接口 31 天全窗口跨账号汇总；本地快照差分无模型明细）
      if (buddyActive && source === 'official' && wbOfficial?.models) {
        for (const m of wbOfficial.models) {
          if (modelFilter && m.model !== modelFilter) continue;
          modelTotals.set(m.model, (modelTotals.get(m.model) ?? 0) + m.credit);
        }
      }
      // 获得积分（与消耗同源：仅官网源计 Trae 快照 earned；本地/网关源无 Trae 获得数据，避免净消耗系统性偏负）
      if (traeActive && source === 'official') {
        for (const [d, v] of traeEarnedByDate(creditsDaily)) {
          if (inRange(d)) consumeEarned(d, v);
        }
      }
      // Buddy 获得（方案 B 快照 earned 优先，缺失日回退方案 A 签到）
      if (buddyActive) {
        const earned = mergeEarned(wbCheckinEarnedByDate(wbCheckin), wbSnapshotEarnedByDate(wbSnapshots));
        for (const [d, v] of earned) {
          if (inRange(d)) consumeEarned(d, v);
        }
      }
    }
    let consumed = 0;
    for (const v of consumeByDate.values()) consumed += v;
    let earned = 0;
    for (const v of earnedByDate.values()) earned += v;
    return { consumeByDate, earnedByDate, modelTotals, consumed, earned, sessions, requests, okRequests, errRequests };

    function consumeEarned(date: string, v: number) {
      earnedByDate.set(date, (earnedByDate.get(date) ?? 0) + v);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    isGateway, scope, gateway, traeActive, buddyActive, source, traePoints, buddyPoints,
    modelFilter, creditsDaily, wbCheckin, wbSnapshots, wbOfficial, startStr, todayStr,
  ]);

  // ---- 趋势数据（消耗柱 + 获得线；网关源为请求数柱）----
  const trend = useMemo(
    () =>
      dateList.map((date) => ({
        label:
          range === 'year'
            ? `${+date.slice(0, 4)}/${+date.slice(5, 7)}/${+date.slice(8, 10)}`
            : `${+date.slice(5, 7)}/${+date.slice(8, 10)}`,
        consumed: agg.consumeByDate.get(date) ?? null,
        earned: isGateway ? null : (agg.earnedByDate.get(date) ?? null),
      })),
    [dateList, range, agg, isGateway],
  );
  const hasTrend = trend.some((d) => d.consumed != null || d.earned != null);
  const showDots = range === 'today' || range === '7d';
  const consumeLabel = isGateway ? '请求数' : '消耗积分';

  // ---- 年度热力图（全量历史，不受区间/模型筛选影响；按平台维度过滤）----
  const heatValues = useMemo(() => {
    const m = new Map<string, number>();
    if (isGateway) {
      const pools = scope === 'trae' ? (gateway?.trae ?? []) : (gateway?.buddy ?? []);
      for (const d of pools) m.set(d.date, (m.get(d.date) ?? 0) + d.total_requests);
      return m;
    }
    const pts = [
      ...(traeActive && source === 'official' ? traePoints : []),
      ...(buddyActive ? buddyPoints : []),
    ];
    for (const p of pts) m.set(p.date, (m.get(p.date) ?? 0) + (p.credits ?? 0));
    return m;
  }, [isGateway, scope, gateway, traeActive, buddyActive, source, traePoints, buddyPoints]);

  // ---- 覆盖窗口标注（§8.1）----
  const coverage = useMemo(() => {
    const parts: string[] = [];
    if (isGateway) {
      parts.push(scope === 'trae' ? 'Trae 池 90 天' : 'Buddy 池 90 天');
    } else {
      if (traeActive && source === 'official') parts.push('Trae 365 天');
      if (buddyActive) {
        parts.push(source === 'official' ? 'Buddy 31 天' : `Buddy 快照 ${wbFallback?.snapshot_days ?? '—'} 天`);
      }
    }
    return parts.length > 0 ? `覆盖窗口：${parts.join(' · ')}` : '';
  }, [isGateway, scope, traeActive, buddyActive, source, wbFallback]);

  // 源可用性（§8 诚实空态兜底；源禁用说明在页面第二层分类的切换控件上）
  const localUnavailable = source === 'local' && !buddyActive;

  const axisProps = {
    tick: { fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' },
    axisLine: { stroke: isDark ? '#3f3f46' : '#e2e8f0' },
    tickLine: false,
  } as const;

  return (
    <div className="card p-5">
      <ChartFilterBar
        title="积分统计"
        icon={<Coins size={16} className="text-violet-500" />}
        badges={
          <>
            <Badge tone="slate">{SOURCE_LABELS[source]}源</Badge>
            {isGateway && (
              <Badge tone="amber" title="api_usage 仅记录 tokens，不记录积分扣减；按倍率估算易失真，故以请求数为主要口径（§3.1 决策）。">
                口径 = 请求数（不估算积分）
              </Badge>
            )}
            {wbOfficial?.stale && source === 'official' && buddyActive && (
              <Badge tone="amber" title={wbOfficial.stale_reason || '本次拉取失败，展示历史缓存数据'}>
                Buddy 过期缓存回退
              </Badge>
            )}
            {source === 'local' && buddyActive && wbFallback?.note && (
              <Badge tone="slate" title={wbFallback.note}>消耗为快照差分推导</Badge>
            )}
          </>
        }
        sessions={isGateway || (source === 'official' && traeActive) ? agg.sessions : null}
        models={allModels}
        modelFilter={modelFilter}
        onModelFilter={setModelFilter}
        range={range}
        onRange={setRange}
        extra={
          usage && source === 'official' && traeActive ? (
            <span
              className="hidden text-xs text-slate-400 lg:inline"
              title="Trae 消耗明细最近更新时间（接口口径）"
            >
              明细更新于 {new Date(usage.fetched_at * 1000).toLocaleString('zh-CN', { hour12: false })}
            </span>
          ) : undefined
        }
      />

      {localUnavailable ? (
        <EmptyState
          icon={<Coins size={22} />}
          title="本地源无 Trae 数据"
          hint="本地源 = WorkBuddy 客户端会话快照差分；Trae 无本地源，切「官网」查看 Trae 消耗明细。"
        />
      ) : (
        <>
          {/* ① 统计卡：网关源 = 请求数/成功/失败；官网/本地源 = 区间总消耗/总获得/净消耗 */}
          <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
            {isGateway ? (
              <>
                <StatCard
                  label="区间总请求"
                  value={agg.requests.toLocaleString()}
                  hint={`${SOURCE_LABELS[source]}源（90 天保留窗口）${modelFilter ? ` · 已筛选 ${modelFilter}` : ''}`}
                  tone="amber"
                />
                <StatCard
                  label="成功请求"
                  value={agg.okRequests.toLocaleString()}
                  hint={modelFilter ? '已按模型筛选' : '口径 = 请求数'}
                  tone="green"
                />
                <StatCard
                  label="失败请求"
                  value={agg.errRequests.toLocaleString()}
                  hint={modelFilter ? '已按模型筛选' : '超时/上游错误计入'}
                  tone="red"
                />
              </>
            ) : (
              <>
                <StatCard
                  label="区间总获得"
                  value={fmtCredits(agg.earned)}
                  hint={[
                    traeActive && source === 'official' ? 'Trae 积分包归日' : null,
                    buddyActive ? 'Buddy 快照 earned（缺失日回退签到口径）' : null,
                  ]
                    .filter(Boolean)
                    .join(' + ')}
                  tone="green"
                />
                <StatCard
                  label="区间总消耗"
                  value={fmtCredits(agg.consumed)}
                  hint={`${SOURCE_LABELS[source]}源${modelFilter ? ` · ${modelFilter}` : ''}${source === 'official' && traeActive ? ` · 会话 ${agg.sessions.toLocaleString()}` : ''}`}
                  tone="amber"
                />
                <StatCard
                  label="净获得"
                  value={fmtCredits(agg.earned - agg.consumed)}
                  hint="获得 − 消耗"
                  tone={agg.earned - agg.consumed >= 0 ? 'green' : 'red'}
                />
              </>
            )}
          </div>

          {/* ② 积分趋势图（消耗柱 + 获得线；网关源仅请求数柱） */}
          <div className="mt-5">
            <div className="mb-2 flex items-center gap-3">
              <h4 className="text-sm font-medium">{isGateway ? '请求趋势图' : '积分趋势图'}</h4>
              <div className="flex items-center gap-3 text-xs text-slate-400">
                <span className="flex items-center gap-1">
                  <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#f59e0b' }} />
                  {consumeLabel}
                  {modelFilter ? `（${modelFilter}）` : ''}
                </span>
                {!isGateway && (
                  <span className="flex items-center gap-1">
                    <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#22c55e' }} />
                    获得积分
                  </span>
                )}
              </div>
            </div>
            {!hasTrend ? (
              <EmptyState
                icon={<Coins size={22} />}
                title="暂无趋势数据"
                hint={
                  isGateway
                    ? '网关代理转发产生请求后展示（保留 90 天）。'
                    : '点击右上角「刷新数据」拉取消耗明细；Buddy 官网源仅覆盖近 31 天。'
                }
              />
            ) : (
              <div className="h-56">
                <ResponsiveContainer>
                  <ComposedChart data={trend} margin={{ top: 12, right: 16, left: 0, bottom: 4 }}>
                    <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                    <XAxis dataKey="label" {...axisProps} minTickGap={24} />
                    <YAxis {...axisProps} axisLine={false} width={56} />
                    <Tooltip
                      cursor={{ stroke: isDark ? '#52525b' : '#cbd5e1', strokeWidth: 1, strokeDasharray: '3 3' }}
                      contentStyle={tooltipStyle(isDark)}
                      formatter={(v: number, name: string) => [
                        isGateway ? String(v) : fmtCredits(v),
                        name === 'consumed' ? consumeLabel : '获得积分',
                      ]}
                    />
                    <Bar dataKey="consumed" name={consumeLabel} fill="#f59e0b" maxBarSize={22} radius={[3, 3, 0, 0]} />
                    {!isGateway && (
                      <Line
                        type="monotone"
                        dataKey="earned"
                        name="获得积分"
                        stroke="#22c55e"
                        strokeWidth={2}
                        dot={showDots ? { r: 3, fill: '#22c55e', strokeWidth: 0 } : false}
                        activeDot={{ r: 5 }}
                        connectNulls
                      />
                    )}
                  </ComposedChart>
                </ResponsiveContainer>
              </div>
            )}
          </div>

          {/* ③ 模型消耗排行（官网源 = Trae 区间明细 + Buddy 31 天全窗口汇总；网关源请求数；本地快照差分无模型明细） */}
          <div className="mt-5 border-t border-slate-100 pt-4 dark:border-zinc-800">
            <div className="mb-2 flex items-center gap-2">
              <h4 className="text-sm font-medium">模型消耗排行</h4>
              <span className="text-xs text-slate-400">
                {source === 'official' && buddyActive && (wbOfficial?.models?.length ?? 0) > 0
                  ? 'Top8 + 其余合计'
                  : '所选区间 · Top8 + 其余合计'}
              </span>
              {source === 'official' && buddyActive && (wbOfficial?.models?.length ?? 0) > 0 && (
                <Badge
                  tone="slate"
                  title="Buddy 模型积分为官网接口 31 天全窗口跨账号汇总，不随上方区间筛选变化；Trae 部分为所选区间口径，同名模型两者相加。"
                >
                  Buddy 口径：31 天全窗口
                </Badge>
              )}
            </div>
            <ModelRanking
              items={[...agg.modelTotals.entries()].map(([model, value]) => ({ model, value }))}
              fmtValue={isGateway ? (n: number) => n.toLocaleString('zh-CN') : fmtCredits}
              unit={isGateway ? '次请求' : '积分'}
              emptyHint={
                source === 'official'
                  ? scope === 'buddy'
                    ? 'Buddy 官网暂无模型消耗记录（31 天窗口），点「刷新数据」重拉后再试。'
                    : '该区间暂无模型粒度消耗记录。'
                  : source === 'local'
                    ? '本地源仅有日合计（快照差分），无模型明细；切「官网」查看模型排行。'
                    : '该区间暂无网关请求记录。'
              }
            />
          </div>

          {/* ④ 年度活动热力图 */}
          <div className="mt-5 border-t border-slate-100 pt-4 dark:border-zinc-800">
            <div className="mb-2 flex items-center gap-2">
              <h4 className="text-sm font-medium">年度活动热力图</h4>
              {coverage && <span className="text-xs text-slate-400">{coverage}</span>}
            </div>
            <ActivityHeatmap
              values={heatValues}
              unit={isGateway ? '次请求' : '积分'}
              fmtValue={isGateway ? (n: number) => n.toLocaleString('zh-CN') : fmtCredits}
              emptyHint={
                isGateway
                  ? '网关用量仅保留 90 天，热力图仅覆盖该窗口。'
                  : '暂无消耗记录：点击右上角「刷新数据」拉取后展示。'
              }
            />
          </div>
        </>
      )}
    </div>
  );
}
