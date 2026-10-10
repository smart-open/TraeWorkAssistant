//! OAuth 本机回环监听器（F-78 批次 1，issue #10 根因②）。
//! redirect_uri 为 http://127.0.0.1:17388/authorize，但原实现无进程监听该端口，
//! 浏览器完成授权后回调无法落地，用户只能手动复制地址栏 URL。
//! 本模块在发起登录前于回环地址起一个短生命周期 HTTP server：
//! - 收到回调后复用 oauth_parse_callback / oauth_login 完成解析与落库；
//! - 通过 `oauth-login-done` 事件通知前端自动收尾（关闭弹窗、刷新账号列表）；
//! - 5 分钟空闲自动关闭；oauth_stop_loopback 主动停止；登录完成（无论成败）自动关闭。
//! 同时联动 device_proxy::bypass（批次 2）：监听期间把 OAuth 域名加入系统代理直连白名单，
//! 避免 MITM 代理解密浏览器 OAuth 流量导致 ERR_CERT_AUTHORITY_INVALID。

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::extract::State as AxumState;
use axum::http::Uri;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::commands::oauth::{oauth_login, OAUTH_LOOPBACK_PORT, OAUTH_REDIRECT_URI};
use crate::fs_utils;
use crate::state::AppState;

/// 空闲超时：5 分钟内没有任何请求（含回调）即自动关闭监听
const IDLE_TIMEOUT_SECS: i64 = 300;
/// 空闲看门狗巡检间隔
const IDLE_CHECK_INTERVAL_SECS: i64 = 30;

/// 前端 `oauth-login-done` 事件负载
#[derive(Serialize, Clone)]
pub struct OAuthLoginDoneEvent {
    pub ok: bool,
    pub message: String,
    /// 登录成功时的账号备注名（合并场景为实际被更新的已有账号名）
    pub account: Option<String>,
    /// 登录成功时的 user_id
    pub user_id: Option<String>,
    /// 登录成功时标记是否与已有账号同 uid（合并更新而非新增）；失败时为 None
    pub merged: Option<bool>,
}

/// 回环监听器运行句柄（全局唯一；重启登录时先停旧的）
struct LoopbackHandle {
    /// 发送 true 触发 graceful shutdown
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

fn loopback_handle() -> &'static Mutex<Option<LoopbackHandle>> {
    static HANDLE: OnceLock<Mutex<Option<LoopbackHandle>>> = OnceLock::new();
    HANDLE.get_or_init(|| Mutex::new(None))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// axum handler 共享状态
struct LoopbackState {
    app: AppHandle,
    account_name: Option<String>,
    group_id: Option<String>,
    /// 最近一次请求时间（秒），用于空闲超时
    last_active: Arc<AtomicI64>,
    /// 登录流程结束后由 handler 自发触发关闭
    shutdown_tx: watch::Sender<bool>,
}

/// GET /authorize?...：OAuth 回调落地
///
/// 复用 oauth_login（内含 oauth_parse_callback → exchange_token → get_user_info → 落库），
/// 网络请求为阻塞式（ureq，最多 2×120s），放入 spawn_blocking 避免卡住 tokio worker。
async fn handle_authorize(AxumState(st): AxumState<Arc<LoopbackState>>, uri: Uri) -> Html<String> {
    st.last_active.store(now_secs(), Ordering::Relaxed);

    // 还原完整回调 URL（oauth_parse_callback 只关心 query，补上协议主机即可）
    let path_query = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/authorize".to_string());
    let callback_url = format!("http://127.0.0.1:{}{}", OAUTH_LOOPBACK_PORT, path_query);

    let app = st.app.clone();
    let account_name = st.account_name.clone();
    let group_id = st.group_id.clone();
    let url = callback_url.clone();
    let result = tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let runtime = app.state::<std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>();
        oauth_login(state, runtime, url, account_name, group_id)
    })
    .await;

    let (ok, message, account, user_id, merged) = match result {
        Ok(Ok(r)) => {
            if r.merged {
                // 同 uid 合并：文案与账号名都指向实际被更新的已有账号，避免误报「新增」（issue #80）
                let actual = r.existing_name.clone().unwrap_or_else(|| r.name.clone());
                (
                    true,
                    format!("账号 [{actual}] 登录成功（与本次登录同 uid，已更新其凭证，未新增账号）"),
                    Some(actual),
                    Some(r.user_id),
                    Some(true),
                )
            } else {
                (
                    true,
                    format!("账号 [{}] 登录成功", r.name),
                    Some(r.name),
                    Some(r.user_id),
                    Some(false),
                )
            }
        }
        Ok(Err(e)) => (false, e, None, None, None),
        Err(e) => (false, format!("登录任务执行异常: {e}"), None, None, None),
    };

    // 通知前端（自动收尾：ok=true 关闭弹窗刷新列表；ok=false 提示走手动粘贴兜底）
    let _ = st.app.emit(
        "oauth-login-done",
        &OAuthLoginDoneEvent {
            ok,
            message: message.clone(),
            account,
            user_id,
            merged,
        },
    );

    // 登录流程已结束，自动关闭监听（释放端口；下一次登录由 oauth_start_loopback 重启）
    let _ = st.shutdown_tx.send(true);

    html_response(ok, merged.unwrap_or(false), &message, &callback_url)
}

