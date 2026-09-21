//! 全部建表 DDL + user_version 版本管理（docs/sqllite-storage-plan.md §2.2）。
//!
//! 三组表：
//! ① kv —— 配置/整存整取文档（content = 原 serde JSON）；
//! ② 行文档实体表 —— (pk TEXT PK, data JSON, updated_at)；
//! ③ 列化流水表 —— 追加/裁剪/按日聚合。
//!
//! 版本约定：`PRAGMA user_version >= SCHEMA_VERSION` 表示本迁移完成；<SCHEMA_VERSION 触发启动迁移。

use rusqlite::Connection;

pub const SCHEMA_VERSION: i32 = 2;

/// 行文档表白名单（rows_* 原语允许操作的表，防表名拼接注入）
pub const ROW_TABLES: &[&str] = &[
    "device_map",
    "groups",
    "remaining_credits",
    "account_cooldowns",
    "pay_status",
    "api_keys",
    "custom_models",
    "doubao_accounts",
    "wb_accounts",
    "wb_tokens",
];

const DDL: &[&str] = &[
    // ① KV 文档表
    "CREATE TABLE IF NOT EXISTS kv (
        key        TEXT PRIMARY KEY,
        content    TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    // ② 行文档实体表
    "CREATE TABLE IF NOT EXISTS device_map (
        pk         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE TABLE IF NOT EXISTS remaining_credits (
        pk         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE TABLE IF NOT EXISTS account_cooldowns (
        pk         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE TABLE IF NOT EXISTS pay_status (
        pk         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE TABLE IF NOT EXISTS api_keys (
        pk         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE TABLE IF NOT EXISTS custom_models (
        pk         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE TABLE IF NOT EXISTS doubao_accounts (
        pk         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE TABLE IF NOT EXISTS wb_accounts (
        pk         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE TABLE IF NOT EXISTS wb_tokens (
        pk         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    // Trae 账号（保序数组语义：seq 自增保序，user_id 可空但非空时唯一）
    "CREATE TABLE IF NOT EXISTS accounts (
        seq     INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id TEXT UNIQUE,
        data    TEXT NOT NULL
    )",
    // 分组：定义 + 成员映射
    "CREATE TABLE IF NOT EXISTS groups (
        pk         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE TABLE IF NOT EXISTS group_members (
        uid        TEXT PRIMARY KEY,
        group_id   TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    // API 用量：(bucket, day) 行文档，data = DayStats 全量 JSON（含 latency 样本）
    "CREATE TABLE IF NOT EXISTS api_usage (
        bucket     TEXT NOT NULL,
        day        TEXT NOT NULL,
        data       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime')),
        PRIMARY KEY (bucket, day)
    )",
    // ③ 列化流水表
    "CREATE TABLE IF NOT EXISTS credits_history (
        id      INTEGER PRIMARY KEY AUTOINCREMENT,
        date    TEXT NOT NULL,
        user_id TEXT NOT NULL DEFAULT '',
        credits INTEGER NOT NULL DEFAULT 0,
        delta   INTEGER NOT NULL DEFAULT 0
    )",
    "CREATE TABLE IF NOT EXISTS credits_daily (
        date     TEXT PRIMARY KEY,
        total    REAL NOT NULL DEFAULT 0,
        earned   REAL NOT NULL DEFAULT 0,
        consumed REAL NOT NULL DEFAULT 0
    )",
    "CREATE TABLE IF NOT EXISTS checkin_results (
        day        TEXT NOT NULL,
        uid        TEXT NOT NULL,
        name       TEXT NOT NULL DEFAULT '',
        status     TEXT NOT NULL DEFAULT '',
        updated_at TEXT NOT NULL DEFAULT '',
        PRIMARY KEY (day, uid)
    )",
    "CREATE TABLE IF NOT EXISTS wb_checkin_results (
        id      INTEGER PRIMARY KEY AUTOINCREMENT,
        date    TEXT NOT NULL DEFAULT '',
        time    TEXT NOT NULL DEFAULT '',
        user_id TEXT NOT NULL DEFAULT '',
        name    TEXT NOT NULL DEFAULT '',
        status  TEXT NOT NULL DEFAULT '',
        message TEXT NOT NULL DEFAULT '',
        reward  REAL
    )",
    "CREATE TABLE IF NOT EXISTS doubao_health_events (
        id      INTEGER PRIMARY KEY AUTOINCREMENT,
        payload TEXT NOT NULL
    )",
    // P6 流水迁出：WB 每日积分快照（原 kv workbuddy_credits_history，同日覆盖 + 365 天）
    "CREATE TABLE IF NOT EXISTS wb_credits_history (
        date         TEXT PRIMARY KEY,
        ts           INTEGER NOT NULL DEFAULT 0,
        total_balance REAL NOT NULL DEFAULT 0,
        accounts     TEXT NOT NULL DEFAULT '[]'
    )",
    // P6 流水迁出：消耗明细增量拉取缓存（原 kv usage_history，per-account/per-day 行）
    "CREATE TABLE IF NOT EXISTS usage_history_accounts (
        uid              TEXT PRIMARY KEY,
        name             TEXT NOT NULL DEFAULT '',
        last_fetch_end_ts INTEGER,
        updated_at       TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE TABLE IF NOT EXISTS usage_history_days (
        uid  TEXT NOT NULL,
        date TEXT NOT NULL,
        data TEXT NOT NULL,
        PRIMARY KEY (uid, date)
    )",
    "CREATE INDEX IF NOT EXISTS idx_usage_history_days_date ON usage_history_days(date)",
    // P6 流水迁出：会话粘性绑定（原 kv wb_sticky_sessions；过期项落库前清理）
    "CREATE TABLE IF NOT EXISTS sticky_bindings (
        key        TEXT PRIMARY KEY,
        uid        TEXT NOT NULL,
        conv_id    TEXT NOT NULL DEFAULT '',
        last_seen  INTEGER NOT NULL DEFAULT 0,
        explicit   INTEGER NOT NULL DEFAULT 0,
        updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
    )",
    "CREATE INDEX IF NOT EXISTS idx_credits_history_date ON credits_history(date)",
    "CREATE INDEX IF NOT EXISTS idx_wb_checkin_results_date ON wb_checkin_results(date)",
    "CREATE INDEX IF NOT EXISTS idx_checkin_results_day ON checkin_results(day)",
];

/// 建库（幂等：IF NOT EXISTS）。user_version 由迁移器负责写入。
/// 返回错误供 Store::try_open 判定库不可用（触发隔离重建自愈）。
pub fn init(conn: &Connection) -> Result<(), String> {
    for sql in DDL {
        conn.execute_batch(sql).map_err(|e| format!("建表失败: {e}"))?;
    }
    Ok(())
}

pub fn user_version(conn: &Connection) -> i32 {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0)
}

pub fn set_user_version(conn: &Connection, v: i32) {
    let _ = conn.pragma_update(None, "user_version", v);
}
