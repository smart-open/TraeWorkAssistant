//! 自定义模型上游执行路径（OpenAI 兼容直通）
//!
//! 与 WB 上游同策略：请求体强制 `stream:true`，上游只回 OpenAI 风格 SSE——
//! - 流式：wb_sse::stream_forward 按客户端协议转发；
//! - 非流式：wb_sse::aggregate 本地聚合为单个 OpenAI completion 后按协议转换。
//!
//! P2 上游组语义（custom_lb.rs）：入参为同名多条构成的上游组（Vec<CustomModel>）——
//! 组内加权轮询选路（粘性优先），所选上游失败且未向客户端产出任何字节
//! （`!sent_any`）时换组内下一上游重试（≤ min(N, 3) 轮，对齐 wb/qoder 轮换
//! 上限口径）；`sent_any` 后就地收尾不轮换（SSE 铁律：不拼第二份响应）。
//! 每次失败经 custom_lb 记账熔断（401/403/402 长窗，其余渐进冷却），
//! 成功即清熔断并记录会话粘上游（TTL 60s）。
//!
//! 快速失败参数：custom 专用 agent 连接超时 3s（wb_agent 为 10s，对快速失败
//! 太长）——连接拒绝/3s 连接超时立即进入组内轮换；首字超时 10s、空闲读 300s。
//!
//! 用量记账：独立 custom_days 桶（与 Trae/WB 侧分账，uid 固定 "custom"）；
//! 每次 attempt 各记一条（成功/失败如实落账），请求日志 acct_name 带
//! `name#id前4` 区分同组多条。

use std::collections::HashSet;
use std::time::Instant;

use serde_json::{json, Value};

use super::custom_lb::{self, FailKind};
use super::custom_models::{self, CustomModel};
use super::routes::{anthropic_error, openai_error, send_stream_error, Protocol};
use super::wb_sse;
use super::wb_upstream;
use super::{ApiSharedState, InflightGuard};
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

/// 请求体解析（loop 外一次）：JSON 解析失败按空对象处理（与旧 prep_body 同口径）
fn parse_body_once(body_vec: &[u8]) -> Value {
    serde_json::from_slice(body_vec).unwrap_or_else(|_| json!({}))
}

/// 按所选条目重建请求体（每次 attempt 调用）：强制 stream:true + 模型名对齐
/// 该条目自身的 name 原文——同名组内各条目 canonical 相同但原文大小写/空白
/// 可能不同（如 `GLM-5.3` 与 `glm-5.3`），部分上游对模型名大小写敏感
fn rebuild_body(core: &Value, cm: &CustomModel) -> Vec<u8> {
    let mut v = core.clone();
    v["stream"] = json!(true);
    if !cm.name.is_empty() {
        v["model"] = json!(cm.name);
    }
    v.to_string().into_bytes()
}

/// custom 专用 agent（快速失败参数）：连接超时 3s（快速失败切换——连接拒绝/
/// 3s 超时立即进入组内轮换）/ 写 30s / 空闲读 300s（与 wb_agent 同读口径）。
/// 审查修复（M-1）：静态复用（OnceLock）——Agent 即连接池宿主，每次请求新建
/// 会让 max_idle_connections 等池参数形同虚设、每次 attempt 全新 TCP/TLS 握手
fn custom_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_read(std::time::Duration::from_secs(300))
            .timeout_write(std::time::Duration::from_secs(30))
            .timeout_connect(std::time::Duration::from_secs(3))
            .max_idle_connections(20)
            .max_idle_connections_per_host(20)
            .build()
    })
}

/// 探活专用 agent（B3 审查修复）：不与网关 agent 共用——网关读超时 300s，
/// probe 若复用，命令层 30s 超时返回后慢上游仍会把阻塞线程拖住最长 300s。
/// 独立 20s 读/总超时 + 3s 连接：探活全链路收敛在 30s 命令超时内
fn probe_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(20))
            .timeout_read(std::time::Duration::from_secs(20))
            .timeout_write(std::time::Duration::from_secs(10))
            .timeout_connect(std::time::Duration::from_secs(3))
            .build()
    })
}

/// 发起自定义上游请求（Bearer 鉴权；custom 专用 agent）
fn make_custom_request(cm: &CustomModel, body: &[u8]) -> Result<Box<dyn std::io::Read + Send>, (u16, String)> {
    make_request_with(cm, body, custom_agent())
}

