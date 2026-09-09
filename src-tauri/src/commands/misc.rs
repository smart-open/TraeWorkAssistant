use std::os::windows::process::CommandExt;
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

// ---------------- 开机自启（T11，tauri-plugin-autostart：Windows 写注册表 Run 项） ----------------

/// 查询开机自启状态
#[tauri::command]
pub fn autostart_status(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// 设置开机自启（即时生效，安装版/便携版均写当前 exe 路径）
#[tauri::command]
pub fn autostart_set(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let autolaunch = app.autolaunch();
    if enabled {
        autolaunch.enable().map_err(|e| e.to_string())
    } else {
        autolaunch.disable().map_err(|e| e.to_string())
    }
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
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    if raw.starts_with('[') {
        if let Some(end) = raw.find("] ") {
            let time = raw[1..end].to_string();
            return (time, raw[end + 2..].to_string());
        }
    }
    ("".to_string(), raw.to_string())
}

/// 清理指定类型日志文件（proxy/checkin/switch；all 为全清）。
/// 各写入方均为「每次追加时重新打开」，删除后文件按需自动重建，无需特殊处理。
/// 返回实际删除的文件数。
#[tauri::command]
pub fn logs_clear(state: State<AppState>, log_type: String) -> Result<u32, String> {
    let files = [
        ("proxy", "proxy.log"),
        ("checkin", "checkin.log"),
        ("switch", "switcher.log"),
    ];
    let mut removed = 0u32;
    for (t, fname) in files {
        if log_type != "all" && log_type != t {
            continue;
        }
        let p = state.path("logs").join(fname);
        match std::fs::remove_file(&p) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("删除 {fname} 失败: {e}")),
        }
    }
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!("已清理日志: {log_type}（删除 {removed} 个文件）"),
    );
    Ok(removed)
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

// ---------------- 文件导出 ----------------

#[tauri::command]
pub fn write_text_file(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content.as_bytes()).map_err(|e| format!("写入文件失败: {e}"))
}

/// 读取本地文本文件（配合导入账号：文件选择后由 Rust 侧读取，避免前端路径权限问题）
#[tauri::command]
pub fn read_text_file(path: String) -> Result<String, String> {
    let meta = std::fs::metadata(&path).map_err(|e| format!("读取文件失败: {e}"))?;
    if !meta.is_file() {
        return Err("路径不是常规文件".into());
    }
    // 大小上限 10MB：导入的账号/配置 JSON 远小于此，防止误选超大文件拖垮前端
    if meta.len() > 10 * 1024 * 1024 {
        return Err("文件过大（超过 10MB），请确认选择的是账号/配置 JSON 文件".into());
    }
    let bytes =
        std::fs::read(&path).map_err(|e| format!("读取文件失败: {e}"))?;
    String::from_utf8(bytes).map_err(|_| "文件不是有效的 UTF-8 文本".into())
}

// ---------------- 定时任务 ----------------

/// 新版每日签到计划任务名（品牌 ai-work-assistant）
pub const TASK_NAME: &str = "AIWorkAssistant_DailyCheckin";
/// 旧版计划任务名（品牌迁移前），启动时自动迁移到新任务名
pub const LEGACY_TASK_NAME: &str = "TraeWorkAssistant_DailyCheckin";
/// 豆包会话续期每日计划任务名（P3）
pub const DOUBAO_TASK_NAME: &str = "AIWorkAssistant_DoubaoRenew";
/// 豆包额度巡检每日计划任务名（批量查额度 + 回写缓存/历史 + 用完记录）
pub const DOUBAO_QUOTA_TASK_NAME: &str = "AIWorkAssistant_DoubaoQuotaCheck";

