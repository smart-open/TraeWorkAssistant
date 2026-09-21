//! WorkBuddy 积分三件套查询（F-20/F-22，原 src-python/workbuddy_credits.py 的 Rust 移植）。
//! 取数三层降级（§3.6）：云端三件套 → 本地 quota 端口兜底（T5.8/F-21）→ 无凭证明示；
//! 解析宽容：容量字段全树收集（6 种嵌套路径兼容，F-20）+ 容量字段链。
//! 输出契约（commands/workbuddy/credits.rs 消费，逐字段对齐 python）：
//!   {"ok":true,"cached":bool,"accounts":[{user_id,name,ok,balance,packages[],source,fetched_at}],"total_balance":N}
//!   packages item：{name,code,remaining,total,used,end_time,expire_ts,expire_soon}
//! 缓存：data/workbuddy_credits_cache.json（≥10 分钟；fresh 强制刷新）；stale-on-error（F-59）。

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::fs_utils;
use crate::state::AppState;

use super::http_agent;
use super::wb_common;

const CACHE_TTL_SECS: f64 = 10.0 * 60.0; // ≥10min 缓存（频控红线，原 5min）

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 宽容数值：bool 排除；数字/数字字符串均可（对齐 python _num）。
fn num_of(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 到期字段宽容归一 → Unix 秒（解析失败 None）。
/// 兼容：ISO 字符串（含 Z / 空格分隔 / 日期分隔符变体）、纯日期（补 23:59:59，
/// 无时区输入假定 UTC+8）、数字或数字字符串时间戳（毫秒 ≥1e12 自动除 1000）。
fn to_ts(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    if v.is_null() || v.is_boolean() {
        return None;
    }
    if let Some(n) = num_of(Some(v)) {
        if n >= 1e12 {
            return Some((n / 1000.0) as i64); // 毫秒
        }
        if n >= 1e9 {
            return Some(n as i64); // 秒
        }
        return None; // 过小（如剩余量）不是时间戳
    }
    let s = v.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    let s = s.replace('Z', "+00:00").replace('/', "-");
    // 纯日期 YYYY-MM-DD → 当天 23:59:59（避免凌晨 0 点误报「已过期」）
    let s = if s.len() == 10 && s.matches('-').count() == 2 {
        format!("{s} 23:59:59")
    } else {
        s
    };
    use chrono::TimeZone;
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(&s) {
        return Some(d.timestamp());
    }
    let cn = chrono::FixedOffset::east_opt(8 * 3600)?;
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(&s, fmt) {
            return cn.from_local_datetime(&d).single().map(|dt| dt.timestamp());
        }
    }
    None
}

/// Unix 秒 → 统一展示形态 "YYYY-MM-DD HH:MM:SS"（UTC+8，与 to_ts 无时区假定一致；
/// 前端明细按 end_time[:10] 截日期）。
fn fmt_ts(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::FixedOffset::east_opt(8 * 3600)
        .and_then(|cn| cn.timestamp_opt(ts, 0).single())
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

// ── 容量解析（宽容链）───────────────────────────────────────────────────────

/// 总量字段链：CycleTotalCapacity（summary 新结构）→ CycleCapacitySize（free/旧接口
/// 周期总量）→ CapacitySize → TotalCapacity（§5.4）。
fn total_of(pkg: &Value) -> Option<f64> {
    for k in [
        "CycleTotalCapacity",
        "CycleCapacitySize",
        "CapacitySize",
        "TotalCapacity",
        "capacity",
    ] {
        if let Some(v) = num_of(fs_utils::dig(pkg, &[k])) {
            return Some(v);
        }
    }
    None
}

/// 剩余字段链：CycleCapacityRemainPrecise（free/旧接口周期剩余，实测 2026-09）优先，
/// CycleRemainCapacity（summary 新结构）次之，回退 RemainingCapacity/remaining；
/// 仅剩 used 时由总量倒推。注意 CycleCapacitySizePrecise 是周期**总量**而非剩余，
/// 不参与剩余链。
fn remaining_of(pkg: &Value, total: Option<f64>) -> Option<f64> {
    for k in [
        "CycleCapacityRemainPrecise",
        "CycleRemainCapacity",
        "RemainingCapacity",
        "remaining",
        "Remaining",
        "LeftCapacity",
    ] {
        if let Some(v) = num_of(fs_utils::dig(pkg, &[k])) {
            return Some(v);
        }
    }
    let used = num_of(fs_utils::dig(pkg, &["UsedCapacity", "used", "Used"]));
    if let (Some(used), Some(total)) = (used, total) {
        return Some((total - used).max(0.0));
    }
    None
}

const CAPACITY_KEYS: &[&str] = &[
    "CycleCapacitySizePrecise",
    "CycleRemainCapacity",
    "CycleCapacityRemainPrecise",
    "CycleCapacityRemain",
    "RemainingCapacity",
    "Remaining",
    "CycleTotalCapacity",
    "CycleCapacitySize",
    "CapacitySize",
    "TotalCapacity",
    "UsedCapacity",
    "CycleUsedCapacity",
];

/// 全树（限深 8）收集含任意容量字段的 dict——天然兼容
/// data.Accounts / data.data.Accounts / data.Response.Data.Accounts 等 6 种嵌套路径（F-20）。
fn walk_capable_dicts<'a>(v: &'a Value, depth: usize, out: &mut Vec<&'a Value>) {
    if depth > 8 {
        return;
    }
    match v {
        Value::Object(map) => {
            if CAPACITY_KEYS.iter().any(|k| map.contains_key(*k)) {
                out.push(v);
            }
            for child in map.values() {
                walk_capable_dicts(child, depth + 1, out);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                walk_capable_dicts(item, depth + 1, out);
            }
        }
        _ => {}
    }
}

