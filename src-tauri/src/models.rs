use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Clone, Default)]
pub struct AccountView {
    pub user_id: String,
    pub name: String,
    pub group_id: Option<String>,
    pub jwt: String,
    pub jwt_exp_hours: Option<f64>,
    pub jwt_exp_timestamp: Option<i64>,
    pub checked_today: Option<bool>,
    pub credits: Option<i64>,
    pub remaining_credits: Option<f64>,
    pub device_id_masked: Option<String>,
    pub cooldown_type: Option<String>,
    pub cooldown_until: Option<i64>,
    pub cooldown_reason: Option<String>,
    pub has_refresh_token: bool,
    pub jwt_auto_refresh: bool,
    pub credits_expire_at: Option<i64>,
    /// 通用积分（product_id != 209）剩余
    #[serde(default)]
    pub general_credits: Option<f64>,
    /// Work 积分（product_id == 209）剩余
    #[serde(default)]
    pub work_credits: Option<f64>,
    /// 套餐身份（Free / Lite / Pro ...，来自 ide_user_pay_status 缓存）
    #[serde(default)]
    pub pay_identity: Option<String>,
    /// 会员套餐到期时间（Unix 秒，来自 ent_usage 会员包）
    #[serde(default)]
    pub membership_expire: Option<i64>,
    /// 会员套餐下次自动续费时间（Unix 秒，无自动续费为 None）
    #[serde(default)]
    pub membership_next_billing: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RawAccount {
    pub name: String,
    #[serde(rename = "UserID", default)]
    pub user_id: Option<String>,
    pub jwt: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub added_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// 账户中心（icube-dc）id。实测为本机设备/数据中心级标识（跨账号恒定，不具备账号区分度），
    /// 仅记录预留供未来与外部数据源对账合并，不参与去重/合并/展示（用户确认 2026-09-07）
    #[serde(rename = "DcID", default)]
    pub dc_id: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct AccountsFile {
    pub accounts: Vec<RawAccount>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub order: i32,
}

#[derive(Serialize, Deserialize, Default)]
pub struct GroupsFile {
    pub groups: Vec<Group>,
    #[serde(default)]
    pub membership: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct DeviceEntry {
    pub device_id: String,
    #[serde(default)]
    pub market_user_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

pub type DeviceMap = HashMap<String, DeviceEntry>;

#[derive(Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default = "default_port")]
    pub proxy_port: u16,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub launch_minimized: bool,
    /// 启动静默签到（T11）：启动 60s 后对未签到账号自动执行一轮签到
    #[serde(default)]
    pub silent_checkin: bool,
    #[serde(default = "default_true")]
    pub auto_start_proxy: bool,
    #[serde(default = "default_true")]
    pub tray: bool,
    #[serde(default = "default_lang")]
    pub language: String,
    #[serde(default = "default_true")]
    pub checkin_skip_checked: bool,
    #[serde(default = "default_true")]
    pub checkin_skip_expired: bool,
    #[serde(default = "default_retry")]
    pub retry: i32,
    #[serde(default = "default_notify")]
    pub notify: String,
    #[serde(default)]
    pub trae_path: Option<String>,
    #[serde(default)]
    pub trae_cn_path: Option<String>,
    /// 豆包桌面版 exe 手动路径（环境配置页持久化；app_locate doubao 档案读取）
    #[serde(default)]
    pub doubao_path: Option<String>,
    /// 豆包会话续期保活端点（探活巡检用；KeepAlive 不依赖此项；空值由 state 迁移回填默认）
    #[serde(default)]
    pub doubao_renew_url: Option<String>,
    /// 豆包会员额度接口（会员额度汇总 XHR；空 = 额度查询不可用，由 state 迁移回填默认）
    #[serde(default)]
    pub doubao_quota_url: Option<String>,
    /// 豆包快照可选纳入 Default/IndexedDB（C4：对话历史等完整状态随账号迁移；体积代价大，默认排除）
    #[serde(default)]
    pub doubao_snapshot_include_idb: bool,
    /// WorkBuddy 桌面版 exe 手动路径（切换桥 workbuddy 档案 settings_key；页面随后续批次接入）
    #[serde(default)]
    pub workbuddy_path: Option<String>,
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default = "default_retention")]
    pub log_retention_days: i32,
    #[serde(default = "default_proxy_domains")]
    pub proxy_domains: String,
    #[serde(default)]
    pub proxy_log_path: Option<String>,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    #[serde(default = "default_api_model")]
    pub api_default_model: String,
}

fn default_api_port() -> u16 {
    7864
}
fn default_api_model() -> String {
    "deepseek-v4-flash".into()
}

fn default_port() -> u16 {
    8899
}
fn default_theme() -> String {
    "system".into()
}
fn default_true() -> bool {
    true
}
fn default_lang() -> String {
    "zh-CN".into()
}
fn default_retry() -> i32 {
    1
}
fn default_notify() -> String {
    "toast".into()
}
fn default_retention() -> i32 {
    30
}
pub fn default_proxy_domains() -> String {
    "trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com,doubao.com".into()
}

/// 旧版默认域名列表（未含 doubao.com）：用于把升级前已持久化的旧默认无缝迁移到新默认
pub fn legacy_proxy_domains() -> String {
    "trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com".into()
}

/// 豆包保活端点默认值：GET /info/v2/（通知未读数，轻量、必须登录，200=有效 / 302=过期）。
/// 实测字节 passport 为 30 天滑动续期（服务端按会话活跃内部刷新，不回发新 cookie），
/// 探活只需携带凭证访问一个"必须登录"的轻量端点即可判定有效性。
pub fn default_doubao_renew_url() -> String {
    "https://www.doubao.com/info/v2/".into()
}

/// 豆包会员额度接口默认值：POST /alice/commerce/sale/subscription/quota/summary/
/// （请求体 {"product_line":"membership"}，200 JSON 会员额度汇总；代理日志实测确认）
pub fn default_doubao_quota_url() -> String {
    "https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/".into()
}

/// 旧版保活端点默认值（doubao.com 首页）：升级时迁移到新默认
pub fn legacy_doubao_renew_url() -> String {
    "https://www.doubao.com/".into()
}

#[derive(Serialize, Deserialize, Default)]
pub struct CreditRecord {
    pub date: String,
    pub user_id: String,
    pub credits: i64,
    #[serde(default)]
    pub delta: i64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct CreditsFile {
    pub records: Vec<CreditRecord>,
}

/// 每日积分快照：记录当天所有账号的积分总数、获得数、消耗数
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct CreditsDailySnapshot {
    pub date: String,
    pub total: f64,
    pub earned: f64,
    pub consumed: f64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct CreditsDailyFile {
    #[serde(default)]
    pub snapshots: Vec<CreditsDailySnapshot>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct CheckinSummary {
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub results: Vec<serde_json::Value>,
    #[serde(default)]
    pub total_ok: i32,
    #[serde(default)]
    pub already: i32,
    #[serde(default)]
    pub failed: i32,
}

/// 剩余积分缓存文件：user_id -> 剩余积分
#[derive(Serialize, Deserialize, Default)]
pub struct RemainingCreditsFile {
    #[serde(default)]
    pub credits: HashMap<String, f64>,
    #[serde(default)]
    pub expire_times: HashMap<String, i64>,
    /// 通用积分（product_id != 209）剩余缓存
    #[serde(default)]
    pub general: HashMap<String, f64>,
    /// Work 积分（product_id == 209）剩余缓存
    #[serde(default)]
    pub work: HashMap<String, f64>,
    /// 会员套餐到期时间缓存（Unix 秒，来自 ent_usage 会员包 end_time）
    #[serde(default)]
    pub membership_expire: HashMap<String, i64>,
    /// 会员套餐下次自动续费时间缓存（Unix 秒，next_billing_time）
    #[serde(default)]
    pub membership_next_billing: HashMap<String, i64>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// 积分明细条目（仅剩余 > 0 且未过期的积分包）
#[derive(Serialize, Clone)]
pub struct CreditPackDetail {
    /// "ͨ用" | "Work"
    pub kind: String,
    /// 来源名称（如「每日签到」「每月登录积分」）
    pub source: String,
    /// 该包剩余积分 = credits_limit - usage.credits_amount
    pub remaining: f64,
    /// 过期时间（Unix 秒）
    pub expire_time: i64,
}

/// 单账号积分明细（悬浮展示用）
#[derive(Serialize, Clone)]
pub struct CreditDetail {
    pub general: f64,
    pub work: f64,
    pub total: f64,
    /// 按过期时间升序
    pub packs: Vec<CreditPackDetail>,
}

/// 单个账号的冷却状鎬?
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct CooldownEntry {
    #[serde(rename = "type", default)]
    pub error_type: String,
    #[serde(default)]
    pub until: i64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub error_count: i32,
}

/// 冷却状态文件：account_cooldowns.json
#[derive(Serialize, Deserialize, Default)]
pub struct AccountCooldownsFile {
    #[serde(default)]
    pub cooldowns: HashMap<String, CooldownEntry>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// API 池配置文件：api_pool.json
#[derive(Serialize, Deserialize, Default)]
pub struct ApiPoolFile {
    #[serde(default)]
    pub enabled_uids: Vec<String>,
    /// 调度策略：expire_first（默认）/ credit_first / random / weighted / p2c
    #[serde(default)]
    pub strategy: String,
    /// 参与调度的分组 id 列表；空 = 不限分组
    #[serde(default)]
    pub group_ids: Vec<String>,
    /// WorkBuddy 上游开关（T2.1）：开启后 WB 目录模型路由到 WB 账号池
    #[serde(default)]
    pub wb_enabled: bool,
    /// 默认深度思考（T5.3/F-62）：客户端未显式请求 reasoning_effort 时默认 high
    #[serde(default)]
    pub wb_default_thinking: bool,
    /// 工具代执行（T5.5/F-64）：客户端声明 web_search 类工具时代理侧代执行
    #[serde(default = "default_true")]
    pub wb_tool_exec: bool,
    /// 后台任务降级（T5.6③/F-65）：标题/摘要类短请求路由到目录最低倍率模型
    #[serde(default)]
    pub wb_bg_downgrade: bool,
}

/// 池中单个账号的运行时状态（给 /status 和前端使用）
#[derive(Serialize, Clone)]
pub struct PoolStatus {
    pub uid: String,
    pub name: String,
    pub credits: Option<f64>,
    pub credits_expire_at: Option<i64>,
    pub cooling: bool,
    pub cooldown_until: Option<i64>,
    pub cooldown_reason: Option<String>,
    pub disabled: bool,
    pub err_count: i32,
    /// 账号五态机（T2.2/F-29 v1.2）：Available/QuotaProtection/RateLimited/Forbidden/ProxyDisabled
    #[serde(default)]
    pub state: String,
}

/// API 服务整体状态（给前端用）
#[derive(Serialize, Clone)]
pub struct ApiServiceStatus {
    pub running: bool,
    pub port: u16,
    pub total_requests: u64,
    pub active_uid: Option<String>,
    pub last_error: Option<String>,
    pub started_at: Option<u64>,
}
