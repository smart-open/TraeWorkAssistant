//! Qoder 签到域（F-80 M1，对照 commands/workbuddy/checkin.rs 模式）：
//! 签到（NDJSON 管线）/ 签到结果 / 启动自动补签。
//!
//! 每日定时统一由应用内 Rust 调度器（tasks/scheduler.rs，60s tick + 当日幂等 +
//! 启动补跑）驱动，全平台一致；2026-10-05 起移除 Windows schtasks 双轨命令
//! （qoder_checkin_task_register/status/unregister）——原双轨与调度器重复维护
//! 时刻口径，且 mac 无对应物；时刻修改统一走 settings.qoder_checkin_hhmm。

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;
use crate::tasks::qoder_checkin::{self, QoderCheckinOpts};

use super::common::load_settings;

#[derive(serde::Deserialize)]
pub struct QoderCheckinOptsDto {
    #[serde(default)]
    pub user_ids: Option<Vec<String>>,
    #[serde(default)]
    pub skip_checked_in: bool,
    #[serde(default)]
    pub lazy_hours: Option<i64>,
}

/// NDJSON 事件转发（与 wb 管线同款：emit 序列化 JSON 字符串，前端逐行 JSON.parse）
fn emit_qoder_event(app: &AppHandle, ev: &Value) {
    if let Ok(line) = serde_json::to_string(ev) {
        let _ = app.emit("qoder-checkin-progress", &line);
    }
}

/// 发起 Qoder 签到（轮次锁互斥 + 工作线程执行 + exit 收尾事件）
#[tauri::command(async)]
pub fn qoder_checkin_start(
    app: AppHandle,
    state: State<AppState>,
    opts: QoderCheckinOptsDto,
) -> Result<(), String> {
    let round = qoder_checkin::try_acquire_qoder_round()?;
    let o = QoderCheckinOpts {
        uids: opts.user_ids.unwrap_or_default(),
        skip_checked: opts.skip_checked_in,
        lazy_hours: opts.lazy_hours.unwrap_or(24),
    };
    let app2 = app.clone();
    let state2 = state.inner().clone();
    // I17（对齐 oauth）：命名线程便于诊断；spawn 失败时闭包（已 move 持有轮次锁
    // guard）随之 drop 自动释放，不会阻塞后续签到
    let spawned = std::thread::Builder::new()
        .name("qoder-checkin".into())
        .spawn(move || {
            let _guard = round;
            // panic 不外泄线程：捕获后记录，exit 终态照常下发（否则前端运行态永挂）
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                qoder_checkin::run_checkin_round(&state2, &o, &mut |ev| emit_qoder_event(&app2, ev));
            }));
            // P3 审查修复：panic 轮次的 exit 终态 ok=false——此前恒 true 会把「签到线程
            // 崩溃」上报为成功轮次（结果失真）；前端仅按 type=="exit" 复位运行态，
            // 不消费 ok 字段，语义收紧无破坏
            let ok = result.is_ok();
            if !ok {
                fs_utils::app_log(&state2.data_dir, "Qoder 签到轮次线程 panic（已捕获，exit 终态 ok=false 下发）");
            }
            // 终态事件（前端据 "type":"exit" 复位运行态）：emit 失败落日志（issue #44 约定对齐）。
            // 契约同 wb：NDJSON **字符串** payload（listen<string> 后 JSON.parse），传对象会
            // 破坏 parseLine 导致 exit 事件被静默丢弃
            let line = serde_json::json!({ "type": "exit", "ok": ok }).to_string();
            crate::events::emit_logged(
                &app2,
                "qoder-checkin-progress",
                serde_json::Value::String(line),
                Some(state2.data_dir.as_path()),
            );
        });
    if let Err(e) = spawned {
        return Err(format!("签到后台线程启动失败: {e}"));
    }
    Ok(())
}

