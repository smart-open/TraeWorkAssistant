//! WorkBuddy 签到域（原 workbuddy.rs 机械拆分）：M4 签到/成长（F-15/F-17，NDJSON 管线）、
//! 签到结果（F-15）、每日定时任务（F-16/F-55）、UI 坐标点击兜底（T4.2/F-18）、启动自动补签（F-55）。
//! 签到/成长执行直调 tasks::wb_checkin（Python 移除后无子进程管线），事件契约不变。

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;
use crate::tasks::wb_checkin::{self, CheckinOpts, GrowthOpts};

use super::common::{load_settings, push_notify};

// ── M4 签到（F-15，NDJSON 管线）─────────────────────────────────────────────

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

/// 签到/成长全局轮次锁（审查 P1）：签到/成长管线共享结果落盘，
/// 并发轮次会互相踩踏——tokio Mutex try_lock 拿不到即拒绝，不排队不阻塞。
static WB_ROUND_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 入口尝试获取轮次锁；guard 移交工作线程并持有至轮次结束
///（RAII：正常结束与 panic 展开均可靠释放，防泄漏）。
/// pub(crate)：应用内调度器（tasks/scheduler.rs）到点跑签到轮次时同样抢锁互斥。
pub(crate) fn try_acquire_wb_round() -> Result<tokio::sync::MutexGuard<'static, ()>, String> {
    WB_ROUND_LOCK
        .try_lock()
        .map_err(|_| "已有签到/成长任务在执行中，请等待当前轮次完成".to_string())
}

/// WB 自动签到开关（应用内调度器 wb-checkin 任务的启用判定，与启动补签同源设置）
pub(crate) fn wb_auto_checkin_enabled(state: &AppState) -> bool {
    load_settings(state).auto_checkin
}

/// NDJSON 事件转发（原 python 管线同款：emit 序列化 JSON 字符串，前端逐行 JSON.parse）
fn emit_wb_event(app: &AppHandle, ev: &Value) {
    if let Ok(line) = serde_json::to_string(ev) {
        let _ = app.emit("wb-checkin-progress", &line);
    }
}

/// 直调轮次共通封装：轮次锁 guard 移交工作线程，执行完发 exit 事件
///（原 python 子进程管线退出事件同款，前端据 "type":"exit" 复位运行态）。
fn spawn_wb_round<F>(
    app: AppHandle,
    state: AppState,
    round: tokio::sync::MutexGuard<'static, ()>,
    f: F,
) -> Result<(), String>
where
    F: FnOnce(&AppHandle, &AppState) + Send + 'static,
{
    std::thread::spawn(move || {
        let _guard = round;
        f(&app, &state);
        let _ = app.emit("wb-checkin-progress", "{\"type\":\"exit\",\"ok\":true}");
    });
    Ok(())
}

/// 启动 WorkBuddy 签到（tasks::wb_checkin 直调），
/// NDJSON → `wb-checkin-progress` 事件（独立管线，避免与 Trae checkin 状态串扰）。
#[tauri::command(async)]
pub fn workbuddy_checkin_start(app: AppHandle, state: State<AppState>, opts: WbCheckinOpts) -> Result<(), String> {
    let round = try_acquire_wb_round()?;
    let o = CheckinOpts {
        uids: opts.user_ids.unwrap_or_default(),
        skip_checked: opts.skip_checked_in,
        skip_expired: opts.skip_expired,
        lazy_hours: opts.lazy_hours.unwrap_or(24),
    };
    spawn_wb_round(app, state.inner().clone(), round, move |app, st| {
        wb_checkin::run_checkin_round(st, &o, &mut |ev| emit_wb_event(app, ev));
    })
}

/// 成长中心执行（F-17；旅行/盲盒/任务开关随设置）
#[tauri::command(async)]
pub fn workbuddy_growth_run(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let round = try_acquire_wb_round()?;
    let s = load_settings(&state);
    let flags = GrowthOpts {
        travel: s.growth_travel,
        lottery: s.growth_lottery,
        tasks: s.growth_tasks,
    };
    spawn_wb_round(app, state.inner().clone(), round, move |app, st| {
        wb_checkin::run_growth_round(st, &flags, &[], &mut |ev| emit_wb_event(app, ev));
    })
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
    let raw: serde_json::Value =
        crate::store::docs::wb_checkin_results_load(&crate::store::db(&state.data_dir));
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

/// 构造 WorkBuddy 计划任务 /TR：主 exe 直调 CLI 任务模式（--task-run，见 tasks::run_cli_task），
/// 不再依赖 python 运行时；schtasks 不继承进程环境变量，cmd /c 内显式 set AIWORKDATA_DIR。
fn build_wb_task_tr(state: &AppState, task: &str) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取主程序路径失败: {e}"))?;
    let data_dir = state.data_dir.to_string_lossy().to_string();
    Ok(format!(
        "cmd /c set \"AIWORKDATA_DIR={}\" && \"{}\" --task-run {}",
        data_dir,
        exe.to_string_lossy(),
        task
    ))
}

