//! WorkBuddy 账号域（原 workbuddy.rs 机械拆分）：账号池 CRUD（F-04）、
//! M3 凭证续期（F-09）、账号库导入导出（F-46 扩展）。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。
//! Web 化改造：删除 workbuddy_env_check（桌面环境检测）/ workbuddy_open_auth_dir
//! （打开本机目录）与 wb_upstream_accounts（已下沉 api_server::runtime）。

use serde::Serialize;

use crate::fs_utils;
use crate::state::AppState;

use super::common::{
    account_id_of, as_str, as_ts_seconds, auth_file_path_of, load_cli_rotate_state, load_pool,
    save_cli_rotate_state, save_pool, upsert_token_store, wb_renew_locks, WorkBuddyAccount, WbPool,
};

// ── 数据结构 ────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct WorkBuddyAccountView {
    pub id: String,
    pub uid: String,
    pub nickname: String,
    pub phone_masked: String,
    pub edition_type: String,
    pub access_token_expires_at: Option<i64>,
    pub refresh_token_expires_at: Option<i64>,
    pub auth_saved_at: Option<i64>,
    pub needs_relogin: bool,
    pub relogin_reason: String,
    pub group_id: String,
    pub note: String,
    pub credits_balance: Option<f64>,
    pub credits_fetched_at: Option<String>,
    /// 在线 = 本机 auth 文件当前生效账号（F-54 双态）
    pub is_current: bool,
    /// 有工具侧凭证副本或 auth 文件匹配
    pub has_credential: bool,
    /// 已录快照（PS 桥 profiles_workbuddy/<id>/）
    pub has_snapshot: bool,
    /// F2-2 已录 CodeBuddy 快照（profiles_codebuddy/<id>/）——双端登录态分端展示
    pub has_snapshot_codebuddy: bool,
    /// F2-2 WorkBuddy 端当前账号（桥 profiles_workbuddy/current_account.txt 标记）
    pub is_current_workbuddy: bool,
    /// F2-2 CodeBuddy 端当前账号（桥 profiles_codebuddy/current_account.txt 标记）
    pub is_current_codebuddy: bool,
}

/// auth 文件扫描结果（F-04 导入预览；凭证字段不回传前端）
#[derive(Serialize, Clone)]
pub struct WorkBuddyScanResult {
    pub id: String,
    pub uid: String,
    pub nickname: String,
    pub edition_type: String,
    pub has_access_token: bool,
    pub has_refresh_token: bool,
    pub access_token_expires_at: Option<i64>,
    /// 已在池中（同 token → 同 id）
    pub exists: bool,
    /// 同一账号（同 uid）已在池中：客户端换发 token 后 id 会变，但按账号身份（uid）判定已在池；
    /// 前端可后续据此展示，导入确认后走原位更新，不会重复入池
    pub already_in_pool: bool,
}

// ── 账号池（F-04）──────────────────────────────────────────────────────────

pub fn workbuddy_accounts_list(state: &AppState) -> Result<Vec<WorkBuddyAccountView>, String> {
    accounts_list_inner(state)
}

fn accounts_list_inner(state: &AppState) -> Result<Vec<WorkBuddyAccountView>, String> {
    let pool = load_pool(state);
    // 在线判定：auth 文件 uid 与账号一致（客户端当前生效登录）
    let auth_uid = {
        let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path_of(state));
        if raw.is_object() { as_str(fs_utils::dig(&raw, &["uid"])) } else { None }
    };
    let snap_path = state.data_dir.join("data").join("profiles_workbuddy");
    // F2-2：CodeBuddy 快照目录 + 双端"当前账号"标记（桥按端写入 <profiles>/current_account.txt）
    let snap_cb_path = state.data_dir.join("data").join("profiles_codebuddy");
    let cur_wb = read_current_account_marker(&snap_path);
    let cur_cb = read_current_account_marker(&snap_cb_path);
    // F2-5（实测 2026-09-15）：CodeBuddy 端「当前账号」以 live 登录信号为准——
    // genie.userId（客户端随登录/切换改写，池内按 uid 反查账号 id）。桥标记只反映
    // 上次切换目标：客户端内手动重登、切换后客户端未接受恢复（守卫跳过回写）时均失真，
    // 徽标会指向未登录的账号。genie 不可读（客户端未装/storage.json 缺失）回退桥标记。
    // WorkBuddy 端不受影响：is_current 用 auth 文件 uid（本就是 WorkBuddy 登录真源）。
    let cb_live_id = codebuddy_live_uid()
        .and_then(|uid| {
            pool.accounts.iter().find(|a| a.uid == uid).map(|a| a.id.clone())
        })
        .or(cur_cb.clone());
    let store: serde_json::Value = crate::tasks::wb_common::load_token_store(state);
    let store_tokens = store.get("tokens").cloned().unwrap_or(serde_json::Value::Null);

    let views = pool
        .accounts
        .iter()
        .map(|a| {
            let has_cred = store_tokens.get(&a.id).is_some()
                || auth_uid.as_deref() == Some(a.uid.as_str());
            let has_snapshot = snap_path.join(&a.id).is_dir();
            WorkBuddyAccountView {
                id: a.id.clone(),
                uid: a.uid.clone(),
                nickname: a.nickname.clone(),
                phone_masked: a.phone_masked.clone(),
                edition_type: a.edition_type.clone(),
                access_token_expires_at: a.access_token_expires_at,
                refresh_token_expires_at: a.refresh_token_expires_at,
                auth_saved_at: a.auth_saved_at,
                needs_relogin: a.needs_relogin,
                relogin_reason: a.relogin_reason.clone(),
                group_id: a.group_id.clone(),
                note: a.note.clone(),
                credits_balance: a.credits_balance,
                credits_fetched_at: a.credits_fetched_at.clone(),
                is_current: auth_uid.is_some() && auth_uid == Some(a.uid.clone()),
                has_credential: has_cred,
                has_snapshot: has_snapshot,
                has_snapshot_codebuddy: snap_cb_path.join(&a.id).is_dir(),
                is_current_workbuddy: cur_wb.as_deref() == Some(a.id.as_str()),
                is_current_codebuddy: cb_live_id.as_deref() == Some(a.id.as_str()),
            }
        })
        .collect();
    Ok(views)
}

