import { Ban, BatteryLow } from 'lucide-react';
import { Badge } from '../../components/ui';
import { CooldownBadge } from './CooldownBadge';
import type { PoolStatus } from '../../types';

/**
 * 账号行内「运行期」限流/冷却徽标（issue #178）。
 *
 * 数据源 = pool_status 系命令返回的内存池快照（usePoolStatus 联表按 uid 注入），
 * 与账号 View 持久表签到冷却（CooldownBadge 直连）是两套数据、并存不冲突：
 * - 硬冷却 hard_credit（积分耗尽）：红色「积分耗尽」+ 恢复倒计时（次日 04:00 探测）；
 * - 软冷却 cooling（429 限流默认 60s / PlanLimit 12h / Server 熔断等）：
 *   按冷却原因映射 CooldownBadge 标签，倒计时每秒跳字、到期随 3s 轮询自动消失；
 * - 禁用 disabled：灰色「已禁用」——账号被禁用后不参与取号，重新启用即恢复。
 * 网关未启动 / 账号不在池中 → status=undefined → 不渲染（即无运行期冷却）。
 */
export function PoolStatusBadge({ status }: { status: PoolStatus | undefined }) {
  if (!status) return null;
  // 禁用账号无冷却但完全不参与取号，此前列表中无任何徽标、用户不知为何不参与
  if (status.disabled) {
    return (
      <Badge tone="slate" title="账号已禁用：网关取号跳过该账号；在账号管理中重新启用后恢复">
        <Ban size={12} /> 已禁用
      </Badge>
    );
  }
  if (status.hard_credit) {
    const secs = status.hard_until != null ? status.hard_until - Math.floor(Date.now() / 1000) : 0;
    const remaining =
      secs > 0 ? `${Math.floor(secs / 3600)}h${Math.floor((secs % 3600) / 60)}m` : '';
    return (
      <Badge tone="red" title="积分耗尽（hard_credit）：转发侧已跳过该账号，次日 04:00 自动恢复探测">
        <BatteryLow size={12} /> 积分耗尽{remaining && ` ${remaining}`}
      </Badge>
    );
  }
  if (!status.cooling || !status.cooldown_until) return null;
  return (
    <CooldownBadge
      type={status.cooldown_reason || 'SoftRate'}
      until={status.cooldown_until}
    />
  );
}
