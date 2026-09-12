//! WorkBuddy 积分域（原 workbuddy.rs 机械拆分）：M5 积分余额（F-20/F-22）、
//! 快照回退用量（T4.3/F-27）、M5 官方请求用量（F-25/F-58）、活动信息展示（T4.4/F-51）。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::os::windows::process::CommandExt;
use std::process::Command;
use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::state::AppState;

use super::common::{as_str, auth_file_path_of, load_pool, save_pool, token_store_path};

// ── M5 积分（F-20/F-22，python 三件套 + 缓存）──────────────────────────────

#[tauri::command(async)]
pub fn workbuddy_credits_fetch(app: AppHandle, state: State<AppState>, user_id: Option<String>, fresh: Option<bool>) -> Result<serde_json::Value, String> {
    let _ = &app; // 预留：wb-credits-updated 事件随批次2趋势图启用
    let mut args: Vec<String> = Vec::new();
    if let Some(uid) = &user_id {
        args.push("--uid".into());
        args.push(uid.clone());
    }
    if fresh.unwrap_or(false) {
        args.push("--fresh".into());
    }
    let script_path = state.python_dir.join("workbuddy_credits.py");
    if !script_path.exists() {
        return Err(format!("找不到脚本: {}", script_path.display()));
    }
    let out = Command::new(&state.python_exe)
        .arg(&script_path)
        .args(&args)
        .creation_flags(0x08000000)
        .env("AIWORKDATA_DIR", &state.data_dir)
        .env("PYTHONIOENCODING", "utf-8")
        .output()
        .map_err(|e| format!("积分查询失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // 取末行 JSON（脚本可能输出告警行）
    let parsed = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .ok_or_else(|| format!("积分查询输出无法解析: {}", stdout.trim().chars().take(200).collect::<String>()))?;
    if parsed.get("ok") != Some(&serde_json::json!(true)) {
        return Err("积分查询失败（详见脚本输出）".into());
    }
    // 回写账号池余额缓存（列表/概述展示）
    if let Some(accounts) = parsed.get("accounts").and_then(|v| v.as_array()) {
        let mut pool = load_pool(&state);
        for acc in accounts {
            let uid = acc.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
            let bal = acc.get("balance").and_then(|v| v.as_f64());
            if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == uid) {
                a.credits_balance = bal;
                a.credits_fetched_at = acc.get("fetched_at").and_then(|v| v.as_str()).map(|s| s.to_string());
            }
        }
        let _ = save_pool(&state, &pool);
    }
    // 每日余额快照（F-27 数据源）：非缓存命中时追加，按日去重，cap 365 天
    if parsed.get("cached") != Some(&serde_json::json!(true)) {
        append_credits_snapshot(&state, &parsed);
    }
    Ok(parsed)
}

/// 追加每日积分余额快照（F-27）：workbuddy_credits_history.json，同日覆盖最新
fn append_credits_snapshot(state: &AppState, parsed: &Value) {
    let path = state.data_dir.join("data").join("workbuddy_credits_history.json");
    let mut hist: Value = fs_utils::read_json(&path);
    if !hist.is_object() {
        hist = serde_json::json!({});
    }
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let accounts: Vec<Value> = parsed
        .get("accounts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|a| {
                    serde_json::json!({
                        "user_id": a.get("user_id").cloned().unwrap_or_default(),
                        "balance": a.get("balance").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let total: f64 = accounts.iter().filter_map(|a| a.get("balance").and_then(Value::as_f64)).sum();
    let snap = serde_json::json!({
        "date": today,
        "ts": chrono::Utc::now().timestamp_millis(),
        "total_balance": total,
        "accounts": accounts,
    });
    let Some(obj) = hist.as_object_mut() else { return };
    let arr = obj.entry("snapshots".to_string()).or_insert_with(|| serde_json::json!([]));
    if let Some(list) = arr.as_array_mut() {
        match list.iter().position(|s| s.get("date").and_then(Value::as_str) == Some(today.as_str())) {
            Some(pos) => list[pos] = snap,
            None => list.push(snap),
        }
        let len = list.len();
        if len > 365 {
            list.drain(..len - 365);
        }
    }
    let _ = fs_utils::write_json(&path, &hist);
}

// ── 积分用量快照回退（T4.3/F-27）────────────────────────────────────────────

/// 快照回退用量：官方用量不可用时自动切换数据源（F-27）。
/// 推导：当日消耗 = 前一日总余额 − 当日总余额 + 当日签到奖励（签到日志 message「+N」）；
/// 负差值（充值包到账/快照波动）记 0。口径明示「快照回退」，非官方逐请求统计。
#[tauri::command]
pub fn workbuddy_usage_fallback(state: State<AppState>) -> Result<serde_json::Value, String> {
    let hist: Value = fs_utils::read_json(&state.data_dir.join("data").join("workbuddy_credits_history.json"));
    let snapshots = hist.get("snapshots").and_then(Value::as_array).cloned().unwrap_or_default();
    if snapshots.len() < 2 {
        return Err(
            "快照回退不可用：本地余额时序不足（至少两天快照）。请在积分页刷新几次建立时序后重试。".to_string(),
        );
    }

    // 签到日志 → 每日奖励充值（90 天滚动，仅 success 事件）
    let results: Value = fs_utils::read_json(&state.data_dir.join("data").join("workbuddy_checkin_results.json"));
    let mut recharge: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for r in results.get("results").and_then(Value::as_array).into_iter().flatten() {
        if r.get("status").and_then(Value::as_str) != Some("success") {
            continue;
        }
        let Some(date) = r.get("date").and_then(Value::as_str) else { continue };
        let msg = r.get("message").and_then(Value::as_str).unwrap_or("");
        if let Some(v) = parse_reward_plus(msg) {
            *recharge.entry(date.to_string()).or_insert(0.0) += v;
        }
    }

    // 快照差分（快照按 date 升序，credits 追加时保序）
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut daily: Vec<Value> = vec![];
    for i in 1..snapshots.len() {
        let (Some(prev_bal), Some(cur_bal)) = (
            snapshots[i - 1].get("total_balance").and_then(Value::as_f64),
            snapshots[i].get("total_balance").and_then(Value::as_f64),
        ) else {
            continue;
        };
        let Some(date) = snapshots[i].get("date").and_then(Value::as_str) else { continue };
        let reward = recharge.get(date).copied().unwrap_or(0.0);
        let usage = (prev_bal - cur_bal + reward).max(0.0);
        daily.push(serde_json::json!({ "date": date, "usage": usage }));
    }

    // 聚合：今日/近 7 天/本月（口径与官方用量对齐）
    use chrono::Datelike;
    let now = chrono::Local::now().date_naive();
    let mut usage_today = 0.0f64;
    let mut usage_week = 0.0f64;
    let mut usage_month = 0.0f64;
    for d in &daily {
        let (Some(date), Some(u)) = (d.get("date").and_then(Value::as_str), d.get("usage").and_then(Value::as_f64)) else {
            continue;
        };
        let Ok(day) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") else { continue };
        let dist = (now - day).num_days();
        if dist == 0 {
            usage_today += u;
        }
        if (0..7).contains(&dist) {
            usage_week += u;
        }
        if day.year() == now.year() && day.month() == now.month() {
            usage_month += u;
        }
    }

    Ok(serde_json::json!({
        "status": "snapshot",
        "snapshot_days": snapshots.len(),
        "summary": {
            "usage_today": usage_today,
            "usage_7days": usage_week,
            "usage_this_month": usage_month,
        },
        "daily": daily,
        "note": "快照回退数据源（本地余额时序差分 + 签到日志推导），非官方逐请求口径",
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        "_today": today,
    }))
}

/// 从签到 message 提取「+N」奖励数额（不硬编码数额，仅解析接口回显）
fn parse_reward_plus(msg: &str) -> Option<f64> {
    let idx = msg.find('+')?;
    let rest = &msg[idx + 1..];
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    num.parse::<f64>().ok().filter(|v| *v > 0.0)
}

// ── M5 官方请求用量（F-25/F-58，批次3）────────────────────────────────────
//
// POST <domain>/billing/meter/get-user-request-usage（设计 §7.1，语义对齐
// oss-research/workbuddy-switch official_usage.rs）：近 31 天窗口分页拉取请求明细，
// 聚合 今日/近7天/本月 消耗积分 + 逐日/按模型排行；本地缓存 10 分钟（refresh 强制刷新）。
// 脱敏红线：上游可能携带的 prompt/input 等字段一律不复制，不入缓存不返回。

/// requestTime → 本地日期 "YYYY-MM-DD"（兼容 epoch 秒/毫秒、RFC3339、本地时间串、纯日期）
fn usage_row_date(item: &Value) -> Option<String> {
    use chrono::TimeZone;
    let raw = item.get("requestTime").or_else(|| item.get("request_time"))?;
    if let Some(n) = raw.as_f64() {
        if !n.is_finite() {
            return None;
        }
        let ms = if n.abs() < 10_000_000_000.0 { (n * 1000.0).round() as i64 } else { n.round() as i64 };
        return chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string());
    }
    let text = raw.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(text, fmt) {
            return chrono::Local
                .from_local_datetime(&parsed)
                .single()
                .map(|d| d.date_naive().format("%Y-%m-%d").to_string());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Some(d.format("%Y-%m-%d").to_string());
    }
    None
}

fn usage_credit(item: &Value) -> Option<f64> {
    let v = item.get("credit")?;
    let n = v.as_f64().or_else(|| v.as_str()?.trim().parse::<f64>().ok())?;
    (n.is_finite() && n >= 0.0).then_some(n)
}

/// 官方请求用量（F-25）：user_id 指定账号，缺省取 auth 文件当前账号或首个有凭证账号。
#[tauri::command(async)]
pub fn workbuddy_usage_official(
    state: State<AppState>,
    user_id: Option<String>,
    refresh: Option<bool>,
) -> Result<serde_json::Value, String> {
    let cache_path = state.data_dir.join("data").join("workbuddy_usage_official_cache.json");

    // 选号：user_id → auth 文件当前账号 → 首个有 token store 凭证的账号
    //（审查 P1：选号解析提到缓存命中判断之前——缓存按账号区分，命中须同账号）
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(&state));
    let tokens = store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let pick = |id: &str| -> Option<(String, String, String)> {
        let rec = tokens.get(id)?;
        let token = as_str(fs_utils::dig(&rec, &["access_token"]))?;
        if token.is_empty() {
            return None;
        }
        let domain = as_str(fs_utils::dig(&rec, &["domain"])).unwrap_or_default();
        Some((id.to_string(), token, domain))
    };
    let pool = load_pool(&state);
    let chosen = user_id
        .as_deref()
        .and_then(|uid| pick(uid))
        .or_else(|| {
            let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path_of(&state));
            let fuid = as_str(fs_utils::dig(&raw, &["uid"]))?;
            pool.accounts.iter().find(|a| a.uid == fuid).map(|a| a.id.clone()).and_then(|id| pick(&id))
        })
        .or_else(|| pool.accounts.iter().find_map(|a| pick(&a.id)))
        .or_else(|| tokens.keys().find_map(|k| pick(k)))
        .ok_or("无可用账号凭证（请先在账号管理导入/扫码入池并续期）")?;
    let (acct_id, token, domain) = chosen;

    // 缓存命中条件（审查 P1）：状态存在 + 10min 内 + 缓存 account_id 与本次解析账号一致；
    // 不一致或旧缓存缺 account_id 一律视为未命中，重新按当前账号拉取
    if !refresh.unwrap_or(false) {
        let cached: serde_json::Value = fs_utils::read_json(&cache_path);
        let fetched = cached.get("fetched_at_ms").and_then(Value::as_i64).unwrap_or(0);
        let cached_acct = cached.get("account_id").and_then(Value::as_str);
        if cached.get("status").is_some()
            && cached_acct == Some(acct_id.as_str())
            && chrono::Utc::now().timestamp_millis() - fetched < 10 * 60_000
        {
            return Ok(cached);
        }
    }

    // 区域路由（T4.5/F-36，§5.2）：Global 账号（domain 含 workbuddy.ai）billing
    // 全走 www.workbuddy.ai；CN 账号维持既有 workbuddy.cn 网关。
    let base = if domain.contains("workbuddy.ai") {
        "https://www.workbuddy.ai".to_string()
    } else if domain.is_empty() {
        "https://www.workbuddy.cn".to_string()
    } else if domain.starts_with("http://") || domain.starts_with("https://") {
        domain.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", domain.trim_end_matches('/'))
    };
    let url = format!("{base}/billing/meter/get-user-request-usage");
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(30)).build();

    let today = chrono::Local::now().date_naive();
    let start = today - chrono::Duration::days(30);
    let start_text = format!("{start} 00:00:00");
    let end_text = format!("{today} 23:59:59");

    // 分页拉取（≤20 页 × 3000 行；requestId 去重；跳过 credit 缺失/负值/时间不可解析行）
    let mut rows: Vec<(String, f64, String)> = vec![]; // (date, credit, model)
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut reported_total: u64 = 0;
    for page in 1u64..=20 {
        let body = serde_json::json!({
            "startTime": start_text,
            "endTime": end_text,
            "pageNum": page,
            "pageSize": 3000,
        });
        let resp = agent
            .post(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("X-Client-Platform", "web")
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let v: serde_json::Value = match resp {
            Ok(r) => r.into_json().unwrap_or_default(),
            Err(ureq::Error::Status(code, _)) => {
                return Err(format!("官方用量请求失败（HTTP {code}）：请检查凭证有效期"));
            }
            Err(e) => return Err(format!("官方用量请求失败: {e}")),
        };
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 && code != 200 {
            return Err(format!("官方用量请求失败（code={code}）"));
        }
        let data = v.get("data").ok_or("官方响应格式无效")?;
        let items = data
            .get("data")
            .and_then(Value::as_array)
            .ok_or("官方响应格式无效")?;
        reported_total = reported_total.max(
            data.get("total")
                .and_then(Value::as_u64)
                .unwrap_or(items.len() as u64),
        );
        for item in items {
            let Some(credit) = usage_credit(item) else { continue };
            let Some(date) = usage_row_date(item) else { continue };
            let model = as_str(fs_utils::dig(item, &["model"]))
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "未知模型".into());
            let rid = as_str(fs_utils::dig(item, &["requestId", "request_id"]))
                .unwrap_or_else(|| uuidless_key(&date, &model, &credit));
            if seen.insert(rid) {
                rows.push((date, credit, model));
            }
        }
        if items.is_empty() || seen.len() as u64 >= reported_total {
            break;
        }
    }

    // 聚合：今日/近7天/本月 + 逐日（含按模型）+ 模型排行
    use chrono::Datelike;
    let mut usage_today = 0.0f64;
    let mut usage_week = 0.0f64;
    let mut usage_month = 0.0f64;
    let mut daily_credits: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut daily_models: std::collections::HashMap<String, std::collections::HashMap<String, (u64, f64)>> =
        std::collections::HashMap::new();
    let mut model_totals: std::collections::HashMap<String, (u64, f64)> = std::collections::HashMap::new();
    for (date, credit, model) in &rows {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            let dist = (today - d).num_days();
            if dist == 0 {
                usage_today += credit;
            }
            if (0..7).contains(&dist) {
                usage_week += credit;
            }
            if d.year() == today.year() && d.month() == today.month() {
                usage_month += credit;
            }
        }
        *daily_credits.entry(date.clone()).or_insert(0.0) += credit;
        let dm = daily_models.entry(date.clone()).or_default();
        let e = dm.entry(model.clone()).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += credit;
        let mt = model_totals.entry(model.clone()).or_insert((0, 0.0));
        mt.0 += 1;
        mt.1 += credit;
    }

    let mut models_out: Vec<serde_json::Value> = model_totals
        .into_iter()
        .map(|(model, (count, credit))| {
            serde_json::json!({ "model": model, "request_count": count, "credit": credit })
        })
        .collect();
    models_out.sort_by(|a, b| {
        let ca = a.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        let cb = b.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut daily_out: Vec<serde_json::Value> = vec![];
    for i in (0..=30).rev() {
        let d = today - chrono::Duration::days(i);
        let key = d.format("%Y-%m-%d").to_string();
        let mut day_models: Vec<serde_json::Value> = daily_models
            .get(&key)
            .map(|m| {
                m.iter()
                    .map(|(model, (count, credit))| {
                        serde_json::json!({ "model": model, "request_count": count, "credit": credit })
                    })
                    .collect()
            })
            .unwrap_or_default();
        day_models.sort_by(|a, b| {
            let ca = a.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
            let cb = b.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
            cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
        });
        daily_out.push(serde_json::json!({
            "date": key,
            "usage": daily_credits.get(&key).copied().unwrap_or(0.0),
            "models": day_models,
        }));
    }

    let payload = serde_json::json!({
        "status": "complete",
        "account_id": acct_id,
        "domain": base,
        "range_start": start.format("%Y-%m-%d").to_string(),
        "range_end": today.format("%Y-%m-%d").to_string(),
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        "request_count_total": seen.len(),
        "summary": {
            "usage_today": usage_today,
            "usage_7days": usage_week,
            "usage_this_month": usage_month,
        },
        "daily": daily_out,
        "models": models_out,
    });
    let _ = fs_utils::write_json(&cache_path, &payload);
    fs_utils::app_log(
        &state.data_dir,
        &format!("workbuddy: 官方用量刷新（{acct_id}，{}/{} 行）", seen.len(), reported_total),
    );
    Ok(payload)
}

/// requestId 缺失时的稳定兜底 key（不含任何敏感字段）
fn uuidless_key(date: &str, model: &str, credit: &f64) -> String {
    let mut h = Sha256::new();
    h.update(date.as_bytes());
    h.update(model.as_bytes());
    h.update(credit.to_le_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("auto-{}", &hex[..16])
}

// ==================== 活动信息展示（T4.4/F-51） ====================

/// 活动信息三端点聚合（F-51，§7.1）：活动 banner（公开 GET）+ 付费类型 +
/// 用量提醒（billing POST）。低频附加展示：10min 缓存；端点失败不致命，
/// 逐项容错并记入 errors（宽容解析，字段缺失返回 null）。
#[tauri::command]
pub fn workbuddy_activity_info(
    state: State<AppState>,
    user_id: Option<String>,
    refresh: Option<bool>,
) -> Result<serde_json::Value, String> {
    let cache_path = state.data_dir.join("data").join("workbuddy_activity_cache.json");

    // 选号逻辑与 workbuddy_usage_official 一致（复用同一降级链）
    //（审查 P1：选号解析提到缓存命中判断之前——缓存按账号区分，命中须同账号）
    let store: serde_json::Value = fs_utils::read_json(&token_store_path(&state));
    let tokens = store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let pick = |id: &str| -> Option<(String, String, String)> {
        let rec = tokens.get(id)?;
        let token = as_str(fs_utils::dig(&rec, &["access_token"]))?;
        if token.is_empty() {
            return None;
        }
        let domain = as_str(fs_utils::dig(&rec, &["domain"])).unwrap_or_default();
        Some((id.to_string(), token, domain))
    };
    let pool = load_pool(&state);
    let chosen = user_id
        .as_deref()
        .and_then(|uid| pick(uid))
        .or_else(|| {
            let raw = fs_utils::read_json::<serde_json::Value>(&auth_file_path_of(&state));
            let fuid = as_str(fs_utils::dig(&raw, &["uid"]))?;
            pool.accounts.iter().find(|a| a.uid == fuid).map(|a| a.id.clone()).and_then(|id| pick(&id))
        })
        .or_else(|| pool.accounts.iter().find_map(|a| pick(&a.id)))
        .or_else(|| tokens.keys().find_map(|k| pick(k)));
    let resolved_acct = chosen.as_ref().map(|(id, _, _)| id.clone());

    // 缓存命中条件（审查 P1）：10min 内 + 缓存 account_id 与本次解析账号一致
    //（无凭证时解析结果为 None，与无凭证缓存 payload 的 account_id=null 对齐）
    if !refresh.unwrap_or(false) {
        let cached: serde_json::Value = fs_utils::read_json(&cache_path);
        let fetched = cached.get("fetched_at_ms").and_then(Value::as_i64).unwrap_or(0);
        let cached_acct = cached.get("account_id").and_then(Value::as_str);
        if cached.get("account_id").is_some()
            && cached_acct == resolved_acct.as_deref()
            && chrono::Utc::now().timestamp_millis() - fetched < 10 * 60_000
        {
            return Ok(cached);
        }
    }

    let Some((acct_id, token, domain)) = chosen else {
        // 无凭证：banner 仍可拉（公开端点），付费类型/用量提醒跳过
        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
        let banners = fetch_activity_banners(&agent, "https://www.workbuddy.cn");
        return Ok(serde_json::json!({
            "account_id": null, "payment_type": null, "dosage_notify": null,
            "banners": banners, "errors": [], "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        }));
    };

    // 区域路由（F-36，§5.2）：billing/activity 随账号 domain
    let base = if domain.contains("workbuddy.ai") {
        "https://www.workbuddy.ai".to_string()
    } else {
        "https://www.workbuddy.cn".to_string()
    };
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    let mut errors: Vec<String> = vec![];

    let banners = fetch_activity_banners(&agent, &base);

    // 付费类型：POST /v2/billing/meter/get-payment-type → data.paymentType
    let payment_type = match billing_post_json(&agent, &format!("{base}/v2/billing/meter/get-payment-type"), &token) {
        Ok(v) => as_str(fs_utils::dig(&v, &["data", "paymentType", "payment_type"]))
            .filter(|s| !s.is_empty() && *s != "unknown")
            .map(|s| s.to_string()),
        Err(e) => {
            errors.push(format!("payment-type: {e}"));
            None
        }
    };

    // 用量提醒：POST /v2/billing/meter/get-dosage-notify（宽容透传 data 内容）
    let dosage_notify = match billing_post_json(&agent, &format!("{base}/v2/billing/meter/get-dosage-notify"), &token) {
        Ok(v) => {
            let data = v.get("data").cloned().unwrap_or(Value::Null);
            if data.is_null() { None } else { Some(data) }
        }
        Err(e) => {
            errors.push(format!("dosage-notify: {e}"));
            None
        }
    };

    let payload = serde_json::json!({
        "account_id": acct_id,
        "payment_type": payment_type,
        "dosage_notify": dosage_notify,
        "banners": banners,
        "errors": errors,
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
    });
    let _ = fs_utils::write_json(&cache_path, &payload);
    Ok(payload)
}

/// 活动 banner：公开 GET /v2/activity/banner（宽容解析 banners/banner/list 数组）
fn fetch_activity_banners(agent: &ureq::Agent, base: &str) -> Vec<Value> {
    let resp = agent.get(&format!("{base}/v2/activity/banner")).call();
    let v: Value = match resp {
        Ok(r) => r.into_json().unwrap_or_default(),
        Err(_) => return vec![],
    };
    let arr = fs_utils::dig(&v, &["data", "banners"])
        .and_then(|x| x.as_array().cloned())
        .or_else(|| fs_utils::dig(&v, &["data", "banner"]).and_then(|x| x.as_array().cloned()))
        .or_else(|| fs_utils::dig(&v, &["data", "list"]).and_then(|x| x.as_array().cloned()))
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    // 只保留展示字段，剥离未知字段
    arr.iter()
        .filter(|b| b.is_object())
        .map(|b| {
            serde_json::json!({
                "title": as_str(fs_utils::dig(b, &["title", "name"])).unwrap_or_default(),
                "content": as_str(fs_utils::dig(b, &["content", "description", "desc"])).unwrap_or_default(),
                "url": as_str(fs_utils::dig(b, &["url", "link", "jump_url", "jumpUrl"])).unwrap_or_default(),
                "start_time": as_str(fs_utils::dig(b, &["start_time", "startTime"])).unwrap_or_default(),
                "end_time": as_str(fs_utils::dig(b, &["end_time", "endTime"])).unwrap_or_default(),
            })
        })
        .collect()
}

/// billing POST（Bearer + web 平台头，脱敏红线：不携带 X-Refresh-Token）
fn billing_post_json(agent: &ureq::Agent, url: &str, token: &str) -> Result<Value, String> {
    let resp = agent
        .post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("X-Client-Platform", "web")
        .set("Content-Type", "application/json")
        .send_string("{}");
    match resp {
        Ok(r) => r.into_json().map_err(|e| format!("响应解析失败: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(test)]
mod credits_tests {
    use super::*;

    #[test]
    fn parse_reward_plus_extracts_amounts() {
        assert_eq!(parse_reward_plus("签到成功 +10"), Some(10.0));
        assert_eq!(parse_reward_plus("签到成功 +2.5"), Some(2.5));
        assert_eq!(parse_reward_plus("签到成功"), None);
        assert_eq!(parse_reward_plus("已签到"), None);
        assert_eq!(parse_reward_plus("签到成功 +0"), None);
    }
}
