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

/// 持有者标签登记（issue #134 方案2）：(标签, 起始时刻, 超时已告警)。
/// std Mutex 短临界区（登记/清除/读取），panic 中毒时 into_inner 继续读写
///（登记丢失只降级报错详情，不值得连锁 panic）
static WB_ROUND_HOLDER: std::sync::Mutex<Option<(String, std::time::Instant, bool)>> =
    std::sync::Mutex::new(None);

fn wb_holder_lock() -> std::sync::MutexGuard<'static, Option<(String, std::time::Instant, bool)>> {
    WB_ROUND_HOLDER.lock().unwrap_or_else(|p| p.into_inner())
}

/// 进程内轮次锁守卫：tokio 锁 + 持有者登记 RAII（drop 清登记）。
/// tokio MutexGuard 本身 Send——UI 路径在本结构内移交工作线程持有至轮次结束
struct WbRoundLocalGuard {
    _round: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for WbRoundLocalGuard {
    fn drop(&mut self) {
        *wb_holder_lock() = None;
    }
}

/// 完整轮次锁守卫（issue #134 方案2+4）：进程内 tokio 锁 + Windows 命名互斥体跨进程锁。
/// ⚠ 仅限获取线程内持有至轮次结束：Windows mutex 所有权线程亲和，跨线程移交会令
/// ReleaseMutex 报 ERROR_NOT_OWNER 且所有权挂死在长存线程（如 tauri async worker）上，
/// 互斥体永久锁死——完整版各调用点（调度器/启动补签/托盘/CLI）均为同线程执行全程。
/// UI 路径例外：command 线程仅取 local 守卫 + 预检跨进程锁，正式持有在工作线程内完成。
pub(crate) struct WbRoundGuard {
    _local: WbRoundLocalGuard,
    _cross: Option<crate::tasks::qoder_common::CrossProcLock>,
}

/// 撞锁详情（issue #134 方案2）：报错携带持有者标签与已运行分钟数，
/// 让「谁在跑、跑了多久」一眼可辨；持有超 15 分钟疑似卡死，落 app_log 告警一次
///（warned 标记防同轮重复告警——告警由下一次撞锁触发，无后台时钟依赖）
fn wb_round_busy_detail(data_dir: &std::path::Path) -> String {
    let mut holder = wb_holder_lock();
    let Some((holder_tag, since, warned)) = holder.as_mut() else {
        return "已有签到/成长任务在执行中，请等待当前轮次完成".to_string();
    };
    let mins = since.elapsed().as_secs() / 60;
    let overlong = since.elapsed() >= std::time::Duration::from_secs(15 * 60);
    let need_warn = overlong && !*warned;
    if need_warn {
        *warned = true;
    }
    let holder_tag = holder_tag.clone();
    drop(holder);
    if need_warn {
        fs_utils::app_log(
            data_dir,
            &format!("[WB 轮次] 持有者「{holder_tag}」已运行超 15 分钟，疑似卡死，请排查"),
        );
    }
    // 撞锁多发生在持有初期（最常见场景）：不足 1 分钟显示「刚启动」，避免「已运行 0 分钟」
    let ran = if mins < 1 { "刚启动".to_string() } else { format!("已运行 {mins} 分钟") };
    format!("已有签到/成长任务在执行中（{holder_tag}，{ran}），请等待当前轮次完成")
}

/// 进程内 tokio 锁获取 + 持有者登记（UI local 路径共用）
fn try_tokio_round(tag: &str, data_dir: &std::path::Path) -> Result<tokio::sync::MutexGuard<'static, ()>, String> {
    let Ok(round) = WB_ROUND_LOCK.try_lock() else {
        return Err(wb_round_busy_detail(data_dir));
    };
    *wb_holder_lock() = Some((tag.to_string(), std::time::Instant::now(), false));
    Ok(round)
}

