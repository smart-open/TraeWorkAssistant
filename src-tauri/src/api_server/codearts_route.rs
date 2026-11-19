//! CodeArts（华为云码道）上游路由执行层（阶段2）。
//!
//! 请求流程（OpenAI 兼容上游，精简自 qoder_route / custom_route）：
//! 1. 目录条目解析（`codearts_catalog::resolve`：model → upstream_key/model_id）；
//! 2. 请求体构造：强制 `stream:true`，模型名对齐目录条目的 model_id；
//! 3. 取号（codearts_pool 调度，Key 白名单/专一约束同构适用）→ identity 回调
//!    取 AK/SK → `codearts_sign::sign` 对 `POST {SNAP_HOST}{CHAT_PATH}` 签名；
//! 4. 分级重试：429/5xx 同号退避、401/403 换号（凭证永久失效）、其余按
//!    `retry_plan` 决策；
//! 5. 流式：首字超时 10s → 上游 OpenAI SSE 直连 `wb_sse::stream_forward_ex`
//!    按客户端协议转发；非流式：`wb_sse::aggregate` 本地聚合后协议投影；
//! 6. 用量记账 `record_usage_codearts` 独立桶 + 请求级日志。
//!
//! 零明文红线：AK/SK 仅在 identity 回调内内存流转，绝不写日志；错误 message
//! 不含任何密钥片段。benefit 通道已下线，无需 maas_type 头。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use super::retry::{retry_plan, RetryAction};
use super::routes::{anthropic_error, openai_error, send_stream_error, Protocol};
use super::wb_sse;
use super::wb_sticky::SessionKey;
use super::wb_upstream;
use super::{ApiSharedState, ErrKind, InflightGuard};
use crate::tasks::codearts_common::CodeartsCreds;
use crate::tasks::codearts_sign::{self, CodeartsIdentity, CHAT_PATH, SNAP_HOST};

/// 上游推理完整 URL（纯函数便于单测锚定端点形态）
pub fn chat_url() -> String {
    format!("https://{SNAP_HOST}{CHAT_PATH}")
}

// ==================== 会话名额登记（并发闸控制协议）====================
//
// 码道的并发上限不吃「HTTP 请求结束」，它认的是一套显式会话登记协议：
//
//   1. `PUT /snap-manager/v1/chat-session/heartbeat?status=busy`
//      + 头 `user-session-id: <32 位 hex>` —— 登记一个会话名额；
//   2. 推理请求带**同一个** `user-session-id` 头，这次对话才计入该名额；
//   3. `PUT …?status=idle` —— 注销名额。
//
// 单账号只给 3 个名额，而**未登记的请求上游会各自新开一条会话且永不回收**
// （它只按超时回收）。不实现本协议的后果：串行请求 #1 成功后，#2 起一律撞
// `TM.00001041 并发会话数已达上限(3个)`——看起来像「一个对话里每个请求都算
// 一条新会话」，实际是它们全都无主、无处注销。
//
// status 必须走 **query**：放 body 会 400 TM.00001001 参数校验失败，
// POST/GET 同路径是 404 APIG.0101。

/// 会话心跳端点路径（签名与推理请求同一套 SDK-HMAC-SHA256）
pub const HEARTBEAT_PATH: &str = "/snap-manager/v1/chat-session/heartbeat";

/// 心跳完整 URL（纯函数便于单测锚定端点形态）
pub fn heartbeat_url(status: &str) -> String {
    format!("https://{SNAP_HOST}{HEARTBEAT_PATH}?status={status}")
}

/// 生成本次上游会话**名额** id：32 位十六进制（对齐官方 UUIDv4 去连字符的形状）。
///
/// 绝不复用外部传入的 id：`idle` 是**注销**语义，拿别人的 id 去 idle 会把
/// 用户正在用的那条对话踢掉。每请求新建——它标识的是「这次在途请求」。
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 对话亲和 id：**同一对话多轮稳定**，用作 `x-session-affinity`，让上游把同一
/// 对话路由到同一后端实例命中 prompt cache。
///
/// 口径与 WbSticky 的会话键同源（显式 `conversation_id` 优先，否则 messages
/// 前若干条的指纹），但不落盘、不绑账号——CodeArts 的上游粘性由上游按此 id
/// 自行路由，我们只需保证「同一对话的每轮请求发同一个值」。
pub fn affinity_of(body: &Value) -> String {
    let key = super::wb_sticky::SessionKey::from_body(body);
    let raw = key.cache_key();
    // 上游只认「32 位 hex 形状」更稳妥；指纹也可能是显式 cid，统一做一次摘要
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// 会话名额守卫：构造即登记 busy，Drop 即注销 idle。
///
/// 用 RAII 而非「成功路径手动注销」：签名失败、上游报错、客户端断连、重试换号
/// 等所有提前 return 的分支都能归还名额——漏一条就会永久吃掉一个并发位。
///
/// **名额 id 与亲和 id 是两回事**（两者都走 `user-session-id` 头，但语义不同）：
/// - **名额 id**（本结构持有）：每请求新建，只用于让 heartbeat 的 busy/idle
///   配对，管的是「并发槽有没有还回去」；
/// - **亲和 id**（`affinity_of`）：同一对话多轮稳定，用于上游会话粘性路由 +
///   prompt cache 命中，管的是「同一对话是否落到同一后端实例」。
///
/// 参考实现把两者混用同一个值，导致同一对话第 2 轮换了 id → 缓存全miss；
/// 而并发槽本身必须每请求独立（它按「在途请求数」计数，不是按对话数）。
struct SessionSlot {
    identity: CodeartsIdentity,
    session_id: String,
}

impl SessionSlot {
    /// 登记会话名额。失败返回 None（调用方按「无会话头」继续——上游会给未登记
    /// 请求单独开新会话，能跑但会漏名额，比直接失败对用户友好）。
    ///
    /// `data_dir` 仅用于失败留痕：名额登记失败直接决定并发名额会不会泄漏，
    /// 必须能在 app.log 里查到，否则「跑几十轮后突然全撞上限」无从归因。
    fn acquire(identity: &CodeartsIdentity, data_dir: &std::path::Path) -> Option<Self> {
        let session_id = new_session_id();
        let (ok, detail) = send_heartbeat(identity, &session_id, "busy");
        if !ok {
            crate::fs_utils::app_log(
                data_dir,
                &format!("codearts 会话登记失败（本轮不带会话头，可能漏名额）: {detail}"),
            );
            return None;
        }
        Some(Self { identity: identity.clone(), session_id })
    }

    fn id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        // 注销失败只能尽力而为：名额最终会被上游按超时回收
        let (_ok, _detail) = send_heartbeat(&self.identity, &self.session_id, "idle");
    }
}

/// 心跳专用 agent：连接 5s / 写 10s / 读 10s。
///
/// 不能用推理用的 `codearts_agent`（读超时 300s）：心跳是每请求两次的短控制
/// 调用，上游抖动时若按 300s 等，会把整个请求拖住 5 分钟才失败——而它只是
/// 「登记/归还一个并发名额」，失败本就该快速放弃（名额最终由上游超时回收）。
fn heartbeat_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_write(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(10))
        .max_idle_connections(8)
        .max_idle_connections_per_host(8)
        .build()
}

/// 发一条会话心跳（busy 登记 / idle 注销）。返回 (是否生效, 诊断文本)。
///
/// 判定为 **200 且 body 里 status == "ok"**：网关层可能用 HTTP 200 包着业务
/// 失败，只看状态码会让我们以为名额登记/注销成功。
fn send_heartbeat(identity: &CodeartsIdentity, session_id: &str, status: &str) -> (bool, String) {
    let url = heartbeat_url(status);
    // user-session-id 参与签名：改了头不重签会被上游 401
    let extra: [(&str, &str); 1] = [("user-session-id", session_id)];
    let signed = match codearts_sign::sign(identity, "PUT", &url, b"{}", &extra) {
        Ok(s) => s,
        Err(e) => return (false, format!("心跳签名失败: {e}")),
    };
    let agent = heartbeat_agent();
    let mut req = agent.put(&url);
    req = req.set("Authorization", &signed.authorization);
    req = req.set("x-sdk-date", &signed.x_sdk_date);
    req = req.set("content-type", &signed.content_type);
    if let Some(st) = &signed.x_security_token {
        req = req.set("x-security-token", st.as_str());
    }
    if let Some(sha) = &signed.x_sdk_content_sha256 {
        req = req.set("x-sdk-content-sha256", sha.as_str());
    }
    for (k, v) in &signed.extra {
        req = req.set(k.as_str(), v.as_str());
    }
    match req.send_bytes(&b"{}"[..]) {
        Ok(resp) => match resp.into_string() {
            Ok(text) => {
                let ok = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v.get("status").and_then(Value::as_str).map(|s| s == "ok"))
                    .unwrap_or(false);
                (ok, text.chars().take(160).collect())
            }
            Err(e) => (false, format!("心跳响应读取失败: {e}")),
        },
        Err(ureq::Error::Status(code, resp)) => {
            (false, format!("HTTP {code} {}", resp.into_string().unwrap_or_default().chars().take(160).collect::<String>()))
        }
        Err(e) => (false, format!("心跳传输错误: {e}")),
    }
}

/// 安全获取 Mutex 锁（毒化恢复）
fn safe_lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn chat_id_for(proto: Protocol) -> String {
    match proto {
        Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
        Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
        Protocol::Anthropic => format!("msg_{}", now_ts()),
        Protocol::Responses => format!("resp_{}", now_ts()),
    }
}

/// 请求体解析（loop 外一次）
fn parse_body_once(body_vec: &[u8]) -> Value {
    serde_json::from_slice(body_vec).unwrap_or_else(|_| json!({}))
}

/// 规范化客户端 messages 的 role，避免上游 `InferHub.001001044.400 Message
/// role cannot empty`：
/// - role 缺失/空串/非字符串 → 补 `user`（码道不认空 role，WB 侧也因空 role
///   原样透传踩过同样的坑）；
/// - `developer` → `system`（码道不认 developer 枚举）；
/// - 大小写归一为小写。
///
/// 只动 role 字段，不重排、不改 content——保持对话语义。
fn normalize_roles(core: &mut Value) {
    let Some(msgs) = core.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in msgs.iter_mut() {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        let role = obj.get("role").and_then(Value::as_str).unwrap_or("").trim();
        let fixed = if role.is_empty() {
            "user"
        } else if role.eq_ignore_ascii_case("developer") {
            "system"
        } else {
            // 大小写归一：role 枚举小写敏感
            let lower = role.to_ascii_lowercase();
            match lower.as_str() {
                "system" => "system",
                "user" => "user",
                "assistant" => "assistant",
                "tool" => "tool",
                _ => "user", // 未知枚举按 user 兜底，避免上游拒收
            }
        };
        obj.insert("role".into(), json!(fixed));
    }
}

