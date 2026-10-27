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
use tauri::State;

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
/// 已过期/临期权益包查询（issue #82 T5.1：TraeTools GetExpiredEntsAsync 同接口）。
/// 返回含已过期包——credit_packs 缓存只含「剩余>0 且未过期」的包，本接口补齐到期日历的已过期展示
const EXPIRED_ENTS_URL: &str = "https://api.trae.cn/trae/api/v2/pay/expired_ents";

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

/// 会话级行明细（按本地日落盘 date → rows，供「会话明细」Tab 查询；issue #82）。
/// 与 UsageDayStat 同源采集（fetch_chunk 内一次遍历同时产出聚合与行明细）。
#[derive(Serialize, serde::Deserialize, Clone, Default)]
pub struct UsageSessionRow {
    pub session_id: String,
    /// usage_time（Unix 秒）
    pub ts: i64,
    pub model: String,
    /// 会话模式（agent/chat 等；上游缺失为空串）
    pub mode: String,
    /// 消耗积分（credits_float，兜底 amount_float）
    pub credits: f64,
    /// 消耗金额（cost_money_float；上游缺失为 0）
    pub cost_money: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// 会话说明（user_input_preview，截断 200 字符）
    pub preview: String,
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
    pub accounts: Vec<UsageHistoryAccount>,
}

/// 已过期/临期权益包（issue #82 T5.1：/trae/api/v2/pay/expired_ents 实时查询）
#[derive(Serialize, Clone, Default)]
pub struct TraeExpiredEnt {
    /// 包名（与 credit_packs[].source 同源口径，前端按「包名+到期时间」去重）
    pub name: String,
    /// 包总额度（credits_limit）
    pub credits_limit: f64,
    /// 过期时间（Unix 毫秒，上游原值；前端转秒展示）
    pub expire_time_ms: i64,
}

#[derive(Serialize, Clone, Default)]
pub struct TraeExpiredEntsAccount {
    pub user_id: String,
    pub name: String,
    pub ok: bool,
    /// 单账号查询失败原因（其余账号不受影响）
    pub error: Option<String>,
    pub expired: Vec<TraeExpiredEnt>,
}

