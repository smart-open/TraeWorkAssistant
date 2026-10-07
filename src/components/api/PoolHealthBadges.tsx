import { Badge } from '../ui';
import type { PoolStatus } from '../../types';

/**
 * 池账号运行时健康徽标（三池共用：Trae/Buddy/Qoder）
 *
 * 后端可观测字段：disabled（401/403 禁用）、hard_credit（积分耗尽硬冷却，
 * 次日 04:00 自动恢复探测 F-29 v1.2）、cooling + cooldown_reason（软冷却）、
 * credits / credits_expire_at（零积分 / 积分过期）、inflight（在途并发）。
 * 可选口径对齐后端 pool::selectable——任一不可选条件命中即不显示「就绪」，
 * 杜绝「界面就绪、实际取不到号」的可观测盲点。
 */

/** 冷却原因中文映射（ErrKind.as_str 大驼峰 + consecutive_errors / hard_credit 小写蛇形） */
const REASON_LABELS: Record<string, string> = {
  sessiondead: '会话失效(401)',
  forbidden: '禁止访问(403)',
  hardcredit: '积分耗尽',
  hard_credit: '积分耗尽',
  planlimit: '套餐额度用尽',
  softrate: '上游限流(429)',
  notfound: '模型不存在(404)',
  server: '上游服务错误(5xx)',
  client: '上游请求错误(4xx)',
  consecutive_errors: '连续错误熔断',
  refresh_token_invalid: '凭证失效',
};

export function cooldownReasonLabel(reason: string | null | undefined): string {
  if (!reason) return '冷却中';
  return REASON_LABELS[reason.toLowerCase()] ?? reason;
}

export function PoolHealthBadges({
  s,
  running,
  tokenInvalid = false,
}: {
  s?: PoolStatus;
  running: boolean;
  /** 凭证已失效（RefreshTokenBadge 已示，避免「已禁用」徽标重复） */
  tokenInvalid?: boolean;
}) {
  if (!s) return null;
  const nowSec = Math.floor(Date.now() / 1000);
  const creditsExhausted = s.credits != null && s.credits <= 0;
  const creditsExpired =
    s.credits_expire_at != null && s.credits_expire_at > 0 && s.credits_expire_at < nowSec;
  const selectable =
    !s.disabled && !s.cooling && !s.hard_credit && !creditsExhausted && !creditsExpired;
  return (
    <>
      {s.disabled && !tokenInvalid && <Badge tone="red">已禁用</Badge>}
      {s.hard_credit && <Badge tone="red">积分耗尽·次日04:00恢复</Badge>}
      {s.cooling && (
        <Badge tone="amber">冷却中·{cooldownReasonLabel(s.cooldown_reason)}</Badge>
      )}
      {creditsExhausted && <Badge tone="amber">零积分</Badge>}
      {creditsExpired && <Badge tone="amber">积分已过期</Badge>}
      {running &&
        selectable &&
        ((s.inflight ?? 0) > 0 ? (
          <Badge tone="amber">在途 {s.inflight}</Badge>
        ) : (
          <Badge tone="green">就绪</Badge>
        ))}
    </>
  );
}
