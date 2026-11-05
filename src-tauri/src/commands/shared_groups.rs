//! 跨应用共享分组（shared-groups-tags-design.md P0，需求①）：
//! - 分组「定义」统一收敛到既有 `groups` 行文档表（每行一个 Group JSON，id 前缀
//!   统一 `shg_`，启动迁移保证存量全量重定向）；「成员关系」保持各池现状（D1）：
//!   Trae 走 group_members 映射表，WB/Qoder 走账号记录 group_id 字段；
//! - 三池既有分组命令保留旧名作薄封装（前端零改动），内部转发统一实现；
//! - 删除分组时按「先回落成员 → 再删定义 → 最后清引用」顺序（对齐
//!   qoder_groups_remove 审查结论），并联动清理 api_pool / api_keys 引用 + 热重载。

use tauri::State;

use crate::models::Group;
use crate::state::AppState;

use super::accounts::GroupView;

// ── 内部工具 ────────────────────────────────────────────────────────────────

pub(crate) fn defs_load(state: &AppState) -> Vec<Group> {
    crate::store::docs::group_defs_load(&crate::store::db(&state.data_dir))
}

fn defs_save(state: &AppState, defs: &[Group]) -> Result<(), String> {
    crate::store::docs::group_defs_save(&crate::store::db(&state.data_dir), defs)
}

/// 全局重名校验（推广 qoder validate_name；trim/非空/共享定义内唯一）。
/// exclude_id 供 update 排除自身；返回 trim 后的名称
pub(crate) fn validate_name(
    defs: &[Group],
    name: &str,
    exclude_id: Option<&str>,
) -> Result<String, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("分组名不能为空".into());
    }
    if defs
        .iter()
        .any(|g| g.name == name && Some(g.id.as_str()) != exclude_id)
    {
        return Err(format!("分组「{name}」已存在"));
    }
    Ok(name)
}

/// 池整读（B5 raw 版）：rows_all 失败显式上抛（docs::wb_pool_load / qoder_pool_load
/// 内部静默空集，读失败会走「空池回落」伪成功）；形状与两者一致
/// （{accounts: [...]}），wb_pool_save / qoder_pool_save 对称
fn pool_load_checked(store: &crate::store::Store, table: &str) -> Result<serde_json::Value, String> {
    let accounts: Vec<serde_json::Value> = store
        .rows_all(table)?
        .into_iter()
        .map(|(_, data)| data)
        .collect();
    Ok(serde_json::json!({ "accounts": accounts }))
}

/// 各池成员一次性扫描（B6）：uid → 分组 id 列表映射，三池各查一次整表——
/// 供 shared_groups_list 组装，替代原「每分组逐个查询」的 N+1 模式
fn uids_by_gid_all(state: &AppState) -> std::collections::HashMap<String, Vec<String>> {
    let store = crate::store::db(&state.data_dir);
    let mut map: std::collections::HashMap<String, Vec<String>> = Default::default();
    // Trae：group_members 表
    let _ = store.with_conn(|c| {
        let mut stmt = c.prepare("SELECT uid, group_id FROM group_members")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (uid, gid) in rows {
            map.entry(gid).or_default().push(uid);
        }
        Ok(())
    });
    // Buddy / Qoder：行文档 group_id 字段（读失败留痕后按空池处理——列表只读展示
    // 与原 pool_members 的 fail-open 口径一致，B5 的 fail-closed 仅约束删除路径；
    // 但按 log_dedup_dropped 先例，静默跳过须 app_log 落痕）
    for table in ["wb_accounts", "qoder_accounts", "buddy_ai_accounts"] {
        match store.rows_all(table) {
            Ok(rows) => {
                for (_, data) in rows {
                    if let (Some(gid), Some(id)) = (
                        data.get("group_id").and_then(|v| v.as_str()),
                        data.get("id").and_then(|v| v.as_str()),
                    ) {
                        if !gid.is_empty() {
                            map.entry(gid.to_string()).or_default().push(id.to_string());
                        }
                    }
                }
            }
            Err(e) => crate::fs_utils::app_log(
                &state.data_dir,
                &format!("shared_groups_list 读 {table} 失败，该池成员按空处理：{e}"),
            ),
        }
    }
    // 豆包：doubao_accounts 行文档主键 = user_id（非 id 字段），单独扫描
    match store.rows_all("doubao_accounts") {
        Ok(rows) => {
            for (_, data) in rows {
                if let (Some(gid), Some(id)) = (
                    data.get("group_id").and_then(|v| v.as_str()),
                    data.get("user_id").and_then(|v| v.as_str()),
                ) {
                    if !gid.is_empty() {
                        map.entry(gid.to_string()).or_default().push(id.to_string());
                    }
                }
            }
        }
        Err(e) => crate::fs_utils::app_log(
            &state.data_dir,
            &format!("shared_groups_list 读 doubao_accounts 失败，该池成员按空处理：{e}"),
        ),
    }
    map
}