#[derive(Serialize, Clone)]
pub struct TraeExpiredEntsResult {
    pub fetched_at: i64,
    pub accounts: Vec<TraeExpiredEntsAccount>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct CachedAccount {
    name: String,
    /// 上次拉取的 end_time（Unix 秒）——增量起点 = 该时刻所在本地日的 00:00
    last_fetch_end_ts: Option<i64>,
    daily: BTreeMap<String, UsageDayStat>,
    /// 会话级行明细（date → 行列表；serde default 兼容旧缓存文件）
    #[serde(default)]
    sessions: BTreeMap<String, Vec<UsageSessionRow>>,
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
/// T5.1 起泛化 url 参数：usage 拉取与 expired_ents 逐包到期共用同一头部惯例。
fn web_usage_post(
    agent: &ureq::Agent,
    jwt: &str,
    url: &str,
    body: serde_json::Value,
) -> Result<Value, String> {
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    let resp = agent
        .post(url)
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
/// 失败返回 Err（调用方沿用缓存）。
fn fetch_account_usage(
    jwt: &str,
    start_ts: i64,
    end_ts: i64,
) -> Result<
    (
        BTreeMap<String, UsageDayStat>,
        BTreeMap<String, Vec<UsageSessionRow>>,
    ),
    String,
> {
    let agent = crate::commands::accounts::pay_status_agent();

    let mut agg: BTreeMap<String, UsageDayStat> = BTreeMap::new();
    let mut sessions: BTreeMap<String, Vec<UsageSessionRow>> = BTreeMap::new();
    let mut chunk_end = end_ts;
    loop {
        let chunk_start = (chunk_end - CHUNK_DAYS * 86400 + 1).max(start_ts);
        fetch_chunk(&agent, jwt, chunk_start, chunk_end, &mut agg, &mut sessions)?;
        if chunk_start <= start_ts {
            break;
        }
        chunk_end = chunk_start - 1;
    }
    Ok((agg, sessions))
}

/// 会话说明截断（200 字符，防异常长文本撑爆缓存与表格）
fn truncate_preview(s: &str) -> String {
    s.chars().take(200).collect()
}

/// 会话积分提取（T1.5 回归口径）：credits_float 优先，旧版字段 amount_float 兜底；
/// 均缺失或非数值时为 0.0（不伪造积分）
fn extract_session_credits(s: &Value) -> f64 {
    s.get("credits_float")
        .and_then(Value::as_f64)
        .or_else(|| s.get("amount_float").and_then(Value::as_f64))
        .unwrap_or(0.0)
}

/// 单分块（≤30 天）分页拉取并聚合进 agg，同时产出会话级行明细（issue #82）
fn fetch_chunk(
    agent: &ureq::Agent,
    jwt: &str,
    start_ts: i64,
    end_ts: i64,
    agg: &mut BTreeMap<String, UsageDayStat>,
    sessions: &mut BTreeMap<String, Vec<UsageSessionRow>>,
) -> Result<(), String> {
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
        let resp = web_usage_post(agent, jwt, USAGE_URL, body)?;
        // total 字段缺失时不写入哨兵 0（G9）：保持 None，终止条件按满页判断继续
        if total.is_none() {
            if let Some(n) = resp.get("total").and_then(Value::as_u64) {
                total = Some(n as usize);
            }
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
            let credits = extract_session_credits(s);
            let model = s
                .get("model_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or("未知模型")
                .to_string();
            // 会话级行明细（issue #82）：与聚合同源同次遍历产出；字段缺失用安全默认值
            let extra = s.get("extra_info");
            let tok = |k: &str| -> u64 {
                extra
                    .and_then(|e| e.get(k))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            };
            let row = UsageSessionRow {
                session_id: s
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                ts,
                model: model.clone(),
                mode: s
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                credits,
                cost_money: s
                    .get("cost_money_float")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                input_tokens: tok("input_token"),
                output_tokens: tok("output_token"),
                cache_read_tokens: tok("cache_read_token"),
                cache_write_tokens: tok("cache_write_token"),
                preview: s
                    .get("user_input_preview")
                    .and_then(Value::as_str)
                    .map(truncate_preview)
                    .unwrap_or_default(),
            };
            let e = agg.entry(date.clone()).or_default();
            e.date = date.clone();
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
            sessions.entry(date).or_default().push(row);
        }
        got += arr.len();
        // 终止条件（G9）：total 明确时取满即止；total 字段缺失按满页判断继续拉取
        //（原实现缺失被当作 0，第一页后即终止静默截断），至 MAX_PAGES 上限如实
        // 返回已取行；本页不满页大小（服务端截断页）照常终止
        if matches!(total, Some(n) if got >= n)
            || (arr.len() as u32) < PAGE_SIZE
            || page >= MAX_PAGES
        {
            break;
        }
        page += 1;
    }
    Ok(())
}

/// 拉取全部账号的积分消耗历史（按本地日聚合），落盘缓存供查询展示。
/// fresh=false：纯缓存读取（零网络）；fresh=true：增量拉取（无缓存账号全量近一年，
/// 已有账号从上次拉取日 00:00 起重拉并替换该日期及之后的聚合）。
/// async 派发：逐账号串行分页网络请求（每请求最长 60s），同步命令会冻住 UI。
#[tauri::command(async)]
pub fn usage_history_fetch(
    state: State<AppState>,
    fresh: Option<bool>,
) -> Result<UsageHistoryResult, String> {
    usage_history_fetch_impl(&state, fresh.unwrap_or(false))
}

/// Trae 逐包精确到期（issue #82 T5.1）：`POST /trae/api/v2/pay/expired_ents` 实时查询，
/// 返回含已过期包——credit_packs 缓存只含「剩余>0 且未过期」的包，本接口补齐到期日历的
/// 已过期展示（前端按「包名+到期时间」去重后并入）。头部复用 web_usage_post 的 Web 路由
/// 形态（浏览器 UA + origin/referer，不带 IDE 指纹头）；JWT 仅进请求头（凭证红线）。
/// 无分页无缓存、轻请求：随看板进入/手动刷新触发，调度器静默同步不触发。
/// 单账号失败行内注明（对齐部分失败惯例）；无 JWT 占位账号行内注明原因、不发请求。
#[tauri::command(async)]
pub fn trae_expired_ents(state: State<AppState>) -> Result<TraeExpiredEntsResult, String> {
    let now_ms = chrono::Local::now().timestamp_millis();
    let agent = crate::commands::accounts::pay_status_agent();
    let accounts = crate::vault::load_accounts(&state);
    let mut out: Vec<TraeExpiredEntsAccount> = Vec::new();
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
            continue;
        };
        let name = a.name.clone();
        // 占位账号（无 JWT）：不发请求，行内注明原因（原先静默跳过，前端无从感知，审查 P5）
        if a.jwt.trim().is_empty() {
            out.push(TraeExpiredEntsAccount {
                user_id: uid,
                name,
                ok: false,
                error: Some("无有效凭证（未登录），已跳过".to_string()),
                expired: Vec::new(),
            });
            continue;
        }
        // 接口无分页参数（实测响应 expired_ent_list 一次性返回），空 JSON 请求体
        let expired = match web_usage_post(&agent, &a.jwt, EXPIRED_ENTS_URL, json!({})) {
            Ok(resp) => resp
                .get("expired_ent_list")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .map(|e| TraeExpiredEnt {
                            name: e
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            credits_limit: e
                                .get("credits_limit")
                                .and_then(Value::as_f64)
                                .unwrap_or(0.0),
                            expire_time_ms: e
                                .get("expire_time_ms")
                                .and_then(Value::as_i64)
                                .unwrap_or(0),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Err(e) => {
                out.push(TraeExpiredEntsAccount {
                    user_id: uid,
                    name,
                    ok: false,
                    error: Some(e),
                    expired: Vec::new(),
                });
                continue;
            }
        };
        out.push(TraeExpiredEntsAccount {
            user_id: uid,
            name,
            ok: true,
            error: None,
            expired,
        });
    }
    Ok(TraeExpiredEntsResult {
        fetched_at: now_ms,
        accounts: out,
    })
}

/// 实现（本命令与调度器 `trae-usage-sync` / CLI `--task-run trae-usage-sync` 共用，issue #61）。
/// 失败分流（对齐 qoder_credits::run_snapshot_task 惯例）：全部有凭证账号拉取失败 → Err
/// （不覆盖 fetched_at，调度器 30 分钟冷却重试）；部分失败 → Ok（保留旧缓存逐账号注明错误）。
/// 消耗明细表级读改写互斥（审查 P2-3）：调度器定时任务（scheduler.rs）、GUI 手动刷新、
/// CLI `--task-run trae-usage-sync`（tasks/mod.rs）三路共用本实现，并发 fresh 拉取时
/// 整表 load→save 互相覆盖丢增量（CHANGELOG 曾声称修复，但此前仅 Buddy 侧落实）。
/// 持锁覆盖 fresh 路径 load→网络→save 全程；只读缓存路径不持锁，不被网络往返阻塞。
/// 叶子锁：持锁期间不得再获取其他锁。跨进程并发由 SQLite 事务保证单次落库原子性，
/// 极端场景最坏丢一次增量，下轮拉取按 last_fetch_end_ts 区间覆盖自愈。
static USAGE_HISTORY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn usage_history_fetch_impl(
    state: &AppState,
    fresh: bool,
) -> Result<UsageHistoryResult, String> {
    let now_ts = chrono::Local::now().timestamp();
    let accounts = crate::vault::load_accounts(state);

    // 纯缓存读取（零网络；尚未拉取过的账号如实提示）——不持锁
    if !fresh {
        let cache: CacheFile = serde_json::from_value(
            crate::store::docs::usage_history_load(&crate::store::db(&state.data_dir)),
        )
        .unwrap_or_default();
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
            accounts: out,
        });
    }

    // fresh 拉取：全程持表级互斥（load→网络→save；并发拉取串行化防丢增量）
    let _guard = USAGE_HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cache: CacheFile = serde_json::from_value(
        crate::store::docs::usage_history_load(&crate::store::db(&state.data_dir)),
    )
    .unwrap_or_default();

    // 增量拉取：无缓存账号全量近一年；已有账号从上次拉取日 00:00 重拉并替换该日及之后
    let full_start = now_ts - FULL_PULL_DAYS * 86400;
    let mut errors: BTreeMap<String, String> = BTreeMap::new();
    let mut attempted = 0usize;
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
            continue;
        };
        let name = a.name.clone();
        if a.jwt.trim().is_empty() {
            // 占位账号（无 JWT）：保留既有缓存，不发起请求
            continue;
        }
        attempted += 1;
        // 旧缓存回填（issue #82）：会话级明细上线前的缓存只有 daily、sessions 为空
        // （serde(default)），增量路径只重拉最近一天，历史会话行永不回填，前端
        // 「会话明细」永远空白。检测「daily 有值但 sessions 无行」→ 该账号一次性
        // 走全量重拉（refetch_from 置空整体替换），拉成功后 sessions 非空自然恢复增量。
        let legacy_no_sessions = cache
            .accounts
            .get(&uid)
            .map(|c| !c.daily.is_empty() && c.sessions.values().all(|v| v.is_empty()))
            .unwrap_or(false);
        let (start_ts, refetch_from) =
            match cache.accounts.get(&uid).and_then(|c| c.last_fetch_end_ts) {
                Some(last_end) if !legacy_no_sessions => {
                    let from_date = local_date_of(last_end)
                        .or_else(|| local_date_of(now_ts))
                        .unwrap_or_default();
                    let midnight = local_midnight_ts(&from_date).unwrap_or(now_ts - 86400);
                    (midnight, from_date)
                }
                _ => (full_start, String::new()),
            };
        match fetch_account_usage(&a.jwt, start_ts, now_ts) {
            Ok((new_agg, new_sessions)) => {
                let entry = cache.accounts.entry(uid.clone()).or_default();
                entry.name = name;
                apply_fetch_result(entry, new_agg, new_sessions, &refetch_from);
                entry.last_fetch_end_ts = Some(now_ts);
            }
            Err(e) => {
                // 拉取失败：保留旧缓存，错误在结果中注明
                errors.insert(uid, e);
            }
        }
    }

    // 全部有凭证账号拉取失败：暂态失败上抛（不覆盖 fetched_at、不落盘），
    // 交调度器 30 分钟冷却重试；部分失败仍 Ok（展示旧缓存 + 逐账号注明）
    if attempted > 0 && errors.len() == attempted {
        return Err(format!(
            "Trae 消耗明细拉取失败（{}/{} 账号）：{}",
            errors.len(),
            attempted,
            errors.values().next().map(String::as_str).unwrap_or("未知错误")
        ));
    }

    cache.fetched_at = Some(now_ts);
    let _ = serde_json::to_value(&cache)
        .map_err(|e| e.to_string())
        .and_then(|v| crate::store::docs::usage_history_save(&crate::store::db(&state.data_dir), &v));

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
        accounts: out,
    })
}

