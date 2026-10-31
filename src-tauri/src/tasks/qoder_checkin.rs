//! Qoder 签到引擎（F-80 M1 核心，对照 wb_checkin.rs 骨架裁剪）。
//!
//! 端点（sash 活动体系，§2.2/§2.3）：
//! - `GET {open_api}/sash/api/v1/me/campaigns`：活动列表，双活动自然全覆盖
//! - `POST {open_api}/sash/api/v1/me/campaigns/{campaignId}/claim`：**幂等**领取
//!   （重复调用返回 `data.status=="CLAIMED"` + `data.replayed==true`，无副作用）
//!
//! NDJSON 事件契约（`qoder-checkin-progress` 管线，前端逐行 JSON.parse，
//! 与 Buddy 签到前端组件同构）：start {type,total} / account {user_id,name,
//! status,message[,reward],index} / done {type:"done",ok,already,failed}。
//!
//! 调度设计结论（§2.2）：每日 10:15 单次调度同时覆盖「0 点签到」与「10:00 登录奖励」
//! 双活动；失败进入 30 分钟起步的指数退避重试冷却（scheduler retry_cooldown_ms 同源，
//! 连续失败 30→60→120 分钟封顶）。
//!
//! 风控合规内建（§5.2）：claim 间隔 1~3s 抖动；绝不重试轰炸；claim POST 与列表
//! 补学统一最小头形态（issue #114：claim 端点带设备指纹头族被上游风控按形态
//! 拦截 503 RISK_DEPENDENCY_UNAVAILABLE，最小头通过——探针 + 4/4 账号到账实证）；
//! 主列表/余额 GET 维持完整头主请求形态。
//! 红线：全程零 token 输出（凭证不入日志/事件）。

use serde_json::{json, Value};

use crate::fs_utils;
use crate::state::AppState;

use super::http_agent;
use super::qoder_common;

/// 签到轮次参数（对齐 wb_checkin::CheckinOpts；skip_checked 保留契约字段）
#[derive(Clone, Default)]
pub struct QoderCheckinOpts {
    pub uids: Vec<String>,
    #[allow(dead_code)]
    pub skip_checked: bool,
    pub lazy_hours: i64,
}

impl QoderCheckinOpts {
    /// 每日计划任务默认（skip_checked，lazy 24h）
    pub fn daily() -> Self {
        Self {
            uids: vec![],
            skip_checked: true,
            lazy_hours: 24,
        }
    }
}

/// 签到/成长轮次全局锁（对照 WB_ROUND_LOCK；应用内调度器与 UI 路径互斥）。
/// tokio Mutex try_lock 拿不到即拒绝，不排队不阻塞。
static QODER_ROUND_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 入口尝试获取轮次锁；guard 移交工作线程并持有至轮次结束（RAII 防泄漏）。
pub(crate) fn try_acquire_qoder_round() -> Result<tokio::sync::MutexGuard<'static, ()>, String> {
    QODER_ROUND_LOCK
        .try_lock()
        .map_err(|_| "已有 Qoder 签到任务在执行中，请等待当前轮次完成".to_string())
}

// ── 端点表（R-4/R-7 抓包固化；常量集中可改）────────────────────────────────

struct QoderUrls {
    campaigns: String,
}

fn urls_for() -> QoderUrls {
    let b = qoder_common::OPEN_API_BASE;
    QoderUrls {
        campaigns: format!("{b}/sash/api/v1/me/campaigns"),
    }
}

// ── 解析辅助（对齐 wb_checkin 同名模式）─────────────────────────────────────

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

/// claim 间隔抖动（1~3s；审查 L-jitter：原时间戳取模可预测且同毫秒调用序列相同，
/// 改为 SystemTime 纳秒 + 栈地址熵播种的 xorshift64*，跨次调用链式推进——
/// 风控抖动仅需不可预测性而非密码学强度，不引新依赖）
fn jitter_sleep() {
    static STATE: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if *s == 0 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() ^ u64::from(d.subsec_nanos()))
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        // 地址熵：ASLR 下每次进程启动不同；|1 保证非零（xorshift 零态吸收）
        let addr = std::ptr::from_ref::<std::sync::Mutex<u64>>(&STATE) as u64;
        *s = (nanos ^ addr.rotate_left(17)) | 1;
    }
    let mut x = *s;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *s = x;
    let ms = 1000 + x.wrapping_mul(0x2545_F491_4F6C_DD1D) % 2000;
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// POST 空 body（R-4 抓包实测：claim 请求 Content-Length: 0，非 JSON `{}`）。
/// 返回 (http_status, parsed, raw_text)；status=0 网络不可达。
/// 红线（响应体读取失败禁吞空串）：读取失败时 raw 携带 `<响应体读取失败: …>`
/// 标记——Ok 分支原样返回真实 status（原硬编码 200 会把 201/204 记失真），
/// 供 claim_one 区分「空响应」与「读取失败」并保留诊断信息。
fn post_empty(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
) -> (u16, Option<Value>, String) {
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(resp) => {
            let code = resp.status();
            let raw = resp
                .into_string()
                .unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            (code, serde_json::from_str(&raw).ok(), raw)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp
                .into_string()
                .unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            (code, serde_json::from_str(&raw).ok(), raw)
        }
        Err(_e) => (0, None, String::new()),
    }
}

// ── 结果存储 ───────────────────────────────────────────────────────────────

/// 签到结果 90 天滚动存储（趋势/日志数据源；SQLite 化：qoder_checkin_results 表）。
/// 写入为逐条 UPSERT（pk = date|user_id|time，内容派生）：原「整表 load→save」
/// 在计划任务与 UI 并发触发时互相覆盖丢记录（数组下标 pk 冲突）；90 天裁剪内置于
/// docs::qoder_checkin_results_upsert（逐 pk DELETE，不触碰新写入）。
fn append_results(state: &AppState, events: &[Value]) {
    let store = crate::store::db(&state.data_dir);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    for ev in events {
        let mut rec = json!({
            "date": today,
            "time": fs_utils::now_ts(),
            // pk 去重源：毫秒时间戳（now_ts 秒级，同账号同秒两进程并发写入会碰撞互覆）
            "time_ms": chrono::Utc::now().timestamp_millis(),
            "user_id": ev.get("user_id").cloned().unwrap_or_default(),
            "name": ev.get("name").cloned().unwrap_or_default(),
            "status": ev.get("status").cloned().unwrap_or_default(),
            "message": ev.get("message").cloned().unwrap_or_default(),
        });
        if let Some(r) = ev.get("reward").filter(|r| !r.is_null()) {
            rec["reward"] = r.clone();
        }
        // 逐活动明细（F-80-余 v2 档期日历数据源；历史记录无此字段前端做兼容）
        if let Some(c) = ev.get("campaigns").filter(|c| c.is_array()) {
            rec["campaigns"] = c.clone();
        }
        if let Err(e) = crate::store::docs::qoder_checkin_results_upsert(&store, &rec) {
            fs_utils::app_log(&state.data_dir, &format!("[qoder] 签到结果落库失败: {e}"));
        }
    }
}

// ── 签到主流程 ─────────────────────────────────────────────────────────────

/// empty_campaigns 失败标记（结构化 fail_kind 的唯一产源标记）。
/// 产源 = list_campaigns 空活动分支（message 以本常量开头）；诊断日志与 fail_kind
/// 均按 `starts_with(EMPTY_CAMPAIGNS_TAG)` 派生——调整「：」后的展示文案不影响
/// 统计，更换标记本身只需改本常量一处（勿在消费侧绕过常量硬编码前缀）。
const EMPTY_CAMPAIGNS_TAG: &str = "empty_campaigns";

/// 永久性认证失败标记（fail_kind 产源标记，与 EMPTY_CAMPAIGNS_TAG 同模式）：
/// pat_rejected / expired_needs_relogin / auth_dead 前置拦截分支产出——
/// 重试注定失败，调度器按 failed - failed_empty - failed_permanent 判定
/// 是否冷却重试，避免全天约 28 轮无效重试（审查 minor）
const PERMANENT_AUTH_TAG: &str = "permanent_auth";

/// 409 过期 id 特征码（describe_blind_failure 的 409 分支、blind_round 的 stale
/// 剔除与 fallback_with_relearn 的同轮补学重试共用，勿在消费侧绕开常量硬编码）：
/// 上游 claim 对过期活动返回 `{"code":{"code":"CAMPAIGN_NOT_ACTIVE",...}}`。
const CAMPAIGN_NOT_ACTIVE_TAG: &str = "CAMPAIGN_NOT_ACTIVE";

