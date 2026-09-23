//! 网关运行时装配：自 commands/api_server.rs `do_start`（L33-303）下沉，
//! 剥离托盘同步/系统通知/AppHandle；诊断日志语义原样保留。
//! server 以单端口单体运行：build_shared 构造共享状态，路由合并见 aiwork-server。

use std::sync::{Arc, Mutex};

use super::gateway_settings;
use super::pool::{ApiPool, PoolStrategy, WbSyncAccount};
use super::wb_sticky::StickyStore;
use super::{ApiLogger, ApiSharedState, usage};
use crate::fs_utils;
use crate::models::{AccountCooldownsFile, ApiPoolFile, DeviceMap, GroupsFile, RemainingCreditsFile};
use crate::state::AppState;
use crate::store;

/// 汇总网关共享状态（原 do_start 装配段）：网关设置 → 池装配 → 诊断日志 → 共享状态。
/// 不负责监听绑定（由调用方 merge build_router 后自行 axum::serve）。
pub fn build_shared(state: &AppState) -> Arc<ApiSharedState> {
    // 网关设置（§8.1/§9.2）：port / default_model 读 data/api_gateway_settings.json；
    // load 已将空 default_model 兜底为内置默认
    let gw = gateway_settings::load(&state.data_dir);
    let port = gw.port;
    let default_model = gw.default_model;

    // 读取账号数据、冷却状态、剩余积分（账号经 vault 解密还原明文 jwt）
    let accounts = crate::vault::load_accounts(state);
    // SQLite 化（P2）：api_pool.json → kv `api_pool`
    let pool_file: ApiPoolFile = store::db(&state.data_dir).kv_get("api_pool");
    // SQLite 化（P3）：groups/cooldowns/remaining_credits/device_map 经 store 读取
    let groups_file: GroupsFile = store::docs::groups_load(&store::db(&state.data_dir));
    let cooldowns_file: AccountCooldownsFile =
        store::docs::account_cooldowns_load(&store::db(&state.data_dir));
    let credits_file: RemainingCreditsFile =
        store::docs::remaining_credits_load(&store::db(&state.data_dir));
    // 诊断口径与池装配一致：通用积分余额/到期（老缓存账号回退混合口径）
    let merged_expires = merge_pool_expire_times(&credits_file);

    // 调度策略（T10）：api_pool.json.strategy，空/未知值回退 expire_first
    let strategy = PoolStrategy::parse(&pool_file.strategy);

    // ===== 启动诊断日志：详细记录账号池资源情况 =====
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-账号池诊断: total_accounts={} enabled_in_pool={} strategy={} group_filter={} cooldowns={} credits_entries={}",
            accounts.accounts.len(),
            pool_file.enabled_uids.len(),
            strategy.as_str(),
            if pool_file.group_ids.is_empty() { "all".to_string() } else { pool_file.group_ids.join(",") },
            cooldowns_file.cooldowns.len(),
            credits_file.credits.len(),
        ),
    );

    // 逐账号诊断：哪些会被加入池，哪些会被跳过及原因
    let enabled_set: std::collections::HashSet<&str> =
        pool_file.enabled_uids.iter().map(|s| s.as_str()).collect();
    let group_filter: Option<std::collections::HashSet<&str>> = if pool_file.group_ids.is_empty() {
        None
    } else {
        Some(pool_file.group_ids.iter().map(|s| s.as_str()).collect())
    };
    for a in &accounts.accounts {
        let uid = a.user_id.as_deref().unwrap_or("(none)");
        let name = &a.name;
        if !enabled_set.contains(uid) {
            fs_utils::app_log(
                &state.data_dir,
                &format!("  账号池跳过: name={} uid={} reason=not_in_enabled_list", name, uid),
            );
        } else if !group_filter.as_ref().map_or(true, |f| {
            groups_file.membership.get(uid).map_or(false, |g| f.contains(g.as_str()))
        }) {
            fs_utils::app_log(
                &state.data_dir,
                &format!("  账号池跳过: name={} uid={} reason=not_in_selected_group", name, uid),
            );
        } else {
            // 检查冷却和积分状态
            let cd = cooldowns_file.cooldowns.get(uid);
            let credits = credits_file
                .general
                .get(uid)
                .copied()
                .or_else(|| credits_file.credits.get(uid).copied());
            let expire = merged_expires.get(uid).copied();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let status = if cd.map_or(false, |c| c.error_type == "SessionDead") {
                "SessionDead(disabled)".to_string()
            } else if cd.map_or(false, |c| c.until > 0 && now < c.until) {
                format!("cooldown(remaining={}s)", cd.unwrap().until - now)
            } else if let Some(exp) = expire {
                if exp > 0 && exp < now {
                    "credits_expired".to_string()
                } else if let Some(c) = credits {
                    if c <= 0.0 {
                        "zero_credits".to_string()
                    } else {
                        "healthy".to_string()
                    }
                } else {
                    "healthy(no_credits_info)".to_string()
                }
            } else {
                "healthy(no_expiry)".to_string()
            };
            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "  账号池纳入: name={} uid={} credits={} expire={} status={}",
                    name, uid,
                    credits.map(|c| format!("{:.0}", c)).unwrap_or_else(|| "None".into()),
                    expire.map(|e| e.to_string()).unwrap_or_else(|| "None".into()),
                    status,
                ),
            );
        }
    }

    // 创建池并同步（装配逻辑抽公共函数：build_shared 与凭据变更热重载共用）
    let pool = ApiPool::new();
    pool.set_strategy(strategy);
    let wb_pool = ApiPool::new();
    let (pool_count, wb_uids_len, wb_accounts_total) =
        apply_pool_snapshot(state, &pool, &wb_pool);

    let healthy_count = pool.diagnose().iter().filter(|d| d.reason.starts_with("healthy")).count();
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-池状态: pool_size={} healthy={} port={}",
            pool_count, healthy_count, port,
        ),
    );

    // Buddy 池策略：wb_strategy 独立配置优先；空 = 跟随 Trae 池
    let wb_strategy = PoolStrategy::resolve_wb(&pool_file.strategy, &pool_file.wb_strategy);
    let wb_healthy = wb_pool.diagnose().iter().filter(|d| d.reason.starts_with("healthy")).count();
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-WB上游池: enabled={} accounts={} healthy={} strategy={} whitelist={} (total_accounts={})",
            pool_file.wb_enabled,
            wb_pool.count(),
            wb_healthy,
            wb_strategy.as_str(),
            wb_uids_len,
            wb_accounts_total,
        ),
    );
    wb_pool.set_strategy(wb_strategy);

    // 池为空时给出明确警告：Trae 池空 ≠ 全部资源不可用，Buddy(WB) 池可能正常服务
    let wb_pool_count = wb_pool.count(); // ApiPool 非 Copy：移入 shared 前取值
    let wb_summary = format!(
        "Buddy(WB)池 enabled={} accounts={} healthy={}",
        pool_file.wb_enabled,
        wb_pool_count,
        wb_healthy
    );
    if pool_count == 0 {
        if pool_file.wb_enabled && wb_pool.count() > 0 {
            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "警告: Trae 模型池为空（Trae 池请求不可用），{wb_summary} 可正常服务。\
如需 Trae 池，请在API服务页面勾选 Trae 账号并保存。",
                ),
            );
        } else {
            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "警告: 全部账号池为空或未启用！Trae 池: 0 个账号；{wb_summary}。\
请在API服务页面勾选账号并保存后再启动。",
                ),
            );
        }
    } else if healthy_count == 0 {
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "警告: Trae 模型池中无健康账号（冷却/积分过期/SessionDead），{wb_summary}。\
请检查 Trae 账号状态或清除冷却。",
            ),
        );
    }

    let shared = Arc::new(ApiSharedState {
        pool,
        wb_pool,
        wb_enabled: std::sync::atomic::AtomicBool::new(pool_file.wb_enabled),
        wb_sanitize: std::sync::atomic::AtomicBool::new(true),
        // T5.3/T5.5/T5.6③ 开关（api_pool.json，serde default 兼容旧文件）
        wb_default_thinking: std::sync::atomic::AtomicBool::new(pool_file.wb_default_thinking),
        wb_tool_exec: std::sync::atomic::AtomicBool::new(pool_file.wb_tool_exec),
        wb_bg_downgrade: std::sync::atomic::AtomicBool::new(pool_file.wb_bg_downgrade),
        // F-76/F-77 新开关（api_pool.json，serde default 兼容旧文件）
        wb_longctx_downgrade: std::sync::atomic::AtomicBool::new(pool_file.wb_longctx_downgrade),
        wb_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(pool_file.wb_hedge_threshold_ms),
        account_concurrency_limit: std::sync::atomic::AtomicU32::new(
            pool_file.account_concurrency_limit,
        ),
        pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(pool_file.pool_sticky_ttl_secs),
        wb_sticky: StickyStore::load(&state.data_dir),
        pool_sticky: Mutex::new(std::collections::HashMap::new()),
        model_cooldowns: Mutex::new(std::collections::HashMap::new()),
        default_model,
        data_dir: state.data_dir.clone(),
        total_requests: std::sync::atomic::AtomicU64::new(0),
        inflight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        active_uid: Mutex::new(None),
        last_error: Mutex::new(None),
        logger: ApiLogger::new(state.logs_dir()),
        debug_enabled: std::sync::atomic::AtomicBool::new(false),
        usage: Mutex::new(usage::load(&state.data_dir)),
        usage_dirty: Mutex::new(Vec::new()),
        wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
        wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        // Trae 401 自愈回调（issue #27 方案 B）：AppState 克隆进闭包，走 refresh_jwt_impl
        // 全防护链路（并发锁/冷却/轮换写回/5s 成功去重窗）强制刷新并持久化
        trae_jwt_refresh: {
            let st = state.clone();
            Some(std::sync::Arc::new(move |uid: &str| {
                crate::commands::accounts::refresh_jwt_impl(&st, uid, true)
            })
                as std::sync::Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>)
        },
    });

    // F-76②/F-77 热参数：池并发上限（两池同构生效）+ wb_sticky 显式 TTL
    shared.pool.set_concurrency_limit(pool_file.account_concurrency_limit);
    shared
        .wb_pool
        .set_concurrency_limit(pool_file.account_concurrency_limit);
    shared
        .wb_sticky
        .set_explicit_ttl(pool_file.wb_sticky_ttl_secs as i64);

    shared
}

