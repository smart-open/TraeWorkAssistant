use serde::Serialize;
use tauri::State;

use crate::fs_utils;
use crate::jwt;
use crate::models::{
    AccountView, DeviceMap, DeviceEntry, GroupsFile, Group, RawAccount,
    CreditsFile, CreditsDailyFile, CreditsDailySnapshot, CheckinSummary, RemainingCreditsFile, AccountCooldownsFile,
    CreditDetail, CreditPackDetail,
};

use crate::state::AppState;

// ---------------- 双 HTTP Client 设计 ----------------

/// 短请求 Agent：总超时 120s，用于签到/积分查询/Token 刷新等 JSON 请求
fn short_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

/// 流式 Agent（死代码预留）：未设置任何超时，启用前需补齐读/写/连接超时
/// 预留给 Phase 3 OpenAI 兼容 API 使用
#[allow(dead_code)]
fn streaming_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        // 不设置 timeout_read（Duration::from_secs(0) 会触发 std 错误）
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

/// 按账号 uid 确定性派生字节流（对齐 auto_checkin.py `_seeded_stream`：
/// SHA-256("salt:uid" + 4 字节大端计数器) 级联），保证 Rust 与签到脚本
/// 对同一账号生成完全一致的设备标识。
fn seeded_stream(seed: &str, salt: &str, nbytes: usize) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let prefix = format!("{}:{}", salt, seed).into_bytes();
    let mut out = Vec::with_capacity(nbytes + 32);
    let mut counter: u32 = 0;
    while out.len() < nbytes {
        let mut h = Sha256::new();
        h.update(&prefix);
        h.update(counter.to_be_bytes());
        out.extend_from_slice(&h.finalize());
        counter = counter.wrapping_add(1);
    }
    out.truncate(nbytes);
    out
}

/// 由 uid 派生设备三元组（算法对齐 auto_checkin.py `get_device_for` gen=2）
pub fn derive_device(uid: &str) -> DeviceEntry {
    let device_id: String = seeded_stream(uid, "devid", 15)
        .iter()
        .map(|b| char::from(b'0' + b % 10))
        .collect();
    let session_id: String = seeded_stream(uid, "sess", 32)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    let mut m = seeded_stream(uid, "market", 16);
    m[6] = (m[6] & 0x0F) | 0x40; // UUID v4 版本位
    m[8] = (m[8] & 0x3F) | 0x80; // UUID v4 变体位
    let market_user_id = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5], m[6], m[7], m[8], m[9], m[10], m[11], m[12], m[13], m[14], m[15]
    );
    DeviceEntry {
        device_id,
        market_user_id: Some(market_user_id),
        session_id: Some(session_id),
    }
}

/// 解析账号设备标识：device_map.json 已有条目（代理捕获/切换流程写入）优先，
/// 否则按 uid 确定性派生（与签到脚本同算法，无需写盘）。
pub fn resolve_device(state: &AppState, uid: &str) -> DeviceEntry {
    let map: DeviceMap = fs_utils::read_json(&state.path("device_map.json"));
    map.get(uid)
        .cloned()
        .unwrap_or_else(|| derive_device(uid))
}

/// 套餐查询 Agent：总超时 60s（对齐原 pay_status 独立短超时 agent）
pub fn pay_status_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(60))
        .build()
}

/// IDE 查询类 POST 统一入口（积分/套餐等 api.trae.cn 接口）：
/// 挂完整客户端指纹头（对齐 auto_checkin.py `_build_headers`）。
/// 2026-09 实测：重新登录签发的新 JWT 会校验设备指纹，仅带 authorization 会 401；
/// 老账号 JWT 宽容放行，因此此前仅 3 个头的请求部分账号可正常返回。
pub fn ide_query_post(
    agent: &ureq::Agent,
    url: &str,
    jwt: &str,
    dev: &DeviceEntry,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    let request_id = crate::commands::oauth::random_hex(32);
    let trace_id = format!("00-{}-01", crate::commands::oauth::random_hex(16));
    let mut req = agent
        .post(url)
        .set("accept", "*/*")
        .set("accept-language", "zh-CN")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .set("user-agent", "VSCode 1.107.1 (TRAE SOLO CN)")
        .set("x-market-client-id", "VSCode 1.107.1")
        .set("x-market-user-id", dev.market_user_id.as_deref().unwrap_or(""))
        .set("x-user-region", "CN")
        .set("x-device-id", &dev.device_id)
        .set("x-lgw-req-sdk-type", "3")
        .set("package-type", "stable_cn")
        .set("x-request-id", &request_id)
        .set("x-lscbd-aid", "787976")
        .set("x-lscbd-platform", "windows")
        .set("app-version", "0.1.45")
        .set("x-tt-trace-id", &trace_id)
        .set("sec-fetch-dest", "empty")
        .set("sec-fetch-mode", "no-cors")
        .set("sec-fetch-site", "none");
    if let Some(sid) = dev.session_id.as_deref() {
        if !sid.is_empty() {
            req = req.set("vscode-sessionid", sid);
        }
    }
    let resp = req.send_json(body).map_err(|e| match e {
        // HTTP 错误：读出响应体附带服务端错误码（区分 token 过期/吊销/风控）
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            format!("API 请求失败: status code {}，响应: {}", code, snippet)
        }
        other => format!("API 请求失败: {}", other),
    })?;
    resp.into_json().map_err(|e| format!("解析响应失败: {}", e))
}

#[tauri::command]
pub fn accounts_list(state: State<AppState>) -> Vec<AccountView> {
    build_account_views(&state)
}

