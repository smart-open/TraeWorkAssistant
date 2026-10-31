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

/// 单轮刷新的账号并发上限：账号之间无依赖，原串行 for 让「N 账号 × ≥3 次请求」在慢网络
/// （全局代理 / 跨境出口）下线性放大；同账号内部仍保持 summary → 包明细的先后顺序。
const MAX_FETCH_WORKERS: usize = 4;

/// 缓存过期时的「先回旧值」直出（stale-while-revalidate 的前半程，见
/// commands/workbuddy/credits.rs）：TTL 内 / 无缓存 / 账号明细为空 → None，
/// 交调用方走原路径（命中缓存或同步拉取）。
/// 返回体带 `cached:true, refreshing:true`，供前端稍后重取一次拿最新值；
/// 刻意不打 `stale`——该字段是 F-59「刷新失败回退」语义，本处刷新只是转后台、并未失败。
pub fn stale_reply(state: &AppState, user_id: Option<&str>) -> Option<Value> {
    let cache: Value = crate::store::db(&state.data_dir).kv_get("workbuddy_credits_cache");
    let fetched_ts = cache.get("fetched_ts").and_then(Value::as_f64).unwrap_or(0.0);
    if fetched_ts <= 0.0 || now_secs() - fetched_ts < CACHE_TTL_SECS {
        return None;
    }
    let cached_accounts = cache.get("accounts").cloned().unwrap_or(json!([]));
    if cached_accounts.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return None;
    }
    let accounts = match user_id {
        Some(uid) => filter_accounts(cached_accounts, uid, "user_id"),
        None => cached_accounts,
    };
    Some(json!({"ok": true, "cached": true, "refreshing": true, "accounts": accounts}))
}

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

/// 包标识码（summary 明细包均有；分类汇总行没有）。数值型 code 同样算明细包——
/// 上游序列化形态不固定，按字符串过滤会把它误判为无 code 汇总行而丢弃。
/// 口径：仅判定 dict **自身**是否携带 PackageCode——沿用 fs_utils::dig 会沿
/// ENVELOPE_KEYS 下钻到子层取到 code，把自身无 code 的父容器误判为明细包
/// → 父 + 子双算（issue #120 嵌套形态复发）
fn package_code_of(item: &Value) -> Option<String> {
    let v = item
        .get("PackageCode")
        .or_else(|| item.get("packageCode"))?;
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    (!s.is_empty()).then_some(s)
}

/// 已知分类汇总行名单（平台奖励积分 / 套餐基础积分 …）：summary 响应里这些行的
/// 数值是同类明细包的合计，与明细并存时重复计入会让余额精确翻倍。
const CATEGORY_SUMMARY_NAMES: [&str; 4] =
    ["平台奖励积分", "套餐基础积分", "套餐赠送积分", "购买积分"];

/// 无 code 项是否可识别为「已知分类汇总行」：只有这类无 code 行才与明细包重复、
/// 可在 has_coded 时跳过；其余无 code 项是旧结构真实包，不可静默丢弃（会少计）。
/// 顶层直查名字键优先——真实包顶层名不中黑名单即保留，不受子树内嵌名字干扰；
/// 顶层全缺时才降级 dig 深搜，保留「信封包裹的汇总行/包裹层」跳过能力（删深搜
/// 会让包裹层漏判 → 重复计数复发，比误丢无顶层名的真实包更严重）。
fn is_category_summary_row(item: &Value) -> bool {
    const NAME_KEYS: [&str; 6] =
        ["PackageName", "packageName", "Name", "name", "ProductCode", "description"];
    let hit = NAME_KEYS
        .iter()
        .find_map(|k| item.get(*k))
        .or_else(|| fs_utils::dig(item, &NAME_KEYS));
    hit.and_then(Value::as_str)
        .map_or(false, |n| CATEGORY_SUMMARY_NAMES.contains(&n))
}

