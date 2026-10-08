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
//! 客户端断连（F-34 §5.5 #8，issue #41 系列 0028c93/73f3480/5abb891 修订）：
//! 转发链路三层检测——轮换/重试入口 tx.is_closed() 快速终止、停滞期
//! next_event_polling 轮询（LINE_POLL）、活跃流逐事件顶部检测；断连即终止
//! 转发并释放账号并发槽（usage 记账取断连前已收到的上游 usage 事件，
//! 不再为残缺流量保持消费上游到 EOF）。

use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};
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

/// 记录一次模型级失败：渐进退避 10→20→40s（封顶 40s，成功请求后清除）。
/// 带错误消息时先尝试解析精确恢复时刻（note_model_failure_ex）。
pub fn note_model_failure(state: &ApiSharedState, model: &str) {
    note_model_failure_ex(state, model, None);
}

/// 记录一次模型级失败（带可选上游错误消息）：
/// ① 消息含「将在 YYYY-MM-DD HH:MM:SS UTC+8 重置」文案（WB 6004 限额，agent2api
///    errors.rs 情报互证）→ 冷却至精确恢复时刻（钳制 ≤7 天防异常值）；
/// ② 解析失败/无消息 → 既有渐进退避。后续不可解析的失败取「渐进退避 vs 既有
///    冷却截止」的较大者——账号实际仍被限额到恢复时刻，不能被 10s 渐进值缩短。
pub fn note_model_failure_ex(state: &ApiSharedState, model: &str, msg: Option<&str>) {
    let mut map = safe_lock(&state.model_cooldowns);
    let (prev_until, fails) = map.get(model).copied().unwrap_or((0, 0));
    let fails = fails + 1;
    let now = now_ts();
    let until = match msg.and_then(parse_quota_reset_at) {
        // 审查 #4：与 None 分支同口径取 max——连续两次 6004 时第二次文案的
        // 恢复时刻可能早于首次已记录的冷却截止，不得缩短既有冷却
        Some(ts) => ts.max(prev_until),
        None => {
            let delay = 10i64 << (fails - 1).min(2); // 10/20/40
            (now + delay).max(prev_until)
        }
    };
    map.insert(model.to_string(), (until, fails));
}

/// 从限额文案解析精确恢复时刻（agent2api parse_quota_reset_at 同源语义）：
/// 命中 `UTC+8` 字面量后向前扫描 `YYYY-MM-DD HH:MM:SS`（按 UTC+8 解释为 Unix 秒）。
/// 零正则依赖（项目约定）；有效性钳制：必须在未来且 ≤7 天——上游改措辞/时区
/// 即解析失败，调用方落回渐进退避（本函数只作快路径，不承担唯一职责）。
/// 复审修复：遍历**所有**锚点——文案可能含两个时刻（「您于 X UTC+8 触发限额，
/// 将在 Y UTC+8 重置」），首个锚点前是过去时刻（触发时间），跳过它继续找下一个，
/// 直到解析出可信的未来恢复时刻。
pub(crate) fn parse_quota_reset_at(msg: &str) -> Option<i64> {
    const DT_LEN: usize = 19; // yyyy-mm-dd hh:mm:ss
    let bytes = msg.as_bytes();
    let mut from = 0usize;
    loop {
        let anchor = msg[from..].find("UTC+8")? + from;
        from = anchor + "UTC+8".len();
        if anchor >= DT_LEN {
            // 从锚点紧前方回扫，最多 48 字节（文案前缀长度）
            let start_limit = anchor.saturating_sub(48);
            let mut i = anchor - DT_LEN;
            loop {
                if let Some(ts) = try_parse_dt(bytes, i) {
                    let now = now_ts();
                    // 未来且 ≤7 天才可信（解析到过去时刻/异常远期 = 该时刻不是恢复时刻）
                    if ts > now && ts - now <= 7 * 24 * 3600 {
                        return Some(ts);
                    }
                    break; // 此锚点解析出的时刻不可信 → 换下一个锚点
                }
                if i == start_limit {
                    break;
                }
                i -= 1;
            }
        }
    }
}

/// 在 bytes[i..] 处匹配 `dddd-dd-dd dd:dd:dd` 并按 UTC+8 解析为 Unix 秒
fn try_parse_dt(b: &[u8], i: usize) -> Option<i64> {
    if i + 19 > b.len() {
        return None;
    }
    let p = &b[i..i + 19];
    let digit = |x: u8| x.is_ascii_digit();
    let sep = |x: u8, c: u8| x == c;
    let num = |s: &[u8]| -> Option<i64> {
        s.iter().try_fold(0i64, |acc, c| {
            if digit(*c) { Some(acc * 10 + (*c - b'0') as i64) } else { None }
        })
    };
    let shape = sep(p[4], b'-')
        && sep(p[7], b'-')
        && sep(p[10], b' ')
        && sep(p[13], b':')
        && sep(p[16], b':')
        && p[0..4].iter().all(|c| digit(*c))
        && digit(p[5])
        && digit(p[6])
        && digit(p[8])
        && digit(p[9])
        && digit(p[11])
        && digit(p[12])
        && digit(p[14])
        && digit(p[15])
        && digit(p[17])
        && digit(p[18]);
    if !shape {
        return None;
    }
    let (y, mo, d) = (num(&p[0..4])?, num(&p[5..7])?, num(&p[8..10])?);
    let (h, mi, s) = (num(&p[11..13])?, num(&p[14..16])?, num(&p[17..19])?);
    // 按 UTC+8 解释：Unix 时刻 = naive(UTC 解释) - 8h
    chrono::NaiveDate::from_ymd_opt(y as i32, mo as u32, d as u32)
        .and_then(|date| date.and_hms_opt(h as u32, mi as u32, s as u32))
        .map(|dt| dt.and_utc().timestamp() - 8 * 3600)
}