/// 浏览器展示的中文结果页（成功：可关闭此页；失败：提示手动复制 URL 兜底）
///
/// 安全（P0 缺陷6）：`message` 含上游错误详情、`callback_url` 含浏览器可控的
/// path/query，均必须 HTML 转义后再插值，防回调页 XSS
fn html_response(ok: bool, merged: bool, message: &str, callback_url: &str) -> Html<String> {
    // HTML 实体转义：&, <, >, ", '（属性与文本上下文均覆盖）
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")
    }
    let (icon, title, color) = if ok {
        ("✓", "登录成功", "#16a34a")
    } else {
        ("✕", "登录失败", "#dc2626")
    };
    let body = if ok {
        // 同 uid 合并场景如实表述「同步更新」而非「已添加」（issue #80）
        let tail = if merged {
            "账号信息已同步到 AIWorkAssistant，可关闭此页面返回应用。"
        } else {
            "账号已添加到 AIWorkAssistant，可关闭此页面返回应用。"
        };
        format!(
            "<p>{}</p>\
             <p>{}</p>",
            esc(message),
            esc(tail)
        )
    } else {
        format!(
            "<p>{}</p>\
             <p>可改用手动兜底：复制浏览器<b>地址栏中的完整 URL</b>，回到 AIWorkAssistant 的 OAuth 登录窗口粘贴提交。</p>\
             <p class=\"url\">{}</p>",
            esc(message),
            esc(callback_url)
        )
    };
    Html(format!(
        "<!DOCTYPE html>\
         <html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Trae 账号登录</title>\
         <style>\
           body {{ font-family: system-ui, -apple-system, 'Segoe UI', 'Microsoft YaHei', sans-serif; \
                  display: flex; align-items: center; justify-content: center; min-height: 100vh; margin: 0; background: #f6f7f9; }}\
           .card {{ background: #fff; border-radius: 12px; padding: 40px 48px; max-width: 560px; \
                    box-shadow: 0 4px 16px rgba(0,0,0,.08); text-align: center; }}\
           .icon {{ width: 56px; height: 56px; border-radius: 50%; color: #fff; font-size: 28px; \
                     line-height: 56px; margin: 0 auto 16px; background: {color}; }}\
           h1 {{ font-size: 20px; margin: 0 0 12px; }}\
           p {{ color: #555; line-height: 1.7; margin: 6px 0; }}\
           .url {{ word-break: break-all; background: #f1f5f9; border-radius: 6px; padding: 8px 12px; \
                   font-size: 12px; color: #334155; text-align: left; }}\
         </style></head>\
         <body><div class=\"card\">\
           <div class=\"icon\">{icon}</div>\
           <h1>{title}</h1>\
           {body}\
         </div></body></html>"
    ))
}

