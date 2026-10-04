import { describe, expect, it } from 'vitest';

import {
  canShiftMonth,
  dayDotToneClass,
  daysFromQoderRecords,
  daysFromTrendPoints,
  daysFromWbRecords,
  monthBounds,
  monthCells,
  shiftMonth,
} from './checkinCalendarModel';

describe('monthCells（月份网格）', () => {
  it('2026-10：1 号为周四（lead=4），31 天，日期键补零', () => {
    const { lead, cells } = monthCells(2026, 9);
    expect(lead).toBe(4); // 2026-10-01 为周四
    expect(cells.length).toBe(31);
    expect(cells[0]).toEqual({ day: 1, date: '2026-10-01' });
    expect(cells[30]).toEqual({ day: 31, date: '2026-10-31' });
    expect(cells[14]).toEqual({ day: 15, date: '2026-10-15' });
  });

  it('闰年 2024-02 为 29 天，平年 2023-02 为 28 天', () => {
    expect(monthCells(2024, 1).cells.length).toBe(29);
    expect(monthCells(2023, 1).cells.length).toBe(28);
  });

  it('2026-03-01 为周日（lead=0，无前导空格）', () => {
    const { lead, cells } = monthCells(2026, 2);
    expect(lead).toBe(0);
    expect(cells[0]).toEqual({ day: 1, date: '2026-03-01' });
  });
});

describe('dayDotToneClass（日格状态点配色）', () => {
  it('fail=0 且 ok>0 → 绿（全部已领）', () => {
    expect(dayDotToneClass(3, 0)).toBe('bg-emerald-500');
  });
  it('fail=0 且 ok=0 → 灰（无数据，不渲染但配色约定仍可测）', () => {
    expect(dayDotToneClass(0, 0)).toBe('bg-slate-300 dark:bg-zinc-600');
  });
  it('fail>0 且 ok>0 → 琥珀（部分失败）', () => {
    expect(dayDotToneClass(2, 1)).toBe('bg-amber-500');
  });
  it('fail>0 且 ok=0 → 红（全部失败）', () => {
    expect(dayDotToneClass(0, 2)).toBe('bg-rose-500');
  });
});

describe('月份导航（90 天回看窗口）', () => {
  // 固定「今天」= 2026-10-02：90 天前 = 2026-07-04 → 边界 [2026-07, 2026-10]
  const now = new Date(2026, 9, 2, 12, 0, 0);
  const bounds = monthBounds(now);

  it('monthBounds：minM/maxM 为 0 基月份（7 月 = 6）', () => {
    expect(bounds).toEqual({ minY: 2026, minM: 6, maxY: 2026, maxM: 9 });
  });

  it('canShiftMonth：边界月禁止继续翻页，窗口内允许', () => {
    expect(canShiftMonth({ y: 2026, m: 9 }, bounds, 1)).toBe(false); // 当前月不可前进
    expect(canShiftMonth({ y: 2026, m: 9 }, bounds, -1)).toBe(true);
    expect(canShiftMonth({ y: 2026, m: 6 }, bounds, -1)).toBe(false); // 最早月不可后退
    expect(canShiftMonth({ y: 2026, m: 6 }, bounds, 1)).toBe(true);
  });

  it('shiftMonth：跨年双向归一', () => {
    expect(shiftMonth({ y: 2026, m: 0 }, -1)).toEqual({ y: 2025, m: 11 });
    expect(shiftMonth({ y: 2025, m: 11 }, 1)).toEqual({ y: 2026, m: 0 });
    expect(shiftMonth({ y: 2026, m: 3 }, 12)).toEqual({ y: 2027, m: 3 });
    expect(shiftMonth({ y: 2026, m: 3 }, -14)).toEqual({ y: 2025, m: 1 });
  });
});

