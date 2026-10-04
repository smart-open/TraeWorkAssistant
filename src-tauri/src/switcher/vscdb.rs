//! F-68：Trae 项目列表 / 最近打开跨账号保留（icube 布局 `state.vscdb` 全局键合并）。
//!
//! 背景：切换账号时 `state.vscdb` 随槽位快照整体回滚，而项目列表
//!（`solo-lite.local-project-folders`）与最近打开
//!（`history.recentlyOpenedPathsList`）是**全局单键**（非账号分区键），
//! 回滚后只剩目标账号自己那一份 → 用户感知为"项目列表消失了"。
//!
//! 方案：恢复快照**前**抽出这两个键，恢复**后**按条目合并回写（快照内已有的以
//! 快照为准，仅补入切换前多出来的条目）；账号分区键
//!（`solo-lite:content-map:<uid>` / `solo-lite-mode-state-map-<uid>`）
//! 一律不碰（跨账号合并会产生服务端归属校验失败的"幽灵会话"）。
//!
//! 零新增依赖：`rusqlite` 已是项目依赖（vscdb 读库先例见 `trae_apps.rs`）。

use std::collections::HashSet;
use std::path::Path;

/// 项目列表（SOLO 本地项目文件夹）
const KEY_PROJECT_FOLDERS: &str = "solo-lite.local-project-folders";
/// 最近打开（VSCode 系通用键）
const KEY_RECENT_PATHS: &str = "history.recentlyOpenedPathsList";

/// 单键取值（保留原存储类型：VSCode 系该列多为 BLOB 存 JSON 文本，
/// 回写时按原类型写，避免把 BLOB 列改成 TEXT 造成客户端读取差异）
#[derive(Clone, Debug, PartialEq, Eq)]
struct KeyVal {
    text: String,
    blob: bool,
}

/// 切换前抽出的全局键快照
#[derive(Default, Clone, Debug)]
pub struct GlobalKeys {
    pub project_folders: Option<String>,
    pub recent_paths: Option<String>,
    /// 原值是 BLOB 存储时为 true（回写保持同类型）
    folders_blob: bool,
    recent_blob: bool,
}

impl GlobalKeys {
    fn is_empty(&self) -> bool {
        self.project_folders.is_none() && self.recent_paths.is_none()
    }
}

