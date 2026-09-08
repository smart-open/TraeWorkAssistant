use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use super::sse;
use super::{classify_error, classify_solo_error, streaming_agent, ApiSharedState, ErrKind,
            AGENT_HOST, APP_ID, EP_LLM_CHAT, IDE_VERSION, IDE_VERSION_CODE, REFERER_BASE};

const MAX_ROTATE: usize = 3;
const MAX_BODY_BYTES: usize = 8 << 20;

/// 客户端协议：决定响应/流事件的输出格式（请求侧均已统一转为 OpenAI 内部格式）
#[derive(Clone, Copy, PartialEq)]
pub enum Protocol {
    OpenAi,
    Anthropic,
}

impl Protocol {
    fn log_path(self) -> &'static str {
        match self {
            Protocol::OpenAi => "/v1/chat/completions",
            Protocol::Anthropic => "/v1/messages",
        }
    }
}

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ==================== Handlers ====================

pub async fn health(State(state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    let pool = state.pool.status_list();
    let available = pool.iter().filter(|p| !p.disabled && !p.cooling).count();
    let cooling = pool.iter().filter(|p| p.cooling).count();
    let disabled = pool.iter().filter(|p| p.disabled).count();
    let total_credits: f64 = pool.iter().filter_map(|p| p.credits).sum();
    let total = state.total_requests.load(std::sync::atomic::Ordering::Relaxed);
    let active = safe_lock(&state.active_uid).clone();
    let last_err = safe_lock(&state.last_error).clone();

    Json(json!({
        "status": "ok",
        "running": true,
        "total_requests": total,
        "active_uid": active,
        "last_error": last_err,
        "pool": {
            "total_accounts": pool.len(),
            "available": available,
            "cooling": cooling,
            "disabled": disabled,
            "total_credits": (total_credits * 100.0).round() / 100.0,
        }
    }))
}

pub async fn status(State(state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    let pool = state.pool.status_list();
    let now: i64 = now_ts() as i64;
    let total = state.total_requests.load(std::sync::atomic::Ordering::Relaxed);
    let active = safe_lock(&state.active_uid).clone();
    let last_err = safe_lock(&state.last_error).clone();

    // 汇总统计
    let total_accounts = pool.len();
    let available = pool.iter().filter(|p| !p.disabled && !p.cooling).count();
    let cooling = pool.iter().filter(|p| p.cooling).count();
    let disabled = pool.iter().filter(|p| p.disabled).count();
    let total_credits: f64 = pool.iter().filter_map(|p| p.credits).sum();
    let total_credits = (total_credits * 100.0).round() / 100.0;

    // 账号明细
    let accounts: Vec<Value> = pool.iter().map(|p| {
        let status = if p.disabled {
            "disabled"
        } else if p.cooling {
            "cooling"
        } else if p.credits_expire_at.map_or(false, |exp| exp < now) {
            "expired"
        } else if p.credits.map_or(false, |c| c <= 0.0) {
            "no_credits"
        } else {
            "available"
        };
        json!({
            "uid": p.uid,
            "name": p.name,
            "status": status,
            "credits": p.credits,
            "credits_expire_at": p.credits_expire_at,
            "cooling": p.cooling,
            "cooldown_until": p.cooldown_until,
            "cooldown_reason": p.cooldown_reason,
            "disabled": p.disabled,
            "err_count": p.err_count,
        })
    }).collect();

    Json(json!({
        "running": true,
        "total_requests": total,
        "active_uid": active,
        "last_error": last_err,
        "summary": {
            "total_accounts": total_accounts,
            "available": available,
            "cooling": cooling,
            "disabled": disabled,
            "total_credits": total_credits,
        },
        "accounts": accounts,
    }))
}

pub async fn models(State(state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    // 与应用配置同源：读取 api_models.json（缺失时写入默认列表），
    // 官网同步后无需重启 API 服务即可通过 /v1/models 看到最新列表
    let data_dir = state.data_dir.clone();
    let list = tokio::task::spawn_blocking(move || super::models_sync::load_models(&data_dir))
        .await
        .unwrap_or_default();
    let data: Vec<Value> = list
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "created": 1753600000,
                "owned_by": "trae-solo",
                "context_length": 131072,
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

pub async fn chat_completions(
    State(state): State<Arc<ApiSharedState>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body exceeds 8MB limit",
        );
    }

    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let body_vec = body.to_vec();
    // 校验 JSON：无效请求体直接 400，不转发上游（与 /v1/messages 行为对齐）
    let peek: Value = match serde_json::from_slice(&body_vec) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(&state.default_model)
        .to_string();
    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();

    if stream {
        stream_chat(state_clone, body_vec, model, stream, start_ts, Protocol::OpenAi)
    } else {
        aggregate_chat(state_clone, body_vec, model, stream, start_ts, Protocol::OpenAi).await
    }
}

/// Anthropic Messages 端点（F-39：+Anthropic 适配）
/// 请求：POST /v1/messages，鉴权支持 x-api-key 或 Authorization: Bearer
pub async fn messages(
    State(state): State<Arc<ApiSharedState>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return anthropic_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request body exceeds 8MB limit",
        );
    }

    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // 校验 JSON 并预读 stream/model，再整体转为 OpenAI 内部格式复用现有链路
    let peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    if peek.get("messages").and_then(|m| m.as_array()).is_none() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages: field required",
        );
    }
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();

    let body_vec = super::payload::anthropic_to_openai(&body);
    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();

    if stream {
        stream_chat(state_clone, body_vec, model, stream, start_ts, Protocol::Anthropic)
    } else {
        aggregate_chat(state_clone, body_vec, model, stream, start_ts, Protocol::Anthropic).await
    }
}

