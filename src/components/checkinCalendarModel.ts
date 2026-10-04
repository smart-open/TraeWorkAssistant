/**
 * 签到档期日历纯模型（CheckinCalendarCard 的可测内核，F-80-余 v2）。
 *
 * 全部为纯函数（无 React / DOM / invoke 依赖），单测覆盖：
 * - monthCells：月份网格（周首偏移 + 每日格日期键）
 * - dayDotToneClass：日格状态点配色（绿=全部已领 / 琥珀=部分失败 / 红=全部失败 / 灰=无数据）
 * - monthBounds / canShiftMonth / shiftMonth：90 天回看的月份导航
 * - daysFromWbRecords：Buddy 逐账号签到记录 → 逐日聚合（wb_checkin_results 表）
 * - daysFromTrendPoints：Trae 签到趋势点 → 逐日聚合（checkin_trends 命令，全账号汇总行）
 */

/** 单日明细行 */
export interface CheckinCalendarEntry {
  name: string;
  status: 'success' | 'already' | 'fail' | 'partial';
  note?: string;
}

/** 单日聚合：ok = 已领账号数（success + already），fail = 失败账号数 */
export interface CheckinCalendarDay {
  date: string; // YYYY-MM-DD
  ok: number;
  fail: number;
  entries: CheckinCalendarEntry[];
  /** 逐活动状态点配色（bg-* 类，Qoder 双活动用；缺省回退 ok/fail 单点） */
  dots?: string[];
  /** 当日奖励合计（>0 时日格底部展示 +N）；无奖励数据可省略 */
  reward?: number;
}

/** 月份网格：lead = 1 号为周几（0 = 周日，与 grid-cols-7 表头「日一二三四五六」对位） */
export function monthCells(y: number, m: number): {
  lead: number;
  cells: { day: number; date: string }[];
} {
  const first = new Date(y, m, 1);
  const daysInMonth = new Date(y, m + 1, 0).getDate();
  const cells: { day: number; date: string }[] = [];
  for (let d = 1; d <= daysInMonth; d++) {
    cells.push({
      day: d,
      date: `${y}-${String(m + 1).padStart(2, '0')}-${String(d).padStart(2, '0')}`,
    });
  }
  return { lead: first.getDay(), cells };
}

/** 日格状态点配色：fail=0 且 ok>0 → 绿；fail=0 且 ok=0 → 灰（无数据）；部分失败 → 琥珀；全失败 → 红 */
export function dayDotToneClass(ok: number, fail: number): string {
  return fail === 0
    ? ok > 0
      ? 'bg-emerald-500'
      : 'bg-slate-300 dark:bg-zinc-600'
    : ok > 0
      ? 'bg-amber-500'
      : 'bg-rose-500';
}

/** 月份边界：[now-lookbackDays, now] 所跨的年月（日历不可翻出该窗口） */
export function monthBounds(now: Date, lookbackDays = 90): {
  minY: number;
  minM: number;
  maxY: number;
  maxM: number;
} {
  const earliest = new Date(now.getTime() - lookbackDays * 86400000);
  return {
    minY: earliest.getFullYear(),
    minM: earliest.getMonth(),
    maxY: now.getFullYear(),
    maxM: now.getMonth(),
  };
}

/** 是否可向 delta（-1 上一月 / +1 下一月）翻页 */
export function canShiftMonth(
  cur: { y: number; m: number },
  bounds: { minY: number; minM: number; maxY: number; maxM: number },
  delta: number,
): boolean {
  return delta < 0
    ? cur.y > bounds.minY || cur.m > bounds.minM
    : cur.y < bounds.maxY || cur.m < bounds.maxM;
}

/** 月份推进（跨年归一：m 可为任意整数） */
export function shiftMonth(cur: { y: number; m: number }, delta: number): { y: number; m: number } {
  const nm = cur.m + delta;
  return { y: cur.y + Math.floor(nm / 12), m: ((nm % 12) + 12) % 12 };
}

/**
 * Buddy 逐账号签到记录（wb_checkin_results 表 → workbuddy_checkin_results 命令）→ 逐日聚合。
 * 结果表为纯追加（调度/手动/重试多轮并存，同日同账号可能多条），按账号取当日最终态：
 * 任一 success/already 即视为当日已领（口径对齐 BuddyCheckin.checkinStatusOf 的列表推导），
 * 仅当日全失败账号计 fail（取最新失败原因；recs 为新→旧序，首条即最新）。
 * status 归一：success/already 计入 ok，其余（fail/未知值防御）计入 fail；
 * note 取奖励（>0 时为当日最大奖励额），失败行取失败原因；账号名缺失回落 user_id。
 */
export function daysFromWbRecords(
  recs: {
    date: string;
    user_id: string;
    name: string;
    status: string;
    message?: string;
    reward?: number;
  }[],
): CheckinCalendarDay[] {
  // date → (账号键 → 该账号当日记录数组，保持新→旧序)：账号键 user_id 优先，
  // 旧记录无 user_id 时回落 name（对齐后端去重键约定）
  const byDay = new Map<string, Map<string, { date: string; user_id: string; name: string; status: string; message?: string; reward?: number }[]>>();
  for (const r of recs) {
    const accs = byDay.get(r.date) ?? new Map();
    const key = r.user_id || r.name;
    const arr = accs.get(key) ?? [];
    arr.push(r);
    accs.set(key, arr);
    byDay.set(r.date, accs);
  }
  return [...byDay.entries()].map(([date, accs]) => {
    let ok = 0;
    let fail = 0;
    const entries: CheckinCalendarEntry[] = [];
    for (const arr of accs.values()) {
      const status: CheckinCalendarEntry['status'] = arr.some((r) => r.status === 'success')
        ? 'success'
        : arr.some((r) => r.status === 'already')
          ? 'already'
          : 'fail';
      if (status === 'fail') fail += 1;
      else ok += 1;
      const reward = Math.max(...arr.map((r) => r.reward ?? 0));
      entries.push({
        name: arr.map((r) => r.name || r.user_id).find(Boolean) ?? '',
        status,
        note:
          reward > 0
            ? `+${reward} 积分`
            : status === 'fail'
              ? arr.find((r) => r.message)?.message
              : undefined,
      });
    }
    return { date, ok, fail, entries };
  });
}

