//! WorkBuddy 公共请求层（原 src-python/wb_common.py 的 Rust 移植）。
//! 依据 docs/workbuddy-product-design.md §5.1~§5.4：
//! - 凭证双源化（F-10）：工具侧 token_store 与桌面 auth 文件「谁新用谁」（expiresAtMs 晚者胜出）
//! - 统一请求头（§5.3）+ 宽容解析 dig（复用 fs_utils::dig 信封下钻语义，对齐 wb.dig）
//! - 红线：chat 请求绝不携带 X-Refresh-Token；仅 refresh 端点携带
//! - 网络：ureq 默认直连（不读系统/环境代理），对齐 python OPENER 绕代理约定
//!
//! P1 先行落地（wb_credits 依赖）；P2 wb_checkin / trae_checkin 直接复用本层。

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::fs_utils;
use crate::state::AppState;

// ── 路径 ────────────────────────────────────────────────────────────────────
// （SQLite 化 P3：pool/token store/checkin results 均改走 store，文件路径函数已删除）

/// 桌面 auth 文件读取路径：settings.wb_auth_file_path 人工指定优先（与
/// commands/workbuddy/common.rs auth_file_path_of 同语义），否则默认布局。
fn auth_file_path(state: &AppState) -> PathBuf {
    if let Some(p) = state.settings().wb_auth_file_path.as_deref() {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    PathBuf::from(local)
        .join("CodeBuddyExtension")
        .join("Data")
        .join("Public")
        .join("auth")
        .join("workbuddy-desktop.info")
}

// ── 凭证结构（对齐 python creds_of 返回 dict）───────────────────────────────

#[derive(Serialize, Clone, Default, Debug)]
pub struct Creds {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub refresh_expires_at_ms: Option<i64>,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub edition: String,
}

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

/// 宽容整数：数字或数字字符串（对齐 python int(v) 兜底）
fn i_of(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok())
}

/// 从 auth 文件 / token store 记录中提取凭证字段（兼容多种嵌套形态，F-04）。
pub fn creds_of(source: &Value) -> Creds {
    let auth = source.get("auth").filter(|v| v.is_object()).unwrap_or(source);
    let account = source
        .get("account")
        .filter(|v| v.is_object())
        .unwrap_or(source);
    Creds {
        access_token: s_of(fs_utils::dig(auth, &["accessToken", "access_token", "token"])),
        refresh_token: s_of(fs_utils::dig(auth, &["refreshToken", "refresh_token"])),
        expires_at_ms: i_of(fs_utils::dig(
            auth,
            &[
                "expiresAtMs",
                "expires_at_ms",
                "expiresAt",
                "expires_in_ms",
                "accessTokenExpiresAtMs",
            ],
        )),
        refresh_expires_at_ms: i_of(fs_utils::dig(
            auth,
            &["refreshExpiresAtMs", "refresh_expires_at_ms", "refreshExpiresAt"],
        )),
        uid: s_of(fs_utils::dig(account, &["uid", "userId", "user_id", "id"])),
        domain: s_of(fs_utils::dig(source, &["domain"])),
        nickname: s_of(fs_utils::dig(account, &["nickname", "name", "displayName"])),
        edition: s_of(fs_utils::dig(account, &["editionType", "edition_type", "edition"])),
    }
}

fn read_auth_file(state: &AppState) -> Creds {
    creds_of(&fs_utils::read_json::<Value>(&auth_file_path(state)))
}

// ── token store 凭证 vault 收敛（审查 P0-1）────────────────────────────────
//
// wb_tokens 表中敏感字段（access_token / refresh_token）一律占位（空串）存储，
// 明文经 vault::ns_set 走 Stronghold + DPAPI 加密（与 Trae 家族同一 vault）。
// 读取统一回填（仅内存）；写入统一占位。vault 写失败时禁止明文落库（对齐 Trae 红线）。

/// 敏感字段键名集合：snake_case 为主（save_token_store / oauth 写入形态），
/// 兼容 auth 文件导入的 camelCase 形态（accessToken / refreshToken）。
const TOKEN_SENSITIVE_KEYS: [&str; 4] = [
    "access_token",
    "accessToken",
    "refresh_token",
    "refreshToken",
];

/// DB 读取 + vault 回填（仅内存，不落明文）：敏感字段为占位空串时从 vault 回填。
/// vault 不可用 / 无记录 → 保持空串（上层按 no_credential 处理，fail-secure）。
pub fn token_store_load_secure(data_dir: &std::path::Path) -> Value {
    let mut store = crate::store::docs::wb_token_store_load(&crate::store::db(data_dir));
    let Some(tokens) = store.get_mut("tokens").and_then(Value::as_object_mut) else {
        return store;
    };
    for (id, rec) in tokens.iter_mut() {
        let Some(rm) = rec.as_object_mut() else { continue };
        let Some(sec) = crate::vault::ns_get(data_dir, "wb", id) else { continue };
        let acc = sec.get("access_token").and_then(Value::as_str).unwrap_or("");
        let rt = sec.get("refresh_token").and_then(Value::as_str).unwrap_or("");
        let mut has_access_key = false;
        let mut has_refresh_key = false;
        for k in TOKEN_SENSITIVE_KEYS {
            let is_access = k.ends_with("ccess_token");
            let val = if is_access { acc } else { rt };
            if !rm.contains_key(k) {
                continue;
            }
            if is_access {
                has_access_key = true;
            } else {
                has_refresh_key = true;
            }
            // 仅填空值：DB 明文优先（更新鲜，如迁移残留，待下次写入收敛）
            if !val.is_empty()
                && rm.get(k).and_then(Value::as_str).map_or(true, |s| s.is_empty())
            {
                rm.insert(k.to_string(), serde_json::json!(val));
            }
        }
        // rec 无任何敏感键但 vault 有值 → 补 snake_case 两键（与 save_token_store 写入形态一致）
        if !acc.is_empty() && !has_access_key {
            rm.insert("access_token".into(), serde_json::json!(acc));
        }
        if !rt.is_empty() && !has_refresh_key {
            rm.insert("refresh_token".into(), serde_json::json!(rt));
        }
    }
    store
}

