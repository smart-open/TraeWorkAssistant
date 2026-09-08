//! F-08 双应用账号自动发现 + Trae 会员/套餐信息展示
//!
//! 数据来源（本机实测，2026-09-07）：
//! - `%APPDATA%\TRAE SOLO CN\User\globalStorage\storage.json`（Trae Work）
//! - `%APPDATA%\Trae CN\User\globalStorage\storage.json`（Trae CN IDE）
//!
//! **两套 uid 体系（重要）**：
//! - `iCubeAuthInfo://icube-dc:<uid>` 键名中的 uid 是**账户中心（dc）id 空间**，
//!   与账号池 / JWT `data.id` 的 **Cloud-IDE id 空间不是同一体系**（实测同一登录账号
//!   dc=199439841787403 vs Cloud-IDE=2328112497170937），直接用会导致重复入池。
//! - 因此当前登录账号的 Cloud-IDE uid 由本机使用痕迹推导：
//!   1. Trae CN：storage.json `icube_gtm.users` 键名（仅记录当前/近期使用用户）；
//!   2. Trae Work：state.vscdb（SQLite ItemTable）`solo.mobile.allowControl` 的
//!      per-uid `updatedTime` 最新者，辅以 `<uid>:*` / `:user:<uid>` 键名证据计数；
//!   3. 两应用证据合并取（最新时间, 证据数）最大者。
//! - 推导失败时回退展示 dc uid，但标记 `uid_confident=false` 并禁止入池，
//!   避免再产生跨体系的重复账号。
//!
//! 套餐信息：storage.json 键 `iCubeServerData://icube.cloudide`（明文 JSON），
//! `entitlementInfo.identityStr` / `identity` 与 `ide_user_pay_status` 接口的
//! `user_pay_identity_str` 同源。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::State;

use crate::fs_utils;
use crate::state::AppState;

const STORAGE_SUFFIX: &str = r"User\globalStorage\storage.json";
const VSCDB_SUFFIX: &str = r"User\globalStorage\state.vscdb";

/// 单个应用的数据目录候选（按优先级）
fn app_data_dirs(app_kind: &str) -> Vec<std::path::PathBuf> {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    match app_kind {
        "TraeWork" => vec![
            std::path::PathBuf::from(&appdata).join("TRAE SOLO CN"),
            std::path::PathBuf::from(&appdata).join("TRAE SOLO"),
        ],
        "Trae" => vec![std::path::PathBuf::from(&appdata).join("Trae CN")],
        _ => vec![],
    }
}

fn app_label(app_kind: &str) -> &str {
    match app_kind {
        "TraeWork" => "Trae Work",
        "Trae" => "Trae",
        _ => app_kind,
    }
}

