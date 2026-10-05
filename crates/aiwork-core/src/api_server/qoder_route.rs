//! Qoder 上游路由执行层（p3-3-wire）。
//!
//! 请求流程（wb_route 的 Qoder 侧镜像，v1 精简）：
//! 1. 目录条目解析（`qoder_upstream::resolve` → upstreamKey/config，执行时再查
//!    ——调度与执行之间目录可能刷新，条目为准）；
//! 2. 请求体构造（`prepare_qoder_body`：OpenAI body → agent 固定信封，
//!    session 由账号 uid + 下游种子派生）；
//! 3. 取号（qoder_pool 调度，Key 白名单/专一约束同构适用）→ identity 回调解析
//!    凭证（PAT 换 24h 作业令牌 / 刷新链路在回调闭包内走全防护）→
//!    `make_qoder_request`（COSY 19 头签名）；
//! 4. 分级重试：HTTP 层走 `retry_plan`（429/5xx/502 同号退避、401/403 换号）；
//!    **排队（业务码 10605）同号退避优先**——首个进入排队的请求按上游建议
//!    时长同号退避重试（≤3 次，超限放回调度轮换，防止占死并发槽）；并发场景
//!    下若该模型已记录冷却则直接 break 换号（快速失败）；其余流内错误按
//!    ErrMeta 分类映射 ErrKind 后换号；
//! 5. 流式：首字超时 10s（`open_qoder_stream`）→ QoderTranslate 翻译为标准
//!    OpenAI chunk → `wb_sse::stream_forward_ex`（keep-alive 15s + 断连三层
//!    检测同构）；非流式：`aggregate_qoder` → 协议投影复用 wb_sse 转换器；
//! 6. 用量记账（`record_usage_qoder` 独立 qoder 桶）+ 请求级日志。
//!
//! 与 wb_route 的差异（v2）：**会话粘性**（F-80-余 v2，开关默认关）与**竞速对冲**
//! （F-80-余 v2，阈值热参数默认 8s）已按 WB 同构补齐——粘性命中锁定账号（busy
//! 且有空闲候选时让位，F-77④），同账号 + 同种子派生同一上游 session_id 保住
//! 会话侧复用；对冲在原始行源层竞速，胜者行源统一经 QoderTranslate 翻译。仍无
//! 模板清洗、无工具代执行、无 401 刷新重试（凭证新鲜度由 identity 回调按次解析
//! 兜住）。模型级冷却已补齐（审查修复）：排队/超限类错误记录 model → 冷却截止，
//! dispatch 预检快速回退、执行路径跳过同号退避（见下方「模型级冷却」节）。
//! 对冲接管后排队同号退避不适用（重试凭证/请求体属主账号，与生效账号不一致）。
//!
//! 客户端断连：与 wb_route 同款三层检测（轮换/重试入口 tx.is_closed、停滞期
//! next_event_polling 轮询、活跃流逐事件顶部检测）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use super::retry::{retry_plan, RetryAction};
use super::wb_sse;
use super::wb_sticky::SessionKey;
use super::wb_upstream::{lines_with_first_byte_hedged, lines_with_first_byte_timeout, InterruptibleLines};
use super::{ApiSharedState, ErrKind, InflightGuard};
use crate::api_server::routes::{anthropic_error, openai_error, Protocol};
use crate::tasks::qoder_common::QoderCreds;
use crate::tasks::qoder_upstream::{self, ErrMeta, UpstreamKind};

/// 排队同号重试上限（10605 按上游建议退避；超限放回调度轮换防占死并发槽）
const QUEUE_RETRY_LIMIT: u32 = 3;
/// 排队退避缺省秒数（上游 retryAfterSeconds/waitTime 缺失时）
const QUEUE_DEFAULT_BACKOFF_SECS: u64 = 5;
/// 退避上限（上游建议值异常大时钳制）
const QUEUE_BACKOFF_MAX_SECS: u64 = 30;

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ==================== 模型级冷却（审查修复，镜像 wb_route::model_cooldowns） ====================

/// Qoder 模型级冷却表：model → 冷却截止时刻。排队/超限类错误（ErrMeta 分类
/// Queued）是模型级现象（换账号同样排队），与账号无关，故进程级静态表语义
/// 等价；挂 ApiSharedState 需改 server.rs（他人负责），静态 OnceLock 最小改动。
/// 备忘（审查 G6）：键为裸 model 名、无区域前缀，v1 恒 CN 区解析无歧义；
/// 接线 Global 区时需加区域前缀（如 "global:{model}"）。表不做主动回收，
/// 残留条目随冷却窗口（≤30s，QUEUE_BACKOFF_MAX_SECS 钳制）自然过期，重启即清零。
static QODER_MODEL_COOLDOWNS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn cooldown_map() -> &'static Mutex<HashMap<String, Instant>> {
    QODER_MODEL_COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 记录模型级冷却：按本次排队退避时长（已钳制）设定截止时刻。冷却窗口内
/// 后续请求由 dispatch 健康预检快速回退（多源）/显式 429（单源），执行路径
/// 跳过同号退避，不再每请求空转 5s×3~30s×3。
/// 备忘（审查 G6）：键为裸 model 名、无区域前缀（v1 恒 CN 区）；接线 Global
/// 区时需加区域前缀，见 QODER_MODEL_COOLDOWNS 定义处备忘。
fn note_model_cooldown(model: &str, secs: u64) {
    cooldown_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(model.to_string(), Instant::now() + Duration::from_secs(secs));
}

/// 模型冷却剩余秒数；None = 无冷却（过期残留条目同 wb 版语义按无冷却处理）
pub fn model_cooling_remaining_secs(model: &str) -> Option<u64> {
    let map = cooldown_map().lock().unwrap_or_else(|e| e.into_inner());
    let until = map.get(model)?;
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        None
    } else {
        Some(left.as_secs())
    }
}

/// 请求成功后清除该模型冷却（同 wb_route::clear_model_failure 语义）
fn clear_model_cooldown(model: &str) {
    cooldown_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(model);
}

