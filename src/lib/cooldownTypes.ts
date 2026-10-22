/**
 * 冷却类型共享口径（审查修复：原在 CooldownBadge.tsx 与 Checkin.tsx 双份维护，
 * 新增冷却类型（如 AuthRejected/DeviceSigned）容易漏改一处）。
 * 标签全集与后端 classify_error / save_cooldown 写入的 error_type 对齐
 *（src-tauri/src/tasks/trae_checkin.rs），Unknown 为后端兜底类型。
 */
export const COOLDOWN_LABELS: Record<string, string> = {
  PlanLimit: '套餐限额',
  SoftRate: '限流',
  SessionDead: '登录已失效',
  NotFound: '接口异常',
  Server: '服务端错误',
  Client: '客户端错误',
  BusinessError: '业务错误',
  DeviceSigned: '设备已签',
  AuthRejected: '认证被拒',
  Unknown: '未知错误',
};

/** 冷却类型展示标签：未知类型回退原文（后端兜底 Unknown 已收录） */
export const cooldownLabel = (type: string): string => COOLDOWN_LABELS[type] ?? type;

/** 永久冷却判定：SessionDead（JWT 吊销）与 AuthRejected（服务端拒绝认证，issue #128） */
export const isPermanentCooldown = (type: string): boolean =>
  type === 'SessionDead' || type === 'AuthRejected';

/** 临时冷却剩余展示文案（分钟级精度）；永久冷却或已到期返回空串 */
export const cooldownRemaining = (type: string, until: number | null): string => {
  if (isPermanentCooldown(type) || !until) return '';
  const secs = until - Math.floor(Date.now() / 1000);
  if (secs <= 0) return '';
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return h > 0 ? `${h}h${m}m` : `${m}m`;
};