fn run_schtasks(args: &[&str]) -> Result<(bool, String, String), String> {
    crate::commands::misc::run_schtasks(args)
}

#[cfg(windows)] // 仅 workbuddy_renew_task_status 的 Windows 分支消费（mac 恒 false）
fn task_exists(name: &str) -> bool {
    run_schtasks(&["/Query", "/TN", name, "/FO", "LIST"]).map(|(ok, _, _)| ok).unwrap_or(false)
}

/// 枚举当前用户可见的计划任务名（审查 P2）：`schtasks /Query /FO CSV /NH`。
/// 注意：CSV 列序**不跨机固定**——部分系统为 HostName,TaskName,…（TaskName 第 2 列），
/// 部分系统仅 TaskName,NextRunTime,Status（TaskName 第 1 列，实测 2026-09-24 Win11）。
/// 不按固定下标取列，改为扫描各行字段：TaskName 字段带引号且以 `\` 开头
/// （根目录任务形如 `\AIWorkAssistant_WorkBuddyCheckin_0900`），取末段 `\` 之后为任务名；
/// 本仓任务名不含逗号，按逗号切分安全。
fn list_task_names() -> Vec<String> {
    let Ok((_, stdout, _)) = run_schtasks(&["/Query", "/FO", "CSV", "/NH"]) else {
        return vec![];
    };
    parse_task_names_from_csv(&stdout)
}

/// 从 schtasks CSV 输出提取任务名（纯函数，便于单测覆盖两种列序）。
fn parse_task_names_from_csv(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        for field in line.split(',') {
            let f = field.trim().trim_matches('"');
            let Some(name) = f.strip_prefix('\\') else { continue };
            let name = name.rsplit('\\').next().unwrap_or(name);
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
    }
    out
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
    crate::commands::misc::schtasks_gate()?;
    if times.is_empty() {
        return Err("至少需要一个触发时间（如 09:00 / 21:00）".into());
    }
    // 审查修复（命令注入）：times 逐项严格校验后再进 schtasks（与 misc::task_register 同 sink）
    for t in &times {
        crate::commands::misc::validate_hhmm(t)?;
    }
    let tr = build_wb_task_tr(&state, "wb-checkin")?;
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
    fs_utils::app_log(
        &state.data_dir,
        &format!("WorkBuddy 每日签到定时任务已注册: {}", times.join(" / ")),
    );
    Ok(())
}

#[tauri::command(async)]
pub fn workbuddy_checkin_task_status() -> Result<Vec<String>, String> {
    crate::commands::misc::schtasks_gate()?;
    let prefix = format!("{WB_CHECKIN_TASK_PREFIX}_");
    Ok(wb_checkin_task_names()
        .iter()
        .map(|name| name.trim_start_matches(&prefix).replace('_', ":"))
        .collect())
}

#[tauri::command(async)]
pub fn workbuddy_checkin_task_unregister(state: State<AppState>) -> Result<(), String> {
    crate::commands::misc::schtasks_gate()?;
    for name in wb_checkin_task_names() {
        let _ = run_schtasks(&["/Delete", "/TN", &name, "/F"]);
    }
    fs_utils::app_log(&state.data_dir, "WorkBuddy 每日签到定时任务已注销");
    Ok(())
}

/// token 每周兜底续期任务（F-09；python --renew-only 惰性刷新）
#[tauri::command(async)]
pub fn workbuddy_renew_task_register(state: State<AppState>, day: String, time: String) -> Result<(), String> {
    crate::commands::misc::schtasks_gate()?;
    // day: MON..SUN（schtasks /SC WEEKLY /D）；默认 SUN。
    // 审查修复（命令注入）：白名单校验（此前仅大写化，"mon&calc" → "MON&CALC" 仍可注入）
    let d = if day.is_empty() { "SUN".to_string() } else { day.to_uppercase() };
    if !["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"].contains(&d.as_str()) {
        return Err(format!("星期无效: {day}（应为 MON..SUN）"));
    }
    // 触发时刻 HH:MM（默认 10:30，任务配置页可改）
    let t = time.trim();
    let t = if t.is_empty() { "10:30" } else { t };
    crate::commands::misc::validate_hhmm(t)?;
    let tr = build_wb_task_tr(&state, "wb-renew")?;
    let (ok, _, stderr) = run_schtasks(&[
        "/Create", "/TN", WB_RENEW_TASK_NAME, "/TR", &tr, "/SC", "WEEKLY", "/D", &d, "/ST", t, "/F",
    ])?;
    if !ok {
        return Err(format!("注册续期任务失败: {}", stderr.trim()));
    }
    fs_utils::app_log(
        &state.data_dir,
        &format!("WorkBuddy 每周续期定时任务已注册: 每周{d} {t}"),
    );
    Ok(())
}