/// 按目录条目重建请求体：强制 stream:true + 模型名对齐 model_id
/// + max_tokens 夹到目录声明上限 + role 规范化。
///
/// 夹取不是可选项：客户端习惯发「模型族最大值」（DSH 默认 128000），而码道
/// 各模型输出上限多为 65536，超限不回「值太大」而是 `InferHub.001001005.400
/// The request param is invalid`，且该错误会让整条请求失败（重试无意义）。
fn rebuild_body(core: &Value, model_id: &str, upstream_key: &str) -> Vec<u8> {
    let mut v = core.clone();
    v["stream"] = json!(true);
    if !model_id.is_empty() {
        v["model"] = json!(model_id);
    } else if !upstream_key.is_empty() {
        v["model"] = json!(upstream_key);
    }
    // role 规范化：空 role 直接触发 001001044，必须在发上游前修掉
    normalize_roles(&mut v);
    if let Some(limit) = crate::tasks::codearts_catalog::max_tokens_of(&v["model"]) {
        let asked = v.get("max_tokens").and_then(Value::as_u64).unwrap_or(0);
        // 未指定或超限都收敛到上限：缺省交给上游按默认值处理更保守，故仅在
        // 客户端显式给了值时才动（0/缺失保持缺失语义）
        if asked > limit {
            v["max_tokens"] = json!(limit);
        }
    }
    v.to_string().into_bytes()
}

/// CodeArts 专用 agent（直连，无系统代理：ureq 未启用 proxy-from-env feature）：
/// 连接 10s / 写 30s / 空闲读 300s，与流式转发长流同口径
fn codearts_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_read(Duration::from_secs(300))
        .timeout_write(Duration::from_secs(30))
        .timeout_connect(Duration::from_secs(10))
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

/// identity 回调解析 AK/SK → 构造 CodeartsIdentity（零明文日志）
fn identity_of(
    state: &ApiSharedState,
    uid: &str,
) -> Result<CodeartsIdentity, String> {
    let cb = state.codearts_identity.as_ref().ok_or_else(|| {
        "CodeArts 凭证解析回调未注入".to_string()
    })?;
    let creds: CodeartsCreds = cb(uid).map_err(|e| {
        // 永久失效（refresh_token 被拒/已过期）：转成用户可读提示，
        // 而不是把内部 STS5.1806 原文甩给客户端（用户看不懂要重登）
        if crate::tasks::codearts_common::is_needs_relogin_err(&e) {
            format!("CodeArts 账号凭证已失效，请在「CodeArts 账号管理」重新登录（原 refresh_token 已被使用或拒绝）")
        } else {
            e
        }
    })?;
    if !creds.is_valid() {
        return Err("CodeArts AK/SK 为空或失效，请重新登录或录入 AK/SK".into());
    }
    Ok(codearts_sign::identity_from_creds(&creds))
}

/// 发签名请求（ak/sk 已由 identity 校验；签名头逐条 set）：
/// 返回 reader（SSE 行源）或 (status, body) 错误。
///
/// `session_id`：已登记的会话名额句柄。**必须参与签名**（上游按签名覆盖的
/// 头集合校验，改了头不重签直接 401）。带上它这次推理才计入已 busy 的那条
/// 会话，收尾的 idle 才能真正归还并发名额。
fn send_signed_request(
    agent: &ureq::Agent,
    identity: &CodeartsIdentity,
    body: &[u8],
    session_id: Option<&str>,
    affinity: Option<&str>,
) -> Result<Box<dyn std::io::Read + Send>, (u16, String)> {
    let url = chat_url();
    // 会话头先落地再参与签名（顺序不能反：签名覆盖的正是最终发送的头集合）
    let mut extra: Vec<(&str, &str)> = Vec::new();
    if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        extra.push(("user-session-id", sid));
    }
    // 亲和头独立于名额头：同一对话稳定，换来 prompt cache 命中
    if let Some(aff) = affinity.filter(|s| !s.is_empty()) {
        extra.push(("x-session-affinity", aff));
    }
    let signed = match codearts_sign::sign(identity, "POST", &url, body, &extra) {
        Ok(s) => s,
        Err(e) => return Err((0, format!("CodeArts 请求签名失败: {e}"))),
    };
    let mut req = agent.post(&url);
    req = req.set("Authorization", &signed.authorization);
    req = req.set("x-sdk-date", &signed.x_sdk_date);
    req = req.set("content-type", &signed.content_type);
    // OAuth 临时凭据：security-token / content-sha256 必须逐条发送且与签名一致
    if let Some(st) = &signed.x_security_token {
        req = req.set("x-security-token", st.as_str());
    }
    if let Some(sha) = &signed.x_sdk_content_sha256 {
        req = req.set("x-sdk-content-sha256", sha.as_str());
    }
    req = req.set("accept", "text/event-stream");
    for (k, v) in &signed.extra {
        req = req.set(k.as_str(), v.as_str());
    }
    match req.send_bytes(body) {
        Ok(resp) => Ok(Box::new(resp.into_reader())),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            Err((code, text))
        }
        Err(e) => Err((502, format!("CodeArts 上游传输错误: {e}"))),
    }
}

/// 分流决策：同号退避 / 换号 / 终态（401/403 恒换号）
fn retry_decision(status: u16, body: &str, attempt: u32) -> RetryAction {
    if status == 401 || status == 403 {
        return RetryAction::SwitchKey;
    }
    retry_plan(status, body, attempt, None)
}

// ==================== CodeArts 错误归因（状态码 + 账号处置）====================

/// 并发会话数超限（码道单账号只给 3 个会话）：瞬时限流，短退避后重试即可，
/// **与账号健康无关**——按 Server 熔断 30 分钟会把整池打进
/// `no_healthy_account`，用户侧表现为「反代一直失败」。
fn is_concurrency_full(text: &str) -> bool {
    text.contains("TM.00001041")
        || text.contains("并发会话数已达上限")
}

/// 上游令牌桶限流（账号级速率上限，如 `InferHub.ModelArts.81114.429`）：
/// 与「并发会话数超限」是两回事——前者是**令牌/分钟速率**被打满，后者是
/// **在途会话条数**超限。两者都是短退避即可的瞬时限流，但归因文案不同，
/// 且限流窗口按分钟重置、会话数上限按会话释放。
///
/// 此前把 `ModelArts.811` / `rate limit` / `too many requests` 一并塞进
/// `is_concurrency_full`，导致上游令牌桶 429 被误报成「并发会话数已达上限」。
fn is_rate_limited(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    text.contains("ModelArts.811")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("429")
}

/// 令牌桶限流的冷却时长（秒）。
///
/// 码道的限流口径是「每分钟令牌上限」（如 `the rate limit is 300000000
/// tokens per minute`）——窗口按**自然分钟**重置，不是固定 60s。固定 60s
/// 有两个问题：
/// ① 冷却可能跨过下一个分钟窗口仍不放行（白等）；
/// ② 也可能刚过窗口就放行，随即再次 429（反复撞）。
///
/// 取「到下一个分钟边界 + 3s 余量」，既不早放行（避免立刻再 429）也不
/// 白等太久（最多多等 63s）。文案未明确分钟粒度时退回固定 60s。
fn rate_limit_cooldown_secs(text: &str) -> i64 {
    let lower = text.to_ascii_lowercase();
    let per_minute = lower.contains("per minute")
        || lower.contains("/min")
        || lower.contains("tokens per minute");
    if !per_minute {
        return 60;
    }
    let now = now_ts();
    // 到下一个自然分钟边界的秒数 + 3s 余量
    let secs_into_minute = now % 60;
    (60 - secs_into_minute) + 3
}

/// 请求级错误：请求本身不合法，换号重试不会变，冷却账号只会缩小可用池 →
/// `ErrKind::None`（不冷却，但透传真实原因）。返回该错误对客户端的状态码
/// （参数错 400 / 模型不可用 404），未命中返回 None。
fn request_level_status(text: &str) -> Option<u16> {
    let lower = text.to_ascii_lowercase();
    // 模型侧不可用（名字不对 / 未注册 / 账号无该模型权益）
    if lower.contains("002002009")
        || lower.contains("model is not registered")
        || lower.contains("4004.200")
        || lower.contains("benefit not found")
    {
        return Some(404);
    }
    // 请求参数本身不合法（含把图发给纯文本模型、内容审核拦截、空 role）
    if text.contains("TM.00001054")
        || text.contains("内容安全")
        || lower.contains("001001005")
        || lower.contains("001001044")     // Message role cannot empty：请求体问题
        || lower.contains("role cannot empty")
        || lower.contains("param is invalid")
        || lower.contains("malformed_json")
        || lower.contains("001001020")
        || lower.contains("not multimodal")
        || text.contains("不支持多模态")
    {
        return Some(400);
    }
    None
}

/// CodeArts 上游失败 → (对客户端的状态码, 账号处置分类)。
///
/// `status` 是上游 HTTP 状态码（流内错误帧传 200，真实码在文本里）。
/// 归因顺序即优先级：并发上限先于通用 5xx、请求级错误先于通用 4xx——
/// 否则这两类都会被 `classify_error` 的兜底口径吃掉，误熔断账号。
pub(crate) fn codearts_failure(status: u16, text: &str) -> (u16, ErrKind) {
    if is_concurrency_full(text) {
        return (429, ErrKind::SoftRate);
    }
    if is_rate_limited(text) {
        // 上游令牌桶限流：账号级短冷却（SoftRate）+ 429 回客户端。
        // 不按 Server 熔断——限流窗口过后账号即可用，熔断 30 分钟会把整池打死。
        return (429, ErrKind::SoftRate);
    }
    if let Some(s) = request_level_status(text) {
        // 模型/参数侧问题而非账号问题，不冷却
        return (s, ErrKind::None);
    }
    match status {
        // 凭据失效：APIG 签名/STS 令牌问题，换号（由 note_error 记 SessionDead）
        401 | 403 => (status, ErrKind::SessionDead),
        // 429 未带可识别文案：仍按短冷却限流处理（账号级速率上限），
        // 不能落到 4xx 兜底（ErrKind::None = 不冷却）——不冷却会让限流账号
        // 持续被选中并反复 429。
        429 => (429, ErrKind::SoftRate),
        s if s >= 500 => (s, ErrKind::Server),
        // 其余 4xx 属于客户端请求，账号不动
        s if s >= 400 => (s, ErrKind::None),
        // 流内错误帧（HTTP 200）未命中任何已知码：上游确实异常，按 Server
        // 熔断与其余池口径一致（连续 3 次才触发，不会单次打死账号）
        _ => (502, ErrKind::Server),
    }
}

