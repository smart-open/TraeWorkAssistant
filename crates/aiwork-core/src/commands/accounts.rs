use serde::Serialize;

use crate::fs_utils;
use crate::jwt;
use crate::models::{
    AccountView, AccountsFile, DeviceMap, DeviceEntry, GroupsFile, Group, RawAccount,
    CreditsFile, CreditsDailyFile, CreditsDailySnapshot, CheckinSummary, RemainingCreditsFile, AccountCooldownsFile,
    CreditDetail, CreditPackDetail,
};

use crate::state::AppState;

// ---------------- 双 HTTP Client 设计 ----------------

/// 短请求 Agent：总超时 120s，用于签到/积分查询/Token 刷新等 JSON 请求
/// （项目未启用 ureq 的 proxy-from-env feature，Agent 默认直连，不会被本地代理拦截）
fn short_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

/// 流式 Agent：无整体超时，读超时 300s（容忍长间隔 token 并防上游挂起），
/// 用于 SSE 流式对话。预留给 Phase 3 OpenAI 兼容 API 使用
#[allow(dead_code)]
fn streaming_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_read(std::time::Duration::from_secs(300))
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
/// 参数为 &AppState（测试也可直接构造）
pub fn resolve_device(state: &crate::state::AppState, uid: &str) -> DeviceEntry {
    let map: DeviceMap = crate::store::docs::device_map_load(&crate::store::db(&state.data_dir));
    map.get(uid)
        .cloned()
        .unwrap_or_else(|| derive_device(uid))
}

/// 套餐查询 Agent：总超时 60s（原 query_pay_status 用的独立短超时 agent）
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

pub fn accounts_list(state: &AppState) -> Vec<AccountView> {
    build_account_views(state)
}

/// 导出账号（Web 简版 JSON）：仅迁移必需字段（凭证 + 身份 + 分组），键名与
/// accounts_import 兼容口径一致（userId/cloudIdeJwt/refreshToken/dcId/groupId）；
/// 运行时状态（冷却/签到/余额等）不入简版。桌面端大而全导出仍可被导入兼容。
pub fn accounts_export_raw(state: &AppState) -> Result<serde_json::Value, String> {
    let accounts = crate::vault::load_accounts(state);
    let groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    let views = build_account_views(state);

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
            // 导出必须给完整 JWT：视图 jwt 字段已改为掩码（防下发），此处从原始账号取
            let jwt_full = raw.map(|a| a.jwt.clone()).unwrap_or_default();

            serde_json::json!({
                "userId": v.user_id,
                "name": v.name,
                "cloudIdeJwt": jwt_full,
                "refreshToken": refresh_token,
                "dcId": raw.and_then(|a| a.dc_id.clone()),
                "groupId": v.group_id,
            })
        })
        .collect();

    // 兜底：视图未覆盖的原始账号（如既无 user_id 又无有效 JWT 的坏行）也导出，保证数据不丢
    let view_uids: std::collections::HashSet<&str> =
        views.iter().map(|v| v.user_id.as_str()).collect();
    let extras: Vec<serde_json::Value> = accounts
        .accounts
        .iter()
        .filter(|a| {
            let uid = a.user_id.as_deref().unwrap_or("");
            !view_uids.contains(uid)
        })
        .map(|a| {
            serde_json::json!({
                "userId": a.user_id,
                "name": a.name,
                "cloudIdeJwt": a.jwt,
                "refreshToken": a.refresh_token.clone().unwrap_or_default(),
                "dcId": a.dc_id,
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

    let mut all_accounts = merged;
    all_accounts.extend(extras);

    Ok(serde_json::json!({
        "kind": "aiwork-trae-pool",
        "version": 1,
        "exportedAt": fs_utils::now_iso(),
        "appVersion": env!("CARGO_PKG_VERSION"),
        "accountCount": all_accounts.len(),
        "accounts": all_accounts,
        "groups": groups_arr,
    }))
}

/// 导入结果报告
#[derive(serde::Serialize)]
pub struct ImportReport {
    /// 文件中的账号总数
    pub total: usize,
    /// 实际新增数量
    pub added: usize,
    /// 跳过（重复）数量
    pub skipped: usize,
    /// 跳过的账号标识（uid 或名称），用于前端提示
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
/// 1. 本应用导出格式 `{accounts:[{userId, cloudIdeJwt, refreshToken, dcId, groupId,...}], groups:[...]}`
/// 2. 原始账号池格式 `{accounts:[{name, UserID, jwt, refresh_token?, dc_id?}]}`
/// 3. 裸数组 `[{...}]`
/// 按 uid（user_id 字段或 JWT 解析）去重；分组按 id 合并，不存在则新增。
/// `only`：F-46 按索引导入——仅导入指定下标的账号（下标为文件中 accounts 数组顺序）。
pub fn accounts_import(
    state: &AppState,
    content: String,
    only: Option<Vec<usize>>,
) -> Result<ImportReport, String> {
    let (accounts_arr, groups_arr) = parse_import_file(&content)?;

    let mut accounts = crate::vault::load_accounts(state);
    let mut groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));

    // 已有 uid 集合（user_id 字段 + JWT 解析），与自动发现共用同一去重口径
    let mut known: std::collections::HashSet<String> = build_known_uids(&accounts);

    // 合并分组：按 id 去重，缺失即新增
    let mut groups_added = 0usize;
    let existing_group_ids: std::collections::HashSet<String> =
        groups.groups.iter().map(|g| g.id.clone()).collect();
    for g in &groups_arr {
        let Some(id) = pick_str(g, &["id"]).or_else(|| pick_str(g, &["Id"])) else {
            continue;
        };
        if existing_group_ids.contains(&id) {
            continue;
        }
        groups.groups.push(crate::models::Group {
            id: id.clone(),
            // 按字符截断（字节切片在多字节 UTF-8 边界处会 panic）
            name: pick_str(g, &["name"]).unwrap_or_else(|| {
                format!("分组 {}", id.chars().take(4).collect::<String>())
            }),
            color: pick_str(g, &["color"]).unwrap_or_else(|| "#6366f1".into()),
            order: g.get("order").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        });
        groups_added += 1;
    }
    let group_ids: std::collections::HashSet<String> =
        groups.groups.iter().map(|g| g.id.clone()).collect();

    // F-46：按索引过滤——only 为 None 时导入全部
    let selected: Vec<(usize, &serde_json::Value)> = match &only {
        Some(indexes) => accounts_arr
            .iter()
            .enumerate()
            .filter(|(i, _)| indexes.contains(i))
            .collect(),
        None => accounts_arr.iter().enumerate().collect(),
    };

    let mut report = ImportReport {
        total: selected.len(),
        added: 0,
        skipped: 0,
        skipped_names: Vec::new(),
        groups_added,
    };

    for (_, entry) in selected {
        // 兼容导出格式(userId/cloudIdeJwt/dcId)与原始格式(UserID/jwt/dc_id)
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
            // 按字符取尾部（字节切片在多字节 UTF-8 边界处会 panic）
            let tail: String = uid.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
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
            dc_id: pick_str(entry, &["dcId", "DcID", "dc_id"]),
            // refresh_token 生命周期字段（F-78 批次 3）：导入账号从零计数
            refresh_token_expires_at: None,
            refresh_token_fails: 0,
            refresh_token_invalid: false,
            auth_saved_at: Some(fs_utils::now_iso()),
        });
        report.added += 1;
    }

    if report.added > 0 || report.groups_added > 0 {
        crate::vault::save_accounts(state, &mut accounts)?;
        crate::store::docs::groups_save(&crate::store::db(&state.data_dir), &groups)?;
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "导入账号: 新增 {} 跳过 {} 新增分组 {}",
                report.added, report.skipped, report.groups_added
            ),
        );
        // 凭据变更联动：运行中 API 池热重载（导入账号已在池白名单内时立即生效）
        if report.added > 0 {
            crate::api_server::runtime::reload_pools_after_change(state);
        }
    }
    Ok(report)
}

// ── F-46 残余：导入前 JSON 预览 + 按索引导入 ──────────────────────────────

