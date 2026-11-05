//! Buddy 国际版每日活跃打卡（buddy-ai 阶段 3b，§4.5 补篇）。
//!
//! 与国内 wb_checkin 的本质差异（不复用/不污染国内骨架）：
//! - 国际版无 daily-checkin 接口，取而代之是「网页会话 + 沙箱 ACP 跑完一轮对话」
//!   复合动作（R-4 全链实测：completed 仅 3 秒）；
//! - **无 claim 端点**：奖励由服务端在会话 completed 后自动入账（30/50 两档服务端
//!   按套餐决定），客户端仅保证「跑完的 agent 会话」成立即得分；
//! - 桌面腿 chat/completions **不计入**活跃，网页腿（console 会话）才是计数通道；
//! - 网页腿身份 = 复用账号 token + `X-User-Id` + Chrome UA + Origin/Referer，
//!   **不带桌面 X-IDE-\* 指纹头**（UID 派生指纹仅桌面腿用，本任务无桌面腿）。
//!
//! 端点链（全走 `www.workbuddy.ai`，§4.5 补篇 R-4 实测收口）：
//! ① POST /console/as/conversations/（建会话 → data.id）
//! ② GET  /console/as/conversations/{id}/session（沙箱 link + 独立 token + sessionId）
//! ③ GET  <link>（Accept: text/event-stream → 响应头 Acp-Connection-Id）
//! ④ POST <link>（JSON-RPC：initialize → session/load → session/set_model → session/prompt；
//!    HTTP 202 即受理）——**set_model 必发**（省钱红线：缺省按 Auto 模型计费扣积分，
//!    免费模型钉 deepseek-v4.1-flash；外部项目踩坑实证）
//! ⑤ GET  /console/as/conversations/{id}（data.status == "completed"，超时 120s）
//! ⑥ POST /v2/billing/meter/get-user-resource（TotalDosage 对比打卡前快照，R-6 同构）

use serde_json::{json, Value};

use crate::state::AppState;

/// 打卡跑轮免费模型（§4.5 补篇省钱红线：set_model 必发钉免费档）
pub const FREE_MODEL: &str = "deepseek-v4.1-flash";
/// 建会话 prompt（外部项目同款最小一轮）
const PROMPT_TEXT: &str = "Hi";
/// ⑤ 完成判定轮询超时（可配口径默认 120s）+ 轮询间隔
/// 会话完成判定窗口（180s：实测 17:52 轮 conv 已建但 120s 未完成判失败，
/// 4 分钟后调度轮同账号成功——沙箱对话偶发慢，放宽到 3 分钟减少误判）
const COMPLETE_TIMEOUT_SECS: u64 = 180;
const COMPLETE_POLL_SECS: u64 = 3;
/// 出口级瞬时抖动退避：1s 起翻倍封顶 15s，每步最多 3 次重试
const STEP_RETRIES: u32 = 3;

/// 秒级退避（§4.5 落码要点：1s 起翻倍封顶 15s，避免一次抖动熔断整池）
fn backoff_sleep(attempt: u32) {
    let secs = 1u64.saturating_mul(1 << attempt.min(4)).min(15);
    std::thread::sleep(std::time::Duration::from_secs(secs));
}

/// 本地日期幂等戳（YYYY-MM-DD；与调度器 last_run_date 同口径）
pub fn today_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

// ── 积分快照（R-6 字段链：TotalDosage 即总积分口径）─────────────────────────

/// dig 候选穿透包装（fs_utils::dig 返回 Option<&Value>，本模块统一借用法）
fn digv<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    crate::fs_utils::dig(v, path)
}

/// 拉取单账号积分快照：POST {base}/v2/billing/meter/get-user-resource。
/// 返回 (TotalDosage, 最早未来 ExpiredTime 秒)；解析失败恒 None（调用方容错）。
/// 积分包明细（P9 抓包校准：intl 包条目含 PackageName/CapacityRemain(Precise)/
/// CapacitySize/CycleEndTime；Status 0=有效 3=宽限期）
#[derive(Debug, Clone, serde::Serialize)]
pub struct DosagePackage {
    pub name: String,
    pub code: String,
    pub remain: f64,
    pub size: f64,
    /// 包到期（秒级；CycleEndTime 按 +08:00 墙钟解析）
    pub expire_secs: Option<i64>,
}

/// get-user-resource 深度解析结果（P9 抓包校准）：total + 最早到期 + 分包明细。
/// 到期来源修正：intl 包 `ExpiredTime` 恒为空串，真实到期是 `CycleEndTime`
/// 字符串日期时间（"2026-10-29 08:47:12"）——原实现只收集数值型 ExpiredTime，
/// credits_expire_at 永远 None（看板到期列恒空）
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct DosageInfo {
    pub total: f64,
    pub expire_secs: Option<i64>,
    pub packages: Vec<DosagePackage>,
    /// 解析诊断（issue #179）：包明细为空 / 出现未知 Status 值时携带响应骨架，
    /// None = 正常形态。调用方落 app_log 留证（响应体不含 token，无敏感泄漏面）
    pub parse_diag: Option<String>,
}

/// 官网日期时间字符串解析（"YYYY-MM-DD HH:MM:SS"，+08:00 墙钟 → epoch 秒）
pub fn parse_cn_datetime(s: &str) -> Option<i64> {
    use chrono::TimeZone;
    let dt = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
    Some(offset.from_local_datetime(&dt).single()?.timestamp())
}

/// 到期字符串宽容解析（issue #179）：官网墙钟 "%Y-%m-%d %H:%M:%S"（+08:00）→
/// RFC3339/ISO 带时区 → 无时区 ISO "%Y-%m-%dT%H:%M:%S"（按 +08:00 墙钟）→
/// 纯日期（当日 23:59:59 +08:00）
pub fn parse_expire_str(s: &str) -> Option<i64> {
    use chrono::TimeZone;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(t) = parse_cn_datetime(s) {
        return Some(t);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
        return Some(offset.from_local_datetime(&dt).single()?.timestamp());
    }
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
    let dt = d.and_hms_opt(23, 59, 59)?;
    Some(offset.from_local_datetime(&dt).single()?.timestamp())
}

/// 包特征键：名字类（PackageCode/PackageName）或容量类（Precise 优先的 remain/size）。
/// 上游换容器名/挪层级时条目仍带这些字段——据此全树识别而非依赖固定路径
const PACKAGE_SIG_KEYS: [&str; 6] = [
    "PackageCode",
    "PackageName",
    "CapacityRemainPrecise",
    "CapacityRemain",
    "CycleCapacitySizePrecise",
    "CycleCapacitySize",
];

fn looks_like_package(o: &serde_json::Map<String, Value>) -> bool {
    PACKAGE_SIG_KEYS.iter().any(|k| o.contains_key(*k))
}