/// 从三件套/旧接口响应提取包列表（容量键全树收集，6 种嵌套路径兼容）。
fn packages_from(body: &Value) -> Vec<Value> {
    let mut found = vec![];
    walk_capable_dicts(body, 0, &mut found);
    let mut out = vec![];
    for item in found {
        let total = total_of(item);
        let remaining = remaining_of(item, total);
        if remaining.is_none() && total.is_none() {
            continue;
        }
        let name = fs_utils::dig(
            item,
            &["PackageName", "packageName", "Name", "name", "ProductCode", "description"],
        )
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "积分包".to_string());
        // PackageCode：跨接口去重/合并用（summary 与 paid/free 的 name 字段不同源）
        let code = fs_utils::dig(item, &["PackageCode", "packageCode"])
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .filter(|s| !s.is_empty());
        // 到期字段宽容链（§3.6）→ to_ts 归一 Unix 秒；expire_ts 有值 ⟺ end_time 有值
        //（此前 end_time 仅字符串原始值时回填致明细「到期时间未知」而日历正常，已统一由
        // expire_ts 生成，与到期日历口径严格对齐）
        let end_raw = fs_utils::dig(
            item,
            &[
                "DeductionEndTime",
                "deductionEndTime",
                "PackageEndTime",
                "packageEndTime",
                "EndTime",
                "endTime",
                "expireTime",
                "expireAt",
                "ExpireTime",
                "expire_time",
                "expiredAt",
                "ExpiredTime",
            ],
        );
        let end_ts = to_ts(end_raw);
        let used = num_of(fs_utils::dig(item, &["UsedCapacity", "used", "Used"]));
        out.push(json!({
            "name": name,
            "code": code,
            "remaining": remaining.unwrap_or(0.0),
            "total": total.unwrap_or(0.0),
            "used": used.unwrap_or(0.0),
            "end_time": end_ts.map(fmt_ts),
            "expire_ts": end_ts,
        }));
    }
    out
}

/// summary 响应 → 余额。旧结构：顶层 RemainingCapacity/Balance 等字段；
/// 新结构（2026-09 实测）：data.Packages[] 各包 CycleRemainCapacity 求和。
fn balance_from_summary(body: &Value) -> Option<f64> {
    if let Some(t) = num_of(fs_utils::dig(
        body,
        &["RemainingCapacity", "remaining", "TotalRemaining", "Balance", "balance"],
    )) {
        return Some(t);
    }
    let mut found = vec![];
    walk_capable_dicts(body, 0, &mut found);
    let mut s = 0.0;
    let mut has = false;
    for item in found {
        if let Some(v) = num_of(fs_utils::dig(item, &["CycleRemainCapacity", "RemainingCapacity", "remaining"])) {
            s += v;
            has = true;
        }
    }
    if has {
        return Some(s);
    }
    num_of(fs_utils::dig(body, &["CycleCapacitySizePrecise", "CycleTotalCapacity", "CapacitySize"]))
}