/// 跨进程轮次锁获取（issue #134 方案4，wait 0 非阻塞）：GUI 进程与 schtasks CLI
/// 进程（--task-run）双进程互斥。Err 文案可直接上抛 UI / 落日志 / 输出 skipped_busy。
/// CreateFailed/WaitFailed 一并 fail-closed（对齐 qoder cross_refresh_guard 口径）：
/// 锁机制不可用时带病降级会静默丢失 GUI vs CLI 落盘互斥保障。
/// Err = (文案, 是否真占用)：Busy（他方持有）幂等跳过不落日志——调度器每 tick
/// 撞锁时会与调用方各自的 skipped 日志重复刷屏；机制性失败（创建/等待系统错误）
/// 需落日志人工排查（对齐 CrossProcLockFail 文档分级）
fn wb_cross_round_guard(
    data_dir: &std::path::Path,
) -> Result<crate::tasks::qoder_common::CrossProcLock, (String, bool)> {
    use crate::tasks::qoder_common::CrossProcLockFail;
    let (g, fail) =
        crate::tasks::qoder_common::CrossProcLock::try_acquire_ns(data_dir, "wb", "round", 0);
    match g {
        Some(g) => Ok(g),
        None => {
            let fail = fail.unwrap_or(CrossProcLockFail::Busy);
            let busy = fail == CrossProcLockFail::Busy;
            Err((fail.describe(), busy))
        }
    }
}

/// 入口尝试获取完整轮次锁（进程内 + 跨进程）；guard 在获取线程内持有至轮次结束
///（RAII：正常结束与 panic 展开均可靠释放，防泄漏）。
/// pub(crate)：应用内调度器（tasks/scheduler.rs）、启动补签、托盘、CLI 任务
///（tasks/mod.rs）到点跑签到/成长轮次时同样抢锁互斥。
pub(crate) fn try_acquire_wb_round(tag: &str, data_dir: &std::path::Path) -> Result<WbRoundGuard, String> {
    let cross = wb_cross_round_guard(data_dir).map_err(|(why, busy)| {
        // 仅机制性失败（CreateFailed/WaitFailed）落日志供人工排查；Busy（他方持有）
        // 不落——调度器每 tick 撞锁时与调用方 skipped_busy 日志语义重复，纯刷屏
        if !busy {
            fs_utils::app_log(data_dir, &format!("WorkBuddy 轮次锁获取失败：{why}"));
        }
        format!("其他进程正在执行签到轮次（{why}），请稍后再试")
    })?;
    let Ok(round) = WB_ROUND_LOCK.try_lock() else {
        // 立即归还跨进程锁，避免占坑令他方进程误判忙
        drop(cross);
        return Err(wb_round_busy_detail(data_dir));
    };
    *wb_holder_lock() = Some((tag.to_string(), std::time::Instant::now(), false));
    Ok(WbRoundGuard { _local: WbRoundLocalGuard { _round: round }, _cross: Some(cross) })
}

/// 仅进程内锁版（UI command 线程专用，issue #134 方案4 配套）：不碰跨进程锁——
/// Windows mutex 所有权线程亲和，command 线程取得的跨进程锁不能随 tokio 守卫移交
/// 工作线程；跨进程锁由 spawn_wb_round 在工作线程内正式获取（command 线程另行预检）
fn try_acquire_wb_round_local(tag: &str, data_dir: &std::path::Path) -> Result<WbRoundLocalGuard, String> {
    Ok(WbRoundLocalGuard { _round: try_tokio_round(tag, data_dir)? })
}

/// WB 自动签到开关（应用内调度器 wb-checkin 任务的启用判定，与启动补签同源设置）
pub(crate) fn wb_auto_checkin_enabled(state: &AppState) -> bool {
    load_settings(state).auto_checkin
}

/// NDJSON 事件转发（原 python 管线同款：emit 序列化 JSON 字符串，前端逐行 JSON.parse）。
/// pub(crate)：issue #134 方案1——调度器后台轮次（wb-checkin/wb-growth）同步接入
/// 事件管线，前端运行态不再只由 UI 点击驱动
pub(crate) fn emit_wb_event(app: &AppHandle, ev: &Value) {
    if let Ok(line) = serde_json::to_string(ev) {
        let _ = app.emit("wb-checkin-progress", &line);
    }
}