/// 读取应用的 storage.json（多个候选目录取第一个存在的）
fn read_storage_json(app_kind: &str) -> Option<serde_json::Value> {
    for dir in app_data_dirs(app_kind) {
        let p = dir.join(STORAGE_SUFFIX);
        if p.is_file() {
            if let Ok(s) = std::fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// 从 storage.json 提取登录账号的账户中心 uid 列表（键名 `iCubeAuthInfo://icube-dc:<uid>`）。
/// 注意：这是账户中心 id 空间，不能与账号池（Cloud-IDE uid）直接比对。
fn extract_dc_uids(storage: &serde_json::Value) -> Vec<String> {
    let mut uids = Vec::new();
    if let Some(obj) = storage.as_object() {
        for key in obj.keys() {
            if let Some(uid) = key.strip_prefix("iCubeAuthInfo://icube-dc:") {
                let uid = uid.trim();
                if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) {
                    uids.push(uid.to_string());
                }
            }
        }
    }
    uids.sort();
    uids.dedup();
    uids
}

// ---------------- Cloud-IDE uid 推导 ----------------

/// 单个 Cloud-IDE uid 的本机使用证据
#[derive(Default, Clone)]
struct UidEvidence {
    /// 最新使用时间（Unix 毫秒；无时间戳证据为 0）
    latest_ts_ms: i64,
    /// 出现次数（键名证据计数）
    count: i64,
}

/// 判断 token 是否为 15~16 位纯数字 id
fn is_uid_token(t: &str) -> bool {
    (15..=16).contains(&t.chars().count()) && t.chars().all(|c| c.is_ascii_digit())
}

/// 把 "YYYY-MM" 转为近似时间戳（当月 1 日 0 点，Unix 秒→毫秒），用于与毫秒时间戳同维度比较
fn month_to_ts_ms(month: &str) -> i64 {
    let parts: Vec<&str> = month.split('-').collect();
    if parts.len() != 2 {
        return 0;
    }
    let (Ok(y), Ok(m)) = (parts[0].parse::<i32>(), parts[1].parse::<u32>()) else {
        return 0;
    };
    if !(1..=12).contains(&m) {
        return 0;
    }
    chrono::NaiveDate::from_ymd_opt(y, m, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp_millis())
        .unwrap_or(0)
}

/// 从 state.vscdb（SQLite ItemTable）提取 per-uid 使用证据。
/// 覆盖的键模式（本机实测）：
/// - `solo.mobile.allowControl`：JSON `{uid: {updatedTime}}`，含精确毫秒时间戳（最强证据）
/// - `<uid>:...` 键名前缀（如 `<uid>:AI.agent.model...`）
/// - `*:user:<uid>[:YYYY-MM]`（如 `commercial-banner-popup:...:user:<uid>:2026-09`）
/// - `solo-lite-mode-state-map-<uid>`
fn vscdb_uid_evidence(app_kind: &str) -> HashMap<String, UidEvidence> {
    let mut out: HashMap<String, UidEvidence> = HashMap::new();
    let Some(db_path) = app_data_dirs(app_kind)
        .into_iter()
        .map(|d| d.join(VSCDB_SUFFIX))
        .find(|p| p.is_file())
    else {
        return out;
    };
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return out;
    };
    // 简化：直接查询全部键值
    let mut stmt = match conn.prepare("SELECT key, value FROM ItemTable") {
        Ok(s) => s,
        Err(_) => return out,
    };
    let rows = stmt.query_map([], |row| {
        let key: String = row.get(0)?;
        let value: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
        Ok((key, value))
    });
    let rows = match rows {
        Ok(r) => r,
        Err(_) => return out,
    };
    for row in rows.flatten() {
        let (key, value) = row;
        // 1) solo.mobile.allowControl：JSON {uid: {updatedTime}} —— 精确时间戳
        if key == "solo.mobile.allowControl" {
            if let Ok(map) = serde_json::from_str::<serde_json::Value>(&value) {
                if let Some(obj) = map.as_object() {
                    for (uid, info) in obj {
                        if !is_uid_token(uid) {
                            continue;
                        }
                        let ts = info
                            .get("updatedTime")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        let e = out.entry(uid.clone()).or_default();
                        e.latest_ts_ms = e.latest_ts_ms.max(ts);
                        e.count += 2;
                    }
                }
            }
            continue;
        }
        // 2) `solo-lite-mode-state-map-<uid>`
        if let Some(rest) = key.strip_prefix("solo-lite-mode-state-map-") {
            if is_uid_token(rest.trim()) {
                let e = out.entry(rest.trim().to_string()).or_default();
                e.count += 2;
            }
            continue;
        }
        // 3) 键名冒号分段扫描：`<uid>:...` 前缀 与 `:user:<uid>[:YYYY-MM]`
        let tokens: Vec<&str> = key.split(':').collect();
        for (i, t) in tokens.iter().enumerate() {
            let tt = t.trim();
            // `user:<uid>` 模式：优先取 user 后面的 uid（避免把月份段误判）
            let _is_user_pattern = i > 0 && tokens[i - 1].trim() == "user";
            if !is_uid_token(tt) {
                continue;
            }
            let e = out.entry(tt.to_string()).or_default();
            e.count += 1;
            // 紧随 uid 的 YYYY-MM 段 → 月度时间证据
            if let Some(next) = tokens.get(i + 1) {
                let nt = next.trim();
                if nt.len() == 7 && nt.as_bytes()[4] == b'-' {
                    let ts = month_to_ts_ms(nt);
                    e.latest_ts_ms = e.latest_ts_ms.max(ts);
                }
            }
        }
    }
    out
}

/// 从 storage.json 提取 Cloud-IDE uid 证据：
/// `icube_gtm.users` 键名仅记录当前/近期使用的用户（本机实测恒为当前登录账号）。
fn storage_uid_evidence(storage: &serde_json::Value) -> HashMap<String, UidEvidence> {
    let mut out = HashMap::new();
    if let Some(users) = storage
        .get("icube_gtm")
        .and_then(|g| g.get("users"))
        .and_then(|u| u.as_object())
    {
        for uid in users.keys() {
            if is_uid_token(uid) {
                let e: &mut UidEvidence = out.entry(uid.clone()).or_default();
                // gtm.users 是强证据：权重 3
                e.count += 3;
            }
        }
    }
    out
}

/// 合并证据并选出当前登录账号的 Cloud-IDE uid：
/// 主排序 = 最新时间戳（无时间戳证据按 count 折算），次排序 = 证据数。
fn select_cloud_uid(evidence: HashMap<String, UidEvidence>) -> Option<String> {
    // 近似折算：无时间戳的证据视作 3 个月前，保证带真实时间戳的证据优先
    let fallback_ts = chrono::Utc::now().timestamp_millis() - 90 * 24 * 3600 * 1000;
    evidence
        .into_iter()
        .max_by_key(|(_, e)| (if e.latest_ts_ms > 0 { e.latest_ts_ms } else { fallback_ts }, e.count))
        .map(|(uid, _)| uid)
}

/// 推导应用当前登录账号的 Cloud-IDE uid（合并 storage.json 与 state.vscdb 证据）
fn infer_cloud_uid(app_kind: &str, storage: &serde_json::Value) -> Option<String> {
    let mut merged = vscdb_uid_evidence(app_kind);
    for (uid, e) in storage_uid_evidence(storage) {
        let slot = merged.entry(uid).or_default();
        slot.count += e.count;
        slot.latest_ts_ms = slot.latest_ts_ms.max(e.latest_ts_ms);
    }
    select_cloud_uid(merged)
}

#[derive(Serialize, Clone)]
pub struct DiscoveredAccount {
    /// 账号池体系（Cloud-IDE）uid。uid_confident=false 时为账户中心 uid（仅展示，不可入池）
    pub user_id: String,
    /// 账户中心（dc）uid —— 与账号池 id 体系不同，仅作诊断展示
    pub dc_uid: Option<String>,
    /// Cloud-IDE uid 是否经本机使用证据确认（false 时 user_id 实为 dc uid，入池会重复）
    pub uid_confident: bool,
    /// 应用类别：TraeWork | Trae
    pub app: String,
    /// 展示名：Trae Work / Trae
    pub app_label: String,
    /// 是否已在账号池
    pub in_pool: bool,
    /// 命中的 storage.json 路径
    pub storage_path: String,
}

/// 账号池已有 uid 集合：包含 user_id 字段与 JWT 解析出的 data.id（部分账号 user_id 为空）
fn pool_uid_set(accounts: &crate::models::AccountsFile) -> std::collections::HashSet<String> {
    accounts
        .accounts
        .iter()
        .flat_map(|a| {
            let mut ids = Vec::new();
            if let Some(uid) = a.user_id.clone() {
                if !uid.is_empty() {
                    ids.push(uid);
                }
            }
            if !a.jwt.trim().is_empty() {
                if let Some(uid) = crate::jwt::parse(&a.jwt).user_id {
                    ids.push(uid);
                }
            }
            ids
        })
        .collect()
}

/// F-08：扫描本机两个 Trae 应用的登录账号（推导 Cloud-IDE uid，标记是否已入池）。
/// async 派发：全表读取 state.vscdb（可达数十 MB）+ storage.json，同步命令会冻住 UI。
#[tauri::command(async)]
pub fn apps_accounts_discover(state: State<AppState>) -> Vec<DiscoveredAccount> {
    let accounts: crate::models::AccountsFile =
        fs_utils::read_json(&state.path("checkin_accounts.json"));
    let known = pool_uid_set(&accounts);

    let mut out = Vec::new();
    for kind in ["TraeWork", "Trae"] {
        // 记录实际命中的 storage 路径用于展示
        let mut path_display = String::new();
        for dir in app_data_dirs(kind) {
            let p = dir.join(STORAGE_SUFFIX);
            if p.is_file() {
                path_display = p.to_string_lossy().to_string();
                break;
            }
        }
        let Some(storage) = read_storage_json(kind) else {
            continue;
        };
        let dc_uids = extract_dc_uids(&storage);
        if dc_uids.is_empty() {
            // 未登录任何账号
            continue;
        }
        // 推导当前登录账号的 Cloud-IDE uid；失败则回退 dc uid（标记不置信，禁止入池）
        match infer_cloud_uid(kind, &storage) {
            Some(cloud_uid) => {
                out.push(DiscoveredAccount {
                    in_pool: known.contains(&cloud_uid),
                    dc_uid: dc_uids.first().cloned(),
                    uid_confident: true,
                    user_id: cloud_uid,
                    app: kind.to_string(),
                    app_label: app_label(kind).to_string(),
                    storage_path: path_display.clone(),
                });
            }
            None => {
                for dc in &dc_uids {
                    out.push(DiscoveredAccount {
                        in_pool: false,
                        dc_uid: Some(dc.clone()),
                        uid_confident: false,
                        user_id: dc.clone(),
                        app: kind.to_string(),
                        app_label: app_label(kind).to_string(),
                        storage_path: path_display.clone(),
                    });
                }
            }
        }
    }
    out
}

/// 补充指定账号的账户中心（icube-dc）id —— **只记录、不展示**（用户确认 2026-09-07）。
///
/// 背景与结论（本机全量实测）：`iCubeAuthInfo://icube-dc:<uid>` 在 8 个不同账号的
/// 快照与两应用 live 数据中恒为同一值（199439841787403），且跨设备标识重置不变——
/// 它是设备/数据中心级标识而非账号 id，不具备账号区分度。按用户要求预留记录，
/// 供未来与外部数据源对账合并；不参与去重/合并/展示。
///
/// 来源优先级：live 应用 storage.json（当前登录最准）→ profiles[_trae]/<uid> 快照。
pub fn backfill_dc_id_for(data_dir: &std::path::Path, user_id: &str) -> Option<String> {
    let uid = user_id.trim();
    if uid.is_empty() {
        return None;
    }
    let accounts_path = data_dir.join("checkin_accounts.json");
    let accounts: crate::models::AccountsFile = fs_utils::read_json(&accounts_path);
    // 已记录则跳过
    if accounts
        .accounts
        .iter()
        .any(|a| a.user_id.as_deref() == Some(uid) && a.dc_id.is_some())
    {
        return None;
    }
    // 候选来源：live 两应用 → 该账号两体系快照
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for kind in ["TraeWork", "Trae"] {
        for dir in app_data_dirs(kind) {
            candidates.push(dir.join(STORAGE_SUFFIX));
        }
    }
    for prof in ["profiles", "profiles_trae"] {
        candidates.push(
            data_dir
                .join("data")
                .join(prof)
                .join(uid)
                .join(STORAGE_SUFFIX),
        );
    }
    let mut found = None;
    for p in candidates {
        if !p.is_file() {
            continue;
        }
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                if let Some(dc) = extract_dc_uids(&v).into_iter().next() {
                    found = Some(dc);
                    break;
                }
            }
        }
    }
    let dc = found?;
    let mut accounts = accounts;
    if let Some(a) = accounts
        .accounts
        .iter_mut()
        .find(|a| a.user_id.as_deref() == Some(uid) && a.dc_id.is_none())
    {
        a.dc_id = Some(dc.clone());
        a.updated_at = Some(fs_utils::now_iso());
        if fs_utils::write_json(&accounts_path, &accounts).is_ok() {
            fs_utils::app_log(
                data_dir,
                &format!("已记录账户中心id(预留): user_id={uid} dc={dc}"),
            );
            return Some(dc);
        }
    }
    None
}

