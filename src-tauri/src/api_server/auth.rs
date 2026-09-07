use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::ApiSharedState;

/// API Key 鉴权中间件：
/// - /health 跳过鉴权
/// - api_key 为空时跳过鉴权
/// - 否则校验 Authorization: Bearer <key>（OpenAI 风格）或 x-api-key: <key>（Anthropic 风格）
pub async fn bearer_auth(
    State(state): State<Arc<ApiSharedState>>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }
    if state.api_key.is_empty() {
        return next.run(request).await;
    }
    let authz = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    if let Some(s) = authz {
        if s.len() > 7 && s[..7].eq_ignore_ascii_case("Bearer ") && &s[7..] == state.api_key {
            return next.run(request).await;
        }
    }
    // Anthropic 客户端使用 x-api-key 头
    let xkey = request.headers().get("x-api-key").and_then(|v| v.to_str().ok());
    if let Some(key) = xkey {
        if key == state.api_key {
            return next.run(request).await;
        }
    }
    if authz.is_none() && xkey.is_none() {
        (StatusCode::UNAUTHORIZED, "missing api key").into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "invalid api key").into_response()
    }
}