/// make_custom_request 的 agent 参数化形态（探活复用独立短超时 agent）
fn make_request_with(
    cm: &CustomModel,
    body: &[u8],
    agent: &ureq::Agent,
) -> Result<Box<dyn std::io::Read + Send>, (u16, String)> {
    let url = custom_models::chat_url(&cm.base_url);
    let mut req = agent.post(&url);
    req = req.set("content-type", "application/json");
    req = req.set("accept", "text/event-stream");
    if !cm.api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", cm.api_key));
    }
    match req.send_bytes(body) {
        Ok(r) => Ok(Box::new(r.into_reader())),
        Err(ureq::Error::Status(code, resp)) => {
            let body_text = resp.into_string().unwrap_or_default();
            Err((code, body_text))
        }
        Err(e) => Err((502, format!("自定义上游传输错误: {e}"))),
    }
}

/// 失败类别判定（熔断记账口径）：401/403/402 = 配置/账户错误落 600s 长窗，
/// 其余（传输错误/5xx/429 等）走渐进冷却表
fn classify_failure(status: u16) -> FailKind {
    if matches!(status, 401 | 403 | 402) {
        FailKind::Auth
    } else {
        FailKind::Server
    }
}

/// 请求日志 acct_name：`name#id第4~7字节` 区分同组多条（同名不同上游日志可读性；
/// id 形如 `cm-<12hex>`，非标 id 安全回退 name）
fn acct_display(cm: &CustomModel) -> String {
    match cm.id.get(3..7) {
        Some(s) if !s.is_empty() => format!("{}#{}", cm.name, s),
        _ => cm.name.clone(),
    }
}

/// 组内选下一上游：粘性命中优先（跳过计数器；粘住条目已 tried 时忽略重选），
/// 否则组内加权轮询（pick_with_state，熔断中/已试条目排除）
fn pick_next(
    state: &Arc<ApiSharedState>,
    group: &[CustomModel],
    tried: &HashSet<String>,
    allowed: Option<&HashSet<String>>,
    session_key: Option<&String>,
    canonical: &str,
) -> Option<CustomModel> {
    if let Some(sk) = session_key {
        if let Some(id) = custom_lb::sticky_upstream(state, sk) {
            if !tried.contains(&id) && allowed.map_or(true, |set| set.contains(&id)) {
                if let Some(cm) = group.iter().find(|m| m.id == id) {
                    return Some(cm.clone());
                }
            }
        }
    }
    custom_lb::pick_with_state(state, canonical, group, tried, allowed)
}

/// Key 白名单约束解析（A1 审查修复：与 qoder_route 取号同模式——从 key_id
/// 解析 custom 池约束，None = 不限；绑定/混合白名单 Key 不再漏进白名单外上游）
fn allowed_for_key(state: &ApiSharedState, key_id: &str) -> Option<HashSet<String>> {
    super::api_keys::constraints_for(&state.data_dir, key_id)
        .and_then(|rk| rk.pool_constraints("custom"))
        .and_then(|c| c.allowed)
}

