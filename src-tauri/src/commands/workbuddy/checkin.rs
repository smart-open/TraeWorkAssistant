//! WorkBuddy 签到域（原 workbuddy.rs 机械拆分）：M4 签到/成长（F-15/F-17，python NDJSON 管线）、
//! 签到结果（F-15）、每日定时任务（F-16/F-55）、UI 坐标点击兜底（T4.2/F-18）、启动自动补签（F-55）。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。

use serde::Serialize;
use std::os::windows::process::CommandExt;
use std::process::Command;
use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::state::AppState;

use super::common::{checkin_results_path, load_settings, push_notify, spawn_wb_script};

// ── M4 签到（F-15，python NDJSON 管线）─────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct WbCheckinOpts {
    #[serde(default)]
    pub user_ids: Option<Vec<String>>,
    #[serde(default)]
    pub skip_checked_in: bool,
    #[serde(default)]
    pub skip_expired: bool,
    #[serde(default)]
    pub lazy_hours: Option<i64>,
}

/// 签到/成长全局轮次锁（审查 P1）：python 签到/成长管线共享 uid 文件锁与结果落盘，
/// 并发轮次会互相踩踏——tokio Mutex try_lock 拿不到即拒绝，不排队不阻塞。
static WB_ROUND_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 入口尝试获取轮次锁；guard 移交 spawn_wb_script 工作线程并持有至脚本退出
///（RAII：正常结束与 panic 展开均可靠释放，防泄漏）。
fn try_acquire_wb_round() -> Result<tokio::sync::MutexGuard<'static, ()>, String> {
    WB_ROUND_LOCK
        .try_lock()
        .map_err(|_| "已有签到/成长任务在执行中，请等待当前轮次完成".to_string())
}

/// 启动 WorkBuddy 签到（python workbuddy_checkin.py --json-stream），
/// NDJSON → `wb-checkin-progress` 事件（独立管线，避免与 Trae checkin 状态串扰）。
#[tauri::command(async)]
pub fn workbuddy_checkin_start(app: AppHandle, state: State<AppState>, opts: WbCheckinOpts) -> Result<(), String> {
    let round = try_acquire_wb_round()?;
    let mut args: Vec<String> = vec!["--json-stream".into()];
    if opts.skip_checked_in {
        args.push("--skip-checked".into());
    }
    if opts.skip_expired {
        args.push("--skip-expired".into());
    }
    if let Some(lh) = opts.lazy_hours {
        args.push("--lazy-hours".into());
        args.push(lh.to_string());
    }
    for uid in opts.user_ids.unwrap_or_default() {
        args.push("--uid".into());
        args.push(uid);
    }
    spawn_wb_script(app, &state, "workbuddy_checkin.py", &args, "wb-checkin-progress", round)
}

/// 成长中心执行（F-17，批次2 消费；批次1 端点已就绪时 python 会按开关执行）
#[tauri::command(async)]
pub fn workbuddy_growth_run(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let round = try_acquire_wb_round()?;
    let s = load_settings(&state);
    let mut args: Vec<String> = vec!["--growth".into()];
    if s.growth_travel { args.push("--growth-travel".into()); }
    if s.growth_lottery { args.push("--growth-lottery".into()); }
    if s.growth_tasks { args.push("--growth-tasks".into()); }
    spawn_wb_script(app, &state, "workbuddy_checkin.py", &args, "wb-checkin-progress", round)
}

// ── 签到结果 / 定时任务（F-15/F-16/F-55）───────────────────────────────────

#[derive(Serialize, Clone)]
pub struct WbCheckinRecord {
    pub date: String,
    pub time: String,
    pub user_id: String,
    pub name: String,
    pub status: String,
    pub message: String,
    /// 签到获得积分（接口返回或前后余额差值兜底；无则为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reward: Option<f64>,
}

/// 签到日志（90 天存储，UI 默认展示 30 天）
#[tauri::command]
pub fn workbuddy_checkin_results(state: State<AppState>, days: Option<i64>) -> Result<Vec<WbCheckinRecord>, String> {
    let days = days.unwrap_or(30).clamp(1, 90);
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let raw: serde_json::Value = fs_utils::read_json(&checkin_results_path(&state));
    let mut out = Vec::new();
    if let Some(arr) = raw.get("results").and_then(|v| v.as_array()) {
        for r in arr {
            let date = r.get("date").and_then(|v| v.as_str()).unwrap_or("");
            if date < cutoff.as_str() {
                continue;
            }
            out.push(WbCheckinRecord {
                date: date.into(),
                time: r.get("time").and_then(|v| v.as_str()).unwrap_or("").into(),
                user_id: r.get("user_id").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: r.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                status: r.get("status").and_then(|v| v.as_str()).unwrap_or("").into(),
                message: r.get("message").and_then(|v| v.as_str()).unwrap_or("").into(),
                reward: r.get("reward").and_then(|v| v.as_f64()),
            });
        }
    }
    out.reverse(); // 新→旧
    Ok(out)
}

