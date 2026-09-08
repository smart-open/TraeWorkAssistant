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

// ---- F-01 安装位置自动识别（跨应用通用三级探测）----
export interface AppLocate {
  app: string;
  exe: string | null;
  user_data_dir: string;
  version: string | null;
  /** settings | registry | default | process | not_found */
  source: string;
}


export interface DiscoveredAccount {
  user_id: string;
  /** 账户中心（dc）uid —— 与账号池 Cloud-IDE id 体系不同，仅诊断展示 */
  dc_uid?: string | null;
  /** Cloud-IDE uid 是否经本机使用证据确认（false 时 user_id 实为 dc uid，不可入池） */
  uid_confident: boolean;
  /** TraeWork | Trae */
  app: string;
  app_label: string;
  in_pool: boolean;
  storage_path: string;
}

// ---- Trae 会员/套餐信息 ----
export interface AppEntitlement {
  app: string;
  app_label: string;
  identity_str: string | null;
  identity: number | null;
  last_sync_time: number | null;
}

export interface LocalEntitlement {
  work: AppEntitlement | null;
  cn: AppEntitlement | null;
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
  auto_start_proxy: boolean;
  tray: boolean;
  language: string;
  checkin_skip_checked: boolean;
  checkin_skip_expired: boolean;
  retry: number;
  notify: string;
  trae_path: string | null;
  trae_cn_path: string | null;
  data_dir: string | null;
  log_retention_days: number;
  proxy_domains: string;
  proxy_log_path: string | null;
  api_port: number;
  api_key: string;
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
  asset_name: string;
  download_url: string;
  size: number;
  release_page: string;
}

export interface UpdateDownloadProgress {
  received: number;
  total: number;
  percent: number;
}

// 下载完成后的安装包信息（update_download 返回，供「确认安装」使用）
export interface UpdateDownloaded {
  file_path: string;
  asset_name: string;
  size: number;
  version: string;
}