/// 从上游 HTTP 错误响应体提取 message（6004 限额文案随 JSON body 下发）；
/// 兼容 message / error.message / msg（腾讯系接口两种字段名都有）；
/// 解析失败返回 None → 调用方落回渐进退避
fn upstream_msg(resp_body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(resp_body).ok()?;
    v.get("message")
        .and_then(|m| m.as_str())
        .or_else(|| v.pointer("/error/message").and_then(|m| m.as_str()))
        .or_else(|| v.get("msg").and_then(|m| m.as_str()))
        .map(str::to_string)
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
    // SQLite 化（P2）：data/wb_template_map.json → kv `wb_template_map`（热路径单行读取）
    crate::store::db(&state.data_dir)
        .kv_get::<wb_payload::TemplateMapFile>("wb_template_map")
        .into_rules()
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

// ==================== F-76③ 慢请求竞速对冲（取号侧编排） ====================

/// 对冲账号在途计数租约（F-76③/F-77，P0 泄漏修复）：
/// 构造即 +1（竞速窗口占用），Drop 即 -1——建连失败（闭包内 `?` 提前返回）、
/// 双败（`lines_with_first_byte_hedged` Err 路径内 Drop）、竞速胜出
/// （随 `RaceOutcome.hedge` 移交调用方，`settle_hedge` 落定后 Drop）三类
/// 退出路径均恰好释放一次，杜绝计数泄漏导致的账号永久 busy。
struct HedgeLease {
    /// 对冲账号 uid（日志 / guard 重绑定位）
    uid: String,
    /// 该账号在途计数句柄（与 `wb_pool.inflight_handle(uid)` 同一 Arc）
    counter: Arc<std::sync::atomic::AtomicU32>,
}

impl HedgeLease {
    fn acquire(state: &ApiSharedState, uid: &str) -> Self {
        let counter = state.wb_pool.inflight_handle(uid);
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { uid: uid.to_string(), counter }
    }
}

impl Drop for HedgeLease {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 首字竞速胜者信息：生效账号（主/对冲接管者）+ 对冲计数租约
struct RaceWin {
    lines: Box<dyn Iterator<Item = String> + Send>,
    /// 生效账号 uid（对冲接管时为对冲账号）
    uid: String,
    /// 对冲侧计数租约（None = 未触发对冲；Some = 竞速已定，待 settle 释放）
    hedge: Option<HedgeLease>,
    /// 对冲接管（对冲请求先出首字）
    takeover: bool,
}

/// 首字竞速（F-76③）：对冲关闭（阈值 0）时与纯首字超时语义完全一致；开启时
/// 主请求首字超阈值 → 从池内取第二账号（走同一 busy 过滤/负载因子——主账号已
/// inflight 天然让位，受 F-77 并发上限约束）发对冲请求，先出首字者胜。
/// 对冲账号取号时在途计数 +1 由 `HedgeLease` RAII 管理全生命周期；
/// 接管时 guard 重绑到对冲账号（流计数随流存续）。
/// 审查 #1：对冲请求必须按对冲账号 uid 重建 body（`raw_body` + 模型/会话等
/// 重建材料），prompt_cache_key 按账号硬隔离——跨账号命中前缀缓存 = 对话泄露，
/// 严禁复用按主账号注入后的产物。
#[allow(clippy::too_many_arguments)]
fn race_first_byte(
    state: &Arc<ApiSharedState>,
    primary_uid: &str,
    reader: Box<dyn Read + Send>,
    tried: &HashSet<String>,
    allowed: Option<&HashSet<String>>,
    dedicated: Option<&str>,
    raw_body: &[u8],
    model: &str,
    conv_id: &str,
    effort: Option<&str>,
    sanitize: bool,
    templates: &[(String, String)],
) -> Result<RaceWin, ()> {
    let hedge_ms = state.wb_hedge_threshold_ms.load(std::sync::atomic::Ordering::Relaxed);
    if hedge_ms == 0 {
        return wb_upstream::lines_with_first_byte_timeout(reader).map(|lines| RaceWin {
            lines,
            uid: primary_uid.to_string(),
            hedge: None,
            takeover: false,
        });
    }
    let state2 = state.clone();
    let tried2 = tried.clone();
    let allowed2 = allowed.cloned();
    let dedicated2 = dedicated.map(str::to_string);
    let spawn_backup = move || -> Option<(Box<dyn Read + Send>, HedgeLease)> {
        let (picked2, ev) = state2
            .wb_pool
            .pick_excluding_constrained_ev(&tried2, allowed2.as_ref(), dedicated2.as_deref())?;
        // F-77⑤ 可观测：对冲取号同样记录 busy 让位/降级事件
        if let Some(ev) = ev {
            state2.logger.log_sched_event(&ev);
        }
        // 租约先于建连获取：建连失败（下行 `?`）时随闭包局部变量 Drop 自动 -1
        let lease = HedgeLease::acquire(&state2, &picked2.uid);
        // 审查 #1：按对冲账号 uid 重建请求体（prompt_cache_key 首段=账号 uid 硬隔离；
        // conv_id 保持同一对话），不再复用主账号的 converted
        let body = wb_payload::prepare_wb_chat_body(
            raw_body, model, &picked2.uid, conv_id, effort, sanitize, templates,
        );
        let creds2 = WbCreds {
            id: picked2.uid.clone(),
            uid: picked2.uid.clone(),
            name: String::new(),
            token: picked2.jwt.clone(),
            domain: picked2.domain.clone(),
            enterprise_id: picked2.enterprise_id.clone(),
            global_region: picked2.global_region,
        };
        let reader2 = wb_upstream::make_wb_request(&creds2, &body).ok()?;
        Some((reader2, lease))
    };
    match wb_upstream::lines_with_first_byte_hedged(reader, hedge_ms, spawn_backup) {
        Ok(out) => {
            let uid = if out.takeover {
                out.hedge
                    .as_ref()
                    .map(|l| l.uid.clone())
                    .unwrap_or_else(|| primary_uid.to_string())
            } else {
                primary_uid.to_string()
            };
            Ok(RaceWin { uid, lines: out.lines, hedge: out.hedge, takeover: out.takeover })
        }
        Err(()) => Err(()),
    }
}

/// 竞速结束后处理对冲计数与日志（F-76③/F-77）：
/// - 释放对冲账号竞速窗口的在途占用（租约 Drop，-1 恰好一次）；
/// - 接管时 guard 重绑到对冲账号（原账号解绑 -1、对冲账号 +1，流计数随流存续）；
/// - [SCHED] 日志记录 hedge_takeover / hedge_lost。
fn settle_hedge(
    state: &ApiSharedState,
    win: &mut RaceWin,
    mut guard: InflightGuard,
    primary_uid: &str,
) -> InflightGuard {
    // take 移交所有权：本函数返回前 Drop（竞速窗口计数 -1 恰好一次）
    let Some(lease) = win.hedge.take() else {
        return guard;
    };
    let hedge_uid = lease.uid.as_str();
    if win.takeover {
        state.logger.log_sched_event(&format!(
            "hedge_takeover primary={} hedge={}",
            primary_uid, hedge_uid
        ));
        guard = guard.bind_account(lease.counter.clone());
    } else {
        state.logger.log_sched_event(&format!(
            "hedge_lost primary={} hedge={}",
            primary_uid, hedge_uid
        ));
    }
    drop(lease); // 释放竞速窗口占用（接管路径先重绑流计数再释放，语义与原实现一致）
    guard
}

/// 长上下文提示（F-76④）：输入粗估超阈值时返回 (token 估算, 降档开关状态)
fn longctx_estimate(state: &ApiSharedState, peek: &Value) -> Option<(u64, bool)> {
    let t = super::wb_model_route::estimate_input_tokens(peek);
    if t < super::wb_model_route::LONGCTX_TOKEN_THRESHOLD {
        return None;
    }
    Some((
        t,
        state
            .wb_longctx_downgrade
            .load(std::sync::atomic::Ordering::Relaxed),
    ))
}

/// 上下文过大提示日志（F-76④：仅标注不做真裁剪）
fn log_longctx_hint(state: &ApiSharedState, peek: &Value, model: &str) {
    if let Some((tokens, downgrade)) = longctx_estimate(state, peek) {
        state.logger.log_sched_event(&format!(
            "longctx_hint model={} tokens≈{}k downgrade={}",
            model,
            tokens / 1000,
            downgrade
        ));
    }
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

    // 批次 D-1 线程隔离：流任务迁入专用阻塞池（见 mod.rs stream_runtime 注释）
    super::stream_runtime().spawn_blocking(move || {
        // inflight guard 随后台任务存续至流结束（§4.5，客户端断连由 Drop 兜底）；
        // 取号后经 bind_account 维护账号级在途计数（F-77），流结束 Drop 配对释放
        let chat_id = match proto {
            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
            Protocol::Anthropic => format!("msg_{}", now_ts()),
            Protocol::Responses => format!("resp_{}", now_ts()),
        };

        // SSE keep-alive 15s（T2.7/F-34 §5.5 #7：防中间层回收长流）。
        // P1 修复：与 routes.rs 同款 watch + DoneSignal 方案，替换旧 AtomicBool
        // 15s 轮询（主任务发完后 ticker 仍可空转至多一个 15s 周期，流终结被拖延）；
        // 主任务结束（DoneSignal Drop，含 panic 展开）置 done=true，ticker select!
        // 收到退出信号即退出 → sender 全部关闭 → 流正常终结
        let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
        let _done = super::routes::DoneSignal(done_tx);
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

        run_wb_stream(&state, &body_vec, &model, proto, &key_id, &chat_id, &tx, start_ts, guard);
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
    mut guard: InflightGuard,
) {
    // 请求日志附带的 API Key 展示名（匿名/未知 → 空串，日志显示 "-"）
    let key_name = super::api_keys::key_name_for(&state.data_dir, key_id);
    let peek: Value = serde_json::from_slice(body_vec).unwrap_or(json!({}));
    let sticky_key = SessionKey::from_body(&peek);
    // F-76④ 上下文过大提示（仅日志标注，不做真裁剪）
    log_longctx_hint(state, &peek, model);
    let templates = load_templates(state);
    let mut sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);

    // F-35 子 Key 约束：限定上游 + 专一/临期优先（匿名/无约束 Key 全空 → 走默认调度）。
    // issue #25 资源池绑定：仅当 Key 约束作用域含 buddy 池时应用（绑定 trae 的
    // Key 白名单指 Trae 账号，不得误过滤 WB 池）。
    // issue #30 混合白名单：按前缀作用域提取 Buddy 条目（空集 = 排除，不得过滤）
    let (allowed_set, dedicated) = super::api_keys::constraints_for(&state.data_dir, key_id)
        .and_then(|k| k.pool_constraints("buddy"))
        .map_or((None, None), |c| (c.allowed, c.dedicated));

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
    // 首选：粘性 > 专一绑定 > 调度策略（F-77④：粘性账号 busy 且有空闲候选时让位）
    let mut first_pick: Option<PickedAccount> = sticky0
        .as_ref()
        .and_then(|(u, _)| {
            state
                .wb_pool
                .pick_sticky_yield(u, allowed_set.as_ref())
                .map(|(p, ev)| {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                })
        })
        .or_else(|| dedicated.as_deref().and_then(|uid| state.wb_pool.pick_by_uid(uid)));

    let mut tried: HashSet<String> = HashSet::new();
    let mut refreshed: HashSet<String> = HashSet::new(); // 401 刷新每账号一次
    let mut empty_hits: usize = 0; // 空完成计数：轮换耗尽收尾区分指纹拦截与账号耗尽

    loop {
        // 客户端断连检测：通道关闭即终止轮换/重试，不再占用账号并发槽（对齐 routes.rs stream_chat）
        if tx.is_closed() {
            return;
        }
        // ── 取号：粘性/专一命中优先，否则按 Key 约束 + 调度策略；换号后仅走策略 ──
        let picked = match first_pick.take() {
            Some(p) => p,
            None => match state.wb_pool.pick_excluding_constrained_ev(&tried, allowed_set.as_ref(), dedicated.as_deref()) {
                Some((p, ev)) => {
                    // F-77⑤ 可观测：busy_yield / busy_fallback 调度事件
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                }
                None => {
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    state.record_usage(true, model, "none", key_id, false, true, duration_ms, 0, 0);
                    // 审查 P2：轮换耗尽收尾对齐 routes.rs（issue #57 可观测）——
                    // 空完成主导 → 明示渠道指纹拦截语义（账号/模型无关，提示更新
                    // wb_template_map.json），否则维持原文案；code 保持 no_healthy_account
                    let (healthy_total, _) = state.wb_pool.selectable_stats_in(None);
                    let (exhaust_msg, exhaust_tag) =
                        super::routes::exhaust_message(empty_hits, tried.len(), healthy_total);
                    state.logger.log_request(
                        "buddy", "POST", "/v2/chat/completions", model, true, 503, "none",
                        duration_ms, &key_name, "",
                        Some(&exhaust_msg),
                    );
                    let diag = state.wb_pool.diagnose();
                    let diag_summary: Vec<String> = diag
                        .iter()
                        .map(|d| format!("{}({})", d.name, d.reason))
                        .collect();
                    state.logger.log_debug_line(format!(
                        "[DEBUG] {exhaust_tag} tried={} empty_hits={} pool=buddy reasons=[{}]",
                        tried.len(),
                        empty_hits,
                        diag_summary.join(", "),
                    ));
                    let err_body = json!({
                        "error": {"message": exhaust_msg, "type": "api_error", "code": "no_healthy_account"}
                    });
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                        "data: {err_body}\n\n"
                    ))));
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                    return;
                }
            },
        };
        tried.insert(picked.uid.clone());
        // F-77 账号级在途计数：取号即绑定（重试换号时 bind_account 自动解绑旧账号）。
        // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
        // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（专一/粘性
        // 锁定、busy_fallback 降级）保留原无条件绑定语义。
        // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
        let bound = if picked.over_limit {
            guard = guard.bind_account(state.wb_pool.inflight_handle(&picked.uid));
            true
        } else {
            guard.bind_account_cas(
                state.wb_pool.inflight_handle(&picked.uid),
                state.wb_pool.concurrency_limit(),
            )
        };
        if !bound {
            continue;
        }
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
        let mut converted = wb_payload::prepare_wb_chat_body(
            body_vec, model, &picked.uid, &conv_id, effort.as_deref(), sanitize, &templates,
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
            // 客户端断连检测：重试等待/长路径期间离开则终止（同账号 guard 随 Drop 释放）
            if tx.is_closed() {
                return;
            }
            let ttfb_start = Instant::now();
            match wb_upstream::make_wb_request(&creds, &converted) {
                Ok(reader) => {
                    // 首字超时 10s（T2.7/F-34）：超时视为上游故障 → 换号；
                    // F-76③ 慢请求竞速对冲：首字超阈值时向第二账号发对冲请求，
                    // 先出首字者胜（对冲关闭时与纯首字超时语义一致）
                    let mut win = match race_first_byte(
                        state,
                        &picked.uid,
                        reader,
                        &tried,
                        allowed_set.as_ref(),
                        dedicated.as_deref(),
                        body_vec,
                        model,
                        &conv_id,
                        effort.as_deref(),
                        sanitize,
                        &templates,
                    ) {
                        Ok(w) => w,
                        Err(()) => {
                            state.wb_pool.note_error(&picked.uid, ErrKind::Server);
                            note_model_failure(state, model);
                            break;
                        }
                    };
                    let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                    // 对冲计数落定 + guard 重绑（接管时生效账号 = 对冲账号）
                    guard = settle_hedge(state, &mut win, guard, &picked.uid);
                    let win_uid = win.uid.as_str();
                    // 竞速胜者行源（Box 迭代器）桥接为可中断行源：stream_forward_ex
                    // 停滞期间周期性检查断连（转发线程复用；语义与既有 ttfb 包装一致）
                    let win_lines = super::wb_upstream::InterruptibleLines::from_iterator(
                        Box::new(win.lines) as Box<dyn Iterator<Item = String> + Send>,
                    );
                    let (error_info, sent_any, failed_inline, usage) =
                        wb_sse::stream_forward_ex(win_lines, tx, proto, chat_id, model);
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
                        // F-76① TTFT 入账：用量页 P50/P95/TTFT 分位统计
                        state.record_usage_ttfb(
                            true, model, win_uid, key_id, error_info.is_none() && !failed_inline,
                            true, duration_ms, pt, ct, Some(ttfb_ms),
                        );
                    }
                    match error_info {
                        Some((code, msg)) => {
                            if super::is_empty_completion(code, &msg) {
                                // sent_any 护栏（对齐 qoder_route）：内容已流出时换号重发
                                // 会向同一 SSE 流拼接第二份完整响应 → 就地收尾不再重试
                                if sent_any {
                                    state.logger.log_request_ttfb(
                                        "buddy", "POST", "/v2/chat/completions", model, true, 200, win_uid,
                                        duration_ms, Some(ttfb_ms), &key_name,
                                        &state.wb_pool.name_of(win_uid), Some("空完成（内容已流出）→ 就地收尾"),
                                    );
                                    return;
                                }
                                // 空完成（影子风控/上游异常，issue #57）：不冷却、不透传、
                                // 不绑定粘性，换号重试（收尾帧未发，重试流可续传）
                                empty_hits += 1;
                                state.logger.log_request_ttfb(
                                    "buddy", "POST", "/v2/chat/completions", model, true, 200, win_uid,
                                    duration_ms, Some(ttfb_ms), &key_name,
                                    &state.wb_pool.name_of(win_uid), Some(&format!("空完成 → 换号重试{}", wb_payload::template_hit_note())),
                                );
                                break;
                            }
                            let kind = classify_wb_error(code, &msg);
                            if kind != ErrKind::None {
                                state.wb_pool.note_error(win_uid, kind);
                                note_model_failure_ex(state, model, Some(&msg));
                                *safe_lock(&state.last_error) =
                                    Some(format!("wb uid={} code={} msg={}", win_uid, code, msg));
                                // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.wb_sticky.unbind_uid(&win_uid);
                                }
                            }
                            if !sent_any {
                                // 流未开始：错误不下发，允许换号重试
                                break;
                            }
                            state.logger.log_request_ttfb(
                                "buddy", "POST", "/v2/chat/completions", model, true, 200, win_uid,
                                duration_ms, Some(ttfb_ms), &key_name,
                                &state.wb_pool.name_of(win_uid), Some(&msg),
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
                                    "buddy", "POST", "/v2/chat/completions", model, true, 200, win_uid,
                                    duration_ms, Some(ttfb_ms), &key_name,
                                    &state.wb_pool.name_of(win_uid), Some("流内失败已透传客户端"),
                                );
                                return;
                            }
                            state.wb_pool.note_success(win_uid);
                            clear_model_failure(state, model);
                            // 绑定粘性会话（Mutex 内 re-check 防 TOCTOU）
                            state.wb_sticky.bind(&sticky_key, win_uid, &conv_id, now_ts());
                            state.wb_sticky.save(&state.data_dir);
                            state.logger.log_request_ttfb(
                                "buddy", "POST", "/v2/chat/completions", model, true, 200, win_uid,
                                duration_ms, Some(ttfb_ms), &key_name,
                                &state.wb_pool.name_of(win_uid), None,
                            );
                            return;
                        }
                    }
                }
                Err((status, resp_body, retry_after)) => {
                    // 11128 渠道风控（issue #57）：按请求指纹拦截、与账号/模型无关，
                    // 换号无意义。清洗未启用时强制清洗后同号重试一次；已清洗仍命中
                    // → 走下方 retry_plan 原样 Fatal（需更新 wb_template_map.json）
                    if status == 400
                        && super::retry::is_illegal_channel_error(&resp_body)
                        && !sanitize
                    {
                        sanitize = true;
                        state.logger.log_sched_event(&format!(
                            "11128 强制清洗重试{}", wb_payload::template_hit_note()
                        ));
                        converted = wb_payload::prepare_wb_chat_body(
                            body_vec, model, &picked.uid, &conv_id, effort.as_deref(), true, &templates,
                        );
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        // 断连检测：重试等待期间客户端离开则终止
                        if tx.is_closed() {
                            return;
                        }
                        continue;
                    }
                    // 分级重试策略表（T2.2/F-33 v1.2）
                    match retry_plan(status, &resp_body, same_attempt, retry_after) {
                        RetryAction::RetrySame { delay_ms } => {
                            same_attempt += 1;
                            std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(60_000)));
                            // 断连检测：重试等待期间客户端离开则终止，不再占用该账号并发槽
                            if tx.is_closed() {
                                return;
                            }
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
                            // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                            if kind.unbinds_sticky() {
                                state.wb_sticky.unbind_uid(&picked.uid);
                            }
                            note_model_failure_ex(state, model, upstream_msg(&resp_body).as_deref());
                            *safe_lock(&state.last_error) = Some(format!(
                                "wb uid={} status={} body={}",
                                picked.uid,
                                status,
                                safe_slice(&resp_body, 200)
                            ));
                            state.logger.log_request(
                                "buddy", "POST", "/v2/chat/completions", model, true, status, &picked.uid,
                                start_ts.elapsed().as_millis() as u64,
                                &key_name, &state.wb_pool.name_of(&picked.uid),
                                Some(&format!("upstream status={}", status)),
                            );
                            break; // 换号
                        }
                        RetryAction::Fatal => {
                            let msg = format!("upstream {} error: {}", status, safe_slice(&resp_body, 300));
                            state.logger.log_request(
                                "buddy", "POST", "/v2/chat/completions", model, true, status, &picked.uid,
                                start_ts.elapsed().as_millis() as u64,
                                &key_name, &state.wb_pool.name_of(&picked.uid),
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
    // P2 修复：聚合含分级重试（RetrySame 退避 std::thread::sleep 最长 60s×N），
    // 长阻塞占主池会饿死鉴权等短任务，迁入 stream_runtime 专用阻塞池
    let result = super::stream_runtime().spawn_blocking(move || {
        // inflight guard 随后台任务存续至聚合完成（§4.5）；F-77 取号后绑定账号级计数
        let mut guard = guard;
        // 请求日志附带的 API Key 展示名（匿名/未知 → 空串，日志显示 "-"）
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        let peek: Value = serde_json::from_slice(&body_vec).unwrap_or(json!({}));
        let sticky_key = SessionKey::from_body(&peek);
        // F-76④ 上下文过大提示（仅日志标注，不做真裁剪）
        log_longctx_hint(&state, &peek, &model);
        let templates = load_templates(&state);
        let mut sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);

        // F-35 子 Key 约束（与非流式同款；issue #30 混合白名单按前缀作用域分池）
        let (allowed_set, dedicated) = super::api_keys::constraints_for(&state.data_dir, &key_id)
            .and_then(|k| k.pool_constraints("buddy"))
            .map_or((None, None), |c| (c.allowed, c.dedicated));

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
            .and_then(|(u, _)| {
                state
                    .wb_pool
                    .pick_sticky_yield(u, allowed_set.as_ref())
                    .map(|(p, ev)| {
                        if let Some(ev) = ev {
                            state.logger.log_sched_event(&ev);
                        }
                        p
                    })
            })
            .or_else(|| dedicated.as_deref().and_then(|uid| state.wb_pool.pick_by_uid(uid)));

        let mut tried: HashSet<String> = HashSet::new();
        let mut refreshed: HashSet<String> = HashSet::new();
        let mut empty_hits: usize = 0; // 空完成计数：轮换耗尽收尾区分指纹拦截与账号耗尽

        loop {
            let picked = match first_pick.take() {
                Some(p) => p,
                None => match state.wb_pool.pick_excluding_constrained_ev(&tried, allowed_set.as_ref(), dedicated.as_deref()) {
                    Some((p, ev)) => {
                        if let Some(ev) = ev {
                            state.logger.log_sched_event(&ev);
                        }
                        p
                    }
                    None => {
                        state.record_usage(true, &model, "none", &key_id, false, stream,
                            start_ts.elapsed().as_millis() as u64, 0, 0);
                        // 审查 P2：轮换耗尽收尾对齐 routes.rs——空完成主导 → 指纹拦截语义
                        let (healthy_total, _) = state.wb_pool.selectable_stats_in(None);
                        let (exhaust_msg, _) =
                            super::routes::exhaust_message(empty_hits, tried.len(), healthy_total);
                        return Err(exhaust_msg);
                    }
                },
            };
            tried.insert(picked.uid.clone());
            // F-77 账号级在途计数：取号即绑定。
            // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
            // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（专一/粘性
            // 锁定、busy_fallback 降级）保留原无条件绑定语义。
            // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
            let bound = if picked.over_limit {
                guard = guard.bind_account(state.wb_pool.inflight_handle(&picked.uid));
                true
            } else {
                guard.bind_account_cas(
                    state.wb_pool.inflight_handle(&picked.uid),
                    state.wb_pool.concurrency_limit(),
                )
            };
            if !bound {
                continue;
            }
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
            let mut converted = wb_payload::prepare_wb_chat_body(
                &body_vec, &model, &picked.uid, &conv_id, effort.as_deref(), sanitize, &templates,
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
                        // 首字超时：非流式聚合同样适用（上游只回 SSE）；
                        // F-76③ 慢请求竞速对冲（对冲关闭时与纯首字超时语义一致）
                        let mut win = match race_first_byte(
                            &state,
                            &picked.uid,
                            reader,
                            &tried,
                            allowed_set.as_ref(),
                            dedicated.as_deref(),
                            &body_vec,
                            &model,
                            &conv_id,
                            effort.as_deref(),
                            sanitize,
                            &templates,
                        ) {
                            Ok(w) => w,
                            Err(()) => {
                                state.wb_pool.note_error(&picked.uid, ErrKind::Server);
                                note_model_failure(&state, &model);
                                break;
                            }
                        };
                        let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                        guard = settle_hedge(&state, &mut win, guard, &picked.uid);
                        let win_uid = win.uid.as_str();
                        let (resp, error_info) =
                            wb_sse::aggregate(win.lines, &format!("chatcmpl-{}", now_ts()));
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        match (resp, error_info) {
                            (Some(mut r), None) => {
                                if super::aggregated_response_is_empty(&r) {
                                    // 空完成（影子风控/上游异常，issue #57）：换号重试，
                                    // 口径同下方「empty response」（Server 级短冷却）
                                    empty_hits += 1;
                                    state.wb_pool.note_error(win_uid, ErrKind::Server);
                                    note_model_failure(&state, &model);
                                    state.record_usage_ttfb(true, &model, win_uid, &key_id, false, stream, duration_ms, 0, 0, Some(ttfb_ms));
                                    state.logger.log_request_ttfb(
                                        "buddy", "POST", "/v2/chat/completions", &model, stream, 502, win_uid,
                                        duration_ms, Some(ttfb_ms), &key_name,
                                        &state.wb_pool.name_of(win_uid), Some(&format!("空完成 → 换号重试{}", wb_payload::template_hit_note())),
                                    );
                                    break;
                                }
                                r["model"] = json!(model);
                                let (pt, ct) = r.get("usage").map(|u| (
                                    u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                    u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                )).unwrap_or((0, 0));
                                // F-76① TTFT 入账
                                state.record_usage_ttfb(true, &model, win_uid, &key_id, true, stream, duration_ms, pt, ct, Some(ttfb_ms));
                                state.wb_pool.note_success(win_uid);
                                clear_model_failure(&state, &model);
                                state.wb_sticky.bind(&sticky_key, win_uid, &conv_id, now_ts());
                                state.wb_sticky.save(&state.data_dir);
                                state.logger.log_request_ttfb(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, 200, win_uid,
                                    duration_ms, Some(ttfb_ms), &key_name,
                                    &state.wb_pool.name_of(win_uid), None,
                                );
                                return Ok(r);
                            }
                            (None, Some((code, msg))) => {
                                let kind = classify_wb_error(code, &msg);
                                if kind != ErrKind::None {
                                    state.wb_pool.note_error(win_uid, kind);
                                    note_model_failure_ex(&state, &model, Some(&msg));
                                    // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                                    if kind.unbinds_sticky() {
                                        state.wb_sticky.unbind_uid(&win_uid);
                                    }
                                }
                                *safe_lock(&state.last_error) =
                                    Some(format!("wb uid={} code={} msg={}", win_uid, code, msg));
                                state.record_usage_ttfb(true, &model, win_uid, &key_id, false, stream, duration_ms, 0, 0, Some(ttfb_ms));
                                state.logger.log_request_ttfb(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, 200, win_uid,
                                    duration_ms, Some(ttfb_ms), &key_name,
                                    &state.wb_pool.name_of(win_uid), Some(&msg),
                                );
                                // 流内错误且未产出内容 → 换号重试
                                break;
                            }
                            _ => {
                                state.wb_pool.note_error(win_uid, ErrKind::Server);
                                note_model_failure(&state, &model);
                                state.record_usage_ttfb(true, &model, win_uid, &key_id, false, stream, duration_ms, 0, 0, Some(ttfb_ms));
                                state.logger.log_request_ttfb(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, 502, win_uid,
                                    duration_ms, Some(ttfb_ms), &key_name,
                                    &state.wb_pool.name_of(win_uid), Some("empty response"),
                                );
                                break;
                            }
                        }
                    }
                    Err((status, resp_body, retry_after)) => {
                        // 11128 渠道风控（issue #57）：清洗未启用时强制清洗后同号重试一次
                        if status == 400
                            && super::retry::is_illegal_channel_error(&resp_body)
                            && !sanitize
                        {
                            sanitize = true;
                            state.logger.log_sched_event(&format!(
                                "11128 强制清洗重试{}", wb_payload::template_hit_note()
                            ));
                            converted = wb_payload::prepare_wb_chat_body(
                                &body_vec, &model, &picked.uid, &conv_id, effort.as_deref(), true, &templates,
                            );
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            continue;
                        }
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
                                // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.wb_sticky.unbind_uid(&picked.uid);
                                }
                                note_model_failure_ex(&state, &model, upstream_msg(&resp_body).as_deref());
                                *safe_lock(&state.last_error) =
                                    Some(format!("wb uid={} status={}", picked.uid, status));
                                state.record_usage(true, &model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0);
                                state.logger.log_request(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64,
                                    &key_name, &state.wb_pool.name_of(&picked.uid),
                                    Some(&format!("upstream status={}", status)),
                                );
                                break;
                            }
                            RetryAction::Fatal => {
                                state.logger.log_request(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64,
                                    &key_name, &state.wb_pool.name_of(&picked.uid),
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
        Ok(Err(msg)) => {
            // 审查修复：Fatal 错误透传上游状态码（对齐 SOLO 聚合路径 AggregateFail::Upstream）。
            // 上游 400/404/413 等请求级错误此前被统一降级 503 no_healthy_account，
            // 严格客户端（Codex/Claude Code）会按「可重试临时故障」无意义重试；
            // 401/403/429 属账号/池问题，维持 503 语义（换号重试仍由池层决策）。
            let upstream_status = msg
                .strip_prefix("upstream ")
                .and_then(|rest| rest.split(' ').next())
                .and_then(|s| s.parse::<u16>().ok());
            let status = match upstream_status {
                Some(s) if (400..500).contains(&s) && !matches!(s, 401 | 403 | 429) => {
                    StatusCode::from_u16(s).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
                }
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            match proto {
                Protocol::Anthropic => anthropic_error(status, "api_error", &msg),
                _ => openai_error(status, "upstream_error", &msg),
            }
        }
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
    // P2 修复：工具代执行多轮上游请求 + 重试退避（最长 60s×N），长阻塞占主池
    // 会饿死鉴权等短任务，与非流式聚合一并迁入 stream_runtime 专用阻塞池
    let result = super::stream_runtime().spawn_blocking(move || {
        // inflight guard 随后台任务存续至编排完成（§4.5）；F-77 取号后绑定账号级计数
        let mut guard = guard;
        let model = model_inner;
        // 请求日志附带的 API Key 展示名（匿名/未知 → 空串，日志显示 "-"）
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        let templates = load_templates(&state);
        let mut sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);
        // P1-6 劫持防护：false = 客户端已声明同名 function 工具，未注入代理工具，
        // 全程跳过代执行（客户端工具调用照常透传，由客户端自行执行）
        let proxy_exec = super::wb_toolexec::inject_proxy_tools(&mut chat_body);
        // F-76④ 上下文过大提示（仅日志标注，不做真裁剪）
        log_longctx_hint(&state, &chat_body, &model);

        // F-35 子 Key 约束（与非流式同款；issue #30 混合白名单按前缀作用域分池）
        let (allowed_set, dedicated) = super::api_keys::constraints_for(&state.data_dir, &key_id)
            .and_then(|k| k.pool_constraints("buddy"))
            .map_or((None, None), |c| (c.allowed, c.dedicated));

        let catalog = super::wb_catalog::load(&state.data_dir);
        let effort = super::wb_catalog::find(&catalog, &model)
            .and_then(|m| m.resolve_effort(chat_body.get("reasoning_effort").and_then(|v| v.as_str())));

        let mut tried: HashSet<String> = HashSet::new();
        let mut refreshed: HashSet<String> = HashSet::new();
        let mut empty_hits: usize = 0; // 空完成计数：轮换耗尽收尾区分指纹拦截与账号耗尽
        let mut records: Vec<super::wb_toolexec::SearchRecord> = Vec::new();
        let mut final_completion: Option<Value> = None;
        let mut success_uid: Option<String> = None; // 审查修复：保留真实账号归因
        let mut last_err: Option<String> = None;
        // F-76① TTFT：各轮首字耗时（最终轮即最终回复的首字延迟）
        let mut last_ttfb_ms: Option<u64> = None;
        let resp_id = format!("resp_{}", now_ts());

        'accounts: loop {
            let picked = match state
                .wb_pool
                .pick_excluding_constrained_ev(&tried, allowed_set.as_ref(), dedicated.as_deref())
            {
                Some((p, ev)) => {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                }
                None => break,
            };
            tried.insert(picked.uid.clone());
            let conv_id = gen_conv_id();
            // F-77 账号级在途计数：取号即绑定。
            // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
            // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（专一锁定）
            // 保留原无条件绑定语义。
            // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
            let bound = if picked.over_limit {
                guard = guard.bind_account(state.wb_pool.inflight_handle(&picked.uid));
                true
            } else {
                guard.bind_account_cas(
                    state.wb_pool.inflight_handle(&picked.uid),
                    state.wb_pool.concurrency_limit(),
                )
            };
            if !bound {
                continue;
            }
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());
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
                    &body_bytes, &model, &picked.uid, &conv_id, effort.as_deref(), sanitize, &templates,
                );

                // F-76① 各轮记首字耗时（最终轮即最终回复的 TTFT）
                let ttfb_start = Instant::now();
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
                        last_ttfb_ms = Some(ttfb_start.elapsed().as_millis() as u64);
                        let (completion, error_info) =
                            wb_sse::aggregate(lines, &format!("chatcmpl-{}", now_ts()));
                        if let Some((code, msg)) = error_info {
                            let kind = classify_wb_error(code, &msg);
                            if kind != ErrKind::None {
                                state.wb_pool.note_error(&picked.uid, kind);
                                note_model_failure_ex(&state, &model, Some(&msg));
                                // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.wb_sticky.unbind_uid(&picked.uid);
                                }
                            }
                            *safe_lock(&state.last_error) =
                                Some(format!("wb-toolexec uid={} code={} msg={}", picked.uid, code, msg));
                            break Err(msg);
                        }
                        let Some(completion) = completion else {
                            break Err("上游返回空响应".to_string());
                        };
                        if super::aggregated_response_is_empty(&completion) {
                            // 空完成（影子风控/上游异常，issue #57）：换号重试。
                            // 口径：同流式路径不冷却（哨兵注释见 mod.rs）；此处补即时
                            // 观测日志，接通「空完成 → 模板命中」反查通道（与其余路径一致）
                            empty_hits += 1;
                            let duration_ms = start_ts.elapsed().as_millis() as u64;
                            state.logger.log_request_ttfb(
                                "buddy", "POST", "/v1/responses", &model, stream, 502, &picked.uid,
                                duration_ms, last_ttfb_ms, &key_name,
                                &state.wb_pool.name_of(&picked.uid),
                                Some(&format!("空完成 → 换号重试{}", wb_payload::template_hit_note())),
                            );
                            break Err("空完成（影子风控/上游异常）".to_string());
                        }
                        if let Some(u) = completion.get("usage") {
                            acc_usage = (
                                acc_usage.0 + u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                acc_usage.1 + u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                            );
                        }
                        // P1-6：仅代理成功注入工具时才按名提取代执行调用；
                        // 客户端同名工具的调用一律视为普通透传（不提取不执行）
                        let calls = if proxy_exec {
                            super::wb_toolexec::extract_proxy_calls(&completion)
                        } else {
                            Vec::new()
                        };
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
                        // 11128 渠道风控（issue #57）：清洗未启用时强制清洗后重试
                        // （round 回退，不消耗轮次；下一轮循环顶部按新 sanitize 重建请求体）
                        if status == 400
                            && super::retry::is_illegal_channel_error(&resp_body)
                            && !sanitize
                        {
                            sanitize = true;
                            state.logger.log_sched_event(&format!(
                                "11128 强制清洗重试{}", wb_payload::template_hit_note()
                            ));
                            round = round.saturating_sub(1);
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            continue;
                        }
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
                                // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.wb_sticky.unbind_uid(&picked.uid);
                                }
                                note_model_failure_ex(&state, &model, upstream_msg(&resp_body).as_deref());
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
                // F-76① TTFT 入账：最终轮首字耗时
                state.record_usage_ttfb(true, &model, usage_uid, &key_id, true, stream, duration_ms, pt, ct, last_ttfb_ms);
                state.logger.log_request_ttfb(
                    "buddy", "POST", "/v1/responses", &model, stream, 200, usage_uid,
                    duration_ms, last_ttfb_ms, &key_name, &state.wb_pool.name_of(usage_uid),
                    Some(&format!("rounds={} searches={}", records.len(), records.iter().filter(|r| r.tool == super::wb_toolexec::TOOL_SEARCH).count())),
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
                    duration_ms, &key_name, "", last_err.as_deref(),
                );
                // 审查 P2：轮换耗尽收尾对齐 routes.rs——空完成主导 → 指纹拦截语义
                Err(last_err.unwrap_or_else(|| {
                    let (healthy_total, _) = state.wb_pool.selectable_stats_in(None);
                    super::routes::exhaust_message(empty_hits, tried.len(), healthy_total).0
                }))
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

    /// 6004 限额文案 → 精确恢复时刻（agent2api errors.rs 同源语义；
    /// UTC+8 字面量必须命中，按 UTC+8 解释为 Unix 秒）
    #[test]
    fn parse_quota_reset_at_from_limit_message() {
        // 未来时刻动态构造（UTC+8 时区格式化），格式与上游文案一致
        let reset = (chrono::Utc::now() + chrono::Duration::hours(3))
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
        let msg = format!(
            "当前模型使用额度已达上限，将在 {} UTC+8 重置",
            reset.format("%Y-%m-%d %H:%M:%S")
        );
        let ts = parse_quota_reset_at(&msg).expect("应解析出恢复时刻");
        assert_eq!(ts, reset.timestamp());
        // 缺 UTC+8 字面量 / 缺时刻 → None
        assert_eq!(parse_quota_reset_at("将在 2099-01-01 00:00:00 重置"), None);
        assert_eq!(parse_quota_reset_at("quota exceeded"), None);
        // 过去时刻 → None（落回渐进退避）
        assert_eq!(parse_quota_reset_at("已在 2020-01-01 00:00:00 UTC+8 重置"), None);
        // 超远未来（>7 天钳制）→ None
        let far = (chrono::Utc::now() + chrono::Duration::days(30))
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
        let far_msg = format!("将在 {} UTC+8 重置", far.format("%Y-%m-%d %H:%M:%S"));
        assert_eq!(parse_quota_reset_at(&far_msg), None);
        // 复审修复回归：双时刻文案（触发时刻为过去 + 恢复时刻为未来）→
        // 首锚点解析出过去时刻须跳过，继续用第二锚点解析出恢复时刻
        let past = (chrono::Utc::now() - chrono::Duration::hours(2))
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
        let dual = format!(
            "您于 {} UTC+8 触发限额，将在 {} UTC+8 重置",
            past.format("%Y-%m-%d %H:%M:%S"),
            reset.format("%Y-%m-%d %H:%M:%S")
        );
        assert_eq!(parse_quota_reset_at(&dual), Some(reset.timestamp()));
    }

    /// upstream_msg 三种字段形态（message / error.message / msg）提取
    #[test]
    fn upstream_msg_field_variants() {
        assert_eq!(
            upstream_msg(r#"{"code":6004,"message":"将在 2099-01-01 00:00:00 UTC+8 重置"}"#).as_deref(),
            Some("将在 2099-01-01 00:00:00 UTC+8 重置")
        );
        assert_eq!(
            upstream_msg(r#"{"error":{"code":6004,"message":"限额"}}"#).as_deref(),
            Some("限额")
        );
        assert_eq!(upstream_msg(r#"{"code":6004,"msg":"腾讯系 msg 字段"}"#).as_deref(), Some("腾讯系 msg 字段"));
        assert_eq!(upstream_msg("not json"), None);
        assert_eq!(upstream_msg("{}"), None);
    }
}
