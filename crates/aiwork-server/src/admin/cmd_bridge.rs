//! T5 命令桥：`POST /api/cmd/{command}` → aiwork-core 命令函数白名单分发。
//!
//! 参数约定沿用 tauri invoke：body 为 JSON 对象，按原命令参数名为键取字段
//! （无参命令 body 可为 `{}`）；core 函数多为 ureq/IO 阻塞调用，
//! handler 统一 `spawn_blocking` 包裹，避免卡住异步运行时。
//!
//! 响应契约：
//! - 成功            → 200 `{"ok":true,"data":<返回值>}`
//! - core 返回 Err   → 500 `{"ok":false,"error":<e>}`
//! - 未注册命令      → 404 `{"ok":false,"error":"未知命令: <name>"}`
//! - 参数解析失败    → 500 `{"ok":false,"error":"参数错误: ..."}`

use std::sync::Arc;

use aiwork_core::commands::{
    accounts, api_server as api_server_cmd, checkin as checkin_cmd, misc, oauth, usage_history,
    wb_config, workbuddy,
};
use aiwork_core::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};

use super::{json_response, AdminState};

/// 未注册命令错误前缀（handler 据此把 dispatch 的 Err 映射为 404）
const UNKNOWN_PREFIX: &str = "未知命令: ";

/// POST /api/cmd/:command 入口：解析 body → spawn_blocking 分发 → 契约响应
pub(super) async fn cmd_handler(
    State(admin): State<Arc<AdminState>>,
    Path(command): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    // tauri invoke 约定：参数为 JSON 对象；空 body 视为 {}
    let args: Value = if body.is_empty() {
        Value::Object(Default::default())
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"ok": false, "error": format!("参数错误: {e}")}),
                )
            }
        }
    };
    // core 函数多为阻塞 IO：放阻塞线程池执行（闭包内不 await）
    let result = tokio::task::spawn_blocking(move || dispatch(&admin, &command, args)).await;
    match result {
        Ok(Ok(data)) => json_response(StatusCode::OK, json!({"ok": true, "data": data})),
        Ok(Err(e)) => {
            if e.starts_with(UNKNOWN_PREFIX) {
                json_response(StatusCode::NOT_FOUND, json!({"ok": false, "error": e}))
            } else {
                json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({"ok": false, "error": e}))
            }
        }
        // 工作线程 panic 等异常
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"ok": false, "error": format!("命令任务执行失败: {e}")}),
        ),
    }
}

/// 必填参数：按原 tauri 参数名取字段，缺失/类型不符 → "参数错误: ..."
fn arg<T: serde::de::DeserializeOwned>(args: &Value, key: &str) -> Result<T, String> {
    let v = args
        .get(key)
        .ok_or_else(|| format!("参数错误: 缺少必填参数 {key}"))?;
    serde_json::from_value(v.clone()).map_err(|e| format!("参数错误: {key} 解析失败: {e}"))
}

/// 可选参数：字段缺失或为 null → None；存在但类型不符 → "参数错误: ..."
fn opt_arg<T: serde::de::DeserializeOwned>(args: &Value, key: &str) -> Result<Option<T>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| format!("参数错误: {key} 解析失败: {e}")),
    }
}

/// 返回值 → serde_json::Value（Vec/结构体/基础类型统一序列化）
fn to_json<T: serde::Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| format!("结果序列化失败: {e}"))
}

/// 在阻塞线程内执行 core async 函数：优先复用当前运行时 Handle
/// （spawn_blocking 闭包内可用），兜底建临时 current-thread runtime。
fn block_on_async<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => h.block_on(fut),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("创建临时 runtime 失败")
            .block_on(fut),
    }
}

