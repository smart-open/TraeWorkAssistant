//! 应用内定时调度器（Rust 原生方案，补充 Windows schtasks 计划任务）。
//!
//! ## 背景（依赖分析结论）
//!
//! 工程内的每日/每周业务任务此前 **100% 依赖 Windows schtasks 注册**：
//! 各设置页 `/SC DAILY|WEEKLY` 注册任务名（AIWorkAssistant_DailyCheckin、
//! AIWorkAssistant_WorkBuddyCheckin_<HHMM>、AIWorkAssistant_WorkBuddyRenew、
//! AIWorkAssistant_DoubaoRenew、AIWorkAssistant_DoubaoQuotaCheck），
//! 触发时直调主 exe CLI 任务模式（`--task-run <name>`）。缺口：
//!
//! 1. schtasks 未注册 / 注册失败（如权限不足 Access Denied）时，应用开着
//!    也不会跑任何每日任务；
//! 2. 积分余额快照（Trae `credits_daily.json` / WorkBuddy
//!    `workbuddy_credits_history.json`）此前无任何定时写入——只在打开积分页
//!    且非缓存命中时落盘，未打开应用的日子快照缺天，「近 7 日积分消耗」
//!    （快照差分口径）因此只剩昨天一格。
//!
//! ## 语义
//!
//! - 单后台线程 60s tick；每个任务有默认触发时刻（HH:MM，对齐 schtasks 典型时段）；
//! - 触发条件：`今天已过触发时刻 && 状态文件 last_run_date != 今天` → 执行；
//!   应用在触发时刻之后才启动也会补跑一次（启动补跑，等价每日一次语义）；
//! - 失败重试：仅成功才记 last_run_date；失败记 last_fail_ts，30 分钟冷却后
//!   自动重试（避免一次网络抖动丢掉整天的签到/快照，也避免每 tick 连打）；
//! - 所有任务实现均幂等（签到 skip-checked / 快照同日覆盖），与 schtasks
//!   重复触发不会产生重复效果；
//! - 串行执行：一轮 tick 内任务逐个跑完，WB 签到复用轮次锁与 UI 路径互斥。
//!
//! ## 状态
//!
//! `scheduler_state.json`：
//! `{ "tasks": { "<key>": { "last_run_date", "last_run_ts", "last_ok", "last_fail_ts", "last_summary" } } }`
//! 前端经 `scheduler_status` 命令查看各任务最近一次执行情况。

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::fs_utils;
use crate::state::AppState;

/// 调度任务定义（默认触发时刻为本地时间 HH:MM）
struct SchedTask {
    key: &'static str,
    name: &'static str,
    /// 默认触发时刻 HH:MM
    hhmm: &'static str,
    /// 调度配置来源："fix"=既有语义（恒开或跟随专属开关/时刻）；
    /// "credits"=看板数据同步（settings {off|hourly|daily, hhmm}）；
    /// "models"=模型同步（settings {enabled, hhmm}）
    kind: &'static str,
}

const TASKS: &[SchedTask] = &[
    SchedTask { key: "trae-checkin", name: "Trae 每日签到", hhmm: "09:00", kind: "fix" },
    // Trae JWT 定时续期（issue #27）：默认 09:00，环境配置页可改（settings.jwt_renew_hhmm）；
    // 惰性判定（剩余 ≤48h 才真正刷新），每日一次为安全超集
    SchedTask { key: "trae-renew", name: "Trae JWT 定时续期", hhmm: "09:00", kind: "fix" },
    SchedTask { key: "wb-checkin", name: "WorkBuddy 每日签到", hhmm: "09:10", kind: "fix" },
    // WorkBuddy 每日成长（任务配置页）：默认 09:00 可改（settings.wb_growth_hhmm）；
    // 成长三开关驱动（旅行/盲盒/任务），全关时空轮无副作用
    SchedTask { key: "wb-growth", name: "WorkBuddy 每日成长", hhmm: "09:00", kind: "fix" },
    // F-09 兜底续期：lazy 24h（到期前 24h 内才真正刷新），每天跑一次是安全超集
    SchedTask { key: "wb-renew", name: "WorkBuddy token 兜底续期", hhmm: "10:30", kind: "fix" },
    SchedTask { key: "doubao-keepalive", name: "豆包会话每日续期", hhmm: "09:20", kind: "fix" },
    SchedTask { key: "doubao-quota", name: "豆包会员额度每日巡检", hhmm: "09:30", kind: "fix" },
    // 看板数据同步（原快照任务升级为可配置频率：每日 HH:MM / 每小时 / 关闭）：
    // wb 侧含积分 fresh 拉取+池回写+快照、Token 统计重扫与官方用量刷新；
    // 排到晚間接近日末，差分口径最准；此前无定时写入是近 7 日消耗缺天的根因
    SchedTask { key: "wb-credits-snapshot", name: "WorkBuddy 积分与 Token 数据同步", hhmm: "23:30", kind: "credits" },
    SchedTask { key: "trae-credits-snapshot", name: "Trae 积分数据同步", hhmm: "23:40", kind: "credits" },
    // 模型同步（每日 HH:MM，默认开；无账号时静默跳过不计失败，对齐 models-sync 惯例）
    SchedTask { key: "trae-models-sync", name: "Trae 模型列表同步", hhmm: "05:40", kind: "models" },
    SchedTask { key: "wb-catalog-sync", name: "WorkBuddy 模型目录同步", hhmm: "05:45", kind: "models" },
];

