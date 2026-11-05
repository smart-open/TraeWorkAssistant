use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
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
    spawn_persistence_flusher(state.clone());
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

/// 持久化 flusher（网关性能批次 C/E）：每 2s 排空用量脏队列与 api_keys 计数脏副本。
/// 持 Weak 引用：服务停止、共享状态被释放后线程自动退出（重启服务重建新线程），
/// 避免重启循环泄漏线程。stop / 应用退出时另有一次性 flush_pending_writes 兜底。
fn spawn_persistence_flusher(state: Arc<ApiSharedState>) {
    let weak = Arc::downgrade(&state);
    drop(state);
    let _ = std::thread::Builder::new()
        .name("api-persist-flush".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let Some(state) = weak.upgrade() else {
                return; // ApiSharedState 已释放（服务停止且运行时句柄移除）
            };
            state.flush_usage_dirty();
            crate::api_server::api_keys::flush_dirty(&state.data_dir);
        });
}

/// 网关路由（公开：server 单体把本 router 与管理面/静态托管 merge 后单端口服务）
pub fn build_router(state: Arc<ApiSharedState>) -> Router {
    Router::new()
        // 网关 Web 状态页（/gw-status，浏览器直访；免鉴权见 auth::bearer_auth）。
        // 根路径 / 不在此注册：落入静态托管 fallback（SPA → 登录页/管理面）
        .route("/gw-status", get(routes::status_page))
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
        // issue #21：axum 对 `Bytes` 提取器默认限制 2MiB，超限请求在进 handler 前
        // 就被 413 纯文本拒绝（handler 内检查不可达）；显式放开到
        // routes::max_body_bytes()（AIWORK_MAX_BODY_BYTES 可调，
        // clamp [256KiB, 64MiB]，issue #138 P1；issue #181 默认回调 32MiB，
        // agent CLI 大包不再被 413 误判为超上下文窗口），与 handler 内检查阈值保持一致
        .layer(DefaultBodyLimit::max(routes::max_body_bytes()))
        .layer(from_fn_with_state(state.clone(), auth::bearer_auth))
        .with_state(state)
}

