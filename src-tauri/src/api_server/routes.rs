use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use super::custom_route;
use super::dispatch::{self, DispatchError, TargetPool};
use super::efforts;
use super::retry::{retry_plan, RetryAction};
use super::sse;
use super::unified_catalog;
use super::usage::{extract_tokens, KeyId};
use super::wb_catalog;
use super::wb_model_route;
use super::wb_route;
use super::wb_sticky::SessionKey;
use super::{classify_error, classify_solo_error, streaming_agent, ApiSharedState, ErrKind,
            InflightGuard,
            AGENT_HOST, APP_ID, EP_LLM_CHAT, IDE_VERSION, IDE_VERSION_CODE, REFERER_BASE};

// 审查修复（L3）：备案注释——Trae 池轮换上限有意收窄为 3，与 WB/Qoder 池的
// 全池轮换（while 循环不限次）口径不同系有意为之，非遗漏：Trae 上游建连成本高
// （设备指纹/机器号绑定），扩全池轮换会线性放大排队请求的首字延迟与上游压力，
// 且 Trae 池换号对空完成类故障收益有限。调整前需先评估首字延迟回归
const MAX_ROTATE: usize = 3;
/// 请求体上限（issue #21；issue #138 P1 环境变量化）：axum `Bytes` 提取器受
/// DefaultBodyLimit 约束（默认 2MiB），build_router 已显式放开，两处阈值必须一致。
/// 默认 8MiB 适配长上下文客户端（每轮重发完整历史 + 图片 base64 场景）；可用
/// 环境变量 AIWORK_MAX_BODY_BYTES 覆盖（单位字节，合法区间 [256KiB, 64MiB]，
/// 非法/越界值回退默认并 stderr 告警），解析结果进程内缓存（OnceLock 只解析一次）
pub(super) fn max_body_bytes() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        const DEFAULT: usize = 8 << 20;
        const MIN: usize = 256 << 10;
        const MAX: usize = 64 << 20;
        match std::env::var("AIWORK_MAX_BODY_BYTES") {
            Ok(raw) => match raw.trim().parse::<usize>() {
                Ok(n) if (MIN..=MAX).contains(&n) => n,
                _ => {
                    eprintln!(
                        "[aiwork] AIWORK_MAX_BODY_BYTES=\"{raw}\" 非法（合法范围 {}~{}），回退默认 {}",
                        fmt_size_human(MIN),
                        fmt_size_human(MAX),
                        fmt_size_human(DEFAULT)
                    );
                    DEFAULT
                }
            },
            Err(_) => DEFAULT,
        }
    })
}

/// 字节数人性化（issue #138 P1）：整 MiB 显示 "8MB"，否则按 KiB 显示
fn fmt_size_human(n: usize) -> String {
    if n != 0 && n % (1 << 20) == 0 {
        format!("{}MB", n >> 20)
    } else {
        format!("{}KiB", n >> 10)
    }
}

/// 请求体超限文案（issue #21）：由阈值推导，避免阈值调整后文案脱节
pub(super) fn body_too_large_msg() -> String {
    format!("request body exceeds {} limit", fmt_size_human(max_body_bytes()))
}

/// issue #138 P1：首字对冲大 body 门槛——请求体超过该值跳过对冲竞速
/// （对冲需全量快照 + 按账号整包重建，大包下内存瞬时翻倍，得不偿失）
pub(super) const HEDGE_SKIP_BODY_BYTES: usize = 256 << 10;

/// 客户端协议：决定响应/流事件的输出格式（请求侧均已统一转为 OpenAI 内部格式）
#[derive(Clone, Copy, PartialEq)]
pub enum Protocol {
    OpenAi,
    /// OpenAI legacy text completions（/v1/completions）
    OpenAiText,
    Anthropic,
    /// Codex Responses API（/v1/responses，T4.1/F-40）
    Responses,
}

impl Protocol {
    pub(super) fn log_path(self) -> &'static str {
        match self {
            Protocol::OpenAi => "/v1/chat/completions",
            Protocol::OpenAiText => "/v1/completions",
            Protocol::Anthropic => "/v1/messages",
            Protocol::Responses => "/v1/responses",
        }
    }
}

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// keep-alive 退出信号（P1 修复）：主任务（spawn_blocking）结束时 Drop 触发
/// watch 通知，ticker 收到后退出 → sender 全部关闭 → 流可正常终结。
/// Drop 兜底覆盖 panic 展开与提前 return 路径。
/// pub(super)：WB / Custom 流式路径（wb_route.rs / custom_route.rs）复用同一方案
pub(super) struct DoneSignal(pub(super) tokio::sync::watch::Sender<bool>);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// 聚合路径失败（P1 修复1）：区分「无健康账号」（维持原 503/格式）与
/// 「Fatal 上游错误透传」（携带上游状态码与错误体摘要）
enum AggregateFail {
    NoHealthy(String),
    Upstream(u16, String),
}

// ==================== Handlers ====================

/// WB 上游模型目录命中（T2.1 原始判定，保留供 /v1/models 与诊断复用）
#[allow(dead_code)]
fn wb_model_requested(state: &ApiSharedState, model: &str) -> bool {
    wb_catalog::find(&wb_catalog::load(&state.data_dir), model).is_some()
}

fn internal_error_response() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
        .unwrap()
}

/// T5.2/F-61 四段模型路由解析（别名→规则→系列通配→后缀）+ T5.6③ 后台任务降级。
/// 返回 (最终模型, 路由级 effort 注入提示)；全未命中目录 → None（走 SOLO 上游）。
fn resolve_wb_target(
    state: &ApiSharedState,
    model: &str,
    body: &Value,
) -> Option<(String, Option<String>)> {
    let cfg = wb_model_route::load_config(&state.data_dir);
    let catalog = wb_catalog::load(&state.data_dir);
    let r = wb_model_route::resolve(&cfg, &catalog, model);
    // Intl 目录动态化（2026-10-15）：动态抓取结果优先，缺失回落内置静态表
    let intl_catalog = wb_catalog::buddy_ai_load(&state.data_dir);
    // 审查修复：CN 目录未命中并查 Intl 目录（/v1/responses 同 dispatch 源判定，
    // Intl 专属 id 如 deepseek-v4.1-flash 不得 404）；Intl 专属命中跳过 CN 目录降级
    //（降级候选发往 buddy-ai 池上游不存在）
    let intl_only = wb_catalog::find(&catalog, &r.model).is_none()
        && (wb_catalog::find(&intl_catalog, &r.model).is_some()
            || wb_catalog::find(&intl_catalog, model).is_some());
    if wb_catalog::find(&catalog, &r.model).is_none()
        && wb_catalog::find(&intl_catalog, &r.model).is_none()
        && wb_catalog::find(&intl_catalog, model).is_none()
    {
        return None;
    }
    // T5.6③ 后台任务降级（显式开启才生效）：标题/摘要类短请求 → 目录最低倍率模型；
    // F-76④ 长上下文降档：输入粗估超阈值 → flash 档模型。
    // issue #26 白名单感知：降级候选 ∩ 白名单为空则跳过降级（原模型已过端点准入）
    let whitelist = unified_catalog::load_whitelist(&state.data_dir);
    let in_wl = |m: &str| unified_catalog::whitelist_allows(&whitelist, m);
    let final_model = if !intl_only
        && state.wb_bg_downgrade.load(std::sync::atomic::Ordering::Relaxed)
        && wb_model_route::is_background_task(body)
    {
        wb_model_route::cheapest_catalog_model_filtered(&catalog, &in_wl).unwrap_or(r.model)
    } else if !intl_only
        && state.wb_longctx_downgrade.load(std::sync::atomic::Ordering::Relaxed)
        && wb_model_route::estimate_input_tokens(body)
            >= wb_model_route::LONGCTX_TOKEN_THRESHOLD
    {
        wb_model_route::flash_catalog_model_filtered(&catalog, &in_wl).unwrap_or(r.model)
    } else {
        r.model
    };
    Some((final_model, r.effort_hint))
}

/// T5.3/F-62 默认深度思考：客户端未显式请求 effort 且无路由级提示时默认 high。
/// `explicit_effort` 由调用方按协议判定（OpenAI: reasoning_effort 字段；Anthropic: thinking 参数）
fn effective_effort_hint(
    state: &ApiSharedState,
    route_hint: Option<String>,
    explicit_effort: bool,
) -> Option<String> {
    if route_hint.is_some() {
        return route_hint;
    }
    if !explicit_effort
        && state
            .wb_default_thinking
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Some("high".to_string());
    }
    None
}

/// 注入 effort 提示到请求体字节流（T5.2④/T5.3）
fn apply_effort_hint(body_vec: Vec<u8>, hint: Option<String>) -> Vec<u8> {
    wb_model_route::inject_effort_hint(&body_vec, &hint)
}

/// Trae 出站 effort wire 字段名（issue #31 T0.2 待确认）：
/// 2026-09 客户端抓包仅实证档位值（light/high/extra_high，efforts::TRAE_EFFORTS_REF），
/// 字段名以 T0.2 抓包为准，确认后仅需修改本常量。注入依赖上游「未知字段忽略」
/// 惯例，且仅实证表内模型才产生 wire 值（表外不下发，不按名称猜测）
const TRAE_EFFORT_FIELD: &str = "reasoning_effort_level";

/// Trae 池 effort 解析（issue #31 T3.2/T3.3）：显式请求 > 路由级提示（后缀剥离
/// 所得）> 默认深度思考，三源合成为统一档位后转 Trae wire（efforts::trae_request_wire：
/// 实证表内精确命中或按降级链取兼容值）。
/// 实证表外模型（如档位仅 Buddy 侧声明、Trae 无实证）：显式客户端请求按统一→
/// Trae 映射填充默认下发（efforts::trae_request_wire_fill）；合成默认（路由提示/
/// 默认深度思考）不下发，保守走上游默认。
/// 显式关闭以空串编码（Anthropic thinking disabled）：短路默认思考与路由提示
fn trae_effort_wire(
    state: &ApiSharedState,
    route_hint: Option<String>,
    explicit: Option<String>,
    model: &str,
) -> Option<String> {
    let supported = efforts::trae_supported_wire(&unified_catalog::canonical_id(model));
    // 合成请求侧档位：显式 > 路由提示 > 全局默认思考（表外模型仅显式参与填充）。
    // 日志需记合成后的 requested 而非仅 explicit，否则 route_hint/默认思考生效时
    // 日志与实际出站不符
    let requested = if supported.is_empty() {
        explicit.clone()
    } else {
        explicit.clone().or(route_hint).or_else(|| {
            state
                .wb_default_thinking
                .load(std::sync::atomic::Ordering::Relaxed)
                .then(|| "high".to_string())
        })
    };
    let wire = if supported.is_empty() {
        // 表外无实证：仅显式请求填充默认（统一→Trae 映射），合成默认不下发
        efforts::trae_request_wire_fill(explicit.as_deref())
    } else {
        requested
            .as_deref()
            .and_then(|req| efforts::trae_request_wire(Some(req), &supported))
    };
    // 可观测性（issue #38-5）：请求侧档位 → wire 档位映射落盘（脱敏，仅模型与档位），
    // 用于验证「档位到底有没有下发/映射成哪一档」
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!(
            "trae effort: model={} requested={:?} wire={:?}",
            model, requested, wire
        ),
    );
    wire
}

/// Trae 出站 Max Mode 字段名（issue #31 T4.2，T0.3 客户端实证）：请求级布尔门控。
/// 注入值用布尔 true（2026-09-26 真机实证 is_max_mode:true + pmt:168000 → 200）。
/// 注意：issue #38 报告的 4001 真机根因是 body model 字段未随 `-max` 剥离回写
/// （见 inject_trae_outbound），字段值类型并非根因。仅入口标志置位且模型在
/// 支持表内才注入（表外不冒进）
const TRAE_MAX_MODE_FIELD: &str = "is_max_mode";

/// Trae 出站注入（issue #31 T3.3/T4.2）：effort wire 与 Max Mode 两路合并为单次
/// parse/serialize（Max Mode 场景恰为大 body，避免重复往返）。同时回写 body 的
/// model 字段为剥离后基名（issue #38 真机根因：dispatch 剥离 `-max`/`-thinking`
/// 仅用于路由与日志，body 原样透传致 payload 生成 `xxx-max__dev` → 上游 4001
/// param invalid）。三者均未激活时零 parse 原样返回；非对象/非 JSON body 原样返回。
/// payload 层（prepare_llm_chat_body）为透传+增补模式，注入字段直达上游。
/// 注入结果落 app.log（issue #38-5：is_max_mode 是否注入此前无任何痕迹，
/// 无法区分「注入被拒」与「后缀未剥离」）
fn inject_trae_outbound(
    data_dir: &std::path::Path,
    body_vec: Vec<u8>,
    wire: Option<String>,
    max_mode_hint: bool,
    requested_model: &str,
    model: &str,
) -> Vec<u8> {
    let inject_max =
        max_mode_hint && efforts::trae_max_mode_supported(&unified_catalog::canonical_id(model));
    let rewrite_model = requested_model != model;
    if wire.is_none() && !inject_max && !rewrite_model {
        return body_vec;
    }
    match serde_json::from_slice::<Value>(&body_vec) {
        Ok(Value::Object(mut obj)) => {
            if rewrite_model {
                obj.insert("model".into(), json!(model));
            }
            if let Some(w) = &wire {
                obj.insert(TRAE_EFFORT_FIELD.to_string(), json!(w));
            }
            if inject_max {
                obj.insert(TRAE_MAX_MODE_FIELD.to_string(), json!(true));
            }
            crate::fs_utils::app_log(
                data_dir,
                &format!(
                    "trae outbound: model={} requested_model={} effort_injected={} max_mode_injected={}",
                    model,
                    requested_model,
                    wire.is_some(),
                    inject_max,
                ),
            );
            serde_json::to_vec(&obj).unwrap_or(body_vec)
        }
        _ => body_vec,
    }
}

/// 模型级冷却快速失败（T2.7/F-34：优先级高于 Key 级）。
/// 审查 #21：剩余秒数按调用方档位视图取——此前固定走 CN wb 档便捷包装，
/// buddy-ai 模型冷却时（独立冷却表有值）客户端拿到 "retry after 0s"
fn model_cooling_response(view: &wb_route::WbView, model: &str, proto: Protocol) -> Response {
    let rem = wb_route::model_cooling_remaining(view, model).unwrap_or(0);
    let msg = format!("model {} cooling down, retry after {}s", model, rem);
    match proto {
        Protocol::Anthropic => anthropic_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", &msg),
        _ => openai_error(StatusCode::TOO_MANY_REQUESTS, "model_cooldown", &msg),
    }
}

/// 调度错误矩阵 → 按客户端协议格式化响应（§4.3/§4.5；统一调度分流点专用）。
/// issue #29 修复1：调度阶段失败同样落 API 请求日志——此前失败仅 app.log 有
/// dispatch exhausted，前端「API 请求日志」页无记录，用户无法自查失败原因。
/// pub(crate)：dispatch 测试模块经 Fixture 直测日志落盘（与 no_healthy_detail 同模式）
pub(crate) fn dispatch_error_response(
    state: &ApiSharedState,
    err: DispatchError,
    proto: Protocol,
    model: &str,
    stream: bool,
    key_str: &str,
    duration_ms: u64,
) -> Response {
    // 单次 match：错误 → (状态码, 池标识, 消息, OpenAI 侧错误码, Anthropic 侧错误码)
    let (status, pool_str, msg, oa_code, anthro_code): (StatusCode, &str, String, &'static str, &'static str) =
        match err {
            DispatchError::WbDisabled => (
                StatusCode::BAD_REQUEST,
                "buddy",
                "该模型属 WorkBuddy 上游，但 WB 上游未启用（api_pool.json wb_enabled）".to_string(),
                "wb_upstream_disabled",
                "invalid_request_error",
            ),
            DispatchError::QoderDisabled => (
                StatusCode::BAD_REQUEST,
                "qoder",
                "该模型属 Qoder 上游，但 Qoder 上游未启用（api_pool.json qoder_enabled）".to_string(),
                "qoder_upstream_disabled",
                "invalid_request_error",
            ),
            DispatchError::TraeDisabled => (
                StatusCode::BAD_REQUEST,
                "trae",
                "该模型属 Trae 上游，但 Trae 资源池已停用（api_pool.json trae_enabled）".to_string(),
                "trae_pool_disabled",
                "invalid_request_error",
            ),
            // 审查修复（模型冷却归因）：池标识改取错误携带的真实目标池，不再
            // 硬编码 "buddy"——buddy-ai/qoder 的模型级 429 在 API 请求日志与
            // 用量看板中按发起池准确归因
            // issue #158 P0-1：ModelCooling 到达客户端即意味着无跨池回退
            // （dispatch 层 can_fallback=false：单源/fallback 关闭/无后续可用源），
            // 响应体附加排障指引——日志评论区高频疑问「为啥 429 不自动换池」
            DispatchError::ModelCooling(rem, pool) => (
                StatusCode::TOO_MANY_REQUESTS,
                pool.as_str(),
                format!(
                    "model {model} cooling down, retry after {rem}s（当前无跨池回退可用：该模型仅此上游或无后续可用源；如需自动切池，可在 API 服务页「资源总览」的 per_model 模型绑定池中为该模型配置备用上游池）"
                ),
                "model_cooldown",
                "rate_limit_error",
            ),
            DispatchError::NoHealthy(pool) => (
                StatusCode::SERVICE_UNAVAILABLE,
                pool.as_str(),
                no_healthy_detail(state, pool, key_str),
                "no_healthy_account",
                "api_error",
            ),
        };
    // issue #29 修复1：uid/acct 置空（调度阶段未取号），error 携带失败原因
    let key_name = super::api_keys::key_name_for(&state.data_dir, key_str);
    state.logger.log_request(
        pool_str, "POST", proto.log_path(), model, stream, status.as_u16(), "-",
        duration_ms, &key_name, "", Some(&msg),
    );
    match proto {
        Protocol::Anthropic => anthropic_error(status, anthro_code, &msg),
        _ => openai_error(status, oa_code, &msg),
    }
}

/// 轮换耗尽收尾文案（issue #57 后续可观测）：区分「渠道指纹拦截」与「账号池耗尽」。
/// 空完成（影子风控，HTTP 200 零内容）与账号无关，换号轮换打满整池后原文案
/// "no healthy account available" 会误导用户排查账号——现按 empty_hits 分流：
/// 空完成主导（过半轮换账号返回空完成，且最小样本量 ≥2）→ 明示指纹拦截语义
/// （账号/模型无关，提示更新清洗规则表）；混合失败（空完成仅零星出现、以
/// 限流/5xx 为主）或样本不足（issue #130：单号轮换 1 次空完成即宣判易误报，
/// 上游瞬时抖动同样返回空完成）时维持原文案，避免 "switching won't help"
/// 误导排障方向。返回 (错误消息, 诊断日志标记)
/// issue #156：`hard` = 池内处于积分耗尽硬冷却（次日 04:00 恢复）的账号数，
/// >0 时在消息中携带冷却原因与恢复时间，使客户端可见「积分耗尽 04:00 重置」
/// （此前该语义只在管理端徽标与日志，对 API 客户端隐形）
pub(crate) fn exhaust_message(
    empty_hits: usize,
    rotated: usize,
    healthy_total: usize,
    hard: usize,
) -> (String, &'static str) {
    exhaust_message_with_transport_cut(empty_hits, rotated, healthy_total, hard, 0)
}

/// 在通用耗尽文案上补充传输掐断归因（issue #158 同族可见化）：
/// 全部轮换账号都在响应中途被掐时，不再误报 "no healthy account available"；
/// 混合失败则保留原归因并追加 cut 计数，避免把网络问题当成账号健康问题。
pub(crate) fn exhaust_message_with_transport_cut(
    empty_hits: usize,
    rotated: usize,
    healthy_total: usize,
    hard: usize,
    cut_hits: usize,
) -> (String, &'static str) {
    if cut_hits > 0 && rotated > 0 && cut_hits >= rotated {
        return (
            format!(
                "transport cut suspected: {cut_hits} transport cut(s) across {rotated} rotated accounts (upstream connection closed mid-response, account/model agnostic — retry later or check local network/proxy; pool=trae healthy={healthy_total})"
            ),
            "TRANSPORT_CUT",
        );
    }
    let (mut msg, tag) = exhaust_message_without_cut(empty_hits, rotated, healthy_total, hard);
    if cut_hits > 0 && rotated > 0 {
        msg.push_str(&format!(
            "; {cut_hits} transport cut(s) across {rotated} rotated accounts (upstream connection closed mid-response, account/model agnostic — retry later or check local network/proxy)"
        ));
    }
    (msg, tag)
}

fn exhaust_message_without_cut(
    empty_hits: usize,
    rotated: usize,
    healthy_total: usize,
    hard: usize,
) -> (String, &'static str) {
    if rotated >= 2 && empty_hits * 2 > rotated {
        (
            format!(
                "channel fingerprint block suspected: {empty_hits} empty completion(s) across {rotated} rotated accounts (Trae shadow risk control, account/model agnostic — switching won't help); try updating wb_template_map.json (UI「指纹清洗」tab) or the tool alias table (UI「工具别名」tab) or enabling sanitize (pool=trae healthy={healthy_total}); client agent-tool fingerprint suspected too (issue #150) — disabling agent tools / plain chat on the client bypasses it",
            ),
            "CHANNEL_FINGERPRINT_BLOCK",
        )
    } else if hard > 0 {
        // issue #156：积分耗尽硬冷却账号存在时，携带冷却原因与次日 04:00 恢复
        // 语义。hard 冷却账号不参与轮换（healthy() 直接剔除），rotated 计的是
        // 本次实际试过且因其他原因（401/429/5xx/超时）失败的账号——rotated>0
        // 时补充诚实语境，避免把其他失败也归因于积分耗尽
        let rotated_note = if rotated > 0 {
            format!("; {rotated} further account(s) also failed this rotation")
        } else {
            String::new()
        };
        (
            format!(
                "no healthy account available: credit exhausted on {hard} account(s), auto-retry after next 04:00 (Asia/Shanghai daily reset){rotated_note}; top up credits or switch accounts to recover sooner"
            ),
            "NO_HEALTHY_ACCOUNT_CREDIT",
        )
    } else {
        (
            "no healthy account available".to_string(),
            "NO_HEALTHY_ACCOUNT",
        )
    }
}

