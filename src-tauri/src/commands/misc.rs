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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sse_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sse_tokens: Option<String>,
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
        .unwrap_or_else(|| state.logs_dir())
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
        sse_model: extract_sse_field(raw, "model"),
        sse_tokens: extract_sse_tokens(raw),
    })
}

/// 从 SSE Summary 区块中提取指定字段
fn extract_sse_field(raw: &str, field: &str) -> Option<String> {
    let in_summary = raw.lines().skip_while(|l| !l.starts_with("--- SSE Summary ---"));
    for line in in_summary {
        let line = line.trim();
        if line.starts_with("--- ") && !line.starts_with("--- SSE Summary") {
            break;
        }
        if let Some(rest) = line.strip_prefix(&format!("  {}: ", field)) {
            return Some(rest.to_string());
        }
    }
    None
}

/// 提取 token 用量摘要字符串
fn extract_sse_tokens(raw: &str) -> Option<String> {
    let pt = extract_sse_field(raw, "prompt_tokens")?;
    let ct = extract_sse_field(raw, "completion_tokens").unwrap_or_else(|| "?".to_string());
    let tt = extract_sse_field(raw, "total_tokens").unwrap_or_else(|| "?".to_string());
    Some(format!("p:{} c:{} t:{}", pt, ct, tt))
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

    // 列出所有 proxy_req_*.log 文件，按文件名升序（旧文件在前）
    // 这样 all_entries 中条目按时间正序排列（旧→新），reverse() 后得到正确的时间倒序（新→旧）
    let mut files: Vec<String> = std::fs::read_dir(&log_dir)
        .map_err(|e| format!("读取代理日志目录失败: {e}"))?
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().to_string_lossy().to_string();
            // 只匹配 proxy_req_ 前缀，排除 proxy.log（操作日志）和其他日志
            if name.starts_with("proxy_req_") && name.ends_with(".log") {
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
        let content = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
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
    let content = std::fs::read(&path)
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
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
        if let Ok(bytes) = std::fs::read(&p) {
            let content = String::from_utf8_lossy(&bytes);
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
    let output = Command::new("schtasks")
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
        .output()
        .map_err(|e| format!("注册计划任务失败: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { &stderr } else { &stdout };

        // 检测是否是权限不足
        let is_access_denied = detail.contains("Access is denied")
            || detail.contains("ERROR: Access is denied")
            || detail.contains("拒绝访问")
            || output.status.code() == Some(5);

        if is_access_denied {
            return Err(format!(
                "注册计划任务失败：权限不足（Access Denied）\n\n\
                 原因：使用 /RL HIGHEST 创建计划任务需要管理员权限，当前应用以普通用户身份运行。\n\n\
                 解决方法（任选其一）：\n\
                 1. 右键 TraeWorkAssistant → 「以管理员身份运行」后重新注册\n\
                 2. 打开「管理员命令提示符」手动执行：\n\
                    schtasks /Create /TN TraeWorkAssistant_DailyCheckin /TR \"cmd /c set TRAEDATA_DIR={}&\\\"{}\\\" \\\"{}\\\"\" /SC DAILY /ST {} /RL HIGHEST /F\n\
                 3. 如不需最高权限，可在管理员 CMD 中去掉 /RL HIGHEST 参数后重试\n\n\
                 详细错误：{detail}",
                data_dir, py.replace('\\', "/"), script.to_string_lossy().replace('\\', "/"), time,
            ));
        }

        return Err(format!("注册计划任务失败：{detail}"));
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
