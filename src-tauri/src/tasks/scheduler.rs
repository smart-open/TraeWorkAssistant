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
//! - 失败重试：仅成功才记 last_run_date；失败记 last_fail_ts + 连续失败计数
//!   （fail_streak），冷却 30 分钟起步、随连续失败指数退避 30→60→120 分钟封顶
//!  （issue #66 健壮性④：上游风控依赖故障时恒 30 分钟重试只会全天对上游连打
//!   ~28 轮，退避后收敛到 ~8 轮；成功一轮 mark_run 整体覆盖条目自然清零计数）；
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
    // Trae 每日签到：默认 09:00，环境配置页可改（settings.trae_checkin_hhmm）；
    // Windows 计划任务（AIWorkAssistant_DailyCheckin）注册时间复用同一设置值
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
    // F-80 Qoder 每日签到：默认 10:15 单次覆盖「0 点签到」与「10:00 登录奖励」双活动
    //（§2.2 调度设计结论；settings.qoder_checkin_hhmm 可改，任务配置页）
    SchedTask { key: "qoder-checkin", name: "Qoder 每日签到", hhmm: "10:15", kind: "fix" },
    // 看板数据同步（原快照任务升级为可配置频率：每日 HH:MM / 每小时 / 关闭）：
    // wb 侧含积分 fresh 拉取+池回写+快照、Token 统计重扫与官方用量刷新；
    // 排到晚間接近日末，差分口径最准；此前无定时写入是近 7 日消耗缺天的根因
    SchedTask { key: "wb-credits-snapshot", name: "WorkBuddy 积分与 Token 数据同步", hhmm: "23:30", kind: "credits" },
    SchedTask { key: "trae-credits-snapshot", name: "Trae 积分数据同步", hhmm: "23:40", kind: "credits" },
    // Trae 消耗明细同步（issue #61）：usage_history 此前只有前端点「更新消耗明细」才拉，
    // 页面常驻时快照停留在最后打开时刻。跟随 trae_credits_sync_mode 频率；daily 模式用
    // 内置 23:50（不与积分同步共占配置时刻，天然错峰 10 分钟）
    SchedTask { key: "trae-usage-sync", name: "Trae 消耗明细同步", hhmm: "23:50", kind: "credits" },
    // F-80 Qoder 积分快照：每日 HH:MM（settings.qoder_credits_sync_hhmm 可改，开关独立）
    SchedTask { key: "qoder-credits-snapshot", name: "Qoder 积分快照", hhmm: "23:40", kind: "fix" },
    // F-80 Qoder 凭证兜底刷新（kind "refresh" → EveryHours(间隔可配)；hhmm 不参与判定）。
    // 空池空转；开关/间隔独立（settings.qoder_token_renew_enabled /
    // qoder_token_renew_interval_hours，默认开/6h，环境配置页可改）
    SchedTask { key: "qoder-refresh", name: "Qoder 凭证定时刷新", hhmm: "06:00", kind: "refresh" },
    // 模型同步（每日 HH:MM，默认开；无账号时静默跳过不计失败，对齐 models-sync 惯例）
    SchedTask { key: "trae-models-sync", name: "Trae 模型列表同步", hhmm: "05:40", kind: "models" },
    SchedTask { key: "wb-catalog-sync", name: "WorkBuddy 模型目录同步", hhmm: "05:45", kind: "models" },
    // Qoder 模型目录同步（p3-3 收尾）：真 COSY 签名拉 model/list → adopt_remote
    // 替换 CN 区缓存；开关/时刻独立（settings.qoder_catalog_sync_enabled /
    // qoder_catalog_sync_hhmm，均默认开，环境配置页可改；空池/无凭证静默跳过不计失败）
    SchedTask { key: "qoder-catalog-sync", name: "Qoder 模型目录同步", hhmm: "05:50", kind: "models" },
];

/// 启动调度线程（main.rs setup 调用；启动 90s 后首跑，避开启动高峰）
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(90));
        loop {
            let st = app.state::<AppState>();
            tick(&app, &st);
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });
}