/// 拉取结果落库（全量整体替换 / 增量替换 refetch_from 及之后的本地日）。
/// daily 与 sessions 同语义：同日重拉不叠加（issue #61 增量语义对会话行同样适用）。
fn apply_fetch_result(
    entry: &mut CachedAccount,
    mut new_agg: BTreeMap<String, UsageDayStat>,
    new_sessions: BTreeMap<String, Vec<UsageSessionRow>>,
    refetch_from: &str,
) {
    // date 从映射键回填（防上游遗漏导致 daily.date 为空、前端区间匹配失败）
    for (d, v) in new_agg.iter_mut() {
        if v.date.is_empty() {
            v.date = d.clone();
        }
    }
    if refetch_from.is_empty() {
        // 全量：整体替换
        entry.daily = new_agg;
        entry.sessions = new_sessions;
    } else {
        // 增量：替换 refetch_from 及之后的日聚合/会话行（当天多次拉取不叠加）
        entry.daily.retain(|d, _| d.as_str() < refetch_from);
        for (d, v) in new_agg {
            entry.daily.insert(d, v);
        }
        entry.sessions.retain(|d, _| d.as_str() < refetch_from);
        for (d, v) in new_sessions {
            entry.sessions.insert(d, v);
        }
    }
}

/// 会话明细视图行（缓存行 + 账号信息拼装）
#[derive(Serialize, Clone, Default)]
pub struct UsageSessionView {
    pub user_id: String,
    pub account_name: String,
    pub session_id: String,
    /// usage_time（Unix 秒）
    pub ts: i64,
    /// 本地自然日（YYYY-MM-DD）
    pub date: String,
    pub model: String,
    pub mode: String,
    /// 失败行结构化标记（3.8.5 审查 L6：明细行失败徽标此前依赖
    /// `mode.contains("失败")` 字符串耦合，标签措辞变更会静默失效）。
    /// custom 会话明细写入真实值；Qoder/Buddy 侧无此语义，None 且不序列化
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
    pub credits: f64,
    pub cost_money: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub preview: String,
    /// 轮键（Qoder 侧 chat_record_id；Trae/Buddy 上游无此字段恒空）。
    /// 同一段消息 + 同一模型 + 同一输出上限派生同值——「展开会话看各轮」分组用
    pub record_id: String,
}

