import { useEffect, useState } from 'react';
import { Snowflake } from 'lucide-react';
import { Badge } from '../../components/ui';
// 标签/剩余计算抽自 lib/cooldownTypes（与 Checkin.tsx 共享口径）；
// main 侧 #178 新增的 consecutive_errors 标签与秒级精度已并入该库
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
  // 动态倒计时（issue #178）：冷却中每秒跳字。429 限流默认仅 60s，静态渲染
  // 会停在「1m」误导用户；<1h 用秒级精度（Xm Ys / Xs），≥1h 沿用 Xh Ym 粗粒度
  const [, setTick] = useState(0);
  const label = cooldownLabel(type);
  const isPermanent = isPermanentCooldown(type);
  const guide =
    type === 'SessionDead' ? SESSION_DEAD_GUIDE : type === 'AuthRejected' ? AUTH_REJECTED_GUIDE : undefined;
  const secs = !isPermanent && until ? until - Math.floor(Date.now() / 1000) : 0;
  const ticking = secs > 0;
  useEffect(() => {
    if (!ticking) return;
    const t = setInterval(() => setTick((n) => n + 1), 1000);
    return () => clearInterval(t);
  }, [ticking]);
  const remaining = cooldownRemaining(type, until);
  return (
    <Badge tone={isPermanent ? 'red' : 'amber'} title={guide}>
      <Snowflake size={12} /> {label}{remaining && ` ${remaining}`}
    </Badge>
  );
}
