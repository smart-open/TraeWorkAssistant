import { useCallback, useEffect, useMemo, useState } from 'react';
import { RefreshCw, Save, Coins, ToggleLeft, Activity, Layers } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner, StatCard } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type {
  ApiServiceStatus,
  ApiPoolFile,
  GroupView,
  PoolStatus,
  UnifiedModel,
  UsageDayView,
  QoderAccountView,
} from '../../types';

/**
 * Qoder · 资源调度（对齐方案 P1 + 账号池配置对齐 Buddy）
 * 参照 Buddy「资源调度」页同构布局，按 Qoder 实际能力裁剪：
 * - 资源开关仅 qoderEnabled 一个（Qoder 无路由级 effort/工具代执行等 wb 同构特性）；
 * - 账号池选择与 Buddy 同构：勾选白名单（清空 = 全部含凭证账号 fail-open 入池）+
 *   分组筛选（qoder_groups 分组体系，账号 group_id 随账号池持久化）；
 * - 模型目录数据源为统一目录聚合（api.apiServer.unifiedModels 过滤 qoder 源），
 *   手动同步复用每日调度任务入口（qoderCatalogSync），每日定时时刻存 app Settings。
 * 网关级功能（服务启停 / 接口配置 / API Keys / 用量统计）在全局 API 管理弹窗，
 * 页内不重复。
 */

