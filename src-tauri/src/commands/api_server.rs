use std::sync::{Arc, Mutex};

use tauri::State;

use crate::fs_utils;
use crate::models::{
    AccountCooldownsFile, AccountsFile, ApiPoolFile, ApiServiceStatus, PoolStatus,
    RemainingCreditsFile,
};
use crate::state::AppState;

use crate::api_server::pool::ApiPool;
use crate::api_server::server::{start_api_server, ApiServerHandle};
use crate::api_server::ApiSharedState;

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

#[tauri::command]
pub async fn api_server_start(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> Result<ApiServiceStatus, String> {
    // 检查是否已运行
    {
        let guard = safe_lock(&runtime);
        if guard.is_some() {
            return Err("API 服务已在运行".into());
        }
    }

    let settings = state.settings();
    let port = settings.api_port;
    let api_key = settings.api_key.clone();
    let default_model = {
        let m = settings.api_default_model.trim();
        if m.is_empty() {
            crate::api_server::DEFAULT_MODEL.to_string()
        } else {
            m.to_string()
        }
    };

    // 读取账号数据、冷却状态、剩余积分
    let accounts: AccountsFile = fs_utils::read_json(&state.path("checkin_accounts.json"));
    let pool_file: ApiPoolFile = fs_utils::read_json(&state.path("api_pool.json"));
    let cooldowns_file: AccountCooldownsFile =
        fs_utils::read_json(&state.path("account_cooldowns.json"));
    let credits_file: RemainingCreditsFile =
        fs_utils::read_json(&state.path("remaining_credits.json"));

    // 创建池并同步
    let pool = ApiPool::new();
    pool.sync_from_accounts(
        &accounts.accounts,
        &pool_file.enabled_uids,
        &cooldowns_file.cooldowns,
        &credits_file.credits,
        &credits_file.expire_times,
    );

    let pool_count = pool.count();

    let shared = Arc::new(ApiSharedState {
        pool,
        api_key,
        default_model,
        total_requests: std::sync::atomic::AtomicU64::new(0),
        active_uid: Mutex::new(None),
        last_error: Mutex::new(None),
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

    *safe_lock(&runtime) = Some(ApiServerRuntime {
        handle,
        shared: shared.clone(),
        started_at: now,
    });

    Ok(status)
}

#[tauri::command]
pub async fn api_server_stop(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> Result<(), String> {
    let mut guard = safe_lock(&runtime);
    if let Some(mut rt) = guard.take() {
        rt.handle.stop();
        fs_utils::app_log(&state.data_dir, "API 服务已停止");
    }
    Ok(())
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

/// 批量设置池中的账号 UID 列表
#[tauri::command]
pub fn pool_set(state: State<'_, AppState>, uids: Vec<String>) -> Result<(), String> {
    let pool_file = ApiPoolFile {
        enabled_uids: uids,
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
