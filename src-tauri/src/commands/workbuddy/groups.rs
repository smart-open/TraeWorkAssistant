//! Buddy 账号分组（对齐 Trae 账号分组能力，T：Buddy 分组）：
//! - 分组定义存 kv("workbuddy_groups")（`Vec<Group>`，结构与 Trae groups 表一致）；
//! - 成员关系直接落在 WB 账号记录的 `group_id` 字段（随账号池 JSON 持久化，
//!   导入/导出天然携带，无需独立 membership 表）；
//! - 删除分组时组内账号回落「未分组」（对齐 Trae group_delete 语义）。

use tauri::State;

use crate::state::AppState;

use super::common::{load_pool, save_pool};

fn load_defs(state: &AppState) -> Vec<crate::models::Group> {
    crate::store::db(&state.data_dir).kv_get("workbuddy_groups")
}

fn save_defs(state: &AppState, defs: &[crate::models::Group]) -> Result<(), String> {
    crate::store::db(&state.data_dir).kv_set("workbuddy_groups", &defs.to_vec())
}

/// 分组列表（count/uids 从账号池账号的 group_id 实时推导，供前端过滤 chips 与账号池分组筛选预览）
#[tauri::command]
pub fn workbuddy_groups_list(state: State<AppState>) -> Vec<crate::commands::accounts::GroupView> {
    let defs = load_defs(&state);
    let pool = load_pool(&state);
    defs.into_iter()
        .map(|g| {
            let uids: Vec<String> = pool
                .accounts
                .iter()
                .filter(|a| a.group_id == g.id)
                .map(|a| a.id.clone())
                .collect();
            let count = uids.len();
            crate::commands::accounts::GroupView {
                id: g.id,
                name: g.name,
                color: g.color,
                order: g.order,
                count,
                uids,
            }
        })
        .collect()
}

#[tauri::command]
pub fn workbuddy_groups_create(state: State<AppState>, name: String, color: String) -> Result<String, String> {
    let mut defs = load_defs(&state);
    let id = format!("wbg_{}", chrono::Local::now().timestamp_millis());
    let order = (defs.len() as i32) + 1;
    defs.push(crate::models::Group {
        id: id.clone(),
        name,
        color,
        order,
    });
    save_defs(&state, &defs)?;
    Ok(id)
}

#[tauri::command]
pub fn workbuddy_groups_update(
    state: State<AppState>,
    id: String,
    name: Option<String>,
    color: Option<String>,
    order: Option<i32>,
) -> Result<(), String> {
    let mut defs = load_defs(&state);
    let g = defs.iter_mut().find(|g| g.id == id).ok_or("分组不存在")?;
    if let Some(n) = name {
        g.name = n;
    }
    if let Some(c) = color {
        g.color = c;
    }
    if let Some(o) = order {
        g.order = o;
    }
    save_defs(&state, &defs)
}

#[tauri::command]
pub fn workbuddy_groups_remove(state: State<AppState>, id: String) -> Result<(), String> {
    let mut defs = load_defs(&state);
    defs.retain(|g| g.id != id);
    save_defs(&state, &defs)?;
    // 组内账号回落「未分组」
    let mut pool = load_pool(&state);
    let mut changed = false;
    for a in pool.accounts.iter_mut() {
        if a.group_id == id {
            a.group_id = String::new();
            changed = true;
        }
    }
    if changed {
        save_pool(&state, &pool)?;
    }
    Ok(())
}

/// 移动账号到分组（group_id=None 回落「未分组」）；user_id = 账号 id（wb- 前缀，与 save/remove 同键）
#[tauri::command]
pub fn workbuddy_account_move(
    state: State<AppState>,
    user_id: String,
    group_id: Option<String>,
) -> Result<(), String> {
    let mut pool = load_pool(&state);
    let acct = pool
        .accounts
        .iter_mut()
        .find(|a| a.id == user_id)
        .ok_or_else(|| format!("账号不存在: {user_id}"))?;
    acct.group_id = group_id.unwrap_or_default();
    save_pool(&state, &pool)
}