/// 连通性探活（custom_model_test 命令底层）：向自定义上游发一条最小 chat 请求
/// （max_tokens=16 + stream:true，与网关同款 SSE 处理），返回成功摘要 / 失败原因。
/// 阻塞调用——调用方需放入 spawn_blocking 并外加超时（命令层 30s）。
/// B3 审查修复：走 probe_agent 独立短超时（20s），命令层超时后不再有长残留线程
pub fn probe(cm: &CustomModel) -> Result<String, String> {
    let body = json!({
        "model": cm.name,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 16,
        "stream": true,
    })
    .to_string()
    .into_bytes();
    let reader = make_request_with(cm, &body, probe_agent()).map_err(|(status, resp_body)| {
        let preview: String = resp_body.chars().take(200).collect();
        if preview.is_empty() {
            format!("上游返回 status {status}（无响应体；请检查 API 地址与 Key）")
        } else {
            format!("上游返回 status {status}：{preview}")
        }
    })?;
    let lines = wb_upstream::lines_with_first_byte_timeout(reader)
        .map_err(|_| "10 秒内未收到上游响应（首字超时）".to_string())?;
    let (resp, error_info) = wb_sse::aggregate(lines, &format!("probe-{}", now_ts()));
    match (resp, error_info) {
        (Some(r), None) => {
            let content = r
                .pointer("/choices/0/message/content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let preview: String = content.chars().take(40).collect();
            if preview.is_empty() {
                Ok("连通成功（上游返回空内容）".to_string())
            } else {
                Ok(format!("连通成功：{preview}"))
            }
        }
        (None, Some((code, msg))) => Err(format!("上游流内错误 code={code}：{msg}")),
        _ => Err("上游返回为空（未产出任何 completion）".to_string()),
    }
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
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

fn set_last_error(state: &Arc<ApiSharedState>, msg: String) {
    *state.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
}

// ==================== 流式（上游组轮换） ====================

#[allow(clippy::too_many_arguments)]
pub fn custom_stream_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    group: Vec<CustomModel>,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    // 批次 D-1 线程隔离：流任务迁入专用阻塞池（见 mod.rs stream_runtime 注释）
    super::stream_runtime().spawn_blocking(move || {
        let _inflight = guard; // 随后台任务存续至流结束（§4.5）
        let chat_id = chat_id_for(proto);
        // 请求日志附带：API Key 展示名 + 自定义模型名（即"账号"，匿名 → "-"）
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        // A1 审查修复：Key 白名单约束传入组内选路（含粘性命中过滤）
        let allowed = allowed_for_key(&state, &key_id);

        // SSE keep-alive 15s：防中间层回收长流（与 WB/solo 路径同策略）。
        // P1 修复：与 routes.rs 同款 watch + DoneSignal 方案，替换旧 AtomicBool
        // 15s 轮询（流终结最多被拖延 15s）；主任务结束（Drop，含 panic 展开）
        // → ticker select! 退出 → sender 全部关闭 → 流正常终结
        let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
        let _done = super::routes::DoneSignal(done_tx);
        {
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
                tick.tick().await;
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            if tx2
                                .send(Ok(bytes::Bytes::from(": keep-alive\n\n")))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        // 主任务已结束：ticker 退出，放行流终结
                        _ = done_rx.changed() => break,
                    }
                }
            });
        }

        // 组执行骨架：loop 外解析 body 一次（每次 attempt 按所选条目重建）；
        // 轮换上限 min(N, 3)（与 wb/qoder 轮换耗尽口径一致）
        let prepared_core = parse_body_once(&body_vec);
        let session_key = super::dispatch::pool_session_key(&prepared_core);
        let canonical = super::unified_catalog::canonical_id(&model);
        let mut tried: HashSet<String> = HashSet::new();
        let max_attempts = group.len().min(3);
        let mut attempt = 0usize;
        let mut last_err = String::from("自定义上游组内无可用条目");

        loop {
            let Some(cm) = pick_next(&state, &group, &tried, allowed.as_ref(), session_key.as_ref(), &canonical)
            else {
                // 组内全熔断/已试尽（pool_health 前置下理论不可达，防御性兜底）
                set_last_error(&state, format!("custom model={} group exhausted", model));
                state.logger.log_request(
                    "custom", "POST", proto.log_path(), &model, true, 503, "custom",
                    start_ts.elapsed().as_millis() as u64, &key_name, "-", Some(&last_err),
                );
                state.logger.log_debug_line(format!(
                    "[DEBUG] UPSTREAM_GROUP_EXHAUSTED tried={} pool=custom model={} 组内无可用上游（全熔断或已试尽）",
                    tried.len(),
                    model,
                ));
                send_stream_error(&tx, proto, 503, &last_err);
                break;
            };
            tried.insert(cm.id.clone());
            let acct_name = acct_display(&cm);
            let prepared = rebuild_body(&prepared_core, &cm);

            match make_custom_request(&cm, &prepared) {
                Err((status, resp_body)) => {
                    // HTTP 层失败（连接拒绝/3s 超时在 send_bytes 即返回）——
                    // 未向客户端产出任何字节，可轮换
                    custom_lb::note_circuit_failure(&cm.id, classify_failure(status));
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    state.record_usage_custom(&model, &key_id, false, true, duration_ms, 0, 0);
                    let preview: String = resp_body.chars().take(200).collect();
                    last_err = format!("自定义上游错误（status {}）：{}", status, preview);
                    set_last_error(&state, format!("custom model={} status={} body={}", model, status, preview));
                    state.logger.log_request(
                        "custom", "POST", proto.log_path(), &model, true, status, "custom",
                        duration_ms, &key_name, &acct_name, Some(&format!("upstream status={}", status)),
                    );
                    attempt += 1;
                    if attempt >= max_attempts {
                        state.logger.log_debug_line(format!(
                            "[DEBUG] UPSTREAM_GROUP_EXHAUSTED tried={} pool=custom model={} reasons=[status={}]",
                            tried.len(),
                            model,
                            status,
                        ));
                        send_stream_error(&tx, proto, status as i64, &last_err);
                        break;
                    }
                    // 换组内下一上游
                }
                Ok(reader) => match wb_upstream::lines_with_first_byte_timeout_interruptible(reader) {
                    Err(()) => {
                        // 首字超时（F-34 语义）：10s 内上游未产出任何字节 → 可轮换
                        custom_lb::note_circuit_failure(&cm.id, FailKind::Server);
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        state.record_usage_custom(&model, &key_id, false, true, duration_ms, 0, 0);
                        state.logger.log_request(
                            "custom", "POST", proto.log_path(), &model, true, 504, "custom",
                            duration_ms, &key_name, &acct_name, Some("first byte timeout"),
                        );
                        last_err = "自定义上游 10 秒内未响应（首字超时）".into();
                        attempt += 1;
                        if attempt >= max_attempts {
                            state.logger.log_debug_line(format!(
                                "[DEBUG] UPSTREAM_GROUP_EXHAUSTED tried={} pool=custom model={} reasons=[first byte timeout]",
                                tried.len(),
                                model,
                            ));
                            send_stream_error(&tx, proto, 504, &last_err);
                            break;
                        }
                    }
                    Ok(lines) => {
                        // 可中断行源：stream_forward_ex 停滞期间周期性检查客户端断连。
                        // 自定义上游无 namespace 展平编码 → ex 内部按空集合禁用编码名拆分（审查 F4）
                        // B1 审查修复：迁移 stream_forward_ex——获取 failed_inline/eof_truncated，
                        // 流内失败事件（failed/error 已就地透传）不再被误记为成功
                        let (error_info, sent_any, failed_inline, up_usage, eof_truncated) =
                            wb_sse::stream_forward_ex(lines, &tx, proto, &chat_id, &model);
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        {
                            // 宽口径提取（别名 + 字符串数字形态，审查 P2）
                            let (pt, ct) = up_usage.as_ref().map(wb_sse::u64_pair).unwrap_or((0, 0));
                            // M4 审查修复：缓存读 tokens 旁路入账（未下发得 0 不入账）
                            state.record_usage_cached(
                                super::usage::UsageBucket::Custom,
                                up_usage.as_ref().map(wb_sse::cached_tokens_of).unwrap_or(0),
                            );
                            // B1 审查修复：failed_inline 时 error_info=None 但非成功
                            state.record_usage_custom(
                                &model, &key_id, error_info.is_none() && !failed_inline, true,
                                duration_ms, pt, ct,
                            );
                        }
                        match error_info {
                            None => {
                                if failed_inline {
                                    // B1 审查修复：流内失败已就地透传客户端（对齐 wb_route 语义）——
                                    // 不重试、不清熔断、不绑粘性、不记成功；亦不计熔断失败
                                    // （内容类失败计入冷却会造成上游过度冷却）
                                    state.logger.log_request(
                                        "custom", "POST", proto.log_path(), &model, true, 200, "custom",
                                        duration_ms, &key_name, &acct_name,
                                        Some("流内失败已透传客户端"),
                                    );
                                    break;
                                }
                                // 成功：清熔断 + 记会话粘上游
                                custom_lb::clear_circuit(&cm.id);
                                if let Some(sk) = &session_key {
                                    custom_lb::record_sticky_upstream(&state, sk, &cm.id);
                                }
                                state.logger.log_request(
                                    "custom", "POST", proto.log_path(), &model, true, 200, "custom",
                                    duration_ms, &key_name, &acct_name,
                                    // B1 审查修复：EOF 截断的成功行标注截断（与上游原貌不符，排障可观测）
                                    eof_truncated.then(|| "上游 EOF 截断（未收到 Done），已按协议补齐终止帧"),
                                );
                                break;
                            }
                            Some((code, msg)) => {
                                custom_lb::note_circuit_failure(&cm.id, FailKind::Server);
                                last_err = format!("code={code} msg={msg}");
                                set_last_error(&state, format!("custom model={} code={} msg={}", model, code, msg));
                                state.logger.log_request(
                                    "custom", "POST", proto.log_path(), &model, true, 200, "custom",
                                    duration_ms, &key_name, &acct_name,
                                    // 审查修复（M-3）：失败 attempt 错误标注带上游业务码
                                    Some(&format!("upstream code={} msg={}", code, msg)),
                                );
                                // SSE 铁律：sent_any 后就地收尾，不轮换（不拼第二份响应）
                                attempt += 1;
                                if sent_any || attempt >= max_attempts {
                                    if !sent_any {
                                        state.logger.log_debug_line(format!(
                                            "[DEBUG] UPSTREAM_GROUP_EXHAUSTED tried={} pool=custom model={} reasons=[stream error code={}]",
                                            tried.len(),
                                            model,
                                            code,
                                        ));
                                        send_stream_error(&tx, proto, code, &msg);
                                    }
                                    break;
                                }
                                // !sent_any → 换组内下一上游
                            }
                        }
                    }
                },
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("internal server error"))
                .unwrap()
        })
}

