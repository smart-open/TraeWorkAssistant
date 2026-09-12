import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw, Activity, Eraser, Save, Layers, Pencil, Download } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Badge, Modal, Spinner, StatCard } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import { withMinDelay } from '../lib/delay';
import type {
  ApiServiceStatus,
  PoolStatus,
  TraeModelMeta,
  UnifiedModel,
  UsageDayView,
  GroupView,
} from '../types';

/**
 * Trae · 资源调度（unified-api-gateway-design §6.1，Phase 3）
 * 页内仅保留本应用资源级内容（自上而下）：积分体系说明 / 池指标行 / 账号池选择 / 模型目录（Trae）。
 * 模型目录数据源为统一目录聚合（api.apiServer.unifiedModels 过滤 Trae 源，§3.2 四层兜底），
 * 行内「编辑」弹框维护 L1 人工覆盖层（trae_model_meta_set/clear），保存后聚合视图即时生效。
 * 网关级功能（启停 / 接口配置 / API Keys / 生态接入 / 用量统计）已全部迁至
 * 全局 API 管理弹窗（左侧栏 KeyRound 图标），页内不再出现网关级内容。
 */

/** 思考档位选项（编辑弹框多选，顺序固定） */
const EFFORT_OPTIONS = ['low', 'medium', 'high'] as const;

/** 表单数值解析：空串 → null（未设置）；非法数字返回 NaN 由调用方拦截 */
const numOrNull = (s: string) => {
  const t = s.trim();
  if (t === '') return null;
  const n = Number(t);
  return Number.isFinite(n) ? n : NaN;
};

