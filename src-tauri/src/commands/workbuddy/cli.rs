//! CodeBuddy CLI 切号桥 + 五重防护自动轮换（F-06/F-59，批次3 T3.4）（原 workbuddy.rs 机械拆分）。
//! 纯逻辑（decide_target / settings env token JSON 操作 / 活动扫描）在 workbuddy_cli.rs；
//! 本模块为有状态粘合：账号池 / token store / 轮换状态文件 / 后台线程。
//! 凭证红线：token 不进日志、不进返回值（activeAccountId 为池内稳定 id）。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。

use std::path::PathBuf;
use tauri::State;

use crate::fs_utils;
use crate::state::AppState;
use crate::workbuddy_cli;

use super::common::{
    account_id_of, as_str, cli_settings_path, load_pool, load_settings, restore_cli_settings,
    token_store_path,
};

pub(super) fn cli_rotate_state_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("wb_cli_rotate_state.json")
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub(super) struct CliRotateState {
    #[serde(default)]
    pub(super) last_switch_at_ms: Option<i64>,
    #[serde(default)]
    pub(super) active_account_id: Option<String>,
    /// 轮换日志（旧→新，cap 50）
    #[serde(default)]
    pub(super) logs: Vec<serde_json::Value>,
}

pub(super) fn load_cli_rotate_state(state: &AppState) -> CliRotateState {
    fs_utils::read_json(&cli_rotate_state_path(state))
}

fn append_cli_log(_state: &AppState, st: &mut CliRotateState, mut entry: serde_json::Value) {
    if entry.get("ts").is_none() {
        entry["ts"] = serde_json::json!(chrono::Utc::now().timestamp_millis());
    }
    st.logs.push(entry);
    let len = st.logs.len();
    if len > 50 {
        st.logs.drain(..len - 50);
    }
}

/// CLI 当前账号 = settings.json env token 的稳定池 id（wb-<sha256 前 12 位>）。
fn cli_current_account_id() -> Option<String> {
    let raw = fs_utils::read_json::<serde_json::Value>(&cli_settings_path());
    workbuddy_cli::settings_env_token(&raw).map(|t| account_id_of(&t))
}

/// 从 token store 取账号 access_token（CLI 桥唯一凭证来源；auth 文件只读态不入桥）。
fn cli_token_of(state: &AppState, account_id: &str) -> Option<String> {
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(state));
    let rec = store.get("tokens").and_then(|t| t.get(account_id)).cloned().unwrap_or_default();
    as_str(fs_utils::dig(&rec, &["access_token"])).filter(|t| !t.is_empty())
}

/// 轮换候选：积分缓存（credits_fetch 回写）+ 账号池凭证 → CliCandidate。
/// 缓存缺失/无凭证 → invalid 候选（供日志与防横跳参照，不作为目标）。
fn cli_candidates(state: &AppState) -> Vec<workbuddy_cli::CliCandidate> {
    let cache: serde_json::Value =
        fs_utils::read_json(&state.data_dir.join("data").join("workbuddy_credits_cache.json"));
    let cache_accounts = cache.get("accounts").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let pool = load_pool(state);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let has_store = |id: &str| cli_token_of(state, id).is_some();

    pool.accounts
        .iter()
        .map(|a| {
            let display = if a.nickname.is_empty() { a.id.clone() } else { a.nickname.clone() };
            if !has_store(&a.id) {
                return workbuddy_cli::CliCandidate {
                    account_id: a.id.clone(),
                    display_name: display,
                    soonest_expire_at_ms: None,
                    total_remaining: 0.0,
                    valid: false,
                    error: Some("无工具侧凭证副本".into()),
                };
            }
            let Some(acc) = cache_accounts
                .iter()
                .find(|c| c.get("user_id").and_then(|v| v.as_str()) == Some(a.id.as_str()))
            else {
                return workbuddy_cli::CliCandidate {
                    account_id: a.id.clone(),
                    display_name: display,
                    soonest_expire_at_ms: None,
                    total_remaining: 0.0,
                    valid: false,
                    error: Some("积分缓存缺失（请先在积分页刷新）".into()),
                };
            };
            // 未过期且仍有剩余的包 → 合计剩余 + 最早到期（缓存 expire_ts 为 Unix 秒）
            let mut total = 0.0f64;
            let mut soonest: Option<i64> = None;
            if let Some(pkgs) = acc.get("packages").and_then(|v| v.as_array()) {
                for p in pkgs {
                    let remaining = p.get("remaining").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let expire_s = p.get("expire_ts").and_then(|v| v.as_i64());
                    let expired = matches!(expire_s, Some(s) if s * 1000 <= now_ms);
                    if !expired && remaining > 0.0 {
                        total += remaining;
                        if let Some(s) = expire_s {
                            let ms = s * 1000;
                            soonest = Some(soonest.map_or(ms, |cur| cur.min(ms)));
                        }
                    }
                }
            }
            workbuddy_cli::CliCandidate {
                account_id: a.id.clone(),
                display_name: display,
                soonest_expire_at_ms: soonest,
                total_remaining: total,
                valid: total > 0.0,
                error: if total > 0.0 { None } else { Some("无剩余积分（或已全部过期）".into()) },
            }
        })
        .collect()
}

