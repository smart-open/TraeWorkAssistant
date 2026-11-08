import { useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw, TrendingUp } from 'lucide-react';
import {
  Bar,
  Line,
  XAxis,
  YAxis,
  ComposedChart,
  ResponsiveContainer,
  Tooltip,
  CartesianGrid,
  Legend,
} from 'recharts';
import PageHeader from '../../components/PageHeader';
import { StatCard } from '../../components/ui';
import { MultiSelectDropdown } from '../../components/MultiSelectDropdown';
import { Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { DoubaoAccountView, DoubaoDashboardStats, DoubaoDayDetail, GroupView } from '../../types';

/** 时间区间（近 7 天 / 近 30 天 / 本月 / 近一年） */
type RangeKey = '7d' | '30d' | 'month' | 'year';

const RANGE_LABEL: Record<RangeKey, string> = {
  '7d': '近 7 天',
  '30d': '近 30 天',
  month: '本月',
  year: '近一年',
};

/** 区间起始日期键（YYYY-MM-DD，含当日；year = 全量 365 天窗口交给后端裁剪） */
function rangeStart(key: RangeKey): string | null {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, '0');
  const ymd = (x: Date) => `${x.getFullYear()}-${p(x.getMonth() + 1)}-${p(x.getDate())}`;
  if (key === '7d') {
    const s = new Date(d);
    s.setDate(s.getDate() - 6);
    return ymd(s);
  }
  if (key === '30d') {
    const s = new Date(d);
    s.setDate(s.getDate() - 29);
    return ymd(s);
  }
  if (key === 'month') return `${d.getFullYear()}-${p(d.getMonth() + 1)}-01`;
  return null;
}

/** 今日日期键（本机时区 YYYY-MM-DD，与 DoubaoCheckin 的 todayKey 口径一致；
 *  每次渲染现取而非 useMemo 固化，跨零点后「今日新增额度」自动翻新到当日） */
