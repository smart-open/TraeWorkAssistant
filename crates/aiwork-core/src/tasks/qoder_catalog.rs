//! Qoder 模型目录每日同步（p3-3 收尾接线）。
//!
//! 网关路由判定依赖 [`qoder_upstream`] 目录（`resolve`/`list`）；静态兜底表是
//! 蓝本快照，倍率/名单会漂移（2026-10-01 实抓实证 GLM-5.3 0.6→0.8 等 7 处）。
//! 本任务每日拉取 `model/list`（真 COSY 签名 GET，凭证走 `ensure_fresh` 全防护）
//! → [`qoder_upstream::adopt_remote`] 原子替换 CN 区缓存。
//! - 空池空转；全部账号无凭证（未登录/需重登）静默跳过不计失败；
//! - 网络/服务端失败返 Err 交调度器 30 分钟冷却重试；
//! - 目录是账号无关的全局数据，无需逐账号拉取：按池序逐可用账号尝试，
//!   任一账号成功即收口；单个账号失败记日志后试下一个，全部失败才 Err（P3）。

use serde_json::{json, Value};

use crate::state::AppState;

use super::{http_agent, qoder_common, qoder_sign, qoder_upstream};

/// model/list 路径（拼在 [`qoder_upstream::QoderRegion::gateway`] 后；
/// GET 空体，COSY 签名覆盖见 qoder_sign——sigpath 自动剥离 /algo 前缀与查询串）
const MODEL_LIST_PATH: &str = "algo/api/v2/model/list";

/// CN 区 model/list 完整 URL（抽纯函数便于单测锚定端点形态）
fn model_list_url() -> String {
    format!("{}{MODEL_LIST_PATH}", qoder_upstream::QoderRegion::Cn.gateway())
}

/// 调度器/CLI 共用入口：CN 区模型目录同步
pub fn run_task(state: &AppState) -> Result<Value, String> {
    let accounts: Vec<serde_json::Value> = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir))
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if accounts.is_empty() {
        return Ok(json!({ "ok": true, "skipped": "无 Qoder 账号" }));
    }
    let agent = http_agent(20);
    let url = model_list_url();
    // P3：单个账号失败不再整体 Err——记日志后试下一个可用账号，全部失败才上抛
    let mut last_err: Option<String> = None;
    for a in &accounts {
        let id = a.get("id").and_then(Value::as_str).unwrap_or_default();
        // 24h 惰性窗口（对齐探针/常规任务语义）：临期先刷再用
        let (creds, _refreshed, _note) = qoder_common::ensure_fresh(state, &agent, id, 24);
        // 无凭证/需重登账号跳过，试下一个（needs_relogin 账号的 token 可能已死）
        if creds.access_token.is_empty() || creds.uid.is_empty() {
            continue;
        }
        let identity = qoder_sign::CosyIdentity {
            user_id: &creds.uid,
            auth_token: &creds.access_token,
            name: &creds.nickname,
            email: "",
            machine_id: &creds.machine_id,
        };
        let headers = match qoder_sign::build_cosy_headers(None, &url, &identity) {
            Ok(h) => h,
            Err(e) => {
                last_err = Some(format!("model/list 签名构造失败: {e}"));
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!(
                        "[qoder] 账号 {id} 目录同步失败，尝试下一账号: {}",
                        last_err.as_deref().unwrap_or("")
                    ),
                );
                continue;
            }
        };
        let resp = headers
            .into_iter()
            .fold(agent.get(&url), |r, (k, v)| r.set(&k, &v))
            .call();
        match resp {
            Ok(resp) if resp.status() == 200 => {
                // Q2：响应读取失败 / 非 JSON / 目录采纳失败与签名/HTTP 非 200/网络
                // 失败同款容错——记 last_err 后落循环尾部日志试下一账号，仅在全部
                // 可用账号尝试完毕仍无成功时上抛最后错误（不再因单账号解析失败
                // 提前 Err，与函数头「单个账号失败记日志后试下一个」声明一致）
                match resp
                    .into_string()
                    .map_err(|e| format!("model/list 响应读取失败: {e}"))
                    .and_then(|body| {
                        serde_json::from_str::<Value>(&body)
                            .map_err(|e| format!("model/list 响应非 JSON: {e}"))
                    })
                    .and_then(|payload| {
                        qoder_upstream::adopt_remote(qoder_upstream::QoderRegion::Cn, &payload)
                            .map_err(|e| format!("model/list 目录采纳失败: {e}"))
                    })
                {
                    Ok(count) => return Ok(json!({ "ok": true, "models": count, "account": id })),
                    Err(e) => last_err = Some(e),
                }
            }
            Ok(resp) => {
                last_err = Some(format!("model/list HTTP {}", resp.status()));
            }
            Err(e) => {
                last_err = Some(format!("model/list 网络失败: {e}"));
            }
        }
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("[qoder] 账号 {id} 目录同步失败，尝试下一账号: {}", last_err.as_deref().unwrap_or("")),
        );
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(json!({ "ok": true, "skipped": "无可用 Qoder 凭证" })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_state(tag: &str) -> AppState {
        let dir = std::env::temp_dir()
            .join(format!("aiwork_qoder_catalog_test_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("data"));
        AppState {
            data_dir: dir,
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            events: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
    #[test]
    fn url_anchored_to_cn_gateway() {
        assert_eq!(
            model_list_url(),
            "https://gateway.qoder.com.cn/algo/api/v2/model/list"
        );
    }

    #[test]
    fn empty_pool_skips() {
        let st = temp_state("empty");
        let out = run_task(&st).expect("空池应 Ok");
        assert_eq!(out["ok"], true);
        assert!(out["skipped"].as_str().unwrap_or_default().contains("无 Qoder 账号"));
    }

    #[test]
    fn accounts_without_creds_skip_silently() {
        let st = temp_state("nocred");
        // 池里放一个账号（qoder_accounts 表），但 token store 为空
        // → ensure_fresh 返回 no_credential → 跳过
        crate::store::docs::qoder_pool_save(
            &crate::store::db(&st.data_dir),
            &json!({ "accounts": [ { "id": "qd-testnocred", "nickname": "t" } ] }),
        )
        .expect("写池失败");
        let out = run_task(&st).expect("无凭证应 Ok 跳过");
        assert_eq!(out["ok"], true);
        assert!(out["skipped"].as_str().unwrap_or_default().contains("无可用 Qoder 凭证"));
    }
}
