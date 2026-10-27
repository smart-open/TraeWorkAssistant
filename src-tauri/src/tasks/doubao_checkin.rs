//! 豆包每日签到（需求③）：detail 查询签到状态 → 未签账号提交 landing/checkin →
//! 按日落库 doubao_checkin_results + 账号池回写（quota_total/签到缓存）+ 运维历史。
//! 端点（2026-10 抓包固化）：
//! - POST /alice/ug/campaign/v3/detail/      body {"campaign_group_code":"pc_incentive_task_mvp_v3"}
//!   → data.progress.total_amount（累计获得额度 = 「总额度」口径）、
//!     data.tasks[] task_kind=="daily_signin" 的 daily_checkin（今日已签/连续天数/上次日期）
//! - POST /alice/ug/campaign/landing/checkin body {"scene":"pc_incentive_task_mvp_v3"}
//!   → data.checked_in / reward_amount / continuous_days / idempotent
//! msToken/a_bogus 为一次性反爬 token（CLI 无法生成），实测仅稳定 query 参数 + Cookie 即可通过。
//! 红线：凭证等同密码，仅本地使用；仅执行官方签到动作，不做代刷。

use serde_json::{json, Value};
use std::time::Duration;

use crate::fs_utils;
use crate::state::AppState;

use super::http_agent;

const TIMEOUT_SECS: u64 = 15;
const HISTORY_MAX: usize = 400;
/// 签到活动 scene / campaign_group_code（抓包固化）
const SCENE: &str = "pc_incentive_task_mvp_v3";
/// detail 端点（含稳定 query 参数；msToken/a_bogus 省略）
const DETAIL_URL: &str = "https://www.doubao.com/alice/ug/campaign/v3/detail/?version_code=20800&language=zh&device_platform=web&aid=497858&real_aid=497858&pc_version=2.33.5&samantha_web=1&web_platform=desktop&use-olympus-account=1";
/// 签到执行端点（同上）
const CHECKIN_URL: &str = "https://www.doubao.com/alice/ug/campaign/landing/checkin?version_code=20800&language=zh&device_platform=web&aid=497858&real_aid=497858&pc_version=2.33.5&samantha_web=1&web_platform=desktop&use-olympus-account=1";
const REFERER: &str = "https://www.doubao.com/pc/agent-explore";
/// 抓包 UA：桌面客户端自带 SamanthaDoubao 标识（区别于纯浏览器请求）
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 SamanthaDoubao/2.33.5";
/// 批量模式下账号间隔（防上游频控，与 wb/trae 的 gap 思路一致，固定短间隔）
const ACCOUNT_GAP_MS: u64 = 800;

/// 构造 Cookie（对齐 doubao_quota::query_account：sessionid + sid_guard）
fn cookie_of(sid: &str, sid_guard: Option<&str>) -> String {
    let mut cookie = format!("sessionid={sid}");
    if let Some(sg) = sid_guard.filter(|s| !s.is_empty()) {
        cookie.push_str(&format!("; sid_guard={sg}"));
    }
    cookie
}

/// POST JSON 通用（agw 信封）：code==0 视为成功返回 body；否则 Err（含登录失效提示）
fn post_json(agent: &ureq::Agent, url: &str, sid: &str, sg: Option<&str>, body: &Value) -> Result<Value, String> {
    let resp = agent
        .post(url)
        .set("Cookie", &cookie_of(sid, sg))
        .set("User-Agent", UA)
        .set("Referer", REFERER)
        .set("Origin", "https://www.doubao.com")
        .set("Accept", "application/json, text/plain, */*")
        .set("Content-Type", "application/json")
        .set("agw-js-conv", "str")
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .send_string(&body.to_string());
    let raw: Value = match resp {
        Ok(r) => {
            let text = r.into_string().unwrap_or_default();
            serde_json::from_str(&text).map_err(|e| format!("响应解析失败: {e}"))?
        }
        Err(ureq::Error::Status(code, _)) => return Err(format!("HTTP {code}")),
        Err(e) => return Err(format!("请求失败: {e}")),
    };
    if !raw.is_object() {
        return Err("接口返回异常 code=?".to_string());
    }
    let code_v = raw.get("code");
    let ok_code = match code_v {
        None | Some(Value::Null) => true,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(_) => false,
    };
    if !ok_code {
        let code_str = code_v
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "?".to_string());
        // 常见码提示：710012001 = 登录态失效（凭证过期/不属于当前登录账号）
        let hint = if code_v.and_then(Value::as_i64) == Some(710012001) {
            "登录态已失效，请开启代理重新抓取凭证或重新保存该账号登录态"
        } else {
            ""
        };
        return Err(if hint.is_empty() {
            format!("接口返回异常 code={code_str}")
        } else {
            format!("接口返回异常 code={code_str}：{hint}")
        });
    }
    Ok(raw)
}