/// 启动调度线程（main.rs setup 调用；启动 90s 后首跑，避开启动高峰）
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(90));
        loop {
            let st = app.state::<AppState>();
            tick(&st);
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });
}

/// 失败重试冷却：失败后 30 分钟内不重复尝试（避免每 60s tick 连打）
const RETRY_COOLDOWN_MS: i64 = 30 * 60_000;
/// hourly 模式节流：距上次成功执行 ≥1h 才再跑（失败走 30 分钟冷却，不受此门限制）
const HOURLY_INTERVAL_MS: i64 = 60 * 60_000;

/// 单任务的生效调度计划：Skip=关闭；Daily=每日 HH:MM（到点+当日未跑，启动补跑）；
/// Hourly=每小时（距上次成功执行 ≥1h，无记录=首次立即跑）
enum SchedPlan {
    Skip,
    Daily(String),
    Hourly,
}

/// credits 类任务的同步模式（空/未知值按 daily 处理——state::settings 已归一，此处兜底）
fn credits_sync_mode(st: &AppState, key: &str) -> String {
    let s = st.settings();
    let raw = match key {
        "wb-credits-snapshot" => s.wb_credits_sync_mode.as_str(),
        _ => s.trae_credits_sync_mode.as_str(),
    };
    match raw.trim() {
        "off" | "hourly" | "daily" => raw.trim().to_string(),
        _ => "daily".to_string(),
    }
}

/// 单任务调度计划：合并启用判定与模式/时刻解析（tick 与 scheduler_status 共用）
fn sched_plan(st: &AppState, t: &SchedTask) -> SchedPlan {
    if !enabled(st, t.key) {
        return SchedPlan::Skip;
    }
    match t.kind {
        // 看板数据同步：hourly 按小时节流，daily（含非法值回退）按每日时刻
        "credits" if credits_sync_mode(st, t.key) == "hourly" => SchedPlan::Hourly,
        _ => SchedPlan::Daily(effective_hhmm(st, t)),
    }
}

