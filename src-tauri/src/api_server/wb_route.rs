//! WorkBuddy 上游请求路径（T2.1/T2.4/T2.7 集成点）
//!
//! 请求流程：
//! 1. 模型级冷却检查（F-34：优先级高于 Key 级，命中直接快速失败）；
//! 2. 会话粘性双模式（T2.4/F-31）：显式 conversationId / 前 3 消息指纹 60s 窗，
//!    命中绑定则锁定账号与上游会话（双段分配）；
//! 3. 上游请求（headers 三铁律 + 强制 stream + effort 降级 + 审核模板黑名单
//!    最小改写）；
//! 4. 分级重试（T2.2/F-33）：RetrySame 同号重试 / SwitchKey 换号（401 先刷新
//!    一次凭证，T2.6）/ Fatal 透传终止；
//! 5. SSE keep-alive 15s（T2.7/F-34）+ 首字超时 10s 故障转移；
//! 6. 用量记账 + 请求级日志（含 TTFB，F-32）。
//!
//! 客户端断连（F-34 §5.5 #8）：wb_sse 层对 send 失败（接收端已 drop）保持
//! 消费上游直到 EOF——usage 完整记账，等效 `_drain_upstream`。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use super::pool::PickedAccount;
use super::retry::{retry_plan, RetryAction};
use super::wb_payload;
use super::wb_sse;
use super::wb_sticky::SessionKey;
use super::wb_upstream::{self, WbCreds};
use super::{classify_error, ApiSharedState, ErrKind, InflightGuard};
use crate::api_server::routes::{anthropic_error, openai_error, Protocol};

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 生成上游 conversation_id（粘性绑定的第二段：客户端会话 → 上游会话）
pub(crate) fn gen_conv_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seed = (nanos as u64).wrapping_mul(0x517cc1b727220a95);
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&seed.to_le_bytes());
    buf[8..16].copy_from_slice(&(seed.wrapping_add(0x9e3779b97f4a7c15)).to_le_bytes());
    let hex: String = buf.iter().map(|b| format!("{:02x}", b)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]
    )
}

// ==================== 模型级冷却（T2.7/F-34） ====================

/// 模型是否处于冷却中；返回剩余秒数
pub fn model_cooling_remaining(state: &ApiSharedState, model: &str) -> Option<i64> {
    let map = safe_lock(&state.model_cooldowns);
    let (until, _) = map.get(model)?;
    let now = now_ts();
    if *until > now {
        Some(*until - now)
    } else {
        None
    }
}

/// 记录一次模型级失败：渐进退避 10→20→40s（封顶 40s，成功请求后清除）
pub fn note_model_failure(state: &ApiSharedState, model: &str) {
    let mut map = safe_lock(&state.model_cooldowns);
    let (_, fails) = map.get(model).copied().unwrap_or((0, 0));
    let fails = fails + 1;
    let delay = 10i64 << (fails - 1).min(2); // 10/20/40
    map.insert(model.to_string(), (now_ts() + delay, fails));
}

/// 请求成功后清除该模型的冷却与失败计数
pub fn clear_model_failure(state: &ApiSharedState, model: &str) {
    safe_lock(&state.model_cooldowns).remove(model);
}

// ==================== 审核模板映射热更新（T2.1） ====================

/// 每请求读取外置映射表（fs_utils::read_json_cached：mtime+size 线程安全缓存，
/// 免自建缓存结构与 Mutex 内磁盘 IO）；data/ 新路径缺失回退旧根路径（存量数据
/// 兼容）；文件缺失/损坏/空表用内置兜底
pub fn load_templates(state: &ApiSharedState) -> Vec<(String, String)> {
    let new_path = wb_payload::template_map_path(&state.data_dir);
    let effective = if new_path.exists() {
        new_path
    } else {
        wb_payload::template_map_path_legacy(&state.data_dir)
    };
    crate::fs_utils::read_json_cached::<wb_payload::TemplateMapFile>(&effective)
        .and_then(|f| f.into_rules())
        .unwrap_or_else(wb_payload::default_template_map)
}

// ==================== 错误分类（WB 上游） ====================

/// WB 流内错误 → ErrKind：WB 无 SOLO 业务码体系，按类 HTTP 状态码 +
/// message 关键词判定（积分耗尽标记词表集中维护，F-33）
pub fn classify_wb_error(code: i64, msg: &str) -> ErrKind {
    let lower = msg.to_lowercase();
    if code == 401 || lower.contains("unauthorized") || lower.contains("token expired") {
        return ErrKind::SessionDead;
    }
    if code == 403 || lower.contains("forbidden") || lower.contains("banned") {
        return ErrKind::Forbidden;
    }
    if lower.contains("insufficient credit")
        || lower.contains("积分不足")
        || lower.contains("额度不足")
        || lower.contains("quota exceeded")
    {
        return ErrKind::HardCredit;
    }
    if code == 429 || lower.contains("rate") || lower.contains("too many") || lower.contains("限频") {
        return ErrKind::SoftRate;
    }
    if code >= 500 {
        return ErrKind::Server;
    }
    // 未识别的 4xx：客户端侧错误（换号重试无意义），Client 短冷却即可；
    // 归 Server 会误触 30m 账号熔断。code<400 的未知业务码维持 Server 保守判定。
    if code >= 400 {
        return ErrKind::Client;
    }
    ErrKind::Server
}