/// 解析 detail 响应 → 签到状态 + 总额度（progress.total_amount）。
/// tasks[] 无 daily_signin 项时按未签处理（checked_in_today=false）。
pub fn parse_detail(body: &Value) -> Value {
    let data = body.get("data");
    let total_amount = data
        .and_then(|d| d.get("progress"))
        .and_then(|p| p.get("total_amount"))
        .and_then(Value::as_i64);
    let mut checked_in_today = false;
    let mut continuous_days: Option<i64> = None;
    let mut last_checkin_date: Option<String> = None;
    let mut reward_amount: Option<i64> = None;
    if let Some(tasks) = data.and_then(|d| d.get("tasks")).and_then(Value::as_array) {
        for t in tasks {
            if t.get("task_kind").and_then(Value::as_str) != Some("daily_signin") {
                continue;
            }
            if let Some(dc) = t.get("daily_checkin").filter(|d| d.is_object()) {
                checked_in_today = dc
                    .get("checked_in_today")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                continuous_days = dc.get("continuous_days").and_then(Value::as_i64);
                last_checkin_date = dc
                    .get("last_checkin_date")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                reward_amount = dc
                    .get("reward")
                    .and_then(|r| r.get("reward_amount"))
                    .and_then(Value::as_i64);
            }
            break;
        }
    }
    json!({
        "checked_in_today": checked_in_today,
        "continuous_days": continuous_days,
        "last_checkin_date": last_checkin_date,
        "reward_amount": reward_amount,
        "total_amount": total_amount,
    })
}