// ── 列表 ────────────────────────────────────────────────────────────────────

/// 共享分组列表：defs 全局一份；uids = 四池成员并集（trae 数字 uid + wb- + qd- +
/// 豆包 user_id——大整数雪花号，与 trae 小数字 uid 实际不重叠），count = uids.len()。
/// 供 API Key 分组绑定等跨池消费方使用。
#[tauri::command]
pub fn shared_groups_list(state: State<AppState>) -> Vec<GroupView> {
    let defs = defs_load(&state);
    // B6：三池各一次整表扫描构建映射（原每分组 3 次查询，N 分组 = 3N+1 次读）
    let mut uids_by_gid = uids_by_gid_all(&state);
    defs.into_iter()
        .map(|g| {
            let uids = uids_by_gid.remove(&g.id).unwrap_or_default();
            let count = uids.len();
            GroupView { id: g.id, name: g.name, color: g.color, order: g.order, count, uids }
        })
        .collect()
}

/// 各池视图组装（三池既有 list 命令共用）：defs 读统一存储，uids 仍从各自池推导。
/// uids_by_gid 由调用方**一次**整池扫描构建（防 N+1：禁止在每分组闭包内重载池）。
pub(crate) fn list_for_pool(
    state: &AppState,
    uids_by_gid: std::collections::HashMap<String, Vec<String>>,
) -> Vec<GroupView> {
    defs_load(state)
        .into_iter()
        .map(|g| {
            let uids = uids_by_gid.get(&g.id).cloned().unwrap_or_default();
            let count = uids.len();
            GroupView { id: g.id, name: g.name, color: g.color, order: g.order, count, uids }
        })
        .collect()
}

// ── 增删改 ──────────────────────────────────────────────────────────────────

/// 创建共享分组（shg_ 前缀 + 全局重名校验）。
/// 持 group_defs_lock：defs「load→save」整体互斥，防并发创建/删除互相覆盖
pub(crate) fn create_inner(state: &AppState, name: String, color: String) -> Result<String, String> {
    let _guard = state.group_defs_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut defs = defs_load(state);
    let name = validate_name(&defs, &name, None)?;
    let rand = uuid::Uuid::new_v4().simple().to_string();
    let id = format!("shg_{}{}", chrono::Local::now().timestamp_millis(), &rand[..4]);
    let order = (defs.len() as i32) + 1;
    defs.push(Group { id: id.clone(), name, color, order });
    defs_save(state, &defs)?;
    Ok(id)
}

#[tauri::command]
pub fn shared_groups_create(state: State<AppState>, name: String, color: String) -> Result<String, String> {
    create_inner(&state, name, color)
}

/// 持 group_defs_lock：defs「load→save」整体互斥（同 create_inner）
pub(crate) fn update_inner(
    state: &AppState,
    id: String,
    name: Option<String>,
    color: Option<String>,
    order: Option<i32>,
) -> Result<(), String> {
    let _guard = state.group_defs_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut defs = defs_load(state);
    // 先校验后可变借用（规避 iter_mut 与校验读借用冲突，对齐 qoder 实现）
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
    defs_save(state, &defs)
}

#[tauri::command]
pub fn shared_groups_update(
    state: State<AppState>,
    id: String,
    name: Option<String>,
    color: Option<String>,
    order: Option<i32>,
) -> Result<(), String> {
    update_inner(&state, id, name, color, order)
}

