//! CodeArts（华为云码道，国内站）积分引擎：余额查询 + 每日历史快照。
//!
//! 取数端点：`GET {SNAP_HOST}/snap-manager/v1/statistics/plugin`
//! （SDK-HMAC-SHA256 签名 + 小写 `agent-type: PromptCenter` 参与签名）。
//! 响应为裸对象（非 {code,data} 信封）：metrics[] 数组里按候选键取
//! usageTotalPackageCredit（总额）/ usageBasicPackageCredit（基础包 500）/
//! usageBonusPackageCredit（赠送包）；已用从 usage*Used* 推导；到期取 end_date。
//!
//! 缓存：kv `codearts_credits_cache`（TTL 600s + stale-on-error，fresh=true 跳过）。
//! 每日历史：codearts_credits_history（pk=date 同日覆盖）+ 回写账号池
//! credits_balance / credits_fetched_at / credits_expire_at。
//!
//! 红线：全程零 AK/SK 输出（仅从 vault 读，message 不含密钥）。

use serde_json::{json, Value};

use crate::fs_utils;
use crate::state::AppState;

use super::codearts_common::{self, TRANSIENT_ERR_TAG};
use super::codearts_sign::{self, STATS_PLUGIN_PATH, SNAP_HOST};
use super::http_agent;

/// 缓存 TTL（秒）
const CACHE_TTL_SECS: i64 = 600;

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

fn num_of(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 从 metrics[] 中按多个候选键取第一个有数值的项（字段名可能随版本变化）。
/// 返回 (数值, 命中的原始 Value) —— 命中原始项便于后续从同项取 used/end_date。
fn metric_of<'a>(metrics: &'a [Value], keys: &[&str]) -> Option<(f64, &'a Value)> {
    for m in metrics {
        for k in keys {
            if let Some(v) = m.get(*k) {
                if let Some(n) = num_of(Some(v)) {
                    return Some((n, m));
                }
            }
        }
    }
    None
}

/// 解析积分统计响应为余额行数据。
/// 返回 (总额, 已用, 剩余, 到期 Unix 秒, message)。
/// 剩余优先直接字段 usageTotalPackageCreditRemain / remain / remaining；
/// 否则 total - used（钳 0）。end_date 取 body 顶层/data 层（宽容候选）。
fn parse_stats(body: &Value) -> (Option<f64>, Option<f64>, Option<f64>, Option<i64>, String) {
    // metrics[] 宽容提取：裸数组 / data.metrics / metrics / list / data 为数组。
    // 同时展平 {key,value} 形态的 KV 指标项为宽字段，供 metric_of 统一取值。
    let root = fs_utils::dig(body, &["data"]).unwrap_or(body);
    let raw_metrics: Vec<Value> = if let Some(arr) = body.as_array() {
        arr.clone()
    } else {
        let mut found = Vec::new();
        for key in ["metrics", "list", "items", "records"] {
            if let Some(arr) = root.get(key).and_then(Value::as_array) {
                found = arr.clone();
                break;
            }
        }
        if found.is_empty() {
            root.as_array().cloned().unwrap_or_default()
        } else {
            found
        }
    };
    // 真实响应是 {name, value, package_credit_*} 形态：保留原始项供按 name 取 package_credit_*，
    // 同时追加展平项兼容旧 {key:value} 形态。
    let mut metrics = raw_metrics.clone();
    metrics.extend(flatten_metric_entries(&raw_metrics));
    let (total, used, remain, msg) = finish_parse(&metrics, &raw_metrics);

    // end_date：优先 body 顶层 / data 层，其次任一 metric 内
    let end_date = ["end_date", "endDate", "expireAt", "expire_at"]
        .iter()
        .find_map(|k| fs_utils::dig(body, &[*k]).and_then(parse_end_date))
        .or_else(|| {
            ["end_date", "endDate", "expireAt", "expire_at"]
                .iter()
                .find_map(|k| fs_utils::dig(root, &[*k]).and_then(parse_end_date))
        })
        .or_else(|| {
            metrics.iter().find_map(|m| {
                ["end_date", "endDate", "expireAt", "expire_at"]
                    .iter()
                    .find_map(|k| m.get(*k).and_then(parse_end_date))
            })
        });
    (total, used, remain, end_date, msg)
}