/// 批量补充所有缺失 dc_id 的账号（有快照或本机登录痕迹即可补全），返回补充数量。
#[tauri::command]
pub fn accounts_backfill_dc_ids(state: State<AppState>) -> usize {
    let accounts: crate::models::AccountsFile =
        fs_utils::read_json(&state.path("checkin_accounts.json"));
    let mut n = 0usize;
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone() else { continue };
        if a.dc_id.is_some() {
            continue;
        }
        if backfill_dc_id_for(&state.data_dir, &uid).is_some() {
            n += 1;
        }
    }
    n
}

/// F-08：把本机发现的账号加入账号池（无 JWT 占位，待代理捕获后自动回填）。
#[tauri::command]
pub fn apps_account_add(
    state: State<AppState>,
    user_id: String,
    name: String,
    app: String,
    dc_id: Option<String>,
) -> Result<(), String> {
    let uid = user_id.trim().to_string();
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        return Err("无效的 UserID".into());
    }
    let mut accounts: crate::models::AccountsFile =
        fs_utils::read_json(&state.path("checkin_accounts.json"));
    let known = pool_uid_set(&accounts);
    if known.contains(&uid) {
        return Err("该账号已在账号池中".into());
    }
    let display_name = if name.trim().is_empty() {
        let tail = &uid[uid.len().saturating_sub(4)..];
        format!("{}-…{}", app_label(&app), tail)
    } else {
        name.trim().to_string()
    };
    accounts.accounts.push(crate::models::RawAccount {
        name: display_name.clone(),
        user_id: Some(uid.clone()),
        jwt: String::new(),
        refresh_token: None,
        added_at: Some(fs_utils::now_iso()),
        updated_at: Some(fs_utils::now_iso()),
        // 预留记录账户中心 id（仅当发现结果置信时传入；实测该值设备级恒定，不作账号区分）
        dc_id: dc_id.filter(|s| !s.trim().is_empty()),
    });
    fs_utils::write_json(&state.path("checkin_accounts.json"), &accounts)?;
    fs_utils::app_log(
        &state.data_dir,
        &format!("自动发现入池 [{}]: uid={} app={}", display_name, uid, app),
    );
    Ok(())
}