/// 直调轮次共通封装：轮次锁 guard 移交工作线程，执行完发 exit 事件
///（原 python 子进程管线退出事件同款，前端据 "type":"exit" 复位运行态）。
/// 跨进程锁在本工作线程内正式获取（issue #134 方案4）：command 线程已预检，
/// 但 Windows mutex 所有权线程亲和、跨线程移交会挂死互斥体，故正式持有与
/// 释放均在本线程；获取失败（CLI 计划任务正在跑）→ 事件流反馈原因后空转收尾
fn spawn_wb_round<F>(
    app: AppHandle,
    state: AppState,
    round: WbRoundLocalGuard,
    f: F,
) -> Result<(), String>
where
    F: FnOnce(&AppHandle, &AppState) + Send + 'static,
{
    std::thread::spawn(move || {
        let _guard = round;
        // 跨进程锁正式持有（wait 0 + 短重试，issue #134 方案4）：调度器 tick 同刻
        // 抢到跨进程锁后立即因进程内锁（本线程持有）失败而释放（微秒级占坑），
        // wait=0 探测可能恰好撞上——重试两次吸收该竞态窗口；真占用（CLI 计划任务
        // 整轮数分钟）重试无果依旧放弃，失败原因进事件流
        let mut cross = wb_cross_round_guard(&state.data_dir);
        for _ in 0..2 {
            if cross.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
            cross = wb_cross_round_guard(&state.data_dir);
        }
        let _cross = match cross {
            Ok(g) => Some(g),
            Err((why, _busy)) => {
                fs_utils::app_log(&state.data_dir, &format!("WorkBuddy 签到轮次放弃：{why}"));
                // 进度列表反馈一行失败原因（前端 fail 行渲染逻辑同款，无新契约）
                emit_wb_event(
                    &app,
                    &serde_json::json!({
                        "index": 1, "user_id": "", "name": "", "status": "fail",
                        "message": format!("其他进程正在执行签到轮次（{why}），本轮放弃"),
                    }),
                );
                None
            }
        };
        if _cross.is_some() {
            f(&app, &state);
        }
        // 终态事件（前端据 "type":"exit" 复位运行态）：emit 失败落日志（issue #44 遗留项）。
        // 注意：wb-checkin-progress 前端契约是 NDJSON **字符串**（listen<string> 后
        // JSON.parse）——payload 必须序列化为 String，传对象会破坏 parseLine 导致
        // exit 事件被静默丢弃、前端运行态永挂
        let line = serde_json::json!({ "type": "exit", "ok": true }).to_string();
        crate::events::emit_logged(
            &app,
            "wb-checkin-progress",
            serde_json::Value::String(line),
            Some(state.data_dir.as_path()),
        );
    });
    Ok(())
}

/// 启动 WorkBuddy 签到（tasks::wb_checkin 直调），
/// NDJSON → `wb-checkin-progress` 事件（独立管线，避免与 Trae checkin 状态串扰）。
#[tauri::command(async)]
pub fn workbuddy_checkin_start(app: AppHandle, state: State<AppState>, opts: WbCheckinOpts) -> Result<(), String> {
    // 先取进程内锁：同进程调度器/托盘/启动补签持锁时报错携带持有者标签与已运行
    // 时长，避免把同进程占用误报为「其他进程」（跨进程预检放在其后——此时失败
    // 才是真他方进程 CLI 计划任务在跑，即时报错避免白等一整轮，issue #134 方案4）
    let round = try_acquire_wb_round_local("ui-checkin", &state.data_dir)?;
    if let Err((why, _busy)) = wb_cross_round_guard(&state.data_dir) {
        return Err(format!("其他进程正在执行签到轮次（{why}），请稍后再试"));
    }
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
    // 先取进程内锁再跨进程预检（同 workbuddy_checkin_start，issue #134 方案4）
    let round = try_acquire_wb_round_local("ui-growth", &state.data_dir)?;
    if let Err((why, _busy)) = wb_cross_round_guard(&state.data_dir) {
        return Err(format!("其他进程正在执行签到轮次（{why}），请稍后再试"));
    }
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
pub(crate) const WB_CHECKIN_TASK_PREFIX: &str = "AIWorkAssistant_WorkBuddyCheckin";
pub(crate) const WB_RENEW_TASK_NAME: &str = "AIWorkAssistant_WorkBuddyRenew";

/// 构造 WorkBuddy 计划任务 /TR：主 exe 直调 CLI 任务模式（--task-run，见 tasks::run_cli_task）。
/// issue #112：release 主 exe 为 GUI 子系统，直调不分配控制台 → 触发不弹黑框；
/// 旧 `cmd /c set AIWORKDATA_DIR=…` 包裹已删（该变量全仓只写不读，且 cmd.exe
/// 正是黑框根源）。与 misc::build_task_tr 同款形态。
fn build_wb_task_tr(task: &str) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取主程序路径失败: {e}"))?;
    Ok(format!("\"{}\" --task-run {task}", exe.to_string_lossy()))
}