// ==================== 流式入口 ====================

pub fn wb_stream_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::task::spawn_blocking(move || {
        // inflight guard 随后台任务存续至流结束（§4.5，客户端断连由 Drop 兜底）
        let _inflight = guard;
        let chat_id = match proto {
            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
            Protocol::Anthropic => format!("msg_{}", now_ts()),
            Protocol::Responses => format!("resp_{}", now_ts()),
        };

        // SSE keep-alive 15s（T2.7/F-34 §5.5 #7：防中间层回收长流）；
        // 主任务结束置 done 退出，客户端断连后发送失败自然退出
        let done = Arc::new(AtomicBool::new(false));
        {
            let tx2 = tx.clone();
            let done2 = done.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
                tick.tick().await; // 首个 tick 立即返回，跳过
                loop {
                    tick.tick().await;
                    if done2.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    if tx2
                        .send(Ok(bytes::Bytes::from(": keep-alive\n\n")))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }

        run_wb_stream(&state, &body_vec, &model, proto, &key_id, &chat_id, &tx, start_ts);
        done.store(true, std::sync::atomic::Ordering::Relaxed);
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

#[allow(clippy::too_many_arguments)]
fn run_wb_stream(
    state: &Arc<ApiSharedState>,
    body_vec: &[u8],
    model: &str,
    proto: Protocol,
    key_id: &str,
    chat_id: &str,
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    start_ts: Instant,
) {
    let peek: Value = serde_json::from_slice(body_vec).unwrap_or(json!({}));
    let sticky_key = SessionKey::from_body(&peek);
    let templates = load_templates(state);
    let sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);

    // F-35 子 Key 约束：限定上游 + 专一/临期优先（匿名/无约束 Key 全空 → 走默认调度）
    let key_constraints = super::api_keys::constraints_for(&state.data_dir, key_id);
    let allowed_set: Option<HashSet<String>> = key_constraints
        .as_ref()
        .map(|k| k.allowed_accounts.iter().cloned().collect())
        .filter(|s: &HashSet<String>| !s.is_empty());
    let dedicated: Option<String> = key_constraints
        .as_ref()
        .filter(|k| k.schedule_mode == super::api_keys::MODE_DEDICATED)
        .map(|k| {
            if k.dedicated_account.is_empty() {
                k.allowed_accounts.first().cloned().unwrap_or_default()
            } else {
                k.dedicated_account.clone()
            }
        })
        .filter(|s: &String| !s.is_empty());

    // 粘性首轮：命中绑定且账号 healthy → 锁定账号与上游会话（双段分配）；
    // 子 Key 限定上游不含粘性账号时忽略粘性
    let sticky0: Option<(String, String)> = state
        .wb_sticky
        .resolve(&sticky_key, now_ts())
        .and_then(|b| {
            if allowed_set.as_ref().map_or(false, |a| !a.contains(&b.uid)) {
                return None;
            }
            state
                .wb_pool
                .pick_by_uid(&b.uid)
                .map(|_| (b.uid, b.conv_id))
        });
    let sticky_uid: Option<String> = sticky0.as_ref().map(|(u, _)| u.clone());
    let sticky_conv: String = sticky0
        .as_ref()
        .map(|(_, c)| c.clone())
        .unwrap_or_default();
    // 首选：粘性 > 专一绑定 > 调度策略
    let mut first_pick: Option<PickedAccount> = sticky0
        .as_ref()
        .and_then(|(u, _)| state.wb_pool.pick_by_uid(u))
        .or_else(|| dedicated.as_deref().and_then(|uid| state.wb_pool.pick_by_uid(uid)));

    let mut tried: HashSet<String> = HashSet::new();
    let mut refreshed: HashSet<String> = HashSet::new(); // 401 刷新每账号一次

    loop {
        // ── 取号：粘性/专一命中优先，否则按 Key 约束 + 调度策略；换号后仅走策略 ──
        let picked = match first_pick.take() {
            Some(p) => p,
            None => match state.wb_pool.pick_excluding_constrained(&tried, allowed_set.as_ref(), dedicated.as_deref()) {
                Some(p) => p,
                None => {
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    state.record_usage(true, model, "none", key_id, false, true, duration_ms, 0, 0);
                    state.logger.log_request(
                        "buddy", "POST", "/v2/chat/completions", model, true, 503, "none",
                        duration_ms, Some("no healthy account"),
                    );
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from(
                        "data: {\"error\":{\"message\":\"no healthy account available\",\"type\":\"api_error\",\"code\":\"no_healthy_account\"}}\n\n",
                    )));
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                    return;
                }
            },
        };
        tried.insert(picked.uid.clone());
        *safe_lock(&state.active_uid) = Some(picked.uid.clone());

        // 上游会话 id：粘性命中复用，否则新生成（成功后绑定）
        let is_sticky_hit = sticky_uid.as_deref() == Some(picked.uid.as_str()) && !sticky_conv.is_empty();
        let conv_id = if is_sticky_hit {
            sticky_conv.clone()
        } else {
            gen_conv_id()
        };

        let catalog = super::wb_catalog::load(&state.data_dir);
        let effort = super::wb_catalog::find(&catalog, model)
            .and_then(|m| m.resolve_effort(peek.get("reasoning_effort").and_then(|v| v.as_str())));
        let converted = wb_payload::prepare_wb_chat_body(
            body_vec, model, &conv_id, effort.as_deref(), sanitize, &templates,
        );
        let mut creds = WbCreds {
            id: picked.uid.clone(),
            uid: picked.uid.clone(),
            name: String::new(),
            token: picked.jwt.clone(),
            domain: picked.domain.clone(),
            enterprise_id: picked.enterprise_id.clone(),
            global_region: picked.global_region,
        };

        let mut same_attempt: u32 = 0;
        loop {
            let ttfb_start = Instant::now();
            match wb_upstream::make_wb_request(&creds, &converted) {
                Ok(reader) => {
                    // 首字超时 10s（T2.7/F-34）：超时视为上游故障 → 换号
                    let lines = match wb_upstream::lines_with_first_byte_timeout(reader) {
                        Ok(l) => l,
                        Err(()) => {
                            state.wb_pool.note_error(&picked.uid, ErrKind::Server);
                            note_model_failure(state, model);
                            break;
                        }
                    };
                    let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                    let (error_info, sent_any, failed_inline, usage) =
                        wb_sse::stream_forward_ex(lines, tx, proto, chat_id, model);
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    {
                        let (pt, ct) = usage
                            .as_ref()
                            .map(|u| {
                                (
                                    u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                    u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                )
                            })
                            .unwrap_or((0, 0));
                        state.record_usage(true, model, &picked.uid, key_id, error_info.is_none() && !failed_inline, true, duration_ms, pt, ct);
                    }
                    match error_info {
                        Some((code, msg)) => {
                            let kind = classify_wb_error(code, &msg);
                            if kind != ErrKind::None {
                                state.wb_pool.note_error(&picked.uid, kind);
                                note_model_failure(state, model);
                                *safe_lock(&state.last_error) =
                                    Some(format!("wb uid={} code={} msg={}", picked.uid, code, msg));
                            }
                            if !sent_any {
                                // 流未开始：错误不下发，允许换号重试
                                break;
                            }
                            state.logger.log_request_ttfb(
                                "buddy", "POST", "/v2/chat/completions", model, true, 200, &picked.uid,
                                duration_ms, Some(ttfb_ms), Some(&msg),
                            );
                            return; // 已有数据流出：就地收尾
                        }
                        None => {
                            if failed_inline {
                                // 流内失败已就地透传客户端（response.failed / error 事件
                                // 已下发）：不重试、不 note_success、不清模型冷却、不绑定
                                // 粘性会话；亦不 note_error——错误已原样给到客户端，内容类
                                // 失败计入冷却会造成账号过度冷却
                                state.logger.log_request_ttfb(
                                    "buddy", "POST", "/v2/chat/completions", model, true, 200, &picked.uid,
                                    duration_ms, Some(ttfb_ms), Some("流内失败已透传客户端"),
                                );
                                return;
                            }
                            state.wb_pool.note_success(&picked.uid);
                            clear_model_failure(state, model);
                            // 绑定粘性会话（Mutex 内 re-check 防 TOCTOU）
                            state.wb_sticky.bind(&sticky_key, &picked.uid, &conv_id, now_ts());
                            state.wb_sticky.save(&state.data_dir);
                            state.logger.log_request_ttfb(
                                "buddy", "POST", "/v2/chat/completions", model, true, 200, &picked.uid,
                                duration_ms, Some(ttfb_ms), None,
                            );
                            return;
                        }
                    }
                }
                Err((status, resp_body, retry_after)) => {
                    // 分级重试策略表（T2.2/F-33 v1.2）
                    match retry_plan(status, &resp_body, same_attempt, retry_after) {
                        RetryAction::RetrySame { delay_ms } => {
                            same_attempt += 1;
                            std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(60_000)));
                            continue;
                        }
                        RetryAction::SwitchKey => {
                            // T2.6：401 → 刷新一次凭证后同号重试（每账号每请求一次）
                            if status == 401 && !refreshed.contains(&picked.uid) {
                                refreshed.insert(picked.uid.clone());
                                match wb_upstream::refresh_access_token(&state.data_dir, &picked.uid) {
                                    Ok(new_token) => {
                                        state.wb_pool.update_jwt(&picked.uid, &new_token);
                                        creds.token = new_token;
                                        continue;
                                    }
                                    Err(e) => {
                                        *safe_lock(&state.last_error) =
                                            Some(format!("wb refresh uid={} err={}", picked.uid, e));
                                    }
                                }
                            }
                            let kind = classify_error(status, &resp_body);
                            state.wb_pool.note_error(&picked.uid, kind);
                            note_model_failure(state, model);
                            *safe_lock(&state.last_error) = Some(format!(
                                "wb uid={} status={} body={}",
                                picked.uid,
                                status,
                                safe_slice(&resp_body, 200)
                            ));
                            state.logger.log_request(
                                "buddy", "POST", "/v2/chat/completions", model, true, status, &picked.uid,
                                start_ts.elapsed().as_millis() as u64,
                                Some(&format!("upstream status={}", status)),
                            );
                            break; // 换号
                        }
                        RetryAction::Fatal => {
                            let msg = format!("upstream {} error: {}", status, safe_slice(&resp_body, 300));
                            state.logger.log_request(
                                "buddy", "POST", "/v2/chat/completions", model, true, status, &picked.uid,
                                start_ts.elapsed().as_millis() as u64,
                                Some(&msg),
                            );
                            send_stream_error_wb(tx, proto, status as i64, &msg);
                            return;
                        }
                    }
                }
            }
        }
    }
}