/// 单账号 UPSERT + 敏感字段 vault 收敛：非空敏感值字段级合并写入 vault（不丢旧值），
/// DB 记录一律占位（空串）。vault 写失败时**数据库保持原状**（issue #153 修复 1：
/// 原「落占位行 + 返 Err」会销毁 DB 中未收敛的明文凭证，迁移场景无处恢复）——
/// 不落库直接返回 Err，凭据仅存内存，重新登录可恢复。
pub fn token_store_upsert_secure(
    data_dir: &std::path::Path,
    id: &str,
    rec: &Value,
) -> Result<(), String> {
    let mut rec = rec.clone();
    let Some(rm) = rec.as_object_mut() else {
        return crate::store::docs::wb_token_store_upsert(&crate::store::db(data_dir), id, &rec);
    };
    // 提取非空敏感值（两组键名各取其一）
    let mut acc = String::new();
    let mut rt = String::new();
    for k in TOKEN_SENSITIVE_KEYS {
        let v = rm.get(k).and_then(Value::as_str).unwrap_or("");
        if v.is_empty() {
            continue;
        }
        if k.ends_with("ccess_token") {
            acc = v.to_string();
        } else {
            rt = v.to_string();
        }
    }
    if acc.is_empty() && rt.is_empty() {
        // 无敏感值：直接落库（rec 保持调用方形态）
        return crate::store::docs::wb_token_store_upsert(&crate::store::db(data_dir), id, &rec);
    }
    let mut entry = crate::vault::ns_get(data_dir, "wb", id).unwrap_or_else(|| serde_json::json!({}));
    if !acc.is_empty() {
        entry["access_token"] = serde_json::json!(acc);
    }
    if !rt.is_empty() {
        entry["refresh_token"] = serde_json::json!(rt);
    }
    // vault 写失败：数据库保持原状，不落库（issue #153 修复 1）
    if let Err(e) = crate::vault::ns_set(data_dir, "wb", id, &entry) {
        crate::fs_utils::app_log(
            data_dir,
            &format!("[wb] vault 写入失败，token store 未落库（id={id}，凭据未持久化）: {e}"),
        );
        return Err(format!("WB 凭据加密存储失败（数据库未改动，凭据未持久化，重新登录可恢复）: {e}"));
    }
    // vault 已持久化 → DB 占位落库（明文唯一驻留地是 vault）
    for k in TOKEN_SENSITIVE_KEYS {
        if rm.contains_key(k) {
            rm.insert(k.to_string(), serde_json::json!(""));
        }
    }
    crate::store::docs::wb_token_store_upsert(&crate::store::db(data_dir), id, &rec)
}

// ── buddy-ai（国际版）token store：独立表 + 独立 vault 命名空间 ──────────────
//
// §3.4 隔离清单：与 wb 池零共享（表 / vault ns 双维度隔离）；secure 收敛口径
// （敏感字段进 vault、DB 占位）与 wb 侧完全一致，仅表名/命名空间参数化。

/// buddy-ai token store 表名（schema.rs ROW_TABLES 白名单内）
pub const BUDDY_AI_TOKEN_TABLE: &str = "buddy_ai_tokens";
/// buddy-ai vault 命名空间（与 wb 池 "wb" 隔离）
pub const BUDDY_AI_VAULT_NS: &str = "buddy-ai";

/// 通用 secure 读取（table + vault ns 参数化）：与 token_store_load_secure 同口径
fn token_store_load_secure_in(data_dir: &std::path::Path, table: &str, ns: &str) -> Value {
    let mut store = crate::store::docs::token_store_load(table, &crate::store::db(data_dir));
    let Some(tokens) = store.get_mut("tokens").and_then(Value::as_object_mut) else {
        return store;
    };
    for (id, rec) in tokens.iter_mut() {
        let Some(rm) = rec.as_object_mut() else { continue };
        let Some(sec) = crate::vault::ns_get(data_dir, ns, id) else { continue };
        let acc = sec.get("access_token").and_then(Value::as_str).unwrap_or("");
        let rt = sec.get("refresh_token").and_then(Value::as_str).unwrap_or("");
        let mut has_access_key = false;
        let mut has_refresh_key = false;
        for k in TOKEN_SENSITIVE_KEYS {
            let is_access = k.ends_with("ccess_token");
            let val = if is_access { acc } else { rt };
            if !rm.contains_key(k) {
                continue;
            }
            if is_access {
                has_access_key = true;
            } else {
                has_refresh_key = true;
            }
            if !val.is_empty()
                && rm.get(k).and_then(Value::as_str).map_or(true, |s| s.is_empty())
            {
                rm.insert(k.to_string(), serde_json::json!(val));
            }
        }
        if !acc.is_empty() && !has_access_key {
            rm.insert("access_token".into(), serde_json::json!(acc));
        }
        if !rt.is_empty() && !has_refresh_key {
            rm.insert("refresh_token".into(), serde_json::json!(rt));
        }
    }
    store
}

/// 通用 secure upsert（table + vault ns 参数化）：与 token_store_upsert_secure 同口径
///（vault 写失败时数据库保持原状不落库，issue #153 修复 1）
fn token_store_upsert_secure_in(
    data_dir: &std::path::Path,
    table: &str,
    ns: &str,
    id: &str,
    rec: &Value,
) -> Result<(), String> {
    let mut rec = rec.clone();
    let Some(rm) = rec.as_object_mut() else {
        return crate::store::docs::token_store_upsert(&crate::store::db(data_dir), table, id, &rec);
    };
    let mut acc = String::new();
    let mut rt = String::new();
    for k in TOKEN_SENSITIVE_KEYS {
        let v = rm.get(k).and_then(Value::as_str).unwrap_or("");
        if v.is_empty() {
            continue;
        }
        if k.ends_with("ccess_token") {
            acc = v.to_string();
        } else {
            rt = v.to_string();
        }
    }
    if acc.is_empty() && rt.is_empty() {
        return crate::store::docs::token_store_upsert(&crate::store::db(data_dir), table, id, &rec);
    }
    let mut entry = crate::vault::ns_get(data_dir, ns, id).unwrap_or_else(|| serde_json::json!({}));
    // vault 旧条目损坏为非对象（String/Array 等）时 index 赋值会 panic——
    // panic 点在续期/导入/OAuth 回写路径上，防御性归零重建
    if !entry.is_object() {
        entry = serde_json::json!({});
    }
    if !acc.is_empty() {
        entry["access_token"] = serde_json::json!(acc);
    }
    if !rt.is_empty() {
        entry["refresh_token"] = serde_json::json!(rt);
    }
    // vault 写失败：数据库保持原状，不落库（issue #153 修复 1）
    if let Err(e) = crate::vault::ns_set(data_dir, ns, id, &entry) {
        crate::fs_utils::app_log(
            data_dir,
            &format!("[{ns}] vault 写入失败，token store 未落库（id={id}，凭据未持久化）: {e}"),
        );
        return Err(format!("凭据加密存储失败（数据库未改动，凭据未持久化，重新登录可恢复）: {e}"));
    }
    // vault 已持久化 → DB 占位落库（明文唯一驻留地是 vault）
    for k in TOKEN_SENSITIVE_KEYS {
        if rm.contains_key(k) {
            rm.insert(k.to_string(), serde_json::json!(""));
        }
    }
    crate::store::docs::token_store_upsert(&crate::store::db(data_dir), table, id, &rec)
}

/// buddy-ai token store 读取（vault 回填，仅内存不落明文）
pub fn buddy_ai_token_store_load(data_dir: &std::path::Path) -> Value {
    token_store_load_secure_in(data_dir, BUDDY_AI_TOKEN_TABLE, BUDDY_AI_VAULT_NS)
}

/// buddy-ai token store 单账号 UPSERT（敏感字段进 buddy-ai vault 命名空间，DB 占位）
pub fn buddy_ai_token_store_upsert(
    data_dir: &std::path::Path,
    id: &str,
    rec: &Value,
) -> Result<(), String> {
    token_store_upsert_secure_in(data_dir, BUDDY_AI_TOKEN_TABLE, BUDDY_AI_VAULT_NS, id, rec)
}

/// 生效凭证 = token store 与 auth 文件中 expiresAtMs 更晚者（F-10 谁新用谁）。
/// auth 文件仅当其 uid 与账号匹配时参与双源比较（桌面当前登录态）。
/// 读 token store（SQLite 化 P3：wb_tokens 表；结构 {version, tokens:{id:rec}}）。
/// 全部 token store 读点统一入口。
pub fn load_token_store(state: &AppState) -> Value {
    token_store_load_secure(&state.data_dir)
}

