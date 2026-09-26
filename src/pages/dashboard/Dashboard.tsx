import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { SOURCE_LABELS, SOURCES, type BoardSource } from '../../components/ChartFilterBar';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import { fmtCredits } from '../../lib/format';
import KpiRow, { type PlatformKpi, type PlatformScope } from './KpiRow';
import ExpiryTab from './ExpiryTab';
import CreditsTab from './CreditsTab';
import TokensTab, { type GatewayDays } from './TokensTab';
import type {
  UsageDayView,
  UsageHistoryResult,
  WbCheckinRecord,
  WbCreditsResult,
  WbCreditsSnapshot,
  WbTokenStats,
  WbUsageFallback,
  WbUsageOfficialAll,
} from '../../types';

/**
 * 积分看板（credits-dashboard-plan.md；平台拆分调整）：
 * 同一套看板组件按 platform 参数分别渲染 Trae / Buddy 两个独立页面——
 * Trae 页挂在 `credits` 视图，Buddy 页挂在 `buddy-credits` 视图，互不混装。
 * 布局：PageHeader（标题+概述）→ KPI 7 卡 → Tab 工具行（Tab 切换 + 刷新）→ Tab 内容。
 */

type BoardTab = 'credits' | 'tokens' | 'expiry';

const TABS: { key: BoardTab; label: string }[] = [
  { key: 'credits', label: '积分统计' },
  { key: 'tokens', label: 'Token 统计' },
  { key: 'expiry', label: '积分到期' },
];

/** 页面标题/概述（两页同一结构：积分余额 · 积分明细 · Token 统计 · 到期日历） */
const PAGE_META: Record<'trae' | 'buddy', { title: string; desc: string }> = {
  trae: { title: 'Trae · 积分看板', desc: '积分余额 · 积分明细 · Token 统计 · 到期日历' },
  buddy: { title: 'Buddy · 积分看板', desc: '积分余额 · 积分明细 · Token 统计 · 到期日历' },
};

