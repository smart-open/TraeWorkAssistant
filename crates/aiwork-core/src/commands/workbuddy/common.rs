//! WorkBuddy 共享底层（原 workbuddy.rs 机械拆分）：路径常量、账号池/凭证库/设置读写、
//! 字段提取、续期互斥锁等被多域复用的辅助。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。
//! Web 化改造：删除桌面通知（notify_all/NotifyChannels）、tasklist 进程检测（is_running）、
//! std::os::windows::process/Command 依赖、CLI settings.json 回滚、会话 uid 防护（仅桌面命令使用）。

use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::fs_utils;
use crate::state::AppState;

// ── 路径常量（与 wb_common.py / trae-switch-bridge.ps1 保持一致）────────────

pub(super) fn auth_file_path() -> PathBuf {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    PathBuf::from(local)
        .join("CodeBuddyExtension")
        .join("Data")
        .join("Public")
        .join("auth")
        .join("workbuddy-desktop.info")
}

/// auth 文件读取路径：settings.wb_auth_file_path 人工指定优先（环境配置页），否则默认布局。
/// 仅作用于读取类路径（检测/列表/导入/续期/用量）；环境重置等清理动作仍针对客户端真实落盘位置。
pub(super) fn auth_file_path_of(state: &AppState) -> PathBuf {
    if let Some(p) = state.settings().wb_auth_file_path.as_deref() {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    auth_file_path()
}

pub(super) fn account_id_of(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("wb-{}", &hex[..12])
}

// ── 数据结构 ────────────────────────────────────────────────────────────────

/// 账号池记录（§3.3 结构）
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct WorkBuddyAccount {
    /// wb-<sha256(token) 前 12 位>（同 token 稳定同 id，F-04）
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub phone_masked: String,
    #[serde(default)]
    pub edition_type: String,
    /// accessToken 过期时间（Unix 秒；刷新/导入时回填）
    #[serde(default)]
    pub access_token_expires_at: Option<i64>,
    #[serde(default)]
    pub refresh_token_expires_at: Option<i64>,
    #[serde(default)]
    pub auth_saved_at: Option<i64>,
    #[serde(default)]
    pub needs_relogin: bool,
    #[serde(default)]
    pub relogin_reason: String,
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub note: String,
    /// 余额缓存（credits_fetch 成功后回写，供列表/概述展示）
    #[serde(default)]
    pub credits_balance: Option<f64>,
    #[serde(default)]
    pub credits_fetched_at: Option<String>,
    /// 最早积分包到期时间缓存（Unix 秒；积分查询/每日快照回写）：
    /// 剩余>0 且未过期包取 min，Buddy 不分包类型（issue #28 调度口径）；
    /// None = 无到期信息或全部包长期有效，键值随每次回写覆盖（不留 stale）
    #[serde(default)]
    pub credits_expire_at: Option<i64>,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub(super) struct WbPool {
    #[serde(default)]
    pub(super) accounts: Vec<WorkBuddyAccount>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct WorkBuddySettings {
    /// 启动自动补签（F-55）：启动时核验未签账号自动补签
    #[serde(default)]
    pub auto_checkin: bool,
    /// 保活阈值（天）；0 = 每天无条件刷新全部带 refreshToken 账号
    #[serde(default)]
    pub keepalive_days: i64,
    /// 惰性刷新（小时）：剩余有效期低于该值才刷新
    #[serde(default = "default_lazy_hours")]
    pub lazy_refresh_hours: i64,
    // 成长中心开关（F-17，批次2 消费；先落配置）
    #[serde(default = "default_true")]
    pub growth_travel: bool,
    #[serde(default = "default_true")]
    pub growth_lottery: bool,
    #[serde(default = "default_true")]
    pub growth_tasks: bool,
    // CLI 五重防护自动轮换（F-59，批次3 T3.4）
    #[serde(default)]
    pub cli_rotate_enabled: bool,
    /// 检查间隔（分钟，后台线程周期触发）
    #[serde(default = "default_cli_interval")]
    pub cli_rotate_interval_minutes: i64,
    /// ① 冷却期：切换后 N 分钟内不切
    #[serde(default = "default_cli_cooldown")]
    pub cli_cooldown_minutes: i64,
    /// ② 到期差异阈值：目标比当前早到期超过 N 小时才切（防横跳）
    #[serde(default = "default_cli_gap")]
    pub cli_min_gap_hours: i64,
    /// ③ 到期紧迫阈值：目标剩余超过 N 小时 = 都还早，不切
    #[serde(default = "default_cli_urgency")]
    pub cli_min_urgency_hours: i64,
    /// ④ 活跃保护：CLI 最近会话写入 N 分钟内不切
    #[serde(default = "default_cli_guard")]
    pub cli_active_guard_minutes: i64,
    /// ⑤ 最小剩余积分：目标低于该值不切（0 = 关闭）
    #[serde(default)]
    pub cli_min_remaining_credits: f64,
    // 失败通知渠道已并入全局通知配置（kv notify_config，系统设置 → 通知渠道）；
    // 旧设置 JSON 中的 notify_* 字段由 serde 忽略，无需迁移
    // UI 坐标点击签到兜底（F-18，批次4 T4.2）：仅手动触发，默认关闭
    #[serde(default)]
    pub ui_click_enabled: bool,
    /// 签到按钮屏幕坐标（0 = 未配置）
    #[serde(default)]
    pub ui_click_x: i64,
    #[serde(default)]
    pub ui_click_y: i64,
}

fn default_lazy_hours() -> i64 {
    24
}
fn default_true() -> bool {
    true
}
fn default_cli_interval() -> i64 {
    30
}
fn default_cli_cooldown() -> i64 {
    120
}
fn default_cli_gap() -> i64 {
    24
}
fn default_cli_urgency() -> i64 {
    72
}
fn default_cli_guard() -> i64 {
    30
}

impl WorkBuddySettings {
    fn with_defaults() -> Self {
        Self {
            auto_checkin: false,
            keepalive_days: 0,
            lazy_refresh_hours: 24,
            growth_travel: true,
            growth_lottery: true,
            growth_tasks: true,
            cli_rotate_enabled: false,
            cli_rotate_interval_minutes: 30,
            cli_cooldown_minutes: 120,
            cli_min_gap_hours: 24,
            cli_min_urgency_hours: 72,
            cli_active_guard_minutes: 30,
            cli_min_remaining_credits: 0.0,
            ui_click_enabled: false,
            ui_click_x: 0,
            ui_click_y: 0,
        }
    }
}

// ── 工具函数 ────────────────────────────────────────────────────────────────

pub(super) fn load_pool(state: &AppState) -> WbPool {
    // SQLite 化（P3）：workbuddy_accounts.json → wb_accounts 表
    let v = crate::store::docs::wb_pool_load(&crate::store::db(&state.data_dir));
    serde_json::from_value(v).unwrap_or_default()
}

pub(super) fn save_pool(state: &AppState, pool: &WbPool) -> Result<(), String> {
    let v = serde_json::to_value(pool).map_err(|e| format!("序列化失败: {e}"))?;
    crate::store::docs::wb_pool_save(&crate::store::db(&state.data_dir), &v)
}

pub(crate) fn load_settings(state: &AppState) -> WorkBuddySettings {
    // SQLite 化（P2）：workbuddy_settings.json → kv `workbuddy_settings`
    let mut s: WorkBuddySettings = crate::store::db(&state.data_dir).kv_get("workbuddy_settings");
    // 审查 P2：单字段非法只钳制该字段为默认值，不再整体 with_defaults() 重置
    //（避免损坏一个字段连带丢掉轮换/通知等其余配置）
    if s.lazy_refresh_hours <= 0 {
        s.lazy_refresh_hours = WorkBuddySettings::with_defaults().lazy_refresh_hours;
    }
    s
}

// 宽容字段提取统一走 fs_utils::dig（含 data/result/resp/response/info/auth/account
// 包裹键下钻，与新版客户端 auth 文件嵌套结构 {account, auth} 兼容；审查 P2：消除本地重复实现漂移）。

pub(super) fn as_str(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(|s| s.to_string())
}

pub(super) fn as_ts_seconds(v: Option<&serde_json::Value>) -> Option<i64> {
    let raw = v?;
    if let Some(ms) = raw.as_i64() {
        // expiresAtMs 毫秒级（>1e12），统一折算秒
        return Some(if ms > 1_000_000_000_000 { ms / 1000 } else { ms });
    }
    if let Some(f) = raw.as_f64() {
        return Some(if f > 1_000_000_000_000.0 { (f / 1000.0) as i64 } else { f as i64 });
    }
    raw.as_str().and_then(|s| {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.timestamp())
            .or_else(|| s.parse::<i64>().ok().map(|ms| if ms > 1_000_000_000_000 { ms / 1000 } else { ms }))
    })
}

/// 值类型名（诊断用，绝不含值本身）
fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// 诊断用键名提取（issue #51）：仅列键名 + 值类型（绝不含值），顶层 + auth/account 一层子对象。
/// 供「未找到 accessToken」类错误自带结构线索，用户截图即可定位，省去跑 PowerShell 往返。
/// 类型标注（issue #58）：键存在但值为 null 时「未找到」报错与键名清单自相矛盾，
/// 附类型后用户截图可直接看出 accessToken（null）→ 未登录，而非结构变更。
pub(super) fn auth_key_names(raw: &serde_json::Value) -> String {
    if !raw.is_object() {
        return "（非 JSON 对象）".into();
    }
    let mut names: Vec<String> = Vec::new();
    if let Some(map) = raw.as_object() {
        names.extend(map.iter().map(|(k, v)| format!("{k}（{}）", json_type_name(v))));
        for wk in ["auth", "account"] {
            if let Some(child) = map.get(wk).and_then(|v| v.as_object()) {
                names.extend(
                    child
                        .iter()
                        .map(|(k, v)| format!("{wk}.{k}（{}）", json_type_name(v))),
                );
            }
        }
    }
    if names.is_empty() { "（对象无键）".into() } else { names.join(", ") }
}

/// auth 文件 access token 提取 + 失败原因分类（issue #58）：
/// 对候选键逐个 dig，命中即决（语义与 dig 一致，null 不回退尝试后续候选键，避免误采其他 token 字段串号）：
/// 命中且为非空字符串 → Ok；命中但为 null/非字符串/空串 → 「未登录/凭证为空」类错误（不再误报结构变更）；
/// 全部候选键未命中 → 「结构可能已变更」+ 键名诊断（含值类型）。
pub(super) fn extract_access_token(raw: &serde_json::Value) -> Result<String, String> {
    for k in ["accessToken", "access_token", "token"] {
        let Some(v) = fs_utils::dig(raw, &[k]) else { continue };
        if let Some(s) = v.as_str() {
            if !s.is_empty() {
                return Ok(s.to_string());
            }
            return Err(format!(
                "auth 文件 accessToken 为空字符串（可能未登录）：键 {k} 存在但无值，请先在 WorkBuddy 客户端登录后重试"
            ));
        }
        return Err(format!(
            "auth 文件凭证不可用：键 {k} 存在但值为 {}（预期字符串；null 多为未登录或客户端已清空登录态），请先在 WorkBuddy 客户端登录后重试",
            json_type_name(v)
        ));
    }
    Err(format!(
        "auth 文件中未找到 accessToken（结构可能已变更）；实际键名: {}",
        auth_key_names(raw)
    ))
}

// ── 设置（F-55 配置化）──────────────────────────────────────────────────────

pub fn workbuddy_settings_get(state: &AppState) -> WorkBuddySettings {
    load_settings(state)
}

pub fn workbuddy_settings_set(state: &AppState, patch: WorkBuddySettings) -> Result<(), String> {
    crate::store::db(&state.data_dir).kv_set("workbuddy_settings", &patch)
}

// ── 工具侧凭证副本写入（F-10 双源化）───────────────────────────────────────

pub(super) fn upsert_token_store(state: &AppState, id: &str, creds: &serde_json::Value) -> Result<(), String> {
    // H-1：表级读改写互斥（与 tasks 侧 save_token_store 共用 WB_TOKEN_STORE_LOCK，
    // 导入/OAuth 与签到/积分刷新并发写不互相覆盖）
    let _table =
        crate::tasks::wb_common::WB_TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 凭证收敛（P0-1）：读走 secure 回填，写走 secure 占位（敏感字段进 vault，DB 不落明文）
    let existing = crate::tasks::wb_common::token_store_load_secure(&state.data_dir);
    let mut rec = existing
        .get("tokens")
        .and_then(|t| t.get(id))
        .cloned()
        .unwrap_or(serde_json::json!({}));
    if let Some(rm) = rec.as_object_mut() {
        for (k, v) in creds.as_object().unwrap_or(&serde_json::Map::new()) {
            if !v.is_null() {
                rm.insert(k.clone(), v.clone());
            }
        }
        rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
    }
    crate::tasks::wb_common::token_store_upsert_secure(&state.data_dir, id, &rec)
}

// ── M3 凭证续期互斥（F-09，Rust 侧手动触发；schtasks 每周兜底走 python --renew-only）──

/// 每账号续期互斥（审查 P1）：外层 std Mutex 只保护锁表本身（短临界区），
/// 内层 tokio Mutex 才是账号级互斥——try_lock 拿不到即拒绝，不排队不阻塞。
/// 锁表用 OnceLock 惰性初始化（HashMap::new 非 const，无法直接用于 static）。
pub(super) fn wb_renew_locks(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    LOCKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

// ── CLI 轮换状态（自 cli.rs 内联：cli 域桌面链退役后，account_remove 清理
//    active_account_id 残留仍需读写该 kv；结构原样保留以防 logs 等字段被覆盖丢失）──

/// 保存 CLI 轮换状态（SQLite 化 P4：kv `wb_cli_rotate_state`，accounts.rs 删除账号时复用）
pub(super) fn save_cli_rotate_state(state: &AppState, st: &CliRotateState) -> Result<(), String> {
    crate::store::db(&state.data_dir).kv_set("wb_cli_rotate_state", st)
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub(super) struct CliRotateState {
    #[serde(default)]
    pub(super) last_switch_at_ms: Option<i64>,
    #[serde(default)]
    pub(super) active_account_id: Option<String>,
    /// 轮换日志（旧→新，cap 50）
    #[serde(default)]
    pub(super) logs: Vec<serde_json::Value>,
}

pub(super) fn load_cli_rotate_state(state: &AppState) -> CliRotateState {
    // SQLite 化（P2）：wb_cli_rotate_state.json → kv `wb_cli_rotate_state`
    crate::store::db(&state.data_dir).kv_get("wb_cli_rotate_state")
}
