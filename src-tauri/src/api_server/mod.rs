pub mod api_keys;
pub mod api_logger;
pub mod auth;
pub mod config_cache;
pub mod context_window;
pub mod custom_lb;
pub mod custom_models;
pub mod custom_route;
pub mod dispatch;
pub mod efforts;
pub mod gateway_settings;
pub mod models_sync;
pub mod org_index;
pub mod pool;
pub mod payload;
pub mod qoder_route;
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
    /// 长上下文降档（F-76④）：输入粗估 ≥100k token 的请求自动换 flash 档模型
    pub wb_longctx_downgrade: std::sync::atomic::AtomicBool,
    /// 慢请求竞速对冲阈值毫秒（F-76③，0 = 关闭；默认 8000，运行时 clamp 1s–8s）：
    /// 流式首字节超过该阈值且池内有其他健康账号时向第二账号发对冲请求
    pub wb_hedge_threshold_ms: std::sync::atomic::AtomicU64,
    /// Trae 池慢请求竞速对冲阈值毫秒（0 = 关闭；默认 8000，运行时 clamp 1s–8s），
    /// 与 Buddy/Qoder 池互不共享
    pub trae_hedge_threshold_ms: std::sync::atomic::AtomicU64,
    /// 池粘性 TTL 秒（F-76② per-pool，默认 300）：TTL 内同会话必落同一池同账号，
    /// 上游 KV cache 复用直接砍 prefill 时间。三池各自配置，record_sticky 按
    /// 胜出池取对应值（旧版单字段三池共用已拆分）
    pub trae_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64,
    /// Buddy 池粘性 TTL 秒（per-pool，默认 300）
    pub wb_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64,
    /// Qoder 池粘性 TTL 秒（per-pool，默认 300）
    pub qoder_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64,
    /// buddy-ai（国际版）池粘性 TTL 秒（per-pool，默认 300；§4.2 粘性按池独立）
    pub buddy_ai_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64,
    /// buddy-ai 竞速对冲阈值毫秒（per-pool 独立配置，0 = 关闭；WbView::route_hedge_ms
    /// 按 CN/Intl 档取值）
    pub buddy_ai_hedge_threshold_ms: std::sync::atomic::AtomicU64,
    /// buddy-ai 显式会话粘性开关（§4.2：qoder_sticky_enabled 同款独立开关，
    /// 默认关——国际档显式开启制；wb 档粘性恒开不受此字段影响）
    pub buddy_ai_sticky_enabled: std::sync::atomic::AtomicBool,
    /// 免费活动模型倍率覆盖（issue #111/#156，默认开）：开启时 Qwen3.8-Flash /
    /// DeepSeek-Flash 目录倍率按 0 处理——展示 0× + 取号豁免零积分过滤；上游
    /// 活动结束后关闭恢复官方同步倍率（api_pool.json.qoder_flash_free_rate 持久化）
    pub qoder_flash_free_rate: std::sync::atomic::AtomicBool,
    /// 会话粘性双模式存储（T2.4/F-31，仅 WB 上游消费）
    pub wb_sticky: wb_sticky::StickyStore,
    /// buddy-ai 显式会话粘性存储（§4.2：`g:` 前缀命名空间，与 wb/qoder/trae 互不串绑）
    pub buddy_ai_sticky: wb_sticky::StickyStore,
    /// 会话池粘性（统一网关 §4.4，内存态不落盘、重启即清）：
    /// session_key → (上次成功服务的资源池, 过期时间 Unix 秒)，TTL 60s 软粘
    pub pool_sticky: Mutex<std::collections::HashMap<String, (dispatch::TargetPool, i64)>>,
    /// 模型级冷却（F-34）：model → (until 秒, 连续失败次数)；10→20→40s 渐进退避，
    /// 优先级高于 Key 级冷却
    pub model_cooldowns: Mutex<std::collections::HashMap<String, (i64, u32)>>,
    /// buddy-ai（国际版）模型级冷却独立实例（§4.2：Intl 池冷却表独立——同名模型
    /// 跨区不混轮，CN 侧模型故障不得冷却 Intl 池同型号，反之亦然）
    pub buddy_ai_model_cooldowns: Mutex<std::collections::HashMap<String, (i64, u32)>>,
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
    /// 用量统计（内存累积；落盘由持久化 flusher 削峰，网关性能批次 C）
    pub usage: Mutex<usage::UsageFile>,
    /// 用量待落盘脏队列（批次 C）：记账只记内存 + 标记 (桶, 日)，
    /// 后台 flusher（2s）与 stop 时统一落盘，消除每请求一次 SQLite 写事务
    pub usage_dirty: Mutex<Vec<(usage::UsageBucket, String)>>,
    /// WB 上游健康探针（F-34 ④/§2.2 频控）：最近探测 Unix 毫秒；-1 = 尚未探测
    pub wb_probe_ts_ms: std::sync::atomic::AtomicI64,
    /// WB 上游健康探针结果：-1 未探测 / 0 不可达 / 1 在线
    pub wb_probe_ok: std::sync::atomic::AtomicI64,
    /// buddy-ai（国际版）上游健康探针（G8 审查修复）：与 wb_probe 同频控口径，
    /// 探测 BILLING_BASE_GLOBAL（workbuddy.ai）；-1 未探测 / 0 不可达 / 1 在线
    pub buddy_ai_probe_ts_ms: std::sync::atomic::AtomicI64,
    /// buddy-ai（国际版）上游健康探针结果：-1 未探测 / 0 不可达 / 1 在线
    pub buddy_ai_probe_ok: std::sync::atomic::AtomicI64,
    /// 跨池回退日志节流（审查中等项 + 二审细化）：key = "model|reason"，
    /// value = 上次落盘 Unix 毫秒；60s 窗口内同 model 同因回退只落盘一次，
    /// 防逐请求刷盘放大；不同 model/reason 互不抑制
    pub fallback_log_last_ms:
        Mutex<std::collections::HashMap<String, i64>>,
    /// Trae 上游 401 自愈回调（issue #27 方案 B；网关层无 AppState 引用，由启动方注入）：
    /// 入参 user_id，内部走 refresh_jwt_impl 全防护链路强制刷新并持久化。
    /// None（单测/降级构造）时 401 维持原「note_error + 换号」语义
    pub trae_jwt_refresh: Option<std::sync::Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>>,
    /// Trae 池参与调度开关（api_pool.json.trae_enabled，默认开）：关闭后 Trae 目录
    /// 模型不路由 Trae 池（每个资源池管理自己的开关，与 wb_enabled/qoder_enabled 同族）
    pub trae_enabled: std::sync::atomic::AtomicBool,
    // ── Qoder 上游（p3-3，gateway.qoder.com.cn 推理网关）─────────────────
    /// Qoder 上游账号池：独立池实例与 SOLO/WB 并列；Qoder 无积分余额概念，
    /// 调度只看冷却/禁用/在途。凭证正文不在池内（access_token 会过期），
    /// 请求时经 qoder_identity 回调解析
    pub qoder_pool: ApiPool,
    /// Qoder 上游开关（api_pool.json.qoder_enabled；关闭时 Qoder 模型不参与路由）
    pub qoder_enabled: std::sync::atomic::AtomicBool,
    /// Qoder 凭证解析回调（网关层无 AppState 引用，由启动方注入）：
    /// 入参账号 uid（token store 键），返回经 secure 回填与 effective 合并的
    /// 完整凭证（PAT 换 24h 作业令牌 / 刷新链路在闭包内走全防护）。
    /// None（单测/未启用 Qoder）时 Qoder 池不参与路由
    pub qoder_identity:
        Option<std::sync::Arc<dyn Fn(&str) -> Result<crate::tasks::qoder_common::QoderCreds, String> + Send + Sync>>,
    /// Qoder 慢请求竞速对冲阈值毫秒（F-80-余 v2，0 = 关闭；默认 8000，
    /// 运行时 clamp 1s–8s 同 WB）：流式/聚合首字节超阈值且池内有其他健康账号时
    /// 向第二账号发对冲请求，先出首字者胜
    pub qoder_hedge_threshold_ms: std::sync::atomic::AtomicU64,
    /// Qoder 会话粘性开关（F-80-余 v2，默认关）：开启后同会话（conversationId /
    /// 消息指纹）TTL 内绑定同一 Qoder 账号；同账号 + 同种子派生同一上游 session_id
    pub qoder_sticky_enabled: std::sync::atomic::AtomicBool,
    /// Qoder 会话粘性存储（F-80-余 v2）：与 wb_sticky 共用 sticky_bindings 表，
    /// 键命名空间 "q:" 隔离，互不串绑
    pub qoder_sticky: wb_sticky::StickyStore,
    /// Qoder 池指纹清洗开关（issue #158，默认开）：请求侧 sanitize + 模板映射 +
    /// 工具别名正向改写、响应侧逆映射回改；与 Trae 池 wb_sanitize 独立（池级
    /// 风控画像不同，分开治理），UI「指纹清洗」tab 池级开关热切换
    pub qoder_sanitize: std::sync::atomic::AtomicBool,
    // ── Buddy 国际版（buddy-ai）独立池（§3.4 隔离清单）─────────────────────
    /// buddy-ai 上游账号池：与 wb 池并列的独立池实例（§3.1 硬约束 1：禁止混居；
    /// 阶段 1 仅注册 + healthz selectable，dispatch 接线在阶段 2）
    pub buddy_ai_pool: ApiPool,
    /// buddy-ai 上游开关（api_pool.json.buddy_ai_enabled；默认关——dispatch
    /// 「buddy-ai」档显式开启制，由用户在 API 服务页显式开启国际档）
    pub buddy_ai_enabled: std::sync::atomic::AtomicBool,
    /// Trae 池会话粘性存储（per-pool 会话粘性批次新增）：与 wb/qoder sticky 共用
    /// sticky_bindings 表，键命名空间 "t:" 隔离；会话→账号绑定供 Trae 取号粘住
    ///（显式 conversationId / 消息指纹双模式，TTL 经 trae_sticky_ttl_secs 配置）
    pub trae_sticky: wb_sticky::StickyStore,
    /// 实际绑定的监听地址（2026-10-12 审查 M3）：服务启动时从 gateway 配置读入
    /// 一次注入。auth 层的 /health 环回豁免判定改读本字段——与 server.rs 的 bind
    /// 物理同源，改配置未重启时不再出现「配置值 vs 实际监听值」分离导致的
    /// fail-open 误判（配置改非环回但服务仍绑环回时按实际值豁免，反之按实际值走鉴权）
    pub listen_host: std::sync::Mutex<String>,
    /// custom 上游组会话粘上游（P2）：session_key → (cm.id, 过期 Instant)，
    /// TTL 60s，内存态不落盘（与池粘性同口径；custom_lb 模块消费）
    pub custom_upstream_sticky:
        Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
    /// custom 上游组加权轮询计数（P2）：canonical 模型名 → 组内共享计数器——
    /// 按模型分组各计各的，避免单全局计数器跨组交替破坏组内分布
    pub custom_lb_counters: Mutex<std::collections::HashMap<String, std::sync::atomic::AtomicUsize>>,
}

