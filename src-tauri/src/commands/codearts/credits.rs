//! CodeArts 积分命令域：积分查询（带缓存）/ 历史快照读取。

use tauri::State;

use crate::state::AppState;
use crate::tasks::codearts_credits;

/// 积分查询（user_id=Some 单账号 / None 全池；fresh=true 跳过缓存）。
#[tauri::command]
pub async fn codearts_credits_fetch(
    state: State<'_, AppState>,
    user_id: Option<String>,
    fresh: Option<bool>,
) -> Result<serde_json::Value, String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        codearts_credits::fetch_credits(&st, user_id.as_deref(), fresh.unwrap_or(false))
    })
    .await
    .map_err(|e| format!("积分查询任务失败: {e}"))?
}

/// 积分每日快照历史（365 天）。
#[tauri::command]
pub fn codearts_credits_history_list(state: State<AppState>) -> Vec<serde_json::Value> {
    crate::store::docs::codearts_credits_history_load(&crate::store::db(&state.data_dir))
}