/// 导出所有账号原始数据，字段名对齐参考 JSON（camelCase），供前端一键导出使用。
#[tauri::command]
pub fn accounts_export_raw(state: State<AppState>) -> Result<serde_json::Value, String> {
    let accounts = crate::vault::load_accounts(&state);
    let groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    let device_map: DeviceMap = fs_utils::read_json(&state.path("device_map.json"));
    let views = build_account_views(&state);

    let merged: Vec<serde_json::Value> = views
        .iter()
        .map(|v| {
            let raw = accounts
                .accounts
                .iter()
                .find(|a| a.user_id.as_deref() == Some(&v.user_id));
            let refresh_token = raw
                .and_then(|a| a.refresh_token.clone())
                .unwrap_or_default();
            let has_rt = !refresh_token.is_empty();

            let device_id = device_map
                .get(&v.user_id)
                .map(|d| d.device_id.clone())
                .unwrap_or_default();

            let jwt_source = if has_rt { "session" } else { "manual" };

            serde_json::json!({
                "name": v.name,
                "cloudIdeJwt": v.jwt,
                "deviceId": device_id,
                "jwtExp": v.jwt_exp_timestamp,
                "balance": v.credits,
                "refreshToken": refresh_token,
                "jwtSource": jwt_source,
                "userId": v.user_id,
                "groupId": v.group_id,
                "jwtExpHours": v.jwt_exp_hours,
                "checkedToday": v.checked_today,
                "remainingCredits": v.remaining_credits,
                "deviceIdMasked": v.device_id_masked,
                "cooldownType": v.cooldown_type,
                "cooldownUntil": v.cooldown_until,
                "cooldownReason": v.cooldown_reason,
                "hasRefreshToken": v.has_refresh_token,
                "jwtAutoRefresh": v.jwt_auto_refresh,
                "creditsExpireAt": v.credits_expire_at,
            })
        })
        .collect();

    let groups_arr: Vec<serde_json::Value> = groups
        .groups
        .iter()
        .map(|g| {
            serde_json::json!({
                "id": g.id,
                "name": g.name,
                "color": g.color,
                "order": g.order,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "exportedAt": fs_utils::now_iso(),
        "appVersion": env!("CARGO_PKG_VERSION"),
        "accountCount": merged.len(),
        "accounts": merged,
        "groups": groups_arr,
    }))
}

#[tauri::command]
pub fn account_add_manual(
    state: State<AppState>,
    name: String,
    jwt: String,
    group_id: Option<String>,
) -> Result<(), String> {
    let info = jwt::parse(&jwt);
    let uid = info.user_id.ok_or("无法从 JWT 解析 UserID，请检查格式")?;
    let mut accounts = crate::vault::load_accounts(&state);
    if accounts
        .accounts
        .iter()
        .any(|a| a.user_id.as_deref() == Some(&uid))
    {
        return Err("该账号已存在".into());
    }
    accounts.accounts.push(RawAccount {
        name: name.clone(),
        user_id: Some(uid.clone()),
        jwt,
        refresh_token: None,
        added_at: Some(fs_utils::now_iso()),
        updated_at: Some(fs_utils::now_iso()),
    });
    crate::vault::save_accounts(&state, &mut accounts)?;
    if let Some(g) = group_id {
        let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
        groups.membership.insert(uid, g);
        fs_utils::write_json(&state.path("groups.json"), &groups)?;
    }
    Ok(())
}

/// 导入报告
#[derive(serde::Serialize)]
pub struct ImportReport {
    /// 文件中的账号总数
    pub total: usize,
    /// 实际新增数量
    pub added: usize,
    /// 跳过（重复）数量
    pub skipped: usize,
    /// 跳过的账号标识（uid 或名称）
    pub skipped_names: Vec<String>,
    /// 新增分组数量
    pub groups_added: usize,
}

/// 从字段取第一个非空字符串值（兼容导出格式与原始格式两套键名）
fn pick_str(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k))
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 导入账号：兼容三种格式 ——
/// 1. 本应用导出格式 `{accounts:[{userId, cloudIdeJwt, refreshToken, groupId,...}], groups:[...]}`
/// 2. 原始账号池格式 `{accounts:[{name, UserID, jwt, refresh_token?}]}`
/// 3. 裸数组 `[{...}]`
/// 按 uid（user_id 字段或 JWT 解析）去重；分组按 id 合并，不存在则新增。
#[tauri::command]
pub fn accounts_import(state: State<AppState>, content: String) -> Result<ImportReport, String> {
    let root: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("JSON 解析失败: {e}"))?;
    let empty = Vec::new();
    let accounts_arr = match &root {
        serde_json::Value::Array(arr) => arr,
        serde_json::Value::Object(obj) => obj
            .get("accounts")
            .and_then(|v| v.as_array())
            .ok_or("缺少 accounts 数组：请使用本应用导出的 JSON 文件")?,
        _ => return Err("无法识别的导入格式：需要对象或数组".into()),
    };
    let groups_arr = root
        .get("groups")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    let mut accounts = crate::vault::load_accounts(&state);
    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));

    // 已有 uid 集合（user_id 字段 + JWT 解析），与自动发现共用同一去重口径
    let mut known: std::collections::HashSet<String> = accounts
        .accounts
        .iter()
        .flat_map(|a| {
            let mut ids = Vec::new();
            if let Some(uid) = a.user_id.clone().filter(|s| !s.is_empty()) {
                ids.push(uid);
            }
            if !a.jwt.trim().is_empty() {
                if let Some(uid) = jwt::parse(&a.jwt).user_id {
                    ids.push(uid);
                }
            }
            ids
        })
        .collect();

    // 合并分组：按 id 去重，缺失即新增
    let mut groups_added = 0usize;
    let existing_group_ids: std::collections::HashSet<String> =
        groups.groups.iter().map(|g| g.id.clone()).collect();
    for g in groups_arr {
        let Some(id) = pick_str(g, &["id"]).or_else(|| pick_str(g, &["Id"])) else {
            continue;
        };
        if existing_group_ids.contains(&id) {
            continue;
        }
        groups.groups.push(Group {
            id: id.clone(),
            name: pick_str(g, &["name"]).unwrap_or_else(|| {
                let head: String = id.chars().take(4).collect();
                format!("分组 {head}")
            }),
            color: pick_str(g, &["color"]).unwrap_or_else(|| "#6366f1".into()),
            order: g.get("order").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        });
        groups_added += 1;
    }
    let group_ids: std::collections::HashSet<String> =
        groups.groups.iter().map(|g| g.id.clone()).collect();

    let mut report = ImportReport {
        total: accounts_arr.len(),
        added: 0,
        skipped: 0,
        skipped_names: Vec::new(),
        groups_added,
    };

    for entry in accounts_arr {
        // 兼容导出格式(userId/cloudIdeJwt)与原始格式(UserID/jwt)
        let user_id = pick_str(entry, &["userId", "UserID", "user_id", "uid"]);
        let jwt = pick_str(entry, &["cloudIdeJwt", "jwt"]).unwrap_or_default();
        // 无 user_id 字段时尝试从 JWT 解析
        let uid = match user_id {
            Some(u) => Some(u),
            None if !jwt.trim().is_empty() => jwt::parse(&jwt).user_id,
            _ => None,
        };
        let Some(uid) = uid else {
            report.skipped += 1;
            report
                .skipped_names
                .push(pick_str(entry, &["name"]).unwrap_or_else(|| "(无 ID)".into()));
            continue;
        };
        if known.contains(&uid) {
            report.skipped += 1;
            report
                .skipped_names
                .push(pick_str(entry, &["name"]).unwrap_or_else(|| uid.clone()));
            continue;
        }
        known.insert(uid.clone());

        let name = pick_str(entry, &["name"]).unwrap_or_else(|| {
            let skip = uid.chars().count().saturating_sub(4);
            let tail: String = uid.chars().skip(skip).collect();
            format!("导入-…{tail}")
        });
        // 分组映射：仅当目标分组存在（原有或本次导入）才记录
        let group_id = pick_str(entry, &["groupId", "group_id"]).filter(|gid| group_ids.contains(gid));
        if let Some(gid) = &group_id {
            groups.membership.insert(uid.clone(), gid.clone());
        }
        accounts.accounts.push(RawAccount {
            name,
            user_id: Some(uid),
            jwt,
            refresh_token: pick_str(entry, &["refreshToken", "refresh_token"]),
            added_at: Some(fs_utils::now_iso()),
            updated_at: Some(fs_utils::now_iso()),
        });
        report.added += 1;
    }

    if report.added > 0 || report.groups_added > 0 {
        crate::vault::save_accounts(&state, &mut accounts)?;
        fs_utils::write_json(&state.path("groups.json"), &groups)?;
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "导入账号: 新增 {} 跳过 {} 新增分组 {}",
                report.added, report.skipped, report.groups_added
            ),
        );
    }
    Ok(report)
}

