use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use super::sse;
use super::usage::{extract_tokens, KeyId};
use super::wb_catalog;
use super::wb_model_route;
use super::wb_route;
use super::{classify_error, classify_solo_error, streaming_agent, ApiSharedState, ErrKind,
            AGENT_HOST, APP_ID, EP_LLM_CHAT, IDE_VERSION, IDE_VERSION_CODE, REFERER_BASE};

const MAX_ROTATE: usize = 3;
const MAX_BODY_BYTES: usize = 8 << 20;

/// 客户端协议：决定响应/流事件的输出格式（请求侧均已统一转为 OpenAI 内部格式）
#[derive(Clone, Copy, PartialEq)]
pub enum Protocol {
    OpenAi,
    /// OpenAI legacy text completions（/v1/completions）
    OpenAiText,
    Anthropic,
    /// Codex Responses API（/v1/responses，T4.1/F-40）
    Responses,
}

impl Protocol {
    fn log_path(self) -> &'static str {
        match self {
            Protocol::OpenAi => "/v1/chat/completions",
            Protocol::OpenAiText => "/v1/completions",
            Protocol::Anthropic => "/v1/messages",
            Protocol::Responses => "/v1/responses",
        }
    }
}

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ==================== Handlers ====================

/// WB 上游模型目录命中（T2.1 原始判定，保留供 /v1/models 与诊断复用）
#[allow(dead_code)]
fn wb_model_requested(state: &ApiSharedState, model: &str) -> bool {
    wb_catalog::find(&wb_catalog::load(&state.data_dir), model).is_some()
}

fn internal_error_response() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
        .unwrap()
}

/// T5.2/F-61 四段模型路由解析（别名→规则→系列通配→后缀）+ T5.6③ 后台任务降级。
/// 返回 (最终模型, 路由级 effort 注入提示)；全未命中目录 → None（走 SOLO 上游）。
fn resolve_wb_target(
    state: &ApiSharedState,
    model: &str,
    body: &Value,
) -> Option<(String, Option<String>)> {
    let cfg = wb_model_route::load_config(&state.data_dir);
    let catalog = wb_catalog::load(&state.data_dir);
    let r = wb_model_route::resolve(&cfg, &catalog, model);
    if wb_catalog::find(&catalog, &r.model).is_none() {
        return None;
    }
    // T5.6③ 后台任务降级（显式开启才生效）：标题/摘要类短请求 → 目录最低倍率模型
    let final_model = if state.wb_bg_downgrade.load(std::sync::atomic::Ordering::Relaxed)
        && wb_model_route::is_background_task(body)
    {
        wb_model_route::cheapest_catalog_model(&catalog).unwrap_or(r.model)
    } else {
        r.model
    };
    Some((final_model, r.effort_hint))
}

/// T5.3/F-62 默认深度思考：客户端未显式请求 effort 且无路由级提示时默认 high。
/// `explicit_effort` 由调用方按协议判定（OpenAI: reasoning_effort 字段；Anthropic: thinking 参数）
fn effective_effort_hint(
    state: &ApiSharedState,
    route_hint: Option<String>,
    explicit_effort: bool,
) -> Option<String> {
    if route_hint.is_some() {
        return route_hint;
    }
    if !explicit_effort
        && state
            .wb_default_thinking
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Some("high".to_string());
    }
    None
}

/// 注入 effort 提示到请求体字节流（T5.2④/T5.3）
fn apply_effort_hint(body_vec: Vec<u8>, hint: Option<String>) -> Vec<u8> {
    wb_model_route::inject_effort_hint(&body_vec, &hint)
}

/// 模型级冷却快速失败（T2.7/F-34：优先级高于 Key 级）
fn model_cooling_response(state: &ApiSharedState, model: &str, proto: Protocol) -> Response {
    let rem = wb_route::model_cooling_remaining(state, model).unwrap_or(0);
    let msg = format!("model {} cooling down, retry after {}s", model, rem);
    match proto {
        Protocol::Anthropic => anthropic_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", &msg),
        _ => openai_error(StatusCode::TOO_MANY_REQUESTS, "model_cooldown", &msg),
    }
}