// ==================== 模型级冷却（对齐 wb_route / qoder_route）====================

/// CodeArts 专属模型级冷却表：model → (冷却截止, 连续失败次数)。
///
/// **必须独立于 `state.model_cooldowns`（WB 池表）**：CodeArts 与 Buddy/Qoder
/// 存在同名模型（如 deepseek-v4.1-flash），共用裸 model 键会串池污染——
/// 一方模型的失败会让另一方的 `pool_health` 误判 ModelCooldown 而回退，
/// 违反 §3.1 失败域隔离（wb / buddy_ai 拆两张表正是为此）。
/// 照 Qoder 用进程级 OnceLock（最小改动，不动 ApiSharedState 的 N 处构造）。
static CODEARTS_MODEL_COOLDOWNS: OnceLock<Mutex<HashMap<String, (Instant, u32)>>> = OnceLock::new();

fn codearts_cooldown_map() -> &'static Mutex<HashMap<String, (Instant, u32)>> {
    CODEARTS_MODEL_COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 记录模型级失败：渐进退避 10→20→40s（封顶 40s），成功请求后清除。
///
/// 窗口未过期 = 同一次失败片段内的换号，不递增（对齐 qoder 片段计数）；
/// 已过期才视为新一轮失败，退避翻倍。单模型故障只降权该模型，让 dispatch
/// 的 pool_health 快速回退到其他池/模型，不每请求硬打已知故障的模型。
fn note_model_failure(model: &str) {
    let mut map = codearts_cooldown_map().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    map.entry(model.to_string())
        .and_modify(|(until, fails)| {
            if now >= *until {
                // 窗口已过期 = 新失败片段 → 递增并设新窗口；未过期 = 同片段换号，
                // 维持原截止时刻不后延（防持续失败把窗口无限滑动）
                *fails = fails.saturating_add(1);
                *until = now + Duration::from_secs(10u64 << (*fails - 1).min(2));
            }
        })
        .or_insert_with(|| (now + Duration::from_secs(10), 1));
}

/// 模型冷却剩余秒数（None = 无冷却 / 已过期）。dispatch pool_health 预检调用。
/// 键为裸 model 名（= dispatch 的 sources.codearts），与执行层记账同键。
pub fn model_cooling_remaining_secs(model: &str) -> Option<u64> {
    let map = codearts_cooldown_map().lock().unwrap_or_else(|e| e.into_inner());
    let (until, _) = map.get(model)?;
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() { None } else { Some(left.as_secs()) }
}

/// 请求成功后清除模型冷却：连续失败计数随条目一并清除，恢复从基准起步。
fn clear_model_failure(model: &str) {
    codearts_cooldown_map().lock().unwrap_or_else(|e| e.into_inner()).remove(model);
}

/// 按归因结果记账：账号级冷却 + 模型级冷却（仅 Server 类）。
/// 集中一处，避免流内错误/HTTP 错误/聚合三条路径口径漂移。
///
/// `upstream_text`：上游错误原文，用于按限流粒度算冷却时长（令牌桶按分钟
/// 窗口重置，固定 60s 会白等或早放行）。None = 走该错误类的默认时长。
fn note_failure(
    state: &ApiSharedState,
    model: &str,
    uid: &str,
    kind: ErrKind,
    upstream_text: Option<&str>,
) {
    if kind == ErrKind::None {
        return;
    }
    if kind == ErrKind::SoftRate {
        // 限流冷却时长按上游文案粒度算（令牌桶按分钟窗口）；无文案则用默认 60s
        let secs = upstream_text.map(rate_limit_cooldown_secs).unwrap_or(60);
        state.codearts_pool.note_error_until(
            uid,
            kind,
            Some(now_ts() + secs.max(1)),
        );
    } else {
        state.codearts_pool.note_error(uid, kind);
    }
    // 只有 Server（上游 5xx / 未知流内帧）是模型/上游故障：账号级
    // （401/429/并发名额）与请求级（400 参数/role）都不该冷却模型
    if kind == ErrKind::Server {
        note_model_failure(model);
    }
}

// ==================== 慢请求竞速对冲（对齐 wb_route::race_first_byte）====================

/// 对冲账号的上下文租约：在途计数 + CodeArts 会话名额（后者是码道独有）。
/// 构造即登记（计数 +1 + heartbeat busy），Drop 即归还（计数 -1 + idle）——
/// 建连失败、双败、竞速胜出移交后 settle 三条路径都恰好释放一次。
struct CodeArtsHedgeLease {
    uid: String,
    counter: Arc<std::sync::atomic::AtomicU32>,
    /// 对冲请求的会话名额；随租约存活，Drop 时 idle 注销上游并发槽
    _slot: SessionSlot,
}

impl Drop for CodeArtsHedgeLease {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 竞速胜者：生效 uid + 行源 + 对冲租约 + 败者标记
struct CodeArtsRaceWin {
    lines: Box<dyn Iterator<Item = String> + Send>,
    uid: String,
    hedge: Option<CodeArtsHedgeLease>,
    primary_failed: bool,
    hedge_failed: bool,
    read_slot: wb_upstream::ReadErrSlot,
}

/// 首字竞速（阈值 0 关闭 = 与纯首字超时语义一致）。
///
/// 与 WB 的关键差异：对冲请求必须**独立登记会话名额**（SessionSlot），否则
/// 上游按无主会话处理、名额只靠超时回收，几轮就撞 TM.00001041。affinity
/// 沿用主请求的值保 prompt cache；session_id 必须独立（idle 是注销语义）。
#[allow(clippy::too_many_arguments)]
fn race_codearts_first_byte(
    state: &Arc<ApiSharedState>,
    primary_uid: &str,
    body: &[u8],
    affinity: &str,
    reader: Box<dyn std::io::Read + Send>,
    hedge_ms: u64,
) -> Result<CodeArtsRaceWin, ()> {
    if hedge_ms == 0 || body.len() > super::routes::HEDGE_SKIP_BODY_BYTES {
        let read_slot = wb_upstream::new_read_err_slot();
        return wb_upstream::lines_with_first_byte_timeout_tracked(reader, read_slot.clone())
            .map(|lines| CodeArtsRaceWin {
                lines,
                uid: primary_uid.to_string(),
                hedge: None,
                primary_failed: false,
                hedge_failed: false,
                read_slot,
            });
    }
    let state2 = state.clone();
    let body2 = body.to_vec();
    let affinity2 = affinity.to_string();
    let data_dir2 = state.data_dir.clone();
    let spawn_backup = move || -> Option<(Box<dyn std::io::Read + Send>, CodeArtsHedgeLease)> {
        // 取第二账号（主账号在 inflight 中，pick_excluding 自然排除）
        let picked = state2.codearts_pool.pick_excluding_constrained(
            &[primary_uid.to_string()].into_iter().collect(), None, None,
        )?;
        // 对冲账号的 AK/SK 按 uid 重新解析（不能复用主账号的 identity）
        let hedge_identity = identity_of(&state2, &picked.uid).ok()?;
        // 会话名额：对冲独有，绝不复用主请求的 session_id
        let slot = SessionSlot::acquire(&hedge_identity, &data_dir2)?;
        let counter = state2.codearts_pool.inflight_handle(&picked.uid);
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 建连失败时租约随闭包局部变量 Drop（计数 -1 + slot idle）
        let reader = send_signed_request(
            &codearts_agent(), &hedge_identity, &body2, Some(slot.id()), Some(&affinity2),
        )
        .ok()?;
        Some((reader, CodeArtsHedgeLease {
            uid: picked.uid, counter, _slot: slot,
        }))
    };
    let out = wb_upstream::lines_with_first_byte_hedged(reader, hedge_ms, spawn_backup)
        .map_err(|_| ())?;
    Ok(CodeArtsRaceWin {
        lines: out.lines,
        uid: if out.takeover {
            out.hedge.as_ref().map(|l| l.uid.clone()).unwrap_or_else(|| primary_uid.to_string())
        } else {
            primary_uid.to_string()
        },
        hedge: out.hedge,
        primary_failed: out.primary_failed,
        hedge_failed: out.hedge_failed,
        read_slot: out.read_slot,
    })
}

/// 竞速收尾：败者补记错误（primary_failed/hedge_failed），对冲租约释放，
/// 接管时把 in-flight guard 重绑到对冲账号。返回 (生效 uid, 是否接管)。
fn settle_codearts_hedge(
    state: &Arc<ApiSharedState>,
    win: &mut CodeArtsRaceWin,
    primary_uid: &str,
) -> (String, bool) {
    let effective_uid = win.uid.clone();
    let takeover = effective_uid != primary_uid;
    if win.primary_failed {
        state.codearts_pool.note_error(primary_uid, ErrKind::Server);
    }
    if win.hedge_failed {
        if let Some(lease) = win.hedge.as_ref() {
            state.codearts_pool.note_error(&lease.uid, ErrKind::Server);
        }
    }
    if let Some(lease) = win.hedge.take() {
        state.logger.log_sched_event(&format!(
            "hedge_{} primary={} effective={}",
            if takeover { "takeover" } else { "lost" },
            primary_uid, lease.uid
        ));
        drop(lease); // 计数 -1 + slot idle（SessionSlot Drop）
    }
    (effective_uid, takeover)
}

/// 请求阶段（观测口径）
fn stage_of(peek: &Value) -> &'static str {
    let tools = peek.get("tools").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
    if tools > 0 { "tool" } else { "main" }
}

/// 末条 user 消息截断（日志/流水预览）
fn preview_of(peek: &Value) -> String {
    let text = peek
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().rev().find(|m| m.get("role").and_then(Value::as_str) == Some("user")))
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .unwrap_or("");
    text.chars().take(120).collect()
}

fn set_last_error(state: &ApiSharedState, msg: String) {
    *safe_lock(&state.last_error) = Some(msg);
}