// ==================== 非流式（上游只回 SSE → 本地聚合） ====================

pub async fn wb_aggregate_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    stream: bool,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let model_out = model.clone();
    let result = tokio::task::spawn_blocking(move || {
        // inflight guard 随后台任务存续至聚合完成（§4.5）
        let _inflight = guard;
        let peek: Value = serde_json::from_slice(&body_vec).unwrap_or(json!({}));
        let sticky_key = SessionKey::from_body(&peek);
        let templates = load_templates(&state);
        let sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);

        // F-35 子 Key 约束（与非流式同款）
        let key_constraints = super::api_keys::constraints_for(&state.data_dir, &key_id);
        let allowed_set: Option<HashSet<String>> = key_constraints
            .as_ref()
            .map(|k| k.allowed_accounts.iter().cloned().collect())
            .filter(|s: &HashSet<String>| !s.is_empty());
        let dedicated: Option<String> = key_constraints
            .as_ref()
            .filter(|k| k.schedule_mode == super::api_keys::MODE_DEDICATED)
            .map(|k| {
                if k.dedicated_account.is_empty() {
                    k.allowed_accounts.first().cloned().unwrap_or_default()
                } else {
                    k.dedicated_account.clone()
                }
            })
            .filter(|s: &String| !s.is_empty());

        let sticky0: Option<(String, String)> = state
            .wb_sticky
            .resolve(&sticky_key, now_ts())
            .and_then(|b| {
                if allowed_set.as_ref().map_or(false, |a| !a.contains(&b.uid)) {
                    return None;
                }
                state
                    .wb_pool
                    .pick_by_uid(&b.uid)
                    .map(|_| (b.uid, b.conv_id))
            });
        let sticky_uid: Option<String> = sticky0.as_ref().map(|(u, _)| u.clone());
        let sticky_conv: String = sticky0
            .as_ref()
            .map(|(_, c)| c.clone())
            .unwrap_or_default();
        let mut first_pick: Option<PickedAccount> = sticky0
            .as_ref()
            .and_then(|(u, _)| state.wb_pool.pick_by_uid(u))
            .or_else(|| dedicated.as_deref().and_then(|uid| state.wb_pool.pick_by_uid(uid)));

        let mut tried: HashSet<String> = HashSet::new();
        let mut refreshed: HashSet<String> = HashSet::new();

        loop {
            let picked = match first_pick.take() {
                Some(p) => p,
                None => match state.wb_pool.pick_excluding_constrained(&tried, allowed_set.as_ref(), dedicated.as_deref()) {
                    Some(p) => p,
                    None => {
                        state.record_usage(true, &model, "none", &key_id, false, stream,
                            start_ts.elapsed().as_millis() as u64, 0, 0);
                        return Err("no healthy account available".to_string());
                    }
                },
            };
            tried.insert(picked.uid.clone());
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            let is_sticky_hit =
                sticky_uid.as_deref() == Some(picked.uid.as_str()) && !sticky_conv.is_empty();
            let conv_id = if is_sticky_hit {
                sticky_conv.clone()
            } else {
                gen_conv_id()
            };

            let catalog = super::wb_catalog::load(&state.data_dir);
            let effort = super::wb_catalog::find(&catalog, &model)
                .and_then(|m| m.resolve_effort(peek.get("reasoning_effort").and_then(|v| v.as_str())));
            let converted = wb_payload::prepare_wb_chat_body(
                &body_vec, &model, &conv_id, effort.as_deref(), sanitize, &templates,
            );
            let mut creds = WbCreds {
                id: picked.uid.clone(),
                uid: picked.uid.clone(),
                name: String::new(),
                token: picked.jwt.clone(),
                domain: picked.domain.clone(),
                enterprise_id: picked.enterprise_id.clone(),
                global_region: picked.global_region,
            };

            let mut same_attempt: u32 = 0;
            loop {
                match wb_upstream::make_wb_request(&creds, &converted) {
                    Ok(reader) => {
                        // 首字超时：非流式聚合同样适用（上游只回 SSE）
                        let lines = match wb_upstream::lines_with_first_byte_timeout(reader) {
                            Ok(l) => l,
                            Err(()) => {
                                state.wb_pool.note_error(&picked.uid, ErrKind::Server);
                                note_model_failure(&state, &model);
                                break;
                            }
                        };
                        let (resp, error_info) =
                            wb_sse::aggregate(lines, &format!("chatcmpl-{}", now_ts()));
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        match (resp, error_info) {
                            (Some(mut r), None) => {
                                r["model"] = json!(model);
                                let (pt, ct) = r.get("usage").map(|u| (
                                    u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                    u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                )).unwrap_or((0, 0));
                                state.record_usage(true, &model, &picked.uid, &key_id, true, stream, duration_ms, pt, ct);
                                state.wb_pool.note_success(&picked.uid);
                                clear_model_failure(&state, &model);
                                state.wb_sticky.bind(&sticky_key, &picked.uid, &conv_id, now_ts());
                                state.wb_sticky.save(&state.data_dir);
                                state.logger.log_request(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, 200, &picked.uid,
                                    duration_ms, None,
                                );
                                return Ok(r);
                            }
                            (None, Some((code, msg))) => {
                                let kind = classify_wb_error(code, &msg);
                                if kind != ErrKind::None {
                                    state.wb_pool.note_error(&picked.uid, kind);
                                    note_model_failure(&state, &model);
                                }
                                *safe_lock(&state.last_error) =
                                    Some(format!("wb uid={} code={} msg={}", picked.uid, code, msg));
                                state.record_usage(true, &model, &picked.uid, &key_id, false, stream, duration_ms, 0, 0);
                                state.logger.log_request(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, 200, &picked.uid,
                                    duration_ms, Some(&msg),
                                );
                                // 流内错误且未产出内容 → 换号重试
                                break;
                            }
                            _ => {
                                state.wb_pool.note_error(&picked.uid, ErrKind::Server);
                                note_model_failure(&state, &model);
                                state.record_usage(true, &model, &picked.uid, &key_id, false, stream, duration_ms, 0, 0);
                                state.logger.log_request(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, 502, &picked.uid,
                                    duration_ms, Some("empty response"),
                                );
                                break;
                            }
                        }
                    }
                    Err((status, resp_body, retry_after)) => {
                        match retry_plan(status, &resp_body, same_attempt, retry_after) {
                            RetryAction::RetrySame { delay_ms } => {
                                same_attempt += 1;
                                std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(60_000)));
                                continue;
                            }
                            RetryAction::SwitchKey => {
                                if status == 401 && !refreshed.contains(&picked.uid) {
                                    refreshed.insert(picked.uid.clone());
                                    match wb_upstream::refresh_access_token(&state.data_dir, &picked.uid) {
                                        Ok(new_token) => {
                                            state.wb_pool.update_jwt(&picked.uid, &new_token);
                                            creds.token = new_token;
                                            continue;
                                        }
                                        Err(e) => {
                                            *safe_lock(&state.last_error) =
                                                Some(format!("wb refresh uid={} err={}", picked.uid, e));
                                        }
                                    }
                                }
                                let kind = classify_error(status, &resp_body);
                                state.wb_pool.note_error(&picked.uid, kind);
                                note_model_failure(&state, &model);
                                *safe_lock(&state.last_error) =
                                    Some(format!("wb uid={} status={}", picked.uid, status));
                                state.record_usage(true, &model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0);
                                state.logger.log_request(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64,
                                    Some(&format!("upstream status={}", status)),
                                );
                                break;
                            }
                            RetryAction::Fatal => {
                                state.logger.log_request(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64,
                                    Some(&safe_slice(&resp_body, 300)),
                                );
                                return Err(format!(
                                    "upstream {} error: {}",
                                    status,
                                    safe_slice(&resp_body, 300)
                                ));
                            }
                        }
                    }
                }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(resp)) => {
            let body = match proto {
                Protocol::Anthropic => wb_sse::completion_to_anthropic(
                    &resp,
                    &format!("msg_{}", now_ts()),
                    &model_out,
                ),
                Protocol::OpenAiText => wb_sse::completion_to_text(&resp, &model_out),
                Protocol::Responses => super::wb_responses::completion_to_responses(
                    &resp,
                    &format!("resp_{}", now_ts()),
                    &model_out,
                ),
                Protocol::OpenAi => resp,
            };
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|_| internal_error_response())
        }
        Ok(Err(msg)) => match proto {
            Protocol::Anthropic => anthropic_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", &msg),
            _ => openai_error(StatusCode::SERVICE_UNAVAILABLE, "no_healthy_account", &msg),
        },
        Err(e) => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("task join error: {}", e),
        ),
    }
}