/// 聚合父容器判定（issue #120：`data.CycleRemainCapacity` 与 `data.Packages[]`
/// 并存时父/子重复计数导致余额精确翻倍）：该无 code 项的子树里含任何一个带
/// code 的明细包 dict，则它只是明细的包裹层，其容量字段是明细合计——存在
/// 明细包时必须跳过。
fn is_aggregate_container(item: &Value, coded: &[&Value]) -> bool {
    fn contains(v: &Value, coded: &[&Value], depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        // 每层先判自身指针——信封键（response/data…）下的对象值也要判，
        // 只看数组元素会漏掉「父 → 对象键 → 明细包」路径，父被当成真实包 → 父 + 子双算
        if coded.iter().any(|c| std::ptr::eq(*c, v)) {
            return true;
        }
        match v {
            Value::Array(arr) => arr.iter().any(|e| contains(e, coded, depth + 1)),
            Value::Object(map) => map.values().any(|e| contains(e, coded, depth + 1)),
            _ => false,
        }
    }
    contains(item, coded, 0)
}

/// `packages_from` / `balance_from_summary` 共用的跳过谓词：存在带 code 明细时，
/// 无 code 项仅当「已知分类汇总行」或「聚合父容器」才跳过（其余是真实包，不可丢）。
fn is_skippable_uncoded(item: &Value, has_coded: bool, coded: &[&Value]) -> bool {
    has_coded
        && package_code_of(item).is_none()
        && (is_category_summary_row(item) || is_aggregate_container(item, coded))
}

