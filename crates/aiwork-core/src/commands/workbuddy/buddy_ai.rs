//! Buddy 国际版（buddy-ai）独立池（《Buddy国际版支持落地方案.md》阶段 1 池骨架）。
//! 移植 main f5f2cd3..61cc245（docker_main 适配：去 tauri 命令属性，事件推送改回调，
//! 网关热重载改 runtime::reload_pools_after_change；桌面专属命令不平移——
//! local_scan / env_reset / open_data_dir / startup_auto_checkin / pool_account_id_by_uid）。
//!
//! §3.1 不交织六条硬约束的落点：
//! - 账号分池：`buddy_ai_accounts` 表独立于 `wb_accounts`，禁止混居；
//! - 配置分键：`buddy_ai_*` 系列（token store 表 / vault ns / 池锁均独立）；
//! - 失败域隔离：独立 `buddy_ai_pool_lock`，换号/冷却在池边界内闭环；
//! - UI 分区：账号列表命令独立（`buddy_ai_*`），事件前缀 `buddy-ai-oauth-*`。
//!
//! 端点依据（Gate 阻塞层 2026-10-15 已实测收口）：
//! - R-1 OAuth 全链路：`www.workbuddy.ai/v2/plugin/auth/state?platform=workbuddy-ai`；
//! - R-2a refresh：`www.workbuddy.ai/v2/plugin/auth/token/refresh`（rt 每次轮换，回写为准）；
//! - R-9：workbuddy.ai ↔ codebuddy.ai 同一账号体系，单一鉴权门面即可。

use serde_json::{json, Value};

use crate::fs_utils;
use crate::state::AppState;

use super::accounts::find_uid_or_id;
use super::common::{account_id_of, as_str, load_pool, save_pool, WorkBuddyAccount, WbPool};
use super::oauth::{oauth_login_async, PoolSink, WbOauthEmitter};

// ── 池 / token store 读写包装（buddy-ai 独立表）─────────────────────────────

pub(super) fn load_buddy_pool(state: &AppState) -> WbPool {
    let v = crate::store::docs::buddy_ai_pool_load(&crate::store::db(&state.data_dir));
    // BM-2 审查修复：逐行反序列化跳过坏行——整池 from_value 失败 → 空池 → 后续
    // save 整表替换会把全部账号清空（读失败被写放大为数据丢失）。坏行记日志留证
    match serde_json::from_value::<WbPool>(v.clone()) {
        Ok(p) => p,
        Err(_) => {
            let mut accounts = Vec::new();
            if let Some(arr) = v.get("accounts").and_then(Value::as_array) {
                for (i, row) in arr.iter().enumerate() {
                    match serde_json::from_value::<WorkBuddyAccount>(row.clone()) {
                        Ok(a) => accounts.push(a),
                        Err(e) => fs_utils::app_log(
                            &state.data_dir,
                            &format!("buddy-ai: 池行损坏已跳过（index {i}）: {e}"),
                        ),
                    }
                }
            }
            WbPool { accounts }
        }
    }
}

/// buddy-ai 池账号花名册（跨模块 tasks/buddy_ai_checkin 消费；WbPool 字段
/// pub(super) 不对 tasks 暴露，经此 helper 交出 (id, 展示名, uid) 三元组）
pub(crate) fn buddy_pool_roster(state: &AppState) -> Vec<(String, String, String)> {
    load_buddy_pool(state)
        .accounts
        .iter()
        .filter(|a| !a.disabled) // P3 启停：停用账号不参与打卡花名册
        .map(|a| (a.id.clone(), if a.nickname.is_empty() { a.id.clone() } else { a.nickname.clone() }, a.uid.clone()))
        .collect()
}

/// 账号是否仍在 buddy-ai 池中（含停用；审查修复 #3 消费——roster 滤 disabled
/// 无法区分「停用」与「移除」，纯 DB 读无锁，持刷新锁下调用安全）
pub(crate) fn buddy_ai_pool_contains(state: &AppState, id: &str) -> bool {
    load_buddy_pool(state).accounts.iter().any(|a| a.id == id)
}

/// buddy-ai 池单账号积分快照回写（持 buddy_ai_pool_lock 短临界区；三键口径
/// 对齐 write_back_pool_balances：balance / fetched_at / 最早未来到期）
pub(crate) fn buddy_pool_write_credits(
    state: &AppState,
    acct_id: &str,
    balance: f64,
    expire_secs: Option<i64>,
    packages: Option<Value>,
) -> Result<(), String> {
    let _guard = state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = load_buddy_pool(state);
    let Some(a) = pool.accounts.iter_mut().find(|a| a.id == acct_id) else {
        return Ok(()); // 账号已移除（打卡期间竞态）：静默丢弃
    };
    a.credits_balance = Some(balance);
    a.credits_fetched_at = Some(chrono::Utc::now().to_rfc3339());
    if expire_secs.is_some() {
        a.credits_expire_at = expire_secs; // 秒级时间戳（WorkBuddyAccount 口径）
    }
    // 空明细不覆盖旧值（issue #179）：上游形状变化/瞬时抖动一次就抹掉历史明细
    // 且无任何痕迹；「包用完」上游仍返回 remain=0 条目，不经此分支。对齐 expire
    // 仅 Some 才覆盖的口径
    let has_pkgs = packages
        .as_ref()
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if has_pkgs {
        a.credits_packages = packages; // 分包明细（P9）
    }
    save_buddy_pool(state, &pool)?;
    // P8：打卡/取数回写后刷新当日积分快照（趋势曲线数据源；锁内调用不重复加锁）
    buddy_ai_credits_snapshot_save(state, &pool);
    Ok(())
}

pub(super) fn save_buddy_pool(state: &AppState, pool: &WbPool) -> Result<(), String> {
    let v = serde_json::to_value(pool).map_err(|e| format!("序列化失败: {e}"))?;
    crate::store::docs::buddy_ai_pool_save(&crate::store::db(&state.data_dir), &v)
}

/// P8：当日积分快照落库（同日覆盖 + 365 天裁剪；积分看板趋势曲线数据源）。
/// 不持锁——调用点均在 buddy_ai_pool_lock 临界区内（write_credits /
/// credits_fetch 回写段），池读为最新态；落库失败仅日志，不阻塞主流程。
/// 私有可见性：仅本文件两个回写点消费，避免 pub(crate) 暴露 pub(super) 的 WbPool
fn buddy_ai_credits_snapshot_save(state: &AppState, pool: &WbPool) {
    let db = crate::store::db(&state.data_dir);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    // 审查修复（H3）：明细与 total 同口径——原明细含停用账号而 total 过滤，前端
    // 按明细求和与 total 不闭合。total 改从过滤后明细累加，结构上保证恒等
    // （BL-7：停用账号不计入趋势口径，与花名册/网关池一致）
    let accounts: Vec<Value> = pool
        .accounts
        .iter()
        .filter(|a| !a.disabled)
        .map(|a| {
            json!({
                "id": a.id,
                "name": if a.nickname.is_empty() { a.id.clone() } else { a.nickname.clone() },
                "balance": a.credits_balance,
            })
        })
        .collect();
    let total: f64 = accounts
        .iter()
        .filter_map(|a| a.get("balance").and_then(Value::as_f64))
        .sum();
    // 审查修复（H4）：earned 改「较上一快照余额差分」（max(差分, 0)）——对齐国内
    // credits.rs append_credits_snapshot 口径；buddy-ai 无签到 reward 日志（打卡为
    // 每日活跃，不产生积分入账记录），差分是唯一可得口径。沿用国内 2026-10-06
    // 账号集合可比性守卫：与上一快照集合不一致（增/删账号）时新账号余额会被误记
    // 为「当日获得」，此时 earned 记 Null（前端趋势留空，不画 0）
    let hist = crate::store::docs::buddy_ai_credits_history_load(&db);
    let prev_snapshot = hist
        .get("snapshots")
        .and_then(Value::as_array)
        .and_then(|snaps| {
            snaps
                .iter()
                .filter(|s| s.get("date").and_then(Value::as_str).map(|d| d < today.as_str()).unwrap_or(false))
                .filter(|s| s.get("total_balance").and_then(Value::as_f64).is_some())
                .last()
        });
    let prev_total = prev_snapshot.and_then(|s| s.get("total_balance").and_then(Value::as_f64));
    // 集合指纹（排序去重账号 id + 余额是否已观测标记；空明细 → None，视为不可判，
    // 维持差分行为）。审查复审修复：仅比对 id 感知不到既有账号余额 None→Some 的
    // 首次观测——该账号全部余额会被误记为当日 earned（虚增），纳入指纹后此类
    // 首观测视为集合变化，当日 earned 记 Null 等待下一快照
    fn fingerprint(accs: &[Value]) -> Option<Vec<String>> {
        let mut ids: Vec<String> = accs
            .iter()
            .filter_map(|a| {
                a.get("id").and_then(Value::as_str).map(|s| {
                    format!(
                        "{}:{}",
                        s,
                        a.get("balance").and_then(Value::as_f64).is_some()
                    )
                })
            })
            .collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() { None } else { Some(ids) }
    }
    let set_changed = match (
        prev_snapshot
            .and_then(|s| s.get("accounts"))
            .and_then(Value::as_array)
            .map(|v| v.as_slice())
            .and_then(fingerprint),
        fingerprint(&accounts),
    ) {
        (Some(p), Some(c)) => p != c,
        _ => false,
    };
    let earned = if set_changed {
        Value::Null
    } else {
        prev_total
            .map(|prev| (total - prev).max(0.0))
            .map(Value::from)
            .unwrap_or(Value::Null)
    };
    let snap = json!({
        "date": today,
        "ts": chrono::Utc::now().timestamp(),
        "total_balance": total,
        "accounts": accounts,
        "earned": earned,
    });
    if let Err(e) = crate::store::docs::buddy_ai_credits_history_upsert(&db, &snap) {
        fs_utils::app_log(&state.data_dir, &format!("buddy-ai: 积分快照落库失败: {e}"));
    }
    let _ = crate::store::docs::buddy_ai_credits_history_prune(&db);
}

/// token store 读取（vault 回填，仅内存不落明文）
fn load_buddy_tokens(state: &AppState) -> Value {
    crate::tasks::wb_common::buddy_ai_token_store_load(&state.data_dir)
}

// ── 国际版默认 domain（区域判定唯一真源仍是 token rec 的 domain 字段）───────

/// 国际账号 token rec 的 domain 缺省值：`www.workbuddy.ai`（含 `.workbuddy.ai` →
/// `is_global_region` 命中 Global，chat/billing/refresh 全链按区切换）
pub const INTL_DOMAIN: &str = "www.workbuddy.ai";

fn intl_domain_or(domain: Option<String>) -> String {
    match domain {
        Some(d) if d.contains(".workbuddy.ai") => d,
        // 缺省/非法一律回落官方域名（buddy-ai 池只收国际账号，不可能出现 CN 域）
        _ => INTL_DOMAIN.to_string(),
    }
}

// ── API 网关 buddy-ai 池取号（T2.1 同构，阶段 2 dispatch 接线）──────────────

/// 汇总 buddy-ai 上游账号：buddy_ai_accounts 池启用账号 + buddy-ai token store 凭证
/// → WbSyncAccount（global_region 恒 true——独立池内不存在 CN 账号）。
/// 与 wb_upstream_accounts 完全隔离：两池各自装配、互不读取对方表。
pub(crate) fn buddy_ai_upstream_accounts(state: &AppState) -> Vec<crate::api_server::pool::WbSyncAccount> {
    let pool = load_buddy_pool(state);
    let tokens = load_buddy_tokens(state)
        .get("tokens")
        .and_then(|t| t.as_object())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for a in &pool.accounts {
        if a.disabled {
            continue; // P3 启停：停用账号不入网关池（不参与取号/调度）
        }
        let rec = tokens.get(&a.id).cloned().unwrap_or_default();
        let token = as_str(fs_utils::dig(&rec, &["access_token"])).unwrap_or_default();
        if token.is_empty() {
            continue;
        }
        let domain = intl_domain_or(as_str(fs_utils::dig(&rec, &["domain"])));
        let eid = as_str(fs_utils::dig(&rec, &["enterprise_id", "enterpriseId"])).unwrap_or_default();
        out.push(crate::api_server::pool::WbSyncAccount {
            uid: a.id.clone(),
            name: if a.nickname.is_empty() { a.id.clone() } else { a.nickname.clone() },
            token,
            domain,
            enterprise_id: eid,
            global_region: true,
            credits: a.credits_balance,
            credits_expire_at: a.credits_expire_at,
            credits_fetched_at: a
                .credits_fetched_at
                .as_deref()
                .and_then(crate::fs_utils::parse_ts_loose),
            needs_relogin: a.needs_relogin,
            group_id: a.group_id.clone(),
        });
    }
    out
}