/// 删除共享分组（顺序敏感，见模块注释）：
/// ①a Trae 成员回落 → ①b Buddy 成员回落 + ② 删定义（同一临界区，防幽灵
/// TOCTOU，见段内注释）→ ①c Qoder 成员回落（定义已删，并发 move 复检必拒）
/// → ③ 清 api_pool / api_keys.allowed_groups 引用 → ④ 热重载 + org 索引失效。
pub(crate) fn delete_inner(
    state: &AppState,
    runtime: &std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>,
    id: &str,
) -> Result<(), String> {
    let store = crate::store::db(&state.data_dir);
    // B5 预检（零副作用）：两池源表读失败即中止——原实现 ①b/①c 读失败静默空集
    // 后仍继续删定义/清引用，成员残留成半回落（定义已删、绑定悬空）。
    // 预检通过后 ①b/①c 改 raw 读（rows_all 直读，失败 `?` 上抛不再静默空集）
    store.rows_all("wb_accounts")?;
    store.rows_all("qoder_accounts")?;
    store.rows_all("buddy_ai_accounts")?;
    // ①a Trae 成员回落：直接删 group_members 行（缺失 = 未分组，幂等）
    store.with_conn(|c| {
        c.execute("DELETE FROM group_members WHERE group_id = ?1", [id])?;
        Ok(())
    })?;
    // ①b Buddy 成员回落（读-改-写互斥：罩住池段防旧副本整文档覆盖）
    // ② 删定义并入本临界区（持 wb_pool_lock 期间取 group_defs_lock，锁序池锁→defs锁）：
    // 关闭「回落释放池锁后、删定义前」的幽灵 TOCTOU 窗口——并发 workbuddy_account_move
    // 在该窗口持池锁+defs锁复检会通过（定义还在）并把账号绑进即将删除的分组。
    // 删定义后 ①c 的 Qoder 回落仍照常执行：此刻后并发的 move 复检必失败（定义已删），
    // 先于删定义完成的 move 绑定会被 ①c 回落清除——两侧窗口均闭合
    {
        let _guard = state.wb_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool = pool_load_checked(&store, "wb_accounts")?;
        let mut changed = false;
        if let Some(arr) = pool.get_mut("accounts").and_then(serde_json::Value::as_array_mut) {
            for a in arr.iter_mut() {
                if a.get("group_id").and_then(|v| v.as_str()) == Some(id) {
                    a["group_id"] = serde_json::json!("");
                    changed = true;
                }
            }
        }
        if changed {
            crate::store::docs::wb_pool_save(&store, &pool)?;
        }
        // ② 删定义（同一临界区内取 defs 锁）
        {
            let _defs = state.group_defs_lock.lock().unwrap_or_else(|e| e.into_inner());
            // ①a 复删 Trae 成员（幂等）：闭合「①a 已删、② 未删定义」窗口内并发
            // group_move 持 defs 锁复检通过、把账号重新绑进本分组的幽灵绑定
            // （删定义后 membership 悬空）。复检与写入在 defs 锁内整体串行，
            // 此处的 DELETE 要么先于并发写入完成（随后被清除），要么并发写入
            // 因定义已删被复检拒绝——两侧窗口均闭合
            store.with_conn(|c| {
                c.execute("DELETE FROM group_members WHERE group_id = ?1", [id])?;
                Ok(())
            })?;
            let mut defs = defs_load(state);
            defs.retain(|g| g.id != id);
            defs_save(state, &defs)?;
        }
    }
    // ①c Qoder 成员回落（Value 语义直改保留未知字段，持 qoder_pool_lock——
    // qoder::common 为私有模块，此处与 WB 侧同构实现）
    {
        let _guard = state.qoder_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool = pool_load_checked(&store, "qoder_accounts")?;
        let mut changed = false;
        if let Some(arr) = pool.get_mut("accounts").and_then(serde_json::Value::as_array_mut) {
            for a in arr.iter_mut() {
                if a.get("group_id").and_then(|v| v.as_str()) == Some(id) {
                    a["group_id"] = serde_json::json!("");
                    changed = true;
                }
            }
        }
        if changed {
            crate::store::docs::qoder_pool_save(&store, &pool)?;
        }
    }
    // ①d Buddy 国际版成员回落（持 buddy_ai_pool_lock；锁序 wb→buddy 与迁移路径一致）：
    // 原遗漏会让 buddy_ai_accounts.group_id 悬空指向已删分组
    {
        let _guard = state.buddy_ai_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool = crate::store::docs::buddy_ai_pool_load(&store);
        let mut changed = false;
        if let Some(arr) = pool.get_mut("accounts").and_then(serde_json::Value::as_array_mut) {
            for a in arr.iter_mut() {
                if a.get("group_id").and_then(|v| v.as_str()) == Some(id) {
                    a["group_id"] = serde_json::json!("");
                    changed = true;
                }
            }
        }
        if changed {
            crate::store::docs::buddy_ai_pool_save(&store, &pool)?;
        }
    }
    // ①e 豆包成员回落（持 doubao_pool_lock；typed 池，group_id=None 即未分组）：
    // 与 ①d 同理，防 doubao_accounts.group_id 悬空指向已删分组
    {
        let _guard = state.doubao_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool = crate::store::docs::doubao_pool_load(&store);
        let mut changed = false;
        for a in pool.accounts.iter_mut() {
            if a.group_id.as_deref() == Some(id) {
                a.group_id = None;
                changed = true;
            }
        }
        if changed {
            crate::store::docs::doubao_pool_save(&store, &pool)?;
        }
    }
    // ③ 清引用：api_pool 三池分组筛选 + api_keys.allowed_groups
    // api_pool RMW 持 POOL_CFG_LOCK（B1，配置叶子锁；此时池/org 锁均已释放）
    let refs_changed = {
        let _cfg = crate::commands::api_server::POOL_CFG_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut refs_changed = false;
        let mut pool_file = crate::commands::api_server::load_pool_file(&state.data_dir);
        for ids in [
            &mut pool_file.group_ids,
            &mut pool_file.wb_group_ids,
            &mut pool_file.qoder_group_ids,
            &mut pool_file.buddy_ai_group_ids,
        ] {
            if ids.iter().any(|g| g == id) {
                ids.retain(|g| g != id);
                refs_changed = true;
            }
        }
        if refs_changed {
            store.kv_set("api_pool", &pool_file)?;
        }
        refs_changed
    };
    // Key 引用清理走锁内原子修改（防与并发 UI 保存互覆约束字段）
    // 审查修复（中等项）：modify 返回值（是否清理了引用）不丢弃，留痕可观测
    let keys_cleaned = crate::api_server::api_keys::modify(&state.data_dir, |keys_file| {
        let mut changed = false;
        for k in keys_file.keys.iter_mut() {
            if k.allowed_groups.iter().any(|g| g == id) {
                k.allowed_groups.retain(|g| g != id);
                changed = true;
            }
        }
        changed
    });
    if keys_cleaned {
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("分组 {id} 已删除：同步清理了引用它的 API Key allowed_groups"),
        );
    }    // ④ 联动：成员回落 / 池筛选变更都影响调度池；Key 约束展开索引失效
    if refs_changed {
        crate::commands::api_server::reload_pools_if_running(state, runtime);
    }
    crate::api_server::org_index::invalidate(&state.data_dir);
    Ok(())
}