#[tauri::command]
pub fn account_delete(
    state: State<AppState>,
    user_id: String,
    delete_profile: bool,
) -> Result<(), String> {
    let mut accounts = crate::vault::load_accounts(&state);
    accounts
        .accounts
        .retain(|a| a.user_id.as_deref() != Some(user_id.as_str()));
    crate::vault::save_accounts(&state, &mut accounts)?;
    // 同步清理 vault 中的凭据记录（失败仅记录日志，不阻断删除）
    crate::vault::remove_secret(&state, &user_id);

    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    groups.membership.remove(&user_id);
    fs_utils::write_json(&state.path("groups.json"), &groups)?;

    if delete_profile {
        let p = state.path("profiles").join(&user_id);
        let _ = std::fs::remove_dir_all(p);
    }
    Ok(())
}

#[tauri::command]
pub fn account_update(
    state: State<AppState>,
    user_id: String,
    name: Option<String>,
    jwt: Option<String>,
) -> Result<(), String> {
    let mut accounts = crate::vault::load_accounts(&state);
    let a = accounts
        .accounts
        .iter_mut()
        .find(|a| a.user_id.as_deref() == Some(user_id.as_str()))
        .ok_or("账号不存在")?;

    if let Some(n) = name {
        let n = n.trim().to_string();
        if !n.is_empty() {
            a.name = n;
        }
    }
    if let Some(j) = jwt {
        let j = j.trim().to_string();
        if !j.is_empty() {
            // 更新 JWT 后同步 user_id（JWT 可能换了账号）
            let info = crate::jwt::parse(&j);
            if let Some(uid) = info.user_id {
                a.user_id = Some(uid);
            }
            a.jwt = j;
        }
    }
    a.updated_at = Some(fs_utils::now_iso());
    crate::vault::save_accounts(&state, &mut accounts)?;
    Ok(())
}

// ---------------- 分组 ----------------

#[derive(Serialize)]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub color: String,
    pub order: i32,
    pub count: usize,
    /// 组内账号 uid 列表（供账号池分组筛选实时预览，T10）
    pub uids: Vec<String>,
}

#[tauri::command]
pub fn groups_list(state: State<AppState>) -> Vec<GroupView> {
    let groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    groups
        .groups
        .iter()
        .map(|g| {
            let uids: Vec<String> = groups
                .membership
                .iter()
                .filter(|(_, v)| *v == &g.id)
                .map(|(k, _)| k.clone())
                .collect();
            let count = uids.len();
            GroupView {
                id: g.id.clone(),
                name: g.name.clone(),
                color: g.color.clone(),
                order: g.order,
                count,
                uids,
            }
        })
        .collect()
}

#[tauri::command]
pub fn group_create(state: State<AppState>, name: String, color: String) -> Result<String, String> {
    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    let id = format!("g_{}", chrono::Local::now().timestamp_millis());
    let order = (groups.groups.len() as i32) + 1;
    groups.groups.push(Group {
        id: id.clone(),
        name,
        color,
        order,
    });
    fs_utils::write_json(&state.path("groups.json"), &groups)?;
    Ok(id)
}

#[tauri::command]
pub fn group_update(
    state: State<AppState>,
    id: String,
    name: Option<String>,
    color: Option<String>,
    order: Option<i32>,
) -> Result<(), String> {
    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    let g = groups
        .groups
        .iter_mut()
        .find(|g| g.id == id)
        .ok_or("分组不存在")?;
    if let Some(n) = name {
        g.name = n;
    }
    if let Some(c) = color {
        g.color = c;
    }
    if let Some(o) = order {
        g.order = o;
    }
    fs_utils::write_json(&state.path("groups.json"), &groups)?;
    Ok(())
}

#[tauri::command]
pub fn group_delete(state: State<AppState>, id: String) -> Result<(), String> {
    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    groups.groups.retain(|g| g.id != id);
    groups.membership.retain(|_, v| *v != id);
    fs_utils::write_json(&state.path("groups.json"), &groups)?;
    Ok(())
}

#[tauri::command]
pub fn group_move(
    state: State<AppState>,
    user_id: String,
    group_id: Option<String>,
) -> Result<(), String> {
    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    match group_id {
        Some(g) => {
            groups.membership.insert(user_id, g);
        }
        None => {
            groups.membership.remove(&user_id);
        }
    }
    fs_utils::write_json(&state.path("groups.json"), &groups)?;
    Ok(())
}

// ---------------- 可用积分 ----------------

/// 积分统计结果（区分通用积分 / Work 积分）
///
/// 官方积分体系（2026-09 实测）：
/// - product_id 208 = 通用积分（IDE 使用）、209 = Work 积分（SOLO Agent 使用），
///   其余带 credits_limit 的包（如 221 每月登录积分）归入通用积分。
/// - 注意：签到积分归属会变动——2026-09-04 前签到发 209（Work，200/天），
///   之后改为发 208（通用，150/天），分类必须按 product_id 动态判断，不可写死来源。
/// - 积分来源（pack 顶层 group_name / display_desc / group_type）：
///   每日签到（group_type=1）、每月登录（group_type=3）、
///   会员/购买（charge_amount>0）、兑换等。
struct CreditStats {
    /// 全部可用积分（通用 + Work）
    total: f64,
    /// 通用积分剩余
    general: f64,
    /// Work 积分剩余
    work: f64,
    /// 最近一个仍未用完且未过期的积分包过期时间（Unix 秒）
    earliest_expire: Option<i64>,
    /// 今日购买获得积分（charge_amount > 0 且 start_time 在今日）
    today_non_checkin_earned: f64,
    /// 会员套餐到期时间（Unix 秒，如「会员 Lite 连续包月」包的 end_time）
    membership_expire: Option<i64>,
    /// 会员套餐下次自动续费扣款时间（Unix 秒，next_billing_time）
    membership_next_billing: Option<i64>,
}