fn internal_error_response() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// CodeArts 流式对话（routes.rs 各协议端点 TargetPool::CodeArts 分支调用）。
/// 签名与 qoder_route::qoder_stream_chat 完全一致（encoded/fallback 仅透传，
/// CodeArts 自身不消费编码工具名；fallback 未接线，保留参数以统一签名）。
#[allow(clippy::too_many_arguments)]
pub fn codearts_stream_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
    encoded: HashSet<String>,
    fallback: Vec<super::dispatch::PoolCandidate>,
) -> Response {
    let _ = (&encoded, &fallback); // 精简版暂不消费，保留签名统一
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    super::stream_runtime().spawn_blocking(move || {
        let chat_id = chat_id_for(proto);
        let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
        let _done = super::routes::DoneSignal(done_tx);
        {
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(15));
                tick.tick().await;
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            if tx2.send(Ok(bytes::Bytes::from(": keep-alive\n\n"))).await.is_err() {
                                break;
                            }
                        }
                        _ = done_rx.changed() => break,
                    }
                }
            });
        }
        run_codearts_stream(&state, &body_vec, &model, proto, &key_id, &chat_id, &tx, start_ts, guard);
    });
    let stream = ReceiverStream::new(rx);
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| internal_error_response())
}

/// 流式执行核心
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_codearts_stream(
    state: &Arc<ApiSharedState>,
    body_vec: &[u8],
    model: &str,
    proto: Protocol,
    key_id: &str,
    chat_id: &str,
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    start_ts: Instant,
    mut guard: InflightGuard,
) {
    let key_name = super::api_keys::key_name_for(&state.data_dir, key_id);
    let peek = parse_body_once(body_vec);
    let _stage = stage_of(&peek);
    let preview = preview_of(&peek);
    let session_ctx = super::usage::CodeartsSessionCtx {
        request_id: uuid::Uuid::new_v4().to_string(),
        session_id: String::new(),
        ts: chrono::Local::now().timestamp(),
        preview: preview.clone(),
    };
    // 目录条目（upstream_key/model_id）：调度命中后目录刷新导致条目消失按 404 透传
    let Some((upstream_key, model_id)) = crate::tasks::codearts_catalog::resolve(model) else {
        let msg = format!("model {model} not in CodeArts catalog");
        state.logger.log_request("codearts", "POST", proto.log_path(), model, true, 404, "-",
            start_ts.elapsed().as_millis() as u64, &key_name, "", Some(&msg));
        send_stream_error(tx, proto, 404, &msg);
        return;
    };
    let core = peek.clone();
    let prepared = rebuild_body(&core, &model_id, &upstream_key);
    // 对话亲和 id：同一对话多轮稳定（prompt cache 命中），与名额 id 解耦
    let affinity = affinity_of(&core);
    let (allowed, dedicated) = super::api_keys::constraints_for(&state.data_dir, key_id)
        .and_then(|k| k.pool_constraints("codearts"))
        .map_or((None, None), |c| (c.allowed, c.dedicated));
    // 会话粘性（对齐 WB/Qoder）：同一对话绑定到同一账号，保住上游 prompt cache。
    // 与 affinity_of（x-session-affinity 头）正交：前者是本地账号绑定，后者是
    // 上游会话亲和头。开关默认关，字段/命名空间/TTL 均已就绪，纯接线。
    let sticky_key = SessionKey::from_body(&core);
    let sticky_uid: Option<String> = if state
        .codearts_sticky_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        state.codearts_sticky.resolve(&sticky_key, now_ts()).and_then(|b| {
            // 子 Key 白名单不含绑定账号 → 忽略粘性
            if allowed.as_ref().is_some_and(|a| !a.contains(&b.uid)) {
                return None;
            }
            // 账号仍在池中且可选才沿用（账号被删/冷却则忽略）
            state.codearts_pool.pick_by_uid(&b.uid, false).map(|_| b.uid)
        })
    } else {
        None
    };
    let mut tried: HashSet<String> = HashSet::new();
    // 每账号同号重试计数（retry_plan 的 attempt 入参，控制退避轮次上限）
    let mut attempts: HashMap<String, u32> = HashMap::new();
    let mut last_err = String::from("CodeArts 池无可用账号");
    // 轮换耗尽时的对客户端状态码：优先透传上游真实归因（并发上限 429 /
    // 请求级 400-404），未记录过具体错误才回落 503「池无可用账号」。
    // 修复前一律 503，客户端把瞬时限流当成网关故障无限重试同一条请求。
    let mut exhausted_status: i64 = 503;
    loop {
        // 首轮优先粘性账号（busy 自动让位并记录 [SCHED]），其后按常规调度
        let picked = if tried.is_empty() {
            sticky_uid.as_ref().and_then(|uid| {
                state.codearts_pool.pick_sticky_yield(uid, allowed.as_ref(), false).map(|(p, ev)| {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                })
            })
        } else {
            None
        }
        .or_else(|| state.codearts_pool.pick_excluding_constrained(
            &tried, allowed.as_ref(), dedicated.as_deref(),
        ));
        let Some(picked) = picked else {
            state.record_usage_codearts(model, "-", key_id, false, true,
                start_ts.elapsed().as_millis() as u64, 0, 0, None, &session_ctx);
            state.logger.log_request("codearts", "POST", proto.log_path(), model, true,
                exhausted_status as u16, "-", start_ts.elapsed().as_millis() as u64,
                &key_name, "", Some(&last_err));
            send_stream_error(tx, proto, exhausted_status, &last_err);
            return;
        };
        tried.insert(picked.uid.clone());
        // F-77 账号在途绑定（CAS 复核并发上限）
        if !picked.over_limit {
            let acct = state.codearts_pool.inflight_handle(&picked.uid);
            if !guard.bind_account_cas(acct, state.codearts_pool.concurrency_limit()) {
                continue;
            }
        }
        let identity = match identity_of(state, &picked.uid) {
            Ok(id) => id,
            Err(e) => {
                // 暂态错误（vault 抖动等）不标记账号永久失效，仅换号重试；
                // 凭证解析永久失败才记 SessionDead，避免一次抖动禁用健康账号
                if !crate::tasks::codearts_common::is_transient_err(&e) {
                    state.codearts_pool.note_error(&picked.uid, ErrKind::SessionDead);
                    // 账号永久失效：解绑会话粘性，让后续请求改选其他账号
                    if state.codearts_sticky_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                        state.codearts_sticky.unbind_uid(&picked.uid);
                        state.codearts_sticky.save(&state.data_dir);
                    }
                }
                last_err = e;
                continue;
            }
        };
        // 登记会话名额（RAII：本循环迭代结束时自动 idle 注销）。
        // 必须在取号之后、发请求之前——名额是**按账号**算的。
        let slot = SessionSlot::acquire(&identity, &state.data_dir);
        let session_id = slot.as_ref().map(|s| s.id());
        match send_signed_request(&codearts_agent(), &identity, &prepared, session_id, Some(&affinity)) {
            Err((status, resp_body)) => {
                // 同号重试计数：retry_plan 以 attempt 递增判断「退避是否耗尽」，
                // 恒传 0 会让 429/503 永远返回 RetrySame（要么死循环，要么因
                // 账号已进 tried 而假重试 → 直接退化成池耗尽 503）
                let attempt = attempts.entry(picked.uid.clone()).or_insert(0);
                match retry_decision(status, &resp_body, *attempt) {
                    RetryAction::RetrySame { delay_ms } => {
                        *attempt += 1;
                        std::thread::sleep(Duration::from_millis(delay_ms.min(60_000)));
                        // 同号重试：把账号移出 tried 才可能被再次选中
                        tried.remove(&picked.uid);
                        continue;
                    }
                    RetryAction::SwitchKey => {
                        let (client_status, kind) = codearts_failure(status, &resp_body);
                        note_failure(state, model, &picked.uid, kind, Some(&resp_body));
                        let msg = format!("upstream status={status}");
                        set_last_error(state, format!("codearts uid={} model={} status={} body={}",
                            picked.uid, model, status, resp_body.chars().take(200).collect::<String>()));
                        state.logger.log_request("codearts", "POST", proto.log_path(), model, true,
                            status, &picked.uid, start_ts.elapsed().as_millis() as u64, &key_name,
                            &state.codearts_pool.name_of(&picked.uid), Some(&msg));
                        exhausted_status = client_status as i64;
                        last_err = msg;
                        continue;
                    }
                    RetryAction::Fatal => {
                        // 终态：请求级错误（参数/模型/审核）换号无意义，也不冷却账号
                        let (client_status, kind) = codearts_failure(status, &resp_body);
                        note_failure(state, model, &picked.uid, kind, Some(&resp_body));
                        let msg = format!("upstream {status} error: {}",
                            resp_body.chars().take(300).collect::<String>());
                        state.record_usage_codearts(model, &picked.uid, key_id, false, true,
                            start_ts.elapsed().as_millis() as u64, 0, 0, None, &session_ctx);
                        state.logger.log_request("codearts", "POST", proto.log_path(), model, true,
                            client_status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                            &key_name, &state.codearts_pool.name_of(&picked.uid), Some(&msg));
                        send_stream_error(tx, proto, client_status as i64, &msg);
                        return;
                    }
                }
            }
            Ok(reader) => {
                // 首字竞速对冲（阈值 0 关闭，与纯首字超时同语义）：主请求慢时
                // 对第二账号发对冲请求（独立 SessionSlot 登记名额），先出首字者胜
                let hedge_ms = state.codearts_hedge_threshold_ms.load(std::sync::atomic::Ordering::Relaxed);
                let mut win = match race_codearts_first_byte(
                    state, &picked.uid, &prepared, &affinity, reader, hedge_ms,
                ) {
                    Ok(w) => w,
                    Err(()) => {
                        note_failure(state, model, &picked.uid, ErrKind::Server, None);
                        last_err = "CodeArts 上游 10 秒内未响应（首字超时）".into();
                        state.logger.log_request("codearts", "POST", proto.log_path(), model, true,
                            504, &picked.uid, start_ts.elapsed().as_millis() as u64, &key_name,
                            &state.codearts_pool.name_of(&picked.uid), Some(&last_err));
                        continue;
                    }
                };
                // 竞速收尾：败者补记错误、释放对冲租约；接管时生效 uid 切到对冲账号
                let (win_uid, _took_over) = settle_codearts_hedge(state, &mut win, &picked.uid);
                let read_slot = win.read_slot.clone();
                // 竞速胜者行源是 Box<dyn Iterator>，桥接为 InterruptibleLines
                // 供 stream_forward_ex 消费（读错误经 read_slot 单独合并）
                let lines = wb_upstream::InterruptibleLines::from_iterator(win.lines);
                let (error_info, _sent_any, failed_inline, up_usage) = wb_sse::stream_forward_ex(
                    lines, tx, proto, chat_id, &model_id, &HashSet::new(), None,
                );
                // 胜者行源读错误（h2 RST 等掐断）合并进错误归因
                let cut = super::transport_cut_from_slot(&read_slot);
                let error_info = match (cut, error_info) {
                    (Some(cut), None) => Some(cut),
                    (Some(cut), Some((c, m))) if super::is_empty_completion(c, &m) => Some(cut),
                    (_, other) => other,
                };
                let duration_ms = start_ts.elapsed().as_millis() as u64;
                let ok = error_info.is_none() && !failed_inline;
                let (pt, ct) = up_usage.as_ref().map(wb_sse::u64_pair).unwrap_or((0, 0));
                // 记账按生效账号（对冲接管时为 win_uid，不能记到主账号头上）
                state.record_usage_codearts(&model, &win_uid, key_id, ok, true,
                    duration_ms, pt, ct, None, &session_ctx);
                if ok {
                    state.codearts_pool.note_success(&win_uid);
                    clear_model_failure(model);
                    // 成功后写入会话粘性绑定（conv_id 用 affinity 留档：CodeArts
                    // 的上游会话由 affinity/session-id 决定，绑定真正生效的是 uid）
                    if state.codearts_sticky_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                        state.codearts_sticky.bind(&sticky_key, &win_uid, &affinity, now_ts());
                        state.codearts_sticky.save(&state.data_dir);
                    }
                    state.logger.log_request("codearts", "POST", proto.log_path(), model, true,
                        200, &win_uid, duration_ms, &key_name,
                        &state.codearts_pool.name_of(&win_uid), None);
                    return;
                }
                if failed_inline {
                    state.logger.log_request("codearts", "POST", proto.log_path(), model, true,
                        200, &win_uid, duration_ms, &key_name,
                        &state.codearts_pool.name_of(&win_uid), Some("流内失败已透传客户端"));
                    return;
                }
                if let Some((code, msg)) = error_info {
                    // 归因按真实错误码分轨（并发上限 / 请求级错误不冷却账号）：
                    // 修复前一律 Server → 账号熔断 30 分钟 → 整池 no_healthy_account
                    let detail = format!("{code} {msg}");
                    let (client_status, kind) = codearts_failure(0, &detail);
                    note_failure(state, model, &win_uid, kind, Some(&detail));
                    set_last_error(state, format!("codearts uid={} model={} code={} msg={}",
                        win_uid, model, code, msg));
                    // 保留真实状态码与原因，轮换耗尽时按此透传（不再一律 503）
                    exhausted_status = client_status as i64;
                    last_err = msg;
                }
            }
        }
    }
}