/// 排队/退避分段等待（审查修复）：长 sleep 切成 ≤500ms 片段逐段睡，每段后
/// 检查流通道是否关闭——客户端已断连即提前退出，不再空等数十秒排队/退避
/// 时长；返回 false = 已断连。非流式路径无下行通道可查（handler 返回
/// Response 前无断连通知机制），保持整段 sleep
fn sleep_interruptible(
    total: Duration,
    tx: Option<&tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>>,
) -> bool {
    let mut left = total;
    while !left.is_zero() {
        let step = left.min(Duration::from_millis(500));
        std::thread::sleep(step);
        left = left.saturating_sub(step);
        if let Some(t) = tx {
            if t.is_closed() {
                return false;
            }
        }
    }
    true
}

/// resolve 命中条目是否 Global 专属（审查修复：v1 恒 CN 网关执行，Global
/// 专属条目调度 resolve 双区兜底判活、执行必死，需在执行入口明确拒绝）。
///
/// qoder_upstream 未暴露分区分目录/兜底表的查询接口（他人文件不可改），判定
/// 基于两路**确定性证据**，不依赖旧实现「resolve(Global) 结果与命中条目 Value
/// 全等 ⟹ 两区同名条目声明必有差异」的脆弱值假设（同名同值场景会误判）：
/// ① list() 并集按 id 去重、Global 侧先注册：标注 "cn" ⟺ Global 侧（远程目录
///    + 静态兜底表）完全无此 id → CN resolve 的命中必来自 CN 侧 → 必非专属；
/// ② 标注 "global" 时借 resolve 双区优先序差异取第二证据：resolve(Global) 序
///    = [Global 目录, CN 目录, CN 兜底, Global 兜底]，resolve(Cn) 序 = [CN 目录,
///    Global 目录, Global 兜底, CN 兜底]。两路命中**不同**条目时可逐源证明
///    CN 侧必有此 id（CN 远程目录或 CN 兜底表至少其一声明）→ 必非专属；
/// ③ 两路命中全等：既有 pub 接口无法区分「仅 Global 独持」与「两区同名同值」
///    （后者仅可能来自远程目录动态刷新；静态兜底表两区同名条目倍率/档位全
///    不同）。按 v1 恒 CN 执行的 fail-closed 策略判 Global 专属：Global 侧
///    声明了此 id 而 CN 侧无任何确定性可得证据，放行只会把请求送进 CN 网关
///    收上游诡异报错，拒绝则走明确的 404 提示通道。
fn entry_is_global_only(model: &str, entry: &Value) -> bool {
    let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return false; // name/upstreamKey 命中且无 id 的边缘：保守放行
    }
    let Some(listed) = qoder_upstream::list()
        .into_iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(id))
    else {
        return false; // list 无此 id（按 name/upstreamKey 命中）：保守放行
    };
    if listed.get("region").and_then(Value::as_str) == Some("cn") {
        return false; // 证据①：Global 侧确定无此 id
    }
    match qoder_upstream::resolve(model, qoder_upstream::QoderRegion::Global) {
        // 证据②：两路命中不同 ⟹ CN 侧至少一源声明此 id（逐源可证，见上）
        Some(g) if g != *entry => false,
        // 全等：CN 侧确定性可得证据缺失，按 fail-closed 策略判专属（见③）
        _ => true,
    }
}

/// 字符边界安全截断（自抄 wb_route::safe_slice，私有不可复用）：
/// 字节落点在多字节字符内时回退到前一个边界，避免超长上游响应体
/// 整段进入错误消息/日志
fn safe_slice(s: &str, n: usize) -> &str {
    if let Some(t) = s.get(..n) {
        return t;
    }
    let mut end = n.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ==================== F-80-余 v2 慢请求竞速对冲（取号侧编排，wb_route 同构） ====================

/// 对冲账号在途计数租约（镜像 wb_route::HedgeLease）：构造即 +1（竞速窗口
/// 占用），Drop 即 -1——建连/凭证解析失败（闭包内提前返回）、双败、竞速胜出
/// 三类退出路径均恰好释放一次，杜绝计数泄漏导致的账号永久 busy
struct QoderHedgeLease {
    uid: String,
    counter: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl QoderHedgeLease {
    fn acquire(state: &ApiSharedState, uid: &str) -> Self {
        let counter = state.qoder_pool.inflight_handle(uid);
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { uid: uid.to_string(), counter }
    }
}

impl Drop for QoderHedgeLease {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 首字竞速胜者信息：**原始未翻译**上游行源（Qoder 双层信封翻译在竞速落定后
/// 统一进行，胜者行源经 QoderTranslate 转换）+ 生效账号 + 对冲计数租约
struct QoderRaceWin {
    lines: Box<dyn std::iter::Iterator<Item = String> + Send>,
    uid: String,
    hedge: Option<QoderHedgeLease>,
    takeover: bool,
}

/// 首字竞速（F-80-余 v2，镜像 wb_route::race_first_byte）：对冲关闭（阈值 0）
/// 时与纯首字超时语义完全一致；开启时主请求首字节超阈值 → 从池内取第二账号
/// （走同一 busy 过滤——主账号已 inflight 天然让位，受 F-77 并发上限约束）
/// 发对冲请求，先出首字者胜。对冲请求独立解析凭证（identity 回调）并按对冲
/// 账号 uid 重建 agent 信封（session 派生依赖 uid）。
#[allow(clippy::too_many_arguments)]
fn race_qoder_first_byte(
    state: &Arc<ApiSharedState>,
    primary_uid: &str,
    primary_reader: Box<dyn std::io::Read + Send>,
    peek: &Value,
    entry: &Value,
    model_key: &str,
    model_source: &str,
    tried: &HashSet<String>,
    allowed: Option<&HashSet<String>>,
    dedicated: Option<&str>,
) -> Result<QoderRaceWin, ()> {
    let hedge_ms = state
        .qoder_hedge_threshold_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    if hedge_ms == 0 {
        let lines = lines_with_first_byte_timeout(primary_reader)?;
        return Ok(QoderRaceWin {
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
    let peek2 = peek.clone();
    let entry2 = entry.clone();
    let mk2 = model_key.to_string();
    let ms2 = model_source.to_string();
    let spawn_backup = move || -> Option<(Box<dyn std::io::Read + Send>, QoderHedgeLease)> {
        let (picked2, ev) = state2
            .qoder_pool
            .pick_excluding_constrained_ev(&tried2, allowed2.as_ref(), dedicated2.as_deref())?;
        // F-77⑤ 可观测：对冲取号同样记录 busy 让位/降级事件
        if let Some(ev) = ev {
            state2.logger.log_sched_event(&ev);
        }
        // 租约先于凭证解析获取：失败路径随闭包局部变量 Drop 自动 -1
        let lease = QoderHedgeLease::acquire(&state2, &picked2.uid);
        let creds2 = (state2.qoder_identity.as_ref()? )(&picked2.uid).ok()?;
        let converted2 = qoder_upstream::prepare_qoder_body(&peek2, &entry2, &creds2.uid).ok()?;
        let reader2 = qoder_upstream::make_qoder_request(&creds2, &converted2, &mk2, &ms2).ok()?;
        Some((reader2, lease))
    };
    match lines_with_first_byte_hedged(primary_reader, hedge_ms, spawn_backup) {
        Ok(out) => {
            let uid = if out.takeover {
                out.hedge
                    .as_ref()
                    .map(|l| l.uid.clone())
                    .unwrap_or_else(|| primary_uid.to_string())
            } else {
                primary_uid.to_string()
            };
            Ok(QoderRaceWin { lines: out.lines, uid, hedge: out.hedge, takeover: out.takeover })
        }
        Err(()) => Err(()),
    }
}

/// 竞速结束后处理对冲计数与日志（镜像 wb_route::settle_hedge）：
/// 释放对冲账号竞速窗口占用；接管时 guard 重绑到对冲账号；[SCHED] 日志记录
/// hedge_takeover / hedge_lost
fn settle_qoder_hedge(
    state: &ApiSharedState,
    win: &mut QoderRaceWin,
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
    drop(lease);
    guard
}

/// 竞速胜者原始行源 → 翻译为 OpenAI chunk 行 → 可中断行源（对冲开启路径）。
/// 对冲关闭时 run_* 直接走 open_qoder_stream（等价实现，少一层迭代器包装）
fn translate_race_lines(
    win: QoderRaceWin,
    err_slot: Arc<Mutex<Option<ErrMeta>>>,
    chat_id: &str,
    model: &str,
) -> InterruptibleLines {
    let translated = qoder_upstream::QoderTranslate::new(win.lines, err_slot, chat_id, model);
    InterruptibleLines::from_iterator(Box::new(translated))
}

// ==================== 流式入口 ====================

/// Qoder 流式对话（routes.rs 各协议端点 TargetPool::Qoder 分支调用）
pub fn qoder_stream_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    // 批次 D-1 线程隔离：流任务迁入专用阻塞池（同 wb_stream_chat）
    super::stream_runtime().spawn_blocking(move || {
        let chat_id = match proto {
            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
            Protocol::Anthropic => format!("msg_{}", now_ts()),
            Protocol::Responses => format!("resp_{}", now_ts()),
        };

        // SSE keep-alive 15s：watch + DoneSignal 方案（主任务结束含 panic 展开
        // 置 done=true，ticker 退出放行流终结）——与 wb_stream_chat 同构
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
                        _ = done_rx.changed() => break,
                    }
                }
            });
        }

        run_qoder_stream(&state, &body_vec, &model, proto, &key_id, &chat_id, &tx, start_ts, guard);
    });

    let stream = ReceiverStream::new(rx);
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| internal_error_response())
}