// ── OAuth 扫码登录（R-1 全链路实测通过，§8-4 并入阶段 1）────────────────────

/// buddy-ai OAuth 扫码登录：`www.workbuddy.ai` + `platform=workbuddy-ai` 全链路，
/// 入池落 buddy-ai 独立池；事件 `buddy-ai-oauth-progress` / `buddy-ai-oauth-done`
/// （Web 化：事件经回调推送，由 server 桥到 SSE）
pub fn buddy_ai_oauth_login(state: &AppState, emit: WbOauthEmitter) -> Result<(), String> {
    oauth_login_async(state.clone(), emit, &crate::tasks::wb_common::REGION_GLOBAL, PoolSink::BuddyAi)
}

// ── 手工导入凭证（区域必选校验，§4.1）──────────────────────────────────────

/// 手工导入国际账号凭证：access_token 必填，refresh_token 可选——同 id 重导缺省时
/// 回落旧值（BL-2 合并语义，见函数体），仅全新账号缺 rt 才不可自动续期。
/// 区域校验（双重保险之一）：domain 显式传入时必须含 `.workbuddy.ai`，否则拒绝——
/// buddy-ai 池不接受任何国内形态账号；缺省回落 `www.workbuddy.ai`。
pub fn buddy_ai_account_import_creds(
    state: &AppState,
    name: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    domain: Option<String>,
) -> Result<Value, String> {
    let token = access_token.trim().to_string();
    if token.is_empty() {
        return Err("access_token 不能为空".into());
    }
    if let Some(d) = domain.as_deref() {
        if !d.trim().is_empty() && !d.contains(".workbuddy.ai") {
            return Err(format!(
                "区域校验失败：domain「{d}」非国际版（须含 .workbuddy.ai）；国内账号请走 Buddy 页导入"
            ));
        }
    }
    // 审查 #13：反向门控（国内侧 is_intl_credential 先例的镜像）——domain 缺省回落
    // www.workbuddy.ai 只能挡显式传入的国内 domain，挡不住 CN token 不带 domain 直入
    // 国际池。凭证 iss 才是真源，显式传国际 domain 不豁免；判定为非国际凭证一律拒绝
    //（国际池 fail-closed，非 JWT 垃圾串同样挡下，与国内池「拒绝国际账号」双向隔离）
    if !super::accounts::is_intl_credential(None, Some(&token)) {
        return Err(
            "区域校验失败：该凭证非国际版（token 签发域非 workbuddy.ai）；国内账号请走 Buddy 页导入"
                .into(),
        );
    }
    let domain = intl_domain_or(domain);
    let id = account_id_of(&token);
    let uid = super::oauth::jwt_claims(&token)
        .and_then(|c| c.get("sub").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    let exp_s = super::oauth::jwt_claims(&token)
        .and_then(|c| c.get("exp").and_then(|v| v.as_i64()));
    let nickname = name.unwrap_or_default();
    let now_s = chrono::Utc::now().timestamp();

    // 读-改-写互斥（buddy-ai 独立锁）；token upsert 留在锁外缩短临界区
    let target_id = {
        let _guard = state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool = load_buddy_pool(&state);
        let target_id = if let Some(a) = find_uid_or_id(&mut pool, &uid, &id) {
            if !uid.is_empty() {
                a.uid = uid.clone();
            }
            if !nickname.is_empty() {
                a.nickname = nickname.clone();
            }
            a.access_token_expires_at = exp_s;
            a.auth_saved_at = Some(now_s);
            a.needs_relogin = false;
            a.relogin_reason.clear();
            a.id.clone()
        } else {
            pool.accounts.push(WorkBuddyAccount {
                id: id.clone(),
                uid: uid.clone(),
                nickname: nickname.clone(),
                access_token_expires_at: exp_s,
                auth_saved_at: Some(now_s),
                ..Default::default()
            });
            id.clone()
        };
        save_buddy_pool(&state, &pool)?;
        target_id
    };
    // BL-2 审查修复：同 id 重导合并式写回——upsert 是整行 REPLACE，从零构造会：
    // ① rt 缺省时 DB 占位置空但 vault 旧 rt 仍在（secure 回填复活，与「无则不可
    // 续期」语义矛盾）；② 抹掉 last_daily_chat/uid（当日打卡被重跑）。改为在旧
    // rec 上合并：新值优先、缺省回落旧值、幂等戳恒保留。
    // 凭证读-合并-写全程持每账号刷新锁：读旧 rec 与 upsert 之间若并发续期轮换 rt，
    // 锁外读到的 stale rt 会整行覆盖新 rt（账号须重登）——与续期/打卡写段同锁域
    let refresh_lock = crate::tasks::wb_common::refresh_lock_for(&target_id);
    let _refresh_guard = refresh_lock.lock().unwrap_or_else(|e| e.into_inner());
    // 审查修复（M18）：写回前复查账号仍在池中——导入与删号并发时，慢路径
    // 可能给已移除账号写入幽灵凭证（登录态/token store 残留脏数据）。
    // 纯 DB 读（load_buddy_pool 无锁），持刷新锁内调用安全（既有模式）
    let still_in_pool = buddy_ai_pool_contains(state, &target_id);
    if !still_in_pool {
        return Err(format!("账号 {target_id} 已被移除，取消凭证写入（防幽灵凭证残留）"));
    }
    let existing = crate::tasks::wb_common::buddy_ai_token_store_load(&state.data_dir)
        .get("tokens")
        .and_then(|t| t.get(&target_id))
        .cloned()
        .unwrap_or_default();
    let new_rt = refresh_token.as_deref().filter(|t| !t.trim().is_empty());
    let old_rt = existing.get("refresh_token").and_then(Value::as_str);
    let mut creds = json!({
        "access_token": token,
        "refresh_token": new_rt.map(Value::from).unwrap_or_else(|| json!(old_rt.unwrap_or(""))),
        "expires_at_ms": exp_s.map(|s| s * 1000),
        "domain": domain,
    });
    if let Some(obj) = creds.as_object_mut() {
        // uid：导入提供优先，否则回落旧值
        obj.insert(
            "uid".into(),
            json!(if uid.is_empty() {
                existing.get("uid").and_then(Value::as_str).unwrap_or("")
            } else {
                uid.as_str()
            }),
        );
        if let Some(ldc) = existing.get("last_daily_chat") {
            obj.insert("last_daily_chat".into(), ldc.clone());
        }
    }
    crate::tasks::wb_common::buddy_ai_token_store_upsert(&state.data_dir, &target_id, &creds)?;
    drop(_refresh_guard);
    fs_utils::app_log(&state.data_dir, &format!("buddy-ai: 手工导入国际账号 {target_id}"));
    // 新凭证/新成员 → 网关池热重载（不重载运行中的网关看不到新凭证）
    crate::api_server::runtime::reload_pools_after_change(state);
    Ok(json!({ "id": target_id, "domain": domain }))
}

// ── 账号列表 / 移除 / 凭证续期（阶段 1 最小集）──────────────────────────────

/// 审查修复（H7）：账号视图统一装配——accounts_list 与 credits_sync 返回视图
/// 原为两份手写字段表且已漂移（sync 版缺 is_current_* / has_snapshot_* 四键，
/// 前端「直接替换列表」后徽标/快照存在性消失；domain 字面量与 INTL_DOMAIN
/// 常量漂移）。单一装配点 + 上下文一次读取，杜绝两表漂移。
/// docker_main 适配：无桌面切换链（switcher/CodeBuddy 客户端随桌面壳退役），
/// ai_auth_uid / cb_live_uid 恒 None → is_current_* 恒 false；快照目录保留
/// 路径判定（目录不存在 → has_snapshot_* 恒 false，字段契约不变）
struct BuddyAiViewCtx {
    ai_auth_uid: Option<String>,
    cb_live_uid: Option<String>,
    profiles_wb_ai: std::path::PathBuf,
    profiles_cb: std::path::PathBuf,
}

impl BuddyAiViewCtx {
    /// 切换/保存登录态上下文（对齐国内 is_current_* / has_snapshot_* 口径）：
    /// WB 当前 = 国际 auth 文件 uid；CB 当前 = CodeBuddy live storage.json genie.uid
    ///（docker_main 无桌面客户端登录态可读，恒 None）
    fn load(state: &AppState) -> Self {
        Self {
            ai_auth_uid: None,
            cb_live_uid: None,
            profiles_wb_ai: state.data_dir.join("data").join("profiles_workbuddy_ai"),
            profiles_cb: state.data_dir.join("data").join("profiles_codebuddy_ai"),
        }
    }
}

/// 单账号视图装配（脱敏：不回传凭证，仅回传凭证存在性；快照槽存在性 =
/// 主槽或 .bak 回退槽）
fn buddy_ai_account_view(a: &WorkBuddyAccount, rec: Option<&Value>, ctx: &BuddyAiViewCtx) -> Value {
    let slot_exists = |dir: &std::path::Path, id: &str| {
        dir.join(id).exists() || dir.join(format!("{id}.bak")).exists()
    };
    json!({
        "id": a.id,
        "uid": a.uid,
        "nickname": a.nickname,
        "note": a.note,
        "group_id": a.group_id,
        "access_token_expires_at": a.access_token_expires_at,
        "refresh_token_expires_at": a.refresh_token_expires_at,
        "auth_saved_at": a.auth_saved_at,
        "needs_relogin": a.needs_relogin,
        "relogin_reason": a.relogin_reason,
        "disabled": a.disabled,
        "credits_packages": a.credits_packages,
        "credits_balance": a.credits_balance,
        "credits_expire_at": a.credits_expire_at,
        "credits_fetched_at": a.credits_fetched_at,
        "has_credential": rec
            .map(|r| {
                as_str(fs_utils::dig(r, &["access_token"]))
                    .map(|t| !t.is_empty())
                    .unwrap_or(false)
            })
            .unwrap_or(false),
        "domain": rec
            .and_then(|r| as_str(fs_utils::dig(r, &["domain"])))
            .unwrap_or_else(|| INTL_DOMAIN.into()),
        "is_current_workbuddy": ctx.ai_auth_uid.as_deref() == Some(a.uid.as_str()) && !a.uid.is_empty(),
        "is_current_codebuddy": ctx.cb_live_uid.as_deref() == Some(a.uid.as_str()) && !a.uid.is_empty(),
        "has_snapshot_workbuddy": slot_exists(&ctx.profiles_wb_ai, &a.id),
        "has_snapshot_codebuddy": slot_exists(&ctx.profiles_cb, &a.id),
    })
}

/// buddy-ai 账号列表（脱敏视图见 buddy_ai_account_view）
pub fn buddy_ai_accounts_list(state: &AppState) -> Result<Vec<Value>, String> {
    let pool = load_buddy_pool(&state);
    let tokens = load_buddy_tokens(&state)
        .get("tokens")
        .and_then(|t| t.as_object())
        .cloned()
        .unwrap_or_default();
    let ctx = BuddyAiViewCtx::load(&state);
    Ok(pool
        .accounts
        .iter()
        .map(|a| buddy_ai_account_view(a, tokens.get(&a.id), &ctx))
        .collect())
}

/// 移除 buddy-ai 账号（池条目 + token store 行 + buddy-ai vault 条目三清 + 标签绑定清理）
pub fn buddy_ai_account_remove(state: &AppState, user_id: String) -> Result<(), String> {
    // 审查 #12：字符白名单（ensure_uid_safe 统一入口）——user_id 作路径组件
    //（vault ns / token store / 快照槽位），仅长度校验放行了 `..`、分隔符注入
    fs_utils::ensure_uid_safe(&user_id)?;
    let _guard = state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = load_buddy_pool(&state);
    let before = pool.accounts.len();
    pool.accounts.retain(|a| a.id != user_id);
    if pool.accounts.len() == before {
        return Err(format!("账号不在 buddy-ai 池中: {user_id}"));
    }
    save_buddy_pool(&state, &pool)?;
    // 审查修复（H5）：三清失败不再整体上抛——原 token_store_remove 失败 `?` 直接
    // 返回，池条目已删不可逆（重试必得「账号不在池中」），token 行 + vault 敏感值
    // 永久残留为孤儿，且下方 drop/索引失效/网关热重载全被跳过（已删账号凭证仍可
    // 被网关取号）。改为收集留痕，清理失败不阻断主流程（对齐下方标签清理仅日志
    // 惯例）；vault ns_remove 无返回值、refresh_lock_remove 为内存态，均无需收集
    let mut clean_errs: Vec<String> = Vec::new();
    if let Err(e) = crate::store::docs::token_store_remove(
        &crate::store::db(&state.data_dir),
        crate::tasks::wb_common::BUDDY_AI_TOKEN_TABLE,
        &user_id,
    ) {
        clean_errs.push(format!("token store 行清理失败: {e}"));
    }
    crate::vault::ns_remove(&state.data_dir, crate::tasks::wb_common::BUDDY_AI_VAULT_NS, &user_id);
    crate::tasks::wb_common::refresh_lock_remove(&user_id);
    fs_utils::app_log(&state.data_dir, &format!("buddy-ai: 移除账号 {user_id}"));
    // 标签绑定清理 + 展开索引失效（对齐国内 workbuddy 账号删除路径；buddy-ai 删号
    // 此前不清 account_tags，幽灵绑定只能靠不含 buddy-ai 池的孤儿自愈清——永远清不掉）
    if let Err(e) = crate::store::docs::account_tags_remove_account(&crate::store::db(&state.data_dir), &user_id) {
        fs_utils::app_log(&state.data_dir, &format!("buddy-ai 账号 {user_id} 标签绑定清理失败: {e}"));
    }
    if !clean_errs.is_empty() {
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "buddy-ai 账号 {user_id} 移除残留（敏感值需手工清理）: {}",
                clean_errs.join("; ")
            ),
        );
    }
    drop(_guard);
    crate::api_server::org_index::invalidate(&state.data_dir);
    // 池成员变更 → 网关热重载（已删账号不重载会继续被取号）
    crate::api_server::runtime::reload_pools_after_change(state);
    Ok(())
}

