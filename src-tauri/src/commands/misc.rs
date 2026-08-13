use std::process::Command;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::jwt;
use crate::models::{CreditRecord, CreditsFile, DeviceMap, Settings};
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

// ---------------- 代理请求日志 ----------------

#[derive(Serialize, Clone)]
pub struct ProxyLogEntry {
    pub id: String,
    pub timestamp: String,
    pub method: String,
    pub host: String,
    pub path: String,
    pub status: String,
    pub size: usize,
}

#[derive(Serialize)]
pub struct ProxyLogListResult {
    pub entries: Vec<ProxyLogEntry>,
    pub total: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyLogQueryOpts {
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

fn proxy_log_dir(state: &State<AppState>) -> std::path::PathBuf {
    let settings = state.settings();
    settings
        .proxy_log_path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| state.data_dir.join("proxy-logs"))
}

/// 解析单条代理日志，提取摘要信息
fn parse_proxy_entry(raw: &str, file_name: &str, index: usize) -> Option<ProxyLogEntry> {
    let lines: Vec<&str> = raw.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // 找到时间戳行: [2024-01-15 10:30:00] METHOD host/path
    // 或: [2024-01-15 10:30:00] [WebSocket Upgrade] host/path
    let header_line = lines.iter().find(|l| l.starts_with('['))?;
    let timestamp = header_line
        .get(1..20)
        .unwrap_or("")
        .to_string();

    let rest = &header_line[header_line.find("] ").map(|i| i + 2).unwrap_or(0)..];

    let (method, host, path, status) = if rest.starts_with("[WebSocket") {
        // WebSocket 条目
        let hp = rest.find("] ").map(|i| &rest[i + 2..]).unwrap_or(rest);
        let (host, path) = split_host_path(hp);
        ("WebSocket".to_string(), host, path, "101 Upgrade".to_string())
    } else {
        // 普通请求
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        let raw_method = parts.first().unwrap_or(&"").to_string();
        let hp = parts.get(1).unwrap_or(&"");
        let (host, path) = split_host_path(hp);
        // 从内容中提取状态码
        let status = raw
            .lines()
            .find(|l| l.starts_with("--- Response:"))
            .and_then(|l| {
                l.trim_start_matches("--- Response: ")
                    .trim_end_matches(" ---")
                    .to_string()
                    .into()
            })
            .unwrap_or_else(|| "-".to_string());
        (format!("HTTP {}", raw_method), host, path, status)
    };

    Some(ProxyLogEntry {
        id: format!("{}:{}", file_name, index),
        timestamp,
        method,
        host,
        path,
        status,
        size: raw.len(),
    })
}

fn split_host_path(hp: &str) -> (String, String) {
    // hp 可能是 "api.trae.cn/trae/api/..." 或 "api.trae.cn"
    if let Some(idx) = hp.find('/') {
        (hp[..idx].to_string(), hp[idx..].to_string())
    } else {
        (hp.to_string(), String::new())
    }
}

#[tauri::command]
pub fn proxy_logs_list(
    state: State<AppState>,
    opts: ProxyLogQueryOpts,
) -> Result<ProxyLogListResult, String> {
    let log_dir = proxy_log_dir(&state);
    if !log_dir.exists() {
        return Ok(ProxyLogListResult {
            entries: vec![],
            total: 0,
        });
    }

    // 列出所有 .log 文件，按文件名升序（旧文件在前）
    // 这样 all_entries 中条目按时间正序排列（旧→新），reverse() 后得到正确的时间倒序（新→旧）
    let mut files: Vec<String> = std::fs::read_dir(&log_dir)
        .map_err(|e| format!("读取代理日志目录失败: {e}"))?
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".log") {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    files.sort_by(|a, b| a.cmp(b));

    let keyword = opts.keyword.as_deref().unwrap_or("");
    let start = opts.start_time.as_deref().unwrap_or("");
    let end = opts.end_time.as_deref().unwrap_or("");
    let offset = opts.offset.unwrap_or(0);
    let limit = opts.limit.unwrap_or(50);

    let mut all_entries: Vec<ProxyLogEntry> = Vec::new();

    for file_name in &files {
        let path = log_dir.join(file_name);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // 按 ====== 分隔条目
        let mut index = 0;
        for chunk in content.split("================================================================================") {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                continue;
            }

            // 时间过滤
            if !start.is_empty() || !end.is_empty() {
                let ts = chunk
                    .lines()
                    .next()
                    .and_then(|l| l.get(1..20))
                    .unwrap_or("");
                if !start.is_empty() && ts < start {
                    continue;
                }
                if !end.is_empty() && ts > end {
                    continue;
                }
            }

            // 关键字过滤
            if !keyword.is_empty() && !chunk.to_lowercase().contains(&keyword.to_lowercase()) {
                continue;
            }

            if let Some(entry) = parse_proxy_entry(chunk, file_name, index) {
                all_entries.push(entry);
            }
            index += 1;
        }
    }