// ==================== T5.5/F-64 网关工具代执行编排 ====================

/// Responses 工具代执行主流程（仅 /v1/responses 且声明 web_search 类工具时进入）：
/// 上游请求内部固定非流式（stream:false）→ 聚合 → 提取代执行工具调用 → 本地执行
/// → 结果回喂 → 循环直至最终回复（上限 MAX_ROUNDS 防积分失控）。
///
/// 输出投影：
/// - 历史代执行搜索轮 → 原生 `web_search_call` 输出项（置于 message 之前）；
/// - 最终回复 → 既有 completion_to_responses 投影；
/// - 客户端 stream=true 时按 Responses SSE 事件序列（created → items → completed）
///   由聚合结果合成下发；stream=false 直接返回 JSON。
///
/// 简化说明：本路径不复用粘性绑定（多轮工具回喂非单轮会话语义），账号选取与
/// Key 约束/分级重试与非流式管线同一套规则。
pub async fn wb_tool_exec_chat(
    state: Arc<ApiSharedState>,
    mut chat_body: Value,
    model: String,
    stream: bool,
    start_ts: Instant,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let model_inner = model.clone();
    let result = tokio::task::spawn_blocking(move || {
        // inflight guard 随后台任务存续至编排完成（§4.5）
        let _inflight = guard;
        let model = model_inner;
        let templates = load_templates(&state);
        let sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);
        super::wb_toolexec::inject_proxy_tools(&mut chat_body);

        // F-35 子 Key 约束（与非流式同款）
        let key_constraints = super::api_keys::constraints_for(&state.data_dir, &key_id);
        let allowed_set: Option<HashSet<String>> = key_constraints
            .as_ref()
            .map(|k| k.allowed_accounts.iter().cloned().collect())
            .filter(|s: &HashSet<String>| !s.is_empty());
        let dedicated: Option<String> = key_constraints
            .as_ref()
            .filter(|k| k.schedule_mode == super::api_keys::MODE_DEDICATED)
            .map(|k| {
                if k.dedicated_account.is_empty() {
                    k.allowed_accounts.first().cloned().unwrap_or_default()
                } else {
                    k.dedicated_account.clone()
                }
            })
            .filter(|s: &String| !s.is_empty());

        let catalog = super::wb_catalog::load(&state.data_dir);
        let effort = super::wb_catalog::find(&catalog, &model)
            .and_then(|m| m.resolve_effort(chat_body.get("reasoning_effort").and_then(|v| v.as_str())));

        let mut tried: HashSet<String> = HashSet::new();
        let mut refreshed: HashSet<String> = HashSet::new();
        let mut records: Vec<super::wb_toolexec::SearchRecord> = Vec::new();
        let mut final_completion: Option<Value> = None;
        let mut success_uid: Option<String> = None; // 审查修复：保留真实账号归因
        let mut last_err: Option<String> = None;
        let resp_id = format!("resp_{}", now_ts());

        'accounts: loop {
            let picked = match state
                .wb_pool
                .pick_excluding_constrained(&tried, allowed_set.as_ref(), dedicated.as_deref())
            {
                Some(p) => p,
                None => break,
            };
            tried.insert(picked.uid.clone());
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());
            let conv_id = gen_conv_id();
            let mut creds = WbCreds {
                id: picked.uid.clone(),
                uid: picked.uid.clone(),
                name: String::new(),
                token: picked.jwt.clone(),
                domain: picked.domain.clone(),
                enterprise_id: picked.enterprise_id.clone(),
                global_region: picked.global_region,
            };
            let mut same_attempt: u32 = 0;

            // 工具代执行轮次循环（每轮 = 一次上游请求）
            let mut round: usize = 0;
            // 各轮上游 usage 累加（含最终轮），成功时写回最终 completion——
            // 多轮回喂的真实消耗 = 各轮之和，只记最终轮会少计中间轮
            let mut acc_usage = (0u64, 0u64);
            let outcome = loop {
                if round >= super::wb_toolexec::MAX_ROUNDS {
                    break Err(format!(
                        "工具代执行轮数已达上限（{} 轮），上游仍未产出最终回复；已执行 {} 次搜索/读取",
                        super::wb_toolexec::MAX_ROUNDS,
                        records.len()
                    ));
                }
                round += 1;

                let body_bytes = serde_json::to_vec(&chat_body).unwrap_or_default();
                let converted = wb_payload::prepare_wb_chat_body(
                    &body_bytes, &model, &conv_id, effort.as_deref(), sanitize, &templates,
                );

                match wb_upstream::make_wb_request(&creds, &converted) {
                    Ok(reader) => {
                        let lines = match wb_upstream::lines_with_first_byte_timeout(reader) {
                            Ok(l) => l,
                            Err(()) => {
                                state.wb_pool.note_error(&picked.uid, ErrKind::Server);
                                note_model_failure(&state, &model);
                                break Err("上游首字超时".to_string());
                            }
                        };
                        let (completion, error_info) =
                            wb_sse::aggregate(lines, &format!("chatcmpl-{}", now_ts()));
                        if let Some((code, msg)) = error_info {
                            let kind = classify_wb_error(code, &msg);
                            if kind != ErrKind::None {
                                state.wb_pool.note_error(&picked.uid, kind);
                                note_model_failure(&state, &model);
                            }
                            *safe_lock(&state.last_error) =
                                Some(format!("wb-toolexec uid={} code={} msg={}", picked.uid, code, msg));
                            break Err(msg);
                        }
                        let Some(completion) = completion else {
                            break Err("上游返回空响应".to_string());
                        };
                        if let Some(u) = completion.get("usage") {
                            acc_usage = (
                                acc_usage.0 + u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                acc_usage.1 + u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                            );
                        }
                        let calls = super::wb_toolexec::extract_proxy_calls(&completion);
                        if calls.is_empty() {
                            // 最终回复：usage 写回全部轮次累加值（中间轮消耗计入总账）
                            let mut completion = completion;
                            match completion.get_mut("usage") {
                                Some(u) => {
                                    u["prompt_tokens"] = json!(acc_usage.0);
                                    u["completion_tokens"] = json!(acc_usage.1);
                                    if u.get("total_tokens").is_some() {
                                        u["total_tokens"] = json!(acc_usage.0 + acc_usage.1);
                                    }
                                }
                                None => {
                                    completion["usage"] = json!({
                                        "prompt_tokens": acc_usage.0,
                                        "completion_tokens": acc_usage.1,
                                    });
                                }
                            }
                            break Ok(completion); // 最终回复
                        }
                        // 本地代执行 + 回喂
                        let mut tool_msgs: Vec<Value> = Vec::new();
                        let assistant_msg = completion
                            .pointer("/choices/0/message")
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        for (call_id, name, args) in &calls {
                            let (output, ok) = super::wb_toolexec::execute(name, args);
                            let query = serde_json::from_str::<Value>(args)
                                .ok()
                                .and_then(|a| {
                                    a.get("query")
                                        .or_else(|| a.get("url"))
                                        .and_then(|v| v.as_str())
                                        .map(str::to_string)
                                })
                                .unwrap_or_default();
                            records.push(super::wb_toolexec::SearchRecord {
                                tool: name.clone(),
                                query,
                                ok,
                            });
                            tool_msgs.push(json!({
                                "role": "tool",
                                "tool_call_id": call_id,
                                "content": output,
                            }));
                        }
                        if let Some(msgs) = chat_body
                            .get_mut("messages")
                            .and_then(|m| m.as_array_mut())
                        {
                            msgs.push(assistant_msg);
                            msgs.extend(tool_msgs);
                        }
                        continue;
                    }
                    Err((status, resp_body, retry_after)) => {
                        match retry_plan(status, &resp_body, same_attempt, retry_after) {
                            RetryAction::RetrySame { delay_ms } => {
                                same_attempt += 1;
                                std::thread::sleep(std::time::Duration::from_millis(
                                    delay_ms.min(60_000),
                                ));
                                continue;
                            }
                            RetryAction::SwitchKey => {
                                if status == 401 && !refreshed.contains(&picked.uid) {
                                    refreshed.insert(picked.uid.clone());
                                    match wb_upstream::refresh_access_token(&state.data_dir, &picked.uid) {
                                        Ok(new_token) => {
                                            state.wb_pool.update_jwt(&picked.uid, &new_token);
                                            creds.token = new_token;
                                            continue;
                                        }
                                        Err(e) => {
                                            *safe_lock(&state.last_error) = Some(format!(
                                                "wb-toolexec refresh uid={} err={}",
                                                picked.uid, e
                                            ));
                                        }
                                    }
                                }
                                let kind = classify_error(status, &resp_body);
                                state.wb_pool.note_error(&picked.uid, kind);
                                note_model_failure(&state, &model);
                                break Err(format!("upstream {} error: {}", status, safe_slice(&resp_body, 200)));
                            }
                            RetryAction::Fatal => {
                                break Err(format!("upstream {} error: {}", status, safe_slice(&resp_body, 300)));
                            }
                        }
                    }
                }
            };

            match outcome {
                Ok(c) => {
                    state.wb_pool.note_success(&picked.uid);
                    clear_model_failure(&state, &model);
                    success_uid = Some(picked.uid.clone());
                    final_completion = Some(c);
                    break 'accounts;
                }
                Err(e) => {
                    last_err = Some(e);
                    continue; // 换号
                }
            }
        }

        let duration_ms = start_ts.elapsed().as_millis() as u64;
        match final_completion {
            Some(mut completion) => {
                let (pt, ct) = completion.get("usage").map(|u| (
                    u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                )).unwrap_or((0, 0));
                let usage_uid = success_uid.as_deref().unwrap_or("wb-toolexec");
                state.record_usage(true, &model, usage_uid, &key_id, true, stream, duration_ms, pt, ct);
                state.logger.log_request(
                    "buddy", "POST", "/v1/responses", &model, stream, 200, usage_uid,
                    duration_ms, Some(&format!("rounds={} searches={}", records.len(), records.iter().filter(|r| r.tool == super::wb_toolexec::TOOL_SEARCH).count())),
                );
                // Responses 投影：web_search_call 历史项前置
                completion["model"] = json!(model.clone());
                let ws_items = super::wb_toolexec::search_call_items(&records, &resp_id);
                Ok((completion, ws_items, resp_id))
            }
            None => {
                state.record_usage(true, &model, "wb-toolexec", &key_id, false, stream, duration_ms, 0, 0);
                state.logger.log_request(
                    "buddy", "POST", "/v1/responses", &model, stream, 502, "wb-toolexec",
                    duration_ms, last_err.as_deref(),
                );
                Err(last_err.unwrap_or_else(|| "no healthy account available".to_string()))
            }
        }
    })
    .await;

    let (final_completion, ws_items, resp_id) = match result {
        Ok(Ok(t)) => t,
        Ok(Err(msg)) => {
            return openai_error(StatusCode::BAD_GATEWAY, "tool_exec_error", &msg);
        }
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &format!("task join error: {}", e),
            );
        }
    };

    let resp_obj = {
        let mut r = super::wb_responses::completion_to_responses(&final_completion, &resp_id, &model);
        if !ws_items.is_empty() {
            let mut output = ws_items;
            if let Some(old) = r.get("output").and_then(|o| o.as_array()).cloned() {
                output.extend(old);
            }
            r["output"] = json!(output);
        }
        r
    };

    if stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        // 由聚合结果合成 Responses SSE 事件序列（created → items → completed）
        tokio::task::spawn_blocking(move || {
            let send = |event: &str, data: &Value| {
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                    "event: {}\ndata: {}\n\n",
                    event, data
                ))));
            };
            let mut created = resp_obj.clone();
            created["status"] = json!("in_progress");
            created["output"] = json!([]);
            send("response.created", &json!({"type": "response.created", "response": created}));
            let output_items = resp_obj.get("output").and_then(|o| o.as_array()).cloned().unwrap_or_default();
            for (i, item) in output_items.iter().enumerate() {
                send("response.output_item.added", &json!({
                    "type": "response.output_item.added",
                    "output_index": i,
                    "item": item,
                }));
                // message 文本增量（整段一次下发，聚合投影无逐 token 流）
                if item.get("type").and_then(|v| v.as_str()) == Some("message") {
                    if let Some(text) = item
                        .pointer("/content/0/text")
                        .and_then(|t| t.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        send("response.output_text.delta", &json!({
                            "type": "response.output_text.delta",
                            "item_id": item.get("id").cloned().unwrap_or(json!("")),
                            "output_index": i,
                            "content_index": 0,
                            "delta": text,
                        }));
                    }
                }
                // function_call 参数增量
                if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                    if let Some(args) = item.get("arguments").and_then(|a| a.as_str()).filter(|s| !s.is_empty()) {
                        send("response.function_call_arguments.delta", &json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": item.get("id").cloned().unwrap_or(json!("")),
                            "output_index": i,
                            "delta": args,
                        }));
                    }
                }
                send("response.output_item.done", &json!({
                    "type": "response.output_item.done",
                    "output_index": i,
                    "item": item,
                }));
            }
            send("response.completed", &json!({"type": "response.completed", "response": resp_obj}));
        });
        let stream = ReceiverStream::new(rx);
        Response::builder()
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| internal_error_response())
    } else {
        Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(resp_obj.to_string()))
            .unwrap_or_else(|_| internal_error_response())
    }
}