// ---------------- 会员/套餐信息 ----------------

#[derive(Serialize, Clone, Default)]
pub struct AppEntitlement {
    /// 应用类别：TraeWork | Trae
    pub app: String,
    /// 展示名
    pub app_label: String,
    /// 套餐名（Free / Lite / Pro ...）
    pub identity_str: Option<String>,
    /// 套餐数值（0=Free, 5=Lite ...）
    pub identity: Option<i64>,
    /// 服务端最后同步时间（毫秒）
    pub last_sync_time: Option<i64>,
}

#[derive(Serialize, Clone)]
pub struct LocalEntitlement {
    pub work: Option<AppEntitlement>,
    pub cn: Option<AppEntitlement>,
}

fn parse_entitlement(kind: &str, storage: &serde_json::Value) -> Option<AppEntitlement> {
    let raw = storage
        .get("iCubeServerData://icube.cloudide")
        .and_then(|v| v.as_str())?;
    let data: serde_json::Value = serde_json::from_str(raw).ok()?;
    let ent = data.get("entitlementInfo")?;
    Some(AppEntitlement {
        app: kind.to_string(),
        app_label: app_label(kind).to_string(),
        identity_str: ent
            .get("identityStr")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        identity: ent.get("identity").and_then(|v| v.as_i64()),
        last_sync_time: data
            .get("serverTimeInfo")
            .and_then(|s| s.get("lastSyncTime"))
            .and_then(|v| v.as_i64()),
    })
}

