/**
 * 全局 API 管理 · 资源总览与调度策略中心（unified-api-gateway-design §5.2/§5.3）
 * 「调度策略中心」卡集中三块（任务7，从资源角度整体设计）：
 *  ① 最佳组合预设：5 组专家组合（池间 + Trae 池 + Buddy 池）一键应用，当前命中组合高亮；
 *  ② 池间调度策略：smart / priority + 优先级序编辑 + 跨池回退（dispatch_policy_get/set 即时生效）；
 *  ③ 各资源池池内调度：Trae（strategy）/ Buddy（wb_strategy，空 = 跟随 Trae 池）下拉编辑，
 *     自定义模型命中即直达、无池内调度（只读展示）。
 * 下方为 Trae / Buddy / 自定义 三池摘要卡；前两者详情引导至各应用「资源调度」页，
 * 自定义池详情引导至「自定义模型」Tab。
 * 数据源：pool_list / accounts_list（Trae 池）、pool_list.wb_enabled / workbuddy_accounts_list
 * （Buddy 池）、custom_models_list（自定义模型池）。
 */
import { useEffect, useMemo, useState, type ReactNode } from 'react';
import { ArrowDown, ArrowUp, Blocks, Bot, Layers, Server, SlidersHorizontal, Star } from 'lucide-react';
import { Badge } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { fmtCredits } from '../../lib/format';
import { DISPATCH_PRESETS, matchPreset } from './dispatchPresets';
import type { AccountView, ApiPoolFile, CustomModel, DispatchPolicy, WorkBuddyAccountView } from '../../types';

function Row({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-2 text-xs">
      <span className="text-slate-400 dark:text-zinc-500">{label}</span>
      <span className="tabular-nums text-slate-700 dark:text-zinc-200">{value}</span>
    </div>
  );
}

/** 池内调度策略取值（Trae / Buddy 同一套，pool.rs PoolStrategy） */
const STRATEGIES = ['expire_first', 'credit_first', 'random', 'weighted', 'p2c'] as const;

/** 池内调度策略文案（与账号池选择下拉一致） */
const STRATEGY_LABELS: Record<string, string> = {
  expire_first: '积分先过期优先',
  credit_first: '剩余积分多优先',
  random: '随机',
  weighted: '三因子加权随机',
  p2c: 'P2C 随机二选一',
};

const POOL_LABELS: Record<string, string> = {
  buddy: 'Buddy 池',
  trae: 'Trae 池',
};

const selectCls =
  'h-7 rounded-md border border-slate-200 bg-white px-2 text-xs text-slate-700 outline-none focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200';

/**
 * 调度策略中心卡：预设 + 池间 + 池内三块集中管理。
 * pool / onPoolChanged 由父级持有（三池摘要卡共用同一份数据）。
 */
