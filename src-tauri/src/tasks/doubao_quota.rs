//! 豆包会员额度查询（原 src-python/doubao_quota.py 的 Rust 移植）。
//! 端点：settings.doubao_quota_url（豆包会员接口无公开文档，须用户经 MITM 抓包固化后填入）。
//! 解析：优先按代理实测结构精确解析（quota/summary，2026-09 实测），失败回退宽容
//! deep dig（全树下钻，无信封键——与 wb_common/fs_utils 的信封语义 dig 不同，勿混用）。
//! 两条路径：
//! - 单账号（应用内 doubao_quota_fetch）：读池凭证 → 查询 → 解析结果交调用方回写缓存
//! - 批量（CLI `--task-run doubao-quota`，schtasks 每日巡检）：遍历池内账号 → 查询 →
//!   回写账号池缓存 + 追加运维历史 → 汇总（对齐 python --all）
//! 红线：凭证等同密码，仅本地使用；仅查询展示，不做代刷。

use serde_json::{json, Value};

use crate::fs_utils;
use crate::state::AppState;

use super::http_agent;

const DIG_MAX_DEPTH: usize = 10;
const HISTORY_MAX: usize = 400;
const TIMEOUT_SECS: u64 = 15;

const NAME_KEYS: &[&str] = &["name", "title", "item_name", "metric_name", "label"];
const TOTAL_KEYS: &[&str] = &["total", "limit", "quota", "max", "total_count", "total_num"];
const USED_KEYS: &[&str] = &["used", "use", "consume", "used_count"];
const LEFT_KEYS: &[&str] = &["remaining", "left", "remain", "available", "left_count", "rest"];
const LEVEL_KEYS: &[&str] = &[
    "level", "vip_level", "member_level", "grade", "vip_type", "member_type", "plan",
];
const EXPIRE_KEYS: &[&str] = &[
    "expire_time",
    "expire_at",
    "expired_time",
    "due_time",
    "due_date",
    "valid_end_time",
    "end_time",
    "end_date",
    "subscription_expire_time",
];

/// 与浏览器一致 UA（python 版同值）
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";

// ── 宽容解析（deep dig，对齐 python doubao_quota.dig：无信封键、全树下钻）──────

/// 全树递归查找首个命中键。
/// python 语义对齐：① dict 顶层键命中即返回（哪怕值为 null）；② 子递归命中 null 视为
/// 未命中继续找下一个 child（`if hit is not None: return hit`）；③ 数组展开各元素。
fn deep_find<'a>(v: &'a Value, keys: &[&str], depth: usize) -> Option<&'a Value> {
    if depth > DIG_MAX_DEPTH {
        return None;
    }
    match v {
        Value::Object(map) => {
            for k in keys {
                if let Some(hit) = map.get(*k) {
                    return Some(hit);
                }
            }
            for child in map.values() {
                if let Some(hit) = deep_find(child, keys, depth + 1) {
                    if !hit.is_null() {
                        return Some(hit);
                    }
                }
            }
            None
        }
        Value::Array(arr) => arr
            .iter()
            .find_map(|item| deep_find(item, keys, depth + 1).filter(|h| !h.is_null())),
        _ => None,
    }
}

/// dig 命中后规整为可展示标量：bool 排除；数字保留；字符串 strip 截 64。
fn pick_scalar(v: Option<&Value>) -> Option<Value> {
    let v = v?;
    match v {
        Value::Bool(_) => None,
        Value::Number(_) => Some(v.clone()),
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| Value::String(s.chars().take(64).collect()))
        }
        _ => None,
    }
}

fn value_to_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 安全截取前 n 个字符（避免中文串按字节切片 panic）
fn take_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map(|(i, _)| &s[..i]).unwrap_or(s)
}

