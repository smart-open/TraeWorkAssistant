use serde::Serialize;
use tauri::State;

use crate::fs_utils;
use crate::jwt;
use crate::models::{
    AccountView, AccountsFile, DeviceMap, DeviceEntry, GroupsFile, Group, RawAccount,
    CreditsFile, CheckinSummary,
};
use std::collections::HashMap;

use crate::state::AppState;

#[tauri::command]
pub fn accounts_list(state: State<AppState>) -> Vec<AccountView> {
    build_account_views(&state)
}

#[tauri::command]
pub fn account_add_manual(
    state: State<AppState>,
    name: String,
    jwt: String,
    group_id: Option<String>,
) -> Result<(), String> {
    let info = jwt::parse(&jwt);
    let uid = info.user_id.ok_or("无法从 JWT 解析 UserID，请检查格式")?;
    let mut accounts: AccountsFile = fs_utils::read_json(&state.path("checkin_accounts.json"));
    if accounts
        .accounts
        .iter()
        .any(|a| a.user_id.as_deref() == Some(&uid))
    {
        return Err("该账号已存在".into());
    }
    accounts.accounts.push(RawAccount {
        name: name.clone(),
        user_id: Some(uid.clone()),
        jwt,
        added_at: Some(fs_utils::now_iso()),
        updated_at: Some(fs_utils::now_iso()),
    });
    fs_utils::write_json(&state.path("checkin_accounts.json"), &accounts)?;
    if let Some(g) = group_id {
        let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
        groups.membership.insert(uid, g);
        fs_utils::write_json(&state.path("groups.json"), &groups)?;
    }
    Ok(())
}

#[tauri::command]
pub fn account_delete(
    state: State<AppState>,
    user_id: String,
    delete_profile: bool,
) -> Result<(), String> {
    let mut accounts: AccountsFile = fs_utils::read_json(&state.path("checkin_accounts.json"));
    accounts
        .accounts
        .retain(|a| a.user_id.as_deref() != Some(user_id.as_str()));
    fs_utils::write_json(&state.path("checkin_accounts.json"), &accounts)?;

    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    groups.membership.remove(&user_id);
    fs_utils::write_json(&state.path("groups.json"), &groups)?;

    if delete_profile {
        let p = state.path("profiles").join(&user_id);
        let _ = std::fs::remove_dir_all(p);
    }
    Ok(())
}

// ---------------- 分组 ----------------

#[derive(Serialize)]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub color: String,
    pub order: i32,
    pub count: usize,
}

#[tauri::command]
pub fn groups_list(state: State<AppState>) -> Vec<GroupView> {
    let groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    groups
        .groups
        .iter()
        .map(|g| {
            let count = groups
                .membership
                .values()
                .filter(|v| *v == &g.id)
                .count();
            GroupView {
                id: g.id.clone(),
                name: g.name.clone(),
                color: g.color.clone(),
                order: g.order,
                count,
            }
        })
        .collect()
}

#[tauri::command]
pub fn group_create(state: State<AppState>, name: String, color: String) -> Result<String, String> {
    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    let id = format!("g_{}", chrono::Local::now().timestamp_millis());
    let order = (groups.groups.len() as i32) + 1;
    groups.groups.push(Group {
        id: id.clone(),
        name,
        color,
        order,
    });
    fs_utils::write_json(&state.path("groups.json"), &groups)?;
    Ok(id)
}

#[tauri::command]
pub fn group_update(
    state: State<AppState>,
    id: String,
    name: Option<String>,
    color: Option<String>,
    order: Option<i32>,
) -> Result<(), String> {
    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    let g = groups
        .groups
        .iter_mut()
        .find(|g| g.id == id)
        .ok_or("分组不存在")?;
    if let Some(n) = name {
        g.name = n;
    }
    if let Some(c) = color {
        g.color = c;
    }
    if let Some(o) = order {
        g.order = o;
    }
    fs_utils::write_json(&state.path("groups.json"), &groups)?;
    Ok(())
}

