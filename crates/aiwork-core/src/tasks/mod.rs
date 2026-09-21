//! 后台任务域（自 src-tauri/src/tasks 平移，T5/T8）：
//! 签到/积分任务实现 + 统一 ureq agent（直连语义：不读系统/环境代理）。
//! 豆包任务（doubao_*）与 UI 点击兜底（ui_click）随桌面壳退役，不平移。

pub mod trae_checkin;
pub mod wb_checkin;
pub mod wb_common;
pub mod wb_credits;

use crate::state::AppState;

/// 构建 ureq agent（统一出口）。
/// 直连语义：ureq 默认不读系统/环境代理（对齐 python OPENER 绕代理约定），
/// 单请求可再用 `.timeout()` 覆盖。
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

/// CLI 任务分发（aiwork-server `--task-run` 兜底入口）。
/// 成功输出任务结果 JSON，失败输出 {"ok":false,"error":...}；返回进程退出码。
pub fn run_cli_task(name: &str, state: &AppState) -> i32 {
    let result = match name {
        // Trae 每日签到：vault 解密全量账号跑单轮（状态核验幂等）
        "checkin" => {
            let accounts = crate::vault::load_accounts(state);
            let retry = state.settings().retry.max(0) as u32;
            Ok(trae_checkin::run_round(state, &accounts.accounts, retry, &mut |ev| {
                println!("{}", serde_json::to_string(ev).unwrap_or_default());
            }))
        }
        // WorkBuddy 每日签到（--json-stream --skip-checked 同款参数）
        "wb-checkin" => Ok(wb_checkin::run_checkin_round(
            state,
            &wb_checkin::CheckinOpts::daily(),
            &mut |ev| println!("{}", serde_json::to_string(ev).unwrap_or_default()),
        )),
        // WorkBuddy token 兜底续期（lazy 24h）
        "wb-renew" => Ok(wb_checkin::run_renew_only(state, 24)),
        // 刷新全部账号剩余积分（credits_daily 快照按日重算）
        "refresh-credits" => crate::commands::accounts::refresh_remaining_credits_impl(state)
            .map(|n| serde_json::json!({ "refreshed": n, "snapshot": "credits_daily.json" })),
        other => Err(format!("未知任务: {other}")),
    };
    match result {
        Ok(v) => { println!("{}", serde_json::to_string(&v).unwrap_or_default()); 0 }
        Err(e) => { println!("{}", serde_json::json!({"ok": false, "error": e})); 1 }
    }
}
