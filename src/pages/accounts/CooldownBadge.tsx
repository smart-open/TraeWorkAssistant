import { Snowflake } from 'lucide-react';
import { Badge } from '../../components/ui';

const COOLDOWN_LABELS: Record<string, string> = {
  PlanLimit: '套餐限额',
  SoftRate: '限流',
  SessionDead: '会话失效',
  NotFound: '接口异常',
  Server: '服务端错误',
  Client: '客户端错误',
  BusinessError: '业务错误',
};

export function CooldownBadge({ type, until }: { type: string; until: number | null }) {
  const label = COOLDOWN_LABELS[type] ?? type;
  const isPermanent = type === 'SessionDead';
  let remaining = '';
  if (!isPermanent && until) {
    const secs = until - Math.floor(Date.now() / 1000);
    if (secs > 0) {
      const h = Math.floor(secs / 3600);
      const m = Math.floor((secs % 3600) / 60);
      remaining = h > 0 ? `${h}h${m}m` : `${m}m`;
    }
  }
  return (
    <Badge tone={isPermanent ? 'red' : 'amber'}>
      <Snowflake size={12} /> {label}{remaining && ` ${remaining}`}
    </Badge>
  );
}