/// 池到期表装配（issue #28）：通用积分（product_id != 209）最早到期优先，
/// 与 pool_credits 的通用口径对齐。回退规则：
/// - 通用剩余表未覆盖的老缓存账号（升级前未刷新过）→ 回退混合口径 expire_times
/// - 已刷新但通用包均长期有效/耗尽的账号（general 有键而 general_expire_times 无键）
///   → 不回退，无到期约束（混合口径里的 Work 包到期不得泄漏进调度）
fn merge_pool_expire_times(rc: &crate::models::RemainingCreditsFile) -> std::collections::HashMap<String, i64> {
    let mut out: std::collections::HashMap<String, i64> = rc
        .expire_times
        .iter()
        .filter(|(uid, _)| !rc.general.contains_key(*uid))
        .map(|(uid, e)| (uid.clone(), *e))
        .collect();
    for (uid, exp) in &rc.general_expire_times {
        out.insert(uid.clone(), *exp);
    }
    out
}

/// 池装配公共逻辑（build_shared 构建 / 凭据变更热重载共用）：
/// 读取 vault 账号 + 池配置 + 分组 + 冷却 + 积分 + 设备映射，全量重建两池内条目。
/// 返回 (trae 池条目数, wb 白名单长度, wb 账号总数) 供调用方记日志。
pub fn apply_pool_snapshot(state: &AppState, pool: &ApiPool, wb_pool: &ApiPool) -> (usize, usize, usize) {
    let accounts = crate::vault::load_accounts(state);
    let pool_file: ApiPoolFile = store::db(&state.data_dir).kv_get("api_pool");
    let groups_file: GroupsFile = store::docs::groups_load(&store::db(&state.data_dir));
    let cooldowns_file: AccountCooldownsFile =
        store::docs::account_cooldowns_load(&store::db(&state.data_dir));
    let credits_file: RemainingCreditsFile =
        store::docs::remaining_credits_load(&store::db(&state.data_dir));
    let device_map: DeviceMap = store::docs::device_map_load(&store::db(&state.data_dir));
    // 池积分语义 = 通用积分（product_id 208，llm_utils_chat 实际扣减的类别）：
    // 优先取 general 表，账号未重新刷新过缓存时回退旧的总积分表
    let pool_credits: std::collections::HashMap<String, f64> = credits_file
        .credits
        .iter()
        .map(|(uid, c)| (uid.clone(), credits_file.general.get(uid).copied().unwrap_or(*c)))
        .collect();
    pool.sync_from_accounts(
        &accounts.accounts,
        &pool_file.enabled_uids,
        &pool_file.group_ids,
        &groups_file.membership,
        &cooldowns_file.cooldowns,
        &pool_credits,
        &merge_pool_expire_times(&credits_file),
        &device_map,
    );
    let wb_accounts_all = wb_upstream_accounts(state);
    // Buddy 池分组筛选（对齐 Trae 池 T10 语义）：wb_group_ids 非空时仅纳入所选分组的
    // WB 账号，未分组账号不参与；空 = 不限分组
    let wb_group_filter: Option<std::collections::HashSet<&str>> = if pool_file.wb_group_ids.is_empty() {
        None
    } else {
        Some(pool_file.wb_group_ids.iter().map(|s| s.as_str()).collect())
    };
    let wb_accounts: Vec<_> = match &wb_group_filter {
        Some(f) => wb_accounts_all
            .into_iter()
            .filter(|a| f.contains(a.group_id.as_str()))
            .collect(),
        None => wb_accounts_all,
    };
    let wb_uids = effective_wb_uids(&pool_file, &wb_accounts);
    wb_pool.sync_from_wb(&wb_accounts, &wb_uids);
    (pool.count(), wb_uids.len(), wb_accounts.len())
}

