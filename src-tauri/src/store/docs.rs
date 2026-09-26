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

/// 读回 CacheFile 形状 Value：{fetched_at, accounts:{uid:{name,last_fetch_end_ts,daily:{date:stat}}}}
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
            json!({"name": name, "last_fetch_end_ts": ts, "daily": {}}),
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
    }
    s.with_conn(move |c| {
        c.execute_batch("BEGIN; DELETE FROM usage_history_accounts; DELETE FROM usage_history_days;")?;
        {
            let mut meta = c.prepare(
                "INSERT INTO usage_history_accounts(uid, name, last_fetch_end_ts, updated_at) VALUES(?1, ?2, ?3, datetime('now','localtime'))",
            )?;
            let mut day = c.prepare(
                "INSERT INTO usage_history_days(uid, date, data) VALUES(?1, ?2, ?3)",
            )?;
            for (uid, name, ts) in &metas {
                meta.execute(rusqlite::params![uid, name, ts])?;
            }
            for (uid, date, text) in &day_rows {
                day.execute(rusqlite::params![uid, date, text])?;
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

/// 整表替换（save 的落库等价物；过期项由调用方 evict 后传入）
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
        f.work.insert("u1".into(), 50.0);
        f.total_limit.insert("u2".into(), 2000.0);
        f.updated_at = Some("2026-09-15T10:00:00".into());
        remaining_credits_save(&s, &f).unwrap();
        let got = remaining_credits_load(&s);
        assert_eq!(got.credits.get("u1"), Some(&123.45));
        assert_eq!(got.expire_times.get("u1"), Some(&1790000000));
        assert_eq!(got.work.get("u1"), Some(&50.0));
        assert_eq!(got.total_limit.get("u2"), Some(&2000.0));
        assert!(got.general.is_empty());
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
        let root = json!({
            "fetched_at": 12345,
            "accounts": {"u1": {"name": "甲", "last_fetch_end_ts": 999,
                                "daily": {"2026-09-15": {"date": "2026-09-15", "credits": 1.5, "sessions": 2}}}}
        });
        usage_history_save(&s, &root).unwrap();
        let got = usage_history_load(&s);
        assert_eq!(got["fetched_at"], 12345);
        assert_eq!(got["accounts"]["u1"]["name"], "甲");
        assert_eq!(got["accounts"]["u1"]["daily"]["2026-09-15"]["credits"], 1.5);

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

pub fn groups_save(s: &Store, f: &GroupsFile) -> Result<(), String> {
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

// ── 剩余积分缓存（remaining_credits.json → remaining_credits 表，7 平行 map 合并行）──

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
    }
    f.updated_at = s.kv_get::<Option<String>>("remaining_credits_updated_at");
    f
}

pub fn remaining_credits_save(s: &Store, f: &RemainingCreditsFile) -> Result<(), String> {
    // 并集 of 7 map 的键；每账号一行，仅写入存在的字段（缺省语义由 load 侧空值兜底）
    let maps_f: [&std::collections::HashMap<String, f64>; 4] =
        [&f.credits, &f.general, &f.work, &f.total_limit];
    let maps_i: [&std::collections::HashMap<String, i64>; 3] =
        [&f.expire_times, &f.membership_expire, &f.membership_next_billing];
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
            ins_f(&mut obj, "general", &f.general);
            ins_f(&mut obj, "work", &f.work);
            ins_f(&mut obj, "total_limit", &f.total_limit);
            ins_i(&mut obj, "membership_expire", &f.membership_expire);
            ins_i(&mut obj, "membership_next_billing", &f.membership_next_billing);
            (uid, Value::Object(obj))
        })
        .collect();
    s.rows_replace("remaining_credits", &rows)?;
    s.kv_set("remaining_credits_updated_at", &f.updated_at)
}

// ── 冷却状态（account_cooldowns.json → account_cooldowns 表）─────────────────

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
    s.rows_replace("account_cooldowns", &rows)?;
    s.kv_set("account_cooldowns_updated_at", &f.updated_at)
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
    s.rows_replace("pay_status", &rows)?;
    s.kv_set("pay_status_updated_at", &f.updated_at)
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
    s.rows_replace("api_keys", &rows)?;
    s.kv_set_raw("api_keys_auth_disabled", if f.auth_disabled { "true" } else { "false" })
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
    s.rows_replace("custom_models", &rows)?;
    s.kv_set_raw("custom_models_updated_at", &f.updated_at.to_string())
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
    s.rows_replace("doubao_accounts", &rows)?;
    s.kv_set(
        "doubao_pool_meta",
        &json!({ "last_keepalive_at": pool.last_keepalive_at }),
    )
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

// ── API 用量（api_usage.json → api_usage 表，(bucket, day) 行文档）───────────

fn bucket_name(b: UsageBucket) -> &'static str {
    match b {
        UsageBucket::Trae => "trae",
        UsageBucket::Wb => "wb",
        UsageBucket::Custom => "custom",
    }
}

fn bucket_of(name: &str) -> Option<UsageBucket> {
    match name {
        "trae" => Some(UsageBucket::Trae),
        "wb" => Some(UsageBucket::Wb),
        "custom" => Some(UsageBucket::Custom),
        _ => None,
    }
}

fn bucket_map_mut<'a>(f: &'a mut UsageFile, b: UsageBucket) -> &'a mut std::collections::HashMap<String, DayStats> {
    match b {
        UsageBucket::Trae => &mut f.days,
        UsageBucket::Wb => &mut f.wb_days,
        UsageBucket::Custom => &mut f.custom_days,
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
            let mut stmt = c.prepare("SELECT date, total, earned, consumed FROM credits_daily ORDER BY date")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(crate::models::CreditsDailySnapshot {
                        date: r.get(0)?,
                        total: r.get(1)?,
                        earned: r.get(2)?,
                        consumed: r.get(3)?,
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
            let mut stmt = c
                .prepare("INSERT INTO credits_daily(date, total, earned, consumed) VALUES(?1, ?2, ?3, ?4)")?;
            for x in &f.snapshots {
                stmt.execute(rusqlite::params![x.date, x.total, x.earned, x.consumed])?;
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
