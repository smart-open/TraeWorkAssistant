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
pub mod doubao_quota;
pub mod doubao_session;
pub mod scheduler;
pub mod trae_checkin;
pub mod ui_click;
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
        // WorkBuddy 每日签到（schtasks 直调；--json-stream --skip-checked 同款参数）
        "wb-checkin" => Ok(wb_checkin::run_checkin_round(
            state,
            &wb_checkin::CheckinOpts::daily(),
            &mut print_progress,
        )),
        // WorkBuddy 每周兜底续期（对齐 python --renew-only，lazy 24h）
        "wb-renew" => Ok(wb_checkin::run_renew_only(state, 24)),
        // WorkBuddy 每日成长（任务配置页；调度器/CLI 共用，成长三开关驱动）
        "wb-growth" => {
            let s = crate::commands::workbuddy::load_settings(state);
            let flags = wb_checkin::GrowthOpts {
                travel: s.growth_travel,
                lottery: s.growth_lottery,
                tasks: s.growth_tasks,
            };
            wb_checkin::run_growth_round(state, &flags, &[], &mut print_progress);
            Ok(serde_json::json!({ "ok": true }))
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
            let sink = crate::switcher::CliSink::new(&state.data_dir);
            crate::switcher::run_action(
                crate::switcher::RunArgs {
                    action: crate::switcher::Action::KeepAlive,
                    target_app: crate::switcher::TargetApp::Doubao,
                    user_id: None,
                    proxy_port: None,
                    include_indexeddb: false,
                    expected_current_uid: String::new(),
                    data_dir: state.data_dir.clone(),
                },
                &sink,
            )
            .map(|_| serde_json::json!({ "ok": true }))
        }
        // 刷新全部账号剩余积分：按积分包 CycleStartTime 归日口径重算 credits_daily
        // 快照（今日 earned + API 可见历史修正），无需启动 GUI
        "refresh-credits" => crate::commands::accounts::refresh_remaining_credits_impl(state)
            .map(|n| serde_json::json!({ "refreshed": n, "snapshot": "credits_daily.json" })),
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
