//! F-08 双应用账号自动发现 + Trae 会员/套餐信息展示
//!
//! 数据来源（本机实测，2026-09-07）：
//! - `%APPDATA%\TRAE SOLO CN\User\globalStorage\storage.json`（Trae Work）
//! - `%APPDATA%\Trae CN\User\globalStorage\storage.json`（Trae CN IDE）
//!
//! 两个应用的 storage.json 同构：
//! - 登录账号：键 `iCubeAuthInfo://icube-dc:<uid>`（uid 明文在键名中，值为 aha_kit 加密
//!   blob，不做解密——沿用"快照/恢复"策略）。一个文件里可能出现多个账号键。
//! - 套餐信息：键 `iCubeServerData://icube.cloudide`，值为 JSON 字符串（明文），
//!   `entitlementInfo.identityStr` / `identity` 即当前登录账号的套餐
//!   （Free=0 / Lite=5 / Pro=...），与 `ide_user_pay_status` 接口的
//!   `user_pay_identity_str` 同源。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::State;

use crate::fs_utils;
use crate::state::AppState;

const STORAGE_SUFFIX: &str = r"User\globalStorage\storage.json";

/// 单个应用的数据目录候选（按优先级）
fn app_data_dirs(app_kind: &str) -> Vec<std::path::PathBuf> {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    match app_kind {
        "TraeWork" => vec![
            std::path::PathBuf::from(&appdata).join("TRAE SOLO CN"),
            std::path::PathBuf::from(&appdata).join("TRAE SOLO"),
        ],
        "Trae" => vec![std::path::PathBuf::from(&appdata).join("Trae CN")],
        _ => vec![],
    }
}

fn app_label(app_kind: &str) -> &str {
    match app_kind {
        "TraeWork" => "Trae Work",
        "Trae" => "Trae",
        _ => app_kind,
    }
}

/// 读取应用的 storage.json（多个候选目录取第一个存在的）
fn read_storage_json(app_kind: &str) -> Option<serde_json::Value> {
    for dir in app_data_dirs(app_kind) {
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

/// 从 storage.json 提取登录账号 uid 列表（键名 `iCubeAuthInfo://icube-dc:<uid>`）
fn extract_uids(storage: &serde_json::Value) -> Vec<String> {
    let mut uids = Vec::new();
    if let Some(obj) = storage.as_object() {
        for key in obj.keys() {
            if let Some(uid) = key.strip_prefix("iCubeAuthInfo://icube-dc:") {
                let uid = uid.trim();
                if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) {
                    uids.push(uid.to_string());
                }
            }
        }
    }
    uids.sort();
    uids.dedup();
    uids
}

#[derive(Serialize, Clone)]
pub struct DiscoveredAccount {
    pub user_id: String,
    /// 应用类别：TraeWork | Trae
    pub app: String,
    /// 展示名：Trae Work / Trae
    pub app_label: String,
    /// 是否已在账号池
    pub in_pool: bool,
    /// 该应用本机登录的账号总数（含已入池）
    pub storage_path: String,
}

/// F-08：扫描本机两个 Trae 应用的 storage.json，返回登录账号列表（标记是否已入池）。
#[tauri::command]
pub fn apps_accounts_discover(state: State<AppState>) -> Vec<DiscoveredAccount> {
    let accounts: crate::models::AccountsFile =
        fs_utils::read_json(&state.path("checkin_accounts.json"));
    let known: std::collections::HashSet<String> = accounts
        .accounts
        .iter()
        .filter_map(|a| a.user_id.clone())
        .collect();

    let mut out = Vec::new();
    for kind in ["TraeWork", "Trae"] {
        // 记录实际命中的 storage 路径用于展示
        let mut path_display = String::new();
        for dir in app_data_dirs(kind) {
            let p = dir.join(STORAGE_SUFFIX);
            if p.is_file() {
                path_display = p.to_string_lossy().to_string();
                break;
            }
        }
        if let Some(storage) = read_storage_json(kind) {
            for uid in extract_uids(&storage) {
                out.push(DiscoveredAccount {
                    in_pool: known.contains(&uid),
                    user_id: uid,
                    app: kind.to_string(),
                    app_label: app_label(kind).to_string(),
                    storage_path: path_display.clone(),
                });
            }
        }
    }
    out
}

/// F-08：把本机发现的账号加入账号池（无 JWT 占位，待代理捕获后自动回填）。
#[tauri::command]
pub fn apps_account_add(
    state: State<AppState>,
    user_id: String,
    name: String,
    app: String,
) -> Result<(), String> {
    let uid = user_id.trim().to_string();
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        return Err("无效的 UserID".into());
    }
    let mut accounts: crate::models::AccountsFile =
        fs_utils::read_json(&state.path("checkin_accounts.json"));
    if accounts
        .accounts
        .iter()
        .any(|a| a.user_id.as_deref() == Some(uid.as_str()))
    {
        return Err("该账号已在账号池中".into());
    }
    let display_name = if name.trim().is_empty() {
        let tail = &uid[uid.len().saturating_sub(4)..];
        format!("{}-…{}", app_label(&app), tail)
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
        &format!("自动发现入池 [{}]: uid={} app={}", display_name, uid, app),
    );
    Ok(())
}

