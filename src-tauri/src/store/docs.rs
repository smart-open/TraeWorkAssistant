//! 各文件的类型化 load/save（迁移器与运行时共用的唯一存储出口）。
//!
//! 约定：函数签名以「原文件 struct」为边界，调用方（Phase 2/3 切换后）不再感知
//! 存储介质；行文档表的 data 列 = 对应条目的 serde JSON（struct 演进零成本）。
//! KV 文档无需类型化包装（调用方直接 kv_get/kv_set 自有 struct）。

// ── P6 流水迁出：WB 每日积分快照（原 kv workbuddy_credits_history）───────────

/// 读回 {snapshots:[{date,ts,total_balance,earned,accounts}]}（按日期升序；原文件形状兼容）
pub fn wb_credits_history_load(s: &Store) -> Value {
    let rows = s
        .with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT date, ts, total_balance, accounts, earned FROM wb_credits_history ORDER BY date",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    let accounts: String = r.get(3)?;
                    Ok(json!({
                        "date": r.get::<_, String>(0)?,
                        "ts": r.get::<_, i64>(1)?,
                        "total_balance": r.get::<_, f64>(2)?,
                        "accounts": serde_json::from_str::<Value>(&accounts)
                            .unwrap_or(Value::Array(vec![])),
                        "earned": r.get::<_, Option<f64>>(4)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    json!({ "snapshots": rows })
}

/// 同日覆盖 UPSERT（append_credits_snapshot 的落库等价物）
pub fn wb_credits_history_upsert(s: &Store, snap: &Value) -> Result<(), String> {
    let date = snap.get("date").and_then(Value::as_str).unwrap_or("").to_string();
    if date.is_empty() {
        return Err("快照缺 date 字段".into());
    }
    let ts = snap.get("ts").and_then(Value::as_i64).unwrap_or(0);
    let total = snap.get("total_balance").and_then(Value::as_f64).unwrap_or(0.0);
    let accounts = serde_json::to_string(
        snap.get("accounts").unwrap_or(&Value::Array(vec![])),
    )
    .map_err(|e| format!("序列化失败: {e}"))?;
    let earned = snap.get("earned").and_then(Value::as_f64);
    s.with_conn(move |c| {
        c.execute(
            "INSERT INTO wb_credits_history(date, ts, total_balance, accounts, earned) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(date) DO UPDATE SET ts = excluded.ts, total_balance = excluded.total_balance, accounts = excluded.accounts, earned = excluded.earned",
            rusqlite::params![date, ts, total, accounts, earned],
        )?;
        Ok(())
    })
}

/// 365 天滚动裁剪（原 cap 语义）
pub fn wb_credits_history_prune(s: &Store) -> Result<(), String> {
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(365))
        .format("%Y-%m-%d")
        .to_string();
    s.with_conn(|c| {
        c.execute("DELETE FROM wb_credits_history WHERE date < ?1", [cutoff.as_str()])?;
        Ok(())
    })
}

/// 全量导入（迁移器用）：{snapshots:[...]} → 逐日 upsert
pub fn wb_credits_history_save(s: &Store, root: &Value) -> Result<(), String> {
    let empty = Vec::new();
    let arr = root.get("snapshots").and_then(Value::as_array).unwrap_or(&empty);
    for snap in arr {
        wb_credits_history_upsert(s, snap)?;
    }
    wb_credits_history_prune(s)
}

// ── P6 流水迁出：消耗明细增量缓存（原 kv usage_history）──────────────────────

/// 读回 CacheFile 形状 Value：{fetched_at, accounts:{uid:{name,last_fetch_end_ts,daily:{date:stat},sessions:{date:[rows]}}}}
pub fn usage_history_load(s: &Store) -> Value {
    let mut out = serde_json::Map::new();
    let metas = s
        .with_conn(|c| {
            let mut stmt = c
                .prepare("SELECT uid, name, last_fetch_end_ts FROM usage_history_accounts")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    for (uid, name, ts) in metas {
        out.insert(
            uid.clone(),
            json!({"name": name, "last_fetch_end_ts": ts, "daily": {}, "sessions": {}}),
        );
    }
    let days = s
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT uid, date, data FROM usage_history_days")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    for (uid, date, data) in days {
        if let Some(acc) = out.get_mut(&uid).and_then(Value::as_object_mut) {
            if let Some(daily) = acc.get_mut("daily").and_then(Value::as_object_mut) {
                daily.insert(
                    date,
                    serde_json::from_str::<Value>(&data).unwrap_or(Value::Null),
                );
            }
        }
    }
    // 会话行回读（issue #82 会话明细 Tab 数据源；旧库无该表时返回空集，serde default 兜底）
    let sessions = s
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT uid, date, data FROM usage_history_sessions")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    for (uid, date, data) in sessions {
        if let Some(acc) = out.get_mut(&uid).and_then(Value::as_object_mut) {
            if let Some(list) = acc.get_mut("sessions").and_then(Value::as_object_mut) {
                list.insert(
                    date,
                    serde_json::from_str::<Value>(&data).unwrap_or(Value::Array(vec![])),
                );
            }
        }
    }
    let meta: Value = s.kv_get("usage_history_meta");
    json!({
        "fetched_at": meta.get("fetched_at").and_then(Value::as_i64),
        "accounts": out,
    })
}
pub fn usage_history_save(s: &Store, root: &Value) -> Result<(), String> {
    let empty_map = serde_json::Map::new();
    let accounts = root.get("accounts").and_then(Value::as_object).unwrap_or(&empty_map);
    let fetched_at = root.get("fetched_at").cloned().unwrap_or(Value::Null);
    // 序列化在事务外完成（闭包内仅做 rusqlite 操作）
    let mut metas: Vec<(String, String, Option<i64>)> = Vec::with_capacity(accounts.len());
    let mut day_rows: Vec<(String, String, String)> = Vec::new();
    let mut session_rows: Vec<(String, String, String)> = Vec::new();
    for (uid, acc) in accounts {
        metas.push((
            uid.clone(),
            acc.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            acc.get("last_fetch_end_ts").and_then(Value::as_i64),
        ));
        if let Some(daily) = acc.get("daily").and_then(Value::as_object) {
            for (date, stat) in daily {
                day_rows.push((
                    uid.clone(),
                    date.clone(),
                    serde_json::to_string(stat).map_err(|e| format!("序列化失败: {e}"))?,
                ));
            }
        }
        // 会话行明细同事务落库（issue #82；缺键 = 旧调用方/旧缓存，视为无会话行）
        if let Some(sessions) = acc.get("sessions").and_then(Value::as_object) {
            for (date, list) in sessions {
                session_rows.push((
                    uid.clone(),
                    date.clone(),
                    serde_json::to_string(list).map_err(|e| format!("序列化失败: {e}"))?,
                ));
            }
        }
    }
    s.with_conn(move |c| {
        c.execute_batch(
            "BEGIN; DELETE FROM usage_history_accounts; DELETE FROM usage_history_days; DELETE FROM usage_history_sessions;",
        )?;
        {
            let mut meta = c.prepare(
                "INSERT INTO usage_history_accounts(uid, name, last_fetch_end_ts, updated_at) VALUES(?1, ?2, ?3, datetime('now','localtime'))",
            )?;
            let mut day = c.prepare(
                "INSERT INTO usage_history_days(uid, date, data) VALUES(?1, ?2, ?3)",
            )?;
            let mut sess = c.prepare(
                "INSERT INTO usage_history_sessions(uid, date, data) VALUES(?1, ?2, ?3)",
            )?;
            for (uid, name, ts) in &metas {
                meta.execute(rusqlite::params![uid, name, ts])?;
            }
            for (uid, date, text) in &day_rows {
                day.execute(rusqlite::params![uid, date, text])?;
            }
            for (uid, date, text) in &session_rows {
                sess.execute(rusqlite::params![uid, date, text])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })?;
    // 365 天滚动裁剪（原实现无界增长，P6 修复）
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(365))
        .format("%Y-%m-%d")
        .to_string();
    s.with_conn(|c| {
        c.execute("DELETE FROM usage_history_days WHERE date < ?1", [cutoff.as_str()])?;
        c.execute("DELETE FROM usage_history_sessions WHERE date < ?1", [cutoff.as_str()])?;
        Ok(())
    })?;
    // fetched_at 回写 kv（原文件顶层字段）
    s.kv_set("usage_history_meta", &json!({ "fetched_at": fetched_at }))
}

// ── P6 流水迁出：会话粘性绑定（原 kv wb_sticky_sessions）─────────────────────

/// 读回 {version, bindings:[...]}（按 last_seen 升序 = 原内存序近似）
pub fn sticky_bindings_load(s: &Store) -> Value {
    let rows = s
        .with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT key, uid, conv_id, last_seen, explicit FROM sticky_bindings ORDER BY last_seen",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "key": r.get::<_, String>(0)?,
                        "uid": r.get::<_, String>(1)?,
                        "conv_id": r.get::<_, String>(2)?,
                        "last_seen": r.get::<_, i64>(3)?,
                        "explicit": r.get::<_, i64>(4)? != 0,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    json!({ "version": 1, "bindings": rows })
}

/// 整表替换（save 的落库等价物；过期项由调用方 evict 后传入）。
/// 多池并存时请用 [sticky_bindings_save_ns]（本函数仅遗留种子/单池场景使用）。
pub fn sticky_bindings_save(s: &Store, root: &Value) -> Result<(), String> {
    let empty = Vec::new();
    let arr = root.get("bindings").and_then(Value::as_array).unwrap_or(&empty);
    s.with_conn(|c| {
        c.execute_batch("BEGIN; DELETE FROM sticky_bindings;")?;
        {
            let mut stmt = c.prepare(
                "INSERT INTO sticky_bindings(key, uid, conv_id, last_seen, explicit, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, datetime('now','localtime'))",
            )?;
            for b in arr {
                stmt.execute(rusqlite::params![
                    b.get("key").and_then(Value::as_str).unwrap_or(""),
                    b.get("uid").and_then(Value::as_str).unwrap_or(""),
                    b.get("conv_id").and_then(Value::as_str).unwrap_or(""),
                    b.get("last_seen").and_then(Value::as_i64).unwrap_or(0),
                    (b.get("explicit").and_then(Value::as_bool).unwrap_or(false)) as i64,
                ])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

/// 命名空间范围替换（F-80-余 v2 多池共用 sticky_bindings 表）：
/// 仅删除并写入归属 `ns` 的键，其他命名空间（`other_ns`）的行原样保留——
/// 多池 StickyStore 并存时整表替换会互删对方运行期新增的绑定（审查 P1）。
/// 归属规则与 wb_sticky::owns_key 一致：ns 非空 = key 以 ns 前缀开头；
/// ns 空 = key 不以任何 other_ns 前缀开头（other_ns 为空时退化为整表删除，
/// 与 [sticky_bindings_save] 等价）。LIKE 模式由常量前缀经参数绑定构造，
/// ns 不得含 LIKE 通配符 %/_（当前登记命名空间均满足）。
pub fn sticky_bindings_save_ns(
    s: &Store,
    root: &Value,
    ns: &str,
    other_ns: &[&str],
) -> Result<(), String> {
    let empty = Vec::new();
    let arr = root.get("bindings").and_then(Value::as_array).unwrap_or(&empty);
    s.with_conn(|c| {
        c.execute_batch("BEGIN;")?;
        if ns.is_empty() && other_ns.is_empty() {
            c.execute_batch("DELETE FROM sticky_bindings;")?;
        } else if ns.is_empty() {
            // 无前缀键（WB/遗留）：删除不属于任何已登记命名空间的行。
            // 逐前缀 NOT LIKE 后 AND 连接（≡ NOT(任一前缀命中)）。曾误写
            // LIKE+AND 的恒假条件——单个 key 不可能同时命中两个前缀模式，
            // DELETE 恒 0 行：过期绑定永不清理，且残留行占主键使后续 INSERT
            // 整事务回滚，WB 池粘性绑定在首次落库成功后永久失效（审查 major）
            // 防御：过滤 other_ns 中的空串——空串的 LIKE '%' 模式会让 NOT LIKE 恒假
            // （DELETE 0 行 → 残留行占主键 → INSERT 整事务回滚，与已修复的旧 bug 同构）
            let other_ns: Vec<&str> = other_ns.iter().copied().filter(|p| !p.is_empty()).collect();
            if other_ns.is_empty() {
                // 无任何有效已登记命名空间 → 退化为整表删除（与上方全空分支等价）
                c.execute_batch("DELETE FROM sticky_bindings;")?;
            } else {
                let conds: Vec<String> =
                    other_ns.iter().map(|_| "key NOT LIKE ?".to_string()).collect();
                let sql = format!("DELETE FROM sticky_bindings WHERE {}", conds.join(" AND "));
                let mut stmt = c.prepare(&sql)?;
                let patterns: Vec<String> = other_ns.iter().map(|p| format!("{}%", p)).collect();
                stmt.execute(rusqlite::params_from_iter(patterns.iter()))?;
            }
        } else {
            let mut stmt = c.prepare("DELETE FROM sticky_bindings WHERE key LIKE ?")?;
            stmt.execute(rusqlite::params![format!("{}%", ns)])?;
        }
        {
            let mut stmt = c.prepare(
                "INSERT INTO sticky_bindings(key, uid, conv_id, last_seen, explicit, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, datetime('now','localtime'))",
            )?;
            for b in arr {
                stmt.execute(rusqlite::params![
                    b.get("key").and_then(Value::as_str).unwrap_or(""),
                    b.get("uid").and_then(Value::as_str).unwrap_or(""),
                    b.get("conv_id").and_then(Value::as_str).unwrap_or(""),
                    b.get("last_seen").and_then(Value::as_i64).unwrap_or(0),
                    (b.get("explicit").and_then(Value::as_bool).unwrap_or(false)) as i64,
                ])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AccountsFile, RemainingCreditsFile};

    fn tmp_store(tag: &str) -> (std::path::PathBuf, super::super::Arc<Store>) {
        let d = std::env::temp_dir().join(format!("twa_docs_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let s = super::super::db(&d);
        (d, s)
    }

    /// 审查修复（M7）：rows_replace_with_kv 单事务原语回环——行表替换与 kv 元数据
    /// 同事务生效（api_keys_save 最敏感：Key 行与鉴权开关不得分叉）。
    #[test]
    fn rows_replace_with_kv_roundtrip_via_api_keys_save() {
        use crate::api_server::api_keys::ApiKeyEntry;
        let (_dir, s) = tmp_store("rrkv");

        // 经真实调用点 api_keys_save 写入：行表 + 鉴权开关一次落库
        let entry: ApiKeyEntry =
            serde_json::from_value(json!({ "id": "k1", "name": "测试Key", "key": "sk-test" })).unwrap();
        api_keys_save(&s, &ApiKeysFile { keys: vec![entry], auth_disabled: true }).unwrap();
        let loaded = api_keys_load(&s);
        assert_eq!(loaded.keys.len(), 1, "Key 行随调用点写入");
        assert_eq!(loaded.keys[0].id, "k1");
        assert!(loaded.auth_disabled, "鉴权开关与 Key 行同事务写入，不得缺失");

        // 原语直测：整表替换 + kv 键覆盖，同事务内双双生效
        let rows = vec![("k2".to_string(), json!({ "id": "k2", "name": "n2", "key": "sk-2" }))];
        s.rows_replace_with_kv("api_keys", &rows, "probe_key", "v1").unwrap();
        let all = s.rows_all("api_keys").unwrap();
        assert_eq!(all.len(), 1, "整表替换生效");
        assert_eq!(all[0].0, "k2");
        assert_eq!(s.kv_get_raw("probe_key").as_deref(), Some("v1"));

        // 白名单守门与 rows_replace 同口径：非法表名直接拒绝
        assert!(s.rows_replace_with_kv("not_a_table", &rows, "probe_key", "v2").is_err());
    }

    /// qoder_sessions_compact 全链路（会话聚合修复后口径）：按 (date, uid, preview)
    /// 归并（随机 session_id 旧行同轮归并）、跨账号不串组、近期窗不动、
    /// 幂等（再跑 0 变更）、迟到行并入已合并组不双计、agg 前缀防御。
    #[test]
    fn qoder_sessions_compact_merges_idempotent_and_self_heals() {
        let (dir, s) = tmp_store("qcompact");
        let old = (chrono::Local::now().date_naive() - chrono::Duration::days(5))
            .format("%Y-%m-%d")
            .to_string();
        let recent = (chrono::Local::now().date_naive() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let ins = |s: &Store, rid: &str, sid: &str, uid: &str, date: &str, ts: i64,
                   model: &str, credits: f64, pt: i64, ct: i64, preview: &str| {
            // 测试辅助：插入失败应让测试立刻失败，而非静默（unused Result 警告修复）
            qoder_session_insert(s, rid, sid, uid, date, ts, model, credits, pt, ct, 10, true, true, preview, "rec-1")
                .unwrap();
        };
        // 修复前旧行：同轮同 preview、随机 sid 各异（u1@旧日，3 行）
        ins(&s, "r1", "aaaa", "u1", &old, 1000, "gpt", 1.0, 100, 10, "审查代码");
        ins(&s, "r2", "bbbb", "u1", &old, 2000, "gpt", 2.0, 200, 20, "审查代码");
        ins(&s, "r3", "cccc", "u1", &old, 3000, "pro", 3.0, 300, 30, "审查代码");
        // 跨账号同 preview：必须独立成组不串账
        ins(&s, "r4", "dddd", "u2", &old, 4000, "gpt", 4.0, 400, 40, "审查代码");
        // 同会话不同轮（preview 不同）：独立组
        ins(&s, "r5", "sess-1", "u1", &old, 5000, "gpt", 5.0, 500, 50, "第二轮");
        // 空 preview（失败早退行）：独立组
        ins(&s, "r6", "", "u1", &old, 6000, "gpt", 0.0, 0, 0, "");
        // 近期窗内（date >= cutoff）：不合并
        ins(&s, "r7", "sess-1", "u1", &recent, 7000, "gpt", 7.0, 700, 70, "今日轮");

        let (merged, deleted) = qoder_sessions_compact(&s, 3).unwrap();
        assert_eq!(merged, 4, "4 组：(u1,审查代码)/(u2,审查代码)/(u1,第二轮)/(u1,空)");
        assert_eq!(deleted, 6, "删 6 条原始请求行");

        let count = |s: &Store| -> i64 {
            s.with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM qoder_sessions", [], |r| r.get(0))
            })
            .unwrap()
        };
        assert_eq!(count(&s), 5, "4 聚合行 + 近期窗 1 行");

        let agg1: (f64, i64, i64, i64, String, String, String) = s
            .with_conn(|c| {
                c.query_row(
                    "SELECT credits, pt, ct, ts, model, session_id, preview FROM qoder_sessions WHERE request_id = ?1",
                    [format!("agg:{old}:u1:{}", hex_str("审查代码")).as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
                )
            })
            .unwrap();
        assert_eq!(
            agg1,
            (6.0, 600, 60, 3000, "pro".into(), "cccc".into(), "审查代码".into()),
            "SUM 与最新请求取值（sid/model 随最新行）"
        );

        // 幂等：已合并组（仅含 agg 行）再跑零变更
        let again = qoder_sessions_compact(&s, 3).unwrap();
        assert_eq!(again, (0, 0), "幂等：无新行不重写");
        assert_eq!(count(&s), 5);

        // 迟到行并入已合并组：下次运行 OR REPLACE 覆盖，合计 = 原聚合 + 迟到值
        ins(&s, "r8", "zzzz", "u1", &old, 3500, "gpt", 10.0, 1000, 100, "审查代码");
        let healed = qoder_sessions_compact(&s, 3).unwrap();
        assert_eq!(healed, (1, 1), "迟到行所在组重合并");
        let agg2: f64 = s
            .with_conn(|c| {
                c.query_row(
                    "SELECT credits FROM qoder_sessions WHERE request_id = ?1",
                    [format!("agg:{old}:u1:{}", hex_str("审查代码")).as_str()],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(agg2, 16.0, "迟到行并入不双计");

        // 防御（审查修复）：上游伪造 "agg:" 前缀 request_id 不得顶撞聚合行主键
        ins(&s, "agg:2020-01-01:u1:x", "S1", "u1", &old, 9000, "x", 99.0, 0, 0, "审查代码");
        let agg3: f64 = s
            .with_conn(|c| {
                c.query_row(
                    "SELECT credits FROM qoder_sessions WHERE request_id = ?1",
                    [format!("agg:{old}:u1:{}", hex_str("审查代码")).as_str()],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(agg3, 16.0, "伪造前缀 id 被换 uuid，聚合行未被覆盖");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 与 SQL hex(CAST(preview AS BLOB)) 对齐的测试辅助（UTF-8 字节 hex）
    fn hex_str(s: &str) -> String {
        s.bytes().map(|b| format!("{b:02X}")).collect()
    }

    #[test]
    fn accounts_roundtrip_preserves_order_and_optional_uid() {
        let (dir, s) = tmp_store("acc");
        let f = AccountsFile {
            accounts: vec![
                crate::models::RawAccount {
                    name: "a1".into(),
                    user_id: Some("u1".into()),
                    jwt: "j1".into(),
                    ..Default::default()
                },
                crate::models::RawAccount { name: "a2".into(), ..Default::default() },
                crate::models::RawAccount {
                    name: "a3".into(),
                    user_id: Some("u3".into()),
                    jwt: "j3".into(),
                    ..Default::default()
                },
            ],
        };
        accounts_save(&s, &f).unwrap();
        let got = accounts_load(&s);
        assert_eq!(got.accounts.len(), 3);
        assert_eq!(got.accounts[0].name, "a1");
        assert_eq!(got.accounts[1].user_id, None); // 占位账号（无 uid）可空
        assert_eq!(got.accounts[2].jwt, "j3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P7 验收：UNIQUE 防线——保存侧传入重复 user_id 时保序取首条（事务不失败），
    /// 占位账号（无 uid）不受影响可并存。
    #[test]
    fn accounts_save_dedups_duplicate_uids() {
        let (dir, s) = tmp_store("acc_dup");
        let f = AccountsFile {
            accounts: vec![
                crate::models::RawAccount { name: "a1".into(), user_id: Some("u1".into()), jwt: "j1".into(), ..Default::default() },
                crate::models::RawAccount { name: "a2".into(), user_id: Some("u1".into()), jwt: "j2".into(), ..Default::default() },
                crate::models::RawAccount { name: "p1".into(), ..Default::default() },
                crate::models::RawAccount { name: "p2".into(), ..Default::default() },
            ],
        };
        accounts_save(&s, &f).unwrap();
        let got = accounts_load(&s);
        assert_eq!(got.accounts.len(), 3, "重复 u1 去重，两个占位账号保留");
        assert_eq!(got.accounts[0].jwt, "j1");
        assert_eq!(got.accounts[1].name, "p1");
        assert_eq!(got.accounts[2].name, "p2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remaining_credits_parallel_maps_roundtrip() {
        let (dir, s) = tmp_store("rc");
        let mut f = RemainingCreditsFile::default();
        f.credits.insert("u1".into(), 123.45);
        f.expire_times.insert("u1".into(), 1790000000);
        f.general_expire_times.insert("u1".into(), 1791000000);
        f.general_expire_times.insert("u2".into(), 1792000000);
        f.work.insert("u1".into(), 50.0);
        f.total_limit.insert("u2".into(), 2000.0);
        // 包明细（到期日历包级口径）：含长期有效哨兵；u2 空数组（刷新成功但无可用包）
        f.packs.insert(
            "u1".into(),
            vec![crate::models::CreditPackDetail {
                kind: "通用".into(),
                source: "每日签到".into(),
                remaining: 88.5,
                total: 100.0,
                expire_time: 4102444800,
            }],
        );
        f.packs.insert("u2".into(), vec![]);
        f.updated_at = Some("2026-09-15T10:00:00".into());
        remaining_credits_save(&s, &f).unwrap();
        let got = remaining_credits_load(&s);
        assert_eq!(got.credits.get("u1"), Some(&123.45));
        assert_eq!(got.expire_times.get("u1"), Some(&1790000000));
        // 调度口径通用到期（issue #75）：混合口径与调度口径分离持久化
        assert_eq!(got.general_expire_times.get("u1"), Some(&1791000000));
        // u2 仅靠 general_expire_time 入并集，行不丢
        assert_eq!(got.general_expire_times.get("u2"), Some(&1792000000));
        // 缺键账号不误报（None 语义 = 无到期约束）
        assert!(got.general_expire_times.get("u3").is_none());
        assert_eq!(got.work.get("u1"), Some(&50.0));
        assert_eq!(got.total_limit.get("u2"), Some(&2000.0));
        assert!(got.general.is_empty());
        let packs = got.packs.get("u1").expect("u1 packs 应有值");
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].kind, "通用");
        assert_eq!(packs[0].source, "每日签到");
        assert_eq!(packs[0].remaining, 88.5);
        assert_eq!(packs[0].total, 100.0);
        assert_eq!(packs[0].expire_time, 4102444800);
        // 空数组也是有效状态（区分「无包」与「未刷新」）
        assert!(got.packs.get("u2").map_or(false, |v| v.is_empty()));
        assert!(got.packs.get("u3").is_none());
        assert_eq!(got.updated_at.as_deref(), Some("2026-09-15T10:00:00"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn api_usage_buckets_roundtrip() {
        use crate::api_server::usage::{DayStats, UsageBucket, UsageFile};
        let (dir, s) = tmp_store("usage");
        let mut f = UsageFile::default();
        let mut d1 = DayStats::default();
        d1.prompt_tokens = 11;
        d1.models.insert("m1".into(), Default::default());
        f.days.insert("2026-09-15".into(), d1);
        f.wb_days.insert("2026-09-15".into(), DayStats::default());
        api_usage_save(&s, &f).unwrap();
        let got = api_usage_load(&s);
        assert_eq!(got.days.len(), 1);
        assert_eq!(got.days["2026-09-15"].prompt_tokens, 11);
        assert_eq!(got.wb_days.len(), 1);
        assert!(got.custom_days.is_empty());
        let _ = bucket_name(UsageBucket::Custom); // 静默引用（load/save 均已覆盖）
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wb_token_store_upsert_and_version() {
        use serde_json::json;
        let (dir, s) = tmp_store("tokens");
        wb_token_store_upsert(&s, "a1", &json!({"access_token": "t1"})).unwrap();
        s.kv_set_raw("wb_tokens_meta", "1").unwrap();
        let store = wb_token_store_load(&s);
        assert_eq!(store["version"], 1);
        assert_eq!(store["tokens"]["a1"]["access_token"], "t1");
        // upsert 覆盖
        wb_token_store_upsert(&s, "a1", &json!({"access_token": "t2"})).unwrap();
        assert_eq!(wb_token_store_load(&s)["tokens"]["a1"]["access_token"], "t2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p6_flow_tables_roundtrip() {
        use serde_json::json;
        let (dir, s) = tmp_store("p6");
        // WB 每日积分快照：upsert 同日覆盖 + prune
        wb_credits_history_upsert(&s, &json!({"date":"2026-09-14","ts":1,"total_balance":10.0,"accounts":[{"user_id":"u","balance":10.0}]})).unwrap();
        wb_credits_history_upsert(&s, &json!({"date":"2026-09-15","ts":2,"total_balance":8.5,"accounts":[]})).unwrap();
        wb_credits_history_upsert(&s, &json!({"date":"2026-09-15","ts":3,"total_balance":8.0,"accounts":[]})).unwrap();
        let hist = wb_credits_history_load(&s);
        let snaps = hist["snapshots"].as_array().unwrap();
        assert_eq!(snaps.len(), 2, "同日覆盖");
        assert_eq!(snaps[1]["total_balance"], 8.0);
        wb_credits_history_prune(&s).unwrap();

        // usage_history：save/load 回环（含 365 天裁剪语义由 save 内执行）
        // sessions 回环（issue #82）：会话行必须与 daily 同事务落库、同读回——
        // 回归锚点：v3.7.7 曾只序列化 metas+day_rows 丢弃 sessions，重启后会话明细恒空
        let root = json!({
            "fetched_at": 12345,
            "accounts": {"u1": {"name": "甲", "last_fetch_end_ts": 999,
                                "daily": {"2026-09-15": {"date": "2026-09-15", "credits": 1.5, "sessions": 2}},
                                "sessions": {"2026-09-15": [
                                    {"session_id": "s1", "ts": 10, "model": "glm", "mode": "agent",
                                     "credits": 1.0, "cost_money": 0.0, "input_tokens": 5,
                                     "output_tokens": 6, "cache_read_tokens": 0, "cache_write_tokens": 0,
                                     "preview": "你好"},
                                    {"session_id": "s2", "ts": 20, "model": "kimi", "mode": "chat",
                                     "credits": 0.5, "cost_money": 0.0, "input_tokens": 1,
                                     "output_tokens": 2, "cache_read_tokens": 0, "cache_write_tokens": 0,
                                     "preview": "第二会话"}
                                ]}}}
        });
        usage_history_save(&s, &root).unwrap();
        let got = usage_history_load(&s);
        assert_eq!(got["fetched_at"], 12345);
        assert_eq!(got["accounts"]["u1"]["name"], "甲");
        assert_eq!(got["accounts"]["u1"]["daily"]["2026-09-15"]["credits"], 1.5);
        let sess = got["accounts"]["u1"]["sessions"]["2026-09-15"].as_array().unwrap();
        assert_eq!(sess.len(), 2, "会话行必须随 save 落库并随 load 回读");
        assert_eq!(sess[0]["session_id"], "s1");
        assert_eq!(sess[1]["model"], "kimi");
        // 同日重拉（增量替换语义在调用方）：save 覆盖后不叠加
        usage_history_save(&s, &root).unwrap();
        let got2 = usage_history_load(&s);
        assert_eq!(
            got2["accounts"]["u1"]["sessions"]["2026-09-15"].as_array().unwrap().len(),
            2,
            "同日重存整行替换，不得叠加"
        );
        // typed 回环：load 输出必须能反序列化回 CacheFile（usage_history.rs 消费形态）
        let cache: serde_json::Value = got;
        assert!(cache["accounts"]["u1"]["sessions"].is_object());

        // sticky bindings：save/load 回环
        sticky_bindings_save(&s, &json!({"bindings": [
            {"key": "cid:a", "uid": "u1", "conv_id": "c1", "last_seen": 10, "explicit": true}
        ]})).unwrap();
        let b = sticky_bindings_load(&s);
        assert_eq!(b["bindings"][0]["uid"], "u1");
        assert_eq!(b["bindings"][0]["explicit"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sticky_bindings_save_ns_no_prefix_scope_replaces_only_unowned_rows() {
        use serde_json::json;
        let (dir, s) = tmp_store("sticky_ns");
        // 三池共存：q:/t: 命名空间行 + WB 无前缀行（一条存活、一条待清理）
        let seed = json!({"bindings": [
            {"key": "q:a", "uid": "q1", "conv_id": "", "last_seen": 1, "explicit": false},
            {"key": "t:b", "uid": "t1", "conv_id": "", "last_seen": 1, "explicit": false},
            {"key": "cid:keep", "uid": "w1", "conv_id": "", "last_seen": 1, "explicit": false},
            {"key": "cid:stale", "uid": "w0", "conv_id": "", "last_seen": 1, "explicit": false}
        ]});
        sticky_bindings_save_ns(&s, &seed, "q:", &["t:", ""]).unwrap();
        // WB 池范围替换（ns=""，other_ns=q:/t:）：只允许动无前缀行——
        // 回归锚点：曾误写 LIKE+AND 恒假条件，DELETE 恒 0 行后 cid:stale
        // 残留占主键，本次 INSERT 整事务回滚（落库永久失效）
        let wb = json!({"bindings": [
            {"key": "cid:fresh", "uid": "w2", "conv_id": "", "last_seen": 2, "explicit": false}
        ]});
        sticky_bindings_save_ns(&s, &wb, "", &["q:", "t:"]).unwrap();
        let root = sticky_bindings_load(&s);
        let keys: Vec<&str> = root["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b["key"].as_str())
            .collect();
        assert!(keys.contains(&"q:a"), "q: 命名空间行必须原样保留");
        assert!(keys.contains(&"t:b"), "t: 命名空间行必须原样保留");
        assert!(keys.contains(&"cid:fresh"), "WB 新绑定必须写入");
        assert!(!keys.contains(&"cid:stale"), "无前缀旧行必须被范围替换删除");
        assert!(!keys.contains(&"cid:keep"), "ns=\"\" 为整命名空间替换，旧无前缀行不保留");
        // 二次保存仍成功（旧 bug 下主键冲突会回滚）
        sticky_bindings_save_ns(&s, &wb, "", &["q:", "t:"]).unwrap();
        assert_eq!(sticky_bindings_load(&s)["bindings"].as_array().unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn groups_and_cooldowns_roundtrip() {
        let (dir, s) = tmp_store("groups");
        let f = crate::models::GroupsFile {
            groups: vec![crate::models::Group {
                id: "g1".into(),
                name: "主号".into(),
                color: "#f00".into(),
                order: 1,
            }],
            membership: [("u1".to_string(), "g1".to_string())].into_iter().collect(),
        };
        groups_save(&s, &f).unwrap();
        let got = groups_load(&s);
        assert_eq!(got.groups.len(), 1);
        assert_eq!(got.groups[0].name, "主号");
        assert_eq!(got.membership.get("u1").map(String::as_str), Some("g1"));

        let mut cd = crate::models::AccountCooldownsFile::default();
        cd.cooldowns.insert(
            "u1".into(),
            crate::models::CooldownEntry {
                error_type: "SessionDead".into(),
                until: 999,
                reason: "x".into(),
                error_count: 2,
            },
        );
        account_cooldowns_save(&s, &cd).unwrap();
        let got = account_cooldowns_load(&s);
        assert_eq!(got.cooldowns["u1"].error_type, "SessionDead");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

use serde_json::{json, Value};

use super::Store;
use crate::models::{
    AccountCooldownsFile, AccountsFile, CooldownEntry, CreditRecord, CreditsDailyFile,
    CreditsFile, DeviceEntry, DeviceMap, GroupsFile, RemainingCreditsFile, RawAccount,
};
use crate::api_server::api_keys::ApiKeysFile;
use crate::api_server::custom_models::CustomModelsFile;
use crate::api_server::usage::{DayStats, UsageBucket, UsageFile};

// ── Trae 账号（checkin_accounts.json → accounts 表，seq 保序）────────────────

/// accounts 表 `user_id UNIQUE` 防线：非空 uid 重复时保序取首条（SQLite UNIQUE
/// 允许多个 NULL/空 uid 占位账号并存）。迁移导入与运行期保存共用——否则历史
/// JSON 中遗留的重复 uid 会让整个保存事务失败、启动迁移永久卡住（账号「消失」）。
/// 返回 (去重后行, 丢弃数)；丢弃数供调用方写日志（静默丢账号不可观测）。
fn dedup_account_rows(rows: Vec<(Option<String>, String)>) -> (Vec<(Option<String>, String)>, usize) {
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut dropped = 0usize;
    let out = rows
        .into_iter()
        .filter(|(uid, _)| match uid {
            Some(u) if !u.is_empty() => {
                if seen.insert(u.clone()) {
                    true
                } else {
                    dropped += 1;
                    false
                }
            }
            _ => true,
        })
        .collect();
    (out, dropped)
}

/// dedup 丢弃账号的可观测性：触发即写 app_log（账号属核心数据，静默丢弃须留痕）
fn log_dedup_dropped(s: &Store, dropped: usize, scope: &str) {
    if dropped > 0 {
        crate::fs_utils::app_log(
            &s.data_dir,
            &format!("accounts 保存去重（{scope}）：丢弃 {dropped} 条重复 user_id 账号（保序取首条）"),
        );
    }
}

pub fn accounts_load(s: &Store) -> AccountsFile {
    let rows: Vec<(Option<String>, String)> = s
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT user_id, data FROM accounts ORDER BY seq")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    let accounts = rows
        .into_iter()
        .filter_map(|(_, data)| serde_json::from_str::<RawAccount>(&data).ok())
        .collect();
    AccountsFile { accounts }
}

pub fn accounts_save(s: &Store, f: &AccountsFile) -> Result<(), String> {
    // 序列化在事务外完成；UNIQUE 防线去重（保序取首条）
    let mut rows: Vec<(Option<String>, String)> = Vec::with_capacity(f.accounts.len());
    for a in &f.accounts {
        rows.push((
            a.user_id.clone(),
            serde_json::to_string(a).map_err(|e| format!("序列化失败: {e}"))?,
        ));
    }
    let rows = dedup_account_rows(rows);
    log_dedup_dropped(s, rows.1, "accounts_save");
    let rows = rows.0;
    s.with_conn(move |c| {
        c.execute_batch("BEGIN; DELETE FROM accounts;")?;
        {
            let mut stmt = c.prepare("INSERT INTO accounts(user_id, data) VALUES(?1, ?2)")?;
            for (uid, data) in &rows {
                stmt.execute(rusqlite::params![uid, data])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

/// 以 Value 形态读 accounts 表（`{"accounts":[...]}`；保留 struct 外字段——
/// device_proxy 捕获路径会写入 refresh_token_updated_at 等扩展字段，typed roundtrip 会丢）
pub fn accounts_load_raw(s: &Store) -> Value {
    let arr: Vec<Value> = s
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT data FROM accounts ORDER BY seq")?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .filter_map(|d| serde_json::from_str(&d).ok())
                .collect())
        })
        .unwrap_or_default();
    serde_json::json!({ "accounts": arr })
}

/// 以 Value 形态整表写 accounts（pk = user_id/UserID，数字 uid 折算字符串；
/// 与 device_proxy 原始 JSON 处理语义对齐）
pub fn accounts_save_raw(s: &Store, root: &Value) -> Result<(), String> {
    let empty = Vec::new();
    let arr = root.get("accounts").and_then(Value::as_array).unwrap_or(&empty);
    let mut rows: Vec<(Option<String>, String)> = Vec::with_capacity(arr.len());
    for a in arr {
        let uid = a
            .get("user_id")
            .or_else(|| a.get("UserID"))
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())
            })
            .filter(|s| !s.is_empty());
        rows.push((
            uid,
            serde_json::to_string(a).map_err(|e| format!("序列化失败: {e}"))?,
        ));
    }
    let rows = dedup_account_rows(rows);
    log_dedup_dropped(s, rows.1, "accounts_save_raw");
    let rows = rows.0;
    s.with_conn(move |c| {
        c.execute_batch("BEGIN; DELETE FROM accounts;")?;
        {
            let mut stmt = c.prepare("INSERT INTO accounts(user_id, data) VALUES(?1, ?2)")?;
            for (uid, data) in &rows {
                stmt.execute(rusqlite::params![uid, data])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

// ── 设备映射（device_map.json → device_map 表）───────────────────────────────

/// device_map 表级读改写互斥（审查 C3）：签到 get_device_for 写回分支与 OAuth
/// 登录回写 writeback_oauth_device 并发时，整表 load→save 互相覆盖丢条目
///（指纹漂移）。两个运行期写点持锁覆盖 load→save 全程；只读点不持锁。
/// 叶子锁：持锁期间不得再获取其他锁。
pub static DEVICE_MAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn device_map_load(s: &Store) -> DeviceMap {
    s.rows_all("device_map")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(uid, data)| serde_json::from_value::<DeviceEntry>(data).ok().map(|e| (uid, e)))
        .collect()
}

pub fn device_map_save(s: &Store, map: &DeviceMap) -> Result<(), String> {
    let rows: Vec<(String, Value)> = map
        .iter()
        .map(|(uid, e)| serde_json::to_value(e).map(|v| (uid.clone(), v)))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("序列化失败: {e}"))?;
    s.rows_replace("device_map", &rows)
}

// ── 分组（groups.json → groups + group_members 表）───────────────────────────

pub fn groups_load(s: &Store) -> GroupsFile {
    let groups = s
        .rows_all("groups")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, data)| serde_json::from_value(data).ok())
        .collect();
    let membership = s
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT uid, group_id FROM group_members")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default()
        .into_iter()
        .collect();
    GroupsFile { groups, membership }
}

/// 组织定义类行文档表（groups / group_members / tags / account_tags）统一写互斥
/// （2026-10-12 审查 P4，最高优先级）：上述表均为「整删全写 / 读-改-写」模型，
/// 多处入口并发 RMW 互相覆盖会丢更新（丢新分组 / 复活已删分组 / 丢标签绑定）。
/// store 层无 AppState 可挂，用进程级全局锁；全部写入口在本层持锁（ choke point
/// 覆盖既有与未来全部调用方，调用处无需各自加锁）。
/// 锁序强制单向嵌套「池锁 → group_defs_lock → 本锁」：本层不感知上层锁、
/// 持本锁期间绝不回调上层，故不可能出现反向嵌套死锁。
pub(crate) fn org_defs_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn groups_save(s: &Store, f: &GroupsFile) -> Result<(), String> {
    let _org_guard = org_defs_lock();
    // 序列化在事务外完成
    let mut group_rows: Vec<(String, String)> = Vec::with_capacity(f.groups.len());
    for g in &f.groups {
        group_rows.push((g.id.clone(), serde_json::to_string(g).map_err(|e| format!("序列化失败: {e}"))?));
    }
    let member_rows: Vec<(String, String)> =
        f.membership.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    s.with_conn(move |c| {
        c.execute_batch("BEGIN; DELETE FROM groups; DELETE FROM group_members;")?;
        {
            let mut g = c.prepare("INSERT INTO groups(pk, data, updated_at) VALUES(?1, ?2, datetime('now','localtime'))")?;
            for (pk, data) in &group_rows {
                g.execute(rusqlite::params![pk, data])?;
            }
            let mut m = c.prepare("INSERT INTO group_members(uid, group_id, updated_at) VALUES(?1, ?2, datetime('now','localtime'))")?;
            for (uid, gid) in &member_rows {
                m.execute(rusqlite::params![uid, gid])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

// ── 共享分组定义（groups 表仅定义行；跨应用统一存储出口）──────────────────────

/// 读统一分组定义（仅 groups 表定义行，不附带 membership）。
/// 迁移器、三池 list 命令与共享分组命令共用。
pub fn group_defs_load(s: &Store) -> Vec<crate::models::Group> {
    s.rows_all("groups")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, data)| serde_json::from_value(data).ok())
        .collect()
}

/// 全删全写定义行（不触碰 group_members 表——成员关系属各池机制，D1）。
/// 持 org_defs_lock：与并发 groups_save/tags_save/account_tags_* 写入口互斥
pub fn group_defs_save(s: &Store, defs: &[crate::models::Group]) -> Result<(), String> {
    let _org_guard = org_defs_lock();
    group_defs_save_unlocked(s, defs)
}

/// 不加锁版本：仅供整段已持 org_defs_lock 的调用方（启动迁移 migrate_unified_groups
/// 全程持锁）使用，防同线程重入 `Mutex` 死锁
pub(crate) fn group_defs_save_unlocked(s: &Store, defs: &[crate::models::Group]) -> Result<(), String> {
    let rows: Vec<(String, String)> = defs
        .iter()
        .map(|g| serde_json::to_string(g).map(|text| (g.id.clone(), text)))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("序列化失败: {e}"))?;
    s.with_conn(move |c| {
        c.execute_batch("BEGIN; DELETE FROM groups;")?;
        {
            let mut stmt = c.prepare(
                "INSERT INTO groups(pk, data, updated_at) VALUES(?1, ?2, datetime('now','localtime'))",
            )?;
            for (pk, data) in &rows {
                stmt.execute(rusqlite::params![pk, data])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

// ── 共享标签（tags 定义行 + account_tags 绑定行，跨池一张表）─────────────────

pub fn tags_load(s: &Store) -> Vec<crate::models::Tag> {
    s.rows_all("tags")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, data)| serde_json::from_value(data).ok())
        .collect()
}

/// 持 org_defs_lock：tags 全删全写与并发绑定写入口互斥
pub fn tags_save(s: &Store, defs: &[crate::models::Tag]) -> Result<(), String> {
    let _org_guard = org_defs_lock();
    let rows: Vec<(String, String)> = defs
        .iter()
        .map(|t| serde_json::to_string(t).map(|text| (t.id.clone(), text)))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("序列化失败: {e}"))?;
    s.with_conn(move |c| {
        c.execute_batch("BEGIN; DELETE FROM tags;")?;
        {
            let mut stmt = c.prepare(
                "INSERT INTO tags(pk, data, updated_at) VALUES(?1, ?2, datetime('now','localtime'))",
            )?;
            for (pk, data) in &rows {
                stmt.execute(rusqlite::params![pk, data])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

/// 一次拉全量 uid → tag_ids 映射（各池账号列表组装联表用，避免 N+1）
pub fn account_tags_map(s: &Store) -> std::collections::HashMap<String, Vec<String>> {
    s.rows_all("account_tags")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(uid, data)| {
            let ids: Vec<String> = serde_json::from_value(data).ok()?;
            Some((uid, ids))
        })
        .collect()
}

/// 全量覆盖式设置某账号的标签绑定（校验 tag_id 存在由命令层负责）。持 org_defs_lock
pub fn account_tags_set(s: &Store, uid: &str, tag_ids: &[String]) -> Result<(), String> {
    let _org_guard = org_defs_lock();
    let data = serde_json::to_string(&tag_ids).map_err(|e| format!("序列化失败: {e}"))?;
    s.with_conn(move |c| {
        c.execute(
            "INSERT INTO account_tags(pk, data, updated_at) VALUES(?1, ?2, datetime('now','localtime'))
             ON CONFLICT(pk) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
            rusqlite::params![uid, data],
        )?;
        Ok(())
    })
}

/// 删除某账号的全部标签绑定（账号删除路径调用，防幽灵绑定污染
/// 标签计数 / 组标签展开 / Key 覆盖预览）。持 org_defs_lock
pub fn account_tags_remove_account(s: &Store, uid: &str) -> Result<(), String> {
    let _org_guard = org_defs_lock();
    s.with_conn(|c| {
        c.execute("DELETE FROM account_tags WHERE pk = ?1", [uid])?;
        Ok(())
    })
}

/// 删除某标签的全部绑定（标签定义删除时清理引用）。
/// 持 org_defs_lock：整表扫描读-改-写，与并发 account_tags_set 互斥防丢改
pub fn account_tags_remove_tag(s: &Store, tag_id: &str) -> Result<usize, String> {
    let _org_guard = org_defs_lock();
    let map = account_tags_map(s);
    let mut affected = 0usize;
    for (uid, ids) in map {
        if !ids.iter().any(|t| t == tag_id) {
            continue;
        }
        affected += 1;
        let remaining: Vec<&String> = ids.iter().filter(|t| t.as_str() != tag_id).collect();
        if remaining.is_empty() {
            s.with_conn(|c| c.execute("DELETE FROM account_tags WHERE pk = ?1", [&uid]))?;
        } else {
            let data = serde_json::to_string(&remaining).map_err(|e| format!("序列化失败: {e}"))?;
            s.with_conn(move |c| {
                c.execute(
                    "UPDATE account_tags SET data = ?1, updated_at = datetime('now','localtime') WHERE pk = ?2",
                    rusqlite::params![data, uid],
                )?;
                Ok(())
            })?;
        }
    }
    Ok(affected)
}

// ── 剩余积分缓存（remaining_credits.json → remaining_credits 表，8 平行 map 合并行）──

pub fn remaining_credits_load(s: &Store) -> RemainingCreditsFile {
    let mut f = RemainingCreditsFile::default();
    for (uid, data) in s.rows_all("remaining_credits").unwrap_or_default() {
        let get_f = |k: &str| data.get(k).and_then(Value::as_f64);
        let get_i = |k: &str| data.get(k).and_then(Value::as_i64);
        if let Some(v) = get_f("credits") {
            f.credits.insert(uid.clone(), v);
        }
        if let Some(v) = get_i("expire_time") {
            f.expire_times.insert(uid.clone(), v);
        }
        // 通用积分到期（API 网关调度口径，issue #28）；缺键 = 无到期约束/未刷新
        if let Some(v) = get_i("general_expire_time") {
            f.general_expire_times.insert(uid.clone(), v);
        }
        if let Some(v) = get_f("general") {
            f.general.insert(uid.clone(), v);
        }
        if let Some(v) = get_f("work") {
            f.work.insert(uid.clone(), v);
        }
        if let Some(v) = get_f("total_limit") {
            f.total_limit.insert(uid.clone(), v);
        }
        if let Some(v) = get_i("membership_expire") {
            f.membership_expire.insert(uid.clone(), v);
        }
        if let Some(v) = get_i("membership_next_billing") {
            f.membership_next_billing.insert(uid.clone(), v);
        }
        // 积分包明细（按包口径到期计算用）；解析失败的条目跳过不阻断
        if let Some(arr) = data.get("packs").and_then(Value::as_array) {
            let packs: Vec<crate::models::CreditPackDetail> = arr
                .iter()
                .filter_map(|p| serde_json::from_value(p.clone()).ok())
                .collect();
            f.packs.insert(uid.clone(), packs);
        }
    }
    f.updated_at = s.kv_get::<Option<String>>("remaining_credits_updated_at");
    f
}

pub fn remaining_credits_save(s: &Store, f: &RemainingCreditsFile) -> Result<(), String> {
    // 并集 of 8 map 的键；每账号一行，仅写入存在的字段（缺省语义由 load 侧空值兜底）
    let maps_f: [&std::collections::HashMap<String, f64>; 4] =
        [&f.credits, &f.general, &f.work, &f.total_limit];
    let maps_i: [&std::collections::HashMap<String, i64>; 4] = [
        &f.expire_times,
        &f.general_expire_times,
        &f.membership_expire,
        &f.membership_next_billing,
    ];
    let mut uids: Vec<String> = Vec::new();
    for m in maps_f.iter() {
        for k in m.keys() {
            if !uids.iter().any(|u| u == k) {
                uids.push(k.clone());
            }
        }
    }
    for m in maps_i.iter() {
        for k in m.keys() {
            if !uids.iter().any(|u| u == k) {
                uids.push(k.clone());
            }
        }
    }
    for k in f.packs.keys() {
        if !uids.iter().any(|u| u == k) {
            uids.push(k.clone());
        }
    }
    let rows: Vec<(String, Value)> = uids
        .into_iter()
        .map(|uid| {
            let mut obj = serde_json::Map::new();
            let ins_f = |obj: &mut serde_json::Map<String, Value>, k: &str, m: &std::collections::HashMap<String, f64>| {
                if let Some(v) = m.get(&uid) {
                    obj.insert(k.into(), json!(v));
                }
            };
            let ins_i = |obj: &mut serde_json::Map<String, Value>, k: &str, m: &std::collections::HashMap<String, i64>| {
                if let Some(v) = m.get(&uid) {
                    obj.insert(k.into(), json!(v));
                }
            };
            ins_f(&mut obj, "credits", &f.credits);
            ins_i(&mut obj, "expire_time", &f.expire_times);
            ins_i(&mut obj, "general_expire_time", &f.general_expire_times);
            ins_f(&mut obj, "general", &f.general);
            ins_f(&mut obj, "work", &f.work);
            ins_f(&mut obj, "total_limit", &f.total_limit);
            ins_i(&mut obj, "membership_expire", &f.membership_expire);
            ins_i(&mut obj, "membership_next_billing", &f.membership_next_billing);
            // 积分包明细：有值才写（含空数组，空 = 刷新成功但无可用包）；老缓存无键 → load 侧无记录回退账号级口径
            if let Some(v) = f.packs.get(&uid) {
                obj.insert("packs".into(), json!(v));
            }
            (uid, Value::Object(obj))
        })
        .collect();
    // 审查修复（M7）：行表与 updated_at 元数据同事务写入，消除崩溃/竞争分叉窗口
    let kv = serde_json::to_string(&f.updated_at).map_err(|e| format!("序列化失败: {e}"))?;
    s.rows_replace_with_kv("remaining_credits", &rows, "remaining_credits_updated_at", &kv)
}

// ── 冷却状态（account_cooldowns.json → account_cooldowns 表）─────────────────

/// account_cooldowns 表级读改写互斥（审查 C4）：调度签到 save_cooldown 与手动
/// 清理命令（cooldown_clear 等）/解冻回写并发时，整表 load→save 互相覆盖丢冷却
/// 条目（已冷却账号被重新探测、对上游连打）。全部读改写点持锁；只读点不持锁。
/// 叶子锁：持锁期间不得再获取其他锁。
pub static ACCOUNT_COOLDOWNS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn account_cooldowns_load(s: &Store) -> AccountCooldownsFile {
    let cooldowns = s
        .rows_all("account_cooldowns")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(uid, data)| serde_json::from_value::<CooldownEntry>(data).ok().map(|e| (uid, e)))
        .collect();
    let updated_at = s.kv_get::<Option<String>>("account_cooldowns_updated_at");
    AccountCooldownsFile { cooldowns, updated_at }
}

pub fn account_cooldowns_save(s: &Store, f: &AccountCooldownsFile) -> Result<(), String> {
    let rows: Vec<(String, Value)> = f
        .cooldowns
        .iter()
        .map(|(uid, e)| serde_json::to_value(e).map(|v| (uid.clone(), v)))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("序列化失败: {e}"))?;
    // 审查修复（M7）：行表与 updated_at 元数据同事务写入，消除崩溃/竞争分叉窗口
    let kv = serde_json::to_string(&f.updated_at).map_err(|e| format!("序列化失败: {e}"))?;
    s.rows_replace_with_kv("account_cooldowns", &rows, "account_cooldowns_updated_at", &kv)
}

// ── 套餐身份（pay_status.json → pay_status 表）───────────────────────────────

pub fn pay_status_load(s: &Store) -> crate::commands::trae_apps::PayStatusFile {
    let statuses = s
        .rows_all("pay_status")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(uid, data)| {
            serde_json::from_value::<crate::commands::trae_apps::PayStatusEntry>(data)
                .ok()
                .map(|e| (uid, e))
        })
        .collect();
    let updated_at = s.kv_get::<Option<String>>("pay_status_updated_at");
    crate::commands::trae_apps::PayStatusFile { statuses, updated_at }
}

pub fn pay_status_save(s: &Store, f: &crate::commands::trae_apps::PayStatusFile) -> Result<(), String> {
    let rows: Vec<(String, Value)> = f
        .statuses
        .iter()
        .map(|(uid, e)| serde_json::to_value(e).map(|v| (uid.clone(), v)))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("序列化失败: {e}"))?;
    // 审查修复（M7）：行表与 updated_at 元数据同事务写入，消除崩溃/竞争分叉窗口
    let kv = serde_json::to_string(&f.updated_at).map_err(|e| format!("序列化失败: {e}"))?;
    s.rows_replace_with_kv("pay_status", &rows, "pay_status_updated_at", &kv)
}

// ── API Key（api_keys.json → api_keys 表 + kv 鉴权开关）──────────────────────

pub fn api_keys_load(s: &Store) -> ApiKeysFile {
    let keys = s
        .rows_all("api_keys")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, data)| serde_json::from_value(data).ok())
        .collect();
    ApiKeysFile {
        keys,
        auth_disabled: s.kv_get_raw("api_keys_auth_disabled").map(|v| v == "true").unwrap_or(false),
    }
}

pub fn api_keys_save(s: &Store, f: &ApiKeysFile) -> Result<(), String> {
    let rows: Vec<(String, Value)> = f
        .keys
        .iter()
        .map(|k| serde_json::to_value(k).map(|v| (k.id.clone(), v)))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("序列化失败: {e}"))?;
    // 审查修复（M7）：行表与鉴权开关同事务写入（最敏感分叉点——崩溃后可能
    // 出现「Key 已换而开关未改」的陈旧鉴权状态）
    s.rows_replace_with_kv(
        "api_keys",
        &rows,
        "api_keys_auth_disabled",
        if f.auth_disabled { "true" } else { "false" },
    )
}

// ── 自定义模型（custom_models.json → custom_models 表）───────────────────────

pub fn custom_models_load(s: &Store) -> CustomModelsFile {
    let models = s
        .rows_all("custom_models")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, data)| serde_json::from_value(data).ok())
        .collect();
    CustomModelsFile {
        models,
        updated_at: s.kv_get_raw("custom_models_updated_at").and_then(|v| v.parse().ok()).unwrap_or(0),
    }
}

pub fn custom_models_save(s: &Store, f: &CustomModelsFile) -> Result<(), String> {
    let rows: Vec<(String, Value)> = f
        .models
        .iter()
        .map(|m| serde_json::to_value(m).map(|v| (m.id.clone(), v)))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("序列化失败: {e}"))?;
    // 审查修复（M7）：行表与 updated_at 元数据同事务写入，消除崩溃/竞争分叉窗口
    s.rows_replace_with_kv("custom_models", &rows, "custom_models_updated_at", &f.updated_at.to_string())
}

// ── 豆包账号池（doubao_accounts.json → doubao_accounts 表）───────────────────

pub fn doubao_pool_load(s: &Store) -> crate::commands::doubao::DoubaoAccountPool {
    let accounts = s
        .rows_all("doubao_accounts")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, data)| serde_json::from_value::<crate::commands::doubao::DoubaoAccount>(data).ok())
        .collect();
    let meta: Value = s.kv_get("doubao_pool_meta");
    crate::commands::doubao::DoubaoAccountPool {
        accounts,
        last_keepalive_at: meta.get("last_keepalive_at").and_then(Value::as_str).map(String::from),
    }
}

pub fn doubao_pool_save(s: &Store, pool: &crate::commands::doubao::DoubaoAccountPool) -> Result<(), String> {
    let rows: Vec<(String, Value)> = pool
        .accounts
        .iter()
        .map(|a| serde_json::to_value(a).map(|v| (a.user_id.clone(), v)))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("序列化失败: {e}"))?;
    // 审查修复（M7）：行表与 keepalive 元数据同事务写入，消除崩溃/竞争分叉窗口
    let kv = serde_json::to_string(&json!({ "last_keepalive_at": pool.last_keepalive_at }))
        .map_err(|e| format!("序列化失败: {e}"))?;
    s.rows_replace_with_kv("doubao_accounts", &rows, "doubao_pool_meta", &kv)
}

// ── WorkBuddy 账号池（workbuddy_accounts.json → wb_accounts 表，Value 语义）──

/// 读整池（结构 {accounts: [...]}；账号对象原样保真）
pub fn wb_pool_load(s: &Store) -> Value {
    let accounts: Vec<Value> = s
        .rows_all("wb_accounts")
        .unwrap_or_default()
        .into_iter()
        .map(|(_, data)| data)
        .collect();
    json!({ "accounts": accounts })
}

/// 写整池（对齐原「整文件写回」语义；pk = 账号 id，缺 id 用序号占位）
pub fn wb_pool_save(s: &Store, pool: &Value) -> Result<(), String> {
    let empty = Vec::new();
    let arr = pool.get("accounts").and_then(Value::as_array).unwrap_or(&empty);
    let rows: Vec<(String, Value)> = arr
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let id = a.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
            (id.map(String::from).unwrap_or_else(|| format!("__idx{i}")), a.clone())
        })
        .collect();
    s.rows_replace("wb_accounts", &rows)
}

// ── WorkBuddy token store（workbuddy_token_store.json → wb_tokens 表）────────

/// 读整库（结构 {version, tokens: {id: rec}}；wb_upstream 死引用统一收敛到此）。
/// 损坏行（data 非 JSON 对象，如手工编辑产生的 NULL）过滤丢弃，不混入消费方。
pub fn wb_token_store_load(s: &Store) -> Value {
    let tokens: serde_json::Map<String, Value> = s
        .rows_all("wb_tokens")
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, v)| v.is_object())
        .collect();
    let version = s.kv_get_raw("wb_tokens_meta").and_then(|v| v.parse::<i64>().ok());
    let mut root = serde_json::Map::new();
    root.insert("tokens".into(), Value::Object(tokens));
    if let Some(v) = version {
        root.insert("version".into(), json!(v));
    }
    Value::Object(root)
}

/// 写整库（version 闸门由调用方维持，此处整表替换）
pub fn wb_token_store_save(s: &Store, store_val: &Value) -> Result<(), String> {
    let empty = serde_json::Map::new();
    let tokens = store_val.get("tokens").and_then(Value::as_object).unwrap_or(&empty);
    let rows: Vec<(String, Value)> =
        tokens.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    s.rows_replace("wb_tokens", &rows)?;
    if let Some(v) = store_val.get("version").and_then(Value::as_i64) {
        s.kv_set_raw("wb_tokens_meta", &v.to_string())?;
    }
    Ok(())
}

/// 单账号 UPSERT（对齐 save_token_store 的 merge 增量写语义）
pub fn wb_token_store_upsert(s: &Store, id: &str, rec: &Value) -> Result<(), String> {
    s.row_upsert("wb_tokens", id, rec)
}

/// 仅写 version kv 元数据（供凭证收敛后的单行 upsert 路径维持版本闸门）
pub fn wb_token_store_save_version(s: &Store, version: i64) -> Result<(), String> {
    s.kv_set_raw("wb_tokens_meta", &version.to_string())
}

// ── 行文档表泛化原语（token store 通用化：wb / buddy-ai 双命名空间复用）──────

/// 任意 token store 表读取（结构 {version, tokens: {id: rec}}；损坏行过滤）
pub fn token_store_load(table: &str, s: &Store) -> Value {
    debug_assert!(super::schema::ROW_TABLES.contains(&table), "表不在白名单: {table}");
    let tokens: serde_json::Map<String, Value> = s
        .rows_all(table)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, v)| v.is_object())
        .collect();
    let meta_key = format!("{table}_meta");
    let version = s.kv_get_raw(&meta_key).and_then(|v| v.parse::<i64>().ok());
    let mut root = serde_json::Map::new();
    root.insert("tokens".into(), Value::Object(tokens));
    if let Some(v) = version {
        root.insert("version".into(), json!(v));
    }
    Value::Object(root)
}

/// 任意 token store 表单账号 UPSERT
pub fn token_store_upsert(s: &Store, table: &str, id: &str, rec: &Value) -> Result<(), String> {
    debug_assert!(super::schema::ROW_TABLES.contains(&table), "表不在白名单: {table}");
    s.row_upsert(table, id, rec)
}

/// 任意 token store 表删除单账号（迁移/移除路径）
pub fn token_store_remove(s: &Store, table: &str, id: &str) -> Result<(), String> {
    debug_assert!(super::schema::ROW_TABLES.contains(&table), "表不在白名单: {table}");
    s.row_delete(table, id)
}

/// 任意 token store 表整库替换（version 闸门由调用方维持；buddy-ai 迁移/阶段 2 接线用）
#[allow(dead_code)]
pub fn token_store_save(table: &str, s: &Store, store_val: &Value) -> Result<(), String> {
    debug_assert!(super::schema::ROW_TABLES.contains(&table), "表不在白名单: {table}");
    let empty = serde_json::Map::new();
    let tokens = store_val.get("tokens").and_then(Value::as_object).unwrap_or(&empty);
    let rows: Vec<(String, Value)> =
        tokens.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    s.rows_replace(table, &rows)?;
    if let Some(v) = store_val.get("version").and_then(Value::as_i64) {
        s.kv_set_raw(&format!("{table}_meta"), &v.to_string())?;
    }
    Ok(())
}

// ── Buddy 国际版（buddy-ai）独立池（buddy_ai_accounts / buddy_ai_tokens 表）──
//
// §3.4 隔离清单：与 wb 池零共享；账号结构与 WorkBuddyAccount serde 同构
//（复用序列化形态，落库表独立）。

/// 读 buddy-ai 整池（结构 {accounts: [...]}）
pub fn buddy_ai_pool_load(s: &Store) -> Value {
    let accounts: Vec<Value> = s
        .rows_all("buddy_ai_accounts")
        .unwrap_or_default()
        .into_iter()
        .map(|(_, data)| data)
        .collect();
    json!({ "accounts": accounts })
}

/// 写 buddy-ai 整池（pk = 账号 id，缺 id 用序号占位，语义同 wb_pool_save）
pub fn buddy_ai_pool_save(s: &Store, pool: &Value) -> Result<(), String> {
    let empty = Vec::new();
    let arr = pool.get("accounts").and_then(Value::as_array).unwrap_or(&empty);
    let rows: Vec<(String, Value)> = arr
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let id = a.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
            (id.map(String::from).unwrap_or_else(|| format!("__idx{i}")), a.clone())
        })
        .collect();
    s.rows_replace("buddy_ai_accounts", &rows)
}

// ── API 用量（api_usage.json → api_usage 表，(bucket, day) 行文档）───────────

fn bucket_name(b: UsageBucket) -> &'static str {
    match b {
        UsageBucket::Trae => "trae",
        UsageBucket::Wb => "wb",
        UsageBucket::Custom => "custom",
        UsageBucket::Qoder => "qoder",
        UsageBucket::BuddyAi => "buddy-ai",
    }
}

fn bucket_of(name: &str) -> Option<UsageBucket> {
    match name {
        "trae" => Some(UsageBucket::Trae),
        "wb" => Some(UsageBucket::Wb),
        "custom" => Some(UsageBucket::Custom),
        "qoder" => Some(UsageBucket::Qoder),
        "buddy-ai" => Some(UsageBucket::BuddyAi),
        _ => None,
    }
}

fn bucket_map_mut<'a>(f: &'a mut UsageFile, b: UsageBucket) -> &'a mut std::collections::HashMap<String, DayStats> {
    match b {
        UsageBucket::Trae => &mut f.days,
        UsageBucket::Wb => &mut f.wb_days,
        UsageBucket::Custom => &mut f.custom_days,
        UsageBucket::Qoder => &mut f.qoder_days,
        UsageBucket::BuddyAi => &mut f.buddy_ai_days,
    }
}

pub fn api_usage_load(s: &Store) -> UsageFile {
    let mut f = UsageFile::default();
    let rows = s
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT bucket, day, data FROM api_usage")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    for (bucket, day, data) in rows {
        let (Some(b), Ok(stats)) = (bucket_of(&bucket), serde_json::from_str::<DayStats>(&data)) else {
            continue;
        };
        bucket_map_mut(&mut f, b).insert(day, stats);
    }
    f
}

pub fn api_usage_save(s: &Store, f: &UsageFile) -> Result<(), String> {
    let rows: Vec<(String, String, String)> = [
        (UsageBucket::Trae, &f.days),
        (UsageBucket::Wb, &f.wb_days),
        (UsageBucket::Custom, &f.custom_days),
        // 四桶对称（审查修复清单 v1.4.9 B8）：漏 Qoder 桶会使 JSON→SQLite 迁移时
        // Qoder 历史用量整表丢弃（api_usage_load 侧 bucket_map_mut 四桶齐全）
        (UsageBucket::Qoder, &f.qoder_days),
        // 审查修复（M8，国际版）：漏 BuddyAi 桶同因——save 侧丢桶会让
        // api_usage_load→save 往返及迁移导入静默清空 buddy-ai 历史用量
        //（load 侧 bucket_map_mut 五桶齐全，save 侧必须对称）
        (UsageBucket::BuddyAi, &f.buddy_ai_days),
    ]
    .into_iter()
    .flat_map(|(b, map)| {
        map.iter().map(move |(day, stats)| {
            serde_json::to_string(stats).map(|text| (bucket_name(b).to_string(), day.clone(), text))
        })
    })
    .collect::<Result<_, _>>()
    .map_err(|e| format!("序列化失败: {e}"))?;
    s.with_conn(move |c| {
        c.execute_batch("BEGIN; DELETE FROM api_usage;")?;
        {
            let mut stmt = c.prepare(
                "INSERT INTO api_usage(bucket, day, data, updated_at) VALUES(?1, ?2, ?3, datetime('now','localtime'))",
            )?;
            for (bucket, day, text) in &rows {
                stmt.execute(rusqlite::params![bucket, day, text])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

/// 单日行 UPSERT（每请求记账热路径：仅写当日一行，替代整表 DELETE+重插）
pub fn api_usage_upsert_day(s: &Store, bucket: &str, day: &str, data: &str) -> Result<(), String> {
    s.with_conn(|c| {
        c.execute(
            "INSERT INTO api_usage(bucket, day, data, updated_at) VALUES(?1, ?2, ?3, datetime('now','localtime'))
             ON CONFLICT(bucket, day) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
            rusqlite::params![bucket, day, data],
        )?;
        Ok(())
    })
}

/// 保留期裁剪（超出 keep_days 的行删除；启动 load 时执行一次）
pub fn api_usage_prune(s: &Store, keep_days: i64) -> Result<(), String> {
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(keep_days))
        .format("%Y-%m-%d")
        .to_string();
    s.with_conn(|c| {
        c.execute("DELETE FROM api_usage WHERE day < ?1", [cutoff.as_str()])?;
        Ok(())
    })
}

// ── Qoder 网关会话流水（issue #82 问题5：qoder_sessions 表）──────────────────

/// 单请求会话行 UPSERT（request_id 主键幂等；记账热路径直写，失败由调用方静默）。
/// issue #82 审查修复 A：同一信封重试（排队退避）复用同一 request_id，
/// 若首尝试失败行已落库，后到的成功行须覆盖（按 ts 更新，旧覆盖新为 no-op）；
/// 同 ts 重复记账（重放）不覆盖，保留首行口径。
#[allow(clippy::too_many_arguments)]
pub fn qoder_session_insert(
    s: &Store,
    request_id: &str,
    session_id: &str,
    uid: &str,
    date: &str,
    ts: i64,
    model: &str,
    credits: f64,
    pt: i64,
    ct: i64,
    duration_ms: i64,
    stream: bool,
    ok: bool,
    preview: &str,
    record_id: &str,
) -> Result<(), String> {
    // 防御（审查修复）：request_id 取自上游信封（外部可控字段），"agg:" 为
    // qoder_sessions_compact 聚合行保留前缀（DELETE 排除与 HAVING 判定依据）；
    // 上游伪造同前缀 id 会经 ON CONFLICT 顶撞/覆盖历史聚合行主键——入口统一换 uuid。
    // 判定与 SQLite GLOB（NOT GLOB 'agg:*'，ASCII 大小写敏感）同口径：小写化后
    // 前缀匹配，AGG:x 等大小写变体同样拦截（此前 starts_with 大小写敏感而
    // LIKE 不敏感，AGG:x 会落库但永不被合并删除，credits 永久双计——S2）
    let request_id = if request_id.to_ascii_lowercase().starts_with("agg:") {
        uuid::Uuid::new_v4().to_string()
    } else {
        request_id.to_string()
    };
    s.with_conn(|c| {
        c.execute(
            "INSERT INTO qoder_sessions
             (request_id, session_id, uid, date, ts, model, credits, pt, ct, duration_ms, stream, ok, preview, record_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(request_id) DO UPDATE SET
               uid = excluded.uid, date = excluded.date, ts = excluded.ts,
               model = excluded.model, credits = excluded.credits,
               pt = excluded.pt, ct = excluded.ct,
               duration_ms = excluded.duration_ms, stream = excluded.stream,
               ok = excluded.ok, preview = excluded.preview, record_id = excluded.record_id
             WHERE excluded.ts > qoder_sessions.ts",
            rusqlite::params![
                request_id, session_id, uid, date, ts, model, credits, pt, ct,
                duration_ms, stream as i64, ok as i64, preview, record_id
            ],
        )?;
        Ok(())
    })
}

/// qoder_sessions 保留期裁剪（与 api_usage 同口径 90 天；低频附带执行）
pub fn qoder_sessions_prune(s: &Store, keep_days: i64) -> Result<usize, String> {
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(keep_days))
        .format("%Y-%m-%d")
        .to_string();
    s.with_conn(|c| Ok(c.execute("DELETE FROM qoder_sessions WHERE date < ?1", [cutoff.as_str()])?))
}

// ── Custom 网关会话流水（issue #149 P2：custom_sessions 表）──────────────────

/// 单请求会话行插入（request_id 由调用方内部生成 uuid，无冲突可能，纯 INSERT）。
/// 记账热路径直写，失败由调用方静默（与 qoder_session_insert 同策略）；
/// 每次 attempt 各记一行（成功/失败如实落账）。
#[allow(clippy::too_many_arguments)]
pub fn custom_session_insert(
    s: &Store,
    request_id: &str,
    session_id: &str,
    uid: &str,
    date: &str,
    ts: i64,
    model: &str,
    stage: &str,
    key_name: &str,
    pt: i64,
    ct: i64,
    duration_ms: i64,
    stream: bool,
    ok: bool,
    preview: &str,
) -> Result<(), String> {
    s.with_conn(|c| {
        c.execute(
            "INSERT INTO custom_sessions
             (request_id, session_id, uid, date, ts, model, stage, key_name, pt, ct, duration_ms, stream, ok, preview)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                request_id, session_id, uid, date, ts, model, stage, key_name, pt, ct,
                duration_ms, stream as i64, ok as i64, preview
            ],
        )?;
        Ok(())
    })
}

/// custom_sessions 保留期裁剪（与 qoder_sessions 同口径 90 天；低频附带执行）
pub fn custom_sessions_prune(s: &Store, keep_days: i64) -> Result<usize, String> {
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(keep_days))
        .format("%Y-%m-%d")
        .to_string();
    s.with_conn(|c| Ok(c.execute("DELETE FROM custom_sessions WHERE date < ?1", [cutoff.as_str()])?))
}

/// 合并保留窗默认值（天）：早于 N 天的请求级流水参与合并（date < today-N），
/// 近 N 天保留请求级明细供排查；实际窗口经 settings.qoder_sessions_compact_days
/// 可配（环境配置页，非法/越界回退本值并 clamp 1..=90）
pub const QODER_SESSIONS_COMPACT_AFTER_DAYS: i64 = 3;

/// qoder_sessions 定时合并（幂等，qoder-sessions-compact 调度任务调用）：
/// 早于 before_days 的请求行按 **(date, uid, preview)** 合并为单行
/// （request_id = "agg:<date>:<uid>:<hex(preview)>"，hex 编码防 preview 冒号
/// 与超长污染主键）。
///
/// 分组键不含 session_id 是刻意的：历史落库行 session_id 为每请求随机值
/// （修复前的 seed 缺口），按 session_id 分组对旧行「每组一行」等于没合并；
/// 同轮请求 preview 恒相同（末条 user 消息），同轮归并即可压缩体积，
/// 页面读时聚合（aggregate_session_rows 两遍归并）再按 session_id 恢复会话级。
///
/// - SUM(credits/pt/ct)：合并前后行和恒一致；
/// - ts/session_id/model/preview：取组内最新请求（ORDER BY ts DESC,
///   request_id DESC，相关子查询引用 GROUP BY 列，SQLite 合法）；
/// - duration_ms/stream/ok：合并行不携带请求级语义，落 0/0/1（查询层不消费）；
/// - 幂等：已合并组（仅含 agg 行）经 HAVING 剔除不再重写；迟到行并入组后
///   下次运行 OR REPLACE 覆盖聚合行，累计正确不双计；
/// - 两步同事务（审查修复）：INSERT 与 DELETE 原子提交，中断整体回滚，
///   绝不遗留「聚合行+原始行」并存——并存会使读时聚合双倍展示，且下次
///   重跑 SUM 把旧聚合行自身计入而双倍固化（HAVING 仅要求组内有非 agg 行，
///   拦不住该场景，依赖事务保证）；
/// - 空 preview 行（失败早退）同组归并（同日同账号），合计诚实不隐藏。
///
/// 返回 (本次合并的会话组数, 删除的原始请求行数)。
pub fn qoder_sessions_compact(s: &Store, before_days: i64) -> Result<(usize, usize), String> {
    // 守卫（审查 S1 附带）：pub fn 无调用方保证，负数/0 会把当天活跃会话一并
    // 合并删除；调度器/设置层已 clamp 1..=90，此处再兜底 clamp 1..=365
    let before_days = before_days.clamp(1, 365);
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(before_days))
        .format("%Y-%m-%d")
        .to_string();
    s.with_conn(|c| {
        // 两步同事务（审查修复）：INSERT 与 DELETE 原子提交——中断（进程退出/
        // 语句报错）整体回滚，绝不遗留「聚合行+原始行」并存（并存会读时双倍
        // 展示，且下次重跑 SUM 把旧聚合行计入而双倍固化）
        let tx = c.unchecked_transaction()?;
        let merged = tx.execute(
            "INSERT OR REPLACE INTO qoder_sessions
             (request_id, session_id, uid, date, ts, model, credits, pt, ct, duration_ms, stream, ok, preview)
             SELECT 'agg:' || date || ':' || uid || ':' || hex(CAST(preview AS BLOB)),
                    (SELECT x.session_id FROM qoder_sessions x
                      WHERE x.date = q.date AND x.uid = q.uid AND x.preview = q.preview
                      ORDER BY x.ts DESC, x.request_id DESC LIMIT 1),
                    uid, date, MAX(ts),
                    (SELECT x.model FROM qoder_sessions x
                      WHERE x.date = q.date AND x.uid = q.uid AND x.preview = q.preview
                      ORDER BY x.ts DESC, x.request_id DESC LIMIT 1),
                    SUM(credits), SUM(pt), SUM(ct), 0, 0, 1,
                    preview
             FROM qoder_sessions q
             WHERE q.date < ?1
             GROUP BY q.date, q.uid, q.preview
             HAVING SUM(request_id NOT GLOB 'agg:*') > 0",
            [&cutoff],
        )?;
        let deleted = tx.execute(
            "DELETE FROM qoder_sessions
             WHERE date < ?1 AND request_id NOT GLOB 'agg:*'",
            [&cutoff],
        )?;
        tx.commit()?;
        Ok((merged, deleted))
    })
}

// ── Trae 积分流水（credits_history.json → credits_history 表）────────────────

pub fn credits_history_load(s: &Store) -> CreditsFile {
    let records = s
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT date, user_id, credits, delta FROM credits_history ORDER BY id")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(CreditRecord {
                        date: r.get(0)?,
                        user_id: r.get(1)?,
                        credits: r.get(2)?,
                        delta: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    CreditsFile { records }
}

/// 追加流水（替代原「整文件读改写」追加；同日内多条合法）
pub fn credits_history_append(s: &Store, records: &[CreditRecord]) -> Result<(), String> {
    s.with_conn(|c| {
        let mut stmt = c
            .prepare("INSERT INTO credits_history(date, user_id, credits, delta) VALUES(?1, ?2, ?3, ?4)")?;
        for r in records {
            stmt.execute(rusqlite::params![r.date, r.user_id, r.credits, r.delta])?;
        }
        Ok(())
    })
}

/// 整表替换（原「读取历史文件→整体覆盖」语义的等价物）
pub fn credits_history_save(s: &Store, f: &CreditsFile) -> Result<(), String> {
    s.with_conn(|c| {
        c.execute_batch("BEGIN; DELETE FROM credits_history;")?;
        {
            let mut stmt = c
                .prepare("INSERT INTO credits_history(date, user_id, credits, delta) VALUES(?1, ?2, ?3, ?4)")?;
            for r in &f.records {
                stmt.execute(rusqlite::params![r.date, r.user_id, r.credits, r.delta])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

// ── 每日积分快照（credits_daily.json → credits_daily 表）─────────────────────

pub fn credits_daily_load(s: &Store) -> CreditsDailyFile {
    let snapshots = s
        .with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT date, total, earned, consumed, accounts FROM credits_daily ORDER BY date")?;
            let rows = stmt
                .query_map([], |r| {
                    let accounts_json: Option<String> = r.get(4)?;
                    let accounts = accounts_json
                        .as_deref()
                        .and_then(|t| serde_json::from_str(t).ok())
                        .unwrap_or_default();
                    Ok(crate::models::CreditsDailySnapshot {
                        date: r.get(0)?,
                        total: r.get(1)?,
                        earned: r.get(2)?,
                        consumed: r.get(3)?,
                        accounts,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    CreditsDailyFile { snapshots }
}

pub fn credits_daily_save(s: &Store, f: &CreditsDailyFile) -> Result<(), String> {
    s.with_conn(|c| {
        c.execute_batch("BEGIN; DELETE FROM credits_daily;")?;
        {
            let mut stmt = c.prepare(
                "INSERT INTO credits_daily(date, total, earned, consumed, accounts) VALUES(?1, ?2, ?3, ?4, ?5)",
            )?;
            for x in &f.snapshots {
                let accounts_json = serde_json::to_string(&x.accounts).unwrap_or_else(|_| "{}".to_string());
                stmt.execute(rusqlite::params![x.date, x.total, x.earned, x.consumed, accounts_json])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

// ── 签到结果（checkin_results.json → checkin_results 表）─────────────────────

pub fn checkin_results_load(s: &Store) -> crate::checkin_results::ResultsFile {
    use std::collections::BTreeMap;
    let mut days: BTreeMap<String, crate::checkin_results::DayRecord> = BTreeMap::new();
    let rows = s
        .with_conn(|c| {
            let mut stmt = c
                .prepare("SELECT day, uid, name, status, updated_at FROM checkin_results ORDER BY day, uid")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    for (day, uid, name, status, updated_at) in rows {
        days.entry(day).or_default().accounts.insert(
            uid,
            crate::checkin_results::AccountResult { name, status, updated_at },
        );
    }
    crate::checkin_results::ResultsFile { days }
}

pub fn checkin_results_save(s: &Store, f: &crate::checkin_results::ResultsFile) -> Result<(), String> {
    s.with_conn(|c| {
        c.execute_batch("BEGIN; DELETE FROM checkin_results;")?;
        {
            let mut stmt = c.prepare(
                "INSERT INTO checkin_results(day, uid, name, status, updated_at) VALUES(?1, ?2, ?3, ?4, ?5)",
            )?;
            for (day, rec) in &f.days {
                for (uid, a) in &rec.accounts {
                    stmt.execute(rusqlite::params![day, uid, a.name, a.status, a.updated_at])?;
                }
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

// ── 豆包签到结果（按日落库 → doubao_checkin_results 表）─────────────────────

pub fn doubao_checkin_results_load(
    s: &Store,
) -> crate::doubao_checkin_results::ResultsFile {
    use std::collections::BTreeMap;
    let mut days: BTreeMap<String, crate::doubao_checkin_results::DayRecord> = BTreeMap::new();
    let rows = s
        .with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT day, uid, name, status, reward, updated_at FROM doubao_checkin_results ORDER BY day, uid",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    for (day, uid, name, status, reward, updated_at) in rows {
        days.entry(day).or_default().accounts.insert(
            uid,
            crate::doubao_checkin_results::AccountResult { name, status, reward, updated_at },
        );
    }
    crate::doubao_checkin_results::ResultsFile { days }
}

pub fn doubao_checkin_results_save(
    s: &Store,
    f: &crate::doubao_checkin_results::ResultsFile,
) -> Result<(), String> {
    s.with_conn(|c| {
        c.execute_batch("BEGIN; DELETE FROM doubao_checkin_results;")?;
        {
            let mut stmt = c.prepare(
                "INSERT INTO doubao_checkin_results(day, uid, name, status, reward, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (day, rec) in &f.days {
                for (uid, a) in &rec.accounts {
                    stmt.execute(rusqlite::params![day, uid, a.name, a.status, a.reward, a.updated_at])?;
                }
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

// ── WorkBuddy 签到结果（workbuddy_checkin_results.json → wb_checkin_results 表）──

/// 读回 {results: [...]}（按 id 升序 = 原追加序）
pub fn wb_checkin_results_load(s: &Store) -> Value {
    let rows = s
        .with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT date, time, user_id, name, status, message, reward FROM wb_checkin_results ORDER BY id",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    let reward: Option<f64> = r.get(6)?;
                    Ok(json!({
                        "date": r.get::<_, String>(0)?,
                        "time": r.get::<_, String>(1)?,
                        "user_id": r.get::<_, String>(2)?,
                        "name": r.get::<_, String>(3)?,
                        "status": r.get::<_, String>(4)?,
                        "message": r.get::<_, String>(5)?,
                        "reward": reward,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    json!({ "results": rows })
}

/// 整表替换（原「读改写 results 数组 + 90 天裁剪」的落库等价物，裁剪仍由调用方计算）
pub fn wb_checkin_results_save(s: &Store, root: &Value) -> Result<(), String> {
    let empty = Vec::new();
    let arr = root.get("results").and_then(Value::as_array).unwrap_or(&empty);
    s.with_conn(|c| {
        c.execute_batch("BEGIN; DELETE FROM wb_checkin_results;")?;
        {
            let mut stmt = c.prepare(
                "INSERT INTO wb_checkin_results(date, time, user_id, name, status, message, reward) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for r in arr {
                let reward = r.get("reward").and_then(Value::as_f64);
                stmt.execute(rusqlite::params![
                    r.get("date").and_then(Value::as_str).unwrap_or(""),
                    r.get("time").and_then(Value::as_str).unwrap_or(""),
                    r.get("user_id").and_then(Value::as_str).unwrap_or(""),
                    r.get("name").and_then(Value::as_str).unwrap_or(""),
                    r.get("status").and_then(Value::as_str).unwrap_or(""),
                    r.get("message").and_then(Value::as_str).unwrap_or(""),
                    reward,
                ])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

// ── buddy-ai 每日活跃打卡结果（P2 对齐 wb_checkin_results；镜像同构）──

/// 读回 {results: [...]}（按 id 升序 = 原追加序）
pub fn buddy_ai_checkin_results_load(s: &Store) -> Value {
    let rows = s
        .with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT date, time, user_id, name, status, message, reward FROM buddy_ai_checkin_results ORDER BY id",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    let reward: Option<f64> = r.get(6)?;
                    Ok(json!({
                        "date": r.get::<_, String>(0)?,
                        "time": r.get::<_, String>(1)?,
                        "user_id": r.get::<_, String>(2)?,
                        "name": r.get::<_, String>(3)?,
                        "status": r.get::<_, String>(4)?,
                        "message": r.get::<_, String>(5)?,
                        "reward": reward,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    json!({ "results": rows })
}

/// 整表替换（裁剪由调用方计算）
pub fn buddy_ai_checkin_results_save(s: &Store, root: &Value) -> Result<(), String> {
    let empty = Vec::new();
    let arr = root.get("results").and_then(Value::as_array).unwrap_or(&empty);
    s.with_conn(|c| {
        c.execute_batch("BEGIN; DELETE FROM buddy_ai_checkin_results;")?;
        {
            let mut stmt = c.prepare(
                "INSERT INTO buddy_ai_checkin_results(date, time, user_id, name, status, message, reward) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for r in arr {
                let reward = r.get("reward").and_then(Value::as_f64);
                stmt.execute(rusqlite::params![
                    r.get("date").and_then(Value::as_str).unwrap_or(""),
                    r.get("time").and_then(Value::as_str).unwrap_or(""),
                    r.get("user_id").and_then(Value::as_str).unwrap_or(""),
                    r.get("name").and_then(Value::as_str).unwrap_or(""),
                    r.get("status").and_then(Value::as_str).unwrap_or(""),
                    r.get("message").and_then(Value::as_str).unwrap_or(""),
                    reward,
                ])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

// ── P8：buddy-ai 每日积分快照（镜像 wb_credits_history 同构）───────────────

/// 读回 {snapshots: [...]}（按日期升序）
pub fn buddy_ai_credits_history_load(s: &Store) -> Value {
    let rows = s
        .with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT date, ts, total_balance, accounts, earned FROM buddy_ai_credits_history ORDER BY date",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    let accounts: String = r.get(3)?;
                    Ok(json!({
                        "date": r.get::<_, String>(0)?,
                        "ts": r.get::<_, i64>(1)?,
                        "total_balance": r.get::<_, f64>(2)?,
                        "accounts": serde_json::from_str::<Value>(&accounts)
                            .unwrap_or(Value::Array(vec![])),
                        "earned": r.get::<_, Option<f64>>(4)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    json!({ "snapshots": rows })
}

/// 同日覆盖 UPSERT（打卡回写/手动同步双挂点调用，当日多账号回写自然收敛为最终态）
pub fn buddy_ai_credits_history_upsert(s: &Store, snap: &Value) -> Result<(), String> {
    let date = snap.get("date").and_then(Value::as_str).unwrap_or("").to_string();
    if date.is_empty() {
        return Err("快照缺 date 字段".into());
    }
    let ts = snap.get("ts").and_then(Value::as_i64).unwrap_or(0);
    let total = snap.get("total_balance").and_then(Value::as_f64).unwrap_or(0.0);
    let accounts = serde_json::to_string(
        snap.get("accounts").unwrap_or(&Value::Array(vec![])),
    )
    .map_err(|e| format!("序列化失败: {e}"))?;
    let earned = snap.get("earned").and_then(Value::as_f64);
    s.with_conn(move |c| {
        c.execute(
            "INSERT INTO buddy_ai_credits_history(date, ts, total_balance, accounts, earned) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(date) DO UPDATE SET ts = excluded.ts, total_balance = excluded.total_balance, accounts = excluded.accounts, earned = excluded.earned",
            rusqlite::params![date, ts, total, accounts, earned],
        )?;
        Ok(())
    })
}

/// 365 天滚动裁剪
pub fn buddy_ai_credits_history_prune(s: &Store) -> Result<(), String> {
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(365))
        .format("%Y-%m-%d")
        .to_string();
    s.with_conn(|c| {
        c.execute("DELETE FROM buddy_ai_credits_history WHERE date < ?1", [cutoff.as_str()])?;
        Ok(())
    })
}

// ── 豆包运维历史（doubao_health_history.json → doubao_health_events 表）──────

pub fn doubao_health_load(s: &Store) -> Vec<Value> {
    s.with_conn(|c| {
        let mut stmt = c.prepare("SELECT payload FROM doubao_health_events ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|p| serde_json::from_str(&p).ok())
            .collect())
    })
    .unwrap_or_default()
}

/// 整表替换（cap 由调用方裁剪后传入；原「超限 drain 最旧」语义保留在调用方）
pub fn doubao_health_save(s: &Store, events: &[Value]) -> Result<(), String> {
    // 序列化在事务外完成
    let mut payloads: Vec<String> = Vec::with_capacity(events.len());
    for e in events {
        payloads.push(serde_json::to_string(e).map_err(|e| format!("序列化失败: {e}"))?);
    }
    s.with_conn(move |c| {
        c.execute_batch("BEGIN; DELETE FROM doubao_health_events;")?;
        {
            let mut stmt = c.prepare("INSERT INTO doubao_health_events(payload) VALUES(?1)")?;
            for p in &payloads {
                stmt.execute(rusqlite::params![p])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .or_else(|e| {
        let _ = s.with_conn(|c| c.execute_batch("ROLLBACK;"));
        Err(e)
    })
}

// ── Qoder 账号池（F-80 M1；qoder_accounts 表，Value 语义，与 wb_pool 同款）──

/// 读整池（结构 {accounts: [...]}；账号对象原样保真）
pub fn qoder_pool_load(s: &Store) -> Value {
    let accounts: Vec<Value> = s
        .rows_all("qoder_accounts")
        .unwrap_or_default()
        .into_iter()
        .map(|(_, data)| data)
        .collect();
    json!({ "accounts": accounts })
}

/// 写整池（pk = 账号 id，缺 id 用序号占位）
pub fn qoder_pool_save(s: &Store, pool: &Value) -> Result<(), String> {
    let empty = Vec::new();
    let arr = pool.get("accounts").and_then(Value::as_array).unwrap_or(&empty);
    let rows: Vec<(String, Value)> = arr
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let id = a.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
            (id.map(String::from).unwrap_or_else(|| format!("__idx{i}")), a.clone())
        })
        .collect();
    s.rows_replace("qoder_accounts", &rows)
}

// ── Qoder token store（qoder_tokens 表；结构 {version, tokens: {id: rec}}）──

/// 读整库（损坏行过滤丢弃，不混入消费方；version 存 kv qoder_tokens_meta）
pub fn qoder_token_store_load(s: &Store) -> Value {
    let tokens: serde_json::Map<String, Value> = s
        .rows_all("qoder_tokens")
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, v)| v.is_object())
        .collect();
    let version = s.kv_get_raw("qoder_tokens_meta").and_then(|v| v.parse::<i64>().ok());
    let mut root = serde_json::Map::new();
    root.insert("tokens".into(), Value::Object(tokens));
    if let Some(v) = version {
        root.insert("version".into(), json!(v));
    }
    Value::Object(root)
}

/// 写整库（version 闸门由调用方维持，此处整表替换）
pub fn qoder_token_store_save(s: &Store, store_val: &Value) -> Result<(), String> {
    let empty = serde_json::Map::new();
    let tokens = store_val.get("tokens").and_then(Value::as_object).unwrap_or(&empty);
    let rows: Vec<(String, Value)> =
        tokens.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    s.rows_replace("qoder_tokens", &rows)?;
    if let Some(v) = store_val.get("version").and_then(Value::as_i64) {
        s.kv_set_raw("qoder_tokens_meta", &v.to_string())?;
    }
    Ok(())
}

// ── Qoder 签到结果（qoder_checkin_results 表；行文档 + 90 天滚动由调用方裁剪）──

/// 读回 {results: [...]}（rows_all 按 rowid 序 = 原追加序）
pub fn qoder_checkin_results_load(s: &Store) -> Value {
    let results: Vec<Value> = s
        .rows_all("qoder_checkin_results")
        .unwrap_or_default()
        .into_iter()
        .map(|(_, data)| data)
        .collect();
    json!({ "results": results })
}

/// 单条签到结果 UPSERT + 90 天滚动裁剪。
/// pk = `date|user_id|time_ms`（内容派生）：原 `date|user_id|idx` 的 idx 是「载入时数组
/// 下标」，计划任务与 UI 触发两进程各自从 0 计数，同日同账号并发写入必然互相覆盖；
/// 秒级 time 同账号同秒仍会碰撞，故优先毫秒级 time_ms（旧记录/旧调用方回退 time 兼容）。
/// 裁剪为逐 pk DELETE，仅删过期行不触碰新写入（对齐 qoder_credits_history_upsert 模式）。
pub fn qoder_checkin_results_upsert(s: &Store, rec: &Value) -> Result<(), String> {
    let date = rec.get("date").and_then(Value::as_str).unwrap_or("").to_string();
    let uid = rec.get("user_id").and_then(Value::as_str).unwrap_or("").to_string();
    let tail = match rec.get("time_ms").and_then(Value::as_i64) {
        Some(ms) => ms.to_string(),
        None => rec
            .get("time")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    };
    if date.is_empty() || uid.is_empty() || tail.is_empty() {
        return Err("签到结果缺少 date/user_id/time_ms(time)".into());
    }
    s.row_upsert("qoder_checkin_results", &format!("{date}|{uid}|{tail}"), rec)?;
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(90))
        .format("%Y-%m-%d")
        .to_string();
    let stale: Vec<String> = s
        .rows_all("qoder_checkin_results")
        .unwrap_or_default()
        .into_iter()
        .map(|(pk, _)| pk)
        .filter(|pk| pk.as_str() < cutoff.as_str())
        .collect();
    let mut fail = 0usize;
    let mut first_err = String::new();
    for pk in stale {
        if let Err(e) =
            s.with_conn(|c| c.execute("DELETE FROM qoder_checkin_results WHERE pk = ?1", [&pk]))
        {
            fail += 1;
            if first_err.is_empty() {
                first_err = e;
            }
        }
    }
    if fail > 0 {
        // store 层无 data_dir 不可 app_log；裁剪失败不影响本次写入，过期行留待下次
        eprintln!("[qoder] 签到结果滚动裁剪删除失败 {fail} 条（首错: {first_err}），过期行将留待下次裁剪");
    }
    Ok(())
}

// ── Qoder 每日积分快照（qoder_credits_history 表；pk = date，同日覆盖）──────

/// 读全部快照（按 pk 升序 = 日期升序）
pub fn qoder_credits_history_load(s: &Store) -> Vec<Value> {
    s.rows_all("qoder_credits_history")
        .unwrap_or_default()
        .into_iter()
        .map(|(_, data)| data)
        .collect()
}

/// 同日覆盖 upsert + 365 天裁剪（pk 即 date，YYYY-MM-DD 字符串比较即时间序）
pub fn qoder_credits_history_upsert(s: &Store, snap: &Value) -> Result<(), String> {
    let date = snap.get("date").and_then(Value::as_str).unwrap_or("").to_string();
    if date.is_empty() {
        return Err("快照缺少 date 字段".into());
    }
    s.row_upsert("qoder_credits_history", &date, snap)?;
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(365))
        .format("%Y-%m-%d")
        .to_string();
    let stale: Vec<String> = s
        .rows_all("qoder_credits_history")
        .unwrap_or_default()
        .into_iter()
        .map(|(pk, _)| pk)
        .filter(|pk| pk.as_str() < cutoff.as_str())
        .collect();
    let mut fail = 0usize;
    let mut first_err = String::new();
    for pk in stale {
        if let Err(e) =
            s.with_conn(|c| c.execute("DELETE FROM qoder_credits_history WHERE pk = ?1", [&pk]))
        {
            fail += 1;
            if first_err.is_empty() {
                first_err = e;
            }
        }
    }
    if fail > 0 {
        eprintln!("[qoder] 积分快照滚动裁剪删除失败 {fail} 条（首错: {first_err}），过期行将留待下次裁剪");
    }
    Ok(())
}
