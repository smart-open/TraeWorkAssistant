import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { SOURCE_LABELS, SOURCES, type BoardSource } from '../../components/ChartFilterBar';
import { api, listen } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import { fmtCredits, dateStrToEndTs } from '../../lib/format';
import KpiRow, { type PlatformKpi, type PlatformScope } from './KpiRow';
import ExpiryTab from './ExpiryTab';
import CreditsTab from './CreditsTab';
import TokensTab, { type GatewayDays } from './TokensTab';
import SessionsTab from './SessionsTab';
import { qoderEarnedByDateFiltered, traeEarnedByDateFiltered, wbEarnedByDateFiltered } from './adapters';
import type {
  AccountView,
  GroupView,
  QoderCheckinRecord,
  QoderCreditsResult,
  QoderCreditsSnapshot,
  TraeExpiredEntsResult,
  UnifiedModel,
  UsageDayView,
  UsageHistoryResult,
  WbCheckinRecord,
  WbCreditsResult,
  WbCreditsSnapshot,
  WbTokenStats,
  WbUsageFallback,
  WbUsageOfficialAll,
} from '../../types';

/** 多选下拉（chips 触发 + 面板勾选；看板顶部分组/账号筛选控件） */
function MultiSelectDropdown({
  label,
  options,
  selected,
  onChange,
  onOpen,
  width = 'w-56',
}: {
  label: string;
  options: { id: string; name: string; color?: string }[];
  selected: string[];
  onChange: (ids: string[]) => void;
  /** 打开面板时回调（分组下拉重拉成员数据用） */
  onOpen?: () => void;
  width?: string;
}) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState('');
  const ref = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [open]);
  const toggle = (id: string) =>
    onChange(selected.includes(id) ? selected.filter((x) => x !== id) : [...selected, id]);
  // 全量搜索（文档需求②：账号多选可直接全量搜账号）；打开时重置
  const kw = search.trim().toLowerCase();
  const visible = kw ? options.filter((o) => o.name.toLowerCase().includes(kw) || o.id.toLowerCase().includes(kw)) : options;
  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        className={`btn-outline !py-1.5 !text-xs ${selected.length ? '!border-sky-400 text-sky-600 dark:text-sky-400' : ''}`}
        onClick={() => {
          setOpen((v) => !v);
          setSearch('');
          if (!open) onOpen?.();
        }}
      >
        {label}
        {selected.length > 0 && (
          <span className="ml-1 rounded-full bg-sky-500 px-1.5 text-[10px] font-medium text-white">
            {selected.length}
          </span>
        )}
      </button>
      {open && (
        <div className={`absolute right-0 z-30 mt-1 max-h-72 ${width} overflow-auto rounded-lg border border-slate-200 bg-white p-2 shadow-lg dark:border-zinc-700 dark:bg-zinc-900`}>
          {options.length > 5 && (
            <input
              autoFocus
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder={`搜索${label}…`}
              className="input mb-1 !py-1 !text-xs"
            />
          )}
          {visible.length === 0 && (
            <div className="px-2 py-1 text-xs text-slate-400">
              {options.length === 0 ? '暂无可选项' : '无匹配项'}
            </div>
          )}
          {visible.map((o) => (
            <label
              key={o.id}
              className="flex cursor-pointer items-center gap-2 rounded px-2 py-1 text-xs hover:bg-slate-50 dark:hover:bg-zinc-800"
            >
              <input
                type="checkbox"
                checked={selected.includes(o.id)}
                onChange={() => toggle(o.id)}
                className="h-3.5 w-3.5"
              />
              {o.color && (
                <span className="inline-block h-2.5 w-2.5 shrink-0 rounded-full" style={{ background: o.color }} />
              )}
              <span className="truncate">{o.name}</span>
            </label>
          ))}
          {selected.length > 0 && (
            <button
              type="button"
              onClick={() => onChange([])}
              className="mt-1 w-full rounded px-2 py-1 text-left text-xs text-slate-400 hover:bg-slate-50 dark:hover:bg-zinc-800"
            >
              清空筛选（全部）
            </button>
          )}
        </div>
      )}
    </div>
  );
}

/**
 * 积分看板（credits-dashboard-plan.md；平台拆分调整）：
 * 同一套看板组件按 platform 参数分别渲染 Trae / Buddy / Qoder 三个独立页面——
 * Trae 页挂在 `credits` 视图，Buddy 页挂在 `buddy-credits` 视图，
 * Qoder 页挂在 `qoder-credits` 视图，互不混装。
 * 布局：PageHeader（标题+概述）→ KPI 7 卡 → Tab 工具行（Tab 切换 + 刷新）→ Tab 内容。
 */

type BoardTab = 'credits' | 'tokens' | 'expiry' | 'sessions';

const TABS: { key: BoardTab; label: string }[] = [
  { key: 'credits', label: '积分统计' },
  { key: 'tokens', label: 'Token 统计' },
  { key: 'expiry', label: '积分到期' },
  { key: 'sessions', label: '会话明细' },
];

/** 页面标题/概述（三页同一结构：积分余额 · 积分明细 · Token 统计 · 到期日历 · 会话明细） */
const PAGE_META: Record<'trae' | 'buddy' | 'qoder', { title: string; desc: string }> = {
  trae: { title: 'Trae · 积分看板', desc: '积分余额 · 积分明细 · Token 统计 · 到期日历 · 会话明细' },
  buddy: { title: 'Buddy · 积分看板', desc: '积分余额 · 积分明细 · Token 统计 · 到期日历 · 会话明细' },
  qoder: { title: 'Qoder · 积分看板', desc: '积分余额 · 积分明细 · Token 统计 · 到期日历 · 会话明细' },
};