/// 单轮 tick：按各任务生效调度计划检查触发条件 → 执行
fn tick(st: &AppState) {
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let now_hm = now.format("%H:%M").to_string();
    for t in TASKS {
        // 触发判定（trigger 用于日志展示：HH:MM 或「每小时」）
        let trigger = match sched_plan(st, t) {
            SchedPlan::Skip => continue,
            SchedPlan::Daily(hhmm) => {
                if last_run_date(st, t.key).as_deref() == Some(today.as_str()) {
                    continue;
                }
                // HH:MM 零填充，字符串比较即时间序
                if now_hm.as_str() < hhmm.as_str() {
                    continue;
                }
                hhmm
            }
            SchedPlan::Hourly => {
                if let Some(ts) = last_run_ts(st, t.key) {
                    if chrono::Utc::now().timestamp_millis() - ts < HOURLY_INTERVAL_MS {
                        continue;
                    }
                }
                "每小时".to_string()
            }
        };
        // 失败冷却：30 分钟内静默等待重试，不重复执行也不刷日志
        if let Some(ts) = last_fail_ts(st, t.key) {
            if chrono::Utc::now().timestamp_millis() - ts < RETRY_COOLDOWN_MS {
                continue;
            }
        }
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_task(t.key, st))) {
            Ok(Ok(v)) => Ok(summarize(&v)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("任务线程 panic（已捕获，不影响后续调度）".to_string()),
        };
        match outcome {
            Ok(summary) => {
                mark_run(st, t.key, &today, &summary);
                fs_utils::app_log(&st.data_dir, &format!("[调度器] {}（每日 {}）：{}", t.name, trigger, summary));
            }
            Err(summary) => {
                // 当日首败判定（在 mark_fail 覆盖前读旧值）：上次失败不在今天 → 今天首次失败。
                // 只有当日首败才推送渠道通知，30 分钟冷却后的重试失败只落日志，
                // 避免全天故障时单任务刷 ~30 条通知（Server酱免费档每日仅 5 条）
                let first_fail_today = match last_fail_ts(st, t.key) {
                    None => true,
                    Some(ts) => {
                        let prev_date = chrono::DateTime::from_timestamp_millis(ts)
                            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string());
                        prev_date.as_deref() != Some(today.as_str())
                    }
                };
                mark_fail(st, t.key, &summary);
                fs_utils::app_log(&st.data_dir, &format!("[调度器] {}（每日 {}）失败，30 分钟后重试：{}", t.name, trigger, summary));
                // 调度任务失败通知（通知渠道面板）：推 Bark/Webhook/Server酱（无 AppHandle，仅渠道；
                // 渠道失败静默，绝不影响调度循环）
                if first_fail_today {
                    crate::commands::workbuddy::push_notify(
                        None,
                        &st.data_dir,
                        &format!("{} 失败", t.name),
                        &format!("{summary}（30 分钟后自动重试）"),
                        crate::notify::NotifyEvent::TaskFail,
                    );
                }
            }
        }
    }
}

/// 任务生效触发时刻：trae-renew / wb-growth 跟随设置页时刻（环境配置页可改），
/// 看板同步与模型同步跟随各自 settings 时刻；其余任务用内置默认；
/// 配置非法（非 HH:MM 格式）时回退默认值
fn effective_hhmm(st: &AppState, t: &SchedTask) -> String {
    let configured = match t.key {
        "trae-renew" => Some(st.settings().jwt_renew_hhmm),
        "wb-growth" => Some(st.settings().wb_growth_hhmm),
        "wb-credits-snapshot" => Some(st.settings().wb_credits_sync_hhmm),
        "trae-credits-snapshot" => Some(st.settings().trae_credits_sync_hhmm),
        "wb-catalog-sync" => Some(st.settings().wb_catalog_sync_hhmm),
        "trae-models-sync" => Some(st.settings().trae_models_sync_hhmm),
        _ => None,
    };
    let Some(v) = configured else {
        return t.hhmm.to_string();
    };
    // 合法性：严格 HH:MM（小时 00–23，分钟 00–59），非法回退内置默认
    let ok = |s: &str| match s.split_once(':') {
        Some((h, m)) => {
            h.len() == 2
                && m.len() == 2
                && h.bytes().all(|c| c.is_ascii_digit())
                && m.bytes().all(|c| c.is_ascii_digit())
                && h.parse::<u32>().map(|x| x < 24).unwrap_or(false)
                && m.parse::<u32>().map(|x| x < 60).unwrap_or(false)
        }
        None => false,
    };
    let v = v.trim();
    if ok(v) {
        v.to_string()
    } else {
        t.hhmm.to_string()
    }
}

/// 任务启用判定：快照/巡检/续期类恒开（补齐数据时序），签到类跟随各自设置开关
fn enabled(st: &AppState, key: &str) -> bool {
    match key {
        // WorkBuddy 签到跟随「启动自动补签」开关（F-55 同源设置）
        "wb-checkin" => crate::commands::workbuddy::wb_auto_checkin_enabled(st),
        // Trae JWT 定时续期（issue #27）：跟随环境配置页开关（默认开）
        "trae-renew" => st.settings().jwt_renew_enabled,
        // WorkBuddy 每日成长（任务配置页）：跟随成长调度开关（默认开）
        "wb-growth" => st.settings().wb_growth_enabled,
        // 看板数据同步（积分/Token）：mode=off 即关闭（hourly/daily 均视为启用）
        "wb-credits-snapshot" | "trae-credits-snapshot" => credits_sync_mode(st, key) != "off",
        // 模型同步开关（默认开；无账号时任务内部静默跳过不计失败）
        "wb-catalog-sync" => st.settings().wb_catalog_sync_enabled,
        "trae-models-sync" => st.settings().trae_models_sync_enabled,
        // 其余任务幂等且低风险，恒开（Trae 签到 run_round 自带状态核验）
        _ => true,
    }
}