/// 每日签到定时任务（F-16 双时段：每个时间一个任务，后缀 _HHMM）
const WB_CHECKIN_TASK_PREFIX: &str = "AIWorkAssistant_WorkBuddyCheckin";
const WB_RENEW_TASK_NAME: &str = "AIWorkAssistant_WorkBuddyRenew";

fn build_wb_task_tr(state: &AppState, script_args: &[&str]) -> String {
    let py = state.python_exe.replace('\\', "/");
    let script = state.python_dir.join("workbuddy_checkin.py").to_string_lossy().replace('\\', "/");
    let data_dir = state.data_dir.to_string_lossy().to_string();
    let args = script_args.join(" ");
    format!("cmd /c set \"AIWORKDATA_DIR={}\" && \"{}\" \"{}\" {}", data_dir, py, script, args)
}

fn run_schtasks(args: &[&str]) -> Result<(bool, String, String), String> {
    crate::commands::misc::run_schtasks(args)
}

fn task_exists(name: &str) -> bool {
    run_schtasks(&["/Query", "/TN", name, "/FO", "LIST"]).map(|(ok, _, _)| ok).unwrap_or(false)
}

/// 枚举当前用户可见的计划任务名（审查 P2）：`schtasks /Query /FO CSV /NH`，
/// CSV 列序固定为 HostName, TaskName, ...（取第 2 列），结构不受系统语言影响；
/// 带引号字段与根目录前缀 `\` 需剥离（本仓任务名不含逗号，按逗号切分安全）。
fn list_task_names() -> Vec<String> {
    let Ok((_, stdout, _)) = run_schtasks(&["/Query", "/FO", "CSV", "/NH"]) else {
        return vec![];
    };
    stdout
        .lines()
        .filter_map(|line| line.split(',').nth(1))
        .map(|f| f.trim().trim_matches('"').trim_start_matches('\\').to_string())
        .filter(|n| !n.is_empty())
        .collect()
}

/// 按任务名前缀枚举本功能全部签到任务（兼容任意 _HHMM 后缀，不再硬编码 _0900/_2100/_1200）
fn wb_checkin_task_names() -> Vec<String> {
    let prefix = format!("{WB_CHECKIN_TASK_PREFIX}_");
    list_task_names()
        .into_iter()
        .filter(|n| n.starts_with(&prefix))
        .collect()
}

#[tauri::command(async)]
pub fn workbuddy_checkin_task_register(state: State<AppState>, times: Vec<String>) -> Result<(), String> {
    if times.is_empty() {
        return Err("至少需要一个触发时间（如 09:00 / 21:00）".into());
    }
    let tr = build_wb_task_tr(&state, &["--json-stream", "--skip-checked"]);
    // 先清理旧实例（按任务名前缀枚举，兼容历史任意 HHMM 后缀），保证重注册幂等
    for name in wb_checkin_task_names() {
        let _ = run_schtasks(&["/Delete", "/TN", &name, "/F"]);
    }
    for t in &times {
        let hhmm = t.replace(':', "");
        let name = format!("{WB_CHECKIN_TASK_PREFIX}_{hhmm}");
        let (ok, _, stderr) = run_schtasks(&[
            "/Create", "/TN", &name, "/TR", &tr, "/SC", "DAILY", "/ST", t, "/F",
        ])?;
        if !ok {
            return Err(format!("注册任务 {t} 失败: {}", stderr.trim()));
        }
    }
    Ok(())
}

#[tauri::command(async)]
pub fn workbuddy_checkin_task_status() -> Result<Vec<String>, String> {
    let prefix = format!("{WB_CHECKIN_TASK_PREFIX}_");
    Ok(wb_checkin_task_names()
        .iter()
        .map(|name| name.trim_start_matches(&prefix).replace('_', ":"))
        .collect())
}

#[tauri::command(async)]
pub fn workbuddy_checkin_task_unregister() -> Result<(), String> {
    for name in wb_checkin_task_names() {
        let _ = run_schtasks(&["/Delete", "/TN", &name, "/F"]);
    }
    Ok(())
}

/// token 每周兜底续期任务（F-09；python --renew-only 惰性刷新）
#[tauri::command(async)]
pub fn workbuddy_renew_task_register(state: State<AppState>, day: String) -> Result<(), String> {
    // day: MON..SUN（schtasks /SC WEEKLY /D）；默认 SUN
    let d = if day.is_empty() { "SUN".to_string() } else { day.to_uppercase() };
    let tr = build_wb_task_tr(&state, &["--renew-only"]);
    let (ok, _, stderr) = run_schtasks(&[
        "/Create", "/TN", WB_RENEW_TASK_NAME, "/TR", &tr, "/SC", "WEEKLY", "/D", &d, "/ST", "10:30", "/F",
    ])?;
    if !ok {
        return Err(format!("注册续期任务失败: {}", stderr.trim()));
    }
    Ok(())
}