pub fn workbuddy_account_save(state: &AppState, user_id: String, name: Option<String>, note: Option<String>) -> Result<(), String> {
    let mut pool = load_pool(state);
    let acct = pool
        .accounts
        .iter_mut()
        .find(|a| a.id == user_id)
        .ok_or_else(|| format!("账号不存在: {user_id}"))?;
    if let Some(n) = name {
        acct.nickname = n;
    }
    if let Some(n) = note {
        acct.note = n;
    }
    save_pool(state, &pool)
}

pub fn workbuddy_account_remove(state: &AppState, user_id: String, delete_snapshot: Option<bool>) -> Result<(), String> {
    // 审查 P1：id 将作为 profiles_workbuddy/<id> 目录名参与 remove_dir_all，
    // 先过字符集白名单，杜绝 `..`/绝对路径注入导致的目录逃逸删除
    fs_utils::ensure_uid_safe(&user_id)?;
    let mut pool = load_pool(state);
    let before = pool.accounts.len();
    pool.accounts.retain(|a| a.id != user_id);
    if pool.accounts.len() == before {
        return Err(format!("账号不存在: {user_id}"));
    }
    save_pool(state, &pool)?;
    if delete_snapshot.unwrap_or(false) {
        // F2-2：双端快照槽对称清理——只清 WorkBuddy 会留下 CodeBuddy 孤儿槽，
        // 且账号列表徽标虽随池删除消失，孤儿目录持续占用磁盘。
        let slot = state.data_dir.join("data").join("profiles_workbuddy").join(&user_id);
        if slot.is_dir() {
            let _ = std::fs::remove_dir_all(&slot);
        }
        let slot_cb = state.data_dir.join("data").join("profiles_codebuddy").join(&user_id);
        if slot_cb.is_dir() {
            let _ = std::fs::remove_dir_all(&slot_cb);
        }
    }
    // 被删账号是 CodeBuddy CLI 当前号 → 清理轮换状态残留（active_account_id），
    // 避免轮换日志参考失真（下次轮换 current 判定按 settings.json 自愈）
    let mut st = load_cli_rotate_state(state);
    if st.active_account_id.as_deref() == Some(user_id.as_str()) {
        st.active_account_id = None;
        let _ = save_cli_rotate_state(state, &st);
    }
    Ok(())
}

/// 扫描本机 auth 文件（F-04 导入预览；不写盘）
pub fn workbuddy_scan_auth_file(state: &AppState) -> Result<Option<WorkBuddyScanResult>, String> {
    let path = auth_file_path_of(state);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs_utils::read_json::<serde_json::Value>(&path);
    if !raw.is_object() {
        return Err("auth 文件格式无法识别（JSON 解析失败）".into());
    }
    let access = as_str(fs_utils::dig(&raw, &["accessToken", "access_token"]))
        .ok_or("auth 文件中未找到 accessToken（结构可能已变更）")?;
    if access.is_empty() {
        return Err("auth 文件 accessToken 为空（可能未登录）".into());
    }
    let id = account_id_of(&access);
    let uid = as_str(fs_utils::dig(&raw, &["uid"])).unwrap_or_default();
    let pool = load_pool(state);
    let exists = pool.accounts.iter().any(|a| a.id == id);
    let already_in_pool = !uid.is_empty() && pool.accounts.iter().any(|a| a.uid == uid);
    Ok(Some(WorkBuddyScanResult {
        id,
        uid,
        nickname: as_str(fs_utils::dig(&raw, &["nickname", "displayName", "name"])).unwrap_or_default(),
        edition_type: as_str(fs_utils::dig(&raw, &["editionType", "edition"])).unwrap_or_default(),
        has_access_token: true,
        has_refresh_token: as_str(fs_utils::dig(&raw, &["refreshToken", "refresh_token"])).is_some(),
        access_token_expires_at: as_ts_seconds(fs_utils::dig(&raw, &["expiresAtMs", "expiresAt", "expires_in_ms"])),
        exists,
        already_in_pool,
    }))
}

