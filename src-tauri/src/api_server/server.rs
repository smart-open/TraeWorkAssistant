use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::auth;
use super::routes;
use super::ApiSharedState;

/// API 服务器句柄：用于优雅停止
pub struct ApiServerHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl ApiServerHandle {
    /// 发送 shutdown 信号并 abort 线程
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.join_handle.take() {
            h.abort();
        }
    }
}

impl Drop for ApiServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动 axum HTTP 服务器
///
/// 使用 Tauri 内置 tokio runtime，不新建 runtime。
pub async fn start_api_server(
    port: u16,
    state: Arc<ApiSharedState>,
) -> Result<ApiServerHandle, String> {
    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("端口 {} 绑定失败: {}", port, e))?;

    let app = build_router(state.clone());
    spawn_wb_health_probe(state.clone());
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    });

    let join_handle = tokio::spawn(async move {
        if let Err(e) = server.await {
            eprintln!("API server error: {}", e);
        }
    });

    Ok(ApiServerHandle {
        shutdown_tx: Some(shutdown_tx),
        join_handle: Some(join_handle),
    })
}

fn build_router(state: Arc<ApiSharedState>) -> Router {
    Router::new()
        .route("/health", get(routes::health))
        .route("/healthz", get(routes::healthz))
        .route("/status", get(routes::status))
        .route("/v1/models", get(routes::models))
        .route("/v1/chat/completions", post(routes::chat_completions))
        .route("/v1/completions", post(routes::completions))
        .route("/v1/embeddings", post(routes::embeddings))
        .route("/v1/messages", post(routes::messages))
        .route("/v1/responses", post(routes::responses_api))
        .layer(from_fn_with_state(state.clone(), auth::bearer_auth))
        .with_state(state)
}

/// WB 上游健康检测线程（F-34 ④/§2.2 频控维度）：每 5min + 0-60s 抖动对 CN 主域名
/// 发一次轻量 GET（模型目录路径）。无凭证探测——任何 HTTP 响应（含 401）都证明
/// 服务在线，仅连接失败/超时判不可达；结果写 state.wb_probe_* 供 /status 透出。
/// 频控红线：单次单请求、不重试、不批量，探活流量可忽略。
fn spawn_wb_health_probe(state: Arc<ApiSharedState>) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        // 探测目标：WB 上游对话主域名（CN）的轻量 GET 路径
        const PROBE_URL: &str =
            concat!("https://copilot.tencent.com", "/console/enterprises/personal/models");
        loop {
            // 5min 基础间隔 + 0-60s 抖动（多实例同时启动时错峰；零新增依赖，纳秒派生）
            let jitter = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 % 60)
                .unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_secs(300 + jitter));
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(10))
                .build();
            let ok = match agent
                .get(PROBE_URL)
                .set("User-Agent", super::wb_upstream::WB_UA)
                .call()
            {
                Ok(_) => true,
                Err(ureq::Error::Status(_, _)) => true, // 有 HTTP 响应 = 服务在线
                Err(_) => false,                        // 网络/超时 = 不可达
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            state.wb_probe_ts_ms.store(now, Ordering::Relaxed);
            state.wb_probe_ok.store(if ok { 1 } else { 0 }, Ordering::Relaxed);
            if !ok {
                // 失败明示（§2.2 接口稳定性）：记日志不静默
                eprintln!("[wb-probe] 上游健康检测失败: {PROBE_URL}");
            }
        }
    });
}
