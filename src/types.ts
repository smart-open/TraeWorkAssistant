// 与 Rust 端 DTO 对齐的类型定义。注意：Tauri 命令参数默认使用 snake_case，
// 嵌套对象（CheckinOpts / LogsOpts / Settings）的字段必须保持 snake_case。

export type ViewKey =
  | 'dashboard'
  | 'accounts'
  | 'checkin'
  | 'credits'
  | 'logs'
  | 'api-service'
  | 'settings'
  // Buddy 应用页面（侧边栏应用切换 Tab → Buddy，批次1）
  | 'buddy-overview'
  | 'buddy-accounts'
  | 'buddy-checkin'
  | 'buddy-credits'
  | 'buddy-api-service'
  | 'buddy-settings'
  // Qoder 应用页面（F-80，Web 版：应用内调度，无桌面客户端逻辑）
  | 'qoder-overview'
  | 'qoder-accounts'
  | 'qoder-checkin'
  | 'qoder-credits'
  | 'qoder-api-service'
  | 'qoder-settings'
  // BuddyAI 应用页面（buddy-ai 国际版独立池；六页子导航对齐国内 Buddy，应用显示默认不展示）
  | 'buddyai-overview'
  | 'buddyai-accounts'
  | 'buddyai-checkin'
  | 'buddyai-credits'
  | 'buddyai-api-service'
  | 'buddyai-settings';

/** 侧边栏应用切换（左下角 Tab）：Trae 菜单 / Buddy 菜单 / Qoder 菜单 / BuddyAI（buddy-ai 国际版） */
export type AppKey = 'trae' | 'buddy' | 'qoder' | 'buddy-ai';

/** 各应用的默认落地页 */
export const APP_HOME_VIEW: Record<AppKey, ViewKey> = {
  trae: 'dashboard',
  buddy: 'buddy-overview',
  qoder: 'qoder-overview',
  'buddy-ai': 'buddyai-overview',
};


export interface AccountView {
  user_id: string;
  name: string;
  group_id: string | null;
  /** 共享标签 tag id 列表（account_tags 表联表填充；老后端缺省 undefined） */
  tags?: string[];
  jwt: string;
  jwt_exp_hours: number | null;
  jwt_exp_timestamp: number | null;
  checked_today: boolean | null;
  credits: number | null;
  remaining_credits: number | null;
  device_id_masked: string | null;
  cooldown_type: string | null;
  cooldown_until: number | null;
  cooldown_reason: string | null;
  has_refresh_token: boolean;
  jwt_auto_refresh: boolean;
  credits_expire_at: number | null;
  /** 通用积分（product_id != 209）剩余 */
  general_credits: number | null;
  /** Work 积分（product_id == 209）剩余 */
  work_credits: number | null;
  /** 本周期积分包总额度（credits_limit 合计；到期日历「剩余 X / 总 Y」口径） */
  total_credits?: number | null;
  /** 套餐身份（Free / Lite / Pro ...，来自 ide_user_pay_status 缓存） */
  pay_identity?: string | null;
  /** 会员套餐到期时间（Unix 秒，来自 ent_usage 会员包） */
  membership_expire?: number | null;
  /** 会员套餐下次自动续费时间（Unix 秒，无自动续费为空） */
  membership_next_billing?: number | null;
  /** refresh_token 过期时间（Unix 秒；上游未下发为 null） */
  refresh_token_expires_at?: number | null;
  /** refresh_token 连续刷新失败次数（成功清零） */
  refresh_token_fails?: number;
  /** refresh_token 是否已判定失效（连续 3 次失败或服务端明确拒绝，需重新 OAuth 登录） */
  refresh_token_invalid?: boolean;
  /** 凭证最近一次落盘时间（OAuth 登录/导入/刷新成功时更新，F-78 批次 3 收尾） */
  auth_saved_at?: string | null;
}

// ---- F-08 双应用账号自动发现 ----
/** 账号导入结果报告 */
export interface ImportReport {
  /** 文件中的账号总数 */
  total: number;
  /** 实际新增数量 */
  added: number;
  /** 跳过（重复）数量 */
  skipped: number;
  /** 跳过的账号标识（uid 或名称） */
  skipped_names: string[];
  /** 新增分组数量 */
  groups_added: number;
  /** 恢复的标签绑定条数（仅本机已存在的 tag id；老后端缺省 0） */
  tags_restored?: number;
  /** 导入文件中本机不存在的 tag id 数量（忽略不自动创建） */
  tags_ignored?: number;
}

// ---- F-46 导入预览（按索引导入）----
/** 导入预览条目；index 为文件中 accounts 数组下标 */
export interface ImportPreviewAccount {
  index: number;
  user_id: string | null;
  /** 展示名：name 字段 > uid > "(无 ID)" */
  name: string;
  has_jwt: boolean;
  group_id: string | null;
  /** uid 已存在于账号池（默认不勾选） */
  exists: boolean;
}

/** 导入预览报告 */
export interface ImportPreview {
  total: number;
  accounts: ImportPreviewAccount[];
  /** 将新增的分组 */
  new_groups: { id: string; name: string; color: string; order: number }[];
}

/** 积分明细条目（仅剩余 > 0 且未过期的积分包） */
export interface CreditPackDetail {
  /** '通用' | 'Work' */
  kind: string;
  /** 来源名称（如「每日签到」「每月登录积分」） */
  source: string;
  remaining: number;
  /** 包总额度 credits_limit（到期日历 note「剩余 X / 总 Y」） */
  total?: number;
  /** 过期时间（Unix 秒；4102444800 = 2100-01-01 长期有效哨兵） */
  expire_time: number;
}

/** 单账号积分明细 */
export interface CreditDetail {
  general: number;
  work: number;
  total: number;
  packs: CreditPackDetail[];
}

export interface GroupView {
  id: string;
  name: string;
  color: string;
  order: number;
  count: number;
  /** 组内账号 uid 列表（账号池分组筛选实时预览用，T10） */
  uids: string[];
}

/** 共享标签视图（跨池一张表；uids = 绑定该标签的账号 uid） */
export interface TagView {
  id: string;
  name: string;
  color: string;
  order: number;
  count: number;
  uids: string[];
}

export type JwtStatus = 'ok' | 'warn' | 'expired' | 'unknown';

export interface JwtParseResult {
  user_id: string | null;
  exp_hours: number | null;
  exp_timestamp: number | null;
  status: JwtStatus;
}

export interface LogLine {
  time: string;
  log_type: string;
  message: string;
}