/// 拉取活动列表原始数据（**全量，不过滤，允许空列表**）。
/// 返回 Ok(campaign 列表，可为空)；Err(kind, message)：auth（401）| fail。
/// 与 [`list_campaigns`] 的区别：不把空列表判为 fail——盲发兜底当轮重学
///（issue #105）需要「列表为空 → 学不到 → 放弃重学」而非「列表为空 → 整账号失败」。
fn fetch_campaigns(
    state: &AppState,
    agent: &ureq::Agent,
    headers: &[(String, String)],
    urls: &QoderUrls,
) -> Result<Vec<Value>, (String, String)> {
    let (status, body, raw) = qoder_common::get_json(agent, &urls.campaigns, headers);
    if status == 401 {
        return Err(("auth".into(), "登录态失效（401）".into()));
    }
    if status == 0 {
        let head: String = raw.chars().take(120).collect();
        return Err(("fail".into(), format!("网络不可达: {head}")));
    }
    if !(200..=201).contains(&status) {
        return Err(("fail".into(), format!("campaigns 不可用（HTTP {status}）")));
    }
    let Some(b) = body.filter(|b| b.is_object()) else {
        return Err(("fail".into(), "campaigns 响应非 JSON".into()));
    };
    // 信封穿透（fs_utils::dig 含 data 包裹下钻）
    let campaigns = fs_utils::dig(&b, &["campaigns"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 空表诊断（2026-10-21 起 CN 侧持续空表，issue #132）：记录 HTTP 状态、
    // campaigns 键是否存在、响应体头部——用于区分「真空数组」「键缺失/改名」
    // 「信封错误」。响应体为活动元数据，不含任何凭证。
    if campaigns.is_empty() {
        let head: String = raw.chars().take(400).collect();
        let has_key = fs_utils::dig(&b, &["campaigns"]).is_some();
        fs_utils::app_log(
            &state.data_dir,
            &format!("[qoder] campaigns 空表: http={status} campaigns_key={has_key} body={head}"),
        );
    }
    Ok(campaigns)
}

/// 拉取活动列表（**全量，不过滤**）。返回 Ok(全量 campaign 列表)；
/// Err(kind, message)：auth（401，调用方刷新重试）| fail。
/// 全量返回的原因：列表内 CLAIMED grant 是「每日奖励已领取」的判定数据源
///（daily_credits_claimed_in_list），CLAIMABLE 过滤交给调用方（filter_claimable）。
fn list_campaigns(
    state: &AppState,
    agent: &ureq::Agent,
    headers: &[(String, String)],
    urls: &QoderUrls,
) -> Result<Vec<Value>, (String, String)> {
    let campaigns = fetch_campaigns(state, agent, headers, urls)?;
    // ⚠ campaigns 为空 ≠ 已签：可能活动未开始 / Cosy-ClientType 缺失（本层
    // 恒带）/ 接口结构变更——与「无可领项」显式区分，kind=fail 触发 UI 提示（§5.2）
    if campaigns.is_empty() {
        return Err((
            "fail".into(),
            format!("{EMPTY_CAMPAIGNS_TAG}：活动列表为空（活动未开始或不可用）"),
        ));
    }
    Ok(campaigns)
}

/// 拉活动列表（完整头 → 空表则最小头复取一次）。
///
/// 上游对 campaigns 列表按**请求形态**分桶投放：完整设备头族形态可能返回
/// `showCampaign=false` + `campaigns: []`（HTTP 200），而同刻最小头形态可见
/// 活动条目（2026-10-14、2026-10-21 两次实测，issue #132）。[`list_campaigns`]
/// 把空数组判为 fail，而最小头补学 `learn_from_min_headers` 写在
/// `process_account` 的 Ok 分支内——空表走 Err，补学从未执行，故此处兜一层。
///
/// 错误口径：复取自身失败（401 / 429 / 5xx / 网络）时**透传其错误**，不伪装成
/// empty_campaigns——否则 401 进不了凭证刷新自愈，暂态故障还会被调度器按
/// `failed_empty_campaigns` 排除当日重试。仅当复取同样为空表时才回退到首次的
/// 空表错误。
///
/// 返回值第二项 = 列表是否来自最小头复取：调用方据此跳过 `learn_from_min_headers`
/// 的重复补学（否则同一 token/头/URL 会立刻再 GET 一次）。
fn list_campaigns_with_fallback(
    state: &AppState,
    agent: &ureq::Agent,
    headers: &[(String, String)],
    min_token: &str,
    urls: &QoderUrls,
    aid: &str,
) -> Result<(Vec<Value>, bool), (String, String)> {
    match list_campaigns(state, agent, headers, urls) {
        Err((k, m)) if m.starts_with(EMPTY_CAMPAIGNS_TAG) => {
            match list_campaigns(state, agent, &minimal_auth_headers(min_token), urls) {
                Ok(c) => {
                    fs_utils::app_log(
                        &state.data_dir,
                        &format!("[qoder] 完整头空表、最小头复取得 {} 条活动（{aid}）", c.len()),
                    );
                    Ok((c, true))
                }
                Err((_, m2)) if m2.starts_with(EMPTY_CAMPAIGNS_TAG) => {
                    fs_utils::app_log(
                        &state.data_dir,
                        &format!("[qoder] 完整头与最小头均返回空活动列表（{aid}）"),
                    );
                    Err((k, m))
                }
                Err((k2, m2)) => {
                    // 复取自身故障（401 / 429 / 5xx / 网络）：如实透传，保留
                    // 刷新自愈与当日重试资格
                    fs_utils::app_log(
                        &state.data_dir,
                        &format!("[qoder] 最小头复取失败（{k2}）: {m2}（{aid}）"),
                    );
                    Err((k2, m2))
                }
            }
        }
        other => other.map(|c| (c, false)),
    }
}

/// 过滤 claimStatus==CLAIMABLE（大小写宽容，纯函数可单测），双活动自然全覆盖。
fn filter_claimable(campaigns: &[Value]) -> Vec<Value> {
    campaigns
        .iter()
        .filter(|c| {
            let st =
                s_of(fs_utils::dig(c, &["claimStatus", "claim_status"])).to_ascii_uppercase();
            st == "CLAIMABLE"
        })
        .cloned()
        .collect()
}

/// 「每日 100 Credits」已领判定（纯函数，**从严**口径——宁 fail 不假 already）：
/// 列表中存在同时满足以下**全部**条件的条目才算已领：
/// - claimStatus == CLAIMED（大小写宽容）
/// - actionType == CLAIM_BENEFIT（排除 bogo 类 VIEW_DETAILS「只看不领」活动）
/// - benefit.kind == CREDITS（每日 100 Credits 活动的奖励类型）
/// - startAt <= now <= endAt（当日窗口内；字段缺失视为不命中——昨日的
///   CLAIMED grant 无条件展示在列表里，没有窗口校验会把昨日已领误判今日已领）
fn daily_credits_claimed_in_list(campaigns: &[Value], now_secs: i64) -> bool {
    campaigns.iter().any(|c| {
        let claimed =
            s_of(fs_utils::dig(c, &["claimStatus", "claim_status"])).to_ascii_uppercase()
                == "CLAIMED";
        let action = s_of(c.get("actionType")).to_ascii_uppercase() == "CLAIM_BENEFIT";
        let credits = c
            .get("benefit")
            .and_then(|x| x.get("kind"))
            .map(|v| s_of(Some(v)).to_ascii_uppercase() == "CREDITS")
            .unwrap_or(false);
        let start = c.get("startAt").and_then(|v| num_or_none(Some(v))).map(|v| v as i64);
        let end = c.get("endAt").and_then(|v| num_or_none(Some(v))).map(|v| v as i64);
        let in_window = match (start, end) {
            (Some(s), Some(e)) => now_secs >= s && now_secs <= e,
            _ => false,
        };
        claimed && action && credits && in_window
    })
}

/// 复查判定（纯函数，可单测）：campaigns 列表中是否存在 campaignId 匹配且
/// claimStatus 已变 CLAIMED 的条目（字段语义与 filter_claimable 过滤同构）。
fn campaign_is_claimed(campaigns: &[Value], campaign_id: &str) -> bool {
    campaigns.iter().any(|c| {
        let cid = s_of(fs_utils::dig(c, &["campaignId", "campaign_id"]));
        cid == campaign_id
            && s_of(fs_utils::dig(c, &["claimStatus", "claim_status"]))
                .to_ascii_uppercase()
                == "CLAIMED"
    })
}

/// 多活动领取结果归并（纯函数，可单测）：success > fail > already——
/// 任一 success → success；否则任一 fail → fail（保证失败触发调度器 30 分钟重试）；
/// 否则 already。原实现「首个非 success 定型不互相覆盖」使 [already, fail] 归并为
/// already，掩盖真实失败（全天不再重试）。
fn merge_claim_kind(acc: &str, next: &str) -> String {
    if acc == "success" || next == "success" {
        return "success".into();
    }
    if acc == "fail" || next == "fail" {
        return "fail".into();
    }
    if acc.is_empty() {
        return next.to_string();
    }
    acc.to_string()
}

/// claim 网络异常后复查恢复（M1 任务，recon §3.2 cpa-multi-plugins 方案）：
/// claim 请求可能已到达服务端但响应丢失（超时/断连），重新 GET campaigns 检查
/// 该活动是否已变 CLAIMED——GET 幂等安全，绝不重发 claim（避免重复领取副作用）。
fn recheck_claimed(
    agent: &ureq::Agent,
    headers: &[(String, String)],
    urls: &QoderUrls,
    campaign_id: &str,
) -> bool {
    let (status, body, _) = qoder_common::get_json(agent, &urls.campaigns, headers);
    if !(200..=201).contains(&status) {
        return false;
    }
    let Some(b) = body.filter(|b| b.is_object()) else {
        return false;
    };
    let campaigns = fs_utils::dig(&b, &["campaigns"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    campaign_is_claimed(&campaigns, campaign_id)
}

/// 单个 campaign 领取。返回 (kind, message, reward)：success / already / fail。
/// 幂等：重复调用 replayed=true 归类 already（非错误）。
/// claim POST 与网络异常复查 GET 均用 `minimal_auth_headers(access_token)` 最小头
/// 形态（issue #114）：claim 端点带设备指纹头族被上游风控按形态拦截（503
/// RISK_DEPENDENCY_UNAVAILABLE），最小头通过（同 IP 同 token 同 id 探针 + 4/4
/// 账号到账实证）。
fn claim_one(
    agent: &ureq::Agent,
    access_token: &str,
    urls: &QoderUrls,
    campaign: &Value,
) -> (String, String, Option<f64>) {
    let campaign_id = s_of(fs_utils::dig(campaign, &["campaignId", "campaign_id"]));
    if campaign_id.is_empty() {
        return ("fail".into(), "campaign 缺少 campaignId".into(), None);
    }
    // campaign_id path 段白名单（审查 L-URL 转义：id 来自服务端响应，恶意/异常值
    // 携带 / ? # 等字符会改变请求路径语义）。项目无 URL 编码依赖，按「只放行
    // [A-Za-z0-9_-] 否则拒绝该活动」处理——合法 id（UUID/短横线串）不受影响
    if !campaign_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return (
            "fail".into(),
            format!("campaignId 含非法字符，已拒绝（len={}）", campaign_id.len()),
            None,
        );
    }
    // 奖励数额以接口返回为准（campaigns.benefit.amount 优先；claim 响应 benefit.amount 兜底——
    // R-9 抓包实测：claim 成功响应顶层含完整 benefit{kind,amount,validity}）。
    // 显式路径取值（dig 为候选键语义，不按路径下钻）
    let known_reward = campaign
        .get("benefit")
        .and_then(|x| x.get("amount"))
        .and_then(|v| num_or_none(Some(v)));
    let url = format!(
        "{}/sash/api/v1/me/campaigns/{}/claim",
        qoder_common::OPEN_API_BASE,
        campaign_id
    );
    // 最小头形态（issue #114，见函数注释）：本函数内 claim POST 与复查 GET 共用
    let headers = minimal_auth_headers(access_token);
    jitter_sleep();
    let (status, body, raw) = post_empty(agent, &url, &headers);
    if status == 401 {
        return ("auth".into(), "登录态失效（401）".into(), None);
    }
    if status == 0 {
        // 网络不可达 ≠ 必然失败：claim 可能已到达服务端但响应丢失，
        // 复查 campaigns（GET 幂等安全），已变 CLAIMED 视为已领（M1 复查恢复）
        if recheck_claimed(agent, &headers, urls, &campaign_id) {
            return (
                "already".into(),
                "claim 网络异常，复查确认已领取".into(),
                known_reward,
            );
        }
        return ("fail".into(), "网络不可达（claim）".into(), None);
    }
    // 响应体读取失败标记（红线）：2xx 下服务端已受理，维持宽容成功语义
    //（重试只会触发幂等回放，复查亦可恢复），但 message 携带失败信息入
    // 落库/事件供排障；非 2xx 由末尾 fail 分支 raw 兜底
    let read_failed = raw.starts_with("<响应体读取失败");
    let replayed = body
        .as_ref()
        .and_then(|b| fs_utils::dig(b, &["replayed", "data.replayed"]))
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false);
    let claimed_status = body
        .as_ref()
        .and_then(|b| fs_utils::dig(b, &["status", "data.status"]))
        .map(|v| s_of(Some(v)).to_ascii_uppercase());
    if (200..=201).contains(&status) {
        // claim 响应顶层 benefit.amount（R-9 抓包实测含完整 benefit{kind,amount,validity}）
        let body_reward = body
            .as_ref()
            .and_then(|b| b.get("benefit"))
            .and_then(|x| x.get("amount"))
            .and_then(|v| num_or_none(Some(v)));
        let reward = known_reward.or(body_reward);
        if replayed {
            // 幂等回放：无副作用，归类 already（非错误）
            return ("already".into(), "今日已领取（幂等回放）".into(), reward);
        }
        if claimed_status.as_deref() == Some("CLAIMED") {
            return ("success".into(), "领取成功".into(), reward);
        }
        // 200 但无明确状态：宽容视为成功（响应结构 R-9 已固化，仍保留兜底）；
        // 响应体读取失败（replayed/status 均不可判）同样维持成功但携带诊断信息
        if read_failed {
            return ("success".into(), format!("领取成功（{raw}）"), reward);
        }
        return ("success".into(), "领取成功".into(), reward);
    }
    let msg = body
        .as_ref()
        .and_then(|b| fs_utils::dig(b, &["message", "msg"]))
        .map(|v| s_of(Some(v)))
        .filter(|m| !m.is_empty())
        .or_else(|| {
            // 服务端错误文案缺失时取 raw 头部兜底（含「响应体读取失败」标记，
            // 不再退化为干瘪的「claim 失败（HTTP xxx）」）
            let head: String = raw.chars().take(160).collect();
            (!head.is_empty()).then_some(head)
        });
    (
        "fail".into(),
        msg.unwrap_or_else(|| format!("claim 失败（HTTP {status}）")),
        None,
    )
}

// ── 已知每日活动兜底直领 ────────────────────────────────────────────────────
//
// 根因（探针 probe_sash_campaigns_headers 实证）：campaigns 列表端点按请求
// 形态分桶投放——「带设备指纹头族」的容器请求形态看不到 CLAIMABLE 条目（已领
// grant 无条件展示），列表零可领项 → 依赖盲发兜底；而 claim 端点不受列表过滤
// 约束（幂等，重复领取返回 replayed=true 回放）。
//
// ⚠ 活动批次轮换：每日 100 Credits 的 campaignId 随活动批次更换（campaignKey
// 形如 act-YYYYMMDD-NNN，一批数日窗口，同批内 id 不变）。过期 ID 盲发得到
// 409 CAMPAIGN_NOT_ACTIVE——经 classify_blind_step 如实归 Failed（宁 fail 不假
// already），不再静默归 already（2026-10-07「已领（此前已领）」误报的根因）。
//
// 2026-10-14 全天 409 事故固化——过期 id 由三路自动/半自动补正（候选优先级
// conf 覆盖表 > 当日重学 > 本表）：
//   ① learn_daily_campaign_ids：从当日活动列表重学（列表含可学条目时可用）；
//   ② learn_from_min_headers：最小头补学（只带鉴权四件套，不带设备指纹头族）
//      ——10-14 探针实证同 IP 同 token 下，最小头请求形态可见 CLAIMABLE 条目，
//      而带派生设备头的容器形态全天不可见（当日 409 全败的直接根因）；
//   ③ conf 覆盖表 <conf>/qoder_campaigns.json：免重编译人工下发（格式见
//      parse_conf_campaigns），上游换批次而①②失效时改文件即可。
// 本表为最后人工兜底（①②③皆失效时）；更新时直接替换下方条目。
/// 已知每日活动常量表：(campaignId, 展示名)。campaignId 仅允许 [A-Za-z0-9_-]
///（路径安全白名单，known_daily_fallback 内强制校验）。
/// 当前条目 = act-20261013-603（2026-10-14 探针实测，窗口 10-14 10:00 →
/// 10-15 10:29 CST，盲发直领 4/4 账号 200+CLAIMED+replayed=false）。
const KNOWN_DAILY_CAMPAIGNS: &[(&str, &str)] = &[
    ("01a130ea-7c31-71b3-a22b-cbe019571350", "每日领 100 Credits"),
];

/// 从活动条目提取 (campaignId, 展示名)（id 过路径白名单 [A-Za-z0-9_-]，非法
/// 丢弃；展示名取 name/title/campaignName/campaign_name 候选键，缺失回退 id 前 8 位）。
/// 各学习档位共用的提取口径。
fn learned_entry_from(c: &Value) -> Option<(String, String)> {
    let cid = s_of(fs_utils::dig(c, &["campaignId", "campaign_id"]));
    if cid.is_empty()
        || !cid.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    let cname = s_of(fs_utils::dig(
        c,
        &["name", "title", "campaignName", "campaign_name"],
    ));
    let cname = if cname.is_empty() { cid.chars().take(8).collect() } else { cname };
    Some((cid, cname))
}

/// 从活动列表学习当日「每日 100 Credits」活动 id（issue #66 健壮性①：每日轮换
/// 的 campaignId 硬编码必然过期，逐日人工抓包更新不可持续）。
///
/// 可行性依据（probe_sash_campaigns_headers 探针实证）：campaigns 列表端点对
/// 工具请求形态过滤的是 **CLAIMABLE 状态**，活动条目本身（actionType/benefit/
/// 窗口/campaignId）照常返回（已领 grant 无条件展示即是例证），足以反推出当日 id。
///
/// 口径（对齐 daily_credits_claimed_in_list 但**不要求 claimStatus**——
/// CLAIMED/CLAIMABLE/未领均为有效候选，保证零 CLAIMABLE 时仍能学到 id）：
/// actionType == CLAIM_BENEFIT && benefit.kind == CREDITS && startAt <= now <= endAt。
fn learn_daily_campaign_ids(campaigns: &[Value], now_secs: i64) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for c in campaigns {
        let action = s_of(c.get("actionType")).to_ascii_uppercase() == "CLAIM_BENEFIT";
        let credits = c
            .get("benefit")
            .and_then(|x| x.get("kind"))
            .map(|v| s_of(Some(v)).to_ascii_uppercase() == "CREDITS")
            .unwrap_or(false);
        let start = c.get("startAt").and_then(|v| num_or_none(Some(v))).map(|v| v as i64);
        let end = c.get("endAt").and_then(|v| num_or_none(Some(v))).map(|v| v as i64);
        let in_window = match (start, end) {
            (Some(s), Some(e)) => now_secs >= s && now_secs <= e,
            _ => false,
        };
        if !(action && credits && in_window) {
            continue;
        }
        if let Some(entry) = learned_entry_from(c) {
            merge_learned(&mut out, vec![entry]);
        }
    }
    out
}

/// 宽松档学习（issue #105：服务端字段口径变更/窗口缺失时严格档学不到 id，
/// 盲发兜底只剩过期硬编码表 → 全天 409 死循环）。仅作严格档产空时的兜底，
/// 学习错 id 的代价是一次 409（已归 stale_parts：不掩码成功、剔除候选、
/// 触发重试），风险可控。分档（取**首个非空档**，宁缺勿滥）：
/// - 档 A：actionType == CLAIM_BENEFIT && benefit.kind == CREDITS（去窗口要求——
///   服务端省略/改用其他窗口字段形态时仍可学）
/// - 档 B：benefit.kind == CREDITS（去 actionType 要求——覆盖 actionType 枚举变更）
/// - 档 C：actionType == CLAIM_BENEFIT && 展示名含「每日」（覆盖 benefit.kind
///   也变更的极端形态；「每日」限定避免撞上无关活动）
fn learn_daily_campaign_ids_lenient(campaigns: &[Value]) -> Vec<(String, String)> {
    let is_credits = |c: &Value| {
        c.get("benefit")
            .and_then(|x| x.get("kind"))
            .map(|v| s_of(Some(v)).to_ascii_uppercase() == "CREDITS")
            .unwrap_or(false)
    };
    let is_claim_benefit =
        |c: &Value| s_of(c.get("actionType")).to_ascii_uppercase() == "CLAIM_BENEFIT";
    let learn = |pred: &dyn Fn(&Value) -> bool| -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for c in campaigns {
            if pred(c) {
                if let Some(entry) = learned_entry_from(c) {
                    merge_learned(&mut out, vec![entry]);
                }
            }
        }
        out
    };
    let tier_a = learn(&|c| is_claim_benefit(c) && is_credits(c));
    if !tier_a.is_empty() {
        return tier_a;
    }
    let tier_b = learn(&is_credits);
    if !tier_b.is_empty() {
        return tier_b;
    }
    learn(&|c| {
        is_claim_benefit(c)
            && s_of(fs_utils::dig(
                c,
                &["name", "title", "campaignName", "campaign_name"],
            ))
            .contains("每日")
    })
}