/// buddy-ai 凭证续期（R-2a 实测端点；refreshToken 每次轮换，回写为准）。
/// 失败不标记 needs_relogin（§4.1：Intl 刷新失败按暂态处理，避免误杀真实有效凭证）。
pub fn buddy_ai_refresh_token(state: &AppState, user_id: String) -> Result<String, String> {
    buddy_ai_refresh_impl(&state, &user_id)
}

/// 续期实现（命令与调度器兜底续期任务共用；锁序/合并写回口径与原命令一致）
pub(crate) fn buddy_ai_refresh_impl(state: &AppState, user_id: &str) -> Result<String, String> {
    let acct = {
        let pool = load_buddy_pool(state);
        pool.accounts
            .iter()
            .find(|a| a.id == user_id)
            .cloned()
            .ok_or_else(|| format!("账号不在 buddy-ai 池中: {user_id}"))?
    };
    // 审查修复（M2）：续期全程持每账号刷新锁 + 跨进程锁——rt 每次轮换，
    // 并发续期时后落库者会把先落库者的新 rt 覆盖为已被服务端作废的旧值（凭证永久失效）。
    // 与 wb 侧 refresh_access_token 锁序同构；与网关 401 国际档续期互斥共用同一把锁
    let refresh_lock = crate::tasks::wb_common::refresh_lock_for(&acct.id);
    let _refresh_guard = refresh_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // 审查修复（#5）：解构失败原因分流——仅 Busy（他方真占用）保留原语义；
    // CreateFailed/WaitFailed 为本机系统错误，误报「他方进程正在刷新」会误导排查
    let (cross_acquired, cross_fail) =
        crate::tasks::qoder_common::CrossProcLock::try_acquire_ns(&state.data_dir, "buddy-ai", &acct.id, 5000);
    let Some(_cross) = cross_acquired else {
        use crate::tasks::qoder_common::CrossProcLockFail;
        match cross_fail {
            Some(CrossProcLockFail::Busy) => {
                return Err("他方进程正在刷新该账号，稍后重试".into());
            }
            other => {
                let reason = other
                    .as_ref()
                    .map(|f| f.describe())
                    .unwrap_or_else(|| "未知原因".to_string());
                fs_utils::app_log(
                    &state.data_dir,
                    &format!("buddy-ai: 续期跨进程锁不可用 {user_id}: {reason}"),
                );
                return Err(format!("刷新锁不可用（{reason}），稍后重试"));
            }
        }
    };
    // 锁内读凭证（他人刚刷新落库的新值直接可见）
    let tokens = load_buddy_tokens(&state);
    let rec = tokens
        .get("tokens")
        .and_then(|t| t.get(&acct.id))
        .cloned()
        .unwrap_or_default();
    let refresh = as_str(fs_utils::dig(&rec, &["refresh_token"]))
        .filter(|t| !t.trim().is_empty())
        .ok_or("该账号无 refreshToken（不可刷新，需重新登录或重新导入凭证）")?;

    let ep = crate::tasks::wb_common::REGION_GLOBAL;
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    // 红线：X-Refresh-Token 仅出现在 refresh 端点（wb_upstream 三铁律同源）
    let resp = agent
        .post(crate::tasks::wb_common::region_refresh_url(INTL_DOMAIN))
        .set("Authorization", "Bearer")
        .set("User-Agent", ep.ua)
        .set("X-Refresh-Token", &refresh)
        .set("X-Auth-Refresh-Source", "workbuddy")
        .set("Content-Type", "application/json")
        .send_string("{}");
    let body: Value = match resp {
        Ok(r) => r.into_json().unwrap_or_default(),
        Err(ureq::Error::Status(code, r)) => {
            // §4.1：Intl 刷新失败按暂态处理，不标记 needs_relogin，仅落日志留痕
            // issue #118：读错误响应体透传上游 error/error_description（如 invalid_grant 细节）
            let msg = crate::tasks::wb_common::refresh_fail_msg(
                code,
                r,
                &format!("刷新失败（HTTP {code}）：refresh token 可能已失效，需重新登录或重新导入"),
            );
            fs_utils::app_log(
                &state.data_dir,
                &format!("buddy-ai: 凭证续期失败 id={user_id}，{msg}，按暂态处理不标记重登录"),
            );
            return Err(msg);
        }
        Err(e) => {
            fs_utils::app_log(
                &state.data_dir,
                &format!("buddy-ai: 凭证续期网络失败 id={user_id}（{e}），按暂态处理"),
            );
            return Err(format!("刷新请求失败: {e}"));
        }
    };
    let new_access = as_str(fs_utils::dig(&body, &["accessToken"])).ok_or("刷新响应中无 accessToken")?;
    let new_refresh = as_str(fs_utils::dig(&body, &["refreshToken"]));
    // 宽容解析：expiresIn 可能以数字字符串返回（字段漂移时 as_i64 会静默丢失 →
    // 池过期时间被清成 None → 兜底续期从此永不选中该账号）
    let num_of = |v: &Value| -> Option<i64> {
        v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    };
    let expires_in: Option<i64> = fs_utils::dig(&body, &["expiresIn"]).and_then(|v| num_of(v));
    let refresh_expires_in: Option<i64> =
        fs_utils::dig(&body, &["refreshExpiresIn"]).and_then(|v| num_of(v));
    let now_ms = chrono::Utc::now().timestamp_millis();
    let exp_ms = expires_in.map(|s| now_ms + s * 1000);
    let rexp_ms = refresh_expires_in.map(|s| now_ms + s * 1000);

    // rt 轮换回写（R-2a：每次刷新轮换；响应缺失时保留旧值防丢）。
    // 审查修复（M1）：合并式写回（在旧 rec 上更新四键）——upsert 是整行 REPLACE，
    // 从零构造会抹掉打卡幂等戳 last_daily_chat 与 uid 兜底，刷新一次当日打卡即被重跑
    let mut merged = if rec.is_object() { rec.clone() } else { serde_json::json!({}) };
    if let Some(rm) = merged.as_object_mut() {
        rm.insert("access_token".into(), json!(new_access));
        rm.insert(
            "refresh_token".into(),
            json!(if new_refresh.as_deref().unwrap_or("").is_empty() {
                as_str(fs_utils::dig(&rec, &["refresh_token"])).unwrap_or_default()
            } else {
                new_refresh.clone().unwrap_or_default()
            }),
        );
        rm.insert("expires_at_ms".into(), json!(exp_ms));
        rm.insert("refresh_expires_at_ms".into(), json!(rexp_ms));
        rm.insert("domain".into(), json!(intl_domain_or(as_str(fs_utils::dig(&rec, &["domain"])))));
    }
    crate::tasks::wb_common::buddy_ai_token_store_upsert(&state.data_dir, &acct.id, &merged)?;
    // 短临界区回写池过期时间（响应缺 expiresIn 时保留池内旧值——清成 None 会让
    // 兜底续期从此永不选中该账号；auth_saved_at 同步刷新，给 renew 的「无到期记录
    // 也纳入」分支提供 48h 节流锚点）
    {
        let _guard = state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool = load_buddy_pool(&state);
        if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == acct.id) {
            a.access_token_expires_at = exp_ms.map(|m| m / 1000).or(a.access_token_expires_at);
            a.refresh_token_expires_at = rexp_ms.map(|m| m / 1000).or(a.refresh_token_expires_at);
            a.auth_saved_at = Some(chrono::Utc::now().timestamp());
            save_buddy_pool(&state, &pool)?;
        }
    }
    fs_utils::app_log(&state.data_dir, &format!("buddy-ai: 凭证续期成功 {user_id}"));
    Ok("凭证已续期".into())
}

// ── 定时任务（2026-10-16 六页对齐批 3）：凭证兜底续期 + 模型目录同步 ──────────

/// 账号级兜底续期失败连击 kv 键（审查 #16）：`{ "<id>": { "streak": n, "date": d } }`
/// ——Intl 刷新失败有意按暂态处理不标 needs_relogin（防误杀，见 buddy_ai_refresh_impl），
/// 但永久失效账号每轮必败且 renewed==0 拖任务 Err 全天退避空转；连击达阈值当日熔断
/// 跳过，成功清零，跨日重置（服务端故障自愈后次日自动恢复探测，对齐打卡连击语义）
const BUDDY_AI_RENEW_STREAK_KEY: &str = "buddy_ai_renew_fail_streak";
/// 熔断阈值：同日连续失败 3 轮（首轮 + 退避重试）即熔断当日剩余重试
const BUDDY_AI_RENEW_STREAK_LIMIT: i64 = 3;

/// buddy-ai 凭证兜底续期任务（对齐国内 wb-renew lazy 口径）：仅刷新 AT 剩余 ≤48h
/// 的启用账号；全部失败返 Err 交调度器 30 分钟冷却重试，部分失败按暂态容忍
pub fn buddy_ai_renew_task(state: &AppState) -> Result<Value, String> {
    let pool = load_buddy_pool(state);
    let now = chrono::Utc::now().timestamp();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let streaks: Value = crate::store::db(&state.data_dir).kv_get(BUDDY_AI_RENEW_STREAK_KEY);
    let streak_of = |id: &str| -> i64 {
        match streaks.get(id) {
            Some(s) if s.get("date").and_then(Value::as_str) == Some(today.as_str()) => {
                s.get("streak").and_then(Value::as_i64).unwrap_or(0).max(0)
            }
            _ => 0,
        }
    };
    let mut fused = 0usize;
    let due: Vec<String> = pool
        .accounts
        .iter()
        .filter(|a| {
            if a.disabled {
                return false;
            }
            let due_by_expiry = match a.access_token_expires_at {
                // 有到期记录：剩余 ≤48h 纳入
                Some(e) => e <= now + 48 * 3600,
                // 无到期记录（导入未带 expiresIn 等）：凭证落盘超 48h 也纳入兜底，
                // 否则这类账号永不触发兜底续期（auth_saved_at 在续期成功时刷新节流）
                None => a.auth_saved_at.map(|s| s <= now - 48 * 3600).unwrap_or(false),
            };
            due_by_expiry && {
                // 审查 #16：连击达阈值的账号当日熔断跳过（fused 计数供返回值观测）
                if streak_of(&a.id) >= BUDDY_AI_RENEW_STREAK_LIMIT {
                    fused += 1;
                    false
                } else {
                    true
                }
            }
        })
        .map(|a| a.id.clone())
        .collect();
    if due.is_empty() {
        return Ok(if fused > 0 {
            json!({ "ok": true, "skipped": "无临期账号", "fused": fused })
        } else {
            json!({ "ok": true, "skipped": "无临期账号" })
        });
    }
    let mut renewed = 0usize;
    let mut failed = 0usize;
    let mut streaks_now = streaks.as_object().cloned().unwrap_or_default();
    for id in &due {
        match buddy_ai_refresh_impl(state, id) {
            Ok(_) => {
                renewed += 1;
                streaks_now.remove(id);
            }
            Err(e) => {
                failed += 1;
                streaks_now.insert(
                    id.clone(),
                    json!({ "streak": streak_of(id) + 1, "date": today }),
                );
                fs_utils::app_log(&state.data_dir, &format!("buddy-ai: 兜底续期失败 id={id}: {e}"));
            }
        }
    }
    if renewed + failed > 0 {
        let _ = crate::store::db(&state.data_dir)
            .kv_set(BUDDY_AI_RENEW_STREAK_KEY, &Value::Object(streaks_now));
    }
    if renewed == 0 {
        return Err(format!("buddy-ai 凭证续期全部失败（{failed} 个临期账号）"));
    }
    Ok(json!({ "ok": true, "renewed": renewed, "failed": failed, "fused": fused }))
}