function localDate(d: Date): string {
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

export default function CreditsDashboard({ platform }: { platform: 'trae' | 'buddy' | 'qoder' }) {
  const pushToast = useAppStore((s) => s.pushToast);
  const accounts = useAppStore((s) => s.accounts);
  const creditsDaily = useAppStore((s) => s.creditsDaily);
  const refreshRemainingCredits = useAppStore((s) => s.refreshRemainingCredits);
  const refreshCreditsDaily = useAppStore((s) => s.refreshCreditsDaily);
  // 注（docker/web 版适配）：桌面版的本地 Token 统计（扫描本机 ~/.workbuddy 会话文件）
  // 在 Web 版不可用——本地数据源整体下线（tokenStats 恒 null）

  const isTrae = platform === 'trae';
  const isQoder = platform === 'qoder';

  const [tab, setTab] = useState<BoardTab>('credits');
  const [loading, setLoading] = useState(false);

  // Trae 侧数据（官网源消耗明细 + store 快照/签到历史；Buddy/Qoder 页不加载）
  const [usage, setUsage] = useState<UsageHistoryResult | null>(null);
  // Trae 逐包到期（issue #82 T5.1：expired_ents 实时查询，含已过期包——credit_packs 缓存只含未过期可用包）
  const [expiredEnts, setExpiredEnts] = useState<TraeExpiredEntsResult | null>(null);
  // Buddy 侧数据（Trae 页不加载）
  const [wbCredits, setWbCredits] = useState<WbCreditsResult | null>(null);
  // 签到日志取 90 天窗口：KPI 今日新增 + 积分统计 Tab「区间总获得」共用
  const [wbCheckin, setWbCheckin] = useState<WbCheckinRecord[]>([]);
  const [wbOfficial, setWbOfficial] = useState<WbUsageOfficialAll | null>(null);
  const [wbFallback, setWbFallback] = useState<WbUsageFallback | null>(null);
  // 快照时序（§2.2 方案 B：earned 已落库；null 日期回退方案 A 签到聚合）
  const [wbSnapshots, setWbSnapshots] = useState<WbCreditsSnapshot[]>([]);
  // Qoder 侧数据（官网积分查询 + 本地快照时序 + 签到日志 90 天窗口；其他页不加载）
  const [qoderCredits, setQoderCredits] = useState<QoderCreditsResult | null>(null);
  const [qoderSnapshots, setQoderSnapshots] = useState<QoderCreditsSnapshot[]>([]);
  const [qoderCheckin, setQoderCheckin] = useState<QoderCheckinRecord[]>([]);
  // 各统计 Tab 独立数据源状态（§3.4）；默认源按平台选择
  // （Trae/Buddy 官网优先；Qoder 官网无消耗明细 → 本地快照），积分统计两平台均默认官网源
  const [creditsSource, setCreditsSource] = useState<BoardSource>(platform === 'qoder' ? 'local' : 'official');
  // Web 版无本地 token 源：Trae 官网优先，Buddy/Qoder 默认网关（main 侧 Buddy 默认 local 为桌面专属）
  const [tokensSource, setTokensSource] = useState<BoardSource>(platform === 'trae' ? 'official' : 'gateway');
  // 会话明细 Tab 联动刷新键（refresh 完成后 +1 触发重查缓存，issue #82）
  const [sessionsRefreshKey, setSessionsRefreshKey] = useState(0);

  // ---- 顶部分组/账号筛选（P1 需求②）：平台视图内全局生效（KPI/积分/到期/Token 联动）----
  // 分组下拉数据源 = 本池 groups_list().GroupView{id,name,uids}（共享分组生效后自动
  // 包含其他应用创建的分组）；账号候选随分组筛选联动。筛选状态内存态（P1 不做持久化）。
  const [poolGroups, setPoolGroups] = useState<GroupView[]>([]);
  const [groupFilter, setGroupFilter] = useState<string[]>([]);
  const [accountFilter, setAccountFilter] = useState<string[]>([]);
  // 分组成员可能在他处（账号页/其他池）变更，下拉打开时重拉（失败保留旧值）
  const reloadPoolGroups = useCallback(() => {
    const loader =
      platform === 'trae'
        ? api.groups.list()
        : platform === 'buddy'
          ? api.workbuddy.groups.list()
          : api.qoder.groups.list();
    loader.then(setPoolGroups).catch(() => {});
  }, [platform]);
  useEffect(() => {
    reloadPoolGroups();
    // 平台切换时重置筛选（三实例共享组件，状态按平台语义独立）
    setGroupFilter([]);
    setAccountFilter([]);
  }, [platform, reloadPoolGroups]);
  // 生效 uid 集 = 组筛选成员集 ∩ 账号筛选集（均空 = 不限，null）
  const visibleUids = useMemo<Set<string> | null>(() => {
    let set: Set<string> | null = null;
    if (groupFilter.length > 0) {
      set = new Set();
      for (const g of poolGroups) {
        if (groupFilter.includes(g.id)) g.uids.forEach((u) => set!.add(u));
      }
    }
    if (accountFilter.length > 0) {
      const f = new Set(accountFilter);
      set = set ? new Set([...set].filter((u) => f.has(u))) : f;
    }
    return set;
  }, [poolGroups, groupFilter, accountFilter]);
  // 有筛选时可见账号 uid 集合（空集 = 筛选结果为空，各面板如实展示零数据）
  const filterByUid = useCallback(
    <T extends { user_id: string }>(arr: T[]): T[] =>
      visibleUids ? arr.filter((x) => visibleUids.has(x.user_id)) : arr,
    [visibleUids],
  );
  const visibleAccounts = useMemo<AccountView[]>(() => filterByUid(accounts), [accounts, filterByUid]);
  const visibleWbCredits = useMemo<WbCreditsResult | null>(
    () => (wbCredits ? { ...wbCredits, accounts: filterByUid(wbCredits.accounts) } : wbCredits),
    [wbCredits, filterByUid],
  );
  const visibleQoderCredits = useMemo<QoderCreditsResult | null>(
    () => (qoderCredits ? { ...qoderCredits, accounts: filterByUid(qoderCredits.accounts) } : qoderCredits),
    [qoderCredits, filterByUid],
  );
  // 消耗明细/Token 明细的账号级过滤（按明细携带的 user_id 过滤；无 uid 维度的
  // 数据源不静默截断，工具行显示「不支持账号级筛选」徽标）
  const visibleUsage = useMemo<UsageHistoryResult | null>(
    () => (usage ? { ...usage, accounts: filterByUid(usage.accounts) } : usage),
    [usage, filterByUid],
  );
  // 分组筛选生效 uid 集（仅分组，不含账号筛选；issue #82 问题4-2）
  const groupUids = useMemo<Set<string> | null>(() => {
    if (groupFilter.length === 0) return null;
    const set = new Set<string>();
    for (const g of poolGroups) {
      if (groupFilter.includes(g.id)) g.uids.forEach((u) => set.add(u));
    }
    return set;
  }, [poolGroups, groupFilter]);
  // 账号多选候选（随分组筛选联动：勾选分组后仅列组内账号）。
  // 审查修复（issue #82 问题4-2）：候选集必须只随分组收窄，不能取已按 accountFilter
  // 过滤后的 visible* 集合——否则勾选任一账号后其余候选立即从下拉消失，多选无法叠加。
  const accountCandidates = useMemo<{ id: string; name: string }[]>(() => {
    const base =
      platform === 'trae'
        ? accounts.map((a) => ({ id: a.user_id, name: a.name }))
        : platform === 'buddy'
          ? (wbCredits?.accounts ?? []).map((a) => ({ id: a.user_id, name: a.name }))
          : (qoderCredits?.accounts ?? []).map((a) => ({ id: a.user_id, name: a.name }));
    return base.filter((a) => a.id && (!groupUids || groupUids.has(a.id)));
  }, [platform, accounts, wbCredits, qoderCredits, groupUids]);
  // 会话明细账号下拉按平台切换（Trae=vault 账号；Buddy/Qoder=积分接口账号清单；
  // Qoder 数据源=网关请求级本地流水，issue #82 问题5）。
  // 跟随顶部分组/账号筛选（shared-groups-tags 合并集成：平台视图内全局生效契约）。
  const sessionAccountOptions = useMemo(
    () =>
      platform === 'buddy'
        ? filterByUid(wbCredits?.accounts ?? []).map((a) => ({ user_id: a.user_id, name: a.name }))
        : platform === 'qoder'
          ? filterByUid(qoderCredits?.accounts ?? []).map((a) => ({ user_id: a.user_id, name: a.name }))
          : filterByUid(accounts).map((a) => ({ user_id: a.user_id, name: a.name })),
    [platform, wbCredits, qoderCredits, accounts, filterByUid],
  );
  const filtersActive = groupFilter.length > 0 || accountFilter.length > 0;

  // Token 统计 Tab 数据（Web 版无本地 token 源，tokenStats 恒 null；网关源 SQLite 直查）
  const [tokenStats] = useState<WbTokenStats | null>(null);
  const [gateway, setGateway] = useState<GatewayDays | null>(null);
  const [gatewayLoading, setGatewayLoading] = useState(false);
  // 网关数据触发点有两个：Token 统计 Tab 与 积分统计 Tab 的网关源（审查修复）
  const gatewayLoadedRef = useRef(false);
  // Qoder 网关就绪标记（issue #82 T3.1）：统一目录含 enabled 的 qoder 源才点亮网关数据源，
  // 否则 sourceDisabled 提示未接入（静态文案的动态化）
  const [qoderGatewayReady, setQoderGatewayReady] = useState(false);

  const loadUsage = useCallback(
    // 返回是否成功，供 refresh 判定是否弹「看板数据已刷新」（失败已有专属 error toast）
    async (fresh: boolean): Promise<boolean> => {
      try {
        setUsage(await api.accounts.usageHistory(fresh));
        return true;
      } catch (err) {
        pushToast('error', `Trae 消耗明细查询失败：${String(err)}`);
        return false;
      }
    },
    [pushToast],
  );

  // Trae 逐包到期（issue #82 T5.1）：expired_ents 实时轻请求；失败保留旧数据展示 + toast
  // （stale-on-error 惯例，不置 null 清空既有到期条目）
  const loadExpiredEnts = useCallback(async (): Promise<boolean> => {
    try {
      setExpiredEnts(await api.accounts.traeExpiredEnts());
      return true;
    } catch (err) {
      pushToast('error', `Trae 逐包到期查询失败：${String(err)}`);
      return false;
    }
  }, [pushToast]);

  const loadBuddy = useCallback(
    // 返回 credits 主链路是否真实成功（辅助源 allSettled 优雅回退不计失败），
    // 供 refresh 决定是否弹「看板数据已刷新」，避免失败提示与成功提示并存
    async (fresh: boolean): Promise<boolean> => {
      let ok = true;
      await Promise.allSettled([
        api.workbuddy
          .creditsFetch(undefined, fresh)
          .then((r) => {
            setWbCredits(r);
            if (fresh && r.stale) {
              ok = false;
              pushToast('warn', 'Buddy 积分刷新失败，已回退展示历史缓存数据');
            }
          })
          .catch((err) => {
            ok = false;
            pushToast('error', `Buddy 积分查询失败：${String(err)}`);
          }),
        // 今日新增（§2.2 方案 A）+ 积分统计「区间总获得」：签到日志 reward 聚合（90 天）
        api.workbuddy
          .checkinResults(90)
          .then(setWbCheckin)
          .catch(() => setWbCheckin([])),
        // 官网用量聚合（31 天零填充；今日消耗 + 积分统计官网源）
        api.workbuddy
          .usageOfficialAll()
          .then(setWbOfficial)
          .catch(() => setWbOfficial(null)),
        // 快照差分回退（365 天；本地源 + 官网失败回退）
        api.workbuddy
          .usageFallback()
          .then(setWbFallback)
          .catch(() => setWbFallback(null)),
        // 快照时序（方案 B：Buddy 获得积分优先口径；失败回退签到聚合）
        api.workbuddy
          .creditsHistoryList()
          .then((r) => setWbSnapshots(r.snapshots ?? []))
          .catch(() => setWbSnapshots([])),
      ]);
      return ok;
    },
    [pushToast],
  );

  // ---- Qoder 页数据装载 ----
  // 先查积分（后端全量查询且全部成功时落当日快照，同日覆盖），完成后再读快照时序，
  // 保证「今日快照」在首次装载即可见；签到日志取 90 天窗口（与 Buddy 侧同口径）：
  // KPI 今日新增回退 + 积分统计 Tab「区间总获得」共用，避免短窗口少计
  const loadQoder = useCallback(
    // 返回 credits 主链路是否真实成功（辅助源 allSettled 优雅回退不计失败），同 loadBuddy
    async (fresh: boolean): Promise<boolean> => {
      let ok = true;
      await api.qoder
        .creditsFetch(undefined, fresh)
        .then((r) => {
          setQoderCredits(r);
          if (fresh && r.stale) {
            ok = false;
            pushToast('warn', 'Qoder 积分刷新失败，已回退展示历史缓存数据');
          }
        })
        .catch((err) => {
          ok = false;
          pushToast('error', `Qoder 积分查询失败：${String(err)}`);
        });
      await Promise.allSettled([
        api.qoder
          .creditsHistoryList()
          .then((r) => setQoderSnapshots(r.snapshots ?? []))
          .catch(() => setQoderSnapshots([])),
        api.qoder
          .checkinResults(90)
          .then(setQoderCheckin)
          .catch(() => setQoderCheckin([])),
      ]);
      return ok;
    },
    [pushToast],
  );

  // ---- Token 统计 Tab 数据源（Web 版仅网关源；本地 token 扫描为桌面版专属）----
  const loadGateway = useCallback(async (): Promise<boolean> => {
    setGatewayLoading(true);
    try {
      // 看板按单平台展示，拉取 Trae/Buddy/Qoder 三池（custom 池仅 API 服务页 UsageStatsPanel 使用）
      // Qoder 网关用量管道已铺（阶段 3 四桶基建），上游未接入前 qoder 池为空；
      // 三平台一并装载，上游接入后无需再改门控
      const failed: string[] = [];
      const [trae, buddy, qoder] = await Promise.all([
        api.apiServer.usageStats(90).catch(() => {
          failed.push('Trae');
          return [] as UsageDayView[];
        }),
        api.apiServer.wbUsageStats(90).catch(() => {
          failed.push('Buddy');
          return [] as UsageDayView[];
        }),
        api.apiServer.qoderUsageStats(90).catch(() => {
          failed.push('Qoder');
          return [] as UsageDayView[];
        }),
      ]);
      setGateway({ trae, buddy, qoder });
      if (failed.length > 0) {
        pushToast('warn', `网关用量加载失败（${failed.join('、')}池）：已按空态展示，可点击「刷新」重试`);
        return false;
      }
      return true;
    } finally {
      setGatewayLoading(false);
    }
  }, [pushToast]);

  // 懒加载（§3.4）：网关数据在 Token Tab 或 积分统计 Tab 网关源首次激活时拉取
  // （SQLite 直查，轻）；本地 token 源 Web 版禁用不触发
  useEffect(() => {
    const needGateway = tab === 'tokens' || (tab === 'credits' && creditsSource === 'gateway');
    if (needGateway && !gatewayLoadedRef.current) {
      gatewayLoadedRef.current = true;
      void loadGateway();
    }
  }, [platform, tab, creditsSource, loadGateway]);

  const refresh = useCallback(
    async (fresh = false, silent = false) => {
      // silent：调度器联动（board-data-synced）静默重读缓存——不置全局 loading、
      // 不强制最短展示时长，避免看板每次同步后闪烁并瞬时禁用刷新按钮
      if (!silent) setLoading(true);
      // 汇总各真实数据源装载结果（失败方均已弹专属 toast），供末尾判定是否弹成功提示——
      // 避免「已刷新」与失败提示并存造成语义矛盾
      let ok = true;
      const work = async (): Promise<boolean> => {
        // Trae fresh 时先串行刷新剩余积分：后端逐账号网络查询并落盘 credits_daily.json 快照
        // （旧页即串行保证此顺序），完成后再并行读取；否则日快照竞速先返回旧值，今日 KPI 滞后
        if (isTrae && fresh) ok = (await refreshRemainingCredits()) && ok;
        await Promise.all([
          // 按平台只加载本侧数据源
          isTrae ? refreshCreditsDaily() : Promise.resolve(),
          isTrae ? loadUsage(fresh).then((v) => void (ok = ok && v)) : Promise.resolve(),
          // 逐包到期实时查询（issue #82 T5.1）：仅「刷新数据」联动重拉；
          // 进入页面由独立 effect 拉取、调度器静默同步不触发（避免后台静默网络请求）
          isTrae && fresh ? loadExpiredEnts().then((v) => void (ok = ok && v)) : Promise.resolve(),
          isTrae || isQoder
            ? Promise.resolve()
            : loadBuddy(fresh).then((v) => void (ok = ok && v)),
          isQoder ? loadQoder(fresh).then((v) => void (ok = ok && v)) : Promise.resolve(),
          // Qoder 网关就绪探测（issue #82 T3.1）：统一目录含 enabled 的 qoder 源 → 点亮网关数据源
          isQoder
            ? api.apiServer
                .unifiedModels()
                .then((list: UnifiedModel[]) =>
                  setQoderGatewayReady(list.some((m) => m.sources.some((s) => s.pool === 'qoder' && s.enabled))),
                )
                .catch(() => setQoderGatewayReady(false))
            : Promise.resolve(),
          // 已加载过的源随全局刷新联动（fresh=true 网关直查）
          gatewayLoadedRef.current && fresh
            ? loadGateway().then((v) => void (ok = ok && v))
            : Promise.resolve(),
        ]);
        // 会话明细 Tab 纯读缓存：随看板刷新联动重查（含调度器静默同步）
        setSessionsRefreshKey((k) => k + 1);
        return ok;
      };
      try {
        const allOk = silent ? await work() : await withMinDelay(work(), 600);
        if (fresh && allOk) {
          pushToast('success', '看板数据已刷新');
        }
      } finally {
        if (!silent) setLoading(false);
      }
    },
    [isTrae, isQoder, refreshRemainingCredits, refreshCreditsDaily, loadUsage, loadExpiredEnts, loadBuddy, loadQoder, loadGateway, pushToast],
  );

  // refresh 引用最新闭包（依赖链深，listen effect 只随 platform 注册一次，
  // 经 ref 调用避免重注册，也保证读到最新 store 状态）
  const refreshRef = useRef(refresh);
  useEffect(() => {
    refreshRef.current = refresh;
  }, [refresh]);

  useEffect(() => {
    void refresh(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Trae 逐包到期（issue #82 T5.1）：进入页面实时查询一次；「刷新数据」经 refresh(fresh=true)
  // 联动重拉，调度器静默同步不触发（实时轻请求，避免后台静默网络请求）
  useEffect(() => {
    if (isTrae) void loadExpiredEnts();
  }, [isTrae, loadExpiredEnts]);

  // 调度器看板数据同步成功后联动重读缓存（issue #61：页面常驻时数据不再滞后）；
  // payload.platform 与本视图平台匹配才刷新，静默走 refresh(false, true)（纯读缓存、零网络、无 loading 闪烁）
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    listen<{ platform: string }>('board-data-synced', (e) => {
      if (e.payload.platform === platform) void refreshRef.current(false, true);
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [platform]);

  const today = localDate(new Date());

  // Trae 筛选态「今日新增」按账号明细推导结果（skippedDates = 老快照行缺 accounts
  // 明细被跳过的日数，仅历史迁移前行会命中，今日恒有明细）。提升为独立 memo 供
  // KPI 数值与 earnedHint 口径标注共用，避免为取标注重复推导
  const traeFilteredEarned = useMemo(
    () => (visibleUids ? traeEarnedByDateFiltered(creditsDaily, visibleUids) : null),
    [creditsDaily, visibleUids],
  );

  // ---- Trae KPI（口径对齐原积分看板）----
  const traeKpi = useMemo<PlatformKpi>(() => {
    const nowSec = Date.now() / 1000;
    const horizon = nowSec + 7 * 86400;
    let totalCredits = 0;
    let packages = 0;
    let expiring = 0;
    for (const a of visibleAccounts) {
      totalCredits += a.remaining_credits ?? 0;
      // 积分包总数：积分包 + 会员包计数（expiryItems 口径）
      if (a.credits_expire_at != null) packages += 1;
      if (a.membership_expire != null) packages += 1;
      // 7 天内到期：按剩余积分额度合计（非包数）；下限 now 排除已过期包
      if (a.credits_expire_at != null && a.credits_expire_at > nowSec && a.credits_expire_at <= horizon) {
        expiring += a.remaining_credits ?? 0;
      }
    }
    // 今日新增：快照 earned 优先（积分包 CycleStartTime 归日口径）；
    // Web 版无 Trae 逐条积分流水，无 history delta 回退（快照未统计时记 0）。
    // KPI 余额/包数维持快照账号级口径（逐包实时拉取 N 账号成本过高，仅在
    // 积分到期 Tab 按需逐包展示，见 ExpiryTab）
    const snap = creditsDaily.find((s) => s.date === today);
    // 审查修复：有账号筛选时按筛选口径取数（快照 accounts 明细，与积分统计 Tab 同源），
    // 无明细的老快照按过滤跳过；无筛选时保持原快照口径
    const todayEarned = traeFilteredEarned
      ? (traeFilteredEarned.byDate.get(today) ?? 0)
      : snap && snap.earned > 0
        ? snap.earned
        : 0;
    // 今日消耗：官网接口明细优先（credits_float 实际口径），回退快照 consumed。
    // 审查修复：按当前筛选口径取数（visibleUsage），与「筛选作用于 KPI」语义一致
    const usageVal =
      visibleUsage?.accounts.reduce(
        (s, a) => s + (a.daily.find((d) => d.date === today)?.credits ?? 0),
        0,
      ) ?? 0;
    // 审查 No.10：筛选明细口径可用时完全信任其值（含 0——筛掉全部有消耗账号后应显示
    // 0，而非静默混入未过滤的全局快照口径）；仅明细缺失（fetch 失败/未加载）时回退快照
    const todayConsumed = visibleUsage != null ? usageVal : (snap?.consumed ?? 0);
    return {
      accounts: visibleAccounts.length,
      totalCredits,
      packages,
      todayEarned,
      todayConsumed,
      expiring7d: expiring,
    };
  }, [visibleAccounts, traeFilteredEarned, creditsDaily, visibleUsage, today]);

  // Trae「可用积分总数」卡细分（对齐原积分看板 totalHint）；账号无通用/Work 字段时回退通用文案
  const traeTotalHint = useMemo(() => {
    if (!isTrae) return undefined;
    if (!visibleAccounts.some((a) => a.general_credits != null || a.work_credits != null)) {
      return '总剩余可用积分';
    }
    const generalTotal = visibleAccounts.reduce((s, a) => s + (a.general_credits ?? 0), 0);
    const workTotal = visibleAccounts.reduce((s, a) => s + (a.work_credits ?? 0), 0);
    return `通用 ${fmtCredits(generalTotal)} 积分 · Work ${fmtCredits(workTotal)} 积分`;
  }, [isTrae, visibleAccounts]);

  // ---- Buddy KPI（§2.1/§2.2）----
  const buddyKpi = useMemo<PlatformKpi>(() => {
    const nowSec = Date.now() / 1000;
    const horizon = nowSec + 7 * 86400;
    const accs = visibleWbCredits?.accounts ?? [];
    let totalCredits = 0;
    let packages = 0;
    let expiring = 0;
    for (const a of accs) {
      totalCredits += a.balance ?? 0;
      for (const p of a.packages ?? []) {
        // 包计数与到期日历对齐：remaining > 0 才计
        if (p.remaining <= 0) continue;
        packages += 1;
        if (p.expire_ts != null && p.expire_ts > nowSec && p.expire_ts <= horizon) {
          expiring += p.remaining;
        }
      }
    }
    // 今日新增：有账号筛选时按筛选口径取数（与积分统计 Tab 同源的逐账号差分+签到归并）；
    // 无筛选时方案 B 快照 earned 优先，回退方案 A 签到 reward 聚合
    let todayEarned: number;
    if (visibleUids) {
      todayEarned = wbEarnedByDateFiltered(wbSnapshots, wbCheckin, visibleUids).get(today) ?? 0;
    } else {
      const snapEarned = wbSnapshots.find((s) => s.date === today)?.earned;
      const checkinEarned = wbCheckin
        .filter((r) => r.date === today)
        .reduce((s, r) => s + (r.reward ?? 0), 0);
      todayEarned = snapEarned != null ? Math.max(0, snapEarned) : checkinEarned;
    }
    // 今日消耗：官方用量优先（31 天窗口），回退快照差分；两源冷启动均可能返回
    // {status:'empty'} 空态（缺 summary 字段），?. 只保护第一层，summary 须可选链（issue #72）
    const todayConsumed =
      wbOfficial?.summary?.usage_today ?? wbFallback?.summary?.usage_today ?? 0;
    return {
      accounts: accs.length,
      totalCredits,
      packages,
      todayEarned,
      todayConsumed,
      expiring7d: expiring,
    };
  }, [visibleWbCredits, visibleUids, wbCheckin, wbSnapshots, wbOfficial, wbFallback, today]);

  // ---- Qoder KPI（官网积分查询 + 本地快照差分）----
  const qoderKpi = useMemo<PlatformKpi>(() => {
    const nowSec = Date.now() / 1000;
    const horizon = nowSec + 7 * 86400;
    const accs = visibleQoderCredits?.accounts ?? [];
    let totalCredits = 0;
    let packages = 0;
    let expiring = 0;
    for (const a of accs) {
      totalCredits += a.total ?? 0;
      // 包明细已含 plan 订阅配额包（R-11 逐包口径）→ 独立 Plan 条目不重复计入；
      // 判定与到期日历同款（some 不过滤 amount）：plan 包已用完也视为「明细已含」
      const hasPlanPkg = (a.packages ?? []).some((p) => p.source === 'plan');
      for (const p of a.packages ?? []) {
        // 包计数与到期日历对齐：剩余未知或 > 0 计入，已用完不计
        if (p.amount != null && p.amount <= 0) continue;
        packages += 1;
        const endTs = dateStrToEndTs(p.expire_at);
        if (endTs != null && endTs > nowSec && endTs <= horizon) expiring += p.amount ?? 0;
      }
      // 7 天内到期含 Plan 订阅重置额度：订阅周期在窗口内到期，plan 剩余全额计入
      if (!hasPlanPkg) {
        const planEnd = dateStrToEndTs(a.plan_expires_at);
        if (planEnd != null && (a.plan_credits ?? 0) > 0) {
          // 包计数与到期日历口径对齐：包明细缺 plan 包（明细接口失败/老缓存回退
          // 聚合口径）时，日历会补一条「Plan 订阅重置」独立条目 → 此处同步 +1
          packages += 1;
          if (planEnd > nowSec && planEnd <= horizon) expiring += a.plan_credits ?? 0;
        }
      }
    }
    // 今日新增：有账号筛选时按筛选口径取数（签到日志逐条过滤，与积分统计 Tab 同源）；
    // 无筛选时快照 earned（签到合计）优先，回退签到日志 reward 聚合（fetch 未落快照时）
    const todaySnap = qoderSnapshots.find((s) => s.date === today);
    let todayEarned: number;
    if (visibleUids) {
      todayEarned = qoderEarnedByDateFiltered(qoderCheckin, visibleUids).get(today) ?? 0;
    } else {
      const checkinEarned = qoderCheckin
        .filter((r) => r.date === today)
        .reduce((s, r) => s + (r.reward ?? 0), 0);
      todayEarned = todaySnap?.earned != null ? Math.max(0, todaySnap.earned) : checkinEarned;
    }
    return {
      accounts: accs.length,
      totalCredits,
      packages,
      todayEarned,
      // null = 差分不可比（首日/账号数变动日）→ KPI 卡显示「—」，不与零消耗混淆
      todayConsumed: todaySnap?.consumed ?? null,
      expiring7d: expiring,
    };
  }, [visibleQoderCredits, visibleUids, qoderSnapshots, qoderCheckin, today]);

  const kpi = isTrae ? traeKpi : isQoder ? qoderKpi : buddyKpi;

  // 可用积分总数卡 hint：Buddy/Qoder 缓存/stale 状态说明；无数据时不给失真提示
  const creditsResult = platform === 'trae' ? null : platform === 'buddy' ? wbCredits : qoderCredits;
  const creditsHint = !creditsResult
    ? undefined
    : creditsResult.stale
      ? '历史缓存回退（本次查询失败）'
      : creditsResult.cached
        ? '缓存数据（≥10 分钟）'
        : '实时数据';
  // 今日新增口径诚实标注（§8.4）：快照 earned 已统计 vs 回退签到口径
  const todaySnapEarned = wbSnapshots.find((s) => s.date === today)?.earned;
  const qoderTodaySnap = qoderSnapshots.find((s) => s.date === today);
  const qoderLastSnapDate = qoderSnapshots.reduce<string | null>((acc, s) => (!acc || s.date > acc ? s.date : acc), null);
  const earnedHint = isTrae
    ? filtersActive && traeFilteredEarned && traeFilteredEarned.skippedDates > 0
      ? '筛选口径 · 部分老快照日缺账号明细未计入推导'
      : undefined
    : isQoder
      ? qoderTodaySnap?.earned != null
        ? '口径：当日签到奖励合计'
        : qoderCheckin.length > 0
          ? '仅含签到新增'
          : undefined
      : todaySnapEarned != null
        ? '口径：余额差分+签到归并'
        : wbCheckin.length > 0
          ? '仅含签到新增'
          : undefined;
  // 今日消耗时效标注（对齐 CreditsTab stale Badge）：stale 缓存/快照差分推导均非当日数据，避免缓存值误读为今日
  const consumedHint = isTrae
    ? undefined
    : isQoder
      ? qoderTodaySnap?.consumed != null
        ? undefined
        : qoderTodaySnap
          ? '账号数变动 · 当日差分不可比'
          : qoderLastSnapDate
            ? `快照差分推导（截至 ${qoderLastSnapDate}）`
            : '暂无快照 · 待首次积分查询落库'
      : wbOfficial
        ? wbOfficial.stale
          ? '缓存回退 · 非当日数据'
          : undefined
        : wbFallback
          ? '快照差分推导（截至最后快照日）'
          : undefined;
  // 审查修复：有筛选时 KPI hint 追加口径标注（今日新增已按筛选取数；今日消耗为
  // 聚合口径无账号维度，不做静默截断，与工具行徽标语义一致）
  const earnedHintShown =
    !isTrae && filtersActive
      ? earnedHint
        ? `${earnedHint} · 已按账号筛选`
        : '已按账号筛选'
      : earnedHint;
  const consumedHintShown =
    !isTrae && filtersActive
      ? consumedHint
        ? `${consumedHint} · 聚合口径，未按账号筛选`
        : '聚合口径，未按账号筛选'
      : consumedHint;

  const scope: PlatformScope = platform;

  // ---- 第二层分类：数据源切换（业务面板外，页面工具行承载）----
  // gateway 禁用文案动态化（issue #82 T3.1）：Qoder 网关就绪（统一目录含 enabled qoder 源）
  // 时不禁用网关源，未就绪时给接入引导
  const activeSource = tab === 'credits' ? creditsSource : tokensSource;
  // 数据源禁用原因；本地源按平台可用性渲染（Qoder 本地快照为积分统计唯一消耗来源）
  const sourceDisabled: Partial<Record<BoardSource, string>> =
    tab === 'tokens'
      ? isQoder
        ? {
            local: 'Qoder 本地日志 token 字段恒为 0（官方模型计费在服务端，实测无可用值）',
            official: 'Qoder 官网未提供 token 用量接口',
            ...(qoderGatewayReady
              ? {}
              : { gateway: 'Qoder 网关未启用（在 API 服务页启用 Qoder 池并将客户端指向本地网关后点亮）' }),
          }
        : platform === 'trae'
          ? { local: '本地 Token 统计 = WB/CodeBuddy 客户端会话，Trae 无本地源' }
          : { official: 'Buddy 官网未提供按日 token 明细接口' }
      : isQoder
        ? {
            official: 'Qoder 官网未提供按日消耗明细接口',
            ...(qoderGatewayReady
              ? {}
              : { gateway: 'Qoder 网关未启用（在 API 服务页启用 Qoder 池并将客户端指向本地网关后点亮）' }),
          }
        : platform === 'trae'
          ? { local: '本地源 = WB 客户端会话快照差分，Trae 无本地源' }
          : {};
  const handleSourceChange = (s: BoardSource) => {
    if (tab === 'credits') setCreditsSource(s);
    else setTokensSource(s);
  };

  return (
    <div className="animate-fade-in">
      <PageHeader title={PAGE_META[platform].title} desc={PAGE_META[platform].desc} />

      {/* KPI 统计面板（7 卡，§2；单平台口径） */}
      <div className="mb-5">
        <KpiRow
          kpi={kpi}
          platform={platform}
          today={today}
          creditsHint={creditsHint}
          totalHint={traeTotalHint}
          earnedHint={earnedHintShown}
          consumedHint={consumedHintShown}
        />
      </div>

      {/* 第一层分类：Tab 切换（左）+ 刷新（右） */}
      <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
        <div className="flex rounded-lg bg-slate-100 p-1 dark:bg-zinc-900">
          {TABS.map((it) => (
            <button
              key={it.key}
              className={`rounded-md px-3.5 py-1.5 text-xs font-medium ${
                tab === it.key
                  ? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-800 dark:text-zinc-100'
                  : 'text-slate-500 dark:text-zinc-400'
              }`}
              onClick={() => setTab(it.key)}
            >
              {it.label}
            </button>
          ))}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {/* 顶部分组/账号筛选（P1 需求②）：平台视图内全局生效 */}
          <MultiSelectDropdown
            label="分组"
            onOpen={reloadPoolGroups}
            options={poolGroups.map((g) => ({ id: g.id, name: `${g.name} (${g.count})`, color: g.color }))}
            selected={groupFilter}
            onChange={(ids) => {
              setGroupFilter(ids);
              // 分组变化后账号候选联动收窄；已选账号若不在新候选内则清空账号筛选
              setAccountFilter((prev) => prev.filter((id) => {
                if (ids.length === 0) return true;
                return poolGroups.some((g) => ids.includes(g.id) && g.uids.includes(id));
              }));
            }}
          />
          <MultiSelectDropdown
            label="账号"
            options={accountCandidates}
            selected={accountFilter}
            onChange={setAccountFilter}
          />
          {filtersActive && tab !== 'expiry' && tab !== 'sessions' && !(platform === 'trae' && activeSource === 'official') && (
            <span
              className="inline-flex items-center rounded-full bg-amber-100 px-2.5 py-0.5 text-[11px] font-medium text-amber-700 dark:bg-amber-900/40 dark:text-amber-300"
              title="当前数据源为聚合口径（无账号维度），筛选仅作用于 KPI 与到期日历；消耗/Token 明细不做静默截断"
            >
              该数据源不支持账号级筛选
            </span>
          )}
          {/* 会话明细 Tab：顶部筛选仅收窄账号候选下拉、不进查询参数（后端 user_id 单值）——
              此前徽标豁免会话 Tab，用户误以为已按组过滤。明示语义断层（2026-10-12 审查） */}
          {filtersActive && tab === 'sessions' && (
            <span
              className="inline-flex items-center rounded-full bg-amber-100 px-2.5 py-0.5 text-[11px] font-medium text-amber-700 dark:bg-amber-900/40 dark:text-amber-300"
              title="顶部分组/账号筛选仅收窄会话页的账号候选下拉，不直接过滤查询结果；请使用会话页内的筛选行（时间/账号/模型）过滤"
            >
              顶部筛选仅收窄账号候选，会话结果按页内筛选过滤
            </span>
          )}
          <button className="btn-outline" onClick={() => void refresh(true)} disabled={loading}>
            <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 刷新数据
          </button>
        </div>
      </div>

      {/* 第二层分类：数据源切换（积分统计 / Token 统计 Tab；到期/会话明细 Tab 无数据源维度） */}
      {tab !== 'expiry' && tab !== 'sessions' && (
        <div className="mb-4 flex flex-wrap items-center gap-2">
          <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">数据源</span>
          <div className="flex rounded-lg bg-slate-100 p-1 dark:bg-zinc-900">
            {/* 三源全渲染，按平台经 sourceDisabled 禁用（Qoder 页 local 为唯一消耗来源） */}
            {SOURCES.map((s) => {
              const reason = sourceDisabled[s];
              return (
                <button
                  key={s}
                  disabled={!!reason}
                  title={reason ?? SOURCE_LABELS[s]}
                  className={`rounded-md px-3 py-1 text-xs font-medium transition ${
                    activeSource === s
                      ? 'bg-white text-zinc-900 shadow-sm dark:bg-zinc-800 dark:text-zinc-100'
                      : 'text-slate-500 dark:text-zinc-400'
                  } ${reason ? 'cursor-not-allowed opacity-40' : ''}`}
                  onClick={() => handleSourceChange(s)}
                >
                  {SOURCE_LABELS[s]}
                </button>
              );
            })}
          </div>
          <span className="hidden text-xs text-slate-400 lg:inline">
            各源覆盖窗口与口径不同，面板内明示
          </span>
        </div>
      )}

      {tab === 'expiry' ? (
        <ExpiryTab scope={scope} accounts={visibleAccounts} wbCredits={visibleWbCredits} qoderCredits={visibleQoderCredits} expiredEnts={expiredEnts} />
      ) : tab === 'sessions' ? (
        <SessionsTab scope={scope} accountOptions={sessionAccountOptions} refreshKey={sessionsRefreshKey} />
      ) : tab === 'credits' ? (
        <CreditsTab
          scope={scope}
          source={activeSource}
          usage={visibleUsage}
          creditsDaily={creditsDaily}
          wbOfficial={wbOfficial}
          wbFallback={wbFallback}
          wbSnapshots={wbSnapshots}
          wbCheckin={wbCheckin}
          qoderSnapshots={qoderSnapshots}
          qoderCheckin={qoderCheckin}
          gateway={gateway}
          uidSet={visibleUids}
        />
      ) : (
        <TokensTab
          scope={scope}
          source={activeSource}
          usage={visibleUsage}
          tokenStats={tokenStats}
          gateway={gateway}
        />
      )}
    </div>
  );
}