// 运行 schtasks 并正确解码输出。
// 关键：默认控制台代码页是 GBK（中文 Windows），schtasks 的中文报错(如"系统找不到指定的文件")
// 以 GBK 字节输出；若直接 from_utf8_lossy 会读成 ϵͳ... 乱码，导致 "找不到" 永远匹配不上、
// 错误文案变成乱码。前置 `chcp 65001` 让 schtasks 以 UTF-8 输出，从而能正确匹配与展示。
// 返回 (成功?, stdout, stderr)，三者均为 UTF-8 字符串。
pub(crate) fn run_schtasks(args: &[&str]) -> Result<(bool, String, String), String> {
    let mut full: Vec<String> = vec![
        "/c".to_string(),
        "chcp".to_string(),
        "65001".to_string(),
        ">nul".to_string(),
        "&&".to_string(),
        "schtasks".to_string(),
    ];
    for a in args {
        full.push((*a).to_string());
    }
    let out = Command::new("cmd")
        .args(&full)
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| format!("执行 schtasks 失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    Ok((out.status.success(), stdout, stderr))
}

/// 构造计划任务的 /TR 命令行（直接调用 python 签到脚本，注入数据目录）
fn build_task_tr(state: &AppState) -> String {
    let py = state.python_exe.clone();
    let script = state.python_dir.join("auto_checkin.py");
    let data_dir = state.data_dir.to_string_lossy().to_string();
    // schtasks /TR 不会继承当前进程环境变量，需在命令行中显式设置 AIWORKDATA_DIR。
    // 必须用 set "VAR=value"（带引号）以兼容含空格的路径（如 C:\Users\<带空格用户名>\...）；
    // 用 && 串联，仅当 set 成功后才执行 python。
    // 不再使用 /RL HIGHEST：签到脚本只读取/写入 %APPDATA% 并运行 python，无需提权，
    // 否则普通用户会卡在「access denied」而注册失败（详见问题分析报告）。
    format!(
        "cmd /c set \"AIWORKDATA_DIR={}\" && \"{}\" \"{}\"",
        data_dir,
        py.replace('\\', "/"),
        script.to_string_lossy().replace('\\', "/")
    )
}

/// 注册每日签到任务（新任务名），供命令与旧任务迁移共用
fn register_daily_task(state: &AppState, time: &str) -> Result<(), String> {
    let tr = build_task_tr(state);
    let (ok, _stdout, stderr) = run_schtasks(&[
        "/Create",
        "/TN",
        TASK_NAME,
        "/TR",
        tr.as_str(),
        "/SC",
        "DAILY",
        "/ST",
        time,
        "/F",
    ])?;
    if !ok {
        let detail = stderr.trim();
        // 权限不足：最常见的失败原因（/RL HIGHEST 或普通用户受限）
        let is_access_denied = detail.contains("Access is denied")
            || detail.contains("ERROR: Access is denied")
            || detail.contains("拒绝访问")
            || detail.contains("权限");
        if is_access_denied {
            return Err(format!(
                "权限不足（Access Denied）。\n\n\
                 解决方法（任选其一）：\n\
                 1. 右键 AI Work 助手 →「以管理员身份运行」后重新点击「注册任务」\n\
                 2. 打开「管理员命令提示符」手动执行：\n\
                    schtasks /Create /TN {TASK_NAME} /TR \"cmd /c set \\\"AIWORKDATA_DIR={}\\\" && \\\"{}\\\" \\\"{}\\\"\" /SC DAILY /ST {time} /F\n\
                 3. 如不需最高权限，可去掉 /RL HIGHEST 后重试",
                state.data_dir.to_string_lossy(),
                state.python_exe.replace('\\', "/"),
                state.python_dir.join("auto_checkin.py").to_string_lossy().replace('\\', "/")
            ));
        }
        return Err(detail.to_string());
    }
    Ok(())
}

// 计划任务命令含 schtasks 子进程调用（可达数秒），标记 async 派发到线程池执行，避免阻塞 UI
#[tauri::command(async)]
pub fn task_register(state: State<AppState>, time: String) -> Result<(), String> {
    register_daily_task(&state, &time)?;
    // 注册成功后清理旧版计划任务（品牌迁移），失败不影响本次注册
    let _ = run_schtasks(&["/Delete", "/TN", LEGACY_TASK_NAME, "/F"]);
    Ok(())
}

/// 查询计划任务是否存在
fn task_exists(name: &str) -> bool {
    let (ok, _stdout, _stderr) = match run_schtasks(&["/Query", "/TN", name, "/FO", "LIST"]) {
        Ok(v) => v,
        Err(_) => return false,
    };
    ok
}

/// 导出计划任务 XML 并解析每日触发时间（HH:MM）。
/// schtasks /XML 输出为 UTF-16LE（带 BOM），需按 UTF-16 解码；解析失败返回 None。
fn legacy_task_start_time(name: &str) -> Option<String> {
    let out = Command::new("cmd")
        .args([
            "/c",
            "chcp",
            "65001",
            ">nul",
            "&&",
            "schtasks",
            "/Query",
            "/TN",
            name,
            "/XML",
        ])
        .creation_flags(0x08000000)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let xml = if out.stdout.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = out.stdout[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let start = xml
        .split("<StartBoundary>")
        .nth(1)?
        .split("</StartBoundary>")
        .next()?;
    // 形如 2026-09-07T09:30:00 → 取 T 后的 HH:MM
    let time = start.split('T').nth(1)?;
    let hh_mm = time.get(0..5)?;
    let ok = hh_mm.len() == 5
        && hh_mm.as_bytes()[2] == b':'
        && hh_mm.chars().all(|c| c.is_ascii_digit() || c == ':');
    if ok { Some(hh_mm.to_string()) } else { None }
}

/// 旧版计划任务自动迁移（品牌迁移，**并存语义**）：
/// 检测到 TraeWorkAssistant_DailyCheckin（指向旧 exe/旧数据目录）时，
/// 按其原触发时间重建 AIWorkAssistant_DailyCheckin；**旧任务始终保留**，
/// 供老应用继续使用（两版并存，各自独立签到）。任何一步失败都静默跳过。
pub fn try_migrate_legacy_task(state: &AppState) -> Option<String> {
    let legacy = task_exists(LEGACY_TASK_NAME);
    if !legacy {
        return None;
    }
    if task_exists(TASK_NAME) {
        // 新旧并存（如用户手动注册过新任务）：旧任务属老应用，保留不动
        return Some(
            "任务迁移：新旧计划任务并存，各自服务对应应用（旧任务保留给老应用）".to_string(),
        );
    }
    let time = match legacy_task_start_time(LEGACY_TASK_NAME) {
        Some(t) => t,
        None => {
            return Some(
                "任务迁移：检测到旧任务 TraeWorkAssistant_DailyCheckin，但未能解析其触发时间，请在设置页手动注册新任务"
                    .to_string(),
            )
        }
    };
    match register_daily_task(state, &time) {
        Ok(()) => Some(format!(
            "任务迁移：已按旧任务触发时间创建新任务 {TASK_NAME}（每日 {time}）；旧任务保留供老应用继续使用"
        )),
        Err(e) => Some(format!(
            "任务迁移：检测到旧任务 {LEGACY_TASK_NAME}，创建新任务失败（{e}），可在设置页手动注册"
        )),
    }
}

#[tauri::command(async)]
pub fn task_status(state: State<AppState>, _app: AppHandle) -> Result<String, String> {
    let (ok, stdout, stderr) = run_schtasks(&["/Query", "/TN", TASK_NAME, "/FO", "LIST"])?;
    if !ok {
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        // 任务本就不存在：返回友好提示而非带乱码的错误，避免前端叠加"查询失败："前缀
        if detail.contains("can't find")
            || detail.contains("找不到")
            || detail.contains("does not exist")
            || detail.contains("ERROR: The system cannot find")
            || detail.contains("系统找不到")
        {
            // 尝试顺带迁移旧任务；迁移成功则再次查询
            if try_migrate_legacy_task(&state).is_some() && task_exists(TASK_NAME) {
                let (ok2, stdout2, _e2) =
                    run_schtasks(&["/Query", "/TN", TASK_NAME, "/FO", "LIST"])?;
                if ok2 {
                    return Ok(stdout2);
                }
            }
            return Ok("未注册每日签到任务（请先在设置页点击「注册任务」）。".to_string());
        }
        return Err(detail.to_string());
    }
    Ok(stdout)
}

#[tauri::command(async)]
pub fn task_unregister(_app: AppHandle, _state: State<AppState>) -> Result<(), String> {
    // 只删除本应用的新任务名；旧任务 TraeWorkAssistant_DailyCheckin 属老应用，
    // 两版并存时不得越权删除（老应用的签到计划需继续工作）
    let mut last_detail = String::new();
    let mut deleted = false;
    for name in [TASK_NAME] {
        let (ok, _stdout, stderr) = run_schtasks(&["/Delete", "/TN", name, "/F"])?;
        if ok {
            deleted = true;
            continue;
        }
        let detail = stderr.trim();
        // 任务本就不存在：视为已删除，不报错
        if detail.contains("can't find")
            || detail.contains("找不到")
            || detail.contains("does not exist")
            || detail.contains("ERROR: The system cannot find")
            || detail.contains("系统找不到")
        {
            continue;
        }
        last_detail = detail.to_string();
    }
    if !deleted && !last_detail.is_empty() {
        return Err(last_detail);
    }
    Ok(())
}