// ==================== Streaming ====================

fn stream_chat(state: Arc<ApiSharedState>, body_vec: Vec<u8>, model: String, stream: bool, start_ts: std::time::Instant, proto: Protocol) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::task::spawn_blocking(move || {
        let chat_id = match proto {
            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
            Protocol::Anthropic => format!("msg_{}", now_ts()),
        };
        let mut tried = HashSet::new();

        for _ in 0..MAX_ROTATE {
            let picked = match state.pool.pick_excluding(&tried) {
                Some(p) => p,
                None => break,
            };
            tried.insert(picked.uid.clone());
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            let converted = super::payload::prepare_llm_chat_body(
                &body_vec, &state.default_model, &picked.uid, &picked.device_id, &picked.machine_id,
            );

            match make_upstream_request(&picked.jwt, &picked.uid, &picked.device_id, &picked.machine_id, &converted) {
                Ok(reader) => {
                    // 连接成功 → 开始流式转换，mid-stream error 只冷却不轮换
                    let error_info = match proto {
                        Protocol::OpenAi => sse::stream_convert(reader, tx.clone(), &chat_id),
                        Protocol::Anthropic => {
                            sse::stream_convert_anthropic(reader, tx.clone(), &chat_id, &model)
                        }
                    };
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    if let Some((code, msg)) = error_info {
                        let kind = classify_solo_error(code, &msg);
                        if kind != ErrKind::None {
                            state.pool.note_error(&picked.uid, kind);
                            *safe_lock(&state.last_error) =
                                Some(format!("uid={} code={} msg={}", picked.uid, code, msg));
                        }
                        state.logger.log_request(
                            "POST", proto.log_path(), &model, stream,
                            200, &picked.uid, duration_ms, Some(&msg),
                        );
                        if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                            state.logger.log_debug(&picked.uid, &converted, None, 200, Some(&msg));
                        }
                    } else {
                        state.pool.note_success(&picked.uid);
                        state.logger.log_request(
                            "POST", proto.log_path(), &model, stream,
                            200, &picked.uid, duration_ms, None,
                        );
                        if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                            state.logger.log_debug(&picked.uid, &converted, None, 200, None);
                        }
                    }
                    return; // 流式结束后直接返回
                }
                Err((status, resp_body)) => {
                    let kind = classify_error(status, &resp_body);
                    state.pool.note_error(&picked.uid, kind);
                    let preview = safe_slice(&resp_body, 200);
                    *safe_lock(&state.last_error) =
                        Some(format!("uid={} status={} body={}", picked.uid, status, preview));
                    state.logger.log_request(
                        "POST", proto.log_path(), &model, stream,
                        status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                        Some(&format!("upstream status={}", status)),
                    );
                    if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                        state.logger.log_debug(&picked.uid, &converted, Some(resp_body.as_bytes()), status, Some(&preview));
                    }
                    continue;
                }
            }
        }

        // 所有账号不可用
        let duration_ms = start_ts.elapsed().as_millis() as u64;
        let diag = state.pool.diagnose();
        let diag_summary: Vec<String> = diag
            .iter()
            .map(|d| {
                let credits_str = d.credits.map(|c| format!("{:.0}", c)).unwrap_or_else(|| "N/A".to_string());
                let cd_str = if d.until > 0 { format!(",cd={}s", d.until.saturating_sub(now_ts() as i64)) } else { String::new() };
                let exp_str = d.credits_expire_at.filter(|&e| e > 0).map(|e| format!(",exp={}", e)).unwrap_or_default();
                let dis_str = if d.disabled { ",DIS" } else { "" };
                format!("{}({}:{},cr={}{}{}{})", d.name, d.uid.get(..8).unwrap_or(&d.uid), d.reason, credits_str, cd_str, exp_str, dis_str)
            })
            .collect();
        state.logger.log_request(
            "POST", proto.log_path(), &model, stream,
            503, "none", duration_ms, Some("no healthy account"),
        );
        // 写入 app.log 供排查
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let local_ts = now + 8 * 3600;
            let h = (local_ts % 86400) / 3600;
            let m = (local_ts % 3600) / 60;
            let s = local_ts % 60;
            let diag_line = format!(
                "NO_HEALTHY_ACCOUNT [{:02}:{:02}:{:02}] tried={} pool={} reasons=[{}]",
                h, m, s, tried.len(), diag.len(), diag_summary.join(", "),
            );
            if let Some(mut f) = state.logger.get_writer() {
                use std::io::Write;
                let _ = writeln!(f, "[DEBUG] {}", diag_line);
            }
        }
        match proto {
            Protocol::OpenAi => {
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(
                    "data: {\"error\":{\"message\":\"no healthy account available\",\"type\":\"api_error\",\"code\":\"no_healthy_account\"}}\n\n",
                )));
                let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
            }
            Protocol::Anthropic => {
                let err = json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "no healthy account available",
                    },
                });
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                    "event: error\ndata: {}\n\n",
                    err
                ))));
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

