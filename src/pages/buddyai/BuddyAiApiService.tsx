import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw, Globe, Activity, Save, Gauge, Check, X } from 'lucide-react';
import { Badge, Spinner, StatCard } from '../../components/ui';
import PageHeader from '../../components/PageHeader';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type { ApiPoolFile, BuddyAiAccountView, GroupView, TagView, WbModelInfo } from '../../types';

/**
 * BuddyAI · 资源调度页（布局对齐国内 Buddy「资源调度」）。
 *
 * 琥珀积分体系说明横幅（含 buddy-ai 池可用积分总余额；未启用国际档时附一键开启）+
 * 池指标三卡（健康账号/池内账号/可用总积分）+ 主体左右分栏（lg:grid-cols-12 各占 6）：
 * 左 = 账号池与调度参数合并面板（账号池选择：已选计数 + 全选/清空 + 右上统一「保存」；
 * 其下资源开关与调度参数节），右 = 同步官网模型卡（同步按钮 + 每日定时同步行 +
 * 模型目录表格）。API 网关启停收敛在顶栏（与其他应用同构）。
 */
/** 模型名系列推断厂商（统一目录无记录时的兜底，对齐国内「按模型名系列推断」口径；
 *  用户实测：gemini-* 恒为 Google、gpt/chatgpt 恒为 ChatGPT、deepseek-* 恒为 DeepSeek） */
function inferVendor(id: string): string | null {
  const s = id.toLowerCase();
  if (s.startsWith('gemini')) return 'Google';
  if (s.startsWith('gpt') || s.startsWith('chatgpt') || /^(o[134](-|$))/.test(s)) return 'ChatGPT';
  if (s.startsWith('deepseek')) return 'DeepSeek';
  if (s.startsWith('claude')) return 'Anthropic';
  if (s.startsWith('qwen')) return '阿里云';
  if (s.startsWith('glm')) return '智谱';
  if (s.startsWith('grok')) return 'xAI';
  if (s.startsWith('mistral') || s.startsWith('magistral') || s.startsWith('codestral')) return 'Mistral';
  if (s.startsWith('kimi') || s.startsWith('moonshot')) return '月之暗面';
  if (s.startsWith('doubao') || s.startsWith('seed')) return '字节跳动';
  if (s.startsWith('llama')) return 'Meta';
  if (s.startsWith('ernie')) return '百度';
  if (s.startsWith('hunyuan')) return '腾讯';
  if (s.startsWith('minimax') || s.startsWith('abab')) return 'MiniMax';
  return null;
}

