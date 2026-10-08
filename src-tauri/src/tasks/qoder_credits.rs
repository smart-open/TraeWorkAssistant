//! Qoder 积分引擎（F-80；M0 抓包固化，对照 wb_credits.rs 模式）。
//!
//! 三通道取数设计（§5.3，按可用性降级）：
//! - A（首选官方）：PAT `pt-` 直调 usage（jobToken 通道见下，M2 评估）
//! - B：客户端 token（dt-/jt-，MITM 捕获透传）直调 usage
//! - C：CLI `/usage` 输出解析 —— M4 评估
//!
//! R-7 已闭合（2026-09-27 抓包实测）——真正的用量端点是：
//!   `GET {open_api}/sash/api/v2/me/usage`（Bearer + cosy-clienttype:10，UA "Qoder"）
//! 响应（设计文档猜测的 /api/v2/quota/usage 客户端并未调用）：
//! ```json
//! { "displayMode":"qoder", "qoderUsage": {
//!     "userId":"...", "userType":"personal_professional_trial", "usageType":"credits",
//!     "isQuotaExceeded":false, "expiresAt":1791673619906,
//!     "userQuota":  { "total":300, "used":0, "remaining":300, "percentage":0, "unit":"credits" },
//!     "addOnQuota": { "total":100, "used":0, "remaining":100, "percentage":0,
//!                     "unit":"credits", "detailUrl":"https://qoder.com/account/usage" } } }
//! ```
//! （addOnQuota 仅在账号持有 Add-on 包时出现）
//!
//! jobToken 端点（R-6 抓包实测，M2 通道 A 备用）：`POST /api/v1/me/jobToken`
//! body `{"clientId":"<uuid>"}` → `{token, expires_in:86400000(ms,24h),
//! refresh_token, refresh_token_expires_in:172800000(48h)}`。
//!
//! 缓存：kv `qoder_credits_cache`，TTL 600s + stale-on-error（F-59 模式）。
//! 快照：每日 qoder_credits_history 同日覆盖 + 365 天裁剪。

use serde_json::{json, Value};

use crate::fs_utils;
use crate::state::AppState;

use super::http_agent;
use super::qoder_common;

/// 缓存 TTL（秒）
const CACHE_TTL_SECS: i64 = 600;

/// 逐包明细探测失败负缓存 TTL：双端点全失败后 30 分钟内不再重复探测。
/// 2026-10-04 实测：qoder.cn 对 openapi Bearer 恒 401（账户页端点走 Web 会话
/// 鉴权，与 R-11 抓包「Bearer 同源」假设不符；携带 Referer/Origin/浏览器 UA
/// 仍 401）、openapi 主机恒 503（alb 无路由）——反复探测只产生日志噪音
///（实测看板 8 秒内 4 轮 × 2 账号 × 2 端点空打 16 次 + 8 条重复 warn）。
/// TTL 过后自动复探：服务端放开鉴权/路由即自愈
const PER_PACK_FAIL_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// 逐包明细失败负缓存（进程内内存态：账号 id → 首败时刻）
fn per_pack_fail_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 负缓存命中：TTL 内本轮跳过双端点探测
fn per_pack_fail_hit(aid: &str) -> bool {
    per_pack_fail_cache()
        .lock()
        .map(|c| c.get(aid).is_some_and(|t| t.elapsed() < PER_PACK_FAIL_TTL))
        .unwrap_or(false)
}

fn per_pack_fail_mark(aid: &str) {
    if let Ok(mut c) = per_pack_fail_cache().lock() {
        c.insert(aid.to_string(), std::time::Instant::now());
    }
}

fn per_pack_fail_clear(aid: &str) {
    if let Ok(mut c) = per_pack_fail_cache().lock() {
        c.remove(aid);
    }
}

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