/// summary 响应 → PackageCode 列表（去重，保序）。2026-09 起 paid/free 接口要求
/// 请求体携带 PackageCodes（空 body 返回 400 code=10001），代码列表只能先从 summary 提取。
fn package_codes_from(body: &Value) -> Vec<String> {
    let mut found = vec![];
    walk_capable_dicts(body, 0, &mut found);
    let mut codes: Vec<String> = vec![];
    for item in found {
        if let Some(c) = fs_utils::dig(item, &["PackageCode", "packageCode"]).and_then(Value::as_str) {
            if !c.is_empty() && !codes.iter().any(|x| x == c) {
                codes.push(c.to_string());
            }
        }
    }
    codes
}

/// 区域路由（T4.5/F-36，§5.2）：Global 账号 billing 走 www.workbuddy.ai。
/// 返回 (summary, paid, free, old_resource) 四元组。
fn billing_urls(domain: &str) -> (String, String, String, String) {
    let base = wb_common::region_billing_base(domain);
    (
        format!("{base}/billing/meter/get-user-resource-summary"),
        format!("{base}/billing/meter/get-user-resource-paid-packages"),
        format!("{base}/billing/meter/get-user-resource-free-packages"),
        format!("{base}/v2/billing/meter/get-user-resource"),
    )
}

/// 旧接口参数（F-20 回退路径）
fn old_body() -> Value {
    json!({
        "ProductCode": "p_tcaca",
        "Status": [0, 3],
        "PackageEndTimeRange": {
            "StartTime": "2000-01-01T00:00:00Z",
            "EndTime": "2099-12-31T23:59:59Z"
        }
    })
}

/// 单轮取数 → (pkgs, balance, saw_auth, net_down)。
/// 2026-09 实测结构（代理诊断）：
/// - summary：200，data.Packages[] 自带 PackageCode + 容量三项（**无到期时间字段**）；
/// - paid/free：400 code=10001 "PackageCodes required"——需携带 summary 提取的
///   PackageCodes 回查（到期时间仅这两个接口提供）。
/// 流程：summary（余额 + 包）→ paid/free 带 PackageCodes 回查到期 → 按 PackageCode
/// 去重合并（paid/free 有到期信息的优先，summary 包补齐未被覆盖的）。
/// paid/free 失败不影响 cloud 成功判定（仅缺到期信息，明细显示「未知」）。
fn fetch_round(
    agent: &ureq::Agent,
    headers: &[(String, String)],
    urls: (&str, &str, &str),
) -> (Vec<Value>, Option<f64>, bool, bool) {
    let (summary_url, paid_url, free_url) = urls;
    let (status, body) = wb_common::post_json(agent, summary_url, headers, &json!({}));
    if status == 0 {
        return (vec![], None, false, true); // 网络不可达 → 供双探测判定
    }
    if status == 401 {
        return (vec![], None, true, false);
    }
    if status != 200 {
        return (vec![], None, false, false);
    }
    let Some(body) = body.filter(|b| b.is_object()) else {
        return (vec![], None, false, false);
    };

    let balance = balance_from_summary(&body);
    let codes = package_codes_from(&body);

    // paid/free 带 PackageCodes + 分页回查（到期时间/周期明细数据源）
    let mut enriched: Vec<Value> = vec![];
    let mut enriched_codes: std::collections::HashSet<String> = Default::default();
    let mut saw_auth = false;
    if !codes.is_empty() {
        let req = json!({"PackageCodes": codes, "PageNumber": 1, "PageSize": 100});
        for url in [paid_url, free_url] {
            let (st2, body2) = wb_common::post_json(agent, url, headers, &req);
            if st2 == 401 {
                saw_auth = true;
                continue;
            }
            if st2 == 200 {
                if let Some(b2) = body2.filter(|b| b.is_object()) {
                    for p in packages_from(&b2) {
                        if let Some(code) = p.get("code").and_then(Value::as_str) {
                            enriched_codes.insert(code.to_string());
                        }
                        enriched.push(p);
                    }
                }
            }
        }
    }

    // summary 自带包：仅补 paid/free 未覆盖的 PackageCode（无到期信息）
    for p in packages_from(&body) {
        let dup = p
            .get("code")
            .and_then(Value::as_str)
            .map(|c| enriched_codes.contains(c))
            .unwrap_or(false);
        if dup {
            continue;
        }
        enriched.push(p);
    }
    (enriched, balance, saw_auth, false)
}

