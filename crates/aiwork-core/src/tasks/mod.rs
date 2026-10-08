//! 后台任务域（自 src-tauri/src/tasks 平移，T5/T8）：
//! 签到/积分任务实现 + 统一 ureq agent（直连语义：不读系统/环境代理）。
//! 豆包任务（doubao_*）与 UI 点击兜底（ui_click）随桌面壳退役，不平移。

pub mod qoder_catalog;
pub mod qoder_checkin;
pub mod qoder_common;
pub mod qoder_credits;
pub mod qoder_device;
pub mod qoder_oauth;
pub mod qoder_refresh;
pub mod qoder_sign;
pub mod qoder_upstream;
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
        // Qoder 每日签到（F-80；CLI 直调 + 应用内调度器共用；10:15 单次覆盖双活动）
        "qoder-checkin" => {
            let opts = qoder_checkin::QoderCheckinOpts::daily();
            let done = qoder_checkin::run_checkin_round(state, &opts, &mut |ev| {
                if ev.get("type").and_then(serde_json::Value::as_str) != Some("done") {
                    println!("{}", serde_json::to_string(ev).unwrap_or_default());
                }
            });
            // 审查 M-1：存在失败账号时返 Err（CLI 退出码可见 + 通知渠道）。
            // 口径对齐调度器（scheduler.rs 同款）：empty_campaigns（活动未上线/不可用）
            // 属非用户可操作失败，不计失败——CLI 进程本身无 30min 冷却重试链，
            // 重试由次日调度覆盖；done 事件恒携带该字段，旧结构缺字段时按 0 兜底
            let failed = done.get("failed").and_then(serde_json::Value::as_i64).unwrap_or(0);
            let failed_empty = done
                .get("failed_empty_campaigns")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            // 永久性认证失败与调度器口径对齐（scheduler.rs 同款）：重试注定失败，
            // 不计入 Err（否则失效账号让 CLI 每日执行结果恒为失败）
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
        // Qoder 积分快照（调度器/CLI 共用；空池自然空转）
        "qoder-credits-snapshot" => qoder_credits::run_snapshot_task(state),
        // Qoder 凭证 6h 兜底刷新（调度器/CLI 共用；空池空转）
        "qoder-refresh" => qoder_refresh::run_task(state),
        // Qoder 模型目录每日同步（调度器/CLI 共用；空池/无凭证空转，p3-3 收尾）
        "qoder-catalog-sync" => qoder_catalog::run_task(state),
        // 刷新全部账号剩余积分（credits_daily 快照按日重算）
        "refresh-credits" => crate::commands::accounts::refresh_remaining_credits_impl(state)
            .map(|n| serde_json::json!({ "refreshed": n, "snapshot": "credits_daily.json" })),
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
        other => Err(format!("未知任务: {other}")),
    };
    match result {
        Ok(v) => { println!("{}", serde_json::to_string(&v).unwrap_or_default()); 0 }
        Err(e) => { println!("{}", serde_json::json!({"ok": false, "error": e})); 1 }
    }
}
