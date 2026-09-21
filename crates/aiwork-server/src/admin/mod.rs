//! 管理面模块（Web 化改造 T5/T6/T7 装配）：
//! - T7 鉴权：token 加载/生成（env `AIWORK_ADMIN_TOKEN` 优先 → conf/admin_token）
//!   + Cookie 会话中间件（`/api/login` 放行）；
//! - T5 命令桥：`POST /api/cmd/{command}` 白名单分发到 aiwork-core（cmd_bridge）；
//! - T6 SSE：`GET /api/events/checkin` 签到事件流（sse）。
//!
//! ADR-4：单管理员 token 模型——登录换取 HttpOnly Cookie，后续请求凭 Cookie 鉴权；
//! 鉴权失败统一 401 `{"ok":false,"error":...}`，不产生逐请求噪音日志。

mod admin_tokens;
mod cmd_bridge;
pub(crate) mod ip_allow;
mod sse;
mod ws;

use std::sync::Arc;

use aiwork_core::commands::checkin::CheckinGuard;
use aiwork_core::fs_utils;
use aiwork_core::state::AppState;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;

/// 会话 Cookie 名
const COOKIE_NAME: &str = "aiwork_admin";
/// 会话有效期：7 天（秒）
const COOKIE_MAX_AGE_SECS: u32 = 604_800;

/// 管理面共享状态
pub struct AdminState {
    pub state: std::sync::Arc<aiwork_core::state::AppState>,
    pub guard: std::sync::Arc<aiwork_core::commands::checkin::CheckinGuard>,
    /// 签到事件广播：(事件名, 载荷)。事件名 = "checkin-progress" | "checkin-done"
    /// （WorkBuddy 管线事件 wb-checkin-progress / wb-oauth-* 亦经此通道转发，SSE 客户端按事件名过滤）
    pub events: tokio::sync::broadcast::Sender<(String, serde_json::Value)>,
    /// 管理面 token（env AIWORK_ADMIN_TOKEN 优先；否则 conf/admin_token 读取/生成）
    pub token: String,
}

impl AdminState {
    /// 装配：token 加载/生成 + 广播通道（容量 256）
    pub fn new(state: std::sync::Arc<AppState>) -> std::sync::Arc<Self> {
        let token = load_or_create_token(&state);
        // 附加管理员令牌缓存（T12b）：登录/鉴权并集校验的附加集
        admin_tokens::reload(&state);
        // 容量 256：慢消费者（SSE 端 lag）丢帧不阻塞签到工作线程，
        // done 事件已落库 checkin_results，前端可降级轮询
        let (events, _rx) = tokio::sync::broadcast::channel(256);
        Arc::new(Self {
            state,
            guard: Arc::new(CheckinGuard(tokio::sync::Mutex::new(()))),
            events,
            token,
        })
    }
}

/// token 加载/生成：
/// 1. env `AIWORK_ADMIN_TOKEN` 非空即用（容器部署）；
/// 2. 读 `<data_dir>/conf/admin_token`（已存在且非空直接复用）；
/// 3. 生成 32 字节随机 hex（64 字符，OsRng，与 core::oauth::random_hex 同源实现）
///    写入文件（unix 下权限 0600；Windows 跳过权限设置），并打日志提示路径。
fn load_or_create_token(state: &AppState) -> String {
    // 1) 环境变量优先
    if let Ok(t) = std::env::var("AIWORK_ADMIN_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    // 2) 已有 token 文件
    let path = state.conf_path("admin_token");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    // 3) 首次生成并落盘（写失败不影响本次进程内可用，仅重启后需重新登录）
    let token = aiwork_core::commands::oauth::random_hex(64);
    match std::fs::write(&path, &token) {
        Ok(()) => {
            // unix 下收紧权限：token 等同口令，仅属主可读写
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            let msg = format!("管理面 token 已生成: {}", path.display());
            println!("{msg}");
            fs_utils::app_log(&state.data_dir, &msg);
        }
        Err(e) => {
            let msg = format!("管理面 token 写入失败（{}）: {e}", path.display());
            println!("{msg}");
            fs_utils::app_log(&state.data_dir, &msg);
        }
    }
    token
}

/// 从 Cookie 头手工解析指定 cookie 的值（不引第三方 cookie 库）。
/// 形态：`name1=value1; name2=value2`（容忍分号/等号两侧空白；值不支持引号包裹）。
fn cookie_value(raw: &str, name: &str) -> Option<String> {
    raw.split(';')
        .map(|p| p.trim())
        .find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            k.trim()
                .eq(name)
                .then(|| v.trim().to_string())
        })
}