/// 学习结果按 id 去重合并（纯函数，可单测）：同 id 保留先到条目（learned 优先）。
fn merge_learned(learned: &mut Vec<(String, String)>, found: Vec<(String, String)>) {
    for (id, name) in found {
        if !learned.iter().any(|(e, _)| e == &id) {
            learned.push((id, name));
        }
    }
}

// ── conf 覆盖表与最小头补学（2026-10-14 全天 409 事故固化，移植 PR #106）──────

/// conf 覆盖表解析（纯函数，可单测）。文件格式两种皆收：
///   [{"id": "...", "name": "每日领 100 Credits"}, ...]
///   {"campaigns": [...同上...]}
/// id 校验与路径白名单同口径（[A-Za-z0-9_-]，len 1..=128）；name 缺失回退 id
/// 前 8 位；解析失败/非法条目静默忽略（调用方 load_campaign_table 负责 app_log
/// 留痕，本层保持纯函数）。
fn parse_conf_campaigns(raw: &str) -> Vec<(String, String)> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let arr = match v {
        Value::Array(_) => Some(&v),
        Value::Object(_) => v.get("campaigns").filter(|c| c.is_array()),
        _ => None,
    };
    let Some(arr) = arr.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for c in arr {
        let Some(id) = fs_utils::dig(c, &["id", "campaignId", "campaign_id"])
            .and_then(Value::as_str)
            .map(str::trim)
        else {
            continue;
        };
        if id.is_empty()
            || id.len() > 128
            || !id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            continue;
        }
        let cname = fs_utils::dig(c, &["name", "title"])
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| id.chars().take(8).collect());
        out.push((id.to_string(), cname));
    }
    out
}

/// 常量表与 conf 条目合并（纯函数，可单测）：conf 在前（人工下发的最新 id 优先
/// 盲发），常量表按 id 去重补后。
fn merge_campaign_table(
    mut conf: Vec<(String, String)>,
    builtins: &[(&str, &str)],
) -> Vec<(String, String)> {
    for (id, name) in builtins {
        if !conf.iter().any(|(e, _)| e == id) {
            conf.push(((*id).to_string(), (*name).to_string()));
        }
    }
    conf
}

/// 装配本轮盲发候选常量表：conf 覆盖表（<conf>/qoder_campaigns.json，每轮读取，
/// 免重编译换 id）+ KNOWN_DAILY_CAMPAIGNS 去重补后。文件存在但零有效条目时
/// app_log 留痕（人工改错的可见性）。
fn load_campaign_table(state: &AppState) -> Vec<(String, String)> {
    let raw = std::fs::read_to_string(state.conf_path("qoder_campaigns.json")).ok();
    let conf = raw.as_deref().map(parse_conf_campaigns).unwrap_or_default();
    if raw.is_some() && conf.is_empty() {
        fs_utils::app_log(
            &state.data_dir,
            "[qoder] conf 覆盖表 qoder_campaigns.json 存在但无有效条目（格式见 parse_conf_campaigns），已忽略",
        );
    } else if !conf.is_empty() {
        fs_utils::app_log(
            &state.data_dir,
            &format!("[qoder] conf 覆盖表载入 {} 条活动 id（优先于内置常量表）", conf.len()),
        );
    }
    merge_campaign_table(conf, KNOWN_DAILY_CAMPAIGNS)
}

/// 最小头（只带鉴权四件套）请求头：不带设备指纹头族（Cosy-MachineId/MachineToken/
/// Version/MachineOS/MachineHostname/MachineCode/MachineType）。
///
/// 依据（探针实证，同 IP 同 token 对照）：
/// - 2026-10-14 19:26：campaigns 列表按请求形态分桶投放，带派生设备头的容器
///   形态全天看不到 CLAIMABLE 条目，最小头请求可见（当时 claim 端点两形态均可用）；
/// - 2026-10-16（issue #114）：上游风控升级为按头形态拦截 claim——完整头一律
///   503 RISK_DEPENDENCY_UNAVAILABLE，最小头 200 CLAIMED（4/4 账号签到到账）。
///
/// 现列表学习、**全部 claim POST**（claim_one / blind_round）与网络异常复查
/// GET（claim_one → recheck_claimed）统一走最小头；
/// 主列表 / 余额 GET 维持完整头主请求形态不变。
fn minimal_auth_headers(access_token: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), format!("Bearer {access_token}")),
        ("Cosy-ClientType".to_string(), qoder_common::COSY_CLIENT_TYPE.to_string()),
        ("User-Agent".to_string(), qoder_common::CLIENT_USER_AGENT.to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ]
}

/// Unix 秒 → "MM-DD HH:MM"（CST）诊断展示（窗口起止用；解析失败给空串）。
fn cst_hhmm(secs: i64) -> String {
    use chrono::TimeZone;
    chrono::FixedOffset::east_opt(8 * 3600)
        .and_then(|tz| tz.timestamp_opt(secs, 0).single())
        .map(|t| t.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 最小头补学：以最小头拉一次活动列表并重学当日「每日 100 Credits」id。
/// 与主列表请求同端点（GET 幂等，无副作用）；失败/空表返回空（尽力而为，
/// 不阻塞主流程）。列表视野（脱敏：activityKey/id/状态/窗口，无任何凭证）
/// 全量落 app_log——2026-10-14 排障时「工具到底看到了什么」无据可查的教训。
/// 取列表用 fetch_campaigns（允许空表——空表 = 学不到 = 放弃，对齐 issue #105
/// 重学口径，不把空列表升格为账号失败）。
fn learn_from_min_headers(
    state: &AppState,
    agent: &ureq::Agent,
    access_token: &str,
    urls: &QoderUrls,
    now_secs: i64,
) -> Vec<(String, String)> {
    let headers = minimal_auth_headers(access_token);
    match fetch_campaigns(state, agent, &headers, urls) {
        Ok(campaigns) => {
            let learned = learn_daily_campaign_ids(&campaigns, now_secs);
            let view: Vec<String> = campaigns
                .iter()
                .map(|c| {
                    let key = s_of(c.get("campaignKey"));
                    let cid = s_of(fs_utils::dig(c, &["campaignId", "campaign_id"]));
                    let act = s_of(c.get("actionType"));
                    let st = s_of(fs_utils::dig(c, &["claimStatus", "claim_status"]));
                    let kind = c
                        .get("benefit")
                        .and_then(|x| x.get("kind"))
                        .map(|v| s_of(Some(v)))
                        .unwrap_or_default();
                    let start = c
                        .get("startAt")
                        .and_then(|v| num_or_none(Some(v)))
                        .map(|v| v as i64)
                        .unwrap_or(0);
                    let end = c
                        .get("endAt")
                        .and_then(|v| num_or_none(Some(v)))
                        .map(|v| v as i64)
                        .unwrap_or(0);
                    format!(
                        "{key}({cid} {act}/{st}/{kind} {}~{})",
                        cst_hhmm(start),
                        cst_hhmm(end)
                    )
                })
                .collect();
            fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "[qoder] 最小头列表视野 {} 条，可学 {}：{}",
                    campaigns.len(),
                    learned.len(),
                    view.join("；")
                ),
            );
            learned
        }
        Err((k, m)) => {
            fs_utils::app_log(
                &state.data_dir,
                &format!("[qoder] 最小头学习拉取失败（{k}）：{m}"),
            );
            Vec::new()
        }
    }
}

/// 活动条目奖励类型是否 CREDITS（到账校验降级的前置条件）。kind 缺失从严视为
/// 非 CREDITS——无法归因奖励类型时不判负（宁 fail 不假 already 的对称面：
/// 宁不判负不误杀真实 success）
fn benefit_is_credits(c: &Value) -> bool {
    c.get("benefit")
        .and_then(|x| x.get("kind"))
        .map(|v| s_of(Some(v)).to_ascii_uppercase() == "CREDITS")
        .unwrap_or(false)
}

/// 盲发单步归类（纯函数，可单测）：宁 fail 不假 already——非明确成功/回放
/// 形态一律 Failed。原实现把回放/4xx/5xx/网络异常全部静默忽略，None 时调用方
/// 维持 already，是「已领（此前已领）」误报的直接源头（2026-10-07 实证：盲发
/// 过期 campaignId 得到非成功响应被吞 → 误报 + 调度器不再重试）。
enum BlindStep {
    /// 200 && CLAIMED && !replayed：真实新领取（兜底目标）
    FreshClaim,
    /// 200 && CLAIMED && replayed：幂等回放（今日确已领过，服务端确认）
    Replay,
    /// 401：登录态失效（中断遍历走 process_account 的 auth 自愈）
    Auth,
    /// 其余（4xx/5xx/网络 status=0/200 未知形态）：失败，如实上报
    Failed(u16),
}

fn classify_blind_step(status: u16, claimed: Option<&str>, replayed: bool, kind: Option<&str>) -> BlindStep {
    if status == 401 {
        return BlindStep::Auth;
    }
    if (200..=201).contains(&status) {
        return match claimed {
            Some("CLAIMED") => {
                // 奖励类型防线（审查 P2）：领取响应 benefit.kind 显式非 CREDITS
                //（如 BOGO）时不判 FreshClaim/Replay——盲发目标是每日积分活动，
                // 非积分领取伪装成 success / 非积分回放伪装成 already 都会掩盖
                // 「积分未领」的真话；kind 缺失不降级（宁不判负不误杀真实
                // success，与 benefit_is_credits 缺失从严的对称取舍）
                if kind.is_some_and(|k| !k.eq_ignore_ascii_case("CREDITS")) {
                    return BlindStep::Failed(status);
                }
                if replayed { BlindStep::Replay } else { BlindStep::FreshClaim }
            }
            // 200 但非 CLAIMED 形态（未知响应结构）不宽容——盲发语义已探针锁定
            _ => BlindStep::Failed(status),
        };
    }
    BlindStep::Failed(status)
}

/// 盲发失败的人类可读原因（known_daily_fallback Failed 分支消息构造）。
/// 已探针锁定的形态给可操作提示，其余保留 HTTP 码 + 原始响应前缀（截 120
/// 字符，与旧格式一致）供排障：
/// - 200/BLOCKED：服务端风控明确拒绝本次领取（非依赖故障、非瞬时错误）。
///   归因是工具盲发携带的每账号指纹未获服务端信任分桶——旧文案「在客户端
///   登录一次」无效：客户端登录建立的是客户端原生指纹的信任，与工具盲发
///   指纹不一致（issue #119）。有效路径：先在工具内切换到该账号（切换器会把
///   该账号指纹同步为本机 machineid/storage.json/state.vscdb），再开客户端
///   正常使用一次，使服务端为该指纹建立信任，然后回工具重试
/// - 503/RISK_DEPENDENCY_UNAVAILABLE：风控前置依赖暂不可用，调度器会自动重试
///   （issue #114 修复后 claim 已用最小头形态，此形态下仍 503 说明上游风控再升级）
/// - 409/CAMPAIGN_NOT_ACTIVE：候选 campaignId 过期（活动批次轮换，issue #66），
///   重学习机制（主列表 + 最小头补学）将自动补正
fn describe_blind_failure(code: u16, claimed: Option<&str>, raw: &str) -> String {
    if claimed == Some("BLOCKED") {
        return "服务端风控拦截（status=BLOCKED，本次领取被拒绝）——该账号设备指纹未建立服务端信任：请在工具内切换到该账号（切换会把该账号指纹同步到本机），再打开 Qoder 客户端正常使用一次建立信任，然后重试".into();
    }
    if code == 503 && raw.contains("RISK_DEPENDENCY_UNAVAILABLE") {
        return "服务端风控依赖暂不可用（503 RISK_DEPENDENCY_UNAVAILABLE），稍后将自动重试（本请求已用最小头形态，若持续出现说明上游风控再升级，请关注工具更新）".into();
    }
    // 409 CAMPAIGN_NOT_ACTIVE（issue #66 健壮性②）：明确归因为「候选 id 错」
    // 而非账号/网络错——盲发到已过期的每日活动 id 的特征形态，给准确提示
    //（旧实现落通用「（HTTP 409）{head}」不可读，也与账号级失败无法区分）。
    // issue #105 修复后该形态会当轮重学并视情补一轮盲发（学得新 id 才重试），
    // PR #106 后重学含最小头补学一层，此处文案同步对齐（产空不重试，不宣称
    //「已重试」）
    if code == 409 && raw.contains(CAMPAIGN_NOT_ACTIVE_TAG) {
        return "每日活动 id 已过期（409 CAMPAIGN_NOT_ACTIVE，活动批次轮换）——已尝试当轮重学（主列表+最小头补学，未学得新候选时不再重试）".into();
    }
    let head: String = raw.chars().take(120).collect();
    format!("（HTTP {code}）{head}")
}

/// 盲发兜底聚合结果（替代原 Option<(kind, message, reward)>——None 曾被调用方
/// 解释为「无可领活动（均已领取）」，掩盖真实失败）。
enum FallbackOutcome {
    /// 至少一条真实新领取（message 已聚合，reward 已累计）
    Success(String, Option<f64>),
    /// 全部条目幂等回放：今日确已领过（服务端确认，非「看不到就猜已领」）
    AlreadyReplayed,
    /// 任一条目 401（中断遍历；已累计 reward 随行，对齐 claim_one auth 语义）
    Auth(Option<f64>),
    /// 存在失败条目（4xx/5xx/网络/未知形态）——如实报 fail，触发调度器
    /// 指数退避重试 + UI 显示真话（即使同时有成功条目：claim 幂等，重试轮次
    /// 以列表已领判定/回放确认，不产生重复领取副作用）。已累计 reward 随行
    /// 不丢弃（真实入账不因整体 fail 而回滚，对齐 Auth 变体语义；无成功场景
    /// 下必为 None——统一透传防未来演进丢账，审查 P3）
    Failed(String, Option<f64>),
}

/// 盲发兜底聚合判定（纯函数，可单测）。优先级：
/// Success > Failed(fail_parts) > AlreadyReplayed > Failed(stale_parts) > Failed(空表)。
/// - fail_parts（账号级/网络级真实失败）掩码一切：宁 fail 不假 already/success
///   （claim 幂等，重试无重复领取副作用）
/// - stale_parts（409 CAMPAIGN_NOT_ACTIVE = 候选 id 过期，issue #66 健壮性②）
///   **不掩码**新鲜成功/回放：id 错 ≠ 账号错，且同批 learned 候选往往已命中；
///   仅当全部候选均过期且无任何成功/回放时如实报 fail（触发重试，重试轮次
///   经重学习带上新 id）
fn aggregate_fallback(
    any_fresh: bool,
    any_replay: bool,
    messages: &[String],
    fail_parts: &[String],
    stale_parts: &[String],
    reward: Option<f64>,
) -> FallbackOutcome {
    if !fail_parts.is_empty() {
        let mut msg = fail_parts.join("；");
        if any_fresh {
            msg = format!("{}；{}", messages.join("；"), msg);
        }
        return FallbackOutcome::Failed(msg, reward);
    }
    if any_fresh {
        return FallbackOutcome::Success(messages.join("；"), reward);
    }
    if any_replay {
        return FallbackOutcome::AlreadyReplayed;
    }
    if !stale_parts.is_empty() {
        return FallbackOutcome::Failed(stale_parts.join("；"), reward);
    }
    FallbackOutcome::Failed("已知每日活动表为空，无兜底可尝试".into(), reward)
}

