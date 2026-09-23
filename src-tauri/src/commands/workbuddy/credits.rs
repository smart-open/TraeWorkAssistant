//! WorkBuddy 积分域（原 workbuddy.rs 机械拆分）：M5 积分余额（F-20/F-22）、
//! 快照回退用量（T4.3/F-27）、M5 官方请求用量（F-25/F-58）、活动信息展示（T4.4/F-51）。
//! 函数逻辑零改动，仅将跨子模块引用项提升为 `pub(super)`。

use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::state::AppState;

use super::common::{as_str, auth_file_path_of, load_pool, save_pool};

// ── M5 积分（F-20/F-22，python 三件套 + 缓存）──────────────────────────────

#[tauri::command(async)]
pub fn workbuddy_credits_fetch(app: AppHandle, state: State<AppState>, user_id: Option<String>, fresh: Option<bool>) -> Result<serde_json::Value, String> {
    let _ = &app; // 预留：wb-credits-updated 事件随批次2趋势图启用
    // Rust 直调 tasks::wb_credits（原 python workbuddy_credits.py 移植）；
    // 失败以 Err 返回，成功恒为 {"ok":true,"cached":bool,"accounts":[...]}（消费契约见 tasks/wb_credits.rs 模块注释）
    let parsed = crate::tasks::wb_credits::fetch_credits(&state, user_id.as_deref(), fresh.unwrap_or(false))?;
    // 回写账号池余额缓存（列表/概述展示）
    write_back_pool_balances(&state, &parsed);
    // 会员套餐回填（仅 edition_type 为空的账号，见 backfill_edition_from_payment_type）
    backfill_edition_from_payment_type(&state);
    // 每日余额快照（F-27 数据源）：非缓存命中时追加，按日去重，cap 365 天
    if parsed.get("cached") != Some(&serde_json::json!(true)) {
        append_credits_snapshot(&state, &parsed);
    }
    Ok(parsed)
}

/// 将积分查询结果中的余额回写账号池缓存（列表/概述展示；命令与调度器快照任务共用）
fn write_back_pool_balances(state: &AppState, parsed: &Value) {
    if let Some(accounts) = parsed.get("accounts").and_then(|v| v.as_array()) {
        let mut pool = load_pool(state);
        let now_ts = chrono::Utc::now().timestamp();
        for acc in accounts {
            let uid = acc.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
            let bal = acc.get("balance").and_then(|v| v.as_f64());
            if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == uid) {
                a.credits_balance = bal;
                a.credits_fetched_at = acc.get("fetched_at").and_then(|v| v.as_str()).map(|s| s.to_string());
                // 最早积分包到期（issue #28 API 网关调度口径，Buddy 不分包类型）：
                // 仅成功取数的账号回写，失败（ok=false）保留旧值防误清
                let ok = acc.get("ok").and_then(Value::as_bool).unwrap_or(true);
                if ok {
                    a.credits_expire_at = earliest_pack_expire(acc, now_ts);
                }
            }
        }
        let _ = save_pool(state, &pool);
    }
}

/// 派生账号积分包最早到期（纯函数，便于单测）：
/// packages[]{remaining, expire_ts} 中剩余>0 且未过期的包取 min；
/// 无到期时间的包视为长期有效不参与；全部长期有效/无包信息 → None
fn earliest_pack_expire(acc: &Value, now_ts: i64) -> Option<i64> {
    acc.get("packages")
        .and_then(|v| v.as_array())
        .and_then(|pkgs| {
            pkgs.iter()
                .filter_map(|p| {
                    let rem = p.get("remaining").and_then(Value::as_f64)?;
                    let exp = p.get("expire_ts").and_then(Value::as_i64)?;
                    (rem > 0.0 && exp > now_ts).then_some(exp)
                })
                .min()
        })
}

/// 每日积分余额快照任务（调度器 `wb-credits-snapshot`；补齐近 7 日用量时序的关键）：
/// 强制刷新全部账号积分 → 回写池余额 → 追加当日快照（同日覆盖）。
/// 此前快照仅在「打开积分页且非缓存命中」时写入，应用内调度器到点自动建快照，
/// 差分序列不再因未打开页面而断档。
pub(crate) fn wb_credits_snapshot_task(state: &AppState) -> Result<Value, String> {
    let parsed = crate::tasks::wb_credits::fetch_credits(state, None, true)?;
    write_back_pool_balances(state, &parsed);
    append_credits_snapshot(state, &parsed);
    fs_utils::app_log(
        &state.data_dir,
        "workbuddy: 每日积分余额快照已写入（应用内调度器 wb-credits-snapshot）",
    );
    Ok(parsed)
}