pub fn effective_creds(state: &AppState, acct_id: &str, acct_uid: &str) -> Creds {
    let store: Value = load_token_store(state);
    let store_creds = store
        .get("tokens")
        .and_then(|t| t.get(acct_id))
        .map(creds_of)
        .unwrap_or_default();
    let mut file_creds = read_auth_file(state);
    if !acct_uid.is_empty() && !file_creds.uid.is_empty() && file_creds.uid != acct_uid {
        file_creds = Creds::default();
    }
    let a = store_creds.expires_at_ms;
    let b = file_creds.expires_at_ms;
    // python: file 有 token 且 (store 无到期时间 或 file 到期 >= store 到期)
    let file_newer = a.is_none() || b.is_some_and(|bv| a.map_or(true, |av| bv >= av));
    if !file_creds.access_token.is_empty() && file_newer {
        return file_creds;
    }
    store_creds
}

// ── 并发刷新防护（审查 H-1，对齐 qoder_common 同款双锁）──────────────────────

/// token store 表级读改写互斥：load→merge→upsert 非原子，签到/积分/续期/导入/
/// OAuth 多通道并发写会互相覆盖丢更新（last-writer-wins 抹掉彼此的新 token）。
/// tasks 侧 save_token_store 与 commands 侧 upsert_token_store 共用本锁串行化
/// 表级读改写；仅护本地 IO，不覆盖网络请求路径（无死锁面）。
/// 锁序约定：账号刷新锁 → 本表锁（单向获取，无环）。
pub(crate) static WB_TOKEN_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 每账号刷新互斥（审查 H-1/P0）：签到/积分 401 自愈/续期/调度器与 schtasks CLI
/// 双进程多通道并发触发同一账号刷新时，两个线程可能同时拿旧 refresh_token 换新
/// token——服务端一次性轮换下后落库者覆盖先落库者，被覆盖方刚拿到的令牌即刻失效
///（严重时 refresh_token 一并丢，被迫重登）。以账号 id 为键的进程内锁串行化
/// 「读凭证→网络刷新→落库」全程；持锁后重读 token store 天然构成二次检查。
static WB_REFRESH_LOCKS: std::sync::Mutex<
    Option<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>,
> = std::sync::Mutex::new(None);

pub(crate) fn refresh_lock_for(acct_id: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    let mut g = WB_REFRESH_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(std::collections::HashMap::new)
        .entry(acct_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
        .clone()
}

/// 账号移除路径主动清理（P3-L，对齐 qoder refresh_lock_remove）：回收刷新锁表项，
/// 防止账号删除后条目永驻 HashMap（进程生命周期内每次增删账号泄漏一把锁）。
/// 审查修复（M20）：仅当 Arc::strong_count==1（仅剩表内自身引用）时才回收——
/// remove 与旧持有者 drop 守卫之间有窗口，贸然 remove 会让新读者经
/// refresh_lock_for 重建新锁与旧持有者并行进入临界区，互斥失效；条目留待后续
/// 移除再回收，仅延迟不泄漏。
pub(crate) fn refresh_lock_remove(acct_id: &str) {
    let mut g = WB_REFRESH_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = g.as_mut() {
        // 持表锁期间 check-then-remove 与 refresh_lock_for 的 get_or_insert 互斥，
        // get 的借用不计入 strong_count，判定可靠
        if map
            .get(acct_id)
            .map(|arc| std::sync::Arc::strong_count(arc) == 1)
            .unwrap_or(false)
        {
            map.remove(acct_id);
        }
    }
}

/// 跨进程刷新互斥（H-1 ②）：GUI 调度器与 schtasks CLI（--task-run，绕开单实例
/// 插件）双进程并存，进程内锁无法约束跨进程并发。命名互斥体按账号隔离
///（ns="wb"，scope=账号 id，锁名含 data_dir 短哈希）；抢锁失败幂等跳过本次刷新
///（沿用现有凭证，仅少一次自愈机会，无损失）。
/// C1（审查）：改按 data_dir 传参——网关侧 wb_upstream::refresh_access_token
/// 无 AppState，与 tasks 侧复用同一把锁；pub(crate) 供 api_server 模块调用。
pub(crate) fn cross_refresh_lock(
    data_dir: &std::path::Path,
    acct_id: &str,
) -> Option<crate::tasks::qoder_common::CrossProcLock> {
    let (guard, fail) =
        crate::tasks::qoder_common::CrossProcLock::try_acquire_ns(data_dir, "wb", acct_id, 5000);
    if guard.is_none() {
        let reason = fail
            .as_ref()
            .map(|f| f.describe())
            .unwrap_or_default();
        fs_utils::app_log(
            data_dir,
            &format!("[wb-fresh] 跨进程刷新锁未获取(id={acct_id}, {reason})，本轮沿用现有凭证"),
        );
    }
    guard
}

/// 写工具侧凭证副本（F-10 谁新用谁）。version≠1 拒绝写入（版本闸门）；
/// 非空字段合并 + updated_at（与 commands/workbuddy/common.rs upsert_token_store 同语义）。
/// 凭证收敛（P0-1）：落库走 token_store_upsert_secure——敏感字段进 vault、DB 占位；
/// vault 写失败时仅落占位并返回 Err（禁止明文落库）。
pub fn save_token_store(state: &AppState, id: &str, creds: &Creds) -> Result<(), String> {
    // H-1：表级读改写互斥（与 commands 侧 upsert_token_store 共锁）
    let _table = WB_TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store: Value = load_token_store(state);
    if !store.is_object() {
        store = serde_json::json!({});
    }
    let obj = store.as_object_mut().unwrap();
    match obj.get("version") {
        Some(v) if v.as_i64() != Some(1) => {
            return Err("token_store 版本不识别，拒绝写入".to_string());
        }
        _ => {}
    }
    obj.insert("version".into(), serde_json::json!(1));
    let tokens = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
    let mut rec = serde_json::json!({});
    if let Some(t) = tokens.as_object() {
        if let Some(r) = t.get(id) {
            rec = r.clone();
        }
    }
    if let Some(rm) = rec.as_object_mut() {
        let val = serde_json::to_value(creds).map_err(|e| e.to_string())?;
        for (k, v) in val.as_object().into_iter().flatten() {
            if !v.is_null() {
                rm.insert(k.clone(), v.clone());
            }
        }
        rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
    }
    token_store_upsert_secure(&state.data_dir, id, &rec)?;
    // version 闸门语义不变（整库字段，单行 upsert 不携带 version——写入 kv 元数据）
    crate::store::docs::wb_token_store_save_version(&crate::store::db(&state.data_dir), 1)
}

// ── 客户端指纹伪装（issue #48）─────────────────────────────────────────────

/// billing/签到/刷新链路桌面端 UA 伪装：必须带版本号——裸 "WorkBuddy" 在
/// 个人中心「请求明细」的客户端列识别为 "-"（issue #48 反馈的可检测特征）。
pub const WB_DESKTOP_UA: &str = "WorkBuddy/5.5.6";

/// 账号级稳定设备指纹：sha256("wb-fingerprint:{kind}:{uid}") 前 16 字节 →
/// 32 位小写十六进制。同账号恒定（虚拟设备稳定）、跨账号隔离（防关联）；
/// uid 为空返回 None（缺失即不带，不伪造）。
pub fn derive_device_fingerprint(uid: &str, kind: &str) -> Option<String> {
    if uid.trim().is_empty() {
        return None;
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!("wb-fingerprint:{kind}:{uid}").as_bytes());
    let d = h.finalize();
    Some(d[..16].iter().map(|b| format!("{b:02x}")).collect())
}

// ── 统一请求头（§5.3）───────────────────────────────────────────────────────

/// Bearer + X-User-Id（缺省 X-No-* 占位）+ 客户端指纹（issue #48：UA 带版本、
/// X-Machine-ID/X-Session-ID 账号级稳定派生、X-Domain 域标识）；web_platform=true
/// 附加 X-Client-Platform: web（积分三件套必需）。
pub fn build_auth_headers(creds: &Creds, web_platform: bool) -> Vec<(String, String)> {
    let mut h = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", creds.access_token),
        ),
        ("User-Agent".to_string(), WB_DESKTOP_UA.to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if creds.uid.is_empty() {
        h.push(("X-No-User-Id".to_string(), "1".to_string()));
    } else {
        h.push(("X-User-Id".to_string(), creds.uid.clone()));
        // 客户端指纹（issue #48）：个人中心请求明细按设备/会话标识识别客户端
        if let Some(mid) = derive_device_fingerprint(&creds.uid, "machine") {
            h.push(("X-Machine-ID".to_string(), mid));
        }
        if let Some(sid) = derive_device_fingerprint(&creds.uid, "session") {
            h.push(("X-Session-ID".to_string(), sid));
        }
    }
    if !creds.domain.is_empty() {
        h.push(("X-Domain".to_string(), creds.domain.clone()));
    }
    if web_platform {
        h.push(("X-Client-Platform".to_string(), "web".to_string()));
    }
    h
}

/// POST JSON → (http_status, parsed, raw_text)；status=0 表示网络不可达
///（对齐 python post_json 三元组；HTTPError 同样返回状态码与响应体原文）。
pub fn post_json_raw(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> (u16, Option<Value>, String) {
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_string(&body.to_string()) {
        Ok(resp) => {
            // 审查 P2：读取失败不再吞成空串（调用方诊断日志需区分「空响应」与「读取失败」）
            let raw = resp.into_string().unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            let parsed = serde_json::from_str(&raw).ok();
            (200, parsed, raw)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp.into_string().unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            let parsed = serde_json::from_str(&raw).ok();
            (code, parsed, raw)
        }
        Err(e) => (0, None, e.to_string()),
    }
}

/// POST JSON → (http_status, parsed)；status=0 表示网络不可达
///（对齐 python post_json 三元组的可消费部分；HTTPError 同样返回状态码）。
pub fn post_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> (u16, Option<Value>) {
    let (status, parsed, _) = post_json_raw(agent, url, headers, body);
    (status, parsed)
}

/// GET 请求 → (http_status, parsed, raw_text)；status=0 网络不可达
///（对齐 python get_json，F-17 成长中心等 GET 端点；HTTPError 同样返回状态码）。
pub fn get_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
) -> (u16, Option<Value>, String) {
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(resp) => {
            // 审查 P2：读取失败不再吞成空串（调用方诊断日志需区分「空响应」与「读取失败」）
            let raw = resp.into_string().unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            let parsed = serde_json::from_str(&raw).ok();
            (200, parsed, raw)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp.into_string().unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            let parsed = serde_json::from_str(&raw).ok();
            (code, parsed, raw)
        }
        Err(e) => (0, None, e.to_string()),
    }
}