pub async fn health(State(state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    let pool = state.pool.status_list();
    let wb_pool = state.wb_pool.status_list();
    let available = pool.iter().filter(|p| !p.disabled && !p.cooling).count();
    let cooling = pool.iter().filter(|p| p.cooling).count();
    let disabled = pool.iter().filter(|p| p.disabled).count();
    let total_credits: f64 = pool.iter().filter_map(|p| p.credits).sum();
    let total = state.total_requests.load(std::sync::atomic::Ordering::Relaxed);
    let active = safe_lock(&state.active_uid).clone();
    let last_err = safe_lock(&state.last_error).clone();
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let wb_available = wb_pool.iter().filter(|p| !p.disabled && !p.cooling).count();

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
        },
        "wb": {
            "enabled": wb_enabled,
            "total_accounts": wb_pool.len(),
            "available": wb_available,
        }
    }))
}

/// /healthz（T2.3/F-32）：无健康账号（两个池都没有）→ 503，供探活/看门狗
pub async fn healthz(State(state): State<Arc<ApiSharedState>>) -> Response {
    let pool = state.pool.status_list();
    let wb_pool = state.wb_pool.status_list();
    let solo_ok = pool.iter().any(|p| !p.disabled && !p.cooling);
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let wb_ok = wb_pool.iter().any(|p| !p.disabled && !p.cooling);
    if solo_ok || (wb_enabled && wb_ok) {
        (
            StatusCode::OK,
            axum::Json(json!({ "status": "ok", "solo_available": solo_ok, "wb_available": wb_ok })),
        )
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "status": "unavailable", "reason": "no healthy account" })),
        )
            .into_response()
    }
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
            "state": p.state,
        })
    }).collect();

    // WB 池画像（T2.3/F-32）
    let wb_pool = state.wb_pool.status_list();
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let wb_accounts: Vec<Value> = wb_pool.iter().map(|p| {
        json!({
            "uid": p.uid, "name": p.name, "credits": p.credits,
            "cooling": p.cooling, "cooldown_until": p.cooldown_until,
            "cooldown_reason": p.cooldown_reason, "disabled": p.disabled,
            "err_count": p.err_count, "state": p.state,
        })
    }).collect();
    let model_cooldowns: Vec<Value> = {
        let map = safe_lock(&state.model_cooldowns);
        map.iter()
            .filter(|(_, (until, _))| *until > now)
            .map(|(m, (until, fails))| json!({
                "model": m, "until": until, "remaining_s": until - now, "fails": fails,
            }))
            .collect()
    };

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
        "wb": {
            "enabled": wb_enabled,
            "total_accounts": wb_pool.len(),
            "accounts": wb_accounts,
            "model_cooldowns": model_cooldowns,
            "sticky_sessions": state.wb_sticky.len(),
            // 上游健康探针（F-34 ④/§2.2）：-1 未探测 / 0 不可达 / 1 在线
            "probe_ok": state.wb_probe_ok.load(std::sync::atomic::Ordering::Relaxed),
            "probe_ts_ms": state.wb_probe_ts_ms.load(std::sync::atomic::Ordering::Relaxed),
        },
    }))
}