/**
 * Trae 签到趋势点（checkin_trends 命令，无逐账号数据）→ 逐日聚合。
 * 每日转一条「全账号汇总」行：failed>0 且有成功 → partial；全失败 → fail；无失败 → success
 *（全 0 属防御分支：趋势点只在有记录的日子产生）。
 */
export function daysFromTrendPoints(
  pts: { date: string; ok: number; already: number; failed: number }[],
): CheckinCalendarDay[] {
  return pts.map((t) => {
    const ok = t.ok + t.already;
    const fail = t.failed;
    const status: CheckinCalendarEntry['status'] =
      fail > 0 ? (ok > 0 ? 'partial' : 'fail') : ok > 0 ? 'success' : 'fail';
    return {
      date: t.date,
      ok,
      fail,
      entries: [
        {
          name: '全账号汇总',
          status,
          note: `成功 ${t.ok} · 已签 ${t.already} · 失败 ${t.failed}`,
        },
      ],
    };
  });
}

/** Qoder 逐账号签到记录（qoder_checkin_results 表 → qoder_checkin_results 命令）*/
export interface QoderCalRecord {
  date: string;
  user_id: string;
  name: string;
  status: string;
  message?: string;
  reward?: number | null;
  campaigns?: { id?: string; name?: string; kind?: string }[];
}

const CAMPAIGN_KIND_LABEL: Record<string, string> = {
  success: '已领',
  already: '此前已领',
  fail: '失败',
};

/**
 * Qoder 逐账号记录 → 逐日聚合（双活动档期日历）：
 * - dots：逐活动跨账号聚合状态点（auth=登录态中断为瞬态条目，不入状态点）
 * - entries：逐账号明细行，note = 逐活动摘要 + 失败原因
 * - reward：逐账号两遍去重求和——success/fail 的 reward 全计；already 与真实入账
 *   同额时视为同活动重复回放不双计（与 Qoder 页「今日获得」同口径）
 */
export function daysFromQoderRecords(recs: QoderCalRecord[]): CheckinCalendarDay[] {
  const byDay = new Map<
    string,
    {
      ok: number;
      fail: number;
      entries: CheckinCalendarEntry[];
      accs: Map<string, QoderCalRecord[]>;
      rewardSum: number;
    }
  >();
  for (const r of recs) {
    const d =
      byDay.get(r.date) ??
      { ok: 0, fail: 0, entries: [] as CheckinCalendarEntry[], accs: new Map(), rewardSum: 0 };
    const status = r.status === 'success' ? 'success' : r.status === 'already' ? 'already' : 'fail';
    if (status === 'fail') d.fail += 1;
    else d.ok += 1;
    // 明细 note：逐活动摘要（auth 单独标注「登录态中断」）+ 失败原因
    const chips: string[] = [];
    for (const c of r.campaigns ?? []) {
      const label = c.name || c.id || '活动';
      if (c.kind === 'auth') {
        chips.push(`${label} 登录态中断`);
        continue;
      }
      const verb = c.kind ? CAMPAIGN_KIND_LABEL[c.kind] ?? c.kind : '';
      chips.push(`${label} ${verb}`.trim());
    }
    const note = [...(r.status === 'fail' && r.message ? [r.message] : []), ...chips]
      .filter(Boolean)
      .join(' · ');
    d.entries.push({ name: r.name || r.user_id, status, note: note || undefined });
    const arr = d.accs.get(r.user_id) ?? [];
    arr.push(r);
    d.accs.set(r.user_id, arr);
    byDay.set(r.date, d);
  }
  return [...byDay.entries()].map(([date, d]) => {
    // 奖励：逐账号去重聚合（跨账号同额的 already 不得误杀他账号真实入账）
    for (const recsA of d.accs.values()) {
      const real = recsA.filter((r) => r.status !== 'already' && r.reward != null);
      const realAmts = new Set(real.map((r) => r.reward));
      let sum = real.reduce((s, r) => s + (r.reward ?? 0), 0);
      const seenAlready = new Set<number>();
      for (const r of recsA) {
        if (r.status !== 'already' || r.reward == null) continue;
        if (realAmts.has(r.reward) || seenAlready.has(r.reward)) continue;
        seenAlready.add(r.reward);
        sum += r.reward;
      }
      d.rewardSum += sum;
    }
    // dots：逐活动跨账号聚合 → 状态点配色
    const camps = new Map<string, { ok: number; fail: number }>();
    for (const recsA of d.accs.values()) {
      for (const r of recsA) {
        for (const c of r.campaigns ?? []) {
          if (c.kind === 'auth') continue;
          const key = c.name || c.id || '活动';
          const cur = camps.get(key) ?? { ok: 0, fail: 0 };
          if (c.kind === 'success' || c.kind === 'already') cur.ok += 1;
          else if (c.kind === 'fail') cur.fail += 1;
          camps.set(key, cur);
        }
      }
    }
    const reward = Math.round(d.rewardSum * 100) / 100;
    return {
      date,
      ok: d.ok,
      fail: d.fail,
      entries: d.entries,
      dots: [...camps.values()].map((c) => dayDotToneClass(c.ok, c.fail)),
      ...(reward > 0 ? { reward } : {}),
    };
  });
}
