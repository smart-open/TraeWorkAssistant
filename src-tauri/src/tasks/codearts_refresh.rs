//! CodeArts OAuth 凭据 6h 定时刷新兜底（对齐 qoder_refresh）。
//!
//! 现有 `ensure_fresh_creds` 为「用时惰性刷新」（请求前临期才刷，窗口 30min）：
//! 应用长开但用户不触发请求时，凭证可能静默过期；且 30min 窗口太短，一旦
//! refresh_token 因他进程争抢被消耗（STS5.1806「has been used」），健康账号
//! 立即掉线。本任务后台兜底：
//! - 每 6h 遍历池内 OAuth 账号，提前 7h 窗口执行续期（窗口 > 间隔保证必命中）；
//! - 跨进程互斥：安装版调度 tick 与 CLI `--task-run` 同刻刷新同账号会以同一
//!   refresh_token 轮换，后到者拿到失效 token 被误标 needs_relogin（实测事故根因）；
//! - 永久失败（refresh_token 被拒/无续期材料）：落池 needs_relogin 徽标，停止
//!   无效重试；暂态失败（网络/服务端）返 Err 交调度器 30min 冷却后重试。
//! 空池空转；单账号失败不阻塞其余账号。

use serde_json::{json, Value};

use crate::state::AppState;

use super::codearts_common;

/// 惰性刷新窗口（小时）：必须大于调度间隔 6h，保证任一兜底 tick 落在窗口内。
/// 对 OAuth STS 临时凭据（有效期数小时）用 7h 意味着「到期前 7h 就开始续」——
/// 比用时惰性 30min 提前得多，留足重试余量。
const LAZY_HOURS: i64 = 7;

/// 标记账号需重新登录（对照 qoder_common::mark_needs_relogin 同语义落库）：
/// refresh_token 一次性轮换下被服务端拒绝即不可恢复，重试无解——回写池
/// needs_relogin 后，看板徽标与调度跳过即时生效（用户重登后自然恢复）。
fn mark_needs_relogin(state: &AppState, acct_id: &str, reason: &str) {
    let _guard = state
        .codearts_pool_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let db = crate::store::db(&state.data_dir);
    let mut pool = crate::store::docs::codearts_pool_load_checked(&db)
        .unwrap_or_else(|_| crate::store::docs::codearts_pool_load(&db));
    let mut changed = false;
    if let Some(accounts) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for a in accounts.iter_mut() {
            if a.get("id").and_then(Value::as_str) == Some(acct_id) {
                a["needs_relogin"] = json!(true);
                a["relogin_reason"] = json!(reason);
                changed = true;
            }
        }
    }
    if changed {
        let _ = crate::store::docs::codearts_pool_save(&db, &pool);
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("[codearts] 账号 {acct_id} 已标记需重新登录（{reason}）"),
        );
    }
}

/// 单账号续期结果分类（供计数与提示）
enum RenewOutcome {
    /// 凭据已刷新并落库
    Refreshed,
    /// 未到窗口 / 手工 AK/SK（无需刷新）
    Fresh,
    /// 暂态失败（网络/服务端/vault）：可重试
    Transient,
    /// 永久失败（refresh_token 被拒 / 缺材料）：需重新登录
    Permanent(String),
}

/// 按指定惰性窗口续期单账号（调度任务与 CLI 共用核心）。
fn renew_account(state: &AppState, acct_id: &str) -> RenewOutcome {
    let data_dir = &state.data_dir;
    let initial = match codearts_common::effective_creds(data_dir, acct_id) {
        Ok(c) => c,
        Err(e) => {
            // vault 系统性故障凭证读空：暂态，不得标记永久需重登
            crate::fs_utils::app_log(data_dir, &format!("[codearts] 账号 {acct_id} 凭证读取失败: {e}"));
            return RenewOutcome::Transient;
        }
    };
    if !initial.is_oauth() {
        return RenewOutcome::Fresh; // 手工 AK/SK 长期有效，无需续期
    }
    let lead = LAZY_HOURS * 3600;
    if !initial.needs_refresh(lead) && initial.expires_at > 0 {
        return RenewOutcome::Fresh;
    }
    if initial.refresh_token.is_empty()
        || initial.code_verifier.is_empty()
        || initial.dpop_key_pem.is_empty()
    {
        return RenewOutcome::Permanent("凭据缺少续期材料（refresh_token/verifier/DPoP key）".into());
    }
    // 进程内互斥（`ensure_fresh_creds` 用同一把锁，避免与请求热路径并发刷新）
    let creds = match codearts_common::ensure_fresh_creds(data_dir, acct_id) {
        Ok(c) => c,
        Err(e) => {
            // ensure_fresh_creds 仅在「旧凭据也已过期」时才 Err（未过期时沿用旧凭据）
            if initial.usable_now() {
                return RenewOutcome::Transient;
            }
            return RenewOutcome::Permanent(e);
        }
    };
    // 真的换了新凭据（到期时间推后）才算 refreshed
    if creds.expires_at > initial.expires_at {
        RenewOutcome::Refreshed
    } else {
        RenewOutcome::Fresh
    }
}

/// 调度器/CLI 共用入口：全池凭证兜底刷新
pub fn run_task(state: &AppState) -> Result<Value, String> {
    // 跨进程互斥：6h 调度 tick 与 schtasks CLI 同刻双进程刷新同账号会互相
    // 消耗一次性 refresh_token（STS5.1806）。抢锁失败幂等跳过。
    let (_cross, lock_fail) = codearts_common::CrossProcLock::try_acquire(
        &state.data_dir,
        "refresh",
        3_000,
    );
    let Some(_cross) = _cross else {
        let reason = lock_fail;
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("[codearts] 凭证刷新未执行（{reason}），本轮幂等跳过"),
        );
        return Ok(json!({ "ok": true, "skipped": format!("{reason}，本轮幂等跳过"), "skipped_busy": true }));
    };

    let db = crate::store::db(&state.data_dir);
    let pool = crate::store::docs::codearts_pool_load_checked(&db)
        .map_err(|e| format!("CodeArts 账号池读取失败: {e}"))?;
    let accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if accounts.is_empty() {
        return Ok(json!({ "ok": true, "skipped": "无 CodeArts 账号" }));
    }

    let mut refreshed = 0u32;
    let mut transient = 0u32;
    let mut permanent = 0u32;
    for a in &accounts {
        let Some(id) = a.get("id").and_then(Value::as_str) else {
            continue;
        };
        match renew_account(state, id) {
            RenewOutcome::Refreshed => refreshed += 1,
            RenewOutcome::Fresh => {}
            RenewOutcome::Transient => transient += 1,
            RenewOutcome::Permanent(reason) => {
                permanent += 1;
                mark_needs_relogin(state, id, &reason);
            }
        }
    }
    if transient > 0 {
        return Err(format!(
            "CodeArts 凭证刷新：{refreshed} 成功 / {transient} 暂态失败 / {permanent} 需重登（共 {} 账号，30 分钟后重试）",
            accounts.len()
        ));
    }
    Ok(json!({
        "ok": true,
        "accounts": accounts.len(),
        "refreshed": refreshed,
        "failed": 0,
        "needs_relogin": permanent,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 惰性窗口必须大于调度间隔（6h），保证兜底 tick 必命中
    #[test]
    fn lazy_window_covers_schedule_interval() {
        assert!(LAZY_HOURS > 6, "LAZY_HOURS={LAZY_HOURS} 未覆盖 6h 调度间隔");
    }
}
