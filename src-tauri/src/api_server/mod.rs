pub mod api_keys;
pub mod api_logger;
pub mod auth;
pub mod custom_models;
pub mod custom_route;
pub mod dispatch;
pub mod gateway_settings;
pub mod models_sync;
pub mod pool;
pub mod payload;
pub mod retry;
pub mod routes;
pub mod server;
pub mod sse;
pub mod unified_catalog;
pub mod usage;
pub mod wb_catalog;
pub mod wb_images;
pub mod wb_model_route;
pub mod wb_payload;
pub mod wb_responses;
pub mod wb_route;
pub mod wb_sse;
pub mod wb_sticky;
pub mod wb_toolexec;
pub mod wb_upstream;

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

pub use api_logger::ApiLogger;
pub use pool::ApiPool;

/// SOLO 上游常量
/// llm_utils_chat 使用 trae-api-cn.mchost.guru（通用积分 product_id 208）
pub const AGENT_HOST: &str = "https://trae-api-cn.mchost.guru";
pub const EP_LLM_CHAT: &str = "/api/agent/v3/llm_utils_chat";
pub const APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
pub const IDE_VERSION: &str = "0.1.50";
pub const IDE_VERSION_CODE: &str = "20260811";
pub const FUNCTION: &str = "solo_work_lite";
pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";
pub const REFERER_BASE: &str = "https://trae-api-cn.mchost.guru";

/// API 服务器运行时共享状态（传入 axum State）
pub struct ApiSharedState {
    /// SOLO 上游账号池（trae llm_utils_chat）
    pub pool: ApiPool,
    /// WorkBuddy 上游账号池（copilot /v2/chat/completions，T2.1）；
    /// 与 SOLO 池并存，按模型目录路由（wb_catalog 命中 → WB 池）
    pub wb_pool: ApiPool,
    /// WB 上游开关（api_pool.json.wb_enabled；关闭时 WB 模型返回 400）
    pub wb_enabled: std::sync::atomic::AtomicBool,
    /// WB 指纹清洗开关（F-30，默认开；wb_template_map 清洗联动）
    pub wb_sanitize: std::sync::atomic::AtomicBool,
    /// 默认深度思考（T5.3/F-62）：客户端未显式请求 reasoning_effort 时默认 high
    pub wb_default_thinking: std::sync::atomic::AtomicBool,
    /// 工具代执行（T5.5/F-64）：客户端声明 web_search 类工具时代理侧代执行
    pub wb_tool_exec: std::sync::atomic::AtomicBool,
    /// 后台任务降级（T5.6③/F-65）：标题/摘要类短请求路由到目录最低倍率模型
    pub wb_bg_downgrade: std::sync::atomic::AtomicBool,
    /// 会话粘性双模式存储（T2.4/F-31，仅 WB 上游消费）
    pub wb_sticky: wb_sticky::StickyStore,
    /// 会话池粘性（统一网关 §4.4，内存态不落盘、重启即清）：
    /// session_key → (上次成功服务的资源池, 过期时间 Unix 秒)，TTL 60s 软粘
    pub pool_sticky: Mutex<std::collections::HashMap<String, (dispatch::TargetPool, i64)>>,
    /// 模型级冷却（F-34）：model → (until 秒, 连续失败次数)；10→20→40s 渐进退避，
    /// 优先级高于 Key 级冷却
    pub model_cooldowns: Mutex<std::collections::HashMap<String, (i64, u32)>>,
    /// 审核模板映射表热更新缓存：(文件 mtime, 映射)；None = 用内置兜底
    pub wb_template_cache: Mutex<Option<(std::time::SystemTime, Vec<(String, String)>)>>,
    pub default_model: String,
    /// 数据目录（读取/持久化 api_models.json 的 function 自学习覆盖）
    pub data_dir: std::path::PathBuf,
    pub total_requests: AtomicU64,
    /// 当前并发数（统一网关 §4.5）：由 InflightGuard RAII 维护，覆盖全部业务端点。
    /// Arc 包装使 guard 可跨 spawn_blocking/流任务持有（'static + Send）
    pub inflight: Arc<AtomicU64>,
    pub active_uid: Mutex<Option<String>>,
    pub last_error: Mutex<Option<String>>,
    pub logger: ApiLogger,
    /// Debug 模式：开启后记录完整请求/响应到 API 日志
    pub debug_enabled: std::sync::atomic::AtomicBool,
    /// 用量统计（内存累积，每次请求后落盘）
    pub usage: Mutex<usage::UsageFile>,
    /// WB 上游健康探针（F-34 ④/§2.2 频控）：最近探测 Unix 毫秒；-1 = 尚未探测
    pub wb_probe_ts_ms: std::sync::atomic::AtomicI64,
    /// WB 上游健康探针结果：-1 未探测 / 0 不可达 / 1 在线
    pub wb_probe_ok: std::sync::atomic::AtomicI64,
}