/// 执行单个任务（复用 CLI 任务同款实现，进度静默、结果汇总落日志）
fn run_task(key: &str, st: &AppState) -> Result<Value, String> {
    match key {
        // Trae 每日签到：与 `--task-run checkin` 同款（vault 全账号单轮，状态核验幂等）
        "trae-checkin" => {
            let accounts = crate::vault::load_accounts(st);
            let retry = st.settings().retry.max(0) as u32;
            Ok(super::trae_checkin::run_round(st, &accounts.accounts, retry, &mut |_| {}))
        }
        // WorkBuddy 每日签到：与 `--task-run wb-checkin` 同款；抢轮次锁与 UI 路径互斥
        "wb-checkin" => {
            let Ok(_round) = crate::commands::workbuddy::try_acquire_wb_round() else {
                return Err("跳过：已有签到/成长任务在执行中".into());
            };
            Ok(super::wb_checkin::run_checkin_round(st, &super::wb_checkin::CheckinOpts::daily(), &mut |_| {}))
        }
        // WorkBuddy 每日成长（任务配置页）：成长三开关驱动，抢轮次锁与 UI 路径互斥
        "wb-growth" => {
            let Ok(_round) = crate::commands::workbuddy::try_acquire_wb_round() else {
                return Err("跳过：已有签到/成长任务在执行中".into());
            };
            let s = crate::commands::workbuddy::load_settings(st);
            let flags = super::wb_checkin::GrowthOpts {
                travel: s.growth_travel,
                lottery: s.growth_lottery,
                tasks: s.growth_tasks,
            };
            // run_growth_round 为 NDJSON 推进式输出（无汇总返回值），调度日志记 ok 概要即可
            super::wb_checkin::run_growth_round(st, &flags, &[], &mut |_| {});
            Ok(json!({ "ok": true }))
        }
        // Trae JWT 定时续期（issue #27）：与 `--task-run trae-renew` 同款（lazy 48h 惰性门）
        "trae-renew" => crate::commands::accounts::renew_due_accounts_impl(st),
        // WorkBuddy token 兜底续期：与 `--task-run wb-renew` 同款（lazy 24h）
        "wb-renew" => Ok(super::wb_checkin::run_renew_only(st, 24)),
        // 豆包会话每日续期：与 `--task-run doubao-keepalive` 同款
        "doubao-keepalive" => {
            let sink = crate::switcher::CliSink::new(&st.data_dir);
            crate::switcher::run_action(
                crate::switcher::RunArgs {
                    action: crate::switcher::Action::KeepAlive,
                    target_app: crate::switcher::TargetApp::Doubao,
                    user_id: None,
                    proxy_port: None,
                    include_indexeddb: false,
                    expected_current_uid: String::new(),
                    data_dir: st.data_dir.clone(),
                },
                &sink,
            )
            .map(|_| json!({ "ok": true }))
        }
        // 豆包会员额度每日巡检：与 `--task-run doubao-quota` 同款
        "doubao-quota" => super::doubao_quota::run_batch(st),
        // WorkBuddy 积分与 Token 数据同步（看板定时同步）：积分 fresh 拉取+池回写+快照为主目标
        // （空池自然空转不报错）；顺带 Token 统计重扫（本地增量缓存）与官方请求用量刷新——
        // 两者尽力而为不上抛（页面打开时本就有各自缓存/降级链路）
        "wb-credits-snapshot" => {
            let parsed = crate::commands::workbuddy::wb_credits_snapshot_task(st)?;
            let token_files = crate::commands::workbuddy_stats::workbuddy_token_stats_impl(st, true)
                .get("files_scanned")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let usage = match crate::commands::workbuddy::workbuddy_usage_official_impl(st, None, true) {
                Ok(_) => "已刷新",
                Err(_) => "跳过（无可用凭证或拉取失败）",
            };
            Ok(json!({
                "ok": true,
                "accounts": parsed.get("accounts").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
                "token_files": token_files,
                "usage": usage,
            }))
        }
        // Trae 积分数据同步：与 `--task-run refresh-credits` 同款（无账号返回 refreshed=0）
        "trae-credits-snapshot" => crate::commands::accounts::refresh_remaining_credits_impl(st)
            .map(|n| json!({ "ok": true, "refreshed": n })),
        // Trae 模型列表同步：官网配置接口（batch_get_detail_param，不消耗积分）；
        // 无账号时静默跳过不计失败（对齐 models-sync 惯例）
        "trae-models-sync" => {
            let accounts = crate::vault::load_accounts(st);
            if accounts.accounts.is_empty() {
                Ok(json!({ "ok": true, "skipped": "无 Trae 账号" }))
            } else {
                crate::api_server::models_sync::fetch_official(&st.data_dir, accounts)
                    .map(|list| json!({ "ok": true, "models": list.len() }))
            }
        }
        // WorkBuddy 上游模型目录同步：取首个含凭证 WB 账号；无凭证账号静默跳过不计失败
        "wb-catalog-sync" => {
            if crate::commands::workbuddy::wb_upstream_accounts(st).is_empty() {
                Ok(json!({ "ok": true, "skipped": "无可用 WB 账号凭证" }))
            } else {
                crate::commands::api_server::wb_catalog_sync_impl(st)
                    .map(|n| json!({ "ok": true, "models": n }))
            }
        }
        other => Err(format!("未知调度任务: {other}")),
    }
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

/// 最近一次成功执行时间戳（hourly 模式节流用；mark_run 写入）
fn last_run_ts(st: &AppState, key: &str) -> Option<i64> {
    load_state(st)
        .pointer(&format!("/tasks/{key}/last_run_ts"))
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

/// 结果 JSON → 单行摘要（签到轮次取 ok/already/failed 计数，其余截断展示）
fn summarize(v: &Value) -> String {
    if v.is_object() {
        let get = |k: &str| v.get(k).and_then(Value::as_i64);
        if let (Some(o), Some(a), Some(f)) = (get("ok"), get("already"), get("failed")) {
            return format!("成功 {o}，已签 {a}，失败 {f}");
        }
    }
    let s = serde_json::to_string(v).unwrap_or_default();
    s.chars().take(120).collect()
}

/// 调度器状态查询（前端/排查用）：任务定义 + 最近一次执行情况
#[tauri::command]
pub fn scheduler_status(st: tauri::State<AppState>) -> Value {
    let raw = load_state(&st);
    let tasks: Vec<Value> = TASKS
        .iter()
        .map(|t| {
            let e = raw.pointer(&format!("/tasks/{}", t.key)).cloned().unwrap_or(Value::Null);
            // 展示时刻：hourly → 「每小时」；关闭 → 「已关闭」；其余 HH:MM
            let time = match sched_plan(&st, t) {
                SchedPlan::Hourly => "每小时".to_string(),
                SchedPlan::Daily(hhmm) => hhmm,
                SchedPlan::Skip => "已关闭".to_string(),
            };
            json!({
                "key": t.key,
                "name": t.name,
                "time": time,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state(tag: &str) -> AppState {
        let dir = std::env::temp_dir()
            .join(format!("aiwork_sched_renew_test_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("data"));
        AppState {
            data_dir: dir,
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        }
    }

    fn renew_task() -> &'static SchedTask {
        TASKS.iter().find(|t| t.key == "trae-renew").unwrap()
    }

    fn seed_settings(st: &AppState, enabled: bool, hhmm: &str) {
        crate::store::db(&st.data_dir)
            .kv_set("app_settings", &json!({ "jwt_renew_enabled": enabled, "jwt_renew_hhmm": hhmm }))
            .unwrap();
    }

    /// 成长调度配置种子（wb_growth_enabled/wb_growth_hhmm → app_settings kv）
    fn seed_growth_settings(st: &AppState, enabled: bool, hhmm: &str) {
        crate::store::db(&st.data_dir)
            .kv_set("app_settings", &json!({ "wb_growth_enabled": enabled, "wb_growth_hhmm": hhmm }))
            .unwrap();
    }

    /// 默认（无 kv）：零值回填链路保证「默认开 + 09:00」（issue #27 语义）
    #[test]
    fn effective_hhmm_default_0900_and_enabled() {
        let st = temp_state("dflt");
        assert_eq!(effective_hhmm(&st, renew_task()), "09:00");
        assert!(enabled(&st, "trae-renew"), "默认应启用续期任务");
    }

    /// 环境配置页可改：合法 HH:MM 覆盖内置默认
    #[test]
    fn effective_hhmm_follows_settings() {
        let st = temp_state("cfg");
        seed_settings(&st, true, "07:30");
        assert_eq!(effective_hhmm(&st, renew_task()), "07:30");
    }

    /// 非法配置回退内置默认：小时越界 / 非 HH:MM 格式 / 分钟越界 / 非法串 / 缺冒号
    #[test]
    fn effective_hhmm_invalid_falls_back() {
        for bad in ["24:00", "9:00", "07:60", "abc", "0730"] {
            let st = temp_state("bad");
            seed_settings(&st, true, bad);
            assert_eq!(effective_hhmm(&st, renew_task()), "09:00", "非法值 {bad:?} 应回退默认");
        }
    }

    /// 前后空白容忍：trim 后生效
    #[test]
    fn effective_hhmm_trims_whitespace() {
        let st = temp_state("trim");
        seed_settings(&st, true, " 08:15 ");
        assert_eq!(effective_hhmm(&st, renew_task()), "08:15");
    }

    /// 续期时刻配置不外溢：其他任务（wb-checkin）仍用内置 09:10
    #[test]
    fn other_tasks_ignore_renew_settings() {
        let st = temp_state("other");
        seed_settings(&st, true, "07:30");
        let wb = TASKS.iter().find(|t| t.key == "wb-checkin").unwrap();
        assert_eq!(effective_hhmm(&st, wb), "09:10");
    }

    /// WorkBuddy 每日成长：默认 09:00 + 默认开；合法覆盖生效；非法回退；显式关闭
    #[test]
    fn wb_growth_default_and_settings() {
        let st = temp_state("wbg");
        let growth = || TASKS.iter().find(|t| t.key == "wb-growth").unwrap();
        assert_eq!(effective_hhmm(&st, growth()), "09:00");
        assert!(enabled(&st, "wb-growth"), "成长调度默认启用");
        seed_growth_settings(&st, true, "08:20");
        assert_eq!(effective_hhmm(&st, growth()), "08:20");
        seed_growth_settings(&st, true, "25:00");
        assert_eq!(effective_hhmm(&st, growth()), "09:00", "非法时刻回退默认");
    }

    #[test]
    fn wb_growth_disabled_skipped() {
        let st = temp_state("wgoff");
        seed_growth_settings(&st, false, "09:00");
        assert!(!enabled(&st, "wb-growth"));
    }

    /// 显式关闭：enabled() 返回 false（环境配置页开关关闭后调度器跳过）
    #[test]
    fn enabled_false_when_disabled() {
        let st = temp_state("off");
        seed_settings(&st, false, "09:00");
        assert!(!enabled(&st, "trae-renew"));
    }

    /// 看板同步/模型同步的调度计划：默认 daily+内置时刻；hourly 生效；off 关闭；
    /// 模型同步默认开、时刻可改、关闭后 Skip
    #[test]
    fn sched_plan_credits_and_models() {
        let st = temp_state("plan");
        let wb_credits = || TASKS.iter().find(|t| t.key == "wb-credits-snapshot").unwrap();
        let trae_models = || TASKS.iter().find(|t| t.key == "trae-models-sync").unwrap();
        let set = |v: Value| crate::store::db(&st.data_dir).kv_set("app_settings", &v).unwrap();
        // 默认（无 kv）：daily + 内置时刻
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::Daily(h) if h == "23:30"));
        assert!(matches!(sched_plan(&st, trae_models()), SchedPlan::Daily(h) if h == "05:40"));
        // hourly 生效
        set(json!({ "wb_credits_sync_mode": "hourly" }));
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::Hourly));
        // off 关闭（enabled=false → Skip）
        set(json!({ "wb_credits_sync_mode": "off" }));
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::Skip));
        // 非法模式回退 daily（credits_sync_mode 兜底）
        set(json!({ "wb_credits_sync_mode": "weekly" }));
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::Daily(_)));
        // 模型同步时刻覆盖 + 显式关闭
        set(json!({ "trae_models_sync_hhmm": "06:10" }));
        assert!(matches!(sched_plan(&st, trae_models()), SchedPlan::Daily(h) if h == "06:10"));
        set(json!({ "trae_models_sync_enabled": false }));
        assert!(matches!(sched_plan(&st, trae_models()), SchedPlan::Skip));
    }
}