pub async fn models(State(state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    // 与应用配置同源：读取 api_models.json（缺失时写入默认列表），
    // 官网同步后无需重启 API 服务即可通过 /v1/models 看到最新列表
    let data_dir = state.data_dir.clone();
    let list = tokio::task::spawn_blocking(move || super::models_sync::load_models(&data_dir))
        .await
        .unwrap_or_default();
    let mut data: Vec<Value> = list
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
    // WB 上游模型目录合并（T2.1/T2.3）：启用时并入，owned_by=workbuddy；
    // 能力字段读目录（T5.1/F-37：inputModalities→supports_image、supportedEfforts、
    // 倍率透传——徽章/降级判定由客户端按元数据自决，勿硬编码）
    if state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed) {
        for m in wb_catalog::load(&state.data_dir) {
            data.push(json!({
                "id": m.id,
                "object": "model",
                "created": 1753600000,
                "owned_by": "workbuddy",
                "context_length": m.context_length,
                "max_tokens": m.max_tokens,
                "rate": m.rate,
                "supports_image": m.supports_image,
                "supported_efforts": m.supported_efforts,
            }));
        }
    }
    Json(json!({ "object": "list", "data": data }))
}

pub async fn chat_completions(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body exceeds 8MB limit",
        );
    }

    // T5.6② 单端口协议区分：anthropic-version 头出现 → 客户端实为 Anthropic
    // Messages 协议，按路径分流给出明确指引（避免三协议混投后字段级静默错乱）
    if headers.contains_key("anthropic-version") {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "wrong_endpoint",
            "检测到 anthropic-version 头：该请求应为 Anthropic Messages 协议，请改用 POST /v1/messages（本网关单端口三协议按路径区分）",
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
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());

    // 模型路由（T5.2/F-61 四段管线）：解析成功 → WB 上游；未命中 → SOLO
    if let Some((resolved_model, route_hint)) = resolve_wb_target(&state, &model, &peek) {
        if !state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "wb_upstream_disabled",
                "该模型属 WorkBuddy 上游，但 WB 上游未启用（api_pool.json wb_enabled）",
            );
        }
        if wb_route::model_cooling_remaining(&state, &resolved_model).is_some() {
            return model_cooling_response(&state, &resolved_model, Protocol::OpenAi);
        }
        // T5.3 默认深度思考：客户端未带 reasoning_effort 时注入 high
        let explicit = peek.get("reasoning_effort").and_then(|v| v.as_str()).is_some();
        let hint = effective_effort_hint(&state, route_hint, explicit);
        let body_vec = apply_effort_hint(body_vec, hint);
        if stream {
            return wb_route::wb_stream_chat(state_clone, body_vec, resolved_model, start_ts, Protocol::OpenAi, key_str);
        }
        return wb_route::wb_aggregate_chat(state_clone, body_vec, resolved_model, stream, start_ts, Protocol::OpenAi, key_str).await;
    }

    if stream {
        stream_chat(state_clone, body_vec, model, stream, start_ts, Protocol::OpenAi, key_str)
    } else {
        aggregate_chat(state_clone, body_vec, model, stream, start_ts, Protocol::OpenAi, key_str).await
    }
}