impl ApiSharedState {
    /// 进入业务端点时获取并发 guard（§4.5）：inflight +1，Drop 时 -1。
    /// panic 展开 / 客户端断连（axum 丢弃 handler future）/ 流异常终止均兜底释放
    pub fn inflight_guard(&self) -> InflightGuard {
        InflightGuard::acquire(&self.inflight)
    }

    /// 记录一次请求用量并原子落盘；写盘失败静默忽略，不影响主流程。
    /// `is_wb`：WB 上游路由的请求记入独立 wb_days 桶（与 Trae 侧分账，页面互不串数）
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage(
        &self,
        is_wb: bool,
        model: &str,
        uid: &str,
        key_id: &str,
        ok: bool,
        is_stream: bool,
        duration_ms: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        let mut guard = self
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.record(
            is_wb, model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens, completion_tokens,
        );
        usage::save(&self.data_dir, &guard);
    }

    /// 记录一次自定义模型请求用量（独立 custom_days 桶，与 Trae/WB 侧分账）；
    /// uid 固定 "custom"，落盘策略与 record_usage 相同
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage_custom(
        &self,
        model: &str,
        key_id: &str,
        ok: bool,
        is_stream: bool,
        duration_ms: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        let mut guard = self
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.record_in(
            usage::UsageBucket::Custom,
            model, "custom", key_id, ok, is_stream, duration_ms, prompt_tokens, completion_tokens,
        );
        usage::save(&self.data_dir, &guard);
    }
}

/// 当前并发计数 RAII guard（统一网关 §4.5）：构造时 +1，Drop 时 -1。
/// 持有 Arc 克隆（'static + Send）——端点获取后作为参数移入执行路径，
/// 流式场景随 spawn 任务存续至流结束；不做下溢防护依赖"构造必 +1"配对语义
pub struct InflightGuard {
    counter: Arc<AtomicU64>,
}