fn run_schtasks(args: &[&str]) -> Result<(bool, String, String), String> {
    crate::commands::misc::run_schtasks(args)
}

#[cfg(windows)] // 仅 workbuddy_renew_task_status 的 Windows 分支消费（mac 恒 false）
fn task_exists(name: &str) -> bool {
    run_schtasks(&["/Query", "/TN", name, "/FO", "LIST"]).map(|(ok, _, _)| ok).unwrap_or(false)
}

/// 按任务名前缀枚举本功能全部签到任务（兼容任意 _HHMM 后缀，不再硬编码 _0900/_2100/_1200）
fn wb_checkin_task_names() -> Vec<String> {
    let prefix = format!("{WB_CHECKIN_TASK_PREFIX}_");
    crate::commands::misc::list_task_names()
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
    let tr = build_wb_task_tr("wb-checkin")?;
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
    // I15 同步修（蓝本同款缺陷）：register 侧存 4 位数字任务名（如 1015），
    // 需还原为 HH:MM；通用 '_'→':' 替换对其恒 no-op
    Ok(wb_checkin_task_names()
        .iter()
        .map(|name| match name.strip_prefix(&prefix) {
            Some(d) if d.len() == 4 && d.chars().all(|c| c.is_ascii_digit()) => {
                format!("{}:{}", &d[..2], &d[2..])
            }
            _ => name.trim_start_matches(&prefix).to_string(),
        })
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
    let tr = build_wb_task_tr("wb-renew")?;
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
        // 抢完整轮次锁（审查修复 #15 + issue #134 方案2+4）：与 UI 签到/成长轮次、
        // CLI 计划任务互斥；抢不到则本轮静默跳过（补签幂等，下轮启动/次日定时任务会再核验）
        let Ok(_round) = try_acquire_wb_round("启动补签", &state2.data_dir) else {
            let why = "WorkBuddy 启动补签跳过：已有签到/成长任务在执行中";
            fs_utils::app_log(&state2.data_dir, why);
            return;
        };
        fs_utils::app_log(&state2.data_dir, "WorkBuddy 启动补签：开始核验签到状态");
        // issue #134 方案1：后台轮次接入事件管线（此前空闭包，前端零显示撞锁不可辨）
        let done = wb_checkin::run_checkin_round(&state2, &CheckinOpts::daily(), &mut |ev| {
            emit_wb_event(&app2, ev);
        });
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
    // 抢完整轮次锁（审查修复 #15 + issue #134 方案2+4）：与 UI 签到/成长轮次、
    // CLI 计划任务互斥（共享结果落盘并发会踩踏）；抢不到则通知并跳过，不排队阻塞
    // 托盘线程（签到幂等，稍后可再点）。报错文案带持有者标签与已运行时长
    let Ok(_round) = try_acquire_wb_round("托盘一键签到", &state.data_dir) else {
        let msg = "已有签到/成长任务在执行中，一键签到已跳过";
        fs_utils::app_log(&state.data_dir, msg);
        push_notify(Some(app), &state.data_dir, "一键签到", msg, crate::notify::NotifyEvent::Other);
        return;
    };
    let s = load_settings(state);
    // 阶段 1：签到（每日任务同款参数：skip_checked + lazy 24h）
    // issue #134 方案1：托盘后台轮次接入事件管线（同启动补签）
    let done = wb_checkin::run_checkin_round(state, &CheckinOpts::daily(), &mut |ev| {
        emit_wb_event(app, ev);
    });
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
    // 阶段 2：成长计划（旅行/盲盒/任务开关随设置）——同接入事件管线（issue #134 方案1）
    let flags = GrowthOpts {
        travel: s.growth_travel,
        lottery: s.growth_lottery,
        tasks: s.growth_tasks,
    };
    wb_checkin::run_growth_round(state, &flags, &[], &mut |ev| {
        emit_wb_event(app, ev);
    });
    let msg = "WorkBuddy 成长计划: 完成";
    fs_utils::app_log(&state.data_dir, msg);
    push_notify(Some(app), &state.data_dir, "一键签到", msg, crate::notify::NotifyEvent::CheckinDone);
}

#[cfg(test)]
mod tests {
    use crate::commands::misc::parse_task_names_from_csv;

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
