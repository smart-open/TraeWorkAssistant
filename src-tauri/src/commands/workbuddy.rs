//! WorkBuddy 应用接入（批次1）：环境检测 / 账号池 / auth 导入 / 凭证续期 / 签到 / 积分 / 设置。
//! 方案依据 docs/workbuddy-product-design.md §3.2~§3.8（M1~M5 + 命令契约）。
//!
//! ⚠ serde 命名约定：全部 snake_case，与前端 types.ts 严格对齐（同 doubao.rs 红线）。
//! 凭证红线：accessToken/refreshToken 等同密码——不进日志、不进 NDJSON、前端掩码展示。

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;
use crate::workbuddy_cli;

// ── 路径常量（与 wb_common.py / trae-switch-bridge.ps1 保持一致）────────────

fn auth_file_path() -> PathBuf {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    PathBuf::from(local)
        .join("CodeBuddyExtension")
        .join("Data")
        .join("Public")
        .join("auth")
        .join("workbuddy-desktop.info")
}

fn wb_data_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    PathBuf::from(home).join(".workbuddy")
}

fn snapshot_json_path() -> PathBuf {
    wb_data_dir().join("storage").join("skeleton").join("account-snapshot.json")
}

fn pool_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("workbuddy_accounts.json")
}

fn token_store_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("workbuddy_token_store.json")
}

fn settings_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("workbuddy_settings.json")
}

fn checkin_results_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("workbuddy_checkin_results.json")
}

fn account_id_of(token: &str) -> String {
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
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct WbPool {
    #[serde(default)]
    accounts: Vec<WorkBuddyAccount>,
}

#[derive(Serialize, Clone)]
pub struct WorkBuddyAccountView {
    pub id: String,
    pub uid: String,
    pub nickname: String,
    pub phone_masked: String,
    pub edition_type: String,
    pub access_token_expires_at: Option<i64>,
    pub refresh_token_expires_at: Option<i64>,
    pub auth_saved_at: Option<i64>,
    pub needs_relogin: bool,
    pub relogin_reason: String,
    pub group_id: String,
    pub note: String,
    pub credits_balance: Option<f64>,
    pub credits_fetched_at: Option<String>,
    /// 在线 = 本机 auth 文件当前生效账号（F-54 双态）
    pub is_current: bool,
    /// 有工具侧凭证副本或 auth 文件匹配
    pub has_credential: bool,
    /// 已录快照（PS 桥 profiles_workbuddy/<id>/）
    pub has_snapshot: bool,
}

#[derive(Serialize, Clone)]
pub struct WorkBuddyEnvCheck {
    pub installed: bool,
    pub running: bool,
    pub version: Option<String>,
    pub exe: Option<String>,
    pub auth_file_exists: bool,
    pub data_dir_exists: bool,
    /// account-snapshot.json 当前登录 uid（可读时）
    pub snapshot_uid: Option<String>,
    pub snapshot_nickname: Option<String>,
    pub snapshot_edition: Option<String>,
}

/// auth 文件扫描结果（F-04 导入预览；凭证字段不回传前端）
#[derive(Serialize, Clone)]
pub struct WorkBuddyScanResult {
    pub id: String,
    pub uid: String,
    pub nickname: String,
    pub edition_type: String,
    pub has_access_token: bool,
    pub has_refresh_token: bool,
    pub access_token_expires_at: Option<i64>,
    /// 已在池中
    pub exists: bool,
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
    // 失败通知渠道（F-19，批次3 T3.6）：桌面通知之外的可选渠道
    /// 企业微信群机器人 webhook（空 = 关闭）
    #[serde(default)]
    pub notify_wechat_webhook: Option<String>,
    /// Server酱 SendKey（空 = 关闭）
    #[serde(default)]
    pub notify_serverchan_sendkey: Option<String>,
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
            notify_wechat_webhook: None,
            notify_serverchan_sendkey: None,
            ui_click_enabled: false,
            ui_click_x: 0,
            ui_click_y: 0,
        }
    }
}

// ── 工具函数 ────────────────────────────────────────────────────────────────

fn load_pool(state: &AppState) -> WbPool {
    fs_utils::read_json(&pool_path(state))
}

fn save_pool(state: &AppState, pool: &WbPool) -> Result<(), String> {
    fs_utils::write_json(&pool_path(state), pool)
}

fn load_settings(state: &AppState) -> WorkBuddySettings {
    let s: WorkBuddySettings = fs_utils::read_json(&settings_path(state));
    if s.lazy_refresh_hours <= 0 {
        return WorkBuddySettings::with_defaults();
    }
    s
}

/// 失败通知统一入口（F-19）：桌面通知（有 AppHandle 时）+ 企业微信/Server酱可选渠道。
/// 渠道配置来自 workbuddy_settings.json；渠道失败静默记日志，不影响主流程。
pub fn push_notify(app: Option<&AppHandle>, data_dir: &std::path::Path, title: &str, body: &str) {
    let s: WorkBuddySettings = fs_utils::read_json(&data_dir.join("data").join("workbuddy_settings.json"));
    let channels = crate::notify::NotifyChannels {
        wechat_webhook: s.notify_wechat_webhook.clone().filter(|x| !x.trim().is_empty()),
        serverchan_sendkey: s.notify_serverchan_sendkey.clone().filter(|x| !x.trim().is_empty()),
    };
    crate::notify::notify_all(app, data_dir, title, body, &channels);
}

fn is_running() -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq WorkBuddy.exe", "/NH"])
        .creation_flags(0x08000000)
        .output();
    matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).contains("WorkBuddy.exe"))
}

/// 从 auth 文件 / snapshot JSON 宽容提取字段（浅层双链：account.* / auth.*，与 wb_common 对齐）
fn dig(v: &serde_json::Value, keys: &[&str]) -> Option<serde_json::Value> {
    fn find(v: &serde_json::Value, key: &str, depth: usize) -> Option<serde_json::Value> {
        if depth > 8 {
            return None;
        }
        match v {
            serde_json::Value::Object(m) => {
                if let Some(hit) = m.get(key) {
                    return Some(hit.clone());
                }
                // 新版客户端 auth 文件为嵌套结构：token/到期在 .auth.*，账号信息在 .account.*
                // （实测 2026-09 结构 {account, accounts, allAccounts, auth}），穿透这两层兼容新旧
                for wk in ["data", "result", "resp", "response", "info", "auth", "account"] {
                    if let Some(child) = m.get(wk) {
                        if let Some(hit) = find(child, key, depth + 1) {
                            return Some(hit);
                        }
                    }
                }
                None
            }
            serde_json::Value::Array(a) => a.iter().find_map(|i| find(i, key, depth + 1)),
            _ => None,
        }
    }
    keys.iter().find_map(|k| find(v, k, 0))
}

fn as_str(v: &Option<serde_json::Value>) -> Option<String> {
    v.as_ref().and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn as_ts_seconds(v: &Option<serde_json::Value>) -> Option<i64> {
    let raw = v.as_ref()?;
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

// ── M1 环境检测（workbuddy_env_check）──────────────────────────────────────

#[tauri::command(async)]
pub fn workbuddy_env_check(state: State<AppState>) -> WorkBuddyEnvCheck {
    // 复用 F-01 app_locate 四级探测（env.rs 内部实现走注册表/默认路径/进程反查）
    let locate = crate::commands::env::app_locate_inner(&state, "workbuddy");
    let auth_exists = auth_file_path().exists();
    let snap = fs_utils::read_json::<serde_json::Value>(&snapshot_json_path());
    let has_snap = snap.is_object();
    WorkBuddyEnvCheck {
        installed: locate.exe.is_some(),
        running: is_running(),
        version: locate.version,
        exe: locate.exe,
        auth_file_exists: auth_exists,
        data_dir_exists: wb_data_dir().exists(),
        snapshot_uid: if has_snap { as_str(&dig(&snap, &["uid", "accountId"])) } else { None },
        snapshot_nickname: if has_snap { as_str(&dig(&snap, &["nickname", "displayName", "name"])) } else { None },
        snapshot_edition: if has_snap { as_str(&dig(&snap, &["editionType", "edition"])) } else { None },
    }
}

// ── 账号池（F-04）──────────────────────────────────────────────────────────

#[tauri::command]
pub fn workbuddy_accounts_list(state: State<AppState>) -> Result<Vec<WorkBuddyAccountView>, String> {
    accounts_list_inner(&state)
}

fn accounts_list_inner(state: &AppState) -> Result<Vec<WorkBuddyAccountView>, String> {
    let pool = load_pool(state);
    // 在线判定：auth 文件 uid 与账号一致（客户端当前生效登录）
    let auth_uid = {
        let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path());
        if raw.is_object() { as_str(&dig(&raw, &["uid"])) } else { None }
    };
    let snap_path = state.data_dir.join("data").join("profiles_workbuddy");
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(state));
    let store_tokens = store.get("tokens").cloned().unwrap_or(serde_json::Value::Null);

    let views = pool
        .accounts
        .iter()
        .map(|a| {
            let has_cred = store_tokens.get(&a.id).is_some()
                || auth_uid.as_deref() == Some(a.uid.as_str());
            let has_snapshot = snap_path.join(&a.id).is_dir();
            WorkBuddyAccountView {
                id: a.id.clone(),
                uid: a.uid.clone(),
                nickname: a.nickname.clone(),
                phone_masked: a.phone_masked.clone(),
                edition_type: a.edition_type.clone(),
                access_token_expires_at: a.access_token_expires_at,
                refresh_token_expires_at: a.refresh_token_expires_at,
                auth_saved_at: a.auth_saved_at,
                needs_relogin: a.needs_relogin,
                relogin_reason: a.relogin_reason.clone(),
                group_id: a.group_id.clone(),
                note: a.note.clone(),
                credits_balance: a.credits_balance,
                credits_fetched_at: a.credits_fetched_at.clone(),
                is_current: auth_uid.is_some() && auth_uid == Some(a.uid.clone()),
                has_credential: has_cred,
                has_snapshot: has_snapshot,
            }
        })
        .collect();
    Ok(views)
}

#[tauri::command]
pub fn workbuddy_account_save(state: State<AppState>, user_id: String, name: Option<String>, note: Option<String>) -> Result<(), String> {
    let mut pool = load_pool(&state);
    let acct = pool
        .accounts
        .iter_mut()
        .find(|a| a.id == user_id)
        .ok_or_else(|| format!("账号不存在: {user_id}"))?;
    if let Some(n) = name {
        acct.nickname = n;
    }
    if let Some(n) = note {
        acct.note = n;
    }
    save_pool(&state, &pool)
}

#[tauri::command]
pub fn workbuddy_account_remove(state: State<AppState>, user_id: String, delete_snapshot: Option<bool>) -> Result<(), String> {
    let mut pool = load_pool(&state);
    let before = pool.accounts.len();
    pool.accounts.retain(|a| a.id != user_id);
    if pool.accounts.len() == before {
        return Err(format!("账号不存在: {user_id}"));
    }
    save_pool(&state, &pool)?;
    if delete_snapshot.unwrap_or(false) {
        let slot = state.data_dir.join("data").join("profiles_workbuddy").join(&user_id);
        if slot.is_dir() {
            let _ = std::fs::remove_dir_all(&slot);
        }
    }
    Ok(())
}

/// 扫描本机 auth 文件（F-04 导入预览；不写盘）
#[tauri::command(async)]
pub fn workbuddy_scan_auth_file(state: State<AppState>) -> Result<Option<WorkBuddyScanResult>, String> {
    let path = auth_file_path();
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs_utils::read_json::<serde_json::Value>(&path);
    if !raw.is_object() {
        return Err("auth 文件格式无法识别（JSON 解析失败）".into());
    }
    let access = as_str(&dig(&raw, &["accessToken", "access_token"]))
        .ok_or("auth 文件中未找到 accessToken（结构可能已变更）")?;
    if access.is_empty() {
        return Err("auth 文件 accessToken 为空（可能未登录）".into());
    }
    let id = account_id_of(&access);
    let exists = load_pool(&state).accounts.iter().any(|a| a.id == id);
    Ok(Some(WorkBuddyScanResult {
        id,
        uid: as_str(&dig(&raw, &["uid"])).unwrap_or_default(),
        nickname: as_str(&dig(&raw, &["nickname", "displayName", "name"])).unwrap_or_default(),
        edition_type: as_str(&dig(&raw, &["editionType", "edition"])).unwrap_or_default(),
        has_access_token: true,
        has_refresh_token: as_str(&dig(&raw, &["refreshToken", "refresh_token"])).is_some(),
        access_token_expires_at: as_ts_seconds(&dig(&raw, &["expiresAtMs", "expiresAt", "expires_in_ms"])),
        exists,
    }))
}

/// auth 文件导入入池（F-04）：写账号池 + 工具侧凭证副本（掩码入池、凭证不外泄）
#[tauri::command(async)]
pub fn workbuddy_account_import_auth(state: State<AppState>, name: Option<String>) -> Result<WorkBuddyAccountView, String> {
    let path = auth_file_path();
    if !path.exists() {
        return Err("未找到 auth 文件，请先在 WorkBuddy 客户端登录".into());
    }
    let raw = fs_utils::read_json::<serde_json::Value>(&path);
    let access = as_str(&dig(&raw, &["accessToken", "access_token"]))
        .ok_or("auth 文件中未找到 accessToken")?;
    let refresh = as_str(&dig(&raw, &["refreshToken", "refresh_token"]));
    let id = account_id_of(&access);
    let uid = as_str(&dig(&raw, &["uid"])).unwrap_or_default();
    let nickname = name
        .clone()
        .or_else(|| as_str(&dig(&raw, &["nickname", "displayName", "name"])))
        .unwrap_or_else(|| uid.chars().take(8).collect());

    let mut pool = load_pool(&state);
    if let Some(existing) = pool.accounts.iter_mut().find(|a| a.id == id) {
        // 重复导入 = 更新凭证时间戳与元数据
        existing.nickname = nickname;
        existing.uid = uid.clone();
        existing.auth_saved_at = Some(chrono::Utc::now().timestamp());
        existing.access_token_expires_at = as_ts_seconds(&dig(&raw, &["expiresAtMs", "expiresAt", "expires_in_ms"]));
        existing.needs_relogin = false;
        save_pool(&state, &pool)?;
    } else {
        pool.accounts.push(WorkBuddyAccount {
            id: id.clone(),
            uid: uid.clone(),
            nickname,
            edition_type: as_str(&dig(&raw, &["editionType", "edition"])).unwrap_or_default(),
            access_token_expires_at: as_ts_seconds(&dig(&raw, &["expiresAtMs", "expiresAt", "expires_in_ms"])),
            refresh_token_expires_at: as_ts_seconds(&dig(&raw, &["refreshExpiresAt", "refresh_expires_at"])),
            auth_saved_at: Some(chrono::Utc::now().timestamp()),
            ..Default::default()
        });
        save_pool(&state, &pool)?;
    }

    // 工具侧凭证副本（F-10 双源化）
    let creds = serde_json::json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_at_ms": dig(&raw, &["expiresAtMs", "expiresAt"]).and_then(|v| v.as_i64()),
        "uid": uid,
        "domain": as_str(&dig(&raw, &["domain"])),
    });
    upsert_token_store(&state, &id, &creds)?;

    // 返回合并视图（简化：直接重查）
    let views = accounts_list_inner(&state)?;
    views
        .into_iter()
        .find(|v| v.id == id)
        .ok_or_else(|| "导入后回读失败".into())
}

