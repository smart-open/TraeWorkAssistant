//! WorkBuddy 一键签到（F-15/F-16/F-55）+ 成长中心（T2.5/F-17）+ token 惰性续期（F-09）。
//! 原 src-python/workbuddy_checkin.py 的 Rust 移植，逐函数对齐：
//! - 签到：checkin_status（新路径回退旧路径）→ checkin_do（域名双探测 + 已签容错 + 奖励宽容解析）
//!   → 余额差值兜底（F-17，仅差值>0 采信）→ 401 刷新一次重试（F-09，禁二次刷新）
//! - 成长：travel（status→claim→config→depart）/ lottery（chances→draw 循环上限 20）/
//!   tasks（has_reward 过滤 → accept）各步独立容错，401 同款刷新重试
//! - NDJSON 事件契约（wb-checkin-progress 管线，前端逐行 JSON.parse）：
//!   start {type,total[,mode]} / account {user_id,name,status,message[,reward],index} /
//!   growth {type:"growth",...,index} / done {type:"done"[,mode],ok,already,failed}
//! - 结果 90 天滚动存储；刷新成功回写账号池过期时间（调度/到期日历数据源）
//! 红线：全程零 token 输出（凭证不入日志/事件）。

use serde_json::{json, Map, Value};

use crate::fs_utils;
use crate::state::AppState;

use super::http_agent;
use super::wb_common::{self, Creds};

/// 盲盒抽取循环上限（防接口异常时死循环；正常 balance 会归零）
const LOTTERY_MAX_DRAWS: usize = 20;

/// 签到轮次参数（对齐 python --uid/--skip-checked/--skip-expired/--lazy-hours）
#[derive(Clone, Default)]
pub struct CheckinOpts {
    pub uids: Vec<String>,
    /// 对齐 python --skip-checked 形参（python 侧同样声明未消费：
    /// 「今日已签」实际由 checkin_status 预检兜底返回 already）；保留字段维持调用方契约
    #[allow(dead_code)]
    pub skip_checked: bool,
    pub skip_expired: bool,
    pub lazy_hours: i64,
}

impl CheckinOpts {
    /// 每日计划任务默认（--json-stream --skip-checked，lazy 24h）
    pub fn daily() -> Self {
        Self {
            skip_checked: true,
            lazy_hours: 24,
            ..Default::default()
        }
    }
}

/// 成长中心开关（对齐 python --growth-travel/--growth-lottery/--growth-tasks）
#[derive(Clone, Copy, Default)]
pub struct GrowthOpts {
    pub travel: bool,
    pub lottery: bool,
    pub tasks: bool,
}

/// 区域端点表（对齐 python _urls_for；base 不带尾斜杠）
struct Urls {
    base: &'static str,
    checkin_status: String,
    checkin_status_old: String,
    checkin_do: String,
    travel_status: String,
    travel_claim: String,
    travel_config: String,
    travel_depart: String,
    lottery_chances: String,
    lottery_draw: String,
    tasks: String,
    tasks_accept: String,
    energy: String,
    streak: String,
}

fn urls_for(base: &'static str) -> Urls {
    let b = base;
    let g = format!("{b}/v2/activity/growth");
    Urls {
        base,
        checkin_status: format!("{b}/v2/billing/meter/checkin-activity-status"),
        checkin_status_old: format!("{b}/v2/billing/meter/checkin-status"),
        checkin_do: format!("{b}/v2/billing/meter/daily-checkin"),
        travel_status: format!("{g}/buddy/travel/status"),
        travel_claim: format!("{g}/buddy/travel/claim"),
        travel_config: format!("{g}/buddy/travel/config"),
        travel_depart: format!("{g}/buddy/travel/depart"),
        lottery_chances: format!("{g}/lottery/chances"),
        lottery_draw: format!("{g}/lottery/draw"),
        tasks: format!("{g}/tasks"),
        tasks_accept: format!("{g}/tasks/accept"),
        energy: format!("{g}/energy"),
        streak: format!("{g}/streak"),
    }
}

impl Urls {
    /// 备用域名端点表（§2.2 域名双探测）：主域名网络不可达时切换重试一次
    fn alt(&self) -> Urls {
        let bases = wb_common::billing_bases("");
        let alt = if bases[0] == self.base { bases[1] } else { bases[0] };
        urls_for(alt)
    }
}

// ── 解析辅助（对齐 python _num_or_none / _scalar_of / bool(v)）─────────────

/// 奖励/余额数值归一：数字或纯数字字符串 → f64，其余 None（不递归，对齐 _num_or_none）
fn num_or_none(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// python bool() 语义（0/""/null/false → false，其余 true）
fn py_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map_or(true, |f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(_)) => true,
    }
}

/// 嵌套对象按常用数值键归一（energy/streak 兼容 {current:..} 等对象返回，
/// 修复前端「连签 [object Object] 天」展示）
fn scalar_of(v: &Value) -> Option<f64> {
    if let Some(m) = v.as_object() {
        for k in ["current", "days", "count", "value", "num", "total", "streak", "energy"] {
            if let Some(x) = m.get(k) {
                if !x.is_null() {
                    return scalar_of(x);
                }
            }
        }
        return None;
    }
    num_or_none(Some(v))
}