/// NoHealthy 详情（issue #29 修复2）：错误消息携带 Key 约束细节（绑定池/白名单
/// 条数/专一账号）与池内健康计数，帮助用户自查 Key 配置（匿名/未知 Key 仅报
/// 池健康数）；healthy_in_scope 为约束作用域内健康账号数，0 即「约束排除致败」。
/// issue #136 hint 按根因分流：allowed=Some(空集)（约束条目全指向其他池，本池被
/// 整体排除，调度优先级对该 Key 形同虚设）与 allowed=Some(非空) 但作用域内账号
/// 均不可用，两种根因用户动作不同，混用同一文案会误导排障方向。
/// pub(crate)：dispatch 测试模块经 Fixture 直测（与 trae_pool_constraints 同模式）
pub(crate) fn no_healthy_detail(state: &ApiSharedState, pool: TargetPool, key_str: &str) -> String {
    let pool_ref = match pool {
        TargetPool::Trae => &state.pool,
        TargetPool::Buddy => &state.wb_pool,
        TargetPool::BuddyAi => &state.buddy_ai_pool,
        TargetPool::Qoder => &state.qoder_pool,
        // custom 池无账号列表，无 healthier 账号枚举——直接按错误矩阵基准文案返回
        // （v3：组内全熔断 → NoHealthy(Custom) 走此处；前缀锁池即 503 单池语义）
        TargetPool::Custom => return "no healthy account available".to_string(),
    };
    let (healthy_total, _) = pool_ref.selectable_stats_in(None, false);
    // issue #156：积分耗尽（硬冷却到次日 04:00）账号数 >0 时在诊断消息中携带
    // 冷却原因与恢复时间——此前该语义只体现在管理端徽标，对 API 客户端隐形
    let (hard_credit_cnt, hard_until) = pool_ref.hard_credit_stats();
    let hard_note = if hard_credit_cnt > 0 {
        match hard_until {
            Some(ts) => {
                let remain = ts.saturating_sub(now_ts() as i64);
                format!(" credit_exhausted={hard_credit_cnt} (auto-retry in ~{remain}s at next 04:00 Asia/Shanghai)")
            }
            None => format!(" credit_exhausted={hard_credit_cnt}"),
        }
    } else {
        String::new()
    };
    let base = format!(
        "no healthy account available (pool={} healthy={healthy_total}){hard_note}",
        pool.as_str(),
    );
    // 单锁快照：一次取（展示名, 约束），替代 constraints_for + key_name_for 双加锁
    let Some((name, rk)) = super::api_keys::key_snapshot_for(&state.data_dir, key_str) else {
        return base;
    };
    let name = if name.is_empty() { key_str } else { name.as_str() };
    let bind = rk.bind_pool().unwrap_or("-");
    match rk.pool_constraints(pool.as_str()) {
        None => format!("{base} key={name} bind={bind} constraint=none"),
        Some(c) => {
            let (_, healthy_scope) = pool_ref.selectable_stats_in(c.allowed.as_ref(), false);
            let wl = match &c.allowed {
                None => "none".to_string(),
                Some(s) => s.len().to_string(),
            };
            let ded = c.dedicated.as_deref().unwrap_or("-");
            // issue #136：hint 按根因分流——
            // A) allowed=Some(空集)：约束条目全指向其他池（或专一锁他池），本池被整体
            //    排除；列出约束实际涉及的池，提示「模型由本池提供但白名单未覆盖」
            // B) allowed=Some(非空) 且作用域内 0 健康：白名单指向的本池账号均不可用
            let hint = if healthy_scope == 0 && healthy_total > 0 {
                if matches!(&c.allowed, Some(s) if s.is_empty()) {
                    if rk.org_constrained {
                        // 组/标签约束（api_keys::apply_org_expansion 展开条目必带池前缀
                        // 如 trae:xxx）：交集为空即本池被整池排除。此时按原始白名单
                        // 条目回拼池名会得出「约束均指向其他池（trae）」，与本池即
                        // trae 的排除事实同句矛盾——直接给组/标签口径提示
                        format!(
                            " hint=该 Key 的组/标签约束与本池账号交集为空（交集为空即整池排除）：请编辑该 Key 的组/标签或账号白名单后重试"
                        )
                    } else {
                        // 约束条目涉及的池集合（裸 uid 防御性归属 buddy，与 pool_constraints 同口径）
                        let mut pools: Vec<&'static str> = Vec::new();
                        for u in &rk.allowed_accounts {
                            let (p, _) = super::api_keys::split_pool_tagged(u);
                            let p = p.unwrap_or("buddy");
                            if !pools.contains(&p) {
                                pools.push(p);
                            }
                        }
                        if rk.schedule_mode == super::api_keys::MODE_DEDICATED {
                            let (p, _) = super::api_keys::split_pool_tagged(&rk.dedicated_account);
                            if let Some(p) = p {
                                if !pools.contains(&p) {
                                    pools.push(p);
                                }
                            }
                        }
                        if pools.is_empty() {
                            format!(
                                " hint=该 Key 的账号约束未包含 {pool} 池账号，而本次请求由 {pool} 池提供：请编辑该 Key 的账号白名单/专一账号后重试",
                                pool = pool.as_str(),
                            )
                        } else {
                            format!(
                                " hint=该 Key 的账号约束均指向其他资源池（{pools}），未包含 {pool} 池账号，而本次请求由 {pool} 池提供：请编辑该 Key 的账号白名单/专一账号纳入 {pool} 池，或检查约束目标池的账号健康度后重试",
                                pools = pools.join("/"),
                                pool = pool.as_str(),
                            )
                        }
                    }
                } else {
                    format!(
                        " hint=约束排除了池内全部健康账号（池内尚有 {healthy_total} 个）：白名单/专一指向的本池账号均不可用，请检查这些账号状态或编辑该 Key 的账号白名单/专一账号后重试"
                    )
                }
            } else {
                String::new()
            };
            format!(
                "{base} key={name} bind={bind} whitelist={wl} dedicated={ded} healthy_in_scope={healthy_scope}{hint}"
            )
        }
    }
}

/// issue #26 全局模型白名单准入：请求模型不在白名单时返回按协议格式化的 404。
/// 拒绝码与官方语义一致：OpenAI 侧 `model_not_found`、Anthropic 侧 `not_found_error`。
/// 白名单为空 = 不限（默认行为零变化）
fn whitelist_check(state: &ApiSharedState, model: &str, proto: Protocol) -> Option<Response> {
    let list = unified_catalog::load_whitelist(&state.data_dir);
    if unified_catalog::whitelist_allows(&list, model) {
        return None;
    }
    // issue #90 修复③a：池前缀模型 id（trae/xxx 等）按裸名准入——dispatch 与
    // /v1/models 别名条目使用同一前缀语义，启用白名单的部署不得在准入层拦截。
    // 分层检查：先原始名（覆盖白名单字面量含前缀条目的配置），再裸名兜底；
    // 拒绝信息保留用户请求的原始名（显示裸名会让人误以为请求的是另一个模型）
    let bare = super::dispatch::split_pool_prefix(model).map(|(_, bare)| bare);
    if let Some(bare) = bare {
        if unified_catalog::whitelist_allows(&list, bare) {
            return None;
        }
    }
    // C1（审查修复）：后缀剥离仅按裸名段进行；拒绝信息保留用户请求的原始名
    // （含前缀）——修复此前 `let model = bare.unwrap_or(model)` 遮蔽导致 404
    // 文案显示裸名、与上方注释「保留原始名」矛盾的问题（放行/剥离逻辑不变）
    let stripped = bare.unwrap_or(model);
    // issue #38 实测修复：`-max` / `-thinking` 等入口后缀在 dispatch ② 段才剥离，
    // 白名单须按同一剥离规则对基名放行——否则启用白名单的部署下，Max Mode /
    // 路由后缀入口在准入层就被 404 拦截，整个能力不可用（剥离规则与 dispatch 同源，
    // 基名最终可服务性仍由 dispatch 查模型列表判定，此处仅准入放行）
    let cfg = wb_model_route::load_config(&state.data_dir);
    let bases = [
        wb_model_route::strip_max_suffix(stripped),
        wb_model_route::strip_route_suffix(stripped, &cfg).map(|(b, _)| b),
    ];
    for base in bases.into_iter().flatten() {
        if unified_catalog::whitelist_allows(&list, &base) {
            return None;
        }
    }
    Some(whitelist_error_response(model, proto))
}

/// 白名单拒绝响应壳（独立纯函数便于单测错误格式）
fn whitelist_error_response(model: &str, proto: Protocol) -> Response {
    let msg = format!(
        "model {} is not in the model whitelist; see GET /v1/models for allowed models",
        model
    );
    match proto {
        Protocol::Anthropic => anthropic_error(StatusCode::NOT_FOUND, "not_found_error", &msg),
        _ => openai_error(StatusCode::NOT_FOUND, "model_not_found", &msg),
    }
}

pub async fn health(State(state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    // 收敏（2026-10-12 审查 P2）：/health 在 auth.rs 免鉴权放行（探活豁免），
    // 原实现透出 total_credits/active_uid/last_error/池账号数——非环回监听时
    // 局域网内无 Key 即可抓取账号与积分画像，与「非环回强制鉴权」防线矛盾。
    // 探活仅透出低敏指标（issue #138 P2）：inflight 在途计数 + rss_kib（仅
    // Linux，供内存压力观察）；完整诊断由需鉴权的 GET /status 承载（字段超集）
    #[cfg(target_os = "linux")]
    let rss_kib = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| {
        s.lines().find_map(|line| {
            line.strip_prefix("VmRSS:").and_then(|rest| {
                rest.trim().split_whitespace().next().and_then(|v| v.parse::<u64>().ok())
            })
        })
    });
    #[cfg(target_os = "linux")]
    let body = json!({
        "status": "ok",
        "running": true,
        "inflight": state.inflight.load(std::sync::atomic::Ordering::Relaxed),
        "rss_kib": rss_kib,
    });
    #[cfg(not(target_os = "linux"))]
    let body = json!({
        "status": "ok",
        "running": true,
        "inflight": state.inflight.load(std::sync::atomic::Ordering::Relaxed),
    });
    Json(body)
}

/// /healthz（T2.3/F-32）：三池均无可用账号 → 503，供探活/看门狗。
/// H1 修复（审查）：旧口径仅 Trae ∨ WB 且不查 hard_credit/积分过期/零积分——
/// 仅部署 Qoder 账号或全部账号 hard_credit 冷却时探活恒 503（可能触发误重启）。
/// 改用 has_selectable()（= pool::selectable 口径：健康 + 未禁用 + 未冷却 +
/// 积分未过期且非零），三池 × enabled 开关，与 /v1/models 徽章判定同源
pub async fn healthz(State(state): State<Arc<ApiSharedState>>) -> Response {
    let trae_enabled = state.trae_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let solo_ok = trae_enabled && state.pool.has_selectable();
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let wb_ok = wb_enabled && state.wb_pool.has_selectable();
    let qoder_enabled = state.qoder_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let qoder_ok = qoder_enabled && state.qoder_pool.has_selectable();
    // buddy-ai 独立池（§3.4 隔离清单）：healthz 四池化——仅部署 Intl 账号时探活同样正常
    let buddy_ai_enabled = state.buddy_ai_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let buddy_ai_ok = buddy_ai_enabled && state.buddy_ai_pool.has_selectable();
    if solo_ok || wb_ok || qoder_ok || buddy_ai_ok {
        (
            StatusCode::OK,
            axum::Json(json!({
                "status": "ok",
                "solo_available": solo_ok,
                "wb_available": wb_ok,
                "qoder_available": qoder_ok,
                "buddy_ai_available": buddy_ai_ok,
            })),
        )
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "status": "unavailable", "reason": "no healthy account" })),
        )
            .into_response()
    }
}

pub async fn status(State(state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    let pool = state.pool.status_list();
    let now: i64 = now_ts() as i64;
    let total = state.total_requests.load(std::sync::atomic::Ordering::Relaxed);
    let active = safe_lock(&state.active_uid).clone();
    let last_err = safe_lock(&state.last_error).clone();

    // 汇总统计
    let total_accounts = pool.len();
    let available = pool.iter().filter(|p| !p.disabled && !p.cooling).count();
    let cooling = pool.iter().filter(|p| p.cooling).count();
    let disabled = pool.iter().filter(|p| p.disabled).count();
    let total_credits: f64 = pool.iter().filter_map(|p| p.credits).sum();
    let total_credits = (total_credits * 100.0).round() / 100.0;

    // 账号明细（inflight 为 F-77 账号级实时并发）
    let accounts: Vec<Value> = pool.iter().map(|p| {
        let status = if p.disabled {
            "disabled"
        } else if p.cooling {
            "cooling"
        } else if p.credits_expire_at.map_or(false, |exp| exp < now) {
            "expired"
        } else if p.credits.map_or(false, |c| c <= 0.0) {
            "no_credits"
        } else {
            "available"
        };
        json!({
            "uid": p.uid,
            "name": p.name,
            "status": status,
            "credits": p.credits,
            "credits_expire_at": p.credits_expire_at,
            "cooling": p.cooling,
            "cooldown_until": p.cooldown_until,
            "cooldown_reason": p.cooldown_reason,
            "disabled": p.disabled,
            "err_count": p.err_count,
            "state": p.state,
            "inflight": p.inflight,
        })
    }).collect();

    // WB 池画像（T2.3/F-32；inflight 为 F-77 账号级实时并发）
    let wb_pool = state.wb_pool.status_list();
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let wb_accounts: Vec<Value> = wb_pool.iter().map(|p| {
        json!({
            "uid": p.uid, "name": p.name, "credits": p.credits,
            "cooling": p.cooling, "cooldown_until": p.cooldown_until,
            "cooldown_reason": p.cooldown_reason, "disabled": p.disabled,
            "err_count": p.err_count, "state": p.state, "inflight": p.inflight,
        })
    }).collect();
    // F-77：per-account 并发列表（活跃账号 [{uid, name, inflight}]）——
    // 见下方 buddy-ai 画像块（三池合并后统一构造）
    let model_cooldowns: Vec<Value> = {
        let map = safe_lock(&state.model_cooldowns);
        map.iter()
            .filter(|(_, (until, _))| *until > now)
            .map(|(m, (until, fails))| json!({
                "model": m, "until": until, "remaining_s": until - now, "fails": fails,
            }))
            .collect()
    };

    // buddy-ai 独立池画像（阶段 2，§3.1 失败域隔离：与 wb 画面完全独立）
    let buddy_ai_pool_status = state.buddy_ai_pool.status_list();
    let buddy_ai_enabled = state
        .buddy_ai_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    let buddy_ai_accounts: Vec<Value> = buddy_ai_pool_status.iter().map(|p| {
        json!({
            "uid": p.uid, "name": p.name, "credits": p.credits,
            "cooling": p.cooling, "cooldown_until": p.cooldown_until,
            "cooldown_reason": p.cooldown_reason, "disabled": p.disabled,
            "err_count": p.err_count, "state": p.state, "inflight": p.inflight,
        })
    }).collect();
    let buddy_ai_model_cooldowns: Vec<Value> = {
        let map = safe_lock(&state.buddy_ai_model_cooldowns);
        map.iter()
            .filter(|(_, (until, _))| *until > now)
            .map(|(m, (until, fails))| json!({
                "model": m, "until": until, "remaining_s": until - now, "fails": fails,
            }))
            .collect()
    };
    // buddy-ai 活跃账号并入 busy 画像（同一网关出口，并发可视性对齐三池）
    let active_accounts: Vec<Value> = pool
        .iter()
        .chain(wb_pool.iter())
        .chain(buddy_ai_pool_status.iter())
        .filter(|p| p.inflight > 0)
        .map(|p| json!({ "uid": p.uid, "name": p.name, "inflight": p.inflight }))
        .collect();

    Json(json!({
        "running": true,
        "total_requests": total,
        // 当前并发数（统一网关 §4.5）：InflightGuard RAII 维护，覆盖 6 业务端点
        "inflight": state.inflight.load(std::sync::atomic::Ordering::Relaxed),
        "active_uid": active,
        // F-77：per-account 并发列表（busy 账号实时画像）
        "active_accounts": active_accounts,
        "last_error": last_err,
        "summary": {
            "total_accounts": total_accounts,
            "available": available,
            "cooling": cooling,
            "disabled": disabled,
            "total_credits": total_credits,
        },
        "accounts": accounts,
        "wb": {
            "enabled": wb_enabled,
            "total_accounts": wb_pool.len(),
            "accounts": wb_accounts,
            "model_cooldowns": model_cooldowns,
            "sticky_sessions": state.wb_sticky.len(),
            // 上游健康探针（F-34 ④/§2.2）：-1 未探测 / 0 不可达 / 1 在线
            "probe_ok": state.wb_probe_ok.load(std::sync::atomic::Ordering::Relaxed),
            "probe_ts_ms": state.wb_probe_ts_ms.load(std::sync::atomic::Ordering::Relaxed),
        },
        "buddy_ai": {
            "enabled": buddy_ai_enabled,
            "total_accounts": buddy_ai_pool_status.len(),
            "accounts": buddy_ai_accounts,
            "model_cooldowns": buddy_ai_model_cooldowns,
            "sticky_sessions": state.buddy_ai_sticky.len(),
            // 上游健康探针（G8 审查修复）：与 wb 块同口径对齐，前端/监控可按
            // 池区分上游可达性；-1 未探测 / 0 不可达 / 1 在线
            "probe_ok": state.buddy_ai_probe_ok.load(std::sync::atomic::Ordering::Relaxed),
            "probe_ts_ms": state.buddy_ai_probe_ts_ms.load(std::sync::atomic::Ordering::Relaxed),
        },
    }))
}

/// 对外 /v1/models 条目可见性（issue #145）：
/// ① WB 准入口径——wb_enabled=false 时仅 WB 系源（buddy / buddy-ai）的条目不发布，
///    多源模型保留（仍可由 Trae 源服务 §3.4；buddy-ai 开启时 CN 目录模型经 buddy-ai
///    池仍可路由，不得在准入目录层被滤掉——与 dispatch available 门控同口径）。
///    判定将 s.disabled 视同源不存在，防「WB 关 + 仅剩 Trae 源被 L1 禁用」组合
///    产生幽灵条目（目录列出但任何请求都 400 wb_upstream_disabled）；
/// ② issue #145：全部源被 L1 显式禁用的条目不对外发布（管理端 api_unified_models
///    不受此限，仍可见以便重新启用）
fn models_entry_visible(
    m: &unified_catalog::UnifiedModel,
    wb_enabled: bool,
    buddy_ai_enabled: bool,
) -> bool {
    (wb_enabled
        || buddy_ai_enabled
        || !m.sources
            .iter()
            .all(|s| s.pool == "buddy" || s.pool == "buddy-ai" || s.disabled))
        && !m.sources.iter().all(|s| s.disabled)
}

pub async fn models(State(state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    // 统一模型目录（§3.4）：实时聚合 data/api_models.json（Trae，元数据四层链）
    // 与 data/wb_model_catalog.json（Buddy），纯派生不落盘。官网/目录同步后
    // 无需重启 API 服务即可通过 /v1/models 看到最新列表
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    // 可用性标记运行时派生（§3.3 #5）：HTTP 端点用实时池健康；
    // Trae 开关（trae_enabled，默认开）参与徽章判定——关闭时 Trae 源徽章置灰
    let trae_enabled = state.trae_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let trae_ok = trae_enabled && state.pool.has_selectable();
    let buddy_ok = state.wb_pool.has_selectable();
    // Qoder 源旗标（p3-3）：条目始终入表（开关关闭时徽章置灰）
    let qoder_enabled = state.qoder_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let qoder_ok = state.qoder_pool.has_selectable();
    // buddy-ai 源旗标（阶段 2 审查修复）：Intl 静态目录入统一目录（Intl 专属 id
    // 可被发现/加入白名单），徽章 = 开关 && 池健康
    let buddy_ai_enabled = state
        .buddy_ai_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    let buddy_ai_ok = state.buddy_ai_pool.has_selectable();
    // Qwen3.8-Flash 免费活动覆盖（issue #111）：目录展示与取号/排序同口径
    let qoder_flash_free = state
        .qoder_flash_free_rate
        .load(std::sync::atomic::Ordering::Relaxed);
    let data_dir = state.data_dir.clone();
    // issue #90 修复③a：来源标识开关（dispatch_policy.model_source_labels）——
    // 开启后为多源同名模型附带池前缀别名条目（trae/xxx / buddy/xxx / buddy-ai/xxx
    // / qoder/xxx），客户端（如 CC Switch）可在同名模型间区分来源池（各池倍率与效果不同）
    let source_labels = dispatch::load_policy(&data_dir).model_source_labels;
    let list = tokio::task::spawn_blocking(move || {
        // issue #26：对外目录经全局白名单过滤（管理端 api_unified_models 不过滤）
        unified_catalog::unified_models_whitelisted_ex(
            &data_dir,
            wb_enabled,
            trae_ok,
            buddy_ok,
            Some((qoder_enabled, qoder_ok)),
            Some((buddy_ai_enabled, buddy_ai_ok)),
            qoder_flash_free,
        )
    })
    .await
    .unwrap_or_default();
    // wb_enabled=false：仅 WB 系源（buddy / buddy-ai）的模型过滤，多源模型保留
    //（仍可由 Trae 源服务 §3.4；buddy-ai 开启时 CN 目录模型经 buddy-ai 池仍可路由，
    // 不得在准入目录层被滤掉——与 dispatch available 门控同口径）
    // issue #145：全部源被 L1 显式禁用的条目不对外发布（管理端 api_unified_models
    // 不受此限，仍可见以便重新启用）
    let entry_visible =
        |m: &unified_catalog::UnifiedModel| models_entry_visible(m, wb_enabled, buddy_ai_enabled);
    let mut data: Vec<Value> = list
        .iter()
        .filter(|m| entry_visible(m))
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "created": 1753600000,
                "owned_by": "unified",
                "display": m.display,
                "rate": m.rate,
                "context_length": m.context_length,
                "max_tokens": m.max_tokens,
                "supports_image": m.supports_image,
                "supported_efforts": m.efforts,
                // Max Mode 支持（issue #38-1：UnifiedModel 已按支持表计算，
                // 此前手工序列化遗漏导致 release notes/手册声明的字段从未出现在响应中）
                "max_mode": m.max_mode,
                // 来源池集合：[{pool: "trae"|"buddy", rate, enabled}]（徽章/降级判定
                // 由客户端按元数据自决，勿硬编码 §3.4）
                "sources": m.sources,
                "manual": m.manual,
            })
        })
        .collect();
    // issue #90 修复③a：多源同名模型的池前缀别名条目（开关开启时追加，不改写
    // 原有 id——已有客户端零影响）。带前缀 id 请求经 resolve_target 锁定该池，
    // 白名单按裸名放行（whitelist_check 同步剥离）；别名条目随基础条目白名单
    // 过滤结果派生，不重复过滤
    if source_labels {
        // 别名随基础条目可见性派生（issue #145：全禁条目不生成别名）；
        // 被禁单源不发布前缀别名（trae/xxx 指向不可路由源无意义）
        for m in list.iter().filter(|m| entry_visible(m) && m.sources.len() >= 2) {
            // v3：custom 入别名列表（custom/xxx 前缀锁池直达与其他池同语义）
            for s in m.sources.iter().filter(|s| !s.disabled) {
                data.push(json!({
                    "id": format!("{}/{}", s.pool, m.id),
                    "object": "model",
                    "created": 1753600000,
                    "owned_by": "unified",
                    "display": format!("[{}] {}", s.pool, m.display),
                    "rate": s.rate,
                    "context_length": m.context_length,
                    "max_tokens": m.max_tokens,
                    "supports_image": m.supports_image,
                    "supported_efforts": m.efforts,
                    "max_mode": m.max_mode,
                    "sources": m.sources,
                    // 别名标记：客户端可据此折叠展示；source_pool = 该 id 实际锁定的池
                    "alias_of": m.id,
                    "source_pool": s.pool,
                    "manual": m.manual,
                }));
            }
        }
    }
    Json(json!({ "object": "list", "data": data }))
}

