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
/// - 存在启用 Key 时必须鉴权；无任何启用 Key 时：auth_disabled=true（显式关闭鉴权，UI 标注风险）
///   放行并记为 anonymous，否则拒绝（默认，fail-closed）并返回引导提示
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

    // 未配置任何启用的 Key：显式关闭鉴权时放行记 anonymous（用户主动选择，UI 已标注风险）；
    // 否则默认拒绝（fail-closed，防本机任意进程无鉴权借用上游凭证消耗额度，S2 missing_authz）
    let mut keys: ApiKeysFile = api_keys::load(&state.data_dir);
    if !keys.has_enabled() {
        if keys.auth_disabled {
            request.extensions_mut().insert(KeyId("anonymous".into()));
            return next.run(request).await;
        }
        return auth_not_configured();
    }

    let Some(presented) = presented else {
        return (StatusCode::UNAUTHORIZED, "missing api key").into_response();
    };

    // 校验 Key（含每日配额）
    match keys.verify_and_consume(&presented, &super::usage::today_key()) {
        KeyCheck::Ok(id) => {
            api_keys::save(&state.data_dir, &keys);
            request.extensions_mut().insert(KeyId(id));
            next.run(request).await
        }
        KeyCheck::QuotaExceeded { limit } => quota_exceeded(limit),
        KeyCheck::Invalid => (StatusCode::UNAUTHORIZED, "invalid api key").into_response(),
    }
}

/// 401 未配置鉴权响应：无启用 Key 且未显式关闭鉴权。
/// JSON 错误体给出两条出路（创建启用 Key / 显式关闭鉴权），OpenAI/Anthropic 客户端均可解析 message。
fn auth_not_configured() -> Response {
    let body = json!({
        "error": {
            "message": "API 服务已启用鉴权：请在应用的「API 服务」页创建并启用 API Key，\
                        或在该页显式关闭鉴权（不推荐，任何本机程序均可调用）",
            "type": "invalid_request_error",
            "code": "api_key_required",
        }
    });
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
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