// ==================== Non-streaming ====================

async fn aggregate_chat(state: Arc<ApiSharedState>, body_vec: Vec<u8>, model: String, stream: bool, start_ts: std::time::Instant, proto: Protocol) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let mut tried = HashSet::new();

        for _ in 0..MAX_ROTATE {
            let picked = match state.pool.pick_excluding(&tried) {
                Some(p) => p,
                None => break,
            };
            tried.insert(picked.uid.clone());
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            let converted = super::payload::prepare_llm_chat_body(
                &body_vec, &state.default_model, &picked.uid, &picked.device_id, &picked.machine_id,
            );

            match make_upstream_request(&picked.jwt, &picked.uid, &picked.device_id, &picked.machine_id, &converted) {
                Ok(reader) => {
                    let chat_id = match proto {
                        Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
                        Protocol::Anthropic => format!("msg_{}", now_ts()),
                    };
                    let (resp, error_info) = match proto {
                        Protocol::OpenAi => sse::aggregate(reader, &chat_id),
                        Protocol::Anthropic => sse::aggregate_anthropic(reader, &chat_id, &model),
                    };
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    match (resp, error_info) {
                        (Some(r), None) => {
                            state.pool.note_success(&picked.uid);
                            state.logger.log_request(
                                "POST", proto.log_path(), &model, stream,
                                200, &picked.uid, duration_ms, None,
                            );
                            if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                state.logger.log_debug(&picked.uid, &converted, Some(r.to_string().as_bytes()), 200, None);
                            }
                            return Ok(r);
                        }
                        (None, Some((code, msg))) => {
                            let kind = classify_solo_error(code, &msg);
                            state.pool.note_error(&picked.uid, kind);
                            *safe_lock(&state.last_error) =
                                Some(format!("uid={} code={} msg={}", picked.uid, code, msg));
                            state.logger.log_request(
                                "POST", proto.log_path(), &model, stream,
                                200, &picked.uid, duration_ms, Some(&msg),
                            );
                            if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                state.logger.log_debug(&picked.uid, &converted, None, 200, Some(&msg));
                            }
                            continue;
                        }
                        _ => {
                            state.pool.note_error(&picked.uid, ErrKind::Server);
                            state.logger.log_request(
                                "POST", proto.log_path(), &model, stream,
                                502, &picked.uid, duration_ms, Some("empty response"),
                            );
                            if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                state.logger.log_debug(&picked.uid, &converted, None, 502, Some("empty response"));
                            }
                            continue;
                        }
                    }
                }
                Err((status, resp_body)) => {
                    let kind = classify_error(status, &resp_body);
                    state.pool.note_error(&picked.uid, kind);
                    *safe_lock(&state.last_error) =
                        Some(format!("uid={} status={}", picked.uid, status));
                    state.logger.log_request(
                        "POST", proto.log_path(), &model, stream,
                        status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                        Some(&format!("upstream status={}", status)),
                    );
                    if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                        state.logger.log_debug(&picked.uid, &converted, Some(resp_body.as_bytes()), status, Some(&resp_body));
                    }
                    continue;
                }
            }
        }

        let duration_ms = start_ts.elapsed().as_millis() as u64;
        let diag = state.pool.diagnose();
        let diag_summary: Vec<String> = diag
            .iter()
            .map(|d| {
                let credits_str = d.credits.map(|c| format!("{:.0}", c)).unwrap_or_else(|| "N/A".to_string());
                let cd_str = if d.until > 0 { format!(",cd={}s", d.until.saturating_sub(now_ts() as i64)) } else { String::new() };
                let exp_str = d.credits_expire_at.filter(|&e| e > 0).map(|e| format!(",exp={}", e)).unwrap_or_default();
                let dis_str = if d.disabled { ",DIS" } else { "" };
                format!("{}({}:{},cr={}{}{}{})", d.name, d.uid.get(..8).unwrap_or(&d.uid), d.reason, credits_str, cd_str, exp_str, dis_str)
            })
            .collect();
        state.logger.log_request(
            "POST", proto.log_path(), &model, stream,
            503, "none", duration_ms, Some("no healthy account"),
        );
        // 写入诊断日志
        {
            if let Some(mut f) = state.logger.get_writer() {
                use std::io::Write;
                let _ = writeln!(
                    f,
                    "[DEBUG] NO_HEALTHY_ACCOUNT(non-stream) tried={} pool={} reasons=[{}]",
                    tried.len(), diag.len(), diag_summary.join(", "),
                );
            }
        }
        Err("no healthy account available".to_string())
    })
    .await;

    match result {
        Ok(Ok(resp)) => Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(resp.to_string()))
            .unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("internal server error"))
                    .unwrap()
            }),
        Ok(Err(msg)) => match proto {
            Protocol::OpenAi => openai_error(StatusCode::SERVICE_UNAVAILABLE, "no_healthy_account", &msg),
            Protocol::Anthropic => anthropic_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", &msg),
        },
        Err(e) => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("task join error: {}", e),
        ),
    }
}

