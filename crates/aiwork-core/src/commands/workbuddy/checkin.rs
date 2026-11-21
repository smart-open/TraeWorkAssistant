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

/// 持有者标签登记（issue #134 方案2，移植 main）：(标签, 起始时刻, 超时已告警)。
/// std Mutex 短临界区（登记/清除/读取），panic 中毒时 into_inner 继续读写
///（登记丢失只降级报错详情，不值得连锁 panic）。
/// 桌面蓝本另有 Windows 命名互斥体跨进程互斥（方案4）——OS 专属，docker 单进程不移植
static WB_ROUND_HOLDER: std::sync::Mutex<Option<(String, std::time::Instant, bool)>> =
    std::sync::Mutex::new(None);

fn wb_holder_lock() -> std::sync::MutexGuard<'static, Option<(String, std::time::Instant, bool)>> {
    WB_ROUND_HOLDER.lock().unwrap_or_else(|p| p.into_inner())
}

/// 轮次锁守卫（方案2 进程内部分）：tokio 锁 + 持有者登记 RAII（drop 清登记）。
/// tokio MutexGuard 本身 Send——UI 路径在本结构内移交工作线程持有至轮次结束
pub(crate) struct WbRoundGuard {
    _round: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for WbRoundGuard {
    fn drop(&mut self) {
        *wb_holder_lock() = None;
    }
}

/// 撞锁详情（方案2）：报错携带持有者标签与已运行分钟数，
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
        crate::fs_utils::app_log(
            data_dir,
            &format!("[WB 轮次] 持有者「{holder_tag}」已运行超 15 分钟，疑似卡死，请排查"),
        );
    }
    // 撞锁多发生在持有初期（最常见场景）：不足 1 分钟显示「刚启动」，避免「已运行 0 分钟」
    let ran = if mins < 1 { "刚启动".to_string() } else { format!("已运行 {mins} 分钟") };
    format!("已有签到/成长任务在执行中（{holder_tag}，{ran}），请等待当前轮次完成")
}

/// 入口尝试获取轮次锁（持有者登记版）；guard 移交工作线程并持有至轮次结束
///（RAII：正常结束与 panic 展开均可靠释放，防泄漏）。
/// pub(crate)：应用内调度器（tasks/scheduler.rs）到点跑签到/成长轮次时同样抢锁互斥。
pub(crate) fn try_acquire_wb_round(
    tag: &str,
    data_dir: &std::path::Path,
) -> Result<WbRoundGuard, String> {
    let Ok(round) = WB_ROUND_LOCK.try_lock() else {
        return Err(wb_round_busy_detail(data_dir));
    };
    *wb_holder_lock() = Some((tag.to_string(), std::time::Instant::now(), false));
    Ok(WbRoundGuard { _round: round })
}

/// WB 自动签到开关（应用内调度器 wb-checkin 任务的启用判定，与启动补签同源设置）
pub(crate) fn wb_auto_checkin_enabled(state: &AppState) -> bool {
    load_settings(state).auto_checkin
}

// ── OAuth 重登自动补签（issue #220 P2）───────────────────────────────────────

/// 当日该账号签到状态（wb_checkin_results 当日最后一条记录；None = 无记录）
fn wb_today_checkin_status(state: &AppState, aid: &str) -> Option<String> {
    let root: Value =
        crate::store::docs::wb_checkin_results_load(&crate::store::db(&state.data_dir));
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    root.get("results")?
        .as_array()?
        .iter()
        .filter(|r| {
            r.get("user_id").and_then(Value::as_str) == Some(aid)
                && r.get("date").and_then(Value::as_str) == Some(today.as_str())
        })
        .last()
        .and_then(|r| r.get("status").and_then(Value::as_str).map(str::to_string))
}

/// OAuth 重登后单号补签：自动签到开启且当日记录非成功/已签时，后台单号补跑
/// 一轮（完整轮次锁幂等互斥，撞锁静默放弃——当日调度/手动轮次仍会覆盖该账号）。
/// 供 oauth.rs CN sink 调用；开关关闭/当日已成功直接返回，失败仅落日志不影响主流程。
/// docker 适配：无 AppHandle/事件总线，补签进度不进前端 SSE，仅落 app_log；
/// 结果可见性由签到结果页（wb_checkin_results）覆盖
pub(crate) fn spawn_oauth_checkin_catchup(state: &AppState, target_id: &str, label: &str) {
    if !wb_auto_checkin_enabled(state) {
        return;
    }
    if matches!(
        wb_today_checkin_status(state, target_id).as_deref(),
        Some("success") | Some("already")
    ) {
        return;
    }
    let st = state.clone();
    let target_id = target_id.to_string();
    let label = label.to_string();
    let spawned = std::thread::Builder::new()
        .name("wb-oauth-catchup".into())
        .spawn(move || {
            // 完整轮次锁在本线程内获取并持有至轮次结束（与 UI/调度轮次互斥）
            let Ok(_round) = try_acquire_wb_round("oauth-catchup", &st.data_dir) else {
                crate::fs_utils::app_log(
                    &st.data_dir,
                    &format!("wb: OAuth 补签跳过（{label}）：签到轮次锁被占用"),
                );
                return;
            };
            crate::fs_utils::app_log(
                &st.data_dir,
                &format!("wb: OAuth 重登后自动补签（{label}，{target_id}）"),
            );
            let opts = CheckinOpts {
                uids: vec![target_id],
                skip_checked: true,
                skip_expired: true,
                lazy_hours: 24,
            };
            // docker 适配：无事件总线，进度事件丢弃（结果落 wb_checkin_results 表）
            wb_checkin::run_checkin_round(&st, &opts, &mut |_ev| {});
        });
    if let Err(e) = spawned {
        crate::fs_utils::app_log(&state.data_dir, &format!("wb: OAuth 补签线程创建失败: {e}"));
    }
}