// ── 区域路由（T4.5/F-36，§5.2）──────────────────────────────────────────────
// CN：billing/积分走 www.codebuddy.cn、活动接口走 www.workbuddy.cn；
// Global（domain 含 .workbuddy.ai）全走 www.workbuddy.ai。
// token refresh 随区域切换（R-2a 实测收口：Intl=workbuddy.ai /v2/plugin/auth/token/refresh
// code 0，rt 每次轮换）——见下方 region_refresh_url；REFRESH_URL 仅 CN 侧消费。

pub const BILLING_BASE_CN: &str = "https://www.codebuddy.cn";
pub const BILLING_BASE_GLOBAL: &str = "https://www.workbuddy.ai";
/// CN 活动/积分网关（签到/活动 banner/成长任务/积分领取走 www.workbuddy.cn，
/// 与 BILLING_BASE_CN=codebuddy.cn（token refresh）是**两个不同域**——
/// credits.rs 三处散点收敛到此单点（§4.5 域名收敛，CN 行为保持不变）
pub const ACTIVITY_BASE_CN: &str = "https://www.workbuddy.cn";
pub const REFRESH_URL: &str = "https://www.codebuddy.cn/v2/plugin/auth/token/refresh";

/// 活动/积分网关 base 按区域取用（§4.5 域名收敛：Global=workbuddy.ai、
/// CN=workbuddy.cn，消除 credits.rs 中「含 workbuddy.ai 判定 + 双域混用」散点）
pub fn region_activity_base(domain: &str) -> &'static str {
    if is_global_region(domain) {
        BILLING_BASE_GLOBAL
    } else {
        ACTIVITY_BASE_CN
    }
}

pub fn is_global_region(domain: &str) -> bool {
    domain.contains(".workbuddy.ai")
}

pub fn region_billing_base(domain: &str) -> &'static str {
    if is_global_region(domain) {
        BILLING_BASE_GLOBAL
    } else {
        BILLING_BASE_CN
    }
}

/// 域名双探测（§2.2 接口稳定性）：主域名在前、备用域名在后。
pub fn billing_bases(domain: &str) -> [&'static str; 2] {
    let main = region_billing_base(domain);
    let alt = if main == BILLING_BASE_CN {
        BILLING_BASE_GLOBAL
    } else {
        BILLING_BASE_CN
    };
    [main, alt]
}

// ── 区域端点结构体（buddy-ai 国际版，阶段 1 开工项）────────────────────────
//
// 《Buddy国际版支持落地方案.md》§4.1：从硬编码提升为 WbRegionEndpoints，
// CN/Global 两份静态实例。Gate 阻塞层已全部实测收口（2026-10-15，§5）：
// R-1 OAuth 全链路 / R-2a refresh / R-3 身份 / R-6 billing 字段同构。
// 身份基座 = WorkBuddy 桌面端身份（§8-1 推荐候选：模型目录最全 51 个、
// 两外部项目长期实证）；UA/版本号单点维护，版本更新仅改常量。