// snake_case 必须与 Rust Settings 完全一致
export interface Settings {
  proxy_port: number;
  theme: string;
  launch_minimized: boolean;
  /** 启动静默签到：启动 60s 后对未签到账号自动执行一轮签到（T11） */
  silent_checkin: boolean;
  auto_start_proxy: boolean;
  tray: boolean;
  language: string;
  checkin_skip_checked: boolean;
  checkin_skip_expired: boolean;
  retry: number;
  notify: string;
  trae_cn_path: string | null;
  doubao_path: string | null;
  /** 豆包会话续期保活端点（null = 用 doubao.com 首页滑动续期） */
  doubao_renew_url: string | null;
  /** 豆包会员额度接口（抓包固化后填入；null = 额度查询不可用） */
  doubao_quota_url: string | null;
  /** 豆包快照可选纳入 Default/IndexedDB（对话历史等完整状态随账号迁移；体积代价大） */
  doubao_snapshot_include_idb: boolean;
  /** WorkBuddy 桌面版 exe 手动路径（环境配置页） */
  workbuddy_path: string | null;
  /** CodeBuddy 桌面版 exe 手动路径（环境配置页） */
  codebuddy_path: string | null;
  /** WorkBuddy auth 文件人工路径（默认 %LOCALAPPDATA%\CodeBuddyExtension\...\workbuddy-desktop.info） */
  wb_auth_file_path: string | null;
  data_dir: string | null;
  log_retention_days: number;
  proxy_domains: string;
  proxy_log_path: string | null;
  api_port: number;
  api_default_model: string;
  /** F-74：WorkBuddy/CodeBuddy 切换账号时自动把当前账号会话迁移到目标账号（默认关） */
  buddy_switch_migrate_chats: boolean;
  /** issue #82 T6.1：会话明细显示首条消息概述（默认开；关闭仅隐藏 UI 展示，不删存量缓存） */
  sessions_preview_enabled: boolean;
  /** WebUI 免令牌访问（安全与管理页开关，默认关）：开启后打开页面即进主界面 */
  web_auth_disabled: boolean;
  // ── 侧边栏应用显示（仅 UI 偏好：隐藏不停功能、不删数据、不影响计划任务） ──
  /** 固定应用 key：始终显示在侧边栏且不可隐藏（单选，默认 'trae'） */
  pinned_app: AppKey;
  /** 侧边栏隐藏的应用 key 列表（空 = 全部显示；固定应用即使列入也强制显示） */
  hidden_apps: AppKey[];
  /** 侧边栏应用自定义图标（应用 key → 图标名，lib/appIcons.ts 候选表；缺失/非法回退内置默认图标） */
  app_icons: Partial<Record<AppKey, string>>;
  /**
   * 侧边栏应用显示顺序（AppKey 列表，应用显示面板拖拽排序写入；空/缺省 = 内置默认顺序）。
   * 未提及的应用由 getOrderedApps 按默认顺序补在末尾——新版本新增应用对老配置天然兼容
   */
  app_order: AppKey[];
  /** Trae 签到多账号间隔秒（默认 3s，0=关闭；防上游频控） */
  trae_checkin_gap_secs: number | null;
  /** WorkBuddy 签到/成长多账号间隔秒（默认 3s，0=关闭；两轮共用） */
  wb_checkin_gap_secs: number | null;
  /** Qoder 签到多账号间隔秒（默认 3s，0=关闭） */
  qoder_checkin_gap_secs: number | null;
  // ── Qoder 调度字段（对齐 main 3.7.x；docker 后端调度走 scheduler_cfg，这些字段经
  //    settings kv patch 可持久化，settings_get 强类型暂不回传，组件侧按默认值兜底） ──
  /** Qoder 每日签到调度触发时刻 HH:MM（默认 10:15：单次覆盖 0 点签到与 10:00 登录奖励双活动） */
  qoder_checkin_hhmm: string;
  /** Qoder 积分数据同步触发时刻 HH:MM（daily 模式生效，默认 23:40） */
  qoder_credits_sync_hhmm: string;
  /** Qoder 积分数据同步旧开关（保留兼容；调度已改按 qoder_credits_sync_mode 判定） */
  qoder_credits_sync_enabled: boolean;
  /** Qoder 积分数据同步模式：off（关闭）| hourly（每小时）| daily（每日 HH:MM）| every_hours（每N小时，默认） */
  qoder_credits_sync_mode: string;
  /** Qoder 积分数据同步间隔小时数（every_hours 模式生效，默认 2，1..24） */
  qoder_credits_sync_hours: string;
  /** Qoder Token 定时续期开关（默认开：qoder-refresh 每 6 小时兜底刷新全部账号凭证） */
  qoder_token_renew_enabled: boolean;
  /**
   * BuddyAI（buddy-ai 国际版）应用 Tab 显示开关（默认 false = 不展示）。
   * 独立于 hidden_apps：存量配置 hidden_apps 已持久化为 []，借道隐藏列表无法
   * 区分「默认隐藏」与「用户显式取消隐藏」，故用 opt-in 字段；
   * 系统设置-应用显示面板勾选后生效
   */
  buddy_ai_visible: boolean;
  /** buddy-ai 每日活跃打卡调度开关（默认关，国际档显式开启制） */
  buddy_ai_checkin_enabled: boolean;
  /** buddy-ai 打卡调度时刻 HH:MM（默认 09:20） */
  buddy_ai_checkin_hhmm: string;
  /** buddy-ai 打卡多账号间隔秒（默认 3，0 = 关闭） */
  buddy_ai_checkin_gap_secs: number | null;
  /** buddy-ai 凭证兜底续期调度开关（默认开，lazy 48h——AT 到期前 48h 内才真正刷新） */
  buddy_ai_renew_enabled: boolean;
  /** buddy-ai 凭证兜底续期时刻 HH:MM（默认 10:35） */
  buddy_ai_renew_hhmm: string;
  /** buddy-ai 积分数据每日同步开关（默认开；旧键，新装由 mode 派生） */
  buddy_ai_credits_sync_enabled: boolean;
  /** buddy-ai 积分同步时刻 HH:MM（默认 23:45） */
  buddy_ai_credits_sync_hhmm: string;
  /** buddy-ai 积分同步模式：off | hourly | every_hours | every_days | daily（空 = 旧数据） */
  buddy_ai_credits_sync_mode: string;
  /** buddy-ai 积分同步间隔小时数（every_hours 模式，默认 2，1..24） */
  buddy_ai_credits_sync_hours: string;
  /** buddy-ai 积分同步间隔天数（every_days 模式，默认 1，1..30） */
  buddy_ai_credits_sync_days: string;
  /** buddy-ai 模型目录每日同步开关（默认开） */
  buddy_ai_catalog_sync_enabled: boolean;
  /** buddy-ai 模型目录同步时刻 HH:MM（默认 05:55） */
  buddy_ai_catalog_sync_hhmm: string;
}

/** 指纹清洗规则档位：wb = WB 池（workbuddy-cn / 国内档）；buddy-ai = 国际档独立表 */
export type SanitizeTier = 'wb' | 'buddy-ai';

// 池级指纹清洗开关视图（issue #158 P1-4）：仅内存热切换、重启复位默认开
// wb = wb_sanitize（wb + Trae 池共用）；qoder = qoder_sanitize（Qoder 池独立）
export interface SanitizeSwitches {
  wb: boolean;
  qoder: boolean;
}

/** 网关命中（指纹拦截/空完成影子风控）落下的指纹样本：供一键生成清洗规则 */
export interface FingerprintSample {
  /** 11128 = 指纹拦截；empty = 空完成（影子风控收尾采样） */
  kind: string;
  /** wb | trae | qoder | buddy-ai */
  pool: string;
  /** unix 秒 */
  ts: number;
  /** 样本：system="…" tools=[…] */
  sample: string;
  /** 模板命中注记 */
  note: string;
}

/** 通知渠道配置（Phase 3 T11：Bark / Server酱 / 通用 webhook，独立 kv） */
export interface NotifyConfig {
  /** 总开关：关闭时一切通知静默 */
  enabled: boolean;
  /** Bark 推送地址（形如 https://api.day.app/<key>），null = 未配置 */
  bark_url: string | null;
  /** Server酱 SendKey，null = 未配置 */
  serverchan_sendkey: string | null;
  /** 通用 webhook 地址（POST JSON），null = 未配置 */
  webhook_url: string | null;
  /** 签到完成时通知 */
  on_checkin_done: boolean;
  /** 调度任务失败时通知 */
  on_task_failed: boolean;
}