fn upsert_token_store(state: &AppState, id: &str, creds: &serde_json::Value) -> Result<(), String> {
    let mut store: serde_json::Value = fs_utils::read_json(&token_store_path(state));
    if !store.is_object() {
        store = serde_json::json!({});
    }
    let obj = store.as_object_mut().unwrap();
    if obj.get("version").is_none() {
        obj.insert("version".into(), serde_json::json!(1));
    }
    let tokens = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
    if let Some(t) = tokens.as_object_mut() {
        let mut rec = t.get(id).cloned().unwrap_or(serde_json::json!({}));
        if let Some(rm) = rec.as_object_mut() {
            for (k, v) in creds.as_object().unwrap_or(&serde_json::Map::new()) {
                if !v.is_null() {
                    rm.insert(k.clone(), v.clone());
                }
            }
            rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
        }
        t.insert(id.to_string(), rec);
    }
    fs_utils::write_json(&token_store_path(state), &store)
}

// ── M3 凭证续期（F-09，Rust 侧手动触发；schtasks 每周兜底走 python --renew-only）──

/// 刷新单账号凭证：读 token store（双源副本）→ POST plugin refresh → 回写。
/// 客户端运行中跳过 auth 文件写入（只更新工具侧副本，F-10 保证谁新用谁）。
#[tauri::command(async)]
pub fn workbuddy_refresh_token(state: State<AppState>, user_id: String) -> Result<String, String> {
    let mut pool = load_pool(&state);
    let acct = pool
        .accounts
        .iter()
        .find(|a| a.id == user_id)
        .ok_or_else(|| format!("账号不存在: {user_id}"))?
        .clone();

    // 工具侧副本凭证（结构化格式，自有域可安全读取）
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(&state));
    let rec = store.get("tokens").and_then(|t| t.get(&acct.id)).cloned().unwrap_or_default();
    let refresh = as_str(&dig(&rec, &["refresh_token"]))
        .or_else(|| {
            // 兜底：auth 文件 uid 匹配时取其 refreshToken（客户端当前生效账号）
            let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path());
            let fuid = as_str(&dig(&raw, &["uid"]));
            if fuid.as_deref() == Some(acct.uid.as_str()) && !acct.uid.is_empty() {
                as_str(&dig(&raw, &["refreshToken", "refresh_token"]))
            } else {
                None
            }
        })
        .ok_or("该账号无 refreshToken（不可刷新，需重新登录）")?;
    if refresh.is_empty() {
        return Err("该账号 refreshToken 为空（不可刷新，需重新登录）".into());
    }

    // 红线：X-Refresh-Token 仅出现在 refresh 端点
    let agent = ureq::AgentBuilder::new().build();
    let resp = agent
        .post("https://www.codebuddy.cn/v2/plugin/auth/token/refresh")
        .set("Authorization", "Bearer")
        .set("User-Agent", "WorkBuddy")
        .set("X-Refresh-Token", &refresh)
        .set("X-Auth-Refresh-Source", "workbuddy")
        .set("Content-Type", "application/json")
        .send_string("{}");
    let body: serde_json::Value = match resp {
        Ok(r) => r.into_json().unwrap_or_default(),
        Err(ureq::Error::Status(code, r)) => {
            let _ = r;
            return Err(format!("刷新失败（HTTP {code}）：refresh token 可能已失效，需重新登录"));
        }
        Err(e) => return Err(format!("刷新请求失败: {e}")),
    };
    let new_access = as_str(&dig(&body, &["accessToken"])).ok_or("刷新响应中无 accessToken")?;
    let new_refresh = as_str(&dig(&body, &["refreshToken"]));
    let expires_in: Option<i64> = dig(&body, &["expiresIn"]).and_then(|v| v.as_i64());
    let refresh_expires_in: Option<i64> = dig(&body, &["refreshExpiresIn"]).and_then(|v| v.as_i64());
    let now_ms = chrono::Utc::now().timestamp_millis();
    let exp_ms = expires_in.map(|s| now_ms + s * 1000);
    let rexp_ms = refresh_expires_in.map(|s| now_ms + s * 1000);

    // 回写工具侧副本 + 账号池过期时间
    let creds = serde_json::json!({
        "access_token": new_access,
        "refresh_token": new_refresh,
        "expires_at_ms": exp_ms,
        "refresh_expires_at_ms": rexp_ms,
    });
    upsert_token_store(&state, &acct.id, &creds)?;
    if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == acct.id) {
        a.access_token_expires_at = exp_ms.map(|m| m / 1000);
        a.refresh_token_expires_at = rexp_ms.map(|m| m / 1000);
        a.needs_relogin = false;
        a.relogin_reason.clear();
    }
    save_pool(&state, &pool)?;
    fs_utils::app_log(&state.data_dir, &format!("WorkBuddy 凭证续期成功: {}", acct.id));
    Ok("凭证已续期".into())
}

// ── API 网关 WB 上游取号（T2.1）───────────────────────────────────────────

/// 汇总 WB 上游账号：账号池启用账号 + token store 凭证 → WbSyncAccount。
/// 区域判定（§5.2）：domain 含 `.workbuddy.ai` → Global（chat 全走 www.workbuddy.ai）。
/// 仅纳入有工具侧凭证副本的账号（auth 文件为只读态，不在此兜底）。
pub(crate) fn wb_upstream_accounts(state: &AppState) -> Vec<crate::api_server::pool::WbSyncAccount> {
    let pool = load_pool(state);
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(state));
    let tokens = store
        .get("tokens")
        .and_then(|t| t.as_object())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for a in &pool.accounts {
        let rec = tokens.get(&a.id).cloned().unwrap_or_default();
        let token = as_str(&dig(&rec, &["access_token"])).unwrap_or_default();
        if token.is_empty() {
            continue;
        }
        let domain = as_str(&dig(&rec, &["domain"])).unwrap_or_default();
        let eid = as_str(&dig(&rec, &["enterprise_id", "enterpriseId"])).unwrap_or_default();
        out.push(crate::api_server::pool::WbSyncAccount {
            uid: a.id.clone(),
            name: if a.nickname.is_empty() { a.id.clone() } else { a.nickname.clone() },
            token,
            domain: domain.clone(),
            enterprise_id: eid,
            global_region: domain.contains(".workbuddy.ai"),
            credits: a.credits_balance,
            needs_relogin: a.needs_relogin,
        });
    }
    out
}

// ── M4 签到（F-15，python NDJSON 管线）─────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct WbCheckinOpts {
    #[serde(default)]
    pub user_ids: Option<Vec<String>>,
    #[serde(default)]
    pub skip_checked_in: bool,
    #[serde(default)]
    pub skip_expired: bool,
    #[serde(default)]
    pub lazy_hours: Option<i64>,
}

/// 启动 WorkBuddy 签到（python workbuddy_checkin.py --json-stream），
/// NDJSON → `wb-checkin-progress` 事件（独立管线，避免与 Trae checkin 状态串扰）。
#[tauri::command(async)]
pub fn workbuddy_checkin_start(app: AppHandle, state: State<AppState>, opts: WbCheckinOpts) -> Result<(), String> {
    let mut args: Vec<String> = vec!["--json-stream".into()];
    if opts.skip_checked_in {
        args.push("--skip-checked".into());
    }
    if opts.skip_expired {
        args.push("--skip-expired".into());
    }
    if let Some(lh) = opts.lazy_hours {
        args.push("--lazy-hours".into());
        args.push(lh.to_string());
    }
    for uid in opts.user_ids.unwrap_or_default() {
        args.push("--uid".into());
        args.push(uid);
    }
    spawn_wb_script(app, &state, "workbuddy_checkin.py", &args, "wb-checkin-progress")
}

/// 成长中心执行（F-17，批次2 消费；批次1 端点已就绪时 python 会按开关执行）
#[tauri::command(async)]
pub fn workbuddy_growth_run(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let s = load_settings(&state);
    let mut args: Vec<String> = vec!["--growth".into()];
    if s.growth_travel { args.push("--growth-travel".into()); }
    if s.growth_lottery { args.push("--growth-lottery".into()); }
    if s.growth_tasks { args.push("--growth-tasks".into()); }
    spawn_wb_script(app, &state, "workbuddy_checkin.py", &args, "wb-checkin-progress")
}

/// 启动 python 脚本并把 stdout 逐行 emit 为 NDJSON 事件；done 行附带完成事件。
fn spawn_wb_script(app: AppHandle, state: &State<AppState>, script: &str, args: &[String], event: &str) -> Result<(), String> {
    let script_path = state.python_dir.join(script);
    if !script_path.exists() {
        return Err(format!("找不到脚本: {}", script_path.display()));
    }
    let mut cmd = Command::new(&state.python_exe);
    cmd.arg(&script_path)
        .args(args)
        .creation_flags(0x08000000)
        .env("AIWORKDATA_DIR", &state.data_dir)
        .env("PYTHONIOENCODING", "utf-8")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动脚本失败: {e}"))?;
    let stdout = child.stdout.take().ok_or("脚本无输出")?;
    let stderr = child.stderr.take();
    let app2 = app.clone();
    let ev = event.to_string();
    let data_dir = state.data_dir.clone();

    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let l = line.trim().to_string();
            if l.is_empty() {
                continue;
            }
            // done 行同时发独立 done 事件（前端据 "type":"done" 归约即可，无需额外事件名）
            let _ = app2.emit(&ev, &l);
        }
        let status = child.wait();
        let _ = app2.emit(&ev, format!("{{\"type\":\"exit\",\"ok\":{}}}", status.map(|s| s.success()).unwrap_or(false)));
        // stderr 落日志（防管道缓冲区写满死锁 + 保留排查线索）
        if let Some(stderr) = stderr {
            let reader = BufReader::new(stderr);
            for line in reader.lines().flatten() {
                fs_utils::app_log(&data_dir, &format!("[wb-script] {line}"));
            }
        }
    });
    Ok(())
}

// ── 签到结果 / 定时任务（F-15/F-16/F-55）───────────────────────────────────

#[derive(Serialize, Clone)]
pub struct WbCheckinRecord {
    pub date: String,
    pub time: String,
    pub user_id: String,
    pub name: String,
    pub status: String,
    pub message: String,
}

/// 签到日志（90 天存储，UI 默认展示 30 天）
#[tauri::command]
pub fn workbuddy_checkin_results(state: State<AppState>, days: Option<i64>) -> Result<Vec<WbCheckinRecord>, String> {
    let days = days.unwrap_or(30).clamp(1, 90);
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let raw: serde_json::Value = fs_utils::read_json(&checkin_results_path(&state));
    let mut out = Vec::new();
    if let Some(arr) = raw.get("results").and_then(|v| v.as_array()) {
        for r in arr {
            let date = r.get("date").and_then(|v| v.as_str()).unwrap_or("");
            if date < cutoff.as_str() {
                continue;
            }
            out.push(WbCheckinRecord {
                date: date.into(),
                time: r.get("time").and_then(|v| v.as_str()).unwrap_or("").into(),
                user_id: r.get("user_id").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: r.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                status: r.get("status").and_then(|v| v.as_str()).unwrap_or("").into(),
                message: r.get("message").and_then(|v| v.as_str()).unwrap_or("").into(),
            });
        }
    }
    out.reverse(); // 新→旧
    Ok(out)
}

/// 每日签到定时任务（F-16 双时段：每个时间一个任务，后缀 _HHMM）
const WB_CHECKIN_TASK_PREFIX: &str = "AIWorkAssistant_WorkBuddyCheckin";
const WB_RENEW_TASK_NAME: &str = "AIWorkAssistant_WorkBuddyRenew";

fn build_wb_task_tr(state: &AppState, script_args: &[&str]) -> String {
    let py = state.python_exe.replace('\\', "/");
    let script = state.python_dir.join("workbuddy_checkin.py").to_string_lossy().replace('\\', "/");
    let data_dir = state.data_dir.to_string_lossy().to_string();
    let args = script_args.join(" ");
    format!("cmd /c set \"AIWORKDATA_DIR={}\" && \"{}\" \"{}\" {}", data_dir, py, script, args)
}

fn run_schtasks(args: &[&str]) -> Result<(bool, String, String), String> {
    crate::commands::misc::run_schtasks(args)
}

fn task_exists(name: &str) -> bool {
    run_schtasks(&["/Query", "/TN", name, "/FO", "LIST"]).map(|(ok, _, _)| ok).unwrap_or(false)
}

#[tauri::command(async)]
pub fn workbuddy_checkin_task_register(state: State<AppState>, times: Vec<String>) -> Result<(), String> {
    if times.is_empty() {
        return Err("至少需要一个触发时间（如 09:00 / 21:00）".into());
    }
    let tr = build_wb_task_tr(&state, &["--json-stream", "--skip-checked"]);
    // 先清理旧实例（按前缀），保证重注册幂等
    for suffix in ["_0900", "_2100", "_1200"] {
        let _ = run_schtasks(&["/Delete", "/TN", &format!("{WB_CHECKIN_TASK_PREFIX}{suffix}"), "/F"]);
    }
    for t in &times {
        let hhmm = t.replace(':', "");
        let name = format!("{WB_CHECKIN_TASK_PREFIX}_{hhmm}");
        let (ok, _, stderr) = run_schtasks(&[
            "/Create", "/TN", &name, "/TR", &tr, "/SC", "DAILY", "/ST", t, "/F",
        ])?;
        if !ok {
            return Err(format!("注册任务 {t} 失败: {}", stderr.trim()));
        }
    }
    Ok(())
}

#[tauri::command(async)]
pub fn workbuddy_checkin_task_status() -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for suffix in ["_0900", "_2100", "_1200"] {
        let name = format!("{WB_CHECKIN_TASK_PREFIX}{suffix}");
        if task_exists(&name) {
            out.push(name.trim_start_matches("AIWorkAssistant_WorkBuddyCheckin_").replace('_', ":"));
        }
    }
    Ok(out)
}

#[tauri::command(async)]
pub fn workbuddy_checkin_task_unregister() -> Result<(), String> {
    for suffix in ["_0900", "_2100", "_1200"] {
        let _ = run_schtasks(&["/Delete", "/TN", &format!("{WB_CHECKIN_TASK_PREFIX}{suffix}"), "/F"]);
    }
    Ok(())
}

/// token 每周兜底续期任务（F-09；python --renew-only 惰性刷新）
#[tauri::command(async)]
pub fn workbuddy_renew_task_register(state: State<AppState>, day: String) -> Result<(), String> {
    // day: MON..SUN（schtasks /SC WEEKLY /D）；默认 SUN
    let d = if day.is_empty() { "SUN".to_string() } else { day.to_uppercase() };
    let tr = build_wb_task_tr(&state, &["--renew-only"]);
    let (ok, _, stderr) = run_schtasks(&[
        "/Create", "/TN", WB_RENEW_TASK_NAME, "/TR", &tr, "/SC", "WEEKLY", "/D", &d, "/ST", "10:30", "/F",
    ])?;
    if !ok {
        return Err(format!("注册续期任务失败: {}", stderr.trim()));
    }
    Ok(())
}

#[tauri::command]
pub fn workbuddy_renew_task_status() -> bool {
    task_exists(WB_RENEW_TASK_NAME)
}

#[tauri::command(async)]
pub fn workbuddy_renew_task_unregister() -> Result<(), String> {
    let _ = run_schtasks(&["/Delete", "/TN", WB_RENEW_TASK_NAME, "/F"]);
    Ok(())
}

// ── M5 积分（F-20/F-22，python 三件套 + 缓存）──────────────────────────────

