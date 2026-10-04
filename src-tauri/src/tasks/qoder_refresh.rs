//! Qoder 凭证 6h 定时刷新兜底（F-80 M4）。
//!
//! 现有 `ensure_fresh` 为「用时惰性刷新」（签到/积分查询前临期才刷）——应用长开但
//! 用户不触发任何 Qoder 操作时，凭证可能静默过期。本任务为后台兜底：每 6h 遍历
//! 账号池逐账号执行 `ensure_fresh(lazy_hours=7)`：
//! - 客户端 token（dt-/jt- 作业令牌）：<7h 走 deviceToken/refresh 续期；
//! - PAT 通道：作业令牌过期后自动用原始 PAT 重换（PAT 长期有效）。
//! 惰性窗口 7h > 调度间隔 6h，保证任一 tick 必落在窗口内（凭证永不过夜）。
//! 顺带 best-effort fresh 拉取一次积分（暖缓存；同日快照覆盖不产生多余历史点）。
//! 空池空转；单账号失败不阻塞其余账号（ensure_fresh 内部自容错）。

use serde_json::{json, Value};

use crate::state::AppState;

use super::{http_agent, qoder_common, qoder_credits};

/// 惰性刷新窗口（小时）：必须大于调度间隔 6h，兜底 tick 才必然命中
const LAZY_HOURS: i64 = 7;

/// 调度器/CLI 共用入口：全池凭证兜底刷新 + 积分 fresh 拉取
pub fn run_task(state: &AppState) -> Result<Value, String> {
    // 跨进程互斥（审查 P1，同 qoder_checkin）：6h 调度 tick 与 schtasks CLI 同刻
    // 双进程全池刷新，双进程对同账号并发 ensure_fresh 会以同一 refresh_token 刷新
    //（服务端一次性轮换下后到者误标 needs_relogin）。抢锁失败幂等跳过；
    // 失败原因落日志与返回值——锁创建失败（机制不可用）≠ 他方占用，混报会掩盖根因
    //（2026-10-04 锁名多段路径 err=3 曾致每分钟空转 skip 54+ 分钟）
    let (_cross, lock_fail) = qoder_common::CrossProcLock::try_acquire(&state.data_dir, "refresh", 3_000);
    let Some(_cross) = _cross else {
        let reason = lock_fail.as_ref().map(|f| f.describe()).unwrap_or_default();
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("[qoder] Qoder 凭证刷新未执行（{reason}），本轮幂等跳过"),
        );
        // skipped 文案与日志同源（P3 审查修复）：写死「另一进程正在刷新」会在
        // 锁创建失败时误导 CLI/--task-run 输出与日志矛盾
        return Ok(json!({ "ok": true, "skipped": format!("{reason}，本轮幂等跳过"), "skipped_busy": true }));
    };
    let pool: Value = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
    let accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if accounts.is_empty() {
        return Ok(json!({ "ok": true, "skipped": "无 Qoder 账号" }));
    }
    let agent = http_agent(30);
    let mut refreshed = 0u32;
    let mut failed = 0u32;
    let mut permanent = 0u32;
    for a in &accounts {
        let Some(id) = a.get("id").and_then(Value::as_str) else {
            continue;
        };
        let (_creds, _did, note) = qoder_common::ensure_fresh(state, &agent, id, LAZY_HOURS);
        match note {
            "refreshed" => refreshed += 1,
            // 暂态失败（网络/服务端）：返 Err 交调度器 30 分钟冷却重试，可能自行恢复
            "refresh_failed" => failed += 1,
            // 永久失败（PAT 被拒 / 凭证过期需重登 / refresh_token 被服务端 4xx 永久拒绝
            // ——P1 分类后 auth_dead 已在 ensure_fresh 内回写池 needs_relogin）：
            // 重试无解。返 Ok 停止 30 分钟无效重试循环（原实现计失败会全天 48 次 tick
            // 全部失败 + 当日首败误报通知）；落日志提示用户人工处理（用户重登后下个
            // 6h tick 自然恢复）
            "pat_rejected" | "expired_needs_relogin" | "auth_dead" => {
                permanent += 1;
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!("[qoder] 账号 {id} 凭证需人工处理（{note}），请重新登录或更新 PAT"),
                );
            }
            // no_credential / fresh：无需刷新
            _ => {}
        }
    }
    // 积分兜底拉取（best-effort：token 刷新才是主目标，失败不上抛）
    let credits_ok = qoder_credits::fetch_credits(state, None, true)
        .map(|v| v.get("ok").and_then(Value::as_bool).unwrap_or(false))
        .unwrap_or(false);
    // 暂态失败返 Err：调度器按失败处理（记 last_fail_ts，30 分钟冷却后重试），
    // 避免 ok:false 被误判为成功而错过当日后续兜底；CLI `--task-run` 同样输出 ok:false 退出码 1
    if failed > 0 {
        return Err(format!(
            "刷新完成：{refreshed} 成功 / {failed} 暂态失败 / {permanent} 需重登（共 {} 账号，30 分钟后重试）",
            accounts.len()
        ));
    }
    Ok(json!({
        "ok": true,
        "accounts": accounts.len(),
        "refreshed": refreshed,
        "failed": 0,
        "needs_relogin": permanent,
        "credits_ok": credits_ok,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 惰性窗口必须大于调度间隔（6h），保证任一兜底 tick 落在窗口内
    #[test]
    fn lazy_window_covers_schedule_interval() {
        assert!(LAZY_HOURS > 6, "LAZY_HOURS={LAZY_HOURS} 未覆盖 6h 调度间隔");
    }
}
