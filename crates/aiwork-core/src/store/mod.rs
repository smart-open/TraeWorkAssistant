//! store 基础设施：SQLite 存储层（data 目录 JSON 全量迁移，见 docs/sqllite-storage-plan.md）。
//!
//! - `schema.rs`：全部建表 DDL + user_version 版本管理；
//! - `docs.rs`：各文件的类型化 load/save（迁移器与运行时共用，替代 fs_utils::read_json/write_json）；
//! - `migrate.rs`：启动迁移器（旧 JSON 导入 → 移入 data/backup/，幂等）。
//!
//! 连接策略：按 data_dir 缓存的单 `Mutex<Connection>`（个人应用 QPS 低，免连接池）；
//! WAL + busy_timeout=5000 支撑主进程与 `--task-run` CLI 子进程并发访问。
//! 打开失败自愈：库文件损坏时隔离现场（*.corrupt-<ts>）后重建空库，不 panic 闪退。

pub mod docs;
pub mod migrate;
pub mod schema;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::de::DeserializeOwned;
use serde::Serialize;

pub struct Store {
    conn: Mutex<rusqlite::Connection>,
    /// 数据根目录（供 docs 层写 app_log 等辅助用途；db_path 上两级）
    data_dir: PathBuf,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Store>>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Arc<Store>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取（或创建）data_dir 对应的存储连接。库文件：`<data_dir>/data/aiwork.sqlite`。
pub fn db(data_dir: &Path) -> Arc<Store> {
    let db_path = data_dir.join("data").join("aiwork.sqlite");
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = reg.get(&db_path) {
        return s.clone();
    }
    let store = Store::open(&db_path, data_dir);
    reg.insert(db_path, store.clone());
    store
}

impl Store {
    fn open(path: &Path, data_dir: &Path) -> Arc<Store> {
        match Self::try_open(path, data_dir) {
            Ok(s) => s,
            Err(e) => {
                // 库损坏/不可用自愈：隔离现场后重建空库再试一次。个人应用数据可由
                // data/backup/ 与各上游重新拉取兜底；隔离件保留待人工诊断，绝不静默删除。
                eprintln!("[store] SQLite 打开失败 {}: {e}，隔离损坏库并重建", path.display());
                quarantine_corrupt_db(path, &e);
                Self::try_open(path, data_dir)
                    .unwrap_or_else(|e2| panic!("SQLite 重建仍失败 {}: {e2}", path.display()))
            }
        }
    }

    fn try_open(path: &Path, data_dir: &Path) -> Result<Arc<Store>, String> {
        let conn = rusqlite::Connection::open(path).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.busy_timeout(std::time::Duration::from_millis(5000)).ok();
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        schema::init(&conn)?;
        // 健康探针：损坏库可能 open 成功但任何查询失败（页损坏等），在此暴露
        conn.query_row("SELECT count(*) FROM kv", [], |r| r.get::<_, i64>(0))
            .map_err(|e| format!("库健康探针失败: {e}"))?;
        Ok(Arc::new(Store { conn: Mutex::new(conn), data_dir: data_dir.to_path_buf() }))
    }

    /// 在连接上执行（串行化访问的统一出口）
    pub fn with_conn<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Result<T, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn).map_err(|e| format!("SQLite 操作失败: {e}"))
    }

    // ── KV 文档表 ───────────────────────────────────────────────────────────

    pub fn kv_get_raw(&self, key: &str) -> Option<String> {
        self.with_conn(|c| {
            c.query_row("SELECT content FROM kv WHERE key = ?1", [key], |r| r.get::<_, String>(0))
        })
        .ok()
    }

    pub fn kv_set_raw(&self, key: &str, content: &str) -> Result<(), String> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO kv(key, content, updated_at) VALUES(?1, ?2, datetime('now','localtime'))
                 ON CONFLICT(key) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
                rusqlite::params![key, content],
            )?;
            Ok(())
        })
    }

    pub fn kv_delete(&self, key: &str) -> Result<(), String> {
        self.with_conn(|c| {
            c.execute("DELETE FROM kv WHERE key = ?1", [key])?;
            Ok(())
        })
    }

    /// 读取 KV 文档（语义对齐 fs_utils::read_json：缺失/损坏回退 Default）
    pub fn kv_get<T: DeserializeOwned + Default>(&self, key: &str) -> T {
        self.kv_get_raw(key)
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 写入 KV 文档（紧凑 JSON）
    pub fn kv_set<T: Serialize>(&self, key: &str, value: &T) -> Result<(), String> {
        let content = serde_json::to_string(value).map_err(|e| format!("序列化失败: {e}"))?;
        self.kv_set_raw(key, &content)
    }

    // ── 行文档表原语（单 pk + data JSON 的表）────────────────────────────────
    // 表名白名单：仅限本模块常量，杜绝拼接注入。

    pub fn rows_all(&self, table: &str) -> Result<Vec<(String, serde_json::Value)>, String> {
        if !schema::ROW_TABLES.contains(&table) {
            return Err(format!("非法表名: {table}"));
        }
        let sql = format!("SELECT pk, data FROM [{table}] ORDER BY rowid");
        self.with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map([], |r| {
                    let pk: String = r.get(0)?;
                    let data: String = r.get(1)?;
                    Ok((pk, data))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .map(|(pk, data)| (pk, serde_json::from_str(&data).unwrap_or(serde_json::Value::Null)))
                .collect())
        })
    }

    /// 整表替换（单事务：DELETE 全部 + 顺序 INSERT；对齐原「整文件写回」语义）
    pub fn rows_replace(&self, table: &str, rows: &[(String, serde_json::Value)]) -> Result<(), String> {
        if !schema::ROW_TABLES.contains(&table) {
            return Err(format!("非法表名: {table}"));
        }
        // 序列化在事务外完成（闭包内仅做 rusqlite 操作）
        let mut texts: Vec<(String, String)> = Vec::with_capacity(rows.len());
        for (pk, data) in rows {
            texts.push((
                pk.clone(),
                serde_json::to_string(data).map_err(|e| format!("序列化失败: {e}"))?,
            ));
        }
        self.with_conn(move |c| {
            c.execute_batch(&format!("BEGIN; DELETE FROM [{table}];"))?;
            {
                let mut stmt = c.prepare(&format!(
                    "INSERT INTO [{table}](pk, data, updated_at) VALUES(?1, ?2, datetime('now','localtime'))"
                ))?;
                for (pk, text) in &texts {
                    stmt.execute(rusqlite::params![pk, text])?;
                }
            }
            c.execute_batch("COMMIT;")?;
            Ok(())
        })
        .or_else(|e| {
            // 事务中途失败回滚（连接可能残留未结束的事务）
            let _ = self.with_conn(|c| c.execute_batch("ROLLBACK;"));
            Err(e)
        })
    }

    /// 单行 UPSERT（多写方 map 的增量更新路径，如 wb_tokens / account_cooldowns）
    pub fn row_upsert(&self, table: &str, pk: &str, data: &serde_json::Value) -> Result<(), String> {
        if !schema::ROW_TABLES.contains(&table) {
            return Err(format!("非法表名: {table}"));
        }
        let text = serde_json::to_string(data).map_err(|e| format!("序列化失败: {e}"))?;
        self.with_conn(|c| {
            c.execute(
                &format!(
                    "INSERT INTO [{table}](pk, data, updated_at) VALUES(?1, ?2, datetime('now','localtime'))
                     ON CONFLICT(pk) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at"
                ),
                rusqlite::params![pk, text],
            )?;
            Ok(())
        })
    }
}