#[tauri::command(async)]
pub fn workbuddy_credits_fetch(app: AppHandle, state: State<AppState>, user_id: Option<String>, fresh: Option<bool>) -> Result<serde_json::Value, String> {
    let _ = &app; // 预留：wb-credits-updated 事件随批次2趋势图启用
    let mut args: Vec<String> = Vec::new();
    if let Some(uid) = &user_id {
        args.push("--uid".into());
        args.push(uid.clone());
    }
    if fresh.unwrap_or(false) {
        args.push("--fresh".into());
    }
    let script_path = state.python_dir.join("workbuddy_credits.py");
    if !script_path.exists() {
        return Err(format!("找不到脚本: {}", script_path.display()));
    }
    let out = Command::new(&state.python_exe)
        .arg(&script_path)
        .args(&args)
        .creation_flags(0x08000000)
        .env("AIWORKDATA_DIR", &state.data_dir)
        .env("PYTHONIOENCODING", "utf-8")
        .output()
        .map_err(|e| format!("积分查询失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // 取末行 JSON（脚本可能输出告警行）
    let parsed = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .ok_or_else(|| format!("积分查询输出无法解析: {}", stdout.trim().chars().take(200).collect::<String>()))?;
    if parsed.get("ok") != Some(&serde_json::json!(true)) {
        return Err("积分查询失败（详见脚本输出）".into());
    }
    // 回写账号池余额缓存（列表/概述展示）
    if let Some(accounts) = parsed.get("accounts").and_then(|v| v.as_array()) {
        let mut pool = load_pool(&state);
        for acc in accounts {
            let uid = acc.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
            let bal = acc.get("balance").and_then(|v| v.as_f64());
            if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == uid) {
                a.credits_balance = bal;
                a.credits_fetched_at = acc.get("fetched_at").and_then(|v| v.as_str()).map(|s| s.to_string());
            }
        }
        let _ = save_pool(&state, &pool);
    }
    // 每日余额快照（F-27 数据源）：非缓存命中时追加，按日去重，cap 365 天
    if parsed.get("cached") != Some(&serde_json::json!(true)) {
        append_credits_snapshot(&state, &parsed);
    }
    Ok(parsed)
}

/// 追加每日积分余额快照（F-27）：workbuddy_credits_history.json，同日覆盖最新
fn append_credits_snapshot(state: &AppState, parsed: &Value) {
    let path = state.data_dir.join("data").join("workbuddy_credits_history.json");
    let mut hist: Value = fs_utils::read_json(&path);
    if !hist.is_object() {
        hist = serde_json::json!({});
    }
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let accounts: Vec<Value> = parsed
        .get("accounts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|a| {
                    serde_json::json!({
                        "user_id": a.get("user_id").cloned().unwrap_or_default(),
                        "balance": a.get("balance").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let total: f64 = accounts.iter().filter_map(|a| a.get("balance").and_then(Value::as_f64)).sum();
    let snap = serde_json::json!({
        "date": today,
        "ts": chrono::Utc::now().timestamp_millis(),
        "total_balance": total,
        "accounts": accounts,
    });
    let Some(obj) = hist.as_object_mut() else { return };
    let arr = obj.entry("snapshots".to_string()).or_insert_with(|| serde_json::json!([]));
    if let Some(list) = arr.as_array_mut() {
        match list.iter().position(|s| s.get("date").and_then(Value::as_str) == Some(today.as_str())) {
            Some(pos) => list[pos] = snap,
            None => list.push(snap),
        }
        let len = list.len();
        if len > 365 {
            list.drain(..len - 365);
        }
    }
    let _ = fs_utils::write_json(&path, &hist);
}

// ── 积分用量快照回退（T4.3/F-27）────────────────────────────────────────────

/// 快照回退用量：官方用量不可用时自动切换数据源（F-27）。
/// 推导：当日消耗 = 前一日总余额 − 当日总余额 + 当日签到奖励（签到日志 message「+N」）；
/// 负差值（充值包到账/快照波动）记 0。口径明示「快照回退」，非官方逐请求统计。
#[tauri::command]
pub fn workbuddy_usage_fallback(state: State<AppState>) -> Result<serde_json::Value, String> {
    let hist: Value = fs_utils::read_json(&state.data_dir.join("data").join("workbuddy_credits_history.json"));
    let snapshots = hist.get("snapshots").and_then(Value::as_array).cloned().unwrap_or_default();
    if snapshots.len() < 2 {
        return Err(
            "快照回退不可用：本地余额时序不足（至少两天快照）。请在积分页刷新几次建立时序后重试。".to_string(),
        );
    }

    // 签到日志 → 每日奖励充值（90 天滚动，仅 success 事件）
    let results: Value = fs_utils::read_json(&state.data_dir.join("data").join("workbuddy_checkin_results.json"));
    let mut recharge: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for r in results.get("results").and_then(Value::as_array).into_iter().flatten() {
        if r.get("status").and_then(Value::as_str) != Some("success") {
            continue;
        }
        let Some(date) = r.get("date").and_then(Value::as_str) else { continue };
        let msg = r.get("message").and_then(Value::as_str).unwrap_or("");
        if let Some(v) = parse_reward_plus(msg) {
            *recharge.entry(date.to_string()).or_insert(0.0) += v;
        }
    }

    // 快照差分（快照按 date 升序，credits 追加时保序）
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut daily: Vec<Value> = vec![];
    for i in 1..snapshots.len() {
        let (Some(prev_bal), Some(cur_bal)) = (
            snapshots[i - 1].get("total_balance").and_then(Value::as_f64),
            snapshots[i].get("total_balance").and_then(Value::as_f64),
        ) else {
            continue;
        };
        let Some(date) = snapshots[i].get("date").and_then(Value::as_str) else { continue };
        let reward = recharge.get(date).copied().unwrap_or(0.0);
        let usage = (prev_bal - cur_bal + reward).max(0.0);
        daily.push(serde_json::json!({ "date": date, "usage": usage }));
    }

    // 聚合：今日/近 7 天/本月（口径与官方用量对齐）
    use chrono::Datelike;
    let now = chrono::Local::now().date_naive();
    let mut usage_today = 0.0f64;
    let mut usage_week = 0.0f64;
    let mut usage_month = 0.0f64;
    for d in &daily {
        let (Some(date), Some(u)) = (d.get("date").and_then(Value::as_str), d.get("usage").and_then(Value::as_f64)) else {
            continue;
        };
        let Ok(day) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") else { continue };
        let dist = (now - day).num_days();
        if dist == 0 {
            usage_today += u;
        }
        if (0..7).contains(&dist) {
            usage_week += u;
        }
        if day.year() == now.year() && day.month() == now.month() {
            usage_month += u;
        }
    }

    Ok(serde_json::json!({
        "status": "snapshot",
        "snapshot_days": snapshots.len(),
        "summary": {
            "usage_today": usage_today,
            "usage_7days": usage_week,
            "usage_this_month": usage_month,
        },
        "daily": daily,
        "note": "快照回退数据源（本地余额时序差分 + 签到日志推导），非官方逐请求口径",
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        "_today": today,
    }))
}

/// 从签到 message 提取「+N」奖励数额（不硬编码数额，仅解析接口回显）
fn parse_reward_plus(msg: &str) -> Option<f64> {
    let idx = msg.find('+')?;
    let rest = &msg[idx + 1..];
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    num.parse::<f64>().ok().filter(|v| *v > 0.0)
}

// ── UI 坐标点击签到兜底（T4.2/F-18）────────────────────────────────────────
// 无 API 可用时的最后手段：仅手动触发、默认关闭（ui_click_enabled）；
// 坐标由用户「取点」预配置；单次执行只单击一次，不循环连点；零 token 输出。

#[tauri::command]
pub fn workbuddy_ui_click_capture(state: State<AppState>) -> Result<serde_json::Value, String> {
    run_ui_click_script(&state, vec!["--capture".to_string()])
}

#[tauri::command]
pub fn workbuddy_ui_click_checkin(state: State<AppState>) -> Result<serde_json::Value, String> {
    let s = load_settings(&state);
    if !s.ui_click_enabled {
        return Err("UI 坐标点击兜底未启用：请在设置中显式开启（F-18 仅作 API 不可用时的最后手段）".to_string());
    }
    if s.ui_click_x <= 0 || s.ui_click_y <= 0 {
        return Err("签到按钮坐标未配置：请先在客户端打开签到页，再用「取点」记录按钮位置".to_string());
    }
    run_ui_click_script(
        &state,
        vec![
            "--click".to_string(),
            "--x".to_string(),
            s.ui_click_x.to_string(),
            "--y".to_string(),
            s.ui_click_y.to_string(),
        ],
    )
}

fn run_ui_click_script(state: &AppState, args: Vec<String>) -> Result<serde_json::Value, String> {
    let script_path = state.python_dir.join("workbuddy_ui_click.py");
    if !script_path.exists() {
        return Err(format!("找不到脚本: {}", script_path.display()));
    }
    let out = Command::new(&state.python_exe)
        .arg(&script_path)
        .args(&args)
        .creation_flags(0x08000000)
        .env("PYTHONIOENCODING", "utf-8")
        .output()
        .map_err(|e| format!("UI 点击执行失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .ok_or_else(|| format!("UI 点击脚本输出无法解析: {}", stdout.trim().chars().take(120).collect::<String>()))
}

// ── 设置（F-55 配置化）──────────────────────────────────────────────────────

#[tauri::command]
pub fn workbuddy_settings_get(state: State<AppState>) -> WorkBuddySettings {
    load_settings(&state)
}

#[tauri::command]
pub fn workbuddy_settings_set(state: State<AppState>, patch: WorkBuddySettings) -> Result<(), String> {
    fs_utils::write_json(&settings_path(&state), &patch)
}

// ── CodeBuddy CLI 切号桥 + 五重防护自动轮换（F-06/F-59，批次3 T3.4）─────────
// 纯逻辑（decide_target / settings env token JSON 操作 / 活动扫描）在 workbuddy_cli.rs；
// 本节为有状态粘合：账号池 / token store / 轮换状态文件 / 后台线程。
// 凭证红线：token 不进日志、不进返回值（activeAccountId 为池内稳定 id）。

fn cli_settings_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    PathBuf::from(home).join(".codebuddy").join("settings.json")
}

fn cli_rotate_state_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("wb_cli_rotate_state.json")
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct CliRotateState {
    #[serde(default)]
    last_switch_at_ms: Option<i64>,
    #[serde(default)]
    active_account_id: Option<String>,
    /// 轮换日志（旧→新，cap 50）
    #[serde(default)]
    logs: Vec<serde_json::Value>,
}

fn load_cli_rotate_state(state: &AppState) -> CliRotateState {
    fs_utils::read_json(&cli_rotate_state_path(state))
}

fn append_cli_log(_state: &AppState, st: &mut CliRotateState, mut entry: serde_json::Value) {
    if entry.get("ts").is_none() {
        entry["ts"] = serde_json::json!(chrono::Utc::now().timestamp_millis());
    }
    st.logs.push(entry);
    let len = st.logs.len();
    if len > 50 {
        st.logs.drain(..len - 50);
    }
}

/// CLI 当前账号 = settings.json env token 的稳定池 id（wb-<sha256 前 12 位>）。
fn cli_current_account_id() -> Option<String> {
    let raw = fs_utils::read_json::<serde_json::Value>(&cli_settings_path());
    workbuddy_cli::settings_env_token(&raw).map(|t| account_id_of(&t))
}

/// 从 token store 取账号 access_token（CLI 桥唯一凭证来源；auth 文件只读态不入桥）。
fn cli_token_of(state: &AppState, account_id: &str) -> Option<String> {
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(state));
    let rec = store.get("tokens").and_then(|t| t.get(account_id)).cloned().unwrap_or_default();
    as_str(&dig(&rec, &["access_token"])).filter(|t| !t.is_empty())
}

/// 轮换候选：积分缓存（credits_fetch 回写）+ 账号池凭证 → CliCandidate。
/// 缓存缺失/无凭证 → invalid 候选（供日志与防横跳参照，不作为目标）。
fn cli_candidates(state: &AppState) -> Vec<workbuddy_cli::CliCandidate> {
    let cache: serde_json::Value =
        fs_utils::read_json(&state.data_dir.join("data").join("workbuddy_credits_cache.json"));
    let cache_accounts = cache.get("accounts").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let pool = load_pool(state);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let has_store = |id: &str| cli_token_of(state, id).is_some();

    pool.accounts
        .iter()
        .map(|a| {
            let display = if a.nickname.is_empty() { a.id.clone() } else { a.nickname.clone() };
            if !has_store(&a.id) {
                return workbuddy_cli::CliCandidate {
                    account_id: a.id.clone(),
                    display_name: display,
                    soonest_expire_at_ms: None,
                    total_remaining: 0.0,
                    valid: false,
                    error: Some("无工具侧凭证副本".into()),
                };
            }
            let Some(acc) = cache_accounts
                .iter()
                .find(|c| c.get("user_id").and_then(|v| v.as_str()) == Some(a.id.as_str()))
            else {
                return workbuddy_cli::CliCandidate {
                    account_id: a.id.clone(),
                    display_name: display,
                    soonest_expire_at_ms: None,
                    total_remaining: 0.0,
                    valid: false,
                    error: Some("积分缓存缺失（请先在积分页刷新）".into()),
                };
            };
            // 未过期且仍有剩余的包 → 合计剩余 + 最早到期（缓存 expire_ts 为 Unix 秒）
            let mut total = 0.0f64;
            let mut soonest: Option<i64> = None;
            if let Some(pkgs) = acc.get("packages").and_then(|v| v.as_array()) {
                for p in pkgs {
                    let remaining = p.get("remaining").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let expire_s = p.get("expire_ts").and_then(|v| v.as_i64());
                    let expired = matches!(expire_s, Some(s) if s * 1000 <= now_ms);
                    if !expired && remaining > 0.0 {
                        total += remaining;
                        if let Some(s) = expire_s {
                            let ms = s * 1000;
                            soonest = Some(soonest.map_or(ms, |cur| cur.min(ms)));
                        }
                    }
                }
            }
            workbuddy_cli::CliCandidate {
                account_id: a.id.clone(),
                display_name: display,
                soonest_expire_at_ms: soonest,
                total_remaining: total,
                valid: total > 0.0,
                error: if total > 0.0 { None } else { Some("无剩余积分（或已全部过期）".into()) },
            }
        })
        .collect()
}

fn cli_rotate_config(state: &AppState) -> (i64, i64, i64, i64, i64, f64) {
    let s = load_settings(state);
    (
        s.cli_cooldown_minutes.max(1) * 60_000,
        s.cli_min_gap_hours.max(0) * 3600_000,
        s.cli_min_urgency_hours.max(0) * 3600_000,
        s.cli_active_guard_minutes.max(0) * 60_000,
        s.cli_rotate_interval_minutes.max(5),
        s.cli_min_remaining_credits.max(0.0),
    )
}

/// CLI 轮换状态（当前 CLI 账号 + 五重防护配置 + 上次切换），供前端展示。
fn cli_status_value(state: &AppState) -> serde_json::Value {
    let st = load_cli_rotate_state(state);
    let s = load_settings(state);
    let settings_raw = fs_utils::read_json::<serde_json::Value>(&cli_settings_path());
    let token_present = workbuddy_cli::settings_env_token(&settings_raw).is_some();
    let current_id = cli_current_account_id();
    let pool = load_pool(state);
    let active_name = current_id
        .as_ref()
        .and_then(|id| pool.accounts.iter().find(|a| &a.id == id))
        .map(|a| if a.nickname.is_empty() { a.id.clone() } else { a.nickname.clone() });
    serde_json::json!({
        "settings_present": cli_settings_path().is_file(),
        "env_token_present": token_present,
        // 进程环境变量会覆盖 settings.json（CLI 启动时取 env 优先）——检测并提示
        "environment_override": std::env::var_os(workbuddy_cli::AUTH_ENV_KEY)
            .map(|v| !v.to_string_lossy().trim().is_empty())
            .unwrap_or(false),
        "active_account_id": current_id,
        "active_account_name": active_name,
        "recent_activity_ms": workbuddy_cli::cli_recent_activity(),
        "last_switch_at_ms": st.last_switch_at_ms,
        "config": {
            "cli_rotate_enabled": s.cli_rotate_enabled,
            "cli_rotate_interval_minutes": s.cli_rotate_interval_minutes,
            "cli_cooldown_minutes": s.cli_cooldown_minutes,
            "cli_min_gap_hours": s.cli_min_gap_hours,
            "cli_min_urgency_hours": s.cli_min_urgency_hours,
            "cli_active_guard_minutes": s.cli_active_guard_minutes,
            "cli_min_remaining_credits": s.cli_min_remaining_credits,
        },
    })
}

/// 写 settings.json env 认证（保留其余字段；写入后回读校验）。
fn apply_cli_token(token: &str) -> Result<(), String> {
    let path = cli_settings_path();
    let mut value: serde_json::Value = fs_utils::read_json(&path);
    if !value.is_object() {
        value = serde_json::json!({});
    }
    workbuddy_cli::with_env_token(&mut value, token)?;
    fs_utils::write_json(&path, &value)?;
    // 回读校验：写入的认证信息必须与目标账号一致
    let verify = fs_utils::read_json::<serde_json::Value>(&path);
    if workbuddy_cli::settings_env_token(&verify).as_deref() != Some(workbuddy_cli::clean_bearer(token)) {
        return Err("写入后回读校验失败：CodeBuddy settings.json 认证信息与目标账号不一致".into());
    }
    Ok(())
}

#[tauri::command]
pub fn workbuddy_cli_status(state: State<AppState>) -> serde_json::Value {
    cli_status_value(&state)
}

/// 手动切 CLI 账号（F-06 桥）：token store 凭证 → settings.json env，记录状态与日志。
#[tauri::command(async)]
pub fn workbuddy_cli_bridge_set(state: State<AppState>, user_id: String) -> Result<serde_json::Value, String> {
    let pool = load_pool(&state);
    let acct = pool
        .accounts
        .iter()
        .find(|a| a.id == user_id)
        .ok_or_else(|| format!("账号不存在: {user_id}"))?;
    let display = if acct.nickname.is_empty() { acct.id.clone() } else { acct.nickname.clone() };
    let token = cli_token_of(&state, &acct.id)
        .ok_or_else(|| "该账号无工具侧凭证副本，请先导入凭证（auth 导入或续期回写）".to_string())?;

    let previous = std::fs::read_to_string(cli_settings_path()).ok();
    apply_cli_token(&token).map_err(|e| {
        // 写入失败回滚：恢复写入前的 settings.json 原文
        if let Some(prev) = &previous {
            let _ = std::fs::write(cli_settings_path(), prev);
        }
        e
    })?;

    let mut st = load_cli_rotate_state(&state);
    st.active_account_id = Some(acct.id.clone());
    append_cli_log(
        &state,
        &mut st,
        serde_json::json!({
            "action": "manual",
            "to": {"id": acct.id, "name": display},
            "reason": "手动切换 CLI 账号",
        }),
    );
    fs_utils::write_json(&cli_rotate_state_path(&state), &st)?;
    fs_utils::app_log(&state.data_dir, &format!("CodeBuddy CLI 手动切号: {} ({})", display, acct.id));
    Ok(cli_status_value(&state))
}

/// 执行一轮五重防护轮换（F-59）：候选来自积分缓存 → decide_target → 写 CLI settings。
#[tauri::command(async)]
pub fn workbuddy_cli_rotate_run(state: State<AppState>) -> serde_json::Value {
    cli_rotate_cycle(&state)
}

/// 轮换日志（新→旧，供前端展示；无凭证字段）。
#[tauri::command]
pub fn workbuddy_cli_rotate_logs(state: State<AppState>, limit: Option<usize>) -> Vec<serde_json::Value> {
    let st = load_cli_rotate_state(&state);
    let mut logs = st.logs;
    logs.reverse();
    logs.into_iter().take(limit.unwrap_or(20)).collect()
}

/// 轮换周期核心（手动命令与后台线程共用；线程路径静默，不弹通知）。
fn cli_rotate_cycle(state: &AppState) -> serde_json::Value {
    let (cooldown_ms, gap_ms, urgency_ms, guard_ms, _interval, min_remaining) = cli_rotate_config(state);
    let candidates = cli_candidates(state);
    let current = cli_current_account_id();
    let st0 = load_cli_rotate_state(state);
    let now_ms = chrono::Utc::now().timestamp_millis();

    let decision = workbuddy_cli::decide_target(
        &candidates,
        current.as_deref(),
        now_ms,
        st0.last_switch_at_ms,
        cooldown_ms,
        gap_ms,
        urgency_ms,
        workbuddy_cli::cli_recent_activity(),
        guard_ms,
        min_remaining,
    );

    // 候选快照入日志（供观察后调整 min_remaining 等阈值）
    let detail: Vec<serde_json::Value> = candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.display_name,
                "remaining": c.total_remaining,
                "soonest_expire_at": c.soonest_expire_at_ms,
                "valid": c.valid,
                "error": c.error,
            })
        })
        .collect();
    let mut entry = serde_json::json!({
        "ts": now_ms,
        "action": "noop",
        "reason": serde_json::Value::Null,
        "from": current,
        "to": serde_json::Value::Null,
        "detail": detail,
    });

    let result = match &decision {
        workbuddy_cli::RotateDecision::Skip(reason) => {
            entry["action"] = serde_json::json!("skipped");
            entry["reason"] = serde_json::json!(reason);
            serde_json::json!({"status": "skipped", "reason": reason})
        }
        workbuddy_cli::RotateDecision::Switch(target_id) => {
            let display = candidates
                .iter()
                .find(|c| &c.account_id == target_id)
                .map(|c| c.display_name.clone())
                .unwrap_or_else(|| target_id.clone());
            match cli_token_of(state, target_id)
                .ok_or_else(|| "目标账号无工具侧凭证副本".to_string())
                .and_then(|token| {
                    let previous = std::fs::read_to_string(cli_settings_path()).ok();
                    apply_cli_token(&token).map_err(|e| {
                        if let Some(prev) = &previous {
                            let _ = std::fs::write(cli_settings_path(), prev);
                        }
                        e
                    })
                }) {
                Ok(()) => {
                    entry["action"] = serde_json::json!("switched");
                    entry["to"] = serde_json::json!({"id": target_id, "name": display});
                    serde_json::json!({"status": "switched", "to": {"id": target_id, "name": display}})
                }
                Err(e) => {
                    entry["action"] = serde_json::json!("error");
                    entry["reason"] = serde_json::json!(e);
                    serde_json::json!({"status": "error", "error": e})
                }
            }
        }
    };

    let mut st = st0;
    if result.get("status") == Some(&serde_json::json!("switched")) {
        st.last_switch_at_ms = Some(now_ms);
        st.active_account_id = result
            .pointer("/to/id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    append_cli_log(state, &mut st, entry);
    let _ = fs_utils::write_json(&cli_rotate_state_path(state), &st);
    if result.get("status") == Some(&serde_json::json!("switched")) {
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "CodeBuddy CLI 自动轮换: → {}",
                result.pointer("/to/name").and_then(|v| v.as_str()).unwrap_or("?")
            ),
        );
    }
    result
}