/// 区域端点集（常量仅承载，不接线；阶段 2 网关转发按账号 region 分流——
/// chat/refresh/billing 三字段届时由 wb_upstream/wb_common 消费，当前仅 auth 链读用）
#[allow(dead_code)]
pub struct WbRegionEndpoints {
    /// chat 网关 base（/v2/chat/completions）
    pub chat_base: &'static str,
    /// OAuth 三件套 base（auth/state → auth/token → login/account）
    pub auth_base: &'static str,
    /// authUrl 缺失/不完整时的登录页构造 base（resolve_auth_url 回退形态）
    pub login_base: &'static str,
    /// auth/state platform 参数——国际版的灵魂差异（附录 B 修正 3：必须纳入字段）
    pub platform: &'static str,
    /// OAuth 请求 Origin/Referer（CN=www.codebuddy.cn / Intl=www.workbuddy.ai）
    pub web_origin: &'static str,
    /// OAuth ④ 步账号资料端点路径（R-1：国际域单数 `/v2/plugin/auth/account` 404，
    /// 账号标识走 JWT sub 兜底；CN 为 `/v2/plugin/login/account`）
    pub account_path: &'static str,
    /// token refresh base（/v2/plugin/auth/token/refresh 前缀）
    pub refresh_base: &'static str,
    /// billing/meter base
    pub billing_base: &'static str,
    /// 出站身份 UA（桌面腿）
    pub ua: &'static str,
    /// 客户端版本（authUrl version 参数，R-1：官方桌面端实证 5.7.6；可选参数）
    pub client_version: &'static str,
}

/// CN 区（2026-10-18 实测：platform=workbuddy + WorkBuddy 桌面 UA 两者同时满足，
/// 服务端才返回 www.workbuddy.cn/login 新登录页；老 UA 下改 platform 无效、
/// 新 UA 下 platform=CLI 亦回老链接，Origin 无影响。auth API 上游仍是 copilot.tencent.com）
pub const REGION_CN: WbRegionEndpoints = WbRegionEndpoints {
    chat_base: "https://copilot.tencent.com",
    auth_base: "https://copilot.tencent.com",
    login_base: "https://www.workbuddy.cn",
    platform: "workbuddy",
    web_origin: "https://www.codebuddy.cn",
    account_path: "/v2/plugin/login/account",
    refresh_base: "https://www.codebuddy.cn",
    billing_base: BILLING_BASE_CN,
    // 与国际区同形态三段式（对齐官方桌面端 5.8.3 抓包；auth 探针已验证）
    ua: "WorkBuddy/5.8.3 WorkBuddy/5.8.3 CLI/2.161.0",
    client_version: "5.8.3",
};

/// Global 区（buddy-ai）：workbuddy.ai 单一鉴权门面（R-9 实锤两域同账号体系，无双门面）
pub const REGION_GLOBAL: WbRegionEndpoints = WbRegionEndpoints {
    chat_base: "https://www.workbuddy.ai",
    auth_base: "https://www.workbuddy.ai",
    login_base: "https://www.workbuddy.ai",
    platform: "workbuddy-ai",
    web_origin: "https://www.workbuddy.ai",
    account_path: "/v2/plugin/auth/account",
    refresh_base: "https://www.workbuddy.ai",
    billing_base: BILLING_BASE_GLOBAL,
    // R-3 附录 A：桌面端身份 `WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/2.137.1`
    ua: "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/2.137.1",
    // R-1：官方桌面端 authUrl 实证带 version=5.7.6（比 UA 基准 5.5.2 新，跟进官方）
    client_version: "5.7.6",
};

/// 区域端点取用（唯一入口，杜绝散落硬编码；阶段 2 网关转发接线时启用）
#[allow(dead_code)]
pub fn region_endpoints(global_region: bool) -> &'static WbRegionEndpoints {
    if global_region {
        &REGION_GLOBAL
    } else {
        &REGION_CN
    }
}

/// token refresh URL 按区域收敛（G-2）：三处硬编码统一到本函数。
/// R-2a 实测收口（2026-10-15）：`www.workbuddy.ai/v2/plugin/auth/token/refresh` code 0，
/// `X-Auth-Refresh-Source` 取 plugin / workbuddy 均接受，refreshToken 每次轮换（回写为准）。
pub fn region_refresh_url(domain: &str) -> &'static str {
    if is_global_region(domain) {
        "https://www.workbuddy.ai/v2/plugin/auth/token/refresh"
    } else {
        REFRESH_URL
    }
}

// ── token 刷新（F-09）──────────────────────────────────────────────────────

/// refresh 失败原因（审查 P1-4：区分「凭证失效」与「网络故障」，
/// 供调用方决定是否标记 needs_relogin——网络失败不应误标）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFail {
    /// 无 refreshToken（不可刷新）
    NoRefreshToken,
    /// 网络不可达 / 传输层失败（不标记重登录，下次重试）
    Network,
    /// HTTP 4xx：refresh token 已失效（应标记 needs_relogin）
    Auth,
    /// 200 但响应无 accessToken（响应结构异常，不标记重登录）
    BadResponse,
    /// H-1：跨进程锁未获取（他方进程正在刷新同账号，本侧幂等跳过；不标记重登录）
    Busy,
}

/// refresh 4xx 响应体 → 上游错误细节（issue #118）：提取 OAuth 标准
/// error/error_description（如 `invalid_grant: Session doesn't have required client`）
/// 及常见 message 字段；JSON 不可解析时回退原文截断（≤160 字符），均空返回 None。
/// 供 relogin_reason / 网关与手动刷新错误透传，让用户能区分「会话失效（重登可恢复）」
/// 与「疑似封禁」，不必再来 issue 求证。
pub fn refresh_fail_detail(parsed: Option<&Value>, raw: &str) -> Option<String> {
    // 审查修复：空白压缩——HTML/WAF 错误页多行原文会破坏 app_log 单行格式与 UI 展示
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(v) = parsed {
        let mut parts: Vec<&str> = Vec::new();
        for k in [
            "error",
            "error_description",
            "message",
            "msg",
            "errorMessage",
            "description",
        ] {
            if let Some(s) = v
                .get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if !parts.contains(&s) {
                    parts.push(s);
                }
            }
        }
        if !parts.is_empty() {
            return Some(collapse(&parts.join(": ")));
        }
    }
    let raw = collapse(raw.trim());
    if raw.is_empty() {
        return None;
    }
    let cut = raw
        .char_indices()
        .nth(160)
        .map(|(i, _)| i)
        .unwrap_or(raw.len());
    Some(raw[..cut].to_string())
}

/// 上游错误细节 → 用户可读处置提示（issue #118）。
/// 封禁/限制类关键词优先判定：避免混合文案（如 "session ... (account blocked)"）
/// 被会话失效分支误安抚为「重登可恢复」；不含 forbidden（WAF/HTTP 403 页
/// 普遍含该词，避免拦截页被误判为封禁）。
pub fn refresh_fail_hint(detail: &str) -> &'static str {
    let d = detail.to_ascii_lowercase();
    if d.contains("disabled")
        || d.contains("banned")
        || d.contains("suspended")
        || d.contains("blocked")
    {
        "疑似账号被禁用/限制，请到官方平台确认"
    } else if d.contains("invalid_grant") || d.contains("session") || d.contains("token is not active") {
        "服务端会话已失效（被迫下线，非账号封禁），重新登录即可恢复"
    } else {
        "refresh token 已被服务端拒绝，重新登录或重新导入凭证可恢复"
    }
}

/// refresh 4xx → 用户可读错误消息（issue #118）：读取 ureq `Error::Status(code, resp)`
/// 的响应体，经 refresh_fail_detail / refresh_fail_hint 透传上游细节；
/// 无可提取细节时回退 fallback（调用方组装，含 HTTP 状态码文案）。
pub fn refresh_fail_msg(code: u16, resp: ureq::Response, fallback: &str) -> String {
    let raw = resp.into_string().unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(&raw).ok();
    match refresh_fail_detail(parsed.as_ref(), &raw) {
        Some(d) => format!("刷新失败（HTTP {code}）：{d}（{}）", refresh_fail_hint(&d)),
        None => fallback.to_string(),
    }
}