/// 池内账号幂等匹配（账号身份以 uid 为准）：uid 非空时按 uid 匹配，或按 id（token 派生）兜底匹配，
/// 保证「同 uid 换 token」与「同 token（auth 文件缺 uid / uid 漂移）」都不会产生重复条目。
/// pub(super)：OAuth 自动入池（oauth.rs）复用同一匹配语义，避免同 uid 重复入池。
pub(super) fn find_uid_or_id<'a>(pool: &'a mut WbPool, uid: &str, id: &str) -> Option<&'a mut WorkBuddyAccount> {
    pool.accounts
        .iter_mut()
        .find(|a| (!uid.is_empty() && a.uid == uid) || (!id.is_empty() && a.id == id))
}

/// F2-2：读桥按端写入的当前账号标记（<profiles_workbuddy|profiles_codebuddy>/current_account.txt）。
/// 文件不存在/空 → None（该端从未切换过）。
/// 注意剥 BOM：桥（Windows PowerShell 5.1）Set-Content -Encoding UTF8 写出带 BOM，
/// 而 U+FEFF 不属于 Rust trim() 的空白字符，不剥则标记永远匹配失败。
fn read_current_account_marker(dir: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(dir.join("current_account.txt")).ok()?;
    let t = s.trim().trim_start_matches('\u{feff}').trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// F-74：读指定端的当前账号标记（桥按端写入的 `profiles_<app>/current_account.txt`）。
/// 与 `is_current_workbuddy` / `is_current_codebuddy` 徽标同源；文件不存在 → None。
pub fn current_account_marker(state: &AppState, app: &str) -> Option<String> {
    let dir = match app {
        "CodeBuddy" => "profiles_codebuddy",
        _ => "profiles_workbuddy",
    };
    read_current_account_marker(&state.data_dir.join("data").join(dir))
}

/// F1-3（switch.rs 防误覆盖守卫用）：读共享 auth 文件当前 uid → 账号池中反查账号 id。
/// 返回的 id 与桥的 current_account.txt 同命名空间，供 -ExpectedCurrentUid 比对；
/// None = 未登录 / 池中无此账号（调用方 fail-open 传空串，不阻断切换）。
/// 注意：auth 文件是两端共同的"最近写入者"信号——WorkBuddy（auth 驱动）准确；
/// CodeBuddy（vscdb 驱动）被 WorkBuddy 覆盖时反查失败 → None，守卫按既定语义跳过回写。
pub fn pool_account_id_by_auth_uid(state: &AppState) -> Option<String> {
    let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path_of(state));
    if !raw.is_object() {
        return None;
    }
    let uid = as_str(fs_utils::dig(&raw, &["uid"]))?;
    if uid.is_empty() {
        return None;
    }
    let pool = load_pool(state);
    pool.accounts.iter().find(|a| a.uid == uid).map(|a| a.id.clone())
}

/// 池内按真实账号 uid（auth/genie 体系 uuid）反查账号 id（wb-<hash>）。
/// F2-5（switch.rs 保存/切换守卫用）：把客户端 live 登录 uid 映射到账号池 id。
pub fn pool_account_id_by_uid(state: &AppState, uid: &str) -> Option<String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return None;
    }
    load_pool(state)
        .accounts
        .into_iter()
        .find(|a| a.uid == uid)
        .map(|a| a.id)
}

/// CodeBuddy 客户端 live 登录 uid：`%APPDATA%\CodeBuddy CN\User\globalStorage\storage.json`
/// 的 `genie.userId`。F2-5：这是 CodeBuddy 当前登录的**真源信号**——共享 auth 文件属
/// WorkBuddy（会被其覆盖），不能作为 CodeBuddy 的登录证据；genie.userId 由客户端随
/// 登录/切换改写（实测：恢复某账号快照启动后该值变为该账号 uid）。
/// None = 文件不存在/未登录/解析失败（调用方 fail-open）。
pub fn codebuddy_live_uid() -> Option<String> {
    let appdata = std::env::var("APPDATA").ok()?;
    let p = std::path::PathBuf::from(appdata)
        .join("CodeBuddy CN")
        .join("User")
        .join("globalStorage")
        .join("storage.json");
    let raw = fs_utils::read_json::<serde_json::Value>(&p);
    let uid = as_str(fs_utils::dig(&raw, &["genie.userId"]))?;
    (!uid.is_empty()).then_some(uid.to_string())
}

/// auth 文件导入的池合并输入（从 auth 文件解析出的可覆盖字段集合）
struct AuthMerge {
    /// 本次 token 派生 id（wb-<sha256 前 12 位>）；命中已有条目时不采用，保留旧 id
    id: String,
    uid: String,
    nickname: String,
    edition_type: String,
    access_token_expires_at: Option<i64>,
    refresh_token_expires_at: Option<i64>,
}