/// 调用 TRAE API 拉取积分包列表
fn query_ent_packs(jwt: &str, dev: &DeviceEntry) -> Result<Vec<serde_json::Value>, String> {
    let body = ide_query_post(
        &short_agent(),
        "https://api.trae.cn/trae/api/v2/pay/ide_user_ent_usage",
        jwt,
        dev,
        ureq::json!({"require_usage": true, "req_source": 2}),
    )?;

    body.get("user_entitlement_pack_list")
        .and_then(|v| v.as_array().cloned())
        .ok_or_else(|| "响应中缺少 user_entitlement_pack_list".to_string())
}

/// TRAE 切换/一键打开前预检：账号池中该 uid 的 JWT 在服务端是否仍有效。
/// 背景（issue #9，2026-09-11 诊断）：TRAE 服务端会吊销 JWT（该账号在别处重新登录被顶替、
/// IDE 内退出登录、风控等），本地快照文件完好但服务端已判死——切换恢复后 IDE 一联网即被
/// 强制登出，表现为「切换成功但账号没变/未登录」。调签到 status 轻量只读接口探活：
/// HTTP 401 判死 → 中止切换并给补救指引；无 JWT/网络故障/其他错误一律 fail-open 不阻断
/// （与豆包 probe_slot_session_alive 策略对齐）。
pub(crate) fn probe_trae_jwt_alive(state: &AppState, user_id: &str) -> Result<(), String> {
    let accounts = crate::vault::load_accounts(state);
    let Some(account) = accounts
        .accounts
        .iter()
        .find(|a| a.user_id.as_deref() == Some(user_id))
    else {
        return Ok(()); // 账号池无此账号（纯快照场景）→ 不预检，交给快照管线
    };
    if account.jwt.trim().is_empty() {
        return Ok(()); // 无 JWT 无法探活 → fail-open
    }
    // 预检专用短超时（15s）：网络故障时 fail-open 快速放行，不让用户点切换后白等
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build();
    let dev = resolve_device(state, user_id);
    // 判死文案：name(user_id) 双标识——toast 面向用户用账号名可读，日志保留 uid 便于排查
    let dead_msg = format!(
        "账号 {}({}) 的 JWT 已被服务端吊销（常见原因：该账号在别处重新登录、在 IDE 内退出登录或触发风控），\
         本地保存的登录态已失效，切换后必然未登录。请先在 TRAE 中重新登录该账号并「保存当前登录态」，\
         或开启代理重新捕获 JWT 后再切换",
        account.name, user_id
    );
    match ide_query_post(
        &agent,
        "https://api.trae.cn/trae/api/v2/ug/checkin_credits/status",
        &account.jwt,
        &dev,
        ureq::json!({}),
    ) {
        Ok(body) => {
            // 不能仅凭 HTTP 200 判活：服务端对吊销 JWT 也可能回 200 + 顶层通用鉴权失败码 1001
            // （docs/product-design.md 记录的形态；对齐 Python status_check 顶层取 code 的口径，
            // 不用 dig 深挖以免误伤嵌套业务对象里的 code）。其余业务码（如 1005 套餐限额）
            // 说明 JWT 鉴权仍通过 → 放行
            if body.get("code").and_then(|v| v.as_i64()) == Some(1001) {
                Err(dead_msg)
            } else {
                Ok(())
            }
        }
        Err(e) if e.contains("status code 401") => Err(dead_msg),
        Err(_) => Ok(()), // 网络故障/其他错误 fail-open，不阻断切换
    }
}