describe('daysFromWbRecords（Buddy 逐账号记录 → 逐日聚合）', () => {
  it('多账号多日聚合：success/already 计入 ok，fail 计入 fail，按日分组', () => {
    const days = daysFromWbRecords([
      { date: '2026-10-01', user_id: 'wb-a', name: '账号A', status: 'success' },
      { date: '2026-10-01', user_id: 'wb-b', name: '账号B', status: 'fail', message: '网络不可达' },
      { date: '2026-10-01', user_id: 'wb-c', name: '账号C', status: 'already' },
      { date: '2026-10-02', user_id: 'wb-a', name: '账号A', status: 'success' },
    ]);
    expect(days.length).toBe(2);
    expect(days[0]).toMatchObject({ date: '2026-10-01', ok: 2, fail: 1 });
    expect(days[0].entries.length).toBe(3);
    expect(days[1]).toMatchObject({ date: '2026-10-02', ok: 1, fail: 0 });
  });

  it('note 口径：奖励优先（>0），失败行取 message，already 无奖励则无 note', () => {
    const days = daysFromWbRecords([
      { date: '2026-10-01', user_id: 'u1', name: 'A', status: 'success', reward: 100, message: '签到成功' },
      { date: '2026-10-01', user_id: 'u2', name: 'B', status: 'fail', message: '需重新登录，已跳过' },
      { date: '2026-10-01', user_id: 'u3', name: 'C', status: 'already', message: '今日已签到' },
      { date: '2026-10-01', user_id: 'u4', name: 'D', status: 'fail', reward: 5, message: 'x' },
    ]);
    const entries = days[0].entries;
    expect(entries[0].note).toBe('+100 积分'); // 奖励优先于 message
    expect(entries[1].note).toBe('需重新登录，已跳过'); // 失败原因
    expect(entries[2].note).toBeUndefined(); // already 无奖励 → 无 note
    expect(entries[3].note).toBe('+5 积分'); // 异常数据：有奖励按奖励展示
  });

  it('账号名缺失回落 user_id；未知 status 防御为 fail；空输入返回空数组', () => {
    const days = daysFromWbRecords([
      { date: '2026-10-01', user_id: 'wb-x', name: '', status: 'skip' },
    ]);
    expect(days[0].entries[0].name).toBe('wb-x');
    expect(days[0]).toMatchObject({ ok: 0, fail: 1 });
    expect(daysFromWbRecords([])).toEqual([]);
  });

  it('同日同账号多轮去重（新→旧序）：任一 success/already 即当日已领，不再计 fail', () => {
    // 用户实测场景：同日三轮记录 = already（最新）→ success(+100) → fail(no_credential)
    const days = daysFromWbRecords([
      { date: '2026-10-04', user_id: 'u1', name: 'A', status: 'already', reward: 100 },
      { date: '2026-10-04', user_id: 'u1', name: 'A', status: 'success', reward: 100, message: '签到成功' },
      { date: '2026-10-04', user_id: 'u1', name: 'A', status: 'fail', message: '无可用凭证（no_credential）' },
    ]);
    expect(days[0]).toMatchObject({ ok: 1, fail: 0 }); // 全部已领 → 日格绿点
    expect(days[0].entries.length).toBe(1); // 明细只出一行（此前 3 行）
    expect(days[0].entries[0]).toMatchObject({ name: 'A', status: 'success' });
    expect(days[0].entries[0].note).toBe('+100 积分');
  });

  it('同日同账号全失败去重：仅最新失败原因；多账号按账号计数不重复', () => {
    const days = daysFromWbRecords([
      { date: '2026-10-04', user_id: 'u1', name: 'A', status: 'fail', message: '最新失败原因' },
      { date: '2026-10-04', user_id: 'u1', name: 'A', status: 'fail', message: '旧失败原因' },
      { date: '2026-10-04', user_id: 'u2', name: 'B', status: 'success', reward: 100 },
      { date: '2026-10-04', user_id: 'u2', name: 'B', status: 'fail', message: '无可用凭证（no_credential）' },
    ]);
    // u1 全失败（1 fail）、u2 曾成功（1 ok）——此前 fail=3 导致日格误显琥珀
    expect(days[0]).toMatchObject({ ok: 1, fail: 1 });
    expect(days[0].entries.length).toBe(2);
    const a = days[0].entries.find((e) => e.name === 'A')!;
    const b = days[0].entries.find((e) => e.name === 'B')!;
    expect(a.status).toBe('fail');
    expect(a.note).toBe('最新失败原因'); // recs 新→旧，取最新一条
    expect(b.status).toBe('success');
    expect(b.note).toBe('+100 积分');
  });

  it('同日同账号跨轮奖励取最大额（already 回填与 success 同额不叠加）', () => {
    const days = daysFromWbRecords([
      { date: '2026-10-04', user_id: 'u1', name: 'A', status: 'already', reward: 100 },
      { date: '2026-10-04', user_id: 'u1', name: 'A', status: 'success', reward: 100 },
    ]);
    expect(days[0].entries[0].note).toBe('+100 积分');
  });
});