/// 损坏库隔离：主库/-wal/-shm 改名 `<原名>.corrupt-<时间戳>`（保留现场，不删除），
/// 随后重建空库。隔离原因尽量落应用日志（库路径上两级 = data_dir）。
fn quarantine_corrupt_db(path: &Path, reason: &str) {
    let stamp = chrono::Local::now().format("%Y%m%d%H%M%S");
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let src = PathBuf::from(name);
        if src.exists() {
            let mut dst = src.as_os_str().to_os_string();
            dst.push(format!(".corrupt-{stamp}"));
            let _ = std::fs::rename(&src, PathBuf::from(dst));
        }
    }
    if let Some(root) = path.parent().and_then(Path::parent) {
        crate::fs_utils::app_log(
            root,
            &format!("SQLite 库损坏已隔离重建: {reason}（原库保留为 *.corrupt-* 文件，可人工抢救）"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("twa_store_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn kv_roundtrip_and_default_fallback() {
        let dir = tmp_dir("kv");
        let s = db(&dir);
        #[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug)]
        struct V {
            a: i32,
            b: String,
        }
        // 缺失 → Default
        assert_eq!(s.kv_get::<V>("nope"), V::default());
        // 写入 → 读回
        s.kv_set("k1", &V { a: 7, b: "x".into() }).unwrap();
        assert_eq!(s.kv_get::<V>("k1"), V { a: 7, b: "x".into() });
        // 覆盖
        s.kv_set("k1", &V { a: 8, b: "y".into() }).unwrap();
        assert_eq!(s.kv_get::<V>("k1"), V { a: 8, b: "y".into() });
        // 删除 → Default
        s.kv_delete("k1").unwrap();
        assert_eq!(s.kv_get::<V>("k1"), V::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rows_roundtrip_and_whitelist() {
        let dir = tmp_dir("rows");
        let s = db(&dir);
        let rows = vec![
            ("u1".to_string(), json!({"v": 1})),
            ("u2".to_string(), json!({"v": 2})),
        ];
        s.rows_replace("device_map", &rows).unwrap();
        let got = s.rows_all("device_map").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "u1");
        assert_eq!(got[1].1["v"], 2);
        // UPSERT 覆盖
        s.row_upsert("device_map", "u1", &json!({"v": 9})).unwrap();
        assert_eq!(s.rows_all("device_map").unwrap()[0].1["v"], 9);
        // 白名单拦截
        assert!(s.rows_all("sqlite_master").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn db_registry_reuses_connection() {
        let dir = tmp_dir("reg");
        let a = db(&dir);
        let b = db(&dir);
        assert!(Arc::ptr_eq(&a, &b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 损坏库自愈：垃圾内容库 → 隔离为 *.corrupt-* → 重建空库可用，不 panic
    #[test]
    fn corrupt_db_is_quarantined_and_rebuilt() {
        let dir = tmp_dir("corrupt");
        let db_path = dir.join("data").join("aiwork.sqlite");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        std::fs::write(&db_path, b"definitely not a sqlite database").unwrap();

        let s = db(&dir); // 应自愈而非 panic
        s.kv_set("k", &json!({"v": 1})).unwrap();
        assert_eq!(s.kv_get_raw("k").is_some(), true);

        // 现场保留为 *.corrupt-*（不静默删除，可人工抢救）
        let quarantined = std::fs::read_dir(db_path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt-"));
        assert!(quarantined, "损坏库应被隔离保留");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
