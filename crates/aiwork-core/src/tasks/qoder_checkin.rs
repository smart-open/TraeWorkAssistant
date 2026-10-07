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
//! 双活动；失败进入 30 分钟重试冷却（scheduler RETRY_COOLDOWN_MS 同款）。
//!
//! 风控合规内建（§5.2）：claim 间隔 1~3s 抖动；绝不重试轰炸；设备指纹经
//! effective_creds 注入（§5.10 每账号稳定绑定，真实捕获优先）。
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

/// 拉取活动列表（**全量，不过滤**）。返回 Ok(全量 campaign 列表)；
/// Err(kind, message)：auth（401，调用方刷新重试）| fail。
/// 全量返回的原因：列表内 CLAIMED grant 是「每日奖励已领取」的判定数据源
///（daily_credits_claimed_in_list），CLAIMABLE 过滤交给调用方（filter_claimable）。
fn list_campaigns(
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
    // 信封穿透（fs_utils::dig 含 data 包裹下钻）；⚠ campaigns 为空 ≠ 已签：
    // 可能活动未开始 / Cosy-ClientType 缺失（本层恒带）/ 接口结构变更——
    // 与「无可领项」显式区分，kind=fail 触发 UI 提示（§5.2）
    let campaigns = fs_utils::dig(&b, &["campaigns"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if campaigns.is_empty() {
        return Err((
            "fail".into(),
            format!("{EMPTY_CAMPAIGNS_TAG}：活动列表为空（活动未开始或不可用）"),
        ));
    }
    Ok(campaigns)
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
fn claim_one(
    agent: &ureq::Agent,
    headers: &[(String, String)],
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
    jitter_sleep();
    let (status, body, raw) = post_empty(agent, &url, headers);
    if status == 401 {
        return ("auth".into(), "登录态失效（401）".into(), None);
    }
    if status == 0 {
        // 网络不可达 ≠ 必然失败：claim 可能已到达服务端但响应丢失，
        // 复查 campaigns（GET 幂等安全），已变 CLAIMED 视为已领（M1 复查恢复）
        if recheck_claimed(agent, headers, urls, &campaign_id) {
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
// 根因（探针 probe_sash_campaigns_headers 实证）：campaigns 列表端点对工具
// 请求形态按投放条件过滤 CLAIMABLE 条目（已领 grant 无条件展示），工具全天
// 看不到 CLAIMABLE → 列表零可领项 → 依赖盲发兜底；而 claim 端点不受列表过滤
// 约束（幂等，重复领取返回 replayed=true 回放）。extend_client_device_headers
// 头对齐生效后列表应可见 CLAIMABLE，本表仅作列表仍不可见时的兜底。
//
// ⚠ 每日轮换：每日 100 Credits 活动的 campaignId 每天更换（UUIDv7 前缀+随机
// 后缀，单日 24h 窗口；下表 ID = act-20260930-664，2026-10-07 当日窗口）。过期
// ID 盲发会得到非成功响应——经 classify_blind_step 如实归 Failed（fail 触发
// 30 分钟重试与 UI 真话），不再静默归 already（2026-10-07「已领（此前已领）」
// 误报的直接根因）。ID 失效时需从客户端抓包/探针更新下表。
/// 已知每日活动常量表：(campaignId, 展示名)。campaignId 仅允许 [A-Za-z0-9_-]
///（路径安全白名单，known_daily_fallback 内强制校验）。
const KNOWN_DAILY_CAMPAIGNS: &[(&str, &str)] = &[
    ("01a0f1cd-945c-7217-b373-f62e70da792d", "每日领 100 Credits"),
];

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

fn classify_blind_step(status: u16, claimed: Option<&str>, replayed: bool) -> BlindStep {
    if status == 401 {
        return BlindStep::Auth;
    }
    if (200..=201).contains(&status) {
        return match claimed {
            Some("CLAIMED") if !replayed => BlindStep::FreshClaim,
            Some("CLAIMED") => BlindStep::Replay,
            // 200 但非 CLAIMED 形态（未知响应结构）不宽容——盲发语义已探针锁定
            _ => BlindStep::Failed(status),
        };
    }
    BlindStep::Failed(status)
}

/// 盲发失败的人类可读原因（known_daily_fallback Failed 分支消息构造）。
/// 已探针锁定的形态给可操作提示，其余保留 HTTP 码 + 原始响应前缀（截 120
/// 字符，与旧格式一致）供排障：
/// - 200/BLOCKED：服务端风控明确拒绝本次领取（非依赖故障、非瞬时错误），
///   提示到真实客户端建立设备信任
/// - 503/RISK_DEPENDENCY_UNAVAILABLE：风控前置依赖暂不可用，调度器会自动重试
fn describe_blind_failure(code: u16, claimed: Option<&str>, raw: &str) -> String {
    if claimed == Some("BLOCKED") {
        return "服务端风控拦截（status=BLOCKED，本次领取被拒绝）——请在 Qoder 客户端正常登录/使用一次建立设备信任后重试".into();
    }
    if code == 503 && raw.contains("RISK_DEPENDENCY_UNAVAILABLE") {
        return "服务端风控依赖暂不可用（503 RISK_DEPENDENCY_UNAVAILABLE），稍后将自动重试".into();
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
    /// 30 分钟重试 + UI 显示真话（即使同时有成功条目：claim 幂等，重试轮次
    /// 以列表已领判定/回放确认，不产生重复领取副作用）
    Failed(String),
}

/// 列表零 CLAIMABLE 时的已知每日活动盲发直领。逐条对 KNOWN_DAILY_CAMPAIGNS
/// 盲发 claim，单步经 classify_blind_step 归类后聚合：
/// - 任一 Failed → `Failed`（宁 fail 不假 already；成功/回放条目信息保留在 message）
/// - 无失败且任一 FreshClaim → `Success`
/// - 全部 Replay → `AlreadyReplayed`
/// - 表空/全被路径白名单拦下（无任何结果）→ `Failed`
/// - 任一 401 → 中断遍历返回 `Auth`（已累计 reward 不丢弃）
fn known_daily_fallback(agent: &ureq::Agent, headers: &[(String, String)]) -> FallbackOutcome {
    let mut messages: Vec<String> = Vec::new();
    let mut fail_parts: Vec<String> = Vec::new();
    let mut reward: Option<f64> = None;
    let mut any_fresh = false;
    let mut any_replay = false;
    for (cid, cname) in KNOWN_DAILY_CAMPAIGNS {
        // 路径安全防线：常量表手滑引入非法字符时拒绝该条（与 claim_one 同白名单）
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
        let (status, body, raw) = post_empty(agent, &url, headers);
        let claimed = body
            .as_ref()
            .and_then(|b| fs_utils::dig(b, &["status", "data.status"]))
            .map(|v| s_of(Some(v)).to_ascii_uppercase());
        let replayed = body
            .as_ref()
            .and_then(|b| fs_utils::dig(b, &["replayed", "data.replayed"]))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match classify_blind_step(status, claimed.as_deref(), replayed) {
            BlindStep::Auth => {
                return FallbackOutcome::Auth(reward);
            }
            BlindStep::FreshClaim => {
                let amt = body
                    .as_ref()
                    .and_then(|b| b.get("benefit"))
                    .and_then(|x| x.get("amount"))
                    .and_then(|v| num_or_none(Some(v)));
                any_fresh = true;
                messages.push(format!("{cname}（盲发直领）"));
                if let Some(a) = amt {
                    reward = Some(reward.unwrap_or(0.0) + a);
                }
            }
            BlindStep::Replay => {
                any_replay = true;
            }
            BlindStep::Failed(code) => {
                fail_parts.push(format!(
                    "{cname} 盲发失败：{}",
                    describe_blind_failure(code, claimed.as_deref(), &raw)
                ));
                // 继续尝试下一条：单条失败不中断（聚合时整体归 Failed）
            }
        }
    }
    if !fail_parts.is_empty() {
        let mut msg = fail_parts.join("；");
        if any_fresh {
            msg = format!("{}；{}", messages.join("；"), msg);
        }
        return FallbackOutcome::Failed(msg);
    }
    if any_fresh {
        return FallbackOutcome::Success(messages.join("；"), reward);
    }
    if any_replay {
        return FallbackOutcome::AlreadyReplayed;
    }
    FallbackOutcome::Failed("已知每日活动表为空，无兜底可尝试".into())
}

/// 处理单账号签到（含 401 刷新一次重试，禁二次刷新）。返回 account 事件（不含 index）。
fn process_account(state: &AppState, agent: &ureq::Agent, acct: &Value, opts: &QoderCheckinOpts) -> Value {
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
                       "message": format!("无可用凭证（{note}）") });
        if note == "auth_dead" {
            // 凭证为空且刷新令牌已被永久拒绝：同样属终态，单列免全天重试
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

    let (mut kind, mut message, mut reward) = match list_campaigns(agent, &headers, &urls) {
        Ok(campaigns) => {
            let claimable = filter_claimable(&campaigns);
            if claimable.is_empty() {
                // 列表零 CLAIMABLE：campaigns 列表对工具请求形态按投放过滤
                // CLAIMABLE 条目（已领 grant 无条件展示）。两步定真话：
                // ① 先查列表内当日「每日 100 Credits」CLAIMED grant（服务端对
                //    已领无条件展示，命中即真实 already，不发起多余请求）；
                // ② 否则盲发已知活动兜底直领，结果如实归类（Failed 不再归
                //    already——2026-10-07 误报「已领（此前已领）」的修复点）
                let now = chrono::Utc::now().timestamp();
                if daily_credits_claimed_in_list(&campaigns, now) {
                    ("already".into(), "每日奖励已领取".into(), None)
                } else {
                    match known_daily_fallback(agent, &headers) {
                        FallbackOutcome::Success(m, r) => ("success".into(), m, r),
                        FallbackOutcome::AlreadyReplayed => {
                            ("already".into(), "每日奖励已领取（盲发回放确认）".into(), None)
                        }
                        FallbackOutcome::Auth(r) => {
                            ("auth".into(), "登录态失效（401）".into(), r)
                        }
                        FallbackOutcome::Failed(m) => ("fail".into(), m, None),
                    }
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
                    let (k, m, r) = claim_one(agent, &headers, &urls, c);
                    if k == "auth" {
                        kind = "auth".into();
                        auth_msg = m;
                        // 已领取活动的累计奖励不丢弃（真实入账，原 reward=None 会抹掉）
                        // auth 中断条目不入 campaigns_log（档期日历只收真实领取结果）
                        break;
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
        Err((k, m)) => (k, m, None),
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
                let retry = match list_campaigns(agent, &headers, &urls) {
                    Ok(campaigns) => {
                        let claimable = filter_claimable(&campaigns);
                        if claimable.is_empty() {
                            // 与首次路径同口径：先查列表内当日已领 grant，再盲发
                            // 兜底直领，失败如实归 fail（不掩盖）
                            let now = chrono::Utc::now().timestamp();
                            if daily_credits_claimed_in_list(&campaigns, now) {
                                ("already".into(), "每日奖励已领取".into(), None)
                            } else {
                                match known_daily_fallback(agent, &headers) {
                                    FallbackOutcome::Success(m, r) => ("success".into(), m, r),
                                    FallbackOutcome::AlreadyReplayed => (
                                        "already".into(),
                                        "每日奖励已领取（盲发回放确认）".into(),
                                        None,
                                    ),
                                    FallbackOutcome::Auth(r) => {
                                        ("auth".into(), "登录态失效（401）".into(), r)
                                    }
                                    FallbackOutcome::Failed(m) => ("fail".into(), m, None),
                                }
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
                                let (k, m, r) = claim_one(agent, &headers, &urls, c);
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
                    Err((k, m)) => (k, m, None),
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
    // 奖励差值兜底（对齐 F-17 模式）：接口未返回数额时用签到前后余额差值，仅 >0 采信
    if kind == "success" && reward.is_none() {
        if let Some(pre) = pre_balance {
            if let Some(post) = super::qoder_credits::fetch_usage_balance(agent, &headers) {
                if post > pre {
                    reward = Some(post - pre);
                }
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
    if kind == "success" && note == "refreshed" {
        ev["message"] = json!(format!("{}（凭证已续期）", ev["message"].as_str().unwrap_or("")));
    }
    ev
}

/// 签到整轮：逐账号串行处理，事件经 emit 回调逐条输出（NDJSON 管线复用）。
/// 返回 done 事件（ok/already/failed 计数），供启动补签/调度静默路径直接消费。
pub fn run_checkin_round(state: &AppState, opts: &QoderCheckinOpts, emit: &mut dyn FnMut(&Value)) -> Value {
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
    // 多账号签到间隔（任务配置页可改，默认 3s，0=关闭）：防上游频控，账号间串行等待
    let gap_secs = crate::models::effective_checkin_gap(state.settings().qoder_checkin_gap_secs);
    for (i, acct) in accounts.iter().enumerate() {
        if i > 0 && gap_secs > 0 {
            std::thread::sleep(std::time::Duration::from_secs(gap_secs));
        }
        // 单账号失败不中断整轮（对齐 wb try/except 语义）
        let mut ev = process_account(state, &agent, acct, opts);
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
    /// 401 单列走 auth 自愈
    #[test]
    fn blind_step_classification() {
        use BlindStep::*;
        // 200+CLAIMED+!replayed → 真实新领取
        assert!(matches!(classify_blind_step(200, Some("CLAIMED"), false), FreshClaim));
        assert!(matches!(classify_blind_step(201, Some("CLAIMED"), false), FreshClaim));
        // 幂等回放
        assert!(matches!(classify_blind_step(200, Some("CLAIMED"), true), Replay));
        // 200 但非 CLAIMED 形态（未知响应结构）→ Failed（不再宽容吞掉）
        assert!(matches!(classify_blind_step(200, None, false), Failed(200)));
        assert!(matches!(
            classify_blind_step(200, Some("PENDING"), false),
            Failed(200)
        ));
        // 4xx/5xx/网络 status=0 → Failed
        assert!(matches!(classify_blind_step(400, None, false), Failed(400)));
        assert!(matches!(classify_blind_step(500, None, false), Failed(500)));
        assert!(matches!(classify_blind_step(0, None, false), Failed(0)));
        // 401 → Auth
        assert!(matches!(classify_blind_step(401, None, false), Auth));
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
        }
    }
}