/// 递归深挖奖励数额：键名含奖励语义子串的数值字段（宽容解析，对齐 doubao_quota
/// deep dig 模式）。刻意不含 balance/total/remaining 语义——避免把账户余额误当本次奖励。
fn deep_reward_dig(v: &Value, depth: usize) -> Option<f64> {
    if depth > 8 {
        return None;
    }
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                let kl = k.to_ascii_lowercase();
                let looks_reward = ["reward", "bonus", "earned", "add_integral", "add_credits", "addcredit", "integral_add", "gain", "obtain", "prize"]
                    .iter()
                    .any(|s| kl.contains(s));
                if looks_reward {
                    if let Some(n) = num_or_none(Some(val)) {
                        return Some(n);
                    }
                }
            }
            for val in m.values() {
                if val.is_object() || val.is_array() {
                    if let Some(n) = deep_reward_dig(val, depth + 1) {
                        return Some(n);
                    }
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(|x| deep_reward_dig(x, depth + 1)),
        _ => None,
    }
}

/// 递归找余额数值字段：键名含 remaining/balance/capacity（大小写不敏感，排除 total）。
/// 用于 billing summary 结构未知时的余额差值兜底（pre/post 同结构，首命中稳定）。
fn deep_balance_dig(v: &Value, depth: usize) -> Option<f64> {
    if depth > 8 {
        return None;
    }
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                let kl = k.to_ascii_lowercase();
                if (kl.contains("remaining") || kl.contains("balance") || kl.contains("capacity"))
                    && !kl.contains("total")
                {
                    if let Some(n) = num_or_none(Some(val)) {
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

/// 奖励数额一律以接口返回为准，不硬编码（F-17 红线）；无奖励回退 raw 摘要
fn reward_text(body: Option<&Value>, raw: &str) -> String {
    let r = body.and_then(|b| fs_utils::dig(b, &["reward", "credits", "points", "amount", "value"]));
    match r {
        Some(v) if !v.is_null() => format!("+{v}"),
        _ => {
            let t: String = raw.chars().take(60).collect();
            if t.is_empty() { "ok".to_string() } else { t }
        }
    }
}

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

/// 取首个真值字符串（对齐 python `a or b or c`：空串视为假）
fn first_str(parts: &[Option<String>]) -> String {
    parts
        .iter()
        .flatten()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default()
}

// ── 签到端点（对齐 python checkin_status / checkin_do / fetch_balance）─────

/// 查询今日是否已签：新路径回退旧路径；返回 (today_checked_in, status_ok)
fn checkin_status(
    agent: &ureq::Agent,
    headers: &[(String, String)],
    urls: &Urls,
) -> (Option<bool>, bool) {
    for url in [&urls.checkin_status, &urls.checkin_status_old] {
        let (status, body) = wb_common::post_json(agent, url, headers, &json!({}));
        if status == 401 {
            return (None, false);
        }
        if status == 200 {
            if let Some(b) = body.filter(|b| b.is_object()) {
                let mut v = fs_utils::dig(&b, &["today_checked_in", "todayCheckedIn", "checked_in", "checkedIn"]);
                if v.is_none() {
                    // 部分响应以 0/1 表达
                    v = fs_utils::dig(&b, &["checked", "is_checked"]);
                }
                if let Some(v) = v {
                    return (Some(py_truthy(Some(v))), true);
                }
                return (None, true); // 200 但字段缺失：视为未知但不失败（执行时容错）
            }
        }
        // 其它状态码尝试旧路径
    }
    (None, false)
}

/// 执行签到。返回 (kind, message, reward)：success / already / fail（auth 由调用方刷新重试）
/// 签到执行。返回 (kind, message, reward, dbg_keys)：
/// success / already / fail（auth 由调用方刷新重试）；dbg_keys = 成功但奖励未识别时
/// 的响应键路径（仅键名不含值，供 app.log 诊断校准，见 process_account）
fn checkin_do(
    agent: &ureq::Agent,
    headers: &[(String, String)],
    urls: &Urls,
) -> (String, String, Option<f64>, Option<String>) {
    let (mut status, mut body, mut raw) =
        wb_common::post_json_raw(agent, &urls.checkin_do, headers, &json!({}));
    if status == 0 {
        // 域名双探测（§2.2）：主域名网络不可达 → 备用域名重试一次
        let alt = urls.alt().checkin_do;
        if alt != urls.checkin_do {
            let r = wb_common::post_json_raw(agent, &alt, headers, &json!({}));
            status = r.0;
            body = r.1;
            raw = r.2;
        }
    }
    if status == 401 {
        return ("auth".into(), "登录态失效（401）".into(), None, None);
    }
    let code: Option<i64> = body
        .as_ref()
        .and_then(|b| b.get("code"))
        .and_then(Value::as_i64)
        .or_else(|| body.as_ref().and_then(|b| fs_utils::dig(b, &["code"])).and_then(Value::as_i64));
    let message = body
        .as_ref()
        .and_then(|b| fs_utils::dig(b, &["message", "msg"]))
        .map(|v| s_of(Some(v)));
    if status == 0 {
        let head: String = raw.chars().take(120).collect();
        return ("fail".into(), format!("网络不可达: {head}"), None, None);
    }
    if (200..=201).contains(&status) || code.is_some_and(|c| c == 0 || c == 200) {
        // 奖励数额以接口返回为准，不硬编码（F-17）；精确键未命中走递归深挖，
        // 仍无则兜底走签到前后余额差值（process_account）
        let reward = body.as_ref().and_then(|b| {
            num_or_none(fs_utils::dig(
                b,
                &[
                    "reward", "credits", "points", "amount", "integral", "score", "bonus",
                    "reward_amount", "add_integral", "addCredits", "earned",
                ],
            ))
            .or_else(|| deep_reward_dig(b, 0))
        });
        // 奖励未识别时收集响应键路径（不含值）供诊断校准
        let dbg_keys = if reward.is_none() {
            body.as_ref().map(|b| {
                let mut paths = Vec::new();
                collect_key_paths(b, "", 0, &mut paths);
                paths.join(" | ")
            })
        } else {
            None
        };
        return ("success".into(), "签到成功".into(), reward, dbg_keys);
    }
    // 已签容错（F-15）：code:10001 / message 含「已签到」/「repeat」
    let msg_low = message.as_deref().unwrap_or("").to_ascii_lowercase();
    if code == Some(10001)
        || ["已签到", "repeat", "already"].iter().any(|k| msg_low.contains(k))
    {
        return ("already".into(), "今日已签到".into(), None, None);
    }
    let head: String = raw.chars().take(120).collect();
    let shown = message
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| if head.is_empty() { format!("HTTP {status}") } else { head });
    (
        "fail".into(),
        format!("{shown}（code={}）", code.map(|c| c.to_string()).unwrap_or_else(|| "None".into())),
        None,
        None,
    )
}

/// 收集 JSON 键路径（深度/数量受限；只采集键名不采集值——诊断用，避免敏感信息入日志）
fn collect_key_paths(v: &Value, prefix: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 6 || out.len() >= 40 {
        return;
    }
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                out.push(p.clone());
                collect_key_paths(val, &p, depth + 1, out);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate().take(3) {
                collect_key_paths(x, &format!("{prefix}[{i}]"), depth + 1, out);
            }
        }
        _ => {}
    }
}