/// 调 plugin refresh 端点（X-Refresh-Token 仅允许出现在此端点）。
/// 成功返回新 Creds（expires 字段按 expiresIn/refreshExpiresIn 秒数回填）。
pub fn refresh_token_once(agent: &ureq::Agent, creds: &Creds) -> Option<Creds> {
    refresh_token_once_ex(agent, creds).0
}

/// refresh_token_once 的带失败原因版本（审查 P1-4）。
/// 第三元为 4xx 时的上游错误细节（issue #118，如 invalid_grant 及其描述；见 refresh_fail_detail）。
pub fn refresh_token_once_ex(
    agent: &ureq::Agent,
    creds: &Creds,
) -> (Option<Creds>, RefreshFail, Option<String>) {
    if creds.refresh_token.is_empty() {
        return (None, RefreshFail::NoRefreshToken, None);
    }
    let mut h = build_auth_headers(creds, false);
    h.push(("X-Refresh-Token".to_string(), creds.refresh_token.clone()));
    h.push((
        "X-Auth-Refresh-Source".to_string(),
        "workbuddy".to_string(),
    ));
    let (status, parsed, raw) = post_json_raw(agent, REFRESH_URL, &h, &serde_json::json!({}));
    if status == 0 {
        return (None, RefreshFail::Network, None);
    }
    if (400..500).contains(&status) {
        let detail = refresh_fail_detail(parsed.as_ref(), &raw);
        return (None, RefreshFail::Auth, detail);
    }
    if status != 200 {
        return (None, RefreshFail::Network, None); // 5xx 等服务端故障：不标记重登录
    }
    let Some(body) = parsed else {
        return (None, RefreshFail::BadResponse, None);
    };
    let acc = s_of(fs_utils::dig(&body, &["accessToken"]));
    if acc.is_empty() {
        return (None, RefreshFail::BadResponse, None);
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut out = creds.clone();
    out.access_token = acc;
    let ref_tok = s_of(fs_utils::dig(&body, &["refreshToken"]));
    if !ref_tok.is_empty() {
        out.refresh_token = ref_tok;
    }
    out.expires_at_ms = i_of(fs_utils::dig(&body, &["expiresIn"])).map(|e| now_ms + e * 1000);
    out.refresh_expires_at_ms = i_of(fs_utils::dig(&body, &["refreshExpiresIn"]))
        .map(|e| now_ms + e * 1000);
    (Some(out), RefreshFail::NoRefreshToken, None)
}

/// 标记账号需重新登录（审查 P1-4：刷新凭证失效时回写账号池，
/// 网关池同步（sync_from_wb disabled=needs_relogin）与调度跳过随之生效）
pub fn mark_needs_relogin(state: &AppState, acct_id: &str, reason: &str) {
    let store = crate::store::db(&state.data_dir);
    // 读-改-写互斥：罩住 load→改→save 段，防命令/后台/调度线程旧副本整文档覆盖
    let _guard = state.wb_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = crate::store::docs::wb_pool_load(&store);
    let mut changed = false;
    if let Some(arr) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for a in arr {
            if a.get("id").and_then(Value::as_str) == Some(acct_id) {
                a["needs_relogin"] = serde_json::json!(true);
                a["relogin_reason"] = serde_json::json!(reason);
                changed = true;
            }
        }
    }
    if changed {
        let _ = crate::store::docs::wb_pool_save(&store, &pool);
        fs_utils::app_log(
            &state.data_dir,
            &format!("wb: 账号 {acct_id} 已标记需重新登录（{reason}）"),
        );
    }
}

// ── 本地 quota 端口发现兜底（T5.8/F-21）────────────────────────────────────
// 云端 billing 全链失败时的最后兜底：WorkBuddy/CodeBuddy 桌面端本地服务在
// 127.0.0.1 暴露 quota 查询端点。发现顺序：
// ① ~/.workbuddy/*.port 文件声明的端口；② 固定候选端口；③ 有界端口段。
// 红线：单次单发不重试、每端口 0.8s 超时、候选总数有界。

const QUOTA_PATH: &str = "/api/v1/quota";
const QUOTA_PORT_CANDIDATES: [u16; 4] = [18789, 11101, 8890, 8899];
const QUOTA_PORT_RANGE: std::ops::RangeInclusive<u16> = 18780..=18795;

// 审查修复（quota 探测闸门）：容器/无桌面客户端环境下本机无 quota 端点可连，
// *.port 恒空后仍会对固定候选 + 有界端口段逐端口串行探测，每端口烧满 0.8s
// 超时，阻塞积分/调度线程。环境变量 AIWORK_DISABLE_LOCAL_QUOTA 置 1/true
//（不区分大小写）时整体跳过本地探测，直接空返回（与探测失败同语义：上游
// local_quota_balance → None，按「云端全链失败且无本地兜底」处理）。
const LOCAL_QUOTA_DISABLE_ENV: &str = "AIWORK_DISABLE_LOCAL_QUOTA";

/// 闸门取值判定：仅 1 / true（不区分大小写、容忍首尾空白）视为开启；
/// 未设置/0/false/其他值均不开启（fail-open 保持原探测行为）。
fn env_flag_on(v: Option<&str>) -> bool {
    v.map(|s| {
        let s = s.trim();
        s.eq_ignore_ascii_case("1") || s.eq_ignore_ascii_case("true")
    })
    .unwrap_or(false)
}

fn local_quota_disabled() -> bool {
    match std::env::var(LOCAL_QUOTA_DISABLE_ENV) {
        Ok(v) => env_flag_on(Some(&v)),
        Err(_) => false,
    }
}

/// 响应含 remaining/credits/quota/balance 任一键即认定 quota 端点（浅层宽容）。
fn quota_looks_valid(v: &Value, depth: usize) -> bool {
    if depth > 3 {
        return false;
    }
    if let Some(map) = v.as_object() {
        for (k, val) in map {
            let kl = k.to_ascii_lowercase();
            if kl == "remaining" || kl == "credits" || kl == "quota" || kl == "balance" {
                return true;
            }
            if quota_looks_valid(val, depth + 1) {
                return true;
            }
        }
    }
    false
}

fn quota_probe_port(agent: &ureq::Agent, port: u16) -> Option<Value> {
    let url = format!("http://127.0.0.1:{port}{QUOTA_PATH}");
    let resp = agent
        .get(&url)
        .timeout(Duration::from_millis(800))
        .set("User-Agent", "WorkBuddy")
        .set("Accept", "application/json")
        .call()
        .ok()?;
    let raw = resp.into_string().ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    quota_looks_valid(&v, 0).then_some(v)
}

