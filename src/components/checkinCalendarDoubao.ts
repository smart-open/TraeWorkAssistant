/**
 * 豆包签到档期日历数据适配（CheckinCalendarCard 的数据源转换）。
 * 纯函数：doubao_checkin_results_detail（按日逐账号明细）→ CheckinCalendarDay[]。
 * 口径：success / already 均计 ok（已签 = 之前已成功，标记成功）；fail 计 fail；
 * 当日奖励合计 = 各账号 reward 之和（success 时非空）。
 */
import type { CheckinCalendarDay, CheckinCalendarEntry } from './checkinCalendarModel';
import type { DoubaoDayDetail } from '../types';

export function daysFromDoubaoDetail(detail: DoubaoDayDetail[]): CheckinCalendarDay[] {
  return detail.map((d) => {
    let ok = 0;
    let fail = 0;
    let reward = 0;
    const entries: CheckinCalendarEntry[] = d.accounts.map((r) => {
      if (r.status === 'fail') {
        fail += 1;
        // 失败原因透出（后端 message 落库）：日历明细点开即见根因
        //（风控 710010702 / 凭证过期 710012001 等可定性），不再只显示笼统「失败」
        const reason = (r.message ?? '').trim();
        return { name: r.name, status: 'fail', note: reason || undefined };
      }
      ok += 1;
      const rw = r.reward ?? 0;
      reward += rw;
      return {
        name: r.name,
        status: r.status === 'already' ? 'already' : 'success',
        note: rw > 0 ? `+${rw} 额度` : undefined,
      };
    });
    return { date: d.date, ok, fail, entries, reward: reward > 0 ? reward : undefined };
  });
}