/// 查询当前通用积分余额（get-user-resource-summary，与积分页同口径）。
/// 网络失败/解析失败返回 None——仅用于签到获得积分差值兜底，不阻塞签到。
fn fetch_balance(agent: &ureq::Agent, headers: &[(String, String)], base: &str) -> Option<f64> {
    let path = "/billing/meter/get-user-resource-summary";
    let (mut status, mut body) = wb_common::post_json(agent, &format!("{base}{path}"), headers, &json!({}));
    if status == 0 {
        let alt = if base == wb_common::BILLING_BASE_CN {
            wb_common::BILLING_BASE_GLOBAL
        } else {
            wb_common::BILLING_BASE_CN
        };
        if alt != base {
            let r = wb_common::post_json(agent, &format!("{alt}{path}"), headers, &json!({}));
            status = r.0;
            body = r.1;
        }
    }
    if status != 200 {
        return None;
    }
    let b = body?;
    num_or_none(fs_utils::dig(
        &b,
        &["RemainingCapacity", "remaining", "TotalRemaining", "Balance", "balance"],
    ))
    .or_else(|| deep_balance_dig(&b, 0))
}

/// WB credits 域余额（WorkBuddy 积分账本）：billing meter 是通用积分账本，
/// 签到奖励入 WB 积分——billing 差值恒 0 时用 credits 差值兜底
fn fetch_wb_credits_balance(state: &AppState, aid: &str) -> Option<f64> {
    let parsed = crate::tasks::wb_credits::fetch_credits(state, Some(aid), true).ok()?;
    parsed
        .get("accounts")?
        .as_array()?
        .iter()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(aid))
        .and_then(|a| a.get("balance"))
        .and_then(Value::as_f64)
}

// ── 账号池回写 / 结果存储 ──────────────────────────────────────────────────

/// 刷新成功后回写账号池 token 过期时间（调度/到期日历数据源）
fn sync_pool_expiry(state: &AppState, aid: &str, creds: &Creds) {
    // SQLite 化（P3）：wb_accounts 表
    let mut pool: Value = crate::store::docs::wb_pool_load(&crate::store::db(&state.data_dir));
    let mut changed = false;
    if let Some(accounts) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for a in accounts.iter_mut() {
            if a.get("id").and_then(Value::as_str) == Some(aid) {
                if let Some(exp) = creds.expires_at_ms {
                    a["access_token_expires_at"] = json!(exp.div_euclid(1000));
                }
                if let Some(rexp) = creds.refresh_expires_at_ms {
                    a["refresh_token_expires_at"] = json!(rexp.div_euclid(1000));
                }
                a["needs_relogin"] = json!(false);
                a["relogin_reason"] = json!("");
                changed = true;
            }
        }
    }
    if changed {
        let _ = crate::store::docs::wb_pool_save(&crate::store::db(&state.data_dir), &pool);
    }
}