fn cli_rotate_config(state: &AppState) -> (i64, i64, i64, i64, i64, f64) {
    let s = load_settings(state);
    (
        s.cli_cooldown_minutes.max(1) * 60_000,
        s.cli_min_gap_hours.max(0) * 3600_000,
        s.cli_min_urgency_hours.max(0) * 3600_000,
        s.cli_active_guard_minutes.max(0) * 60_000,
        s.cli_rotate_interval_minutes.max(5),
        s.cli_min_remaining_credits.max(0.0),
    )
}

/// CLI 轮换状态（当前 CLI 账号 + 五重防护配置 + 上次切换），供前端展示。
fn cli_status_value(state: &AppState) -> serde_json::Value {
    let st = load_cli_rotate_state(state);
    let s = load_settings(state);
    let settings_raw = fs_utils::read_json::<serde_json::Value>(&cli_settings_path());
    let token_present = workbuddy_cli::settings_env_token(&settings_raw).is_some();
    let current_id = cli_current_account_id();
    let pool = load_pool(state);
    let active_name = current_id
        .as_ref()
        .and_then(|id| pool.accounts.iter().find(|a| &a.id == id))
        .map(|a| if a.nickname.is_empty() { a.id.clone() } else { a.nickname.clone() });
    serde_json::json!({
        "settings_present": cli_settings_path().is_file(),
        "env_token_present": token_present,
        // 进程环境变量会覆盖 settings.json（CLI 启动时取 env 优先）——检测并提示
        "environment_override": std::env::var_os(workbuddy_cli::AUTH_ENV_KEY)
            .map(|v| !v.to_string_lossy().trim().is_empty())
            .unwrap_or(false),
        "active_account_id": current_id,
        "active_account_name": active_name,
        "recent_activity_ms": workbuddy_cli::cli_recent_activity(),
        "last_switch_at_ms": st.last_switch_at_ms,
        "config": {
            "cli_rotate_enabled": s.cli_rotate_enabled,
            "cli_rotate_interval_minutes": s.cli_rotate_interval_minutes,
            "cli_cooldown_minutes": s.cli_cooldown_minutes,
            "cli_min_gap_hours": s.cli_min_gap_hours,
            "cli_min_urgency_hours": s.cli_min_urgency_hours,
            "cli_active_guard_minutes": s.cli_active_guard_minutes,
            "cli_min_remaining_credits": s.cli_min_remaining_credits,
        },
    })
}

/// 写 settings.json env 认证（保留其余字段；写入后回读校验）。
fn apply_cli_token(token: &str) -> Result<(), String> {
    let path = cli_settings_path();
    let mut value: serde_json::Value = fs_utils::read_json(&path);
    if !value.is_object() {
        value = serde_json::json!({});
    }
    workbuddy_cli::with_env_token(&mut value, token)?;
    fs_utils::write_json(&path, &value)?;
    // 回读校验：写入的认证信息必须与目标账号一致
    let verify = fs_utils::read_json::<serde_json::Value>(&path);
    if workbuddy_cli::settings_env_token(&verify).as_deref() != Some(workbuddy_cli::clean_bearer(token)) {
        return Err("写入后回读校验失败：CodeBuddy settings.json 认证信息与目标账号不一致".into());
    }
    Ok(())
}

#[tauri::command]
pub fn workbuddy_cli_status(state: State<AppState>) -> serde_json::Value {
    cli_status_value(&state)
}

/// 手动切 CLI 账号（F-06 桥）：token store 凭证 → settings.json env，记录状态与日志。
#[tauri::command(async)]
pub fn workbuddy_cli_bridge_set(state: State<AppState>, user_id: String) -> Result<serde_json::Value, String> {
    let pool = load_pool(&state);
    let acct = pool
        .accounts
        .iter()
        .find(|a| a.id == user_id)
        .ok_or_else(|| format!("账号不存在: {user_id}"))?;
    let display = if acct.nickname.is_empty() { acct.id.clone() } else { acct.nickname.clone() };
    let token = cli_token_of(&state, &acct.id)
        .ok_or_else(|| "该账号无工具侧凭证副本，请先导入凭证（auth 导入或续期回写）".to_string())?;

    let previous = std::fs::read_to_string(cli_settings_path()).ok();
    apply_cli_token(&token).map_err(|e| {
        // 写入失败回滚：原子恢复写入前的 settings.json 原文（审查 P2）
        if let Some(prev) = &previous {
            restore_cli_settings(prev);
        }
        e
    })?;

    let mut st = load_cli_rotate_state(&state);
    st.active_account_id = Some(acct.id.clone());
    append_cli_log(
        &state,
        &mut st,
        serde_json::json!({
            "action": "manual",
            "to": {"id": acct.id, "name": display},
            "reason": "手动切换 CLI 账号",
        }),
    );
    fs_utils::write_json(&cli_rotate_state_path(&state), &st)?;
    fs_utils::app_log(&state.data_dir, &format!("CodeBuddy CLI 手动切号: {} ({})", display, acct.id));
    Ok(cli_status_value(&state))
}

