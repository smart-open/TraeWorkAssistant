import { useCallback, useEffect, useMemo, useState } from 'react';
import { RefreshCw, Play, Square, Save, Copy, Coins, TerminalSquare, Plug, BarChart3 } from 'lucide-react';
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
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner, StatCard } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import { fmtTokens } from '../../lib/format';
import type {
  ApiServiceStatus,
  ApiPoolFile,
  UsageDayView,
  WbModelInfo,
  WorkBuddyAccountView,
} from '../../types';

/**
 * buddy-api-service API 服务（WB 上游管理，方案A UI 隔离）：
 * 与 Trae「API 服务」共用同一网关实例（同端口/同 Keys），本页聚焦 WB 上游：
 * WB 路由开关 + 模型目录 + WB 账号池状态 + 接入示例 + CC Switch（WB 侧独立条目）。
 * Trae 页的「WorkBuddy 上游」开关组与「同步 WB 模型目录」已迁回此处。
 */

const WB_FLAG_FIELDS: { key: 'wbEnabled' | 'wbDefaultThinking' | 'wbToolExec' | 'wbBgDowngrade'; label: string; desc: string }[] = [
  {
    key: 'wbEnabled',
    label: '启用 WB 上游',
    desc: 'WB 目录模型（owned_by=workbuddy）路由到 WB 账号池，消耗 WB 积分',
  },
  {
    key: 'wbDefaultThinking',
    label: '默认深度思考',
    desc: '客户端未显式请求 reasoning_effort 时默认注入 high',
  },
  {
    key: 'wbToolExec',
    label: '网关工具代执行',
    desc: '/v1/responses 声明 web_search 时由代理侧执行搜索并回喂（最多 3 轮）',
  },
  {
    key: 'wbBgDowngrade',
    label: '后台任务降级',
    desc: '标题/摘要类短请求（≤128 token 且 ≤512 字符）路由到最低倍率模型',
  },
];