describe('daysFromTrendPoints（Trae 趋势点 → 逐日汇总行）', () => {
  it('无失败：status=success，ok = 成功 + 已签', () => {
    const days = daysFromTrendPoints([{ date: '2026-10-01', ok: 2, already: 1, failed: 0 }]);
    expect(days[0]).toMatchObject({ date: '2026-10-01', ok: 3, fail: 0 });
    expect(days[0].entries[0]).toEqual({
      name: '全账号汇总',
      status: 'success',
      note: '成功 2 · 已签 1 · 失败 0',
    });
  });

  it('有成功有失败：status=partial（日格琥珀）；全失败：status=fail（日格红）', () => {
    const [partial] = daysFromTrendPoints([{ date: '2026-10-01', ok: 1, already: 0, failed: 1 }]);
    expect(partial).toMatchObject({ ok: 1, fail: 1 });
    expect(partial.entries[0].status).toBe('partial');

    const [allFail] = daysFromTrendPoints([{ date: '2026-10-02', ok: 0, already: 0, failed: 3 }]);
    expect(allFail).toMatchObject({ ok: 0, fail: 3 });
    expect(allFail.entries[0].status).toBe('fail');
  });

  it('全 0 防御分支归 fail（趋势点不应产生，但归一语义保持「非已领」）；空输入返回空数组', () => {
    const [zero] = daysFromTrendPoints([{ date: '2026-10-03', ok: 0, already: 0, failed: 0 }]);
    expect(zero.entries[0].status).toBe('fail');
    expect(daysFromTrendPoints([])).toEqual([]);
  });
});

describe('daysFromQoderRecords（Qoder 双活动记录 → 逐日聚合）', () => {
  it('双活动状态点：跨账号逐活动聚合，全成功出双绿点；ok/fail 计数与奖励合计', () => {
    const days = daysFromQoderRecords([
      {
        date: '2026-10-01', user_id: 'u1', name: 'A', status: 'success', reward: 200,
        campaigns: [
          { id: 'c1', name: '每日签到', kind: 'success' },
          { id: 'c2', name: '登录奖励', kind: 'success' },
        ],
      },
      {
        date: '2026-10-01', user_id: 'u2', name: 'B', status: 'success', reward: 200,
        campaigns: [
          { id: 'c1', name: '每日签到', kind: 'success' },
          { id: 'c2', name: '登录奖励', kind: 'success' },
        ],
      },
    ]);
    expect(days.length).toBe(1);
    expect(days[0]).toMatchObject({ ok: 2, fail: 0, reward: 400 });
    expect(days[0].dots).toEqual(['bg-emerald-500', 'bg-emerald-500']);
  });

  it('奖励去重口径：同账号 success+already 同额不双计；仅 already 视为真实入账', () => {
    const days = daysFromQoderRecords([
      {
        date: '2026-10-01', user_id: 'u1', name: 'A', status: 'success', reward: 100,
        campaigns: [{ id: 'c1', name: '每日签到', kind: 'success' }],
      },
      {
        date: '2026-10-01', user_id: 'u1', name: 'A', status: 'already', reward: 100,
        campaigns: [{ id: 'c1', name: '每日签到', kind: 'already' }],
      },
      {
        date: '2026-10-01', user_id: 'u2', name: 'B', status: 'already', reward: 100,
        campaigns: [{ id: 'c1', name: '每日签到', kind: 'already' }],
      },
    ]);
    // u1：success 100 + already 100（同额视为重复回放）= 100；u2：仅 already = 100
    expect(days[0].reward).toBe(200);
  });

  it('auth 登录态中断：不入状态点但明细标注；失败行 note 含失败原因；部分失败出琥珀点', () => {
    const days = daysFromQoderRecords([
      {
        date: '2026-10-01', user_id: 'u1', name: 'A', status: 'fail', message: '401 未授权',
        campaigns: [
          { id: 'c1', name: '每日签到', kind: 'auth' },
          { id: 'c2', name: '登录奖励', kind: 'fail' },
        ],
      },
      {
        date: '2026-10-01', user_id: 'u2', name: 'B', status: 'success', reward: 100,
        campaigns: [{ id: 'c1', name: '每日签到', kind: 'success' }],
      },
    ]);
    expect(days[0]).toMatchObject({ ok: 1, fail: 1 });
    // 逐活动点：登录奖励全失败 → 红；每日签到 1 成功 1 auth（auth 不计入）→ 绿
    expect(days[0].dots).toContain('bg-rose-500');
    expect(days[0].dots).toContain('bg-emerald-500');
    expect(days[0].entries[0].note).toContain('401 未授权');
    expect(days[0].entries[0].note).toContain('每日签到 登录态中断');
    expect(days[0].entries[0].note).toContain('登录奖励 失败');
  });

  it('空输入返回空数组；无 campaigns 的历史记录 dots 为空（组件回退 ok/fail 单点）', () => {
    expect(daysFromQoderRecords([])).toEqual([]);
    const days = daysFromQoderRecords([
      { date: '2026-10-01', user_id: 'u1', name: 'A', status: 'already' },
    ]);
    expect(days[0].dots).toEqual([]);
    expect(dayDotToneClass(days[0].ok, days[0].fail)).toBe('bg-emerald-500');
    expect(days[0].entries[0].status).toBe('already');
  });
});