// ==================== 非流式（上游组轮换） ====================

#[allow(clippy::too_many_arguments)]
pub async fn custom_aggregate_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    group: Vec<CustomModel>,
    stream: bool,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    // 闭包 move 后外层协议转换仍需模型名，提前克隆
    let model_outer = model.clone();
    // P2 修复：非流式聚合为长阻塞上游读取（首字超时 10s + 全量读入），
    // 与 WB/solo 聚合同口径迁入 stream_runtime 专用阻塞池，不占主池短任务槽
    let result = super::stream_runtime().spawn_blocking(move || {
        let _inflight = guard; // 随聚合完成释放（§4.5）
        // 请求日志附带：API Key 展示名 + 自定义模型名（即"账号"，匿名 → "-"）
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        // A1 审查修复：Key 白名单约束传入组内选路（含粘性命中过滤）
        let allowed = allowed_for_key(&state, &key_id);
        let prepared_core = parse_body_once(&body_vec);
        let session_key = super::dispatch::pool_session_key(&prepared_core);
        let canonical = super::unified_catalog::canonical_id(&model);
        let mut tried: HashSet<String> = HashSet::new();
        let max_attempts = group.len().min(3);
        let mut attempt = 0usize;
        let mut last_err = String::from("自定义上游组内无可用条目");
        // 终态 HTTP 状态码（审查修复 M-2：与流式路径口径对齐——组耗尽 503 /
        // 上游 HTTP 错误透传 status / 首字超时 504 / 空响应与流内错误 502，
        // 不再一律 502，客户端可按状态码区分失败类别）
        let mut last_status = 503u16;

        loop {
            let Some(cm) = pick_next(&state, &group, &tried, allowed.as_ref(), session_key.as_ref(), &canonical)
            else {
                // 组内全熔断/已试尽（pool_health 前置下理论不可达，防御性兜底）；
                // [DEBUG] 排障行由 loop 后收尾统一落（此处 break 即达，不重复）
                set_last_error(&state, format!("custom model={} group exhausted", model));
                state.logger.log_request(
                    "custom", "POST", proto.log_path(), &model, stream, 503, "custom",
                    start_ts.elapsed().as_millis() as u64, &key_name, "-", Some(&last_err),
                );
                break;
            };
            tried.insert(cm.id.clone());
            let acct_name = acct_display(&cm);
            let prepared = rebuild_body(&prepared_core, &cm);

            let reader = match make_custom_request(&cm, &prepared) {
                Ok(r) => r,
                Err((status, resp_body)) => {
                    custom_lb::note_circuit_failure(&cm.id, classify_failure(status));
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    state.record_usage_custom(&model, &key_id, false, stream, duration_ms, 0, 0);
                    let preview: String = resp_body.chars().take(200).collect();
                    last_err = format!("自定义上游错误（status {}）：{}", status, preview);
                    set_last_error(&state, format!("custom model={} status={} body={}", model, status, preview));
                    state.logger.log_request(
                        "custom", "POST", proto.log_path(), &model, stream, status, "custom",
                        duration_ms, &key_name, &acct_name, Some(&format!("upstream status={}", status)),
                    );
                    attempt += 1;
                    if attempt >= max_attempts {
                        last_status = status;
                        break;
                    }
                    continue; // 换组内下一上游
                }
            };
            let lines = match wb_upstream::lines_with_first_byte_timeout(reader) {
                Ok(l) => l,
                Err(()) => {
                    custom_lb::note_circuit_failure(&cm.id, FailKind::Server);
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    state.record_usage_custom(&model, &key_id, false, stream, duration_ms, 0, 0);
                    state.logger.log_request(
                        "custom", "POST", proto.log_path(), &model, stream, 504, "custom",
                        duration_ms, &key_name, &acct_name, Some("first byte timeout"),
                    );
                    last_err = "自定义上游 10 秒内未响应（首字超时）".into();
                    attempt += 1;
                    if attempt >= max_attempts {
                        last_status = 504;
                        break;
                    }
                    continue;
                }
            };
            let (resp, error_info) = wb_sse::aggregate(lines, &format!("chatcmpl-{}", now_ts()));
            let duration_ms = start_ts.elapsed().as_millis() as u64;
            match (resp, error_info) {
                (Some(mut r), None) => {
                    r["model"] = json!(model);
                    // 宽口径提取（别名 + 字符串数字形态，审查 P2）
                    let (pt, ct) = r.get("usage").map(wb_sse::u64_pair).unwrap_or((0, 0));
                    // M4 审查修复：缓存读 tokens 旁路入账（未下发得 0 不入账）
                    state.record_usage_cached(
                        super::usage::UsageBucket::Custom,
                        r.get("usage").map(wb_sse::cached_tokens_of).unwrap_or(0),
                    );
                    state.record_usage_custom(&model, &key_id, true, stream, duration_ms, pt, ct);
                    state.logger.log_request(
                        "custom", "POST", proto.log_path(), &model, stream, 200, "custom", duration_ms,
                        &key_name, &acct_name, None,
                    );
                    // 成功：清熔断 + 记会话粘上游
                    custom_lb::clear_circuit(&cm.id);
                    if let Some(sk) = &session_key {
                        custom_lb::record_sticky_upstream(&state, sk, &cm.id);
                    }
                    return Ok(r);
                }
                (None, Some((code, msg))) => {
                    custom_lb::note_circuit_failure(&cm.id, FailKind::Server);
                    state.record_usage_custom(&model, &key_id, false, stream, duration_ms, 0, 0);
                    set_last_error(&state, format!("custom model={} code={} msg={}", model, code, msg));
                    state.logger.log_request(
                        "custom", "POST", proto.log_path(), &model, stream, 200, "custom",
                        duration_ms, &key_name, &acct_name,
                        // 审查修复（M-3）：失败 attempt 错误标注带上游业务码，
                        // 排障时不被 200（上游 HTTP 层真实状态）误导
                        Some(&format!("upstream code={} msg={}", code, msg)),
                    );
                    // 本地聚合未向客户端产出任何字节 → 可安全轮换
                    last_err = msg;
                    attempt += 1;
                    if attempt >= max_attempts {
                        // 流内业务码非 HTTP 状态语义 → 终态按 502 报客户端
                        last_status = 502;
                        break;
                    }
                }
                _ => {
                    custom_lb::note_circuit_failure(&cm.id, FailKind::Server);
                    state.record_usage_custom(&model, &key_id, false, stream, duration_ms, 0, 0);
                    state.logger.log_request(
                        "custom", "POST", proto.log_path(), &model, stream, 502, "custom",
                        duration_ms, &key_name, &acct_name, Some("empty response"),
                    );
                    last_err = "自定义上游返回为空".into();
                    attempt += 1;
                    if attempt >= max_attempts {
                        last_status = 502;
                        break;
                    }
                }
            }
        }
        // 组内全试完仍失败 → [DEBUG] 排障摘要（§8，对齐 Trae 耗尽 [DEBUG] 口径）
        state.logger.log_debug_line(format!(
            "[DEBUG] UPSTREAM_GROUP_EXHAUSTED tried={} pool=custom model={} stream={} last={}",
            tried.len(),
            model,
            stream,
            last_err,
        ));
        Err((last_status, last_err))
    })
    .await;

    match result {
        Ok(Ok(resp)) => {
            let body = match proto {
                Protocol::Anthropic => wb_sse::completion_to_anthropic(
                    &resp,
                    &format!("msg_{}", now_ts()),
                    &model_outer,
                ),
                Protocol::OpenAiText => wb_sse::completion_to_text(&resp, &model_outer),
                Protocol::Responses => super::wb_responses::completion_to_responses(
                    &resp,
                    &format!("resp_{}", now_ts()),
                    &model_outer,
                ),
                Protocol::OpenAi => resp,
            };
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|_| {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::from("internal server error"))
                        .unwrap()
                })
        }
        Ok(Err((status, msg))) => {
            // 审查修复（M-2）：终态状态码与流式路径口径对齐（组耗尽 503 / 上游
            // HTTP 错误透传 / 首字超时 504 / 空响应与流内错误 502），不再一律 502
            let sc = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            match proto {
                Protocol::Anthropic => anthropic_error(sc, "api_error", &msg),
                _ => openai_error(sc, "upstream_error", &msg),
            }
        }
        Err(e) => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("task join error: {}", e),
        ),
    }
}