/// 盲发单轮聚合中间态（纯数据，可单测）：一圈候选的归类结果，
/// 供 stale-only 判定与跨轮合并（known_daily_fallback 当轮重学重试）。
struct BlindRound {
    messages: Vec<String>,
    fail_parts: Vec<String>,
    stale_parts: Vec<String>,
    reward: Option<f64>,
    any_fresh: bool,
    any_replay: bool,
    /// 任一候选 401（blind_round 中断遍历的标记；调用方上报 Auth，
    /// 已累计 reward 随行——对齐 claim_one auth 语义）
    auth: bool,
}

impl BlindRound {
    /// 全部候选均 409 过期且无任何成功/回放/真实失败（issue #105 死循环特征：
    /// 学习产空 + 硬编码表过期，重试轮次原样复现）——当轮重学重试的触发条件。
    /// auth 中断轮不触发（401 需凭证刷新自愈，同头重试注定失败）
    fn stale_only(&self) -> bool {
        !self.auth
            && !self.any_fresh
            && !self.any_replay
            && self.fail_parts.is_empty()
            && !self.stale_parts.is_empty()
    }

    /// 经 aggregate_fallback 定级为 FallbackOutcome
    fn aggregate(self) -> FallbackOutcome {
        aggregate_fallback(
            self.any_fresh,
            self.any_replay,
            &self.messages,
            &self.fail_parts,
            &self.stale_parts,
            self.reward,
        )
    }

    /// 跨轮合并（重试轮并入首轮）：各分桶拼接、reward 累加、fresh/replay 或合并，
    /// 再由 aggregate_fallback 统一定级（stale 不掩码新鲜成功/回放的语义不变）
    fn merge(mut self, other: BlindRound) -> BlindRound {
        self.messages.extend(other.messages);
        self.fail_parts.extend(other.fail_parts);
        self.stale_parts.extend(other.stale_parts);
        self.reward = match (self.reward, other.reward) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => b.or(a),
        };
        self.any_fresh |= other.any_fresh;
        self.any_replay |= other.any_replay;
        self.auth |= other.auth;
        self
    }
}

/// 盲发一圈候选（known_daily_fallback 的执行单元）。逐条盲发 claim，单步经
/// classify_blind_step 归类；任一 401 → 置 auth 标记中断遍历（调用方据此上报
/// `Auth`，已累计 reward 随行不丢弃，对齐 claim_one auth 语义）。409 过期 id
/// 会从 `learned` 与 `table` 原地剔除（&mut），同轮后续账号不再重复盲发同一 409
///（table 是整轮共享的候选池，剔除跨账号生效）。
/// claim POST 一律最小头形态（由 `access_token` 现场构建，issue #114：完整头
/// 被上游风控按形态 503 拦截，最小头通过）。
fn blind_round(
    agent: &ureq::Agent,
    access_token: &str,
    candidates: &[(String, String)],
    learned: &mut Vec<(String, String)>,
    table: &mut Vec<(String, String)>,
) -> BlindRound {
    let headers = minimal_auth_headers(access_token);
    let mut round = BlindRound {
        messages: Vec::new(),
        fail_parts: Vec::new(),
        stale_parts: Vec::new(),
        reward: None,
        any_fresh: false,
        any_replay: false,
        auth: false,
    };
    for (cid, cname) in candidates {
        // 路径安全防线：候选含非法字符时拒绝该条（与 claim_one 同白名单）
        if cid.is_empty()
            || !cid.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            continue;
        }
        let url = format!(
            "{}/sash/api/v1/me/campaigns/{cid}/claim",
            qoder_common::OPEN_API_BASE
        );
        jitter_sleep();
        let (status, body, raw) = post_empty(agent, &url, &headers);
        let claimed = body
            .as_ref()
            .and_then(|b| fs_utils::dig(b, &["status", "data.status"]))
            .map(|v| s_of(Some(v)).to_ascii_uppercase());
        let replayed = body
            .as_ref()
            .and_then(|b| fs_utils::dig(b, &["replayed", "data.replayed"]))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // 领取响应的奖励类型（benefit.kind，与 FreshClaim 分支 amount 同路径）：
        // 供 classify_blind_step 校验非 CREDITS 领取不误报 success/already
        let kind = body
            .as_ref()
            .and_then(|b| b.get("benefit"))
            .and_then(|x| x.get("kind"))
            .and_then(Value::as_str);
        match classify_blind_step(status, claimed.as_deref(), replayed, kind) {
            BlindStep::Auth => {
                round.auth = true;
                return round;
            }
            BlindStep::FreshClaim => {
                let amt = body
                    .as_ref()
                    .and_then(|b| b.get("benefit"))
                    .and_then(|x| x.get("amount"))
                    .and_then(|v| num_or_none(Some(v)));
                round.any_fresh = true;
                round.messages.push(format!("{cname}（盲发直领）"));
                if let Some(a) = amt {
                    round.reward = Some(round.reward.unwrap_or(0.0) + a);
                }
            }
            BlindStep::Replay => {
                round.any_replay = true;
            }
            BlindStep::Failed(code) => {
                let reason = describe_blind_failure(code, claimed.as_deref(), &raw);
                // 409 CAMPAIGN_NOT_ACTIVE = 候选 id 过期（活动批次轮换）→ 归
                // stale_parts 不掩码新鲜成功/回放；其余真实失败照旧掩码。
                // 过期 id 同步从本轮共享学习池与候选表剔除（审查修复：跨午夜
                // 竞态下首个账号学到的 id 已过期时，后续账号不再重复盲发同一
                // 409；table 剔除为 PR #106 移植——conf/内置表过期 id 同样防重复）
                if code == 409 && raw.contains(CAMPAIGN_NOT_ACTIVE_TAG) {
                    learned.retain(|(e, _)| e != cid);
                    table.retain(|(e, _)| e != cid);
                    round.stale_parts.push(format!("{cname} 盲发失败：{reason}"));
                } else {
                    round.fail_parts.push(format!("{cname} 盲发失败：{reason}"));
                }
                // 继续尝试下一条：单条失败不中断（聚合时按 aggregate_fallback 定级）
            }
        }
    }
    round
}

/// 列表零 CLAIMABLE 时的每日活动盲发直领。候选序（issue #66 健壮性①）：
/// **learned 优先**（当日列表/最小头补学重学的 id，最新鲜），`table` 去重补后
/// 兜底（conf 覆盖表 + 硬编码表，由 load_campaign_table 装配）。逐条盲发
/// claim，单步经 classify_blind_step 归类；任一 401 → 中断遍历返回 `Auth`
///（已累计 reward 不丢弃）。409 过期 id 会从 `learned` 与 `table` 原地剔除
///（&mut），同轮后续账号不再重复盲发同一 409。
///
/// 当轮重学闭环（issue #105）：全部候选 409 过期且无成功/回放/真实失败
///（stale_only）时，**当轮**重取活动列表并经宽松档（learn_daily_campaign_ids_lenient）
/// 重学，学得新 id 立即补一轮盲发——原实现只承诺「将从活动列表自动重学」却把
/// 自愈寄望于调度器下一轮，学习产空 + 硬编码过期时即全天 409 死循环。
/// 重试轮与首轮经 BlindRound::merge 合并后统一定级（409 归 stale 的掩码语义不变）。
///
/// 整条兜底链路（盲发 claim + 内部重学 GET）均最小头形态（`access_token` 现场构建，
/// issue #114：claim 端点完整头被风控 503 拦截；重学 GET 用最小头视野为完整头
/// 形态的超集——CLAIMABLE 条目可见，学习可见性只增不减）。
fn known_daily_fallback(
    state: &AppState,
    agent: &ureq::Agent,
    access_token: &str,
    urls: &QoderUrls,
    learned: &mut Vec<(String, String)>,
    table: &mut Vec<(String, String)>,
) -> FallbackOutcome {
    let mut candidates: Vec<(String, String)> = learned.clone();
    for (cid, cname) in table.iter() {
        if !candidates.iter().any(|(e, _)| e == cid) {
            candidates.push((cid.clone(), cname.clone()));
        }
    }
    let tried: Vec<String> = candidates.iter().map(|(cid, _)| cid.clone()).collect();
    let first = blind_round(agent, access_token, &candidates, learned, table);
    // 401 中断：上报 Auth 走账号级凭证刷新自愈（已累计 reward 随行，不重学不重试）
    if first.auth {
        return FallbackOutcome::Auth(first.reward);
    }
    if !first.stale_only() {
        return first.aggregate();
    }
    // 当轮重学：重取列表（fetch_campaigns 允许空表——空表 = 学不到 = 放弃重试），
    // 宽松档学习，仅取**未试过**的 id（409 剔除只动 learned/table，tried 防重学撞旧 id）。
    // 重学 GET 用最小头（同上：视野超集，CLAIMABLE 可见，学习更稳）
    // 审查备案（L20）：宽松档学习不校验活动时间窗，学到的 id 可能含未开放/已过期
    // 活动，仅作第二轮盲发的候选范围提示（加速收敛）——真值以端点判定为准：
    // already 只能来自服务端 replayed/CLAIMED 回放确认（classify_blind_step），
    // 学习值本身不会把未领活动误归 already
    let mut retry: Vec<(String, String)> = Vec::new();
    if let Ok(campaigns) =
        fetch_campaigns(state, agent, &minimal_auth_headers(access_token), urls)
    {
        merge_learned(&mut retry, learn_daily_campaign_ids_lenient(&campaigns));
    }
    retry.retain(|(cid, _)| !tried.iter().any(|t| t == cid));
    if retry.is_empty() {
        return first.aggregate();
    }
    // 重学结果入共享池（跨账号复用；若重试轮再 409 会被 blind_round 剔除）
    merge_learned(learned, retry.clone());
    let second = blind_round(agent, access_token, &retry, learned, table);
    let merged = first.merge(second);
    if merged.auth {
        return FallbackOutcome::Auth(merged.reward);
    }
    merged.aggregate()
}

/// 盲发兜底 + 409 同轮自愈（2026-10-14 事故固化，移植 PR #106）：先正常盲发
///（known_daily_fallback 内含 issue #105 主列表当轮重学闭环）；若整批候选仍均
/// 因 CAMPAIGN_NOT_ACTIVE 过期（无新鲜成功/回放/真实失败掩码），用最小头补学
/// 一次（learn_from_min_headers），拿到新 id 立即重试盲发——同轮自愈，不等
/// 调度器下一轮（旧实现 409 后只能等 30 分钟起步的退避重试，且主列表视野对
/// 容器形态不可见 CLAIMABLE 时永远收敛不了）。
/// 注意：宽松档补学不校验时间窗，可能学回本轮已被 409 剔除的过期 id——合并前
/// 按首轮候选全集快照（learned ∪ table）过滤，重试候选只含未试过的新 id。
///
/// 全链路最小头形态（issue #114）：`access_token` 为唯一请求身份入参，盲发
/// claim 与两路补学 GET 均经 `minimal_auth_headers` 构建，不带设备指纹头族。
fn fallback_with_relearn(
    state: &AppState,
    agent: &ureq::Agent,
    access_token: &str,
    urls: &QoderUrls,
    learned: &mut Vec<(String, String)>,
    table: &mut Vec<(String, String)>,
) -> FallbackOutcome {
    // 快照本轮已盲发过的候选全集：409 剔除只动 learned/table，本快照不受影响
    let mut attempted: Vec<String> = learned.iter().map(|(c, _)| c.clone()).collect();
    for (cid, _) in table.iter() {
        if !attempted.iter().any(|c| c == cid) {
            attempted.push(cid.clone());
        }
    }
    let outcome = known_daily_fallback(state, agent, access_token, urls, learned, table);
    if let FallbackOutcome::Failed(m, _) = &outcome {
        if m.contains(CAMPAIGN_NOT_ACTIVE_TAG) {
            let before = learned.len();
            let found: Vec<(String, String)> = learn_from_min_headers(
                state,
                agent,
                access_token,
                urls,
                chrono::Utc::now().timestamp(),
            )
            .into_iter()
            .filter(|(cid, _)| !attempted.iter().any(|c| c == cid))
            .collect();
            merge_learned(learned, found);
            // 过滤后 merge 仍增长 = 确有未试过的新 id 才值得重试盲发
            if learned.len() > before {
                return known_daily_fallback(state, agent, access_token, urls, learned, table);
            }
        }
    }
    outcome
}

/// 盲发兜底结果 → (kind, message, reward) 统一映射（4 处调用点共用）。
/// `fail_ctx = Some((k, m))`：空表路径——Failed 维持原空表分类 `k`（根因仍是
/// 列表不可见，fail_kind=empty_campaigns 语义不变：调度器当日重试排除 + 推送
/// 抑制照旧），兜底详情追加在原消息之后而非整体替换（fail_kind 派生与空表
/// app_log 诊断均按 starts_with 前缀判定，原消息在前不受影响；试领 409/503
/// 详情透出后，候选池诊断 contains CAMPAIGN_NOT_ACTIVE_TAG 在空表路径恢复
/// 可见）；`None`：零 CLAIMABLE 路径——Failed 直接归 fail，不掩盖（2026-10-07
/// 误报「已领（此前已领）」修复点的延续口径）。
fn run_fallback(
    state: &AppState,
    agent: &ureq::Agent,
    access_token: &str,
    urls: &QoderUrls,
    learned: &mut Vec<(String, String)>,
    table: &mut Vec<(String, String)>,
    fail_ctx: Option<(&str, &str)>,
) -> (String, String, Option<f64>) {
    match fallback_with_relearn(state, agent, access_token, urls, learned, table) {
        FallbackOutcome::Success(m, r) => ("success".into(), m, r),
        FallbackOutcome::AlreadyReplayed => {
            ("already".into(), "每日奖励已领取（盲发回放确认）".into(), None)
        }
        FallbackOutcome::Auth(r) => ("auth".into(), "登录态失效（401）".into(), r),
        FallbackOutcome::Failed(fm, r) => match fail_ctx {
            Some((k, m)) => (k.into(), format!("{m}；盲发试领全 miss：{fm}"), r),
            None => ("fail".into(), fm, r),
        },
    }
}

