import { Badge } from '../../components/ui';

export function CreditsExpireBadge({ expireAt }: { expireAt: number | null }) {
  if (!expireAt) return <span className="text-xs text-slate-300">-</span>;
  const now = Math.floor(Date.now() / 1000);
  const secs = expireAt - now;
  if (secs <= 0) return <Badge tone="red">已过期</Badge>;
  const days = Math.floor(secs / 86400);
  const hours = Math.floor((secs % 86400) / 3600);
  const isUrgent = secs < 86400; // < 24h
  // 最近一条仍有剩余的积分包到期，展示距离现在的剩余时间
  const text = days > 0 ? `${days} 天后过期` : `${hours} 小时后过期`;
  const expireTime = new Date(expireAt * 1000).toLocaleString('zh-CN');
  return (
    <span
      className={`cursor-help text-xs ${isUrgent ? 'text-amber-500 font-semibold' : 'text-slate-500'}`}
      title={`最近到期积分包：${expireTime}`}
    >
      {text}
    </span>
  );
}