/// {key:"usageTotalPackageCredit", value:500} → {usageTotalPackageCredit:500}：
/// 把 KV 形态的指标项展平为宽字段形态，供 metric_of 统一取值。
fn flatten_metric_entries(arr: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for item in arr {
        let k = s_of(fs_utils::dig(item, &["key", "name", "metric", "metricKey"]));
        if k.is_empty() {
            continue;
        }
        let v = fs_utils::dig(item, &["value", "amount", "total", "num"])
            .cloned()
            .unwrap_or(Value::Null);
        let mut flat = json!({ k: v });
        if let Some(ed) = fs_utils::dig(item, &["end_date", "endDate"]) {
            flat["end_date"] = ed.clone();
        }
        out.push(flat);
    }
    out
}

fn finish_parse(metrics: &[Value], raw_metrics: &[Value]) -> (Option<f64>, Option<f64>, Option<f64>, String) {
    // 真实响应形态（2026-11 实测）：metric 项形如
    // {"name":"usageTotalPackageCredit","value":67,"package_credit_amount":13600,
    //  "package_credit_used":9137.06,"package_credit_remain":4462.94}
    // `value` 只是类型/状态标记，真正的额度在 package_credit_*。
    // 因此按 name 找到原始项，优先取 package_credit_*；缺失才回退展平项的 value。
    let named_item = |names: &[&str]| -> Option<&Value> {
        raw_metrics.iter().find(|m| {
            m.get("name").and_then(Value::as_str).is_some_and(|n| names.contains(&n))
        })
    };
    let total_item = named_item(&["usageTotalPackageCredit", "totalCredit", "total_credit", "total"]);
    let pkg_amount = total_item.and_then(|m| num_of(m.get("package_credit_amount")));
    let pkg_used = total_item.and_then(|m| num_of(m.get("package_credit_used")));
    let pkg_remain = total_item.and_then(|m| num_of(m.get("package_credit_remain")));
    let total = metric_of(
        metrics,
        &["usageTotalPackageCredit", "totalCredit", "total_credit", "total"],
    );
    let basic = metric_of(metrics, &["usageBasicPackageCredit", "basicCredit", "basic_credit"]);
    let bonus = metric_of(metrics, &["usageBonusPackageCredit", "bonusCredit", "bonus_credit"]);
    // 总额：package_credit_amount 优先；否则旧的 value / basic+bonus 兜底
    let total_value = pkg_amount
        .or_else(|| total.as_ref().map(|(n, _)| *n))
        .or_else(|| {
            match (basic.as_ref().map(|(n, _)| *n), bonus.as_ref().map(|(n, _)| *n)) {
                (Some(b), Some(o)) => Some(b + o),
                (Some(b), None) | (None, Some(b)) => Some(b),
                _ => None,
            }
        });
    // 已用：package_credit_used 优先，再回退旧候选
    let used = pkg_used
        .or_else(|| total.as_ref().and_then(|(_, m)| {
            num_of(fs_utils::dig(m, &["usageTotalPackageCreditUsed", "used", "usedCredit", "used_credit"]))
        }))
        .or_else(|| metric_of(metrics, &["usageTotalPackageCreditUsed", "usedCredit", "used_credit"]).map(|(n, _)| n));
    // 剩余：package_credit_remain 优先；否则直接字段；否则 total - used
    let remain_direct = pkg_remain.or_else(|| metric_of(
        metrics,
        &[
            "usageTotalPackageCreditRemain",
            "usageTotalPackageCreditRemaining",
            "remainCredit",
            "remainingCredit",
            "remaining_credit",
            "remain",
            "remaining",
        ],
    )
    .map(|(n, _)| n));
    let remain = remain_direct
        .or_else(|| match (total_value, used) {
            (Some(t), Some(u)) => Some((t - u).max(0.0)),
            (Some(t), None) => Some(t.max(0.0)),
            _ => None,
        });
    let msg = if total_value.is_none() && remain.is_none() {
        "积分统计结构未识别（接口可能已变更）".to_string()
    } else {
        String::new()
    };
    (total_value, used, remain, msg)
}

fn parse_end_date(v: &Value) -> Option<i64> {
    if let Some(n) = num_of(Some(v)) {
        let n = n as i64;
        if n > 0 {
            // 毫秒阈值：1e12 约为 2001-09-09，小于它的正整数按秒
            return Some(if n > 100_000_000_000 { n / 1000 } else { n });
        }
        return None;
    }
    let s = s_of(Some(v));
    chrono::DateTime::parse_from_rfc3339(&s)
        .ok()
        .map(|d| d.timestamp())
}