/// 失败重试冷却基数：失败后按连续失败次数指数退避（30 分钟起步 ×2 封顶 120 分钟）
const RETRY_COOLDOWN_MS: i64 = 30 * 60_000;
/// hourly 模式节流：距上次成功执行 ≥1h 才再跑（失败走退避冷却，不受此门限制）
const HOURLY_INTERVAL_MS: i64 = 60 * 60_000;
/// qoder-refresh 档位（默认 6 小时）说明：客户端 token 惰性窗 7h，间隔 ≤7h 即可
/// 确保过期令牌在窗口内被续上；真实缺省值在 models.rs serde default
/// （settings.qoder_token_renew_interval_hours，环境配置页可改 1~24h）

/// 单任务的生效调度计划：Skip=关闭；Daily=每日 HH:MM（到点+当日未跑，启动补跑）；
/// Hourly=每小时（距上次成功执行 ≥1h，无记录=首次立即跑）；
/// EveryHours(h)=每 h 小时（距上次成功执行 ≥h·1h，无记录=首次立即跑）
enum SchedPlan {
    Skip,
    Daily(String),
    Hourly,
    EveryHours(i64),
}

/// credits 类任务的同步模式（空/未知值按各平台默认处理——Buddy daily / Trae hourly（issue #61），
/// state::settings 已归一空值，此处兜底未知值）
fn credits_sync_mode(st: &AppState, key: &str) -> String {
    let s = st.settings();
    let (raw, fallback) = match key {
        "wb-credits-snapshot" => (s.wb_credits_sync_mode.as_str(), "daily"),
        _ => (s.trae_credits_sync_mode.as_str(), "hourly"),
    };
    match raw.trim() {
        "off" | "hourly" | "daily" => raw.trim().to_string(),
        _ => fallback.to_string(),
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
        // Qoder 凭证兜底刷新：每 N 小时（间隔 settings.qoder_token_renew_interval_hours
        // 可配，环境配置页；state 加载已 clamp 1~24，此处 max(1) 兜底防越界值直达）
        "refresh" => {
            let hours = (st.settings().qoder_token_renew_interval_hours.max(1)) as i64;
            SchedPlan::EveryHours(hours)
        }
        _ => SchedPlan::Daily(effective_hhmm(st, t)),
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

/// 单轮 tick：按各任务生效调度计划检查触发条件 → 执行
/// （AppHandle 仅用于看板数据同步成功后的前端事件联动，任务执行本身不依赖）
fn tick(app: &AppHandle, st: &AppState) {
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let now_hm = now.format("%H:%M").to_string();
    // 网关运行态句柄（main.rs manage；积分类任务运行中回写池快照用，issue #67）
    let api_runtime = app
        .state::<std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>();
    for t in TASKS {
        // 触发判定（trigger 用于日志展示：「每日 HH:MM」/「每小时」/「每N小时」）
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
                format!("每日 {hhmm}")
            }
            SchedPlan::Hourly => {
                if let Some(ts) = last_run_ts(st, t.key) {
                    if chrono::Utc::now().timestamp_millis() - ts < HOURLY_INTERVAL_MS {
                        continue;
                    }
                }
                "每小时".to_string()
            }
            SchedPlan::EveryHours(hours) => {
                if let Some(ts) = last_run_ts(st, t.key) {
                    if chrono::Utc::now().timestamp_millis() - ts < hours * HOURLY_INTERVAL_MS {
                        continue;
                    }
                }
                format!("每{hours}小时")
            }
        };
        // 失败冷却：退避窗口内静默等待重试，不重复执行也不刷日志
        if let Some(ts) = last_fail_ts(st, t.key) {
            if chrono::Utc::now().timestamp_millis() - ts < retry_cooldown_ms(st, t.key) {
                continue;
            }
        }
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_task(t.key, st, Some(api_runtime.inner()))
        })) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("任务线程 panic（已捕获，不影响后续调度）".to_string()),
        };
        match outcome {
            Ok(v) => {
                let summary = summarize(&v);
                // skipped_busy（审查修复：qoder-checkin 轮次锁被 UI 路径持有的幂等跳过）
                // 不 mark_run——跳过≠完成，mark_run 会固化「当日已跑」关闭当日重试链，
                // UI 轮次的失败账号将失去当日调度兜底。其余 skipped（空池静默空转）
                // 仍照常记账，防 EveryHours/Hourly 任务每 tick 空转刷日志
                if v.get("skipped_busy").is_some() {
                    fs_utils::app_log(
                        &st.data_dir,
                        &format!("[调度器] {}（{}）：{}（不记当日已跑，稍后重试）", t.name, trigger, summary),
                    );
                } else {
                    mark_run(st, t.key, &today, &summary);
                    fs_utils::app_log(&st.data_dir, &format!("[调度器] {}（{}）：{}", t.name, trigger, summary));
                    // 看板数据同步成功 → 通知前端重读缓存（issue #61 前端联动：
                    // Dashboard listen 后按 platform 匹配静默 refresh(false)，零额外网络）
                    if let Some(p) = board_platform(t.key) {
                        crate::events::emit_logged(
                            app,
                            "board-data-synced",
                            json!({ "platform": p, "task": t.key }),
                            Some(&st.data_dir),
                        );
                    }
                }
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
                // 退避窗口按实际分钟数展示（连续失败 30→60→120 分钟封顶）
                let cooldown_min = (retry_cooldown_ms(st, t.key) / 60_000).max(1);
                fs_utils::app_log(&st.data_dir, &format!("[调度器] {}（{}）失败，{cooldown_min} 分钟后重试：{}", t.name, trigger, summary));
                // 调度任务失败通知（通知渠道面板）：推 Bark/Webhook/Server酱（无 AppHandle，仅渠道；
                // 渠道失败静默，绝不影响调度循环）
                if first_fail_today {
                    crate::commands::workbuddy::push_notify(
                        None,
                        &st.data_dir,
                        &format!("{} 失败", t.name),
                        &format!("{summary}（{cooldown_min} 分钟后自动重试）"),
                        crate::notify::NotifyEvent::TaskFail,
                    );
                }
            }
        }
    }
}

