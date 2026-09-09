// 与 Rust 端 DTO 对齐的类型定义。注意：Tauri 命令参数默认使用 snake_case，
// 嵌套对象（CheckinOpts / LogsOpts / Settings）的字段必须保持 snake_case。

export type ViewKey =
  | 'dashboard'
  | 'accounts'
  | 'checkin'
  | 'credits'
  | 'logs'
  | 'api-service'
  | 'settings';

export interface EnvStatus {
  installed: boolean;
  running: boolean;
  version: string | null;
  path: string | null;
}

export interface ProxyStatus {
  running: boolean;
  port: number;
  captured: number;
  started_at: number | null;
}

export interface AccountView {
  user_id: string;
  name: string;
  group_id: string | null;
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
  /** 套餐身份（Free / Lite / Pro ...，来自 ide_user_pay_status 缓存） */
  pay_identity?: string | null;
  /** 会员套餐到期时间（Unix 秒，来自 ent_usage 会员包） */
  membership_expire?: number | null;
  /** 会员套餐下次自动续费时间（Unix 秒，无自动续费为空） */
  membership_next_billing?: number | null;
}

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
}

export interface DiscoveredAccount {
  user_id: string;
  /** uid 是否经本机使用证据确认（false 时本机存在多个候选账号，不可入池） */
  uid_confident: boolean;
  /** TraeWork */
  app: string;
  app_label: string;
  in_pool: boolean;
  storage_path: string;
}

/** 本机 Trae Work 套餐信息（storage.json 明文，零 API） */
export interface AppEntitlement {
  identity_str: string | null;
  identity: number | null;
  last_sync_time: number | null;
}

/** 积分明细条目（仅剩余 > 0 且未过期的积分包） */
export interface CreditPackDetail {
  /** '通用' | 'Work' */
  kind: string;
  /** 来源名称（如「每日签到」「每月登录积分」） */
  source: string;
  remaining: number;
  /** 过期时间（Unix 秒） */
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
  /** 组内账号 uid 列表（账号池分组筛选实时预览用） */
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
  /** 启动静默签到：启动 60s 后对未签到账号自动执行一轮签到 */
  silent_checkin: boolean;
  auto_start_proxy: boolean;
  tray: boolean;
  language: string;
  checkin_skip_checked: boolean;
  checkin_skip_expired: boolean;
  retry: number;
  notify: string;
  trae_path: string | null;
  data_dir: string | null;
  log_retention_days: number;
  proxy_domains: string;
  proxy_log_path: string | null;
  api_port: number;
  api_default_model: string;
}

export interface CheckinOpts {
  scope: string;
  user_ids?: string[];
  skip_checked_in: boolean;
  skip_expired: boolean;
}

export type CheckinAccountStatus = 'pending' | 'already' | 'success' | 'fail';

export interface CheckinAccountResult {
  index: number;
  user_id: string;
  name: string;
  status: CheckinAccountStatus;
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

/** 单日签到结果趋势点（Dashboard 堆叠图） */
export interface CheckinTrendPoint {
  date: string;
  ok: number;
  already: number;
  failed: number;
}

export interface CreditRecord {
  date: string;
  user_id: string;
  credits: number;
  delta: number;
}

export interface CreditsDailySnapshot {
  date: string;
  total: number;
  earned: number;
  consumed: number;
}

export interface ProxyLogEntry {
  id: string;
  timestamp: string;
  method: string;
  host: string;
  path: string;
  status: string;
  size: number;
  sse_model?: string;
  sse_tokens?: string;
}

export interface ProxyLogListResult {
  entries: ProxyLogEntry[];
  total: number;
}

export interface ApiServiceStatus {
  running: boolean;
  port: number;
  total_requests: number;
  active_uid: string | null;
  last_error: string | null;
  started_at: number | null;
}

/** 模型选项：id = 上游 config_name，label = 官方展示名 */
export interface ModelOption {
  id: string;
  label: string;
  /** 是否经实测/官方可见性确认；get_skill_detail 注册表补集发现的为 false（未验证） */
  verified?: boolean;
}

export interface PoolStatus {
  uid: string;
  name: string;
  credits: number | null;
  credits_expire_at: number | null;
  cooling: boolean;
  cooldown_until: number | null;
  cooldown_reason: string | null;
  disabled: boolean;
  err_count: number;
}

export interface ApiPoolFile {
  enabled_uids: string[];
  /** 调度策略：expire_first（默认）/ credit_first / random */
  strategy?: string;
  /** 参与调度的分组 id 列表；空 = 不限分组 */
  group_ids?: string[];
}

/** 用量统计计数（按模型/账号/Key 维度） */
export interface UsageCounterView {
  name: string;
  requests: number;
  ok: number;
  errors: number;
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
  avg_duration_ms: number;
  models: UsageCounterView[];
  accounts: UsageCounterView[];
  keys: UsageCounterView[];
  key_tokens: UsageKeyTokenView[];
}

/** API Key 条目（data/api_keys.json；daily_limit=0 表示不限） */
export interface ApiKeyEntry {
  id: string;
  name: string;
  key: string;
  enabled: boolean;
  daily_limit: number;
  created_at: number;
  used_date: string;
  used_today: number;
}

// ---- 登录态快照 ----
export interface ProfileInfo {
  slot: string;
  size_bytes: number;
  file_count: number;
  last_modified: string;
}

// ---- OAuth 登录 ----
export interface OAuthLoginUrl {
  url: string;
  state: string;
  redirect_uri: string;
}

export interface OAuthCallbackInfo {
  refresh_token: string;
  access_token: string | null;
  user_id: string | null;
  user_name: string | null;
  avatar: string | null;
}

export interface OAuthLoginResult {
  user_id: string;
  name: string;
  jwt: string;
  refresh_token: string;
  has_refresh_token: boolean;
}

// ---- 应用自更新 ----

export interface UpdateCheckResult {
  has_update: boolean;
  current_version: string;
  latest_version: string;
  /** 资产文件名，如 "Trae Work 助手_2.5.1_x64-setup.exe" */
  asset_name: string;
  /** 资产下载直链（browser_download_url） */
  download_url: string;
  /** 资产字节数 */
  size: number;
  release_page: string;
}

export interface UpdateDownloadProgress {
  received: number;
  total: number;
  percent: number;
}
