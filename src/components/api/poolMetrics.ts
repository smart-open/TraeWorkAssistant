/**
 * 资源总览（ResourceSummary）多池摘要指标的纯计算，抽出以便单测。
 * 池内成员口径：
 * - Trae：enabled_uids（按 AccountView.user_id 匹配）；
 * - Buddy：wb_enabled_uids 非空按名单取（按 WorkBuddyAccountView.id 匹配，fail-open 冻结）；
 *   空/缺失 = fail-open，全部含凭证账号自动入池；
 * - Qoder：分组筛选（qoder_group_ids 非空时仅纳入所选分组的账号）∩ 白名单
 *   （qoder_enabled_uids 非空按名单取，空/缺失 = fail-open 全部含凭证账号入池）。
 * 积分口径：可用积分 = 池内账号积分之和；积分总余额 = 全部账号积分之和；
 * Buddy/Qoder 侧全部账号无余额数据（credits_balance 全 null）时返回 null（上层展示「未知」）。
 */
import type { AccountView, ApiPoolFile, CustomModel, QoderAccountView, WorkBuddyAccountView } from '../../types';

export interface PoolMetrics {
  /** Trae 池内账号数（enabled_uids） */
  traePoolCount: number;
  /** Trae 池内账号通用积分之和 */
  traePoolCredits: number;
  /** Trae 账号总数 */
  traeAccountTotal: number;
  /** Trae 全部账号通用积分之和 */
  traeTotalCredits: number;
  /** Buddy 上游开关 */
  buddyEnabled: boolean;
  /** Buddy 池内账号数（白名单或 fail-open 全量含凭证账号） */
  buddyPoolCount: number;
  /** Buddy 池内账号积分之和；无任何余额数据 = null（展示「未知」） */
  buddyPoolCredits: number | null;
  /** Buddy 全部含凭证账号积分之和；无任何余额数据 = null */
  buddyTotalCredits: number | null;
  /** Buddy 账号总数 */
  buddyAccountTotal: number;
  /** Qoder 上游开关 */
  qoderEnabled: boolean;
  /** Qoder 池内账号数（分组筛选 ∩ 白名单，白名单空 = fail-open 全部含凭证账号） */
  qoderPoolCount: number;
  /** Qoder 池内账号积分之和；无任何余额数据 = null（展示「未知」） */
  qoderPoolCredits: number | null;
  /** Qoder 全部含凭证账号积分之和；无任何余额数据 = null */
  qoderTotalCredits: number | null;
  /** Qoder 账号总数 */
  qoderAccountTotal: number;
  /** 自定义模型池：启用条目数 */
  customEnabledCount: number;
  /** 自定义模型池：总条目数 */
  customModelTotal: number;
}

export function computePoolMetrics(
  pool: ApiPoolFile | null,
  accounts: AccountView[],
  wbAccounts: WorkBuddyAccountView[],
  customModels: CustomModel[],
  qoderAccounts: QoderAccountView[] = [],
): PoolMetrics {
  const traePoolUids = pool?.enabled_uids ?? [];
  const traePoolAccounts = accounts.filter((a) => traePoolUids.includes(a.user_id));

  const credAccounts = wbAccounts.filter((a) => a.has_credential);
  const wbPoolIds = pool?.wb_enabled_uids ?? [];
  const buddyPoolAccounts = wbPoolIds.length ? wbAccounts.filter((a) => wbPoolIds.includes(a.id)) : credAccounts;
  const buddyPoolKnown = buddyPoolAccounts.some((a) => a.credits_balance != null);
  const buddyTotalKnown = credAccounts.some((a) => a.credits_balance != null);

  // Qoder 池内口径（与后端 apply_pool_snapshot 装配语义一致）：分组筛选
  // （qoder_group_ids 非空时仅纳入所选分组的账号，未分组账号不参与）∩ 白名单
  // （qoder_enabled_uids 非空按名单取，空/缺失 = fail-open 全部含凭证账号入池）
  const qoderCredAccounts = qoderAccounts.filter((a) => a.has_credential);
  const qoderGroupIds = pool?.qoder_group_ids ?? [];
  const qoderPoolIds = pool?.qoder_enabled_uids ?? [];
  const qoderPoolAccounts = qoderCredAccounts.filter(
    (a) =>
      (qoderGroupIds.length === 0 || qoderGroupIds.includes(a.group_id)) &&
      (qoderPoolIds.length === 0 || qoderPoolIds.includes(a.id)),
  );
  const qoderKnown = qoderPoolAccounts.some((a) => a.credits_balance != null);
  const qoderTotalKnown = qoderCredAccounts.some((a) => a.credits_balance != null);

  return {
    traePoolCount: traePoolAccounts.length,
    traePoolCredits: traePoolAccounts.reduce((s, a) => s + (a.general_credits ?? 0), 0),
    traeAccountTotal: accounts.length,
    traeTotalCredits: accounts.reduce((s, a) => s + (a.general_credits ?? 0), 0),
    buddyEnabled: pool?.wb_enabled ?? false,
    buddyPoolCount: buddyPoolAccounts.length,
    buddyPoolCredits: buddyPoolKnown ? buddyPoolAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0) : null,
    buddyTotalCredits: buddyTotalKnown ? credAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0) : null,
    buddyAccountTotal: wbAccounts.length,
    qoderEnabled: pool?.qoder_enabled ?? false,
    qoderPoolCount: qoderPoolAccounts.length,
    qoderPoolCredits: qoderKnown ? qoderPoolAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0) : null,
    qoderTotalCredits: qoderTotalKnown ? qoderCredAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0) : null,
    qoderAccountTotal: qoderAccounts.length,
    customEnabledCount: customModels.filter((m) => m.enabled).length,
    customModelTotal: customModels.length,
  };
}