#[allow(clippy::too_many_arguments)]
fn run_qoder_stream(
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
    let key_name = super::api_keys::key_name_for(&state.data_dir, key_id);
    let peek: Value = serde_json::from_slice(body_vec).unwrap_or(json!({}));

    // 目录条目（upstreamKey/config）：调度命中后目录刷新导致条目消失时按 404 透传。
    // 区域边界（v1）：恒 CN 区解析 + CN 网关执行（QODER_CHAT_URL）——Global 区
    // 条目仅经 resolve 双区兜底可见，实际请求仍打 gateway.qoder.com.cn；接线
    // Global 区（api3.qoder.sh）时此处与 dispatch 源判定需一并按账号区域分流
    let Some(entry) = qoder_upstream::resolve(model, qoder_upstream::QoderRegion::Cn) else {
        let msg = format!("model {model} not in Qoder catalog");
        state.logger.log_request(
            "qoder", "POST", "/v1/chat/completions", model, true, 404, "-",
            start_ts.elapsed().as_millis() as u64, &key_name, "", Some(&msg),
        );
        send_stream_error(tx, proto, 404, &msg);
        return;
    };
    // 区域边界执行入口检查（审查修复）：resolve 双区兜底可能命中 Global 专属
    // 条目——调度判活但 CN 网关执行必死，此处明确拒绝（404 可解析通道）而非
    // 透传上游诡异报错
    if entry_is_global_only(model, &entry) {
        let msg = format!(
            "upstream 404 error: model {model} only available in Qoder Global region, not yet supported"
        );
        state.logger.log_request(
            "qoder", "POST", "/v1/chat/completions", model, true, 404, "-",
            start_ts.elapsed().as_millis() as u64, &key_name, "", Some(&msg),
        );
        send_stream_error(tx, proto, 404, &msg);
        return;
    }
    let model_key = entry
        .get("upstreamKey")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let model_source = entry
        .pointer("/config/source")
        .and_then(Value::as_str)
        .unwrap_or("system")
        .to_string();

    // F-35 子 Key 约束（qoder 池作用域；匿名/无约束 Key 全空 → 走默认调度）
    let (allowed_set, dedicated) = super::api_keys::constraints_for(&state.data_dir, key_id)
        .and_then(|k| k.pool_constraints("qoder"))
        .map_or((None, None), |c| (c.allowed, c.dedicated));

    // F-80-余 v2 会话粘性（开关关闭时恒 None，v1 轮换行为零变化）：粘性命中且
    // 账号在池 → 首选粘住账号（busy 且有空闲候选时让位，F-77④ 同构）；子 Key
    // 限定上游不含粘性账号时忽略粘性。同账号 + 同种子派生同一上游 session_id
    //（prepare_qoder_body session_id_for），粘住账号即保住上游会话侧复用
    let sticky_key = SessionKey::from_body(&peek);
    let sticky0: Option<String> = if state
        .qoder_sticky_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        state
            .qoder_sticky
            .resolve(&sticky_key, now_ts())
            .and_then(|b| {
                if allowed_set.as_ref().is_some_and(|a| !a.contains(&b.uid)) {
                    return None;
                }
                state.qoder_pool.pick_by_uid(&b.uid).map(|_| b.uid)
            })
    } else {
        None
    };
    // 绑定回写用会话种子（仅落库留档；session 由 body 构造时按 uid+seed 派生）
    let sticky_seed: String = peek
        .get("session_id")
        .and_then(Value::as_str)
        .or_else(|| peek.get("user").and_then(Value::as_str))
        .unwrap_or("-")
        // 审查 P3：seed 进 sticky 存储做会话匹配，超长值放大留档体积——
        // 限长 128 字符（与上游请求体 session_seed 同款确定性截断）
        .chars()
        .take(128)
        .collect();

    // 首选：粘性 > 调度策略（F-77④：粘性账号 busy 且有空闲候选时让位）
    let mut first_pick: Option<super::pool::PickedAccount> = sticky0.as_ref().and_then(|u| {
        state
            .qoder_pool
            .pick_sticky_yield(u, allowed_set.as_ref())
            .map(|(p, ev)| {
                if let Some(ev) = ev {
                    state.logger.log_sched_event(&ev);
                }
                p
            })
    });

    let mut tried: HashSet<String> = HashSet::new();

    loop {
        // 客户端断连检测：通道关闭即终止轮换/重试（对齐 run_wb_stream）
        if tx.is_closed() {
            return;
        }
        // ── 取号：粘性命中优先，否则按 Key 约束 + 调度策略；换号后仅走策略 ──
        let picked = match first_pick.take() {
            Some(p) => p,
            None => match state
                .qoder_pool
                .pick_excluding_constrained_ev(&tried, allowed_set.as_ref(), dedicated.as_deref())
            {
                Some((p, ev)) => {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                }
                None => {
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    state.record_usage_qoder(model, "none", key_id, false, true, duration_ms, 0, 0, None);
                    state.logger.log_request(
                        "qoder", "POST", "/v1/chat/completions", model, true, 503, "none",
                        duration_ms, &key_name, "", Some("no healthy account"),
                    );
                    // 审查 P3：错误消息面向客户端用户展示，中文化（code 保留机器可读）
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from(
                        "data: {\"error\":{\"message\":\"Qoder 上游暂无可用账号（无健康账号可调度），请检查账号池或稍后重试\",\"type\":\"api_error\",\"code\":\"no_healthy_account\"}}\n\n",
                    )));
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                    return;
                }
            },
        };
        tried.insert(picked.uid.clone());
        // 当前账号排队重试计数（账号局部：换号自然重置）
        let mut queue_same: u32 = 0;
        *safe_lock(&state.active_uid) = Some(picked.uid.clone());
        // F-77 账号级在途计数：取号即绑定（换号时 bind_account 自动解绑旧账号）
        guard = guard.bind_account(state.qoder_pool.inflight_handle(&picked.uid));

        // 凭证解析：回调缺失（未注入/单测）→ 换号不冷却（配置问题非账号问题）；
        // 回调报错（PAT 换令牌失败/登录态失效）→ SessionDead 禁用后换号
        let creds: QoderCreds = match state.qoder_identity.as_ref() {
            None => {
                state.logger.log_request(
                    "qoder", "POST", "/v1/chat/completions", model, true, 503, &picked.uid,
                    start_ts.elapsed().as_millis() as u64, &key_name,
                    &state.qoder_pool.name_of(&picked.uid), Some("qoder identity 回调未注入"),
                );
                continue;
            }
            Some(resolve) => match resolve(&picked.uid) {
                Ok(c) => c,
                Err(e) => {
                    // 审查修复：瞬时失败（网络/5xx，带 TRANSIENT_ERR_TAG）记 Server 熔断
                    //（可自愈）；仅永久失效 SessionDead 禁用
                    let kind = if crate::tasks::qoder_common::is_transient_identity_err(&e) {
                        ErrKind::Server
                    } else {
                        ErrKind::SessionDead
                    };
                    state.qoder_pool.note_error(&picked.uid, kind);
                    *safe_lock(&state.last_error) =
                        Some(format!("qoder identity uid={} err={}", picked.uid, e));
                    state.logger.log_request(
                        "qoder", "POST", "/v1/chat/completions", model, true, 503, &picked.uid,
                        start_ts.elapsed().as_millis() as u64, &key_name,
                        &state.qoder_pool.name_of(&picked.uid), Some("凭证解析失败"),
                    );
                    continue;
                }
            },
        };

        // 请求体（agent 信封；依赖账号 uid 派生 session）
        let converted = match qoder_upstream::prepare_qoder_body(&peek, &entry, &creds.uid) {
            Ok(b) => b,
            Err(e) => {
                state.logger.log_request(
                    "qoder", "POST", "/v1/chat/completions", model, true, 500, &picked.uid,
                    start_ts.elapsed().as_millis() as u64, &key_name,
                    &state.qoder_pool.name_of(&picked.uid), Some(&e),
                );
                send_stream_error(tx, proto, 500, &e);
                return;
            }
        };

        let mut same_attempt: u32 = 0;
        // 对冲接管标记（本轮尝试内有效）：接管后排队同号退避不再适用——
        // 退避重试走主账号 creds/converted，与生效（对冲）账号不一致。
        // 初值在 Ok 分支必然先赋值后读取，lint 对初值误报，显式允许
        #[allow(unused_assignments)]
        let mut hedge_taken = false;
        loop {
            // 断连检测：重试等待/长路径期间离开则终止（guard 随 Drop 释放）
            if tx.is_closed() {
                return;
            }
            // F-76① TTFT 采样（同 wb_route：各次尝试独立计时，竞速胜者首字即 TTFB）
            let ttfb_start = Instant::now();
            match qoder_upstream::make_qoder_request(
                &creds,
                &converted,
                &model_key,
                &model_source,
            ) {
                Ok(reader) => {
                    // 首字超时 10s：超时视为上游故障 → 换号（同 run_wb_stream 语义）；
                    // F-80-余 v2 慢请求竞速对冲：首字超阈值向第二账号发对冲请求，
                    // 先出首字者胜（对冲关闭时与纯首字超时语义一致）
                    let err_slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
                    let mut race = match race_qoder_first_byte(
                        state,
                        &picked.uid,
                        reader,
                        &peek,
                        &entry,
                        &model_key,
                        &model_source,
                        &tried,
                        allowed_set.as_ref(),
                        dedicated.as_deref(),
                    ) {
                        Ok(w) => w,
                        Err(()) => {
                            state.qoder_pool.note_error(&picked.uid, ErrKind::Server);
                            break;
                        }
                    };
                    let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                    // 对冲计数落定 + guard 重绑（接管时生效账号 = 对冲账号）
                    guard = settle_qoder_hedge(state, &mut race, guard, &picked.uid);
                    hedge_taken = race.takeover;
                    let win_uid = race.uid.clone();
                    // 竞速胜者行源（原始上游行）统一翻译后喂 stream_forward_ex
                    let ilines = translate_race_lines(race, err_slot.clone(), chat_id, model);
                    let (error_info, sent_any, failed_inline, usage) =
                        wb_sse::stream_forward_ex(ilines, tx, proto, chat_id, model);
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    let meta = err_slot
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let (pt, ct) = usage
                        .as_ref()
                        .map(|u| {
                            (
                                u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                u.get("completion_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                            )
                        })
                        .unwrap_or((0, 0));
                    state.record_usage_qoder(
                        model,
                        &win_uid,
                        key_id,
                        error_info.is_none() && !failed_inline,
                        true,
                        duration_ms,
                        pt,
                        ct,
                        Some(ttfb_ms),
                    );
                    match error_info {
                        Some((code, msg)) => {
                            // 流内错误分类以 ErrMeta 为准（translate 写入，含排队/额度信号）
                            match meta.as_ref().map(|m| m.kind) {
                                Some(UpstreamKind::Queued) => {
                                    // 排队：同号退避重试为主（对冲接管后不适用——重试
                                    // 凭证/请求体属主账号，与生效账号不一致，直接换号）；
                                    // 若该模型已被并发请求记入冷却，则直接 break 换号
                                    //（快速失败）
                                    if !sent_any && !hedge_taken && queue_same < QUEUE_RETRY_LIMIT {
                                        let secs = meta
                                            .as_ref()
                                            .and_then(|m| m.queue.as_ref())
                                            .and_then(|q| q.retry_after_secs)
                                            .unwrap_or(QUEUE_DEFAULT_BACKOFF_SECS)
                                            .clamp(1, QUEUE_BACKOFF_MAX_SECS);
                                        // 审查修复：模型级冷却——排队是模型级现象，
                                        // 记录窗口供 dispatch 预检快速回退；已在冷却
                                        // 中（并发请求刚记录过）则跳过同号退避空转
                                        if model_cooling_remaining_secs(model).is_some() {
                                            break;
                                        }
                                        note_model_cooldown(model, secs);
                                        queue_same += 1;
                                        if !sleep_interruptible(Duration::from_secs(secs), Some(tx)) {
                                            return; // 客户端断连：提前退出
                                        }
                                        continue;
                                    }
                                    // 超限/sent_any：不再 break 硬换号——内容已流出
                                    // （sent_any=true）时换号重发会向同一 SSE 流拼接
                                    // 第二份完整响应；交下方公共收尾（!sent_any→换号，
                                    // sent_any→就地收尾），与其他错误分支口径一致
                                }
                                Some(kind) => {
                                    let ek = kind.to_err_kind();
                                    if ek != ErrKind::None {
                                        state.qoder_pool.note_error(&win_uid, ek);
                                    }
                                    *safe_lock(&state.last_error) = Some(format!(
                                        "qoder uid={} code={} msg={}",
                                        win_uid, code, msg
                                    ));
                                }
                                None => {
                                    // translate 之外的错误帧。EOF 零完成哨兵（-9901）不经
                                    // translate 写 ErrMeta，必落本分支（非「理论不可达」）：
                                    // 上游间歇性空回放属暂态，按 wb_route 口径免熔断，
                                    // 仅由下方 !sent_any 分支换号重试
                                    if !super::is_empty_completion(code, &msg) {
                                        state.qoder_pool.note_error(&win_uid, ErrKind::Server);
                                    }
                                }
                            }
                            if !sent_any {
                                // 流未开始：错误不下发，允许换号重试
                                break;
                            }
                            state.logger.log_request_ttfb(
                                "qoder", "POST", "/v1/chat/completions", model, true, 200,
                                &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                &state.qoder_pool.name_of(&win_uid), Some(&msg),
                            );
                            return; // 已有数据流出：就地收尾
                        }
                        None => {
                            if failed_inline {
                                // 流内失败已就地透传客户端：不重试不记账冷却（同 wb_route）
                                state.logger.log_request_ttfb(
                                    "qoder", "POST", "/v1/chat/completions", model, true, 200,
                                    &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                    &state.qoder_pool.name_of(&win_uid),
                                    Some("流内失败已透传客户端"),
                                );
                                return;
                            }
                            clear_model_cooldown(model); // 审查修复：成功即清模型级冷却
                            state.qoder_pool.note_success(&win_uid);
                            // F-80-余 v2：绑定粘性会话（开关开启时；Mutex 内 re-check
                            // 防 TOCTOU 由 StickyStore::bind 保证）
                            if state
                                .qoder_sticky_enabled
                                .load(std::sync::atomic::Ordering::Relaxed)
                            {
                                state.qoder_sticky.bind(&sticky_key, &win_uid, &sticky_seed, now_ts());
                                state.qoder_sticky.save(&state.data_dir);
                            }
                            state.logger.log_request_ttfb(
                                "qoder", "POST", "/v1/chat/completions", model, true, 200,
                                &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                &state.qoder_pool.name_of(&win_uid), None,
                            );
                            return;
                        }
                    }
                }
                Err((status, resp_body, retry_after)) => {
                    // HTTP 层分级重试（429/5xx/502 同号退避；401/403/4xx 换号/终止）
                    match retry_plan(status, &resp_body, same_attempt, retry_after) {
                        RetryAction::RetrySame { delay_ms } => {
                            same_attempt += 1;
                            // 审查修复：分段 sleep + 断连感知（原整段 sleep 最长 60s，
                            // 断连后仍空等；原 sleep 后的 is_closed 检查一并合并）
                            if !sleep_interruptible(
                                Duration::from_millis(delay_ms.min(60_000)),
                                Some(tx),
                            ) {
                                return;
                            }
                            continue;
                        }
                        RetryAction::SwitchKey => {
                            // Qoder 业务错误常在 403/200 信封里：按 upstream 分类；
                            // 排队（10605）同号退避优先——模型已在冷却中则
                            // break 换号（快速失败，见下方冷却检查）
                            let classified =
                                qoder_upstream::classify_upstream_error(status, &resp_body);
                            if classified.kind == UpstreamKind::Queued
                                && queue_same < QUEUE_RETRY_LIMIT
                            {
                                let secs = classified
                                    .queue
                                    .as_ref()
                                    .and_then(|q| q.retry_after_secs)
                                    .unwrap_or(QUEUE_DEFAULT_BACKOFF_SECS)
                                    .clamp(1, QUEUE_BACKOFF_MAX_SECS);
                                // 审查修复：模型级冷却（同流内 Queued 分支）
                                if model_cooling_remaining_secs(model).is_some() {
                                    break;
                                }
                                note_model_cooldown(model, secs);
                                queue_same += 1;
                                if !sleep_interruptible(Duration::from_secs(secs), Some(tx)) {
                                    return; // 客户端断连：提前退出
                                }
                                continue;
                            }
                            let kind = classified.kind.to_err_kind();
                            if kind != ErrKind::None {
                                state.qoder_pool.note_error(&picked.uid, kind);
                            }
                            *safe_lock(&state.last_error) = Some(format!(
                                "qoder uid={} status={} body={}",
                                picked.uid,
                                status,
                                safe_slice(&resp_body, 200)
                            ));
                            state.logger.log_request(
                                "qoder", "POST", "/v1/chat/completions", model, true, status,
                                &picked.uid, start_ts.elapsed().as_millis() as u64, &key_name,
                                &state.qoder_pool.name_of(&picked.uid),
                                Some(&format!("upstream status={status}")),
                            );
                            break; // 换号
                        }
                        RetryAction::Fatal => {
                            let msg =
                                format!("upstream {} error: {}", status, safe_slice(&resp_body, 300));
                            state.logger.log_request(
                                "qoder", "POST", "/v1/chat/completions", model, true, status,
                                &picked.uid, start_ts.elapsed().as_millis() as u64, &key_name,
                                &state.qoder_pool.name_of(&picked.uid), Some(&msg),
                            );
                            send_stream_error(tx, proto, status as i64, &msg);
                            return;
                        }
                    }
                }
            }
        }
    }
}

