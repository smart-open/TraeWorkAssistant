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
}

fn default_lazy_hours() -> i64 {
    24
}
fn default_true() -> bool {
    true
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
                for wk in ["data", "result", "resp", "response", "info"] {
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
            refresh_token_expires_at: None,
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

fn upsert_token_store(state: &State<AppState>, id: &str, creds: &serde_json::Value) -> Result<(), String> {
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
    Ok(parsed)
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
                            crate::notify::notify(&app2, "WorkBuddy 签到提醒", &format!("启动补签有 {failed} 个账号失败，请在签到与成长页查看"));
                        }
                    }
                    None => fs_utils::app_log(&data_dir, "WorkBuddy 启动补签：无有效结果输出"),
                }
            }
            Err(e) => fs_utils::app_log(&data_dir, &format!("WorkBuddy 启动补签失败: {e}")),
        }
    });
}