/// 签到结果 90 天滚动存储（趋势/日志数据源，F-55/F-22；SQLite 化 P3：wb_checkin_results 表）
fn append_results(state: &AppState, events: &[Value]) {
    let store = crate::store::db(&state.data_dir);
    let mut data: Value = crate::store::docs::wb_checkin_results_load(&store);
    let results: Vec<Value> = data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(90))
        .format("%Y-%m-%d")
        .to_string();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut kept: Vec<Value> = results
        .into_iter()
        .filter(|r| r.get("date").and_then(Value::as_str).unwrap_or("") >= cutoff.as_str())
        .collect();
    for ev in events {
        let mut rec = json!({
            "date": today,
            "time": fs_utils::now_ts(),
            "user_id": ev.get("user_id").cloned().unwrap_or_default(),
            "name": ev.get("name").cloned().unwrap_or_default(),
            "status": ev.get("status").cloned().unwrap_or_default(),
            "message": ev.get("message").cloned().unwrap_or_default(),
        });
        if let Some(r) = ev.get("reward").filter(|r| !r.is_null()) {
            rec["reward"] = r.clone();
        }
        kept.push(rec);
    }
    data["results"] = json!(kept);
    let _ = crate::store::docs::wb_checkin_results_save(&store, &data);
}

// ── 签到主流程 ─────────────────────────────────────────────────────────────

/// 处理单账号签到（含 401 刷新一次重试）。返回 account 事件（不含 index）
fn process_account(state: &AppState, agent: &ureq::Agent, acct: &Value, opts: &CheckinOpts) -> Value {
    let aid = s_of(acct.get("id"));
    let name = first_str(&[
        acct.get("nickname").and_then(Value::as_str).map(str::to_string),
        Some(s_of(acct.get("uid")).chars().take(8).collect()),
        Some(aid.clone()),
    ]);
    let base_ev = json!({"user_id": aid, "name": name});
    if opts.skip_expired && py_truthy(acct.get("needs_relogin")) {
        return json!({ "user_id": aid, "name": base_ev["name"], "status": "fail", "message": "需重新登录，已跳过" });
    }
    let (creds, refreshed, note) = wb_common::ensure_fresh(state, agent, acct, opts.lazy_hours);
    if creds.access_token.is_empty() {
        return json!({ "user_id": aid, "name": base_ev["name"], "status": "fail", "message": format!("无可用凭证（{note}）") });
    }
    if refreshed {
        sync_pool_expiry(state, &aid, &creds);
    }
    let base = wb_common::region_billing_base(&creds.domain);
    let urls = urls_for(base);
    let mut headers = wb_common::build_auth_headers(&creds, false);
    let (checked, ok) = checkin_status(agent, &headers, &urls);
    if ok && checked == Some(true) {
        return json!({ "user_id": aid, "name": base_ev["name"], "status": "already", "message": "今日已签到" });
    }
    // 签到前余额（获得积分差值兜底数据源；两级恒取：①billing meter 通用积分
    // ②WB credits 域 WorkBuddy 积分——查询失败不阻塞签到。恒取两级的原因：
    // billing 前值可能成功但余额恰好不变（签到加的是 WB 积分账本），差值恒 0 时
    // 仍需 WB credits 差值兜底）
    let pre_balance = fetch_balance(agent, &headers, base);
    let pre_wb_credits = fetch_wb_credits_balance(state, &aid);
    let (mut kind, mut message, mut reward, mut dbg_keys) = checkin_do(agent, &headers, &urls);
    if kind == "auth" {
        // 401：刷新一次仅重试失败分支（禁止二次刷新，F-09）
        match wb_common::refresh_token_once(agent, &creds) {
            Some(new) => {
                let _ = wb_common::save_token_store(state, &aid, &new);
                sync_pool_expiry(state, &aid, &new);
                headers = wb_common::build_auth_headers(&new, false);
                let r = checkin_do(agent, &headers, &urls);
                kind = r.0;
                message = r.1;
                reward = r.2;
                dbg_keys = r.3;
            }
            None => {
                kind = "fail".into();
                message = "登录态失效且刷新失败，需重新登录".into();
                reward = None;
            }
        }
    }
    // 获得积分兜底（F-17）：接口未返回奖励数额时用签到前后余额差值；
    // 仅差值>0 才采信（防并发扣减/查询时点差造成负值误报）。
    // 两级数据源都尝试：①billing meter（通用积分）②WB credits（WorkBuddy 积分）
    if kind == "success" && reward.is_none() {
        if let Some(pre) = pre_balance {
            if let Some(post) = fetch_balance(agent, &headers, base) {
                if post > pre {
                    reward = Some(((post - pre) * 100.0).round() / 100.0);
                }
            }
        }
        if reward.is_none() {
            if let Some(pre) = pre_wb_credits {
                if let Some(post) = fetch_wb_credits_balance(state, &aid) {
                    if post > pre {
                        reward = Some(((post - pre) * 100.0).round() / 100.0);
                    }
                }
            }
        }
    }
    let status_txt = match kind.as_str() {
        "success" => "success",
        "already" => "already",
        _ => "fail",
    };
    // 「已签」回填：签到奖励当日只发一次，今日早前成功记录若已捕获奖励则同步展示
    //（用户语义：无论成功还是已签，获取积分列都应显示当日所得）
    if status_txt == "already" && reward.is_none() {
        let results: Value = crate::store::docs::wb_checkin_results_load(&crate::store::db(&state.data_dir));
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        reward = results
            .get("results")
            .and_then(Value::as_array)
            .and_then(|arr| {
                arr.iter()
                    .find(|r| {
                        r.get("date").and_then(Value::as_str) == Some(today.as_str())
                            && r.get("user_id").and_then(Value::as_str) == Some(aid.as_str())
                            && r.get("status").and_then(Value::as_str) == Some("success")
                            && r.get("reward").and_then(Value::as_f64).is_some()
                    })
                    .and_then(|r| r.get("reward"))
                    .and_then(Value::as_f64)
            });
    }
    // 诊断（脱敏红线：只输出键路径不含值）：成功但三层提取（精确键/深挖/双差值）
    // 均未命中时记录响应键路径，便于按真实结构校准奖励键
    if kind == "success" && reward.is_none() {
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "wb 签到成功但奖励数额未识别（接口回显/递归深挖/双余额差值均未命中）: {aid}，响应键路径: {}",
                dbg_keys.as_deref().unwrap_or("（响应非 JSON）")
            ),
        );
    }
    let mut ev = json!({ "user_id": aid, "name": base_ev["name"], "status": status_txt, "message": message });
    if let Some(r) = reward {
        ev["reward"] = json!(r);
    }
    if kind == "success" && note == "refreshed" {
        ev["message"] = json!(format!("{}（凭证已续期）", ev["message"].as_str().unwrap_or("")));
    }
    ev
}