function todayKey(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/** 豆包 · 额度看板（自账号管理页迁移）：KPI 一行 + 额度趋势图（分组/账号/时间段筛选）。
 *  数据源 = doubao_dashboard_stats（池聚合）+ doubao_checkin_results_detail（按日逐账号明细，
 *  前端按筛选条件聚合出「每日新增额度」柱 + 「累计新增额度」曲线）。
 *  分组/账号筛选复用 Trae 积分面板的 MultiSelectDropdown（chips 触发 + 面板多选勾选）：
 *  生效 uid 集 = 分组成员集 ∩ 账号勾选集（均空 = 不限），账号候选随分组筛选联动收窄。 */
export default function DoubaoQuotaDashboard() {
  const pushToast = useAppStore((s) => s.pushToast);

  const [stats, setStats] = useState<DoubaoDashboardStats | null>(null);
  const [accounts, setAccounts] = useState<DoubaoAccountView[]>([]);
  const [groups, setGroups] = useState<GroupView[]>([]);
  const [detail, setDetail] = useState<DoubaoDayDetail[]>([]);
  const [loading, setLoading] = useState(false);
  // 最近一次成功刷新时间（刷新按钮的可见反馈）
  const [refreshedAt, setRefreshedAt] = useState('');
  const [range, setRange] = useState<RangeKey>('30d');
  // 多选筛选（对齐 Trae 积分面板）：空数组 = 不限
  const [groupSel, setGroupSel] = useState<string[]>([]);
  const [accountSel, setAccountSel] = useState<string[]>([]);

  const today = todayKey();

  // 刷新并发序号（对齐 store.ts logsReqSeq 模式）：手动刷新与挂载请求并发时旧响应直接丢弃；
  // 卸载后组件销毁无需 cleanup（重新挂载是新实例、新 seqRef），seq 单调递增即可保证旧响应不落地
  const seqRef = useRef(0);
  const refresh = async () => {
    const seq = ++seqRef.current;
    setLoading(true);
    try {
      const [st, accs, gs, det] = await Promise.all([
        // stats 独立降级（审查 #3/#14）：失败保留旧值（undefined 不覆盖 setStats）+
        // 补失败 toast（读失败不得静默、不得伪装成「暂无数据」空态），不再短路
        // Promise.all 连累已成功的 accs/gs/det——KPI 保留上次数据继续可见。
        // 对齐 DoubaoOverview 的 undefined 哨兵口径；null 是 stats 的「空数据」语义，
        // 失败置 null 会把已有统计闪成空态
        api.doubao.dashboardStats().catch(() => undefined),
        // 审查 M-6：accounts/groups/detail 同款哨兵化（undefined 不覆盖）——
        // accounts 失败若落地空数组，knownUids/currentTotal=0 会把积分曲线渲染成
        // 「0 − 前值」负值跳水；groups/detail 失败落地空数组会伪装成「暂无签到
        // 数据」空态。失败一律保留上次数据
        api.doubao.accountsList().catch(() => undefined),
        api.groups.sharedList().catch(() => undefined),
        // 明细一次拉满 365 天窗口，时间段切换纯前端过滤（后端已按 365 天裁剪）
        api.doubao.resultsDetail(365).catch(() => undefined),
      ]);
      // 过期响应（已被更新请求取代或组件已卸载）：各 set 与 pushToast 前校验，避免旧数据回写/全局 toast
      if (seq !== seqRef.current) return;
      if (st !== undefined) setStats(st);
      if (accs !== undefined) setAccounts(accs);
      if (gs !== undefined) setGroups(gs);
      if (det !== undefined) setDetail(det);
      // 失败源合并为单条 toast（防多源同时失败刷屏）；按失败源是否有旧数据如实
      // 区分「已保留/暂无」，不把读失败伪装成「暂无数据」空态
      const failedNames = [
        st === undefined && '额度概览',
        accs === undefined && '账号列表',
        gs === undefined && '分组',
        det === undefined && '签到明细',
      ].filter(Boolean) as string[];
      if (failedNames.length > 0) {
        const anyRetained =
          (st === undefined && stats !== null) ||
          (accs === undefined && accounts.length > 0) ||
          (gs === undefined && groups.length > 0) ||
          (det === undefined && detail.length > 0);
        pushToast(
          'error',
          `${failedNames.join('、')}读取失败${anyRetained ? '，已保留上次数据' : '，暂无数据'}`,
        );
      }
      // 刷新可见反馈：数据无变化时也明确「已刷新到几点几分」（修复点击无反馈的观感问题）
      setRefreshedAt(new Date().toLocaleTimeString('zh-CN', { hour12: false }));
    } catch (err) {
      if (seq !== seqRef.current) return;
      pushToast('error', `读取额度看板失败：${String(err)}`);
    } finally {
      // 仅最新请求可复位 loading（过期请求不得关闭新请求的加载态）
      if (seq === seqRef.current) setLoading(false);
    }
  };

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** 账号 → 分组映射（明细里只有 uid，分组归属以账号池为准） */
  const groupOf = useMemo(() => {
    const m = new Map<string, string | null>();
    for (const a of accounts) m.set(a.user_id, a.group_id);
    return m;
  }, [accounts]);

  /** 分组下拉选项：名称带本池账号计数（Trae 面板同款「名称 (N)」） */
  const groupOptions = useMemo(
    () =>
      groups.map((g) => ({
        id: g.id,
        name: `${g.name} (${accounts.filter((a) => a.group_id === g.id).length})`,
        color: g.color,
      })),
    [groups, accounts],
  );

  /** 分组筛选生效 uid 集（仅分组，不含账号勾选；Trae 面板同款口径） */
  const groupUids = useMemo<Set<string> | null>(() => {
    if (groupSel.length === 0) return null;
    const set = new Set<string>();
    for (const [uid, gid] of groupOf) {
      if (gid && groupSel.includes(gid)) set.add(uid);
    }
    return set;
  }, [groupOf, groupSel]);

  /** 账号多选候选（随分组筛选联动收窄，不随自身勾选收窄——否则无法叠加多选） */
  const accountCandidates = useMemo(
    () =>
      accounts
        .filter((a) => !groupUids || groupUids.has(a.user_id))
        .map((a) => ({ id: a.user_id, name: a.name })),
    [accounts, groupUids],
  );

  /** 生效 uid 集 = 组筛选成员集 ∩ 账号勾选集（均空 = 不限，null） */
  const uidSel = useMemo<Set<string> | null>(() => {
    let set: Set<string> | null = null;
    if (groupUids) set = new Set(groupUids);
    if (accountSel.length > 0) {
      const f = new Set(accountSel);
      set = set ? new Set([...set].filter((u) => f.has(u))) : f;
    }
    return set;
  }, [groupUids, accountSel]);

  /** 按筛选条件聚合出趋势序列：每日新增额度（柱）+ 总额度（线）。
   *  总额度为倒推值：当日总额度 = 当前总额度 − 之后各日新增合计（额度只增口径，
   *  尾点精确对齐账号池当前真实总额度；随 uid 筛选联动取对应账号集的总额度）。
   *  倒推扣减仅统计「已知总额度账号」的 reward（明细含 uid 可按账号归属）：
   *  基线 currentTotal 只累计 quota_total 已知账号，未知额度账号的新增若也参与扣减，
   *  会把历史点系统性压低——柱保持全量口径，仅线的扣减与基线同口径。 */
  const series = useMemo(() => {
    const start = rangeStart(range);
    const days = detail.filter((d) => (start ? d.date >= start : true));
    // 已知总额度的账号集（quota_total != null），倒推扣减的口径集合
    const knownUids = new Set(accounts.filter((a) => a.quota_total != null).map((a) => a.user_id));
    const rewards = days.map((d) => {
      let reward = 0; // 柱：筛选内全部账号新增（口径不变）
      let knownReward = 0; // 线：仅已知额度账号新增（参与倒推扣减，与基线口径一致）
      for (const r of d.accounts) {
        if (uidSel && !uidSel.has(r.user_id)) continue;
        const rw = r.reward ?? 0;
        reward += rw;
        if (knownUids.has(r.user_id)) knownReward += rw;
      }
      return { date: d.date, reward, knownReward };
    });
    const currentTotal = (uidSel ? accounts.filter((a) => uidSel.has(a.user_id)) : accounts).reduce(
      (s, a) => s + (a.quota_total ?? 0),
      0,
    );
    // suffix[i] = 第 i 日及之后「已知额度账号」的新增合计（多留一位便于取「之后各日」）
    const suffix = new Array(rewards.length + 1).fill(0);
    for (let i = rewards.length - 1; i >= 0; i--) suffix[i] = suffix[i + 1] + rewards[i].knownReward;
    return rewards.map((d, i) => ({
      date: d.date.slice(5),
      reward: d.reward,
      totalQuota: currentTotal - suffix[i + 1],
    }));
  }, [detail, range, uidSel, accounts]);

  const rangeReward = series.reduce((s, p) => s + p.reward, 0);

  /** 今日新增额度：筛选生效时按 uid 集从明细重算（Trae 面板同款「KPI 跟随筛选」） */
  const todayReward = useMemo(() => {
    if (!uidSel) return stats?.today_reward ?? 0;
    const todayDetail = detail.find((d) => d.date === today);
    if (!todayDetail) return 0;
    return todayDetail.accounts.reduce((s, r) => (uidSel.has(r.user_id) ? s + (r.reward ?? 0) : s), 0);
  }, [uidSel, detail, today, stats]);

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="豆包 · 额度看板"
        desc="账号池额度汇总与签到额度趋势（签到/巡检成功后自动回写）"
      />

      {/* KPI 一行：账号数 / 总额度 / 今日新增额度（今日新增跟随筛选，Trae 面板同款） */}
      {stats && (
        <div className="mt-5 grid grid-cols-1 gap-4 sm:grid-cols-3">
          <StatCard label="账号数" value={String(stats.account_count)} hint="当前豆包账号池规模" tone="violet" />
          <StatCard
            label="总额度"
            value={stats.quota_total.toLocaleString()}
            hint={stats.quota_known_count > 0 ? `${stats.quota_known_count} 个账号已知额度` : '签到/巡检后自动回写'}
            tone="amber"
          />
          <StatCard
            label="今日新增额度"
            value={`+${todayReward.toLocaleString()}`}
            hint={
              uidSel
                ? `已按 ${uidSel.size} 个账号筛选（聚合口径未筛选时以池统计为准）`
                : stats.today_ok + stats.today_already + stats.today_failed > 0
                  ? `签到成功 ${stats.today_ok + stats.today_already} · 失败 ${stats.today_failed}`
                  : '今日尚未签到'
            }
            tone="green"
          />
        </div>
      )}

      {/* 最长连续签到徽标（后端 stats.max_continuous_days 此前零消费，此处补展示；stats 为空或无数据时隐藏） */}
      {stats && stats.max_continuous_days != null && (
        <div className="mt-2 text-xs text-slate-400">
          <Badge tone="amber">最长连续签到 {stats.max_continuous_days} 天</Badge>
        </div>
      )}

      {/* 额度趋势：右上角 分组 / 账号 / 时间段 / 刷新（筛选与 Trae 积分面板同款多选下拉） */}
      <div className="card mt-4 p-4">
        <div className="mb-3 flex flex-wrap items-center gap-2">
          <div className="flex items-center gap-2">
            <TrendingUp size={16} className="text-amber-500" />
            <span className="text-sm font-medium">额度趋势</span>
          </div>
          <div className="ml-auto flex flex-wrap items-center gap-2 text-xs">
            <MultiSelectDropdown
              label="分组"
              options={groupOptions}
              selected={groupSel}
              onChange={(ids) => {
                setGroupSel(ids);
                // 分组变化后账号候选联动收窄；已选账号若不在新候选内则清空账号筛选
                setAccountSel((prev) =>
                  prev.filter((id) => {
                    if (ids.length === 0) return true;
                    return groupOf.get(id) != null && ids.includes(groupOf.get(id)!);
                  }),
                );
              }}
            />
            <MultiSelectDropdown
              label="账号"
              options={accountCandidates}
              selected={accountSel}
              onChange={setAccountSel}
            />
            <div className="flex items-center gap-1">
              {(Object.keys(RANGE_LABEL) as RangeKey[]).map((k) => (
                <button
                  key={k}
                  onClick={() => setRange(k)}
                  className={`chip border ${range === k ? 'border-brand-500 text-brand-600' : 'border-slate-300 text-slate-500'}`}
                >
                  {RANGE_LABEL[k]}
                </button>
              ))}
            </div>
            {/* 刷新数据：紧邻时间段选择（页头按钮过远难以感知 loading） */}
            <button onClick={() => void refresh()} className="btn-outline !py-1 !text-xs" disabled={loading}>
              <RefreshCw size={13} className={loading ? 'animate-spin' : ''} /> {loading ? '刷新中…' : '刷新数据'}
            </button>
            {refreshedAt && !loading && <span className="text-[11px] text-slate-400">刷新于 {refreshedAt}</span>}
          </div>
        </div>

        <div className="mb-2 text-xs text-slate-400">
          {RANGE_LABEL[range]}新增额度合计
          <span className="ml-1 font-medium text-amber-600 dark:text-amber-400">+{rangeReward.toLocaleString()}</span>
          {series.length > 0 && <span className="ml-2">（{series[0]?.date} ~ {series[series.length - 1]?.date}）</span>}
          {uidSel && <span className="ml-2">· 已按 {uidSel.size} 个账号筛选</span>}
        </div>

        {series.length === 0 ? (
          <div className="py-16 text-center text-xs text-slate-400">
            暂无签到数据：完成一次签到或注册「每日签到」任务后生成趋势（额度巡检结果见下方快照）。
          </div>
        ) : (
          <div className="h-64 w-full">
            <ResponsiveContainer width="100%" height="100%">
              <ComposedChart data={series} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
                <CartesianGrid strokeDasharray="3 3" className="stroke-slate-200 dark:stroke-zinc-700" vertical={false} />
                <XAxis dataKey="date" tick={{ fontSize: 11 }} tickLine={false} axisLine={false} minTickGap={24} />
                {/* 双轴：左轴每日新增（绿，对齐 KPI「今日新增额度」）、右轴总额度（琥珀，对齐 KPI「总额度」） */}
                <YAxis yAxisId="reward" tick={{ fontSize: 11 }} tickLine={false} axisLine={false} width={48} />
                <YAxis yAxisId="total" orientation="right" tick={{ fontSize: 10 }} tickLine={false} axisLine={false} width={52} />
                <Tooltip
                  contentStyle={{ fontSize: 12 }}
                  formatter={(value: number | string, name: string) => [
                    Number(value).toLocaleString(),
                    name === 'reward' ? '每日新增额度' : '总额度',
                  ]}
                />
                <Legend
                  formatter={(v: string) => (v === 'reward' ? '每日新增额度' : '总额度')}
                  wrapperStyle={{ fontSize: 12 }}
                />
                <Bar yAxisId="reward" dataKey="reward" name="reward" fill="#10b981" radius={[2, 2, 0, 0]} maxBarSize={18} />
                <Line
                  yAxisId="total"
                  type="monotone"
                  dataKey="totalQuota"
                  name="totalQuota"
                  stroke="#f59e0b"
                  strokeWidth={2}
                  dot={false}
                />
              </ComposedChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>

      {/* 额度巡检快照（数据源 = 账号池巡检回写；与签到趋势解耦，无签到数据时也有最新额度面貌） */}
      {accounts.some((a) => a.quota_checked_at) && (
        <div className="card mt-4 p-4">
          <div className="mb-3 flex items-center gap-2">
            <TrendingUp size={16} className="text-violet-500" />
            <span className="text-sm font-medium">额度巡检快照</span>
            <span className="text-xs text-slate-400">来自最近一次额度巡检/手动查询（账号管理可手动刷新）</span>
          </div>
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                <tr>
                  <th className="whitespace-nowrap px-3 py-2 text-left">账号</th>
                  <th className="whitespace-nowrap px-3 py-2 text-left">套餐</th>
                  <th className="whitespace-nowrap px-3 py-2 text-left">额度状态</th>
                  <th className="whitespace-nowrap px-3 py-2 text-left">会员到期</th>
                  <th className="whitespace-nowrap px-3 py-2 text-left">最近巡检</th>
                </tr>
              </thead>
              <tbody>
                {accounts
                  .filter((a) => a.quota_checked_at)
                  .map((a) => (
                    <tr key={a.user_id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                      <td className="px-3 py-2">
                        <div className="flex items-center gap-2">
                          <span className="font-medium">{a.name}</span>
                          {a.is_current && <Badge tone="green">当前</Badge>}
                        </div>
                        <div className="font-mono text-xs text-slate-400">{a.user_id}</div>
                      </td>
                      <td className="whitespace-nowrap px-3 py-2">
                        {a.quota_level ? <Badge tone="violet">{a.quota_level}</Badge> : <span className="text-xs text-slate-400">免费</span>}
                      </td>
                      <td className="px-3 py-2 text-xs text-slate-500 dark:text-zinc-400">{a.quota_summary ?? '—'}</td>
                      <td className="whitespace-nowrap px-3 py-2 text-xs">
                        {a.quota_expire_at ?? <span className="text-slate-400">—</span>}
                      </td>
                      <td className="whitespace-nowrap px-3 py-2 text-xs text-slate-400">{a.quota_checked_at}</td>
                    </tr>
                  ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}
