//! WorkBuddy 签到域（原 workbuddy.rs 机械拆分）：M4 签到/成长（F-15/F-17，NDJSON 管线）、
//! 签到结果（F-15）、启动自动补签开关（F-55）。
//! 签到/成长执行直调 tasks::wb_checkin（Python 移除后无子进程管线），事件契约不变。
//! Web 化改造：桌面事件推送改回调（Emitter 回调 → server 桥到 SSE）；
//! 每日定时任务（schtasks 链）与 UI 坐标点击兜底（F-18）退役删除；
//! 启动补签/托盘一键签到（依赖 AppHandle/桌面通知）随桌面壳一并裁剪。

use serde::Serialize;
use serde_json::Value;

use crate::state::AppState;
use crate::tasks::wb_checkin::{self, CheckinOpts, GrowthOpts};

use super::common::load_settings;

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

/// WB 签到/进度事件回调（Web 化替代 AppHandle::emit）：(事件名, 载荷)。
/// 事件名固定 "wb-checkin-progress"，载荷与桌面 emit 契约一致（序列化 JSON 字符串，
/// 前端逐行 JSON.parse；exit 结束事件同款）。
pub type WbCheckinEmitter = std::sync::Arc<dyn Fn(&str, Value) + Send + Sync>;

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
fn emit_wb_event(emit: &WbCheckinEmitter, ev: &Value) {
    if let Ok(line) = serde_json::to_string(ev) {
        let _ = emit("wb-checkin-progress", Value::String(line));
    }
}

/// 直调轮次共通封装：轮次锁 guard 移交工作线程，执行完发 exit 事件
///（原 python 子进程管线退出事件同款，前端据 "type":"exit" 复位运行态）。
fn spawn_wb_round<F>(
    state: AppState,
    round: tokio::sync::MutexGuard<'static, ()>,
    emit: WbCheckinEmitter,
    f: F,
) -> Result<(), String>
where
    F: FnOnce(&AppState) + Send + 'static,
{
    std::thread::spawn(move || {
        let _guard = round;
        f(&state);
        let _ = emit(
            "wb-checkin-progress",
            Value::String("{\"type\":\"exit\",\"ok\":true}".to_string()),
        );
    });
    Ok(())
}

/// 启动 WorkBuddy 签到（tasks::wb_checkin 直调），
/// NDJSON → `wb-checkin-progress` 回调（独立管线，避免与 Trae checkin 状态串扰）。
pub fn workbuddy_checkin_start(
    state: &AppState,
    opts: WbCheckinOpts,
    emit: WbCheckinEmitter,
) -> Result<(), String> {
    let round = try_acquire_wb_round()?;
    let o = CheckinOpts {
        uids: opts.user_ids.unwrap_or_default(),
        skip_checked: opts.skip_checked_in,
        skip_expired: opts.skip_expired,
        lazy_hours: opts.lazy_hours.unwrap_or(24),
    };
    spawn_wb_round(state.clone(), round, emit.clone(), move |st| {
        wb_checkin::run_checkin_round(st, &o, &mut |ev| emit_wb_event(&emit, ev));
    })
}

/// 成长中心执行（F-17；旅行/盲盒/任务开关随设置）
pub fn workbuddy_growth_run(state: &AppState, emit: WbCheckinEmitter) -> Result<(), String> {
    let round = try_acquire_wb_round()?;
    let s = load_settings(state);
    let flags = GrowthOpts {
        travel: s.growth_travel,
        lottery: s.growth_lottery,
        tasks: s.growth_tasks,
    };
    spawn_wb_round(state.clone(), round, emit.clone(), move |st| {
        wb_checkin::run_growth_round(st, &flags, &[], &mut |ev| emit_wb_event(&emit, ev));
    })
}

// ── 签到结果（F-15）────────────────────────────────────────────────────────

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
pub fn workbuddy_checkin_results(state: &AppState, days: Option<i64>) -> Result<Vec<WbCheckinRecord>, String> {
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
