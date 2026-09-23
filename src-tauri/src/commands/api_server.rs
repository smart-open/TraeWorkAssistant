use std::sync::{Arc, Mutex};

use tauri::{Manager, State};

use crate::fs_utils;
use crate::models::{
    AccountCooldownsFile, ApiPoolFile, ApiServiceStatus, DeviceMap, PoolStatus,
    RemainingCreditsFile,
};
use crate::state::AppState;

use crate::api_server::models_sync;
use crate::api_server::pool::ApiPool;
use crate::api_server::server::{start_api_server, ApiServerHandle};
use crate::api_server::{ApiLogger, ApiSharedState};

/// 运行时状态：服务器句柄 + 共享状态
pub struct ApiServerRuntime {
    pub handle: ApiServerHandle,
    pub shared: Arc<ApiSharedState>,
    pub started_at: u64,
}

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ==================== 启停命令 ====================

/// 启动 API 服务核心逻辑（页面命令 / 托盘菜单共用）。
/// 成功后同步托盘菜单文本并发送系统通知。
pub async fn do_start(
    app: &tauri::AppHandle,
    state: &AppState,
    runtime: &Mutex<Option<ApiServerRuntime>>,
) -> Result<ApiServiceStatus, String> {
    // 检查是否已运行
    {
        let guard = safe_lock(runtime);
        if guard.is_some() {
            return Err("API 服务已在运行".into());
        }
    }

    // 网关设置（§8.1/§9.2）：port / default_model 改读 data/api_gateway_settings.json；
    // 新文件缺失时从 app_settings.json 旧字段一次性迁移（旧字段保留不删，防回滚）。
    // load 已将空 default_model 兜底为内置默认，无需再 trim 判空
    let gw = crate::api_server::gateway_settings::load(&state.data_dir);
    let port = gw.port;

    // 代理循环说明：ureq 2.12 未启用 proxy-from-env feature，构建 Agent 时
    // 既不读 HTTP(S)_PROXY/NO_PROXY 环境变量、也不读系统代理，Agent 未显式
    // 配置 proxy 即直连，不会形成 127.0.0.1:8899 回环。
    // 旧实现曾进程级 set_var("NO_PROXY","*")：多线程下 setenv 有竞态
    // （Rust 2024 已标 unsafe），且污染 python 签到等子进程的代理行为，已移除
    let default_model = gw.default_model;

    // 读取账号数据、冷却状态、剩余积分（账号经 vault 解密还原明文 jwt）
    let accounts = crate::vault::load_accounts(state);
    // SQLite 化（P2）：api_pool.json → kv `api_pool`
    let pool_file: ApiPoolFile = crate::store::db(&state.data_dir).kv_get("api_pool");
    // SQLite 化（P3）：groups/cooldowns/remaining_credits/device_map 经 store 读取
    let groups_file: crate::models::GroupsFile =
        crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    let cooldowns_file: AccountCooldownsFile =
        crate::store::docs::account_cooldowns_load(&crate::store::db(&state.data_dir));
    let credits_file: RemainingCreditsFile =
        crate::store::docs::remaining_credits_load(&crate::store::db(&state.data_dir));

    // 调度策略（T10）：api_pool.json.strategy，空/未知值回退 expire_first
    let strategy = crate::api_server::pool::PoolStrategy::parse(&pool_file.strategy);

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
            let credits = credits_file.credits.get(uid).copied();
            let expire = credits_file.expire_times.get(uid).copied();
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

    // 创建池并同步（装配逻辑抽公共函数：do_start 与凭据变更热重载共用，见 apply_pool_snapshot）
    let pool = ApiPool::new();
    pool.set_strategy(strategy);
    let wb_pool = ApiPool::new();
    let (pool_count, wb_uids_len, wb_accounts_total) = apply_pool_snapshot(state, &pool, &wb_pool);

    let healthy_count = pool.diagnose().iter().filter(|d| d.reason.starts_with("healthy")).count();
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-池状态: pool_size={} healthy={} port={}",
            pool_count, healthy_count, port,
        ),
    );

    // Buddy 池策略：wb_strategy 独立配置优先；空 = 跟随 Trae 池（与 pool_set 热应用逻辑一致）
    let wb_strategy = crate::api_server::pool::PoolStrategy::resolve_wb(&pool_file.strategy, &pool_file.wb_strategy);
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

    // 池为空时给出明确警告：分别指明是哪个池——Trae 池空 ≠ 全部资源不可用，
    // Buddy(WB) 池可能正常服务（2026-09-20 实测反馈：警告误导排查方向）
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
        wb_sticky: crate::api_server::wb_sticky::StickyStore::load(&state.data_dir),
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
        usage: Mutex::new(crate::api_server::usage::load(&state.data_dir)),
        usage_dirty: Mutex::new(Vec::new()),
        wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
        wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        // Trae 401 自愈回调（issue #27 方案 B）：网关层无 AppState，闭包借 AppHandle
        // 每次取 State 调 refresh_jwt_impl(force=true)（全防护：锁/冷却/轮换/失效标记）
        trae_jwt_refresh: Some({
            let app = app.clone();
            std::sync::Arc::new(move |uid: &str| {
                let st = app.state::<AppState>();
                crate::commands::accounts::refresh_jwt_impl(&st, uid, true)
            })
        }),
    });

    // F-76②/F-77 热参数：池并发上限（两池同构生效）+ wb_sticky 显式 TTL
    shared.pool.set_concurrency_limit(pool_file.account_concurrency_limit);
    shared
        .wb_pool
        .set_concurrency_limit(pool_file.account_concurrency_limit);
    shared
        .wb_sticky
        .set_explicit_ttl(pool_file.wb_sticky_ttl_secs as i64);

    let handle = start_api_server(port, shared.clone()).await?;

    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API 服务已启动: port={} 资源概况: Trae池 accounts={} healthy={} | {}",
            port, pool_count, healthy_count, wb_summary
        ),
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let status = ApiServiceStatus {
        running: true,
        port,
        total_requests: 0,
        active_uid: None,
        last_error: None,
        started_at: Some(now),
    };

    *safe_lock(runtime) = Some(ApiServerRuntime {
        handle,
        shared: shared.clone(),
        started_at: now,
    });

    // 同步托盘菜单文本 + 系统通知（账号数分池展示：Trae 池空 ≠ 无资源，Buddy 池可能正常服务）
    sync_tray_api_text(app, true);
    let notify_body = if pool_file.wb_enabled {
        format!("端口 {port}，Trae 池 {pool_count} 个账号，Buddy 池 {wb_pool_count} 个账号")
    } else {
        format!("端口 {port}，池内 {pool_count} 个账号")
    };
    crate::notify::notify(app, "API 网关已启动", &notify_body);

    Ok(status)
}