fn num_or_none(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 递归找余额数值（remaining/balance 语义，排除 total；宽容兜底用）
fn deep_balance_dig(v: &Value, depth: usize) -> Option<f64> {
    if depth > 8 {
        return None;
    }
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                let kl = k.to_ascii_lowercase();
                if (kl.contains("remaining") || kl.contains("balance")) && !kl.contains("total") {
                    // 审查 P3：负余额钳 0——服务端脏数据（used>total、负 remaining）
                    // 不该让看板出现「负积分」（与 quota_pair 推导路径同款守卫）
                    if let Some(n) = num_or_none(Some(val)).map(|n| n.max(0.0)) {
                        return Some(n);
                    }
                }
            }
            for val in m.values() {
                if val.is_object() || val.is_array() {
                    if let Some(n) = deep_balance_dig(val, depth + 1) {
                        return Some(n);
                    }
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(|x| deep_balance_dig(x, depth + 1)),
        _ => None,
    }
}

/// quota 子对象 → (remaining, used)（宽容：remaining / total-used 键序取值）。
/// used = 订阅周期内已消耗，积分看板 v2「订阅版本的资源」进度数据源（原实现丢弃）。
fn quota_pair(q: Option<&Value>) -> (Option<f64>, Option<f64>) {
    let Some(q) = q else { return (None, None) };
    let used = num_or_none(q.get("used"));
    // total-used 推导路径钳 0（审查 L-负余额）：服务端 used>total 的脏数据会让
    // 推导 remaining 为负，余额看板出现「负积分」误导用户
    let remaining = num_or_none(q.get("remaining"))
        .or_else(|| num_or_none(q.get("total")).map(|t| (t - used.unwrap_or(0.0)).max(0.0)));
    (remaining, used)
}

/// 毫秒时间戳 → YYYY-MM-DD（到期日历展示）
fn ms_to_date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// usage 解析产出（R-7 主结构 + 宽容兜底）。
/// used（userQuota/addOnQuota.used，订阅周期内已消耗）与 plan_expires_at
/// （qoderUsage.expiresAt → 订阅周期到期日）为积分看板 v2 新增捕获字段。
#[derive(Default, Debug, PartialEq)]
struct UsageParsed {
    plan: Option<f64>,
    plan_used: Option<f64>,
    addon: Option<f64>,
    addon_used: Option<f64>,
    total: Option<f64>,
    /// 订阅周期到期日（YYYY-MM-DD；空 = 响应缺失）
    plan_expires_at: String,
    packages: Vec<Value>,
}

/// usage 响应 → UsageParsed。
/// 主路径：R-7 抓包结构（qoderUsage.userQuota / addOnQuota）；
/// 兜底：候选键 + 递归深挖（结构变更时不静默返回 0，而是全 None → 显式失败）。
fn parse_usage(b: &Value) -> UsageParsed {
    let q = b.get("qoderUsage");
    let (plan, plan_used) = quota_pair(q.and_then(|v| v.get("userQuota")));
    let (addon, addon_used) = quota_pair(q.and_then(|v| v.get("addOnQuota")));
    let expires_at_ms = q
        .and_then(|v| v.get("expiresAt"))
        .and_then(Value::as_i64);
    let plan_expires_at = expires_at_ms.map(ms_to_date).unwrap_or_default();
    let total = match (plan, addon) {
        (Some(p), Some(a)) => Some(p + a),
        (Some(p), None) | (None, Some(p)) => Some(p),
        _ => num_or_none(fs_utils::dig(
            b,
            &["totalRemaining", "remainingCredits", "remaining_credits", "remaining", "balance"],
        ))
        .or_else(|| deep_balance_dig(b, 0)),
    };
    // Add-on 包 → 到期日历（expiresAt 为 qoderUsage 级 ms 时间戳）
    let mut packages: Vec<Value> = Vec::new();
    if let Some(a) = addon {
        packages.push(json!({
            "amount": a,
            "expire_at": plan_expires_at,
            "source": "addon",
        }));
    }
    // 专属/组织资源包逐包明细（2026-10-04 Work 客户端 app.asar 解析器 Upt 反推，
    // sash usage 响应原生携带）：[{total, used, remaining, expires_at|expiresAt(ms),
    // status: "QUOTA_DETAIL_STATUS_*", name | display_labels[].value}]
    // 口径对齐 parse_big_model：已用完（remaining ≤ 0）、非激活（status 非空且非
    // ACTIVE）不进明细；expires_at 缺失/0 视为随订阅周期（plan_expires_at 兜底）。
    // 逐包与 addon 聚合并存——聚合供余额链路，逐包供到期日历
    if let Some(arr) = q
        .and_then(|v| {
            v.get("dedicated_resource_packages")
                .or_else(|| v.get("dedicatedResourcePackages"))
        })
        .and_then(Value::as_array)
    {
        for d in arr {
            let used = num_or_none(d.get("used"));
            // remaining 缺失时 total-used 推导并钳 0（同 quota_pair 负余额防护）
            let amount = num_or_none(d.get("remaining"))
                .or_else(|| num_or_none(d.get("total")).map(|t| (t - used.unwrap_or(0.0)).max(0.0)));
            // 已用完的包无到期提醒价值，不进明细（Trae/Buddy 同款口径）
            if amount.map(|a| a <= 0.0).unwrap_or(true) {
                continue;
            }
            let status = s_of(d.get("status"));
            // sash 侧 status 为枚举全前缀形态（QUOTA_DETAIL_STATUS_ACTIVE，Work 客户端
            // 同款），web big_model 侧为裸 ACTIVE——剥离前缀后归一比较
            let status = status.strip_prefix("QUOTA_DETAIL_STATUS_").unwrap_or(&status);
            if !status.is_empty() && status != "ACTIVE" {
                continue;
            }
            let exp_ms = d
                .get("expires_at")
                .or_else(|| d.get("expiresAt"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let expire_at = if exp_ms > 0 { ms_to_date(exp_ms) } else { plan_expires_at.clone() };
            // 包名取值序：name → display_labels[0].value → 「专属资源包」（Work 客户端同款）
            let name = {
                let n = s_of(d.get("name"));
                if !n.is_empty() {
                    n
                } else {
                    d.get("display_labels")
                        .or_else(|| d.get("displayLabels"))
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(|l| l.get("value"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("专属资源包")
                        .to_string()
                }
            };
            packages.push(json!({
                "amount": amount,
                "total": num_or_none(d.get("total")),
                "expire_at": expire_at,
                "source": "dedicated",
                "name": name,
            }));
        }
    }
    // 宽容兜底：数组形态积分包（结构变更时尽力展示）
    if packages.is_empty() {
        for key in ["packages", "creditPackages", "credit_packages", "items", "resources"] {
            if let Some(arr) = fs_utils::dig(b, &[key]).and_then(Value::as_array) {
                for p in arr {
                    let amount = num_or_none(fs_utils::dig(p, &["amount", "remaining", "balance", "credits", "value"]));
                    let expire = s_of(fs_utils::dig(p, &["expireAt", "expire_at", "endAt", "end_at", "expiredAt", "expiresAt"]));
                    if amount.is_some() || !expire.is_empty() {
                        packages.push(json!({ "amount": amount, "expire_at": expire, "source": s_of(fs_utils::dig(p, &["source", "type", "kind"])) }));
                    }
                }
                if !packages.is_empty() {
                    break;
                }
            }
        }
    }
    UsageParsed { plan, plan_used, addon, addon_used, total, plan_expires_at, packages }
}

// ── 逐包明细端点（R-11 抓包 2026-10-04，官网 account/usage 页）─────────────
// `GET {WEB_BASE}/api/v2/me/usages/big_model_credits`（端点在官网域 qoder.cn，
// Bearer 鉴权与 openapi 同源；openapi 主机无此路由——实测 alb 503，保留兜底）
// 响应四族配额：plan_quota（订阅配额）/ resource_package_quota（个人资源包）/
// dedicated_resource_package_quota（专属资源包）/ total_quota（全量合并视图），
// 每族 { quota_summary, quota_detail[] }；detail 条目含
// id / limit_value / used_value / remaining_value / expires_at（ms，PLAN 为 0=
// 随订阅周期重置）/ source（"PLAN" | "RESOURCE_PACKAGE_SOURCE_BONUS" | ...）/ status。
// 明细取 total_quota.quota_detail——合并视图天然覆盖专属族，无需逐族拼接。

/// big_model_credits 解析产出（字段级覆盖 sash 聚合口径，缺失回退）
#[derive(Default, Debug, PartialEq)]
struct BigModelParsed {
    /// total_quota.quota_summary.remaining_value（全量剩余；与 sash plan+addon 同语义）
    total: Option<f64>,
    /// plan_quota.quota_summary（订阅配额，随订阅周期重置）
    plan: Option<f64>,
    plan_used: Option<f64>,
    /// resource_package_quota.quota_summary（个人资源包聚合；sash addOnQuota 同源）
    addon: Option<f64>,
    addon_used: Option<f64>,
    /// nextResetAt（订阅周期重置时刻 ms；sash expiresAt 缺失时兜底）
    next_reset_ms: Option<i64>,
    /// 逐包明细（PLAN + 个人资源包；已用完/非激活过滤）
    packages: Vec<Value>,
}

/// detail 条目 source → 稳定标识（前端分型展示用）：
/// "PLAN" → "plan"（订阅配额）；"RESOURCE_PACKAGE*" → "bonus"（个人资源包）；
/// 其余保留原值小写（未知来源前端按通用积分包展示）
fn big_model_source(s: &str) -> String {
    if s == "PLAN" {
        "plan".into()
    } else if s.starts_with("RESOURCE_PACKAGE") {
        "bonus".into()
    } else {
        s.to_ascii_lowercase()
    }
}

/// big_model_credits 响应 → BigModelParsed（纯函数便于单测）。
/// 过滤口径对齐 Trae/Buddy 包级口径：已用完（remaining 0）、非激活（is_active=false
/// 或 status 非 ACTIVE）的包不进明细。
fn parse_big_model(b: &Value, plan_expires_at: &str) -> BigModelParsed {
    let summary = |key: &str| b.get(key).and_then(|v| v.get("quota_summary"));
    let pair = |q: Option<&Value>| {
        let remaining = num_or_none(q.and_then(|v| v.get("remaining_value")));
        let used = num_or_none(q.and_then(|v| v.get("used_value")));
        (remaining, used)
    };
    let (plan, plan_used) = pair(summary("plan_quota"));
    let (addon, addon_used) = pair(summary("resource_package_quota"));
    let total = num_or_none(summary("total_quota").and_then(|v| v.get("remaining_value")));
    let next_reset_ms = b.get("nextResetAt").and_then(Value::as_i64);
    let mut packages: Vec<Value> = Vec::new();
    if let Some(arr) = b
        .get("total_quota")
        .and_then(|v| v.get("quota_detail"))
        .and_then(Value::as_array)
    {
        for d in arr {
            let amount = num_or_none(d.get("remaining_value"));
            // 已用完的包无到期提醒价值，不进明细（Trae/Buddy 同款口径）
            if amount.map(|a| a <= 0.0).unwrap_or(true) {
                continue;
            }
            // 激活态宽容过滤：status 非空且非 ACTIVE / is_active 显式 false 才跳过
            let status = s_of(d.get("status"));
            if !status.is_empty() && status != "ACTIVE" {
                continue;
            }
            if d.get("is_active").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let exp_ms = d.get("expires_at").and_then(Value::as_i64).unwrap_or(0);
            // expires_at=0（PLAN）＝随订阅周期重置：到期时间取 sash 侧订阅周期到期日
            let expire_at = if exp_ms > 0 {
                ms_to_date(exp_ms)
            } else {
                plan_expires_at.to_string()
            };
            packages.push(json!({
                "amount": amount,
                "total": num_or_none(d.get("limit_value")),
                "expire_at": expire_at,
                "source": big_model_source(&s_of(d.get("source"))),
            }));
        }
    }
    BigModelParsed { total, plan, plan_used, addon, addon_used, next_reset_ms, packages }
}

/// 当前余额（签到奖励差值兜底数据源）：plan + addon remaining 之和；失败 None。
pub fn fetch_usage_balance(agent: &ureq::Agent, headers: &[(String, String)]) -> Option<f64> {
    let url = format!("{}/sash/api/v2/me/usage", qoder_common::OPEN_API_BASE);
    let (status, body, _raw) = qoder_common::get_json(agent, &url, headers);
    if status != 200 {
        return None;
    }
    let parsed = parse_usage(&body?);
    // total 为 None 时 plan/addon 必为 (None, None)（任一为 Some 则 total 必为 Some），
    // 旧版 or_else 兜底恒返回 None，属死代码
    parsed.total
}

/// 单账号积分查询（token 直调 usage 通道）。creds 由调用方解析（需要 AppState 读 token store）。
/// data_dir 仅用于明细端点全失败时的 warn 日志（app_log）。
fn fetch_account(
    agent: &ureq::Agent,
    acct: &Value,
    creds: &qoder_common::QoderCreds,
    data_dir: &std::path::Path,
) -> Value {
    let aid = s_of(acct.get("id"));
    let name = acct
        .get("nickname")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| aid.clone());
    let mut row = json!({
        "user_id": aid,
        "name": name,
        "ok": false,
        "plan_credits": Value::Null,
        "plan_used": Value::Null,
        "addon_credits": Value::Null,
        "addon_used": Value::Null,
        "total": Value::Null,
        "plan_expires_at": "",
        "packages": [],
        "source": "fetch_failed",
        "fetched_at": fs_utils::now_iso(),
    });
    if creds.access_token.is_empty() {
        row["message"] = json!("无可用凭证");
        row["source"] = json!("none");
        return row;
    }
    let headers = qoder_common::build_auth_headers(&creds);
    let url = format!("{}/sash/api/v2/me/usage", qoder_common::OPEN_API_BASE);
    let (status, body, _raw) = qoder_common::get_json(agent, &url, &headers);
    if status != 200 {
        // P3：结构化状态码随行——401 自愈判定不再依赖 message 文本匹配（message
        // 仅人类可读描述）；前端按需读取，多出字段无副作用
        row["http_status"] = json!(status);
        row["message"] = json!(format!("usage 不可用（HTTP {status}）"));
        return row;
    }
    let Some(b) = body else {
        row["message"] = json!("usage 响应非 JSON");
        return row;
    };
    let p = parse_usage(&b);
    if p.plan.is_none() && p.addon.is_none() && p.total.is_none() {
        // 结构未识别：显式失败（§九-2 不静默），不阻塞其他账号
        row["message"] = json!("usage 结构未识别（接口可能已变更）");
        return row;
    }
    row["ok"] = json!(true);
    row["plan_credits"] = json!(p.plan);
    row["plan_used"] = json!(p.plan_used);
    row["addon_credits"] = json!(p.addon);
    row["addon_used"] = json!(p.addon_used);
    row["total"] = json!(p.total);
    row["plan_expires_at"] = json!(p.plan_expires_at);
    row["packages"] = json!(p.packages);
    // 逐包明细增强（R-11）：端点在官网域（WEB_BASE）——openapi 主机无此路由
    //（实测 alb 503），保留为兜底（未来开放即生效）。任一主机 200 即解析；
    // 字段级覆盖——新接口有值才覆盖，缺失/失败（非 200、非 JSON）静默回退
    // sash 聚合口径，不影响既有余额链路；双端点全失败记 warn（首败一次）。
    // 负缓存：TTL 内命中跳过探测（2026-10-04 实测双端点对现有凭证恒 401/503，
    // 详见 PER_PACK_FAIL_TTL 注释）
    let mut big_body: Option<Value> = None;
    let mut tried: Vec<String> = Vec::new();
    let fail_cached = per_pack_fail_hit(&aid);
    if !fail_cached {
        for host in [qoder_common::WEB_BASE, qoder_common::OPEN_API_BASE] {
            let big_url = format!("{host}/api/v2/me/usages/big_model_credits");
            let (bs, bb, _) = qoder_common::get_json(agent, &big_url, &headers);
            if bs == 200 {
                if bb.is_some() {
                    big_body = bb;
                    break;
                }
                tried.push(format!("{host} HTTP 200 非 JSON"));
            } else {
                tried.push(format!("{host} HTTP {bs}"));
            }
        }
    }
    if let Some(bv) = big_body {
        per_pack_fail_clear(&aid);
        let bm = parse_big_model(&bv, &p.plan_expires_at);
        if let Some(v) = bm.total {
            row["total"] = json!(v);
        }
        if let Some(v) = bm.plan {
            row["plan_credits"] = json!(v);
        }
        if let Some(v) = bm.plan_used {
            row["plan_used"] = json!(v);
        }
        if let Some(v) = bm.addon {
            row["addon_credits"] = json!(v);
        }
        if let Some(v) = bm.addon_used {
            row["addon_used"] = json!(v);
        }
        if !bm.packages.is_empty() {
            row["packages"] = json!(bm.packages);
        }
        // sash 侧 expiresAt 缺失时以 nextResetAt 兜底订阅周期到期日
        if p.plan_expires_at.is_empty() {
            if let Some(nr) = bm.next_reset_ms {
                if nr > 0 {
                    row["plan_expires_at"] = json!(ms_to_date(nr));
                }
            }
        }
    } else if !tried.is_empty() {
        // 首败落 warn 并记负缓存；TTL 内静默（负缓存命中本轮 tried 恒空，不达此处）
        per_pack_fail_mark(&aid);
        fs_utils::app_log(
            data_dir,
            &format!(
                "qoder_credits: 账号 {name} 逐包明细端点均不可用（{}），回退聚合口径；{} 分钟内不再探测",
                tried.join("；"),
                PER_PACK_FAIL_TTL.as_secs() / 60
            ),
        );
    }
    // source 徽标：PAT 通道（access_token 已换为作业令牌，kind 恒为 pat）/ 客户端 token（dt- 等）
    row["source"] = json!(if creds.kind == "pat" || creds.access_token.starts_with("pt-") { "pat" } else { "client_token" });
    row
}

/// 缓存读取（TTL 内命中返回 Some；过期/损坏返回 None）
fn cache_valid(cache: &Value, now_ms: i64) -> bool {
    let fetched = cache.get("fetched_at_ms").and_then(Value::as_i64);
    cache.get("accounts").and_then(Value::as_array).is_some_and(|a| !a.is_empty())
        && fetched.is_some_and(|t| now_ms - t < CACHE_TTL_SECS * 1000)
}

/// 单账号查询时过滤缓存（仅保留目标账号并按剩余行重算 total_balance；
/// 缓存的 accounts[].user_id = 账号池 id，与 fetch_account 产出一致）。
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

/// 当日签到奖励合计（qoder_checkin_results success 事件 reward 列；无签到数据为 0）。
/// 消耗快照差分的充值修正项：签到入账会让余额上升，不修正会把消耗低估成负差。
fn checkin_recharge_today(store: &std::sync::Arc<crate::store::Store>, today: &str) -> f64 {
    crate::store::docs::qoder_checkin_results_load(store)
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("date").and_then(Value::as_str) == Some(today))
        .filter(|r| r.get("status").and_then(Value::as_str) == Some("success"))
        .filter_map(|r| r.get("reward").and_then(Value::as_f64))
        .sum()
}

/// 积分查询主入口（commands 与调度共用）。
/// user_id=None 查全部账号；fresh=true 跳过缓存。stale-on-error：刷新全失败时
/// 回退历史缓存并标记 stale=true（F-59 模式，前端不白屏）。
/// 成功后同日覆盖写积分快照（qoder_credits_history，365 天裁剪）。
pub fn fetch_credits(state: &AppState, user_id: Option<&str>, fresh: bool) -> Result<Value, String> {
    let db = crate::store::db(&state.data_dir);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let cache: Value = db.kv_get("qoder_credits_cache");
    if !fresh && cache_valid(&cache, now_ms) {
        // clone：命中路径不消费缓存，后面 stale-on-error 回退还要用（E0382）
        let mut out = match user_id {
            Some(uid) => filter_cache_by_user(cache.clone(), uid),
            None => cache.clone(),
        };
        // 过滤后无该账号行（缓存未覆盖目标账号）→ 放弃缓存走实时拉取
        if out.get("accounts").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
            out["ok"] = json!(true);
            out["cached"] = json!(true);
            out["stale"] = json!(false);
            return Ok(out);
        }
    }
    let agent = http_agent(30);
    // 账号池读取（审查 P2 同族修复，对齐 run_snapshot_task 口径）：qoder_pool_load
    // 将 rows_all 失败吞为空池，「库损坏/IO 故障」会被误判「账号池为空」→ 快照任务
    // 记当日已跑且快照点缺失。store API 可区分：Ok(空 rows)=真实空池；no such table
    //（表未建，db() 打开时 schema::init 已兜底建表，理论罕见）对齐空池语义；
    // 其余 Err=读取失败 → Err 暂态交调度器冷却重试
    let mut accounts: Vec<Value> = match db.rows_all("qoder_accounts") {
        Ok(rows) => rows.into_iter().map(|(_, data)| data).collect(),
        Err(e) if e.contains("no such table") => Vec::new(),
        Err(e) => return Err(format!("Qoder 账号池读取失败: {e}")),
    };
    if let Some(uid) = user_id {
        accounts.retain(|a| a.get("id").and_then(Value::as_str) == Some(uid));
    }
    if accounts.is_empty() {
        return Ok(json!({ "ok": false, "cached": false, "accounts": [], "message": "账号池为空" }));
    }
    // 跨进程互斥（审查 major，对齐 checkin/refresh scope 模式）：schtasks 调度的
    // qoder-credits-snapshot（CLI 进程）与应用内 UI 拉积分分属两进程，401 自愈
    // 并发 ensure_fresh 同一账号会以同一 refresh_token 刷新——服务端一次性轮换下
    // 败方 4xx 误标 needs_relogin 丢 refresh_token。抢锁失败幂等回退缓存（wait 0
    // 不阻塞 UI 线程；锁创建失败=机制不可用，同样走回退并落日志区分根因）
    let (_cross, lock_fail) =
        qoder_common::CrossProcLock::try_acquire(&state.data_dir, "credits", 0);
    if _cross.is_none() {
        let reason = lock_fail.as_ref().map(|f| f.describe()).unwrap_or_default();
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("[qoder-credits] 跨进程锁未获取（{reason}），本轮回退缓存"),
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
            return Ok(out);
        }
        return Ok(json!({
            "ok": false, "cached": false, "stale": false,
            // 锁忙标记（审查 P2）：run_snapshot_task 据此区分「锁忙空手而归」与真实
            // 失败/空池——该场景必须返 Err 暂态让调度器冷却重试，否则快照点永久缺失；
            // UI 消费方忽略本字段（多字段对前端无影响）
            "lock_busy": true,
            "accounts": [], "total_balance": 0.0,
            "message": "另一进程正在同步积分，请稍后重试",
        }));
    }
    let rows: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let aid = a.get("id").and_then(Value::as_str).unwrap_or("");
            let creds = qoder_common::effective_creds(state, aid);
            let row = fetch_account(&agent, a, &creds, &state.data_dir);
            // 401 自愈：令牌失效时强制刷新一次并重试（lazy_hours=MAX 恒走刷新；
            // PAT 通道 is_pat||has_pat 恒覆盖有备份的凭证）。刷新失败/令牌未变则
            // 保留原失败行，不二次重试（对齐 F-09 禁二次刷新）。
            // P3：判定改用结构化 http_status（fetch_account 错误行携带），
            // message.contains("401") 文本匹配保留兜底（容异常路径无字段）
            let is_401 = row.get("http_status").and_then(Value::as_i64) == Some(401)
                || row
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.contains("401"));
            let (creds, mut row) =
                if row.get("ok").and_then(Value::as_bool) != Some(true) && is_401 {
                    let (new_creds, refreshed, _) =
                        qoder_common::ensure_fresh(state, &agent, aid, i64::MAX);
                    if refreshed && new_creds.access_token != creds.access_token {
                        // 401 自愈成功：回写池过期时间/登录态（原自愈路径只刷新不回写，
                        // 池内 token_expires_at 仍是旧值，到期看板会误报「已过期」）
                        qoder_common::sync_pool_expiry(state, aid, &new_creds);
                        let retry_row = fetch_account(&agent, a, &new_creds, &state.data_dir);
                        (new_creds, retry_row)
                    } else {
                        (creds, row)
                    }
                } else {
                    (creds, row)
                };
            // 套餐档位回填（余额刷新顺带拉 /api/v2/user/plan）。门控决策在
            // qoder_common::need_plan_fetch（含单测）：行成功且池内无套餐/存量值
            // 待归一时才发起查询，已归一账号零额外 HTTP 往返
            let row_ok = row.get("ok").and_then(Value::as_bool) == Some(true);
            let stored_plan = a.get("plan").and_then(Value::as_str).unwrap_or("").to_string();
            row["plan_tier"] = json!(if qoder_common::need_plan_fetch(row_ok, &stored_plan) {
                qoder_common::fetch_plan(&agent, &creds).0.unwrap_or_default()
            } else {
                stored_plan
            });
            row
        })
        .collect();
    let ok_count = rows.iter().filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true)).count();
    let total_balance = rows
        .iter()
        .filter_map(|r| r.get("total").and_then(Value::as_f64))
        .sum::<f64>();
    if ok_count == 0 {
        // stale-on-error（F-59）：全失败回退缓存（无论是否过期），标记 stale；
        // 单账号查询先过滤缓存行，过滤后为空则视为无可用历史缓存
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
            return Ok(out);
        }
        return Ok(json!({
            "ok": false, "cached": false, "stale": false,
            "accounts": rows, "total_balance": 0.0,
            "message": "全部账号查询失败且无历史缓存",
        }));
    }
    // 池回写（QoderAccount.credits_balance/credits_fetched_at 为 Overview 余额展示数据源，
    // 原实现只写快照/缓存不回写池，Overview 永远显示空余额）：
    // I09：直操原始 JSON 保留未知字段（不可走 with_pool_mut），持池锁防并发整池覆盖丢写
    let _guard = state.qoder_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = crate::store::docs::qoder_pool_load(&db);
    let mut changed = false;
    if let Some(accounts) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for a in accounts.iter_mut() {
            let Some(aid) = a.get("id").and_then(Value::as_str).map(str::to_string) else { continue };
            let Some(row) = rows.iter().find(|r| {
                r.get("user_id").and_then(Value::as_str) == Some(aid.as_str())
                    && r.get("ok").and_then(Value::as_bool) == Some(true)
            }) else {
                continue;
            };
            a["credits_balance"] = row.get("total").cloned().unwrap_or(Value::Null);
            a["credits_fetched_at"] = json!(fs_utils::now_iso());
            // 套餐档位回填（v3.7.x）：行内 plan_tier 非空且与池内不同才更新（幂等，
            // 顺带把存量账号空套餐补齐、旧值如 "Pro Trial" 归一为展示名）
            if let Some(tier) = row
                .get("plan_tier")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                if a.get("plan").and_then(Value::as_str) != Some(tier) {
                    a["plan"] = json!(tier);
                }
            }
            changed = true;
        }
    }
    if changed {
        // 回写失败落日志（审查 2026-10-05，sync_pool_expiry 同款）：余额/套餐回写
        // 丢失仅缓存滞后，但静默丢错无法排查
        if let Err(e) = crate::store::docs::qoder_pool_save(&db, &pool) {
            fs_utils::app_log(&state.data_dir, &format!("[qoder-credits] 池回写失败: {e}"));
        }
    }
    // 审查 P3：入口 now_ms 在逐账号网络拉取前取得（多账号可耗数分钟），快照 ts
    // 与缓存 fetched_at_ms 沿用入口值会让拉取耗时「吃掉」缓存 TTL（10 分钟 TTL
    // 实际仅剩 7 分钟）；此处影子重取，让快照与缓存 TTL 按完成时刻计
    let now_ms = chrono::Utc::now().timestamp_millis();
    // 快照落库（同日覆盖 + 365 天裁剪；失败不阻塞返回）。
    // 仅「全量查询且全部成功」落快照：单账号 rows 会覆盖全量快照并污染缓存（过滤失效）；
    // 部分失败时 total_balance 偏低，落快照会污染差分基准且同日覆盖抹掉当日正确快照
    if user_id.is_none() && ok_count == rows.len() {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        // 当日获得（签到奖励合计）与当日消耗（快照差分推导，口径对齐 Buddy usage_fallback）：
        // consumed = 前一快照日余额 − 当日余额 + 当日签到奖励；负值（套餐重置/资源包到账）记 0；
        // 首个快照无前日基准记 null。无签到奖励日 earned 记 null（趋势线留空不画 0）。
        let earned = checkin_recharge_today(&db, &today);
        let prev_snap = crate::store::docs::qoder_credits_history_load(&db)
            .iter()
            .filter(|s| s.get("date").and_then(Value::as_str).is_some_and(|d| d < today.as_str()))
            .max_by(|a, b| {
                let ka = a.get("date").and_then(Value::as_str).unwrap_or("");
                let kb = b.get("date").and_then(Value::as_str).unwrap_or("");
                ka.cmp(kb)
            })
            .cloned();
        let prev_total = prev_snap.as_ref().and_then(|s| s.get("total_balance").and_then(Value::as_f64));
        let prev_n = prev_snap.as_ref().and_then(|s| s.get("accounts").and_then(Value::as_array).map(|a| a.len()));
        // 账号数与上一快照不一致（期间增删账号）：差分口径不可比，consumed 记 null
        // （增账号差分会被钳 0、删账号余额下降会被误计为消耗）
        let consumed = if prev_n.is_some_and(|n| n != rows.len()) {
            None
        } else {
            prev_total.map(|prev| (prev - total_balance + earned).max(0.0))
        };
        let snap = json!({
            "date": today,
            "ts": now_ms,
            "total_balance": total_balance,
            "consumed": consumed,
            "earned": if earned > 0.0 { json!(earned) } else { Value::Null },
            "accounts": rows.iter()
                .map(|r| json!({"user_id": r["user_id"], "total": r["total"]}))
                .collect::<Vec<_>>(),
        });
        // 落库失败不能静默（审查修复，对齐 710-712 qoder_pool_save 先例）：写失败
        // （磁盘满/IO 错误）时 fetch 照常 ok，调度器记当日已跑，快照点永久丢失
        //（consumed 差分链断一环）——至少落日志可感知排查
        if let Err(e) = crate::store::docs::qoder_credits_history_upsert(&db, &snap) {
            crate::fs_utils::app_log(
                &state.data_dir,
                &format!("[qoder] 积分快照落库失败({today}): {e}"),
            );
        }
    }
    let out = json!({
        "ok": true, "cached": false, "stale": false,
        "accounts": rows,
        "total_balance": total_balance,
        "fetched_at_ms": now_ms,
    });
    // 缓存写入门禁对齐快照（381 行）：仅「全部成功」落缓存（审查 M-3）。
    // 原条件 ok_count>0：部分失败的行（fail 明细）进缓存后，TTL 10 分钟内命中路径
    // 返回污染结果（失败账号余额显示为 null/旧值且 cached=true 掩盖暂态错误）；
    // 部分失败不写缓存，下次查询自然重拉（暂态失败自愈）
    if user_id.is_none() && ok_count == rows.len() {
        let _ = db.kv_set("qoder_credits_cache", &out);
    }
    Ok(out)
}