/// 测试专用：全字段默认构造（custom_route 端到端轮换测试等无 fixture 的
/// 测试模块复用；开关取生产默认值，池为空池）
#[cfg(test)]
pub(crate) fn test_shared_state(data_dir: std::path::PathBuf) -> ApiSharedState {
    ApiSharedState {
        pool: pool::ApiPool::new(),
        wb_pool: pool::ApiPool::new(),
        wb_enabled: std::sync::atomic::AtomicBool::new(true),
        wb_sanitize: std::sync::atomic::AtomicBool::new(true),
        wb_default_thinking: std::sync::atomic::AtomicBool::new(false),
        wb_tool_exec: std::sync::atomic::AtomicBool::new(false),
        wb_bg_downgrade: std::sync::atomic::AtomicBool::new(false),
        wb_longctx_downgrade: std::sync::atomic::AtomicBool::new(false),
        wb_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
        trae_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
        trae_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(0),
        wb_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(0),
        qoder_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(0),
        buddy_ai_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(0),
        buddy_ai_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
        buddy_ai_sticky_enabled: std::sync::atomic::AtomicBool::new(false),
        qoder_flash_free_rate: std::sync::atomic::AtomicBool::new(true),
        wb_sticky: wb_sticky::StickyStore::default(),
        buddy_ai_sticky: wb_sticky::StickyStore::default(),
        pool_sticky: Mutex::new(std::collections::HashMap::new()),
        model_cooldowns: Mutex::new(std::collections::HashMap::new()),
        buddy_ai_model_cooldowns: Mutex::new(std::collections::HashMap::new()),
        default_model: String::new(),
        logger: ApiLogger::new(data_dir.join("logs")),
        data_dir,
        total_requests: AtomicU64::new(0),
        inflight: Arc::new(AtomicU64::new(0)),
        active_uid: Mutex::new(None),
        last_error: Mutex::new(None),
        debug_enabled: std::sync::atomic::AtomicBool::new(false),
        usage: Mutex::new(usage::UsageFile::default()),
        usage_dirty: Mutex::new(Vec::new()),
        wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
        wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        buddy_ai_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
        buddy_ai_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        fallback_log_last_ms: Mutex::new(std::collections::HashMap::new()),
        trae_jwt_refresh: None,
        trae_enabled: std::sync::atomic::AtomicBool::new(true),
        qoder_pool: pool::ApiPool::new(),
        qoder_enabled: std::sync::atomic::AtomicBool::new(true),
        qoder_identity: None,
        qoder_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
        qoder_sticky_enabled: std::sync::atomic::AtomicBool::new(false),
        qoder_sticky: wb_sticky::StickyStore::default(),
        qoder_sanitize: std::sync::atomic::AtomicBool::new(true),
        trae_sticky: wb_sticky::StickyStore::default(),
        buddy_ai_pool: pool::ApiPool::new(),
        buddy_ai_enabled: std::sync::atomic::AtomicBool::new(false),
        listen_host: Mutex::new("127.0.0.1".to_string()),
        custom_upstream_sticky: Mutex::new(std::collections::HashMap::new()),
        custom_lb_counters: Mutex::new(std::collections::HashMap::new()),
    }
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
        self.record_usage_ttfb(
            is_wb, model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens,
            completion_tokens, None,
        );
    }

    /// 记录一次请求用量（带 TTFT，F-76①）：流式路径已知首字耗时（毫秒）时使用，
    /// 供用量页 P50/P95/TTFT 分位统计；`ttfb_ms=None` 等价 record_usage
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage_ttfb(
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
        ttfb_ms: Option<u64>,
    ) {
        let mut guard = self
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.record_ttfb(
            is_wb, model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens,
            completion_tokens, ttfb_ms,
        );
        // 批次 C 削峰：只记内存 + 标脏，SQLite 落盘由 flusher（2s）/stop 时统一执行
        let bucket = if is_wb { usage::UsageBucket::Wb } else { usage::UsageBucket::Trae };
        let day = usage::today_key();
        drop(guard);
        self.usage_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((bucket, day));
    }

    /// 记录一次请求用量（按桶，buddy-ai 档记账分区用，§3.1 硬约束 5）；落盘策略同 record_usage
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage_bucket(
        &self,
        bucket: usage::UsageBucket,
        model: &str,
        uid: &str,
        key_id: &str,
        ok: bool,
        is_stream: bool,
        duration_ms: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        self.record_usage_bucket_ttfb(
            bucket, model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens,
            completion_tokens, None,
        );
    }

    /// 记录一次请求用量（按桶 + TTFT）：wb_route 池视图分流记账的唯一入口
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage_bucket_ttfb(
        &self,
        bucket: usage::UsageBucket,
        model: &str,
        uid: &str,
        key_id: &str,
        ok: bool,
        is_stream: bool,
        duration_ms: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
        ttfb_ms: Option<u64>,
    ) {
        let mut guard = self
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.record_in_ttfb(
            bucket, model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens,
            completion_tokens, ttfb_ms,
        );
        // 批次 C 削峰：只记内存 + 标脏，SQLite 落盘由 flusher（2s）/stop 时统一执行
        let day = usage::today_key();
        drop(guard);
        self.usage_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((bucket, day));
    }

    /// 记录一次自定义模型请求用量（独立 custom_days 桶，与 Trae/WB 侧分账）；
    /// uid 固定 "custom"，落盘策略与 record_usage 相同。
    /// `session`：请求级会话上下文（issue #149 P2）——同步直写 custom_sessions 流水
    /// （request_id 内部生成 uuid，纯 INSERT；写失败静默，不影响记账主路径）
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
        session: &usage::CustomSessionCtx,
    ) {
        // day 先取定（同 record_usage_qoder）：用量行、流水行与标脏行共用同一日期键，
        // 杜绝跨零点时三者分裂到不同日期
        let day = usage::today_key();
        let mut guard = self
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.record_in(
            usage::UsageBucket::Custom,
            model, "custom", key_id, ok, is_stream, duration_ms, prompt_tokens, completion_tokens,
        );
        // 批次 C 削峰：同 record_usage_ttfb，仅标脏不落盘
        drop(guard);
        // issue #149 P2：请求级流水同步落库（静默，不影响记账主路径）
        let _ = crate::store::docs::custom_session_insert(
            &crate::store::db(&self.data_dir),
            &uuid::Uuid::new_v4().to_string(),
            &session.session_id,
            &session.uid,
            &day,
            chrono::Local::now().timestamp(),
            model,
            session.stage,
            &session.key_name,
            prompt_tokens as i64,
            completion_tokens as i64,
            duration_ms as i64,
            is_stream,
            ok,
            &session.preview,
        );
        self.usage_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((usage::UsageBucket::Custom, day));
    }

    /// 记录一次上游缓存读 tokens 测量（M4 审查修复：DayStats 新增 cached_tokens
    /// 维度的旁路入口——record_usage 系列签名被 15+ 调用点依赖，不扩参；
    /// 上游未下发 usage 的调用方不调用本方法，0 值不入账不标脏）
    pub fn record_usage_cached(&self, bucket: usage::UsageBucket, cached: u64) {
        if cached == 0 {
            return;
        }
        let mut guard = self
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 审查修复（用量跨日归行）：day 在持锁期间取定一次，cached_tokens 写入
        // 与标脏落盘共用同一日期键，杜绝跨零点时写入行与标脏行分裂到两日
        let day = usage::today_key();
        guard.record_cached_in(bucket, &day, cached);
        drop(guard);
        self.usage_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((bucket, day));
    }

    /// 记录一次 Qoder 上游请求用量（独立 qoder_days 桶，与 Trae/WB/Custom 侧分账）；
    /// uid 用 Qoder 账号 uid，落盘策略与 record_usage 相同。
    /// `credits`/`original_credits`：上游 usage 中的积分测量（issue #82 T2），
    /// 上游未下发 usage 时传 (None, None) 不入账（不得伪造）
    /// `session`：请求级会话上下文（issue #82 问题5）——同步直写 qoder_sessions 流水
    /// （UPSERT：ON CONFLICT(request_id) DO UPDATE 且仅新 ts 覆盖旧行，幂等；
    /// 写失败静默，不影响记账主路径）
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage_qoder(
        &self,
        model: &str,
        uid: &str,
        key_id: &str,
        ok: bool,
        is_stream: bool,
        duration_ms: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
        // F-76① 同构 TTFT 入账（审查补齐）：首字耗时；无首字语义的失败（no-healthy/
        // identity 未达首字）传 None
        ttfb_ms: Option<u64>,
        credits: Option<f64>,
        original_credits: Option<f64>,
        session: &usage::QoderSessionCtx,
    ) {
        let day = usage::today_key();
        let mut guard = self
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.record_credits_qoder(model, &day, credits, original_credits);
        // G3：ttfb 统计用入口取定的 day（与积分/流水同行），不自取 today 分裂日期行
        guard.record_in_ttfb_on(
            usage::UsageBucket::Qoder,
            &day,
            model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens, completion_tokens,
            ttfb_ms,
        );
        // 批次 C 削峰：同 record_usage_custom，仅标脏不落盘
        drop(guard);
        let _ = crate::store::docs::qoder_session_insert(
            &crate::store::db(&self.data_dir),
            &session.request_id,
            &session.session_id,
            uid,
            &day,
            session.ts,
            model,
            credits.unwrap_or(0.0),
            prompt_tokens as i64,
            completion_tokens as i64,
            duration_ms as i64,
            is_stream,
            ok,
            &session.preview,
            &session.record_id,
        );
        self.usage_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((usage::UsageBucket::Qoder, day));
    }

    /// 排空用量脏队列并落盘（flusher 线程 2s 一次 + stop 时调用）。
    /// 按 (bucket, day) 去重后每桶只写一次 SQLite 事务；锁内只克隆快照，
    /// SQLite 写在锁外执行，避免拖住记账路径
    pub fn flush_usage_dirty(&self) {
        let dirty: std::collections::HashSet<(usage::UsageBucket, String)> = {
            let mut q = self
                .usage_dirty
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *q).into_iter().collect()
        };
        if dirty.is_empty() {
            return;
        }
        let snap: Vec<(usage::UsageBucket, String, usage::DayStats)> = {
            let guard = self.usage.lock().unwrap_or_else(|e| e.into_inner());
            dirty
                .into_iter()
                .map(|(b, d)| {
                    let stats = guard.day_stats(b, &d).cloned().unwrap_or_default();
                    (b, d, stats)
                })
                .collect()
        };
        let mut failed: Vec<(usage::UsageBucket, String)> = Vec::new();
        for (bucket, day, stats) in snap {
            if !usage::save_day(&self.data_dir, bucket, &day, &stats) {
                failed.push((bucket, day));
            }
        }
        if !failed.is_empty() {
            // 落盘失败：恢复脏标记由 flusher 下轮重试（R3），不静默丢当批数据
            self.usage_dirty
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(failed);
            self.logger.log_debug_line(
                "[WARN] 用量落盘失败（已恢复脏标记待重试）".to_string(),
            );
        }
    }

    /// 全量持久化收尾（服务 stop / 应用退出时调用）：
    /// 用量脏队列 + api_keys 计数 + API 日志队列一次性排空
    pub fn flush_pending_writes(&self) {
        self.flush_usage_dirty();
        api_keys::flush_dirty(&self.data_dir);
        self.logger.flush();
    }
}