function DispatchCenterCard({
  pool,
  onPoolChanged,
}: {
  pool: ApiPoolFile | null;
  onPoolChanged: (p: ApiPoolFile) => void;
}) {
  const toast = useAppStore((s) => s.pushToast);
  const [policy, setPolicy] = useState<DispatchPolicy | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    api.apiServer
      .dispatchPolicyGet()
      .then(setPolicy)
      .catch(() => {
        /* 保留空态 */
      });
  }, []);

  /** 当前组合命中的预设（匹配规则见 dispatchPresets.matchPreset） */
  const activePreset = useMemo(() => matchPreset(pool, policy), [pool, policy]);

  /** 统一保存入口：inter 走 dispatch_policy_set；trae/buddy 走 pool_set（未传维度保留原值） */
  const save = async (patch: { inter?: DispatchPolicy['strategy']; trae?: string; buddy?: string }) => {
    if (!pool) return;
    const trae = patch.trae ?? pool.strategy ?? '';
    const buddy = patch.buddy ?? pool.wb_strategy ?? '';
    setSaving(true);
    try {
      if (patch.inter && policy) {
        setPolicy(await api.apiServer.dispatchPolicySet({ ...policy, strategy: patch.inter }));
      }
      // uids/groups 原样回传（本卡不改成员与分组）；wbFlags 未传 → 后端保留原值
      await api.apiServer.poolSet(pool.enabled_uids, trae, pool.group_ids, undefined, buddy);
      onPoolChanged({ ...pool, strategy: trae, wb_strategy: buddy });
      toast('success', '调度策略已保存，运行中网关即时生效');
    } catch (e) {
      toast('error', `调度策略保存失败：${String(e).slice(0, 120)}`);
    } finally {
      setSaving(false);
    }
  };

  /** 优先级数组内移动（priority 模式编辑） */
  const move = (i: number, dir: -1 | 1) => {
    if (!policy) return;
    const next = policy.priority.slice();
    const j = i + dir;
    if (j < 0 || j >= next.length) return;
    [next[i], next[j]] = [next[j], next[i]];
    void savePolicy({ priority: next });
  };

  const savePolicy = async (patch: Partial<DispatchPolicy>) => {
    if (!policy) return;
    setSaving(true);
    try {
      setPolicy(await api.apiServer.dispatchPolicySet({ ...policy, ...patch }));
    } catch (e) {
      toast('error', `调度策略保存失败：${String(e).slice(0, 120)}`);
    } finally {
      setSaving(false);
    }
  };

  if (!policy) return null;

  return (
    <div className="card p-4 lg:col-span-3">
      {/* 头部 */}
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <SlidersHorizontal size={16} className="text-brand-500" />
        <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">调度策略中心</span>
        {activePreset ? (
          <Badge tone="blue">{activePreset.name}</Badge>
        ) : (
          pool && <Badge tone="slate">自定义组合</Badge>
        )}
        {saving && <span className="text-[11px] text-slate-400">保存中…</span>}
        <span className="ml-auto text-[11px] text-slate-400">运行中网关即时生效，无需重启</span>
      </div>

      {/* ① 最佳组合预设 */}
      <div className="mb-3">
        <div className="mb-1.5 text-[11px] font-medium text-slate-500 dark:text-zinc-400">
          最佳组合预设（池间 + Trae 池 + Buddy 池 一键应用）
        </div>
        <div className="flex flex-wrap gap-1.5">
          {DISPATCH_PRESETS.map((p) => {
            const active = activePreset?.key === p.key;
            return (
              <button
                key={p.key}
                title={p.desc}
                disabled={!pool || saving}
                onClick={() => void save({ inter: p.inter, trae: p.trae, buddy: p.buddy })}
                className={`flex items-center gap-1 rounded-full border px-2.5 py-1 text-xs transition-colors disabled:opacity-50 ${
                  active
                    ? 'border-brand-500 bg-brand-50 font-semibold text-brand-600 dark:bg-brand-500/10 dark:text-brand-400'
                    : 'border-slate-200 bg-white text-slate-600 hover:border-brand-300 hover:text-brand-600 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-300'
                }`}
              >
                {p.recommended && <Star size={11} className="text-amber-500" fill="currentColor" />}
                {p.name}
              </button>
            );
          })}
        </div>
        <p className="mt-1.5 text-[11px] text-slate-400 dark:text-zinc-500">
          {activePreset ? activePreset.desc : '当前为手动组合；选择预设可一键对齐推荐配置'}
        </p>
      </div>

      {/* ② 池间调度策略 */}
      <div className="border-t border-slate-100 pt-3 dark:border-zinc-800">
        <div className="mb-1.5 text-[11px] font-medium text-slate-500 dark:text-zinc-400">
          池间调度（仅作用于双源模型 Trae/Buddy 同名；自定义模型命中即直达不受影响）
        </div>
        <div className="flex flex-wrap items-center gap-x-6 gap-y-2 text-xs">
          <label className="flex items-center gap-2">
            <span className="text-slate-400 dark:text-zinc-500">策略</span>
            <select
              className={selectCls}
              value={policy.strategy}
              onChange={(e) => void savePolicy({ strategy: e.target.value as DispatchPolicy['strategy'] })}
            >
              <option value="smart">智能调度（默认）</option>
              <option value="priority">固定优先级</option>
            </select>
          </label>
          {policy.strategy === 'priority' && (
            <div className="flex items-center gap-1.5">
              <span className="text-slate-400 dark:text-zinc-500">优先级</span>
              {policy.priority.map((p, i) => (
                <span
                  key={p}
                  className="flex items-center gap-0.5 rounded-full border border-slate-200 py-0.5 pl-2 pr-0.5 dark:border-zinc-700"
                >
                  {POOL_LABELS[p] ?? p}
                  <button
                    className="rounded p-0.5 text-slate-400 hover:text-brand-500 disabled:opacity-30"
                    disabled={i === 0}
                    onClick={() => move(i, -1)}
                    title="上移"
                  >
                    <ArrowUp size={11} />
                  </button>
                  <button
                    className="rounded p-0.5 text-slate-400 hover:text-brand-500 disabled:opacity-30"
                    disabled={i === policy.priority.length - 1}
                    onClick={() => move(i, 1)}
                    title="下移"
                  >
                    <ArrowDown size={11} />
                  </button>
                </span>
              ))}
            </div>
          )}
          <label className="flex items-center gap-1.5 text-slate-500 dark:text-zinc-400">
            <input
              type="checkbox"
              checked={policy.fallback}
              onChange={(e) => void savePolicy({ fallback: e.target.checked })}
            />
            首选池不可用时跨池回退
          </label>
        </div>
        <p className="mt-1.5 text-[11px] text-slate-400 dark:text-zinc-500">
          智能调度按请求模型对可用池排序：积分先到期优先 → 免费 / 倍率小优先 → 剩余积分多优先；全并列时按上方优先级序。
        </p>
      </div>

      {/* ③ 各资源池池内调度 */}
      <div className="mt-3 border-t border-slate-100 pt-3 dark:border-zinc-800">
        <div className="mb-1.5 text-[11px] font-medium text-slate-500 dark:text-zinc-400">
          各资源池池内调度（账号取号顺序；成员与分组在各应用「资源调度」页维护）
        </div>
        <div className="flex flex-wrap items-center gap-x-6 gap-y-2 text-xs">
          <label className="flex items-center gap-2">
            <span className="text-slate-400 dark:text-zinc-500">Trae 池</span>
            <select
              className={selectCls}
              value={pool?.strategy || 'expire_first'}
              disabled={!pool}
              onChange={(e) => void save({ trae: e.target.value })}
            >
              {STRATEGIES.map((s) => (
                <option key={s} value={s}>
                  {STRATEGY_LABELS[s]}
                  {s === 'expire_first' ? '（默认）' : ''}
                </option>
              ))}
            </select>
          </label>
          <label className="flex items-center gap-2">
            <span className="text-slate-400 dark:text-zinc-500">Buddy 池</span>
            <select
              className={selectCls}
              value={pool?.wb_strategy ?? ''}
              disabled={!pool}
              onChange={(e) => void save({ buddy: e.target.value })}
            >
              <option value="">跟随 Trae 池</option>
              {STRATEGIES.map((s) => (
                <option key={s} value={s}>
                  {STRATEGY_LABELS[s]}
                </option>
              ))}
            </select>
          </label>
          <span className="text-slate-400 dark:text-zinc-500">
            自定义模型：<span className="text-slate-700 dark:text-zinc-200">命中即直达</span>（上游自有计费，无池内调度）
          </span>
        </div>
      </div>
    </div>
  );
}