#[tauri::command]
pub fn shared_groups_delete(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    id: String,
) -> Result<(), String> {
    delete_inner(&state, runtime.inner(), &id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Group;

    fn tmp_state(tag: &str) -> (std::path::PathBuf, AppState) {
        let d = std::env::temp_dir().join(format!("twa_shg_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let state = AppState {
            data_dir: d.clone(),
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            buddy_ai_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        (d, state)
    }

    #[test]
    fn validate_name_global_unique() {
        let defs = vec![Group { id: "shg_1".into(), name: "主号".into(), color: String::new(), order: 1 }];
        assert!(validate_name(&defs, "  主号 ", None).is_err(), "全局重名拒绝");
        assert_eq!(validate_name(&defs, "  主号 ", Some("shg_1")).unwrap(), "主号", "排除自身后放行");
        assert!(validate_name(&defs, "   ", None).is_err(), "空名拒绝");
    }

    #[test]
    fn create_update_delete_roundtrip() {
        let (dir, state) = tmp_state("crud");
        let id = create_inner(&state, "主号".into(), "#f00".into()).unwrap();
        assert!(id.starts_with("shg_"));
        // 重名拒绝（全局）
        assert!(create_inner(&state, "主号".into(), "#0f0".into()).is_err());
        update_inner(&state, id.clone(), Some("主力".into()), None, None).unwrap();
        let defs = defs_load(&state);
        assert_eq!(defs[0].name, "主力");
        // 删除：无成员无引用也应成功（幂等空操作）
        let runtime: std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>> =
            std::sync::Mutex::new(None);
        delete_inner(&state, &runtime, &id).unwrap();
        assert!(defs_load(&state).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