/// 单账号一轮签到（detail → 判定 → 未签则提交 checkin）。
/// 返回 outcome：status ∈ success | already | fail。
pub fn run_one(
    agent: &ureq::Agent,
    uid: &str,
    sid: &str,
    sg: Option<&str>,
    name: &str,
) -> Value {
    // ① detail：签到状态 + 总额度
    let detail = match post_json(agent, DETAIL_URL, sid, sg, &json!({"campaign_group_code": SCENE})) {
        Ok(body) => parse_detail(&body),
        Err(e) => {
            return json!({
                "uid": uid, "name": name, "status": "fail", "reward": Value::Null,
                "continuous_days": Value::Null, "total_amount": Value::Null,
                "last_checkin_date": Value::Null, "message": e,
            })
        }
    };
    let checked_in_today = detail
        .get("checked_in_today")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let total_amount = detail.get("total_amount").cloned().unwrap_or(Value::Null);

    // ② 已签 → already（回写 detail 携带的连签/上次日期即可）
    if checked_in_today {
        return json!({
            "uid": uid, "name": name, "status": "already", "reward": Value::Null,
            "continuous_days": detail.get("continuous_days").cloned().unwrap_or(Value::Null),
            "total_amount": total_amount,
            "last_checkin_date": detail.get("last_checkin_date").cloned().unwrap_or(Value::Null),
            "message": "今日已签到",
        });
    }

    // ③ 未签 → 提交 landing/checkin
    match post_json(agent, CHECKIN_URL, sid, sg, &json!({"scene": SCENE})) {
        Ok(body) => {
            let data = body.get("data");
            let checked = data
                .and_then(|d| d.get("checked_in"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let idempotent = data
                .and_then(|d| d.get("idempotent"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let reward = data
                .and_then(|d| d.get("reward_amount"))
                .and_then(Value::as_i64);
            let continuous = data
                .and_then(|d| d.get("continuous_days"))
                .and_then(Value::as_i64);
            let date = data
                .and_then(|d| d.get("checkin_date"))
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            if idempotent || (!checked && date.is_some()) {
                // 幂等命中：当日实际已签（detail 未反映或并发窗口）
                return json!({
                    "uid": uid, "name": name, "status": "already", "reward": Value::Null,
                    "continuous_days": continuous.map(Value::from)
                        .or_else(|| detail.get("continuous_days").cloned()),
                    "total_amount": total_amount,
                    "last_checkin_date": date.map(Value::from)
                        .or_else(|| detail.get("last_checkin_date").cloned()),
                    "message": "今日已签到（幂等）",
                });
            }
            if checked {
                return json!({
                    "uid": uid, "name": name, "status": "success",
                    "reward": reward.map(Value::from).unwrap_or(Value::Null),
                    "continuous_days": continuous.map(Value::from)
                        .or_else(|| detail.get("continuous_days").cloned()),
                    "total_amount": total_amount,
                    "last_checkin_date": date.map(Value::from)
                        .or_else(|| detail.get("last_checkin_date").cloned()),
                    "message": format!("签到成功 +{} 额度", reward.unwrap_or(0)),
                });
            }
            json!({
                "uid": uid, "name": name, "status": "fail", "reward": Value::Null,
                "continuous_days": detail.get("continuous_days").cloned().unwrap_or(Value::Null),
                "total_amount": total_amount,
                "last_checkin_date": detail.get("last_checkin_date").cloned().unwrap_or(Value::Null),
                "message": "签到未生效（checked_in=false）",
            })
        }
        Err(e) => json!({
            "uid": uid, "name": name, "status": "fail", "reward": Value::Null,
            "continuous_days": detail.get("continuous_days").cloned().unwrap_or(Value::Null),
            "total_amount": total_amount,
            "last_checkin_date": detail.get("last_checkin_date").cloned().unwrap_or(Value::Null),
            "message": e,
        }),
    }
}

/// 追加滚动运维历史（同 doubao_quota::append_history，kind="checkin"；失败落 app_log）
fn append_history(state: &AppState, event: Value) {
    let store = crate::store::db(&state.data_dir);
    let mut events = crate::store::docs::doubao_health_load(&store);
    events.push(event);
    if events.len() > HISTORY_MAX {
        events.drain(0..events.len() - HISTORY_MAX);
    }
    if let Err(e) = crate::store::docs::doubao_health_save(&store, &events) {
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("[doubao_checkin] 运维历史写入失败（本次签到事件可能丢失）: {e}"),
        );
    }
}

/// 池回写合并（纯函数，两阶段短临界区调用）：将签到结果按 uid 合并进最新池——
/// 只更新仍存在账号的 quota_total / checkin_continuous_days / checkin_last_date /
/// checkin_last_run_at 四字段；fail 时仅回写 checkin_last_run_at（签到字段保留旧值）。
/// 被删账号不复活（返回 0 即无需写盘）、新入池账号不受影响、并发方更新的其他字段不覆盖。
fn merge_checkin_updates_into_pool(
    latest: &mut Value,
    updates: &[(String, String, Option<i64>, Option<i64>, Option<String>, String)],
) -> usize {
    // (uid, status, quota_total, continuous_days, last_date, run_at)
    let mut merged = 0usize;
    if let Some(arr) = latest.get_mut("accounts").and_then(Value::as_array_mut) {
        for (uid, status, quota_total, continuous, last_date, run_at) in updates {
            let Some(acc) = arr
                .iter_mut()
                .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid.as_str()))
            else {
                continue; // 签到期间被删除的账号不复活
            };
            let Some(obj) = acc.as_object_mut() else { continue };
            obj.insert("checkin_last_run_at".into(), Value::String(run_at.clone()));
            if status == "fail" {
                merged += 1;
                continue;
            }
            if let Some(t) = quota_total {
                obj.insert("quota_total".into(), Value::Number((*t).into()));
            }
            if let Some(d) = continuous {
                obj.insert("checkin_continuous_days".into(), Value::Number((*d).into()));
            }
            if let Some(dt) = last_date {
                obj.insert("checkin_last_date".into(), Value::String(dt.clone()));
            }
            merged += 1;
        }
    }
    merged
}

/// 收集池内可签到目标（uid + 凭证 + name），避免边遍历边写
fn collect_targets(state: &AppState) -> Result<Vec<(String, String, Option<String>, String)>, String> {
    let pool: Value =
        serde_json::to_value(crate::commands::doubao::load_pool(state)).unwrap_or(json!({}));
    let accounts = pool
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or("账号池解析失败")?;
    Ok(accounts
        .iter()
        .filter_map(|acc| {
            let uid = acc.get("user_id").and_then(Value::as_str)?.to_string();
            let sid = acc.get("session_id").and_then(Value::as_str)?.to_string();
            if uid.is_empty() || sid.is_empty() {
                return None;
            }
            let sg = acc
                .get("sid_guard")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            let name = acc
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((uid, sid, sg, name))
        })
        .collect())
}

/// 落库 + 池回写 + 运维历史（batch/single 共用收尾）
fn persist_outcomes(state: &AppState, outcomes: &[Value], source: &str) {
    // ① 按日落库（success/already/fail 全记；reward 仅 success 携带）
    let entries = outcomes.iter().map(|o| {
        (
            o.get("uid").and_then(Value::as_str).unwrap_or("").to_string(),
            o.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            o.get("status").and_then(Value::as_str).unwrap_or("fail").to_string(),
            o.get("reward").and_then(Value::as_i64),
        )
    });
    crate::doubao_checkin_results::record_today(&state.data_dir, entries);

    // ② 池回写（两阶段短临界区：重读最新池按 uid 合并）
    let run_at = fs_utils::now_ts();
    let updates: Vec<(String, String, Option<i64>, Option<i64>, Option<String>, String)> = outcomes
        .iter()
        .map(|o| {
            (
                o.get("uid").and_then(Value::as_str).unwrap_or("").to_string(),
                o.get("status").and_then(Value::as_str).unwrap_or("fail").to_string(),
                o.get("total_amount").and_then(Value::as_i64),
                o.get("continuous_days").and_then(Value::as_i64),
                o.get("last_checkin_date")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string()),
                run_at.clone(),
            )
        })
        .collect();
    if !updates.is_empty() {
        let _guard = state.doubao_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut latest: Value =
            serde_json::to_value(crate::commands::doubao::load_pool(state)).unwrap_or(json!({}));
        let merged = merge_checkin_updates_into_pool(&mut latest, &updates);
        if merged > 0 {
            if let Ok(file) =
                serde_json::from_value::<crate::commands::doubao::DoubaoAccountPool>(latest)
            {
                let _ = crate::commands::doubao::save_pool(state, &file);
            }
        }
    }

    // ③ 运维历史（逐账号，kind="checkin"）
    for o in outcomes {
        let ok = o.get("status").and_then(Value::as_str) != Some("fail");
        append_history(
            state,
            json!({
                "ts": fs_utils::now_ts(), "kind": "checkin", "uid": o.get("uid"), "ok": ok,
                "status": o.get("status"), "reward": o.get("reward"),
                "summary": o.get("message"), "source": source,
            }),
        );
    }
}