// ==================== 非流式（上游只回 SSE → 本地聚合） ====================

/// Qoder 非流式对话：聚合 + 协议投影（routes.rs 各协议端点调用）
#[allow(clippy::too_many_arguments)]
pub async fn qoder_aggregate_chat(
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
    // 聚合含分级重试（同号退避最长 60s×N），迁入 stream_runtime 专用阻塞池
    //（同 wb_aggregate_chat；长阻塞占主池会饿死鉴权等短任务）
    let result = super::stream_runtime().spawn_blocking(move || {
        let mut guard = guard;
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        let peek: Value = serde_json::from_slice(&body_vec).unwrap_or(json!({}));

        // 区域边界（v1）：恒 CN 区解析，同 run_qoder_stream 注释（Global 区未接线）
        let Some(entry) = qoder_upstream::resolve(&model, qoder_upstream::QoderRegion::Cn) else {
            // 审查修复：走 "upstream {status} error:" 可解析通道——非流式错误
            // 解析按前缀提取状态码，裸消息此前落 503，与流式 404 语义不一致
            return Err(format!("upstream 404 error: model {model} not in Qoder catalog"));
        };
        // 区域边界执行入口检查（审查修复）：同 run_qoder_stream，Global 专属
        // 条目调度判活但 CN 网关执行必死，明确拒绝而非透传上游诡异报错
        if entry_is_global_only(&model, &entry) {
            return Err(format!(
                "upstream 404 error: model {model} only available in Qoder Global region, not yet supported"
            ));
        }
        let model_key = entry
            .get("upstreamKey")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let model_source = entry
            .pointer("/config/source")
            .and_then(Value::as_str)
            .unwrap_or("system")
            .to_string();

        let (allowed_set, dedicated) =
            super::api_keys::constraints_for(&state.data_dir, &key_id)
                .and_then(|k| k.pool_constraints("qoder"))
                .map_or((None, None), |c| (c.allowed, c.dedicated));

        // F-80-余 v2 会话粘性（同 run_qoder_stream；开关关闭恒 None 保持 v1 行为）
        let sticky_key = SessionKey::from_body(&peek);
        let sticky0: Option<String> = if state
            .qoder_sticky_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            state
                .qoder_sticky
                .resolve(&sticky_key, now_ts())
                .and_then(|b| {
                    if allowed_set.as_ref().is_some_and(|a| !a.contains(&b.uid)) {
                        return None;
                    }
                    state.qoder_pool.pick_by_uid(&b.uid).map(|_| b.uid)
                })
        } else {
            None
        };
        let sticky_seed: String = peek
            .get("session_id")
            .and_then(Value::as_str)
            .or_else(|| peek.get("user").and_then(Value::as_str))
            .unwrap_or("-")
            // 审查 #6：与流式路径同款限长 128（seed 进 sticky 留档，超长放大存储）
            .chars()
            .take(128)
            .collect();
        // 首选：粘性 > 调度策略（F-77④：粘性账号 busy 且有空闲候选时让位）
        let mut first_pick: Option<super::pool::PickedAccount> = sticky0.as_ref().and_then(|u| {
            state
                .qoder_pool
                .pick_sticky_yield(u, allowed_set.as_ref())
                .map(|(p, ev)| {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                })
        });

        let mut tried: HashSet<String> = HashSet::new();

        loop {
            // ── 取号：粘性命中优先，否则按 Key 约束 + 调度策略；换号后仅走策略 ──
            let picked = match first_pick.take() {
                Some(p) => p,
                None => match state.qoder_pool.pick_excluding_constrained_ev(
                    &tried,
                    allowed_set.as_ref(),
                    dedicated.as_deref(),
                ) {
                    Some((p, ev)) => {
                        if let Some(ev) = ev {
                            state.logger.log_sched_event(&ev);
                        }
                        p
                    }
                    None => {
                        state.record_usage_qoder(
                            &model,
                            "none",
                            &key_id,
                            false,
                            stream,
                            start_ts.elapsed().as_millis() as u64,
                            0,
                            0,
                            None,
                        );
                        // 审查 P3：错误消息面向客户端用户展示，中文化
                        return Err("Qoder 上游暂无可用账号（无健康账号可调度），请检查账号池或稍后重试".to_string());
                    }
                },
            };
            tried.insert(picked.uid.clone());
            // 当前账号排队重试计数（账号局部：换号自然重置）
            let mut queue_same: u32 = 0;
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());
            guard = guard.bind_account(state.qoder_pool.inflight_handle(&picked.uid));

            let creds: QoderCreds = match state.qoder_identity.as_ref() {
                None => continue, // 回调未注入：换号（tried 增长自然耗尽后 503）
                Some(resolve) => match resolve(&picked.uid) {
                    Ok(c) => c,
                    Err(e) => {
                        // 审查修复：瞬时失败走 Server 熔断可自愈（同流式路径）
                        let kind = if crate::tasks::qoder_common::is_transient_identity_err(&e) {
                            ErrKind::Server
                        } else {
                            ErrKind::SessionDead
                        };
                        state.qoder_pool.note_error(&picked.uid, kind);
                        *safe_lock(&state.last_error) =
                            Some(format!("qoder identity uid={} err={}", picked.uid, e));
                        continue;
                    }
                },
            };

            let converted = match qoder_upstream::prepare_qoder_body(&peek, &entry, &creds.uid) {
                Ok(b) => b,
                Err(e) => return Err(e),
            };

            let mut same_attempt: u32 = 0;
            // 对冲接管标记：接管后排队同号退避不适用（重试凭证/请求体属主账号）
            #[allow(unused_assignments)]
            let mut hedge_taken = false;
            loop {
                // F-76① TTFT 采样（同 wb_route 聚合：各轮独立计时，最终轮即最终回复 TTFT）
                let ttfb_start = Instant::now();
                match qoder_upstream::make_qoder_request(
                    &creds,
                    &converted,
                    &model_key,
                    &model_source,
                ) {
                    Ok(reader) => {
                        let err_slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
                        // 首字超时 10s + F-80-余 v2 竞速对冲（对冲关闭时与纯首字
                        // 超时语义一致）：超时/双败视为上游故障 → 换号
                        let mut race = match race_qoder_first_byte(
                            &state,
                            &picked.uid,
                            reader,
                            &peek,
                            &entry,
                            &model_key,
                            &model_source,
                            &tried,
                            allowed_set.as_ref(),
                            dedicated.as_deref(),
                        ) {
                            Ok(w) => w,
                            Err(()) => {
                                state.qoder_pool.note_error(&picked.uid, ErrKind::Server);
                                break;
                            }
                        };
                        let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                        // 对冲计数落定 + guard 重绑（接管时生效账号 = 对冲账号）
                        guard = settle_qoder_hedge(&state, &mut race, guard, &picked.uid);
                        hedge_taken = race.takeover;
                        let win_uid = race.uid.clone();
                        // 竞速胜者原始行源 → 翻译 → 聚合（tool_calls 合并/usage 收集）；
                        // 首字超时已在 race 内裁定，聚合阶段无失败通道
                        let agg_chat_id = format!("chatcmpl-{}", now_ts());
                        let translated =
                            qoder_upstream::QoderTranslate::new(race.lines, err_slot.clone(), &agg_chat_id, &model);
                        let (resp, error) = wb_sse::aggregate(translated, &agg_chat_id);
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        match (resp, error) {
                            (Some(mut r), None) => {
                                r["model"] = json!(model);
                                let (pt, ct) = r
                                    .get("usage")
                                    .map(|u| {
                                        (
                                            u.get("prompt_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0),
                                            u.get("completion_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0),
                                        )
                                    })
                                    .unwrap_or((0, 0));
                                state.record_usage_qoder(
                                    &model, &win_uid, &key_id, true, stream, duration_ms, pt, ct,
                                    Some(ttfb_ms),
                                );
                                clear_model_cooldown(&model); // 审查修复：成功即清模型级冷却
                                state.qoder_pool.note_success(&win_uid);
                                // F-80-余 v2：绑定粘性会话（开关开启时；同流式路径）
                                if state
                                    .qoder_sticky_enabled
                                    .load(std::sync::atomic::Ordering::Relaxed)
                                {
                                    state.qoder_sticky.bind(&sticky_key, &win_uid, &sticky_seed, now_ts());
                                    state.qoder_sticky.save(&state.data_dir);
                                }
                                state.logger.log_request_ttfb(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream, 200,
                                    &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                    &state.qoder_pool.name_of(&win_uid), None,
                                );
                                return Ok(r);
                            }
                            (None, Some((code, msg))) => {
                                let meta = err_slot
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .clone();
                                match meta.as_ref().map(|m| m.kind) {
                                    Some(UpstreamKind::Queued) => {
                                        // 排队：同号退避重试为主（对冲接管后不适用，
                                        // 同流式路径——直接换号）；模型已在冷却中
                                        // 则 break 换号（快速失败，见下方冷却检查）
                                        if !hedge_taken && queue_same < QUEUE_RETRY_LIMIT {
                                            let secs = meta
                                                .as_ref()
                                                .and_then(|m| m.queue.as_ref())
                                                .and_then(|q| q.retry_after_secs)
                                                .unwrap_or(QUEUE_DEFAULT_BACKOFF_SECS)
                                                .clamp(1, QUEUE_BACKOFF_MAX_SECS);
                                            // 审查修复：模型级冷却（同流式分支）；
                                            // 非流式无下行通道，保持整段 sleep
                                            if model_cooling_remaining_secs(&model).is_some() {
                                                break;
                                            }
                                            note_model_cooldown(&model, secs);
                                            queue_same += 1;
                                            std::thread::sleep(Duration::from_secs(secs));
                                            continue;
                                        }
                                        break; // 超限换号
                                    }
                                    Some(kind) => {
                                        let ek = kind.to_err_kind();
                                        if ek != ErrKind::None {
                                            state.qoder_pool.note_error(&win_uid, ek);
                                        }
                                    }
                                    None => {
                                        state.qoder_pool.note_error(&win_uid, ErrKind::Server);
                                    }
                                }
                                *safe_lock(&state.last_error) = Some(format!(
                                    "qoder uid={} code={} msg={}",
                                    win_uid, code, msg
                                ));
                                state.logger.log_request_ttfb(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream, 200,
                                    &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                    &state.qoder_pool.name_of(&win_uid), Some(&msg),
                                );
                                // 流内错误且未产出内容 → 换号重试
                                break;
                            }
                            _ => {
                                state.qoder_pool.note_error(&win_uid, ErrKind::Server);
                                state.logger.log_request_ttfb(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream, 502,
                                    &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                    &state.qoder_pool.name_of(&win_uid),
                                    Some("empty response"),
                                );
                                break;
                            }
                        }
                    }
                    Err((status, resp_body, retry_after)) => {
                        match retry_plan(status, &resp_body, same_attempt, retry_after) {
                            RetryAction::RetrySame { delay_ms } => {
                                same_attempt += 1;
                                std::thread::sleep(Duration::from_millis(delay_ms.min(60_000)));
                                continue;
                            }
                            RetryAction::SwitchKey => {
                                let classified =
                                    qoder_upstream::classify_upstream_error(status, &resp_body);
                                if classified.kind == UpstreamKind::Queued
                                    && queue_same < QUEUE_RETRY_LIMIT
                                {
                                    let secs = classified
                                        .queue
                                        .as_ref()
                                        .and_then(|q| q.retry_after_secs)
                                        .unwrap_or(QUEUE_DEFAULT_BACKOFF_SECS)
                                        .clamp(1, QUEUE_BACKOFF_MAX_SECS);
                                    // 审查修复：模型级冷却（同流式分支）；非流式
                                    // 无下行通道，保持整段 sleep
                                    if model_cooling_remaining_secs(&model).is_some() {
                                        break;
                                    }
                                    note_model_cooldown(&model, secs);
                                    queue_same += 1;
                                    std::thread::sleep(Duration::from_secs(secs));
                                    continue;
                                }
                                let kind = classified.kind.to_err_kind();
                                if kind != ErrKind::None {
                                    state.qoder_pool.note_error(&picked.uid, kind);
                                }
                                *safe_lock(&state.last_error) = Some(format!(
                                    "qoder uid={} status={}",
                                    picked.uid, status
                                ));
                                // 记账口径对齐流式路径（每请求一行）：换号中间态不记
                                // usage（原聚合路径每次换号多记一条失败，qoder 桶
                                // requests/errors 虚高）；最终 no-healthy 由取号分支记录
                                state.logger.log_request(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream,
                                    status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64, &key_name,
                                    &state.qoder_pool.name_of(&picked.uid),
                                    Some(&format!("upstream status={status}")),
                                );
                                break;
                            }
                            RetryAction::Fatal => {
                                state.logger.log_request(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream,
                                    status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64, &key_name,
                                    &state.qoder_pool.name_of(&picked.uid),
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
            // 审查修复（对齐 wb_aggregate_chat）：Fatal 错误透传上游状态码——
            // 上游 400/404/413 等请求级错误不降级 503，避免严格客户端无意义重试；
            // 401/403/429 属账号/池问题，维持 503 语义
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

// ==================== 小工具 ====================

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 流内错误帧下发（自抄 send_stream_error_wb：私有不可复用；形态同构）
fn send_stream_error(
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
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 远程目录测试条目（adopt_remote 接受的最小合法信封载荷）
    fn remote_entry(key: &str, display: &str, factor: f64) -> Value {
        json!({
            "key": key,
            "display_name": display,
            "enable": true,
            "is_vl": false,
            "is_reasoning": true,
            "price_factor": factor,
            "max_input_tokens": 128000,
            "source": "remote",
        })
    }

    /// G4 确定性判定 remote 态：注入独有前缀（TwaG4）的双区远程目录，
    /// 顺序覆盖 entry_is_global_only 的全部分支。
    /// adopt_remote 写进程级共享目录且远程非空时会替换静态兜底——必须
    /// 持 catalog_test_guard 独占并在退出（含 panic）时自动复位回落兜底，
    /// 否则会污染并行运行的其他目录测试（如 qoder_upstream 的 list 去重）。
    #[test]
    fn entry_is_global_only_remote_states() {
        let _catalog = qoder_upstream::catalog_test_guard();
        // ① CN 远程独有：list 标注 "cn"（Global 侧完全无此 id）→ 必非专属
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Cn,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4cn_key", "TwaG4CnOnly", 0.5)]}),
        )
        .unwrap();
        let entry = qoder_upstream::resolve("TwaG4CnOnly", qoder_upstream::QoderRegion::Cn)
            .expect("CN 远程目录应命中");
        assert!(!entry_is_global_only("TwaG4CnOnly", &entry));

        // ② Global 远程独有（CN 侧零可得证据）：两路 resolve 同落 Global 条目
        //    → fail-closed 判专属
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Global,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4g_key", "TwaG4GlobalOnly", 0.7)]}),
        )
        .unwrap();
        let entry =
            qoder_upstream::resolve("TwaG4GlobalOnly", qoder_upstream::QoderRegion::Cn)
                .expect("双区兜底应命中 Global 独有条目");
        assert!(entry_is_global_only("TwaG4GlobalOnly", &entry));

        // ③ 两区同名但声明不同（Global 倍率 0.7 / CN 倍率 0.3）：list 标注被
        //    Global 吞并为 "global"，但两路 resolve 命中不同条目 → CN 远程
        //    确定可得 → 非专属（证据②，不再依赖值全等假设的反向推断）
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Global,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4same_key", "TwaG4Same", 0.7)]}),
        )
        .unwrap();
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Cn,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4same_key", "TwaG4Same", 0.3)]}),
        )
        .unwrap();
        let entry = qoder_upstream::resolve("TwaG4Same", qoder_upstream::QoderRegion::Cn)
            .expect("CN 远程目录应命中");
        assert!(!entry_is_global_only("TwaG4Same", &entry));

        // ④ 两区同名且同值（旧「同名必异」假设被打破的场景）：确定性证据缺失
        //    → 策略性判专属（fail-closed，行为与旧版一致、依据改为显式策略）
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Global,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4same_key", "TwaG4Same", 0.3)]}),
        )
        .unwrap();
        let entry = qoder_upstream::resolve("TwaG4Same", qoder_upstream::QoderRegion::Cn)
            .expect("CN 远程目录应命中");
        assert!(entry_is_global_only("TwaG4Same", &entry));
    }
}