fn open_ro(path: &Path) -> Option<rusqlite::Connection> {
    if !path.is_file() {
        return None;
    }
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// 读 ItemTable 单键；值列为 BLOB 时按 UTF-8 解码（失败 → None，保守不动）
fn read_key(conn: &rusqlite::Connection, key: &str) -> Option<KeyVal> {
    let mut stmt = conn.prepare("SELECT value FROM ItemTable WHERE key = ?1").ok()?;
    let mut rows = stmt.query(rusqlite::params![key]).ok()?;
    let row = rows.next().ok()??;
    let v = row.get::<_, rusqlite::types::Value>(0).ok()?;
    match v {
        rusqlite::types::Value::Text(s) => Some(KeyVal { text: s, blob: false }),
        rusqlite::types::Value::Blob(b) => String::from_utf8(b).ok().map(|text| KeyVal { text, blob: true }),
        _ => None,
    }
}

/// 抽出待保留的全局键（恢复快照前调用；文件缺失/读失败 → 空快照，调用方自然跳过）
pub fn snapshot_global_keys(vscdb: &Path) -> GlobalKeys {
    let Some(conn) = open_ro(vscdb) else {
        return GlobalKeys::default();
    };
    let folders = read_key(&conn, KEY_PROJECT_FOLDERS);
    let recent = read_key(&conn, KEY_RECENT_PATHS);
    GlobalKeys {
        folders_blob: folders.as_ref().map(|k| k.blob).unwrap_or(false),
        recent_blob: recent.as_ref().map(|k| k.blob).unwrap_or(false),
        project_folders: folders.map(|k| k.text),
        recent_paths: recent.map(|k| k.text),
    }
}

/// 恢复快照后把切换前的全局键合并回写。
///
/// - `Ok(None)`：无需写入（无切换前数据 / 快照已含全部条目 / 结构无法解析）；
/// - `Ok(Some(摘要))`：已写入，摘要供进度流展示；
/// - `Err`：写入失败（已回滚备份，调用方按 warn 处理，不阻断切换）。
pub fn merge_global_keys(vscdb: &Path, pre: &GlobalKeys) -> Result<Option<String>, String> {
    if pre.is_empty() || !vscdb.is_file() {
        return Ok(None);
    }
    let cur = snapshot_global_keys(vscdb);

    let mut writes: Vec<(&'static str, String, bool)> = Vec::new();
    let mut folders_added = 0usize;
    let mut recent_added = 0usize;
    if let Some(p) = pre.project_folders.as_deref() {
        match merge_value(cur.project_folders.as_deref(), p) {
            Some((merged, added)) => {
                folders_added = added;
                let blob = cur.folders_blob || pre.folders_blob;
                writes.push((KEY_PROJECT_FOLDERS, merged, blob));
            }
            None => {}
        }
    }
    if let Some(p) = pre.recent_paths.as_deref() {
        match merge_value(cur.recent_paths.as_deref(), p) {
            Some((merged, added)) => {
                recent_added = added;
                let blob = cur.recent_blob || pre.recent_blob;
                writes.push((KEY_RECENT_PATHS, merged, blob));
            }
            None => {}
        }
    }
    if writes.is_empty() {
        return Ok(None);
    }

    // 写前一次性备份，失败回滚（与 F-68 设计"操作前 .bak 备份"一致；单代覆盖）
    let bak = bak_path(vscdb);
    std::fs::copy(vscdb, &bak).map_err(|e| format!("备份 state.vscdb 失败: {e}"))?;
    if let Err(e) = write_keys(vscdb, &writes) {
        let _ = std::fs::copy(&bak, vscdb);
        return Err(e);
    }

    let mut parts: Vec<String> = Vec::new();
    if writes.iter().any(|w| w.0 == KEY_PROJECT_FOLDERS) {
        parts.push(format!("项目列表 +{folders_added}"));
    }
    if writes.iter().any(|w| w.0 == KEY_RECENT_PATHS) {
        parts.push(format!("最近打开 +{recent_added}"));
    }
    Ok(Some(parts.join(" / ")))
}

fn bak_path(vscdb: &Path) -> std::path::PathBuf {
    let name = vscdb
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "state.vscdb".to_string());
    vscdb.with_file_name(format!("{name}.f68.bak"))
}

fn write_keys(vscdb: &Path, writes: &[(&str, String, bool)]) -> Result<(), String> {
    let mut conn = rusqlite::Connection::open(vscdb).map_err(|e| format!("打开 state.vscdb 失败: {e}"))?;
    let tx = conn.transaction().map_err(|e| format!("开启事务失败: {e}"))?;
    for (key, val, blob) in writes {
        let v: rusqlite::types::Value = if *blob {
            rusqlite::types::Value::Blob(val.as_bytes().to_vec())
        } else {
            rusqlite::types::Value::Text(val.clone())
        };
        tx.execute(
            "INSERT INTO ItemTable(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, v],
        )
        .map_err(|e| format!("写入 {key} 失败: {e}"))?;
    }
    tx.commit().map_err(|e| format!("提交 state.vscdb 失败: {e}"))
}

/// F-80 §5.10.2：单键 TEXT upsert（键不存在则插入）。文件缺失 → Ok(false)
///（调用方按可跳过处理）；单条写入事务原子、失败即整体回滚，无需写前备份
///（区别于 merge_global_keys 的多键合并回滚场景）。返回 Ok(true) = 已写入。
pub fn upsert_text_key(vscdb: &Path, key: &str, val: &str) -> Result<bool, String> {
    if !vscdb.is_file() {
        return Ok(false);
    }
    let mut conn =
        rusqlite::Connection::open(vscdb).map_err(|e| format!("打开 state.vscdb 失败: {e}"))?;
    let tx = conn.transaction().map_err(|e| format!("开启事务失败: {e}"))?;
    tx.execute(
        "INSERT INTO ItemTable(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, val],
    )
    .map_err(|e| format!("写入 {key} 失败: {e}"))?;
    tx.commit().map_err(|e| format!("提交 state.vscdb 失败: {e}"))?;
    Ok(true)
}

/// 合并单键值：快照值（cur）优先，仅补入切换前（pre）多出的条目。
/// 返回 None = 无需写入（相同 / 结构不可解析，保守保持快照值）。
fn merge_value(cur: Option<&str>, pre: &str) -> Option<(String, usize)> {
    match cur {
        // 快照无此键 → 整体取切换前的值
        None => {
            let n = count_items(pre);
            Some((pre.to_string(), n))
        }
        Some(c) => {
            if c == pre {
                return None;
            }
            let (cv, pv) = (serde_json::from_str::<serde_json::Value>(c).ok()?, serde_json::from_str::<serde_json::Value>(pre).ok()?);
            let (merged, added) = merge_json(&cv, &pv)?;
            if added == 0 {
                return None;
            }
            Some((serde_json::to_string(&merged).unwrap_or_else(|_| c.to_string()), added))
        }
    }
}

/// 条目计数（JSON 数组长度 / 含 entries 数组的对象取其长度 / 对象取键数 / 其他 0）
fn count_items(raw: &str) -> usize {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return 0;
    };
    match &v {
        serde_json::Value::Array(a) => a.len(),
        serde_json::Value::Object(o) => match o.get("entries").and_then(|e| e.as_array()) {
            Some(a) => a.len(),
            None => o.len(),
        },
        _ => 0,
    }
}

