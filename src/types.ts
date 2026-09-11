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
  // 豆包应用页面（侧边栏应用切换 Tab → 豆包）
  | 'doubao-overview'
  | 'doubao-accounts'
  | 'doubao-settings';

/** 侧边栏应用切换（左下角 Tab）：Trae 当前菜单 / Buddy 后期扩展 / 豆包 接入中 */
export type AppKey = 'trae' | 'buddy' | 'doubao';

/** 各应用的默认落地页 */
export const APP_HOME_VIEW: Record<AppKey, ViewKey> = {
  trae: 'dashboard',
  buddy: 'dashboard',
  doubao: 'doubao-overview',
};


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
  /** 组内账号 uid 列表（账号池分组筛选实时预览用，T10） */
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
  trae_path: string | null;
  trae_cn_path: string | null;
  doubao_path: string | null;
  /** 豆包会话续期保活端点（null = 用 doubao.com 首页滑动续期） */
  doubao_renew_url: string | null;
  /** 豆包会员额度接口（抓包固化后填入；null = 额度查询不可用） */
  doubao_quota_url: string | null;
  /** 豆包快照可选纳入 Default/IndexedDB（对话历史等完整状态随账号迁移；体积代价大） */
  doubao_snapshot_include_idb: boolean;
  /** WorkBuddy 桌面版 exe 手动路径（随后续批次接入） */
  workbuddy_path: string | null;
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
  /** 调度策略：expire_first（默认）/ credit_first / random（T10） */
  strategy?: string;
  /** 参与调度的分组 id 列表；空 = 不限分组（T10） */
  group_ids?: string[];
}

/** 用量统计计数（按模型/账号/Key 维度，T1） */
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

/** API Key 条目（data/api_keys.json；daily_limit=0 表示不限，T2） */
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

// ---- 豆包账号池（P2：快照槽 + 别名元数据合并视图，对应 Rust doubao.rs DoubaoAccountView）----
export interface DoubaoAccountView {
  user_id: string;
  name: string;
  note: string;
  has_snapshot: boolean;
  size_bytes: number;
  file_count: number;
  last_modified: string;
  is_current: boolean;
  added_at: string | null;
  /** 会话状态（P3）：ok=有效 / expired=已过期 / unknown=未探活 / none=无 sessionid */
  session_state: 'ok' | 'expired' | 'unknown' | 'none';
  /** 明文 sessionid（编辑弹框回填用，仅本地） */
  session_id: string | null;
  /** sid_guard 原文（编辑弹框回填用） */
  sid_guard: string | null;
  /** ttwid 设备 Cookie（对话导出 API 必需；代理抓包或手动录入） */
  ttwid: string | null;
  /** 会员等级（null = 免费或未识别；quota_checked_at 非空表示已查询过） */
  quota_level: string | null;
  /** 会员到期时间（免费账号为 null） */
  quota_expire_at: string | null;
  /** 额度状态一句话（如 "图片 80/100 · 视频 3/10"） */
  quota_summary: string | null;
  /** 最近一次额度查询时间 */
  quota_checked_at: string | null;
  session_expire_at: string | null;
  cookies_synced_at: string | null;
  last_renew_at: string | null;
  session_source: string | null;
  /** 池级：最近一次 KeepAlive 保活时间（所有行同值） */
  last_keepalive_at: string | null;
}

/** doubao_renew.py 摘要 JSON（P3 巡检/诊断结果） */
export interface DoubaoRenewSummary {
  mode: 'full' | 'diagnose';
  finished_at: string;
  renew_url?: string;
  sync: { synced: number; sources: { source: string; decryptable: boolean; cookies?: string[]; ascii_values?: number; note?: string; detail?: string }[] };
  renew?: { ok: number; expired: number; error: number; skipped: number };
  accounts?: { user_id: string; status: string; detail?: string; renewed?: boolean }[];
  logs?: string[];
}

/** 豆包切换/保存的目标应用参数（与 switch_account / save_current_login 的 target_app 对齐） */
export type DoubaoTargetApp = 'Doubao';

/** 代理自动抓到的豆包会话凭证（device_proxy.py 写 doubao_captured_credentials.json，Rust doubao.rs 透传） */
export interface DoubaoCapturedCredential {
  session_id: string;
  sid_guard: string;
  host: string;
  captured_at: string;
  /** ttwid 设备 Cookie（对话导出 API 必需） */
  ttwid: string;
}

/** D1：对话数据备份/恢复结果（doubao_chatdata_backup / doubao_chatdata_restore） */
export interface DoubaoChatdataResult {
  ok: boolean;
  files: number;
  /** 备份目录（仅 backup 返回） */
  path?: string;
}

/** D1：对话数据备份状态（doubao_chatdata_info） */
export interface DoubaoChatdataInfo {
  backed: boolean;
  files?: number;
  size_bytes?: number;
  backed_at?: string | null;
}

/** D2：对话记录导出结果（doubao_chats.py stdout 末行 JSON） */
export interface DoubaoExportResult {
  ok: boolean;
  conversations: number;
  messages: number;
  md_path: string;
  json_path: string;
}

/** doubao_quota.py 摘要 JSON（会员额度查询结果；字段由宽容解析尽力得到，均可为 null） */
export interface DoubaoQuotaResult {
  ok: boolean;
  http_status: number;
  url: string;
  user_id: string;
  parsed: {
    level: string | number | null;
    expire_at: string | null;
    is_gift?: boolean | null;
    has_subscription?: boolean | null;
    /** 订阅记录（对齐客户端「订阅记录」页；免费账号为 null） */
    subscription: {
      name: string | null;
      period_days: number | null;
      start_at: string | null;
      expire_at: string | null;
      is_gift: boolean | null;
      active: boolean;
    } | null;
    items: (
      | { name: string; total: string | number; left: string | number | null; used: string | number | null }
      | { name: string; used_percent: number; exhausted: boolean; reset_at: string | null }
    )[];
  };
  finished_at?: string;
}

/** 豆包运维历史事件（keepalive/renew/quota；doubao_health_history.json） */
export interface DoubaoHistoryEvent {
  ts: string;
  kind: 'keepalive' | 'renew' | 'quota';
  ok: boolean;
  uid?: string;
  level?: string | null;
  summary?: string | null;
  windows?: { name: string; used_percent: number; reset_at: string }[];
  source?: string;
}

/** 豆包快照版本元数据（C3：snapshot_meta.json + Last Version；schema_version 0 = 旧版快照无元数据） */
export interface DoubaoSnapshotMeta {
  schema_version: number;
  created_at: string;
  chromium_version: string;
  include_idb: boolean;
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
  // 发布方提供的安装包 SHA256（release 正文约定行；旧版本无此行时缺省，跳过校验）
  sha256?: string | null;
}

// API Key 数据文件视图（api_keys_list 返回：列表 + 鉴权开关）
export interface ApiKeysFileView {
  keys: ApiKeyEntry[];
  // 显式关闭鉴权：仅当无启用 Key 时生效（true=放行，默认 false=拒绝）
  auth_disabled: boolean;
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