/// 全树递归收集包条目（限深 8）：命中特征键的对象不再下钻（防嵌套重复收集）。
/// 旧实现固定 `core.get("Accounts")` 路径，上游换层级/容器改名即整包清零而
/// TotalDosage（递归 dig）仍显示——「总分在、包和到期全空」的 issue #179 症状
fn collect_package_items<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    fn walk<'a>(v: &'a Value, depth: u32, out: &mut Vec<&'a Value>) {
        if depth > 8 {
            return;
        }
        match v {
            Value::Object(m) => {
                if looks_like_package(m) {
                    out.push(v);
                    return;
                }
                for x in m.values() {
                    walk(x, depth + 1, out);
                }
            }
            Value::Array(a) => {
                for x in a {
                    walk(x, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    walk(v, 0, out);
}

/// 响应骨架（诊断，issue #179）：对象展开键名树、数组取前 2 个元素、长字符串
/// 截断为长度标记；数值保留（容量/总量对定位有用，响应体不含 token）；限深 4
fn skeleton(v: &Value, depth: u32) -> Value {
    match v {
        Value::Object(m) => {
            let mut o = serde_json::Map::new();
            if depth < 4 {
                for (k, x) in m {
                    o.insert(k.clone(), skeleton(x, depth + 1));
                }
            }
            Value::Object(o)
        }
        Value::Array(a) => Value::Array(a.iter().take(2).map(|x| skeleton(x, depth + 1)).collect()),
        Value::Null => json!(null),
        Value::Bool(b) => json!(b),
        Value::Number(n) => json!(n),
        Value::String(s) => {
            let n = s.chars().count();
            if n > 32 {
                json!(format!("str(len={n})"))
            } else {
                json!(s)
            }
        }
    }
}

/// 包条目字段取值：Precise 字符串优先（精度丢失小），回退整数字段
fn pkg_f64(obj: &Value, precise_key: &str, plain_key: &str) -> f64 {
    dig_f64(obj, precise_key)
        .or_else(|| dig_f64(obj, plain_key))
        .unwrap_or(0.0)
}

/// 解析诊断落日志（issue #179）：仅异常形态（明细空/未知 Status）时记录，
/// 正常取数零开销零噪音
fn log_parse_diag(state: &AppState, acct_id: &str, info: &Option<DosageInfo>) {
    if let Some(d) = info.as_ref().and_then(|i| i.parse_diag.as_deref()) {
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("buddy-ai: 积分解析诊断 {acct_id}: {d}"),
        );
    }
}

pub fn fetch_dosage_info(
    agent: &ureq::Agent,
    base: &str,
    token: &str,
    uid: &str,
) -> Option<DosageInfo> {
    let url = format!("{base}/v2/billing/meter/get-user-resource");
    let resp = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("X-User-Id", uid)
        .set("X-Client-Platform", "web")
        .set("Content-Type", "application/json")
        .send_string("{}")
        .ok()?;
    let v: Value = resp.into_json().ok()?;
    // R-6 实测嵌套：{data: {Response: {Data: {TotalDosage, Accounts[...]}}}}。
    // ⚠️ dig 的信封穿透表 ENVELOPE_KEYS 全小写，"Response"/"Data" 大写不命中——
    // 固定层级用 fs_utils::path 严格下钻，回退 dig 平铺形态（兼容旧版）。
    // 无 data 信封 = 响应不可信（登录失效/接口变更），维持 None 口径——调用方
    // 跳过回写，不得把 total=0 写进池
    digv(&v, &["data"])?; // 无 data 信封 = 响应不可信（登录失效/接口变更），维持
    // None 口径——调用方跳过回写，不得把 total=0 写进池。解析本身以整个响应为
    // 范围（issue #179：包收集/total 深挖/骨架都不再受固定 Response/Data 层级
    // 约束），无需再下钻 core
    Some(parse_dosage_body(&v))
}

/// 响应体解析（issue #179 兼容加固，与网络层解耦便于单测）：
/// - 包条目**全响应**递归收集（不再依赖固定 Accounts 路径/容器名，包容器挪层级/改名
///   均能命中；包特征键足够特异，全树收集误收风险极低）；
/// - Status 未知值 fail-open 保留并落诊断（上游新增状态值不再静默清空整包）；
/// - 到期解析补 ISO/纯日期兜底；ExpiredTime 补字符串形态。
/// `root` = 整个响应：包收集、TotalDosage 深挖、诊断骨架统一范围。
pub fn parse_dosage_body(root: &Value) -> DosageInfo {
    let now = chrono::Utc::now().timestamp();
    // 分包明细（Status 0=有效/3=宽限；未知值保留 + 记诊断）
    let mut packages = Vec::new();
    let mut expire_candidates: Vec<i64> = Vec::new();
    let mut unknown_statuses: Vec<Value> = Vec::new();
    let mut items = Vec::new();
    collect_package_items(root, &mut items);
    for a in items {
        let status_raw = a.get("Status").cloned().unwrap_or(Value::Null);
        let status = status_raw
            .as_i64()
            .or_else(|| status_raw.as_str().and_then(|s| s.trim().parse().ok()))
            .unwrap_or(0);
        if status != 0 && status != 3 {
            unknown_statuses.push(status_raw.clone());
        }
        let cycle_end = a
            .get("CycleEndTime")
            .and_then(Value::as_str)
            .and_then(parse_expire_str);
        // 已知状态（0=有效/3=宽限）才参与「最近到期」汇总：未知 Status（上游新增
        // 作废/异常/冻结包）fail-open 保留明细与诊断，但其到期不入池——否则未知包
        // 的 CycleEndTime 会把账号到期误导为未来某刻
        if let Some(e) = cycle_end {
            if (status == 0 || status == 3) && e > now {
                expire_candidates.push(e);
            }
        }
        // 旧字段兼容：数值/数字字符串 ExpiredTime（秒/毫秒归一）
        let expired_num = a
            .get("ExpiredTime")
            .and_then(Value::as_f64)
            .or_else(|| {
                a.get("ExpiredTime")
                    .and_then(Value::as_str)
                    .and_then(|s| s.trim().parse::<f64>().ok())
            });
        if let Some(t) = expired_num {
            let secs = if t > 1e12 { t / 1000.0 } else { t };
            if (status == 0 || status == 3) && secs > now as f64 {
                expire_candidates.push(secs as i64);
            }
        }
        packages.push(DosagePackage {
            name: a
                .get("PackageName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            code: a
                .get("PackageCode")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            remain: pkg_f64(a, "CapacityRemainPrecise", "CapacityRemain"),
            size: pkg_f64(a, "CycleCapacitySizePrecise", "CycleCapacitySize"),
            expire_secs: cycle_end,
        });
    }
    // TotalDosage 从整个响应递归深挖（core 内固定层级外的候选也命中；dig 全树递归）
    let total = dig_f64(root, "TotalDosage").unwrap_or_else(|| packages.iter().map(|p| p.remain).sum());
    // 解析诊断（issue #179）：明细为空（形状可能变化）或未知 Status 时携带证据，
    // 调用方落 app_log——修复前此场景完全静默，无法定位。骨架基于整个响应
    // （包容器挪出 core 时骨架仍能看到新容器名）
    let parse_diag = if packages.is_empty() {
        Some(
            json!({
                "warn": "get-user-resource 包明细解析为空，上游响应形状可能变化（issue #179）",
                "total": total,
                "statuses": unknown_statuses,
                "shape": skeleton(root, 0),
            })
            .to_string(),
        )
    } else if !unknown_statuses.is_empty() {
        Some(
            json!({
                "warn": "get-user-resource 出现未知 Status 值，已计入明细",
                "statuses": unknown_statuses,
            })
            .to_string(),
        )
    } else {
        None
    };
    let expire_secs = expire_candidates.iter().copied().min();
    DosageInfo {
        total,
        expire_secs,
        packages,
        parse_diag,
    }
}

/// dig 单键 f64（数值或数字字符串兜底）
fn dig_f64(v: &Value, key: &str) -> Option<f64> {
    let x = digv(v, &[key])?;
    x.as_f64().or_else(|| x.as_str()?.parse().ok())
}

// ── ACP 六步链 ──────────────────────────────────────────────────────────────

/// 网页腿公共头（§4.5：Bearer 账号 token + X-User-Id + Chrome UA + Origin/Referer，
/// 不带 X-IDE-* 指纹头）
fn web_headers<'a>(
    req: ureq::Request,
    token: &str,
    uid: &str,
) -> ureq::Request {
    const CHROME_UA: &str =
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
    req.set("Authorization", &format!("Bearer {token}"))
        .set("X-User-Id", uid)
        .set("User-Agent", CHROME_UA)
        .set("Origin", "https://www.workbuddy.ai")
        .set("Referer", "https://www.workbuddy.ai/")
        .set("Accept", "application/json, text/plain, */*")
}

/// 退避重试的错误分类：HTTP 状态码（区分 4xx 语义失败 / 5xx 瞬时抖动）与消息
enum BackoffErr {
    Status(u16),
    Msg(String),
}

impl From<ureq::Error> for BackoffErr {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::Status(code, _) => BackoffErr::Status(code),
            other => BackoffErr::Msg(other.to_string()),
        }
    }
}

