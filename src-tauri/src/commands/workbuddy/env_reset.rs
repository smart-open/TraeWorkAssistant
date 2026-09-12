//! WorkBuddy M7 生态接入 · 环境重置 / 彻底登出（F-14，§3.10）（原 workbuddy.rs 机械拆分）。
//!
//! 16 项认证残留清理清单（对齐 oss-research/antigravity-tools oauth.py _clear_all_auth 的
//! 17 个物理位置：「认证文件」一项合并 workbuddy-desktop.info 与 .neodata_token 两个文件）。
//! 执行顺序：Keycloak SSO 注销（需当前 token）→ 关闭 WorkBuddy → 按勾选项逐项清理。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。

use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::state::AppState;

use super::common::{as_str, auth_file_path, auth_file_path_of, push_notify, token_store_path, wb_data_dir};
use super::oauth::{jwt_claims, open_in_browser};

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
    let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path_of(state));
    if let Some(t) = as_str(fs_utils::dig(&raw, &["accessToken"])) {
        if !t.is_empty() {
            return Some(t);
        }
    }
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(state));
    let mut best: Option<(i64, String)> = None;
    if let Some(tokens) = store.get("tokens").and_then(|t| t.as_object()) {
        for rec in tokens.values() {
            let t = as_str(fs_utils::dig(&rec, &["access_token"])).unwrap_or_default();
            if t.is_empty() {
                continue;
            }
            let exp = fs_utils::dig(&rec, &["expires_at_ms"]).and_then(|v| v.as_i64()).unwrap_or(0);
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

#[cfg(test)]
mod env_reset_tests {
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
}
