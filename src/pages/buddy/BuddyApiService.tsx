import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw, Save, Coins, ToggleLeft, Activity, Gauge } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner, StatCard } from '../../components/ui';
import { PoolHealthBadges } from '../../components/api/PoolHealthBadges';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type {
  ApiPoolFile,
  PoolStatus,
  TagView,
  UnifiedModel,
  UsageDayView,
  WbModelInfo,
  WorkBuddyAccountView,
  GroupView,
} from '../../types';

/**
 * Buddy · 资源调度（unified-api-gateway-design §6.2，Phase 3）
 * 页内仅保留 Buddy 资源级内容：积分体系说明 / 池指标行 /
 * 左列（账号池选择 + 资源开关与调度参数合并面板，共用右上角「保存」一次保存全部）/
 * 右列（模型目录（Buddy）），同行左右两列各占 1/2。
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

/** F-76②/③/F-77 数值参数（保存时写回 api_pool.json，服务运行中热生效；per-pool 拆分后均为 Buddy 池专属） */
const WB_PARAM_FIELDS: {
  key: 'wbAccountConcurrencyLimit' | 'wbPoolStickyTtlSecs' | 'wbStickyTtlSecs' | 'wbHedgeThresholdMs';
  label: string;
  desc: string;
  min: number;
  max: number;
  step: number;
  unit: string;
}[] = [
  {
    key: 'wbAccountConcurrencyLimit',
    label: '账号并发上限',
    desc: '单账号在途请求数达到上限即让位其他账号（全部 busy 时取负载最小者）；0 = 不限',
    min: 0,
    max: 32,
    step: 1,
    unit: '并发',
  },
  {
    key: 'wbPoolStickyTtlSecs',
    label: '池粘性 TTL',
    desc: 'TTL 内同会话落同一账号（上游 KV cache 复用）；0 = 关闭',
    min: 0,
    max: 3600,
    step: 30,
    unit: '秒',
  },
  {
    key: 'wbStickyTtlSecs',
    label: '会话粘性 TTL',
    desc: '显式 conversationId 绑定账号的有效期（最小 60 秒，无独立关闭开关）',
    min: 60,
    max: 86_400,
    step: 60,
    unit: '秒',
  },
  {
    key: 'wbHedgeThresholdMs',
    label: '竞速对冲阈值',
    desc: '流式首字节超过该时长即向第二账号发对冲请求，先出首字者胜；0 = 关闭（有效范围 1s–8s，与后端对齐）',
    min: 0,
    max: 8_000,
    step: 500,
    unit: 'ms',
  },
];

/** 数值参数默认值（与后端 serde default 对齐：Buddy 并发 1 / Buddy 池粘性 300s / 会话粘性 1800s / 对冲 8s） */
const WB_PARAM_DEFAULTS = {
  wbAccountConcurrencyLimit: 1,
  wbPoolStickyTtlSecs: 300,
  wbStickyTtlSecs: 1800,
  wbHedgeThresholdMs: 8_000,
};