/// 后台轮换线程（F-59 检查间隔）：按配置间隔静默执行；开关关闭时空转。
/// 独立重建 AppState（cli_rotate_cycle 仅依赖 data_dir 下文件，无 UI 事件）。
pub fn start_cli_rotate_thread() {
    std::thread::spawn(move || {
        loop {
            // 先睡再查：避开启动高峰；间隔每轮重读（配置可随时改）
            let state = match AppState::new() {
                Ok(s) => s,
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_secs(300));
                    continue;
                }
            };
            let s = load_settings(&state);
            let interval = s.cli_rotate_interval_minutes.max(5) as u64;
            std::thread::sleep(std::time::Duration::from_secs(interval * 60));
            if !s.cli_rotate_enabled {
                continue;
            }
            let _ = cli_rotate_cycle(&state);
        }
    });
}

// ── 账号库导入导出扩展（F-46 扩展，批次3 T3.6）────────────────────────────

/// 导出账号池（F-46 扩展）：元数据必含；include_credentials=true 时附工具侧凭证副本
///（迁移场景用；导出文件等同密码，由调用方提示）。与 Trae accounts_export_raw 同语义。
#[tauri::command(async)]
pub fn workbuddy_accounts_export(state: State<AppState>, include_credentials: Option<bool>) -> Result<serde_json::Value, String> {
    let pool = load_pool(&state);
    let include_cred = include_credentials.unwrap_or(false);
    let store: serde_json::Value = if include_cred {
        fs_utils::read_json(&token_store_path(&state))
    } else {
        serde_json::json!({})
    };
    let tokens = store.get("tokens").cloned().unwrap_or(serde_json::Value::Null);
    let accounts: Vec<serde_json::Value> = pool
        .accounts
        .iter()
        .map(|a| {
            let mut v = serde_json::json!({
                "id": a.id,
                "uid": a.uid,
                "nickname": a.nickname,
                "phone_masked": a.phone_masked,
                "edition_type": a.edition_type,
                "access_token_expires_at": a.access_token_expires_at,
                "refresh_token_expires_at": a.refresh_token_expires_at,
                "group_id": a.group_id,
                "note": a.note,
            });
            if include_cred {
                v["credential"] = tokens.get(&a.id).cloned().unwrap_or(serde_json::Value::Null);
            }
            v
        })
        .collect();
    Ok(serde_json::json!({
        "kind": "aiwork-workbuddy-pool",
        "version": 1,
        "exported_at": fs_utils::now_iso(),
        "include_credentials": include_cred,
        "accounts": accounts,
    }))
}

