pub mod auth;
pub mod pool;
pub mod payload;
pub mod routes;
pub mod server;
pub mod sse;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Mutex;

pub use pool::ApiPool;

/// SOLO 上游常量（来自 traework2api 实测）
pub const AGENT_HOST: &str = "https://trae-api-cn.mchost.guru";
pub const EP_CHAT: &str = "/api/agent/v3/llm_utils_chat";
pub const APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
pub const IDE_VERSION: &str = "0.1.43";
pub const IDE_VERSION_CODE: &str = "20260716";
pub const FUNCTION: &str = "solo_work_lite";
pub const DEFAULT_MODEL: &str = "glm-5.2";

/// API 服务器运行时共享状态（传入 axum State）
pub struct ApiSharedState {
    pub pool: ApiPool,
    pub api_key: String,
    pub default_model: String,
    pub total_requests: AtomicU64,
    pub active_uid: Mutex<Option<String>>,
    pub last_error: Mutex<Option<String>>,
    pub data_dir: PathBuf,
}

/// 上游错误分类（与 Phase 1 冷却状态机对齐）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrKind {
    None,
    PlanLimit,
    SoftRate,
    SessionDead,
    NotFound,
    Server,
    Client,
}

impl ErrKind {
    pub fn cooldown_duration(self) -> std::time::Duration {
        match self {
            ErrKind::PlanLimit => std::time::Duration::from_secs(12 * 3600),
            ErrKind::SoftRate | ErrKind::NotFound => std::time::Duration::from_secs(60),
            ErrKind::SessionDead => std::time::Duration::from_secs(24 * 3600),
            ErrKind::Client | ErrKind::Server => std::time::Duration::from_secs(10 * 60),
            ErrKind::None => std::time::Duration::ZERO,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrKind::None => "none",
            ErrKind::PlanLimit => "PlanLimit",
            ErrKind::SoftRate => "SoftRate",
            ErrKind::SessionDead => "SessionDead",
            ErrKind::NotFound => "NotFound",
            ErrKind::Server => "Server",
            ErrKind::Client => "Client",
        }
    }
}

/// 按 HTTP 状态码 + body 判定错误类别
pub fn classify_error(status: u16, body: &str) -> ErrKind {
    if body.contains("\"code\":1005") || (body.contains("1005") && body.to_lowercase().contains("plan")) {
        return ErrKind::PlanLimit;
    }
    match status {
        401 => ErrKind::SessionDead,
        429 => ErrKind::SoftRate,
        404 => ErrKind::NotFound,
        s if s >= 500 => ErrKind::Server,
        s if s >= 400 => ErrKind::Client,
        _ => ErrKind::None,
    }
}

/// 按 SOLO 业务错误码 + message 判定错误类别（流内 error 事件）
pub fn classify_solo_error(code: i64, msg: &str) -> ErrKind {
    if code == 1005 || msg.to_lowercase().contains("plan") {
        return ErrKind::PlanLimit;
    }
    match code {
        401 => ErrKind::SessionDead,
        429 => ErrKind::SoftRate,
        404 => ErrKind::NotFound,
        c if c >= 500 => ErrKind::Server,
        c if c >= 400 => ErrKind::Client,
        _ => ErrKind::Server,
    }
}

/// 流式上游 Agent：无总超时，用于 SSE 流式对话
pub fn streaming_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_read(std::time::Duration::from_secs(0))
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}