/// auth 文件入池合并（纯逻辑，便于单测）：按 uid（优先）/ id（兜底）匹配已有条目 →
/// 原位更新（uid/昵称/版本/过期时间/auth_saved_at 以新值覆盖）并**保留原 id**，返回该 id；
/// 未命中 → 新增（沿用现有 id 生成规则），返回新 id。
/// 保留原 id 的取舍：id 派生自 token，客户端换发 token 后重导入会派生新 id，若改用新 id
/// 会使已有快照（profiles_workbuddy/<id>/）、分组与外部引用悬空，故沿用旧 id 作为账号身份。
fn merge_auth_entry(pool: &mut WbPool, m: AuthMerge) -> String {
    if let Some(a) = find_uid_or_id(pool, &m.uid, &m.id) {
        // 重复导入（同 token，或同账号换发 token）= 原位更新，不产生重复条目
        if !m.uid.is_empty() {
            a.uid = m.uid;
        }
        a.nickname = m.nickname;
        if !m.edition_type.is_empty() {
            a.edition_type = m.edition_type;
        }
        a.access_token_expires_at = m.access_token_expires_at;
        a.refresh_token_expires_at = m.refresh_token_expires_at;
        a.auth_saved_at = Some(chrono::Utc::now().timestamp());
        a.needs_relogin = false;
        a.id.clone()
    } else {
        let id = m.id.clone();
        pool.accounts.push(WorkBuddyAccount {
            id: id.clone(),
            uid: m.uid,
            nickname: m.nickname,
            edition_type: m.edition_type,
            access_token_expires_at: m.access_token_expires_at,
            refresh_token_expires_at: m.refresh_token_expires_at,
            auth_saved_at: Some(chrono::Utc::now().timestamp()),
            ..Default::default()
        });
        id
    }
}

/// auth 文件导入入池（F-04）：写账号池 + 工具侧凭证副本（掩码入池、凭证不外泄）。
/// 按 uid 幂等：重复导入（同 token 或同账号换 token）原位更新并保留原 id，不产生重复条目。
pub fn workbuddy_account_import_auth(state: &AppState, name: Option<String>) -> Result<WorkBuddyAccountView, String> {
    let path = auth_file_path_of(state);
    if !path.exists() {
        return Err("未找到 auth 文件，请先在 WorkBuddy 客户端登录".into());
    }
    let raw = fs_utils::read_json::<serde_json::Value>(&path);
    let access = as_str(fs_utils::dig(&raw, &["accessToken", "access_token"]))
        .ok_or("auth 文件中未找到 accessToken")?;
    let refresh = as_str(fs_utils::dig(&raw, &["refreshToken", "refresh_token"]));
    let id = account_id_of(&access);
    let uid = as_str(fs_utils::dig(&raw, &["uid"])).unwrap_or_default();
    let nickname = name
        .clone()
        .or_else(|| as_str(fs_utils::dig(&raw, &["nickname", "displayName", "name"])))
        .unwrap_or_else(|| uid.chars().take(8).collect());

    let mut pool = load_pool(state);
    let target_id = merge_auth_entry(
        &mut pool,
        AuthMerge {
            id,
            uid: uid.clone(),
            nickname,
            edition_type: as_str(fs_utils::dig(&raw, &["editionType", "edition"])).unwrap_or_default(),
            access_token_expires_at: as_ts_seconds(fs_utils::dig(&raw, &["expiresAtMs", "expiresAt", "expires_in_ms"])),
            refresh_token_expires_at: as_ts_seconds(fs_utils::dig(&raw, &["refreshExpiresAt", "refresh_expires_at"])),
        },
    );
    save_pool(state, &pool)?;

    // 工具侧凭证副本（F-10 双源化）：写回保留的原 id 名下（换 token 重导入时与池条目对齐，
    // 避免 pool 用旧 id / token store 用新 id 导致凭证与账号脱钩）
    let creds = serde_json::json!({
        "access_token": access,
        "refresh_token": refresh,
        // 审查 P2：到期时间兼容整数毫秒/秒与 RFC3339 字符串（as_ts_seconds 归一为秒，再折算毫秒存储）
        "expires_at_ms": as_ts_seconds(fs_utils::dig(&raw, &["expiresAtMs", "expiresAt"])).map(|s| s * 1000),
        "uid": uid,
        "domain": as_str(fs_utils::dig(&raw, &["domain"])),
    });
    upsert_token_store(state, &target_id, &creds)?;

    // 返回合并视图（简化：直接重查）
    let views = accounts_list_inner(state)?;
    views
        .into_iter()
        .find(|v| v.id == target_id)
        .ok_or_else(|| "导入后回读失败".into())
}

// ── M3 凭证续期（F-09，Rust 侧手动触发）───────────────────────────────────

