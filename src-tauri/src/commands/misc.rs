use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::Command;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::jwt;
use crate::models::{DeviceMap, Settings};
use crate::state::AppState;

pub const INVITE_LINK: &str =
    "https://www.trae.cn/work-fission/4CP3KDBT5W9A?utm_source=copy_link&utm_medium=friends_invite";

// ---------------- 设备 ID ----------------

#[tauri::command]
pub fn device_reset(state: State<AppState>, user_id: String) -> Result<(), String> {
    let mut map: DeviceMap = fs_utils::read_json(&state.path("device_map.json"));
    map.remove(&user_id);
    fs_utils::write_json(&state.path("device_map.json"), &map)?;
    Ok(())
}

// ---------------- JWT 解析 ----------------

#[derive(Serialize)]
pub struct JwtParseResult {
    pub user_id: Option<String>,
    pub exp_hours: Option<f64>,
    pub status: String,
}

#[tauri::command]
pub fn jwt_parse(_app: AppHandle, _state: State<AppState>, jwt: String) -> JwtParseResult {
    let info = jwt::parse(&jwt);
    let status = jwt::status_of(info.exp_hours).to_string();
    JwtParseResult {
        user_id: info.user_id,
        exp_hours: info.exp_hours,
        status,
    }
}

// ---------------- 日志 ----------------

#[derive(Deserialize)]
pub struct LogsOpts {
    #[serde(default)]
    pub log_type: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct LogLine {
    pub time: String,
    pub log_type: String,
    pub message: String,
}

#[tauri::command]
pub fn logs_query(state: State<AppState>, opts: LogsOpts) -> Vec<LogLine> {
    let files = [
        ("proxy", "proxy.log"),
        ("checkin", "checkin.log"),
        ("switch", "switcher.log"),
    ];
    let mut out = Vec::new();
    for (t, fname) in files {
        if let Some(ref want) = opts.log_type {
            if want != "all" && want != t {
                continue;
            }
        }
        let p = state.path("logs").join(fname);
        if let Ok(content) = std::fs::read_to_string(&p) {
            for raw in content.lines() {
                let (time, msg) = split_time(raw);
                if let Some(kw) = &opts.keyword {
                    if !msg.contains(kw) && !time.contains(kw) {
                        continue;
                    }
                }
                out.push(LogLine {
                    time,
                    log_type: t.to_string(),
                    message: msg,
                });
            }
        }
    }
    out.sort_by(|a, b| b.time.cmp(&a.time));
    let limit = opts.limit.unwrap_or(500);
    out.into_iter().take(limit).collect()
}

fn split_time(raw: &str) -> (String, String) {
    if raw.starts_with('[') {
        if let Some(end) = raw.find("] ") {
            let time = raw[1..end].to_string();
            return (time, raw[end + 2..].to_string());
        }
    }
    ("".to_string(), raw.to_string())
}

// ---------------- 设置 ----------------

#[tauri::command]
pub fn settings_get(state: State<AppState>) -> Settings {
    state.settings()
}

#[tauri::command]
pub fn settings_set(state: State<AppState>, patch: Settings) -> Result<(), String> {
    fs_utils::write_json(&state.path("app_settings.json"), &patch)
}

// ---------------- 邀请 ----------------

#[derive(Serialize)]
pub struct Invite {
    pub url: String,
}

#[tauri::command]
pub fn invite_link(_app: AppHandle, _state: State<AppState>) -> Invite {
    Invite {
        url: INVITE_LINK.to_string(),
    }
}

// ---------------- 定时任务 ----------------

#[tauri::command]
pub fn task_register(state: State<AppState>, time: String) -> Result<(), String> {
    // 直接调用 python 签到脚本（无界面、可定时），注入数据目录
    let py = state.python_exe.clone();
    let script = state.python_dir.join("auto_checkin.py");
    let data_dir = state.data_dir.to_string_lossy().to_string();
    let tr = format!(
        "\"{}\" \"{}\"",
        py.replace('\\', "/"),
        script.to_string_lossy().replace('\\', "/")
    );
    let task_name = "TraeWorkAssistant_DailyCheckin";
    let status = Command::new("schtasks")
        .args([
            "/Create",
            "/TN",
            task_name,
            "/TR",
            &tr,
            "/SC",
            "DAILY",
            "/ST",
            &time,
            "/RL",
            "HIGHEST",
            "/F",
        ])
        .env("TRAEDATA_DIR", &data_dir)
        .status()
        .map_err(|e| format!("注册计划任务失败: {e}"))?;
    if !status.success() {
        return Err("注册计划任务失败（可能需要管理员权限）".into());
    }
    Ok(())
}

#[tauri::command]
pub fn task_status(_app: AppHandle, _state: State<AppState>) -> Result<String, String> {
    let out = Command::new("schtasks")
        .args(["/Query", "/TN", "TraeWorkAssistant_DailyCheckin", "/FO", "LIST"])
        .output()
        .map_err(|e| format!("查询计划任务失败: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[tauri::command]
pub fn task_unregister(_app: AppHandle, _state: State<AppState>) -> Result<(), String> {
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", "TraeWorkAssistant_DailyCheckin", "/F"])
        .status();
    Ok(())
}

// 避免未使用的导入告警
#[allow(dead_code)]
fn _keep(_: HashMap<String, String>) {}