/// 解析导入文件：返回 (账号数组, 分组数组)，兼容三种格式（所有者为返回值，便于按索引过滤）
fn parse_import_file(
    content: &str,
) -> Result<(Vec<serde_json::Value>, Vec<serde_json::Value>), String> {
    let root: serde_json::Value =
        serde_json::from_str(content).map_err(|e| format!("JSON 解析失败: {e}"))?;
    let accounts_arr: Vec<serde_json::Value> = match &root {
        serde_json::Value::Array(arr) => arr.clone(),
        serde_json::Value::Object(obj) => obj
            .get("accounts")
            .and_then(|v| v.as_array())
            .ok_or("缺少 accounts 数组：请使用本应用导出的 JSON 文件")?
            .clone(),
        _ => return Err("无法识别的导入格式：需要对象或数组".into()),
    };
    let groups_arr: Vec<serde_json::Value> = root
        .get("groups")
        .and_then(|v| v.as_array())
        .map(|a| a.clone())
        .unwrap_or_default();
    Ok((accounts_arr, groups_arr))
}

/// 账号池已有 uid 集合（user_id 字段 + JWT 解析）
fn build_known_uids(accounts: &AccountsFile) -> std::collections::HashSet<String> {
    accounts
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
        .collect()
}

/// 预览条目：index 为文件中 accounts 数组下标，供按索引导入回传
/// 注意：项目 DTO 约定为蛇形命名上线（与前端 types.ts 对齐），勿加 camelCase 改名
#[derive(serde::Serialize)]
pub struct ImportPreviewAccount {
    pub index: usize,
    pub user_id: Option<String>,
    /// 展示名：name 字段 > uid > "(无 ID)"
    pub name: String,
    pub has_jwt: bool,
    pub group_id: Option<String>,
    /// uid 已存在于账号池（默认不勾选）
    pub exists: bool,
}

/// 导入预览报告（蛇形命名上线，与前端 types.ts 对齐）
#[derive(serde::Serialize)]
pub struct ImportPreview {
    pub total: usize,
    pub accounts: Vec<ImportPreviewAccount>,
    /// 将新增的分组（不在现有分组中）
    pub new_groups: Vec<crate::models::Group>,
}

/// 导入前预览：解析文件内容，标记每个账号的 uid / 是否已存在，不写盘
pub fn accounts_import_preview(
    state: &AppState,
    content: String,
) -> Result<ImportPreview, String> {
    let (accounts_arr, groups_arr) = parse_import_file(&content)?;
    let accounts = crate::vault::load_accounts(state);
    let known = build_known_uids(&accounts);
    let groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    let existing_group_ids: std::collections::HashSet<String> =
        groups.groups.iter().map(|g| g.id.clone()).collect();

    // 待新增分组（与 accounts_import 的合并逻辑口径一致）
    let new_groups: Vec<crate::models::Group> = groups_arr
        .iter()
        .filter_map(|g| {
            let id = pick_str(g, &["id"]).or_else(|| pick_str(g, &["Id"]))?;
            if existing_group_ids.contains(&id) {
                return None;
            }
            Some(crate::models::Group {
                name: pick_str(g, &["name"]).unwrap_or_else(|| {
                    format!("分组 {}", id.chars().take(4).collect::<String>())
                }),
                id,
                color: pick_str(g, &["color"]).unwrap_or_else(|| "#6366f1".into()),
                order: g.get("order").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            })
        })
        .collect();

    let items = accounts_arr
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let user_id = pick_str(entry, &["userId", "UserID", "user_id", "uid"]);
            let jwt = pick_str(entry, &["cloudIdeJwt", "jwt"]).unwrap_or_default();
            let uid = match user_id {
                Some(u) => Some(u),
                None if !jwt.trim().is_empty() => jwt::parse(&jwt).user_id,
                _ => None,
            };
            let exists = uid.as_ref().map(|u| known.contains(u)).unwrap_or(false);
            ImportPreviewAccount {
                index,
                name: pick_str(entry, &["name"])
                    .or_else(|| uid.clone())
                    .unwrap_or_else(|| "(无 ID)".into()),
                user_id: uid,
                has_jwt: !jwt.trim().is_empty(),
                group_id: pick_str(entry, &["groupId", "group_id"]),
                exists,
            }
        })
        .collect();

    Ok(ImportPreview {
        total: accounts_arr.len(),
        accounts: items,
        new_groups,
    })
}

pub fn account_add_manual(
    state: &AppState,
    name: String,
    jwt: String,
    group_id: Option<String>,
) -> Result<(), String> {
    let info = jwt::parse(&jwt);
    let uid = info.user_id.ok_or("无法从 JWT 解析 UserID，请检查格式")?;
    let mut accounts = crate::vault::load_accounts(state);
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
        dc_id: None,
        refresh_token_expires_at: None,
        refresh_token_fails: 0,
        refresh_token_invalid: false,
        auth_saved_at: Some(fs_utils::now_iso()),
    });
    crate::vault::save_accounts(state, &mut accounts)?;
    if let Some(g) = group_id {
        let mut groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
        groups.membership.insert(uid, g);
        crate::store::docs::groups_save(&crate::store::db(&state.data_dir), &groups)?;
    }
    Ok(())
}

pub fn account_delete(
    state: &AppState,
    user_id: String,
    delete_profile: bool,
) -> Result<(), String> {
    let mut accounts = crate::vault::load_accounts(state);
    accounts
        .accounts
        .retain(|a| a.user_id.as_deref() != Some(user_id.as_str()));
    crate::vault::save_accounts(state, &mut accounts)?;
    // 同步清理 vault 中的凭据记录（失败仅记录日志，不阻断删除）
    crate::vault::remove_secret(state, &user_id);

    let mut groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    groups.membership.remove(&user_id);
    crate::store::docs::groups_save(&crate::store::db(&state.data_dir), &groups)?;

    if delete_profile {
        // P0 防目录逃逸：user_id 直接拼进 profiles/ 路径并整目录删除，先做字符集白名单校验
        fs_utils::ensure_uid_safe(&user_id)?;
        let p = state.path("profiles").join(&user_id);
        let _ = std::fs::remove_dir_all(p);
    }
    Ok(())
}

/// 按 UserID 查询单账号完整 JWT（编辑弹框按需回填；列表接口只返回掩码，凭据不下发全量列表）。
/// 顶层参数命名遵循仓库约定：Rust 签名 user_id，前端 invoke 传 userId 自动映射。
pub fn account_get_jwt(state: &AppState, user_id: String) -> Result<String, String> {
    fs_utils::ensure_uid_safe(&user_id)?;
    let accounts = crate::vault::load_accounts(state);
    let account = accounts
        .accounts
        .iter()
        .find(|a| a.user_id.as_deref() == Some(user_id.as_str()))
        .ok_or_else(|| format!("账号 {user_id} 不在账号池中"))?;
    let jwt = account.jwt.trim();
    if jwt.is_empty() {
        return Err(format!("账号 {user_id} 无有效 JWT（可能未被 vault 解密回填）"));
    }
    Ok(jwt.to_string())
}