/// 任务生效触发时刻：trae-checkin / trae-renew / wb-growth / wb-checkin 跟随设置页时刻
/// （环境配置页 / 任务配置页可改），看板同步与模型同步跟随各自 settings 时刻；
/// 其余任务用内置默认；配置非法（非 HH:MM 格式）时回退默认值
fn effective_hhmm(st: &AppState, t: &SchedTask) -> String {
    let configured = match t.key {
        "trae-checkin" => Some(st.settings().trae_checkin_hhmm),
        "trae-renew" => Some(st.settings().jwt_renew_hhmm),
        "wb-growth" => Some(st.settings().wb_growth_hhmm),
        "wb-checkin" => Some(st.settings().wb_checkin_hhmm),
        "wb-credits-snapshot" => Some(st.settings().wb_credits_sync_hhmm),
        "trae-credits-snapshot" => Some(st.settings().trae_credits_sync_hhmm),
        "qoder-checkin" => Some(st.settings().qoder_checkin_hhmm),
        "qoder-credits-snapshot" => Some(st.settings().qoder_credits_sync_hhmm),
        "wb-catalog-sync" => Some(st.settings().wb_catalog_sync_hhmm),
        "trae-models-sync" => Some(st.settings().trae_models_sync_hhmm),
        "qoder-catalog-sync" => Some(st.settings().qoder_catalog_sync_hhmm),
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
        // F-80 Qoder：签到跟随环境配置开关（qoder_settings.auto_checkin 默认开）；
        // 积分快照独立开关（默认开；无账号时任务内部静默跳过不计失败）
        "qoder-checkin" => crate::commands::qoder::load_settings(st).auto_checkin,
        "qoder-credits-snapshot" => st.settings().qoder_credits_sync_enabled,
        // Qoder 凭证定时续期（环境配置页可关；默认开，每 6h 兜底刷新）
        "qoder-refresh" => st.settings().qoder_token_renew_enabled,
        // 看板数据同步（积分/Token/消耗明细）：mode=off 即关闭（hourly/daily 均视为启用）
        "wb-credits-snapshot" | "trae-credits-snapshot" | "trae-usage-sync" => {
            credits_sync_mode(st, key) != "off"
        }
        // 模型同步开关（默认开；无账号时任务内部静默跳过不计失败）
        "wb-catalog-sync" => st.settings().wb_catalog_sync_enabled,
        "trae-models-sync" => st.settings().trae_models_sync_enabled,
        "qoder-catalog-sync" => st.settings().qoder_catalog_sync_enabled,
        // 其余任务幂等且低风险，恒开（Trae 签到 run_round 自带状态核验）
        _ => true,
    }
}

