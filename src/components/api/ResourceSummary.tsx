/**
 * 全局 API 管理 · 资源总览与调度策略中心（unified-api-gateway-design §5.2/§5.3）
 * 「调度策略中心」卡集中三块（任务7，从资源角度整体设计）：
 *  ① 最佳组合预设：5 组专家组合（池间 + Trae / Buddy / Qoder / BuddyAI 国际 池内策略）一键应用，
 *     当前命中组合高亮（Qoder 与 Trae/Buddy 同等参与池间调度，默认序尾部可上移；
 *     BuddyAI 国际池由后端对缺失池自动补尾参与候选序）；
 *  ② 池间调度策略：smart / priority + 优先级序编辑 + 跨池回退（dispatch_policy_get/set 即时生效）；
 *  ③ 各资源池池内调度：Trae（strategy）/ Buddy（wb_strategy，空 = 跟随 Trae 池）/
 *     Qoder（qoder_strategy，空 = 跟随 Trae 池）/ BuddyAI 国际（buddy_ai_strategy，
 *     空 = 跟随 Trae 池）下拉编辑，自定义模型命中即直达；
 *     上游开关与池成员在各应用「资源调度」页维护。
 * 下方为 Trae / Buddy / Qoder / BuddyAI 国际 / 自定义 五池摘要卡；前四者详情引导至
 * 各应用「资源调度」页，自定义池详情引导至「自定义模型」Tab。
 * 数据源：pool_list / accounts_list（Trae 池）、pool_list.wb_enabled / workbuddy_accounts_list
 * （Buddy 池）、pool_list.qoder_enabled / qoder_accounts_list（Qoder 池）、
 * buddyai.accountsList（BuddyAI 国际池）、custom_models_list（自定义模型池）。
 */
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ArrowDown, ArrowUp, Blocks, Bot, Boxes, Globe, Layers, Server, SlidersHorizontal, Star, X } from 'lucide-react';
import { Badge } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { fmtCredits } from '../../lib/format';
import { isPoolVisible } from '../../lib/appVisibility';
import { DISPATCH_PRESETS, matchPreset } from './dispatchPresets';
import { computePoolMetrics } from './poolMetrics';
import type {
  AccountView,
  ApiPoolFile,
  BuddyAiAccountView,
  CustomModel,
  DispatchPolicy,
  QoderAccountView,
  TagView,
  WorkBuddyAccountView,
} from '../../types';

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
  qoder: 'Qoder 池',
  'buddy-ai': 'BuddyAI 国际池',
  custom: '自定义上游池',
};

const selectCls =
  'h-7 rounded-md border border-slate-200 bg-white px-2 text-xs text-slate-700 outline-none focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200';

const inputCls =
  'h-7 w-44 rounded-md border border-slate-200 bg-white px-2 text-xs text-slate-700 outline-none placeholder:text-slate-300 focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200';

/** 池间候选序全集（优先级编辑补尾与沉尾写回用全量；可见池排列的候选序在组件内按可见性动态派生。v3：custom 入池间策略全集，尾部兜底位） */
const POOL_KEYS = ['buddy', 'trae', 'qoder', 'buddy-ai', 'custom'];

/** 全排列（per_model 候选序选项：n 池 n! 种，覆盖全部合法顺序） */
function permutations(arr: string[]): string[][] {
  if (arr.length <= 1) return [arr];
  return arr.flatMap((item, i) =>
    permutations([...arr.slice(0, i), ...arr.slice(i + 1)]).map((rest) => [item, ...rest]),
  );
}

const orderKey = (arr: string[]) => arr.join('>');

