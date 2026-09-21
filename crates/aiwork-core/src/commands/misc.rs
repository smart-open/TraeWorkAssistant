use serde::{Deserialize, Serialize};

use crate::jwt;
use crate::models::Settings;
use crate::state::AppState;

// ---------------- JWT 解析 ----------------

#[derive(Serialize)]
pub struct JwtParseResult {
    pub user_id: Option<String>,
    pub exp_hours: Option<f64>,
    pub exp_timestamp: Option<i64>,
    pub status: String,
}

pub fn jwt_parse(jwt: String) -> JwtParseResult {
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

pub fn logs_query(state: &AppState, opts: LogsOpts) -> Vec<LogLine> {
    let files = [
        ("proxy", "proxy.log"),
        ("checkin", "checkin.log"),
        ("switch", "switcher.log"),
        ("app", "app.log"),
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

/// 清理指定类型日志文件（proxy/checkin/switch/app；all 为全清）。
/// 各写入方均为「每次追加时重新打开」，删除后文件按需自动重建，无需特殊处理。
/// 返回实际删除的文件数。
pub fn logs_clear(state: &AppState, log_type: String) -> Result<u32, String> {
    let files = [
        ("proxy", "proxy.log"),
        ("checkin", "checkin.log"),
        ("switch", "switcher.log"),
        ("app", "app.log"),
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

pub fn settings_get(state: &AppState) -> Settings {
    state.settings()
}

pub fn settings_set(state: &AppState, patch: serde_json::Value) -> Result<(), String> {
    // SQLite 化（P2）：app_settings 入 kv 文档（patch 合并语义不变）
    let store = crate::store::db(&state.data_dir);
    let mut current: serde_json::Value = store.kv_get("app_settings");
    // 内容为 null 时初始化为空对象，避免 patch 被丢弃
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
    store.kv_set("app_settings", &current)
}
