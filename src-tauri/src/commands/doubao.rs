//! 豆包应用账号池（P2）：doubao_accounts.json + 快照槽合并视图 + 当前登录账号探测。
//! 方案依据 doubao-trae-switch-plan.md §2.1（目录级快照）/ §2.3（账号池：保存 = 快照，
//! 账号元数据入 doubao_accounts.json，结构对齐 checkin_accounts.json 理念）。
//!
//! ⚠ serde 命名约定：全部 snake_case，与前端 types.ts 严格对齐；
//!   严禁 rename_all = "camelCase"（曾导致导入预览弹框前端必崩）。
//! P3（会话续期）将在 DoubaoAccount 上追加 sessionid/sid_guard 等字段（serde default，向后兼容）。

use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::{copy_dir_recursive, AppState};

/// doubao_accounts.json 单条账号记录（P2 元数据 + P3 会话续期字段，均 serde default 向后兼容）
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct DoubaoAccount {
    /// 豆包 user_id（与快照槽目录名一致）
    pub user_id: String,
    /// 别名（展示名，默认 = user_id）
    #[serde(default)]
    pub name: String,
    /// 备注
    #[serde(default)]
    pub note: String,
    /// 入池时间（YYYY-MM-DD HH:MM:SS）
    #[serde(default)]
    pub added_at: String,
    /// 最近一次切换/保存登录态时间（展示用，PS 桥写 current_account.txt，这里由前端操作后回填）
    #[serde(default)]
    pub last_active_at: Option<String>,
    // ── P3 会话续期字段（由续期巡检 tasks/doubao_session 写回）──
    /// 明文 sessionid（凭证等同密码：仅存本地文件，前端全程掩码展示）
    #[serde(default)]
    pub session_id: Option<String>,
    /// sid_guard 原文（'sid|create_ts|duration|...'，滑动续期载体）
    #[serde(default)]
    pub sid_guard: Option<String>,
    /// 会话到期时间（由 sid_guard 解析）
    #[serde(default)]
    pub session_expire_at: Option<String>,
    /// 巡检判定：true=过期 / false=有效 / None=未知
    #[serde(default)]
    pub expired: Option<bool>,
    /// 最近一次 cookie 解密同步时间
    #[serde(default)]
    pub cookies_synced_at: Option<String>,
    /// 最近一次续期探活时间
    #[serde(default)]
    pub last_renew_at: Option<String>,
    /// 会话来源：live=当前 User Data / snapshot=快照槽解密
    #[serde(default)]
    pub session_source: Option<String>,
    /// ttwid 设备 Cookie（对话历史 API 登录校验必需；代理抓包或手动录入）
    #[serde(default)]
    pub ttwid: Option<String>,
    // ── P4 会员额度缓存（doubao_quota_fetch 成功后回写，供列表徽标/悬停提示展示）──
    /// 会员等级（None = 免费或未识别）
    #[serde(default)]
    pub quota_level: Option<String>,
    /// 会员到期时间
    #[serde(default)]
    pub quota_expire_at: Option<String>,
    /// 额度状态一句话（如 "图片 80/100 · 视频 3/10"）
    #[serde(default)]
    pub quota_summary: Option<String>,
    /// 最近一次额度查询时间（Some = 已查询过，据此展示免费/会员标识）
    #[serde(default)]
    pub quota_checked_at: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub(crate) struct DoubaoAccountPool {
    #[serde(default)]
    pub(crate) accounts: Vec<DoubaoAccount>,
    /// 最近一次 KeepAlive 保活时间（池级：保活由豆包客户端对当前登录会话统一滑动续期）
    #[serde(default)]
    pub last_keepalive_at: Option<String>,
}

/// 前端合并视图：快照槽（profiles_doubao/<uid>/）+ 账号池别名 + 当前账号标记
#[derive(Serialize, Clone)]
pub struct DoubaoAccountView {
    pub user_id: String,
    pub name: String,
    pub note: String,
    pub has_snapshot: bool,
    pub size_bytes: u64,
    pub file_count: u64,
    pub last_modified: String,
    pub is_current: bool,
    pub added_at: Option<String>,
    // ── P3 会话状态 ──
    /// ok=有效 / expired=已过期 / unknown=未探活 / none=无 sessionid
    pub session_state: String,
    /// 明文 sessionid（本地应用，编辑弹框回填用）
    pub session_id: Option<String>,
    /// sid_guard 原文（编辑弹框回填用）
    pub sid_guard: Option<String>,
    pub session_expire_at: Option<String>,
    pub cookies_synced_at: Option<String>,
    pub last_renew_at: Option<String>,
    pub session_source: Option<String>,
    pub ttwid: Option<String>,
    // ── P4 会员额度缓存 ──
    pub quota_level: Option<String>,
    pub quota_expire_at: Option<String>,
    pub quota_summary: Option<String>,
    pub quota_checked_at: Option<String>,
    /// 池级：最近一次 KeepAlive 保活时间（所有行同值，供前端提醒判断）
    pub last_keepalive_at: Option<String>,
}

fn session_state_of(acc: &DoubaoAccount) -> String {
    match (&acc.session_id, acc.expired) {
        (None, _) => "none".to_string(),
        (Some(_), Some(true)) => "expired".to_string(),
        (Some(_), Some(false)) => "ok".to_string(),
        (Some(_), None) => "unknown".to_string(),
    }
}

fn profiles_root(state: &State<AppState>) -> PathBuf {
    state.data_dir.join("data").join("profiles_doubao")
}

/// 读取账号池；文件不存在/损坏时返回空池（损坏仅记日志，避免一个坏文件锁死整页）
/// 读取账号池（SQLite 化 P3：doubao_accounts 表；tasks/* 共用统一入口）
pub(crate) fn load_pool(state: &AppState) -> DoubaoAccountPool {
    crate::store::docs::doubao_pool_load(&crate::store::db(&state.data_dir))
}

/// 保存账号池（SQLite 化 P3：doubao_accounts 表整表替换）
pub(crate) fn save_pool(state: &AppState, pool: &DoubaoAccountPool) -> Result<(), String> {
    crate::store::docs::doubao_pool_save(&crate::store::db(&state.data_dir), pool)
}

/// 读取当前账号（PS 桥 Set-CurrentAccount 写 profiles_doubao/current_account.txt，UTF8 带 BOM）
fn read_current_uid(state: &State<AppState>) -> Option<String> {
    let path = profiles_root(state).join("current_account.txt");
    let raw = std::fs::read_to_string(path).ok()?;
    let uid = raw.trim().trim_start_matches('\u{feff}').trim().to_string();
    if uid.is_empty() { None } else { Some(uid) }
}

/// 豆包账号合并视图：账号池 ∪ 快照槽目录，标注当前账号。
/// 账号池可含无快照的账号（如手动收录待登录）；快照槽也可含未入池账号（如 PS 桥自动备份的 last）。
// async：列表构建含快照目录遍历/目录大小统计，避免 UI 冻结（内部逻辑不变）
#[tauri::command(async)]
pub fn doubao_accounts_list(state: State<AppState>) -> Result<Vec<DoubaoAccountView>, String> {
    let pool = load_pool(&state);
    let current = read_current_uid(&state);
    let root = profiles_root(&state);
    let keepalive_at = pool.last_keepalive_at.clone();

    // 以账号池为基底
    let mut views: Vec<DoubaoAccountView> = pool
        .accounts
        .iter()
        .map(|a| DoubaoAccountView {
            user_id: a.user_id.clone(),
            name: if a.name.is_empty() { a.user_id.clone() } else { a.name.clone() },
            note: a.note.clone(),
            has_snapshot: false,
            size_bytes: 0,
            file_count: 0,
            last_modified: String::new(),
            is_current: current.as_deref() == Some(a.user_id.as_str()),
            added_at: if a.added_at.is_empty() { None } else { Some(a.added_at.clone()) },
            session_state: session_state_of(a),
            // 审查 P1：凭证等同密码，列表全程掩码展示；完整值走 doubao_account_get_credential 按需获取
            session_id: a.session_id.as_deref().map(fs_utils::mask_secret),
            sid_guard: a.sid_guard.as_deref().map(fs_utils::mask_secret),
            session_expire_at: a.session_expire_at.clone(),
            cookies_synced_at: a.cookies_synced_at.clone(),
            last_renew_at: a.last_renew_at.clone(),
            session_source: a.session_source.clone(),
            ttwid: a.ttwid.as_deref().map(fs_utils::mask_secret),
            quota_level: a.quota_level.clone(),
            quota_expire_at: a.quota_expire_at.clone(),
            quota_summary: a.quota_summary.clone(),
            quota_checked_at: a.quota_checked_at.clone(),
            last_keepalive_at: keepalive_at.clone(),
        })
        .collect();

    // 并入快照槽（last 为 PS 桥安全备份槽，不在账号列表展示）
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let slot = entry.file_name().to_string_lossy().to_string();
            // last = PS 桥安全备份槽；*.bak = 单代回滚代次（Backup-ChromiumProfile 覆盖前的
            // 旧快照）——两者都是内部回滚数据而非账号，不进账号列表（否则用户会看到一堆
            // "xxx.bak 账号"误以为多了账号）
            if slot == "last" || slot.ends_with(".bak") {
                continue;
            }
            let (size_bytes, file_count) = crate::commands::profile::dir_stats(&entry.path());
            let last_modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| {
                    let dt = chrono::DateTime::<chrono::Local>::from(std::time::SystemTime::UNIX_EPOCH + d);
                    dt.format("%Y-%m-%d %H:%M:%S").to_string()
                })
                .unwrap_or_default();
            if let Some(v) = views.iter_mut().find(|v| v.user_id == slot) {
                v.has_snapshot = true;
                v.size_bytes = size_bytes;
                v.file_count = file_count;
                v.last_modified = last_modified;
            } else {
                views.push(DoubaoAccountView {
                    user_id: slot.clone(),
                    name: slot,
                    note: String::new(),
                    has_snapshot: true,
                    size_bytes,
                    file_count,
                    last_modified,
                    is_current: current.as_deref() == Some(entry.file_name().to_string_lossy().as_ref()),
                    added_at: None,
                    session_state: "none".to_string(),
                    session_id: None,
                    sid_guard: None,
                    session_expire_at: None,
                    cookies_synced_at: None,
                    last_renew_at: None,
                    session_source: None,
                    ttwid: None,
                    quota_level: None,
                    quota_expire_at: None,
                    quota_summary: None,
                    quota_checked_at: None,
                    last_keepalive_at: keepalive_at.clone(),
                });
            }
        }
    }

    // 排序：当前账号 > 有快照 > 无快照；同级按最近活动倒序
    views.sort_by(|a, b| {
        b.is_current
            .cmp(&a.is_current)
            .then(b.has_snapshot.cmp(&a.has_snapshot))
            .then(b.last_modified.cmp(&a.last_modified))
    });
    Ok(views)
}