/// 刷新单账号凭证：读 token store（双源副本）→ POST plugin refresh → 回写。
/// 客户端运行中跳过 auth 文件写入（只更新工具侧副本，F-10 保证谁新用谁）。
/// force=false（默认）时惰性：accessToken 剩余有效期充足则跳过续期（减少无谓
/// refresh 调用——refresh 端点可能轮换 refreshToken，减少轮换面）；
/// force=true 强制续期（手动按钮使用）。
pub fn workbuddy_refresh_token(
    state: &AppState,
    user_id: String,
    force: Option<bool>,
) -> Result<String, String> {
    // 每账号续期互斥（审查 P1）：并发触发同一账号续期时后到者直接拒绝，
    // 避免双请求交错回写 token store / 账号池造成凭证覆盖竞态
    let acct_lock = {
        let mut table = wb_renew_locks()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        table
            .entry(user_id.clone())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _renew_guard = acct_lock
        .try_lock()
        .map_err(|_| "该账号正在续期中，请等待当前续期完成后再试".to_string())?;

    let mut pool = load_pool(state);
    let acct = pool
        .accounts
        .iter()
        .find(|a| a.id == user_id)
        .ok_or_else(|| format!("账号不存在: {user_id}"))?
        .clone();

    // 惰性续期门（force=false 时生效）：判定抽为纯函数 lazy_renew_skippable。
    // expires_at 缺失时放行（无法判定就续一次，顺带拿到准确的 expiresIn）。
    if !force.unwrap_or(false)
        && lazy_renew_skippable(acct.access_token_expires_at, chrono::Utc::now().timestamp())
    {
        let remaining = acct.access_token_expires_at.unwrap_or_default() - chrono::Utc::now().timestamp();
        return Err(format!(
            "凭证剩余有效期 {:.1} 小时，暂无需续期（如需立即续期请使用手动续期）",
            remaining as f64 / 3600.0
        ));
    }

    // 双源取值「谁新用谁」（审查 P1）：token store 副本与桌面 auth 文件都可能被更新
    // （客户端运行时刷新 auth 文件，工具侧续期刷新 token store）。分别解析两源的
    // refresh 到期时间（as_ts_seconds 统一归一为秒：整数毫秒/秒与 RFC3339 字符串均可），
    // 取到期更晚的一源作为生效 refresh_token；无到期信息的源视为 0（最旧），
    // 同新/均无到期时优先工具侧 store 副本（保持原有语义）。
    // auth 文件仅在 uid 与本账号匹配时采信，防止把客户端当前登录的其他账号 token 串号。
    let store: serde_json::Value = crate::tasks::wb_common::load_token_store(state);
    let rec = store.get("tokens").and_then(|t| t.get(&acct.id)).cloned().unwrap_or_default();
    let store_cand = (
        as_str(fs_utils::dig(&rec, &["refresh_token"])),
        as_ts_seconds(fs_utils::dig(&rec, &["refresh_expires_at_ms", "refreshExpiresAt", "refresh_expires_at"]))
            .or_else(|| as_ts_seconds(fs_utils::dig(&rec, &["expires_at_ms"]))),
    );
    let auth_raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path_of(state));
    let auth_cand = {
        let fuid = as_str(fs_utils::dig(&auth_raw, &["uid"]));
        if fuid.as_deref() == Some(acct.uid.as_str()) && !acct.uid.is_empty() {
            (
                as_str(fs_utils::dig(&auth_raw, &["refreshToken", "refresh_token"])),
                as_ts_seconds(fs_utils::dig(&auth_raw, &["refreshExpiresAt", "refresh_expires_at"]))
                    .or_else(|| as_ts_seconds(fs_utils::dig(&auth_raw, &["expiresAtMs", "expiresAt"]))),
            )
        } else {
            (None, None)
        }
    };
    let pick_newer = |a: &(Option<String>, Option<i64>), b: &(Option<String>, Option<i64>)| -> Option<String> {
        let a_tok = a.0.as_deref().filter(|t| !t.is_empty());
        let b_tok = b.0.as_deref().filter(|t| !t.is_empty());
        match (a_tok, b_tok) {
            (Some(x), Some(y)) => {
                if b.1.unwrap_or(0) > a.1.unwrap_or(0) {
                    Some(y.to_string())
                } else {
                    Some(x.to_string())
                }
            }
            (Some(x), None) => Some(x.to_string()),
            (None, Some(y)) => Some(y.to_string()),
            (None, None) => None,
        }
    };
    let refresh = pick_newer(&store_cand, &auth_cand)
        .ok_or("该账号无 refreshToken（不可刷新，需重新登录）")?;
    if refresh.is_empty() {
        return Err("该账号 refreshToken 为空（不可刷新，需重新登录）".into());
    }

    // 红线：X-Refresh-Token 仅出现在 refresh 端点；30s 超时（审查 P2，与其余 ureq 调用点一致）
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let resp = agent
        .post("https://www.codebuddy.cn/v2/plugin/auth/token/refresh")
        .set("Authorization", "Bearer")
        .set("User-Agent", "WorkBuddy")
        .set("X-Refresh-Token", &refresh)
        .set("X-Auth-Refresh-Source", "workbuddy")
        .set("Content-Type", "application/json")
        .send_string("{}");
    let body: serde_json::Value = match resp {
        Ok(r) => r.into_json().unwrap_or_default(),
        Err(ureq::Error::Status(code, r)) => {
            let _ = r;
            return Err(format!("刷新失败（HTTP {code}）：refresh token 可能已失效，需重新登录"));
        }
        Err(e) => return Err(format!("刷新请求失败: {e}")),
    };
    let new_access = as_str(fs_utils::dig(&body, &["accessToken"])).ok_or("刷新响应中无 accessToken")?;
    let new_refresh = as_str(fs_utils::dig(&body, &["refreshToken"]));
    let expires_in: Option<i64> = fs_utils::dig(&body, &["expiresIn"]).and_then(|v| v.as_i64());
    let refresh_expires_in: Option<i64> = fs_utils::dig(&body, &["refreshExpiresIn"]).and_then(|v| v.as_i64());
    let now_ms = chrono::Utc::now().timestamp_millis();
    let exp_ms = expires_in.map(|s| now_ms + s * 1000);
    let rexp_ms = refresh_expires_in.map(|s| now_ms + s * 1000);

    // 回写工具侧副本 + 账号池过期时间
    let creds = serde_json::json!({
        "access_token": new_access,
        "refresh_token": new_refresh,
        "expires_at_ms": exp_ms,
        "refresh_expires_at_ms": rexp_ms,
    });
    upsert_token_store(state, &acct.id, &creds)?;
    if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == acct.id) {
        a.access_token_expires_at = exp_ms.map(|m| m / 1000);
        a.refresh_token_expires_at = rexp_ms.map(|m| m / 1000);
        a.needs_relogin = false;
        a.relogin_reason.clear();
    }
    save_pool(state, &pool)?;
    fs_utils::app_log(&state.data_dir, &format!("WorkBuddy 凭证续期成功: {}", acct.id));
    Ok("凭证已续期".into())
}