/// 归一化积分包来源标签（明细悬浮展示用）
///
/// 识别规则（按优先级）：
/// 1. charge_amount > 0 → 付费获得（会员连续包月赠送 / 购买）
/// 2. group_name / display_desc 关键词匹配 → 每日签到、每月登录、兑换
/// 3. fallback：原样展示 group_name → display_desc → "积分包"
fn classify_source(pack: &serde_json::Value) -> String {
    let charge = pack
        .get("entitlement_base_info")
        .and_then(|e| e.get("charge_amount"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let group_name = pack
        .get("group_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let display_desc = pack
        .get("display_desc")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let combined = format!("{}{}", group_name, display_desc);

    if charge > 0 {
        // 付费包：会员连续包月赠送、直接购买
        return "会员/购买".to_string();
    }
    if combined.contains("签到") {
        return "每日签到".to_string();
    }
    if combined.contains("登录") {
        return "每月登录".to_string();
    }
    if combined.contains("兑换") || combined.contains("redeem") {
        return "兑换".to_string();
    }
    if !group_name.is_empty() {
        return group_name.to_string();
    }
    if !display_desc.is_empty() {
        return display_desc.to_string();
    }
    "积分包".to_string()
}

/// 计算剩余积分（区分通用 / Work）
///
/// 计算逻辑：遍历 user_entitlement_pack_list，仅对 quota.credits_limit 存在的包，
/// 剩余 = credits_limit - usage.credits_amount（usage 为空则已用=0），按 product_id 分类求和。
fn calc_remaining_credits(jwt: &str, dev: &DeviceEntry) -> Result<CreditStats, String> {
    let packs = query_ent_packs(jwt, dev)?;

    let mut total: f64 = 0.0;
    let mut general: f64 = 0.0;
    let mut work: f64 = 0.0;
    let mut earliest_expire: Option<i64> = None;
    let mut today_non_checkin_earned: f64 = 0.0;
    let mut membership_expire: Option<i64> = None;
    let mut membership_next_billing: Option<i64> = None;

    // 使用固定 UTC+8 偏移，不依赖 chrono::Local（某些 Windows 环境下可能误判时区）
    let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    let now_ts = chrono::Utc::now().timestamp();

    // 今日北京时间范围 [00:00:00 +08:00, 23:59:59 +08:00]
    // start_time 来自 API 是 UTC Unix 时间戳，比较时需要按北京时间判定日期
    let today_start = chrono::Utc::now()
        .with_timezone(&cst)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(cst)
        .unwrap()
        .timestamp();
    let today_end = today_start + 86400;

    for pack in &packs {
        // ---- 会员套餐到期时间（不限积分包，扫描全部权益包）----
        // 实测（2026-09）：连续包月会员包 display_desc="会员 Lite 连续包月"、
        // group_name="会员积分"，end_time/expire_time=到期日，next_billing_time=下次扣款日。
        let group_name = pack.get("group_name").and_then(|v| v.as_str()).unwrap_or("");
        let display_desc = pack.get("display_desc").and_then(|v| v.as_str()).unwrap_or("");
        if group_name.contains("会员") || display_desc.contains("会员") {
            let end = pack
                .get("entitlement_base_info")
                .and_then(|e| e.get("end_time"))
                .and_then(|v| v.as_i64())
                .or_else(|| pack.get("expire_time").and_then(|v| v.as_i64()));
            if let Some(end) = end {
                if membership_expire.map_or(true, |cur| end > cur) {
                    membership_expire = Some(end);
                    // next_billing_time：0 / 1970 时间戳表示无自动续费
                    let nb = pack
                        .get("next_billing_time")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    membership_next_billing = if nb > 86400 { Some(nb) } else { None };
                }
            }
        }

        // 仅对有 credits_limit 的包计入统计
        let credits_limit = pack
            .get("entitlement_base_info")
            .and_then(|e| e.get("quota"))
            .and_then(|q| q.get("credits_limit"))
            .and_then(|v| v.as_f64());
        if let Some(limit) = credits_limit {
            // usage 在 pack 顶层，不在 entitlement_base_info 内
            let used = pack
                .get("usage")
                .and_then(|u| u.get("credits_amount"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let remaining = (limit - used).max(0.0);
            total += remaining;

            // product_id == 209 → Work 积分，其余归入通用积分
            let product_id = pack
                .get("entitlement_base_info")
                .and_then(|e| e.get("product_id"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            if product_id == 209 {
                work += remaining;
            } else {
                general += remaining;
            }

            // expire_time 在 pack 顶层，取最近的（仅统计仍有剩余且未过期的包）
            let expire = pack
                .get("expire_time")
                .and_then(|v| v.as_i64());
            if let Some(exp) = expire {
                if exp > now_ts && remaining > 0.0 {
                    earliest_expire = Some(earliest_expire.map_or(exp, |e| e.min(exp)));
                }
            }

            // 今日购买获得的积分：
            // start_time 在今日北京时间范围内，且 charge_amount > 0（实际付费购买）
            // 签到获得的 pack charge_amount=0，不会误判为购买积分
            let start_time = pack
                .get("entitlement_base_info")
                .and_then(|e| e.get("start_time"))
                .and_then(|v| v.as_i64());
            let charge_amount = pack
                .get("entitlement_base_info")
                .and_then(|e| e.get("charge_amount"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            // charge_amount > 0 表示付费购买（如会员连续包月），签到 pack charge_amount=0
            let is_purchased = charge_amount > 0;
            if let Some(st) = start_time {
                if st >= today_start && st < today_end && is_purchased {
                    today_non_checkin_earned += limit;
                }
            }
        }
    }

    // r2 归一到 2 位小数并消除负零：服务端 usage 与 limit 存在浮点误差（如 used 超出
    // limit 1e-7），max(0.0) 兜底后经 round 仍可能得到 -0.0，序列化为 "-0.0" 后前端
    // 显示 "-0"；IEEE 754 中 -0.0 + 0.0 = +0.0，统一归一。
    let r2 = |v: f64| (v * 100.0).round() / 100.0 + 0.0;
    Ok(CreditStats {
        total: r2(total),
        general: r2(general),
        work: r2(work),
        earliest_expire,
        today_non_checkin_earned: r2(today_non_checkin_earned),
        membership_expire,
        membership_next_billing,
    })
}

/// 获取单账号积分明细（悬浮展示用）：
/// 仅返回剩余 > 0 且未过期的积分包，按过期时间升序。
/// 网络请求命令，标记 async 交由异步线程池派发，避免阻塞主线程。
#[tauri::command(async)]
pub fn fetch_credit_detail(state: State<AppState>, user_id: String) -> Result<CreditDetail, String> {
    let accounts = crate::vault::load_accounts(&state);
    let account = accounts
        .accounts
        .iter()
        .find(|a| a.user_id.as_deref() == Some(user_id.as_str()))
        .ok_or("账号不存在")?;
    let dev = resolve_device(&state, &user_id);
    let packs = query_ent_packs(&account.jwt, &dev)?;

    let now_ts = chrono::Utc::now().timestamp();
    let mut detail_packs: Vec<CreditPackDetail> = Vec::new();
    for pack in &packs {
        let base = pack.get("entitlement_base_info");
        let limit = base
            .and_then(|e| e.get("quota"))
            .and_then(|q| q.get("credits_limit"))
            .and_then(|v| v.as_f64());
        let Some(limit) = limit else { continue };
        let used = pack
            .get("usage")
            .and_then(|u| u.get("credits_amount"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let remaining = (limit - used).max(0.0);
        // 已用完的积分包不展示
        if remaining <= 0.0 {
            continue;
        }
        // 已过期的积分包不展示
        let Some(expire) = pack.get("expire_time").and_then(|v| v.as_i64()) else {
            continue;
        };
        if expire <= now_ts {
            continue;
        }
        let product_id = base
            .and_then(|e| e.get("product_id"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let kind = if product_id == 209 { "Work" } else { "通用" }.to_string();
        let source = classify_source(pack);
        detail_packs.push(CreditPackDetail {
            kind,
            source,
            // + 0.0 消除负零（同 r2，见 refresh_credits_stats 内注释）
            remaining: (remaining * 100.0).round() / 100.0 + 0.0,
            expire_time: expire,
        });
    }
    detail_packs.sort_by_key(|p| p.expire_time);

    let general: f64 = detail_packs
        .iter()
        .filter(|p| p.kind == "通用")
        .map(|p| p.remaining)
        .sum();
    let work: f64 = detail_packs
        .iter()
        .filter(|p| p.kind == "Work")
        .map(|p| p.remaining)
        .sum();
    let r2 = |v: f64| (v * 100.0).round() / 100.0 + 0.0;
    let general = r2(general);
    let work = r2(work);
    Ok(CreditDetail {
        general,
        work,
        total: r2(general + work),
        packs: detail_packs,
    })
}

/// 获取单个账号的剩余积分（实时请求 API）
#[tauri::command]
pub fn fetch_remaining_credits(state: State<AppState>, user_id: String) -> Result<f64, String> {
    let accounts = crate::vault::load_accounts(&state);
    let account = accounts
        .accounts
        .iter()
        .find(|a| a.user_id.as_deref() == Some(user_id.as_str()))
        .ok_or("账号不存在")?;
    let jwt = &account.jwt;
    let dev = resolve_device(&state, &user_id);
    let stats = calc_remaining_credits(jwt, &dev)?;
    // 写入缓存
    let mut rc: RemainingCreditsFile = fs_utils::read_json(&state.path("remaining_credits.json"));
    rc.credits.insert(user_id.clone(), stats.total);
    rc.general.insert(user_id.clone(), stats.general);
    rc.work.insert(user_id.clone(), stats.work);
    if let Some(exp) = stats.earliest_expire {
        rc.expire_times.insert(user_id.clone(), exp);
    }
    match stats.membership_expire {
        Some(v) => {
            rc.membership_expire.insert(user_id.clone(), v);
        }
        None => {
            rc.membership_expire.remove(&user_id);
        }
    }
    match stats.membership_next_billing {
        Some(v) => {
            rc.membership_next_billing.insert(user_id.clone(), v);
        }
        None => {
            rc.membership_next_billing.remove(&user_id);
        }
    }
    rc.updated_at = Some(fs_utils::now_iso());
    fs_utils::write_json(&state.path("remaining_credits.json"), &rc)?;
    Ok(stats.total)
}

/// 刷新所有账号的剩余积分（批量请求 API），返回成功数量。
/// 同时执行自动解冻：签到成功且有积分（credits > 0）且冷却类型非 SessionDead → 清除冷却。
#[tauri::command]
pub fn refresh_remaining_credits(state: State<AppState>) -> Result<usize, String> {
    let accounts = crate::vault::load_accounts(&state);
    let mut rc: RemainingCreditsFile = fs_utils::read_json(&state.path("remaining_credits.json"));
    let mut cd: AccountCooldownsFile = fs_utils::read_json(&state.path("account_cooldowns.json"));
    let mut ok_count = 0usize;
    let mut thawed_count = 0usize;
    let mut total_non_checkin_earned: f64 = 0.0;
    for a in &accounts.accounts {
        let uid = a
            .user_id
            .clone()
            .or_else(|| jwt::parse(&a.jwt).user_id.clone())
            .unwrap_or_default();
        if uid.is_empty() {
            continue;
        }
        let dev = resolve_device(&state, &uid);
        match calc_remaining_credits(&a.jwt, &dev) {
            Ok(stats) => {
                rc.credits.insert(uid.clone(), stats.total);
                rc.general.insert(uid.clone(), stats.general);
                rc.work.insert(uid.clone(), stats.work);
                if let Some(exp) = stats.earliest_expire {
                    rc.expire_times.insert(uid.clone(), exp);
                }
                match stats.membership_expire {
                    Some(v) => {
                        rc.membership_expire.insert(uid.clone(), v);
                    }
                    None => {
                        rc.membership_expire.remove(&uid);
                    }
                }
                match stats.membership_next_billing {
                    Some(v) => {
                        rc.membership_next_billing.insert(uid.clone(), v);
                    }
                    None => {
                        rc.membership_next_billing.remove(&uid);
                    }
                }
                total_non_checkin_earned += stats.today_non_checkin_earned;
                ok_count += 1;
                // 自动解冻：有积分 + 冷却类型非 SessionDead → 清除
                if stats.total > 0.0 {
                    let thaw_type = cd.cooldowns.get(&uid).and_then(|e| {
                        if e.error_type != "SessionDead" && !e.error_type.is_empty() {
                            Some(e.error_type.clone())
                        } else {
                            None
                        }
                    });
                    if let Some(et) = thaw_type {
                        cd.cooldowns.remove(&uid);
                        thawed_count += 1;
                        crate::fs_utils::app_log(
                            &state.data_dir,
                            &format!("自动解冻 [{}]: 类型={} 积分={}", a.name, et, stats.total),
                        );
                    }
                }
            }
            Err(e) => {
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!("获取剩余积分失败 [{}]: {}", a.name, e),
                );
            }
        }
    }
    rc.updated_at = Some(fs_utils::now_iso());
    fs_utils::write_json(&state.path("remaining_credits.json"), &rc)?;

    // 记录每日积分快照（total / earned / consumed）
    record_daily_snapshot(&state, &rc, total_non_checkin_earned);

    if thawed_count > 0 {
        cd.updated_at = Some(fs_utils::now_iso());
        fs_utils::write_json(&state.path("account_cooldowns.json"), &cd)?;
    }
    Ok(ok_count)
}

/// 记录每日积分快照（每天计算一次）：
/// - total = 所有账号剩余积分之和
/// - earned = 签到获得积分（credits_history.json delta 之和）+ 购买获得积分（API 查询 charge_amount > 0）
/// - consumed = |total - earned - 昨日total|（取绝对值）
fn record_daily_snapshot(state: &State<AppState>, rc: &RemainingCreditsFile, non_checkin_earned: f64) {
    // 日期口径统一为固定 UTC+8：与 calc_remaining_credits 的"今日"判定、
    // 签到脚本写入 credits_history.json 的本地日期（目标用户均为北京时间）保持一致。
    // chrono::Local 在部分 Windows 环境下会误判时区（回退 UTC），导致快照日期
    // 与签到记录 / 非签到积分的日期归属错位，earned 出现多计或漏计。
    let today = {
        let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        chrono::Utc::now().with_timezone(&cst).format("%Y-%m-%d").to_string()
    };
    let total: f64 = rc.credits.values().sum();
    // + 0.0 消除负零（同 r2 归一，避免图表/展示出现 "-0"）
    let total = (total * 100.0).round() / 100.0 + 0.0;

    let mut file: CreditsDailyFile = fs_utils::read_json(&state.path("credits_daily.json"));

    // earned = 签到获得积分（从 credits_history.json 汇总 delta）+ 非签到获得积分（API 查询）
    let credits_file: CreditsFile = fs_utils::read_json(&state.path("credits_history.json"));
    let checkin_earned: f64 = credits_file
        .records
        .iter()
        .filter(|r| r.date == today && r.user_id != "_daily_total")
        .map(|r| r.delta as f64)
        .sum();
    let earned = ((checkin_earned + non_checkin_earned) * 100.0).round() / 100.0 + 0.0;

    // 昨日积分总数：取 today 之前最近一条快照
    let yesterday_total = file
        .snapshots
        .iter()
        .filter(|s| s.date < today)
        .last()
        .map(|s| s.total)
        .unwrap_or(0.0);

    // consumed = |total - earned - yesterday_total|
    let consumed = (total - earned - yesterday_total).abs();
    let consumed = (consumed * 100.0).round() / 100.0 + 0.0;

    // 如果今天已有快照，更新全部字段（非首次记录也需刷新 earned/consumed）
    if let Some(existing) = file.snapshots.iter_mut().find(|s| s.date == today) {
        existing.total = total;
        existing.earned = earned;
        existing.consumed = consumed;
    } else {
        file.snapshots.push(CreditsDailySnapshot {
            date: today,
            total,
            earned,
            consumed,
        });
    }

    // 保留 90 天
    let cutoff = {
        let now = chrono::Utc::now();
        let cutoff_date = now - chrono::Duration::days(90);
        cutoff_date.format("%Y-%m-%d").to_string()
    };
    file.snapshots.retain(|s| s.date >= cutoff);

    let _ = fs_utils::write_json(&state.path("credits_daily.json"), &file);
}

/// 获取每日积分快照列表
#[tauri::command]
pub fn credits_daily_list(state: State<AppState>) -> Vec<CreditsDailySnapshot> {
    let file: CreditsDailyFile = fs_utils::read_json(&state.path("credits_daily.json"));
    file.snapshots
}

/// 手动清除指定账号的冷却状态
#[tauri::command]
pub fn cooldown_clear(state: State<AppState>, user_id: String) -> Result<(), String> {
    let mut cd: AccountCooldownsFile = fs_utils::read_json(&state.path("account_cooldowns.json"));
    if cd.cooldowns.remove(&user_id).is_some() {
        cd.updated_at = Some(fs_utils::now_iso());
        fs_utils::write_json(&state.path("account_cooldowns.json"), &cd)?;
    }
    Ok(())
}

/// 一键清除所有账号的冷却状态（用于所有账号被冷却导致 503 的场景）
/// 同时清除 JSON 文件中的持久化冷却记录和运行中 API 池的内存冷却状态
#[tauri::command]
pub fn cooldown_clear_all(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
) -> Result<usize, String> {
    let mut cd: AccountCooldownsFile = fs_utils::read_json(&state.path("account_cooldowns.json"));
    let file_count = cd.cooldowns.len();
    if file_count > 0 {
        cd.cooldowns.clear();
        cd.updated_at = Some(fs_utils::now_iso());
        fs_utils::write_json(&state.path("account_cooldowns.json"), &cd)?;
    }

    // 同时清除运行中 API 池的内存冷却状态
    let mem_count = {
        let guard = runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(rt) => rt.shared.pool.clear_cooldowns(),
            None => 0,
        }
    };

    Ok(file_count.max(mem_count))
}

/// 使用 refresh_token 刷新 JWT（ExchangeToken）
/// 成功后原子写回新 accessToken + refresh_token，返回新 JWT
#[tauri::command]
pub fn refresh_jwt(state: State<AppState>, user_id: String) -> Result<String, String> {
    refresh_jwt_impl(&state, &user_id)
}

/// refresh_jwt 核心逻辑（&AppState，供命令与测试探针共用）
pub fn refresh_jwt_impl(state: &AppState, user_id: &str) -> Result<String, String> {
    // 并发安全：持锁防止多个并发请求同时 ExchangeToken
    let _lock = state
        .jwt_refresh_lock
        .lock()
        .map_err(|_| "JWT 刷新锁获取失败")?;

    // Double-check：持锁后重新读取文件，防止其他线程已刷新
    let mut accounts = crate::vault::load_accounts(state);
    let account = accounts
        .accounts
        .iter()
        .find(|a| a.user_id.as_deref() == Some(user_id))
        .ok_or("账号不存在")?;

    let refresh_token = account
        .refresh_token
        .as_ref()
        .filter(|s| !s.is_empty())
        .ok_or("该账号无 refresh_token，无法自动刷新")?;

    // 调用 ExchangeToken API
    let resp = short_agent()
        .post("https://api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken")
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({
            "ClientID": "en1oxy7wnw8j9n",
            "RefreshToken": refresh_token,
            "ClientSecret": "-",
            "UserID": ""
        }))
        .map_err(|e| format!("ExchangeToken 请求失败: {}", e))?;

    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;

    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("未知错误");
        return Err(format!("ExchangeToken 失败 (code={}): {}", code, msg));
    }

    let data = body
        .get("data")
        .ok_or("响应中缺少 data 字段")?;

    // 提取新 accessToken
    let new_access_token = data
        .get("access_token")
        .or_else(|| data.get("token"))
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少 access_token")?;

    // 提取新 refresh_token（可能轮换）
    let new_refresh_token = data
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // 验证新 accessToken 的 user_id 一致
    let new_jwt_full = if new_access_token.starts_with("Cloud-IDE-JWT ") {
        new_access_token.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", new_access_token)
    };
    let new_info = jwt::parse(&new_jwt_full);
    if let Some(ref new_uid) = new_info.user_id {
        if new_uid.as_str() != user_id {
            return Err(format!(
                "刷新后 user_id 不匹配: 期望={}, 实际={}",
                user_id, new_uid
            ));
        }
    }

    // 原子写回
    let log_name = {
        let account = accounts
            .accounts
            .iter_mut()
            .find(|a| a.user_id.as_deref() == Some(user_id))
            .ok_or("账号不存在")?;
        account.jwt = new_jwt_full.clone();
        if let Some(rt) = new_refresh_token {
            account.refresh_token = Some(rt);
        }
        account.updated_at = Some(fs_utils::now_iso());
        account.name.clone()
    };
    crate::vault::save_accounts(&state, &mut accounts)?;

    crate::fs_utils::app_log(
        &state.data_dir,
        &format!(
            "JWT 自动刷新成功 [{}]: 新 exp={}",
            log_name,
            new_info
                .exp_hours
                .map(|h| format!("{:.1}h", h))
                .unwrap_or_else(|| "?".to_string())
        ),
    );

    Ok(new_jwt_full)
}

// ---------------- 内部工具 ----------------

/// 构建账号视图（聚合 JWT / 分组 / 设备 / 积分 / 今日签到 / 冷却状态 / 套餐身份）。
pub fn build_account_views(state: &AppState) -> Vec<AccountView> {
    let accounts = crate::vault::load_accounts(state);
    let groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    let device_map: DeviceMap = fs_utils::read_json(&state.path("device_map.json"));
    let credits: CreditsFile = fs_utils::read_json(&state.path("credits_history.json"));
    let rc: RemainingCreditsFile = fs_utils::read_json(&state.path("remaining_credits.json"));
    let cd: AccountCooldownsFile = fs_utils::read_json(&state.path("account_cooldowns.json"));
    let pay: crate::commands::pay_status::PayStatusFile =
        fs_utils::read_json(&state.path("pay_status.json"));
    let summary: CheckinSummary = fs_utils::read_json(&state.path("checkin_summary.json"));
    let summary_today = summary
        .time
        .as_ref()
        .map(|t| t.starts_with(&fs_utils::today_prefix()))
        .unwrap_or(false);
    let checked_names: std::collections::HashSet<String> = if summary_today {
        summary
            .results
            .iter()
            .filter(|r| {
                let ok = r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let action = r.get("action").and_then(|v| v.as_str()).unwrap_or("");
                ok || (action != "fail" && !action.is_empty())
            })
            .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect()
    } else {
        Default::default()
    };

    let now_ts = chrono::Local::now().timestamp();
    let mut out = Vec::new();
    for a in &accounts.accounts {
        let uid = a
            .user_id
            .clone()
            .or_else(|| jwt::parse(&a.jwt).user_id.clone())
            .unwrap_or_default();
        let info = jwt::parse(&a.jwt);
        let group_id = groups.membership.get(&uid).cloned();
        // 取该账号最近日期的积分记录（credits_history.json 按日期追加，可能多条）；
        // 同日期取较大值，跨日期取较新日期，避免展示历史峰值而非当前余额。
        let credits_val = {
            let mut best: Option<(String, i64)> = None;
            for r in &credits.records {
                if r.user_id != uid {
                    continue;
                }
                match &best {
                    None => best = Some((r.date.clone(), r.credits)),
                    Some((d, c)) => {
                        if r.date > *d || (r.date == *d && r.credits > *c) {
                            best = Some((r.date.clone(), r.credits));
                        }
                    }
                }
            }
            best.map(|(_, c)| c)
        };
        let device_mask = device_map
            .get(&uid)
            .map(|d: &DeviceEntry| fs_utils::mask(&d.device_id));
        let checked = if summary_today {
            checked_names.contains(&a.name)
        } else {
            false
        };
        // 冷却状态：until > now 表示仍在冷却中（SessionDead 的 until=9999999999 始终 > now）
        let (cd_type, cd_until, cd_reason) = if let Some(entry) = cd.cooldowns.get(&uid) {
            if entry.until > now_ts && !entry.error_type.is_empty() {
                (
                    Some(entry.error_type.clone()),
                    Some(entry.until),
                    if entry.reason.is_empty() { None } else { Some(entry.reason.clone()) },
                )
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        };
        let has_rt = a
            .refresh_token
            .as_ref()
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        // 自动刷新条件：有 refresh_token 且 JWT 24h 内过期或已过期
        let need_refresh = has_rt
            && info
                .exp_hours
                .map(|h| h <= 24.0)
                .unwrap_or(true);
        out.push(AccountView {
            user_id: uid.clone(),
            name: a.name.clone(),
            group_id,
            jwt: a.jwt.clone(),
            jwt_exp_hours: info.exp_hours,
            jwt_exp_timestamp: info.exp_timestamp,
            checked_today: Some(checked),
            credits: credits_val,
            remaining_credits: rc.credits.get(&uid).copied(),
            device_id_masked: device_mask,
            cooldown_type: cd_type,
            cooldown_until: cd_until,
            cooldown_reason: cd_reason,
            has_refresh_token: has_rt,
            jwt_auto_refresh: need_refresh,
            credits_expire_at: rc.expire_times.get(&uid).copied(),
            general_credits: rc.general.get(&uid).copied(),
            work_credits: rc.work.get(&uid).copied(),
            pay_identity: pay
                .statuses
                .get(&uid)
                .map(|p| p.identity_str.clone()),
            membership_expire: rc.membership_expire.get(&uid).copied(),
            membership_next_billing: rc.membership_next_billing.get(&uid).copied(),
        });
    }
    out
}

/// 根据 scope 解析目标 user_id 列表。
pub fn resolve_user_ids(
    state: &AppState,
    scope: &str,
    selected: Option<Vec<String>>,
) -> Result<Vec<String>, String> {
    let views = build_account_views(state);
    match scope {
        "all" => Ok(views.into_iter().map(|v| v.user_id).collect()),
        s if s.starts_with("group:") => {
            let gid = &s["group:".len()..];
            Ok(views
                .into_iter()
                .filter(|v| v.group_id.as_deref() == Some(gid))
                .map(|v| v.user_id)
                .collect())
        }
        "selected" => Ok(selected.unwrap_or_default()),
        _ => Err("未知的执行范围".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// derive_device 必须与 auto_checkin.py `get_device_for`（gen=2）产出完全一致，
    /// 否则 Rust 积分请求与 Python 签到请求的设备指纹漂移，新 JWT 会被服务端 401。
    /// 期望值由构建期内置 Python 运行时调用 auto_checkin 实测得出（2026-09-10 复核）。
    #[test]
    fn test_derive_device_matches_python() {
        let dev = derive_device("2117003799429594");
        assert_eq!(dev.device_id, "924145245134852");
        assert_eq!(
            dev.market_user_id.as_deref(),
            Some("6955401e-d035-4d36-94f7-3d747b751147")
        );
        assert_eq!(
            dev.session_id.as_deref(),
            Some("93a43272d6671b20fe2391827aa2954272ccbc253ccc197d9bbae6060dee5b03")
        );
    }

    #[test]
    fn test_derive_device_stable_and_fmt() {
        let a = derive_device("12345");
        let b = derive_device("12345");
        assert_eq!(a.device_id, b.device_id);
        assert_eq!(a.device_id.len(), 15);
        assert!(a.device_id.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(a.session_id.clone().unwrap().len(), 64);
        // UUID v4 格式与版本/变体位
        let mu = a.market_user_id.unwrap();
        assert_eq!(mu.len(), 36);
        assert_eq!(&mu[14..15], "4");
        assert!(matches!(&mu[19..20], "8" | "9" | "a" | "b"));
    }

    /// 实测探针（默认忽略）：用真实数据目录 + vault 凭据验证积分接口设备指纹修复。
    /// 运行：cargo test probe_credit -- --ignored --nocapture
    /// 注意：应用正在运行时 vault 快照可能被锁，load_accounts 会降级读不到 JWT（探针报错无害）。
    #[test]
    #[ignore]
    fn probe_credit_query_real_accounts() {
        let state = AppState::new().expect("构造 AppState 失败");
        let accounts = crate::vault::load_accounts(&state);
        let target = accounts
            .accounts
            .iter()
            .find(|a| a.name == "liu_1676")
            .expect("账号列表中未找到 liu_1676");
        let uid = target.user_id.clone().expect("liu_1676 无 user_id");
        assert!(!target.jwt.trim().is_empty(), "liu_1676 JWT 为空（vault 被锁或未保存）");
        let dev = resolve_device(&state, &uid);
        println!("uid = {}", uid);
        println!("device_id = {} (device_map={})", dev.device_id, state.path("device_map.json").display());
        // 解码 JWT claims（不打印完整 token）：核对 user_id 是否串号、exp 判断时效
        let info = crate::jwt::parse(&target.jwt);
        println!("jwt.user_id = {:?}", info.user_id);
        println!("jwt.exp = {:?}", info.exp_timestamp);
        println!(
            "refresh_token = {}",
            if target.refresh_token.as_deref().map_or(true, |s| s.is_empty()) { "无" } else { "有" }
        );
        match query_ent_packs(&target.jwt, &dev) {
            Ok(list) => println!("RESULT: OK，{} 个积分包（旧 JWT 仍有效）", list.len()),
            Err(e) => {
                println!("STEP1 旧 JWT 查询: FAIL — {}", e);
                // 尝试 refresh_token 自愈：ExchangeToken 换新 JWT 后重试
                match refresh_jwt_impl(&state, &uid) {
                    Ok(new_jwt) => {
                        let ni = crate::jwt::parse(&new_jwt);
                        println!(
                            "STEP2 ExchangeToken: OK，新 exp={:?}，重试积分查询...",
                            ni.exp_timestamp
                        );
                        match query_ent_packs(&new_jwt, &dev) {
                            Ok(list) => println!(
                                "RESULT: FIXED — 刷新后查询 OK，{} 个积分包（根因=vault 旧 JWT 被吊销）",
                                list.len()
                            ),
                            Err(e2) => println!("RESULT: STILL FAIL — 刷新后仍失败: {}", e2),
                        }
                    }
                    Err(re) => println!("RESULT: FAIL — refresh_token 也已失效: {}", re),
                }
            }
        }
    }
}
