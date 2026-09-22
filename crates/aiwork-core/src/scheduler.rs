//! 应用内定时调度器（Web 版唯一任务触发通道；桌面 schtasks 链随桌面壳退役）。
//!
//! ## 语义（与桌面版一致）
//!
//! - 单后台线程 60s tick；每个任务有默认触发时刻（HH:MM）；
//! - 触发条件：`今天已过触发时刻 && 状态文件 last_run_date != 今天` → 执行；
//!   服务在触发时刻之后才启动也会补跑一次（启动补跑，等价每日一次语义）；
//! - 失败重试：仅成功才记 last_run_date；失败记 last_fail_ts，30 分钟冷却后自动重试；
//! - 所有任务实现均幂等（签到 skip-checked / 快照同日覆盖 / 续期 48h lazy gate）；
//! - 串行执行：一轮 tick 内任务逐个跑完，WB 签到复用轮次锁与手动路径互斥。
//!
//! ## T8 变更
//!
//! - `start(state: AppState)` 去 AppHandle（server main 直接传入）；
//! - 豆包任务（doubao-keepalive / doubao-quota）删除；
//! - 新增 `trae-jwt-renew`（05:30）：遍历 vault 账号调 `refresh_jwt_impl(force=false)`，
//!   48h lazy gate 内置于 impl（JWT 剩余有效期 > 48h 时零网络请求），
//!   invalid 账号计入「需重新 OAuth N 个」摘要。
//!
//! ## 状态
//!
//! kv `scheduler_state`：
//! `{ "tasks": { "<key>": { "last_run_date", "last_run_ts", "last_ok", "last_fail_ts", "last_summary" } } }`
//! 前端经 `scheduler_status` 命令（命令桥分发到本模块）查看各任务最近一次执行情况。

use serde_json::{json, Value};

use crate::commands::accounts;
use crate::fs_utils;
use crate::state::AppState;

/// 调度任务定义（默认触发时刻为本地时间 HH:MM）
struct SchedTask {
    key: &'static str,
    name: &'static str,
    /// 默认触发时刻 HH:MM
    hhmm: &'static str,
}

const TASKS: &[SchedTask] = &[
    // T8 新增：临期 JWT 自动续期（排在签到前，避免签到时大面积 401）
    SchedTask { key: "trae-jwt-renew", name: "Trae JWT 自动续期", hhmm: "05:30" },
    // 官网模型列表每日同步（batch_get_detail_param，不消耗积分），供网关模型映射与
    // 前端模型选择器使用；此前仅 API 服务页手动触发，自动化后「自动同步服务器数据」闭环
    SchedTask { key: "models-sync", name: "Trae 模型列表同步", hhmm: "05:40" },
    SchedTask { key: "trae-checkin", name: "Trae 每日签到", hhmm: "09:00" },
    SchedTask { key: "wb-checkin", name: "WorkBuddy 每日签到", hhmm: "09:10" },
    // F-09 兜底续期：lazy 24h（到期前 24h 内才真正刷新），每天跑一次是安全超集
    SchedTask { key: "wb-renew", name: "WorkBuddy token 兜底续期", hhmm: "10:30" },
    // 快照类排到晚间（接近日末，差分口径最准）
    SchedTask { key: "wb-credits-snapshot", name: "WorkBuddy 积分余额每日快照", hhmm: "23:30" },
    SchedTask { key: "trae-credits-snapshot", name: "Trae 积分余额每日快照", hhmm: "23:40" },
];

/// 启动调度线程（server main 调用；启动 90s 后首跑，避开启动高峰）
pub fn start(state: AppState) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(90));
        loop {
            tick(&state);
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });
}

/// 失败重试冷却：失败后 30 分钟内不重复尝试（避免每 60s tick 连打）
const RETRY_COOLDOWN_MS: i64 = 30 * 60_000;