/// JSON 结构合并（快照 cur 在先，pre-only 条目追加）：
/// 数组 → 按条目身份键去重追加；含 entries 数组的对象 → 合并 entries；
/// 其他对象 → 按键合并（cur 键优先）；其余形态 → None（不合并）
fn merge_json(cur: &serde_json::Value, pre: &serde_json::Value) -> Option<(serde_json::Value, usize)> {
    match (cur, pre) {
        (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
            let (merged, added) = merge_array(a, b);
            Some((serde_json::Value::Array(merged), added))
        }
        (serde_json::Value::Object(_), serde_json::Value::Object(_)) => {
            let (ce, pe) = (cur.get("entries").and_then(|v| v.as_array()), pre.get("entries").and_then(|v| v.as_array()));
            match (ce, pe) {
                (Some(a), Some(b)) => {
                    let (merged, added) = merge_array(a, b);
                    let mut out = cur.clone();
                    out["entries"] = serde_json::Value::Array(merged);
                    Some((out, added))
                }
                _ => {
                    // 无 entries 的纯对象：按键合并（cur 键优先）
                    let mut out = cur.as_object().cloned()?;
                    let mut added = 0usize;
                    for (k, v) in pre.as_object()? {
                        if !out.contains_key(k) {
                            out.insert(k.clone(), v.clone());
                            added += 1;
                        }
                    }
                    Some((serde_json::Value::Object(out), added))
                }
            }
        }
        _ => None,
    }
}

fn merge_array(cur: &[serde_json::Value], pre: &[serde_json::Value]) -> (Vec<serde_json::Value>, usize) {
    let mut seen: HashSet<String> = cur.iter().map(item_key).collect();
    let mut out = cur.to_vec();
    let mut added = 0usize;
    for it in pre {
        let k = item_key(it);
        if seen.insert(k) {
            out.push(it.clone());
            added += 1;
        }
    }
    (out, added)
}