/// 发现本机 quota 服务：*.port 声明端口 → 固定候选 → 有界端口段。
/// 返回已确认可用的 (port, 响应)（按发现序，至多 limit 个）。
pub fn discover_local_quota_services(
    agent: &ureq::Agent,
    limit: usize,
) -> Vec<(u16, Value)> {
    // 审查修复（quota 探测闸门）：AIWORK_DISABLE_LOCAL_QUOTA=1/true 时视本地
    // quota 数据不可用，跳过 *.port / 固定候选 / 端口段的逐端口探测循环，直接空返回
    if local_quota_disabled() {
        return vec![];
    }
    let mut found: Vec<(u16, Value)> = vec![];
    let mut seen: std::collections::HashSet<u16> = Default::default();
    let mut ports: Vec<u16> = vec![];
    // ① ~/.workbuddy/*.port（服务启动时落盘的端口声明，最多扫 16 个）
    if let Ok(home) = std::env::var("USERPROFILE") {
        let wb_dir = PathBuf::from(home).join(".workbuddy");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&wb_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "port").unwrap_or(false))
            .collect();
        files.sort();
        for f in files.into_iter().take(16) {
            if let Ok(txt) = std::fs::read_to_string(&f) {
                if let Some(first) = txt.split_whitespace().next() {
                    if let Ok(p) = first.parse::<u16>() {
                        ports.push(p);
                    }
                }
            }
        }
    }
    // ② 固定候选 ③ 有界端口段
    for p in ports
        .into_iter()
        .chain(QUOTA_PORT_CANDIDATES.into_iter())
        .chain(QUOTA_PORT_RANGE.clone())
    {
        if seen.contains(&p) || p == 0 {
            continue;
        }
        seen.insert(p);
        if found.len() >= limit {
            return found;
        }
        if let Some(v) = quota_probe_port(agent, p) {
            found.push((p, v));
        }
    }
    found
}

/// 本地 quota 兜底余额：首个可用端点的 remaining/credits/balance 取数；无可用端点 None。
pub fn local_quota_balance(agent: &ureq::Agent) -> Option<f64> {
    for (_port, v) in discover_local_quota_services(agent, 2) {
        if let Some(num) = dig_num(fs_utils::dig(
            &v,
            &["remaining", "RemainingCapacity", "credits", "balance"],
        )) {
            return Some(num);
        }
    }
    None
}

/// 宽容取数：递归摘出首个数值（含数字字符串）。
pub fn dig_num(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Null => None,
        Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Object(m) => m.values().find_map(|c| dig_num(Some(c))),
        Value::Array(a) => a.iter().find_map(|c| dig_num(Some(c))),
    }
}

// ── 惰性刷新（F-09/F-55，原 wb_common.ensure_fresh）────────────────────────

/// 惰性刷新：距过期 < lazy_hours 才刷；一次调用最多一次刷新。
/// 返回 (creds, refreshed, note)，note ∈ no_credential/vault_down/fresh/
/// expired_needs_relogin/refreshed/refresh_failed。
/// vault_down（issue #153 修复 2）：vault 系统性故障（DPAPI/快照/锁）导致凭证
/// 读出为空，与「确实无凭证」区分——暂态语义，调用方不得据此永久禁用账号。
///
/// H-1（P0 修复）：「读凭证→网络刷新→落库」全程持每账号刷新锁（进程内）+ 跨进程
/// 命名互斥体，双进程/多通道并发触发同账号刷新时串行化；持锁后重读凭证构成二次
/// 检查——他人已刷新落库则直接命中新凭证（fresh），不再重复发网络请求。新鲜快路径
/// 无锁直返（调度/签到热路径不承担锁开销）。
pub fn ensure_fresh(
    state: &AppState,
    agent: &ureq::Agent,
    acct: &Value,
    lazy_hours: i64,
) -> (Creds, bool, &'static str) {
    let acct_id = acct.get("id").and_then(Value::as_str).unwrap_or("");
    let acct_uid = acct.get("uid").and_then(Value::as_str).unwrap_or("");
    let creds = effective_creds(state, acct_id, acct_uid);
    if creds.access_token.is_empty() {
        // 修复 2（issue #153）：vault 系统性故障时 ns_get fail-secure 返回 None，
        // 与「无凭证」不可区分——故障期返回暂态 vault_down，调用方按暂态处理
        // （不永久禁用账号，故障恢复后自动自愈）
        if !crate::vault::available(&state.data_dir) {
            return (creds, false, "vault_down");
        }
        return (creds, false, "no_credential");
    }
    if let Some(exp) = creds.expires_at_ms {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let remain_h = (exp - now_ms) as f64 / 3_600_000.0;
        if remain_h > lazy_hours as f64 {
            return (creds, false, "fresh");
        }
        if remain_h < 0.0 && creds.refresh_token.is_empty() {
            return (creds, false, "expired_needs_relogin");
        }
    }
    // H-1 ①：进程内每账号锁串行化；②跨进程命名互斥体（GUI/CLI 双进程）
    let lock = refresh_lock_for(acct_id);
    let _refresh_guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_cross) = cross_refresh_lock(&state.data_dir, acct_id) else {
        // 他方进程刷新中：幂等跳过（沿用现有凭证，不误标 needs_relogin）
        return (creds, false, "refresh_failed");
    };
    // 二次检查：他人已刷新落库 → 直接复用新凭证（不发网络请求）
    let creds = effective_creds(state, acct_id, acct_uid);
    if creds.access_token.is_empty() {
        if !crate::vault::available(&state.data_dir) {
            return (creds, false, "vault_down");
        }
        return (creds, false, "no_credential");
    }
    if let Some(exp) = creds.expires_at_ms {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let remain_h = (exp - now_ms) as f64 / 3_600_000.0;
        if remain_h > lazy_hours as f64 {
            return (creds, false, "fresh");
        }
        if remain_h < 0.0 && creds.refresh_token.is_empty() {
            return (creds, false, "expired_needs_relogin");
        }
    }
    if let Some(new) = refresh_token_once(agent, &creds) {
        let _ = save_token_store(state, acct_id, &new);
        return (new, true, "refreshed");
    }
    (creds, false, "refresh_failed")
}

