/**
 * 资源总览（ResourceSummary）多池摘要指标的纯计算，抽出以便单测。
 * 池内成员口径（与后端 apply_pool_snapshot 装配语义一致）：
 * - Trae：enabled_uids（按 AccountView.user_id 匹配）；
 * - Buddy：分组筛选（wb_group_ids）∩ 标签筛选（wb_tag_ids）→ 白名单
 *   （wb_enabled_uids 非空按名单取，空/缺失 = fail-open 全部含凭证账号入池）；
 * - Qoder：分组筛选（qoder_group_ids）∩ 标签筛选（qoder_tag_ids）→ 白名单
 *   （qoder_enabled_uids 非空按名单取，空/缺失 = fail-open 全部含凭证账号入池）；
 * - BuddyAI 国际：分组筛选（buddy_ai_group_ids）∩ 标签筛选（buddy_ai_tag_ids）；
 *   无独立白名单字段 = fail-open 全部含凭证账号入池（已停用账号不入池，与后端一致）。
 * 积分口径：可用积分 = 池内账号积分之和；积分总余额 = 全部账号积分之和；
 * Buddy/Qoder/BuddyAI 侧全部账号无余额数据（credits_balance 全 null）时返回 null
 * （上层展示「未知」）。
 */
import type {
  AccountView,
  ApiPoolFile,
  BuddyAiAccountView,
  CustomModel,
  QoderAccountView,
  TagView,
  WorkBuddyAccountView,
} from '../../types';

