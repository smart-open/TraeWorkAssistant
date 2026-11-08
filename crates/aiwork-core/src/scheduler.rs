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
//! ## Buddy 国际版任务（移植 main f5f2cd3..61cc245）
//!
//! - `buddy-ai-checkin`（09:20，默认关）：每日活跃打卡轮次（settings.buddy_ai_checkin_enabled）；
//! - `buddy-ai-renew`（10:35，默认开）：凭证兜底续期（lazy 48h）；
//! - `buddy-ai-credits-sync`（23:45，默认 daily）：积分数据同步，五模式 settings 驱动
//!   （off/hourly/every_hours/every_days/daily，新增 SchedPlan::EveryDays 档；docker
//!   适配——模式走 settings 字段不入 scheduler_cfg.task_modes/HOURLY_CAPABLE）；
//! - `buddy-ai-catalog-sync`（05:55，默认开）：模型目录同步（/v3/config 直连）。
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
    // F-80 Qoder 积分数据同步（移植 main）：与 Buddy/Trae 同款四模式（off/hourly/
    // daily/every_hours，scheduler_cfg.task_modes 可改；daily 用内置 23:40）；
    // 空池静默空转不计失败
    SchedTask { key: "qoder-credits-snapshot", name: "Qoder 积分数据同步", hhmm: "23:40" },
    // F-80 Qoder 模型目录同步（移植 main p3-3）：真签名拉 model/list → adopt_remote
    // 替换缓存；空池/无凭证静默跳过不计失败，时刻经 scheduler_cfg.task_times 可改
    SchedTask { key: "qoder-catalog-sync", name: "Qoder 模型目录同步", hhmm: "05:50" },
    // F-80 Qoder 凭证 6h 兜底刷新（移植 main）：固定每 6 小时（hhmm 不参与判定）；
    // 空池空转；与 UI 路径经跨进程锁互斥（任务内部处理）
    SchedTask { key: "qoder-refresh", name: "Qoder 凭证定时刷新", hhmm: "06:00" },
    // Qoder 会话流水定时合并（移植 main）：每 6 小时把「保留窗之前」（默认 3 天，
    // QODER_SESSIONS_COMPACT_AFTER_DAYS）的请求级流水按 (date, uid, preview) 合并为
    // 单行（幂等自愈，积分合计不变；历史行 session_id 为随机值，按 preview 归并才
    // 对旧行生效）。展示层另有读时聚合（qoder_sessions_query 两遍归并），本任务只
    // 负责压缩历史行体积
    SchedTask { key: "qoder-sessions-compact", name: "Qoder 会话流水定时合并", hhmm: "04:30" },
    // ── Buddy 国际版四任务（移植 main f5f2cd3..61cc245）────────────────────────
    // Buddy 国际版每日活跃打卡：网页会话 + 沙箱 ACP 六步链跑完一轮 → 服务端自动
    // 入账积分（无 claim 端点）；开关独立（buddy_ai_checkin_enabled，默认关）
    SchedTask { key: "buddy-ai-checkin", name: "Buddy 国际版每日活跃打卡", hhmm: "09:20" },
    // Buddy 国际版凭证兜底续期（对齐国内 wb-renew）：lazy 48h——AT 到期前 48h 内
    // 才真正刷新；开关独立（buddy_ai_renew_enabled，默认开）
    SchedTask { key: "buddy-ai-renew", name: "Buddy 国际版凭证兜底续期", hhmm: "10:35" },
    // Buddy 国际版积分数据同步（对齐国内看板同步口径）：get-user-resource 全池取数
    // → 池三键回写 + 当日快照落库；五模式 settings 驱动（off/hourly/every_hours/
    // every_days/daily，不入 scheduler_cfg.task_modes/HOURLY_CAPABLE，docker 适配）；
    // daily 用内置 23:45（task_times 可覆盖），错峰 wb 23:30 / trae 23:40
    SchedTask { key: "buddy-ai-credits-sync", name: "Buddy 国际版积分数据同步", hhmm: "23:45" },
    // Buddy 国际版模型目录同步：/v3/config 直连拉取（动态目录替换，与 API 服务启动
    // 挂点互补）；开关独立（buddy_ai_catalog_sync_enabled，默认开），05:55 错峰
    SchedTask { key: "buddy-ai-catalog-sync", name: "Buddy 国际版模型目录同步", hhmm: "05:55" },
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
/// 单轮 tick 内多个到期任务（典型为启动补跑积压）之间的错峰间隔（issue #84 C1）：
/// 重任务（快照/用量同步）背靠背连跑会形成分钟级 CPU 爆发（实测 103s），任务间
/// 睡眠错开峰值，给系统留出处理其他负载的余量。
const TICK_STAGGER_SECS: u64 = 30;
/// hourly 模式节流：距上次成功执行 ≥1h 才再跑（失败走退避冷却，不受此门限制）
const HOURLY_INTERVAL_MS: i64 = 60 * 60_000;

/// qoder-refresh 档位（移植 main）：每 6 小时兜底刷新一次凭证（设计 v1.3 §M4；
/// 客户端 token 惰性窗 7h > 6h 调度间隔，任一 tick 必落窗内，确保过期令牌被续）
const REFRESH_INTERVAL_HOURS: i64 = 6;

/// qoder-sessions-compact 档位（移植 main）：每 6 小时合并一次「当天之前」的会话
/// 流水（当天数据由查询层读时聚合即时呈现，无等待诉求）
const COMPACT_INTERVAL_HOURS: i64 = 6;

/// 可配置「每小时/每N小时」模式的任务（看板数据同步类，移植 main@c8e855b 的 credits 语义）：
/// 仅这些任务接受 scheduler_cfg.task_modes 的 hourly/every_hours 值，其余任务恒为每日模式
const HOURLY_CAPABLE: &[&str] =
    &["wb-credits-snapshot", "trae-credits-snapshot", "trae-usage-sync", "qoder-credits-snapshot"];

/// 单任务的生效调度计划：Skip=关闭；Daily=每日 HH:MM（到点+当日未跑，启动补跑）；
/// Hourly=每小时（距上次成功执行 ≥1h，无记录=首次立即跑）；
/// EveryHours(h)=每 h 小时（距上次成功执行 ≥h·1h，无记录=首次立即跑）；
/// EveryDays(d)=每 d 天（距上次成功执行 ≥d·24h，无记录=首次立即跑；
/// 移植 main buddy-ai 积分同步 every_days 档）
#[derive(Debug)]
enum SchedPlan {
    Skip,
    Daily(String),
    Hourly,
    EveryHours(i64),
    EveryDays(i64),
}

/// 单任务调度计划：合并启用判定与模式/时刻解析（tick 与 scheduler_status 共用；
/// cfg 为整轮一次性读出的 scheduler_cfg，避免每任务重复读 db；
/// st 用于外部设置语义门与 buddy-ai settings 驱动模式（移植 main））
fn sched_plan(st: &AppState, cfg: &Value, t: &SchedTask) -> SchedPlan {
    if !enabled_from(cfg, t.key) || !extra_enabled(st, t.key) {
        return SchedPlan::Skip;
    }
    // Qoder 凭证兜底刷新（移植 main）：固定每 6 小时，hhmm 不参与判定
    if t.key == "qoder-refresh" {
        return SchedPlan::EveryHours(REFRESH_INTERVAL_HOURS);
    }
    // Qoder 会话流水定时合并（移植 main）：固定每 6 小时，hhmm 不参与判定
    if t.key == "qoder-sessions-compact" {
        return SchedPlan::EveryHours(COMPACT_INTERVAL_HOURS);
    }
    // Buddy 国际版积分数据同步（移植 main）：五模式 settings 驱动（docker 适配——
    // 不入 scheduler_cfg.task_modes/HOURLY_CAPABLE）；off 已被 extra_enabled 门拦截
    if t.key == "buddy-ai-credits-sync" {
        let s = st.settings();
        match s.buddy_ai_credits_sync_mode.trim() {
            "hourly" => SchedPlan::Hourly,
            "every_hours" => SchedPlan::EveryHours(
                s.buddy_ai_credits_sync_hours.trim().parse::<i64>().unwrap_or(2).clamp(1, 24),
            ),
            "every_days" => SchedPlan::EveryDays(
                s.buddy_ai_credits_sync_days.trim().parse::<i64>().unwrap_or(1).clamp(1, 30),
            ),
            _ => SchedPlan::Daily(effective_hhmm_from(st, cfg, t)),
        }
    } else if HOURLY_CAPABLE.contains(&t.key) {
        match effective_task_mode_from(cfg, t.key).as_str() {
            // 看板数据同步：按模式分档——hourly 每小时节流 / every_hours 每 N 小时节流
            // （N 可手工设置，默认 2）/ daily（含非法值）按每日时刻
            "hourly" => SchedPlan::Hourly,
            "every_hours" => SchedPlan::EveryHours(effective_task_hours_from(cfg, t.key)),
            _ => SchedPlan::Daily(effective_hhmm_from(st, cfg, t)),
        }
    } else {
        SchedPlan::Daily(effective_hhmm_from(st, cfg, t))
    }
}