function localDate(d: Date): string {
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

export default function CreditsDashboard({ platform }: { platform: 'trae' | 'buddy' }) {
  const pushToast = useAppStore((s) => s.pushToast);
  const accounts = useAppStore((s) => s.accounts);
  const creditsDaily = useAppStore((s) => s.creditsDaily);
  const creditsHistory = useAppStore((s) => s.creditsHistory);
  const refreshRemainingCredits = useAppStore((s) => s.refreshRemainingCredits);
  const refreshCreditsDaily = useAppStore((s) => s.refreshCreditsDaily);
  const refreshCreditsHistory = useAppStore((s) => s.refreshCreditsHistory);

  const isTrae = platform === 'trae';

  const [tab, setTab] = useState<BoardTab>('credits');
  const [loading, setLoading] = useState(false);

  // Trae 侧数据（官网源消耗明细 + store 快照/签到历史；Buddy 页不加载）
  const [usage, setUsage] = useState<UsageHistoryResult | null>(null);
  // Buddy 侧数据（Trae 页不加载）
  const [wbCredits, setWbCredits] = useState<WbCreditsResult | null>(null);
  // 签到日志取 90 天窗口：KPI 今日新增 + 积分统计 Tab「区间总获得」共用
  const [wbCheckin, setWbCheckin] = useState<WbCheckinRecord[]>([]);
  const [wbOfficial, setWbOfficial] = useState<WbUsageOfficialAll | null>(null);
  const [wbFallback, setWbFallback] = useState<WbUsageFallback | null>(null);
  // 快照时序（§2.2 方案 B：earned 已落库；null 日期回退方案 A 签到聚合）
  const [wbSnapshots, setWbSnapshots] = useState<WbCreditsSnapshot[]>([]);
  // 各统计 Tab 独立数据源状态（§3.4）；默认源按平台选择（Trae 无本地源 → 官网优先）
  const [creditsSource, setCreditsSource] = useState<BoardSource>('official');
  const [tokensSource, setTokensSource] = useState<BoardSource>(platform === 'trae' ? 'official' : 'local');

  // Token 统计 Tab 数据（懒加载：首次切到 Token Tab 才拉取，§3.4 lazy 触发）
  const [tokenStats, setTokenStats] = useState<WbTokenStats | null>(null);
  const [tokenStatsLoading, setTokenStatsLoading] = useState(false);
  const [gateway, setGateway] = useState<GatewayDays | null>(null);
  const [gatewayLoading, setGatewayLoading] = useState(false);
  const tokensLoadedRef = useRef(false);
  // 网关数据触发点有两个：Token 统计 Tab 与 积分统计 Tab 的网关源（审查修复）
  const gatewayLoadedRef = useRef(false);

  const loadUsage = useCallback(
    async (fresh: boolean) => {
      try {
        setUsage(await api.accounts.usageHistory(fresh));
      } catch (err) {
        pushToast('error', `Trae 消耗明细查询失败：${String(err)}`);
      }
    },
    [pushToast],
  );

  const loadBuddy = useCallback(
    async (fresh: boolean) => {
      await Promise.allSettled([
        api.workbuddy
          .creditsFetch(undefined, fresh)
          .then((r) => {
            setWbCredits(r);
            if (fresh && r.stale) {
              pushToast('warn', 'Buddy 积分刷新失败，已回退展示历史缓存数据');
            }
          })
          .catch((err) => pushToast('error', `Buddy 积分查询失败：${String(err)}`)),
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
    },
    [pushToast],
  );

  // ---- Token 统计 Tab 数据源（§7 缓存策略：本地 token 结果级 10min；网关 SQLite 直查）----
  const loadTokens = useCallback(
    async (fresh: boolean) => {
      setTokenStatsLoading(true);
      try {
        // fresh=false 走后端 10 分钟结果缓存；「刷新数据」传 true 强制重扫
        setTokenStats(await withMinDelay(api.workbuddy.tokenStats(fresh), 800));
      } catch (err) {
        pushToast('error', `本地 Token 统计失败：${String(err)}`);
      } finally {
        setTokenStatsLoading(false);
      }
    },
    [pushToast],
  );

  const loadGateway = useCallback(async () => {
    setGatewayLoading(true);
    try {
      // 看板按单平台展示，仅拉取 Trae/Buddy 两池（custom 池仅 API 服务页 UsageStatsPanel 使用）
      const [trae, buddy] = await Promise.all([
        api.apiServer.usageStats(90).catch(() => [] as UsageDayView[]),
        api.apiServer.wbUsageStats(90).catch(() => [] as UsageDayView[]),
      ]);
      setGateway({ trae, buddy });
    } finally {
      setGatewayLoading(false);
    }
  }, []);

  // 懒加载（§3.4）：本地 token 仅 Buddy 页拉取（Trae 页本地源禁用，tokenStats 为
  // WB/CodeBuddy 会话扫描的重操作，Trae 页零消费不触发）；网关数据在 Token Tab
  // 或 积分统计 Tab 网关源首次激活时拉取（SQLite 直查，轻）
  useEffect(() => {
    if (platform === 'buddy' && tab === 'tokens' && !tokensLoadedRef.current) {
      tokensLoadedRef.current = true;
      void loadTokens(false);
    }
    const needGateway = tab === 'tokens' || (tab === 'credits' && creditsSource === 'gateway');
    if (needGateway && !gatewayLoadedRef.current) {
      gatewayLoadedRef.current = true;
      void loadGateway();
    }
  }, [platform, tab, creditsSource, loadTokens, loadGateway]);

  const refresh = useCallback(
    async (fresh = false) => {
      setLoading(true);
      try {
        await withMinDelay(
          (async () => {
            // Trae fresh 时先串行刷新剩余积分：后端逐账号网络查询并落盘 credits_daily.json 快照
            // （旧页即串行保证此顺序），完成后再并行读取；否则日快照竞速先返回旧值，今日 KPI 滞后
            if (isTrae && fresh) await refreshRemainingCredits();
            await Promise.all([
              // 按平台只加载本侧数据源
              isTrae ? refreshCreditsDaily() : Promise.resolve(),
              isTrae ? refreshCreditsHistory() : Promise.resolve(),
              isTrae ? loadUsage(fresh) : Promise.resolve(),
              isTrae ? Promise.resolve() : loadBuddy(fresh),
              // 已加载过的源随全局刷新联动（fresh=true 本地重扫 + 网关直查）
              tokensLoadedRef.current && fresh ? loadTokens(true) : Promise.resolve(),
              gatewayLoadedRef.current && fresh ? loadGateway() : Promise.resolve(),
            ]);
          })(),
          600,
        );
        if (fresh) {
          pushToast('success', '看板数据已刷新');
        }
      } finally {
        setLoading(false);
      }
    },
    [isTrae, refreshRemainingCredits, refreshCreditsDaily, refreshCreditsHistory, loadUsage, loadBuddy, loadTokens, loadGateway, pushToast],
  );

  useEffect(() => {
    void refresh(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const today = localDate(new Date());

  // ---- Trae KPI（口径对齐原积分看板）----
  const traeKpi = useMemo<PlatformKpi>(() => {
    const nowSec = Date.now() / 1000;
    const horizon = nowSec + 7 * 86400;
    let totalCredits = 0;
    let packages = 0;
    let expiring = 0;
    for (const a of accounts) {
      totalCredits += a.remaining_credits ?? 0;
      // 积分包总数：积分包 + 会员包计数（expiryItems 口径）
      if (a.credits_expire_at != null) packages += 1;
      if (a.membership_expire != null) packages += 1;
      // 7 天内到期：按剩余积分额度合计（非包数）；下限 now 排除已过期包
      if (a.credits_expire_at != null && a.credits_expire_at > nowSec && a.credits_expire_at <= horizon) {
        expiring += a.remaining_credits ?? 0;
      }
    }
    // 今日新增：快照 earned 优先（积分包 CycleStartTime 归日口径），回退签到 history delta
    const snap = creditsDaily.find((s) => s.date === today);
    const histDelta = creditsHistory
      .filter((r) => r.date === today)
      .reduce((s, r) => s + (r.delta || 0), 0);
    const todayEarned = snap && snap.earned > 0 ? snap.earned : Math.max(0, histDelta);
    // 今日消耗：官网接口明细优先（credits_float 实际口径），回退快照 consumed
    const usageVal =
      usage?.accounts.reduce(
        (s, a) => s + (a.daily.find((d) => d.date === today)?.credits ?? 0),
        0,
      ) ?? 0;
    const todayConsumed = usage != null && usageVal > 0 ? usageVal : (snap?.consumed ?? 0);
    return {
      accounts: accounts.length,
      totalCredits,
      packages,
      todayEarned,
      todayConsumed,
      expiring7d: expiring,
    };
  }, [accounts, creditsDaily, creditsHistory, usage, today]);

  // Trae「可用积分总数」卡细分（对齐原积分看板 totalHint）；账号无通用/Work 字段时回退通用文案
  const traeTotalHint = useMemo(() => {
    if (!isTrae) return undefined;
    if (!accounts.some((a) => a.general_credits != null || a.work_credits != null)) {
      return '总剩余可用积分';
    }
    const generalTotal = accounts.reduce((s, a) => s + (a.general_credits ?? 0), 0);
    const workTotal = accounts.reduce((s, a) => s + (a.work_credits ?? 0), 0);
    return `通用 ${fmtCredits(generalTotal)} 积分 · Work ${fmtCredits(workTotal)} 积分`;
  }, [isTrae, accounts]);

  // ---- Buddy KPI（§2.1/§2.2）----
  const buddyKpi = useMemo<PlatformKpi>(() => {
    const nowSec = Date.now() / 1000;
    const horizon = nowSec + 7 * 86400;
    const accs = wbCredits?.accounts ?? [];
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
    // 今日新增：方案 B 快照 earned（余额差分+签到归并）优先，回退方案 A 签到 reward 聚合
    const snapEarned = wbSnapshots.find((s) => s.date === today)?.earned;
    const checkinEarned = wbCheckin
      .filter((r) => r.date === today)
      .reduce((s, r) => s + (r.reward ?? 0), 0);
    const todayEarned = snapEarned != null ? Math.max(0, snapEarned) : checkinEarned;
    // 今日消耗：官方用量优先（31 天窗口），回退快照差分
    const todayConsumed =
      wbOfficial?.summary.usage_today ?? wbFallback?.summary.usage_today ?? 0;
    return {
      accounts: accs.length,
      totalCredits,
      packages,
      todayEarned,
      todayConsumed,
      expiring7d: expiring,
    };
  }, [wbCredits, wbCheckin, wbSnapshots, wbOfficial, wbFallback, today]);

  const kpi = isTrae ? traeKpi : buddyKpi;

  // 可用积分总数卡 hint：Buddy 缓存/stale 状态说明；无数据时不给失真提示
  const creditsHint = isTrae
    ? undefined
    : !wbCredits
      ? undefined
      : wbCredits.stale
        ? '历史缓存回退（本次查询失败）'
        : wbCredits.cached
          ? '缓存数据（≥10 分钟）'
          : '实时数据';
  // 今日新增口径诚实标注（§8.4）：快照 earned（方案 B）已统计 vs 回退签到口径（方案 A）
  const todaySnapEarned = wbSnapshots.find((s) => s.date === today)?.earned;
  const earnedHint = isTrae
    ? undefined
    : todaySnapEarned != null
      ? '口径：余额差分+签到归并'
      : wbCheckin.length > 0
        ? '仅含签到新增'
        : undefined;
  // 今日消耗时效标注（对齐 CreditsTab stale Badge）：stale 缓存/快照差分推导均非当日数据，避免缓存值误读为今日
  const consumedHint = isTrae
    ? undefined
    : wbOfficial
      ? wbOfficial.stale
        ? '缓存回退 · 非当日数据'
        : undefined
      : wbFallback
        ? '快照差分推导（截至最后快照日）'
        : undefined;

  const scope: PlatformScope = platform;

  // ---- 第二层分类：数据源切换（业务面板外，页面工具行承载）----
  const activeSource = tab === 'credits' ? creditsSource : tokensSource;
  const sourceDisabled: Partial<Record<BoardSource, string>> =
    tab === 'tokens'
      ? platform === 'trae'
        ? { local: '本地 Token 统计 = WB/CodeBuddy 客户端会话，Trae 无本地源' }
        : { official: 'Buddy 官网未提供按日 token 明细接口' }
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
          earnedHint={earnedHint}
          consumedHint={consumedHint}
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
        <button className="btn-outline" onClick={() => void refresh(true)} disabled={loading}>
          <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 刷新数据
        </button>
      </div>

      {/* 第二层分类：数据源切换（积分统计 / Token 统计 Tab；到期 Tab 无数据源维度） */}
      {tab !== 'expiry' && (
        <div className="mb-4 flex flex-wrap items-center gap-2">
          <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">数据源</span>
          <div className="flex rounded-lg bg-slate-100 p-1 dark:bg-zinc-900">
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
        <ExpiryTab scope={scope} accounts={accounts} wbCredits={wbCredits} />
      ) : tab === 'credits' ? (
        <CreditsTab
          scope={scope}
          source={activeSource}
          usage={usage}
          creditsDaily={creditsDaily}
          wbOfficial={wbOfficial}
          wbFallback={wbFallback}
          wbSnapshots={wbSnapshots}
          wbCheckin={wbCheckin}
          gateway={gateway}
        />
      ) : (
        <TokensTab
          scope={scope}
          source={activeSource}
          usage={usage}
          tokenStats={tokenStats}
          gateway={gateway}
        />
      )}
    </div>
  );
}