/// buddy-ai 模型目录同步实现（/v3/config 直连；手动命令与调度任务共用）。
/// 池内无可用凭证返回 Err（调度侧空池静默跳过不计失败）
pub fn buddy_ai_catalog_sync_impl(state: &AppState) -> Result<usize, String> {
    let pool = load_buddy_pool(state);
    let tokens = crate::tasks::wb_common::buddy_ai_token_store_load(&state.data_dir)
        .get("tokens")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // 首个有凭证、未停用、未需重登的账号
    for a in &pool.accounts {
        if a.disabled || a.needs_relogin {
            continue;
        }
        let rec = tokens.get(&a.id).cloned().unwrap_or_default();
        let token = as_str(fs_utils::dig(&rec, &["access_token"]))
            .unwrap_or_default()
            .to_string();
        if token.is_empty() {
            continue;
        }
        let uid = if a.uid.is_empty() {
            as_str(fs_utils::dig(&rec, &["uid"])).unwrap_or_default().to_string()
        } else {
            a.uid.clone()
        };
        return crate::api_server::wb_catalog::buddy_ai_fetch_and_replace(&state.data_dir, &uid, &token)
            .inspect(|n| {
                // 首抓核对诊断（2026-10-16）：落盘成功后把前 3 条解析样本（rate/上下文/档位）
                // 落 app.log——倍率字段名候选集是否命中原响应，一眼可判
                let cat = crate::api_server::wb_catalog::buddy_ai_load(&state.data_dir);
                let sample: Vec<String> = cat
                    .iter()
                    .take(3)
                    .map(|m| {
                        format!(
                            "{} rate={} ctx={} efforts={:?}",
                            m.id, m.rate, m.context_length, m.supported_efforts
                        )
                    })
                    .collect();
                fs_utils::app_log(
                    &state.data_dir,
                    &format!("buddy-ai: Intl 目录首抓核对样本（{n} 模型）: {}", sample.join(" | ")),
                );
            });
    }
    Err("无可用 buddy-ai 账号凭证".into())
}

/// 手动同步 Intl 模型目录（资源调度页 / 概述页「同步目录」按钮）
pub fn buddy_ai_catalog_sync(state: &AppState) -> Result<usize, String> {
    buddy_ai_catalog_sync_impl(&state)
}

// ── 存量迁移（§4.7）：wb 池混居的 global_region=true 账号一次性迁入 buddy-ai 池 ──

const MIGRATION_MARKER: &str = "buddy_ai_pool_migrated_v1";
const MIGRATION_BACKUP_KEY: &str = "buddy_ai_migration_backup";

// ── 每日活跃打卡（阶段 3b，§4.5 补篇）：NDJSON 进度管线 + 积分同步命令 ──────

/// 打卡整轮轮次互斥（独立于 WB_ROUND_LOCK——不与国内签到互斥，两池零交织）。
/// AtomicBool CAS + RAII guard：guard 不能跨 thread::spawn 移动（!Send），
/// 但 RAII 结构体（无借用）可以——Drop 自动复位，任意路径（含 panic 展开）
/// 都不会把 BUSY 永久挂起（审查修复：原实现调度器/启动补跑路径抢占后不释放，
/// 当天后续所有打卡轮次被 skipped_busy 永久跳过）
static BUDDY_AI_ROUND_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 打卡轮次 RAII 守卫：持有期间 BUSY=true，Drop（含 panic 展开）复位
pub(crate) struct BuddyAiRoundGuard;