/// 401 自愈专用加锁刷新（H-1）：串行化「网络刷新→落库」并做二次检查——他人已刷新
/// 落库（token store 中该账号 access_token 与调用方持有凭证不同）则直接复用新凭证，
/// 不再重复发网络请求；否则以调用方凭证刷新并落库（调用方无须再自行 save）。
/// 跨进程抢锁失败 → (None, Some(RefreshFail::Busy), None)：调用方按非 Auth 处理
///（不误标 needs_relogin，下次自然重试）。
/// 第三元为 4xx 时的上游错误细节（issue #118，见 refresh_fail_detail）。
pub fn refresh_token_once_locked(
    state: &AppState,
    agent: &ureq::Agent,
    acct_id: &str,
    stale: &Creds,
) -> (Option<Creds>, Option<RefreshFail>, Option<String>) {
    let lock = refresh_lock_for(acct_id);
    let _refresh_guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_cross) = cross_refresh_lock(&state.data_dir, acct_id) else {
        return (None, Some(RefreshFail::Busy), None);
    };
    // 二次检查：store 权威源中该账号已换新 token（他人刷新落库）→ 直接复用。
    // 仅比对 store 记录（刷新产物只落 store，不落 auth 文件），避免 auth 文件
    // 属于其他账号时误判
    let store_cur: Creds = load_token_store(state)
        .get("tokens")
        .and_then(|t| t.get(acct_id))
        .map(creds_of)
        .unwrap_or_default();
    if !store_cur.access_token.is_empty() && store_cur.access_token != stale.access_token {
        return (Some(store_cur), None, None);
    }
    let (new, fail, detail) = refresh_token_once_ex(agent, stale);
    if let Some(n) = &new {
        let _ = save_token_store(state, acct_id, n);
    }
    (new, Some(fail), detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== 客户端指纹伪装（issue #48） ====================

    #[test]
    fn derive_device_fingerprint_deterministic_and_isolated() {
        // 同账号恒定、32 位小写 hex；不同 kind / 不同 uid 互异；uid 空缺失即不带
        let a1 = derive_device_fingerprint("u-1", "machine").unwrap();
        let a2 = derive_device_fingerprint("u-1", "machine").unwrap();
        assert_eq!(a1, a2, "同账号指纹必须恒定");
        assert_eq!(a1.len(), 32);
        assert!(a1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(
            derive_device_fingerprint("u-1", "machine").unwrap(),
            derive_device_fingerprint("u-1", "session").unwrap()
        );
        assert_ne!(
            derive_device_fingerprint("u-1", "machine").unwrap(),
            derive_device_fingerprint("u-2", "machine").unwrap(),
            "跨账号必须隔离防关联"
        );
        assert!(derive_device_fingerprint("", "machine").is_none());
        assert!(derive_device_fingerprint("  ", "machine").is_none());
    }

    #[test]
    fn auth_headers_carry_client_fingerprint() {
        let creds = Creds {
            access_token: "tk".into(),
            uid: "u-1".into(),
            domain: "d1".into(),
            ..Default::default()
        };
        let h = build_auth_headers(&creds, false);
        let get = |hs: &[(String, String)], k: &str| {
            hs.iter()
                .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
        };
        // UA 带版本（issue #48：裸 "WorkBuddy" 在个人中心明细识别为 "-"）
        assert_eq!(get(&h, "User-Agent").as_deref(), Some(WB_DESKTOP_UA));
        assert_eq!(get(&h, "X-User-Id").as_deref(), Some("u-1"));
        assert_eq!(get(&h, "X-Domain").as_deref(), Some("d1"));
        // 设备/会话指纹：与派生函数一致、32 位 hex
        let mid = get(&h, "X-Machine-ID").expect("X-Machine-ID 必须存在");
        let sid = get(&h, "X-Session-ID").expect("X-Session-ID 必须存在");
        assert_eq!(mid, derive_device_fingerprint("u-1", "machine").unwrap());
        assert_eq!(sid, derive_device_fingerprint("u-1", "session").unwrap());
        assert_eq!(mid.len(), 32);
        // web 平台附加
        let hw = build_auth_headers(&creds, true);
        assert!(hw.iter().any(|(k, v)| k == "X-Client-Platform" && v == "web"));
        // uid/domain 缺失：X-No-* 占位、不带指纹与域标识
        let empty = build_auth_headers(&Creds::default(), false);
        assert_eq!(get(&empty, "X-No-User-Id").as_deref(), Some("1"));
        assert!(get(&empty, "X-User-Id").is_none());
        assert!(get(&empty, "X-Machine-ID").is_none());
        assert!(get(&empty, "X-Session-ID").is_none());
        assert!(get(&empty, "X-Domain").is_none());
    }

    // ==================== refresh 4xx 透传（issue #118） ====================

    #[test]
    fn refresh_fail_detail_extracts_oauth_fields() {
        // OAuth 标准字段：error + error_description 拼接（issue #118 原始报文形态）
        let v: Value = serde_json::from_str(
            r#"{"error":"invalid_grant","error_description":"Session doesn't have required client"}"#,
        )
        .unwrap();
        let d = refresh_fail_detail(Some(&v), "").unwrap();
        assert!(d.contains("invalid_grant") && d.contains("required client"), "{d}");

        // 同文案去重：description 与 error_description 相同时不重复拼接
        let v: Value =
            serde_json::from_str(r#"{"error":"invalid_grant","error_description":"gone","description":"gone"}"#)
                .unwrap();
        let d = refresh_fail_detail(Some(&v), "").unwrap();
        assert_eq!(d.matches("gone").count(), 1, "{d}");

        // JSON 无识别字段 → 回退原文；原文为空白 / 缺失 → None
        let v = serde_json::json!({ "unexpected": 1 });
        assert_eq!(
            refresh_fail_detail(Some(&v), "plain gateway error").as_deref(),
            Some("plain gateway error")
        );
        assert_eq!(refresh_fail_detail(Some(&v), "   "), None);
        assert_eq!(refresh_fail_detail(None, ""), None);
    }

    #[test]
    fn refresh_fail_detail_truncates_long_raw_on_char_boundary() {
        // 300 个多字节汉字：按字符（非字节）边界截断到 160 字符且为严格前缀（不 panic）
        let raw = "错".repeat(300);
        let d = refresh_fail_detail(None, &raw).unwrap();
        assert_eq!(d.chars().count(), 160);
        assert!(raw.starts_with(&d), "截断结果必须是原文前缀");
        // ASCII 长文同样按 160 字符截断
        let d = refresh_fail_detail(None, &"a".repeat(300)).unwrap();
        assert_eq!(d.chars().count(), 160);
        // 审查修复：多行 HTML/WAF 原文压缩为单行（不破坏 app_log 单行格式）
        let d = refresh_fail_detail(None, "<html>\r\n  <body>Blocked\r\n</body>\n</html>").unwrap();
        assert_eq!(d, "<html> <body>Blocked </body> </html>");
        // 纯空白原文压缩后为空 → None
        assert_eq!(refresh_fail_detail(None, " \n\t "), None);
    }

    #[test]
    fn refresh_fail_hint_classifies_by_keywords() {
        // 会话失效类（issue #118 原始场景）
        assert!(refresh_fail_hint("invalid_grant: Session doesn't have required client").contains("会话已失效"));
        assert!(refresh_fail_hint("token is not active").contains("会话已失效"));
        // 封禁关键词优先于会话词：混合文案不得被会话分支误安抚为「重登可恢复」
        assert!(refresh_fail_hint("Session terminated (account blocked)").contains("禁用"));
        assert!(refresh_fail_hint("user is banned").contains("禁用"));
        // 通用拒绝 → 重新登录/导入凭证
        assert!(refresh_fail_hint("invalid_client").contains("重新登录或重新导入"));
    }

    // ==================== 本地 quota 探测闸门（审查第 5 项） ====================

    #[test]
    fn local_quota_disable_flag_only_accepts_1_or_true() {
        // 审查修复（quota 探测闸门）：仅 1/true（任意大小写、容忍首尾空白）开启；
        // 未设置/0/false/其他值一律不开启（fail-open 保持原探测行为）。
        // 直接测纯函数而非 set_var：进程级 setenv 在并行测试下有竞态（见 api_server.rs）。
        assert!(env_flag_on(Some("1")));
        assert!(env_flag_on(Some("true")));
        assert!(env_flag_on(Some("TRUE")));
        assert!(env_flag_on(Some("True")));
        assert!(env_flag_on(Some(" 1 ")));
        assert!(env_flag_on(Some(" true ")));
        assert!(!env_flag_on(None));
        assert!(!env_flag_on(Some("")));
        assert!(!env_flag_on(Some("0")));
        assert!(!env_flag_on(Some("false")));
        assert!(!env_flag_on(Some("yes")));
        assert!(!env_flag_on(Some("10")));
        assert!(!env_flag_on(Some("enabled")));
    }
}