/// 命令白名单分发表：命令名 → core 函数调用。
/// 实参解析一律按 core 中真实签名（原 tauri 参数名 = rust snake_case 参数名）。
pub fn dispatch(admin: &AdminState, name: &str, args: Value) -> Result<Value, String> {
    let state: &AppState = &admin.state;
    match name {
        // ==================== accounts（账号/分组/积分） ====================
        "accounts_list" => to_json(accounts::accounts_list(state)),
        "accounts_export_raw" => accounts::accounts_export_raw(state),
        "accounts_import" => {
            let content = arg(&args, "content")?;
            let only = opt_arg(&args, "only")?;
            accounts::accounts_import(state, content, only).and_then(to_json)
        }
        "accounts_import_preview" => {
            let content = arg(&args, "content")?;
            accounts::accounts_import_preview(state, content).and_then(to_json)
        }
        "account_add_manual" => {
            let name_ = arg(&args, "name")?;
            let jwt = arg(&args, "jwt")?;
            let group_id = opt_arg(&args, "group_id")?;
            accounts::account_add_manual(state, name_, jwt, group_id).and_then(to_json)
        }
        "account_delete" => {
            let user_id = arg(&args, "user_id")?;
            // delete_profile 未传默认 false（保守：不连带删除 profile 目录）
            let delete_profile = opt_arg(&args, "delete_profile")?.unwrap_or(false);
            accounts::account_delete(state, user_id, delete_profile).and_then(to_json)
        }
        "account_update" => {
            let user_id = arg(&args, "user_id")?;
            let name_ = opt_arg(&args, "name")?;
            let jwt = opt_arg(&args, "jwt")?;
            accounts::account_update(state, user_id, name_, jwt).and_then(to_json)
        }
        "account_get_jwt" => {
            let user_id = arg(&args, "user_id")?;
            accounts::account_get_jwt(state, user_id).and_then(to_json)
        }
        "groups_list" => to_json(accounts::groups_list(state)),
        "group_create" => {
            let name_ = arg(&args, "name")?;
            let color = arg(&args, "color")?;
            accounts::group_create(state, name_, color).and_then(to_json)
        }
        "group_update" => {
            let id = arg(&args, "id")?;
            let name_ = opt_arg(&args, "name")?;
            let color = opt_arg(&args, "color")?;
            let order = opt_arg(&args, "order")?;
            accounts::group_update(state, id, name_, color, order).and_then(to_json)
        }
        "group_delete" => {
            let id = arg(&args, "id")?;
            accounts::group_delete(state, id).and_then(to_json)
        }
        "group_move" => {
            let user_id = arg(&args, "user_id")?;
            let group_id = opt_arg(&args, "group_id")?;
            accounts::group_move(state, user_id, group_id).and_then(to_json)
        }
        "fetch_remaining_credits" => {
            let user_id = arg(&args, "user_id")?;
            accounts::fetch_remaining_credits(state, user_id).and_then(to_json)
        }
        "fetch_credit_detail" => {
            let user_id = arg(&args, "user_id")?;
            accounts::fetch_credit_detail(state, user_id).and_then(to_json)
        }
        "refresh_remaining_credits" => accounts::refresh_remaining_credits(state).and_then(to_json),
        "credits_daily_list" => to_json(accounts::credits_daily_list(state)),
        "usage_history" => {
            let fresh = opt_arg(&args, "fresh")?;
            usage_history::usage_history_fetch(state, fresh).and_then(to_json)
        }
        "cooldown_clear" => {
            let user_id = arg(&args, "user_id")?;
            accounts::cooldown_clear(state, user_id).and_then(to_json)
        }
        "cooldown_clear_all" => accounts::cooldown_clear_all(state).and_then(to_json),
        "refresh_jwt" => {
            let user_id = arg(&args, "user_id")?;
            let force = opt_arg(&args, "force")?;
            accounts::refresh_jwt(state, user_id, force).and_then(to_json)
        }

        // ==================== checkin（Trae 签到） ====================
        "checkin_start" => {
            // body 即 CheckinOpts：scope / user_ids / skip_checked_in / skip_expired
            let opts: checkin_cmd::CheckinOpts = serde_json::from_value(args.clone())
                .map_err(|e| format!("参数错误: {e}"))?;
            let events = admin.events.clone();
            let emitter: checkin_cmd::CheckinEmitter =
                Arc::new(move |name: &str, payload: serde_json::Value| {
                    let _ = events.send((name.to_string(), payload));
                });
            checkin_cmd::start_checkin_core(admin.state.clone(), admin.guard.clone(), opts, emitter)
                .and_then(to_json)
        }
        "checkin_trends" => {
            let days = opt_arg(&args, "days")?;
            to_json(checkin_cmd::checkin_trends(state, days))
        }

        // ==================== misc（JWT/日志/设置） ====================
        "jwt_parse" => {
            let jwt = arg(&args, "jwt")?;
            to_json(misc::jwt_parse(jwt))
        }
        "logs_query" => {
            let opts = arg(&args, "opts")?;
            to_json(misc::logs_query(state, opts))
        }
        "logs_clear" => {
            let log_type = arg(&args, "log_type")?;
            misc::logs_clear(state, log_type).and_then(to_json)
        }
        "settings_get" => to_json(misc::settings_get(state)),
        "settings_set" => {
            // patch = body.patch（存在时）或整个 body
            let patch = args
                .get("patch")
                .cloned()
                .filter(|v| v.is_object())
                .unwrap_or_else(|| args.clone());
            misc::settings_set(state, patch).and_then(to_json)
        }

        // ==================== api_server（网关管理） ====================
        "pool_list" => to_json(api_server_cmd::pool_list(state)),
        "pool_set" => {
            let uids = opt_arg::<Vec<String>>(&args, "uids")?.unwrap_or_default();
            let strategy = opt_arg(&args, "strategy")?;
            let wb_strategy = opt_arg(&args, "wb_strategy")?;
            let group_ids = opt_arg(&args, "group_ids")?;
            let wb_group_ids = opt_arg(&args, "wb_group_ids")?;
            let wb_enabled = opt_arg(&args, "wb_enabled")?;
            let wb_default_thinking = opt_arg(&args, "wb_default_thinking")?;
            let wb_tool_exec = opt_arg(&args, "wb_tool_exec")?;
            let wb_bg_downgrade = opt_arg(&args, "wb_bg_downgrade")?;
            let wb_longctx_downgrade = opt_arg(&args, "wb_longctx_downgrade")?;
            let wb_hedge_threshold_ms = opt_arg(&args, "wb_hedge_threshold_ms")?;
            let account_concurrency_limit = opt_arg(&args, "account_concurrency_limit")?;
            let pool_sticky_ttl_secs = opt_arg(&args, "pool_sticky_ttl_secs")?;
            let wb_sticky_ttl_secs = opt_arg(&args, "wb_sticky_ttl_secs")?;
            let wb_uids = opt_arg(&args, "wb_uids")?;
            api_server_cmd::pool_set(
                state,
                uids,
                strategy,
                wb_strategy,
                group_ids,
                wb_group_ids,
                wb_enabled,
                wb_default_thinking,
                wb_tool_exec,
                wb_bg_downgrade,
                wb_longctx_downgrade,
                wb_hedge_threshold_ms,
                account_concurrency_limit,
                pool_sticky_ttl_secs,
                wb_sticky_ttl_secs,
                wb_uids,
            )
            .and_then(to_json)
        }
        "pool_status" => to_json(api_server_cmd::pool_status()),
        "wb_pool_status" => to_json(api_server_cmd::wb_pool_status()),
        "api_logs_list" => to_json(api_server_cmd::api_logs_list(state)),
        "api_logs_detail" => {
            let date = arg(&args, "date")?;
            to_json(api_server_cmd::api_logs_detail(state, date))
        }
        "api_logs_search" => {
            let opts = arg(&args, "opts")?;
            to_json(api_server_cmd::api_logs_search(state, opts))
        }
        "api_debug_toggle" => api_server_cmd::api_debug_toggle().and_then(to_json),
        "api_debug_status" => to_json(api_server_cmd::api_debug_status()),
        "api_models_list" => to_json(api_server_cmd::api_models_list(state)),
        "api_models_sync" => {
            block_on_async(api_server_cmd::api_models_sync(state)).and_then(to_json)
        }
        "api_wb_catalog_sync" => {
            block_on_async(api_server_cmd::api_wb_catalog_sync(state)).and_then(to_json)
        }
        "api_wb_catalog_list" => to_json(api_server_cmd::api_wb_catalog_list(state)),
        "api_usage_stats" => {
            let days = opt_arg(&args, "days")?;
            to_json(api_server_cmd::api_usage_stats(state, days))
        }
        "api_wb_usage_stats" => {
            let days = opt_arg(&args, "days")?;
            to_json(api_server_cmd::api_wb_usage_stats(state, days))
        }
        "api_custom_usage_stats" => {
            let days = opt_arg(&args, "days")?;
            to_json(api_server_cmd::api_custom_usage_stats(state, days))
        }
        "api_keys_list" => to_json(api_server_cmd::api_keys_list(state)),
        "api_keys_save" => {
            let keys = arg(&args, "keys")?;
            let auth_disabled = opt_arg(&args, "auth_disabled")?;
            api_server_cmd::api_keys_save(state, keys, auth_disabled).and_then(to_json)
        }
        "api_unified_models" => {
            let available_only = opt_arg(&args, "available_only")?;
            api_server_cmd::api_unified_models(state, available_only).and_then(to_json)
        }
        "model_whitelist_get" => to_json(api_server_cmd::model_whitelist_get(state)),
        "model_whitelist_set" => {
            let models = arg(&args, "models")?;
            api_server_cmd::model_whitelist_set(state, models).and_then(to_json)
        }
        "dispatch_policy_get" => to_json(api_server_cmd::dispatch_policy_get(state)),
        "dispatch_policy_set" => {
            let policy = arg(&args, "policy")?;
            api_server_cmd::dispatch_policy_set(state, policy).and_then(to_json)
        }
        "gateway_settings_get" => to_json(api_server_cmd::gateway_settings_get(state)),
        "gateway_settings_set" => {
            let settings = arg(&args, "settings")?;
            api_server_cmd::gateway_settings_set(state, settings).and_then(to_json)
        }
        "custom_models_list" => to_json(api_server_cmd::custom_models_list(state)),
        "custom_models_save" => {
            let model = arg(&args, "model")?;
            api_server_cmd::custom_models_save(state, model).and_then(to_json)
        }
        "custom_models_remove" => {
            let id = arg(&args, "id")?;
            api_server_cmd::custom_models_remove(state, id).and_then(to_json)
        }
        "custom_model_test" => {
            let model = arg(&args, "model")?;
            block_on_async(api_server_cmd::custom_model_test(model)).and_then(to_json)
        }
        "trae_model_meta_set" => {
            let model = arg(&args, "model")?;
            let meta = arg(&args, "meta")?;
            api_server_cmd::trae_model_meta_set(state, model, meta).and_then(to_json)
        }
        "trae_model_meta_get" => {
            let model = arg(&args, "model")?;
            to_json(api_server_cmd::trae_model_meta_get(state, model))
        }
        "trae_model_meta_clear" => {
            let model = arg(&args, "model")?;
            api_server_cmd::trae_model_meta_clear(state, model).and_then(to_json)
        }

        // ==================== wb_config（WB 手工配置） ====================
        "wb_route_config_get" => wb_config::wb_route_config_get(state),
        "wb_route_config_set" => {
            let config = arg(&args, "config")?;
            wb_config::wb_route_config_set(state, config).and_then(to_json)
        }
        "wb_template_map_get" => wb_config::wb_template_map_get(state),
        "wb_template_map_set" => {
            let map = arg(&args, "map")?;
            wb_config::wb_template_map_set(state, map).and_then(to_json)
        }

        // ==================== oauth（OAuth 登录） ====================
        "oauth_get_login_url" => to_json(oauth::oauth_get_login_url(state)),
        "oauth_parse_callback" => {
            let callback_url = arg(&args, "callback_url")?;
            oauth::oauth_parse_callback(state, callback_url).and_then(to_json)
        }
        "oauth_login" => {
            let callback_url = arg(&args, "callback_url")?;
            let account_name = opt_arg(&args, "account_name")?;
            let group_id = opt_arg(&args, "group_id")?;
            oauth::oauth_login(state, callback_url, account_name, group_id).and_then(to_json)
        }

        // ==================== workbuddy（WorkBuddy 全域） ====================
        "workbuddy_accounts_list" => workbuddy::workbuddy_accounts_list(state).and_then(to_json),
        "workbuddy_account_save" => {
            let user_id = arg(&args, "user_id")?;
            let name_ = opt_arg(&args, "name")?;
            let note = opt_arg(&args, "note")?;
            workbuddy::workbuddy_account_save(state, user_id, name_, note).and_then(to_json)
        }
        "workbuddy_account_move" => {
            let user_id = arg(&args, "user_id")?;
            let group_id = opt_arg(&args, "group_id")?;
            workbuddy::workbuddy_account_move(state, user_id, group_id).and_then(to_json)
        }
        "workbuddy_groups_list" => to_json(workbuddy::workbuddy_groups_list(state)),
        "workbuddy_groups_create" => {
            let name_ = arg(&args, "name")?;
            let color = arg(&args, "color")?;
            workbuddy::workbuddy_groups_create(state, name_, color).and_then(to_json)
        }
        "workbuddy_groups_update" => {
            let id = arg(&args, "id")?;
            let name_ = opt_arg(&args, "name")?;
            let color = opt_arg(&args, "color")?;
            let order = opt_arg(&args, "order")?;
            workbuddy::workbuddy_groups_update(state, id, name_, color, order).and_then(to_json)
        }
        "workbuddy_groups_remove" => {
            let id = arg(&args, "id")?;
            workbuddy::workbuddy_groups_remove(state, id).and_then(to_json)
        }
        "workbuddy_account_remove" => {
            let user_id = arg(&args, "user_id")?;
            let delete_snapshot = opt_arg(&args, "delete_snapshot")?;
            workbuddy::workbuddy_account_remove(state, user_id, delete_snapshot).and_then(to_json)
        }
        "workbuddy_scan_auth_file" => workbuddy::workbuddy_scan_auth_file(state).and_then(to_json),
        "workbuddy_account_import_auth" => {
            let name_ = opt_arg(&args, "name")?;
            workbuddy::workbuddy_account_import_auth(state, name_).and_then(to_json)
        }
        "workbuddy_refresh_token" => {
            let user_id = arg(&args, "user_id")?;
            let force = opt_arg(&args, "force")?;
            workbuddy::workbuddy_refresh_token(state, user_id, force).and_then(to_json)
        }
        "workbuddy_checkin_start" => {
            // 真实签名 (state, opts, emit)：轮次锁 + 工作线程模式，立即返回；
            // 兼容 {"opts":{...}} 包装与 body 即 opts 两种传参
            let raw = args.get("opts").cloned().unwrap_or_else(|| args.clone());
            let opts: workbuddy::WbCheckinOpts =
                serde_json::from_value(raw).map_err(|e| format!("参数错误: {e}"))?;
            let events = admin.events.clone();
            let emit: workbuddy::WbCheckinEmitter =
                Arc::new(move |name: &str, payload: serde_json::Value| {
                    let _ = events.send((name.to_string(), payload));
                });
            workbuddy::workbuddy_checkin_start(state, opts, emit).and_then(to_json)
        }
        "workbuddy_growth_run" => {
            let events = admin.events.clone();
            let emit: workbuddy::WbCheckinEmitter =
                Arc::new(move |name: &str, payload: serde_json::Value| {
                    let _ = events.send((name.to_string(), payload));
                });
            workbuddy::workbuddy_growth_run(state, emit).and_then(to_json)
        }
        "workbuddy_checkin_results" => {
            let days = opt_arg(&args, "days")?;
            workbuddy::workbuddy_checkin_results(state, days).and_then(to_json)
        }
        "workbuddy_credits_fetch" => {
            let user_id = opt_arg(&args, "user_id")?;
            let fresh = opt_arg(&args, "fresh")?;
            workbuddy::workbuddy_credits_fetch(state, user_id, fresh).and_then(to_json)
        }
        "workbuddy_credits_history_list" => {
            workbuddy::workbuddy_credits_history_list(state).and_then(to_json)
        }
        "workbuddy_editions_backfill" => {
            workbuddy::workbuddy_editions_backfill(state).and_then(to_json)
        }
        "workbuddy_settings_get" => to_json(workbuddy::workbuddy_settings_get(state)),
        "workbuddy_settings_set" => {
            let patch = arg(&args, "patch")?;
            workbuddy::workbuddy_settings_set(state, patch).and_then(to_json)
        }
        "workbuddy_accounts_export" => {
            let include_credentials = opt_arg(&args, "include_credentials")?;
            workbuddy::workbuddy_accounts_export(state, include_credentials).and_then(to_json)
        }
        "workbuddy_accounts_import" => {
            let payload = arg(&args, "payload")?;
            workbuddy::workbuddy_accounts_import(state, payload).and_then(to_json)
        }
        "workbuddy_oauth_login" => {
            // 扫码全流程在后台线程执行，事件经 "wb-oauth-progress"/"wb-oauth-done" 广播
            let events = admin.events.clone();
            let emit: workbuddy::WbOauthEmitter =
                Arc::new(move |name: &str, payload: serde_json::Value| {
                    let _ = events.send((name.to_string(), payload));
                });
            workbuddy::workbuddy_oauth_login(state, emit).and_then(to_json)
        }
        "workbuddy_usage_official" => {
            let user_id = opt_arg(&args, "user_id")?;
            let refresh = opt_arg(&args, "refresh")?;
            workbuddy::workbuddy_usage_official(state, user_id, refresh).and_then(to_json)
        }
        "workbuddy_usage_fallback" => workbuddy::workbuddy_usage_fallback(state).and_then(to_json),
        "workbuddy_credits_trend" => workbuddy::workbuddy_credits_trend(state).and_then(to_json),
        "workbuddy_usage_official_all" => {
            workbuddy::workbuddy_usage_official_all(state).and_then(to_json)
        }
        "workbuddy_activity_info" => {
            let user_id = opt_arg(&args, "user_id")?;
            let refresh = opt_arg(&args, "refresh")?;
            workbuddy::workbuddy_activity_info(state, user_id, refresh).and_then(to_json)
        }

        // ==================== scheduler（调度器） ====================
        "scheduler_status" => Ok(aiwork_core::scheduler::scheduler_status(state)),
        "scheduler_config_get" => Ok(aiwork_core::scheduler::scheduler_config_get(state)),
        "scheduler_config_set" => {
            let cfg = arg(&args, "config")?;
            aiwork_core::scheduler::scheduler_config_set(state, cfg).and_then(to_json)
        }

        // ==================== notify（通知渠道，Phase 3 T11） ====================
        "notify_config_get" => to_json(aiwork_core::notify::notify_config_get(state)),
        "notify_config_set" => {
            let config = arg(&args, "config")?;
            aiwork_core::notify::notify_config_set(state, config).and_then(to_json)
        }
        "notify_test" => {
            let config = arg(&args, "config")?;
            Ok(aiwork_core::notify::notify_test(state, config))
        }

        // ==================== ip_allowlist（IP 允许列表，Phase 3 T12a） ====================
        "ip_allowlist_get" => to_json(super::ip_allow::config_get(state)),
        "ip_allowlist_set" => {
            let config = arg(&args, "config")?;
            super::ip_allow::config_set(state, config).and_then(to_json)
        }

        // ==================== admin_tokens（多管理员附加令牌，Phase 3 T12b） ====================
        "admin_tokens_list" => to_json(super::admin_tokens::list(state)),
        "admin_token_create" => {
            let label = arg(&args, "label")?;
            super::admin_tokens::create(state, label).and_then(to_json)
        }
        "admin_token_revoke" => {
            let id = arg(&args, "id")?;
            super::admin_tokens::revoke(state, id).and_then(to_json)
        }

        // 未注册命令 → 404
        _ => Err(format!("{UNKNOWN_PREFIX}{name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试用 AdminState：AppState 字段直构（数据目录指向临时路径，不触碰真实数据）
    fn test_admin() -> AdminState {
        let dir = std::env::temp_dir().join(format!("aiwork_admin_bridge_{}", std::process::id()));
        let state = Arc::new(AppState {
            data_dir: dir,
            jwt_refresh_lock: Arc::new(std::sync::Mutex::new(())),
        });
        AdminState {
            state,
            guard: Arc::new(checkin_cmd::CheckinGuard(tokio::sync::Mutex::new(()))),
            events: tokio::sync::broadcast::channel(16).0,
            token: "test-token".to_string(),
        }
    }

    /// 未注册命令 → dispatch 返回「未知命令: 」前缀错误（handler 映射为 404）
    #[test]
    fn unknown_command_branch() {
        let admin = test_admin();
        let err = dispatch(&admin, "no_such_command", json!({})).unwrap_err();
        assert!(err.starts_with(UNKNOWN_PREFIX), "实际错误: {err}");
        assert_eq!(err, "未知命令: no_such_command");
    }

    /// 参数工具：必填缺失报错、可选 null/缺失归一 None
    #[test]
    fn arg_and_opt_arg_semantics() {
        let args = json!({"a": "x", "n": null, "list": [1, 2]});
        assert_eq!(arg::<String>(&args, "a").unwrap(), "x");
        assert!(arg::<String>(&args, "missing").is_err());
        assert_eq!(opt_arg::<String>(&args, "a").unwrap(), Some("x".into()));
        assert_eq!(opt_arg::<String>(&args, "n").unwrap(), None);
        assert_eq!(opt_arg::<String>(&args, "missing").unwrap(), None);
        assert_eq!(opt_arg::<Vec<i32>>(&args, "list").unwrap(), Some(vec![1, 2]));
    }
}