#[derive(Serialize, Clone)]
pub struct QoderCheckinRecord {
    pub date: String,
    pub time: String,
    pub user_id: String,
    pub name: String,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reward: Option<f64>,
    /// 逐活动明细（F-80-余 v3 档期日历）：[{id, name, kind, reward?}]；
    /// 历史数据（升级前）无此字段 → None，前端按整体状态渲染
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaigns: Option<Value>,
}

/// 签到日志（90 天存储，UI 默认展示 30 天）
#[tauri::command]
pub fn qoder_checkin_results(
    state: State<AppState>,
    days: Option<i64>,
) -> Result<Vec<QoderCheckinRecord>, String> {
    let days = days.unwrap_or(30).clamp(1, 90);
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let raw: Value = crate::store::docs::qoder_checkin_results_load(&crate::store::db(&state.data_dir));
    let mut out = Vec::new();
    if let Some(arr) = raw.get("results").and_then(|v| v.as_array()) {
        for r in arr {
            let date = r.get("date").and_then(|v| v.as_str()).unwrap_or("");
            if date < cutoff.as_str() {
                continue;
            }
            out.push(QoderCheckinRecord {
                date: date.into(),
                time: r.get("time").and_then(|v| v.as_str()).unwrap_or("").into(),
                user_id: r.get("user_id").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: r.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                status: r.get("status").and_then(|v| v.as_str()).unwrap_or("").into(),
                message: r.get("message").and_then(|v| v.as_str()).unwrap_or("").into(),
                reward: r.get("reward").and_then(|v| v.as_f64()),
                campaigns: r.get("campaigns").filter(|v| v.is_array()).cloned(),
            });
        }
    }
    out.reverse(); // 新→旧
    Ok(out)
}

// ── 启动自动补签（F-55 模式；main.rs setup 调用）───────────────────────────

/// 启动自动补签核心：延迟 60s + 轮次锁互斥；未签自动补签，静默执行零打扰。
pub fn startup_auto_checkin(app: &AppHandle, state: &AppState) {
    let s = load_settings(state);
    if !s.auto_checkin {
        return;
    }
    let app2 = app.clone();
    let state2 = state.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(60));
        let Ok(_round) = qoder_checkin::try_acquire_qoder_round() else {
            fs_utils::app_log(&state2.data_dir, "Qoder 启动补签跳过：已有签到任务在执行中");
            return;
        };
        let opts = QoderCheckinOpts::daily();
        fs_utils::app_log(&state2.data_dir, "Qoder 启动补签：开始核验签到状态");
        let done = qoder_checkin::run_checkin_round(&state2, &opts, &mut |_| {});
        // skipped_busy：轮次锁已获取但跨进程锁被占（如 schtasks 同刻触发），幂等跳过；
        // failed=0 本就不会触发失败推送，此处仅修正日志可读性（不再误记「成功 0 失败 0」）
        let msg = if done["skipped_busy"].as_bool().unwrap_or(false) {
            "Qoder 启动补签跳过：另一进程正在执行签到（跨进程锁占用）".to_string()
        } else {
            format!(
                "Qoder 启动补签完成: 成功 {}，已签 {}，失败 {}",
                done["ok"].as_i64().unwrap_or(0),
                done["already"].as_i64().unwrap_or(0),
                done["failed"].as_i64().unwrap_or(0),
            )
        };
        fs_utils::app_log(&state2.data_dir, &msg);
        let failed = done["failed"].as_i64().unwrap_or(0);
        // empty_campaigns（活动未开始/不可用）非用户可操作失败：重试也无解，仅记日志
        // 不推送打扰（审查 L；done.failed_empty_campaigns 由 run_checkin_round 单列）
        let failed_actionable =
            failed - done["failed_empty_campaigns"].as_i64().unwrap_or(0);
        if failed_actionable > 0 {
            crate::commands::workbuddy::push_notify(
                Some(&app2),
                &state2.data_dir,
                "Qoder 签到提醒",
                &format!("启动补签有 {failed_actionable} 个账号失败，请在 Qoder 签到页查看"),
                crate::notify::NotifyEvent::TaskFail,
            );
        }
    });
}