/// 批量签到（调度器/CLI `--task-run doubao-checkin`）：遍历池内有凭证账号 → 签到 →
/// 按日落库 + 池回写 + 运维历史 → 汇总。空池跳过（对齐 doubao-quota 惯例）。
pub fn run_batch(state: &AppState) -> Result<Value, String> {
    let targets = collect_targets(state)?;
    if targets.is_empty() {
        return Ok(json!({ "ok": true, "skipped": "无豆包账号" }));
    }

    let agent = http_agent(TIMEOUT_SECS);
    let mut outcomes: Vec<Value> = Vec::new();
    for (i, (uid, sid, sg, name)) in targets.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(ACCOUNT_GAP_MS));
        }
        outcomes.push(run_one(&agent, uid, sid, sg.as_deref(), name));
    }
    persist_outcomes(state, &outcomes, "task");

    let success = outcomes
        .iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("success"))
        .count();
    let already = outcomes
        .iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("already"))
        .count();
    let failed = outcomes.len() - success - already;
    let reward_total: i64 = outcomes
        .iter()
        .filter_map(|o| o.get("reward").and_then(Value::as_i64))
        .sum();
    let errors: Vec<Value> = outcomes
        .iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("fail"))
        .map(|o| {
            json!({
                "user_id": o.get("uid"),
                "name": o.get("name"),
                "error": o.get("message"),
            })
        })
        .collect();

    Ok(json!({
        "ok": true, "mode": "all",
        "total": outcomes.len(), "success": success, "already": already, "failed": failed,
        "reward_total": reward_total,
        "errors": errors,
        "finished_at": fs_utils::now_ts(),
    }))
}

/// 单账号签到（应用内手动路径）：读池凭证 → run_one → 落库/回写/历史 → 契约返回
pub fn checkin_single(state: &AppState, uid: &str) -> Result<Value, String> {
    let targets = collect_targets(state)?;
    let Some((_, sid, sg, name)) = targets.iter().find(|(u, _, _, _)| u == uid) else {
        return Err(format!("账号 {uid} 不在账号池中或未录入 sessionid 凭证"));
    };
    let agent = http_agent(TIMEOUT_SECS);
    let outcome = run_one(&agent, uid, sid, sg.as_deref(), name);
    persist_outcomes(state, std::slice::from_ref(&outcome), "manual");
    let ok = outcome.get("status").and_then(Value::as_str) != Some("fail");
    Ok(json!({
        "ok": ok,
        "outcome": outcome,
        "finished_at": fs_utils::now_ts(),
    }))
}

