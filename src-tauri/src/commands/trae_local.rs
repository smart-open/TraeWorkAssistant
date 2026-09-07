//! 本机 Trae Work（TRAE SOLO）应用数据读取：当前登录账号扫描 + 本机套餐。
//!
//! 数据来源（本机实测，2026-09）：
//! - `%APPDATA%\TRAE SOLO CN\User\globalStorage\storage.json`（及 TRAE SOLO）
//!
//! uid 体系说明：storage.json 键 `iCubeAuthInfo://icube-dc:<uid>` 是账户中心 id 空间，
//! 与账号池 / JWT `data.id` 的 Cloud-IDE id 空间**不是同一体系**，不可直接入池。
//! 当前登录账号的 Cloud-IDE uid 取自 `icube_gtm.users` 键名（强证据，本机实测
//! 恒为当前登录账号）；存在多个候选时标记 uid_confident=false 并禁止入池，
//! 避免跨体系重复账号。
//!
//! 套餐信息：storage.json 键 `iCubeServerData://icube.cloudide`（明文 JSON），
//! `entitlementInfo.identityStr` 与 ide_user_pay_status 接口同源，零 API 读取。

use serde::Serialize;
use tauri::State;

use crate::fs_utils;
use crate::state::AppState;

const STORAGE_SUFFIX: &str = r"User\globalStorage\storage.json";

/// Trae Work 应用数据目录候选（按优先级）
fn app_data_dirs() -> Vec<std::path::PathBuf> {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    vec![
        std::path::PathBuf::from(&appdata).join("TRAE SOLO CN"),
        std::path::PathBuf::from(&appdata).join("TRAE SOLO"),
    ]
}

/// 读取 Trae Work 的 storage.json（多个候选目录取第一个存在的）
fn read_storage_json() -> Option<serde_json::Value> {
    for dir in app_data_dirs() {
        let p = dir.join(STORAGE_SUFFIX);
        if p.is_file() {
            if let Ok(s) = std::fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                    return Some(v);
                }
            }
        }
    }
    None
}

// ---------------- 本机套餐（零 API，storage.json 明文） ----------------

#[derive(Serialize, Clone, Default)]
pub struct AppEntitlement {
    /// 套餐名（Free / Lite / Pro ...）
    pub identity_str: Option<String>,
    /// 套餐数值（0=Free, 5=Lite ...）
    pub identity: Option<i64>,
    /// 服务端最后同步时间（毫秒）
    pub last_sync_time: Option<i64>,
}

/// 读取本机 Trae Work 当前登录账号的套餐信息（零 API，storage.json 明文缓存）。
#[tauri::command]
pub fn apps_entitlement_read() -> Option<AppEntitlement> {
    let storage = read_storage_json()?;
    let raw = storage
        .get("iCubeServerData://icube.cloudide")
        .and_then(|v| v.as_str())?;
    let data: serde_json::Value = serde_json::from_str(raw).ok()?;
    let ent = data.get("entitlementInfo")?;
    Some(AppEntitlement {
        identity_str: ent
            .get("identityStr")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        identity: ent.get("identity").and_then(|v| v.as_i64()),
        last_sync_time: data
            .get("serverTimeInfo")
            .and_then(|s| s.get("lastSyncTime"))
            .and_then(|v| v.as_i64()),
    })
}

// ---------------- 本机账号扫描 ----------------

/// 判断 token 是否为 15~16 位纯数字 id（Cloud-IDE uid 形态）
fn is_uid_token(t: &str) -> bool {
    (15..=16).contains(&t.chars().count()) && t.chars().all(|c| c.is_ascii_digit())
}

/// 从 storage.json 提取 Cloud-IDE uid 证据：
/// `icube_gtm.users` 键名仅记录当前/近期使用的用户（本机实测恒为当前登录账号）。
fn storage_uid_candidates(storage: &serde_json::Value) -> Vec<String> {
    let mut uids = Vec::new();
    if let Some(users) = storage
        .get("icube_gtm")
        .and_then(|g| g.get("users"))
        .and_then(|u| u.as_object())
    {
        for uid in users.keys() {
            if is_uid_token(uid) {
                uids.push(uid.clone());
            }
        }
    }
    uids.sort();
    uids.dedup();
    uids
}