/// 单轮 tick：检查每个任务「今天已过触发时刻 && 今天未跑」→ 执行
fn tick(st: &AppState) {
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let now_hm = now.format("%H:%M").to_string();
    // T11：通知配置整轮读一次（签到成功/任务失败推送，内部再按总开关静默）
    let notify_cfg = crate::notify::load_config(st);
    // 自定义时刻整轮读一次（kv 单读，各任务共用）
    let times = task_times(st);
    for t in TASKS {
        // 各任务启用判定（与既有设置语义保持一致）
        if !enabled(st, t.key) {
            continue;
        }
        if last_run_date(st, t.key).as_deref() == Some(today.as_str()) {
            continue;
        }
        // 生效时刻：自定义覆盖 > 默认；HH:MM 零填充，字符串比较即时间序
        let hhmm = times.get(t.key).map(String::as_str).unwrap_or(t.hhmm);
        if now_hm.as_str() < hhmm {
            continue;
        }
        // 失败冷却：30 分钟内静默等待重试，不重复执行也不刷日志
        if let Some(ts) = last_fail_ts(st, t.key) {
            if chrono::Utc::now().timestamp_millis() - ts < RETRY_COOLDOWN_MS {
                continue;
            }
        }
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_task(t.key, st)
        })) {
            Ok(Ok(v)) => Ok(summarize(&v)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("任务线程 panic（已捕获，不影响后续调度）".to_string()),
        };
        match outcome {
            Ok(summary) => {
                mark_run(st, t.key, &today, &summary);
                fs_utils::app_log(
                    &st.data_dir,
                    &format!("[调度器] {}（每日 {hhmm}）：{}", t.name, summary),
                );
                // T11：仅签到类任务完成时推送（快照/续期类静默，避免每日刷屏）
                if notify_cfg.on_checkin_done && matches!(t.key, "trae-checkin" | "wb-checkin") {
                    crate::notify::send(st, &format!("{}完成", t.name), &summary, "checkin");
                }
            }
            Err(summary) => {
                mark_fail(st, t.key, &summary);
                fs_utils::app_log(
                    &st.data_dir,
                    &format!(
                        "[调度器] {}（每日 {hhmm}）失败，30 分钟后重试：{}",
                        t.name, summary
                    ),
                );
                // T11：任务失败推送（全部任务；通知内部再按总开关静默）
                if notify_cfg.on_task_failed {
                    crate::notify::send(st, &format!("{}失败", t.name), &summary, "task_failed");
                }
            }
        }
    }
}

/// 任务启用判定：先看用户开关（kv `scheduler_cfg.disabled_tasks`，默认全开 = 推荐配置），
/// 再叠加各任务既有设置语义
fn enabled(st: &AppState, key: &str) -> bool {
    if disabled_tasks(st).iter().any(|k| k == key) {
        return false;
    }
    match key {
        // WorkBuddy 签到跟随「启动自动补签」开关（F-55 同源设置）
        "wb-checkin" => crate::commands::workbuddy::wb_auto_checkin_enabled(st),
        // 其余任务幂等且低风险，恒开（Trae 签到 run_round 自带状态核验；JWT 续期自带 48h 门）
        _ => true,
    }
}

// ── 任务配置（kv `scheduler_cfg`）────────────────────────────────────────────
// 形态：{ "disabled_tasks": ["trae-checkin", ...], "task_times": { "trae-checkin": "08:30", ... } }
// 缺省 = 全部启用 + 各任务默认时刻（推荐配置）。前端在 Trae / Buddy 环境配置页展示。