#[tauri::command]
pub fn group_delete(state: State<AppState>, id: String) -> Result<(), String> {
    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    groups.groups.retain(|g| g.id != id);
    groups.membership.retain(|_, v| *v != id);
    fs_utils::write_json(&state.path("groups.json"), &groups)?;
    Ok(())
}

#[tauri::command]
pub fn group_move(
    state: State<AppState>,
    user_id: String,
    group_id: Option<String>,
) -> Result<(), String> {
    let mut groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    match group_id {
        Some(g) => {
            groups.membership.insert(user_id, g);
        }
        None => {
            groups.membership.remove(&user_id);
        }
    }
    fs_utils::write_json(&state.path("groups.json"), &groups)?;
    Ok(())
}

// ---------------- 内部工具 ----------------

/// 构建账号视图（聚合 JWT / 分组 / 设备 / 积分 / 今日签到）。
pub fn build_account_views(state: &State<AppState>) -> Vec<AccountView> {
    let accounts: AccountsFile = fs_utils::read_json(&state.path("checkin_accounts.json"));
    let groups: GroupsFile = fs_utils::read_json(&state.path("groups.json"));
    let device_map: DeviceMap = fs_utils::read_json(&state.path("device_map.json"));
    let credits: CreditsFile = fs_utils::read_json(&state.path("credits_history.json"));
    let summary: CheckinSummary = fs_utils::read_json(&state.path("checkin_summary.json"));
    let summary_today = summary
        .time
        .as_ref()
        .map(|t| t.starts_with(&fs_utils::today_prefix()))
        .unwrap_or(false);
    let checked_names: std::collections::HashSet<String> = if summary_today {
        summary
            .results
            .iter()
            .filter(|r| {
                let ok = r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let action = r.get("action").and_then(|v| v.as_str()).unwrap_or("");
                ok || (action != "fail" && !action.is_empty())
            })
            .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect()
    } else {
        Default::default()
    };

    let mut out = Vec::new();
    for a in &accounts.accounts {
        let uid = a
            .user_id
            .clone()
            .or_else(|| jwt::parse(&a.jwt).user_id.clone())
            .unwrap_or_default();
        let info = jwt::parse(&a.jwt);
        let group_id = groups.membership.get(&uid).cloned();
        // 取该账号最近日期的积分记录（credits_history.json 按日期追加，可能多条）；
        // 同日期取较大值，跨日期取较新日期，避免展示历史峰值而非当前余额。
        let credits_val = {
            let mut best: Option<(String, i64)> = None;
            for r in &credits.records {
                if r.user_id != uid {
                    continue;
                }
                match &best {
                    None => best = Some((r.date.clone(), r.credits)),
                    Some((d, c)) => {
                        if r.date > *d || (r.date == *d && r.credits > *c) {
                            best = Some((r.date.clone(), r.credits));
                        }
                    }
                }
            }
            best.map(|(_, c)| c)
        };
        let device_mask = device_map
            .get(&uid)
            .map(|d: &DeviceEntry| fs_utils::mask(&d.device_id));
        let checked = if summary_today {
            checked_names.contains(&a.name)
        } else {
            false
        };
        out.push(AccountView {
            user_id: uid,
            name: a.name.clone(),
            group_id,
            jwt_exp_hours: info.exp_hours,
            checked_today: Some(checked),
            credits: credits_val,
            device_id_masked: device_mask,
        });
    }
    out
}

/// 根据 scope 解析目标 user_id 列表。
pub fn resolve_user_ids(
    state: &State<AppState>,
    scope: &str,
    selected: Option<Vec<String>>,
) -> Result<Vec<String>, String> {
    let views = build_account_views(state);
    match scope {
        "all" => Ok(views.into_iter().map(|v| v.user_id).collect()),
        s if s.starts_with("group:") => {
            let gid = &s["group:".len()..];
            Ok(views
                .into_iter()
                .filter(|v| v.group_id.as_deref() == Some(gid))
                .map(|v| v.user_id)
                .collect())
        }
        "selected" => Ok(selected.unwrap_or_default()),
        _ => Err("未知的执行范围".into()),
    }
}