/** IP 允许列表配置（Phase 3 T12a：应用层访问控制，独立 kv） */
export interface IpAllowlistConfig {
  /** 总开关：关闭时所有来源放行 */
  enabled: boolean;
  /** 反代信任：开启后取 X-Real-IP / X-Forwarded-For 首项作为客户端 IP；直连部署必须关闭 */
  trust_proxy: boolean;
  /** 允许的 CIDR/IP 条目（如 192.168.1.0/24、10.0.0.5）；回环地址始终放行 */
  cidrs: string[];
}

/** 附加管理员令牌完整条目（T12b；token 明文仅创建响应返回一次） */
export interface AdminTokenEntry {
  id: string;
  token: string;
  label: string;
  /** 创建时间（epoch 毫秒） */
  created_at: number;
}

/** 附加管理员令牌列表项（token 掩码显示） */
export interface AdminTokenView {
  id: string;
  token_masked: string;
  label: string;
  /** 创建时间（epoch 毫秒） */
  created_at: number;
}

/** 通知发送结果：sent = 至少一渠道成功；各渠道 null=未配置 / "ok" / 失败原因 */
export interface NotifyResult {
  sent: boolean;
  reason?: string;
  bark: string | null;
  serverchan: string | null;
  webhook: string | null;
}

/** 调度任务状态项（scheduler_status）：任务定义 + 最近一次执行情况 */
export interface SchedulerTaskView {
  key: string;
  name: string;
  /** 每日触发时刻 HH:MM；every6h 模式下为固定周期描述（如「每6小时」） */
  time: string;
  /** 执行模式：daily（每日 HH:MM）| hourly（每小时）| every_hours（每 N 小时，看板同步类默认）| every6h（固定每 6 小时，时刻不可配）| off（已关闭） */
  mode?: string;
  /** 当前是否启用（用户开关 + 既有设置语义合成） */
  enabled: boolean;
  last_run_date: string | null;
  last_run_ts: number | null;
  last_fail_ts: number | null;
  last_ok: boolean | null;
  last_summary: string | null;
}

/** 调度任务配置（scheduler_config_get/set）：停用名单 + 自定义时刻，缺省 = 全部启用 + 默认时刻 */
export interface SchedulerConfig {
  disabled_tasks: string[];
  /** 自定义触发时刻（key → HH:MM）；空对象 = 全部用默认时刻 */
  task_times?: Record<string, string>;
  /** 执行模式（key → "daily" | "hourly" | "every_hours"）；仅看板同步类任务可 hourly/every_hours，缺省 = every_hours（每 2 小时） */
  task_modes?: Record<string, string>;
  /** every_hours 间隔小时数（key → 1..24，缺省 2）；仅 every_hours 模式任务生效 */
  task_hours?: Record<string, number>;
}

export interface CheckinOpts {
  scope: string;
  user_ids?: string[];
  skip_checked_in: boolean;
  skip_expired: boolean;
}

export type CheckinAccountStatus = 'pending' | 'already' | 'success' | 'fail' | 'skip';
/** 跳过原因（status=skip 时）：checked_in=已签 / expired=JWT 过期 / cooldown=冷却中 */
export type CheckinSkipReason = 'checked_in' | 'expired' | 'cooldown';

export interface CheckinAccountResult {
  index: number;
  user_id: string;
  name: string;
  status: CheckinAccountStatus;
  /** 本账号被跳过的原因（仅 status=skip） */
  skip_reason?: CheckinSkipReason | null;
  credits?: number;
  delta?: number;
  elapsed?: number;
  code?: number;
  message?: string;
  error_type?: string | null;
  cooldown_until?: number | null;
}

export interface CheckinDone {
  ok: number;
  already: number;
  failed: number;
  total?: number;
}

/** 单日签到结果趋势点（Dashboard 堆叠图，T8） */
export interface CheckinTrendPoint {
  date: string;
  ok: number;
  already: number;
  failed: number;
}

/** 设备冲突账号（issue #110 签到前预警）：与同组其他账号共用同一 device_id */
export interface DeviceConflictAccount {
  user_id: string;
  name: string;
}

/** 设备冲突组：同一 device_id 绑定 ≥2 个账号，服务端每日仅先签账号可领积分 */
export interface DeviceConflictGroup {
  device_id: string;
  accounts: DeviceConflictAccount[];
}

export interface CreditsDailySnapshot {
  date: string;
  total: number;
  earned: number;
  consumed: number;
  /** 逐账号 earned 维度（uid → 当日获得积分；issue #82 问题4-1）；
   * 缺失/空 = 老快照行无账号明细（账号筛选时该日不计入过滤后合计） */
  accounts?: Record<string, number>;
}

// ---- 统一网关（unified-api-gateway-design §3.1/§8.1）----
/** 统一模型目录来源池标记（enabled 为运行时派生，不落盘；
 *  disabled 为 L1 显式静态禁用（issue #145 渠道内单模型开关，meta.enabled=false），
 *  与运行时 enabled 徽章不同构：disabled 源不参与调度且不对外发布） */
export interface UnifiedModelSource {
  pool: 'trae' | 'buddy' | 'buddy-ai' | 'custom' | 'qoder';
  rate: number | null;
  enabled: boolean;
  /** L1 显式禁用（issue #145）：true = 该源被管理端单模型开关停用 */
  disabled: boolean;
}

/** 统一模型目录条目（api_unified_models 返回，实时聚合派生视图） */
export interface UnifiedModel {
  id: string;
  display: string;
  /** 供应商：自定义模型用户填写值优先，否则按模型名系列推断（空串 = 未知） */
  vendor: string;
  /** 实际生效倍率 = 当前调度策略命中的来源侧 */
  rate: number | null;
  /** 思考档位（双语义合并展示，仅 Buddy 池作为请求参数下发） */
  efforts: string[];
  /** Max Mode 支持（Trae 池 1M 上下文；Buddy/自定义单源恒 false） */
  max_mode: boolean;
  context_length: number | null;
  max_tokens: number | null;
  supports_image: boolean | null;
  /** L1 人工维护标记 */
  manual: boolean;
  sources: UnifiedModelSource[];
}

/** 池间调度策略（data/dispatch_policy.json；dispatch_policy_get/set） */
export interface DispatchPolicy {
  /** smart = 智能调度（默认：到期→倍率/免费→积分多）；priority = 固定优先级 */
  strategy: 'smart' | 'priority';
  /** 池优先级（priority 模式或 smart 并列时生效；取值 trae/buddy） */
  priority: string[];
  /** 模型级覆盖（键 canonical_id；显式覆盖不做智能重排） */
  per_model: Record<string, string[]>;
  /** 双源首选池不可用时按序回退 */
  fallback: boolean;
  /** /v1/models 为多源同名模型附带池前缀别名条目（trae/xxx 等，issue #90 修复③a）；默认关 */
  model_source_labels: boolean;
  /** 账号保留剩余积分（issue #91）：剩余 ≤ 该值的账号暂停调度（0 = 关闭），默认 50 */
  reserve_credits: number;
  updated_at: number;
}

