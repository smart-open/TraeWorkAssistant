//! 应用内定时调度器（Web 版唯一任务触发通道；桌面 schtasks 链随桌面壳退役）。
//!
//! ## 语义（与桌面版一致）
//!
//! - 单后台线程 60s tick；每个任务有默认触发时刻（HH:MM）；
//! - 触发条件：`今天已过触发时刻 && 状态文件 last_run_date != 今天` → 执行；
//!   服务在触发时刻之后才启动也会补跑一次（启动补跑，等价每日一次语义）；
//! - 失败重试：仅成功才记 last_run_date；失败记 last_fail_ts + 连续失败计数
//!   （fail_streak），冷却 30 分钟起步、随连续失败指数退避 30→60→120 分钟封顶
//!  （issue #66 健壮性④：上游风控依赖故障时恒 30 分钟重试只会全天对上游连打
//!   ~28 轮，退避后收敛到 ~8 轮；成功一轮 mark_run 整体覆盖条目自然清零计数）；
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
    // WorkBuddy 上游模型目录同步（移植 main@c8e855b）：与 models-sync 对称（wb_catalog
    // fetch_and_replace 同款实现），默认开 05:45；无凭证账号时任务内部静默跳过不计失败
    SchedTask { key: "wb-catalog-sync", name: "WorkBuddy 模型目录同步", hhmm: "05:45" },
    SchedTask { key: "trae-checkin", name: "Trae 每日签到", hhmm: "09:00" },
    SchedTask { key: "wb-checkin", name: "WorkBuddy 每日签到", hhmm: "09:10" },
    // WorkBuddy 每日成长（issue #27 随 ad63bdc 移植）：成长三开关驱动（旅行/盲盒/任务），
    // 全关时空轮无副作用；启用/时刻经 scheduler_cfg 配置（默认开 + 09:00）
    SchedTask { key: "wb-growth", name: "WorkBuddy 每日成长", hhmm: "09:00" },
    // F-80 Qoder 每日签到（移植 main）：默认 10:15 单次覆盖「0 点签到」与
    // 「10:00 登录奖励」双活动；开关跟随 qoder_settings.auto_checkin（默认开），
    // 时刻经 scheduler_cfg.task_times 可改
    SchedTask { key: "qoder-checkin", name: "Qoder 每日签到", hhmm: "10:15" },
    // F-09 兜底续期：lazy 24h（到期前 24h 内才真正刷新），每天跑一次是安全超集
    SchedTask { key: "wb-renew", name: "WorkBuddy token 兜底续期", hhmm: "10:30" },
    // 快照类排到晚间（接近日末，差分口径最准）
    SchedTask { key: "wb-credits-snapshot", name: "WorkBuddy 积分余额每日快照", hhmm: "23:30" },
    SchedTask { key: "trae-credits-snapshot", name: "Trae 积分余额每日快照", hhmm: "23:40" },
    // Trae 消耗明细同步（issue #61 移植）：usage_history 此前只有前端手动拉取，
    // 页面常驻时快照停留在最后打开时刻。看板数据同步类任务（可 hourly）；daily
    // 模式用内置 23:50（不与积分同步共占配置时刻，天然错峰 10 分钟）
    SchedTask { key: "trae-usage-sync", name: "Trae 消耗明细同步", hhmm: "23:50" },
    // F-80 Qoder 积分快照（移植 main）：每日 HH:MM；空池静默空转不计失败
    SchedTask { key: "qoder-credits-snapshot", name: "Qoder 积分快照", hhmm: "23:40" },
    // F-80 Qoder 模型目录同步（移植 main p3-3）：真签名拉 model/list → adopt_remote
    // 替换缓存；空池/无凭证静默跳过不计失败，时刻经 scheduler_cfg.task_times 可改
    SchedTask { key: "qoder-catalog-sync", name: "Qoder 模型目录同步", hhmm: "05:50" },
    // F-80 Qoder 凭证 6h 兜底刷新（移植 main）：固定每 6 小时（hhmm 不参与判定）；
    // 空池空转；与 UI 路径经跨进程锁互斥（任务内部处理）
    SchedTask { key: "qoder-refresh", name: "Qoder 凭证定时刷新", hhmm: "06:00" },
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

/// 失败重试冷却基数：失败后按连续失败次数指数退避（30 分钟起步 ×2 封顶 120 分钟）
const RETRY_COOLDOWN_MS: i64 = 30 * 60_000;

/// hourly 模式节流：距上次成功执行 ≥1h 才再跑（失败走退避冷却，不受此门限制）
const HOURLY_INTERVAL_MS: i64 = 60 * 60_000;

/// qoder-refresh 档位（移植 main）：每 6 小时兜底刷新一次凭证（设计 v1.3 §M4；
/// 客户端 token 惰性窗 7h > 6h 调度间隔，任一 tick 必落窗内，确保过期令牌被续）
const REFRESH_INTERVAL_HOURS: i64 = 6;

/// 可配置「每小时」模式的任务（看板数据同步类，移植 main@c8e855b 的 credits 语义）：
/// 仅这些任务接受 scheduler_cfg.task_modes 的 hourly 值，其余任务恒为每日模式
const HOURLY_CAPABLE: &[&str] = &["wb-credits-snapshot", "trae-credits-snapshot", "trae-usage-sync"];

