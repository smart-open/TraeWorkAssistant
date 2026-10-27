//! 后台任务域（Python → Rust 重写）：原 src-python 运行时脚本的 Rust 实现。
//!
//! 布局：
//! - `wb_common`：WorkBuddy 公共请求层（双源凭证/统一请求头/区域路由/token 刷新/本地 quota 兜底）
//! - `wb_credits`：M5 积分三件套查询（F-20/F-22）
//! - `wb_checkin`：WorkBuddy 签到/成长中心/token 续期（F-15/F-16/F-55/F-09/F-17）
//! - `trae_checkin`：Trae 多账号签到单轮执行（status/claim/错误分类/冷却/积分三层兜底）
//! - `doubao_quota`：豆包会员额度查询（单账号 + 批量巡检）
//! - `doubao_session`：豆包会话续期巡检（DPAPI+AES-GCM 诊断 / 两段式探活 / 槽位预检）
//! - `doubao_chats`：豆包 Cookies 登录态检测 / Local Storage leveldb uid 探测 / IM API 对话导出
//! - `ui_click`：WorkBuddy UI 坐标点击兜底（F-18，windows-sys user32）
//!
//! CLI 任务模式（D2）：schtasks 计划任务直调主 exe，`--task-run <name>` 执行后退出，
//! 不启动 Tauri（分支在 Builder 之前，天然绕开单实例插件）。任务名随批次扩展：
//! P1 `doubao-quota`；P2 `wb-checkin`/`wb-renew`/`checkin`；P3 `doubao-renew`。

pub mod doubao_chats;
pub mod doubao_checkin;
pub mod doubao_quota;
pub mod doubao_session;
pub mod qoder_catalog;
pub mod qoder_checkin;
pub mod qoder_common;
pub mod qoder_credits;
pub mod qoder_device;
pub mod qoder_oauth;
pub mod qoder_refresh;
pub mod qoder_sign;
pub mod qoder_upstream;
pub mod scheduler;
pub mod trae_checkin;
pub mod ui_click;
pub mod buddy_ai_checkin;
pub mod wb_checkin;
pub mod wb_common;
pub mod wb_credits;

use crate::state::AppState;

/// 构建 ureq agent（统一出口）。
/// 直连语义：ureq 默认不读系统/环境代理（对齐 python OPENER 绕代理约定），
/// 本地 MITM 代理死端口不会劫持本域出网请求；单请求可再用 `.timeout()` 覆盖。
pub fn http_agent(timeout_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
}

/// 解析 `--task-run <name>` CLI 参数（`--task-run` 为首个参数时进入任务模式）。
pub fn parse_task_mode(args: &[String]) -> Option<String> {
    if args.len() >= 2 && args[1] == "--task-run" {
        return args.get(2).cloned().filter(|s| !s.is_empty());
    }
    None
}

/// Qoder 会话流水合并保留窗天数（审查 F12）：统一读 settings.qoder_sessions_compact_days
/// （非法/缺失回退默认值，越界 clamp 1..=90），调度器与应用内/CLI 手工触发同源，
/// 防两入口口径漂移
pub(crate) fn qoder_sessions_compact_days(state: &AppState) -> i64 {
    state
        .settings()
        .qoder_sessions_compact_days
        .trim()
        .parse::<i64>()
        .unwrap_or(crate::store::docs::QODER_SESSIONS_COMPACT_AFTER_DAYS)
        .clamp(1, 90)
}

/// 豆包保活前置检查（issue #147）：返回 Some(跳过原因) 时不应拉起客户端。
/// ① 账号池为空 → 无可续期的豆包会话；② 六级发现找不到 Doubao.exe →
/// 启动注定失败（避免每日失败日志与当日首败通知）
pub fn doubao_keepalive_skip_reason(state: &AppState) -> Option<String> {
    if crate::commands::doubao::load_pool(state).accounts.is_empty() {
        return Some("无豆包账号".into());
    }
    let args = crate::switcher::RunArgs {
        action: crate::switcher::Action::KeepAlive,
        target_app: crate::switcher::TargetApp::Doubao,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        machine_id_override: None,
        data_dir: state.data_dir.clone(),
    };
    let mut sess = crate::switcher::Session::new(&args);
    if crate::switcher::locate::find_exe(&mut sess).is_none() {
        return Some("未找到豆包客户端（Doubao.exe）".into());
    }
    None
}