/// 单账号积分查询：返回行 Value（含 user_id/name/ok/total/message）。
fn fetch_account(
    agent: &ureq::Agent,
    acct: &Value,
    creds: &codearts_common::CodeartsCreds,
) -> Value {
    let uid = s_of(acct.get("id"));
    let name = {
        let n = s_of(fs_utils::dig(acct, &["name", "nickname"]));
        if n.is_empty() { uid.clone() } else { n }
    };
    let mut row = json!({
        "user_id": uid,
        "name": name,
        "ok": false,
        "total": Value::Null,
        "message": "",
    });
    if !creds.is_valid() {
        row["message"] = json!("未配置有效的 AK/SK（请在账号管理中重新录入）");
        row["source"] = json!("none");
        return row;
    }
    let identity = crate::tasks::codearts_sign::identity_from_creds(&creds);
    let url = format!("https://{SNAP_HOST}{STATS_PLUGIN_PATH}");
    let extra: [(&str, &str); 1] = [("agent-type", "PromptCenter")];
    let signed = match codearts_sign::sign(&identity, "GET", &url, b"", &extra) {
        Ok(s) => s,
        Err(_) => {
            row["message"] = json!("请求签名失败（请检查 AK/SK）");
            return row;
        }
    };
    let mut req = agent.get(&url);
    req = req
        .set("Authorization", &signed.authorization)
        .set("x-sdk-date", &signed.x_sdk_date)
        .set("content-type", &signed.content_type);
    if let Some(st) = &signed.x_security_token {
        req = req.set("x-security-token", st.as_str());
    }
    if let Some(sha) = &signed.x_sdk_content_sha256 {
        req = req.set("x-sdk-content-sha256", sha.as_str());
    }
    for (k, v) in &signed.extra {
        req = req.set(k.as_str(), v.as_str());
    }
    let (status, body) = match req.call() {
        Ok(resp) => {
            let code = resp.status();
            let raw = resp.into_string().unwrap_or_default();
            (code, serde_json::from_str::<Value>(&raw).ok())
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp.into_string().unwrap_or_default();
            (code, serde_json::from_str::<Value>(&raw).ok())
        }
        Err(_) => (0, None),
    };
    if status == 401 || status == 403 {
        row["http_status"] = json!(status);
        row["message"] = json!("AK/SK 鉴权失败（HTTP 401/403，请检查密钥）");
        return row;
    }
    if status == 0 {
        row["message"] = json!(format!("{TRANSIENT_ERR_TAG} 网络不可达（积分统计）"));
        return row;
    }
    if status != 200 {
        row["http_status"] = json!(status);
        row["message"] = json!(format!("积分统计不可用（HTTP {status}）"));
        return row;
    }
    let Some(b) = body else {
        row["message"] = json!("积分统计响应非 JSON");
        return row;
    };
    let (total, used, remain, end_date, parse_msg) = parse_stats(&b);
    if total.is_none() && remain.is_none() {
        row["message"] = json!(parse_msg);
        return row;
    }
    row["ok"] = json!(true);
    row["total"] = json!(remain.or(total));
    row["total_credit"] = json!(total);
    row["used"] = json!(used);
    row["earned"] = json!(num_of(b.get("earned")));
    row["consumed"] = json!(num_of(b.get("consumed")));
    row["credits_expire_at"] = json!(end_date);
    row["message"] = json!("ok");
    row
}

/// 缓存是否有效（TTL 内、非空 accounts）。
fn cache_valid(cache: &Value, now_ms: i64) -> bool {
    let fetched = cache.get("fetched_at_ms").and_then(Value::as_i64);
    cache.get("accounts").and_then(Value::as_array).is_some_and(|a| !a.is_empty())
        && fetched.is_some_and(|t| now_ms - t < CACHE_TTL_SECS * 1000)
}

/// 单用户过滤缓存（accounts[].user_id = 池 id）。
fn filter_cache_by_user(mut cache: Value, user_id: &str) -> Value {
    if let Some(arr) = cache.get_mut("accounts").and_then(Value::as_array_mut) {
        arr.retain(|a| a.get("user_id").and_then(Value::as_str) == Some(user_id));
    }
    if let Some(total) = cache
        .get("accounts")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|r| r.get("total").and_then(Value::as_f64)).sum::<f64>())
    {
        cache["total_balance"] = json!(total);
    }
    cache
}