/// 签到整轮：逐账号串行处理，事件经 emit 回调逐条输出（NDJSON 管线复用）。
/// 返回 done 事件（ok/already/failed 计数），供启动补签/托盘静默路径直接消费。
pub fn run_checkin_round(state: &AppState, opts: &CheckinOpts, emit: &mut dyn FnMut(&Value)) -> Value {
    let agent = http_agent(30);
    let pool: Value = crate::store::docs::wb_pool_load(&crate::store::db(&state.data_dir));
    let mut accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !opts.uids.is_empty() {
        accounts.retain(|a| opts.uids.iter().any(|u| s_of(a.get("id")) == *u));
    }
    emit(&json!({"type": "start", "total": accounts.len()}));

    let mut events: Vec<Value> = Vec::new();
    for (i, acct) in accounts.iter().enumerate() {
        // 单账号失败不中断整轮（python try/except 语义；Rust 直调无异常路径）
        let mut ev = process_account(state, &agent, acct, opts);
        ev["index"] = json!(i + 1);
        emit(&ev);
        events.push(ev);
    }

    let ok = events.iter().filter(|e| e["status"] == "success").count();
    let already = events.iter().filter(|e| e["status"] == "already").count();
    let failed = events.len() - ok - already;
    let done = json!({"type": "done", "ok": ok, "already": already, "failed": failed});
    emit(&done);
    append_results(state, &events);
    done
}

// ── 成长中心自动化（T2.5/F-17）────────────────────────────────────────────

/// 401 刷新一次重试封装（对齐 python with_retry）：f 以 headers 为参，
/// 返回 (kind, msg)；kind=auth 时刷新凭证后重试一次。
fn with_retry<F>(state: &AppState, agent: &ureq::Agent, acct_id: &str, creds: &Creds, f: F) -> (String, String)
where
    F: Fn(&[(String, String)]) -> (String, String),
{
    let headers = wb_common::build_auth_headers(creds, false);
    let (kind, msg) = f(&headers);
    if kind == "auth" {
        if let Some(new) = wb_common::refresh_token_once(agent, creds) {
            let _ = wb_common::save_token_store(state, acct_id, &new);
            sync_pool_expiry(state, acct_id, &new);
            let h2 = wb_common::build_auth_headers(&new, false);
            return f(&h2);
        }
        return ("fail".into(), "登录态失效且刷新失败".into());
    }
    (kind, msg)
}