/// CLI 任务分发：执行后返回进程退出码。
/// stdout 末行 JSON（对齐原 python 脚本输出约定，便于排查与双轨对照）；
/// 成功输出任务结果，失败输出 {"ok":false,"error":...}。
/// 签到类任务的 start/account 进度事件逐行打印（NDJSON 同款），
/// 末尾 done 事件由统一出口打印，避免重复。
pub fn run_cli_task(name: &str, state: &AppState) -> i32 {
    // SQLite 启动迁移（与 GUI main.rs setup 同款幂等语义）：CLI 分支先于 Tauri
    // setup 退出，若不在此补齐，升级后首次启动 GUI 前触发的计划任务会对着
    // store::db() 刚建的空库运行——账号/池/设置全为默认，任务静默空转，
    // 且窗口内写入的数据会被 GUI 首启迁移整表覆盖。
    if let Some(summary) = crate::store::migrate::migrate_on_startup(&state.data_dir) {
        crate::fs_utils::app_log(&state.data_dir, &summary);
    }
    /// 打印除 done 外的 NDJSON 进度事件（done 由 run_cli_task 统一收尾输出）
    fn print_progress(ev: &serde_json::Value) {
        if ev.get("type").and_then(serde_json::Value::as_str) != Some("done") {
            println!("{}", serde_json::to_string(ev).unwrap_or_default());
        }
    }
    let result = match name {
        "doubao-quota" => doubao_quota::run_batch(state),
        // 豆包每日签到（需求③：schtasks 直调 + 应用内调度器共用；空池跳过）
        "doubao-checkin" => doubao_checkin::run_batch(state),
        // WorkBuddy 每日签到（schtasks 直调；--json-stream --skip-checked 同款参数）。
        // issue #134 方案4：抢完整轮次锁与 GUI 进程互斥——CLI 是独立进程，GUI 进程内
        // 的 tokio 轮次锁对它无效，必须靠跨进程命名互斥体；GUI 正在跑（UI/调度器/
        // 启动补签/托盘）时幂等跳过，避免双进程并发落盘踩踏
        "wb-checkin" => {
            match crate::commands::workbuddy::try_acquire_wb_round("CLI 签到", &state.data_dir) {
                Ok(_round) => Ok(wb_checkin::run_checkin_round(
                    state,
                    &wb_checkin::CheckinOpts::daily(),
                    &mut print_progress,
                )),
                Err(why) => Ok(serde_json::json!({ "ok": true, "skipped": why, "skipped_busy": true })),
            }
        }
        // WorkBuddy 每周兜底续期（对齐 python --renew-only，lazy 24h）
        "wb-renew" => Ok(wb_checkin::run_renew_only(state, 24)),
        // WorkBuddy 每日成长（任务配置页；调度器/CLI 共用，成长三开关驱动）。
        // issue #134 方案4：抢完整轮次锁同 wb-checkin
        "wb-growth" => {
            let s = crate::commands::workbuddy::load_settings(state);
            let flags = wb_checkin::GrowthOpts {
                travel: s.growth_travel,
                lottery: s.growth_lottery,
                tasks: s.growth_tasks,
            };
            match crate::commands::workbuddy::try_acquire_wb_round("CLI 成长", &state.data_dir) {
                Ok(_round) => {
                    wb_checkin::run_growth_round(state, &flags, &[], &mut print_progress);
                    Ok(serde_json::json!({ "ok": true }))
                }
                Err(why) => Ok(serde_json::json!({ "ok": true, "skipped": why, "skipped_busy": true })),
            }
        }
        // Qoder 每日签到（F-80；schtasks 直调 + 应用内调度器共用；10:15 单次覆盖双活动）
        "qoder-checkin" => {
            let opts = qoder_checkin::QoderCheckinOpts::daily();
            let done = qoder_checkin::run_checkin_round(state, &opts, None, &mut print_progress);
            // 审查 M-1：存在失败账号时返 Err（schtasks 退出码可见 + 通知渠道）。
            // 口径对齐调度器（scheduler.rs 同款）：empty_campaigns（活动未上线/不可用）
            // 属非用户可操作失败，不计失败——CLI 进程本身无 30min 冷却重试链，
            // 重试由次日调度覆盖；done 事件恒携带该字段，旧结构缺字段时按 0 兜底
            let failed = done.get("failed").and_then(serde_json::Value::as_i64).unwrap_or(0);
            let failed_empty = done
                .get("failed_empty_campaigns")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            // 永久性认证失败与调度器口径对齐（scheduler.rs 同款）：重试注定失败，
            // 不计入 Err（否则失效账号让 schtasks 每日执行结果恒为失败）
            let failed_permanent = done
                .get("failed_permanent")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            if failed - failed_empty - failed_permanent > 0 {
                let ok = done.get("ok").and_then(serde_json::Value::as_i64).unwrap_or(0);
                let already = done.get("already").and_then(serde_json::Value::as_i64).unwrap_or(0);
                Err(format!(
                    "Qoder 签到：{ok} 成功 / {already} 已领 / {failed} 失败（稍后自动重试）"
                ))
            } else {
                Ok(done)
            }
        }
        // Qoder 积分快照（调度器/CLI 共用；空池自然空转）。runtime=None：CLI
        // 独立进程无网关运行态，仅落 doc 快照不做内存池拨新
        "qoder-credits-snapshot" => qoder_credits::run_snapshot_task(state, None),
        // Qoder 凭证 6h 兜底刷新（调度器/CLI 共用；空池空转）
        "qoder-refresh" => qoder_refresh::run_task(state),
        // Qoder 模型目录每日同步（调度器/CLI 共用；空池/无凭证空转，p3-3 收尾）
        "qoder-catalog-sync" => qoder_catalog::run_task(state),
        // Buddy 国际版积分数据同步 / 凭证兜底续期 / 模型目录同步（调度器/CLI 共用；
        // runtime=None：CLI 独立进程无网关运行态，仅落库不做运行池拨新）
        "buddy-ai-credits-sync" => crate::commands::workbuddy::buddy_ai_credits_sync_impl(state, None)
            .map(|v| serde_json::json!({ "ok": true, "accounts": v.len() })),
        "buddy-ai-renew" => crate::commands::workbuddy::buddy_ai_renew_task(state),
        "buddy-ai-catalog-sync" => crate::commands::workbuddy::buddy_ai_catalog_sync_impl(state)
            .map(|n| serde_json::json!({ "ok": true, "models": n })),
        // Buddy 国际版每日活跃打卡（T4 审查修复：调度器有 buddy-ai-checkin 分支，
        // run_cli_task 却无对应分发——`--task-run buddy-ai-checkin` 落入未知任务
        // 报错退出）。口径对齐 scheduler.rs 同名分支：抢轮次锁与 UI 路径互斥
        // （skipped_busy 幂等跳过）；存在失败账号返 Err（退出码可见，NDJSON 事件
        // 已逐行打印失败明细）。不设 permanent 剔除：buddy-ai 凭证 401 可经续期/
        // 401 自愈轮换新值重试成功（T7 归一语义），CLI 无冷却重试链、次日调度覆盖
        "buddy-ai-checkin" => {
            match crate::commands::workbuddy::try_acquire_buddy_ai_round() {
                Ok(_round) => {
                    // runtime=None：CLI 独立进程无网关运行态，打卡到账仅落持久池
                    let done = buddy_ai_checkin::run_checkin_round(state, None, &mut print_progress);
                    let ok = done.get("ok").and_then(serde_json::Value::as_i64).unwrap_or(0);
                    let already = done
                        .get("already")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    let failed = done
                        .get("failed")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    if failed > 0 {
                        Err(format!(
                            "buddy-ai 打卡：{ok} 成功 / {already} 已完成 / {failed} 失败"
                        ))
                    } else {
                        Ok(done)
                    }
                }
                Err(_) => Ok(serde_json::json!({
                    "ok": true,
                    "skipped": "已有 buddy-ai 打卡任务在执行中，本轮跳过",
                    "skipped_busy": true
                })),
            }
        }
        // Trae JWT 定时续期（issue #27；调度器/CLI 共用批量惰性刷新）
        "trae-renew" => crate::commands::accounts::renew_due_accounts_impl(state),
        // Trae 每日签到：vault 解密全量账号跑单轮
        //（修复 python 直读 checkin_accounts.json 占位文件导致全部「未配置 jwt」的隐性失效）
        "checkin" => {
            let accounts = crate::vault::load_accounts(state);
            let retry = state.settings().retry.max(0) as u32;
            Ok(trae_checkin::run_round(state, &accounts.accounts, retry, &mut print_progress))
        }
        // 豆包会话续期巡检（诊断 + 网络续期；schtasks 每日任务走 PS 桥 KeepAlive，此为手动巡检入口）
        "doubao-renew" => doubao_session::run(state, false, None),
        // TRAE 本地登录态捕获（原 device_proxy.py --capture-local 兜底迁移）：
        // MITM 抓不到鉴权头时，解密 TRAE 本地 Cookies + 扫描 leveldb 提取 Cloud-IDE-JWT 写回
        "trae-capture-local" => crate::device_proxy::local_capture::capture_from_local(state),
        // 豆包会话保活（schtasks 直调主 exe；原 PS 桥 KeepAlive 已 Rust 化）：
        // 进度 NDJSON 打印到 stdout（任务日志可查），终态 {"ok":bool}
        "doubao-keepalive" => {
            // 前置检查（issue #147）：无账号/客户端缺失时静默跳过，不拉起客户端
            if let Some(reason) = doubao_keepalive_skip_reason(state) {
                Ok(serde_json::json!({ "ok": true, "skipped": reason }))
            } else {
                let sink = crate::switcher::CliSink::new(&state.data_dir);
                crate::switcher::run_action(
                    crate::switcher::RunArgs {
                        action: crate::switcher::Action::KeepAlive,
                        target_app: crate::switcher::TargetApp::Doubao,
                        user_id: None,
                        proxy_port: None,
                        include_indexeddb: false,
                        expected_current_uid: String::new(),
                        machine_id_override: None,
                        data_dir: state.data_dir.clone(),
                    },
                    &sink,
                )
                .map(|_| serde_json::json!({ "ok": true }))
            }
        }
        // 刷新全部账号剩余积分：按积分包 CycleStartTime 归日口径重算 credits_daily
        // 快照（今日 earned + API 可见历史修正），无需启动 GUI。
        // runtime=None：CLI 独立进程，不存在网关运行态
        "refresh-credits" => {
            crate::commands::accounts::refresh_remaining_credits_impl(state, None)
                .map(|n| serde_json::json!({ "refreshed": n, "snapshot": "credits_daily.json" }))
        }
        // Trae 消耗明细同步（issue #61；调度器/CLI 共用；无账号跳过，全部账号拉取失败返 Err）
        "trae-usage-sync" => {
            let accounts = crate::vault::load_accounts(state);
            if accounts.accounts.is_empty() {
                Ok(serde_json::json!({ "ok": true, "skipped": "无 Trae 账号" }))
            } else {
                crate::commands::usage_history::usage_history_fetch_impl(state, true)
                    .map(|r| serde_json::json!({ "ok": true, "accounts": r.accounts.len() }))
            }
        }
        // Qoder 会话流水定时合并（调度器/CLI 共用；纯本地 SQLite 无网络，幂等）。
        // 不注册 schtasks——本地任务无「应用未启动也要跑」诉求，CLI 入口仅供手工触发/排查。
        // 保留天数与调度器同源：读 settings.qoder_sessions_compact_days（审查 F12）
        "qoder-sessions-compact" => {
            let days = qoder_sessions_compact_days(state);
            crate::store::docs::qoder_sessions_compact(&crate::store::db(&state.data_dir), days)
                .map(|(merged, deleted)| {
                    serde_json::json!({ "ok": true, "merged": merged, "deleted": deleted, "days": days })
                })
        }
        other => {
            eprintln!("未知任务: {other}");
            println!("{}", serde_json::json!({"ok": false, "error": format!("未知任务: {other}")}));
            return 1;
        }
    };
    match result {
        Ok(v) => {
            println!("{}", serde_json::to_string(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            println!("{}", serde_json::json!({"ok": false, "error": e}));
            1
        }
    }
}