/// 带秒级退避的请求执行：4xx 语义性失败不重试直返；网络类 Err 与 5xx 重试
/// STEP_RETRIES 次（1s 起翻倍封顶 15s，§4.5：避免一次抖动熔断整池）
fn with_backoff<T>(mut f: impl FnMut() -> Result<T, BackoffErr>) -> Result<T, String> {
    let mut last = String::new();
    for attempt in 0..STEP_RETRIES {
        match f() {
            Ok(v) => return Ok(v),
            Err(BackoffErr::Status(code)) => {
                last = format!("HTTP {code}");
                if code < 500 {
                    return Err(last);
                }
            }
            Err(BackoffErr::Msg(m)) => last = m,
        }
        if attempt + 1 < STEP_RETRIES {
            backoff_sleep(attempt);
        }
    }
    Err(last)
}

/// 单账号打卡结果事件（NDJSON 契约对齐 wb-checkin：index 由整轮填充）
fn result_ev(id: &str, name: &str, status: &str, message: &str) -> Value {
    json!({ "id": id, "name": name, "status": status, "message": message })
}

/// SSE 读线程停机哨兵（M2 审查修复）：宿主流程（process_account）结束时 drop
/// 置位停机旗标，读线程在每行循环头检查旗标退出。覆盖双泄漏场景——
/// ① 阻塞 read 永不返回（服务端不发数据也不关流）→ sse_agent timeout_read(240s)
///    兜底强制返回（240s > COMPLETE_TIMEOUT_SECS，不误断活跃 turn 静默期）；
/// ② keepalive 注释行（`:ping`）走 continue 分支永不触碰 send 检测 → 循环头
///    旗标检查兜底。
struct SseShutdownGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for SseShutdownGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// 单账号打卡全链（§4.5 补篇 ①-⑥）。返回事件；幂等戳成功才落盘（不成功不记打卡）。
#[allow(clippy::too_many_lines)]
fn process_account(
    state: &AppState,
    agent: &ureq::Agent,
    acct_id: &str,
    nickname: &str,
    token: &str,
    uid: &str,
    base: &str,
    shared: Option<&std::sync::Arc<crate::api_server::ApiSharedState>>,
    emit: &mut dyn FnMut(&Value),
) -> Value {
    // T7 审查修复：fail 文案统一改写「HTTP 401/403」→「凭证401/403」。调度器
    // failed_permanent 按 fail 消息含「HTTP 401」子串判 permanent 当日剔除；
    // buddy-ai 凭证失效可经网关 401 自愈/续期轮换新值（六步链已用锁内最新凭证），
    // 30 分钟冷却重试可能成功，不应判 permanent——改写子串即归一（BL-5 意图
    // 随 M3/P1 修正）。原文细节不受影响：轮询异常已先行 app_log 明文落盘
    let fail = |m: String| {
        result_ev(
            acct_id,
            nickname,
            "fail",
            &m.replace("HTTP 401", "凭证401")
                .replace("HTTP 403", "凭证403"),
        )
    };

    // 幂等戳：token rec `last_daily_chat`（本地日期；成功才落盘）。
    // 读段持每账号刷新锁：打卡与网关 401 自愈/手动续期共享一把锁（R-2a rt 轮换
    // 一次一换，读-用-写跨 120s+ 六步链窗口，无锁会把并发轮换的新凭证读旧）
    let refresh_lock = crate::tasks::wb_common::refresh_lock_for(acct_id);
    let _read_guard = refresh_lock.lock().unwrap_or_else(|e| e.into_inner());
    let store = crate::tasks::wb_common::buddy_ai_token_store_load(&state.data_dir);
    let rec = store
        .get("tokens")
        .and_then(|t| t.get(acct_id))
        .cloned()
        .unwrap_or_default();
    let last = rec.get("last_daily_chat").and_then(Value::as_str).unwrap_or("");
    if last == today_stamp() {
        return result_ev(acct_id, nickname, "already", "今日已完成活跃打卡");
    }
    // 审查修复（P1）：六步链用锁内读到的最新凭证——轮次开始快照的 token 可能在
    // 锁内读段前后已被续期/401 自愈轮换，继续用旧值整链 401 且被调度器判
    // permanent 当日剔除（漏打卡）；锁内读到了新值就改用新值
    let token = rec
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .unwrap_or(token);
    // 锁释放（六步链执行期不持锁——避免阻塞并发的凭证续期；写段见下方二次加锁）
    drop(_read_guard);

    // ⑥ 前置：打卡前积分快照（失败不阻塞打卡——到账校验是加分项不是门槛）
    let before = fetch_dosage_info(agent, base, token, uid);
    log_parse_diag(state, acct_id, &before);

    // ① 建会话
    let body = json!({
        "prompt": PROMPT_TEXT,
        "model": FREE_MODEL,
        "conversationOrigin": "workbuddy-app",
        "plugins": [{ "name": "weixinpay", "marketplace": "codebuddy-builtin" }],
    });
    // ① 建会话（审查 #17：POST 非幂等，不走 with_backoff——请求已达但响应解析
    // 失败（Msg 类：into_json 失败/data.id 缺失）时重发会在服务端再建一个会话 →
    // 孤儿会话累积；失败直接 fail 交整轮冷却由下一轮重打。对齐 session/prompt 不重试口径）
    let conv_id = (|| {
        let resp = web_headers(agent.post(&format!("{base}/console/as/conversations/")), token, uid)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())?;
        let v: Value = resp.into_json().map_err(|e| BackoffErr::Msg(e.to_string()))?;
        // 2026-10-16 实抓修复：dig 是「任意键命中即返回」语义（find_map 逐键全树搜），
        // ["data","id"] 会命中 data 对象本身导致 as_str() 失败——改用 path 严格逐段下钻；
        // id 以字符串为主，兼容数字形态
        let id = crate::fs_utils::path(&v, &["data", "id"])
            .ok_or_else(|| BackoffErr::Msg(format!("建会话响应无 data.id：{}", trim_json(&v))))?;
        Ok(match id {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            other => {
                return Err(BackoffErr::Msg(format!(
                    "建会话响应 data.id 类型异常（{other}）：{}",
                    trim_json(&v)
                )))
            }
        })
    })()
    .map_err(|e| match e {
        BackoffErr::Status(c) => format!("HTTP {c}"),
        BackoffErr::Msg(m) => m,
    });
    let conv_id = match conv_id {
        Ok(v) => v,
        Err(e) => return fail(format!("建会话失败：{e}")),
    };

    // ② 取沙箱（link + 独立 token + sessionId）
    let sess = with_backoff(|| {
        let resp = web_headers(
            agent.get(&format!("{base}/console/as/conversations/{conv_id}/session")),
            token,
            uid,
        )
        .call()?;
        let v: Value = resp.into_json().map_err(|e| BackoffErr::Msg(e.to_string()))?;
        let d = v.get("data").cloned().unwrap_or(v.clone());
        let link = d.get("link").and_then(Value::as_str).map(|s| s.to_string());
        let stoken = d
            .get("token")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let sid = d
            .get("sessionId")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        match (link, stoken, sid) {
            (Some(l), Some(t), Some(s)) => Ok((l, t, s)),
            _ => Err(BackoffErr::Msg(format!("取沙箱响应缺字段：{}", trim_json(&v)))),
        }
    });
    let (link, sandbox_token, session_id) = match sess {
        Ok(v) => v,
        Err(e) => return fail(format!("取沙箱失败：{e}")),
    };

    // ③ ACP 通道：GET link（SSE）→ 响应头 Acp-Connection-Id。
    // SSE GET 用无整体超时的专用 agent（2026-10-16 二号账号 stuck working 整改）：
    // 流由读线程持续消费（读线程 detached，阻塞的是它自己不是打卡线程），
    // acp_agent 的 30s 整体超时会在读线程 30s 后掐断 SSE → turn 被服务端中止。
    // M2 审查修复：补 timeout_read——原实现仅 timeout_connect，读线程 read_line
    // 在服务端既不发数据也不关流时永久阻塞（线程+socket 泄漏）。240s 大于
    // COMPLETE_TIMEOUT_SECS(180)：活跃 turn 最长静默期不会误断
    let sse_agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout_read(std::time::Duration::from_secs(240))
        .build();
    let acp_agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build();
    // 2026-10-16 实测修复（404 根因）：SSE Response 必须带出闭包保持存活——原实现
    // 在 with_backoff 闭包返回时 drop，服务端随即回收 ACP 连接，④ 全部
    // 404 {"error":"connection not found"}（响应体已由 4xx 诊断落盘佐证）
    let sse = with_backoff(|| {
        let resp = sse_agent.get(&link)
            .set("Accept", "text/event-stream")
            .set("Authorization", &format!("Bearer {sandbox_token}"))
            .set("User-Agent", "Mozilla/5.0")
            .call()?;
        let conn = resp
            .header("Acp-Connection-Id")
            .map(|s| s.to_string())
            .ok_or_else(|| BackoffErr::Msg("ACP 响应缺 Acp-Connection-Id".to_string()))?;
        Ok((resp, conn))
    });
    let (sse_resp, conn_id) = match sse {
        Ok(v) => v,
        Err(e) => return fail(format!("ACP 通道建立失败：{e}")),
    };

    // SSE 读线程（2026-10-16 二号账号诊断）：此前 SSE 只挂不读——turn 的
    // session/update 流量、服务端→客户端请求（turn 等客户端应答会永远挂住）、
    // 四连 RPC 的错误回执全部不可见。读线程解析 SSE data 行：转发事件摘要给
    // ⑤ 轮询循环；带 id 的服务端请求按 ACP 语义代答（request_permission 放行
    // 首个 allow 选项、其余回 -32601）防 turn 挂死；RPC 错误回执落 app.log。
    // 连接存活由读线程的阻塞 read 天然维持（EOF/服务端关闭才退出）。
    let (acp_ev_tx, acp_ev_rx) = std::sync::mpsc::channel::<String>();
    // M2 审查修复：停机哨兵存活到 process_account 返回（含各 fail 早退路径），
    // drop 置位后读线程在下一次 read 返回时退出
    let sse_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _sse_stop_guard = SseShutdownGuard(sse_stop.clone());
    {
        let rd_link = link.clone();
        let rd_conn = conn_id.clone();
        let rd_token = sandbox_token.clone();
        let rd_agent = acp_agent.clone();
        let rd_data_dir = state.data_dir.clone();
        let rd_stop = sse_stop.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(sse_resp.into_reader());
            let mut line = String::new();
            loop {
                // M2：循环头检查停机旗标——keepalive 注释行走 continue 永不触碰
                // send 检测，宿主流程结束后线程原可无限存活
                if rd_stop.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Some(data) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(method) = v.get("method").and_then(Value::as_str) {
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    match v.get("id") {
                        // 服务端→客户端请求：必须应答，否则 turn 挂死等客户端
                        Some(id) => {
                            let summary = acp_answer_request(
                                &rd_agent, &rd_link, &rd_conn, &rd_token, id, method, &params,
                            );
                            crate::fs_utils::app_log(
                                &rd_data_dir,
                                &format!("buddy-ai: ACP 服务端请求 {summary}（conv 诊断）"),
                            );
                            // 发送失败 = 本账号打卡流程已结束（接收端 drop）→ 读线程退出，
                            // 防「stuck working/失败」场景下线程+socket 无限累积
                            if acp_ev_tx.send(summary).is_err() {
                                break;
                            }
                        }
                        // 通知：session/update 流量（轮询循环汇总，不逐条落盘）
                        None => {
                            let kind = params
                                .get("update")
                                .and_then(|u| u.get("sessionUpdate"))
                                .and_then(Value::as_str)
                                .unwrap_or("?");
                            if acp_ev_tx.send(format!("session/update:{kind}")).is_err() {
                                break;
                            }
                        }
                    }
                } else if let Some(id) = v.get("id") {
                    // 我方四连 RPC 的回执（结果错误才落盘，成功只走摘要流）
                    if let Some(err) = v.get("error") {
                        let m = err.get("message").and_then(Value::as_str).unwrap_or("?");
                        crate::fs_utils::app_log(
                            &rd_data_dir,
                            &format!("buddy-ai: ACP RPC 回执错误 id={id}: {m}"),
                        );
                        if acp_ev_tx.send(format!("RPC 回执 id={id} 错误: {m}")).is_err() {
                            break;
                        }
                    } else if acp_ev_tx.send(format!("RPC 回执 id={id} ok")).is_err() {
                        break;
                    }
                }
            }
        });
    }

    // ④ ACP JSON-RPC 四连（HTTP 202/200 即受理；set_model 必发——省钱红线）。
    // payload 形态按 ACP 规范 + workbuddy2api-hub run_turn 口径构造；R-4 实测全 202。
    let rpc = |id: u32, method: &str, params: Value| {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    };
    let steps: [(u32, &str, Value); 4] = [
        (
            1,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } },
            }),
        ),
        (
            2,
            "session/load",
            json!({ "sessionId": session_id, "cwd": "/", "mcpServers": [] }),
        ),
        (
            3,
            "session/set_model",
            json!({ "sessionId": session_id, "modelId": FREE_MODEL }),
        ),
        (
            4,
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": PROMPT_TEXT }] }),
        ),
    ];
    for (rid, method, params) in steps {
        let payload = rpc(rid, method, params);
        let attempt = || {
            let out = acp_agent.post(&link)
                .set("Acp-Connection-Id", &conn_id)
                .set("Authorization", &format!("Bearer {sandbox_token}"))
                .set("Content-Type", "application/json")
                // 2026-10-16 实测修复：POST 缺 Accept/Origin 时沙箱网关回 406——
                // 与 ③ GET 同口径补 SSE Accept + 站点 Origin/Referer
                .set("Accept", "text/event-stream, application/json")
                .set("Origin", "https://www.workbuddy.ai")
                .set("Referer", "https://www.workbuddy.ai/")
                .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                .send_string(&payload.to_string());
            match out {
                // 202 = 受理（SSE 异步回 session/update）；200 兼容同步响应形态
                Ok(resp) => Ok(u32::from(resp.status())),
                // 4xx 诊断：响应体落 app.log（BackoffErr::Status 只带 code 丢失 body，
                // Accept/Origin 修复是否命中需原文佐证）
                Err(ureq::Error::Status(code, resp)) => {
                    // T6 审查修复：4xx 响应体可能携带沙箱凭证（token/link 等），
                    // JSON 形态先经 trim_json 脱敏截断再落盘；非 JSON 文本回退原
                    // 口径（chars().take(600) 按字符截断，无 panic 风险）
                    let head = match resp.into_string() {
                        Ok(body) => match serde_json::from_str::<Value>(&body) {
                            Ok(v) => trim_json(&v),
                            // 审查 #18：非 JSON 文本（HTML 错误页/网关诊断）无法
                            // 结构化脱敏——含敏感键名即整段隐去（保留字节数供诊断），
                            // 凭证值不落 app.log；无敏感键仍原文截断保留诊断价值
                            Err(_) => {
                                let lower = body.to_lowercase();
                                if SENSITIVE_KEYS
                                    .iter()
                                    .any(|k| lower.contains(&k.to_lowercase()))
                                {
                                    format!(
                                        "[非 JSON 响应体 {} 字节，含敏感字段已整段隐去]",
                                        body.len()
                                    )
                                } else {
                                    body.chars().take(600).collect()
                                }
                            }
                        },
                        Err(_) => String::new(),
                    };
                    if !head.is_empty() {
                        crate::fs_utils::app_log(
                            &state.data_dir,
                            &format!("buddy-ai: ACP {method} HTTP {code} 响应体：{head}"),
                        );
                    }
                    Err(BackoffErr::Status(code))
                }
                Err(e) => Err(BackoffErr::from(e)),
            }
        };
        // session/prompt 不重试（审查修复）：202 受理后 turn 已在服务端排队，
        // 超时重发会创建重复 turn（双计费/双 turn）——失败直接 fail 交整轮冷却重试
        let res = if method == "session/prompt" {
            attempt().map_err(|e| match e {
                BackoffErr::Status(c) => format!("HTTP {c}"),
                BackoffErr::Msg(m) => m,
            })
        } else {
            with_backoff(attempt)
        };
        if let Err(e) = res {
            return fail(format!("ACP {method} 失败：{e}"));
        }
    }
    // ⑤ 完成判定轮询（data.status == "completed"，180s 超时）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(COMPLETE_TIMEOUT_SECS);
    let mut completed = false;
    let mut poll_rounds = 0u64;
    // 诊断追踪（2026-10-16 21:05 超时事故教训：此前轮询期错误被 _ => continue
    // 静默吞掉，失败只有「未在 180s 内完成」一句，401/结构漂移/状态值变更全不可见）
    let mut last_status = String::from("(无)");
    let mut first_poll_err: Option<String> = None;
    // ACP 流量追踪（2026-10-16 二号账号 stuck working：只挂不读时代码全盲）——
    // 零流量=turn 未启动/等代答；停在 RPC 回执=agent 挂死；持续 chunk=真慢
    let mut acp_updates = 0u64;
    let mut last_acp_event: Option<(String, std::time::Instant)> = None;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_secs(COMPLETE_POLL_SECS));
        poll_rounds += 1;
        // 吸收 SSE 读线程的事件摘要（非阻塞 drain）
        while let Ok(s) = acp_ev_rx.try_recv() {
            acp_updates += 1;
            last_acp_event = Some((s, std::time::Instant::now()));
        }
        // 心跳（长静默期修复）：每 5 轮（15s）向前端报一次等待进度——
        // 此前整个轮询期零事件，用户感知为「点了没反应一直执行中」
        if poll_rounds % 5 == 0 {
            emit(&json!({
                "type": "progress",
                "name": nickname,
                "message": format!(
                    "等待会话完成…（{}s/{}s）",
                    poll_rounds * COMPLETE_POLL_SECS,
                    COMPLETE_TIMEOUT_SECS
                ),
            }));
        }
        let status = with_backoff(|| {
            let resp = web_headers(
                agent.get(&format!("{base}/console/as/conversations/{conv_id}")),
                token,
                uid,
            )
            .call()?;
            let v: Value = resp.into_json().map_err(|e| BackoffErr::Msg(e.to_string()))?;
            Ok(v.get("data")
                .and_then(|d| d.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string())
        });
        match status {
            Ok(s) if s == "completed" => {
                completed = true;
                break;
            }
            // 终态集合（completed 之外的失败终态直接退出，不空转烧满 180s）
            Ok(s) if s == "failed" || s == "error" || s == "cancelled" => {
                last_status = s;
                break;
            }
            Ok(other) => {
                last_status = other;
                continue;
            }
            Err(e) => {
                // 首次轮询异常落盘（token 失效/网络中断一目了然），后续不刷屏
                if first_poll_err.is_none() {
                    crate::fs_utils::app_log(
                        &state.data_dir,
                        &format!("buddy-ai: 打卡轮询异常（conv={conv_id}）：{e}"),
                    );
                    first_poll_err = Some(e.clone());
                }
                // 凭证失效早退（审查修复）：401/403 空转满 180s 无意义，直接退出
                // 交 30 分钟冷却重试（凭证续期/401 自愈后新值可成功）。last_status
                // 携带的「HTTP 401」原文会经 fail 闭包改写为「凭证401」（T7），
                // 不触发调度器 failed_permanent；原始错误已 app_log 佐证
                // 审查修复（确定性4xx早退）：401/403 之外的确定性 4xx 同样空转满
                // 180s 无意义——按同款「HTTP {code}」口径提取状态码（with_backoff
                // 的 Status 错误文案），400-499（除 408/429）确定性失败直接退出；
                // 408/429/5xx/网络抖动保留重试。早退走普通 fail（调度器 buddy-ai
                // 打卡分支已无 failed_permanent 判定，全部失败交冷却重试）
                let poll_code = e
                    .rsplit("HTTP ")
                    .next()
                    .and_then(|rest| {
                        rest.chars()
                            .take_while(|c| c.is_ascii_digit())
                            .collect::<String>()
                            .parse::<u16>()
                            .ok()
                    });
                if let Some(code) = poll_code {
                    if (400..500).contains(&code) && code != 408 && code != 429 {
                        last_status = if e.contains("HTTP 401") || e.contains("HTTP 403") {
                            format!("轮询凭证失效（{e}）")
                        } else {
                            format!("轮询确定性失败（{e}）")
                        };
                        break;
                    }
                }
                continue;
            }
        }
    }
    if !completed {
        let acp_tail = match (&last_acp_event, acp_updates) {
            (Some((s, t)), n) => format!(
                "，ACP 流量 {n} 条，最后事件：{s}（{}s 前）",
                t.elapsed().as_secs()
            ),
            (None, _) => "，ACP 流量 0 条（沙箱零响应：turn 未启动或等客户端应答）".to_string(),
        };
        return fail(format!(
            "会话未在 {COMPLETE_TIMEOUT_SECS}s 内完成（conv={conv_id}，最后状态={last_status}，轮询异常={}){acp_tail}",
            first_poll_err.as_deref().unwrap_or("无")
        ));
    }

    // ⑥ 到账校验 + 幂等戳落盘 + 池快照回写（成功才落盘，§4.5：不成功不记打卡）
    let after = fetch_dosage_info(agent, base, token, uid);
    log_parse_diag(state, acct_id, &after);
    let gain = match (&before, &after) {
        (Some(b), Some(a)) if a.total > b.total => Some(a.total - b.total),
        _ => None,
    };
    // 幂等戳落盘（BH-1 审查修复）：持刷新锁 + 锁内重读最新 rec 合并写回——
    // 六步链窗口内并发续期（网关 401 自愈/手动续期）已轮换 rt 并合并落库，
    // 若用六步链前的旧 rec 整行 REPLACE 会把新凭证抹回旧值（rt 已作废 → 账号须重登）
    let write_lock = crate::tasks::wb_common::refresh_lock_for(acct_id);
    let _write_guard = write_lock.lock().unwrap_or_else(|e| e.into_inner());
    // 删号竞态保护（审查修复）：六步链执行期（最长 3 分钟）账号可能已被用户移除
    //（remove 已三清 token store/vault/锁表）——upsert 语义会把幽灵凭证行复活，
    // 先确认账号仍在池中才落戳。审查修复（停用误判）：删号判定不能用 roster
    //（滤了 disabled，停用账号不在花名册但仍在池中，会被误判「已移除」致幂等
    // 戳不落盘 + 归因失实）——改用含停用账号的池原始判定：停用（在池）放行落戳
    // + 正常回写；真移除（池中已无此 id）才保留删号保护
    if !crate::commands::workbuddy::buddy_ai_pool_contains(state, acct_id) {
        return fail("账号已在本轮打卡期间被移除，幂等戳不落盘".to_string());
    }
    let latest = crate::tasks::wb_common::buddy_ai_token_store_load(&state.data_dir);
    let mut new_rec = latest
        .get("tokens")
        .and_then(|t| t.get(acct_id))
        .cloned()
        .unwrap_or_else(|| rec.clone());
    // T5 审查修复：serde_json IndexMut 对 Null 自动转 Object 安全，但对
    // Array/Number/String/Bool panic——token store 遭手动改动损坏为非对象时
    // panic 会沿 process_account 杀掉整轮打卡线程；损坏记录本就不可信，
    // 重置空对象仅保幂等戳语义（upsert 以此行整体覆盖）
    if !new_rec.is_object() {
        new_rec = json!({});
    }
    new_rec["last_daily_chat"] = json!(today_stamp());
    if let Err(e) = crate::tasks::wb_common::buddy_ai_token_store_upsert(&state.data_dir, acct_id, &new_rec) {
        return fail(format!("打卡幂等戳落盘失败（记为未完成防假打卡）：{e}"));
    }
    // 池快照回写（after 口径；before/after 不可用时跳过不阻塞成功语义）
    if let Some(info) = &after {
        let packages = serde_json::to_value(&info.packages).unwrap_or(Value::Null);
        if let Err(err) = crate::commands::workbuddy::buddy_pool_write_credits(
            state,
            acct_id,
            info.total,
            info.expire_secs,
            Some(packages),
        ) {
            crate::fs_utils::app_log(&state.data_dir, &format!("buddy-ai: 打卡积分回写失败 {acct_id}: {err}"));
        }
    }
    // 审查 #4：打卡到账同步拨进运行期网关 buddy_ai_pool（qoder/wb 先例）——
    // 此前只写持久池，网关运行池钉死在装配时刻快照，零积分过滤窗口内到账的
    // 新余额进不了运行池，账号被持续排除到下次积分同步（默认 every_hours 2h）。
    // 键位口径：buddy-ai 网关装配以账号 id 充当条目 uid（审查 H2 复核），按
    // acct_id 拨新即命中；expire None 不抹池内旧值（对齐 buddy_pool_write_credits
    // 的「仅 Some 才覆盖」口径，防 expire_first「先到期优先」误压）
    if let (Some(shared), Some(info)) = (shared, &after) {
        let expire = match info.expire_secs {
            Some(e) => Some(e),
            None => shared
                .buddy_ai_pool
                .status_list()
                .into_iter()
                .find(|s| s.uid == acct_id)
                .and_then(|s| s.credits_expire_at),
        };
        let m: std::collections::HashMap<String, (f64, Option<i64>)> =
            std::collections::HashMap::from([(acct_id.to_string(), (info.total, expire))]);
        shared.buddy_ai_pool.update_credits_snapshot(&m);
    }
    let msg = match gain {
        Some(g) => format!("活跃打卡完成，积分 +{g:.0}（{:.0} → {:.0}）",
            before.as_ref().map(|b| b.total).unwrap_or(0.0),
            after.as_ref().map(|a| a.total).unwrap_or(0.0)),
        None => "活跃打卡完成（会话 completed，奖励自动入账）".to_string(),
    };
    let mut ev = result_ev(acct_id, nickname, "success", &msg);
    // P2：积分增量随事件落盘（打卡结果表 reward 列，趋势/日历展示用）
    if let Some(g) = gain {
        ev["reward"] = json!(g);
    }
    ev
}

