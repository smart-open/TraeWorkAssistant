//! Qoder 签到域（F-80 M1 平移，对照 commands/workbuddy/checkin.rs 模式）：
//! 签到（NDJSON 管线）/ 签到结果。
//! 桌面专属（schtasks 双轨定时任务/startup_auto_checkin）不移植——
//! Web 版调度走 tasks/scheduler.rs 应用内调度器。

use serde::Serialize;
use serde_json::Value;

use crate::fs_utils;
use crate::state::AppState;
use crate::tasks::qoder_checkin::{self, QoderCheckinOpts};

// ── M1 签到（NDJSON 管线）──────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct QoderCheckinOptsDto {
    #[serde(default)]
    pub user_ids: Option<Vec<String>>,
    #[serde(default)]
    pub skip_checked_in: bool,
    #[serde(default)]
    pub lazy_hours: Option<i64>,
}

/// Qoder 签到/进度事件回调（Web 化替代 AppHandle::emit）：(事件名, 载荷)。
/// 事件名固定 "qoder-checkin-progress"，载荷与桌面 emit 契约一致（序列化 JSON
/// 字符串，前端逐行 JSON.parse；exit 结束事件同款）。
pub type QoderCheckinEmitter = std::sync::Arc<dyn Fn(&str, Value) + Send + Sync>;

/// NDJSON 事件转发（与 wb 管线同款：emit 序列化 JSON 字符串，前端逐行 JSON.parse）
fn emit_qoder_event(emit: &QoderCheckinEmitter, ev: &Value) {
    if let Ok(line) = serde_json::to_string(ev) {
        let _ = emit("qoder-checkin-progress", Value::String(line));
    }
}

/// 发起 Qoder 签到（轮次锁互斥 + 工作线程执行 + exit 收尾事件）。
/// I17（对齐 oauth）：命名线程便于诊断；spawn 失败时闭包（已 move 持有轮次锁
/// guard）随之 drop 自动释放，不会阻塞后续签到。
pub fn qoder_checkin_start(
    state: &AppState,
    opts: QoderCheckinOptsDto,
    emit: QoderCheckinEmitter,
) -> Result<(), String> {
    let round = qoder_checkin::try_acquire_qoder_round()?;
    let o = QoderCheckinOpts {
        uids: opts.user_ids.unwrap_or_default(),
        skip_checked: opts.skip_checked_in,
        lazy_hours: opts.lazy_hours.unwrap_or(24),
    };
    let state2 = state.clone();
    let spawned = std::thread::Builder::new()
        .name("qoder-checkin".into())
        .spawn(move || {
            let _guard = round;
            // panic 不外泄线程：捕获后记录，exit 终态照常下发（否则前端运行态永挂）
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                qoder_checkin::run_checkin_round(&state2, &o, &mut |ev| emit_qoder_event(&emit, ev));
            }));
            // P3 审查修复（main 版）：panic 轮次的 exit 终态 ok=false——此前恒 true 会把
            // 「签到线程崩溃」上报为成功轮次（结果失真）；前端仅按 type=="exit" 复位
            // 运行态，不消费 ok 字段，语义收紧无破坏
            let ok = result.is_ok();
            if !ok {
                fs_utils::app_log(&state2.data_dir, "Qoder 签到轮次线程 panic（已捕获，exit 终态 ok=false 下发）");
            }
            // 终态事件（前端据 "type":"exit" 复位运行态）：emit 失败落日志（issue #44 约定对齐）。
            // 契约同 wb：NDJSON **字符串** payload（listen<string> 后 JSON.parse），传对象会
            // 破坏 parseLine 导致 exit 事件被静默丢弃
            let line = serde_json::json!({ "type": "exit", "ok": ok }).to_string();
            let _ = emit("qoder-checkin-progress", Value::String(line));
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
pub fn qoder_checkin_results(
    state: &AppState,
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