const orderLabel = (k: string) =>
  k
    .split('>')
    .map((p) => POOL_LABELS[p] ?? p)
    .join(' → ');

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
  const settings = useAppStore((s) => s.settings);
  const [policy, setPolicy] = useState<DispatchPolicy | null>(null);
  // 审查修复（第7项7a）：调度策略读取失败此前被 .catch 静默吞掉，整块 UI 消失无提示——
  // 现改为错误态：toast 报错一次 + 卡内渲染错误提示与「重试」按钮（reloadSeq 递增重触发拉取）
  const [policyError, setPolicyError] = useState(false);
  const [reloadSeq, setReloadSeq] = useState(0);
  const [saving, setSaving] = useState(false);
  // per_model 新增绑定行（本地输入态，添加时才落 policy；实际生效值见 newOrderEff 派生）
  const [newPerModelName, setNewPerModelName] = useState('');
  const [newPerModelOrder, setNewPerModelOrder] = useState('');
  // 账号保留剩余积分（issue #91）输入草稿：null = 未编辑（展示 policy 生效值），
  // onBlur 才落盘。审查 F14 修复：改存原始字符串——旧实现 Number('')=0 使「清空
  // 重输」直接跳 0，误失焦即静默保存 0（=关闭调度保护）；现空串/非法输入失焦时
  // 还原策略值不落盘，合法数值才提交（保存侧 fail-closed，无失焦抢跑钳制）
  const [reserveDraft, setReserveDraft] = useState<string | null>(null);
  // F2：保存请求序号守卫（仅最新一次响应可写状态）
  const saveSeqRef = useRef(0);

  useEffect(() => {
    setPolicyError(false);
    api.apiServer
      .dispatchPolicyGet()
      .then(setPolicy)
      .catch((e) => {
        // 审查修复（第7项7a）：读取失败不再静默保留空态——toast 报错一次并转卡内错误态
        setPolicyError(true);
        toast('error', `调度策略读取失败：${String(e).slice(0, 120)}`);
      });
  }, [reloadSeq, toast]);

  /** 当前组合命中的预设（匹配规则见 dispatchPresets.matchPreset） */
  const activePreset = useMemo(() => matchPreset(pool, policy), [pool, policy]);

  /** 可见资源池（系统设置「应用显示」联动；custom 池恒可见但不参与池间候选序）。
   *  复审修复：按组件既有 POOL_KEYS 顺序派生（而非 lib 的 POOL_APPS 顺序）——
   *  全池可见时保持原 POOL_ORDERS[0] 默认预选序 buddy>trae>qoder>buddy-ai 不变 */
  const visiblePools = useMemo(() => POOL_KEYS.filter((p) => isPoolVisible(settings, p)), [settings]);
  /** 可见池全排列（per_model 候选序基座；四池全隐藏时为空 → 新增绑定禁用，存量绑定保留原文展示） */
  const poolOrders = useMemo(() => permutations(visiblePools).map((a) => a.join('>')), [visiblePools]);
  // 可见性变化后旧默认序不再在候选内时回落首个可见序（四池全隐藏时为空串 = 禁用新增绑定）
  const newOrderEff = poolOrders.includes(newPerModelOrder) ? newPerModelOrder : (poolOrders[0] ?? '');

  /** 统一保存入口：inter 走 dispatch_policy_set；trae/buddy/qoder/buddyAi 走 pool_set（未传维度保留原值） */
  const save = async (patch: {
    inter?: DispatchPolicy['strategy'];
    trae?: string;
    buddy?: string;
    qoder?: string;
    buddyAi?: string;
  }) => {
    if (!pool) return;
    // F2 序号守卫（与 savePolicy 同类竞态）：池内策略快速连续修改时保存响应可能
    // 乱序返回，旧响应覆盖最后选择——仅最新一次请求允许写状态/toast/saving 复位
    const seq = ++saveSeqRef.current;
    setSaving(true);
    try {
      if (patch.inter && policy) {
        // 审查复审修复：写状态同样纳入 seq 门控——乱序的旧 inter 响应不得覆盖
        // 最新策略（同函数其余写状态均已门控，此处为漏网之笔）
        const next = await api.apiServer.dispatchPolicySet({ ...policy, strategy: patch.inter });
        if (seq === saveSeqRef.current) setPolicy(next);
      }
      // 只传本次修改的维度（其余 null = 后端保留原值）：若全量回传四池快照，
      // 快速连续修改时后发请求会携带旧快照，把先前已落库的其他池策略静默回退
      await api.apiServer.poolSet(
        pool.enabled_uids,
        patch.trae,
        pool.group_ids,
        { qoderStrategy: patch.qoder, buddyAiStrategy: patch.buddyAi },
        patch.buddy,
      );
      if (seq === saveSeqRef.current) {
        // 审查修复（H3 / H-3 伴生修复）：保存成功后回读后端最新池配置刷新父级状态
        // （poolSet 的 uids/group_ids 参数为页面加载时的旧快照，弹窗常驻期间他端改过
        // 的池配置会被旧快照静默回退，原先仅做本地合并无法感知；Qoder/Buddy 页同款
        // 修复）。回读失败时退回本地合并（仅覆盖本次修改维度），保证 UI 至少反映本次
        // 保存结果；回读 await 后复检 seq，乱序的旧回读响应不得覆盖新保存
        const latest = await api.apiServer.poolList().catch(() => null);
        if (seq === saveSeqRef.current) {
          if (latest) {
            onPoolChanged(latest);
          } else {
            // 本地合并仅覆盖本次修改的维度，其余保持父级现值
            onPoolChanged({
              ...pool,
              ...(patch.trae !== undefined ? { strategy: patch.trae } : null),
              ...(patch.buddy !== undefined ? { wb_strategy: patch.buddy } : null),
              ...(patch.qoder !== undefined ? { qoder_strategy: patch.qoder } : null),
              ...(patch.buddyAi !== undefined ? { buddy_ai_strategy: patch.buddyAi } : null),
            });
          }
          toast('success', '调度策略已保存，运行中网关即时生效');
        }
      }
    } catch (e) {
      if (seq === saveSeqRef.current) {
        toast('error', `调度策略保存失败：${String(e).slice(0, 120)}`);
      }
    } finally {
      if (seq === saveSeqRef.current) setSaving(false);
    }
  };

  // 优先级编辑全集：存量配置可能缺 buddy-ai（后端 resolve_target 对缺失池自动补尾，
  // 行为 = 尾部）——展示/编辑时补齐到四池全集，移动/保存写回完整数组
  const priorityFull = useMemo(() => {
    if (!policy) return [];
    return [...policy.priority, ...POOL_KEYS.filter((p) => !policy.priority.includes(p))];
  }, [policy]);

  /** 优先级可见子序列（隐藏池不参与编辑展示；仅用于 chips 渲染与 move 计算） */
  const visiblePriority = useMemo(
    () => priorityFull.filter((p) => isPoolVisible(settings, p)),
    [priorityFull, settings],
  );

  /** 优先级数组内移动（priority 模式编辑；在可见子序列上交换，隐藏池按原相对序沉尾写回全集） */
  const move = (i: number, dir: -1 | 1) => {
    if (!policy) return;
    const j = i + dir;
    if (j < 0 || j >= visiblePriority.length) return;
    const nextVisible = visiblePriority.slice();
    [nextVisible[i], nextVisible[j]] = [nextVisible[j], nextVisible[i]];
    const hidden = priorityFull.filter((p) => !nextVisible.includes(p));
    void savePolicy({ priority: [...nextVisible, ...hidden] });
  };

  const savePolicy = async (patch: Partial<DispatchPolicy>) => {
    if (!policy) return;
    // F2（审查修复清单 v1.4.9）：per_model 快速连续切换时保存响应可能乱序返回，
    // 旧响应覆盖最后选择——序号守卫：仅最新一次请求允许 setState/toast/saving 复位
    const seq = ++saveSeqRef.current;
    setSaving(true);
    try {
      const next = await api.apiServer.dispatchPolicySet({ ...policy, ...patch });
      if (seq === saveSeqRef.current) {
        setPolicy(next);
        // 审查修复（中等项）：写操作必须有可见反馈——成功也提示（对齐 L140 既有口径）
        toast('success', '调度策略已保存，运行中网关即时生效');
      }
    } catch (e) {
      if (seq === saveSeqRef.current) {
        toast('error', `调度策略保存失败：${String(e).slice(0, 120)}`);
      }
    } finally {
      if (seq === saveSeqRef.current) setSaving(false);
    }
  };

  if (!policy) {
    // 审查修复（第7项7a）：读取失败不再整块消失——渲染错误态（提示 + 重试）；
    // 首次加载中（无错误）仍保持隐藏，避免闪现空卡
    if (policyError) {
      return (
        <div className="card p-4">
          <div className="mb-2 flex flex-wrap items-center gap-2">
            <SlidersHorizontal size={16} className="text-slate-400" />
            <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">调度策略中心</span>
            <Badge tone="red">读取失败</Badge>
          </div>
          <p className="text-xs text-rose-600 dark:text-rose-400">
            调度策略读取失败，组合预设与池间 / 池内调度编辑暂不可用（下方各资源池摘要不受影响）。
          </p>
          <button className="btn-outline mt-3" onClick={() => setReloadSeq((n) => n + 1)}>
            重试
          </button>
        </div>
      );
    }
    return null;
  }

  return (
    <div className="card p-4">
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
          最佳组合预设（池间 + Trae / Buddy / Qoder / BuddyAI 国际 池内策略 一键应用）
        </div>
        <div className="flex flex-wrap gap-1.5">
          {DISPATCH_PRESETS.map((p) => {
            const active = activePreset?.key === p.key;
            return (
              <button
                key={p.key}
                title={p.desc}
                disabled={!pool || saving}
                onClick={() =>
                  void save({ inter: p.inter, trae: p.trae, buddy: p.buddy, qoder: p.qoder, buddyAi: p.buddyAi })
                }
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
        <p className="mt-1 text-[11px] text-slate-400 dark:text-zinc-500">
          Qoder 池与 BuddyAI 国际池和 Trae / Buddy 同等地位参与策略调度（存量配置未列出的池由网关自动补至候选序尾部，可在「固定优先级」中上移）。
        </p>
      </div>

      {/* ② 池间调度策略 */}
      <div className="border-t border-slate-100 pt-3 dark:border-zinc-800">
        <div className="mb-1.5 text-[11px] font-medium text-slate-500 dark:text-zinc-400">
          池间调度（仅作用于多源同名模型 Trae/Buddy/Qoder/BuddyAI 国际；自定义模型命中即直达不受影响）
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
              {visiblePriority.length === 0 ? (
                <span className="text-[11px] text-amber-600 dark:text-amber-400">
                  当前无可见资源池，优先级编辑不可用（存量配置保留生效）
                </span>
              ) : (
                visiblePriority.map((p, i) => (
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
                      disabled={i === visiblePriority.length - 1}
                      onClick={() => move(i, 1)}
                      title="下移"
                    >
                      <ArrowDown size={11} />
                    </button>
                  </span>
                ))
              )}
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
          {/* issue #90 修复③a：多源同名模型来源标识（/v1/models 池前缀别名条目） */}
          <label
            className="flex items-center gap-1.5 text-slate-500 dark:text-zinc-400"
            title="/v1/models 为多源同名模型附带 trae/xxx、buddy/xxx、buddy-ai/xxx、qoder/xxx 别名条目，客户端可按前缀点名指定池（各池同名模型倍率与效果不同）；关闭时目录保持原形态"
          >
            <input
              type="checkbox"
              checked={policy.model_source_labels}
              onChange={(e) => void savePolicy({ model_source_labels: e.target.checked })}
            />
            模型目录标注来源池
          </label>
          <label
            className="flex items-center gap-2"
            title="剩余积分低于或等于该值的账号暂停调度（切换其他账号），防止积分被耗尽后免费模型也不可用；0 = 关闭"
          >
            <span className="text-slate-400 dark:text-zinc-500">账号保留剩余积分</span>
            <input
              type="number"
              className={`${selectCls} w-20`}
              min={0}
              step={1}
              value={reserveDraft ?? String(policy.reserve_credits)}
              disabled={saving}
              onChange={(e) => setReserveDraft(e.target.value)}
              onBlur={() => {
                if (reserveDraft == null) return;
                const t = reserveDraft.trim();
                // 空串/非法数值：放弃编辑还原策略值（不静默清零/落盘）；
                // 接受小数（后端 reserve_credits 为 f64，避免既有小数值无法往返编辑）
                if (t === '' || !/^\d+(\.\d+)?$/.test(t)) {
                  setReserveDraft(null);
                  return;
                }
                const v = Math.max(0, Number(t));
                if (v !== policy.reserve_credits) {
                  void savePolicy({ reserve_credits: v });
                }
                setReserveDraft(null);
              }}
            />
            <span className="text-slate-400 dark:text-zinc-500">积分</span>
          </label>
        </div>
        <p className="mt-1.5 text-[11px] text-slate-400 dark:text-zinc-500">
          智能调度按请求模型对可用池排序：积分先到期优先 → 免费 / 倍率小优先 → 剩余积分多优先；全并列时按上方优先级序。
        </p>
        <p className="mt-1 text-[11px] text-slate-400 dark:text-zinc-500">
          账号保留剩余积分：剩余 ≤ 阈值的账号暂停调度（默认 50，0 = 关闭），防止积分被调度耗尽后免费模型也不可用。
        </p>

        {/* ②+ per_model 模型绑定池（DispatchPolicy.per_model）：显式为单个模型固定
            池候选序，优先于全局策略且不做智能重排（后端 dispatch.rs 有绑定序即最终序） */}
        <div className="mt-3">
          <div className="mb-1.5 text-[11px] font-medium text-slate-500 dark:text-zinc-400">
            模型绑定池（可选：为单个模型固定池候选序，优先于上方全局策略）
          </div>
          {poolOrders.length === 0 && (
            <p className="mb-1.5 text-[11px] text-amber-600 dark:text-amber-400">
              当前无可见资源池：新增绑定不可用，存量绑定按原候选序保留展示。
            </p>
          )}
          {Object.keys(policy.per_model).length === 0 ? (
            <p className="text-[11px] text-slate-400 dark:text-zinc-500">
              暂无绑定，全部模型按上方全局策略调度。
            </p>
          ) : (
            <div className="space-y-1.5">
              {Object.entries(policy.per_model).map(([model, order]) => (
                <div key={model} className="flex items-center gap-2 text-xs">
                  <span
                    className="w-44 truncate font-mono text-slate-700 dark:text-zinc-200"
                    title={model}
                  >
                    {model}
                  </span>
                  <select
                    className={selectCls}
                    value={orderKey(order)}
                    onChange={(e) => {
                      const next = { ...policy.per_model, [model]: e.target.value.split('>') };
                      void savePolicy({ per_model: next });
                    }}
                  >
                    {poolOrders.map((k) => (
                      <option key={k} value={k}>
                        {orderLabel(k)}
                      </option>
                    ))}
                    {!poolOrders.includes(orderKey(order)) && (
                      <option value={orderKey(order)}>{orderKey(order)}（自定义）</option>
                    )}
                  </select>
                  <button
                    className="rounded p-1 text-slate-400 hover:text-rose-500"
                    title="移除绑定"
                    disabled={saving}
                    onClick={() => {
                      const next = { ...policy.per_model };
                      delete next[model];
                      void savePolicy({ per_model: next });
                    }}
                  >
                    <X size={12} />
                  </button>
                </div>
              ))}
            </div>
          )}
          <div className="mt-1.5 flex items-center gap-2">
            <input
              className={inputCls}
              placeholder="模型名（如 gpt-4.1）"
              value={newPerModelName}
              onChange={(e) => setNewPerModelName(e.target.value)}
            />
            <select
              className={selectCls}
              value={newOrderEff}
              disabled={poolOrders.length === 0}
              onChange={(e) => setNewPerModelOrder(e.target.value)}
            >
              {poolOrders.map((k) => (
                <option key={k} value={k}>
                  {orderLabel(k)}
                </option>
              ))}
            </select>
            <button
              className="h-7 rounded-md border border-slate-200 px-2.5 text-xs text-slate-600 transition-colors hover:border-brand-300 hover:text-brand-600 disabled:opacity-40 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-300"
              disabled={!newPerModelName.trim() || !newOrderEff || saving || Boolean(policy.per_model[newPerModelName.trim()])}
              title={!newOrderEff ? '当前无可见资源池，无法新增绑定' : policy.per_model[newPerModelName.trim()] ? '该模型已绑定' : '添加绑定'}
              onClick={() => {
                const model = newPerModelName.trim();
                if (!model || !newOrderEff || policy.per_model[model]) return;
                const next = { ...policy.per_model, [model]: newOrderEff.split('>') };
                void savePolicy({ per_model: next });
                setNewPerModelName('');
              }}
            >
              添加
            </button>
          </div>
        </div>
      </div>

      {/* ③ 各资源池池内调度 */}
      <div className="mt-3 border-t border-slate-100 pt-3 dark:border-zinc-800">
        <div className="mb-1.5 text-[11px] font-medium text-slate-500 dark:text-zinc-400">
          各资源池池内调度（账号取号顺序；成员与分组在各应用「资源调度」页维护）
        </div>
        <div className="flex flex-wrap items-center gap-x-6 gap-y-2 text-xs">
          {isPoolVisible(settings, 'trae') && (
            <label className="flex items-center gap-2">
              <span className="text-slate-400 dark:text-zinc-500">Trae 池</span>
              <select
                className={selectCls}
                value={pool?.strategy || 'expire_first'}
                disabled={!pool || saving}
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
          )}
          {isPoolVisible(settings, 'buddy') && (
            <label className="flex items-center gap-2">
              <span className="text-slate-400 dark:text-zinc-500">Buddy 池</span>
              <select
                className={selectCls}
                value={pool?.wb_strategy ?? ''}
                disabled={!pool || saving}
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
          )}
          {isPoolVisible(settings, 'qoder') && (
            <label className="flex items-center gap-2">
              <span className="text-slate-400 dark:text-zinc-500">Qoder 池</span>
              <select
                className={selectCls}
                value={pool?.qoder_strategy ?? ''}
                disabled={!pool || saving}
                onChange={(e) => void save({ qoder: e.target.value })}
              >
                <option value="">跟随 Trae 池</option>
                {STRATEGIES.map((s) => (
                  <option key={s} value={s}>
                    {STRATEGY_LABELS[s]}
                  </option>
                ))}
              </select>
            </label>
          )}
          {isPoolVisible(settings, 'buddy-ai') && (
            <label className="flex items-center gap-2">
              <span className="text-slate-400 dark:text-zinc-500">BuddyAI 国际池</span>
              <select
                className={selectCls}
                value={pool?.buddy_ai_strategy ?? ''}
                disabled={!pool || saving}
                onChange={(e) => void save({ buddyAi: e.target.value })}
              >
                <option value="">跟随 Trae 池</option>
                {STRATEGIES.map((s) => (
                  <option key={s} value={s}>
                    {STRATEGY_LABELS[s]}
                  </option>
                ))}
              </select>
            </label>
          )}
          <span className="text-slate-400 dark:text-zinc-500">
            自定义模型：<span className="text-slate-700 dark:text-zinc-200">命中即直达</span>（上游自有计费，无池内调度）
          </span>
          <span className="text-slate-400 dark:text-zinc-500">
            上游开关与成员在各应用「资源调度」页配置
          </span>
        </div>
      </div>
    </div>
  );
}

export default function ResourceSummary() {
  const settings = useAppStore((s) => s.settings);
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [accounts, setAccounts] = useState<AccountView[]>([]);
  const [wbAccounts, setWbAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [qoderAccounts, setQoderAccounts] = useState<QoderAccountView[]>([]);
  const [buddyAiAccounts, setBuddyAiAccounts] = useState<BuddyAiAccountView[]>([]);
  // 共享标签（Buddy/Qoder/BuddyAI 池内口径的标签筛选 ∩ 集合，与后端装配一致）
  const [tags, setTags] = useState<TagView[]>([]);
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
    api.qoder
      .accountsList()
      .then(setQoderAccounts)
      .catch(() => {
        /* 保留空摘要 */
      });
    api.buddyai
      .accountsList()
      .then(setBuddyAiAccounts)
      .catch(() => {
        /* 保留空摘要 */
      });
    api.tags
      .list()
      .then(setTags)
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

  // 五池摘要指标（池内成员与积分口径的纯计算见 poolMetrics.ts，含单测）
  const metrics = computePoolMetrics(
    pool,
    accounts,
    wbAccounts,
    customModels,
    qoderAccounts,
    buddyAiAccounts,
    tags,
  );
  // Buddy 池内策略文案：空 = 跟随 Trae 池（展示 Trae 当前生效策略）
  const buddyStrategyText = pool?.wb_strategy
    ? (STRATEGY_LABELS[pool.wb_strategy] ?? pool.wb_strategy)
    : pool?.strategy
      ? `跟随 Trae 池（${STRATEGY_LABELS[pool.strategy] ?? pool.strategy}）`
      : '—';
  // Qoder 池内策略文案：空 = 跟随 Trae 池（同 Buddy 语义）
  const qoderStrategyText = pool?.qoder_strategy
    ? (STRATEGY_LABELS[pool.qoder_strategy] ?? pool.qoder_strategy)
    : pool?.strategy
      ? `跟随 Trae 池（${STRATEGY_LABELS[pool.strategy] ?? pool.strategy}）`
      : '—';
  // BuddyAI 国际池内策略文案：空 = 跟随 Trae 池（同 Buddy/Qoder 语义）
  const buddyAiStrategyText = pool?.buddy_ai_strategy
    ? (STRATEGY_LABELS[pool.buddy_ai_strategy] ?? pool.buddy_ai_strategy)
    : pool?.strategy
      ? `跟随 Trae 池（${STRATEGY_LABELS[pool.strategy] ?? pool.strategy}）`
      : '—';

  // 自定义模型池：启用条目数 / 总条目数
  const customEnabled = customModels.filter((m) => m.enabled).length;

  return (
    <div className="space-y-4">
      {/* 调度策略中心（预设 + 池间 + 池内集中管理，整宽一行） */}
      <DispatchCenterCard pool={pool} onPoolChanged={setPool} />

      {/* 五池摘要卡（Trae / Buddy / Qoder / BuddyAI 国际 / 自定义）：两行一行三个，自适应宽度 */}
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3">
      {/* Trae 资源池摘要（应用隐藏时不渲染，数据照常统计） */}
      {isPoolVisible(settings, 'trae') && (
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Server size={16} className="text-brand-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">Trae 资源池</span>
          {/* 对齐 Buddy/Qoder 卡口径：上游开关状态（原「可用/空池」与池成员数语义重复） */}
          <Badge tone={(pool?.trae_enabled ?? true) ? 'green' : 'slate'}>
            {(pool?.trae_enabled ?? true) ? '上游已启用' : '上游未启用'}
          </Badge>
        </div>
        <div className="space-y-1.5">
          <Row label="池内账号数" value={metrics.traePoolCount} />
          <Row
            label="可用积分"
            value={
              <span className="font-semibold text-amber-600 dark:text-amber-400">
                {fmtCredits(metrics.traePoolCredits)}
              </span>
            }
          />
          <Row label="账号总数" value={metrics.traeAccountTotal} />
          <Row
            label="积分总余额"
            value={
              <span className="font-semibold text-amber-600 dark:text-amber-400">
                {fmtCredits(metrics.traeTotalCredits)}
              </span>
            }
          />
          <Row
            label="池内调度策略"
            value={pool?.strategy ? (STRATEGY_LABELS[pool.strategy] ?? pool.strategy) : '—'}
          />
          <Row label="消耗口径" value="通用积分（product_id 208）" />
        </div>
        <p className="mt-3 border-t border-slate-100 pt-2 text-[11px] text-slate-400 dark:border-zinc-800 dark:text-zinc-500">
          详情（账号池选择 / 分组筛选 / 模型目录）见 Trae「资源调度」页
        </p>
      </div>
      )}

      {/* Buddy 资源池摘要（应用隐藏时不渲染，数据照常统计） */}
      {isPoolVisible(settings, 'buddy') && (
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Bot size={16} className="text-violet-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">Buddy 资源池</span>
          <Badge tone={metrics.buddyEnabled ? 'green' : 'slate'}>
            {metrics.buddyEnabled ? '上游已启用' : '上游未启用'}
          </Badge>
        </div>
        <div className="space-y-1.5">
          <Row label="池内账号数" value={metrics.buddyPoolCount} />
          <Row
            label="可用积分"
            value={
              <span className="font-semibold text-amber-600 dark:text-amber-400">
                {metrics.buddyPoolCredits != null ? fmtCredits(metrics.buddyPoolCredits) : '未知'}
              </span>
            }
          />
          <Row label="账号总数" value={metrics.buddyAccountTotal} />
          <Row
            label="积分总余额"
            value={
              <span className="font-semibold text-amber-600 dark:text-amber-400">
                {metrics.buddyTotalCredits != null ? fmtCredits(metrics.buddyTotalCredits) : '未知'}
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
      )}

      {/* Qoder 资源池摘要（应用隐藏时不渲染，数据照常统计） */}
      {isPoolVisible(settings, 'qoder') && (
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Boxes size={16} className="text-emerald-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">Qoder 资源池</span>
          <Badge tone={metrics.qoderEnabled ? 'green' : 'slate'}>
            {metrics.qoderEnabled ? '上游已启用' : '上游未启用'}
          </Badge>
        </div>
        <div className="space-y-1.5">
          <Row label="池内账号数" value={metrics.qoderPoolCount} />
          <Row
            label="可用积分"
            value={
              <span className="font-semibold text-amber-600 dark:text-amber-400">
                {metrics.qoderPoolCredits != null ? fmtCredits(metrics.qoderPoolCredits) : '未知'}
              </span>
            }
          />
          <Row label="账号总数" value={metrics.qoderAccountTotal} />
          <Row
            label="积分总余额"
            value={
              <span className="font-semibold text-amber-600 dark:text-amber-400">
                {metrics.qoderTotalCredits != null ? fmtCredits(metrics.qoderTotalCredits) : '未知'}
              </span>
            }
          />
          <Row label="池内调度策略" value={qoderStrategyText} />
          <Row label="消耗口径" value="Qoder 通用 credits" />
        </div>
        <p className="mt-3 border-t border-slate-100 pt-2 text-[11px] text-slate-400 dark:border-zinc-800 dark:text-zinc-500">
          <Layers size={11} className="mr-1 inline" />
          详情（上游开关 / 模型目录 / 账号清单）见 Qoder「资源调度」页
        </p>
      </div>
      )}

      {/* BuddyAI 国际资源池摘要（应用隐藏时不渲染，数据照常统计） */}
      {isPoolVisible(settings, 'buddy-ai') && (
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Globe size={16} className="text-cyan-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">BuddyAI 国际资源池</span>
          <Badge tone={metrics.buddyAiEnabled ? 'green' : 'slate'}>
            {metrics.buddyAiEnabled ? '上游已启用' : '上游未启用'}
          </Badge>
        </div>
        <div className="space-y-1.5">
          <Row label="池内账号数" value={metrics.buddyAiPoolCount} />
          <Row
            label="可用积分"
            value={
              <span className="font-semibold text-amber-600 dark:text-amber-400">
                {metrics.buddyAiPoolCredits != null ? fmtCredits(metrics.buddyAiPoolCredits) : '未知'}
              </span>
            }
          />
          <Row label="账号总数" value={metrics.buddyAiAccountTotal} />
          <Row
            label="积分总余额"
            value={
              <span className="font-semibold text-amber-600 dark:text-amber-400">
                {metrics.buddyAiTotalCredits != null ? fmtCredits(metrics.buddyAiTotalCredits) : '未知'}
              </span>
            }
          />
          <Row label="池内调度策略" value={buddyAiStrategyText} />
          <Row label="消耗口径" value="BuddyAI 国际积分（按模型倍率）" />
        </div>
        <p className="mt-3 border-t border-slate-100 pt-2 text-[11px] text-slate-400 dark:border-zinc-800 dark:text-zinc-500">
          <Layers size={11} className="mr-1 inline" />
          详情（上游开关 / 模型目录 / 账号池）见 BuddyAI 国际「资源调度」页
        </p>
      </div>
      )}

      {/* 自定义模型池摘要（恒可见） */}
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Blocks size={16} className="text-sky-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">自定义资源池</span>
          <Badge tone={metrics.customEnabledCount > 0 ? 'green' : 'slate'}>
            {metrics.customEnabledCount > 0 ? '调度中' : '无启用条目'}
          </Badge>
        </div>
        <div className="space-y-1.5">
          <Row label="启用模型数" value={metrics.customEnabledCount} />
          <Row label="模型总数" value={metrics.customModelTotal} />
          <Row label="消耗口径" value="上游自有计费" />
          <Row label="调度语义" value="模型名命中即直达" />
        </div>
        <p className="mt-3 border-t border-slate-100 pt-2 text-[11px] text-slate-400 dark:border-zinc-800 dark:text-zinc-500">
          <Blocks size={11} className="mr-1 inline" />
          详情（新增 / 编辑 / 启停）见「自定义模型」Tab
        </p>
      </div>
      </div>
    </div>
  );
}