pub async fn chat_completions(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > max_body_bytes() {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            &body_too_large_msg(),
        );
    }

    // T5.6② 单端口协议区分：anthropic-version 头出现 → 客户端实为 Anthropic
    // Messages 协议，按路径分流给出明确指引（避免三协议混投后字段级静默错乱）
    if headers.contains_key("anthropic-version") {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "wrong_endpoint",
            "检测到 anthropic-version 头：该请求应为 Anthropic Messages 协议，请改用 POST /v1/messages（本网关单端口三协议按路径区分）",
        );
    }

    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let body_vec = body.to_vec();
    // 校验 JSON：无效请求体直接 400，不转发上游（与 /v1/messages 行为对齐）
    let peek: Value = match serde_json::from_slice(&body_vec) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();
    // issue #26 白名单准入（空名单不限）
    if let Some(resp) = whitelist_check(&state, &model, Protocol::OpenAi) {
        return resp;
    }
    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());

    // 统一调度分流点（§4.1 ③~⑥）：resolve_target 决定资源池/会话池粘性/跨池回退/
    // 错误矩阵，替代原 resolve_wb_target 单向判定；默认策略下行为与改造前一致（§9.1）。
    // inflight guard 随执行路径持有至请求结束（流式含整个后台任务）
    let guard = state.inflight_guard();
    match dispatch::resolve_target(&state, &model, &peek, Some(&key_str)) {
        Err(e) => dispatch_error_response(
            &state, e, Protocol::OpenAi, &model, stream, &key_str,
            start_ts.elapsed().as_millis() as u64,
        ),
        Ok(r) => match r.pool {
            // buddy-ai（阶段 2）：与 Buddy 档共用执行内核，视图按池分流
            // 审查修复备案：/v1/chat 与 /v1/responses 的 Intl 分流存在已知不对称——
            // 本路径 Intl 专属模型（buddy_intl_only）经 dispatch available 门控
            // （Buddy(CN) 池不服务该模型）走 buddy-ai 国际池，而 /v1/responses 在
            // wb 开启时会把同模型投 CN 上游；与蓝本同位一致，属有意设计——
            // 若要统一需改 resolve_wb_target 签名波及多处调用，暂不处理。
            TargetPool::Buddy | TargetPool::BuddyAi => {
                // T5.3 默认深度思考：客户端未带 reasoning_effort 时注入 high
                let explicit = peek.get("reasoning_effort").and_then(|v| v.as_str()).is_some();
                let hint = effective_effort_hint(&state, r.effort_hint, explicit);
                let body_vec = apply_effort_hint(body_vec, hint);
                let buddy_ai = r.pool == TargetPool::BuddyAi;
                if stream {
                    return wb_route::wb_stream_chat(state_clone, buddy_ai, body_vec, r.model, start_ts, Protocol::OpenAi, key_str, guard, std::collections::HashSet::new());
                }
                return wb_route::wb_aggregate_chat(state_clone, buddy_ai, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, guard, std::collections::HashSet::new()).await;
            }
            TargetPool::Qoder => {
                // Qoder 上游（p3-3）：请求体由执行路径按目录条目构造（agent 信封），
                // 无 effort/后缀预处理（思考档位由 resolve_thinking 按条目解析）
                if stream {
                    return super::qoder_route::qoder_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::OpenAi, key_str, guard);
                }
                return super::qoder_route::qoder_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, guard).await;
            }
            TargetPool::Trae => {
                // issue #31 T3.2/T3.3：Trae 池 effort 通道（默认思考两池对齐）——
                // 显式 reasoning_effort / 路由级提示 / 默认思考 → wire 档位注入
                let explicit = peek
                    .get("reasoning_effort")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let wire = trae_effort_wire(&state, r.effort_hint, explicit, &r.model);
                let body_vec = inject_trae_outbound(
                    &state.data_dir, body_vec, wire, r.max_mode_hint, &model, &r.model,
                );
                if stream {
                    stream_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, guard)
                } else {
                    aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, guard).await
                }
            }
            TargetPool::Custom => {
                // 自定义上游组执行（P2）：同名多条 = 上游组，组内加权轮询+轮换；
                // 条目可能热更新，执行时重读全量 enabled 组，缺失（已被删）按 404 报错
                let group = super::custom_models::find_enabled_all(&state.data_dir, &model);
                if group.is_empty() {
                    openai_error(StatusCode::NOT_FOUND, "model_not_found", &format!("自定义模型 {} 已被删除", model))
                } else if stream {
                    custom_route::custom_stream_chat(state_clone, body_vec, r.model, group, start_ts, Protocol::OpenAi, key_str, guard)
                } else {
                    custom_route::custom_aggregate_chat(state_clone, body_vec, r.model, group, stream, start_ts, Protocol::OpenAi, key_str, guard).await
                }
            }
        },
    }
}

/// Codex Responses API 端点（T4.1/F-40）：POST /v1/responses
///
/// 请求投影为 OpenAI 内部格式后复用 WB 上游既有管线（取号/重试/粘性/脱敏一份）。
/// 仅支持 WB 上游模型（Codex CLI `wire_api="responses"` 直配 base_url 的目标场景）；
/// 脱敏沿用全局 `wb_sanitize` 开关，审核命中按既有分级重试表退回重试。
pub async fn responses_api(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > max_body_bytes() {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            &body_too_large_msg(),
        );
    }

    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();
    // issue #26 白名单准入（空名单不限）
    if let Some(resp) = whitelist_check(&state, &model, Protocol::Responses) {
        return resp;
    }

    // Responses → OpenAI chat 内部格式（纯投影，失败即 400）
    let chat_body: Value = match super::wb_responses::responses_to_chat(&peek) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &e);
        }
    };
    // 审查 F4：请求侧登记 namespace 分组展平编码名，随执行链下发；回程投影
    // （流式/非流式/toolexec）仅对集合内命中者做 {name, namespace} 还原——
    // 客户端原生扁平工具名可能天然含 `__`，无差别拆分会破坏工具分发
    let encoded_tools = super::wb_responses::encoded_tool_names(&peek);

    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());
    // inflight guard：随执行路径持有至请求结束（§4.5）
    let guard = state.inflight_guard();

    // T5.2/F-61 四段路由解析（Responses 仅支持 WB 上游模型）
    let (resolved_model, route_hint) = match resolve_wb_target(&state, &model, &chat_body) {
        Some(t) => t,
        None => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "model_not_found",
                &format!(
                    "model {} is not a WorkBuddy upstream model（/v1/responses 仅支持 WB 上游模型，目录见 /v1/models）",
                    model
                ),
            );
        }
    };
    // buddy-ai（阶段 2）：wb 档关闭且国际档开启时走国际池（与 /v1/chat 的
    // priority 语义一致：buddy 优先、buddy-ai 兜底；wb 开时保持现状 CN 路径）
    // 审查修复备案：/v1/responses 与 /v1/chat 的 Intl 分流存在已知不对称——wb 开启时
    // Intl 专属模型（buddy_intl_only；/v1/chat 经 dispatch available 门控不走 CN 池）
    // 在此处因 `buddy_ai = !wb_enabled && buddy_ai_enabled` 可能投给 CN 上游；
    // 与蓝本同位一致，属有意设计——若要统一需改 resolve_wb_target 签名波及多处调用，暂不处理。
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let buddy_ai_enabled = state
        .buddy_ai_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    let buddy_ai = !wb_enabled && buddy_ai_enabled;
    if !wb_enabled && !buddy_ai_enabled {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "wb_upstream_disabled",
            "WB 上游未启用（api_pool.json wb_enabled）",
        );
    }
    {
        let view = if buddy_ai {
            wb_route::WbView::intl(state.clone())
        } else {
            wb_route::WbView::wb(state.clone())
        };
        if wb_route::model_cooling_remaining(&view, &resolved_model).is_some() {
            return model_cooling_response(&view, &resolved_model, Protocol::Responses);
        }
    }

    let mut chat_body = chat_body;
    // T5.5/F-64 工具代执行：客户端声明 web_search 类工具且开关开启 → 代理侧代执行编排
    if state.wb_tool_exec.load(std::sync::atomic::Ordering::Relaxed)
        && super::wb_toolexec::responses_declares_web_search(&peek)
    {
        return wb_route::wb_tool_exec_chat(
            state_clone,
            buddy_ai,
            chat_body,
            resolved_model,
            stream,
            start_ts,
            key_str,
            guard,
            encoded_tools,
        )
        .await;
    }
    chat_body["stream"] = json!(stream);
    // T5.3 默认深度思考（Responses: reasoning.effort 已投影为 reasoning_effort）
    let explicit = chat_body.get("reasoning_effort").and_then(|v| v.as_str()).is_some();
    let hint = effective_effort_hint(&state, route_hint, explicit);
    let body_vec = apply_effort_hint(serde_json::to_vec(&chat_body).unwrap_or_default(), hint);

    if stream {
        wb_route::wb_stream_chat(state_clone, buddy_ai, body_vec, resolved_model, start_ts, Protocol::Responses, key_str, guard, encoded_tools)
    } else {
        wb_route::wb_aggregate_chat(state_clone, buddy_ai, body_vec, resolved_model, stream, start_ts, Protocol::Responses, key_str, guard, encoded_tools).await
    }
}

// ==================== issue #54 Claude Code 分类器模型兜底 ====================

/// 剥离模型名尾部的 `[1m]` 窗口标记（Claude 1M 上下文变体后缀），ASCII 大小写不敏感。
/// 纯字节尾比较：`[1m]` 全 ASCII（<0x80），不可能与多字节 UTF-8 尾字节混淆，
/// 切分点必为字符边界（'[' 起点）——任何输入不 panic。不引入 to_lowercase
/// 字节漂移（对照 wb_model_route::strip_suffix_ci 审查修复的同类切片问题）；
/// 仅识别尾部完整标记且剥离后非空
fn strip_bracket_1m(name: &str) -> Option<&str> {
    const SUFFIX: &[u8] = b"[1m]";
    let bytes = name.as_bytes();
    // 短路顺序保证：len > 4 先于切片，不会下溢
    if bytes.len() > SUFFIX.len()
        && bytes[bytes.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX)
    {
        Some(&name[..bytes.len() - SUFFIX.len()])
    } else {
        None
    }
}

/// 可路由判定（Anthropic 入口兜底专用）：Custom 直达命中 /
/// WB 路由解析命中目录且上游启用 / buddy-ai Intl 目录命中且国际档启用 /
/// Trae 模型列表 canonical 命中
fn anthropic_model_servable(state: &ApiSharedState, model: &str) -> bool {
    if super::custom_models::find_enabled(&state.data_dir, model).is_some() {
        return true;
    }
    let cfg = wb_model_route::load_config(&state.data_dir);
    let catalog = wb_catalog::load(&state.data_dir);
    let r = wb_model_route::resolve(&cfg, &catalog, model);
    if state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed)
        && wb_catalog::find(&catalog, &r.model).is_some()
    {
        return true;
    }
    // G6 审查修复：buddy-ai 国际档对称判定——/v1/messages 经 dispatch 已可路由
    // Intl 目录模型（TargetPool::BuddyAi），本判定原先只认 CN 目录，纯国际档
    // 用户（wb 关、intl 开）被误判不可路由，分类器名一律回落默认模型。仅按
    // 请求原名直查 Intl 目录：Intl 专属 id（如 deepseek-v4.1-flash）不经 CN
    // 四段路由映射，原名即 dispatch 路由名。不可用 r.model（L1043 CN 目录
    // resolve 结果，claude-* 系会被映射成 CN 模型名）查 Intl 目录——映射名
    // 恰在 Intl 目录时会产生假阳性，分类器名被误判「可路由」而跳过回落
    if state
        .buddy_ai_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        let intl_catalog = wb_catalog::buddy_ai_load(&state.data_dir);
        if wb_catalog::find(&intl_catalog, model).is_some() {
            return true;
        }
    }
    super::models_sync::load_models(&state.data_dir)
        .iter()
        .any(|m| unified_catalog::canonical_id(&m.id) == unified_catalog::canonical_id(model))
}

/// Claude Code auto 模式（v2.1.83+）把权限审批交给独立「安全分类器」模型：
/// 分类器以 side_query 请求本网关 `/v1/messages`，模型名为 Claude Code 服务端
/// feature flag 下发的官方名（如 `claude-sonnet-5[1m]`）——无环境变量可覆盖，
/// 三池不可路由时上游必 4001/失败，Claude Code 随即 fail-closed 报
/// "temporarily unavailable (timed out)" 并阻断 Bash/Write（issue #54）。
///
/// 入口兜底：原名可路由 → 原样（WB 用户经内置系列 claude-* 映射的行为不变）；
/// 剥离 `[1m]` 标记后基名可路由 → 用基名；仍不可路由的 `claude-*` 系名
/// （Anthropic 内部名，上游不可能服务）回落网关默认模型。
/// 返回 Some(改写后模型名)；None = 原样保留（非 claude 未知名维持透传语义）
fn anthropic_model_fallback(state: &ApiSharedState, model: &str) -> Option<String> {
    let m = model.trim();
    if m.is_empty() {
        return None;
    }
    // 热路径短路（零分配）：仅 claude-* 系名与带 [1m] 窗口标记的请求参与判定
    let bytes = m.as_bytes();
    let is_claude = bytes.len() >= 6 && bytes[..6].eq_ignore_ascii_case(b"claude");
    let has_1m = strip_bracket_1m(m).is_some();
    if !is_claude && !has_1m {
        return None;
    }
    // ① 原名可路由（含 Custom 命中、WB 系列映射命中且启用）→ 原样保留
    if anthropic_model_servable(state, m) {
        return None;
    }
    // ② 剥 [1m] 后基名可路由（如用户以基名配置了 Custom claude 条目）→ 用基名
    if let Some(base) = strip_bracket_1m(m) {
        if anthropic_model_servable(state, base) {
            return Some(base.to_string());
        }
    }
    // ③ claude-* 系不可路由 → 回落网关默认模型；非 claude 未知名维持透传
    //（与 dispatch t07 透传语义零行为差异）。边界：默认模型自身不可服务时
    // 不再二次改写，与「未指定 model 的请求」走同一失败路径，行为一致
    let default = state.default_model.trim();
    if is_claude && !default.is_empty() && !default.eq_ignore_ascii_case(m) {
        return Some(default.to_string());
    }
    None
}

/// Anthropic Messages 端点（F-39：+Anthropic 适配）
/// 请求：POST /v1/messages，鉴权支持 x-api-key 或 Authorization: Bearer
pub async fn messages(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > max_body_bytes() {
        return anthropic_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            &body_too_large_msg(),
        );
    }

    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // 校验 JSON 并预读 stream/model，再整体转为 OpenAI 内部格式复用现有链路
    let peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    if peek.get("messages").and_then(|m| m.as_array()).is_none() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages: field required",
        );
    }
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();
    // issue #54 分类器模型兜底：改写原子生效——peek 插入与 body 重序列化任一步
    // 失败则整体跳过（model 变量与请求体模型名永不不一致），保证调度、outbound
    // 转换与白名单准入使用同一（可路由）模型名
    let mut peek = peek;
    let mut body = body;
    if let Some(rewritten) = anthropic_model_fallback(&state, &model) {
        let new_body = peek
            .as_object_mut()
            .map(|obj| {
                obj.insert("model".into(), serde_json::Value::String(rewritten.clone()));
            })
            .and_then(|_| serde_json::to_vec(&peek).ok())
            .map(axum::body::Bytes::from);
        if let Some(bytes) = new_body {
            crate::fs_utils::app_log(
                &state.data_dir,
                &format!(
                    "anthropic model fallback (issue #54): model={} -> {}",
                    model, rewritten
                ),
            );
            body = bytes;
            model = rewritten;
        }
    }
    // issue #26 白名单准入（空名单不限）
    if let Some(resp) = whitelist_check(&state, &model, Protocol::Anthropic) {
        return resp;
    }

    let body_vec = super::payload::anthropic_to_openai(&body);
    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());

    // 统一调度分流点（§4.1）：resolve_target 决定资源池/回退/错误矩阵；
    // guard 随执行路径持有至请求结束（流式含整个后台任务）
    let guard = state.inflight_guard();
    match dispatch::resolve_target(&state, &model, &peek, Some(&key_str)) {
        Err(e) => dispatch_error_response(
            &state, e, Protocol::Anthropic, &model, stream, &key_str,
            start_ts.elapsed().as_millis() as u64,
        ),
        Ok(r) => match r.pool {
            // buddy-ai（阶段 2）：与 Buddy 档共用执行内核，视图按池分流
            TargetPool::Buddy | TargetPool::BuddyAi => {
                // T5.3 默认深度思考：Anthropic 侧 thinking 参数视为显式请求
                let explicit = peek.get("thinking").map_or(false, |t| !t.is_null());
                let hint = effective_effort_hint(&state, r.effort_hint, explicit);
                let body_vec = apply_effort_hint(body_vec, hint);
                let buddy_ai = r.pool == TargetPool::BuddyAi;
                if stream {
                    return wb_route::wb_stream_chat(state_clone, buddy_ai, body_vec, r.model, start_ts, Protocol::Anthropic, key_str, guard, std::collections::HashSet::new());
                }
                return wb_route::wb_aggregate_chat(state_clone, buddy_ai, body_vec, r.model, stream, start_ts, Protocol::Anthropic, key_str, guard, std::collections::HashSet::new()).await;
            }
            TargetPool::Qoder => {
                // Qoder 上游（p3-3）：Anthropic 入参已在端点层投影为 OpenAI 内部格式
                if stream {
                    return super::qoder_route::qoder_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::Anthropic, key_str, guard);
                }
                return super::qoder_route::qoder_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::Anthropic, key_str, guard).await;
            }
            TargetPool::Trae => {
                // issue #31 T3.2/T3.3：Anthropic thinking 参数视为显式请求
                //（enabled → high 档；disabled → 空串短路默认思考与路由提示）
                let explicit = match peek.get("thinking") {
                    Some(t) if !t.is_null() => {
                        match t.get("type").and_then(|v| v.as_str()) {
                            Some("enabled") => Some("high".to_string()),
                            // 显式关闭（type=disabled 等已知关闭形态）：空串经
                            // trae_request_wire 短路 → 不下发
                            Some(_) => Some(String::new()),
                            // 缺 type（非合规形态，如仅 {"budget_tokens":N}）：对齐
                            // Buddy 侧「thinking 存在即显式开启」语义，判为开启
                            None => Some("high".to_string()),
                        }
                    }
                    _ => None,
                };
                let wire = trae_effort_wire(&state, r.effort_hint, explicit, &r.model);
                let body_vec = inject_trae_outbound(
                    &state.data_dir, body_vec, wire, r.max_mode_hint, &model, &r.model,
                );
                if stream {
                    stream_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::Anthropic, key_str, guard)
                } else {
                    aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::Anthropic, key_str, guard).await
                }
            }
            TargetPool::Custom => {
                // 自定义上游组执行（P2）：同名多条 = 上游组（注释同 /v1/chat/completions 分支）
                let group = super::custom_models::find_enabled_all(&state.data_dir, &model);
                if group.is_empty() {
                    anthropic_error(StatusCode::NOT_FOUND, "model_not_found", &format!("自定义模型 {} 已被删除", model))
                } else if stream {
                    custom_route::custom_stream_chat(state_clone, body_vec, r.model, group, start_ts, Protocol::Anthropic, key_str, guard)
                } else {
                    custom_route::custom_aggregate_chat(state_clone, body_vec, r.model, group, stream, start_ts, Protocol::Anthropic, key_str, guard).await
                }
            }
        },
    }
}

