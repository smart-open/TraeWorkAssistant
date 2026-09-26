//! Trae Work 积分消耗历史（`POST /trae/api/v1/pay/query_user_usage_group_by_session`）。
//!
//! 此前积分看板的「消耗」口径是 credits_daily 快照的余额差值推算（含签到获得等噪声）；
//! 本模块改为直连接口拉取会话级用量（credits_float / model_name / token 明细），按本地
//! 自然日聚合落盘 data/usage_history.json，供积分趋势图查询展示。
//!
//! 增量语义（避免重复计数）：
//! - 首次拉取（无缓存）：全量拉取近一年（FULL_PULL_DAYS）；
//! - 后续拉取（fresh=true）：从「上次拉取 end_time 所在本地日的 00:00」起重拉，
//!   并**替换**缓存中该日期及之后的日聚合（当天多次拉取不叠加；更早的历史保持不动）；
//! - fresh=false：纯缓存读取，零网络。
//!
//! 请求形态（2026-09-13 代理日志实测）：
//! `{"start_time":<unix秒>,"end_time":<unix秒>,"page_size":N,"page_num":1,"usage_type":[7]}`
//! 响应：`{"total":<会话总数>,"user_usage_group_by_sessions":[{usage_time, credits_float,
//! model_name, extra_info:{input_token,output_token,cache_read_token}, ...}]}`
//!
//! 凭证红线：JWT 仅进请求头（复用 ide_query_post），不进日志/返回值。

use chrono::TimeZone;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use crate::state::AppState;

/// 首次全量拉取窗口（天）
const FULL_PULL_DAYS: i64 = 365;
/// 拉取分块窗口（天）：对齐官方控制台请求粒度，过大区间会被参数校验拒绝（400/9004）
const CHUNK_DAYS: i64 = 30;
/// 单账号单块分页安全上限（防 total 异常导致死循环；50 页 × 20 = 1000 会话/块）
const MAX_PAGES: u32 = 50;
/// 单页大小（对齐官方控制台实测值 20）
const PAGE_SIZE: u32 = 20;
/// 用量类型 7 = Cloud-IDE 会话积分消耗（实测口径）
const USAGE_TYPE: i64 = 7;

const USAGE_URL: &str = "https://api.trae.cn/trae/api/v1/pay/query_user_usage_group_by_session";