/// Buddy 旅行：status → arrived 则 claim → config → depart（各步独立容错）
fn growth_travel(agent: &ureq::Agent, headers: &[(String, String)], urls: &Urls) -> (String, String) {
    let (status, body, raw) = wb_common::get_json(agent, &urls.travel_status, headers);
    if status == 401 {
        return ("auth".into(), "登录态失效（401）".into());
    }
    if status != 200 || !body.as_ref().is_some_and(|b| b.is_object()) {
        let head: String = raw.chars().take(60).collect();
        let msg = if status != 0 {
            format!("travel/status 不可用（HTTP {status}）")
        } else {
            format!("travel/status 不可达: {head}")
        };
        return ("fail".into(), msg);
    }
    let b = body.as_ref().unwrap();
    let arrived = py_truthy(fs_utils::dig(b, &["arrived", "is_arrived", "has_arrived"]));
    let record_id = fs_utils::dig(b, &["record_id", "recordId"]).cloned();
    if !arrived {
        // 未到达：报告在途状态即可
        let dest = fs_utils::dig(b, &["destination", "name", "target"]).map(|v| s_of(Some(v)));
        let suffix = dest
            .filter(|d| !d.is_empty())
            .map(|d| format!("（{d}）"))
            .unwrap_or_default();
        return ("skip".into(), format!("旅行在途{suffix}"));
    }
    // 领奖
    let claim_body = match &record_id {
        Some(r) => json!({"record_id": r}),
        None => json!({}),
    };
    let (st1, b1, r1) = wb_common::post_json_raw(agent, &urls.travel_claim, headers, &claim_body);
    if st1 == 401 {
        return ("auth".into(), "登录态失效（401）".into());
    }
    let claim_txt = if (200..=201).contains(&st1) {
        reward_text(b1.as_ref(), &r1)
    } else {
        format!("claim 失败（HTTP {st1}）")
    };
    // 查目的地配置并出发（config 失败不阻塞 depart）
    let dest = wb_common::get_json(agent, &urls.travel_config, headers)
        .1
        .filter(|b2| b2.is_object())
        .and_then(|b2| fs_utils::dig(&b2, &["destination", "name", "target"]).cloned());
    let depart_body = match &dest {
        Some(d) => json!({"destination": d}),
        None => json!({}),
    };
    let (st3, _b3, _r3) = wb_common::post_json_raw(agent, &urls.travel_depart, headers, &depart_body);
    let dest_txt = dest
        .as_ref()
        .map(|d| format!("（{}）", value_display(d)))
        .unwrap_or_default();
    if (200..=201).contains(&st3) {
        return ("ok".into(), format!("领奖{claim_txt}，已出发{dest_txt}"));
    }
    ("ok".into(), format!("领奖{claim_txt}；depart 失败（HTTP {st3}）"))
}

/// 展示用值文本（数字去尾零、字符串原样）
fn value_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 盲盒：chances(balance>0) → draw 循环（可开关）
fn growth_lottery(agent: &ureq::Agent, headers: &[(String, String)], urls: &Urls) -> (String, String) {
    let (status, body, raw) = wb_common::get_json(agent, &urls.lottery_chances, headers);
    if status == 401 {
        return ("auth".into(), "登录态失效（401）".into());
    }
    if status != 200 || !body.as_ref().is_some_and(|b| b.is_object()) {
        let head: String = raw.chars().take(60).collect();
        let msg = if status != 0 {
            format!("lottery/chances 不可用（HTTP {status}）")
        } else {
            format!("lottery/chances 不可达: {head}")
        };
        return ("fail".into(), msg);
    }
    let b = body.as_ref().unwrap();
    // 对齐 python int(balance)：数字（float 截断）或整数字符串；小数字符串视为无效
    let balance = fs_utils::dig(b, &["balance", "chances", "count", "remain"]).and_then(|v| match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    });
    let Some(balance) = balance.filter(|n| *n > 0) else {
        return ("skip".into(), "无可用次数".into());
    };
    let mut balance = balance;
    let mut draws = 0usize;
    let mut rewards: Vec<String> = vec![];
    while balance > 0 && draws < LOTTERY_MAX_DRAWS {
        let (st, b, _r) = wb_common::post_json_raw(agent, &urls.lottery_draw, headers, &json!({}));
        if st == 401 {
            return ("auth".into(), format!("登录态失效（401，已抽 {draws} 次）"));
        }
        if !(200..=201).contains(&st) {
            break;
        }
        draws += 1;
        if let Some(rw) = b.as_ref().and_then(|bb| fs_utils::dig(bb, &["reward", "credits", "points", "amount"])) {
            if !rw.is_null() {
                rewards.push(value_display(rw));
            }
        }
        balance -= 1;
    }
    if draws == 0 {
        return ("fail".into(), "draw 不可用".into());
    }
    let joined = if rewards.is_empty() {
        "见响应".to_string()
    } else {
        rewards.join("/")
    };
    ("ok".into(), format!("抽取 {draws} 次（奖励 {joined}）"))
}

