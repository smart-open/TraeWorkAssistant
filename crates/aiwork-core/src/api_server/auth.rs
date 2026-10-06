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
/// - 存在启用 Key 时必须鉴权；无任何启用 Key 时：auth_disabled=true（显式关闭鉴权）放行记 anonymous，
///   否则拒绝（默认）并返回引导提示；非环回监听时忽略该开关（双门禁，见 listen_is_loopback）
/// - Key 每次命中即累加当日用量并写盘，超配额返回 429
/// - 每次请求重读 api_keys.json：新增/删除/禁用/开关立即生效
/// 校验通过后向 request extensions 插入命中的 Key 标识（KeyId），供 handler 用量记账
pub async fn bearer_auth(
    State(state): State<Arc<ApiSharedState>>,
    mut request: Request,
    next: Next,
) -> Response {
    // 网关状态页免鉴权：页面为静态 HTML，/health 仅输出聚合探活级汇总
    // （池计数 / 通用积分合计 / 今日 token 三池合计，无账号级明细）；
    // 账号明细与模型目录由浏览器另行请求鉴权端点获得
    if request.uri().path() == "/gw-status" || request.uri().path() == "/health" {
        return next.run(request).await;
    }

    // 请求体预检（issue #21）：Content-Length 超过网关上限时在 body 提取器之前
    // 返回结构化 413（axum 提取器超限默认回纯文本，客户端难以解析识别）；
    // 无 Content-Length（chunked）的超限请求仍由 DefaultBodyLimit 兜底拒绝
    if let Some(len) = request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        if len > super::routes::MAX_BODY_BYTES {
            log_reject(&state, &request, 413, "body too large");
            return body_too_large();
        }
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

    // 鉴权热路径（P1 修复5b）：load + verify_and_consume_locked（内含记账写盘）
    // 全为同步磁盘 IO，整体移入 spawn_blocking（参数转 owned），避免阻塞
    // async 调度线程；锁与原子语义不变（verify_and_consume_locked 进程级锁内完成）
    let (auth_required, check) = {
        let data_dir = state.data_dir.clone();
        tokio::task::spawn_blocking(move || {
            let keys: ApiKeysFile = api_keys::load(&data_dir);
            // 双门禁（9ba5fd0 移植）：存在启用 Key 时必须鉴权；无启用 Key 时由
            // 显式开关决定放行或拒绝；非环回监听（局域网/公网/容器 0.0.0.0）时
            // 忽略 auth_disabled 显式开关——付费上游代理不允许无鉴权暴露，
            // 仅环回（本机自用）可关鉴权
            let auth_required =
                keys.has_enabled() || !keys.auth_disabled || !listen_is_loopback();
            let check = presented.map(|p| {
                api_keys::verify_and_consume_locked(&data_dir, &p, &super::usage::today_key())
            });
            (auth_required, check)
        })
        .await
        // join 失败（panic 等）按最严格处理：要求鉴权 + 视为无效 Key → 401
        .unwrap_or((true, Some(KeyCheck::Invalid)))
    };

    match check {
        Some(KeyCheck::Ok(rk)) => {
            let id = rk.id.clone();
            request.extensions_mut().insert(rk);
            request.extensions_mut().insert(KeyId(id));
            return next.run(request).await;
        }
        Some(KeyCheck::QuotaExceeded { limit }) => {
            log_reject(&state, &request, 429, &format!("quota exceeded (limit {limit}/day)"));
            return quota_exceeded(limit);
        }
        Some(KeyCheck::Invalid) => {}
        None => {
            // 未携带 Key
            if !auth_required {
                request.extensions_mut().insert(KeyId("anonymous".into()));
                return next.run(request).await;
            }
            log_reject(&state, &request, 401, "no api key presented");
            return auth_required_rejected().into_response();
        }
    }
    // 无启用 Key 且显式关闭鉴权：放行携带未知 Key 的请求并记为 anonymous
    if !auth_required {
        request.extensions_mut().insert(KeyId("anonymous".into()));
        return next.run(request).await;
    }
    // 无效 Key 拒绝（fail-closed）：补记请求日志（此前黑盒，客户端侧报错但
    // 请求日志零记录，难以区分 Key 填错 / 地址路径错误 / 服务未达）
    log_reject(&state, &request, 401, "invalid api key");
    (StatusCode::UNAUTHORIZED, "invalid api key").into_response()
}

/// 鉴权层拒绝日志：请求未进入业务 handler，此前不留任何记录，导致客户端
/// 报错（如 Trae「empty content 502」）时无法区分 Key 无效 / 未携带 / 超限。
/// model 未知（body 未解析）记 "-"，uid 记 "-"；reason 写入 error 字段。
fn log_reject(state: &ApiSharedState, request: &Request, status: u16, reason: &str) {
    state.logger.log_request(
        "-",
        request.method().as_str(),
        request.uri().path(),
        "-",
        false,
        status,
        "-",
        0,
        "",
        "",
        Some(reason),
    );
}