/// 积分查询主入口（commands 与调度共用）。
/// user_id=None 查全部；fresh=true 跳过缓存。全失败回退历史缓存并标 stale。
pub fn fetch_credits(
    state: &AppState,
    user_id: Option<&str>,
    fresh: bool,
) -> Result<Value, String> {
    let db = crate::store::db(&state.data_dir);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let cache: Value = db.kv_get("codearts_credits_cache");
    if !fresh && cache_valid(&cache, now_ms) {
        let mut out = match user_id {
            Some(uid) => filter_cache_by_user(cache.clone(), uid),
            None => cache.clone(),
        };
        if out.get("accounts").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
            out["ok"] = json!(true);
            out["cached"] = json!(true);
            out["stale"] = json!(false);
            out["stale_reason"] = json!("");
            out["message"] = json!("");
            return Ok(out);
        }
    }

    // 跨进程互斥（CLI 快照进程与 GUI UI 拉积分同刻触发时幂等）
    let (_cross, lock_fail) =
        codearts_common::CrossProcLock::try_acquire(&state.data_dir, "credits", 0);
    if _cross.is_none() {
        fs_utils::app_log(
            &state.data_dir,
            &format!("[codearts-credits] 跨进程锁未获取（{lock_fail}），本轮回退缓存"),
        );
        let fallback = match user_id {
            Some(uid) => filter_cache_by_user(cache, uid),
            None => cache,
        };
        if fallback.get("accounts").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
            let mut out = fallback;
            out["ok"] = json!(false);
            out["cached"] = json!(true);
            out["stale"] = json!(true);
            out["stale_reason"] = json!("另一进程正在同步积分，展示历史缓存");
            out["message"] = json!("");
            return Ok(out);
        }
        return Ok(json!({
            "ok": false, "cached": false, "stale": false, "stale_reason": "",
            "accounts": [], "total_balance": 0.0,
            "message": "另一进程正在同步积分，请稍后重试",
        }));
    }

    let agent = http_agent(30);
    let pool = crate::store::docs::codearts_pool_load(&db);
    let mut accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(uid) = user_id {
        accounts.retain(|a| a.get("id").and_then(Value::as_str) == Some(uid));
    }
    if accounts.is_empty() {
        return Ok(json!({
            "ok": false, "cached": false, "stale": false, "stale_reason": "",
            "accounts": [], "total_balance": 0.0, "message": "账号池为空",
        }));
    }
    let rows: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let aid = a.get("id").and_then(Value::as_str).unwrap_or("");
            let creds = codearts_common::ensure_fresh_creds(&state.data_dir, aid)
                .unwrap_or_default();
            fetch_account(&agent, a, &creds)
        })
        .collect();
    let ok_count = rows.iter().filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true)).count();
    let total_balance = rows
        .iter()
        .filter_map(|r| r.get("total").and_then(Value::as_f64))
        .sum::<f64>();

    if ok_count == 0 {
        // stale-on-error：全失败回退缓存（无论是否过期）
        let fallback = match user_id {
            Some(uid) => filter_cache_by_user(cache, uid),
            None => cache,
        };
        if fallback.get("accounts").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
            let mut out = fallback;
            out["ok"] = json!(false);
            out["cached"] = json!(true);
            out["stale"] = json!(true);
            out["stale_reason"] = json!("本轮全部账号刷新失败，展示历史缓存");
            out["message"] = json!("");
            return Ok(out);
        }
        return Ok(json!({
            "ok": false, "cached": false, "stale": false, "stale_reason": "",
            "accounts": rows, "total_balance": 0.0,
            "message": "全部账号查询失败且无历史缓存",
        }));
    }

    // 回写账号池 credits_balance / credits_fetched_at / credits_expire_at
    // 直操原始 JSON 保留未知字段（read→merge→save，单进程内本锁串行）
    let mut pool = crate::store::docs::codearts_pool_load(&db);
    let mut changed = false;
    if let Some(list) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for a in list.iter_mut() {
            let Some(aid) = a.get("id").and_then(Value::as_str).map(str::to_string) else { continue };
            let Some(row) = rows.iter().find(|r| {
                r.get("user_id").and_then(Value::as_str) == Some(aid.as_str())
                    && r.get("ok").and_then(Value::as_bool) == Some(true)
            }) else { continue };
            a["credits_balance"] = row.get("total").cloned().unwrap_or(Value::Null);
            a["credits_fetched_at"] = json!(fs_utils::now_iso());
            a["credits_expire_at"] = row.get("credits_expire_at").cloned().unwrap_or(Value::Null);
            changed = true;
        }
    }
    if changed {
        let _ = crate::store::docs::codearts_pool_save(&db, &pool);
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    // 快照落库（仅全量查询且全部成功，避免单账号/部分失败覆盖当日正确快照）
    if user_id.is_none() && ok_count == rows.len() {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let snap = json!({
            "date": date,
            "ts": now_ms,
            "total": total_balance,
            "earned": rows.iter().filter_map(|r| r.get("earned").and_then(Value::as_f64)).sum::<f64>(),
            "consumed": rows.iter().filter_map(|r| r.get("consumed").and_then(Value::as_f64)).sum::<f64>(),
            "accounts": rows.iter().map(|r| json!({
                "user_id": r.get("user_id").cloned().unwrap_or_default(),
                "total": r.get("total").cloned().unwrap_or(Value::Null),
            })).collect::<Vec<_>>(),
        });
        let _ = crate::store::docs::codearts_credits_history_upsert(&db, &[snap]);
    }
    let out = json!({
        "ok": true, "cached": false, "stale": false, "stale_reason": "",
        "accounts": rows, "total_balance": total_balance,
        "fetched_at_ms": now_ms, "message": "",
    });
    if user_id.is_none() && ok_count == rows.len() {
        let _ = db.kv_set("codearts_credits_cache", &out);
    }
    Ok(out)
}