impl InflightGuard {
    pub fn acquire(counter: &Arc<AtomicU64>) -> InflightGuard {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        InflightGuard {
            counter: counter.clone(),
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 上游错误分类（与 Phase 1 冷却状态机对齐；T2.2 扩展 HardCredit/Forbidden）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrKind {
    None,
    PlanLimit,
    SoftRate,
    SessionDead,
    NotFound,
    Server,
    Client,
    /// 积分耗尽（F-29 v1.2）：冷却到次日 04:00 自动恢复探测
    HardCredit,
    /// 403 封禁（F-29 v1.2）：禁用账号，标注需人工确认
    Forbidden,
}

impl ErrKind {
    pub fn cooldown_duration(self) -> std::time::Duration {
        match self {
            ErrKind::PlanLimit => std::time::Duration::from_secs(12 * 3600),
            // soft_rate：60s 短冷却（§3.9 ②）
            ErrKind::SoftRate | ErrKind::NotFound => std::time::Duration::from_secs(60),
            ErrKind::SessionDead => std::time::Duration::from_secs(24 * 3600),
            // 熔断基础时长 30m；指数递增在 pool::note_error 内按 cb_trips 计算
            ErrKind::Server => std::time::Duration::from_secs(30 * 60),
            ErrKind::Client => std::time::Duration::from_secs(10 * 60),
            // 由 note_error 特殊处理（次日 04:00 / 直接禁用），无固定时长
            ErrKind::HardCredit | ErrKind::Forbidden | ErrKind::None => std::time::Duration::ZERO,
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
            ErrKind::HardCredit => "HardCredit",
            ErrKind::Forbidden => "Forbidden",
        }
    }
}

/// 4001/model config is empty：模型在当前 function 下不可用（模型问题非账号问题）
/// 判定依据：SOLO 业务错误码 4001 或上游 message 关键字。
/// 注意边界：`"code":4001` 后必须紧跟非数字字符，避免误匹配 40012 等其它错误码
pub fn is_model_config_mismatch(text: &str) -> bool {
    let lower = text.to_lowercase();
    if lower.contains("model config is empty") {
        return true;
    }
    let is_code_4001 = |key: &str| {
        let bytes = lower.as_bytes();
        let key_bytes = key.as_bytes();
        let mut from = 0;
        while let Some(pos) = lower[from..].find(key) {
            let after = from + pos + key_bytes.len();
            let ok = bytes
                .get(after)
                .map_or(true, |b| !(b.is_ascii_digit() || *b == b'.'));
            if ok {
                return true;
            }
            from = after;
        }
        false
    };
    is_code_4001("\"code\":4001")
        || is_code_4001("\"code\": 4001")
        || is_code_4001("\"error_code\":4001")
        || is_code_4001("\"error_code\": 4001")
}

/// 按 HTTP 状态码 + body 判定错误类别
pub fn classify_error(status: u16, body: &str) -> ErrKind {
    // 4001：模型问题非账号问题，不冷却账号
    if is_model_config_mismatch(body) {
        return ErrKind::None;
    }
    // 1005：精确匹配 JSON 键 + plan limit 短语（避免任意 "1005"/"plan" 字样误判）
    let lower_body = body.to_lowercase();
    if (body.contains("\"code\":1005") || body.contains("\"code\": 1005"))
        && (lower_body.contains("plan limit") || lower_body.contains("plan_limit"))
    {
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
    let msg_lower = msg.to_lowercase();
    // 1005: Plan 套餐额度用尽 → 12 小时冷却
    // 仅匹配 "plan limit/plan_limit/plan quota" 短语：宽泛 contains("plan") 会把
    // "planned maintenance" 等消息误判为套餐耗尽、触发 12 小时冷却
    if code == 1005
        || msg_lower.contains("plan limit")
        || msg_lower.contains("plan_limit")
        || msg_lower.contains("plan quota")
    {
        return ErrKind::PlanLimit;
    }
    // 4001: 模型配置不存在（model config is empty）→ 不冷却账号，是模型问题非账号问题
    if code == 4001 || msg_lower.contains("model config is empty") {
        return ErrKind::None;
    }
    // 4008: 请求频率超限（quota exceeded）→ 60 秒短冷却，避免误杀
    if code == 4008
        || msg_lower.contains("quota")
        || msg_lower.contains("exceeded")
        || msg_lower.contains("rate")
    {
        return ErrKind::SoftRate;
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

/// 流式上游 Agent：无总超时；连接 10s / 写 30s / 空闲读 300s，用于 SSE 流式对话
/// 注意：ureq 2.12 默认不读环境变量/系统代理（需显式 proxy-from-env feature），
/// 本 crate 未启用该 feature，天然直连，不会走本应用 127.0.0.1:8899 形成循环
pub fn streaming_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        // 空闲读超时仅约束单次 read 等待，SSE 正常流式（持续出包）不受影响；
        // 防止上游建立连接后长期不发数据，spawn_blocking 线程与客户端连接永久挂起。
        // 注意：timeout_read(Duration::from_secs(0)) 会触发 Rust std 的
        // "cannot set a 0 duration timeout" 错误，不能以 0 表示"禁用"
        .timeout_read(std::time::Duration::from_secs(300))
        .timeout_write(std::time::Duration::from_secs(30)) // 写超时 30s
        .timeout_connect(std::time::Duration::from_secs(10)) // 连接超时 10s
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

#[cfg(test)]
mod inflight_tests {
    use super::*;

    /// 正常路径：acquire/drop 配对，计数归零
    #[test]
    fn t01_guard_drop_decrements() {
        let c = Arc::new(AtomicU64::new(0));
        {
            let _g1 = InflightGuard::acquire(&c);
            let _g2 = InflightGuard::acquire(&c);
            assert_eq!(c.load(std::sync::atomic::Ordering::Relaxed), 2);
        }
        assert_eq!(c.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    /// panic 路径（§10 关键断言）：栈展开执行 guard Drop，计数不泄漏
    #[test]
    fn t02_guard_released_on_panic_unwind() {
        let c = Arc::new(AtomicU64::new(0));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = InflightGuard::acquire(&c);
            assert_eq!(c.load(std::sync::atomic::Ordering::Relaxed), 1);
            panic!("boom");
        }));
        assert!(result.is_err());
        assert_eq!(
            c.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "panic 展开时 guard Drop 必须释放计数"
        );
    }

    /// ApiSharedState::inflight_guard 便捷方法配对生效
    #[test]
    fn t03_state_helper_pairs() {
        let dir = std::env::temp_dir().join(format!("twa_inflight_{}", std::process::id()));
        let state = ApiSharedState {
            pool: pool::ApiPool::new(),
            wb_pool: pool::ApiPool::new(),
            wb_enabled: std::sync::atomic::AtomicBool::new(true),
            wb_sanitize: std::sync::atomic::AtomicBool::new(true),
            wb_default_thinking: std::sync::atomic::AtomicBool::new(false),
            wb_tool_exec: std::sync::atomic::AtomicBool::new(false),
            wb_bg_downgrade: std::sync::atomic::AtomicBool::new(false),
            wb_sticky: wb_sticky::StickyStore::default(),
            pool_sticky: Mutex::new(std::collections::HashMap::new()),
            model_cooldowns: Mutex::new(std::collections::HashMap::new()),
            wb_template_cache: Mutex::new(None),
            default_model: String::new(),
            data_dir: dir.clone(),
            total_requests: AtomicU64::new(0),
            inflight: Arc::new(AtomicU64::new(0)),
            active_uid: Mutex::new(None),
            last_error: Mutex::new(None),
            logger: ApiLogger::new(dir.join("logs")),
            debug_enabled: std::sync::atomic::AtomicBool::new(false),
            usage: Mutex::new(usage::UsageFile::default()),
            wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        };
        {
            let _g = state.inflight_guard();
            assert_eq!(state.inflight.load(std::sync::atomic::Ordering::Relaxed), 1);
        }
        assert_eq!(state.inflight.load(std::sync::atomic::Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