/** 网关设置（data/api_gateway_settings.json；gateway_settings_get/set） */
export interface GatewaySettings {
  port: number;
  default_model: string;
  updated_at: number;
}

/** 自定义模型条目（data/custom_models.json；custom_models_list/save/remove）。
 *  OpenAI 兼容上游直通：请求模型名 canonical 命中组内 enabled 条目，
 *  同名多条 = 上游组（负载均衡轮询） */
export interface CustomModel {
  /** 稳定 id（cm-<12hex>，保存时为空则新增） */
  id: string;
  /** 请求模型名（路由键） */
  name: string;
  /** OpenAI 兼容 API 地址（如 https://api.openai.com 或含 /v1 前缀） */
  base_url: string;
  /** API Key（Bearer） */
  api_key: string;
  /** 供应商（展示用，如 OpenAI / DeepSeek / 智谱） */
  vendor: string;
  enabled: boolean;
  context_length: number;
  max_tokens: number;
  supports_image: boolean;
  /** 展示倍率（0 = 免费；支持两位小数，如 0.01） */
  rate: number;
  note: string;
  updated_at: number;
  /** 负载权重（默认 1；0 视为 1）——上游组内加权轮询用 */
  weight: number;
  /** 备胎标记（默认 false）：true = 活跃条目全部失败/熔断时才启用 */
  standby: boolean;
}

/** Trae 模型元数据 L1 覆盖层（data/trae_model_meta.json；trae_model_meta_set/clear，§6.1）
 *  嵌套字段保持 snake_case；null = 未设置，交由下层自动来源兜底（官网同步 > 内置参考 > 名称推断） */
export interface TraeModelMeta {
  label?: string | null;
  rate?: number | null;
  efforts?: string[] | null;
  context_length?: number | null;
  max_tokens?: number | null;
  supports_image?: boolean | null;
  /** 渠道内单模型开关（issue #145）：null = 默认启用；false = 禁用该模型（不路由 Trae 源） */
  enabled?: boolean | null;
}

/** Trae 长上下文开关（kv trae_long_context；trae_long_context_list）。仅 Trae 池生效，
 *  与 Qoder 上下文长度分表存储、互不影响；未列出的模型 = 跟随上游 */
export interface TraeLongContextView {
  /** 已开启长上下文的模型（canonical_id） */
  enabled: string[];
}

/** Qoder 上下文长度（kv qoder_context_length；qoder_context_length_list）。仅 Qoder 池生效，
 *  请求体下发 parameters.context_length；未列出的模型 = 跟随上游（不下发字段） */
export interface QoderContextLengthView {
  /** 已设置条目（model 为 canonical_id） */
  items: { model: string; tokens: number }[];
  /** 合法区间（token，含端点） */
  min: number;
  max: number;
}

/** 模型选项：id = 上游 config_name，label = 官方展示名 */
export interface ModelOption {
  id: string;
  label: string;
}

export interface PoolStatus {
  uid: string;
  name: string;
  credits: number | null;
  credits_expire_at: number | null;
  cooling: boolean;
  cooldown_until: number | null;
  cooldown_reason: string | null;
  /** 积分耗尽硬冷却中（次日 04:00 自动恢复探测；cooling 仅覆盖软冷却，旧后端未返回时为 undefined） */
  hard_credit?: boolean;
  disabled: boolean;
  err_count: number;
  /** 账号实时在途并发数（F-77⑤ 可观测；旧后端未返回时为 undefined） */
  inflight?: number;
  /** refresh_token 是否已判定失效（F-78 批次 3：展示「Token 失效」徽标，需重新 OAuth 登录） */
  refresh_invalid?: boolean;
}

export interface ApiPoolFile {
  /** Trae 池参与调度开关（默认开）：关闭后 Trae 目录模型不路由 Trae 池 */
  trae_enabled?: boolean;
  enabled_uids: string[];
  /** 调度策略：expire_first（默认）/ credit_first / random（T10） */
  strategy?: string;
  /** Buddy 池调度策略（取值同 strategy）；空 = 跟随 Trae 池（两池同策略） */
  wb_strategy?: string;
  /** 参与调度的分组 id 列表；空 = 不限分组（T10） */
  group_ids?: string[];
  /** WorkBuddy 上游开关（T2.1）：开启后 WB 目录模型路由到 WB 账号池 */
  wb_enabled?: boolean;
  /** Buddy 国际版（buddy-ai）独立池开关（阶段 2）：开启后 buddy-ai 档参与国际池路由；默认 false（显式开启制） */
  buddy_ai_enabled?: boolean;
  /** buddy-ai 显式会话粘性开关（§4.2，独立于国内档粘性）；默认 false */
  buddy_ai_sticky_enabled?: boolean;
  /** buddy-ai 池粘性 TTL 秒（per-pool 参数，默认 300） */
  buddy_ai_pool_sticky_ttl_secs?: number;
  /** buddy-ai 池账号并发上限（0 = 不限，默认 1） */
  buddy_ai_account_concurrency_limit?: number;
  /** buddy-ai 显式会话粘性 TTL 秒（默认 1800） */
  buddy_ai_sticky_ttl_secs?: number;
  /** buddy-ai 竞速对冲阈值毫秒（0 = 关闭，默认 8000） */
  buddy_ai_hedge_threshold_ms?: number;
  /** buddy-ai 池分组筛选：非空 = 仅所选分组的国际账号参与调度；空 = 不限分组 */
  buddy_ai_group_ids?: string[];
  /** buddy-ai 池标签筛选：与分组筛选取交集（D5）；空 = 不限标签 */
  buddy_ai_tag_ids?: string[];
  /** buddy-ai 池内调度策略（取值同 strategy）；空 = 跟随 Trae 池（同 wb_strategy 语义） */
  buddy_ai_strategy?: string;
  /** 默认深度思考（T5.3/F-62）：客户端未显式请求 reasoning_effort 时默认 high */
  wb_default_thinking?: boolean;
  /** 工具代执行（T5.5/F-64）：客户端声明 web_search 类工具时代理侧代执行 */
  wb_tool_exec?: boolean;
  /** 后台任务降级（T5.6③/F-65）：标题/摘要类短请求路由到目录最低倍率模型 */
  wb_bg_downgrade?: boolean;
  /** 长上下文降档（F-76④）：后台任务类 + 超长输入（粗估 ≥100k token）自动换 flash 档模型 */
  wb_longctx_downgrade?: boolean;
  /** 慢请求竞速对冲阈值毫秒（F-76③）：流式首字节超阈值时向第二账号发对冲请求；0 = 关闭 */
  wb_hedge_threshold_ms?: number;
  /** Buddy 池入池白名单（wb- 前缀账号 id）；空/缺省 = 全部含凭证账号自动入池 */
  wb_enabled_uids?: string[];
  /** Buddy 池分组筛选（wb_group_ids）：非空 = 仅所选分组的 WB 账号参与调度；空 = 不限分组 */
  wb_group_ids?: string[];
  /** Trae 池账号并发上限（F-77，per-pool 三参数之一）：单账号在途请求数达到上限视为 busy；0 = 不限。旧三池共用字段已退役，Trae 池按默认值 1 落地 */
  trae_account_concurrency_limit?: number;
  /** Trae 池粘性 TTL 秒（F-76②，per-pool 三参数之一）：TTL 内同会话落同一池同账号（KV cache 复用；默认 300） */
  trae_pool_sticky_ttl_secs?: number;
  /** Trae 池显式会话粘性 TTL 秒（per-pool 三参数之一）：显式 conversationId 绑定账号的有效期（默认 1800） */
  trae_sticky_ttl_secs?: number;
  /** Trae 池慢请求竞速对冲阈值毫秒：流式首字节超阈值时向第二账号发对冲请求，先出首字者胜；0 = 关闭（默认 8000） */
  trae_hedge_threshold_ms?: number;
  /** Buddy 池显式会话粘性 TTL 秒（F-76②，per-pool 三参数之一） */
  wb_sticky_ttl_secs?: number;
  /** Buddy 池账号并发上限（per-pool 三参数之一，默认 1；0 = 不限） */
  wb_account_concurrency_limit?: number;
  /** Buddy 池粘性 TTL 秒（per-pool 三参数之一，默认 300） */
  wb_pool_sticky_ttl_secs?: number;
  /** Qoder 上游开关（p3-3）：开启后 Qoder 目录模型路由到 Qoder 账号池（默认关） */
  qoder_enabled?: boolean;
  /** Qoder 竞速对冲阈值毫秒（F-80-余 v2）：首字节超阈值向第二账号发对冲请求；0 = 关闭 */
  qoder_hedge_threshold_ms?: number;
  /** Qoder 会话粘性开关（F-80-余 v2）：同会话 TTL 内绑定同一 Qoder 账号（默认关） */
  qoder_sticky_enabled?: boolean;
  /** 免费活动模型倍率覆盖（issue #111/#156，默认开）：Qwen3.8-Flash / DeepSeek-Flash 目录倍率按 0 处理 + 取号豁免零积分过滤 */
  qoder_flash_free_rate?: boolean;
  /** Qoder 池账号并发上限（per-pool 三参数之一，默认 1；0 = 不限） */
  qoder_account_concurrency_limit?: number;
  /** Qoder 池粘性 TTL 秒（per-pool 三参数之一，默认 300） */
  qoder_pool_sticky_ttl_secs?: number;
  /** Qoder 池显式会话粘性 TTL 秒（per-pool 三参数之一，默认 1800；仅会话粘性开启时生效） */
  qoder_sticky_ttl_secs?: number;
  /** Qoder 池内调度策略；空 = 跟随 Trae 池（同 wb_strategy 语义） */
  qoder_strategy?: string;
  /** Qoder 池入池白名单（qd- 账号 id）；空 = fail-open 全部含凭证账号入池 */
  qoder_enabled_uids?: string[];
  /** Qoder 池分组筛选（共享分组 id）；空 = 不限分组 */
  qoder_group_ids?: string[];
  /** Trae 池标签筛选（共享标签 id）；空 = 不限。与分组筛选取交集（D5） */
  trae_tag_ids?: string[];
  /** Buddy 池标签筛选；空 = 不限 */
  wb_tag_ids?: string[];
  /** Qoder 池标签筛选；空 = 不限 */
  qoder_tag_ids?: string[];
}