/// 执行单个任务（复用 CLI 任务同款实现，进度静默、结果汇总落日志）。
/// `runtime`：GUI 进程内的网关运行态句柄（tick 经 AppHandle 取得），
/// 供积分类任务运行中回写池快照（issue #67）；CLI 侧传 None
fn run_task(
    key: &str,
    st: &AppState,
    runtime: Option<&std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
) -> Result<Value, String> {
    match key {
        // Trae 每日签到：与 `--task-run checkin` 同款（vault 全账号单轮，状态核验幂等）。
        // 审查 P2：存在可重试失败时返 Err，交调度器 30 分钟冷却重试（对齐 qoder-checkin）；
        // 永久性失败（未配置 jwt，重试注定成功无望）从重试判定剔除——否则全天 ~28 轮无效重试
        "trae-checkin" => {
            let accounts = crate::vault::load_accounts(st);
            let retry = st.settings().retry.max(0) as u32;
            let mut events: Vec<Value> = Vec::new();
            let done =
                super::trae_checkin::run_round(st, &accounts.accounts, retry, &mut |ev| events.push(ev.clone()));
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
        // 审查 P2：同 trae-checkin——可重试失败返 Err 交 30 分钟冷却重试；
        // 永久性凭证失败（需重新登录 / 无可用凭证，30 分钟内不会自愈）剔除出重试判定
        "wb-checkin" => {
            let Ok(_round) = crate::commands::workbuddy::try_acquire_wb_round() else {
                return Err("跳过：已有签到/成长任务在执行中".into());
            };
            let mut events: Vec<Value> = Vec::new();
            let done = super::wb_checkin::run_checkin_round(
                st,
                &super::wb_checkin::CheckinOpts::daily(),
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
        // F-80 Qoder 每日签到：与 `--task-run qoder-checkin` 同款；抢轮次锁与 UI 路径互斥
        "qoder-checkin" => {
            // 抢不到轮次锁（UI 路径正在签到）= 幂等跳过而非失败：返 Err 会被调度器记
            // 当日首败并推送「签到失败」误报通知（签到本身未失败，UI 路径会照常完成）
            let Ok(_round) = crate::tasks::qoder_checkin::try_acquire_qoder_round() else {
                // skipped_busy：tick 据此跳过 mark_run（保留当日后续 tick 重试机会）
                return Ok(json!({ "ok": true, "skipped": "已有 Qoder 签到任务在执行中，本轮跳过", "skipped_busy": true }));
            };
            let opts = super::qoder_checkin::QoderCheckinOpts::daily();
            let done = super::qoder_checkin::run_checkin_round(st, &opts, &mut |_| {});
            // 审查 M-1：存在失败账号时返 Err，交调度器 30 分钟冷却重试（暂态失败自愈）。
            // P2 重试口径：empty_campaigns（活动未上线/不可用）属非用户可操作失败，
            // 计入重试只会全天无效重试 + 当日首败误报通知——按 failed - failed_empty_campaigns
            // > 0 判定（对照 qoder_checkin::run_checkin_round 的设计注释）。done 事件恒携带
            // 该字段；旧结构缺字段时 as_i64 为 None → 0，退回原口径，安全兼容
            let failed = done.get("failed").and_then(serde_json::Value::as_i64).unwrap_or(0);
            let failed_empty = done
                .get("failed_empty_campaigns")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            // 审查 minor：永久性认证失败（pat_rejected/expired_needs_relogin/auth_dead）
            // 重试注定失败，与 empty_campaigns 一并从重试判定剔除——否则失效账号
            // 会拖动整轮全天约 28 次冷却重试（含每轮必败的刷新请求）
            let failed_permanent = done
                .get("failed_permanent")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            if failed - failed_empty - failed_permanent > 0 {
                let ok = done.get("ok").and_then(serde_json::Value::as_i64).unwrap_or(0);
                let already = done.get("already").and_then(serde_json::Value::as_i64).unwrap_or(0);
                return Err(format!(
                    "Qoder 签到：{ok} 成功 / {already} 已领 / {failed} 失败（稍后自动重试）"
                ));
            }
            Ok(done)
        }
        // F-80 Qoder 积分快照：与 `--task-run qoder-credits-snapshot` 同款（空池空转）
        "qoder-credits-snapshot" => super::qoder_credits::run_snapshot_task(st),
        // Qoder Token 定时刷新：与 `--task-run qoder-refresh` 同款（6h 周期，lazy 7h 惰性门；
        // 有账号刷新失败时返 Err，交由调度器 30 分钟冷却重试）
        "qoder-refresh" => super::qoder_refresh::run_task(st),
        // Qoder 模型目录同步：与 `--task-run qoder-catalog-sync` 同款（真签名拉
        // model/list → adopt_remote；空池/无凭证空转，网络失败返 Err 冷却重试）
        "qoder-catalog-sync" => super::qoder_catalog::run_task(st),
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
                    machine_id_override: None,
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
        // 统计尽力而为不上抛（页面打开时本就有各自缓存/降级链路）；用量刷全账号聚合缓存
        // （看板 Dashboard 唯一消费源 workbuddy_usage_official_all_cache，force 跳过 10min
        // 缓存；原单账号缓存分支前端无调用方，属刷错目标，issue #61 同类缺口一并修复）
        "wb-credits-snapshot" => {
            let parsed = crate::commands::workbuddy::wb_credits_snapshot_task(st)?;
            // token 统计同步沿用结果级缓存（fresh=false，2026-10-07 性能优化）：
            // 原传 true 会绕过 10 分钟结果缓存，每天（hourly 档）强制一次全量
            // walk+stat；当日数据本就有 10 分钟 TTL 兜底，无需强制重扫。
            let token_files = crate::commands::workbuddy_stats::workbuddy_token_stats_impl(st, false)
                .get("files_scanned")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let usage = match crate::commands::workbuddy::workbuddy_usage_official_all_impl(st, true) {
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
        // Trae 积分数据同步：与 `--task-run refresh-credits` 同款（无账号返回 refreshed=0）；
        // GUI 进程内把 runtime 句柄传入，网关运行中同步回写池内积分快照（issue #67）
        "trae-credits-snapshot" => crate::commands::accounts::refresh_remaining_credits_impl(st, runtime)
            .map(|n| json!({ "ok": true, "refreshed": n })),
        // Trae 消耗明细同步（issue #61）：与 `--task-run trae-usage-sync` 同款（fresh=true 增量拉取）；
        // 无账号静默跳过（对齐 trae-models-sync 惯例）；全部账号拉取失败返 Err 交 30 分钟冷却重试
        "trae-usage-sync" => {
            let accounts = crate::vault::load_accounts(st);
            if accounts.accounts.is_empty() {
                Ok(json!({ "ok": true, "skipped": "无 Trae 账号" }))
            } else {
                crate::commands::usage_history::usage_history_fetch_impl(st, true)
                    .map(|r| json!({ "ok": true, "accounts": r.accounts.len() }))
            }
        }
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
        // WorkBuddy 上游模型目录同步：wb_upstream_accounts 取一次共用（判空跳过 + impl 首账号）；
        // 无凭证账号静默跳过不计失败
        "wb-catalog-sync" => {
            let accounts = crate::commands::workbuddy::wb_upstream_accounts(st);
            if accounts.is_empty() {
                Ok(json!({ "ok": true, "skipped": "无可用 WB 账号凭证" }))
            } else {
                crate::commands::api_server::wb_catalog_sync_impl(&st.data_dir, &accounts)
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

/// 退避冷却时长：cooldown = 30 分钟 << min(streak-1, 2)，连续失败 30→60→120 分钟封顶
/// （streak=0/缺字段兼容旧条目回 30 分钟；mark_run 成功整体覆盖条目自然清零计数）
/// shift 用显式分支求值：saturating_sub(1).min(2) 在 streak=0 时得 -1（0-1=-1 是
/// 合法语义，min 只夹上界不夹下界），-1 as u32 = u32::MAX 触发移位溢出 panic——
/// 显式比较同时夹上下界，语义等价且边界自明。
fn retry_cooldown_ms(st: &AppState, key: &str) -> i64 {
    let streak = load_state(st)
        .pointer(&format!("/tasks/{key}/fail_streak"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let streak = if streak < 0 { 0 } else { streak };
    let shift: u32 = if streak <= 1 { 0 } else if streak >= 3 { 2 } else { 1 };
    RETRY_COOLDOWN_MS << shift
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
    // C6（审查）：落库失败补日志不静默——last_run_date 丢失 → 当日任务重复执行；
    // last_fail_ts 丢失 → 失败冷却失效引发重试风暴。保持不 panic
    if let Err(e) = crate::store::db(&st.data_dir).kv_set("scheduler_state", &root) {
        fs_utils::app_log(&st.data_dir, &format!("[调度器] 状态落库失败({key}): {e}"));
    }
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
            // 展示时刻：hourly → 「每小时」；EveryHours(h) → 「每h小时」；关闭 → 「已关闭」；其余 HH:MM
            let time = match sched_plan(&st, t) {
                SchedPlan::Hourly => "每小时".to_string(),
                SchedPlan::EveryHours(h) => format!("每{h}小时"),
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
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
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
        // Trae 看板同步（issue #61）：默认 hourly——积分快照与消耗明细（trae-usage-sync）均每小时
        let trae_credits = || TASKS.iter().find(|t| t.key == "trae-credits-snapshot").unwrap();
        let trae_usage = || TASKS.iter().find(|t| t.key == "trae-usage-sync").unwrap();
        assert!(matches!(sched_plan(&st, trae_credits()), SchedPlan::Hourly), "Trae 默认 hourly");
        assert!(matches!(sched_plan(&st, trae_usage()), SchedPlan::Hourly), "消耗明细跟随 hourly");
        // daily 模式：积分快照走配置时刻（23:40），消耗明细内置 23:50 错峰
        set(json!({ "trae_credits_sync_mode": "daily" }));
        assert!(matches!(sched_plan(&st, trae_credits()), SchedPlan::Daily(h) if h == "23:40"));
        assert!(matches!(sched_plan(&st, trae_usage()), SchedPlan::Daily(h) if h == "23:50"));
        // off 同时关闭积分快照与消耗明细
        set(json!({ "trae_credits_sync_mode": "off" }));
        assert!(matches!(sched_plan(&st, trae_credits()), SchedPlan::Skip));
        assert!(matches!(sched_plan(&st, trae_usage()), SchedPlan::Skip));
        // 非法模式回退 Trae 默认 hourly
        set(json!({ "trae_credits_sync_mode": "weekly" }));
        assert!(matches!(sched_plan(&st, trae_usage()), SchedPlan::Hourly));
        // 模型同步时刻覆盖 + 显式关闭
        set(json!({ "trae_models_sync_hhmm": "06:10" }));
        assert!(matches!(sched_plan(&st, trae_models()), SchedPlan::Daily(h) if h == "06:10"));
        set(json!({ "trae_models_sync_enabled": false }));
        assert!(matches!(sched_plan(&st, trae_models()), SchedPlan::Skip));
    }

    /// 看板联动平台映射（issue #61 + 审查修复）：三个看板数据任务分别 emit
    /// buddy/trae/qoder；凭证刷新与签到类任务不属于看板数据同步，不得误 emit
    #[test]
    fn board_platform_maps_board_tasks_only() {
        assert_eq!(board_platform("wb-credits-snapshot"), Some("buddy"));
        assert_eq!(board_platform("trae-credits-snapshot"), Some("trae"));
        assert_eq!(board_platform("trae-usage-sync"), Some("trae"));
        assert_eq!(board_platform("qoder-credits-snapshot"), Some("qoder"));
        assert_eq!(board_platform("qoder-refresh"), None);
        assert_eq!(board_platform("qoder-checkin"), None);
    }

    /// 失败退避（issue #66 健壮性④）：无记录/首败 30 分钟起步，连续失败 ×2
    /// 30→60→120 封顶；负值 streak 兜底按 0；mark_run 成功整体覆盖条目清零计数
    #[test]
    fn retry_cooldown_backoff_and_streak_reset() {
        let st = temp_state("backoff");
        let key = "qoder-checkin";
        // 无记录：30 分钟起步
        assert_eq!(retry_cooldown_ms(&st, key), 30 * 60_000);
        // 连续失败 1/2/3/4 次：30/60/120/120（封顶）
        let mut total = 0;
        for (n, expect_min) in [(1, 30), (2, 60), (3, 120), (4, 120)] {
            while total < n {
                mark_fail(&st, key, "模拟失败");
                total += 1;
            }
            assert_eq!(
                retry_cooldown_ms(&st, key),
                expect_min * 60_000,
                "连续失败 {n} 次后冷却应为 {expect_min} 分钟"
            );
        }
        // streak 确实持久化到状态
        assert_eq!(
            load_state(&st).pointer(&format!("/tasks/{key}/fail_streak")).and_then(Value::as_i64),
            Some(4)
        );
        // 成功 mark_run 整体覆盖条目 → fail_streak 消失，冷却回 30 分钟
        mark_run(&st, key, "2026-10-07", "成功 1，已签 0，失败 0");
        assert_eq!(retry_cooldown_ms(&st, key), 30 * 60_000);
        assert_eq!(
            load_state(&st).pointer(&format!("/tasks/{key}/last_fail_ts")).and_then(Value::as_i64),
            None,
            "mark_run 应清掉 last_fail_ts"
        );
        // 手写负值 streak（脏数据）：兜底按 0 → 30 分钟
        let mut root = load_state(&st);
        root["tasks"][key]["fail_streak"] = json!(-7);
        crate::store::db(&st.data_dir).kv_set("scheduler_state", &root).unwrap();
        assert_eq!(retry_cooldown_ms(&st, key), 30 * 60_000);
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
        crate::store::db(&st.data_dir).kv_set("scheduler_state", &root).unwrap();
        // 跨天首败：streak 重置为 1 → 30 分钟起步
        mark_fail(&st, key, "跨天后首次失败");
        assert_eq!(retry_cooldown_ms(&st, key), 30 * 60_000);
        assert_eq!(
            load_state(&st).pointer(&format!("/tasks/{key}/fail_streak")).and_then(Value::as_i64),
            Some(1)
        );
        // 24h 内连续失败：照旧累计（2 → 60 分钟）
        mark_fail(&st, key, "同日二连失败");
        assert_eq!(retry_cooldown_ms(&st, key), 60 * 60_000);
    }
}