/// 每日快照任务（调度器/CLI 共用）：fresh 拉全量 + 快照落库 + 池回写。
/// 空池自然空转；暂态失败返 Err 交调度器冷却重试，永久失败返 Ok 停止重试。
pub fn run_snapshot_task(state: &AppState) -> Result<Value, String> {
    let db = crate::store::db(&state.data_dir);
    let pool = crate::store::docs::codearts_pool_load(&db);
    let n = pool.get("accounts").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
    if n == 0 {
        return Ok(json!({ "ok": true, "skipped": "无 CodeArts 账号" }));
    }
    let parsed = fetch_credits(state, None, true)?;
    if parsed.get("cached").and_then(Value::as_bool) == Some(true) {
        return Err("积分快照本轮全部账号拉取失败（已回退历史缓存），未落当日快照，稍后自动重试".into());
    }
    let ok = parsed.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let rows = parsed.get("accounts").and_then(Value::as_array).cloned().unwrap_or_default();
    let failed: Vec<&Value> = rows
        .iter()
        .filter(|r| r.get("ok").and_then(Value::as_bool) != Some(true))
        .collect();
    if failed.is_empty() && ok {
        return Ok(json!({ "ok": true, "accounts": n }));
    }
    // 永久失败：无凭证（source=none）/ 4xx（鉴权失效）；429 为限流暂态
    let is_permanent = |r: &Value| {
        r.get("source").and_then(Value::as_str) == Some("none")
            || r.get("http_status")
                .and_then(Value::as_i64)
                .is_some_and(|s| (400..500).contains(&s) && s != 429)
    };
    let permanent = failed.iter().filter(|r| is_permanent(r)).count();
    let transient = failed.len() - permanent;
    if transient > 0 {
        return Err(format!(
            "积分快照暂态失败 {transient}/{n}（永久 {permanent}），未落当日快照，稍后自动重试"
        ));
    }
    Ok(json!({ "ok": false, "accounts": n, "needs_relogin": permanent }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实响应（2026-11-18 抓包）：usageTotalPackageCredit.value=67 只是类型标记，
    /// 真正额度在 package_credit_amount/used/remain。防止回归把 67 当余额。
    #[test]
    fn parse_stats_uses_package_credit_fields_not_value() {
        let raw: Value = serde_json::from_str(r#"{
            "metrics":[
                {"name":"usageDataChatMessages","value":-1},
                {"name":"usageTotalPackageCredit","value":67,
                 "package_credit_amount":13600.00,
                 "package_credit_used":9137.06,
                 "package_credit_remain":4462.94,"package_credit_expiring_amount":100.0},
                {"name":"usageBasicPackageCredit","value":100,"package_credit_amount":500.00,"package_credit_remain":0},
                {"name":"usageBonusPackageCredit","value":65,"package_credit_amount":13100.0,"package_credit_remain":4462.94}
            ]
        }"#).unwrap();
        let (total, used, remain, _end, msg) = parse_stats(&raw);
        assert!(msg.is_empty());
        assert_eq!(total, Some(13600.0), "总额取 package_credit_amount 而非 value=67");
        assert_eq!(used, Some(9137.06));
        assert_eq!(remain, Some(4462.94), "余额取 package_credit_remain");
    }
}