// ==================== Upstream Request ====================

fn make_upstream_request(
    jwt: &str,
    _uid: &str,
    device_id: &str,
    machine_id: &str,
    body: &[u8],
) -> Result<Box<dyn Read + Send>, (u16, String)> {
    let url = format!("{}{}", AGENT_HOST, EP_LLM_CHAT);
    let referer = format!("{}{}", REFERER_BASE, EP_LLM_CHAT);
    let trace_id = format!(
        "00-{}-{}-01",
        uuid_like_id(),
        uuid_like_id()
    );
    let request_id = format!("req_{}", uuid_like_id());

    let resp = streaming_agent()
        .post(&url)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .set("accept-encoding", "gzip, deflate, br, zstd")
        .set("user-agent", "TraeClient/TTNet")
        .set("x-ide-token", jwt)
        .set("x-app-id", APP_ID)
        .set("x-app-version", "default")
        .set("x-app-version-code", IDE_VERSION_CODE)
        .set("x-ide-version", IDE_VERSION)
        .set("x-ide-version-code", IDE_VERSION_CODE)
        .set("x-ide-version-type", "stable")
        .set("x-device-type", "windows")
        .set("x-device-brand", "CREFG-XX")
        .set("x-device-cpu", "Intel")
        .set("x-device-id", device_id)
        .set("x-machine-id", machine_id)
        .set("x-os-version", "Windows 11 Home China")
        .set("request-traffic-type", "prod")
        .set("package-type", "stable_cn")
        .set("x-lgw-req-sdk-type", "3")
        .set("x-lscbd-aid", "787976")
        .set("x-lscbd-platform", "windows")
        .set("x-ss-dp", "787976")
        .set("app-version", IDE_VERSION)
        .set("x-custom-trace-id", &trace_id[..16])
        .set("x-flow-traceparent", &format!("04-{}-{}-01", &trace_id[3..35], uuid_like_id()))
        .set("x-tt-trace-id", &trace_id)
        .set("x-request-id", &request_id)
        .set("referer", &referer)
        .send_bytes(body);

    match resp {
        Ok(r) => Ok(Box::new(r.into_reader())),
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            Err((code, body))
        }
        Err(e) => {
            let err_str = format!("{}", e);
            // 区分 DNS 解析失败 / 连接超时 / TLS 错误，提供更精准的诊断
            let detail = if err_str.contains("dns") || err_str.contains("resolve") || err_str.contains("name resolution") {
                format!("DNS解析失败（{} 无法解析），请检查网络或代理设置: {}", AGENT_HOST, e)
            } else if err_str.contains("timed out") || err_str.contains("timeout") {
                format!("连接超时（{} 10秒内未响应），请检查网络连通性: {}", AGENT_HOST, e)
            } else if err_str.contains("tls") || err_str.contains("certificate") || err_str.contains("ssl") {
                format!("TLS证书验证失败: {}", e)
            } else {
                format!("传输错误: {}", e)
            };
            Err((502, detail))
        }
    }
}