/// 任务领奖：tasks → 过滤 has_reward && 未领取 → accept {task_code}（可开关）
fn growth_tasks(agent: &ureq::Agent, headers: &[(String, String)], urls: &Urls) -> (String, String) {
    let (status, body, raw) = wb_common::get_json(agent, &urls.tasks, headers);
    if status == 401 {
        return ("auth".into(), "登录态失效（401）".into());
    }
    if status != 200 || !body.as_ref().is_some_and(|b| b.is_object()) {
        let head: String = raw.chars().take(60).collect();
        let msg = if status != 0 {
            format!("tasks 不可用（HTTP {status}）")
        } else {
            format!("tasks 不可达: {head}")
        };
        return ("fail".into(), msg);
    }
    let b = body.as_ref().unwrap();
    let tasks: Vec<&Value> = fs_utils::dig(b, &["tasks", "list", "records"])
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let mut claimed = 0usize;
    let mut skipped = 0usize;
    for t in tasks {
        let Some(obj) = t.as_object() else { continue };
        if !py_truthy(obj.get("has_reward")) && fs_utils::dig(t, &["hasReward"]).is_none() {
            continue;
        }
        // accept_status：顶层缺省时 dig 兜底；空值归一 ""（对齐 python `or ""` + str()）
        let accept_status = obj
            .get("accept_status")
            .filter(|v| !v.is_null())
            .or_else(|| fs_utils::dig(t, &["acceptStatus", "status"]))
            .map(value_display)
            .unwrap_or_default();
        if ["1", "2", "claimed", "accepted", "已领取", "true", "True"].contains(&accept_status.as_str()) {
            skipped += 1;
            continue;
        }
        let code = obj
            .get("task_code")
            .filter(|v| !v.is_null())
            .or_else(|| obj.get("taskCode").filter(|v| !v.is_null()))
            .or_else(|| obj.get("code").filter(|v| !v.is_null()))
            .cloned();
        let accept_body = match &code {
            Some(c) => json!({"task_code": c}),
            None => json!({}),
        };
        let (st, _bb, _r) = wb_common::post_json_raw(agent, &urls.tasks_accept, headers, &accept_body);
        if st == 401 {
            return ("auth".into(), format!("登录态失效（401，已领 {claimed} 项）"));
        }
        if (200..=201).contains(&st) {
            claimed += 1;
        }
    }
    if claimed == 0 {
        return ("skip".into(), format!("无可领奖励（已领 {skipped} 项）"));
    }
    ("ok".into(), format!("领取 {claimed} 项任务奖励"))
}

/// 能量与连签天数（页面附注展示；对象响应归一为标量，防 [object Object]）
fn growth_info(agent: &ureq::Agent, headers: &[(String, String)], urls: &Urls) -> Vec<(&'static str, f64)> {
    let mut info = vec![];
    let (st, b, _r) = wb_common::get_json(agent, &urls.energy, headers);
    if st == 200 {
        if let Some(v) = b
            .filter(|bb| bb.is_object())
            .and_then(|bb| fs_utils::dig(&bb, &["energy", "value", "balance", "num"]).cloned())
            .and_then(|v| scalar_of(&v))
        {
            info.push(("energy", v));
        }
    }
    let (st, b, _r) = wb_common::get_json(agent, &urls.streak, headers);
    if st == 200 {
        if let Some(v) = b
            .filter(|bb| bb.is_object())
            .and_then(|bb| fs_utils::dig(&bb, &["streak", "days", "continuous_days", "count"]).cloned())
            .and_then(|v| scalar_of(&v))
        {
            info.push(("streak", v));
        }
    }
    info
}

/// 单账号成长中心链式执行（旅行→盲盒→任务，各步独立容错；401 刷新一次重试）
fn process_account_growth(state: &AppState, agent: &ureq::Agent, acct: &Value, flags: &GrowthOpts) -> Value {
    let aid = s_of(acct.get("id"));
    let name = first_str(&[
        acct.get("nickname").and_then(Value::as_str).map(str::to_string),
        Some(s_of(acct.get("uid")).chars().take(8).collect()),
        Some(aid.clone()),
    ]);
    let base_ev = json!({"type": "growth", "user_id": aid, "name": name});
    let (creds, _refreshed, note) = wb_common::ensure_fresh(state, agent, acct, 24);
    if creds.access_token.is_empty() {
        return json!({ "type": "growth", "user_id": aid, "name": base_ev["name"], "status": "fail", "message": format!("无可用凭证（{note}）") });
    }
    let urls = urls_for(wb_common::region_billing_base(&creds.domain));

    let mut result = Map::new();
    if flags.travel {
        let (kind, msg) = with_retry(state, agent, &aid, &creds, |h| growth_travel(agent, h, &urls));
        result.insert("travel".into(), json!(if kind == "fail" { "fail" } else { msg.as_str() }));
    }
    if flags.lottery {
        let (kind, msg) = with_retry(state, agent, &aid, &creds, |h| growth_lottery(agent, h, &urls));
        result.insert("lottery".into(), json!(if kind == "fail" { "fail" } else { msg.as_str() }));
    }
    if flags.tasks {
        let (kind, msg) = with_retry(state, agent, &aid, &creds, |h| growth_tasks(agent, h, &urls));
        result.insert("tasks".into(), json!(if kind == "fail" { "fail" } else { msg.as_str() }));
    }
    // 能量/连签（不带刷新重试，纯展示）
    let headers = wb_common::build_auth_headers(&creds, false);
    for (k, v) in growth_info(agent, &headers, &urls) {
        result.insert(k.to_string(), json!(v));
    }

    let enabled = ["travel", "lottery", "tasks"]
        .iter()
        .filter(|k| result.contains_key(**k))
        .count();
    let fails = ["travel", "lottery", "tasks"]
        .iter()
        .filter(|k| result.get(**k).and_then(Value::as_str) == Some("fail"))
        .count();
    let status = if fails > 0 && fails == enabled { "fail" } else { "ok" };
    let mut ev = json!({ "type": "growth", "user_id": aid, "name": base_ev["name"], "status": status });
    for (k, v) in result {
        ev[k.as_str()] = v;
    }
    ev
}