// ---------------- 会员/套餐信息 ----------------

#[derive(Serialize, Clone, Default)]
pub struct AppEntitlement {
    /// 应用类别：TraeWork | Trae
    pub app: String,
    /// 展示名
    pub app_label: String,
    /// 套餐名（Free / Lite / Pro ...）
    pub identity_str: Option<String>,
    /// 套餐数值（0=Free, 5=Lite ...）
    pub identity: Option<i64>,
    /// 服务端最后同步时间（毫秒）
    pub last_sync_time: Option<i64>,
}

#[derive(Serialize, Clone)]
pub struct LocalEntitlement {
    pub work: Option<AppEntitlement>,
    pub cn: Option<AppEntitlement>,
}

fn parse_entitlement(kind: &str, storage: &serde_json::Value) -> Option<AppEntitlement> {
    let raw = storage
        .get("iCubeServerData://icube.cloudide")
        .and_then(|v| v.as_str())?;
    let data: serde_json::Value = serde_json::from_str(raw).ok()?;
    let ent = data.get("entitlementInfo")?;
    Some(AppEntitlement {
        app: kind.to_string(),
        app_label: app_label(kind).to_string(),
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

/// 读取本机两个 Trae 应用当前登录账号的套餐信息（零 API，来自 storage.json 明文缓存）。
#[tauri::command]
pub fn apps_entitlement_read() -> LocalEntitlement {
    let mut work = None;
    let mut cn = None;
    if let Some(storage) = read_storage_json("TraeWork") {
        work = parse_entitlement("TraeWork", &storage);
    }
    if let Some(storage) = read_storage_json("Trae") {
        cn = parse_entitlement("Trae", &storage);
    }
    LocalEntitlement { work, cn }
}

// ---------------- 账号级套餐（API） ----------------

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PayStatusEntry {
    /// 套餐名（Free / Lite / Pro ...）
    pub identity_str: String,
    /// 套餐数值
    pub identity: i64,
    /// 是否新客（未付费过）
    pub is_pay_freshman: bool,
    /// 是否积分计费
    pub is_credits_billing: bool,
    /// 查询时间（Unix 秒）
    pub fetched_at: i64,
}

/// pay_status.json 结构：{ statuses: {uid: entry}, updated_at }
#[derive(Serialize, Deserialize, Default)]
pub struct PayStatusFile {
    #[serde(default)]
    pub statuses: HashMap<String, PayStatusEntry>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

fn query_pay_status(jwt: &str) -> Result<PayStatusEntry, String> {
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    let resp = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .post("https://api.trae.cn/trae/api/v2/pay/ide_user_pay_status")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({"req_source": 2}))
        .map_err(|e| format!("API 请求失败: {}", e))?;
    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;
    Ok(PayStatusEntry {
        identity_str: body
            .get("user_pay_identity_str")
            .and_then(|v| v.as_str())
            .unwrap_or("Free")
            .to_string(),
        identity: body
            .get("user_pay_identity")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        is_pay_freshman: body
            .get("is_pay_freshman")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        is_credits_billing: body
            .get("is_credits_billing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        fetched_at: chrono::Utc::now().timestamp(),
    })
}

/// 刷新所有账号的套餐身份（批量调用 ide_user_pay_status，写入 pay_status.json 缓存）。
/// 返回成功数量。
#[tauri::command]
pub fn refresh_pay_status(state: State<AppState>) -> Result<usize, String> {
    let accounts: crate::models::AccountsFile =
        fs_utils::read_json(&state.path("checkin_accounts.json"));
    let mut file: PayStatusFile = fs_utils::read_json(&state.path("pay_status.json"));
    let mut ok = 0usize;
    for a in &accounts.accounts {
        // 无 JWT 的占位账号（自动发现入池）跳过
        if a.jwt.trim().is_empty() {
            continue;
        }
        let Some(uid) = a.user_id.clone() else { continue };
        match query_pay_status(&a.jwt) {
            Ok(entry) => {
                file.statuses.insert(uid, entry);
                ok += 1;
            }
            Err(e) => {
                fs_utils::app_log(
                    &state.data_dir,
                    &format!("查询套餐失败 [{}]: {}", a.name, e),
                );
            }
        }
    }
    file.updated_at = Some(fs_utils::now_iso());
    fs_utils::write_json(&state.path("pay_status.json"), &file)?;
    Ok(ok)
}
