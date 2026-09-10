import { useMemo } from 'react';
import { CalendarClock } from 'lucide-react';
import { Badge } from './ui';

/**
 * F-13 到期日历（跨应用通用组件，批次1）：token / 积分包 / 会员三类到期统一展示。
 * 不引第三方日历库：按月分组的倒序列表 + 倒计时徽标，主题类全走 Tailwind dark:。
 */

export interface ExpiryItem {
  /** 唯一 key */
  key: string;
  /** 展示名（账号名 / 包名） */
  label: string;
  /** 类别徽标文本（如 accessToken / 积分包 / 会员） */
  kind: string;
  /** 过期时间（Unix 秒） */
  expire_ts: number | null;
  /** 附注（如剩余额度） */
  note?: string | null;
}

function daysUntil(ts: number): number {
  return Math.ceil((ts * 1000 - Date.now()) / 86400000);
}

function fmtDate(ts: number): string {
  const d = new Date(ts * 1000);
  return `${d.getFullYear()}/${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')}`;
}

function toneOf(days: number): 'red' | 'amber' | 'slate' {
  if (days <= 0) return 'red';
  if (days <= 7) return 'amber';
  return 'slate';
}

export default function ExpiryCalendar({ items, emptyHint }: { items: ExpiryItem[]; emptyHint?: string }) {
  const sorted = useMemo(
    () =>
      items
        .filter((i) => i.expire_ts != null)
        .sort((a, b) => (a.expire_ts ?? 0) - (b.expire_ts ?? 0))
        .slice(0, 30),
    [items],
  );

  if (sorted.length === 0) {
    return (
      <p className="py-6 text-center text-xs text-slate-400">
        {emptyHint ?? '暂无到期项：待账号录入凭证并完成签到/积分查询后展示。'}
      </p>
    );
  }

  return (
    <div className="space-y-2">
      {sorted.map((it) => {
        const days = daysUntil(it.expire_ts!);
        const expired = days <= 0;
        return (
          <div
            key={it.key}
            className="flex items-center gap-3 rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800"
          >
            <CalendarClock
              size={15}
              className={expired ? 'shrink-0 text-rose-500' : days <= 7 ? 'shrink-0 text-amber-500' : 'shrink-0 text-slate-400'}
            />
            <div className="min-w-0 flex-1">
              <div className="truncate font-medium text-slate-700 dark:text-zinc-200">{it.label}</div>
              {it.note && <div className="truncate text-xs text-slate-400">{it.note}</div>}
            </div>
            <Badge tone="slate">{it.kind}</Badge>
            <span className={expired ? 'text-xs font-medium text-rose-500' : 'text-xs text-slate-500'}>
              {fmtDate(it.expire_ts!)}
            </span>
            <Badge tone={toneOf(days)}>
              {expired ? '已过期' : days <= 7 ? `即将到期 ${days} 天` : `剩 ${days} 天`}
            </Badge>
          </div>
        );
      })}
    </div>
  );
}