/// 惰性续期判定（纯函数化便于单测）：true = 可跳过（剩余有效期充足）。
/// 跳过 = accessToken 剩余 > 24h——refresh 端点可能轮换 refreshToken，减少无谓轮换。
/// expires_at 缺失 → false（放行刷新，顺带拿到准确的 expiresIn）。
fn lazy_renew_skippable(access_token_expires_at: Option<i64>, now_ts: i64) -> bool {
    const WB_LAZY_RENEW_MIN_SECS: i64 = 24 * 3600;
    match access_token_expires_at {
        None => false,
        Some(exp) => exp - now_ts > WB_LAZY_RENEW_MIN_SECS,
    }
}

// ── 账号库导入导出扩展（F-46 扩展，批次3 T3.6）────────────────────────────

/// 导出账号池（F-46 扩展）：元数据必含；include_credentials=true 时附工具侧凭证副本
///（迁移场景用；导出文件等同密码，由调用方提示）。与 Trae accounts_export_raw 同语义。
pub fn workbuddy_accounts_export(state: &AppState, include_credentials: Option<bool>) -> Result<serde_json::Value, String> {
    let pool = load_pool(state);
    let include_cred = include_credentials.unwrap_or(false);
    let store: serde_json::Value = if include_cred {
        crate::tasks::wb_common::load_token_store(state)
    } else {
        serde_json::json!({})
    };
    let tokens = store.get("tokens").cloned().unwrap_or(serde_json::Value::Null);
    let accounts: Vec<serde_json::Value> = pool
        .accounts
        .iter()
        .map(|a| {
            let mut v = serde_json::json!({
                "id": a.id,
                "uid": a.uid,
                "nickname": a.nickname,
                "phone_masked": a.phone_masked,
                "edition_type": a.edition_type,
                "access_token_expires_at": a.access_token_expires_at,
                "refresh_token_expires_at": a.refresh_token_expires_at,
                "group_id": a.group_id,
                "note": a.note,
            });
            if include_cred {
                v["credential"] = tokens.get(&a.id).cloned().unwrap_or(serde_json::Value::Null);
            }
            v
        })
        .collect();
    Ok(serde_json::json!({
        "kind": "aiwork-workbuddy-pool",
        "version": 1,
        "exported_at": fs_utils::now_iso(),
        "include_credentials": include_cred,
        "accounts": accounts,
    }))
}

