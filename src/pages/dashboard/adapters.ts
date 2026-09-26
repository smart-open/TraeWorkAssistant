import type {
  CreditsDailySnapshot,
  UsageHistoryResult,
  WbCheckinRecord,
  WbCreditsSnapshot,
  WbUsageFallback,
  WbUsageOfficialAll,
} from '../../types';

/**
 * 三源视图统一适配层（credits-dashboard-plan.md §4.4）：
 * 各数据命令返回 → BoardPoint[] 的纯函数适配器，可单测。
 * 页内不做静默口径合并——各源覆盖窗口/粒度差异由调用方明示（§8）。
 */

/** 看板统一数据点（date = 本地自然日 YYYY-MM-DD） */
export interface BoardPoint {
  date: string;
  credits?: number;
  tokens?: number;
  calls?: number;
  models: Record<string, { credits?: number; tokens?: number; calls?: number }>;
}

/** Trae 官网消耗明细（usage_history_fetch，日 × 模型，365 天） */
export function traeUsageToPoints(usage: UsageHistoryResult | null): BoardPoint[] {
  if (!usage) return [];
  const byDate = new Map<string, BoardPoint>();
  for (const a of usage.accounts) {
    for (const d of a.daily) {
      const date = d.date.slice(0, 10);
      let p = byDate.get(date);
      if (!p) {
        p = { date, models: {} };
        byDate.set(date, p);
      }
      p.credits = (p.credits ?? 0) + d.credits;
      p.calls = (p.calls ?? 0) + d.sessions;
      for (const [model, credits] of Object.entries(d.models)) {
        const m = (p.models[model] ??= {});
        m.credits = (m.credits ?? 0) + credits;
      }
    }
  }
  return [...byDate.values()].sort((x, y) => x.date.localeCompare(y.date));
}

/** Buddy 官网用量聚合（workbuddy_usage_official_all，日合计，31 天零填充） */
export function wbOfficialAllToPoints(r: WbUsageOfficialAll | null): BoardPoint[] {
  if (!r) return [];
  return r.daily.map((d) => ({ date: d.date.slice(0, 10), credits: d.usage, models: {} }));
}

/** Buddy 快照差分回退（workbuddy_usage_fallback，日合计，365 天快照史） */
export function wbFallbackToPoints(fb: WbUsageFallback | null): BoardPoint[] {
  if (!fb) return [];
  return fb.daily.map((d) => ({ date: d.date.slice(0, 10), credits: d.usage, models: {} }));
}

/** Trae 区间「获得积分」：每日快照 earned（积分包 CycleStartTime 归日口径） */
export function traeEarnedByDate(snaps: CreditsDailySnapshot[]): Map<string, number> {
  const m = new Map<string, number>();
  for (const s of snaps) {
    if (s.earned > 0) m.set(s.date, (m.get(s.date) ?? 0) + s.earned);
  }
  return m;
}

/** Buddy 区间「获得积分」：签到日志 reward 聚合（§2.2 方案 A 口径，仅含签到新增） */
export function wbCheckinEarnedByDate(recs: WbCheckinRecord[]): Map<string, number> {
  const m = new Map<string, number>();
  for (const r of recs) {
    const reward = r.reward ?? 0;
    if (reward > 0) m.set(r.date, (m.get(r.date) ?? 0) + reward);
  }
  return m;
}

/**
 * Buddy 区间「获得积分」（§2.2 方案 B 优先口径）：快照 earned（余额差分 + 签到归并）。
 * earned 为 null/缺省（老快照行/未统计）的日期不出现在结果中，由调用方回退方案 A。
 */
export function wbSnapshotEarnedByDate(snaps: WbCreditsSnapshot[]): Map<string, number> {
  const m = new Map<string, number>();
  for (const s of snaps) {
    if (s.earned != null && s.earned > 0) m.set(s.date, (m.get(s.date) ?? 0) + s.earned);
  }
  return m;
}

/** 两张 earned 映射归并：优先取 b（快照口径），缺失日期回落 a（签到口径） */
export function mergeEarned(
  fallback: Map<string, number>,
  preferred: Map<string, number>,
): Map<string, number> {
  const m = new Map<string, number>();
  for (const [d, v] of fallback) m.set(d, v);
  for (const [d, v] of preferred) m.set(d, v);
  return m;
}