#[tauri::command]
pub async fn api_server_start(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> Result<ApiServiceStatus, String> {
    do_start(&app, &state, &runtime).await
}

/// 停止 API 服务核心逻辑（页面命令 / 托盘菜单共用）
pub async fn do_stop(
    app: &tauri::AppHandle,
    state: &AppState,
    runtime: &Mutex<Option<ApiServerRuntime>>,
) -> Result<(), String> {
    let mut guard = safe_lock(runtime);
    if let Some(mut rt) = guard.take() {
        rt.handle.stop();
        // 批次 C/E：停止前排空用量脏队列、api_keys 计数与日志队列（flusher 已停）
        rt.shared.flush_pending_writes();
        fs_utils::app_log(&state.data_dir, "API 服务已停止");
        drop(guard);
        sync_tray_api_text(app, false);
        crate::notify::notify(app, "API 网关已停止", "本地网关已关闭");
    }
    Ok(())
}

#[tauri::command]
pub async fn api_server_stop(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> Result<(), String> {
    do_stop(&app, &state, &runtime).await
}

/// 同步托盘「API 服务」菜单文本（服务未运行显示"启动"，运行中显示"停止"）
fn sync_tray_api_text(app: &tauri::AppHandle, running: bool) {
    if let Some(tray) = app.try_state::<crate::TrayMenu>() {
        let _ = tray
            .api_item
            .set_text(if running { "停止 API 服务" } else { "启动 API 服务" });
    }
}

#[tauri::command]
pub fn api_server_status(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> ApiServiceStatus {
    let guard = safe_lock(&runtime);
    // 端口显示与 do_start 同源（gateway_settings，§8.2），避免双源显示漂移
    let port = crate::api_server::gateway_settings::load(&state.data_dir).port;
    match guard.as_ref() {
        Some(rt) => {
            let total = rt
                .shared
                .total_requests
                .load(std::sync::atomic::Ordering::Relaxed);
            let active = safe_lock(&rt.shared.active_uid).clone();
            let last_err = safe_lock(&rt.shared.last_error).clone();
            ApiServiceStatus {
                running: true,
                port,
                total_requests: total,
                active_uid: active,
                last_error: last_err,
                started_at: Some(rt.started_at),
            }
        }
        None => ApiServiceStatus {
            running: false,
            port,
            total_requests: 0,
            active_uid: None,
            last_error: None,
            started_at: None,
        },
    }
}

// ==================== 池管理命令 ====================

#[tauri::command]
pub fn pool_list(state: State<'_, AppState>) -> ApiPoolFile {
    crate::store::db(&state.data_dir).kv_get("api_pool")
}

/// Buddy 池生效入池白名单（纯函数，便于单测）：显式 wb_enabled_uids 优先；
/// 空时兼容旧数据——旧版 WB 账号混存于共享 enabled_uids（wb- 前缀条目），有则沿用；
/// 两者皆空 = 全部含凭证账号自动入池（对齐 Buddy 页「含凭证账号参与 WB 上游调度」
/// 的设计语义。修复：WB 池此前误用 Trae 共享白名单过滤，而 UI 只能勾选 Trae 账号，
/// WB 池恒空 → Buddy 源模型永远 503 no_healthy_account、双源模型失去跨池兜底）
fn effective_wb_uids(
    pf: &ApiPoolFile,
    wb_accounts: &[crate::api_server::pool::WbSyncAccount],
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

/// 池装配公共逻辑（do_start 构建 / 凭据变更热重载共用）：
/// 读取 vault 账号 + 池配置 + 分组 + 冷却 + 积分 + 设备映射，全量重建两池内条目。
/// 返回 (trae 池条目数, wb 白名单长度, wb 账号总数) 供调用方记日志。
fn apply_pool_snapshot(state: &AppState, pool: &ApiPool, wb_pool: &ApiPool) -> (usize, usize, usize) {
    let accounts = crate::vault::load_accounts(state);
    let pool_file: ApiPoolFile = crate::store::db(&state.data_dir).kv_get("api_pool");
    let groups_file: crate::models::GroupsFile =
        crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
    let cooldowns_file: AccountCooldownsFile =
        crate::store::docs::account_cooldowns_load(&crate::store::db(&state.data_dir));
    let credits_file: RemainingCreditsFile =
        crate::store::docs::remaining_credits_load(&crate::store::db(&state.data_dir));
    let device_map: DeviceMap = crate::store::docs::device_map_load(&crate::store::db(&state.data_dir));
    // 池积分语义 = 通用积分（product_id 208，llm_utils_chat 实际扣减的类别）：
    // 优先取 general 表，账号未重新刷新过缓存时回退旧的总积分表（与 do_start 原装配一致）
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
        &credits_file.expire_times,
        &device_map,
    );
    let wb_accounts_all = crate::commands::workbuddy::wb_upstream_accounts(state);
    // Buddy 池分组筛选（对齐 Trae 池 T10 语义）：wb_group_ids 非空时仅纳入所选
    // 分组的 WB 账号，未分组账号不参与；空 = 不限分组。筛选在装配层先行完成，
    // sync_from_wb 白名单交集语义不变。
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

/// 网关运行中热重载两池（凭据/成员变更联动）：OAuth 重登、refresh_token 刷新、
/// 手动更新 JWT、导入账号、保存账号池后调用；服务未运行时为 no-op。
/// 全量重建修复运行中池的陈旧快照——旧 JWT / SessionDead 禁用 / 冷却 / 积分 /
/// 新勾选成员缺失（此前仅 note_refresh_success 单点回填 JWT，覆盖不了这些场景，
/// 用户实测「刚登录的 JWT 网关还是不行」即此根因）。
pub fn reload_pools_if_running(
    state: &AppState,
    runtime: &Mutex<Option<ApiServerRuntime>>,
) {
    let guard = safe_lock(runtime);
    let Some(rt) = guard.as_ref() else { return };
    let (trae, wb_uids, wb_total) = apply_pool_snapshot(state, &rt.shared.pool, &rt.shared.wb_pool);
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务池热重载(凭据/成员变更联动): trae_pool={trae} wb_whitelist={wb_uids} wb_accounts={wb_total}"
        ),
    );
}

/// pool_set 字段合并（纯函数，便于单测）：未传（None）保留 existing 原值，传值覆盖。
/// 注意 strategy/wb_strategy 的显式空串是合法值（"跟随默认"语义），与 None（未传）区分；
/// group_ids 显式空数组 = 清空分组，None = 保留（语义与其他字段统一）。
fn merge_pool_set(
    existing: &ApiPoolFile,
    uids: Vec<String>,
    strategy: Option<String>,
    wb_strategy: Option<String>,
    group_ids: Option<Vec<String>>,
    wb_group_ids: Option<Vec<String>>,
    wb_enabled: Option<bool>,
    wb_default_thinking: Option<bool>,
    wb_tool_exec: Option<bool>,
    wb_bg_downgrade: Option<bool>,
    wb_longctx_downgrade: Option<bool>,
    wb_hedge_threshold_ms: Option<u64>,
    account_concurrency_limit: Option<u32>,
    pool_sticky_ttl_secs: Option<u64>,
    wb_sticky_ttl_secs: Option<u64>,
    wb_uids: Option<Vec<String>>,
) -> ApiPoolFile {
    // Trae 池白名单剥离 wb- 前缀条目：WB 账号归属独立白名单 wb_enabled_uids，
    // 旧版混存于共享 enabled_uids（历史兼容形态），保存时迁移归位防丢失
    let trae_uids: Vec<String> = uids
        .into_iter()
        .filter(|u| !u.starts_with("wb-"))
        .collect();
    let wb_enabled_uids = wb_uids.unwrap_or_else(|| {
        // 未显式传 WB 白名单（如仅改 Trae 池配置的保存）：迁移 existing 中混存的
        // wb- 条目，避免 Trae 页保存把 Buddy 池成员清空
        let legacy: Vec<String> = existing
            .enabled_uids
            .iter()
            .filter(|u| u.starts_with("wb-"))
            .cloned()
            .collect();
        if existing.wb_enabled_uids.is_empty() && !legacy.is_empty() {
            legacy
        } else {
            existing.wb_enabled_uids.clone()
        }
    });
    ApiPoolFile {
        enabled_uids: trae_uids,
        strategy: strategy.unwrap_or_else(|| existing.strategy.clone()),
        wb_strategy: wb_strategy.unwrap_or_else(|| existing.wb_strategy.clone()),
        group_ids: group_ids.unwrap_or_else(|| existing.group_ids.clone()),
        wb_group_ids: wb_group_ids.unwrap_or_else(|| existing.wb_group_ids.clone()),
        wb_enabled: wb_enabled.unwrap_or(existing.wb_enabled),
        wb_default_thinking: wb_default_thinking.unwrap_or(existing.wb_default_thinking),
        wb_tool_exec: wb_tool_exec.unwrap_or(existing.wb_tool_exec),
        wb_bg_downgrade: wb_bg_downgrade.unwrap_or(existing.wb_bg_downgrade),
        wb_longctx_downgrade: wb_longctx_downgrade.unwrap_or(existing.wb_longctx_downgrade),
        wb_hedge_threshold_ms: wb_hedge_threshold_ms
            .unwrap_or(existing.wb_hedge_threshold_ms),
        account_concurrency_limit: account_concurrency_limit
            .unwrap_or(existing.account_concurrency_limit),
        pool_sticky_ttl_secs: pool_sticky_ttl_secs.unwrap_or(existing.pool_sticky_ttl_secs),
        wb_sticky_ttl_secs: wb_sticky_ttl_secs.unwrap_or(existing.wb_sticky_ttl_secs),
        wb_enabled_uids,
    }
}

/// 批量设置池中的账号 UID 列表 + 调度策略 + 分组筛选（T10）+ WB 上游开关（T2.1）
/// + T5.3 默认深度思考 / T5.5 工具代执行 / T5.6③ 后台任务降级（未传字段保留原值）。
/// + F-76/F-77 热参数：长上下文降档 / 慢请求对冲阈值 / 账号并发上限 /
/// 池粘性 TTL / wb_sticky TTL（未传字段保留原值）。
/// 策略部分热应用：运行中池立即生效（成员/分组变更仍需重启重建池）。
#[tauri::command]
pub fn pool_set(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    uids: Vec<String>,
    strategy: Option<String>,
    wb_strategy: Option<String>,
    group_ids: Option<Vec<String>>,
    wb_group_ids: Option<Vec<String>>,
    wb_enabled: Option<bool>,
    wb_default_thinking: Option<bool>,
    wb_tool_exec: Option<bool>,
    wb_bg_downgrade: Option<bool>,
    wb_longctx_downgrade: Option<bool>,
    wb_hedge_threshold_ms: Option<u64>,
    account_concurrency_limit: Option<u32>,
    pool_sticky_ttl_secs: Option<u64>,
    wb_sticky_ttl_secs: Option<u64>,
    // Buddy 池入池白名单（wb- 前缀账号 id）；None = 保留原值（含旧数据迁移），
    // Some(list) = 覆盖（Buddy 页账号池勾选保存）
    wb_uids: Option<Vec<String>>,
) -> Result<(), String> {
    let existing: ApiPoolFile = crate::store::db(&state.data_dir).kv_get("api_pool");
    let pool_file = merge_pool_set(
        &existing,
        uids,
        strategy,
        wb_strategy,
        group_ids,
        wb_group_ids,
        wb_enabled,
        wb_default_thinking,
        wb_tool_exec,
        wb_bg_downgrade,
        wb_longctx_downgrade,
        wb_hedge_threshold_ms,
        account_concurrency_limit,
        pool_sticky_ttl_secs,
        wb_sticky_ttl_secs,
        wb_uids,
    );
    crate::store::db(&state.data_dir).kv_set("api_pool", &pool_file)?;
    // 热应用：运行中即改内存池策略（Buddy 池空值沿用 Trae 池策略，与启动逻辑一致）
    if let Some(rt) = safe_lock(&runtime).as_ref() {
        rt.shared
            .pool
            .set_strategy(crate::api_server::pool::PoolStrategy::parse(&pool_file.strategy));
        rt.shared.wb_pool.set_strategy(
            crate::api_server::pool::PoolStrategy::resolve_wb(&pool_file.strategy, &pool_file.wb_strategy),
        );
        // F-76②/F-77 热参数即时生效（两池同构）
        rt.shared
            .pool
            .set_concurrency_limit(pool_file.account_concurrency_limit);
        rt.shared
            .wb_pool
            .set_concurrency_limit(pool_file.account_concurrency_limit);
        rt.shared.wb_sticky.set_explicit_ttl(pool_file.wb_sticky_ttl_secs as i64);
        rt.shared.wb_longctx_downgrade.store(
            pool_file.wb_longctx_downgrade,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.wb_hedge_threshold_ms.store(
            pool_file.wb_hedge_threshold_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.account_concurrency_limit.store(
            pool_file.account_concurrency_limit,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared.pool_sticky_ttl_secs.store(
            pool_file.pool_sticky_ttl_secs,
            std::sync::atomic::Ordering::Relaxed,
        );
        // Buddy 资源开关热应用（此前仅启动时读取，改动需重启服务生效）
        rt.shared
            .wb_enabled
            .store(pool_file.wb_enabled, std::sync::atomic::Ordering::Relaxed);
        rt.shared.wb_default_thinking.store(
            pool_file.wb_default_thinking,
            std::sync::atomic::Ordering::Relaxed,
        );
        rt.shared
            .wb_tool_exec
            .store(pool_file.wb_tool_exec, std::sync::atomic::Ordering::Relaxed);
        rt.shared
            .wb_bg_downgrade
            .store(pool_file.wb_bg_downgrade, std::sync::atomic::Ordering::Relaxed);
    }
    // 成员/分组热应用：此前仅策略/参数热生效，成员变更要求重启服务；
    // 现统一走凭据/成员变更联动热重载，保存账号池后立即生效
    reload_pools_if_running(&state, &runtime);
    Ok(())
}

/// 返回运行中池的实时状态（冷却/积分等）；服务未运行时返回空数组
#[tauri::command]
pub fn pool_status(runtime: State<'_, Mutex<Option<ApiServerRuntime>>>) -> Vec<PoolStatus> {
    let guard = safe_lock(&runtime);
    match guard.as_ref() {
        Some(rt) => rt.shared.pool.status_list(),
        None => vec![],
    }
}

/// 返回运行中 WB 池的实时状态（F-77⑤ 可观测：含 per-account inflight 在途计数）；
/// 服务未运行时返回空数组
#[tauri::command]
pub fn wb_pool_status(runtime: State<'_, Mutex<Option<ApiServerRuntime>>>) -> Vec<PoolStatus> {
    let guard = safe_lock(&runtime);
    match guard.as_ref() {
        Some(rt) => rt.shared.wb_pool.status_list(),
        None => vec![],
    }
}

/// 列出 API 日志可用日期列表
#[tauri::command]
pub fn api_logs_list(state: State<'_, AppState>) -> Vec<String> {
    let logger = ApiLogger::new(state.logs_dir());
    logger.list_dates(30)
}

/// 读取指定日期的 API 日志内容
#[tauri::command]
pub fn api_logs_detail(state: State<AppState>, date: String) -> Option<String> {
    let logger = ApiLogger::new(state.logs_dir());
    logger.read_log(&date)
}

/// 按时间段和关键字搜索 API 日志
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiLogSearchOpts {
    pub date: String,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
}

#[tauri::command]
pub fn api_logs_search(state: State<AppState>, opts: ApiLogSearchOpts) -> Option<String> {
    let logger = ApiLogger::new(state.logs_dir());
    logger.search_log(
        &opts.date,
        opts.start_time.as_deref().unwrap_or(""),
        opts.end_time.as_deref().unwrap_or(""),
        opts.keyword.as_deref().unwrap_or(""),
    )
}

/// 切换 API Debug 模式（开启后记录完整请求/响应）
#[tauri::command]
pub fn api_debug_toggle(
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> Result<bool, String> {
    let guard = safe_lock(&runtime);
    match guard.as_ref() {
        Some(rt) => {
            let current = rt
                .shared
                .debug_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let new_val = !current;
            rt.shared
                .debug_enabled
                .store(new_val, std::sync::atomic::Ordering::Relaxed);
            Ok(new_val)
        }
        None => Err("API 服务未运行".into()),
    }
}

/// 查询 API Debug 模式状态
#[tauri::command]
pub fn api_debug_status(
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> bool {
    let guard = safe_lock(&runtime);
    match guard.as_ref() {
        Some(rt) => rt
            .shared
            .debug_enabled
            .load(std::sync::atomic::Ordering::Relaxed),
        None => false,
    }
}

// ==================== 模型列表命令 ====================

/// 读取模型列表（api_models.json，缺失时写入默认列表）
#[tauri::command]
pub fn api_models_list(state: State<'_, AppState>) -> Vec<models_sync::ModelOption> {
    models_sync::load_models(&state.data_dir)
}

/// 从官网配置接口同步模型列表（batch_get_detail_param，不消耗积分）
#[tauri::command]
pub async fn api_models_sync(
    state: State<'_, AppState>,
) -> Result<Vec<models_sync::ModelOption>, String> {
    let data_dir = state.data_dir.clone();
    // 预先在调用方解密账号（vault 依赖 AppState，阻塞线程内不便访问）
    let accounts = crate::vault::load_accounts(&state);
    // 阻塞网络请求放入阻塞线程池，避免卡住异步运行时
    tauri::async_runtime::spawn_blocking(move || models_sync::fetch_official(&data_dir, accounts))
        .await
        .map_err(|e| format!("同步任务执行失败: {e}"))?
}

/// 从 WB 上游模型目录接口同步 wb_model_catalog.json（T5.1/F-37，动态替换；
/// 网关启动时已自动做一次，此命令供手动刷新）。取任一含凭证的 WB 账号。
#[tauri::command]
pub async fn api_wb_catalog_sync(state: State<'_, AppState>) -> Result<usize, String> {
    let data_dir = state.data_dir.clone();
    let accounts = crate::commands::workbuddy::wb_upstream_accounts(&state);
    tauri::async_runtime::spawn_blocking(move || {
        let acct = accounts.first().ok_or("无可用 WB 账号凭证，无法拉取上游目录")?;
        crate::api_server::wb_catalog::fetch_and_replace(
            &data_dir,
            &acct.uid,
            &acct.token,
            &acct.domain,
            &acct.enterprise_id,
            acct.global_region,
        )
    })
    .await
    .map_err(|e| format!("同步任务执行失败: {e}"))?
}

/// 列出 wb_model_catalog.json 中的 WB 模型（Buddy API 服务页展示模型 id/倍率/档位）
#[tauri::command]
pub fn api_wb_catalog_list(state: State<'_, AppState>) -> Vec<crate::api_server::wb_catalog::WbModel> {
    crate::api_server::wb_catalog::load(&state.data_dir)
}

/// 查询最近 N 天的 API 用量统计（Trae 模型请求桶，按日聚合，直接读盘，服务未运行也可查）
#[tauri::command]
pub fn api_usage_stats(state: State<'_, AppState>, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent(&state.data_dir, days, false)
}

/// 查询最近 N 天的 WB 上游用量统计（wb_days 桶，Buddy「API 服务」页专用，与 Trae 侧分账）
#[tauri::command]
pub fn api_wb_usage_stats(state: State<'_, AppState>, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent(&state.data_dir, days, true)
}

/// 查询最近 N 天的自定义模型用量统计（custom_days 桶，API 管理·用量统计「自定义」筛选专用）
#[tauri::command]
pub fn api_custom_usage_stats(state: State<'_, AppState>, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent_in(
        &state.data_dir,
        days,
        crate::api_server::usage::UsageBucket::Custom,
    )
}

// ==================== 多 API Key 命令 ====================

/// 读取 API Key 列表与鉴权开关
#[tauri::command]
pub fn api_keys_list(
    state: State<'_, AppState>,
) -> crate::api_server::api_keys::ApiKeysFile {
    crate::api_server::api_keys::load(&state.data_dir)
}

/// 保存 API Key 列表（整表写盘；每次请求重读文件，改动立即生效）。
/// `auth_disabled` 不传时保留现值（避免整表保存覆盖鉴权开关）。
#[tauri::command]
pub fn api_keys_save(
    state: State<'_, AppState>,
    keys: Vec<crate::api_server::api_keys::ApiKeyEntry>,
    auth_disabled: Option<bool>,
) -> Result<(), String> {
    let prev = crate::api_server::api_keys::load(&state.data_dir);
    let file = crate::api_server::api_keys::ApiKeysFile {
        keys,
        auth_disabled: auth_disabled.unwrap_or(prev.auth_disabled),
    };
    crate::api_server::api_keys::save(&state.data_dir, &file);
    Ok(())
}

// ==================== 统一网关命令（Phase 1 §8.1） ====================

/// 统一模型目录聚合视图（实时派生，不落盘）。
/// `available_only=true`：过滤全部来源不可用的模型。
/// 服务运行中用实时池健康派生 enabled 标记；未运行时放宽（池健康视为可选，
/// wb_enabled 读落盘值）——目录展示不因服务停启而失真（§3.3 #5）
#[tauri::command]
pub fn api_unified_models(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    available_only: Option<bool>,
) -> Vec<crate::api_server::unified_catalog::UnifiedModel> {
    let (wb_enabled, trae_ok, buddy_ok) = match safe_lock(&runtime).as_ref() {
        Some(rt) => {
            let s = &rt.shared;
            (
                s.wb_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
                s.pool.has_selectable(),
                s.wb_pool.has_selectable(),
            )
        }
        None => {
            let pf: ApiPoolFile = crate::store::db(&state.data_dir).kv_get("api_pool");
            (pf.wb_enabled, true, true)
        }
    };
    let data_dir = state.data_dir.clone();
    let mut list = crate::api_server::unified_catalog::unified_models(
        &data_dir,
        wb_enabled,
        trae_ok,
        buddy_ok,
    );
    if available_only.unwrap_or(false) {
        list.retain(|m| m.sources.iter().any(|s| s.enabled));
    }
    list
}

/// 读取全局模型白名单（canonical 归一列表；空 = 不限，issue #26）
#[tauri::command]
pub fn model_whitelist_get(state: State<'_, AppState>) -> Vec<String> {
    crate::api_server::unified_catalog::load_whitelist(&state.data_dir)
}

/// 保存全局模型白名单（canonical 归一 + 去空 + 去重；空列表 = 不限）；
/// 返回归一后的生效值（前端展示以返回值为准）
#[tauri::command]
pub fn model_whitelist_set(
    state: State<'_, AppState>,
    models: Vec<String>,
) -> Result<Vec<String>, String> {
    crate::api_server::unified_catalog::save_whitelist(&state.data_dir, &models)
}

/// 读取调度策略（规范化后视图：非法池名/空优先级已回退默认）
#[tauri::command]
pub fn dispatch_policy_get(
    state: State<'_, AppState>,
) -> crate::api_server::dispatch::DispatchPolicy {
    crate::api_server::dispatch::load_policy(&state.data_dir)
}

/// 保存调度策略；返回规范化后的生效值（前端展示以返回值为准）
#[tauri::command]
pub fn dispatch_policy_set(
    state: State<'_, AppState>,
    policy: crate::api_server::dispatch::DispatchPolicy,
) -> Result<crate::api_server::dispatch::DispatchPolicy, String> {
    crate::api_server::dispatch::save_policy(&state.data_dir, &policy)?;
    Ok(crate::api_server::dispatch::load_policy(&state.data_dir))
}

/// 读取网关设置（port / default_model；缺失时从 app_settings 旧字段一次性迁移）
#[tauri::command]
pub fn gateway_settings_get(
    state: State<'_, AppState>,
) -> crate::api_server::gateway_settings::GatewaySettings {
    crate::api_server::gateway_settings::load(&state.data_dir)
}

/// 保存网关设置（端口改动在下次启动 API 服务后生效；返回规范化后的生效值）
#[tauri::command]
pub fn gateway_settings_set(
    state: State<'_, AppState>,
    settings: crate::api_server::gateway_settings::GatewaySettings,
) -> Result<crate::api_server::gateway_settings::GatewaySettings, String> {
    crate::api_server::gateway_settings::save(&state.data_dir, settings)?;
    Ok(crate::api_server::gateway_settings::load(&state.data_dir))
}

// ==================== 自定义模型资源池（custom_models.json） ====================

/// 自定义模型列表（OpenAI 兼容上游直通；命中即直达，§custom_models）
#[tauri::command]
pub fn custom_models_list(
    state: State<'_, AppState>,
) -> Vec<crate::api_server::custom_models::CustomModel> {
    crate::api_server::custom_models::load(&state.data_dir)
}

/// 保存自定义模型（upsert：id 为空新增并生成 cm- id，存在则整条覆盖；
/// 校验 name/base_url 必填 + 名称 canonical 唯一；返回保存后的完整列表）
#[tauri::command]
pub fn custom_models_save(
    state: State<'_, AppState>,
    model: crate::api_server::custom_models::CustomModel,
) -> Result<Vec<crate::api_server::custom_models::CustomModel>, String> {
    crate::api_server::custom_models::upsert(&state.data_dir, model)
}

/// 删除自定义模型（按 id）；返回是否确有删除
#[tauri::command]
pub fn custom_models_remove(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    crate::api_server::custom_models::remove(&state.data_dir, &id)
}

/// 自定义模型连通性测试：向上游发一条最小 chat 请求（max_tokens=16），
/// 成功返回摘要、失败返回原因（保存/编辑前的前置校验入口）。
/// 阻塞 IO 放 spawn_blocking，外加 30s 总超时（覆盖连接 10s + 首字 10s + 出字余量）。
#[tauri::command]
pub async fn custom_model_test(
    model: crate::api_server::custom_models::CustomModel,
) -> Result<String, String> {
    // 与保存同口径的参数预检：给出明确错误而非透传上游 4xx
    let mut cm = model;
    cm.name = cm.name.trim().to_string();
    cm.base_url = cm.base_url.trim().trim_end_matches('/').to_string();
    if cm.name.is_empty() {
        return Err("请先填写模型名称".into());
    }
    if !cm.base_url.starts_with("http://") && !cm.base_url.starts_with("https://") {
        return Err("API 地址必须以 http:// 或 https:// 开头".into());
    }
    let handle = tokio::task::spawn_blocking(move || crate::api_server::custom_route::probe(&cm));
    match tokio::time::timeout(std::time::Duration::from_secs(30), handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => Err(format!("测试任务失败: {e}")),
        Err(_) => Err("测试超时（30 秒）".into()),
    }
}

/// Trae 模型元数据人工覆盖（L1 覆盖层，键 canonical_id；编辑后聚合视图即时生效）
#[tauri::command]
pub fn trae_model_meta_set(
    state: State<'_, AppState>,
    model: String,
    meta: crate::api_server::unified_catalog::TraeModelMeta,
) -> Result<(), String> {
    crate::api_server::unified_catalog::meta_set(&state.data_dir, &model, meta)
}

/// 读取 Trae 模型元数据人工覆盖（编辑弹框回显用；None = 无人工值，交由自动来源链）
#[tauri::command]
pub fn trae_model_meta_get(
    state: State<'_, AppState>,
    model: String,
) -> Option<crate::api_server::unified_catalog::TraeModelMeta> {
    crate::api_server::unified_catalog::load_meta(&state.data_dir)
        .remove(&crate::api_server::unified_catalog::canonical_id(&model))
}

/// 清除 Trae 模型元数据人工覆盖；返回是否存在过
#[tauri::command]
pub fn trae_model_meta_clear(state: State<'_, AppState>, model: String) -> Result<bool, String> {
    crate::api_server::unified_catalog::meta_clear(&state.data_dir, &model)
}

// ==================== 单元测试：pool_set 合并语义（调度策略收口） ====================

#[cfg(test)]
mod pool_merge_tests {
    use super::merge_pool_set;
    use crate::models::ApiPoolFile;

    /// 模拟已存在的 api_pool.json（各字段均非默认值，验证"保留"是否生效）
    fn existing() -> ApiPoolFile {
        ApiPoolFile {
            enabled_uids: vec!["u1".into()],
            strategy: "weighted".into(),
            wb_strategy: "p2c".into(),
            group_ids: vec!["g1".into()],
            wb_enabled: true,
            wb_default_thinking: true,
            wb_tool_exec: false,
            wb_bg_downgrade: false,
            wb_longctx_downgrade: true,
            wb_hedge_threshold_ms: 8000,
            account_concurrency_limit: 2,
            pool_sticky_ttl_secs: 600,
            wb_sticky_ttl_secs: 3600,
            wb_enabled_uids: Vec::new(),
            wb_group_ids: vec!["wg1".into()],
        }
    }

    #[test]
    fn none_fields_preserve_existing() {
        // 只改成员（uids 必传覆盖），其余未传 → 全部保留原值（含 F-76/F-77 新参数）
        let m = merge_pool_set(
            &existing(),
            vec!["u2".into()],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(m.enabled_uids, vec!["u2".to_string()]);
        assert_eq!(m.strategy, "weighted");
        assert_eq!(m.wb_strategy, "p2c");
        assert_eq!(m.group_ids, vec!["g1".to_string()]);
        // wb_group_ids 未传 → 保留原值
        assert_eq!(m.wb_group_ids, vec!["wg1".to_string()]);
        assert!(m.wb_enabled);
        assert!(m.wb_default_thinking);
        assert!(!m.wb_tool_exec);
        assert!(!m.wb_bg_downgrade);
        // F-76/F-77 新参数未传 → 保留原值
        assert!(m.wb_longctx_downgrade);
        assert_eq!(m.wb_hedge_threshold_ms, 8000);
        assert_eq!(m.account_concurrency_limit, 2);
        assert_eq!(m.pool_sticky_ttl_secs, 600);
        assert_eq!(m.wb_sticky_ttl_secs, 3600);
    }

    #[test]
    fn trae_save_migrates_legacy_wb_uids() {
        // 旧版共享白名单混存 wb- 条目：Trae 页保存（未传 wb_uids）时 wb- 条目
        // 从 enabled_uids 剥离并迁移进 wb_enabled_uids，Buddy 池成员不丢失
        let mut legacy = existing();
        legacy.enabled_uids = vec!["1001".into(), "wb-abc".into(), "1002".into()];
        legacy.wb_enabled_uids = Vec::new();
        let m = merge_pool_set(
            &legacy,
            vec!["1001".into(), "wb-abc".into(), "1002".into(), "wb-def".into()],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        // Trae 白名单剥离 wb- 条目
        assert_eq!(m.enabled_uids, vec!["1001".to_string(), "1002".to_string()]);
        // 仅 existing 混存的 wb- 条目迁移进 WB 白名单；传入名单中的未知 wb- 条目
        // （wb-def，非本次迁移来源）不并入，随剥离丢弃（白名单隔离语义）
        assert_eq!(m.wb_enabled_uids, vec!["wb-abc".to_string()]);
    }

    #[test]
    fn explicit_wb_uids_override() {
        // Buddy 页勾选保存：显式传 wb_uids → 覆盖（不再走旧数据迁移）
        let mut legacy = existing();
        legacy.enabled_uids = vec!["u1".into(), "wb-old".into()];
        let m = merge_pool_set(
            &legacy,
            vec!["u1".into()],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(vec!["wb-new".into()]),
        );
        assert_eq!(m.wb_enabled_uids, vec!["wb-new".to_string()]);
        assert_eq!(m.enabled_uids, vec!["u1".to_string()]);
    }

    #[test]
    fn effective_wb_uids_priority() {
        use super::effective_wb_uids;
        use crate::api_server::pool::WbSyncAccount;
        let acc = |uid: &str| WbSyncAccount {
            uid: uid.into(),
            name: String::new(),
            token: "t".into(),
            domain: String::new(),
            enterprise_id: String::new(),
            global_region: false,
            credits: None,
            needs_relogin: false,
            group_id: String::new(),
        };
        let accounts = vec![acc("wb-a"), acc("wb-b")];
        // ① 显式白名单优先
        let mut pf = existing();
        pf.wb_enabled_uids = vec!["wb-a".into()];
        assert_eq!(effective_wb_uids(&pf, &accounts), vec!["wb-a".to_string()]);
        // ② 旧混存 wb- 条目沿用
        pf.wb_enabled_uids = Vec::new();
        pf.enabled_uids = vec!["1001".into(), "wb-b".into()];
        assert_eq!(effective_wb_uids(&pf, &accounts), vec!["wb-b".to_string()]);
        // ③ 两者皆空 = 全部含凭证账号自动入池（fail-open）
        pf.enabled_uids = vec!["1001".into()];
        assert_eq!(
            effective_wb_uids(&pf, &accounts),
            vec!["wb-a".to_string(), "wb-b".to_string()]
        );
    }

    #[test]
    fn some_fields_override_and_empty_string_is_legal_value() {
        // 显式空串 = "跟随默认"合法值（区别于 None 未传）；显式空数组 = 清空分组
        let m = merge_pool_set(
            &existing(),
            vec![],
            Some("p2c".into()),
            Some("".into()),
            Some(vec![]),
            // wb_group_ids 显式覆盖
            Some(vec!["wg2".into()]),
            Some(false),
            Some(false),
            Some(true),
            Some(true),
            Some(false),
            Some(3000),
            Some(0),
            Some(60),
            Some(120),
            None,
        );
        assert_eq!(m.strategy, "p2c");
        assert_eq!(m.wb_strategy, "");
        assert!(m.group_ids.is_empty());
        assert_eq!(m.wb_group_ids, vec!["wg2".to_string()]);
        assert!(!m.wb_enabled);
        assert!(!m.wb_default_thinking);
        assert!(m.wb_tool_exec);
        assert!(m.wb_bg_downgrade);
        // F-76/F-77 新参数显式传入 → 覆盖
        assert!(!m.wb_longctx_downgrade);
        assert_eq!(m.wb_hedge_threshold_ms, 3000);
        assert_eq!(m.account_concurrency_limit, 0);
        assert_eq!(m.pool_sticky_ttl_secs, 60);
        assert_eq!(m.wb_sticky_ttl_secs, 120);
    }

    #[test]
    fn strategy_only_caller_does_not_touch_wb_and_flags() {
        // Trae 资源调度页收口后只保存成员/分组：不传 strategy/wb_strategy/开关组 → 均保留
        let m = merge_pool_set(
            &existing(),
            vec!["u1".into(), "u3".into()],
            None,
            None,
            Some(vec!["g2".into()]),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(m.enabled_uids.len(), 2);
        assert_eq!(m.group_ids, vec!["g2".to_string()]);
        assert_eq!(m.strategy, "weighted");
        assert_eq!(m.wb_strategy, "p2c");
        assert!(m.wb_enabled);
    }

    #[test]
    fn empty_existing_preserves_nothing_but_fills_defaults() {
        // 旧版 api_pool.json（无策略字段）+ 只传成员：策略落为空串（运行时 parse 回退 expire_first）；
        // F-76/F-77 新参数未传 → serde default 生效（对冲 8s、并发=1、池粘性 300s）
        let m = merge_pool_set(
            &ApiPoolFile::default(),
            vec!["u1".into()],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(m.strategy, "");
        assert_eq!(m.wb_strategy, "");
        assert!(m.group_ids.is_empty());
        assert!(!m.wb_enabled);
        assert!(!m.wb_longctx_downgrade);
        assert_eq!(m.wb_hedge_threshold_ms, 8000);
        assert_eq!(m.account_concurrency_limit, 1);
        assert_eq!(m.pool_sticky_ttl_secs, 300);
        assert_eq!(m.wb_sticky_ttl_secs, 1800);
    }
}