export default function BuddyApiService() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [wbFlags, setWbFlags] = useState({
    wbEnabled: false,
    wbDefaultThinking: false,
    wbToolExec: false,
    wbBgDowngrade: false,
    wbLongctxDowngrade: false,
    // buddy-ai（国际版独立池，阶段 2）：显式开启制，默认关
    buddyAiEnabled: false,
    buddyAiStickyEnabled: false,
  });
  const [wbParams, setWbParams] = useState({ ...WB_PARAM_DEFAULTS });
  // Buddy 池入池白名单（wb-<hash> 账号 id = a.id，与网关池键/PoolStatus.uid 同域；
  // 注意不是 a.uid——那是真实账号 uuid）：null = 未自定义（后端按「全部含凭证账号」自动入池）
  const [wbUids, setWbUids] = useState<string[] | null>(null);
  // Buddy 池分组筛选（对齐 Trae T10）：所选分组 id 集合，空 = 不限（全部参与）
  const [wbPoolGroups, setWbPoolGroups] = useState<Set<string>>(new Set());
  const [wbGroups, setWbGroups] = useState<GroupView[]>([]);
  // 共享标签筛选（P2 需求④）：与分组筛选取交集（D5），空 = 不限标签
  const [wbPoolTags, setWbPoolTags] = useState<Set<string>>(new Set());
  const [tagDefs, setTagDefs] = useState<TagView[]>([]);
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [catalog, setCatalog] = useState<WbModelInfo[]>([]);
  // 厂商列数据源（§6.2）：WbModelInfo 无 vendor 字段，经统一目录聚合按模型 id
  // 关联（vendor = 自定义模型用户填写值优先，否则按模型名系列推断）；id 缺失/
  // 失败 → 显示 —，不影响主列表
  const [vendorMap, setVendorMap] = useState<Map<string, string>>(new Map());
  const [syncing, setSyncing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  // 当日活跃账号数据源（今日 WB 用量桶的账号维度计数）
  const [usage, setUsage] = useState<UsageDayView[]>([]);
  // WB 池实时状态（F-77⑤ 可观测：per-account inflight 在途计数）
  const [wbPool, setWbPool] = useState<PoolStatus[]>([]);
  // 伪流式灰度（issue #213）：仅内存热切换、重启复位默认关；
  // null = 服务未运行/读取失败（开关置灰、输入禁用），与指纹清洗开关同口径
  const [pseudoEnabled, setPseudoEnabled] = useState<boolean | null>(null);
  // 池级换号重试预算（0 = 旧行为：重试机会 = 池内账号数）
  const [pseudoBudget, setPseudoBudget] = useState<number | null>(null);
  // 伪流式等待期 keep-alive 间隔秒数（全局配置，两档共用；0 = 关闭，0–60）
  const [pseudoKeepalive, setPseudoKeepalive] = useState<number | null>(null);
  // 伪流式数值参数最近落库值（审查修复：onBlur 与「最近落库值」比较而非 state——
  // 清空输入（Number("")=0）或改后回退原值时 state 已等于 clamped，按 state 判断会
  // 漏保存导致前后端分叉；保存成功才记入 ref，与 BuddyAiApiService savedParamsRef 同范式）
  const savedPseudoRef = useRef<{ budget: number | null; keepalive: number | null }>({
    budget: null,
    keepalive: null,
  });
  // 审查 F1：账号列表加载失败态（null = 正常）——区分「成功但空」（空集引导）
  // 与「请求失败」（错误态），避免把读取失败误报成「暂无账号」
  const [accountsError, setAccountsError] = useState<string | null>(null);
  // refresh 序号守卫（2026-11-21 审查修复，对齐 BuddyAiApiService refreshSeqRef 范式）：
  // 挂载 effect、手动「刷新」与 saveFlags 成功后的池快照重取三方并发时，两个
  // Promise.all 乱序 resolve，旧快照（含旧 wbFlags/wbUids/pool）覆盖新快照——
  // 保存后的回显被回滚成保存前状态
  const refreshSeqRef = useRef(0);

  const refresh = useCallback(async () => {
    const seq = ++refreshSeqRef.current;
    setRefreshing(true);
    try {
      // 审查 F1：账号列表失败时返回 null（区别于「成功但空数组」）
      let accsErr: unknown = null;
      // 审查修复（2026-11-21）：池配置加载失败补可见反馈（对齐 BuddyAi 页 FM-1）——
      // 原静默置 null，用户无从分辨「未启用」与「加载失败」
      let poolErr: unknown = null;
      const [pf, accs, cat, wbp, wbg, unified, tags] = await Promise.all([
        api.apiServer.poolList().catch((e) => {
          poolErr = e;
          return null;
        }),
        api.workbuddy
          .accountsList()
          .catch((e) => {
            accsErr = e;
            return null;
          }),
        api.apiServer.wbCatalogList().catch(() => [] as WbModelInfo[]),
        api.apiServer.wbPoolStatus().catch(() => [] as PoolStatus[]),
        api.workbuddy.groups.list().catch(() => [] as GroupView[]),
        api.apiServer.unifiedModels().catch(() => [] as UnifiedModel[]),
        api.tags.list().catch(() => [] as TagView[]),
      ]);
      // 过期响应丢弃：新请求已发出，旧快照不得覆盖新回显（错误 toast 同口径）
      if (seq !== refreshSeqRef.current) return;
      setPool(pf);
      setTagDefs(tags);
      if (accs === null) {
        // 审查 F1：读取失败 → 错误态 + toast，不落空数组（不触发空集引导）
        setAccountsError(String(accsErr));
        pushToast('error', `账号列表加载失败：${String(accsErr)}`);
      } else {
        setAccounts(accs);
        setAccountsError(null);
      }
      setCatalog(cat);
      setWbPool(wbp);
      setWbGroups(wbg);
      if (pf === null) {
        pushToast('error', `池配置加载失败：${String(poolErr)}`);
      }
      setVendorMap(new Map(unified.filter((m) => m.vendor).map((m) => [m.id, m.vendor])));
      if (pf) {
        setWbFlags({
          wbEnabled: pf.wb_enabled ?? false,
          wbDefaultThinking: pf.wb_default_thinking ?? false,
          wbToolExec: pf.wb_tool_exec ?? false,
          wbBgDowngrade: pf.wb_bg_downgrade ?? false,
          wbLongctxDowngrade: pf.wb_longctx_downgrade ?? false,
          buddyAiEnabled: pf.buddy_ai_enabled ?? false,
          buddyAiStickyEnabled: pf.buddy_ai_sticky_enabled ?? false,
        });
        setWbParams({
          wbHedgeThresholdMs: pf.wb_hedge_threshold_ms ?? WB_PARAM_DEFAULTS.wbHedgeThresholdMs,
          wbAccountConcurrencyLimit:
            pf.wb_account_concurrency_limit ?? WB_PARAM_DEFAULTS.wbAccountConcurrencyLimit,
          wbPoolStickyTtlSecs:
            pf.wb_pool_sticky_ttl_secs ?? WB_PARAM_DEFAULTS.wbPoolStickyTtlSecs,
          wbStickyTtlSecs: Math.max(
            60,
            pf.wb_sticky_ttl_secs ?? WB_PARAM_DEFAULTS.wbStickyTtlSecs,
          ),
        });
        // 空数组 = fail-open（全部自动入池）→ 视为未自定义，显示为全选
        setWbUids(pf.wb_enabled_uids?.length ? pf.wb_enabled_uids : null);
        // 分组筛选：空 = 不限（fail-open，全部参与）
        setWbPoolGroups(new Set(pf.wb_group_ids ?? []));
        // 标签筛选：空 = 不限（与分组合取交集，D5）
        setWbPoolTags(new Set(pf.wb_tag_ids ?? []));
      }
    } catch (err) {
      pushToast('error', `读取资源状态失败：${String(err)}`);
    } finally {
      if (seq === refreshSeqRef.current) setRefreshing(false);
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

  // 伪流式灰度回显（issue #213）：失败置 null（开关置灰按默认关展示），启动后点「刷新」可恢复。
  // 并发守卫（2026-11-21 审查修复，对齐 BuddyAi 侧 refreshSeqRef 范式）：挂载与
  // savePseudoParam 失败回调可能并发触发，慢的旧响应不得覆盖新回显
  const pseudoSeqRef = useRef(0);
  const loadPseudo = useCallback(async () => {
    const seq = ++pseudoSeqRef.current;
    try {
      const [sw, budgets, ka] = await Promise.all([
        api.apiServer.pseudoStreamSwitchesGet(),
        api.apiServer.retryBudgetsGet(),
        api.apiServer.pseudoKeepaliveGet(),
      ]);
      // 过期响应丢弃：新请求已发出，旧快照不得覆盖新回显
      if (seq !== pseudoSeqRef.current) return;
      setPseudoEnabled(sw.wb);
      setPseudoBudget(budgets.wb);
      setPseudoKeepalive(ka);
      savedPseudoRef.current = { budget: budgets.wb, keepalive: ka };
    } catch {
      // 失败路径同口径：过期请求的失败不得清空新回显
      if (seq !== pseudoSeqRef.current) return;
      setPseudoEnabled(null);
      setPseudoBudget(null);
      setPseudoKeepalive(null);
      savedPseudoRef.current = { budget: null, keepalive: null };
    }
  }, []);

  useEffect(() => {
    void loadPseudo();
  }, [loadPseudo]);

  /** 伪流式开关热切换（issue #213）：乐观更新 + 失败回滚，即时生效无保存按钮 */
  const togglePseudo = async (enabled: boolean) => {
    const prev = pseudoEnabled;
    setPseudoEnabled(enabled);
    try {
      await api.apiServer.pseudoStreamSwitchSet('wb', enabled);
      pushToast(
        'success',
        `已${enabled ? '开启' : '关闭'}WB 池伪流式，下一条请求即生效`,
      );
    } catch (err) {
      // 条件回滚（审查修复：快速连点+失败时陈旧 prev 会覆盖新状态——仅当 state
      // 仍等于本次乐观值才回滚，其后已成功的切换不被乱序旧失败覆盖）
      setPseudoEnabled((cur) => (cur === enabled ? prev : cur));
      pushToast('error', `切换伪流式开关失败：${String(err)}`);
    }
  };

  /** 伪流式数值参数（重试预算 / keep-alive 秒）保存：后端 clamp 归一，以热切换即生效 */
  const savePseudoParam = async (kind: 'budget' | 'keepalive', value: number) => {
    try {
      if (kind === 'budget') {
        await api.apiServer.retryBudgetSet('wb', value);
        savedPseudoRef.current.budget = value;
        pushToast('success', `已保存 WB 池重试预算（${value}，下一条请求即生效）`);
      } else {
        await api.apiServer.pseudoKeepaliveSet(value);
        savedPseudoRef.current.keepalive = value;
        pushToast('success', `已保存伪流式 keep-alive 间隔（${value} 秒，下一条请求即生效）`);
      }
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
      void loadPseudo();
    }
  };

  // 保存资源开关/调度参数/账号池白名单：uids/strategy/groups 原样回传（本页不改 Trae 池配置）；
  // WB 白名单随本次保存提交：未自定义传 null（后端保留原值，fail-open 语义不变，
  // 新增账号可持续自动入池；显式全量名单会冻结 fail-open）；清空传 []（后端同样 fail-open）；
  // F-76②/③/F-77 数值参数随开关一起保存（后端热应用，运行中即时生效）
  const saveFlags = async () => {
    // 池配置未加载时禁止保存（对齐 Qoder 页同款守卫）：uids/strategy/groups 原样
    // 回传依赖 pool 快照，pool=null 时 `pool?.enabled_uids ?? []` 会把 Trae 池
    // 白名单清空（[] 在调度层为 fail-open 全量入池）
    if (!pool) {
      pushToast('error', '池配置未加载，无法保存（请先刷新重试）');
      return;
    }
    setSaving(true);
    try {
      await withMinDelay(
        api.apiServer.poolSet(pool?.enabled_uids ?? [], pool?.strategy, pool?.group_ids, {
          ...wbFlags,
          wbUids,
          wbGroupIds: [...wbPoolGroups],
          wbTagIds: [...wbPoolTags],
          wbHedgeThresholdMs: wbParams.wbHedgeThresholdMs,
          wbAccountConcurrencyLimit: wbParams.wbAccountConcurrencyLimit,
          wbPoolStickyTtlSecs: wbParams.wbPoolStickyTtlSecs,
          wbStickyTtlSecs: wbParams.wbStickyTtlSecs,
        }),
        600,
      );
      pushToast('success', '账号池选择、资源开关与调度参数已保存（服务运行中即时生效）');
      // 保存后重新拉取池快照（审查残留项修复）：本页保存原样回传加载时的
      // enabled_uids/strategy/groups，页面停留期间若他端（全局 API 管理弹窗等）
      // 改动了池配置，旧快照回传会覆盖回去——保存成功后立即同步最新快照收窄窗口
      try {
        const seq = ++refreshSeqRef.current;
        const pf = await api.apiServer.poolList();
        // 快照重取纳入 refresh 序号守卫：与并发 refresh 乱序 resolve 时旧值不得回滚新值
        if (seq === refreshSeqRef.current) setPool(pf);
      } catch {
        /* 快照同步失败不阻塞保存结果，下次刷新自愈 */
      }
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
      // 新增模型需重取统一目录关联 vendor（否则新行厂商列恒 —）
      const unified = await api.apiServer.unifiedModels();
      setVendorMap(new Map(unified.filter((m) => m.vendor).map((m) => [m.id, m.vendor])));
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

  // 分组筛选实时预览（对齐 Trae T10）：GroupView.uids 值域 = a.id（wb-<hash>），与白名单同域；
  // 后端在池装配层按 wb_group_ids 过滤账号后再应用白名单交集
  const wbGroupUidSets = useMemo(
    () => wbGroups.map((g) => ({ id: g.id, uids: new Set(g.uids ?? []) })),
    [wbGroups],
  );
  // P2 补齐：标签筛选与分组合取交集（D5），预览与灰显同后端装配语义一致
  const tagUidSets = useMemo(
    () => tagDefs.map((t) => ({ id: t.id, uids: new Set(t.uids ?? []) })),
    [tagDefs],
  );
  const inWbPoolFilter = (id: string) =>
    (wbPoolGroups.size === 0 ||
      wbGroupUidSets.some((g) => wbPoolGroups.has(g.id) && g.uids.has(id))) &&
    (wbPoolTags.size === 0 || tagUidSets.some((t) => wbPoolTags.has(t.id) && t.uids.has(id)));
  const wbPoolPreview = (() => {
    // 生效池 = (分组 ∩ 标签筛选) ∩ 白名单勾选（与后端 apply_pool_snapshot 装配语义
    // 一致，对齐 Qoder 页口径）；excluded 仅统计筛选外账号（未勾选由 checkbox
    // 状态表达，不计入 excluded）
    let inPool = 0;
    let excluded = 0;
    for (const a of credAccounts) {
      if (!inWbPoolFilter(a.id)) excluded += 1;
      else if (wbSelected.includes(a.id)) inPool += 1;
    }
    return { inPool, excluded };
  })();

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
          value={wbPoolPreview.inPool}
          tone="violet"
          hint="勾选并符合分组筛选的账号数（清空勾选 = 全部含凭证账号自动入池）"
        />
      </div>

      {/* 左列：账号池选择 + 资源开关与调度参数（合并面板）｜右列：模型目录（Buddy），同行两列各占 1/2 */}
      <div className="mt-4 grid grid-cols-12 items-start gap-4">
        {/* 左列：账号池选择（上）+ 资源开关与调度参数（下）合并面板，共用右上角「保存」（随 poolSet 一并落盘） */}
        <div className="col-span-6">
          <div className="card p-4">
            {/* 面板头＝账号池选择；「保存」位于面板整体右上角，一次保存账号池选择/资源开关/调度参数 */}
            <div className="mb-3 flex items-center justify-between gap-2">
              <div className="flex items-center gap-2">
                <Activity size={16} className="text-brand-500" />
                <span className="text-sm font-medium">账号池选择</span>
                <span className="text-xs text-slate-400">
                  {credAccounts.length > 0 && `已选 ${wbSelected.length}/${credAccounts.length}`}
                </span>
              </div>
              <div className="flex items-center gap-1">
                {credAccounts.length > 0 && (
                  <>
                    <button
                      className="btn-ghost px-2 py-0.5 text-xs"
                      onClick={() => setWbUids(credAccounts.map((a) => a.id))}
                    >
                      全选
                    </button>
                    <button className="btn-ghost px-2 py-0.5 text-xs" onClick={() => setWbUids([])}>
                      清空
                    </button>
                  </>
                )}
                <button
                  className="btn-outline"
                  onClick={() => void saveFlags()}
                  disabled={saving || !pool}
                  title={pool ? undefined : '池配置未加载，请先刷新重试'}
                >
                  {saving ? <Spinner /> : <Save size={15} />} 保存
                </button>
              </div>
            </div>
            <p className="mb-2 text-xs text-slate-400">
              {credAccounts.length === 0
                ? accountsError
                  ? `账号列表加载失败（${accountsError}），请点击右上角「刷新」重试`
                  : '暂无含凭证账号'
                : `勾选账号参与 WB 上游调度（清空 = 全部含凭证账号自动入池）；保存后即时生效`}
            </p>
            {/* 分组筛选（对齐 Trae T10，保存后热重载即时生效）；分组在「账号管理 → 分组管理」维护 */}
            {wbGroups.length > 0 && credAccounts.length > 0 && (
              <div className="mb-3 space-y-2 rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50">
                <div className="flex items-start gap-2">
                  <label className="shrink-0 pt-1 text-xs text-slate-500 dark:text-zinc-400">
                    分组筛选
                  </label>
                  <div className="flex flex-1 flex-wrap gap-1">
                    {wbGroups.map((g) => {
                      const active = wbPoolGroups.has(g.id);
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
                            setWbPoolGroups((prev) => {
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
                    {(wbPoolGroups.size > 0 || wbPoolTags.size > 0) && (
                      <>
                        <span className="pt-0.5 text-xs text-slate-400">
                          将纳入 {wbPoolPreview.inPool} 个账号
                          {wbPoolPreview.excluded > 0 &&
                            `，${wbPoolPreview.excluded} 个筛选外账号不参与调度`}
                        </span>
                        {wbPoolPreview.inPool === 0 && (
                          <span className="pt-0.5 text-xs font-medium text-amber-600 dark:text-amber-400">
                            当前分组/标签组合展开为空集：保存后该池将无可取号账号
                          </span>
                        )}
                      </>
                    )}
                  </div>
                </div>
                <p className="text-xs text-slate-400 dark:text-zinc-500">
                  分组筛选作用于 WB 池取号范围，保存后即时生效；不选分组 = 全部参与（分组在「账号管理 → 分组管理」维护）。
                </p>
                {/* 标签筛选（P2 需求④）：与分组合取交集（D5）；标签在「账号管理 → 标签管理」维护 */}
                {(tagDefs.length > 0 || wbPoolTags.size > 0) && (
                  <div className="flex items-start gap-2">
                    <label className="shrink-0 pt-1 text-xs text-slate-500 dark:text-zinc-400">标签筛选</label>
                    <div className="flex flex-1 flex-wrap gap-1">
                      {tagDefs.map((t) => {
                        const active = wbPoolTags.has(t.id);
                        return (
                          <button
                            key={t.id}
                            type="button"
                            className={`rounded-full border px-2 py-0.5 text-xs transition ${
                              active
                                ? 'border-teal-400 bg-teal-50 text-teal-700 dark:border-teal-500 dark:bg-teal-500/15 dark:text-teal-300'
                                : 'border-slate-200 text-slate-500 hover:border-slate-300 dark:border-zinc-700 dark:text-zinc-400 dark:hover:border-zinc-600'
                            }`}
                            onClick={() =>
                              setWbPoolTags((prev) => {
                                const next = new Set(prev);
                                if (next.has(t.id)) next.delete(t.id);
                                else next.add(t.id);
                                return next;
                              })
                            }
                          >
                            {t.name}
                          </button>
                        );
                      })}
                      {wbPoolTags.size > 0 && (
                        <span className="pt-0.5 text-xs text-slate-400">
                          与分组筛选取交集（同时满足分组与标签的账号才参与调度）
                        </span>
                      )}
                    </div>
                  </div>
                )}

                {/* 国际版（buddy-ai）开关已迁至 BuddyAI · 资源调度页（资源开关与调度参数节），
                    此处不再重复；wbFlags 仍加载/回传 buddyAi* 键，保存国内开关不会清掉国际档配置 */}
              </div>
            )}
            {credAccounts.length === 0 ? (
              accountsError ? (
                /* 审查 F1：请求失败显示错误态，不显示「暂无账号」空集引导 */
                <p className="py-4 text-center text-xs text-amber-600 dark:text-amber-400">
                  账号列表加载失败：{accountsError}，请点击右上角「刷新」重试
                </p>
              ) : (
                <p className="py-4 text-center text-xs text-slate-400">
                  暂无含凭证账号：请先在「账号管理」OAuth 登录入池，或扫描已挂载的 auth 文件（凭证写入服务端 token store）。
                </p>
              )
            ) : (
              <div className="space-y-1">
                {credAccounts.map((a) => {
                  // F-77⑤ 可观测：实时在途并发（服务未运行/未匹配时为 0）；
                  // PoolStatus.uid = 池键 = a.id（wb-<hash>），非 a.uid
                  const poolItem = wbPool.find((p) => p.uid === a.id);
                  // 分组筛选激活时，分组外账号不参与调度（整行半透明标记，对齐 Trae）
                  const filteredOut = !inWbPoolFilter(a.id);
                  return (
                    <div
                      key={a.id}
                      className={`flex items-center gap-3 rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800 ${
                        filteredOut ? 'opacity-50' : ''
                      }`}
                    >
                      <input
                        type="checkbox"
                        checked={wbSelected.includes(a.id)}
                        onChange={() => toggleWbUid(a.id)}
                      />
                      <div className="min-w-0 flex-1 truncate font-medium">{a.nickname || a.id}</div>
                      {filteredOut && <Badge tone="slate">筛选外</Badge>}
                      {a.is_current && <Badge tone="green">在线</Badge>}
                      {a.needs_relogin && <Badge tone="amber">需重新登录</Badge>}
                      {/* 运行时健康徽标：禁用/积分耗尽/冷却原因/零积分/在途·就绪（三池共用） */}
                      <PoolHealthBadges
                        s={poolItem}
                        running
                        tokenInvalid={a.needs_relogin}
                      />
                      <span className="shrink-0 text-right tabular-nums text-xs text-slate-500">
                        {a.credits_balance != null ? `${a.credits_balance.toFixed(2)} 积分` : '余额未知'}
                      </span>
                    </div>
                  );
                })}
              </div>
            )}

            {/* 资源开关与调度参数（与账号池选择同面板，共用右上角「保存」） */}
            <div className="mt-4 border-t border-slate-100 pt-3 dark:border-zinc-800">
              <div className="mb-2 flex items-center gap-2">
                <ToggleLeft size={16} className="text-brand-500" />
                <span className="text-sm font-medium">资源开关与调度参数</span>
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

              {/* 伪流式灰度（issue #213）：仅内存热切换，重启复位默认关；开关样式对齐
                  全局 API 管理「池级开关」范式——乐观更新 + 失败回滚，下一请求即生效 */}
              <div className="mt-4">
                <div className="mb-2 flex items-center gap-2">
                  <ToggleLeft size={16} className="text-brand-500" />
                  <span className="text-sm font-medium">伪流式灰度（issue #213）</span>
                  <span className="text-xs text-slate-400">
                    仅内存热切换 · 重启网关复位为关闭
                  </span>
                  <button
                    onClick={() => void loadPseudo()}
                    className="ml-auto shrink-0 rounded-md border border-slate-200 px-2 py-1 text-xs text-slate-600 transition hover:bg-slate-50 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800"
                  >
                    刷新
                  </button>
                </div>
                {pseudoEnabled === null && (
                  <p className="mb-1 text-[11px] leading-4 text-amber-600 dark:text-amber-400">
                    网关未运行或配置读取失败：已按默认（关闭）展示并置灰，启动 API 服务后点「刷新」即可操作
                  </p>
                )}
                <label
                  className={`flex items-start gap-2.5 rounded-md px-1.5 py-1 transition hover:bg-slate-100/60 dark:hover:bg-zinc-800/60 ${pseudoEnabled ? 'cursor-pointer' : 'cursor-not-allowed opacity-60'}`}
                >
                  <input
                    type="checkbox"
                    className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                    checked={pseudoEnabled ?? false}
                    disabled={pseudoEnabled === null}
                    onChange={(e) => void togglePseudo(e.target.checked)}
                  />
                  <span className="min-w-0">
                    <span className="block text-xs text-slate-700 dark:text-zinc-200">
                      启用伪流式（WB 池）
                    </span>
                    <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                      开启后客户端流式请求内部走聚合管线，成功后合成 SSE 下发；上游断流可无损换号重试（issue #213）
                    </span>
                  </span>
                </label>
                <div className="mt-1.5 space-y-1">
                  <div className="rounded-md px-1.5 py-1">
                    <div className="flex items-center justify-between gap-3">
                      <span className="text-xs text-slate-700 dark:text-zinc-200">换号重试预算</span>
                      <div className="flex items-center gap-2">
                        <input
                          type="number"
                          className="w-24 rounded-md border border-slate-200 bg-transparent px-2 py-1 text-right text-xs tabular-nums focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900"
                          min={0}
                          max={16}
                          step={1}
                          value={pseudoBudget ?? 0}
                          disabled={pseudoBudget === null}
                          onChange={(e) => {
                            const v = Number(e.target.value);
                            if (Number.isFinite(v)) setPseudoBudget(Math.min(16, Math.max(0, v)));
                          }}
                          onBlur={() => {
                            const clamped = Math.min(Math.max(Math.round(pseudoBudget ?? 0), 0), 16);
                            setPseudoBudget(clamped);
                            if (clamped !== savedPseudoRef.current.budget) void savePseudoParam('budget', clamped);
                          }}
                        />
                        <span className="w-8 shrink-0 text-[11px] text-slate-400">次</span>
                      </div>
                    </div>
                    <p className="mt-1 text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                      单请求换号重试次数上限；0 = 旧行为（重试机会 = 池内账号数）；失焦保存，下一条请求即生效
                    </p>
                  </div>
                  <div className="rounded-md px-1.5 py-1">
                    <div className="flex items-center justify-between gap-3">
                      <span className="text-xs text-slate-700 dark:text-zinc-200">keep-alive 间隔</span>
                      <div className="flex items-center gap-2">
                        <input
                          type="number"
                          className="w-24 rounded-md border border-slate-200 bg-transparent px-2 py-1 text-right text-xs tabular-nums focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900"
                          min={0}
                          max={60}
                          step={1}
                          value={pseudoKeepalive ?? 0}
                          disabled={pseudoKeepalive === null}
                          onChange={(e) => {
                            const v = Number(e.target.value);
                            if (Number.isFinite(v)) setPseudoKeepalive(Math.min(60, Math.max(0, v)));
                          }}
                          onBlur={() => {
                            const clamped = Math.min(Math.max(Math.round(pseudoKeepalive ?? 0), 0), 60);
                            setPseudoKeepalive(clamped);
                            if (clamped !== savedPseudoRef.current.keepalive) void savePseudoParam('keepalive', clamped);
                          }}
                        />
                        <span className="w-8 shrink-0 text-[11px] text-slate-400">秒</span>
                      </div>
                    </div>
                    <p className="mt-1 text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                      伪流式聚合等待期向客户端发 SSE 注释帧的间隔（两档共用）；0 = 关闭；失焦保存，下一条请求即生效
                    </p>
                  </div>
                </div>
              </div>

              {/* 调度参数（F-76②/③/F-77：数值热参数，保存即热生效） */}
              <div className="mt-4">
                <div className="mb-2 flex items-center gap-2">
                  <Gauge size={15} className="text-brand-500" />
                  <span className="text-sm font-medium">调度参数</span>
                  <span className="text-xs text-slate-400">保存即热生效 · 并发 0 = 不限 · 对冲/池粘性 0 = 关闭</span>
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
            </div>

            <p className="mt-3 text-xs text-slate-400 dark:text-zinc-500">
              保存后即时生效；Trae 池的成员/分组与调度策略在 Trae「资源调度」页或全局 API 管理配置，本页不改动。
            </p>
          </div>
        </div>

        {/* 右列：模型目录（Buddy） */}
        <div className="col-span-6">
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
                      <th className="px-3 py-2 text-left">厂商</th>
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
                        <td className="px-3 py-2 text-xs text-slate-500">
                          {vendorMap.get(m.id) || '—'}
                        </td>
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