/// 看板数据任务的缺省执行模式（task_modes 缺省/非法值兜底，对齐 main
/// credits_sync_mode 语义——issue #82：会话明细需随账号定时落盘，
/// Buddy/Trae 看板数据默认每 2 小时）
fn fallback_task_mode(_key: &str) -> &'static str {
    "every_hours"
}

/// 任务生效执行模式（HOURLY_CAPABLE 任务用）：scheduler_cfg.task_modes 显式
/// 配置优先（hourly/daily/every_hours），缺省或非法值回落 fallback_task_mode
fn effective_task_mode_from(cfg: &Value, key: &str) -> String {
    match task_modes_from(cfg).get(key).map(String::as_str) {
        Some(m @ ("hourly" | "daily" | "every_hours")) => m.to_string(),
        _ => fallback_task_mode(key).to_string(),
    }
}

/// every_hours 模式的间隔小时数（scheduler_cfg.task_hours，key → 小时数）：
/// 缺省/非法回退 2，越界 clamp 1..=24
fn effective_task_hours_from(cfg: &Value, key: &str) -> i64 {
    cfg.get("task_hours")
        .and_then(Value::as_object)
        .and_then(|m| m.get(key))
        .and_then(Value::as_i64)
        .unwrap_or(2)
        .clamp(1, 24)
}

/// 任务外部设置语义门（整轮读一次）：wb-checkin 联动「启动自动补签」开关（F-55），
/// qoder-checkin 联动 Qoder 自动签到开关（移植 main）；Buddy 国际版四任务各自
/// 联动 settings 开关（移植 main，积分同步 off 门也在此判定，enabled 字段才准确）
fn extra_enabled(st: &AppState, key: &str) -> bool {
    match key {
        "wb-checkin" => crate::commands::workbuddy::wb_auto_checkin_enabled(st),
        "qoder-checkin" => crate::commands::qoder::qoder_auto_checkin_enabled(st),
        "buddy-ai-checkin" => crate::commands::workbuddy::buddy_ai_checkin_enabled(st),
        "buddy-ai-renew" => st.settings().buddy_ai_renew_enabled,
        "buddy-ai-catalog-sync" => st.settings().buddy_ai_catalog_sync_enabled,
        "buddy-ai-credits-sync" => st.settings().buddy_ai_credits_sync_mode.trim() != "off",
        _ => true,
    }
}