export default function QoderApiService() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [status, setStatus] = useState<ApiServiceStatus | null>(null);
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [qoderEnabled, setQoderEnabled] = useState(false);
  // F-80-余 v2：竞速对冲阈值（0 = 关闭）+ 会话粘性开关（默认关）
  const [qoderHedgeThresholdMs, setQoderHedgeThresholdMs] = useState(8_000);
  const [qoderStickyEnabled, setQoderStickyEnabled] = useState(false);
  // per-pool 调度参数（Qoder 池专属，与 Trae/Buddy 互不共享）：并发 0 = 不限、池粘性 0 = 关
  const [qoderAccountConcurrencyLimit, setQoderAccountConcurrencyLimit] = useState(1);
  const [qoderPoolStickyTtlSecs, setQoderPoolStickyTtlSecs] = useState(300);
  const [qoderStickyTtlSecs, setQoderStickyTtlSecs] = useState(1800);
  // 账号池选择（对齐 Buddy）：null = 未自定义（fail-open 全量，显示为全选）；
  // 值域 = a.id（qd- 前缀账号 id）；分组筛选空集 = 不限分组
  const [qoderUids, setQoderUids] = useState<string[] | null>(null);
  const [qoderPoolGroups, setQoderPoolGroups] = useState<Set<string>>(new Set());
  const [qoderGroups, setQoderGroups] = useState<GroupView[]>([]);
  const [accounts, setAccounts] = useState<QoderAccountView[]>([]);
  const [models, setModels] = useState<UnifiedModel[]>([]);
  const [syncing, setSyncing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  // 模型目录定时同步配置（qoder_catalog_sync_enabled/hhmm 存 app Settings，独立保存）
  const [catSync, setCatSync] = useState({ enabled: true, hhmm: '05:50' });
  const [savingCatSync, setSavingCatSync] = useState(false);
  // 当日活跃账号数据源（今日 qoder 用量桶的账号维度计数）
  const [usage, setUsage] = useState<UsageDayView[]>([]);
  // Qoder 池实时状态（可观测：per-account inflight 在途计数）
  const [qoderPool, setQoderPool] = useState<PoolStatus[]>([]);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [st, pf, accs, cat, groups] = await Promise.all([
        api.apiServer.status().catch(() => null),
        api.apiServer.poolList().catch(() => null),
        api.qoder.accountsList().catch(() => [] as QoderAccountView[]),
        api.apiServer
          .unifiedModels()
          .then((list) => list.filter((m) => m.sources.some((s) => s.pool === 'qoder')))
          .catch(() => [] as UnifiedModel[]),
        api.qoder.groups.list().catch(() => [] as GroupView[]),
      ]);
      setStatus(st);
      setPool(pf);
      setAccounts(accs);
      setModels(cat);
      setQoderGroups(groups);
      if (pf) {
        setQoderEnabled(pf.qoder_enabled ?? false);
        // F-80-余 v2 参数回显（缺省对齐后端 serde default：对冲 8s / 粘性关）
        setQoderHedgeThresholdMs(pf.qoder_hedge_threshold_ms ?? 8_000);
        setQoderStickyEnabled(pf.qoder_sticky_enabled ?? false);
        // per-pool 调度参数回显（缺省对齐后端 serde default：并发 1 / 池粘性 300s / 会话粘性 1800s）
        setQoderAccountConcurrencyLimit(pf.qoder_account_concurrency_limit ?? 1);
        setQoderPoolStickyTtlSecs(pf.qoder_pool_sticky_ttl_secs ?? 300);
        setQoderStickyTtlSecs(pf.qoder_sticky_ttl_secs ?? 1800);
        // 空数组 = fail-open（全部自动入池）→ 视为未自定义，显示为全选（对齐 Buddy）
        setQoderUids(pf.qoder_enabled_uids?.length ? pf.qoder_enabled_uids : null);
        // 分组筛选：空 = 不限（全部参与）
        setQoderPoolGroups(new Set(pf.qoder_group_ids ?? []));
      }
      api.misc
        .settingsGet()
        .then((s) =>
          setCatSync({ enabled: s.qoder_catalog_sync_enabled ?? true, hhmm: s.qoder_catalog_sync_hhmm || '05:50' }),
        )
        .catch(() => {});
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

  // 当日 Qoder 用量（query_recent 含今日；仅用于「今日使用」池指标）
  const loadTodayUsage = useCallback(async () => {
    try {
      setUsage(await api.apiServer.qoderUsageStats(1));
    } catch {
      /* 加载失败按无数据处理 */
    }
  }, []);

  useEffect(() => {
    void loadTodayUsage();
  }, [loadTodayUsage]);

  // 服务运行中轮询 Qoder 池实时状态（inflight 徽标；3s 同 Trae/Buddy 页惯例）
  const refreshPoolStatus = useCallback(async () => {
    if (!status?.running) {
      setQoderPool([]);
      return;
    }
    try {
      setQoderPool(await api.apiServer.qoderPoolStatus());
    } catch {
      /* ignore */
    }
  }, [status?.running]);

  useEffect(() => {
    void refreshPoolStatus();
    if (!status?.running) return;
    const id = setInterval(() => void refreshPoolStatus(), 3000);
    return () => clearInterval(id);
  }, [refreshPoolStatus]);

  // 保存资源开关 + 账号池选择：uids/strategy/groups 原样回传（本页不改 Trae/WB 池配置）；
  // qoderEnabled/白名单/分组随本次保存提交（成员/分组变更服务运行中热重载即时生效）
  const saveFlags = async () => {
    // 池配置未加载时禁止保存：uids/strategy/groups 原样回传依赖 pool 快照，
    // pool=null 时保存会把 Trae 池 enabled_uids 清空（与 Buddy 页同款防护）
    if (!pool) {
      pushToast('error', '池配置未加载，无法保存（请先刷新重试）');
      return;
    }
    setSaving(true);
    try {
      await withMinDelay(
        api.apiServer.poolSet(pool?.enabled_uids ?? [], pool?.strategy, pool?.group_ids, {
          qoderEnabled,
          qoderHedgeThresholdMs,
          qoderStickyEnabled,
          qoderAccountConcurrencyLimit,
          qoderPoolStickyTtlSecs,
          qoderStickyTtlSecs,
          // null = 未自定义（后端保留原值保持 fail-open）；数组 = 白名单覆盖（[] = 清空恢复全量）
          ...(qoderUids !== null ? { qoderUids } : {}),
          qoderGroupIds: Array.from(qoderPoolGroups),
        }),
        600,
      );
      pushToast('success', 'Qoder 上游配置已保存（服务运行中即时生效）');
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  // 定时同步配置保存：settings_set 真 patch 语义，只写本卡两个字段
  const saveCatSync = async () => {
    setSavingCatSync(true);
    try {
      await withMinDelay(
        api.misc.settingsSet({
          qoder_catalog_sync_enabled: catSync.enabled,
          qoder_catalog_sync_hhmm: catSync.hhmm.trim() || '05:50',
        }),
        400,
      );
      pushToast(
        'success',
        catSync.enabled
          ? `已保存：每天 ${catSync.hhmm || '05:50'} 自动同步 Qoder 模型目录`
          : '已关闭定时同步（仅手动同步）',
      );
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSavingCatSync(false);
    }
  };

  // 手动同步模型目录：复用每日调度任务入口（按池序逐可用账号拉取 CN 区 model/list）
  const syncCatalog = async () => {
    if (syncing) return;
    setSyncing(true);
    try {
      const n = await withMinDelay(api.apiServer.qoderCatalogSync(), 800);
      pushToast('success', `Qoder 模型目录已更新（${n} 个模型），/v1/models 与路由即时生效`);
      const list = await api.apiServer.unifiedModels();
      setModels(list.filter((m) => m.sources.some((s) => s.pool === 'qoder')));
    } catch (err) {
      pushToast('error', `Qoder 模型目录同步失败：${String(err).slice(0, 120)}`);
    } finally {
      setSyncing(false);
    }
  };

  const credAccounts = useMemo(() => accounts.filter((a) => a.has_credential), [accounts]);

  // 生效白名单：未自定义（null）= 全量含凭证账号（与后端 fail-open 默认一致）；
  // 值域 = a.id（qd- 前缀账号 id）
  const qoderSelected = qoderUids ?? credAccounts.map((a) => a.id);
  const toggleQoderUid = (id: string) => {
    const base = qoderUids ?? credAccounts.map((a) => a.id);
    setQoderUids(base.includes(id) ? base.filter((u) => u !== id) : [...base, id]);
  };

  // 分组筛选实时预览（对齐 Buddy/Trae T10）：GroupView.uids 值域 = a.id，与白名单同域；
  // 后端在池装配层按 qoder_group_ids 过滤账号后再应用白名单交集
  const qoderGroupUidSets = useMemo(
    () => qoderGroups.map((g) => ({ id: g.id, uids: new Set(g.uids ?? []) })),
    [qoderGroups],
  );
  const inQoderFilter = (id: string) =>
    qoderPoolGroups.size === 0 ||
    qoderGroupUidSets.some((g) => qoderPoolGroups.has(g.id) && g.uids.has(id));
  const qoderPoolPreview = (() => {
    // 生效池 = 分组筛选 ∩ 白名单勾选（与后端 apply_pool_snapshot 装配语义一致）；
    // excluded 仅统计分组外账号（未勾选由 checkbox 状态表达，不计入 excluded）
    let inPool = 0;
    let excluded = 0;
    for (const a of credAccounts) {
      if (!inQoderFilter(a.id)) excluded += 1;
      else if (qoderSelected.includes(a.id)) inPool += 1;
    }
    return { inPool, excluded };
  })();

  // 池指标（对齐 Buddy 口径）：健康 = 含凭证且无需重新登录；
  // 今日使用 = 当日被调度使用；池内 = 勾选且符合分组筛选的账号数
  const todayKey = new Date().toLocaleDateString('sv-SE');
  const healthyCount = credAccounts.filter((a) => !a.needs_relogin).length;
  const activeToday =
    usage.find((d) => d.date === todayKey)?.accounts.filter((a) => a.requests > 0).length ?? 0;
  const totalQoderCredits = useMemo(
    () => credAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0),
    [credAccounts],
  );
  const running = status?.running ?? false;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Qoder · 资源调度"
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
          <span className="rounded bg-amber-200 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-700/50 dark:text-amber-100">本服务消耗 Qoder 通用 credits</span>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-slate-500 dark:text-zinc-400">
          <span>
            Qoder 目录模型按倍率消耗通用 credits（x0.0 免费档 ~ x3.2）；Qoder 源模型的请求由
            Qoder 账号池服务（CN 网关），轮换消耗各账号 credits（模型级排队冷却同现有实现）。
          </span>
          <span className="ml-auto">
            含凭证账号 Qoder credits 总余额：
            <span className="font-bold tabular-nums text-amber-700 dark:text-amber-300">
              {credAccounts.some((a) => a.credits_balance != null)
                ? totalQoderCredits.toLocaleString('zh-CN', { maximumFractionDigits: 2 })
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
          value={qoderPoolPreview.inPool}
          tone="violet"
          hint="勾选并符合分组筛选的账号数（清空勾选 = 全部含凭证账号自动入池）"
        />
      </div>

      {/* 左列：账号池选择 + 资源开关与调度参数（合并面板）｜右列：模型目录（Qoder），同行两列各占 1/2（Buddy 同构） */}
      <div className="mt-4 grid grid-cols-12 items-start gap-4">
        {/* 左列：账号池选择（上）+ 资源开关与调度参数（下）合并面板，共用右上角「保存」 */}
        <div className="col-span-6">
          <div className="card p-4">
            {/* 面板头＝账号池选择；「保存」位于面板整体右上角，一次保存账号池选择/资源开关/调度参数 */}
            <div className="mb-3 flex items-center justify-between gap-2">
              <div className="flex items-center gap-2">
                <Activity size={16} className="text-brand-500" />
                <span className="text-sm font-medium">账号池选择</span>
                <span className="text-xs text-slate-400">
                  {credAccounts.length > 0 && `已选 ${qoderSelected.length}/${credAccounts.length}`}
                </span>
              </div>
              <div className="flex items-center gap-1">
                {credAccounts.length > 0 && (
                  <>
                    <button
                      className="btn-ghost px-2 py-0.5 text-xs"
                      onClick={() => setQoderUids(credAccounts.map((a) => a.id))}
                    >
                      全选
                    </button>
                    <button className="btn-ghost px-2 py-0.5 text-xs" onClick={() => setQoderUids([])}>
                      清空
                    </button>
                  </>
                )}
                <button className="btn-outline" onClick={() => void saveFlags()} disabled={saving || !pool}>
                  {saving ? <Spinner /> : <Save size={15} />} 保存
                </button>
              </div>
            </div>
            <p className="mb-2 text-xs text-slate-400">
              {credAccounts.length === 0
                ? '暂无含凭证账号'
                : '勾选账号参与 Qoder 上游调度（清空 = 全部含凭证账号自动入池）；保存后即时生效'}
            </p>

            {/* 分组筛选（对齐 Buddy/T10，保存后热重载即时生效）；分组在「账号管理 → 分组管理」维护。
                分组列表加载失败但仍持有已保存筛选时保留控件区，提供清除出口（防幽灵筛选锁死） */}
            {(qoderGroups.length > 0 || qoderPoolGroups.size > 0) && credAccounts.length > 0 && (
              <div className="mb-3 space-y-2 rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50">
                <div className="flex items-start gap-2">
                  <label className="shrink-0 pt-1 text-xs text-slate-500 dark:text-zinc-400">
                    分组筛选
                  </label>
                  <div className="flex flex-1 flex-wrap gap-1">
                    {qoderGroups.map((g) => {
                      const active = qoderPoolGroups.has(g.id);
                      return (
                        <button
                          key={g.id}
                          type="button"
                          className={`rounded-full border px-2 py-0.5 text-xs transition ${
                            active
                              ? 'border-brand-400 bg-brand-50 text-brand-700 dark:border-brand-500 dark:bg-brand-500/15 dark:text-brand-300'
                              : 'border-slate-200 text-slate-500 hover:border-slate-300 dark:border-zinc-700 dark:text-zinc-400 dark:hover:border-zinc-600'
                          }`}
                          onClick={() =>
                            setQoderPoolGroups((prev) => {
                              const next = new Set(prev);
                              if (next.has(g.id)) next.delete(g.id);
                              else next.add(g.id);
                              return next;
                            })
                          }
                        >
                          {g.name}
                        </button>
                      );
                    })}
                    {qoderGroups.length === 0 && qoderPoolGroups.size > 0 && (
                      <span className="flex items-center gap-2 pt-0.5 text-xs text-amber-600 dark:text-amber-400">
                        分组列表加载失败，仍按已保存的 {qoderPoolGroups.size} 个分组筛选
                        <button
                          type="button"
                          className="underline hover:opacity-80"
                          onClick={() => setQoderPoolGroups(new Set())}
                        >
                          清除筛选
                        </button>
                      </span>
                    )}
                    {qoderPoolGroups.size > 0 && (
                      <span className="pt-0.5 text-xs text-slate-400">
                        将纳入 {qoderPoolPreview.inPool} 个账号
                        {qoderPoolPreview.excluded > 0 &&
                          `，${qoderPoolPreview.excluded} 个分组外账号不参与调度`}
                      </span>
                    )}
                  </div>
                </div>
                <p className="text-xs text-slate-400 dark:text-zinc-500">
                  分组筛选作用于 Qoder 池取号范围，保存后即时生效；不选分组 = 全部参与（分组在「账号管理 → 分组管理」维护）。
                </p>
              </div>
            )}

            {/* 账号清单（勾选白名单：健康状态 / 在途计数 / credits 余额；分组外整行灰显） */}
            {credAccounts.length === 0 ? (
              <p className="py-4 text-center text-xs text-slate-400">
                暂无含凭证账号：请先在「账号管理」PAT 导入或 OAuth 登录入池（凭证写入本地 token store）。
              </p>
            ) : (
              <div className="space-y-1">
                {credAccounts.map((a) => {
                  // 可观测：实时在途并发（服务未运行/未匹配时为 0）；PoolStatus.uid 与账号 id 同域
                  const inflight = qoderPool.find((p) => p.uid === a.id)?.inflight ?? 0;
                  // 分组筛选激活时，分组外账号不参与调度（整行半透明标记，对齐 Buddy/Trae）
                  const filteredOut = !inQoderFilter(a.id);
                  return (
                    <div
                      key={a.id}
                      className={`flex items-center gap-3 rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800 ${
                        filteredOut ? 'opacity-50' : ''
                      }`}
                    >
                      <input
                        type="checkbox"
                        checked={qoderSelected.includes(a.id)}
                        onChange={() => toggleQoderUid(a.id)}
                      />
                      <div className="min-w-0 flex-1 truncate font-medium">{a.nickname || a.id}</div>
                      {filteredOut && <Badge tone="slate">分组外</Badge>}
                      <Badge tone="slate">{a.plan || a.credential_source || '—'}</Badge>
                      {a.needs_relogin && <Badge tone="amber">需重新登录</Badge>}
                      {running && inflight > 0 && <Badge tone="amber">在途 {inflight}</Badge>}
                      <span className="shrink-0 text-right tabular-nums text-xs text-slate-500">
                        {a.credits_balance != null ? `${a.credits_balance.toFixed(2)} credits` : '余额未知'}
                      </span>
                    </div>
                  );
                })}
              </div>
            )}

            <p className="mt-3 text-xs text-slate-400 dark:text-zinc-500">
              保存后即时生效；Trae 池的成员/分组与调度策略在 Trae「资源调度」页配置，Buddy 池配置在
              Buddy「资源调度」页，本页不改动。
            </p>

            {/* 资源开关与调度参数（与账号池选择同面板分节，共用右上角「保存」；Buddy 同构） */}
            <div className="mt-4 flex items-center gap-2 border-t border-slate-100 pt-3 dark:border-zinc-800">
              <ToggleLeft size={16} className="text-brand-500" />
              <span className="text-sm font-medium">资源开关与调度参数</span>
            </div>
            {/* 资源开关（Qoder v1 仅上游总开关；池成员 fail-open 全量入池，无白名单/分组） */}
            <div className="mb-3 space-y-2 rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50">
              <label className="flex cursor-pointer items-start gap-2.5 rounded-md px-1.5 py-1.5 transition hover:bg-slate-100/60 dark:hover:bg-zinc-800/60">
                <input
                  type="checkbox"
                  className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                  checked={qoderEnabled}
                  onChange={() => setQoderEnabled((v) => !v)}
                />
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">启用 Qoder 上游</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    Qoder 目录模型路由到 Qoder 账号池（CN 网关），消耗各账号 credits；
                    关闭后仅 Qoder 源模型显式报错，Trae/Buddy 不受影响
                  </span>
                </span>
              </label>
              <div className="px-1.5">
                {qoderEnabled ? (
                  <Badge tone="green">上游已启用</Badge>
                ) : (
                  <Badge tone="amber">上游未启用 — Qoder 源模型将显式报错</Badge>
                )}
              </div>
              <p className="px-1.5 text-[11px] text-slate-400 dark:text-zinc-500">
                上方勾选与分组筛选决定 Qoder 池取号范围（清空勾选 = 全部含凭证账号自动入池）；
                池间调度序为 Buddy → Trae → Qoder（Qoder 尾部接管），池内策略在全局 API
                管理「调度策略中心」配置。
              </p>
              {/* per-pool 调度参数（Qoder 池专属，与 Trae/Buddy 互不共享）：保存即热生效 */}
              <div className="flex items-center justify-between gap-3 px-1.5 pt-1">
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">账号并发上限</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    单账号在途请求数达到上限即让位其他账号（全部 busy 时取负载最小者）；0 = 不限
                  </span>
                </span>
                <span className="flex shrink-0 items-center gap-1.5">
                  <input
                    type="number"
                    min={0}
                    max={32}
                    step={1}
                    value={qoderAccountConcurrencyLimit}
                    onChange={(e) =>
                      setQoderAccountConcurrencyLimit(
                        Math.max(0, Math.min(32, Number(e.target.value) || 0)),
                      )
                    }
                    className="w-24 rounded-lg border border-slate-200 bg-white px-2 py-1.5 text-right text-xs tabular-nums text-slate-700 focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                  <span className="text-[11px] text-slate-400">并发</span>
                </span>
              </div>
              <div className="flex items-center justify-between gap-3 px-1.5">
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">池粘性 TTL</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    TTL 内同会话落同一账号（上游 KV cache 复用）；0 = 关闭
                  </span>
                </span>
                <span className="flex shrink-0 items-center gap-1.5">
                  <input
                    type="number"
                    min={0}
                    max={3600}
                    step={30}
                    value={qoderPoolStickyTtlSecs}
                    onChange={(e) =>
                      setQoderPoolStickyTtlSecs(
                        Math.max(0, Math.min(3600, Number(e.target.value) || 0)),
                      )
                    }
                    className="w-24 rounded-lg border border-slate-200 bg-white px-2 py-1.5 text-right text-xs tabular-nums text-slate-700 focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                  <span className="text-[11px] text-slate-400">秒</span>
                </span>
              </div>
              <div className="flex items-center justify-between gap-3 px-1.5">
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">会话粘性 TTL</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    显式 conversationId 绑定账号的有效期（仅会话粘性开启时生效）
                  </span>
                </span>
                <span className="flex shrink-0 items-center gap-1.5">
                  <input
                    type="number"
                    min={0}
                    max={86400}
                    step={60}
                    value={qoderStickyTtlSecs}
                    onChange={(e) =>
                      setQoderStickyTtlSecs(
                        Math.max(0, Math.min(86400, Number(e.target.value) || 0)),
                      )
                    }
                    className="w-24 rounded-lg border border-slate-200 bg-white px-2 py-1.5 text-right text-xs tabular-nums text-slate-700 focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                  <span className="text-[11px] text-slate-400">秒</span>
                </span>
              </div>
              {/* F-80-余 v2：会话粘性开关（默认关；同账号+同种子派生同一上游 session） */}
              <label className="flex cursor-pointer items-start gap-2.5 rounded-md px-1.5 py-1.5 transition hover:bg-slate-100/60 dark:hover:bg-zinc-800/60">
                <input
                  type="checkbox"
                  className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                  checked={qoderStickyEnabled}
                  onChange={() => setQoderStickyEnabled((v) => !v)}
                />
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">会话粘性</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    显式 conversationId 按会话粘性 TTL（当前 {qoderStickyTtlSecs}s）绑定账号（滚动续期）、
                    消息指纹 60 秒短窗；同账号 + 同种子派生同一上游 session_id，保住会话侧复用；
                    账号 busy 且有空闲候选时自动让位（并发优先）。默认关闭（轮换调度）
                  </span>
                </span>
              </label>
              {/* F-80-余 v2：竞速对冲阈值（0 = 关闭；有效范围 1s–8s 与后端对齐） */}
              <div className="flex items-center justify-between gap-3 px-1.5 pt-1">
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">竞速对冲阈值</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    流式/聚合首字节超过该时长即向第二账号发对冲请求，先出首字者胜；
                    0 = 关闭（有效范围 1s–8s）。适用于上游首字节缓慢（静默排队 /
                    prefill 等待）；10605 排队通知为即时信封、由同号退避处理
                  </span>
                </span>
                <span className="flex shrink-0 items-center gap-1.5">
                  <input
                    type="number"
                    min={0}
                    max={8000}
                    step={500}
                    value={qoderHedgeThresholdMs}
                    onChange={(e) =>
                      setQoderHedgeThresholdMs(Math.max(0, Math.min(8000, Number(e.target.value) || 0)))
                    }
                    className="w-24 rounded-lg border border-slate-200 bg-white px-2 py-1.5 text-right text-xs tabular-nums text-slate-700 focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                  <span className="text-[11px] text-slate-400">ms</span>
                </span>
              </div>
            </div>
          </div>
        </div>

        {/* 右列：模型目录（Qoder 源，统一目录聚合实时派生） */}
        <div className="col-span-6">
          <div className="card p-4">
            <div className="mb-3 flex items-center justify-between">
              <div className="flex items-center gap-2">
                <Layers size={16} className="text-brand-500" />
                <span className="text-sm font-medium">模型目录（Qoder）</span>
                <span className="text-xs text-slate-400">{models.length} 个模型</span>
              </div>
              <button className="btn-outline" onClick={() => void syncCatalog()} disabled={syncing}>
                <RefreshCw size={14} className={syncing ? 'animate-spin' : ''} />
                {syncing ? '同步中…' : '同步目录'}
              </button>
            </div>
            <p className="mb-3 text-xs text-slate-400">
              从 Qoder CN 网关 model/list 拉取并替换目录缓存（倍率/思考档位/图片模态以服务端为准）。
            </p>
            {/* 定时同步（app Settings）：应用内置调度器每日到点自动执行，无账号时静默跳过 */}
            <div className="mb-3 flex flex-wrap items-center gap-2 rounded-lg border border-slate-100 p-2.5 text-xs dark:border-zinc-800">
              <label className="flex cursor-pointer items-center gap-1.5">
                <input
                  type="checkbox"
                  checked={catSync.enabled}
                  onChange={(e) => setCatSync((f) => ({ ...f, enabled: e.target.checked }))}
                />
                每日定时同步
              </label>
              <input
                type="time"
                value={catSync.hhmm}
                onChange={(e) => setCatSync((f) => ({ ...f, hhmm: e.target.value || '05:50' }))}
                disabled={!catSync.enabled}
                className="input h-8 !w-28 text-xs"
              />
              <button className="btn-outline !px-2 !py-1" disabled={savingCatSync} onClick={() => void saveCatSync()}>
                {savingCatSync ? <Spinner /> : <Save size={13} />} 保存
              </button>
              <span className="text-slate-400 dark:text-zinc-500">
                {catSync.enabled
                  ? `应用运行期间每天 ${catSync.hhmm || '05:50'} 自动同步（无账号时静默跳过）`
                  : '已关闭定时同步，仅手动同步'}
              </span>
            </div>
            {models.length === 0 ? (
              <p className="py-4 text-center text-xs text-slate-400">
                暂无目录数据：点击「同步目录」拉取（需至少一个含凭证的 Qoder 账号）。
              </p>
            ) : (
              <div className="overflow-x-auto">
                <table className="w-full min-w-[640px] text-sm">
                  <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                    <tr>
                      <th className="px-3 py-2 text-left">模型 ID</th>
                      <th className="px-3 py-2 text-left">展示名</th>
                      <th className="px-3 py-2 text-left">厂商</th>
                      <th className="px-3 py-2 text-right">积分倍率</th>
                      <th className="px-3 py-2 text-left">思考档位</th>
                      <th className="px-3 py-2 text-right">上下文</th>
                      <th className="px-3 py-2 text-center">图片支持</th>
                    </tr>
                  </thead>
                  <tbody>
                    {models.map((m) => (
                      <tr key={m.id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                        <td className="px-3 py-2 font-mono text-xs">{m.id}</td>
                        <td className="px-3 py-2">{m.display || '—'}</td>
                        <td className="px-3 py-2 text-xs text-slate-500">{m.vendor || '—'}</td>
                        <td className="px-3 py-2 text-right tabular-nums text-amber-600 dark:text-amber-400">
                          {m.rate != null ? m.rate.toFixed(2) : '—'}
                        </td>
                        <td className="px-3 py-2 text-xs text-slate-500">
                          {m.efforts.length > 0 ? m.efforts.join(' / ') : '—'}
                        </td>
                        <td className="px-3 py-2 text-right tabular-nums text-xs text-slate-500">
                          {m.context_length != null ? `${Math.round(m.context_length / 1000)}k` : '—'}
                        </td>
                        <td className="px-3 py-2 text-center text-xs">
                          {m.supports_image === true ? (
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
            <p className="mt-3 flex items-center gap-1.5 text-xs text-slate-400 dark:text-zinc-500">
              <Coins size={13} className="text-amber-500" />
              倍率为 Qoder 通用 credits 消耗倍率；产品决策下线的模型（Auto / Cantus / Efficient /
              Performance / Sonus / Ultimate）已从目录与路由移除。
            </p>
            {/* F-80-余 v2 Global 区标记说明：当前仅接入 CN 区（国内版）上游 */}
            <div className="mt-2 rounded-lg border border-slate-200 bg-slate-50/80 px-3 py-2 text-[11px] leading-4 text-slate-500 dark:border-zinc-800 dark:bg-zinc-800/40 dark:text-zinc-400">
              <span className="font-medium text-slate-600 dark:text-zinc-300">地区说明：</span>
              当前仅接入 <span className="font-medium">CN 区（国内版）</span> 网关
              （gateway.qoder.com.cn）。Global 区（api3.qoder.sh，海外版）尚未接线：
              Global 专属模型仅目录可见、不可路由，请求将显式返回 404。
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