/// ACP 服务端→客户端请求代答（SSE 读线程调用）：request_permission 放行首个
/// allow 选项（打卡是沙箱内最小「Hi」轮，代答与网页端默认放行同效）；其余方法
/// 回 -32601，避免 turn 因等客户端应答而挂死。返回事件摘要供诊断流。
fn acp_answer_request(
    agent: &ureq::Agent,
    link: &str,
    conn_id: &str,
    token: &str,
    id: &Value,
    method: &str,
    params: &Value,
) -> String {
    let payload = if method == "session/request_permission" {
        // 只放行 allow 选项：无 allow 可选时回 cancelled（兜底选第一个 option 可能
        // 选中 reject/deny 项，导致 turn 被拒绝提前终止）
        let allow = params.get("options").and_then(Value::as_array).and_then(|os| {
            os.iter().find(|o| {
                o.get("kind")
                    .and_then(Value::as_str)
                    .map_or(false, |k| k.starts_with("allow"))
            })
        });
        match allow {
            Some(o) => {
                let oid = o
                    .get("optionId")
                    .or_else(|| o.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "outcome": { "outcome": "selected", "optionId": oid } }
                })
            }
            None => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "outcome": { "outcome": "cancelled" } }
            }),
        }
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "method not supported by checkin client" }
        })
    };
    let status = agent
        .post(link)
        .set("Acp-Connection-Id", conn_id)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream, application/json")
        .set("Origin", "https://www.workbuddy.ai")
        .set("Referer", "https://www.workbuddy.ai/")
        .send_string(&payload.to_string())
        .map(|r| u32::from(r.status()))
        .unwrap_or(0);
    format!("{method} ← 已代答（HTTP {status}）")
}

