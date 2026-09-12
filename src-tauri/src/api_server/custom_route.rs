//! 自定义模型上游执行路径（OpenAI 兼容直通）
//!
//! 与 WB 上游同策略：请求体强制 `stream:true`，上游只回 OpenAI 风格 SSE——
//! - 流式：wb_sse::stream_forward 按客户端协议转发；
//! - 非流式：wb_sse::aggregate 本地聚合为单个 OpenAI completion 后按协议转换。
//!
//! 与账号池路径的差异：无账号选取 / 无重试轮换 / 无脱敏 / 无粘性——
//! 单上游单 Key，失败即错误直返（调度语义见 custom_models.rs）。
//! 用量记账：独立 custom_days 桶（与 Trae/WB 侧分账，uid 固定 "custom"）。

use std::time::Instant;

use serde_json::{json, Value};

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

/// 请求体准备：强制 stream:true（统一 SSE 处理）+ 模型名对齐自定义条目
fn prep_body(body_vec: &[u8], cm: &CustomModel) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(body_vec).unwrap_or_else(|_| json!({}));
    v["stream"] = json!(true);
    if !cm.name.is_empty() {
        v["model"] = json!(cm.name);
    }
    v.to_string().into_bytes()
}

/// 发起自定义上游请求（Bearer 鉴权；连接 10s / 空闲读 300s，复用 WB agent）
fn make_custom_request(cm: &CustomModel, body: &[u8]) -> Result<Box<dyn std::io::Read + Send>, (u16, String)> {
    let url = custom_models::chat_url(&cm.base_url);
    let mut req = wb_upstream::wb_agent().post(&url);
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

/// 连通性探活（custom_model_test 命令底层）：向自定义上游发一条最小 chat 请求
/// （max_tokens=16 + stream:true，与网关同款 SSE 处理），返回成功摘要 / 失败原因。
/// 阻塞调用——调用方需放入 spawn_blocking 并外加超时（命令层 30s）。
pub fn probe(cm: &CustomModel) -> Result<String, String> {
    let body = json!({
        "model": cm.name,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 16,
        "stream": true,
    })
    .to_string()
    .into_bytes();
    let reader = make_custom_request(cm, &body).map_err(|(status, resp_body)| {
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

fn usage_pair(u: &Value) -> (u64, u64) {
    (
        u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
    )
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

// ==================== 流式 ====================

#[allow(clippy::too_many_arguments)]
pub fn custom_stream_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    cm: CustomModel,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::task::spawn_blocking(move || {
        let _inflight = guard; // 随后台任务存续至流结束（§4.5）
        let chat_id = chat_id_for(proto);

        // SSE keep-alive 15s：防中间层回收长流（与 WB/solo 路径同策略）
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let tx2 = tx.clone();
            let done2 = done.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
                tick.tick().await;
                loop {
                    tick.tick().await;
                    if done2.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    if tx2
                        .send(Ok(bytes::Bytes::from(": keep-alive\n\n")))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }

        let prepared = prep_body(&body_vec, &cm);
        match make_custom_request(&cm, &prepared) {
            Ok(reader) => match wb_upstream::lines_with_first_byte_timeout(reader) {
                Ok(lines) => {
                    let (error_info, sent_any, up_usage) =
                        wb_sse::stream_forward(lines, &tx, proto, &chat_id, &model);
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    {
                        let (pt, ct) = up_usage.as_ref().map(usage_pair).unwrap_or((0, 0));
                        state.record_usage_custom(
                            &model, &key_id, error_info.is_none(), true,
                            duration_ms, pt, ct,
                        );
                    }
                    if let Some((code, msg)) = error_info {
                        if !sent_any {
                            send_stream_error(&tx, proto, code, &msg);
                        }
                        set_last_error(&state, format!("custom model={} code={} msg={}", model, code, msg));
                        state.logger.log_request(
                            "custom", "POST", proto.log_path(), &model, true, 200, "custom",
                            duration_ms, Some(&msg),
                        );
                    } else {
                        state.logger.log_request(
                            "custom", "POST", proto.log_path(), &model, true, 200, "custom",
                            duration_ms, None,
                        );
                    }
                }
                Err(()) => {
                    // 首字超时（F-34 语义）：10s 内上游未产出任何字节
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    state.record_usage_custom(&model, &key_id, false, true, duration_ms, 0, 0);
                    state.logger.log_request(
                        "custom", "POST", proto.log_path(), &model, true, 504, "custom",
                        duration_ms, Some("first byte timeout"),
                    );
                    send_stream_error(&tx, proto, 504, "自定义上游 10 秒内未响应（首字超时）");
                }
            },
            Err((status, resp_body)) => {
                let duration_ms = start_ts.elapsed().as_millis() as u64;
                state.record_usage_custom(&model, &key_id, false, true, duration_ms, 0, 0);
                let preview: String = resp_body.chars().take(200).collect();
                set_last_error(&state, format!("custom model={} status={} body={}", model, status, preview));
                state.logger.log_request(
                    "custom", "POST", proto.log_path(), &model, true, status, "custom",
                    duration_ms, Some(&format!("upstream status={}", status)),
                );
                let msg = format!("自定义上游错误（status {}）：{}", status, preview);
                send_stream_error(&tx, proto, status as i64, &msg);
            }
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
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

// ==================== 非流式 ====================

#[allow(clippy::too_many_arguments)]
pub async fn custom_aggregate_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    cm: CustomModel,
    stream: bool,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    // 闭包 move 后外层协议转换仍需模型名，提前克隆
    let model_outer = model.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _inflight = guard; // 随聚合完成释放（§4.5）
        let prepared = prep_body(&body_vec, &cm);
        let reader = match make_custom_request(&cm, &prepared) {
            Ok(r) => r,
            Err((status, resp_body)) => {
                let duration_ms = start_ts.elapsed().as_millis() as u64;
                state.record_usage_custom(&model, &key_id, false, stream, duration_ms, 0, 0);
                let preview: String = resp_body.chars().take(200).collect();
                set_last_error(&state, format!("custom model={} status={} body={}", model, status, preview));
                state.logger.log_request(
                    "custom", "POST", proto.log_path(), &model, stream, status, "custom",
                    duration_ms, Some(&format!("upstream status={}", status)),
                );
                return Err(format!("自定义上游错误（status {}）：{}", status, preview));
            }
        };
        let lines = match wb_upstream::lines_with_first_byte_timeout(reader) {
            Ok(l) => l,
            Err(()) => {
                let duration_ms = start_ts.elapsed().as_millis() as u64;
                state.record_usage_custom(&model, &key_id, false, stream, duration_ms, 0, 0);
                state.logger.log_request(
                    "custom", "POST", proto.log_path(), &model, stream, 504, "custom",
                    duration_ms, Some("first byte timeout"),
                );
                return Err("自定义上游 10 秒内未响应（首字超时）".into());
            }
        };
        let (resp, error_info) = wb_sse::aggregate(lines, &format!("chatcmpl-{}", now_ts()));
        let duration_ms = start_ts.elapsed().as_millis() as u64;
        match (resp, error_info) {
            (Some(mut r), None) => {
                r["model"] = json!(model);
                let (pt, ct) = r.get("usage").map(usage_pair).unwrap_or((0, 0));
                state.record_usage_custom(&model, &key_id, true, stream, duration_ms, pt, ct);
                state.logger.log_request(
                    "custom", "POST", proto.log_path(), &model, stream, 200, "custom", duration_ms, None,
                );
                Ok(r)
            }
            (None, Some((code, msg))) => {
                state.record_usage_custom(&model, &key_id, false, stream, duration_ms, 0, 0);
                set_last_error(&state, format!("custom model={} code={} msg={}", model, code, msg));
                state.logger.log_request(
                    "custom", "POST", proto.log_path(), &model, stream, 200, "custom",
                    duration_ms, Some(&msg),
                );
                Err(msg)
            }
            _ => {
                state.record_usage_custom(&model, &key_id, false, stream, duration_ms, 0, 0);
                state.logger.log_request(
                    "custom", "POST", proto.log_path(), &model, stream, 502, "custom",
                    duration_ms, Some("empty response"),
                );
                Err("自定义上游返回为空".into())
            }
        }
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
        Ok(Err(msg)) => match proto {
            Protocol::Anthropic => anthropic_error(StatusCode::BAD_GATEWAY, "api_error", &msg),
            _ => openai_error(StatusCode::BAD_GATEWAY, "upstream_error", &msg),
        },
        Err(e) => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("task join error: {}", e),
        ),
    }
}