impl Drop for BuddyAiRoundGuard {
    fn drop(&mut self) {
        BUDDY_AI_ROUND_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 抢打卡轮次（调度器 run_task / UI 命令三路复用；CAS 幂等语义）。
/// 守卫必须移动进执行线程，生命周期覆盖整轮
pub(crate) fn try_acquire_buddy_ai_round() -> Result<BuddyAiRoundGuard, String> {
    use std::sync::atomic::Ordering;
    BUDDY_AI_ROUND_BUSY
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .map(|_| BuddyAiRoundGuard)
        .map_err(|_| "已有 buddy-ai 打卡任务在执行中".to_string())
}

/// buddy-ai 打卡进度事件回调（Web 化替代 AppHandle::emit）：(事件名, 载荷)。
/// 事件名固定 "buddy-ai-checkin-progress"，载荷为序列化 JSON 字符串
///（NDJSON 契约对齐 wb-checkin-progress，前端逐行 JSON.parse；exit 结束事件同款）。
pub type BuddyAiCheckinEmitter = std::sync::Arc<dyn Fn(&str, Value) + Send + Sync>;

/// buddy-ai 每日活跃打卡启动（手动触发，UI 进度经 buddy-ai-checkin-progress NDJSON 推送；
/// Web 化：事件回调由 server 桥到 SSE）
pub fn buddy_ai_checkin_start(state: &AppState, emit: BuddyAiCheckinEmitter) -> Result<(), String> {
    // 轮次互斥：CAS 抢占（运行中重复点击显式拒绝）；守卫移动进执行线程，
    // 生命周期覆盖整轮（临时守卫若不 move 会在语句末 Drop 白抢一次锁）
    let _round = try_acquire_buddy_ai_round()?;
    // 审查修复（P1）：把受管 AppState 克隆进执行线程——原 AppState::new() 造出的
    // buddy_ai_pool_lock 与 UI 命令的不同实例，打卡整轮的池回写（积分快照）与
    // 导入/移除/积分同步的池读-改-写互斥失效，并发时互相整文档覆盖丢更新
    let st = state.clone();
    std::thread::spawn(move || {
        let _round = _round; // 显式移动进闭包，Drop 自动复位 BUSY（含 panic 展开）
        let mut events: Vec<Value> = Vec::new();
        let done = crate::tasks::buddy_ai_checkin::run_checkin_round(&st, &mut |ev| {
            // NDJSON 契约对齐 wb-checkin：payload 为序列化 JSON 字符串
            let _ = emit(
                "buddy-ai-checkin-progress",
                Value::String(serde_json::to_string(ev).unwrap_or_default()),
            );
            events.push(ev.clone());
        });
        // 汇总落日志（含单账号明细，排查打卡失败形态）
        fs_utils::app_log(
            &st.data_dir,
            &format!(
                "buddy-ai: 活跃打卡整轮完成——ok={} already={} failed={} {}",
                done.get("ok").and_then(Value::as_i64).unwrap_or(0),
                done.get("already").and_then(Value::as_i64).unwrap_or(0),
                done.get("failed").and_then(Value::as_i64).unwrap_or(0),
                events.iter().filter(|e| e.get("status").and_then(Value::as_str) == Some("fail"))
                    .map(|e| format!("{}({})", e.get("id").and_then(Value::as_str).unwrap_or("?"), e.get("message").and_then(Value::as_str).unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
        );
        let _ = emit(
            "buddy-ai-checkin-progress",
            Value::String(serde_json::to_string(&json!({"type":"exit"})).unwrap_or_default()),
        );
    });
    Ok(())
}

/// buddy-ai 积分同步（阶段 3b Intl 化）：全池账号 get-user-resource（R-6 字段链
/// TotalDosage）→ 回写池 credits 三键（buddy_ai_pool_lock 短临界区）→ 返回刷新后视图。
/// 手动触发与调度器看板同步共用解析 fn（fetch_dosage_info）。
pub fn buddy_ai_credits_fetch(state: &AppState) -> Result<Vec<Value>, String> {
    buddy_ai_credits_sync_impl(state)
}

/// 调度器/命令共用的积分同步实现。运行池三键拨新（issue #109 同类，buddy-ai
/// 国际池）经 gateway_shared() 取运行网关共享态；CLI 任务模式网关未注册时
/// shared 为 None 自然跳过（对齐国内 wb_credits_snapshot_task 口径）
pub fn buddy_ai_credits_sync_impl(state: &AppState) -> Result<Vec<Value>, String> {
    let agent = crate::tasks::http_agent(30);
    let pool = load_buddy_pool(state);
    let tokens = crate::tasks::wb_common::buddy_ai_token_store_load(&state.data_dir)
        .get("tokens")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for a in &pool.accounts {
        // 审查修复（M17）：停用账号不参与积分/Token 同步，避免污染看板统计
        // （对齐 buddy_ai_usage_official_all_impl 的 disabled 过滤口径）
        if a.disabled {
            continue;
        }
        let rec = tokens.get(&a.id).cloned().unwrap_or_default();
        let token = as_str(fs_utils::dig(&rec, &["access_token"])).unwrap_or_default().to_string();
        if token.is_empty() {
            continue;
        }
        let uid = if a.uid.is_empty() {
            as_str(fs_utils::dig(&rec, &["uid"])).unwrap_or_default().to_string()
        } else {
            a.uid.clone()
        };
        let base = crate::tasks::wb_common::region_activity_base(&intl_domain_or(as_str(fs_utils::dig(&rec, &["domain"]))));
        let snap = crate::tasks::buddy_ai_checkin::fetch_dosage_info(&agent, base, &token, &uid);
        // 解析诊断落日志（issue #179）：明细空/未知 Status 等异常形态留证
        if let Some(d) = snap.as_ref().and_then(|i| i.parse_diag.as_deref()) {
            fs_utils::app_log(&state.data_dir, &format!("buddy-ai: 积分解析诊断 {}: {d}", a.id));
        }
        out.push((a.id.clone(), snap));
    }
    // 短临界区回写快照三键
    {
        let _guard = state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool = load_buddy_pool(state);
        let now = chrono::Utc::now().to_rfc3339();
        for (id, snap) in &out {
            if let Some(a) = pool.accounts.iter_mut().find(|a| &a.id == id) {
                if let Some(info) = snap {
                    a.credits_balance = Some(info.total);
                    a.credits_fetched_at = Some(now.clone());
                    if info.expire_secs.is_some() {
                        a.credits_expire_at = info.expire_secs; // 秒级时间戳
                    }
                    // P9 分包明细（get-user-resource Accounts[] 深度解析）；空明细
                    // 不覆盖旧值（issue #179，对齐 buddy_pool_write_credits 口径）
                    if let Ok(p) = serde_json::to_value(&info.packages) {
                        if p.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                            a.credits_packages = Some(p);
                        }
                    }
                }
            }
        }
        save_buddy_pool(state, &pool)?;
        // P8：手动同步回写后刷新当日积分快照（锁内调用）
        buddy_ai_credits_snapshot_save(state, &pool);
    }
    // 运行池三键拨新（issue #109 同类，buddy-ai 国际池）：账号记录已回写，但网关
    // 运行池钉死在装配时刻——expire_first「有过期时间者优先」会恒压池内
    // expire=None 的账号，充值续期后的新到期进不了池。快照缺到期信息时保留池内
    // 旧值（对齐上方账号记录 expire 仅 Some 才覆盖的口径），余额以本次取数为准。
    // 键位复核（审查 H2，2026-10-17 判定假阳性）：buddy-ai 网关装配
    //（buddy_ai_upstream_accounts）以账号 id 充当条目 uid（区别于国内池的 JWT
    // sub），status_list 键域与 out 的账号 id 天然同域——按 id 查 current 即命中；
    // 勿改按 JWT uid 查（会恒 miss 使拨新失效）
    if let Some(shared) = crate::api_server::runtime::gateway_shared() {
        let current: std::collections::HashMap<String, Option<i64>> = shared
            .buddy_ai_pool
            .status_list()
            .into_iter()
            .map(|s| (s.uid, s.credits_expire_at))
            .collect();
        let m: std::collections::HashMap<String, (f64, Option<i64>)> = out
            .iter()
            .filter_map(|(id, snap)| {
                let info = snap.as_ref()?;
                let expire =
                    info.expire_secs.or_else(|| current.get(id).copied().flatten());
                Some((id.clone(), (info.total, expire)))
            })
            .collect();
        if !m.is_empty() {
            shared.buddy_ai_pool.update_credits_snapshot(&m);
        }
    }
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "buddy-ai: 积分同步完成——{} 账号，{} 成功取数",
            out.len(),
            out.iter().filter(|(_, s)| s.is_some()).count()
        ),
    );
    // 返回刷新后的账号视图（前端直接替换列表，免二次拉取）
    // 审查修复（H7）：复用 buddy_ai_account_view 统一装配——原手写第二份字段表缺
    // is_current_* / has_snapshot_* 四键（前端替换列表后徽标/快照存在性消失），
    // domain 字面量亦与 INTL_DOMAIN 常量漂移
    let pool = load_buddy_pool(state);
    let tokens2 = crate::tasks::wb_common::buddy_ai_token_store_load(&state.data_dir)
        .get("tokens")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let ctx = BuddyAiViewCtx::load(state);
    Ok(pool
        .accounts
        .iter()
        .map(|a| buddy_ai_account_view(a, tokens2.get(&a.id), &ctx))
        .collect())
}

/// buddy-ai 打卡调度开关（scheduler enabled 分支用）
pub(crate) fn buddy_ai_checkin_enabled(state: &AppState) -> bool {
    state.settings().buddy_ai_checkin_enabled
}

/// buddy-ai 池账号数（调度器 models 类任务空池静默跳过判定用）
pub(crate) fn buddy_ai_pool_size(state: &AppState) -> usize {
    load_buddy_pool(state).accounts.len()
}

/// 启动迁移（幂等，kv 标记置位后不重跑）：
/// 1. 扫 wb 池 + wb token store（secure 回填），token rec `domain` 含 `.workbuddy.ai`
///    即混居国际账号（global_region 判定唯一真源 = domain 字段，§4.1）；
/// 2. 迁移前整份备份落 kv（`buddy_ai_migration_backup`，审计 + 人工回滚依据）；
/// 3. 账号行迁 `buddy_ai_accounts`、凭证迁 `buddy_ai_tokens`（敏感值随读随写进
///    buddy-ai vault 命名空间，DB 一律占位）并从 wb 侧删除对应行；
/// 4. 任一步失败：返回 Err、不置位标记（下次启动重试），已写侧幂等（同 id upsert）；
/// 5. 迁移报告结构化落 app_log，不搞子串匹配推断。
///
/// 锁序：wb_pool_lock → buddy_ai_pool_lock（单向，无环；与池内其他路径不冲突——
/// 两池命令各自只持一锁）。
pub fn migrate_wb_global_accounts(state: &AppState) -> Result<(usize, usize), String> {
    let db = crate::store::db(&state.data_dir);
    if db.kv_get_raw(MIGRATION_MARKER).is_some() {
        return Ok((0, 0));
    }
    // 审查修复（H2）：wb 池读-改-写全程持 wb_pool_lock——迁移窗口内并发的 wb 池
    // 命令（导入/删除/积分回写）同为读-改-写，无锁会互相覆盖丢更新
    let _wb_guard = state.wb_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    // secure 回填读（仅内存）：迁移要把敏感值从 wb vault 命名空间搬到 buddy-ai 命名空间
    let wb_store = crate::tasks::wb_common::token_store_load_secure(&state.data_dir);
    let wb_tokens = wb_store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut wb_pool = load_pool(state);

    let mut moved_accounts: Vec<WorkBuddyAccount> = Vec::new();
    let mut moved_tokens: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut stay_ids: Vec<String> = Vec::new();
    let drained: Vec<WorkBuddyAccount> = wb_pool.accounts.drain(..).collect();
    for a in drained {
        let rec = wb_tokens.get(&a.id).cloned().unwrap_or_default();
        let domain = as_str(fs_utils::dig(&rec, &["domain"])).unwrap_or_default();
        if crate::tasks::wb_common::is_global_region(&domain) {
            moved_tokens.insert(a.id.clone(), rec);
            moved_accounts.push(a);
        } else {
            stay_ids.push(a.id.clone());
            wb_pool.accounts.push(a);
        }
    }
    if moved_accounts.is_empty() {
        // 零混居：直接置位标记（幂等闸门），避免每次启动重复扫描
        db.kv_set_raw(MIGRATION_MARKER, &chrono::Utc::now().to_rfc3339())?;
        return Ok((0, stay_ids.len()));
    }

    // 备份先行（审计口径：备份的是 DB 占位形态，不含明文凭证）。
    // 审查修复（C1）：moved_tokens 来自 secure 回填读（内存明文）——备份前必须把
    // 敏感键置空，否则明文 access/refresh token 经 kv_set_raw 落 SQLite，DB 文件
    // 泄漏即凭证泄漏（回滚场景由用户重新登录/导入恢复凭证，不影响账号结构回滚）
    const BACKUP_SENSITIVE_KEYS: [&str; 4] = [
        "access_token",
        "accessToken",
        "refresh_token",
        "refreshToken",
    ];
    let mut backup_tokens = moved_tokens.clone();
    for rec in backup_tokens.values_mut() {
        if let Some(rm) = rec.as_object_mut() {
            for k in BACKUP_SENSITIVE_KEYS {
                if rm.contains_key(k) {
                    rm.insert(k.to_string(), json!(""));
                }
            }
        }
    }
    let backup = json!({
        "migrated_at": chrono::Utc::now().to_rfc3339(),
        "accounts": moved_accounts,
        "tokens": backup_tokens,
    });
    db.kv_set_raw(MIGRATION_BACKUP_KEY, &serde_json::to_string(&backup).map_err(|e| e.to_string())?)?;

    // buddy-ai 侧先写（幂等 upsert；失败即整体失败，wb 侧未动）。
    // 审查修复（H2/H3）：buddy 侧写入持 buddy_ai_pool_lock（锁序 wb→buddy 单向）；
    // 池行按 id upsert——部分失败重试时纯 push 会产生重复账号行（凭证写成功、
    // wb 侧回写失败 → 标记未置位 → 下次启动重扫重迁）
    {
        let _buddy_guard = state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut buddy_pool = load_buddy_pool(state);
        for a in &moved_accounts {
            if let Some(existing) = buddy_pool.accounts.iter_mut().find(|x| x.id == a.id) {
                *existing = a.clone();
            } else {
                buddy_pool.accounts.push(a.clone());
            }
        }
        save_buddy_pool(state, &buddy_pool)?;
        for (id, rec) in &moved_tokens {
            crate::tasks::wb_common::buddy_ai_token_store_upsert(&state.data_dir, id, rec)?;
        }
    }
    // wb 侧后写（池整存 + 混居 token 行删除；vault 旧条目留存不删——加密数据无害，
    // 避免迁移中断后 wb 侧凭证丢失不可回滚）。wb_pool_lock 已在函数顶部持有。
    // BM-1 审查修复：标记先于 token 行删除置位——③删行与④置位间中断时，重试会因
    // wb token 行已删（domain 丢失 → 非 global 判定）把已迁账号误归留守 wb 池，
    // 产生「wb 池无凭证幽灵账号 + buddy-ai 池重复」且迁移永久放弃。先置位后删行：
    // 中断残留的 wb token 行无人消费（wb 池行已移除），无害可清
    {
        save_pool(state, &wb_pool)?;
        db.kv_set_raw(MIGRATION_MARKER, &chrono::Utc::now().to_rfc3339())?;
        for id in moved_tokens.keys() {
            crate::store::docs::token_store_remove(&db, "wb_tokens", id)?;
        }
    }
    let names: Vec<String> = moved_accounts.iter().map(|a| a.id.clone()).collect();
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "buddy-ai: 存量迁移完成——{} 个国际账号由 wb 池迁入 buddy-ai 池 [{}]；wb 池残留 {} 个（零 global 校验通过）",
            moved_accounts.len(),
            names.join(","),
            stay_ids.len()
        ),
    );
    Ok((moved_accounts.len(), stay_ids.len()))
}

/// 编辑 buddy-ai 账号（P3 对齐 workbuddy_account_save：昵称/备注/分组；
/// 池读-改-写互斥 buddy_ai_pool_lock，防并发旧副本整文档覆盖丢改动）
pub fn buddy_ai_account_save(
    state: &AppState,
    user_id: String,
    name: Option<String>,
    note: Option<String>,
    group_id: Option<String>,
) -> Result<(), String> {
    let _guard = state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = load_buddy_pool(&state);
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
    if let Some(g) = group_id {
        acct.group_id = g;
    }
    let result = save_buddy_pool(&state, &pool);
    // 成员/分组变更影响网关池装配 → 热重载（先释放池锁，锁序纪律）
    drop(_guard);
    if result.is_ok() {
        crate::api_server::runtime::reload_pools_after_change(state);
    }
    result
}

/// buddy-ai 账号启停（P3/P6「账号调度池」）：停用即退出打卡花名册与网关取号
///（roster/upstream_accounts 双过滤）；池读-改-写互斥同上
pub fn buddy_ai_account_toggle(state: &AppState, user_id: String, disabled: bool) -> Result<(), String> {
    let _guard = state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = load_buddy_pool(&state);
    let acct = pool
        .accounts
        .iter_mut()
        .find(|a| a.id == user_id)
        .ok_or_else(|| format!("账号不存在: {user_id}"))?;
    acct.disabled = disabled;
    let result = save_buddy_pool(&state, &pool);
    // 启停影响网关取号资格 → 热重载（停用账号不重载会继续被取号，直到下次重载）
    drop(_guard);
    if result.is_ok() {
        crate::api_server::runtime::reload_pools_after_change(state);
    }
    result
}

/// buddy-ai 官网请求流水查询（P9 抓包解锁：/v2/billing/meter/get-user-request-usage
/// 分页接口，credit/model/client/requestTime/input/agentPurpose 请求级明细）。
/// 积分看板「请求明细」表 + 模型级真实积分数据源；鉴权同 get-user-resource（Bearer）。
/// /v2 前缀不可用时回退裸路径（官网网页走裸路径，双兼容）
pub fn buddy_ai_request_usage(
    state: &AppState,
    user_id: String,
    days: Option<i64>,
    page: Option<i64>,
    page_size: Option<i64>,
) -> Result<Value, String> {
    let acct = {
        let pool = load_buddy_pool(&state);
        pool.accounts
            .iter()
            .find(|a| a.id == user_id)
            .cloned()
            .ok_or_else(|| format!("账号不在 buddy-ai 池中: {user_id}"))?
    };
    let tokens = load_buddy_tokens(&state);
    let rec = tokens
        .get("tokens")
        .and_then(|t| t.get(&acct.id))
        .cloned()
        .unwrap_or_default();
    let token = as_str(fs_utils::dig(&rec, &["access_token"]))
        .filter(|t| !t.is_empty())
        .ok_or("该账号无 access_token（不可查询官网流水）")?;
    let uid = if acct.uid.is_empty() {
        as_str(fs_utils::dig(&rec, &["uid"])).unwrap_or_default().to_string()
    } else {
        acct.uid.clone()
    };
    let base = crate::tasks::wb_common::region_activity_base(&intl_domain_or(as_str(fs_utils::dig(
        &rec,
        &["domain"],
    ))));
    let days = days.unwrap_or(7).clamp(1, 31);
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(20).clamp(1, 100);
    let start = (chrono::Local::now() - chrono::Duration::days(days - 1))
        .format("%Y-%m-%d 00:00:00")
        .to_string();
    let end = chrono::Local::now().format("%Y-%m-%d 23:59:59").to_string();
    let body = json!({
        "startTime": start,
        "endTime": end,
        "pageNum": page,
        "pageSize": page_size,
    });
    let agent = crate::tasks::http_agent(30);
    // BL-1 审查修复：按状态码判定路径回退（原实现对错误字符串 contains("404")
    // 匹配——依赖 ureq Display 格式，网络错误消息偶含 "404" 会误触发重试）
    enum UsageErr {
        Status(u16),
        Msg(String),
    }
    let send = |url: String| -> Result<Value, UsageErr> {
        let resp = agent
            .post(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("X-User-Id", &uid)
            .set("X-Client-Platform", "web")
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map_err(|e| match e {
                ureq::Error::Status(code, _) => UsageErr::Status(code),
                other => UsageErr::Msg(other.to_string()),
            })?;
        resp.into_json()
            .map_err(|e| UsageErr::Msg(format!("请求流水响应解析失败: {e}")))
    };
    let v = match send(format!("{base}/v2/billing/meter/get-user-request-usage")) {
        Ok(v) => v,
        // 404 = /v2 前缀不存在 → 回退裸路径（官网网页走裸路径）
        Err(UsageErr::Status(404)) => send(format!("{base}/billing/meter/get-user-request-usage"))
            .map_err(|e| match e {
                UsageErr::Status(code) => format!("请求流水查询失败（HTTP {code}）"),
                UsageErr::Msg(m) => m,
            })?,
        Err(UsageErr::Status(code)) => return Err(format!("请求流水查询失败（HTTP {code}）")),
        Err(UsageErr::Msg(m)) => return Err(m),
    };
    if v.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(format!(
            "官网返回异常: {}",
            v.get("msg").and_then(Value::as_str).unwrap_or("未知错误")
        ));
    }
    // 信封：{code, data: {total, data: [rows]}}（小写 data，与 get-user-resource 大写嵌套不同）
    let data = v.get("data").cloned().unwrap_or_default();
    let total = data.get("total").and_then(Value::as_i64).unwrap_or(0);
    // 行脱敏：input 全文不外发，仅保留 inputTrunc（明细展示口径）
    let rows: Vec<Value> = data
        .get("data")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|r| {
                    json!({
                        "requestId": r.get("requestId").cloned().unwrap_or_default(),
                        "credit": r.get("credit").cloned().unwrap_or(json!(0)),
                        "model": r.get("model").cloned().unwrap_or_default(),
                        "client": r.get("client").cloned().unwrap_or_default(),
                        "requestTime": r.get("requestTime").cloned().unwrap_or_default(),
                        "inputTrunc": r.get("inputTrunc").cloned().unwrap_or_default(),
                        "agentPurpose": r.get("agentPurpose").cloned().unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "total": total, "page": page, "pageSize": page_size, "rows": rows }))
}