/// 启动本机回环监听器（redirect_uri = http://127.0.0.1:17388/authorize）。
/// 幂等：重复调用先停止旧监听再重启；端口被占用返回明确错误（前端降级为手动粘贴兜底）。
#[tauri::command]
pub async fn oauth_start_loopback(
    app: AppHandle,
    state: State<'_, AppState>,
    account_name: Option<String>,
    group_id: Option<String>,
) -> Result<(), String> {
    // 已在监听 → 先停旧的（等待端口释放，最多 2 秒；仍未结束则由下方 bind 的明确报错兜底）
    stop_loopback().await;

    // 代理豁免（批次 2）：MITM 代理运行时把 OAuth 域名加入系统代理直连白名单，
    // 避免 OAuth 登录页被自签 CA 解密报证书错误；失败不阻断登录（可能未开代理）
    if let Err(e) = crate::device_proxy::bypass::enable_oauth_bypass(&state.data_dir) {
        fs_utils::app_log(&state.data_dir, &format!("[OAuth] 设置代理直连豁免失败（不阻断登录）: {e}"));
    }

    let addr = format!("127.0.0.1:{}", OAUTH_LOOPBACK_PORT);
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!(
                "OAuth 回调端口 {} 已被其他程序占用（{e}），自动收尾不可用；请在下一步改用手动粘贴回调 URL 兜底",
                OAUTH_LOOPBACK_PORT
            )
        } else {
            format!("OAuth 回调监听启动失败: {e}")
        }
    })?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let last_active = Arc::new(AtomicI64::new(now_secs()));
    let st = Arc::new(LoopbackState {
        app: app.clone(),
        account_name,
        group_id,
        last_active: last_active.clone(),
        shutdown_tx: shutdown_tx.clone(),
    });

    let router = Router::new()
        .route("/authorize", get(handle_authorize))
        .with_state(st);

    // graceful shutdown：主动停止（oauth_stop_loopback / 登录完成）或 5 分钟空闲超时
    let mut rx = shutdown_rx.clone();
    let la = last_active.clone();
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        tokio::select! {
            // 任意一次 send(true) 或所有 sender drop 均触发
            _ = rx.changed() => {}
            _ = idle_watchdog(la) => {}
        }
    });
    let join = tokio::spawn({
        let data_dir = state.data_dir.clone();
        async move {
            if let Err(e) = server.await {
                fs_utils::app_log(&data_dir, &format!("[OAuth] 回环监听器异常退出: {e}"));
            }
        }
    });

    *loopback_handle()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(LoopbackHandle { shutdown_tx, join });

    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "[OAuth] 回环监听已启动 {}（空闲 {} 秒自动关闭，redirect_uri={OAUTH_REDIRECT_URI}）",
            addr, IDLE_TIMEOUT_SECS
        ),
    );
    Ok(())
}

/// 停止本机回环监听器并还原代理豁免（弹窗关闭/组件卸载/登录完成时调用；未启动时幂等成功）
#[tauri::command]
pub async fn oauth_stop_loopback(state: State<'_, AppState>) -> Result<(), String> {
    stop_loopback().await;
    if let Err(e) = crate::device_proxy::bypass::disable_oauth_bypass(&state.data_dir) {
        fs_utils::app_log(&state.data_dir, &format!("[OAuth] 还原代理直连配置失败: {e}"));
    }
    Ok(())
}

/// 停止运行中的监听（如有）：发送 shutdown 并最多等待 2 秒让端口释放
async fn stop_loopback() {
    let Some(h) = loopback_handle()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    else {
        return;
    };
    let _ = h.shutdown_tx.send(true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !h.join.is_finished() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 空闲看门狗：每 30 秒巡检一次，距最近请求超过 5 分钟即触发 graceful shutdown
async fn idle_watchdog(last_active: Arc<AtomicI64>) {
    loop {
        tokio::time::sleep(Duration::from_secs(IDLE_CHECK_INTERVAL_SECS as u64)).await;
        if now_secs() - last_active.load(Ordering::Relaxed) >= IDLE_TIMEOUT_SECS {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_response_escapes_message_and_url() {
        // P0 缺陷6 回归：script 注入必须被实体转义，不得原样出现在 HTML 中
        let html = html_response(
            false,
            false,
            "兑换失败: <script>alert(1)</script>",
            "http://127.0.0.1:17388/authorize?code=ab&state=<img src=x onerror=alert(2)>",
        )
        .0;
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&lt;img src=x onerror=alert(2)&gt;"));
        // 正常中文消息与 URL 参数不受影响
        assert!(html.contains("登录失败"));
        assert!(html.contains("code=ab&amp;state="));
    }

    #[test]
    fn html_response_success_page() {
        // 新增场景：第二段文案为「已添加」
        let html = html_response(true, false, "账号 [测试] 登录成功", "").0;
        assert!(html.contains("登录成功"));
        assert!(html.contains("账号 [测试] 登录成功"));
        assert!(html.contains("账号已添加到 AIWorkAssistant"));
        // 同 uid 合并场景（issue #80）：第二段文案为「已同步」，不得再出现「已添加」
        let merged_html = html_response(
            true,
            true,
            "账号 [测试] 登录成功（与本次登录同 uid，已更新其凭证，未新增账号）",
            "",
        )
        .0;
        assert!(merged_html.contains("账号信息已同步到 AIWorkAssistant"));
        assert!(!merged_html.contains("账号已添加到"));
    }
}