#[derive(Serialize, Clone)]
pub struct DiscoveredAccount {
    /// 账号池体系（Cloud-IDE）uid
    pub user_id: String,
    /// uid 是否经本机证据确认（false 表示本机存在多个候选账号，入池可能不准）
    pub uid_confident: bool,
    /// 应用类别（当前版本固定 "TraeWork"）
    pub app: String,
    /// 展示名
    pub app_label: String,
    /// 是否已在账号池
    pub in_pool: bool,
    /// 命中的 storage.json 路径
    pub storage_path: String,
}

/// 账号池已有 uid 集合：包含 user_id 字段与 JWT 解析出的 data.id（部分账号 user_id 为空）
fn pool_uid_set(accounts: &crate::models::AccountsFile) -> std::collections::HashSet<String> {
    accounts
        .accounts
        .iter()
        .flat_map(|a| {
            let mut ids = Vec::new();
            if let Some(uid) = a.user_id.clone() {
                if !uid.is_empty() {
                    ids.push(uid);
                }
            }
            if !a.jwt.trim().is_empty() {
                if let Some(uid) = crate::jwt::parse(&a.jwt).user_id {
                    ids.push(uid);
                }
            }
            ids
        })
        .collect()
}

/// 扫描本机 Trae Work 应用的登录账号（推导 Cloud-IDE uid，标记是否已入池）。
#[tauri::command]
pub fn apps_accounts_discover(state: State<AppState>) -> Vec<DiscoveredAccount> {
    let accounts: crate::models::AccountsFile =
        fs_utils::read_json(&state.path("checkin_accounts.json"));
    let known = pool_uid_set(&accounts);

    let mut out = Vec::new();
    // 记录实际命中的 storage 路径用于展示
    let mut path_display = String::new();
    for dir in app_data_dirs() {
        let p = dir.join(STORAGE_SUFFIX);
        if p.is_file() {
            path_display = p.to_string_lossy().to_string();
            break;
        }
    }
    let Some(storage) = read_storage_json() else {
        return out;
    };
    let uids = storage_uid_candidates(&storage);
    if uids.is_empty() {
        // 未登录任何账号
        return out;
    }
    // 单一候选 → 置信；多个候选 → 逐条列出但标记不置信（禁止入池）
    let confident = uids.len() == 1;
    for uid in &uids {
        out.push(DiscoveredAccount {
            in_pool: known.contains(uid),
            uid_confident: confident,
            user_id: uid.clone(),
            app: "TraeWork".to_string(),
            app_label: "Trae Work".to_string(),
            storage_path: path_display.clone(),
        });
    }
    out
}

/// 把本机发现的账号加入账号池（无 JWT 占位，待代理捕获后自动回填）。
#[tauri::command]
pub fn apps_account_add(state: State<AppState>, user_id: String, name: String) -> Result<(), String> {
    let uid = user_id.trim().to_string();
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        return Err("无效的 UserID".into());
    }
    let mut accounts: crate::models::AccountsFile =
        fs_utils::read_json(&state.path("checkin_accounts.json"));
    let known = pool_uid_set(&accounts);
    if known.contains(&uid) {
        return Err("该账号已在账号池中".into());
    }
    let display_name = if name.trim().is_empty() {
        let tail = &uid[uid.len().saturating_sub(4)..];
        format!("本机-…{tail}")
    } else {
        name.trim().to_string()
    };
    accounts.accounts.push(crate::models::RawAccount {
        name: display_name.clone(),
        user_id: Some(uid.clone()),
        jwt: String::new(),
        refresh_token: None,
        added_at: Some(fs_utils::now_iso()),
        updated_at: Some(fs_utils::now_iso()),
    });
    fs_utils::write_json(&state.path("checkin_accounts.json"), &accounts)?;
    fs_utils::app_log(
        &state.data_dir,
        &format!("本机发现入池 [{}]: uid={}", display_name, uid),
    );
    Ok(())
}
