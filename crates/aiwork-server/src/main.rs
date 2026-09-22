//! aiwork-server：Web 版单进程入口（T4 骨架 + T5/T6/T7/T8 接线）
//! 启动序：数据目录（AIWORK_DATA_DIR）→ SQLite 迁移 → vault 启动序 → 网关共享状态装配
//! → 管理面（/api/login + /api/cmd 桥 + SSE）→ 调度线程（6 任务）
//! → 单端口 axum（/v1/* 网关 + /api/* 管理面 + /health + 静态托管）→ SIGTERM 优雅退出
//!
//! 环境变量：
//! - AIWORK_DATA_DIR     数据目录（默认 /data/AIWorkAssistant，Windows 按启动盘符解析；旧 %APPDATA%/XDG 数据首启自动复制迁移）
//! - AIWORK_LISTEN_ADDR  监听地址（默认 127.0.0.1:{网关设置端口}；容器部署设 0.0.0.0:<port>）
//! - AIWORK_WEB_DIST     前端构建产物目录（默认 web/dist）
//! - AIWORK_ADMIN_TOKEN  管理面 token（未设置时首启生成 conf/admin_token）
//! - --task-run <name>   CLI 任务兜底（checkin / wb-checkin / wb-renew / refresh-credits，跑完即退）

use std::sync::Arc;

use aiwork_core::api_server::runtime::{build_shared, set_gateway_shared};
use aiwork_core::api_server::server::{build_router, spawn_shared_tasks};
use aiwork_core::api_server::ApiSharedState;
use aiwork_core::fs_utils;
use aiwork_core::state::AppState;
use aiwork_core::store;
use aiwork_core::vault;
use axum::routing::get;
use axum::Router;

mod admin;

#[tokio::main]
async fn main() {
    // CLI 任务模式：--task-run <name> 直调单任务后退出（容器 exec / 手动兜底入口）
    let args: Vec<String> = std::env::args().collect();
    if let Some(task) = aiwork_core::tasks::parse_task_mode(&args) {
        let state = AppState::new().unwrap_or_else(|e| {
            eprintln!("aiwork-server 启动失败: {e}");
            std::process::exit(1);
        });
        if let Some(msg) = store::migrate::migrate_on_startup(&state.data_dir) {
            fs_utils::app_log(&state.data_dir, &format!("SQLite 迁移提示: {msg}"));
        }
        std::process::exit(aiwork_core::tasks::run_cli_task(&task, &state));
    }

    // 数据目录：AIWORK_DATA_DIR 优先（AppState::new 内部处理），失败即退出
    let state = AppState::new().unwrap_or_else(|e| {
        eprintln!("aiwork-server 启动失败: {e}");
        std::process::exit(1);
    });

    // SQLite 迁移（幂等；与桌面 tasks/mod.rs CLI 分支同款语义）
    if let Some(msg) = store::migrate::migrate_on_startup(&state.data_dir) {
        fs_utils::app_log(&state.data_dir, &format!("SQLite 迁移提示: {msg}"));
    }

    // vault 启动序（对齐桌面语义）：清理残留临时凭据文件 + 库中明文凭据收敛进 vault
    vault::cleanup_temp_accounts(&state);
    vault::migrate_on_startup(&state);

    // 网关共享状态装配（vault 解密 → 双池装配 → 诊断日志）
    let shared: Arc<ApiSharedState> = build_shared(&state);

    // 注册网关共享句柄：commands 层（pool_set / oauth_login / cooldown 清除）热重载据此取用
    set_gateway_shared(shared.clone());

    // 常驻任务：WB 健康探针 + 持久化 flusher（2s 周期）
    spawn_shared_tasks(shared.clone());

    // 调度线程（T8）：6 任务集（trae-jwt-renew 05:30 / trae-checkin 09:00 / wb-checkin 09:10
    // / wb-renew 10:30 / wb-credits-snapshot 23:30 / trae-credits-snapshot 23:40），启动 90s 后首跑
    aiwork_core::scheduler::start(state.clone());

    // 管理面（T5/T6/T7）：token 加载/生成 + 命令桥 + SSE 事件流 + cookie 鉴权
    let admin = admin::AdminState::new(Arc::new(state.clone()));

    // IP 允许列表（T12a）：启动时从 kv 重载进程内缓存（保存后由 ip_allowlist_set 热更新）
    admin::ip_allow::reload(&state);

    // 路由合并：网关（/v1/* + /health + /healthz + /status，api_keys fail-closed）优先，
    // 其次管理面（/api/*，cookie 鉴权），其余路径落入静态托管（React 构建产物）；
    // 最外层挂 IP 允许列表中间件（/health、/healthz 探活放行，回环地址始终放行）
    let app = build_router(shared.clone())
        .merge(admin::router(admin))
        .merge(static_router())
        .layer(axum::middleware::from_fn(admin::ip_allow::ip_middleware));

    // 监听地址：AIWORK_LISTEN_ADDR 优先；默认 127.0.0.1:{网关设置端口}
    let port = aiwork_core::api_server::gateway_settings::load(&state.data_dir).port;
    let addr = std::env::var("AIWORK_LISTEN_ADDR").unwrap_or_else(|_| format!("127.0.0.1:{port}"));
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            let msg = format!("aiwork-server 启动失败: {addr} 绑定失败: {e}");
            eprintln!("{msg}");
            fs_utils::app_log(&state.data_dir, &msg);
            std::process::exit(1);
        }
    };
    let started = format!(
        "aiwork-server 已启动: {addr}（/v1/* 网关 + /api/* 管理面 + /health + 静态 UI）"
    );
    println!("{started}");
    fs_utils::app_log(&state.data_dir, &started);

    // into_make_service_with_connect_info：向请求注入 ConnectInfo<SocketAddr>，
    // IP 允许列表中间件据此取 TCP 对端地址（trust_proxy 关闭时的唯一信任来源）
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("axum serve 异常退出");

    // 优雅退出：排空用量脏队列与 api_keys 计数（对齐桌面 do_stop 语义）
    shared.flush_pending_writes();
    fs_utils::app_log(&state.data_dir, "aiwork-server 已退出（用量/计数已落库）");
}

/// 静态资源路由：AIWORK_WEB_DIST 目录存在 → ServeDir（目录自动补 index.html）；
/// 不存在 → 指引占位页（T4 判据允许空壳）。默认 dist/（vite build 输出目录，T9 对齐）
fn static_router() -> Router {
    let dir = std::env::var("AIWORK_WEB_DIST").unwrap_or_else(|_| "dist".to_string());
    if std::path::Path::new(&dir).is_dir() {
        Router::new().fallback_service(
            tower_http::services::ServeDir::new(&dir).append_index_html_on_directories(true),
        )
    } else {
        Router::new().route(
            "/",
            get(|| async {
                axum::response::Html(
                    "<h1>aiwork-server</h1><p>运行中。Web UI 未构建：设置 AIWORK_WEB_DIST 指向前端构建产物目录（npm run build）。</p>",
                )
            }),
        )
    }
}

/// Ctrl+C / SIGTERM 优雅退出信号
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("安装 SIGTERM 处理器失败")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