/// 一次取数：summary（余额+包）→ paid/free 带 PackageCodes 回查到期 →
/// 全 401 刷新一次仅重试失败分支 → 旧接口回退。
/// 主域名整体网络不可达时切备用域名重试一轮（§2.2 域名双探测，仅一次、不循环）。
/// 返回 (packages, balance, source, new_creds)。
fn fetch_credits_once(
    agent: &ureq::Agent,
    creds: &wb_common::Creds,
) -> (Vec<Value>, Option<f64>, &'static str, Option<wb_common::Creds>) {
    let headers = wb_common::build_auth_headers(creds, true);
    let (summary_url, paid_url, free_url, old_url) = billing_urls(&creds.domain);
    let (mut pkgs, mut balance, mut saw_auth, net_down) =
        fetch_round(agent, &headers, (&summary_url, &paid_url, &free_url));
    if net_down && pkgs.is_empty() && balance.is_none() {
        // 双探测（§2.2）：主域名网络不可达 → 备用域名重试一轮
        let alt = wb_common::billing_bases(&creds.domain)[1];
        let (s2, p2, f2) = (
            format!("{alt}/billing/meter/get-user-resource-summary"),
            format!("{alt}/billing/meter/get-user-resource-paid-packages"),
            format!("{alt}/billing/meter/get-user-resource-free-packages"),
        );
        let r = fetch_round(agent, &headers, (&s2, &p2, &f2));
        pkgs = r.0;
        balance = r.1;
        saw_auth = r.2;
    }
    let mut new_creds = None;
    if saw_auth && pkgs.is_empty() && balance.is_none() {
        if let Some(nc) = wb_common::refresh_token_once(agent, creds) {
            let headers2 = wb_common::build_auth_headers(&nc, true);
            let (p2, b2, _saw2, _nd2) = fetch_round(agent, &headers2, (&summary_url, &paid_url, &free_url));
            pkgs = p2;
            balance = b2;
            new_creds = Some(nc);
        }
    }
    if !pkgs.is_empty() || balance.is_some() {
        return (pkgs, balance, "cloud", new_creds);
    }
    // 旧接口回退（F-20）
    let (status, body) = wb_common::post_json(agent, &old_url, &headers, &old_body());
    if status == 200 {
        if let Some(b) = body.filter(|b| b.is_object()) {
            let pkgs = packages_from(&b);
            if !pkgs.is_empty() {
                let bal: f64 = pkgs
                    .iter()
                    .filter_map(|p| p.get("remaining").and_then(Value::as_f64))
                    .sum();
                return (pkgs, Some(bal), "legacy", new_creds);
            }
        }
    }
    // 本地 quota 端口发现兜底（T5.8/F-21）：云端全链失败 → 探测本机桌面服务
    if let Some(local) = wb_common::local_quota_balance(agent) {
        return (vec![], Some(local), "local_quota", new_creds);
    }
    (vec![], None, "fetch_failed", new_creds)
}