// ==================== Helpers ====================

fn openai_error(status: StatusCode, code: &str, msg: &str) -> Response {
    let body = json!({
        "error": {
            "message": msg,
            "type": "api_error",
            "code": code,
        }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
                .unwrap()
        })
}

/// Anthropic 错误响应格式：{"type":"error","error":{"type","message"}}
fn anthropic_error(status: StatusCode, err_type: &str, msg: &str) -> Response {
    let body = json!({
        "type": "error",
        "error": {
            "type": err_type,
            "message": msg,
        }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("{\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"internal error\"}}"))
                .unwrap()
        })
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 生成类似 UUID 的十六进制字符串，用于 trace-id 等请求头
fn uuid_like_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = now.as_nanos();
    let seed = (nanos as u64).wrapping_mul(0x517cc1b727220a95);
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&seed.to_le_bytes());
    buf[8..16].copy_from_slice(&(seed.wrapping_add(0x9e3779b97f4a7c15)).to_le_bytes());
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

fn safe_slice(s: &str, n: usize) -> &str {
    // 按字符边界截断：字节切片 &s[..n] 在多字节字符中间会 panic
    // （上游错误 JSON 常含中文，200 字节处极可能落在 UTF-8 序列中间）
    s.get(..n).unwrap_or(s)
}
