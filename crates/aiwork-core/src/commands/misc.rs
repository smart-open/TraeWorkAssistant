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

/// 待取启动通知（一次性）：后端启动期产生的用户可见消息（如分组统一迁移结果）
/// 落 kv("startup_notice")，前端 init 拉取 toast 展示后即删——通知不会因
/// 「事件先于监听注册发出」而丢失，也不会重复弹出。
pub fn startup_notice_take(state: &AppState) -> Option<String> {
    let store = crate::store::db(&state.data_dir);
    let msg: Option<String> = store.kv_get("startup_notice");
    if msg.is_some() {
        let _ = store.kv_delete("startup_notice");
    }
    msg
}

pub fn settings_get(state: &AppState) -> Settings {
    state.settings()
}

pub fn settings_set(state: &AppState, patch: serde_json::Value) -> Result<(), String> {
    // SQLite 化（P2）：app_settings 入 kv 文档（patch 合并语义不变）；
    // 原子读-改-写（移植 main d04de4fc 审查修复）：两段式 kv_get→kv_set 之间
    // 他方写入会被旧快照整体覆盖丢键，读改写改走 kv_update 同一连接锁区完成；
    // 校验 Err 整体放弃写回（语义同先校验后写）
    let store = crate::store::db(&state.data_dir);
    store.kv_update("app_settings", |current| {
        if let (Some(current_obj), Some(patch_obj)) =
            (current.as_object_mut(), patch.as_object())
        {
            for (k, v) in patch_obj {
                // C4：app_order 元素合法性校验——合法键走 models::is_valid_app_key 单一来源
                // （与 state.rs 读取侧归一同谓词），任意字符串不得入库；非法元素剔除 +
                // HashSet 全量去重保序（对齐读取侧，Vec::dedup 只去相邻重复）；
                // 非数组形态不写入（防破坏 Vec<String> 反序列化）
                if k == "app_order" {
                    let Some(arr) = v.as_array() else {
                        continue;
                    };
                    let mut seen = std::collections::HashSet::new();
                    let cleaned: Vec<serde_json::Value> = arr
                        .iter()
                        .filter_map(|x| x.as_str())
                        .filter(|s| crate::models::is_valid_app_key(s))
                        .filter(|s| seen.insert(*s))
                        .map(|s| serde_json::Value::String(s.to_string()))
                        .collect();
                    current_obj.insert(k.clone(), serde_json::Value::Array(cleaned));
                    continue;
                }
                current_obj.insert(k.clone(), v.clone());
            }
        }
        Ok::<_, String>(true)
    })
}
