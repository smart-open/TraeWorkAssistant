import { useCallback, useEffect, useMemo, useState } from 'react';
import { RefreshCw, Save, Coins, ToggleLeft, Activity, Gauge } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner, StatCard } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type {
  ApiServiceStatus,
  ApiPoolFile,
  PoolStatus,
  UsageDayView,
  WbModelInfo,
  WorkBuddyAccountView,
} from '../../types';

/**
 * Buddy · 资源调度（unified-api-gateway-design §6.2，Phase 3）
 * 页内仅保留 Buddy 资源级内容：积分体系说明 / 池指标行 /
 * 左列（资源开关 + 调度参数合并面板，共用「保存」）/ 右列（账号池选择 + 模型目录（Buddy）上下排布），
 * 同行左右两列各占 1/2。
 * 网关级功能（服务启停 / 使用方式 / 生态接入 / WB 用量统计）已全部迁至
 * 全局 API 管理弹窗（左侧栏 KeyRound 图标），页内不再出现网关级内容。
 */

// Buddy 资源开关字段（wb_enabled 上游总开关 / 默认深度思考 / 工具执行 / 后台任务降级 / 长上下文降档）
const WB_FLAG_FIELDS: { key: 'wbEnabled' | 'wbDefaultThinking' | 'wbToolExec' | 'wbBgDowngrade' | 'wbLongctxDowngrade'; label: string; desc: string }[] = [
  {
    key: 'wbEnabled',
    label: '启用 WB 上游',
    desc: 'Buddy 目录模型路由到 WB 账号池，消耗 Buddy 积分（关闭后仅 Buddy 源模型显式报错）',
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
  {
    key: 'wbLongctxDowngrade',
    label: '长上下文降档',
    desc: '输入粗估 ≥100k token 的请求自动换 flash 档低倍率模型',
  },
];

/** F-76②/③/F-77 数值参数（保存时写回 api_pool.json，服务运行中热生效） */
const WB_PARAM_FIELDS: {
  key: 'wbHedgeThresholdMs' | 'accountConcurrencyLimit' | 'poolStickyTtlSecs' | 'wbStickyTtlSecs';
  label: string;
  desc: string;
  min: number;
  max: number;
  step: number;
  unit: string;
}[] = [
  {
    key: 'wbHedgeThresholdMs',
    label: '竞速对冲阈值',
    desc: '流式首字节超过该时长即向第二账号发对冲请求，先出首字者胜；0 = 关闭（有效范围 1s–8s，与后端对齐）',
    min: 0,
    max: 8_000,
    step: 500,
    unit: 'ms',
  },
  {
    key: 'accountConcurrencyLimit',
    label: '账号并发上限',
    desc: '单账号在途请求数达到上限即让位其他账号（全部 busy 时取负载最小者）；0 = 不限',
    min: 0,
    max: 32,
    step: 1,
    unit: '并发',
  },
  {
    key: 'poolStickyTtlSecs',
    label: '池粘性 TTL',
    desc: 'TTL 内同会话落同一账号（上游 KV cache 复用）',
    min: 0,
    max: 3600,
    step: 30,
    unit: '秒',
  },
  {
    key: 'wbStickyTtlSecs',
    label: '会话粘性 TTL',
    desc: '显式 conversationId 绑定账号的有效期',
    min: 0,
    max: 86_400,
    step: 60,
    unit: '秒',
  },
];

/** 数值参数默认值（与后端 serde default 对齐：对冲 8s / 并发 1 / 池粘性 300s / 会话粘性 1800s） */
const WB_PARAM_DEFAULTS = {
  wbHedgeThresholdMs: 8_000,
  accountConcurrencyLimit: 1,
  poolStickyTtlSecs: 300,
  wbStickyTtlSecs: 1800,
};

export default function BuddyApiService() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [status, setStatus] = useState<ApiServiceStatus | null>(null);
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [wbFlags, setWbFlags] = useState({
    wbEnabled: false,
    wbDefaultThinking: false,
    wbToolExec: false,
    wbBgDowngrade: false,
    wbLongctxDowngrade: false,
  });
  const [wbParams, setWbParams] = useState({ ...WB_PARAM_DEFAULTS });
  // Buddy 池入池白名单（wb-<hash> 账号 id = a.id，与网关池键/PoolStatus.uid 同域；
  // 注意不是 a.uid——那是真实账号 uuid）：null = 未自定义（后端按「全部含凭证账号」自动入池）
  const [wbUids, setWbUids] = useState<string[] | null>(null);
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [catalog, setCatalog] = useState<WbModelInfo[]>([]);
  const [syncing, setSyncing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  // 当日活跃账号数据源（今日 WB 用量桶的账号维度计数）
  const [usage, setUsage] = useState<UsageDayView[]>([]);
  // WB 池实时状态（F-77⑤ 可观测：per-account inflight 在途计数）
  const [wbPool, setWbPool] = useState<PoolStatus[]>([]);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [st, pf, accs, cat, wbp] = await Promise.all([
        api.apiServer.status().catch(() => null),
        api.apiServer.poolList().catch(() => null),
        api.workbuddy.accountsList().catch(() => [] as WorkBuddyAccountView[]),
        api.apiServer.wbCatalogList().catch(() => [] as WbModelInfo[]),
        api.apiServer.wbPoolStatus().catch(() => [] as PoolStatus[]),
      ]);
      setStatus(st);
      setPool(pf);
      setAccounts(accs);
      setCatalog(cat);
      setWbPool(wbp);
      if (pf) {
        setWbFlags({
          wbEnabled: pf.wb_enabled ?? false,
          wbDefaultThinking: pf.wb_default_thinking ?? false,
          wbToolExec: pf.wb_tool_exec ?? false,
          wbBgDowngrade: pf.wb_bg_downgrade ?? false,
          wbLongctxDowngrade: pf.wb_longctx_downgrade ?? false,
        });
        setWbParams({
          wbHedgeThresholdMs: pf.wb_hedge_threshold_ms ?? WB_PARAM_DEFAULTS.wbHedgeThresholdMs,
          accountConcurrencyLimit: pf.account_concurrency_limit ?? WB_PARAM_DEFAULTS.accountConcurrencyLimit,
          poolStickyTtlSecs: pf.pool_sticky_ttl_secs ?? WB_PARAM_DEFAULTS.poolStickyTtlSecs,
          wbStickyTtlSecs: pf.wb_sticky_ttl_secs ?? WB_PARAM_DEFAULTS.wbStickyTtlSecs,
        });
        // 空数组 = fail-open（全部自动入池）→ 视为未自定义，显示为全选
        setWbUids(pf.wb_enabled_uids?.length ? pf.wb_enabled_uids : null);
      }
    } catch (err) {
      pushToast('error', `读取资源状态失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 当日 WB 用量（query_recent 含今日；仅用于「今日使用」池指标）
  const loadTodayUsage = useCallback(async () => {
    try {
      setUsage(await api.apiServer.wbUsageStats(1));
    } catch {
      /* 加载失败按无数据处理 */
    }
  }, []);

  useEffect(() => {
    void loadTodayUsage();
  }, [loadTodayUsage]);

  // 保存资源开关：uids/strategy/groups 原样回传（本页不改 Trae 池配置）；
  // WB 白名单随本次保存提交：未自定义传 null（后端保留原值，fail-open 语义不变，
  // 新增账号可持续自动入池；显式全量名单会冻结 fail-open）；清空传 []（后端同样 fail-open）；
  // F-76②/③/F-77 数值参数随开关一起保存（后端热应用，运行中即时生效）
  const saveFlags = async () => {
    setSaving(true);
    try {
      await withMinDelay(
        api.apiServer.poolSet(pool?.enabled_uids ?? [], pool?.strategy, pool?.group_ids, {
          ...wbFlags,
          wbUids,
          wbHedgeThresholdMs: wbParams.wbHedgeThresholdMs,
          accountConcurrencyLimit: wbParams.accountConcurrencyLimit,
          poolStickyTtlSecs: wbParams.poolStickyTtlSecs,
          wbStickyTtlSecs: wbParams.wbStickyTtlSecs,
        }),
        600,
      );
      pushToast('success', '资源开关与调度参数已保存（服务运行中即时生效）');
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const syncCatalog = async () => {
    if (syncing) return;
    setSyncing(true);
    try {
      const n = await withMinDelay(api.apiServer.wbCatalogSync(), 800);
      pushToast('success', `Buddy 模型目录已更新（${n} 个模型），/v1/models 与路由即时生效`);
      setCatalog(await api.apiServer.wbCatalogList());
    } catch (err) {
      pushToast('error', `Buddy 模型目录同步失败：${String(err).slice(0, 120)}`);
    } finally {
      setSyncing(false);
    }
  };

  const credAccounts = accounts.filter((a) => a.has_credential);
  // 生效白名单：未自定义（null）= 全量含凭证账号（与后端 fail-open 默认一致）；
  // 值域 = a.id（wb-<hash>），非 a.uid
  const wbSelected = wbUids ?? credAccounts.map((a) => a.id);
  const toggleWbUid = (id: string) => {
    const base = wbUids ?? credAccounts.map((a) => a.id);
    setWbUids(base.includes(id) ? base.filter((u) => u !== id) : [...base, id]);
  };

  // 池指标（§6.2 Buddy 口径）：健康 = 含凭证且无需重新登录；今日使用 = 当日被调度使用；池内 = 含凭证账号总数
  const todayKey = new Date().toLocaleDateString('sv-SE');
  const healthyCount = credAccounts.filter((a) => !a.needs_relogin).length;
  const activeToday =
    usage.find((d) => d.date === todayKey)?.accounts.filter((a) => a.requests > 0).length ?? 0;
  const totalBuddyCredits = useMemo(
    () => credAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0),
    [credAccounts],
  );

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 资源调度"
        desc="服务资源池管理 · 上游开关与模型目录 · 资源提供给API网关使用"
        actions={
          <button className="btn-outline" onClick={() => void refresh()} disabled={refreshing}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      {/* 积分体系说明（资源级） */}
      <div className="rounded-xl border border-amber-300/70 bg-amber-50/80 px-3.5 py-2.5 dark:border-amber-700/40 dark:bg-amber-900/10">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-semibold text-amber-800 dark:text-amber-200">积分体系说明</span>
          <span className="rounded bg-amber-200 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-700/50 dark:text-amber-100">本服务消耗 Buddy 积分</span>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-slate-500 dark:text-zinc-400">
          <span>
            Buddy 积分：签到 / 成长中心获取，各模型按倍率消耗；Buddy 目录模型的请求由 WB 账号池服务，
            轮换消耗各账号积分（粘性会话 / 模型级冷却同现有实现）。
          </span>
          <span className="ml-auto">
            含凭证账号 Buddy 积分总余额：
            <span className="font-bold tabular-nums text-amber-700 dark:text-amber-300">
              {credAccounts.some((a) => a.credits_balance != null)
                ? totalBuddyCredits.toLocaleString('zh-CN', { maximumFractionDigits: 2 })
                : '未知'}
            </span>
          </span>
        </div>
      </div>

      {/* 池指标行 */}
      <div className="mt-4 grid grid-cols-3 gap-3">
        <StatCard
          label="健康账号"
          value={healthyCount}
          tone="green"
          hint="含凭证且无需重新登录，可参与调度"
        />
        <StatCard
          label="今日使用"
          value={activeToday}
          tone="blue"
          hint="当日被调度使用过的账号"
        />
        <StatCard
          label="池内账号"
          value={credAccounts.length}
          tone="violet"
          hint="含凭证账号总数（wb_pool）"
        />
      </div>

      {/* 资源开关 + 调度参数（左列，合并面板）｜账号池选择 + 模型目录（右列），同行两列各占 1/2 */}
      <div className="mt-4 grid grid-cols-12 items-start gap-4">
        {/* 左列：资源开关 + 调度参数（合并为一个面板，共用「保存」，两者本就随 poolSet 一并落盘） */}
        <div className="col-span-6">
          <div className="card p-4">
            <div className="mb-3 flex items-center justify-between">
              <div className="flex items-center gap-2">
                <ToggleLeft size={16} className="text-brand-500" />
                <span className="text-sm font-medium">资源开关与调度参数</span>
              </div>
              <button className="btn-outline" onClick={() => void saveFlags()} disabled={saving}>
                {saving ? <Spinner /> : <Save size={15} />} 保存
              </button>
            </div>
            <div className="mb-2">
              {wbFlags.wbEnabled ? (
                <Badge tone="green">上游已启用</Badge>
              ) : (
                <Badge tone="amber">上游未启用 — Buddy 源模型将显式报错</Badge>
              )}
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

            {/* 调度参数（F-76②/③/F-77：数值热参数，与资源开关同面板，保存即热生效） */}
            <div className="mt-4 border-t border-slate-100 pt-3 dark:border-zinc-800">
              <div className="mb-2 flex items-center gap-2">
                <Gauge size={15} className="text-brand-500" />
                <span className="text-sm font-medium">调度参数</span>
                <span className="text-xs text-slate-400">保存即热生效 · 0 表示关闭/不限</span>
              </div>
              <div className="space-y-3">
                {WB_PARAM_FIELDS.map((item) => (
                  <div key={item.key} className="rounded-md px-1.5 py-1.5">
                    <div className="flex items-center justify-between gap-3">
                      <span className="text-xs text-slate-700 dark:text-zinc-200">{item.label}</span>
                      <div className="flex items-center gap-2">
                        <input
                          type="number"
                          className="w-24 rounded-md border border-slate-200 bg-transparent px-2 py-1 text-right text-xs tabular-nums focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900"
                          min={item.min}
                          max={item.max}
                          step={item.step}
                          value={wbParams[item.key]}
                          onChange={(e) => {
                            const v = Number(e.target.value);
                            setWbParams((prev) => ({
                              ...prev,
                              [item.key]: Number.isFinite(v)
                                ? Math.min(item.max, Math.max(item.min, v))
                                : prev[item.key],
                            }));
                          }}
                        />
                        <span className="w-8 shrink-0 text-[11px] text-slate-400">{item.unit}</span>
                      </div>
                    </div>
                    <p className="mt-1 text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                      {item.desc}
                    </p>
                  </div>
                ))}
              </div>
            </div>

            <p className="mt-3 text-xs text-slate-400 dark:text-zinc-500">
              保存后即时生效；Trae 池的调度策略与分组筛选在 Trae「资源调度」页配置，本页不改动。
            </p>
          </div>
        </div>

        {/* 右列：账号池选择（上）+ 模型目录（Buddy）（下），上下排布 */}
        <div className="col-span-6 space-y-4">
          {/* 账号池选择卡（现有 WB 池状态卡功能保留迁移；勾选即自定义入池白名单） */}
          <div className="card p-4">
            <div className="mb-3 flex items-center justify-between">
              <div className="flex items-center gap-2">
                <Activity size={16} className="text-brand-500" />
                <span className="text-sm font-medium">账号池选择</span>
                <span className="text-xs text-slate-400">
                  {credAccounts.length > 0 && `已选 ${wbSelected.length}/${credAccounts.length}`}
                </span>
              </div>
              {credAccounts.length > 0 && (
                <div className="flex items-center gap-1">
                  <button
                    className="btn-ghost px-2 py-0.5 text-xs"
                    onClick={() => setWbUids(credAccounts.map((a) => a.id))}
                  >
                    全选
                  </button>
                  <button className="btn-ghost px-2 py-0.5 text-xs" onClick={() => setWbUids([])}>
                    清空
                  </button>
                </div>
              )}
            </div>
            <p className="mb-2 text-xs text-slate-400">
              {credAccounts.length === 0
                ? '暂无含凭证账号'
                : `勾选账号参与 WB 上游调度（清空 = 全部含凭证账号自动入池）；保存后即时生效`}
            </p>
            {credAccounts.length === 0 ? (
              <p className="py-4 text-center text-xs text-slate-400">
                暂无含凭证账号：请先在「账号管理」导入本机账号或 OAuth 扫码入池（凭证写入本地 token store）。
              </p>
            ) : (
              <div className="space-y-1">
                {credAccounts.map((a) => {
                  // F-77⑤ 可观测：实时在途并发（服务未运行/未匹配时为 0）；
                  // PoolStatus.uid = 池键 = a.id（wb-<hash>），非 a.uid
                  const inflight = wbPool.find((p) => p.uid === a.id)?.inflight ?? 0;
                  return (
                    <div
                      key={a.id}
                      className="flex items-center gap-3 rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800"
                    >
                      <input
                        type="checkbox"
                        checked={wbSelected.includes(a.id)}
                        onChange={() => toggleWbUid(a.id)}
                      />
                      <div className="min-w-0 flex-1 truncate font-medium">{a.nickname || a.id}</div>
                      {a.is_current && <Badge tone="green">在线</Badge>}
                      {a.needs_relogin && <Badge tone="amber">需重新登录</Badge>}
                      {inflight > 0 && <Badge tone="amber">在途 {inflight}</Badge>}
                      <span className="shrink-0 text-right tabular-nums text-xs text-slate-500">
                        {a.credits_balance != null ? `${a.credits_balance.toFixed(2)} 积分` : '余额未知'}
                      </span>
                    </div>
                  );
                })}
              </div>
            )}
          </div>

          {/* 同步官网模型（Buddy）卡（现有目录同步能力保留） */}
          <div className="card p-4">
            <div className="mb-3 flex items-center justify-between">
              <div className="flex items-center gap-2">
                <Coins size={16} className="text-amber-500" />
                <span className="text-sm font-medium">同步官网模型（Buddy）</span>
                <span className="text-xs text-slate-400">{catalog.length} 个模型</span>
              </div>
              <button className="btn-outline" onClick={() => void syncCatalog()} disabled={syncing}>
                <RefreshCw size={14} className={syncing ? 'animate-spin' : ''} />
                {syncing ? '同步中…' : '同步官网模型'}
              </button>
            </div>
            <p className="mb-3 text-xs text-slate-400">
              从 Buddy 上游模型目录接口拉取并替换 wb_model_catalog.json（倍率/思考档位/图片模态以服务端为准）；
              网关启动时也会自动同步一次。
            </p>
            {catalog.length === 0 ? (
              <p className="py-4 text-center text-xs text-slate-400">暂无目录数据：点击「同步目录」拉取（需至少一个含凭证的 WB 账号）。</p>
            ) : (
              <div className="overflow-x-auto">
                <table className="w-full min-w-[560px] text-sm">
                  <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                    <tr>
                      <th className="px-3 py-2 text-left">模型 ID</th>
                      <th className="px-3 py-2 text-left">展示名</th>
                      <th className="px-3 py-2 text-right">积分倍率</th>
                      <th className="px-3 py-2 text-left">思考档位</th>
                      <th className="px-3 py-2 text-right">上下文</th>
                      <th className="px-3 py-2 text-center">图片支持</th>
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
                        <td className="px-3 py-2 text-center text-xs">
                          {m.supports_image ? (
                            <span className="font-semibold text-emerald-600 dark:text-emerald-400">✓</span>
                          ) : (
                            <span className="text-slate-400 dark:text-zinc-500">✗</span>
                          )}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