/// 账号库导入（F-46 扩展）：解析导出文件 → 逐账号按 uid 幂等入池
///（已存在（同 uid，可能换 token/异机 id）原位更新并保留原 id，不重复入池）+ 凭证回写 token store。
pub fn workbuddy_accounts_import(state: &AppState, payload: serde_json::Value) -> Result<serde_json::Value, String> {
    if payload.get("kind").and_then(|v| v.as_str()) != Some("aiwork-workbuddy-pool") {
        return Err("文件格式无法识别（缺少 aiwork-workbuddy-pool 标记）".into());
    }
    let accounts = payload
        .get("accounts")
        .and_then(|v| v.as_array())
        .ok_or("导出文件缺少 accounts 数组")?;
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut with_cred = 0usize;
    // 审查 P1：被拒绝条目逐条标注原因（id 非法/缺失的不入池，避免注入任意目录名）
    let mut rejected: Vec<serde_json::Value> = Vec::new();
    let mut pool = load_pool(state);
    for a in accounts {
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if id.is_empty() {
            rejected.push(serde_json::json!({ "id": id, "reason": "缺少 id" }));
            continue;
        }
        // id 安全校验（审查 P1）：字符集白名单，杜绝 `..`/绝对路径/分隔符注入
        if let Err(e) = fs_utils::ensure_uid_safe(&id) {
            rejected.push(serde_json::json!({ "id": id, "reason": e }));
            continue;
        }
        // id 规则校验（审查 P1）：须与池内生成规则一致——wb-<sha256 前 12 位十六进制小写>
        let hex = id.strip_prefix("wb-").unwrap_or("");
        let id_valid = hex.len() == 12 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !id_valid {
            rejected.push(serde_json::json!({ "id": id, "reason": "id 不符合 wb-<12位十六进制小写> 规则" }));
            continue;
        }
        // 按 uid 幂等（uid 优先，id 兜底，同 find_uid_or_id）：命中 → 原位更新并保留原 id
        // （换 token / 异机 id 的同账号不再产生重复条目，快照与分组引用不悬空）。
        // group_id/note/phone_masked 属本地/用户可编辑数据，更新时不覆盖，仅新增时采用。
        let entry_uid = a.get("uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        if let Some(x) = find_uid_or_id(&mut pool, &entry_uid, &id) {
            if !entry_uid.is_empty() {
                x.uid = entry_uid;
            }
            let nickname = a.get("nickname").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            if !nickname.is_empty() {
                x.nickname = nickname;
            }
            let edition = a.get("edition_type").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            if !edition.is_empty() {
                x.edition_type = edition;
            }
            if let Some(v) = a.get("access_token_expires_at").and_then(|v| v.as_i64()) {
                x.access_token_expires_at = Some(v);
            }
            if let Some(v) = a.get("refresh_token_expires_at").and_then(|v| v.as_i64()) {
                x.refresh_token_expires_at = Some(v);
            }
            x.auth_saved_at = Some(chrono::Utc::now().timestamp());
            updated += 1;
            // 凭证副本回写（导出时含凭证才有效）：写回保留的原 id 名下，与池条目对齐
            if let Some(cred) = a.get("credential").filter(|c| c.is_object()) {
                let rec = cred.clone();
                upsert_token_store(state, &x.id, &rec)?;
                with_cred += 1;
            }
            continue;
        }
        pool.accounts.push(WorkBuddyAccount {
            id: id.clone(),
            uid: entry_uid,
            nickname: a.get("nickname").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            phone_masked: a.get("phone_masked").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            edition_type: a.get("edition_type").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            access_token_expires_at: a.get("access_token_expires_at").and_then(|v| v.as_i64()),
            refresh_token_expires_at: a.get("refresh_token_expires_at").and_then(|v| v.as_i64()),
            group_id: a.get("group_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            note: a.get("note").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            auth_saved_at: Some(chrono::Utc::now().timestamp()),
            ..Default::default()
        });
        added += 1;
        // 凭证副本回写（导出时含凭证才有效）
        if let Some(cred) = a.get("credential").filter(|c| c.is_object()) {
            let rec = cred.clone();
            upsert_token_store(state, &id, &rec)?;
            with_cred += 1;
        }
    }
    save_pool(state, &pool)?;
    fs_utils::app_log(
        &state.data_dir,
        &format!("workbuddy: 账号库导入 新增 {added} / 更新 {updated} / 带凭证 {with_cred} / 拒绝 {}", rejected.len()),
    );
    // skipped 保留为旧版前端兼容字段：旧语义「同 id 跳过」已改为按 uid 原位合并（updated）
    Ok(serde_json::json!({ "added": added, "updated": updated, "skipped": 0, "with_credentials": with_cred, "rejected": rejected }))
}

#[cfg(test)]
mod tests {
    use super::{lazy_renew_skippable, merge_auth_entry, AuthMerge, WorkBuddyAccount, WbPool};

    fn entry(id: &str, uid: &str, nickname: &str) -> WorkBuddyAccount {
        WorkBuddyAccount {
            id: id.into(),
            uid: uid.into(),
            nickname: nickname.into(),
            ..Default::default()
        }
    }

    fn auth_merge(id: &str, uid: &str, exp: Option<i64>) -> AuthMerge {
        AuthMerge {
            id: id.into(),
            uid: uid.into(),
            nickname: "new-nick".into(),
            edition_type: "pro".into(),
            access_token_expires_at: exp,
            refresh_token_expires_at: None,
        }
    }

    /// 重复导入同 token：池大小不变、id 不变、字段以新值覆盖
    #[test]
    fn merge_same_token_keeps_pool_size_and_id() {
        let mut pool = WbPool { accounts: vec![entry("wb-aaaaaaaaaaaa", "u1", "old")] };
        let target_id = merge_auth_entry(&mut pool, auth_merge("wb-aaaaaaaaaaaa", "u1", Some(123)));
        assert_eq!(pool.accounts.len(), 1);
        assert_eq!(target_id, "wb-aaaaaaaaaaaa");
        assert_eq!(pool.accounts[0].id, "wb-aaaaaaaaaaaa");
        assert_eq!(pool.accounts[0].nickname, "new-nick");
        assert_eq!(pool.accounts[0].edition_type, "pro");
        assert_eq!(pool.accounts[0].access_token_expires_at, Some(123));
        assert!(!pool.accounts[0].needs_relogin);
    }

    /// 同 uid 不同 token（客户端换发 token）：池大小不变、凭证（过期时间）更新、id 保持旧值
    #[test]
    fn merge_same_uid_new_token_updates_and_keeps_old_id() {
        let mut pool = WbPool { accounts: vec![entry("wb-oldoldoldold", "u1", "old")] };
        let target_id = merge_auth_entry(&mut pool, auth_merge("wb-newnewnewnew", "u1", Some(456)));
        assert_eq!(pool.accounts.len(), 1);
        assert_eq!(pool.accounts[0].id, "wb-oldoldoldold"); // 保留旧 id：快照/分组引用不悬空
        assert_eq!(target_id, "wb-oldoldoldold"); // 凭证副本须写回旧 id 名下
        assert_eq!(pool.accounts[0].uid, "u1");
        assert_eq!(pool.accounts[0].access_token_expires_at, Some(456));
    }

    /// 新 uid（新账号）：新增条目，沿用传入 id
    #[test]
    fn merge_new_uid_appends_entry() {
        let mut pool = WbPool { accounts: vec![entry("wb-aaaaaaaaaaaa", "u1", "a")] };
        let target_id = merge_auth_entry(&mut pool, auth_merge("wb-bbbbbbbbbbbb", "u2", None));
        assert_eq!(pool.accounts.len(), 2);
        assert_eq!(target_id, "wb-bbbbbbbbbbbb");
        assert_eq!(pool.accounts[1].id, "wb-bbbbbbbbbbbb");
        assert_eq!(pool.accounts[1].uid, "u2");
        assert_eq!(pool.accounts[1].nickname, "new-nick");
    }

    /// 同 id 但 uid 漂移（池内旧 uid 与 auth 文件不一致）：按 id 兜底匹配，不产生第二条，
    /// 且 uid 以 auth 文件新值修正
    #[test]
    fn merge_same_id_drifted_uid_does_not_duplicate() {
        let mut pool = WbPool { accounts: vec![entry("wb-cccccccccccc", "u-old", "old")] };
        let target_id = merge_auth_entry(&mut pool, auth_merge("wb-cccccccccccc", "u-new", Some(789)));
        assert_eq!(pool.accounts.len(), 1);
        assert_eq!(target_id, "wb-cccccccccccc");
        assert_eq!(pool.accounts[0].uid, "u-new");
    }

    /// auth 文件缺 uid（uid 为空）：按 id 匹配原位更新，且不以空 uid 回填覆盖旧 uid
    #[test]
    fn merge_empty_uid_falls_back_to_id_and_keeps_old_uid() {
        let mut pool = WbPool { accounts: vec![entry("wb-dddddddddddd", "u1", "old")] };
        let target_id = merge_auth_entry(&mut pool, auth_merge("wb-dddddddddddd", "", None));
        assert_eq!(pool.accounts.len(), 1);
        assert_eq!(target_id, "wb-dddddddddddd");
        assert_eq!(pool.accounts[0].uid, "u1");
    }

    // ── 惰性续期门（lazy_renew_skippable）边界测试 ──────────────────────────
    // 语义：true = 可跳过续期（access_token 剩余 >24h）；false = 需要续期。
    // 与 Trae 的 lazy_refresh_needed 方向相反，注意断言极性。

    /// 基准时刻（固定 now，不依赖真实时钟）
    const WB_T0: i64 = 1_700_000_000;
    /// 阈值 24h（秒），须与 WB_LAZY_RENEW_MIN_SECS 一致
    const WB_H24: i64 = 24 * 3600;

    /// expires_at 缺失（旧版 auth 文件/字段解析失败）：无法判断时效 → 保守放行续期
    #[test]
    fn lazy_renew_none_expires_at_conservatively_renews() {
        assert!(!lazy_renew_skippable(None, WB_T0));
    }

    /// 剩余 25h：跳过 /access_token/refresh，减少一次上游取号
    #[test]
    fn lazy_renew_over_24h_skips() {
        assert!(lazy_renew_skippable(Some(WB_T0 + WB_H24 + 3600), WB_T0));
    }

    /// 剩余恰好 24h：严格大于（> 24h）→ 不算充足 → 续期
    #[test]
    fn lazy_renew_exactly_24h_boundary_renews() {
        assert!(!lazy_renew_skippable(Some(WB_T0 + WB_H24), WB_T0));
    }

    /// 剩余不足 24h：续期
    #[test]
    fn lazy_renew_under_24h_renews() {
        assert!(!lazy_renew_skippable(Some(WB_T0 + WB_H24 - 1), WB_T0));
    }

    /// 已过期（负剩余）：必须续期
    #[test]
    fn lazy_renew_expired_renews() {
        assert!(!lazy_renew_skippable(Some(WB_T0 - 3600), WB_T0));
    }
}