/// 处理单账号签到（含 401 刷新一次重试，禁二次刷新）。返回 account 事件（不含 index）。
/// `learned`：本轮跨账号共享的当日活动 id 重学习结果（多账号场景首个账号学到的
/// id 直接惠及后续账号，campaign id 全局有效与账号无关）。
/// `table`：本轮共享的盲发候选常量表（conf 覆盖表 + 内置表，PR #106 移植），
/// 409 过期 id 剔除跨账号生效。
/// `shared`：GUI 进程内网关运行态（None = CLI 独立进程），CREDITS 奖励增量回写
/// qoder_pool（见尾部回写注释）。
fn process_account(
    state: &AppState,
    agent: &ureq::Agent,
    acct: &Value,
    opts: &QoderCheckinOpts,
    learned: &mut Vec<(String, String)>,
    table: &mut Vec<(String, String)>,
    shared: Option<&std::sync::Arc<crate::api_server::ApiSharedState>>,
) -> Value {
    let aid = s_of(acct.get("id"));
    let name = acct
        .get("nickname")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            let uid = s_of(acct.get("uid"));
            if uid.is_empty() { aid.clone() } else { uid.chars().take(12).collect() }
        });
    let base_ev = json!({"user_id": aid, "name": name});
    let (creds, refreshed, note) = qoder_common::ensure_fresh(state, agent, &aid, opts.lazy_hours);
    if creds.access_token.is_empty() {
        let mut ev = json!({ "user_id": aid, "name": base_ev["name"], "status": "fail",
                       "message": if note == "vault_down" {
                           "凭证加密存储暂时不可用（vault 故障），稍后自动恢复".to_string()
                       } else {
                           format!("无可用凭证（{note}）")
                       } });
        // 审查 M11：no_credential（store 无凭证）与 auth_dead 同属终态——重试
        // 不会凭空产生凭证，单列免全天无效重试；refresh_failed/refresh_busy 为
        // 暂态不标记（调度器冷却后自动重试）
        if note == "auth_dead" || note == "no_credential" {
            ev["fail_kind"] = json!(PERMANENT_AUTH_TAG);
        }
        return ev;
    }
    if note == "pat_rejected" {
        return json!({ "user_id": aid, "name": base_ev["name"], "status": "fail",
                       "fail_kind": PERMANENT_AUTH_TAG,
                       "message": "PAT 校验失败（无效或已吊销）：请到 qoder.com.cn/account/integrations 重新创建并导入" });
    }
    // 前置拦截（审查 L）：凭证已过期且无刷新令牌（expired_needs_relogin）时，本轮
    // 请求与 401 自愈都注定失败——直接 fail 跳过，省一次必败网络请求
    if note == "expired_needs_relogin" {
        return json!({ "user_id": aid, "name": base_ev["name"], "status": "fail",
                       "fail_kind": PERMANENT_AUTH_TAG,
                       "message": "凭证已过期且无刷新令牌，需重新登录或重新导入 PAT" });
    }
    // P1 前置拦截：刷新令牌已被服务端 4xx 永久拒绝（auth_dead，池已标记 needs_relogin）
    // 时本轮请求与 401 自愈同样注定失败——直接 fail 跳过，省一次必败网络请求
    if note == "auth_dead" {
        return json!({ "user_id": aid, "name": base_ev["name"], "status": "fail",
                       "fail_kind": PERMANENT_AUTH_TAG,
                       "message": "登录凭证已失效（刷新令牌被服务端拒绝），请重新导入账号凭证" });
    }
    if refreshed {
        qoder_common::sync_pool_expiry(state, &aid, &creds);
    }
    let urls = urls_for();
    let mut headers = qoder_common::build_auth_headers(&creds);
    // 逐活动明细（F-80-余 v2 档期日历数据源）：auth 401 重试路径继续累计
    let mut campaigns_detail: Option<Vec<Value>> = None;
    // 签到前余额（差值兜底数据源；查询失败不阻塞签到。共享 qoder_credits 解析，
    // 端点 R-7 固化为 GET /sash/api/v2/me/usage）
    let pre_balance = super::qoder_credits::fetch_usage_balance(agent, &headers);
    // 本轮成功领取的活动是否全部为 CREDITS（审查修复：非积分奖励如 BOGO 到账不
    // 体现为余额增长，不能凭「余额未动」判未到账——仅全 CREDITS 时才启用降级；
    // 盲发兜底候选口径强制 CREDITS，天然保持 true）
    let mut all_credits = true;
    // 最小头形态请求的令牌（PR #106 移植 + issue #114 扩展：列表补学与全部
    // claim POST 均经 minimal_auth_headers 用此令牌走最小头——claim 端点带设备
    // 指纹头族被上游风控 503 拦截，最小头通过；401 刷新成功后随新令牌更新）
    let mut min_token = creds.access_token.clone();

    // 列表请求：完整头（容器派生设备头形态）返回空表时，用最小头复取一次
    //（issue #132：空表走 Err 分支，原 Ok 分支内的最小头补学从未执行）。
    // 实证依据见 minimal_auth_headers 注释与 list_campaigns_with_fallback。
    let list_call = list_campaigns_with_fallback(state, agent, &headers, &min_token, &urls, &aid);
    // 列表已由最小头复取得到时，不再用同一形态重复补学（见封装注释）
    let list_from_min = matches!(&list_call, Ok((_, true)));
    let list_res = list_call.map(|(c, _)| c);
    let (mut kind, mut message, mut reward) = match list_res {
        Ok(campaigns) => {
            // 健壮性①：从列表反学当日活动 id（列表虽过滤 CLAIMABLE 状态，条目
            // 本身可见），供盲发兜底优先使用；401 重试路径同款
            merge_learned(
                learned,
                learn_daily_campaign_ids(&campaigns, chrono::Utc::now().timestamp()),
            );
            // 健壮性①+（PR #106 移植）：主列表（容器形态）零可学条目时，用最小头
            // 形态补学一次（learned 整轮共享，仅每轮首个扑空账号实际发起，后续账号
            // 跳过；列表本身已由最小头复取得到时跳过——同形态再 GET 只是重复请求）
            if learned.is_empty() && !list_from_min {
                merge_learned(
                    learned,
                    learn_from_min_headers(
                        state,
                        agent,
                        &min_token,
                        &urls,
                        chrono::Utc::now().timestamp(),
                    ),
                );
            }
            let claimable = filter_claimable(&campaigns);
            if claimable.is_empty() {
                // 列表零 CLAIMABLE：campaigns 列表对工具请求形态按投放过滤
                // CLAIMABLE 条目（已领 grant 无条件展示）。两步定真话：
                // ① 先查列表内当日「每日 100 Credits」CLAIMED grant（服务端对
                //    已领无条件展示，命中即真实 already，不发起多余请求）；
                // ② 否则盲发每日活动兜底直领（learned 重学 id 优先 + 硬编码表
                //    兜底），结果如实归类（Failed 不再归 already——2026-10-07
                //    误报「已领（此前已领）」的修复点）
                let now = chrono::Utc::now().timestamp();
                if daily_credits_claimed_in_list(&campaigns, now) {
                    ("already".into(), "每日奖励已领取".into(), None)
                } else {
                    run_fallback(state, agent, &min_token, &urls, learned, table, None)
                }
            } else {
                let mut kind = String::new();
                let mut auth_msg = String::new();
                let mut messages: Vec<String> = Vec::new();
                let mut reward = None;
                // 逐活动明细（F-80-余 v2 档期日历数据源）
                let mut campaigns_log: Vec<Value> = Vec::new();
                for c in &claimable {
                    let cid = s_of(fs_utils::dig(c, &["campaignId", "campaign_id"]));
                    let cname = s_of(fs_utils::dig(
                        c,
                        &["name", "title", "campaignName", "campaign_name"],
                    ));
                    let (k, m, r) = claim_one(agent, &min_token, &urls, c);
                    if k == "auth" {
                        kind = "auth".into();
                        auth_msg = m;
                        // 已领取活动的累计奖励不丢弃（真实入账，原 reward=None 会抹掉）
                        // auth 中断条目不入 campaigns_log（档期日历只收真实领取结果）
                        break;
                    }
                    // 非 CREDITS 活动成功领取 → 本轮不可凭「余额未动」判未到账
                    if k == "success" && !benefit_is_credits(c) {
                        all_credits = false;
                    }
                    campaigns_log.push(json!({
                        "id": cid,
                        "name": if cname.is_empty() { cid.clone() } else { cname },
                        "kind": k,
                        "reward": r,
                    }));
                    // kind 优先级归并（P2）：success > fail > already——
                    // 部分活动失败不掩盖整体成功，任一失败也不被 already 掩盖
                    kind = merge_claim_kind(&kind, &k);
                    if !m.is_empty() {
                        messages.push(m);
                    }
                    // 同轮多活动奖励累加（原实现覆盖取最后一个，真实少记）
                    if let Some(r) = r {
                        reward = Some(reward.unwrap_or(0.0) + r);
                    }
                }
                campaigns_detail = if campaigns_log.is_empty() { None } else { Some(campaigns_log) };
                if kind == "auth" {
                    (kind, auth_msg, reward)
                } else {
                    if kind.is_empty() {
                        kind = "fail".into();
                    }
                    let message = messages.join("；");
                    (kind, message, reward)
                }
            }
        }
        Err((k, m)) => {
            // 空表盲发兜底（issue #141）：上游按账号分桶投放列表，本账号两种请求
            // 形态均不可见，但 claim 端点不受列表过滤约束（探针实证）且幂等——
            // 先盲发试领（多账号轮次 learned 通常已含当日 id，首个账号靠 conf/
            // 内置表），命中即真实领取/回放确认，与零 CLAIMABLE 路径同款
            if m.starts_with(EMPTY_CAMPAIGNS_TAG) {
                // Failed 分支保留原空表分类 + 详情追加的完整语义说明见 run_fallback 文档
                run_fallback(state, agent, &min_token, &urls, learned, table, Some((k.as_str(), m.as_str())))
            } else {
                (k, m, None)
            }
        }
    };

    // 401：刷新一次仅重试失败分支（禁二次刷新，对齐 wb_checkin F-09）。
    // 走 ensure_fresh（lazy=MAX 恒刷新，内部成功已落库）：PAT 账号经
    // exchange_job_token 用原始 PAT 重换作业令牌，客户端账号走 refresh_token_once；
    // 此前直接调后者，PAT 换发的作业令牌对其不兼容（qoder_common 明令规避），
    // 自愈必失败且误报「需重新登录」。令牌未变不重试（对齐 credits 401 自愈）
    if kind == "auth" {
        let (new, refreshed, _) = qoder_common::ensure_fresh(state, agent, &aid, i64::MAX);
        let retry_cred =
            if refreshed && new.access_token != creds.access_token { Some(new) } else { None };
        match retry_cred {
            Some(new) => {
                qoder_common::sync_pool_expiry(state, &aid, &new);
                headers = qoder_common::build_auth_headers(&new);
                // 最小头令牌（列表补学 + claim POST）随刷新同步更新（PR #106 /
                // issue #114：重试路径的补学与 claim 均须用新令牌，旧令牌已被
                // 一次性轮换作废，用旧令牌补学必 401）
                min_token = new.access_token.clone();
                // 刷新后同样走「完整头 → 空表则最小头复取」（issue #132）：本次
                // 修复针对的正是完整头持续空表，只用完整头重试会立刻再次空表并结束
                let retry = match list_campaigns_with_fallback(
                    state, agent, &headers, &min_token, &urls, &aid,
                ) {
                    Ok((campaigns, from_min)) => {
                        // 重试路径同样重学（新令牌下列表视野可能不同，多学无害）
                        merge_learned(
                            learned,
                            learn_daily_campaign_ids(&campaigns, chrono::Utc::now().timestamp()),
                        );
                        // 重试路径同款最小头补学（learned 仍空时；整轮共享，通常
                        // 首个账号已补学到位，此处多为空转跳过。用刷新后的新令牌；
                        // 列表本身已来自最小头复取时不重复请求）
                        if learned.is_empty() && !from_min {
                            merge_learned(
                                learned,
                                learn_from_min_headers(
                                    state,
                                    agent,
                                    &min_token,
                                    &urls,
                                    chrono::Utc::now().timestamp(),
                                ),
                            );
                        }
                        let claimable = filter_claimable(&campaigns);
                        if claimable.is_empty() {
                            // 与首次路径同口径：先查列表内当日已领 grant，再盲发
                            // 兜底直领（learned 优先），失败如实归 fail（不掩盖）
                            let now = chrono::Utc::now().timestamp();
                            if daily_credits_claimed_in_list(&campaigns, now) {
                                ("already".into(), "每日奖励已领取".into(), None)
                            } else {
                                run_fallback(state, agent, &min_token, &urls, learned, table, None)
                            }
                        } else {
                            // kind 空串起步（对照首次路径）：全部 claim 失败时应判 fail
                            // 而非误标 already（成功才覆写 success，否则取首个非成功 kind）
                            let mut kind = String::new();
                            let mut messages: Vec<String> = Vec::new();
                            let mut reward = None;
                            // 重试路径同样累计逐活动明细（合并进首次已收部分）
                            let mut retry_log: Vec<Value> = Vec::new();
                            for c in &claimable {
                                let cid = s_of(fs_utils::dig(c, &["campaignId", "campaign_id"]));
                                let cname = s_of(fs_utils::dig(
                                    c,
                                    &["name", "title", "campaignName", "campaign_name"],
                                ));
                                let (k, m, r) = claim_one(agent, &min_token, &urls, c);
                                // 与首次路径同口径：非 CREDITS 成功 → 不凭余额判未到账
                                if k == "success" && !benefit_is_credits(c) {
                                    all_credits = false;
                                }
                                retry_log.push(json!({
                                    "id": cid,
                                    "name": if cname.is_empty() { cid.clone() } else { cname },
                                    "kind": k,
                                    "reward": r,
                                }));
                                // kind 优先级归并（P2）：与首次路径同口径（success > fail > already）
                                kind = merge_claim_kind(&kind, &k);
                                if !m.is_empty() {
                                    messages.push(m);
                                }
                                // 同轮多活动奖励累加（与首次路径同语义）
                                if let Some(r) = r {
                                    reward = Some(reward.unwrap_or(0.0) + r);
                                }
                            }
                            if kind.is_empty() {
                                kind = "fail".into();
                            }
                            campaigns_detail = match campaigns_detail.take() {
                                Some(mut prev) => {
                                    prev.extend(retry_log);
                                    Some(prev)
                                }
                                None => if retry_log.is_empty() { None } else { Some(retry_log) },
                            };
                            (kind, messages.join("；"), reward)
                        }
                    }
                    Err((k, m)) => {
                        // 重试路径同款空表盲发兜底（issue #141）：刷新后列表仍不可
                        // 见时，用刷新后的最小头令牌盲发试领，全 miss 才回落原分类
                        // （兜底详情追加在原消息后，保持空表前缀不变，同首次路径）
                        if m.starts_with(EMPTY_CAMPAIGNS_TAG) {
                            // 同首次路径：Failed 保留原空表分类 + 详情追加（见 run_fallback 文档）
                            run_fallback(state, agent, &min_token, &urls, learned, table, Some((k.as_str(), m.as_str())))
                        } else {
                            (k, m, None)
                        }
                    }
                };
                kind = retry.0;
                message = retry.1;
                // 重试奖励合并进首次已累计部分（auth 中断前可能已领到部分活动）：
                // 原实现整体覆盖，401 前已入账的奖励被抹掉
                reward = match (reward, retry.2) {
                    (Some(a), Some(b)) => Some(a + b),
                    (a, b) => b.or(a),
                };
            }
            None => {
                kind = "fail".into();
                message = "登录态失效且刷新失败，需重新登录".into();
                // 已累计奖励保留（真实入账不因刷新失败而回滚；原 reward=None 丢弃）
            }
        }
    }
    // 到账校验（issue #66 健壮性③）：claim success 后回读余额，未到账如实降级
    // fail（保留调度器自动重试），杜绝「报已领但积分余额未变」的假成功。
    // 规则：
    // - reward 为 None 且余额增长 → 差值兜底采信（对齐 F-17 模式，仅 >0 采信）
    // - 本轮成功活动全为 CREDITS 且余额未增长 → 降级 fail（服务端已确认领取而
    //   余额未动 = 未到账；奖励类型已归因，reward 有值/无值均可判——审查修复：
    //   原条件仅看 reward 有值，非积分活动（BOGO 等）amount 有值但本就不进余额，
    //   会被误判未到账）
    // - 本轮含非 CREDITS 活动（all_credits=false）→ 不判负防误杀（余额无法归因
    //   非 CREDITS 奖励是否到账）
    // - 审查 M12：原「单次 jitter_sleep + 单次回读」遇上游入账延迟超出单次窗口
    //   （1~3s）会误判未到账 → 降级 fail 触发整轮重签；改最多 3 次回读，到账
    //  （post > pre）即提前采信，3 轮成功回读均未增长才降级
    // - 回读失败（None）不判负——余额端点故障不应推翻已成功的领取
    if kind == "success" {
        // 审查修复（L18）：原循环内 `let Some(pre) = pre_balance else { break; }` 是
        // 不可达守卫（外层条件已保证 pre_balance 为 Some），死代码在日后重构外层
        // 条件时会静默退化——改为安全绑定：不变量破坏时跳过整个到账校验
        //（不判到账，维持 success），不依赖不可达分支兜底
        if let Some(pre) = pre_balance {
            let mut flat_reads: u32 = 0; // 成功回读且未增长的次数
            let mut arrived = false;
            for _attempt in 0..3 {
                jitter_sleep();
                let Some(post) = super::qoder_credits::fetch_usage_balance(agent, &headers) else {
                    continue; // 回读失败：本轮作废重试；3 轮全失败则 flat_reads=0 不判负
                };
                if reward.is_none() && post > pre {
                    reward = Some(post - pre);
                }
                if post > pre {
                    arrived = true;
                    break;
                }
                flat_reads += 1;
                if !all_credits {
                    break; // 余额无法归因非 CREDITS 奖励：不判负，无需重读（原口径）
                }
            }
            if all_credits && !arrived && flat_reads > 0 {
                kind = "fail".into();
                message = format!("{message}；但余额未增长（未到账），保留自动重试");
                reward = None;
            }
        }
    }
    // 签到奖励运行期回写（对齐 qoder_credits::run_snapshot_task 的 issue #102 口径）：
    // CREDITS 奖励是真实入账，拨进网关 qoder_pool 防止「签到充值成功后账号仍被零积分
    // 过滤持续排除到下次快照」（默认 every_hours 2h / daily 档最长约 13.5h）。
    // 仅回写 credits 增量 + fetched_at，expire 以快照为准不动；未到账降级已置
    // reward=None 天然不回写；非 CREDITS 活动（BOGO 等）金额不进余额无法归因，
    // 不回写；差值兜底采信（reward 从余额差值派生）同样覆盖
    if all_credits {
        if let (Some(shared), Some(r)) = (shared, reward) {
            if r > 0.0 {
                shared.qoder_pool.apply_credits_delta(&aid, r);
            }
        }
    }
    let status_txt = match kind.as_str() {
        "success" => "success",
        "already" => "already",
        _ => "fail",
    };
    // 空活动列表诊断（脱敏：只记账号与结论，不含 token/响应原文）
    if message.starts_with(EMPTY_CAMPAIGNS_TAG) {
        fs_utils::app_log(
            &state.data_dir,
            &format!("qoder 签到空活动列表: {aid}（Cosy-ClientType={} 已带；若持续出现请核查活动状态/接口结构 R-9）", qoder_common::COSY_CLIENT_TYPE),
        );
    }
    // 409 全过期诊断（PR #106 移植，2026-10-14 事故教训：当时只知道 409，不知道
    // 候选池还剩什么）——记本轮候选池现状，供判断是「表过期未补」还是
    //「补学也扑空（上游结构变更）」
    if message.contains(CAMPAIGN_NOT_ACTIVE_TAG) {
        fs_utils::app_log(
            &state.data_dir,
            &format!(
                "[qoder] 409 过期诊断 {aid}: learned={} table={}",
                learned.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>().join(","),
                table.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>().join(","),
            ),
        );
    }
    let mut ev = json!({ "user_id": aid, "name": base_ev["name"], "status": status_txt, "message": message });
    // 结构化失败类别（审查 L-empty_campaigns）：调度器/CLI/UI 统计一律消费
    // fail_kind 字段，不再各自耦合 message 文案；前缀派生已收敛到
    // EMPTY_CAMPAIGNS_TAG 常量（与产源 list_campaigns 同源，改展示文案尾缀
    // 不破坏统计，换标记只动常量一处）
    if message.starts_with(EMPTY_CAMPAIGNS_TAG) {
        ev["fail_kind"] = json!(EMPTY_CAMPAIGNS_TAG);
    }
    if let Some(r) = reward {
        ev["reward"] = json!(r);
    }
    // 逐活动明细（F-80-余 v2 档期日历数据源；仅在确有领取动作时携带）
    if let Some(camps) = campaigns_detail.filter(|c| !c.is_empty()) {
        ev["campaigns"] = Value::Array(camps);
    }
    // 审查 Low：refreshed_unsaved（续期成功但持久化失败）同样是「已续期」，
    // 不标注会让用户误以为凭证已安全保存；尾缀如实区分两种形态
    if kind == "success" && (note == "refreshed" || note == "refreshed_unsaved") {
        let suffix = if note == "refreshed_unsaved" {
            "凭证已续期，但保存失败，下次仍会重复刷新"
        } else {
            "凭证已续期"
        };
        ev["message"] = json!(format!("{}（{suffix}）", ev["message"].as_str().unwrap_or("")));
    }
    ev
}