/// OpenAI legacy text completions 端点（T9）
/// prompt（string 或 string[]）转单条 user message 复用现有链路，响应包装回
/// text_completion 结构。suffix/echo/logprobs/n 等参数不支持（忽略，上游单次补全）
pub async fn completions(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > max_body_bytes() {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            &body_too_large_msg(),
        );
    }

    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    let prompt_text = match peek.get("prompt") {
        Some(Value::String(s)) => s.clone(),
        // 多段 prompt：拼接为单个 prompt（上游一次只产出一个补全，无法返回多 choice）
        Some(Value::Array(arr)) => {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if parts.is_empty() {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "prompt: array must contain strings",
                );
            }
            parts.join("\n\n")
        }
        _ => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "prompt: field required",
            )
        }
    };
    if prompt_text.trim().is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "prompt: must not be empty",
        );
    }
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();
    // issue #26 白名单准入（空名单不限）
    if let Some(resp) = whitelist_check(&state, &model, Protocol::OpenAiText) {
        return resp;
    }

    // prompt → user message，复用 /v1/chat/completions 内部链路
    let internal = json!({
        "model": model,
        "stream": stream,
        "messages": [{ "role": "user", "content": prompt_text }],
    });
    let body_vec = internal.to_string().into_bytes();

    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());

    // 统一调度分流点（§4.1）；guard 随执行路径持有至请求结束
    let guard = state.inflight_guard();
    match dispatch::resolve_target(&state, &model, &internal, Some(&key_str)) {
        Err(e) => dispatch_error_response(
            &state, e, Protocol::OpenAiText, &model, stream, &key_str,
            start_ts.elapsed().as_millis() as u64,
        ),
        Ok(r) => match r.pool {
            // buddy-ai（阶段 2）：与 Buddy 档共用执行内核，视图按池分流
            TargetPool::Buddy | TargetPool::BuddyAi => {
                // T5.3 默认深度思考（text completions 无 effort 字段 → 默认思考直接生效）
                let hint = effective_effort_hint(&state, r.effort_hint, false);
                let body_vec = apply_effort_hint(body_vec, hint);
                let buddy_ai = r.pool == TargetPool::BuddyAi;
                if stream {
                    return wb_route::wb_stream_chat(state_clone, buddy_ai, body_vec, r.model, start_ts, Protocol::OpenAiText, key_str, guard, std::collections::HashSet::new());
                }
                return wb_route::wb_aggregate_chat(state_clone, buddy_ai, body_vec, r.model, stream, start_ts, Protocol::OpenAiText, key_str, guard, std::collections::HashSet::new()).await;
            }
            TargetPool::Qoder => {
                // Qoder 上游（p3-3）：text completions 入参已在端点层投影为内部格式
                if stream {
                    return super::qoder_route::qoder_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::OpenAiText, key_str, guard);
                }
                return super::qoder_route::qoder_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAiText, key_str, guard).await;
            }
            TargetPool::Trae => {
                // issue #31 T3.2/T3.3：text completions 无 effort 字段 → 仅默认思考生效
                let wire = trae_effort_wire(&state, r.effort_hint, None, &r.model);
                let body_vec = inject_trae_outbound(
                    &state.data_dir, body_vec, wire, r.max_mode_hint, &model, &r.model,
                );
                if stream {
                    stream_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAiText, key_str, guard)
                } else {
                    aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAiText, key_str, guard).await
                }
            }
            TargetPool::Custom => {
                // 自定义上游组执行（P2）：同名多条 = 上游组（注释同 /v1/chat/completions 分支）
                let group = super::custom_models::find_enabled_all(&state.data_dir, &model);
                if group.is_empty() {
                    openai_error(StatusCode::NOT_FOUND, "model_not_found", &format!("自定义模型 {} 已被删除", model))
                } else if stream {
                    custom_route::custom_stream_chat(state_clone, body_vec, r.model, group, start_ts, Protocol::OpenAiText, key_str, guard)
                } else {
                    custom_route::custom_aggregate_chat(state_clone, body_vec, r.model, group, stream, start_ts, Protocol::OpenAiText, key_str, guard).await
                }
            }
        },
    }
}

/// /v1/embeddings：上游 SOLO 无向量能力，明确返回 501（不做假实现）
pub async fn embeddings() -> Response {
    openai_error(
        StatusCode::NOT_IMPLEMENTED,
        "not_supported",
        "上游服务无 embeddings 能力，本网关不支持 /v1/embeddings，请使用 /v1/chat/completions 或 /v1/completions",
    )
}

/// /v1/images/generations 文生图（T5.4/F-63）：投影 WB 上游生图端点
pub async fn images_generations(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    images_entry(state, key_id, body, false).await
}

/// /v1/images/edits 图生图（T5.4/F-63）：接受 JSON（image 为 base64/data URL）。
/// 注：OpenAI SDK 默认 multipart/form-data；本端点仅接受 JSON 变体（零新增依赖红线），
/// 客户端需将图像读为 base64 后以 JSON 提交。
pub async fn images_edits(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    images_entry(state, key_id, body, true).await
}

async fn images_entry(
    state: Arc<ApiSharedState>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
    is_edit: bool,
) -> Response {
    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // inflight guard（§4.5）：async fn 全程 inline await，作用域即请求生命周期
    let _guard = state.inflight_guard();
    if body.len() > max_body_bytes() {
        return openai_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", &body_too_large_msg());
    }
    let peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &format!("invalid JSON body: {}", e)),
    };
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("hy4")
        .to_string();
    // issue #26 白名单准入（空名单不限）
    if let Some(resp) = whitelist_check(&state, &model, Protocol::OpenAi) {
        return resp;
    }
    let prompt = peek.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let image_b64 = peek.get("image").and_then(|v| v.as_str()).map(str::to_string);
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());
    let start_ts = std::time::Instant::now();
    // 请求日志附带的 API Key 展示名（匿名/未知 → 空串，日志显示 "-"）
    let key_name = super::api_keys::key_name_for(&state.data_dir, &key_str);

    // 校验（目录命中 + 图片模态 + prompt/image 非空）
    let catalog = wb_catalog::load(&state.data_dir);
    if let Err((code, msg)) = super::wb_images::validate(
        &catalog,
        &model,
        &prompt,
        if is_edit { Some(image_b64.as_deref().unwrap_or("")) } else { None },
    ) {
        return openai_error(
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
            "invalid_request_error",
            &msg,
        );
    }

    // 取健康 WB 账号（生图无粘性语义，任一健康账号）；携带当前请求 Key 的
    // 约束（P1 修复4c）：白名单 allowed_accounts 过滤 + dedicated 专一锁定，
    // 与 wb_route 同款解析；Key 无约束/匿名（constraints_for 为 None）时不限制。
    // issue #25 资源池绑定：Key 绑定 trae 时约束作用域不在 WB 池（白名单空），
    // 生图仍用 WB 池但不应用 Trae 账号白名单（避免误过滤）。
    // issue #30 混合白名单：按前缀作用域提取 Buddy 条目（空集 = 排除，不得过滤）
    let (allowed_set, dedicated) = super::api_keys::constraints_for(&state.data_dir, &key_str)
        .and_then(|k| k.pool_constraints("buddy"))
        .map_or((None, None), |c| (c.allowed, c.dedicated));
    let picked = {
        let tried = HashSet::new();
        state
            .wb_pool
            .pick_excluding_constrained_ev(&tried, allowed_set.as_ref(), dedicated.as_deref(), false)
            .map(|(p, ev)| {
                // F-77⑤ 可观测：busy_yield / busy_fallback 调度事件
                if let Some(ev) = ev {
                    state.logger.log_sched_event(&ev);
                }
                p
            })
    };
    let Some(picked) = picked else {
        return openai_error(StatusCode::SERVICE_UNAVAILABLE, "no_healthy_account", "no healthy WB account available");
    };
    let creds = super::wb_upstream::WbCreds {
        id: picked.uid.clone(),
        uid: picked.uid.clone(),
        name: String::new(),
        token: picked.jwt.clone(),
        domain: picked.domain.clone(),
        enterprise_id: picked.enterprise_id.clone(),
        global_region: picked.global_region,
    };

    let result = tokio::task::spawn_blocking(move || super::wb_images::generate(&creds, &peek))
        .await
        .unwrap_or_else(|e| Err((500u16, format!("task join error: {e}"))));
    let duration_ms = start_ts.elapsed().as_millis() as u64;
    match result {
        Ok(resp) => {
            // P1 修复4a：WB 账号服务的请求记 wb 桶（is_wb=true），与 Trae 侧分账
            state.record_usage(true, &model, &picked.uid, &key_str, true, false, duration_ms, 0, 0);
            state.wb_pool.note_success(&picked.uid);
            // P1 修复4b：上游池为 buddy，日志池归属同步纠正
            state.logger.log_request(
                "buddy", "POST",
                if is_edit { "/v1/images/edits" } else { "/v1/images/generations" },
                &model, false, 200, &picked.uid, duration_ms, &key_name,
                &state.wb_pool.name_of(&picked.uid), None,
            );
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(resp.to_string()))
                .unwrap_or_else(|_| internal_error_response())
        }
        Err((code, msg)) => {
            state.record_usage(true, &model, &picked.uid, &key_str, false, false, duration_ms, 0, 0);
            state.logger.log_request(
                "buddy", "POST",
                if is_edit { "/v1/images/edits" } else { "/v1/images/generations" },
                &model, false, code, &picked.uid, duration_ms, &key_name,
                &state.wb_pool.name_of(&picked.uid), Some(&msg),
            );
            openai_error(
                StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                "upstream_error",
                &msg,
            )
        }
    }
}

// ==================== Streaming ====================

/// issue #25 资源池绑定：Key 绑定 trae 时 F-35 约束（白名单/专一）作用于 Trae 池；
/// 其余情况（未绑定旧语义/绑定 buddy/匿名）返回 (None, None) = 不限制。
/// F-35 旧语义兼容：未绑定 Key 的白名单历来只作用 WB 池，绝不能波及 Trae。
/// issue #30 混合白名单：带池前缀的未绑定 Key 按前缀作用域提取 Trae 条目
/// （零条目 = 空集排除）。pub(crate) 供 dispatch 测试模块复用 Fixture 覆盖四种作用域
pub(crate) fn trae_pool_constraints(
    state: &ApiSharedState,
    key_id: &str,
) -> (Option<HashSet<String>>, Option<String>) {
    super::api_keys::constraints_for(&state.data_dir, key_id)
        .and_then(|kc| kc.pool_constraints("trae"))
        .map_or((None, None), |c| (c.allowed, c.dedicated))
}

