//! Qoder 积分域（F-80 M1 最小通道）：积分查询（缓存 + stale-on-error）/ 快照时序。

use serde_json::Value;
use tauri::State;

use crate::state::AppState;
use crate::tasks::qoder_credits;

/// 积分查询（user_id=None 全部账号；fresh=true 跳过 600s 缓存）。
/// 返回契约对齐 WbCreditsResult（§5.3）：{ok, cached, stale?, accounts:[...], total_balance}。
/// I11：async 使网络 IO 脱离主线程（对照蓝本 workbuddy/credits.rs 同款标注）
#[tauri::command(async)]
pub async fn qoder_credits_fetch(
    state: State<'_, AppState>,
    user_id: Option<String>,
    fresh: Option<bool>,
) -> Result<Value, String> {
    // spawn_blocking：fetch_credits 内部为阻塞 ureq 网络请求（30s 超时），
    // async 命令体内直接执行会占用 async worker 线程
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        qoder_credits::fetch_credits(&state, user_id.as_deref(), fresh.unwrap_or(false))
    })
    .await
    .map_err(|e| format!("积分查询任务失败: {e}"))?
}

/// 积分快照时序（趋势图/到期日历数据源；365 天）
#[tauri::command]
pub fn qoder_credits_history_list(state: State<AppState>) -> Result<Value, String> {
    let snapshots = crate::store::docs::qoder_credits_history_load(&crate::store::db(&state.data_dir));
    Ok(serde_json::json!({ "snapshots": snapshots }))
}