/// 账号库导入（F-46 扩展）：解析导出文件 → 逐账号入池（已存在跳过）+ 凭证回写 token store。
#[tauri::command(async)]
pub fn workbuddy_accounts_import(state: State<AppState>, payload: serde_json::Value) -> Result<serde_json::Value, String> {
    if payload.get("kind").and_then(|v| v.as_str()) != Some("aiwork-workbuddy-pool") {
        return Err("文件格式无法识别（缺少 aiwork-workbuddy-pool 标记）".into());
    }
    let accounts = payload
        .get("accounts")
        .and_then(|v| v.as_array())
        .ok_or("导出文件缺少 accounts 数组")?;
    let mut added = 0usize;
    let mut skipped = 0usize;
    let mut with_cred = 0usize;
    let mut pool = load_pool(&state);
    for a in accounts {
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if id.is_empty() {
            continue;
        }
        if pool.accounts.iter().any(|x| x.id == id) {
            skipped += 1;
            continue;
        }
        pool.accounts.push(WorkBuddyAccount {
            id: id.clone(),
            uid: a.get("uid").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            nickname: a.get("nickname").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            phone_masked: a.get("phone_masked").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            edition_type: a.get("edition_type").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            access_token_expires_at: a.get("access_token_expires_at").and_then(|v| v.as_i64()),
            refresh_token_expires_at: a.get("refresh_token_expires_at").and_then(|v| v.as_i64()),
            group_id: a.get("group_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            note: a.get("note").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            auth_saved_at: Some(chrono::Utc::now().timestamp()),
            ..Default::default()
        });
        added += 1;
        // 凭证副本回写（导出时含凭证才有效）
        if let Some(cred) = a.get("credential").filter(|c| c.is_object()) {
            let rec = cred.clone();
            upsert_token_store(&state, &id, &rec)?;
            with_cred += 1;
        }
    }
    save_pool(&state, &pool)?;
    fs_utils::app_log(&state.data_dir, &format!("workbuddy: 账号库导入 新增 {added} / 跳过 {skipped} / 带凭证 {with_cred}"));
    Ok(serde_json::json!({ "added": added, "skipped": skipped, "with_credentials": with_cred }))
}

// ── M8 会话三件套备份/恢复（F-44，批次3 T3.1）─────────────────────────────
// 三件套（缺一不可，§3.11）：
//   ① 正文 ~/.workbuddy/projects/{workspace}/{cid}.jsonl（每行含 sessionId）
//   ② 元数据 ~/.workbuddy/workbuddy.db（sessions 表，id = 会话 UUID）
//   ③ 云端映射 ~/.workbuddy/edge-sync-mapping-v2.db（edge_sync_mapping 表，msg_channel=convmsg:{uid}）
// 备份 = 整目录 + 双 db 快照至 data/workbuddy_chats/<uid>/；执行前先优雅关闭客户端。

fn wb_chats_dir() -> PathBuf {
    wb_data_dir().join("projects")
}

fn wb_chat_backup_root(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("workbuddy_chats")
}

/// 会话三件套命令的 user_id 入参防护（审查 P0-1）：路径段只允许池内账号 id，
/// 且必须匹配白名单字符集——杜绝 `..`/绝对路径/分隔符注入导致的目录逃逸
/// （backup 对该路径有 remove_dir_all，逃逸即任意目录删除，restore/info 可读任意路径）。
fn wb_chat_uid_guard(state: &AppState, user_id: &str) -> Result<(), String> {
    if user_id.is_empty()
        || user_id.len() > 64
        || !user_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || user_id.contains("..")
    {
        return Err(format!("非法账号标识: {user_id}"));
    }
    let pool = load_pool(state);
    if !pool.accounts.iter().any(|a| a.id == user_id) {
        return Err(format!("账号不在池中: {user_id}"));
    }
    Ok(())
}

/// 备份当前 ~/.workbuddy 会话三件套（先优雅关闭 WorkBuddy）。
/// 覆盖式备份（保留最新一份），返回 {ok, files, path}。
#[tauri::command(async)]
pub fn workbuddy_chatdata_backup(state: State<AppState>, user_id: String) -> Result<serde_json::Value, String> {
    wb_chat_uid_guard(&state, &user_id)?;
    if !wb_data_dir().is_dir() {
        return Err("未找到 WorkBuddy 数据目录（~/.workbuddy），请先安装并登录".into());
    }
    if !wb_chats_dir().is_dir() {
        return Err("未发现会话正文目录（~/.workbuddy/projects 为空）".into());
    }
    crate::commands::process::graceful_kill_app("WorkBuddy")?;

    let dest_root = wb_chat_backup_root(&state).join(&user_id);
    // 先写临时目录（.staging）：复制中断不毁旧备份；校验通过后原子替换（审查 P1-3）
    let staging = wb_chat_backup_root(&state).join(format!("{user_id}.staging"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("创建备份目录失败: {e}"))?;

    // ① 会话正文整目录
    let files = crate::state::copy_dir_recursive(
        &wb_chats_dir(),
        &staging.join("projects"),
        &[],
    )?;
    // ②③ 双 db 快照（SQLite 文件级拷贝；客户端已关闭保证一致性）
    let mut db_files = 0usize;
    for db in ["workbuddy.db", "edge-sync-mapping-v2.db"] {
        let src = wb_data_dir().join(db);
        if src.is_file() {
            std::fs::copy(&src, staging.join(db)).map_err(|e| format!("复制 {db} 失败: {e}"))?;
            db_files += 1;
        }
    }
    // 三件套完整性：正文必须有，双 db 至少其一（旧版本客户端可能无 edge db）
    if files == 0 || db_files == 0 {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("备份不完整（正文或 workbuddy.db 缺失），已放弃写入（旧备份保持原状）".into());
    }
    let meta = serde_json::json!({
        "schemaVersion": 1, "user_id": user_id, "files": files + db_files,
        "has_edge_mapping": db_files == 2,
        "backedAt": fs_utils::now_ts(),
    });
    let _ = std::fs::write(
        staging.join("chat_backup_meta.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );
    // 校验通过 → 替换旧份（旧份删除失败不致命：目录被占用时保留旧份，下次覆盖）
    let _ = std::fs::remove_dir_all(&dest_root);
    std::fs::rename(&staging, &dest_root).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        format!("备份目录替换失败: {e}")
    })?;
    fs_utils::app_log(&state.data_dir, &format!("workbuddy: 会话三件套已备份 {user_id}（{files} 正文 + {db_files} db）"));
    Ok(serde_json::json!({ "ok": true, "files": files + db_files, "path": dest_root.display().to_string() }))
}

/// 恢复会话三件套到 ~/.workbuddy（先优雅关闭 WorkBuddy；恢复前自动快照现有数据到 .bak）。
#[tauri::command(async)]
pub fn workbuddy_chatdata_restore(state: State<AppState>, user_id: String) -> Result<serde_json::Value, String> {
    wb_chat_uid_guard(&state, &user_id)?;
    let backup = wb_chat_backup_root(&state).join(&user_id);
    if !backup.is_dir() {
        return Err(format!("该账号没有会话备份：{}", backup.display()));
    }
    let projects_backup = backup.join("projects");
    if !projects_backup.is_dir() {
        return Err("备份缺少 projects 正文目录（备份不完整）".into());
    }
    crate::commands::process::graceful_kill_app("WorkBuddy")?;

    // 恢复前保护现场：现有 projects/db → 同名 .bak（单代，成功后保留供手动回退）
    if wb_chats_dir().is_dir() {
        let bak = wb_data_dir().join("projects.bak");
        let _ = std::fs::remove_dir_all(&bak);
        std::fs::rename(&wb_chats_dir(), &bak).map_err(|e| format!("快照现有 projects 失败: {e}"))?;
    }
    std::fs::create_dir_all(wb_chats_dir()).map_err(|e| format!("重建 projects 目录失败: {e}"))?;
    for db in ["workbuddy.db", "edge-sync-mapping-v2.db"] {
        let src = wb_data_dir().join(db);
        if src.is_file() {
            let _ = std::fs::rename(&src, wb_data_dir().join(format!("{db}.bak")));
        }
    }

    // 恢复复制阶段失败 → 自动从 .bak 回滚（审查 P1-1：防"半恢复 + db 已挪走"悬挂态）
    macro_rules! rollback_on_fail {
        ($expr:expr, $msg:expr) => {
            match $expr {
                Ok(v) => v,
                Err(e) => {
                    // 回滚：projects.bak → projects、*.db.bak → *.db
                    let bak = wb_data_dir().join("projects.bak");
                    if bak.is_dir() {
                        let _ = std::fs::remove_dir_all(wb_chats_dir());
                        let _ = std::fs::rename(&bak, wb_chats_dir());
                    }
                    for db in ["workbuddy.db", "edge-sync-mapping-v2.db"] {
                        let dbbak = wb_data_dir().join(format!("{db}.bak"));
                        if dbbak.is_file() && !wb_data_dir().join(db).exists() {
                            let _ = std::fs::rename(&dbbak, wb_data_dir().join(db));
                        }
                    }
                    return Err(format!("{}: {e}（已自动回滚到恢复前现场）", $msg));
                }
            }
        };
    }

    let files = rollback_on_fail!(
        crate::state::copy_dir_recursive(&projects_backup, &wb_chats_dir(), &[]),
        "恢复会话正文失败"
    );
    let mut db_files = 0usize;
    for db in ["workbuddy.db", "edge-sync-mapping-v2.db"] {
        let src = backup.join(db);
        if src.is_file() {
            rollback_on_fail!(
                std::fs::copy(&src, wb_data_dir().join(db)).map(|_| ()),
                format!("恢复 {db} 失败").as_str()
            );
            db_files += 1;
        }
    }
    if files == 0 || db_files == 0 {
        return Err(format!("恢复失败（正文 {files} 文件 + {db_files} db），现场已保留 .bak 可手动回退"));
    }
    fs_utils::app_log(&state.data_dir, &format!("workbuddy: 会话三件套已恢复 {user_id}（{files} 正文 + {db_files} db）"));
    Ok(serde_json::json!({ "ok": true, "files": files + db_files }))
}

/// 会话备份状态（供账号卡片展示：是否有备份 / 时间 / 体积）。
#[tauri::command]
pub fn workbuddy_chatdata_info(state: State<AppState>, user_id: String) -> Result<serde_json::Value, String> {
    wb_chat_uid_guard(&state, &user_id)?;
    let dir = wb_chat_backup_root(&state).join(&user_id);
    if !dir.is_dir() {
        return Ok(serde_json::json!({ "backed": false }));
    }
    let (size, files) = crate::commands::profile::dir_stats(&dir);
    let meta_raw = std::fs::read_to_string(dir.join("chat_backup_meta.json")).unwrap_or_default();
    let meta: serde_json::Value = serde_json::from_str(&meta_raw).unwrap_or(serde_json::Value::Null);
    Ok(serde_json::json!({
        "backed": true, "size_bytes": size, "files": files,
        "backed_at": meta.get("backedAt").and_then(|s| s.as_str()),
        "has_edge_mapping": meta.get("has_edge_mapping").and_then(|b| b.as_bool()).unwrap_or(false),
    }))
}

/// SQL 标识符引号包裹（审查 P2-4）：内嵌双引号转义为两个双引号，防列名/表名
/// 含 `"` 时拼接畸形 SQL（列名来自 PRAGMA table_info，非完全可控）
fn sql_quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ── M8 会话复制/迁移·新 id 算法（F-45，批次3 T3.2）─────────────────────────
// 流程（§3.11）：读源账号 jsonl → 替换 sessionId 为新 UUID → 写目标 projects
//   → sessions 表整行克隆插行（id=新 UUID）→ edge_sync_mapping 注册 convmsg:{目标 uid}。
// 复制前对双 db 做 .pre-copy.bak 快照；执行前先优雅关闭客户端。
//
// 零新增依赖：UUID v4 由 sha256（时间纳秒+pid+计数器+路径熵）截 16 字节构造
// （version=4 / variant=10），唯一性对本场景足够。

/// 由种子构造 v4 形态 UUID 字符串（sha256 截断，非密码学随机）。
fn pseudo_uuid_v4(seed: &str) -> String {
    use sha2::{Digest, Sha256};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(seed.as_bytes());
    h.update(nanos.to_le_bytes());
    h.update(n.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    let digest = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 会话复制结果明细（单会话）
#[derive(serde::Serialize)]
struct WbChatCopyItem {
    old_cid: String,
    new_cid: String,
    jsonl_lines: usize,
    sessions_row_cloned: bool,
}

/// 复制/迁移会话：source_user_id 的会话（备份优先，其次现有 projects）以新 id
/// 写入当前 ~/.workbuddy，并注册到目标账号的云端映射（convmsg:{target}）。
#[tauri::command(async)]
pub fn workbuddy_chatdata_copy(
    state: State<AppState>,
    source_user_id: String,
    target_user_id: String,
) -> Result<serde_json::Value, String> {
    if source_user_id == target_user_id {
        return Err("源与目标账号相同，无需复制".into());
    }
    let pool = load_pool(&state);
    if !pool.accounts.iter().any(|a| a.id == source_user_id) {
        return Err(format!("源账号不在池中: {source_user_id}"));
    }
    if !pool.accounts.iter().any(|a| a.id == target_user_id) {
        return Err(format!("目标账号不在池中: {target_user_id}"));
    }
    // 会话正文来源：源账号备份优先（只读安全），否则现有 projects
    let backup_projects = wb_chat_backup_root(&state).join(&source_user_id).join("projects");
    let live_projects = wb_chats_dir();
    let (src_projects, src_label) = if backup_projects.is_dir() {
        (backup_projects, format!("备份({source_user_id})"))
    } else if live_projects.is_dir() {
        (live_projects.clone(), "现有 projects".to_string())
    } else {
        return Err("未找到可复制的会话正文（该账号无备份且 ~/.workbuddy/projects 为空）".into());
    };

    crate::commands::process::graceful_kill_app("WorkBuddy")?;

    // 复制前快照双 db（design: backup_workbuddy_db；.pre-copy.bak 单代覆盖）
    let mut db_pre = 0usize;
    for db in ["workbuddy.db", "edge-sync-mapping-v2.db"] {
        let p = wb_data_dir().join(db);
        if p.is_file() {
            std::fs::copy(&p, wb_data_dir().join(format!("{db}.pre-copy.bak")))
                .map_err(|e| format!("预备份 {db} 失败: {e}"))?;
            db_pre += 1;
        }
    }

    // ①② 遍历源 jsonl → 新 UUID → 写目标 projects
    let mut items: Vec<WbChatCopyItem> = Vec::new();
    let mut stack = vec![src_projects.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let old_cid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            if old_cid.is_empty() {
                continue;
            }
            let new_cid = pseudo_uuid_v4(&format!("{source_user_id}:{old_cid}"));
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut out_lines = String::with_capacity(content.len());
            let mut n_lines = 0usize;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(mut v) => {
                        // 每行顶层 sessionId 统一替换为本会话新 id
                        if let Some(obj) = v.as_object_mut() {
                            if obj.contains_key("sessionId") {
                                obj.insert("sessionId".into(), serde_json::json!(new_cid));
                            }
                        }
                        out_lines.push_str(&serde_json::to_string(&v).unwrap_or_default());
                    }
                    Err(_) => out_lines.push_str(trimmed), // 非事件行原样保留
                }
                out_lines.push('\n');
                n_lines += 1;
            }
            // 目标路径：保持相对 workspace 目录结构
            let rel = path
                .parent()
                .and_then(|p| p.strip_prefix(&src_projects).ok())
                .unwrap_or_else(|| std::path::Path::new(""));
            let dst_dir = live_projects.join(rel);
            std::fs::create_dir_all(&dst_dir).map_err(|e| format!("创建目标目录失败: {e}"))?;
            std::fs::write(dst_dir.join(format!("{new_cid}.jsonl")), &out_lines)
                .map_err(|e| format!("写入目标会话失败: {e}"))?;
            items.push(WbChatCopyItem {
                old_cid,
                new_cid,
                jsonl_lines: n_lines,
                sessions_row_cloned: false,
            });
        }
    }
    if items.is_empty() {
        return Err("源数据中未发现任何会话正文（0 个 .jsonl）".into());
    }

    // ③ sessions 表整行克隆（workbuddy.db；id = 会话 UUID）
    let main_db = wb_data_dir().join("workbuddy.db");
    let mut sessions_cloned = 0usize;
    if main_db.is_file() {
        if let Ok(conn) = rusqlite::Connection::open(&main_db) {
            let cols: Vec<String> = {
                let mut out = Vec::new();
                if let Ok(mut stmt) = conn.prepare("PRAGMA table_info(sessions)") {
                    if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) {
                        for name in rows.flatten() {
                            out.push(name);
                        }
                    }
                }
                out
            };
            if !cols.is_empty() {
                for it in &mut items {
                    let sel = format!(
                        "SELECT {} FROM sessions WHERE id = ?1",
                        cols.iter().map(|c| sql_quote_ident(c)).collect::<Vec<_>>().join(", ")
                    );
                    let Ok(mut stmt) = conn.prepare(&sel) else { continue };
                    let mut row_vals: Option<Vec<rusqlite::types::Value>> = None;
                    if let Ok(mut rows) = stmt.query(rusqlite::params![it.old_cid]) {
                        if let Ok(Some(row)) = rows.next() {
                            let mut vals = Vec::new();
                            for i in 0..cols.len() {
                                vals.push(row.get(i).unwrap_or(rusqlite::types::Value::Null));
                            }
                            row_vals = Some(vals);
                        }
                    }
                    let Some(vals) = row_vals else { continue };
                    // 组装 INSERT：id 列替换为新 UUID，其余整行复制
                    let id_idx = cols.iter().position(|c| c == "id").unwrap_or(0);
                    let col_list = cols.iter().map(|c| sql_quote_ident(c)).collect::<Vec<_>>().join(", ");
                    let ph = vec!["?"; cols.len()].join(", ");
                    let ins = format!("INSERT OR IGNORE INTO sessions ({col_list}) VALUES ({ph})");
                    if let Ok(mut ins_stmt) = conn.prepare(&ins) {
                        let params: Vec<rusqlite::types::Value> = vals
                            .into_iter()
                            .enumerate()
                            .map(|(i, v)| {
                                if i == id_idx {
                                    rusqlite::types::Value::Text(it.new_cid.clone())
                                } else {
                                    v
                                }
                            })
                            .collect();
                        if ins_stmt.execute(rusqlite::params_from_iter(params)).is_ok() {
                            sessions_cloned += 1;
                            it.sessions_row_cloned = true;
                        }
                    }
                }
            }
        }
    }

    // ④ edge_sync_mapping 云端映射：把含 convmsg:{source} 的行克隆并替换为 convmsg:{target}
    let edge_db = wb_data_dir().join("edge-sync-mapping-v2.db");
    let mut mappings = 0usize;
    if edge_db.is_file() {
        let old_channel = format!("convmsg:{source_user_id}");
        let new_channel = format!("convmsg:{target_user_id}");
        if let Ok(conn) = rusqlite::Connection::open(&edge_db) {
            // 宽容发现：遍历所有表，找含含旧 channel 文本的行（表名/结构随客户端版本浮动）
            let tables: Vec<String> = {
                let mut out = Vec::new();
                if let Ok(mut stmt) = conn.prepare("SELECT name FROM sqlite_master WHERE type='table'") {
                    if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                        for t in rows.flatten() {
                            out.push(t);
                        }
                    }
                }
                out
            };
            for table in tables {
                if table.starts_with("sqlite_") {
                    continue;
                }
                let cols: Vec<String> = {
                    let mut out = Vec::new();
                    if let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({})", sql_quote_ident(&table))) {
                        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) {
                            for name in rows.flatten() {
                                out.push(name);
                            }
                        }
                    }
                    out
                };
                if cols.is_empty() {
                    continue;
                }
                // 找出文本列中命中旧 channel 的行，整行克隆替换
                let col_list = cols.iter().map(|c| sql_quote_ident(c)).collect::<Vec<_>>().join(", ");
                let Ok(mut stmt) = conn.prepare(&format!("SELECT rowid, {col_list} FROM {}", sql_quote_ident(&table))) else { continue };
                let mut hits: Vec<(i64, Vec<rusqlite::types::Value>)> = Vec::new();
                if let Ok(mut rows) = stmt.query([]) {
                    while let Ok(Some(row)) = rows.next() {
                        let rowid: i64 = row.get(0).unwrap_or(0);
                        let mut vals = Vec::new();
                        let mut hit = false;
                        for i in 0..cols.len() {
                            let v = row.get::<_, rusqlite::types::Value>(i + 1).unwrap_or(rusqlite::types::Value::Null);
                            if let rusqlite::types::Value::Text(ref s) = v {
                                if s.contains(&old_channel) {
                                    hit = true;
                                }
                            }
                            vals.push(v);
                        }
                        if hit {
                            hits.push((rowid, vals));
                        }
                    }
                }
                for (_, vals) in hits {
                    let ph = vec!["?"; cols.len()].join(", ");
                    let ins = format!("INSERT OR IGNORE INTO {} ({col_list}) VALUES ({ph})", sql_quote_ident(&table));
                    if let Ok(mut ins_stmt) = conn.prepare(&ins) {
                        let params: Vec<rusqlite::types::Value> = vals
                            .into_iter()
                            .map(|v| match v {
                                rusqlite::types::Value::Text(s) => {
                                    rusqlite::types::Value::Text(s.replace(&old_channel, &new_channel))
                                }
                                other => other,
                            })
                            .collect();
                        if ins_stmt.execute(rusqlite::params_from_iter(params)).is_ok() {
                            mappings += 1;
                        }
                    }
                }
            }
        }
    }

    let copied = items.len();
    let total_lines: usize = items.iter().map(|i| i.jsonl_lines).sum();
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "workbuddy: 会话复制 {source_user_id} → {target_user_id}（{copied} 会话 / {total_lines} 行，sessions 克隆 {sessions_cloned}，映射注册 {mappings}，预备份 {db_pre} db）"
        ),
    );
    Ok(serde_json::json!({
        "ok": true,
        "copied": copied,
        "total_lines": total_lines,
        "sessions_cloned": sessions_cloned,
        "mappings_registered": mappings,
        "db_pre_backup": db_pre,
        "source": src_label,
        "items": items,
    }))
}