/// 读取本机两个 Trae 应用当前登录账号的套餐信息（零 API，来自 storage.json 明文缓存）。
#[tauri::command]
pub fn apps_entitlement_read() -> LocalEntitlement {
    let mut work = None;
    let mut cn = None;
    if let Some(storage) = read_storage_json("TraeWork") {
        work = parse_entitlement("TraeWork", &storage);
    }
    if let Some(storage) = read_storage_json("Trae") {
        cn = parse_entitlement("Trae", &storage);
    }
    LocalEntitlement { work, cn }
}

// ---------------- 账号级套餐（API） ----------------

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PayStatusEntry {
    /// 套餐名（Free / Lite / Pro ...）
    pub identity_str: String,
    /// 套餐数值
    pub identity: i64,
    /// 是否新客（未付费过）
    pub is_pay_freshman: bool,
    /// 是否积分计费
    pub is_credits_billing: bool,
    /// 查询时间（Unix 秒）
    pub fetched_at: i64,
}

/// pay_status.json 结构：{ statuses: {uid: entry}, updated_at }
#[derive(Serialize, Deserialize, Default)]
pub struct PayStatusFile {
    #[serde(default)]
    pub statuses: HashMap<String, PayStatusEntry>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

fn query_pay_status(jwt: &str) -> Result<PayStatusEntry, String> {
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    let resp = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .post("https://api.trae.cn/trae/api/v2/pay/ide_user_pay_status")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({"req_source": 2}))
        .map_err(|e| format!("API 请求失败: {}", e))?;
    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;
    // 业务异常响应（2xx 但缺关键字段）必须报错而非静默降级为 Free，
    // 否则 refresh 会用错误的 "Free" 覆盖缓存中的正确套餐
    let identity_str = body
        .get("user_pay_identity_str")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!(
                "套餐响应异常（缺少 user_pay_identity_str）：{}",
                serde_json::to_string(&body).unwrap_or_default().chars().take(120).collect::<String>()
            )
        })?;
    let identity = body
        .get("user_pay_identity")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "套餐响应异常（缺少 user_pay_identity）".to_string())?;
    Ok(PayStatusEntry {
        identity_str: identity_str.to_string(),
        identity,
        is_pay_freshman: body
            .get("is_pay_freshman")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        is_credits_billing: body
            .get("is_credits_billing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        fetched_at: chrono::Utc::now().timestamp(),
    })
}