export interface PoolMetrics {
  /** Trae 池内账号数（enabled_uids ∩ 分组 ∩ 标签，对齐后端装配语义） */
  traePoolCount: number;
  /** Trae 池内账号通用积分之和 */
  traePoolCredits: number;
  /** Trae 账号总数 */
  traeAccountTotal: number;
  /** Trae 全部账号通用积分之和 */
  traeTotalCredits: number;
  /** Buddy 上游开关 */
  buddyEnabled: boolean;
  /** Buddy 池内账号数（分组 ∩ 标签 → 白名单，白名单空 = fail-open） */
  buddyPoolCount: number;
  /** Buddy 池内账号积分之和；无任何余额数据 = null（展示「未知」） */
  buddyPoolCredits: number | null;
  /** Buddy 全部含凭证账号积分之和；无任何余额数据 = null */
  buddyTotalCredits: number | null;
  /** Buddy 账号总数 */
  buddyAccountTotal: number;
  /** BuddyAI 国际池上游开关 */
  buddyAiEnabled: boolean;
  /** BuddyAI 国际池内账号数（分组 ∩ 标签筛选，空 = fail-open 全部含凭证账号） */
  buddyAiPoolCount: number;
  /** BuddyAI 国际池内账号积分之和；无任何余额数据 = null（展示「未知」） */
  buddyAiPoolCredits: number | null;
  /** BuddyAI 国际全部含凭证账号积分之和；无任何余额数据 = null */
  buddyAiTotalCredits: number | null;
  /** BuddyAI 国际账号总数 */
  buddyAiAccountTotal: number;
  /** Qoder 上游开关 */
  qoderEnabled: boolean;
  /** Qoder 池内账号数（分组 ∩ 标签 → 白名单，白名单空 = fail-open） */
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

/** 选中标签的账号 uid 并集；null = 未选标签（不限，fail-open） */
function tagUidUnion(tagIds: string[], tags: TagView[]): Set<string> | null {
  if (tagIds.length === 0) return null;
  const s = new Set<string>();
  for (const t of tags) {
    if (!tagIds.includes(t.id)) continue;
    for (const u of t.uids ?? []) s.add(u);
  }
  return s;
}

/** 分组 ∩ 标签双维过滤（tagSet null = 不限标签；groupId 为空串 = 未分组账号） */
function inOrgScope(
  groupId: string,
  groupIds: string[],
  uid: string,
  tagSet: Set<string> | null,
): boolean {
  const groupOk = groupIds.length === 0 || groupIds.includes(groupId);
  const tagOk = tagSet == null || tagSet.has(uid);
  return groupOk && tagOk;
}

export function computePoolMetrics(
  pool: ApiPoolFile | null,
  accounts: AccountView[],
  wbAccounts: WorkBuddyAccountView[],
  customModels: CustomModel[],
  qoderAccounts: QoderAccountView[] = [],
  buddyAiAccounts: BuddyAiAccountView[] = [],
  tags: TagView[] = [],
): PoolMetrics {
  // Trae 池内口径：enabled_uids ∩ 分组（group_ids）∩ 标签（trae_tag_ids）。
  // 此前仅按 enabled_uids 过滤，被分组/标签拦截的账号被计入池内，使
  // ResourceSummary「未入池账号」差额漏报（审查修复，issue #186 排障目标）。
  // group_id null（未分组）按空串比对：分组筛选激活时未分组账号不入池，
  // 与后端 membership.get(uid).map_or(false, ...) 语义一致
  const traePoolUids = pool?.enabled_uids ?? [];
  const traeGroupIds = pool?.group_ids ?? [];
  const traeTagSet = tagUidUnion(pool?.trae_tag_ids ?? [], tags);
  const traePoolAccounts = accounts
    .filter((a) => traePoolUids.includes(a.user_id))
    .filter((a) => inOrgScope(a.group_id ?? '', traeGroupIds, a.user_id, traeTagSet));

  // Buddy 池内口径：分组 ∩ 标签过滤后，白名单非空按名单取（fail-open 冻结语义：
  // 显式名单优先；空/缺失 = 过滤后全集自动入池）
  const credAccounts = wbAccounts.filter((a) => a.has_credential);
  const wbGroupIds = pool?.wb_group_ids ?? [];
  const wbTagSet = tagUidUnion(pool?.wb_tag_ids ?? [], tags);
  const wbOrgScoped = credAccounts.filter((a) => inOrgScope(a.group_id, wbGroupIds, a.id, wbTagSet));
  const wbPoolIds = pool?.wb_enabled_uids ?? [];
  const buddyPoolAccounts = wbPoolIds.length ? wbOrgScoped.filter((a) => wbPoolIds.includes(a.id)) : wbOrgScoped;
  const buddyPoolKnown = buddyPoolAccounts.some((a) => a.credits_balance != null);
  const buddyTotalKnown = credAccounts.some((a) => a.credits_balance != null);

  // Qoder 池内口径：分组 ∩ 标签 → 白名单（qoder_enabled_uids 非空按名单取，
  // 空/缺失 = fail-open 全部含凭证账号入池）
  const qoderCredAccounts = qoderAccounts.filter((a) => a.has_credential);
  const qoderGroupIds = pool?.qoder_group_ids ?? [];
  const qoderTagSet = tagUidUnion(pool?.qoder_tag_ids ?? [], tags);
  const qoderOrgScoped = qoderCredAccounts.filter((a) =>
    inOrgScope(a.group_id, qoderGroupIds, a.id, qoderTagSet),
  );
  const qoderPoolIds = pool?.qoder_enabled_uids ?? [];
  const qoderPoolAccounts = qoderPoolIds.length
    ? qoderOrgScoped.filter((a) => qoderPoolIds.includes(a.id))
    : qoderOrgScoped;
  const qoderKnown = qoderPoolAccounts.some((a) => a.credits_balance != null);
  const qoderTotalKnown = qoderCredAccounts.some((a) => a.credits_balance != null);

  // BuddyAI 国际池内口径：分组 ∩ 标签（无独立白名单字段 = fail-open 全部含
  // 凭证账号入池）；已停用（disabled）账号不入网关池，与后端装配一致不计入
  const buddyAiCredAccounts = buddyAiAccounts.filter((a) => !a.disabled && a.has_credential);
  const buddyAiGroupIds = pool?.buddy_ai_group_ids ?? [];
  const buddyAiTagSet = tagUidUnion(pool?.buddy_ai_tag_ids ?? [], tags);
  const buddyAiPoolAccounts = buddyAiCredAccounts.filter((a) =>
    inOrgScope(a.group_id, buddyAiGroupIds, a.id, buddyAiTagSet),
  );
  const buddyAiPoolKnown = buddyAiPoolAccounts.some((a) => a.credits_balance != null);
  const buddyAiTotalKnown = buddyAiCredAccounts.some((a) => a.credits_balance != null);

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
    buddyAiEnabled: pool?.buddy_ai_enabled ?? false,
    buddyAiPoolCount: buddyAiPoolAccounts.length,
    buddyAiPoolCredits: buddyAiPoolKnown
      ? buddyAiPoolAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0)
      : null,
    buddyAiTotalCredits: buddyAiTotalKnown
      ? buddyAiCredAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0)
      : null,
    buddyAiAccountTotal: buddyAiAccounts.length,
    qoderEnabled: pool?.qoder_enabled ?? false,
    qoderPoolCount: qoderPoolAccounts.length,
    qoderPoolCredits: qoderKnown ? qoderPoolAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0) : null,
    qoderTotalCredits: qoderTotalKnown ? qoderCredAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0) : null,
    qoderAccountTotal: qoderAccounts.length,
    customEnabledCount: customModels.filter((m) => m.enabled).length,
    customModelTotal: customModels.length,
  };
}