pub fn account_update(
    state: &AppState,
    user_id: String,
    name: Option<String>,
    jwt: Option<String>,
) -> Result<(), String> {
    let mut accounts = crate::vault::load_accounts(state);
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
    let mut jwt_updated = false;
    if let Some(j) = jwt {
        let j = j.trim().to_string();
        if !j.is_empty() {
            // 更新 JWT 后同步 user_id（JWT 可能换了账号）
            let info = crate::jwt::parse(&j);
            if let Some(uid) = info.user_id {
                a.user_id = Some(uid);
            }
            a.jwt = j;
            jwt_updated = true;
        }
    }
    a.updated_at = Some(fs_utils::now_iso());
    crate::vault::save_accounts(state, &mut accounts)?;
    // 凭据变更联动：运行中 API 池热重载（手动粘贴 JWT 后立即参与调度，无需重启）
    if jwt_updated {
        crate::api_server::runtime::reload_pools_after_change(state);
    }
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

pub fn groups_list(state: &AppState) -> Vec<GroupView> {
    let groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
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

pub fn group_create(state: &AppState, name: String, color: String) -> Result<String, String> {
    let mut groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    let id = format!("g_{}", chrono::Local::now().timestamp_millis());
    let order = (groups.groups.len() as i32) + 1;
    groups.groups.push(Group {
        id: id.clone(),
        name,
        color,
        order,
    });
    crate::store::docs::groups_save(&crate::store::db(&state.data_dir), &groups)?;
    Ok(id)
}

pub fn group_update(
    state: &AppState,
    id: String,
    name: Option<String>,
    color: Option<String>,
    order: Option<i32>,
) -> Result<(), String> {
    let mut groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
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
    crate::store::docs::groups_save(&crate::store::db(&state.data_dir), &groups)?;
    Ok(())
}

pub fn group_delete(state: &AppState, id: String) -> Result<(), String> {
    let mut groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    groups.groups.retain(|g| g.id != id);
    groups.membership.retain(|_, v| *v != id);
    crate::store::docs::groups_save(&crate::store::db(&state.data_dir), &groups)?;
    Ok(())
}

pub fn group_move(
    state: &AppState,
    user_id: String,
    group_id: Option<String>,
) -> Result<(), String> {
    let mut groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    match group_id {
        Some(g) => {
            groups.membership.insert(user_id, g);
        }
        None => {
            groups.membership.remove(&user_id);
        }
    }
    crate::store::docs::groups_save(&crate::store::db(&state.data_dir), &groups)?;
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
    /// 本周期积分包总额度（有效积分包 credits_limit 合计；到期日历「剩余 X / 总 Y」）
    total_limit: f64,
    /// 最近一个仍未用完且未过期的积分包过期时间（Unix 秒，含 Work 包；UI 展示/签到排序口径）
    earliest_expire: Option<i64>,
    /// 通用积分（product_id != 209）最早到期时间（Unix 秒，不含 Work 包）；
    /// API 网关调度口径（网关只扣通用积分），issue #28
    general_earliest_expire: Option<i64>,
    /// 各日期新开积分包额度聚合（键=北京时间日期）：
    /// entitlement_base_info.start_time 即积分包 CycleStartTime（如
    /// "2026-09-14 15:52:38"），某日获得积分 = 该日新开全部积分包 credits_limit
    /// 合计（签到包与购买包均计）；覆盖范围受 API 返回的包历史限制
    pack_earned_daily: std::collections::BTreeMap<String, f64>,
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

    // F-49 宽容解析：字段可能被 data 等包裹键包裹，dig 自动下钻
    crate::fs_utils::dig(&body, &["user_entitlement_pack_list"])
        .and_then(|v| v.as_array().cloned())
        .ok_or_else(|| "响应中缺少 user_entitlement_pack_list".to_string())
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
    let now_ts = chrono::Utc::now().timestamp();
    Ok(parse_credit_stats(&packs, now_ts))
}

/// 解析积分包列表 → 统计（纯函数，便于单测；分类口径见 CreditStats 注释）
fn parse_credit_stats(packs: &[serde_json::Value], now_ts: i64) -> CreditStats {
    let mut total: f64 = 0.0;
    let mut general: f64 = 0.0;
    let mut work: f64 = 0.0;
    let mut total_limit: f64 = 0.0;
    let mut earliest_expire: Option<i64> = None;
    let mut general_earliest_expire: Option<i64> = None;
    let mut pack_earned_daily: std::collections::BTreeMap<String, f64> = Default::default();
    let mut membership_expire: Option<i64> = None;
    let mut membership_next_billing: Option<i64> = None;

    // 使用固定 UTC+8 偏移，不依赖 chrono::Local（某些 Windows 环境下可能误判时区）
    let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();

    for pack in packs {
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
            // 本周期总额度：与剩余同口径（有 credits_limit 的包）求和
            total_limit += limit;

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
                    // 调度口径（issue #28）：网关只扣通用积分，Work 包到期不参与
                    if product_id != 209 {
                        general_earliest_expire =
                            Some(general_earliest_expire.map_or(exp, |e| e.min(exp)));
                    }
                }
            }

            // 获得积分归日（积分包 CycleStartTime 口径）：
            // entitlement_base_info.start_time 即该包 CycleStartTime，某日获得积分 =
            // 该日新开全部积分包的 credits_limit 合计。签到包（charge_amount=0）与
            // 购买包（>0）均计入——不能按「签到 delta 合计」算（漏购买），也不能按
            // 「total - 昨日total + consumed」恒等式反推（包过期/消耗波动虚增，
            // 实测昨日 earned 虚增至 1300）。
            let start_time = pack
                .get("entitlement_base_info")
                .and_then(|e| e.get("start_time"))
                .and_then(|v| v.as_i64());
            if let Some(st) = start_time {
                // start_time 为 UTC 秒，按固定 UTC+8 归日
                let date = chrono::TimeZone::timestamp_opt(&chrono::Utc, st, 0)
                    .single()
                    .map(|dt| dt.with_timezone(&cst).date_naive().to_string());
                if let Some(date) = date {
                    *pack_earned_daily.entry(date).or_insert(0.0) += limit;
                }
            }
        }
    }

    // 保留 2 位小数；结果为 0 时归一为 +0.0，避免序列化成 -0.0 导致前端显示 "-0"
    let r2 = |v: f64| {
        let r = (v * 100.0).round() / 100.0;
        if r == 0.0 { 0.0 } else { r }
    };
    CreditStats {
        total: r2(total),
        general: r2(general),
        work: r2(work),
        total_limit: r2(total_limit),
        earliest_expire,
        general_earliest_expire,
        pack_earned_daily,
        membership_expire,
        membership_next_billing,
    }
}