/// 每日快照任务（调度器/CLI 共用；fresh 拉取全部账号 + 快照落库，空池自然空转）
/// 审查 P2：原实现永不返回 Err（fetch_credits 全路径 Ok），当日全部账号失败时
/// （无缓存 → 空结果；有缓存 → stale 回退行内全 ok、failed 不可见）快照缺失且
/// 调度器不冷却重试——当日快照点位不可后补，consumed 差分链断一环。现按行失败
/// 性质分流（对齐 qoder_refresh 口径）：
/// - 暂态失败（网络/5xx/结构异常；含 stale 回退整轮）→ Err 交调度器 30 分钟
///   冷却重试（同日 UPSERT 覆盖，重试成功即补落当日快照）；
/// - 仅永久失败（无凭证/4xx，401 自愈已试过）→ Ok 停止重试（重试无解，
///   全天 48 次 tick 徒劳 + 误报通知），落日志提示人工处理。
pub fn run_snapshot_task(state: &AppState) -> Result<Value, String> {
    // 账号池读取（审查 P2 同族修复）：qoder_pool_load 将 rows_all 失败吞为空池，
    // 「库损坏/IO 故障」会误走 n==0 空转 Ok 被调度器记当日已跑，快照点永久缺失。
    // 区分：Ok(空 rows)=真实空池；no such table（表未建，db() 打开时已建表，理论
    // 罕见）对齐空池语义；其余 Err=读取失败返 Err 暂态
    let n = match crate::store::db(&state.data_dir).rows_all("qoder_accounts") {
        Ok(rows) => rows.len(),
        Err(e) if e.contains("no such table") => 0,
        Err(e) => return Err(format!("Qoder 账号池读取失败: {e}")),
    };
    if n == 0 {
        return Ok(json!({ "ok": true, "skipped": "无 Qoder 账号" }));
    }
    let parsed = fetch_credits(state, None, true)?;
    // 锁忙且无历史缓存（审查 P2）：fetch_credits 此路返回 ok:false + 空 accounts 的
    // Ok——failed 为空 → transient=0 → 原实现落到尾部 Ok(ok:false)，调度器 mark_run
    // 固化当日已跑，快照点永久缺失。lock_busy 标记区分「锁忙」与真实失败/空池：
    // 返 Err 暂态交调度器 30 分钟冷却重试（同日 UPSERT 覆盖，重试成功即补落当日
    // 快照）。真实空池已在上方 n==0 分支 Ok 跳过，不受影响
    if parsed.get("lock_busy").and_then(Value::as_bool) == Some(true) {
        return Err("积分快照跨进程锁忙（另一进程正在同步积分），本轮未执行，30 分钟后重试".into());
    }
    // stale-on-error 缺口（审查修复）：fresh=true 下 cached=true 仅此一路——全部账号
    // 本轮拉取失败、返回的是历史缓存行（行内全 ok=true，failed 为空），快照未落且
    // 失败性质不可见。按暂态处理交调度器退避冷却重试，不能按「完成」记账
    //（否则 mark_run 固化当日已跑，当日快照静默丢失且无重试）
    if parsed.get("cached").and_then(Value::as_bool) == Some(true) {
        return Err(
            "积分快照本轮全部账号拉取失败（已回退历史缓存），未落当日快照，稍后自动重试".into(),
        );
    }
    let ok = parsed.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let rows = parsed
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let failed: Vec<&Value> = rows
        .iter()
        .filter(|r| r.get("ok").and_then(Value::as_bool) != Some(true))
        .collect();
    if failed.is_empty() && ok {
        return Ok(json!({ "ok": true, "accounts": n }));
    }
    // 行级失败性质：http_status ∈ 4xx（401 自愈已试过）或 source=none（无凭证）为
    // 永久；其余（0 网络不可达 / 5xx / 非 JSON / 结构未识别）为暂态
    let is_permanent = |r: &Value| {
        r.get("source").and_then(Value::as_str) == Some("none")
            || r
                .get("http_status")
                .and_then(Value::as_i64)
                .is_some_and(|s| (400..500).contains(&s))
    };
    let permanent = failed.iter().filter(|r| is_permanent(r)).count();
    let transient = failed.len() - permanent;
    if transient > 0 {
        return Err(format!(
            "积分快照拉取暂态失败 {transient}/{}（永久 {permanent}），未落当日快照，稍后自动重试",
            rows.len()
        ));
    }
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!("[qoder] 积分快照：{permanent} 个账号凭证永久失效，当日快照未落（需重新登录/更新 PAT）"),
    );
    Ok(json!({ "ok": false, "accounts": n, "needs_relogin": permanent }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// R-7 抓包样本（2026-09-27 实测，claim 第二活动后 addOnQuota 出现）
    const CAPTURED_SAMPLE: &str = r#"{"displayMode":"qoder","qoderUsage":{"userId":"01a0dff8-cc47-7b64-bd6d-d9cb2aea6792","userType":"personal_professional_trial","usageType":"credits","totalUsagePercentage":0,"isQuotaExceeded":false,"expiresAt":1791673619906,"upgradeUrl":"https://qoder.com/pricing?client=qoder","userQuota":{"total":300,"used":0,"remaining":300,"percentage":0,"unit":"credits"},"addOnQuota":{"total":100,"used":0,"remaining":100,"percentage":0,"unit":"credits","detailUrl":"https://qoder.com/account/usage"},"isPlanQuotaProrated":false}}"#;

    #[test]
    fn parse_usage_handles_captured_r7_structure() {
        let b: Value = serde_json::from_str(CAPTURED_SAMPLE).unwrap();
        let p = parse_usage(&b);
        assert_eq!(p.plan, Some(300.0));
        assert_eq!(p.plan_used, Some(0.0));
        assert_eq!(p.addon, Some(100.0));
        assert_eq!(p.addon_used, Some(0.0));
        assert_eq!(p.total, Some(400.0));
        // 到期日断言用 ms_to_date 同函数推导（审查 L-时区：写死日期在非 UTC+8
        // 时区的机器上会因本地化转换而翻转失败；此处验证解析链路而非具体日期）
        let expected_date = ms_to_date(1791673619906);
        assert_eq!(p.plan_expires_at, expected_date);
        assert_eq!(p.packages.len(), 1);
        assert_eq!(p.packages[0]["source"], json!("addon"));
        assert_eq!(p.packages[0]["expire_at"], json!(expected_date));
    }

    #[test]
    fn parse_usage_without_addon_quota() {
        // claim 前 addOnQuota 缺省：plan 300 / addon None / total 300
        let b = json!({"displayMode":"qoder","qoderUsage":{"userQuota":{"total":300,"used":0,"remaining":300}}});
        let p = parse_usage(&b);
        assert_eq!(p.plan, Some(300.0));
        assert_eq!(p.plan_used, Some(0.0));
        assert_eq!(p.addon, None);
        assert_eq!(p.addon_used, None);
        assert_eq!(p.total, Some(300.0));
    }

    #[test]
    fn parse_usage_captures_used_for_progress() {
        // 订阅周期内已消耗（used）必须捕获：看板「订阅版本的资源」进度 = used / (remaining+used)
        let b = json!({"qoderUsage":{
            "userQuota":{"total":300,"used":120,"remaining":180},
            "addOnQuota":{"total":100,"used":40,"remaining":60}
        }});
        let p = parse_usage(&b);
        assert_eq!(p.plan, Some(180.0));
        assert_eq!(p.plan_used, Some(120.0));
        assert_eq!(p.addon, Some(60.0));
        assert_eq!(p.addon_used, Some(40.0));
        assert_eq!(p.total, Some(240.0));
    }

    #[test]
    fn parse_usage_missing_remaining_derives_from_total_minus_used() {
        // 宽容兜底：remaining 缺失时 total-used 推导（对齐原 quota_remaining 口径）
        let b = json!({"qoderUsage":{"userQuota":{"total":300,"used":50}}});
        let p = parse_usage(&b);
        assert_eq!(p.plan, Some(250.0));
        assert_eq!(p.plan_used, Some(50.0));
    }

    #[test]
    fn parse_usage_unrecognized_returns_all_none() {
        let p = parse_usage(&json!({"foo": "bar"}));
        assert!(p.plan.is_none() && p.addon.is_none() && p.total.is_none() && p.packages.is_empty());
    }

    #[test]
    fn deep_balance_excludes_total_key() {
        assert_eq!(deep_balance_dig(&json!({"totalRemaining": 5}), 0), None);
        assert_eq!(deep_balance_dig(&json!({"remainingCredits": 5}), 0), Some(5.0));
    }

    #[test]
    fn cache_valid_requires_accounts_and_ttl() {
        let now = 1_000_000_000_000i64;
        let ok_cache = json!({"fetched_at_ms": now - 1000, "accounts": [{"user_id": "a"}]});
        assert!(cache_valid(&ok_cache, now));
        let expired = json!({"fetched_at_ms": now - CACHE_TTL_SECS * 1000 - 1, "accounts": [{"user_id": "a"}]});
        assert!(!cache_valid(&expired, now));
        let empty = json!({"fetched_at_ms": now, "accounts": []});
        assert!(!cache_valid(&empty, now));
    }

    // ── big_model_credits 逐包明细（R-11 抓包 2026-10-04）───────────────────

    /// 抓包样本（官网 account/usage 页实测）：PLAN 300（expires_at=0 随订阅周期，
    /// 已用 19）+ 5 个 RESOURCE_PACKAGE_SOURCE_BONUS 各 100；total_quota 为 6 条合并视图
    const BIG_MODEL_SAMPLE: &str = r#"{
        "user_id": "01a0dff8-cc47-7b64-bd6d-d9cb2aea6792",
        "quota_key": "big_model_credits",
        "status": "active",
        "plan_quota": {
            "quota_summary": {"used_value": 19, "limit_value": 300, "remaining_value": 281, "unit": "credits"},
            "quota_detail": [{"id": "p1", "limit_value": 300, "used_value": 19, "remaining_value": 281, "unit": "credits", "is_active": true, "expires_at": 0, "source": "PLAN", "status": "ACTIVE"}]
        },
        "resource_package_quota": {
            "quota_summary": {"used_value": 0, "limit_value": 500, "remaining_value": 500, "unit": "credits"},
            "quota_detail": [{"id": "b1", "limit_value": 100, "used_value": 0, "remaining_value": 100, "expires_at": 1793060431732, "source": "RESOURCE_PACKAGE_SOURCE_BONUS", "status": "ACTIVE"}]
        },
        "dedicated_resource_package_quota": {
            "quota_summary": {"used_value": 0, "limit_value": 0, "remaining_value": 0, "unit": "credits"},
            "quota_detail": null
        },
        "total_quota": {
            "quota_summary": {"used_value": 19, "limit_value": 800, "remaining_value": 781, "unit": "credits"},
            "quota_detail": [
                {"id": "p1", "limit_value": 300, "used_value": 19, "remaining_value": 281, "is_active": true, "expires_at": 0, "source": "PLAN", "status": "ACTIVE"},
                {"id": "b5", "limit_value": 100, "used_value": 0, "remaining_value": 100, "is_active": true, "expires_at": 1793535788250, "source": "RESOURCE_PACKAGE_SOURCE_BONUS", "status": "ACTIVE"},
                {"id": "b4", "limit_value": 100, "used_value": 0, "remaining_value": 100, "is_active": true, "expires_at": 1793350958633, "source": "RESOURCE_PACKAGE_SOURCE_BONUS", "status": "ACTIVE"},
                {"id": "b3", "limit_value": 100, "used_value": 0, "remaining_value": 100, "is_active": true, "expires_at": 1793274555259, "source": "RESOURCE_PACKAGE_SOURCE_BONUS", "status": "ACTIVE"},
                {"id": "b2", "limit_value": 100, "used_value": 0, "remaining_value": 100, "is_active": true, "expires_at": 1793190775403, "source": "RESOURCE_PACKAGE_SOURCE_BONUS", "status": "ACTIVE"},
                {"id": "b1", "limit_value": 100, "used_value": 0, "remaining_value": 100, "is_active": true, "expires_at": 1793060431732, "source": "RESOURCE_PACKAGE_SOURCE_BONUS", "status": "ACTIVE"}
            ]
        },
        "lastResetAt": 1790464019910,
        "nextResetAt": 1791673619906
    }"#;

    #[test]
    fn parse_big_model_captured_sample() {
        let b: Value = serde_json::from_str(BIG_MODEL_SAMPLE).unwrap();
        let plan_cycle = ms_to_date(1791673619906);
        let p = parse_big_model(&b, &plan_cycle);
        // 汇总：total 全量剩余 / plan 订阅配额 / addon 个人资源包聚合
        assert_eq!(p.total, Some(781.0));
        assert_eq!(p.plan, Some(281.0));
        assert_eq!(p.plan_used, Some(19.0));
        assert_eq!(p.addon, Some(500.0));
        assert_eq!(p.addon_used, Some(0.0));
        assert_eq!(p.next_reset_ms, Some(1791673619906));
        // 逐包明细：total_quota 合并视图 6 条（1 PLAN + 5 bonus）
        assert_eq!(p.packages.len(), 6);
        // PLAN：expires_at=0 → 订阅周期到期日兜底
        assert_eq!(p.packages[0]["source"], json!("plan"));
        assert_eq!(p.packages[0]["amount"], json!(281.0));
        assert_eq!(p.packages[0]["total"], json!(300.0));
        assert_eq!(p.packages[0]["expire_at"], json!(plan_cycle));
        // 个人资源包：ms → 日期 + source 映射 bonus（到期日断言用同函数推导，防时区翻转）
        assert_eq!(p.packages[1]["source"], json!("bonus"));
        assert_eq!(p.packages[1]["amount"], json!(100.0));
        assert_eq!(p.packages[1]["total"], json!(100.0));
        assert_eq!(p.packages[1]["expire_at"], json!(ms_to_date(1793535788250)));
    }

    #[test]
    fn parse_big_model_filters_inactive_and_empty_detail() {
        // 已用完（remaining 0）/ status 非 ACTIVE / is_active=false 的包不进明细；
        // 未知 source 保留小写；quota_detail=null（dedicated 族）不 panic
        let b = json!({
            "total_quota": {
                "quota_summary": {"remaining_value": 50.0, "used_value": 10.0, "limit_value": 60.0},
                "quota_detail": [
                    {"remaining_value": 0.0, "limit_value": 100.0, "expires_at": 1793535788250i64, "source": "PLAN", "status": "ACTIVE", "is_active": true},
                    {"remaining_value": 50.0, "limit_value": 100.0, "expires_at": 1793535788250i64, "source": "RESOURCE_PACKAGE_SOURCE_BONUS", "status": "EXPIRED", "is_active": true},
                    {"remaining_value": 50.0, "limit_value": 100.0, "expires_at": 1793535788250i64, "source": "RESOURCE_PACKAGE_SOURCE_BONUS", "status": "ACTIVE", "is_active": false},
                    {"remaining_value": 50.0, "limit_value": 100.0, "expires_at": 1793535788250i64, "source": "DEDICATED_SOURCE_X", "status": "ACTIVE", "is_active": true}
                ]
            },
            "dedicated_resource_package_quota": {"quota_summary": {"remaining_value": 0.0, "used_value": 0.0, "limit_value": 0.0}, "quota_detail": null}
        });
        let p = parse_big_model(&b, "");
        assert_eq!(p.total, Some(50.0));
        assert_eq!(p.packages.len(), 1);
        assert_eq!(p.packages[0]["source"], json!("dedicated_source_x"));
        // 非 0 expires_at 直接 ms → 日期（与抓包样本同函数推导，防时区翻转）
        assert_eq!(p.packages[0]["expire_at"], json!(ms_to_date(1793535788250)));
    }

    #[test]
    fn parse_big_model_empty_and_missing_detail_degrade() {
        // 空响应 → 全 None 不 panic（fetch 侧静默回退 sash 聚合口径）
        assert_eq!(parse_big_model(&json!({}), "2026-12-05"), BigModelParsed::default());
        // total_quota 缺 quota_detail（null）→ 无明细
        let b = json!({"total_quota": {"quota_summary": {"remaining_value": 30.0}, "quota_detail": null}});
        let p = parse_big_model(&b, "");
        assert_eq!(p.total, Some(30.0));
        assert!(p.packages.is_empty());
    }

    #[test]
    fn parse_usage_dedicated_packages_per_pack() {
        // 专属/组织资源包逐包（sash usage 原生携带，Work 客户端 Upt 解析器同款字段）：
        // ACTIVE + 剩余 > 0 进明细；已用完 / 非激活过滤；snake/camel 键宽容；
        // name 缺失走 display_labels 兜底；expires_at 缺失/0 → 订阅周期到期日兜底；
        // addon 聚合条目并存
        let b = json!({
            "qoderUsage": {
                "userQuota": {"total": 300.0, "used": 19.0},
                "addOnQuota": {"total": 500.0, "used": 0.0},
                "expiresAt": 1791673619906i64,
                "dedicated_resource_packages": [
                    // 标准 ACTIVE 包（snake 键 + name + 枚举全前缀 status）
                    {"total": 1000.0, "used": 200.0, "remaining": 800.0,
                     "expires_at": 1793535788250i64, "status": "QUOTA_DETAIL_STATUS_ACTIVE", "name": "企业专属包A"},
                    // 已用完 → 过滤
                    {"total": 100.0, "used": 100.0, "remaining": 0.0,
                     "expires_at": 1793535788250i64, "status": "QUOTA_DETAIL_STATUS_ACTIVE", "name": "用完包"},
                    // status 非 ACTIVE → 过滤
                    {"total": 100.0, "used": 10.0, "remaining": 90.0,
                     "expires_at": 1793535788250i64, "status": "QUOTA_DETAIL_STATUS_SUSPENDED", "name": "停用包"},
                    // remaining 缺失 → total-used 推导；camel expiresAt；无 name → display_labels 兜底
                    {"total": 50.0, "used": 20.0, "expiresAt": 1793190775403i64,
                     "status": "QUOTA_DETAIL_STATUS_ACTIVE", "display_labels": [{"dimension": "pkg", "value": "企业专属包B"}]},
                    // expires_at=0 → 订阅周期到期日兜底
                    {"total": 30.0, "remaining": 30.0, "expires_at": 0,
                     "status": "QUOTA_DETAIL_STATUS_ACTIVE", "name": "周期包"}
                ]
            }
        });
        let p = parse_usage(&b);
        assert_eq!(p.packages.len(), 4); // addon 聚合 1 + dedicated 3
        assert_eq!(p.packages[0]["source"], json!("addon"));
        // 标准 ACTIVE 包
        assert_eq!(p.packages[1]["source"], json!("dedicated"));
        assert_eq!(p.packages[1]["amount"], json!(800.0));
        assert_eq!(p.packages[1]["total"], json!(1000.0));
        assert_eq!(p.packages[1]["expire_at"], json!(ms_to_date(1793535788250)));
        assert_eq!(p.packages[1]["name"], json!("企业专属包A"));
        // remaining 缺失 → total-used 推导；display_labels 兜底名；camel expiresAt
        assert_eq!(p.packages[2]["amount"], json!(30.0));
        assert_eq!(p.packages[2]["name"], json!("企业专属包B"));
        assert_eq!(p.packages[2]["expire_at"], json!(ms_to_date(1793190775403)));
        // expires_at=0 → 订阅周期（qoderUsage.expiresAt）兜底
        assert_eq!(p.packages[3]["expire_at"], json!(ms_to_date(1791673619906)));
    }

    #[test]
    fn parse_usage_dedicated_packages_absent_and_malformed() {
        // 无 dedicated 数组 → 仅 addon 条目（行为与旧版一致）
        let b = json!({"qoderUsage": {"userQuota": {"total": 10.0}, "addOnQuota": {"total": 5.0}}});
        let p = parse_usage(&b);
        assert_eq!(p.packages.len(), 1);
        assert_eq!(p.packages[0]["source"], json!("addon"));
        // dedicated 非数组（null/对象）→ 不 panic，仅 addon
        let b2 = json!({"qoderUsage": {"addOnQuota": {"total": 5.0}, "dedicated_resource_packages": null}});
        assert_eq!(parse_usage(&b2).packages.len(), 1);
    }
}
