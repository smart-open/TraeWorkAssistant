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
    // Buddy 国际版每日活跃打卡（阶段 3b，§4.5 补篇）：网页会话 + 沙箱 ACP 跑完一轮
    // → completed 后服务端自动入账 30/50 积分（无 claim 端点）；默认 09:20 可改
    //（settings.buddy_ai_checkin_hhmm）；开关独立（buddy_ai_checkin_enabled，默认关）
    SchedTask { key: "buddy-ai-checkin", name: "Buddy 国际版每日活跃打卡", hhmm: "09:20", kind: "fix" },
    // Buddy 国际版凭证兜底续期（2026-10-16 六页对齐批 3，对齐国内 wb-renew）：
    // lazy 48h——AT 到期前 48h 内才真正刷新；开关独立（默认开，环境配置页可关）
    SchedTask { key: "buddy-ai-renew", name: "Buddy 国际版凭证兜底续期", hhmm: "10:35", kind: "fix" },
    // Buddy 国际版积分数据同步（对齐国内看板同步口径）：get-user-resource 全池
    // 取数 → 池三键回写 + 当日快照落库（趋势曲线数据源）；五模式可配
    //（off/hourly/every_hours/every_days/daily，settings.buddy_ai_credits_sync_mode，
    // daily 时刻默认 23:45 错峰）
    SchedTask { key: "buddy-ai-credits-sync", name: "Buddy 国际版积分数据同步", hhmm: "23:45", kind: "credits" },
    // 看板数据同步（原快照任务升级为可配置频率：每日 HH:MM / 每小时 / 关闭）：
    // wb 侧含积分 fresh 拉取+池回写+快照、Token 统计重扫与官方用量刷新；
    // 排到晚間接近日末，差分口径最准；此前无定时写入是近 7 日消耗缺天的根因
    SchedTask { key: "wb-credits-snapshot", name: "WorkBuddy 积分与 Token 数据同步", hhmm: "23:30", kind: "credits" },
    SchedTask { key: "trae-credits-snapshot", name: "Trae 积分数据同步", hhmm: "23:40", kind: "credits" },
    // Trae 消耗明细同步（issue #61）：usage_history 此前只有前端点「更新消耗明细」才拉，
    // 页面常驻时快照停留在最后打开时刻。跟随 trae_credits_sync_mode 频率；daily 模式用
    // 内置 23:50（不与积分同步共占配置时刻，天然错峰 10 分钟）
    SchedTask { key: "trae-usage-sync", name: "Trae 消耗明细同步", hhmm: "23:50", kind: "credits" },
    // F-80 Qoder 积分数据同步：与 Buddy/Trae 同款四模式（off/hourly/daily/every_hours，
    // settings.qoder_credits_sync_mode；daily 时刻可改，默认 23:40；mode 缺失时 state 按旧 enabled 派生）
    SchedTask { key: "qoder-credits-snapshot", name: "Qoder 积分数据同步", hhmm: "23:40", kind: "credits" },
    // F-80 Qoder 凭证兜底刷新（kind "refresh" → EveryHours(间隔可配)；hhmm 不参与判定）。
    // 空池空转；开关/间隔独立（settings.qoder_token_renew_enabled /
    // qoder_token_renew_interval_hours，默认开/6h，环境配置页可改）
    SchedTask { key: "qoder-refresh", name: "Qoder 凭证定时刷新", hhmm: "06:00", kind: "refresh" },
    // Qoder 会话流水定时合并（kind "compact" → EveryHours(6)；hhmm 不参与判定）：
    // 每 6 小时把「7 天前」的请求级流水按 (date, uid, preview) 合并为单行
    // （幂等自愈，积分/token 合计不变；历史行 session_id 为随机值，按 preview
    // 归并才对旧行生效）。展示层另有读时聚合（qoder_sessions_query 两遍归并），
    // 本任务只负责压缩历史行体积；开关独立（settings.qoder_sessions_compact_enabled，
    // 默认开，环境配置页可改）
    SchedTask { key: "qoder-sessions-compact", name: "Qoder 会话流水定时合并", hhmm: "04:30", kind: "compact" },
    // 模型同步（每日 HH:MM，默认开；无账号时静默跳过不计失败，对齐 models-sync 惯例）
    SchedTask { key: "trae-models-sync", name: "Trae 模型列表同步", hhmm: "05:40", kind: "models" },
    SchedTask { key: "wb-catalog-sync", name: "WorkBuddy 模型目录同步", hhmm: "05:45", kind: "models" },
    // Qoder 模型目录同步（p3-3 收尾）：真 COSY 签名拉 model/list → adopt_remote
    // 替换 CN 区缓存；开关/时刻独立（settings.qoder_catalog_sync_enabled /
    // qoder_catalog_sync_hhmm，均默认开，环境配置页可改；空池/无凭证静默跳过不计失败）
    SchedTask { key: "qoder-catalog-sync", name: "Qoder 模型目录同步", hhmm: "05:50", kind: "models" },
    // Buddy 国际版模型目录同步（2026-10-16）：/v3/config 直连拉取（动态目录替换，
    // 与 API 服务启动挂点互补）；开关/时刻独立（默认开，05:55 错峰）
    SchedTask { key: "buddy-ai-catalog-sync", name: "Buddy 国际版模型目录同步", hhmm: "05:55", kind: "models" },
];

/// 启动调度线程（main.rs setup 调用；启动 90s 后首跑，避开启动高峰）
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(90));
        // 审查 #14：buddy-ai 打卡逐账号连击内存态——跨 tick 存活于调度线程，
        // 只在调度线程访问（单线程串行 tick），无需加锁
        let mut buddy_acct_streaks = BuddyAiAcctFailStreaks::default();
        loop {
            let st = app.state::<AppState>();
            tick(&app, &st, &mut buddy_acct_streaks);
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });
}

/// 失败重试冷却基数：失败后按连续失败次数指数退避（30 分钟起步 ×2 封顶 120 分钟）
const RETRY_COOLDOWN_MS: i64 = 30 * 60_000;
/// 单轮 tick 内多个到期任务（典型为启动补跑积压）之间的错峰间隔（issue #84 C1）：
/// 重任务（快照/用量同步）背靠背连跑会形成分钟级 CPU 爆发（实测 103s），任务间
/// 睡眠错开峰值，给系统留出处理其他负载的余量。
const TICK_STAGGER_SECS: u64 = 30;
/// hourly 模式节流：距上次成功执行 ≥1h 才再跑（失败走退避冷却，不受此门限制）
const HOURLY_INTERVAL_MS: i64 = 60 * 60_000;
/// qoder-refresh 档位（默认 6 小时）说明：客户端 token 惰性窗 7h，间隔 ≤7h 即可
/// 确保过期令牌在窗口内被续上；真实缺省值在 models.rs serde default
/// （settings.qoder_token_renew_interval_hours，环境配置页可改 1~24h）
/// qoder-sessions-compact 档位：每 6 小时合并一次「当天之前」的会话流水
/// （当天数据由查询层读时聚合即时呈现，无等待诉求）
const COMPACT_INTERVAL_HOURS: i64 = 6;
/// 审查修复（第6项；#14 起按账号粒度累计）：buddy-ai 打卡单账号连续失败轮数暂停
/// 阈值——失效凭证（封禁/会话失效/refresh 永久拒绝）触发的账号失败不可自愈
///（T7/BL-5「401 可自愈」只覆盖凭证类瞬时失效），全天 30min 冷却重试 + 每日首败
/// 通知是纯噪声；单账号连击达阈值当日暂停，次日自动重置
const BUDDY_AI_CHECKIN_FAIL_STREAK_LIMIT: i64 = 3;
/// 审查 #16：buddy-ai-renew 全败轮连击当日暂停阈值（与打卡 LIMIT 同值）。
/// Intl 刷新失败有意不标 needs_relogin（防误杀真实凭证），永久失效账号会每日 +
/// 每 30 分钟重试——调度侧熔断：连续 3 轮全败当日暂停，次日自动恢复
const BUDDY_AI_RENEW_FAIL_STREAK_LIMIT: i64 = 3;