    // 文件按升序处理（旧→新），同文件内条目按写入顺序也是旧→新，
    // 因此 all_entries 整体为时间正序（旧→新），reverse() 后得到时间倒序（新→旧）
    all_entries.reverse();

    let total = all_entries.len();
    let entries = all_entries
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect();

    Ok(ProxyLogListResult { entries, total })
}

#[tauri::command]
pub fn proxy_log_detail(state: State<AppState>, id: String) -> Result<String, String> {
    // id 格式: "filename:index"
    let parts: Vec<&str> = id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Err("无效的日志 ID".into());
    }
    let file_name = parts[0];
    let index: usize = parts[1].parse().map_err(|_| "无效的索引")?;

    let log_dir = proxy_log_dir(&state);
    let path = log_dir.join(file_name);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取日志文件失败: {e}"))?;

    let mut current = 0;
    for chunk in content.split("================================================================================") {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        if current == index {
            return Ok(chunk.to_string());
        }
        current += 1;
    }

    Err("找不到指定的日志条目".into())
}

// ---------------- JWT 解析 ----------------

#[derive(Serialize)]
pub struct JwtParseResult {
    pub user_id: Option<String>,
    pub exp_hours: Option<f64>,
    pub exp_timestamp: Option<i64>,
    pub status: String,
}

#[tauri::command]
pub fn jwt_parse(_app: AppHandle, _state: State<AppState>, jwt: String) -> JwtParseResult {
    let info = jwt::parse(&jwt);
    let status = jwt::status_of(info.exp_hours).to_string();
    JwtParseResult {
        user_id: info.user_id,
        exp_hours: info.exp_hours,
        exp_timestamp: info.exp_timestamp,
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
                if let Some(ref date) = opts.date {
                    if !time.starts_with(date) {
                        continue;
                    }
                }
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
pub fn settings_set(state: State<AppState>, patch: serde_json::Value) -> Result<(), String> {
    let path = state.path("app_settings.json");
    // 读取现有设置，合并 patch 中出现的字段（真正的 patch 语义）
    let mut current: serde_json::Value = fs_utils::read_json(&path);
    // 文件不存在或内容为 null 时初始化为空对象，避免 patch 被丢弃
    if !current.is_object() {
        current = serde_json::json!({});
    }
    if let (Some(current_obj), Some(patch_obj)) =
        (current.as_object_mut(), patch.as_object())
    {
        for (k, v) in patch_obj {
            current_obj.insert(k.clone(), v.clone());
        }
    }
    fs_utils::write_json(&path, &current)
}

// ---------------- 积分历史（供看板/趋势图） ----------------

#[tauri::command]
pub fn credits_history(state: State<AppState>) -> Vec<CreditRecord> {
    fs_utils::read_json::<CreditsFile>(&state.path("credits_history.json")).records
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
    // schtasks /TR 不会继承当前进程环境变量，需在命令行中显式设置 TRAEDATA_DIR
    let tr = format!(
        "cmd /c set TRAEDATA_DIR={}&\"{}\" \"{}\"",
        data_dir,
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
