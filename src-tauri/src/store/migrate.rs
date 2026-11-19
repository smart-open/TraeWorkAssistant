//! 启动迁移器：旧 data/ JSON 全量导入 SQLite，成功后原文件移入 `data/backup/`（幂等）。
//!
//! 三态（docs/sqllite-storage-plan.md §三）：
//! - 正常：解析 → 写库 → `fs::rename` 到 `backup/<相对路径>`；
//! - 损坏：JSON 解析失败 → 移 `backup/corrupt/`（对齐 read_json「坏文件回退默认」）；
//! - 失败：IO/DB 错误 → 保留原位，下次启动重试（不置 user_version）；
//!   「导入成功但移动失败」（杀软/句柄占用 rename 失败）→ 记 kv 待移动集合
//!   （migration_pending_move），重跑时跳过导入仅补移动——防止残留旧 JSON
//!   以陈旧内容整体覆盖运行期 kv（H2 审查修复）。
//!
//! 全部条目处理完且零失败才置 `PRAGMA user_version = 1`；明细落
//! `data/backup/migration_manifest.json`。失败不阻断启动（返回摘要由调用方写日志）。
//!
//! 另含 TraeWork 双应用合并存量迁移（`migrate_traework_merge`，独立 kv 标记幂等，
//! Trae 运行中自动跳过下次重试；见 docs/completed/Trae双应用合并适配调整方案.md §5.8）。

use serde_json::{json, Value};
use std::path::Path;

use super::{db, docs, schema, Store};

/// KV 组：（相对路径 → kv 键）。content = 文件文本原样（校验为合法 JSON 后入库）。
const KV_ENTRIES: &[(&str, &str)] = &[
    ("conf/app_settings.json", "app_settings"),
    ("data/api_pool.json", "api_pool"),
    ("data/dispatch_policy.json", "dispatch_policy"),
    ("data/api_gateway_settings.json", "api_gateway_settings"),
    ("data/api_models.json", "api_models"),
    ("data/wb_model_catalog.json", "wb_model_catalog"),
    ("data/trae_model_meta.json", "trae_model_meta"),
    ("data/wb_model_route.json", "wb_model_route"),
    ("data/wb_template_map.json", "wb_template_map"),
    ("data/checkin_summary.json", "checkin_summary"),
    ("data/workbuddy_settings.json", "workbuddy_settings"),
    ("data/workbuddy_credits_cache.json", "workbuddy_credits_cache"),
    ("data/workbuddy_usage_official_cache.json", "workbuddy_usage_official_cache"),
    ("data/workbuddy_usage_official_all_cache.json", "workbuddy_usage_official_all_cache"),
    ("data/workbuddy_activity_cache.json", "workbuddy_activity_cache"),
    ("data/wb_cli_rotate_state.json", "wb_cli_rotate_state"),
    ("data/token_stats_files.json", "token_stats_files"),
    ("data/doubao_renew_result.json", "doubao_renew_result"),
    ("data/oauth_device.json", "oauth_device"),
    ("data/scheduler_state.json", "scheduler_state"),
    ("data/doubao_captured_credentials.json", "doubao_captured_credentials"),
];
// P6：workbuddy_credits_history / usage_history / wb_sticky_sessions 三键为流水型数据，
// 已从 kv 组移出为表（wb_credits_history / usage_history_* / sticky_bindings），
// 由下方 DOC_ENTRIES / v1→v2 增量迁移处理。

/// 结构化组：（相对路径 → 导入函数）
const DOC_ENTRIES: &[(&str, fn(&Store, &Path) -> ImportStatus)] = &[
    ("data/checkin_accounts.json", import_accounts),
    ("data/device_map.json", import_device_map),
    ("data/groups.json", import_groups),
    ("data/remaining_credits.json", import_remaining_credits),
    ("data/account_cooldowns.json", import_account_cooldowns),
    ("data/pay_status.json", import_pay_status),
    ("data/api_keys.json", import_api_keys),
    ("data/custom_models.json", import_custom_models),
    ("data/doubao_accounts.json", import_doubao_pool),
    ("data/workbuddy_accounts.json", import_wb_pool),
    ("data/workbuddy_token_store.json", import_wb_token_store),
    ("data/api_usage.json", import_api_usage),
    ("data/credits_history.json", import_credits_history),
    ("data/credits_daily.json", import_credits_daily),
    ("data/checkin_results.json", import_checkin_results),
    ("data/workbuddy_checkin_results.json", import_wb_checkin_results),
    ("data/doubao_health_history.json", import_doubao_health),
    // P6 流水迁出
    ("data/workbuddy_credits_history.json", import_wb_credits_history),
    ("data/usage_history.json", import_usage_history),
    ("data/wb_sticky_sessions.json", import_sticky_bindings),
];

/// 遗留根路径兜底（存在才迁移；含 wb_upstream 死引用曾写出的根路径 token store）。
/// （相对路径, 目标 kv 键 或 None=走结构化导入器）
const LEGACY_ROOT: &[(&str, Option<&str>)] = &[
    ("workbuddy_token_store.json", None),
    ("wb_template_map.json", Some("wb_template_map")),
    ("api_models.json", Some("api_models")),
    ("wb_model_route.json", Some("wb_model_route")),
    ("wb_sticky_sessions.json", None),
    ("checkin_accounts.json", None),
];

enum ImportStatus {
    Ok,
    Corrupt(String),
    Error(String),
}