/// 获取单账号积分明细（悬浮展示用）：
/// 仅返回剩余 > 0 且未过期的积分包，按过期时间升序。
/// 内部走网络请求（最长 120s），调用方按 async 派发。
pub fn fetch_credit_detail(state: &AppState, user_id: String) -> Result<CreditDetail, String> {
    let accounts = crate::vault::load_accounts(state);
    let account = accounts
        .accounts
        .iter()
        .find(|a| a.user_id.as_deref() == Some(user_id.as_str()))
        .ok_or("账号不存在")?;
    let dev = resolve_device(state, &user_id);
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
        // 已过期的积分包不展示；无 expire_time 视为长期有效（与 calc_remaining_credits 统计口径一致），
        // 以 2100-01-01 哨兵时间戳参与排序，前端识别该值显示「长期有效」
        if let Some(expire) = pack.get("expire_time").and_then(|v| v.as_i64()) {
            if expire <= now_ts {
                continue;
            }
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
            remaining: (remaining * 100.0).round() / 100.0,
            // 无 expire_time → 长期有效哨兵（2100-01-01），排序靠后且前端特殊展示
            expire_time: pack
                .get("expire_time")
                .and_then(|v| v.as_i64())
                .unwrap_or(4102444800),
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
    // 保留 2 位小数；结果为 0 时归一为 +0.0，避免序列化成 -0.0 导致前端显示 "-0"
    let r2 = |v: f64| {
        let r = (v * 100.0).round() / 100.0;
        if r == 0.0 { 0.0 } else { r }
    };
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
/// 内部走网络请求（最长 120s），调用方按 async 派发。
pub fn fetch_remaining_credits(state: &AppState, user_id: String) -> Result<f64, String> {
    let accounts = crate::vault::load_accounts(state);
    let account = accounts
        .accounts
        .iter()
        .find(|a| a.user_id.as_deref() == Some(user_id.as_str()))
        .ok_or("账号不存在")?;
    let jwt = &account.jwt;
    let dev = resolve_device(state, &user_id);
    let stats = calc_remaining_credits(jwt, &dev)?;
    // 写入缓存
    let mut rc: RemainingCreditsFile = crate::store::docs::remaining_credits_load(&crate::store::db(&state.data_dir));
    rc.credits.insert(user_id.clone(), stats.total);
    rc.general.insert(user_id.clone(), stats.general);
    rc.work.insert(user_id.clone(), stats.work);
    rc.total_limit.insert(user_id.clone(), stats.total_limit);
    if let Some(exp) = stats.earliest_expire {
        rc.expire_times.insert(user_id.clone(), exp);
    }
    // 通用积分到期（API 网关调度口径，issue #28）：None → 清除，
    // 避免 Work 包污染回退或包耗尽/长期有效后残留 stale 老值误伤调度
    match stats.general_earliest_expire {
        Some(exp) => {
            rc.general_expire_times.insert(user_id.clone(), exp);
        }
        None => {
            rc.general_expire_times.remove(&user_id);
        }
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
    crate::store::docs::remaining_credits_save(&crate::store::db(&state.data_dir), &rc)?;
    Ok(stats.total)
}

/// 刷新所有账号的剩余积分（批量请求 API），返回成功数量。
/// 刷新所有账号剩余积分（管理页/积分页刷新按钮入口）。
/// 同时执行自动解冻：签到成功且有积分（credits > 0）且冷却类型非 SessionDead → 清除冷却。
pub fn refresh_remaining_credits(state: &AppState) -> Result<usize, String> {
    refresh_remaining_credits_impl(state)
}

/// 刷新实现（供命令层与 `--task-run refresh-credits` CLI 任务共用）：
/// 逐账号查询积分包 → 回写 remaining_credits.json → 按 CycleStartTime 归日口径
/// 重算 credits_daily.json 快照（今日 earned + API 可见历史修正）。
pub fn refresh_remaining_credits_impl(state: &AppState) -> Result<usize, String> {
    let accounts = crate::vault::load_accounts(state);
    let mut rc: RemainingCreditsFile = crate::store::docs::remaining_credits_load(&crate::store::db(&state.data_dir));
    let mut cd: AccountCooldownsFile = crate::store::docs::account_cooldowns_load(&crate::store::db(&state.data_dir));
    let mut ok_count = 0usize;
    let mut thawed_count = 0usize;
    let mut pack_earned_daily: std::collections::BTreeMap<String, f64> = Default::default();
    for a in &accounts.accounts {
        let uid = a
            .user_id
            .clone()
            .or_else(|| jwt::parse(&a.jwt).user_id.clone())
            .unwrap_or_default();
        if uid.is_empty() {
            continue;
        }
        let dev = resolve_device(state, &uid);
        match calc_remaining_credits(&a.jwt, &dev) {
            Ok(stats) => {
                rc.credits.insert(uid.clone(), stats.total);
                rc.general.insert(uid.clone(), stats.general);
                rc.work.insert(uid.clone(), stats.work);
                rc.total_limit.insert(uid.clone(), stats.total_limit);
                if let Some(exp) = stats.earliest_expire {
                    rc.expire_times.insert(uid.clone(), exp);
                }
                // 通用积分到期（调度口径）：None → 清除，与单账号刷新同语义
                match stats.general_earliest_expire {
                    Some(exp) => {
                        rc.general_expire_times.insert(uid.clone(), exp);
                    }
                    None => {
                        rc.general_expire_times.remove(&uid);
                    }
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
                // 各账号包起始日聚合合并（跨账号同日累加）
                for (d, e) in &stats.pack_earned_daily {
                    *pack_earned_daily.entry(d.clone()).or_insert(0.0) += e;
                }
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
    crate::store::docs::remaining_credits_save(&crate::store::db(&state.data_dir), &rc)?;

    // 记录每日积分快照（total / earned / consumed）
    record_daily_snapshot(state, &rc, &pack_earned_daily);

    if thawed_count > 0 {
        cd.updated_at = Some(fs_utils::now_iso());
        crate::store::docs::account_cooldowns_save(&crate::store::db(&state.data_dir), &cd)?;
    }
    Ok(ok_count)
}

/// 记录每日积分快照（每次刷新剩余积分时计算）：
/// - total = 所有账号剩余积分之和
/// - earned = 积分包 CycleStartTime 归日口径：某日获得积分 = 该日新开积分包
///   （entitlement_base_info.start_time 落在该日）的 credits_limit 合计，签到包与
///   购买包均计。不能按签到 delta 合计（获得也可能来自购买），也不能按
///   「total - 昨日total + consumed」恒等式反推——包过期/消耗波动都会被塞进
///   earned 造成虚增（实测昨日 earned 虚增至 1300）。API 返回历史包时，
///   可见范围内的历史快照 earned 一并修正。
/// - consumed 优先取 Trae Work 用量接口今日合计（usage_history.json 的 credits_float，
///   实际消耗口径，见 commands/usage_history.rs）；无接口数据时由余额式推算
fn record_daily_snapshot(
    state: &AppState,
    rc: &RemainingCreditsFile,
    pack_earned_daily: &std::collections::BTreeMap<String, f64>,
) {
    let today = fs_utils::today_prefix(); // "YYYY-MM-DD"
    let total: f64 = rc.credits.values().sum();
    let total = (total * 100.0).round() / 100.0;

    let mut file: CreditsDailyFile = crate::store::docs::credits_daily_load(&crate::store::db(&state.data_dir));

    // 昨日积分总数：取 today 之前最近一条快照
    let yesterday_total = file
        .snapshots
        .iter()
        .filter(|s| s.date < today)
        .last()
        .map(|s| s.total)
        .unwrap_or(0.0);

    // 优先口径：consumed = 用量接口今日合计；earned = total - 昨日total + consumed
    let usage_cache: serde_json::Value =
        crate::store::docs::usage_history_load(&crate::store::db(&state.data_dir));
    let mut usage_consumed: Option<f64> = None;
    if let Some(accs) = usage_cache.get("accounts").and_then(|v| v.as_object()) {
        let mut sum = 0.0;
        let mut has = false;
        for (_, acc) in accs {
            if let Some(c) = acc
                .get("daily")
                .and_then(|d| d.get(&today))
                .and_then(|d| d.get("credits"))
                .and_then(|v| v.as_f64())
            {
                sum += c;
                has = true;
            }
        }
        if has {
            usage_consumed = Some((sum * 100.0).round() / 100.0);
        }
    }

    let r2 = |v: f64| {
        let r = (v * 100.0).round() / 100.0;
        if r == 0.0 { 0.0 } else { r }
    };

    // earned：今日新开积分包额度合计（CycleStartTime 归今日；今日无新包则为 0）
    let earned = pack_earned_daily.get(&today).copied().unwrap_or(0.0);
    let earned = r2(earned);

    let consumed = match usage_consumed {
        Some(consumed) => consumed,
        None => {
            // 回退口径（用量接口无今日数据时）：余额式推算 |昨日total + earned - total|
            let consumed = (yesterday_total + earned - total).abs();
            r2(consumed)
        }
    };

    // 如果今天已有快照，更新全部字段（非首次记录也需刷新 earned/consumed）
    if let Some(existing) = file.snapshots.iter_mut().find(|s| s.date == today) {
        existing.total = total;
        existing.earned = earned;
        existing.consumed = consumed;
    } else {
        file.snapshots.push(CreditsDailySnapshot {
            date: today.clone(),
            total,
            earned,
            consumed,
        });
    }

    // 历史修正：API 可见范围内的历史日期（如昨日已入账的签到/购买包），
    // 用 CycleStartTime 归日口径覆盖旧 earned（旧值多为恒等式反推的失真数据，
    // 实测昨日 1300）；无快照的日期不补建（total/consumed 无数据源）
    for (date, e) in pack_earned_daily {
        if date == &today {
            continue;
        }
        if let Some(snap) = file.snapshots.iter_mut().find(|s| &s.date == date) {
            snap.earned = r2(*e);
        }
    }

    // 保留 90 天
    let cutoff = {
        let now = chrono::Utc::now();
        let cutoff_date = now - chrono::Duration::days(90);
        cutoff_date.format("%Y-%m-%d").to_string()
    };
    file.snapshots.retain(|s| s.date >= cutoff);

    let _ = crate::store::docs::credits_daily_save(&crate::store::db(&state.data_dir), &file);
}

/// 获取每日积分快照列表
pub fn credits_daily_list(state: &AppState) -> Vec<CreditsDailySnapshot> {
    let file: CreditsDailyFile = crate::store::docs::credits_daily_load(&crate::store::db(&state.data_dir));
    file.snapshots
}

/// 手动清除指定账号的冷却状态
pub fn cooldown_clear(state: &AppState, user_id: String) -> Result<(), String> {
    let mut cd: AccountCooldownsFile = crate::store::docs::account_cooldowns_load(&crate::store::db(&state.data_dir));
    if cd.cooldowns.remove(&user_id).is_some() {
        cd.updated_at = Some(fs_utils::now_iso());
        crate::store::docs::account_cooldowns_save(&crate::store::db(&state.data_dir), &cd)?;
    }
    Ok(())
}

/// 一键清除所有账号的冷却状态（用于所有账号被冷却导致 503 的场景）
/// 同时清除 JSON 文件中的持久化冷却记录和运行中 API 池的内存冷却状态
pub fn cooldown_clear_all(state: &AppState) -> Result<usize, String> {
    let mut cd: AccountCooldownsFile = crate::store::docs::account_cooldowns_load(&crate::store::db(&state.data_dir));
    let file_count = cd.cooldowns.len();
    if file_count > 0 {
        cd.cooldowns.clear();
        cd.updated_at = Some(fs_utils::now_iso());
        crate::store::docs::account_cooldowns_save(&crate::store::db(&state.data_dir), &cd)?;
    }

    // 同时清除运行中 API 池的内存冷却状态（网关未注册/未运行时计 0）
    let mem_count = match crate::api_server::runtime::gateway_shared() {
        Some(shared) => shared.pool.clear_cooldowns(),
        None => 0,
    };

    Ok(file_count.max(mem_count))
}

/// 使用 refresh_token 刷新 JWT（ExchangeToken）
/// 成功后原子写回新 accessToken + refresh_token，返回新 JWT。
/// force=false（默认）时惰性：JWT 剩余有效期充足则跳过（防无谓 refresh_token 轮换——
/// 轮换会使 TRAE IDE 侧旧凭证失效，是双端互踢冲突的直接诱因）；
/// force=true 强制刷新（手动按钮/测试探针使用）。
// 内含 ExchangeToken 网络请求（最长 120s），调用方按 async 派发。
pub fn refresh_jwt(
    state: &AppState,
    user_id: String,
    force: Option<bool>,
) -> Result<String, String> {
    let result = refresh_jwt_impl(state, &user_id, force.unwrap_or(false));
    // 运行中 API 池联动：成功 → 全量热重载（新 JWT 落池 + 解除失效/SessionDead 禁用快照，
    // 单点 note_refresh_success 覆盖不了禁用/冷却/积分陈旧快照）；
    // 失败且已判定 refresh_token 失效 → 池内同步禁用（entry 按 uid 命中，双池双查无害）
    match &result {
        Ok(_) => crate::api_server::runtime::reload_pools_after_change(state),
        Err(_) => {
            let accounts = crate::vault::load_accounts(state);
            if accounts
                .accounts
                .iter()
                .any(|a| a.user_id.as_deref() == Some(user_id.as_str()) && a.refresh_token_invalid)
            {
                if let Some(shared) = crate::api_server::runtime::gateway_shared() {
                    shared.pool.note_refresh_invalid(&user_id);
                    shared.wb_pool.note_refresh_invalid(&user_id);
                }
            }
        }
    }
    result
}

/// refresh 失败短冷却（D，60s）：同账号刷新失败后 60s 内直接拒绝重试，
/// 消除重试风暴与 vault 写放大（app.log 实证 19s 内 4 连败 + 4 次全量加密写盘）。
/// 进程内态即可：重启清零无害，冷却目的仅是限频。
static REFRESH_COOLDOWN: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
const REFRESH_COOLDOWN_SECS: u64 = 60;
/// 401 自愈并发去重窗：force 刷新成功后记录时刻，窗内同账号 force 直接复用 vault 现有 JWT
const FORCE_REFRESH_DEDUP_SECS: u64 = 5;
static LAST_SUCCESS_REFRESH: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 惰性刷新门阈值（优化 2）：JWT 剩余有效期超过 48h（且 refresh_token 未临近过期）
/// 时不发 ExchangeToken。JWT 全量寿命约 13 天——把刷新压到最后 48h，每次刷新都是
/// refresh_token 轮换（IDE 侧旧凭证随之失效），减少轮换次数 = 缩小双端互踢冲突面。
const TRAE_LAZY_REFRESH_MIN_SECS: i64 = 48 * 3600;

/// 惰性刷新判定（优化 2，纯函数化便于单测）：false = 可跳过刷新。
/// 跳过 = JWT 剩余 > 48h 且 refresh_token 未临近过期（剩余 ≥48h）。
/// JWT exp 缺失（空串/解析失败）→ 需要刷新（保守放行）；
/// refresh_token 缺失 → 不因 rt 提前触发刷新。
fn lazy_refresh_needed(jwt: &str, refresh_token_expires_at: Option<i64>, now_ts: i64) -> bool {
    let jwt_fresh = jwt::parse(jwt)
        .exp_timestamp
        .map(|exp| exp - now_ts > TRAE_LAZY_REFRESH_MIN_SECS)
        .unwrap_or(false);
    let rt_expiring = refresh_token_expires_at
        .map(|exp| exp - now_ts < TRAE_LAZY_REFRESH_MIN_SECS)
        .unwrap_or(false);
    !jwt_fresh || rt_expiring
}

/// refresh_jwt 核心逻辑（&AppState，供命令与测试探针共用）
pub fn refresh_jwt_impl(state: &AppState, user_id: &str, force: bool) -> Result<String, String> {
    // 并发安全：持锁防止多个并发请求同时 ExchangeToken
    let _lock = state
        .jwt_refresh_lock
        .lock()
        .map_err(|_| "JWT 刷新锁获取失败")?;

    // D 短冷却：失败后 60s 内拒绝重试（持锁检查，防并发穿透）
    {
        let cd = REFRESH_COOLDOWN.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(until) = cd.get(user_id) {
            if until.elapsed() < std::time::Duration::from_secs(REFRESH_COOLDOWN_SECS) {
                return Err(format!(
                    "该账号刷新处于冷却期（{}s 内失败过，请稍后重试）",
                    REFRESH_COOLDOWN_SECS
                ));
            }
        }
    }

    // Double-check：持锁后重新读取文件，防止其他线程已刷新
    let mut accounts = crate::vault::load_accounts(state);

    // 401 自愈并发去重（force 路径）：去重窗内该账号刚成功刷新过 → vault 中即最新 JWT，
    // 直接复用不再重复轮换——并发 401 会串行进锁，每次真实轮换都作废上一轮凭证
    if force {
        let recent = LAST_SUCCESS_REFRESH
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(user_id)
            .map(|t: &std::time::Instant| t.elapsed());
        if matches!(recent, Some(e) if e < std::time::Duration::from_secs(FORCE_REFRESH_DEDUP_SECS)) {
            if let Some(acct) = accounts
                .accounts
                .iter()
                .find(|a| a.user_id.as_deref() == Some(user_id))
            {
                return Ok(acct.jwt.clone());
            }
        }
    }

    // C 入口拦截：已判定 refresh_token 失效的账号不再发网络请求
    // （此前无效标记仅影响调度，手动/自动刷新仍会持续探测——app.log 实证
    // 「已标记失效」后 19s 内仍 4 连败 + 每次触发 vault 全量加密写盘）
    // Web 化裁剪：桌面版此处会先尝试从 TRAE IDE 本机登录态恢复（try_recover_from_local，
    // 解密本机 Chromium Cookies/leveldb），Web 侧无本机 IDE 登录态可解密，该恢复分支删除。
    // invalid 账号直接走「需重新 OAuth 登录」既有标记逻辑；冷却与 invalid 标记写入
    // 仍由 record_refresh_failure 落盘，解除途径为重新 OAuth 登录。
    let account = {
        let acct = accounts
            .accounts
            .iter()
            .find(|a| a.user_id.as_deref() == Some(user_id))
            .ok_or("账号不存在")?;
        if acct.refresh_token_invalid {
            return Err("refresh_token 已失效，需重新 OAuth 登录".to_string());
        }
        acct
    };

    // 惰性刷新门（优化 2，force=false 时生效）：判定抽为纯函数 lazy_refresh_needed
    //（含边界语义：exp 缺失放行、rt 临期续命）。ExchangeToken 会轮换 refresh_token，
    // 工具侧续期即会使 TRAE IDE 持有的旧凭证失效——减少无谓轮换 = 缩小双端互踢冲突面。
    // JWT 已被服务端提前吊销（exp 仍远）时此门放不住，需由调用方传 force=true；
    // 手动刷新按钮保持强制语义。
    if !force
        && !lazy_refresh_needed(
            &account.jwt,
            account.refresh_token_expires_at,
            chrono::Utc::now().timestamp(),
        )
    {
        let remain_h = jwt::parse(&account.jwt)
            .exp_timestamp
            .map(|exp| (exp - chrono::Utc::now().timestamp()) as f64 / 3600.0)
            .unwrap_or(0.0);
        return Err(format!(
            "JWT 剩余有效期 {:.1} 小时，暂无需刷新（如需立即刷新请使用手动刷新按钮）",
            remain_h
        ));
    }

    let refresh_token = account
        .refresh_token
        .as_ref()
        .filter(|s| !s.is_empty())
        .ok_or("该账号无 refresh_token，无法自动刷新")?
        .clone();

    // 调用 ExchangeToken（2026-09-16 协议迁移：固化协议 DeviceProof 主变体 +
    // 旧协议兜底探测；仅旧形态数字 code != 0 判为服务端明确拒绝——
    // 旧实现 unwrap_or(-1) 会把无 code 字段的异构响应误判为拒绝，第 1 次即误标失效）
    let exchange = crate::commands::oauth::exchange_token_refresh(state, &refresh_token);
    let (new_access_token, new_refresh_token, body) = match exchange {
        Ok(t) => t,
        Err(e) => {
            let err = format!("ExchangeToken 失败: {}", e.msg);
            // B 判定收窄：仅服务端明确拒绝（数字 code != 0）才立即置 invalid；
            // 网络/解析/协议级失败走 rejected=false（连续 3 次仍会置 invalid 兜底）
            record_refresh_failure(state, user_id, e.server_rejected, &err);
            // D 失败进入冷却
            REFRESH_COOLDOWN
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(user_id.to_string(), std::time::Instant::now());
            return Err(err);
        }
    };

    // 提取新 refresh_token（可能轮换）——已在 exchange_token_refresh 内宽容提取

    // 验证新 accessToken 的 user_id 一致
    let new_jwt_full = if new_access_token.starts_with("Cloud-IDE-JWT ") {
        new_access_token.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", new_access_token)
    };
    let new_info = jwt::parse(&new_jwt_full);
    if let Some(ref new_uid) = new_info.user_id {
        if new_uid.as_str() != user_id {
            // 换发 token 归属他人：refresh_token 已不可信，立即置 invalid（F-78 批次 3）
            let err = format!(
                "刷新后 user_id 不匹配: 期望={}, 实际={}",
                user_id, new_uid
            );
            record_refresh_failure(state, user_id, true, &err);
            // D：异常 token 同样进入冷却
            REFRESH_COOLDOWN
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(user_id.to_string(), std::time::Instant::now());
            return Err(err);
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
        // 刷新成功：生命周期计数清零、失效标记解除（F-78 批次 3）
        account.refresh_token_fails = 0;
        account.refresh_token_invalid = false;
        account.auth_saved_at = Some(fs_utils::now_iso());
        // 若响应携带 refresh_token 过期时间则更新（兼容秒/毫秒两种时间戳）
        if let Some(exp) = crate::fs_utils::dig(
            &body,
            &[
                "refresh_token_expires_at",
                "refresh_expires_at",
                "refreshTokenExpiresAt",
                "refresh_expires_at_ms",
            ],
        )
        .and_then(|v| v.as_i64())
        {
            account.refresh_token_expires_at =
                Some(if exp > 10_000_000_000 { exp / 1000 } else { exp });
        }
        account.name.clone()
    };
    crate::vault::save_accounts(state, &mut accounts)?;
    // 记录成功刷新时刻：供 401 自愈 force 路径并发去重（FORCE_REFRESH_DEDUP_SECS 窗内复用）
    LAST_SUCCESS_REFRESH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(user_id.to_string(), std::time::Instant::now());

    // 自动解冻（含 SessionDead）：新 JWT 刚从 ExchangeToken 换发、必然有效，
    // 此前签到 401 打上的 SessionDead 永久冷却若不清除，调度会永远跳过该账号
    //（自动解冻逻辑明确排除 SessionDead，见 refresh_remaining_credits）
    let mut cd: AccountCooldownsFile = crate::store::docs::account_cooldowns_load(&crate::store::db(&state.data_dir));
    if let Some(entry) = cd.cooldowns.remove(user_id) {
        cd.updated_at = Some(fs_utils::now_iso());
        crate::store::docs::account_cooldowns_save(&crate::store::db(&state.data_dir), &cd)?;
        fs_utils::app_log(
            &state.data_dir,
            &format!("JWT 刷新成功自动解冻 [{}]（原冷却类型={}）", log_name, entry.error_type),
        );
    }

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

/// Trae JWT 定时批量续期（issue #27，调度器 `trae-renew` 共用）：
/// 遍历 vault 全账号，对「有 refresh_token 且未判失效且 lazy_refresh_needed（JWT 剩余
/// ≤48h 或 refresh_token 临期）」的账号逐个惰性刷新。复用 refresh_jwt_impl 全部防护
/// （并发锁/冷却/轮换写回/user_id 校验/失效标记），本函数只做批量编排，串行执行。
///
/// 返回计数 JSON；仅当发起过刷新且全部失败时返回 Err（调度器 30 分钟后重试），
/// invalid-only 等永久性失败计 skipped——避免无网络请求的本地错误整日重试刷日志。
pub fn renew_due_accounts_impl(state: &AppState) -> Result<serde_json::Value, String> {
    let accounts = crate::vault::load_accounts(state);
    let now_ts = chrono::Utc::now().timestamp();
    let (mut refreshed, mut skipped, mut no_rt, mut failed) = (0u32, 0u32, 0u32, 0u32);
    let mut details: Vec<String> = Vec::new();
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let has_rt = a.refresh_token.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
        if !has_rt {
            no_rt += 1; // 导入账号：无刷新前提（UI 已有「无 RT」徽标提示）
            continue;
        }
        // invalid 账号不在此跳过：交给 refresh_jwt_impl 入口拦截处理（其内部直接
        // 报「已失效」，不发网络请求）；renew 按错误文案归入 skipped，无重试风暴
        if !lazy_refresh_needed(&a.jwt, a.refresh_token_expires_at, now_ts) {
            skipped += 1; // JWT 剩余 >48h 且 rt 未临期：不轮换 = 缩小 IDE 互踢冲突面
            continue;
        }
        // force=false：与惰性门双保险（候选与判定之间状态可能变化）
        match refresh_jwt_impl(state, uid, false) {
            Ok(_) => refreshed += 1,
            Err(e) if e.contains("已失效") => {
                skipped += 1;
                details.push(format!("[{}] {}", a.name, e));
            }
            Err(e) => {
                failed += 1;
                details.push(format!("[{}] {}", a.name, e));
            }
        }
    }
    let v = serde_json::json!({
        "ok": failed == 0,
        "refreshed": refreshed,
        "skipped": skipped,
        "no_refresh_token": no_rt,
        "failed": failed,
        "details": details,
    });
    if failed > 0 && refreshed == 0 {
        Err(serde_json::to_string(&v).unwrap_or_default())
    } else {
        Ok(v)
    }
}

/// 记录一次 refresh_token 刷新失败（F-78 批次 3 生命周期管理）：
/// 服务端明确拒绝（code != 0 / user_id 不匹配）→ 递增计数并立即置 refresh_token_invalid=true，
/// 供调度与 UI 提前规避（原实现只能等签到 401 才暴露）；网络/解析类失败仅计数不置失效
///（调度 30 分钟重试会快速累计 3 次，若计入会把一过性网络故障误判为 refresh_token 不可用）。
/// 刷新成功在 refresh_jwt_impl 写回时清零；重新 OAuth 登录亦会重置（commands/oauth.rs）。
/// 调用方持有 jwt_refresh_lock，无并发写竞争。
fn record_refresh_failure(state: &AppState, user_id: &str, rejected: bool, err_msg: &str) {
    let mut accounts = crate::vault::load_accounts(state);
    let Some(acct) = accounts
        .accounts
        .iter_mut()
        .find(|a| a.user_id.as_deref() == Some(user_id))
    else {
        return;
    };
    // 网络/解析类失败（rejected=false）：计数但不触发失效（区分服务端明确拒绝）
    if !rejected {
        acct.refresh_token_fails = acct.refresh_token_fails.saturating_add(1);
        acct.updated_at = Some(fs_utils::now_iso());
        let (name, fails) = (acct.name.clone(), acct.refresh_token_fails);
        if let Err(e) = crate::vault::save_accounts(state, &mut accounts) {
            fs_utils::app_log(&state.data_dir, &format!("refresh_token 失败计数写入失败: {e}"));
        }
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "[{}] refresh_token 刷新失败（网络类，不计失效）第 {} 次: {}",
                name, fails, err_msg
            ),
        );
        return;
    }
    acct.refresh_token_fails = acct.refresh_token_fails.saturating_add(1);
    acct.refresh_token_invalid = true;
    acct.updated_at = Some(fs_utils::now_iso());
    let (name, fails, invalid) =
        (acct.name.clone(), acct.refresh_token_fails, acct.refresh_token_invalid);
    if let Err(e) = crate::vault::save_accounts(state, &mut accounts) {
        fs_utils::app_log(&state.data_dir, &format!("refresh_token 失败计数写入失败: {e}"));
    }
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "refresh_token 刷新失败 [{}] 连续第 {} 次{}: {}",
            name,
            fails,
            if invalid {
                "（已标记失效，需重新 OAuth 登录）"
            } else {
                ""
            },
            err_msg
        ),
    );
}

// ---------------- 内部工具 ----------------

/// 构建账号视图（聚合 JWT / 分组 / 设备 / 积分 / 今日签到 / 冷却状态 / 套餐身份）。
pub fn build_account_views(state: &AppState) -> Vec<AccountView> {
    let accounts = crate::vault::load_accounts(state);
    let groups: GroupsFile = crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    let device_map: DeviceMap = crate::store::docs::device_map_load(&crate::store::db(&state.data_dir));
    let credits: CreditsFile = crate::store::docs::credits_history_load(&crate::store::db(&state.data_dir));
    let rc: RemainingCreditsFile = crate::store::docs::remaining_credits_load(&crate::store::db(&state.data_dir));
    let cd: AccountCooldownsFile = crate::store::docs::account_cooldowns_load(&crate::store::db(&state.data_dir));
    let pay: crate::legacy_types::PayStatusFile =
        crate::store::docs::pay_status_load(&crate::store::db(&state.data_dir));
    let summary: CheckinSummary = crate::store::db(&state.data_dir).kv_get("checkin_summary");
    let summary_today = summary
        .time
        .as_ref()
        .map(|t| t.starts_with(&fs_utils::today_prefix()))
        .unwrap_or(false);
    // 「今日已签」判定（issue #24）：仅 ok==true（claim_ok / skip_already）算已签。
    // 旧条件 ok || (action != "fail" && !action.is_empty()) 会把失败的 claim 记录
    // （ok=false, action="claim"）误判为已签 → 状态误报 + 手动签到被跳过规则过滤。
    // 匹配键用 user_id（与账号视图同源于 JWT），旧记录缺 user_id 时回退按 name。
    let mut checked_uids: std::collections::HashSet<String> = Default::default();
    let mut checked_names: std::collections::HashSet<String> = Default::default();
    if summary_today {
        for r in &summary.results {
            if !r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                continue;
            }
            if let Some(u) = r
                .get("user_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                checked_uids.insert(u.to_string());
            }
            if let Some(n) = r.get("name").and_then(|v| v.as_str()) {
                checked_names.insert(n.to_string());
            }
        }
    }

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
            // 有 uid 严格按 uid 匹配（同名账号不互吞）；uid 为空才回退按 name
            if uid.is_empty() {
                checked_names.contains(&a.name)
            } else {
                checked_uids.contains(&uid)
            }
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
            // 审查 P1：列表/导出视图不再下发完整 JWT，仅掩码展示；
            // 需要完整凭据的场景（编辑回填）走 account_get_jwt 按需获取
            jwt: fs_utils::mask_secret(&a.jwt),
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
            total_credits: rc.total_limit.get(&uid).copied(),
            pay_identity: pay
                .statuses
                .get(&uid)
                .map(|p| p.identity_str.clone()),
            membership_expire: rc.membership_expire.get(&uid).copied(),
            membership_next_billing: rc.membership_next_billing.get(&uid).copied(),
            // refresh_token 生命周期（F-78 批次 3）：过期时间/连续失败次数/失效标记
            refresh_token_expires_at: a.refresh_token_expires_at,
            refresh_token_fails: a.refresh_token_fails,
            refresh_token_invalid: a.refresh_token_invalid,
            auth_saved_at: a.auth_saved_at.clone(),
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
    /// 期望值由 python -c 调用 auto_checkin.get_device_for 实测得出（2026-09-11）。
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
        assert_eq!(a.session_id.unwrap().len(), 64);
        // UUID v4 格式与版本/变体位
        let mu = a.market_user_id.unwrap();
        assert_eq!(mu.len(), 36);
        assert_eq!(&mu[14..15], "4");
        assert!(matches!(&mu[19..20], "8" | "9" | "a" | "b"));
    }

    // ── parse_credit_stats 通用/Work 到期口径（issue #28）───────────────────

    /// 构造积分包 JSON（credits_limit/usage/product_id/expire_time，字段层级与 API 实测一致）
    fn pack(product_id: i64, limit: f64, used: f64, expire: Option<i64>) -> serde_json::Value {
        serde_json::json!({
            "expire_time": expire,
            "usage": {"credits_amount": used},
            "entitlement_base_info": {
                "product_id": product_id,
                "quota": {"credits_limit": limit}
            }
        })
    }

    const NOW_TS: i64 = 1_800_000_000;

    #[test]
    fn parse_credit_stats_general_expire_ignores_work_pack() {
        // issue #28 核心：Work 包（209）先到期不得污染通用积分调度口径
        let stats = parse_credit_stats(
            &[
                pack(208, 100.0, 10.0, Some(NOW_TS + 86_400)),
                pack(209, 50.0, 0.0, Some(NOW_TS + 3_600)),
            ],
            NOW_TS,
        );
        assert_eq!(stats.earliest_expire, Some(NOW_TS + 3_600));
        assert_eq!(stats.general_earliest_expire, Some(NOW_TS + 86_400));
        assert_eq!(stats.general, 90.0);
        assert_eq!(stats.work, 50.0);
    }

    #[test]
    fn parse_credit_stats_only_work_expiring_leaves_general_none() {
        // 通用包长期有效 + Work 包临期：通用调度口径应为 None（无到期约束）
        let stats = parse_credit_stats(
            &[
                pack(208, 100.0, 10.0, None),
                pack(209, 50.0, 0.0, Some(NOW_TS + 60)),
            ],
            NOW_TS,
        );
        assert_eq!(stats.earliest_expire, Some(NOW_TS + 60));
        assert_eq!(stats.general_earliest_expire, None);
    }

    #[test]
    fn parse_credit_stats_excludes_expired_and_drained_packs() {
        // 已用完/已过期的包不参与两个口径的最早到期统计
        let stats = parse_credit_stats(
            &[
                pack(208, 100.0, 100.0, Some(NOW_TS + 60)),
                pack(208, 100.0, 0.0, Some(NOW_TS - 60)),
                pack(208, 80.0, 20.0, Some(NOW_TS + 120)),
            ],
            NOW_TS,
        );
        assert_eq!(stats.earliest_expire, Some(NOW_TS + 120));
        assert_eq!(stats.general_earliest_expire, Some(NOW_TS + 120));
    }

    /// 实测探针（默认忽略）：用真实数据目录 + vault 凭据验证积分接口设备指纹修复。
    /// 运行：cargo test probe_credit -- --ignored --nocapture
    /// 注意：应用正在运行时 vault 快照可能被锁，load_accounts 会降级读不到 JWT（探针报错无害）。
    #[test]
    #[ignore]
    fn probe_credit_query_real_accounts() {
        let state = crate::state::AppState::new().expect("构造 AppState 失败");
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
                match refresh_jwt_impl(&state, &uid, true) {
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

    // ── 惰性刷新门（lazy_refresh_needed）边界测试 ────────────────────────────
    // 语义：true = 需要刷新；false = 跳过 ExchangeToken（少一次调用 = 少一次
    // refresh_token 轮换 = 缩小双端互踢冲突面）

    /// 基准时刻（固定 now，全部边界相对它推算，不依赖真实时钟）
    const T0: i64 = 1_700_000_000;
    /// 阈值 48h（秒），须与 TRAE_LAZY_REFRESH_MIN_SECS 一致
    const H48: i64 = 48 * 3600;

    /// 构造带指定 exp 的最小 JWT（header.payload.sig 形态；parse 不验签，
    /// payload 为 base64url 无填充编码，含 data.id + exp）
    fn jwt_with_exp(exp: Option<i64>) -> String {
        use base64::Engine as _;
        let payload = match exp {
            Some(e) => format!(r#"{{"data":{{"id":"u-test"}},"exp":{e}}}"#),
            None => r#"{"data":{"id":"u-test"}}"#.to_string(),
        };
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("h.{enc}.s")
    }

    /// JWT 剩余 >48h 且 refresh_token 剩余 >48h：跳过刷新
    #[test]
    fn test_lazy_refresh_needed_fresh_jwt_and_rt_skips() {
        let jwt = jwt_with_exp(Some(T0 + H48 + 3600)); // 剩余 49h
        let rt = Some(T0 + 96 * 3600); // 剩余 96h
        assert!(!lazy_refresh_needed(&jwt, rt, T0));
    }

    /// JWT 剩余恰好 48h：jwt_fresh 为严格大于（> 48h）→ 不算新鲜 → 触发刷新
    #[test]
    fn test_lazy_refresh_needed_jwt_48h_boundary_triggers() {
        let jwt = jwt_with_exp(Some(T0 + H48));
        assert!(lazy_refresh_needed(&jwt, Some(T0 + 96 * 3600), T0));
    }

    /// JWT 剩余不足 48h：触发刷新
    #[test]
    fn test_lazy_refresh_needed_jwt_under_48h_triggers() {
        let jwt = jwt_with_exp(Some(T0 + H48 - 3600)); // 剩余 47h
        assert!(lazy_refresh_needed(&jwt, Some(T0 + 96 * 3600), T0));
    }

    /// JWT 仍新鲜但 refresh_token 临期（< 48h）：提前换发防 rt 失效后无法自愈
    #[test]
    fn test_lazy_refresh_needed_rt_expiring_forces_refresh() {
        let jwt = jwt_with_exp(Some(T0 + 200 * 3600)); // 剩余 200h
        let rt = Some(T0 + H48 - 1); // 剩余 48h - 1s
        assert!(lazy_refresh_needed(&jwt, rt, T0));
    }

    /// refresh_token 剩余恰好 48h：rt_expiring 为严格小于（< 48h）→ 不算临期，不触发
    #[test]
    fn test_lazy_refresh_needed_rt_48h_boundary_no_trigger() {
        let jwt = jwt_with_exp(Some(T0 + 200 * 3600));
        assert!(!lazy_refresh_needed(&jwt, Some(T0 + H48), T0));
    }

    /// JWT exp 缺失或 token 损坏：解析不出 exp_timestamp → 保守触发刷新
    #[test]
    fn test_lazy_refresh_needed_missing_exp_forces_refresh() {
        assert!(lazy_refresh_needed("not-a-jwt", Some(T0 + 96 * 3600), T0));
        assert!(lazy_refresh_needed(&jwt_with_exp(None), Some(T0 + 96 * 3600), T0));
    }

    /// JWT 已过期（exp - now 为负）：触发刷新
    #[test]
    fn test_lazy_refresh_needed_expired_jwt_triggers() {
        let jwt = jwt_with_exp(Some(T0 - 3600)); // 已过期 1h
        assert!(lazy_refresh_needed(&jwt, Some(T0 + 96 * 3600), T0));
    }

    /// 无 refresh_token 的账号（rt 缺失）：只要 JWT 新鲜就不触发——
    /// rt_expiring 对 None 恒 false，不因缺 rt 而提前刷
    #[test]
    fn test_lazy_refresh_needed_no_rt_not_triggered_early() {
        let jwt = jwt_with_exp(Some(T0 + 49 * 3600));
        assert!(!lazy_refresh_needed(&jwt, None, T0));
    }

    // ── renew_due_accounts_impl 批量续期编排测试（issue #27）───────────────
    // 约束：VAULT 为进程级单例（首个 open() 绑定目录），且「临期触发刷新」分支
    // 会发起真实网络请求——故全部断言共用一个临时目录，且只种可离线判定的账号
    // （无 rt / 已失效 / JWT 新鲜）；网络刷新成功路径不入单测（联调覆盖）。
    #[test]
    fn test_renew_due_accounts_skip_branches_and_counts() {
        let dir = std::env::temp_dir()
            .join(format!("aiwork_renew_orch_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("data"));
        let state = crate::state::AppState {
            data_dir: dir,
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        let now = chrono::Utc::now().timestamp();
        let acc = |name: &str, uid: Option<&str>, exp: i64, rt: Option<&str>, invalid: bool| {
            RawAccount {
                name: name.into(),
                user_id: uid.map(|s| s.to_string()),
                jwt: jwt_with_exp(Some(exp)),
                refresh_token: rt.map(|s| s.to_string()),
                // rt 过期时间给足 30 天：确保新鲜账号不因 rt 临期误触发
                refresh_token_expires_at: rt.map(|_| now + 30 * 24 * 3600),
                refresh_token_invalid: invalid,
                ..Default::default()
            }
        };
        let mut f = AccountsFile {
            accounts: vec![
                // JWT 剩余 7 天（>48h）且 rt 未临期 → 惰性门跳过
                acc("fresh", Some("uid-fresh"), now + 7 * 24 * 3600, Some("rt-fresh"), false),
                // JWT 已过期但无 refresh_token → 无法刷新，计 no_refresh_token
                acc("nort", Some("uid-nort"), now - 3600, None, false),
                // 已判失效 → 刷新前拦截，计 skipped 并出明细（不发网络请求）
                acc("invalid", Some("uid-invalid"), now - 3600, Some("rt-invalid"), true),
                // 无 user_id → 无法定位设备/刷新，编排层直接忽略
                acc("nouid", None, now - 3600, Some("rt-nouid"), false),
            ],
        };
        crate::vault::save_accounts(&state, &mut f).unwrap();

        let v =
            serde_json::to_value(renew_due_accounts_impl(&state).unwrap()).unwrap();
        assert_eq!(v["refreshed"], 0, "三种可离线分支均不应调用 ExchangeToken");
        assert_eq!(v["skipped"], 2, "新鲜账号 + 已失效账号计入 skipped");
        assert_eq!(v["no_refresh_token"], 1, "无 rt 账号单列计数");
        assert_eq!(v["failed"], 0);
        assert_eq!(v["ok"], true, "零刷新零失败 = 无事可做的成功");
        let details = v["details"].as_array().unwrap();
        assert!(
            details
                .iter()
                .any(|d| d.as_str().unwrap_or("").contains("invalid")),
            "失效账号应出现在明细中（提示重新 OAuth 登录）"
        );
    }
}