// ── M7 生态接入 · OAuth 扫码登录（F-50，§3.10）────────────────────────────
//
// 流程（无 PKCE，state 服务端签发）：
//   ① POST /v2/plugin/auth/state?platform=CLI → state + authUrl
//   ② 系统浏览器打开 authUrl（用户扫码/登录，与工具侧 cookie 天然隔离）
//   ③ GET /v2/plugin/auth/token?state= 轮询（≤300s，间隔 3s）
//   ④ GET /v2/plugin/login/account?state= 取 uid/nickname → 自动入池 + 凭证回写 token store
// 每流程独立 cookie jar（手工捕获 Set-Cookie 回传，不引新依赖）；凭证零明文输出（不进日志/事件/UI）。

static OAUTH_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 捕获响应 Set-Cookie 到独立 jar（简化：保留 name=value，忽略 Path/Expires 等属性）
fn oauth_capture_cookies(jar: &mut std::collections::HashMap<String, String>, resp: &ureq::Response) {
    for hv in resp.all("Set-Cookie") {
        if let Some(kv) = hv.split(';').next() {
            if let Some((k, v)) = kv.split_once('=') {
                jar.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
}

fn oauth_cookie_header(jar: &std::collections::HashMap<String, String>) -> Option<String> {
    if jar.is_empty() {
        None
    } else {
        Some(jar.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "))
    }
}

/// JWT payload 解码（不验证签名）：取 iss / sub / exp
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let seg = parts[1].trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(seg).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 系统浏览器打开 URL（Windows：cmd /c start，隐藏控制台；仅放行 http/https 且无引号空格）
fn open_in_browser(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.contains(['"', '\'', ' ']) {
        return Err(format!("拒绝打开非法 URL：{url}"));
    }
    Command::new("cmd")
        .args(["/c", "start", "", url])
        .creation_flags(0x08000000)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开浏览器失败: {e}"))
}

fn mask_phone(p: &str) -> String {
    let c: Vec<char> = p.chars().collect();
    if c.len() >= 7 {
        format!(
            "{}****{}",
            c[..3].iter().collect::<String>(),
            c[c.len() - 4..].iter().collect::<String>()
        )
    } else {
        "****".into()
    }
}

/// OAuth 主流程（后台线程执行）：进度经 wb-oauth-progress 事件推送，结果经 wb-oauth-done。
fn oauth_flow(app: &AppHandle, state: &AppState) -> Result<(String, String), String> {
    const BASE: &str = "https://www.codebuddy.cn";
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15)).build();
    let mut jar = std::collections::HashMap::new();
    let emit = |stage: &str, message: &str, auth_url: Option<&str>| {
        let _ = app.emit(
            "wb-oauth-progress",
            serde_json::json!({ "stage": stage, "message": message, "auth_url": auth_url }),
        );
    };

    // ① 发起：auth/state?platform=CLI
    emit("init", "正在请求登录 state…", None);
    let mut req = agent
        .post(&format!("{BASE}/v2/plugin/auth/state?platform=CLI"))
        .set("User-Agent", "WorkBuddy")
        .set("Origin", BASE)
        .set("Referer", &format!("{BASE}/"))
        .set("Content-Type", "application/json");
    if let Some(c) = oauth_cookie_header(&jar) {
        req = req.set("Cookie", &c);
    }
    let resp = req.send_string("{}").map_err(|e| format!("请求 auth/state 失败: {e}"))?;
    oauth_capture_cookies(&mut jar, &resp);
    let body: serde_json::Value = resp.into_json().unwrap_or_default();
    let state_id = as_str(&dig(&body, &["state"]))
        .or_else(|| as_str(&dig(&body, &["authState"])))
        .ok_or("auth/state 响应中未找到 state")?;
    let auth_url = as_str(&dig(&body, &["authUrl"]))
        .or_else(|| as_str(&dig(&body, &["auth_url"])))
        .or_else(|| as_str(&dig(&body, &["url"])))
        .ok_or("auth/state 响应中未找到 authUrl")?;

    // ② 浏览器打开登录页
    open_in_browser(&auth_url)?;
    emit(
        "browser",
        "已在系统浏览器打开登录页：请完成扫码/登录（完成后停留在结果页即可，无需复制内容）",
        Some(&auth_url),
    );
    fs_utils::app_log(&state.data_dir, "workbuddy: OAuth 扫码流程开始（auth/state 成功）");

    // ③ 轮询 auth/token（≤300s，间隔 3s；pending/非 2xx 一律继续等待）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut token = String::new();
    let mut refresh_token = String::new();
    let mut expires_in_s: Option<i64> = None;
    let mut polls: u32 = 0;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_secs(3));
        polls += 1;
        if polls % 10 == 1 {
            emit("polling", "等待登录完成…（最长 300 秒）", None);
        }
        let mut req = agent
            .get(&format!("{BASE}/v2/plugin/auth/token?state={state_id}"))
            .set("User-Agent", "WorkBuddy")
            .set("Origin", BASE)
            .set("Referer", &format!("{BASE}/"));
        if let Some(c) = oauth_cookie_header(&jar) {
            req = req.set("Cookie", &c);
        }
        if let Ok(resp) = req.call() {
            oauth_capture_cookies(&mut jar, &resp);
            let body: serde_json::Value = resp.into_json().unwrap_or_default();
            if let Some(t) = as_str(&dig(&body, &["accessToken"])) {
                if !t.is_empty() {
                    token = t;
                    refresh_token = as_str(&dig(&body, &["refreshToken"])).unwrap_or_default();
                    expires_in_s = dig(&body, &["expiresIn"]).and_then(|v| v.as_i64());
                    break;
                }
            }
        }
    }
    if token.is_empty() {
        return Err("登录超时（300 秒）：请重试并确保在浏览器中完成登录".into());
    }

    // ④ 取账号资料（uid/nickname；uid 兜底取 JWT sub，过期时间兜底取 JWT exp）
    let mut uid = String::new();
    let mut nickname = String::new();
    let mut phone_masked = String::new();
    let mut edition = String::new();
    let mut req = agent
        .get(&format!("{BASE}/v2/plugin/login/account?state={state_id}"))
        .set("User-Agent", "WorkBuddy")
        .set("Origin", BASE)
        .set("Referer", &format!("{BASE}/"));
    if let Some(c) = oauth_cookie_header(&jar) {
        req = req.set("Cookie", &c);
    }
    if let Ok(resp) = req.call() {
        let body: serde_json::Value = resp.into_json().unwrap_or_default();
        uid = as_str(&dig(&body, &["uid", "userId", "user_id"])).unwrap_or_default();
        nickname = as_str(&dig(&body, &["nickname", "nickName", "name"])).unwrap_or_default();
        phone_masked = as_str(&dig(&body, &["phone", "mobile", "phoneNumber"]))
            .map(|p| mask_phone(&p))
            .unwrap_or_default();
        edition = as_str(&dig(&body, &["editionType", "edition_type"])).unwrap_or_default();
    }
    if uid.is_empty() {
        uid = jwt_claims(&token)
            .and_then(|c| c.get("sub").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
    }
    let exp_s: Option<i64> = expires_in_s
        .map(|s| chrono::Utc::now().timestamp() + s)
        .or_else(|| jwt_claims(&token).and_then(|c| c.get("exp").and_then(|v| v.as_i64())));

    // 入池（同 token 稳定同 id，F-04）+ 凭证回写 token store（不外泄）
    let id = account_id_of(&token);
    let now_s = chrono::Utc::now().timestamp();
    let mut pool = load_pool(state);
    if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == id) {
        if !uid.is_empty() {
            a.uid = uid.clone();
        }
        if !nickname.is_empty() {
            a.nickname = nickname.clone();
        }
        if !phone_masked.is_empty() {
            a.phone_masked = phone_masked.clone();
        }
        if !edition.is_empty() {
            a.edition_type = edition.clone();
        }
        a.access_token_expires_at = exp_s;
        a.auth_saved_at = Some(now_s);
        a.needs_relogin = false;
        a.relogin_reason.clear();
    } else {
        pool.accounts.push(WorkBuddyAccount {
            id: id.clone(),
            uid: uid.clone(),
            nickname: nickname.clone(),
            phone_masked,
            edition_type: edition,
            access_token_expires_at: exp_s,
            auth_saved_at: Some(now_s),
            ..Default::default()
        });
    }
    save_pool(state, &pool)?;
    let creds = serde_json::json!({
        "access_token": token,
        "refresh_token": if refresh_token.is_empty() { None } else { Some(refresh_token) },
        "expires_at_ms": exp_s.map(|s| s * 1000),
    });
    upsert_token_store(state, &id, &creds)?;
    fs_utils::app_log(&state.data_dir, &format!("workbuddy: OAuth 扫码入池 {id}（{nickname}）"));

    let label = if nickname.is_empty() { id.clone() } else { nickname.clone() };
    emit("success", &format!("账号「{label}」已扫码登录并自动入池"), None);
    Ok((id, label))
}

/// OAuth 扫码登录（F-50）：后台线程执行全流程，事件驱动 UI；同时仅允许一个流程。
#[tauri::command(async)]
pub fn workbuddy_oauth_login(app: AppHandle) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    if OAUTH_RUNNING.swap(true, Ordering::SeqCst) {
        return Err("已有 OAuth 扫码流程进行中".into());
    }
    std::thread::spawn(move || {
        let payload = match AppState::new().and_then(|st| oauth_flow(&app, &st)) {
            Ok((id, nickname)) => serde_json::json!({
                "ok": true,
                "id": id,
                "nickname": nickname,
                "message": format!("账号「{nickname}」已扫码登录并自动入池"),
            }),
            Err(e) => serde_json::json!({ "ok": false, "message": e }),
        };
        let _ = app.emit("wb-oauth-done", payload);
        OAUTH_RUNNING.store(false, Ordering::SeqCst);
    });
    Ok(())
}

// ── M7 生态接入 · 环境重置 / 彻底登出（F-14，§3.10）────────────────────────
//
// 16 项认证残留清理清单（对齐 oss-research/antigravity-tools oauth.py _clear_all_auth 的
// 17 个物理位置：「认证文件」一项合并 workbuddy-desktop.info 与 .neodata_token 两个文件）。
// 执行顺序：Keycloak SSO 注销（需当前 token）→ 关闭 WorkBuddy → 按勾选项逐项清理。

const WB_ACCESS_TOKEN_SECRET_KEY: &str =
    r#"secret://{"extensionId":"tencent-cloud.coding-copilot","key":"planning-genie.new.accessTokencn"}"#;

fn wb_roaming_dir() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("WorkBuddy")
}

fn wb_state_vscdb() -> PathBuf {
    wb_roaming_dir().join("User").join("globalStorage").join("state.vscdb")
}

/// 16 项清单（id, label, detail）——存在性检查在命令层动态计算
fn wb_reset_catalog() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        ("auth_files", "认证文件", "删除 workbuddy-desktop.info（新版登录文件）与 .neodata_token（旧版 JWT）"),
        ("vscdb_access_token", "vscdb AccessToken", "删除 state.vscdb 中加密账号凭证 secret://…accessTokencn"),
        ("storage_json_uid", "storage.json 用户标识", "清除 globalStorage/storage.json 的 genie.userId"),
        ("local_storage", "local_storage 目录", "删除 ~/.workbuddy/local_storage/（userId 与 agent 配置）"),
        ("app_session", "内嵌浏览器 session", "删除 ~/.workbuddy/app/session/ 整个会话目录"),
        ("roaming_sessions", "主进程浏览器会话", "清空 %APPDATA%/WorkBuddy 下 Network/Session Storage/Local Storage 等 9 个会话目录"),
        ("vscdb_copilot", "copilot 产品缓存", "删除 state.vscdb 的 Tencent-Cloud.coding-copilot 配置缓存"),
        ("vscdb_secrets", "secret:// 条目", "删除 state.vscdb 中所有 secret:// 加密条目"),
        ("claw_channels", "claw.channels", "清除两处 settings.json 中的 claw.channels 通道配置"),
        ("memory_uid_files", "memory 用户记忆", "删除 ~/.workbuddy/memory/ 中以 userId(UUID) 命名的记忆文件"),
        ("memery_uid_files", "memery 用户文件", "删除 ~/.workbuddy/memery/ 中以 userId(UUID) 命名的文件"),
        ("sessions_dir", "sessions 目录", "删除 ~/.workbuddy/sessions/ 整个目录"),
        ("vscdb_marker", "存储标记", "删除 state.vscdb 的 __$__targetStorageMarker（防启动回写恢复）"),
        ("wb_db_sessions", "workbuddy.db 会话", "清空 workbuddy.db 的 sessions / workspaces 表"),
        ("codebuddy_sessions_vscdb", "codebuddy 会话库", "清空 codebuddy-sessions.vscdb 中 session:% 记录"),
        ("vscdb_backup", "state.vscdb.backup", "清理 backup 中认证数据（防 VS Code 启动时从 backup 恢复已删条目）"),
    ]
}

