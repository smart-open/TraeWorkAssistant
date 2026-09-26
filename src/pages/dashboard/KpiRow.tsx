import { StatCard } from '../../components/ui';
import { fmtCredits } from '../../lib/format';

/**
 * 积分看板 KPI 统计面板（credits-dashboard-plan.md §2）。
 * 纯展示组件：KPI 聚合在 Dashboard.tsx 完成；按 platform 单平台渲染
 * （Trae / Buddy 页各自独立看板，不再有「全部」混合口径）。
 */

/** 数据源平台维度（Tab 内部过滤用；页面级已固定为单平台，无「全部」混合口径） */
export type PlatformScope = 'trae' | 'buddy';

/** 单平台 KPI 口径（credits-dashboard-plan.md §2.1） */
export interface PlatformKpi {
  /** 账号数 */
  accounts: number;
  /** 可用积分总数 */
  totalCredits: number;
  /** 积分包总数（Trae = 积分包 + 会员包计数；Buddy = remaining > 0 的包计数） */
  packages: number;
  /** 今日新增积分 */
  todayEarned: number;
  /** 今日消耗积分 */
  todayConsumed: number;
  /** 7 天内到期（按剩余积分额度合计，非包数；已过期不计） */
  expiring7d: number;
}

export default function KpiRow({
  kpi,
  platform,
  today,
  creditsHint,
  earnedHint,
  totalHint,
  consumedHint,
}: {
  kpi: PlatformKpi;
  /** 页面所属平台（决定 hint 文案） */
  platform: 'trae' | 'buddy';
  /** 本地日期 YYYY-MM-DD（今日新增/消耗卡 hint） */
  today: string;
  /** 可用积分总数卡 hint（Buddy 缓存/stale 状态说明；无数据时缺省） */
  creditsHint?: string;
  /** 可用积分总数卡细分（Trae 通用/Work，对齐原积分看板 totalHint）；与 creditsHint 二选一生效 */
  totalHint?: string;
  /** 今日新增卡附加口径说明（如「仅含签到新增」） */
  earnedHint?: string;
  /** 今日消耗卡时效标注（Buddy stale 缓存/快照差分推导均非当日数据）；缺省显示今日日期 */
  consumedHint?: string;
}) {
  const label = platform === 'trae' ? 'Trae' : 'Buddy';
  const avg = kpi.accounts > 0 ? kpi.totalCredits / kpi.accounts : 0;
  return (
    <div className="grid grid-cols-2 gap-3 md:grid-cols-4 xl:grid-cols-7">
      <StatCard label="账号数" value={kpi.accounts} hint={`${label} ${kpi.accounts} 个账号`} tone="brand" />
      <StatCard
        label="可用积分总数"
        value={fmtCredits(kpi.totalCredits)}
        hint={creditsHint ?? totalHint}
        tone="violet"
      />
      <StatCard
        label="平均可用积分"
        value={kpi.accounts > 0 ? Math.round(avg).toLocaleString() : '0'}
        hint="平台内平均"
        tone="blue"
      />
      <StatCard
        label="积分包总数"
        value={String(kpi.packages)}
        hint={platform === 'trae' ? '积分包 + 会员包计数' : '剩余积分包（已用完不计）'}
        tone="amber"
      />
      <StatCard
        label="今日新增积分"
        value={fmtCredits(kpi.todayEarned)}
        hint={[today, earnedHint].filter(Boolean).join(' · ')}
        tone="green"
      />
      <StatCard label="今日消耗积分" value={fmtCredits(kpi.todayConsumed)} hint={consumedHint ?? today} tone="red" />
      <StatCard
        label="7 天内到期"
        value={fmtCredits(kpi.expiring7d)}
        hint="按剩余积分额度合计 · 见到期日历"
        tone={kpi.expiring7d > 0 ? 'red' : 'green'}
      />
    </div>
  );
}
