import { AlertTriangle, Clock, ShieldOff, Save, KeyRound } from 'lucide-react';
import { Badge } from '../../components/ui';

/**
 * F-78 批次 3：refresh_token 生命周期徽标（账号管理 + API 服务账号池两处消费）。
 * 优先级：无凭证（历史导入）> 已判定失效 > 连续刷新失败 > 即将过期；无异常时若有凭证保存时间则渲染灰色时间戳。
 * missing（issue #76 痛点②）：历史导入账号无 refresh_token，无法自动刷新——
 * 灰徽标降噪（批量导入时不刷屏），title 给出恢复路径（重新 OAuth 登录按账号 id 自动更新既有账号）。
 */
export function RefreshTokenBadge({
  invalid,
  fails,
  expiresAt,
  savedAt,
  missing,
}: {
  invalid?: boolean;
  fails?: number;
  expiresAt?: number | null;
  /** 凭证最近落盘时间（OAuth 登录/刷新/导入时更新，F-78 批次 3 收尾） */
  savedAt?: string | null;
  /** 账号无 refresh_token（历史导入/手贴 JWT），无法自动刷新（issue #76 痛点②） */
  missing?: boolean;
}) {
  if (missing && !invalid) {
    return (
      <Badge
        tone="slate"
        title="该账号无 refresh_token，无法自动刷新/续期，JWT 过期后签到与查询将失败。恢复：走「添加账号」重新 OAuth 登录该账号，登录会按账号 id 自动更新既有账号的凭证"
      >
        <KeyRound size={12} /> 无 Refresh
      </Badge>
    );
  }
  if (invalid) {
    return (
      <Badge tone="red" title="refresh_token 已失效，需重新 OAuth 登录后恢复">
        <ShieldOff size={12} /> Token 失效
      </Badge>
    );
  }
  if ((fails ?? 0) > 0) {
    return (
      <Badge tone="amber" title={`refresh_token 连续刷新失败 ${fails} 次（累计 3 次判定失效）`}>
        <AlertTriangle size={12} /> 刷新失败×{fails}
      </Badge>
    );
  }
  if (expiresAt && expiresAt > 0) {
    const days = (expiresAt - Date.now() / 1000) / 86400;
    if (days <= 0) {
      return (
        <Badge tone="red" title="refresh_token 已过期，自动续期将不可用">
          <Clock size={12} /> Refresh 过期
        </Badge>
      );
    }
    if (days <= 7) {
      return (
        <Badge tone="amber" title={`refresh_token 约 ${days.toFixed(1)} 天后过期`}>
          <Clock size={12} /> Refresh {days.toFixed(0)}d
        </Badge>
      );
    }
  }
  if (savedAt) {
    const t = savedAt.replace('T', ' ');
    return (
      <span
        className="inline-flex items-center gap-0.5 text-xs text-slate-400"
        title={`凭证最近保存时间：${t}`}
      >
        <Save size={11} /> {t.slice(5, 10)}
      </span>
    );
  }
  return null;
}