/// Buddy 池生效入池白名单（纯函数，便于单测）：显式 wb_enabled_uids 优先；
/// 空时兼容旧数据——旧版 WB 账号混存于共享 enabled_uids（wb- 前缀条目），有则沿用；
/// 两者皆空 = 全部含凭证账号自动入池。
pub fn effective_wb_uids(
    pf: &ApiPoolFile,
    wb_accounts: &[WbSyncAccount],
) -> Vec<String> {
    if !pf.wb_enabled_uids.is_empty() {
        return pf.wb_enabled_uids.clone();
    }
    let legacy: Vec<String> = pf
        .enabled_uids
        .iter()
        .filter(|u| u.starts_with("wb-"))
        .cloned()
        .collect();
    if !legacy.is_empty() {
        return legacy;
    }
    wb_accounts.iter().map(|a| a.uid.clone()).collect()
}

// ── WB 上游取号（自 commands/workbuddy/accounts.rs wb_upstream_accounts 下沉）──

/// wb_accounts 表的最小反序列化视图（仅网关装配所需字段；serde 契约与
/// WorkBuddyAccount 一致——snake_case，无 rename_all）
#[derive(serde::Deserialize, Default)]
struct WbPoolLite {
    #[serde(default)]
    accounts: Vec<WbAcctLite>,
}