/// 成长中心整轮：NDJSON 输出（wb-checkin-progress 管线复用）
pub fn run_growth_round(state: &AppState, flags: &GrowthOpts, uids: &[String], emit: &mut dyn FnMut(&Value)) {
    let agent = http_agent(30);
    let pool: Value = crate::store::docs::wb_pool_load(&crate::store::db(&state.data_dir));
    let mut accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !uids.is_empty() {
        accounts.retain(|a| uids.iter().any(|u| s_of(a.get("id")) == *u));
    }
    emit(&json!({"type": "start", "total": accounts.len(), "mode": "growth"}));
    for (i, acct) in accounts.iter().enumerate() {
        let mut ev = process_account_growth(state, &agent, acct, flags);
        ev["index"] = json!(i + 1);
        emit(&ev);
    }
    emit(&json!({"type": "done", "mode": "growth"}));
}

// ── token 每周兜底续期（F-09，schtasks / --task-run wb-renew）──────────────

/// 每周兜底：对全部账号执行惰性刷新（lazy_hours 传 0 = 每天无条件刷新全部，F-55）。
/// 返回末行 JSON 摘要（零 token 输出）。
pub fn run_renew_only(state: &AppState, lazy_hours: i64) -> Value {
    let agent = http_agent(30);
    let pool: Value = crate::store::docs::wb_pool_load(&crate::store::db(&state.data_dir));
    let accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut items: Vec<Value> = vec![];
    for acct in &accounts {
        let aid = s_of(acct.get("id"));
        if aid.is_empty() {
            continue;
        }
        let (creds, refreshed, note) = wb_common::ensure_fresh(state, &agent, acct, lazy_hours);
        if refreshed {
            sync_pool_expiry(state, &aid, &creds);
        }
        let mut item = json!({"user_id": aid, "refreshed": refreshed, "note": note});
        if creds.access_token.is_empty() {
            item["note"] = json!("no_credential");
        }
        items.push(item);
    }
    json!({"mode": "renew", "finished_at": fs_utils::now_ts(), "accounts": items})
}

#[cfg(test)]
mod wb_checkin_tests {
    use super::*;

    #[test]
    fn scalar_of_unwraps_common_keys() {
        // {current:..} 等对象返回归一为标量（修复前端 [object Object] 展示）
        assert_eq!(scalar_of(&json!({"current": 5})), Some(5.0));
        assert_eq!(scalar_of(&json!({"streak": {"days": "3"}})), Some(3.0));
        assert_eq!(scalar_of(&json!(7)), Some(7.0));
        assert_eq!(scalar_of(&json!("2.5")), Some(2.5));
        assert_eq!(scalar_of(&json!({"other": 1})), None);
        assert_eq!(scalar_of(&json!({"current": null})), None);
    }

    #[test]
    fn num_or_none_rejects_bools_and_garbage() {
        assert_eq!(num_or_none(Some(&json!(true))), None);
        assert_eq!(num_or_none(Some(&json!(10))), Some(10.0));
        assert_eq!(num_or_none(Some(&json!("12.5"))), Some(12.5));
        assert_eq!(num_or_none(Some(&json!("abc"))), None);
        assert_eq!(num_or_none(Some(&json!({"a": 1}))), None);
    }

    #[test]
    fn py_truthy_matches_python_bool_semantics() {
        assert!(!py_truthy(Some(&json!(0))));
        assert!(py_truthy(Some(&json!(1))));
        assert!(!py_truthy(Some(&json!(""))));
        assert!(py_truthy(Some(&json!("false")))); // python bool("false") == True
        assert!(!py_truthy(Some(&json!(null))));
        assert!(py_truthy(Some(&json!({"a": 1}))));
    }

    #[test]
    fn urls_cover_all_endpoints_and_alt_region() {
        let u = urls_for(wb_common::BILLING_BASE_CN);
        assert!(u.checkin_do.starts_with(wb_common::BILLING_BASE_CN));
        assert!(u.travel_status.contains("/v2/activity/growth/buddy/travel/status"));
        let alt = u.alt();
        assert_eq!(alt.base, wb_common::BILLING_BASE_GLOBAL);
        assert_ne!(alt.checkin_do, u.checkin_do);
    }

    #[test]
    fn reward_text_prefers_interface_values() {
        assert_eq!(reward_text(Some(&json!({"reward": 10})), ""), "+10");
        assert_eq!(reward_text(Some(&json!({"reward": 1.5})), ""), "+1.5");
        assert_eq!(reward_text(Some(&json!({})), "oops"), "oops");
        assert_eq!(reward_text(Some(&json!({})), ""), "ok");
    }
}