#[cfg(test)]
mod doubao_checkin_tests {
    use super::*;

    #[test]
    fn parse_detail_抓包实测结构() {
        // 2026-10-27 抓包 detail 响应关键段
        let body = json!({
            "code": 0,
            "data": {
                "progress": {"total_amount": 1500},
                "tasks": [
                    {
                        "campaign_code": "daily_signin",
                        "task_kind": "daily_signin",
                        "daily_checkin": {
                            "checked_in_today": false,
                            "continuous_days": 5,
                            "last_checkin_date": "2026-10-26",
                            "reward": {"reward_amount": 200, "reward_type": "quota_grant"},
                            "total_days": 5
                        }
                    },
                    {"campaign_code": "browser_tool", "task_kind": "chat_activity"}
                ]
            }
        });
        let p = parse_detail(&body);
        assert_eq!(p["checked_in_today"], false);
        assert_eq!(p["continuous_days"], 5);
        assert_eq!(p["last_checkin_date"], "2026-10-26");
        assert_eq!(p["reward_amount"], 200);
        assert_eq!(p["total_amount"], 1500);
    }

    #[test]
    fn parse_detail_无签到任务按未签处理() {
        let body = json!({"code": 0, "data": {"progress": {"total_amount": 0}, "tasks": []}});
        let p = parse_detail(&body);
        assert_eq!(p["checked_in_today"], false);
        assert_eq!(p["continuous_days"], Value::Null);
        assert_eq!(p["total_amount"], 0);
    }

    /// 回归（两阶段回写）：签到合并只动签到相关字段——窗口内新入池账号保留、
    /// 被删账号不复活、并发方更新的其他字段不被覆盖、fail 仅回写运行时间。
    #[test]
    fn merge_checkin_updates_preserves_pool_changes_during_window() {
        let mut latest = json!({"accounts": [
            {"user_id": "u1", "name": "甲", "session_id": "sid-A", "quota_summary": "旧摘要"},
            {"user_id": "u3", "name": "新入池", "session_id": "sid-C"},
        ]});
        let updates = vec![
            (
                "u1".to_string(), "success".to_string(),
                Some(1700), Some(6), Some("2026-10-27".to_string()),
                "2026-10-27T09:15:00".to_string(),
            ),
            // u2：签到期间被删除 → 不得复活
            (
                "u2".to_string(), "already".to_string(),
                Some(100), Some(1), Some("2026-10-27".to_string()),
                "2026-10-27T09:15:00".to_string(),
            ),
        ];
        let merged = merge_checkin_updates_into_pool(&mut latest, &updates);
        assert_eq!(merged, 1, "被删账号不参与合并");
        let accounts = latest["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2, "被删账号不复活、无新账号混入");
        let a1 = accounts.iter().find(|a| a["user_id"] == "u1").unwrap();
        assert_eq!(a1["quota_total"], 1700);
        assert_eq!(a1["checkin_continuous_days"], 6);
        assert_eq!(a1["checkin_last_date"], "2026-10-27");
        assert_eq!(a1["checkin_last_run_at"], "2026-10-27T09:15:00");
        assert_eq!(a1["quota_summary"], "旧摘要", "并发方更新的其他字段不被覆盖");
    }

    #[test]
    fn merge_checkin_updates_fail_仅回写运行时间() {
        let mut latest = json!({"accounts": [{"user_id": "u1"}]});
        let updates = vec![(
            "u1".to_string(),
            "fail".to_string(),
            Some(1700),
            None,
            None,
            "2026-10-27T09:15:00".to_string(),
        )];
        let merged = merge_checkin_updates_into_pool(&mut latest, &updates);
        assert_eq!(merged, 1);
        let a1 = latest["accounts"].as_array().unwrap().first().unwrap();
        assert_eq!(a1["checkin_last_run_at"], "2026-10-27T09:15:00");
        assert_eq!(a1.get("quota_total"), None, "失败不写总额度");
        assert_eq!(a1.get("checkin_last_date"), None, "失败不写签到日期");
    }
}