#[derive(Serialize, Clone, Default)]
pub struct UsageSessionsResult {
    pub fetched_at: i64,
    /// 过滤后总行数（分页前）
    pub total: u64,
    /// 过滤后区间总积分（分页前）
    pub credits_total: f64,
    pub page: u32,
    pub page_size: u32,
    pub rows: Vec<UsageSessionView>,
    /// 区间内出现过的模型清单（筛选下拉用，不受 model 过滤影响）
    pub models: Vec<String>,
}

/// 会话明细查询（纯缓存零网络；issue #82 会话明细 Tab）。
/// 区间 [start, end] 按本地日字符串（YYYY-MM-DD 字典序即时间序）比较；
/// model/user_id 可选过滤；按会话时间倒序分页返回。
#[tauri::command(async)]
pub fn usage_sessions_query(
    state: State<AppState>,
    start: String,
    end: String,
    model: Option<String>,
    user_id: Option<String>,
    page: Option<u32>,
    page_size: Option<u32>,
) -> Result<UsageSessionsResult, String> {
    let cache: CacheFile = serde_json::from_value(
        crate::store::docs::usage_history_load(&crate::store::db(&state.data_dir)),
    )
    .unwrap_or_default();

    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(50).clamp(1, 200);
    let model_filter = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let uid_filter = user_id.as_deref().map(str::trim).filter(|u| !u.is_empty());

    // 账号清单以 vault 为准（与聚合视图口径一致：已删除账号的缓存行不展示）
    let accounts = crate::vault::load_accounts(&state);
    let mut rows: Vec<UsageSessionView> = Vec::new();
    let mut models: BTreeSet<String> = BTreeSet::new();
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
            continue;
        };
        if uid_filter.is_some_and(|f| uid != f) {
            continue;
        }
        let Some(c) = cache.accounts.get(&uid) else {
            continue;
        };
        for (date, list) in &c.sessions {
            if date.as_str() < start.as_str() || date.as_str() > end.as_str() {
                continue;
            }
            for r in list {
                models.insert(r.model.clone());
                if model_filter.is_some_and(|m| r.model != m) {
                    continue;
                }
                rows.push(UsageSessionView {
                    user_id: uid.clone(),
                    account_name: c.name.clone(),
                    session_id: r.session_id.clone(),
                    ts: r.ts,
                    date: date.clone(),
                    model: r.model.clone(),
                    mode: r.mode.clone(),
                    failed: None,
                    credits: r.credits,
                    cost_money: r.cost_money,
                    input_tokens: r.input_tokens,
                    output_tokens: r.output_tokens,
                    cache_read_tokens: r.cache_read_tokens,
                    cache_write_tokens: r.cache_write_tokens,
                    preview: r.preview.clone(),
                    record_id: String::new(),
                });
            }
        }
    }

    // 新会话在前（同 ts 按 session_id 稳定排序）
    rows.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.session_id.cmp(&a.session_id)));
    let total = rows.len() as u64;
    let credits_total = rows.iter().map(|r| r.credits).sum();
    // 页码夹取：越界页回夹到最后一页（数据减少后翻页不再出现空页）；乘法 u64 饱和
    let total_pages = total.div_ceil(page_size as u64).max(1) as u32;
    let page = page.min(total_pages);
    let begin = ((page as u64 - 1).saturating_mul(page_size as u64)) as usize;
    let rows: Vec<UsageSessionView> = rows
        .into_iter()
        .skip(begin)
        .take(page_size as usize)
        .collect();

    Ok(UsageSessionsResult {
        fetched_at: cache.fetched_at.unwrap_or(0),
        total,
        credits_total,
        page,
        page_size,
        rows,
        models: models.into_iter().collect(),
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

    /// 回归（issue #82）：增量重拉当日时 sessions 行必须被替换而非叠加（与 daily 同语义）
    #[test]
    fn sessions_refetch_replaces_same_day_without_duplication() {
        let mut entry = CachedAccount::default();
        let mut old: BTreeMap<String, Vec<UsageSessionRow>> = BTreeMap::new();
        old.insert(
            "2026-09-09".into(),
            vec![UsageSessionRow {
                ts: 1,
                ..Default::default()
            }],
        );
        old.insert(
            "2026-09-10".into(),
            vec![
                UsageSessionRow {
                    ts: 2,
                    model: "m".into(),
                    ..Default::default()
                },
                UsageSessionRow {
                    ts: 3,
                    model: "m".into(),
                    ..Default::default()
                },
            ],
        );
        entry.sessions = old;

        let mut new_agg = BTreeMap::new();
        new_agg.insert("2026-09-10".into(), UsageDayStat::default());
        let mut new_sessions: BTreeMap<String, Vec<UsageSessionRow>> = BTreeMap::new();
        new_sessions.insert(
            "2026-09-10".into(),
            vec![UsageSessionRow {
                ts: 4,
                ..Default::default()
            }],
        );

        apply_fetch_result(&mut entry, new_agg, new_sessions, "2026-09-10");
        // 当日重拉：旧行被替换，不叠加
        assert_eq!(entry.sessions["2026-09-10"].len(), 1);
        assert_eq!(entry.sessions["2026-09-10"][0].ts, 4);
        // 更早日期保持不动
        assert_eq!(entry.sessions["2026-09-09"].len(), 1);
        assert_eq!(entry.daily["2026-09-10"].date, "2026-09-10");
    }

    /// 回归：会话说明截断 200 字符（防异常长文本撑爆缓存与表格）
    #[test]
    fn session_preview_truncated_to_200_chars() {
        let long = "x".repeat(500);
        assert_eq!(truncate_preview(&long).chars().count(), 200);
        assert_eq!(truncate_preview("短文本"), "短文本");
        assert_eq!(truncate_preview(""), "");
    }

    /// 回归（T1.5）：会话积分字段映射——credits_float 优先、旧版 amount_float 兜底、
    /// 双缺/非数值为 0（不伪造积分）
    #[test]
    fn session_credits_prefers_credits_float_with_amount_fallback() {
        let credits_first = json!({ "credits_float": 1.25, "amount_float": 9.0 });
        assert!((extract_session_credits(&credits_first) - 1.25).abs() < 1e-9);
        let amount_only = json!({ "amount_float": 0.5 });
        assert!((extract_session_credits(&amount_only) - 0.5).abs() < 1e-9);
        let neither = json!({ "model_name": "m" });
        assert_eq!(extract_session_credits(&neither), 0.0);
        let empty = json!({});
        assert_eq!(extract_session_credits(&empty), 0.0);
        // 非数值类型（字符串/null）不参与兜底链
        let bad_type = json!({ "credits_float": null, "amount_float": "3" });
        assert_eq!(extract_session_credits(&bad_type), 0.0);
    }

    /// 回归（T1.5）：会话行字段映射——上游 JSON → UsageSessionRow 安全默认值
    /// （session_id/mode/preview 缺省空串、token 缺省 0、model 缺省「未知模型」语义由调用方兜底）
    #[test]
    fn session_row_defaults_from_sparse_upstream_payload() {
        let s = json!({
            "usage_time": 1_789_009_361i64,
            "credits_float": 2.0,
            "extra_info": { "input_token": 10, "output_token": 5 }
        });
        let tok = |k: &str| -> u64 {
            s.get("extra_info")
                .and_then(|e| e.get(k))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        let row = UsageSessionRow {
            session_id: s.get("session_id").and_then(Value::as_str).unwrap_or_default().to_string(),
            ts: s.get("usage_time").and_then(Value::as_i64).unwrap_or(0),
            model: String::new(),
            mode: s.get("mode").and_then(Value::as_str).unwrap_or_default().to_string(),
            credits: extract_session_credits(&s),
            cost_money: s.get("cost_money_float").and_then(Value::as_f64).unwrap_or(0.0),
            input_tokens: tok("input_token"),
            output_tokens: tok("output_token"),
            cache_read_tokens: tok("cache_read_token"),
            cache_write_tokens: tok("cache_write_token"),
            preview: s.get("user_input_preview").and_then(Value::as_str).map(truncate_preview).unwrap_or_default(),
        };
        assert_eq!(row.ts, 1_789_009_361);
        assert!((row.credits - 2.0).abs() < 1e-9);
        assert_eq!(row.input_tokens, 10);
        assert_eq!(row.output_tokens, 5);
        assert_eq!(row.cache_read_tokens, 0);
        assert_eq!(row.cache_write_tokens, 0);
        assert!(row.session_id.is_empty());
        assert!(row.mode.is_empty());
        assert!(row.preview.is_empty());
        assert_eq!(row.cost_money, 0.0);
    }
}
