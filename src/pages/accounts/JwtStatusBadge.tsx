import { AlertTriangle, CheckCircle2, HelpCircle, XCircle } from 'lucide-react';
import { Badge } from '../../components/ui';

export function JwtStatusBadge({ hours }: { hours: number | null }) {
  if (hours === null) return <Badge tone="slate"><HelpCircle size={12} /> 未知</Badge>;
  if (hours <= 0) return <Badge tone="red"><XCircle size={12} /> 已过期</Badge>;
  if (hours <= 24) return <Badge tone="amber"><AlertTriangle size={12} /> {hours.toFixed(1)}h</Badge>;
  return <Badge tone="green"><CheckCircle2 size={12} /> {hours.toFixed(0)}h</Badge>;
}