/// Codex Responses API 端点（T4.1/F-40）：POST /v1/responses
///
/// 请求投影为 OpenAI 内部格式后复用 WB 上游既有管线（取号/重试/粘性/脱敏一份）。
/// 仅支持 WB 上游模型（Codex CLI `wire_api="responses"` 直配 base_url 的目标场景）；
/// 脱敏沿用全局 `wb_sanitize` 开关，审核命中按既有分级重试表退回重试。
pub async fn responses_api(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
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

    let peek: Value = match serde_json::from_slice(&body) {
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
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();

    // Responses → OpenAI chat 内部格式（纯投影，失败即 400）
    let chat_body: Value = match super::wb_responses::responses_to_chat(&peek) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &e);
        }
    };

    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());

    // T5.2/F-61 四段路由解析（Responses 仅支持 WB 上游模型）
    let (resolved_model, route_hint) = match resolve_wb_target(&state, &model, &chat_body) {
        Some(t) => t,
        None => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "model_not_found",
                &format!(
                    "model {} is not a WorkBuddy upstream model（/v1/responses 仅支持 WB 上游模型，目录见 /v1/models）",
                    model
                ),
            );
        }
    };
    if !state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed) {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "wb_upstream_disabled",
            "WB 上游未启用（api_pool.json wb_enabled）",
        );
    }
    if wb_route::model_cooling_remaining(&state, &resolved_model).is_some() {
        return model_cooling_response(&state, &resolved_model, Protocol::Responses);
    }

    let mut chat_body = chat_body;
    // T5.5/F-64 工具代执行：客户端声明 web_search 类工具且开关开启 → 代理侧代执行编排
    if state.wb_tool_exec.load(std::sync::atomic::Ordering::Relaxed)
        && super::wb_toolexec::responses_declares_web_search(&peek)
    {
        return wb_route::wb_tool_exec_chat(
            state_clone,
            chat_body,
            resolved_model,
            stream,
            start_ts,
            key_str,
        )
        .await;
    }
    chat_body["stream"] = json!(stream);
    // T5.3 默认深度思考（Responses: reasoning.effort 已投影为 reasoning_effort）
    let explicit = chat_body.get("reasoning_effort").and_then(|v| v.as_str()).is_some();
    let hint = effective_effort_hint(&state, route_hint, explicit);
    let body_vec = apply_effort_hint(serde_json::to_vec(&chat_body).unwrap_or_default(), hint);

    if stream {
        wb_route::wb_stream_chat(state_clone, body_vec, resolved_model, start_ts, Protocol::Responses, key_str)
    } else {
        wb_route::wb_aggregate_chat(state_clone, body_vec, resolved_model, stream, start_ts, Protocol::Responses, key_str).await
    }
}

/// Anthropic Messages 端点（F-39：+Anthropic 适配）
/// 请求：POST /v1/messages，鉴权支持 x-api-key 或 Authorization: Bearer
pub async fn messages(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
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
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());

    // 模型路由（T5.2/F-61 四段管线；body 已转为 OpenAI 内部格式）
    if let Some((resolved_model, route_hint)) = resolve_wb_target(&state, &model, &peek) {
        if !state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "该模型属 WorkBuddy 上游，但 WB 上游未启用（api_pool.json wb_enabled）",
            );
        }
        if wb_route::model_cooling_remaining(&state, &resolved_model).is_some() {
            return model_cooling_response(&state, &resolved_model, Protocol::Anthropic);
        }
        // T5.3 默认深度思考：Anthropic 侧 thinking 参数视为显式请求
        let explicit = peek.get("thinking").map_or(false, |t| !t.is_null());
        let hint = effective_effort_hint(&state, route_hint, explicit);
        let body_vec = apply_effort_hint(body_vec, hint);
        if stream {
            return wb_route::wb_stream_chat(state_clone, body_vec, resolved_model, start_ts, Protocol::Anthropic, key_str);
        }
        return wb_route::wb_aggregate_chat(state_clone, body_vec, resolved_model, stream, start_ts, Protocol::Anthropic, key_str).await;
    }

    if stream {
        stream_chat(state_clone, body_vec, model, stream, start_ts, Protocol::Anthropic, key_str)
    } else {
        aggregate_chat(state_clone, body_vec, model, stream, start_ts, Protocol::Anthropic, key_str).await
    }
}