/// 条目身份键：对象按 id → uri/path/folderUri/fileUri → label 取首个非空字符串字段；
/// 无可用标识字段时退化为整段序列化（结构相同即视为同一条目）
fn item_key(v: &serde_json::Value) -> String {
    if let Some(o) = v.as_object() {
        for k in ["id", "uri", "path", "folderUri", "fileUri", "label"] {
            if let Some(s) = o.get(k).and_then(|x| x.as_str()) {
                return format!("{k}={s}");
            }
        }
    }
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "f68-{tag}-{}-{}.vscdb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// 造库：ItemTable(key TEXT PRIMARY KEY, value BLOB)，value 按 blob 标志存 BLOB/TEXT
    fn seed(path: &std::path::Path, rows: &[(&str, &str, bool)]) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value BLOB)")
            .unwrap();
        for (k, v, blob) in rows {
            let val = if *blob {
                rusqlite::types::Value::Blob(v.as_bytes().to_vec())
            } else {
                rusqlite::types::Value::Text((*v).to_string())
            };
            conn.execute("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?1, ?2)", rusqlite::params![k, val])
                .unwrap();
        }
        conn
    }

    fn read_text(path: &std::path::Path, key: &str) -> Option<String> {
        let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        read_key(&conn, key).map(|k| k.text)
    }

    #[test]
    fn snapshot_读取text与blob两种存储() {
        let p = tmp_db("snap");
        seed(&p, &[(KEY_PROJECT_FOLDERS, "[{\"id\":\"a\"}]", true), (KEY_RECENT_PATHS, "{\"entries\":[]}", false)]);
        let g = snapshot_global_keys(&p);
        assert_eq!(g.project_folders.as_deref(), Some("[{\"id\":\"a\"}]"));
        assert_eq!(g.recent_paths.as_deref(), Some("{\"entries\":[]}"));
        assert!(g.folders_blob && !g.recent_blob);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 快照缺键时整体补入切换前的值() {
        let p = tmp_db("miss");
        seed(&p, &[(KEY_RECENT_PATHS, "{\"entries\":[{\"folderUri\":\"file:///x\"}]}", false)]);
        let pre = GlobalKeys {
            project_folders: Some("[{\"id\":\"p1\"},{\"id\":\"p2\"}]".into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        let msg = merge_global_keys(&p, &pre).unwrap().unwrap();
        assert!(msg.contains("项目列表 +2"), "{msg}");
        assert_eq!(read_text(&p, KEY_PROJECT_FOLDERS).as_deref(), Some("[{\"id\":\"p1\"},{\"id\":\"p2\"}]"));
        // 未提供的键零改动
        assert_eq!(
            read_text(&p, KEY_RECENT_PATHS).as_deref(),
            Some("{\"entries\":[{\"folderUri\":\"file:///x\"}]}")
        );
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(bak_path(&p));
    }

    #[test]
    fn 数组按id去重_快照项在前() {
        let p = tmp_db("arr");
        seed(&p, &[(KEY_PROJECT_FOLDERS, "[{\"id\":\"b\"}]", false)]);
        let pre = GlobalKeys {
            project_folders: Some("[{\"id\":\"a\"},{\"id\":\"b\"},{\"id\":\"c\"}]".into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        let msg = merge_global_keys(&p, &pre).unwrap().unwrap();
        assert!(msg.contains("+2"), "{msg}");
        let out = read_text(&p, KEY_PROJECT_FOLDERS).unwrap();
        let arr = serde_json::from_str::<serde_json::Value>(&out).unwrap();
        let ids: Vec<String> = arr
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.get("id").and_then(|x| x.as_str()).unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["b".to_string(), "a".to_string(), "c".to_string()]);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(bak_path(&p));
    }

    #[test]
    fn entries对象形态_按folder_uri去重() {
        let p = tmp_db("entries");
        seed(
            &p,
            &[(
                KEY_RECENT_PATHS,
                "{\"entries\":[{\"folderUri\":\"file:///curr\"}]}",
                false,
            )],
        );
        let pre = GlobalKeys {
            project_folders: None,
            recent_paths: Some(
                "{\"entries\":[{\"folderUri\":\"file:///curr\"},{\"folderUri\":\"file:///old\"}]}".into(),
            ),
            folders_blob: false,
            recent_blob: false,
        };
        let msg = merge_global_keys(&p, &pre).unwrap().unwrap();
        assert!(msg.contains("最近打开 +1"), "{msg}");
        let out = read_text(&p, KEY_RECENT_PATHS).unwrap();
        assert!(out.contains("file:///old") && out.matches("file:///curr").count() == 1);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(bak_path(&p));
    }

    #[test]
    fn 完全一致时零写入() {
        let p = tmp_db("same");
        seed(&p, &[(KEY_PROJECT_FOLDERS, "[{\"id\":\"a\"}]", false)]);
        let pre = GlobalKeys {
            project_folders: Some("[{\"id\":\"a\"}]".into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        assert!(merge_global_keys(&p, &pre).unwrap().is_none());
        // 零写入不产生备份文件
        assert!(!bak_path(&p).exists());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 非json值保持快照值不合并() {
        let p = tmp_db("plain");
        seed(&p, &[(KEY_PROJECT_FOLDERS, "not-json", false)]);
        let pre = GlobalKeys {
            project_folders: Some("also-not-json".into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        assert!(merge_global_keys(&p, &pre).unwrap().is_none());
        assert_eq!(read_text(&p, KEY_PROJECT_FOLDERS).as_deref(), Some("not-json"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 文件缺失时安全跳过() {
        let p = tmp_db("nofile");
        let pre = GlobalKeys {
            project_folders: Some("[]".into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        assert!(merge_global_keys(&p, &pre).unwrap().is_none());
    }
}