/** 用量统计计数（按模型/账号/Key 维度，T1） */
export interface UsageCounterView {
  name: string;
  requests: number;
  ok: number;
  errors: number;
  /** 网关测量的累计消耗积分（issue #82 T2.4：仅 Qoder 桶 models 非零） */
  credits: number;
}

/** 按 Key 的 token 用量 */
export interface UsageKeyTokenView {
  name: string;
  prompt_tokens: number;
  completion_tokens: number;
}

/** 单日用量统计（api_usage_stats 返回，按日期升序） */
export interface UsageDayView {
  date: string;
  total_requests: number;
  ok: number;
  errors: number;
  stream_requests: number;
  prompt_tokens: number;
  completion_tokens: number;
  /** 缓存读 tokens 累计（M4 审查修复；上游未下发时为 0） */
  cached_tokens: number;
  /** 网关测量的累计消耗积分（issue #82 T2：仅 Qoder 桶非零；上游未下发 usage 的请求不计入） */
  credits: number;
  /** 上游计费原值（original_credits，峰谷折扣前），用于对账 */
  original_credits: number;
  avg_duration_ms: number;
  /** 总耗时 P50/P95/最大值（F-76；样本不足时缺省） */
  p50_duration_ms?: number;
  p95_duration_ms?: number;
  max_duration_ms?: number;
  /** 首字延迟 TTFT（F-76①；仅流式成功请求有样本） */
  avg_ttfb_ms?: number;
  p95_ttfb_ms?: number;
  ttfb_samples?: number;
  models: UsageCounterView[];
  accounts: UsageCounterView[];
  keys: UsageCounterView[];
  key_tokens: UsageKeyTokenView[];
  /** 按模型的延迟分位（F-76①，按请求数降序） */
  model_latency?: UsageModelLatencyView[];
}

/** 按模型的延迟分位视图（F-76①：P50/P95/最大值 + TTFT 分桶） */
export interface UsageModelLatencyView {
  model: string;
  samples: number;
  p50_duration_ms?: number;
  p95_duration_ms?: number;
  max_duration_ms?: number;
  avg_ttfb_ms?: number;
  p95_ttfb_ms?: number;
}

/** API Key 条目（data/api_keys.json；daily_limit=0 表示不限，T2；F-35 子 Key 体系批次3） */
export interface ApiKeyEntry {
  id: string;
  name: string;
  key: string;
  enabled: boolean;
  daily_limit: number;
  created_at: number;
  used_date: string;
  used_today: number;
  /**
   * 限定上游账号白名单（空 = 不限）。bind_pool=""（跟随全局调度）时条目可带
   * 池前缀 `trae:`/`buddy:`（issue #30 混合白名单，按前缀分池限定）；绑定池时
   * 为对应池裸 uid；旧数据全裸 uid = 仅限定 Buddy 池
   */
  allowed_accounts: string[];
  /** 调度模式：expire_first（默认，临期优先）| dedicated（专一） */
  schedule_mode: string;
  /**
   * 专一模式绑定的上游账号 uid（空 = allowed_accounts 首个）；跟随全局调度时
   * 可带 `trae:`/`buddy:` 池前缀（专一锁定其归属池）
   */
  dedicated_account: string;
  /** 资源池绑定（issue #25）："" = 跟随全局调度 | "trae" | "buddy" | "qoder" */
  bind_pool: string;
  /** 共享分组 id 白名单（shg_ 前缀）；空 = 不通过该维度限制。运行时实时展开并入
   * 白名单，与 allowed_tags 同时勾选时取交集（D5） */
  allowed_groups?: string[];
  /** 共享标签 id 白名单（tag_ 前缀）；空 = 不通过该维度限制 */
  allowed_tags?: string[];
  /** 按日请求统计（升序，保留最近 90 天） */
  daily_stats: ApiKeyDailyStat[];
}

/** 子 Key 按日统计项（F-35） */
export interface ApiKeyDailyStat {
  date: string;
  requests: number;
}

// ---- OAuth 登录 ----
export interface OAuthLoginUrl {
  url: string;
  state: string;
  redirect_uri: string;
}

export interface OAuthLoginResult {
  user_id: string;
  name: string;
  jwt: string;
  refresh_token: string;
  has_refresh_token: boolean;
  /** 本次登录是否与已有账号同 uid（合并更新而非新增） */
  merged: boolean;
  /** 合并时被更新的已有账号名（merged=false 时为 null） */
  existing_name: string | null;
}

