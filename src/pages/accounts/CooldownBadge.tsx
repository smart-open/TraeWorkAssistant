import { useEffect, useState } from 'react';
import { Snowflake } from 'lucide-react';
import { Badge } from '../../components/ui';
import { cooldownLabel, cooldownRemaining, isPermanentCooldown } from '../../lib/cooldownTypes';

/** SessionDead（JWT 被服务端吊销）的恢复指引：重登 → 保存登录态 → 续期捕获新凭证 */
const SESSION_DEAD_GUIDE =
  '该账号的登录已失效（JWT 被服务端吊销，常见于账号在别处重新登录或触发风控）。' +
  '恢复步骤：在 TRAE 中重新登录该账号 → 点击「保存当前登录态」保留新登录 → ' +
  '开启代理后点击右侧「续期 JWT」重新捕获凭证（新凭证捕获成功会自动解除失效标记），签到即可恢复。';

/** AuthRejected（服务端拒绝认证，issue #128）的恢复指引 */
const AUTH_REJECTED_GUIDE =
  '该账号的登录凭证被 Trae 服务端拒绝认证（服务端侧认证失败，与工具版本升级无关，自动重试无法恢复）。' +
  '临时方案：在「账号管理」对该账号重新执行一次 OAuth 登录，新凭证生效后签到自动恢复。';

export function CooldownBadge({ type, until }: { type: string; until: number | null }) {
  // 分钟级时钟驱动（审查修复 L）：停留页面时剩余时长不再冻结在首帧值
  const [, setTick] = useState(0);
  useEffect(() => {
    if (isPermanentCooldown(type) || !until) return;
    const timer = setInterval(() => setTick((n) => n + 1), 30_000);
    return () => clearInterval(timer);
  }, [type, until]);
  const label = cooldownLabel(type);
  const isPermanent = isPermanentCooldown(type);
  const guide =
    type === 'SessionDead' ? SESSION_DEAD_GUIDE : type === 'AuthRejected' ? AUTH_REJECTED_GUIDE : undefined;
  const remaining = cooldownRemaining(type, until);
  return (
    <Badge tone={isPermanent ? 'red' : 'amber'} title={guide}>
      <Snowflake size={12} /> {label}{remaining && ` ${remaining}`}
    </Badge>
  );
}