/// 单任务的生效调度计划：Skip=关闭；Daily=每日 HH:MM（到点+当日未跑，启动补跑）；
/// Hourly=每小时（距上次成功执行 ≥1h，无记录=首次立即跑）；
/// EveryHours(h)=每 h 小时（距上次成功执行 ≥h·1h，无记录=首次立即跑）
#[derive(Debug)]
enum SchedPlan {
    Skip,
    Daily(String),
    Hourly,
    EveryHours(i64),
}

/// 单任务调度计划：合并启用判定与模式/时刻解析（tick 与 scheduler_status 共用；
/// cfg 为整轮一次性读出的 scheduler_cfg，避免每任务重复读 db；
/// extra_enabled 为外部设置语义门（wb-checkin → 启动自动补签开关 F-55、
/// qoder-checkin → qoder_settings.auto_checkin，其余任务恒 true）
fn sched_plan(cfg: &Value, t: &SchedTask, extra_enabled: bool) -> SchedPlan {
    if !enabled_from(cfg, t.key) || !extra_enabled {
        return SchedPlan::Skip;
    }
    // Qoder 凭证兜底刷新（移植 main）：固定每 6 小时，hhmm 不参与判定
    if t.key == "qoder-refresh" {
        return SchedPlan::EveryHours(REFRESH_INTERVAL_HOURS);
    }
    if HOURLY_CAPABLE.contains(&t.key) && effective_task_mode_from(cfg, t.key) == "hourly" {
        SchedPlan::Hourly
    } else {
        SchedPlan::Daily(effective_hhmm_from(cfg, t))
    }
}

/// 看板数据任务的缺省执行模式（task_modes 缺省/非法值兜底，对齐 main
/// credits_sync_mode 语义——issue #61：Trae 看板数据默认 hourly 保常驻页面
/// 小时级新鲜度；Buddy 维持每日）
fn fallback_task_mode(key: &str) -> &'static str {
    match key {
        "wb-credits-snapshot" => "daily",
        // trae-credits-snapshot / trae-usage-sync
        _ => "hourly",
    }
}

/// 任务生效执行模式（HOURLY_CAPABLE 任务用）：scheduler_cfg.task_modes 显式
/// 配置优先（hourly/daily），缺省或非法值回落 fallback_task_mode
fn effective_task_mode_from(cfg: &Value, key: &str) -> String {
    match task_modes_from(cfg).get(key).map(String::as_str) {
        Some(m @ ("hourly" | "daily")) => m.to_string(),
        _ => fallback_task_mode(key).to_string(),
    }
}

/// 任务外部设置语义门（整轮读一次）：wb-checkin 联动「启动自动补签」开关（F-55），
/// qoder-checkin 联动 Qoder 自动签到开关（移植 main），其余任务恒 true
fn extra_enabled(st: &AppState, key: &str) -> bool {
    match key {
        "wb-checkin" => crate::commands::workbuddy::wb_auto_checkin_enabled(st),
        "qoder-checkin" => crate::commands::qoder::qoder_auto_checkin_enabled(st),
        _ => true,
    }
}

/// 看板数据任务 → 平台 key 映射（emit `board-data-synced` 载荷，前端 Dashboard
/// 按 platform 匹配后才静默重读缓存；issue #61 前端联动侧）
fn board_platform(key: &str) -> Option<&'static str> {
    match key {
        "wb-credits-snapshot" => Some("buddy"),
        "trae-credits-snapshot" | "trae-usage-sync" => Some("trae"),
        // F-80 Qoder 看板与 trae/buddy 同款联动：快照任务写入 qoder_credits_cache
        // 与当日历史后，常驻页面需静默重读（审查修复：此前缺映射致 Qoder 看板滞后）
        "qoder-credits-snapshot" => Some("qoder"),
        _ => None,
    }
}