/// 413 请求体超限响应（issue #21）：结构化 JSON 错误体（OpenAI/Anthropic 客户端
/// 均可解析 message），替代 axum 提取器的纯文本 413
fn body_too_large() -> Response {
    let body = json!({
        "error": {
            "message": super::routes::body_too_large_msg(),
            "type": "invalid_request_error",
            "code": "request_too_large",
        }
    });
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// 401：要求鉴权但不满足（未配置启用 Key 且未显式关闭鉴权，或未携带 Key）。
/// JSON 错误体给出下一步指引（OpenAI/Anthropic 客户端均可解析 message）。
fn auth_required_rejected() -> axum::response::Response {
    let body = json!({
        "error": {
            "message": "API 服务已启用鉴权：请在应用的「API 服务」页创建并启用 API Key，\
                        或在该页显式关闭鉴权（仅监听 127.0.0.1 时可关：非环回地址监听时\
                        为防止付费账号被匿名调用，关闭鉴权开关不生效）",
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

/// 网关监听地址是否为环回（9ba5fd0 双门禁的运行期侧）：解析 AIWORK_LISTEN_ADDR
/// （aiwork-server main 绑定用同一变量；缺省按 127.0.0.1 环回）。监听地址进程
/// 生命周期内不变，OnceLock 缓存。非环回（0.0.0.0 / :: / 具名网卡 / 局域网 IP）
/// 时 bearer_auth 忽略 auth_disabled 显式开关，防止容器/公网部署下网关裸奔代理
/// 付费账号。
pub fn listen_is_loopback() -> bool {
    static LOOPBACK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LOOPBACK.get_or_init(|| {
        // 与 main.rs 绑定侧同口径：显式空白视为未设置（缺省环回），避免
        // 「绑到 127.0.0.1 默认端口而双门禁按原始空白串误判非环回」的分叉
        let addr = std::env::var("AIWORK_LISTEN_ADDR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_default();
        host_is_loopback(&addr)
    })
}

/// 环回地址判定（纯函数，便于单测）：兼容 `host:port` / `[v6]:port` / `[v6]` /
/// 裸 IPv4·域名 / 裸 IPv6（如 `::1`）/ 空串（缺省 = 127.0.0.1 环回）。
/// 裸 IPv6 判据 = 冒号多于 1 个（此时整串即 host，不能按 host:port 拆——
/// `::1` 会被 `rsplit_once(':')` 拆出 host="::" 而误判非环回）
fn host_is_loopback(addr: &str) -> bool {
    let host = if addr.starts_with('[') {
        // `[v6]:port` / `[v6]`：取 ] 之前的 v6 字面量
        addr.split(']')
            .next()
            .unwrap_or(addr)
            .trim_start_matches('[')
    } else if addr.matches(':').count() > 1 {
        // 裸 IPv6（含缩写 `::`）：整体即 host
        addr
    } else {
        // `host:port`（1 个冒号）或裸 host（无冒号）
        addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr)
    };
    host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        // IPv6 环回等价写法（对齐上游白名单）：全写形式与 IPv4-mapped
        // （点分十进制与十六进制两种形式）
        || host == "0:0:0:0:0:0:0:1"
        || host.eq_ignore_ascii_case("::ffff:127.0.0.1")
        || host.eq_ignore_ascii_case("::ffff:7f00:1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// listen_is_loopback 解析回归：host:port / [v6]:port / [v6] / 裸 host /
    /// 裸 IPv6 / 缺省（OnceLock 进程级缓存无法按测试改 env，故拆纯函数
    /// host_is_loopback 单测，listen_is_loopback 仅做 env 组装 + 缓存）
    #[test]
    fn loopback_host_classification() {
        assert!(host_is_loopback("127.0.0.1:8080"));
        assert!(host_is_loopback("localhost:8080"));
        assert!(host_is_loopback("[::1]:8080"));
        assert!(host_is_loopback("[::1]"));
        assert!(host_is_loopback("::1"));
        assert!(host_is_loopback("127.0.0.1"));
        // IPv6 环回等价写法（全写形式 / IPv4-mapped，对齐上游白名单）
        assert!(host_is_loopback("0:0:0:0:0:0:0:1"));
        assert!(host_is_loopback("[::ffff:127.0.0.1]:8080"));
        assert!(host_is_loopback("::ffff:127.0.0.1"));
        assert!(host_is_loopback("::ffff:7f00:1"));
        assert!(host_is_loopback(""));
        assert!(!host_is_loopback("0.0.0.0:8080"));
        assert!(!host_is_loopback("[::]:8080"));
        assert!(!host_is_loopback("[::]"));
        assert!(!host_is_loopback("192.168.1.10:8080"));
        assert!(!host_is_loopback("example.com:8080"));
        // 裸非环回 v6（冒号 >1 且非 ::1）不误判为环回
        assert!(!host_is_loopback("fe80::1"));
    }
}