/// 迁移入口。返回 None = 无需迁移（库已是最新）；Some = 摘要（调用方写 app_log）。
pub fn migrate_on_startup(data_dir: &Path) -> Option<String> {
    let store = db(data_dir);
    // M6 修复（审查）：user_version 读取失败不再 fail-open 按 0 处理——库异常时
    // 盲目重跑迁移会让残留旧 JSON 覆盖运行期 kv。读失败 → 落日志跳过本轮
    let current = match store.with_conn(|c| Ok(schema::user_version(c))) {
        Ok(v) => v,
        Err(e) => {
            crate::fs_utils::app_log(
                data_dir,
                &format!("[迁移] user_version 读取失败，跳过本轮迁移: {e}"),
            );
            return None;
        }
    };
    if current >= schema::SCHEMA_VERSION {
        return None;
    }

    let mut items: Vec<Value> = Vec::new();
    let mut failed = 0usize;
    let mut imported = 0usize;
    let mut corrupt = 0usize;
    let mut missing = 0usize;
    // H2 修复（审查）：上轮「导入成功但移动失败」的文件集合（kv 持久化）。
    // 重跑时这些文件跳过导入、仅重试移动——否则 rename 失败的残留 JSON 会以
    // 陈旧内容整体覆盖运行期 kv（app_settings、api_pool 等用户已改的键被回滚）。
    // 读取失败 fail-closed 跳过本轮（集合状态未知时盲目导入同样有覆盖风险）
    let mut pending_move: std::collections::BTreeSet<String> = match load_pending_move(&store) {
        Ok(s) => s,
        Err(e) => {
            crate::fs_utils::app_log(
                data_dir,
                &format!("[迁移] 待移动集合读取失败，跳过本轮迁移: {e}"),
            );
            return None;
        }
    };
    // 已成功导入的正牌文件集合（遗留根路径兜底仅在其正牌文件缺失时执行，
    // 防止根路径残留覆盖 data/ 正牌数据）
    let mut imported_rels: std::collections::HashSet<String> = Default::default();

    for (rel, key) in KV_ENTRIES {
        // H2：上轮已导入仅剩移动未完成 → 跳过导入（防陈旧覆盖），只重试移动。
        // 仍计入 imported_rels（导入确已发生），供根路径残留垃圾清理判定
        if pending_move.contains(*rel) {
            imported_rels.insert(rel.to_string());
            if !data_dir.join(rel).exists() {
                pending_move.remove(*rel);
                missing += 1;
                items.push(json!({"file": rel, "result": "missing", "scope": "pending_move"}));
                continue;
            }
            match move_to_backup(data_dir, rel, false) {
                Ok(()) => {
                    pending_move.remove(*rel);
                    imported += 1;
                    items.push(json!({"file": rel, "result": "moved", "scope": "pending_move", "target": format!("kv:{key}")}));
                }
                Err(e) => {
                    failed += 1;
                    items.push(json!({"file": rel, "result": "move_failed", "detail": e.to_string()}));
                }
            }
            continue;
        }
        match import_kv(&store, data_dir, rel, key) {
            ItemResult::Imported => {
                imported += 1;
                imported_rels.insert(rel.to_string());
                match move_to_backup(data_dir, rel, false) {
                    Ok(()) => {
                        items.push(json!({"file": rel, "result": "imported", "target": format!("kv:{key}")}));
                    }
                    Err(e) => {
                        // H2：导入已入库但移动失败——记 pending 集合，failed++ 使版本号
                        // 不置位；重跑只补移动，绝不重导（防运行期 kv 被陈旧内容覆盖）
                        pending_move.insert(rel.to_string());
                        failed += 1;
                        items.push(json!({"file": rel, "result": "imported_but_move_failed", "detail": e.to_string(), "target": format!("kv:{key}")}));
                    }
                }
            }
            ItemResult::Missing => {
                missing += 1;
                items.push(json!({"file": rel, "result": "missing"}));
            }
            ItemResult::Corrupt(e) => {
                corrupt += 1;
                match move_to_backup(data_dir, rel, true) {
                    Ok(()) => {
                        items.push(json!({"file": rel, "result": "corrupt", "detail": e}));
                    }
                    // 损坏文件未入库、无覆盖风险：留原位，重跑再判 Corrupt 再移；
                    // failed++ 保证版本号不置位、移动有重试兜底
                    Err(m) => {
                        failed += 1;
                        items.push(json!({"file": rel, "result": "corrupt_move_failed", "detail": format!("{e}; 移动失败: {m}")}));
                    }
                }
            }
            ItemResult::Error(e) => {
                failed += 1;
                items.push(json!({"file": rel, "result": "error", "detail": e}));
            }
        }
    }

    for (rel, importer) in DOC_ENTRIES {
        // H2：上轮已导入仅剩移动未完成 → 跳过导入，只重试移动（同 KV 组）
        if pending_move.contains(*rel) {
            imported_rels.insert(rel.to_string());
            let path = data_dir.join(rel);
            if !path.exists() {
                pending_move.remove(*rel);
                missing += 1;
                items.push(json!({"file": rel, "result": "missing", "scope": "pending_move"}));
                continue;
            }
            match move_to_backup(data_dir, rel, false) {
                Ok(()) => {
                    pending_move.remove(*rel);
                    imported += 1;
                    items.push(json!({"file": rel, "result": "moved", "scope": "pending_move"}));
                }
                Err(e) => {
                    failed += 1;
                    items.push(json!({"file": rel, "result": "move_failed", "detail": e.to_string()}));
                }
            }
            continue;
        }
        let path = data_dir.join(rel);
        if !path.exists() {
            missing += 1;
            items.push(json!({"file": rel, "result": "missing"}));
            continue;
        }
        match importer(&store, &path) {
            ImportStatus::Ok => {
                imported += 1;
                imported_rels.insert(rel.to_string());
                match move_to_backup(data_dir, rel, false) {
                    Ok(()) => {
                        items.push(json!({"file": rel, "result": "imported"}));
                    }
                    Err(e) => {
                        // H2：导入成功但移动失败 → 记 pending，重跑只补移动
                        pending_move.insert(rel.to_string());
                        failed += 1;
                        items.push(json!({"file": rel, "result": "imported_but_move_failed", "detail": e.to_string()}));
                    }
                }
            }
            ImportStatus::Corrupt(e) => {
                corrupt += 1;
                match move_to_backup(data_dir, rel, true) {
                    Ok(()) => {
                        items.push(json!({"file": rel, "result": "corrupt", "detail": e}));
                    }
                    Err(m) => {
                        failed += 1;
                        items.push(json!({"file": rel, "result": "corrupt_move_failed", "detail": format!("{e}; 移动失败: {m}")}));
                    }
                }
            }
            ImportStatus::Error(e) => {
                failed += 1;
                items.push(json!({"file": rel, "result": "error", "detail": e}));
            }
        }
    }

    for (name, kv_key) in LEGACY_ROOT {
        // H2：上轮已导入仅剩移动未完成 → 跳过导入，只重试移动
        if pending_move.contains(*name) {
            if !data_dir.join(name).exists() {
                pending_move.remove(*name);
                items.push(json!({"file": name, "result": "missing", "scope": "pending_move"}));
                continue;
            }
            match move_legacy_root_to_backup(data_dir, name) {
                Ok(()) => {
                    pending_move.remove(*name);
                    imported += 1;
                    items.push(json!({"file": name, "result": "moved", "scope": "pending_move"}));
                }
                Err(e) => {
                    failed += 1;
                    items.push(json!({"file": name, "result": "move_failed", "detail": e.to_string()}));
                }
            }
            continue;
        }
        let path = data_dir.join(name);
        if !path.exists() {
            continue;
        }
        // 正牌文件（data/ 下同名）已导入 → 根路径残留仅作垃圾清理
        if imported_rels.contains(&format!("data/{name}")) {
            items.push(json!({"file": name, "result": "skipped_stale", "scope": "legacy_root"}));
            if let Err(e) = move_legacy_root_to_backup(data_dir, name) {
                // 垃圾清理失败无数据风险，计入 failed 让下轮重试
                failed += 1;
                items.push(json!({"file": name, "result": "stale_move_failed", "detail": e.to_string()}));
            }
            continue;
        }
        let result = match kv_key {
            Some(key) => import_kv(&store, data_dir, name, key),
            None => {
                let importer = match *name {
                    "checkin_accounts.json" => import_accounts,
                    "wb_sticky_sessions.json" => import_sticky_bindings,
                    _ => import_wb_token_store,
                };
                to_item(run_importer(&store, &path, importer))
            }
        };
        match result {
            ItemResult::Imported => {
                imported += 1;
                items.push(json!({"file": name, "result": "imported", "scope": "legacy_root"}));
                if let Err(e) = move_legacy_root_to_backup(data_dir, name) {
                    pending_move.insert(name.to_string());
                    failed += 1;
                    items.push(json!({"file": name, "result": "imported_but_move_failed", "detail": e.to_string()}));
                }
            }
            ItemResult::Corrupt(_) => {
                corrupt += 1;
                items.push(json!({"file": name, "result": "corrupt", "scope": "legacy_root"}));
                if let Err(e) = move_legacy_root_to_backup(data_dir, name) {
                    failed += 1;
                    items.push(json!({"file": name, "result": "corrupt_move_failed", "detail": e.to_string()}));
                }
            }
            ItemResult::Missing => {}
            ItemResult::Error(e) => {
                failed += 1;
                items.push(json!({"file": name, "result": "error", "detail": e}));
            }
        }
    }

    // 待移动集合持久化（H2）：空集合同样写入，语义 = 无待移动。写失败按迁移
    // 失败计（版本号不置位）——集合丢失会让下轮把已导入未移走的文件误判为
    // 待导入，重新导入覆盖运行期 kv
    if let Err(e) = save_pending_move(&store, &pending_move) {
        failed += 1;
        items.push(json!({"file": "<pending_move>", "result": "error", "detail": e}));
    }
    // 全部条目处理完且零失败才置版本号（有失败保留原位/或记入待移动集合，
    // 下次启动重试）。设计取舍：单文件失败 → 整体重跑；「已导入但移动失败」
    // 由 pending 集合保护、跳过重导仅补移动（H2），其余已移走文件记 missing
    // 幂等无害；代价是失败期间每次启动重写 manifest，属可接受日志噪音。
    if failed == 0 {
        // P6 v1→v2 增量：老库 kv 中三个流水键搬入表（v0 全新导入路径键已由 DOC_ENTRIES 处理，此处空转）
        if let Err(e) = migrate_kv_flows_v1_to_v2(&store) {
            failed += 1;
            items.push(json!({"file": "<kv_flows_v1_to_v2>", "result": "error", "detail": e}));
        }
    }
    if failed == 0 {
        // v3→v4 增量：三套独立分组定义统一为共享分组（shg_ 新 ID + id_map 全量重定向）；
        // kv 标记幂等，全新安装空转
        match migrate_unified_groups(&store) {
            Ok(n) if n > 0 => {
                items.push(json!({"file": "<unified_groups>", "result": "imported", "detail": format!("统一分组 {n} 条")}));
                imported += 1;
            }
            Ok(_) => {}
            Err(e) => {
                failed += 1;
                items.push(json!({"file": "<unified_groups>", "result": "error", "detail": e}));
            }
        }
    }
    if failed == 0 {
        // M6 修复（审查）：版本号写失败不再静默——文件已移 backup，下轮空转
        // 重试（幂等无害），但吞错会掩盖库异常
        if let Err(e) = store.with_conn(|c| {
            schema::set_user_version(c, schema::SCHEMA_VERSION);
            Ok(())
        }) {
            crate::fs_utils::app_log(
                data_dir,
                &format!("[迁移] user_version 写入失败（下轮空转重试）: {e}"),
            );
        }
    }

    // 明细清单落 backup/
    let manifest = json!({
        "started_at": crate::fs_utils::now_ts(),
        "schema_version": schema::SCHEMA_VERSION,
        "imported": imported,
        "missing": missing,
        "corrupt": corrupt,
        "failed": failed,
        "items": items,
    });
    let backup_dir = data_dir.join("data").join("backup");
    let _ = std::fs::create_dir_all(&backup_dir);
    let _ = crate::fs_utils::write_json(&backup_dir.join("migration_manifest.json"), &manifest);

    Some(format!(
        "SQLite 迁移完成：导入 {imported}，缺失 {missing}，损坏 {corrupt}，失败 {failed}（明细见 data/backup/migration_manifest.json）"
    ))
}

enum ItemResult {
    Imported,
    Missing,
    Corrupt(String),
    Error(String),
}

/// KV 导入：文件文本校验为合法 JSON 后入库（serde 解析→重序列化，键序规范化，
/// 语义与原文件等价；解析失败 = Corrupt）
fn import_kv(store: &Store, data_dir: &Path, rel: &str, key: &str) -> ItemResult {
    let path = data_dir.join(rel);
    if !path.exists() {
        return ItemResult::Missing;
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return ItemResult::Corrupt("空文件".into());
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v) => match store.kv_set_raw(key, &v.to_string()) {
                    Ok(()) => ItemResult::Imported,
                    Err(e) => ItemResult::Error(e),
                },
                Err(e) => ItemResult::Corrupt(format!("JSON 解析失败: {e}")),
            }
        }
        Err(e) => ItemResult::Error(format!("读取失败: {e}")),
    }
}

fn run_importer(store: &Store, path: &Path, f: fn(&Store, &Path) -> ImportStatus) -> ImportStatus {
    f(store, path)
}

fn to_item(s: ImportStatus) -> ItemResult {
    match s {
        ImportStatus::Ok => ItemResult::Imported,
        ImportStatus::Corrupt(e) => ItemResult::Corrupt(e),
        ImportStatus::Error(e) => ItemResult::Error(e),
    }
}

/// H2：待移动集合 kv 键（导入成功但 rename 失败的文件；见 migrate_on_startup）
const MIGRATION_PENDING_MOVE: &str = "migration_pending_move";

fn load_pending_move(store: &Store) -> Result<std::collections::BTreeSet<String>, String> {
    match store.kv_get_raw_checked(MIGRATION_PENDING_MOVE)? {
        None => Ok(Default::default()),
        Some(raw) => serde_json::from_str(&raw).map_err(|e| format!("解析失败: {e}")),
    }
}

fn save_pending_move(
    store: &Store,
    set: &std::collections::BTreeSet<String>,
) -> Result<(), String> {
    let text =
        serde_json::to_string(set).map_err(|e| format!("序列化失败: {e}"))?;
    store.kv_set_raw(MIGRATION_PENDING_MOVE, &text)
}