#[allow(clippy::too_many_arguments)]
fn stream_chat(state: Arc<ApiSharedState>, body_vec: Vec<u8>, model: String, stream: bool, start_ts: std::time::Instant, proto: Protocol, key_id: String, guard: InflightGuard) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    // SSE keep-alive 15s（T2.7/F-34 §5.5 #7）：防中间层回收长流。
    // P1 修复：原 ticker 独占持有 sender 克隆，主任务发完 [DONE] 后流因
    // sender 未全部关闭而无法终结（普通 HTTP 客户端只能靠断连收尾）。
    // 改用 tokio::sync::watch：主任务结束（DoneSignal Drop）置 done=true，
    // ticker select! 收到退出信号即退出 → rx 关闭 → 流正常结束
    let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
    {
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            tick.tick().await; // 首个 tick 立即返回，跳过
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if tx2
                            .send(Ok(bytes::Bytes::from(": keep-alive\n\n")))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // 主任务已结束：ticker 退出，放行流终结
                    _ = done_rx.changed() => break,
                }
            }
        });
    }

    // 批次 D-1 线程隔离：流任务迁入专用阻塞池，长流不再占用主 runtime 的
    // spawn_blocking 池（鉴权/短 IO 依赖它），防并发流耗尽池导致网关级联卡死。
    // Key 绑定 trae 的约束在闭包外解析（clone 进闭包，锁外取号）
    let (trae_allowed, trae_dedicated) = trae_pool_constraints(&state, &key_id);
    super::stream_runtime().spawn_blocking(move || {
        // inflight guard 随后台任务存续至流结束（§4.5：客户端断连/流终止由
        // 任务结束 Drop 兜底释放）；F-77 取号后绑定账号级计数
        let mut guard = guard;
        // 主任务结束（含 panic 展开）→ 通知 keep-alive ticker 退出（P1 修复3）
        let _done = DoneSignal(done_tx);
        // 请求日志附带的 API Key 展示名（匿名/未知 → 空串，日志显示 "-"）
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        let chat_id = match proto {
            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
            Protocol::Anthropic => format!("msg_{}", now_ts()),
            // Responses 仅走 WB 上游；solo 管线不会收到，兜底给 resp_ id
            Protocol::Responses => format!("resp_{}", now_ts()),
        };
        // 会话→账号粘性（per-pool 三参数批次，对齐 Buddy/Qoder）：显式
        // conversationId / 消息指纹命中且账号健康 → 首选粘住账号（上游 KV cache
        // 复用；busy 且有空闲候选时让位，F-77④ 同构）。子 Key 限定上游不含
        // 粘性账号时忽略粘性
        // issue #138 P0：sticky 键窄探测——不再整包解析 DOM，仅提取会话键所需
        // 字段（conversation_id + 前 3 条消息），大 body 下省 3~6 倍解析放大
        let sticky_key = SessionKey::from_slice(&body_vec);
        // 空指纹（无 messages / 无 conversation_id）不可粘
        let sticky_usable = !sticky_key.cache_key().ends_with(':');
        let sticky0: Option<String> = if sticky_usable {
            state
                .trae_sticky
                .resolve(&sticky_key, now_ts() as i64)
                .and_then(|b| {
                    if trae_allowed.as_ref().is_some_and(|a| !a.contains(&b.uid)) {
                        return None;
                    }
                    state.pool.pick_by_uid(&b.uid, false).map(|_| b.uid)
                })
        } else {
            None
        };
        // 首选：粘性 > 调度策略；绑定回写用会话键占位 conv_id（Trae 上游无
        // 会话 id 复用语义，仅落库留档满足非空校验）
        let sticky_conv = sticky_key.cache_key();
        let mut first_pick: Option<super::pool::PickedAccount> = sticky0.as_ref().and_then(|u| {
            state
                .pool
                .pick_sticky_yield(u, trae_allowed.as_ref(), false)
                .map(|(p, ev)| {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                })
        });
        let mut tried = HashSet::new();
        // 审查修复（L1）：最近绑定账号（绑定成功才更新）——外层断连终局归因复用，
        // 防换号重试期间断连落 "none" 致账单排查失据（对齐 wb_route）
        let mut last_bound_uid: Option<String> = None;
        // 401 自愈去重（issue #27 方案 B）：每账号每请求最多强制刷新一次（对齐 wb_route T2.6）
        let mut refreshed_401 = HashSet::new();
        // 空完成计数（issue #57 后续可观测）：轮换耗尽收尾时区分「渠道指纹拦截」与「账号池耗尽」
        let mut empty_hits: usize = 0;
        // 传输掐断计数：与空完成分开归因，耗尽时用于修正 "no healthy account" 文案
        let mut cut_hits: usize = 0;
        // 11128 诊断样本按请求去重（换号轮换重复命中不重复打样）
        let mut last_11128_sample: Option<String> = None;
        // 指纹清洗（issue #57）：请求级快照（对齐 wb_route）——11128 强制开启后
        // 跨账号保持，换号不再以未清洗状态重烧一次拦截；热更新开关下请求生效
        let mut sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);
        // 11128 显式指纹确认置位（issue #122 缺口3 审查补口）：强制清洗 rebuild 后
        // 同号重试直接消费 rebuild 结果（内层循环不重算 converted），但换号到下一
        // 账号 / 触发对冲竞速时循环顶会以 empty_hits>0 || fp_block_recent 重算激进
        // 位——纯 11128 首拦两条件均假，激进意图被丢弃且主/对冲清洗强度不一致。
        // 此处置位后循环顶与对冲重建统一沿用全力清洗
        let mut force_aggressive = false;
        let templates = super::wb_route::load_templates(&super::wb_route::WbView::wb(state.clone()));
        // 工具别名表（issue #150）：sanitize 门控下请求侧正向改写 + 响应侧逆映射
        // 回改（客户端按名匹配 tool_calls，单向改名即废）；表热更新、保存即生效
        let tool_alias_rules =
            super::wb_route::load_tool_alias_rules(&super::wb_route::WbView::wb(state.clone()));
        let mut last_residual_note: Option<String> = None; // 残指纹自检按请求去重（issue #122 C3）
        // 多候选改写种子基底（issue #122 方案 C1）：会话键 + 请求时间戳绑定请求，
        // tried.len() 随换号轮换递增——跨重试候选必变
        let seed_base = format!("{}#{}", sticky_conv, now_ts());

        for _ in 0..MAX_ROTATE {
            // 客户端断连检测：通道关闭即终止轮换/重试，不再占用账号并发槽。
            // 审查 M5：断连也是请求终局，补记一行失败用量（未取号 → "none"，
            // 与下方轮换耗尽收尾同口径），防用量页 requests 缺口
            if tx.is_closed() {
                // 审查修复（L1）：归因复用最近绑定账号（从未绑定才落 "none"）
                let uid = last_bound_uid.as_deref().unwrap_or("none");
                state.record_usage(
                    false, &model, uid, &key_id, false, true,
                    start_ts.elapsed().as_millis() as u64, 0, 0,
                );
                return;
            }
            // 取号：粘性命中优先（每请求仅首轮），否则按 Key 约束 + 调度策略；
            // 换号重试后仅走策略
            let mut picked = match first_pick.take() {
                Some(p) => p,
                None => match state
                    .pool
                    .pick_excluding_constrained(&tried, trae_allowed.as_ref(), trae_dedicated.as_deref())
                {
                    Some(p) => p,
                    None => break,
                },
            };
            tried.insert(picked.uid.clone());
            // F-77 账号级在途计数：取号即绑定（换号时 bind_account 自动解绑旧账号）。
            // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
            // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（专一/粘性
            // 锁定、busy_fallback 降级）保留原无条件绑定语义
            let bound = if picked.over_limit {
                guard = guard.bind_account(state.pool.inflight_handle(&picked.uid));
                true
            } else {
                guard.bind_account_cas(
                    state.pool.inflight_handle(&picked.uid),
                    state.pool.concurrency_limit(),
                )
            };
            if !bound {
                continue;
            }
            // 审查修复（L1）：绑定成功才更新「最近绑定账号」（CAS 拒绝的候选不算）
            last_bound_uid = Some(picked.uid.clone());
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            // 多候选改写种子（issue #122 方案 C1）：tried.len() 随换号轮换递增；
            // 空完成后（empty_hits>0）、池级指纹拦截 TTL 内（issue #122 缺口2）
            // 或 11128 强制清洗置位（force_aggressive）时追加激进清洗（方案 C2）
            let seed = super::wb_payload::variant_seed(&seed_base, tried.len() as u64);
            let mut converted = super::payload::prepare_llm_chat_body(
                &body_vec, &state.default_model, &picked.uid, &picked.device_id, &picked.machine_id,
                // 模型目录（config_cache 缓存，热路径）：function 查表优先
                &super::models_sync::load_models(&state.data_dir),
                sanitize, &templates,
                seed, empty_hits > 0 || super::wb_payload::fp_block_recent("trae") || force_aggressive,
            );
            // 工具别名正向改写（issue #150）：sanitize 门控；逆映射供响应侧回改
            //（覆盖 tools/tool_choice/历史 tool_calls 三个透传点，零命中 None 直通）
            let mut tool_alias: Option<std::collections::HashMap<String, String>> = None;
            if sanitize {
                tool_alias =
                    super::wb_payload::apply_tool_alias_forward(&mut converted, &tool_alias_rules);
            }
            // 残指纹自检（issue #122 方案 C3）：改写后首条 system 仍命中已知特征词
            // → debug 日志（按请求去重），提示模板表需经 UI「指纹清洗」tab 热更新
            if sanitize {
                if let Some(note) = super::wb_payload::residual_fingerprint_note(&converted) {
                    if last_residual_note.as_deref() != Some(note.as_str()) {
                        last_residual_note = Some(note.clone());
                        state.logger.log_debug_line(format!(
                            "[DEBUG] 残指纹自检命中: {note} pool=trae uid={}",
                            picked.uid
                        ));
                    }
                }
            }

            // 分级重试（T2.2/F-33，与 wb_route 同一张表）：same_attempt 为同账号
            // 重试计数，换号后随新账号归零；总轮换上限仍受 MAX_ROTATE 约束
            let mut same_attempt: u32 = 0;
            loop {
                // TTFB 计时（请求发起 → 上游首行到达，与 wb_route 同语义）：
                // AtomicU64 0 哨兵 = 尚未读到首行（首字超时等失败路径不产出 ttfb）
                let ttfb_start = std::time::Instant::now();
                let ttfb_us = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                match make_upstream_request(&picked.jwt, &picked.uid, &picked.device_id, &picked.machine_id, &converted) {
                    Ok((reader, primary_hdr)) => {
                        // 首字超时 10s（T2.7/F-34）+ F-76③ 慢请求竞速对冲（Trae 池，
                        // wb_route/qoder_route 同构）：首字节超阈值且有其他健康账号时
                        // 向第二账号发对冲请求，先出首字者胜；阈值 0 = 纯首字超时（原语义）。
                        // 双败/首字超时 → 冷却换号；首字节到达后正常流速不受限
                        let mut win = match race_trae_first_byte(
                            &state, &picked.uid, reader, primary_hdr, &tried, trae_allowed.as_ref(),
                            trae_dedicated.as_deref(), &body_vec, sanitize, &templates,
                            &tool_alias_rules,
                            // 对冲重建沿用主请求的激进口径与种子（issue #122 C1/C2；
                            // 11128 强制清洗后 force_aggressive 贯穿，主/对冲同强度）
                            seed, empty_hits > 0 || super::wb_payload::fp_block_recent("trae") || force_aggressive,
                        ) {
                            Ok(w) => w,
                            Err(()) => {
                                state.pool.note_error(&picked.uid, ErrKind::Server);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} first-byte timeout(10s)", picked.uid));
                                // 每请求终态单行（审查问题6）：首字超时 → 换号是中间态，
                                // 不落账（对齐 wb 流式路径），由终局分支统一落行
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    504, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                    &key_name, &state.pool.name_of(&picked.uid),
                                    Some("first byte timeout"),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, None, 504, Some("first byte timeout"));
                                }
                                break; // 换号
                            }
                        };
                        // 对冲计数落定 + guard 重绑（接管时生效账号 = 对冲账号）；
                        // 后续记账/日志/粘性绑定均以生效账号 win_uid 为准
                        guard = settle_trae_hedge(&state, &mut win, guard, &picked.uid);
                        let TraeRaceWin { lines: win_lines, uid: win_uid, upstream_hdr, .. } = win;
                        // 首行打点包装：首次成功读到上游行即记录 ttfb（0 哨兵防重复
                        // 覆盖；叠加首字超时包装，语义为「请求发起 → 首行到达」）
                        let lines = {
                            let ttfb_flag = ttfb_us.clone();
                            win_lines.map(move |l| {
                                let _ = ttfb_flag.compare_exchange(
                                    0,
                                    ttfb_start.elapsed().as_micros() as u64,
                                    std::sync::atomic::Ordering::Relaxed,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                l
                            })
                        };
                        // ttfb 包装产物为普通迭代器 → 桥接回可中断行源
                        // （转发线程复用；转换循环的停滞期断连轮询语义不变）
                        let lines = super::wb_upstream::InterruptibleLines::from_iterator(
                            Box::new(lines) as Box<dyn Iterator<Item = String> + Send>,
                        );
                        // 连接成功 → 开始流式转换，mid-stream error 只冷却不轮换
                        let (error_info, sent_any, up_usage) = match proto {
                            Protocol::OpenAi => {
                                let (e, s, u) =
                                    sse::stream_convert_lines(lines, tx.clone(), &chat_id, tool_alias.as_ref());
                                (e, s, u)
                            }
                            Protocol::OpenAiText => {
                                let (e, s, u) =
                                    sse::stream_convert_text_lines(lines, tx.clone(), &chat_id, &model);
                                (e, s, u)
                            }
                            Protocol::Anthropic => {
                                let (e, s, u) = sse::stream_convert_anthropic_lines(
                                    lines, tx.clone(), &chat_id, &model, tool_alias.as_ref(),
                                );
                                (e, s, u)
                            }
                            // Responses 仅走 WB 上游；solo 管线兜底按 OpenAI 透传
                            Protocol::Responses => {
                                let (e, s, u) =
                                    sse::stream_convert_lines(lines, tx.clone(), &chat_id, tool_alias.as_ref());
                                (e, s, u)
                            }
                        };
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        // TTFB：首行到达耗时（未读到首行 → None，不输出该字段）
                        let ttfb_ms = {
                            let us = ttfb_us.load(std::sync::atomic::Ordering::Relaxed);
                            if us == 0 { None } else { Some(us / 1000) }
                        };
                        // 用量记账（流式结束即落盘；F-76① TTFT 入账；对冲接管记生效账号）。
                        // 每请求终态单行（审查问题6）：换号重试为中间态不落账，避免
                        // requests 虚高——判定与下方换号分支同口径（空完成未流出 /
                        // 账号级错误未流出）；终态（成功 / 就地收尾 / 错误透传）恰落一行
                        let rotating = match &error_info {
                            None => false,
                            Some((code, msg)) => {
                                (super::is_empty_completion(*code, msg) && !sent_any)
                                    || (!sent_any
                                        && classify_solo_error(*code, msg) != ErrKind::None)
                            }
                        };
                        if !rotating {
                            let (pt, ct) = up_usage.as_ref().map(extract_tokens).unwrap_or((0, 0));
                            // M4 审查修复：缓存读 tokens 旁路入账（宽口径别名提取；
                            // 上游未下发得 0 不入账，与 credits「不得伪造」口径一致）
                            state.record_usage_cached(
                                super::usage::UsageBucket::Trae,
                                up_usage.as_ref().map(super::wb_sse::cached_tokens_of).unwrap_or(0),
                            );
                            state.record_usage_ttfb(
                                false, &model, &win_uid, &key_id, error_info.is_none(), true,
                                duration_ms, pt, ct, ttfb_ms,
                            );
                        }
                        if let Some((code, msg)) = error_info {
                            if super::is_empty_completion(code, &msg) {
                                // sent_any 护栏（对齐 qoder_route）：内容已流出时换号重发
                                // 会向同一 SSE 流拼接第二份完整响应 → 就地收尾不再重试
                                if sent_any {
                                    state.logger.log_request_ttfb(
                                        "trae", "POST", proto.log_path(), &model, stream,
                                        200, &win_uid, duration_ms, ttfb_ms, &key_name,
                                        &state.pool.name_of(&win_uid),
                                        Some(&format!(
                                            "空完成（内容已流出）→ 就地收尾 [upstream: {}]",
                                            upstream_hdr
                                        )),
                                    );
                                    return;
                                }
                                // 空完成（影子风控/上游异常，issue #57）：不冷却、不透传，
                                // 换号重试（收尾帧未发，重试流可在同一连接续传）
                                empty_hits += 1;
                                state.logger.log_request_ttfb(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    200, &win_uid, duration_ms, ttfb_ms, &key_name,
                                    &state.pool.name_of(&win_uid),
                                    Some(&format!(
                                        "空完成 → 换号重试{} [upstream: {}]",
                                        super::wb_payload::template_hit_note(),
                                        upstream_hdr
                                    )),
                                );
                                break; // 退出重试循环 → 换号
                            }
                            // 传输层掐断哨兵（issue #158 报错A，P2-1）：上游 h2
                            // RST_STREAM/连接重置掐断长流——与账号质量无关：不冷却、
                            // 不解绑粘性、不计空完成指纹。须在 classify_solo_error
                            // 之前预检（哨兵码经分类器会落 Server 误冷却）。按
                            // sent_any 分流：
                            // - 有内容被掐：sse 层已补收尾帧，客户端正常收完——
                            //   就地收尾落账（success=false，诚实记账），结构化
                            //   日志让「~5min 长流被掐」从不可见变可见
                            // - 零内容被掐：错误帧未下发，换号重试（不落账，
                            //   中间态与空完成换号同口径）
                            if super::is_transport_cut(code, &msg) {
                                cut_hits += 1;
                                if sent_any {
                                    state.logger.log_request_ttfb(
                                        "trae", "POST", proto.log_path(), &model, stream,
                                        200, &win_uid, duration_ms, ttfb_ms, &key_name,
                                        &state.pool.name_of(&win_uid),
                                        Some(&format!(
                                            "长流被上游掐断（{}）→ 就地收尾 [upstream: {}]",
                                            safe_slice(&msg, 160),
                                            upstream_hdr
                                        )),
                                    );
                                    if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                        state.logger.log_debug(&win_uid, &converted, None, 200, Some(&msg));
                                    }
                                    return;
                                }
                                state.logger.log_request_ttfb(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    200, &win_uid, duration_ms, ttfb_ms, &key_name,
                                    &state.pool.name_of(&win_uid),
                                    Some(&format!(
                                        "传输层掐断（未流出）→ 换号重试 [upstream: {}]",
                                        upstream_hdr
                                    )),
                                );
                                break; // 退出重试循环 → 换号
                            }
                            let kind = classify_solo_error(code, &msg);
                            if kind != ErrKind::None {
                                state.pool.note_error(&win_uid, kind);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} code={} msg={}", win_uid, code, msg));
                                // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.trae_sticky.unbind_uid(&win_uid);
                                }
                            }
                            // issue #67：流未开始（零内容流出）的账号级错误 → 换号重试
                            // （对齐空完成分支：错误帧未下发，重试流可在同一连接续传；
                            // ErrKind::None 属请求级错误，换号无意义，照旧延迟透传）
                            if !sent_any && kind != ErrKind::None {
                                state.logger.log_request_ttfb(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    200, &win_uid, duration_ms, ttfb_ms, &key_name,
                                    &state.pool.name_of(&win_uid),
                                    Some(&format!("{} → 换号重试", msg)),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&win_uid, &converted, None, 200, Some(&msg));
                                }
                                break; // 退出重试循环 → 换号
                            }
                            if !sent_any {
                                // 流未开始：错误延迟下发（sse 层未透传，由这里统一发）
                                send_stream_error(&tx, proto, code, &msg);
                            }
                            state.logger.log_request_ttfb(
                                "trae", "POST", proto.log_path(), &model, stream,
                                200, &win_uid, duration_ms, ttfb_ms, &key_name,
                                &state.pool.name_of(&win_uid), Some(&msg),
                            );
                            if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                state.logger.log_debug(&win_uid, &converted, None, 200, Some(&msg));
                            }
                        } else {
                            state.pool.note_success(&win_uid);
                            // 绑定会话→账号粘性（仅 clean success；save 1s 节流；
                            // 对冲接管时绑定生效账号，会话后续粘住实际服务者）
                            if sticky_usable {
                                state.trae_sticky.bind(&sticky_key, &win_uid, &sticky_conv, now_ts() as i64);
                                state.trae_sticky.save(&state.data_dir);
                            }
                            state.logger.log_request_ttfb(
                                "trae", "POST", proto.log_path(), &model, stream,
                                200, &win_uid, duration_ms, ttfb_ms, &key_name,
                                &state.pool.name_of(&win_uid), None,
                            );
                            if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                state.logger.log_debug(&win_uid, &converted, None, 200, None);
                            }
                        }
                        return; // 流式结束后直接返回
                    }
                    Err((status, resp_body, retry_after)) => {
                        // 11128 渠道风控（issue #57）：按请求指纹拦截、与账号/模型无关，
                        // 换号无意义。清洗未启用时强制清洗后同号重试一次；已清洗仍命中
                        // → 走下方 retry_plan 原样 Fatal（需更新 wb_template_map.json）
                        if status == 400 && super::retry::is_illegal_channel_error(&resp_body) {
                            if !sanitize {
                                sanitize = true;
                                // 激进口径贯穿（issue #122 缺口3）：置位后换号/对冲重建
                                // 仍全力清洗，不被循环顶重算丢弃
                                force_aggressive = true;
                                state.logger.log_sched_event(&format!(
                                    "11128 强制清洗重试{}", super::wb_payload::template_hit_note()
                                ));
                                // aggressive=true（issue #122 缺口3）：11128 是显式
                                // 指纹确认，强制清洗即全力清洗（激进规则一并生效）
                                converted = super::payload::prepare_llm_chat_body(
                                    &body_vec, &state.default_model, &picked.uid, &picked.device_id, &picked.machine_id,
                                    &super::models_sync::load_models(&state.data_dir),
                                    true, &templates,
                                    seed, true,
                                );
                                // 11128 强制清洗后别名改写同步重放（issue #150）：
                                // rebuild 以原始 body_vec 为基，逆映射按新表重算
                                tool_alias =
                                    super::wb_payload::apply_tool_alias_forward(&mut converted, &tool_alias_rules);
                                std::thread::sleep(std::time::Duration::from_millis(200));
                                // 断连检测：重试等待期间客户端离开则终止（审查 M5：补记失败终态用量）
                                if tx.is_closed() {
                                    state.record_usage(
                                        false, &model, &picked.uid, &key_id, false, stream,
                                        start_ts.elapsed().as_millis() as u64, 0, 0,
                                    );
                                    return;
                                }
                                continue;
                            }
                            // 清洗已启用仍命中（issue #99 诊断闭环）：落原始指纹样本
                            // + 按请求去重（换号轮换重复命中样本恒定）；Fatal 透传前
                            // 附排查指引（仅日志，客户端响应体不变）
                            let sample = super::wb_payload::fingerprint_sample(&body_vec);
                            if last_11128_sample.as_deref() != Some(sample.as_str()) {
                                last_11128_sample = Some(sample.clone());
                                let note = super::wb_payload::template_hit_note();
                                state.logger.log_sched_event(&format!(
                                    "11128 指纹样本: {}{}",
                                    sample, note
                                ));
                                // issue #125 增强：同步入内存样本环形缓冲（UI「指纹清洗」tab 一键生成规则）
                                super::wb_payload::record_fingerprint_sample(
                                    "11128",
                                    "trae",
                                    &sample,
                                    &note,
                                );
                                state.logger.log_sched_event(
                                    "11128 清洗已启用仍被拦截：模板需更新（UI「指纹清洗」tab 热更新 wb_template_map.json），指纹样本见上方日志",
                                );
                            }
                        }
                        // 分级重试策略表（T2.2/F-33 v1.2，与 wb_route 保持一致）
                        match retry_plan(status, &resp_body, same_attempt, retry_after) {
                            RetryAction::RetrySame { delay_ms } => {
                                // 同账号重试：不 note_error 不冷却
                                same_attempt += 1;
                                std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(60_000)));
                                // 断连检测：重试等待期间客户端离开则终止（审查 M5：补记失败终态用量）
                                if tx.is_closed() {
                                    state.record_usage(
                                        false, &model, &picked.uid, &key_id, false, stream,
                                        start_ts.elapsed().as_millis() as u64, 0, 0,
                                    );
                                    return;
                                }
                                continue;
                            }
                            RetryAction::SwitchKey => {
                                // Trae 401 自愈（issue #27 方案 B，对齐 wb_route T2.6）：
                                // 强制刷新一次凭证后同号重试（每请求每账号一次）；成功则
                                // 不冷却不换号。失败/无回调维持原「note_error + 换号」语义。
                                if status == 401
                                    && !refreshed_401.contains(&picked.uid)
                                    && state.trae_jwt_refresh.is_some()
                                {
                                    refreshed_401.insert(picked.uid.clone());
                                    let refresh = state.trae_jwt_refresh.as_ref().unwrap();
                                    match refresh(&picked.uid) {
                                        Ok(new_jwt) => {
                                            let clean = new_jwt
                                                .strip_prefix("Cloud-IDE-JWT ")
                                                .unwrap_or(&new_jwt)
                                                .trim()
                                                .to_string();
                                            state.pool.update_jwt(&picked.uid, &clean);
                                            picked.jwt = clean;
                                            same_attempt += 1;
                                            continue; // 同号重试（新 JWT）
                                        }
                                        Err(e) => {
                                            state.logger.log_request(
                                                "trae", "POST", proto.log_path(), &model, stream,
                                                status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                                &key_name, &state.pool.name_of(&picked.uid),
                                                Some(&format!("401 自愈刷新失败: {}", e)),
                                            );
                                        }
                                    }
                                }
                                let kind = classify_error(status, &resp_body);
                                state.pool.note_error(&picked.uid, kind);
                                // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.trae_sticky.unbind_uid(&picked.uid);
                                }
                                let preview = safe_slice(&resp_body, 200);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} status={} body={}", picked.uid, status, preview));
                                // 每请求终态单行（审查问题6）：SwitchKey → 换号是中间态，
                                // 不落账（对齐 wb 流式路径），由终局分支统一落行
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                    &key_name, &state.pool.name_of(&picked.uid),
                                    Some(&format!("upstream status={}", status)),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, Some(resp_body.as_bytes()), status, Some(&preview));
                                }
                                break; // 换号（same_attempt 随新账号归零）
                            }
                            RetryAction::Fatal => {
                                // 不冷却：请求本身问题（换号无意义），终止并透传上游错误体
                                let msg = format!("upstream {} error: {}", status, safe_slice(&resp_body, 300));
                                // 终态记账补齐（审查问题6，对齐 wb_route Fatal F3 口径）：
                                // Fatal 是请求终局，此前仅 log_request 导致用量统计页缺口
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0,
                                );
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                    &key_name, &state.pool.name_of(&picked.uid),
                                    Some(&msg),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, Some(resp_body.as_bytes()), status, Some(&msg));
                                }
                                send_stream_error(&tx, proto, status as i64, &msg);
                                return;
                            }
                        }
                    }
                }
            }
        }

        // 所有账号不可用（空完成/传输掐断主导 → 明示对应归因，否则维持原文案）
        let duration_ms = start_ts.elapsed().as_millis() as u64;
        state.record_usage(false, &model, "none", &key_id, false, true, duration_ms, 0, 0);
        let (healthy_total, _) = state.pool.selectable_stats_in(None, false);
        let (hard_credit_cnt, _) = state.pool.hard_credit_stats();
        let (exhaust_msg, exhaust_tag) =
            exhaust_message_with_transport_cut(
                empty_hits, tried.len(), healthy_total, hard_credit_cnt, cut_hits,
            );
        // 池级指纹拦截短记忆（issue #122 缺口2）：空完成主导（指纹拦截）才标记，
        // TTL 内同池新请求直接激进起步，不再从零试探烧账号
        super::wb_payload::mark_channel_fp_block("trae", exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK");
        let diag = state.pool.diagnose();
        let diag_summary: Vec<String> = diag
            .iter()
            .map(|d| {
                let credits_str = d.credits.map(|c| format!("{:.0}", c)).unwrap_or_else(|| "N/A".to_string());
                let cd_str = if d.until > 0 { format!(",cd={}s", d.until.saturating_sub(now_ts() as i64)) } else { String::new() };
                let exp_str = d.credits_expire_at.filter(|&e| e > 0).map(|e| format!(",exp={}", e)).unwrap_or_default();
                let dis_str = if d.disabled { ",DIS" } else { "" };
                format!("{}({}:{},cr={}{}{}{})", d.name, d.uid.get(..8).unwrap_or(&d.uid), d.reason, credits_str, cd_str, exp_str, dis_str)
            })
            .collect();
        state.logger.log_request(
            "trae", "POST", proto.log_path(), &model, stream,
            503, "none", duration_ms, &key_name, "",
            Some(&exhaust_msg),
        );
        // 写入 app.log 供排查
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let local_ts = now + 8 * 3600;
            let h = (local_ts % 86400) / 3600;
            let m = (local_ts % 3600) / 60;
            let s = local_ts % 60;
            // issue #99 ① + #125 增强：影子风控收尾采样入内存环形缓冲（kind=empty），
            // 并返回日志注记供反查漏网触发句
            let sample_note = super::wb_payload::empty_sample_note(&exhaust_tag, "trae", &body_vec);
            let diag_line = format!(
                "{exhaust_tag} [{:02}:{:02}:{:02}] tried={} empty_hits={} cut_hits={} pool=trae reasons=[{}]{}",
                h, m, s, tried.len(), empty_hits, cut_hits, diag_summary.join(", "), sample_note,
            );
            state.logger.log_debug_line(format!("[DEBUG] {diag_line}"));
        }
        match proto {
            Protocol::OpenAi | Protocol::OpenAiText => {
                let err_body = json!({
                    "error": {"message": exhaust_msg, "type": "api_error", "code": "no_healthy_account"}
                });
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                    "data: {}\n\n",
                    err_body
                ))));
                let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
            }
            Protocol::Anthropic => {
                let err = json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": exhaust_msg,
                    },
                });
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                    "event: error\ndata: {}\n\n",
                    err
                ))));
            }
            Protocol::Responses => {
                let body = json!({
                    "type": "response.failed",
                    "response": {
                        "id": format!("resp_{}", now_ts()),
                        "object": "response",
                        "status": "failed",
                        "output": [],
                        "error": {"code": "no_healthy_account", "message": exhaust_msg},
                    },
                });
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                    "event: response.failed\ndata: {}\n\n",
                    body
                ))));
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("internal server error"))
                .unwrap()
        })
}

// ==================== Non-streaming ====================