export default function BuddyAiApiService() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [accounts, setAccounts] = useState<BuddyAiAccountView[]>([]);
  const [catalog, setCatalog] = useState<WbModelInfo[]>([]);
  const [catalogSyncing, setCatalogSyncing] = useState(false);
  // 厂商列数据源（对齐国内 §6.2）：WbModelInfo 无 vendor 字段，经统一目录
  // 聚合按模型 id 关联；id 缺失/失败 → 显示 —，不影响主列表
  const [vendorMap, setVendorMap] = useState<Map<string, string>>(new Map());
  // 分组/标签筛选（对齐国内：持久化取号范围，空 = 不限；组 ∩ 标签 D5 交集）
  const [groups, setGroups] = useState<GroupView[]>([]);
  const [tagDefs, setTagDefs] = useState<TagView[]>([]);
  const [poolGroupScope, setPoolGroupScope] = useState<string[]>([]);
  const [poolTagScope, setPoolTagScope] = useState<string[]>([]);
  // 调度参数（per-pool 三参数：并发上限 / 会话粘性 TTL / 对冲阈值，随 onBlur 保存）
  const [aiConcurrency, setAiConcurrency] = useState(1);
  const [aiSessionTtl, setAiSessionTtl] = useState(1800);
  const [aiHedgeMs, setAiHedgeMs] = useState(8000);
  const [flags, setFlags] = useState<{
    buddyAiEnabled: boolean;
    buddyAiStickyEnabled: boolean;
    buddyAiPoolStickyTtlSecs?: number;
  }>({ buddyAiEnabled: false, buddyAiStickyEnabled: false });
  const [poolTtl, setPoolTtl] = useState(0);
  // 每日定时同步目录（存 app Settings，即改即存）
  const [catalogTask, setCatalogTask] = useState({ enabled: true, hhmm: '05:55' });
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [poolSaving, setPoolSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  // 账号池本地编辑态（desired enabled；保存时统一提交 diff，对齐国内「右上角保存」模式）
  const [pendingEnabled, setPendingEnabled] = useState<Record<string, boolean>>({});
  // 调度参数最近一次落库值（审查修复 P3 同源：onBlur 未改动不重复保存——原实现每次
  // 失焦都触发 poolSet + 「已保存」toast，用户仅点击路过输入框也会弹保存噪音）
  const savedParamsRef = useRef<Record<string, number>>({});
  // 伪流式灰度（issue #213）：仅内存热切换、重启复位默认关；
  // null = 服务未运行/读取失败（开关置灰、输入禁用），与指纹清洗开关同口径
  const [pseudoEnabled, setPseudoEnabled] = useState<boolean | null>(null);
  // 池级换号重试预算（0 = 旧行为：重试机会 = 池内账号数）
  const [pseudoBudget, setPseudoBudget] = useState<number | null>(null);
  // 伪流式等待期 keep-alive 间隔秒数（全局配置，两档共用；0 = 关闭，0–60）
  const [pseudoKeepalive, setPseudoKeepalive] = useState<number | null>(null);
  // 伪流式数值参数最近落库值（审查修复：onBlur 与「最近落库值」比较而非 state——
  // 清空输入（Number("")=0）或改后回退原值时 state 已等于 clamped，按 state 判断会
  // 漏保存导致前后端分叉；保存成功才记入 ref，与 savedParamsRef 同范式）
  const savedPseudoRef = useRef<{ budget: number | null; keepalive: number | null }>({
    budget: null,
    keepalive: null,
  });
  // 保存请求序号守卫（审查 #6，与 ResourceSummary F2 同类竞态）：连续保存时旧请求
  // 慢败返回，陈旧快照回滚会覆盖其后已成功保存的字段并可能经下次 onBlur 写回后端
  const saveSeqRef = useRef(0);

  const refreshCatalog = useCallback(async () => {
    const [cat, unified] = await Promise.all([
      api.apiServer.buddyAiCatalogList().catch(() => [] as WbModelInfo[]),
      api.apiServer.unifiedModels().catch(() => []),
    ]);
    setCatalog(cat);
    // 新增模型需重取统一目录关联 vendor（否则新行厂商列恒 —）
    setVendorMap(new Map(unified.filter((m) => m.vendor).map((m) => [m.id, m.vendor])));
  }, []);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const [pf, accs, cat, unified, gs, tags] = await Promise.all([
        api.apiServer.poolList().catch((e) => {
          // FM-1 审查修复：池配置加载失败必须可见（原静默吞掉 → 横幅显示「未启用」
          // 假态，点一键开启又报池未加载，用户无从分辨）
          pushToast('error', `池配置加载失败：${String(e).slice(0, 120)}`);
          return null;
        }),
        api.buddyai.accountsList().catch((e) => {
          pushToast('error', `账号列表加载失败：${String(e).slice(0, 120)}`);
          return [] as BuddyAiAccountView[];
        }),
        api.apiServer.buddyAiCatalogList().catch(() => [] as WbModelInfo[]),
        api.apiServer.unifiedModels().catch(() => []),
        api.groups.sharedList().catch(() => [] as GroupView[]),
        api.tags.list().catch(() => [] as TagView[]),
      ]);
      setPool(pf);
      setAccounts(accs);
      setCatalog(cat);
      setVendorMap(new Map(unified.filter((m) => m.vendor).map((m) => [m.id, m.vendor])));
      setGroups(gs);
      setTagDefs(tags);
      setPendingEnabled({});
      if (pf) {
        setFlags({
          buddyAiEnabled: pf.buddy_ai_enabled ?? false,
          buddyAiStickyEnabled: pf.buddy_ai_sticky_enabled ?? false,
        });
        setPoolTtl(pf.buddy_ai_pool_sticky_ttl_secs ?? 0);
        setPoolGroupScope(pf.buddy_ai_group_ids ?? []);
        setPoolTagScope(pf.buddy_ai_tag_ids ?? []);
        setAiConcurrency(pf.buddy_ai_account_concurrency_limit ?? 1);
        setAiSessionTtl(pf.buddy_ai_sticky_ttl_secs ?? 1800);
        setAiHedgeMs(pf.buddy_ai_hedge_threshold_ms ?? 8000);
        savedParamsRef.current = {
          concurrency: pf.buddy_ai_account_concurrency_limit ?? 1,
          poolTtl: pf.buddy_ai_pool_sticky_ttl_secs ?? 0,
          sessionTtl: pf.buddy_ai_sticky_ttl_secs ?? 1800,
          hedge: pf.buddy_ai_hedge_threshold_ms ?? 8000,
        };
      }
    } finally {
      setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
    api.misc
      .settingsGet()
      .then((st) => {
        setCatalogTask({
          enabled: st.buddy_ai_catalog_sync_enabled ?? true,
          hhmm: st.buddy_ai_catalog_sync_hhmm || '05:55',
        });
      })
      .catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refresh]);

  // 伪流式灰度回显（issue #213）：失败置 null（开关置灰按默认关展示），启动后点「刷新」可恢复
  const loadPseudo = useCallback(async () => {
    try {
      const [sw, budgets, ka] = await Promise.all([
        api.apiServer.pseudoStreamSwitchesGet(),
        api.apiServer.retryBudgetsGet(),
        api.apiServer.pseudoKeepaliveGet(),
      ]);
      setPseudoEnabled(sw.buddy_ai);
      setPseudoBudget(budgets.buddy_ai);
      setPseudoKeepalive(ka);
      savedPseudoRef.current = { budget: budgets.buddy_ai, keepalive: ka };
    } catch {
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
      await api.apiServer.pseudoStreamSwitchSet('buddy_ai', enabled);
      pushToast(
        'success',
        `已${enabled ? '开启' : '关闭'}buddy-ai 池伪流式，下一条请求即生效`,
      );
    } catch (err) {
      // 条件回滚（审查修复：快速连点+失败时陈旧 prev 会覆盖新状态——仅当 state
      // 仍等于本次乐观值才回滚，其后已成功的切换不被乱序旧失败覆盖）
      setPseudoEnabled((cur) => (cur === enabled ? prev : cur));
      pushToast('error', `切换伪流式开关失败：${String(err)}`);
    }
  };

  /** 伪流式数值参数（重试预算 / keep-alive 秒）保存：后端 clamp 归一，热切换即生效 */
  const savePseudoParam = async (kind: 'budget' | 'keepalive', value: number) => {
    try {
      if (kind === 'budget') {
        await api.apiServer.retryBudgetSet('buddy_ai', value);
        savedPseudoRef.current.budget = value;
        pushToast('success', `已保存 buddy-ai 池重试预算（${value}，下一条请求即生效）`);
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

  const saveFlags = async (next: {
    buddyAiEnabled: boolean;
    buddyAiStickyEnabled: boolean;
    buddyAiPoolStickyTtlSecs?: number;
    buddyAiAccountConcurrencyLimit?: number;
    buddyAiStickyTtlSecs?: number;
    buddyAiHedgeThresholdMs?: number;
    buddyAiGroupIds?: string[];
    buddyAiTagIds?: string[];
  }) => {
    if (!pool) {
      pushToast('error', '池配置未加载，无法保存（请先刷新重试）');
      return false;
    }
    // 序号守卫（F2 同范式）：仅最新一次请求允许回滚/saving 复位，乱序旧失败不得
    // 用陈旧快照覆盖其后已成功保存的字段
    const seq = ++saveSeqRef.current;
    setSaving(true);
    // 乐观更新（审查 P2：原在 await 之后 setFlags，保存进行中（≥400ms）再点
    // 另一个开关会基于旧 flags 快照提交，把第一次改动静默回退）
    const prev = flags;
    setFlags(next);
    try {
      await withMinDelay(
        api.apiServer.poolSet(pool.enabled_uids, pool.strategy, pool.group_ids, next),
        400,
      );
      pushToast(
        'success',
        next.buddyAiEnabled ? 'buddy-ai 国际档已开启（独立池参与调度）' : '已保存 buddy-ai 调度配置',
      );
      return true;
    } catch (err) {
      if (seq === saveSeqRef.current) {
        // 按本次 patch 字段函数式回滚（审查修复）：不用陈旧快照整份覆盖，基于最新
        // state 仅回滚本次 patch 键，避免连带抹掉并发已成功落库的其他字段
        setFlags((cur) => {
          const revert = { ...cur };
          for (const k of Object.keys(next) as Array<keyof typeof next>) {
            (revert as Record<string, unknown>)[k] = prev[k as keyof typeof prev];
          }
          return revert;
        });
        pushToast('error', `保存失败：${String(err).slice(0, 160)}`);
      } else {
        // 复审 No.2：本次失败已被更新的保存取代，静默不上报（后续保存成功以全量
        // payload 提交会自动收敛，弹 toast 反成虚假提示）；仅补诊断日志便于排查
        console.warn('[BuddyAiApiService] 保存请求已被更新的保存取代，失败已静默：', err);
      }
      return false;
    } finally {
      if (seq === saveSeqRef.current) setSaving(false);
    }
  };

  /** 账号池统一保存：对本地编辑态与当前状态的 diff 逐个提交启停 */
  const savePool = async () => {
    const diffs = accounts.filter(
      (a) => pendingEnabled[a.id] !== undefined && pendingEnabled[a.id] === a.disabled,
    );
    if (diffs.length === 0) {
      pushToast('info', '账号池无改动');
      return;
    }
    setPoolSaving(true);
    // 逐项容错（FH-4 同源，对齐账号管理页批量移除）：单项失败不中断其余项，
    // 结束后统一刷新回滚显示 + 失败清单提示（审查残留项修复：原中途失败即中断，
    // 用户无法知道哪些账号已生效）
    const failed: string[] = [];
    for (const a of diffs) {
      try {
        await withMinDelay(api.buddyai.accountToggle(a.id, !pendingEnabled[a.id]), 200);
      } catch (err) {
        failed.push(`${a.nickname || a.id}（${String(err).slice(0, 80)}）`);
      }
    }
    if (failed.length === 0) {
      pushToast(
        'success',
        `账号池已保存（启用 ${diffs.filter((a) => pendingEnabled[a.id]).length} / 停用 ${diffs.filter((a) => !pendingEnabled[a.id]).length}）`,
      );
    } else {
      pushToast(
        'error',
        `账号池部分保存失败（成功 ${diffs.length - failed.length}/${diffs.length}）：${failed.join('；')}`,
      );
    }
    try {
      await refresh();
    } finally {
      setPoolSaving(false);
    }
  };

  /** 每日定时同步目录：开关/时刻即改即存（与国内「每日定时同步行」同范式） */
  const saveCatalogTask = async (next: { enabled: boolean; hhmm: string }) => {
    setCatalogTask(next);
    try {
      await api.misc.settingsSet({
        buddy_ai_catalog_sync_enabled: next.enabled,
        buddy_ai_catalog_sync_hhmm: next.hhmm || '05:55',
      });
      await useAppStore.getState().refreshSettings().catch(() => {
        /* 全局设置快照刷新失败不阻塞本地保存结果，下次刷新自愈 */
      });
      // 审查修复（L27）：保存成功补反馈，文案含触发时刻
      pushToast(
        'success',
        next.enabled ? `每日定时同步已保存（每天 ${next.hhmm || '05:55'} 触发）` : '每日定时同步已关闭',
      );
    } catch (err) {
      pushToast('error', `保存定时同步配置失败：${String(err).slice(0, 120)}`);
    }
  };

  const refreshAll = async () => {
    setRefreshing(true);
    try {
      await refresh();
      await refreshCatalog();
    } finally {
      setRefreshing(false);
    }
  };

  const enabled = (a: BuddyAiAccountView) => pendingEnabled[a.id] ?? !a.disabled;
  const selCount = accounts.filter(enabled).length;
  // 分组/标签取号范围（与后端装配语义一致：组按 group_id、标签按 tag→uids，D5 交集；
  // 均 = 不限）
  const tagUidSets = useMemo(
    () => tagDefs.map((t) => ({ id: t.id, uids: new Set(t.uids ?? []) })),
    [tagDefs],
  );
  const inPoolScope = (a: BuddyAiAccountView) =>
    (poolGroupScope.length === 0 ||
      (a.group_id != null && poolGroupScope.includes(a.group_id))) &&
    (poolTagScope.length === 0 || tagUidSets.some((t) => poolTagScope.includes(t.id) && t.uids.has(a.id)));
  const dispatchable = accounts.filter((a) => !a.disabled && a.has_credential && !a.needs_relogin);
  const totalCredits = accounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0);
  const withCredits = accounts.filter((a) => a.credits_balance != null).length;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="BuddyAI · 资源调度"
        desc="服务资源池管理 · 上游开关与模型目录 · 资源提供给API网关使用"
        actions={
          <button
            className="btn-outline inline-flex items-center gap-1.5"
            onClick={() => void refreshAll()}
            disabled={loading || refreshing}
          >
            {loading || refreshing ? <Spinner /> : <RefreshCw size={14} />} 刷新
          </button>
        }
      />

      {/* 积分体系说明（对齐国内琥珀横幅：标题+标签+说明行+右余额；未启用国际档时附一键开启） */}
      <div className="rounded-xl border border-amber-300/70 bg-amber-50/80 px-3.5 py-2.5 dark:border-amber-700/40 dark:bg-amber-900/10">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-semibold text-amber-800 dark:text-amber-200">积分体系说明</span>
          <span className="rounded bg-amber-200 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-700/50 dark:text-amber-100">
            本服务消耗 buddy-ai 积分
          </span>
          {!flags.buddyAiEnabled && (
            <button
              className="btn-outline ml-auto shrink-0 !px-2 !py-0.5 text-xs"
              disabled={saving}
              onClick={() => void saveFlags({ ...flags, buddyAiEnabled: true })}
            >
              一键开启国际档
            </button>
          )}
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-slate-500 dark:text-zinc-400">
          <span>
            buddy-ai 积分：打卡 / 套餐获取，各模型按目录积分倍率消耗（免费模型 x0.00 不扣分）；国际档请求由
            buddy-ai 独立池服务，轮换消耗各账号积分（粘性会话 / 模型级冷却同现有实现）。
          </span>
          <span className="ml-auto">
            含凭证账号积分总余额：
            <span className="font-bold tabular-nums text-amber-700 dark:text-amber-300">
              {withCredits > 0
                ? totalCredits.toLocaleString('zh-CN', { maximumFractionDigits: 2 })
                : '未知'}
            </span>
            <span className="ml-1 opacity-75">（{withCredits}/{accounts.length} 个已取数）</span>
          </span>
        </div>
      </div>

      {/* 池指标三卡（对齐国内 grid-cols-3） */}
      <div className="mt-4 grid grid-cols-3 gap-3">
        <StatCard
          label="健康账号"
          value={dispatchable.length}
          hint="含凭证且无需重新登录，可参与调度"
          tone="green"
        />
        <StatCard
          label="池内账号"
          value={accounts.length}
          hint={`需重登 ${accounts.filter((a) => a.needs_relogin).length} · 未勾选 ${accounts.filter((a) => !enabled(a)).length}`}
          tone="violet"
        />
        <StatCard
          label="可用总积分"
          value={withCredits > 0 ? totalCredits.toFixed(2) : '—'}
          hint="积分同步后展示（积分看板）"
          tone="amber"
        />
      </div>

      {/* 主体左右分栏（对齐国内 grid-cols-12 各占 6） */}
      <div className="mt-4 grid grid-cols-1 items-start gap-4 lg:grid-cols-12">
        {/* 左列：账号池与调度参数合并面板（统一「保存」） */}
        <div className="card lg:col-span-6">
          <div className="border-b border-slate-100 px-4 py-3 dark:border-zinc-800">
            <div className="mb-3 flex items-center justify-between gap-2">
              <div className="flex items-center gap-2">
                <Activity size={16} className="text-brand-500" />
                <span className="text-sm font-medium">账号池选择</span>
                <span className="text-xs text-slate-400">已选 {selCount}/{accounts.length}</span>
              </div>
              <div className="flex items-center gap-1">
                {accounts.length > 0 && (
                  <>
                    <button
                      className="btn-ghost px-2 py-0.5 text-xs"
                      onClick={() => setPendingEnabled(Object.fromEntries(accounts.map((a) => [a.id, true])))}
                    >
                      全选
                    </button>
                    <button
                      className="btn-ghost px-2 py-0.5 text-xs"
                      onClick={() => setPendingEnabled(Object.fromEntries(accounts.map((a) => [a.id, false])))}
                    >
                      清空
                    </button>
                  </>
                )}
                <button className="btn-outline" onClick={() => void savePool()} disabled={poolSaving}>
                  {poolSaving ? <Spinner /> : <Save size={15} />} 保存
                </button>
              </div>
            </div>
            <p className="text-xs text-slate-400">
              {accounts.length === 0
                ? '暂无池内账号'
                : '勾选账号参与 buddy-ai 国际档调度（清空 = 全部退出调度）；改动随右上角「保存」统一生效'}
            </p>
            {/* 分组/标签筛选（对齐国内：持久化取号范围，随保存即时生效；组 ∩ 标签 D5 交集） */}
            {accounts.length > 0 && (
              <>
                <div className="mt-2 flex flex-wrap items-center gap-1.5">
                  <span className="shrink-0 text-[11px] text-slate-400">分组筛选</span>
                  {groups.map((g) => {
                    const active = poolGroupScope.includes(g.id);
                    return (
                      <button
                        key={g.id}
                        className={`rounded-full border px-2 py-0.5 text-[11px] transition ${
                          active
                            ? 'border-brand-400 bg-brand-500/10 font-medium text-brand-600 dark:text-brand-400'
                            : 'border-slate-200 text-slate-500 hover:bg-slate-50 dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800'
                        }`}
                        onClick={() => {
                          const next = active
                            ? poolGroupScope.filter((x) => x !== g.id)
                            : [...poolGroupScope, g.id];
                          // 审查修复（L28）：调用前捕获快照，保存失败按 patch 回滚乐观高亮
                          const prev = poolGroupScope;
                          setPoolGroupScope(next);
                          void saveFlags({ ...flags, buddyAiGroupIds: next }).then((ok) => {
                            if (!ok) setPoolGroupScope(prev);
                          });
                        }}
                      >
                        {g.name}
                      </button>
                    );
                  })}
                  {poolGroupScope.length > 0 && (
                    <button
                      className="btn-ghost px-1.5 py-0.5 text-[11px] text-slate-400"
                      onClick={() => {
                        // 审查修复（L28）：调用前捕获快照，保存失败按 patch 回滚乐观高亮
                        const prev = poolGroupScope;
                        setPoolGroupScope([]);
                        void saveFlags({ ...flags, buddyAiGroupIds: [] }).then((ok) => {
                          if (!ok) setPoolGroupScope(prev);
                        });
                      }}
                    >
                      清空
                    </button>
                  )}
                </div>
                <p className="mt-1 text-[11px] text-slate-400">
                  分组筛选作用于 buddy-ai 池取号范围，保存后即时生效；不选分组 = 全部参与（分组在「账号管理 → 分组管理」维护）。
                </p>
                {(tagDefs.length > 0 || poolTagScope.length > 0) && (
                  <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
                    <span className="shrink-0 text-[11px] text-slate-400">标签筛选</span>
                    {tagDefs.map((t) => {
                      const active = poolTagScope.includes(t.id);
                      return (
                        <button
                          key={t.id}
                          className={`rounded-full border px-2 py-0.5 text-[11px] transition ${
                            active
                              ? 'border-sky-400 bg-sky-500/10 font-medium text-sky-600 dark:text-sky-400'
                              : 'border-slate-200 text-slate-500 hover:bg-slate-50 dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800'
                          }`}
                          onClick={() => {
                            const next = active
                              ? poolTagScope.filter((x) => x !== t.id)
                              : [...poolTagScope, t.id];
                            // 审查修复（L28）：调用前捕获快照，保存失败按 patch 回滚乐观高亮
                            const prev = poolTagScope;
                            setPoolTagScope(next);
                            void saveFlags({ ...flags, buddyAiTagIds: next }).then((ok) => {
                              if (!ok) setPoolTagScope(prev);
                            });
                          }}
                        >
                          {t.name}
                        </button>
                      );
                    })}
                    {poolTagScope.length > 0 && (
                      <button
                        className="btn-ghost px-1.5 py-0.5 text-[11px] text-slate-400"
                        onClick={() => {
                        // 审查修复（L28）：调用前捕获快照，保存失败按 patch 回滚乐观高亮
                        const prev = poolTagScope;
                        setPoolTagScope([]);
                        void saveFlags({ ...flags, buddyAiTagIds: [] }).then((ok) => {
                          if (!ok) setPoolTagScope(prev);
                        });
                      }}
                      >
                        清空
                      </button>
                    )}
                  </div>
                )}
              </>
            )}
          </div>

          {/* 账号池选择（勾选行 + 健康徽标 + 积分，对齐国内；不再显示启用/停用徽标） */}
          <div className="max-h-72 overflow-y-auto">
            {loading ? (
              <p className="py-8 text-center text-xs text-slate-400">加载中…</p>
            ) : accounts.length === 0 ? (
              <p className="py-8 text-center text-xs text-slate-400">暂无账号，先到「账号管理」导入。</p>
            ) : (
              accounts.map((a) => {
                // 分组/标签范围外账号半透明标记（对齐国内「筛选外」；勾选仍可保存，
                // 但只有范围内的账号实际入池——与后端装配语义一致）
                const filteredOut = !inPoolScope(a);
                return (
                <div
                  key={a.id}
                  className={`flex items-center justify-between gap-3 border-b border-slate-50 px-4 py-2.5 last:border-0 dark:border-zinc-800/50 ${
                    filteredOut ? 'opacity-50' : ''
                  }`}
                >
                  <label className="flex min-w-0 flex-1 cursor-pointer items-center gap-2">
                    <input
                      type="checkbox"
                      className="mt-0"
                      checked={enabled(a)}
                      onChange={(e) => setPendingEnabled((p) => ({ ...p, [a.id]: e.target.checked }))}
                    />
                    <span className="min-w-0 flex-1">
                      {/* 名称 + 徽标同行（对齐国内 Buddy 账号池选择列表：在线/就绪等徽标紧跟账号名） */}
                      <span className="flex min-w-0 items-center gap-1.5">
                        <span className="truncate text-sm font-medium">{a.nickname || a.id}</span>
                        {(a.is_current_workbuddy || a.is_current_codebuddy) && (
                          <Badge tone="green">在线</Badge>
                        )}
                        {filteredOut && <Badge tone="slate">筛选外</Badge>}
                        {a.needs_relogin ? (
                          <Badge tone="amber">需重登</Badge>
                        ) : !a.has_credential ? (
                          <Badge tone="red">无凭证</Badge>
                        ) : (
                          <Badge tone="green">就绪</Badge>
                        )}
                      </span>
                      <span className="block truncate text-[11px] text-slate-400">{a.uid || a.id}</span>
                    </span>
                  </label>
                  {/* 积分右侧独立列（两位小数 tabular-nums，对齐国内「2415.24 积分」展示） */}
                  <span className="shrink-0 text-right tabular-nums text-xs text-slate-500">
                    {a.credits_balance != null ? `${a.credits_balance.toFixed(2)} 积分` : '余额未知'}
                  </span>
                </div>
                );
              })
            )}
          </div>

          <div className="border-t border-slate-100 px-4 py-3 dark:border-zinc-800">
            {/* 资源开关与调度参数（对齐国内：上游状态徽标 + 开关两行式 + 调度参数右对齐输入） */}
            <div className="mb-2 flex items-center gap-2">
              <Globe size={14} className="text-brand-500" />
              <span className="text-sm font-medium">资源开关与调度参数</span>
              {saving && <span className="text-xs text-slate-400">保存中…</span>}
            </div>
            <div className="mb-2">
              {flags.buddyAiEnabled ? (
                <Badge tone="green">上游已启用</Badge>
              ) : (
                <Badge tone="amber">上游未启用 — buddy-ai 源模型将显式报错</Badge>
              )}
            </div>
            <div className="space-y-1.5">
              <label
                className="flex cursor-pointer items-start gap-2.5 rounded-md px-1.5 py-1.5 transition hover:bg-slate-50 dark:hover:bg-zinc-800/50"
              >
                <input
                  type="checkbox"
                  className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                  checked={flags.buddyAiEnabled}
                  onChange={(e) => void saveFlags({ ...flags, buddyAiEnabled: e.target.checked })}
                />
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">启用 buddy-ai 国际档</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    {/* 审查修复（M9 同款）：登录入口实名是「账号管理」页（原「BuddyAI 页」非页面名，用户无法定位） */}
                    开启后「buddy-ai」档参与调度（与国内 Buddy 档平级、独立账号池/粘性/记账）；需先在「账号管理」页登录国际账号
                  </span>
                </span>
              </label>
              <label
                className="flex cursor-pointer items-start gap-2.5 rounded-md px-1.5 py-1.5 transition hover:bg-slate-50 dark:hover:bg-zinc-800/50"
              >
                <input
                  type="checkbox"
                  className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                  checked={flags.buddyAiStickyEnabled}
                  onChange={(e) => void saveFlags({ ...flags, buddyAiStickyEnabled: e.target.checked })}
                />
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">buddy-ai 会话粘性</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    显式 conversationId 在国际池内绑定账号（TTL 同池粘性口径）；默认关
                  </span>
                </span>
              </label>
            </div>

            {/* 伪流式灰度（issue #213）：仅内存热切换，重启复位默认关；开关样式对齐
                全局 API 管理「池级开关」范式——乐观更新 + 失败回滚，下一请求即生效 */}
            <div className="mt-4">
              <div className="mb-2 flex items-center gap-2">
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
                    启用伪流式（buddy-ai 池）
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

            {/* 调度参数（per-pool 数值热参数，保存即热生效；0 = 关闭/不限） */}
            <div className="mt-4">
              <div className="mb-2 flex items-center gap-2">
                <Gauge size={15} className="text-brand-500" />
                <span className="text-sm font-medium">调度参数</span>
                <span className="text-xs text-slate-400">保存即热生效 · 0 表示关闭/不限</span>
              </div>
              <div className="space-y-3">
                {(
                  [
                    {
                      key: 'concurrency',
                      label: '账号并发上限',
                      unit: '并发',
                      min: 0,
                      max: 16,
                      value: aiConcurrency,
                      setValue: setAiConcurrency,
                      desc: '单账号在途请求数达到上限即让位其他账号（全部 busy 时取负载最小者）；0 = 不限',
                      save: (v: number) => ({ buddyAiAccountConcurrencyLimit: v }),
                    },
                    {
                      key: 'poolTtl',
                      label: '池粘性 TTL',
                      unit: '秒',
                      min: 0,
                      max: 86400,
                      value: poolTtl,
                      setValue: setPoolTtl,
                      // 审查修复（L29）：原文案「60s–24h」与输入框 min=0 矛盾，改为对齐 0 值语义
                      desc: 'TTL 内同会话路由同一账号（上游 KV cache 复用）；0 = 关闭；60–86400 秒',
                      save: (v: number) => ({ buddyAiPoolStickyTtlSecs: v }),
                    },
                    {
                      key: 'sessionTtl',
                      label: '会话粘性 TTL',
                      unit: '秒',
                      min: 0,
                      max: 86400,
                      value: aiSessionTtl,
                      setValue: setAiSessionTtl,
                      desc: '显式 conversationId 绑定账号的有效期',
                      save: (v: number) => ({ buddyAiStickyTtlSecs: v }),
                    },
                    {
                      key: 'hedge',
                      label: '宽渡对冲阈值',
                      unit: 'ms',
                      min: 0,
                      max: 8000,
                      value: aiHedgeMs,
                      setValue: setAiHedgeMs,
                      desc: '流式首字节超此时长即向第二账号发对冲请求，先出首字者胜；0 = 关闭（有效范围 1s-8s、与后端对齐）',
                      save: (v: number) => ({ buddyAiHedgeThresholdMs: v }),
                    },
                  ] as const
                ).map((item) => (
                  <div key={item.label} className="rounded-md px-1.5 py-1.5">
                    <div className="flex items-center justify-between gap-3">
                      <span className="text-xs text-slate-700 dark:text-zinc-200">{item.label}</span>
                      <div className="flex items-center gap-2">
                        <input
                          type="number"
                          className="w-24 rounded-md border border-slate-200 bg-transparent px-2 py-1 text-right text-xs tabular-nums focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900"
                          min={item.min}
                          max={item.max}
                          value={item.value}
                          onChange={(e) => {
                            const v = Number(e.target.value);
                            if (Number.isFinite(v)) item.setValue(v);
                          }}
                          onBlur={() => {
                            const clamped = Math.min(Math.max(Math.round(item.value || 0), item.min), item.max);
                            item.setValue(clamped);
                            // 审查修复（P3 同源）：值未变（含夹取后与上次落库相同）不重复保存；
                            // 保存成功才记入 ref，失败时下次失焦自动重试
                            void (async () => {
                              if (savedParamsRef.current[item.key] === clamped) return;
                              const ok = await saveFlags({ ...flags, ...item.save(clamped) });
                              if (ok) savedParamsRef.current[item.key] = clamped;
                            })();
                          }}
                        />
                        <span className="w-8 shrink-0 text-[11px] text-slate-400">{item.unit}</span>
                      </div>
                    </div>
                    <p className="mt-1 text-[11px] leading-4 text-slate-400 dark:text-zinc-500">{item.desc}</p>
                  </div>
                ))}
              </div>
            </div>
            <div className="mt-3 rounded-md bg-slate-50 px-3 py-2 text-[11px] leading-4 text-slate-500 dark:bg-zinc-900 dark:text-zinc-400">
              隔离说明：buddy-ai 档账号池 / 凭证存储（buddy-ai vault 命名空间）/ 模型冷却 /
              用量记账（buddy_ai_days）/ 清洗模板全部独立于国内 Buddy 档。
            </div>
          </div>
        </div>

        {/* 右列：同步官网模型（对齐国内「同步官网模型」卡） */}
        <div className="card lg:col-span-6">
          <div className="border-b border-slate-100 px-4 py-3 dark:border-zinc-800">
            <div className="flex items-center justify-between gap-2">
              <div className="text-sm font-medium">
                同步官网模型
                <span className="ml-2 text-xs font-normal text-slate-400">共 {catalog.length} 个</span>
              </div>
              <button
                className="btn-outline shrink-0 inline-flex items-center gap-1.5 text-xs"
                disabled={catalogSyncing}
                onClick={() => {
                  setCatalogSyncing(true);
                  api.buddyai
                    .catalogSync()
                    .then((n) => pushToast('success', `Intl 模型目录已同步（${n} 个模型）`))
                    .catch((e) => pushToast('error', `目录同步失败：${String(e).slice(0, 140)}`))
                    .finally(() => {
                      setCatalogSyncing(false);
                      void refreshCatalog();
                    });
                }}
              >
                <RefreshCw size={12} className={catalogSyncing ? 'animate-spin' : ''} /> 同步官网模型
              </button>
            </div>
            <p className="mt-1 text-[11px] text-slate-500">
              直连官网 /v3/config 拉取模型与积分倍率（启动时自动刷新 · 失败回落静态兜底）；积分倍率与官方客户端一致。
            </p>
          </div>

          {/* 每日定时同步行（checkbox + time 即改即存） */}
          <div className="flex flex-wrap items-center gap-2 px-4 py-3 text-xs">
            <label className="flex cursor-pointer items-center gap-2">
              <input
                type="checkbox"
                checked={catalogTask.enabled}
                onChange={(e) => void saveCatalogTask({ ...catalogTask, enabled: e.target.checked })}
              />
              每日定时同步
            </label>
            <input
              type="time"
              value={catalogTask.hhmm}
              onChange={(e) => void saveCatalogTask({ ...catalogTask, hhmm: e.target.value || '05:55' })}
              disabled={!catalogTask.enabled}
              className="input h-8 !w-28 text-sm"
            />
            <span className="text-[11px] text-slate-400">每天 {catalogTask.hhmm || '05:55'} 自动同步（应用运行期间）</span>
          </div>

          {/* 模型目录表格 */}
          <div className="overflow-x-auto px-4 pb-4">
            {catalog.length === 0 ? (
              <p className="py-6 text-center text-xs text-slate-400">目录未加载，点上方「同步官网模型」拉取。</p>
            ) : (
              <table className="w-full min-w-[620px] text-sm">
                <thead>
                  <tr className="border-b border-slate-100 text-left text-xs text-slate-400 dark:border-zinc-800">
                    <th className="py-2 pr-3 font-medium">模型 ID</th>
                    <th className="py-2 pr-3 font-medium">展示名</th>
                    <th className="py-2 pr-3 font-medium">厂商</th>
                    <th className="py-2 pr-3 font-medium">积分倍率</th>
                    <th className="py-2 pr-3 font-medium">思考档位</th>
                    <th className="py-2 pr-3 font-medium">上下文</th>
                    <th className="py-2 text-center font-medium">图片支持</th>
                  </tr>
                </thead>
                <tbody>
                  {catalog.map((m) => (
                    <tr key={m.id} className="border-b border-slate-50 last:border-0 dark:border-zinc-800/50">
                      <td className="py-2 pr-3 font-mono text-xs">{m.id}</td>
                      <td className="py-2 pr-3 text-xs">{m.display || m.id}</td>
                      <td className="py-2 pr-3 text-xs text-slate-500">
                        {vendorMap.get(m.id) || inferVendor(m.id) || '—'}
                      </td>
                      <td className="py-2 pr-3 text-xs tabular-nums text-amber-600 dark:text-amber-400">
                        {m.rate.toFixed(2)}
                      </td>
                      <td className="py-2 pr-3 text-xs text-slate-500">
                        {m.supported_efforts?.length ? m.supported_efforts.join(' / ') : '—'}
                      </td>
                      <td className="py-2 pr-3 text-xs tabular-nums text-slate-500">
                        {m.context_length > 0 ? `${Math.round(m.context_length / 1000)}k` : '—'}
                      </td>
                      <td className="py-2 text-center text-xs">
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
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