// ── 会员套餐回填（edition_type 数据源补齐）────────────────────────────────
// edition_type 现仅两条写入路径：OAuth login/account（带 editionType）与备份导入；
// auth 文件本身无该字段（实测顶层/ account 键均无），「导入本机账号」入池的账号套餐恒空，
// 两个页面的会员套餐列全为「—」。此处用计费域的付费类型接口补齐（实测可用，域随凭证）。

/// paymentType → 套餐展示名（对齐 CodeBuddy 套餐体系：free=体验版；未知值原样透传）
fn payment_type_to_edition(pt: &str) -> String {
    match pt {
        "free" => "体验版".into(),
        other => other.to_string(),
    }
}

/// 套餐回填：池内 edition_type 为空且有凭证副本的账号，逐个调
/// POST /v2/billing/meter/get-payment-type（域随凭证 domain 路由）→ 映射套餐名回写池。
/// 仅补空，不覆盖 OAuth 已写入的 editionType；单账号失败静默跳过（纯展示增强，fail-open）。
/// 回填一次后池内不再为空，后续调用零网络开销。
fn backfill_edition_from_payment_type(state: &AppState) -> usize {
    let mut pool = load_pool(state);
    let need: Vec<String> = pool
        .accounts
        .iter()
        .filter(|a| a.edition_type.is_empty())
        .map(|a| a.id.clone())
        .collect();
    if need.is_empty() {
        return 0;
    }
    let store: Value = crate::tasks::wb_common::load_token_store(state);
    let tokens = store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    let mut filled = 0usize;
    for id in need {
        let Some(rec) = tokens.get(&id) else { continue };
        let Some(token) = as_str(fs_utils::dig(rec, &["access_token"])).filter(|t| !t.is_empty()) else {
            continue;
        };
        let domain = as_str(fs_utils::dig(rec, &["domain"])).unwrap_or_default();
        let base = if domain.contains("workbuddy.ai") {
            "https://www.workbuddy.ai"
        } else {
            "https://www.workbuddy.cn"
        };
        let Ok(v) = billing_post_json(&agent, &format!("{base}/v2/billing/meter/get-payment-type"), &token) else {
            continue;
        };
        // 注意：dig 是候选键语义（自动穿透 data 信封），候选里不能再写 "data"——
        // 否则 "data" 抢先命中整个信封对象，as_str 恒为 None（曾致套餐回填全静默失败）
        let Some(pt) = as_str(fs_utils::dig(&v, &["paymentType", "payment_type"]))
            .filter(|s| !s.is_empty() && *s != "unknown")
        else {
            continue;
        };
        if let Some(a) = pool.accounts.iter_mut().find(|a| a.id == id) {
            a.edition_type = payment_type_to_edition(&pt);
            filled += 1;
        }
    }
    if filled > 0 {
        let _ = save_pool(state, &pool);
        fs_utils::app_log(&state.data_dir, "workbuddy: 会员套餐已回填（payment-type → edition_type，仅补空）");
    }
    filled
}

/// 手动触发套餐回填（前端在列表存在空套餐账号时调用），返回本次回填的账号数。
/// async：内含逐账号计费域网络请求（≤10s/账号），同步命令会冻结 UI。
#[tauri::command(async)]
pub fn workbuddy_editions_backfill(state: State<AppState>) -> Result<usize, String> {
    Ok(backfill_edition_from_payment_type(&state))
}

/// 追加每日积分余额快照（F-27）：SQLite 化 P6 → wb_credits_history 表，同日覆盖最新 + 365 天裁剪
fn append_credits_snapshot(state: &AppState, parsed: &Value) {
    let store = crate::store::db(&state.data_dir);
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
    if let Err(e) = crate::store::docs::wb_credits_history_upsert(&store, &snap) {
        fs_utils::app_log(&state.data_dir, &format!("workbuddy 积分快照写入失败: {e}"));
        return;
    }
    let _ = crate::store::docs::wb_credits_history_prune(&store);
}