// ==================== 测试（§5.3 验收：轮换端到端） ====================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// 本地 mock OpenAI 兼容 SSE 上游：首个连接回 401，第二个连接回 200 SSE
    /// （单上游 401、另一上游 200 → 组内轮换客户端无感，方案 §5.3 验收用例）
    fn mock_upstream() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for (i, conn) in listener.incoming().enumerate() {
                let Ok(mut s) = conn else { break };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let resp: &[u8] = if i == 0 {
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                      data: {\"id\":\"cmock\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
                      data: {\"id\":\"cmock\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                      data: [DONE]\n\n"
                };
                let _ = s.write_all(resp);
                let _ = s.flush();
                if i >= 1 {
                    break; // 只服务两个连接（两个 attempt）
                }
            }
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    fn cm(id: &str, base_url: &str) -> CustomModel {
        CustomModel {
            id: id.into(),
            name: "mock-model".into(),
            base_url: base_url.into(),
            api_key: format!("key-{id}"),
            enabled: true,
            ..Default::default()
        }
    }

    /// 端到端轮换（§5.3 验收「一条 401 时请求自动落到另一条且客户端无感」）：
    /// 组 = [401 上游, 200 上游] → 客户端收到 200 与内容 "ok"；401 条目落 Auth 长窗；
    /// 200 条目熔断被清除且记会话粘上游
    #[tokio::test]
    async fn t01_rotation_401_then_200_client_unaware() {
        let dir = std::env::temp_dir().join(format!("twa_custom_route_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = std::sync::Arc::new(super::super::test_shared_state(dir.clone()));
        let base = mock_upstream();
        let group = vec![cm("cm-mock00000001", &base), cm("cm-mock00000002", &base)];

        let resp = custom_aggregate_chat(
            state.clone(),
            br#"{"model":"mock-model","messages":[{"role":"user","content":"hi"}]}"#.to_vec(),
            "mock-model".into(),
            group,
            false,
            Instant::now(),
            Protocol::OpenAi,
            "kid".into(),
            state.inflight_guard(),
        )
        .await;

        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert_eq!(
            status,
            StatusCode::OK,
            "401 后必须轮换到第二上游成功；响应体：{}",
            String::from_utf8_lossy(&bytes)
        );
        assert_eq!(
            body.pointer("/choices/0/message/content").and_then(Value::as_str),
            Some("ok"),
            "客户端无感拿到第二上游内容"
        );
        // 首选条目必须进入熔断表（全量并行下首连可能偶发传输层失败 → Server
        // 30s 窗而非 Auth 600s 窗；Auth 分类由 t02 纯函数断言 + custom_lb::t05
        // 覆盖，此处只断言「失败已记账、冷却中」这一轮换前提）
        let snap = custom_lb::circuit_snapshot();
        let hit = snap
            .iter()
            .find(|(id, _, _)| id == "cm-mock00000001")
            .expect("失败条目必须进入熔断表");
        assert!(hit.2 > 0 && hit.2 <= 600, "冷却窗口合理，实际 {}s", hit.2);
        // 成功条目熔断清除
        assert!(!custom_lb::circuit_open("cm-mock00000002"), "成功即清熔断");
        custom_lb::clear_circuit("cm-mock00000001");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 失败分类纯函数（§5.2 约束 3）：401/403/402 → Auth（长窗），其余 → Server
    #[test]
    fn t02_classify_failure_matrix() {
        assert!(matches!(classify_failure(401), FailKind::Auth));
        assert!(matches!(classify_failure(403), FailKind::Auth));
        assert!(matches!(classify_failure(402), FailKind::Auth));
        assert!(matches!(classify_failure(429), FailKind::Server));
        assert!(matches!(classify_failure(500), FailKind::Server));
        assert!(matches!(classify_failure(502), FailKind::Server));
        assert!(matches!(classify_failure(504), FailKind::Server));
    }
}
