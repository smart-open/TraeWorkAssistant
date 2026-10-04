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
/// 监听地址（审查 P1-2）：取 gateway_settings.host，默认 127.0.0.1 环回——个人助手
/// 场景默认不对外暴露；用户显式配置非环回地址（0.0.0.0 / 局域网 IP）时强制校验
/// 鉴权（存在启用 Key 且未显式关闭鉴权），否则拒绝启动并返回明确中文错误
/// （经启动命令链 api_server.rs do_start 的 `?` 透传前端展示）。
pub async fn start_api_server(
    port: u16,
    state: Arc<ApiSharedState>,
) -> Result<ApiServerHandle, String> {
    let host = super::gateway_settings::load(&state.data_dir).host;
    ensure_bind_auth_policy(&host, &super::api_keys::load(&state.data_dir))?;
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await.map_err(|e| {
        // 审查 G1：绑定失败错误带监听地址与局域网访问指引（前端展示）
        format!(
            "当前监听地址 {host}:{port} 绑定失败: {e}；\
             如需局域网访问，请在 API 服务设置中修改监听地址并启用 API Key"
        )
    })?;

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

/// 环回监听地址判定（审查 G3 复用：启动门禁 + api_keys_save 保存守卫同一份判定）。
/// fail-closed 白名单语义：仅精确匹配以下字面量，未识别的地址一律按非环回处理——
/// 宁可误拒（多走一次鉴权校验），不可误放（局域网裸奔）。含 IPv6 环回等价写法
/// "0:0:0:0:0:0:0:1" 与 IPv4 映射环回 "::ffff:127.0.0.1"（审查 G8）。
pub(crate) fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.trim(),
        "127.0.0.1" | "::1" | "localhost" | "0:0:0:0:0:0:0:1" | "::ffff:127.0.0.1"
    )
}

/// 非环回绑定的鉴权门禁（审查 P1-2；纯函数，可单测）：
/// - 环回地址（127.0.0.1 / ::1 / localhost 等价写法）→ 放行（本机监听无暴露面）
/// - 非环回地址（0.0.0.0 / 局域网 IP，局域网内任何设备可达）→ 必须**存在启用 Key
///   且未显式关闭鉴权**，否则拒绝启动并给出明确中文提示（前端展示）
fn ensure_bind_auth_policy(
    host: &str,
    keys: &super::api_keys::ApiKeysFile,
) -> Result<(), String> {
    let loopback = is_loopback_host(host);
    if loopback {
        return Ok(());
    }
    if keys.auth_disabled {
        return Err(format!(
            "当前监听地址 {host} 为非环回地址（局域网内任何设备均可访问），\
             不允许在「显式关闭鉴权」状态下对外监听；\
             如需局域网访问，请在 API 服务设置中修改监听地址并启用 API Key，\
             或将监听地址改回 127.0.0.1"
        ));
    }
    if !keys.has_enabled() {
        return Err(format!(
            "当前监听地址 {host} 为非环回地址（局域网内任何设备均可访问），\
             必须先创建并启用至少一个 API Key 才能对外监听；\
             如需局域网访问，请在 API 服务设置中修改监听地址并启用 API Key，\
             或将监听地址改回 127.0.0.1"
        ));
    }
    Ok(())
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
        // issue #21：axum 对 `Bytes` 提取器默认限制 2MiB，超限请求在进 handler 前
        // 就被 413 纯文本拒绝（handler 内 8MB 检查不可达）；显式放开到
        // routes::MAX_BODY_BYTES（32MiB），与 handler 内检查阈值保持一致
        .layer(DefaultBodyLimit::max(routes::MAX_BODY_BYTES))
        .layer(from_fn_with_state(state.clone(), auth::bearer_auth))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== P1-2：非环回绑定鉴权门禁 ====================

    fn keys_with(entries: Vec<super::super::api_keys::ApiKeyEntry>, auth_disabled: bool) -> super::super::api_keys::ApiKeysFile {
        super::super::api_keys::ApiKeysFile { keys: entries, auth_disabled }
    }

    fn key_entry(enabled: bool) -> super::super::api_keys::ApiKeyEntry {
        serde_json::from_value(serde_json::json!({
            "id": "k1", "name": "k1", "key": "ck-test", "enabled": enabled,
        }))
        .unwrap()
    }

    #[test]
    fn bind_policy_loopback_always_allowed() {
        // 环回地址：无 Key / 显式关闭鉴权均放行（本机监听无暴露面）
        for host in ["127.0.0.1", "::1", "localhost", " 127.0.0.1 "] {
            assert!(ensure_bind_auth_policy(host, &keys_with(vec![], true)).is_ok());
            assert!(ensure_bind_auth_policy(host, &keys_with(vec![], false)).is_ok());
            assert!(ensure_bind_auth_policy(host, &keys_with(vec![key_entry(true)], false)).is_ok());
        }
    }

    #[test]
    fn bind_policy_non_loopback_requires_auth() {
        // 0.0.0.0 / 局域网 IP：显式关闭鉴权 → 拒绝
        let e = ensure_bind_auth_policy("0.0.0.0", &keys_with(vec![key_entry(true)], true))
            .expect_err("auth_disabled 应拒绝");
        assert!(e.contains("显式关闭鉴权"));
        // 无任何启用 Key → 拒绝
        let e = ensure_bind_auth_policy("192.168.1.10", &keys_with(vec![], false))
            .expect_err("无启用 Key 应拒绝");
        assert!(e.contains("API Key"));
        let e = ensure_bind_auth_policy("0.0.0.0", &keys_with(vec![key_entry(false)], false))
            .expect_err("仅禁用 Key 应拒绝");
        assert!(e.contains("API Key"));
        // 启用 Key 存在且未关闭鉴权 → 放行
        assert!(ensure_bind_auth_policy("0.0.0.0", &keys_with(vec![key_entry(true)], false)).is_ok());
    }

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