// ==================== 小工具 ====================

fn send_stream_error_wb(
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    proto: Protocol,
    code: i64,
    msg: &str,
) {
    match proto {
        Protocol::Anthropic => {
            let err = json!({"type":"error","error":{"type":"api_error","message":msg}});
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: error\ndata: {}\n\n",
                err
            ))));
        }
        Protocol::Responses => {
            // 取号失败/无健康账号等入口错误 → response.failed（Responses 无 [DONE] 帧）
            let resp = json!({
                "id": format!("resp_{}", now_ts()),
                "object": "response",
                "status": "failed",
                "output": [],
                "error": {"code": code.to_string(), "message": msg},
            });
            let body = json!({"type": "response.failed", "response": resp});
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: response.failed\ndata: {}\n\n",
                body
            ))));
        }
        _ => {
            let body = json!({"error": { "message": msg, "type": "api_error", "code": code }});
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!("data: {}\n\n", body))));
            let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
        }
    }
}

fn internal_error_response() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
        .unwrap()
}

/// 字符边界安全截断（审查 P2-2）：字节落点在多字节字符内时回退到前一个边界，
/// 而非返回整个串（避免超长上游响应体整段进入错误消息/日志）
fn safe_slice(s: &str, n: usize) -> &str {
    if let Some(t) = s.get(..n) {
        return t;
    }
    // n 落在字符边界内：向前找最近的合法边界（最多回退 3 字节，UTF-8 最长 4 字节）
    let mut end = n.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_wb_error_ranges() {
        assert!(matches!(classify_wb_error(401, "x"), ErrKind::SessionDead));
        assert!(matches!(classify_wb_error(429, "x"), ErrKind::SoftRate));
        assert!(matches!(classify_wb_error(503, "x"), ErrKind::Server));
        // 未识别 4xx 归 Client（10m 短冷却），不误触 30m 熔断
        assert!(matches!(classify_wb_error(400, "bad request"), ErrKind::Client));
        assert!(matches!(classify_wb_error(404, "nope"), ErrKind::Client));
        // 未知业务码（<400）维持 Server 保守判定
        assert!(matches!(classify_wb_error(0, "weird"), ErrKind::Server));
        // message 关键词优先级不受 code 分段影响
        assert!(matches!(classify_wb_error(200, "积分不足"), ErrKind::HardCredit));
    }
}