/// 原文件移入 backup/（保留 conf/data 相对路径前缀；corrupt=true 时入 backup/corrupt/）。
/// H2 修复（审查）：rename 失败不再静默吞掉——上抛由调用方计入 failed 并记入
/// 待移动集合，防止重跑重导覆盖运行期 kv
fn move_to_backup(data_dir: &Path, rel: &str, corrupt: bool) -> std::io::Result<()> {
    let src = data_dir.join(rel);
    let file_name = Path::new(rel)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| rel.to_string());
    let backup_dir = data_dir
        .join("data")
        .join("backup")
        .join(if corrupt { "corrupt" } else { "" });
    let dst = backup_dir.join(rel);
    std::fs::create_dir_all(dst.parent().unwrap_or(&backup_dir))?;
    if dst.exists() {
        // 同名冲突：加时间戳后缀，绝不覆盖既有备份
        let stamped = format!(
            "{}_{}",
            file_name.trim_end_matches(".json"),
            chrono::Local::now().format("%Y%m%d%H%M%S")
        );
        return std::fs::rename(&src, dst.with_file_name(format!("{stamped}.json")));
    }
    std::fs::rename(&src, dst)
}

/// 遗留根路径文件 → backup/legacy_root/（H2：rename 失败上抛）
fn move_legacy_root_to_backup(data_dir: &Path, name: &str) -> std::io::Result<()> {
    let src = data_dir.join(name);
    let dir = data_dir.join("data").join("backup").join("legacy_root");
    std::fs::create_dir_all(&dir)?;
    std::fs::rename(&src, dir.join(name))
}

// ── 结构化导入器（读原 struct → docs::save；解析失败 = Corrupt）──────────────

/// raw 形态导入（保真）：device_proxy 会向账号 JSON 写入 struct 外扩展字段
///（refresh_token_updated_at 等，typed roundtrip 会丢），与 accounts_save_raw
/// 运行时语义对齐；整文件 JSON 解析失败仍判 Corrupt。
fn import_accounts(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::accounts_save_raw(store, &v))
}

fn import_device_map(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| {
        let map: crate::models::DeviceMap = serde_json::from_value(v).map_err(ser_err)?;
        docs::device_map_save(store, &map)
    })
}