/// 当前并发计数 RAII guard（统一网关 §4.5）：构造时 +1，Drop 时 -1。
/// 持有 Arc 克隆（'static + Send）——端点获取后作为参数移入执行路径，
/// 流式场景随 spawn 任务存续至流结束；不做下溢防护依赖"构造必 +1"配对语义
///
/// F-77 双维护扩展：取号成功后通过 `bind_account` 绑定账号级在途计数
/// （进入 +1 / Drop -1），实现 busy 让位 idle 的账号并发感知调度；
/// 未绑定时行为与原版完全一致（仅全局计数）
pub struct InflightGuard {
    counter: Arc<AtomicU64>,
    /// 账号级在途计数（F-77）：账号 uid → 计数器的共享句柄（存于 ApiPool）
    account: Option<Arc<std::sync::atomic::AtomicU32>>,
}

impl InflightGuard {
    pub fn acquire(counter: &Arc<AtomicU64>) -> InflightGuard {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        InflightGuard {
            counter: counter.clone(),
            account: None,
        }
    }

    /// F-77：取号成功后绑定账号在途计数（+1）；guard 存续至请求/流结束。
    /// 重复绑定（重试换号）时先解绑旧账号再绑新账号，计数不泄漏
    pub fn bind_account(mut self, acct: Arc<std::sync::atomic::AtomicU32>) -> InflightGuard {
        acct.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(old) = self.account.replace(acct) {
            old.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        self
    }

    /// TOCTOU 收紧版绑定（审查 P1-4）：仅当账号在途 < limit 时 CAS +1 并绑定，
    /// 返回是否绑定成功；limit = 0 视为未启用并发上限（无条件绑定）。
    /// 失败时 guard 保持原绑定不变，由调用方换下一候选（tried 已含该 uid，
    /// 轮换自然排除，不会无限循环）。CAS 循环消除「取号快照 → 绑定」窗口内
    /// 多请求同时通过 busy 过滤导致的同账号超载
    pub fn bind_account_cas(&mut self, acct: Arc<std::sync::atomic::AtomicU32>, limit: u32) -> bool {
        let mut cur = acct.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            if limit > 0 && cur >= limit {
                return false; // 在途已满：不绑定、不覆盖原账号绑定
            }
            match acct.compare_exchange_weak(
                cur,
                cur + 1,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        if let Some(old) = self.account.replace(acct) {
            old.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        true
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(acct) = self.account.take() {
            acct.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
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
    /// 付费受限（issue #145）：模型仅付费账号可用（如 Kimi K3.1 Pro），
    /// 属账号订阅档位与模型的固有不匹配——不冷却不禁用（note_error no-op），
    /// 由 routes 层换号重试；根治手段是渠道内单模型开关（TraeModelMeta.enabled）
    PaidRestricted,
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
            // 由 note_error 特殊处理（次日 04:00 / 直接禁用 / no-op），无固定时长
            ErrKind::HardCredit
            | ErrKind::Forbidden
            | ErrKind::PaidRestricted
            | ErrKind::None => std::time::Duration::ZERO,
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
            ErrKind::PaidRestricted => "PaidRestricted",
        }
    }

    /// 账号级长冷却/禁用（issue #67）：该类错误应解绑账号的全部粘性绑定，
    /// 避免其他会话在 TTL 窗口内反复解析到不可用账号。SoftRate/Server/Client
    /// 等短冷却保留绑定（冷却过后会话可无缝续接）。
    pub fn unbinds_sticky(self) -> bool {
        matches!(
            self,
            ErrKind::HardCredit | ErrKind::SessionDead | ErrKind::Forbidden
        )
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

/// 积分耗尽短语表（issue #67）：与 wb_route::classify_wb_error 对齐。
/// 有意不含 "quota exceeded"——Trae 侧 4008 quota=频率限流（SoftRate 60s），
/// 归入 HardCredit（次日 04:00 冷却）会误杀
pub fn is_credit_exhausted_error(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("insufficient credit")
        || lower.contains("积分不足")
        || lower.contains("额度不足")
}

/// 付费受限短语表（issue #145）：模型仅付费账号/付费套餐可用（如 Kimi K3.1 Pro）。
/// 命中 → PaidRestricted：不冷却账号（免费账号调度付费模型必败是固有不匹配，
/// 冷却 10 分钟只会白白缩减可用池），由 routes 层换号重试、管理端单模型开关根治。
/// 有意采用保守长短语（吸取 contains("plan") 误判 planned maintenance 的教训），
/// 且调用方排在显式配额信号（积分耗尽/1005 plan limit）之后，防
/// "plan limit exceeded, upgrade your plan" 类配额报文被归为付费受限
pub fn is_paid_restricted_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("requires a paid")
        || lower.contains("paid plan")
        || lower.contains("paid subscription")
        || lower.contains("upgrade to a paid")
        || lower.contains("仅限付费")
        || lower.contains("付费用户")
        || lower.contains("付费账号")
        || lower.contains("升级套餐")
        || lower.contains("套餐不支持")
}

/// 按 HTTP 状态码 + body 判定错误类别
pub fn classify_error(status: u16, body: &str) -> ErrKind {
    // 4001：模型问题非账号问题，不冷却账号
    if is_model_config_mismatch(body) {
        return ErrKind::None;
    }
    // 积分耗尽（issue #67）：短语命中 → 硬冷却，优先于状态码口径
    if is_credit_exhausted_error(body) {
        return ErrKind::HardCredit;
    }
    // 11140 账号级风控（issue #136 反馈）：账号被上游限制使用 → Forbidden
    // 禁用为内存态，无手动启用入口——申诉处理后重启应用或重新登录账号即随
    // 池快照重建恢复调度；优先于状态码口径，该错误以 HTTP 400 形态到达时
    // 默认 400 → Client 仅短冷却，账号会被持续调度每请求必败
    if retry::is_account_restricted_error(body) {
        return ErrKind::Forbidden;
    }
    // 1005：精确匹配 JSON 键 + plan limit 短语（避免任意 "1005"/"plan" 字样误判）
    let lower_body = body.to_lowercase();
    if (body.contains("\"code\":1005") || body.contains("\"code\": 1005"))
        && (lower_body.contains("plan limit") || lower_body.contains("plan_limit"))
    {
        return ErrKind::PlanLimit;
    }
    // 付费受限（issue #145）：排在显式配额信号之后，防配额报文误判；
    // 不冷却账号（PaidRestricted → note_error no-op）
    if is_paid_restricted_error(body) {
        return ErrKind::PaidRestricted;
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
    // 积分耗尽（issue #67）：流内 message 短语 → 硬冷却（次日 04:00），
    // 避免落入 4008 SoftRate（60s）冷却后很快被再次选中、循环薅同一账号
    if is_credit_exhausted_error(msg) {
        return ErrKind::HardCredit;
    }
    // 付费受限（issue #145）：排在显式配额信号之后、4008 宽泛分支之前
    //（防 "exceeded" 等宽泛词把付费受限报文归为 SoftRate 误冷却）
    if is_paid_restricted_error(msg) {
        return ErrKind::PaidRestricted;
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

// ==================== 空完成识别（issue #57：影子风控） ====================

/// 空完成哨兵码：上游 HTTP 200 正常收流但零内容（影子风控静默拦截 / 上游异常）。
/// 取负数避免与上游业务码（正整数）冲突；SSE 转换层不发收尾帧、以该哨兵上抛，
/// 调用方按可重试失败换号处理（is_empty_completion 识别）
///
/// # 处置口径（有意按路径分轨，均为换号重试且 tried 集合有界无循环风险）
///
/// - **流式**（wb_route/routes 的 stream 路径）：不冷却、不透传、不绑定粘性——
///   流内判定可能受协议占位帧干扰（如 Responses 协议 created 帧），冷却从严
///   以免误伤健康账号；
/// - **聚合**（wb_aggregate/aggregate 路径）：note_error(Server) 短冷却——
///   整响应零内容判定最确凿，与既有「empty response」处置同口径；
/// - **toolexec**（wb_tool_exec_chat）：不冷却、补观测日志——多轮代执行按
///   换号自愈，日志接通「模板命中」反查通道（issue #57 迭代依赖）。
pub const EMPTY_COMPLETION_CODE: i64 = -9901;
pub const EMPTY_COMPLETION_MSG: &str =
    "empty completion: upstream returned a completed response with no content";

/// 判定是否空完成哨兵（流式转换层上抛 / 聚合响应零内容共用）
pub fn is_empty_completion(code: i64, msg: &str) -> bool {
    code == EMPTY_COMPLETION_CODE || msg.starts_with("empty completion:")
}

// ==================== 传输层掐断识别（issue #158 报错A，P2-1） ====================

/// 传输层掐断哨兵码：上游 h2 RST_STREAM / 连接重置等传输层错误掐断长流。
/// 与账号质量无关（classify_solo_error 对该码落 Server 仅为 rotating 计算
/// 兜底），调用方 routes 须在 classify 之前经 is_transport_cut 预检——
/// 不冷却、不解绑粘性、不计空完成指纹，按 sent_any 分流（就地收尾/换号）
pub const TRANSPORT_CUT_CODE: i64 = -9902;
pub const TRANSPORT_CUT_MSG_PREFIX: &str = "transport cut:";

/// 传输层掐断文案识别（issue #158 报错A：Trae 池 ~5min 长流被上游
/// RST_STREAM 掐断）。仅作用于 io::Error 的 Display 文本（reqwest/hyper
/// 传输层错误消息），不作用于上游业务错误消息——业务侧 4xx/5xx 文本即使
/// 含相近词也不会走到这里（读错误只可能来自传输层）
pub fn is_transport_cut_text(text: &str) -> bool {
    let m = text.to_ascii_lowercase();
    m.contains("rst_stream")
        || m.contains("rst stream")
        || m.contains("stream error")
        || m.contains("http2")
        || m.contains("h2 error")
        || m.contains("goaway")
        || m.contains("connection reset")
        || m.contains("broken pipe")
        || m.contains("connection closed before")
}

/// 判定是否传输层掐断哨兵（对齐 is_empty_completion 双口径：码相等 ||
/// msg 前缀；sse 层上抛 / routes 消费共用）
pub fn is_transport_cut(code: i64, msg: &str) -> bool {
    code == TRANSPORT_CUT_CODE || msg.starts_with(TRANSPORT_CUT_MSG_PREFIX)
}

/// 从 wb_upstream 读错误共享槽构建传输掐断哨兵（issue #158 P2-2）：
/// 竞速桥接/tracked 行源路径的读错误被 Box 擦拭类型后，wb_sse 层只见桥
/// EOF，路由层在 EOF 兜底处经此取出上抛。槽空返回 None（正常完成）
pub fn transport_cut_from_slot(
    slot: &crate::api_server::wb_upstream::ReadErrSlot,
) -> Option<(i64, String)> {
    crate::api_server::wb_upstream::take_read_error_slot(slot)
        .map(|e| (TRANSPORT_CUT_CODE, format!("{TRANSPORT_CUT_MSG_PREFIX} {e}")))
}

/// 聚合响应是否零内容（OpenAI chat / legacy text / Anthropic message 三形态）：
/// 无正文 + 无思考链 + 无工具调用即视为空完成（换号重试）。
/// legacy `message.function_call` 一并判定（防御性：聚合器当前会把
/// function_call 归一为 tool_calls，此处兜底防未来透传形态漏判）
pub fn aggregated_response_is_empty(r: &serde_json::Value) -> bool {
    // Anthropic message：顶层 content 块数组
    if let Some(blocks) = r.get("content").and_then(|c| c.as_array()) {
        return blocks.is_empty();
    }
    // OpenAI chat（message.content/reasoning_content/tool_calls/function_call）与
    // legacy text（choices[0].text）统一判定（缺字段视为空）
    let empty_at = |p: &str| -> bool {
        r.pointer(p)
            .and_then(|v| v.as_str())
            .map_or(true, str::is_empty)
    };
    empty_at("/choices/0/message/content")
        && empty_at("/choices/0/message/reasoning_content")
        && r.pointer("/choices/0/message/tool_calls")
            .and_then(|t| t.as_array())
            .map_or(true, |a| a.is_empty())
        && r.pointer("/choices/0/message/function_call")
            .map_or(true, serde_json::Value::is_null)
        && empty_at("/choices/0/text")
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

/// 流式转发专用阻塞线程池（网关性能批次 D-1，线程隔离）：
/// 每条流（含 F-76 对冲请求）独占一个阻塞线程直至流结束（读空闲上限 300s）。
/// 若与鉴权中间件 / 短 IO 共用主 runtime 的 spawn_blocking 池（默认上限 512），
/// 并发流可将其耗尽，导致鉴权排队、整个网关级联卡死。
///
/// 本池独立于 Tauri 主 runtime：长流占满本池（上限 256）时，主池的
/// 鉴权 / /v1/models / 用量快照等短阻塞任务不受影响。池内闭包中的
/// `tokio::spawn`（keep-alive tick 等）调度到本 runtime 的 async worker。
pub fn stream_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(256)
            .enable_all()
            .build()
            .expect("stream runtime 初始化失败")
    })
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
            wb_longctx_downgrade: std::sync::atomic::AtomicBool::new(false),
            wb_hedge_threshold_ms: AtomicU64::new(0),
            trae_hedge_threshold_ms: AtomicU64::new(0),
            trae_pool_sticky_ttl_secs: AtomicU64::new(0),
            wb_pool_sticky_ttl_secs: AtomicU64::new(0),
            qoder_pool_sticky_ttl_secs: AtomicU64::new(0),
            qoder_flash_free_rate: std::sync::atomic::AtomicBool::new(true),
            wb_sticky: wb_sticky::StickyStore::default(),
            pool_sticky: Mutex::new(std::collections::HashMap::new()),
            model_cooldowns: Mutex::new(std::collections::HashMap::new()),
            buddy_ai_model_cooldowns: Mutex::new(std::collections::HashMap::new()),
            default_model: String::new(),
            data_dir: dir.clone(),
            total_requests: AtomicU64::new(0),
            inflight: Arc::new(AtomicU64::new(0)),
            active_uid: Mutex::new(None),
            last_error: Mutex::new(None),
            logger: ApiLogger::new(dir.join("logs")),
            debug_enabled: std::sync::atomic::AtomicBool::new(false),
            usage: Mutex::new(usage::UsageFile::default()),
            usage_dirty: Mutex::new(Vec::new()),
            wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
            buddy_ai_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            buddy_ai_probe_ok: std::sync::atomic::AtomicI64::new(-1),
            fallback_log_last_ms: Mutex::new(std::collections::HashMap::new()),
            trae_jwt_refresh: None,
            trae_enabled: std::sync::atomic::AtomicBool::new(true),
            qoder_pool: pool::ApiPool::new(),
            qoder_enabled: std::sync::atomic::AtomicBool::new(true),
            qoder_identity: None,
            qoder_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            qoder_sticky_enabled: std::sync::atomic::AtomicBool::new(false),
            qoder_sticky: wb_sticky::StickyStore::default(),
            qoder_sanitize: std::sync::atomic::AtomicBool::new(true),
            trae_sticky: wb_sticky::StickyStore::default(),
            buddy_ai_sticky: wb_sticky::StickyStore::default(),
            buddy_ai_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            buddy_ai_pool: pool::ApiPool::new(),
            buddy_ai_enabled: std::sync::atomic::AtomicBool::new(false),
            buddy_ai_sticky_enabled: std::sync::atomic::AtomicBool::new(false),
            buddy_ai_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            listen_host: std::sync::Mutex::new("127.0.0.1".to_string()),
            custom_upstream_sticky: Mutex::new(std::collections::HashMap::new()),
            custom_lb_counters: Mutex::new(std::collections::HashMap::new()),
        };
        {
            let _g = state.inflight_guard();
            assert_eq!(state.inflight.load(std::sync::atomic::Ordering::Relaxed), 1);
        }
        assert_eq!(state.inflight.load(std::sync::atomic::Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 审查 P1-4：CAS 绑定——未达上限绑定成功；已达上限拒绝且不覆盖原绑定；
    /// limit=0 视为未启用上限（无条件绑定）
    #[test]
    fn t04_bind_account_cas_respects_limit() {
        let c = Arc::new(AtomicU64::new(0));
        let acct = Arc::new(std::sync::atomic::AtomicU32::new(0));
        // 空闲账号：CAS 绑定成功，计数 +1
        let mut g = InflightGuard::acquire(&c);
        assert!(g.bind_account_cas(acct.clone(), 1));
        assert_eq!(acct.load(std::sync::atomic::Ordering::Relaxed), 1);
        // 已达上限（cur=1 >= limit=1）：拒绝，guard 原绑定保持（Drop 时减 1 归零）
        let mut g2 = InflightGuard::acquire(&c);
        assert!(!g2.bind_account_cas(acct.clone(), 1));
        assert_eq!(acct.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(g2.account.is_none(), "拒绝时不得覆盖/解绑原账号绑定");
        // limit=0：未启用上限，无条件绑定
        assert!(g2.bind_account_cas(acct.clone(), 0));
        assert_eq!(acct.load(std::sync::atomic::Ordering::Relaxed), 2);
        // 换绑 + Drop 配对：旧账号解绑、新账号随 Drop 释放
        drop(g);
        assert_eq!(acct.load(std::sync::atomic::Ordering::Relaxed), 1);
        drop(g2);
        assert_eq!(acct.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}

/// issue #145 错误分类测试：付费受限（PaidRestricted）识别与排序防误判
#[cfg(test)]
mod classify_tests {
    use super::*;

    /// 付费受限长短语命中与保守性（不含付费语义的报文不误判）
    #[test]
    fn t01_paid_restricted_phrases() {
        assert!(is_paid_restricted_error(
            "This model requires a paid subscription to use."
        ));
        assert!(is_paid_restricted_error("请升级套餐后再使用该模型"));
        assert!(is_paid_restricted_error("该模型仅限付费用户使用"));
        // 保守性：宽泛 "plan" 字样不命中（吸取 contains("plan") 误判 planned
        // maintenance 的教训）
        assert!(!is_paid_restricted_error("planned maintenance"));
        assert!(!is_paid_restricted_error("plan limit exceeded"));
    }

    /// classify_error：付费受限在状态码兜底（403/400 → Forbidden/Client）之前
    /// 识别——否则免费账号调度付费模型会被 403/400 兜底误冷却
    #[test]
    fn t02_error_paid_restricted_before_status_fallback() {
        assert_eq!(
            classify_error(403, "model requires a paid subscription"),
            ErrKind::PaidRestricted
        );
        assert_eq!(
            classify_error(400, "该模型仅限付费账号使用"),
            ErrKind::PaidRestricted
        );
    }

    /// 排序防误判①：显式配额信号（积分耗尽 / 1005 plan limit）优先于付费受限，
    /// "plan limit exceeded, upgrade your plan" 类配额报文不误归付费受限
    #[test]
    fn t03_explicit_quota_signals_win_over_paid_restricted() {
        assert_eq!(
            classify_error(400, "insufficient credit, upgrade your plan"),
            ErrKind::HardCredit
        );
        assert_eq!(
            classify_error(
                400,
                "{\"code\":1005,\"message\":\"plan limit exceeded, upgrade your plan\"}"
            ),
            ErrKind::PlanLimit
        );
        assert_eq!(
            classify_solo_error(1005, "plan limit exceeded, upgrade your plan"),
            ErrKind::PlanLimit
        );
    }

    /// 排序防误判②：classify_solo_error 付费受限检查在 4008 宽泛分支
    /// （contains "quota"/"exceeded"/"rate"）之前，防付费报文误归 SoftRate（60s）
    #[test]
    fn t04_solo_paid_restricted_before_generic_quota() {
        assert_eq!(
            classify_solo_error(0, "该模型仅限付费用户使用，请升级套餐"),
            ErrKind::PaidRestricted
        );
        // 对照：无付费短语的 exceeded 报文仍按既有口径归 SoftRate
        assert_eq!(classify_solo_error(4008, "quota exceeded"), ErrKind::SoftRate);
    }

    /// 冷却语义：零时长（note_error no-op 的前置约定）、标识串、不解绑粘性
    #[test]
    fn t05_paid_restricted_cooldown_semantics() {
        assert_eq!(
            ErrKind::PaidRestricted.cooldown_duration(),
            std::time::Duration::ZERO
        );
        assert_eq!(ErrKind::PaidRestricted.as_str(), "PaidRestricted");
        assert!(!ErrKind::PaidRestricted.unbinds_sticky());
    }
}
