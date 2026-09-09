use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::api_keys::{self, ApiKeysFile, KeyCheck};
use super::usage::KeyId;
use super::ApiSharedState;

/// API Key 鉴权中间件：
/// - /health 跳过鉴权
/// - Key 统一在 data/api_keys.json 列表中维护（带每日配额），
///   支持 Authorization: Bearer <key>（OpenAI 风格）或 x-api-key: <key>（Anthropic 风格）
/// - 未配置任何启用的 Key 时不鉴权：一律放行并记为 anonymous（携带未知 Key 亦放行，兼容关闭鉴权场景）
/// - Key 每次命中即累加当日用量并写盘，超配额返回 429
/// - 每次请求重读 api_keys.json：新增/删除/禁用立即生效
/// 校验通过后向 request extensions 插入命中的 Key 标识（KeyId），供 handler 用量记账
pub async fn bearer_auth(
    State(state): State<Arc<ApiSharedState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }

    // 提取呈现的 Key（Bearer 优先，其次 x-api-key）
    let authz = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    let bearer = authz
        .filter(|s| s.len() > 7 && s[..7].eq_ignore_ascii_case("Bearer "))
        .map(|s| s[7..].to_string());
    let xkey = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let presented = bearer.or(xkey);

    // 存在启用的 Key 时才要求鉴权
    let mut keys: ApiKeysFile = api_keys::load(&state.data_dir);
    let auth_required = keys.has_enabled();

    let Some(presented) = presented else {
        if !auth_required {
            request.extensions_mut().insert(KeyId("anonymous".into()));
            return next.run(request).await;
        }
        return (StatusCode::UNAUTHORIZED, "missing api key").into_response();
    };

    // 校验 Key（含每日配额）
    match keys.verify_and_consume(&presented, &super::usage::today_key()) {
        KeyCheck::Ok(id) => {
            api_keys::save(&state.data_dir, &keys);
            request.extensions_mut().insert(KeyId(id));
            return next.run(request).await;
        }
        KeyCheck::QuotaExceeded { limit } => {
            return quota_exceeded(limit);
        }
        KeyCheck::Invalid => {}
    }
    // 未配置任何鉴权时放行携带未知 Key 的请求并记为 anonymous：
    // 与旧版「无 Key 即全放行」行为一致，兼容用户关闭鉴权后客户端仍带着旧 Key 的场景
    if !auth_required {
        request.extensions_mut().insert(KeyId("anonymous".into()));
        return next.run(request).await;
    }

    (StatusCode::UNAUTHORIZED, "invalid api key").into_response()
}

/// 429 配额超限响应（JSON 错误体，OpenAI/Anthropic 客户端均可解析 message）
fn quota_exceeded(limit: u64) -> Response {
    let body = json!({
        "error": {
            "message": format!("API Key 已达今日配额上限（{limit} 次/日），请明天再试或调整限额"),
            "type": "quota_exceeded",
            "code": "daily_quota_exceeded",
        }
    });
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}