/// 签到整轮：逐账号串行处理，事件经 emit 回调逐条输出（NDJSON 管线复用）。
/// 返回 done 事件（ok/already/failed 计数），供启动补签/调度静默路径直接消费。
/// `runtime`：GUI 进程内网关运行态句柄（调度器/启动补签/GUI 手签传入，对齐
/// qoder_credits::run_snapshot_task）——CREDITS 奖励增量回写 qoder_pool；
/// CLI 独立进程传 None 跳过
pub fn run_checkin_round(
    state: &AppState,
    opts: &QoderCheckinOpts,
    runtime: Option<&std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    emit: &mut dyn FnMut(&Value),
) -> Value {
    // 跨进程互斥（审查 P1）：schtasks CLI（--task-run qoder-checkin）与应用内调度器
    // 默认同为 10:15 触发，双进程对同账号并发 ensure_fresh 会以同一 refresh_token
    // 刷新（服务端一次性轮换下后到者误标 needs_relogin）。抢锁失败方幂等跳过
    //（done 带 skipped_busy，调度器据此不记当日已跑）；3s 等待区分「瞬时竞争」
    //（短暂等待后获取）与「对方长跑」（放弃跳过，重试幂等无损失）。
    // 失败原因落日志：锁创建失败（机制不可用）≠ 他方占用（2026-10-04 err=3 教训）
    let (_cross, lock_fail) =
        qoder_common::CrossProcLock::try_acquire(&state.data_dir, "checkin", 3_000);
    let Some(_cross) = _cross else {
        let reason = lock_fail.as_ref().map(|f| f.describe()).unwrap_or_default();
        fs_utils::app_log(
            &state.data_dir,
            &format!("[qoder] Qoder 签到未执行（{reason}），本轮幂等跳过"),
        );
        let done = json!({
            "type": "done", "ok": 0, "already": 0, "failed": 0,
            "failed_empty_campaigns": 0, "failed_permanent": 0, "skipped_busy": true,
        });
        emit(&done);
        return done;
    };
    // 设备指纹惰性回填（§5.10：GUI/CLI/启动补签三路共用本漏斗，一处 ensure 全覆盖；
    // 失败不阻塞签到，仅缺注入指纹）
    if let Err(e) = super::qoder_device::ensure_pool_profiles(state) {
        fs_utils::app_log(&state.data_dir, &format!("Qoder 设备指纹回填失败（继续签到）: {e}"));
    }
    let agent = http_agent(30);
    let pool: Value = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
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
    // 当日活动 id 重学习结果（issue #66 健壮性①）：跨账号共享——campaign id 全局
    // 有效，首个账号学到的 id 直接供后续账号盲发兜底使用（多账号不再各自试错）
    let mut learned_ids: Vec<(String, String)> = Vec::new();
    // 盲发候选常量表（conf 覆盖表 + 内置表；PR #106 移植）：整轮共享，
    // 409 过期 id 从中剔除，跨账号不再重复盲发同一过期 id
    let mut campaign_table = load_campaign_table(state);
    // 多账号签到间隔（任务配置页可改，默认 3s，0=关闭）：防上游频控，账号间串行等待
    let gap_secs = crate::models::effective_checkin_gap(state.settings().qoder_checkin_gap_secs);
    // 网关运行态共享句柄（对齐 run_snapshot_task 提取模式）：网关未启动时为 None
    let shared = runtime.and_then(|r| {
        let g = r.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().map(|rt| rt.shared.clone())
    });
    for (i, acct) in accounts.iter().enumerate() {
        if i > 0 && gap_secs > 0 {
            std::thread::sleep(std::time::Duration::from_secs(gap_secs));
        }
        // 单账号失败不中断整轮（对齐 wb try/except 语义）
        let mut ev = process_account(
            state,
            &agent,
            acct,
            opts,
            &mut learned_ids,
            &mut campaign_table,
            shared.as_ref(),
        );
        ev["index"] = json!(i + 1);
        emit(&ev);
        events.push(ev);
    }

    let ok = events.iter().filter(|e| e["status"] == "success").count();
    let already = events.iter().filter(|e| e["status"] == "already").count();
    let failed = events.len() - ok - already;
    // empty_campaigns 单列（审查 L-empty_campaigns）：活动未开始/不可用属非用户可操作
    // 失败，启动补签推送按 failed - failed_empty_campaigns 判定，避免无效打扰。
    // 判定读结构化 fail_kind 字段（process_account 产出），字段值取自
    // EMPTY_CAMPAIGNS_TAG 常量，与产源/派生同源
    let failed_empty_campaigns = events
        .iter()
        .filter(|e| {
            e["status"] == "fail"
                && e.get("fail_kind").and_then(Value::as_str) == Some(EMPTY_CAMPAIGNS_TAG)
        })
        .count();
    // 永久性认证失败单列（审查 minor）：pat_rejected/expired_needs_relogin/auth_dead
    // 重试注定失败，调度器从重试判定中剔除，避免全天约 28 轮无效重试 + 必败刷新请求
    let failed_permanent = events
        .iter()
        .filter(|e| {
            e["status"] == "fail"
                && e.get("fail_kind").and_then(Value::as_str) == Some(PERMANENT_AUTH_TAG)
        })
        .count();
    let done = json!({
        "type": "done", "ok": ok, "already": already, "failed": failed,
        "failed_empty_campaigns": failed_empty_campaigns,
        "failed_permanent": failed_permanent,
    });
    emit(&done);
    append_results(state, &events);
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn daily_opts_defaults() {
        let o = QoderCheckinOpts::daily();
        assert!(o.skip_checked);
        assert_eq!(o.lazy_hours, 24);
    }

    /// R-9 抓包样本：claim 成功响应顶层含 grantId/status/replayed + 完整 benefit
    #[test]
    fn claim_reward_from_campaigns_entry_or_claim_body() {
        // campaigns 条目 benefit.amount 优先
        let c = json!({"campaignId": "c1", "benefit": {"amount": 100}});
        let known = c
            .get("benefit")
            .and_then(|x| x.get("amount"))
            .and_then(|v| num_or_none(Some(v)));
        assert_eq!(known, Some(100.0));
        // claim 响应顶层 benefit.amount 兜底（campaigns 条目缺 benefit 时）
        let claim_body = json!({"grantId": "g", "status": "CLAIMED", "replayed": false,
                                "benefit": {"kind": "CREDITS", "amount": 100}});
        let body_reward = claim_body
            .get("benefit")
            .and_then(|x| x.get("amount"))
            .and_then(|v| num_or_none(Some(v)));
        assert_eq!(body_reward, Some(100.0));
        assert_eq!(known.or(body_reward), Some(100.0));
    }

    #[test]
    fn claimable_filter_is_case_insensitive() {
        // filter_claimable 的过滤口径：CLAIMABLE（大小写宽容）命中
        let c = json!({"campaignId": "c1", "claimStatus": "claimable", "benefit": {"amount": 100}});
        let st = s_of(fs_utils::dig(&c, &["claimStatus"])).to_ascii_uppercase();
        assert_eq!(st, "CLAIMABLE");
        let claimed = json!({"campaignId": "c2", "claimStatus": "CLAIMED"});
        let st2 = s_of(fs_utils::dig(&claimed, &["claimStatus"])).to_ascii_uppercase();
        assert_ne!(st2, "CLAIMABLE");
    }

    /// P2 归并口径：success > fail > already——[already, fail] 必须判 fail
    ///（原实现归并为 already 掩盖失败），[success, fail] 判 success（部分失败不掩盖成功）
    #[test]
    fn merge_claim_kind_priority() {
        assert_eq!(merge_claim_kind("", "already"), "already");
        assert_eq!(merge_claim_kind("already", "fail"), "fail");
        assert_eq!(merge_claim_kind("fail", "already"), "fail");
        assert_eq!(merge_claim_kind("success", "fail"), "success");
        assert_eq!(merge_claim_kind("fail", "success"), "success");
        assert_eq!(merge_claim_kind("already", "already"), "already");
    }

    /// M1 claim 失败复查恢复：网络异常后按 campaignId 复查 claimStatus==CLAIMED
    #[test]
    fn campaign_is_claimed_recheck() {
        let claimed = json!({"campaignId": "c1", "claimStatus": "CLAIMED"});
        let claimable = json!({"campaignId": "c1", "claimStatus": "CLAIMABLE"});
        let other = json!({"campaignId": "c2", "claimStatus": "CLAIMED"});
        // id 匹配 + 已 CLAIMED → true
        assert!(campaign_is_claimed(&[claimed.clone()], "c1"));
        // 同 id 仍 CLAIMABLE → false（复查不通过，保持 fail）
        assert!(!campaign_is_claimed(&[claimable], "c1"));
        // id 不匹配 → false
        assert!(!campaign_is_claimed(&[other], "c1"));
        // 空列表 → false
        assert!(!campaign_is_claimed(&[], "c1"));
        // 大小写宽容 + snake_case 候选键（与 filter_claimable 口径同构）
        let lower = json!({"campaign_id": "c1", "claim_status": "claimed"});
        assert!(campaign_is_claimed(&[lower], "c1"));
    }

    #[test]
    fn urls_cover_sash_endpoints() {
        let u = urls_for();
        assert!(u.campaigns.starts_with(qoder_common::OPEN_API_BASE));
        assert!(u.campaigns.contains("/sash/api/v1/me/campaigns"));
    }

    /// 兜底常量表 campaignId 路径安全（与 claim_one 同白名单 [A-Za-z0-9_-]）：
    /// 非法字符会改变 claim URL 路径语义，must 在表内即被拦下
    #[test]
    fn known_daily_campaigns_ids_path_safe() {
        assert!(!KNOWN_DAILY_CAMPAIGNS.is_empty());
        for (cid, cname) in KNOWN_DAILY_CAMPAIGNS {
            assert!(
                !cid.is_empty()
                    && cid
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "KNOWN_DAILY_CAMPAIGNS 含非法 campaignId：{cid}"
            );
            assert!(!cname.is_empty(), "KNOWN_DAILY_CAMPAIGNS 条目缺展示名：{cid}");
        }
    }

    /// 「每日 100 Credits」已领判定（从严口径）：CLAIMED+CLAIM_BENEFIT+CREDITS+
    /// 当日窗口全部命中才算已领——缺任一条件不得误判 already（宁 fail 不假 already）
    #[test]
    fn daily_credits_claimed_detection() {
        // act-20260930-664 当日窗口（startAt/endAt 取抓包实测值）
        let now = 1_791_380_000i64;
        // 完整命中 → 已领
        let hit = json!({
            "campaignId": "01a0f1cd-945c-7217-b373-f62e70da792d",
            "claimStatus": "CLAIMED", "actionType": "CLAIM_BENEFIT",
            "benefit": {"kind": "CREDITS", "amount": 100},
            "startAt": 1_791_338_400, "endAt": 1_791_424_740,
        });
        assert!(daily_credits_claimed_in_list(&[hit], now));
        // VIEW_DETAILS（bogo 只看不领）→ 不命中
        let view = json!({"claimStatus": "CLAIMED", "actionType": "VIEW_DETAILS",
                          "benefit": {"kind": "CREDITS"},
                          "startAt": 1_791_338_400, "endAt": 1_791_424_740});
        assert!(!daily_credits_claimed_in_list(&[view], now));
        // 窗口外（昨日已领 grant 无条件展示在列表）→ 不命中
        let expired = json!({"claimStatus": "CLAIMED", "actionType": "CLAIM_BENEFIT",
                             "benefit": {"kind": "CREDITS"},
                             "startAt": 1_791_252_000, "endAt": 1_791_260_640});
        assert!(!daily_credits_claimed_in_list(&[expired], now));
        // 仍 CLAIMABLE → 不命中
        let claimable = json!({"claimStatus": "CLAIMABLE", "actionType": "CLAIM_BENEFIT",
                               "benefit": {"kind": "CREDITS"},
                               "startAt": 1_791_338_400, "endAt": 1_791_424_740});
        assert!(!daily_credits_claimed_in_list(&[claimable], now));
        // 字段缺失从严（无 benefit.kind / 无窗口）→ 不命中
        let bare = json!({"claimStatus": "CLAIMED", "actionType": "CLAIM_BENEFIT"});
        assert!(!daily_credits_claimed_in_list(&[bare], now));
        // 大小写宽容 + snake_case 候选键（claimStatus 口径与 filter_claimable 同构）
        let lower = json!({"claim_status": "claimed", "actionType": "claim_benefit",
                           "benefit": {"kind": "credits"},
                           "startAt": 1_791_338_400, "endAt": 1_791_424_740});
        assert!(daily_credits_claimed_in_list(&[lower], now));
    }

    /// 盲发单步归类：非明确成功/回放形态一律 Failed（宁 fail 不假 already）；
    /// 401 单列走 auth 自愈；显式非 CREDITS 领取不误报 success/already
    #[test]
    fn blind_step_classification() {
        use BlindStep::*;
        // 200+CLAIMED+!replayed → 真实新领取（kind 缺失不降级：宁不判负不误杀真实 success）
        assert!(matches!(classify_blind_step(200, Some("CLAIMED"), false, None), FreshClaim));
        assert!(matches!(classify_blind_step(201, Some("CLAIMED"), false, None), FreshClaim));
        // 幂等回放
        assert!(matches!(classify_blind_step(200, Some("CLAIMED"), true, None), Replay));
        // 200 但非 CLAIMED 形态（未知响应结构）→ Failed（不再宽容吞掉）
        assert!(matches!(classify_blind_step(200, None, false, None), Failed(200)));
        assert!(matches!(
            classify_blind_step(200, Some("PENDING"), false, None),
            Failed(200)
        ));
        // 4xx/5xx/网络 status=0 → Failed
        assert!(matches!(classify_blind_step(400, None, false, None), Failed(400)));
        assert!(matches!(classify_blind_step(500, None, false, None), Failed(500)));
        assert!(matches!(classify_blind_step(0, None, false, None), Failed(0)));
        // 401 → Auth
        assert!(matches!(classify_blind_step(401, None, false, None), Auth));

        // ── 奖励类型防线（审查 P2）：benefit.kind 非 CREDITS 的领取不误报 ──
        // ① CREDITS 正向（含大小写宽容）→ FreshClaim / Replay 不受影响
        assert!(matches!(
            classify_blind_step(200, Some("CLAIMED"), false, Some("CREDITS")),
            FreshClaim
        ));
        assert!(matches!(
            classify_blind_step(200, Some("CLAIMED"), false, Some("credits")),
            FreshClaim
        ));
        // ② 显式非 CREDITS（如 BOGO）的领取 → 降级 Failed（不误报 success）
        assert!(matches!(
            classify_blind_step(200, Some("CLAIMED"), false, Some("BOGO")),
            Failed(200)
        ));
        // ③ 显式非 CREDITS 的回放 → 降级 Failed（不误报 already）
        assert!(matches!(
            classify_blind_step(200, Some("CLAIMED"), true, Some("BOGO")),
            Failed(200)
        ));
        // ④ CREDITS 回放 → Replay 保持
        assert!(matches!(
            classify_blind_step(200, Some("CLAIMED"), true, Some("CREDITS")),
            Replay
        ));
        // ⑤ kind 缺失 → 不降级（宁不判负不误杀真实 success）
        assert!(matches!(classify_blind_step(200, Some("CLAIMED"), false, None), FreshClaim));
        assert!(matches!(classify_blind_step(200, Some("CLAIMED"), true, None), Replay));
        // ⑥ 401 Auth 优先，不受 kind 遮蔽
        assert!(matches!(
            classify_blind_step(401, Some("CLAIMED"), false, Some("BOGO")),
            Auth
        ));
    }

    /// 盲发失败消息构造：BLOCKED/风控依赖不可用给可操作提示（不倾倒原始
    /// JSON），未知形态保留旧格式（HTTP 码 + 原始响应前缀）
    #[test]
    fn describe_blind_failure_friendly_for_known_shapes() {
        // 实测 BLOCKED 形态（2026-10-07 nick 账号）：HTTP 200 + status=BLOCKED
        let raw = r#"{"grantId":"01a114bf-5a68-712d-bc12-5fd8391baa59","status":"BLOCKED","replayed":false}"#;
        let msg = describe_blind_failure(200, Some("BLOCKED"), raw);
        assert!(msg.contains("风控拦截"), "{msg}");
        assert!(msg.contains("BLOCKED"), "{msg}");
        assert!(msg.contains("客户端"), "{msg}");
        assert!(!msg.contains("grantId"), "BLOCKED 形态不得倾倒原始 JSON：{msg}");
        // 503 风控依赖不可用
        let msg = describe_blind_failure(
            503,
            None,
            r#"{"code":{"message":"risk dependency unavailable","code":"RISK_DEPENDENCY_UNAVAILABLE"}}"#,
        );
        assert!(msg.contains("RISK_DEPENDENCY_UNAVAILABLE"), "{msg}");
        assert!(msg.contains("自动重试"), "{msg}");
        // 未知形态：保持旧格式（HTTP 码 + 原始响应前缀）
        assert_eq!(
            describe_blind_failure(404, None, r#"{"message":"not found"}"#),
            r#"（HTTP 404）{"message":"not found"}"#
        );
        // 409 CAMPAIGN_NOT_ACTIVE（issue #66 健壮性②）：专门分支，可读归因
        let msg = describe_blind_failure(
            409,
            None,
            r#"{"code":{"message":"campaign not active","code":"CAMPAIGN_NOT_ACTIVE"}}"#,
        );
        assert!(msg.contains("CAMPAIGN_NOT_ACTIVE"), "{msg}");
        assert!(msg.contains("过期"), "{msg}");
        assert!(msg.contains("重学"), "{msg}");
        // 409 但非 CAMPAIGN_NOT_ACTIVE 形态：仍走通用格式（不误标）
        assert_eq!(
            describe_blind_failure(409, None, r#"{"message":"conflict"}"#),
            r#"（HTTP 409）{"message":"conflict"}"#
        );
    }

    /// issue #114 回归防线：最小头只含鉴权四件套，绝不携带设备指纹头族——
    /// claim 端点带指纹头被上游风控按形态拦截（503 RISK_DEPENDENCY_UNAVAILABLE），
    /// 全部 claim 路径（claim_one / blind_round）都经本函数构建请求头
    #[test]
    fn minimal_auth_headers_excludes_device_fingerprint_family() {
        let h = minimal_auth_headers("tok");
        let keys: Vec<String> = h.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        assert_eq!(keys.len(), 4, "最小头应恰为鉴权四件套：{keys:?}");
        for k in ["authorization", "cosy-clienttype", "user-agent", "content-type"] {
            assert!(keys.iter().any(|x| x == k), "最小头缺 {k}：{keys:?}");
        }
        for k in [
            "cosy-machineid",
            "cosy-machinetoken",
            "cosy-version",
            "cosy-machineos",
            "cosy-machinehostname",
            "cosy-machinecode",
            "cosy-machinetype",
        ] {
            assert!(!keys.iter().any(|x| x == k), "最小头不得携带设备指纹头 {k}");
        }
        assert_eq!(h[0].1, "Bearer tok");
    }

    /// 重学习（issue #66 健壮性①）：不要求 claimStatus（CLAIMED/CLAIMABLE 均学），
    /// 窗口/actionType/benefit 口径与 daily_credits_claimed_in_list 对齐；
    /// 非法 id 丢弃；展示名缺失回退 id 前 8 位
    #[test]
    fn learn_daily_campaign_ids_from_list() {
        let now = 1_791_380_000i64;
        let win = (1_791_338_400, 1_791_424_740);
        // 已领 grant（列表零 CLAIMABLE 场景的典型条目）→ 可学
        let claimed = json!({
            "campaignId": "01a0f1cd-945c-7217-b373-f62e70da792d",
            "claimStatus": "CLAIMED", "actionType": "CLAIM_BENEFIT",
            "benefit": {"kind": "CREDITS", "amount": 100},
            "startAt": win.0, "endAt": win.1,
        });
        let out = learn_daily_campaign_ids(&[claimed], now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "01a0f1cd-945c-7217-b373-f62e70da792d");
        // 仍 CLAIMABLE → 同样可学（保证零 CLAIMABLE 视野前先学）
        let claimable = json!({
            "campaignId": "aaaa1111-bbbb-7ccc-dddd-eeeeffff0000",
            "claimStatus": "CLAIMABLE", "actionType": "CLAIM_BENEFIT",
            "benefit": {"kind": "CREDITS"},
            "startAt": win.0, "endAt": win.1,
        });
        let out = learn_daily_campaign_ids(&[claimable.clone()], now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "aaaa1111-bbbb-7ccc-dddd-eeeeffff0000");
        // VIEW_DETAILS（只看不领）→ 不学
        let view = json!({"campaignId": "v1", "actionType": "VIEW_DETAILS",
                          "benefit": {"kind": "CREDITS"}, "startAt": win.0, "endAt": win.1});
        assert!(learn_daily_campaign_ids(&[view], now).is_empty());
        // 窗口外（昨日活动）→ 不学
        let expired = json!({"campaignId": "e1", "actionType": "CLAIM_BENEFIT",
                             "benefit": {"kind": "CREDITS"},
                             "startAt": win.0 - 86_400, "endAt": win.1 - 86_400});
        assert!(learn_daily_campaign_ids(&[expired], now).is_empty());
        // benefit.kind 非 CREDITS / 字段缺失 → 不学（从严）
        let non_credits = json!({"campaignId": "n1", "actionType": "CLAIM_BENEFIT",
                                 "benefit": {"kind": "BOGO"}, "startAt": win.0, "endAt": win.1});
        let bare = json!({"campaignId": "b1", "actionType": "CLAIM_BENEFIT"});
        assert!(learn_daily_campaign_ids(&[non_credits, bare], now).is_empty());
        // 非法 id（路径白名单外字符）→ 丢弃
        let evil = json!({"campaignId": "../evil?", "actionType": "CLAIM_BENEFIT",
                          "benefit": {"kind": "CREDITS"}, "startAt": win.0, "endAt": win.1});
        assert!(learn_daily_campaign_ids(&[evil], now).is_empty());
        // 展示名缺失 → 回退 id 前 8 位；snake_case 候选键同构
        let noname = json!({"campaign_id": "abcdefgh-1234", "claim_status": "claimed",
                            "actionType": "claim_benefit", "benefit": {"kind": "credits"},
                            "startAt": win.0, "endAt": win.1});
        let out = learn_daily_campaign_ids(&[noname], now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, "abcdefgh");
    }

    /// 学习结果按 id 去重：同 id 保留先到条目（learned 优先），不同 id 全保留
    #[test]
    fn merge_learned_dedups_by_id() {
        let mut learned = vec![("id-a".to_string(), "先到".to_string())];
        merge_learned(
            &mut learned,
            vec![("id-a".to_string(), "后到".to_string()), ("id-b".to_string(), "新".to_string())],
        );
        assert_eq!(learned.len(), 2);
        assert_eq!(learned[0], ("id-a".to_string(), "先到".to_string()));
        assert_eq!(learned[1], ("id-b".to_string(), "新".to_string()));
    }

    /// conf 覆盖表解析（PR #106 移植）：裸数组/对象包裹两种格式皆收；
    /// id 路径白名单/长度校验；name 缺失回退 id 前 8 位；垃圾输入零条目
    #[test]
    fn parse_conf_campaigns_shapes() {
        // 裸数组
        let out = parse_conf_campaigns(
            r#"[{"id":"abc-123","name":"每日领 100 Credits"}]"#,
        );
        assert_eq!(out, vec![("abc-123".to_string(), "每日领 100 Credits".to_string())]);
        // 对象包裹 + campaignId 键 + name 缺失回退 id 前 8 位
        let out = parse_conf_campaigns(
            r#"{"campaigns":[{"campaignId":"01a130ea-7c31-71b3-a22b-cbe019571350"}]}"#,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, "01a130ea");
        // 非法 id（路径白名单外字符）与缺 id 条目跳过；合法条目保留
        let out = parse_conf_campaigns(
            r#"[{"id":"../evil?"},{"id":"ok-1"},{"name":"no-id"}]"#,
        );
        assert_eq!(out, vec![("ok-1".to_string(), "ok-1".to_string())]);
        // 超长 id 拒绝
        let long = "a".repeat(129);
        let out = parse_conf_campaigns(&format!(r#"[{{"id":"{long}"}}]"#));
        assert!(out.is_empty());
        // 垃圾输入 → 空（load_campaign_table 负责留痕，解析层保持纯函数）
        assert!(parse_conf_campaigns("not json").is_empty());
        assert!(parse_conf_campaigns(r#"{"other":1}"#).is_empty());
    }

    /// 候选表合并（PR #106 移植）：conf 条目在前（人工下发的最新 id 优先盲发），
    /// 常量表按 id 去重补后；conf 为空 → 恰为常量表
    #[test]
    fn merge_campaign_table_conf_first() {
        let conf = vec![("new-id".to_string(), "新".to_string())];
        let builtins = [("new-id", "重不覆盖"), ("old-id", "旧")];
        let out = merge_campaign_table(conf, &builtins);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], ("new-id".to_string(), "新".to_string()));
        assert_eq!(out[1], ("old-id".to_string(), "旧".to_string()));
        // conf 为空 → 恰为常量表
        let out = merge_campaign_table(Vec::new(), &builtins);
        assert_eq!(out.len(), 2);
    }

    /// 宽松档学习（issue #105）：严格档产空时按档 A（去窗口）→ 档 B（去
    /// actionType）→ 档 C（名称含「每日」）取首个非空档；非法 id 各档均丢弃；
    /// 严格档命中时不走宽松档
    #[test]
    fn learn_daily_campaign_ids_lenient_tiers() {
        // 档 A：actionType+kind 命中但窗口字段缺失（严格档学不到的场景）
        let no_window = json!({
            "campaignId": "aaaa1111-bbbb-7ccc-dddd-eeeeffff0001",
            "claimStatus": "CLAIMABLE", "actionType": "CLAIM_BENEFIT",
            "benefit": {"kind": "CREDITS", "amount": 100},
        });
        let out = learn_daily_campaign_ids_lenient(&[no_window.clone()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "aaaa1111-bbbb-7ccc-dddd-eeeeffff0001");
        // 同条目严格档学不到（无窗口 = 从严丢弃）
        assert!(learn_daily_campaign_ids(&[no_window.clone()], 1_791_380_000).is_empty());
        // 严格档命中（带窗口）→ 宽松档入口不影响主路径（宽松档仅在重学路径调用）
        let with_window = json!({
            "campaignId": "aaaa1111-bbbb-7ccc-dddd-eeeeffff0002",
            "actionType": "CLAIM_BENEFIT", "benefit": {"kind": "CREDITS"},
            "startAt": 1_791_338_400i64, "endAt": 1_791_424_740i64,
        });
        let out = learn_daily_campaign_ids_lenient(&[with_window]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "aaaa1111-bbbb-7ccc-dddd-eeeeffff0002");
        // 档 B：actionType 枚举变更（kind 仍 CREDITS）→ 档 A 空，档 B 命中
        let renamed_action = json!({
            "campaignId": "b1", "actionType": "DAILY_REWARD",
            "benefit": {"kind": "CREDITS"},
        });
        let out = learn_daily_campaign_ids_lenient(&[renamed_action]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "b1");
        // 档 C：kind 也变更，但 actionType+名称含「每日」→ 命中
        let renamed_kind = json!({
            "campaignId": "c1", "actionType": "CLAIM_BENEFIT",
            "benefit": {"kind": "DAILY_POINT"}, "name": "每日领 100 Credits",
        });
        let out = learn_daily_campaign_ids_lenient(&[renamed_kind]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "c1");
        // 档 C 不收无关活动（名称不含「每日」）
        let unrelated = json!({
            "campaignId": "u1", "actionType": "CLAIM_BENEFIT",
            "benefit": {"kind": "DAILY_POINT"}, "name": "新人礼包",
        });
        assert!(learn_daily_campaign_ids_lenient(&[unrelated]).is_empty());
        // 非法 id 各档均丢弃
        let evil = json!({"campaignId": "../evil?", "actionType": "CLAIM_BENEFIT",
                          "benefit": {"kind": "CREDITS"}});
        assert!(learn_daily_campaign_ids_lenient(&[evil]).is_empty());
        // 全空列表 → 空产出（当轮重学放弃重试的路径）
        assert!(learn_daily_campaign_ids_lenient(&[]).is_empty());
    }

    /// BlindRound 语义（issue #105）：stale_only 触发条件与跨轮合并后定级
    #[test]
    fn blind_round_stale_only_and_merge() {
        let stale_only = || BlindRound {
            messages: vec![],
            fail_parts: vec![],
            stale_parts: vec!["stale".to_string()],
            reward: None,
            any_fresh: false,
            any_replay: false,
            auth: false,
        };
        assert!(stale_only().stale_only());
        // 任一新鲜成功/回放/真实失败/auth 中断 → 不触发当轮重学
        let mut r = stale_only();
        r.any_fresh = true;
        assert!(!r.stale_only());
        let mut r = stale_only();
        r.any_replay = true;
        assert!(!r.stale_only());
        let mut r = stale_only();
        r.fail_parts.push("real fail".to_string());
        assert!(!r.stale_only());
        let mut r = stale_only();
        r.auth = true;
        assert!(!r.stale_only());
        // stale_parts 为空（如首轮即 401 中断的空轮）→ 不触发
        let mut r = stale_only();
        r.stale_parts.clear();
        assert!(!r.stale_only());
        // 跨轮合并：stale 轮 + 新鲜成功轮 → fresh 胜出（aggregate 归 Success），
        // reward 累加，auth 标记保留
        let mut fresh = BlindRound {
            messages: vec!["每日领 100 Credits（盲发直领）".to_string()],
            fail_parts: vec![],
            stale_parts: vec![],
            reward: Some(100.0),
            any_fresh: true,
            any_replay: false,
            auth: false,
        };
        fresh.auth = true;
        let merged = stale_only().merge(fresh);
        assert!(merged.auth);
        assert!(matches!(merged.aggregate(), FallbackOutcome::Success(_, Some(100.0))));
        // stale 轮 + 再 stale 轮 → Failed（两轮 stale 文案拼接，触发调度器重试）
        let merged = stale_only().merge(stale_only());
        assert!(matches!(
            merged.aggregate(),
            FallbackOutcome::Failed(m, None) if m.matches("stale").count() == 2
        ));
    }

    /// 盲发聚合判定（issue #66 健壮性②）：stale（409 id 过期）不掩码新鲜成功/
    /// 回放；fail_parts 掩码一切（宁 fail 不假成功）；空表兜底报 fail
    #[test]
    fn aggregate_fallback_priority() {
        let fresh = vec!["每日领 100 Credits（盲发直领）".to_string()];
        // 新鲜成功 + stale → Success（id 错不掩码真实到账）
        assert!(matches!(
            aggregate_fallback(true, false, &fresh, &[], &["stale".to_string()], Some(100.0)),
            FallbackOutcome::Success(_, Some(100.0))
        ));
        // 回放 + stale → AlreadyReplayed（服务端确认已领）
        assert!(matches!(
            aggregate_fallback(false, true, &[], &[], &["stale".to_string()], None),
            FallbackOutcome::AlreadyReplayed
        ));
        // 仅 stale → 如实 Failed（触发重试，重试轮次重学 id）；reward 无成功来源 → None
        assert!(matches!(
            aggregate_fallback(false, false, &[], &[], &["stale".to_string()], None),
            FallbackOutcome::Failed(m, None) if m.contains("stale")
        ));
        // 真实失败掩码一切（含新鲜成功：宁 fail 不假 success）——但已领到的
        // reward 随行不丢弃（真实入账不因整体 fail 而回滚，对齐 Auth 语义）
        assert!(matches!(
            aggregate_fallback(true, false, &fresh, &["real fail".to_string()], &["stale".to_string()], Some(100.0)),
            FallbackOutcome::Failed(m, Some(100.0)) if m.contains("real fail") && m.contains("盲发直领")
        ));
        assert!(matches!(
            aggregate_fallback(false, true, &[], &["real fail".to_string()], &[], None),
            FallbackOutcome::Failed(m, None) if m.contains("real fail")
        ));
        // 全空（表空/全被白名单拦下）→ Failed 兜底；空表 reward 必为 None（透传等价）
        assert!(matches!(
            aggregate_fallback(false, false, &[], &[], &[], None),
            FallbackOutcome::Failed(m, None) if m.contains("表为空")
        ));
    }

    /// filter_claimable：CLAIMABLE 大小写宽容 + 非 CLAIMABLE 全排除
    #[test]
    fn filter_claimable_basic() {
        let list = vec![
            json!({"campaignId": "a", "claimStatus": "CLAIMABLE"}),
            json!({"campaignId": "b", "claimStatus": "claimable"}),
            json!({"campaignId": "c", "claimStatus": "CLAIMED"}),
            json!({"campaignId": "d"}),
        ];
        let out = filter_claimable(&list);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["campaignId"], "a");
        assert_eq!(out[1]["campaignId"], "b");
    }

    // ── sash campaigns 视图分桶探针（#[ignore]：cargo test probe_sash_campaigns -- --ignored --nocapture）──
    // 背景：2026-10-05 每日 100 积分活动（act-20260930-295）客户端 CLAIMABLE 期间
    //（proxy 抓包 10:14-10:15 确认，10:15:17 才被客户端首次领取 replayed:false），
    // 工具三轮报 already——收到的是非空 campaigns 数组但零 CLAIMABLE
    //（empty_campaigns 诊断日志未触发）。工具 ureq 直连不经进程内代理，其响应无法
    // 从 proxy 日志观察。本探针用池账号真凭证分别以「工具当前头」与「补齐客户端
    // 形态头」请求 campaigns，对比响应 uid / 顶层 claimable / 各条 claimStatus，
    // 锁定服务端按请求头（Cosy-Version/设备头族）或凭证形态分桶隐藏活动的证据。
    // 注意：应用运行时 vault 快照可能被锁，凭证读取降级为空（探针跳过该账号）。

    /// 补齐客户端形态的设备头族（抓包 2026-10-05 10:14 客户端头逐项对齐；
    /// MachineCode/MachineType/Hostname 无真值时用占位——服务端若校验配对即拒绝，
    /// 拒绝本身也是证据）
    fn probe_full_client_headers(base: &[(String, String)]) -> Vec<(String, String)> {
        let has = |h: &[(String, String)], k: &str| {
            h.iter().any(|(a, _)| a.eq_ignore_ascii_case(k))
        };
        let mut h = base.to_vec();
        for (k, v) in [
            ("Cosy-Version", "0.4.3"),
            ("Cosy-MachineOS", "x86_64_win32"),
            ("Cosy-MachineHostname", "probe-host"),
            ("Cosy-MachineCode", "0000000000000000000"),
            ("Cosy-MachineType", "000000000000000000"),
        ] {
            if !has(&h, k) {
                h.push((k.to_string(), v.to_string()));
            }
        }
        h
    }

    fn print_campaigns_view(tag: &str, status: u16, body: Option<&Value>, raw: &str) {
        println!("[{tag}] HTTP {status}");
        match body {
            Some(b) => {
                println!(
                    "  uid = {}, top_claimable = {:?}",
                    b.get("uid").and_then(Value::as_str).unwrap_or("<none>"),
                    b.get("claimable")
                );
                match b.get("campaigns").and_then(Value::as_array) {
                    Some(arr) if !arr.is_empty() => {
                        for c in arr {
                            println!(
                                "  campaign {} key={} action={} status={} amount={:?}",
                                c.get("campaignId").and_then(Value::as_str).unwrap_or("?"),
                                c.get("campaignKey").and_then(Value::as_str).unwrap_or("?"),
                                c.get("actionType").and_then(Value::as_str).unwrap_or("?"),
                                c.get("claimStatus").and_then(Value::as_str).unwrap_or("?"),
                                c.get("benefit").and_then(|x| x.get("amount")),
                            );
                        }
                    }
                    Some(_) => println!("  <campaigns 空数组>"),
                    None => println!("  <campaigns 键缺失> head={}", &raw[..raw.len().min(200)]),
                }
            }
            None => println!("  <非 JSON> {}", &raw[..raw.len().min(300)]),
        }
    }

    #[test]
    #[ignore]
    fn probe_sash_campaigns_headers() {
        let state = crate::state::AppState::new().expect("构造 AppState 失败");
        let agent = crate::tasks::http_agent(30);
        let urls = urls_for();
        let pool = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
        let accounts = pool
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(!accounts.is_empty(), "Qoder 账号池为空");
        for a in &accounts {
            let acct_id = a.get("id").and_then(Value::as_str).unwrap_or("?").to_string();
            let (creds, _r, note) = qoder_common::ensure_fresh(&state, &agent, &acct_id, 24);
            println!("===== acct {acct_id} ensure_fresh={note}");
            if creds.access_token.is_empty() {
                println!("  <无可用凭证（vault 被锁或池空），跳过>");
                continue;
            }
            println!(
                "  stored_uid = {}, token_prefix = {}, machine_id = {}, has_machine_token = {}",
                creds.uid,
                &creds.access_token[..creds.access_token.len().min(6)],
                creds.machine_id,
                !creds.machine_token.is_empty(),
            );
            // A 组：工具现状头（build_auth_headers 原样）
            let (st, body, raw) =
                qoder_common::get_json(&agent, &urls.campaigns, &qoder_common::build_auth_headers(&creds));
            print_campaigns_view("A 工具现状头", st, body.as_ref(), &raw);
            // B 组：补齐客户端形态头
            let full = probe_full_client_headers(&qoder_common::build_auth_headers(&creds));
            let (st2, body2, raw2) = qoder_common::get_json(&agent, &urls.campaigns, &full);
            print_campaigns_view("B 补齐客户端头", st2, body2.as_ref(), &raw2);
            // C 组：盲发幂等 claim 实测（campaignId 取 KNOWN_DAILY_CAMPAIGNS 表首条
            // ——每日活动 ID 每日轮换，硬编码会过期，表随当日活动更新）——
            // 已领账号应回放 replayed:true；列表不可见账号若 200+CLAIMED 证明 claim
            // 不受列表过滤约束（兜底直领可行），4xx 则锁定错误体形态
            let (cid, _cname) = KNOWN_DAILY_CAMPAIGNS[0];
            let c_url = format!(
                "{}/sash/api/v1/me/campaigns/{cid}/claim",
                qoder_common::OPEN_API_BASE
            );
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let (st3, body3, raw3) =
                post_empty(&agent, &c_url, &qoder_common::build_auth_headers(&creds));
            println!(
                "[C 盲发claim {cid}] HTTP {st3} body={}",
                body3.as_ref().map(|b| b.to_string()).unwrap_or_else(|| {
                    raw3.chars().take(300).collect()
                })
            );
            // D 组（issue #114 探针口径）：最小头盲发 claim 对照——上游风控按请求
            // 头形态拦截 claim（2026-10-16 实证：完整头 503 RISK_DEPENDENCY_
            // UNAVAILABLE，最小头 200 CLAIMED 4/4 账号到账）。C/D 两组同 IP 同
            // token 同 id 对照，风控再变时直接锁定拦截面
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let (st4, body4, raw4) =
                post_empty(&agent, &c_url, &minimal_auth_headers(&creds.access_token));
            println!(
                "[D 最小头claim {cid}] HTTP {st4} body={}",
                body4.as_ref().map(|b| b.to_string()).unwrap_or_else(|| {
                    raw4.chars().take(300).collect()
                })
            );
        }
    }
}