/// 从三件套/旧接口响应提取包列表（容量键全树收集，6 种嵌套路径兼容）。
///
/// 混排响应过滤（2026-10-18，issue #120 后续）：summary 同时返回带 PackageCode 的
/// 明细包与无 code 的分类汇总行（数值是同类明细合计），汇总行不是积分包——存在
/// 明细包时必须跳过（实测包数 24→20、76→72）；但只按 code 过滤会丢掉上游未打 code
/// 的真实包（余额/包列表少计），因此只跳过「已知汇总行」与「聚合父容器」。
fn packages_from(body: &Value) -> Vec<Value> {
    let mut found = vec![];
    walk_capable_dicts(body, 0, &mut found);
    let coded: Vec<&Value> = found
        .iter()
        .copied()
        .filter(|d| package_code_of(d).is_some())
        .collect();
    let has_coded = !coded.is_empty();
    let mut out = vec![];
    for item in found {
        if is_skippable_uncoded(item, has_coded, &coded) {
            continue;
        }
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
        let code = package_code_of(item);
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
/// 聚合父字段（如 data.CycleRemainCapacity）与 Packages[] 明细并存时，父/子
/// 不可重复计数（issue #120 余额翻倍根因）。与 `packages_from` 同口径：存在带
/// PackageCode 的明细包时跳过「聚合父容器」与「已知分类汇总行」；补充（issue
/// #120 评论区 5 账号实测，2026-10-31 并入 stash 值去重口径）：分类汇总行/无
/// code 项按 remaining 值判重——与 coded Σ、任一 coded 明细同值，或与已计入
/// 的无 code 项同值（「套餐基础积分 ↔ CodeBuddy个人体验版」是同一笔积分的
/// 两个视图）均属重复口径只计其一；汇总行的值对不上任何明细时是唯一真实
/// 视图，计入以免套餐积分整体漏算（看板比官方少算）。明细读不出时宁可返回
/// None，不得回退「全量和」把汇总行加回来。
pub(crate) fn balance_from_summary(body: &Value) -> Option<f64> {
    if let Some(t) = num_of(fs_utils::dig(
        body,
        &["RemainingCapacity", "remaining", "TotalRemaining", "Balance", "balance"],
    )) {
        return Some(t);
    }
    let mut found = vec![];
    walk_capable_dicts(body, 0, &mut found);
    let coded: Vec<&Value> = found
        .iter()
        .copied()
        .filter(|d| package_code_of(d).is_some())
        .collect();
    if coded.is_empty() {
        // 全部无 PackageCode 的旧形态：回退全量和（原行为不变）
        let mut s = 0.0;
        let mut has = false;
        for item in found {
            if let Some(v) = remaining_of(item, total_of(item)) {
                s += v;
                has = true;
            }
        }
        return has.then_some(s);
    }
    // 新形态：coded 明细求和为基础（含 CycleCapacityRemainPrecise 与 used/total
    // 倒推宽键链）；明细读不出 → None，不回退汇总行。
    let coded_vals: Vec<Option<f64>> = coded
        .iter()
        .map(|d| remaining_of(d, total_of(d)))
        .collect();
    let base = coded_vals.iter().copied().sum::<Option<f64>>()?;
    // 无 code 补充口径：父容器跳过；分类汇总行若值与 coded Σ 或任一 coded 明细
    // 重复则不计（那是同类明细的合计视图）；所有无 code 项之间同值去重（双视图
    // 的名字未必都在黑名单里，按值兜底）；非汇总行的无 code 真实包保留。
    let mut seen: Vec<f64> = vec![];
    let mut extra = 0.0;
    for d in found.iter().copied() {
        if package_code_of(d).is_some() || is_aggregate_container(d, &coded) {
            continue;
        }
        let Some(v) = remaining_of(d, total_of(d)) else {
            continue;
        };
        let dup_of_coded =
            approx_f64(v, base) || coded_vals.iter().any(|c| c.is_some_and(|c| approx_f64(c, v)));
        if is_category_summary_row(d) && dup_of_coded {
            continue;
        }
        if seen.iter().any(|s| approx_f64(*s, v)) {
            continue;
        }
        seen.push(v);
        extra += v;
    }
    Some(base + extra)
}

/// 浮点近似相等（相对误差 1e-6）：聚合父包是上游按明细求和落库的，与本地
/// Σ 存在浮点尾数差；套餐双口径连尾数都一致（186.55000009），故用近似比较
/// 兜住两种来源。
fn approx_f64(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1.0)
}

/// summary 响应 → PackageCode 列表（去重，保序）。2026-09 起 paid/free 接口要求
/// 请求体携带 PackageCodes（空 body 返回 400 code=10001），代码列表只能先从 summary 提取。
fn package_codes_from(body: &Value) -> Vec<String> {
    let mut found = vec![];
    walk_capable_dicts(body, 0, &mut found);
    let mut codes: Vec<String> = vec![];
    for item in found {
        if let Some(c) = package_code_of(item) {
            if !codes.iter().any(|x| *x == c) {
                codes.push(c);
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
    state: &AppState,
    agent: &ureq::Agent,
    aid: &str,
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
        // H-1：加锁刷新 + 二次检查（他人已刷新落库直接复用）+ 内部落库
        //（原 refresh_token_once + 调用方 save，两步分离存在并发覆盖丢 token 面）
        if let (Some(nc), _, _) = wb_common::refresh_token_once_locked(state, agent, aid, creds) {
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
    let (pkgs, balance, source, _new_creds) = fetch_credits_once(state, agent, &aid, &creds);
    // new_creds 已由 refresh_token_once_locked 内部落库（H-1），此处无须再 save
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

/// 并行执行（≤workers 并发），结果与逐项耗时按**原序**回填；抽成泛型便于单测锁定
/// 「保序 + 确实并发」两条契约（f panic 就地捕获 → 槽位缺失返回 None，由调用方占位保序）。
fn parallel_ordered<T, R, F>(items: &[T], workers: usize, f: F) -> (Vec<Option<R>>, Vec<u128>)
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    if items.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let workers = workers.clamp(1, items.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: std::sync::Mutex<Vec<Option<(R, u128)>>> =
        std::sync::Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= items.len() {
                    break;
                }
                let t0 = std::time::Instant::now();
                // panic 占位落实（2026-10-08 评审）：thread::scope 会向调用方传播未捕获
                // panic（令整轮取数 Err），此处就地捕获——单账号异常只占位失败行，
                // 对齐「单账号失败不拖垮整轮」语义
                let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&items[i])));
                let ms = t0.elapsed().as_millis();
                if let Ok(out) = out {
                    if let Ok(mut guard) = slots.lock() {
                        guard[i] = Some((out, ms));
                    }
                }
            });
        }
    });
    let mut results: Vec<Option<R>> = Vec::with_capacity(items.len());
    let mut durations: Vec<u128> = Vec::with_capacity(items.len());
    for slot in slots.into_inner().unwrap_or_else(|e| e.into_inner()) {
        match slot {
            Some((r, ms)) => {
                results.push(Some(r));
                durations.push(ms);
            }
            None => {
                results.push(None);
                durations.push(0);
            }
        }
    }
    (results, durations)
}