/// 诊断摘要敏感键（审查 #18：补 sessionId/session_id——ACP ② 步 sessionId 与
/// 沙箱 token/link 同属会话凭证，广播/app_log 落盘前一并打码；模块级供
/// trim_json 与非 JSON 文本整段隐去判定共用）
const SENSITIVE_KEYS: [&str; 9] = [
    "token",
    "access_token",
    "accessToken",
    "refresh_token",
    "refreshToken",
    "authorization",
    "link",
    "sessionId",
    "session_id",
];

/// 失败诊断用的响应摘要（审查 H5/H6 修复）：
/// ① 先脱敏——响应可能携带沙箱凭证（② 步的 token/link 等），原样序列化会经
///    NDJSON 进度事件广播 + app_log 明文落盘（长期存储泄漏面）；
/// ② 再截断——serde_json 输出 UTF-8 原文，按字节切片遇非 ASCII（中文错误文案）
///    不是字符边界会 panic 且 panic 沿 process_account 展开杀掉整轮打卡线程
fn trim_json(v: &Value) -> String {
    let mut redacted = v.clone();
    redact_sensitive(redacted.as_object_mut(), &SENSITIVE_KEYS);
    let s = redacted.to_string();
    if s.len() <= 200 {
        return s;
    }
    let mut end = 200;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// 递归脱敏：命中敏感键（大小写不敏感）的值置为 "***"
fn redact_sensitive(obj: Option<&mut serde_json::Map<String, Value>>, keys: &[&str]) {
    let Some(obj) = obj else { return };
    for (k, val) in obj.iter_mut() {
        if keys.iter().any(|s| k.eq_ignore_ascii_case(s)) {
            *val = Value::String("***".into());
        } else {
            match val {
                Value::Object(_) => redact_sensitive(val.as_object_mut(), keys),
                Value::Array(a) => {
                    for x in a.iter_mut() {
                        redact_sensitive(x.as_object_mut(), keys);
                    }
                }
                _ => {}
            }
        }
    }
}

/// 打卡整轮：buddy-ai 池全账号串行（NDJSON 管线复用 wb-checkin 契约）。
/// 返回 done 事件（ok/already/failed 计数），供调度器/启动钩子直接消费。
/// `runtime`：GUI 进程内网关运行态句柄（调度器/启动补跑/UI 手动打卡传入，对齐
/// qoder_checkin::run_checkin_round 提取模式）——打卡到账运行期拨进 buddy_ai_pool
///（审查 #4）；CLI 独立进程传 None 跳过
pub fn run_checkin_round(
    state: &AppState,
    runtime: Option<&std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    emit: &mut dyn FnMut(&Value),
) -> Value {
    let agent = crate::tasks::http_agent(30);
    // 花名册经 buddy_ai.rs 公开 helper（WbPool 字段不对 tasks 暴露）；
    // 仅含凭证账号参与（无凭证跳过——手工导入无 rt 也打不了卡）
    let store = crate::tasks::wb_common::buddy_ai_token_store_load(&state.data_dir);
    let tokens = store.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
    let roster = crate::commands::workbuddy::buddy_pool_roster(state);
    let accounts: Vec<(String, String, String)> = roster
        .into_iter()
        .filter(|(id, _, _)| {
            tokens
                .get(id)
                .and_then(|r| r.get("access_token"))
                .and_then(Value::as_str)
                .map(|t| !t.is_empty())
                .unwrap_or(false)
        })
        .collect();
    // 审查 #15：空池空轮打 skipped_empty_pool 标记——调度器 tick 据此不 mark_run
    //（mark_run 固化「当日已跑」，当日新增账号会失去调度补打）；事件流契约不变
    //（start/done 齐备，UI/CLI 零改动），结果落库亦跳过（无事件可记）
    if accounts.is_empty() {
        emit(&json!({ "type": "start", "total": 0 }));
        let done = json!({
            "type": "done", "ok": 0, "already": 0, "failed": 0,
            "skipped": "buddy-ai 池无可用账号，本轮空跑跳过（不固化当日已跑）",
            "skipped_empty_pool": true,
        });
        emit(&done);
        return done;
    }
    emit(&json!({ "type": "start", "total": accounts.len() }));

    let gap_secs = crate::models::effective_checkin_gap(state.settings().buddy_ai_checkin_gap_secs);
    // 网关运行态共享句柄（审查 #4 运行期回写；对齐 qoder_checkin::run_checkin_round
    // 与 buddy_ai_credits_sync_impl 提取模式）：网关未启动时为 None
    let shared = runtime.and_then(|r| {
        let g = r.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().map(|rt| rt.shared.clone())
    });
    let mut events: Vec<Value> = Vec::new();
    for (i, (id, name, pool_uid)) in accounts.iter().enumerate() {
        if i > 0 && gap_secs > 0 {
            std::thread::sleep(std::time::Duration::from_secs(gap_secs));
        }
        // 单账号失败不中断整轮；凭证在锁外读（secure 回填），网络链全程无池锁
        let rec = tokens.get(id).cloned().unwrap_or_default();
        let token = rec.get("access_token").and_then(Value::as_str).unwrap_or("").to_string();
        // X-User-Id：池 uid 优先（OAuth/导入时落库），token rec uid 兜底
        let uid = if pool_uid.is_empty() {
            rec.get("uid").and_then(Value::as_str).unwrap_or("").to_string()
        } else {
            pool_uid.clone()
        };
        // base 按 token rec domain 解析（审查修复：原硬编码 www.workbuddy.ai，
        // rec.domain 非默认域的账号会打到错误主机——与积分/用量路径口径一致）。
        // buddy-ai 池只收国际账号：domain 缺省/非法一律回落 Global 官方域
        let base = match rec.get("domain").and_then(Value::as_str) {
            Some(d) if d.contains(".workbuddy.ai") => crate::tasks::wb_common::region_activity_base(d),
            _ => crate::tasks::wb_common::BILLING_BASE_GLOBAL,
        };
        // 开跑即报（长静默修复）：此前逐账号事件仅在处理完成后发出，
        // 多分钟的六步链期间前端零反馈，用户感知为「点了没反应」
        emit(&json!({
            "type": "progress",
            "name": name,
            "message": format!("开始处理（{}/{}），六步链预计 1-3 分钟…", i + 1, accounts.len()),
        }));
        let mut ev = process_account(state, &agent, id, name, &token, &uid, base, shared.as_ref(), &mut *emit);
        ev["index"] = json!(i + 1);
        emit(&ev);
        events.push(ev);
    }

    let ok = events.iter().filter(|e| e["status"] == "success").count();
    let already = events.iter().filter(|e| e["status"] == "already").count();
    let failed = events.len() - ok - already;

    // P2：打卡结果按日落库（镜像 wb_checkin_results，90 天裁剪；同 uid 同日以
    // 最后一次为准——重试轮自然合并为最终态）。落库失败仅日志，不影响打卡结果
    {
        let now = chrono::Local::now();
        let date = now.format("%Y-%m-%d").to_string();
        let time = now.format("%H:%M:%S").to_string();
        let db = crate::store::db(&state.data_dir);
        let mut root = crate::store::docs::buddy_ai_checkin_results_load(&db);
        let mut merged = root
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // 本轮涉及账号的当日旧行移除（最终态覆盖）。
        // BM-4 审查修复：already 是 no-op 非最终态——当日已有 success（含 reward）
        // 行时，重跑产生的 already 事件不得覆盖（否则调度成功后任何一次手动重跑
        // 都会把当日奖励行抹成空 already，趋势/日历丢数据）。仅 fail/success 覆盖
        let uid_of = |r: &Value| r.get("user_id").and_then(Value::as_str).unwrap_or("").to_string();
        let ev_uid = |e: &Value| e.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let success_today: std::collections::HashSet<String> = merged
            .iter()
            .filter(|r| {
                r.get("date").and_then(Value::as_str) == Some(date.as_str())
                    && r.get("status").and_then(Value::as_str) == Some("success")
            })
            .map(uid_of)
            .collect();
        let replace_uids: std::collections::HashSet<String> = events
            .iter()
            .filter(|e| {
                let st = e.get("status").and_then(Value::as_str);
                // already 非最终态不覆盖；当日已有 success 行时迟到 fail 也不覆盖
                //（正常流到不了——成功已落戳；多端/手工清戳场景防奖励行被抹）
                st != Some("already")
                    && !(st == Some("fail") && success_today.contains(&ev_uid(e)))
            })
            .map(ev_uid)
            .collect();
        let already_only_uids: std::collections::HashSet<String> = events
            .iter()
            .filter(|e| {
                e.get("status").and_then(Value::as_str) == Some("already")
                    && !success_today.contains(&ev_uid(e))
            })
            .map(ev_uid)
            .collect();
        merged.retain(|r| {
            let today_row = r.get("date").and_then(Value::as_str) == Some(date.as_str());
            let uid = uid_of(r);
            !(today_row && (replace_uids.contains(&uid) || already_only_uids.contains(&uid)))
        });
        for e in &events {
            // T2 审查修复：与上方 replace_uids 过滤口径对齐——当日已有 success 行时，
            // 迟到 fail 事件同样跳过不落（否则旧行保留 + fail 行新增 = 同 uid 同日
            // 双行，前端趋势/日历按行取数语义漂移）
            let st = e.get("status").and_then(Value::as_str);
            if (st == Some("already") || st == Some("fail"))
                && success_today.contains(&ev_uid(e))
            {
                continue;
            }
            merged.push(json!({
                "date": date,
                "time": time,
                "user_id": e.get("id").cloned().unwrap_or_default(),
                "name": e.get("name").cloned().unwrap_or_default(),
                "status": e.get("status").cloned().unwrap_or_default(),
                "message": e.get("message").cloned().unwrap_or_default(),
                "reward": e.get("reward").cloned().unwrap_or(Value::Null),
            }));
        }
        // 90 天裁剪
        let cutoff = (now.date_naive() - chrono::Duration::days(90))
            .format("%Y-%m-%d")
            .to_string();
        merged.retain(|r| {
            r.get("date")
                .and_then(Value::as_str)
                .map_or(false, |d| d >= cutoff.as_str())
        });
        // T5 审查修复：docs 文件损坏为非对象时 IndexMut panic——重建空对象，
        // 当轮 results 照常落库（旧行本就不可解析）
        if !root.is_object() {
            root = json!({});
        }
        root["results"] = Value::Array(merged);
        if let Err(err) = crate::store::docs::buddy_ai_checkin_results_save(&db, &root) {
            crate::fs_utils::app_log(&state.data_dir, &format!("buddy-ai: 打卡结果落库失败: {err}"));
        }
    }

    let done = json!({ "type": "done", "ok": ok, "already": already, "failed": failed });
    emit(&done);
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 退避序列：1s→2s→4s…封顶 15s（纯计算不 sleep 的口径单测）
    #[test]
    fn backoff_caps_at_15s() {
        let secs = |a: u32| 1u64.saturating_mul(1 << a.min(4)).min(15);
        assert_eq!((secs(0), secs(1), secs(2), secs(3), secs(4), secs(5)), (1, 2, 4, 8, 15, 15));
    }

    /// 幂等戳日期口径：YYYY-MM-DD
    #[test]
    fn today_stamp_format() {
        let s = today_stamp();
        assert_eq!(s.len(), 10);
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[7], b'-');
    }

    /// 全树收集同名标量（限深 8；BL-6：仅测试消费，从生产代码移入）
    fn dig_all(v: &Value, key: &str) -> Vec<Value> {
        let mut out = Vec::new();
        fn walk(v: &Value, key: &str, depth: u32, out: &mut Vec<Value>) {
            if depth > 8 {
                return;
            }
            match v {
                Value::Object(m) => {
                    if let Some(x) = m.get(key) {
                        if !x.is_null() && !x.is_object() && !x.is_array() {
                            out.push(x.clone());
                        }
                    }
                    for x in m.values() {
                        walk(x, key, depth + 1, out);
                    }
                }
                Value::Array(a) => {
                    for x in a {
                        walk(x, key, depth + 1, out);
                    }
                }
                _ => {}
            }
        }
        walk(v, key, 0, &mut out);
        out
    }

    /// TotalDosage 解析：R-6 嵌套形态 data.Response.Data + 毫秒 ExpiredTime 归一
    #[test]
    fn parse_total_dosage_shapes() {
        let v = json!({
            "code": 0,
            "data": { "Response": { "Data": {
                "TotalDosage": 350,
                "Accounts": [ { "ExpiredTime": 1800000000000i64 }, { "ExpiredTime": 1790000000000i64 } ],
            } } }
        });
        let data = crate::fs_utils::dig(&v, &["data"]).expect("data 嵌套存在");
        // 与 fetch_total_dosage 实现一致：Response/Data 大写键不走 dig 信封表（全小写），
        // 用 path 严格下钻
        let core = crate::fs_utils::path(data, &["Response", "Data"]).expect("Response.Data 存在");
        let dosage = dig_f64(core, "TotalDosage");
        assert_eq!(dosage, Some(350.0));
        let expire = dig_all(core, "ExpiredTime")
            .into_iter()
            .filter_map(|t| t.as_f64())
            .map(|t| if t > 1e12 { t / 1000.0 } else { t })
            .min_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(expire, Some(1_790_000_000.0));
    }

    /// P9 抓包校准：CycleEndTime 字符串（+08:00 墙钟）解析；锚点取自实测抓包——
    /// CycleEndTime "2026-10-29 08:47:12" ↔ DeductionEndTime 1793234832000ms 双向印证
    #[test]
    fn parse_cn_datetime_capture_anchored() {
        let secs = parse_cn_datetime("2026-10-29 08:47:12").expect("解析成功");
        assert_eq!(secs, 1_793_234_832);
        assert_eq!(parse_cn_datetime(""), None);
        assert_eq!(parse_cn_datetime("9999-99-99 99:99:99"), None);
    }

    /// P9：分包字段 Precise 字符串优先、整数字段兜底
    #[test]
    fn pkg_f64_precise_priority() {
        let a = json!({"CapacityRemainPrecise": "248.05", "CapacityRemain": 248});
        assert_eq!(pkg_f64(&a, "CapacityRemainPrecise", "CapacityRemain"), 248.05);
        let b = json!({"CapacityRemain": 100});
        assert_eq!(pkg_f64(&b, "CapacityRemainPrecise", "CapacityRemain"), 100.0);
        let c = json!({});
        assert_eq!(pkg_f64(&c, "CapacityRemainPrecise", "CapacityRemain"), 0.0);
    }

    /// with_backoff：4xx 不重试直返、Ok 直返
    #[test]
    fn backoff_policy() {
        let mut calls = 0u32;
        let r: Result<u32, String> = with_backoff(|| {
            calls += 1;
            Err::<u32, _>(BackoffErr::Status(400))
        });
        assert!(r.is_err());
        assert_eq!(calls, 1, "4xx 语义性失败不重试");

        let mut calls2 = 0u32;
        let r2: Result<u32, String> = with_backoff(|| {
            calls2 += 1;
            Ok(42)
        });
        assert_eq!((r2.unwrap(), calls2), (42, 1));
    }

    // ── issue #179 兼容加固回归 ─────────────────────────────────────────────

    /// 包条目全树递归收集：容器改名/挪层级不再整包清零（总分在、包全空的 #179 症状）
    #[test]
    fn packages_collected_recursively_across_containers() {
        let core = json!({
            "TotalDosage": 100,
            "ResourcePackages": [
                {
                    "PackageName": "A",
                    "CapacityRemainPrecise": "12.5",
                    "CycleCapacitySizePrecise": "100",
                    "CycleEndTime": "2035-01-01 00:00:00",
                },
                { "PackageName": "B", "CapacityRemain": 3 },
            ],
        });
        let info = parse_dosage_body(&core);
        assert_eq!(info.packages.len(), 2);
        assert_eq!(info.packages[0].remain, 12.5);
        assert_eq!(info.packages[1].remain, 3.0);
        assert!(info.expire_secs.is_some());
        assert_eq!(info.total, 100.0);
        assert!(info.parse_diag.is_none(), "正常形态不出诊断");
    }

    /// 深层嵌套（信封外再包一层）同样命中：条目在 core 任意深度都能收集
    #[test]
    fn packages_found_at_arbitrary_depth() {
        let core = json!({
            "TotalDosage": 7,
            "Wrapper": { "Deep": { "Accounts": [
                { "PackageCode": "p1", "CapacityRemain": 7 },
            ] } },
        });
        let info = parse_dosage_body(&core);
        assert_eq!(info.packages.len(), 1);
        assert_eq!(info.packages[0].code, "p1");
    }

    /// 包容器挪出 core（data 信封其他分支 / 响应顶层）仍能收集——收集范围是
    /// 整个响应而非仅 core（审查修复：原实现只扫 core，挪出 core 症状依旧）
    #[test]
    fn packages_collected_outside_core() {
        let core = json!({ "TotalDosage": 7 });
        let root = json!({
            "code": 0,
            "data": { "Response": { "Data": { "TotalDosage": 7 } } },
            "Packages": [ { "PackageName": "A", "CapacityRemain": 7 } ],
        });
        let info = parse_dosage_body(&root);
        assert_eq!(info.packages.len(), 1);
        assert_eq!(info.packages[0].name, "A");
        assert_eq!(info.total, 7.0);
    }

    /// CycleEndTime 兼容：ISO 带时区 / 无时区 ISO / 纯日期（当日 23:59:59 +08:00）
    #[test]
    fn cycle_end_iso_and_date_only_fallback() {
        use chrono::TimeZone;
        let core = json!({ "TotalDosage": 1, "Accounts": [
            { "PackageName": "A", "CapacityRemain": 1, "CycleEndTime": "2035-01-02T03:04:05+08:00" },
            { "PackageName": "B", "CapacityRemain": 1, "CycleEndTime": "2035-02-03" },
        ]});
        let info = parse_dosage_body(&core);
        let offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let iso = offset
            .from_local_datetime(
                &chrono::NaiveDateTime::parse_from_str("2035-01-02T03:04:05", "%Y-%m-%dT%H:%M:%S")
                    .unwrap(),
            )
            .single()
            .unwrap()
            .timestamp();
        let date_only = offset
            .from_local_datetime(&chrono::NaiveDate::parse_from_str("2035-02-03", "%Y-%m-%d")
                .unwrap()
                .and_hms_opt(23, 59, 59)
                .unwrap())
            .single()
            .unwrap()
            .timestamp();
        // 「最近过期」= 两者最小值
        assert_eq!(info.expire_secs, Some(iso.min(date_only)));
        let a = info.packages.iter().find(|p| p.name == "A").unwrap();
        assert_eq!(a.expire_secs, Some(iso));
        let b = info.packages.iter().find(|p| p.name == "B").unwrap();
        assert_eq!(b.expire_secs, Some(date_only));
    }

    /// 未知 Status fail-open：不再静默丢弃整包，且落诊断；明细保留到期、
    /// 汇总不入池（未知包 CycleEndTime 不得污染「最近到期」）
    #[test]
    fn unknown_status_fail_open_and_diag() {
        let core = json!({ "TotalDosage": 5, "Accounts": [
            { "PackageName": "A", "CapacityRemain": 2, "Status": 7, "CycleEndTime": "2035-01-01 00:00:00" },
        ]});
        let info = parse_dosage_body(&core);
        assert_eq!(info.packages.len(), 1, "未知状态保留");
        assert!(info.packages[0].expire_secs.is_some(), "明细 fail-open 保留到期");
        assert_eq!(info.expire_secs, None, "未知状态包到期不入「最近到期」汇总");
        let diag = info.parse_diag.expect("未知状态出诊断");
        assert!(diag.contains("7"), "诊断携带原始 Status 值");
    }

    /// 包明细解析为空：出诊断并携带响应骨架（#179 修复前此场景完全静默）
    #[test]
    fn empty_packages_diag_with_shape() {
        let core = json!({ "TotalDosage": 9, "SomeNewContainer": [{ "Unrelated": 1 }] });
        let info = parse_dosage_body(&core);
        assert!(info.packages.is_empty());
        assert_eq!(info.total, 9.0);
        let diag = info.parse_diag.expect("明细空出诊断");
        assert!(diag.contains("#179"));
        assert!(diag.contains("SomeNewContainer"), "骨架暴露新容器名供定位");
    }

    /// ExpiredTime 字符串形态兼容（数字字符串，秒/毫秒归一）
    #[test]
    fn expired_time_string_form() {
        let core = json!({ "TotalDosage": 0, "Accounts": [
            { "PackageName": "A", "CapacityRemain": 1, "ExpiredTime": "4102444800000" },
        ]});
        let info = parse_dosage_body(&core);
        assert_eq!(info.expire_secs, Some(4_102_444_800), "毫秒字符串归一为秒");
    }

    /// 骨架：长字符串截断为长度标记（防日志膨胀），数组取前 2 个元素
    #[test]
    fn skeleton_truncates_and_caps() {
        let v = json!({
            "long": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "arr": [1, 2, 3, 4, 5],
        });
        let s = skeleton(&v, 0);
        assert_eq!(s["long"], json!("str(len=42)"));
        assert_eq!(s["arr"].as_array().unwrap().len(), 2);
    }
}
