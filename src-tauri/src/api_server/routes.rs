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
use super::{classify_error, classify_solo_error, streaming_agent, ApiSharedState, ErrKind,
            InflightGuard,
            AGENT_HOST, APP_ID, EP_LLM_CHAT, IDE_VERSION, IDE_VERSION_CODE, REFERER_BASE};

const MAX_ROTATE: usize = 3;
/// 请求体上限（issue #21）：axum `Bytes` 提取器受 DefaultBodyLimit 约束（默认 2MiB），
/// build_router 已显式放开到本值，两处阈值必须一致。32MiB 适配长上下文客户端
/// （每轮重发完整历史 + 图片 base64 场景）
pub(super) const MAX_BODY_BYTES: usize = 32 << 20;

/// 请求体超限文案（issue #21）：由常量推导，避免阈值调整后文案脱节
pub(super) fn body_too_large_msg() -> String {
    format!("request body exceeds {}MB limit", MAX_BODY_BYTES >> 20)
}

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
    if wb_catalog::find(&catalog, &r.model).is_none() {
        return None;
    }
    // T5.6③ 后台任务降级（显式开启才生效）：标题/摘要类短请求 → 目录最低倍率模型；
    // F-76④ 长上下文降档：输入粗估超阈值 → flash 档模型。
    // issue #26 白名单感知：降级候选 ∩ 白名单为空则跳过降级（原模型已过端点准入）
    let whitelist = unified_catalog::load_whitelist(&state.data_dir);
    let in_wl = |m: &str| unified_catalog::whitelist_allows(&whitelist, m);
    let final_model = if state.wb_bg_downgrade.load(std::sync::atomic::Ordering::Relaxed)
        && wb_model_route::is_background_task(body)
    {
        wb_model_route::cheapest_catalog_model_filtered(&catalog, &in_wl).unwrap_or(r.model)
    } else if state.wb_longctx_downgrade.load(std::sync::atomic::Ordering::Relaxed)
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
    let wire = if supported.is_empty() {
        // 表外无实证：仅显式请求填充默认（统一→Trae 映射），合成默认不下发
        efforts::trae_request_wire_fill(explicit.as_deref())
    } else {
        let requested = explicit.clone().or(route_hint).or_else(|| {
            state
                .wb_default_thinking
                .load(std::sync::atomic::Ordering::Relaxed)
                .then(|| "high".to_string())
        });
        requested.and_then(|req| efforts::trae_request_wire(Some(&req), &supported))
    };
    // 可观测性（issue #38-5）：请求侧档位 → wire 档位映射落盘（脱敏，仅模型与档位），
    // 用于验证「档位到底有没有下发/映射成哪一档」
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!(
            "trae effort: model={} requested={:?} wire={:?}",
            model, explicit, wire
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

/// 模型级冷却快速失败（T2.7/F-34：优先级高于 Key 级）
fn model_cooling_response(state: &ApiSharedState, model: &str, proto: Protocol) -> Response {
    let rem = wb_route::model_cooling_remaining(state, model).unwrap_or(0);
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
            DispatchError::ModelCooling(rem) => (
                StatusCode::TOO_MANY_REQUESTS,
                "buddy",
                format!("model {model} cooling down, retry after {rem}s"),
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

/// NoHealthy 详情（issue #29 修复2）：错误消息携带 Key 约束细节（绑定池/白名单
/// 条数/专一账号）与池内健康计数，帮助用户自查 Key 配置（匿名/未知 Key 仅报
/// 池健康数）；healthy_in_scope 为约束作用域内健康账号数，0 即「约束排除致败」。
/// pub(crate)：dispatch 测试模块经 Fixture 直测（与 trae_pool_constraints 同模式）
pub(crate) fn no_healthy_detail(state: &ApiSharedState, pool: TargetPool, key_str: &str) -> String {
    let pool_ref = match pool {
        TargetPool::Trae => &state.pool,
        TargetPool::Buddy => &state.wb_pool,
        // 不可达：custom 在 resolve_target 顶部短路返回（匹配穷尽兜底）
        TargetPool::Custom => return "no healthy account available".to_string(),
    };
    let (healthy_total, _) = pool_ref.selectable_stats_in(None);
    let base = format!(
        "no healthy account available (pool={} healthy={healthy_total})",
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
            let (_, healthy_scope) = pool_ref.selectable_stats_in(c.allowed.as_ref());
            let wl = match &c.allowed {
                None => "none".to_string(),
                Some(s) => s.len().to_string(),
            };
            let ded = c.dedicated.as_deref().unwrap_or("-");
            format!(
                "{base} key={name} bind={bind} whitelist={wl} dedicated={ded} healthy_in_scope={healthy_scope}"
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
    // issue #38 实测修复：`-max` / `-thinking` 等入口后缀在 dispatch ② 段才剥离，
    // 白名单须按同一剥离规则对基名放行——否则启用白名单的部署下，Max Mode /
    // 路由后缀入口在准入层就被 404 拦截，整个能力不可用（剥离规则与 dispatch 同源，
    // 基名最终可服务性仍由 dispatch 查模型列表判定，此处仅准入放行）
    let cfg = wb_model_route::load_config(&state.data_dir);
    let bases = [
        wb_model_route::strip_max_suffix(model),
        wb_model_route::strip_route_suffix(model, &cfg).map(|(b, _)| b),
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
    let pool = state.pool.status_list();
    let wb_pool = state.wb_pool.status_list();
    let available = pool.iter().filter(|p| !p.disabled && !p.cooling).count();
    let cooling = pool.iter().filter(|p| p.cooling).count();
    let disabled = pool.iter().filter(|p| p.disabled).count();
    let total_credits: f64 = pool.iter().filter_map(|p| p.credits).sum();
    let total = state.total_requests.load(std::sync::atomic::Ordering::Relaxed);
    let active = safe_lock(&state.active_uid).clone();
    let last_err = safe_lock(&state.last_error).clone();
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let wb_available = wb_pool.iter().filter(|p| !p.disabled && !p.cooling).count();

    Json(json!({
        "status": "ok",
        "running": true,
        "total_requests": total,
        "active_uid": active,
        "last_error": last_err,
        "pool": {
            "total_accounts": pool.len(),
            "available": available,
            "cooling": cooling,
            "disabled": disabled,
            "total_credits": (total_credits * 100.0).round() / 100.0,
        },
        "wb": {
            "enabled": wb_enabled,
            "total_accounts": wb_pool.len(),
            "available": wb_available,
        }
    }))
}

/// /healthz（T2.3/F-32）：无健康账号（两个池都没有）→ 503，供探活/看门狗
pub async fn healthz(State(state): State<Arc<ApiSharedState>>) -> Response {
    let pool = state.pool.status_list();
    let wb_pool = state.wb_pool.status_list();
    let solo_ok = pool.iter().any(|p| !p.disabled && !p.cooling);
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let wb_ok = wb_pool.iter().any(|p| !p.disabled && !p.cooling);
    if solo_ok || (wb_enabled && wb_ok) {
        (
            StatusCode::OK,
            axum::Json(json!({ "status": "ok", "solo_available": solo_ok, "wb_available": wb_ok })),
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
    // F-77：per-account 并发列表（活跃账号 [{uid, name, inflight}]），
    // 供前端池状态页实时展示 busy/idle 分布（active_uid 单值的超集）
    let active_accounts: Vec<Value> = pool
        .iter()
        .chain(wb_pool.iter())
        .filter(|p| p.inflight > 0)
        .map(|p| json!({ "uid": p.uid, "name": p.name, "inflight": p.inflight }))
        .collect();
    let model_cooldowns: Vec<Value> = {
        let map = safe_lock(&state.model_cooldowns);
        map.iter()
            .filter(|(_, (until, _))| *until > now)
            .map(|(m, (until, fails))| json!({
                "model": m, "until": until, "remaining_s": until - now, "fails": fails,
            }))
            .collect()
    };

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
    }))
}

pub async fn models(State(state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    // 统一模型目录（§3.4）：实时聚合 data/api_models.json（Trae，元数据四层链）
    // 与 data/wb_model_catalog.json（Buddy），纯派生不落盘。官网/目录同步后
    // 无需重启 API 服务即可通过 /v1/models 看到最新列表
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    // 可用性标记运行时派生（§3.3 #5）：HTTP 端点用实时池健康
    let trae_ok = state.pool.has_selectable();
    let buddy_ok = state.wb_pool.has_selectable();
    let data_dir = state.data_dir.clone();
    let list = tokio::task::spawn_blocking(move || {
        // issue #26：对外目录经全局白名单过滤（管理端 api_unified_models 不过滤）
        unified_catalog::unified_models_whitelisted(&data_dir, wb_enabled, trae_ok, buddy_ok)
    })
    .await
    .unwrap_or_default();
    // wb_enabled=false：仅 Buddy 源的模型过滤，双源模型保留（仍可由 Trae 源服务 §3.4）
    let data: Vec<Value> = list
        .iter()
        .filter(|m| wb_enabled || !m.sources.iter().all(|s| s.pool == "buddy"))
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
    Json(json!({ "object": "list", "data": data }))
}

pub async fn chat_completions(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
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
            TargetPool::Buddy => {
                // T5.3 默认深度思考：客户端未带 reasoning_effort 时注入 high
                let explicit = peek.get("reasoning_effort").and_then(|v| v.as_str()).is_some();
                let hint = effective_effort_hint(&state, r.effort_hint, explicit);
                let body_vec = apply_effort_hint(body_vec, hint);
                if stream {
                    return wb_route::wb_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::OpenAi, key_str, guard);
                }
                return wb_route::wb_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, guard).await;
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
                // 自定义模型直达（custom_models 命中即 Custom，§dispatch ⓪）；
                // 条目可能热更新，执行时重读，缺失（已被删）按 404 语义报错
                match super::custom_models::find_enabled(&state.data_dir, &model) {
                    Some(cm) => {
                        if stream {
                            custom_route::custom_stream_chat(state_clone, body_vec, r.model, cm, start_ts, Protocol::OpenAi, key_str, guard)
                        } else {
                            custom_route::custom_aggregate_chat(state_clone, body_vec, r.model, cm, stream, start_ts, Protocol::OpenAi, key_str, guard).await
                        }
                    }
                    None => openai_error(StatusCode::NOT_FOUND, "model_not_found", &format!("自定义模型 {} 已被删除", model)),
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
    if body.len() > MAX_BODY_BYTES {
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
    if !state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed) {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "wb_upstream_disabled",
            "WB 上游未启用（api_pool.json wb_enabled）",
        );
    }
    if wb_route::model_cooling_remaining(&state, &resolved_model).is_some() {
        return model_cooling_response(&state, &resolved_model, Protocol::Responses);
    }

    let mut chat_body = chat_body;
    // T5.5/F-64 工具代执行：客户端声明 web_search 类工具且开关开启 → 代理侧代执行编排
    if state.wb_tool_exec.load(std::sync::atomic::Ordering::Relaxed)
        && super::wb_toolexec::responses_declares_web_search(&peek)
    {
        return wb_route::wb_tool_exec_chat(
            state_clone,
            chat_body,
            resolved_model,
            stream,
            start_ts,
            key_str,
            guard,
        )
        .await;
    }
    chat_body["stream"] = json!(stream);
    // T5.3 默认深度思考（Responses: reasoning.effort 已投影为 reasoning_effort）
    let explicit = chat_body.get("reasoning_effort").and_then(|v| v.as_str()).is_some();
    let hint = effective_effort_hint(&state, route_hint, explicit);
    let body_vec = apply_effort_hint(serde_json::to_vec(&chat_body).unwrap_or_default(), hint);

    if stream {
        wb_route::wb_stream_chat(state_clone, body_vec, resolved_model, start_ts, Protocol::Responses, key_str, guard)
    } else {
        wb_route::wb_aggregate_chat(state_clone, body_vec, resolved_model, stream, start_ts, Protocol::Responses, key_str, guard).await
    }
}

/// Anthropic Messages 端点（F-39：+Anthropic 适配）
/// 请求：POST /v1/messages，鉴权支持 x-api-key 或 Authorization: Bearer
pub async fn messages(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
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
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();
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
            TargetPool::Buddy => {
                // T5.3 默认深度思考：Anthropic 侧 thinking 参数视为显式请求
                let explicit = peek.get("thinking").map_or(false, |t| !t.is_null());
                let hint = effective_effort_hint(&state, r.effort_hint, explicit);
                let body_vec = apply_effort_hint(body_vec, hint);
                if stream {
                    return wb_route::wb_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::Anthropic, key_str, guard);
                }
                return wb_route::wb_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::Anthropic, key_str, guard).await;
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
                match super::custom_models::find_enabled(&state.data_dir, &model) {
                    Some(cm) => {
                        if stream {
                            custom_route::custom_stream_chat(state_clone, body_vec, r.model, cm, start_ts, Protocol::Anthropic, key_str, guard)
                        } else {
                            custom_route::custom_aggregate_chat(state_clone, body_vec, r.model, cm, stream, start_ts, Protocol::Anthropic, key_str, guard).await
                        }
                    }
                    None => anthropic_error(StatusCode::NOT_FOUND, "model_not_found", &format!("自定义模型 {} 已被删除", model)),
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
    if body.len() > MAX_BODY_BYTES {
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
            TargetPool::Buddy => {
                // T5.3 默认深度思考（text completions 无 effort 字段 → 默认思考直接生效）
                let hint = effective_effort_hint(&state, r.effort_hint, false);
                let body_vec = apply_effort_hint(body_vec, hint);
                if stream {
                    return wb_route::wb_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::OpenAiText, key_str, guard);
                }
                return wb_route::wb_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAiText, key_str, guard).await;
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
                match super::custom_models::find_enabled(&state.data_dir, &model) {
                    Some(cm) => {
                        if stream {
                            custom_route::custom_stream_chat(state_clone, body_vec, r.model, cm, start_ts, Protocol::OpenAiText, key_str, guard)
                        } else {
                            custom_route::custom_aggregate_chat(state_clone, body_vec, r.model, cm, stream, start_ts, Protocol::OpenAiText, key_str, guard).await
                        }
                    }
                    None => openai_error(StatusCode::NOT_FOUND, "model_not_found", &format!("自定义模型 {} 已被删除", model)),
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
    if body.len() > MAX_BODY_BYTES {
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
            .pick_excluding_constrained_ev(&tried, allowed_set.as_ref(), dedicated.as_deref())
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
        let mut tried = HashSet::new();
        // 401 自愈去重（issue #27 方案 B）：每账号每请求最多强制刷新一次（对齐 wb_route T2.6）
        let mut refreshed_401 = HashSet::new();

        for _ in 0..MAX_ROTATE {
            let mut picked = match state
                .pool
                .pick_excluding_constrained(&tried, trae_allowed.as_ref(), trae_dedicated.as_deref())
            {
                Some(p) => p,
                None => break,
            };
            tried.insert(picked.uid.clone());
            // F-77 账号级在途计数：取号即绑定（换号时 bind_account 自动解绑旧账号）
            guard = guard.bind_account(state.pool.inflight_handle(&picked.uid));
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            let converted = super::payload::prepare_llm_chat_body(
                &body_vec, &state.default_model, &picked.uid, &picked.device_id, &picked.machine_id,
                // 模型目录（config_cache 缓存，热路径）：function 查表优先
                &super::models_sync::load_models(&state.data_dir),
            );

            // 分级重试（T2.2/F-33，与 wb_route 同一张表）：same_attempt 为同账号
            // 重试计数，换号后随新账号归零；总轮换上限仍受 MAX_ROTATE 约束
            let mut same_attempt: u32 = 0;
            loop {
                // TTFB 计时（请求发起 → 上游首行到达，与 wb_route 同语义）：
                // AtomicU64 0 哨兵 = 尚未读到首行（首字超时等失败路径不产出 ttfb）
                let ttfb_start = std::time::Instant::now();
                let ttfb_us = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                match make_upstream_request(&picked.jwt, &picked.uid, &picked.device_id, &picked.machine_id, &converted) {
                    Ok(reader) => {
                        // 首字超时 10s（T2.7/F-34，与 wb_upstream 同款包装）：建连后
                        // 首字节 10s 未到视为上游故障 → 冷却换号；首字节到达后正常
                        // 流速不受限（后续行无超时）
                        let lines = match super::wb_upstream::lines_with_first_byte_timeout(reader) {
                            Ok(l) => l,
                            Err(()) => {
                                state.pool.note_error(&picked.uid, ErrKind::Server);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} first-byte timeout(10s)", picked.uid));
                                // P2 修复：失败尝试记账（与非流式/WB 口径一致）
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0,
                                );
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
                        // 首行打点包装：首次成功读到上游行即记录 ttfb（0 哨兵防重复
                        // 覆盖；叠加首字超时包装，语义为「请求发起 → 首行到达」）
                        let lines = {
                            let ttfb_flag = ttfb_us.clone();
                            lines.map(move |l| {
                                let _ = ttfb_flag.compare_exchange(
                                    0,
                                    ttfb_start.elapsed().as_micros() as u64,
                                    std::sync::atomic::Ordering::Relaxed,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                l
                            })
                        };
                        let lines =
                            Box::new(lines) as Box<dyn Iterator<Item = String> + Send>;
                        // 连接成功 → 开始流式转换，mid-stream error 只冷却不轮换
                        let (error_info, sent_any, up_usage) = match proto {
                            Protocol::OpenAi => {
                                let (e, s, u) = sse::stream_convert_lines(lines, tx.clone(), &chat_id);
                                (e, s, u)
                            }
                            Protocol::OpenAiText => {
                                let (e, s, u) =
                                    sse::stream_convert_text_lines(lines, tx.clone(), &chat_id, &model);
                                (e, s, u)
                            }
                            Protocol::Anthropic => {
                                let (e, s, u) =
                                    sse::stream_convert_anthropic_lines(lines, tx.clone(), &chat_id, &model);
                                (e, s, u)
                            }
                            // Responses 仅走 WB 上游；solo 管线兜底按 OpenAI 透传
                            Protocol::Responses => {
                                let (e, s, u) = sse::stream_convert_lines(lines, tx.clone(), &chat_id);
                                (e, s, u)
                            }
                        };
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        // TTFB：首行到达耗时（未读到首行 → None，不输出该字段）
                        let ttfb_ms = {
                            let us = ttfb_us.load(std::sync::atomic::Ordering::Relaxed);
                            if us == 0 { None } else { Some(us / 1000) }
                        };
                        // 用量记账（流式结束即落盘；F-76① TTFT 入账）
                        {
                            let (pt, ct) = up_usage.as_ref().map(extract_tokens).unwrap_or((0, 0));
                            state.record_usage_ttfb(
                                false, &model, &picked.uid, &key_id, error_info.is_none(), true,
                                duration_ms, pt, ct, ttfb_ms,
                            );
                        }
                        if let Some((code, msg)) = error_info {
                            let kind = classify_solo_error(code, &msg);
                            if kind != ErrKind::None {
                                state.pool.note_error(&picked.uid, kind);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} code={} msg={}", picked.uid, code, msg));
                            }
                            if !sent_any {
                                // 流未开始：错误延迟下发（sse 层未透传，由这里统一发）
                                send_stream_error(&tx, proto, code, &msg);
                            }
                            state.logger.log_request_ttfb(
                                "trae", "POST", proto.log_path(), &model, stream,
                                200, &picked.uid, duration_ms, ttfb_ms, &key_name,
                                &state.pool.name_of(&picked.uid), Some(&msg),
                            );
                            if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                state.logger.log_debug(&picked.uid, &converted, None, 200, Some(&msg));
                            }
                        } else {
                            state.pool.note_success(&picked.uid);
                            state.logger.log_request_ttfb(
                                "trae", "POST", proto.log_path(), &model, stream,
                                200, &picked.uid, duration_ms, ttfb_ms, &key_name,
                                &state.pool.name_of(&picked.uid), None,
                            );
                            if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                state.logger.log_debug(&picked.uid, &converted, None, 200, None);
                            }
                        }
                        return; // 流式结束后直接返回
                    }
                    Err((status, resp_body, retry_after)) => {
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
                                let preview = safe_slice(&resp_body, 200);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} status={} body={}", picked.uid, status, preview));
                                // P2 修复：失败尝试记账（与非流式/WB 口径一致）
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0,
                                );
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

        // 所有账号不可用
        let duration_ms = start_ts.elapsed().as_millis() as u64;
        state.record_usage(false, &model, "none", &key_id, false, true, duration_ms, 0, 0);
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
            Some("no healthy account"),
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
            let diag_line = format!(
                "NO_HEALTHY_ACCOUNT [{:02}:{:02}:{:02}] tried={} pool={} reasons=[{}]",
                h, m, s, tried.len(), diag.len(), diag_summary.join(", "),
            );
            state.logger.log_debug_line(format!("[DEBUG] {diag_line}"));
        }
        match proto {
            Protocol::OpenAi | Protocol::OpenAiText => {
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(
                    "data: {\"error\":{\"message\":\"no healthy account available\",\"type\":\"api_error\",\"code\":\"no_healthy_account\"}}\n\n",
                )));
                let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
            }
            Protocol::Anthropic => {
                let err = json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "no healthy account available",
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
                        "error": {"code": "no_healthy_account", "message": "no healthy account available"},
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
        let mut tried = HashSet::new();
        // 401 自愈去重（issue #27 方案 B）：每账号每请求最多强制刷新一次（对齐 wb_route T2.6）
        let mut refreshed_401 = HashSet::new();

        for _ in 0..MAX_ROTATE {
            let mut picked = match state
                .pool
                .pick_excluding_constrained(&tried, trae_allowed.as_ref(), trae_dedicated.as_deref())
            {
                Some(p) => p,
                None => break,
            };
            tried.insert(picked.uid.clone());
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());
            // F-77 账号级在途计数：取号即绑定（换号时自动解绑旧账号）
            guard = guard.bind_account(state.pool.inflight_handle(&picked.uid));

            let converted = super::payload::prepare_llm_chat_body(
                &body_vec, &state.default_model, &picked.uid, &picked.device_id, &picked.machine_id,
                // 模型目录（config_cache 缓存，热路径）：function 查表优先
                &super::models_sync::load_models(&state.data_dir),
            );

            // 分级重试（T2.2/F-33，与 wb_route 同一张表）：same_attempt 为同账号
            // 重试计数，换号后随新账号归零；总轮换上限仍受 MAX_ROTATE 约束
            let mut same_attempt: u32 = 0;
            loop {
                match make_upstream_request(&picked.jwt, &picked.uid, &picked.device_id, &picked.machine_id, &converted) {
                    Ok(reader) => {
                        let chat_id = match proto {
                            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
                            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
                            Protocol::Anthropic => format!("msg_{}", now_ts()),
                            Protocol::Responses => format!("resp_{}", now_ts()),
                        };
                        let (resp, error_info) = match proto {
                            Protocol::OpenAi => sse::aggregate(reader, &chat_id),
                            Protocol::OpenAiText => sse::aggregate_text(reader, &chat_id, &model),
                            Protocol::Anthropic => sse::aggregate_anthropic(reader, &chat_id, &model),
                            // Responses 仅走 WB 上游；solo 管线兜底按 OpenAI 聚合
                            Protocol::Responses => sse::aggregate(reader, &chat_id),
                        };
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        match (resp, error_info) {
                            (Some(r), None) => {
                                // 用量记账（成功：token 数从聚合响应 usage 提取）
                                let (pt, ct) = r.get("usage").map(extract_tokens).unwrap_or((0, 0));
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, true, stream,
                                    duration_ms, pt, ct,
                                );
                                state.pool.note_success(&picked.uid);
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
                                let kind = classify_solo_error(code, &msg);
                                state.pool.note_error(&picked.uid, kind);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} code={} msg={}", picked.uid, code, msg));
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, false, stream,
                                    duration_ms, 0, 0,
                                );
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
                                    //（note_error(None) 不冷却，账号健康度不受影响）
                                    return Err(AggregateFail::Upstream(
                                        400,
                                        format!("upstream code={} msg={}", code, safe_slice(&msg, 300)),
                                    ));
                                }
                                break; // 账号级错误：冷却换号
                            }
                            _ => {
                                state.pool.note_error(&picked.uid, ErrKind::Server);
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, false, stream,
                                    duration_ms, 0, 0,
                                );
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
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} status={}", picked.uid, status));
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0,
                                );
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
            Some("no healthy account"),
        );
        // 写入诊断日志
        {
            state.logger.log_debug_line(format!(
                "[DEBUG] NO_HEALTHY_ACCOUNT(non-stream) tried={} pool={} reasons=[{}]",
                tried.len(), diag.len(), diag_summary.join(", "),
            ));
        }
        Err(AggregateFail::NoHealthy("no healthy account available".to_string()))
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

fn make_upstream_request(
    jwt: &str,
    _uid: &str,
    device_id: &str,
    machine_id: &str,
    body: &[u8],
) -> Result<Box<dyn Read + Send>, (u16, String, Option<u64>)> {
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
        Ok(r) => Ok(Box::new(r.into_reader())),
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

/// OpenAI 错误响应格式（wb_route 复用）
pub(crate) fn openai_error(status: StatusCode, code: &str, msg: &str) -> Response {    let body = json!({
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
            wb_enabled: std::sync::atomic::AtomicBool::new(true),
            wb_sanitize: std::sync::atomic::AtomicBool::new(true),
            wb_default_thinking: std::sync::atomic::AtomicBool::new(false),
            wb_tool_exec: std::sync::atomic::AtomicBool::new(false),
            wb_bg_downgrade: std::sync::atomic::AtomicBool::new(false),
            wb_longctx_downgrade: std::sync::atomic::AtomicBool::new(false),
            wb_hedge_threshold_ms: std::sync::atomic::AtomicU64::new(0),
            account_concurrency_limit: std::sync::atomic::AtomicU32::new(0),
            pool_sticky_ttl_secs: std::sync::atomic::AtomicU64::new(300),
            wb_sticky: super::super::wb_sticky::StickyStore::default(),
            model_cooldowns: std::sync::Mutex::new(std::collections::HashMap::new()),
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
            trae_jwt_refresh: None,
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