export default function BuddyApiService() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [status, setStatus] = useState<ApiServiceStatus | null>(null);
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [wbFlags, setWbFlags] = useState({
    wbEnabled: false,
    wbDefaultThinking: false,
    wbToolExec: false,
    wbBgDowngrade: false,
  });
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [catalog, setCatalog] = useState<WbModelInfo[]>([]);
  const [syncing, setSyncing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [starting, setStarting] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [copying, setCopying] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [ccBusy, setCcBusy] = useState<'claude' | 'codex' | null>(null);
  const [ecoNote, setEcoNote] = useState('');
  const [usage, setUsage] = useState<UsageDayView[]>([]);
  const [usageDays, setUsageDays] = useState(14);
  const [usageLoading, setUsageLoading] = useState(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [st, pf, accs, cat] = await Promise.all([
        api.apiServer.status().catch(() => null),
        api.apiServer.poolList().catch(() => null),
        api.workbuddy.accountsList().catch(() => [] as WorkBuddyAccountView[]),
        api.apiServer.wbCatalogList().catch(() => [] as WbModelInfo[]),
      ]);
      setStatus(st);
      setPool(pf);
      setAccounts(accs);
      setCatalog(cat);
      if (pf) {
        setWbFlags({
          wbEnabled: pf.wb_enabled ?? false,
          wbDefaultThinking: pf.wb_default_thinking ?? false,
          wbToolExec: pf.wb_tool_exec ?? false,
          wbBgDowngrade: pf.wb_bg_downgrade ?? false,
        });
      }
    } catch (err) {
      pushToast('error', `读取网关状态失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // WB 上游用量统计：读独立 wb_days 落盘桶，与 Trae 模型用量（days 桶）互不混淆
  const loadUsage = useCallback(async (days: number) => {
    setUsageLoading(true);
    try {
      setUsage(await api.apiServer.wbUsageStats(days));
    } catch {
      /* 加载失败保留上次数据 */
    } finally {
      setUsageLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadUsage(usageDays);
  }, [usageDays, loadUsage]);

  // 保存 WB 上游开关：uids/strategy/groups 原样回传（本页不改 Trae 池配置）
  const saveFlags = async () => {
    setSaving(true);
    try {
      await withMinDelay(
        api.apiServer.poolSet(pool?.enabled_uids ?? [], pool?.strategy, pool?.group_ids, wbFlags),
        600,
      );
      pushToast('success', 'WB 上游配置已保存');
      if (status?.running) {
        pushToast('info', '需重启 API 服务以应用变更');
      }
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const start = async () => {
    setStarting(true);
    try {
      const s = await withMinDelay(api.apiServer.start(), 800);
      setStatus(s);
      useAppStore.setState({ apiStatus: s });
      pushToast('success', `API 服务已启动（端口 ${s.port}）`);
    } catch (err) {
      pushToast('error', `启动失败：${String(err)}`);
    } finally {
      setStarting(false);
    }
  };

  const stop = async () => {
    setStopping(true);
    try {
      await withMinDelay(api.apiServer.stop(), 600);
      setStatus(null);
      useAppStore.setState({ apiStatus: null });
      pushToast('info', 'API 服务已停止');
    } catch (err) {
      pushToast('error', `停止失败：${String(err)}`);
    } finally {
      setStopping(false);
    }
  };

  const syncCatalog = async () => {
    if (syncing) return;
    setSyncing(true);
    try {
      const n = await withMinDelay(api.apiServer.wbCatalogSync(), 800);
      pushToast('success', `WB 模型目录已更新（${n} 个模型），/v1/models 与路由即时生效`);
      setCatalog(await api.apiServer.wbCatalogList());
    } catch (err) {
      pushToast('error', `WB 模型目录同步失败：${String(err).slice(0, 120)}`);
    } finally {
      setSyncing(false);
    }
  };

  const copyExample = async () => {
    const port = status?.port ?? 7864;
    const model = catalog[0]?.id ?? 'hy4';
    const example = `# 客户端配置示例（WB 模型走 OpenAI 兼容格式）
接口地址: http://127.0.0.1:${port}/v1
API Key:  <在 Trae「API 服务」页的 API Keys 管理中创建并复制>
模型 ID:  ${model}（WB 目录模型，消耗 WB 积分）

# cURL 测试（请将 API Key 替换为列表中的完整值）
curl -X POST http://127.0.0.1:${port}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer your-api-key" \\
  -d '{
    "model": "${model}",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'`;
    setCopying(true);
    try {
      await withMinDelay(navigator.clipboard.writeText(example), 300);
      pushToast('success', '配置示例已复制到剪贴板');
    } catch {
      pushToast('error', '复制失败');
    } finally {
      setCopying(false);
    }
  };

  // CC Switch 协同（WB 侧条目 aiwork-wb-gateway-*，与 Trae 侧条目互不覆盖）
  const registerCcSwitch = async (appType: 'claude' | 'codex') => {
    if (ccBusy) return;
    setCcBusy(appType);
    setEcoNote('');
    try {
      const model = catalog[0]?.id;
      const msg = await withMinDelay(
        api.apiServer.ccSwitchRegister(appType, 'wb', undefined, model),
        600,
      );
      setEcoNote(`✓ ${msg}`);
    } catch (e) {
      setEcoNote(`✗ CC Switch 注册失败：${String(e).slice(0, 160)}`);
    } finally {
      setCcBusy(null);
    }
  };

  const credAccounts = accounts.filter((a) => a.has_credential);
  const running = status?.running ?? false;

  // WB 用量汇总（跨天聚合）+ 图表数据
  const usageSummary = useMemo(() => {
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

  const usageChartData = useMemo(
    () => usage.map((d) => ({ date: d.date.slice(5), 成功: d.ok, 失败: d.errors })),
    [usage],
  );

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="WorkBuddy · API 服务"
        desc="WB 积分转标准 API · 网关 WB 上游管理"
        actions={
          <>
            <button className="btn-outline" onClick={() => void refresh()} disabled={refreshing}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
            </button>
            {running ? (
              <button className="btn-outline !text-rose-600 hover:!border-rose-300" onClick={() => void stop()} disabled={stopping}>
                {stopping ? <Spinner /> : <Square size={15} />} 停止服务
              </button>
            ) : (
              <button className="btn-primary" onClick={() => void start()} disabled={starting}>
                {starting ? <Spinner className="text-white" /> : <Play size={15} />} 启动服务
              </button>
            )}
          </>
        }
      />

      {/* 网关状态卡（共享实例说明） */}
      <div className="card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium">网关状态</span>
            <Badge tone={running ? 'green' : 'slate'}>{running ? `运行 :${status?.port}` : '未启动'}</Badge>
            {running && status?.total_requests != null && (
              <span className="text-xs text-slate-400">累计请求 {status.total_requests} 次</span>
            )}
          </div>
        </div>
        <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-800/50 dark:text-zinc-400">
          与 Trae「API 服务」共用同一网关实例（同端口、同 API Keys、同调度进程）：
          本页管理 WorkBuddy 上游路由与模型目录，Trae 模型的网关级配置（默认模型/Keys/用量）在 Trae 页维护。
          开启下方「启用 WB 上游」后，WB 目录模型（owned_by=workbuddy）的请求将路由到 WB 账号池并消耗 WB 积分。
        </div>
      </div>

      {/* WB 上游配置卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <TerminalSquare size={16} className="text-brand-500" />
            <span className="text-sm font-medium">WorkBuddy 上游</span>
            <Badge tone={wbFlags.wbEnabled ? 'green' : 'slate'}>
              {wbFlags.wbEnabled ? '已启用' : '未启用'}
            </Badge>
          </div>
          <button className="btn-primary" onClick={() => void saveFlags()} disabled={saving}>
            {saving ? <Spinner className="text-white" /> : <Save size={15} />} 保存
          </button>
        </div>
        <div className="space-y-1.5">
          {WB_FLAG_FIELDS.map((item) => (
            <label
              key={item.key}
              className="flex cursor-pointer items-start gap-2.5 rounded-md px-1.5 py-1.5 transition hover:bg-slate-50 dark:hover:bg-zinc-800/50"
            >
              <input
                type="checkbox"
                className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                checked={wbFlags[item.key]}
                onChange={() => setWbFlags((prev) => ({ ...prev, [item.key]: !prev[item.key] }))}
              />
              <span className="min-w-0">
                <span className="block text-xs text-slate-700 dark:text-zinc-200">{item.label}</span>
                <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">{item.desc}</span>
              </span>
            </label>
          ))}
        </div>
        <p className="mt-2 text-xs text-slate-400 dark:text-zinc-500">
          保存后需重启 API 服务生效；Trae 账号池的调度策略与分组筛选在 Trae「API 服务」页配置，本页不改动。
        </p>
      </div>

      {/* WB 模型目录卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <Coins size={16} className="text-amber-500" />
            <span className="text-sm font-medium">WB 模型目录</span>
            <span className="text-xs text-slate-400">{catalog.length} 个模型</span>
          </div>
          <button className="btn-outline" onClick={() => void syncCatalog()} disabled={syncing}>
            <RefreshCw size={14} className={syncing ? 'animate-spin' : ''} />
            {syncing ? '同步中…' : '同步 WB 模型目录'}
          </button>
        </div>
        <p className="mb-3 text-xs text-slate-400">
          从 WB 上游模型目录接口拉取并替换 wb_model_catalog.json（倍率/思考档位/图片模态以服务端为准）；
          网关启动时也会自动同步一次。
        </p>
        {catalog.length === 0 ? (
          <p className="py-4 text-center text-xs text-slate-400">暂无目录数据：点击「同步 WB 模型目录」拉取（需至少一个含凭证的 WB 账号）。</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[640px] text-sm">
              <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                <tr>
                  <th className="px-3 py-2 text-left">模型 ID</th>
                  <th className="px-3 py-2 text-left">展示名</th>
                  <th className="px-3 py-2 text-right">积分倍率</th>
                  <th className="px-3 py-2 text-left">思考档位</th>
                  <th className="px-3 py-2 text-right">上下文</th>
                  <th className="px-3 py-2 text-left">图片</th>
                </tr>
              </thead>
              <tbody>
                {catalog.map((m) => (
                  <tr key={m.id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                    <td className="px-3 py-2 font-mono text-xs">{m.id}</td>
                    <td className="px-3 py-2">{m.display || '—'}</td>
                    <td className="px-3 py-2 text-right tabular-nums text-amber-600 dark:text-amber-400">{m.rate.toFixed(2)}</td>
                    <td className="px-3 py-2 text-xs text-slate-500">
                      {m.effort_override
                        ? `${m.effort_override}（修正）`
                        : m.supported_efforts.length > 0
                          ? m.supported_efforts.join(' / ')
                          : '—'}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums text-xs text-slate-500">
                      {(m.context_length / 1000).toFixed(0)}k
                    </td>
                    <td className="px-3 py-2">{m.supports_image ? <Badge tone="violet">支持</Badge> : <span className="text-xs text-slate-300">-</span>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* WB 账号池状态卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <span className="text-sm font-medium">WB 账号池（上游凭证源）</span>
          <span className="text-xs text-slate-400">{credAccounts.length} 个含凭证账号参与 WB 上游调度</span>
        </div>
        {credAccounts.length === 0 ? (
          <p className="py-4 text-center text-xs text-slate-400">
            暂无含凭证账号：请先在「账号管理」导入本机账号或 OAuth 扫码入池（凭证写入本地 token store）。
          </p>
        ) : (
          <div className="space-y-1">
            {credAccounts.map((a) => (
              <div
                key={a.id}
                className="flex items-center gap-3 rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800"
              >
                <div className="min-w-0 flex-1 truncate font-medium">{a.nickname || a.id}</div>
                {a.is_current && <Badge tone="green">在线</Badge>}
                <span className="w-24 text-right tabular-nums text-xs text-slate-500">
                  {a.credits_balance != null ? `${a.credits_balance.toFixed(2)} 积分` : '余额未知'}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* WB 用量统计（独立 wb_days 桶落盘，仅统计 WB 上游请求，服务未运行也可查看） */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
          <div className="flex items-center gap-2">
            <BarChart3 size={16} className="text-brand-500" />
            <span className="text-sm font-medium">WB 用量统计</span>
            <span className="hidden text-xs text-slate-400 sm:inline">仅 WB 上游请求 · 独立于服务运行状态</span>
          </div>
          <div className="flex items-center gap-1">
            {[7, 14, 30].map((d) => (
              <button
                key={d}
                className={
                  'rounded-md px-2 py-1 text-xs transition ' +
                  (usageDays === d
                    ? 'bg-brand-500/10 font-medium text-brand-600 dark:text-brand-400'
                    : 'text-slate-500 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800')
                }
                onClick={() => setUsageDays(d)}
              >
                {d}天
              </button>
            ))}
            <button
              className="btn-ghost ml-1 flex items-center gap-1 text-xs"
              onClick={() => void loadUsage(usageDays)}
              disabled={usageLoading}
            >
              <RefreshCw size={13} className={usageLoading ? 'animate-spin' : ''} />
              刷新
            </button>
          </div>
        </div>

        <div className="mb-4 grid grid-cols-2 gap-3 sm:grid-cols-4">
          <StatCard
            label="WB 请求数"
            value={usageSummary.requests}
            tone="brand"
            hint={`近 ${usageDays} 天`}
          />
          <StatCard
            label="成功率"
            value={usageSummary.successRate === '—' ? '—' : `${usageSummary.successRate}%`}
            tone="green"
            hint={`失败 ${usageSummary.errors} 次`}
          />
          <StatCard
            label="Token 消耗"
            value={fmtTokens(usageSummary.prompt + usageSummary.completion)}
            tone="amber"
            hint={`输入 ${fmtTokens(usageSummary.prompt)} / 输出 ${fmtTokens(usageSummary.completion)}`}
          />
          <StatCard
            label="平均耗时"
            value={usageSummary.requests > 0 ? `${usageSummary.avgDuration}ms` : '—'}
            tone="blue"
            hint="按请求加权"
          />
        </div>

        {usageSummary.requests > 0 ? (
          <div className="h-48 text-slate-500 dark:text-zinc-400">
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={usageChartData} margin={{ top: 4, right: 8, bottom: 0, left: -16 }}>
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
          <p className="py-5 text-center text-xs text-slate-400">
            暂无 WB 请求数据 — 通过本网关调用 WB 目录模型（消耗 WB 积分）后，这里会展示按日趋势
          </p>
        )}

        {usageSummary.topModels.length > 0 && (
          <div className="mt-3">
            <p className="mb-2 text-xs font-medium text-slate-500 dark:text-zinc-400">
              模型分布（近 {usageDays} 天 Top 5）
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
                  {usageSummary.topModels.map((m) => {
                    const pct = usageSummary.requests > 0 ? (m.requests / usageSummary.requests) * 100 : 0;
                    return (
                      <tr key={m.name} className="border-b border-slate-100 last:border-0 dark:border-zinc-800">
                        <td className="py-1.5 pr-4 font-mono text-xs font-medium text-slate-700 dark:text-zinc-200">
                          {m.name}
                        </td>
                        <td className="py-1.5 pr-4 tabular-nums text-slate-600 dark:text-zinc-300">{m.requests}</td>
                        <td className="py-1.5 pr-4 tabular-nums text-emerald-600 dark:text-emerald-400">{m.ok}</td>
                        <td className="py-1.5 pr-4 tabular-nums text-rose-600 dark:text-rose-400">{m.errors}</td>
                        <td className="w-40 py-1.5">
                          <div className="flex items-center gap-2">
                            <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-slate-100 dark:bg-zinc-800">
                              <div
                                className="h-full rounded-full bg-brand-500"
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

      {/* 使用方式卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-2 flex items-center justify-between">
          <span className="text-sm font-medium">使用方式 & 配置示例</span>
          <button className="btn-outline" onClick={() => void copyExample()} disabled={copying}>
            <Copy size={13} /> {copying ? '复制中…' : '复制示例'}
          </button>
        </div>
        <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-800/50 dark:text-zinc-400">
          <div>
            接口地址：<code className="break-all text-[11px]">http://127.0.0.1:{status?.port ?? 7864}/v1</code>
          </div>
          <div className="mt-1">
            API Key：<code className="text-[11px]">在 Trae「API 服务」页的「API Keys 管理」中创建并复制（网关共用）</code>
          </div>
          <div className="mt-1">
            模型 ID：<code className="text-[11px]">{catalog[0]?.id ?? '（同步目录后展示）'}</code> 及上表其他 WB 模型
          </div>
          <div className="mt-2 text-[11px]">
            Claude Code / Codex CLI 用户：可用下方「生态接入」把 WB 端点注册进 CC Switch（注册的是 WB 侧独立条目，与 Trae 侧互不影响）。
          </div>
        </div>
      </div>

      {/* 生态接入：CC Switch 协同（WB 侧独立条目） */}
      <div className="mt-4 card p-4">
        <div className="mb-2 flex items-center gap-2">
          <Plug size={16} className="text-emerald-500" />
          <span className="text-sm font-medium">生态接入</span>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            className="btn-outline !py-1.5 text-xs"
            onClick={() => void registerCcSwitch('claude')}
            disabled={ccBusy !== null}
            title="把网关 Anthropic 端点注册进 CC Switch（WB 侧条目「WorkBuddy 网关」，默认模型取 WB 目录首个）"
          >
            注册到 CC Switch（Claude Code）
          </button>
          <button
            className="btn-outline !py-1.5 text-xs"
            onClick={() => void registerCcSwitch('codex')}
            disabled={ccBusy !== null}
            title="把网关 Responses 端点注册进 CC Switch（WB 侧条目「WorkBuddy 网关」，默认模型取 WB 目录首个）"
          >
            注册到 CC Switch（Codex）
          </button>
        </div>
        {ecoNote && (
          <p className="mt-2 break-all text-[11px] leading-4 text-slate-500 dark:text-zinc-400">
            {ecoNote}
          </p>
        )}
        <p className="mt-1 text-[11px] text-slate-400 dark:text-zinc-500">
          WB 侧注册「WorkBuddy 网关」独立条目（默认模型 {catalog[0]?.id ?? 'hy4'}），与 Trae「API 服务」页注册的「AI Work 助手网关」互不覆盖；
          CC Switch 注册会先整库备份至 ~/.cc-switch/backups/，仅写入本网关条目，写入后需重启 CC Switch 生效。
        </p>
      </div>
    </div>
  );
}