/// 带重试删除目录（Windows 文件占用场景：最多 3 次，间隔 400ms）；不存在返回 Ok(false)
fn force_rmtree(p: &std::path::Path) -> Result<bool, String> {
    if !p.exists() {
        return Ok(false);
    }
    let mut last = String::new();
    for _ in 0..3 {
        match std::fs::remove_dir_all(p) {
            Ok(()) => return Ok(true),
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
    }
    Err(format!("删除 {} 失败: {last}", p.display()))
}

/// vscdb 执行 DELETE（db 不存在时返回 Ok(0)；key=None 表示 SQL 内联条件）
fn vscdb_execute(db: &std::path::Path, sql: &str, key: Option<&str>) -> Result<usize, String> {
    if !db.exists() {
        return Ok(0);
    }
    let conn = rusqlite::Connection::open(db).map_err(|e| format!("打开 {} 失败: {e}", db.display()))?;
    let n = match key {
        Some(k) => conn.execute(sql, [k]).map_err(|e| e.to_string())?,
        None => conn.execute(sql, []).map_err(|e| e.to_string())?,
    };
    Ok(n)
}

/// 删除目录中以 userId(UUID) 命名的文件 + user-memery-state.json
fn clear_user_id_files(dir: &std::path::Path) -> Result<usize, String> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut n = 0;
    let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let stem = name.split('_').next().unwrap_or("");
        let is_uid_file = stem.len() == 36 && stem.contains('-');
        if is_uid_file || name == "user-memery-state.json" {
            if std::fs::remove_file(e.path()).is_ok() {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// 清除 JSON 顶层的指定键（含嵌套 claw.channels 特例），返回是否有变更
fn clear_claw_channels(path: &std::path::Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut data: serde_json::Value = fs_utils::read_json(path);
    if !data.is_object() {
        return Ok(false);
    }
    let obj = data.as_object_mut().unwrap();
    let mut changed = false;
    if let Some(claw) = obj.get_mut("claw").and_then(|c| c.as_object_mut()) {
        if claw.remove("channels").is_some() {
            changed = true;
        }
        if claw.is_empty() {
            obj.remove("claw");
        }
    }
    let keys: Vec<String> = obj.keys().filter(|k| k.contains("claw.channels")).cloned().collect();
    for k in keys {
        obj.remove(&k);
        changed = true;
    }
    if changed {
        fs_utils::write_json(path, &data)?;
    }
    Ok(changed)
}

fn clear_storage_uid(path: &std::path::Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut data: serde_json::Value = fs_utils::read_json(path);
    let changed = data
        .as_object_mut()
        .map(|o| o.remove("genie.userId").is_some())
        .unwrap_or(false);
    if changed {
        fs_utils::write_json(path, &data)?;
    }
    Ok(changed)
}

/// 执行单个清理项，返回人类可读结果描述
fn run_reset_item(id: &str) -> Result<String, String> {
    let home = wb_data_dir();
    let roaming = wb_roaming_dir();
    let state_vscdb = wb_state_vscdb();
    let state_vscdb_backup = roaming.join("User").join("globalStorage").join("state.vscdb.backup");
    match id {
        "auth_files" => {
            let mut removed = 0;
            let f1 = auth_file_path();
            if f1.exists() {
                std::fs::remove_file(&f1).map_err(|e| e.to_string())?;
                removed += 1;
            }
            let f2 = home.join(".neodata_token");
            if f2.exists() {
                std::fs::remove_file(&f2).map_err(|e| e.to_string())?;
                removed += 1;
            }
            Ok(format!("已删除 {removed} 个认证文件"))
        }
        "vscdb_access_token" => {
            let n = vscdb_execute(&state_vscdb, "DELETE FROM ItemTable WHERE key = ?", Some(WB_ACCESS_TOKEN_SECRET_KEY))?;
            Ok(format!("已删除 AccessToken 条目（{n} 行）"))
        }
        "storage_json_uid" => {
            let changed = clear_storage_uid(&roaming.join("User").join("globalStorage").join("storage.json"))?;
            Ok(if changed { "已清除 genie.userId".into() } else { "未发现 genie.userId（跳过）".into() })
        }
        "local_storage" => match force_rmtree(&home.join("local_storage"))? {
            true => Ok("已删除 local_storage 目录".into()),
            false => Ok("目录不存在（跳过）".into()),
        },
        "app_session" => match force_rmtree(&home.join("app").join("session"))? {
            true => Ok("已删除内嵌浏览器 session 目录".into()),
            false => Ok("目录不存在（跳过）".into()),
        },
        "roaming_sessions" => {
            let mut done = 0;
            for d in [
                "Network",
                "Session Storage",
                "Local Storage",
                "Partitions",
                "Service Worker",
                "Cache",
                "WebStorage",
                "blob_storage",
                "IndexedDB",
            ] {
                if force_rmtree(&roaming.join(d)).is_ok() {
                    done += 1;
                }
            }
            Ok(format!("已清理 {done}/9 个主进程会话目录"))
        }
        "vscdb_copilot" => {
            let n = vscdb_execute(&state_vscdb, "DELETE FROM ItemTable WHERE key = ?", Some("Tencent-Cloud.coding-copilot"))?;
            Ok(format!("已删除 copilot 产品缓存（{n} 行）"))
        }
        "vscdb_secrets" => {
            let n = vscdb_execute(&state_vscdb, "DELETE FROM ItemTable WHERE key LIKE 'secret://%'", None)?;
            Ok(format!("已删除 secret:// 条目（{n} 行）"))
        }
        "claw_channels" => {
            let a = clear_claw_channels(&home.join("settings.json"))?;
            let b = clear_claw_channels(&roaming.join("User").join("settings.json"))?;
            Ok(format!("已清除 claw.channels（.workbuddy: {a}，AppData: {b}）"))
        }
        "memory_uid_files" => {
            let n = clear_user_id_files(&home.join("memory"))?;
            Ok(format!("已删除 {n} 个记忆文件"))
        }
        "memery_uid_files" => {
            let n = clear_user_id_files(&home.join("memery"))?;
            Ok(format!("已删除 {n} 个 memery 文件"))
        }
        "sessions_dir" => match force_rmtree(&home.join("sessions"))? {
            true => Ok("已删除 sessions 目录".into()),
            false => Ok("目录不存在（跳过）".into()),
        },
        "vscdb_marker" => {
            let n = vscdb_execute(&state_vscdb, "DELETE FROM ItemTable WHERE key = '__$__targetStorageMarker'", None)?;
            Ok(format!("已删除存储标记（{n} 行）"))
        }
        "wb_db_sessions" => {
            let db = home.join("workbuddy.db");
            if !db.exists() {
                return Ok("workbuddy.db 不存在（跳过）".into());
            }
            let conn = rusqlite::Connection::open(&db).map_err(|e| e.to_string())?;
            let mut parts: Vec<String> = vec![];
            for t in ["sessions", "workspaces"] {
                match conn.execute(&format!("DELETE FROM {t}"), []) {
                    Ok(n) => parts.push(format!("{t} {n} 行")),
                    Err(_) => parts.push(format!("{t} 跳过")),
                }
            }
            Ok(format!("已清空 {}", parts.join("，")))
        }
        "codebuddy_sessions_vscdb" => {
            let n = vscdb_execute(
                &roaming.join("codebuddy-sessions.vscdb"),
                "DELETE FROM ItemTable WHERE key LIKE 'session:%'",
                None,
            )?;
            Ok(format!("已删除 session 记录（{n} 行）"))
        }
        "vscdb_backup" => {
            if !state_vscdb_backup.exists() {
                return Ok("backup 不存在（跳过）".into());
            }
            let conn = rusqlite::Connection::open(&state_vscdb_backup).map_err(|e| e.to_string())?;
            let steps: [(&str, Option<&str>); 4] = [
                ("DELETE FROM ItemTable WHERE key = ?", Some(WB_ACCESS_TOKEN_SECRET_KEY)),
                ("DELETE FROM ItemTable WHERE key LIKE 'secret://%'", None),
                ("DELETE FROM ItemTable WHERE key = 'Tencent-Cloud.coding-copilot'", None),
                ("DELETE FROM ItemTable WHERE key = '__$__targetStorageMarker'", None),
            ];
            let mut done = 0;
            for (sql, k) in steps {
                let r = match k {
                    Some(k) => conn.execute(sql, [k]),
                    None => conn.execute(sql, []),
                };
                if r.is_ok() {
                    done += 1;
                }
            }
            Ok(format!("已清理 backup 认证数据（{done}/4 项）"))
        }
        _ => Err(format!("未知清理项: {id}")),
    }
}

#[derive(Serialize, Clone)]
pub struct WbResetItem {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub exists: bool,
}

/// 环境重置清单（F-14）：16 项 + 动态存在性标注（供 UI 勾选预览）
#[tauri::command]
pub fn workbuddy_env_reset_items() -> Vec<WbResetItem> {
    let home = wb_data_dir();
    let roaming = wb_roaming_dir();
    let state_vscdb = wb_state_vscdb();
    let settings2 = roaming.join("User").join("settings.json");
    wb_reset_catalog()
        .iter()
        .map(|(id, label, detail)| {
            let exists = match *id {
                "auth_files" => auth_file_path().exists() || home.join(".neodata_token").exists(),
                "vscdb_access_token" | "vscdb_copilot" | "vscdb_secrets" | "vscdb_marker" => state_vscdb.exists(),
                "storage_json_uid" => roaming.join("User").join("globalStorage").join("storage.json").exists(),
                "local_storage" => home.join("local_storage").exists(),
                "app_session" => home.join("app").join("session").exists(),
                "roaming_sessions" => [
                    "Network",
                    "Session Storage",
                    "Local Storage",
                    "Partitions",
                    "Service Worker",
                    "Cache",
                    "WebStorage",
                    "blob_storage",
                    "IndexedDB",
                ]
                .iter()
                .any(|d| roaming.join(d).exists()),
                "claw_channels" => home.join("settings.json").exists() || settings2.exists(),
                "memory_uid_files" => home.join("memory").is_dir(),
                "memery_uid_files" => home.join("memery").is_dir(),
                "sessions_dir" => home.join("sessions").is_dir(),
                "wb_db_sessions" => home.join("workbuddy.db").exists(),
                "codebuddy_sessions_vscdb" => roaming.join("codebuddy-sessions.vscdb").exists(),
                "vscdb_backup" => state_vscdb_backup_exists(),
                _ => false,
            };
            WbResetItem {
                id: id.to_string(),
                label: label.to_string(),
                detail: detail.to_string(),
                exists,
            }
        })
        .collect()
}

fn state_vscdb_backup_exists() -> bool {
    wb_roaming_dir()
        .join("User")
        .join("globalStorage")
        .join("state.vscdb.backup")
        .exists()
}

/// 当前生效 accessToken：auth 文件优先，回退 token store 中有效期最新的账号凭证（仅用于解析 Keycloak iss）
fn current_access_token(state: &AppState) -> Option<String> {
    let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path());
    if let Some(t) = as_str(&dig(&raw, &["accessToken"])) {
        if !t.is_empty() {
            return Some(t);
        }
    }
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(state));
    let mut best: Option<(i64, String)> = None;
    if let Some(tokens) = store.get("tokens").and_then(|t| t.as_object()) {
        for rec in tokens.values() {
            let t = as_str(&dig(&rec, &["access_token"])).unwrap_or_default();
            if t.is_empty() {
                continue;
            }
            let exp = dig(&rec, &["expires_at_ms"]).and_then(|v| v.as_i64()).unwrap_or(0);
            if best.as_ref().map(|(e, _)| exp > *e).unwrap_or(true) {
                best = Some((exp, t));
            }
        }
    }
    best.map(|(_, t)| t)
}

/// 环境重置执行（F-14）：Keycloak SSO 注销 → 关闭 WorkBuddy → 按勾选项逐项清理（单项失败不中断）。
#[tauri::command(async)]
pub fn workbuddy_env_reset(
    app: AppHandle,
    state: State<AppState>,
    items: Vec<String>,
    keycloak_logout: bool,
) -> Result<Vec<serde_json::Value>, String> {
    if items.is_empty() && !keycloak_logout {
        return Err("未选择任何清理项".into());
    }
    let mut results: Vec<serde_json::Value> = vec![];

    // 0) Keycloak SSO 注销必须先于清理（需当前 accessToken 解析 iss）
    if keycloak_logout {
        let iss = current_access_token(&state).and_then(|t| {
            jwt_claims(&t)
                .and_then(|c| c.get("iss").and_then(|v| v.as_str()).map(|s| s.to_string()))
        });
        match iss {
            Some(iss) => {
                let url = format!("{}/protocol/openid-connect/logout", iss.trim_end_matches('/'));
                match open_in_browser(&url) {
                    Ok(()) => results.push(serde_json::json!({
                        "id": "keycloak_logout", "ok": true, "detail": "已打开 Keycloak 注销页（请在浏览器确认 SSO 退出）",
                    })),
                    Err(e) => results.push(serde_json::json!({ "id": "keycloak_logout", "ok": false, "detail": e })),
                }
            }
            None => results.push(serde_json::json!({
                "id": "keycloak_logout", "ok": false, "detail": "未找到可用 accessToken，无法解析 Keycloak iss（跳过 SSO 注销）",
            })),
        }
    }

    // 1) 关闭 WorkBuddy（防 db/会话目录占用与登出后回写）
    if !items.is_empty() {
        let _ = crate::commands::process::graceful_kill_app("WorkBuddy");
    }

    // 2) 按勾选项执行（单项失败不中断其余项）
    for id in &items {
        match run_reset_item(id) {
            Ok(detail) => results.push(serde_json::json!({ "id": id, "ok": true, "detail": detail })),
            Err(e) => results.push(serde_json::json!({ "id": id, "ok": false, "detail": e })),
        }
    }
    let ok_n = results
        .iter()
        .filter(|r| r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    let fail_n = results.len() - ok_n;
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "workbuddy: 环境重置完成（{ok_n}/{} 项成功，Keycloak 注销 {keycloak_logout}）",
            items.len()
        ),
    );
    if fail_n > 0 {
        push_notify(
            Some(&app),
            &state.data_dir,
            "WorkBuddy 环境重置",
            &format!("清理完成，{fail_n} 项失败，请查看详情"),
        );
    }
    Ok(results)
}

// ── M5 官方请求用量（F-25/F-58，批次3）────────────────────────────────────
//
// POST <domain>/billing/meter/get-user-request-usage（设计 §7.1，语义对齐
// oss-research/workbuddy-switch official_usage.rs）：近 31 天窗口分页拉取请求明细，
// 聚合 今日/近7天/本月 消耗积分 + 逐日/按模型排行；本地缓存 10 分钟（refresh 强制刷新）。
// 脱敏红线：上游可能携带的 prompt/input 等字段一律不复制，不入缓存不返回。

use serde_json::Value;

/// requestTime → 本地日期 "YYYY-MM-DD"（兼容 epoch 秒/毫秒、RFC3339、本地时间串、纯日期）
fn usage_row_date(item: &Value) -> Option<String> {
    use chrono::TimeZone;
    let raw = item.get("requestTime").or_else(|| item.get("request_time"))?;
    if let Some(n) = raw.as_f64() {
        if !n.is_finite() {
            return None;
        }
        let ms = if n.abs() < 10_000_000_000.0 { (n * 1000.0).round() as i64 } else { n.round() as i64 };
        return chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string());
    }
    let text = raw.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(text, fmt) {
            return chrono::Local
                .from_local_datetime(&parsed)
                .single()
                .map(|d| d.date_naive().format("%Y-%m-%d").to_string());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Some(d.format("%Y-%m-%d").to_string());
    }
    None
}

fn usage_credit(item: &Value) -> Option<f64> {
    let v = item.get("credit")?;
    let n = v.as_f64().or_else(|| v.as_str()?.trim().parse::<f64>().ok())?;
    (n.is_finite() && n >= 0.0).then_some(n)
}