/// 看板数据任务 → 平台 key 映射（emit `board-data-synced` 载荷，前端 Dashboard
/// 按 platform 匹配后才静默重读缓存；issue #61 前端联动侧）。buddy-ai 积分同步
/// 的池回写与快照同为看板数据（移植 main T3 审查修复：缺映射致 BuddyAI 看板滞后）
fn board_platform(key: &str) -> Option<&'static str> {
    match key {
        "wb-credits-snapshot" => Some("buddy"),
        "trae-credits-snapshot" | "trae-usage-sync" => Some("trae"),
        // F-80 Qoder 看板与 trae/buddy 同款联动：快照任务写入 qoder_credits_cache
        // 与当日历史后，常驻页面需静默重读（审查修复：此前缺映射致 Qoder 看板滞后）
        "qoder-credits-snapshot" => Some("qoder"),
        "buddy-ai-credits-sync" => Some("buddy-ai"),
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
    let mut ran_this_tick: usize = 0;
    for t in TASKS {
        // 触发判定（trigger 用于日志展示：每日 HH:MM 或「每小时」）
        let trigger = match sched_plan(st, &cfg, t) {
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
            SchedPlan::EveryDays(days) => {
                // EveryDays 按档位节流（移植 main buddy-ai every_days）：距上次成功
                // 执行 ≥d·24h 才再跑（无记录=首次立即跑）
                if let Some(ts) = last_run_ts(&state, t.key) {
                    if chrono::Utc::now().timestamp_millis() - ts < days * 24 * HOURLY_INTERVAL_MS {
                        continue;
                    }
                }
                format!("每{days}天")
            }
        };
        // 失败冷却：退避窗口内静默等待重试，不重复执行也不刷日志
        //（retry_cooldown_ms 与 last_fail_ts 同取整轮快照，零额外 IO）
        if let Some(ts) = last_fail_ts(&state, t.key) {
            if chrono::Utc::now().timestamp_millis() - ts < retry_cooldown_ms(&state, t.key) {
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
            run_task(t.key, st)
        })) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("任务线程 panic（已捕获，不影响后续调度）".to_string()),
        };
        match outcome {
            Ok(v) => {
                let summary = summarize(&v, t.key);
                // skipped_busy（移植 main 审查修复：qoder-checkin 轮次锁被 UI 路径持有的
                // 幂等跳过）与 skipped_empty_pool（移植 main 审查 #15：buddy-ai 空池空轮）
                // 均不 mark_run——跳过≠完成，mark_run 会固化「当日已跑」关闭当日重试链，
                // UI 轮次的失败账号将失去当日调度兜底；空池空轮记账则会让当日新增账号
                // 失去调度补打。skipped_busy 落日志（低频、值得留痕）；空轮纯静默
                //（池空期间每 tick 触发，落日志会刷屏，对齐「空池静默空转」定调）
                //
                // 其余普通 skipped（含各池目录/快照/流水同步任务空池
                // `{"ok":true,"skipped":"无 X 账号"}`，issue #147 口径）走 mark_run +
                // 落日志——有意取舍（定调）：这类任务无奖励属性、空池空转无收益，
                // mark_run 关闭当日重复触发可防空转；当日新增账号次日生效可接受
                //（需即刻巡检可手动触发）。奖励型签到任务豁免（漏跑=丢收益）：
                // buddy-ai 每日活跃打卡、trae/wb/qoder 每日签到空池改打
                // skipped_empty_pool，当日导入账号后下一 tick 正常补打。新增空池
                // 任务若同属「漏跑=丢收益」语义，须同款豁免
                if v.get("skipped_busy").is_some() {
                    fs_utils::app_log(
                        &st.data_dir,
                        &format!(
                            "[调度器] {}（{trigger}）：{}（不记当日已跑，稍后重试）",
                            t.name, summary
                        ),
                    );
                } else if v.get("skipped_empty_pool").is_none() {
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
                        // 门禁暂停轮（skipped_fail_streak）：标题用「暂停」——正文已是
                        // 「已暂停…」，标题再叫「完成」自相矛盾
                        let verb = if v.get("skipped_fail_streak").is_some() { "暂停" } else { "完成" };
                        crate::notify::send(st, &format!("{}{verb}", t.name), &summary, "checkin");
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

/// 任务执行模式表（key → hourly/every_hours/daily；缺省 = every_hours）：
/// scheduler_cfg.task_modes，仅 HOURLY_CAPABLE 内的任务生效（移植 main@c8e855b
/// 看板数据同步 hourly 语义）。缺省模式经 fallback_task_mode 回填（恒 every_hours，
/// 对齐 main credits_sync_mode 默认值，issue #82；前端展示与实际调度同源）
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

/// every_hours 间隔小时数表（key → 小时；缺省 = 2）：scheduler_cfg.task_hours，
/// 仅 HOURLY_CAPABLE 内任务生效。查询时对合法键回填生效值（前端整表回显与实际调度同源）
fn task_hours(st: &AppState) -> std::collections::HashMap<String, i64> {
    let cfg = load_cfg(st);
    HOURLY_CAPABLE
        .iter()
        .map(|k| ((*k).to_string(), effective_task_hours_from(&cfg, k)))
        .collect()
}

/// 任务生效触发时刻：task_times 自定义覆盖 > buddy-ai 设置页时刻 > 内置默认
///（HH:MM 零填充，字符串比较即时间序）。
///
/// 审查修复（移植走样）：BuddyAI 设置页暴露了 `buddy_ai_*_hhmm` 四个编辑框并落盘
/// settings，但本分支调度只读 `scheduler_cfg.task_times`（其余任务的 settings.*_hhmm
/// 属桌面遗留、本分支未接线，保持现状零行为变化）——buddy-ai 四任务改了时刻不生效，
/// 与页面承诺不符。此处仅对 buddy-ai 四任务补 settings 二级回退（对齐蓝本
/// `effective_hhmm`），非法 HH:MM 值回退默认，不阻断调度。
fn effective_hhmm_from(st: &AppState, cfg: &Value, t: &SchedTask) -> String {
    if let Some(v) = task_times_from(cfg).get(t.key) {
        return v.clone();
    }
    let s = st.settings();
    let from_settings: Option<&String> = match t.key {
        "buddy-ai-checkin" => Some(&s.buddy_ai_checkin_hhmm),
        "buddy-ai-renew" => Some(&s.buddy_ai_renew_hhmm),
        "buddy-ai-credits-sync" => Some(&s.buddy_ai_credits_sync_hhmm),
        "buddy-ai-catalog-sync" => Some(&s.buddy_ai_catalog_sync_hhmm),
        _ => None,
    };
    if let Some(v) = from_settings.filter(|v| hhmm_valid(v)) {
        return v.trim().to_string();
    }
    t.hhmm.to_string()
}

/// HH:MM 合法性（零填充 5 字符、时 0-23、分 0-59）；非法 → 回退默认时刻
fn hhmm_valid(v: &str) -> bool {
    let v = v.trim();
    let bytes = v.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return false;
    }
    let (h, m) = v.split_at(2);
    matches!(h.parse::<u32>(), Ok(h) if h < 24) && matches!(m[1..].parse::<u32>(), Ok(m) if m < 60)
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
        "task_hours": task_hours(st),
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
    // 执行模式（移植 main@c8e855b）：hourly/daily/every_hours 三值，且仅看板同步类任务可配；
    // 整表替换（空对象 = 全部回默认）。非 HOURLY_CAPABLE 键一律拒绝，防前端误传。
    let mut modes: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(m) = cfg.get("task_modes") {
        let obj = m.as_object().ok_or("task_modes 应为对象")?;
        for (k, v) in obj {
            if !HOURLY_CAPABLE.contains(&k.as_str()) {
                return Err(format!("任务 {k} 不支持配置执行模式"));
            }
            let s = v
                .as_str()
                .ok_or_else(|| format!("任务 {k} 的模式应为 \"daily\"/\"hourly\"/\"every_hours\""))?;
            if s != "daily" && s != "hourly" && s != "every_hours" {
                return Err(format!("任务 {k} 的模式 {s} 非法（应为 daily/hourly/every_hours）"));
            }
            modes.insert(k.clone(), s.to_string());
        }
    }
    // every_hours 间隔小时数（issue #82）：仅 every_hours 模式任务生效；
    // 整数 1..=24 越界拒绝（由调用方 clamp 会掩盖配置错误，这里显式校验）
    let mut hours: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    if let Some(m) = cfg.get("task_hours") {
        let obj = m.as_object().ok_or("task_hours 应为对象")?;
        for (k, v) in obj {
            if !HOURLY_CAPABLE.contains(&k.as_str()) {
                return Err(format!("任务 {k} 不支持配置执行间隔"));
            }
            let n = v.as_i64().ok_or_else(|| format!("任务 {k} 的间隔应为整数小时"))?;
            if !(1..=24).contains(&n) {
                return Err(format!("任务 {k} 的间隔 {n} 非法（应为 1–24 小时）"));
            }
            hours.insert(k.clone(), n);
        }
    }
    // 落库失败必须上抛（审查 P3）：静默成功会让用户以为配置已保存，
    // 实际重启后回退旧配置；磁盘/busy 异常时命令如实报错
    crate::store::db(&st.data_dir).kv_set(
        "scheduler_cfg",
        &json!({ "disabled_tasks": disabled, "task_times": times, "task_modes": modes, "task_hours": hours }),
    )?;
    fs_utils::app_log(
        &st.data_dir,
        &format!(
            "[调度器] 任务配置已更新：{} 项停用，{} 项自定义时刻，{} 项每小时模式，{} 项自定义间隔模式",
            disabled.len(),
            times.len(),
            // 审查 G7：hourly 与 every_hours 分别如实计数——原「每小时模式」槽位
            // 误填 every_hours 数量，hourly 任务被漏计（延续审查 P10 精确匹配口径）
            modes.iter().filter(|(_, v)| v.as_str() == "hourly").count(),
            modes.iter().filter(|(_, v)| v.as_str() == "every_hours").count()
        ),
    );
    Ok(scheduler_config_get(st))
}

/// 失败事件永久性判定（审查收尾）：结构化 `permanent` 字段为权威来源（显式
/// false 同样生效——文案命中不得翻案；wb_checkin 凭证类失败事件自 1.4.13 起携带），
/// 字段缺省时退化文案匹配兼容旧事件——判定不再依赖「需重新登录」/「无可用凭证」
/// 中文措辞稳定
fn is_permanent_failure(e: &Value) -> bool {
    match e.get("permanent") {
        Some(Value::Bool(b)) => *b,
        _ => {
            let m = e.get("message").and_then(Value::as_str).unwrap_or("");
            m.contains("需重新登录") || m.contains("无可用凭证")
        }
    }
}

/// 成长轮失败统计（run_task wb-growth 重试口径）：返回 (失败账号数, 其中永久性失败数)。
/// 仅统计 growth 事件（type=growth && status=fail），start/done 等推进事件不计。
/// pub(crate)：审查 No.3——UI 手动成长轮同口径统计（commands/workbuddy/checkin.rs）
pub(crate) fn growth_fail_stats(events: &[Value]) -> (i64, i64) {
    let is_growth_fail = |e: &Value| {
        e.get("type").and_then(Value::as_str) == Some("growth")
            && e.get("status").and_then(Value::as_str) == Some("fail")
    };
    let failed = events.iter().filter(|e| is_growth_fail(e)).count() as i64;
    let permanent = events
        .iter()
        .filter(|e| is_growth_fail(e) && is_permanent_failure(e))
        .count() as i64;
    (failed, permanent)
}

/// 纯函数：按本轮 成功数（含 already）/失败数 推导新连击值（独立便于单测）。
/// 任一成功（含 already）→ 清零；混合轮不计数（随任一成功一并清零，绝不 +1）；
/// 全轮失败 → +1；两者皆 0（空池空转）→ 原值不动。
/// 移植自 main（buddy-ai 同名函数，wb-checkin/wb-growth 当日全败门禁复用）。
/// pub(crate)：审查 No.3——UI 手动轮次连击同步复用（commands/workbuddy/checkin.rs）
pub(crate) fn buddy_ai_streak_next(prev: i64, ok_count: i64, failed_count: i64) -> i64 {
    if ok_count > 0 {
        0
    } else if failed_count > 0 {
        prev + 1
    } else {
        prev
    }
}

// ── 移植 main #134 方案3：wb-checkin / wb-growth 当日连续整轮全败连击 ──────────
// 仿 buddy-ai streak 模式（buddy-ai-checkin 分支内联门禁）：调度器对可重试失败
// 30→60→120 分钟退避全天重试，凭证级失效时每轮占轮次锁数分钟且反复撞锁
//（用户点「开始签到」必被拒）。连续 ≥3 整轮全败当日暂停（mark_run 记「今日已处理」
// 终止重试链），任一成功清零，次日自动恢复。

/// 当日全败暂停阈值（对齐 buddy-ai；wb-checkin 与 wb-growth 共用同一阈值）
const WB_ROUND_FAIL_STREAK_LIMIT: i64 = 3;
/// 独立 kv 键：签到与成长各自计数（互不影响；计数与日期同文档单次写入防半写分叉）。
/// pub(crate)：审查 No.3——UI 手动轮次（commands/workbuddy/checkin.rs）同键同步连击
pub(crate) const WB_CHECKIN_STREAK_KEY: &str = "wb_checkin_fail_streak";
pub(crate) const WB_GROWTH_STREAK_KEY: &str = "wb_growth_fail_streak";

/// 读当前连击数：date ≠ 今日（跨日/无记录/损坏）→ 从 0 起（对齐 buddy_ai_checkin_streak）。
/// pub(crate)：审查 No.3——UI 手动轮次连击同步复用（commands/workbuddy/checkin.rs）
pub(crate) fn wb_round_fail_streak(st: &AppState, kv_key: &str, today: &str) -> i64 {
    let v: Value = crate::store::db(&st.data_dir).kv_get(kv_key);
    if v.get("date").and_then(Value::as_str) != Some(today) {
        return 0;
    }
    v.get("streak").and_then(Value::as_i64).unwrap_or(0).max(0)
}

/// 持久化连击数。落库失败静默：连击丢失仅回到多试几轮的自愈起点（对齐 buddy-ai 同款注释）。
/// pub(crate)：审查 No.3——UI 手动轮次连击同步复用（commands/workbuddy/checkin.rs）
pub(crate) fn wb_round_fail_streak_save(st: &AppState, kv_key: &str, today: &str, streak: i64) {
    let _ = crate::store::db(&st.data_dir).kv_set(kv_key, &json!({ "streak": streak, "date": today }));
}

/// 后台轮次事件转发回调（方案1 共通封装，审查 No.4 去重）：经 AppState 事件管线
///（emit_event → SSE hub）同步转发进度事件，无需 AppHandle。wb-checkin / wb-growth 共用
fn state_event_emitter(st: &AppState) -> crate::commands::workbuddy::WbCheckinEmitter {
    let st2 = st.clone();
    std::sync::Arc::new(move |name: &str, payload: Value| st2.emit_event(name, payload))
}

/// 后台轮次 panic 兜底 + exit 补发（审查 No.5 同款缺口的后台路径闭环）：方案1 转发
/// start 事件后，若轮次中途 panic，done/exit 复位事件不再发出——前端运行态永久悬挂。
/// tick 层 catch_unwind 拿不到轮次事件句柄补不了发，须在轮次现场兜底：正常结束与
/// panic 均补发 exit（前端 exit 仅复位运行态，幂等无害，与 UI 路径 spawn_wb_round
/// 契约一致）；panic 落日志并返 Err 交调度器 mark_fail + 冷却重试。
fn run_wb_round_guarded<T>(
    st: &AppState,
    emit: &crate::commands::workbuddy::WbCheckinEmitter,
    f: impl FnOnce() -> T,
) -> Result<T, String> {
    let ret = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    crate::commands::workbuddy::emit_wb_event(emit, &json!({ "type": "exit", "ok": ret.is_ok() }));
    ret.map_err(|p| {
        let detail = if let Some(s) = p.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = p.downcast_ref::<String>() {
            s.clone()
        } else {
            "未知 panic".to_string()
        };
        crate::fs_utils::app_log(
            &st.data_dir,
            &format!("[WB 轮次] 后台轮次 panic（exit 已补发，前端运行态已复位）：{detail}"),
        );
        "WB 后台轮次 panic（已捕获，交调度器冷却重试）".to_string()
    })
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
        // （对齐 qoder-checkin）；永久/当日不可恢复失败从重试判定剔除——否则全天
        // ~28 轮无效重试 + 当日首败误报（口径对齐 UI 重试筛选：未配置 jwt（历史
        // 口径 message 匹配）+ SessionDead=401 吊销 / AuthRejected=服务端拒绝认证
        // （issue #128）/ DeviceSigned=设备当日去重）
        "trae-checkin" => {
            let accts = crate::vault::load_accounts(st);
            // 奖励型签到空池豁免（审查修复，对齐 buddy-ai 口径）：签到漏跑
            // =丢积分，mark_run 固化「当日已跑」会让当日导入账号失去调度补打——
            // 空池提前返回 skipped_empty_pool，tick 不 mark_run、下一 tick 补打
            if accts.accounts.is_empty() {
                return Ok(json!({ "ok": true, "skipped": "无 Trae 账号", "skipped_empty_pool": true }));
            }
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
        // 审查 P2（issue #61 批次）：同 trae-checkin——可重试失败返 Err 交 30 分钟冷却重试；
        // 永久性凭证失败（需重新登录 / 无可用凭证，30 分钟内不会自愈）剔除出重试判定
        "wb-checkin" => {
            // 奖励型签到空池豁免（审查修复，对齐 buddy-ai 口径）：空池提前
            // 返回 skipped_empty_pool（tick 不 mark_run，当日导入账号下一 tick 补打）。
            // 分支级早退而非任务层打标：wb 调度轮次会向前端转发 start/done 事件
            //（issue #134 方案1），空池若仍每 tick 走整轮会向前端刷「成功 0」toast；
            // 早退置于抢轮次锁之前——空轮不占锁，连击统计不动（空轮本就不变）。
            // 池读取失败走 mark_fail 重试链不再吞成空池（落库并发加固，main
            // 245a611=>bfa0bc2 批次）：unwrap_or_default 在锁 Busy 瞬时会吞成空池
            // → skipped_empty_pool 提前返回 → mark_run 固化当日已跑 → 当日导入
            // 账号失去补打（与空池豁免定调矛盾）；? 上抛 Err 走 tick 失败冷却，
            // 下一 tick 自动补打
            let pool = crate::store::docs::wb_pool_load_raw(&crate::store::db(&st.data_dir))?;
            if pool.get("accounts").and_then(Value::as_array).map(|a| a.is_empty()).unwrap_or(true) {
                return Ok(json!({ "ok": true, "skipped": "无 WorkBuddy 账号", "skipped_empty_pool": true }));
            }
            // 移植 main #134 方案2：抢轮次锁（进程内 + 持有者标签；Windows 跨进程
            // 互斥体 OS 专属不移植）与 UI 路径互斥，skipped 文案携带持有者明细
            let _round = match crate::commands::workbuddy::try_acquire_wb_round("调度器签到", &st.data_dir) {
                Ok(g) => g,
                Err(why) => {
                    // skipped_busy：抢不到轮次锁（UI 路径在跑）= 幂等跳过而非失败（对齐
                    // qoder-checkin）——返 Err 会触发当日首败误报通知 + 白耗 Server酱 额度
                    return Ok(json!({ "ok": true, "skipped": why, "skipped_busy": true }));
                }
            };
            // 移植 main #134 方案3：当日连续整轮全败门禁（疑似凭证级失效，重试注定
            // 失败）——达阈值当日暂停，mark_run 记「今日已处理」终止全天退避重试链，
            // 次日自动恢复
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let prev_streak = wb_round_fail_streak(st, WB_CHECKIN_STREAK_KEY, &today);
            if prev_streak >= WB_ROUND_FAIL_STREAK_LIMIT {
                return Ok(json!({
                    "ok": true,
                    "skipped": "连续多轮全部失败（疑似凭证失效），当日暂停签到，次日自动恢复",
                    // 审查 No.1：补 summary——mark_run 后通知正文优先取本字段；否则布尔 ok
                    // 无法匹配 (ok,already,failed) 三元组，正文沦为原始 JSON
                    "summary": "签到已暂停：当日连续多轮全部失败（疑似凭证失效），次日自动恢复",
                    "skipped_fail_streak": true,
                }));
            }
            let mut events: Vec<Value> = Vec::new();
            // 移植 main #134 方案1：后台轮次同步转发进度事件（此前仅收集不转发，前端
            // 运行态零显示，用户点击必撞锁且不可见原因）。docker 适配：经 AppState
            // 事件管线（emit_event → SSE hub）转发，无需 AppHandle
            let emit = state_event_emitter(st);
            // 审查补充：panic 兜底 + exit 补发（防前端运行态悬挂），panic 返 Err 冷却重试
            let done = run_wb_round_guarded(st, &emit, || {
                crate::tasks::wb_checkin::run_checkin_round(
                    st,
                    &crate::tasks::wb_checkin::CheckinOpts::daily(),
                    &mut |ev| {
                        crate::commands::workbuddy::emit_wb_event(&emit, ev);
                        events.push(ev.clone());
                    },
                )
            })?;
            let ok = done.get("ok").and_then(Value::as_i64).unwrap_or(0);
            let already = done.get("already").and_then(Value::as_i64).unwrap_or(0);
            let failed = done.get("failed").and_then(Value::as_i64).unwrap_or(0);
            // 移植 main #134 方案3：轮末更新连击（复用 buddy_ai_streak_next 纯函数：
            // 任一成功含 already 清零 / 全败 +1 / 空轮不动），值不变不落库
            let new_streak = buddy_ai_streak_next(prev_streak, ok + already, failed);
            if new_streak != prev_streak {
                wb_round_fail_streak_save(st, WB_CHECKIN_STREAK_KEY, &today, new_streak);
            }
            let failed_permanent = events
                .iter()
                .filter(|e| e.get("status").and_then(Value::as_str) == Some("fail") && is_permanent_failure(e))
                .count() as i64;
            let retryable = (failed - failed_permanent).max(0);
            if retryable > 0 {
                return Err(format!(
                    "WB 签到：{ok} 成功 / {already} 已签 / {retryable} 失败（稍后自动重试）"
                ));
            }
            Ok(done)
        }
        // WorkBuddy 每日成长：成长三开关驱动，抢轮次锁与 UI 路径互斥
        "wb-growth" => {
            // 移植 main #134 方案2：抢轮次锁（进程内 + 持有者标签；Windows 跨进程
            // 互斥体 OS 专属不移植）与 UI 路径互斥
            let _round = match crate::commands::workbuddy::try_acquire_wb_round("调度器成长", &st.data_dir) {
                Ok(g) => g,
                Err(why) => {
                    // skipped_busy：对齐 qoder-checkin/wb-checkin 的幂等跳过（防误报通知）
                    return Ok(json!({ "ok": true, "skipped": why, "skipped_busy": true }));
                }
            };
            // 移植 main #134 方案3：当日连续整轮全败门禁（同 wb-checkin，独立连击键）
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let prev_streak = wb_round_fail_streak(st, WB_GROWTH_STREAK_KEY, &today);
            if prev_streak >= WB_ROUND_FAIL_STREAK_LIMIT {
                return Ok(json!({
                    "ok": true,
                    "skipped": "连续多轮全部失败（疑似凭证失效），当日暂停成长任务，次日自动恢复",
                    // 审查 No.1：补 summary（同 wb-checkin，正文可读）
                    "summary": "成长任务已暂停：当日连续多轮全部失败（疑似凭证失效），次日自动恢复",
                    "skipped_fail_streak": true,
                }));
            }
            let s = crate::commands::workbuddy::load_settings(st);
            let flags = crate::tasks::wb_checkin::GrowthOpts {
                travel: s.growth_travel,
                lottery: s.growth_lottery,
                tasks: s.growth_tasks,
            };
            // run_growth_round 为 NDJSON 推进式输出（无汇总返回值）。审查修复：失败事件
            // 此前被空回调丢弃（恒 Ok：当日不重试不通知）——现累计事件，可重试失败返
            // Err 交 30 分钟冷却重试；永久性凭证失败剔除（对齐 wb-checkin 口径）。
            // 移植 main #134 方案1：后台轮次同步转发进度事件（同 wb-checkin）
            let mut events: Vec<Value> = Vec::new();
            let emit = state_event_emitter(st);
            // 审查补充：panic 兜底 + exit 补发（同 wb-checkin）
            run_wb_round_guarded(st, &emit, || {
                crate::tasks::wb_checkin::run_growth_round(st, &flags, &[], &mut |ev| {
                    crate::commands::workbuddy::emit_wb_event(&emit, ev);
                    events.push(ev.clone());
                });
            })?;
            let (failed, failed_permanent) = growth_fail_stats(&events);
            // 移植 main #134 方案3：轮末更新连击（复用 buddy_ai_streak_next 纯函数；
            // ok_count 统计 growth+ok 事件，对齐桌面口径）
            let ok_count = events
                .iter()
                .filter(|e| {
                    e.get("type").and_then(Value::as_str) == Some("growth")
                        && e.get("status").and_then(Value::as_str) == Some("ok")
                })
                .count() as i64;
            let new_streak = buddy_ai_streak_next(prev_streak, ok_count, failed);
            if new_streak != prev_streak {
                wb_round_fail_streak_save(st, WB_GROWTH_STREAK_KEY, &today, new_streak);
            }
            let retryable = (failed - failed_permanent).max(0);
            if retryable > 0 {
                return Err(format!("WB 成长：{retryable} 个账号失败（稍后自动重试）"));
            }
            // 全部失败且均为永久性（凭证类）时不再 Err 重试（避免注定失败的重试循环），
            // 但须可观测：summary 落入 last_summary 供日志/UI 展示（此前恒 {"ok":true} 零信息量）
            let summary = if failed_permanent > 0 {
                format!("WB 成长完成，{failed_permanent} 个账号永久失败（需重新登录或配置凭证）")
            } else {
                "WB 成长完成".to_string()
            };
            Ok(json!({ "ok": true, "summary": summary }))
        }
        // WorkBuddy token 兜底续期：与 `--task-run wb-renew` 同款（lazy 24h）
        "wb-renew" => Ok(crate::tasks::wb_checkin::run_renew_only(st, 24)),
        // Buddy 国际版每日活跃打卡（移植 main）：抢轮次锁与 UI 路径互斥。
        // T7/BL-5 定调（移植 main）：buddy-ai 凭证失效可经网关 401 自愈/续期轮换
        // 新值，无永久剔除——retryable = failed 全量，返 Err 交 30 分钟冷却重试。
        // 审查修复（第二轮，永久失败门限）：连续 N 次全轮失败时当日暂停自动重试与
        // 失败通知——失效凭证（封禁/会话失效/refresh 永久拒绝）触发的整轮失败本
        // 不可自愈，全天 30min 冷却 + 每日首败通知是纯噪声；达门限后改返 Ok + 跳过
        // 直至次日或用户手动重试（手动 UI 打卡会清零）。跨日重置
        "buddy-ai-checkin" => {
            const FAIL_STREAK_CAP: i64 = 3;
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let fail_v: serde_json::Value = crate::store::db(&st.data_dir).kv_get("buddy_ai_checkin_fail_streak");
            let streak = if fail_v.get("date").and_then(Value::as_str) == Some(today.as_str()) {
                fail_v.get("streak").and_then(Value::as_i64).unwrap_or(0)
            } else {
                0
            };
            if streak >= FAIL_STREAK_CAP {
                fs_utils::app_log(
                    &st.data_dir,
                    &format!(
                        "buddy-ai: 连续 {streak} 次打卡失败，今日暂停自动重试（多为凭证失效，请到 BuddyAI 账号页重新授权或续期；手动打卡一次可清零）"
                    ),
                );
                return Ok(json!({
                    "ok": true,
                    "skipped": format!("连续 {streak} 次打卡失败，今日暂停自动重试"),
                    "skipped_fail_streak": true,
                }));
            }
            let Ok(_round) = crate::commands::workbuddy::try_acquire_buddy_ai_round() else {
                // skipped_busy：抢不到轮次锁（UI 路径在跑）= 幂等跳过而非失败（对齐
                // wb-checkin/qoder-checkin）——返 Err 会触发当日首败误报通知
                return Ok(json!({ "ok": true, "skipped": "已有 buddy-ai 打卡任务在执行中，本轮跳过", "skipped_busy": true }));
            };
            let mut events: Vec<Value> = Vec::new();
            let done = crate::tasks::buddy_ai_checkin::run_checkin_round(st, &mut |ev| {
                events.push(ev.clone())
            });
            // 审查 #15：空池空轮透传 skipped_empty_pool——tick 据此不 mark_run 且
            // 静默（当日新增账号保留调度补打机会）；连击统计无需更新（ok/already/
            // failed 全 0，否则会把 streak 误清零），提前返回
            if done.get("skipped_empty_pool").is_some() {
                return Ok(done);
            }
            let failed = done.get("failed").and_then(Value::as_i64).unwrap_or(0);
            let ok = done.get("ok").and_then(Value::as_i64).unwrap_or(0);
            let already = done.get("already").and_then(Value::as_i64).unwrap_or(0);
            // streak 更新：任一成功（含 already）→ 清零；全轮失败 → +1；混合视作部分自愈不计数
            let new_streak = if ok > 0 || already > 0 {
                0
            } else if failed > 0 {
                streak + 1
            } else {
                0
            };
            let _ = crate::store::db(&st.data_dir).kv_set(
                "buddy_ai_checkin_fail_streak",
                &json!({ "date": today, "streak": new_streak }),
            );
            if failed > 0 {
                // 失败明细落 app_log（对齐 main 蓝本「定时打卡失败明细」格式）
                fs_utils::app_log(
                    &st.data_dir,
                    &format!(
                        "buddy-ai: 定时打卡失败明细——{}",
                        events
                            .iter()
                            .filter(|e| e.get("status").and_then(Value::as_str) == Some("fail"))
                            .map(|e| {
                                format!(
                                    "{}({})",
                                    e.get("name").and_then(Value::as_str).unwrap_or("?"),
                                    e.get("message").and_then(Value::as_str).unwrap_or("")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                );
                return Err(format!(
                    "buddy-ai 打卡：{ok} 成功 / {already} 已完成 / {failed} 失败（稍后自动重试，streak={new_streak}）"
                ));
            }
            Ok(done)
        }
        // WorkBuddy 积分余额每日快照：补齐近 7 日消耗差分时序；顺带刷新官方请求用量
        //（issue #61 批次：用量刷全账号聚合缓存 workbuddy_usage_official_all_cache，
        // force 跳过 10min 缓存；原单账号缓存分支前端无调用方，属刷错目标一并修复）
        "wb-credits-snapshot" => {
            // 运行池句柄：网关运行池三键拨新（issue #109 同类）。main 版透传
            // run_task 的 runtime 参数（CLI None 跳过）；docker 网关常驻单体，
            // 经 gateway_shared() 取（None = 网关未注册，写回跳过拨新不报错）
            let shared = crate::api_server::runtime::gateway_shared();
            let parsed =
                crate::commands::workbuddy::wb_credits_snapshot_task(st, shared.as_ref())?;
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
            // 奖励型签到空池豁免（审查修复，对齐 buddy-ai 口径）：空池提前
            // 返回 skipped_empty_pool（tick 不 mark_run，当日导入账号下一 tick 补打）；
            // 早退置于抢轮次锁之前——空轮不占锁，连击/重试判定均不触及。
            // 池读取失败走 mark_fail 重试链不再吞成空池（同 wb-checkin 口径）
            let pool = crate::store::docs::qoder_pool_load_raw(&crate::store::db(&st.data_dir))?;
            if pool.get("accounts").and_then(Value::as_array).map(|a| a.is_empty()).unwrap_or(true) {
                return Ok(json!({ "ok": true, "skipped": "无 Qoder 账号", "skipped_empty_pool": true }));
            }
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
            // issue #190 收尾：BLOCKED 风控信任单列（blocked_need_trust）重试
            // 同样注定失败（信任单列需用户在风控页确认），从重试判定剔除——
            // 与 qoder_checkin 内部重试链同口径（审查修复）
            let failed_blocked = done.get("failed_blocked").and_then(Value::as_i64).unwrap_or(0);
            if failed - failed_empty - failed_permanent - failed_blocked > 0 {
                let ok = done.get("ok").and_then(Value::as_i64).unwrap_or(0);
                let already = done.get("already").and_then(Value::as_i64).unwrap_or(0);
                return Err(format!(
                    "Qoder 签到：{ok} 成功 / {already} 已领 / {failed} 失败（稍后自动重试）"
                ));
            }
            Ok(done)
        }
        // F-80 Qoder 积分快照：与 `--task-run qoder-credits-snapshot` 同款（空池空转）；
        // 成功行快照三键拨新进 qoder_pool（issue #102 运行期保鲜，函数内部经
        // gateway_shared() 判定，对齐 trae-credits-snapshot 的 issue #67 口径）
        "qoder-credits-snapshot" => crate::tasks::qoder_credits::run_snapshot_task(st),
        // Qoder Token 定时刷新：与 `--task-run qoder-refresh` 同款（6h 周期，lazy 7h 惰性门；
        // 有账号刷新失败时返 Err，交由调度器 30 分钟冷却重试；抢不到跨进程锁返回 skipped_busy）
        "qoder-refresh" => crate::tasks::qoder_refresh::run_task(st),
        // Qoder 会话流水定时合并：把早于保留窗的请求行按 (date, uid, preview) 合并
        // 单行（幂等自愈；纯本地 SQLite 无网络依赖，失败返 Err 交 30 分钟冷却重试）。
        // 保留窗天数：本分支无 settings 字段，直接采用 docs 层常量（默认 3 天）
        "qoder-sessions-compact" => {
            let days = crate::store::docs::QODER_SESSIONS_COMPACT_AFTER_DAYS;
            let (merged, deleted) = crate::store::docs::qoder_sessions_compact(
                &crate::store::db(&st.data_dir),
                days,
            )?;
            Ok(json!({ "ok": true, "merged": merged, "deleted": deleted, "days": days }))
        }
        // Qoder 模型目录同步：与 `--task-run qoder-catalog-sync` 同款（真签名拉
        // model/list → adopt_remote；空池/无凭证空转，网络失败返 Err 冷却重试）
        "qoder-catalog-sync" => crate::tasks::qoder_catalog::run_task(st),
        // Buddy 国际版凭证兜底续期（移植 main，lazy 48h）：无临期账号空转，
        // 全失败返 Err 冷却重试
        "buddy-ai-renew" => crate::commands::workbuddy::buddy_ai_renew_task(st),
        // Buddy 国际版积分数据同步（移植 main）：get-user-resource 全池取数 → 池回写
        // + 快照；空池自然空转。docker 适配——无 runtime 句柄，池三键拨新在 impl
        // 内部经 gateway_shared() 判定完成（对齐 trae-credits-snapshot 口径）。
        // 顺带官网用量聚合刷新（force 跳过 10min 缓存，国内 wb-credits-snapshot 同款）
        // 统计尽力而为不上抛；桌面 token_files 重扫属桌面专属不移植（交接定案）
        "buddy-ai-credits-sync" => {
            let credits = crate::commands::workbuddy::buddy_ai_credits_sync_impl(st)?;
            let usage =
                match crate::commands::workbuddy::buddy_ai_usage_official_all_impl(st, true) {
                    Ok(_) => "已刷新",
                    Err(_) => "跳过（无可用凭证或拉取失败）",
                };
            Ok(json!({
                "ok": true,
                "accounts": credits.len(),
                "fetched": credits.iter().filter(|a| a.get("credits_balance").map(|b| !b.is_null()).unwrap_or(false)).count(),
                "usage": usage,
            }))
        }
        // Buddy 国际版模型目录同步（移植 main，/v3/config 直连 → 动态替换）：
        // 空池/无凭证静默跳过不计失败
        "buddy-ai-catalog-sync" => {
            if crate::commands::workbuddy::buddy_ai_pool_size(st) == 0 {
                Ok(json!({ "ok": true, "skipped": "无 buddy-ai 账号" }))
            } else {
                crate::commands::workbuddy::buddy_ai_catalog_sync_impl(st)
                    .map(|n| json!({ "ok": true, "models": n }))
            }
        }
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
    // 状态落库失败不可静默（审查 P3）：last_run_date 丢失 → 当日任务重复执行；
    // last_fail_ts 丢失 → 失败冷却失效引发重试风暴。记日志供排查（不阻断主流程）
    if let Err(e) = crate::store::db(&st.data_dir).kv_set("scheduler_state", &root) {
        fs_utils::app_log(
            &st.data_dir,
            &format!("[调度器] 任务状态落库失败（task={key}）: {e}"),
        );
    }
}

/// 结果 JSON → 单行摘要（含 summary 字段直接取用；签到轮次取 ok/already/failed
/// 计数，buddy-ai 打卡用「已完成」措辞，其余截断展示；task_key 供轮次措辞分型）
fn summarize(v: &Value, task_key: &str) -> String {
    if let Some(s) = v.get("summary").and_then(Value::as_str) {
        return s.to_string();
    }
    if v.is_object() {
        let get = |k: &str| v.get(k).and_then(Value::as_i64);
        if let (Some(o), Some(a), Some(f)) = (get("ok"), get("already"), get("failed")) {
            let already_word = if task_key == "buddy-ai-checkin" { "已完成" } else { "已签" };
            return format!("成功 {o}，{already_word} {a}，失败 {f}");
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
                // 展示时刻：hourly → 「每小时」；EveryHours(h) → 「每h小时」；
                // EveryDays(d) → 「每d天」；其余 HH:MM
                "time": match sched_plan(st, &cfg, t) {
                    SchedPlan::Hourly => "每小时".to_string(),
                    SchedPlan::EveryHours(h) => format!("每{h}小时"),
                    SchedPlan::EveryDays(d) => format!("每{d}天"),
                    SchedPlan::Daily(hhmm) => hhmm,
                    SchedPlan::Skip => "已关闭".to_string(),
                },
                // 执行模式（daily/hourly/every_hours/every_days/every6h/off）：前端据此
                // 切换时刻输入/模式选择/间隔输入与徽标；every6h 保留给 qoder-refresh
                // 固定档位（不可配置），看板数据类 every_hours 可配间隔小时数，
                // every_days 仅 buddy-ai 积分同步（settings 驱动，移植 main）
                "mode": match sched_plan(st, &cfg, t) {
                    SchedPlan::Hourly => "hourly",
                    SchedPlan::EveryHours(_) if t.key == "qoder-refresh" => "every6h",
                    SchedPlan::EveryHours(_) => "every_hours",
                    SchedPlan::EveryDays(_) => "every_days",
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

    /// 失败事件永久性判定（审查收尾回归）：结构化 permanent 优先、文案匹配退化兼容
    #[test]
    fn permanent_failure_structured_flag_takes_precedence() {
        // 结构化标记：文案不含关键词也判永久
        assert!(is_permanent_failure(&json!({
            "status": "fail", "permanent": true, "message": "自定义永久失败描述"
        })));
        // 文案退化兼容（旧事件无 permanent 字段）
        assert!(is_permanent_failure(&json!({
            "status": "fail", "message": "无可用凭证（token 已过期）"
        })));
        assert!(is_permanent_failure(&json!({
            "status": "fail", "message": "需重新登录，已跳过"
        })));
        // 可重试失败：无标记且文案不含关键词
        assert!(!is_permanent_failure(&json!({
            "status": "fail", "message": "上游 502"
        })));
        // permanent:false 不误判（显式否定优先于文案）
        assert!(!is_permanent_failure(&json!({
            "status": "fail", "permanent": false, "message": "无可用凭证"
        })));
    }

    /// 成长轮失败统计：仅 growth 事件计入；permanent 事件从可重试口径剔除
    #[test]
    fn growth_fail_stats_counts_growth_events_only() {
        let events = vec![
            json!({"type": "start", "total": 3}),
            json!({"type": "growth", "status": "fail", "permanent": true, "message": "无可用凭证（x）"}),
            json!({"type": "growth", "status": "fail", "message": "上游 502"}),
            json!({"type": "growth", "status": "ok"}),
            json!({"type": "done", "ok": 2, "failed": 2}),
        ];
        let (failed, permanent) = growth_fail_stats(&events);
        assert_eq!(failed, 2);
        assert_eq!(permanent, 1);
        // 空轮次（无账号）
        assert_eq!(growth_fail_stats(&[]), (0, 0));
    }

    fn task(key: &str) -> &'static SchedTask {
        TASKS.iter().find(|t| t.key == key).unwrap()
    }

    /// 测试专用 AppState（与 sched_backoff_tests 同款构造）：sched_plan 签名含
    /// &AppState（buddy-ai settings 驱动模式/语义门读取，移植 main 适配）
    fn temp_state(tag: &str) -> AppState {
        let dir = std::env::temp_dir().join(format!("aiwork_schedplan_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        AppState {
            data_dir: dir,
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            buddy_ai_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            events: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// settings 整体读改写回（buddy-ai 模式预置）：整体反序列化避免部分 JSON 落 default
    fn set_settings(st: &AppState, f: impl FnOnce(&mut crate::models::Settings)) {
        let mut s = st.settings();
        f(&mut s);
        crate::store::db(&st.data_dir)
            .kv_set("app_settings", &s)
            .unwrap();
    }

    /// hourly 模式仅对 HOURLY_CAPABLE 任务生效：误配到其他任务按每日处理。
    /// 预置 wb-checkin 外部门（auto_checkin=true，fresh DB 默认关为正确生产行为），
    /// 使断言聚焦「hourly 误配不生效」而非外部 Skip
    #[test]
    fn hourly_mode_only_for_capable_tasks() {
        let st = temp_state("capable");
        crate::store::db(&st.data_dir)
            .kv_set_raw("workbuddy_settings", r#"{"auto_checkin":true}"#)
            .unwrap();
        let cfg = json!({ "task_modes": { "wb-credits-snapshot": "hourly", "wb-checkin": "hourly" } });
        assert!(matches!(
            sched_plan(&st, &cfg, task("wb-credits-snapshot")),
            SchedPlan::Hourly
        ));
        assert!(matches!(sched_plan(&st, &cfg, task("wb-checkin")), SchedPlan::Daily(_)));
    }

    /// 停用名单与外部设置门（wb-checkin → 启动自动补签）任一不满足即 Skip
    #[test]
    fn disabled_or_external_gate_skips() {
        let st = temp_state("gate");
        let cfg = json!({ "disabled_tasks": ["trae-credits-snapshot"] });
        assert!(matches!(
            sched_plan(&st, &cfg, task("trae-credits-snapshot")),
            SchedPlan::Skip
        ));
        assert!(matches!(
            sched_plan(&st, &json!({}), task("wb-checkin")),
            SchedPlan::Skip
        ));
    }

    /// 自定义时刻覆盖默认；缺省回落内置默认时刻
    #[test]
    fn custom_time_overrides_default() {
        let st = temp_state("times");
        let cfg = json!({ "task_times": { "models-sync": "08:15" } });
        let t = task("models-sync");
        match sched_plan(&st, &cfg, t) {
            SchedPlan::Daily(hhmm) => assert_eq!(hhmm, "08:15"),
            _ => panic!("models-sync 应为每日计划"),
        }
        match sched_plan(&st, &json!({}), t) {
            SchedPlan::Daily(hhmm) => assert_eq!(hhmm, t.hhmm),
            _ => panic!("models-sync 应为每日计划"),
        }
    }

    /// qoder-refresh 固定每 6 小时（移植 main）：不受 task_times/task_modes 影响，
    /// 时刻配置不参与判定；停用时仍按 Skip 处理
    #[test]
    fn qoder_refresh_is_every6h() {
        let st = temp_state("qrefresh");
        let cfg = json!({
            "task_times": { "qoder-refresh": "12:00" },
            "task_modes": { "qoder-refresh": "hourly" },
        });
        match sched_plan(&st, &cfg, task("qoder-refresh")) {
            SchedPlan::EveryHours(h) => assert_eq!(h, REFRESH_INTERVAL_HOURS),
            other => panic!("qoder-refresh 应为 EveryHours 计划，实际 {other:?}"),
        }
        assert!(matches!(
            sched_plan(
                &st,
                &json!({ "disabled_tasks": ["qoder-refresh"] }),
                task("qoder-refresh")
            ),
            SchedPlan::Skip
        ));
    }

    /// qoder-sessions-compact 固定每 6 小时（移植 main）：不受 task_times/task_modes
    /// 影响，时刻配置不参与判定；停用时仍按 Skip 处理
    #[test]
    fn qoder_sessions_compact_is_every6h() {
        let st = temp_state("qcompact");
        let cfg = json!({
            "task_times": { "qoder-sessions-compact": "03:10" },
            "task_modes": { "qoder-sessions-compact": "hourly" },
        });
        match sched_plan(&st, &cfg, task("qoder-sessions-compact")) {
            SchedPlan::EveryHours(h) => assert_eq!(h, COMPACT_INTERVAL_HOURS),
            other => panic!("qoder-sessions-compact 应为 EveryHours 计划，实际 {other:?}"),
        }
        assert!(matches!(
            sched_plan(
                &st,
                &json!({ "disabled_tasks": ["qoder-sessions-compact"] }),
                task("qoder-sessions-compact")
            ),
            SchedPlan::Skip
        ));
    }

    /// buddy-ai 积分同步五模式（移植 main f5f2cd3..61cc245，docker settings 驱动）：
    /// 默认 daily 走内置 23:45（task_times 可覆盖）；hourly/every_hours/every_days
    /// 生效并 clamp；mode=off 经语义门 Skip（空 mode 时 state 归一化按旧 enabled
    /// 派生 daily/off）；task_modes 误配不生效
    #[test]
    fn buddy_ai_credits_sync_five_modes() {
        let st = temp_state("buddymode");
        let t = task("buddy-ai-credits-sync");
        // 默认（settings mode 经 state 归一化 = daily）：每日内置 23:45
        match sched_plan(&st, &json!({}), t) {
            SchedPlan::Daily(hhmm) => assert_eq!(hhmm, "23:45"),
            other => panic!("buddy-ai-credits-sync 缺省应为每日计划，实际 {other:?}"),
        }
        // daily 时刻可被 task_times 覆盖（docker 统一链路）
        let times = json!({ "task_times": { "buddy-ai-credits-sync": "22:10" } });
        match sched_plan(&st, &times, t) {
            SchedPlan::Daily(hhmm) => assert_eq!(hhmm, "22:10"),
            other => panic!("task_times 覆盖应生效，实际 {other:?}"),
        }
        // hourly 模式（settings 预置）
        set_settings(&st, |s| s.buddy_ai_credits_sync_mode = "hourly".into());
        assert!(matches!(sched_plan(&st, &json!({}), t), SchedPlan::Hourly));
        // every_hours 模式：默认 2 小时；手工间隔生效 + 越界 clamp（1..=24）
        set_settings(&st, |s| {
            s.buddy_ai_credits_sync_mode = "every_hours".into();
            s.buddy_ai_credits_sync_hours = "99".into();
        });
        assert!(matches!(sched_plan(&st, &json!({}), t), SchedPlan::EveryHours(24)));
        set_settings(&st, |s| s.buddy_ai_credits_sync_hours = "6".into());
        assert!(matches!(sched_plan(&st, &json!({}), t), SchedPlan::EveryHours(6)));
        // every_days 模式：默认 1 天；手工间隔生效 + 越界 clamp（1..=30）
        set_settings(&st, |s| {
            s.buddy_ai_credits_sync_mode = "every_days".into();
            s.buddy_ai_credits_sync_days = "99".into();
        });
        assert!(matches!(sched_plan(&st, &json!({}), t), SchedPlan::EveryDays(30)));
        set_settings(&st, |s| s.buddy_ai_credits_sync_days = "3".into());
        assert!(matches!(sched_plan(&st, &json!({}), t), SchedPlan::EveryDays(3)));
        // off 模式：语义门 Skip（对应 scheduler_status.enabled=false）
        set_settings(&st, |s| s.buddy_ai_credits_sync_mode = "off".into());
        assert!(matches!(sched_plan(&st, &json!({}), t), SchedPlan::Skip));
        // scheduler_cfg.task_modes 误配不生效（docker：模式不入 kv）
        set_settings(&st, |s| s.buddy_ai_credits_sync_mode = "daily".into());
        let bogus = json!({ "task_modes": { "buddy-ai-credits-sync": "hourly" } });
        assert!(matches!(sched_plan(&st, &bogus, t), SchedPlan::Daily(_)));
    }

    /// buddy-ai 其余三任务的 settings 语义门（移植 main）：checkin 默认关 → Skip；
    /// renew/catalog 默认开 → Daily；预置开后 checkin 生效
    #[test]
    fn buddy_ai_tasks_settings_gates() {
        let st = temp_state("buddygate");
        // 默认：checkin 关（Skip），renew/catalog 开（Daily）
        assert!(matches!(
            sched_plan(&st, &json!({}), task("buddy-ai-checkin")),
            SchedPlan::Skip
        ));
        assert!(matches!(
            sched_plan(&st, &json!({}), task("buddy-ai-renew")),
            SchedPlan::Daily(_)
        ));
        assert!(matches!(
            sched_plan(&st, &json!({}), task("buddy-ai-catalog-sync")),
            SchedPlan::Daily(_)
        ));
        // 预置开启 + 停用名单双门任一即 Skip
        set_settings(&st, |s| s.buddy_ai_checkin_enabled = true);
        assert!(matches!(
            sched_plan(&st, &json!({}), task("buddy-ai-checkin")),
            SchedPlan::Daily(_)
        ));
        assert!(matches!(
            sched_plan(
                &st,
                &json!({ "disabled_tasks": ["buddy-ai-checkin"] }),
                task("buddy-ai-checkin")
            ),
            SchedPlan::Skip
        ));
        // renew 关闭 → Skip
        set_settings(&st, |s| s.buddy_ai_renew_enabled = false);
        assert!(matches!(
            sched_plan(&st, &json!({}), task("buddy-ai-renew")),
            SchedPlan::Skip
        ));
    }

    /// 看板同步任务的调度计划（issue #82）：默认 every_hours 每 2 小时；
    /// hourly/daily 生效；非法值回 every_hours；间隔小时数可手工设置并 clamp；
    /// off 关闭
    #[test]
    fn board_tasks_default_every_hours() {
        let st = temp_state("board");
        // 默认（无配置）：every_hours 每 2 小时
        assert!(matches!(
            sched_plan(&st, &json!({}), task("trae-credits-snapshot")),
            SchedPlan::EveryHours(2)
        ));
        assert!(matches!(
            sched_plan(&st, &json!({}), task("trae-usage-sync")),
            SchedPlan::EveryHours(2)
        ));
        // 显式 hourly 覆盖：每小时节流
        let hourly = json!({ "task_modes": { "trae-usage-sync": "hourly" } });
        assert!(matches!(
            sched_plan(&st, &hourly, task("trae-usage-sync")),
            SchedPlan::Hourly
        ));
        // 显式 daily 覆盖：回落每日计划并用内置时刻（23:50，与积分同步 23:40 错峰）
        let daily = json!({ "task_modes": { "trae-usage-sync": "daily" } });
        match sched_plan(&st, &daily, task("trae-usage-sync")) {
            SchedPlan::Daily(hhmm) => assert_eq!(hhmm, "23:50"),
            other => panic!("daily 覆盖应为每日计划，实际 {other:?}"),
        }
        // 非法模式值：回落缺省（every_hours 每 2 小时）
        let bogus = json!({ "task_modes": { "trae-usage-sync": "weekly" } });
        assert!(matches!(
            sched_plan(&st, &bogus, task("trae-usage-sync")),
            SchedPlan::EveryHours(2)
        ));
        // wb 缺省同为 every_hours 每 2 小时
        assert!(matches!(
            sched_plan(&st, &json!({}), task("wb-credits-snapshot")),
            SchedPlan::EveryHours(2)
        ));
        // qoder 缺省同为 every_hours 每 2 小时（移植 main：积分快照加入四模式族）
        assert!(matches!(
            sched_plan(&st, &json!({}), task("qoder-credits-snapshot")),
            SchedPlan::EveryHours(2)
        ));
        // 手工间隔生效 + 越界 clamp（1..=24）
        let h6 = json!({ "task_modes": { "wb-credits-snapshot": "every_hours" }, "task_hours": { "wb-credits-snapshot": 6 } });
        assert!(matches!(
            sched_plan(&st, &h6, task("wb-credits-snapshot")),
            SchedPlan::EveryHours(6)
        ));
        let h99 = json!({ "task_modes": { "wb-credits-snapshot": "every_hours" }, "task_hours": { "wb-credits-snapshot": 99 } });
        assert!(matches!(
            sched_plan(&st, &h99, task("wb-credits-snapshot")),
            SchedPlan::EveryHours(24)
        ));
        // off 关闭（disabled_tasks 命中 → Skip）
        let off = json!({ "disabled_tasks": ["trae-usage-sync"] });
        assert!(matches!(
            sched_plan(&st, &off, task("trae-usage-sync")),
            SchedPlan::Skip
        ));
    }

    /// board_platform 映射（emit `board-data-synced` 载荷）：仅看板数据任务有
    /// 平台 key，签到/续期/目录同步等非看板任务返回 None（不触发前端联动）；
    /// buddy-ai-credits-sync → buddy-ai（移植 main T3 审查修复）
    #[test]
    fn board_platform_maps_board_tasks_only() {
        assert_eq!(board_platform("wb-credits-snapshot"), Some("buddy"));
        assert_eq!(board_platform("trae-credits-snapshot"), Some("trae"));
        assert_eq!(board_platform("trae-usage-sync"), Some("trae"));
        assert_eq!(board_platform("qoder-credits-snapshot"), Some("qoder"));
        assert_eq!(board_platform("buddy-ai-credits-sync"), Some("buddy-ai"));
        assert_eq!(board_platform("buddy-ai-checkin"), None);
        assert_eq!(board_platform("buddy-ai-renew"), None);
        assert_eq!(board_platform("buddy-ai-catalog-sync"), None);
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
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            buddy_ai_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
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

    /// 连击纯函数（移植 main #134 方案3，wb-checkin/wb-growth 门禁复用）：
    /// 任一成功（含 already）清零；全轮失败 +1；混合轮不计数（随任一成功清零，
    /// 绝不 +1）；两者皆 0（空池空转）原值不动
    #[test]
    fn buddy_ai_streak_next_rules() {
        // 全轮失败：+1
        assert_eq!(buddy_ai_streak_next(0, 0, 2), 1);
        assert_eq!(buddy_ai_streak_next(2, 0, 1), 3);
        // 任一成功（含 already）：清零
        assert_eq!(buddy_ai_streak_next(2, 1, 0), 0);
        // 混合（部分成功）：不计数——按「任一成功清零」归零，绝不 +1
        assert_eq!(buddy_ai_streak_next(2, 1, 1), 0);
        assert_eq!(buddy_ai_streak_next(2, 3, 2), 0);
        // 两者皆 0（空池空转）：原值不动
        assert_eq!(buddy_ai_streak_next(4, 0, 0), 4);
        assert_eq!(buddy_ai_streak_next(0, 0, 0), 0);
    }

    /// 连击持久化与跨日重置（审查 No.2 补测）：kv 文档 {streak, date} 同文档单次写入；
    /// date ≠ 今日（跨日/无记录/损坏）一律从 0 起；签到/成长双键独立计数
    #[test]
    fn wb_round_fail_streak_persist_and_day_reset() {
        let st = temp_state("wb_streak");
        let today = "2026-01-30";
        // 无记录 → 0
        assert_eq!(wb_round_fail_streak(&st, WB_CHECKIN_STREAK_KEY, today), 0);
        // 写 2 读回 2
        wb_round_fail_streak_save(&st, WB_CHECKIN_STREAK_KEY, today, 2);
        assert_eq!(wb_round_fail_streak(&st, WB_CHECKIN_STREAK_KEY, today), 2);
        // 原始文档含 date + streak 双字段（计数与日期同文档防半写分叉）
        let raw: Value = crate::store::db(&st.data_dir).kv_get(WB_CHECKIN_STREAK_KEY);
        assert_eq!(raw.get("streak").and_then(Value::as_i64), Some(2));
        assert_eq!(raw.get("date").and_then(Value::as_str), Some(today));
        // 跨日（date ≠ today）→ 从 0 起（次日自动恢复）
        assert_eq!(wb_round_fail_streak(&st, WB_CHECKIN_STREAK_KEY, "2026-01-31"), 0);
        // 双键独立：growth 键不受 checkin 键影响
        assert_eq!(wb_round_fail_streak(&st, WB_GROWTH_STREAK_KEY, today), 0);
        wb_round_fail_streak_save(&st, WB_GROWTH_STREAK_KEY, today, 3);
        assert_eq!(wb_round_fail_streak(&st, WB_GROWTH_STREAK_KEY, today), 3);
        assert_eq!(wb_round_fail_streak(&st, WB_CHECKIN_STREAK_KEY, today), 2);
        // 损坏文档（streak 非数值）→ 0，不 panic 不误判
        let _ = crate::store::db(&st.data_dir)
            .kv_set(WB_GROWTH_STREAK_KEY, &json!({ "streak": "corrupted", "date": today }));
        assert_eq!(wb_round_fail_streak(&st, WB_GROWTH_STREAK_KEY, today), 0);
    }

    /// 奖励型签到空池豁免（审查修复，对齐 buddy-ai 口径）：trae/wb/qoder
    /// 签到空池时 run_task 返回 skipped_empty_pool——tick 据此不 mark_run，当日
    /// 导入账号下一 tick 正常补打；早退在轮次锁/网络请求之前，测试无副作用
    #[test]
    fn reward_checkin_empty_pool_exemption() {
        for key in ["trae-checkin", "wb-checkin", "qoder-checkin"] {
            let st = temp_state("reward_empty_pool");
            let v = run_task(key, &st).expect("空池应为 Ok 跳过");
            assert_eq!(
                v.get("skipped_empty_pool").and_then(Value::as_bool),
                Some(true),
                "{key} 空池须打 skipped_empty_pool（否则 mark_run 固化当日已跑，当日导入账号丢补打）"
            );
            assert!(v.get("skipped").and_then(Value::as_str).is_some(), "{key} 空池须带跳过原因");
            assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
        }
    }
}