// ── 积分用量快照回退（T4.3/F-27）────────────────────────────────────────────

/// 快照回退用量：官方用量不可用时自动切换数据源（F-27）。
/// 推导：当日消耗 = 前一日总余额 − 当日总余额 + 当日签到奖励（签到日志 message「+N」）；
/// 负差值（充值包到账/快照波动）记 0。口径明示「快照回退」，非官方逐请求统计。
#[tauri::command]
pub fn workbuddy_usage_fallback(state: State<AppState>) -> Result<serde_json::Value, String> {
    let hist: Value = crate::store::docs::wb_credits_history_load(&crate::store::db(&state.data_dir));
    let snapshots = hist.get("snapshots").and_then(Value::as_array).cloned().unwrap_or_default();
    if snapshots.len() < 2 {
        return Err(
            "快照回退不可用：本地余额时序不足（至少两天快照）。请在积分页刷新几次建立时序后重试。".to_string(),
        );
    }

    // 签到日志 → 每日奖励充值（90 天滚动，仅 success 事件；SQLite 化 P3 走 store）
    let results: Value = crate::store::docs::wb_checkin_results_load(&crate::store::db(&state.data_dir));
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
    let cache_path = "workbuddy_usage_official_cache"; // kv 键（SQLite 化 P2）

    // 选号：user_id → auth 文件当前账号 → 首个有 token store 凭证的账号
    //（审查 P1：选号解析提到缓存命中判断之前——缓存按账号区分，命中须同账号）
    let store: serde_json::Value = crate::tasks::wb_common::load_token_store(&state);
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
    // 不一致或旧缓存缺 account_id 一律视为未命中，重新按当前账号拉取。
    // F-59 stale-on-error：过期缓存保留一份，拉取失败时降级回退（见下方 fail 闭包）。
    let cached_val: Option<serde_json::Value> = {
        let c: serde_json::Value = crate::store::db(&state.data_dir).kv_get(cache_path);
        (c.get("status").is_some()).then_some(c)
    };
    if !refresh.unwrap_or(false) {
        if let Some(cached) = &cached_val {
            let fetched = cached.get("fetched_at_ms").and_then(Value::as_i64).unwrap_or(0);
            let cached_acct = cached.get("account_id").and_then(Value::as_str);
            if cached_acct == Some(acct_id.as_str())
                && chrono::Utc::now().timestamp_millis() - fetched < 10 * 60_000
            {
                return Ok(cached.clone());
            }
        }
    }
    // 拉取失败的降级出口：同账号存在历史缓存（即使过期）→ 返回缓存 + stale 标记，
    // 不让看板空屏；无任何缓存才向上抛错（前端再走 usageFallback 快照回退链）
    let fail = |msg: &str| -> Result<serde_json::Value, String> {
        if let Some(c) = &cached_val {
            if c.get("account_id").and_then(Value::as_str) == Some(acct_id.as_str()) {
                let mut stale = c.clone();
                stale["stale"] = serde_json::json!(true);
                stale["stale_reason"] = serde_json::json!(msg);
                return Ok(stale);
            }
        }
        Err(msg.to_string())
    };

    // 核心：拉取 + 聚合（拆出供全账号聚合命令 workbuddy_usage_official_all 复用）
    let payload = match usage_official_fetch(&acct_id, &token, &domain) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    let _ = crate::store::db(&state.data_dir).kv_set(cache_path, &payload);
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "workbuddy: 官方用量刷新（{acct_id}，{} 行）",
            payload.get("request_count_total").and_then(Value::as_u64).unwrap_or(0)
        ),
    );
    Ok(payload)
}

/// 官方用量核心（拉取近 31 天分页明细 + 聚合，缓存逻辑留在命令层）。
/// 拆出供单账号命令 `workbuddy_usage_official` 与全账号聚合命令
/// `workbuddy_usage_official_all`（Buddy 积分看板近 7 日趋势数据源）复用。
fn usage_official_fetch(acct_id: &str, token: &str, domain: &str) -> Result<serde_json::Value, String> {
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
        let data = match v.get("data") {
            Some(d) => d,
            None => return Err("官方响应格式无效".into()),
        };
        let items = match data.get("data").and_then(Value::as_array) {
            Some(a) => a,
            None => return Err("官方响应格式无效".into()),
        };
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

    Ok(serde_json::json!({
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
    }))
}