// API Key 数据文件视图（api_keys_list 返回：列表 + 鉴权开关）
export interface ApiKeysFileView {
  keys: ApiKeyEntry[];
  // 显式关闭鉴权：仅当无启用 Key 时生效（true=放行，默认 false=拒绝）
  auth_disabled: boolean;
}

// ---- WorkBuddy 账号池（批次1；对应 Rust workbuddy.rs WorkBuddyAccountView）----
/** Buddy 国际版（buddy-ai）账号视图（buddy_ai_accounts_list，阶段 3 分区 UI） */
export interface BuddyAiAccountView {
  /** 账号 id（池键，wb-<hash> 派生口径同国内） */
  id: string;
  /** 真实账号 uuid（JWT sub，可能为空——手工导入无 uid 时为 ''） */
  uid: string;
  nickname: string;
  note: string;
  group_id: string;
  access_token_expires_at: number | null;
  refresh_token_expires_at: number | null;
  auth_saved_at: number | null;
  needs_relogin: boolean;
  relogin_reason: string;
  /** 启停（P3）：true = 已停用——不参与打卡花名册与网关取号 */
  disabled: boolean;
  /** 积分包明细（P9：get-user-resource Accounts[] 分包解析；null = 未取数） */
  credits_packages: Array<{
    name: string;
    code: string;
    remain: number;
    size: number;
    expire_secs: number | null;
  }> | null;
  credits_balance: number | null;
  /** 积分包最近到期时刻（秒级 unix 时间戳；注意与 credits_fetched_at 的 rfc3339 串不同） */
  credits_expire_at: number | null;
  credits_fetched_at: string | null;
  /** 凭证是否存在（vault 回填后有明文 access_token 才算可用） */
  has_credential: boolean;
  /** token rec domain（恒含 .workbuddy.ai） */
  domain: string;
}

/** BuddyAI 官网按模型用量点（get-user-request-usage 聚合；镜像 WbUsageModelPoint） */
export interface BuddyAiUsageModelPoint {
  model: string;
  request_count: number;
  credit: number;
}

/** BuddyAI 全账号官网用量聚合（积分看板「官网」源；31 天零填充；镜像 WbUsageOfficialAll） */
export interface BuddyAiUsageOfficialAll {
  status: 'complete';
  source: 'official_all';
  accounts_total: number;
  accounts_ok: number;
  range_start: string;
  range_end: string;
  fetched_at_ms: number;
  request_count_total: number;
  /** 全部账号拉取失败时回退的过期聚合缓存（stale-on-error） */
  stale?: boolean;
  stale_reason?: string;
  summary: { usage_today: number; usage_7days: number; usage_this_month: number };
  daily: { date: string; usage: number; models: BuddyAiUsageModelPoint[] }[];
  /** 按模型汇总（31 天全窗口、跨账号合并；stale 旧缓存可能缺省） */
  models?: BuddyAiUsageModelPoint[];
}

export interface WorkBuddyAccountView {
  id: string;
  uid: string;
  nickname: string;
  phone_masked: string;
  edition_type: string;
  access_token_expires_at: number | null;
  refresh_token_expires_at: number | null;
  auth_saved_at: number | null;
  needs_relogin: boolean;
  relogin_reason: string;
  group_id: string;
  note: string;
  credits_balance: number | null;
  credits_fetched_at: string | null;
  /** 共享标签 tag id 列表（老后端缺省 undefined） */
  tags?: string[];
  /** 在线 = 本机 auth 文件当前生效账号 */
  is_current: boolean;
  has_credential: boolean;
  has_snapshot: boolean;
  /** 已录 CodeBuddy 快照（profiles_codebuddy/<id>/）——双端登录态分端展示 */
  has_snapshot_codebuddy: boolean;
  /** WorkBuddy 端当前账号（桥按端写入的 current_account.txt 标记） */
  is_current_workbuddy: boolean;
  /** CodeBuddy 端当前账号（桥按端写入的 current_account.txt 标记） */
  is_current_codebuddy: boolean;
}

export interface WorkBuddyScanResult {
  id: string;
  uid: string;
  nickname: string;
  edition_type: string;
  has_access_token: boolean;
  has_refresh_token: boolean;
  access_token_expires_at: number | null;
  exists: boolean;
  /** 已在账号池中（再次导入 = 更新凭证而非新增；accounts.rs 导入按此 upsert） */
  already_in_pool?: boolean;
}

export interface WorkBuddySettings {
  auto_checkin: boolean;
  keepalive_days: number;
  lazy_refresh_hours: number;
  growth_travel: boolean;
  growth_lottery: boolean;
  growth_tasks: boolean;
  /** CLI 五重防护自动轮换（F-59） */
  cli_rotate_enabled: boolean;
  cli_rotate_interval_minutes: number;
  cli_cooldown_minutes: number;
  cli_min_gap_hours: number;
  cli_min_urgency_hours: number;
  cli_active_guard_minutes: number;
  cli_min_remaining_credits: number;
  /** UI 坐标点击签到兜底（F-18）：仅手动触发，默认关闭 */
  ui_click_enabled: boolean;
  ui_click_x: number;
  ui_click_y: number;
}

/** 账号库导入结果（F-46 扩展；与后端 accounts.rs 导入返回对齐） */
export interface WbPoolImportResult {
  added: number;
  skipped: number;
  with_credentials: number;
  /** 已在池中且凭证被更新的条数 */
  updated?: number;
  /** 被拒绝的条目（id + 原因），非空时前端需提示 */
  rejected?: { id: string; reason: string }[];
}

// ---- OAuth 扫码 + 环境重置（F-50/F-14，批次3）----
/** OAuth 流程进度事件（wb-oauth-progress） */
export interface WbOauthProgress {
  stage: 'init' | 'browser' | 'polling' | 'success' | 'error';
  message: string;
  auth_url?: string | null;
}

/** OAuth 流程结果事件（wb-oauth-done） */
export interface WbOauthDone {
  ok: boolean;
  id?: string;
  nickname?: string;
  message: string;
}

/** 官方按模型用量点（get-user-request-usage） */
export interface WbUsageModelPoint {
  model: string;
  request_count: number;
  credit: number;
}

/** 官方请求用量（F-25：近 31 天窗口） */
export interface WbUsageOfficial {
  status: 'complete' | 'unavailable';
  account_id: string;
  domain: string;
  range_start: string;
  range_end: string;
  fetched_at_ms: number;
  request_count_total: number;
  /** F-59 stale-on-error：拉取失败回退的过期缓存标记 */
  stale?: boolean;
  stale_reason?: string;
  summary: { usage_today: number; usage_7days: number; usage_this_month: number };
  daily: { date: string; usage: number; models: WbUsageModelPoint[] }[];
  models: WbUsageModelPoint[];
}

/** 全账号官方用量聚合（Buddy 积分看板近 7 日消耗主数据源；31 天零填充） */
export interface WbUsageOfficialAllComplete {
  status: 'complete';
  source: 'official_all';
  accounts_total: number;
  accounts_ok: number;
  range_start: string;
  range_end: string;
  fetched_at_ms: number;
  request_count_total: number;
  /** F-59 同款 stale-on-error：全部账号拉取失败时回退的过期聚合缓存 */
  stale?: boolean;
  stale_reason?: string;
  summary: { usage_today: number; usage_7days: number; usage_this_month: number };
  daily: { date: string; usage: number }[];
  /** 按模型汇总（31 天全窗口、跨账号合并；stale 旧缓存可能缺省） */
  models?: WbUsageModelPoint[];
}

