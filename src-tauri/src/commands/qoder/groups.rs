//! Qoder 账号分组（对齐 Buddy 账号分组能力，复用 accounts.rs GroupView / models::Group）：
//! - 分组定义存 kv("qoder_groups")（`Vec<Group>`，结构与 Trae/Buddy 一致）；
//! - 成员关系直接落在 Qoder 账号记录的 `group_id` 字段（serde(default)，随账号池 JSON
//!   持久化，导出/导入天然携带，无需独立 membership 表）；
//! - 删除分组时组内账号回落「未分组」（对齐 Trae/Buddy group_delete 语义）。

use tauri::State;

use crate::state::AppState;

use super::common::{load_pool, load_pool_checked, save_pool, with_pool_mut};

fn load_defs(state: &AppState) -> Vec<crate::models::Group> {
    crate::store::db(&state.data_dir).kv_get("qoder_groups")
}

fn save_defs(state: &AppState, defs: &[crate::models::Group]) -> Result<(), String> {
    crate::store::db(&state.data_dir).kv_set("qoder_groups", &defs.to_vec())
}

/// 分组列表（count/uids 从账号池账号的 group_id 实时推导，供前端过滤 chips 与编辑弹框下拉）
#[tauri::command]
pub fn qoder_groups_list(state: State<AppState>) -> Vec<crate::commands::accounts::GroupView> {
    let defs = load_defs(&state);
    let pool = load_pool(&state);
    defs.into_iter()
        .map(|g| {
            let uids: Vec<String> = pool
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

/// 分组名统一校验（trim/非空/全局重名；P2 审查修复：create 与 update 共用）。
/// exclude_id 供 update 排除自身 id；返回 trim 后的名称
fn validate_name(
    defs: &[crate::models::Group],
    name: &str,
    exclude_id: Option<&str>,
) -> Result<String, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("分组名不能为空".into());
    }
    // 重名校验（审查 L）：同名分组会让前端按名匹配/展示产生歧义
    if defs
        .iter()
        .any(|g| g.name == name && Some(g.id.as_str()) != exclude_id)
    {
        return Err(format!("分组「{name}」已存在"));
    }
    Ok(name)
}

#[tauri::command]
pub fn qoder_groups_create(state: State<AppState>, name: String, color: String) -> Result<String, String> {
    // defs 读改写互斥（审查 P3）：与 update/remove/导入的 defs 写路径互斥防丢更新；
    // 重名校验的读也须在锁内——否则校验通过的名称仍可能与锁外插入的同名分组冲突
    let _guard = state.qoder_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut defs = load_defs(&state);
    let name = validate_name(&defs, &name, None)?;
    // P3：追加 4 位随机 hex 后缀——timestamp_millis 同毫秒并发可撞 id
    let rand = uuid::Uuid::new_v4().simple().to_string();
    let id = format!("qoderg_{}{}", chrono::Local::now().timestamp_millis(), &rand[..4]);
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
pub fn qoder_groups_update(
    state: State<AppState>,
    id: String,
    name: Option<String>,
    color: Option<String>,
    order: Option<i32>,
) -> Result<(), String> {
    // defs 读改写互斥（审查 P3）：同 create——含重名校验的读也在锁内
    let _guard = state.qoder_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut defs = load_defs(&state);
    // P2 审查修复：name 复用 create 同款校验（trim/非空/重名，重名排除自身 id）；
    // 先校验后可变借用，规避 defs 的 iter_mut 与校验读借用冲突
    let new_name = match name.as_deref() {
        Some(n) => Some(validate_name(&defs, n, Some(&id))?),
        None => None,
    };
    let g = defs.iter_mut().find(|g| g.id == id).ok_or("分组不存在")?;
    if let Some(n) = new_name {
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
pub fn qoder_groups_remove(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    id: String,
) -> Result<(), String> {
    // 单临界区（2026-10-05 审查修复）：组内账号回落与删分组定义合并到同一次持锁。
    // 原两段持锁（with_pool_mut 回落 → 解锁 → 再锁删定义）的窗口内可被并发
    // qoder_account_move 插入重新挂上本组（其锁内校验时定义尚在），删定义后
    // 留下幽灵 group_id（网关分组筛选下该账号沉默退出调度）。std Mutex 不可重入，
    // 故此处手动持锁内联回落（load_pool_checked + save_pool，与 with_pool_mut
    // 同语义：损坏池备份后拒绝，防整池覆盖丢账号）。
    //
    // 先回落组内账号、成功后再删分组定义（审查修复）：反序在中途回落失败时会产生
    // 「分组定义已删、账号 group_id 悬空」且重试路径断裂（retain 幂等空操作，
    // 仍因同一原因失败）。本序失败只发生在回落（定义残留，空分组重试即可删除）。
    let _guard = state.qoder_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut accounts = load_pool_checked(&state)?;
    for a in accounts.iter_mut() {
        if a.group_id == id {
            a.group_id = String::new();
        }
    }
    save_pool(&state, &accounts)?;
    let mut defs = load_defs(&state);
    defs.retain(|g| g.id != id);
    save_defs(&state, &defs)?;
    // 清理 api_pool 对该分组的筛选引用：被删分组的 id 在资源调度页无 chip 可取消
    // （幽灵筛选），残留会使 Qoder 池被静默清空且 UI 无出口。有引用变更时联动热重载。
    let store = crate::store::db(&state.data_dir);
    // 读改写基线统一走迁移入口（P1 审查修复）：per-pool 拆分后 Buddy 池旧共享值
    // 靠读取侧回填，直读 kv_get 落 serde default，整表写回会把迁移值静默覆盖丢失
    let mut pool_file = crate::commands::api_server::load_pool_file(&state.data_dir);
    if pool_file.qoder_group_ids.iter().any(|g| g == &id) {
        pool_file.qoder_group_ids.retain(|g| g != &id);
        store.kv_set("api_pool", &pool_file)?;
        crate::commands::api_server::reload_pools_if_running(&state, &runtime);
    }
    Ok(())
}

/// 移动账号到分组（group_id=None 回落「未分组」）；user_id = 账号 id（qd- 前缀，与 save/remove 同键）。
/// P2 审查修复：联动网关池热重载——api_pool 的 qoder_group_ids 按池内 group_id 过滤，
/// 服务运行时移动跨组不重载则网关仍按旧组过滤调度（红线 7 缺口，对齐 save/remove 惯例）
#[tauri::command]
pub fn qoder_account_move(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    user_id: String,
    group_id: Option<String>,
) -> Result<(), String> {
    with_pool_mut(&state, |accounts| {
        let acct = accounts
            .iter_mut()
            .find(|a| a.id == user_id)
            .ok_or_else(|| format!("账号不在池中: {user_id}"))?;
        // 目标分组校验移入锁内（2026-10-05 审查修复）：与 qoder_groups_remove 并发时
        // 锁外校验存在 TOCTOU，可把账号挂上已删除分组的幽灵 group_id（网关分组
        // 筛选下沉默退出调度）；校验与 remove 的删定义同持池锁，完全串行化
        if let Some(gid) = group_id.as_deref().filter(|g| !g.is_empty()) {
            if !load_defs(&state).iter().any(|g| g.id == gid) {
                return Err(format!("目标分组不存在: {gid}"));
            }
        }
        acct.group_id = group_id.unwrap_or_default();
        Ok(())
    })?;
    crate::commands::api_server::reload_pools_if_running(&state, &runtime);
    Ok(())
}