async fn aggregate_chat(state: Arc<ApiSharedState>, body_vec: Vec<u8>, model: String, stream: bool, start_ts: std::time::Instant, proto: Protocol, key_id: String, guard: InflightGuard) -> Response {
    // P2 修复：聚合含分级重试（RetrySame 退避 std::thread::sleep 最长 60s×N），
    // 长阻塞占主池会饿死鉴权等短任务，迁入 stream_runtime 专用阻塞池。
    // Key 绑定 trae 的约束在闭包外解析（clone 进闭包，锁外取号）
    let (trae_allowed, trae_dedicated) = trae_pool_constraints(&state, &key_id);
    let result = super::stream_runtime().spawn_blocking(move || {
        // inflight guard 随后台任务存续至聚合完成（§4.5）；F-77 取号后绑定账号级计数
        let mut guard = guard;
        // 请求日志附带的 API Key 展示名（匿名/未知 → 空串，日志显示 "-"）
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        // 会话→账号粘性（per-pool 三参数批次，与流式路径同构）：显式
        // conversationId / 消息指纹命中且账号健康 → 首选粘住账号；子 Key 限定
        // 上游不含粘性账号时忽略粘性
        // issue #138 P0：sticky 键窄探测——不再整包解析 DOM，仅提取会话键所需
        // 字段（conversation_id + 前 3 条消息），大 body 下省 3~6 倍解析放大
        let sticky_key = SessionKey::from_slice(&body_vec);
        // 空指纹（无 messages / 无 conversation_id）不可粘
        let sticky_usable = !sticky_key.cache_key().ends_with(':');
        let sticky0: Option<String> = if sticky_usable {
            state
                .trae_sticky
                .resolve(&sticky_key, now_ts() as i64)
                .and_then(|b| {
                    if trae_allowed.as_ref().is_some_and(|a| !a.contains(&b.uid)) {
                        return None;
                    }
                    state.pool.pick_by_uid(&b.uid, false).map(|_| b.uid)
                })
        } else {
            None
        };
        // 首选：粘性 > 调度策略；绑定回写用会话键占位 conv_id（仅落库留档）
        let sticky_conv = sticky_key.cache_key();
        let mut first_pick: Option<super::pool::PickedAccount> = sticky0.as_ref().and_then(|u| {
            state
                .pool
                .pick_sticky_yield(u, trae_allowed.as_ref(), false)
                .map(|(p, ev)| {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                })
        });
        let mut tried = HashSet::new();
        // 401 自愈去重（issue #27 方案 B）：每账号每请求最多强制刷新一次（对齐 wb_route T2.6）
        let mut refreshed_401 = HashSet::new();
        // 空完成计数（issue #57 后续可观测）：轮换耗尽收尾时区分「渠道指纹拦截」与「账号池耗尽」
        let mut empty_hits: usize = 0;
        // 传输掐断计数：与空完成分开归因，耗尽时用于修正 "no healthy account" 文案
        let mut cut_hits: usize = 0;
        // 11128 诊断样本按请求去重（换号轮换重复命中不重复打样）
        let mut last_11128_sample: Option<String> = None;
        // 指纹清洗（issue #57）：请求级快照（对齐 wb_route）——11128 强制开启后
        // 跨账号保持，换号不再以未清洗状态重烧一次拦截；热更新开关下请求生效
        let mut sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);
        // 11128 显式指纹确认置位（issue #122 缺口3 审查补口）：强制清洗 rebuild 后
        // 同号重试直接消费 rebuild 结果（内层循环不重算 converted），但换号到下一
        // 账号 / 触发对冲竞速时循环顶会以 empty_hits>0 || fp_block_recent 重算激进
        // 位——纯 11128 首拦两条件均假，激进意图被丢弃且主/对冲清洗强度不一致。
        // 此处置位后循环顶与对冲重建统一沿用全力清洗
        let mut force_aggressive = false;
        let templates = super::wb_route::load_templates(&super::wb_route::WbView::wb(state.clone()));
        // 工具别名表（issue #150）：sanitize 门控下请求侧正向改写 + 响应侧逆映射
        // 回改（客户端按名匹配 tool_calls，单向改名即废）；表热更新、保存即生效
        let tool_alias_rules =
            super::wb_route::load_tool_alias_rules(&super::wb_route::WbView::wb(state.clone()));
        let mut last_residual_note: Option<String> = None; // 残指纹自检按请求去重（issue #122 C3）
        // 多候选改写种子基底（issue #122 方案 C1）：会话键 + 请求时间戳绑定请求，
        // tried.len() 随换号轮换递增——跨重试候选必变
        let seed_base = format!("{}#{}", sticky_conv, now_ts());

        for _ in 0..MAX_ROTATE {
            // 取号：粘性命中优先（每请求仅首轮），否则按 Key 约束 + 调度策略；
            // 换号重试后仅走策略
            let mut picked = match first_pick.take() {
                Some(p) => p,
                None => match state
                    .pool
                    .pick_excluding_constrained(&tried, trae_allowed.as_ref(), trae_dedicated.as_deref())
                {
                    Some(p) => p,
                    None => break,
                },
            };
            tried.insert(picked.uid.clone());
            // F-77 账号级在途计数：取号即绑定（换号时自动解绑旧账号）。
            // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
            // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（专一/粘性
            // 锁定、busy_fallback 降级）保留原无条件绑定语义。
            // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
            let bound = if picked.over_limit {
                guard = guard.bind_account(state.pool.inflight_handle(&picked.uid));
                true
            } else {
                guard.bind_account_cas(
                    state.pool.inflight_handle(&picked.uid),
                    state.pool.concurrency_limit(),
                )
            };
            if !bound {
                continue;
            }
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            // 多候选改写种子（issue #122 方案 C1）：tried.len() 随换号轮换递增；
            // 空完成后（empty_hits>0）、池级指纹拦截 TTL 内（issue #122 缺口2）
            // 或 11128 强制清洗置位（force_aggressive）时追加激进清洗（方案 C2）
            let seed = super::wb_payload::variant_seed(&seed_base, tried.len() as u64);
            let mut converted = super::payload::prepare_llm_chat_body(
                &body_vec, &state.default_model, &picked.uid, &picked.device_id, &picked.machine_id,
                // 模型目录（config_cache 缓存，热路径）：function 查表优先
                &super::models_sync::load_models(&state.data_dir),
                sanitize, &templates,
                seed, empty_hits > 0 || super::wb_payload::fp_block_recent("trae") || force_aggressive,
            );
            // 工具别名正向改写（issue #150）：sanitize 门控；逆映射供响应侧回改
            //（覆盖 tools/tool_choice/历史 tool_calls 三个透传点，零命中 None 直通）
            let mut tool_alias: Option<std::collections::HashMap<String, String>> = None;
            if sanitize {
                tool_alias =
                    super::wb_payload::apply_tool_alias_forward(&mut converted, &tool_alias_rules);
            }
            // 残指纹自检（issue #122 方案 C3）：改写后首条 system 仍命中已知特征词
            // → debug 日志（按请求去重），提示模板表需经 UI「指纹清洗」tab 热更新
            if sanitize {
                if let Some(note) = super::wb_payload::residual_fingerprint_note(&converted) {
                    if last_residual_note.as_deref() != Some(note.as_str()) {
                        last_residual_note = Some(note.clone());
                        state.logger.log_debug_line(format!(
                            "[DEBUG] 残指纹自检命中: {note} pool=trae uid={}",
                            picked.uid
                        ));
                    }
                }
            }

            // 分级重试（T2.2/F-33，与 wb_route 同一张表）：same_attempt 为同账号
            // 重试计数，换号后随新账号归零；总轮换上限仍受 MAX_ROTATE 约束
            let mut same_attempt: u32 = 0;
            loop {
                match make_upstream_request(&picked.jwt, &picked.uid, &picked.device_id, &picked.machine_id, &converted) {
                    Ok((reader, primary_hdr)) => {
                        let chat_id = match proto {
                            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
                            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
                            Protocol::Anthropic => format!("msg_{}", now_ts()),
                            Protocol::Responses => format!("resp_{}", now_ts()),
                        };
                        let (resp, error_info) = match proto {
                            Protocol::OpenAi => sse::aggregate(reader, &chat_id, tool_alias.as_ref()),
                            Protocol::OpenAiText => sse::aggregate_text(reader, &chat_id, &model),
                            Protocol::Anthropic => {
                                sse::aggregate_anthropic(reader, &chat_id, &model, tool_alias.as_ref())
                            }
                            // Responses 仅走 WB 上游；solo 管线兜底按 OpenAI 聚合
                            Protocol::Responses => sse::aggregate(reader, &chat_id, tool_alias.as_ref()),
                        };
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        match (resp, error_info) {
                            (Some(r), None) => {
                                if super::aggregated_response_is_empty(&r) {
                                    // 空完成（影子风控/上游异常，issue #57）：换号重试，
                                    // 口径同下方「empty response」（Server 级短冷却）
                                    empty_hits += 1;
                                    state.pool.note_error(&picked.uid, ErrKind::Server);
                                    // 每请求终态单行（审查问题6）：换号是中间态不落账
                                    // （终局由后续成功行/耗尽 none 行统一落）
                                    state.logger.log_request(
                                        "trae", "POST", proto.log_path(), &model, stream,
                                        502, &picked.uid, duration_ms, &key_name,
                                        &state.pool.name_of(&picked.uid),
                                        Some(&format!(
                                            "空完成 → 换号重试{} [upstream: {}]",
                                            super::wb_payload::template_hit_note(),
                                            primary_hdr
                                        )),
                                    );
                                    break; // 换号
                                }
                                // 用量记账（成功：token 数从聚合响应 usage 提取）
                                let (pt, ct) = r.get("usage").map(extract_tokens).unwrap_or((0, 0));
                                // M4 审查修复：缓存读 tokens 旁路入账（未下发得 0 不入账）
                                state.record_usage_cached(
                                    super::usage::UsageBucket::Trae,
                                    r.get("usage").map(super::wb_sse::cached_tokens_of).unwrap_or(0),
                                );
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, true, stream,
                                    duration_ms, pt, ct,
                                );
                                state.pool.note_success(&picked.uid);
                                // 绑定会话→账号粘性（仅 clean success；save 1s 节流）
                                if sticky_usable {
                                    state.trae_sticky.bind(&sticky_key, &picked.uid, &sticky_conv, now_ts() as i64);
                                    state.trae_sticky.save(&state.data_dir);
                                }
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    200, &picked.uid, duration_ms, &key_name,
                                    &state.pool.name_of(&picked.uid), None,
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, Some(r.to_string().as_bytes()), 200, None);
                                }
                                return Ok(r);
                            }
                            (None, Some((code, msg))) => {
                                // 传输层掐断预检（issue #158 报错A，P2-1 聚合侧补齐，
                                // 须在 classify_solo_error 之前——与流式侧同口径）：
                                // 账号无关传输故障，不冷却、不解绑粘性、不计空完成指纹；
                                // 非流式未向客户端发出任何内容，直接丢弃截断响应 break 换号
                                //（截断的部分响应可能含残缺 tool_calls，不得当成功交付）。
                                // PR #162：cut_hits 计入轮换耗尽归因——全池被掐时
                                // exhaust_message_with_transport_cut 报传输掐断而非
                                // "no healthy account available"；日志 502 对齐
                                // empty response 分支（本次尝试未向客户端交付）
                                if super::is_transport_cut(code, &msg) {
                                    cut_hits += 1;
                                    state.logger.log_request(
                                        "trae", "POST", proto.log_path(), &model, stream,
                                        502, &picked.uid, duration_ms, &key_name,
                                        &state.pool.name_of(&picked.uid),
                                        Some(&format!(
                                            "传输层掐断（聚合响应已丢弃）→ 换号重试：{} [upstream: {}]",
                                            safe_slice(&msg, 160),
                                            primary_hdr
                                        )),
                                    );
                                    if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                        state.logger.log_debug(&picked.uid, &converted, None, 502, Some(&msg));
                                    }
                                    break; // 换号，不冷却、不计空完成指纹
                                }
                                // 空完成哨兵（-9901，影子风控）计入指纹拦截观测；
                                // 冷却行为维持现状（classify_solo_error → Server）
                                if super::is_empty_completion(code, &msg) {
                                    empty_hits += 1;
                                }
                                let kind = classify_solo_error(code, &msg);
                                // 请求级错误（ErrKind::None）不计账号错误——与流式路径
                                // 守卫一致；误计会在连续 3 次后熔断健康账号
                                if kind != ErrKind::None {
                                    state.pool.note_error(&picked.uid, kind);
                                    // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                    if kind.unbinds_sticky() {
                                        state.trae_sticky.unbind_uid(&picked.uid);
                                    }
                                }
                                *safe_lock(&state.last_error) = Some(format!(
                                    "uid={} code={} msg={}",
                                    picked.uid,
                                    code,
                                    safe_slice(&msg, 300)
                                ));
                                // 每请求终态单行（审查问题6）：账号级错误换号是中间态
                                // 不落账（终局由后续成功行/耗尽 none 行统一落）；请求级
                                // 错误（kind==None）为终态透传，恰在此落一行
                                if kind == ErrKind::None {
                                    state.record_usage(
                                        false, &model, &picked.uid, &key_id, false, stream,
                                        duration_ms, 0, 0,
                                    );
                                }
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    200, &picked.uid, duration_ms, &key_name,
                                    &state.pool.name_of(&picked.uid), Some(&msg),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, None, 200, Some(&msg));
                                }
                                if kind == ErrKind::None {
                                    // issue #38-2/T5：请求级错误（4001 param invalid /
                                    // model config mismatch）与账号无关——换任何账号都会
                                    // 复现同类错误，轮换只会打满整池后报 503（issue 实测
                                    // 双账号被连打）。终止轮换，按 400 透传上游错误
                                    //（不记 note_error，账号健康度不受影响）
                                    return Err(AggregateFail::Upstream(
                                        400,
                                        format!("upstream code={} msg={}", code, safe_slice(&msg, 300)),
                                    ));
                                }
                                break; // 账号级错误：冷却换号
                            }
                            _ => {
                                state.pool.note_error(&picked.uid, ErrKind::Server);
                                // 每请求终态单行（审查问题6）：换号是中间态不落账
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    502, &picked.uid, duration_ms, &key_name,
                                    &state.pool.name_of(&picked.uid), Some("empty response"),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, None, 502, Some("empty response"));
                                }
                                break; // 换号
                            }
                        }
                    }
                    Err((status, resp_body, retry_after)) => {
                        // 11128 渠道风控（issue #57）：清洗未启用时强制清洗后同号重试一次；
                        // 已清洗仍命中 → 走下方 retry_plan 原样 Fatal
                        if status == 400 && super::retry::is_illegal_channel_error(&resp_body) {
                            if !sanitize {
                                sanitize = true;
                                // 激进口径贯穿（issue #122 缺口3）：置位后换号重建
                                // 仍全力清洗，不被循环顶重算丢弃
                                force_aggressive = true;
                                state.logger.log_sched_event(&format!(
                                    "11128 强制清洗重试{}", super::wb_payload::template_hit_note()
                                ));
                                // aggressive=true（issue #122 缺口3）：11128 是显式
                                // 指纹确认，强制清洗即全力清洗（激进规则一并生效）
                                converted = super::payload::prepare_llm_chat_body(
                                    &body_vec, &state.default_model, &picked.uid, &picked.device_id, &picked.machine_id,
                                    &super::models_sync::load_models(&state.data_dir),
                                    true, &templates,
                                    seed, true,
                                );
                                // 11128 强制清洗后别名改写同步重放（issue #150）：
                                // rebuild 以原始 body_vec 为基，逆映射按新表重算
                                tool_alias =
                                    super::wb_payload::apply_tool_alias_forward(&mut converted, &tool_alias_rules);
                                std::thread::sleep(std::time::Duration::from_millis(200));
                                continue;
                            }
                            // 清洗已启用仍命中（issue #99 诊断闭环）：落原始指纹样本
                            // + 按请求去重（换号轮换重复命中样本恒定）；Fatal 透传前
                            // 附排查指引（仅日志，客户端响应体不变）
                            let sample = super::wb_payload::fingerprint_sample(&body_vec);
                            if last_11128_sample.as_deref() != Some(sample.as_str()) {
                                last_11128_sample = Some(sample.clone());
                                let note = super::wb_payload::template_hit_note();
                                state.logger.log_sched_event(&format!(
                                    "11128 指纹样本: {}{}",
                                    sample,
                                    note
                                ));
                                // issue #125 增强：同步入内存样本环形缓冲（UI「指纹清洗」tab 一键生成规则）
                                super::wb_payload::record_fingerprint_sample("11128", "trae", &sample, &note);
                                state.logger.log_sched_event(
                                    "11128 清洗已启用仍被拦截：模板需更新（UI「指纹清洗」tab 热更新 wb_template_map.json），指纹样本见上方日志",
                                );
                            }
                        }
                        // 分级重试策略表（T2.2/F-33 v1.2，与 wb_route 保持一致）
                        match retry_plan(status, &resp_body, same_attempt, retry_after) {
                            RetryAction::RetrySame { delay_ms } => {
                                // 同账号重试：不 note_error 不冷却
                                same_attempt += 1;
                                std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(60_000)));
                                continue;
                            }
                            RetryAction::SwitchKey => {
                                // Trae 401 自愈（issue #27 方案 B，对齐 wb_route T2.6）：
                                // 强制刷新一次凭证后同号重试（每请求每账号一次）；成功则
                                // 不冷却不换号。失败/无回调维持原「note_error + 换号」语义。
                                if status == 401
                                    && !refreshed_401.contains(&picked.uid)
                                    && state.trae_jwt_refresh.is_some()
                                {
                                    refreshed_401.insert(picked.uid.clone());
                                    let refresh = state.trae_jwt_refresh.as_ref().unwrap();
                                    match refresh(&picked.uid) {
                                        Ok(new_jwt) => {
                                            let clean = new_jwt
                                                .strip_prefix("Cloud-IDE-JWT ")
                                                .unwrap_or(&new_jwt)
                                                .trim()
                                                .to_string();
                                            state.pool.update_jwt(&picked.uid, &clean);
                                            picked.jwt = clean;
                                            same_attempt += 1;
                                            continue; // 同号重试（新 JWT）
                                        }
                                        Err(e) => {
                                            state.logger.log_request(
                                                "trae", "POST", proto.log_path(), &model, stream,
                                                status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                                &key_name, &state.pool.name_of(&picked.uid),
                                                Some(&format!("401 自愈刷新失败: {}", e)),
                                            );
                                        }
                                    }
                                }
                                let kind = classify_error(status, &resp_body);
                                state.pool.note_error(&picked.uid, kind);
                                // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.trae_sticky.unbind_uid(&picked.uid);
                                }
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} status={}", picked.uid, status));
                                // 每请求终态单行（审查问题6）：SwitchKey → 换号是中间态，
                                // 不落账（对齐流式路径），由终局分支统一落行
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                    &key_name, &state.pool.name_of(&picked.uid),
                                    Some(&format!("upstream status={}", status)),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, Some(resp_body.as_bytes()), status, Some(&resp_body));
                                }
                                break; // 换号（same_attempt 随新账号归零）
                            }
                            RetryAction::Fatal => {
                                // 不冷却：请求本身问题（换号无意义），终止并透传上游错误体
                                let msg = format!("upstream {} error: {}", status, safe_slice(&resp_body, 300));
                                // 终态记账补齐（审查问题6，对齐流式路径/wb/qoder Fatal 口径）：
                                // Fatal 是请求终局，此前仅 log_request 导致用量统计页缺口
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0,
                                );
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                    &key_name, &state.pool.name_of(&picked.uid),
                                    Some(&msg),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, Some(resp_body.as_bytes()), status, Some(&msg));
                                }
                                return Err(AggregateFail::Upstream(status, msg));
                            }
                        }
                    }
                }
            }
        }

        let duration_ms = start_ts.elapsed().as_millis() as u64;
        // 轮换耗尽收尾：空完成/传输掐断主导 → 明示对应归因，否则维持原文案
        let (healthy_total, _) = state.pool.selectable_stats_in(None, false);
        let (hard_credit_cnt, _) = state.pool.hard_credit_stats();
        let (exhaust_msg, exhaust_tag) =
            exhaust_message_with_transport_cut(
                empty_hits, tried.len(), healthy_total, hard_credit_cnt, cut_hits,
            );
        // 池级指纹拦截短记忆（issue #122 缺口2）：空完成主导（指纹拦截）才标记，
        // TTL 内同池新请求直接激进起步，不再从零试探烧账号
        super::wb_payload::mark_channel_fp_block("trae", exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK");
        let diag = state.pool.diagnose();
        // 用量记账（所有账号不可用）
        state.record_usage(false, &model, "none", &key_id, false, stream, duration_ms, 0, 0);
        let diag_summary: Vec<String> = diag
            .iter()
            .map(|d| {
                let credits_str = d.credits.map(|c| format!("{:.0}", c)).unwrap_or_else(|| "N/A".to_string());
                let cd_str = if d.until > 0 { format!(",cd={}s", d.until.saturating_sub(now_ts() as i64)) } else { String::new() };
                let exp_str = d.credits_expire_at.filter(|&e| e > 0).map(|e| format!(",exp={}", e)).unwrap_or_default();
                let dis_str = if d.disabled { ",DIS" } else { "" };
                format!("{}({}:{},cr={}{}{}{})", d.name, d.uid.get(..8).unwrap_or(&d.uid), d.reason, credits_str, cd_str, exp_str, dis_str)
            })
            .collect();
        state.logger.log_request(
            "trae", "POST", proto.log_path(), &model, stream,
            503, "none", duration_ms, &key_name, "",
            Some(&exhaust_msg),
        );
        // 写入诊断日志
        {
            // issue #99 ① + #125 增强：影子风控收尾采样入内存环形缓冲（kind=empty），
            // 并返回日志注记供反查漏网触发句
            let sample_note = super::wb_payload::empty_sample_note(&exhaust_tag, "trae", &body_vec);
            state.logger.log_debug_line(format!(
                "[DEBUG] {exhaust_tag}(non-stream) tried={} empty_hits={} cut_hits={} pool=trae reasons=[{}]{}",
                tried.len(), empty_hits, cut_hits, diag_summary.join(", "), sample_note,
            ));
        }
        Err(AggregateFail::NoHealthy(exhaust_msg))
    })
    .await;

    match result {
        Ok(Ok(resp)) => Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(resp.to_string()))
            .unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("internal server error"))
                    .unwrap()
            }),
        Ok(Err(AggregateFail::NoHealthy(msg))) => match proto {
            Protocol::OpenAi | Protocol::OpenAiText | Protocol::Responses => {
                openai_error(StatusCode::SERVICE_UNAVAILABLE, "no_healthy_account", &msg)
            }
            Protocol::Anthropic => anthropic_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", &msg),
        },
        Ok(Err(AggregateFail::Upstream(status, msg))) => {
            // Fatal：上游错误体透传（不冷却），按协议格式化并保留上游状态码
            let sc = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            match proto {
                Protocol::Anthropic => anthropic_error(sc, "api_error", &msg),
                _ => openai_error(sc, "upstream_error", &msg),
            }
        }
        Err(e) => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("task join error: {}", e),
        ),
    }
}

// ==================== F-76③ 慢请求竞速对冲（Trae 池，wb_route/qoder_route 同构） ====================

/// 对冲账号在途计数租约（镜像 wb_route::HedgeLease，P0 泄漏修复）：
/// 构造即 +1（竞速窗口占用），Drop 即 -1——建连失败（闭包内 `?` 提前返回）、
/// 双败（`lines_with_first_byte_hedged` Err 路径内 Drop）、竞速胜出（随
/// `RaceOutcome.hedge` 移交调用方，`settle_trae_hedge` 落定后 Drop）三类
/// 退出路径均恰好释放一次，杜绝计数泄漏导致的账号永久 busy
struct TraeHedgeLease {
    uid: String,
    counter: Arc<std::sync::atomic::AtomicU32>,
    /// 对冲请求的上游响应头摘要（空 = 建连后未捕获）；对冲接管时用于空完成取证
    upstream_hdr: String,
}

impl TraeHedgeLease {
    fn acquire(state: &ApiSharedState, uid: &str) -> Self {
        let counter = state.pool.inflight_handle(uid);
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { uid: uid.to_string(), counter, upstream_hdr: String::new() }
    }
}