/// 旧版 groups.json 导入（经 DOC_ENTRIES 表接入旧目录迁移链路）。不持
/// org_defs_lock/group_defs_lock：调用点均为启动期旧目录一次性导入、单线程
/// 顺序执行，早于任何并发写路径（调度器/网关/命令层均未启动）；groups_save
/// 内部仍取 org_defs_lock，与未来可能的并发写入口保持互斥底线
fn import_groups(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::GroupsFile>(&text) {
            Ok(f) => wrap(docs::groups_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_remaining_credits(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::RemainingCreditsFile>(&text) {
            Ok(f) => wrap(docs::remaining_credits_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_account_cooldowns(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::AccountCooldownsFile>(&text) {
            Ok(f) => wrap(docs::account_cooldowns_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_pay_status(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| {
        let f: crate::commands::trae_apps::PayStatusFile =
            serde_json::from_value(v).map_err(ser_err)?;
        docs::pay_status_save(store, &f)
    })
}

fn import_api_keys(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::api_server::api_keys::ApiKeysFile>(&text) {
            Ok(f) => wrap(docs::api_keys_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_custom_models(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::api_server::custom_models::CustomModelsFile>(&text) {
            Ok(f) => wrap(docs::custom_models_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_doubao_pool(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            match serde_json::from_str::<crate::commands::doubao::DoubaoAccountPool>(&text) {
                Ok(pool) => wrap(docs::doubao_pool_save(store, &pool)),
                // DoubaoAccountPool 序列化含 private 字段默认值兜底，解析失败按损坏处理
                Err(e) => ImportStatus::Corrupt(e.to_string()),
            }
        }
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_wb_pool(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_pool_save(store, &v))
}

fn import_wb_token_store(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_token_store_save(store, &v))
}

fn import_api_usage(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::api_server::usage::UsageFile>(&text) {
            // 审查修复（M8）：原实现直接 api_usage_save 全量替换（DELETE 后按 JSON
            // 重插）——残留旧 JSON 重放迁移时（备份还原 data/ 等场景），JSON 缺失
            // 的桶（如升级后新增的 buddy-ai）连同运行期新增天数会被整表清掉。
            // 改为合并导入：仅补库中缺失的 (bucket, day)，库已有行保留
            //（库为运行期新数据，优先于迁移源；首次迁移库为空时语义不变）
            Ok(f) => {
                let mut cur = docs::api_usage_load(store);
                for (src, dst) in [
                    (&f.days, &mut cur.days),
                    (&f.wb_days, &mut cur.wb_days),
                    (&f.custom_days, &mut cur.custom_days),
                    (&f.qoder_days, &mut cur.qoder_days),
                    (&f.buddy_ai_days, &mut cur.buddy_ai_days),
                    (&f.codearts_days, &mut cur.codearts_days),
                ] {
                    for (day, stats) in src {
                        dst.entry(day.clone()).or_insert_with(|| stats.clone());
                    }
                }
                wrap(docs::api_usage_save(store, &cur))
            }
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_credits_history(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::CreditsFile>(&text) {
            Ok(f) => wrap(docs::credits_history_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_credits_daily(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::CreditsDailyFile>(&text) {
            Ok(f) => wrap(docs::credits_daily_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_checkin_results(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::checkin_results::ResultsFile>(&text) {
            Ok(f) => wrap(docs::checkin_results_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_wb_checkin_results(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_checkin_results_save(store, &v))
}

fn import_doubao_health(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| {
        let empty = Vec::new();
        let events = v.get("events").and_then(Value::as_array).unwrap_or(&empty);
        docs::doubao_health_save(store, events)
    })
}

// ── P6 流水迁出导入器 ────────────────────────────────────────────────────────

fn import_wb_credits_history(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_credits_history_save(store, &v))
}

fn import_usage_history(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::usage_history_save(store, &v))
}

fn import_sticky_bindings(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::sticky_bindings_save(store, &v))
}

/// v1→v2 增量迁移：kv 中三个流水键搬入对应表（v1 老库升级路径）
fn migrate_kv_flows_v1_to_v2(store: &Store) -> Result<(), String> {
    let importers: [(&str, fn(&Store, &Value) -> Result<(), String>); 3] = [
        ("workbuddy_credits_history", import_wb_credits_history_value),
        ("usage_history", import_usage_history_value),
        ("wb_sticky_sessions", import_sticky_bindings_value),
    ];
    for (key, importer) in importers {
        // R1：走严格版读取——无键（全新安装）才跳过，读库失败显式上抛中止迁移
        //（kv_get_raw 把读失败也折成 None → 静默跳过，流水键残留不迁移）
        let Some(text) = store.kv_get_raw_checked(key)? else {
            continue; // v1 老库无此键（全新安装）→ 跳过
        };
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("kv {key} 解析失败: {e}"))?;
        importer(store, &v)?;
        store.kv_delete(key)?;
    }
    Ok(())
}

fn import_wb_credits_history_value(store: &Store, v: &Value) -> Result<(), String> {
    docs::wb_credits_history_save(store, v)
}

// ── v3→v4：三套分组定义统一为跨应用共享分组（shg_ 新 ID）─────────────────────

/// 三套分组定义统一（shared-groups-tags-design.md D2，已确认 2026-10-10）：
/// 1. 读三处 defs（groups 表 + kv workbuddy_groups + kv qoder_groups），
///    迁移前整体快照至 kv("groups_pre_migration") 可回滚；
/// 2. 按 name 去重合并（同名即同组，颜色/排序取 Trae > Buddy > Qoder 首个出现），
///    全部重新生成 `shg_` 新 ID（不保留任何旧应用 id），构建 id_map 全量重定向；
/// 3. 全删全写 groups 表（defs 先于重定向落库，保证崩溃重跑按 name 复用既有
///    shg_ id，引用不悬空）；
/// 4. 重定向 group_members 表、WB/Qoder 账号 group_id 字段、
///    api_pool.group_ids/wb_group_ids/qoder_group_ids；
/// 5. 删除 kv workbuddy_groups / qoder_groups；
/// 6. kv("unified_groups_migrated") 标记幂等，二次启动跳过。
/// 容错（审查修复）：中途失败重试时，① 快照不覆盖（只写一次保原始备份）；
/// ② 复用 groups 表已残留的 shg_ 定义 id，重定向引用不悬空。
/// 返回统一后的分组条数（0 = 幂等跳过）。
fn migrate_unified_groups(store: &Store) -> Result<usize, String> {
    use crate::models::Group;
    if store.kv_get::<bool>("unified_groups_migrated") {
        return Ok(0);
    }
    // 全程持 org_defs_lock（2026-10-12 审查 P4）：与运行期分组/标签写入口
    // （docs 层同锁）互斥，迁移的 defs 读-改-写不再与并发写互相覆盖。
    // 局限：进程内锁对 CLI 双进程无效——迁移仅启动期执行且有幂等标记，
    // 风险有限，文件锁暂不实施。内部 group_defs_save 走 unlocked 版防重入死锁
    let _org_guard = crate::store::docs::org_defs_lock();
    // 1. 读三处 defs + Trae membership，整体快照备份（可回滚）。
    //    快照只写一次（审查修复）：中途失败重试时若覆盖快照，备份会被
    //    「已部分重定向」的状态污染，恰在回滚最需要时失效。
    // 源读 fail-closed（审查修复清单 v1.4.9 B2）：三处源读失败显式上抛中止迁移——
    // 原实现静默空集会让「全删全写 defs / 删 kv」重创性迁移在残缺快照上误执行
    //（且 groups_pre_migration 备份会固化残缺态）。失败即中止，下次启动重试
    let trae_all: Vec<Group> = store
        .rows_all("groups")?
        .into_iter()
        .filter_map(|(_, data)| serde_json::from_value(data).ok())
        .collect();
    // 上次中途失败残留的 shg_ 定义不算 Trae 旧定义（迁移计数/备份口径均剔除）
    let trae: Vec<Group> = trae_all
        .iter()
        .filter(|g| !g.id.starts_with("shg_"))
        .cloned()
        .collect();
    // 两池私有 kv：缺失（None）= 合法空集；读取失败（Err）与解析失败显式上抛
    //（R1：走 kv_get_raw_checked 严格版——kv_get/kv_get_raw 均把读失败与缺失
    // 合并为默认值/None，fail-open）
    let kv_groups = |key: &str| -> Result<Option<Vec<Group>>, String> {
        store
            .kv_get_raw_checked(key)?
            .map(|raw| serde_json::from_str(&raw).map_err(|e| format!("{key} 解析失败: {e}")))
            .transpose()
    };
    let wb: Vec<Group> = kv_groups("workbuddy_groups")?.unwrap_or_default();
    let qoder: Vec<Group> = kv_groups("qoder_groups")?.unwrap_or_default();
    let member_rows: Vec<(String, String)> = store
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT uid, group_id FROM group_members")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
    let members_before: std::collections::HashMap<String, String> =
        member_rows.iter().cloned().collect();
    // R1：备份判断走严格版——读失败上抛中止（kv_get_raw 把读失败折成 None，
    // 会覆盖重写备份，污染「只写一次」的原始备份红线）；仅确认无键时才写
    if store.kv_get_raw_checked("groups_pre_migration")?.is_none() {
        store.kv_set(
            "groups_pre_migration",
            &json!({
                "groups": trae_all,
                "workbuddy_groups": wb,
                "qoder_groups": qoder,
                "group_members": members_before,
            }),
        )?;
    }

    // 2. 按 name 合并 + 生成统一新 ID（id_map 覆盖全部旧 id，含同名多组）。
    //    中途失败重试场景（审查修复）：groups 表可能残留上次生成的 shg_ 定义
    //    （重定向已生效但 defs 全删全写失败、标记未置位）——按 name 复用既有
    //    shg_ id，保证已重定向的成员/账号/池筛选引用不悬空。
    let mut new_defs: Vec<Group> = trae_all
        .into_iter()
        .filter(|g| g.id.starts_with("shg_"))
        .collect();
    let mut id_map: std::collections::HashMap<String, String> = Default::default();
    let mut name_to_new: std::collections::HashMap<String, String> = new_defs
        .iter()
        .map(|g| (g.name.clone(), g.id.clone()))
        .collect();
    for g in [&trae, &wb, &qoder].into_iter().flatten() {
        if let Some(nid) = name_to_new.get(&g.name) {
            id_map.insert(g.id.clone(), nid.clone());
            continue;
        }
        let rand = uuid::Uuid::new_v4().simple().to_string();
        let nid = format!("shg_{}{}", chrono::Local::now().timestamp_millis(), &rand[..4]);
        name_to_new.insert(g.name.clone(), nid.clone());
        id_map.insert(g.id.clone(), nid.clone());
        new_defs.push(Group { id: nid, name: g.name.clone(), color: g.color.clone(), order: g.order });
    }
    // order 重排（合并后保持出现顺序 1..n）
    for (i, g) in new_defs.iter_mut().enumerate() {
        g.order = (i + 1) as i32;
    }

    // 3. defs 先落库（崩溃窗口修复）：重定向引用**之前**先把统一 defs 全删全写进
    //    groups 表——此后任一步崩溃，重跑经上方「残留 shg_ 定义按 name 复用」
    //    路径拿到同一 id，已重定向的引用不悬空。原实现 defs 在重定向后才落库，
    //    重定向完成至落库之间崩溃时，重跑会生成全新 shg_ id，指向旧 id 的
    //    成员/账号/池筛选引用全部悬空（且重名校验还会拒绝同名新组）。
    docs::group_defs_save_unlocked(store, &new_defs)?;

    // 4. id_map 全量重定向（逐条 UPDATE，旧 id 不存在时 0 行受影响，幂等无害）
    for (old, new) in &id_map {
        if old == new {
            continue;
        }
        store.with_conn(|c| {
            c.execute(
                "UPDATE group_members SET group_id = ?1 WHERE group_id = ?2",
                rusqlite::params![new, old],
            )?;
            Ok(())
        })?;
    }
    redirect_pool_group_ids(store, "wb_accounts", &id_map)?;
    redirect_pool_group_ids(store, "qoder_accounts", &id_map)?;
    redirect_api_pool_group_ids(store, &id_map)?;

    // 5. 删除三套私有 kv；6. 置幂等标记（defs 已在步骤 3 落库）
    store.kv_delete("workbuddy_groups")?;
    store.kv_delete("qoder_groups")?;
    store.kv_set("unified_groups_migrated", &true)?;
    // 一次性启动通知（前端 init 拉取 toast 后即清）：合并结果用户可见，不只落日志
    let _ = store.kv_set(
        "startup_notice",
        &format!(
            "分组已升级为跨应用共享：原 {} 条分组定义合并为 {} 条共享分组（同名自动合并），\
             成员与调度筛选已同步重定向；原定义已备份，可在系统日志查看详情",
            trae.len() + wb.len() + qoder.len(),
            new_defs.len()
        ),
    );
    crate::fs_utils::app_log(
        &store.data_dir,
        &format!(
            "分组统一迁移：{} 条定义合并为 {} 条共享分组（shg_ 新 ID），成员/池筛选引用已全量重定向；原三套定义快照见 kv groups_pre_migration",
            trae.len() + wb.len() + qoder.len(),
            new_defs.len()
        ),
    );
    Ok(new_defs.len())
}

/// 账号池表（wb_accounts/qoder_accounts 行文档，data 内 group_id 字段）重定向
fn redirect_pool_group_ids(
    store: &Store,
    table: &str,
    id_map: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    // R1 fail-closed（对齐 migrate_unified_groups 的 kv 源读取红线）：读库失败
    // 显式上抛中止迁移——若折成空集静默跳过，旧 group_id 在 kv 定义删除、幂等
    // 标记置位后永久悬空且不再重试（一次性数据损坏）
    let rows: Vec<(String, Value)> = store
        .rows_all(table)?
        .into_iter()
        .filter(|(_, data)| {
            data.get("group_id")
                .and_then(Value::as_str)
                .map(|g| id_map.contains_key(g))
                .unwrap_or(false)
        })
        .map(|(pk, mut data)| {
            if let Some(g) = data.get("group_id").and_then(Value::as_str) {
                let new = id_map.get(g).cloned().unwrap_or_else(|| g.to_string());
                data["group_id"] = json!(new);
            }
            (pk, data)
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    for (pk, data) in &rows {
        let text = serde_json::to_string(data).map_err(|e| format!("序列化失败: {e}"))?;
        store.with_conn(move |c| {
            c.execute(
                &format!("UPDATE {table} SET data = ?1 WHERE pk = ?2"),
                rusqlite::params![text, pk],
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

/// api_pool kv 的 group_ids / wb_group_ids / qoder_group_ids 重定向
fn redirect_api_pool_group_ids(
    store: &Store,
    id_map: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    // R1：无键（未配置 api_pool）才跳过重定向；读库失败显式上抛——静默 Ok 会让
    // api_pool 引用残留旧分组 id（悬空），且迁移幂等标记已置位不再重试
    let Some(raw) = store.kv_get_raw_checked("api_pool")? else {
        return Ok(());
    };
    let mut v: Value = serde_json::from_str(&raw).map_err(|e| format!("api_pool 解析失败: {e}"))?;
    let mut changed = false;
    for field in ["group_ids", "wb_group_ids", "qoder_group_ids"] {
        if let Some(arr) = v.get_mut(field).and_then(Value::as_array_mut) {
            for item in arr.iter_mut() {
                if let Some(old) = item.as_str() {
                    if let Some(new) = id_map.get(old) {
                        *item = json!(new);
                        changed = true;
                    }
                }
            }
        }
    }
    if changed {
        let text = serde_json::to_string(&v).map_err(|e| format!("序列化失败: {e}"))?;
        store.kv_set_raw("api_pool", &text)?;
    }
    Ok(())
}

fn import_usage_history_value(store: &Store, v: &Value) -> Result<(), String> {
    docs::usage_history_save(store, v)
}

fn import_sticky_bindings_value(store: &Store, v: &Value) -> Result<(), String> {
    docs::sticky_bindings_save(store, v)
}

// ── TraeWork 双应用合并存量迁移（方案 §5.8）─────────────────────────────────

const TRAEWORK_MERGE_MARKER: &str = "traework_merge_migrated";

/// 升级引导报告 kv 键：迁移完成后写入（前端首启弹「升级引导」展示迁移流程与结果，
/// 关闭后由独立「已读」标记不再展示，报告本身保留）
const TRAEWORK_MERGE_REPORT: &str = "traework_merge_report";

/// TraeWork 合并迁移升级引导报告（迁移明细 + 槽组计数，供前端升级引导弹窗）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TraeWorkMigrationReport {
    pub finished_at_ms: i64,
    /// 迁移明细条目（settings 步 note + 快照池步摘要，用户可读文案）
    pub notes: Vec<String>,
    /// settings 步处置结果的结构化字段（前端「迁移安装路径设置」步直接消费，
    /// 不再对 notes 做子串匹配——后端改 note 文案不会误显「已跳过」；
    /// None = 无 trae_path 存量或迁移未跑该步）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_note: Option<String>,
    /// 整组迁入 profiles_trae 的组数
    pub moved_groups: usize,
    /// 同名冲突整组保留至 profiles_trae_work_legacy 的组数
    pub legacy_groups: usize,
}

/// 双应用合并存量迁移入口（每次启动调用）：
/// ① settings kv `trae_path` 退役（可承接为 `trae_cn_path`）；
/// ② 旧快照池 `data/profiles` 按槽整组搬迁至 `data/profiles_trae`（目标主槽已存在
///    → 整组入 `data/profiles_trae_work_legacy` 保留，绝不覆盖）；
/// ③ kv 标记 `traework_merge_migrated` 幂等；④ 真正空了的旧池目录删除。
///
/// 安全语义：Trae 客户端运行中则跳过（有存量才提示，下次启动重试）；移动而非复制，
/// 任一步失败即中止并保留原状，下次启动重试。返回摘要由调用方写日志；无事发生返回 None。
pub fn migrate_traework_merge(data_dir: &Path) -> Option<String> {
    let store = db(data_dir);
    // M7 修复（审查）：幂等标记读取失败不再 fail-open（按未迁移处理）——库异常时
    // 盲目重跑迁移徒增风险；读失败落日志跳过本轮，下轮再试
    match store.kv_get_raw_checked(TRAEWORK_MERGE_MARKER) {
        Ok(Some(raw)) => {
            if raw.parse::<bool>().unwrap_or(false) {
                return None;
            }
        }
        Ok(None) => {}
        Err(e) => {
            crate::fs_utils::app_log(
                data_dir,
                &format!("[TraeWork 合并迁移] 幂等标记读取失败，跳过本轮: {e}"),
            );
            return None;
        }
    }
    if crate::switcher::proc::any_running(&["Trae CN", "TRAE SOLO CN"]) {
        // 运行中：仅当确有存量才提示跳过，否则静默（下次启动廉价复查，不置标记）。
        // 旧 TRAE SOLO CN 一并检测：用户尚未卸载旧客户端且正在运行时同样暂缓搬迁，
        // 保守对齐 §5.8「复用优雅关闭语义」
        if traework_merge_pending(&store, data_dir) {
            return Some("TraeWork 合并迁移跳过：Trae 客户端正在运行，将在下次启动重试".into());
        }
        return None;
    }
    migrate_traework_merge_inner(&store, data_dir)
}

/// 迁移主体（无进程检测，供单测直调）：settings 步 + 快照池步，全部成功才置标记。
fn migrate_traework_merge_inner(store: &Store, data_dir: &Path) -> Option<String> {
    let mut notes: Vec<String> = Vec::new();
    let mut settings_note: Option<String> = None;
    match migrate_traework_settings(store) {
        Ok(Some(note)) => {
            settings_note = Some(note.clone());
            notes.push(note);
        }
        Ok(None) => {}
        Err(e) => {
            return Some(format!(
                "TraeWork 合并迁移中止（settings）：{e}；原状保留，下次启动重试"
            ))
        }
    }
    let (moved_groups, legacy_groups) = match migrate_traework_pool(data_dir) {
        Ok((moved, legacy)) => {
            if moved > 0 || legacy > 0 {
                notes.push(format!("快照槽迁入 profiles_trae {moved} 组、legacy 保留 {legacy} 组"))
            }
            (moved, legacy)
        }
        Err(e) => {
            return Some(format!(
                "TraeWork 合并迁移中止（快照池）：{e}；原状保留，下次启动重试"
            ))
        }
    };
    // 升级引导报告（仅完成时落，中止/跳过不落——下次启动重试成功后才有）：前端首启
    // 弹窗展示迁移流程与结果。写失败仅无引导，不影响迁移结果
    if !notes.is_empty() {
        // M8 修复（审查）：报告写失败仅无引导弹窗、不影响迁移结果——但不再
        // 静默，落日志可诊断
        if let Err(e) = store.kv_set(
            TRAEWORK_MERGE_REPORT,
            &TraeWorkMigrationReport {
                finished_at_ms: chrono::Utc::now().timestamp_millis(),
                notes: notes.clone(),
                settings_note: settings_note.clone(),
                moved_groups,
                legacy_groups,
            },
        ) {
            crate::fs_utils::app_log(
                data_dir,
                &format!("[TraeWork 合并迁移] 升级引导报告写入失败（仅无引导弹窗）: {e}"),
            );
        }
    }
    // 标记写失败：下次重跑两步均为空操作后补置——落日志留痕（M8）
    if let Err(e) = store.kv_set(TRAEWORK_MERGE_MARKER, &true) {
        crate::fs_utils::app_log(
            data_dir,
            &format!("[TraeWork 合并迁移] 幂等标记写入失败（下次启动空转重试）: {e}"),
        );
    }
    if notes.is_empty() {
        return None;
    }
    Some(format!("TraeWork 合并迁移完成：{}", notes.join("；")))
}

/// Trae 运行中时的存量探测：settings 含 `trae_path` 键（读取/解析失败按有存量处理）
/// 或旧快照池非空 → true
fn traework_merge_pending(store: &Store, data_dir: &Path) -> bool {
    match store.kv_get_raw_checked("app_settings") {
        Ok(Some(raw)) => {
            if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(&raw) {
                if o.contains_key("trae_path") {
                    return true;
                }
            }
        }
        // 读失败（数据库异常等）fail-closed 按有存量处理，等下轮重试；
        // Ok(None)（全新安装无 app_settings）不算存量，继续看旧池目录
        Err(_) => return true,
        Ok(None) => {}
    }
    // M9 修复（审查）：仅计槽形态条目（目录或 .meta.json sidecar，排除
    // current_account* 运行时标记）——原 rd.next().is_some() 对任意杂散文件
    // 为真：只剩运行时标记时 Trae 运行中每次启动都误报「有存量，跳过」
    match std::fs::read_dir(data_dir.join("data").join("profiles")) {
        Ok(rd) => rd.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            !name.starts_with("current_account")
                && (e.path().is_dir() || name.ends_with(".meta.json"))
        }),
        Err(_) => false,
    }
}

/// settings 步：从 kv `app_settings` 原文移除 `trae_path`；若其非空、`trae_cn_path`
/// 为空/缺失且路径存在为文件 → 承接为 `trae_cn_path`（绝不覆盖既有值）。
/// 键缺失或 app_settings 非对象 → Ok(None)（不动）。回写失败上抛，原值保留。
fn migrate_traework_settings(store: &Store) -> Result<Option<String>, String> {
    let raw = match store.kv_get_raw_checked("app_settings")? {
        Some(r) => r,
        None => return Ok(None),
    };
    let mut obj = match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(o)) => o,
        _ => return Ok(None),
    };
    // M10 修复（审查）：消除 unwrap（受 is_none 保护仅风格问题，但违反零 unwrap 红线）
    let Some(old) = obj.remove("trae_path") else {
        return Ok(None);
    };
    let mut note = "trae_path 键已删除".to_string();
    if let Some(p) = old.as_str() {
        if !p.trim().is_empty() {
            let cn = obj.get("trae_cn_path").and_then(|v| v.as_str()).unwrap_or("");
            if cn.trim().is_empty() && Path::new(p).is_file() {
                // 承接守卫：trae_path 是退役 Trae Work 的专属键，其值可能指向未卸载的
                // 旧客户端（TRAE SOLO CN.exe / 旧版 ...\Programs\Trae\Trae.exe）——承接会让
                // 「打开 Trae」静默拉起旧 exe，且 app_locate 一级命中压制自动探测。按文件名
                // 拒绝旧客户端名单（solo 形态 + 旧版 Trae.exe），留空走 env.rs 自动探测
                // （默认路径/注册表/进程）
                let fname = Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if fname.contains("solo") || fname == "trae.exe" {
                    note = "trae_path 指向已退役的旧 Trae 客户端，已丢弃（不承接，走自动探测）".to_string();
                } else {
                    obj.insert("trae_cn_path".into(), json!(p));
                    note = "trae_path 已承接为 trae_cn_path（原键删除）".to_string();
                }
            }
        }
    }
    let out =
        serde_json::to_string(&Value::Object(obj)).map_err(|e| format!("序列化失败: {e}"))?;
    store.kv_set_raw("app_settings", &out)?;
    Ok(Some(note))
}

/// 快照池步：`data/profiles` 内按槽 base 名整组搬迁（主槽目录 + `<base>.meta.json`
/// sidecar + `<base>.bak`/`.bak2` 代际）；`current_account*` 运行时标记与杂散文件
/// 留原池。目标主槽已存在且组内仍含主槽条目（新旧同 uid 冲突）→ 整组入
/// `profiles_trae_work_legacy`；否则入 `profiles_trae`（含上轮部分成功后仅剩
/// sidecar/代际的补齐场景）并 best-effort 写入 sidecar `source: TraeWork-migrated`。
/// 返回 (迁入组数, legacy 组数)。
fn migrate_traework_pool(data_dir: &Path) -> Result<(usize, usize), String> {
    let old_pool = data_dir.join("data").join("profiles");
    if !old_pool.exists() {
        return Ok((0, 0));
    }
    let new_pool = data_dir.join("data").join("profiles_trae");
    let legacy_pool = data_dir.join("data").join("profiles_trae_work_legacy");
    let mut groups: std::collections::BTreeMap<String, Vec<std::path::PathBuf>> = Default::default();
    let rd = std::fs::read_dir(&old_pool).map_err(|e| format!("读取旧快照池失败: {e}"))?;
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("current_account") {
            continue; // 运行时标记留旧池，不属于任何槽
        }
        if p.is_dir() || name.ends_with(".meta.json") {
            groups.entry(slot_base_name(&name)).or_default().push(p);
        }
        // 其他杂散文件留原池
    }
    let mut moved = 0usize;
    let mut legacy = 0usize;
    for (base, items) in groups {
        // 冲突判定：① 目标主槽存在且本次组内仍含主槽条目（新旧同 uid 真冲突）→ 整组入
        // legacy。② legacy 池已存在该 base 主槽（上轮已判冲突整组入 legacy 后，本轮
        // 只剩 sidecar/代际残留）→ 剩余条目必须继续入 legacy：若按「补齐」入新池会
        // 把同组拆散、孤儿 sidecar 散落新池根部。③ 主槽不在 items 且 legacy 无主槽
        // （上轮已迁入新池）才是真正的补齐：剩余 sidecar/代际迁入新池，
        // rename_no_clobber 防覆盖
        let has_main = items
            .iter()
            .any(|p| p.file_name().map(|n| n.to_string_lossy() == base).unwrap_or(false));
        let conflict = (has_main && new_pool.join(&base).exists())
            || legacy_pool.join(&base).exists();
        let dst_root = if conflict { &legacy_pool } else { &new_pool };
        std::fs::create_dir_all(dst_root).map_err(|e| format!("创建目标池失败: {e}"))?;
        for src in &items {
            let name = src.file_name().unwrap_or_default().to_string_lossy().to_string();
            rename_no_clobber(src, &dst_root.join(&name))
                .map_err(|e| format!("搬迁 {name} 失败: {e}"))?;
        }
        if conflict {
            legacy += 1;
        } else {
            moved += 1;
            // 身份标注 best-effort：失败不影响迁移结果（标注池根 sidecar；
            // 审查修复 2026-10-14：原指向槽内 meta.json，生产 icube 布局
            // 槽内从无该文件（白名单不含根级 meta.json），恒 no-op）
            annotate_traework_source(&new_pool.join(format!("{base}.meta.json")));
        }
    }
    // 仅剩运行时标记等杂项时保留；真正空了则删除空目录
    try_remove_empty_pool(&old_pool);
    Ok((moved, legacy))
}

/// 槽 base 名：依次剥 `.meta.json` sidecar 与 `.bak2`/`.bak` 代际后缀
fn slot_base_name(name: &str) -> String {
    let mut s = name.to_string();
    for suf in [".meta.json", ".bak2", ".bak"] {
        if let Some(stripped) = s.strip_suffix(suf) {
            s = stripped.to_string();
            break;
        }
    }
    s
}

/// 移动且绝不覆盖：目标已存在（重试残留等）时加时间戳后缀；时间戳撞名（同秒
/// 迁移两个同名文件）时递增序号后缀（.2/.3/…）直至找到空位——Windows rename
/// 会静默覆盖已存在目标，仅秒级时间戳时同秒第二个文件会吞掉第一个（审查 P2）
fn rename_no_clobber(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.exists() {
        return std::fs::rename(src, dst);
    }
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let stamped = format!("{}_{}", name, chrono::Local::now().format("%Y%m%d%H%M%S"));
    let mut cand = dst.with_file_name(&stamped);
    let mut seq = 2u32;
    while cand.exists() {
        cand = dst.with_file_name(format!("{stamped}.{seq}"));
        seq += 1;
    }
    std::fs::rename(src, cand)
}

/// 槽身份标注（best-effort）：已有池根 sidecar `<base>.meta.json` 时以原子替换写入
/// `source: TraeWork-migrated`（临时文件 + rename，防写盘中断损坏 sidecar）；
/// 槽位本无 sidecar（2026-09-29 sidecar 机制引入前的旧快照）则跳过，不凭空创建。
/// 审查修复 2026-10-14：原实现读槽目录内的 meta.json——生产 icube 布局槽内从无
/// 该文件（backup_icube 只拷 15 项白名单，sidecar 位于池根），标注恒 no-op
fn annotate_traework_source(sidecar: &Path) {
    let Ok(t) = std::fs::read_to_string(sidecar) else {
        return;
    };
    let mut v = serde_json::from_str::<Value>(&t).unwrap_or_else(|_| json!({}));
    if !v.is_object() {
        v = json!({});
    }
    v["source"] = json!("TraeWork-migrated");
    if let Ok(s) = serde_json::to_string(&v) {
        let tmp = sidecar.with_extension("json.aiwork-tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, sidecar);
        }
    }
}

/// 旧池清理：目录不存在或非空时不动；确为空目录才删除
fn try_remove_empty_pool(pool: &Path) {
    match std::fs::read_dir(pool) {
        Ok(mut rd) => {
            if rd.next().is_none() {
                let _ = std::fs::remove_dir(pool);
            }
        }
        Err(_) => {}
    }
}

// ── 工具 ─────────────────────────────────────────────────────────────────────

fn parse_then(
    path: &Path,
    f: impl FnOnce(Value) -> Result<(), String>,
) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => match f(v) {
                Ok(()) => ImportStatus::Ok,
                Err(e) => ImportStatus::Error(e),
            },
            Err(e) => ImportStatus::Corrupt(format!("JSON 解析失败: {e}")),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

/// pay_status 等 struct 导入的 serde 错误转换（parse_then 闭包用）
fn ser_err(e: serde_json::Error) -> String {
    e.to_string()
}

fn wrap(r: Result<(), String>) -> ImportStatus {
    match r {
        Ok(()) => ImportStatus::Ok,
        Err(e) => ImportStatus::Error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("twa_migrate_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("conf")).unwrap();
        std::fs::create_dir_all(d.join("data")).unwrap();
        d
    }

    fn write(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn migrate_imports_moves_and_is_idempotent() {
        let dir = tmp_dir("full");
        // 造三类数据：KV、行文档、列化流水 + 一个损坏文件
        write(&dir.join("conf/app_settings.json"), r#"{"theme":"dark","proxy_port":8899}"#);
        write(&dir.join("data/api_pool.json"), r#"{"enabled_uids":["u1"],"strategy":"expire_first"}"#);
        write(
            &dir.join("data/checkin_accounts.json"),
            r#"{"accounts":[{"name":"a1","UserID":"u1","jwt":"j1"},{"name":"a2","jwt":""}]}"#,
        );
        write(
            &dir.join("data/credits_history.json"),
            r#"{"records":[{"date":"2026-09-15","user_id":"u1","credits":100,"delta":10}]}"#,
        );
        write(&dir.join("data/device_map.json"), r#"{"u1":{"device_id":"924145245134852"}}"#);
        write(&dir.join("data/pay_status.json"), "not-json");
        write(&dir.join("data/workbuddy_token_store.json"), r#"{"version":1,"tokens":{"a1":{"access_token":"t"}}}"#);
        // 遗留根路径兜底
        write(&dir.join("workbuddy_token_store.json"), r#"{"version":1,"tokens":{"a2":{"access_token":"t2"}}}"#);

        let summary = migrate_on_startup(&dir).expect("首次应触发迁移");
        assert!(summary.contains("导入"), "摘要应含导入数: {summary}");
        assert!(summary.contains("损坏"), "摘要应含损坏计数: {summary}");
        assert!(!summary.contains("失败 1"), "不应有失败项: {summary}");

        // 库生成且版本置位
        let store = db(&dir);
        let v = store.with_conn(|c| Ok(schema::user_version(c))).unwrap();
        assert_eq!(v, schema::SCHEMA_VERSION);

        // 原文件移入 backup/（保留 conf/data 相对路径前缀）
        assert!(dir.join("data/backup/conf/app_settings.json").exists());
        assert!(dir.join("data/backup/legacy_root/workbuddy_token_store.json").exists());
        assert!(dir.join("data/backup/corrupt/data/pay_status.json").exists());
        assert!(!dir.join("data/checkin_accounts.json").exists());
        // manifest 落盘
        assert!(dir.join("data/backup/migration_manifest.json").exists());

        // 数据验证：accounts 两行、credits_history 一条、kv 生效、wb_tokens 两行（data + legacy）
        let acc = docs::accounts_load(&store);
        assert_eq!(acc.accounts.len(), 2);
        assert_eq!(acc.accounts[0].user_id.as_deref(), Some("u1"));
        assert_eq!(acc.accounts[0].jwt, "j1");
        assert_eq!(acc.accounts[1].user_id, None);
        let ch = docs::credits_history_load(&store);
        assert_eq!(ch.records.len(), 1);
        assert_eq!(ch.records[0].credits, 100);
        let settings: Value = store.kv_get("app_settings");
        assert_eq!(settings["theme"], "dark");
        let tokens = docs::wb_token_store_load(&store);
        assert_eq!(tokens["tokens"]["a1"]["access_token"], "t");
        // 遗留根路径残留不覆盖正牌数据（仅清理，a2 不得混入）
        assert!(tokens["tokens"].get("a2").is_none());

        // 幂等：二次迁移不执行
        assert!(migrate_on_startup(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_dir_first_boot_creates_empty_db() {
        let dir = tmp_dir("empty");
        let summary = migrate_on_startup(&dir).expect("空目录也应完成建库");
        assert!(summary.contains("导入 0"));
        assert!(dir.join("data/aiwork.sqlite").exists());
        assert!(migrate_on_startup(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P7 验收：历史 JSON 遗留的重复 user_id 不得卡死迁移（accounts.user_id UNIQUE
    /// 防线去重、保序取首条），更不得让运行期 accounts 表为空。
    #[test]
    fn duplicate_uid_accounts_import_dedups_not_fails() {
        let dir = tmp_dir("dup");
        write(
            &dir.join("data/checkin_accounts.json"),
            r#"{"accounts":[
                {"name":"a","UserID":"u1","jwt":"j1"},
                {"name":"b","UserID":"u1","jwt":"j2"},
                {"name":"c","jwt":""}]}"#,
        );
        let summary = migrate_on_startup(&dir).expect("迁移应完成");
        assert!(!summary.contains("失败 1"), "重复 uid 不得计为失败: {summary}");
        let store = db(&dir);
        let acc = docs::accounts_load(&store);
        assert_eq!(acc.accounts.len(), 2, "重复 uid 去重，占位账号保留");
        assert_eq!(acc.accounts[0].name, "a");
        assert_eq!(acc.accounts[0].jwt, "j1", "保序取首条");
        assert_eq!(acc.accounts[1].name, "c");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P5 安全验收：store 迁移先于 vault 迁移时，JSON 中的明文凭据先入库、
    /// 再由 vault::migrate_on_startup 收敛进 Stronghold 并从库中占位化抹除。
    /// （main.rs 实际调用顺序与此测试一致；红线「vault 写失败禁止明文落盘」的库侧等价物）
    #[test]
    fn plaintext_converges_into_vault_after_store_migration() {
        use crate::state::AppState;
        let dir = tmp_dir("vault_conv");
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        write(
            &dir.join("data/checkin_accounts.json"),
            r#"{"accounts":[{"name":"a1","UserID":"u1","jwt":"secret-jwt","refresh_token":"secret-rt"}]}"#,
        );

        // ① store 迁移（main.rs 顺序：先于 vault）
        migrate_on_startup(&dir);
        // 入库后此刻仍为明文（待 vault 收敛）
        let store = db(&dir);
        let mid = super::super::docs::accounts_load(&store);
        assert_eq!(mid.accounts[0].jwt, "secret-jwt", "store 迁移保真导入");

        // ② vault 迁移（ AppState 手工构造，同 doubao_session 测试模式）
        let state = AppState {
            data_dir: dir.clone(),
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            codearts_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            buddy_ai_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        crate::vault::migrate_on_startup(&state);

        // ③ 库中凭据已占位化抹除（明文只存在于 vault）
        let after = super::super::docs::accounts_load(&store);
        assert_eq!(after.accounts.len(), 1);
        assert_eq!(after.accounts[0].jwt, "", "jwt 必须被占位化抹除");
        assert!(after.accounts[0].refresh_token.is_none(), "refresh_token 必须被抹除");
        assert_eq!(after.accounts[0].name, "a1", "非敏感字段保留");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v3→v4 验收：三套分组定义统一为 shg_ 共享分组——同名合并、旧 id 全量重定向
    /// （group_members / WB / Qoder 账号字段 / api_pool 筛选）、私有 kv 清除、
    /// 备份键可回滚、二次调用幂等。
    #[test]
    fn unified_groups_migration_merges_redirects_and_is_idempotent() {
        use crate::models::Group;
        let dir = tmp_dir("unified_groups");
        let store = db(&dir);

        // 种子：Trae g_1「主号」；Buddy wbg_1「主号」（同名合并）+ wbg_2「备用」；
        // Qoder qoderg_1「备用」；成员关系与 api_pool 引用齐全
        docs::group_defs_save(
            &store,
            &[Group { id: "g_1".into(), name: "主号".into(), color: "#f00".into(), order: 1 }],
        )
        .unwrap();
        store
            .kv_set(
                "workbuddy_groups",
                &vec![
                    Group { id: "wbg_1".into(), name: "主号".into(), color: "#0f0".into(), order: 1 },
                    Group { id: "wbg_2".into(), name: "备用".into(), color: "#00f".into(), order: 2 },
                ],
            )
            .unwrap();
        store
            .kv_set(
                "qoder_groups",
                &vec![Group { id: "qoderg_1".into(), name: "备用".into(), color: "#ff0".into(), order: 1 }],
            )
            .unwrap();
        store
            .with_conn(|c| {
                c.execute("INSERT INTO group_members(uid, group_id) VALUES('u1','g_1')", [])?;
                Ok(())
            })
            .unwrap();
        docs::wb_pool_save(&store, &json!({"accounts": [
            {"id": "wb-1", "group_id": "wbg_1"},
            {"id": "wb-2", "group_id": "wbg_2"}
        ]}))
        .unwrap();
        docs::qoder_pool_save(&store, &json!({"accounts": [
            {"id": "qd-1", "group_id": "qoderg_1"}
        ]}))
        .unwrap();
        store
            .kv_set_raw(
                "api_pool",
                r#"{"group_ids":["g_1"],"wb_group_ids":["wbg_2"],"qoder_group_ids":["qoderg_1"]}"#,
            )
            .unwrap();

        // 执行迁移
        let n = migrate_unified_groups(&store).unwrap();
        assert_eq!(n, 2, "同名「主号」合并 → 主号 + 备用 两条");

        // defs 全部 shg_ 前缀；私有 kv 清除
        let defs = docs::group_defs_load(&store);
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().all(|g| g.id.starts_with("shg_")), "统一新 ID: {:?}", defs);
        assert!(store.kv_get_raw("workbuddy_groups").is_none());
        assert!(store.kv_get_raw("qoder_groups").is_none());
        assert_eq!(store.kv_get::<bool>("unified_groups_migrated"), true);
        // 备份键可回滚
        let backup: Value = store.kv_get("groups_pre_migration");
        assert_eq!(backup["groups"][0]["id"], "g_1");
        assert_eq!(backup["workbuddy_groups"].as_array().unwrap().len(), 2);
        // 一次性启动通知已写入（前端 init 拉取 toast 后即清）
        let notice: Option<String> = store.kv_get("startup_notice");
        let notice = notice.unwrap();
        assert!(notice.contains("2 条共享分组"), "通知应含合并结果: {notice}");

        // 旧 id 引用零残留：group_members / WB / Qoder / api_pool 均重定向到对应新 id
        let name_new: std::collections::HashMap<String, String> =
            defs.iter().map(|g| (g.name.clone(), g.id.clone())).collect();
        let main_new = &name_new["主号"];
        let alt_new = &name_new["备用"];
        let m: std::collections::HashMap<String, String> = store
            .with_conn(|c| {
                let mut stmt = c.prepare("SELECT uid, group_id FROM group_members")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.into_iter().collect())
            })
            .unwrap();
        assert_eq!(m.get("u1").unwrap(), main_new);
        let wb = docs::wb_pool_load(&store);
        assert_eq!(wb["accounts"][0]["group_id"], json!(main_new));
        assert_eq!(wb["accounts"][1]["group_id"], json!(alt_new));
        let qd = docs::qoder_pool_load(&store);
        assert_eq!(qd["accounts"][0]["group_id"], json!(alt_new));
        let pool: Value = store.kv_get("api_pool");
        assert_eq!(pool["group_ids"], json!([main_new]));
        assert_eq!(pool["wb_group_ids"], json!([alt_new]));
        assert_eq!(pool["qoder_group_ids"], json!([alt_new]));

        // 幂等：二次调用跳过
        assert_eq!(migrate_unified_groups(&store).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v3→v4 容错验收（审查修复）：上次中途失败残留的 shg_ 定义（重定向已生效、
    /// defs 全删全写失败、标记未置位）重试时必须按 name 复用既有 shg_ id——
    /// 不得重新生成新 id，否则已重定向的成员/账号引用悬空。
    #[test]
    fn unified_groups_migration_resumes_from_partial_failure() {
        use crate::models::Group;
        let dir = tmp_dir("unified_groups_resume");
        let store = db(&dir);
        // 模拟残留态：shg_ 定义已在 groups 表、成员与 WB 账号已指向 shg_A，
        // 但旧 kv 定义仍在（上次 defs 全删全写失败）、迁移标记未置位
        docs::group_defs_save(
            &store,
            &[Group { id: "shg_A".into(), name: "主号".into(), color: "#f00".into(), order: 1 }],
        )
        .unwrap();
        store
            .kv_set(
                "workbuddy_groups",
                &vec![Group { id: "wbg_1".into(), name: "主号".into(), color: "#0f0".into(), order: 1 }],
            )
            .unwrap();
        store
            .with_conn(|c| {
                c.execute("INSERT INTO group_members(uid, group_id) VALUES('u1','shg_A')", [])?;
                Ok(())
            })
            .unwrap();
        docs::wb_pool_save(&store, &json!({"accounts": [{"id": "wb-1", "group_id": "shg_A"}]}))
            .unwrap();

        // 重试迁移：应复用 shg_A（同名对齐），不生成新 id
        let n = migrate_unified_groups(&store).unwrap();
        assert_eq!(n, 1, "残留 shg_ 定义 + 同名旧定义合并 → 仍 1 条");
        let defs = docs::group_defs_load(&store);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "shg_A", "必须复用残留 shg_ id，防引用悬空");
        // 已重定向的引用仍有效
        let m: Vec<(String, String)> = store
            .with_conn(|c| {
                let mut stmt = c.prepare("SELECT uid, group_id FROM group_members")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(m, vec![("u1".into(), "shg_A".into())], "成员引用未悬空");
        let wb = docs::wb_pool_load(&store);
        assert_eq!(wb["accounts"][0]["group_id"], json!("shg_A"), "WB 账号引用未悬空");
        assert!(store.kv_get_raw("workbuddy_groups").is_none());

        // 幂等：二次调用跳过
        assert_eq!(migrate_unified_groups(&store).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写一个快照槽（`<name>/meta.json`，含 uid/savedAt）
    fn write_slot(pool: &Path, name: &str, uid: &str) {
        let dir = pool.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = json!({ "uid": uid, "savedAt": 1_700_000_000_000i64 });
        std::fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();
    }

    #[test]
    fn traework_merge_moves_slots_and_migrates_settings() {
        let dir = tmp_dir("traework_merge");
        let store = db(&dir);

        // settings 承接：trae_path 指向真实文件、无 trae_cn_path → 承接
        let fake_exe = dir.join("fake").join("Trae CN.exe");
        std::fs::create_dir_all(fake_exe.parent().unwrap()).unwrap();
        std::fs::write(&fake_exe, "exe").unwrap();
        store
            .kv_set_raw(
                "app_settings",
                &json!({ "trae_path": fake_exe.to_string_lossy(), "theme": "dark" }).to_string(),
            )
            .unwrap();

        // 旧池：u1 完整组（主槽 + 代际 + sidecar）、u2 冲突组、运行时标记
        let old_pool = dir.join("data").join("profiles");
        write_slot(&old_pool, "u1", "u1");
        std::fs::create_dir_all(old_pool.join("u1.bak")).unwrap();
        std::fs::write(old_pool.join("u1.bak").join("snapshot.dat"), "x").unwrap();
        std::fs::write(old_pool.join("u1.meta.json"), r#"{"uid":"u1"}"#).unwrap();
        write_slot(&old_pool, "u2", "u2");
        std::fs::create_dir_all(old_pool.join("u2.bak2")).unwrap();
        std::fs::write(old_pool.join("u2.meta.json"), r#"{"uid":"u2"}"#).unwrap();
        std::fs::write(old_pool.join("current_account.txt"), "u2").unwrap();
        // 新池预置 u2 主槽（不同身份）→ u2 冲突
        write_slot(&dir.join("data").join("profiles_trae"), "u2", "u2new");

        let summary = migrate_traework_merge_inner(&store, &dir).unwrap();
        assert!(summary.contains("trae_cn_path"), "settings 承接摘要: {summary}");
        assert!(summary.contains("1 组"), "搬迁计数摘要: {summary}");

        // settings：trae_path 删除、trae_cn_path 承接、其余键保留
        let raw = store.kv_get_raw("app_settings").unwrap();
        assert!(!raw.contains("trae_path"));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["trae_cn_path"], json!(fake_exe.to_string_lossy()));
        assert_eq!(v["theme"], json!("dark"));

        // u1 组整组入 profiles_trae：主槽 + 代际 + sidecar，池根 sidecar 含身份标注且 uid 保留
        let new_pool = dir.join("data").join("profiles_trae");
        assert!(new_pool.join("u1").join("meta.json").exists());
        assert!(new_pool.join("u1.bak").join("snapshot.dat").exists());
        assert!(new_pool.join("u1.meta.json").exists());
        let m1: Value =
            serde_json::from_str(&std::fs::read_to_string(new_pool.join("u1.meta.json")).unwrap())
                .unwrap();
        assert_eq!(m1["uid"], json!("u1"));
        assert_eq!(m1["source"], json!("TraeWork-migrated"));
        // u2 冲突：整组入 legacy，目标主槽不被覆盖
        let legacy = dir.join("data").join("profiles_trae_work_legacy");
        assert!(legacy.join("u2").join("meta.json").exists());
        assert!(legacy.join("u2.bak2").exists());
        assert!(legacy.join("u2.meta.json").exists());
        let m_l: Value =
            serde_json::from_str(&std::fs::read_to_string(legacy.join("u2").join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(m_l["uid"], json!("u2"), "旧槽整组入 legacy 且身份保留");
        let m222: Value = serde_json::from_str(
            &std::fs::read_to_string(new_pool.join("u2").join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(m222["uid"], json!("u2new"), "冲突组绝不覆盖既有主槽");
        assert!(!new_pool.join("u2.bak2").exists(), "旧组代际未混入新池");
        // 运行时标记留旧池 → 旧池非空不删
        assert!(old_pool.join("current_account.txt").exists());
        assert!(old_pool.exists());

        // 升级引导报告：完成时落 kv，计数与本次搬迁一致、明细非空
        let report: Value = serde_json::from_str(&store.kv_get_raw("traework_merge_report").unwrap())
            .unwrap();
        assert_eq!(report["moved_groups"], json!(1));
        assert_eq!(report["legacy_groups"], json!(1));
        assert!(!report["notes"].as_array().unwrap().is_empty());
        assert!(report["finished_at_ms"].as_i64().unwrap() > 0);

        // 幂等：二次调用跳过
        assert!(migrate_traework_merge_inner(&store, &dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn traework_merge_settings_edge_and_empty_pool_cleanup() {
        // 场景 A：trae_cn_path 已有值 → 即使 trae_path 指向真实文件也不覆盖
        let dir_a = tmp_dir("traework_edge_a");
        let store_a = db(&dir_a);
        let fake_exe = dir_a.join("fake").join("Trae CN.exe");
        std::fs::create_dir_all(fake_exe.parent().unwrap()).unwrap();
        std::fs::write(&fake_exe, "exe").unwrap();
        store_a
            .kv_set_raw(
                "app_settings",
                &json!({
                    "trae_path": fake_exe.to_string_lossy(),
                    "trae_cn_path": "C:/already/exe"
                })
                .to_string(),
            )
            .unwrap();
        let s = migrate_traework_merge_inner(&store_a, &dir_a).unwrap();
        assert!(!s.contains("承接"), "已有值不覆盖: {s}");
        let v: Value =
            serde_json::from_str(&store_a.kv_get_raw("app_settings").unwrap()).unwrap();
        assert_eq!(v["trae_cn_path"], json!("C:/already/exe"), "既有 trae_cn_path 保留");
        assert!(v.get("trae_path").is_none(), "trae_path 键删除");
        let _ = std::fs::remove_dir_all(&dir_a);

        // 场景 B：trae_path 指向不存在路径 → 不承接仅删键；空池搬迁后目录删除；幂等
        let dir_b = tmp_dir("traework_edge_b");
        let store_b = db(&dir_b);
        store_b
            .kv_set_raw(
                "app_settings",
                &json!({ "trae_path": "C:/no/such/Trae CN.exe" }).to_string(),
            )
            .unwrap();
        write_slot(&dir_b.join("data").join("profiles"), "u3", "u3");
        let s = migrate_traework_merge_inner(&store_b, &dir_b).unwrap();
        assert!(!s.contains("承接"), "无效路径不承接: {s}");
        let v: Value =
            serde_json::from_str(&store_b.kv_get_raw("app_settings").unwrap()).unwrap();
        assert!(v.get("trae_cn_path").is_none());
        assert!(v.get("trae_path").is_none());
        // 旧池仅剩 u3（无运行时标记）→ 搬走后空目录删除
        assert!(!dir_b.join("data").join("profiles").exists(), "空旧池应删除");
        assert!(dir_b.join("data").join("profiles_trae").join("u3").exists());
        // 幂等：二次调用跳过
        assert!(migrate_traework_merge_inner(&store_b, &dir_b).is_none());
        let _ = std::fs::remove_dir_all(&dir_b);

        // 全新安装：无 trae_path、无旧池 → 两次调用均 None，且不落升级引导报告
        let dir_c = tmp_dir("traework_fresh");
        let store_c = db(&dir_c);
        assert!(migrate_traework_merge_inner(&store_c, &dir_c).is_none());
        assert!(migrate_traework_merge_inner(&store_c, &dir_c).is_none());
        assert!(store_c.kv_get_raw("traework_merge_report").is_none());
        let _ = std::fs::remove_dir_all(&dir_c);
    }

    #[test]
    fn traework_merge_retry_moves_remaining_entries_to_new_pool() {
        // 上轮部分成功（主槽已迁走）后重试：剩余 sidecar 应补齐入新池而非误入 legacy
        let dir = tmp_dir("traework_retry");
        let store = db(&dir);
        let old_pool = dir.join("data").join("profiles");
        let new_pool = dir.join("data").join("profiles_trae");
        write_slot(&old_pool, "u1", "u1");
        std::fs::write(old_pool.join("u1.meta.json"), r#"{"uid":"u1"}"#).unwrap();
        // 模拟上轮部分失败：主槽已手动迁到新池，sidecar 留在旧池
        std::fs::create_dir_all(&new_pool).unwrap();
        std::fs::rename(old_pool.join("u1"), new_pool.join("u1")).unwrap();

        let summary = migrate_traework_merge_inner(&store, &dir).unwrap();
        assert!(
            summary.contains("profiles_trae 1 组"),
            "补齐应计入迁入而非 legacy: {summary}"
        );

        // sidecar 补齐入新池，不拆散入 legacy
        assert!(new_pool.join("u1.meta.json").exists(), "sidecar 应补齐入新池");
        assert!(
            !dir.join("data").join("profiles_trae_work_legacy").exists(),
            "补齐场景不得整组入 legacy"
        );
        // 主槽身份保留且补齐路径仍走迁入分支（池根 sidecar 含身份标注）
        let m1: Value = serde_json::from_str(
            &std::fs::read_to_string(new_pool.join("u1.meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(m1["uid"], json!("u1"));
        assert_eq!(m1["source"], json!("TraeWork-migrated"));
        // 旧池已空 → 删除
        assert!(!old_pool.exists(), "补齐完成后空旧池应删除");
        // 幂等：二次调用跳过
        assert!(migrate_traework_merge_inner(&store, &dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn traework_merge_settings_rejects_solo_client_path() {
        // trae_path 指向真实存在但为退役 Solo 客户端的文件 → 拒绝承接、仅删键
        let dir = tmp_dir("traework_solo_guard");
        let store = db(&dir);
        let solo_exe = dir.join("fake").join("TRAE SOLO CN.exe");
        std::fs::create_dir_all(solo_exe.parent().unwrap()).unwrap();
        std::fs::write(&solo_exe, "exe").unwrap();
        store
            .kv_set_raw(
                "app_settings",
                &json!({ "trae_path": solo_exe.to_string_lossy(), "theme": "dark" }).to_string(),
            )
            .unwrap();

        let summary = migrate_traework_merge_inner(&store, &dir).unwrap();
        assert!(summary.contains("丢弃"), "Solo 路径应拒绝承接: {summary}");

        let v: Value =
            serde_json::from_str(&store.kv_get_raw("app_settings").unwrap()).unwrap();
        assert!(v.get("trae_cn_path").is_none(), "不得承接 Solo 路径为 trae_cn_path");
        assert!(v.get("trae_path").is_none(), "trae_path 键仍应删除");
        assert_eq!(v["theme"], json!("dark"), "其余设置保留");
        // 幂等：二次调用跳过
        assert!(migrate_traework_merge_inner(&store, &dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn traework_merge_settings_rejects_legacy_trae_exe() {
        // 存量旧客户端 ...\Programs\Trae\Trae.exe（文件名不含 solo）同样拒绝承接：
        // 承接为 trae_cn_path 会让「打开 Trae」静默拉起退役旧客户端
        let dir = tmp_dir("traework_legacy_guard");
        let store = db(&dir);
        let old_exe = dir.join("Programs").join("Trae").join("Trae.exe");
        std::fs::create_dir_all(old_exe.parent().unwrap()).unwrap();
        std::fs::write(&old_exe, "exe").unwrap();
        store
            .kv_set_raw(
                "app_settings",
                &json!({ "trae_path": old_exe.to_string_lossy() }).to_string(),
            )
            .unwrap();

        let summary = migrate_traework_merge_inner(&store, &dir).unwrap();
        assert!(summary.contains("丢弃"), "旧版 Trae.exe 应拒绝承接: {summary}");
        let v: Value =
            serde_json::from_str(&store.kv_get_raw("app_settings").unwrap()).unwrap();
        assert!(v.get("trae_cn_path").is_none(), "不得承接旧版 Trae.exe 为 trae_cn_path");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