/// 网关常驻任务（公开：server 单体自行绑定监听时调用；start_api_server 内部已含）：
/// WB 健康探针 + 持久化 flusher（2s 周期，Weak 引用随共享状态释放自动退出）
pub fn spawn_shared_tasks(state: Arc<ApiSharedState>) {
    spawn_wb_health_probe(state.clone());
    spawn_persistence_flusher(state);
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
/// G8 审查修复：同循环对 buddy-ai 国际上游（BILLING_BASE_GLOBAL，workbuddy.ai）
/// 同口径再探一次，结果写 state.buddy_ai_probe_* 供 /status buddy_ai 块透出——
/// 国际档开关独立于 CN，CN 可达 ≠ Intl 可达，双上游各自画像。
/// 频控红线：单次单请求、不重试、不批量，探活流量可忽略。
fn spawn_wb_health_probe(state: Arc<ApiSharedState>) {
    use std::sync::atomic::Ordering;
    // 与 spawn_persistence_flusher 同构：持 Weak 引用，服务停止、共享状态被释放后
    // 线程自动退出（重启服务重建新线程），避免反复启动/停止累积线程与整份共享状态。
    let weak = Arc::downgrade(&state);
    drop(state);
    std::thread::spawn(move || {
        // T5.1/F-37：启动即做一次上游目录动态替换（best effort，失败不影响启动——
        // 本地静态兜底目录保持不动）；取任一健康 WB 账号的凭证拉取。
        // 强引用升级仅限块内临时（2026-10-12 审查 P3 修复，对齐蓝本）：严禁把
        // state 提出本块绑定到闭包顶层——线程自持强引用会令下方循环内的
        // weak.upgrade 恒成功，「服务停止自动退出」失效成线程泄漏
        if let Some(state) = weak.upgrade() {
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
            // buddy-ai（国际版）目录动态抓取（2026-10-15）：GET /v3/config（R-5 锚点，
            // UA 自带 CLI 段下发全量），取 buddy-ai 池任一账号 Bearer 直连——不依赖
            // 客户端/抓包代理。同款 best effort：失败保持内置静态表；池未启用跳过
            if state.buddy_ai_enabled.load(Ordering::Relaxed) {
                if let Some(p) = state.buddy_ai_pool.pick_excluding_constrained(
                    &std::collections::HashSet::new(),
                    None,
                    None,
                ) {
                    match wb_catalog::buddy_ai_fetch_and_replace(&state.data_dir, &p.uid, &p.jwt) {
                        Ok(n) => {
                            crate::fs_utils::app_log(
                                &state.data_dir,
                                &format!("BuddyAI Intl 模型目录动态替换成功: {} 个模型", n),
                            );
                        }
                        Err(e) => {
                            crate::fs_utils::app_log(
                                &state.data_dir,
                                &format!("BuddyAI Intl 模型目录动态替换失败（保持静态兜底）: {}", e),
                            );
                        }
                    }
                }
            }
        }
        // 探测目标：WB 上游对话主域名（CN）的轻量 GET 路径
        const PROBE_URL: &str =
            concat!("https://copilot.tencent.com", "/console/enterprises/personal/models");
        // G8 审查修复：buddy-ai 国际上游探针目标（无凭证根路径 GET，同款「有
        // HTTP 响应 = 在线」口径；引用 wb_common 常量避免双写漂移）
        const BUDDY_AI_PROBE_URL: &str = crate::tasks::wb_common::BILLING_BASE_GLOBAL;
        loop {
            // 5min 基础间隔 + 0-60s 抖动（多实例同时启动时错峰；零新增依赖，纳秒派生）
            let jitter = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 % 60)
                .unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_secs(300 + jitter));
            // 服务停止后（共享状态释放）退出线程，避免泄漏
            let Some(state) = weak.upgrade() else {
                return;
            };
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(10))
                .build();
            // 无凭证探活判定（CN/Intl 共用）：任何 HTTP 响应（含 401）= 服务在线。
            // UA 沿用原 CN 探针口径——部分 WAF 对缺 UA 连接直接重置（Err）会被
            // 误判不可达，保留 WB_UA 与既有判定行为零差异
            let probe_ok = |url: &str| {
                match agent
                    .get(url)
                    .set("User-Agent", super::wb_upstream::WB_UA)
                    .call()
                {
                    Ok(_) => true,
                    Err(ureq::Error::Status(_, _)) => true,
                    Err(_) => false,
                }
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let ok = probe_ok(PROBE_URL);
            state.wb_probe_ts_ms.store(now, Ordering::Relaxed);
            state.wb_probe_ok.store(if ok { 1 } else { 0 }, Ordering::Relaxed);
            if !ok {
                // 失败明示（§2.2 接口稳定性）：记日志不静默
                eprintln!("[wb-probe] 上游健康检测失败: {PROBE_URL}");
            }
            // G8：Intl 上游独立探测（同频控节奏串行第二请求，探活流量仍可忽略）；
            // 未启用 buddy-ai 池也照探——开关随时可热开，探针只描述上游可达性。
            // 审查复审修复：探测前重采样时间戳——CN 探针 HTTP 超时（10s）会让
            // 沿用的 now 最多回溯 10s，/status 时间戳展示失真
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let intl_ok = probe_ok(BUDDY_AI_PROBE_URL);
            state.buddy_ai_probe_ts_ms.store(now, Ordering::Relaxed);
            state
                .buddy_ai_probe_ok
                .store(if intl_ok { 1 } else { 0 }, Ordering::Relaxed);
            if !intl_ok {
                eprintln!("[buddy-ai-probe] 国际上游健康检测失败: {BUDDY_AI_PROBE_URL}");
            }
        }
    });
}