export default function ResourceSummary() {
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [accounts, setAccounts] = useState<AccountView[]>([]);
  const [wbAccounts, setWbAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [customModels, setCustomModels] = useState<CustomModel[]>([]);

  useEffect(() => {
    api.apiServer
      .poolList()
      .then(setPool)
      .catch(() => {
        /* 保留空摘要 */
      });
    api.accounts
      .list()
      .then(setAccounts)
      .catch(() => {
        /* 保留空摘要 */
      });
    api.workbuddy
      .accountsList()
      .then(setWbAccounts)
      .catch(() => {
        /* 保留空摘要 */
      });
    api.apiServer
      .customModelsList()
      .then(setCustomModels)
      .catch(() => {
        /* 保留空摘要 */
      });
  }, []);

  // Trae 池：池内账号与通用积分（本服务消耗通用积分，零积分账号无法服务请求）
  const traePoolCount = pool?.enabled_uids.length ?? 0;
  const traeWithCredits = accounts.filter((a) => (a.general_credits ?? 0) > 0).length;
  const traeTotalCredits = accounts.reduce((s, a) => s + (a.general_credits ?? 0), 0);

  // Buddy 池：上游开关 + 含凭证账号与积分余额
  const buddyEnabled = pool?.wb_enabled ?? false;
  const credAccounts = wbAccounts.filter((a) => a.has_credential);
  const buddyTotalCredits = credAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0);
  // Buddy 池内策略文案：空 = 跟随 Trae 池（展示 Trae 当前生效策略）
  const buddyStrategyText = pool?.wb_strategy
    ? (STRATEGY_LABELS[pool.wb_strategy] ?? pool.wb_strategy)
    : pool?.strategy
      ? `跟随 Trae 池（${STRATEGY_LABELS[pool.strategy] ?? pool.strategy}）`
      : '—';

  // 自定义模型池：启用条目数 / 总条目数
  const customEnabled = customModels.filter((m) => m.enabled).length;

  return (
    <div className="grid grid-cols-1 gap-4 lg:grid-cols-3">
      {/* 调度策略中心（预设 + 池间 + 池内集中管理） */}
      <DispatchCenterCard pool={pool} onPoolChanged={setPool} />

      {/* Trae 资源池摘要 */}
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Server size={16} className="text-brand-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">Trae 资源池</span>
          <span className="ml-auto text-[11px] text-slate-400">消耗通用积分（product_id 208）</span>
        </div>
        <div className="space-y-1.5">
          <Row label="池内账号" value={traePoolCount} />
          <Row label="含通用积分账号" value={traeWithCredits} />
          <Row
            label="通用积分总余额"
            value={<span className="font-semibold text-amber-600 dark:text-amber-400">{fmtCredits(traeTotalCredits)}</span>}
          />
          <Row
            label="池内调度策略"
            value={pool?.strategy ? (STRATEGY_LABELS[pool.strategy] ?? pool.strategy) : '—'}
          />
        </div>
        <p className="mt-3 border-t border-slate-100 pt-2 text-[11px] text-slate-400 dark:border-zinc-800 dark:text-zinc-500">
          详情（账号池选择 / 分组筛选 / 模型目录）见 Trae「资源调度」页
        </p>
      </div>

      {/* Buddy 资源池摘要 */}
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Bot size={16} className="text-violet-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">Buddy 资源池</span>
          <Badge tone={buddyEnabled ? 'green' : 'slate'}>{buddyEnabled ? '上游已启用' : '上游未启用'}</Badge>
        </div>
        <div className="space-y-1.5">
          <Row label="含凭证账号" value={credAccounts.length} />
          <Row label="账号总数" value={wbAccounts.length} />
          <Row
            label="Buddy 积分总余额"
            value={
              <span className="font-semibold text-amber-600 dark:text-amber-400">
                {credAccounts.some((a) => a.credits_balance != null) ? fmtCredits(buddyTotalCredits) : '未知'}
              </span>
            }
          />
          <Row label="池内调度策略" value={buddyStrategyText} />
          <Row label="消耗口径" value="Buddy 积分（按模型倍率）" />
        </div>
        <p className="mt-3 border-t border-slate-100 pt-2 text-[11px] text-slate-400 dark:border-zinc-800 dark:text-zinc-500">
          <Layers size={11} className="mr-1 inline" />
          详情（上游开关 / 模型目录 / 账号池）见 Buddy「资源调度」页
        </p>
      </div>

      {/* 自定义模型池摘要 */}
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Blocks size={16} className="text-sky-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">自定义资源池</span>
          <Badge tone={customEnabled > 0 ? 'green' : 'slate'}>
            {customEnabled > 0 ? '调度中' : '无启用条目'}
          </Badge>
        </div>
        <div className="space-y-1.5">
          <Row label="启用模型" value={customEnabled} />
          <Row label="模型总数" value={customModels.length} />
          <Row label="消耗口径" value="上游自有计费" />
          <Row label="调度语义" value="模型名命中即直达" />
        </div>
        <p className="mt-3 border-t border-slate-100 pt-2 text-[11px] text-slate-400 dark:border-zinc-800 dark:text-zinc-500">
          <Blocks size={11} className="mr-1 inline" />
          详情（新增 / 编辑 / 启停）见「自定义模型」Tab
        </p>
      </div>
    </div>
  );
}
