//! 本机 Trae Work（TRAE SOLO CN）应用数据读取：当前登录账号扫描 + 本机套餐。
//!
//! 数据来源（本机实测，2026-09）：
//! - `%APPDATA%\TRAE SOLO CN\User\globalStorage\storage.json`
//! - `%APPDATA%\TRAE SOLO CN\logs\<会话>\*.log`（当前登录 uid 证据）
//! - `%APPDATA%\TRAE SOLO CN\User\globalStorage\state.vscdb`（历史登录 uid 痕迹）
//!
//! uid 证据链与体系说明：
//! 1. **当前登录 uid（置信）**：会话日志（main.log / dynamicConfig.log 等）中
//!    请求 URL 的查询参数 `…&did=<设备id>&uid=<CloudIDE uid>&…`，取最新一次命中。
//! 2. **历史登录 uid（不置信）**：state.vscdb 原始字节中的键名痕迹
//!    （`<uid>:AI.agent…`、`solo-lite-mode-state-map-<uid>` 等），多账号时无法区分当前。
//! 3. `storage.json` 的 `icube_gtm.users` 键名（Trae CN IDE 变体，兼容保留）。
//!
//! ⚠️ `storage.json` 键 `iCubeAuthInfo://icube-dc:<did>` 是**设备 id**（ICDRS did），
//! 不是用户 id，不可入池。
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

/// 从文本中找最后一次出现的 `uid=<15~16位纯数字>`（URL 查询参数形态）。
/// 要求 uid 后不再跟数字、前面不是字母数字，避免误匹配更长数字串。
fn find_last_uid_param(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut last = None;
    let mut from = 0usize;
    while let Some(pos) = text[from..].find("uid=") {
        let start = from + pos + 4;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let n = end - start;
        if (15..=16).contains(&n) {
            let prev_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
            let next_ok = end >= bytes.len() || !bytes[end].is_ascii_digit();
            if prev_ok && next_ok {
                last = Some(text[start..end].to_string());
            }
        }
        from = end.max(start).max(from + 1);
        if from >= text.len() {
            break;
        }
    }
    last
}

/// 从会话日志提取当前登录 uid（置信证据）：
/// logs/<会话目录>/main.log|dynamicConfig.log 等中请求 URL 的 `&uid=<digits>`，
/// 会话目录按名称倒序（名称含启动时间戳），取最新命中。
fn current_uid_from_logs(dir: &std::path::Path) -> Option<String> {
    let mut sessions: Vec<std::path::PathBuf> = std::fs::read_dir(dir.join("logs"))
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    sessions.sort();
    sessions.reverse();
    for session in sessions.iter().take(3) {
        for name in [
            "dynamicConfig.log",
            "main.log",
            "renderer.log",
            "sharedprocess.log",
        ] {
            let p = session.join(name);
            if let Ok(content) = std::fs::read_to_string(&p) {
                if let Some(uid) = find_last_uid_param(&content) {
                    return Some(uid);
                }
            }
        }
    }
    None
}

/// 从 state.vscdb（SQLite）原始字节提取历史登录 uid（不置信证据）。
/// 键名形态：`<uid>:AI.agent.model.model_list_map`、`<uid>:cn.fast_request.lock_map`、
/// `<uid>:remote-welcome-toast-shown`、`solo-lite-mode-state-map-<uid>`。
/// 零 SQLite 依赖，直接字节扫描（键名在文件中为明文 ASCII）。
fn historical_uids_from_vscdb(dir: &std::path::Path) -> Vec<String> {
    let p = dir.join(r"User\globalStorage\state.vscdb");
    let Ok(bytes) = std::fs::read(&p) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let tb = text.as_bytes();
    let mut uids = Vec::new();

    // 形态1：<uid>:<已知键名> — 在标记前回退数字
    for marker in [
        ":AI.agent.model.model_list_map",
        ":cn.fast_request.lock_map",
        ":remote-welcome-toast-shown",
    ] {
        let mut from = 0usize;
        while let Some(pos) = text[from..].find(marker) {
            let abs = from + pos;
            let mut s = abs;
            while s > 0 && tb[s - 1].is_ascii_digit() {
                s -= 1;
            }
            let n = abs - s;
            if (15..=16).contains(&n) {
                uids.push(text[s..abs].to_string());
            }
            from = abs + marker.len();
            if from >= text.len() {
                break;
            }
        }
    }

    // 形态2：solo-lite-mode-state-map-<uid> — 在标记后取数字
    let marker = "solo-lite-mode-state-map-";
    let mut from = 0usize;
    while let Some(pos) = text[from..].find(marker) {
        let start = from + pos + marker.len();
        let mut end = start;
        while end < tb.len() && tb[end].is_ascii_digit() {
            end += 1;
        }
        let n = end - start;
        if (15..=16).contains(&n) {
            uids.push(text[start..end].to_string());
        }
        from = end.max(from + 1);
        if from >= text.len() {
            break;
        }
    }

    uids.sort();
    uids.dedup();
    uids
}

/// 从 storage.json 提取 Cloud-IDE uid 证据（Trae CN IDE 变体的回退证据）：
/// `icube_gtm.users` 键名仅记录当前/近期使用的用户。
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
///
/// 证据链（按目录逐个尝试，首个有结果的目录生效）：
/// 1. 会话日志 `&uid=` → 当前登录（置信）；
/// 2. state.vscdb 历史痕迹 + storage.json `icube_gtm.users` → 历史候选（多个时不置信）。
/// 文件扫描命令（vscdb 可达数十 MB），标记 async 交由异步线程池派发，避免阻塞主线程。
#[tauri::command(async)]
pub fn apps_accounts_discover(state: State<AppState>) -> Vec<DiscoveredAccount> {
    let accounts: crate::models::AccountsFile =
        fs_utils::read_json(&state.path("checkin_accounts.json"));
    let known = pool_uid_set(&accounts);

    let mut out = Vec::new();
    for dir in app_data_dirs() {
        if !dir.is_dir() {
            continue;
        }
        let storage_path = dir.join(STORAGE_SUFFIX);
        let path_display = if storage_path.is_file() {
            storage_path.to_string_lossy().to_string()
        } else {
            String::new()
        };

        let push = |uids: Vec<String>, confident: bool, out: &mut Vec<DiscoveredAccount>| {
            for uid in uids {
                out.push(DiscoveredAccount {
                    in_pool: known.contains(&uid),
                    uid_confident: confident,
                    user_id: uid,
                    app: "TraeWork".to_string(),
                    app_label: "Trae Work".to_string(),
                    storage_path: path_display.clone(),
                });
            }
        };

        // 证据 1：会话日志中的当前登录 uid（置信，单一结果）
        if let Some(uid) = current_uid_from_logs(&dir) {
            push(vec![uid], true, &mut out);
            return out;
        }

        // 证据 2：历史登录痕迹（vscdb 键名 + icube_gtm），多个候选时无法区分当前
        let mut uids = historical_uids_from_vscdb(&dir);
        if let Ok(content) = std::fs::read_to_string(&storage_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                uids.extend(storage_uid_candidates(&v));
            }
        }
        uids.sort();
        uids.dedup();
        if !uids.is_empty() {
            let confident = uids.len() == 1;
            push(uids, confident, &mut out);
            return out;
        }
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
    crate::vault::save_accounts(&state, &mut accounts)?;
    fs_utils::app_log(
        &state.data_dir,
        &format!("本机发现入池 [{}]: uid={}", display_name, uid),
    );
    Ok(())
}
