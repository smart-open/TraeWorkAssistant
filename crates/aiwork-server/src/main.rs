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
    // 最外层挂 IP 允许列表中间件（/health、/healthz 探活放行，回环地址始终放行）+
    // CSP（仅 HTML 文档响应下发）
    let app = build_router(shared.clone())
        .merge(admin::router(admin))
        .merge(static_router())
        .layer(axum::middleware::from_fn(csp_html_middleware))
        .layer(axum::middleware::from_fn(admin::ip_allow::ip_middleware));

    // 监听地址：AIWORK_LISTEN_ADDR 优先（显式空串/空白视为未设置，防御 bind("") 失败）；
    // 默认 127.0.0.1:{网关设置端口}
    let port = aiwork_core::api_server::gateway_settings::load(&state.data_dir).port;
    let addr = std::env::var("AIWORK_LISTEN_ADDR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("127.0.0.1:{port}"));
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

    // 调度触发的时区可观测（审查-兼容 P6）：全部 HH:MM 任务按 chrono::Local 触发，
    // TZ 被覆盖/清空时签到时刻会静默偏移；启动时输出生效时区与偏移供部署核对
    let tz_now = chrono::Local::now();
    let tz_hours = (tz_now.offset().local_minus_utc() as f64) / 3600.0;
    let tz_info = format!(
        "调度时区（HH:MM 任务触发依据）: UTC{tz_hours:+.2}（本地 {}）——Docker 部署需 TZ=Asia/Shanghai",
        tz_now.format("%Y-%m-%d %H:%M:%S"),
    );
    println!("{tz_info}");
    fs_utils::app_log(&state.data_dir, &tz_info);

    // 双门禁启动侧（9ba5fd0 移植）：非环回监听时提示 auth_disabled 不生效（运行期
    // 强制在 aiwork-core auth.rs::bearer_auth，此处仅可观测提示）
    if !aiwork_core::api_server::auth::listen_is_loopback() {
        let warn = format!(
            "安全提示: 监听地址 {addr} 非环回——「关闭鉴权」开关不生效，/v1/* 网关必须携带 API Key 访问"
        );
        println!("{warn}");
        fs_utils::app_log(&state.data_dir, &warn);
    }

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

/// CSP 响应头中间件（9ba5fd0 移植，纵深防御）：仅对 HTML 文档响应下发——
/// SPA 产物无内联脚本；style 'unsafe-inline' 为 React 内联 style 属性所需；
/// ws:/wss: 为管理面 WS 事件桥（CSP 无法按源匹配 WS，scheme 放行）；img data:
/// 为 base64 横幅、font data: 为字体兜底（img 无需 blob:——createObjectURL 仅用于
/// a.download 下载链接，不涉 img-src）。仅经 axum 托管时生效（vite dev 不经过
/// 此层，开发模式不受限）。
/// /gw-status 内嵌状态页为服务端自渲染 HTML（内联 <script> + onclick，脚本内容
/// 静态常量、用户输入经 esc() 转义），统一策略的 script-src 'self' 会拦截其页面
/// 脚本（第四轮审查 Critical）——该路径单发含 script 'unsafe-inline' 的专属策略
async fn csp_html_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self' ws: wss:; font-src 'self' data:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-src 'none'";
    // /gw-status 专属：内联脚本/onclick 需 'unsafe-inline'（其余指令与常规策略一致）；
    // 精确匹配（含尾斜杠子路径），避免 /gw-status-xxx 等无关路径误享宽松策略
    const CSP_GW_STATUS: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self' ws: wss:; font-src 'self' data:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-src 'none'";
    let path = req.uri().path();
    let csp = if path == "/gw-status" || path.starts_with("/gw-status/") {
        CSP_GW_STATUS
    } else {
        CSP
    };
    let mut resp = next.run(req).await;
    let is_html = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    if is_html && !resp.headers().contains_key(axum::http::header::CONTENT_SECURITY_POLICY) {
        resp.headers_mut().insert(
            axum::http::header::CONTENT_SECURITY_POLICY,
            axum::http::HeaderValue::from_static(csp),
        );
    }
    resp
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