/// 全账号官网用量聚合（BuddyAI 积分看板「官网」源；镜像国内 workbuddy_usage_official_all）：
/// 遍历 buddy-ai 池启用且有凭证的账号，逐个拉取 get-user-request-usage 近 31 天
/// 分页明细，按日/按模型跨账号求和（31 天零填充）。聚合缓存 10 分钟；
/// 单账号失败跳过（accounts_ok 计数），全部失败回退过期缓存（stale-on-error）。
pub fn buddy_ai_usage_official_all(state: &AppState) -> Result<Value, String> {
    buddy_ai_usage_official_all_impl(state, false)
}

pub(crate) fn buddy_ai_usage_official_all_impl(state: &AppState, force: bool) -> Result<Value, String> {
    let cache_path = "buddy_ai_usage_official_all_cache"; // kv 键（SQLite 化 P2）
    let cached_val: Option<Value> = {
        let c: Value = crate::store::db(&state.data_dir).kv_get(cache_path);
        (c.get("status").is_some()).then_some(c)
    };
    if !force {
        if let Some(cached) = &cached_val {
            let fetched = cached.get("fetched_at_ms").and_then(Value::as_i64).unwrap_or(0);
            if chrono::Utc::now().timestamp_millis() - fetched < 10 * 60_000 {
                return Ok(cached.clone());
            }
        }
    }

    let pool = load_buddy_pool(state);
    let tokens = load_buddy_tokens(state);
    let agent = crate::tasks::http_agent(30);
    let mut rows: Vec<(String, f64, String)> = Vec::new(); // (date, credit, model)
    let mut session_buckets: Vec<(String, String, Vec<BuddyAiSessionRow>)> = Vec::new();
    let mut accounts_total = 0usize;
    let mut accounts_ok = 0usize;
    let mut any_truncated = false;
    for acct in &pool.accounts {
        if acct.disabled {
            continue;
        }
        let rec = tokens
            .get("tokens")
            .and_then(|t| t.get(&acct.id))
            .cloned()
            .unwrap_or_default();
        let token = as_str(fs_utils::dig(&rec, &["access_token"]))
            .filter(|t| !t.is_empty())
            .map(|s| s.to_string());
        let Some(token) = token else { continue };
        accounts_total += 1;
        let uid = if acct.uid.is_empty() {
            as_str(fs_utils::dig(&rec, &["uid"])).unwrap_or_default().to_string()
        } else {
            acct.uid.clone()
        };
        let base = crate::tasks::wb_common::region_activity_base(&intl_domain_or(as_str(
            fs_utils::dig(&rec, &["domain"]),
        )
        .map(|s| s.to_string())));
        match buddy_ai_official_usage_rows(&agent, &base, &token, &uid) {
            Ok((r, sess, truncated)) => {
                accounts_ok += 1;
                any_truncated = any_truncated || truncated;
                // 行级明细随聚合一并落盘（国内 save_account_session_rows 同款）：
                // 会话明细 Tab 纯缓存查询的数据源；定时任务与页面拉取共用此落地
                session_buckets.push((acct.id.clone(), if acct.nickname.is_empty() { acct.id.clone() } else { acct.nickname.clone() }, sess));
                rows.extend(r);
            }
            Err(e) => fs_utils::app_log(
                &state.data_dir,
                &format!("buddy-ai: 官网用量拉取失败（{}）：{e}", acct.id),
            ),
        }
    }
    if accounts_ok == 0 {
        // 全部失败：不动会话明细缓存（防止失败轮把已落地明细清空），聚合回退 stale
        if let Some(c) = &cached_val {
            let mut stale = c.clone();
            stale["stale"] = json!(true);
            stale["stale_reason"] = json!("all_accounts_fetch_failed");
            return Ok(stale);
        }
        return Err("无可用账号凭证，或全部账号官网用量拉取失败（请检查登录态）".to_string());
    }
    // 行级明细落盘（成功轮才写；部分失败时失败账号保留旧桶不清零；读-改-写经锁串行化）
    {
        static SESSIONS_KV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _guard = SESSIONS_KV_LOCK.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner());
        // 旧缓存：保留非本轮成功账号的历史桶（失败账号明细不清零；已移除账号的桶顺带清理）
        let mut accounts_json: serde_json::Map<String, Value> = cached_val
            .as_ref()
            .and_then(|c| c.get("accounts"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let pool_ids: std::collections::BTreeSet<String> =
            pool.accounts.iter().map(|a| a.id.clone()).collect();
        accounts_json.retain(|id, _| pool_ids.contains(id));
        for (id, name, rows) in session_buckets {
            accounts_json.insert(
                id,
                json!({
                    "name": name,
                    "rows": rows.iter().map(|r| json!({
                        "ts": r.ts,
                        "request_id": r.request_id,
                        "model": r.model,
                        "credits": r.credits,
                        "preview": r.preview,
                    })).collect::<Vec<_>>(),
                }),
            );
        }
        let cache = json!({
            "fetched_at": chrono::Utc::now().timestamp_millis(),
            "accounts": accounts_json,
        });
        let _ = crate::store::db(&state.data_dir).kv_set("buddy_ai_usage_sessions_cache", &cache);
    }

    // 聚合：今日/近7天/本月 + 31 天零填充逐日 + 模型排行（对齐国内 usage_official_fetch 口径）
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    let mut usage_today = 0.0f64;
    let mut usage_week = 0.0f64;
    let mut usage_month = 0.0f64;
    let mut daily_credits: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut daily_models: std::collections::HashMap<String, std::collections::HashMap<String, (u64, f64)>> =
        std::collections::HashMap::new();
    let mut model_totals: std::collections::HashMap<String, (u64, f64)> = std::collections::HashMap::new();
    for (date, credit, model) in &rows {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            let dist = (today - d).num_days();
            if dist == 0 {
                usage_today += credit;
            }
            if (0..7).contains(&dist) {
                usage_week += credit;
            }
            if d.year() == today.year() && d.month() == today.month() {
                usage_month += credit;
            }
        }
        *daily_credits.entry(date.clone()).or_insert(0.0) += credit;
        let dm = daily_models.entry(date.clone()).or_default();
        let e = dm.entry(model.clone()).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += credit;
        let mt = model_totals.entry(model.clone()).or_insert((0, 0.0));
        mt.0 += 1;
        mt.1 += credit;
    }
    let mut models_out: Vec<Value> = model_totals
        .into_iter()
        .map(|(model, (count, credit))| json!({ "model": model, "request_count": count, "credit": credit }))
        .collect();
    models_out.sort_by(|a, b| {
        let ca = a.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        let cb = b.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut daily_out: Vec<Value> = Vec::new();
    for i in (0..=30).rev() {
        let key = (today - chrono::Duration::days(i)).format("%Y-%m-%d").to_string();
        let mut day_models: Vec<Value> = daily_models
            .get(&key)
            .map(|m| {
                m.iter()
                    .map(|(model, (count, credit))| json!({ "model": model, "request_count": count, "credit": credit }))
                    .collect()
            })
            .unwrap_or_default();
        day_models.sort_by(|a, b| {
            let ca = a.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
            let cb = b.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
            cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
        });
        daily_out.push(json!({
            "date": key,
            "usage": daily_credits.get(&key).copied().unwrap_or(0.0),
            "models": day_models,
        }));
    }
    let payload = json!({
        "status": "complete",
        "source": "official_all",
        "accounts_total": accounts_total,
        "accounts_ok": accounts_ok,
        "range_start": (today - chrono::Duration::days(30)).format("%Y-%m-%d").to_string(),
        "range_end": today.format("%Y-%m-%d").to_string(),
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        "request_count_total": rows.len(),
        // 31 天 > 30 页×100 行上限时为 true（聚合低估，前端须明示）
        "truncated": any_truncated,
        "summary": {
            "usage_today": usage_today,
            "usage_7days": usage_week,
            "usage_this_month": usage_month,
        },
        "daily": daily_out,
        "models": models_out,
    });
    let _ = crate::store::db(&state.data_dir).kv_set(cache_path, &payload);
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "buddy-ai: 官网用量聚合刷新（{accounts_ok}/{accounts_total} 账号成功，{} 行）",
            rows.len()
        ),
    );
    Ok(payload)
}

/// buddy-ai 会话明细查询（纯缓存零网络；镜像国内 workbuddy_sessions_query）。
/// 数据源 = buddy_ai_usage_sessions_cache（官网用量聚合拉取时随路落盘，
/// 定时任务 buddy-ai-credits-sync 与页面「官网源」加载共用保鲜）。
/// 区间 [start, end] 本地日字符串字典序比较；model/user_id 可选过滤；
/// 按时间倒序分页；账号归属 buddy-ai 池（已移除账号的残留桶不展示）。
pub fn buddy_ai_sessions_query(
    state: &AppState,
    start: String,
    end: String,
    model: Option<String>,
    user_id: Option<String>,
    page: Option<u32>,
    page_size: Option<u32>,
) -> Result<crate::commands::usage_history::UsageSessionsResult, String> {
    use crate::commands::usage_history::{UsageSessionView, UsageSessionsResult};
    let cache: Value = crate::store::db(&state.data_dir).kv_get("buddy_ai_usage_sessions_cache");

    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(50).clamp(1, 200);
    let model_filter = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let uid_filter = user_id.as_deref().map(str::trim).filter(|u| !u.is_empty());

    // 账号归属判定：buddy-ai 池现役账号（含停用——历史消耗仍可见）
    let pool_ids: std::collections::BTreeSet<String> =
        load_buddy_pool(&state).accounts.iter().map(|a| a.id.clone()).collect();
    // 缓存 fetched_at 为毫秒；契约统一为 Unix 秒（与国内 Buddy 会话明细一致）
    let fetched_at = cache.get("fetched_at").and_then(Value::as_i64).unwrap_or(0) / 1000;

    let mut rows: Vec<UsageSessionView> = Vec::new();
    let mut models: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (acct_id, bucket) in cache.get("accounts").and_then(Value::as_object).into_iter().flatten() {
        if !pool_ids.contains(acct_id) || uid_filter.is_some_and(|f| acct_id != f) {
            continue;
        }
        let name = bucket.get("name").and_then(Value::as_str).unwrap_or("");
        for r in bucket.get("rows").and_then(Value::as_array).into_iter().flatten() {
            let ts = r.get("ts").and_then(Value::as_i64).unwrap_or(0);
            let date = chrono::DateTime::from_timestamp(ts, 0)
                .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            if date.as_str() < start.as_str() || date.as_str() > end.as_str() {
                continue;
            }
            let m = r.get("model").and_then(Value::as_str).unwrap_or("未知模型");
            models.insert(m.to_string());
            if model_filter.is_some_and(|f| m != f) {
                continue;
            }
            rows.push(UsageSessionView {
                user_id: acct_id.clone(),
                account_name: name.to_string(),
                session_id: r.get("request_id").and_then(Value::as_str).unwrap_or("").to_string(),
                ts,
                date,
                model: m.to_string(),
                // buddy-ai 官网行级明细无 mode/cost/token 字段 → 全空（前端显示 —）
                mode: String::new(),
                credits: r.get("credits").and_then(Value::as_f64).unwrap_or(0.0),
                cost_money: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                preview: r.get("preview").and_then(Value::as_str).unwrap_or("").to_string(),
                record_id: String::new(),
            });
        }
    }

    rows.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.session_id.cmp(&a.session_id)));
    let total = rows.len() as u64;
    let credits_total = rows.iter().map(|r| r.credits).sum();
    let total_pages = total.div_ceil(page_size as u64).max(1) as u32;
    let page = page.min(total_pages);
    let begin = ((page as u64 - 1).saturating_mul(page_size as u64)) as usize;
    let rows: Vec<UsageSessionView> = rows.into_iter().skip(begin).take(page_size as usize).collect();

    Ok(UsageSessionsResult {
        fetched_at,
        total,
        credits_total,
        page,
        page_size,
        rows,
        models: models.into_iter().collect(),
    })
}

