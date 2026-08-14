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
            AGENT_HOST, APP_ID, EP_CHAT, IDE_VERSION, IDE_VERSION_CODE};

const MAX_ROTATE: usize = 3;
const MAX_BODY_BYTES: usize = 8 << 20;

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ==================== Handlers ====================

pub async fn healthz() -> &'static str {
    "ok"
}

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

pub async fn models() -> impl IntoResponse {
    Json(json!({
        "object": "list",
        "data": static_models(),
    }))
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
    let peek: Value = serde_json::from_slice(&body_vec).unwrap_or(json!({}));
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let converted = super::payload::prepare_body(&body_vec, &state.default_model);
    let state_clone = state.clone();

    if stream {
        stream_chat(state_clone, converted)
    } else {
        aggregate_chat(state_clone, converted).await
    }
}

// ==================== Streaming ====================

fn stream_chat(state: Arc<ApiSharedState>, converted: Vec<u8>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::task::spawn_blocking(move || {
        let chat_id = format!("chatcmpl-{}", now_ts());
        let mut tried = HashSet::new();

        for _ in 0..MAX_ROTATE {
            let picked = match state.pool.pick_excluding(&tried) {
                Some(p) => p,
                None => break,
            };
            tried.insert(picked.uid.clone());
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            match make_upstream_request(&picked.jwt, &picked.uid, &converted) {
                Ok(reader) => {
                    // 连接成功 → 开始流式转换，mid-stream error 只冷却不轮换
                    let error_info = sse::stream_convert(reader, tx.clone(), &chat_id);
                    if let Some((code, msg)) = error_info {
                        let kind = classify_solo_error(code, &msg);
                        if kind != ErrKind::None {
                            state.pool.note_error(&picked.uid, kind);
                            *safe_lock(&state.last_error) =
                                Some(format!("uid={} code={} msg={}", picked.uid, code, msg));
                        }
                    } else {
                        state.pool.note_success(&picked.uid);
                    }
                    return; // 流式结束后直接返回
                }
                Err((status, resp_body)) => {
                    let kind = classify_error(status, &resp_body);
                    state.pool.note_error(&picked.uid, kind);
                    let preview = safe_slice(&resp_body, 200);
                    *safe_lock(&state.last_error) =
                        Some(format!("uid={} status={} body={}", picked.uid, status, preview));
                    continue;
                }
            }
        }

        // 所有账号不可用
        let _ = tx.blocking_send(Ok(bytes::Bytes::from(
            "data: {\"error\":{\"message\":\"no healthy account available\",\"type\":\"api_error\",\"code\":\"no_healthy_account\"}}\n\n",
        )));
        let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
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

async fn aggregate_chat(state: Arc<ApiSharedState>, converted: Vec<u8>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let mut tried = HashSet::new();

        for _ in 0..MAX_ROTATE {
            let picked = match state.pool.pick_excluding(&tried) {
                Some(p) => p,
                None => break,
            };
            tried.insert(picked.uid.clone());
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            match make_upstream_request(&picked.jwt, &picked.uid, &converted) {
                Ok(reader) => {
                    let chat_id = format!("chatcmpl-{}", now_ts());
                    let (resp, error_info) = sse::aggregate(reader, &chat_id);
                    match (resp, error_info) {
                        (Some(r), None) => {
                            state.pool.note_success(&picked.uid);
                            return Ok(r);
                        }
                        (None, Some((code, msg))) => {
                            let kind = classify_solo_error(code, &msg);
                            state.pool.note_error(&picked.uid, kind);
                            *safe_lock(&state.last_error) =
                                Some(format!("uid={} code={} msg={}", picked.uid, code, msg));
                            continue;
                        }
                        _ => {
                            state.pool.note_error(&picked.uid, ErrKind::Server);
                            continue;
                        }
                    }
                }
                Err((status, resp_body)) => {
                    let kind = classify_error(status, &resp_body);
                    state.pool.note_error(&picked.uid, kind);
                    *safe_lock(&state.last_error) =
                        Some(format!("uid={} status={}", picked.uid, status));
                    continue;
                }
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
        Ok(Err(msg)) => {
            openai_error(StatusCode::SERVICE_UNAVAILABLE, "no_healthy_account", &msg)
        }
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
    uid: &str,
    body: &[u8],
) -> Result<Box<dyn Read + Send>, (u16, String)> {
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt)
    };
    let url = format!("{}{}", AGENT_HOST, EP_CHAT);

    let resp = streaming_agent()
        .post(&url)
        .set("content-type", "application/json")
        .set("accept", "text/event-stream")
        .set("user-agent", &format!("Trae/{}", IDE_VERSION))
        .set("authorization", &auth)
        .set("x-cloudide-token", jwt)
        .set("x-ide-token", jwt)
        .set("x-uid", uid)
        .set("x-app-id", APP_ID)
        .set("x-app-version", "default")
        .set("x-ide-version", IDE_VERSION)
        .set("x-ide-version-code", IDE_VERSION_CODE)
        .set("x-app-version-code", IDE_VERSION_CODE)
        .set("x-ide-version-type", "stable")
        .set("x-device-type", "windows")
        .set("x-os-version", "Windows 11 Pro")
        .set("x-device-brand", "83DG")
        .set("request-traffic-type", "prod")
        .send_bytes(body);

    match resp {
        Ok(r) => Ok(Box::new(r.into_reader())),
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            Err((code, body))
        }
        Err(e) => Err((502, format!("transport: {}", e))),
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

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn safe_slice(s: &str, n: usize) -> &str {
    if s.len() > n {
        &s[..n]
    } else {
        s
    }
}

fn static_models() -> Vec<Value> {
    let names = [
        "Doubao-Seed-2.1-Pro",
        "seed-code-pro-0430",
        "Doubao-Seed-2.1-Turbo",
        "Doubao-Seed-2.0-Code",
        "DeepSeek-V4-Flash-Official",
        "browser_use_subagent",
        "glm-5.2",
        "glm-5-turbo",
        "glm-5",
        "DeepSeek-V4-Pro",
        "DeepSeek-V4-Flash",
        "kimi-k3",
        "kimi-k2.7-code",
        "kimi-k2.6",
        "minimax-m3",
        "qwen-3.7-plus",
        "sagitta",
        "aquila",
        "custom_model_gemini",
        "custom_model_placeholder",
        "custom_model_1M_text",
        "custom_model_1M",
        "custom_model_kimi",
        "custom_model_claude",
        "custom_model_gpt-5",
        "custom_model_no-fc",
        "custom_model_deepseek_chat",
        "custom_model_deepseek_reasoner",
        "custom_model_deepseek_v4",
        "explore_sub_agent_v13",
        "explore_sub_agent_v2",
        "summary",
    ];
    names
        .iter()
        .map(|name| {
            json!({
                "id": name,
                "object": "model",
                "created": 1753600000,
                "owned_by": "trae-solo",
                "context_length": 131072,
            })
        })
        .collect()
}