/// 收录/更新账号（upsert 别名与备注；不存在则入池）
#[tauri::command]
pub fn doubao_account_save(
    state: State<AppState>,
    user_id: String,
    name: Option<String>,
    note: Option<String>,
) -> Result<(), String> {
    let user_id = user_id.trim().to_string();
    if user_id.is_empty() {
        return Err("user_id 不能为空".to_string());
    }
    // uid 后续会被拼进快照槽/备份等文件路径，入口统一做字符集白名单校验
    fs_utils::ensure_uid_safe(&user_id)?;
    let mut pool = load_pool(&state);
    if let Some(acc) = pool.accounts.iter_mut().find(|a| a.user_id == user_id) {
        if let Some(n) = name {
            acc.name = n.trim().to_string();
        }
        if let Some(n) = note {
            acc.note = n.trim().to_string();
        }
    } else {
        pool.accounts.push(DoubaoAccount {
            name: name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| user_id.clone()),
            note: note.map(|n| n.trim().to_string()).unwrap_or_default(),
            user_id,
            added_at: fs_utils::now_ts(),
            last_active_at: None,
            session_id: None,
            sid_guard: None,
            session_expire_at: None,
            expired: None,
            cookies_synced_at: None,
            last_renew_at: None,
            session_source: None,
            ttwid: None,
            quota_level: None,
            quota_expire_at: None,
            quota_summary: None,
            quota_checked_at: None,
        });
    }
    save_pool(&state, &pool)
}

/// 从账号池移除（不动快照目录；快照删除走 profile_delete）
#[tauri::command]
pub fn doubao_account_remove(state: State<AppState>, user_id: String) -> Result<(), String> {
    // uid 是快照槽/备份目录名的唯一键，入口先做防路径注入校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let mut pool = load_pool(&state);
    let before = pool.accounts.len();
    pool.accounts.retain(|a| a.user_id != user_id);
    if pool.accounts.len() == before {
        return Ok(()); // 不存在视为已移除
    }
    save_pool(&state, &pool)
}

/// 探测豆包当前登录账号。
/// 来源⓪（主）：Local Storage leveldb 的 client_device_info.userId（客户端每次启动自写，
///   不依赖代理；tasks/doubao_chats::detect_uid 解析，内部与抓包文件按时间戳比新鲜度）。
///   注意：同 profile 重新登录后 Local State 的 saman.user_id **不更新**（实测 2026-09-09），
///   抓包文件在代理未开时也不更新——两者都只能作兜底。
/// 来源①（兜底）：抓包凭证文件 uid（multi_sids）→ `%LOCALAPPDATA%\Doubao\User Data\Local State`
///   → `profile.info_cache[*].saman`（取最近活跃 Profile）。
/// 来源②（兜底）：`%APPDATA%\Doubao\public_config.json` 全树递归搜 user_id/uid（text_picker）。
/// 来源③（最后兜底）：profiles_doubao/current_account.txt（PS 桥保存/切换后写入）。
// async：探测链含 leveldb/文件遍历，避免 UI 冻结（内部逻辑不变）
#[tauri::command(async)]
pub fn doubao_detect_uid(state: State<AppState>) -> Result<Option<String>, String> {
    // 首选：豆包客户端 Local Storage 的 client_device_info.userId（客户端每次启动自写，
    // **不依赖代理**；与抓包文件按时间戳比新鲜度取新者）——修复无代理时重新登录识别不到
    // 新账号的问题（Local State 的 saman.user_id 同 profile 重登不更新、抓包文件无代理不更新）
    if let Some(uid) = detect_uid_from_local_storage(&state) {
        return Ok(Some(uid));
    }
    // 兜底链：抓包 uid → Local State → public_config → current_account.txt
    if let Some(uid) = read_captured_uid(&state) {
        return Ok(Some(uid));
    }
    if let Ok(Some(uid)) = detect_uid_from_local_state() {
        return Ok(Some(uid));
    }
    if let Ok(Some(uid)) = detect_uid_from_public_config() {
        return Ok(Some(uid));
    }
    Ok(read_current_uid(&state))
}

/// 检测某 User Data 目录（Live 或快照槽）的登录会话综合判定（Issue #15 修复）。
/// 原实现只看 Local State `profile.last_used` 指向的活跃 Profile，存在两条误报路径：
/// ① 豆包多 Profile 下登录会话可能不在活跃 Profile（localStorage 的 uid 检测链扫全部
///    Profile，而 Cookie 检查只看活跃 → 弹窗预填了账号但预检误报「未检测到登录会话」）；
/// ② 客户端运行中 Cookies 被独占锁导致活跃 Profile 复制失败，「读不到」被当成「未登录」。
/// 统一口径：任一 Profile 持有非游客登录会话即算已登录（快照含全部 Profile，会话随快照
/// 走，恢复后不会丢）；存在读取失败 → 全貌不可知 → Unavailable 由调用方 fail-open。
#[derive(Debug, PartialEq)]
enum LoginSessionAnalysis {
    /// 存在非游客登录会话（remaining=None 视为最优，否则取剩余最长者所在的 Profile）
    LoggedIn,
    /// 所有 Profile 均读取成功，但仅存在游客/临时会话
    GuestOnly { remaining: i64 },
    /// 所有 Profile 均读取成功且均无 sessionid/sid_guard → 确认未登录（带诊断摘要）
    NoSession { summary: String },
    /// 检测不可用（无 Profile/存在读取失败等，无法确认全貌）→ 调用方 fail-open
    Unavailable,
}

fn analyze_login_sessions(user_data: &std::path::Path) -> LoginSessionAnalysis {
    let v = crate::tasks::doubao_chats::check_login_cookie(user_data);
    let Some(profiles) = v.get("profiles").and_then(serde_json::Value::as_array) else {
        return LoginSessionAnalysis::Unavailable;
    };
    if profiles.is_empty() {
        return LoginSessionAnalysis::Unavailable;
    }
    let active = v
        .get("active_profile")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let mut logged: Option<(Option<i64>, &str)> = None; // 非游客会话：None 剩余优先，其次取最大
    let mut guest: Option<i64> = None;
    let mut any_err = false;
    let mut total_cookies = 0i64;
    for p in profiles {
        let name = p.get("name").and_then(serde_json::Value::as_str).unwrap_or("?");
        let err = p
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        total_cookies += p
            .get("doubao_cookies")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if !err.is_empty() {
            any_err = true; // 锁占用/无 Cookies 库等：该 Profile 会话状态未知
            continue;
        }
        if !p
            .get("has_session")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let remaining = p
            .get("sessionid_remaining_sec")
            .and_then(serde_json::Value::as_i64);
        match remaining {
            Some(sec) if is_guest_session(Some(sec)) => {
                guest = Some(guest.map_or(sec, |g| g.max(sec)));
            }
            other => {
                let better = match logged {
                    None => true,
                    Some((Some(_), _)) => other.is_none(),
                    Some((None, _)) => false,
                };
                if better {
                    logged = Some((other, name));
                }
            }
        }
    }
    if logged.is_some() {
        return LoginSessionAnalysis::LoggedIn;
    }
    // 无非游客会话且有读取失败 → 可能真登录被锁挡住，不可断言「未登录」
    if any_err {
        return LoginSessionAnalysis::Unavailable;
    }
    if let Some(sec) = guest {
        return LoginSessionAnalysis::GuestOnly { remaining: sec };
    }
    LoginSessionAnalysis::NoSession {
        summary: format!(
            "活跃 Profile {}，共扫描 {} 个 Profile / {} 个 doubao.com Cookie，全部读取成功但均无 sessionid/sid_guard",
            if active.is_empty() { "未知" } else { active },
            profiles.len(),
            total_cookies,
        ),
    }
}

/// 豆包客户端 User Data 目录（Live 态，登录 Cookie/uid 检测用）。
/// 基根：Windows=%LOCALAPPDATA%，mac=Application Support（豆包桌面端 Chromium 布局）
fn doubao_live_user_data_dir() -> Option<PathBuf> {
    let dir = crate::platform::local_data_root()
        .ok()?
        .join("Doubao")
        .join("User Data");
    dir.exists().then_some(dir)
}

/// 切换守卫的严格版：检测当前登录 uid，且**必须**在 Live User Data 的 Cookies 里验证到
/// 登录会话（sessionid/sid_guard，任一 Profile）才返回该 uid；无登录会话返回空串。
/// 背景（实测 2026-09-09 16:5x）：客户端恢复某账号快照后未登录，uid 检测链被快照自带的
/// localStorage 残留（client_device_info.userId=旧账号）骗过 → 下次切换时守卫误通过，
/// 把"未登录态"又备份进该账号槽，反复污染。Cookie 存在性无法被残留数据伪造。
pub(crate) fn detect_guard_uid_strict(state: &State<AppState>) -> String {
    let Some(uid) = detect_uid_from_local_storage(state) else {
        return String::new();
    };
    let Some(dir) = doubao_live_user_data_dir() else {
        return String::new();
    };
    match analyze_login_sessions(&dir) {
        // 有真实登录会话（非游客态）才放行回写；游客/确认无会话/检测不可用一律不回写
        //（宁可不备份，不可覆盖错；Unavailable 保守拦截维持原 fail-closed 语义）
        LoginSessionAnalysis::LoggedIn => uid,
        _ => String::new(),
    }
}

/// 退出登录后豆包客户端会残留 **6 小时有效期**的匿名(游客) sessionid，正常登录会话为
/// 30 天。sessionid 剩余有效期低于 12 小时即判为游客/临时会话——仅看"有没有 sessionid"
/// 会把它误认成登录态（实测把游客态存进了 908 槽）。
const GUEST_SESSION_MAX_REMAINING_SEC: i64 = 12 * 3600;

fn is_guest_session(remaining_sec: Option<i64>) -> bool {
    match remaining_sec {
        // 检测不到有效期信息（旧版脚本）不做判定，保持原行为
        None => false,
        Some(sec) => sec < GUEST_SESSION_MAX_REMAINING_SEC,
    }
}