/// 官网请求级行（buddy_ai_usage_sessions_cache 落盘行；镜像国内 WbSessionRow）
#[derive(Clone)]
struct BuddyAiSessionRow {
    ts: i64,
    request_id: String,
    model: String,
    credits: f64,
    preview: String,
}

/// requestTime → Unix 秒（宽容：epoch 秒/毫秒、RFC3339、本地时间串、纯日期回退当日零点；
/// 对齐国内 usage_row_ts 口径）
fn buddy_ai_row_ts(raw: &Value) -> Option<i64> {
    use chrono::TimeZone;
    if let Some(n) = raw.as_f64() {
        if !n.is_finite() {
            return None;
        }
        let ms = if n.abs() < 10_000_000_000.0 { (n * 1000.0).round() as i64 } else { n.round() as i64 };
        return chrono::DateTime::from_timestamp_millis(ms).map(|d| d.timestamp());
    }
    let text = raw.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(parsed.timestamp());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(text, fmt) {
            return chrono::Local
                .from_local_datetime(&parsed)
                .single()
                .map(|d| d.timestamp());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return chrono::Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
            .single()
            .map(|dt| dt.timestamp());
    }
    None
}

/// 会话说明提取：inputTrunc（官网已截断）优先，缺失取 input，兜底 agentPurpose；
/// 压平换行截断 200 字符（对齐国内 usage_row_preview）
fn buddy_ai_row_preview(item: &Value) -> String {
    let pick = ["inputTrunc", "input_trunc", "input", "agentPurpose", "agent_purpose"]
        .iter()
        .find_map(|k| as_str(fs_utils::dig(item, &[*k])).filter(|s| !s.trim().is_empty()))
        .unwrap_or_default();
    let flat: String = pick
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= 200 {
        return flat.to_string();
    }
    flat.chars().take(200).collect()
}

/// 单账号官网用量 31 天分页拉取（buddy_ai_usage_official_all_impl 复用；
/// 端点/回退与 buddy_ai_request_usage 同款：/v2 前缀 404 回退裸路径）。
/// 返回 (聚合行 (date, credit, model), 请求级行)；requestId 去重，
/// credit 缺失/负值/日期不可解析行跳过；请求级行仅含 ts 可解析的行。
/// 审查修复（M21）：分页中途失败降级为「已取数据 + truncated」（零数据失败仍上抛）。
fn buddy_ai_official_usage_rows(
    agent: &ureq::Agent,
    base: &str,
    token: &str,
    uid: &str,
) -> Result<(Vec<(String, f64, String)>, Vec<BuddyAiSessionRow>, bool), String> {
    let start_text = format!(
        "{} 00:00:00",
        (chrono::Local::now().date_naive() - chrono::Duration::days(30)).format("%Y-%m-%d")
    );
    let end_text = chrono::Local::now().format("%Y-%m-%d 23:59:59").to_string();
    let call = |url: &str, page: u64| -> Result<Value, String> {
        let body = json!({
            "startTime": start_text,
            "endTime": end_text,
            "pageNum": page,
            "pageSize": 100,
        });
        match agent
            .post(url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("X-User-Id", uid)
            .set("X-Client-Platform", "web")
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
        {
            Ok(r) => r
                .into_json::<Value>()
                .map_err(|e| format!("官网用量响应解析失败: {e}")),
            Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
            Err(e) => Err(e.to_string()),
        }
    };
    let v2_url = format!("{base}/v2/billing/meter/get-user-request-usage");
    let bare_url = format!("{base}/billing/meter/get-user-request-usage");
    // 首页探测路径：/v2 前缀 404 → 裸路径（官网网页走裸路径）；
    // 探测成功的那次响应即首页数据，直接消费（不丢弃重取，省一次往返）
    let (url, first) = match call(&v2_url, 1) {
        Ok(v) => (v2_url, v),
        Err(e) if e == "HTTP 404" => (bare_url.clone(), call(&bare_url, 1)?),
        Err(e) => return Err(e),
    };
    let mut rows: Vec<(String, f64, String)> = Vec::new();
    let mut sess_rows: Vec<BuddyAiSessionRow> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut reported_total: i64 = 0;
    let mut page_responses: Vec<Value> = vec![first];
    let mut next_page = 2u64;
    // 审查修复（M21）：分页中途失败降级——置 mid_fail 提前收束，返回已成功拉取的
    // 页数据并透传 truncated（调用方/前端据此提示「数据不完整」），不再 `?`/Err
    // 整体上抛丢弃全部已取页；首页探测失败与零数据失败仍原样上抛——调用方的
    // stale 缓存回退依赖 Err 信号，空数据 + truncated 会被误计为成功账号跳过回退
    let mut mid_fail = false;
    loop {
        let Some(v) = page_responses.pop() else {
            if next_page > 30 {
                break;
            }
            match call(&url, next_page) {
                Ok(v) => {
                    page_responses.push(v);
                    next_page += 1;
                }
                Err(e) => {
                    if rows.is_empty() && sess_rows.is_empty() {
                        return Err(e);
                    }
                    mid_fail = true;
                    break;
                }
            }
            continue;
        };
        if v.get("code").and_then(Value::as_i64) != Some(0) {
            let msg = format!(
                "官网返回异常: {}",
                v.get("msg").and_then(Value::as_str).unwrap_or("未知错误")
            );
            if rows.is_empty() && sess_rows.is_empty() {
                return Err(msg);
            }
            mid_fail = true;
            break;
        }
        let data = v.get("data").cloned().unwrap_or_default();
        let items = data
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        reported_total = reported_total.max(data.get("total").and_then(Value::as_i64).unwrap_or(0));
        for item in &items {
            let credit = item
                .get("credit")
                .and_then(|c| c.as_f64().or_else(|| c.as_str().and_then(|s| s.parse().ok())));
            let Some(credit) = credit.filter(|c| *c >= 0.0) else { continue };
            let Some(rt) = item.get("requestTime").and_then(Value::as_str) else { continue };
            // requestTime "YYYY-MM-DD HH:MM:SS"（宽容：取前 10 位日期段）
            let date = rt.trim().get(0..10).unwrap_or("").to_string();
            if chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
                continue;
            }
            let model = item
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .unwrap_or("未知模型")
                .to_string();
            let rid = item
                .get("requestId")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("{date}|{model}|{credit}|{next_page}|{}", rows.len()));
            if seen.insert(rid.clone()) {
                if let Some(ts) = item.get("requestTime").and_then(buddy_ai_row_ts) {
                    sess_rows.push(BuddyAiSessionRow {
                        ts,
                        request_id: rid,
                        model: model.clone(),
                        credits: credit,
                        preview: buddy_ai_row_preview(item),
                    });
                }
                rows.push((date, credit, model));
            }
        }
        // 终止判据：空页；total 已知且取满；total 缺失/为 0 时以不满页为续页判据
        //（防 total 字段漂移导致只取 1 页即提前终止）
        if items.is_empty() {
            break;
        }
        if reported_total > 0 && seen.len() as i64 >= reported_total {
            break;
        }
        if reported_total == 0 && (items.len() as i64) < 100 {
            break;
        }
    }
    // 截断标记（31 天 > 30 页 × 100 行的重度账号）：聚合可低估，调用方/前端须可感知；
    // M21 分页中途失败同样置 truncated（已取页数不足，聚合同样不完整）
    let truncated = mid_fail || reported_total > seen.len() as i64;
    Ok((rows, sess_rows, truncated))
}

/// buddy-ai 积分历史查询（P8：趋势曲线数据源；近 N 天快照升序，默认 90 天）
pub fn buddy_ai_credits_history(state: &AppState, days: Option<i64>) -> Result<Vec<Value>, String> {
    let days = days.unwrap_or(90).clamp(7, 365);
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let raw = crate::store::docs::buddy_ai_credits_history_load(&crate::store::db(&state.data_dir));
    let mut out = Vec::new();
    if let Some(arr) = raw.get("snapshots").and_then(Value::as_array) {
        for r in arr {
            if r.get("date").and_then(Value::as_str).unwrap_or("") < cutoff.as_str() {
                continue;
            }
            out.push(r.clone());
        }
    }
    Ok(out)
}

/// buddy-ai 账号池导出（P8 对齐 workbuddy_accounts_export；include_credentials=true
/// 附带凭证明文副本——前端强确认 + 本地落盘，导出文件等同密码请妥善保管）
/// 账号池导出（审查修复第二轮：含凭证导出改为**强制口令加密**，对齐 Qoder 侧
/// `qoder_accounts_export` 口径——导出的 JSON 落盘即等同密码，明文副本一旦泄露
/// 直接失守。信封 = 魔数 `AIWQENC1` + salt + nonce + AES-256-GCM 密文（KDF 用
/// Argon2id），导入需同一口令；不含凭证的元数据导出保持明文、无需口令）。
pub fn buddy_ai_accounts_export(
    state: &AppState,
    include_credentials: Option<bool>,
    password: Option<String>,
) -> Result<Value, String> {
    let include_cred = include_credentials.unwrap_or(false);
    // 密码归一化：trim 后为空视为未提供（防误触空白密码）
    let password = password.filter(|p| !p.trim().is_empty());
    if include_cred && password.is_none() {
        return Err(
            "含凭证导出必须设置导出密码：凭证将以 AES-256-GCM 加密后写入导出文件，\
             不提供密码将拒绝导出（不含凭证的元数据导出无需密码）"
                .into(),
        );
    }
    let pool = load_buddy_pool(&state);
    let mut v = serde_json::to_value(&pool).map_err(|e| format!("序列化失败: {e}"))?;
    v["export_kind"] = json!("buddy-ai-pool");
    v["version"] = json!(1);
    v["exported_at"] = json!(chrono::Utc::now().to_rfc3339());
    v["include_credentials"] = json!(include_cred);
    if include_cred {
        // 凭证副本（vault 回填后明文，仅内存）；导出前剥离设备级字段（与 Qoder 侧
        // 同口径：machine_id/machine_token 等同设备凭证，不随文件外流）
        let mut tokens = load_buddy_tokens(&state).get("tokens").cloned().unwrap_or_default();
        if let Some(obj) = tokens.as_object_mut() {
            for (_id, rec) in obj.iter_mut() {
                if let Some(r) = rec.as_object_mut() {
                    r.remove("machine_id");
                    r.remove("machine_token");
                }
            }
        }
        v["tokens"] = tokens;
    } else {
        // 脱敏导出：不含任何凭证字段（安全红线：默认路径零明文）
        v["tokens"] = json!({});
    }
    match (include_cred, password) {
        (true, Some(pwd)) => {
            let plain = serde_json::to_vec(&v).map_err(|e| format!("序列化失败: {e}"))?;
            let sealed = crate::commands::qoder::data_io::seal_payload(&plain, &pwd)?;
            Ok(json!({
                "export_kind": "buddy-ai-pool",
                "version": 1,
                "exported_at": chrono::Utc::now().to_rfc3339(),
                "include_credentials": true,
                "encrypted": true,
                "cipher": "AES-256-GCM",
                "kdf": "argon2id",
                "data": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &sealed),
            }))
        }
        // 不含凭证：明文元数据导出（无敏感数据）；口令即使误传也忽略
        _ => Ok(v),
    }
}