/// 全池取数（并发 ≤ MAX_FETCH_WORKERS），结果按账号原序返回；线程异常的账号占位为失败行，
/// 不让后续账号结果错位（对齐原有「单账号失败不拖垮整轮」语义）。
fn fetch_accounts_parallel(
    state: &AppState,
    agent: &ureq::Agent,
    accounts: &[Value],
) -> (Vec<Value>, Vec<u128>) {
    let (slots, durations) = parallel_ordered(accounts, MAX_FETCH_WORKERS, |acct| {
        fetch_account(state, agent, acct)
    });
    let rows = slots
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            r.unwrap_or_else(|| {
                // 审查加固：nickname 缺失/为空时回退 uid 前 8 位，避免占位行
                // 空名（与 qoder_checkin 的 name 回退口径一致）
                let acct = &accounts[i];
                let uid = acct.get("id").and_then(Value::as_str).unwrap_or("");
                let name = acct
                    .get("nickname")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| uid.chars().take(8).collect());
                json!({
                    "user_id": acct.get("id").cloned().unwrap_or(json!("")),
                    "name": name,
                    "ok": false,
                    "message": "取数任务异常退出",
                    "balance": Value::Null,
                    "packages": [],
                    "source": "none",
                })
            })
        })
        .collect();
    (rows, durations)
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

    let t_all = std::time::Instant::now();
    let (results, durations) = fetch_accounts_parallel(state, &agent, &accounts);
    if !accounts.is_empty() {
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "[wb-credits] 取数 {} 账号 共 {}ms（并发 {} 路；逐账号 {}ms）",
                accounts.len(),
                t_all.elapsed().as_millis(),
                accounts.len().min(MAX_FETCH_WORKERS),
                durations
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join("/"),
            ),
        );
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
    // 缓存回写（池级）：至少一个账号成功才回写，全失败保留旧缓存作回退数据源。
    // 单账号过滤路径（user_id=Some）不回写——结果只是池级缓存的子集（目标账号
    // 不存在时为空集），回写会在 TTL 内把池级缓存污染成残缺/空列表，
    // 之后不带 uid 的池级查询会命中并返回空账号
    if user_id.is_none() && (any_ok || results.is_empty()) {
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
    fn balance_from_summary_no_double_count_with_packages() {
        // issue #120 翻倍形态（移植自 main 5f88dfe）：聚合父字段与 Packages[]
        // 明细并存 → 仅按明细求和（修复前：父 800 + 包 500+300 = 1600 = 2× 真实余额，
        // 签到差值兜底随之翻倍）
        let body = json!({"data": {
            "CycleRemainCapacity": 800.0,
            "Packages": [
                {"PackageCode": "p_a", "CycleRemainCapacity": 500.0},
                {"PackageCode": "p_b", "CycleRemainCapacity": 300.0}
            ]
        }});
        assert_eq!(balance_from_summary(&body), Some(800.0));
        // 全部无 PackageCode 的旧形态：回退全量和（原行为不变）
        let legacy = json!({"data": {
            "CycleRemainCapacity": 30.0,
            "Accounts": [{"RemainingCapacity": 10.0}]
        }});
        assert_eq!(balance_from_summary(&legacy), Some(40.0));
    }

    #[test]
    fn balance_from_summary_parent_with_nested_code_not_double_counted() {
        // 「父 dict 自身无 PackageCode、仅子信封键下有」形态：coded 判定若用 dig
        // 深搜会沿 ENVELOPE_KEYS 下钻到子层取到 code，父被误判为 coded →
        // 父 100 + 子 50 双算复发（issue #120 根因形态之一）
        let body = json!({"data": {
            "CycleRemainCapacity": 100.0,
            "response": {"PackageCode": "p_x", "CycleRemainCapacity": 50.0}
        }});
        assert_eq!(balance_from_summary(&body), Some(50.0));
    }

    #[test]
    fn balance_from_summary_coded_uses_wide_remaining_chain() {
        // 审查修复（M3，移植自 main 1148b7b）：明细仅携带 CycleCapacityRemainPrecise
        //（不含 CycleRemainCapacity）时，coded 求和亦须命中——此前窄键链导致
        // coded 求和落空、静默跌回全量和（父 + 明细翻倍）
        let body = json!({"data": {
            "CycleRemainCapacity": 800.0,
            "Packages": [
                {"PackageCode": "p_a", "CycleCapacityRemainPrecise": 500.0},
                {"PackageCode": "p_b", "CycleCapacityRemainPrecise": 300.0}
            ]
        }});
        assert_eq!(balance_from_summary(&body), Some(800.0));
        // M3：末级不再回落「周期总量」键——总量不是余额，宁缺勿错
        let total_only = json!({"data": {"CycleTotalCapacity": 1000.0}});
        assert_eq!(balance_from_summary(&total_only), None);
    }

    /// 分类汇总行（无 code，数值是同类明细包的合计）不得出现在积分包列表里。
    /// 余额在 M3 之后已只加明细包，但包列表仍会把汇总行当包（实测包数多出 4 条）。
    #[test]
    fn summary_rows_not_listed_as_packages() {
        let body = json!({"data": {
            "Packages": [
                {"PackageCode": "p_reward", "PackageName": "裂变包",
                 "CycleRemainCapacity": 100.0, "CycleTotalCapacity": 100.0},
                {"PackageCode": "p_base", "PackageName": "基础包",
                 "CycleRemainCapacity": 50.5, "CycleTotalCapacity": 50.5}
            ],
            "Summary": [
                {"Name": "平台奖励积分", "CycleRemainCapacity": 100.0},
                {"Name": "套餐基础积分", "CycleRemainCapacity": 50.5}
            ]
        }});
        assert_eq!(packages_from(&body).len(), 2, "汇总行不是积分包");
        assert_eq!(balance_from_summary(&body), Some(150.5));
    }

    /// 顶层无余额字段时，不能把某个包的 remaining 当成总余额
    ///（`fs_utils::dig` 是全树深搜，命中第一个包就返回 → 少计）。
    #[test]
    fn balance_does_not_use_single_package_as_total() {
        let body = json!({"data": {"Packages": [
            {"PackageCode": "p_a", "RemainingCapacity": 100.0},
            {"PackageCode": "p_b", "RemainingCapacity": 50.0}
        ]}});
        assert_eq!(balance_from_summary(&body), Some(150.0));
    }

    /// 混排形态：带 code 明细包 + 无 code 但**不是**已知分类汇总行的真实包，
    /// 后者必须保留（只按 code 过滤会少计），余额与包列表同口径。
    #[test]
    fn mixed_uncoded_real_package_is_kept() {
        let body = json!({"data": {"Packages": [
            {"PackageCode": "p_a", "PackageName": "裂变包", "CycleRemainCapacity": 100.0},
            {"PackageName": "老结构体验包", "CycleRemainCapacity": 50.0}
        ]}});
        assert_eq!(packages_from(&body).len(), 2, "无 code 真实包不应被丢弃");
        assert_eq!(balance_from_summary(&body), Some(150.0));
    }

    /// PackageCode 为数值型（上游序列化差异）时同样算明细包，
    /// 不能误判为无 code 汇总行而丢弃。
    #[test]
    fn numeric_package_code_counts_as_detail() {
        let body = json!({"data": {"Packages": [
            {"PackageCode": 123456, "PackageName": "裂变包", "CycleRemainCapacity": 100.0},
            {"PackageCode": "p_b", "PackageName": "基础包", "CycleRemainCapacity": 20.0},
            {"Name": "平台奖励积分", "CycleRemainCapacity": 120.0}
        ]}});
        assert_eq!(packages_from(&body).len(), 2);
        assert_eq!(balance_from_summary(&body), Some(120.0));
    }

    /// 明细包只带总量、没有剩余（上游换字段名/新口径）时宁缺勿错：不得回退
    /// 「全量和」把分类汇总行加回来（那是翻倍的另一种触发路径）。
    #[test]
    fn unreadable_details_do_not_fall_back_to_summary_rows() {
        let body = json!({"data": {
            "Packages": [{"PackageCode": "p_a", "PackageName": "裂变包", "CapacitySize": 100.0}],
            "Summary": [{"Name": "平台奖励积分", "CycleRemainCapacity": 100.0}]
        }});
        assert_eq!(balance_from_summary(&body), None);
    }

    /// issue #120 评论区 5 账号实测形态（回归锁定）：聚合父字段与明细并存 + 无 code
    /// 分类汇总行「平台奖励积分」（== coded Σ）+「套餐基础积分 ↔ CodeBuddy个人体验版」
    /// 同值双口径对 → coded Σ + 体验版值；汇总行/父容器不进包列表。
    #[test]
    fn balance_from_summary_issue120_comment_shape() {
        let body = json!({"data": {
            "CycleRemainCapacity": 1386.5,
            "Packages": [
                {"PackageName": "每日签到裂变包", "PackageCode": "TCACA_code_007_a",
                 "CycleRemainCapacity": 1200.0},
                {"PackageName": "平台奖励积分", "CycleRemainCapacity": 1200.0},
                {"PackageName": "套餐基础积分", "CycleRemainCapacity": 186.5},
                {"PackageName": "CodeBuddy个人体验版", "CycleRemainCapacity": 186.5}
            ]
        }});
        assert_eq!(balance_from_summary(&body), Some(1386.5));
        assert_eq!(packages_from(&body).len(), 2, "汇总行与父容器不是积分包");
    }

    #[test]
    fn balance_from_summary_uncoded_pair_and_aggregate() {
        // issue #120 评论区 5 账号实测形态：coded 明细 + 无 code「聚合父包」
        //（== coded Σ，不计）+「套餐基础积分 ↔ 个人体验版」同值双口径（只计
        // 其一）→ coded Σ + 套餐值，而非漏算套餐
        let body = json!({"data": {"Packages": [
            {"PackageName": "每日签到裂变包", "PackageCode": "TCACA_code_007_a", "CycleRemainCapacity": 1200.0},
            {"PackageName": "平台奖励积分", "CycleRemainCapacity": 1200.0},
            {"PackageName": "套餐基础积分", "CycleRemainCapacity": 186.5},
            {"PackageName": "CodeBuddy个人体验版", "CycleRemainCapacity": 186.5}
        ]}});
        assert_eq!(balance_from_summary(&body), Some(1386.5));
        // 不同值的无 code 包都计（去重仅针对同值重复视图）；父包仍排除
        let distinct = json!({"data": {"Packages": [
            {"PackageCode": "p_a", "CycleRemainCapacity": 500.0},
            {"PackageName": "平台奖励积分", "CycleRemainCapacity": 500.0},
            {"PackageName": "套餐基础积分", "CycleRemainCapacity": 186.5},
            {"PackageName": "其他权益包", "CycleRemainCapacity": 42.5}
        ]}});
        assert_eq!(balance_from_summary(&distinct), Some(729.0));
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

    // ── 并发取数 / SWR 缓存三态（2026-10-08 提速修订）────────────────────

    fn temp_state(tag: &str) -> AppState {
        let dir = std::env::temp_dir().join(format!("aiwork_wbcredits_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("data"));
        AppState {
            data_dir: dir,
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            buddy_ai_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            events: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[test]
    fn 并行取数保序且确实并发() {
        let items: Vec<u32> = (0..8).collect();
        let live = std::sync::atomic::AtomicUsize::new(0);
        let max_live = std::sync::atomic::AtomicUsize::new(0);
        let (out, durations) = parallel_ordered(&items, 4, |x| {
            let cur = live.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            max_live.fetch_max(cur, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(20));
            live.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            x * 2
        });
        let out: Vec<u32> = out.into_iter().map(|v| v.expect("槽位应齐全")).collect();
        assert_eq!(out, vec![0, 2, 4, 6, 8, 10, 12, 14], "结果必须按原序回填");
        assert_eq!(durations.len(), 8);
        assert!(
            max_live.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "应观察到并发执行（≥2 路同时在跑）"
        );
    }

    #[test]
    fn 并发取数panic占位不传播() {
        // 2026-10-08 评审：f panic 必须就地捕获占位 None——thread::scope 会传播未捕获
        // panic，若不捕获则单账号异常令整轮取数 Err（占位语义回归锁定）
        let items: Vec<u32> = vec![1, 2, 3];
        let (out, durations) = parallel_ordered(&items, 2, |x| {
            if *x == 2 {
                panic!("模拟单账号取数异常");
            }
            x * 10
        });
        assert_eq!(out[0], Some(10));
        assert!(out[1].is_none(), "panic 项应占位 None");
        assert_eq!(out[2], Some(30));
        assert_eq!(durations[1], 0, "panic 项耗时占位 0");
    }

    #[test]
    fn 缓存过期才先回旧值() {
        let st = temp_state("stale_reply");
        let store = crate::store::db(&st.data_dir);
        // 无缓存 → None（交调用方同步拉）
        assert!(stale_reply(&st, None).is_none());
        // TTL 内 → None（照常命中缓存，无需转后台）
        let _ = store.kv_set(
            "workbuddy_credits_cache",
            &json!({"fetched_ts": now_secs() - 60.0, "accounts": [{"user_id": "u1", "balance": 1.0}]}),
        );
        assert!(stale_reply(&st, None).is_none());
        // 过期 → 先回旧值 + refreshing 标记；不打 stale
        //（stale 是 F-59「刷新失败回退」语义，SWR 只是转后台刷新，并未失败）
        let _ = store.kv_set(
            "workbuddy_credits_cache",
            &json!({
                "fetched_ts": now_secs() - CACHE_TTL_SECS - 1.0,
                "accounts": [{"user_id": "u1", "balance": 1.0}],
            }),
        );
        let reply = stale_reply(&st, None).expect("过期缓存应能先回旧值");
        assert_eq!(reply["cached"], json!(true));
        assert_eq!(reply["refreshing"], json!(true));
        assert!(reply.get("stale").is_none(), "SWR 回旧值不应打 stale");
        assert_eq!(reply["accounts"].as_array().unwrap().len(), 1);
        // 单账号过滤：命中 1 条、未命中 0 条
        assert_eq!(
            stale_reply(&st, Some("u1")).unwrap()["accounts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            stale_reply(&st, Some("nope")).unwrap()["accounts"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        // 账号明细为空的缓存视为无缓存 → None（无数据可展示，只能同步拉）
        let _ = store.kv_set(
            "workbuddy_credits_cache",
            &json!({"fetched_ts": now_secs(), "accounts": []}),
        );
        assert!(stale_reply(&st, None).is_none());
        let _ = std::fs::remove_dir_all(&st.data_dir);
    }
}