/// 单轮 tick：检查每个任务「今天已过触发时刻 && 今天未跑」→ 执行
fn tick(st: &AppState) {
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let now_hm = now.format("%H:%M").to_string();
    // T11：通知配置整轮读一次（签到成功/任务失败推送，内部再按总开关静默）
    let notify_cfg = crate::notify::load_config(st);
    // 调度配置与执行状态整轮各读一次（每 60s tick，避免每任务重复读 db）
    let cfg = load_cfg(st);
    let state = load_state(st);
    for t in TASKS {
        // 触发判定（trigger 用于日志展示：每日 HH:MM 或「每小时」）
        let trigger = match sched_plan(&cfg, t, extra_enabled(st, t.key)) {
            SchedPlan::Skip => continue,
            SchedPlan::Daily(hhmm) => {
                if last_run_date(&state, t.key).as_deref() == Some(today.as_str()) {
                    continue;
                }
                // HH:MM 零填充，字符串比较即时间序
                if now_hm.as_str() < hhmm.as_str() {
                    continue;
                }
                format!("每日 {hhmm}")
            }
            SchedPlan::Hourly => {
                // hourly 按小时节流：仅成功执行记 last_run_ts（mark_run）
                if let Some(ts) = last_run_ts(&state, t.key) {
                    if chrono::Utc::now().timestamp_millis() - ts < HOURLY_INTERVAL_MS {
                        continue;
                    }
                }
                "每小时".to_string()
            }
            SchedPlan::EveryHours(hours) => {
                // EveryHours 按档位节流：距上次成功执行 ≥h·1h 才再跑（无记录=首次立即跑）
                if let Some(ts) = last_run_ts(&state, t.key) {
                    if chrono::Utc::now().timestamp_millis() - ts < hours * HOURLY_INTERVAL_MS {
                        continue;
                    }
                }
                format!("每{hours}小时")
            }
        };
        // 失败冷却：退避窗口内静默等待重试，不重复执行也不刷日志
        //（retry_cooldown_ms 与 last_fail_ts 同取整轮快照，零额外 IO）
        if let Some(ts) = last_fail_ts(&state, t.key) {
            if chrono::Utc::now().timestamp_millis() - ts < retry_cooldown_ms(&state, t.key) {
                continue;
            }
        }
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_task(t.key, st)
        })) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("任务线程 panic（已捕获，不影响后续调度）".to_string()),
        };
        match outcome {
            Ok(v) => {
                let summary = summarize(&v);
                // skipped_busy（移植 main 审查修复：qoder-checkin 轮次锁被 UI 路径持有的
                // 幂等跳过）不 mark_run——跳过≠完成，mark_run 会固化「当日已跑」关闭当日
                // 重试链，UI 轮次的失败账号将失去当日调度兜底。其余 skipped（空池静默
                // 空转）仍照常记账，防 Hourly/EveryHours 任务每 tick 空转刷日志
                if v.get("skipped_busy").is_some() {
                    fs_utils::app_log(
                        &st.data_dir,
                        &format!(
                            "[调度器] {}（{trigger}）：{}（不记当日已跑，稍后重试）",
                            t.name, summary
                        ),
                    );
                } else {
                    mark_run(st, t.key, &today, &summary);
                    fs_utils::app_log(
                        &st.data_dir,
                        &format!("[调度器] {}（{trigger}）：{}", t.name, summary),
                    );
                    // 看板数据同步成功 → 通知前端重读缓存（issue #61 前端联动：
                    // Dashboard listen 后按 platform 匹配静默 refresh(false)，零额外网络）
                    if let Some(p) = board_platform(t.key) {
                        st.emit_event("board-data-synced", json!({ "platform": p, "task": t.key }));
                    }
                    // T11：仅签到类任务完成时推送（快照/续期类静默，避免每日刷屏）
                    if notify_cfg.on_checkin_done
                        && matches!(t.key, "trae-checkin" | "wb-checkin" | "qoder-checkin")
                    {
                        crate::notify::send(st, &format!("{}完成", t.name), &summary, "checkin");
                    }
                }
            }
            Err(summary) => {
                mark_fail(st, t.key, &summary);
                // 退避窗口按实际分钟数展示（连续失败 30→60→120 分钟封顶）；
                // 重载状态读取刚写入的 fail_streak（tick 顶部快照不含本次失败）
                let cooldown_min =
                    (retry_cooldown_ms(&load_state(st), t.key) / 60_000).max(1);
                fs_utils::app_log(
                    &st.data_dir,
                    &format!(
                        "[调度器] {}（{trigger}）失败，{cooldown_min} 分钟后重试：{}",
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

/// 任务启用判定（用户开关部分）：kv `scheduler_cfg.disabled_tasks`，默认全开 = 推荐配置；
/// 外部设置语义门（wb-checkin → 启动自动补签）由 extra_enabled 单独叠加
fn enabled_from(cfg: &Value, key: &str) -> bool {
    !disabled_tasks_from(cfg).iter().any(|k| k == key)
}

// ── 任务配置（kv `scheduler_cfg`）────────────────────────────────────────────
// 形态：{ "disabled_tasks": ["trae-checkin", ...], "task_times": { "trae-checkin": "08:30", ... },
//        "task_modes": { "wb-credits-snapshot": "hourly", ... } }
// 缺省 = 全部启用 + 各任务默认时刻 + 全部每日模式（推荐配置）。前端在 Trae / Buddy 环境配置页展示。

fn disabled_tasks(st: &AppState) -> Vec<String> {
    disabled_tasks_from(&load_cfg(st))
}

fn disabled_tasks_from(cfg: &Value) -> Vec<String> {
    cfg.get("disabled_tasks")
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
    task_times_from(&load_cfg(st))
}

fn task_times_from(cfg: &Value) -> std::collections::HashMap<String, String> {
    cfg.get("task_times")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// 任务执行模式表（key → "hourly"；缺省 = 每日）：scheduler_cfg.task_modes，
/// 仅 HOURLY_CAPABLE 内的任务生效（移植 main@c8e855b 看板数据同步 hourly 语义）。
/// 看板数据任务缺省模式显式回填（issue #61：Trae 默认 hourly / Buddy 默认 daily，
/// 对齐 main credits_sync_mode 兜底语义；前端展示与实际调度同源）
fn task_modes(st: &AppState) -> std::collections::HashMap<String, String> {
    let mut m = task_modes_from(&load_cfg(st));
    for k in HOURLY_CAPABLE {
        m.entry((*k).to_string())
            .or_insert_with(|| fallback_task_mode(k).to_string());
    }
    m
}

fn task_modes_from(cfg: &Value) -> std::collections::HashMap<String, String> {
    cfg.get("task_modes")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// 任务生效触发时刻：task_times 自定义覆盖 > 内置默认（HH:MM 零填充，字符串比较即时间序）
fn effective_hhmm_from(cfg: &Value, t: &SchedTask) -> String {
    task_times_from(cfg)
        .get(t.key)
        .cloned()
        .unwrap_or_else(|| t.hhmm.to_string())
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
        "task_modes": task_modes(st),
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
    // 执行模式（移植 main@c8e855b）：仅 hourly/daily 两值，且仅看板同步类任务可 hourly；
    // 整表替换（空对象 = 全部回每日）。非 hourly-capable 键一律拒绝，防前端误传。
    let mut modes: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(m) = cfg.get("task_modes") {
        let obj = m.as_object().ok_or("task_modes 应为对象")?;
        for (k, v) in obj {
            if !HOURLY_CAPABLE.contains(&k.as_str()) {
                return Err(format!("任务 {k} 不支持配置执行模式"));
            }
            let s = v.as_str().ok_or_else(|| format!("任务 {k} 的模式应为 \"daily\" 或 \"hourly\""))?;
            if s != "daily" && s != "hourly" {
                return Err(format!("任务 {k} 的模式 {s} 非法（应为 daily/hourly）"));
            }
            modes.insert(k.clone(), s.to_string());
        }
    }
    let _ = crate::store::db(&st.data_dir).kv_set(
        "scheduler_cfg",
        &json!({ "disabled_tasks": disabled, "task_times": times, "task_modes": modes }),
    );
    fs_utils::app_log(
        &st.data_dir,
        &format!(
            "[调度器] 任务配置已更新：{} 项停用，{} 项自定义时刻，{} 项每小时模式",
            disabled.len(),
            times.len(),
            modes.iter().filter(|(_, v)| v.as_str() == "hourly").count()
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
        // Trae 每日签到：与 `--task-run checkin` 同款（vault 全账号单轮，状态核验幂等）。
        // 审查 P2（issue #61 批次）：存在可重试失败时返 Err，交调度器 30 分钟冷却重试
        // （对齐 qoder-checkin）；永久性失败（未配置 jwt，重试注定无望）从重试判定剔除
        "trae-checkin" => {
            let accts = crate::vault::load_accounts(st);
            let retry = st.settings().retry.max(0) as u32;
            let mut events: Vec<Value> = Vec::new();
            let done = crate::tasks::trae_checkin::run_round(
                st,
                &accts.accounts,
                retry,
                &mut |ev| events.push(ev.clone()),
            );
            let failed = done.get("failed").and_then(Value::as_i64).unwrap_or(0);
            let failed_permanent = events
                .iter()
                .filter(|e| {
                    e.get("status").and_then(Value::as_str) == Some("fail")
                        && e.get("message").and_then(Value::as_str) == Some("未配置 jwt")
                })
                .count() as i64;
            let retryable = (failed - failed_permanent).max(0);
            if retryable > 0 {
                let ok = done.get("ok").and_then(Value::as_i64).unwrap_or(0);
                let already = done.get("already").and_then(Value::as_i64).unwrap_or(0);
                return Err(format!(
                    "Trae 签到：{ok} 成功 / {already} 已签 / {retryable} 失败（稍后自动重试）"
                ));
            }
            Ok(done)
        }
        // WorkBuddy 每日签到：与 `--task-run wb-checkin` 同款；抢轮次锁与 UI 路径互斥。
        // 审查 P2（issue #61 批次）：同 trae-checkin——可重试失败返 Err 交 30 分钟冷却重试；
        // 永久性凭证失败（需重新登录 / 无可用凭证，30 分钟内不会自愈）剔除出重试判定
        "wb-checkin" => {
            let Ok(_round) = crate::commands::workbuddy::try_acquire_wb_round() else {
                return Err("跳过：已有签到/成长任务在执行中".into());
            };
            let mut events: Vec<Value> = Vec::new();
            let done = crate::tasks::wb_checkin::run_checkin_round(
                st,
                &crate::tasks::wb_checkin::CheckinOpts::daily(),
                &mut |ev| events.push(ev.clone()),
            );
            let failed = done.get("failed").and_then(Value::as_i64).unwrap_or(0);
            let failed_permanent = events
                .iter()
                .filter(|e| {
                    let m = e.get("message").and_then(Value::as_str).unwrap_or("");
                    e.get("status").and_then(Value::as_str) == Some("fail")
                        && (m.contains("需重新登录") || m.contains("无可用凭证"))
                })
                .count() as i64;
            let retryable = (failed - failed_permanent).max(0);
            if retryable > 0 {
                let ok = done.get("ok").and_then(Value::as_i64).unwrap_or(0);
                let already = done.get("already").and_then(Value::as_i64).unwrap_or(0);
                return Err(format!(
                    "WB 签到：{ok} 成功 / {already} 已签 / {retryable} 失败（稍后自动重试）"
                ));
            }
            Ok(done)
        }
        // WorkBuddy 每日成长：成长三开关驱动，抢轮次锁与 UI 路径互斥
        "wb-growth" => {
            let Ok(_round) = crate::commands::workbuddy::try_acquire_wb_round() else {
                return Err("跳过：已有签到/成长任务在执行中".into());
            };
            let s = crate::commands::workbuddy::load_settings(st);
            let flags = crate::tasks::wb_checkin::GrowthOpts {
                travel: s.growth_travel,
                lottery: s.growth_lottery,
                tasks: s.growth_tasks,
            };
            // run_growth_round 为 NDJSON 推进式输出（无汇总返回值），调度日志记 ok 概要即可
            crate::tasks::wb_checkin::run_growth_round(st, &flags, &[], &mut |_| {});
            Ok(json!({ "ok": true }))
        }
        // WorkBuddy token 兜底续期：与 `--task-run wb-renew` 同款（lazy 24h）
        "wb-renew" => Ok(crate::tasks::wb_checkin::run_renew_only(st, 24)),
        // WorkBuddy 积分余额每日快照：补齐近 7 日消耗差分时序；顺带刷新官方请求用量
        //（issue #61 批次：用量刷全账号聚合缓存 workbuddy_usage_official_all_cache，
        // force 跳过 10min 缓存；原单账号缓存分支前端无调用方，属刷错目标一并修复）
        "wb-credits-snapshot" => {
            let parsed = crate::commands::workbuddy::wb_credits_snapshot_task(st)?;
            let usage =
                match crate::commands::workbuddy::workbuddy_usage_official_all_impl(st, true) {
                    Ok(_) => "已刷新",
                    Err(_) => "跳过（无可用凭证或拉取失败）",
                };
            Ok(json!({
                "ok": true,
                "accounts": parsed.get("accounts").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
                "usage": usage,
            }))
        }
        // WorkBuddy 上游模型目录同步：取首个含凭证 WB 账号；无凭证账号静默跳过不计失败
        "wb-catalog-sync" => {
            if crate::api_server::runtime::wb_upstream_accounts(st).is_empty() {
                Ok(json!({ "ok": true, "skipped": "无可用 WB 账号凭证" }))
            } else {
                crate::commands::api_server::wb_catalog_sync_impl(st)
                    .map(|n| json!({ "ok": true, "models": n }))
            }
        }
        // Trae 积分余额每日快照：与 `--task-run refresh-credits` 同款（无账号返回 refreshed=0）；
        // 网关运行中同步回写池内积分快照（issue #67：只写库不回写池会导致秃号内存
        // credits>0 永远 selectable）——server 单体网关常驻，回写在 impl 内部经
        // gateway_shared 判定完成，无需句柄传参（main@36d628f 的 runtime 参数形态
        // 随桌面壳退役，docker 适配版）
        "trae-credits-snapshot" => accounts::refresh_remaining_credits_impl(st)
            .map(|n| json!({ "ok": true, "refreshed": n })),
        // Trae 消耗明细同步（issue #61 移植）：与 `--task-run trae-usage-sync` 同款
        //（fresh=true 增量拉取）；无账号静默跳过（对齐 models-sync 惯例）；
        // 全部账号拉取失败返 Err 交 30 分钟冷却重试
        "trae-usage-sync" => {
            let accounts_v = crate::vault::load_accounts(st);
            if accounts_v.accounts.is_empty() {
                Ok(json!({ "ok": true, "skipped": "无 Trae 账号" }))
            } else {
                crate::commands::usage_history::usage_history_fetch_impl(st, true)
                    .map(|r| json!({ "ok": true, "accounts": r.accounts.len() }))
            }
        }
        // F-80 Qoder 每日签到：与 `--task-run qoder-checkin` 同款；抢轮次锁与 UI 路径互斥
        "qoder-checkin" => {
            // 抢不到轮次锁（UI 路径正在签到）= 幂等跳过而非失败：返 Err 会被调度器记
            // 当日首败并推送「签到失败」误报通知（签到本身未失败，UI 路径会照常完成）
            let Ok(_round) = crate::tasks::qoder_checkin::try_acquire_qoder_round() else {
                // skipped_busy：tick 据此跳过 mark_run（保留当日后续 tick 重试机会）
                return Ok(json!({
                    "ok": true,
                    "skipped": "已有 Qoder 签到任务在执行中，本轮跳过",
                    "skipped_busy": true,
                }));
            };
            let opts = crate::tasks::qoder_checkin::QoderCheckinOpts::daily();
            let done = crate::tasks::qoder_checkin::run_checkin_round(st, &opts, &mut |_| {});
            // 审查 M-1：存在失败账号时返 Err，交调度器 30 分钟冷却重试（暂态失败自愈）。
            // P2 重试口径：empty_campaigns（活动未上线/不可用）属非用户可操作失败，
            // 计入重试只会全天无效重试 + 当日首败误报通知——按 failed - failed_empty_campaigns
            // > 0 判定。done 事件恒携带该字段；缺字段时 as_i64 为 None → 0，安全兼容
            let failed = done.get("failed").and_then(Value::as_i64).unwrap_or(0);
            let failed_empty = done
                .get("failed_empty_campaigns")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            // 审查 minor：永久性认证失败（pat_rejected/expired_needs_relogin/auth_dead）
            // 重试注定失败，与 empty_campaigns 一并从重试判定剔除——否则失效账号
            // 会拖动整轮全天约 28 次冷却重试（含每轮必败的刷新请求）
            let failed_permanent = done
                .get("failed_permanent")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if failed - failed_empty - failed_permanent > 0 {
                let ok = done.get("ok").and_then(Value::as_i64).unwrap_or(0);
                let already = done.get("already").and_then(Value::as_i64).unwrap_or(0);
                return Err(format!(
                    "Qoder 签到：{ok} 成功 / {already} 已领 / {failed} 失败（稍后自动重试）"
                ));
            }
            Ok(done)
        }
        // F-80 Qoder 积分快照：与 `--task-run qoder-credits-snapshot` 同款（空池空转）
        "qoder-credits-snapshot" => crate::tasks::qoder_credits::run_snapshot_task(st),
        // Qoder Token 定时刷新：与 `--task-run qoder-refresh` 同款（6h 周期，lazy 7h 惰性门；
        // 有账号刷新失败时返 Err，交由调度器 30 分钟冷却重试；抢不到跨进程锁返回 skipped_busy）
        "qoder-refresh" => crate::tasks::qoder_refresh::run_task(st),
        // Qoder 模型目录同步：与 `--task-run qoder-catalog-sync` 同款（真签名拉
        // model/list → adopt_remote；空池/无凭证空转，网络失败返 Err 冷却重试）
        "qoder-catalog-sync" => crate::tasks::qoder_catalog::run_task(st),
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

/// Trae JWT 自动续期（issue #27 方案 A 编排）：委托 `renew_due_accounts_impl`
///（lazy 48h 惰性门前置预筛 + 全失败返回 Err 触发调度器 30 分钟重试；
/// invalid 账号由 impl 入口拦截计 skipped，无重试风暴）。此处仅映射计数 JSON
/// 为调度摘要（summarize 优先取 summary 字段）。
fn run_trae_jwt_renew(st: &AppState) -> Result<Value, String> {
    let v = accounts::renew_due_accounts_impl(st)?;
    let get = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    Ok(json!({
        "summary": format!(
            "续期 {}，跳过 {}，无凭据 {}，失败 {}",
            get("refreshed"),
            get("skipped"),
            get("no_refresh_token"),
            get("failed"),
        ),
        "renewed": get("refreshed"),
        "skipped": get("skipped"),
        "no_refresh_token": get("no_refresh_token"),
        "failed": get("failed"),
    }))
}

// ── 状态持久化 ───────────────────────────────────────────────────────────────

fn load_state(st: &AppState) -> Value {
    // SQLite 化（P2）：scheduler_state.json → kv `scheduler_state`
    let v: Value = crate::store::db(&st.data_dir).kv_get("scheduler_state");
    if v.is_object() { v } else { json!({}) }
}

fn last_run_date(state: &Value, key: &str) -> Option<String> {
    state
        .pointer(&format!("/tasks/{key}/last_run_date"))
        .and_then(Value::as_str)
        .map(String::from)
}

fn last_fail_ts(state: &Value, key: &str) -> Option<i64> {
    state
        .pointer(&format!("/tasks/{key}/last_fail_ts"))
        .and_then(Value::as_i64)
}

/// 退避冷却时长：cooldown = 30 分钟 << min(streak-1, 2)，连续失败 30→60→120 分钟封顶
/// （streak=0/缺字段兼容旧条目回 30 分钟；mark_run 成功整体覆盖条目自然清零计数）
/// shift 用显式分支求值：saturating_sub(1).min(2) 在 streak=0 时得 -1（0-1=-1 是
/// 合法语义，min 只夹上界不夹下界），-1 as u32 = u32::MAX 触发移位溢出 panic——
/// 显式比较同时夹上下界，语义等价且边界自明。
/// 入参取状态快照（与 last_fail_ts/last_run_ts 同风格）：tick 整轮读一次零额外 IO；
/// 失败分支展示分钟数时需先重载（load_state）拿到刚写入的 fail_streak
fn retry_cooldown_ms(state: &Value, key: &str) -> i64 {
    let streak = state
        .pointer(&format!("/tasks/{key}/fail_streak"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let streak = if streak < 0 { 0 } else { streak };
    let shift: u32 = if streak <= 1 { 0 } else if streak >= 3 { 2 } else { 1 };
    RETRY_COOLDOWN_MS << shift
}

/// 最近一次成功执行时间戳（hourly 模式节流用；mark_run 写入）
fn last_run_ts(state: &Value, key: &str) -> Option<i64> {
    state
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

/// 失败：记失败时间戳 + 连续失败计数（退避用），不写 last_run_date（冷却后当天自动重试）。
/// 距上次失败 ≥24h 视为跨天新鲜失败，streak 重置为 1（昨日残留不抬高今日首败冷却）
fn mark_fail(st: &AppState, key: &str, summary: &str) {
    let state = load_state(st);
    let prev_ts = state
        .pointer(&format!("/tasks/{key}/last_fail_ts"))
        .and_then(Value::as_i64);
    let prev_streak = state
        .pointer(&format!("/tasks/{key}/fail_streak"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let streak = if prev_ts
        .map(|ts| chrono::Utc::now().timestamp_millis() - ts >= 24 * 3_600_000)
        .unwrap_or(true)
    {
        1
    } else {
        prev_streak + 1
    };
    write_entry(
        st,
        key,
        json!({
            "last_fail_ts": chrono::Utc::now().timestamp_millis(),
            "fail_streak": streak,
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
    let cfg = load_cfg(st);
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
                // 展示时刻：hourly → 「每小时」；EveryHours(h) → 「每h小时」；其余 HH:MM
                "time": match sched_plan(&cfg, t, extra_enabled(st, t.key)) {
                    SchedPlan::Hourly => "每小时".to_string(),
                    SchedPlan::EveryHours(h) => format!("每{h}小时"),
                    SchedPlan::Daily(hhmm) => hhmm,
                    SchedPlan::Skip => "已关闭".to_string(),
                },
                // 执行模式（daily/hourly/every6h/off）：前端据此切换时刻输入与徽标
                "mode": match sched_plan(&cfg, t, extra_enabled(st, t.key)) {
                    SchedPlan::Hourly => "hourly",
                    SchedPlan::EveryHours(_) => "every6h",
                    SchedPlan::Daily(_) => "daily",
                    SchedPlan::Skip => "off",
                },
                "enabled": enabled_from(&cfg, t.key) && extra_enabled(st, t.key),
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

// ==================== 单元测试：调度计划纯逻辑（移植 main@c8e855b 时补充） ====================

#[cfg(test)]
mod sched_plan_tests {
    use super::*;

    fn task(key: &str) -> &'static SchedTask {
        TASKS.iter().find(|t| t.key == key).unwrap()
    }

    /// hourly 模式仅对 HOURLY_CAPABLE 任务生效：误配到其他任务按每日处理
    #[test]
    fn hourly_mode_only_for_capable_tasks() {
        let cfg = json!({ "task_modes": { "wb-credits-snapshot": "hourly", "wb-checkin": "hourly" } });
        assert!(matches!(
            sched_plan(&cfg, task("wb-credits-snapshot"), true),
            SchedPlan::Hourly
        ));
        assert!(matches!(sched_plan(&cfg, task("wb-checkin"), true), SchedPlan::Daily(_)));
    }

    /// 停用名单与外部设置门（wb-checkin → 启动自动补签）任一不满足即 Skip
    #[test]
    fn disabled_or_external_gate_skips() {
        let cfg = json!({ "disabled_tasks": ["trae-credits-snapshot"] });
        assert!(matches!(
            sched_plan(&cfg, task("trae-credits-snapshot"), true),
            SchedPlan::Skip
        ));
        assert!(matches!(
            sched_plan(&json!({}), task("wb-checkin"), false),
            SchedPlan::Skip
        ));
    }

    /// 自定义时刻覆盖默认；缺省回落内置默认时刻
    #[test]
    fn custom_time_overrides_default() {
        let cfg = json!({ "task_times": { "models-sync": "08:15" } });
        let t = task("models-sync");
        match sched_plan(&cfg, t, true) {
            SchedPlan::Daily(hhmm) => assert_eq!(hhmm, "08:15"),
            _ => panic!("models-sync 应为每日计划"),
        }
        match sched_plan(&json!({}), t, true) {
            SchedPlan::Daily(hhmm) => assert_eq!(hhmm, t.hhmm),
            _ => panic!("models-sync 应为每日计划"),
        }
    }

    /// qoder-refresh 固定每 6 小时（移植 main）：不受 task_times/task_modes 影响，
    /// 时刻配置不参与判定；停用时仍按 Skip 处理
    #[test]
    fn qoder_refresh_is_every6h() {
        let cfg = json!({
            "task_times": { "qoder-refresh": "12:00" },
            "task_modes": { "qoder-refresh": "hourly" },
        });
        match sched_plan(&cfg, task("qoder-refresh"), true) {
            SchedPlan::EveryHours(h) => assert_eq!(h, REFRESH_INTERVAL_HOURS),
            other => panic!("qoder-refresh 应为 EveryHours 计划，实际 {other:?}"),
        }
        assert!(matches!(
            sched_plan(&json!({ "disabled_tasks": ["qoder-refresh"] }), task("qoder-refresh"), true),
            SchedPlan::Skip
        ));
    }

    /// issue #61：Trae 看板数据同步默认 hourly（task_modes 缺省即生效，无需显式
    /// 配置）；wb 维持每日。显式 daily 可覆盖回每日（23:40/23:50 内置错峰），
    /// 非法模式值回落缺省口径
    #[test]
    fn trae_board_tasks_default_hourly() {
        assert!(matches!(
            sched_plan(&json!({}), task("trae-credits-snapshot"), true),
            SchedPlan::Hourly
        ));
        assert!(matches!(
            sched_plan(&json!({}), task("trae-usage-sync"), true),
            SchedPlan::Hourly
        ));
        // 显式 daily 覆盖：回落每日计划并用内置时刻（23:50，与积分同步 23:40 错峰）
        let daily = json!({ "task_modes": { "trae-usage-sync": "daily" } });
        match sched_plan(&daily, task("trae-usage-sync"), true) {
            SchedPlan::Daily(hhmm) => assert_eq!(hhmm, "23:50"),
            other => panic!("daily 覆盖应为每日计划，实际 {other:?}"),
        }
        // 非法模式值：回落缺省（trae → hourly）
        let bogus = json!({ "task_modes": { "trae-usage-sync": "weekly" } });
        assert!(matches!(
            sched_plan(&bogus, task("trae-usage-sync"), true),
            SchedPlan::Hourly
        ));
        // wb 缺省仍为每日
        assert!(matches!(
            sched_plan(&json!({}), task("wb-credits-snapshot"), true),
            SchedPlan::Daily(_)
        ));
    }

    /// board_platform 映射（emit `board-data-synced` 载荷）：仅看板数据任务有
    /// 平台 key，签到/续期/目录同步等非看板任务返回 None（不触发前端联动）
    #[test]
    fn board_platform_maps_board_tasks_only() {
        assert_eq!(board_platform("wb-credits-snapshot"), Some("buddy"));
        assert_eq!(board_platform("trae-credits-snapshot"), Some("trae"));
        assert_eq!(board_platform("trae-usage-sync"), Some("trae"));
        assert_eq!(board_platform("qoder-credits-snapshot"), Some("qoder"));
        assert_eq!(board_platform("qoder-refresh"), None);
        assert_eq!(board_platform("qoder-checkin"), None);
        assert_eq!(board_platform("trae-checkin"), None);
    }
}

// ==================== 单元测试：失败指数退避（移植 main@641376b，issue #66） ====================

#[cfg(test)]
mod sched_backoff_tests {
    use super::*;

    /// 测试专用 AppState：临时目录 + 全新锁（与 wb_credits 测试同款构造）
    fn temp_state(tag: &str) -> AppState {
        let dir = std::env::temp_dir().join(format!("aiwork_sched_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        AppState {
            data_dir: dir,
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            events: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// 失败退避（issue #66 健壮性④）：无记录/首败 30 分钟起步，连续失败 ×2
    /// 30→60→120 封顶；负值 streak 兜底按 0；mark_run 成功整体覆盖条目清零计数
    #[test]
    fn retry_cooldown_backoff_and_streak_reset() {
        let st = temp_state("backoff");
        let key = "qoder-checkin";
        // 无记录：30 分钟起步
        assert_eq!(retry_cooldown_ms(&load_state(&st), key), 30 * 60_000);
        // 连续失败 1/2/3/4 次：30/60/120/120（封顶）
        let mut total = 0;
        for (n, expect_min) in [(1, 30), (2, 60), (3, 120), (4, 120)] {
            while total < n {
                mark_fail(&st, key, "模拟失败");
                total += 1;
            }
            assert_eq!(
                retry_cooldown_ms(&load_state(&st), key),
                expect_min * 60_000,
                "连续失败 {n} 次后冷却应为 {expect_min} 分钟"
            );
        }
        // streak 确实持久化到状态
        assert_eq!(
            load_state(&st)
                .pointer(&format!("/tasks/{key}/fail_streak"))
                .and_then(Value::as_i64),
            Some(4)
        );
        // 成功 mark_run 整体覆盖条目 → fail_streak 消失，冷却回 30 分钟
        mark_run(&st, key, "2026-10-07", "成功 1，已签 0，失败 0");
        assert_eq!(retry_cooldown_ms(&load_state(&st), key), 30 * 60_000);
        assert_eq!(
            load_state(&st)
                .pointer(&format!("/tasks/{key}/last_fail_ts"))
                .and_then(Value::as_i64),
            None,
            "mark_run 应清掉 last_fail_ts"
        );
        // 手写负值 streak（脏数据）：兜底按 0 → 30 分钟
        let mut root = load_state(&st);
        root["tasks"][key]["fail_streak"] = json!(-7);
        crate::store::db(&st.data_dir)
            .kv_set("scheduler_state", &root)
            .unwrap();
        assert_eq!(retry_cooldown_ms(&load_state(&st), key), 30 * 60_000);
    }

    /// 跨天新鲜失败（审查修复）：距上次失败 ≥24h 时 streak 重置为 1，
    /// 昨日残留不抬高今日首败冷却；24h 内连续失败照旧累计
    #[test]
    fn mark_fail_resets_streak_after_24h() {
        let st = temp_state("backoff24h");
        let key = "wb-checkin";
        // 伪造昨日失败残留：streak=3 + 25h 前的失败时间戳
        let mut root = load_state(&st);
        root["tasks"][key]["fail_streak"] = json!(3);
        root["tasks"][key]["last_fail_ts"] =
            json!(chrono::Utc::now().timestamp_millis() - 25 * 3_600_000);
        crate::store::db(&st.data_dir)
            .kv_set("scheduler_state", &root)
            .unwrap();
        // 跨天首败：streak 重置为 1 → 30 分钟起步
        mark_fail(&st, key, "跨天后首次失败");
        assert_eq!(retry_cooldown_ms(&load_state(&st), key), 30 * 60_000);
        assert_eq!(
            load_state(&st)
                .pointer(&format!("/tasks/{key}/fail_streak"))
                .and_then(Value::as_i64),
            Some(1)
        );
        // 24h 内连续失败：照旧累计（2 → 60 分钟）
        mark_fail(&st, key, "同日二连失败");
        assert_eq!(retry_cooldown_ms(&load_state(&st), key), 60 * 60_000);
    }
}
