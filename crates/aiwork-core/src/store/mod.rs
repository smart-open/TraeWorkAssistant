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
                if let Ok(s) = Self::try_open(path, data_dir) {
                    return s;
                }
                // 审查 P2 兜底：重建仍失败不再 panic 击穿启动/调用线程——降级内存库
                // 续命，本会话读写不落盘（退出即失），应用可启动、其余子系统可排查；
                // 内存库也不可用时才 panic（真无路可走）。registry 仍以原路径为键，
                // 同会话后续 db() 命中同一内存库，视图一致
                eprintln!("[store] SQLite 重建仍失败 {}，降级内存库运行（本会话数据不落盘）", path.display());
                crate::fs_utils::app_log(
                    data_dir,
                    &format!(
                        "[store] SQLite 重建仍失败 {}，已降级内存库运行（本会话数据不落盘，请排查 data/ 隔离件）",
                        path.display()
                    ),
                );
                let conn = rusqlite::Connection::open_in_memory()
                    .map_err(|e| e.to_string())
                    .and_then(|c| {
                        schema::init(&c)?;
                        Ok(c)
                    })
                    .unwrap_or_else(|e2| panic!("SQLite 内存库兜底亦失败 {}: {e2}", path.display()));
                Arc::new(Store { conn: Mutex::new(conn), data_dir: data_dir.to_path_buf() })
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

    /// 与 [`Self::kv_get_raw`] 的区别：读库失败返回 Err，与「无键」（Ok(None)）可区分。
    /// 迁移等破坏性路径应使用本方法实现 fail-closed；展示路径可继续用 kv_get_raw。
    pub fn kv_get_raw_checked(&self, key: &str) -> Result<Option<String>, String> {
        self.with_conn(|c| {
            c.query_row("SELECT content FROM kv WHERE key = ?1", [key], |r| r.get::<_, String>(0))
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
        })
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

    /// KV 文档原子读-改-写（移植 main d04de4fc 审查修复）：读、改、写包在
    /// BEGIN IMMEDIATE 事务内完成（进程内连接 Mutex 互斥 + 跨进程写锁），消除
    /// 「kv_get → 内存改 → kv_set」两段式之间他方写入被旧快照整体覆盖丢键的
    /// 窗口——app_settings 调用点（settings_set patch 合并）与其他写入方
    /// （巡检/迁移路径）并发场景互踩丢键。
    /// 事务化（审查 #6）：原实现仅在连接锁内读-改-写，进程内互斥但跨进程不
    /// 原子——Web 服务与 `--task-run` CLI 并发 patch 同一 kv 键时后写者基于旧
    /// 读覆盖丢键；BEGIN IMMEDIATE 从读前即取写锁（WAL + busy_timeout 5s 支撑
    /// 双进程等待），把原子性提升到跨进程。
    /// 回调约束：只做内存改写与校验，不得做 IO 或再次取连接锁（闭包持有连接锁）。
    /// - 回调返回 Err：整体放弃写回（校验失败不落库，语义同原「先校验后写」）
    /// - 回调返回 Ok(false)：内容无变化，空转提交不写盘（保留「仅在值变化时写盘」语义）
    /// panic/失败清场（审查 #13）：f 闭包 panic 直接外抛会把开启的事务残留在共享
    /// 连接上——此后进程内全部查询永久报 cannot start a transaction within a
    /// transaction（旧无事务实现 panic 只毒化 Mutex，连接本身是干净的）。f 闭包包
    /// catch_unwind：panic 时先 ROLLBACK 再 resume_unwind（不吞 panic、仅保证连接
    /// 干净）；COMMIT 失败（提交期 SQLITE_BUSY 等）同样 rollback 后上抛。四种情形
    /// （f Err / 读写出错 / f panic / COMMIT 失败）均保证无事务残留；ROLLBACK
    /// 清场自身失败（连接损坏的极端情形）不吞——错误信息附加清场失败明细
    /// （panic 路径打 stderr），由调用方落日志暴露。
    /// 读到缺失/损坏存量值回退空对象（语义对齐调用点 !is_object 初始化分支）；
    /// 真实读错误（busy 超时/IO 故障/页损坏）显式上抛放弃写——fail-closed 同
    /// kv_get_raw_checked 的 R1 纪律：空集会触发整份覆写的路径绝不静默当空集，
    /// 否则 app_settings 其余键会被单 patch 对象覆盖丢失。
    pub fn kv_update<E: ToString>(
        &self,
        key: &str,
        f: impl FnOnce(&mut serde_json::Value) -> Result<bool, E>,
    ) -> Result<(), String> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute_batch("BEGIN IMMEDIATE;")
            .map_err(|e| format!("开启 kv[{key}] 写事务失败: {e}"))?;
        // 读-改-写全程入 catch_unwind（AssertUnwindSafe：f 与连接守卫非 UnwindSafe，
        // panic 时由下方统一 ROLLBACK 清场，安全性由事务原子性保证）
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let raw = conn
                .query_row("SELECT content FROM kv WHERE key = ?1", [key], |r| {
                    r.get::<_, String>(0)
                })
                .optional()
                .map_err(|e| format!("读取 kv[{key}] 失败，放弃改写以防覆盖丢失其他键: {e}"))?;
            let mut current: serde_json::Value = raw
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null);
            if !current.is_object() {
                current = serde_json::json!({});
            }
            if !f(&mut current).map_err(|e| e.to_string())? {
                return Ok(()); // 无变化：空转提交释放事务，不写盘
            }
            let content = serde_json::to_string(&current).map_err(|e| format!("序列化失败: {e}"))?;
            conn.execute(
                "INSERT INTO kv(key, content, updated_at) VALUES(?1, ?2, datetime('now','localtime'))
                 ON CONFLICT(key) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
                rusqlite::params![key, content],
            )
            .map_err(|e| format!("SQLite 操作失败: {e}"))?;
            Ok(())
        }));
        match outcome {
            Ok(Ok(())) => {
                if let Err(e) = conn.execute_batch("COMMIT;") {
                    // ROLLBACK 失败不吞（交叉审查修复）：残留事务会毒化共享连接，
                    // 须在错误信息中显式暴露（调用方落日志），不能静默成功返回
                    let rb = match conn.execute_batch("ROLLBACK;") {
                        Ok(_) => String::new(),
                        Err(re) => format!("（回滚清场亦失败，连接疑似损坏: {re}）"),
                    };
                    return Err(format!("提交 kv[{key}] 事务失败: {e}{rb}"));
                }
                Ok(())
            }
            Ok(Err(msg)) => {
                if let Err(re) = conn.execute_batch("ROLLBACK;") {
                    return Err(format!("{msg}（回滚清场失败，连接疑似损坏: {re}）"));
                }
                Err(msg)
            }
            Err(panic) => {
                // f panic：先清场再恢复展开（不吞 panic，仅保证共享连接无事务残留）
                if conn.execute_batch("ROLLBACK;").is_err() {
                    // panic 展开路径无法返回错误，清场失败打印到 stderr 兜底
                    eprintln!("[store] kv_update panic 回滚失败，连接疑似损坏: kv[{key}]");
                }
                std::panic::resume_unwind(panic);
            }
        }
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

    /// 整表替换 + kv 元数据写入（单事务）——审查修复（M7）：「行表 + kv 元数据」
    /// 原拆为 rows_replace 与 kv_set 两个独立事务提交，进程崩溃/并发竞争下可分叉
    /// （行表已换新而元数据未写或反之，api_keys 行表与鉴权开关分叉最敏感）。
    /// 本原语在同一事务内完成两者；kv_content 为最终入库字符串（与 kv_set_raw
    /// 同语义），Value 形态由调用方在事务外先序列化（对齐 rows_replace 时机）。
    pub fn rows_replace_with_kv(
        &self,
        table: &str,
        rows: &[(String, serde_json::Value)],
        kv_key: &str,
        kv_content: &str,
    ) -> Result<(), String> {
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
        let kv_text = kv_content.to_string();
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
            c.execute(
                "INSERT INTO kv(key, content, updated_at) VALUES(?1, ?2, datetime('now','localtime'))
                 ON CONFLICT(key) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
                rusqlite::params![kv_key, kv_text],
            )?;
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

    /// 单行删除（白名单表；buddy-ai 迁移/账号移除路径用）
    pub fn row_delete(&self, table: &str, pk: &str) -> Result<(), String> {
        if !schema::ROW_TABLES.contains(&table) {
            return Err(format!("非法表名: {table}"));
        }
        self.with_conn(|c| {
            c.execute(&format!("DELETE FROM [{table}] WHERE pk = ?1"), rusqlite::params![pk])?;
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

    /// kv_update 事务化回归（审查 #6/#13）：事务原子性 + panic/失败无残留。
    /// 连接干净 = 事务 rollback/commit 后 BEGIN IMMEDIATE 不再报
    /// cannot start a transaction within a transaction（残留毒化即此症状）
    #[test]
    fn kv_update_transaction_hygiene() {
        let dir = tmp_dir("kvupd");
        let s = db(&dir);
        s.kv_set("app", &json!({ "a": 1, "b": 2 })).unwrap();

        // ① f Err：放弃写回，存量不变且事务清场（后续 BEGIN IMMEDIATE 正常）
        let r: Result<(), String> = s.kv_update("app", |_| Err::<bool, _>("校验失败"));
        assert!(r.is_err());
        assert_eq!(
            s.kv_get_raw("app").as_deref(),
            Some(r#"{"a":1,"b":2}"#),
            "f Err 不得写回"
        );

        // ② f panic：panic 上抛（不吞）且事务清场——若 rollback 缺失，
        // 此后的 BEGIN IMMEDIATE 将永久报 cannot start a transaction
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            s.kv_update::<String>("app", |v| {
                v["a"] = json!(99);
                panic!("闭包故意 panic");
            })
        }))
        .is_err();
        assert!(panicked, "f panic 必须上抛而非被吞");
        assert_eq!(
            s.kv_get_raw("app").as_deref(),
            Some(r#"{"a":1,"b":2}"#),
            "panic 事务须 rollback，改写不得落盘"
        );

        // ③ 无变化（Ok(false)）：空转提交不写盘，值不变
        s.kv_update::<String>("app", |_| Ok(false)).unwrap();
        assert_eq!(s.kv_get_raw("app").as_deref(), Some(r#"{"a":1,"b":2}"#));

        // ④ 正常改写 + 连接复用（①②③之后 BEGIN IMMEDIATE 仍可用 = 无事务残留）
        s.kv_update::<String>("app", |v| {
            v["a"] = json!(42);
            Ok(true)
        })
        .unwrap();
        assert_eq!(s.kv_get_raw("app").as_deref(), Some(r#"{"a":42,"b":2}"#));

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