/// CodeArts 非流式对话（上游只回 SSE → 本地聚合；签名与 qoder_aggregate_chat 一致）
#[allow(clippy::too_many_arguments)]
pub async fn codearts_aggregate_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    stream: bool,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let model_out = model.clone();
    let result = super::stream_runtime().spawn_blocking(move || {
        let mut guard = guard;
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        let peek = parse_body_once(&body_vec);
        let session_ctx = super::usage::CodeartsSessionCtx {
            request_id: uuid::Uuid::new_v4().to_string(),
            session_id: String::new(),
            ts: chrono::Local::now().timestamp(),
            preview: preview_of(&peek),
        };
        let Some((upstream_key, model_id)) = crate::tasks::codearts_catalog::resolve(&model) else {
            return Err(format!("upstream 404 error: model {model} not in CodeArts catalog"));
        };
        let prepared = rebuild_body(&peek, &model_id, &upstream_key);
        let affinity = affinity_of(&peek);
        let (allowed, dedicated) = super::api_keys::constraints_for(&state.data_dir, &key_id)
            .and_then(|k| k.pool_constraints("codearts"))
            .map_or((None, None), |c| (c.allowed, c.dedicated));
        // 会话粘性（同流式路径）：同一对话绑定同一账号，保住上游 prompt cache
        let sticky_key = SessionKey::from_body(&peek);
        let sticky_uid: Option<String> = if state
            .codearts_sticky_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            state.codearts_sticky.resolve(&sticky_key, now_ts()).and_then(|b| {
                if allowed.as_ref().is_some_and(|a| !a.contains(&b.uid)) {
                    return None;
                }
                state.codearts_pool.pick_by_uid(&b.uid, false).map(|_| b.uid)
            })
        } else {
            None
        };
        let mut tried: HashSet<String> = HashSet::new();
        let mut attempts: HashMap<String, u32> = HashMap::new();
        let mut last_err = String::from("CodeArts 池无可用账号");
        // 轮换耗尽时的对客户端状态码（同流式口径：优先透传真实归因）
        let mut exhausted_status: i64 = 503;
        loop {
            // 首轮优先粘性账号（busy 自动让位）
            let picked = if tried.is_empty() {
                sticky_uid.as_ref().and_then(|uid| {
                    state.codearts_pool.pick_sticky_yield(uid, allowed.as_ref(), false).map(|(p, ev)| {
                        if let Some(ev) = ev {
                            state.logger.log_sched_event(&ev);
                        }
                        p
                    })
                })
            } else {
                None
            }
            .or_else(|| state.codearts_pool.pick_excluding_constrained(
                &tried, allowed.as_ref(), dedicated.as_deref(),
            ));
            let Some(picked) = picked else {
                state.record_usage_codearts(&model, "-", &key_id, false, stream,
                    start_ts.elapsed().as_millis() as u64, 0, 0, None, &session_ctx);
                return Err(format!("upstream {exhausted_status} error: {last_err}"));
            };
            tried.insert(picked.uid.clone());
            if !picked.over_limit {
                let acct = state.codearts_pool.inflight_handle(&picked.uid);
                if !guard.bind_account_cas(acct, state.codearts_pool.concurrency_limit()) {
                    continue;
                }
            }
            let identity = match identity_of(&state, &picked.uid) {
                Ok(id) => id,
                Err(e) => {
                    state.codearts_pool.note_error(&picked.uid, ErrKind::SessionDead);
                    if state.codearts_sticky_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                        state.codearts_sticky.unbind_uid(&picked.uid);
                        state.codearts_sticky.save(&state.data_dir);
                    }
                    last_err = e;
                    continue;
                }
            };
            // 登记会话名额（RAII：离开本轮迭代自动 idle 注销）
            let slot = SessionSlot::acquire(&identity, &state.data_dir);
            let session_id = slot.as_ref().map(|s| s.id());
            match send_signed_request(&codearts_agent(), &identity, &prepared, session_id, Some(&affinity)) {
                Err((status, resp_body)) => {
                    let attempt = attempts.entry(picked.uid.clone()).or_insert(0);
                    match retry_decision(status, &resp_body, *attempt) {
                        RetryAction::RetrySame { delay_ms } => {
                            *attempt += 1;
                            std::thread::sleep(Duration::from_millis(delay_ms.min(60_000)));
                            tried.remove(&picked.uid);
                            continue;
                        }
                        RetryAction::SwitchKey => {
                            let (client_status, kind) = codearts_failure(status, &resp_body);
                            note_failure(&state, &model, &picked.uid, kind, Some(&resp_body));
                            set_last_error(&state, format!("codearts uid={} model={} status={} body={}",
                                picked.uid, model, status,
                                resp_body.chars().take(200).collect::<String>()));
                            state.logger.log_request("codearts", "POST", proto.log_path(), &model,
                                stream, status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                &key_name, &state.codearts_pool.name_of(&picked.uid),
                                Some(&format!("upstream status={status}")));
                            exhausted_status = client_status as i64;
                            last_err = format!("upstream status={status}");
                            continue;
                        }
                        RetryAction::Fatal => {
                            // 终态：请求级错误（参数/模型/审核）换号无意义，也不冷却账号
                            let (client_status, kind) = codearts_failure(status, &resp_body);
                            note_failure(&state, &model, &picked.uid, kind, Some(&resp_body));
                            let msg = format!("upstream {client_status} error: {}",
                                resp_body.chars().take(300).collect::<String>());
                            state.record_usage_codearts(&model, &picked.uid, &key_id, false, stream,
                                start_ts.elapsed().as_millis() as u64, 0, 0, None, &session_ctx);
                            return Err(msg);
                        }
                    }
                }
                Ok(reader) => {
                    let lines = match wb_upstream::lines_with_first_byte_timeout_interruptible(reader) {
                        Ok(l) => l,
                        Err(()) => {
                            note_failure(&state, &model, &picked.uid, ErrKind::Server, None);
                            last_err = "CodeArts 上游 10 秒内未响应（首字超时）".into();
                            continue;
                        }
                    };
                    let agg_chat_id = format!("chatcmpl-{}", now_ts());
                    let (resp, error) = wb_sse::aggregate(lines, &agg_chat_id, None);
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    match (resp, error) {
                        (Some(mut r), None) => {
                            r["model"] = json!(model);
                            let (pt, ct) = r.get("usage").map(wb_sse::u64_pair).unwrap_or((0, 0));
                            state.record_usage_codearts(&model, &picked.uid, &key_id, true, stream,
                                duration_ms, pt, ct, None, &session_ctx);
                            state.codearts_pool.note_success(&picked.uid);
                            clear_model_failure(&model);
                            // 成功后写入会话粘性绑定（同流式路径）
                            if state.codearts_sticky_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                state.codearts_sticky.bind(&sticky_key, &picked.uid, &affinity, now_ts());
                                state.codearts_sticky.save(&state.data_dir);
                            }
                            state.logger.log_request("codearts", "POST", proto.log_path(), &model,
                                stream, 200, &picked.uid, duration_ms, &key_name,
                                &state.codearts_pool.name_of(&picked.uid), None);
                            return Ok(r);
                        }
                        (_, Some((code, msg))) => {
                            // 归因按真实错误码分轨（与非流式同口径）：并发上限与
                            // 请求级错误不得按 Server 熔断账号
                            let detail = format!("{code} {msg}");
                            let (client_status, kind) = codearts_failure(0, &detail);
                            note_failure(&state, &model, &picked.uid, kind, Some(&detail));
                            state.record_usage_codearts(&model, &picked.uid, &key_id, false, stream,
                                duration_ms, 0, 0, None, &session_ctx);
                            set_last_error(&state, format!("codearts uid={} model={} code={} msg={}",
                                picked.uid, model, code, msg));
                            last_err = format!("upstream {client_status} error: {detail}");
                            exhausted_status = client_status as i64;
                        }
                        (None, None) => {
                            note_failure(&state, &model, &picked.uid, ErrKind::Server, None);
                            last_err = "CodeArts 上游返回空响应".into();
                        }
                    }
                }
            }
        }
    }).await;

    match result {
        Ok(Ok(resp)) => {
            let body = match proto {
                Protocol::Anthropic => wb_sse::completion_to_anthropic(
                    &resp, &format!("msg_{}", now_ts()), &model_out, None,
                ),
                Protocol::OpenAiText => wb_sse::completion_to_text(&resp, &model_out),
                Protocol::Responses => super::wb_responses::completion_to_responses(
                    &resp, &format!("resp_{}", now_ts()), &model_out, &HashSet::new(),
                ),
                Protocol::OpenAi => resp,
            };
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|_| internal_error_response())
        }
        Ok(Err(msg)) => {
            let upstream_status = msg
                .strip_prefix("upstream ")
                .and_then(|rest| rest.split(' ').next())
                .and_then(|s| s.parse::<u16>().ok());
            let status = match upstream_status {
                Some(s) if (400..500).contains(&s) && !matches!(s, 401 | 403) => {
                    StatusCode::from_u16(s).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
                }
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            match proto {
                Protocol::Anthropic => anthropic_error(status, "api_error", &msg),
                _ => openai_error(status, "upstream_error", &msg),
            }
        }
        Err(e) => openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error",
            &format!("task join error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_anchored_to_snap_host() {
        assert_eq!(chat_url(), "https://snap-access.cn-north-4.myhuaweicloud.com/api/v2/chat/completions");
    }

    #[test]
    fn rebuild_body_forces_stream_and_model() {
        let core = json!({"model": "old", "stream": false, "messages": [{"role":"user","content":"hi"}]});
        let body = rebuild_body(&core, "glm-5", "glm5-up");
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["stream"], true);
        assert_eq!(v["model"], "glm-5");
    }

    /// max_tokens 夹取：超上游声明上限必须收敛，否则 001001005 整条失败。
    /// 目录未注册（测试进程未注入 data_dir）时必须原样保留，不得凭空猜值。
    /// 空 role 是 001001044 的直接成因：客户端（尤其带历史/工具消息时）可能
    /// 产出 role 为空或非字符串的消息，上游严格校验直接 400。修复前原样透传
    /// → 用户侧「处理失败」，还会被误判成 Server 熔断账号。
    #[test]
    fn rebuild_body_normalizes_empty_roles() {
        let core = json!({
            "model": "x",
            "messages": [
                {"role": "", "content": "空 role"},
                {"content": "缺 role"},
                {"role": 123, "content": "非字符串 role"},
                {"role": "DEVELOPER", "content": "大写 developer"},
                {"role": "user", "content": "正常"},
                {"role": "assistant", "content": "正常助手"}
            ],
            "stream": false
        });
        let v: Value = serde_json::from_slice(&rebuild_body(&core, "glm-5", "u")).unwrap();
        let roles: Vec<&str> = v["messages"].as_array().unwrap()
            .iter().map(|m| m["role"].as_str().unwrap_or("")).collect();
        assert_eq!(
            roles,
            vec!["user", "user", "user", "system", "user", "assistant"],
            "空/缺失/非字符串 role 补 user，developer→system，其余归一"
        );
    }

    #[test]
    fn rebuild_body_preserves_max_tokens_without_catalog() {
        let core = json!({"model": "m", "max_tokens": 128000, "messages": []});
        let v: Value = serde_json::from_slice(&rebuild_body(&core, "glm-5", "u")).unwrap();
        assert_eq!(v["max_tokens"], json!(128000), "无目录声明时不得猜上限");
        // 客户端未给 max_tokens：保持缺失（交上游默认），不注入虚构值
        let core2 = json!({"model": "m", "messages": []});
        let v2: Value = serde_json::from_slice(&rebuild_body(&core2, "glm-5", "u")).unwrap();
        assert!(v2.get("max_tokens").is_none());
    }

    /// 并发会话上限是瞬时限流：必须短退避可重试，且**不得**按 Server 熔断账号。
    /// 修复前该帧被当成空完成 → ErrKind::Server × 3 → 账号熔断 30 分钟 →
    /// 全池 no_healthy_account（用户侧就是「codearts 反代一直失败」）。
    #[test]
    fn concurrency_limit_is_transient_not_account_fault() {
        let (status, kind) = codearts_failure(
            200,
            "TM.00001041 并发会话数已达上限(3个)，请关闭部分会话后重试。",
        );
        assert_eq!(status, 429, "并发上限应回 429 让客户端退避");
        assert_eq!(kind, ErrKind::SoftRate, "只能是短冷却，不能熔断 30 分钟");
        // HTTP 400 形态（上游偶发直接把该错误挂在状态码上）同样处理
        assert_eq!(
            codearts_failure(400, r#"{"error_code":"TM.00001041","error_msg":"并发会话数已达上限"}"#),
            (429, ErrKind::SoftRate)
        );
    }

    /// 请求级错误（参数不合法 / 非多模态 / 内容审核）与账号健康无关：
    /// 换号重试不会变，冷却账号只会白白缩小可用池 → ErrKind::None。
    #[test]
    fn request_level_errors_do_not_penalize_account() {        for (text, want_status) in [
            ("InferHub.001001005.400 The request param is invalid", 400u16),
            ("InferHub.001001020.406 The request model is not multimodal", 400),
            ("TM.00001054 内容安全审核未通过", 400),
            ("InferHub.001001044.400 Message role cannot empty", 400),
            ("InferHub.002002009.404 model is not registered", 404),
            ("InferHub.4004.200 benefit not found", 404),
        ] {
            let (status, kind) = codearts_failure(200, text);
            assert_eq!(kind, ErrKind::None, "{text} 不得冷却账号");
            assert_eq!(status, want_status, "{text} 状态码");
        }
    }

    /// 凭据失效与上游 5xx 仍按原口径归因（换号 / 熔断），不得被上面两条吃掉
    #[test]
    fn credential_and_server_errors_keep_existing_semantics() {
        assert_eq!(
            codearts_failure(401, "APIG.0301 verify ak sk signature fail").1,
            ErrKind::SessionDead
        );
        assert_eq!(codearts_failure(200, "APIG.0203 Backend timeout").1, ErrKind::Server);
        assert_eq!(codearts_failure(503, "service unavailable").1, ErrKind::Server);
    }

    /// 会话 id 形状：32 位十六进制（对齐官方 UUIDv4 去连字符），且每次生成不重复
    #[test]
    fn session_id_shape_and_uniqueness() {
        let a = new_session_id();
        let b = new_session_id();
        assert_eq!(a.len(), 32, "会话 id 必须是 32 位十六进制: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "必须是十六进制: {a}");
        assert_ne!(a, b, "每次必须生成新会话 id（复用会互相注销名额）");
    }

    /// 心跳端点形态锚定：status 必须走 query（放 body 会被上游 400）
    #[test]
    fn heartbeat_url_puts_status_in_query() {
        assert_eq!(
            heartbeat_url("busy"),
            "https://snap-access.cn-north-4.myhuaweicloud.com/snap-manager/v1/chat-session/heartbeat?status=busy"
        );
        assert_eq!(
            heartbeat_url("idle"),
            "https://snap-access.cn-north-4.myhuaweicloud.com/snap-manager/v1/chat-session/heartbeat?status=idle"
        );
        // 路径不能带 snap-manager 之外的变体（参考实现踩过的坑）
        assert!(heartbeat_url("busy").contains("/snap-manager/v1/chat-session/heartbeat"));
    }

    /// 模型冷却：渐进退避 + 成功清除；且只在 Server 类错误时触发
    /// （并发上限/请求级错误不该冷却模型，否则误伤其他账号）
    #[test]
    fn model_cooldown_is_server_only_and_clearable() {
        let m = "__test_model_cooldown__";
        clear_model_failure(m);
        assert!(model_cooling_remaining_secs(m).is_none());

        note_model_failure(m);
        let first = model_cooling_remaining_secs(m).expect("首次失败应冷却");
        assert!((1..=10).contains(&first), "首次 10s: {first}");

        // 同片段内再次失败（窗口未过期）不递增窗口
        note_model_failure(m);
        let again = model_cooling_remaining_secs(m).unwrap();
        assert!(again <= first + 1, "同片段不递增: {first} -> {again}");

        clear_model_failure(m);
        assert!(model_cooling_remaining_secs(m).is_none(), "成功后应清除");
        // 不同模型互不干扰（失败域隔离的关键：与 WB 池表也不能串）
        note_model_failure("__other_model__");
        assert!(model_cooling_remaining_secs(m).is_none());
        clear_model_failure("__other_model__");
    }

    /// 会话头必须进签名集合：上游按 SignedHeaders 校验，头进请求但不进签名 → 401。
    /// 这里锚定「带 session_id 时 SignedHeaders 含 user-session-id 与 x-session-affinity」。
    #[test]
    fn session_headers_participate_in_signature() {
        let id = codearts_sign::CodeartsIdentity::new("AK_TEST123", "SK_TEST456");
        let url = chat_url();
        let body = br#"{"model":"m","messages":[]}"#;
        let sid = "0123456789abcdef0123456789abcdef";
        let aff = "fedcba9876543210fedcba9876543210";
        let signed = codearts_sign::sign_with_date(
            &id, "POST", &url, body,
            &[("user-session-id", sid), ("x-session-affinity", aff)],
            "20261119T120000Z",
        )
        .unwrap();
        assert!(
            signed.authorization.contains("user-session-id"),
            "会话 id 必须参与签名: {}", signed.authorization
        );
        assert!(
            signed.authorization.contains("x-session-affinity"),
            "会话粘性头必须参与签名: {}", signed.authorization
        );
        // extra 里必须仍有这两条（发送时逐条 set）
        assert!(signed.extra.iter().any(|(k, _)| k == "user-session-id"));
        assert!(signed.extra.iter().any(|(k, _)| k == "x-session-affinity"));
    }

    /// 亲和 id：**同一对话多轮稳定**（prompt cache 命中的前提），
    /// 而名额 id 每轮必须不同（名额按在途请求计数，不是按对话）。
    #[test]
    fn affinity_is_stable_per_conversation_but_slot_id_is_not() {
        // 显式 conversation_id 优先，且跨轮稳定（history 变了也不变）
        let c1 = json!({"conversation_id": "abc-123", "messages": [{"role":"user","content":"x"}]});
        let c1b = json!({"conversation_id": "abc-123", "messages": [{"role":"user","content":"y"}]});
        let a1 = affinity_of(&c1);
        assert_eq!(a1.len(), 32, "亲和 id 需为 32 位 hex: {a1}");
        assert!(a1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a1, affinity_of(&c1b), "同 conversation_id 必须稳定");

        // 无显式 id：按 messages 指纹，不同对话不得撞
        let conv2 = json!({"messages": [{"role":"user","content":"完全不同的提问"}]});
        assert_ne!(a1, affinity_of(&conv2), "不同对话应得到不同亲和 id");

        // 名额 id 每请求独立
        assert_ne!(new_session_id(), new_session_id(), "名额 id 每请求必须不同");
    }

    /// 不带会话 id 时签名集合不得凭空多出会话头（未登记请求不得伪装成已登记）
    #[test]
    fn no_session_headers_when_session_absent() {
        let id = codearts_sign::CodeartsIdentity::new("AK_TEST123", "SK_TEST456");
        let signed = codearts_sign::sign_with_date(
            &id, "POST", &chat_url(), br#"{"model":"m"}"#, &[], "20261119T120000Z",
        )
        .unwrap();
        assert!(!signed.authorization.contains("user-session-id"));
        assert!(!signed.authorization.contains("x-session-affinity"));
    }

    /// 上游令牌桶限流（`InferHub.ModelArts.81114.429`）与「并发会话数超限」
    /// 是两回事：前者按**令牌/分钟**速率重置，后者按**在途会话条数**释放。
    /// 两者都短冷却（SoftRate），但归因必须分开——此前混为一谈会把上游
    /// 令牌桶 429 报成「并发会话数已达上限」，排障方向被带偏。
    #[test]
    fn token_bucket_limit_is_distinct_from_concurrency_limit() {
        let bucket = "InferHub.ModelArts.81114.429 Too many requests, the rate limit is 300000000 tokens per minute.";
        // 令牌桶：命中限流分支，但不命中并发上限判定
        assert!(is_rate_limited(bucket), "令牌桶文案应命中限流判定");
        assert!(
            !is_concurrency_full(bucket),
            "令牌桶不得被误判为并发会话数超限"
        );
        // 归因：429 + 短冷却（不熔断账号）
        let (status, kind) = codearts_failure(200, bucket);
        assert_eq!(status, 429, "限流应回 429");
        assert_eq!(kind, ErrKind::SoftRate, "限流只短冷却，不熔断");

        // 并发上限：只命中并发判定，不命中令牌桶判定
        let conc = "TM.00001041 并发会话数已达上限(3个)，请关闭部分会话后重试。";
        assert!(is_concurrency_full(conc));
        assert!(
            !is_rate_limited(conc),
            "并发上限不应被误判为令牌桶限流"
        );
        assert_eq!(codearts_failure(200, conc), (429, ErrKind::SoftRate));

        // 无文案可判的裸 429：仍按限流短冷却（不落 4xx 兜底的不冷却口径，
        // 否则限流账号会被持续选中反复 429）
        assert_eq!(codearts_failure(429, ""), (429, ErrKind::SoftRate));
    }

    /// 令牌桶冷却时长按「到下一个自然分钟边界 + 3s 余量」计算：
    /// 固定 60s 可能跨过下一个窗口仍不放行（白等），也可能刚过窗口就
    /// 放行随即再 429（反复撞）。
    #[test]
    fn rate_limit_cooldown_aligns_to_next_minute_window() {
        let bucket = "the rate limit is 300000000 tokens per minute.";
        let secs = rate_limit_cooldown_secs(bucket);
        // 落点：下一个分钟边界 + 3s 余量 → 距今 4..=63s
        assert!(
            (4..=63).contains(&secs),
            "分钟桶冷却应落在 4..63s，实际 {secs}"
        );
        // 文案未声明分钟粒度 → 退回固定 60s
        assert_eq!(rate_limit_cooldown_secs("Too many requests"), 60);
    }
}

/// 真实链路验证（`cargo test -- --ignored --nocapture` 显式触发）。
///
/// 不依赖安装版 exe：直接读本机 vault 里的真实 AK/SK，走**修复后的**签名 +
/// 请求体构造 + `wb_sse` 解析链路打真实码道上游，断言错误帧被如实归因而
/// 不是退化成 -9901。图片用真 base64 载荷（默认 1×1 PNG，可用环境变量
/// `CODEARTS_LIVE_IMAGE` 指定图片路径替换）。
///
/// 凭据缺失（无 CodeArts 账号/换机/CI）时**跳过**而非失败——它不该污染常规回归。
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::tasks::codearts_common;
    use crate::tasks::codearts_sign;

    /// 生产 data 目录（安装版与 CLI 任务共用）
    fn real_data_dir() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("CODEARTS_LIVE_DATA_DIR") {
            return std::path::PathBuf::from(p);
        }
        std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default())
            .join("AIWorkAssistant")
    }

    /// 取池内第一个可用账号的 (uid, 凭据)。无账号/无 vault → None（跳过）
    fn first_account() -> Option<(String, codearts_common::CodeartsCreds)> {
        let data_dir = real_data_dir();
        // 注入 data_dir：让 codearts_catalog::resolve / max_tokens_of 读到
        // 真实目录缓存（否则 resolve 空表 → 404，测不出真实链路）
        crate::tasks::codearts_catalog::set_data_dir(&data_dir);
        let db = crate::store::db(&data_dir);
        let pool = crate::store::docs::codearts_pool_load(&db);
        let accounts = pool.get("accounts").and_then(Value::as_array)?;
        for a in accounts {
            let Some(uid) = a.get("id").and_then(Value::as_str) else {
                continue;
            };
            match codearts_common::ensure_fresh_creds(&data_dir, uid) {
                Ok(creds) if creds.is_valid() => return Some((uid.to_string(), creds)),
                Ok(_) => {}
                Err(e) => eprintln!("[skip] uid={uid} 凭据不可用: {e}"),
            }
        }
        None
    }

    /// 真实图片 base64：默认 1×1 PNG（最小合规载荷），环境变量可替换成
    /// 真实截图，验证多模态整条链路（大图是最容易触发上游帧内错误的场景）
    fn image_b64() -> String {
        use base64::Engine as _;
        if let Ok(path) = std::env::var("CODEARTS_LIVE_IMAGE") {
            if let Ok(bytes) = std::fs::read(&path) {
                return base64::engine::general_purpose::STANDARD.encode(&bytes);
            }
        }
        // 1×1 透明 PNG
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
            .to_string()
    }

    /// 真实上游调用：用修复后的 `/api/v2/chat/completions` 请求构造 + 签名 +
    /// 帧解析链路，返回 (是否拿到真实正文, 归因结果, 原始首帧样本)
    fn call_live(model: &str, body: &Value) -> (bool, String, String) {
        let Some((uid, creds)) = first_account() else {
            panic!("本机无 CodeArts 凭据（应在上层 skip）");
        };
        let identity = codearts_sign::identity_from_creds(&creds);
        // 修复后的构造：强制 stream + 模型名对齐 + max_tokens 夹取
        let prepared = rebuild_body(body, model, model);
        let affinity = affinity_of(body);
        // 修复后的会话名额登记：busy → 推理（带同一 id）→ idle
        let slot = SessionSlot::acquire(&identity, &real_data_dir());
        let session_id = slot.as_ref().map(|s| s.id());
        let reader = match send_signed_request(
            &codearts_agent(), &identity, &prepared, session_id, Some(&affinity),
        ) {
            Ok(r) => r,
            Err((status, text)) => {
                // HTTP 层错误同样走修复后的归因（不得误熔断账号）
                let (client_status, kind) = codearts_failure(status, &text);
                return (
                    false,
                    format!("HTTP {status} → 对客户端 {client_status} / 账号处置 {kind:?} / {text}"),
                    text.chars().take(200).collect(),
                );
            }
        };
        let lines = match wb_upstream::lines_with_first_byte_timeout_interruptible(reader) {
            Ok(l) => l,
            Err(()) => return (false, "首字超时（10s）".into(), String::new()),
        };
        // 非流式聚合：与生产 aggregate 同一条修复后的解析路径
        let (resp, err) = wb_sse::aggregate(lines, "live-chat", None);
        match (resp, err) {
            (Some(r), None) => {
                let text = r
                    .pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let reasoning = r
                    .pointer("/choices/0/message/reasoning_content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let sample = format!(
                    "content={:?} reasoning_len={} usage={}",
                    text.chars().take(80).collect::<String>(),
                    reasoning.chars().count(),
                    r.get("usage").map(|u| u.to_string()).unwrap_or_default()
                );
                (!text.is_empty() || !reasoning.is_empty(), format!("uid={uid} 正常完成"), sample)
            }
            (_, Some((code, msg))) => {
                // 关键断言点：带 choices 之外的错误帧必须给出真实归因
                let (client_status, kind) = codearts_failure(0, &format!("{code} {msg}"));
                let verdict = format!(
                    "帧内错误 → 对客户端 {client_status} / 账号处置 {kind:?} / 原文 {msg}"
                );
                (false, verdict, msg.chars().take(200).collect())
            }
            (None, None) => (false, "零内容零错误（空完成哨兵路径）".into(), String::new()),
        }
    }

    /// 真实图片输入：走修复后的完整链路，断言「要么拿到真实正文，要么拿到
    /// 真实错误归因」，绝不出现 -9901 空完成伪装。这是用户报障的原始场景。
    #[test]
    #[ignore = "真实上游调用：cargo test -- --ignored --nocapture"]
    fn live_image_request_never_degrades_to_empty_completion() {
        if first_account().is_none() {
            eprintln!("[skip] 本机无可用 CodeArts 凭据");
            return;
        }
        let model = std::env::var("CODEARTS_LIVE_MODEL")
            .unwrap_or_else(|_| "deepseek-v4.1-flash".to_string());
        let body = json!({
            "stream": false,
            "max_tokens": 128000,   // 故意超模型上限：验证夹取生效（否则 001001005）
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "用一句话描述这张图片。"},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", image_b64())}}
                ]
            }]
        });
        let (ok, verdict, sample) = call_live(&model, &body);
        eprintln!("[live image] model={model} ok={ok} verdict={verdict}\n  sample={sample}");
        assert!(
            !verdict.contains("-9901") && !verdict.contains("empty completion"),
            "不得退化成空完成哨兵：{verdict}"
        );
        // 允许上游侧真实失败（并发上限/模型抖动），但归因必须是真实的
        assert!(
            ok || verdict.contains("429") || verdict.contains("Error") || verdict.contains("HTTP"),
            "失败时必须带真实归因，实际：{verdict}"
        );
    }

    /// 只读诊断：不触发刷新，打印 vault 里凭据的到期/续期材料状态（不打印密钥）
    #[test]
    #[ignore = "只读诊断：cargo test -- --ignored --nocapture codearts_cred_state"]
    fn codearts_cred_state() {
        let data_dir = real_data_dir();
        let db = crate::store::db(&data_dir);
        let pool = crate::store::docs::codearts_pool_load(&db);
        let accounts = pool.get("accounts").and_then(Value::as_array).cloned().unwrap_or_default();
        eprintln!("[state] accounts={}", accounts.len());
        for a in &accounts {
            let uid = a.get("id").and_then(Value::as_str).unwrap_or("");
            let creds = codearts_common::effective_creds(&data_dir, uid);
            match creds {
                Ok(c) => {
                    let now = chrono::Local::now().timestamp();
                    let left = c.expires_at - now;
                    eprintln!(
                        "[state] uid={uid} valid={} oauth={} ak_len={} sk_len={} st_len={} \
                         has_refresh={} has_verifier={} has_dpop={} expires_at={} left_secs={} needs_refresh_30m={}",
                        c.is_valid(), c.is_oauth(), c.ak.len(), c.sk.len(),
                        c.security_token.len(), !c.refresh_token.is_empty(),
                        !c.code_verifier.is_empty(), !c.dpop_key_pem.is_empty(),
                        c.expires_at, left, c.needs_refresh(30 * 60)
                    );
                }
                Err(e) => eprintln!("[state] uid={uid} effective_creds Err: {e}"),
            }
        }
    }

    /// 真实复现用户报障：客户端带空 role 消息时，修复前上游直接报
    /// `InferHub.001001044.400 Message role cannot empty`。这里用真实凭据 +
    /// 真实上游验证 rebuild_body 的 role 规范化确实让请求成功。
    #[test]
    #[ignore = "真实上游调用：cargo test -- --ignored --nocapture"]
    fn live_empty_role_request_succeeds() {
        if first_account().is_none() {
            eprintln!("[skip] 本机无可用 CodeArts 凭据");
            return;
        }
        let model = std::env::var("CODEARTS_LIVE_MODEL")
            .unwrap_or_else(|_| "deepseek-v4.1-flash".to_string());
        // 故意构造用户报障时的消息形状：含空 role / 缺 role / developer 大写
        let body = json!({
            "stream": false,
            "max_tokens": 100,
            "messages": [
                {"role": "", "content": "空 role 消息（修复前会 001001044）"},
                {"content": "缺 role 消息"},
                {"role": "DEVELOPER", "content": "developer 会被转成 system"},
                {"role": "user", "content": "请只回复 OK"}
            ]
        });
        let (ok, verdict, sample) = call_live(&model, &body);
        eprintln!("[live empty-role] ok={ok} {verdict}\n  {sample}");
        assert!(ok, "role 规范化后请求必须成功：{verdict} / {sample}");
        assert!(!verdict.contains("001001044"), "不得再报空 role：{verdict}");
        assert!(!verdict.contains("-9901"), "不得退化成空完成：{verdict}");
    }

    /// 用户报障的原始场景：**同一个对话里连续发起的多个串行请求**
    /// 被上游各自当成新会话，第 2 个起必撞 TM.00001041（并发会话数已达上限），
    /// 而实际上是串行的、根本没并发。
    ///
    /// 修复后：每个请求登记会话名额并带同一 user-session-id，收尾 idle 注销，
    /// 所以串行请求必须全部成功。这是本 bug 的**决定性验证**。
    #[test]
    #[ignore = "真实上游调用：cargo test -- --ignored --nocapture"]
    fn live_serial_requests_do_not_exhaust_session_quota() {        if first_account().is_none() {
            eprintln!("[skip] 本机无可用 CodeArts 凭据");
            return;
        }
        let model = std::env::var("CODEARTS_LIVE_MODEL")
            .unwrap_or_else(|_| "deepseek-v4.1-flash".to_string());
        let mut ok_count = 0;
        let mut limit_hits = 0;
        let mut verdicts = Vec::new();
        // 串行 5 次（严格一个一个来，绝不并发）——修复前第 2 次就会撞上限
        for i in 0..5 {
            let body = json!({
                "stream": false,
                "max_tokens": 200,
                "messages": [{"role": "user", "content": format!("只回复数字 {i}，不要别的内容。")}]
            });
            let (ok, verdict, sample) = call_live(&model, &body);
            eprintln!("[serial #{i}] ok={ok} {verdict}\n  {sample}");
            if ok {
                ok_count += 1;
            }
            if verdict.contains("TM.00001041") || verdict.contains("并发会话") {
                limit_hits += 1;
            }
            verdicts.push(verdict);
        }
        eprintln!("[serial] ok={ok_count}/5 limit_hits={limit_hits}");
        assert_eq!(
            limit_hits, 0,
            "串行请求不得撞并发上限（会话名额未归还）：{verdicts:?}"
        );
        assert_eq!(ok_count, 5, "5 次串行请求必须全部成功：{verdicts:?}");
    }

    /// 并发会话上限（TM.00001041）必须被归因为 429 + SoftRate，
    /// **不得**触发账号 30 分钟熔断。用并发压测主动撞该限制。
    #[test]
    #[ignore = "真实上游调用：cargo test -- --ignored --nocapture"]
    fn live_concurrency_limit_is_classified_as_rate_limit() {
        if first_account().is_none() {
            eprintln!("[skip] 本机无可用 CodeArts 凭据");
            return;
        }
        let model = std::env::var("CODEARTS_LIVE_MODEL")
            .unwrap_or_else(|_| "deepseek-v4.1-flash".to_string());
        let body = json!({
            "stream": false,
            "messages": [{"role": "user", "content": "请写一段 300 字的技术说明。"}]
        });
        let mut saw_limit = false;
        let mut saw_success = false;
        let mut verdicts = Vec::new();
        // 连打 6 次：单账号只有 3 个并发会话名额，必然撞上限
        for i in 0..6 {
            let (ok, verdict, _) = call_live(&model, &body);
            eprintln!("[live #{i}] ok={ok} {verdict}");
            if ok {
                saw_success = true;
            }
            if verdict.contains("TM.00001041") || verdict.contains("429") {
                saw_limit = true;
                // 核心断言：上限必须归为瞬时限流，不得判成账号故障
                assert!(
                    verdict.contains("SoftRate"),
                    "并发上限必须归 SoftRate（短冷却），实际：{verdict}"
                );
                assert!(
                    !verdict.contains("Server"),
                    "并发上限不得归 Server（会熔断 30 分钟打死整池），实际：{verdict}"
                );
            }
            verdicts.push(verdict);
        }
        eprintln!("[live] saw_success={saw_success} saw_limit={saw_limit}");
        // 至少要有一次真实结果（成功或真实错误归因），不能全是空完成
        assert!(
            saw_success || saw_limit,
            "6 次调用既无成功也未撞上限，链路可疑：{verdicts:?}"
        );
        for v in &verdicts {
            assert!(!v.contains("-9901"), "出现空完成伪装：{v}");
        }
    }

    /// 上游令牌桶限流（`InferHub.ModelArts.81114.429 Too many requests, the
    /// rate limit is N tokens per minute`）的真实链路归因验证。
    ///
    /// 修复前：该文案被 `is_concurrency_full` 吞成「并发会话数已达上限」，
    /// 且令牌桶是**服务商级**的（同一账号体系的账号会同时撞），两个账号一起
    /// 被 60s 短冷却 → 池内无可选账号 → 网关回 503「无健康账号」——
    /// 瞬时限流被报成网关故障。
    ///
    /// 修复后：归因为 429 + SoftRate（与并发上限分轨），冷却时长按分钟窗口
    /// 计算；池全冷时回 429 + 退避秒数而非 503。
    #[test]
    #[ignore = "真实上游调用：cargo test -- --ignored --nocapture"]
    fn live_token_bucket_limit_is_not_reported_as_no_healthy_account() {
        if first_account().is_none() {
            eprintln!("[skip] 本机无可用 CodeArts 凭据");
            return;
        }
        let model = std::env::var("CODEARTS_LIVE_MODEL")
            .unwrap_or_else(|_| "deepseek-v4.1-flash".to_string());
        // 反复打大请求，主动撞上游令牌桶（每分钟令牌上限）
        let body = json!({
            "stream": false,
            "max_tokens": 60000,
            "messages": [{"role": "user", "content": "请写一篇 3000 字的技术长文。"}]
        });
        let mut verdicts = Vec::new();
        for i in 0..4 {
            let (ok, verdict, sample) = call_live(&model, &body);
            eprintln!("[bucket #{i}] ok={ok} {verdict}\n  {sample}");
            verdicts.push(verdict.clone());
            if verdict.contains("ModelArts.811") || verdict.contains("rate limit") {
                // 撞到令牌桶：核心断言——不得被判成「并发会话数已达上限」
                // （文案与归因码都不同），且必须是 429 + SoftRate
                assert!(
                    !verdict.contains("TM.00001041") && !verdict.contains("并发会话"),
                    "令牌桶限流不得被误报为并发会话数超限：{verdict}"
                );
                assert!(
                    verdict.contains("429"),
                    "令牌桶限流应回 429：{verdict}"
                );
                assert!(
                    verdict.contains("SoftRate"),
                    "令牌桶限流应归 SoftRate（短冷却）：{verdict}"
                );
                assert!(
                    !verdict.contains("Server"),
                    "令牌桶限流不得归 Server（会熔断 30 分钟打死整池）：{verdict}"
                );
                return;
            }
        }
        eprintln!("[bucket] 本轮未撞上令牌桶（上游窗口未满），归因链静态断言已由单测覆盖");
        // 未撞上限也算通过：不能出现空完成伪装或 503 误报
        for v in &verdicts {
            assert!(!v.contains("-9901"), "出现空完成伪装：{v}");
            assert!(
                !v.contains("no healthy account"),
                "限流场景不得报「无健康账号」：{v}"
            );
        }
    }
}