/// 官方请求用量（F-25）：user_id 指定账号，缺省取 auth 文件当前账号或首个有凭证账号。
#[tauri::command(async)]
pub fn workbuddy_usage_official(
    state: State<AppState>,
    user_id: Option<String>,
    refresh: Option<bool>,
) -> Result<serde_json::Value, String> {
    let cache_path = state.data_dir.join("data").join("workbuddy_usage_official_cache.json");
    if !refresh.unwrap_or(false) {
        let cached: serde_json::Value = fs_utils::read_json(&cache_path);
        let fetched = cached.get("fetched_at_ms").and_then(Value::as_i64).unwrap_or(0);
        if cached.get("status").is_some()
            && chrono::Utc::now().timestamp_millis() - fetched < 10 * 60_000
        {
            return Ok(cached);
        }
    }

    // 选号：user_id → auth 文件当前账号 → 首个有 token store 凭证的账号
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(&state));
    let tokens = store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let pick = |id: &str| -> Option<(String, String, String)> {
        let rec = tokens.get(id)?;
        let token = as_str(&dig(&rec, &["access_token"]))?;
        if token.is_empty() {
            return None;
        }
        let domain = as_str(&dig(&rec, &["domain"])).unwrap_or_default();
        Some((id.to_string(), token, domain))
    };
    let pool = load_pool(&state);
    let chosen = user_id
        .as_deref()
        .and_then(|uid| pick(uid))
        .or_else(|| {
            let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path());
            let fuid = as_str(&dig(&raw, &["uid"]))?;
            pool.accounts.iter().find(|a| a.uid == fuid).map(|a| a.id.clone()).and_then(|id| pick(&id))
        })
        .or_else(|| pool.accounts.iter().find_map(|a| pick(&a.id)))
        .or_else(|| tokens.keys().find_map(|k| pick(k)))
        .ok_or("无可用账号凭证（请先在账号管理导入/扫码入池并续期）")?;
    let (acct_id, token, domain) = chosen;

    // 区域路由（T4.5/F-36，§5.2）：Global 账号（domain 含 workbuddy.ai）billing
    // 全走 www.workbuddy.ai；CN 账号维持既有 workbuddy.cn 网关。
    let base = if domain.contains("workbuddy.ai") {
        "https://www.workbuddy.ai".to_string()
    } else if domain.is_empty() {
        "https://www.workbuddy.cn".to_string()
    } else if domain.starts_with("http://") || domain.starts_with("https://") {
        domain.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", domain.trim_end_matches('/'))
    };
    let url = format!("{base}/billing/meter/get-user-request-usage");
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(30)).build();

    let today = chrono::Local::now().date_naive();
    let start = today - chrono::Duration::days(30);
    let start_text = format!("{start} 00:00:00");
    let end_text = format!("{today} 23:59:59");

    // 分页拉取（≤20 页 × 3000 行；requestId 去重；跳过 credit 缺失/负值/时间不可解析行）
    let mut rows: Vec<(String, f64, String)> = vec![]; // (date, credit, model)
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut reported_total: u64 = 0;
    for page in 1u64..=20 {
        let body = serde_json::json!({
            "startTime": start_text,
            "endTime": end_text,
            "pageNum": page,
            "pageSize": 3000,
        });
        let resp = agent
            .post(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("X-Client-Platform", "web")
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let v: serde_json::Value = match resp {
            Ok(r) => r.into_json().unwrap_or_default(),
            Err(ureq::Error::Status(code, _)) => {
                return Err(format!("官方用量请求失败（HTTP {code}）：请检查凭证有效期"));
            }
            Err(e) => return Err(format!("官方用量请求失败: {e}")),
        };
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 && code != 200 {
            return Err(format!("官方用量请求失败（code={code}）"));
        }
        let data = v.get("data").ok_or("官方响应格式无效")?;
        let items = data
            .get("data")
            .and_then(Value::as_array)
            .ok_or("官方响应格式无效")?;
        reported_total = reported_total.max(
            data.get("total")
                .and_then(Value::as_u64)
                .unwrap_or(items.len() as u64),
        );
        for item in items {
            let Some(credit) = usage_credit(item) else { continue };
            let Some(date) = usage_row_date(item) else { continue };
            let model = as_str(&dig(item, &["model"]))
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "未知模型".into());
            let rid = as_str(&dig(item, &["requestId", "request_id"]))
                .unwrap_or_else(|| uuidless_key(&date, &model, &credit));
            if seen.insert(rid) {
                rows.push((date, credit, model));
            }
        }
        if items.is_empty() || seen.len() as u64 >= reported_total {
            break;
        }
    }

    // 聚合：今日/近7天/本月 + 逐日（含按模型）+ 模型排行
    use chrono::Datelike;
    let mut usage_today = 0.0f64;
    let mut usage_week = 0.0f64;
    let mut usage_month = 0.0f64;
    let mut daily_credits: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut daily_models: std::collections::HashMap<String, std::collections::HashMap<String, (u64, f64)>> =
        std::collections::HashMap::new();
    let mut model_totals: std::collections::HashMap<String, (u64, f64)> = std::collections::HashMap::new();
    for (date, credit, model) in &rows {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            let dist = (today - d).num_days();
            if dist == 0 {
                usage_today += credit;
            }
            if (0..7).contains(&dist) {
                usage_week += credit;
            }
            if d.year() == today.year() && d.month() == today.month() {
                usage_month += credit;
            }
        }
        *daily_credits.entry(date.clone()).or_insert(0.0) += credit;
        let dm = daily_models.entry(date.clone()).or_default();
        let e = dm.entry(model.clone()).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += credit;
        let mt = model_totals.entry(model.clone()).or_insert((0, 0.0));
        mt.0 += 1;
        mt.1 += credit;
    }

    let mut models_out: Vec<serde_json::Value> = model_totals
        .into_iter()
        .map(|(model, (count, credit))| {
            serde_json::json!({ "model": model, "request_count": count, "credit": credit })
        })
        .collect();
    models_out.sort_by(|a, b| {
        let ca = a.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        let cb = b.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut daily_out: Vec<serde_json::Value> = vec![];
    for i in (0..=30).rev() {
        let d = today - chrono::Duration::days(i);
        let key = d.format("%Y-%m-%d").to_string();
        let mut day_models: Vec<serde_json::Value> = daily_models
            .get(&key)
            .map(|m| {
                m.iter()
                    .map(|(model, (count, credit))| {
                        serde_json::json!({ "model": model, "request_count": count, "credit": credit })
                    })
                    .collect()
            })
            .unwrap_or_default();
        day_models.sort_by(|a, b| {
            let ca = a.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
            let cb = b.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
            cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
        });
        daily_out.push(serde_json::json!({
            "date": key,
            "usage": daily_credits.get(&key).copied().unwrap_or(0.0),
            "models": day_models,
        }));
    }

    let payload = serde_json::json!({
        "status": "complete",
        "account_id": acct_id,
        "domain": base,
        "range_start": start.format("%Y-%m-%d").to_string(),
        "range_end": today.format("%Y-%m-%d").to_string(),
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        "request_count_total": seen.len(),
        "summary": {
            "usage_today": usage_today,
            "usage_7days": usage_week,
            "usage_this_month": usage_month,
        },
        "daily": daily_out,
        "models": models_out,
    });
    let _ = fs_utils::write_json(&cache_path, &payload);
    fs_utils::app_log(
        &state.data_dir,
        &format!("workbuddy: 官方用量刷新（{acct_id}，{}/{} 行）", seen.len(), reported_total),
    );
    Ok(payload)
}

/// requestId 缺失时的稳定兜底 key（不含任何敏感字段）
fn uuidless_key(date: &str, model: &str, credit: &f64) -> String {
    let mut h = Sha256::new();
    h.update(date.as_bytes());
    h.update(model.as_bytes());
    h.update(credit.to_le_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("auto-{}", &hex[..16])
}

// ==================== 活动信息展示（T4.4/F-51） ====================

/// 活动信息三端点聚合（F-51，§7.1）：活动 banner（公开 GET）+ 付费类型 +
/// 用量提醒（billing POST）。低频附加展示：10min 缓存；端点失败不致命，
/// 逐项容错并记入 errors（宽容解析，字段缺失返回 null）。
#[tauri::command]
pub fn workbuddy_activity_info(
    state: State<AppState>,
    user_id: Option<String>,
    refresh: Option<bool>,
) -> Result<serde_json::Value, String> {
    let cache_path = state.data_dir.join("data").join("workbuddy_activity_cache.json");
    if !refresh.unwrap_or(false) {
        let cached: serde_json::Value = fs_utils::read_json(&cache_path);
        let fetched = cached.get("fetched_at_ms").and_then(Value::as_i64).unwrap_or(0);
        if cached.get("account_id").is_some()
            && chrono::Utc::now().timestamp_millis() - fetched < 10 * 60_000
        {
            return Ok(cached);
        }
    }

    // 选号逻辑与 workbuddy_usage_official 一致（复用同一降级链）
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(&state));
    let tokens = store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let pick = |id: &str| -> Option<(String, String, String)> {
        let rec = tokens.get(id)?;
        let token = as_str(&dig(&rec, &["access_token"]))?;
        if token.is_empty() {
            return None;
        }
        let domain = as_str(&dig(&rec, &["domain"])).unwrap_or_default();
        Some((id.to_string(), token, domain))
    };
    let pool = load_pool(&state);
    let chosen = user_id
        .as_deref()
        .and_then(|uid| pick(uid))
        .or_else(|| {
            let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path());
            let fuid = as_str(&dig(&raw, &["uid"]))?;
            pool.accounts.iter().find(|a| a.uid == fuid).map(|a| a.id.clone()).and_then(|id| pick(&id))
        })
        .or_else(|| pool.accounts.iter().find_map(|a| pick(&a.id)))
        .or_else(|| tokens.keys().find_map(|k| pick(k)));
    let Some((acct_id, token, domain)) = chosen else {
        // 无凭证：banner 仍可拉（公开端点），付费类型/用量提醒跳过
        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
        let banners = fetch_activity_banners(&agent, "https://www.workbuddy.cn");
        return Ok(serde_json::json!({
            "account_id": null, "payment_type": null, "dosage_notify": null,
            "banners": banners, "errors": [], "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        }));
    };

    // 区域路由（F-36，§5.2）：billing/activity 随账号 domain
    let base = if domain.contains("workbuddy.ai") {
        "https://www.workbuddy.ai".to_string()
    } else {
        "https://www.workbuddy.cn".to_string()
    };
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    let mut errors: Vec<String> = vec![];

    let banners = fetch_activity_banners(&agent, &base);

    // 付费类型：POST /v2/billing/meter/get-payment-type → data.paymentType
    let payment_type = match billing_post_json(&agent, &format!("{base}/v2/billing/meter/get-payment-type"), &token) {
        Ok(v) => as_str(&dig(&v, &["data", "paymentType", "payment_type"]))
            .filter(|s| !s.is_empty() && *s != "unknown")
            .map(|s| s.to_string()),
        Err(e) => {
            errors.push(format!("payment-type: {e}"));
            None
        }
    };

    // 用量提醒：POST /v2/billing/meter/get-dosage-notify（宽容透传 data 内容）
    let dosage_notify = match billing_post_json(&agent, &format!("{base}/v2/billing/meter/get-dosage-notify"), &token) {
        Ok(v) => {
            let data = v.get("data").cloned().unwrap_or(Value::Null);
            if data.is_null() { None } else { Some(data) }
        }
        Err(e) => {
            errors.push(format!("dosage-notify: {e}"));
            None
        }
    };

    let payload = serde_json::json!({
        "account_id": acct_id,
        "payment_type": payment_type,
        "dosage_notify": dosage_notify,
        "banners": banners,
        "errors": errors,
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
    });
    let _ = fs_utils::write_json(&cache_path, &payload);
    Ok(payload)
}

/// 活动 banner：公开 GET /v2/activity/banner（宽容解析 banners/banner/list 数组）
fn fetch_activity_banners(agent: &ureq::Agent, base: &str) -> Vec<Value> {
    let resp = agent.get(&format!("{base}/v2/activity/banner")).call();
    let v: Value = match resp {
        Ok(r) => r.into_json().unwrap_or_default(),
        Err(_) => return vec![],
    };
    let arr = dig(&v, &["data", "banners"])
        .and_then(|x| x.as_array().cloned())
        .or_else(|| dig(&v, &["data", "banner"]).and_then(|x| x.as_array().cloned()))
        .or_else(|| dig(&v, &["data", "list"]).and_then(|x| x.as_array().cloned()))
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    // 只保留展示字段，剥离未知字段
    arr.iter()
        .filter(|b| b.is_object())
        .map(|b| {
            serde_json::json!({
                "title": as_str(&dig(b, &["title", "name"])).unwrap_or_default(),
                "content": as_str(&dig(b, &["content", "description", "desc"])).unwrap_or_default(),
                "url": as_str(&dig(b, &["url", "link", "jump_url", "jumpUrl"])).unwrap_or_default(),
                "start_time": as_str(&dig(b, &["start_time", "startTime"])).unwrap_or_default(),
                "end_time": as_str(&dig(b, &["end_time", "endTime"])).unwrap_or_default(),
            })
        })
        .collect()
}

/// billing POST（Bearer + web 平台头，脱敏红线：不携带 X-Refresh-Token）
fn billing_post_json(agent: &ureq::Agent, url: &str, token: &str) -> Result<Value, String> {
    let resp = agent
        .post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("X-Client-Platform", "web")
        .set("Content-Type", "application/json")
        .send_string("{}");
    match resp {
        Ok(r) => r.into_json().map_err(|e| format!("响应解析失败: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(test)]
mod oauth_reset_tests {
    use super::*;

    #[test]
    fn reset_catalog_has_16_unique_ids() {
        let cat = wb_reset_catalog();
        assert_eq!(cat.len(), 16);
        let mut ids: Vec<&str> = cat.iter().map(|(id, _, _)| *id).collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn jwt_claims_decodes_payload() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"u-123","iss":"https://kc.example/realms/demo"}"#);
        let token = format!("aaa.{payload}.bbb");
        let c = jwt_claims(&token).expect("claims should decode");
        assert_eq!(c.get("sub").and_then(|v| v.as_str()), Some("u-123"));
        assert_eq!(c.get("iss").and_then(|v| v.as_str()), Some("https://kc.example/realms/demo"));
    }

    #[test]
    fn parse_reward_plus_extracts_amounts() {
        assert_eq!(parse_reward_plus("签到成功 +10"), Some(10.0));
        assert_eq!(parse_reward_plus("签到成功 +2.5"), Some(2.5));
        assert_eq!(parse_reward_plus("签到成功"), None);
        assert_eq!(parse_reward_plus("已签到"), None);
        assert_eq!(parse_reward_plus("签到成功 +0"), None);
    }

    #[test]
    fn mask_phone_basic() {
        assert_eq!(mask_phone("13812345678"), "138****5678");
        assert_eq!(mask_phone("123"), "****");
    }
}

/// 启动自动补签核心（F-55，main.rs 启动线程调用）：
/// 复用签到脚本 --json-stream --skip-checked（未签自动补签），静默执行零打扰。
pub fn startup_auto_checkin(app: &AppHandle, state: &AppState) {
    let s = load_settings(state);
    if !s.auto_checkin {
        return;
    }
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    let python_dir = state.python_dir.clone();
    let python_exe = state.python_exe.clone();
    std::thread::spawn(move || {
        // 与 Trae 静默签到同款延迟 60s，避开启动高峰
        std::thread::sleep(std::time::Duration::from_secs(60));
        let script = python_dir.join("workbuddy_checkin.py");
        if !script.exists() {
            fs_utils::app_log(&data_dir, "WorkBuddy 启动补签：脚本不存在，跳过");
            return;
        }
        fs_utils::app_log(&data_dir, "WorkBuddy 启动补签：开始核验签到状态");
        match Command::new(&python_exe)
            .arg(&script)
            .args(["--json-stream", "--skip-checked"])
            .creation_flags(0x08000000)
            .env("AIWORKDATA_DIR", &data_dir)
            .env("PYTHONIOENCODING", "utf-8")
            .output()
        {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let done = stdout
                    .lines()
                    .rev()
                    .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                    .filter(|v| v.get("type") == Some(&serde_json::json!("done")));
                match done {
                    Some(d) => {
                        let msg = format!(
                            "WorkBuddy 启动补签完成: 成功 {}，已签 {}，失败 {}",
                            d.get("ok").and_then(|v| v.as_i64()).unwrap_or(0),
                            d.get("already").and_then(|v| v.as_i64()).unwrap_or(0),
                            d.get("failed").and_then(|v| v.as_i64()).unwrap_or(0),
                        );
                        fs_utils::app_log(&data_dir, &msg);
                        let failed = d.get("failed").and_then(|v| v.as_i64()).unwrap_or(0);
                        if failed > 0 {
                            push_notify(
                                Some(&app2),
                                &data_dir,
                                "WorkBuddy 签到提醒",
                                &format!("启动补签有 {failed} 个账号失败，请在签到与成长页查看"),
                            );
                        }
                    }
                    None => fs_utils::app_log(&data_dir, "WorkBuddy 启动补签：无有效结果输出"),
                }
            }
                    Err(e) => fs_utils::app_log(&data_dir, &format!("WorkBuddy 启动补签失败: {e}")),
        }
    });
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn pseudo_uuid_v4_format_and_uniqueness() {
        let a = pseudo_uuid_v4("seed-a");
        let b = pseudo_uuid_v4("seed-b");
        // v4 形态：8-4-4-4-12，第三段 4 开头，第四段 8/9/a/b 开头
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_eq!(&parts[2][..1], "4");
        assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"));
        // 不同种子（与同种子连续两次）均不重复
        assert_ne!(a, b);
        assert_ne!(a, pseudo_uuid_v4("seed-a"));
    }
}
