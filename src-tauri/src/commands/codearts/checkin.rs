//! CodeArts 签到命令域：NDJSON 进度 / 结果查询 / schtasks 任务注册。
//!
//! 事件名 `codearts-checkin-progress`，payload 为序列化后的 JSON **字符串**
//!（前端逐行 JSON.parse），与 Qoder 同款。

use serde_json::json;
use tauri::{AppHandle, State};

use crate::state::AppState;
use crate::tasks::codearts_checkin::{self, CodeartsCheckinOpts};

/// 统一事件发射（payload 必须是 JSON 字符串，否则前端 parseLine 丢弃）。
fn emit_event(app: &AppHandle, data_dir: &std::path::Path, ev: &serde_json::Value) {
    let line = serde_json::to_string(ev).unwrap_or_default();
    let _ = crate::events::emit_logged(app, "codearts-checkin-progress", json!(line), Some(data_dir));
}

#[tauri::command]
pub fn codearts_checkin_start(
    app: AppHandle,
    state: State<AppState>,
    user_ids: Option<Vec<String>>,
    skip_checked_in: Option<bool>,
) -> Result<(), String> {
    let st = state.inner().clone();
    let data_dir = st.data_dir.clone();
    let opts = CodeartsCheckinOpts {
        user_ids,
        skip_checked: skip_checked_in.unwrap_or(true),
    };
    tauri::async_runtime::spawn(async move {
        let app2 = app.clone();
        let dd = data_dir.clone();
        let mut sink = move |ev: &serde_json::Value| emit_event(&app2, &dd, ev);
        let done = codearts_checkin::run_checkin_round(&st, &opts, &mut sink);
        // exit 终态（前端据此收尾）
        let ok = done.get("ok").and_then(serde_json::Value::as_i64).unwrap_or(0);
        emit_event(
            &app,
            &data_dir,
            &json!({"type":"exit","ok":ok,"done":done}),
        );
    });
    Ok(())
}

/// 签到结果（近 N 天，默认 30）。
#[tauri::command]
pub fn codearts_checkin_results(state: State<AppState>, days: Option<i64>) -> Vec<serde_json::Value> {
    let days = days.unwrap_or(30).clamp(1, 365);
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let mut rows: Vec<serde_json::Value> = crate::store::docs::codearts_checkin_results_load(
        &crate::store::db(&state.data_dir),
    )
    .into_iter()
    .filter(|r| r.get("date").and_then(serde_json::Value::as_str).unwrap_or("") >= cutoff.as_str())
    .collect();
    // 新→旧
    rows.sort_by(|a, b| {
        let ka = format!(
            "{}|{}",
            a.get("date").and_then(serde_json::Value::as_str).unwrap_or(""),
            a.get("time").and_then(serde_json::Value::as_str).unwrap_or("")
        );
        let kb = format!(
            "{}|{}",
            b.get("date").and_then(serde_json::Value::as_str).unwrap_or(""),
            b.get("time").and_then(serde_json::Value::as_str).unwrap_or("")
        );
        kb.cmp(&ka)
    });
    rows
}

const CODEARTS_TASK_PREFIX: &str = "AIWorkAssistant_CodeArtsCheckin";

/// 构造计划任务 /TR（主 exe 直调 CLI 任务模式，GUI 子系统不弹黑框）。
fn build_task_tr(task: &str) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取主程序路径失败: {e}"))?;
    Ok(format!("\"{}\" --task-run {task}", exe.to_string_lossy()))
}

fn task_names() -> Vec<String> {
    let prefix = format!("{CODEARTS_TASK_PREFIX}_");
    crate::commands::misc::list_task_names()
        .into_iter()
        .filter(|n| n.starts_with(&prefix))
        .collect()
}

fn delete_task_checked(name: &str) -> Result<(), String> {
    match crate::commands::misc::run_schtasks(&["/Delete", "/TN", name, "/F"]) {
        Ok((true, _, _)) => Ok(()),
        Ok((false, _, stderr)) => Err(if stderr.trim().is_empty() { "schtasks 返回失败".into() } else { stderr.trim().to_string() }),
        Err(e) => Err(e),
    }
}

/// 注册每日签到计划任务（任务名 `AIWorkAssistant_CodeArtsCheckin_<HHMM>`，先建后删）。
#[tauri::command]
pub fn codearts_checkin_task_register(state: State<AppState>, times: Vec<String>) -> Result<(), String> {
    if times.is_empty() {
        return Err("至少需要一个触发时间（如 10:20）".into());
    }
    for t in &times {
        crate::commands::misc::validate_hhmm(t)?;
    }
    let tr = build_task_tr("codearts-checkin")?;
    let mut new_names = Vec::with_capacity(times.len());
    for t in &times {
        let name = format!("{CODEARTS_TASK_PREFIX}_{}", t.replace(':', ""));
        let (ok, _, stderr) = crate::commands::misc::run_schtasks(&[
            "/Create", "/TN", &name, "/TR", &tr, "/SC", "DAILY", "/ST", t, "/F",
        ])?;
        if !ok {
            return Err(format!("注册任务 {t} 失败: {}（原任务未改动）", stderr.trim()));
        }
        crate::commands::misc::enable_task_wakeup_settings(&name);
        new_names.push(name);
    }
    for name in task_names() {
        if !new_names.contains(&name) {
            if let Err(e) = delete_task_checked(&name) {
                crate::fs_utils::app_log(&state.data_dir, &format!("清理旧 CodeArts 签到任务 {name} 失败: {e}"));
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub fn codearts_checkin_task_status() -> Vec<String> {
    let prefix = format!("{CODEARTS_TASK_PREFIX}_");
    task_names()
        .iter()
        .map(|name| match name.strip_prefix(&prefix) {
            Some(d) if d.len() == 4 && d.chars().all(|c| c.is_ascii_digit()) => {
                format!("{}:{}", &d[..2], &d[2..])
            }
            _ => name.trim_start_matches(&prefix).to_string(),
        })
        .collect()
}

#[tauri::command]
pub fn codearts_checkin_task_unregister(state: State<AppState>) -> Result<(), String> {
    let mut failed = Vec::new();
    for name in task_names() {
        if let Err(e) = delete_task_checked(&name) {
            crate::fs_utils::app_log(&state.data_dir, &format!("注销 CodeArts 签到任务 {name} 失败: {e}"));
            failed.push(format!("{name}: {e}"));
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("部分签到任务注销失败: {}", failed.join("；")))
    }
}

/// 启动延迟补签（60s），由 main.rs setup 调用。
pub fn startup_auto_checkin(app: AppHandle, state: AppState) {
    if !super::common::load_settings(&state).auto_checkin {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(60));
        let opts = CodeartsCheckinOpts::daily();
        let data_dir = state.data_dir.clone();
        let app2 = app.clone();
        let mut sink = move |ev: &serde_json::Value| emit_event(&app2, &data_dir, ev);
        let done = codearts_checkin::run_checkin_round(&state, &opts, &mut sink);
        let failed = done.get("failed").and_then(serde_json::Value::as_i64).unwrap_or(0);
        if failed > 0 {
            crate::fs_utils::app_log(
                &state.data_dir,
                &format!("CodeArts 启动补签：{failed} 个账号失败"),
            );
        }
    });
}
