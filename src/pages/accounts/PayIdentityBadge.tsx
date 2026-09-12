import { Crown } from 'lucide-react';

/** 套餐身份徽标（Free / Lite / Pro ...，悬停展示说明） */
export function PayIdentityBadge({
  identity,
  expire,
  nextBilling,
}: {
  identity: string | null | undefined;
  expire?: number | null;
  nextBilling?: number | null;
}) {
  if (!identity) return null;
  const paid = identity.toLowerCase() !== 'free';
  const fmtDay = (ts: number) =>
    new Date(ts * 1000).toLocaleDateString('zh-CN', { month: 'numeric', day: 'numeric' });
  const expTip = expire
    ? `套餐到期：${new Date(expire * 1000).toLocaleDateString('zh-CN')}`
    : '';
  const billTip = nextBilling
    ? `${expTip ? '\n' : ''}下次自动续费：${new Date(nextBilling * 1000).toLocaleDateString('zh-CN')}`
    : '';
  return (
    <span
      title={expTip || billTip ? `当前订阅套餐：${identity}\n${expTip}${billTip}` : `当前订阅套餐：${identity}`}
      className={`inline-flex cursor-help items-center gap-0.5 rounded-full px-1.5 py-px text-[10px] font-semibold ${
        paid
          ? 'bg-amber-100 text-amber-700 dark:bg-amber-500/15 dark:text-amber-400'
          : 'bg-slate-100 text-slate-500 dark:bg-zinc-800 dark:text-zinc-400'
      }`}
    >
      <Crown size={9} /> {identity}
      {expire ? <span className="font-normal opacity-80">· {fmtDay(expire)}到期</span> : null}
    </span>
  );
}