/// 执行一轮五重防护轮换（F-59）：候选来自积分缓存 → decide_target → 写 CLI settings。
#[tauri::command(async)]
pub fn workbuddy_cli_rotate_run(state: State<AppState>) -> serde_json::Value {
    cli_rotate_cycle(&state)
}

/// 轮换日志（新→旧，供前端展示；无凭证字段）。
#[tauri::command]
pub fn workbuddy_cli_rotate_logs(state: State<AppState>, limit: Option<usize>) -> Vec<serde_json::Value> {
    let st = load_cli_rotate_state(&state);
    let mut logs = st.logs;
    logs.reverse();
    logs.into_iter().take(limit.unwrap_or(20)).collect()
}

/// 轮换周期核心（手动命令与后台线程共用；线程路径静默，不弹通知）。
fn cli_rotate_cycle(state: &AppState) -> serde_json::Value {
    let (cooldown_ms, gap_ms, urgency_ms, guard_ms, _interval, min_remaining) = cli_rotate_config(state);
    let candidates = cli_candidates(state);
    let current = cli_current_account_id();
    let st0 = load_cli_rotate_state(state);
    let now_ms = chrono::Utc::now().timestamp_millis();

    let decision = workbuddy_cli::decide_target(
        &candidates,
        current.as_deref(),
        now_ms,
        st0.last_switch_at_ms,
        cooldown_ms,
        gap_ms,
        urgency_ms,
        workbuddy_cli::cli_recent_activity(),
        guard_ms,
        min_remaining,
    );

    // 候选快照入日志（供观察后调整 min_remaining 等阈值）
    let detail: Vec<serde_json::Value> = candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.display_name,
                "remaining": c.total_remaining,
                "soonest_expire_at": c.soonest_expire_at_ms,
                "valid": c.valid,
                "error": c.error,
            })
        })
        .collect();
    let mut entry = serde_json::json!({
        "ts": now_ms,
        "action": "noop",
        "reason": serde_json::Value::Null,
        "from": current,
        "to": serde_json::Value::Null,
        "detail": detail,
    });

    let result = match &decision {
        workbuddy_cli::RotateDecision::Skip(reason) => {
            entry["action"] = serde_json::json!("skipped");
            entry["reason"] = serde_json::json!(reason);
            serde_json::json!({"status": "skipped", "reason": reason})
        }
        workbuddy_cli::RotateDecision::Switch(target_id) => {
            let display = candidates
                .iter()
                .find(|c| &c.account_id == target_id)
                .map(|c| c.display_name.clone())
                .unwrap_or_else(|| target_id.clone());
            match cli_token_of(state, target_id)
                .ok_or_else(|| "目标账号无工具侧凭证副本".to_string())
                .and_then(|token| {
                    let previous = std::fs::read_to_string(cli_settings_path()).ok();
                    apply_cli_token(&token).map_err(|e| {
                        // 写入失败回滚：原子恢复写入前的 settings.json 原文（审查 P2）
                        if let Some(prev) = &previous {
                            restore_cli_settings(prev);
                        }
                        e
                    })
                }) {
                Ok(()) => {
                    entry["action"] = serde_json::json!("switched");
                    entry["to"] = serde_json::json!({"id": target_id, "name": display});
                    serde_json::json!({"status": "switched", "to": {"id": target_id, "name": display}})
                }
                Err(e) => {
                    entry["action"] = serde_json::json!("error");
                    entry["reason"] = serde_json::json!(e);
                    serde_json::json!({"status": "error", "error": e})
                }
            }
        }
    };

    let mut st = st0;
    if result.get("status") == Some(&serde_json::json!("switched")) {
        st.last_switch_at_ms = Some(now_ms);
        st.active_account_id = result
            .pointer("/to/id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    append_cli_log(state, &mut st, entry);
    let _ = fs_utils::write_json(&cli_rotate_state_path(state), &st);
    if result.get("status") == Some(&serde_json::json!("switched")) {
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "CodeBuddy CLI 自动轮换: → {}",
                result.pointer("/to/name").and_then(|v| v.as_str()).unwrap_or("?")
            ),
        );
    }
    result
}

/// 后台轮换线程（F-59 检查间隔）：按配置间隔静默执行；开关关闭时空转。
/// 独立重建 AppState（cli_rotate_cycle 仅依赖 data_dir 下文件，无 UI 事件）。
pub fn start_cli_rotate_thread() {
    std::thread::spawn(move || {
        loop {
            // 先睡再查：避开启动高峰；间隔每轮重读（配置可随时改）
            let state = match AppState::new() {
                Ok(s) => s,
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_secs(300));
                    continue;
                }
            };
            let s = load_settings(&state);
            let interval = s.cli_rotate_interval_minutes.max(5) as u64;
            std::thread::sleep(std::time::Duration::from_secs(interval * 60));
            if !s.cli_rotate_enabled {
                continue;
            }
            let _ = cli_rotate_cycle(&state);
        }
    });
}