/// 单日聚合（date → 消耗合计 / 会话数 / 模型分布 / token 明细）
#[derive(Serialize, serde::Deserialize, Clone, Default)]
pub struct UsageDayStat {
    pub date: String,
    pub credits: f64,
    pub sessions: u64,
    pub models: BTreeMap<String, f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

#[derive(Serialize, Clone, Default)]
pub struct UsageHistoryAccount {
    pub user_id: String,
    pub name: String,
    pub ok: bool,
    /// 本次增量拉取失败但已沿用缓存时的说明；无缓存时为失败原因
    pub error: Option<String>,
    /// 按日期升序
    pub daily: Vec<UsageDayStat>,
}

#[derive(Serialize, Clone)]
pub struct UsageHistoryResult {
    pub fetched_at: i64,
    /// true = 纯缓存读取（未发起网络请求）
    pub cached: bool,
    /// true = 本次拉取中有账号的部分时段会话数超单块分页上限（该时段数据可能不完整）
    pub truncated: bool,
    pub accounts: Vec<UsageHistoryAccount>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct CachedAccount {
    name: String,
    /// 上次拉取的 end_time（Unix 秒）——增量起点 = 该时刻所在本地日的 00:00
    last_fetch_end_ts: Option<i64>,
    daily: BTreeMap<String, UsageDayStat>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct CacheFile {
    fetched_at: Option<i64>,
    accounts: BTreeMap<String, CachedAccount>,
}

fn account_summary(name: String, uid: String, daily: &BTreeMap<String, UsageDayStat>) -> UsageHistoryAccount {
    // 防御：date 一律从映射键回填（旧缓存条目的 date 字段可能为空串）
    let daily = daily
        .iter()
        .map(|(k, v)| {
            let mut d = v.clone();
            d.date = k.clone();
            d
        })
        .collect();
    UsageHistoryAccount {
        user_id: uid,
        name,
        ok: true,
        error: None,
        daily,
    }
}

/// Unix 秒 → 本地自然日（YYYY-MM-DD）
fn local_date_of(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
}

/// 本地自然日 → 当日 00:00 的 Unix 秒（本地时区；无效日期回退 None）
fn local_midnight_ts(date: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    match chrono::Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
    {
        chrono::LocalResult::Single(dt) => Some(dt.timestamp()),
        chrono::LocalResult::Ambiguous(dt, _) => Some(dt.timestamp()),
        chrono::LocalResult::None => None,
    }
}

/// 官网控制台（Web 端）形态请求：对齐 2026-09-13 代理抓包的成功请求——
/// 浏览器 UA + origin/referer www.trae.cn + sec-fetch cors/same-site，
/// **不带** IDE 客户端指纹头（x-market-*/x-device-id 等；该接口按 Web 路由校验，
/// 客户端头 + 大分页/大区间组合返回 400 code=9004 参数错误）。
fn web_usage_post(agent: &ureq::Agent, jwt: &str, body: serde_json::Value) -> Result<Value, String> {
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    let resp = agent
        .post(USAGE_URL)
        .set("accept", "application/json, text/plain, */*")
        .set("accept-language", "zh-CN,zh;q=0.9")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .set("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36")
        .set("origin", "https://www.trae.cn")
        .set("referer", "https://www.trae.cn/")
        .set("sec-fetch-dest", "empty")
        .set("sec-fetch-mode", "cors")
        .set("sec-fetch-site", "same-site")
        .send_json(body)
        .map_err(|e| match e {
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                let snippet: String = body.chars().take(200).collect();
                format!("API 请求失败: status code {code}，响应: {snippet}")
            }
            other => format!("API 请求失败: {other}"),
        })?;
    resp.into_json().map_err(|e| format!("解析响应失败: {e}"))
}

/// 单账号拉取 [start_ts, end_ts] 区间并按本地日聚合。
/// 大区间按 30 天分块（对齐官方控制台请求窗口；chunk 间边界无缝不重叠）。
/// 返回 (日聚合, truncated)：truncated=true 表示某块会话数超单块分页上限、该块尾部
/// 数据被丢弃（其余块继续拉取）；调用方需在结果中注明数据可能不完整。
/// 失败返回 Err（调用方沿用缓存）。
fn fetch_account_usage(
    jwt: &str,
    start_ts: i64,
    end_ts: i64,
) -> Result<(BTreeMap<String, UsageDayStat>, bool), String> {
    let agent = crate::commands::accounts::pay_status_agent();

    let mut agg: BTreeMap<String, UsageDayStat> = BTreeMap::new();
    let mut truncated = false;
    let mut chunk_end = end_ts;
    loop {
        let chunk_start = (chunk_end - CHUNK_DAYS * 86400 + 1).max(start_ts);
        if !fetch_chunk(&agent, jwt, chunk_start, chunk_end, &mut agg)? {
            truncated = true;
        }
        if chunk_start <= start_ts {
            break;
        }
        chunk_end = chunk_start - 1;
    }
    Ok((agg, truncated))
}

/// 单分块（≤30 天）分页拉取并聚合进 agg。
/// 返回 Ok(true) = 该块数据完整；Ok(false) = 达到单块分页安全上限（MAX_PAGES）仍未取完，
/// 该块尾部数据被丢弃（调用方需在结果中注明数据可能不完整，不再静默）。
fn fetch_chunk(
    agent: &ureq::Agent,
    jwt: &str,
    start_ts: i64,
    end_ts: i64,
    agg: &mut BTreeMap<String, UsageDayStat>,
) -> Result<bool, String> {
    let mut page: u32 = 1;
    let mut got: usize = 0;
    let mut total: Option<usize> = None;

    loop {
        let body = json!({
            "start_time": start_ts,
            "end_time": end_ts,
            "page_size": PAGE_SIZE,
            "page_num": page,
            "usage_type": [USAGE_TYPE],
        });
        let resp = web_usage_post(agent, jwt, body)?;
        if total.is_none() {
            total = Some(resp.get("total").and_then(Value::as_u64).unwrap_or(0) as usize);
        }
        let arr = resp
            .get("user_usage_group_by_sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if arr.is_empty() {
            break;
        }
        for s in &arr {
            let ts = s.get("usage_time").and_then(Value::as_i64).unwrap_or(0);
            if ts <= 0 {
                continue;
            }
            // usage_time 为 Unix 秒（实测 1789009361），转本地自然日
            let Some(date) = local_date_of(ts) else {
                continue;
            };
            let credits = s
                .get("credits_float")
                .and_then(Value::as_f64)
                .or_else(|| s.get("amount_float").and_then(Value::as_f64))
                .unwrap_or(0.0);
            let model = s
                .get("model_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or("未知模型")
                .to_string();
            let e = agg.entry(date.clone()).or_default();
            e.date = date;
            e.credits += credits;
            e.sessions += 1;
            *e.models.entry(model).or_insert(0.0) += credits;
            if let Some(extra) = s.get("extra_info") {
                e.input_tokens += extra.get("input_token").and_then(Value::as_u64).unwrap_or(0);
                e.output_tokens += extra.get("output_token").and_then(Value::as_u64).unwrap_or(0);
                e.cache_read_tokens += extra
                    .get("cache_read_token")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }
        }
        got += arr.len();
        let total_n = total.unwrap_or(0);
        // 终止条件：已取满 total / 本页不满页大小（服务端截断页）
        if got >= total_n || (arr.len() as u32) < PAGE_SIZE {
            break;
        }
        // 页数安全上限已到但 total 未取完：如实在上报截断（不再静默丢弃差额）
        if page >= MAX_PAGES {
            return Ok(false);
        }
        page += 1;
    }
    Ok(true)
}

/// 拉取全部账号的积分消耗历史（按本地日聚合），落盘缓存供查询展示。
/// fresh=false：纯缓存读取（零网络）；fresh=true：增量拉取（无缓存账号全量近一年，
/// 已有账号从上次拉取日 00:00 起重拉并替换该日期及之后的聚合）。
/// 逐账号串行分页网络请求（每请求最长 60s），命令桥侧以异步任务派发避免阻塞。
pub fn usage_history_fetch(
    state: &AppState,
    fresh: Option<bool>,
) -> Result<UsageHistoryResult, String> {
    let fresh = fresh.unwrap_or(false);
    let now_ts = chrono::Local::now().timestamp();
    let accounts = crate::vault::load_accounts(&state);
    let mut cache: CacheFile = serde_json::from_value(
        crate::store::docs::usage_history_load(&crate::store::db(&state.data_dir)),
    )
    .unwrap_or_default();

    // 纯缓存读取（零网络；尚未拉取过的账号如实提示）
    if !fresh {
        let mut out = Vec::new();
        for a in &accounts.accounts {
            let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
                continue;
            };
            match cache.accounts.get(&uid) {
                Some(c) => out.push(account_summary(c.name.clone(), uid, &c.daily)),
                None => out.push(UsageHistoryAccount {
                    user_id: uid,
                    name: a.name.clone(),
                    ok: false,
                    error: Some("尚未拉取消耗明细，点击「更新消耗明细」拉取".into()),
                    ..Default::default()
                }),
            }
        }
        return Ok(UsageHistoryResult {
            fetched_at: cache.fetched_at.unwrap_or(0),
            cached: true,
            truncated: false,
            accounts: out,
        });
    }

    // 增量拉取：无缓存账号全量近一年；已有账号从上次拉取日 00:00 重拉并替换该日及之后
    let full_start = now_ts - FULL_PULL_DAYS * 86400;
    let mut errors: BTreeMap<String, String> = BTreeMap::new();
    // 单块分页上限截断的账号（数据可能不完整，需在结果中注明并写运行日志）
    let mut truncated_uids: BTreeSet<String> = BTreeSet::new();
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
            continue;
        };
        let name = a.name.clone();
        if a.jwt.trim().is_empty() {
            // 占位账号（无 JWT）：保留既有缓存，不发起请求
            continue;
        }
        let (start_ts, refetch_from) =
            match cache.accounts.get(&uid).and_then(|c| c.last_fetch_end_ts) {
                Some(last_end) => {
                    let from_date = local_date_of(last_end)
                        .or_else(|| local_date_of(now_ts))
                        .unwrap_or_default();
                    let midnight = local_midnight_ts(&from_date).unwrap_or(now_ts - 86400);
                    (midnight, from_date)
                }
                None => (full_start, String::new()),
            };
        match fetch_account_usage(&a.jwt, start_ts, now_ts) {
            Ok((new_agg, truncated)) => {
                if truncated {
                    truncated_uids.insert(uid.clone());
                    crate::fs_utils::app_log(
                        &state.data_dir,
                        &format!("usage_history: 账号 {name}({uid}) 部分时段会话数超单块分页上限（{MAX_PAGES} 页），该时段数据可能不完整"),
                    );
                }
                let entry = cache.accounts.entry(uid.clone()).or_default();
                entry.name = name;
                if refetch_from.is_empty() {
                    // 全量：整体替换
                    entry.daily = new_agg;
                } else {
                    // 增量：替换 refetch_from 及之后的日聚合（当天多次拉取不叠加）
                    entry
                        .daily
                        .retain(|d, _| d.as_str() < refetch_from.as_str());
                    for (d, v) in new_agg {
                        entry.daily.insert(d, v);
                    }
                }
                entry.last_fetch_end_ts = Some(now_ts);
            }
            Err(e) => {
                // 拉取失败：保留旧缓存，错误在结果中注明
                errors.insert(uid, e);
            }
        }
    }