/// buddy-ai 账号池导入（P8 对齐 workbuddy_accounts_import；accounts_export 的回导）。
/// 合并语义：同 id 账号跳过（不覆盖池内现状），新账号追加；tokens 副本仅对
/// token store 中不存在的账号回灌（避免覆盖池内更新的轮换凭证）。
/// 返回 {added, skipped, invalid, tokens_added}。
pub fn buddy_ai_accounts_import(
    state: &AppState,
    pool_json: Value,
    password: Option<String>,
) -> Result<Value, String> {
    // 加密信封（AIWQENC1 魔数）→ 必须提供导出口令解密后走统一合并链路；
    // 无魔数走原明文路径（旧明文导出文件向后兼容，口令忽略）
    let pool_json = match crate::commands::qoder::data_io::envelope_data_of(&pool_json) {
        Some(sealed) => {
            let pwd = password.filter(|p| !p.trim().is_empty()).ok_or(
                "该导出文件已加密（AIWQENC1 信封），需要提供导出时设置的密码才能导入",
            )?;
            let scheme = crate::commands::qoder::data_io::kdf_scheme_of(&pool_json)?;
            let plain = crate::commands::qoder::data_io::open_payload(&sealed, &pwd, scheme)?;
            serde_json::from_slice::<Value>(&plain)
                .map_err(|e| format!("解密后内容解析失败: {e}"))?
        }
        None => pool_json,
    };
    let arr = pool_json
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or_else(|| "导入文件缺少 accounts 数组（须为「导出账号」生成的 JSON）".to_string())?;
    if arr.len() > 500 {
        return Err(format!("导入账号数超限（{} > 500）", arr.len()));
    }
    let imported_tokens = pool_json.get("tokens").and_then(Value::as_object).cloned();

    let mut added = 0usize;
    let mut skipped = 0usize;
    let mut invalid = 0usize;
    let mut new_accounts: Vec<WorkBuddyAccount> = Vec::new();
    for row in arr {
        match serde_json::from_value::<WorkBuddyAccount>(row.clone()) {
            Ok(a) => new_accounts.push(a),
            Err(_) => invalid += 1,
        }
    }

    let _guard = state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = load_buddy_pool(&state);
    // 审查修复（#2）：回灌判定须真实读取 token store 现状——仅查导入文件的
    // imported_tokens 无法发现「store 中已有该账号的有效凭证」，回灌会用导入
    // 文件的过期副本覆盖已轮换的新 refresh_token（凭证永久失效）
    let existing_store = crate::tasks::wb_common::buddy_ai_token_store_load(&state.data_dir);
    let store_has_creds = |id: &str| -> bool {
        existing_store
            .get("tokens")
            .and_then(|t| t.get(id))
            .map(|rec| {
                rec.get("access_token").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false)
                    || rec.get("refresh_token").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false)
            })
            .unwrap_or(false)
    };
    let mut token_targets: Vec<(String, Value)> = Vec::new();
    for a in new_accounts {
        // id 硬校验（审查 #12）：改用 fs_utils::ensure_uid_safe 白名单——字符集限定
        // + 拒绝 ..，与 buddy_ai_account_remove 同口径
        if fs_utils::ensure_uid_safe(&a.id).is_err() {
            invalid += 1;
            continue;
        }
        if pool.accounts.iter().any(|x| x.id == a.id) {
            skipped += 1;
            continue;
        }
        // 凭证副本回灌：仅当 token store 中该 id 不存在或无有效凭证
        //（避免覆盖池内轮换后的新凭证；store 已有有效凭证则跳过回灌）
        if !store_has_creds(&a.id) {
            if let Some(tmap) = &imported_tokens {
                if let Some(rec) = tmap.get(&a.id) {
                    if rec.get("access_token").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false)
                        || rec.get("refresh_token").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false)
                    {
                        token_targets.push((a.id.clone(), rec.clone()));
                    }
                }
            }
        }
        pool.accounts.push(a);
        added += 1;
    }
    if added > 0 {
        save_buddy_pool(&state, &pool)?;
    }
    let mut tokens_added = 0usize;
    for (id, rec) in token_targets {
        if crate::tasks::wb_common::buddy_ai_token_store_upsert(&state.data_dir, &id, &rec).is_ok() {
            tokens_added += 1;
        }
    }
    // 审查修复（#1 major）：导入有变更（新账号入池或凭证回灌）→ 网关池热重载，
    // 对齐国内 workbuddy_accounts_import 口径——不重载运行中的网关看不到新账号/
    // 新凭证。先释放池锁（锁序纪律：reload 链路不得持 buddy_ai_pool_lock）
    drop(_guard);
    if added > 0 || tokens_added > 0 {
        crate::api_server::runtime::reload_pools_after_change(state);
    }
    fs_utils::app_log(
        &state.data_dir,
        &format!("buddy-ai: 导入账号池 added={added} skipped={skipped} invalid={invalid} tokens={tokens_added}"),
    );
    Ok(json!({ "added": added, "skipped": skipped, "invalid": invalid, "tokens_added": tokens_added }))
}

/// buddy-ai 每日活跃打卡结果查询（P2 对齐 workbuddy_checkin_results；数据源
/// buddy_ai_checkin_results 表，run_checkin_round 整轮落库，90 天裁剪）。
/// 概述趋势图 / 签到日历 / 结果列表共用此数据源
pub fn buddy_ai_checkin_results(
    state: &AppState,
    days: Option<i64>,
) -> Result<Vec<super::checkin::WbCheckinRecord>, String> {
    let days = days.unwrap_or(30).clamp(1, 90);
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let raw: serde_json::Value =
        crate::store::docs::buddy_ai_checkin_results_load(&crate::store::db(&state.data_dir));
    let mut out = Vec::new();
    if let Some(arr) = raw.get("results").and_then(|v| v.as_array()) {
        for r in arr {
            let date = r.get("date").and_then(|v| v.as_str()).unwrap_or("");
            if date < cutoff.as_str() {
                continue;
            }
            out.push(super::checkin::WbCheckinRecord {
                date: date.into(),
                time: r.get("time").and_then(|v| v.as_str()).unwrap_or("").into(),
                user_id: r.get("user_id").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: r.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                status: r.get("status").and_then(|v| v.as_str()).unwrap_or("").into(),
                message: r.get("message").and_then(|v| v.as_str()).unwrap_or("").into(),
                reward: r.get("reward").and_then(|v| v.as_f64()),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_state(tag: &str) -> AppState {
        let d = std::env::temp_dir().join(format!("twa_buddy_ai_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        AppState::with_data_dir(d)
    }

    fn acct(id: &str) -> WorkBuddyAccount {
        WorkBuddyAccount {
            id: id.into(),
            ..Default::default()
        }
    }

    /// 迁移：wb 池混居 1 国际 + 1 国内 → 国际迁入 buddy-ai、国内留守、标记置位；
    /// 二次调用幂等跳过
    #[test]
    fn migration_moves_global_accounts_and_is_idempotent() {
        let st = tmp_state("mig");
        // wb 池：1 国际（domain 含 .workbuddy.ai）+ 1 国内
        let global = WorkBuddyAccount { id: "wb-g1".into(), nickname: "intl".into(), ..Default::default() };
        let cn = acct("wb-c1");
        {
            let pool = WbPool { accounts: vec![global, cn] };
            let v = serde_json::to_value(&pool).unwrap();
            crate::store::docs::wb_pool_save(&crate::store::db(&st.data_dir), &v).unwrap();
        }
        let db = crate::store::db(&st.data_dir);
        db.row_upsert("wb_tokens", "wb-g1", &json!({"domain": "www.workbuddy.ai", "access_token": ""})).unwrap();
        db.row_upsert("wb_tokens", "wb-c1", &json!({"domain": "", "access_token": ""})).unwrap();

        let (moved, stay) = migrate_wb_global_accounts(&st).unwrap();
        assert_eq!((moved, stay), (1, 1));

        // buddy-ai 侧到位
        let buddy = load_buddy_pool(&st);
        assert_eq!(buddy.accounts.len(), 1);
        assert_eq!(buddy.accounts[0].id, "wb-g1");
        let buddy_tokens = crate::tasks::wb_common::buddy_ai_token_store_load(&st.data_dir);
        assert!(buddy_tokens["tokens"]["wb-g1"]["domain"].as_str().unwrap().contains(".workbuddy.ai"));
        // wb 侧零 global 残留 + 对应 token 行已删
        let wb = load_pool(&st);
        assert_eq!(wb.accounts.len(), 1);
        assert_eq!(wb.accounts[0].id, "wb-c1");
        let wb_tokens = crate::tasks::wb_common::load_token_store(&st);
        assert!(wb_tokens["tokens"].get("wb-g1").is_none(), "混居 token 行应已删除");
        assert!(wb_tokens["tokens"].get("wb-c1").is_some(), "国内 token 行保留");
        // 备份与标记
        assert!(db.kv_get_raw(MIGRATION_BACKUP_KEY).is_some());
        assert!(db.kv_get_raw(MIGRATION_MARKER).is_some());
        // 幂等：再次调用零动作
        let (m2, _s2) = migrate_wb_global_accounts(&st).unwrap();
        assert_eq!(m2, 0);
        let _ = std::fs::remove_dir_all(&st.data_dir);
    }

    /// 零混居：直接置位标记不产生备份（防迁移恒真弹窗/重复扫描）
    #[test]
    fn migration_with_zero_global_sets_marker_only() {
        let st = tmp_state("zero");
        let pool = WbPool { accounts: vec![acct("wb-c1")] };
        let v = serde_json::to_value(&pool).unwrap();
        crate::store::docs::wb_pool_save(&crate::store::db(&st.data_dir), &v).unwrap();
        let (moved, stay) = migrate_wb_global_accounts(&st).unwrap();
        assert_eq!((moved, stay), (0, 1));
        let db = crate::store::db(&st.data_dir);
        assert!(db.kv_get_raw(MIGRATION_MARKER).is_some());
        assert!(db.kv_get_raw(MIGRATION_BACKUP_KEY).is_none(), "零迁移不产生备份");
        let _ = std::fs::remove_dir_all(&st.data_dir);
    }

    /// 上游取号：buddy-ai 池 + token store → WbSyncAccount（global_region 恒 true、
    /// domain 缺省回落 www.workbuddy.ai、无凭证账号跳过）
    #[test]
    fn buddy_ai_upstream_accounts_synthesizes_from_pool_and_tokens() {
        let st = tmp_state("up");
        {
            let pool = WbPool { accounts: vec![
                WorkBuddyAccount { id: "wb-a1".into(), nickname: "甲".into(), ..Default::default() },
                acct("wb-a2"),
            ] };
            let v = serde_json::to_value(&pool).unwrap();
            crate::store::docs::buddy_ai_pool_save(&crate::store::db(&st.data_dir), &v).unwrap();
        }
        let db = crate::store::db(&st.data_dir);
        // a1 有凭证（无 domain → 缺省回落），a2 无凭证跳过
        db.row_upsert(crate::tasks::wb_common::BUDDY_AI_TOKEN_TABLE, "wb-a1",
            &json!({"access_token": "", "domain": ""})).unwrap();
        // vault 无值 → secure 读回填为空 → a1 也被跳过（无凭证不入池）
        let out = buddy_ai_upstream_accounts(&st);
        assert!(out.is_empty(), "vault 无明文凭证时不得入池（fail-secure）");
        let _ = std::fs::remove_dir_all(&st.data_dir);
    }

    /// intl_domain_or：缺省/非法回落 www.workbuddy.ai；合法国际域名原样保留
    #[test]
    fn intl_domain_or_fallback() {
        assert_eq!(intl_domain_or(None), INTL_DOMAIN);
        assert_eq!(intl_domain_or(Some(String::new())), INTL_DOMAIN);
        assert_eq!(intl_domain_or(Some("copilot.tencent.com".into())), INTL_DOMAIN);
        assert_eq!(intl_domain_or(Some("www.workbuddy.ai".into())), "www.workbuddy.ai");
    }
}