#[tauri::command]
pub fn workbuddy_renew_task_status() -> bool {
    // mac 无 schtasks；前端注册卡片隐藏，bool 返回值兼容旧契约。
    // 双 cfg 单体形态：mac 构建仅保留 return（无后续代码 → 无 unreachable 警告）
    #[cfg(not(windows))]
    return false;
    #[cfg(windows)]
    task_exists(WB_RENEW_TASK_NAME)
}

#[tauri::command(async)]
pub fn workbuddy_renew_task_unregister(state: State<AppState>) -> Result<(), String> {
    crate::commands::misc::schtasks_gate()?;
    let _ = run_schtasks(&["/Delete", "/TN", WB_RENEW_TASK_NAME, "/F"]);
    fs_utils::app_log(&state.data_dir, "WorkBuddy 每周续期定时任务已注销");
    Ok(())
}

// ── UI 坐标点击签到兜底（T4.2/F-18）────────────────────────────────────────
// 无 API 可用时的最后手段：仅手动触发、默认关闭（ui_click_enabled）；
// 坐标由用户「取点」预配置；单次执行只单击一次，不循环连点；零 token 输出。

/// 取点（F-18）：3 秒倒计时后记录当前鼠标坐标（Rust 直调 tasks::ui_click，输出契约对齐原 python）
#[tauri::command]
pub fn workbuddy_ui_click_capture(state: State<AppState>) -> Result<serde_json::Value, String> {
    let _ = &state; // 预留：设置回写等扩展
    let (x, y) = crate::tasks::ui_click::capture_point()?;
    Ok(serde_json::json!({
        "ok": true, "x": x, "y": y,
        "message": format!("已记录坐标 ({x}, {y})"),
    }))
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
    // settings 坐标字段为 i64，屏幕坐标恒在 i32 范围；防御性转换杜绝截断回绕
    let Ok(x) = i32::try_from(s.ui_click_x) else {
        return Err("坐标无效（超出屏幕坐标范围）".to_string());
    };
    let Ok(y) = i32::try_from(s.ui_click_y) else {
        return Err("坐标无效（超出屏幕坐标范围）".to_string());
    };
    crate::tasks::ui_click::click_at(x, y)?;
    Ok(serde_json::json!({
        "ok": true, "x": s.ui_click_x, "y": s.ui_click_y,
        "message": format!("已点击 ({}, {})，请查看客户端签到结果", s.ui_click_x, s.ui_click_y),
    }))
}

// ── 启动自动补签（F-55）────────────────────────────────────────────────────

/// 启动自动补签核心（F-55，main.rs 启动线程调用）：
/// 复用每日签到参数（--json-stream --skip-checked 同款：skip_checked + lazy 24h），
/// 未签自动补签，静默执行零打扰。
pub fn startup_auto_checkin(app: &AppHandle, state: &AppState) {
    let s = load_settings(state);
    if !s.auto_checkin {
        return;
    }
    let app2 = app.clone();
    let state2 = state.clone();
    std::thread::spawn(move || {
        // 与 Trae 静默签到同款延迟 60s，避开启动高峰
        std::thread::sleep(std::time::Duration::from_secs(60));
        // 抢轮次锁（审查修复 #15）：与 UI 签到/成长轮次互斥，共享结果落盘并发会踩踏；
        // 抢不到则本轮静默跳过（补签幂等，下轮启动/次日定时任务会再核验）
        let Ok(_round) = try_acquire_wb_round() else {
            fs_utils::app_log(&state2.data_dir, "WorkBuddy 启动补签跳过：已有签到/成长任务在执行中");
            return;
        };
        fs_utils::app_log(&state2.data_dir, "WorkBuddy 启动补签：开始核验签到状态");
        let done = wb_checkin::run_checkin_round(&state2, &CheckinOpts::daily(), &mut |_| {});
        let msg = format!(
            "WorkBuddy 启动补签完成: 成功 {}，已签 {}，失败 {}",
            done["ok"].as_i64().unwrap_or(0),
            done["already"].as_i64().unwrap_or(0),
            done["failed"].as_i64().unwrap_or(0),
        );
        fs_utils::app_log(&state2.data_dir, &msg);
        let failed = done["failed"].as_i64().unwrap_or(0);
        if failed > 0 {
            push_notify(
                Some(&app2),
                &state2.data_dir,
                "WorkBuddy 签到提醒",
                &format!("启动补签有 {failed} 个账号失败，请在签到与成长页查看"),
                crate::notify::NotifyEvent::TaskFail,
            );
        }
    });
}