#[derive(serde::Deserialize, Default, Clone)]
struct WbAcctLite {
    #[serde(default)]
    id: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    needs_relogin: bool,
    #[serde(default)]
    group_id: String,
    #[serde(default)]
    credits_balance: Option<f64>,
    /// 最早积分包到期（issue #28 调度口径透传）；老数据缺省 None
    #[serde(default)]
    credits_expire_at: Option<i64>,
}

/// 宽容字符串字段提取（对齐 fs_utils::dig 语义的本地 helper）
fn as_str(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// 汇总 WB 上游账号：账号池启用账号 + token store 凭证 → WbSyncAccount。
/// 区域判定（§5.2）：domain 含 `.workbuddy.ai` → Global（chat 全走 www.workbuddy.ai）。
/// 仅纳入有工具侧凭证副本的账号（auth 文件为只读态，不在此兜底）。
pub fn wb_upstream_accounts(state: &AppState) -> Vec<WbSyncAccount> {
    let pool: WbPoolLite =
        serde_json::from_value(store::docs::wb_pool_load(&store::db(&state.data_dir)))
            .unwrap_or_default();
    let tokens = store::docs::wb_token_store_load(&store::db(&state.data_dir))
        .get("tokens")
        .and_then(|t| t.as_object())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for a in &pool.accounts {
        let rec = tokens.get(&a.id).cloned().unwrap_or_default();
        let token = as_str(fs_utils::dig(&rec, &["access_token"])).unwrap_or_default();
        if token.is_empty() {
            continue;
        }
        let domain = as_str(fs_utils::dig(&rec, &["domain"])).unwrap_or_default();
        let eid = as_str(fs_utils::dig(&rec, &["enterprise_id", "enterpriseId"])).unwrap_or_default();
        out.push(WbSyncAccount {
            uid: a.id.clone(),
            name: if a.nickname.is_empty() { a.id.clone() } else { a.nickname.clone() },
            token,
            domain: domain.clone(),
            enterprise_id: eid,
            global_region: domain.contains(".workbuddy.ai"),
            credits: a.credits_balance,
            credits_expire_at: a.credits_expire_at,
            needs_relogin: a.needs_relogin,
            group_id: a.group_id.clone(),
        });
    }
    out
}

// ── 网关共享句柄注册（server 单体常驻运行）──────────────────────────────────

/// 网关共享状态全局注册点：server main 启动时 build_shared 后 set 一次，
/// commands 层（pool_set / oauth_login / cooldown 清除等凭据变更方）据此取用做热重载。
static GATEWAY: std::sync::OnceLock<Arc<ApiSharedState>> = std::sync::OnceLock::new();

/// 注册网关共享状态（重复 set 静默忽略，首注册生效）
pub fn set_gateway_shared(shared: Arc<ApiSharedState>) {
    let _ = GATEWAY.set(shared);
}

/// 已注册的网关共享状态；None = 网关未运行（CLI 任务模式 / 启动早期）
pub fn gateway_shared() -> Option<Arc<ApiSharedState>> {
    GATEWAY.get().cloned()
}

/// 凭据/池配置变更后热重载（原 commands/api_server.rs `reload_pools_if_running` 语义）：
/// 网关在运行时全量重建两池条目；未注册（CLI 模式）时静默跳过。
pub fn reload_pools_after_change(state: &AppState) {
    let Some(shared) = gateway_shared() else { return };
    let (n, wb_uids, wb_total) = apply_pool_snapshot(state, &shared.pool, &shared.wb_pool);
    fs_utils::app_log(
        &state.data_dir,
        &format!("凭据变更热重载: trae_pool={n} wb_uids={wb_uids} wb_accounts={wb_total}"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Buddy 池白名单优先级：显式 > 旧版混存 wb- 前缀 > 全部含凭证账号
    #[test]
    fn effective_wb_uids_priority() {
        let mut pf = ApiPoolFile::default();
        pf.wb_enabled_uids = vec!["wb-a".into()];
        pf.enabled_uids = vec!["uid-1".into(), "wb-old".into()];
        let accs = vec![WbSyncAccount {
            uid: "wb-b".into(),
            name: "b".into(),
            token: "t".into(),
            domain: String::new(),
            enterprise_id: String::new(),
            global_region: false,
            credits: None,
            credits_expire_at: None,
            needs_relogin: false,
            group_id: String::new(),
        }];
        // 显式优先
        assert_eq!(effective_wb_uids(&pf, &accs), vec!["wb-a".to_string()]);
        // 显式为空 → 旧版混存 wb- 条目沿用
        pf.wb_enabled_uids.clear();
        assert_eq!(effective_wb_uids(&pf, &accs), vec!["wb-old".to_string()]);
        // 两者皆空 → 全部含凭证账号
        pf.enabled_uids.clear();
        assert_eq!(effective_wb_uids(&pf, &accs), vec!["wb-b".to_string()]);
    }

    // ── merge_pool_expire_times（issue #28：调度到期 = 通用口径）────────────

    #[test]
    fn merge_expires_prefers_general_table() {
        // 新旧两表同账号并存 → 通用口径胜出（Work 包污染的混合值不参与）
        let mut rc = crate::models::RemainingCreditsFile::default();
        rc.expire_times.insert("u1".into(), 1000);
        rc.general.insert("u1".into(), 90.0);
        rc.general_expire_times.insert("u1".into(), 2000);
        let got = merge_pool_expire_times(&rc);
        assert_eq!(got.get("u1"), Some(&2000));
    }

    #[test]
    fn merge_expires_legacy_fallback_without_general_cache() {
        // 老缓存账号（通用剩余表未覆盖，升级前未刷新过）→ 回退混合口径保底
        let mut rc = crate::models::RemainingCreditsFile::default();
        rc.expire_times.insert("legacy".into(), 1000);
        let got = merge_pool_expire_times(&rc);
        assert_eq!(got.get("legacy"), Some(&1000));
    }

    #[test]
    fn merge_expires_no_fallback_when_refreshed_without_general_expiry() {
        // 已刷新（general 有键）但通用包均长期有效/耗尽（general_expire_times 无键）
        // → 不回退，混合口径里的 Work 包到期不得泄漏进调度
        let mut rc = crate::models::RemainingCreditsFile::default();
        rc.expire_times.insert("u1".into(), 1000);
        rc.general.insert("u1".into(), 0.0);
        let got = merge_pool_expire_times(&rc);
        assert!(got.get("u1").is_none());
    }
}