/// 单任务的生效调度计划：Skip=关闭；Daily=每日 HH:MM（到点+当日未跑，启动补跑）；
/// Hourly=每小时（距上次成功执行 ≥1h，无记录=首次立即跑）；
/// EveryHours(h)=每 h 小时（距上次成功执行 ≥h·1h，无记录=首次立即跑）；
/// EveryDays(d)=每 d 天（距上次成功执行 ≥d·24h，无记录=首次立即跑）
enum SchedPlan {
    Skip,
    Daily(String),
    Hourly,
    EveryHours(i64),
    EveryDays(i64),
}

/// credits 类任务的同步模式（空/未知值按各平台默认处理——every_hours 每2小时
/// （issue #82 会话明细定时落盘），state::settings 已归一空值，此处兜底未知值）
fn credits_sync_mode(st: &AppState, key: &str) -> String {
    let s = st.settings();
    let raw = match key {
        "wb-credits-snapshot" => s.wb_credits_sync_mode.as_str(),
        "qoder-credits-snapshot" => s.qoder_credits_sync_mode.as_str(),
        "buddy-ai-credits-sync" => s.buddy_ai_credits_sync_mode.as_str(),
        _ => s.trae_credits_sync_mode.as_str(),
    };
    match raw.trim() {
        "off" | "hourly" | "daily" | "every_hours" | "every_days" => raw.trim().to_string(),
        _ => "every_hours".to_string(),
    }
}

/// credits 类任务 every_hours 模式的间隔小时数：非法/越界回退 2（1..=24）
fn credits_sync_hours(st: &AppState, key: &str) -> i64 {
    let s = st.settings();
    let raw = match key {
        "wb-credits-snapshot" => s.wb_credits_sync_hours.as_str(),
        "qoder-credits-snapshot" => s.qoder_credits_sync_hours.as_str(),
        "buddy-ai-credits-sync" => s.buddy_ai_credits_sync_hours.as_str(),
        _ => s.trae_credits_sync_hours.as_str(),
    };
    raw.trim().parse::<i64>().unwrap_or(2).clamp(1, 24)
}

/// credits 类任务 every_days 模式的间隔天数：非法/越界回退 1（1..=30）
/// （当前仅 buddy-ai-credits-sync 提供 every_days 档；其余平台按需扩展）
fn credits_sync_days(st: &AppState, key: &str) -> i64 {
    let s = st.settings();
    let raw = match key {
        "buddy-ai-credits-sync" => s.buddy_ai_credits_sync_days.as_str(),
        _ => "",
    };
    raw.trim().parse::<i64>().unwrap_or(1).clamp(1, 30)
}

/// 单任务调度计划：合并启用判定与模式/时刻解析（tick 与 scheduler_status 共用）
fn sched_plan(st: &AppState, t: &SchedTask) -> SchedPlan {
    if !enabled(st, t.key) {
        return SchedPlan::Skip;
    }
    match t.kind {
        // 看板数据同步：按模式分档——hourly 每小时节流 / every_hours 每 N 小时节流
        // （N 可手工设置，默认 2）/ daily 按每日时刻。非法值不会到达此分支：
        // credits_sync_mode 已把空/未知值归一为 every_hours（state 缺省经
        // fallback 回填，Trae/Buddy/Qoder 三池口径一致，恒 every_hours 兜底）
        "credits" => match credits_sync_mode(st, t.key).as_str() {
            "hourly" => SchedPlan::Hourly,
            "every_hours" => SchedPlan::EveryHours(credits_sync_hours(st, t.key)),
            "every_days" => SchedPlan::EveryDays(credits_sync_days(st, t.key)),
            _ => SchedPlan::Daily(effective_hhmm(st, t)),
        },
        // Qoder 凭证兜底刷新：每 N 小时（间隔 settings.qoder_token_renew_interval_hours
        // 可配，环境配置页；state 加载已 clamp 1~24，此处 max(1) 兜底防越界值直达）
        "refresh" => {
            let hours = (st.settings().qoder_token_renew_interval_hours.max(1)) as i64;
            SchedPlan::EveryHours(hours)
        }
        // Qoder 会话流水合并：固定每 6 小时（hhmm 不参与判定；开关 settings.qoder_sessions_compact_enabled）
        "compact" => SchedPlan::EveryHours(COMPACT_INTERVAL_HOURS),
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
        // T3 审查修复：buddy-ai 积分同步任务写入 buddy_ai_credits_cache 与池快照
        // 后，BuddyAi 看板同款联动（此前缺映射致看板数据滞后到下次手动刷新）
        "buddy-ai-credits-sync" => Some("buddy-ai"),
        _ => None,
    }
}