/// 时间戳/日期串归一为 "YYYY-MM-DD HH:MM"；无法解析返回 None。
fn fmt_ts(v: &Value) -> Option<String> {
    use chrono::TimeZone;
    if v.is_null() {
        return None;
    }
    if let Some(ts) = v.as_f64() {
        let mut ts = ts;
        if ts > 1e12 {
            ts /= 1000.0; // 毫秒
        }
        if 1e8 < ts && ts < 4e10 {
            return chrono::Local
                .timestamp_opt(ts as i64, 0)
                .single()
                .map(|d| d.format("%Y-%m-%d %H:%M").to_string());
        }
        return Some(value_to_str(v));
    }
    let s = match v.as_str() {
        Some(s) => s.trim(),
        None => return Some(value_to_str(v)),
    };
    if s.is_empty() {
        return None;
    }
    let head = take_chars(s, 19);
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%d", "%Y/%m/%d"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(head, fmt) {
            return Some(d.format("%Y-%m-%d %H:%M").to_string());
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(head, fmt) {
            return Some(d.format("%Y-%m-%d 00:00").to_string());
        }
    }
    Some(take_chars(s, 32).to_string())
}

fn fmt_ts_of(v: Option<&Value>) -> Option<String> {
    let v = v?;
    fmt_ts(v)
}

/// 毫秒/秒时间戳 → 本地 datetime（无效 None）。
fn norm_ms_ts(v: &Value) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone;
    let ts = v.as_f64()?;
    let mut t = ts;
    if t > 1e12 {
        t /= 1000.0; // 毫秒
    }
    if !(1e8 < t && t < 4e10) {
        return None;
    }
    chrono::Local.timestamp_opt(t as i64, 0).single()
}

/// 收集数组中「名称键 + 总量键」同存的额度对象（常见 bundling/entitlement 结构）；
/// 命中对象不再向子级递归（对齐 python）。
fn find_quota_items<'a>(v: &'a Value, depth: usize, out: &mut Vec<&'a Value>) {
    if depth > DIG_MAX_DEPTH {
        return;
    }
    match v {
        Value::Object(map) => {
            let has_name = NAME_KEYS.iter().any(|k| map.contains_key(*k));
            let has_total = TOTAL_KEYS.iter().any(|k| map.contains_key(*k));
            if has_name && has_total {
                out.push(v);
            } else {
                for child in map.values() {
                    find_quota_items(child, depth + 1, out);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                find_quota_items(item, depth + 1, out);
            }
        }
        _ => {}
    }
}

const WINDOW_TYPE_NAMES: [(i64, &str); 2] = [(1, "当前时段"), (2, "近 7 天")];