/// 统一 JSON 响应（内容类型 application/json；正文为紧凑 JSON 字符串）
pub(super) fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    let body = serde_json::to_string(&value)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"序列化失败"}"#.to_string());
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// T7 鉴权中间件：覆盖管理面路由整体；`/api/login` 放行，
/// 其余请求校验 Cookie `aiwork_admin=<token>` 精确匹配，失败 → 401。
pub(super) async fn auth_middleware(
    State(admin): State<Arc<AdminState>>,
    req: Request,
    next: Next,
) -> Response {
    // 登录端点放行（其余路径一律鉴权）
    if req.uri().path() == "/api/login" {
        return next.run(req).await;
    }
    let authorized = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| cookie_value(raw, COOKIE_NAME))
        // 主 token 精确匹配 或 附加管理员令牌命中（T12b）
        .map(|c| c == admin.token || admin_tokens::contains(&c))
        .unwrap_or(false);
    if authorized {
        next.run(req).await
    } else {
        json_response(
            StatusCode::UNAUTHORIZED,
            json!({"ok": false, "error": "未登录或会话已过期"}),
        )
    }
}

/// POST /api/login：body `{"token": "..."}`；匹配 → 200 `{"ok":true}` +
/// Set-Cookie 会话（HttpOnly / SameSite=Lax / 7 天）；不匹配 → 401。
async fn login(State(admin): State<Arc<AdminState>>, body: Bytes) -> Response {
    let supplied = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("token")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    // 主 token 或附加管理员令牌任一命中即可登录（T12b）；会话 Cookie 存登录所用 token
    if supplied != admin.token && !admin_tokens::contains(&supplied) {
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({"ok": false, "error": "token 无效"}),
        );
    }
    // 登录事件单条日志（无逐请求鉴权噪音）
    fs_utils::app_log(&admin.state.data_dir, "管理面登录成功");
    // Cookie 存登录所用 token（附加令牌登录时不得回写主 token，避免泄露）
    let cookie = format!(
        "{COOKIE_NAME}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={COOKIE_MAX_AGE_SECS}",
        supplied
    );
    let mut resp = json_response(StatusCode::OK, json!({"ok": true}));
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

/// 管理面路由（含鉴权中间件层）：
/// `POST /api/login` + `POST /api/cmd/{command}` + `GET /api/events/checkin`
pub fn router(admin: Arc<AdminState>) -> Router {
    let api = Router::new()
        .route("/api/login", post(login))
        .route("/api/cmd/:command", post(cmd_bridge::cmd_handler))
        .route("/api/events/checkin", get(sse::checkin_events))
        // WebSocket 双向推送（T12c）：鉴权层覆盖后再升级
        .route("/api/ws", get(ws::ws_handler))
        .with_state(admin.clone());
    // 鉴权层覆盖整个 api（login 在中间件内放行）
    api.layer(middleware::from_fn_with_state(admin.clone(), auth_middleware))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cookie 解析往返：标准 / 多 cookie / 顺序无关 / 空白容忍 / 缺失
    #[test]
    fn cookie_parse_roundtrip() {
        assert_eq!(
            cookie_value("aiwork_admin=abc123", COOKIE_NAME).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            cookie_value("theme=dark; aiwork_admin=abc123", COOKIE_NAME).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            cookie_value("aiwork_admin=abc123; theme=dark", COOKIE_NAME).as_deref(),
            Some("abc123")
        );
        // 容忍等号/分号两侧空白
        assert_eq!(
            cookie_value("aiwork_admin = spaced ; theme=dark", COOKIE_NAME).as_deref(),
            Some("spaced")
        );
        // 前缀同名 cookie 不误匹配
        assert_eq!(
            cookie_value("aiwork_admin_x=other; theme=dark", COOKIE_NAME),
            None
        );
        assert_eq!(cookie_value("theme=dark", COOKIE_NAME), None);
        assert_eq!(cookie_value("", COOKIE_NAME), None);
    }

    /// 会话 cookie 帧格式：HttpOnly + SameSite=Lax + Path=/ + Max-Age=7 天
    #[test]
    fn login_cookie_frame_format() {
        let cookie = format!(
            "{COOKIE_NAME}=<token>; HttpOnly; SameSite=Lax; Path=/; Max-Age={COOKIE_MAX_AGE_SECS}"
        );
        assert_eq!(
            cookie,
            "aiwork_admin=<token>; HttpOnly; SameSite=Lax; Path=/; Max-Age=604800"
        );
    }
}