impl Drop for TraeHedgeLease {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 首字竞速胜者信息（镜像 wb_route::RaceWin）：生效账号 + 对冲计数租约
struct TraeRaceWin {
    lines: Box<dyn Iterator<Item = String> + Send>,
    /// 生效账号 uid（对冲接管时为对冲账号）
    uid: String,
    /// 对冲侧计数租约（None = 未触发对冲；Some = 竞速已定，待 settle 释放）
    hedge: Option<TraeHedgeLease>,
    /// 对冲接管（对冲请求先出首字）
    takeover: bool,
    /// 生效请求的上游响应头摘要：主请求胜出 → 主请求摘要；对冲接管 → 对冲侧
    /// 摘要（未捕获时降级保留主请求摘要）。空完成日志取证用
    upstream_hdr: String,
}

/// 首字竞速（Trae 池，镜像 wb_route::race_first_byte）：对冲关闭（阈值 0）
/// 时与纯首字超时语义完全一致；开启时主请求首字节超阈值 → 从池内取第二账号
/// （走同一 busy 过滤/负载因子——主账号已 inflight 天然让位）发对冲请求，
/// 先出首字者胜。对冲请求体按对冲账号指纹重建（device/machine id 逐账号
/// 注入，不能复用主请求体）。`seed`/`aggressive`（issue #122 方案 C1/C2）：
/// 对冲侧种子做黄金比例翻搅——候选与主请求不同，避免主/对冲同指纹改写；
/// aggressive 沿用主请求口径
#[allow(clippy::too_many_arguments)]
fn race_trae_first_byte(
    state: &Arc<ApiSharedState>,
    primary_uid: &str,
    reader: Box<dyn Read + Send>,
    primary_hdr: String,
    tried: &HashSet<String>,
    allowed: Option<&HashSet<String>>,
    dedicated: Option<&str>,
    body_vec: &[u8],
    sanitize: bool,
    templates: &[(String, String)],
    tool_alias_rules: &[(String, String)],
    seed: u64,
    aggressive: bool,
) -> Result<TraeRaceWin, ()> {
    let hedge_ms = state
        .trae_hedge_threshold_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    // issue #138 P1：大 body 跳过首字对冲——对冲请求体需按账号整包重建
    // （device/machine id 逐账号注入），大包下内存瞬时翻倍，得不偿失
    if hedge_ms == 0 || body_vec.len() > HEDGE_SKIP_BODY_BYTES {
        let lines = super::wb_upstream::lines_with_first_byte_timeout(reader)?;
        return Ok(TraeRaceWin {
            lines,
            uid: primary_uid.to_string(),
            hedge: None,
            takeover: false,
            upstream_hdr: primary_hdr,
        });
    }
    let state2 = state.clone();
    let tried2 = tried.clone();
    let allowed2 = allowed.cloned();
    let dedicated2 = dedicated.map(str::to_string);
    let body = body_vec.to_vec();
    let templates2 = templates.to_vec();
    // 对冲侧种子：黄金比例翻搅（候选与主请求不同，issue #122 方案 C1）
    let hedge_seed = seed ^ 0x9E37_79B9_7F4A_7C15;
    let spawn_backup = move || -> Option<(Box<dyn Read + Send>, TraeHedgeLease)> {
        let (picked2, ev) = state2
            .pool
            .pick_excluding_constrained_ev(&tried2, allowed2.as_ref(), dedicated2.as_deref(), false)?;
        // F-77⑤ 可观测：对冲取号同样记录 busy 让位/降级事件
        if let Some(ev) = ev {
            state2.logger.log_sched_event(&ev);
        }
        // 租约先于建连获取：建连失败（下行 `?`）时随闭包局部变量 Drop 自动 -1
        let mut lease = TraeHedgeLease::acquire(&state2, &picked2.uid);
        // 对冲请求体按对冲账号指纹重建
        let mut converted2 = super::payload::prepare_llm_chat_body(
            &body,
            &state2.default_model,
            &picked2.uid,
            &picked2.device_id,
            &picked2.machine_id,
            &super::models_sync::load_models(&state2.data_dir),
            sanitize,
            &templates2,
            hedge_seed,
            aggressive,
        );
        // 工具别名正向改写（issue #150）：与主请求同规则表——别名集跨主/对冲
        // 一致（表不依赖种子，见 wb_payload::merged_tool_alias_rules 注记），
        // 任一侧胜出，响应侧逆映射（stream_chat 持有）均能正确回改
        if sanitize {
            let _ = super::wb_payload::apply_tool_alias_forward(&mut converted2, tool_alias_rules);
        }
        let (reader2, hdr2) = make_upstream_request(
            &picked2.jwt,
            &picked2.uid,
            &picked2.device_id,
            &picked2.machine_id,
            &converted2,
        )
        .ok()?;
        lease.upstream_hdr = hdr2;
        Some((reader2, lease))
    };
    match super::wb_upstream::lines_with_first_byte_hedged(reader, hedge_ms, spawn_backup) {
        Ok(out) => {
            let uid = if out.takeover {
                out.hedge
                    .as_ref()
                    .map(|l| l.uid.clone())
                    .unwrap_or_else(|| primary_uid.to_string())
            } else {
                primary_uid.to_string()
            };
            // 生效侧响应头摘要：主请求胜出 → 主请求摘要；对冲接管 → 对冲侧摘要
            //（对冲侧未捕获到时降级保留主请求摘要，仍可凭主请求凭据追溯/申诉）
            let mut upstream_hdr = primary_hdr;
            if out.takeover {
                if let Some(l) = out.hedge.as_ref() {
                    if !l.upstream_hdr.is_empty() {
                        upstream_hdr = l.upstream_hdr.clone();
                    }
                }
            }
            Ok(TraeRaceWin {
                lines: out.lines,
                uid,
                hedge: out.hedge,
                takeover: out.takeover,
                upstream_hdr,
            })
        }
        Err(()) => Err(()),
    }
}

/// 竞速结束后处理对冲计数与日志（镜像 wb_route::settle_hedge）：
/// 释放对冲账号竞速窗口占用；接管时 guard 重绑到对冲账号；[SCHED] 日志记录
/// hedge_takeover / hedge_lost
fn settle_trae_hedge(
    state: &ApiSharedState,
    win: &mut TraeRaceWin,
    mut guard: InflightGuard,
    primary_uid: &str,
) -> InflightGuard {
    let Some(lease) = win.hedge.take() else {
        return guard;
    };
    let hedge_uid = lease.uid.as_str();
    if win.takeover {
        state
            .logger
            .log_sched_event(&format!("hedge_takeover primary={} hedge={}", primary_uid, hedge_uid));
        guard = guard.bind_account(lease.counter.clone());
    } else {
        state
            .logger
            .log_sched_event(&format!("hedge_lost primary={} hedge={}", primary_uid, hedge_uid));
    }
    drop(lease); // 释放竞速窗口占用（接管路径先重绑流计数再释放，语义与原实现一致）
    guard
}

// ==================== Upstream Request ====================

/// 上游错误体读取上限（P2 修复11）：防止上游异常回包把整响应读进内存
const MAX_UPSTREAM_ERR_BYTES: usize = 64 * 1024;

/// 限量读取上游错误体（P2 修复11）：最多 MAX_UPSTREAM_ERR_BYTES，读满截断
fn read_limited_body(response: ureq::Response) -> String {
    let mut buf = Vec::new();
    let mut limited = response
        .into_reader()
        .take(MAX_UPSTREAM_ERR_BYTES as u64);
    let _ = limited.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// 空完成取证（issue #57）：上游 HTTP 200 零内容时，把响应侧可追溯标识
/// （logid / 网关请求 id / trace 类头）与我方发出的 trace/request-id 拼成一行
/// 紧凑摘要，追加进空完成日志，为向上游申诉或自查提供凭据。
/// 仅收录存在的头；值按字符截断到 96 防日志膨胀。
fn upstream_header_digest(r: &ureq::Response, sent_trace: &str, sent_req_id: &str) -> String {
    const CANDIDATES: [&str; 7] = [
        "logid",
        "x-tt-logid",
        "x-lgw-request-id",
        "x-lgw-trace-id",
        "x-request-id",
        "x-tt-envflags",
        "server-timing",
    ];
    let mut parts: Vec<String> = Vec::new();
    for name in CANDIDATES {
        if let Some(v) = r.header(name) {
            let v = v.trim();
            if !v.is_empty() {
                parts.push(format!("{}={}", name, v.chars().take(96).collect::<String>()));
            }
        }
    }
    parts.push(format!("sent_trace={}", sent_trace));
    parts.push(format!("sent_req={}", sent_req_id));
    parts.join("; ")
}

fn make_upstream_request(
    jwt: &str,
    _uid: &str,
    device_id: &str,
    machine_id: &str,
    body: &[u8],
) -> Result<(Box<dyn Read + Send>, String), (u16, String, Option<u64>)> {
    let url = format!("{}{}", AGENT_HOST, EP_LLM_CHAT);
    let referer = format!("{}{}", REFERER_BASE, EP_LLM_CHAT);
    let trace_id = format!(
        "00-{}-{}-01",
        uuid_like_id(),
        uuid_like_id()
    );
    let request_id = format!("req_{}", uuid_like_id());

    let resp = streaming_agent()
        .post(&url)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .set("accept-encoding", "gzip, deflate, br, zstd")
        .set("user-agent", "TraeClient/TTNet")
        .set("x-ide-token", jwt)
        .set("x-app-id", APP_ID)
        .set("x-app-version", "default")
        .set("x-app-version-code", IDE_VERSION_CODE)
        .set("x-ide-version", IDE_VERSION)
        .set("x-ide-version-code", IDE_VERSION_CODE)
        .set("x-ide-version-type", "stable")
        .set("x-device-type", "windows")
        .set("x-device-brand", "CREFG-XX")
        .set("x-device-cpu", "Intel")
        .set("x-device-id", device_id)
        .set("x-machine-id", machine_id)
        .set("x-os-version", "Windows 11 Home China")
        .set("request-traffic-type", "prod")
        .set("package-type", "stable_cn")
        .set("x-lgw-req-sdk-type", "3")
        .set("x-lscbd-aid", "787976")
        .set("x-lscbd-platform", "windows")
        .set("x-ss-dp", "787976")
        .set("app-version", IDE_VERSION)
        .set("x-custom-trace-id", &trace_id[..16])
        .set("x-flow-traceparent", &format!("04-{}-{}-01", &trace_id[3..35], uuid_like_id()))
        .set("x-tt-trace-id", &trace_id)
        .set("x-request-id", &request_id)
        .set("referer", &referer)
        .send_bytes(body);

    match resp {
        Ok(r) => {
            // 空完成取证：必须在 into_reader 消费响应前捕获响应头（与下方
            // retry-after 同一约束）；摘要随 reader 一并返回
            let digest = upstream_header_digest(&r, &trace_id[..16], &request_id);
            Ok((Box::new(r.into_reader()), digest))
        }
        Err(ureq::Error::Status(code, response)) => {
            // Retry-After（秒）解析（P1 修复1）：供分级重试表 429 退避决策；
            // header 需在 into_reader 消费响应前读取
            let retry_after = response
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok());
            let body = read_limited_body(response);
            Err((code, body, retry_after))
        }
        Err(e) => {
            let err_str = format!("{}", e);
            // 区分 DNS 解析失败 / 连接超时 / TLS 错误，提供更精准的诊断
            let detail = if err_str.contains("dns") || err_str.contains("resolve") || err_str.contains("name resolution") {
                format!("DNS解析失败（{} 无法解析），请检查网络或代理设置: {}", AGENT_HOST, e)
            } else if err_str.contains("timed out") || err_str.contains("timeout") {
                format!("连接超时（{} 10秒内未响应），请检查网络连通性: {}", AGENT_HOST, e)
            } else if err_str.contains("tls") || err_str.contains("certificate") || err_str.contains("ssl") {
                format!("TLS证书验证失败: {}", e)
            } else {
                format!("传输错误: {}", e)
            };
            Err((502, detail, None))
        }
    }
}

// ==================== Helpers ====================

/// 空 message 兜底（issue #71）：上游错误体可能提取不到 message（如本地代理
/// 劫持 127.0.0.1 回环流量后返回的空体），透传空串会让客户端只看到
/// {"message":""}，缺乏诊断价值；统一回填含状态码的提示文案
pub(crate) fn msg_or_fallback(msg: &str, status: u16) -> Cow<'_, str> {
    if !msg.trim().is_empty() {
        return Cow::Borrowed(msg);
    }
    Cow::Owned(format!(
        "上游服务返回 {status} 但未提供错误详情：常见于网络波动、上游服务异常，或本地全局代理（Clash 等）劫持 127.0.0.1 回环流量；请检查网络与代理设置，详见应用运行日志"
    ))
}

/// 流式错误 message 兜底（issue #71）：code 可能是 HTTP 状态码也可能是业务码
/// （如 6004，或缺失时兜底的 0）；非 HTTP 语义时不冒充状态码，文案与
/// error.code 字段保持一致，避免客户端按 message 误判上游状态
pub(crate) fn stream_msg_or_fallback(msg: &str, code: i64) -> Cow<'_, str> {
    if !msg.trim().is_empty() {
        return Cow::Borrowed(msg);
    }
    if (100..=599).contains(&code) {
        return msg_or_fallback(msg, code as u16);
    }
    Cow::Owned(format!(
        "上游返回错误码 {code} 但未提供错误详情：常见于网络波动、上游服务异常，或本地全局代理（Clash 等）劫持 127.0.0.1 回环流量；请检查网络与代理设置，详见应用运行日志"
    ))
}

/// OpenAI 错误响应格式（wb_route 复用）
pub(crate) fn openai_error(status: StatusCode, code: &str, msg: &str) -> Response {
    let msg = msg_or_fallback(msg, status.as_u16());
    let body = json!({
        "error": {
            "message": msg,
            "type": "api_error",
            "code": code,
        }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
                .unwrap()
        })
}

/// Anthropic 错误响应格式：{"type":"error","error":{"type","message"}}（wb_route 复用）
pub(crate) fn anthropic_error(status: StatusCode, err_type: &str, msg: &str) -> Response {
    let msg = msg_or_fallback(msg, status.as_u16());
    let body = json!({
        "type": "error",
        "error": {
            "type": err_type,
            "message": msg,
        }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("{\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"internal error\"}}"))
                .unwrap()
        })
}

/// 流式错误统一下发：sse 层在流未开始时不透传错误（留待重试决策），
/// 由此处按客户端协议格式化错误事件并收尾
pub(crate) fn send_stream_error(
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    proto: Protocol,
    code: i64,
    msg: &str,
) {
    let msg = stream_msg_or_fallback(msg, code);
    match proto {
        Protocol::OpenAi | Protocol::OpenAiText => {
            let body = json!({
                "error": { "message": msg, "type": "api_error", "code": code }
            });
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!("data: {}\n\n", body))));
            let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
        }
        Protocol::Anthropic => {
            let err = json!({
                "type": "error",
                "error": { "type": "api_error", "message": msg },
            });
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: error\ndata: {}\n\n",
                err
            ))));
        }
        Protocol::Responses => {
            let body = json!({
                "type": "response.failed",
                "response": {
                    "id": format!("resp_{}", now_ts()),
                    "object": "response",
                    "status": "failed",
                    "output": [],
                    "error": {"code": code.to_string(), "message": msg},
                },
            });
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: response.failed\ndata: {}\n\n",
                body
            ))));
        }
    }
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 生成类似 UUID 的十六进制字符串，用于 trace-id 等请求头
fn uuid_like_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = now.as_nanos();
    let seed = (nanos as u64).wrapping_mul(0x517cc1b727220a95);
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&seed.to_le_bytes());
    buf[8..16].copy_from_slice(&(seed.wrapping_add(0x9e3779b97f4a7c15)).to_le_bytes());
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