/** 用量数据空态（非错误）：无账号凭证 / 快照时序不足等冷启动阶段，reason 为引导文案 */
export interface WbUsageEmpty {
  status: 'empty';
  reason: string;
}

/** 看板组件按 main 口径直接消费 complete 形状（empty 空态经状态判断处理） */
export type WbUsageOfficialAll = WbUsageOfficialAllComplete;

// ---- Trae 官网消耗明细（usage_history；积分看板官网源 + 今日消耗 KPI）----

/** 单日聚合（date → 消耗合计 / 会话数 / 模型分布 / token 明细） */
export interface UsageDayStat {
  /** 本地自然日 YYYY-MM-DD */
  date: string;
  credits: number;
  sessions: number;
  /** 模型 → 当日消耗积分 */
  models: Record<string, number>;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
}

export interface UsageHistoryAccount {
  user_id: string;
  name: string;
  ok: boolean;
  /** 本次增量拉取失败但已沿用缓存时的说明；ok=false 时为失败/未拉取原因 */
  error: string | null;
  /** 按日期升序（缓存中全部历史） */
  daily: UsageDayStat[];
}

export interface UsageHistoryResult {
  fetched_at: number;
  /** true = 纯缓存读取（未发起网络请求） */
  cached: boolean;
  /** true = 拉取中有账号部分时段会话数超单块分页上限（该时段数据可能不完整） */
  truncated?: boolean;
  accounts: UsageHistoryAccount[];
}

// ---- Trae 逐包到期（issue #82 T5.1：expired_ents 实时查询，含已过期包；credit_packs 只含未过期可用包） ----
export interface TraeExpiredEnt {
  /** 包名（与 AccountView.credit_packs[].source 同源口径，前端按「包名+到期时间」去重） */
  name: string;
  /** 包总额度（credits_limit） */
  credits_limit: number;
  /** 过期时间（Unix 毫秒，上游原值） */
  expire_time_ms: number;
}

export interface TraeExpiredEntsAccount {
  user_id: string;
  name: string;
  ok: boolean;
  /** 单账号查询失败原因（其余账号不受影响） */
  error: string | null;
  expired: TraeExpiredEnt[];
}

export interface TraeExpiredEntsResult {
  fetched_at: number;
  accounts: TraeExpiredEntsAccount[];
}

// ---- 会话明细（Trae query_user_usage_group_by_session 会话级行，issue #82 会话明细 Tab） ----
export interface UsageSessionView {
  user_id: string;
  account_name: string;
  session_id: string;
  /** usage_time（Unix 秒） */
  ts: number;
  /** 本地自然日 YYYY-MM-DD */
  date: string;
  model: string;
  /** 会话模式（agent/chat 等；上游缺失为空串） */
  mode: string;
  /** 消耗积分（credits_float，兜底 amount_float） */
  credits: number;
  /** 消耗金额（cost_money_float；上游缺失为 0） */
  cost_money: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  /** 会话说明（user_input_preview，后端已截断 200 字符） */
  preview: string;
  /** 轮键（Qoder 展开明细行返回 chat_record_id；会话级行与 Trae/Buddy 为空） */
  record_id?: string;
}

export interface UsageSessionsResult {
  /** 缓存拉取时间（Unix 秒；0 = 从未成功拉取。Trae/Buddy 两来源已统一为秒） */
  fetched_at: number;
  /** 过滤后总行数（分页前） */
  total: number;
  /** 过滤后区间总积分（分页前） */
  credits_total: number;
  page: number;
  page_size: number;
  rows: UsageSessionView[];
  /** 区间内出现过的模型清单（筛选下拉用） */
  models: string[];
}

// ---- 本地 Token 统计（桌面版专属：扫描本机 WB/CodeBuddy 客户端会话；Web 版恒为 null）----

export interface WbTokenAgg {
  total: number;
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
  uncached_input: number;
  calls: number;
  cache_hit_rate?: number | null;
  date?: string;
  key?: string;
}

/** 本地 Token 统计（F-26：JSONL 解析合并双源，365 天窗口） */
export interface WbTokenStats {
  source: string;
  summary: WbTokenAgg;
  models: WbTokenAgg[];
  projects: WbTokenAgg[];
  daily: WbTokenAgg[];
  daily_by_model: Record<string, WbTokenAgg[]>;
  files_scanned: number;
  parse_errors: number;
  coverage_start_at: number | null;
  coverage_end_at: number | null;
  generated_at: number;
  window_days: number;
}

/** WB 每日积分快照行（wb_credits_history；credits-dashboard-plan.md §2.2 方案 B 含 earned） */
export interface WbCreditsSnapshot {
  date: string;
  ts: number;
  total_balance: number;
  /** 当日新增积分（余额差分 + 签到 reward 归并）；null/缺省 = 未统计（v2 老快照行/首日） */
  earned?: number | null;
  accounts: { user_id: string; balance: number | null }[];
}

/** 活动信息三端点聚合（F-51）：banner（公开）+ 付费类型 + 用量提醒 */
export interface WbActivityInfo {
  account_id: string | null;
  payment_type: string | null;
  dosage_notify: Record<string, unknown> | null;
  banners: { title: string; content: string; url: string; start_time: string; end_time: string }[];
  errors: string[];
  fetched_at_ms: number;
}

/** 积分用量快照回退（F-27）：官方用量不可用时的本地推导数据源 */
export interface WbUsageFallbackComplete {
  status: 'snapshot';
  snapshot_days: number;
  summary: { usage_today: number; usage_7days: number; usage_this_month: number };
  daily: { date: string; usage: number }[];
  note: string;
  fetched_at_ms: number;
}

export type WbUsageFallback = WbUsageFallbackComplete;

/** 积分趋势单日快照（wb_credits_history 差分推导；首日 earned/consumed 为 null） */
export interface WbCreditsTrendSnap {
  date: string;
  total: number | null;
  earned: number | null;
  consumed: number | null;
}

/** 积分趋势序列（积分看板三线图数据源） */
export type WbCreditsTrend = { status: 'ok'; snapshots: WbCreditsTrendSnap[] } | WbUsageEmpty;

/** 积分包（tasks/wb_credits.rs 宽容解析输出） */
export interface WbCreditPackage {
  name: string;
  remaining: number;
  total: number;
  used: number;
  end_time: string | null;
  expire_ts: number | null;
  expire_soon?: boolean;
}

export interface WbCreditAccount {
  user_id: string;
  name: string;
  ok: boolean;
  message?: string;
  balance: number | null;
  packages: WbCreditPackage[];
  source: string;
  fetched_at?: string;
}

export interface WbCreditsResult {
  ok: boolean;
  cached?: boolean;
  /** F-59 stale-on-error：全部账号刷新失败时回退的历史缓存 */
  stale?: boolean;
  /**
   * stale-while-revalidate（移植 main@36d628f）：本次返回的是过期缓存，后端已转入后台刷新；
   * 前端稍后重取一次即可拿到新值（见 BuddyOverview 的重取逻辑）。
   */
  refreshing?: boolean;
  accounts: WbCreditAccount[];
  total_balance?: number;
}

/** WorkBuddy 签到日志（90 天存储） */
export interface WbCheckinRecord {
  date: string;
  time: string;
  user_id: string;
  name: string;
  status: string;
  message: string;
  /** 签到获得积分（接口返回或前后余额差值兜底） */
  reward?: number;
}

