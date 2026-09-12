//! WorkBuddy M8 会话域（原 workbuddy.rs 机械拆分）：会话三件套备份/恢复/状态（F-44，§3.11）、
//! 会话复制/迁移·新 id 算法（F-45）。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。

use std::path::PathBuf;
use tauri::State;

use crate::fs_utils;
use crate::state::AppState;

use super::common::{load_pool, wb_chat_uid_guard, wb_data_dir};

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

#[cfg(test)]
mod chatdata_tests {
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