/// 单轮 tick：按各任务生效调度计划检查触发条件 → 执行
/// （AppHandle 仅用于看板数据同步成功后的前端事件联动，任务执行本身不依赖）
/// `buddy_acct_streaks`：buddy-ai 打卡逐账号连击内存态（审查 #14，调度线程独占）
fn tick(
    app: &AppHandle,
    st: &AppState,
    buddy_acct_streaks: &mut BuddyAiAcctFailStreaks,
) {
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let now_hm = now.format("%H:%M").to_string();
    // 网关运行态句柄（main.rs manage；积分类任务运行中回写池快照用，issue #67）
    let api_runtime = app
        .state::<std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>();
    let mut ran_this_tick: usize = 0;
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
            SchedPlan::EveryDays(days) => {
                if let Some(ts) = last_run_ts(st, t.key) {
                    if chrono::Utc::now().timestamp_millis() - ts < days * 24 * HOURLY_INTERVAL_MS {
                        continue;
                    }
                }
                format!("每{days}天")
            }
        };
        // 失败冷却：退避窗口内静默等待重试，不重复执行也不刷日志
        if let Some(ts) = last_fail_ts(st, t.key) {
            if chrono::Utc::now().timestamp_millis() - ts < retry_cooldown_ms(st, t.key) {
                continue;
            }
        }
        // 错峰（issue #84 C1）：本轮此前已有任务执行（典型为启动补跑积压的多个到期
        // 任务单 tick 连跑，实测 103s CPU 爆发），先隔一个间隔再跑下一个。睡眠只
        // 发生在后台调度线程自身；触发判定用的 now_hm 取自 tick 起点，「已过 HH:MM」
        // 语义不受影响，仅执行时刻顺延。
        if ran_this_tick > 0 {
            std::thread::sleep(std::time::Duration::from_secs(TICK_STAGGER_SECS));
        }
        ran_this_tick += 1;
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_task(t.key, st, Some(api_runtime.inner()), buddy_acct_streaks)
        })) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("任务线程 panic（已捕获，不影响后续调度）".to_string()),
        };
        match outcome {
            Ok(v) => {
                let summary = summarize(&v, t.key);
                // skipped_busy（审查修复：qoder-checkin 轮次锁被 UI 路径持有的幂等跳过）
                // 与 skipped_empty_pool（审查 #15：buddy-ai 空池空轮）不 mark_run——
                // 跳过≠完成，mark_run 会固化「当日已跑」关闭当日重试链，UI 轮次的
                // 失败账号将失去当日调度兜底；空池空轮记账则会让当日新增账号失去
                // 调度补打。skipped_busy 落日志（低频、值得留痕）；空轮纯静默
                //（池空期间每 tick 触发，落日志会刷屏，对齐「空池静默空转」定调）
                if v.get("skipped_busy").is_some() {
                    fs_utils::app_log(
                        &st.data_dir,
                        &format!("[调度器] {}（{}）：{}（不记当日已跑，稍后重试）", t.name, trigger, summary),
                    );
                // 审查 #15：skipped_empty_pool（buddy-ai 打卡空轮，本轮无账号参与）
                // 同样不 mark_run——mark_run 会写 last_run_date=今日，当日新增账号
                // 不再被调度（补打窗口被吞）。仅记日志；账号导入后下一 tick 正常执行
                } else if v.get("skipped_empty_pool").is_none() {
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
        "buddy-ai-checkin" => Some(st.settings().buddy_ai_checkin_hhmm),
        "buddy-ai-renew" => Some(st.settings().buddy_ai_renew_hhmm),
        "buddy-ai-credits-sync" => Some(st.settings().buddy_ai_credits_sync_hhmm),
        "buddy-ai-catalog-sync" => Some(st.settings().buddy_ai_catalog_sync_hhmm),
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
        // F-80 Qoder：签到跟随环境配置开关（qoder_settings.auto_checkin 默认开）
        "qoder-checkin" => crate::commands::qoder::load_settings(st).auto_checkin,
        // Buddy 国际版每日活跃打卡（阶段 3b）：独立开关（默认关，国际档显式开启制）
        "buddy-ai-checkin" => crate::commands::workbuddy::buddy_ai_checkin_enabled(st),
        // Buddy 国际版凭证兜底续期（2026-10-16，默认开，任务配置页可关）
        "buddy-ai-renew" => st.settings().buddy_ai_renew_enabled,
        // Qoder 凭证定时续期（环境配置页可关；默认开，每 6h 兜底刷新）
        "qoder-refresh" => st.settings().qoder_token_renew_enabled,
        // Qoder 会话流水定时合并（环境配置页可关；默认开，每日 04:30）
        "qoder-sessions-compact" => st.settings().qoder_sessions_compact_enabled,
        // 看板数据同步（积分/Token/消耗明细）：mode=off 即关闭（其余模式均视为启用；
        // buddy-ai 旧 enabled 布尔已由 state 归一化派生进 mode）
        "wb-credits-snapshot" | "trae-credits-snapshot" | "trae-usage-sync" | "qoder-credits-snapshot"
        | "buddy-ai-credits-sync" => {
            credits_sync_mode(st, key) != "off"
        }
        // 模型同步开关（默认开；无账号时任务内部静默跳过不计失败）
        "wb-catalog-sync" => st.settings().wb_catalog_sync_enabled,
        "buddy-ai-catalog-sync" => st.settings().buddy_ai_catalog_sync_enabled,
        "trae-models-sync" => st.settings().trae_models_sync_enabled,
        "qoder-catalog-sync" => st.settings().qoder_catalog_sync_enabled,
        // 其余任务幂等且低风险，恒开（Trae 签到 run_round 自带状态核验）
        _ => true,
    }
}

/// 执行单个任务（复用 CLI 任务同款实现，进度静默、结果汇总落日志）。
/// `runtime`：GUI 进程内的网关运行态句柄（tick 经 AppHandle 取得），
/// 供积分类任务运行中回写池快照（issue #67）；CLI 侧传 None。
/// `buddy_acct_streaks`：buddy-ai 打卡逐账号连击内存态（审查 #14，仅调度线程读写）
fn run_task(
    key: &str,
    st: &AppState,
    runtime: Option<&std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    buddy_acct_streaks: &mut BuddyAiAcctFailStreaks,
) -> Result<Value, String> {
    match key {
        // Trae 每日签到：与 `--task-run checkin` 同款（vault 全账号单轮，状态核验幂等）。
        // 审查 P2：存在可重试失败时返 Err，交调度器 30 分钟冷却重试（对齐 qoder-checkin）；
        // 永久/当日不可恢复失败从重试判定剔除——否则全天 ~28 轮无效重试 + 当日首败误报
        //（口径对齐 UI 重试筛选：未配置 jwt（历史口径 message 匹配）+ SessionDead=401 吊销 /
        //  AuthRejected=服务端拒绝认证（issue #128）/ DeviceSigned=设备当日去重）
        "trae-checkin" => {
            let accounts = crate::vault::load_accounts(st);
            let retry = st.settings().retry.max(0) as u32;
            // 审查修复 M2：调度轮跳过冷却中账号（until > now，含 SessionDead/
            // AuthRejected 永久冷却与 DeviceSigned 当日去重）——冷却账号每轮真实
            // 请求只会再失败再冷却，对齐 UI 签到路径「冷却中跳过」口径
            let now_ts = chrono::Utc::now().timestamp();
            let cooled: std::collections::HashSet<String> =
                crate::store::docs::account_cooldowns_load(&crate::store::db(&st.data_dir))
                    .cooldowns
                    .into_iter()
                    .filter(|(_, e)| e.until > now_ts)
                    .map(|(uid, _)| uid)
                    .collect();
            let mut skipped_cooldown = 0usize;
            let active: Vec<_> = accounts
                .accounts
                .into_iter()
                .filter(|a| match a.user_id.as_deref() {
                    Some(uid) if cooled.contains(uid) => {
                        skipped_cooldown += 1;
                        false
                    }
                    _ => true,
                })
                .collect();
            if skipped_cooldown > 0 {
                crate::fs_utils::app_log(
                    &st.data_dir,
                    &format!("Trae 签到：跳过 {skipped_cooldown} 个冷却中账号"),
                );
            }
            let mut events: Vec<Value> = Vec::new();
            let done =
                super::trae_checkin::run_round(st, &active, retry, &mut |ev| events.push(ev.clone()));
            let failed = done.get("failed").and_then(Value::as_i64).unwrap_or(0);
            let failed_permanent = events
                .iter()
                .filter(|e| {
                    if e.get("status").and_then(Value::as_str) != Some("fail") {
                        return false;
                    }
                    if e.get("message").and_then(Value::as_str) == Some("未配置 jwt") {
                        return true;
                    }
                    matches!(
                        e.get("error_type").and_then(Value::as_str),
                        Some("SessionDead") | Some("AuthRejected") | Some("DeviceSigned")
                    )
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
            // 审查 M4：抢不到轮次锁（UI 路径正在签到/成长）= 幂等跳过而非失败，
            // 对齐 qoder-checkin 的 skipped_busy 口径——返 Err 会被调度器记当日首败
            // 并推送「签到失败」误报通知，白耗 Server酱 当日通知额度
            let Ok(_round) = crate::commands::workbuddy::try_acquire_wb_round() else {
                // skipped_busy：抢不到轮次锁（UI 路径在跑）= 幂等跳过而非失败（对齐
                // qoder-checkin）——返 Err 会触发当日首败误报通知 + 白耗 Server酱 额度
                return Ok(json!({ "ok": true, "skipped": "已有签到/成长任务在执行中，本轮跳过", "skipped_busy": true }));
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
            // 审查 M4：同 wb-checkin——锁冲突改 skipped_busy 幂等跳过，不记失败不通知
            let Ok(_round) = crate::commands::workbuddy::try_acquire_wb_round() else {
                // skipped_busy：对齐 qoder-checkin/wb-checkin 的幂等跳过（防误报通知）
                return Ok(json!({ "ok": true, "skipped": "已有签到/成长任务在执行中，本轮跳过", "skipped_busy": true }));
            };
            let s = crate::commands::workbuddy::load_settings(st);
            let flags = super::wb_checkin::GrowthOpts {
                travel: s.growth_travel,
                lottery: s.growth_lottery,
                tasks: s.growth_tasks,
            };
            // run_growth_round 为 NDJSON 推进式输出（无汇总返回值）。审查修复：失败事件
            // 此前被空回调丢弃（恒 Ok：当日不重试不通知）——现累计事件，可重试失败返
            // Err 交 30 分钟冷却重试；永久性凭证失败剔除（对齐 wb-checkin 口径）。
            // main 侧改进：按 type=="growth" 精确过滤，非 growth 的 fail 事件不误计入
            let mut events: Vec<Value> = Vec::new();
            super::wb_checkin::run_growth_round(st, &flags, &[], &mut |ev| events.push(ev.clone()));
            let is_growth_fail = |e: &&Value| {
                e.get("type").and_then(Value::as_str) == Some("growth")
                    && e.get("status").and_then(Value::as_str) == Some("fail")
            };
            let failed = events.iter().filter(is_growth_fail).count() as i64;
            let failed_permanent = events
                .iter()
                .filter(|e| {
                    let m = e.get("message").and_then(Value::as_str).unwrap_or("");
                    is_growth_fail(e) && (m.contains("需重新登录") || m.contains("无可用凭证"))
                })
                .count() as i64;
            let retryable = (failed - failed_permanent).max(0);
            if retryable > 0 {
                return Err(format!("WB 成长：{retryable} 个账号失败（稍后自动重试）"));
            }
            Ok(json!({ "ok": true, "failed": failed }))
        }
        // Trae JWT 定时续期（issue #27）：与 `--task-run trae-renew` 同款（lazy 48h 惰性门）
        "trae-renew" => crate::commands::accounts::renew_due_accounts_impl(st),
        // WorkBuddy token 兜底续期：与 `--task-run wb-renew` 同款（lazy 24h）
        "wb-renew" => Ok(super::wb_checkin::run_renew_only(st, 24)),
        // Buddy 国际版每日活跃打卡（阶段 3b）：抢轮次锁与 UI 路径互斥（skipped_busy
        // 幂等跳过同 wb-checkin）；失败返 Err 交 30 分钟冷却重试（401 可经网关自愈，
        // 不判永久剔除，见下方 T7/BL-5 定调注释）。
        // 审查修复（第6项；#14 改按账号粒度）：瞬时失效之外还存在不可自愈的永久失效
        //（封禁/会话失效/refresh 永久拒绝）——原「整轮连击」在混合轮（失效账号混在
        // 健康账号中）永不达阈值，全天 30min 重试空转；#14 起逐账号累计（内存态），
        // 任一账号连击达阈值当日暂停（复用整轮暂停标记）
        "buddy-ai-checkin" => {
            let Ok(_round) = crate::commands::workbuddy::try_acquire_buddy_ai_round() else {
                return Ok(json!({ "ok": true, "skipped": "已有 buddy-ai 打卡任务在执行中，本轮跳过", "skipped_busy": true }));
            };
            // 审查修复（第6项）：门禁置于轮次锁之后（对齐「通过开关与轮次锁检查后」）。
            // skipped_fail_streak 不设 skipped_busy → 走 tick 通用 mark_run 分支记
            // 「今日已处理」，后续 tick 经 Daily last_run_date 判定静默 continue——与
            // skipped_busy「不 mark_run 保留当日重试机会」的意图相反：此处正是要终止
            // 当日重试链，必须 mark_run（连击值在独立 kv，不受 mark_run 覆盖影响）
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let prev_streak = buddy_ai_checkin_streak(st, &today);
            if prev_streak >= BUDDY_AI_CHECKIN_FAIL_STREAK_LIMIT {
                return Ok(json!({
                    "ok": true,
                    "skipped": "连续多轮打卡失败（疑似凭证永久失效），当日暂停打卡，次日自动恢复",
                    "skipped_fail_streak": true,
                }));
            }
            // 审查 #14：内存态连击跨日清零（kv 门禁靠 date 字段惰性归零，内存态
            // 以轮次起点日期为准；date == 今日表示连击延续自本轮次之前）
            if buddy_acct_streaks.date != today {
                buddy_acct_streaks.date = today.clone();
                buddy_acct_streaks.map.clear();
            }
            let mut events: Vec<Value> = Vec::new();
            // runtime 透传（审查 #4）：打卡到账快照回写运行期 buddy_ai_pool
            let done = super::buddy_ai_checkin::run_checkin_round(st, runtime, &mut |ev| events.push(ev.clone()));
            // 审查 #15：空池空轮透传 skipped_empty_pool——tick 据此不 mark_run 且
            // 静默（当日新增账号保留调度补打机会）；连击统计无需更新（ok/already/
            // failed 全 0，buddy_ai_streak_next 原值返回），提前返回省一次判定
            if done.get("skipped_empty_pool").is_some() {
                return Ok(done);
            }
            let ok_count = done.get("ok").and_then(Value::as_i64).unwrap_or(0);
            let already_count = done.get("already").and_then(Value::as_i64).unwrap_or(0);
            let failed = done.get("failed").and_then(Value::as_i64).unwrap_or(0);
            // 审查 #14：逐账号连击——本轮事件中该账号既无 success 也无 already 则
            // +1，有则清零；任一账号达阈值 → 当日暂停（复用整轮暂停标记：kv 连击置
            // 为阈值，后续 tick 门禁 skipped_fail_streak 生效），日志注明失效账号
            let dead = buddy_ai_acct_streaks_apply(
                &mut buddy_acct_streaks.map,
                &events,
                BUDDY_AI_CHECKIN_FAIL_STREAK_LIMIT as u32,
            );
            if !dead.is_empty() {
                buddy_ai_checkin_streak_save(st, &today, BUDDY_AI_CHECKIN_FAIL_STREAK_LIMIT);
                let uids = dead.join(", ");
                crate::fs_utils::app_log(
                    &st.data_dir,
                    &format!(
                        "buddy-ai: 账号 {uids} 连续 {BUDDY_AI_CHECKIN_FAIL_STREAK_LIMIT} 轮打卡失败（疑似永久失效），当日暂停打卡，次日自动恢复"
                    ),
                );
                return Ok(json!({
                    "ok": true,
                    "skipped": format!("账号 {uids} 连续多轮打卡失败（疑似永久失效），当日暂停打卡，次日自动恢复"),
                    "skipped_fail_streak": true,
                }));
            }
            // 逐账号失败明细落 app_log（与 UI 路径 buddy_ai_checkin_start 同口径）：
            // 定时路径此前只记计数——「2 失败」无原因，180s 超时/401/ACP 挂死不可辨
            //（2026-10-17 09:20 定时轮 2/2 失败排查教训）
            if failed > 0 {
                crate::fs_utils::app_log(
                    &st.data_dir,
                    &format!(
                        "buddy-ai: 定时打卡失败明细——{}",
                        events
                            .iter()
                            .filter(|e| e.get("status").and_then(Value::as_str) == Some("fail"))
                            .map(|e| format!(
                                "{}({})",
                                e.get("name").and_then(Value::as_str).unwrap_or("?"),
                                e.get("message").and_then(Value::as_str).unwrap_or("")
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                );
            }
            // 审查复审修复（T7/BL-5 冲突定调）：T7 已把 fail 文案中「HTTP 401/403」
            // 统一改写为「凭证401/403」（buddy_ai_checkin.rs fail 闭包），此处旧
            // contains("HTTP 401") 过滤恒不命中，且原注释与 T7 语义矛盾。定调采纳
            // T7 语义：buddy-ai 凭证失效可经网关 401 自愈/续期轮换新值，30 分钟冷却
            // 重试可能成功——不再有「永久剔除」类失败，全部失败交冷却重试（一旦
            // 自愈即停；无凭证账号本就被花名册过滤，不会全天空转）
            let retryable = failed;
            if retryable > 0 {
                return Err(format!(
                    "buddy-ai 打卡：{ok_count} 成功 / {already_count} 已完成 / {retryable} 失败（稍后自动重试）"
                ));
            }
            Ok(done)
        }
        // F-80 Qoder 每日签到：与 `--task-run qoder-checkin` 同款；抢轮次锁与 UI 路径互斥
        "qoder-checkin" => {
            // 抢不到轮次锁（UI 路径正在签到）= 幂等跳过而非失败：返 Err 会被调度器记
            // 当日首败并推送「签到失败」误报通知（签到本身未失败，UI 路径会照常完成）
            let Ok(_round) = crate::tasks::qoder_checkin::try_acquire_qoder_round() else {
                // skipped_busy：tick 据此跳过 mark_run（保留当日后续 tick 重试机会）
                return Ok(json!({ "ok": true, "skipped": "已有 Qoder 签到任务在执行中，本轮跳过", "skipped_busy": true }));
            };
            let opts = super::qoder_checkin::QoderCheckinOpts::daily();
            // runtime 透传（issue #102 补缺）：签到奖励增量回写运行池余额
            let done = super::qoder_checkin::run_checkin_round(st, &opts, runtime, &mut |_| {});
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
        // F-80 Qoder 积分快照：与 `--task-run qoder-credits-snapshot` 同款（空池空转）；
        // runtime 传入网关运行态，成功行快照三键拨新进 qoder_pool（issue #102
        // 运行期保鲜，对齐 trae-credits-snapshot 的 issue #67 口径）
        "qoder-credits-snapshot" => super::qoder_credits::run_snapshot_task(st, runtime),
        // Qoder Token 定时刷新：与 `--task-run qoder-refresh` 同款（6h 周期，lazy 7h 惰性门；
        // 有账号刷新失败时返 Err，交由调度器 30 分钟冷却重试）
        "qoder-refresh" => super::qoder_refresh::run_task(st),
        // Qoder 会话流水定时合并：早于保留窗（settings.qoder_sessions_compact_days，
        // 解析与 CLI 手工触发同源 super::qoder_sessions_compact_days，审查 F12）的
        // 请求行按 (date, uid, preview) 合并单行（幂等自愈；纯本地 SQLite 无网络
        // 依赖，失败返 Err 交 30 分钟冷却重试）
        "qoder-sessions-compact" => {
            let days = super::qoder_sessions_compact_days(st);
            let (merged, deleted) = crate::store::docs::qoder_sessions_compact(
                &crate::store::db(&st.data_dir),
                days,
            )?;
            Ok(json!({ "ok": true, "merged": merged, "deleted": deleted, "days": days }))
        }
        // Qoder 模型目录同步：与 `--task-run qoder-catalog-sync` 同款（真签名拉
        // model/list → adopt_remote；空池/无凭证空转，网络失败返 Err 冷却重试）
        "qoder-catalog-sync" => super::qoder_catalog::run_task(st),
        // Buddy 国际版凭证兜底续期（lazy 48h）：无临期账号空转，全失败返 Err 冷却重试。
        // 审查 #16：Intl 刷新失败有意不标 needs_relogin（防误杀真实凭证），永久失效
        // 账号会每日 + 每 30 分钟重试——调度侧熔断：连续 3 轮全败当日暂停（kv 与打卡
        // 连击同款 {streak, date} 文档，跨日自动归零），成功/空转轮清零连击
        "buddy-ai-renew" => {
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let prev = day_scoped_streak(st, BUDDY_AI_RENEW_STREAK_KEY, &today);
            if prev >= BUDDY_AI_RENEW_FAIL_STREAK_LIMIT {
                return Ok(json!({
                    "ok": true,
                    "skipped": "buddy-ai 续期连续多轮全部失败，当日暂停续期，次日自动恢复",
                    "skipped_fail_streak": true,
                }));
            }
            match crate::commands::workbuddy::buddy_ai_renew_task(st) {
                Err(e) => {
                    let next = prev + 1;
                    day_scoped_streak_save(st, BUDDY_AI_RENEW_STREAK_KEY, &today, next);
                    if next >= BUDDY_AI_RENEW_FAIL_STREAK_LIMIT {
                        fs_utils::app_log(
                            &st.data_dir,
                            &format!(
                                "buddy-ai: 凭证续期连续 {next} 轮全部失败（{e}），当日暂停续期，次日自动恢复"
                            ),
                        );
                    }
                    Err(e)
                }
                Ok(v) => {
                    // 成功（含「无临期账号」空转轮）清零连击；值不变不写库
                    if prev != 0 {
                        day_scoped_streak_save(st, BUDDY_AI_RENEW_STREAK_KEY, &today, 0);
                    }
                    Ok(v)
                }
            }
        }
        // Buddy 国际版积分数据同步（get-user-resource 全池取数 → 池回写 + 快照）：
        // 空池自然空转；runtime 传网关运行态（运行池三键拨新，issue #109 同类）。
        // 顺带官网用量聚合刷新（force 跳过 10min 缓存，国内 wb-credits-snapshot 同款）——
        // 聚合拉取随路落盘请求级行明细（buddy_ai_usage_sessions_cache），
        // 会话明细 Tab 的定时保鲜数据源；统计尽力而为不上抛
        "buddy-ai-credits-sync" => {
            let credits = crate::commands::workbuddy::buddy_ai_credits_sync_impl(st, runtime)?;
            let usage = match crate::commands::workbuddy::buddy_ai_usage_official_all_impl(st, true) {
                Ok(_) => "已刷新",
                Err(_) => "跳过（无可用凭证或拉取失败）",
            };
            // 本地 Token 统计重扫（对齐国内 wb-credits-snapshot 配套）：fresh=false 走
            // 10min 结果缓存——每日任务时刻缓存必然过期，实际执行重扫；过期短缓存直接命中
            let token_files = crate::commands::workbuddy_stats::buddy_ai_token_stats_impl(st, false)
                .get("files_scanned")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Ok(json!({
                "ok": true,
                "accounts": credits.len(),
                "fetched": credits.iter().filter(|a| a.get("credits_balance").map(|b| !b.is_null()).unwrap_or(false)).count(),
                "usage": usage,
                "token_files": token_files,
            }))
        }
        // Buddy 国际版模型目录同步（/v3/config 直连 → 动态替换）：空池/无凭证静默跳过
        "buddy-ai-catalog-sync" => {
            let pool = crate::commands::workbuddy::buddy_ai_pool_size(st);
            if pool == 0 {
                Ok(json!({ "ok": true, "skipped": "无 buddy-ai 账号" }))
            } else {
                crate::commands::workbuddy::buddy_ai_catalog_sync_impl(st)
                    .map(|n| json!({ "ok": true, "models": n }))
            }
        }
        // 豆包会话每日续期：与 `--task-run doubao-keepalive` 同款
        "doubao-keepalive" => {
            let sink = crate::switcher::CliSink::new(&st.data_dir);
            match crate::switcher::run_action(
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
            ) {
                Ok(_) => Ok(json!({ "ok": true })),
                // 审查修复（M5/M15）：互斥忙态特判转 skipped_busy（对齐 wb-checkin/
                // qoder/buddy-ai-checkin/wb-growth 等任务的幂等跳过口径）——切换互斥忙
                // （action_gate 被占）是幂等跳过而非任务失败，原样上抛会按失败收场，
                // 当日首败误推 Server酱；带 skipped_busy 标志让调度器不 mark_run，
                // 保留当日后续 tick 重试机会
                Err(e) if e.contains("已有切换/备份操作进行中") => Ok(json!({ "ok": true, "skipped": "已有切换/备份操作进行中，本轮跳过", "skipped_busy": true })),
                Err(e) => Err(e),
            }
        }
        // 豆包会员额度每日巡检：与 `--task-run doubao-quota` 同款
        "doubao-quota" => super::doubao_quota::run_batch(st),
        // WorkBuddy 积分与 Token 数据同步（看板定时同步）：积分 fresh 拉取+池回写+快照为主目标
        // （空池自然空转不报错）；顺带 Token 统计重扫（本地增量缓存）与官方请求用量刷新——
        // 统计尽力而为不上抛（页面打开时本就有各自缓存/降级链路）；用量刷全账号聚合缓存
        // （看板 Dashboard 唯一消费源 workbuddy_usage_official_all_cache，force 跳过 10min
        // 缓存；原单账号缓存分支前端无调用方，属刷错目标，issue #61 同类缺口一并修复）
        "wb-credits-snapshot" => {
            // runtime 句柄：网关运行池三键拨新（issue #109 同类，CLI 进程 runtime=None 跳过）
            let parsed = crate::commands::workbuddy::wb_credits_snapshot_task(st, runtime)?;
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

// ── 审查修复（第6项）+ #14/#16：buddy-ai 打卡/续期失败连击 ──────────────────
// scheduler_state 的 fail_streak 服务于退避冷却且被 mark_run 整体覆盖清零，无法表达
// 「跨冷却周期的连续失败」，故用独立 kv 文档承载（阈值见
// BUDDY_AI_CHECKIN_FAIL_STREAK_LIMIT / BUDDY_AI_RENEW_FAIL_STREAK_LIMIT）。
// #14 起打卡连击改为逐账号内存态累计（BuddyAiAcctFailStreaks），kv 文档退化为
// 「当日暂停闩锁」：任一账号连击达阈值时置为 LIMIT，后续 tick 门禁据此跳过

/// 逐账号连击内存态（审查 #14）：uid → 连续失败轮数；date 门槛跨日清零
///（内存态无日期字段，以最近一次打卡轮的日期为准；仅调度线程访问，无需加锁）
#[derive(Default)]
struct BuddyAiAcctFailStreaks {
    date: String,
    map: std::collections::HashMap<String, u32>,
}

/// 通用当日连击 kv：`{ "streak": 连续失败次数, "date": 计数日期 }`——计数与日期
/// 同文档单次 kv_set 写入（无半写分叉）；跨日（date ≠ 今日）读时惰性归零
const BUDDY_AI_CHECKIN_STREAK_KEY: &str = "buddy_ai_checkin_fail_streak";
/// 审查 #16：buddy-ai-renew 全败轮连击 kv 键（与打卡连击同款文档，独立计数互不干扰）
const BUDDY_AI_RENEW_STREAK_KEY: &str = "buddy_ai_renew_fail_streak";

/// 读当日连击：date ≠ 今日（跨日/无记录/损坏）→ 从 0 起；date == 今日 → 计数值
fn day_scoped_streak(st: &AppState, kv_key: &str, today: &str) -> i64 {
    let v: Value = crate::store::db(&st.data_dir).kv_get(kv_key);
    if v.get("date").and_then(Value::as_str) != Some(today) {
        return 0;
    }
    v.get("streak").and_then(Value::as_i64).unwrap_or(0).max(0)
}

/// 持久化当日连击（计数与日期同一把 kv 写入）。落库失败静默：连击丢失仅回到
/// 多试几轮的自愈起点，无重试风暴/重复执行风险（严重度远低于 write_entry 的 C6）
fn day_scoped_streak_save(st: &AppState, kv_key: &str, today: &str, streak: i64) {
    let _ = crate::store::db(&st.data_dir).kv_set(
        kv_key,
        &json!({ "streak": streak, "date": today }),
    );
}

/// buddy-ai 打卡连击 kv 门禁读写包装（沿用旧键名，升级前的持久化数据自然延续）
fn buddy_ai_checkin_streak(st: &AppState, today: &str) -> i64 {
    day_scoped_streak(st, BUDDY_AI_CHECKIN_STREAK_KEY, today)
}

fn buddy_ai_checkin_streak_save(st: &AppState, today: &str, streak: i64) {
    day_scoped_streak_save(st, BUDDY_AI_CHECKIN_STREAK_KEY, today, streak);
}

/// 纯函数（审查 #14）：按本轮事件聚合逐账号连击表，返回达阈值的失效账号 uid。
/// 事件中该账号既无 success 也无 already → +1，有则清零；不在本轮事件中的账号
/// 不动。原「整轮连击任一成功清零」语义（buddy_ai_streak_next）在混合轮失效：
/// 永久失效账号混在健康账号中时连击永不累计，全天 30 分钟重试空转、当日暂停
/// 永不触发——故改按账号粒度（独立纯函数便于单测）
fn buddy_ai_acct_streaks_apply(
    map: &mut std::collections::HashMap<String, u32>,
    events: &[Value],
    limit: u32,
) -> Vec<String> {
    let mut dead: Vec<String> = Vec::new();
    for e in events {
        let Some(uid) = e.get("id").and_then(Value::as_str) else {
            continue;
        };
        if uid.is_empty() {
            continue;
        }
        let ok = matches!(
            e.get("status").and_then(Value::as_str),
            Some("success") | Some("already")
        );
        let next = buddy_ai_acct_streak_next(map.get(uid).copied().unwrap_or(0), ok);
        if next == 0 {
            map.remove(uid);
        } else {
            map.insert(uid.to_string(), next);
            if next >= limit {
                dead.push(uid.to_string());
            }
        }
    }
    dead
}

/// 纯函数（审查 #14）：单账号连击推导——本轮 success/already 清零，否则 +1
fn buddy_ai_acct_streak_next(prev: u32, ok: bool) -> u32 {
    if ok {
        0
    } else {
        prev + 1
    }
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

/// 结果 JSON → 单行摘要（签到/打卡轮次取 ok/already/failed 计数，其余截断展示；
/// buddy-ai 活跃打卡用「已打」，国内各平台签到仍用「已签」）
fn summarize(v: &Value, task_key: &str) -> String {
    if v.is_object() {
        let get = |k: &str| v.get(k).and_then(Value::as_i64);
        if let (Some(o), Some(a), Some(f)) = (get("ok"), get("already"), get("failed")) {
            let unit = if task_key == "buddy-ai-checkin" { "已打" } else { "已签" };
            return format!("成功 {o}，{unit} {a}，失败 {f}");
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
                SchedPlan::EveryDays(d) => format!("每{d}天"),
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
            buddy_ai_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
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

    /// Qoder 会话流水定时合并：默认开 + 每 6 小时（compact 档，hhmm 不参与判定）；
    /// 显式关闭后 Skip
    #[test]
    fn qoder_sessions_compact_plan() {
        let st = temp_state("qcompact");
        let t = || TASKS.iter().find(|t| t.key == "qoder-sessions-compact").unwrap();
        assert!(matches!(sched_plan(&st, t()), SchedPlan::EveryHours(6)));
        let set = |v: Value| crate::store::db(&st.data_dir).kv_set("app_settings", &v).unwrap();
        set(json!({ "qoder_sessions_compact_enabled": false }));
        assert!(matches!(sched_plan(&st, t()), SchedPlan::Skip));
        set(json!({ "qoder_sessions_compact_enabled": true, "qoder_sessions_compact_hhmm": "03:10" }));
        assert!(matches!(sched_plan(&st, t()), SchedPlan::EveryHours(6)), "hhmm 不参与 EveryHours 判定");
    }

    /// 显式关闭：enabled() 返回 false（环境配置页开关关闭后调度器跳过）
    #[test]
    fn enabled_false_when_disabled() {
        let st = temp_state("off");
        seed_settings(&st, false, "09:00");
        assert!(!enabled(&st, "trae-renew"));
    }

    /// 看板同步/模型同步的调度计划：默认 every_hours 每2小时（issue #82）；
    /// hourly/daily 生效；off 关闭；非法值回退 every_hours；间隔小时数可手工设置并 clamp；
    /// 模型同步默认开、时刻可改、关闭后 Skip
    #[test]
    fn sched_plan_credits_and_models() {
        let st = temp_state("plan");
        let wb_credits = || TASKS.iter().find(|t| t.key == "wb-credits-snapshot").unwrap();
        let trae_models = || TASKS.iter().find(|t| t.key == "trae-models-sync").unwrap();
        let set = |v: Value| crate::store::db(&st.data_dir).kv_set("app_settings", &v).unwrap();
        // 默认（无 kv）：every_hours 每 2 小时；模型同步仍 daily + 内置时刻
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::EveryHours(2)), "Buddy 默认每2小时");
        assert!(matches!(sched_plan(&st, trae_models()), SchedPlan::Daily(h) if h == "05:40"));
        // hourly 生效
        set(json!({ "wb_credits_sync_mode": "hourly" }));
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::Hourly));
        // every_hours 显式 + 手工间隔
        set(json!({ "wb_credits_sync_mode": "every_hours", "wb_credits_sync_hours": "6" }));
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::EveryHours(6)));
        // 间隔非法回退 2；越界经 state clamp 到 24
        set(json!({ "wb_credits_sync_mode": "every_hours", "wb_credits_sync_hours": "abc" }));
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::EveryHours(2)));
        set(json!({ "wb_credits_sync_mode": "every_hours", "wb_credits_sync_hours": "99" }));
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::EveryHours(24)));
        // daily 模式走配置时刻
        set(json!({ "wb_credits_sync_mode": "daily" }));
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::Daily(h) if h == "23:30"));
        // off 关闭（enabled=false → Skip）
        set(json!({ "wb_credits_sync_mode": "off" }));
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::Skip));
        // 非法模式回退 every_hours（credits_sync_mode 兜底）
        set(json!({ "wb_credits_sync_mode": "weekly" }));
        assert!(matches!(sched_plan(&st, wb_credits()), SchedPlan::EveryHours(2)));
        // Trae 看板同步（issue #82）：默认 every_hours——积分快照与消耗明细（trae-usage-sync）均每2小时
        let trae_credits = || TASKS.iter().find(|t| t.key == "trae-credits-snapshot").unwrap();
        let trae_usage = || TASKS.iter().find(|t| t.key == "trae-usage-sync").unwrap();
        assert!(matches!(sched_plan(&st, trae_credits()), SchedPlan::EveryHours(2)), "Trae 默认每2小时");
        assert!(
            matches!(sched_plan(&st, trae_usage()), SchedPlan::EveryHours(2)),
            "消耗明细跟随 every_hours"
        );
        // daily 模式：积分快照走配置时刻（23:40），消耗明细内置 23:50 错峰
        set(json!({ "trae_credits_sync_mode": "daily" }));
        assert!(matches!(sched_plan(&st, trae_credits()), SchedPlan::Daily(h) if h == "23:40"));
        assert!(matches!(sched_plan(&st, trae_usage()), SchedPlan::Daily(h) if h == "23:50"));
        // Trae 手工间隔
        set(json!({ "trae_credits_sync_mode": "every_hours", "trae_credits_sync_hours": "3" }));
        assert!(matches!(sched_plan(&st, trae_usage()), SchedPlan::EveryHours(3)));
        // off 同时关闭积分快照与消耗明细
        set(json!({ "trae_credits_sync_mode": "off" }));
        assert!(matches!(sched_plan(&st, trae_credits()), SchedPlan::Skip));
        assert!(matches!(sched_plan(&st, trae_usage()), SchedPlan::Skip));
        // 非法模式回退 Trae 默认 every_hours
        set(json!({ "trae_credits_sync_mode": "weekly" }));
        assert!(matches!(sched_plan(&st, trae_usage()), SchedPlan::EveryHours(2)));
        // Qoder 积分数据同步（同款四模式）：默认（无 kv mode 字段→state 按旧 enabled=true 派生）
        // every_hours 每2小时；hourly/daily/every_hours/off 显式生效
        let qoder_credits = || TASKS.iter().find(|t| t.key == "qoder-credits-snapshot").unwrap();
        assert!(matches!(sched_plan(&st, qoder_credits()), SchedPlan::EveryHours(2)), "Qoder 默认每2小时");
        set(json!({ "qoder_credits_sync_mode": "hourly" }));
        assert!(matches!(sched_plan(&st, qoder_credits()), SchedPlan::Hourly));
        set(json!({ "qoder_credits_sync_mode": "every_hours", "qoder_credits_sync_hours": "5" }));
        assert!(matches!(sched_plan(&st, qoder_credits()), SchedPlan::EveryHours(5)));
        set(json!({ "qoder_credits_sync_mode": "daily" }));
        assert!(matches!(sched_plan(&st, qoder_credits()), SchedPlan::Daily(h) if h == "23:40"));
        set(json!({ "qoder_credits_sync_mode": "daily", "qoder_credits_sync_hhmm": "06:30" }));
        assert!(matches!(sched_plan(&st, qoder_credits()), SchedPlan::Daily(h) if h == "06:30"));
        set(json!({ "qoder_credits_sync_mode": "off" }));
        assert!(matches!(sched_plan(&st, qoder_credits()), SchedPlan::Skip));
        // 旧数据迁移：无 mode 字段 + enabled=false → state 派生 off（关闭态不丢失）
        set(json!({ "qoder_credits_sync_enabled": false }));
        assert!(matches!(sched_plan(&st, qoder_credits()), SchedPlan::Skip), "旧关闭态迁移为 off");
        // 模型同步时刻覆盖 + 显式关闭
        set(json!({ "trae_models_sync_hhmm": "06:10" }));
        assert!(matches!(sched_plan(&st, trae_models()), SchedPlan::Daily(h) if h == "06:10"));
        set(json!({ "trae_models_sync_enabled": false }));
        assert!(matches!(sched_plan(&st, trae_models()), SchedPlan::Skip));
    }

    /// 看板联动平台映射（issue #61 + 审查修复）：四个看板数据任务分别 emit
    /// buddy/trae/qoder/buddy-ai；凭证刷新与签到类任务不属于看板数据同步，不得误 emit
    #[test]
    fn board_platform_maps_board_tasks_only() {
        assert_eq!(board_platform("wb-credits-snapshot"), Some("buddy"));
        assert_eq!(board_platform("trae-credits-snapshot"), Some("trae"));
        assert_eq!(board_platform("trae-usage-sync"), Some("trae"));
        assert_eq!(board_platform("qoder-credits-snapshot"), Some("qoder"));
        assert_eq!(board_platform("buddy-ai-credits-sync"), Some("buddy-ai"));
        assert_eq!(board_platform("qoder-refresh"), None);
        assert_eq!(board_platform("qoder-checkin"), None);
        assert_eq!(board_platform("buddy-ai-checkin"), None);
        assert_eq!(board_platform("buddy-ai-renew"), None);
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

    // ── 审查修复（第6项）+ #14/#16：buddy-ai 打卡/续期失败连击 ──────────────────

    /// 连击纯函数（#14 新语义）：单账号粒度——本轮 success/already 清零，否则 +1。
    /// 原 buddy_ai_streak_next（整轮粒度、任一成功即清零）单测固化「混合轮清零」，
    /// 但该语义使永久失效账号混在健康账号中时连击永不达阈值（全天 30 分钟重试
    /// 空转、当日暂停永不触发）——按账号粒度重写：混合轮中失效账号照常累计，
    /// 健康账号照常清零，互不影响
    #[test]
    fn buddy_ai_acct_streak_next_rules() {
        // 失败：+1
        assert_eq!(buddy_ai_acct_streak_next(0, false), 1);
        assert_eq!(buddy_ai_acct_streak_next(2, false), 3);
        // 本轮 success/already：清零
        assert_eq!(buddy_ai_acct_streak_next(2, true), 0);
        assert_eq!(buddy_ai_acct_streak_next(0, true), 0);
    }

    /// 连击聚合（#14）：混合轮中失效账号逐轮累计至阈值并列入 dead，健康账号
    /// 同轮清零互不影响；连击中断（恢复成功）后重新从 0 累计
    #[test]
    fn buddy_ai_acct_streaks_apply_mixed_rounds() {
        let mut map = std::collections::HashMap::new();
        let ev = |id: &str, status: &str| {
            json!({ "id": id, "status": status })
        };
        // 第 1 轮：A 失败、B 成功 → A=1、B 移除
        let dead = buddy_ai_acct_streaks_apply(
            &mut map,
            &[ev("A", "fail"), ev("B", "success")],
            BUDDY_AI_CHECKIN_FAIL_STREAK_LIMIT as u32,
        );
        assert!(dead.is_empty());
        assert_eq!(map.get("A"), Some(&1));
        assert!(!map.contains_key("B"), "健康账号不留在连击表");
        // 第 2 轮：A 再失败 → A=2；同轮 C already（混合轮失效账号仍累计——旧
        // 整轮语义在此轮清零，正是 #14 修复点）
        let dead = buddy_ai_acct_streaks_apply(
            &mut map,
            &[ev("A", "fail"), ev("C", "already")],
            BUDDY_AI_CHECKIN_FAIL_STREAK_LIMIT as u32,
        );
        assert!(dead.is_empty());
        assert_eq!(map.get("A"), Some(&2));
        // 第 3 轮：A 三连败达阈值 → 进 dead；B 成功事件照常清零
        let dead = buddy_ai_acct_streaks_apply(
            &mut map,
            &[ev("A", "fail"), ev("B", "success")],
            BUDDY_AI_CHECKIN_FAIL_STREAK_LIMIT as u32,
        );
        assert_eq!(dead, vec!["A".to_string()]);
        // 恢复成功：连击清零，不再进 dead
        let dead = buddy_ai_acct_streaks_apply(&mut map, &[ev("A", "success")], 3);
        assert!(dead.is_empty());
        assert!(!map.contains_key("A"));
        // 事件缺 id / 空 id：跳过不计
        let dead = buddy_ai_acct_streaks_apply(
            &mut map,
            &[json!({ "status": "fail" }), ev("", "fail")],
            1,
        );
        assert!(dead.is_empty());
    }

    /// 连击 kv 持久化 + 跨日重置：date == 今日读回计数值；date ≠ 今日（跨日）从 0 起；
    /// 无记录/损坏文档兜底 0（打卡与 renew 各自独立键位，#16）
    #[test]
    fn buddy_ai_checkin_streak_persist_and_day_reset() {
        let st = temp_state("buddy_streak");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        // 无记录：0
        assert_eq!(buddy_ai_checkin_streak(&st, &today), 0);
        // 写入后同日读回；计数与日期同一把写入（文档双字段齐备）
        buddy_ai_checkin_streak_save(&st, &today, 2);
        assert_eq!(buddy_ai_checkin_streak(&st, &today), 2);
        let raw: Value = crate::store::db(&st.data_dir).kv_get(BUDDY_AI_CHECKIN_STREAK_KEY);
        assert_eq!(raw.get("streak").and_then(Value::as_i64), Some(2));
        assert_eq!(raw.get("date").and_then(Value::as_str), Some(today.as_str()));
        // 跨日：日期 ≠ 今日 → 从 0 起（昨日残留不计入今日）
        buddy_ai_checkin_streak_save(&st, "2026-01-01", 3);
        assert_eq!(buddy_ai_checkin_streak(&st, &today), 0);
        // 损坏文档（非 JSON）：兜底 0
        crate::store::db(&st.data_dir)
            .kv_set_raw(BUDDY_AI_CHECKIN_STREAK_KEY, "not-json")
            .unwrap();
        assert_eq!(buddy_ai_checkin_streak(&st, &today), 0);
    }

    /// 调度分支门禁：连续 ≥3 整轮全败 → run_task 返回 skipped_fail_streak 当日暂停，
    /// 不再执行打卡轮（门禁在 run_checkin_round 之前返回，测试无网络调用）
    #[test]
    fn buddy_ai_checkin_gate_skips_at_streak_limit() {
        let st = temp_state("buddy_gate");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        buddy_ai_checkin_streak_save(&st, &today, BUDDY_AI_CHECKIN_FAIL_STREAK_LIMIT);
        let v = run_task("buddy-ai-checkin", &st, None, &mut BuddyAiAcctFailStreaks::default())
            .expect("门禁跳过应为 Ok");
        assert_eq!(v.get("skipped_fail_streak").and_then(Value::as_bool), Some(true));
        assert!(
            v.get("skipped").and_then(Value::as_str).unwrap_or("").contains("当日暂停"),
            "跳过文案应说明当日暂停语义"
        );
    }

    /// 审查 #16：buddy-ai-renew 全败轮连击熔断——Err 轮逐次累计，达阈值后门禁
    /// 返回 skipped_fail_streak 当日暂停（不执行续期轮）；成功/空转轮清零连击
    #[test]
    fn buddy_ai_renew_streak_circuit_breaker() {
        let st = temp_state("buddy_renew_cb");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        // 前两轮全败（模拟：直接写连击值，覆盖 Err 分支的 save 语义）
        day_scoped_streak_save(&st, BUDDY_AI_RENEW_STREAK_KEY, &today, 2);
        // 第 3 轮全败：Err 分支累计至 3（门禁阈值）——此后暂停
        //（不实际执行续期轮：空池 renew 返回 Ok(skipped)，此处仅验连击语义）
        day_scoped_streak_save(&st, BUDDY_AI_RENEW_STREAK_KEY, &today, 3);
        assert_eq!(day_scoped_streak(&st, BUDDY_AI_RENEW_STREAK_KEY, &today), 3);
        // 达阈值 → 门禁跳过（Ok + skipped_fail_streak，不执行任务体）
        let v = run_task("buddy-ai-renew", &st, None, &mut BuddyAiAcctFailStreaks::default())
            .expect("熔断门禁应为 Ok");
        assert_eq!(v.get("skipped_fail_streak").and_then(Value::as_bool), Some(true));
        // 成功/空转轮清零：Ok 分支 prev != 0 时写 0
        day_scoped_streak_save(&st, BUDDY_AI_RENEW_STREAK_KEY, &today, 0);
        assert_eq!(day_scoped_streak(&st, BUDDY_AI_RENEW_STREAK_KEY, &today), 0);
        // 与打卡连击键位隔离：renew 连击不影响打卡门禁读数
        day_scoped_streak_save(&st, BUDDY_AI_RENEW_STREAK_KEY, &today, 3);
        assert_eq!(buddy_ai_checkin_streak(&st, &today), 0);
    }
}