fn safe_slice(s: &str, n: usize) -> &str {
    // P2 修复6：沿字符边界向前找 <=n 的最大可截断点。原实现非字符边界时
    // 回退返回整串（可能超长），且字节切片 &s[..n] 在多字节字符中间会 panic
    // （上游错误 JSON 常含中文）；现保证输出永不超过 n 字节且不 panic
    if n >= s.len() {
        return s;
    }
    let mut cut = 0;
    for (i, _) in s.char_indices().take_while(|(i, _)| *i <= n) {
        cut = i;
    }
    &s[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== 空 message 兜底（issue #71） ====================

    #[test]
    fn msg_or_fallback_keeps_non_empty_message() {
        assert_eq!(
            msg_or_fallback("upstream code=6004 msg=限额", 502),
            "upstream code=6004 msg=限额"
        );
        assert_eq!(msg_or_fallback("  x  ", 500), "  x  ");
    }

    #[test]
    fn msg_or_fallback_backfills_blank_message_with_status_hint() {
        for blank in ["", " ", "\n\t"] {
            let m = msg_or_fallback(blank, 502);
            assert!(m.contains("502"), "{m}");
            assert!(m.contains("127.0.0.1"), "{m}");
        }
    }

    #[test]
    fn stream_msg_or_fallback_maps_http_and_business_codes() {
        // HTTP 语义 code（含边界 100/599）：沿用状态码文案，与 code 字段一致
        for code in [100_i64, 404, 502, 599] {
            let m = stream_msg_or_fallback("", code);
            assert!(m.contains("上游服务返回"), "code={code}: {m}");
            assert!(m.contains(&code.to_string()), "code={code}: {m}");
        }
        // 业务码/缺失（0）/越界（99、600）/负数/超大 i64：按错误码提示，不冒充状态码
        for code in [0_i64, 99, 600, 6004, -9901, i64::MAX] {
            let m = stream_msg_or_fallback("", code);
            assert!(m.contains("错误码"), "code={code}: {m}");
            assert!(m.contains(&code.to_string()), "code={code}: {m}");
        }
        // 非空 msg 一律原样透传（不 trim 保留原文）
        assert_eq!(stream_msg_or_fallback("  x  ", 6004), "  x  ");
    }

    // ==================== 轮换耗尽收尾文案：指纹拦截 vs 账号池耗尽 ====================

    #[test]
    fn exhaust_message_empty_hits_indicates_fingerprint_block() {
        // 空完成主导：明示指纹拦截语义（账号/模型无关），带清洗规则表提示与健康计数
        let (msg, tag) = exhaust_message(3, 3, 4, 0);
        assert!(msg.contains("channel fingerprint block suspected"), "{msg}");
        assert!(msg.contains("3 empty completion(s) across 3 rotated accounts"), "{msg}");
        assert!(msg.contains("healthy=4"), "{msg}");
        assert!(msg.contains("wb_template_map.json"), "{msg}");
        // issue #150：客户端侧绕过提示（agent 工具形态识别，关工具/普通对话可绕过）
        assert!(msg.contains("agent-tool fingerprint"), "{msg}");
        assert_eq!(tag, "CHANNEL_FINGERPRINT_BLOCK");
    }

    #[test]
    fn exhaust_message_no_empty_hits_keeps_original() {
        // 无空完成（401/429/5xx 等真实不健康）：维持原文案，避免误导
        let (msg, tag) = exhaust_message(0, 3, 0, 0);
        assert_eq!(msg, "no healthy account available");
        assert_eq!(tag, "NO_HEALTHY_ACCOUNT");
    }

    #[test]
    fn exhaust_message_single_empty_hit_keeps_original() {
        // issue #130 防误报：单号轮换 + 1 次空完成（样本不足）不宣判指纹拦截，
        // 上游瞬时抖动同样返回空完成，"switching won't help" 会误导排障方向
        let (msg, tag) = exhaust_message(1, 1, 3, 0);
        assert_eq!(msg, "no healthy account available");
        assert_eq!(tag, "NO_HEALTHY_ACCOUNT");
    }

    #[test]
    fn exhaust_message_minority_empty_hits_keeps_original() {
        // 混合失败：仅 1 次空完成 + 其余限流/5xx，空完成非主导 → 不宣判指纹拦截，
        // 避免 "switching won't help" 误导排障方向（限流等待冷却即可自愈）
        let (msg, tag) = exhaust_message(1, 10, 2, 0);
        assert_eq!(msg, "no healthy account available");
        assert_eq!(tag, "NO_HEALTHY_ACCOUNT");
    }

    #[test]
    fn exhaust_message_majority_empty_hits_indicates_fingerprint_block() {
        // 过半轮换账号返回空完成（6/10）→ 判定指纹拦截主导
        let (msg, tag) = exhaust_message(6, 10, 2, 0);
        assert!(msg.contains("channel fingerprint block suspected"), "{msg}");
        assert_eq!(tag, "CHANNEL_FINGERPRINT_BLOCK");
    }

    #[test]
    fn exhaust_message_transport_cut_only_does_not_blame_accounts() {
        let (msg, tag) = exhaust_message_with_transport_cut(0, 3, 5, 0, 3);
        assert!(msg.contains("transport cut suspected"), "{msg}");
        assert!(msg.contains("3 transport cut(s) across 3 rotated accounts"), "{msg}");
        assert!(!msg.contains("no healthy account available"), "{msg}");
        assert_eq!(tag, "TRANSPORT_CUT");
    }

    #[test]
    fn exhaust_message_transport_cut_mixed_appends_note() {
        // 空完成主导仍保留原归因，但必须补充 cut 计数，不能让网络掐断从诊断中消失
        let (msg, tag) = exhaust_message_with_transport_cut(3, 3, 4, 0, 1);
        assert!(msg.contains("channel fingerprint block suspected"), "{msg}");
        assert!(msg.contains("1 transport cut(s) across 3 rotated accounts"), "{msg}");
        assert_eq!(tag, "CHANNEL_FINGERPRINT_BLOCK");

        // 积分硬冷却分支同样补注，避免把 cut 混入账号归因
        let (msg_credit, tag_credit) = exhaust_message_with_transport_cut(0, 3, 0, 2, 1);
        assert!(msg_credit.contains("credit exhausted on 2 account(s)"), "{msg_credit}");
        assert!(msg_credit.contains("1 transport cut(s) across 3 rotated accounts"), "{msg_credit}");
        assert_eq!(tag_credit, "NO_HEALTHY_ACCOUNT_CREDIT");
    }

    #[test]
    fn exhaust_message_without_transport_cut_keeps_legacy_text() {
        let (msg, tag) = exhaust_message_with_transport_cut(0, 3, 0, 0, 0);
        assert_eq!(msg, "no healthy account available");
        assert_eq!(tag, "NO_HEALTHY_ACCOUNT");
    }

    #[test]
    fn exhaust_message_hard_credit_carries_0400_reset_hint() {
        // issue #156：无空完成但存在积分耗尽硬冷却账号 → 消息携带冷却原因与
        // 次日 04:00 恢复语义，客户端可见「积分耗尽 04:00 重置」而非裸 no healthy
        let (msg, tag) = exhaust_message(0, 3, 0, 2);
        assert!(msg.contains("no healthy account available"), "{msg}");
        assert!(msg.contains("credit exhausted on 2 account(s)"), "{msg}");
        assert!(msg.contains("next 04:00"), "{msg}");
        assert_eq!(tag, "NO_HEALTHY_ACCOUNT_CREDIT");
        // 混合失败诚实语境：轮换过其他账号（如 401 软熔断）时补充 further note，
        // 避免把其他失败也归因于积分耗尽
        let (msg_mix, _) = exhaust_message(0, 1, 0, 2);
        assert!(msg_mix.contains("credit exhausted on 2 account(s)"), "{msg_mix}");
        assert!(msg_mix.contains("1 further account(s) also failed this rotation"), "{msg_mix}");
        // 指纹拦截主导优先级高于积分耗尽提示（指纹拦截与账号无关，积分提示会误导）
        let (msg_fp, tag_fp) = exhaust_message(6, 10, 2, 2);
        assert!(msg_fp.contains("channel fingerprint block suspected"), "{msg_fp}");
        assert_eq!(tag_fp, "CHANNEL_FINGERPRINT_BLOCK");
    }

    // ==================== /v1/models 条目可见性（issue #145） ====================

    /// 最小 UnifiedModel 构造（仅可见性判定用字段）
    fn um(id: &str, sources: Vec<unified_catalog::UnifiedSource>) -> unified_catalog::UnifiedModel {
        unified_catalog::UnifiedModel {
            id: id.into(),
            display: id.into(),
            vendor: String::new(),
            region: None,
            rate: None,
            efforts: vec![],
            max_mode: false,
            context_length: None,
            max_tokens: None,
            supports_image: None,
            sources,
            manual: false,
        }
    }

    fn src(pool: &'static str, disabled: bool) -> unified_catalog::UnifiedSource {
        unified_catalog::UnifiedSource {
            pool,
            rate: None,
            enabled: false,
            disabled,
        }
    }

    #[test]
    fn models_entry_visible_filters_ghost_and_all_disabled() {
        // 幽灵条目（issue #145 审查修复）：WB 关 + buddy-ai 关 + 多源模型仅剩的
        // Trae 源被 L1 禁用 → buddy 源不可路由、trae 源被禁，条目不得对外发布
        //（旧行为两子句均放行，发布后任何请求都 400 wb_upstream_disabled）
        let ghost = um("m1", vec![src("buddy", false), src("trae", true)]);
        assert!(!models_entry_visible(&ghost, false, false));
        // 同条目 WB 开启 → buddy 源可路由，正常发布
        assert!(models_entry_visible(&ghost, true, false));
        // 对照：trae 未禁时 WB 关仍保留条目（由 Trae 源服务，§3.4 既有口径）
        let multi = um("m2", vec![src("buddy", false), src("trae", false)]);
        assert!(models_entry_visible(&multi, false, false));
        // 唯一 Trae 源被 L1 全禁 → 不发布（无论 WB 开关，全禁条款 ②）
        let trae_only = um("m3", vec![src("trae", true)]);
        assert!(!models_entry_visible(&trae_only, false, false));
        assert!(!models_entry_visible(&trae_only, true, false));
        // 仅 WB 系源 + WB 关 → 准入口径过滤（既有行为不回退）
        let buddy_only = um("m4", vec![src("buddy", false)]);
        assert!(!models_entry_visible(&buddy_only, false, false));
        assert!(models_entry_visible(&buddy_only, true, false));
        // 纯 Trae 源未禁 + WB 关 → 保留（trae 不受 WB 准入约束）
        let trae_ok = um("m5", vec![src("trae", false)]);
        assert!(models_entry_visible(&trae_ok, false, false));
    }

    // ==================== P2 修复6：safe_slice 字符边界截断 ====================

    #[test]
    fn safe_slice_cuts_on_char_boundary() {
        // ASCII：n 在串内 → 精确截断
        assert_eq!(safe_slice("hello world", 5), "hello");
        // n 超长 → 原文
        assert_eq!(safe_slice("abc", 100), "abc");
        // n 等于串长 → 原文
        assert_eq!(safe_slice("abc", 3), "abc");
        // n=0 → 空串（原实现会 panic）
        assert_eq!(safe_slice("中文", 0), "");
    }

    #[test]
    fn safe_slice_never_returns_overlong_or_panics() {
        // 200 落在多字节字符中间：沿边界向前取最近可截断点，不 panic、不回退整串
        let s = format!("{}{}", "a".repeat(199), "中文中文中文");
        let out = safe_slice(&s, 200);
        assert!(out.len() <= 200, "输出不得超过 n 字节");
        assert_eq!(out.len(), 199, "应回退到最近字符边界（199 处）");
        // n=1/2 落在 3 字节「中」的中间 → 边界回退到 0
        assert_eq!(safe_slice("中文", 1), "");
        assert_eq!(safe_slice("中文", 2), "");
        assert_eq!(safe_slice("中文", 3), "中");
        // 任意 n 都不 panic 且不超长
        let body = "上游错误：{\"code\":1005,\"message\":\"套餐额度用尽\"}";
        for n in 0..=body.len() {
            let out = safe_slice(body, n);
            assert!(out.len() <= n);
            assert!(body.starts_with(out));
        }
    }

    // ==================== issue #26 全局模型白名单 ====================

    /// 白名单准入用的最小 state fixture：仅 data_dir 参与白名单读取（kv SQLite），
    /// 其余字段与 dispatch 测试 fixture 同构（空池/默认开关）；Drop 兜底清理临时目录
    struct WlFixture {
        dir: std::path::PathBuf,
        state: Arc<ApiSharedState>,
    }

    impl Drop for WlFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn wl_fixture(tag: &str) -> WlFixture {
        let dir = std::env::temp_dir().join(format!(
            "twa_routes_wl_test_{}_{}_{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(ApiSharedState {
            pool: super::super::pool::ApiPool::new(),
            wb_pool: super::super::pool::ApiPool::new(),
            fallback_log_last_ms: std::sync::Mutex::new(std::collections::HashMap::new()),
            wb_enabled: std::sync::atomic::AtomicBool::new(true),
            wb_sanitize: std::sync::atomic::AtomicBool::new(true),
            wb_default_thinking: std::sync::atomic::AtomicBool::new(false),
            wb_tool_exec: std::sync::atomic::AtomicBool::new(false),
            wb_bg_downgrade: std::sync::atomic::AtomicBool::new(false),
            wb_longctx_downgrade: std::sync::atomic::AtomicBool::new(false),
            wb_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            trae_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            trae_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            wb_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            qoder_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            qoder_flash_free_rate: std::sync::atomic::AtomicBool::new(true),
            wb_sticky: super::super::wb_sticky::StickyStore::default(),
            model_cooldowns: std::sync::Mutex::new(std::collections::HashMap::new()),
            buddy_ai_model_cooldowns: std::sync::Mutex::new(std::collections::HashMap::new()),
            default_model: "deepseek-v4-flash".into(),
            data_dir: dir.clone(),
            total_requests: std::sync::atomic::AtomicU64::new(0),
            inflight: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            active_uid: std::sync::Mutex::new(None),
            last_error: std::sync::Mutex::new(None),
            pool_sticky: std::sync::Mutex::new(std::collections::HashMap::new()),
            logger: super::super::ApiLogger::new(dir.join("logs")),
            debug_enabled: std::sync::atomic::AtomicBool::new(false),
            usage: std::sync::Mutex::new(super::super::usage::UsageFile::default()),
            usage_dirty: std::sync::Mutex::new(Vec::new()),
            wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
            buddy_ai_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            buddy_ai_probe_ok: std::sync::atomic::AtomicI64::new(-1),
            trae_jwt_refresh: None,
            trae_enabled: std::sync::atomic::AtomicBool::new(true),
            qoder_pool: super::super::pool::ApiPool::new(),
            qoder_enabled: std::sync::atomic::AtomicBool::new(true),
            qoder_identity: None,
            qoder_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            qoder_sticky_enabled: std::sync::atomic::AtomicBool::new(false),
            qoder_sanitize: std::sync::atomic::AtomicBool::new(true),
            qoder_sticky: super::super::wb_sticky::StickyStore::default(),
            trae_sticky: super::super::wb_sticky::StickyStore::default(),
            buddy_ai_sticky: super::super::wb_sticky::StickyStore::default(),
            buddy_ai_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            buddy_ai_pool: super::super::pool::ApiPool::new(),
            buddy_ai_enabled: std::sync::atomic::AtomicBool::new(false),
            buddy_ai_sticky_enabled: std::sync::atomic::AtomicBool::new(false),
            buddy_ai_pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            listen_host: std::sync::Mutex::new("127.0.0.1".to_string()),
            custom_upstream_sticky: std::sync::Mutex::new(std::collections::HashMap::new()),
            custom_lb_counters: std::sync::Mutex::new(std::collections::HashMap::new()),
        });
        WlFixture { dir, state }
    }

    /// 拒绝壳格式（纯函数）：OpenAI 三协议共用 model_not_found 404 壳，
    /// Anthropic 用 not_found_error；消息携带模型名与 /v1/models 指引
    #[tokio::test]
    async fn whitelist_error_response_format_by_protocol() {
        for proto in [Protocol::OpenAi, Protocol::OpenAiText, Protocol::Responses] {
            let resp = whitelist_error_response("kimi-k3", proto);
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{}", proto.log_path());
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v["error"]["code"], "model_not_found");
            assert_eq!(v["error"]["type"], "api_error");
            let msg = v["error"]["message"].as_str().unwrap();
            assert!(msg.contains("kimi-k3") && msg.contains("/v1/models"), "{}", msg);
        }
        // Anthropic：type=error 外壳 + not_found_error
        let resp = whitelist_error_response("kimi-k3", Protocol::Anthropic);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "not_found_error");
        assert!(v["error"]["message"].as_str().unwrap().contains("kimi-k3"));
    }

    /// whitelist_check 端到端（真实 kv 读写）：空名单放行；canonical 归一命中；
    /// 非名单模型返回按协议格式化的 404
    #[tokio::test]
    async fn whitelist_check_state_end_to_end() {
        let f = wl_fixture("e2e");
        // 空名单 = 不限（默认行为零变化）
        assert!(whitelist_check(&f.state, "glm-5.3", Protocol::OpenAi).is_none());
        // 保存「GLM-5.3」→ 大小写/空白变体均命中（canonical 归一）
        unified_catalog::save_whitelist(&f.dir, &["GLM-5.3".to_string()]).unwrap();
        assert!(whitelist_check(&f.state, "GLM-5.3", Protocol::OpenAi).is_none());
        assert!(whitelist_check(&f.state, "  glm-5.3 ", Protocol::Anthropic).is_none());
        // 非名单模型 → 404 model_not_found（OpenAI 壳）
        let denied = whitelist_check(&f.state, "kimi-k3", Protocol::OpenAi).unwrap();
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(denied.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "model_not_found");
    }

    /// 审查 C1 + issue #90 修复③a：池前缀模型按裸名准入（与 /v1/models 别名
    /// 条目同一前缀语义）；被拒时拒绝信息保留用户请求的原始名（含前缀），
    /// 不得显示裸名（遮蔽回归防护）
    #[tokio::test]
    async fn whitelist_check_prefix_admission_keeps_original_name_in_denial() {
        let f = wl_fixture("prefix");
        unified_catalog::save_whitelist(&f.dir, &["glm-5.3".to_string()]).unwrap();
        // 前缀模型按裸名准入
        assert!(whitelist_check(&f.state, "trae/glm-5.3", Protocol::OpenAi).is_none());
        // 非名单前缀模型 → 404 且消息保留原始前缀名
        let denied = whitelist_check(&f.state, "trae/kimi-k3", Protocol::OpenAi).unwrap();
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(denied.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("model trae/kimi-k3 is not in"),
            "拒绝信息应保留原始前缀名: {msg}"
        );
    }

    // ==================== issue #54：Claude Code 分类器模型兜底 ====================

    /// `[1m]` 窗口标记剥离：仅尾部完整标记生效，剥离后非空
    #[test]
    fn strip_bracket_1m_tail_only() {
        assert_eq!(strip_bracket_1m("claude-sonnet-5[1m]"), Some("claude-sonnet-5"));
        assert_eq!(strip_bracket_1m("glm-5.3[1M]"), Some("glm-5.3"));
        assert_eq!(strip_bracket_1m("claude-sonnet-5[1m]x"), None, "非尾部标记不剥");
        assert_eq!(strip_bracket_1m("[1m]"), None, "剥离后为空不剥");
        assert_eq!(strip_bracket_1m("claude-sonnet-5"), None);
    }

    /// 多字节字符 + 大小写字节长度变化：字节尾比较不 panic、不切错位置。
    /// `ẞ`(U+1E9E,3B)→`ß`(2B)、`İ`(2B)→`i̇`(3B) 均会漂移 to_lowercase 偏移，
    /// 旧实现（lower 偏移切原串）在 `claude-ẞ[1m]` 上越过字符边界直接 panic
    #[test]
    fn strip_bracket_1m_multibyte_case_folding_safe() {
        assert_eq!(strip_bracket_1m("claude-ẞ[1m]"), Some("claude-ẞ"));
        assert_eq!(strip_bracket_1m("glm-5.3İ[1M]"), Some("glm-5.3İ"));
        // 多字节结尾/中部标记不误判
        assert_eq!(strip_bracket_1m("中文模型"), None);
        assert_eq!(strip_bracket_1m("模型[1m]中"), None);
        // 4 字节以内短输入不 panic
        assert_eq!(strip_bracket_1m("[1"), None);
        assert_eq!(strip_bracket_1m(""), None);
    }

    /// 纯 Trae 用户（WB 关闭）：分类器官方模型名三池不可路由 → 回落网关默认模型
    #[tokio::test]
    async fn fallback_claude_classifier_to_default_when_unroutable() {
        let f = wl_fixture("claude_fb");
        f.state
            .wb_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // 分类器 side_query 的官方模型名（issue #54 报错模型）
        assert_eq!(
            anthropic_model_fallback(&f.state, "claude-sonnet-5[1m]").as_deref(),
            Some("deepseek-v4-flash")
        );
        // 无窗口标记的 claude 系未知名同样兜底
        assert_eq!(
            anthropic_model_fallback(&f.state, "Claude-Opus-5").as_deref(),
            Some("deepseek-v4-flash")
        );
    }

    /// WB 启用用户：claude-* 经内置系列映射命中 WB 目录 → 原样保留（行为不变）
    #[tokio::test]
    async fn fallback_keeps_claude_when_wb_routes_it() {
        let f = wl_fixture("claude_wb"); // fixture 默认 wb_enabled=true，builtin 目录含 glm-5.3
        assert!(anthropic_model_fallback(&f.state, "claude-sonnet-5[1m]").is_none());
    }

    /// G6 审查修复：纯 buddy-ai 用户（wb 关、intl 开）——Intl 目录模型可路由，
    /// 原样保留（原先 servable 只认 CN 目录被误判，Intl 专属 id 被迫回落默认模型）
    #[tokio::test]
    async fn servable_includes_buddy_ai_intl_catalog() {
        let f = wl_fixture("claude_intl");
        f.state
            .wb_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        f.state
            .buddy_ai_enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Intl 专属 id（内置静态表兜底）：原名直查 Intl 目录命中
        assert!(anthropic_model_servable(&f.state, "deepseek-v4.1-flash"));
        // claude 系分类器名 Intl 目录无映射 → 仍回落默认模型（行为不变）
        assert_eq!(
            anthropic_model_fallback(&f.state, "claude-sonnet-5[1m]").as_deref(),
            Some("deepseek-v4-flash")
        );
    }

    /// 基名在目录（原名带 [1m] 不在）→ 剥标记用基名
    #[tokio::test]
    async fn fallback_strips_1m_when_base_routable() {
        let f = wl_fixture("claude_1m");
        assert_eq!(
            anthropic_model_fallback(&f.state, "glm-5.3[1M]").as_deref(),
            Some("glm-5.3")
        );
    }

    /// 非 claude 未知名维持透传语义（与 dispatch t07 零行为差异）；默认模型同名不循环改写
    #[tokio::test]
    async fn fallback_leaves_unknown_names_passthrough() {
        let f = wl_fixture("claude_pass");
        f.state
            .wb_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(anthropic_model_fallback(&f.state, "brand-new-model").is_none());
        // 未知名带 [1m] 但基名不可路由且非 claude 系 → 原样
        assert!(anthropic_model_fallback(&f.state, "brand-new-model[1m]").is_none());
        // 模型名恰为默认模型 → None（不产生无意义改写）
        assert!(anthropic_model_fallback(&f.state, "deepseek-v4-flash").is_none());
    }

    // ==================== issue #31 T3.2/T3.3：Trae effort wire ====================

    /// 三源合成：显式请求 > 路由提示 > 默认思考；空串显式 = Anthropic thinking
    /// disabled 编码，短路为 None（同时压制默认思考与路由提示）；实证表外模型
    /// 显式请求按统一→Trae 映射填充默认下发（合成默认不下发）；canonical 归一
    /// 命中实证表
    #[test]
    fn trae_effort_wire_three_sources_and_short_circuit() {
        let f = wl_fixture("effort");

        // 无显式、无提示、默认思考关 → None
        assert!(trae_effort_wire(&f.state, None, None, "glm-5.3").is_none());

        // 路由提示兜底（实证表内模型 → wire 命中）
        assert_eq!(
            trae_effort_wire(&f.state, Some("high".into()), None, "glm-5.3").as_deref(),
            Some("high")
        );

        // 显式优先于路由提示；统一档位映射：low → light
        assert_eq!(
            trae_effort_wire(&f.state, Some("high".into()), Some("low".into()), "glm-5.3")
                .as_deref(),
            Some("light")
        );

        // 统一档位映射：xhigh → extra_high
        assert_eq!(
            trae_effort_wire(&f.state, None, Some("xhigh".into()), "glm-5.3").as_deref(),
            Some("extra_high")
        );

        // 默认思考开 → high（仍经实证表转换）
        f.state
            .wb_default_thinking
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            trae_effort_wire(&f.state, None, None, "glm-5.3").as_deref(),
            Some("high")
        );

        // 显式关闭（空串编码）短路一切：默认思考开着、路由提示在也返回 None
        assert!(
            trae_effort_wire(&f.state, Some("high".into()), Some(String::new()), "glm-5.3")
                .is_none()
        );

        // 实证表外模型（Trae 无实证，如档位仅 Buddy 侧声明）：显式请求填充默认
        //（统一→Trae 映射直接下发）；合成默认（路由提示）不下发
        assert_eq!(
            trae_effort_wire(&f.state, None, Some("high".into()), "deepseek-v4-flash").as_deref(),
            Some("high")
        );
        assert_eq!(
            trae_effort_wire(&f.state, None, Some("medium".into()), "deepseek-v4-flash")
                .as_deref(),
            Some("light")
        );
        assert!(trae_effort_wire(&f.state, Some("high".into()), None, "deepseek-v4-flash").is_none());

        // canonical 归一：大小写变体同样命中实证表
        assert_eq!(
            trae_effort_wire(&f.state, None, Some("high".into()), "GLM-5.3").as_deref(),
            Some("high")
        );
    }

    /// Trae 出站注入（issue #31 T3.3/T4.2，合并后单次 parse）：effort wire=Some 写
    /// reasoning_effort_level（T0.2 抓包后仅改 TRAE_EFFORT_FIELD 常量）；Max Mode
    /// 入口标志 + 支持表双门控、未请求不注入（保持请求最小化）、注入值布尔 true；
    /// body model 字段回写剥离后基名（issue #38 真机根因）；原字段保留；三路均未
    /// 激活原样返回；非对象/非 JSON body 原样返回
    #[test]
    fn inject_trae_outbound_variants() {
        // 测试桩：注入函数现需 data_dir 落可观测性日志（issue #38-5），指向临时目录
        let dir = std::env::temp_dir();
        let inject = |body: Vec<u8>, wire: Option<String>, mm: bool, requested: &str, model: &str| {
            inject_trae_outbound(&dir, body, wire, mm, requested, model)
        };
        let body = br#"{"model":"glm-5.3","messages":[]}"#.to_vec();

        // 三路均未激活 → 原样（零 parse）
        assert_eq!(inject(body.clone(), None, false, "glm-5.3", "glm-5.3"), body);

        // effort 单路：注入字段，原字段保留
        let out = inject(body.clone(), Some("extra_high".into()), false, "glm-5.3", "glm-5.3");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning_effort_level"], "extra_high");
        assert_eq!(v["model"], "glm-5.3");
        assert!(v.get("is_max_mode").is_none());

        // Max Mode 单路：注入 is_max_mode:true
        let out = inject(body.clone(), None, true, "glm-5.3", "glm-5.3");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["is_max_mode"], true);
        assert!(v.get("reasoning_effort_level").is_none());

        // 支持表外模型不注入（不按名称冒进）
        assert_eq!(inject(body.clone(), None, true, "doubao-seed-code", "doubao-seed-code"), body);

        // canonical 归一：大小写变体同样命中支持表
        let out = inject(body.clone(), None, true, "GLM-5.3", "GLM-5.3");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["is_max_mode"], true);

        // 双路并发：单次 parse 同时注入两字段
        let out = inject(body.clone(), Some("high".into()), true, "glm-5.3", "glm-5.3");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning_effort_level"], "high");
        assert_eq!(v["is_max_mode"], true);
        assert_eq!(v["model"], "glm-5.3");

        // body model 回写（issue #38 真机根因）：`-max` 剥离后 body model 仍为
        // 带后缀原名 → 上游收到 xxx-max__dev 报 4001；回写为基名
        let sfx = br#"{"model":"DeepSeek-V4-Flash-Official-max","messages":[]}"#.to_vec();
        let out = inject(sfx.clone(), None, false, "DeepSeek-V4-Flash-Official-max", "DeepSeek-V4-Flash-Official");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "DeepSeek-V4-Flash-Official", "仅回写场景（无注入）也生效");
        let out = inject(sfx, None, true, "DeepSeek-V4-Flash-Official-max", "DeepSeek-V4-Flash-Official");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "DeepSeek-V4-Flash-Official");
        assert_eq!(v["is_max_mode"], true);

        // 非对象 body（数组）→ 原样
        let arr = br#"[1,2,3]"#.to_vec();
        assert_eq!(inject(arr.clone(), Some("high".into()), true, "glm-5.3", "glm-5.3"), arr);

        // 非 JSON body → 原样
        let raw = b"not-json".to_vec();
        assert_eq!(inject(raw.clone(), Some("high".into()), true, "glm-5.3", "glm-5.3"), raw);
    }
}