fn disabled_tasks(st: &AppState) -> Vec<String> {
    load_cfg(st)
        .get("disabled_tasks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// 自定义触发时刻表（key → HH:MM；缺省键 = 用任务默认时刻）
fn task_times(st: &AppState) -> std::collections::HashMap<String, String> {
    load_cfg(st)
        .get("task_times")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn load_cfg(st: &AppState) -> Value {
    let v: Value = crate::store::db(&st.data_dir).kv_get("scheduler_cfg");
    if v.is_object() { v } else { json!({}) }
}

/// 任务配置查询（命令桥 scheduler_config_get 分发）
pub fn scheduler_config_get(st: &AppState) -> Value {
    json!({
        "disabled_tasks": disabled_tasks(st),
        "task_times": task_times(st),
    })
}

/// HH:MM 合法性（00:00–23:59）
fn valid_hhmm(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return false;
    }
    let (h, m) = (s[..2].parse::<u8>().ok(), s[3..].parse::<u8>().ok());
    matches!((h, m), (Some(h), Some(m)) if h < 24 && m < 60)
}

/// 任务配置写入（命令桥 scheduler_config_set 分发）：未知任务键整体拒绝，
/// 自定义时刻需合法 HH:MM，返回保存后的生效值
pub fn scheduler_config_set(st: &AppState, cfg: Value) -> Result<Value, String> {
    let list = cfg
        .get("disabled_tasks")
        .and_then(Value::as_array)
        .ok_or("缺少 disabled_tasks 数组")?;
    let mut disabled: Vec<String> = Vec::new();
    for v in list {
        let k = v.as_str().ok_or("disabled_tasks 含非字符串项")?;
        if !TASKS.iter().any(|t| t.key == k) {
            return Err(format!("未知调度任务: {k}"));
        }
        disabled.push(k.to_string());
    }
    // 自定义时刻：整表替换；空对象 = 全部回默认。只存与默认不同的键也无妨——
    // 全量存便于前端整表回显，语义等价
    let mut times: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(m) = cfg.get("task_times") {
        let obj = m.as_object().ok_or("task_times 应为对象")?;
        for (k, v) in obj {
            if !TASKS.iter().any(|t| t.key == k) {
                return Err(format!("未知调度任务: {k}"));
            }
            let s = v.as_str().ok_or_else(|| format!("任务 {k} 的时刻应为 HH:MM 字符串"))?;
            if !valid_hhmm(s) {
                return Err(format!("任务 {k} 的时刻 {s} 非法（应为 00:00–23:59）"));
            }
            times.insert(k.clone(), s.to_string());
        }
    }
    let _ = crate::store::db(&st.data_dir).kv_set(
        "scheduler_cfg",
        &json!({ "disabled_tasks": disabled, "task_times": times }),
    );
    fs_utils::app_log(
        &st.data_dir,
        &format!(
            "[调度器] 任务配置已更新：{} 项停用，{} 项自定义时刻",
            disabled.len(),
            times.len()
        ),
    );
    Ok(scheduler_config_get(st))
}

/// 执行单个任务（复用 CLI 任务同款实现，进度静默、结果汇总落日志）
fn run_task(key: &str, st: &AppState) -> Result<Value, String> {
    match key {
        // T8 新增：Trae JWT 自动续期（48h lazy gate 内置于 refresh_jwt_impl）
        "trae-jwt-renew" => run_trae_jwt_renew(st),
        // 官网模型列表每日同步：与 API 服务页「同步模型列表」同款实现
        "models-sync" => run_models_sync(st),
        // Trae 每日签到：与 `--task-run checkin` 同款（vault 全账号单轮，状态核验幂等）
        "trae-checkin" => {
            let accts = crate::vault::load_accounts(st);
            let retry = st.settings().retry.max(0) as u32;
            Ok(crate::tasks::trae_checkin::run_round(
                st,
                &accts.accounts,
                retry,
                &mut |_| {},
            ))
        }
        // WorkBuddy 每日签到：与 `--task-run wb-checkin` 同款；抢轮次锁与 UI 路径互斥
        "wb-checkin" => {
            let Ok(_round) = crate::commands::workbuddy::try_acquire_wb_round() else {
                return Err("跳过：已有签到/成长任务在执行中".into());
            };
            Ok(crate::tasks::wb_checkin::run_checkin_round(
                st,
                &crate::tasks::wb_checkin::CheckinOpts::daily(),
                &mut |_| {},
            ))
        }
        // WorkBuddy token 兜底续期：与 `--task-run wb-renew` 同款（lazy 24h）
        "wb-renew" => Ok(crate::tasks::wb_checkin::run_renew_only(st, 24)),
        // WorkBuddy 积分余额每日快照：补齐近 7 日消耗差分时序
        "wb-credits-snapshot" => crate::commands::workbuddy::wb_credits_snapshot_task(st).map(
            |p| {
                json!({
                    "ok": true,
                    "accounts": p.get("accounts").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0)
                })
            },
        ),
        // Trae 积分余额每日快照：与 `--task-run refresh-credits` 同款
        "trae-credits-snapshot" => accounts::refresh_remaining_credits_impl(st)
            .map(|n| json!({ "ok": true, "refreshed": n })),
        other => Err(format!("未知调度任务: {other}")),
    }
}

/// 官网模型列表每日同步：vault 全账号复用 models_sync::fetch_official（首个含 JWT
/// 账号，最多尝试 3 个）。无可用账号视为跳过（非失败），避免冷启动每日失败告警。
fn run_models_sync(st: &AppState) -> Result<Value, String> {
    let accounts = crate::vault::load_accounts(st);
    let has_creds = accounts.accounts.iter().any(|a| !a.jwt.trim().is_empty());
    if !has_creds {
        return Ok(json!({ "summary": "跳过：无可用账号（未添加或缺少 JWT）" }));
    }
    match crate::api_server::models_sync::fetch_official(&st.data_dir, accounts) {
        Ok(list) => Ok(json!({
            "summary": format!("官网模型列表同步成功: {} 个模型", list.len()),
            "models": list.len(),
        })),
        Err(e) => Err(e),
    }
}

/// Trae JWT 自动续期（T8 新增）：遍历 vault 账号逐个调 refresh_jwt_impl(force=false)。
/// 48h lazy gate 在 impl 内生效（JWT 剩余有效期 > 48h 时直接返回「暂无需刷新」，零网络请求）；
/// invalid（refresh_token 被服务端吊销/明确拒绝）计入「需重新 OAuth」摘要，由人工重新录入。
fn run_trae_jwt_renew(st: &AppState) -> Result<Value, String> {
    let accts = crate::vault::load_accounts(st);
    let mut renewed = 0usize;
    let mut lazy_skipped = 0usize;
    let mut need_reauth = 0usize;
    let mut no_refresh_token = 0usize;
    let mut failed = 0usize;
    for a in &accts.accounts {
        let Some(uid) = a.user_id.as_deref() else { continue };
        match accounts::refresh_jwt_impl(st, uid, false) {
            Ok(_) => renewed += 1,
            Err(e) if e.contains("暂无需刷新") => lazy_skipped += 1,
            Err(e) if e.contains("需重新 OAuth") => need_reauth += 1,
            Err(e) if e.contains("无 refresh_token") => no_refresh_token += 1,
            Err(_) => failed += 1,
        }
    }
    Ok(json!({
        "summary": format!(
            "续期 {renewed}，门跳过 {lazy_skipped}，需重新 OAuth {need_reauth}，无凭据 {no_refresh_token}，失败 {failed}"
        ),
        "renewed": renewed,
        "lazy_skipped": lazy_skipped,
        "need_reauth": need_reauth,
        "no_refresh_token": no_refresh_token,
        "failed": failed,
    }))
}

// ── 状态持久化 ───────────────────────────────────────────────────────────────

fn load_state(st: &AppState) -> Value {
    // SQLite 化（P2）：scheduler_state.json → kv `scheduler_state`
    let v: Value = crate::store::db(&st.data_dir).kv_get("scheduler_state");
    if v.is_object() { v } else { json!({}) }
}

fn last_run_date(st: &AppState, key: &str) -> Option<String> {
    load_state(st)
        .pointer(&format!("/tasks/{key}/last_run_date"))
        .and_then(Value::as_str)
        .map(String::from)
}

fn last_fail_ts(st: &AppState, key: &str) -> Option<i64> {
    load_state(st)
        .pointer(&format!("/tasks/{key}/last_fail_ts"))
        .and_then(Value::as_i64)
}

/// 成功：记「今天已跑」（整体覆盖条目，同时清掉 last_fail_ts）
fn mark_run(st: &AppState, key: &str, date: &str, summary: &str) {
    write_entry(
        st,
        key,
        json!({
            "last_run_date": date,
            "last_run_ts": chrono::Utc::now().timestamp_millis(),
            "last_ok": true,
            "last_summary": summary,
        }),
    );
}

/// 失败：只记失败时间戳，不写 last_run_date（冷却后当天自动重试）
fn mark_fail(st: &AppState, key: &str, summary: &str) {
    write_entry(
        st,
        key,
        json!({
            "last_fail_ts": chrono::Utc::now().timestamp_millis(),
            "last_ok": false,
            "last_summary": summary,
        }),
    );
}

fn write_entry(st: &AppState, key: &str, entry: Value) {
    let mut root = load_state(st);
    if let Some(obj) = root.as_object_mut() {
        let tasks = obj.entry("tasks".to_string()).or_insert_with(|| json!({}));
        tasks[key] = entry;
    }
    let _ = crate::store::db(&st.data_dir).kv_set("scheduler_state", &root);
}

/// 结果 JSON → 单行摘要（含 summary 字段直接取用；签到轮次取 ok/already/failed 计数，其余截断展示）
fn summarize(v: &Value) -> String {
    if let Some(s) = v.get("summary").and_then(Value::as_str) {
        return s.to_string();
    }
    if v.is_object() {
        let get = |k: &str| v.get(k).and_then(Value::as_i64);
        if let (Some(o), Some(a), Some(f)) = (get("ok"), get("already"), get("failed")) {
            return format!("成功 {o}，已签 {a}，失败 {f}");
        }
    }
    let s = serde_json::to_string(v).unwrap_or_default();
    s.chars().take(120).collect()
}

/// 调度器状态查询（命令桥 scheduler_status 分发到此）：任务定义 + 最近一次执行情况
pub fn scheduler_status(st: &AppState) -> Value {
    let raw = load_state(st);
    let times = task_times(st);
    let tasks: Vec<Value> = TASKS
        .iter()
        .map(|t| {
            let e = raw
                .pointer(&format!("/tasks/{}", t.key))
                .cloned()
                .unwrap_or(Value::Null);
            json!({
                "key": t.key,
                "name": t.name,
                "time": times.get(t.key).cloned().unwrap_or_else(|| t.hhmm.to_string()),
                "enabled": enabled(st, t.key),
                "last_run_date": e.get("last_run_date").cloned().unwrap_or(Value::Null),
                "last_run_ts": e.get("last_run_ts").cloned().unwrap_or(Value::Null),
                "last_fail_ts": e.get("last_fail_ts").cloned().unwrap_or(Value::Null),
                "last_ok": e.get("last_ok").cloned().unwrap_or(Value::Null),
                "last_summary": e.get("last_summary").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({ "tasks": tasks })
}
