use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::auth;
use super::routes;
use super::wb_catalog;
use super::ApiSharedState;

/// API 服务器句柄：用于优雅停止
pub struct ApiServerHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl ApiServerHandle {
    /// 发送 shutdown 信号并等待优雅退出（最多 3s，每 50ms 轮询一次），
    /// 超时才 abort（P2 修复9：原实现 send 后立即 abort，优雅停机被自身取消，
    /// 在途请求被硬断）
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.join_handle.take() {
            if !wait_task_finished(&h, std::time::Duration::from_secs(3)) {
                h.abort();
            }
        }
    }
}

/// 轮询任务是否已结束（每 50ms 一次，最多 max）；P2 修复9 优雅停机用
fn wait_task_finished(h: &JoinHandle<()>, max: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + max;
    loop {
        if h.is_finished() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
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
        // T5.4/F-63 生图双端点投影
        .route("/v1/images/generations", post(routes::images_generations))
        .route("/v1/images/edits", post(routes::images_edits))
        .layer(from_fn_with_state(state.clone(), auth::bearer_auth))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== P2 修复9：优雅停机轮询 ====================

    #[test]
    fn wait_task_finished_detects_completion_and_timeout() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        // 已完成任务：立即判定结束
        let done = rt.spawn(async {});
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(wait_task_finished(&done, std::time::Duration::from_secs(1)));
        // 长任务：达到 max 轮询上限判定未结束（不再立即 abort）
        let slow = rt.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let started = std::time::Instant::now();
        assert!(!wait_task_finished(&slow, std::time::Duration::from_millis(150)));
        assert!(started.elapsed() >= std::time::Duration::from_millis(150));
        slow.abort();
    }
}

/// WB 上游健康检测线程（F-34 ④/§2.2 频控维度）：每 5min + 0-60s 抖动对 CN 主域名
/// 发一次轻量 GET（模型目录路径）。无凭证探测——任何 HTTP 响应（含 401）都证明
/// 服务在线，仅连接失败/超时判不可达；结果写 state.wb_probe_* 供 /status 透出。
/// 频控红线：单次单请求、不重试、不批量，探活流量可忽略。
fn spawn_wb_health_probe(state: Arc<ApiSharedState>) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        // T5.1/F-37：启动即做一次上游目录动态替换（best effort，失败不影响启动——
        // 本地静态兜底目录保持不动）；取任一健康 WB 账号的凭证拉取
        {
            let picked = state.wb_pool.pick_excluding_constrained(
                &std::collections::HashSet::new(),
                None,
                None,
            );
            if let Some(p) = picked {
                match wb_catalog::fetch_and_replace(
                    &state.data_dir,
                    &p.uid,
                    &p.jwt,
                    &p.domain,
                    &p.enterprise_id,
                    p.global_region,
                ) {
                    Ok(n) => {
                        crate::fs_utils::app_log(
                            &state.data_dir,
                            &format!("WB 模型目录动态替换成功: {} 个模型", n),
                        );
                    }
                    Err(e) => {
                        crate::fs_utils::app_log(
                            &state.data_dir,
                            &format!("WB 模型目录动态替换失败（保持静态兜底）: {}", e),
                        );
                    }
                }
            }
        }
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