/// 优先按代理实测结构精确解析（quota/summary，2026-09 实测），失败回退宽容 dig。
pub fn parse_quota(resp: &Value) -> Value {
    let mut level: Option<Value> = None;
    let mut expire: Option<String> = None;
    let mut is_gift: Option<bool> = None;
    let mut has_subscription: Option<bool> = None;
    let mut items: Vec<Value> = vec![];
    let mut subscription: Option<Value> = None;

    if let Some(data) = resp.get("data").filter(|d| d.is_object()) {
        // ① 订阅信息：套餐名（=会员等级展示）+ 到期时间 + 是否赠送
        if let Some(sub) = data.get("current_subscription").filter(|s| s.is_object()) {
            let display = sub.get("display").filter(|d| d.is_object());
            let short_name = display
                .and_then(|d| d.get("short_name"))
                .and_then(Value::as_str);
            let product_name = display
                .and_then(|d| d.get("product_name"))
                .and_then(Value::as_str);
            let mem_display = data
                .get("membership_display_name")
                .and_then(Value::as_str);
            level = short_name
                .or(product_name)
                .or(mem_display)
                .map(|s| Value::String(s.to_string()));
            expire = fmt_ts_of(sub.get("end_time"));
            if let Some(g) = sub.get("is_gift").filter(|g| g.is_boolean()) {
                is_gift = Some(g.as_bool().unwrap_or(false));
            }
            // has_subscription = status not in (None, 0)（python 语义；0.0 同 0）
            has_subscription = Some(match sub.get("status") {
                None | Some(Value::Null) => false,
                Some(Value::Number(n)) => n.as_f64() != Some(0.0),
                Some(_) => true,
            });
            // 订阅记录（对齐客户端「订阅记录」页：套餐 / 周期 / 起止 / 来源 / 状态）
            let start_ts = sub.get("start_time").and_then(norm_ms_ts);
            let end_ts = sub.get("end_time").and_then(norm_ms_ts);
            let period_days = match (&start_ts, &end_ts) {
                (Some(s), Some(e)) => {
                    let secs = (*e - *s).num_seconds();
                    Some((((secs as f64) / 86400.0).round() as i64).max(1))
                }
                _ => None,
            };
            subscription = Some(json!({
                "name": level,
                "period_days": period_days,
                "start_at": start_ts.map(|d| d.format("%Y-%m-%d").to_string()),
                "expire_at": expire,
                "is_gift": is_gift,
                "active": has_subscription.unwrap_or(false),
            }));
        } else if let Some(h) = data.get("has_active_subscription").filter(|h| h.is_boolean()) {
            has_subscription = Some(h.as_bool().unwrap_or(false));
        }
        if level.is_none() {
            level = data
                .get("membership_display_name")
                .and_then(Value::as_str)
                .map(|s| Value::String(s.to_string()));
        }

        // ② 额度窗口：window_limit_groups[].window_limits[]（window_type 1=当前时段
        //    2=近7天，used_percent=已用百分比，end_time=重置时间）
        if let Some(wls) = data.get("window_limit_section").filter(|w| w.is_object()) {
            for group in wls
                .get("window_limit_groups")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if !group.is_object() {
                    continue;
                }
                let gname = group
                    .get("feature_group_name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                for w in group
                    .get("window_limits")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if !w.is_object() {
                        continue;
                    }
                    let wt = w.get("window_type");
                    let used = w.get("used_percent").and_then(Value::as_f64);
                    let reset_at = w.get("end_time").and_then(norm_ms_ts);
                    if used.is_none() && reset_at.is_none() {
                        continue;
                    }
                    let wt_num = wt.and_then(Value::as_i64);
                    let known = wt_num.and_then(|n| {
                        WINDOW_TYPE_NAMES
                            .iter()
                            .find(|(k, _)| *k == n)
                            .map(|(_, label)| label.to_string())
                    });
                    let name = match known {
                        Some(label) => label,
                        None => {
                            let wt_txt = wt.map(value_to_str).unwrap_or_else(|| "None".into());
                            if gname.is_empty() {
                                format!("窗口{wt_txt}")
                            } else {
                                format!("{gname}·窗口{wt_txt}")
                            }
                        }
                    };
                    let exhausted = w
                        .get("usage_exhausted")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        || used.map(|u| u >= 100.0).unwrap_or(false);
                    items.push(json!({
                        "name": name,
                        "used_percent": used,
                        "exhausted": exhausted,
                        "reset_at": reset_at.map(|d| d.format("%Y-%m-%d %H:%M").to_string()),
                    }));
                }
            }
        }
    }

    // ③ 回退：结构不识别时走宽容 dig（老逻辑，保底其他端点形态）
    if level.is_none() && expire.is_none() && items.is_empty() {
        level = pick_scalar(deep_find(resp, LEVEL_KEYS, 0));
        expire = fmt_ts_of(deep_find(resp, EXPIRE_KEYS, 0));
        let mut objs: Vec<&Value> = vec![];
        find_quota_items(resp, 0, &mut objs);
        for obj in objs.into_iter().take(16) {
            let name = pick_scalar(deep_find(obj, NAME_KEYS, 0));
            let total = pick_scalar(deep_find(obj, TOTAL_KEYS, 0));
            if name.is_none() || total.is_none() {
                continue;
            }
            items.push(json!({
                "name": name.unwrap(),
                "total": total.unwrap(),
                "left": pick_scalar(deep_find(obj, LEFT_KEYS, 0)),
                "used": pick_scalar(deep_find(obj, USED_KEYS, 0)),
            }));
        }
    }

    json!({
        "level": level,
        "expire_at": expire,
        "is_gift": is_gift,
        "has_subscription": has_subscription,
        "subscription": subscription,
        "items": items,
    })
}

