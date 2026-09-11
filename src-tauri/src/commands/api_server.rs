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

    let settings = state.settings();
    let port = settings.api_port;

    // 代理循环说明：ureq 2.12 未启用 proxy-from-env feature，构建 Agent 时
    // 既不读 HTTP(S)_PROXY/NO_PROXY 环境变量、也不读系统代理，Agent 未显式
    // 配置 proxy 即直连，不会形成 127.0.0.1:8899 回环。
    // 旧实现曾进程级 set_var("NO_PROXY","*")：多线程下 setenv 有竞态
    // （Rust 2024 已标 unsafe），且污染 python 签到等子进程的代理行为，已移除
    let default_model = {
        let m = settings.api_default_model.trim();
        if m.is_empty() {
            crate::api_server::DEFAULT_MODEL.to_string()
        } else {
            m.to_string()
        }
    };

    // 读取账号数据、冷却状态、剩余积分（账号经 vault 解密还原明文 jwt）
    let accounts = crate::vault::load_accounts(state);
    let pool_file: ApiPoolFile = fs_utils::read_json(&state.path("api_pool.json"));
    let groups_file: crate::models::GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    let cooldowns_file: AccountCooldownsFile =
        fs_utils::read_json(&state.path("account_cooldowns.json"));
    let credits_file: RemainingCreditsFile =
        fs_utils::read_json(&state.path("remaining_credits.json"));
    let device_map: DeviceMap = fs_utils::read_json(&state.path("device_map.json"));

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

    // 创建池并同步（含调度策略与分组筛选）
    let pool = ApiPool::new();
    pool.set_strategy(strategy);
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
        &credits_file.expire_times,
        &device_map,
    );

    let pool_count = pool.count();
    let healthy_count = pool.diagnose().iter().filter(|d| d.reason.starts_with("healthy")).count();
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-池状态: pool_size={} healthy={} port={}",
            pool_count, healthy_count, port,
        ),
    );

    // ===== WorkBuddy 上游池装配（T2.1）=====
    let wb_accounts = crate::commands::workbuddy::wb_upstream_accounts(state);
    let wb_pool = ApiPool::new();
    wb_pool.sync_from_wb(&wb_accounts, &pool_file.enabled_uids);
    let wb_count = wb_pool.count();
    let wb_healthy = wb_pool.diagnose().iter().filter(|d| d.reason.starts_with("healthy")).count();
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "API服务启动-WB上游池: enabled={} accounts={} healthy={} strategy={}",
            pool_file.wb_enabled,
            wb_count,
            wb_healthy,
            crate::api_server::pool::PoolStrategy::parse(&pool_file.strategy).as_str(),
        ),
    );
    wb_pool.set_strategy(strategy);

    // 池为空时给出明确警告
    if pool_count == 0 {
        fs_utils::app_log(
            &state.data_dir,
            "警告: 账号池为空！请在API服务页面勾选账号并保存后再启动。当前 api_pool.json 中 enabled_uids 为空。",
        );
    } else if healthy_count == 0 {
        fs_utils::app_log(
            &state.data_dir,
            "警告: 池中无健康账号！所有账号可能处于冷却/积分过期/SessionDead 状态。请检查账号状态或清除冷却。",
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
        wb_sticky: crate::api_server::wb_sticky::StickyStore::load(&state.data_dir),
        model_cooldowns: Mutex::new(std::collections::HashMap::new()),
        wb_template_cache: Mutex::new(None),
        default_model,
        data_dir: state.data_dir.clone(),
        total_requests: std::sync::atomic::AtomicU64::new(0),
        active_uid: Mutex::new(None),
        last_error: Mutex::new(None),
        logger: ApiLogger::new(state.logs_dir()),
        debug_enabled: std::sync::atomic::AtomicBool::new(false),
        usage: Mutex::new(crate::api_server::usage::load(&state.data_dir)),
        wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
        wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
    });

    let handle = start_api_server(port, shared.clone()).await?;

    fs_utils::app_log(
        &state.data_dir,
        &format!("API 服务已启动: port={} pool_accounts={}", port, pool_count),
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

    // 同步托盘菜单文本 + 系统通知
    sync_tray_api_text(app, true);
    crate::notify::notify(
        app,
        "API 服务已启动",
        &format!("端口 {port}，池内 {pool_count} 个账号"),
    );

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
        fs_utils::app_log(&state.data_dir, "API 服务已停止");
        drop(guard);
        sync_tray_api_text(app, false);
        crate::notify::notify(app, "API 服务已停止", "本地网关已关闭");
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
                port: state.settings().api_port,
                total_requests: total,
                active_uid: active,
                last_error: last_err,
                started_at: Some(rt.started_at),
            }
        }
        None => {
            let settings = state.settings();
            ApiServiceStatus {
                running: false,
                port: settings.api_port,
                total_requests: 0,
                active_uid: None,
                last_error: None,
                started_at: None,
            }
        }
    }
}

// ==================== 池管理命令 ====================

#[tauri::command]
pub fn pool_list(state: State<'_, AppState>) -> ApiPoolFile {
    fs_utils::read_json(&state.path("api_pool.json"))
}

/// 批量设置池中的账号 UID 列表 + 调度策略 + 分组筛选（T10）+ WB 上游开关（T2.1）
/// + T5.3 默认深度思考 / T5.5 工具代执行 / T5.6③ 后台任务降级（未传字段保留原值）
#[tauri::command]
pub fn pool_set(
    state: State<'_, AppState>,
    uids: Vec<String>,
    strategy: Option<String>,
    group_ids: Option<Vec<String>>,
    wb_enabled: Option<bool>,
    wb_default_thinking: Option<bool>,
    wb_tool_exec: Option<bool>,
    wb_bg_downgrade: Option<bool>,
) -> Result<(), String> {
    let existing: ApiPoolFile = fs_utils::read_json(&state.path("api_pool.json"));
    let pool_file = ApiPoolFile {
        enabled_uids: uids,
        strategy: strategy.unwrap_or_default(),
        group_ids: group_ids.unwrap_or_default(),
        wb_enabled: wb_enabled.unwrap_or(existing.wb_enabled),
        wb_default_thinking: wb_default_thinking.unwrap_or(existing.wb_default_thinking),
        wb_tool_exec: wb_tool_exec.unwrap_or(existing.wb_tool_exec),
        wb_bg_downgrade: wb_bg_downgrade.unwrap_or(existing.wb_bg_downgrade),
    };
    fs_utils::write_json(&state.path("api_pool.json"), &pool_file)
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

/// 查询最近 N 天的 API 用量统计（按日聚合，直接读盘，服务未运行也可查）
#[tauri::command]
pub fn api_usage_stats(state: State<'_, AppState>, days: Option<u32>) -> Vec<crate::api_server::usage::UsageDayView> {
    let days = days.unwrap_or(14).clamp(1, 90);
    crate::api_server::usage::query_recent(&state.data_dir, days)
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