/// OpenAI legacy text completions 端点（T9）
/// prompt（string 或 string[]）转单条 user message 复用现有链路，响应包装回
/// text_completion 结构。suffix/echo/logprobs/n 等参数不支持（忽略，上游单次补全）
pub async fn completions(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
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

    let peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    let prompt_text = match peek.get("prompt") {
        Some(Value::String(s)) => s.clone(),
        // 多段 prompt：拼接为单个 prompt（上游一次只产出一个补全，无法返回多 choice）
        Some(Value::Array(arr)) => {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if parts.is_empty() {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "prompt: array must contain strings",
                );
            }
            parts.join("\n\n")
        }
        _ => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "prompt: field required",
            )
        }
    };
    if prompt_text.trim().is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "prompt: must not be empty",
        );
    }
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();

    // prompt → user message，复用 /v1/chat/completions 内部链路
    let internal = json!({
        "model": model,
        "stream": stream,
        "messages": [{ "role": "user", "content": prompt_text }],
    });
    let body_vec = internal.to_string().into_bytes();

    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());

    // 模型路由（T5.2/F-61 四段管线）
    if let Some((resolved_model, route_hint)) = resolve_wb_target(&state, &model, &internal) {
        if !state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "wb_upstream_disabled",
                "该模型属 WorkBuddy 上游，但 WB 上游未启用（api_pool.json wb_enabled）",
            );
        }
        if wb_route::model_cooling_remaining(&state, &resolved_model).is_some() {
            return model_cooling_response(&state, &resolved_model, Protocol::OpenAiText);
        }
        // T5.3 默认深度思考（text completions 无 effort 字段 → 默认思考直接生效）
        let hint = effective_effort_hint(&state, route_hint, false);
        let body_vec = apply_effort_hint(body_vec, hint);
        if stream {
            return wb_route::wb_stream_chat(state_clone, body_vec, resolved_model, start_ts, Protocol::OpenAiText, key_str);
        }
        return wb_route::wb_aggregate_chat(state_clone, body_vec, resolved_model, stream, start_ts, Protocol::OpenAiText, key_str).await;
    }

    if stream {
        stream_chat(state_clone, body_vec, model, stream, start_ts, Protocol::OpenAiText, key_str)
    } else {
        aggregate_chat(state_clone, body_vec, model, stream, start_ts, Protocol::OpenAiText, key_str).await
    }
}

/// /v1/embeddings：上游 SOLO 无向量能力，明确返回 501（不做假实现）
pub async fn embeddings() -> Response {
    openai_error(
        StatusCode::NOT_IMPLEMENTED,
        "not_supported",
        "上游服务无 embeddings 能力，本网关不支持 /v1/embeddings，请使用 /v1/chat/completions 或 /v1/completions",
    )
}

/// /v1/images/generations 文生图（T5.4/F-63）：投影 WB 上游生图端点
pub async fn images_generations(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    images_entry(state, key_id, body, false).await
}

/// /v1/images/edits 图生图（T5.4/F-63）：接受 JSON（image 为 base64/data URL）。
/// 注：OpenAI SDK 默认 multipart/form-data；本端点仅接受 JSON 变体（零新增依赖红线），
/// 客户端需将图像读为 base64 后以 JSON 提交。
pub async fn images_edits(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    images_entry(state, key_id, body, true).await
}

async fn images_entry(
    state: Arc<ApiSharedState>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
    is_edit: bool,
) -> Response {
    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if body.len() > MAX_BODY_BYTES {
        return openai_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "request body exceeds 8MB limit");
    }
    let peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &format!("invalid JSON body: {}", e)),
    };
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("hy4")
        .to_string();
    let prompt = peek.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let image_b64 = peek.get("image").and_then(|v| v.as_str()).map(str::to_string);
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());
    let start_ts = std::time::Instant::now();

    // 校验（目录命中 + 图片模态 + prompt/image 非空）
    let catalog = wb_catalog::load(&state.data_dir);
    if let Err((code, msg)) = super::wb_images::validate(
        &catalog,
        &model,
        &prompt,
        if is_edit { Some(image_b64.as_deref().unwrap_or("")) } else { None },
    ) {
        return openai_error(
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
            "invalid_request_error",
            &msg,
        );
    }

    // 取健康 WB 账号（生图无粘性语义，任一健康账号）
    let picked = {
        let tried = HashSet::new();
        state.wb_pool.pick_excluding_constrained(&tried, None, None)
    };
    let Some(picked) = picked else {
        return openai_error(StatusCode::SERVICE_UNAVAILABLE, "no_healthy_account", "no healthy WB account available");
    };
    let creds = super::wb_upstream::WbCreds {
        id: picked.uid.clone(),
        uid: picked.uid.clone(),
        name: String::new(),
        token: picked.jwt.clone(),
        domain: picked.domain.clone(),
        enterprise_id: picked.enterprise_id.clone(),
        global_region: picked.global_region,
    };

    let result = tokio::task::spawn_blocking(move || super::wb_images::generate(&creds, &peek))
        .await
        .unwrap_or_else(|e| Err((500u16, format!("task join error: {e}"))));
    let duration_ms = start_ts.elapsed().as_millis() as u64;
    match result {
        Ok(resp) => {
            state.record_usage(false, &model, &picked.uid, &key_str, true, false, duration_ms, 0, 0);
            state.wb_pool.note_success(&picked.uid);
            state.logger.log_request(
                "POST",
                if is_edit { "/v1/images/edits" } else { "/v1/images/generations" },
                &model, false, 200, &picked.uid, duration_ms, None,
            );
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(resp.to_string()))
                .unwrap_or_else(|_| internal_error_response())
        }
        Err((code, msg)) => {
            state.record_usage(false, &model, &picked.uid, &key_str, false, false, duration_ms, 0, 0);
            state.logger.log_request(
                "POST",
                if is_edit { "/v1/images/edits" } else { "/v1/images/generations" },
                &model, false, code, &picked.uid, duration_ms, Some(&msg),
            );
            openai_error(
                StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                "upstream_error",
                &msg,
            )
        }
    }
}