    cache.fetched_at = Some(now_ts);
    if let Err(e) = serde_json::to_value(&cache)
        .map_err(|e| e.to_string())
        .and_then(|v| crate::store::docs::usage_history_save(&crate::store::db(&state.data_dir), &v))
    {
        // 落盘失败不再静默：写运行日志便于排查（结果仍返回内存数据）
        crate::fs_utils::app_log(&state.data_dir, &format!("usage_history 缓存落盘失败: {e}"));
    }

    let mut out = Vec::new();
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
            continue;
        };
        let name = a.name.clone();
        match cache.accounts.get(&uid) {
            Some(c) => {
                let mut acc = account_summary(c.name.clone(), uid.clone(), &c.daily);
                if let Some(e) = errors.get(&uid) {
                    acc.error = Some(format!("本次更新失败（展示已有缓存）：{e}"));
                }
                if truncated_uids.contains(&uid) {
                    acc.error = Some("部分时段会话数超单块分页上限（50 页 × 20 条/30 天），该时段数据可能不完整".into());
                }
                out.push(acc);
            }
            None => out.push(UsageHistoryAccount {
                error: errors.get(&uid).cloned(),
                user_id: uid,
                name,
                ok: false,
                ..Default::default()
            }),
        }
    }
    Ok(UsageHistoryResult {
        fetched_at: now_ts,
        cached: false,
        truncated: !truncated_uids.is_empty(),
        accounts: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：聚合产物的 date 字段必须写入（此前仅存于映射键，响应 daily.date 恒为空串，
    /// 导致前端区间匹配全部失败、消耗折线/模型柱状图不显示）
    #[test]
    fn aggregated_day_stat_carries_date() {
        let ts = 1_789_009_361i64; // 2026-09-10（+8）
        let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) else {
            panic!("timestamp out of range");
        };
        let date = dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string();
        let mut agg: BTreeMap<String, UsageDayStat> = BTreeMap::new();
        let e = agg.entry(date.clone()).or_default();
        e.date = date.clone();
        e.credits += 1.5;
        // account_summary 从映射键回填 date（含旧缓存 date 字段为空的条目）
        let mut legacy = BTreeMap::new();
        legacy.insert("2026-09-11".to_string(), UsageDayStat::default());
        let acc = account_summary("n".into(), "u".into(), &legacy);
        assert_eq!(acc.daily.len(), 1);
        assert_eq!(acc.daily[0].date, "2026-09-11");
        assert_eq!(agg[&date].date, date);
    }
}
