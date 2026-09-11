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
  | 'doubao-settings'
  // Buddy 应用页面（侧边栏应用切换 Tab → Buddy，批次1）
  | 'buddy-overview'
  | 'buddy-accounts'
  | 'buddy-checkin'
  | 'buddy-credits'
  | 'buddy-settings';

/** 侧边栏应用切换（左下角 Tab）：Trae 当前菜单 / Buddy 批次1接入 / 豆包 接入中 */
export type AppKey = 'trae' | 'buddy' | 'doubao';

/** 各应用的默认落地页 */
export const APP_HOME_VIEW: Record<AppKey, ViewKey> = {
  trae: 'dashboard',
  buddy: 'buddy-overview',
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
  /** WorkBuddy 上游开关（T2.1）：开启后 WB 目录模型路由到 WB 账号池 */
  wb_enabled?: boolean;
  /** 默认深度思考（T5.3/F-62）：客户端未显式请求 reasoning_effort 时默认 high */
  wb_default_thinking?: boolean;
  /** 工具代执行（T5.5/F-64）：客户端声明 web_search 类工具时代理侧代执行 */
  wb_tool_exec?: boolean;
  /** 后台任务降级（T5.6③/F-65）：标题/摘要类短请求路由到目录最低倍率模型 */
  wb_bg_downgrade?: boolean;
}

/** CC Switch 协同状态（T5.7/F-43） */
export interface CcSwitchStatus {
  installed: boolean;
  dbPath: string;
  claudeRegistered: boolean;
  codexRegistered: boolean;
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
  /** 限定上游（WB 上游账号 uid 白名单；空 = 不限） */
  allowed_accounts: string[];
  /** 调度模式：expire_first（默认，临期优先）| dedicated（专一） */
  schedule_mode: string;
  /** 专一模式绑定的上游账号 uid（空 = allowed_accounts 首个） */
  dedicated_account: string;
  /** 按日请求统计（升序，保留最近 90 天） */
  daily_stats: ApiKeyDailyStat[];
}

/** 子 Key 按日统计项（F-35） */
export interface ApiKeyDailyStat {
  date: string;
  requests: number;
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

// ---- WorkBuddy 账号池（批次1；对应 Rust workbuddy.rs WorkBuddyAccountView）----
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
  /** 在线 = 本机 auth 文件当前生效账号 */
  is_current: boolean;
  has_credential: boolean;
  has_snapshot: boolean;
}

export interface WorkBuddyEnvCheck {
  installed: boolean;
  running: boolean;
  version: string | null;
  exe: string | null;
  auth_file_exists: boolean;
  data_dir_exists: boolean;
  snapshot_uid: string | null;
  snapshot_nickname: string | null;
  snapshot_edition: string | null;
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
  /** 失败通知渠道（F-19）：空 = 关闭 */
  notify_wechat_webhook: string | null;
  notify_serverchan_sendkey: string | null;
  /** UI 坐标点击签到兜底（F-18）：仅手动触发，默认关闭 */
  ui_click_enabled: boolean;
  ui_click_x: number;
  ui_click_y: number;
}

/** 账号库导入结果（F-46 扩展） */
export interface WbPoolImportResult {
  added: number;
  skipped: number;
  with_credentials: number;
}

// ---- CodeBuddy CLI 切号桥（F-06/F-59，批次3；Rust workbuddy_cli.rs + commands）----
export interface WbCliStatus {
  settings_present: boolean;
  env_token_present: boolean;
  /** 进程环境变量 CODEBUDDY_AUTH_TOKEN 存在（会覆盖 settings.json，需删除） */
  environment_override: boolean;
  active_account_id: string | null;
  active_account_name: string | null;
  /** CLI 最近会话写入时间（Unix 毫秒；活跃保护数据源） */
  recent_activity_ms: number | null;
  last_switch_at_ms: number | null;
  config: Pick<
    WorkBuddySettings,
    | 'cli_rotate_enabled'
    | 'cli_rotate_interval_minutes'
    | 'cli_cooldown_minutes'
    | 'cli_min_gap_hours'
    | 'cli_min_urgency_hours'
    | 'cli_active_guard_minutes'
    | 'cli_min_remaining_credits'
  >;
}

export interface WbCliRotateResult {
  status: 'switched' | 'skipped' | 'error';
  reason?: string;
  error?: string;
  to?: { id: string; name: string };
}

export interface WbCliRotateLog {
  ts: number;
  action: 'noop' | 'skipped' | 'switched' | 'error' | 'manual';
  reason: string | null;
  from: string | null;
  to: { id: string; name: string } | null;
  detail?: { name: string; remaining: number; soonest_expire_at: number | null; valid: boolean; error: string | null }[];
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

/** 环境重置清单项（F-14：16 项认证残留清理） */
export interface WbResetItem {
  id: string;
  label: string;
  detail: string;
  exists: boolean;
}

/** 环境重置单项执行结果 */
export interface WbResetResult {
  id: string;
  ok: boolean;
  detail: string;
}

// ---- 本地 Token 统计 + 官方用量（F-25/26/57/58，批次3）----
/** 聚合数字组（本地统计各组通用，snake_case 对齐 Rust 输出） */
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
  summary: { usage_today: number; usage_7days: number; usage_this_month: number };
  daily: { date: string; usage: number; models: WbUsageModelPoint[] }[];
  models: WbUsageModelPoint[];
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
export interface WbUsageFallback {
  status: 'snapshot';
  snapshot_days: number;
  summary: { usage_today: number; usage_7days: number; usage_this_month: number };
  daily: { date: string; usage: number }[];
  note: string;
  fetched_at_ms: number;
}

/** 积分包（workbuddy_credits.py 宽容解析输出） */
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
}