/** WB 上游模型目录条目（Rust wb_catalog::WbModel，snake_case 对齐） */
export interface WbModelInfo {
  id: string;
  display: string;
  context_length: number;
  max_tokens: number;
  supports_image: boolean;
  supported_efforts: string[];
  effort_override: string | null;
  rate: number;
}

// ==================== Qoder（Web 版裁剪：剔除桌面端 env/cli/scan/live-logins 类型） ====================

/** 每账号稳定设备指纹（§5.10 多账号并发；machine_id 32 位 hex，入池生成永不轮换） */
export interface QoderDeviceProfile {
  machine_id: string;
  device_id: string;
  umid: string;
}

export interface QoderAccountView {
  id: string;
  uid: string;
  nickname: string;
  phone_masked: string;
  /** free | pro | pro+ | teams（userinfo 未提供时为空） */
  plan: string;
  /** 凭证来源：pat | ide_store | qoderwork_store | mitm | cli */
  credential_source: string;
  token_expires_at: number | null;
  needs_relogin: boolean;
  relogin_reason: string;
  group_id: string;
  note: string;
  credits_balance: number | null;
  credits_fetched_at: string | null;
  has_credential: boolean;
  /** 共享标签 tag id 列表（老后端缺省 undefined） */
  tags?: string[];
  /** token 种类徽标：pat | client | unknown（脱敏） */
  token_kind: string;
  /** 设备指纹徽标（machine_id 前 8 位；null = 尚未回填） */
  fingerprint: string | null;
  /** 完整设备指纹（指纹查看弹框数据源） */
  device_profile: QoderDeviceProfile | null;
}

export interface QoderSettings {
  /** 启动自动补签 + 应用内调度器启用判定（默认开） */
  auto_checkin: boolean;
}

/** 账号池导出文件（M4：aiwork-qoder-pool；include_credentials=true 时 accounts[].credential 附带凭证副本，导出文件等同密码） */
export interface QoderPoolExport {
  kind: 'aiwork-qoder-pool';
  version: number;
  exported_at: string;
  include_credentials: boolean;
  accounts: Record<string, unknown>[];
}

/** 账号池导入结果（M4，与 WbPoolImportResult 同构） */
export interface QoderPoolImportResult {
  added: number;
  updated: number;
  skipped: number;
  with_credentials: number;
  /** 被拒绝的条目（id + 原因），非空时前端需提示 */
  rejected?: { id: string; reason: string }[];
  /** F-80-余 v2 迁移提示：true = 历史明文含凭证文件，建议重新以加密格式导出归档 */
  plaintext_credentials?: boolean;
}

/** OAuth 设备流进度事件（qoder-oauth-progress，对齐 WbOauthProgress）：
 *  后端仅发 init/browser/polling 三个 stage，终态（成功/失败）走 qoder-oauth-done 事件 */
export interface QoderOauthProgress {
  stage: 'init' | 'browser' | 'polling';
  message: string;
  auth_url?: string | null;
}

/** OAuth 设备流结果事件（qoder-oauth-done，对齐 WbOauthDone） */
export interface QoderOauthDone {
  ok: boolean;
  id?: string;
  nickname?: string;
  message: string;
}

export interface QoderCheckinRecord {
  date: string;
  time: string;
  user_id: string;
  name: string;
  status: string;
  message: string;
  reward?: number;
  /** 逐活动明细（F-80-余 v2 档期日历；历史记录无此字段） */
  campaigns?: { id?: string; name?: string; kind?: string; reward?: number | null }[];
}

/** qoder-checkin-progress NDJSON 单行：逐账号结果（index 为本轮序号，1 起） */
export interface QoderCheckinLine {
  index: number;
  user_id: string;
  name: string;
  status: 'success' | 'already' | 'fail' | 'skip';
  message?: string;
  reward?: number;
}

/** 签到完成汇总（done 事件归约产物） */
export interface QoderCheckinDone {
  ok: number;
  already: number;
  failed: number;
  /** failed 中「活动未开始/不可用」的非用户可操作失败数（口径对齐 Rust 侧） */
  empty: number;
}

export interface QoderCreditPackage {
  amount: number | null;
  /** 包总量（R-11 逐包明细 limit_value；旧聚合口径无此值） */
  total?: number | null;
  expire_at: string;
  /** 包来源：plan = 订阅配额（随订阅周期重置）；bonus = 个人资源包（R-11 逐包明细）；
   *  dedicated = 专属/组织资源包（sash usage 逐包，自有到期时间）；
   *  addon = 旧聚合口径（addOnQuota 总额，随订阅周期展示） */
  source: string;
  /** 包名（dedicated 专属/组织包原生携带；其余来源无此值） */
  name?: string | null;
}

export interface QoderCreditAccount {
  user_id: string;
  name: string;
  ok: boolean;
  message?: string;
  plan_credits: number | null;
  addon_credits: number | null;
  /** 订阅周期内已消耗（userQuota.used，看板「订阅版本的资源」进度数据源） */
  plan_used?: number | null;
  addon_used?: number | null;
  /** 订阅周期到期日（qoderUsage.expiresAt → YYYY-MM-DD） */
  plan_expires_at?: string;
  total: number | null;
  packages: QoderCreditPackage[];
  /** 数据源徽标：pat | client_token | fetch_failed | none */
  source: string;
  fetched_at: string;
}

export interface QoderCreditsResult {
  ok: boolean;
  cached?: boolean;
  /** F-59 stale-on-error：全部账号刷新失败时回退的历史缓存 */
  stale?: boolean;
  stale_reason?: string;
  accounts: QoderCreditAccount[];
  total_balance?: number;
  message?: string;
}

export interface QoderCreditsSnapshot {
  date: string;
  ts: number;
  total_balance: number;
  /** 当日消耗（快照差分：前日余额 − 当日余额 + 当日签到奖励，负值记 0；首日 null） */
  consumed?: number | null;
  /** 当日获得（签到奖励合计；无签到数据 null） */
  earned?: number | null;
  accounts: { user_id: string; total: number | null }[];
}

// ---- 指纹清洗规则表（wb_template_map KV；合并语义：内置默认表始终生效，自定义表增量覆盖/追加）----
// 规则：from → to 子串改写（contains + replace，无正则；from 必填唯一）
// to=null 停用该规则（从生效表移除，原句透传）；to="" 剥离（改写为空串，删除指纹原文）
export interface SanitizeRule {
  from: string;
  to: string | null;
}

// wb_template_map_get 返回：builtin=true 表示当前生效内置默认规则、尚无自定义副本
export interface SanitizeMapView {
  templates: SanitizeRule[];
  updated_at: number | null;
  builtin?: boolean;
}

// ---- 工具名别名表（issue #150：tool_alias_map KV；Trae 池 tools 工具集指纹识别治本）----
// 请求侧 tools[].function.name / tool_choice / 历史轮 tool_calls 名 → 通用别名，
// 响应侧按逆映射回改（协议对客户端透明）。合并语义同指纹清洗：内置默认表始终
// 生效，自定义表按 from 覆盖/追加；to=null 停用该规则（原名透传）。
// 注意：别名必须请求/响应双向一致，不支持指纹清洗的 to="" 剥离语义
export interface ToolAliasMapView {
  rules: SanitizeRule[];
  updated_at: number | null;
  builtin?: boolean;
}