// ── 端点查询 ────────────────────────────────────────────────────────────────

/// 调额度端点并解析 → Ok({ok:true, parsed}) / Err(错误信息)。
/// 直连语义由 http_agent 保证（python 版显式绕过系统代理，避免走 MITM 死端口）。
pub fn query_account(
    agent: &ureq::Agent,
    url: &str,
    sid: &str,
    sid_guard: Option<&str>,
) -> Result<Value, String> {
    let mut cookie = format!("sessionid={sid}");
    if let Some(sg) = sid_guard.filter(|s| !s.is_empty()) {
        cookie.push_str(&format!("; sid_guard={sg}"));
    }
    let resp = agent
        .post(url)
        .set("Cookie", &cookie)
        .set("User-Agent", UA)
        .set("Referer", "https://www.doubao.com/")
        .set("Accept", "application/json, text/plain, */*")
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .send_string(&json!({"product_line": "membership"}).to_string());
    let body: Value = match resp {
        Ok(r) => {
            let raw = r.into_string().unwrap_or_default();
            serde_json::from_str(&raw).map_err(|e| format!("响应解析失败: {e}"))?
        }
        Err(ureq::Error::Status(code, _)) => return Err(format!("HTTP {code}")),
        Err(e) => return Err(format!("请求失败: {e}")),
    };
    if !body.is_object() {
        return Err("接口返回异常 code=?".to_string());
    }
    let code_v = body.get("code");
    let ok_code = match code_v {
        None | Some(Value::Null) => true,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(_) => false,
    };
    if !ok_code {
        let code_str = code_v.map(value_to_str).unwrap_or_else(|| "?".to_string());
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
    Ok(json!({"ok": true, "parsed": parse_quota(&body)}))
}

/// 单账号查询（应用内路径）：从池读取凭证 → 查询。
/// 返回 python 单账号 summary 契约：
/// {"ok":true,"http_status":200,"url","user_id","parsed","finished_at","logs":[]}
pub fn fetch_single(state: &AppState, uid: &str, url: &str) -> Result<Value, String> {
    // SQLite 化（P3）：doubao_accounts 表
    let pool: Value =
        serde_json::to_value(crate::commands::doubao::load_pool(state)).unwrap_or(json!({}));
    let acc = pool
        .get("accounts")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid))
        });
    let Some(acc) = acc else {
        return Err(format!("账号 {uid} 不在账号池中"));
    };
    let sid = acc
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let sid_guard = acc
        .get("sid_guard")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    if sid.is_empty() {
        return Err(
            "该账号未录入 sessionid 凭证（开代理自动写入，或账号管理 → 编辑 手动录入）".to_string(),
        );
    }
    let agent = http_agent(TIMEOUT_SECS);
    let parsed = query_account(&agent, url, &sid, sid_guard.as_deref())?;
    Ok(json!({
        "ok": true,
        "http_status": 200,
        "url": url,
        "user_id": uid,
        "parsed": parsed.get("parsed").cloned().unwrap_or(Value::Null),
        "finished_at": fs_utils::now_ts(),
        "logs": [],
    }))
}