// ── 托盘一键签到（Trae 之外的两个阶段由 main.rs 托盘线程调用）──────────────

/// 托盘一键签到 WorkBuddy 部分：签到 → 成长计划 同步串行执行（共享结果文件落盘，
/// 串行避免并发踩踏），各阶段完成发系统通知。
/// Trae 签到由 main.rs 复用 start_checkin_core 并行触发，互不阻塞。
pub fn tray_checkin_all(app: &AppHandle, state: &AppState) {
    // 抢轮次锁（审查修复 #15）：与 UI 签到/成长轮次互斥（共享结果落盘并发会踩踏）；
    // 抢不到则通知并跳过，不排队阻塞托盘线程（签到幂等，稍后可再点）
    let Ok(_round) = try_acquire_wb_round() else {
        let msg = "已有签到/成长任务在执行中，一键签到已跳过";
        fs_utils::app_log(&state.data_dir, msg);
        push_notify(Some(app), &state.data_dir, "一键签到", msg, crate::notify::NotifyEvent::Other);
        return;
    };
    let s = load_settings(state);
    // 阶段 1：签到（每日任务同款参数：skip_checked + lazy 24h）
    let done = wb_checkin::run_checkin_round(state, &CheckinOpts::daily(), &mut |_| {});
    let summary = match (
        done["ok"].as_i64(),
        done["already"].as_i64(),
        done["failed"].as_i64(),
    ) {
        (Some(o), Some(a), Some(f)) => format!("成功 {o}，已签 {a}，失败 {f}"),
        _ => "完成".to_string(),
    };
    let msg = format!("WorkBuddy 签到: {summary}");
    fs_utils::app_log(&state.data_dir, &msg);
    push_notify(Some(app), &state.data_dir, "一键签到", &msg, crate::notify::NotifyEvent::CheckinDone);
    // 阶段 2：成长计划（旅行/盲盒/任务开关随设置）
    let flags = GrowthOpts {
        travel: s.growth_travel,
        lottery: s.growth_lottery,
        tasks: s.growth_tasks,
    };
    wb_checkin::run_growth_round(state, &flags, &[], &mut |_| {});
    let msg = "WorkBuddy 成长计划: 完成";
    fs_utils::app_log(&state.data_dir, msg);
    push_notify(Some(app), &state.data_dir, "一键签到", msg, crate::notify::NotifyEvent::CheckinDone);
}

#[cfg(test)]
mod tests {
    use super::parse_task_names_from_csv;

    #[test]
    fn csv_解析_三列无主机名_本机实测列序() {
        // 2026-09-24 Win11 实测：无 HostName 列，TaskName 在第 1 列
        let out = parse_task_names_from_csv(
            "\"\\360ZipUpdater\",\"N/A\",\"Ready\"\n\"\\AIWorkAssistant_WorkBuddyCheckin_0900\",\"2026/9/24 9:00:00\",\"Ready\"\n",
        );
        assert_eq!(
            out,
            vec!["360ZipUpdater", "AIWorkAssistant_WorkBuddyCheckin_0900"]
        );
    }

    #[test]
    fn csv_解析_四列含主机名_旧列序() {
        // 部分系统为 HostName,TaskName,...（TaskName 第 2 列）
        let out = parse_task_names_from_csv(
            "\"DESKTOP-ABC\",\"\\AIWorkAssistant_WorkBuddyCheckin_0900\",\"2026/9/24 9:00:00\",\"就绪\"\n",
        );
        assert_eq!(out, vec!["AIWorkAssistant_WorkBuddyCheckin_0900"]);
    }

    #[test]
    fn csv_解析_文件夹任务取末段与空行安全() {
        let out = parse_task_names_from_csv(
            "\"\\Microsoft\\Windows\\TaskScheduler\\Maintenance Configurator\",\"N/A\",\"Ready\"\n\n",
        );
        assert_eq!(out, vec!["Maintenance Configurator"]);
    }
}