/// 单账号取数 → 输出行（字段契约与 python fetch_account 一致）。
fn fetch_account(state: &AppState, agent: &ureq::Agent, acct: &Value) -> Value {
    let aid = acct.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let uid = acct.get("uid").and_then(Value::as_str).unwrap_or("");
    // python: nickname or uid[:8] or id
    let nick = acct.get("nickname").and_then(Value::as_str).unwrap_or("");
    let name = if !nick.is_empty() {
        nick.to_string()
    } else if !uid.is_empty() {
        uid.chars().take(8).collect()
    } else {
        aid.clone()
    };

    let creds = wb_common::effective_creds(state, &aid, uid);
    if creds.access_token.is_empty() {
        return json!({
            "user_id": aid, "name": name, "ok": false,
            "message": "无可用凭证", "balance": Value::Null,
            "packages": [], "source": "none",
        });
    }
    let (pkgs, balance, source, new_creds) = fetch_credits_once(agent, &creds);
    if let Some(nc) = new_creds {
        // 刷新成功 → 回写工具侧副本（F-10 谁新用谁）
        let _ = wb_common::save_token_store(state, &aid, &nc);
    }
    if pkgs.is_empty() && balance.is_none() {
        return json!({
            "user_id": aid, "name": name, "ok": false,
            "message": "积分查询失败（接口/网络）", "balance": Value::Null,
            "packages": [], "source": source,
        });
    }
    let mut pkgs = pkgs;
    let balance = balance.unwrap_or_else(|| {
        pkgs.iter()
            .filter_map(|p| p.get("remaining").and_then(Value::as_f64))
            .sum()
    });
    // 按到期升序（最先到期排最前，F-56）；无到期时间排最后
    pkgs.sort_by(|a, b| {
        let ta = a.get("expire_ts").and_then(Value::as_i64);
        let tb = b.get("expire_ts").and_then(Value::as_i64);
        match (ta, tb) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    let soon_cut = now_secs() as i64 + 7 * 86400;
    for p in pkgs.iter_mut() {
        let exp = p.get("expire_ts").and_then(Value::as_i64);
        p["expire_soon"] = json!(exp.map(|e| e < soon_cut).unwrap_or(false));
    }
    json!({
        "user_id": aid, "name": name, "ok": true, "balance": balance,
        "packages": pkgs, "source": source, "fetched_at": fs_utils::now_ts(),
    })
}

fn filter_accounts(v: Value, uid: &str, key: &str) -> Value {
    let mut arr = v.as_array().cloned().unwrap_or_default();
    arr.retain(|a| a.get(key).and_then(Value::as_str) == Some(uid));
    Value::Array(arr)
}

/// 积分查询主入口（对齐 python main）：缓存 → 全池/单账号取数 → stale-on-error →
/// 回写缓存。返回值即原 python stdout 末行 JSON（credits.rs 消费契约）。
pub fn fetch_credits(state: &AppState, user_id: Option<&str>, fresh: bool) -> Result<Value, String> {
    let agent = http_agent(30);
    let store = crate::store::db(&state.data_dir);
    let cache: Value = store.kv_get("workbuddy_credits_cache");
    let fetched_ts = cache.get("fetched_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let cache_ok = fetched_ts > 0.0;

    if !fresh && cache_ok && now_secs() - fetched_ts < CACHE_TTL_SECS {
        let cached_accounts = cache.get("accounts").cloned().unwrap_or(json!([]));
        let accounts = match user_id {
            Some(uid) => filter_accounts(cached_accounts, uid, "user_id"),
            None => cached_accounts,
        };
        return Ok(json!({"ok": true, "cached": true, "accounts": accounts}));
    }

    let pool: Value = crate::store::docs::wb_pool_load(&crate::store::db(&state.data_dir));
    let mut accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(uid) = user_id {
        accounts.retain(|a| a.get("id").and_then(Value::as_str) == Some(uid));
    }

    let mut results: Vec<Value> = vec![];
    for acct in &accounts {
        results.push(fetch_account(state, &agent, acct));
    }

    // stale-on-error（F-59）：刷新失败（全部账号 ok=false）且存在历史缓存 →
    // 回退输出过期缓存并标注 stale=true，不让看板空屏。
    // 同时不把全失败结果写回缓存（否则摧毁下次回退的数据源）。
    let all_failed = !results.is_empty()
        && results
            .iter()
            .all(|r| r.get("ok").and_then(Value::as_bool) != Some(true));
    if all_failed && cache_ok {
        let cached_accounts = cache.get("accounts").cloned().unwrap_or(json!([]));
        let filtered = match user_id {
            Some(uid) => filter_accounts(cached_accounts, uid, "user_id"),
            None => cached_accounts,
        };
        if filtered.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            return Ok(json!({"ok": true, "cached": true, "stale": true, "accounts": filtered}));
        }
    }

    let any_ok = results
        .iter()
        .any(|r| r.get("ok").and_then(Value::as_bool) == Some(true));
    let total_balance: f64 = results
        .iter()
        .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
        .filter_map(|r| r.get("balance").and_then(Value::as_f64))
        .sum();
    let out = json!({
        "ok": true, "cached": false, "accounts": results, "total_balance": total_balance,
    });
    // 缓存回写（池级）：至少一个账号成功才回写，全失败保留旧缓存作回退数据源
    if any_ok || results.is_empty() {
        let _ = store.kv_set(
            "workbuddy_credits_cache",
            &json!({
                "fetched_ts": now_secs(),
                "fetched_at": fs_utils::now_ts(),
                "accounts": out.get("accounts").cloned().unwrap_or(json!([])),
            }),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod wb_credits_tests {
    use super::*;

    #[test]
    fn to_ts_handles_timestamps_and_dates() {
        // 毫秒 → 秒
        assert_eq!(to_ts(Some(&json!(1_700_000_000_000i64))), Some(1_700_000_000));
        // 秒
        assert_eq!(to_ts(Some(&json!(1_700_000_000i64))), Some(1_700_000_000));
        // 过小（剩余量）非时间戳
        assert_eq!(to_ts(Some(&json!(123.0))), None);
        // ISO 字符串（Z 后缀 → RFC3339）
        assert_eq!(to_ts(Some(&json!("2099-12-31T23:59:59Z"))).is_some(), true);
        // 纯日期 → 23:59:59（UTC+8）
        let ts = to_ts(Some(&json!("2099-12-31"))).unwrap();
        let s = fmt_ts(ts);
        assert_eq!(s, "2099-12-31 23:59:59");
        // 日期分隔符变体
        assert!(to_ts(Some(&json!("2099/12/31"))).is_some());
        // 布尔/空串拒绝
        assert_eq!(to_ts(Some(&json!(true))), None);
        assert_eq!(to_ts(Some(&json!(""))), None);
    }

    #[test]
    fn packages_from_walks_nested_paths() {
        // data.data.Accounts 双层信封嵌套（F-20 的 6 种路径之一）
        let body = json!({
            "data": {
                "data": {
                    "Accounts": [
                        {
                            "PackageName": "体验包",
                            "PackageCode": "p_free",
                            "CycleCapacitySize": 100,
                            "CycleCapacityRemainPrecise": 37.5,
                            "PackageEndTime": 1_700_000_000_000i64,
                            "UsedCapacity": 62.5,
                        }
                    ]
                }
            }
        });
        let pkgs = packages_from(&body);
        assert_eq!(pkgs.len(), 1);
        let p = &pkgs[0];
        assert_eq!(p.get("name").and_then(Value::as_str), Some("体验包"));
        assert_eq!(p.get("code").and_then(Value::as_str), Some("p_free"));
        assert_eq!(p.get("remaining").and_then(Value::as_f64), Some(37.5));
        assert_eq!(p.get("total").and_then(Value::as_f64), Some(100.0));
        assert_eq!(p.get("used").and_then(Value::as_f64), Some(62.5));
        // 毫秒时间戳 → expire_ts 有值，end_time 统一形态
        assert_eq!(p.get("expire_ts").and_then(Value::as_i64), Some(1_700_000_000));
        assert_eq!(
            p.get("end_time").and_then(Value::as_str),
            Some("2023-11-15 06:13:20")
        );
    }

    #[test]
    fn remaining_falls_back_to_total_minus_used() {
        let pkg = json!({"CycleTotalCapacity": 200.0, "UsedCapacity": 50.0});
        assert_eq!(remaining_of(&pkg, total_of(&pkg)), Some(150.0));
        // 倒推不为负
        let pkg2 = json!({"CycleTotalCapacity": 10.0, "UsedCapacity": 90.0});
        assert_eq!(remaining_of(&pkg2, total_of(&pkg2)), Some(0.0));
    }

    #[test]
    fn balance_from_summary_prefers_top_level() {
        assert_eq!(
            balance_from_summary(&json!({"RemainingCapacity": 88.0})),
            Some(88.0)
        );
        // 新结构：Packages[] 各包 CycleRemainCapacity 求和
        let body = json!({"data": {"Packages": [
            {"CycleRemainCapacity": 10.0}, {"CycleRemainCapacity": 32.0}
        ]}});
        assert_eq!(balance_from_summary(&body), Some(42.0));
    }

    #[test]
    fn package_codes_dedup_preserve_order() {
        let body = json!({"data": {"Accounts": [
            {"PackageCode": "p_b", "CapacitySize": 1},
            {"PackageCode": "p_a", "CapacitySize": 1},
            {"PackageCode": "p_b", "CapacitySize": 2},
        ]}});
        assert_eq!(package_codes_from(&body), vec!["p_b", "p_a"]);
    }
}
