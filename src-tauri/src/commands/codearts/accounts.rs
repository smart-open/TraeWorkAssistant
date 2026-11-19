//! CodeArts 账号域：AK/SK 手工入池 / 列表（脱敏）/ 改名备注分组 / 移除。
//!
//! AK/SK 属最高权限凭据：明文只经 tasks::codearts_common 写 vault（ns=codearts），
//! 池与列表只保留掩码与 has_credential 徽标，命令层全程不回明文。

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::state::AppState;
use crate::tasks::codearts_common::{self, CodeartsCreds};

use super::common::{account_id_of, load_pool, mask_ak, with_pool_mut, CodeartsAccount};

#[derive(Serialize, Clone)]
pub struct CodeartsAccountView {
    pub id: String,
    pub name: String,
    pub ak_masked: String,
    pub group_id: String,
    pub note: String,
    pub has_credential: bool,
    pub credits_balance: Option<f64>,
    pub credits_fetched_at: Option<String>,
    pub credits_expire_at: Option<i64>,
    pub auto_checkin: bool,
    pub added_at: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

fn view_of(a: &CodeartsAccount, has_credential: bool, tags: Vec<String>) -> CodeartsAccountView {
    CodeartsAccountView {
        id: a.id.clone(),
        name: a.name.clone(),
        ak_masked: a.ak_masked.clone(),
        group_id: a.group_id.clone(),
        note: a.note.clone(),
        has_credential,
        credits_balance: a.credits_balance,
        credits_fetched_at: a.credits_fetched_at.clone(),
        credits_expire_at: a.credits_expire_at,
        auto_checkin: a.auto_checkin,
        added_at: a.added_at,
        tags,
    }
}

/// 账号列表（脱敏；只回凭证存在徽标，AK/SK 零明文出 Rust）。
#[tauri::command]
pub fn codearts_accounts_list(state: State<AppState>) -> Vec<CodeartsAccountView> {
    let accounts = load_pool(&state);
    let tokens = crate::store::docs::codearts_token_store_load(&crate::store::db(&state.data_dir));
    let tag_map = crate::store::docs::account_tags_map(&crate::store::db(&state.data_dir));
    accounts
        .iter()
        .map(|a| {
            // 零明文红线：DB 占位行 ak/sk 恒空，明文只在 vault；
            // 以非敏感 credential_source 徽标判断凭证存在（save_token 落 "vault"）。
            let has_credential = tokens
                .get("tokens")
                .and_then(|t| t.get(&a.id))
                .and_then(|r| r.get("credential_source"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| !s.is_empty());
            view_of(a, has_credential, tag_map.get(&a.id).cloned().unwrap_or_default())
        })
        .collect()
}

/// AK/SK 手工入池（幂等：同凭据稳定同 id，重复导入只更新掩码/名称）。
#[tauri::command]
pub fn codearts_account_import_ak_sk(
    app: AppHandle,
    state: State<AppState>,
    name: Option<String>,
    ak: String,
    sk: String,
    group_id: Option<String>,
) -> Result<CodeartsAccountView, String> {
    let ak = ak.trim().to_string();
    let sk = sk.trim().to_string();
    if ak.is_empty() || sk.is_empty() {
        return Err("AK 与 SK 均不能为空".into());
    }
    let id = account_id_of(&ak, &sk);
    let masked = mask_ak(&ak);
    // 先写凭证（vault 失败不落库）；成功后再入池，避免半写入
    codearts_common::save_token(&state.data_dir, &id, &CodeartsCreds::new(ak, sk))?;
    let display_name = name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("CodeArts {}", &masked.chars().rev().take(4).collect::<String>()));
    with_pool_mut(&state, |pool| {
        if let Some(existing) = pool.iter_mut().find(|a| a.id == id) {
            existing.name = display_name.clone();
            existing.ak_masked = masked.clone();
            existing.has_credential = true;
            if let Some(g) = group_id.as_deref() {
                existing.group_id = g.to_string();
            }
        } else {
            pool.push(CodeartsAccount {
                id: id.clone(),
                name: display_name.clone(),
                ak_masked: masked.clone(),
                group_id: group_id.clone().unwrap_or_default(),
                has_credential: true,
                added_at: chrono::Utc::now().timestamp(),
                ..Default::default()
            });
        }
        Ok(())
    })?;
    // 账号变更联动网关池热重载（服务未运行时 no-op）
    let rt = app.state::<std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>();
    crate::commands::api_server::reload_pools_if_running(&state, rt.inner());
    Ok(CodeartsAccountView {
        id,
        name: display_name,
        ak_masked: masked,
        group_id: group_id.unwrap_or_default(),
        note: String::new(),
        has_credential: true,
        credits_balance: None,
        credits_fetched_at: None,
        credits_expire_at: None,
        auto_checkin: true,
        added_at: chrono::Utc::now().timestamp(),
        tags: Vec::new(),
    })
}

/// 改名 / 备注 / 分组（仅更新传入的 Some 字段；name/note 传 null 表示不改）。
#[tauri::command]
pub fn codearts_account_save(
    app: AppHandle,
    state: State<AppState>,
    id: String,
    name: Option<String>,
    note: Option<String>,
    group_id: Option<String>,
) -> Result<(), String> {
    with_pool_mut(&state, |pool| {
        let a = pool.iter_mut().find(|a| a.id == id).ok_or("账号不存在")?;
        if let Some(n) = name {
            if !n.trim().is_empty() {
                a.name = n;
            }
        }
        if let Some(n) = note {
            a.note = n;
        }
        if let Some(g) = group_id {
            a.group_id = g;
        }
        Ok(())
    })?;
    // 账号变更联动网关池热重载（服务未运行时 no-op）
    let rt = app.state::<std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>();
    crate::commands::api_server::reload_pools_if_running(&state, rt.inner());
    Ok(())
}

/// 移除账号（同步清 token store / vault / 标签绑定）。
#[tauri::command]
pub fn codearts_account_remove(app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> {
    with_pool_mut(&state, |pool| {
        let before = pool.len();
        pool.retain(|a| a.id != id);
        if pool.len() == before {
            return Err("账号不存在".into());
        }
        Ok(())
    })?;
    // 池已移除后再清凭证（失败也要让账号从池消失，记日志即可）
    if let Err(e) = codearts_common::remove_token(&state.data_dir, &id) {
        crate::fs_utils::app_log(&state.data_dir, &format!("CodeArts 移除凭证失败: {e}"));
    }
    // 账号变更联动网关池热重载（服务未运行时 no-op）
    let rt = app.state::<std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>();
    crate::commands::api_server::reload_pools_if_running(&state, rt.inner());
    Ok(())
}