export default function ApiService() {
  const accounts = useAppStore((s) => s.accounts);
  const refreshAccounts = useAppStore((s) => s.refreshAccounts);
  const toast = useAppStore((s) => s.pushToast);

  // ---- 池指标 / 账号池 ----
  const [status, setStatus] = useState<ApiServiceStatus | null>(null);
  const [poolStatus, setPoolStatus] = useState<PoolStatus[]>([]);
  const [enabledUids, setEnabledUids] = useState<Set<string>>(new Set());
  const [poolStrategy, setPoolStrategy] = useState('expire_first');
  const [poolGroups, setPoolGroups] = useState<Set<string>>(new Set());
  const [groups, setGroups] = useState<GroupView[]>([]);
  const [savingPool, setSavingPool] = useState(false);
  const [clearingCooldowns, setClearingCooldowns] = useState(false);
  const [refreshingPool, setRefreshingPool] = useState(false);
  // 当日活跃账号数据源（今日用量桶的账号维度计数）
  const [usage, setUsage] = useState<UsageDayView[]>([]);

  // ---- 模型目录（Trae 源） ----
  const [models, setModels] = useState<UnifiedModel[]>([]);
  const [loadingModels, setLoadingModels] = useState(false);
  const [syncingModels, setSyncingModels] = useState(false);
  // 编辑弹框（null = 关闭；表单留空 = 未设置，交由下层自动来源兜底）
  const [editing, setEditing] = useState<UnifiedModel | null>(null);
  const [fLabel, setFLabel] = useState('');
  const [fRate, setFRate] = useState('');
  const [fEfforts, setFEfforts] = useState<Set<string>>(new Set());
  const [fContext, setFContext] = useState('');
  const [fMaxTokens, setFMaxTokens] = useState('');
  const [fImage, setFImage] = useState<'' | 'true' | 'false'>('');
  const [savingMeta, setSavingMeta] = useState(false);
  const [clearingMeta, setClearingMeta] = useState(false);

  useEffect(() => {
    void refreshAccounts();
    void loadPool();
    void refreshStatus();
    void loadGroups();
    void loadTodayUsage();
    void loadModels();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refreshAccounts]);

  // 当日用量（query_recent 含今日；仅用于「活跃账号」指标）
  const loadTodayUsage = useCallback(async () => {
    try {
      setUsage(await api.apiServer.usageStats(1));
    } catch {
      /* 加载失败按无数据处理 */
    }
  }, []);

  // 统一模型目录：过滤 sources 含 Trae 池的条目（§6.1，聚合视图实时派生）
  const loadModels = useCallback(
    async (manual = false) => {
      setLoadingModels(true);
      try {
        const list = await withMinDelay(api.apiServer.unifiedModels(), 250);
        setModels(list.filter((m) => m.sources.some((s) => s.pool === 'trae')));
      } catch {
        // 初始化失败静默保留空列表；手动点击刷新失败需给出提示
        if (manual) toast('error', '加载模型目录失败，请重试');
      } finally {
        setLoadingModels(false);
      }
    },
    [toast],
  );

  // 同步官网模型（models_sync 沿用现有调用）；L1 人工覆盖层不受同步影响
  const syncModels = async () => {
    if (syncingModels) return;
    setSyncingModels(true);
    try {
      await withMinDelay(api.apiServer.modelsSync(), 500);
      toast('success', '官网模型已同步（人工维护值不受影响）');
      await loadModels();
    } catch (err) {
      toast('error', `同步官网模型失败：${String(err).slice(0, 120)}`);
    } finally {
      setSyncingModels(false);
    }
  };

  const refreshStatus = useCallback(async () => {
    try {
      const s = await api.apiServer.status();
      setStatus(s);
      if (s.running) {
        try {
          setPoolStatus(await api.apiServer.poolStatus());
        } catch {
          /* ignore */
        }
      } else {
        setPoolStatus([]);
      }
    } catch {
      /* ignore */
    }
  }, []);

  useEffect(() => {
    if (!status?.running) return;
    const id = setInterval(() => void refreshStatus(), 3000);
    return () => clearInterval(id);
  }, [status?.running, refreshStatus]);

  const loadPool = async (manual = false) => {
    setRefreshingPool(true);
    try {
      const pool = await withMinDelay(api.apiServer.poolList());
      setEnabledUids(new Set(pool.enabled_uids));
      setPoolStrategy(pool.strategy || 'expire_first');
      setPoolGroups(new Set(pool.group_ids ?? []));
    } catch {
      // 初始化加载失败静默保留空列表；手动点击刷新失败需给出提示
      if (manual) toast('error', '加载账号池失败，请重试');
    } finally {
      setRefreshingPool(false);
    }
  };

  // 分组列表（供账号池分组筛选使用）
  const loadGroups = async () => {
    try {
      setGroups(await api.groups.list());
    } catch {
      /* 保留空列表 */
    }
  };

  // 分组筛选实时预览（T10）：按当前勾选 + 所选分组即时计算将纳入池中的账号，
  // 让「点选分组」有立即可见的过滤反馈（实际入池在保存并重启 API 服务后生效）
  const groupUidSets = useMemo(
    () => groups.map((g) => ({ id: g.id, uids: new Set(g.uids ?? []) })),
    [groups],
  );
  const poolPreview = useMemo(() => {
    if (poolGroups.size === 0) return { inPool: enabledUids.size, excluded: 0 };
    let inPool = 0;
    let excluded = 0;
    for (const uid of enabledUids) {
      if (groupUidSets.some((g) => poolGroups.has(g.id) && g.uids.has(uid))) inPool += 1;
      else excluded += 1;
    }
    return { inPool, excluded };
  }, [enabledUids, poolGroups, groupUidSets]);
  // 判断账号在当前分组筛选下是否参与调度（账号列表标记用）
  const inPoolFilter = (uid: string) =>
    poolGroups.size === 0 || groupUidSets.some((g) => poolGroups.has(g.id) && g.uids.has(uid));

  const clearAllCooldowns = async () => {
    setClearingCooldowns(true);
    try {
      const cleared = await withMinDelay(api.accounts.cooldownClearAll());
      if (cleared > 0) {
        toast('success', `已清除 ${cleared} 个账号的冷却状态`);
        void refreshStatus();
      } else {
        toast('info', '当前无冷却中的账号');
      }
    } catch (err) {
      toast('error', `清除冷却失败：${String(err)}`);
    } finally {
      setClearingCooldowns(false);
    }
  };

  const toggleUid = (uid: string) => {
    setEnabledUids((prev) => {
      const next = new Set(prev);
      if (next.has(uid)) next.delete(uid);
      else next.add(uid);
      return next;
    });
  };

  const savePool = async () => {
    setSavingPool(true);
    try {
      await withMinDelay(api.apiServer.poolSet([...enabledUids], poolStrategy, [...poolGroups]));
      toast('success', '账号池已更新');
      if (status?.running) {
        toast('info', '需重启 API 服务以应用变更');
      }
    } catch (err) {
      toast('error', `保存账号池失败：${String(err)}`);
    } finally {
      setSavingPool(false);
    }
  };

  // ---- 模型元数据编辑（L1 覆盖层） ----

  // 打开弹框：回显 L1 人工值（metaGet 读取 data/trae_model_meta.json，跨会话可回显）；
  // 读取失败时全表单留空 = 交由下层兜底（绝不把聚合值当人工值回填，区分 L1 与 L2-L4，§6.1）。
  // editReqRef 令牌守卫：快速连点两个模型时仅让最后一次请求回填，防止慢响应覆盖新弹框。
  const editReqRef = useRef('');
  const openEdit = async (m: UnifiedModel) => {
    editReqRef.current = m.id;
    setEditing(m);
    const known = await api.apiServer.metaGet(m.id).catch(() => null);
    if (editReqRef.current !== m.id) return;
    setFLabel(known?.label ?? '');
    setFRate(known?.rate != null ? String(known.rate) : '');
    setFEfforts(new Set(known?.efforts ?? []));
    setFContext(known?.context_length != null ? String(known.context_length) : '');
    setFMaxTokens(known?.max_tokens != null ? String(known.max_tokens) : '');
    setFImage(
      known?.supports_image === true ? 'true' : known?.supports_image === false ? 'false' : '',
    );
  };

  const saveMeta = async () => {
    if (!editing) return;
    const rate = numOrNull(fRate);
    const context = numOrNull(fContext);
    const maxTokens = numOrNull(fMaxTokens);
    if (Number.isNaN(rate) || Number.isNaN(context) || Number.isNaN(maxTokens)) {
      toast('error', '数值字段格式不正确，请检查倍率 / 上下文 / max_tokens');
      return;
    }
    const meta: TraeModelMeta = {
      label: fLabel.trim() || null,
      rate,
      // 未勾选任何档位 = 未设置（交由下层推断），不落空数组
      efforts: fEfforts.size > 0 ? EFFORT_OPTIONS.filter((e) => fEfforts.has(e)) : null,
      context_length: context,
      max_tokens: maxTokens,
      supports_image: fImage === '' ? null : fImage === 'true',
    };
    setSavingMeta(true);
    try {
      await withMinDelay(api.apiServer.metaSet(editing.id, meta), 300);
      toast('success', `已保存 ${editing.id} 人工元数据（L1 覆盖，聚合即时生效）`);
      setEditing(null);
      await loadModels();
    } catch (err) {
      toast('error', `保存失败：${String(err)}`);
    } finally {
      setSavingMeta(false);
    }
  };

  // 清除人工值 → 恢复自动来源链（L2 官网同步 > L3 文档参考 > L4 名称推断）
  const clearMeta = async () => {
    if (!editing) return;
    setClearingMeta(true);
    try {
      await withMinDelay(api.apiServer.metaClear(editing.id), 300);
      toast('success', `已清除 ${editing.id} 人工元数据，恢复自动来源`);
      setEditing(null);
      await loadModels();
    } catch (err) {
      toast('error', `清除失败：${String(err)}`);
    } finally {
      setClearingMeta(false);
    }
  };

  const running = status?.running ?? false;
  const poolCount = enabledUids.size;

  // 池指标（§6.1）：可用 = 健康且未冷却（仅运行中可得实时值）；活跃 = 当日被调度使用；池内 = enabled_uids
  const todayKey = new Date().toLocaleDateString('sv-SE');
  const availableCount = running
    ? poolStatus.filter((p) => !p.cooling && !p.disabled).length
    : null;
  const activeToday =
    usage.find((d) => d.date === todayKey)?.accounts.filter((a) => a.requests > 0).length ?? 0;

  // 账号池仅展示/可选有通用积分的账号（本服务消耗通用积分，零积分账号无法服务请求）
  const poolAccounts = useMemo(
    () => accounts.filter((a) => (a.general_credits ?? 0) > 0),
    [accounts],
  );
  const totalGeneral = useMemo(
    () => accounts.reduce((s, a) => s + (a.general_credits ?? 0), 0),
    [accounts],
  );

  // 输入框通用样式（沿用页面表单惯例）
  const inputCls =
    'h-7 w-full rounded-md border border-slate-200 bg-white px-2 text-xs text-slate-700 outline-none focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200';

  return (
    <div>
      <PageHeader
        title="Trae · 资源调度"
        desc="服务资源池管理 · 账号调度与模型目录 · 资源提供给API网关使用"
        actions={
          <button className="btn-outline" onClick={() => void refreshStatus()}>
            <RefreshCw size={15} /> 刷新
          </button>
        }
      />

      {/* 积分体系说明（资源级） */}
      <div className="rounded-xl border border-amber-300/70 bg-amber-50/80 px-3.5 py-2.5 dark:border-amber-700/40 dark:bg-amber-900/10">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-semibold text-amber-800 dark:text-amber-200">积分体系说明</span>
          <span className="rounded bg-amber-200 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-700/50 dark:text-amber-100">本服务消耗通用积分</span>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-slate-500 dark:text-zinc-400">
          <span>
            Trae 通用积分（product_id 208）：签到 / 活动获取，各模型按倍率消耗；账号池轮换消耗各账号积分，冷却与禁用规则同现有实现。上游接口{' '}
            <code className="font-mono text-amber-700 dark:text-amber-300">llm_utils_chat</code>，明文 JSON。
          </span>
          <span className="ml-auto">
            当前全部账号通用积分总余额：
            <span className="font-bold tabular-nums text-amber-700 dark:text-amber-300">
              {totalGeneral.toLocaleString('zh-CN', { maximumFractionDigits: 0 })}
            </span>
          </span>
        </div>
      </div>

      {/* 池指标行 */}
      <div className="mt-5 grid grid-cols-3 gap-3">
        <StatCard
          label="可用账号数"
          value={availableCount ?? '—'}
          tone="green"
          hint={running ? '健康且未冷却，可被调度选中' : 'API 服务未运行，无实时池状态'}
        />
        <StatCard
          label="活跃账号"
          value={activeToday}
          tone="blue"
          hint="当日被调度使用过的账号"
        />
        <StatCard
          label="池内账号"
          value={poolCount}
          tone="violet"
          hint="已选入轮转池的账号数（enabled_uids）"
        />
      </div>

      <div className="mt-5 grid grid-cols-1 items-stretch gap-5 lg:grid-cols-2">
        {/* 账号池选择（资源级，现有功能原样迁移） */}
        <div className="card flex flex-col p-5">
          <div className="mb-4 flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Activity size={18} className="text-brand-500" />
              <h2 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">
                账号池选择
              </h2>
            </div>
            <div className="flex items-center gap-2">
              {running && (
                <button
                  className="btn-ghost flex items-center gap-1 text-xs"
                  onClick={() => void clearAllCooldowns()}
                  disabled={clearingCooldowns}
                  title="清除所有账号的冷却状态"
                >
                  <Eraser size={13} className={clearingCooldowns ? 'animate-pulse' : ''} />
                  {clearingCooldowns ? '清除中…' : '清除冷却'}
                </button>
              )}
              <button
                className="btn-ghost flex items-center gap-1 text-xs"
                onClick={() => void loadPool(true)}
                disabled={refreshingPool}
              >
                <RefreshCw size={13} className={refreshingPool ? 'animate-spin' : ''} />
                {refreshingPool ? '刷新中…' : '刷新'}
              </button>
            </div>
          </div>

          {poolAccounts.length === 0 ? (
            <p className="flex-1 py-8 text-center text-sm text-slate-400">
              暂无含通用积分的账号，请先签到或刷新积分后重试
            </p>
          ) : (
            <>
              {/* 调度策略 + 分组筛选（T10，保存后需重启 API 服务生效） */}
              <div className="mb-3 space-y-2 rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50">
                <div className="flex items-center gap-2">
                  <label className="shrink-0 text-xs text-slate-500 dark:text-zinc-400">调度策略</label>
                  <select
                    className="h-7 flex-1 rounded-md border border-slate-200 bg-white px-2 text-xs text-slate-700 outline-none focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                    value={poolStrategy}
                    onChange={(e) => setPoolStrategy(e.target.value)}
                  >
                    <option value="expire_first">积分先过期优先（默认）</option>
                    <option value="credit_first">剩余积分多优先</option>
                    <option value="random">随机</option>
                  </select>
                </div>
                {groups.length > 0 && (
                  <div className="flex items-start gap-2">
                    <label className="shrink-0 pt-1 text-xs text-slate-500 dark:text-zinc-400">
                      分组筛选
                    </label>
                    <div className="flex flex-1 flex-wrap gap-1">
                      {groups.map((g) => {
                        const active = poolGroups.has(g.id);
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
                              setPoolGroups((prev) => {
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
                      {poolGroups.size > 0 && (
                        <span className="pt-0.5 text-xs text-slate-400">
                          将纳入 {poolPreview.inPool} 个账号
                          {poolPreview.excluded > 0 &&
                            `，${poolPreview.excluded} 个分组外账号不参与调度`}
                        </span>
                      )}
                    </div>
                  </div>
                )}
                <p className="text-xs text-slate-400 dark:text-zinc-500">
                  分组筛选与调度策略作用于网关取号范围，保存后需重启 API 服务生效；不选分组 = 全部参与
                </p>
              </div>

              <div className="mb-3 flex items-center gap-2">
                <button
                  className="text-xs text-brand-600 hover:underline dark:text-brand-400"
                  onClick={() =>
                    setEnabledUids(new Set(poolAccounts.map((a) => a.user_id)))
                  }
                >
                  全选
                </button>
                <span className="text-slate-300">|</span>
                <button
                  className="text-xs text-brand-600 hover:underline dark:text-brand-400"
                  onClick={() => setEnabledUids(new Set())}
                >
                  清空
                </button>
                <span className="ml-auto text-xs text-slate-400">
                  已选 {enabledUids.size} / {poolAccounts.length}
                </span>
              </div>

              <div className="flex-1 space-y-1">
                {poolAccounts.map((a) => {
                  const checked = enabledUids.has(a.user_id);
                  const poolItem = poolStatus.find((p) => p.uid === a.user_id);
                  const filteredOut = !inPoolFilter(a.user_id);
                  return (
                    <label
                      key={a.user_id}
                      className={`flex cursor-pointer items-center gap-3 rounded-lg px-3 py-2 transition hover:bg-slate-50 dark:hover:bg-zinc-800/50 ${
                        filteredOut ? 'opacity-50' : ''
                      }`}
                    >
                      <input
                        type="checkbox"
                        className="h-4 w-4 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                        checked={checked}
                        onChange={() => toggleUid(a.user_id)}
                      />
                      <div className="min-w-0 flex-1">
                        <div className="truncate text-sm font-medium text-slate-700 dark:text-zinc-200">
                          {a.name}
                        </div>
                        <div className="truncate text-xs text-slate-400">
                          {a.user_id}
                        </div>
                      </div>
                      <div className="flex shrink-0 items-center gap-2">
                        {filteredOut && <Badge tone="slate">分组外</Badge>}
                        {(a.general_credits ?? 0) > 0 && (
                          <span className="text-xs tabular-nums text-slate-500 dark:text-zinc-400">
                            {(a.general_credits ?? 0).toFixed(0)} 通用积分
                          </span>
                        )}
                        {poolItem?.cooling && (
                          <Badge tone="amber">冷却中</Badge>
                        )}
                        {poolItem?.disabled && (
                          <Badge tone="red">已禁用</Badge>
                        )}
                        {running && poolItem && !poolItem.cooling && !poolItem.disabled && (
                          <Badge tone="green">就绪</Badge>
                        )}
                      </div>
                    </label>
                  );
                })}
              </div>

              <button
                className="btn-outline mt-3 flex w-full items-center justify-center gap-2"
                onClick={() => void savePool()}
                disabled={savingPool}
              >
                <Save size={15} />
                {savingPool ? '保存中…' : '保存账号池'}
              </button>
            </>
          )}
        </div>

        {/* 模型目录（Trae）：统一目录聚合 Trae 源 + L1 人工元数据编辑 */}
        <div className="card flex flex-col p-5">
          <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <Layers size={18} className="text-brand-500" />
              <h2 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">
                模型目录（Trae）
              </h2>
              <span className="text-xs text-slate-400">{models.length} 个模型</span>
            </div>
            <div className="flex items-center gap-2">
              <button
                className="btn-outline flex items-center gap-1 text-xs"
                onClick={() => void syncModels()}
                disabled={syncingModels}
              >
                <Download size={13} className={syncingModels ? 'animate-spin' : ''} />
                {syncingModels ? '同步中…' : '同步官网模型'}
              </button>
              <button
                className="btn-ghost flex items-center gap-1 text-xs"
                onClick={() => void loadModels(true)}
                disabled={loadingModels}
              >
                <RefreshCw size={13} className={loadingModels ? 'animate-spin' : ''} />
                {loadingModels ? '刷新中…' : '刷新'}
              </button>
            </div>
          </div>

          <p className="mb-3 text-xs text-slate-400 dark:text-zinc-500">
            四层来源聚合：人工维护 &gt; 官网同步 &gt; 内置参考 &gt; 名称推断；未确定字段显示 —，名称带{' '}
            <span className="font-bold text-amber-500">*</span> 表示已人工维护（官网同步不覆盖）。
          </p>

          {models.length === 0 ? (
            <p className="flex-1 py-8 text-center text-sm text-slate-400">
              暂无模型数据 — 点击「同步官网模型」或「刷新」拉取统一目录
            </p>
          ) : (
            <div className="flex-1 overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400">
                    <th className="pb-2 pr-3 font-medium">模型 ID</th>
                    <th className="pb-2 pr-3 font-medium">展示名</th>
                    <th className="pb-2 pr-3 text-right font-medium">积分倍率</th>
                    <th className="pb-2 pr-3 font-medium">思考档位</th>
                    <th className="pb-2 pr-3 text-right font-medium">上下文</th>
                    <th className="pb-2 pr-3 text-center font-medium">图片</th>
                    <th className="pb-2 text-right font-medium">操作</th>
                  </tr>
                </thead>
                <tbody>
                  {models.map((m) => (
                    <tr
                      key={m.id}
                      className="border-b border-slate-100 last:border-0 dark:border-zinc-800"
                    >
                      <td className="py-2 pr-3 font-mono text-xs font-medium text-slate-700 dark:text-zinc-200">
                        {m.id}
                      </td>
                      <td className="py-2 pr-3 text-slate-600 dark:text-zinc-300">
                        {m.display || '—'}
                        {m.manual && (
                          <span
                            className="ml-0.5 font-bold text-amber-500"
                            title="已人工维护（L1 覆盖层）"
                          >
                            *
                          </span>
                        )}
                      </td>
                      <td className="py-2 pr-3 text-right tabular-nums text-amber-600 dark:text-amber-400">
                        {m.rate != null ? m.rate.toFixed(2) : '—'}
                      </td>
                      <td className="py-2 pr-3 text-xs text-slate-500 dark:text-zinc-400">
                        {m.efforts.length > 0 ? m.efforts.join(' / ') : '—'}
                      </td>
                      <td className="py-2 pr-3 text-right tabular-nums text-xs text-slate-500 dark:text-zinc-400">
                        {m.context_length != null ? `${Math.round(m.context_length / 1000)}k` : '—'}
                      </td>
                      <td className="py-2 pr-3 text-center text-xs">
                        {m.supports_image === true ? (
                          <span className="font-semibold text-emerald-600 dark:text-emerald-400">✓</span>
                        ) : m.supports_image === false ? (
                          <span className="text-slate-400 dark:text-zinc-500">✗</span>
                        ) : (
                          <span className="text-slate-300 dark:text-zinc-600">—</span>
                        )}
                      </td>
                      <td className="py-2 text-right">
                        <button
                          className="btn-ghost inline-flex items-center gap-1 text-xs"
                          onClick={() => void openEdit(m)}
                        >
                          <Pencil size={12} />
                          编辑
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </div>

      {/* 模型元数据编辑弹框（L1 覆盖层，§6.1）：字段全部可留空 = 交由下层自动来源兜底 */}
      <Modal
        open={editing !== null}
        onClose={() => setEditing(null)}
        title={`编辑模型元数据 · ${editing?.id ?? ''}`}
        footer={
          <>
            {editing?.manual && (
              <button
                className="btn-outline mr-auto flex items-center gap-1"
                onClick={() => void clearMeta()}
                disabled={clearingMeta || savingMeta}
                title="清除 L1 人工覆盖，恢复自动来源链（官网同步 > 内置参考 > 名称推断）"
              >
                <Eraser size={14} />
                {clearingMeta ? '清除中…' : '清除人工值'}
              </button>
            )}
            <button className="btn-ghost" onClick={() => setEditing(null)} disabled={savingMeta}>
              取消
            </button>
            <button
              className="btn-primary flex items-center gap-1"
              onClick={() => void saveMeta()}
              disabled={savingMeta}
            >
              {savingMeta ? <Spinner className="text-white" /> : <Save size={15} />}
              {savingMeta ? '保存中…' : '保存'}
            </button>
          </>
        }
      >
        <p className="mb-3 text-xs text-slate-400 dark:text-zinc-500">
          人工维护为最高优先级（L1），官网同步不覆盖；<b>留空 = 未设置</b>，交由下层自动来源兜底。
        </p>
        <div className="grid grid-cols-2 gap-3">
          <div>
            <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">展示名</label>
            <input
              className={inputCls}
              value={fLabel}
              onChange={(e) => setFLabel(e.target.value)}
              placeholder="留空 = 自动来源"
            />
          </div>
          <div>
            <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">积分倍率</label>
            <input
              className={inputCls}
              type="number"
              step="0.01"
              min="0"
              value={fRate}
              onChange={(e) => setFRate(e.target.value)}
              placeholder="留空 = 自动来源"
            />
          </div>
          <div className="col-span-2">
            <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">
              思考档位（可多选；全不选 = 未设置）
            </label>
            <div className="flex items-center gap-4">
              {EFFORT_OPTIONS.map((e) => (
                <label key={e} className="flex cursor-pointer items-center gap-1.5 text-xs text-slate-600 dark:text-zinc-300">
                  <input
                    type="checkbox"
                    className="h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                    checked={fEfforts.has(e)}
                    onChange={() =>
                      setFEfforts((prev) => {
                        const next = new Set(prev);
                        if (next.has(e)) next.delete(e);
                        else next.add(e);
                        return next;
                      })
                    }
                  />
                  {e}
                </label>
              ))}
            </div>
          </div>
          <div>
            <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">上下文长度</label>
            <input
              className={inputCls}
              type="number"
              step="1"
              min="0"
              value={fContext}
              onChange={(e) => setFContext(e.target.value)}
              placeholder="如 1000000"
            />
          </div>
          <div>
            <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">max_tokens</label>
            <input
              className={inputCls}
              type="number"
              step="1"
              min="0"
              value={fMaxTokens}
              onChange={(e) => setFMaxTokens(e.target.value)}
              placeholder="如 96000"
            />
          </div>
          <div>
            <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">图片支持</label>
            <select
              className={inputCls}
              value={fImage}
              onChange={(e) => setFImage(e.target.value as '' | 'true' | 'false')}
            >
              <option value="">未设置（自动来源）</option>
              <option value="true">支持</option>
              <option value="false">不支持</option>
            </select>
          </div>
        </div>
      </Modal>
    </div>
  );
}