/// 探测通用层：对任意 User Data 目录（Live 或快照槽位）做两段式会话探活
///（原 doubao_renew.py --probe-user-data，Rust 实现在 tasks/doubao_session::probe_slot_session）。
/// 返回最后一行摘要的 status（expired / ok / unknown / error / 空）。
/// 注意：含网络请求，须在 #[tauri::command(async)] 标记的命令中调用，防 UI 冻结。
fn probe_user_data_session_raw(user_data: &std::path::Path, user_id: &str) -> String {
    crate::tasks::doubao_session::probe_slot_session(
        user_data,
        user_id,
        crate::tasks::doubao_session::DEFAULT_PROBE_URL,
    )
    .get("status")
    .and_then(serde_json::Value::as_str)
    .unwrap_or("")
    .to_string()
}

/// 切换/一键打开前预检（仅豆包）：目标槽位存储的会话凭证在服务端是否仍有效。
/// 背景（2026-09-09 实测日志）：在豆包客户端内「退出登录」会向 passport/web/logout 申请吊销
/// 该账号的服务端会话——快照文件全部完好，但里面的会话已被服务端判死（api_probe code=
/// 710012001）；恢复后客户端启动一联网即收到 SESSION_EXPIRED 强制登出，表现为「切换成功但
/// 豆包未登录」，且只有被登出过的账号中招（未被登出的账号一切正常，即实测的不对称现象）。
/// 判 expired 时中止切换并给出补救指引；探测不可用/无法验证 fail-open 不阻断。
pub(crate) fn probe_slot_session_alive(
    data_dir: &std::path::Path,
    user_id: &str,
) -> Result<(), String> {
    let profiles = data_dir.join("data").join("profiles_doubao");
    let main_slot = profiles.join(user_id);
    // 与桥的回退逻辑对齐：主槽缺失时桥会回退 .bak，探测跟随
    let slot = if main_slot.exists() {
        main_slot
    } else {
        let bak = profiles.join(format!("{user_id}.bak"));
        if bak.exists() { bak } else { return Ok(()); } // 无快照 → 交给桥报「无快照」
    };
    if probe_user_data_session_raw(&slot, user_id) == "expired" {
        Err(format!(
            "账号 {user_id} 快照里的登录会话已在服务端失效（常见原因：曾在豆包客户端内对该账号\
             退出登录——客户端会调 passport 接口吊销该会话，此前保存的快照随之中毒）。\
             请在豆包中重新登录该账号，再「保存当前登录态」覆盖保存，之后即可正常切换"
        ))
    } else {
        // ok=有效；unknown/error/空=无法验证（网络故障等）fail-open 不阻断
        Ok(())
    }
}

/// 保存前预检（仅豆包）：Live 客户端当前会话在服务端是否仍有效。
/// 只验证本地 Cookie 存在性不够——会话可能早已被服务端吊销（客户端内退出过/被顶替），
/// 此时「保存当前登录态」存进去的就是死会话，之后每次切换该账号都未登录（实测 A 槽事故）。
/// 判 expired 时拒绝保存并给出补救指引；探测不可用/无法验证 fail-open 不阻断。
pub(crate) fn probe_live_session_alive(user_id: &str) -> Result<(), String> {
    let Some(dir) = doubao_live_user_data_dir() else {
        return Ok(()); // Live 目录不存在由 ensure_live_has_login_session 负责报错
    };
    if probe_user_data_session_raw(&dir, user_id) == "expired" {
        Err(format!(
            "当前豆包客户端的登录会话已在服务端失效（常见原因：曾在客户端内退出登录该账号，\
             或该会话被新登录顶替）——现在保存只会把死会话存进账号 {user_id} 的快照。\
             请先在豆包中重新登录，再保存当前登录态"
        ))
    } else {
        Ok(())
    }
}

/// 保存前预检：Live User Data 必须持有登录会话 Cookie，否则禁止「保存当前登录态」。
/// 返回 Err(原因) = 确认无登录会话/游客态；Ok(()) = 放行（含检测不可用 fail-open）。
pub(crate) fn ensure_live_has_login_session() -> Result<(), String> {
    let Some(dir) = doubao_live_user_data_dir() else {
        return Err("未找到豆包客户端数据目录（%LOCALAPPDATA%\\Doubao\\User Data），请先安装并登录豆包".to_string());
    };
    match analyze_login_sessions(&dir) {
        LoginSessionAnalysis::LoggedIn => Ok(()),
        // 有 sessionid 但剩余有效期 <12h = 退出登录后残留的 6h 游客会话，不是真登录
        LoginSessionAnalysis::GuestOnly { remaining } => Err(format!(
            "当前豆包客户端是游客/未登录状态（sessionid 剩余有效期仅约 {} 小时，\
             为退出登录后残留的临时会话），不能保存为账号登录态。请先在豆包中真正登录账号",
            remaining.max(0) / 3600
        )),
        LoginSessionAnalysis::NoSession { summary } => Err(format!(
            "当前豆包客户端未检测到登录会话（{summary}）——请先在豆包中登录账号再保存，\
             否则会把未登录状态存进账号槽（这正是此前账号快照反复被污染的原因）"
        )),
        // 检测不可用（Cookies 被客户端运行占用等）不阻断，保持旧行为
        LoginSessionAnalysis::Unavailable => Ok(()),
    }
}

/// 读客户端 Local Storage leveldb 的 client_device_info.userId（Rust 实现，原 doubao_chats.py --detect-uid）。
/// 内部已与抓包文件按时间戳比新鲜度（代理开着取抓包、没开取客户端本地记录）。
/// 检测失败/无结果返回 None（不阻断后续兜底链）。
fn detect_uid_from_local_storage(state: &State<AppState>) -> Option<String> {
    let v = crate::tasks::doubao_chats::detect_uid(&state);
    let uid = v.get("user_id")?.as_str()?.trim().to_string();
    (!uid.is_empty()).then_some(uid)
}

/// 从代理抓包凭证读 uid（代理 MITM 层解析 multi_sids 得到；None = 未抓到/旧格式无此字段）。
/// SQLite 化（P3）：kv `doubao_captured_credentials`。
fn read_captured_uid(state: &State<AppState>) -> Option<String> {
    let v: serde_json::Value = crate::store::db(&state.data_dir).kv_get("doubao_captured_credentials");
    let uid = v.get("uid")?.as_str()?.trim().to_string();
    (!uid.is_empty()).then_some(uid)
}

/// 来源①：从 User Data/Local State 的 profile.info_cache 取最近活跃 Profile 的 saman.user_id。
/// 文件缺失/无 saman 块/解析失败返回 Ok(None)。
fn detect_uid_from_local_state() -> Result<Option<String>, String> {
    let base = crate::platform::local_data_root()
        .map_err(|e| format!("读取应用数据根目录失败: {e}"))?;
    let path = base
        .join("Doubao")
        .join("User Data")
        .join("Local State");
    if !path.exists() {
        return Ok(None);
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Local State 读取失败（忽略）: {e}");
            return Ok(None);
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Local State 解析失败（忽略）: {e}");
            return Ok(None);
        }
    };
    Ok(pick_uid_from_info_cache(&v))
}

/// 从 Local State JSON 提取最近活跃 Profile 的 saman.user_id（纯函数，便于测试）。
/// 优先 `profile.last_active_profiles` 的最后一个目录名，否则取 active_time 最大的含 saman Profile。
fn pick_uid_from_info_cache(v: &serde_json::Value) -> Option<String> {
    let profile = v.get("profile")?;
    let info_cache = profile.get("info_cache")?.as_object()?;

    // 路径 A：last_active_profiles（数组 of Profile 目录名），取最后一个可解析者
    if let Some(last_active) = profile
        .get("last_active_profiles")
        .and_then(|a| a.as_array())
        .filter(|a| !a.is_empty())
    {
        for dir in last_active.iter().rev() {
            if let Some(dir) = dir.as_str() {
                if let Some(uid) = info_cache
                    .get(dir)
                    .and_then(|e| e.get("saman"))
                    .and_then(|s| s.get("user_id"))
                    .and_then(|u| u.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                {
                    return Some(uid);
                }
            }
        }
    }

    // 路径 B：active_time 最大者（缺失视为 0）
    let mut best: Option<(f64, String)> = None;
    for entry in info_cache.values() {
        let uid = entry
            .get("saman")
            .and_then(|s| s.get("user_id"))
            .and_then(|u| u.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));
        let Some(uid) = uid else { continue };
        let active = entry
            .get("active_time")
            .and_then(|t| t.as_f64())
            .unwrap_or(0.0);
        if best.as_ref().map(|(t, _)| active > *t).unwrap_or(true) {
            best = Some((active, uid));
        }
    }
    best.map(|(_, uid)| uid)
}

/// 从 public_config.json 全树递归收集 uid 候选并择优。文件缺失/解析失败返回 Ok(None)。
fn detect_uid_from_public_config() -> Result<Option<String>, String> {
    let base = crate::platform::app_support_root()
        .map_err(|e| format!("读取应用数据根目录失败: {e}"))?;
    let path = base.join("Doubao").join("public_config.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取 public_config.json 失败: {e}"))?;
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            // 坏文件不该锁死保存流程：记日志后视为未识别（走 current_account.txt 兜底）
            eprintln!("public_config.json 解析失败（忽略）: {e}");
            return Ok(None);
        }
    };
    let mut candidates: Vec<UidCandidate> = Vec::new();
    collect_uid_candidates(&v, false, &mut candidates);
    candidates.sort_by(|a, b| {
        b.from_current_user
            .cmp(&a.from_current_user)
            .then(b.numeric.cmp(&a.numeric))
            .then(b.action_time.cmp(&a.action_time))
    });
    Ok(candidates.into_iter().next().map(|c| c.user_id))
}

#[derive(Debug)]
struct UidCandidate {
    user_id: String,
    /// 值是否为纯数字（user_id 实测为数字串；用于过滤误命中）
    numeric: bool,
    /// 祖先链是否含 current_user 键
    from_current_user: bool,
    /// 关联的 user_action_time（毫秒时间戳，越大越新）
    action_time: Option<i64>,
}

const UID_KEYS: [&str; 3] = ["user_id", "uid", "user_id_str"];
const UID_MAX_DEPTH: usize = 12;

fn collect_uid_candidates(v: &serde_json::Value, in_current_user: bool, out: &mut Vec<UidCandidate>) {
    collect_uid_candidates_impl(v, in_current_user, 0, out);
}