/// NDJSON 事件转发（原 python 管线同款：emit 序列化 JSON 字符串，前端逐行 JSON.parse）。
/// pub(crate)：issue #134 方案1（移植 main）——调度器后台轮次（wb-checkin/wb-growth）
/// 同步接入事件管线，前端运行态不再只由 UI 点击驱动
pub(crate) fn emit_wb_event(emit: &WbCheckinEmitter, ev: &Value) {
    if let Ok(line) = serde_json::to_string(ev) {
        let _ = emit("wb-checkin-progress", Value::String(line));
    }
}

/// 直调轮次共通封装：轮次锁 guard 移交工作线程，执行完发 exit 事件
///（原 python 子进程管线退出事件同款，前端据 "type":"exit" 复位运行态）。
fn spawn_wb_round<F>(
    state: AppState,
    round: WbRoundGuard,
    emit: WbCheckinEmitter,
    f: F,
) -> Result<(), String>
where
    F: FnOnce(&AppState) + Send + 'static,
{
    std::thread::spawn(move || {
        let _guard = round;
        // 审查 No.5：panic 兜底——f panic 会跳过下方 exit 事件，前端运行态永久悬挂。
        // catch_unwind（对齐调度器 run_task 同款先例）捕获后落日志，exit 复位事件
        // 无论如何都发出；guard RAII 不受影响（drop 在块尾依旧展开）
        if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&state))) {
            let detail = if let Some(s) = p.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = p.downcast_ref::<String>() {
                s.clone()
            } else {
                "未知 panic".to_string()
            };
            crate::fs_utils::app_log(
                &state.data_dir,
                &format!("[WB 轮次] 手动轮次 panic（exit 已补发，前端运行态已复位）：{detail}"),
            );
        }
        let _ = emit(
            "wb-checkin-progress",
            Value::String("{\"type\":\"exit\",\"ok\":true}".to_string()),
        );
    });
    Ok(())
}

/// UI 手动轮次连击同步（审查 No.3）：调度器 wb-checkin/wb-growth 有当日全败门禁
///（连击键），但手动轮次此前从不更新连击——手动全败轮会被门禁无视。手动轮次与
/// 调度器同写同一连击键（复用 buddy_ai_streak_next 纯函数）：任一成功清零 /
/// 全败 +1 / 空轮不动，值不变不落库。
fn wb_ui_round_streak_sync(st: &AppState, kv_key: &str, ok_count: i64, failed_count: i64) {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let prev = crate::scheduler::wb_round_fail_streak(st, kv_key, &today);
    let next = crate::scheduler::buddy_ai_streak_next(prev, ok_count, failed_count);
    if next != prev {
        crate::scheduler::wb_round_fail_streak_save(st, kv_key, &today, next);
    }
}

/// 启动 WorkBuddy 签到（tasks::wb_checkin 直调），
/// NDJSON → `wb-checkin-progress` 回调（独立管线，避免与 Trae checkin 状态串扰）。
pub fn workbuddy_checkin_start(
    state: &AppState,
    opts: WbCheckinOpts,
    emit: WbCheckinEmitter,
) -> Result<(), String> {
    let round = try_acquire_wb_round("ui-checkin", &state.data_dir)?;
    let o = CheckinOpts {
        uids: opts.user_ids.unwrap_or_default(),
        skip_checked: opts.skip_checked_in,
        skip_expired: opts.skip_expired,
        lazy_hours: opts.lazy_hours.unwrap_or(24),
    };
    spawn_wb_round(state.clone(), round, emit.clone(), move |st| {
        let done = wb_checkin::run_checkin_round(st, &o, &mut |ev| emit_wb_event(&emit, ev));
        // 审查 No.3：手动轮次同步连击（done 三元组口径对齐调度器：ok+already 清零）
        let ok = done.get("ok").and_then(Value::as_i64).unwrap_or(0);
        let already = done.get("already").and_then(Value::as_i64).unwrap_or(0);
        let failed = done.get("failed").and_then(Value::as_i64).unwrap_or(0);
        wb_ui_round_streak_sync(st, crate::scheduler::WB_CHECKIN_STREAK_KEY, ok + already, failed);
    })
}

/// 成长中心执行（F-17；旅行/盲盒/任务开关随设置）
pub fn workbuddy_growth_run(state: &AppState, emit: WbCheckinEmitter) -> Result<(), String> {
    let round = try_acquire_wb_round("ui-growth", &state.data_dir)?;
    let s = load_settings(state);
    let flags = GrowthOpts {
        travel: s.growth_travel,
        lottery: s.growth_lottery,
        tasks: s.growth_tasks,
    };
    spawn_wb_round(state.clone(), round, emit.clone(), move |st| {
        let mut events: Vec<Value> = Vec::new();
        wb_checkin::run_growth_round(st, &flags, &[], &mut |ev| {
            emit_wb_event(&emit, ev);
            events.push(ev.clone());
        });
        // 审查 No.3：手动轮次同步连击（growth 无汇总返回值，按事件流统计，口径对齐调度器）
        let (failed, _) = crate::scheduler::growth_fail_stats(&events);
        let ok_count = events
            .iter()
            .filter(|e| {
                e.get("type").and_then(Value::as_str) == Some("growth")
                    && e.get("status").and_then(Value::as_str) == Some("ok")
            })
            .count() as i64;
        wb_ui_round_streak_sync(st, crate::scheduler::WB_GROWTH_STREAK_KEY, ok_count, failed);
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