/// 刷新所有账号的套餐身份（批量调用 ide_user_pay_status，写入 pay_status.json 缓存）。
/// 返回成功数量。
/// async 派发：逐账号串行网络请求（每个最长 60s），同步命令跑主线程会冻住 UI。
#[tauri::command(async)]
pub fn refresh_pay_status(state: State<AppState>) -> Result<usize, String> {
    let accounts: crate::models::AccountsFile =
        fs_utils::read_json(&state.path("checkin_accounts.json"));
    let mut file: PayStatusFile = fs_utils::read_json(&state.path("pay_status.json"));
    let mut ok = 0usize;
    for a in &accounts.accounts {
        // 无 JWT 的占位账号（自动发现入池）跳过
        if a.jwt.trim().is_empty() {
            continue;
        }
        let Some(uid) = a.user_id.clone() else { continue };
        match query_pay_status(&a.jwt) {
            Ok(entry) => {
                file.statuses.insert(uid, entry);
                ok += 1;
            }
            Err(e) => {
                fs_utils::app_log(
                    &state.data_dir,
                    &format!("查询套餐失败 [{}]: {}", a.name, e),
                );
            }
        }
    }
    file.updated_at = Some(fs_utils::now_iso());
    fs_utils::write_json(&state.path("pay_status.json"), &file)?;
    Ok(ok)
}