#[tauri::command]
pub fn workbuddy_renew_task_status() -> bool {
    task_exists(WB_RENEW_TASK_NAME)
}

#[tauri::command(async)]
pub fn workbuddy_renew_task_unregister() -> Result<(), String> {
    let _ = run_schtasks(&["/Delete", "/TN", WB_RENEW_TASK_NAME, "/F"]);
    Ok(())
}

// ── UI 坐标点击签到兜底（T4.2/F-18）────────────────────────────────────────
// 无 API 可用时的最后手段：仅手动触发、默认关闭（ui_click_enabled）；
// 坐标由用户「取点」预配置；单次执行只单击一次，不循环连点；零 token 输出。

#[tauri::command]
pub fn workbuddy_ui_click_capture(state: State<AppState>) -> Result<serde_json::Value, String> {
    run_ui_click_script(&state, vec!["--capture".to_string()])
}

#[tauri::command]
pub fn workbuddy_ui_click_checkin(state: State<AppState>) -> Result<serde_json::Value, String> {
    let s = load_settings(&state);
    if !s.ui_click_enabled {
        return Err("UI 坐标点击兜底未启用：请在设置中显式开启（F-18 仅作 API 不可用时的最后手段）".to_string());
    }
    if s.ui_click_x <= 0 || s.ui_click_y <= 0 {
        return Err("签到按钮坐标未配置：请先在客户端打开签到页，再用「取点」记录按钮位置".to_string());
    }
    run_ui_click_script(
        &state,
        vec![
            "--click".to_string(),
            "--x".to_string(),
            s.ui_click_x.to_string(),
            "--y".to_string(),
            s.ui_click_y.to_string(),
        ],
    )
}

fn run_ui_click_script(state: &AppState, args: Vec<String>) -> Result<serde_json::Value, String> {
    let script_path = state.python_dir.join("workbuddy_ui_click.py");
    if !script_path.exists() {
        return Err(format!("找不到脚本: {}", script_path.display()));
    }
    let out = Command::new(&state.python_exe)
        .arg(&script_path)
        .args(&args)
        .creation_flags(0x08000000)
        .env("PYTHONIOENCODING", "utf-8")
        .output()
        .map_err(|e| format!("UI 点击执行失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .ok_or_else(|| format!("UI 点击脚本输出无法解析: {}", stdout.trim().chars().take(120).collect::<String>()))
}

// ── 启动自动补签（F-55）────────────────────────────────────────────────────

/// 启动自动补签核心（F-55，main.rs 启动线程调用）：
/// 复用签到脚本 --json-stream --skip-checked（未签自动补签），静默执行零打扰。
pub fn startup_auto_checkin(app: &AppHandle, state: &AppState) {
    let s = load_settings(state);
    if !s.auto_checkin {
        return;
    }
    let app2 = app.clone();
    let data_dir = state.data_dir.clone();
    let python_dir = state.python_dir.clone();
    let python_exe = state.python_exe.clone();
    std::thread::spawn(move || {
        // 与 Trae 静默签到同款延迟 60s，避开启动高峰
        std::thread::sleep(std::time::Duration::from_secs(60));
        let script = python_dir.join("workbuddy_checkin.py");
        if !script.exists() {
            fs_utils::app_log(&data_dir, "WorkBuddy 启动补签：脚本不存在，跳过");
            return;
        }
        fs_utils::app_log(&data_dir, "WorkBuddy 启动补签：开始核验签到状态");
        match Command::new(&python_exe)
            .arg(&script)
            .args(["--json-stream", "--skip-checked"])
            .creation_flags(0x08000000)
            .env("AIWORKDATA_DIR", &data_dir)
            .env("PYTHONIOENCODING", "utf-8")
            .output()
        {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let done = stdout
                    .lines()
                    .rev()
                    .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                    .filter(|v| v.get("type") == Some(&serde_json::json!("done")));
                match done {
                    Some(d) => {
                        let msg = format!(
                            "WorkBuddy 启动补签完成: 成功 {}，已签 {}，失败 {}",
                            d.get("ok").and_then(|v| v.as_i64()).unwrap_or(0),
                            d.get("already").and_then(|v| v.as_i64()).unwrap_or(0),
                            d.get("failed").and_then(|v| v.as_i64()).unwrap_or(0),
                        );
                        fs_utils::app_log(&data_dir, &msg);
                        let failed = d.get("failed").and_then(|v| v.as_i64()).unwrap_or(0);
                        if failed > 0 {
                            push_notify(
                                Some(&app2),
                                &data_dir,
                                "WorkBuddy 签到提醒",
                                &format!("启动补签有 {failed} 个账号失败，请在签到与成长页查看"),
                            );
                        }
                    }
                    None => fs_utils::app_log(&data_dir, "WorkBuddy 启动补签：无有效结果输出"),
                }
            }
                    Err(e) => fs_utils::app_log(&data_dir, &format!("WorkBuddy 启动补签失败: {e}")),
        }
    });
}