// ==================== Streaming ====================

#[allow(clippy::too_many_arguments)]
fn stream_chat(state: Arc<ApiSharedState>, body_vec: Vec<u8>, model: String, stream: bool, start_ts: std::time::Instant, proto: Protocol, key_id: String) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    // SSE keep-alive 15s（T2.7/F-34 §5.5 #7）：防中间层回收长流；
    // 客户端断连/[DONE] 后发送失败自然退出
    {
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            tick.tick().await; // 首个 tick 立即返回，跳过
            loop {
                tick.tick().await;
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

    tokio::task::spawn_blocking(move || {
        let chat_id = match proto {
            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
            Protocol::Anthropic => format!("msg_{}", now_ts()),
            // Responses 仅走 WB 上游；solo 管线不会收到，兜底给 resp_ id
            Protocol::Responses => format!("resp_{}", now_ts()),
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
                    let (error_info, sent_any, up_usage) = match proto {
                        Protocol::OpenAi => {
                            let (e, s, u) = sse::stream_convert(reader, tx.clone(), &chat_id);
                            (e, s, u)
                        }
                        Protocol::OpenAiText => {
                            let (e, s, u) =
                                sse::stream_convert_text(reader, tx.clone(), &chat_id, &model);
                            (e, s, u)
                        }
                        Protocol::Anthropic => {
                            let (e, s, u) =
                                sse::stream_convert_anthropic(reader, tx.clone(), &chat_id, &model);
                            (e, s, u)
                        }
                        // Responses 仅走 WB 上游；solo 管线兜底按 OpenAI 透传
                        Protocol::Responses => {
                            let (e, s, u) = sse::stream_convert(reader, tx.clone(), &chat_id);
                            (e, s, u)
                        }
                    };
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    // 用量记账（流式结束即落盘）
                    {
                        let (pt, ct) = up_usage.as_ref().map(extract_tokens).unwrap_or((0, 0));
                        state.record_usage(
                            false, &model, &picked.uid, &key_id, error_info.is_none(), true,
                            duration_ms, pt, ct,
                        );
                    }
                    if let Some((code, msg)) = error_info {
                        let kind = classify_solo_error(code, &msg);
                        if kind != ErrKind::None {
                            state.pool.note_error(&picked.uid, kind);
                            *safe_lock(&state.last_error) =
                                Some(format!("uid={} code={} msg={}", picked.uid, code, msg));
                        }
                        if !sent_any {
                            // 流未开始：错误延迟下发（sse 层未透传，由这里统一发）
                            send_stream_error(&tx, proto, code, &msg);
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
        state.record_usage(false, &model, "none", &key_id, false, true, duration_ms, 0, 0);
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
            Protocol::OpenAi | Protocol::OpenAiText => {
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
            Protocol::Responses => {
                let body = json!({
                    "type": "response.failed",
                    "response": {
                        "id": format!("resp_{}", now_ts()),
                        "object": "response",
                        "status": "failed",
                        "output": [],
                        "error": {"code": "no_healthy_account", "message": "no healthy account available"},
                    },
                });
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                    "event: response.failed\ndata: {}\n\n",
                    body
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

async fn aggregate_chat(state: Arc<ApiSharedState>, body_vec: Vec<u8>, model: String, stream: bool, start_ts: std::time::Instant, proto: Protocol, key_id: String) -> Response {
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
                        Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
                        Protocol::Anthropic => format!("msg_{}", now_ts()),
                        Protocol::Responses => format!("resp_{}", now_ts()),
                    };
                    let (resp, error_info) = match proto {
                        Protocol::OpenAi => sse::aggregate(reader, &chat_id),
                        Protocol::OpenAiText => sse::aggregate_text(reader, &chat_id, &model),
                        Protocol::Anthropic => sse::aggregate_anthropic(reader, &chat_id, &model),
                        // Responses 仅走 WB 上游；solo 管线兜底按 OpenAI 聚合
                        Protocol::Responses => sse::aggregate(reader, &chat_id),
                    };
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    match (resp, error_info) {
                        (Some(r), None) => {
                            // 用量记账（成功：token 数从聚合响应 usage 提取）
                            let (pt, ct) = r.get("usage").map(extract_tokens).unwrap_or((0, 0));
                            state.record_usage(
                                false, &model, &picked.uid, &key_id, true, stream,
                                duration_ms, pt, ct,
                            );
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
                            state.record_usage(
                                false, &model, &picked.uid, &key_id, false, stream,
                                duration_ms, 0, 0,
                            );
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
                            state.record_usage(
                                false, &model, &picked.uid, &key_id, false, stream,
                                duration_ms, 0, 0,
                            );
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
                    state.record_usage(
                        false, &model, &picked.uid, &key_id, false, stream,
                        start_ts.elapsed().as_millis() as u64, 0, 0,
                    );
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
        // 用量记账（所有账号不可用）
        state.record_usage(false, &model, "none", &key_id, false, stream, duration_ms, 0, 0);
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
            Protocol::OpenAi | Protocol::OpenAiText | Protocol::Responses => {
                openai_error(StatusCode::SERVICE_UNAVAILABLE, "no_healthy_account", &msg)
            }
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

/// OpenAI 错误响应格式（wb_route 复用）
pub(crate) fn openai_error(status: StatusCode, code: &str, msg: &str) -> Response {    let body = json!({
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

/// Anthropic 错误响应格式：{"type":"error","error":{"type","message"}}（wb_route 复用）
pub(crate) fn anthropic_error(status: StatusCode, err_type: &str, msg: &str) -> Response {
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

/// 流式错误统一下发：sse 层在流未开始时不透传错误（留待重试决策），
/// 由此处按客户端协议格式化错误事件并收尾
fn send_stream_error(
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    proto: Protocol,
    code: i64,
    msg: &str,
) {
    match proto {
        Protocol::OpenAi | Protocol::OpenAiText => {
            let body = json!({
                "error": { "message": msg, "type": "api_error", "code": code }
            });
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!("data: {}\n\n", body))));
            let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
        }
        Protocol::Anthropic => {
            let err = json!({
                "type": "error",
                "error": { "type": "api_error", "message": msg },
            });
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: error\ndata: {}\n\n",
                err
            ))));
        }
        Protocol::Responses => {
            let body = json!({
                "type": "response.failed",
                "response": {
                    "id": format!("resp_{}", now_ts()),
                    "object": "response",
                    "status": "failed",
                    "output": [],
                    "error": {"code": code.to_string(), "message": msg},
                },
            });
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: response.failed\ndata: {}\n\n",
                body
            ))));
        }
    }
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