fn collect_uid_candidates_impl(
    v: &serde_json::Value,
    in_current_user: bool,
    depth: usize,
    out: &mut Vec<UidCandidate>,
) {
    if depth > UID_MAX_DEPTH {
        return;
    }
    match v {
        serde_json::Value::Object(map) => {
            // 本对象若同时含 uid 键与 user_action_time，则合并为一条候选
            let uid_val = UID_KEYS.iter().find_map(|k| map.get(*k));
            if let Some(uid_val) = uid_val {
                if let Some(uid) = value_to_uid(uid_val) {
                    let action_time = map
                        .get("user_action_time")
                        .and_then(|t| t.as_i64().or_else(|| t.as_str().and_then(|s| s.parse().ok())));
                    let numeric = matches!(uid_val, serde_json::Value::Number(_))
                        || uid.chars().all(|c| c.is_ascii_digit());
                    if !out.iter().any(|c| c.user_id == uid) {
                        out.push(UidCandidate { user_id: uid, numeric, from_current_user: in_current_user, action_time });
                    }
                }
            }
            for (k, child) in map {
                if UID_KEYS.contains(&k.as_str()) {
                    continue; // 已在对象层取值，不把 uid 值当子树再搜
                }
                // 仅当下降进入 current_user 对象本身时才置位（不能污染兄弟子树，
                // 如 text_picker.current_user 与 text_picker.users 是平级语义）
                let child_in_current = in_current_user || k == "current_user";
                collect_uid_candidates_impl(child, child_in_current, depth + 1, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_uid_candidates_impl(item, in_current_user, depth + 1, out);
            }
        }
        _ => {}
    }
}

/// uid 值规整：字符串去空白 / 数字转字符串；空值或超长视为无效。
fn value_to_uid(v: &serde_json::Value) -> Option<String> {
    let s = match v {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if s.is_empty() || s.len() > 32 {
        return None;
    }
    Some(s)
}

// ── P3 会话续期 ──────────────────────────────────────────────────────────────
//
// 实测结论（2026-09-08，本机 Doubao Chromium 147）：
// 桌面客户端的 cookie 值（sessionid/sid_guard 等）在 Chromium os_crypt 之下还有一层客户端级
// 加密——v10/DPAPI + AES-GCM 解出的明文仍为二进制密文（GCM tag 验证通过），无法离线得到
// 明文 sessionid。因此续期主路径为 KeepAlive（让豆包客户端自己联网滑动续期 sid_guard），
// 探活巡检对池内明文 sessionid 生效（手动录入 manual 或代理抓包 proxy 来源均可，两者同为明文）。

/// 运行 KeepAlive：启动豆包 → 等待会话联网刷新（8s）→ 优雅关闭（运行中则跳过）。
/// NDJSON 进度走 keepalive-progress / keepalive-done 事件，完成后记录池级保活时间戳。
/// （原经 PS 桥子进程，switcher Rust 化后进程内直调，终态由 run_action 返回值承载）
#[tauri::command]
pub fn doubao_keepalive_run(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let args = crate::switcher::RunArgs {
        action: crate::switcher::Action::KeepAlive,
        target_app: crate::switcher::TargetApp::Doubao,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        data_dir: state.data_dir.clone(),
    };
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    let st2 = state.inner().clone();
    std::thread::spawn(move || {
        let sink = crate::switcher::TauriSink::new(&app2, "keepalive-progress", &data_dir);
        let result = crate::switcher::run_action(args, &sink);
        let (success, raw) = match &result {
            Ok(line) => (true, line.clone()),
            Err(line) => (false, line.clone()),
        };
        let _ = app2.emit("keepalive-done", serde_json::json!({ "success": success, "raw": raw }));
        if success {
            // 记录池级保活时间戳 + 运维历史（写入失败不影响保活结果；SQLite 化 P3 经 store）
            let mut pool = load_pool(&st2);
            pool.last_keepalive_at = Some(fs_utils::now_ts());
            let _ = save_pool(&st2, &pool);
            append_history_event(
                &data_dir,
                serde_json::json!({
                    "ts": fs_utils::now_ts(), "kind": "keepalive", "ok": true,
                    "summary": "KeepAlive 保活完成", "source": "app",
                }),
            );
        }
    });
    Ok(())
}

// ── P4 会员额度 ──────────────────────────────────────────────────────────────
//
// 端点现状：豆包会员额度接口为 www.doubao.com 已登录 XHR，社区无公开文档，
// 须用户抓包（内置代理）固化后填入 settings.doubao_quota_url。
// 本命令为框架：凭证（池内明文 sessionid，manual/proxy 来源均可）+ 可配置端点 + 宽容解析，
// 端点就绪后前端即可展示会员等级 / 到期时间 / 剩余额度条。

/// 查询账号会员额度（Rust 直调 tasks::doubao_quota，网络请求可达数秒，async 派发避免阻塞 UI）
#[tauri::command(async)]
pub fn doubao_quota_fetch(state: State<AppState>, user_id: String) -> Result<serde_json::Value, String> {
    // uid 作为额度缓存/运维历史的账号键，入口先做防注入校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let url = state
        .settings()
        .doubao_quota_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .ok_or("会员额度接口未配置：请先在豆包「环境配置」填入抓包固化的额度接口地址")?;
    if !url.starts_with("http") {
        return Err(format!("额度接口地址无效: {url}（需以 http(s):// 开头）"));
    }
    let summary = crate::tasks::doubao_quota::fetch_single(&state, user_id.trim(), &url)?;
    // 成功后把解析结果回写账号池（额度缓存），供列表徽标/悬停提示展示
    if let Some(parsed) = summary.get("parsed") {
        match update_quota_cache(&state, user_id.trim(), parsed) {
            Ok(quote_summary) => {
                append_history(
                    &state,
                    serde_json::json!({
                        "ts": fs_utils::now_ts(),
                        "kind": "quota",
                        "uid": user_id.trim(),
                        "ok": true,
                        "level": parsed.get("level"),
                        "summary": quote_summary,
                        "windows": windows_of_parsed(parsed),
                        "source": "app",
                    }),
                );
            }
            Err(e) => {
                fs_utils::app_log(&state.data_dir, &format!("额度缓存回写失败: {e}"));
            }
        }
    }
    Ok(summary)
}

/// 把额度查询解析结果回写账号池（quota_level/expire/summary/checked_at）。
/// level 为空 = 免费或未识别，仍记录 checked_at，前端据此显示「免费」标识。
/// 成功时返回一句话摘要（供运维历史事件复用）。
fn update_quota_cache(state: &State<AppState>, uid: &str, parsed: &serde_json::Value) -> Result<Option<String>, String> {
    let mut pool = load_pool(state);
    let Some(acc) = pool.accounts.iter_mut().find(|a| a.user_id == uid) else {
        // 未入池账号（仅快照）不缓存额度；入池后首次查询即可
        return Ok(None);
    };
    let level = parsed.get("level").and_then(|l| match l {
        serde_json::Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    let expire = parsed
        .get("expire_at")
        .and_then(|e| e.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // 额度条目 → 一句话摘要（展示口径统一归口 tasks::doubao_quota::summarize_parsed，
    // 与 CLI 批量巡检 run_batch 完全同构）
    let summary = crate::tasks::doubao_quota::summarize_parsed(parsed);
    acc.quota_level = level;
    acc.quota_expire_at = expire;
    acc.quota_summary = summary.clone();
    acc.quota_checked_at = Some(fs_utils::now_ts());
    save_pool(state, &pool)?;
    Ok(summary)
}

// ── 运维历史（B2 健康度 / A2 额度趋势的数据源）───────────────────────────────
// data/doubao_health_history.json：{events: [...]}，滚动保留最近 HISTORY_MAX 条。
// 事件 schema（与 tasks/doubao_quota.rs 批量巡检 run_batch 同构）：
//   { ts, kind: "keepalive"|"renew"|"quota", ok, uid?, level?, summary, windows?, source? }

const HISTORY_MAX: usize = 400;

/// 追加一条运维历史事件（data_dir 为应用数据根目录；写入失败静默，不影响主流程）
fn append_history_event(data_dir: &std::path::Path, event: serde_json::Value) {
    // SQLite 化（P3）：doubao_health_events 表（HISTORY_MAX 上限裁剪保留）
    let store = crate::store::db(data_dir);
    let mut events = crate::store::docs::doubao_health_load(&store);
    events.push(event);
    if events.len() > HISTORY_MAX {
        events.drain(0..events.len() - HISTORY_MAX);
    }
    let _ = crate::store::docs::doubao_health_save(&store, &events);
}

fn append_history(state: &AppState, event: serde_json::Value) {
    append_history_event(&state.data_dir, event);
}

/// 读取运维历史（旧→新），前端据此渲染健康度卡与额度趋势
#[tauri::command]
pub fn doubao_history(state: State<AppState>) -> Result<Vec<serde_json::Value>, String> {
    // SQLite 化（P3）：doubao_health_events 表
    Ok(crate::store::docs::doubao_health_load(&crate::store::db(&state.data_dir)))
}

/// 从额度解析结果提取窗口数组（A2 趋势图数据点）
fn windows_of_parsed(parsed: &serde_json::Value) -> Vec<serde_json::Value> {
    parsed
        .get("items")
        .and_then(|i| i.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|it| {
                    let pct = it.get("used_percent")?.as_f64()?;
                    Some(serde_json::json!({
                        "name": it.get("name").and_then(|s| s.as_str()).unwrap_or("额度"),
                        "used_percent": pct,
                        "reset_at": it.get("reset_at").and_then(|s| s.as_str()).unwrap_or_default(),
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 代理自动抓到的豆包会话凭证（代理 MITM 层写 data/doubao_captured_credentials.json）。
/// 流程：启动代理 → 浏览器走系统代理登录网页版 doubao.com → 代理从 Cookie 中提取
/// sessionid / sid_guard 落盘 → 本命令读取最新一份，供编辑弹框一键填充。
#[derive(serde::Serialize, Clone)]
pub struct DoubaoCapturedCredential {
    pub session_id: String,
    pub sid_guard: String,
    pub host: String,
    pub captured_at: String,
    /// ttwid 设备 Cookie（对话导出 API 必需，D2）
    pub ttwid: String,
}

#[tauri::command]
pub fn doubao_captured_credential(state: State<AppState>) -> Result<Option<DoubaoCapturedCredential>, String> {
    // SQLite 化（P3）：kv `doubao_captured_credentials`
    let v: serde_json::Value = crate::store::db(&state.data_dir).kv_get("doubao_captured_credentials");
    if v.is_null() {
        return Ok(None);
    }
    let session_id = v
        .get("session_id")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(DoubaoCapturedCredential {
        session_id,
        sid_guard: v
            .get("sid_guard")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        host: v
            .get("host")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        captured_at: v
            .get("captured_at")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        ttwid: v
            .get("ttwid")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
    }))
}

/// 写入账号会话凭证的公共实现（session_source 标记来源）。
/// session_id / sid_guard 传 None 或空串 = 清除该字段；ttwid 仅在传 Some 时更新（None = 保留）。
/// auto_create=false（代理抓包路径）：账号不存在时直接报错不建号——抓包流量可能来自
/// 浏览器网页版/其他字节系应用（实测曾把网页版账号 232 自动建进池里），用户要求
/// 代理只"识别 sessionid 信息"回写已知账号，新账号一律走「保存当前登录态」手动收录。
fn apply_credential(
    state: &State<AppState>,
    pool: &mut DoubaoAccountPool,
    user_id: &str,
    session_id: Option<String>,
    sid_guard: Option<String>,
    ttwid: Option<String>,
    source: &str,
    auto_create: bool,
) -> Result<(), String> {
    // 仅快照、未入池的账号（PS 桥自动备份产生）允许直接补凭证：手动路径自动入池
    if !pool.accounts.iter().any(|a| a.user_id == user_id) {
        if !auto_create {
            return Err(format!("账号 {user_id} 未入池，跳过代理回写（不自动创建账号）"));
        }
        pool.accounts.push(DoubaoAccount {
            name: user_id.to_string(),
            note: String::new(),
            user_id: user_id.to_string(),
            added_at: fs_utils::now_ts(),
            last_active_at: None,
            session_id: None,
            sid_guard: None,
            session_expire_at: None,
            expired: None,
            cookies_synced_at: None,
            last_renew_at: None,
            session_source: None,
            ttwid: None,
            quota_level: None,
            quota_expire_at: None,
            quota_summary: None,
            quota_checked_at: None,
        });
    }
    let acc = pool
        .accounts
        .iter_mut()
        .find(|a| a.user_id == user_id)
        .ok_or_else(|| format!("账号 {user_id} 入池失败"))?;
    acc.session_id = session_id.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    acc.sid_guard = sid_guard.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(t) = ttwid {
        acc.ttwid = Some(t.trim().to_string()).filter(|s| !s.is_empty());
    }
    acc.session_expire_at = if acc.sid_guard.is_some() {
        // sid_guard 到期时间由 python 巡检解析；此处简单重置为未知
        None
    } else {
        acc.session_expire_at.clone()
    };
    acc.expired = None;
    if acc.session_id.is_some() {
        acc.session_source = Some(source.to_string());
    }
    save_pool(state, pool)
}

/// 更新账号的手动录入会话凭证（可选高级功能；有凭证的账号才能走探活巡检/对话导出）
#[tauri::command]
pub fn doubao_account_set_credential(
    state: State<AppState>,
    user_id: String,
    session_id: Option<String>,
    sid_guard: Option<String>,
    ttwid: Option<String>,
) -> Result<(), String> {
    let mut pool = load_pool(&state);
    apply_credential(&state, &mut pool, user_id.trim(), session_id, sid_guard, ttwid, "manual", true)
}

/// 按 UserID 查询单账号完整会话凭证（session_id/sid_guard/ttwid）。
/// 列表接口已改为掩码展示（审查 P1），编辑弹框按需回填走本命令。
/// 顶层参数命名遵循仓库约定：Rust 签名 user_id，前端 invoke 传 userId 自动映射。
#[tauri::command(async)]
pub fn doubao_account_get_credential(
    state: State<AppState>,
    user_id: String,
) -> Result<serde_json::Value, String> {
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let pool = load_pool(&state);
    let acc = pool
        .accounts
        .iter()
        .find(|a| a.user_id == user_id.trim())
        .ok_or_else(|| format!("账号 {user_id} 不在豆包账号池中"))?;
    Ok(serde_json::json!({
        "session_id": acc.session_id.clone().unwrap_or_default(),
        "sid_guard": acc.sid_guard.clone().unwrap_or_default(),
        "ttwid": acc.ttwid.clone().unwrap_or_default(),
    }))
}

/// 代理抓包凭证自动回写已入池账号。
/// 流程：启动代理 → 豆包客户端/网页版流量经过代理 → 代理 MITM 层抓到 sessionid/sid_guard
/// 落盘（multi_sids 按该 sessionid 解析出 uid）→ 本命令把凭证写入**该 uid 且必须已入池**的账号。
/// 目标与凭证同源于抓包文件（自洽）；未入池账号一律跳过、不自动创建（新账号走「保存当前登录态」）。
/// 返回 Some(说明) = 本次发生了写入（前端据此提示并刷新）；None = 无凭证/无 uid/未入池/内容未变。
#[tauri::command]
pub fn doubao_credential_auto_apply(state: State<AppState>) -> Result<Option<String>, String> {
    // 读最新抓包凭证（SQLite 化 P3：kv `doubao_captured_credentials`）
    let v: serde_json::Value = crate::store::db(&state.data_dir).kv_get("doubao_captured_credentials");
    if v.is_null() {
        return Ok(None);
    }
    let session_id = v
        .get("session_id")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return Ok(None);
    }
    let sid_guard = v
        .get("sid_guard")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let captured_at = v
        .get("captured_at")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let ttwid = v
        .get("ttwid")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();

    let mut pool = load_pool(&state);
    // 目标账号 = 抓包文件自带的 uid（代理 MITM 层用该请求 Cookie 里的 multi_sids
    // 按 **同一条 sessionid** 匹配出的主人）——凭证与归属天然自洽，不允许跨来源拼装。
    // 之前目标取 Local Storage/Local State/current_account.txt 等检测链、凭证取抓包文件，
    // 两条链时间窗不一致时会把 A 的 sessionid 写进 B 的账号（实测 908 与 232 的
    // sessionid 完全相同即此污染）；且抓包流量可能来自浏览器网页版/其他字节系应用，
    // 检测链给出的客户端 uid 与抓包凭证根本不是同一个会话。
    let uid = read_captured_uid(&state).unwrap_or_default();
    if uid.is_empty() {
        // 抓包未识别出 uid（无 multi_sids cookie 等）：无法可靠归属，不回写
        fs_utils::app_log(&state.data_dir, "代理凭证自动回写跳过：抓包文件未携带 uid，无法可靠归属账号");
        return Ok(None);
    }
    // 只回写已入池账号，绝不自动建号（新账号一律走「保存当前登录态」手动收录）
    if !pool.accounts.iter().any(|a| a.user_id == uid) {
        fs_utils::app_log(
            &state.data_dir,
            &format!("代理凭证自动回写跳过：抓到账号 {uid} 的凭证但未入池（不自动创建账号）"),
        );
        return Ok(None);
    }
    // 幂等：sessionid / sid_guard / ttwid 均未变化则跳过（sid_guard 会随滑动续期变化，需一并比对）
    if let Some(acc) = pool.accounts.iter().find(|a| a.user_id == uid) {
        if acc.session_id.as_deref() == Some(session_id.as_str())
            && acc.sid_guard.as_deref() == Some(sid_guard.as_str())
            && acc.ttwid.as_deref().unwrap_or("") == ttwid
        {
            return Ok(None);
        }
    }
    let ttwid_opt = if ttwid.is_empty() { None } else { Some(ttwid) };
    apply_credential(&state, &mut pool, &uid, Some(session_id), Some(sid_guard), ttwid_opt, "proxy", false)?;
    Ok(Some(format!("{uid}（{captured_at} 抓到）")))
}

/// 运行续期巡检（解密诊断 cookie + 探活续期，模式由 sync_only 决定；Rust 实现，原 doubao_renew.py）。
/// 含网络请求可达数秒，async 派发线程池执行避免阻塞 UI。
#[tauri::command(async)]
pub fn doubao_renew_run(
    state: State<AppState>,
    sync_only: Option<bool>,
) -> Result<serde_json::Value, String> {
    let v = crate::tasks::doubao_session::run(&state, sync_only.unwrap_or(false), None)?;
    // 记录运维历史（B2）：巡检摘要一句话化
    let summary_txt = if v.get("renew").is_some() {
        let r = &v["renew"];
        format!(
            "有效 {} · 过期 {} · 异常 {} · 跳过 {}",
            r.get("ok").and_then(|n| n.as_i64()).unwrap_or(0),
            r.get("expired").and_then(|n| n.as_i64()).unwrap_or(0),
            r.get("error").and_then(|n| n.as_i64()).unwrap_or(0),
            r.get("skipped").and_then(|n| n.as_i64()).unwrap_or(0),
        )
    } else {
        "Cookie 诊断".to_string()
    };
    append_history(
        &state,
        serde_json::json!({
            "ts": fs_utils::now_ts(), "kind": "renew", "ok": true,
            "summary": summary_txt, "source": "app",
        }),
    );
    Ok(v)
}

/// 把计划任务的长命令写入数据目录的 .cmd 启动器，返回启动器路径。
/// 背景（实测 2026-09-09）：schtasks /TR 参数上限 **261 字符**，dev 构建的
/// python/脚本绝对路径拼出的命令达 273 字符 → schtasks 报参数错误，注册失败，
/// 而错误 toast 仅显示 4 秒，被用户感知为「点击注册没有反应」。改用启动器后
/// /TR 只需 ~74 字符。
/// （审查 P2 修复：实现提升为 misc.rs 公共函数，签到任务与豆包任务共用同一方案，
/// 消除 misc.rs build_task_tr 拼长命令再次超限的回归）
use crate::commands::misc::write_task_launcher;

/// 从 schtasks /FO LIST 输出提取首个 HH:MM（等价正则 \d{2}:\d{2}；
/// 仓库未引入 regex crate，手写字符扫描。旧实现按 ": " 切片在本地化输出
/// "Start Time: 08:30:00" / 中文行混排时可能截到错误字段）。
/// 匹配到 ASCII 数字即处于字符边界，字节切片安全。
fn extract_hhmm(text: &str) -> String {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i + 5 <= n {
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2] == b':'
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 4].is_ascii_digit()
        {
            return text[i..i + 5].to_string();
        }
        i += 1;
    }
    String::new()
}

/// schtasks /Create 失败的统一处理：记入 app_log（注册失败原本无任何痕迹）并转译常见错误。
fn schtasks_create_failure(state: &State<AppState>, task: &str, stderr: &str) -> String {
    let detail = stderr.trim();
    fs_utils::app_log(&state.data_dir, &format!("计划任务 {task} 注册失败: {detail}"));
    let is_access_denied = detail.contains("Access is denied")
        || detail.contains("ERROR: Access is denied")
        || detail.contains("拒绝访问")
        || detail.contains("权限");
    if is_access_denied {
        return "权限不足（Access Denied）：请以管理员身份运行本应用后重新注册任务".to_string();
    }
    detail.to_string()
}

/// 注册豆包会话续期每日计划任务（schtasks 直调主 exe --task-run doubao-keepalive：
/// 启动豆包 8s 联网滑动续期后关闭，switcher Rust 化后不再依赖 PS 桥）
#[tauri::command(async)]
pub fn doubao_renew_task_register(state: State<AppState>, time: String) -> Result<(), String> {
    crate::commands::misc::schtasks_gate()?;
    // 审查修复（命令注入）：原 contains(':')/len 弱校验可被 "12:3&calc" 绕过，改严格白名单
    crate::commands::misc::validate_hhmm(&time)?;
    let exe = std::env::current_exe().map_err(|e| format!("获取主程序路径失败: {e}"))?;
    let data_dir = state.data_dir.to_string_lossy();
    // 命令写入 .cmd 启动器（/TR 261 字符上限，详见 write_task_launcher）；KeepAlive 无需 UserId。
    // 规则 R3：set "VAR=..." 与 "{exe}" 双引号形态使值中空格/中文/& 均安全
    let tr = write_task_launcher(
        &state,
        "doubao_renew",
        format!("set \"AIWORKDATA_DIR={data_dir}\"\r\n\"{}\" --task-run doubao-keepalive", exe.to_string_lossy()),
    )?;
    let (ok, _stdout, stderr) = crate::commands::misc::run_schtasks(&[
        "/Create",
        "/TN",
        crate::commands::misc::DOUBAO_TASK_NAME,
        "/TR",
        tr.as_str(),
        "/SC",
        "DAILY",
        "/ST",
        time.as_str(),
        "/F",
    ])?;
    if !ok {
        return Err(schtasks_create_failure(&state, "豆包续期", &stderr));
    }
    fs_utils::app_log(&state.data_dir, &format!("豆包续期定时任务已注册: {time}（启动器 {tr}）"));
    Ok(())
}

/// 已注册机器的一次性迁移（PS 7.2）：旧 KeepAlive 启动器引用 PS 桥 → 原地改写为
/// `--task-run doubao-keepalive`。schtasks 条目指向启动器文件路径——改写文件内容即
/// 完成迁移，无需 schtasks 操作。幂等：内容不含 trae-switch-bridge.ps1 即跳过；
/// 失败静默（app_log 留痕，下次启动重试）。main.rs 启动期调用（try_migrate_legacy_task 之后）。
pub fn try_migrate_keepalive_launcher(state: &AppState) -> Option<String> {
    let launcher = state.data_dir.join("task_doubao_renew.cmd");
    let Ok(content) = std::fs::read_to_string(&launcher) else { return None };
    if !content.contains("trae-switch-bridge.ps1") {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let body = format!(
        "@echo off\r\nset \"AIWORKDATA_DIR={dir}\"\r\n\"{exe}\" --task-run doubao-keepalive\r\n",
        dir = state.data_dir.to_string_lossy(),
        exe = exe.to_string_lossy()
    );
    if std::fs::write(&launcher, body).is_err() {
        return None;
    }
    let note = "计划任务迁移：豆包续期 KeepAlive 已切换为内置 Rust 实现（--task-run doubao-keepalive）".to_string();
    fs_utils::app_log(&state.data_dir, &note);
    Some(note)
}

/// 查询豆包续期计划任务状态（存在与否 + 触发时间）
#[tauri::command(async)]
pub fn doubao_renew_task_status(_state: State<AppState>) -> Result<String, String> {
    crate::commands::misc::schtasks_gate()?;
    let name = crate::commands::misc::DOUBAO_TASK_NAME;
    let (ok, stdout, _stderr) =
        crate::commands::misc::run_schtasks(&["/Query", "/TN", name, "/FO", "LIST"])?;
    if !ok {
        return Ok("not_registered".to_string());
    }
    // 输出形如 "Start Time: HH:MM:SS"（本地化系统可能是「开始时间:」），提取首个 HH:MM
    let time = extract_hhmm(&stdout);
    Ok(format!("registered:{time}"))
}

/// 注销豆包续期计划任务
#[tauri::command(async)]
pub fn doubao_renew_task_unregister(state: State<AppState>) -> Result<(), String> {
    crate::commands::misc::schtasks_gate()?;
    let name = crate::commands::misc::DOUBAO_TASK_NAME;
    let (ok, _stdout, stderr) =
        crate::commands::misc::run_schtasks(&["/Delete", "/TN", name, "/F"])?;
    if !ok && !stderr.contains("不存在") && !stderr.contains("does not exist") {
        return Err(stderr.trim().to_string());
    }
    fs_utils::app_log(&state.data_dir, "豆包续期定时任务已注销");
    Ok(())
}

// ── 额度定时巡检（A1/B4）：schtasks 每日调主 exe --task-run doubao-quota ────
// Rust 任务自行遍历池内有凭证账号：查额度 → 回写账号池缓存 + 运维历史；应用启动后
// 概述页健康度卡 / 账号页自动查询会读取历史与缓存展示。

/// 注册豆包额度巡检每日计划任务（schtasks 直调主 exe CLI 任务模式，原 python doubao_quota.py --all 已 Rust 化）
#[tauri::command(async)]
pub fn doubao_quota_task_register(state: State<AppState>, time: String) -> Result<(), String> {
    crate::commands::misc::schtasks_gate()?;
    // 审查修复（命令注入）：同上，弱校验改严格白名单
    crate::commands::misc::validate_hhmm(&time)?;
    let exe = std::env::current_exe().map_err(|e| format!("获取主程序路径失败: {e}"))?;
    let data_dir = state.data_dir.to_string_lossy();
    // 命令写入 .cmd 启动器：/TR 261 字符上限，dev 构建绝对路径拼出的命令 273 字符必然超限
    // （详见 write_task_launcher；实测报参数错误 → toast 4s 即逝 → 用户感知"点击注册没有反应"）
    // CLI 任务模式分支在 Tauri Builder 之前，不启动 GUI、不受单实例插件影响
    let tr = write_task_launcher(
        &state,
        "doubao_quota",
        format!(
            "set \"AIWORKDATA_DIR={data_dir}\"\r\n\"{}\" --task-run doubao-quota",
            exe.to_string_lossy()
        ),
    )?;
    let (ok, _stdout, stderr) = crate::commands::misc::run_schtasks(&[
        "/Create",
        "/TN",
        crate::commands::misc::DOUBAO_QUOTA_TASK_NAME,
        "/TR",
        tr.as_str(),
        "/SC",
        "DAILY",
        "/ST",
        time.as_str(),
        "/F",
    ])?;
    if !ok {
        return Err(schtasks_create_failure(&state, "额度巡检", &stderr));
    }
    fs_utils::app_log(&state.data_dir, &format!("豆包额度巡检定时任务已注册: {time}（启动器 {tr}）"));
    Ok(())
}

/// 查询豆包额度巡检计划任务状态
#[tauri::command(async)]
pub fn doubao_quota_task_status(_state: State<AppState>) -> Result<String, String> {
    crate::commands::misc::schtasks_gate()?;
    let name = crate::commands::misc::DOUBAO_QUOTA_TASK_NAME;
    let (ok, stdout, _stderr) =
        crate::commands::misc::run_schtasks(&["/Query", "/TN", name, "/FO", "LIST"])?;
    if !ok {
        return Ok("not_registered".to_string());
    }
    let time = extract_hhmm(&stdout);
    Ok(format!("registered:{time}"))
}

/// 注销豆包额度巡检计划任务
#[tauri::command(async)]
pub fn doubao_quota_task_unregister(state: State<AppState>) -> Result<(), String> {
    crate::commands::misc::schtasks_gate()?;
    let name = crate::commands::misc::DOUBAO_QUOTA_TASK_NAME;
    let (ok, _stdout, stderr) =
        crate::commands::misc::run_schtasks(&["/Delete", "/TN", name, "/F"])?;
    if !ok && !stderr.contains("不存在") && !stderr.contains("does not exist") {
        return Err(stderr.trim().to_string());
    }
    fs_utils::app_log(&state.data_dir, "豆包额度巡检定时任务已注销");
    Ok(())
}

// ── C1 一键以账号打开 / C3 快照版本元数据 ────────────────────────────────────

/// 一键「以账号 X 打开豆包」：恢复该账号快照后直接拉起客户端（复用桥 Switch 动作，
/// 等价于「切换 → 等待 → 打开」两步合并为一步）。代理运行中时注入 --proxy-server，
/// 行为对齐 open_doubao_app。NDJSON 进度复用 switch-progress / switch-done 事件管线。
// async：内含会话预检 python 子进程（网络 I/O）与 detect_guard_uid_strict 子进程，防 UI 冻结
#[tauri::command(async)]
pub fn doubao_open_as_account(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    proxy_port: Option<u16>,
) -> Result<(), String> {
    let uid = user_id.trim().to_string();
    if uid.is_empty() {
        return Err("user_id 不能为空".to_string());
    }
    // uid 直接拼进快照槽路径，先做防路径注入校验
    fs_utils::ensure_uid_safe(&uid)?;
    // 本地预检（桥 Switch 动作在关闭豆包前也会检查，这里提前给出明确错误）
    let slot = profiles_root(&state).join(&uid);
    if !slot.exists() {
        return Err(format!("账号 {uid} 无快照，请先登录该账号并保存登录态"));
    }
    // 目标快照登录 Cookie 预检：快照里没有 sessionid/sid_guard = 恢复后必然未登录
    // （此前 908 槽就是被未登录态污染后反复"切换成功但没登录"），直接拦截并告知补救方式；
    // 剩余有效期 <12h = 快照存的是游客会话，同样拦截
    match analyze_login_sessions(&slot) {
        LoginSessionAnalysis::NoSession { .. } => {
            return Err(format!(
                "账号 {uid} 的快照中未检测到登录会话（无 sessionid Cookie），恢复后必然未登录。\n\
                 请在豆包中登录该账号后重新「保存当前登录态」修复快照"
            ));
        }
        LoginSessionAnalysis::GuestOnly { .. } => {
            return Err(format!(
                "账号 {uid} 的快照保存的是游客/临时会话（sessionid 剩余有效期不足 12 小时），\
                 恢复后无法登录。\n请在豆包中登录该账号后重新「保存当前登录态」修复快照"
            ));
        }
        // LoggedIn（含会话在非活跃 Profile 的聚合判定）放行；Unavailable fail-open 不阻断
        _ => {}
    }
    // 服务端会话预检：本地 Cookie 存在≠会话在服务端仍有效。曾在客户端内退出登录该账号时
    // passport 会吊销会话（快照文件完好但已"中毒"），恢复后一联网即被强制登出。
    // 探测 expired 时中止并给出补救指引；不可验证 fail-open（见 probe_slot_session_alive）。
    probe_slot_session_alive(&state.data_dir, &uid)?;
    let include_idb = state.settings().doubao_snapshot_include_idb;
    // 防误覆盖守卫（严格版）：切换回写"当前态"到来源账号槽前，switcher 会校验检测
    // uid 与 current_account.txt 一致。这里取"uid 检测 + Live Cookies 登录会话验证"
    // 双条件——未登录时返回空串，switcher 跳过账号槽回写只备份 last
    //（防止未登录态污染账号快照）
    let expected_uid = detect_guard_uid_strict(&state);

    fs_utils::app_log(&state.data_dir, &format!("一键以账号打开豆包: user_id={uid}"));

    let args = crate::switcher::RunArgs {
        action: crate::switcher::Action::Switch,
        target_app: crate::switcher::TargetApp::Doubao,
        user_id: Some(uid),
        proxy_port: proxy_port.filter(|p| *p > 0),
        include_indexeddb: include_idb,
        expected_current_uid: expected_uid,
        data_dir: state.data_dir.clone(),
    };
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    // 后台线程执行（含优雅关闭 8s 等待，不阻塞命令返回）
    std::thread::spawn(move || {
        // 进度复用 switch-progress / switch-done 事件管线（与切换管线完全一致）
        let sink = crate::switcher::TauriSink::new(&app2, "switch-progress", &data_dir);
        let result = crate::switcher::run_action(args, &sink);
        let (success, raw) = match &result {
            Ok(line) => (true, line.clone()),
            Err(line) => (false, line.clone()),
        };
        let _ = app2.emit("switch-done", serde_json::json!({ "success": success, "raw": raw }));
    });
    Ok(())
}

/// 豆包快照版本元数据（C3）：读取槽位内 snapshot_meta.json + Last Version，
/// 供前端快照列悬停展示（schema 版本 / Chromium 版本 / 生成时间）。
/// 旧版快照无元数据 → 返回 None（前端展示「旧版快照」）。
#[derive(serde::Serialize, Clone)]
pub struct DoubaoSnapshotMeta {
    pub schema_version: i64,
    pub created_at: String,
    pub chromium_version: String,
    pub include_idb: bool,
}

#[tauri::command]
pub fn doubao_snapshot_meta(state: State<AppState>, user_id: String) -> Result<Option<DoubaoSnapshotMeta>, String> {
    // uid 直接拼进快照槽路径，先做防路径注入校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let slot = profiles_root(&state).join(user_id.trim());
    if !slot.exists() {
        return Ok(None);
    }
    // Last Version：快照生成时的豆包（Chromium 内核）版本
    let chromium_version = std::fs::read_to_string(slot.join("Last Version"))
        .map(|s| s.trim().trim_start_matches('\u{feff}').to_string())
        .unwrap_or_default();
    let meta_path = slot.join("snapshot_meta.json");
    if !meta_path.exists() {
        // 旧版快照：无元数据文件，仅有 Last Version 时也如实返回
        if chromium_version.is_empty() {
            return Ok(None);
        }
        return Ok(Some(DoubaoSnapshotMeta {
            schema_version: 0, // 0 = 无元数据（旧版快照）
            created_at: String::new(),
            chromium_version,
            include_idb: false,
        }));
    }
    let raw = std::fs::read_to_string(&meta_path).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    Ok(Some(DoubaoSnapshotMeta {
        schema_version: v.get("schemaVersion").and_then(|s| s.as_i64()).unwrap_or(0),
        created_at: v
            .get("createdAt")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        chromium_version: v
            .get("chromiumVersion")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or(chromium_version),
        include_idb: v.get("includeIndexedDB").and_then(|b| b.as_bool()).unwrap_or(false),
    }))
}

// ───────────── D1 对话数据独立备份（IndexedDB + DoubaoStorage） ─────────────
// 豆包对话正文在云端（跟账号走），本地 IndexedDB 存的是会话列表缓存/技能配置等客户端状态。
// 独立备份 = 把这些本地状态复制到 data/doubao_chats/<uid>/，与快照解耦：
// 重装/换机后先恢复对话数据再登录，客户端体验立即可用；配合云端同步，对话不丢。

/// 豆包 User Data 目录（与 env.rs 安装探测一致的默认位置；
/// 基根 Windows=%LOCALAPPDATA%，mac=Application Support）
fn doubao_user_data_dir() -> PathBuf {
    crate::platform::local_data_root_lossy()
        .join("Doubao")
        .join("User Data")
}

/// 对话数据备份根目录：data/doubao_chats/<uid>/
fn chat_backup_dir(state: &State<AppState>, user_id: &str) -> PathBuf {
    state.data_dir.join("data").join("doubao_chats").join(user_id)
}

/// 收集 User Data 下各 Chromium Profile 的豆包对话相关目录（IndexedDB 子集 + DoubaoStorage）。
/// 返回 (profile 名, 相对路径) 列表；空 = 无可备份内容。
fn chat_source_dirs(user_data: &std::path::Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(user_data) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let is_profile = name == "Default" || name.starts_with("Profile ");
        if !is_profile {
            continue;
        }
        // IndexedDB 下仅豆包相关库（排除飞书 iframe / 扩展等）
        let idb = p.join("IndexedDB");
        if let Ok(dbs) = std::fs::read_dir(&idb) {
            for db in dbs.flatten() {
                let dn = db.file_name().to_string_lossy().to_string();
                if dn.starts_with("chrome_doubao-") || dn.starts_with("https_www.doubao.com") {
                    out.push((name.clone(), db.path()));
                }
            }
        }
        let storage = p.join("DoubaoStorage");
        if storage.is_dir() {
            out.push((name.clone(), storage));
        }
    }
    out
}

/// 备份当前豆包本地对话数据（自动先关闭豆包：leveldb 运行中复制易损坏）。
/// 覆盖式备份（保留最新一份），结果 {ok, files, path}。
#[tauri::command(async)]
pub fn doubao_chatdata_backup(state: State<AppState>, user_id: String) -> Result<serde_json::Value, String> {
    // uid 直接拼进备份根目录且本命令会整目录删除（remove_dir_all），必须先校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let user_data = doubao_user_data_dir();
    if !user_data.is_dir() {
        return Err("未找到豆包数据目录（%LOCALAPPDATA%\\Doubao\\User Data），请先安装并登录豆包".into());
    }
    let sources = chat_source_dirs(&user_data);
    if sources.is_empty() {
        return Err("未发现可备份的对话数据（IndexedDB / DoubaoStorage 为空）".into());
    }
    crate::commands::process::graceful_kill_app("Doubao")?;

    let dest_root = chat_backup_dir(&state, &user_id);
    let _ = std::fs::remove_dir_all(&dest_root);
    let mut files = 0usize;
    for (profile, src) in &sources {
        // 备份内保留 profile 名，恢复时按名回写
        let rel = src.strip_prefix(&user_data).unwrap_or(src);
        let dst = dest_root.join(profile).join(rel);
        files += copy_dir_recursive(src, &dst, &[])?;
    }
    if files == 0 {
        let _ = std::fs::remove_dir_all(&dest_root);
        return Err("复制对话数据失败（0 个文件）".into());
    }
    let meta = serde_json::json!({
        "schemaVersion": 1, "user_id": user_id, "files": files,
        "backedAt": fs_utils::now_ts(),
    });
    let _ = std::fs::write(dest_root.join("chat_backup_meta.json"), serde_json::to_string_pretty(&meta).unwrap_or_default());
    fs_utils::app_log(&state.data_dir, &format!("doubao: 对话数据已备份 {user_id}（{files} 文件）"));
    append_history(&state, serde_json::json!({
        "ts": fs_utils::now_ts(), "kind": "chats", "ok": true,
        "uid": user_id, "summary": format!("对话数据备份 {files} 文件"), "source": "app",
    }));
    Ok(serde_json::json!({ "ok": true, "files": files, "path": dest_root.display().to_string() }))
}

/// 恢复对话数据备份到豆包 User Data（自动先关闭豆包）。
#[tauri::command(async)]
pub fn doubao_chatdata_restore(state: State<AppState>, user_id: String) -> Result<serde_json::Value, String> {
    // uid 直接拼进备份目录路径，先做防路径注入校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let backup = chat_backup_dir(&state, &user_id);
    if !backup.is_dir() {
        return Err(format!("该账号没有对话数据备份：{}", backup.display()));
    }
    let user_data = doubao_user_data_dir();
    if !user_data.is_dir() {
        return Err("未找到豆包数据目录，请先安装豆包".into());
    }
    crate::commands::process::graceful_kill_app("Doubao")?;
    let mut files = 0usize;
    // 遍历备份内的 profile 目录（Default / Profile N）
    for entry in std::fs::read_dir(&backup).map_err(|e| format!("读取备份失败: {e}"))?.flatten() {
        let profile = entry.file_name().to_string_lossy().to_string();
        if profile == "chat_backup_meta.json" || !entry.path().is_dir() {
            continue;
        }
        let files2 = copy_dir_recursive(&entry.path(), &user_data.join(&profile), &[])?;
        files += files2;
    }
    if files == 0 {
        return Err("恢复失败（0 个文件）".into());
    }
    fs_utils::app_log(&state.data_dir, &format!("doubao: 对话数据已恢复 {user_id}（{files} 文件）"));
    append_history(&state, serde_json::json!({
        "ts": fs_utils::now_ts(), "kind": "chats", "ok": true,
        "uid": user_id, "summary": format!("对话数据恢复 {files} 文件"), "source": "app",
    }));
    Ok(serde_json::json!({ "ok": true, "files": files }))
}

/// 查询对话数据备份状态（供账号行展示：是否有备份 / 时间 / 文件数）
#[tauri::command]
pub fn doubao_chatdata_info(state: State<AppState>, user_id: String) -> Result<serde_json::Value, String> {
    // uid 直接拼进备份目录路径，先做防路径注入校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let dir = chat_backup_dir(&state, &user_id);
    if !dir.is_dir() {
        return Ok(serde_json::json!({ "backed": false }));
    }
    let (size, files) = crate::commands::profile::dir_stats(&dir);
    let meta_raw = std::fs::read_to_string(dir.join("chat_backup_meta.json")).unwrap_or_default();
    let meta: serde_json::Value = serde_json::from_str(&meta_raw).unwrap_or(serde_json::Value::Null);
    Ok(serde_json::json!({
        "backed": true, "size_bytes": size, "files": files,
        "backed_at": meta.get("backedAt").and_then(|s| s.as_str()),
    }))
}

// ───────────── D2 对话记录导出（API 拉取 → markdown/json） ─────────────

/// 导出豆包对话记录：走官方 IM API（recent_conv 列表 + chain/single 消息；Rust 实现，原 doubao_chats.py --export），
/// 输出 markdown + json 到 data/exports/。需要账号已录入凭证（sessionid/sid_guard/ttwid）。
#[tauri::command(async)]
pub fn doubao_export_chats(state: State<AppState>, user_id: String) -> Result<serde_json::Value, String> {
    // uid 用于构造导出路径，入口先做防注入校验
    fs_utils::ensure_uid_safe(user_id.trim())?;
    let v = crate::tasks::doubao_chats::export_account(&state, user_id.trim(), 50, 10)?;
    if v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
        let convs = v.get("conversations").and_then(|n| n.as_i64()).unwrap_or(0);
        append_history(&state, serde_json::json!({
            "ts": fs_utils::now_ts(), "kind": "chats", "ok": true,
            "uid": user_id, "summary": format!("导出对话 {convs} 个"), "source": "app",
        }));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机实测结构（2026-09-08，Doubao 2.27）：user_id 嵌在 text_picker.current_user 下
    const REAL_LIKE_JSON: &str = r#"{
       "text_picker": {
          "current_user": {
             "enable": true,
             "modify_version": "2.27.12",
             "user_action_time": "1788530411262",
             "user_id": "340177329338890"
          },
          "pid": "26664",
          "raw_input_hwnd": "0x51d0e60",
          "users": [ {
             "enable": true,
             "user_action_time": 1788530411262,
             "user_id": "340177329338890"
          } ]
       }
    }"#;

    #[test]
    fn detect_uid_finds_nested_user_id() {
        let v: serde_json::Value = serde_json::from_str(REAL_LIKE_JSON).unwrap();
        let mut cands = Vec::new();
        collect_uid_candidates(&v, false, &mut cands);
        cands.sort_by(|a, b| {
            b.from_current_user
                .cmp(&a.from_current_user)
                .then(b.action_time.cmp(&a.action_time))
        });
        let best = cands.first().expect("应至少识别出一个候选");
        assert_eq!(best.user_id, "340177329338890");
        assert!(best.numeric);
        assert!(best.from_current_user);
    }

    #[test]
    fn detect_uid_prefers_current_user_over_stale() {
        // 两个不同 uid：current_user 下的是旧时间戳，也应胜出
        let v: serde_json::Value = serde_json::from_str(
            r#"{"text_picker":{
                 "users":[{"user_id":"111","user_action_time":999}],
                 "current_user":{"user_id":"222","user_action_time":1}
               }}"#,
        )
        .unwrap();
        let mut cands = Vec::new();
        collect_uid_candidates(&v, false, &mut cands);
        cands.sort_by(|a, b| {
            b.from_current_user
                .cmp(&a.from_current_user)
                .then(b.action_time.cmp(&a.action_time))
        });
        assert_eq!(cands.first().unwrap().user_id, "222");
    }

    #[test]
    fn uid_value_filters_garbage() {
        assert_eq!(value_to_uid(&serde_json::json!(" 12345 ")), Some("12345".into()));
        assert_eq!(value_to_uid(&serde_json::json!(12345)), Some("12345".into()));
        assert_eq!(value_to_uid(&serde_json::json!("")), None);
        assert_eq!(value_to_uid(&serde_json::json!(true)), None);
    }

    /// Bug1 回归：登录账号识别应以 Local State 的 saman 块为准（text_picker 不随登录切换更新）
    #[test]
    fn local_state_picks_most_recent_active_profile() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"profile":{
                 "info_cache":{
                   "Default":{"active_time":1788696072.09,"name":"工作",
                              "saman":{"user_id":"908645355226505","user_name":"zhutianwei"}},
                   "Profile 6":{"name":"用户309873",
                              "saman":{"user_id":"340177329338890","user_name":"old"}}
                 }}}"#,
        )
        .unwrap();
        // Default 活跃时间更新 → 取 908645355226505（而不是字母序第一个 Profile 6）
        assert_eq!(pick_uid_from_info_cache(&v).as_deref(), Some("908645355226505"));
    }

    #[test]
    fn local_state_prefers_last_active_profiles() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"profile":{
                 "last_active_profiles":["Default","Profile 6"],
                 "info_cache":{
                   "Default":{"active_time":99.0,"saman":{"user_id":"111"}},
                   "Profile 6":{"saman":{"user_id":"222"}}}}}"#,
        )
        .unwrap();
        // last_active_profiles 最后一个（最近一次活跃）优先于 active_time
        assert_eq!(pick_uid_from_info_cache(&v).as_deref(), Some("222"));
    }

    #[test]
    fn local_state_skips_profiles_without_saman() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"profile":{"info_cache":{
                   "Default":{"active_time":1.0},
                   "Profile 6":{"active_time":2.0,"saman":{"user_id":"333"}}}}}"#,
        )
        .unwrap();
        assert_eq!(pick_uid_from_info_cache(&v).as_deref(), Some("333"));
        assert_eq!(pick_uid_from_info_cache(&serde_json::json!({"profile":{"info_cache":{}}})), None);
        assert_eq!(pick_uid_from_info_cache(&serde_json::json!({})), None);
    }

    // ── analyze_login_sessions（Issue #15 误报修复回归）────────────────────────

    /// 构造测试 Profile：Cookies 库 + 可选的 sessionid（expires_utc 为 Webkit 微秒时间戳）
    fn make_profile(ud: &std::path::Path, name: &str, session_expires: Option<i64>) {
        let dir = ud.join(name).join("Network");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("Cookies")).unwrap();
        conn.execute_batch(
            "CREATE TABLE cookies (host_key TEXT, name TEXT, expires_utc INTEGER);
             INSERT INTO cookies VALUES ('.doubao.com', 'ttwid', 99999999999000000);",
        )
        .unwrap();
        if let Some(exp) = session_expires {
            conn.execute(
                "INSERT INTO cookies VALUES ('.doubao.com', 'sessionid', ?1)",
                rusqlite::params![exp],
            )
            .unwrap();
        }
    }

    fn make_local_state(ud: &std::path::Path, last_used: &str) {
        std::fs::write(
            ud.join("Local State"),
            format!(r#"{{"profile":{{"last_used":"{last_used}"}}}}"#),
        )
        .unwrap();
    }

    fn webkit_expires_in(secs_from_now: i64) -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        (now + secs_from_now + 11_644_473_600) * 1_000_000
    }

    /// 误报场景①回归：登录会话在非活跃 Profile → 聚合判定为已登录（此前误报「未检测到登录会话」）
    #[test]
    fn analyze_aggregates_session_from_non_active_profile() {
        let td = std::env::temp_dir().join(format!("aw_analyze_t1_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(&td).unwrap();
        make_local_state(&td, "Default");
        make_profile(&td, "Default", None); // 活跃 Profile 无会话
        make_profile(&td, "Profile 1", Some(webkit_expires_in(30 * 86400))); // 真登录在别处
        assert_eq!(analyze_login_sessions(&td), LoginSessionAnalysis::LoggedIn);
        let _ = std::fs::remove_dir_all(&td);
    }

    /// 误报场景②回归：活跃 Profile 无 Cookies 库（读取失败）且其他 Profile 也无会话
    /// → Unavailable fail-open（此前误报「未检测到登录会话」）
    #[test]
    fn analyze_fails_open_when_active_profile_unreadable() {
        let td = std::env::temp_dir().join(format!("aw_analyze_t2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(&td).unwrap();
        make_local_state(&td, "Default");
        std::fs::create_dir_all(td.join("Default").join("Network")).unwrap(); // 无 Cookies 库
        make_profile(&td, "Profile 1", None); // 可读但无会话
        assert_eq!(analyze_login_sessions(&td), LoginSessionAnalysis::Unavailable);
        let _ = std::fs::remove_dir_all(&td);
    }

    /// 全部 Profile 读取成功且均无会话 → 确认未登录（唯一允许拦「未检测到」的路径）
    #[test]
    fn analyze_no_session_when_all_profiles_clean() {
        let td = std::env::temp_dir().join(format!("aw_analyze_t3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(&td).unwrap();
        make_local_state(&td, "Default");
        make_profile(&td, "Default", None);
        assert!(matches!(
            analyze_login_sessions(&td),
            LoginSessionAnalysis::NoSession { .. }
        ));
        let _ = std::fs::remove_dir_all(&td);
    }

    /// 仅剩游客会话（sessionid 剩余 <12h）→ GuestOnly（游客拦截不受聚合影响）
    #[test]
    fn analyze_guest_only_when_remaining_under_12h() {
        let td = std::env::temp_dir().join(format!("aw_analyze_t4_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(&td).unwrap();
        make_local_state(&td, "Default");
        make_profile(&td, "Default", Some(webkit_expires_in(6 * 3600)));
        // remaining 按分析时刻重算，会略小于构造时的 6h → 范围断言而非精确断言
        match analyze_login_sessions(&td) {
            LoginSessionAnalysis::GuestOnly { remaining } => {
                assert!(remaining > 0 && remaining <= 6 * 3600, "remaining={remaining}");
            }
            other => panic!("期望 GuestOnly，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&td);
    }

    /// 活跃 Profile 真登录 → LoggedIn（原主路径行为保持）
    #[test]
    fn analyze_logged_in_from_active_profile() {
        let td = std::env::temp_dir().join(format!("aw_analyze_t5_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(&td).unwrap();
        make_local_state(&td, "Default");
        make_profile(&td, "Default", Some(webkit_expires_in(30 * 86400)));
        assert_eq!(analyze_login_sessions(&td), LoginSessionAnalysis::LoggedIn);
        let _ = std::fs::remove_dir_all(&td);
    }

    /// 无任何 Profile 目录 → Unavailable（不阻断）
    #[test]
    fn analyze_unavailable_without_profiles() {
        let td = std::env::temp_dir().join(format!("aw_analyze_t6_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(&td).unwrap();
        assert_eq!(analyze_login_sessions(&td), LoginSessionAnalysis::Unavailable);
        let _ = std::fs::remove_dir_all(&td);
    }
}