/// 解析结果 → 一句话摘要（窗口形态优先，最多 4 条）。
/// 与 commands/doubao.rs update_quota_cache 的展示口径统一归口本函数。
pub fn summarize_parsed(parsed: &Value) -> Option<String> {
    let mut parts: Vec<String> = vec![];
    if let Some(items) = parsed.get("items").and_then(Value::as_array) {
        for it in items.iter().take(4) {
            let name = it
                .get("name")
                .map(value_to_str)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "额度".to_string());
            if let Some(pct) = it.get("used_percent").and_then(Value::as_f64) {
                let exhausted =
                    it.get("exhausted").and_then(Value::as_bool).unwrap_or(false) || pct >= 100.0;
                let mut state_txt = if exhausted {
                    "已用完".to_string()
                } else {
                    format!("已用 {:.0}%", pct)
                };
                if let Some(reset) = it.get("reset_at").and_then(Value::as_str) {
                    let tail: String = reset.chars().skip(5).collect();
                    if !tail.is_empty() {
                        state_txt += &format!("（{tail} 重置）");
                    }
                }
                parts.push(format!("{name} {state_txt}"));
                continue;
            }
            let total = it.get("total").filter(|t| !t.is_null());
            if let Some(total) = total {
                let total_s = value_to_str(total);
                match it.get("left").filter(|l| !l.is_null()).map(value_to_str) {
                    Some(left) => parts.push(format!("{name} {left}/{total_s}")),
                    None => parts.push(format!("{name} 总量 {total_s}")),
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// 追加滚动运维历史（SQLite 化 P3：doubao_health_events 表，HISTORY_MAX 上限裁剪）。
/// 与 commands/doubao.rs append_history_event 同构（CLI 独立进程路径）。
fn append_history(state: &AppState, event: Value) {
    let store = crate::store::db(&state.data_dir);
    let mut events = crate::store::docs::doubao_health_load(&store);
    events.push(event);
    if events.len() > HISTORY_MAX {
        events.drain(0..events.len() - HISTORY_MAX);
    }
    let _ = crate::store::docs::doubao_health_save(&store, &events);
}

/// 批量巡检（`--task-run doubao-quota`，对齐 python --all）：遍历池内有凭证账号 →
/// 查额度 → 回写账号池缓存 + 追加运维历史 → 汇总。
pub fn run_batch(state: &AppState) -> Result<Value, String> {
    // SQLite 化（P3）：doubao_accounts 表
    // 两阶段：先只读快照收集目标，锁外网络巡检，最后短临界区合并回写
    let pool: Value =
        serde_json::to_value(crate::commands::doubao::load_pool(state)).unwrap_or(json!({}));
    let Some(accounts) = pool.get("accounts").and_then(Value::as_array) else {
        return Err("账号池解析失败".to_string());
    };

    // 端点：settings.doubao_quota_url（state.settings() 已对空值回填默认）
    let url = state
        .settings()
        .doubao_quota_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(crate::models::default_doubao_quota_url);

    let agent = http_agent(TIMEOUT_SECS);
    // 先收集可巡检目标（uid + 凭证 + name），避免边遍历边写
    let targets: Vec<(String, String, Option<String>, String)> = accounts
        .iter()
        .filter_map(|acc| {
            let uid = acc.get("user_id").and_then(Value::as_str)?.to_string();
            let sid = acc.get("session_id").and_then(Value::as_str)?.to_string();
            if uid.is_empty() || sid.is_empty() {
                return None;
            }
            let sg = acc.get("sid_guard").and_then(Value::as_str).map(|s| s.to_string());
            let name = acc.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            Some((uid, sid, sg, name))
        })
        .collect();

    let mut ok_n = 0usize;
    let mut fail_n = 0usize;
    let mut exhausted: Vec<Value> = vec![];
    let mut errors: Vec<Value> = vec![];
    // 巡检变更收集（两阶段）：锁外网络完成后短临界区按 uid 合并回最新池，
    // 防旧快照整体替换丢掉巡检期间新入池/被删账号（lost-update）
    let mut quota_updates: Vec<(String, Value, Option<String>, String)> = Vec::new();

    for (uid, sid, sg, name) in targets {
        let now = fs_utils::now_ts();
        match query_account(&agent, &url, &sid, sg.as_deref()) {
            Ok(r) => {
                ok_n += 1;
                let parsed = r.get("parsed").cloned().unwrap_or(Value::Null);
                let summary = summarize_parsed(&parsed);
                quota_updates.push((uid.clone(), parsed.clone(), summary.clone(), now.clone()));
                let windows: Vec<Value> = parsed
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter(|it| {
                                it.get("used_percent").and_then(Value::as_f64).is_some()
                            })
                            .map(|it| {
                                json!({
                                    "name": it.get("name").cloned().unwrap_or(Value::Null),
                                    "used_percent": it.get("used_percent").cloned().unwrap_or(Value::Null),
                                    "reset_at": it.get("reset_at").cloned().unwrap_or(Value::Null),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if windows
                    .iter()
                    .any(|w| {
                        w.get("used_percent")
                            .and_then(Value::as_f64)
                            .map(|p| p >= 100.0)
                            .unwrap_or(false)
                    })
                {
                    let reset_at = windows
                        .iter()
                        .find_map(|w| w.get("reset_at").and_then(Value::as_str).map(|s| s.to_string()));
                    exhausted.push(json!({
                        "user_id": uid,
                        "name": if name.is_empty() { uid.clone() } else { name.clone() },
                        "reset_at": reset_at,
                    }));
                }
                append_history(
                    state,
                    json!({
                        "ts": now, "kind": "quota", "uid": uid, "ok": true,
                        "level": parsed.get("level"), "summary": summary, "windows": windows,
                        "source": "task",
                    }),
                );
            }
            Err(e) => {
                fail_n += 1;
                errors.push(json!({"user_id": uid, "error": e}));
                append_history(
                    state,
                    json!({
                        "ts": fs_utils::now_ts(), "kind": "quota", "uid": uid, "ok": false,
                        "summary": e, "source": "task",
                    }),
                );
            }
        }
    }

    if !quota_updates.is_empty() {
        // 读-改-写互斥：短临界区重读最新池按 uid 合并额度字段——巡检期间被删账号
        // 跳过不复活、新入池账号保留；quota_checked_at 沿用查询时刻。
        // 账号池含全部账号会话凭证（等同密码），事务内整表替换
        let _guard = state.doubao_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut latest: Value =
            serde_json::to_value(crate::commands::doubao::load_pool(state)).unwrap_or(json!({}));
        let merged = merge_quota_updates_into_pool(&mut latest, &quota_updates);
        if merged > 0 {
            let file = serde_json::from_value::<crate::commands::doubao::DoubaoAccountPool>(latest)
                .map_err(|e| e.to_string())?;
            crate::commands::doubao::save_pool(state, &file)?;
        }
    }
    Ok(json!({
        "ok": true, "mode": "all", "url": url,
        "total": ok_n + fail_n, "success": ok_n, "failed": fail_n,
        "exhausted": exhausted, "errors": errors,
        "finished_at": fs_utils::now_ts(),
    }))
}

/// 额度回写合并（纯函数，供两阶段短临界区调用）：将按 uid 收集的额度查询结果
/// 合并进最新池——只更新仍存在账号（quota_level/quota_expire_at/quota_summary/
/// quota_checked_at 四字段），被删账号不复活（返回 0 即无需写盘）、新入池账号
/// 不受影响；字段级 insert 不覆盖并发方更新的其他字段。
fn merge_quota_updates_into_pool(
    latest: &mut Value,
    updates: &[(String, Value, Option<String>, String)],
) -> usize {
    let mut merged = 0usize;
    if let Some(arr) = latest.get_mut("accounts").and_then(Value::as_array_mut) {
        for (uid, parsed, summary, checked_at) in updates {
            let Some(acc) = arr
                .iter_mut()
                .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid.as_str()))
            else {
                continue; // 巡检期间被删除的账号不复活
            };
            let Some(obj) = acc.as_object_mut() else { continue };
            obj.insert(
                "quota_level".into(),
                parsed.get("level").cloned().unwrap_or(Value::Null),
            );
            obj.insert(
                "quota_expire_at".into(),
                parsed.get("expire_at").cloned().unwrap_or(Value::Null),
            );
            obj.insert(
                "quota_summary".into(),
                summary.clone().map(Value::String).unwrap_or(Value::Null),
            );
            obj.insert("quota_checked_at".into(), Value::String(checked_at.clone()));
            merged += 1;
        }
    }
    merged
}

#[cfg(test)]
mod doubao_quota_tests {
    use super::*;

    #[test]
    fn parse_quota_exact_structure() {
        // 2026-09 代理实测 quota/summary 结构
        let resp = json!({
            "code": 0,
            "data": {
                "current_subscription": {
                    "display": {"short_name": "专业版"},
                    "end_time": 1_800_000_000_000i64,
                    "start_time": 1_770_000_000_000i64,
                    "is_gift": false,
                    "status": 1,
                },
                "window_limit_section": {
                    "window_limit_groups": [
                        {
                            "feature_group_name": "图片生成",
                            "window_limits": [
                                {"window_type": 1, "used_percent": 80, "end_time": 1_799_900_000_000i64},
                                {"window_type": 2, "used_percent": 100, "usage_exhausted": true},
                            ]
                        }
                    ]
                }
            }
        });
        let p = parse_quota(&resp);
        assert_eq!(p.get("level").and_then(Value::as_str), Some("专业版"));
        assert!(p.get("expire_at").and_then(Value::as_str).is_some());
        assert_eq!(p.get("is_gift").and_then(Value::as_bool), Some(false));
        assert_eq!(p.get("has_subscription").and_then(Value::as_bool), Some(true));
        let sub = p.get("subscription").unwrap();
        // (1.8e12 - 1.77e12) ms = 30,000,000 s ≈ 347.2 天 → round 347
        assert_eq!(sub.get("period_days").and_then(Value::as_i64), Some(347));
        assert_eq!(sub.get("active").and_then(Value::as_bool), Some(true));
        let items = p.get("items").and_then(Value::as_array).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].get("name").and_then(Value::as_str), Some("当前时段"));
        assert_eq!(items[0].get("used_percent").and_then(Value::as_f64), Some(80.0));
        assert_eq!(items[0].get("exhausted").and_then(Value::as_bool), Some(false));
        assert_eq!(items[1].get("name").and_then(Value::as_str), Some("近 7 天"));
        assert_eq!(items[1].get("exhausted").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn parse_quota_fallback_dig() {
        // 结构不识别 → 宽容 dig 找 level/expire + 额度项
        let resp = json!({
            "result": {
                "vip_level": "Gold",
                "due_time": "2027-01-02 03:04:05",
                "plans": [
                    {"item_name": "图片", "total_count": 100, "remaining": 40},
                    {"item_name": "视频", "total_count": 10, "used": 3},
                ]
            }
        });
        let p = parse_quota(&resp);
        assert_eq!(p.get("level").and_then(Value::as_str), Some("Gold"));
        assert_eq!(
            p.get("expire_at").and_then(Value::as_str),
            Some("2027-01-02 03:04")
        );
        let items = p.get("items").and_then(Value::as_array).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].get("name").and_then(Value::as_str), Some("图片"));
        assert_eq!(items[0].get("total").and_then(Value::as_f64), Some(100.0));
        assert_eq!(items[0].get("left").and_then(Value::as_f64), Some(40.0));
        assert_eq!(items[1].get("used").and_then(Value::as_f64), Some(3.0));
    }

    #[test]
    fn fmt_ts_variants() {
        assert_eq!(fmt_ts(&json!("2027-01-02 03:04:05")), Some("2027-01-02 03:04".into()));
        assert_eq!(fmt_ts(&json!("2027-01-02")), Some("2027-01-02 00:00".into()));
        assert_eq!(fmt_ts(&json!("2027/01/02")), Some("2027-01-02 00:00".into()));
        // 秒级时间戳
        let got = fmt_ts(&json!(1_800_000_000i64));
        assert!(got.is_some());
        // 空串/None
        assert_eq!(fmt_ts(&json!("")), None);
        assert_eq!(fmt_ts(&Value::Null), None);
        // 非法字符串原样截断返回
        assert_eq!(fmt_ts(&json!("不是日期")), Some("不是日期".into()));
    }

    #[test]
    fn summarize_windows_first() {
        let parsed = json!({"items": [
            {"name": "当前时段", "used_percent": 80.0, "reset_at": "2026-09-13 20:32"},
            {"name": "近 7 天", "used_percent": 100.0},
            {"name": "图片", "total": 100, "left": 40},
        ]});
        let s = summarize_parsed(&parsed).unwrap();
        assert!(s.contains("当前时段 已用 80%"));
        assert!(s.contains("（09-13 20:32 重置）"));
        assert!(s.contains("近 7 天 已用完"));
        assert!(s.contains("图片 40/100"));
        assert_eq!(s.matches(" · ").count(), 2);
    }

    #[test]
    fn summarize_empty_returns_none() {
        assert_eq!(summarize_parsed(&json!({"items": []})), None);
        assert_eq!(summarize_parsed(&json!({})), None);
    }

    /// 回归（档 B 两阶段回写）：额度合并只动四个额度字段——巡检窗口内新入池
    /// 账号保留、被删账号不复活、并发方更新的其他字段不被覆盖。
    #[test]
    fn merge_quota_updates_preserves_pool_changes_during_window() {
        // 模巡检快照之后的最新池：u1 被并发方改了 last_keepalive_at；u2 已被删除；
        // u3 为窗口内新入池账号
        let mut latest = json!({"accounts": [
            {"user_id": "u1", "name": "甲", "session_id": "sid-AAA", "last_keepalive_at": "keep-1"},
            {"user_id": "u3", "name": "新入池", "session_id": "sid-CCC"},
        ]});
        let updates = vec![
            (
                "u1".to_string(),
                json!({"level": 2, "expire_at": "2026-11-01"}),
                Some("剩余较多".to_string()),
                "2026-10-09T00:00:00Z".to_string(),
            ),
            // u2：巡检期间被删除 → 不得复活
            (
                "u2".to_string(),
                json!({"level": 4, "expire_at": "2026-12-01"}),
                None,
                "2026-10-09T00:00:00Z".to_string(),
            ),
        ];
        let merged = merge_quota_updates_into_pool(&mut latest, &updates);
        assert_eq!(merged, 1, "被删账号不参与合并，merged 只计仍存在账号");
        let accounts = latest["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2, "被删账号不复活、无新账号混入");
        let a1 = accounts.iter().find(|a| a["user_id"] == "u1").unwrap();
        assert_eq!(a1["quota_level"], 2, "额度字段写入");
        assert_eq!(a1["quota_expire_at"], "2026-11-01");
        assert_eq!(a1["quota_summary"], "剩余较多");
        assert_eq!(a1["quota_checked_at"], "2026-10-09T00:00:00Z");
        assert_eq!(
            a1["last_keepalive_at"], "keep-1",
            "并发方更新的其他字段不被覆盖"
        );
        let a3 = accounts.iter().find(|a| a["user_id"] == "u3").unwrap();
        assert_eq!(a3["name"], "新入池", "窗口内新入池账号保留");
        assert_eq!(a3["session_id"], "sid-CCC");
    }

    /// summary 为 None 时落库为 null（parse 出无摘要的形态）。
    #[test]
    fn merge_quota_updates_none_summary_becomes_null() {
        let mut latest = json!({"accounts": [{"user_id": "u1"}]});
        let updates = vec![(
            "u1".to_string(),
            json!({}),
            None,
            "2026-10-09T00:00:00Z".to_string(),
        )];
        let merged = merge_quota_updates_into_pool(&mut latest, &updates);
        assert_eq!(merged, 1);
        let a1 = latest["accounts"].as_array().unwrap().first().unwrap();
        assert_eq!(a1["quota_level"], Value::Null);
        assert_eq!(a1["quota_expire_at"], Value::Null);
        assert_eq!(a1["quota_summary"], Value::Null);
        assert_eq!(a1["quota_checked_at"], "2026-10-09T00:00:00Z");
    }
}
