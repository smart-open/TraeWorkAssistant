//! Qoder 积分域（F-80 M1 最小通道平移）：积分查询（缓存 + stale-on-error）/ 快照时序。
//! Web 化改造：async 命令壳删除——cmd_bridge handler 统一 spawn_blocking，
//! 此处保留同步函数（阻塞 ureq 网络请求在工作线程执行，不占 async worker）。

use serde_json::Value;

use crate::state::AppState;
use crate::tasks::qoder_credits;

/// 积分查询（user_id=None 全部账号；fresh=true 跳过 600s 缓存）。
/// 返回契约对齐 WbCreditsResult（§5.3）：{ok, cached, stale?, accounts:[...], total_balance}。
pub fn qoder_credits_fetch(
    state: &AppState,
    user_id: Option<String>,
    fresh: Option<bool>,
) -> Result<Value, String> {
    qoder_credits::fetch_credits(state, user_id.as_deref(), fresh.unwrap_or(false))
}

/// 积分快照时序（趋势图/到期日历数据源；365 天）
pub fn qoder_credits_history_list(state: &AppState) -> Result<Value, String> {
    let snapshots = crate::store::docs::qoder_credits_history_load(&crate::store::db(&state.data_dir));
    Ok(serde_json::json!({ "snapshots": snapshots }))
}
