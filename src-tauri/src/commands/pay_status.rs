//! 账号级套餐身份（来自 ide_user_pay_status API，纯账号级逻辑，与本地应用无关）
//!
//! 套餐身份与 storage.json 里 `entitlementInfo.identityStr` 同源，用于账号列表
//! 的会员徽标展示（Free / Lite / Pro ...）。结果缓存到 `pay_status.json`。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::State;

use crate::fs_utils;
use crate::state::AppState;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PayStatusEntry {
    /// 套餐名（Free / Lite / Pro ...）
    pub identity_str: String,
    /// 套餐数值
    pub identity: i64,
    /// 是否新客（未付费过）
    pub is_pay_freshman: bool,
    /// 是否积分计费
    pub is_credits_billing: bool,
    /// 查询时间（Unix 秒）
    pub fetched_at: i64,
}

/// pay_status.json 结构：{ statuses: {uid: entry}, updated_at }
#[derive(Serialize, Deserialize, Default)]
pub struct PayStatusFile {
    #[serde(default)]
    pub statuses: HashMap<String, PayStatusEntry>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

fn query_pay_status(jwt: &str) -> Result<PayStatusEntry, String> {
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    let resp = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .post("https://api.trae.cn/trae/api/v2/pay/ide_user_pay_status")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({"req_source": 2}))
        .map_err(|e| format!("API 请求失败: {}", e))?;
    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;
    // 严格化：仅接受响应中明确携带的套餐字段。鉴权失效 / 限流等错误响应
    // 不含 user_pay_identity 字段，若兜底为 "Free" 会把有效缓存覆盖成错误值。
    let Some(identity_str) = body
        .get("user_pay_identity_str")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    else {
        return Err(format!(
            "响应缺少 user_pay_identity_str（code={:?} msg={:?}）",
            body.get("code").and_then(|v| v.as_i64()),
            body.get("msg").or_else(|| body.get("message")).and_then(|v| v.as_str())
        ));
    };
    Ok(PayStatusEntry {
        identity_str,
        identity: body
            .get("user_pay_identity")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        is_pay_freshman: body
            .get("is_pay_freshman")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        is_credits_billing: body
            .get("is_credits_billing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        fetched_at: chrono::Utc::now().timestamp(),
    })
}

/// 刷新所有账号的套餐身份（批量调用 ide_user_pay_status，写入 pay_status.json 缓存）。
/// 返回成功数量。网络请求命令，标记 async 交由异步线程池派发，避免阻塞主线程。
#[tauri::command(async)]
pub fn refresh_pay_status(state: State<AppState>) -> Result<usize, String> {
    let accounts = crate::vault::load_accounts(&state);
    let mut file: PayStatusFile = fs_utils::read_json(&state.path("pay_status.json"));
    let mut ok = 0usize;
    for a in &accounts.accounts {
        // 无 JWT 的占位账号跳过
        if a.jwt.trim().is_empty() {
            continue;
        }
        let Some(uid) = a.user_id.clone() else { continue };
        match query_pay_status(&a.jwt) {
            Ok(entry) => {
                file.statuses.insert(uid, entry);
                ok += 1;
            }
            Err(e) => {
                fs_utils::app_log(
                    &state.data_dir,
                    &format!("查询套餐失败 [{}]: {}", a.name, e),
                );
            }
        }
    }
    file.updated_at = Some(fs_utils::now_iso());
    fs_utils::write_json(&state.path("pay_status.json"), &file)?;
    Ok(ok)
}