/// 全账号官方用量聚合（Buddy 积分看板「近 7 日积分消耗」主数据源）：
/// 遍历账号池全部有凭证账号，逐个拉取官方用量明细后按日求和（31 天零填充）。
/// 此前看板用快照差分（usageFallback）作唯一数据源——快照只在打开积分页且非缓存
/// 命中时写入，未打开应用的日子无快照，7 日趋势只剩「昨天」一格。
/// 单账号失败跳过（accounts_ok 计数），全部失败才报错并回退过期缓存（stale）。
/// 聚合结果缓存 10 分钟（跨账号全量拉取代价高，避免看板每次刷新都打满分页请求）。
#[tauri::command(async)]
pub fn workbuddy_usage_official_all(state: State<AppState>) -> Result<serde_json::Value, String> {
    let cache_path = "workbuddy_usage_official_all_cache"; // kv 键（SQLite 化 P2）
    let cached_val: Option<Value> = {
        let c: Value = crate::store::db(&state.data_dir).kv_get(cache_path);
        (c.get("status").is_some()).then_some(c)
    };
    if let Some(cached) = &cached_val {
        let fetched = cached.get("fetched_at_ms").and_then(Value::as_i64).unwrap_or(0);
        if chrono::Utc::now().timestamp_millis() - fetched < 10 * 60_000 {
            return Ok(cached.clone());
        }
    }
    let fail = |msg: &str| -> Result<Value, String> {
        if let Some(c) = &cached_val {
            let mut stale = c.clone();
            stale["stale"] = serde_json::json!(true);
            stale["stale_reason"] = serde_json::json!(msg);
            return Ok(stale);
        }
        Err(msg.to_string())
    };

    // 枚举有凭证账号：账号池优先，token store 补充（按 id 去重）
    let store: Value = crate::tasks::wb_common::load_token_store(&state);
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
    let mut list: Vec<(String, String, String)> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for a in &pool.accounts {
        if let Some(t) = pick(&a.id) {
            if seen_ids.insert(a.id.clone()) {
                list.push(t);
            }
        }
    }
    for id in tokens.keys() {
        if let Some(t) = pick(id) {
            if seen_ids.insert(id.clone()) {
                list.push(t);
            }
        }
    }
    if list.is_empty() {
        return Err("无可用账号凭证（请先在账号管理导入/扫码入池并续期）".into());
    }

    // 逐账号拉取 + 按日聚合（单账号失败跳过，不让一个失效凭证拖垮整板趋势）
    let mut daily_credits: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut ok = 0usize;
    let mut req_total = 0u64;
    for (id, token, domain) in &list {
        if let Ok(p) = usage_official_fetch(id, token, domain) {
            ok += 1;
            req_total += p.get("request_count_total").and_then(Value::as_u64).unwrap_or(0);
            for row in p.get("daily").and_then(Value::as_array).into_iter().flatten() {
                let Some(date) = row.get("date").and_then(Value::as_str) else { continue };
                let Some(u) = row.get("usage").and_then(Value::as_f64) else { continue };
                *daily_credits.entry(date.to_string()).or_insert(0.0) += u;
            }
        }
    }
    if ok == 0 {
        return fail("全部账号官方用量拉取失败（凭证可能已失效，请续期后重试）");
    }

    // 31 天零填充输出（汇总口径与单账号命令一致）
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    let start = today - chrono::Duration::days(30);
    let mut usage_today = 0.0f64;
    let mut usage_week = 0.0f64;
    let mut usage_month = 0.0f64;
    let mut daily_out: Vec<Value> = vec![];
    for i in (0..=30).rev() {
        let d = today - chrono::Duration::days(i);
        let key = d.format("%Y-%m-%d").to_string();
        let u = daily_credits.get(&key).copied().unwrap_or(0.0);
        if i == 0 {
            usage_today = u;
        }
        if i < 7 {
            usage_week += u;
        }
        if d.year() == today.year() && d.month() == today.month() {
            usage_month += u;
        }
        daily_out.push(serde_json::json!({ "date": key, "usage": u }));
    }

    let payload = serde_json::json!({
        "status": "complete",
        "source": "official_all",
        "accounts_total": list.len(),
        "accounts_ok": ok,
        "range_start": start.format("%Y-%m-%d").to_string(),
        "range_end": today.format("%Y-%m-%d").to_string(),
        "fetched_at_ms": chrono::Utc::now().timestamp_millis(),
        "request_count_total": req_total,
        "summary": {
            "usage_today": usage_today,
            "usage_7days": usage_week,
            "usage_this_month": usage_month,
        },
        "daily": daily_out,
    });
    let _ = crate::store::db(&state.data_dir).kv_set(cache_path, &payload);
    fs_utils::app_log(
        &state.data_dir,
        &format!("workbuddy: 全账号官方用量聚合（{ok}/{} 账号，{} 行）", list.len(), req_total),
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
// async：内含 3 次串行网络请求（各 10s 超时），同步命令会冻结 UI（审查修复）
#[tauri::command(async)]
pub fn workbuddy_activity_info(
    state: State<AppState>,
    user_id: Option<String>,
    refresh: Option<bool>,
) -> Result<serde_json::Value, String> {
    let cache_path = "workbuddy_activity_cache"; // kv 键（SQLite 化 P2）

    // 选号逻辑与 workbuddy_usage_official 一致（复用同一降级链）
    //（审查 P1：选号解析提到缓存命中判断之前——缓存按账号区分，命中须同账号）
    let store: serde_json::Value = crate::tasks::wb_common::load_token_store(&state);
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
        let cached: serde_json::Value = crate::store::db(&state.data_dir).kv_get(cache_path);
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
        // dig 候选键语义：自动穿透 data 信封，候选里不写 "data"（同 backfill 处注释）
        Ok(v) => as_str(fs_utils::dig(&v, &["paymentType", "payment_type"]))
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
    let _ = crate::store::db(&state.data_dir).kv_set(cache_path, &payload);
    Ok(payload)
}

/// 活动 banner：公开 GET /v2/activity/banner（宽容解析 banners/banner/list 数组）
fn fetch_activity_banners(agent: &ureq::Agent, base: &str) -> Vec<Value> {
    let resp = agent.get(&format!("{base}/v2/activity/banner")).call();
    let v: Value = match resp {
        Ok(r) => r.into_json().unwrap_or_default(),
        Err(_) => return vec![],
    };
    // dig 候选键语义：自动穿透 data 信封，候选里不写 "data"（否则命中整个信封对象，as_array 恒 None）
    let arr = fs_utils::dig(&v, &["banners"])
        .and_then(|x| x.as_array().cloned())
        .or_else(|| fs_utils::dig(&v, &["banner"]).and_then(|x| x.as_array().cloned()))
        .or_else(|| fs_utils::dig(&v, &["list"]).and_then(|x| x.as_array().cloned()))
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

    // ── earliest_pack_expire（issue #28：Buddy 调度到期派生）────────────────

    #[test]
    fn earliest_pack_expire_takes_min_of_valid_packs() {
        let acc = serde_json::json!({"packages": [
            {"remaining": 10.0, "expire_ts": 2000},
            {"remaining": 5.0, "expire_ts": 1000}
        ]});
        assert_eq!(earliest_pack_expire(&acc, 500), Some(1000));
    }

    #[test]
    fn earliest_pack_expire_skips_drained_expired_and_no_expiry() {
        // 已用完 / 已过期 / 无到期时间（长期有效）的包均不参与，Buddy 不分包类型
        let acc = serde_json::json!({"packages": [
            {"remaining": 0.0, "expire_ts": 900},
            {"remaining": 8.0, "expire_ts": 400},
            {"remaining": 8.0},
            {"remaining": 3.0, "expire_ts": 1500}
        ]});
        assert_eq!(earliest_pack_expire(&acc, 500), Some(1500));
    }

    #[test]
    fn earliest_pack_expire_none_without_valid_packages() {
        assert_eq!(
            earliest_pack_expire(&serde_json::json!({"packages": []}), 500),
            None
        );
        assert_eq!(earliest_pack_expire(&serde_json::json!({}), 500), None);
    }
}
